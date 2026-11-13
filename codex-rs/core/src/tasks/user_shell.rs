use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use codex_async_utils::CancelErr;
use codex_async_utils::OrCancelExt;
use codex_network_proxy::PROXY_ACTIVE_ENV_KEY;
use codex_utils_absolute_path::AbsolutePathBuf;
use tokio_util::sync::CancellationToken;
use tracing::error;
use uuid::Uuid;

use crate::exec::ExecCapturePolicy;
use crate::exec::StdoutStream;
use crate::exec::execute_exec_request;
use crate::exec_env::create_env;
use crate::exec_env::inject_apply_patch_env;
use crate::exec_env::inject_session_env;
use crate::sandboxing::ExecRequest;
use crate::session::TurnInput;
use crate::session::turn_context::TurnContext;
use crate::shell::Shell;
use crate::state::TaskKind;
use crate::tools::runtimes::RuntimePathPrepends;
#[cfg(unix)]
use crate::tools::runtimes::apply_package_path_prepend;
use crate::tools::runtimes::maybe_wrap_shell_lc_with_snapshot;
use crate::tools::runtimes::strip_managed_proxy_env;
use crate::user_shell_command::user_shell_command_record_item;
use codex_protocol::exec_output::ExecToolCallOutput;
use codex_protocol::exec_output::StreamOutput;
use codex_protocol::items::CommandExecutionItem;
use codex_protocol::items::CommandExecutionStatus;
use codex_protocol::items::TurnItem;
use codex_protocol::protocol::ErrorEvent;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::ExecCommandSource;
use codex_sandboxing::SandboxType;
use codex_shell_command::parse_command::parse_command;
use codex_thread_store::PersistContext;

use super::SessionTask;
use super::SessionTaskResult;
use crate::session::session::Session;
use crate::state::ActiveTurn;
use codex_protocol::error::CodexErr;
use codex_protocol::error::Result as CodexResult;
use codex_protocol::models::PermissionProfile;

const USER_SHELL_TIMEOUT_MS: u64 = 60 * 60 * 1000; // 1 hour

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum UserShellCommandMode {
    /// Executes as an independent turn lifecycle (emits TurnStarted/TurnComplete
    /// via task lifecycle plumbing).
    StandaloneTurn,
    /// Executes while another turn is already active. This mode must not emit a
    /// second TurnStarted/TurnComplete pair for the same active turn.
    ActiveTurnAuxiliary,
}

#[derive(Clone)]
pub(crate) struct UserShellCommandTask {
    command: String,
    timeout_ms: Option<u64>,
}

impl UserShellCommandTask {
    pub(crate) fn new(command: String, timeout_ms: Option<u64>) -> Self {
        Self {
            command,
            timeout_ms,
        }
    }
}

impl SessionTask for UserShellCommandTask {
    fn kind(&self) -> TaskKind {
        TaskKind::Regular
    }

    fn span_name(&self) -> &'static str {
        "session_task.user_shell"
    }

    async fn run(
        self: Arc<Self>,
        session: Arc<Session>,
        turn_context: Arc<TurnContext>,
        _input: Vec<TurnInput>,
        cancellation_token: CancellationToken,
    ) -> SessionTaskResult {
        execute_user_shell_command(
            Arc::clone(&session),
            Arc::clone(&turn_context),
            self.command.clone(),
            self.timeout_ms,
            cancellation_token.clone(),
            UserShellCommandMode::StandaloneTurn,
        )
        .await;
        Ok(None)
    }
}

impl Session {
    /// Native admission, shared by the submission dispatcher and app-server's
    /// acknowledged human shell endpoint. Pending model admission rejects rather
    /// than replacing its reservation or silently losing a shell command.
    pub(crate) async fn start_user_shell_command(
        self: &Arc<Self>,
        sub_id: String,
        command: String,
        timeout_ms: Option<u64>,
    ) -> CodexResult<()> {
        let runtime = self.services.local_agent_runtime.clone();
        let membership = runtime.admit_start()?;
        let mut active = self.active_turn.lock().await;
        if let Some(turn) = active.as_ref() {
            let task = turn.task.as_ref().ok_or_else(|| CodexErr::InvalidRequest(
                "a turn is being admitted or finalized; retry the shell command after it settles".to_owned(),
            ))?;
            let context = Arc::clone(&task.turn_context);
            let cancellation = task.cancellation_token.child_token();
            let auxiliary = turn.auxiliary_tasks.token();
            let teardown =
                membership.into_teardown_guard("user_shell_auxiliary", Some(self.thread_id));
            let session = Arc::clone(self);
            tokio::spawn(async move {
                let _auxiliary = auxiliary;
                let execution = execute_user_shell_command(
                    session,
                    context,
                    command,
                    timeout_ms,
                    cancellation.clone(),
                    UserShellCommandMode::ActiveTurnAuxiliary,
                );
                tokio::pin!(execution);
                tokio::select! {
                    biased;
                    _ = runtime.shutdown.cancelled() => { cancellation.cancel(); execution.await; }
                    _ = &mut execution => {}
                }
                teardown.complete();
            });
            return Ok(());
        }
        let admission = self
            .services
            .extensions
            .admit_turn_start()
            .ok_or_else(|| CodexErr::InvalidRequest("server is draining".to_owned()))?;
        let turn = active.get_or_insert_with(ActiveTurn::default);
        let reservation = Arc::clone(&turn.turn_state);
        drop(active);
        // The native lifetime owns accepted admission even if its RPC consumer
        // disconnects. Shutdown cancels setup and clears only this reservation.
        let session = Arc::clone(self);
        tokio::spawn(async move {
            let _admission = admission;
            let teardown = membership.into_teardown_guard("user_shell_admission", Some(session.thread_id));
            let admitted = tokio::select! {
                biased;
                _ = runtime.shutdown.cancelled() => false,
                admitted = async {
                    let context = session.new_turn_with_default_settings(sub_id, Default::default()).await;
                    let task = UserShellCommandTask::new(command, timeout_ms);
                    session.start_task_with_reservation(context, Vec::new(), task,
                        Some((Arc::clone(&reservation), None))).await
                } => admitted,
            };
            if !admitted { session.clear_reserved_idle_turn(&reservation).await; }
            teardown.complete();
            if admitted { Ok(()) } else { Err(CodexErr::TurnAborted) }
        }).await?
    }
}

pub(crate) async fn execute_user_shell_command(
    session: Arc<Session>,
    turn_context: Arc<TurnContext>,
    command: String,
    timeout_ms: Option<u64>,
    cancellation_token: CancellationToken,
    mode: UserShellCommandMode,
) -> bool {
    session
        .services
        .session_telemetry
        .counter("codex.task.user_shell", /*inc*/ 1, &[]);

    if mode == UserShellCommandMode::StandaloneTurn {
        // Auxiliary mode runs within an existing active turn. That turn already
        // emitted TurnStarted, so emitting another TurnStarted here would create
        // duplicate turn lifecycle events and confuse clients.
        // TODO(ccunningham): After TurnStarted, emit model-visible turn context diffs for
        // standalone lifecycle tasks (for example /shell, and review once it emits TurnStarted).
        // `/compact` is an intentional exception because compaction requests should not include
        // freshly reinjected context before the summary/replacement history is applied.
        session
            .emit_turn_started(&turn_context, TaskKind::Regular)
            .await;
    }

    let Some((turn_environment, environment_shell)) = turn_context
        .initial_environments
        .local()
        .and_then(|environment| environment.shell.as_ref().map(|shell| (environment, shell)))
    else {
        send_user_shell_error(
            &session,
            turn_context.as_ref(),
            "shell is unavailable in this session",
        )
        .await;
        return false;
    };

    // Execute the user's script under the environment's shell; this
    // allows commands that use shell features (pipes, &&, redirects, etc.).
    // We do not source rc files or otherwise reformat the script.
    let use_login_shell = true;
    let display_command = environment_shell.derive_exec_args(&command, use_login_shell);
    // TODO(anp): Migrate user-shell events and execution plumbing to PathUri so this local-only
    // feature does not need to project the selected environment cwd onto the Codex host.
    let Ok(cwd) = turn_environment.cwd().to_abs_path() else {
        send_user_shell_error(
            &session,
            turn_context.as_ref(),
            "shell working directory is not native to the Codex host",
        )
        .await;
        return false;
    };
    let shell_snapshot = turn_environment
        .shell_snapshot(
            &cwd,
            &display_command,
            environment_shell,
            &turn_context.config,
            /*sandbox*/ None,
        )
        .await;
    let shell_snapshot_location = shell_snapshot.as_ref().map(|snapshot| snapshot.path());
    let shell_environment_policy = turn_environment.shell_environment_policy();
    let mut exec_env_map = create_env(shell_environment_policy, Some(session.thread_id));
    inject_session_env(&mut exec_env_map, session.session_id());
    inject_apply_patch_env(&mut exec_env_map);
    if exec_env_map.contains_key(PROXY_ACTIVE_ENV_KEY) {
        strip_managed_proxy_env(&mut exec_env_map);
    }
    let exec_command = prepare_user_shell_exec_command(
        &display_command,
        environment_shell,
        shell_snapshot_location.as_ref(),
        &shell_environment_policy.r#set,
        &mut exec_env_map,
    );

    let call_id = Uuid::new_v4().to_string();
    let raw_command = command;

    let parsed_cmd = parse_command(&display_command);
    session
        .emit_turn_item_started(
            turn_context.as_ref(),
            &TurnItem::CommandExecution(CommandExecutionItem {
                model_context: None,
                sandbox_type: None,
                id: call_id.clone(),
                plugin_id: None,
                script_path: None,
                process_id: None,
                command: display_command.clone(),
                cwd: cwd.clone().into(),
                parsed_cmd: parsed_cmd.clone(),
                source: ExecCommandSource::UserShell,
                interaction_input: None,
                status: CommandExecutionStatus::InProgress,
                aggregated_output: None,
                exit_code: None,
                duration: None,
            }),
        )
        .await;

    let permission_profile = PermissionProfile::Disabled;
    let exec_env = ExecRequest {
        command: exec_command.clone(),
        cwd: cwd.clone().into(),
        env: exec_env_map,
        exec_server_env_config: None,
        exec_server_shell_snapshot: None,
        // `/shell` is the explicit full-access escape hatch, so it must not
        // inherit a managed proxy from the surrounding session or turn.
        network: None,
        network_environment_id: None,
        expiration: timeout_ms.unwrap_or(USER_SHELL_TIMEOUT_MS).into(),
        capture_policy: ExecCapturePolicy::ShellTool,
        sandbox: SandboxType::None,
        windows_sandbox_policy_cwd: cwd.clone().into(),
        windows_sandbox_workspace_roots: Vec::new(),
        windows_sandbox_level: turn_context.windows_sandbox_level,
        permission_profile,
        windows_sandbox_filesystem_overrides: None,
        arg0: None,
        exec_server_sandbox: None,
        exec_server_enforce_managed_network: false,
        exec_server_managed_network: None,
        exec_server_network_proxy: None,
    };

    let stdout_stream = Some(StdoutStream {
        sub_id: turn_context.sub_id.clone(),
        call_id: call_id.clone(),
        tx_event: session.get_tx_event(),
    });

    let exec_result = execute_exec_request(exec_env, stdout_stream, /*after_spawn*/ None)
        .or_cancel(&cancellation_token)
        .await;

    match exec_result {
        Err(CancelErr::Cancelled) => {
            let aborted_message = "command aborted by user".to_string();
            let exec_output = ExecToolCallOutput {
                exit_code: -1,
                stdout: StreamOutput::new(String::new()),
                stderr: StreamOutput::new(aborted_message.clone()),
                aggregated_output: StreamOutput::new(aborted_message.clone()),
                duration: Duration::ZERO,
                timed_out: false,
            };
            persist_user_shell_output(
                &session,
                turn_context.as_ref(),
                &raw_command,
                &exec_output,
                mode,
            )
            .await;
            session
                .emit_turn_item_completed(
                    turn_context.as_ref(),
                    TurnItem::CommandExecution(CommandExecutionItem {
                        model_context: None,
                        sandbox_type: None,
                        id: call_id,
                        plugin_id: None,
                        script_path: None,
                        process_id: None,
                        command: display_command.clone(),
                        cwd: cwd.clone().into(),
                        parsed_cmd: parsed_cmd.clone(),
                        source: ExecCommandSource::UserShell,
                        interaction_input: None,
                        status: CommandExecutionStatus::Failed,
                        aggregated_output: Some(aborted_message.clone()),
                        exit_code: Some(-1),
                        duration: Some(Duration::ZERO),
                    }),
                )
                .await;
            false
        }
        Ok(Ok(output)) => {
            session
                .emit_turn_item_completed(
                    turn_context.as_ref(),
                    TurnItem::CommandExecution(CommandExecutionItem {
                        model_context: None,
                        sandbox_type: Some(SandboxType::None),
                        id: call_id.clone(),
                        plugin_id: None,
                        script_path: None,
                        process_id: None,
                        command: display_command.clone(),
                        cwd: cwd.clone().into(),
                        parsed_cmd: parsed_cmd.clone(),
                        source: ExecCommandSource::UserShell,
                        interaction_input: None,
                        status: if output.exit_code == 0 {
                            CommandExecutionStatus::Completed
                        } else {
                            CommandExecutionStatus::Failed
                        },
                        aggregated_output: Some(output.aggregated_output.text.clone()),
                        exit_code: Some(output.exit_code),
                        duration: Some(output.duration),
                    }),
                )
                .await;

            persist_user_shell_output(&session, turn_context.as_ref(), &raw_command, &output, mode)
                .await;
            true
        }
        Ok(Err(err)) => {
            error!("user shell command failed: {err:?}");
            let message = format!("execution error: {err:?}");
            let exec_output = ExecToolCallOutput {
                exit_code: -1,
                stdout: StreamOutput::new(String::new()),
                stderr: StreamOutput::new(message.clone()),
                aggregated_output: StreamOutput::new(message.clone()),
                duration: Duration::ZERO,
                timed_out: false,
            };
            session
                .emit_turn_item_completed(
                    turn_context.as_ref(),
                    TurnItem::CommandExecution(CommandExecutionItem {
                        model_context: None,
                        sandbox_type: None,
                        id: call_id,
                        plugin_id: None,
                        script_path: None,
                        process_id: None,
                        command: display_command,
                        cwd: cwd.into(),
                        parsed_cmd,
                        source: ExecCommandSource::UserShell,
                        interaction_input: None,
                        status: CommandExecutionStatus::Failed,
                        aggregated_output: Some(exec_output.aggregated_output.text.clone()),
                        exit_code: Some(exec_output.exit_code),
                        duration: Some(exec_output.duration),
                    }),
                )
                .await;
            persist_user_shell_output(
                &session,
                turn_context.as_ref(),
                &raw_command,
                &exec_output,
                mode,
            )
            .await;
            true
        }
    }
}

async fn send_user_shell_error(session: &Session, turn_context: &TurnContext, message: &str) {
    session
        .send_event(
            turn_context,
            EventMsg::Error(ErrorEvent {
                misalignment: None,
                message: message.to_string(),
                codex_error_info: None,
                rejected_audio_file_ids: Vec::new(),
            }),
        )
        .await;
}

fn prepare_user_shell_exec_command(
    display_command: &[String],
    shell: &Shell,
    shell_snapshot: Option<&AbsolutePathBuf>,
    shell_environment_set: &HashMap<String, String>,
    exec_env_map: &mut HashMap<String, String>,
) -> Vec<String> {
    #[cfg(unix)]
    {
        prepare_user_shell_exec_command_with_path_prepend(
            display_command,
            shell,
            shell_snapshot,
            shell_environment_set,
            exec_env_map,
            apply_package_path_prepend,
        )
    }

    #[cfg(not(unix))]
    {
        maybe_wrap_shell_lc_with_snapshot(
            display_command,
            shell,
            shell_snapshot,
            shell_environment_set,
            exec_env_map,
            // On non-Unix targets, arg0 has already prepended the package path
            // to the process PATH before create_env() builds exec_env_map.
            // RuntimePathPrepends is only needed for Unix shell snapshot replay.
            &RuntimePathPrepends::default(),
        )
    }
}

/// Prepares a user-shell command after adding runtime-owned PATH entries.
///
/// The callback mutates the live exec environment for commands that are not
/// wrapped with a shell snapshot and records only the runtime-owned entries so
/// snapshot wrapping can reapply them after restoring the user's snapshot PATH.
#[cfg(unix)]
fn prepare_user_shell_exec_command_with_path_prepend(
    display_command: &[String],
    shell: &Shell,
    shell_snapshot: Option<&AbsolutePathBuf>,
    shell_environment_set: &HashMap<String, String>,
    exec_env_map: &mut HashMap<String, String>,
    prepend_runtime_path: impl FnOnce(&mut HashMap<String, String>, &mut RuntimePathPrepends),
) -> Vec<String> {
    let explicit_env_overrides = shell_environment_set.clone();
    let mut runtime_path_prepends = RuntimePathPrepends::default();
    prepend_runtime_path(exec_env_map, &mut runtime_path_prepends);
    maybe_wrap_shell_lc_with_snapshot(
        display_command,
        shell,
        shell_snapshot,
        &explicit_env_overrides,
        exec_env_map,
        &runtime_path_prepends,
    )
}

async fn persist_user_shell_output(
    session: &Session,
    turn_context: &TurnContext,
    raw_command: &str,
    exec_output: &ExecToolCallOutput,
    mode: UserShellCommandMode,
) {
    let output_item = user_shell_command_record_item(raw_command, exec_output, turn_context);

    if mode == UserShellCommandMode::StandaloneTurn {
        session
            .record_conversation_items(
                turn_context,
                turn_context.model_info(),
                std::slice::from_ref(&output_item),
            )
            .await;
        // Standalone shell turns can run before any regular user turn, so
        // explicitly materialize rollout persistence after recording output.
        session
            .ensure_rollout_materialized(PersistContext::Standard)
            .await;
        return;
    }

    session
        .inject_no_new_turn(vec![output_item], Some(turn_context))
        .await;
    // Output is retained before waking the original pending invocation. A child
    // outcome already claimed by that invocation keeps its own delivery fence.
    if session.multi_agent_version() == Some(codex_protocol::protocol::MultiAgentVersion::V2) {
        let runtime = &session.services.local_agent_runtime;
        let result = async {
            if let Some(wait) = runtime
                .work_ledger()?
                .current_wait(session.thread_id)
                .await
                .map_err(crate::agent::control::work_lifecycle::work_error)?
                && wait.origin.turn_id() == turn_context.sub_id
                && wait.phase == codex_state::coordination_wait::WaitPhase::Registered
            {
                runtime
                    .cancel_native_wait_with_cause(
                        &wait,
                        codex_state::coordination_wait::WaitCancellation::ShellOutput {
                            turn_id: turn_context.sub_id.clone(),
                        },
                    )
                    .await?;
            }
            CodexResult::Ok(())
        }
        .await;
        if let Err(error) = result {
            tracing::warn!(%error, "shell output could not settle the pending wait");
        }
    }
}

#[cfg(all(test, unix))]
#[path = "user_shell_tests.rs"]
mod tests;

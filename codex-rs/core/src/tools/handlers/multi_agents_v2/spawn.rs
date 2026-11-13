use super::*;
use crate::agent::role::DEFAULT_ROLE_NAME;
use crate::agent::types::SpawnAgentForkMode;
use crate::tools::handlers::multi_agents::collab_tool_call_status;
use crate::tools::handlers::multi_agents_spec::SpawnAgentToolOptions;
use crate::tools::handlers::multi_agents_spec::create_spawn_agent_tool_v2;
use crate::tools::handlers::multi_agents_v2::message_tool::message_content;
use crate::turn_timing::now_unix_timestamp_ms;
use codex_protocol::protocol::MultiAgentVersion;
use codex_tools::ToolSpec;
use std::num::NonZeroUsize;

#[derive(Default)]
pub(crate) struct Handler {
    options: SpawnAgentToolOptions,
    description_override: Option<String>,
}

impl Handler {
    pub(crate) fn new(
        options: SpawnAgentToolOptions,
        description_override: Option<String>,
    ) -> Self {
        Self {
            options,
            description_override,
        }
    }
}

impl ToolExecutor<ToolInvocation> for Handler {
    fn tool_name(&self) -> ToolName {
        ToolName::plain("spawn_agent")
    }

    fn spec(&self) -> ToolSpec {
        create_spawn_agent_tool_v2(self.options.clone(), self.description_override.as_deref())
    }

    fn handle<'a>(&'a self, invocation: ToolInvocation) -> codex_tools::ToolExecutorFuture<'a>
    where
        ToolInvocation: 'a,
    {
        Box::pin(async move {
            let analytics = invocation.session.services.analytics_events_client.clone();
            let sender_thread_id = invocation.session.thread_id;
            let turn_id = invocation.step_context.turn.sub_id.clone();
            let call_id = invocation.call_id.clone();
            let started_at_ms = now_unix_timestamp_ms();
            let result = handle_spawn_agent(invocation).await;
            let completed_at_ms = now_unix_timestamp_ms();
            let (status, receiver_thread_ids, agents_states) = match &result {
                Ok((_, spawned)) => (
                    collab_tool_call_status(&spawned.agent.status, Some(spawned.agent.thread_id)),
                    vec![spawned.agent.thread_id],
                    [(spawned.agent.thread_id, spawned.agent.status.clone())]
                        .into_iter()
                        .collect(),
                ),
                Err(_) => (
                    CollabAgentToolCallStatus::Failed,
                    Vec::new(),
                    Default::default(),
                ),
            };
            let agent_snapshot = result.as_ref().ok().map(|(_, spawned)| spawned);

            analytics.track_collab_tool_call(
                turn_id,
                CollabAgentToolCallItem {
                    id: call_id,
                    tool: CollabAgentTool::SpawnAgent,
                    status,
                    sender_thread_id,
                    receiver_thread_ids,
                    receiver_agents: Vec::new(),
                    timeout_ms: None,
                    prompt: None,
                    model: agent_snapshot.map(|snapshot| snapshot.config.model.clone()),
                    reasoning_effort: agent_snapshot
                        .and_then(|snapshot| snapshot.config.reasoning_effort.clone()),
                    agents_states,
                },
                started_at_ms,
                completed_at_ms,
            );

            result.map(|(output, _)| boxed_tool_output(output))
        })
    }
}

async fn handle_spawn_agent(
    invocation: ToolInvocation,
) -> Result<(SpawnAgentResult, crate::thread_spawn::ThreadSpawnResult), FunctionCallError> {
    let ToolInvocation {
        session,
        step_context,
        payload,
        call_id,
        source,
        ..
    } = invocation;
    let turn = &step_context.turn;
    let args: SpawnAgentArgs = parse_arguments(&function_arguments(payload)?)?;
    let role = args.agent_type.clone();
    let fork_mode = args.fork_mode()?;
    let (request, message) = args.into_request()?;
    let caller = crate::agent::child_config::SpawnCallerContext::capture(&session, &step_context)
        .await
        .map_err(FunctionCallError::RespondToModel)?;
    let identity = crate::thread_spawn::SpawnCallerIdentity {
        actor_id: session.thread_id,
        source: turn.session_source.clone(),
        turn_id: turn.sub_id.clone(),
        call_id: call_id.clone(),
        root_turn_id: turn.turn_metadata_state.root_turn_id(),
        turn_trigger: turn.turn_metadata_state.current_turn_trigger(),
        disabled_plugin_ids: turn.disabled_plugin_ids.clone(),
        cyber_access_program: turn.cyber_access_program,
    };
    let (settings, path) = crate::thread_spawn::prepare_from_caller(&caller, &identity, request)
        .await
        .map_err(|error| FunctionCallError::RespondToModel(error.to_string()))?;
    let control = session
        .services
        .local_agent_runtime
        .control(session.services.agent_control.identity());
    let spawned = control
        .spawn_prepared_actor(
            &identity,
            settings,
            path,
            agent_message_from_tool(message, &source),
        )
        .await
        .map_err(|error| {
            record_collab_spawn_failure(
                &turn.session_telemetry,
                turn.config.apps_mcp_product_sku.as_deref(),
                &error,
                &call_id,
                &turn.sub_id,
                fork_mode.as_ref(),
                MultiAgentVersion::V2,
            );
            collab_spawn_error(error)
        })?;
    emit_sub_agent_activity(
        &session,
        turn,
        SubAgentActivityItem {
            model: Some(spawned.config.model.clone()),
            reasoning_effort: spawned.config.reasoning_effort.clone(),
            id: call_id,
            agent_thread_id: spawned.agent.thread_id,
            agent_path: spawned.task_name.clone(),
            kind: SubAgentActivityKind::Started,
        },
    )
    .await;
    turn.session_telemetry.counter(
        "codex.multi_agent.spawn",
        1,
        &[
            ("role", role.as_deref().unwrap_or(DEFAULT_ROLE_NAME)),
            ("version", "v2"),
        ],
    );
    let task_name = spawned.task_name.to_string();
    let thread_id = spawned.agent.thread_id.to_string();
    let output = if turn.config.multi_agent_v2.hide_spawn_agent_metadata {
        SpawnAgentResult::HiddenMetadata {
            task_name,
            thread_id,
        }
    } else {
        SpawnAgentResult::WithNickname {
            task_name,
            thread_id,
            nickname: spawned.agent.metadata.agent_nickname.clone(),
        }
    };
    Ok((output, spawned))
}

impl CoreToolRuntime for Handler {
    fn matches_kind(&self, payload: &ToolPayload) -> bool {
        matches!(payload, ToolPayload::Function { .. })
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SpawnAgentArgs {
    message: String,
    task_name: String,
    agent_type: Option<String>,
    model: Option<String>,
    reasoning_effort: Option<ReasoningEffort>,
    cwd: Option<String>,
    #[serde(
        default,
        deserialize_with = "codex_protocol::protocol::ReloadSelection::deserialize_optional"
    )]
    preload: Option<codex_protocol::protocol::ReloadSelection>,
    fork_turns: Option<String>,
    fork_context: Option<bool>,
}

impl SpawnAgentArgs {
    pub(crate) fn into_request(
        self,
    ) -> Result<(crate::thread_spawn::ThreadCreateRequest, String), FunctionCallError> {
        let fork_mode = self.fork_mode()?;
        let message = message_content(self.message)?;
        Ok((
            crate::thread_spawn::ThreadCreateRequest {
                task_name: self.task_name,
                fork_mode,
                agent_type: self.agent_type,
                model: self.model,
                reasoning_effort: self.reasoning_effort,
                cwd: self.cwd,
                preload: self.preload,
            },
            message,
        ))
    }

    fn fork_mode(&self) -> Result<Option<SpawnAgentForkMode>, FunctionCallError> {
        if self.fork_context.is_some() {
            return Err(FunctionCallError::RespondToModel(
                "fork_context is not supported in MultiAgentV2; use fork_turns instead".to_string(),
            ));
        }

        let fork_turns = self
            .fork_turns
            .as_deref()
            .map(str::trim)
            .filter(|fork_turns| !fork_turns.is_empty())
            .unwrap_or("all");

        if fork_turns.eq_ignore_ascii_case("none") {
            return Ok(None);
        }
        // Accept legacy turn counts without limiting the inherited history.
        if fork_turns.eq_ignore_ascii_case("all") || fork_turns.parse::<NonZeroUsize>().is_ok() {
            return Ok(Some(SpawnAgentForkMode::FullHistory));
        }

        Err(FunctionCallError::RespondToModel(
            "fork_turns must be `none` or `all`".to_string(),
        ))
    }
}

#[derive(Debug, Serialize)]
#[serde(untagged)]
pub(crate) enum SpawnAgentResult {
    WithNickname {
        task_name: String,
        thread_id: String,
        nickname: Option<String>,
    },
    HiddenMetadata {
        task_name: String,
        thread_id: String,
    },
}

impl ToolOutput for SpawnAgentResult {
    fn log_output(&self) -> String {
        tool_output_json_text(self, "spawn_agent")
    }

    fn success_for_logging(&self) -> bool {
        true
    }

    fn to_response_item(&self, call_id: &str, payload: &ToolPayload) -> ResponseInputItem {
        tool_output_response_item(call_id, payload, self, Some(true), "spawn_agent")
    }

    fn code_mode_result(&self, _payload: &ToolPayload) -> JsonValue {
        tool_output_code_mode_result(self, "spawn_agent")
    }
}

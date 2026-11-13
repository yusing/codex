//! Shared native MAv2 child preparation and spawning.
//!
//! Callers supply captured step settings; native coordination owns child lifetimes.

use crate::agent::child_config::SpawnCallerContext;
use crate::agent::child_config::SpawnConfigOptions;
use crate::agent::child_config::SpawnConfigVersion;
use crate::agent::child_config::prepare_agent_spawn_config_from_caller;
use crate::agent::child_config::prepare_spawn_environments_from_caller;
use crate::agent::next_thread_spawn_depth;
use crate::agent::types::LiveAgent;
use crate::agent::types::SpawnAgentForkMode;
use crate::agent::types::SpawnAgentOptions;
use crate::codex_thread::ThreadConfigSnapshot;
use crate::config::Config;
use crate::session::multi_agents::resolve_usage_hints;
use crate::session::session::Session;
use crate::session::step_context::StepContext;
use codex_prompts::ResolvedModelMessages;
use codex_protocol::AgentPath;
use codex_protocol::error::CodexErr;
use codex_protocol::openai_models::ReasoningEffort;
use codex_protocol::protocol::MultiAgentVersion;
use codex_protocol::protocol::ReloadSelection;
use codex_protocol::protocol::SessionSource;
use codex_protocol::protocol::SubAgentSource;

/// Child creation options; omitted overrides retain native MAv2 inheritance and role rules.
pub struct ThreadCreateRequest {
    pub task_name: String,
    /// `None` starts fresh; `FullHistory` uses native fork preparation.
    pub fork_mode: Option<SpawnAgentForkMode>,
    pub agent_type: Option<String>,
    pub model: Option<String>,
    pub reasoning_effort: Option<ReasoningEffort>,
    /// Existing recipient working directory, resolved against the invoking environment.
    pub cwd: Option<String>,
    /// Explicit fresh-child sources, acquired before the first inference.
    pub preload: Option<ReloadSelection>,
}

pub(crate) struct ChildSpawnSettings {
    pub(crate) config: Config,
    pub(crate) source: SessionSource,
    pub(crate) options: SpawnAgentOptions,
}

/// Child runtime and its effective configuration.
pub struct ThreadSpawnResult {
    pub agent: LiveAgent,
    pub config: ThreadConfigSnapshot,
    pub task_name: AgentPath,
}

/// Separates preparation errors from backend failures to preserve native tool error reporting.
#[derive(Debug, thiserror::Error)]
pub enum ThreadSpawnError {
    #[error("{0}")]
    Preparation(String),
    #[error(transparent)]
    Backend(#[from] CodexErr),
}

/// Authenticated identity/attribution, separate from backend execution settings.
pub(crate) struct SpawnCallerIdentity {
    pub(crate) actor_id: codex_protocol::ThreadId,
    pub(crate) source: SessionSource,
    pub(crate) turn_id: String,
    pub(crate) call_id: String,
    pub(crate) root_turn_id: Option<String>,
    pub(crate) turn_trigger: Option<String>,
    pub(crate) disabled_plugin_ids: Vec<String>,
    pub(crate) cyber_access_program: Option<codex_protocol::turn_input::CyberAccessProgram>,
}

/// Prepare the native child settings for native coordination.
pub(crate) async fn prepare(
    session: &Session,
    step_context: &StepContext,
    call_id: &str,
    request: ThreadCreateRequest,
) -> Result<(ChildSpawnSettings, AgentPath), ThreadSpawnError> {
    let turn = &step_context.turn;
    let caller = SpawnCallerContext::capture(session, step_context)
        .await
        .map_err(ThreadSpawnError::Preparation)?;
    let identity = SpawnCallerIdentity {
        actor_id: session.thread_id,
        source: turn.session_source.clone(),
        turn_id: turn.sub_id.clone(),
        call_id: call_id.to_owned(),
        root_turn_id: turn.turn_metadata_state.root_turn_id(),
        turn_trigger: turn.turn_metadata_state.current_turn_trigger(),
        disabled_plugin_ids: turn.disabled_plugin_ids.clone(),
        cyber_access_program: turn.cyber_access_program,
    };
    prepare_from_caller(&caller, &identity, request).await
}

pub(crate) async fn prepare_from_caller(
    caller: &SpawnCallerContext,
    identity: &SpawnCallerIdentity,
    request: ThreadCreateRequest,
) -> Result<(ChildSpawnSettings, AgentPath), ThreadSpawnError> {
    let ThreadCreateRequest {
        task_name,
        fork_mode,
        agent_type,
        model,
        reasoning_effort,
        cwd,
        preload,
    } = request;
    if fork_mode.is_some() && preload.is_some() {
        return Err(ThreadSpawnError::Preparation(
            "Full-history forks cannot preload sources; omit preload or start fresh.".to_string(),
        ));
    }
    let role_name = agent_type
        .as_deref()
        .map(str::trim)
        .filter(|role| !role.is_empty());
    let options = SpawnConfigOptions {
        version: SpawnConfigVersion::V2,
        full_history_fork: matches!(fork_mode, Some(SpawnAgentForkMode::FullHistory)),
        role_name,
        model: model.as_deref(),
        reasoning_effort,
    };
    let prepared = prepare_agent_spawn_config_from_caller(caller, options)
        .await
        .map_err(ThreadSpawnError::Preparation)?;
    let mut config = prepared.config;
    let environments = prepare_spawn_environments_from_caller(
        &caller.environments,
        &caller.inherited_environments,
        cwd.as_deref(),
        &mut config,
    )
    .await
    .map_err(ThreadSpawnError::Preparation)?;
    if fork_mode.is_some() {
        // The public full-history request means the complete authorized calling prefix.
        config.multi_agent_v2.preserve_fork_prefix = true;
    }
    let task_name = identity
        .source
        .get_agent_path()
        .unwrap_or_else(AgentPath::root)
        .join(&task_name)
        .map_err(ThreadSpawnError::Preparation)?;
    let spawn_source = SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
        parent_thread_id: identity.actor_id,
        depth: next_thread_spawn_depth(&identity.source),
        agent_path: Some(task_name.clone()),
        agent_nickname: None,
        agent_role: prepared.role_name,
    });
    let multi_agent_v2_usage_hints = if matches!(fork_mode, Some(SpawnAgentForkMode::FullHistory))
        && caller.multi_agent_version == MultiAgentVersion::V2
    {
        let child_model_info = match config.model.as_deref() {
            Some(model) if model != caller.model_info.slug => Some(
                caller
                    .models
                    .get_model_info(model, &config.to_models_manager_config())
                    .await,
            ),
            _ => None,
        };
        let child_multi_agent_messages = ResolvedModelMessages::from_model(
            child_model_info.as_ref().unwrap_or(&caller.model_info),
        )
        .multi_agent();
        Some(resolve_usage_hints(
            &config.multi_agent_v2,
            child_multi_agent_messages,
            !config.update_plan_enabled && config.model_catalog.is_none(),
        ))
    } else {
        None
    };
    Ok((
        ChildSpawnSettings {
            config,
            source: spawn_source,
            options: SpawnAgentOptions {
                fork_parent_spawn_call_id: fork_mode.as_ref().map(|_| identity.call_id.clone()),
                fork_mode,
                parent_thread_id: Some(identity.actor_id),
                parent_turn_id: Some(identity.turn_id.clone()),
                root_turn_id: identity.root_turn_id.clone(),
                turn_trigger: identity.turn_trigger.clone(),
                environments: Some(environments),
                preload,
                disabled_plugin_ids: Some(identity.disabled_plugin_ids.clone()),
                multi_agent_v2_usage_hints,
                cyber_access_program: identity.cyber_access_program,
            },
        },
        task_name,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use codex_login::CodexAuth;
    use codex_utils_absolute_path::AbsolutePathBuf;
    use std::sync::Arc;
    use tokio_util::sync::CancellationToken;

    fn request() -> ThreadCreateRequest {
        ThreadCreateRequest {
            task_name: "worker".to_string(),
            fork_mode: None,
            agent_type: None,
            model: None,
            reasoning_effort: None,
            cwd: None,
            preload: None,
        }
    }

    #[tokio::test]
    async fn native_preparation_rejects_full_fork_overrides_before_admission() {
        let (session, turn) = crate::session::tests::make_session_and_context().await;
        let step = StepContext::for_test(Arc::new(turn));
        for mut request in [
            ThreadCreateRequest {
                model: Some("any-explicit-model".to_string()),
                ..request()
            },
            ThreadCreateRequest {
                reasoning_effort: Some(ReasoningEffort::Low),
                ..request()
            },
            ThreadCreateRequest {
                preload: Some(ReloadSelection {
                    sources: Vec::new(),
                }),
                ..request()
            },
        ] {
            request.fork_mode = Some(SpawnAgentForkMode::FullHistory);
            let result = prepare(&session, &step, "invalid-full-fork", request).await;
            assert!(matches!(result, Err(ThreadSpawnError::Preparation(_))));
        }
        assert_eq!(session.reload_selection().await, None);
    }

    #[tokio::test]
    async fn native_cwd_preparation_preserves_parent_and_captured_permissions() {
        let directory = tempfile::tempdir().unwrap();
        std::fs::create_dir(directory.path().join("recipient")).unwrap();
        std::fs::write(directory.path().join("not-directory"), "text").unwrap();
        let (session, turn, _events) =
            crate::session::tests::make_session_and_context_with_auth_and_config_and_rx(
                CodexAuth::from_api_key("test"),
                Vec::new(),
                |config| {
                    config.cwd = AbsolutePathBuf::from_absolute_path(directory.path()).unwrap();
                    config
                        .features
                        .enable(codex_features::Feature::MultiAgentV2)
                        .unwrap();
                },
            )
            .await;
        let step = session
            .capture_step_context(turn, &CancellationToken::new())
            .await
            .unwrap();
        let parent = step.environments.primary().unwrap();
        let parent_cwd = parent.cwd().clone();
        let parent_config = parent.config().clone();
        let (prepared, _) = prepare(
            &session,
            &step,
            "selected-cwd",
            ThreadCreateRequest {
                cwd: Some("recipient".to_string()),
                ..request()
            },
        )
        .await
        .unwrap();
        let child = prepared
            .options
            .environments
            .as_ref()
            .unwrap()
            .primary()
            .unwrap();
        assert_eq!(
            child.cwd().to_abs_path().unwrap().as_path(),
            directory.path().join("recipient")
        );
        assert_eq!(child.config(), &parent_config);
        assert_eq!(parent.cwd(), &parent_cwd);
        assert_eq!(
            child.config_origin,
            crate::environment_selection::EnvironmentConfigOrigin::Owner
        );
        for cwd in ["", "missing", "not-directory"] {
            assert!(
                prepare(
                    &session,
                    &step,
                    "invalid-cwd",
                    ThreadCreateRequest {
                        cwd: Some(cwd.to_string()),
                        ..request()
                    }
                )
                .await
                .is_err()
            );
        }
        assert_eq!(parent.cwd(), &parent_cwd);
    }

    #[tokio::test]
    async fn native_cwd_preparation_rejects_denied_directories() {
        use crate::config::PermissionProfileSnapshot;
        use crate::environment_selection::TurnEnvironmentState;
        use codex_protocol::models::PermissionProfile;
        use codex_protocol::permissions::FileSystemAccessMode;
        use codex_protocol::permissions::FileSystemPath;
        use codex_protocol::permissions::FileSystemSandboxEntry;

        let directory = tempfile::tempdir().unwrap();
        std::fs::create_dir(directory.path().join("denied")).unwrap();
        let (session, turn, _events) =
            crate::session::tests::make_session_and_context_with_auth_and_config_and_rx(
                CodexAuth::from_api_key("test"),
                Vec::new(),
                |config| {
                    config.cwd = AbsolutePathBuf::from_absolute_path(directory.path()).unwrap();
                },
            )
            .await;
        let mut step = StepContext::for_test(turn);
        let profile = PermissionProfile::read_only();
        let mut policy = profile.file_system_sandbox_policy();
        policy.entries.push(FileSystemSandboxEntry {
            path: FileSystemPath::GlobPattern {
                pattern: format!("{}/denied", directory.path().display()),
            },
            access: FileSystemAccessMode::Deny,
            missing_path_behavior: None,
        });
        let step_mut = Arc::get_mut(&mut step).unwrap();
        let TurnEnvironmentState::Ready(environment) = &mut step_mut.environments.environments[0]
        else {
            panic!("ready parent environment");
        };
        environment.config_mut().permission_profile = PermissionProfileSnapshot::legacy(
            PermissionProfile::from_runtime_permissions_with_enforcement(
                profile.enforcement(),
                &policy,
                profile.network_sandbox_policy(),
            ),
        );
        step_mut.inherited_environments = step_mut.environments.clone();
        assert!(
            prepare(
                &session,
                &step,
                "denied-cwd",
                ThreadCreateRequest {
                    cwd: Some("denied".to_string()),
                    ..request()
                }
            )
            .await
            .is_err()
        );
        assert_eq!(session.reload_selection().await, None);
    }

    #[tokio::test]
    async fn full_fork_applies_configured_subagent_defaults_before_named_role_overrides() {
        let directory = tempfile::tempdir().unwrap();
        let role_path = directory.path().join("role.toml");
        std::fs::write(&role_path, "model = \"role-model\"\nmodel_reasoning_effort = \"minimal\"\ndeveloper_instructions = \"Role instructions\"\n").unwrap();
        let (session, turn, _events) =
            crate::session::tests::make_session_and_context_with_auth_and_config_and_rx(
                CodexAuth::from_api_key("test"),
                Vec::new(),
                |config| {
                    config
                        .features
                        .enable(codex_features::Feature::MultiAgentV2)
                        .unwrap();
                    config.agent_default_subagent_model = Some("gpt-5.5".to_string());
                    config.agent_default_subagent_reasoning_effort = Some(ReasoningEffort::Low);
                    config.multi_agent_v2.preserve_fork_prefix = false;
                    config.agent_roles.insert(
                        "reader".to_string(),
                        crate::config::AgentRoleConfig {
                            description: None,
                            config_file: Some(role_path),
                            nickname_candidates: None,
                        },
                    );
                },
            )
            .await;
        let step = StepContext::for_test(turn);
        let (defaults, _) = prepare(
            &session,
            &step,
            "full-fork-defaults",
            ThreadCreateRequest {
                fork_mode: Some(SpawnAgentForkMode::FullHistory),
                ..request()
            },
        )
        .await
        .unwrap();
        assert_eq!(defaults.config.model.as_deref(), Some("gpt-5.5"));
        assert_eq!(
            defaults.config.model_reasoning_effort,
            Some(ReasoningEffort::Low)
        );
        let (prepared, _) = prepare(
            &session,
            &step,
            "full-fork",
            ThreadCreateRequest {
                fork_mode: Some(SpawnAgentForkMode::FullHistory),
                agent_type: Some("reader".to_string()),
                ..request()
            },
        )
        .await
        .unwrap();
        assert_eq!(prepared.config.model.as_deref(), Some("role-model"));
        assert_eq!(
            prepared.config.model_reasoning_effort,
            Some(ReasoningEffort::Minimal)
        );
        assert_eq!(
            prepared.config.developer_instructions.as_deref(),
            Some("Role instructions")
        );
        assert!(prepared.config.multi_agent_v2.preserve_fork_prefix);
    }
}

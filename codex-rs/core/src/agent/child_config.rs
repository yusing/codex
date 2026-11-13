//! Prepares child configuration from captured step settings and requested overrides.
//!
//! Spawn and reload share live runtime policy; role and model precedence, full-history
//! inheritance, and validation messages remain the same for each multi-agent version.

use crate::agent::host::AgentRuntimeServices;
use crate::agent::role::DEFAULT_ROLE_NAME;
use crate::agent::role::apply_role_to_config_with_model_selection;
use crate::config::Config;
use crate::environment_selection::EnvironmentConfigOrigin;
use crate::environment_selection::TurnEnvironmentSnapshot;
use crate::environment_selection::TurnEnvironmentState;
use crate::session::session::Session;
use crate::session::step_context::StepContext;
use crate::session::turn_context::TurnContext;
use crate::tools::handlers::resolve_tool_environment;
use codex_features::Feature;
use codex_file_system::EnvironmentAccess;
use codex_file_system::GetMetadataOptions;
use codex_models_manager::manager::RefreshStrategy;
use codex_models_manager::manager::SharedModelsManager;
use codex_protocol::config_types::SERVICE_TIER_DEFAULT_REQUEST_VALUE;
use codex_protocol::models::BaseInstructions;
use codex_protocol::openai_models::CatalogModel;
use codex_protocol::openai_models::ModelOrigin;
use codex_protocol::openai_models::ModelPreset;
use codex_protocol::openai_models::ReasoningEffort;
use codex_protocol::openai_models::ReasoningEffortPreset;
use codex_protocol::protocol::MultiAgentVersion;
use std::sync::Arc;

pub(crate) const MAX_SPAWN_AGENT_MODEL_OVERRIDES: usize = 5;

pub(crate) fn model_supports_multi_agent_backend(
    model: &ModelPreset,
    multi_agent_version: MultiAgentVersion,
) -> bool {
    multi_agent_version != MultiAgentVersion::V2
        || model.multi_agent_version != Some(MultiAgentVersion::Disabled)
}

/// Selects the existing spawn tool's role-inheritance rules.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum SpawnConfigVersion {
    V1,
    V2,
}

pub(crate) struct SpawnConfigOptions<'a> {
    pub(crate) version: SpawnConfigVersion,
    pub(crate) full_history_fork: bool,
    pub(crate) role_name: Option<&'a str>,
    pub(crate) model: Option<&'a str>,
    pub(crate) reasoning_effort: Option<ReasoningEffort>,
}

pub(crate) struct PreparedSpawnConfig {
    pub(crate) config: Config,
    pub(crate) role_name: Option<String>,
}

/// Owned exact-invocation inputs for native child preparation.
#[derive(Clone)]
pub(crate) struct SpawnCallerContext {
    pub(crate) config: Config,
    pub(crate) models: SharedModelsManager,
    pub(crate) model_info: Arc<CatalogModel>,
    pub(crate) environments: TurnEnvironmentSnapshot,
    pub(crate) inherited_environments: TurnEnvironmentSnapshot,
    pub(crate) shared_service_tier: Option<Option<String>>,
    pub(crate) multi_agent_version: MultiAgentVersion,
    pub(crate) developer_instructions: Option<String>,
}

impl SpawnCallerContext {
    pub(crate) async fn capture(session: &Session, step: &StepContext) -> Result<Self, String> {
        let mut config = build_agent_spawn_config(&session.get_base_instructions().await, step)?;
        let shared = match &session.agent_runtime {
            AgentRuntimeServices::Controller => true,
            AgentRuntimeServices::Host(services) => services.service_tier.is_some(),
        };
        let shared_service_tier = if shared {
            Some(crate::agent::host::service_tier(session, config.service_tier.clone()).await)
        } else {
            config.service_tier = session.thread_settings_snapshot().await.service_tier;
            None
        };
        Ok(Self {
            config,
            models: session.services.models_manager.clone(),
            model_info: step.settings.model_info.clone(),
            environments: step.environments.clone(),
            inherited_environments: step.inherited_environments.clone(),
            shared_service_tier,
            multi_agent_version: step.turn.multi_agent_version,
            developer_instructions: step.turn.developer_instructions.clone(),
        })
    }
}

/// Resolves child settings before starting the thread, retaining the invoking tool's precedence.
pub(crate) async fn prepare_agent_spawn_config(
    session: &Session,
    step_context: &StepContext,
    options: SpawnConfigOptions<'_>,
) -> Result<PreparedSpawnConfig, String> {
    let caller = SpawnCallerContext::capture(session, step_context).await?;
    prepare_agent_spawn_config_from_caller(&caller, options).await
}

pub(crate) async fn prepare_agent_spawn_config_from_caller(
    caller: &SpawnCallerContext,
    options: SpawnConfigOptions<'_>,
) -> Result<PreparedSpawnConfig, String> {
    if options.version == SpawnConfigVersion::V2
        && options.full_history_fork
        && (options.model.is_some() || options.reasoning_effort.is_some())
    {
        return Err(
            "Full-history forks inherit model and reasoning effort; omit these overrides or start fresh.".to_string(),
        );
    }
    let mut config = caller.config.clone();
    let inherited_ultra = options.version == SpawnConfigVersion::V2
        && config.model_reasoning_effort == Some(ReasoningEffort::Ultra);
    if inherited_ultra {
        config.model_reasoning_effort = Some(
            caller
                .model_info
                .resolve_reasoning_effort(ReasoningEffort::Ultra),
        );
    }
    let inherited_ultra = inherited_ultra
        && options.model.is_none()
        && options.reasoning_effort.is_none()
        && config.agent_default_subagent_model.is_none()
        && config.agent_default_subagent_reasoning_effort.is_none();
    if options.version == SpawnConfigVersion::V1 && options.full_history_fork {
        reject_full_fork_agent_type_override(options.role_name)?;
    }
    apply_requested_spawn_agent_model_overrides(
        caller,
        &mut config,
        options.model,
        options.reasoning_effort.clone(),
    )
    .await?;
    if !options.full_history_fork
        || (options.version == SpawnConfigVersion::V2 && options.role_name.is_some())
    {
        apply_spawn_agent_role(
            &caller.models,
            &mut config,
            options.role_name,
            inherited_ultra,
        )
        .await?;
        if options.version == SpawnConfigVersion::V2
            && options.full_history_fork
            && config.developer_instructions.is_none()
        {
            config
                .developer_instructions
                .clone_from(&caller.developer_instructions);
        }
    }
    if let Some(tier) = &caller.shared_service_tier {
        apply_service_tier(&caller.models, &mut config, tier.clone()).await?;
    }
    // Roles cannot replace the invocation's runtime authority.
    config
        .permissions
        .approval_policy
        .set(caller.config.permissions.approval_policy.value())
        .map_err(|err| format!("approval_policy is invalid: {err}"))?;
    config.approvals_reviewer = caller.config.approvals_reviewer;
    config.cwd = caller.config.cwd.clone();
    config
        .permissions
        .set_permission_profile_from_session_snapshot(
            caller
                .config
                .permissions
                .permission_profile_state()
                .snapshot(),
        )
        .map_err(|err| format!("permission_profile is invalid: {err}"))?;

    // Remember an applied configured default so cold reload reapplies its restrictions.
    let role_name = options
        .role_name
        .or_else(|| {
            (options.version == SpawnConfigVersion::V2
                && !options.full_history_fork
                && config
                    .agent_roles
                    .get(DEFAULT_ROLE_NAME)
                    .is_some_and(|role| role.config_file.is_some()))
            .then_some(DEFAULT_ROLE_NAME)
        })
        .map(str::to_owned);
    Ok(PreparedSpawnConfig { config, role_name })
}

/// Change only the child's selected cwd. The native accessor admits an existing directory;
/// the captured environment retains all permissions, roots and executor configuration.
pub(crate) async fn prepare_spawn_environments_from_caller(
    authority: &TurnEnvironmentSnapshot,
    inherited: &TurnEnvironmentSnapshot,
    cwd: Option<&str>,
    config: &mut Config,
) -> Result<TurnEnvironmentSnapshot, String> {
    let mut environments = inherited.clone();
    let Some(cwd) = cwd else {
        return Ok(environments);
    };
    if cwd.is_empty() {
        return Err("child cwd must name an existing directory".to_string());
    }
    let environment =
        resolve_tool_environment(authority, None).map_err(|error| format!("child cwd: {error}"))?;
    let path = environment
        .cwd()
        .join(cwd)
        .map_err(|error| error.to_string())?;
    // Turn-local extra grants are not inherited by a new child. Validate against the
    // captured environment authority that will actually be carried into its startup.
    let access = environment.fs_accessor(None);
    let path = access
        .canonicalize(&path)
        .await
        .map_err(|error| format!("child cwd: {error}"))?;
    let metadata = access
        .get_metadata(&path, GetMetadataOptions::default())
        .await
        .map_err(|error| format!("child cwd: {error}"))?;
    if !metadata.is_directory {
        return Err("child cwd is not a directory".to_string());
    }
    let child_environment = environments
        .environments
        .iter_mut()
        .find_map(|candidate| match candidate {
            TurnEnvironmentState::Ready(candidate)
                if candidate.selection.environment_id == environment.selection.environment_id =>
            {
                Some(candidate)
            }
            _ => None,
        })
        .ok_or_else(|| "child cwd environment cannot be inherited".to_string())?;
    child_environment.selection.cwd = path.clone();
    // A new cwd must not turn a captured permission profile back into inferred defaults.
    child_environment.config_origin = EnvironmentConfigOrigin::Owner;
    if !environment.environment().is_remote() {
        config.cwd = path.to_abs_path().map_err(|error| error.to_string())?;
    }
    Ok(environments)
}

/// Builds the base config snapshot for a newly spawned sub-agent.
///
/// The returned config starts from the parent's effective config and then refreshes the
/// model selection and reasoning settings captured for the invoking step, plus the turn's
/// runtime approval policy, sandbox, and cwd. Role-specific overrides are layered
/// after this step; skipping this helper and cloning stale config state directly can send the child
/// agent out with the wrong provider or runtime policy.
pub(crate) fn build_agent_spawn_config(
    base_instructions: &BaseInstructions,
    step_context: &StepContext,
) -> Result<Config, String> {
    let mut config = build_agent_shared_config(step_context.turn.as_ref())?;
    let settings = &step_context.settings;
    config.model = Some(settings.model_info.slug.clone());
    config.model_reasoning_effort = settings.effective_reasoning_effort();
    config.model_reasoning_summary = Some(settings.reasoning_summary);
    config.base_instructions = Some(base_instructions.text.clone());
    config.base_instructions_provenance = base_instructions.provenance.clone();
    Ok(config)
}

pub(crate) fn build_agent_resume_config(turn: &TurnContext) -> Result<Config, String> {
    let mut config = build_agent_shared_config(turn)?;
    // For resume, keep base instructions sourced from rollout/session metadata.
    config.base_instructions = None;
    config.base_instructions_provenance = None;
    Ok(config)
}

fn build_agent_shared_config(turn: &TurnContext) -> Result<Config, String> {
    let base_config = turn.config.clone();
    let mut config = (*base_config).clone();
    // Preserve activation for history forks without freezing the parent's model-owned prompts.
    // Fresh child startup restores configured preferences from the retained snapshot.
    config.token_budget = turn.configured_token_budget.clone();
    if let Some(thread_namespaces) = &turn.overridden_direct_only_tool_namespaces {
        config
            .code_mode
            .direct_only_tool_namespaces
            .clone_from(thread_namespaces);
    }
    config.model = Some(turn.model_info().slug.clone());
    config.model_provider = turn.provider.info().clone();
    config.model_reasoning_effort = turn
        .reasoning_effort()
        .or(turn.model_info().default_reasoning_level.as_ref())
        .cloned();
    config.model_reasoning_summary = Some(turn.reasoning_summary());
    config.developer_instructions = turn.developer_instructions.clone();
    apply_child_preparation_defaults(&mut config, turn.multi_agent_version);
    apply_spawn_agent_runtime_overrides(&mut config, turn)?;

    Ok(config)
}

/// Normalization after native actor values have been captured.
pub(crate) fn apply_child_preparation_defaults(config: &mut Config, version: MultiAgentVersion) {
    if config
        .features
        .enabled(Feature::SubagentDefaultContextLimits)
    {
        config.model_context_window = None;
        config.model_auto_compact_token_limit = None;
    }
    if version == MultiAgentVersion::V2
        && let Some(instructions) = config
            .multi_agent_v2
            .subagent_developer_instructions
            .clone()
    {
        config.developer_instructions = Some(instructions);
    }
}

fn reject_full_fork_agent_type_override(agent_type: Option<&str>) -> Result<(), String> {
    if agent_type.is_some() {
        return Err(
            "Full-history forked agents inherit the parent agent type; omit agent_type, or spawn without a full-history fork.".to_string());
    }
    Ok(())
}

/// Copies runtime-only turn state onto a child config before it is handed to `AgentControl`.
///
/// These values are chosen by the live turn rather than persisted config, so leaving them stale can
/// make a child agent disagree with its parent about approval policy, cwd, or sandboxing.
fn apply_spawn_agent_runtime_overrides(
    config: &mut Config,
    turn: &TurnContext,
) -> Result<(), String> {
    config
        .permissions
        .approval_policy
        .set(turn.approval_policy())
        .map_err(|err| format!("approval_policy is invalid: {err}"))?;
    config.approvals_reviewer = turn.config.approvals_reviewer;
    #[allow(deprecated)]
    let turn_cwd = turn.cwd.clone();
    config.cwd = turn_cwd;
    config
        .permissions
        .set_permission_profile_from_session_snapshot(
            turn.config
                .permissions
                .permission_profile_state()
                .snapshot(),
        )
        .map_err(|err| format!("permission_profile is invalid: {err}"))?;
    Ok(())
}

async fn apply_requested_spawn_agent_model_overrides(
    caller: &SpawnCallerContext,
    config: &mut Config,
    requested_model: Option<&str>,
    requested_reasoning_effort: Option<ReasoningEffort>,
) -> Result<(), String> {
    let requested_model = requested_model.or(caller.config.agent_default_subagent_model.as_deref());
    let requested_reasoning_effort = requested_reasoning_effort.or_else(|| {
        caller
            .config
            .agent_default_subagent_reasoning_effort
            .clone()
    });
    if requested_model.is_none() && requested_reasoning_effort.is_none() {
        return Ok(());
    }

    if let Some(requested_model) = requested_model {
        let available_models = caller
            .models
            .list_models(RefreshStrategy::Offline, config.http_client_factory())
            .await;
        let selected_model_name = find_spawn_agent_model_name(
            &available_models,
            requested_model,
            caller.multi_agent_version,
        )?;
        let selected_model_info = caller
            .models
            .get_model_info(&selected_model_name, &config.to_models_manager_config())
            .await;

        config.model = Some(selected_model_name.clone());
        if let Some(reasoning_effort) = requested_reasoning_effort {
            validate_spawn_agent_reasoning_effort(
                &selected_model_name,
                &selected_model_info.supported_reasoning_levels,
                &reasoning_effort,
            )?;
            config.model_reasoning_effort = Some(reasoning_effort);
        } else {
            config.model_reasoning_effort = selected_model_info.default_reasoning_level.clone();
        }

        return Ok(());
    }

    if let Some(reasoning_effort) = requested_reasoning_effort {
        validate_spawn_agent_reasoning_effort(
            &caller.model_info.slug,
            &caller.model_info.supported_reasoning_levels,
            &reasoning_effort,
        )?;
        config.model_reasoning_effort = Some(reasoning_effort);
    }

    Ok(())
}

async fn apply_service_tier(
    models: &SharedModelsManager,
    config: &mut Config,
    tier: Option<String>,
) -> Result<(), String> {
    let Some(service_tier) = tier else {
        config.service_tier = None;
        return Ok(());
    };
    if service_tier == SERVICE_TIER_DEFAULT_REQUEST_VALUE {
        config.service_tier = Some(service_tier);
        return Ok(());
    }

    let model = config.model.clone().ok_or_else(|| {
        "spawn_agent could not resolve the child model for service tier validation".to_string()
    })?;
    let model_info = models
        .get_model_info(model.as_str(), &config.to_models_manager_config())
        .await;

    config.service_tier = model_info
        .supports_service_tier(service_tier.as_str())
        .then_some(service_tier);
    Ok(())
}

async fn apply_spawn_agent_role(
    models: &SharedModelsManager,
    config: &mut Config,
    role_name: Option<&str>,
    inherited_ultra: bool,
) -> Result<(), String> {
    let previous_model = config.model.clone();
    let previous_reasoning_effort = config.model_reasoning_effort.clone();
    let selection = apply_role_to_config_with_model_selection(config, role_name).await?;
    if config.model == previous_model && config.model_reasoning_effort == previous_reasoning_effort
    {
        return Ok(());
    }
    if config.model_reasoning_effort.is_none() {
        return Ok(());
    }
    let Some(model) = config.model.clone() else {
        return Err(
            "spawn_agent could not resolve the child model for reasoning effort validation"
                .to_owned(),
        );
    };
    let model_info = models
        .get_model_info(&model, &config.to_models_manager_config())
        .await;
    if inherited_ultra && !selection.effort {
        config.model_reasoning_effort =
            Some(model_info.resolve_reasoning_effort(ReasoningEffort::Ultra));
    }
    if let Some(effort) = &config.model_reasoning_effort
        && model_info.origin != ModelOrigin::Synthetic
    {
        validate_spawn_agent_reasoning_effort(
            &model,
            &model_info.supported_reasoning_levels,
            effort,
        )?;
    }
    Ok(())
}

fn find_spawn_agent_model_name(
    available_models: &[ModelPreset],
    requested_model: &str,
    multi_agent_version: MultiAgentVersion,
) -> Result<String, String> {
    available_models
        .iter()
        .find(|model| {
            model.model == requested_model
                && model_supports_multi_agent_backend(model, multi_agent_version)
        })
        .map(|model| model.model.clone())
        .ok_or_else(|| {
            let available = available_models
                .iter()
                .filter(|model| model.show_in_picker)
                .filter(|model| model_supports_multi_agent_backend(model, multi_agent_version))
                .take(MAX_SPAWN_AGENT_MODEL_OVERRIDES)
                .map(|model| model.model.as_str())
                .collect::<Vec<_>>()
                .join(", ");
            format!(
                "Unknown model `{requested_model}` for spawn_agent. Available models: {available}"
            )
        })
}

fn validate_spawn_agent_reasoning_effort(
    model: &str,
    supported_reasoning_levels: &[ReasoningEffortPreset],
    requested_reasoning_effort: &ReasoningEffort,
) -> Result<(), String> {
    if supported_reasoning_levels
        .iter()
        .any(|preset| &preset.effort == requested_reasoning_effort)
    {
        return Ok(());
    }

    let supported = supported_reasoning_levels
        .iter()
        .map(|preset| preset.effort.to_string())
        .collect::<Vec<_>>()
        .join(", ");
    Err(format!(
        "Reasoning effort `{requested_reasoning_effort}` is not supported for model `{model}`. Supported reasoning efforts: {supported}"
    ))
}

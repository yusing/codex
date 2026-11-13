use super::analytics::ToolCallAnalytics;
use super::*;
use crate::agent::control::foreign::ForeignResponse;
use crate::tools::handlers::multi_agents_spec::create_list_agents_tool;
use codex_app_server_protocol::AgentRuntimeStatus;
use codex_app_server_protocol::AgentTreeObservation;
use codex_app_server_protocol::AgentWaitObservation;
use codex_app_server_protocol::AgentWorkObservation;
use codex_protocol::AgentPath;
use codex_protocol::ThreadId;
use codex_protocol::agent_foreign::AgentForeignOperation;
use codex_tools::ToolSpec;

pub(crate) struct Handler;

impl ToolExecutor<ToolInvocation> for Handler {
    fn tool_name(&self) -> ToolName {
        ToolName::plain("list_agents")
    }

    fn supports_parallel_tool_calls(&self) -> bool {
        true
    }

    fn spec(&self) -> ToolSpec {
        create_list_agents_tool()
    }

    fn handle<'a>(&'a self, invocation: ToolInvocation) -> codex_tools::ToolExecutorFuture<'a>
    where
        ToolInvocation: 'a,
    {
        Box::pin(async move {
            let analytics = ToolCallAnalytics::new(&invocation, CollabAgentTool::ListAgents);
            let result = self.handle_call(invocation).await;
            analytics.finish(&result);
            result
        })
    }
}

impl Handler {
    async fn handle_call(
        &self,
        invocation: ToolInvocation,
    ) -> Result<Box<dyn crate::tools::context::ToolOutput>, FunctionCallError> {
        let ToolInvocation {
            session,
            turn,
            payload,
            call_id,
            cancellation_token,
            ..
        } = invocation;
        let arguments = function_arguments(payload)?;
        let args: ListAgentsArgs = parse_arguments(&arguments)?;
        let runtime = &session.services.local_agent_runtime;
        runtime.register_session_root(session.thread_id, turn.parent_thread_id);
        if let Some(session_id) = args.session_id.as_deref() {
            ThreadId::from_string(session_id).map_err(|_| {
                FunctionCallError::RespondToModel(
                    "session_id must be an exact native thread ID".to_owned(),
                )
            })?;
            if let Some(target) = runtime
                .classify_foreign_target(session.thread_id, session_id)
                .map_err(collab_spawn_error)?
            {
                let response = runtime
                    .route_foreign(
                        session.thread_id,
                        &turn.sub_id,
                        &call_id,
                        target,
                        AgentForeignOperation::List {
                            path_prefix: args.path_prefix,
                        },
                        cancellation_token,
                    )
                    .await
                    .map_err(|error| collab_v2_agent_error(target, error))?;
                let ForeignResponse::Listed { tree } = response else {
                    return Err(FunctionCallError::RespondToModel(
                        "foreign owner returned a non-list result".to_owned(),
                    ));
                };
                return Ok(boxed_tool_output(project_list_agents(tree, None)));
            }
        }
        let prefix = args
            .path_prefix
            .as_deref()
            .map(|prefix| {
                turn.session_source
                    .get_agent_path()
                    .unwrap_or_else(AgentPath::root)
                    .resolve(prefix)
                    .map_err(FunctionCallError::RespondToModel)
            })
            .transpose()?;
        let root = runtime.observation_root(session.thread_id).ok_or_else(|| {
            FunctionCallError::RespondToModel("caller is not an admitted agent".to_owned())
        })?;
        let tree = runtime
            .observe_agents(root)
            .await
            .map_err(collab_spawn_error)?;

        Ok(boxed_tool_output(project_list_agents(
            tree,
            prefix.as_ref(),
        )))
    }
}

/// The model list is the same projection for Codex, external and foreign callers.
/// Applied settings/context measurements belong to inspect, not every tree row.
pub(crate) fn project_list_agents(
    mut tree: AgentTreeObservation,
    prefix: Option<&AgentPath>,
) -> ListAgentsResult {
    tree.agents.retain(|agent| {
        prefix.is_none_or(|prefix| {
            prefix.is_root()
                || agent.path.as_deref().is_some_and(|path| {
                    path == prefix.as_str()
                        || path
                            .strip_prefix(prefix.as_str())
                            .is_some_and(|suffix| suffix.starts_with('/'))
                })
        })
    });
    let agents = tree
        .agents
        .into_iter()
        .map(|agent| ListedAgent {
            agent_name: agent.path.unwrap_or_else(|| agent.thread_id.clone()),
            agent_id: agent.thread_id,
            parent_agent_id: agent.parent_thread_id,
            role: agent.role,
            loaded: agent.loaded,
            runtime_status: agent.status,
            work: agent.work,
            wait: agent.wait,
        })
        .collect();
    ListAgentsResult { agents }
}

impl CoreToolRuntime for Handler {
    fn matches_kind(&self, payload: &ToolPayload) -> bool {
        matches!(payload, ToolPayload::Function { .. })
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ListAgentsArgs {
    pub(crate) path_prefix: Option<String>,
    pub(crate) session_id: Option<String>,
}

#[derive(Debug, Serialize)]
struct ListedAgent {
    agent_name: String,
    agent_id: String,
    parent_agent_id: Option<String>,
    role: Option<String>,
    loaded: bool,
    runtime_status: Option<AgentRuntimeStatus>,
    work: Option<AgentWorkObservation>,
    wait: Option<AgentWaitObservation>,
}

#[derive(Debug, Serialize)]
pub(crate) struct ListAgentsResult {
    agents: Vec<ListedAgent>,
}

impl ToolOutput for ListAgentsResult {
    fn log_output(&self) -> String {
        tool_output_json_text(self, "list_agents")
    }

    fn success_for_logging(&self) -> bool {
        true
    }

    fn to_response_item(&self, call_id: &str, payload: &ToolPayload) -> ResponseInputItem {
        tool_output_response_item(call_id, payload, self, Some(true), "list_agents")
    }

    fn code_mode_result(&self, _payload: &ToolPayload) -> JsonValue {
        tool_output_code_mode_result(self, "list_agents")
    }
}

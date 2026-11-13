use super::*;
use crate::agent::control::foreign::ForeignResponse;
use crate::tools::handlers::multi_agents_spec::create_inspect_agent_tool;
use codex_app_server_protocol::AgentObservation;
use codex_protocol::agent_foreign::AgentForeignOperation;
use codex_tools::ToolSpec;

pub(crate) struct Handler;

impl ToolExecutor<ToolInvocation> for Handler {
    fn tool_name(&self) -> ToolName {
        ToolName::plain("inspect_agent")
    }

    fn supports_parallel_tool_calls(&self) -> bool {
        true
    }

    fn spec(&self) -> ToolSpec {
        create_inspect_agent_tool()
    }

    fn handle<'a>(&'a self, invocation: ToolInvocation) -> codex_tools::ToolExecutorFuture<'a>
    where
        ToolInvocation: 'a,
    {
        Box::pin(async move {
            let ToolInvocation {
                session,
                turn,
                payload,
                call_id,
                cancellation_token,
                ..
            } = invocation;
            let args: InspectAgentArgs = parse_arguments(&function_arguments(payload)?)?;
            let runtime = &session.services.local_agent_runtime;
            runtime.register_session_root(session.thread_id, turn.parent_thread_id);
            if let Some(target) = runtime
                .classify_foreign_target(session.thread_id, &args.target)
                .map_err(collab_spawn_error)?
            {
                let response = runtime
                    .route_foreign(
                        session.thread_id,
                        &turn.sub_id,
                        &call_id,
                        target,
                        AgentForeignOperation::Inspect {},
                        cancellation_token,
                    )
                    .await
                    .map_err(|error| collab_v2_agent_error(target, error))?;
                let ForeignResponse::Inspected { agent } = response else {
                    return Err(FunctionCallError::RespondToModel(
                        "foreign owner returned a non-inspection result".to_owned(),
                    ));
                };
                return Ok(boxed_tool_output(InspectAgentResult(agent)));
            }
            let target = resolve_agent_target(&session, &turn, &args.target).await?;
            let agent = runtime
                .observe_agent(target)
                .await
                .map_err(|error| collab_v2_agent_error(target, error))?;
            Ok(boxed_tool_output(InspectAgentResult(agent)))
        })
    }
}

impl CoreToolRuntime for Handler {
    fn matches_kind(&self, payload: &ToolPayload) -> bool {
        matches!(payload, ToolPayload::Function { .. })
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct InspectAgentArgs {
    target: String,
}

#[derive(Serialize)]
#[serde(transparent)]
struct InspectAgentResult(AgentObservation);

impl ToolOutput for InspectAgentResult {
    fn log_output(&self) -> String {
        tool_output_json_text(self, "inspect_agent")
    }

    fn success_for_logging(&self) -> bool {
        true
    }

    fn to_response_item(&self, call_id: &str, payload: &ToolPayload) -> ResponseInputItem {
        tool_output_response_item(call_id, payload, self, Some(true), "inspect_agent")
    }

    fn code_mode_result(&self, _payload: &ToolPayload) -> JsonValue {
        tool_output_code_mode_result(self, "inspect_agent")
    }
}

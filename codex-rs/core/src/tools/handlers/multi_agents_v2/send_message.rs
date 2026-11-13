use super::analytics::ToolCallAnalytics;
use super::message_tool::SendMessageArgs;
use super::message_tool::handle_message_string_tool;
use super::*;
use crate::agent::control::foreign::ForeignResponse;
use crate::agent::types::MessageDeliveryMode;
use crate::tools::handlers::multi_agents_spec::create_send_message_tool;
use codex_protocol::agent_foreign::AgentForeignOperation;
use codex_tools::ToolSpec;

pub(crate) struct Handler;

impl ToolExecutor<ToolInvocation> for Handler {
    fn tool_name(&self) -> ToolName {
        ToolName::plain("send_message")
    }

    fn spec(&self) -> ToolSpec {
        create_send_message_tool()
    }

    fn handle<'a>(&'a self, invocation: ToolInvocation) -> codex_tools::ToolExecutorFuture<'a>
    where
        ToolInvocation: 'a,
    {
        Box::pin(async move {
            let mut analytics = ToolCallAnalytics::new(&invocation, CollabAgentTool::SendMessage);
            let result = self.handle_call(invocation, &mut analytics).await;
            analytics.finish(&result);
            result
        })
    }
}

impl Handler {
    async fn handle_call(
        &self,
        invocation: ToolInvocation,
        analytics: &mut ToolCallAnalytics,
    ) -> Result<Box<dyn crate::tools::context::ToolOutput>, FunctionCallError> {
        let arguments = function_arguments(invocation.payload.clone())?;
        let args: SendMessageArgs = parse_arguments(&arguments)?;
        let runtime = &invocation.session.services.local_agent_runtime;
        runtime.register_session_root(
            invocation.session.thread_id,
            invocation.turn.parent_thread_id,
        );
        if let Some(target) = runtime
            .classify_foreign_target(invocation.session.thread_id, &args.target)
            .map_err(collab_spawn_error)?
        {
            analytics.set_receiver(target);
            let message = super::message_tool::message_content(args.message)?;
            let operation = AgentForeignOperation::Message { message };
            let response = runtime
                .route_foreign(
                    invocation.session.thread_id,
                    &invocation.turn.sub_id,
                    &invocation.call_id,
                    target,
                    operation,
                    invocation.cancellation_token,
                )
                .await
                .map_err(|error| collab_v2_agent_error(target, error))?;
            return Ok(boxed_tool_output(ForeignMessageResult(response)));
        }
        handle_message_string_tool(
            invocation,
            MessageDeliveryMode::QueueOnly,
            args.target,
            args.message,
            analytics,
        )
        .await
        .map(boxed_tool_output)
    }
}

impl CoreToolRuntime for Handler {
    fn matches_kind(&self, payload: &ToolPayload) -> bool {
        matches!(payload, ToolPayload::Function { .. })
    }
}

#[derive(Serialize)]
#[serde(transparent)]
struct ForeignMessageResult(ForeignResponse);

impl ToolOutput for ForeignMessageResult {
    fn log_output(&self) -> String {
        tool_output_json_text(self, "send_message")
    }

    fn success_for_logging(&self) -> bool {
        true
    }

    fn to_response_item(&self, call_id: &str, payload: &ToolPayload) -> ResponseInputItem {
        tool_output_response_item(call_id, payload, self, Some(true), "send_message")
    }

    fn code_mode_result(&self, _payload: &ToolPayload) -> JsonValue {
        tool_output_code_mode_result(self, "send_message")
    }
}

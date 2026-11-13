use super::*;
use crate::tools::context::FunctionToolOutput;
use crate::tools::context::ToolCallSource;
use crate::tools::handlers::multi_agents_spec::WaitAgentTimeoutOptions;
use crate::tools::handlers::multi_agents_spec::create_wait_agent_tool_v2;
use codex_code_mode::CellId;
use codex_protocol::protocol::ReloadSelection;
use codex_state::coordination_wait::WaitMode;
use codex_state::coordination_wait::WaitOptions;
use codex_state::coordination_wait::WaitTimeout;
use codex_tools::ToolSpec;

#[derive(Default)]
pub(crate) struct Handler {
    options: WaitAgentTimeoutOptions,
}

impl Handler {
    pub(crate) fn new(options: WaitAgentTimeoutOptions) -> Self {
        Self { options }
    }
}

impl ToolExecutor<ToolInvocation> for Handler {
    fn tool_name(&self) -> ToolName {
        ToolName::plain("wait_agent")
    }

    fn spec(&self) -> ToolSpec {
        create_wait_agent_tool_v2(self.options)
    }

    fn handle<'a>(&'a self, invocation: ToolInvocation) -> codex_tools::ToolExecutorFuture<'a>
    where
        ToolInvocation: 'a,
    {
        Box::pin(self.handle_call(invocation))
    }
}

impl Handler {
    async fn handle_call(
        &self,
        invocation: ToolInvocation,
    ) -> Result<Box<dyn ToolOutput>, FunctionCallError> {
        let arguments = function_arguments(invocation.payload.clone())?;
        let args: WaitArgs = parse_arguments(&arguments)?;
        let config = &invocation.turn.config.multi_agent_v2;
        let options = args.into_options(config.min_wait_timeout_ms, config.max_wait_timeout_ms)?;
        let ToolCallSource::CodeMode { cell_id, .. } = &invocation.source else {
            return Err(FunctionCallError::RespondToModel(
                "wait_agent requires an enclosing JavaScript execution cell".to_owned(),
            ));
        };
        if invocation.cancellation_token.is_cancelled() {
            return Err(FunctionCallError::RespondToModel(
                "wait registration canceled".to_owned(),
            ));
        }
        let session = &invocation.session;
        let runtime = &session.services.local_agent_runtime;
        let ledger = runtime
            .work_ledger()
            .map_err(|error| FunctionCallError::RespondToModel(error.to_string()))?;
        let caller = ledger
            .work_stamp(session.thread_id)
            .await
            .map_err(|error| FunctionCallError::RespondToModel(error.to_string()))?
            .ok_or_else(|| {
                FunctionCallError::RespondToModel("caller has no accepted work".to_owned())
            })?;
        let paused = session
            .services
            .code_mode_service
            .dispatch_broker
            .begin_paused_wait(&CellId::new(cell_id.clone()), &invocation.turn.sub_id)
            .map_err(FunctionCallError::RespondToModel)?;
        let ticket = runtime
            .register_native_wait(caller, &paused.cell, &options)
            .await
            .map_err(|error| FunctionCallError::RespondToModel(error.to_string()))?;
        // Durable registration precedes publication. Repeated calls share the ticket
        // operation identity, so native feature-use reduction counts it only once.
        let item = CollabAgentToolCallItem {
            id: invocation.call_id,
            tool: CollabAgentTool::Wait,
            status: CollabAgentToolCallStatus::InProgress,
            sender_thread_id: session.thread_id,
            receiver_thread_ids: ticket.selected.iter().map(|work| work.actor_id).collect(),
            receiver_agents: Vec::new(),
            timeout_ms: ticket.options.timeout.as_ref().map(|timeout| timeout.effective_ms as u64),
            prompt: None,
            model: None,
            reasoning_effort: None,
            agents_states: Default::default(),
        };
        session
            .emit_turn_item_started(
                &invocation.turn,
                &TurnItem::CollabAgentToolCall(item.clone()),
            )
            .await;
        let result = async {
            if options.compact {
                ledger
                    .begin_wait_reset_origin(&ticket)
                    .await
                    .map_err(|error| FunctionCallError::RespondToModel(error.to_string()))?;
                let diagnostic = session
                    .reset_paused_wait(
                        &invocation.step_context,
                        options.reload.clone(),
                        ticket.id.to_string(),
                        &invocation.cancellation_token,
                    )
                    .await
                    .map_err(|error| FunctionCallError::RespondToModel(error.to_string()))?;
                ledger
                    .finish_wait_reset_origin(&ticket, diagnostic.as_deref())
                    .await
                    .map_err(|error| FunctionCallError::RespondToModel(error.to_string()))?;
            }
            let ready = runtime
                .await_wait(&ticket, &invocation.cancellation_token)
                .await
                .map_err(|error| FunctionCallError::RespondToModel(error.to_string()))?;
            runtime
                .wait_tool_result(&ready)
                .await
                .map_err(|error| FunctionCallError::RespondToModel(error.to_string()))
        }
        .await;
        if result.is_err() {
            let _ = runtime.cancel_native_wait(&ticket).await;
        }
        session
            .emit_turn_item_completed(
                &invocation.turn,
                TurnItem::CollabAgentToolCall(CollabAgentToolCallItem {
                    status: if result.is_ok() {
                        CollabAgentToolCallStatus::Completed
                    } else if invocation.cancellation_token.is_cancelled() {
                        CollabAgentToolCallStatus::Interrupted
                    } else {
                        CollabAgentToolCallStatus::Failed
                    },
                    ..item
                }),
            )
            .await;
        let text = result?;
        drop(paused);
        Ok(boxed_tool_output(FunctionToolOutput::from_text(text, None)))
    }
}

impl CoreToolRuntime for Handler {
    fn matches_kind(&self, payload: &ToolPayload) -> bool {
        matches!(payload, ToolPayload::Function { .. })
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct WaitArgs {
    timeout_ms: Option<i64>,
    #[serde(default)]
    compact: bool,
    reload: Option<ReloadSelection>,
}

impl WaitArgs {
    pub(crate) fn into_options(
        self,
        min_timeout_ms: i64,
        max_timeout_ms: i64,
    ) -> Result<WaitOptions, FunctionCallError> {
        if self.reload.is_some() && !self.compact {
            return Err(FunctionCallError::RespondToModel(
                "reload requires compact: true".to_owned(),
            ));
        }

        let timeout = self
            .timeout_ms
            .map(|requested_ms| {
                if requested_ms > max_timeout_ms {
                    return Err(FunctionCallError::RespondToModel(format!(
                        "timeout_ms must be at most {max_timeout_ms}"
                    )));
                }
                Ok(WaitTimeout {
                    requested_ms,
                    effective_ms: requested_ms.max(min_timeout_ms),
                })
            })
            .transpose()?;
        Ok(WaitOptions {
            mode: WaitMode::Any,
            timeout,
            progress_interval_ms: None,
            compact: self.compact,
            reload: self.reload,
        })
    }
}

#[cfg(test)]
#[path = "wait_tests.rs"]
mod tests;

use crate::function_tool::FunctionCallError;
use crate::session::step_context::StepContext;
use crate::tools::context::ToolInvocation;
use crate::tools::context::ToolPayload;
use crate::tools::context::boxed_tool_output;
use crate::tools::handlers::live_diff::ArgumentPreview;
use crate::tools::handlers::unified_exec::CodeModeEditProjector;
use crate::tools::registry::CoreToolRuntime;
use crate::tools::registry::ToolArgumentDiffConsumer;
use crate::tools::registry::ToolExecutor;
use crate::turn_timing::now_unix_timestamp_ms;
use codex_prompts::ResolvedModelMessages;
use codex_protocol::items::FunctionCallOutputItem;
use codex_protocol::items::TurnItem;
use codex_protocol::protocol::CodeModeInputDeltaEvent;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::ItemCompletedEvent;
use codex_tools::IndirectNamespacePrefixes;
use codex_tools::ToolName;
use codex_tools::ToolSpec;
use std::borrow::Cow;
use std::sync::Arc;

use super::ExecContext;
use super::PUBLIC_TOOL_NAME;
use super::clamp_code_mode_max_output_tokens;
use super::handle_runtime_response;
use super::is_exec_tool_name;
use super::output::CodeModeToolOutput;
use super::prepare_code_mode_tool_definitions;
use super::telemetry::CodeModeToolCallGuard;
use super::telemetry::trace_id;

type CodeModeNestedTool = (Arc<ToolSpec>, Option<Arc<dyn CoreToolRuntime>>, bool);

pub struct CodeModeExecuteHandler {
    spec: ToolSpec,
    nested_tool_specs: Vec<CodeModeNestedTool>,
}

impl CodeModeExecuteHandler {
    pub(crate) fn new(spec: ToolSpec, nested_tool_specs: Vec<CodeModeNestedTool>) -> Self {
        Self {
            spec,
            nested_tool_specs,
        }
    }

    async fn execute(
        &self,
        session: std::sync::Arc<crate::session::session::Session>,
        step_context: Arc<crate::session::step_context::StepContext>,
        call_id: String,
        originating_call: Option<crate::tools::context::ToolCallOrigin>,
        code: String,
        telemetry: &mut CodeModeToolCallGuard,
    ) -> Result<CodeModeToolOutput, FunctionCallError> {
        let args =
            codex_code_mode::parse_exec_source(&code).map_err(FunctionCallError::RespondToModel)?;
        let exec = ExecContext {
            session,
            turn: Arc::clone(&step_context.turn),
        };
        let code_mode_input_schema_max_bytes = step_context
            .turn
            .config
            .code_mode
            .tool_input_schema_max_bytes;
        let mut enabled_tools = Vec::with_capacity(self.nested_tool_specs.len());
        for (spec, cached_runtime, pauses_cell_deadline) in &self.nested_tool_specs {
            let mut definitions = prepare_code_mode_tool_definitions(
                cached_runtime.as_deref(),
                || Cow::Borrowed(spec.as_ref()),
                code_mode_input_schema_max_bytes,
            );
            if *pauses_cell_deadline {
                for definition in definitions.to_mut() {
                    definition.pauses_cell_deadline = true;
                }
            }
            match definitions {
                Cow::Borrowed(definitions) => enabled_tools.extend_from_slice(definitions),
                Cow::Owned(definitions) => enabled_tools.extend(definitions),
            }
        }
        enabled_tools.sort_by(|left, right| left.name.cmp(&right.name));
        enabled_tools.dedup_by(|left, right| left.name == right.name);
        let model_messages = ResolvedModelMessages::from_model(&step_context.settings.model_info);
        IndirectNamespacePrefixes::new(
            model_messages.indirect_description_prefixes(),
            step_context.tool_router.mcp_namespaces(),
        )
        .map_err(|error| FunctionCallError::Fatal(error.to_string()))?
        .apply_code_mode(&mut enabled_tools);
        let started_at = std::time::Instant::now();
        let max_output_tokens = clamp_code_mode_max_output_tokens(
            &step_context.settings.model_info,
            args.max_output_tokens,
            exec.turn.config.code_mode.experimental_show_cell_overhead,
        );
        let started_cell = exec
            .session
            .services
            .code_mode_service
            .execute(
                codex_code_mode::ExecuteRequest {
                    tool_call_id: call_id.clone(),
                    enabled_tools,
                    source: args.code.clone(),
                    yield_time_ms: args.yield_time_ms,
                    max_output_tokens,
                },
                Arc::clone(&step_context),
            )
            .await
            .map_err(FunctionCallError::RespondToModel)?;
        let cell_id = started_cell.cell_id.clone();
        tracing::Span::current().record("cell.id", trace_id(cell_id.as_str()));
        telemetry.cell_id = Some(cell_id.to_string());
        exec.session
            .services
            .analytics_events_client
            .track_code_mode_tool_call(codex_analytics::CodeModeToolCallFact::CellStarted {
                thread_id: exec.session.thread_id.to_string(),
                turn_id: exec.turn.sub_id.clone(),
                call_id: call_id.clone(),
                cell_id: cell_id.to_string(),
            });
        exec.session
            .services
            .executed_tool_calls
            .start_cell(&cell_id, &call_id);
        let runtime_cell_id = cell_id.to_string();
        let code_cell_trace = exec
            .session
            .services
            .rollout_thread_trace
            .start_code_cell_trace(
                exec.turn.sub_id.as_str(),
                runtime_cell_id.as_str(),
                call_id.as_str(),
                args.code.as_str(),
            );
        exec.session
            .services
            .code_mode_service
            .mark_cell_ready_for_dispatch(&cell_id, originating_call);
        let response = started_cell
            .initial_response()
            .await
            .map_err(FunctionCallError::RespondToModel)?;
        if let Some(code_mode_host_duration) = response.code_mode_host_duration() {
            telemetry.record_code_mode_host_duration(code_mode_host_duration);
        }
        // Record the raw runtime boundary. The model-visible custom-tool output
        // is produced by `handle_runtime_response` and later linked through
        // `CodeCell.output_item_ids` in the reduced trace.
        code_cell_trace.record_initial_response(&response);
        // Yielded cells keep running, so terminal lifecycle is only emitted
        // here when the first response also ended the runtime.
        let change_notices = exec
            .session
            .services
            .code_mode_service
            .take_cell_change_notices(&cell_id);
        if matches!(response, codex_code_mode::RuntimeResponse::Yielded { .. }) {
            telemetry.record_cell_timing(
                &cell_id,
                codex_extension_api::CodeModeCellTiming::ExecYielded,
            );
        } else {
            code_cell_trace.record_ended(&response);
            exec.session
                .services
                .executed_tool_calls
                .mark_terminal_capture_output(&cell_id, &call_id);
            exec.session
                .services
                .code_mode_service
                .finish_cell_dispatch(&cell_id);
            exec.session
                .services
                .analytics_events_client
                .track_code_mode_tool_call(codex_analytics::CodeModeToolCallFact::CellClosed {
                    thread_id: exec.session.thread_id.to_string(),
                    turn_id: exec.turn.sub_id.clone(),
                    cell_id: cell_id.to_string(),
                });
        }
        exec.session.services.elicitations.wait_until_clear().await;
        let wall_time = response
            .code_mode_host_duration()
            .unwrap_or_else(|| started_at.elapsed());
        if let codex_code_mode::RuntimeResponse::Result {
            error_text: Some(diagnostic),
            ..
        } = &response
        {
            // Persist the diagnostic as a completed tool item, not a failed
            // turn. The result to the model is independently budgeted.
            exec.session
                .send_event(
                    &exec.turn,
                    EventMsg::ItemCompleted(ItemCompletedEvent {
                        thread_id: exec.session.thread_id,
                        turn_id: exec.turn.sub_id.clone(),
                        item: TurnItem::FunctionCallOutput(
                            FunctionCallOutputItem::code_mode_failure(&call_id, diagnostic.clone()),
                        ),
                        content_token_count: None,
                        started_at_ms: None,
                        completed_at_ms: now_unix_timestamp_ms(),
                    }),
                )
                .await;
        }
        Ok(handle_runtime_response(
            &step_context.settings.model_info,
            response,
            max_output_tokens,
            exec.session.services.code_mode_service.output_truncation(),
            wall_time,
            exec.turn.config.code_mode.experimental_show_cell_overhead,
            Some(&codex_utils_output_truncation::truncated_output_dir(
                &crate::exec_env::session_tmp_path(exec.session.session_id()),
            )),
            &change_notices,
        ))
    }
}

impl ToolExecutor<ToolInvocation> for CodeModeExecuteHandler {
    fn tool_name(&self) -> ToolName {
        ToolName::plain(PUBLIC_TOOL_NAME)
    }

    fn spec(&self) -> ToolSpec {
        self.spec.clone()
    }

    fn handle<'a>(&'a self, invocation: ToolInvocation) -> codex_tools::ToolExecutorFuture<'a>
    where
        ToolInvocation: 'a,
    {
        Box::pin(self.handle_call(invocation))
    }
}

impl CodeModeExecuteHandler {
    // Default to interrupted if this future is dropped; telemetry::CodeModeToolCallGuard::finish
    // overwrites this handler's captured span on explicit success or failure.
    #[tracing::instrument(
        name = "code_mode.handler.execute",
        level = "info",
        skip_all,
        fields(
            conversation.id = %invocation.session.thread_id,
            turn_id = invocation.turn.sub_id.as_str(),
            call_id = trace_id(&invocation.call_id),
            cell.id = tracing::field::Empty,
            outcome = "interrupted",
        )
    )]
    async fn handle_call(
        &self,
        invocation: ToolInvocation,
    ) -> Result<Box<dyn crate::tools::context::ToolOutput>, FunctionCallError> {
        let handler_span = tracing::Span::current();
        let originating_call = invocation.originating_call().await;
        let ToolInvocation {
            session,
            turn,
            step_context,
            call_id,
            tool_name,
            payload,
            ..
        } = invocation;

        let mut telemetry = CodeModeToolCallGuard::new(
            &session,
            turn.sub_id.clone(),
            turn.turn_metadata_state.clone(),
            call_id.clone(),
            PUBLIC_TOOL_NAME,
            handler_span,
        );
        let result = match payload {
            ToolPayload::Custom { input } if is_exec_tool_name(&tool_name) => self
                .execute(
                    session,
                    step_context,
                    call_id,
                    originating_call,
                    input,
                    &mut telemetry,
                )
                .await
                .map(boxed_tool_output),
            _ => Err(FunctionCallError::RespondToModel(format!(
                "{PUBLIC_TOOL_NAME} expects raw JavaScript source text"
            ))),
        };
        telemetry.finish(
            result
                .as_ref()
                .is_ok_and(|output| output.success_for_logging()),
        );
        result
    }
}

impl CoreToolRuntime for CodeModeExecuteHandler {
    fn matches_kind(&self, payload: &ToolPayload) -> bool {
        matches!(payload, ToolPayload::Custom { .. })
    }

    fn create_diff_consumer(&self) -> Option<Box<dyn ToolArgumentDiffConsumer>> {
        Some(Box::<CodeModeInputPreview>::default())
    }
}

#[derive(Default)]
struct CodeModeInputPreview {
    offset: u32,
    edits: CodeModeEditProjector,
}

impl ToolArgumentDiffConsumer for CodeModeInputPreview {
    fn consume_diff(
        &mut self,
        step: &StepContext,
        call_id: String,
        diff: &str,
    ) -> Vec<ArgumentPreview> {
        const MAX_PREVIEW_BYTES: u32 = 256 << 10;
        let mut events = Vec::new();
        let cwd = step
            .environments
            .primary()
            .map(|env| env.selection.cwd.to_path_buf())
            .unwrap_or_default();
        if let Some(patch_event) = self.edits.consume_diff(&cwd, call_id.clone(), diff) {
            events.push(ArgumentPreview::Patch(patch_event));
        }
        let len = u32::try_from(diff.len()).unwrap_or(u32::MAX);
        if len == 0 {
            return events;
        }
        if len > MAX_PREVIEW_BYTES - self.offset {
            self.offset = MAX_PREVIEW_BYTES;
            return events;
        }
        let offset = self.offset;
        self.offset += len;
        events.push(ArgumentPreview::Code(CodeModeInputDeltaEvent {
            call_id,
            offset,
            delta: diff.to_owned(),
        }));
        events
    }

    fn finish(&mut self) -> Result<Vec<ArgumentPreview>, FunctionCallError> {
        Ok(self
            .edits
            .finish()
            .map(ArgumentPreview::Patch)
            .into_iter()
            .collect())
    }
}

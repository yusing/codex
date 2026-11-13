use std::sync::Arc;

use super::SessionTask;
use super::SessionTaskResult;
use super::emit_compact_metric;
use crate::session::TurnInput;
use crate::session::session::Session;
use crate::session::turn_context::TurnContext;
use crate::state::TaskKind;
use codex_features::Feature;
use codex_model_provider::RemoteCompactionSupport;
use codex_protocol::error::CodexErrorDetails;
use codex_protocol::protocol::CodexErrorInfo;
use codex_protocol::user_input::UserInput;
use tokio_util::sync::CancellationToken;

pub(crate) struct CompactTask {
    admission: std::sync::Mutex<
        Option<(
            crate::tools::code_mode::ResetAdmission,
            Option<crate::state::ManualResetLease>,
        )>,
    >,
}

impl CompactTask {
    pub(crate) fn new(
        admission: crate::tools::code_mode::ResetAdmission,
        manual: Option<crate::state::ManualResetLease>,
    ) -> Self {
        Self {
            admission: std::sync::Mutex::new(Some((admission, manual))),
        }
    }
}

impl SessionTask for CompactTask {
    fn kind(&self) -> TaskKind {
        TaskKind::Compact
    }

    fn span_name(&self) -> &'static str {
        "session_task.compact"
    }

    async fn run(
        self: Arc<Self>,
        session: Arc<Session>,
        ctx: Arc<TurnContext>,
        _input: Vec<TurnInput>,
        cancellation_token: CancellationToken,
    ) -> SessionTaskResult {
        // Release admission when reset execution ends, before task finalization
        // can start an independently pending manual reset.
        let (_admission, manual) = self
            .admission
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
            .expect("compact task runs once");
        let _profile_guard = ctx.turn_timing_state.begin_compaction();
        let _compaction_span = tracing::trace_span!(
            "codex.compaction",
            codex.turn.phase = "compaction",
            conversation.id = %session.thread_id,
            turn.id = %ctx.sub_id,
        );
        // Preparation errors must reach the task runner, which reports them to the client.
        session.emit_turn_started(&ctx, TaskKind::Compact).await;
        if ctx.config.features.enabled(Feature::TokenBudget) {
            crate::compact_token_budget::run_manual_compact_task(
                session,
                ctx,
                manual.expect("token-budget reset owns its manual requests"),
                &cancellation_token,
            )
            .await?;
            return Ok(None);
        }
        let step_context = session
            .capture_step_context(Arc::clone(&ctx), &cancellation_token)
            .await?;
        let world_state = Arc::new(
            session
                .build_world_state_for_step(&step_context, /*new_window*/ true)
                .await?,
        );
        let result = match ctx.provider.capabilities().remote_compaction {
            RemoteCompactionSupport::V2 => {
                emit_compact_metric(
                    &session.services.session_telemetry,
                    "remote_v2",
                    /*manual*/ true,
                );
                crate::compact_remote_v2::run_remote_compact_task(
                    session.clone(),
                    step_context,
                    world_state,
                )
                .await
            }
            RemoteCompactionSupport::Unsupported => {
                emit_compact_metric(
                    &session.services.session_telemetry,
                    "local",
                    /*manual*/ true,
                );
                let input = vec![UserInput::Text {
                    text: ctx
                        .config
                        .compact_prompt
                        .as_deref()
                        .unwrap_or(crate::compact::SUMMARIZATION_PROMPT)
                        .to_string(),
                    // Compaction prompt is synthesized; no UI element ranges to preserve.
                    text_elements: Vec::new(),
                }];
                crate::compact::run_compact_task(session.clone(), step_context, world_state, input)
                    .await
            }
        };
        if let Err(err) = result {
            if matches!(err.details(), CodexErrorDetails::TurnAborted) {
                return Err(err);
            }
            let error = err.to_codex_protocol_error();
            if matches!(error, CodexErrorInfo::UsageLimitExceeded) {
                // Compaction already emitted the error; notify extensions without emitting it twice.
                session
                    .emit_turn_error_lifecycle(ctx.as_ref(), error, err.details())
                    .await;
            }
        }
        Ok(None)
    }
}

use std::sync::Arc;

use tokio_util::sync::CancellationToken;

use crate::hook_runtime::PostCompactHookOutcome;
use crate::hook_runtime::PreCompactHookOutcome;
use crate::hook_runtime::run_post_compact_hooks;
use crate::hook_runtime::run_pre_compact_hooks;
use crate::session::session::Session;
use crate::session::step_context::StepContext;
use crate::session::turn_context::TurnContext;
use codex_analytics::CompactionTrigger;
use codex_protocol::context_reset::ContextReset;
use codex_protocol::context_reset::ContextResetStatus;
use codex_protocol::error::CodexErr;
use codex_protocol::error::Result as CodexResult;
use codex_protocol::items::ContextCompactionItem;
use codex_protocol::items::TurnItem;
use codex_protocol::protocol::EventMsg;

#[path = "compact_token_budget_reload.rs"]
pub(crate) mod reload;

impl Session {
    pub(crate) async fn accept_self_reset(&self, turn: &TurnContext, call: &str) {
        self.accept_self_reset_operation(format!("new_context:{}:{call}", turn.sub_id))
            .await;
    }

    pub(crate) async fn accept_self_reset_operation(&self, operation: String) {
        use codex_protocol::feature_use::FeatureUseEvent;
        let preload = {
            let mut state = self.state.lock().await;
            let preload = state
                .history
                .retained_context()
                .reload_selection()
                .is_some_and(reload::has_preload);
            state.accepted_self_resets.push(operation.clone());
            preload
        };
        self.services.local_agent_runtime.record_feature_use(
            self.thread_id,
            &operation,
            FeatureUseEvent::SelfResetRequested { preload },
        );
    }

    pub(crate) async fn finish_aborted_reset(&self, turn: &TurnContext) {
        let receipt = self
            .state
            .lock()
            .await
            .manual_resets
            .take_aborted(&turn.sub_id);
        if let Some((receipt, self_requests)) = receipt {
            let completed = receipt.status == ContextResetStatus::Completed;
            self.send_event(turn, EventMsg::ContextReset(receipt)).await;
            for operation in self_requests {
                use codex_protocol::feature_use::FeatureUseEvent;
                self.services.local_agent_runtime.record_feature_use(
                    self.thread_id,
                    &operation,
                    if completed {
                        FeatureUseEvent::SelfResetCompleted
                    } else {
                        FeatureUseEvent::SelfResetFailed
                    },
                );
            }
        }
    }

    pub(crate) async fn cancel_manual_reset(&self, request_id: String) {
        let generation = self.state.lock().await.manual_resets.cancel(&request_id);
        if let Some(generation) = generation {
            self.manual_reset_canceled(generation, Some(request_id))
                .await;
        }
    }

    pub(crate) async fn manual_reset_canceled(&self, generation: u64, request_id: Option<String>) {
        if generation != 0 {
            self.send_event_raw(codex_protocol::protocol::Event {
                id: String::new(),
                msg: EventMsg::ContextReset(ContextReset {
                    thread_id: self.thread_id.to_string(),
                    turn_id: String::new(),
                    request_id,
                    generation,
                    reset_id: None,
                    status: ContextResetStatus::Canceled,
                    inline: true,
                    diagnostic: None,
                }),
            })
            .await;
        }
    }
}

/// Runs token-budget manual compaction as a normal compaction lifecycle.
///
/// Token-budget compaction skips model/server summarization and installs a fresh context window
/// instead. It is still modeled as compaction so compact hooks and `ContextCompaction` turn items
/// observe the same lifecycle as local or remote compaction.
pub(crate) async fn run_manual_compact_task(
    sess: Arc<Session>,
    turn_context: Arc<TurnContext>,
    manual: crate::state::ManualResetLease,
    cancellation_token: &CancellationToken,
) -> CodexResult<()> {
    run_compact_task_inner(
        &sess,
        turn_context,
        None,
        Some(manual),
        CompactionTrigger::Manual,
        cancellation_token,
        false,
        false,
    )
    .await
}

/// Runs token-budget inline reset through the shared terminal lifecycle.
///
/// Token-budget compaction skips model/server summarization and installs a fresh context window
/// instead. It is still modeled as compaction so compact hooks and `ContextCompaction` turn items
/// observe the same lifecycle as local or remote compaction.
pub(crate) async fn run_inline_compact_task(
    sess: Arc<Session>,
    step_context: Arc<StepContext>,
    trigger: CompactionTrigger,
    required_without_manual: bool,
    cancellation_token: &CancellationToken,
) -> CodexResult<()> {
    run_compact_task_inner(
        &sess,
        Arc::clone(&step_context.turn),
        Some(step_context),
        None,
        trigger,
        cancellation_token,
        true,
        required_without_manual,
    )
    .await
}

async fn run_compact_task_inner(
    sess: &Arc<Session>,
    turn_context: Arc<TurnContext>,
    step_context: Option<Arc<StepContext>>,
    manual: Option<crate::state::ManualResetLease>,
    trigger: CompactionTrigger,
    cancellation_token: &CancellationToken,
    inline: bool,
    required_without_manual: bool,
) -> CodexResult<()> {
    let turn_context = &turn_context;
    // Requests accepted during acquisition belong to the next reset, not this one.
    let compaction_item = TurnItem::ContextCompaction(ContextCompactionItem::new());
    let (requested, requests, self_requests) = {
        let mut state = sess.state.lock().await;
        (
            state.take_new_context_window_request(),
            Arc::clone(&state.manual_resets),
            std::mem::take(&mut state.accepted_self_resets),
        )
    };
    let required = required_without_manual || requested;
    let parent_cancellation = cancellation_token;
    let reset_cancellation = parent_cancellation.child_token();
    let lease = manual.unwrap_or_else(|| {
        requests.begin(&sess.thread_id.to_string(), &turn_context.sub_id, required)
    });
    let generation = lease.generation;
    if generation == 0 && !required {
        return Ok(()); // A canceled deferred manual request never resets context.
    }
    let cancellation_token = &reset_cancellation;
    let mut receipt = ContextReset {
        thread_id: sess.thread_id.to_string(),
        turn_id: turn_context.sub_id.clone(),
        request_id: None,
        generation,
        reset_id: Some(compaction_item.id()),
        status: ContextResetStatus::Started,
        inline,
        diagnostic: None,
    };
    lease.record_self_requests(&receipt, &self_requests);
    if generation != 0 {
        lease.record(&receipt);
    }
    sess.send_event(turn_context, EventMsg::ContextReset(receipt.clone()))
        .await;
    let reset = async {
        let step_context = match step_context {
            Some(step) => step,
            None => {
                sess.capture_step_context(Arc::clone(turn_context), cancellation_token)
                    .await?
            }
        };
        let world_state = Arc::new(sess.build_world_state_for_step(&step_context, true).await?);
        let pre_compact_outcome = run_pre_compact_hooks(sess, turn_context, trigger).await;
        match pre_compact_outcome {
            PreCompactHookOutcome::Continue => {}
            PreCompactHookOutcome::Stopped => return Err(CodexErr::TurnAborted),
        }
        sess.emit_turn_item_started(turn_context, &compaction_item)
            .await;
        let reload = reload::acquire(sess, &step_context, cancellation_token).await?;
        if cancellation_token.is_cancelled() {
            return Err(CodexErr::TurnAborted);
        }
        sess.start_new_context_window(&step_context, world_state, reload)
            .await;
        sess.emit_turn_item_completed(turn_context, compaction_item)
            .await;
        let post_compact_outcome = run_post_compact_hooks(sess, turn_context, trigger).await;
        if let PostCompactHookOutcome::Stopped = post_compact_outcome {
            return Err(CodexErr::TurnAborted);
        }
        Ok(())
    };
    let (result, manual_canceled) = tokio::select! {
        biased;
        _ = cancellation_token.cancelled() => (Err(CodexErr::TurnAborted), false),
        _ = lease.cancellation.cancelled(), if !required => {
            reset_cancellation.cancel();
            (Err(CodexErr::TurnAborted), true)
        },
        result = reset => (result, false),
    };
    receipt.status = match &result {
        Ok(()) => ContextResetStatus::Completed,
        Err(err) => {
            receipt.diagnostic = Some(err.to_string());
            if cancellation_token.is_cancelled() {
                ContextResetStatus::Canceled
            } else {
                ContextResetStatus::Failed
            }
        }
    };
    // Record the terminal outcome before any await, including self-only resets
    // whose manual generation is zero. Joined abort cleanup preserves it.
    lease.record(&receipt);
    sess.send_event(turn_context, EventMsg::ContextReset(receipt))
        .await;
    for operation in self_requests {
        use codex_protocol::feature_use::FeatureUseEvent;
        sess.services.local_agent_runtime.record_feature_use(
            sess.thread_id,
            &operation,
            if result.is_ok() {
                FeatureUseEvent::SelfResetCompleted
            } else {
                FeatureUseEvent::SelfResetFailed
            },
        );
    }
    lease.finish();
    if inline && manual_canceled && !parent_cancellation.is_cancelled() {
        Ok(()) // Cancel the auxiliary reset, not its original regular task.
    } else {
        result
    }
}

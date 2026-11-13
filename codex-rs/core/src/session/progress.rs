//! Nonterminal, tool-free auxiliary sampling at a genuine post-tool boundary.

use super::Session;
use super::StepContext;
use super::TokenUsageContext;
use super::reasoning_effort::RequestEffortUsage;
use crate::agent::control::work_lifecycle::work_error;
use crate::client_common::Prompt;
use crate::client_common::ResponseEvent;
use crate::context_manager::ContextManager;
use crate::responses_metadata::CodexResponsesRequestKind;
use codex_protocol::error::CodexErr;
use codex_protocol::error::CodexErrorDetails;
use codex_protocol::error::Result as CodexResult;
use codex_protocol::models::BaseInstructions;
use codex_protocol::models::ContentItem;
use codex_protocol::models::ResponseItem;
use codex_rollout_trace::InferenceTraceContext;
use codex_state::coordination_progress::SummaryProbe;
use codex_state::coordination_progress::SummaryResult;
use futures::StreamExt;
use std::sync::Arc;

struct ProgressBoundary {
    step: Arc<StepContext>,
}

/// A live boundary notification, not retained work or a second slot owner.
struct ProgressRequested;

impl Session {
    pub(crate) async fn service_progress_boundary(self: &Arc<Self>, step: Arc<StepContext>) {
        if !self.manages_coordination_work() {
            return;
        }
        // Retain settings cheaply, but do not scan SQL or clone/project history
        // at ordinary tool boundaries without an actual pending capture probe.
        self.services
            .thread_extension_data
            .insert(ProgressBoundary {
                step: Arc::clone(&step),
            });
        if self
            .services
            .thread_extension_data
            .get::<ProgressRequested>()
            .is_none()
        {
            return;
        }
        self.services
            .thread_extension_data
            .remove::<ProgressRequested>();
        self.start_progress_probes(step).await;
    }

    pub(crate) async fn service_checkpointed_progress(self: &Arc<Self>) {
        self.services
            .thread_extension_data
            .insert(ProgressRequested);
        let active = self.active_turn.lock().await;
        if active.is_some() {
            return;
        }
        self.services
            .thread_extension_data
            .remove::<ProgressRequested>();
        let Some(boundary) = self
            .services
            .thread_extension_data
            .get::<ProgressBoundary>()
        else {
            // A loaded target without an admitted safe sampler resolves as
            // unavailable rather than leaving the batch pending indefinitely.
            let runtime = &self.services.local_agent_runtime;
            if let Ok(ledger) = runtime.work_ledger()
                && let Ok(Some(work)) = ledger.observe_current(self.thread_id).await
                && let Ok(probes) = runtime.take_progress_probes(work.stamp).await
            {
                for probe in probes {
                    let _ = runtime
                        .resolve_progress_probe(
                            &probe,
                            SummaryResult::Unavailable {
                                reason: "loaded target has no safe sampling context".into(),
                            },
                        )
                        .await;
                }
            }
            return;
        };
        let step = Arc::clone(&boundary.step);
        // Take the slots before releasing admission: another primary turn may
        // start afterward, but inference uses this already-safe frozen snapshot.
        self.start_progress_probes(step).await;
        drop(active);
    }

    async fn start_progress_probes(self: &Arc<Self>, step: Arc<StepContext>) {
        let runtime = &self.services.local_agent_runtime;
        let work = match runtime.work_ledger() {
            Ok(ledger) => match ledger
                .work_for_turn(self.thread_id, &step.turn.sub_id)
                .await
            {
                Ok(Some(work)) => work,
                _ => return,
            },
            Err(_) => return,
        };
        let probes = match runtime.take_progress_probes(work).await {
            Ok(probes) => probes,
            Err(error) => {
                tracing::warn!(%error, "progress boundary unavailable");
                return;
            }
        };
        if probes.is_empty() {
            return;
        }
        let history = self.clone_history().await;
        for probe in probes {
            let Some(wait_cancel) =
                runtime.progress_cancellation(probe.caller.actor_id, probe.wait_id)
            else {
                continue;
            };
            let guard = match runtime.admit_start() {
                Ok(guard) => guard.into_teardown_guard("progress_summary", Some(self.thread_id)),
                Err(_) => continue,
            };
            let session = Arc::clone(self);
            let step = Arc::clone(&step);
            let history = history.clone();
            let runtime = runtime.clone();
            tokio::spawn(async move {
                let result = tokio::select! {
                    biased;
                    _ = runtime.shutdown.cancelled() => Err(CodexErr::TurnAborted),
                    _ = wait_cancel.cancelled() => Err(CodexErr::TurnAborted),
                    result = sample_progress(&session, &step, history, &probe) => result,
                };
                let result = match result {
                    Ok(text) => SummaryResult::Available { text },
                    Err(error) => SummaryResult::Unavailable {
                        reason: error.to_string(),
                    },
                };
                if let Err(error) = runtime.resolve_progress_probe(&probe, result).await {
                    tracing::warn!(%error, "progress slot no longer accepts its summary");
                }
                // Box the scheduler edge: its future can itself reach another
                // post-tool boundary, but this task never owns the primary turn.
                Box::pin(runtime.notify_coordination(probe.caller.actor_id)).await;
                guard.complete();
            });
        }
    }
}

fn progress_prompt(
    mut input: Vec<ResponseItem>,
    mut base_instructions: BaseInstructions,
    probe: &SummaryProbe,
) -> CodexResult<Prompt> {
    if input.iter().any(|item| {
        crate::context::BaseInstructionsFragment::text_from_item(item).is_some()
            || matches!(item, ResponseItem::AdditionalTools { .. })
    }) {
        // Incremental native history already carries the instruction baseline.
        base_instructions.text.clear();
    }
    // Incremental tool declarations are real capabilities even with Prompt.tools
    // empty. Remove them from this private request, not from primary history.
    input.retain(|item| !matches!(item, ResponseItem::AdditionalTools { .. }));
    for item in &mut input {
        if let ResponseItem::ToolSearchOutput { tools, .. } = item {
            tools.clear();
        }
    }
    let selection = serde_json::to_string(&probe.target.intervals).map_err(work_error)?;
    input.push(ResponseItem::Message {
        id: None,
        role: "developer".into(),
        content: vec![ContentItem::InputText {
            text: format!(
                "{}\nFrozen capture selection:\n{}",
                include_str!("../../templates/agents/progress_summary.md"),
                selection
            ),
        }],
        status: None,
        encrypted_content: None,
        phase: None,
        internal_chat_message_metadata_passthrough: None,
    });
    Ok(Prompt {
        input,
        base_instructions,
        ..Default::default()
    })
}

async fn sample_progress(
    session: &Session,
    step: &StepContext,
    history: ContextManager,
    probe: &SummaryProbe,
) -> CodexResult<String> {
    let input = history
        .for_prompt(
            &step.settings.model_info.input_modalities,
            session.services.image_store.as_ref(),
        )
        .await;
    let mut prompt = progress_prompt(input, session.get_prompt_base_instructions().await, probe)?;
    prompt.prompt_cache_ttl = step.turn.prompt_cache_ttl;
    prompt.cyber_access_program = step.turn.cyber_access_program;
    let metadata = session
        .responses_metadata(step, CodexResponsesRequestKind::Progress)
        .await;
    // A separate transport session preserves the primary request's incremental
    // state while sharing authentication, provider settings and usage accounting.
    let mut client = session.services.model_client.new_session();
    let mut stream = client
        .stream(
            &prompt,
            &step.settings.model_info,
            &step.session_telemetry,
            session
                .reasoning_effort_for_request(&step.settings, RequestEffortUsage::Auxiliary)
                .await,
            step.settings.reasoning_summary,
            step.settings.service_tier.clone(),
            &metadata,
            &InferenceTraceContext::disabled(),
        )
        .await?;
    let mut text = String::new();
    while let Some(event) = stream.next().await {
        match event {
            Ok(ResponseEvent::OutputItemDone(ResponseItem::Message { role, content, .. }))
                if role == "assistant" =>
            {
                for item in content {
                    if let ContentItem::OutputText { text: part, .. } = item {
                        if !text.is_empty() {
                            text.push('\n');
                        }
                        text.push_str(&part);
                    }
                }
            }
            Ok(ResponseEvent::RateLimits(snapshot)) => {
                session.update_rate_limits(&step.turn, snapshot).await
            }
            Ok(ResponseEvent::Completed {
                response_id,
                token_usage,
                ..
            }) => {
                session
                    .record_response_token_usage(&step.turn, &response_id, token_usage.as_ref())
                    .await;
                session
                    .record_token_usage_info(
                        &step.turn,
                        &step.settings,
                        token_usage.as_ref(),
                        TokenUsageContext::SpendingOnly,
                    )
                    .await?;
                session.send_token_count_event(&step.turn).await;
                if text.trim().is_empty() {
                    return Err(work_error("auxiliary progress request returned no summary"));
                }
                return Ok(text);
            }
            Err(error) => {
                if let CodexErrorDetails::ResponseIncomplete(incomplete) = error.details() {
                    session
                        .record_incomplete_response_usage(&step.turn, &step.settings, incomplete)
                        .await;
                }
                return Err(error);
            }
            Ok(_) => {}
        }
    }
    Err(CodexErr::Stream(
        "progress stream closed before response.completed".into(),
    ))
}

impl Session {
    pub(crate) async fn project_sampling_final_captures(
        &self,
        history: &ContextManager,
        input: &mut [ResponseItem],
    ) -> CodexResult<()> {
        if !self.manages_coordination_work() {
            return Ok(());
        }
        let envelopes: std::collections::HashMap<_, _> = history
            .annotated_items()
            .iter()
            .filter(|item| {
                item.metadata
                    .as_ref()
                    .is_some_and(|metadata| !metadata.coordination_delivery_ids.is_empty())
            })
            .filter_map(|item| item.item.id().map(|id| (id, item)))
            .collect();
        if envelopes.is_empty()
            || !self
                .services
                .local_agent_runtime
                .recipient_has_capture_suppression(self.thread_id)
                .await?
        {
            return Ok(());
        }
        for item in input {
            let Some(envelope) = item.id().and_then(|id| envelopes.get(id)) else {
                continue;
            };
            if envelope.item != *item {
                continue;
            }
            let Some(metadata) = &envelope.metadata else {
                continue;
            };
            if metadata.coordination_delivery_ids.is_empty()
                && metadata.coordination_wait_claim_id.is_none()
            {
                continue;
            }
            if let Some(mut projected) = self
                .services
                .local_agent_runtime
                .project_sampling_final_item(
                    self.thread_id,
                    metadata.coordination_wait_claim_id,
                    &metadata.coordination_delivery_ids,
                )
                .await?
            {
                projected.set_id(item.id().cloned());
                *item = projected;
            }
        }
        Ok(())
    }
}

/// A stream handle or local input receipt does not prove provider acceptance.
pub(super) fn provider_accepted_coordination_input(event: &ResponseEvent) -> bool {
    match event {
        ResponseEvent::Created {
            response_id: Some(id),
        } => !id.is_empty(),
        ResponseEvent::OutputItemDone(_) | ResponseEvent::Completed { .. } => true,
        _ => false,
    }
}

#[cfg(test)]
#[path = "progress_tests.rs"]
mod tests;

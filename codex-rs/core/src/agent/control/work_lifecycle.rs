//! Native accepted-input and terminal lifecycle over the durable work owner.
//! No history restore, second mailbox, timer or independently started turn lives here.

use super::LocalAgentRuntime;
use super::work::DeliveryKind;
use super::work::PublishDelivery;
use super::work::WorkDisposition;
use super::work::WorkStamp;
use crate::agent::api::AgentTurnOutcome;
use crate::session::TurnInput;
use codex_protocol::AgentPath;
use codex_protocol::ThreadId;
use codex_protocol::agent_coordination::CoordinationDisposition;
use codex_protocol::agent_coordination::CoordinationProjection;
use codex_protocol::agent_coordination::CoordinationTarget;
use codex_protocol::agent_coordination::CoordinationTerminalDelivery;
use codex_protocol::error::CodexErr;
use codex_protocol::error::Result as CodexResult;
use codex_protocol::feature_use::FeatureUseEvent;
use codex_protocol::feature_use::FeatureUseFact;
use codex_protocol::protocol::AgentStatus;
use codex_protocol::protocol::InterAgentCommunication;
use codex_protocol::user_input::UserInput;
use codex_state::coordination_progress::FrozenFinalCapture;
use futures::future::BoxFuture;
use serde::Deserialize;
use serde::Serialize;
use std::collections::HashSet;
use std::collections::VecDeque;
use std::sync::Arc;
use uuid::Uuid;

/// The sole encoding inside PublishDelivery.payload. Public protocol input cannot
/// supply authenticated harness delivery metadata.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub(crate) enum CoordinationPayload {
    UserInput {
        content: Vec<UserInput>,
    },
    Message {
        communication: InterAgentCommunication,
    },
    ForeignMessage {
        origin: codex_protocol::agent_foreign::AgentForeignOrigin,
        original_message: String,
        frozen_sources: Option<String>,
    },
    CompletionGuard {
        text: String,
    },
    Terminal {
        status: AgentStatus,
        answers: Vec<String>,
        capture: Option<FrozenFinalCapture>,
    },
}

#[derive(Default, Debug, PartialEq, Eq)]
pub(crate) struct CoordinationSamplingEvidence {
    pub(crate) delivery_ids: HashSet<Uuid>,
    pub(crate) wait_claim_ids: HashSet<Uuid>,
    pub(crate) ordinary_delivery_ids: HashSet<Uuid>,
}

pub(crate) fn sampling_projection(
    history: &[codex_history::ResponseItemEnvelope],
    input: &[codex_protocol::models::ResponseItem],
) -> (CoordinationSamplingEvidence, HashSet<Uuid>) {
    let projected: std::collections::HashMap<_, _> = input
        .iter()
        .filter_map(|item| item.id().map(|id| (id, item)))
        .collect();
    let mut evidence = CoordinationSamplingEvidence::default();
    let mut ordinary = HashSet::new();
    for envelope in history {
        let Some(metadata) = &envelope.metadata else {
            continue;
        };
        if metadata.coordination_delivery_ids.is_empty()
            && metadata.coordination_wait_claim_id.is_none()
        {
            continue;
        }
        let Some(id) = envelope.item.id() else {
            continue;
        };
        // Compare only the identity-matched candidate, then require the entire
        // frozen item, not a truncated projection carrying the same native ID.
        if projected.get(id).is_none_or(|item| **item != envelope.item) {
            continue;
        }
        if let Some(claim) = metadata.coordination_wait_claim_id {
            evidence.wait_claim_ids.insert(claim);
            evidence
                .delivery_ids
                .extend(metadata.coordination_delivery_ids.iter().copied());
        } else {
            ordinary.extend(metadata.coordination_delivery_ids.iter().copied());
        }
    }
    (evidence, ordinary)
}

pub struct FinalCaptureRequest {
    pub work: WorkStamp,
    pub recipient: ThreadId,
    pub sources: Vec<String>,
    pub recipient_cwd: Option<std::path::PathBuf>,
}

/// Installed by the actual capture owner. None means a successfully prepared
/// final with no attachments; absence of the callback is not readiness.
pub type FinalCapturePreparer = Arc<
    dyn Fn(
            FinalCaptureRequest,
            tokio_util::sync::CancellationToken,
        ) -> BoxFuture<'static, CodexResult<Option<FrozenFinalCapture>>>
        + Send
        + Sync,
>;

pub(crate) fn work_error(error: impl std::fmt::Display) -> CodexErr {
    CodexErr::Fatal(format!("native coordination: {error}"))
}

pub(crate) fn input_delivery_ids(input: &TurnInput) -> &[Uuid] {
    match input {
        TurnInput::UserInput { metadata, .. } => &metadata.coordination_delivery_ids,
        TurnInput::InterAgentCommunication(message) => message.coordination_delivery_ids(),
        TurnInput::FunctionCallOutput(item) | TurnInput::ResponseItem(item) => {
            item.metadata.as_ref().map_or(&[], |metadata| {
                metadata.coordination_delivery_ids.as_slice()
            })
        }
    }
}

pub(crate) fn terminal_delivery_projection(
    record: &super::work::DeliveryRecord,
    author: AgentPath,
    recipient: AgentPath,
    status: &AgentStatus,
    answers: &[String],
    capture: Option<&FrozenFinalCapture>,
) -> CodexResult<CoordinationTerminalDelivery> {
    let work = record
        .publication
        .work
        .ok_or_else(|| work_error("terminal delivery has no source work"))?;
    let disposition = match status {
        AgentStatus::Completed(_) => CoordinationDisposition::Completed,
        AgentStatus::Errored(_) => CoordinationDisposition::Failed,
        AgentStatus::Interrupted => CoordinationDisposition::Interrupted,
        _ => return Err(work_error("terminal delivery has nonterminal status")),
    };
    let status = if matches!(status, AgentStatus::Completed(_)) {
        AgentStatus::Completed((!answers.is_empty()).then(|| answers.join("\n\n")))
    } else {
        status.clone()
    };
    let mut body = crate::session_prefix::inter_agent_completion_payload(&status)
        .unwrap_or_else(|| "Agent work settled.".to_owned());
    if let Some(capture) = capture {
        body.push_str("\n\n");
        body.push_str(&capture.full_text);
    }
    Ok(CoordinationTerminalDelivery {
        delivery_id: record.publication.id,
        source: CoordinationTarget {
            thread_id: work.actor_id,
            task_name: author,
            work_id: work.work_id,
        },
        recipient,
        disposition,
        body,
        capture_resource_ref: capture.map(|capture| capture.resource_ref.clone()),
    })
}

pub(crate) fn delivery_input(
    record: &super::work::DeliveryRecord,
    author: Option<AgentPath>,
    recipient: AgentPath,
) -> CodexResult<TurnInput> {
    let payload: CoordinationPayload =
        serde_json::from_str(&record.publication.payload).map_err(work_error)?;
    delivery_input_from_payload(record, payload, author, recipient)
}

pub(crate) fn delivery_input_from_payload(
    record: &super::work::DeliveryRecord,
    payload: CoordinationPayload,
    author: Option<AgentPath>,
    recipient: AgentPath,
) -> CodexResult<TurnInput> {
    let ids = vec![record.publication.id];
    match payload {
        CoordinationPayload::UserInput { content } => Ok(TurnInput::UserInput {
            content,
            client_id: None,
            metadata: crate::session::UserInputMetadata {
                coordination_delivery_ids: ids,
                ..Default::default()
            },
        }),
        CoordinationPayload::Message { communication } => Ok(TurnInput::InterAgentCommunication(
            crate::PendingAgentMessage::from(communication).with_coordination_delivery_ids(ids),
        )),
        CoordinationPayload::ForeignMessage {
            origin,
            original_message,
            frozen_sources,
        } => {
            let mut content = original_message;
            if let Some(body) = frozen_sources {
                content.push_str("\n\nReferenced sources captured when sent:\n");
                content.push_str(&body);
            }
            let communication = crate::agent::types::AgentMessage::Plaintext(
                super::foreign::attributed_foreign_text(&origin, &content),
            )
            .into_communication(
                origin.agent_path.clone(),
                recipient,
                crate::agent::types::MessageDeliveryMode::QueueOnly,
            );
            Ok(TurnInput::InterAgentCommunication(
                crate::PendingAgentMessage::from(communication).with_coordination_delivery_ids(ids),
            ))
        }
        CoordinationPayload::CompletionGuard { text } => {
            let limit = codex_utils_output_truncation::approx_token_count(&text);
            Ok(TurnInput::ResponseItem(
                codex_history::ResponseItemEnvelope {
                    item: codex_protocol::models::ResponseItem::Message {
                        id: None,
                        role: "developer".into(),
                        content: vec![codex_protocol::models::ContentItem::InputText { text }],
                        status: None,
                        encrypted_content: None,
                        phase: None,
                        internal_chat_message_metadata_passthrough: None,
                    },
                    metadata: Some(codex_history::CodexHarnessMetadata {
                        coordination_delivery_ids: ids,
                        history_truncation_token_limit: Some(limit),
                        ..Default::default()
                    }),
                },
            ))
        }
        CoordinationPayload::Terminal {
            mut status,
            answers,
            capture,
        } => {
            let author = author.ok_or_else(|| work_error("terminal author is not registered"))?;
            let projection = CoordinationProjection {
                deliveries: vec![terminal_delivery_projection(
                    record,
                    author.clone(),
                    recipient.clone(),
                    &status,
                    &answers,
                    capture.as_ref(),
                )?],
                wait: None,
                interrupt: None,
            };
            if matches!(status, AgentStatus::Completed(_)) {
                status =
                    AgentStatus::Completed((!answers.is_empty()).then(|| answers.join("\n\n")));
            }
            let mut content = crate::session_prefix::format_inter_agent_completion_message(
                recipient.clone(),
                author.clone(),
                &status,
            )
            .unwrap_or_else(|| "Agent work settled.".to_owned());
            if let Some(capture) = capture {
                content.push_str("\n\n");
                content.push_str(&capture.full_text);
            }
            Ok(TurnInput::InterAgentCommunication(
                crate::PendingAgentMessage::from(InterAgentCommunication::new(
                    author,
                    recipient,
                    Vec::new(),
                    content,
                    false,
                ))
                .with_coordination_delivery_ids(ids)
                .with_coordination(projection),
            ))
        }
    }
}

impl LocalAgentRuntime {
    pub(crate) fn coordination_sender(&self, path: &AgentPath) -> CodexResult<ThreadId> {
        self.registry
            .agent_id_for_path(path)
            .ok_or_else(|| work_error("message sender is not registered"))
    }

    pub(crate) fn record_feature_use(
        &self,
        actor: ThreadId,
        operation: &str,
        event: FeatureUseEvent,
    ) {
        if let Some(host) = self.host_callbacks.get() {
            (host.record_feature_use)(FeatureUseFact {
                thread_id: actor.to_string(),
                operation: operation.to_owned(),
                event,
            });
        }
    }

    pub(crate) async fn prepare_input(
        &self,
        actor: ThreadId,
        input: &mut TurnInput,
        starts_work: bool,
    ) -> CodexResult<()> {
        let ledger = self.work_ledger()?;
        if !input_delivery_ids(input).is_empty() {
            return Ok(());
        }
        let root = self
            .registry
            .agent_id_for_path(&AgentPath::root())
            .ok_or_else(|| work_error("root is not registered"))?;
        let (id, sender, payload) = match input {
            TurnInput::UserInput {
                content, metadata, ..
            } => {
                let id = *metadata.response_item_uuid.get_or_insert_with(Uuid::now_v7);
                (
                    id,
                    None,
                    CoordinationPayload::UserInput {
                        content: content.clone(),
                    },
                )
            }
            TurnInput::InterAgentCommunication(message) if message.trigger_turn => {
                let sender = self
                    .registry
                    .agent_id_for_path(&message.author)
                    .ok_or_else(|| work_error("agent input sender is not authenticated"))?;
                (
                    Uuid::now_v7(),
                    Some(sender),
                    CoordinationPayload::Message {
                        communication: (**message).clone(),
                    },
                )
            }
            // Wake/tool continuations and auxiliary input never admit an assignment.
            _ => return Ok(()),
        };
        let mut current = ledger.observe_current(actor).await.map_err(work_error)?;
        if let Some(work) = current.as_ref()
            && work.runtime_id != self.tree_id()
            && !work.disposition.is_terminal()
        {
            // Explicit input reconciles paused identity; uncertain execution still
            // rejects in the transaction until its actual receipt is reconciled.
            current = Some(
                ledger
                    .reconcile_work(work.stamp, work.revision)
                    .await
                    .map_err(work_error)?,
            );
        }
        let stamp = match current.as_ref() {
            Some(work) if !starts_work && !work.disposition.is_terminal() => work.stamp,
            _ => WorkStamp {
                root_id: root,
                actor_id: actor,
                work_id: id,
            },
        };
        let publication = PublishDelivery {
            id,
            root_id: root,
            sender_id: sender,
            recipient_id: actor,
            work: Some(stamp),
            kind: DeliveryKind::Message,
            payload: serde_json::to_string(&payload).map_err(work_error)?,
        };
        let previous = current.map(|work| work.stamp);
        let old_deadline = self.live_wait_deadline(actor);
        let old_wait = ledger.current_wait(actor).await.map_err(work_error)?;
        ledger
            .admit_input(&publication, starts_work, previous)
            .await
            .map_err(work_error)?;
        if let Some(id) = old_deadline {
            self.retire_wait_deadline(actor, id);
        }
        if let Some(wait) = old_wait {
            self.emit_cancelled_wait(&wait).await;
        }
        if let Some(previous) = previous.filter(|previous| *previous != stamp) {
            if let Err(error) = self.finish_ready_completions(previous).await {
                tracing::warn!(
                    "accepted input retained; ancestor completion reevaluation failed: {error}"
                );
            }
        }
        match input {
            TurnInput::UserInput { metadata, .. } => metadata.coordination_delivery_ids.push(id),
            TurnInput::InterAgentCommunication(message) => {
                *message = message.clone().with_coordination_delivery_ids(vec![id]);
            }
            _ => unreachable!("only admitted human/agent input reaches metadata attachment"),
        }
        Ok(())
    }

    pub(crate) async fn confirm_input(
        &self,
        actor: ThreadId,
        input: &TurnInput,
        receipt: &str,
    ) -> CodexResult<()> {
        let ledger = self.work_ledger()?;
        for id in input_delivery_ids(input) {
            let delivery = ledger
                .observe_delivery(actor, *id)
                .await
                .map_err(work_error)?
                .ok_or_else(|| work_error("admitted input delivery is absent"))?;
            let dispatch = delivery
                .dispatch_id
                .ok_or_else(|| work_error("input has no dispatch fence"))?;
            ledger
                .confirm_delivery(actor, &[*id], dispatch, receipt)
                .await
                .map_err(work_error)?;
            if let Some(stamp) = delivery.publication.work {
                ledger.bind_turn(stamp, receipt).await.map_err(work_error)?;
                let work = ledger
                    .observe_work(stamp)
                    .await
                    .map_err(work_error)?
                    .ok_or_else(|| work_error("input work is absent"))?;
                if matches!(
                    work.disposition,
                    WorkDisposition::PendingStart
                        | WorkDisposition::Paused
                        | WorkDisposition::Waiting
                ) {
                    ledger
                        .transition_work(stamp, work.revision, WorkDisposition::Running)
                        .await
                        .map_err(work_error)?;
                }
            }
        }
        Ok(())
    }

    /// Publication wakes the existing scheduler only after durable facts exist.
    /// It never restores an unloaded recipient just to inspect or notify it.
    pub(crate) async fn notify_coordination(&self, actor: ThreadId) {
        self.registry.invalidate_observation();
        if self.work.is_some()
            && let Err(error) = self.prepare_ready_progress(actor).await
        {
            tracing::warn!(%error, "native progress output unavailable");
        }
        let Some(manager) = self.manager.upgrade() else {
            return;
        };
        if let Ok(thread) = manager.get_thread(actor).await {
            thread.session.input_queue.notify_mailbox();
            thread.session.maybe_start_turn_for_pending_work().await;
        }
    }

    pub(crate) async fn publish_message(
        &self,
        actor: ThreadId,
        sender: ThreadId,
        communication: InterAgentCommunication,
        sources: bool,
    ) -> CodexResult<String> {
        let root = self
            .registry
            .agent_id_for_path(&AgentPath::root())
            .ok_or_else(|| work_error("root is not registered"))?;
        let id = Uuid::now_v7();
        self.work_ledger()?
            .publish_delivery(&PublishDelivery {
                id,
                root_id: root,
                sender_id: Some(sender),
                recipient_id: actor,
                work: None,
                kind: DeliveryKind::Message,
                payload: serde_json::to_string(&CoordinationPayload::Message { communication })
                    .map_err(work_error)?,
            })
            .await
            .map_err(work_error)?;
        self.record_feature_use(
            sender,
            &id.to_string(),
            FeatureUseEvent::MessageAccepted {
                sources,
                btw: false,
            },
        );
        self.notify_coordination(actor).await;
        Ok(id.to_string())
    }

    pub(crate) async fn settle_terminal(&self, outcome: &AgentTurnOutcome) -> CodexResult<()> {
        let ledger = self.work_ledger()?;
        let Some(stamp) = ledger
            .work_for_turn(outcome.thread_id, &outcome.turn_id)
            .await
            .map_err(work_error)?
        else {
            return Ok(());
        };
        let Some(work) = ledger.observe_work(stamp).await.map_err(work_error)? else {
            return Ok(());
        };
        if work.disposition.is_terminal() {
            return Ok(());
        }
        let (disposition, detail) = match &outcome.status {
            AgentStatus::Completed(_) => (WorkDisposition::Completed, None),
            AgentStatus::Errored(message) => (WorkDisposition::Failed, Some(message.as_str())),
            AgentStatus::Interrupted | AgentStatus::Shutdown => {
                (WorkDisposition::Interrupted, None)
            }
            _ => return Ok(()),
        };
        let recipient = ledger
            .ancestors(outcome.thread_id)
            .first()
            .copied()
            .unwrap_or(outcome.thread_id);
        let Some(mut candidate) = ledger
            .stage_completion(work.stamp, &outcome.turn_id, recipient, disposition, detail)
            .await
            .map_err(work_error)?
        else {
            return Ok(());
        };
        if disposition == WorkDisposition::Completed && !candidate.capture_ready {
            if candidate.answers.is_empty()
                && let AgentStatus::Completed(Some(answer)) = &outcome.status
            {
                ledger
                    .record_answer(work.stamp, &format!("terminal:{}", outcome.turn_id), answer)
                    .await
                    .map_err(work_error)?;
                candidate.answers.push(answer.clone());
            }
            self.prepare_completion_capture(&super::recovery::CompletionCheckpoint {
                work: work.stamp,
                delivery_id: candidate.delivery_id,
            })
            .await?;
        }
        self.finish_ready_completions(work.stamp).await
    }

    /// Called under the native task-admission lock. This stages durable facts
    /// only; recipient observation and capture I/O run after releasing that lock.
    pub(crate) async fn prepare_successful_completion(
        &self,
        actor: ThreadId,
        turn_id: &str,
        final_answer: Option<&str>,
    ) -> CodexResult<Option<super::recovery::CompletionCheckpoint>> {
        let Some(ledger) = self.work.as_deref() else {
            return Ok(None);
        };
        let Some(stamp) = ledger
            .work_for_turn(actor, turn_id)
            .await
            .map_err(work_error)?
        else {
            return Ok(None);
        };
        let Some(work) = ledger.observe_work(stamp).await.map_err(work_error)? else {
            return Ok(None);
        };
        if work.disposition.is_terminal() {
            return Ok(None);
        }
        let recipient = ledger.ancestors(actor).first().copied().unwrap_or(actor);
        let Some(candidate) = ledger
            .stage_completion(stamp, turn_id, recipient, WorkDisposition::Completed, None)
            .await
            .map_err(work_error)?
        else {
            return Ok(None);
        };
        // Newly staged successful candidates are never capture-ready. Hold their
        // publication before capture can expose readiness to descendant callbacks.
        ledger
            .completion_settlement(stamp, candidate.delivery_id, false)
            .await
            .map_err(work_error)?;
        if candidate.answers.is_empty()
            && let Some(answer) = final_answer
        {
            ledger
                .record_answer(stamp, &format!("terminal:{turn_id}"), answer)
                .await
                .map_err(work_error)?;
        }
        Ok(Some(super::recovery::CompletionCheckpoint {
            work: stamp,
            delivery_id: candidate.delivery_id,
        }))
    }

    pub(crate) async fn prepare_completion_capture(
        &self,
        checkpoint: &super::recovery::CompletionCheckpoint,
    ) -> CodexResult<()> {
        let ledger = self.work_ledger()?;
        let Some(candidate) = ledger
            .pending_completion(checkpoint.work)
            .await
            .map_err(work_error)?
        else {
            return Ok(());
        };
        if candidate.delivery_id != checkpoint.delivery_id || candidate.capture_ready {
            return Ok(());
        }
        let sources = crate::compact_token_budget::reload::completion_selection(
            &candidate.answers.join("\n\n"),
        )
        .sources;
        if sources.is_empty() {
            // The native source grammar proves that nothing is selected. This is
            // distinct from an unavailable attachment owner or a failed capture.
            return ledger
                .complete_capture(checkpoint.work, checkpoint.delivery_id, None)
                .await
                .map_err(work_error);
        }
        let acquire = async {
            let prepare = self.host_callbacks.get().map(|host| &host.prepare_final_capture).ok_or_else(|| CodexErr::UnsupportedOperation(
                "native final capture preparation is not connected; completion remains pending".to_owned(),
            ))?;
            let recipient_cwd = self.observe_agent(candidate.recipient_id).await?.cwd;
            prepare(FinalCaptureRequest {
                work: checkpoint.work, recipient: candidate.recipient_id,
                sources, recipient_cwd,
            }, self.shutdown.child_token()).await
        }.await;
        match acquire {
            Ok(capture) => {
                let encoded = capture
                    .as_ref()
                    .map(serde_json::to_string)
                    .transpose()
                    .map_err(work_error)?;
                ledger
                    .complete_capture(checkpoint.work, checkpoint.delivery_id, encoded.as_deref())
                    .await
                    .map_err(work_error)
            }
            Err(error) => {
                ledger
                    .capture_failed(checkpoint.work, checkpoint.delivery_id, &error.to_string())
                    .await
                    .map_err(work_error)?;
                Err(error)
            }
        }
    }

    pub(crate) async fn current_work_checkpoint(
        &self,
        actor: ThreadId,
    ) -> CodexResult<Option<super::recovery::WorkCheckpoint>> {
        let Some(ledger) = self.work.as_deref() else {
            return Ok(None);
        };

        if let Some(work) = ledger.observe_current(actor).await.map_err(work_error)?
            && let Some(candidate) = ledger
                .pending_completion(work.stamp)
                .await
                .map_err(work_error)?
        {
            return Ok(Some(super::recovery::WorkCheckpoint::Completion(
                super::recovery::CompletionCheckpoint {
                    work: work.stamp,
                    delivery_id: candidate.delivery_id,
                },
            )));
        }
        Ok(None)
    }

    /// Called at the actual continuation-dispatch boundary, after settings and
    /// carrier admission. The retained candidate wins if its blockers settled.
    pub(crate) async fn begin_completion_continuation(
        &self,
        checkpoint: &super::recovery::CompletionCheckpoint,
        receipt: Option<&str>,
    ) -> CodexResult<Option<TurnInput>> {
        let ledger = self.work_ledger()?;
        let recipient = self
            .ensure_agent_known(checkpoint.work.actor_id)?
            .agent_path
            .ok_or_else(|| work_error("completion actor has no canonical task name"))?;
        let id = Uuid::now_v7();
        loop {
            let Some(blockers) = ledger
                .completion_blockers(checkpoint)
                .await
                .map_err(work_error)?
            else {
                return Ok(None);
            };
            if blockers.is_empty() {
                self.finish_ready_completions(checkpoint.work).await?;
                return Ok(None);
            }
            let mut names = blockers
                .iter()
                .map(|work| {
                    self.ensure_agent_known(work.actor_id)?
                        .agent_path
                        .ok_or_else(|| work_error("completion blocker has no canonical task name"))
                        .map(|path| path.to_string())
                })
                .collect::<CodexResult<Vec<_>>>()?;
            names.sort();
            names.dedup();
            let text = include_str!("../../../templates/agents/completion_guard.md")
                .replace("{paths}", &names.join(", "));
            let publication = PublishDelivery {
                id,
                root_id: checkpoint.work.root_id,
                sender_id: None,
                recipient_id: checkpoint.work.actor_id,
                work: Some(checkpoint.work),
                kind: DeliveryKind::Message,
                payload: serde_json::to_string(&CoordinationPayload::CompletionGuard { text })
                    .map_err(work_error)?,
            };
            if !ledger
                .begin_completion_continuation(checkpoint, &blockers, &publication, receipt)
                .await
                .map_err(work_error)?
            {
                continue;
            }
            let delivery = ledger
                .observe_delivery(checkpoint.work.actor_id, id)
                .await
                .map_err(work_error)?
                .ok_or_else(|| work_error("committed completion continuation is absent"))?;
            return Ok(Some(delivery_input(&delivery, None, recipient)?));
        }
    }

    /// Release native execution's publication fence after exact task settlement.
    /// Descendant changes then use the same existing completion owner, without
    /// starting another model turn merely to publish the prepared original final.
    pub(crate) async fn settle_completion_checkpoint(
        &self,
        checkpoint: &super::recovery::CompletionCheckpoint,
    ) -> CodexResult<super::recovery::CompletionSettlement> {
        use super::recovery::CompletionSettlement;
        let ledger = self.work_ledger()?;
        if ledger
            .work_stamp(checkpoint.work.actor_id)
            .await
            .map_err(work_error)?
            != Some(checkpoint.work)
        {
            return Ok(CompletionSettlement::Superseded);
        }
        if ledger
            .pending_completion(checkpoint.work)
            .await
            .map_err(work_error)?
            .is_some_and(|candidate| candidate.delivery_id != checkpoint.delivery_id)
        {
            return Ok(CompletionSettlement::Superseded);
        }
        let retained = ledger
            .completion_settlement(checkpoint.work, checkpoint.delivery_id, true)
            .await
            .map_err(work_error)?;
        if retained {
            self.finish_ready_completions(checkpoint.work).await?;
        }
        if let Some(work) = ledger
            .observe_work(checkpoint.work)
            .await
            .map_err(work_error)?
            && work.disposition == WorkDisposition::Completed
            && work.terminal_delivery_id == Some(checkpoint.delivery_id)
        {
            let final_answer = ledger
                .terminal_delivery(checkpoint.work)
                .await
                .map_err(work_error)?
                .map(|delivery| {
                    serde_json::from_str::<CoordinationPayload>(&delivery.publication.payload)
                })
                .transpose()
                .map_err(work_error)?
                .and_then(|payload| match payload {
                    CoordinationPayload::Terminal { answers, .. } => answers.last().cloned(),
                    _ => None,
                });
            return Ok(CompletionSettlement::Published(final_answer));
        }
        Ok(if retained {
            CompletionSettlement::Pending
        } else {
            CompletionSettlement::Superseded
        })
    }

    pub(crate) async fn interrupt_idle_work(&self, expected: WorkStamp) -> CodexResult<()> {
        let ledger = self.work_ledger()?;
        let Some(mut work) = ledger
            .observe_current(expected.actor_id)
            .await
            .map_err(work_error)?
        else {
            return Ok(());
        };
        if work.stamp != expected || work.disposition.is_terminal() {
            return Ok(());
        }
        if work.disposition == WorkDisposition::Uncertain {
            return Err(work_error(
                "uncertain execution must be reconciled before claiming interruption",
            ));
        }
        if work.runtime_id != self.tree_id() {
            work = ledger
                .reconcile_work(expected, work.revision)
                .await
                .map_err(work_error)?;
        }
        let recipient = ledger
            .ancestors(expected.actor_id)
            .first()
            .copied()
            .unwrap_or(expected.actor_id);
        ledger
            .stage_completion(
                work.stamp,
                &format!("interrupt:{}", Uuid::now_v7()),
                recipient,
                WorkDisposition::Interrupted,
                None,
            )
            .await
            .map_err(work_error)?;
        self.finish_ready_completions(expected).await
    }

    pub(crate) async fn finish_ready_completions(&self, changed: WorkStamp) -> CodexResult<()> {
        let ledger = self.work_ledger()?;
        let mut pending = VecDeque::from([changed]);
        while let Some(stamp) = pending.pop_front() {
            if let Some(candidate) = ledger.pending_completion(stamp).await.map_err(work_error)? {
                if !candidate.capture_ready {
                    continue;
                }
                let status = match candidate.disposition {
                    WorkDisposition::Completed => AgentStatus::Completed(None),
                    WorkDisposition::Failed => {
                        AgentStatus::Errored(candidate.detail.clone().unwrap_or_default())
                    }
                    WorkDisposition::Interrupted => AgentStatus::Interrupted,
                    _ => return Err(work_error("invalid completion disposition")),
                };
                let payload = CoordinationPayload::Terminal {
                    status,
                    answers: if candidate.disposition == WorkDisposition::Completed {
                        candidate.answers
                    } else {
                        Vec::new()
                    },
                    capture: candidate
                        .capture
                        .as_deref()
                        .map(serde_json::from_str)
                        .transpose()
                        .map_err(work_error)?,
                };
                let old_wait = ledger
                    .current_wait(stamp.actor_id)
                    .await
                    .map_err(work_error)?;
                if ledger
                    .finish_completion(
                        stamp,
                        candidate.delivery_id,
                        &serde_json::to_string(&payload).map_err(work_error)?,
                    )
                    .await
                    .map_err(work_error)?
                    .is_none()
                {
                    continue;
                }
                self.retire_work_deadline(stamp);
                if let Some(wait) = old_wait {
                    self.emit_cancelled_wait(&wait).await;
                }
            }
            pending.extend(
                ledger
                    .completion_dependents(stamp)
                    .await
                    .map_err(work_error)?,
            );
            for ancestor in ledger.ancestors(stamp.actor_id) {
                self.notify_coordination(ancestor).await;
            }
        }
        Ok(())
    }
}

#[cfg(test)]
#[path = "work_lifecycle_tests.rs"]
mod tests;

//! Materialize one frozen wait claim without transferring other recipients' mail.

use super::LocalAgentRuntime;
use super::work_lifecycle::CoordinationPayload;
use super::work_lifecycle::work_error;
use crate::session::TurnInput;
use codex_history::CodexHarnessMetadata;
use codex_history::ResponseItemEnvelope;
use codex_protocol::AgentPath;
use codex_protocol::ThreadId;
use codex_protocol::agent_coordination::*;
use codex_protocol::agent_metrics::AgentMetrics;
use codex_protocol::error::Result as CodexResult;
use codex_protocol::models::ContentItem;
use codex_protocol::models::FunctionCallOutputPayload;
use codex_protocol::models::ResponseItem;
use codex_protocol::protocol::AgentStatus;
use codex_state::coordination_wait::TurnWaitReason;
use codex_state::coordination_wait::WaitResetState;
use codex_state::coordination_wait::WaitTicket;
use codex_state::coordination_wait::WaitWake;
use serde::Serialize;
use std::collections::BTreeMap;
use std::collections::HashSet;
use uuid::Uuid;

#[derive(Serialize)]
struct WaitContinuation {
    wait_id: Uuid,
    reason: Option<TurnWaitReason>,
    wake: WaitWake,
    reset: WaitResetState,
    metrics: BTreeMap<String, AgentMetrics>,
    messages: Vec<WaitMessage>,
    outcomes: Vec<WaitOutcome>,
    #[serde(skip_serializing_if = "Option::is_none")]
    progress: Option<codex_state::coordination_progress::PreparedProgressOutput>,
}

#[derive(Serialize)]
struct WaitMessage {
    sender_thread_id: Option<ThreadId>,
    payload: CoordinationPayload,
}

#[derive(Serialize)]
struct WaitOutcome {
    thread_id: ThreadId,
    task_name: AgentPath,
    work_id: Uuid,
    status: WaitOutcomeStatus,
    answers: Vec<String>,
    capture: Option<String>,
}

#[derive(Serialize)]
#[serde(rename_all = "snake_case")]
enum WaitOutcomeStatus {
    Paused,
    #[serde(untagged)]
    Terminal(AgentStatus),
}

impl LocalAgentRuntime {
    pub(crate) async fn wait_tool_result(&self, ticket: &WaitTicket) -> CodexResult<String> {
        let envelope = self.materialize_wait_continuation(ticket, false).await?;
        match envelope.item {
            ResponseItem::FunctionCallOutput { output, .. } => {
                Ok(output.body.to_text().unwrap_or_default())
            }
            ResponseItem::Message { content, .. } => Ok(content
                .into_iter()
                .filter_map(|item| match item {
                    ContentItem::InputText { text } => Some(text),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("\n")),
            _ => Err(work_error("wait result has an unsupported shape")),
        }
    }

    pub(crate) async fn finish_awaited_wait(
        &self,
        ticket: &WaitTicket,
        context: Uuid,
    ) -> CodexResult<()> {
        let claim = ticket
            .claim
            .as_ref()
            .ok_or_else(|| work_error("wait has no frozen claim"))?;
        let ledger = self.work_ledger()?;
        ledger
            .state()
            .coordination_consume_awaited_wait(
                ticket.caller,
                self.tree_id(),
                ticket.id,
                claim.id,
                context,
            )
            .await
            .map_err(work_error)?;
        self.registry.invalidate_observation();
        self.retire_wait_deadline(ticket.caller.actor_id, ticket.id);
        self.record_wait_delivery_features(ticket, ledger).await;
        match self.materialize_wait_continuation(ticket, false).await {
            Ok(envelope) => {
                if let Some(projection) =
                    envelope.metadata.and_then(|metadata| metadata.coordination)
                {
                    self.publish_wait_projection(ticket, projection).await;
                }
            }
            Err(error) => {
                tracing::warn!(%error, wait_id = %ticket.id, "consumed wait projection unavailable")
            }
        }
        Ok(())
    }

    pub(crate) async fn wait_continuation_input(
        &self,
        ticket: &WaitTicket,
    ) -> CodexResult<ResponseItemEnvelope> {
        self.materialize_wait_continuation(ticket, false).await
    }

    pub(crate) async fn materialize_wait_continuation(
        &self,
        ticket: &WaitTicket,
        project_capture: bool,
    ) -> CodexResult<ResponseItemEnvelope> {
        let claim = ticket
            .claim
            .as_ref()
            .ok_or_else(|| work_error("wait has no frozen claim"))?;
        let ledger = self.work_ledger()?;
        let retained_metrics = match &claim.metrics {
            Some(metrics) => Some(metrics.clone()),
            None => ledger
                .observe_wait(ticket.caller, ticket.id)
                .await
                .map_err(work_error)?
                .and_then(|ticket| ticket.claim)
                .and_then(|claim| claim.metrics),
        };
        let metrics = if let Some(metrics) = retained_metrics {
            metrics
        } else {
            let mut metrics = BTreeMap::new();
            for selected in &ticket.selected {
                // Measurement is auxiliary. Its failure preserves the successful
                // result and freezes unavailable values for honest later replay.
                let observed = match self.observe_agent(selected.actor_id).await {
                    Ok(agent) => agent.metrics,
                    Err(error) => {
                        tracing::warn!(%error, "wait agent metrics unavailable");
                        AgentMetrics::default()
                    }
                };
                metrics.insert(selected.actor_id.to_string(), observed);
            }
            ledger
                .freeze_wait_metrics(ticket.caller, ticket.id, claim.id, &metrics)
                .await
                .map_err(work_error)?
        };
        let metrics = ticket
            .selected
            .iter()
            .map(|selected| {
                let actor = selected.actor_id.to_string();
                let path = self
                    .ensure_agent_known(selected.actor_id)?
                    .agent_path
                    .ok_or_else(|| work_error("selected agent has no logical path"))?;
                Ok((
                    path.to_string(),
                    metrics.get(&actor).cloned().unwrap_or_default(),
                ))
            })
            .collect::<CodexResult<BTreeMap<_, _>>>()?;
        let claimed_ids: HashSet<_> = claim.delivery_ids.iter().copied().collect();
        let mut outcomes = Vec::with_capacity(claim.outcomes.len());
        let mut deliveries = Vec::new();
        let recipient = self
            .ensure_agent_known(ticket.caller.actor_id)?
            .agent_path
            .ok_or_else(|| work_error("wait caller has no canonical task name"))?;
        let mut terminal_ids = HashSet::with_capacity(claim.outcomes.len());
        for work in &claim.outcomes {
            if work.disposition == super::work::WorkDisposition::Paused {
                outcomes.push(WaitOutcome {
                    thread_id: work.stamp.actor_id,
                    task_name: self.coordination_target(work.stamp)?.task_name,
                    work_id: work.stamp.work_id,
                    status: WaitOutcomeStatus::Paused,
                    answers: Vec::new(),
                    capture: None,
                });
                continue;
            }
            let delivery = ledger
                .terminal_delivery(work.stamp)
                .await
                .map_err(work_error)?
                .ok_or_else(|| work_error("selected terminal work has no delivery"))?;
            let CoordinationPayload::Terminal {
                status,
                mut answers,
                mut capture,
            } = serde_json::from_str(&delivery.publication.payload).map_err(work_error)?
            else {
                return Err(work_error(
                    "selected terminal delivery has a nonterminal payload",
                ));
            };
            // A terminal fact settles transitive or already-read selected work, but
            // only this recipient's claimed notification delivers its answer again.
            if !claimed_ids.contains(&delivery.publication.id) {
                answers.clear();
                capture = None;
            }
            terminal_ids.insert(delivery.publication.id);
            let task_name = self
                .ensure_agent_known(work.stamp.actor_id)?
                .agent_path
                .ok_or_else(|| work_error("selected agent has no logical path"))?;
            if project_capture && let Some(capture) = &mut capture {
                capture.full_text = self
                    .project_final_capture(work.stamp, ticket.caller.actor_id, capture)
                    .await;
            }
            // Only this caller's unread claimed notification carries a final body.
            // Transitive/already-read terminal facts remain status-only outcomes.
            if claimed_ids.contains(&delivery.publication.id) {
                deliveries.push(super::work_lifecycle::terminal_delivery_projection(
                    &delivery,
                    task_name.clone(),
                    recipient.clone(),
                    &status,
                    &answers,
                    capture.as_ref(),
                )?);
            }
            let capture = capture.map(|capture| capture.full_text);
            outcomes.push(WaitOutcome {
                thread_id: work.stamp.actor_id,
                task_name,
                work_id: work.stamp.work_id,
                status: WaitOutcomeStatus::Terminal(status),
                answers,
                capture,
            });
        }
        let mut messages = Vec::new();
        let mut projected_messages = Vec::new();
        for id in &claim.delivery_ids {
            if terminal_ids.contains(id) {
                continue;
            }
            let delivery = ledger
                .observe_delivery(ticket.caller.actor_id, *id)
                .await
                .map_err(work_error)?
                .ok_or_else(|| work_error("selected addressed delivery is absent"))?;
            let payload: CoordinationPayload =
                serde_json::from_str(&delivery.publication.payload).map_err(work_error)?;
            let (sender_thread_id, sender_task_name) = match &payload {
                CoordinationPayload::ForeignMessage { origin, .. } => {
                    (Some(origin.actor_id), Some(origin.agent_path.clone()))
                }
                CoordinationPayload::Message { communication } => (
                    delivery.publication.sender_id,
                    Some(communication.author.clone()),
                ),
                _ => (
                    delivery.publication.sender_id,
                    delivery
                        .publication
                        .sender_id
                        .map(|actor| self.ensure_agent_known(actor).map(|actor| actor.agent_path))
                        .transpose()?
                        .flatten(),
                ),
            };
            let input = super::work_lifecycle::delivery_input_from_payload(
                &delivery,
                payload.clone(),
                sender_task_name.clone(),
                recipient.clone(),
            )?;
            let TurnInput::InterAgentCommunication(message) = input else {
                return Err(work_error(
                    "claimed addressed message has no agent communication payload",
                ));
            };
            // Reuse the renderer's attribution prefix without interpreting message text.
            let mode = if message.trigger_turn {
                crate::agent::types::MessageDeliveryMode::TriggerTurn
            } else {
                crate::agent::types::MessageDeliveryMode::QueueOnly
            };
            let prefix = crate::agent::types::AgentMessage::Plaintext(String::new())
                .into_communication(message.author.clone(), message.recipient.clone(), mode)
                .content;
            let body = message
                .content
                .strip_prefix(&prefix)
                .unwrap_or(&message.content)
                .to_owned();
            projected_messages.push(CoordinationWaitMessage {
                delivery_id: *id,
                sender_thread_id,
                sender_task_name,
                recipient: message.recipient.clone(),
                body,
            });
            messages.push(WaitMessage {
                sender_thread_id: delivery.publication.sender_id,
                payload,
            });
        }
        let projection = CoordinationProjection {
            deliveries,
            interrupt: None,
            wait: Some(self.wait_settlement(
                ticket,
                CoordinationWaitResult::Released {
                    wake: match claim.wake {
                        WaitWake::Events => CoordinationWaitWake::Events,
                        WaitWake::Deadline => CoordinationWaitWake::Deadline,
                        WaitWake::Progress => CoordinationWaitWake::Progress,
                    },
                },
                projected_messages,
            )?),
        };
        let text = serde_json::to_string(&WaitContinuation {
            wait_id: ticket.id,
            reason: ticket.origin.reason(),
            wake: claim.wake,
            reset: ticket.reset.clone(),
            metrics,
            messages,
            outcomes,
            progress: if claim.wake == WaitWake::Progress {
                Some(self.progress_output(ticket).await?)
            } else {
                None
            },
        })
        .map_err(work_error)?;
        let text = if ticket.origin.cell().is_some() {
            text
        } else {
            format!("Native logical continuation:\n{text}")
        };
        let history_truncation_token_limit =
            codex_utils_output_truncation::approx_token_count(&text);
        // One complete output carries the claim proof, including status-only transitive
        // results. Only caller-owned delivery IDs may be consumed by this continuation.
        let item = if ticket.origin.cell().is_some() {
            ResponseItem::FunctionCallOutput {
                id: None,
                call_id: None,
                name: Some("wait_agent".to_owned()),
                namespace: None,
                output: FunctionCallOutputPayload::from_text(text),
                internal_chat_message_metadata_passthrough: None,
            }
        } else {
            // Recovery/provider continuation has no authored function call. Keep
            // its exact claim proof on one complete attributed harness message.
            ResponseItem::Message {
                encrypted_content: None,
                status: None,
                id: None,
                role: "developer".to_owned(),
                content: vec![ContentItem::InputText { text }],
                phase: None,
                internal_chat_message_metadata_passthrough: None,
            }
        };
        Ok(ResponseItemEnvelope {
            item,
            metadata: Some(CodexHarnessMetadata {
                coordination: Some(projection),
                coordination_delivery_ids: claim.delivery_ids.clone(),
                coordination_wait_claim_id: Some(claim.id),
                history_truncation_token_limit: Some(history_truncation_token_limit),
                ..Default::default()
            }),
        })
    }

    fn coordination_target(&self, work: super::work::WorkStamp) -> CodexResult<CoordinationTarget> {
        Ok(CoordinationTarget {
            thread_id: work.actor_id,
            work_id: work.work_id,
            task_name: self
                .ensure_agent_known(work.actor_id)?
                .agent_path
                .ok_or_else(|| work_error("selected actor has no canonical task name"))?,
        })
    }

    fn wait_settlement(
        &self,
        ticket: &WaitTicket,
        result: CoordinationWaitResult,
        messages: Vec<CoordinationWaitMessage>,
    ) -> CodexResult<CoordinationWaitSettlement> {
        use super::work::WorkDisposition;
        use codex_state::coordination_wait::WaitMode;
        use codex_state::coordination_wait::WaitOrigin;
        let origin = match &ticket.origin {
            WaitOrigin::Cell { cell } => CoordinationWaitOrigin::Cell {
                turn_id: cell.turn_id.clone(),
                item_id: cell.item_id.clone(),
                window_id: cell.window_id.clone(),
                cell_id: cell.cell_id.clone(),
            },
            WaitOrigin::Turn { turn_id, reason } => CoordinationWaitOrigin::Turn {
                turn_id: turn_id.clone(),
                reason: match reason {
                    TurnWaitReason::DescendantRecovery => {
                        CoordinationTurnWaitReason::DescendantRecovery
                    }
                    TurnWaitReason::ProviderCapacity => {
                        CoordinationTurnWaitReason::ProviderCapacity
                    }
                    TurnWaitReason::ProviderUsageLimit => {
                        CoordinationTurnWaitReason::ProviderUsageLimit
                    }
                },
            },
        };
        let mut outcomes = Vec::new();
        if let Some(claim) = &ticket.claim {
            for work in &claim.outcomes {
                outcomes.push(CoordinationWaitOutcome {
                    target: self.coordination_target(work.stamp)?,
                    disposition: match work.disposition {
                        WorkDisposition::Completed => CoordinationWaitDisposition::Completed,
                        WorkDisposition::Failed => CoordinationWaitDisposition::Failed,
                        WorkDisposition::Interrupted => CoordinationWaitDisposition::Interrupted,
                        WorkDisposition::Paused => CoordinationWaitDisposition::Paused,
                        _ => return Err(work_error("claimed outcome has not stopped")),
                    },
                });
            }
        }
        Ok(CoordinationWaitSettlement {
            wait_id: ticket.id,
            caller_thread_id: ticket.caller.actor_id,
            caller_work_id: ticket.caller.work_id,
            origin,
            mode: match ticket.options.mode {
                WaitMode::Any => CoordinationWaitMode::Any,
                WaitMode::All => CoordinationWaitMode::All,
            },
            selected: ticket
                .selected
                .iter()
                .map(|work| self.coordination_target(*work))
                .collect::<CodexResult<_>>()?,
            claim_id: ticket.claim.as_ref().map(|claim| claim.id),
            result,
            messages,
            outcomes,
        })
    }

    /// Observe only after the owner transaction commits. Publication is auxiliary:
    /// a history/host failure cannot undo accepted input or terminal work.
    pub(crate) async fn emit_cancelled_wait(&self, previous: &WaitTicket) {
        use codex_state::coordination_wait::WaitCancellation;
        use codex_state::coordination_wait::WaitDispatchRefusal;
        use codex_state::coordination_wait::WaitPhase;
        if previous.phase == WaitPhase::Cancelled || previous.phase == WaitPhase::Consumed {
            return;
        }
        let publish = async {
            let Some(ticket) = self
                .work_ledger()?
                .observe_wait(previous.caller, previous.id)
                .await
                .map_err(work_error)?
            else {
                return Ok(());
            };
            if ticket.phase != WaitPhase::Cancelled {
                return Ok(());
            }
            let cause = match ticket
                .cancellation
                .as_ref()
                .ok_or_else(|| work_error("cancelled wait has no closure evidence"))?
            {
                WaitCancellation::Cancelled => CoordinationWaitCancellation::Cancelled,
                WaitCancellation::ExecutionFailed => CoordinationWaitCancellation::ExecutionFailed,
                WaitCancellation::ShellOutput { turn_id } => {
                    CoordinationWaitCancellation::ShellOutput {
                        turn_id: turn_id.clone(),
                    }
                }
                WaitCancellation::InputSuperseded { delivery_id } => {
                    CoordinationWaitCancellation::InputSuperseded {
                        delivery_id: *delivery_id,
                    }
                }
                WaitCancellation::WorkTerminated { delivery_id } => {
                    CoordinationWaitCancellation::WorkTerminated {
                        delivery_id: *delivery_id,
                    }
                }
                WaitCancellation::DispatchRefused {
                    refusal: WaitDispatchRefusal::Rejected { diagnostic },
                } => CoordinationWaitCancellation::DispatchRejected {
                    diagnostic: diagnostic.clone(),
                },
                WaitCancellation::DispatchRefused {
                    refusal: WaitDispatchRefusal::NotDispatched { diagnostic },
                } => CoordinationWaitCancellation::NotDispatched {
                    diagnostic: diagnostic.clone(),
                },
            };
            self.publish_wait_projection(
                &ticket,
                CoordinationProjection {
                    deliveries: Vec::new(),
                    wait: Some(self.wait_settlement(
                        &ticket,
                        CoordinationWaitResult::Cancelled { cause },
                        Vec::new(),
                    )?),
                    interrupt: None,
                },
            )
            .await;
            Ok::<(), codex_protocol::error::CodexErr>(())
        }
        .await;
        if let Err(error) = publish {
            tracing::warn!(%error, wait_id = %previous.id, "cancelled wait projection unavailable");
        }
    }

    async fn publish_wait_projection(
        &self,
        ticket: &WaitTicket,
        projection: CoordinationProjection,
    ) {
        let event = AgentCoordinationEvent {
            thread_id: ticket.caller.actor_id,
            turn_id: ticket.origin.turn_id().to_owned(),
            projection,
        };
        if let Some(manager) = self.manager.upgrade()
            && let Ok(thread) = manager.get_thread(event.thread_id).await
        {
            thread
                .session
                .send_event_raw(codex_protocol::protocol::Event {
                    id: event.turn_id.clone(),
                    msg: codex_protocol::protocol::EventMsg::AgentCoordination(event),
                })
                .await;
        } else if let Some(host) = self.host_callbacks.get() {
            (host.publish_coordination)(event).await;
        }
    }

    async fn record_wait_delivery_features(
        &self,
        ticket: &WaitTicket,
        ledger: &super::work::WorkLedger,
    ) {
        if let Some(claim) = &ticket.claim {
            if claim.wake == WaitWake::Progress {
            }
            for id in &claim.delivery_ids {
                let delivery = match ledger.observe_delivery(ticket.caller.actor_id, *id).await {
                    Ok(Some(delivery)) => delivery,
                    Ok(None) => continue,
                    Err(error) => {
                        tracing::warn!(%error, "cannot measure delivered wait result");
                        continue;
                    }
                };
                if let (
                    Some(actor),
                    Ok(CoordinationPayload::Terminal {
                        status: AgentStatus::Completed(_),
                        capture,
                        ..
                    }),
                ) = (
                    delivery.publication.sender_id,
                    serde_json::from_str(&delivery.publication.payload),
                ) {
                }
            }
        }
    }
}

#[cfg(test)]
#[path = "wait_delivery_tests.rs"]
mod tests;

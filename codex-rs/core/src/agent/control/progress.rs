//! Capture resources are host-owned; wait selection, slots and delivery remain native.

use super::LocalAgentRuntime;
use super::work::WorkStamp;
use super::work_lifecycle::work_error;
use crate::agent::host_resources::FreezeProgressRequest;
use crate::agent::host_resources::PrepareProgressOutputRequest;
use crate::agent::host_resources::ProjectFinalCaptureRequest;
use codex_protocol::ThreadId;
use codex_protocol::error::CodexErr;
use codex_protocol::error::Result as CodexResult;
use codex_state::coordination_progress::*;
use codex_state::coordination_wait::WaitPhase;
use codex_state::coordination_wait::WaitTicket;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

impl LocalAgentRuntime {
    pub(crate) async fn recipient_has_capture_suppression(
        &self,
        recipient: ThreadId,
    ) -> CodexResult<bool> {
        let context = self.recipient_delivery_context(recipient).await?;
        let ledger = self.work_ledger()?;
        Ok(!ledger
            .state()
            .coordination_capture_cursors(ledger.root().map_err(work_error)?, recipient, context)
            .await
            .map_err(work_error)?
            .is_empty())
    }

    pub(crate) async fn recipient_delivery_context(
        &self,
        recipient: ThreadId,
    ) -> CodexResult<Uuid> {
        let manager = self
            .manager
            .upgrade()
            .ok_or_else(|| work_error("runtime unavailable"))?;
        let thread = manager.get_thread(recipient).await?;
        Ok(thread.session.delivery_context_id().await)
    }

    /// Called by the existing live wait timer, never reconstructed from SQL time.
    /// Host I/O gets a separate joined membership so it cannot delay the deadline.
    pub(crate) fn start_progress_collection(&self, ticket: &WaitTicket, cancel: CancellationToken) {
        let runtime = self.clone();
        let guard = match runtime.admit_start() {
            Ok(guard) => {
                guard.into_teardown_guard("progress_capture", Some(ticket.caller.actor_id))
            }
            Err(_) => return,
        };
        let ticket = ticket.clone();
        tokio::spawn(async move {
            let result = runtime.collect_progress(&ticket, &cancel).await;
            if let Err(error) = result {
                tracing::warn!(%error, wait_id = %ticket.id, "native progress collection unavailable");
            }
            guard.complete();
        });
    }

    async fn collect_progress(
        &self,
        ticket: &WaitTicket,
        cancel: &CancellationToken,
    ) -> CodexResult<()> {
        let ledger = self.work_ledger()?;
        if let Some(claim) = ledger
            .claim_ready_wait_at(ticket.caller, ticket.id, self.wait_readiness(ticket))
            .await
            .map_err(work_error)?
        {
            if claim.wake != codex_state::coordination_wait::WaitWake::Progress {
                self.stop_wait_collection(ticket.caller.actor_id, ticket.id);
            }
            self.notify_coordination(ticket.caller.actor_id).await;
            return Ok(());
        }
        let context = self
            .recipient_delivery_context(ticket.caller.actor_id)
            .await?;
        let Some(batch) = ledger
            .state()
            .coordination_begin_progress(ticket.caller, self.tree_id(), ticket.id, context)
            .await
            .map_err(work_error)?
        else {
            return Ok(());
        };
        let acquire = async {
            let host = self
                .host_callbacks
                .get()
                .ok_or_else(|| work_error("progress capture owner is unavailable"))?;
            // Selection can retain an unread old outcome alongside current work
            // for one actor. Capture authorship is actor-scoped; freeze it once,
            // preferring the newest selected work rather than duplicate intervals.
            let mut selected = std::collections::BTreeMap::new();
            for work in &ticket.selected {
                let task_name = self
                    .ensure_agent_known(work.actor_id)?
                    .agent_path
                    .ok_or_else(|| work_error("selected actor has no canonical task name"))?
                    .to_string();
                selected.insert(
                    work.actor_id.to_string(),
                    ProgressActor {
                        thread_id: work.actor_id,
                        work_id: work.work_id,
                        task_name,
                    },
                );
            }
            let selected: Vec<_> = selected.into_values().collect();
            let seen = ledger
                .state()
                .coordination_capture_cursors(
                    ticket.caller.root_id,
                    ticket.caller.actor_id,
                    context,
                )
                .await
                .map_err(work_error)?;
            let request = FreezeProgressRequest {
                caller: ticket.caller,
                wait_id: ticket.id,
                batch_id: batch.batch_id,
                recipient_context_id: context,
                selected: selected.clone(),
                seen,
            };
            let frozen = (host.freeze_progress)(request, cancel.child_token()).await?;
            if frozen
                .agents
                .iter()
                .any(|agent| !selected.contains(&agent.actor))
            {
                return Err(work_error("capture owner changed selected actor identity"));
            }
            Ok(frozen)
        };
        let acquired = tokio::select! {
            biased;
            _ = self.shutdown.cancelled() => Err(CodexErr::TurnAborted),
            _ = cancel.cancelled() => Err(CodexErr::TurnAborted),
            result = acquire => result,
        };
        let frozen = match acquired {
            Ok(frozen) => frozen,
            Err(error) => {
                // Remove only this undispatched reservation. Cancellation of a
                // host request is owned/joined by its connection callback owner.
                let _ = ledger
                    .state()
                    .coordination_retire_progress(
                        ticket.caller,
                        self.tree_id(),
                        ticket.id,
                        batch.batch_id,
                    )
                    .await;
                return Err(error);
            }
        };
        if !ledger
            .state()
            .coordination_freeze_progress(
                ticket.caller,
                self.tree_id(),
                ticket.id,
                batch.batch_id,
                &frozen,
            )
            .await
            .map_err(work_error)?
        {
            return Ok(());
        }
        for target in frozen
            .agents
            .into_iter()
            .filter(|agent| !agent.intervals.is_empty())
        {
            let manager = self
                .manager
                .upgrade()
                .ok_or_else(|| work_error("runtime unavailable"))?;
            match manager.get_thread(target.actor.thread_id).await {
                Ok(thread) => {
                    // A running task services the slot only after its tools drain.
                    // A loaded checkpoint can snapshot now without primary input.
                    thread.session.service_checkpointed_progress().await;
                }
                Err(_) => {
                    let probe = SummaryProbe {
                        caller: ticket.caller,
                        wait_id: ticket.id,
                        batch_id: batch.batch_id,
                        target,
                    };
                    self.resolve_progress_probe(
                        &probe,
                        SummaryResult::Unavailable {
                            reason: "selected target is not loaded".into(),
                        },
                    )
                    .await?;
                }
            }
        }
        self.notify_coordination(ticket.caller.actor_id).await;
        Ok(())
    }

    pub(crate) async fn take_progress_probes(
        &self,
        target: WorkStamp,
    ) -> CodexResult<Vec<SummaryProbe>> {
        self.work_ledger()?
            .state()
            .coordination_take_progress_probes(target, self.tree_id())
            .await
            .map_err(work_error)
    }

    pub(crate) async fn resolve_progress_probe(
        &self,
        probe: &SummaryProbe,
        result: SummaryResult,
    ) -> CodexResult<()> {
        self.work_ledger()?
            .state()
            .coordination_resolve_progress(self.tree_id(), probe, &result)
            .await
            .map_err(work_error)?;
        Ok(())
    }

    /// Called on normal mailbox/terminal notifications as well as slot completion.
    /// It never starts primary inference and cannot turn a collection into a claim.
    pub(crate) async fn prepare_ready_progress(&self, actor: ThreadId) -> CodexResult<()> {
        let ledger = self.work_ledger()?;
        let Some(ticket) = ledger.current_wait(actor).await.map_err(work_error)? else {
            return Ok(());
        };
        if ticket.runtime_id != self.tree_id() || !matches!(ticket.phase, WaitPhase::Registered) {
            return Ok(());
        }
        let Some(batch) = ledger
            .state()
            .coordination_progress_batch(ticket.caller, self.tree_id(), ticket.id)
            .await
            .map_err(work_error)?
        else {
            return Ok(());
        };
        if batch.output.is_some() {
            return Ok(());
        }
        let Some(frozen) = batch.frozen else {
            return Ok(());
        };
        // A normal result claims before output preparation and cancels lower
        // priority inference. An all-wait terminal still only settles its slot.
        if let Some(claim) = ledger
            .claim_ready_wait_at(ticket.caller, ticket.id, self.wait_readiness(&ticket))
            .await
            .map_err(work_error)?
        {
            if claim.wake != codex_state::coordination_wait::WaitWake::Progress {
                self.stop_wait_collection(ticket.caller.actor_id, ticket.id);
            }
            return Ok(());
        }
        let Some(agents) = ledger
            .state()
            .coordination_progress_agents(ticket.caller, self.tree_id(), ticket.id, batch.batch_id)
            .await
            .map_err(work_error)?
        else {
            return Ok(());
        };
        let Some(cancel) = self.progress_cancellation(actor, ticket.id) else {
            return Ok(());
        };
        let Some(host) = self.host_callbacks.get() else {
            return Ok(());
        };
        let request = PrepareProgressOutputRequest {
            caller: ticket.caller,
            wait_id: ticket.id,
            batch_id: batch.batch_id,
            recipient_context_id: batch.recipient_context_id,
            snapshot_ref: frozen.snapshot_ref,
            agents,
            coverage: frozen.coverage,
        };
        let owner = self.for_actor(actor)?;
        let guard = owner
            .admit_start()?
            .into_teardown_guard("progress_output", Some(actor));
        let output = (host.prepare_progress_output)(request, cancel).await;
        guard.complete();
        let output = output?;
        ledger
            .state()
            .coordination_prepare_progress_output(
                ticket.caller,
                self.tree_id(),
                ticket.id,
                batch.batch_id,
                &output,
            )
            .await
            .map_err(work_error)?;
        Ok(())
    }

    pub(crate) async fn progress_output(
        &self,
        ticket: &WaitTicket,
    ) -> CodexResult<PreparedProgressOutput> {
        self.work_ledger()?
            .state()
            .coordination_progress_batch(ticket.caller, self.tree_id(), ticket.id)
            .await
            .map_err(work_error)?
            .and_then(|batch| batch.output)
            .ok_or_else(|| work_error("progress claim has no frozen output"))
    }

    pub(crate) async fn project_final_capture(
        &self,
        work: WorkStamp,
        recipient: ThreadId,
        capture: &FrozenFinalCapture,
    ) -> String {
        let projected = async {
            let context = self.recipient_delivery_context(recipient).await.ok()?;
            let seen = self
                .work_ledger()
                .ok()?
                .state()
                .coordination_capture_cursors(work.root_id, recipient, context)
                .await
                .ok()?;
            if seen.is_empty() {
                return None;
            }
            let host = self.host_callbacks.get()?;
            let cwd = self.observe_agent(recipient).await.ok()?.cwd;
            let request = ProjectFinalCaptureRequest {
                work,
                recipient,
                recipient_cwd: cwd,
                recipient_context_id: context,
                resource_ref: capture.resource_ref.clone(),
                seen,
            };
            let owner = self.for_actor(recipient).ok()?;
            let membership = owner.admit_start().ok()?;
            let projection = (host.project_final_capture)(request, owner.shutdown.child_token())
                .await
                .ok()?;
            drop(membership);
            if self.recipient_delivery_context(recipient).await.ok()? != context {
                return None;
            }
            Some(projection.text)
        }
        .await;
        projected.unwrap_or_else(|| capture.full_text.clone())
    }
}

impl LocalAgentRuntime {
    /// Recompose an exact native item from its frozen resources for this live
    /// recipient. Retained history stays full, so reset/fork/load never inherits
    /// an irreversibly shortened successful-final attachment.
    pub(crate) async fn project_sampling_final_item(
        &self,
        recipient: ThreadId,
        claim: Option<Uuid>,
        deliveries: &[Uuid],
    ) -> CodexResult<Option<codex_protocol::models::ResponseItem>> {
        use super::work_lifecycle::CoordinationPayload;
        use super::work_lifecycle::delivery_input;
        use crate::session::TurnInput;
        let ledger = self.work_ledger()?;
        if let Some(claim) = claim {
            let Some(ticket) = ledger
                .state()
                .coordination_observe_wait_claim(
                    ledger.root().map_err(work_error)?,
                    recipient,
                    claim,
                )
                .await
                .map_err(work_error)?
            else {
                return Ok(None);
            };
            if ticket
                .claim
                .as_ref()
                .is_none_or(|claim| claim.outcomes.is_empty())
            {
                return Ok(None);
            }
            let input = self.materialize_wait_continuation(&ticket, true).await?;
            return Ok(Some(input.item));
        }
        if deliveries.len() != 1 {
            return Ok(None);
        }
        let Some(mut delivery) = ledger
            .observe_delivery(recipient, deliveries[0])
            .await
            .map_err(work_error)?
        else {
            return Ok(None);
        };
        let mut payload: CoordinationPayload =
            serde_json::from_str(&delivery.publication.payload).map_err(work_error)?;
        let CoordinationPayload::Terminal {
            capture: Some(capture),
            ..
        } = &mut payload
        else {
            return Ok(None);
        };
        let Some(work) = delivery.publication.work else {
            return Ok(None);
        };
        capture.full_text = self.project_final_capture(work, recipient, capture).await;
        delivery.publication.payload = serde_json::to_string(&payload).map_err(work_error)?;
        let author = delivery.publication.sender_id.and_then(|actor| {
            self.ensure_agent_known(actor)
                .ok()
                .and_then(|actor| actor.agent_path)
        });
        let path = self
            .ensure_agent_known(recipient)?
            .agent_path
            .ok_or_else(|| work_error("recipient has no canonical path"))?;
        Ok(match delivery_input(&delivery, author, path)? {
            TurnInput::InterAgentCommunication(message) => Some(message.to_model_input_item()),
            _ => None,
        })
    }
}

use super::delegate::CodeModeDispatchBroker;
use codex_code_mode::CellId;
use codex_code_mode::RuntimeResponse;
use codex_protocol::compound_wait::CompoundExecutionResult;
use codex_protocol::compound_wait::CompoundResetOutcome;
use codex_protocol::compound_wait::CompoundWaitAction;
use codex_protocol::compound_wait::CompoundWaitDisposition;
use codex_protocol::compound_wait::CompoundWaitReset;
use codex_protocol::compound_wait::CompoundWaitSettlement;
use codex_protocol::compound_wait::CompoundWaitTicket;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use tokio::sync::oneshot;

pub(crate) struct ResetAdmission(Arc<CodeModeDispatchBroker>);

pub(crate) struct CompoundRegistration {
    broker: Arc<CodeModeDispatchBroker>,
    cell: CellId,
    settled: bool,
}

impl CompoundRegistration {
    pub(crate) fn finish(mut self, result: Result<Option<CompoundWaitTicket>, String>) {
        self.broker.bind_compound(&self.cell, result);
        self.settled = true;
    }
}

impl Drop for CompoundRegistration {
    fn drop(&mut self) {
        if !self.settled {
            self.broker.bind_compound(
                &self.cell,
                Err("compound invocation canceled before peer settlement".into()),
            );
        }
    }
}

impl Drop for ResetAdmission {
    fn drop(&mut self) {
        self.0.reset_admitted.store(false, Ordering::Release);
    }
}

impl crate::session::session::Session {
    // This barrier runs after sampled tool work drains and before any reset or
    // subsequent model request. The cell owner retains evidence across yields.
    pub(crate) async fn settle_compound_waits(
        self: &Arc<Self>,
        step: &Arc<crate::session::step_context::StepContext>,
        cancellation: &tokio_util::sync::CancellationToken,
    ) -> codex_protocol::error::Result<bool> {
        use codex_protocol::error::CodexErr;
        use codex_protocol::protocol::EventMsg;
        use codex_protocol::protocol::WarningEvent;

        let broker = &self.services.code_mode_service.dispatch_broker;
        if !broker.compound_pending() {
            return Ok(false);
        }
        let steering = step.preempt.clone().unwrap_or_default();
        let _input_watch = self
            .input_queue
            .watch_user_input(&self.active_turn, &step.turn.sub_id, steering.clone())
            .await;
        // A terminal JavaScript module does not join detached native dispatch.
        // Await those invocation owners, including their cancellation cleanup.
        tokio::select! {
            biased;
            _ = cancellation.cancelled() => return Err(CodexErr::TurnAborted),
            _ = steering.cancelled() => return self.hold_uncertain_compound(step, cancellation, &steering).await,
            _ = broker.wait_terminal_registrations() => {}
        }
        let uncertain = broker.uncertain_compounds();
        if !uncertain.is_empty() {
            for receipt in uncertain {
                self.send_event(&step.turn, EventMsg::CompoundWaitReset(receipt))
                    .await;
            }
            self.send_event(
                &step.turn,
                EventMsg::Warning(WarningEvent {
                    message: "Compound wait registration is uncertain; paused without resetting."
                        .into(),
                }),
            )
            .await;
            return self
                .hold_uncertain_compound(step, cancellation, &steering)
                .await;
        }
        let mut committed: Option<(CellId, Option<CompoundWaitDisposition>, CompoundWaitReset)> =
            None;
        for (request, response) in broker.compound_settlements() {
            let cell_id = CellId::new(request.cell_id.clone());
            self.send_event(
                &step.turn,
                EventMsg::CompoundWaitSettlement(request.clone()),
            )
            .await;
            let disposition = tokio::select! {
                biased;
                _ = cancellation.cancelled() => return Err(CodexErr::TurnAborted),
                _ = steering.cancelled() => {
                    return self.hold_uncertain_compound(step, cancellation, &steering).await;
                }
                response = response => response.ok(),
            };
            let action = disposition
                .as_ref()
                .map(|reply| reply.action)
                .unwrap_or(CompoundWaitAction::Uncertain);
            match action {
                CompoundWaitAction::Retain => continue,
                CompoundWaitAction::Retire | CompoundWaitAction::Superseded => {
                    broker.finish_compound(&cell_id);
                    continue;
                }
                CompoundWaitAction::Commit | CompoundWaitAction::Uncertain => {}
            }
            let mut receipt = CompoundWaitReset {
                thread_id: request.thread_id,
                turn_id: request.turn_id,
                item_id: request.item_id,
                window_id: request.window_id,
                cell_id: request.cell_id,
                ticket: Some(request.ticket),
                reset: CompoundResetOutcome::Uncertain,
                diagnostic: disposition
                    .as_ref()
                    .and_then(|reply| reply.diagnostic.clone()),
            };
            if action == CompoundWaitAction::Uncertain {
                receipt.diagnostic.get_or_insert_with(|| {
                    "Compound wait admission is uncertain; reset was not retried.".into()
                });
                if let Some((_, _, mut accepted)) = committed.take() {
                    accepted.diagnostic = receipt.diagnostic.clone();
                    self.send_event(&step.turn, EventMsg::CompoundWaitReset(accepted))
                        .await;
                }
                self.send_event(&step.turn, EventMsg::CompoundWaitReset(receipt.clone()))
                    .await;
                self.send_event(
                    &step.turn,
                    EventMsg::Warning(WarningEvent {
                        message: receipt.diagnostic.unwrap_or_default(),
                    }),
                )
                .await;
                return self
                    .hold_uncertain_compound(step, cancellation, &steering)
                    .await;
            }
            committed = Some((cell_id, disposition, receipt));
        }
        let Some((cell_id, disposition, mut receipt)) = committed else {
            return Ok(false);
        };
        // Steering can supersede admission while the response is in transit.
        if cancellation.is_cancelled() || steering.is_cancelled() {
            receipt.reset = CompoundResetOutcome::Canceled;
            self.send_event(&step.turn, EventMsg::CompoundWaitReset(receipt))
                .await;
            broker.finish_compound(&cell_id);
            if cancellation.is_cancelled() {
                return Err(CodexErr::TurnAborted);
            }
            return Ok(false);
        }
        let Some(_admission) = broker.admit_reset(Some(&cell_id)) else {
            receipt.diagnostic =
                Some("Compound reset admission is unresolved; reset was not retried.".into());
            self.send_event(&step.turn, EventMsg::CompoundWaitReset(receipt))
                .await;
            return self
                .hold_uncertain_compound(step, cancellation, &steering)
                .await;
        };
        broker.commit_compound(&cell_id);
        if let Some(selection) = disposition.and_then(|reply| reply.reload) {
            self.record_retained_context(codex_history::RetainedContextEvent::ReloadSelection {
                selection,
            })
            .await;
        }
        let reset_cancellation = cancellation.child_token();
        let reset = async {
            crate::compact_token_budget::run_inline_compact_task(
                Arc::clone(self),
                Arc::clone(step),
                codex_analytics::CompactionTrigger::Manual,
                true,
                &reset_cancellation,
            )
            .await
        };
        tokio::pin!(reset);
        let reset = tokio::select! {
            biased;
            _ = steering.cancelled() => {
                reset_cancellation.cancel();
                reset.await
            },
            result = &mut reset => result,
        };
        match reset {
            Ok(()) => receipt.reset = CompoundResetOutcome::Completed,
            Err(err) => {
                receipt.reset = if cancellation.is_cancelled() || steering.is_cancelled() {
                    CompoundResetOutcome::Canceled
                } else {
                    CompoundResetOutcome::Failed
                };
                receipt.diagnostic = Some(err.to_string());
                self.send_event(
                    &step.turn,
                    EventMsg::Warning(WarningEvent {
                        message: format!("Compound reset failed: {err}"),
                    }),
                )
                .await;
            }
        }
        self.send_event(&step.turn, EventMsg::CompoundWaitReset(receipt))
            .await;
        if steering.is_cancelled() && !cancellation.is_cancelled() {
            broker.finish_compound(&cell_id);
            return Ok(false);
        }
        Ok(true)
    }

    async fn hold_uncertain_compound(
        &self,
        step: &crate::session::step_context::StepContext,
        cancellation: &tokio_util::sync::CancellationToken,
        steering: &tokio_util::sync::CancellationToken,
    ) -> codex_protocol::error::Result<bool> {
        // Keep the authoritative host alive, including while Go registration
        // may still be in flight. Only real steering or cancellation releases
        // this hold; no reset retry or model sampling reconciles uncertainty.
        tokio::select! {
            biased;
            _ = cancellation.cancelled() => return Err(codex_protocol::error::CodexErr::TurnAborted),
            _ = steering.cancelled() => {}
        }
        for receipt in self
            .services
            .code_mode_service
            .dispatch_broker
            .end_compound_turn(&step.turn.sub_id, true)
        {
            self.send_event(
                &step.turn,
                codex_protocol::protocol::EventMsg::CompoundWaitReset(receipt),
            )
            .await;
        }
        Ok(false)
    }
}

pub(super) struct CompoundCell {
    turn_id: String,
    ticket: Option<CompoundWaitTicket>,
    in_flight: usize,
    uncertain: bool,
    committed: bool,
    evidence: Option<CompoundExecutionResult>,
    reply: Option<oneshot::Sender<CompoundWaitDisposition>>,
}

impl CodeModeDispatchBroker {
    pub(crate) fn begin_compound(
        self: &Arc<Self>,
        cell_id: &CellId,
        turn_id: &str,
    ) -> Result<CompoundRegistration, String> {
        let mut gates = self
            .dispatch_gates
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if self.reset_admitted.load(Ordering::Acquire) {
            return Err("context reset already admitted; compound wait was not dispatched".into());
        }
        let gate = gates
            .get_mut(cell_id)
            .ok_or("compound wait cell is unavailable")?;
        if gate.closed {
            return Err("compound wait cell has already settled".into());
        }
        if gate
            .originating_call
            .as_ref()
            .is_none_or(|origin| origin.item_id.is_none() || origin.window_id.is_empty())
        {
            return Err("compound wait requires the original item and window".into());
        }
        let compound = gate.compound.get_or_insert_with(|| CompoundCell {
            turn_id: turn_id.into(),
            ticket: None,
            in_flight: 0,
            uncertain: false,
            committed: false,
            evidence: None,
            reply: None,
        });
        compound.in_flight += 1;
        Ok(CompoundRegistration {
            broker: Arc::clone(self),
            cell: cell_id.clone(),
            settled: false,
        })
    }

    async fn wait_terminal_registrations(&self) {
        loop {
            let changed = self.compound_changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            let pending = {
                let gates = self
                    .dispatch_gates
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                let terminal = gates.values().any(|gate| {
                    gate.compound.as_ref().is_some_and(|compound| {
                        compound
                            .evidence
                            .is_some_and(|evidence| evidence != CompoundExecutionResult::Yielded)
                    })
                });
                terminal
                    && gates.values().any(|gate| {
                        gate.compound
                            .as_ref()
                            .is_some_and(|compound| compound.in_flight != 0)
                    })
            };
            if !pending {
                return;
            }
            changed.await;
        }
    }

    // Only an actual peer response can clear a provisional registration. A
    // transport failure leaves uncertainty attached to this exact existing cell.
    pub(crate) fn bind_compound(
        &self,
        cell_id: &CellId,
        result: Result<Option<CompoundWaitTicket>, String>,
    ) {
        let mut gates = self
            .dispatch_gates
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let Some(gate) = gates.get_mut(cell_id) else {
            return;
        };
        let Some(compound) = gate.compound.as_mut() else {
            return;
        };
        compound.in_flight = compound.in_flight.saturating_sub(1);
        match result {
            Ok(Some(ticket)) => {
                if compound.ticket.as_ref().is_some_and(|old| old != &ticket) {
                    compound.uncertain = true;
                } else {
                    compound.ticket = Some(ticket);
                }
            }
            Ok(None) => {}
            Err(_) => compound.uncertain = true,
        }
        if compound.in_flight == 0 && compound.ticket.is_none() && !compound.uncertain {
            gate.compound = None;
            if gate.closed {
                gates.remove(cell_id);
            }
        }
        self.compound_changed.notify_waiters();
    }

    pub(crate) fn record_compound_evidence(&self, cell_id: &CellId, response: &RuntimeResponse) {
        let mut gates = self
            .dispatch_gates
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(compound) = gates
            .get_mut(cell_id)
            .and_then(|gate| gate.compound.as_mut())
        {
            if compound
                .evidence
                .is_some_and(|result| result != CompoundExecutionResult::Yielded)
            {
                return;
            }
            compound.evidence = Some(match response {
                RuntimeResponse::Yielded { .. } => CompoundExecutionResult::Yielded,
                RuntimeResponse::Terminated { .. } => CompoundExecutionResult::Terminated,
                RuntimeResponse::Result {
                    error_text: None, ..
                } => CompoundExecutionResult::Succeeded,
                RuntimeResponse::Result { .. } => CompoundExecutionResult::Failed,
            });
        }
    }

    pub(crate) fn compound_uncertain(&self, cell_id: &CellId) {
        let mut gates = self
            .dispatch_gates
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(compound) = gates
            .get_mut(cell_id)
            .and_then(|gate| gate.compound.as_mut())
        {
            if !compound.committed
                && compound
                    .evidence
                    .is_none_or(|result| result == CompoundExecutionResult::Yielded)
            {
                compound.uncertain = true;
            }
        }
    }

    pub(crate) fn compound_pending(&self) -> bool {
        self.dispatch_gates
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .values()
            .any(|gate| gate.compound.is_some())
    }

    pub(crate) fn admit_reset(
        self: &Arc<Self>,
        compound_cell: Option<&CellId>,
    ) -> Option<ResetAdmission> {
        let gates = self
            .dispatch_gates
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if self.reset_admitted.load(Ordering::Acquire)
            || gates
                .iter()
                .any(|(id, gate)| gate.compound.is_some() && compound_cell != Some(id))
        {
            return None;
        }
        self.reset_admitted.store(true, Ordering::Release);
        Some(ResetAdmission(Arc::clone(self)))
    }

    pub(crate) fn compound_settlements(
        &self,
    ) -> Vec<(
        CompoundWaitSettlement,
        oneshot::Receiver<CompoundWaitDisposition>,
    )> {
        let mut gates = self
            .dispatch_gates
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if gates.values().any(|gate| {
            gate.compound
                .as_ref()
                .is_some_and(|compound| compound.in_flight != 0)
        }) {
            return Vec::new();
        }
        gates
            .iter_mut()
            .filter_map(|(cell_id, gate)| {
                let origin = gate.originating_call.as_ref()?;
                let compound = gate.compound.as_mut()?;
                if compound.in_flight != 0 || compound.uncertain || compound.reply.is_some() {
                    return None;
                }
                let ticket = compound.ticket.clone()?;
                let result = compound.evidence.take()?;
                let (tx, rx) = oneshot::channel();
                compound.reply = Some(tx);
                Some((
                    CompoundWaitSettlement {
                        thread_id: self.thread_id.to_string(),
                        turn_id: compound.turn_id.clone(),
                        item_id: origin.item_id.as_ref()?.to_string(),
                        window_id: origin.window_id.clone(),
                        cell_id: cell_id.to_string(),
                        ticket,
                        result,
                    },
                    rx,
                ))
            })
            .collect()
    }

    pub(crate) fn compound_response(&self, cell_id: &CellId, response: CompoundWaitDisposition) {
        let mut gates = self
            .dispatch_gates
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let Some(compound) = gates
            .get_mut(cell_id)
            .and_then(|gate| gate.compound.as_mut())
        else {
            return;
        };
        if compound.ticket.as_ref() == Some(&response.ticket)
            && let Some(tx) = compound.reply.take()
        {
            let _ = tx.send(response);
        }
    }

    pub(crate) fn finish_compound(&self, cell_id: &CellId) {
        let mut gates = self
            .dispatch_gates
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(gate) = gates.get_mut(cell_id) {
            gate.compound = None;
            if gate.closed {
                gates.remove(cell_id);
            }
        }
    }

    pub(crate) fn commit_compound(&self, cell_id: &CellId) {
        let mut gates = self
            .dispatch_gates
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(compound) = gates
            .get_mut(cell_id)
            .and_then(|gate| gate.compound.as_mut())
        {
            compound.committed = true;
        }
    }

    pub(crate) fn end_compound_turn(
        &self,
        turn_id: &str,
        canceled: bool,
    ) -> Vec<CompoundWaitReset> {
        let mut gates = self
            .dispatch_gates
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut receipts = Vec::new();
        gates.retain(|cell_id, gate| {
            let Some(compound) = gate.compound.as_ref() else {
                return true;
            };
            if compound.turn_id != turn_id {
                return true;
            }
            if (canceled || !compound.committed)
                && let Some(origin) = gate.originating_call.as_ref()
                && let Some(item) = origin.item_id.as_ref()
            {
                receipts.push(CompoundWaitReset {
                    thread_id: self.thread_id.to_string(),
                    turn_id: turn_id.into(),
                    item_id: item.to_string(),
                    window_id: origin.window_id.clone(),
                    cell_id: cell_id.to_string(),
                    ticket: compound.ticket.clone(),
                    reset: CompoundResetOutcome::Canceled,
                    diagnostic: None,
                });
            }
            gate.compound = None;
            !gate.closed
        });
        receipts
    }

    pub(crate) fn uncertain_compounds(&self) -> Vec<CompoundWaitReset> {
        let gates = self
            .dispatch_gates
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        gates
            .iter()
            .filter_map(|(cell_id, gate)| {
                let compound = gate.compound.as_ref()?;
                if !compound.uncertain {
                    return None;
                }
                let origin = gate.originating_call.as_ref()?;
                Some(CompoundWaitReset {
                    thread_id: self.thread_id.to_string(),
                    turn_id: compound.turn_id.clone(),
                    item_id: origin.item_id.as_ref()?.to_string(),
                    window_id: origin.window_id.clone(),
                    cell_id: cell_id.to_string(),
                    ticket: compound.ticket.clone(),
                    reset: CompoundResetOutcome::Uncertain,
                    diagnostic: Some(
                        "Compound wait registration is uncertain; reset was not retried.".into(),
                    ),
                })
            })
            .collect()
    }
}

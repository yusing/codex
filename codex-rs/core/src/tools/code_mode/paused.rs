//! A paused nested call retains its cell and original reply channel. Observation
//! deadlines remain transport bounds, not permission to yield to the model.

use super::CodeModeService;
use super::delegate::CodeModeDispatchBroker;
use crate::session::session::Session;
use crate::session::step_context::StepContext;
use codex_async_utils::OrCancelExt;
use codex_code_mode::CellId;
use codex_code_mode::ExecuteToPendingOutcome;
use codex_code_mode::RuntimeResponse;
use codex_code_mode::WaitOutcome;
use codex_code_mode::WaitRequest;
use codex_code_mode::WaitToPendingOutcome;
use codex_code_mode::WaitToPendingRequest;
use codex_protocol::error::CodexErr;
use codex_protocol::error::Result as CodexResult;
use codex_state::coordination_wait::WaitCell;
use codex_state::coordination_wait::WaitOrigin;
use codex_state::coordination_wait::WaitPhase;
use codex_state::coordination_wait::WaitTicket;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use tokio_util::sync::CancellationToken;

pub(crate) struct ResetAdmission(Arc<CodeModeDispatchBroker>);

impl Drop for ResetAdmission {
    fn drop(&mut self) {
        self.0.reset_admitted.store(false, Ordering::Release);
        self.0.paused_changed.notify_waiters();
    }
}

pub(super) struct ActiveCallback {
    broker: Arc<CodeModeDispatchBroker>,
    cell: CellId,
}

impl Drop for ActiveCallback {
    fn drop(&mut self) {
        let mut gates = self
            .broker
            .dispatch_gates
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(gate) = gates.get_mut(&self.cell) {
            gate.active_callbacks -= 1;
        }
        drop(gates);
        self.broker.paused_changed.notify_waiters();
    }
}

pub(crate) struct PausedWait {
    broker: Arc<CodeModeDispatchBroker>,
    pub(crate) cell: WaitCell,
}

impl Drop for PausedWait {
    fn drop(&mut self) {
        let mut gates = self
            .broker
            .dispatch_gates
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let id = CellId::new(self.cell.cell_id.clone());
        if let Some(gate) = gates.get_mut(&id) {
            gate.paused_waits -= 1;
            if gate.closed && gate.paused_waits == 0 {
                gates.remove(&id);
            }
        }
        drop(gates);
        self.broker.paused_changed.notify_waiters();
    }
}

impl CodeModeDispatchBroker {
    pub(super) fn begin_callback(self: &Arc<Self>, cell: &CellId) -> ActiveCallback {
        super::delegate::dispatch_gate(&self.dispatch_gates, cell);
        let mut gates = self
            .dispatch_gates
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(gate) = gates.get_mut(cell) {
            gate.active_callbacks += 1;
        }
        ActiveCallback {
            broker: Arc::clone(self),
            cell: cell.clone(),
        }
    }

    // A pending native wait is one cell. Other live cells or callbacks still
    // own execution, even if this cell has reached its native pending frontier.
    fn can_pause_execution(&self, cell: &CellId) -> bool {
        let gates = self
            .dispatch_gates
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut active = gates.iter().filter(|(_, gate)| !gate.closed);
        let Some((id, gate)) = active.next() else {
            return false;
        };
        id == cell
            && active.next().is_none()
            && gate.paused_waits != 0
            && gate.active_callbacks == gate.paused_waits
    }

    pub(crate) fn begin_paused_wait(
        self: &Arc<Self>,
        cell_id: &CellId,
        turn_id: &str,
    ) -> Result<PausedWait, String> {
        let mut gates = self
            .dispatch_gates
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let gate = gates
            .get_mut(cell_id)
            .filter(|gate| !gate.closed)
            .ok_or("native wait cell is unavailable")?;
        let origin = gate
            .originating_call
            .as_ref()
            .ok_or("native wait cell origin is unavailable")?;
        let cell = WaitCell {
            turn_id: turn_id.to_owned(),
            item_id: origin
                .item_id
                .as_ref()
                .ok_or("native wait requires the original item")?
                .to_string(),
            window_id: origin.window_id.clone(),
            cell_id: cell_id.to_string(),
        };
        gate.paused_waits += 1;
        drop(gates);
        self.paused_changed.notify_waiters();
        Ok(PausedWait {
            broker: Arc::clone(self),
            cell,
        })
    }

    pub(crate) fn wake_paused_waits(&self) {
        self.paused_changed.notify_waiters();
    }

    pub(crate) fn has_paused_waits(&self) -> bool {
        self.dispatch_gates
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .values()
            .any(|gate| gate.paused_waits != 0)
    }

    pub(super) fn cell_paused(&self, cell: &CellId) -> bool {
        self.dispatch_gates
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(cell)
            .is_some_and(|gate| gate.paused_waits != 0)
    }

    pub(crate) fn admit_reset(self: &Arc<Self>) -> Option<ResetAdmission> {
        self.reset_admitted
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .ok()
            .map(|_| ResetAdmission(Arc::clone(self)))
    }

    pub(crate) fn paused_call_ids(&self) -> std::collections::HashSet<String> {
        self.dispatch_gates
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .values()
            .filter(|gate| gate.paused_waits != 0)
            .filter_map(|gate| {
                gate.originating_call
                    .as_ref()?
                    .item_id
                    .as_ref()
                    .map(ToString::to_string)
            })
            .collect()
    }
}

impl CodeModeService {
    /// Interrupt only the observation timer when a callback enters native wait.
    /// The runtime then supplies the real pending frontier; the cell is not canceled.
    pub(super) async fn observe_until_paused<T>(
        &self,
        cell: &CellId,
        preempt: &CancellationToken,
        observation: impl std::future::Future<Output = Result<T, String>>,
    ) -> Result<T, String> {
        tokio::pin!(observation);
        loop {
            let changed = self.dispatch_broker.paused_changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            if self.dispatch_broker.cell_paused(cell) {
                preempt.cancel();
            }
            tokio::select! {
                biased;
                result = &mut observation => return result,
                _ = changed => {},
            }
        }
    }

    pub(super) async fn observe_paused(
        &self,
        session: &Arc<Session>,
        step: &Arc<StepContext>,
        mut response: RuntimeResponse,
        max_output_tokens: Option<usize>,
        cancellation: &CancellationToken,
    ) -> Result<RuntimeResponse, String> {
        let mut content = Vec::new();
        let mut host_duration = std::time::Duration::ZERO;
        loop {
            host_duration += response.code_mode_host_duration().unwrap_or_default();
            let RuntimeResponse::Yielded {
                cell_id,
                content_items,
                ..
            } = &mut response
            else {
                break;
            };
            if !self.dispatch_broker.cell_paused(cell_id) {
                break;
            }
            content.append(content_items);
            // PendingFrontier is the runtime's actual quiescent boundary. It
            // keeps JavaScript parked until the next observation, including if
            // the nested result becomes ready while capacity is reacquired.
            let frontier = self
                .session()
                .await?
                .wait_to_pending(WaitToPendingRequest {
                    cell_id: cell_id.clone(),
                    max_output_tokens,
                })
                .or_cancel(cancellation)
                .await
                .map_err(|_| "native wait canceled".to_owned())??;
            match frontier {
                WaitToPendingOutcome::LiveCell(ExecuteToPendingOutcome::Pending {
                    content_items,
                    ..
                }) => content.extend(content_items),
                WaitToPendingOutcome::LiveCell(ExecuteToPendingOutcome::Completed(completed))
                | WaitToPendingOutcome::MissingCell(completed) => {
                    response = completed;
                    continue;
                }
            }
            // Do not poll observation timeouts while the original nested future
            // is parked. Its actual outcome/cancellation releases this lease.
            session
                .wait_for_paused_cells(step, Some(cell_id), cancellation)
                .await
                .map_err(|error| error.to_string())?;
            let runtime = self.session().await?;
            let preempt = step
                .preempt
                .as_ref()
                .map(CancellationToken::child_token)
                .unwrap_or_default();
            response = self
                .observe_until_paused(
                    cell_id,
                    &preempt,
                    runtime.wait(
                        WaitRequest {
                            cell_id: cell_id.clone(),
                            yield_time_ms: super::DEFAULT_WAIT_YIELD_TIME_MS,
                            max_output_tokens,
                        },
                        Some(preempt.clone()),
                    ),
                )
                .await?
                .into();
        }
        if !content.is_empty() {
            let final_content = match &mut response {
                RuntimeResponse::Yielded { content_items, .. }
                | RuntimeResponse::Terminated { content_items, .. }
                | RuntimeResponse::Result { content_items, .. } => content_items,
            };
            content.append(final_content);
            if self.output_truncation() == codex_code_mode::CodeModeOutputTruncation::Server {
                let content =
                    super::response_adapter::into_function_call_output_content_items(content);
                let content = super::truncate_code_mode_result(content, max_output_tokens);
                *final_content = content
                    .into_iter()
                    .map(|item| {
                        match item {
                        codex_protocol::models::FunctionCallOutputContentItem::InputText {
                            text,
                        } => codex_code_mode::FunctionCallOutputContentItem::InputText { text },
                        codex_protocol::models::FunctionCallOutputContentItem::InputImage {
                            image,
                            detail,
                        } => {
                            let codex_protocol::models::ImageReference::Inline { image_url } =
                                image
                            else {
                                unreachable!("runtime response adapter only creates inline media")
                            };
                            codex_code_mode::FunctionCallOutputContentItem::InputImage {
                                image_url,
                                detail,
                            }
                        }
                        codex_protocol::models::FunctionCallOutputContentItem::InputAudio {
                            audio_url,
                        } => {
                            codex_code_mode::FunctionCallOutputContentItem::InputAudio { audio_url }
                        }
                        codex_protocol::models::FunctionCallOutputContentItem::EncryptedContent {
                            ..
                        } => unreachable!("runtime response adapter only creates visible content"),
                    }
                    })
                    .collect();
            } else {
                *final_content = content;
            }
        }
        Ok(response.with_code_mode_host_duration(host_duration))
    }

    pub(super) async fn observe_paused_wait(
        &self,
        session: &Arc<Session>,
        step: &Arc<StepContext>,
        outcome: WaitOutcome,
        max_output_tokens: Option<usize>,
        cancellation: &CancellationToken,
    ) -> Result<WaitOutcome, String> {
        match outcome {
            WaitOutcome::LiveCell(response) => self
                .observe_paused(session, step, response, max_output_tokens, cancellation)
                .await
                .map(WaitOutcome::LiveCell),
            missing => Ok(missing),
        }
    }
}

impl Session {
    pub(crate) async fn has_pending_turn_wait(&self) -> CodexResult<bool> {
        if self.multi_agent_version() != Some(codex_protocol::protocol::MultiAgentVersion::V2) {
            return Ok(false);
        }
        let turn = self
            .active_turn
            .lock()
            .await
            .as_ref()
            .and_then(|active| active.task.as_ref())
            .map(|task| task.turn_context.sub_id.clone());
        let Some(turn) = turn else {
            return Ok(false);
        };
        Ok(self
            .services
            .local_agent_runtime
            .work_ledger()?
            .current_wait(self.thread_id)
            .await
            .map_err(crate::agent::control::work_lifecycle::work_error)?
            .is_some_and(|wait| {
                matches!(wait.origin, WaitOrigin::Turn { turn_id, .. } if turn_id == turn)
                    && matches!(wait.phase, WaitPhase::Registered | WaitPhase::Claimed)
            }))
    }

    async fn service_paused_reset(
        self: &Arc<Self>,
        step: &Arc<StepContext>,
        cancellation: &CancellationToken,
    ) -> CodexResult<bool> {
        if !(self.has_pending_manual_reset().await || self.has_new_context_window_request().await) {
            return Ok(false);
        }
        let Some(_admission) = self
            .services
            .code_mode_service
            .dispatch_broker
            .admit_reset()
        else {
            return Ok(false);
        };
        let result = crate::compact_token_budget::run_inline_compact_task(
            Arc::clone(self),
            Arc::clone(step),
            codex_analytics::CompactionTrigger::Manual,
            false,
            cancellation,
        )
        .await;
        if cancellation.is_cancelled() {
            return Err(CodexErr::TurnAborted);
        }
        if let Err(error) = result {
            self.send_event(
                &step.turn,
                codex_protocol::protocol::EventMsg::Warning(
                    codex_protocol::protocol::WarningEvent {
                        message: format!("Context reset failed: {error}"),
                    },
                ),
            )
            .await;
        }
        Ok(true)
    }

    /// Provider recovery has no JavaScript cell, but retains the same native
    /// wait future and registered task while servicing auxiliary resets.
    pub(crate) async fn await_turn_wait(
        self: &Arc<Self>,
        step: &Arc<StepContext>,
        ticket: &WaitTicket,
        cancellation: &CancellationToken,
    ) -> CodexResult<WaitTicket> {
        let runtime = &self.services.local_agent_runtime;
        let waiting = runtime.await_wait(ticket, cancellation);
        tokio::pin!(waiting);
        let broker = &self.services.code_mode_service.dispatch_broker;
        let mut paused_execution = None;
        loop {
            let changed = broker.paused_changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            if cancellation.is_cancelled() {
                return waiting.await;
            }
            let cells_finished = broker
                .dispatch_gates
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .values()
                .all(|gate| gate.closed);
            if paused_execution.is_none() && cells_finished {
                paused_execution = Some(self.pause_turn_execution(&step.turn).await?);
            }
            let reset = self.service_paused_reset(step, cancellation).await;
            if cancellation.is_cancelled() {
                return waiting.await;
            }
            if reset? {
                continue;
            }
            tokio::select! {
                biased;
                result = &mut waiting => {
                    let ready = match result {
                        Ok(ready) => ready,
                        Err(error) => {
                            let retained = runtime.work_ledger()?
                                .observe_wait(ticket.caller, ticket.id)
                                .await
                                .map_err(crate::agent::control::work_lifecycle::work_error)?;
                            match retained {
                                Some(retained) if !cancellation.is_cancelled()
                                    && matches!(retained.cancellation,
                                        Some(codex_state::coordination_wait::WaitCancellation::ShellOutput { .. }
                                            | codex_state::coordination_wait::WaitCancellation::InputSuperseded { .. })) => retained,
                                _ => return Err(error),
                            }
                        }
                    };
                    if let Some(execution) = paused_execution {
                        execution.resume().await?;
                    }
                    return Ok(ready);
                }
                _ = changed => {},
            }
        }
    }

    /// Manual resets are serviced by this original pending execution, including a
    /// cell that first yielded before entering its native wait. No task is replaced.
    pub(crate) async fn wait_for_paused_cells(
        self: &Arc<Self>,
        step: &Arc<StepContext>,
        cell: Option<&CellId>,
        cancellation: &CancellationToken,
    ) -> CodexResult<()> {
        let broker = &self.services.code_mode_service.dispatch_broker;
        let mut paused_execution = None;
        loop {
            let changed = broker.paused_changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            if cancellation.is_cancelled() {
                return Err(CodexErr::TurnAborted);
            }
            if paused_execution.is_none() && cell.is_some_and(|id| broker.can_pause_execution(id)) {
                paused_execution = Some(self.pause_turn_execution(&step.turn).await?);
            }
            if self.service_paused_reset(step, cancellation).await? {
                continue;
            }
            let paused =
                cell.map_or_else(|| broker.has_paused_waits(), |id| broker.cell_paused(id));
            if !paused && !broker.reset_admitted.load(Ordering::Acquire) {
                if let Some(execution) = paused_execution {
                    execution.resume().await?;
                }
                return Ok(());
            }
            tokio::select! {
                biased;
                _ = cancellation.cancelled() => return Err(CodexErr::TurnAborted),
                _ = changed => {},
            }
        }
    }

    pub(crate) async fn reset_paused_wait(
        self: &Arc<Self>,
        step: &Arc<crate::session::step_context::StepContext>,
        reload: Option<codex_protocol::protocol::ReloadSelection>,
        operation_id: String,
        cancellation: &CancellationToken,
    ) -> CodexResult<Option<String>> {
        if let Some(selection) = reload {
            self.record_retained_context(codex_history::RetainedContextEvent::ReloadSelection {
                selection,
            })
            .await;
        }
        self.accept_self_reset_operation(operation_id).await;
        let broker = &self.services.code_mode_service.dispatch_broker;
        let admission = loop {
            let changed = broker.paused_changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            if let Some(admission) = broker.admit_reset() {
                break admission;
            }
            tokio::select! {
                biased;
                _ = cancellation.cancelled() => return Err(CodexErr::TurnAborted),
                _ = changed => {},
            }
        };
        let result = crate::compact_token_budget::run_inline_compact_task(
            Arc::clone(self),
            Arc::clone(step),
            codex_analytics::CompactionTrigger::Manual,
            true,
            cancellation,
        )
        .await;
        drop(admission);
        if cancellation.is_cancelled() {
            return Err(CodexErr::TurnAborted);
        }
        let diagnostic = result.err().map(|error| error.to_string());
        if let Some(message) = &diagnostic {
            self.send_event(
                &step.turn,
                codex_protocol::protocol::EventMsg::Warning(
                    codex_protocol::protocol::WarningEvent {
                        message: format!("Wait context reset failed: {message}"),
                    },
                ),
            )
            .await;
        }
        Ok(diagnostic)
    }
}

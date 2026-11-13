//! Native logical recovery after the request retry owner returns. Deadlines only
//! notify the existing pending-work scheduler; they never start inference here.

use super::LocalAgentRuntime;
use super::work_lifecycle::work_error;
use codex_protocol::ThreadId;
use codex_protocol::error::CodexErr;
use codex_protocol::error::CodexErrorDetails;
use codex_protocol::error::ProviderWaitKind;
use codex_protocol::error::Result as CodexResult;
use codex_state::coordination::WorkStamp;
use codex_state::coordination_wait::TurnWaitReason;
use codex_state::coordination_wait::WaitCell;
use codex_state::coordination_wait::WaitId;
use codex_state::coordination_wait::WaitOptions;
use codex_state::coordination_wait::WaitPhase;
use codex_state::coordination_wait::WaitReadiness;
use codex_state::coordination_wait::WaitTicket;
use std::collections::HashMap;
use std::sync::Mutex;
use std::sync::PoisonError;
use std::time::Duration;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct CompletionCheckpoint {
    pub(crate) work: WorkStamp,
    pub(crate) delivery_id: uuid::Uuid,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum WorkCheckpoint {
    Completion(CompletionCheckpoint),
}

pub(crate) enum CompletionSettlement {
    Pending,
    Published(Option<String>),
    Superseded,
}

impl WorkCheckpoint {
    pub(crate) fn event(&self, turn_id: String) -> codex_protocol::protocol::TurnCheckpointedEvent {
        use codex_protocol::protocol::TurnCheckpoint;
        use codex_protocol::protocol::TurnCheckpointedEvent;
        match self {
            Self::Completion(completion) => TurnCheckpointedEvent {
                turn_id,
                work_id: completion.work.work_id.to_string(),
                checkpoint: TurnCheckpoint::CompletionPending {
                    delivery_id: completion.delivery_id.to_string(),
                },
                targets: Vec::new(),
            },
        }
    }
}

#[derive(Default)]
pub(super) struct WaitDeadlines {
    // One active wait per actor. This contains live clock handles, not a second
    // topology, selection, work state, event queue or scheduling policy.
    entries: Mutex<HashMap<ThreadId, LiveDeadline>>,
}

struct LiveDeadline {
    id: WaitId,
    work: WorkStamp,
    deadline: Option<Instant>,
    next_progress: Option<Instant>,
    interval: Option<Duration>,
    cancellation: CancellationToken,
    armed: bool,
}

#[derive(Clone, Copy)]
struct ClockSample {
    unix_ms: i64,
    monotonic: Instant,
}

impl ClockSample {
    fn now() -> Self {
        Self {
            unix_ms: chrono::Utc::now().timestamp_millis(),
            monotonic: Instant::now(),
        }
    }

    fn at_unix(self, unix_ms: i64) -> CodexResult<Instant> {
        let millis = unix_ms.saturating_sub(self.unix_ms).max(0) as u64;
        self.monotonic
            .checked_add(Duration::from_millis(millis))
            .ok_or_else(|| work_error("wait deadline exceeds the monotonic clock range"))
    }

    fn unix_at(self, instant: Instant) -> CodexResult<i64> {
        let millis = if instant >= self.monotonic {
            i64::try_from(instant.duration_since(self.monotonic).as_millis()).map_err(work_error)?
        } else {
            -i64::try_from(self.monotonic.duration_since(instant).as_millis())
                .map_err(work_error)?
        };
        self.unix_ms
            .checked_add(millis)
            .ok_or_else(|| work_error("provider deadline exceeds the wall-clock range"))
    }
}

/// Do not infer recoverability from display prose or general transport retryability.
pub(crate) fn exhausted_reason(error: &CodexErr) -> Option<TurnWaitReason> {
    match error.provider_wait_kind() {
        Some(ProviderWaitKind::Capacity) => return Some(TurnWaitReason::ProviderCapacity),
        Some(ProviderWaitKind::UsageLimit) => return Some(TurnWaitReason::ProviderUsageLimit),
        None => {}
    }
    if matches!(
        error.details(),
        CodexErrorDetails::UsageLimitReached(_)
            | CodexErrorDetails::QuotaExceeded
            | CodexErrorDetails::UsageNotIncluded
    ) {
        return Some(TurnWaitReason::ProviderUsageLimit);
    }
    if error
        .http_status_code_value()
        .is_some_and(|status| (400..500).contains(&status) && status != 408)
    {
        return None;
    }
    matches!(
        error.details(),
        CodexErrorDetails::InternalServerError
            | CodexErrorDetails::InternalAgentDied
            | CodexErrorDetails::ServerOverloaded
            | CodexErrorDetails::Stream(_)
            | CodexErrorDetails::ResponseStreamFailed(_)
            | CodexErrorDetails::RetryLimit(_)
    )
    .then_some(TurnWaitReason::DescendantRecovery)
}

impl LocalAgentRuntime {
    /// A committed pause settles selected ancestor waits without terminal mail.
    pub(crate) async fn notify_work_paused(&self, work: WorkStamp) -> CodexResult<()> {
        let ledger = self.work_ledger()?;
        if ledger
            .observe_work(work)
            .await
            .map_err(work_error)?
            .is_none_or(|record| record.disposition != super::work::WorkDisposition::Paused)
        {
            return Ok(());
        }
        self.finish_ready_completions(work).await?;
        for ancestor in ledger.ancestors(work.actor_id) {
            self.notify_coordination(ancestor).await;
        }
        Ok(())
    }

    pub(crate) async fn register_native_wait(
        &self,
        caller: WorkStamp,
        cell: &WaitCell,
        options: &WaitOptions,
    ) -> anyhow::Result<WaitTicket> {
        let clock = ClockSample::now();
        let ticket = self
            .work_ledger()?
            .register_wait_at(caller, cell, options, clock.unix_ms)
            .await?;
        self.remember_deadline(&ticket, clock, None)?;
        Ok(ticket)
    }

    /// Suspend this invocation over its fixed selection. Registry invalidations
    /// and captured deadlines wake the same future, never a replacement turn.
    pub(crate) async fn await_wait(
        &self,
        ticket: &WaitTicket,
        cancellation: &CancellationToken,
    ) -> CodexResult<WaitTicket> {
        let mut changes = self.registry.subscribe();
        self.arm_wait_deadline(ticket)?;
        let ledger = self.work_ledger()?;
        loop {
            changes.borrow_and_update();
            if cancellation.is_cancelled() || self.shutdown.is_cancelled() {
                self.cancel_native_wait(ticket).await?;
                return Err(CodexErr::TurnAborted);
            }
            let current = ledger
                .observe_wait(ticket.caller, ticket.id)
                .await
                .map_err(work_error)?
                .ok_or_else(|| work_error("wait disappeared"))?;
            if matches!(current.phase, WaitPhase::Cancelled | WaitPhase::Consumed) {
                self.retire_wait_deadline(ticket.caller.actor_id, ticket.id);
                return Err(CodexErr::TurnAborted);
            }
            if let Some(claim) = ledger
                .claim_ready_wait_at(ticket.caller, ticket.id, self.wait_readiness(&current))
                .await
                .map_err(work_error)?
            {
                let mut ready = current;
                ready.phase = WaitPhase::Claimed;
                ready.claim = Some(claim);
                self.stop_wait_collection(ticket.caller.actor_id, ticket.id);
                return Ok(ready);
            }
            tokio::select! {
                biased;
                _ = cancellation.cancelled() => {},
                _ = self.shutdown.cancelled() => {},
                changed = changes.changed() => { changed.map_err(work_error)?; },
            }
        }
    }

    pub(crate) async fn recover_exhausted_turn(
        &self,
        actor: ThreadId,
        turn_id: &str,
        error: &CodexErr,
        exhausted_retry_at: Option<Instant>,
    ) -> CodexResult<Option<WaitTicket>> {
        if self.shutdown.is_cancelled() {
            return Ok(None);
        }
        let Some(reason) = exhausted_reason(error) else {
            return Ok(None);
        };
        let Some(ledger) = self.work.as_deref() else {
            return Ok(None);
        };
        let Some(work) = ledger
            .work_for_turn(actor, turn_id)
            .await
            .map_err(work_error)?
        else {
            return Ok(None);
        };
        let clock = ClockSample::now();
        let retry_at = error
            .retry_after()
            .map(|retry| retry.deadline())
            .or(exhausted_retry_at);
        let (deadline, monotonic) = match (reason, error.details()) {
            (TurnWaitReason::ProviderUsageLimit, CodexErrorDetails::UsageLimitReached(limit))
                if limit.resets_at.is_some() =>
            {
                let deadline = limit.resets_at.as_ref().map(|at| at.timestamp_millis());
                (deadline, deadline.map(|at| clock.at_unix(at)).transpose()?)
            }
            (reason, _) if reason.is_provider() => {
                (retry_at.map(|at| clock.unix_at(at)).transpose()?, retry_at)
            }
            _ => (None, None),
        };
        let Some(ticket) = ledger
            .commit_turn_wait(work, turn_id, reason, deadline, clock.unix_ms)
            .await
            .map_err(work_error)?
        else {
            return Ok(None);
        };
        self.remember_deadline(&ticket, clock, monotonic)?;
        self.arm_wait_deadline(&ticket)?;
        if reason == TurnWaitReason::ProviderUsageLimit && ticket.deadline_unix_ms.is_none() {
            self.notify_work_paused(work).await?;
        }
        Ok(Some(ticket))
    }

    pub(crate) async fn provider_recovered(
        &self,
        actor: ThreadId,
        turn_id: &str,
    ) -> CodexResult<()> {
        let Some(ledger) = self.work.as_deref() else {
            return Ok(());
        };
        if let Some(work) = ledger
            .work_for_turn(actor, turn_id)
            .await
            .map_err(work_error)?
        {
            ledger
                .provider_recovered(work, turn_id)
                .await
                .map_err(work_error)?;
        }
        Ok(())
    }

    pub(crate) async fn cancel_native_wait(&self, ticket: &WaitTicket) -> CodexResult<()> {
        self.work_ledger()?
            .cancel_wait_origin(ticket.caller, ticket.id, &ticket.origin)
            .await
            .map_err(work_error)?;
        self.emit_cancelled_wait(ticket).await;
        self.retire_wait_deadline(ticket.caller.actor_id, ticket.id);
        Ok(())
    }

    pub(crate) async fn cancel_native_wait_with_cause(
        &self,
        ticket: &WaitTicket,
        cause: codex_state::coordination_wait::WaitCancellation,
    ) -> CodexResult<()> {
        self.work_ledger()?
            .cancel_wait_with_cause(ticket, &cause)
            .await
            .map_err(work_error)?;
        self.emit_cancelled_wait(ticket).await;
        self.retire_wait_deadline(ticket.caller.actor_id, ticket.id);
        Ok(())
    }

    fn remember_deadline(
        &self,
        ticket: &WaitTicket,
        clock: ClockSample,
        exact: Option<Instant>,
    ) -> CodexResult<()> {
        if ticket.runtime_id != self.tree_id()
            || matches!(ticket.phase, WaitPhase::Consumed | WaitPhase::Cancelled)
        {
            return Ok(());
        }
        let deadline = match exact {
            Some(at) => Some(at),
            None => ticket
                .deadline_unix_ms
                .map(|value| clock.at_unix(value))
                .transpose()?,
        };
        let interval = ticket
            .options
            .progress_interval_ms
            .map(Duration::from_millis);
        let next_progress = interval
            .map(|interval| {
                clock
                    .monotonic
                    .checked_add(interval)
                    .ok_or_else(|| work_error("progress interval exceeds monotonic clock range"))
            })
            .transpose()?;
        if deadline.is_none() && next_progress.is_none() {
            return Ok(());
        }
        let mut entries = self
            .wait_deadlines
            .entries
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if entries
            .get(&ticket.caller.actor_id)
            .is_some_and(|entry| entry.id == ticket.id)
        {
            return Ok(());
        }
        if let Some(previous) = entries.insert(
            ticket.caller.actor_id,
            LiveDeadline {
                id: ticket.id,
                work: ticket.caller,
                deadline,
                next_progress,
                interval,
                cancellation: self.shutdown.child_token(),
                armed: false,
            },
        ) {
            previous.cancellation.cancel();
        }
        Ok(())
    }

    /// Only a live registration can arm. Observation/reopen never populates this
    /// map, and the state owner separately fences runtime identity at every claim.
    pub(crate) fn arm_wait_deadline(&self, ticket: &WaitTicket) -> CodexResult<()> {
        if ticket.deadline_unix_ms.is_none() && ticket.options.progress_interval_ms.is_none() {
            return Ok(());
        }
        if ticket.runtime_id != self.tree_id() {
            return Err(work_error("cold wait deadline cannot be rearmed"));
        }
        if !matches!(ticket.phase, WaitPhase::Registered | WaitPhase::Claimed) {
            return Err(work_error("deadline cannot arm for a settled wait"));
        }
        let mut entries = self
            .wait_deadlines
            .entries
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let entry = entries
            .get_mut(&ticket.caller.actor_id)
            .filter(|entry| entry.id == ticket.id && entry.work == ticket.caller)
            .ok_or_else(|| work_error("wait has no live captured deadline"))?;
        if entry.armed {
            return Ok(());
        }
        let runtime = self.for_actor(ticket.caller.actor_id)?;
        let guard = runtime
            .admit_start()?
            .into_teardown_guard("wait_deadline", Some(ticket.caller.actor_id));
        entry.armed = true;
        let cancellation = entry.cancellation.clone();
        let mut deadline = entry.deadline;
        let mut progress = entry.next_progress;
        let interval = entry.interval;
        let ticket = ticket.clone();
        tokio::spawn(async move {
            loop {
                let next = match (deadline, progress) {
                    (Some(left), Some(right)) => left.min(right),
                    (Some(at), None) | (None, Some(at)) => at,
                    (None, None) => break,
                };
                tokio::select! {
                    biased;
                    _ = cancellation.cancelled() => break,
                    _ = runtime.shutdown.cancelled() => break,
                    _ = tokio::time::sleep_until(next) => {}
                }
                let now = Instant::now();
                if deadline.is_some_and(|at| now >= at) {
                    // Keep the captured deadline in the map for pre-dispatch
                    // precedence, but notify only once from this live timer.
                    deadline = None;
                    runtime.notify_coordination(ticket.caller.actor_id).await;
                }
                if progress.is_some_and(|at| now >= at) {
                    runtime.start_progress_collection(&ticket, cancellation.child_token());
                    progress = interval.and_then(|interval| now.checked_add(interval));
                }
            }
            guard.complete();
        });
        Ok(())
    }

    pub(crate) fn wait_readiness(&self, ticket: &WaitTicket) -> WaitReadiness {
        let entries = self
            .wait_deadlines
            .entries
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        WaitReadiness {
            deadline_due: ticket.runtime_id == self.tree_id()
                && entries.get(&ticket.caller.actor_id).is_some_and(|entry| {
                    entry.id == ticket.id
                        && entry.work == ticket.caller
                        && entry.armed
                        && entry.deadline.is_some_and(|at| Instant::now() >= at)
                }),
        }
    }

    pub(crate) fn progress_cancellation(
        &self,
        actor: ThreadId,
        id: WaitId,
    ) -> Option<CancellationToken> {
        self.wait_deadlines
            .entries
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(&actor)
            .filter(|entry| entry.id == id)
            .map(|entry| entry.cancellation.child_token())
    }

    /// Normal readiness cancels only wait-owned collection. Keep its captured
    /// clock fact until dispatch so deadline precedence remains checkable.
    pub(crate) fn stop_wait_collection(&self, actor: ThreadId, id: WaitId) {
        if let Some(entry) = self
            .wait_deadlines
            .entries
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(&actor)
            .filter(|entry| entry.id == id)
        {
            entry.cancellation.cancel();
        }
    }

    pub(crate) fn live_wait_deadline(&self, actor: ThreadId) -> Option<WaitId> {
        self.wait_deadlines
            .entries
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(&actor)
            .map(|entry| entry.id)
    }

    pub(crate) fn retire_wait_deadline(&self, actor: ThreadId, id: WaitId) {
        let mut entries = self
            .wait_deadlines
            .entries
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if entries.get(&actor).is_some_and(|entry| entry.id == id)
            && let Some(entry) = entries.remove(&actor)
        {
            // Do not join under task admission, or from the scheduler notification
            // this timer invoked. Tree shutdown waits for all operation memberships.
            entry.cancellation.cancel();
        }
    }

    pub(crate) fn retire_work_deadline(&self, work: WorkStamp) {
        let id = self
            .wait_deadlines
            .entries
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(&work.actor_id)
            .filter(|entry| entry.work == work)
            .map(|entry| entry.id);
        if let Some(id) = id {
            self.retire_wait_deadline(work.actor_id, id);
        }
    }
}

#[cfg(test)]
#[path = "recovery_tests.rs"]
mod tests;

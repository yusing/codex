use codex_protocol::context_reset::ContextReset;
use codex_protocol::context_reset::ContextResetStatus;
use std::sync::Arc;
use std::sync::Mutex;
use tokio_util::sync::CancellationToken;

#[derive(Default)]
pub(crate) struct ManualResets(Mutex<Requests>);

#[derive(Default)]
struct Requests {
    generation: u64,
    pending: Vec<Request>,
    active: Option<Active>,
}

struct Request {
    id: Option<String>,
    generation: u64,
    canceled: bool,
}

struct Active {
    receipt: ContextReset,
    requests: Vec<Request>,
    self_requests: Vec<String>,
    cancellation: Option<CancellationToken>,
}

// Normal publication retires this lease. A dropped reset future leaves its last
// disposition with the native turn-abort owner, including before started delivery.
pub(crate) struct ManualResetLease {
    owner: Arc<ManualResets>,
    pub(crate) generation: u64,
    pub(crate) cancellation: CancellationToken,
}

impl ManualResetLease {
    // Direct model requests share the existing abort-owned reset disposition.
    // Keep their identities until terminal publication finishes, even when a
    // forced task abort drops the reset future while send_event is suspended.
    pub(crate) fn record_self_requests(&self, receipt: &ContextReset, self_requests: &[String]) {
        if self_requests.is_empty() {
            return;
        }
        let mut requests = self
            .owner
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let active = requests.active.get_or_insert_with(|| Active {
            receipt: receipt.clone(),
            requests: Vec::new(),
            self_requests: Vec::new(),
            cancellation: None,
        });
        active.self_requests = self_requests.to_vec();
    }

    pub(crate) fn record(&self, receipt: &ContextReset) {
        let mut requests = self
            .owner
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(active) = requests.active.as_mut()
            && active.receipt.generation == self.generation
        {
            active.receipt = receipt.clone();
        }
    }

    pub(crate) fn finish(self) {
        let mut requests = self
            .owner
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if requests
            .active
            .as_ref()
            .is_some_and(|active| active.receipt.generation == self.generation)
        {
            requests.active = None;
        }
    }
}

impl ManualResets {
    pub(crate) fn reserve(&self) -> u64 {
        let mut requests = self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        requests.generation = requests.generation.saturating_add(1);
        requests.generation
    }

    pub(crate) fn publish(&self, generation: u64, id: Option<String>) {
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .pending
            .push(Request {
                id,
                generation,
                canceled: false,
            });
    }

    pub(crate) fn pending(&self) -> bool {
        !self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .pending
            .is_empty()
    }

    pub(crate) fn begin(
        self: &Arc<Self>,
        thread: &str,
        turn: &str,
        required_without_manual: bool,
    ) -> ManualResetLease {
        let mut requests = self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let pending = std::mem::take(&mut requests.pending);
        let generation = pending.last().map_or(0, |request| request.generation);
        let cancellation = CancellationToken::new();
        if generation != 0 {
            requests.active = Some(Active {
                receipt: ContextReset {
                    thread_id: thread.into(),
                    turn_id: turn.into(),
                    request_id: None,
                    generation,
                    reset_id: None,
                    status: ContextResetStatus::Started,
                    inline: true,
                    diagnostic: None,
                },
                requests: pending,
                self_requests: Vec::new(),
                cancellation: (!required_without_manual).then(|| cancellation.clone()),
            });
        }
        ManualResetLease {
            owner: Arc::clone(self),
            generation,
            cancellation,
        }
    }

    pub(crate) fn take_aborted(&self, turn: &str) -> Option<(ContextReset, Vec<String>)> {
        let mut requests = self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if requests
            .active
            .as_ref()
            .is_none_or(|active| active.receipt.turn_id != turn)
        {
            return None;
        }
        let active = requests.active.take()?;
        let mut receipt = active.receipt;
        if matches!(
            receipt.status,
            ContextResetStatus::Accepted | ContextResetStatus::Started
        ) {
            receipt.status = ContextResetStatus::Canceled;
        }
        Some((receipt, active.self_requests))
    }

    // Pending cancellation removes only this request. A running reset may also
    // satisfy another manual request, a compound checkpoint or automatic work.
    pub(crate) fn cancel(&self, id: &str) -> Option<u64> {
        let mut requests = self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(index) = requests
            .pending
            .iter()
            .position(|request| request.id.as_deref() == Some(id))
        {
            return Some(requests.pending.remove(index).generation);
        }
        if let Some(active) = &mut requests.active {
            for request in &mut active.requests {
                if request.id.as_deref() == Some(id) {
                    request.canceled = true;
                }
            }
            if active.requests.iter().all(|request| request.canceled)
                && let Some(cancellation) = &active.cancellation
            {
                cancellation.cancel();
            }
        }
        None
    }

    pub(crate) fn cancel_pending(&self) -> u64 {
        let mut requests = self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let generation = requests
            .pending
            .last()
            .map_or(0, |request| request.generation);
        requests.pending.clear();
        generation
    }
}

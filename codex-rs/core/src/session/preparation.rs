//! Recipient-owned source acquisition, without a model turn or accepted work.

use super::Session;
use super::TurnContext;
use crate::state::PendingApproval;
use codex_protocol::error::CodexErr;
use codex_protocol::error::Result as CodexResult;
use codex_protocol::protocol::Event;
use codex_protocol::protocol::EventMsg;
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::PoisonError;
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;
use tokio_util::task::task_tracker::TaskTrackerToken;

#[derive(Default)]
pub(crate) struct Preparations {
    state: Mutex<PreparationState>,
    tasks: TaskTracker,
}

#[derive(Default)]
struct PreparationState {
    closed: bool,
    operations: HashMap<String, Arc<Preparation>>,
}

struct Preparation {
    cancellation: CancellationToken,
    approvals: Mutex<HashMap<String, PendingApproval>>,
    executions: TaskTracker,
    events: async_channel::Sender<Event>,
    turn_id: String,
}

impl Preparation {
    fn stop(&self) {
        self.cancellation.cancel();
        let _approvals = std::mem::take(
            &mut *self
                .approvals
                .lock()
                .unwrap_or_else(PoisonError::into_inner),
        );
        self.executions.close();
    }

    fn resolve(&self, approval_id: String) {
        let _ = self.events.try_send(Event {
            id: self.turn_id.clone(),
            msg: EventMsg::CommandApprovalResolved { approval_id },
        });
    }
}

pub(super) struct PreparationApprovalPrompt {
    operation: Arc<Preparation>,
    approval_id: String,
}

impl Drop for PreparationApprovalPrompt {
    fn drop(&mut self) {
        self.operation
            .approvals
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(&self.approval_id);
        // Resolve after request publication has finished or been dropped, not merely when
        // cancellation was signaled: publication itself can yield.
        self.operation.resolve(self.approval_id.clone());
    }
}

pub(crate) struct PreparationGuard {
    owner: Arc<Preparations>,
    operation: Arc<Preparation>,
    lifetime: Option<TaskTrackerToken>,
}

impl PreparationGuard {
    pub(crate) fn cancellation(&self) -> &CancellationToken {
        &self.operation.cancellation
    }

    /// Known exits join exact one-shot cleanup before releasing preparation ownership.
    pub(crate) async fn finish(mut self) {
        self.operation.stop();
        self.operation.executions.wait().await;
        self.remove();
    }

    fn remove(&mut self) {
        self.owner
            .state
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .operations
            .remove(&self.operation.turn_id);
        self.lifetime.take();
    }
}

impl Drop for PreparationGuard {
    fn drop(&mut self) {
        let Some(lifetime) = self.lifetime.take() else {
            return;
        };
        self.operation.stop();
        let owner = Arc::clone(&self.owner);
        let operation = Arc::clone(&self.operation);
        // Dropping the acquisition only signals cancellation. Session shutdown joins this task.
        drop(tokio::spawn(async move {
            let _lifetime = lifetime;
            operation.executions.wait().await;
            owner
                .state
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .operations
                .remove(&operation.turn_id);
        }));
    }
}

pub(super) enum ApprovalOwner {
    Turn(Option<String>),
    Preparation(CancellationToken),
}

impl Session {
    pub(crate) fn register_preparation(
        &self,
        turn: &TurnContext,
        cancellation: &CancellationToken,
    ) -> CodexResult<PreparationGuard> {
        let mut state = self
            .preparations
            .state
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if state.closed || cancellation.is_cancelled() {
            return Err(CodexErr::TurnAborted);
        }
        if state.operations.contains_key(&turn.sub_id) {
            return Err(CodexErr::InvalidRequest(
                "source preparation context is already registered".into(),
            ));
        }
        let operation = Arc::new(Preparation {
            cancellation: cancellation.child_token(),
            approvals: Mutex::new(HashMap::new()),
            executions: TaskTracker::new(),
            events: self.tx_event.clone(),
            turn_id: turn.sub_id.clone(),
        });
        state
            .operations
            .insert(turn.sub_id.clone(), Arc::clone(&operation));
        Ok(PreparationGuard {
            owner: Arc::clone(&self.preparations),
            operation,
            lifetime: Some(self.preparations.tasks.token()),
        })
    }

    pub(crate) fn preparation_execution_token(&self, turn_id: &str) -> Option<TaskTrackerToken> {
        let state = self
            .preparations
            .state
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        state
            .operations
            .get(turn_id)
            .map(|operation| operation.executions.token())
    }

    pub(super) fn preparation_cancellation(&self, turn_id: &str) -> Option<CancellationToken> {
        self.preparations
            .state
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .operations
            .get(turn_id)
            .map(|operation| operation.cancellation.clone())
    }

    pub(super) fn insert_preparation_approval(
        &self,
        approval_id: String,
        pending: PendingApproval,
    ) -> Result<(Option<PendingApproval>, PreparationApprovalPrompt), ()> {
        let state = self
            .preparations
            .state
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let operation = state.operations.get(&pending.turn_id).ok_or(())?;
        let mut approvals = operation
            .approvals
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if operation.cancellation.is_cancelled() {
            return Err(());
        }
        let previous = approvals.insert(approval_id.clone(), pending);
        Ok((
            previous,
            PreparationApprovalPrompt {
                operation: Arc::clone(operation),
                approval_id,
            },
        ))
    }

    pub(super) fn take_preparation_approval(
        &self,
        approval_id: &str,
        expected_turn_id: &str,
    ) -> Option<(PendingApproval, ApprovalOwner)> {
        let state = self
            .preparations
            .state
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let operation = state.operations.get(expected_turn_id)?;
        let mut approvals = operation
            .approvals
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let pending = approvals.remove(approval_id)?;
        if operation.cancellation.is_cancelled() || pending.sender.is_closed() {
            return None;
        }
        Some((
            pending,
            ApprovalOwner::Preparation(operation.cancellation.clone()),
        ))
    }

    pub(super) async fn shutdown_preparations(&self) {
        {
            let mut state = self
                .preparations
                .state
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            state.closed = true;
            for operation in state.operations.values() {
                operation.stop();
            }
            self.preparations.tasks.close();
        }
        self.preparations.tasks.wait().await;
    }
}

#[cfg(test)]
#[path = "preparation_tests.rs"]
mod tests;

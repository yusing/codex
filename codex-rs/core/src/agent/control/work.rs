//! Work and delivery facts for the existing native registry, not another tree.
//! Construction borrows the actual StateRuntime. No history load, executor start,
//! timer or mailbox side effect is permitted in this owner.

use crate::agent::registry::AgentRegistry;
use codex_protocol::AgentPath;
use codex_protocol::ThreadId;
use codex_state::StateRuntime;
use std::sync::Arc;
use uuid::Uuid;

pub use codex_state::coordination::CoordinationError;
pub use codex_state::coordination::DeliveryId;
pub use codex_state::coordination::DeliveryKind;
pub use codex_state::coordination::DeliveryRecord;
pub use codex_state::coordination::PublishDelivery;
pub use codex_state::coordination::WorkDisposition;
pub use codex_state::coordination::WorkRecord;
pub use codex_state::coordination::WorkStamp;

pub(crate) struct WorkLedger {
    state: Arc<StateRuntime>,
    registry: Arc<AgentRegistry>,
    /// The existing tree shutdown/lifetime UUID, never an execution generation.
    runtime_id: Uuid,
}

impl WorkLedger {
    pub(super) fn state(&self) -> &Arc<StateRuntime> {
        &self.state
    }

    pub(super) fn new(
        state: Arc<StateRuntime>,
        registry: Arc<AgentRegistry>,
        runtime_id: Uuid,
    ) -> Self {
        Self {
            state,
            registry,
            runtime_id,
        }
    }

    pub(super) fn root(&self) -> anyhow::Result<ThreadId> {
        self.registry
            .agent_id_for_path(&AgentPath::root())
            .ok_or_else(|| anyhow::anyhow!("agent tree root is not registered"))
    }

    fn member(&self, actor: ThreadId) -> anyhow::Result<()> {
        anyhow::ensure!(
            self.registry.agent_metadata_for_thread(actor).is_some(),
            "actor is not registered in this agent tree"
        );
        Ok(())
    }

    fn stamp(&self, stamp: WorkStamp) -> anyhow::Result<()> {
        anyhow::ensure!(
            stamp.root_id == self.root()?,
            "work belongs to a different agent tree"
        );
        self.member(stamp.actor_id)
    }

    fn publication(&self, publication: &PublishDelivery) -> anyhow::Result<()> {
        anyhow::ensure!(
            publication.root_id == self.root()?,
            "delivery belongs to a different agent tree"
        );
        self.member(publication.recipient_id)?;
        // The admitting controller authenticates attribution, including an active
        // foreign-session sender. Receiving mail does not enroll that sender here.
        if let Some(stamp) = publication.work {
            self.stamp(stamp)?;
        }
        Ok(())
    }

    /// The controller supplies authenticated attribution and fully prepared input.
    /// Registry admission precedes this call; no CodexThread/history is required.
    #[cfg(test)]
    pub(crate) async fn admit_work(
        &self,
        stamp: WorkStamp,
        expected_current: Option<WorkStamp>,
        input: &PublishDelivery,
    ) -> anyhow::Result<WorkRecord> {
        self.stamp(stamp)?;
        self.publication(input)?;
        anyhow::ensure!(
            input.work == Some(stamp),
            "test admission changed work identity"
        );
        self.admit_input(input, true, expected_current).await
    }

    /// Read-only identity comparison for BTW. Observing cannot create an assignment.
    pub(crate) async fn work_stamp(&self, actor: ThreadId) -> anyhow::Result<Option<WorkStamp>> {
        Ok(self.observe_current(actor).await?.map(|work| work.stamp))
    }

    pub(crate) async fn observe_current(
        &self,
        actor: ThreadId,
    ) -> anyhow::Result<Option<WorkRecord>> {
        self.member(actor)?;
        Ok(self
            .state
            .coordination_current_work(self.root()?, actor)
            .await?
            .map(|work| work.observed_in(self.runtime_id)))
    }

    pub(crate) async fn observe_work(
        &self,
        stamp: WorkStamp,
    ) -> anyhow::Result<Option<WorkRecord>> {
        self.stamp(stamp)?;
        Ok(self
            .state
            .coordination_work(stamp)
            .await?
            .map(|work| work.observed_in(self.runtime_id)))
    }

    pub(crate) async fn transition_work(
        &self,
        stamp: WorkStamp,
        expected_revision: i64,
        disposition: WorkDisposition,
    ) -> anyhow::Result<WorkRecord> {
        self.stamp(stamp)?;
        let value = self
            .state
            .coordination_transition_work(stamp, expected_revision, self.runtime_id, disposition)
            .await?;
        self.registry.invalidate_observation();
        Ok(value)
    }

    /// Explicitly adopt retained facts into this runtime, remaining paused/uncertain.
    pub(crate) async fn reconcile_work(
        &self,
        stamp: WorkStamp,
        expected_revision: i64,
    ) -> anyhow::Result<WorkRecord> {
        self.stamp(stamp)?;
        let value = self
            .state
            .coordination_reconcile_work(stamp, expected_revision, self.runtime_id)
            .await?;
        self.registry.invalidate_observation();
        Ok(value)
    }

    pub(crate) async fn publish_delivery(
        &self,
        publication: &PublishDelivery,
    ) -> anyhow::Result<DeliveryRecord> {
        self.publication(publication)?;
        let value = self
            .state
            .coordination_publish_delivery(publication)
            .await?;
        self.registry.invalidate_observation();
        Ok(value)
    }

    pub(crate) async fn observe_delivery(
        &self,
        recipient: ThreadId,
        id: DeliveryId,
    ) -> anyhow::Result<Option<DeliveryRecord>> {
        self.member(recipient)?;
        self.state
            .coordination_delivery(self.root()?, recipient, id)
            .await
    }

    pub(crate) async fn observe_repeated_delivery(
        &self,
        recipient: ThreadId,
        id: DeliveryId,
    ) -> anyhow::Result<Option<DeliveryRecord>> {
        self.member(recipient)?;
        self.state
            .coordination_repeated_delivery(self.root()?, recipient, id)
            .await
    }

    #[cfg(test)]
    pub(crate) async fn unread_deliveries(
        &self,
        recipient: ThreadId,
        after_sequence: i64,
        limit: u32,
    ) -> anyhow::Result<Vec<DeliveryRecord>> {
        self.member(recipient)?;
        self.state
            .coordination_unread_deliveries(self.root()?, recipient, after_sequence, limit)
            .await
    }

    /// True is a newly fenced dispatch. False is an exact retained receipt and
    /// never authorizes replay. Call before I/O; losing its response is uncertain.
    #[cfg(test)]
    pub(crate) async fn begin_delivery(
        &self,
        recipient: ThreadId,
        ids: &[DeliveryId],
        claim_id: Option<Uuid>,
        dispatch_id: Uuid,
    ) -> anyhow::Result<bool> {
        self.member(recipient)?;
        let value = self
            .state
            .coordination_begin_delivery(self.root()?, recipient, ids, claim_id, dispatch_id)
            .await?;
        if value {
            self.registry.invalidate_observation();
        }
        Ok(value)
    }

    pub(crate) async fn confirm_delivery(
        &self,
        recipient: ThreadId,
        ids: &[DeliveryId],
        dispatch_id: Uuid,
        receipt: &str,
    ) -> anyhow::Result<()> {
        self.member(recipient)?;
        let value = self
            .state
            .coordination_confirm_delivery(self.root()?, recipient, ids, dispatch_id, receipt)
            .await?;
        self.registry.invalidate_observation();
        Ok(value)
    }

    #[cfg(test)]
    pub(crate) async fn consume_deliveries(
        &self,
        recipient: ThreadId,
        ids: &[DeliveryId],
        receipt: &str,
    ) -> anyhow::Result<()> {
        self.member(recipient)?;
        let value = self
            .state
            .coordination_consume_deliveries(self.root()?, recipient, ids, receipt)
            .await?;
        self.registry.invalidate_observation();
        Ok(value)
    }
}

#[path = "work_wait.rs"]
pub(crate) mod work_wait;

#[path = "work_observation.rs"]
mod observation;

#[cfg(test)]
#[path = "work_tests.rs"]
mod tests;

impl WorkLedger {
    pub(crate) async fn bind_turn(&self, stamp: WorkStamp, turn_id: &str) -> anyhow::Result<()> {
        self.stamp(stamp)?;
        let value = self
            .state
            .coordination_bind_turn(stamp, self.runtime_id, turn_id)
            .await?;
        self.registry.invalidate_observation();
        Ok(value)
    }

    pub(crate) async fn work_for_turn(
        &self,
        actor: ThreadId,
        turn_id: &str,
    ) -> anyhow::Result<Option<WorkStamp>> {
        self.member(actor)?;
        self.state
            .coordination_work_for_turn(self.root()?, actor, turn_id)
            .await
    }

    pub(crate) async fn terminal_delivery(
        &self,
        stamp: WorkStamp,
    ) -> anyhow::Result<Option<DeliveryRecord>> {
        self.stamp(stamp)?;
        self.state.coordination_terminal_delivery(stamp).await
    }

    pub(crate) async fn has_pending_messages(&self, actor: ThreadId) -> anyhow::Result<bool> {
        self.member(actor)?;
        self.state
            .coordination_has_pending_messages(self.root()?, actor)
            .await
    }

    pub(crate) async fn take_messages(
        &self,
        actor: ThreadId,
    ) -> anyhow::Result<Vec<DeliveryRecord>> {
        self.member(actor)?;
        let value = self
            .state
            .coordination_take_messages(self.root()?, actor)
            .await?;
        if !value.is_empty() {
            self.registry.invalidate_observation();
        }
        Ok(value)
    }

    pub(crate) async fn consume_sampling(
        &self,
        actor: ThreadId,
        ids: &[Uuid],
    ) -> anyhow::Result<Vec<DeliveryRecord>> {
        self.member(actor)?;
        let value = self
            .state
            .coordination_consume_sampling(self.root()?, actor, ids)
            .await?;
        if !value.is_empty() {
            self.registry.invalidate_observation();
        }
        Ok(value)
    }

    pub(super) fn ancestors(&self, actor: ThreadId) -> Vec<ThreadId> {
        let Some(path) = self
            .registry
            .agent_metadata_for_thread(actor)
            .and_then(|metadata| metadata.agent_path)
        else {
            return Vec::new();
        };
        let mut path = path.as_str();
        let mut result = Vec::new();
        while let Some((parent, _)) = path.rsplit_once('/') {
            if let Ok(parent_path) = AgentPath::try_from(parent)
                && let Some(id) = self.registry.agent_id_for_path(&parent_path)
            {
                result.push(id);
            }
            path = parent;
        }
        result
    }

    pub(super) async fn admit_input(
        &self,
        input: &PublishDelivery,
        starts_work: bool,
        current: Option<WorkStamp>,
    ) -> anyhow::Result<WorkRecord> {
        self.publication(input)?;
        let wait = self.current_wait(input.recipient_id).await?;
        let superseded = if starts_work {
            self.observe_current(input.recipient_id)
                .await?
                .filter(|work| !work.disposition.is_terminal())
                .map(|work| -> anyhow::Result<PublishDelivery> {
                    Ok(PublishDelivery {
                        id: Uuid::now_v7(),
                        root_id: work.stamp.root_id,
                        sender_id: Some(input.recipient_id),
                        recipient_id: self
                            .ancestors(input.recipient_id)
                            .first()
                            .copied()
                            .unwrap_or(input.recipient_id),
                        work: Some(work.stamp),
                        kind: DeliveryKind::Terminal,
                        payload: serde_json::to_string(
                            &super::work_lifecycle::CoordinationPayload::Terminal {
                                status: codex_protocol::protocol::AgentStatus::Interrupted,
                                answers: Vec::new(),
                                capture: None,
                            },
                        )?,
                    })
                })
                .transpose()?
        } else {
            None
        };
        let value = self
            .state
            .coordination_admit_input(
                input,
                self.runtime_id,
                starts_work,
                current,
                wait.as_ref(),
                &self.ancestors(input.recipient_id),
                superseded.as_ref(),
            )
            .await?;
        self.registry.invalidate_observation();
        Ok(value)
    }

    pub(crate) async fn record_answer(
        &self,
        stamp: WorkStamp,
        item_id: &str,
        content: &str,
    ) -> anyhow::Result<()> {
        self.stamp(stamp)?;
        let value = self
            .state
            .coordination_record_answer(stamp, self.runtime_id, item_id, content)
            .await?;
        self.registry.invalidate_observation();
        Ok(value)
    }

    pub(super) async fn stage_completion(
        &self,
        stamp: WorkStamp,
        turn_id: &str,
        recipient: ThreadId,
        disposition: WorkDisposition,
        detail: Option<&str>,
    ) -> anyhow::Result<Option<codex_state::coordination::CompletionCandidate>> {
        self.stamp(stamp)?;
        self.member(recipient)?;
        let path = self
            .registry
            .agent_metadata_for_thread(stamp.actor_id)
            .and_then(|metadata| metadata.agent_path)
            .ok_or_else(|| anyhow::anyhow!("completion actor has no registered path"))?;
        let prefix = format!("{path}/");
        let descendants: Vec<_> = self
            .registry
            .live_agents()
            .into_iter()
            .filter(|metadata| {
                metadata
                    .agent_path
                    .as_ref()
                    .is_some_and(|path| path.as_str().starts_with(&prefix))
            })
            .filter_map(|metadata| metadata.agent_id)
            .collect();
        let value = self
            .state
            .coordination_stage_completion(
                stamp,
                self.runtime_id,
                turn_id,
                recipient,
                disposition,
                detail,
                &descendants,
            )
            .await?;
        self.registry.invalidate_observation();
        Ok(value)
    }

    pub(super) async fn pending_completion(
        &self,
        stamp: WorkStamp,
    ) -> anyhow::Result<Option<codex_state::coordination::CompletionCandidate>> {
        self.stamp(stamp)?;
        self.state.coordination_pending_completion(stamp).await
    }

    pub(crate) async fn completion_blockers(
        &self,
        checkpoint: &super::recovery::CompletionCheckpoint,
    ) -> anyhow::Result<Option<Vec<WorkStamp>>> {
        self.stamp(checkpoint.work)?;
        self.state
            .coordination_completion_blockers(
                checkpoint.work,
                self.runtime_id,
                checkpoint.delivery_id,
            )
            .await
    }

    pub(crate) async fn begin_completion_continuation(
        &self,
        checkpoint: &super::recovery::CompletionCheckpoint,
        blockers: &[WorkStamp],
        publication: &PublishDelivery,
        receipt: Option<&str>,
    ) -> anyhow::Result<bool> {
        self.stamp(checkpoint.work)?;
        self.publication(publication)?;
        let admitted = self
            .state
            .coordination_begin_completion_continuation(
                checkpoint.work,
                self.runtime_id,
                checkpoint.delivery_id,
                blockers,
                publication,
                receipt,
            )
            .await?;
        if admitted {
            self.registry.invalidate_observation();
        }
        Ok(admitted)
    }

    pub(super) async fn complete_capture(
        &self,
        stamp: WorkStamp,
        id: Uuid,
        capture: Option<&str>,
    ) -> anyhow::Result<()> {
        self.stamp(stamp)?;
        let value = self
            .state
            .coordination_complete_capture(stamp, id, capture)
            .await?;
        self.registry.invalidate_observation();
        Ok(value)
    }

    pub(crate) async fn completion_settlement(
        &self,
        stamp: WorkStamp,
        id: Uuid,
        settled: bool,
    ) -> anyhow::Result<bool> {
        self.stamp(stamp)?;
        let retained = self
            .state
            .coordination_completion_settlement(stamp, self.runtime_id, id, settled)
            .await?;
        if retained {
            self.registry.invalidate_observation();
        }
        Ok(retained)
    }

    pub(super) async fn capture_failed(
        &self,
        stamp: WorkStamp,
        id: Uuid,
        diagnostic: &str,
    ) -> anyhow::Result<()> {
        self.stamp(stamp)?;
        self.state
            .coordination_capture_failed(stamp, id, diagnostic)
            .await?;
        self.registry.invalidate_observation();
        Ok(())
    }

    pub(super) async fn finish_completion(
        &self,
        stamp: WorkStamp,
        id: Uuid,
        payload: &str,
    ) -> anyhow::Result<Option<WorkRecord>> {
        self.stamp(stamp)?;
        let value = self
            .state
            .coordination_finish_completion(stamp, self.runtime_id, id, payload)
            .await?;
        if value.is_some() {
            self.registry.invalidate_observation();
        }
        Ok(value)
    }

    pub(super) async fn completion_dependents(
        &self,
        stamp: WorkStamp,
    ) -> anyhow::Result<Vec<WorkStamp>> {
        self.stamp(stamp)?;
        self.state.coordination_completion_dependents(stamp).await
    }
}

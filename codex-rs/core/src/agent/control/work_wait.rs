//! Selection over the existing native registry and work ledger. This module
//! neither owns a topology nor starts execution or interprets cell output.

use super::WorkLedger;
use super::WorkStamp;
use codex_protocol::ThreadId;
use uuid::Uuid;

pub(crate) use codex_state::coordination_wait::TurnWaitReason;
pub(crate) use codex_state::coordination_wait::WaitCell;
pub(crate) use codex_state::coordination_wait::WaitClaim;
pub(crate) use codex_state::coordination_wait::WaitId;
#[cfg(test)]
pub(crate) use codex_state::coordination_wait::WaitMode;
pub(crate) use codex_state::coordination_wait::WaitOptions;
pub(crate) use codex_state::coordination_wait::WaitOrigin;
pub(crate) use codex_state::coordination_wait::WaitReadiness;
pub(crate) use codex_state::coordination_wait::WaitTicket;

impl WorkLedger {
    pub(crate) async fn begin_wait_reset_origin(
        &self,
        ticket: &WaitTicket,
    ) -> anyhow::Result<bool> {
        self.stamp(ticket.caller)?;
        self.state
            .coordination_begin_wait_reset_origin(
                ticket.caller,
                self.runtime_id,
                ticket.id,
                &ticket.origin,
            )
            .await
    }

    pub(crate) async fn finish_wait_reset_origin(
        &self,
        ticket: &WaitTicket,
        diagnostic: Option<&str>,
    ) -> anyhow::Result<WaitTicket> {
        self.stamp(ticket.caller)?;
        let ticket = self
            .state
            .coordination_finish_wait_reset_origin(
                ticket.caller,
                self.runtime_id,
                ticket.id,
                &ticket.origin,
                diagnostic,
            )
            .await?;
        self.registry.invalidate_observation();
        Ok(ticket)
    }

    #[cfg(test)]
    pub(crate) async fn register_wait(
        &self,
        caller: WorkStamp,
        cell: &WaitCell,
        mode: WaitMode,
    ) -> anyhow::Result<WaitTicket> {
        self.register_wait_with_options(
            caller,
            cell,
            &WaitOptions {
                mode,
                ..Default::default()
            },
        )
        .await
    }

    #[cfg(test)]
    pub(crate) async fn register_wait_with_options(
        &self,
        caller: WorkStamp,
        cell: &WaitCell,
        options: &WaitOptions,
    ) -> anyhow::Result<WaitTicket> {
        self.register_wait_at(caller, cell, options, chrono::Utc::now().timestamp_millis())
            .await
    }

    pub(crate) async fn register_wait_at(
        &self,
        caller: WorkStamp,
        cell: &WaitCell,
        options: &WaitOptions,
        now_unix_ms: i64,
    ) -> anyhow::Result<WaitTicket> {
        self.stamp(caller)?;
        let descendants = self.wait_descendants(caller)?;
        let ticket = self
            .state
            .coordination_register_wait_at(
                caller,
                self.runtime_id,
                cell,
                options,
                &descendants,
                now_unix_ms,
            )
            .await?;
        self.registry.invalidate_observation();
        Ok(ticket)
    }

    fn wait_descendants(&self, caller: WorkStamp) -> anyhow::Result<Vec<ThreadId>> {
        let path = self
            .registry
            .agent_metadata_for_thread(caller.actor_id)
            .and_then(|metadata| metadata.agent_path)
            .ok_or_else(|| anyhow::anyhow!("wait caller has no registered logical path"))?;
        let prefix = format!("{path}/");
        let mut descendants: Vec<_> = self
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
        descendants.sort_by_key(ToString::to_string);
        Ok(descendants)
    }

    #[cfg(test)]
    pub(crate) async fn claim_ready_wait(
        &self,
        caller: WorkStamp,
        id: WaitId,
    ) -> anyhow::Result<Option<WaitClaim>> {
        self.claim_ready_wait_at(caller, id, WaitReadiness::default())
            .await
    }

    pub(crate) async fn claim_ready_wait_at(
        &self,
        caller: WorkStamp,
        id: WaitId,
        readiness: WaitReadiness,
    ) -> anyhow::Result<Option<WaitClaim>> {
        self.stamp(caller)?;
        let claim = self
            .state
            .coordination_claim_ready_wait_at(caller, self.runtime_id, id, readiness)
            .await?;
        if claim.is_some() {
            self.registry.invalidate_observation();
        }
        Ok(claim)
    }

    pub(crate) async fn freeze_wait_metrics(
        &self,
        caller: WorkStamp,
        id: WaitId,
        claim_id: Uuid,
        metrics: &std::collections::BTreeMap<String, codex_protocol::agent_metrics::AgentMetrics>,
    ) -> anyhow::Result<
        std::collections::BTreeMap<String, codex_protocol::agent_metrics::AgentMetrics>,
    > {
        self.stamp(caller)?;
        self.state
            .coordination_freeze_wait_metrics(caller, self.runtime_id, id, claim_id, metrics)
            .await
    }

    pub(crate) async fn cancel_wait_with_cause(
        &self,
        ticket: &WaitTicket,
        cause: &codex_state::coordination_wait::WaitCancellation,
    ) -> anyhow::Result<()> {
        self.stamp(ticket.caller)?;
        self.state
            .coordination_cancel_wait_with_cause(
                ticket.caller,
                self.runtime_id,
                ticket.id,
                &ticket.origin,
                cause,
            )
            .await?;
        self.registry.invalidate_observation();
        Ok(())
    }

    #[cfg(test)]
    pub(crate) async fn cancel_wait(
        &self,
        caller: WorkStamp,
        id: WaitId,
        cell: &WaitCell,
    ) -> anyhow::Result<()> {
        self.stamp(caller)?;
        self.state
            .coordination_cancel_wait(caller, self.runtime_id, id, cell)
            .await?;
        self.registry.invalidate_observation();
        Ok(())
    }

    pub(crate) async fn cancel_wait_origin(
        &self,
        caller: WorkStamp,
        id: WaitId,
        origin: &WaitOrigin,
    ) -> anyhow::Result<()> {
        self.stamp(caller)?;
        self.state
            .coordination_cancel_wait_origin(caller, self.runtime_id, id, origin)
            .await?;
        self.registry.invalidate_observation();
        Ok(())
    }

    pub(crate) async fn commit_turn_wait(
        &self,
        caller: WorkStamp,
        turn_id: &str,
        reason: TurnWaitReason,
        deadline_unix_ms: Option<i64>,
        now_unix_ms: i64,
    ) -> anyhow::Result<Option<WaitTicket>> {
        self.stamp(caller)?;
        let descendants = if reason == TurnWaitReason::DescendantRecovery {
            self.wait_descendants(caller)?
        } else {
            Vec::new()
        };
        let ticket = self
            .state
            .coordination_commit_turn_wait(
                caller,
                self.runtime_id,
                turn_id,
                reason,
                deadline_unix_ms,
                now_unix_ms,
                &descendants,
            )
            .await?;
        if ticket.is_some() {
            self.registry.invalidate_observation();
        }
        Ok(ticket)
    }

    pub(crate) async fn provider_recovered(
        &self,
        caller: WorkStamp,
        turn_id: &str,
    ) -> anyhow::Result<()> {
        self.stamp(caller)?;
        self.state
            .coordination_provider_recovered(caller, self.runtime_id, turn_id)
            .await
    }

    pub(crate) async fn observe_wait(
        &self,
        caller: WorkStamp,
        id: WaitId,
    ) -> anyhow::Result<Option<WaitTicket>> {
        self.stamp(caller)?;
        self.state.coordination_observe_wait(caller, id).await
    }

    /// A cold ticket remains observable, but its runtime_id cannot authorize
    /// delivery or automatically rearm execution in the receiving runtime.
    pub(crate) async fn current_wait(&self, actor: ThreadId) -> anyhow::Result<Option<WaitTicket>> {
        self.member(actor)?;
        self.state
            .coordination_current_wait(self.root()?, actor)
            .await
    }
}

#[cfg(test)]
#[path = "work_wait_tests.rs"]
mod tests;

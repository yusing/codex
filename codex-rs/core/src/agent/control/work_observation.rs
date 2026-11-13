use super::WorkLedger;
use codex_protocol::ThreadId;
use codex_state::coordination::ActorWorkObservation;

impl WorkLedger {
    pub(crate) async fn observe_members(
        &self,
        actors: &[ThreadId],
    ) -> anyhow::Result<Vec<ActorWorkObservation>> {
        for actor in actors {
            self.member(*actor)?;
        }
        let mut facts = self
            .state
            .coordination_observe(self.root()?, actors)
            .await?;
        for fact in &mut facts {
            fact.work = fact
                .work
                .take()
                .map(|work| work.observed_in(self.runtime_id));
        }
        Ok(facts)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::control::LocalAgentRuntime;
    use crate::agent::control::work::DeliveryKind;
    use crate::agent::control::work::PublishDelivery;
    use crate::agent::control::work::WorkDisposition;
    use crate::agent::control::work::WorkStamp;
    use crate::agent::control::work::work_wait::WaitCell;
    use crate::agent::control::work::work_wait::WaitMode;
    use crate::agent::types::AgentMetadata;
    use crate::thread_manager::default_thread_id_generator;
    use codex_protocol::AgentPath;
    use codex_utils_absolute_path::test_support::PathExt;
    use std::sync::Weak;
    use uuid::Uuid;

    #[tokio::test]
    async fn snapshot_retains_work_revision_and_wait_selection_without_consuming()
    -> anyhow::Result<()> {
        let directory = tempfile::TempDir::new()?;
        let state = codex_state::StateRuntime::init(
            codex_state::SqliteConfig::new_for_testing(directory.path().abs()),
            "test".to_owned(),
        )
        .await?;
        let runtime = LocalAgentRuntime::new(
            Weak::default(),
            default_thread_id_generator(),
            None,
            Some(state),
        );
        let root = ThreadId::new();
        let child = ThreadId::new();
        runtime.registry.register_root_thread(root);
        runtime
            .registry
            .reserve_spawn_slot(None)?
            .commit(AgentMetadata {
                agent_id: Some(child),
                agent_path: Some(
                    AgentPath::root()
                        .join("child")
                        .map_err(anyhow::Error::msg)?,
                ),
                ..Default::default()
            });
        let ledger = runtime.work_ledger()?;
        let mut stamps = Vec::new();
        for actor in [root, child] {
            let stamp = WorkStamp {
                root_id: root,
                actor_id: actor,
                work_id: Uuid::now_v7(),
            };
            ledger
                .admit_work(
                    stamp,
                    None,
                    &PublishDelivery {
                        id: stamp.work_id,
                        root_id: root,
                        sender_id: None,
                        recipient_id: actor,
                        work: Some(stamp),
                        kind: DeliveryKind::Message,
                        payload: "original payload".to_owned(),
                    },
                )
                .await?;
            stamps.push(stamp);
        }
        let ticket = ledger
            .register_wait(
                stamps[0],
                &WaitCell {
                    turn_id: "turn".to_owned(),
                    item_id: "item".to_owned(),
                    window_id: "window".to_owned(),
                    cell_id: "cell".to_owned(),
                },
                WaitMode::All,
            )
            .await?;
        let mut changes = runtime.registry.subscribe();
        let facts = ledger.observe_members(&[root, child]).await?;
        assert_eq!(facts.len(), 2);
        let work = facts[1].work.as_ref().unwrap();
        assert_eq!(work.stamp, stamps[1]);
        assert_eq!(work.revision, 1);
        assert_eq!(work.disposition, WorkDisposition::PendingStart);
        assert_eq!(facts[0].wait.as_ref().unwrap().selected, ticket.selected);
        assert_eq!(ledger.current_wait(root).await?, Some(ticket));
        assert!(
            !ledger
                .observe_delivery(child, stamps[1].work_id)
                .await?
                .unwrap()
                .consumed
        );
        assert!(!changes.has_changed()?);
        assert!(
            ledger
                .transition_work(stamps[1], 99, WorkDisposition::Running)
                .await
                .is_err()
        );
        assert!(!changes.has_changed()?);
        ledger
            .transition_work(stamps[1], 1, WorkDisposition::Running)
            .await?;
        assert!(changes.has_changed()?);
        changes.borrow_and_update();
        Ok(())
    }
}

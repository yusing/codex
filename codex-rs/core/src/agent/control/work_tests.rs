use super::*;
use crate::agent::control::LocalAgentRuntime;
use crate::agent::types::AgentMetadata;
use crate::thread_manager::default_thread_id_generator;
use codex_utils_absolute_path::test_support::PathExt;
use pretty_assertions::assert_eq;
use std::sync::Weak;
use tempfile::TempDir;

fn runtime(state: Arc<StateRuntime>, root: ThreadId, child: ThreadId) -> LocalAgentRuntime {
    let runtime = LocalAgentRuntime::new(
        Weak::default(),
        default_thread_id_generator(),
        None,
        Some(state),
    );
    runtime.registry.register_root_thread(root);
    // Use the existing membership authority, with no CodexThread or history.
    runtime
        .registry
        .reserve_spawn_slot(None)
        .unwrap()
        .commit(AgentMetadata {
            agent_id: Some(child),
            agent_path: Some(AgentPath::root().join("external").unwrap()),
            ..Default::default()
        });
    runtime
}

#[tokio::test]
async fn registered_logical_actor_has_work_without_an_execution_binding() -> anyhow::Result<()> {
    let directory = TempDir::new()?;
    let state = StateRuntime::init(
        codex_state::SqliteConfig::new_for_testing(directory.path().abs()),
        "test-provider".to_owned(),
    )
    .await?;
    let root = ThreadId::new();
    let child = ThreadId::new();
    let runtime = runtime(Arc::clone(&state), root, child);
    let ledger = runtime.work_ledger()?;
    assert_eq!(ledger.work_stamp(child).await?, None);
    let stamp = WorkStamp {
        root_id: root,
        actor_id: child,
        work_id: Uuid::now_v7(),
    };
    let input = PublishDelivery {
        id: stamp.work_id,
        root_id: root,
        sender_id: Some(root),
        recipient_id: child,
        work: Some(stamp),
        kind: DeliveryKind::Message,
        payload: "assignment".to_owned(),
    };
    let admitted = ledger.admit_work(stamp, None, &input).await?;
    assert_eq!(admitted.runtime_id, runtime.tree_id());
    assert_eq!(ledger.work_stamp(child).await?, Some(stamp));
    let mut invalid = input.clone();
    invalid.id = Uuid::now_v7();
    invalid.recipient_id = ThreadId::new();
    assert!(ledger.publish_delivery(&invalid).await.is_err());
    assert!(
        state
            .coordination_delivery(root, invalid.recipient_id, invalid.id)
            .await?
            .is_none()
    );
    invalid.recipient_id = child;
    invalid.root_id = ThreadId::new();
    assert!(ledger.publish_delivery(&invalid).await.is_err());
    let foreign_sender = ThreadId::new();
    let foreign_message = PublishDelivery {
        id: Uuid::now_v7(),
        root_id: root,
        sender_id: Some(foreign_sender),
        recipient_id: child,
        work: None,
        kind: DeliveryKind::Message,
        payload: "authenticated foreign input".to_owned(),
    };
    assert_eq!(
        ledger
            .publish_delivery(&foreign_message)
            .await?
            .publication
            .sender_id,
        Some(foreign_sender)
    );
    assert!(
        runtime
            .registry
            .agent_metadata_for_thread(foreign_sender)
            .is_none()
    );
    assert_eq!(ledger.observe_current(child).await?, Some(admitted));
    Ok(())
}

#[tokio::test]
async fn cold_observation_pauses_without_mutating_facts_or_replacing_work_identity()
-> anyhow::Result<()> {
    let directory = TempDir::new()?;
    let state = StateRuntime::init(
        codex_state::SqliteConfig::new_for_testing(directory.path().abs()),
        "test-provider".to_owned(),
    )
    .await?;
    let root = ThreadId::new();
    let child = ThreadId::new();
    let original = runtime(Arc::clone(&state), root, child);
    let stamp = WorkStamp {
        root_id: root,
        actor_id: child,
        work_id: Uuid::now_v7(),
    };
    let input = PublishDelivery {
        id: stamp.work_id,
        root_id: root,
        sender_id: None,
        recipient_id: child,
        work: Some(stamp),
        kind: DeliveryKind::Message,
        payload: "human input".to_owned(),
    };
    let admitted = original
        .work_ledger()?
        .admit_work(stamp, None, &input)
        .await?;
    let restored = runtime(Arc::clone(&state), root, child);
    let observed = restored.work_ledger()?.observe_work(stamp).await?.unwrap();
    assert_eq!(observed.disposition, WorkDisposition::Paused);
    assert_eq!(observed.runtime_id, original.tree_id());
    assert_eq!(state.coordination_work(stamp).await?, Some(admitted));
    assert_eq!(
        restored.work_ledger()?.work_stamp(child).await?,
        Some(stamp)
    );
    assert!(
        restored
            .work_ledger()?
            .transition_work(stamp, observed.revision, WorkDisposition::Running)
            .await
            .is_err()
    );
    assert!(!restored.shutdown.is_cancelled());
    Ok(())
}

#[test]
fn absent_store_is_an_explicit_error_not_an_ambient_fallback() {
    let runtime =
        LocalAgentRuntime::new(Weak::default(), default_thread_id_generator(), None, None);
    assert!(runtime.work_ledger().is_err());
}

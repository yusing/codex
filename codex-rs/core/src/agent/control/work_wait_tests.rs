use super::*;
use crate::agent::control::LocalAgentRuntime;
use crate::agent::control::work::DeliveryKind;
use crate::agent::control::work::PublishDelivery;
use crate::agent::control::work::WorkDisposition;
use crate::agent::types::AgentMetadata;
use crate::thread_manager::default_thread_id_generator;
use codex_protocol::AgentPath;
use codex_state::StateRuntime;
use codex_state::coordination_wait::WaitError;
use codex_utils_absolute_path::test_support::PathExt;
use pretty_assertions::assert_eq;
use std::sync::Arc;
use std::sync::Weak;
use tempfile::TempDir;

fn register(runtime: &LocalAgentRuntime, path: &str) -> ThreadId {
    let actor = ThreadId::new();
    runtime
        .registry
        .reserve_spawn_slot(None)
        .unwrap()
        .commit(AgentMetadata {
            agent_id: Some(actor),
            agent_path: Some(AgentPath::from_string(path.to_owned()).unwrap()),
            ..Default::default()
        });
    actor
}

async fn admit(ledger: &WorkLedger, root: ThreadId, actor: ThreadId) -> anyhow::Result<WorkStamp> {
    let stamp = WorkStamp {
        root_id: root,
        actor_id: actor,
        work_id: Uuid::now_v7(),
    };
    let input = PublishDelivery {
        id: stamp.work_id,
        root_id: root,
        sender_id: None,
        recipient_id: actor,
        work: Some(stamp),
        kind: DeliveryKind::Message,
        payload: "accepted assignment".to_owned(),
    };
    ledger.admit_work(stamp, None, &input).await?;
    let dispatch = input.id;
    ledger
        .confirm_delivery(actor, &[input.id], dispatch, "input")
        .await?;
    ledger
        .consume_deliveries(actor, &[input.id], "input")
        .await?;
    Ok(stamp)
}

async fn terminal(
    ledger: &WorkLedger,
    stamp: WorkStamp,
    recipient: ThreadId,
) -> anyhow::Result<Uuid> {
    let candidate = ledger
        .stage_completion(
            stamp,
            "terminal-turn",
            recipient,
            WorkDisposition::Completed,
            None,
        )
        .await?
        .unwrap();
    ledger
        .complete_capture(stamp, candidate.delivery_id, None)
        .await?;
    ledger
        .completion_settlement(stamp, candidate.delivery_id, true)
        .await?;
    ledger
        .finish_completion(
            stamp,
            candidate.delivery_id,
            "authoritative completed result with prepared captures",
        )
        .await?
        .expect("selected fixture work is complete");
    Ok(candidate.delivery_id)
}

fn cell() -> WaitCell {
    WaitCell {
        turn_id: "turn".to_owned(),
        item_id: Uuid::now_v7().to_string(),
        window_id: "original-window".to_owned(),
        cell_id: "native-cell".to_owned(),
    }
}

#[tokio::test]
async fn registration_freezes_complete_options_and_captures_one_deadline() -> anyhow::Result<()> {
    use codex_protocol::protocol::ReloadSelection;
    use codex_state::coordination_wait::WaitResetState;
    use codex_state::coordination_wait::WaitTimeout;

    let directory = TempDir::new()?;
    let state = StateRuntime::init(
        codex_state::SqliteConfig::new_for_testing(directory.path().abs()),
        "test-provider".to_owned(),
    )
    .await?;
    let runtime = LocalAgentRuntime::new(
        Weak::default(),
        default_thread_id_generator(),
        None,
        Some(state),
    );
    let root = ThreadId::new();
    runtime.registry.register_root_thread(root);
    let child = register(&runtime, "/root/child");
    let ledger = runtime.work_ledger()?;
    let caller = admit(&ledger, root, root).await?;
    admit(&ledger, root, child).await?;
    let options = WaitOptions {
        mode: WaitMode::All,
        timeout: Some(WaitTimeout {
            requested_ms: 1,
            effective_ms: 1_000,
        }),
        progress_interval_ms: Some(300_000),
        compact: true,
        reload: Some(ReloadSelection {
            sources: Vec::new(),
        }),
    };
    let mut observations = runtime.registry.subscribe();
    let before = chrono::Utc::now().timestamp_millis();
    let ticket = ledger
        .register_wait_with_options(caller, &cell(), &options)
        .await?;
    assert!(observations.has_changed()?);
    let after = chrono::Utc::now().timestamp_millis();
    assert_eq!(ticket.options, options);
    assert_eq!(ticket.reset, WaitResetState::Pending);
    assert!((before + 1_000..=after + 1_000).contains(&ticket.deadline_unix_ms.unwrap()));
    assert_eq!(
        ledger
            .register_wait_with_options(caller, ticket.origin.cell().unwrap(), &options)
            .await?,
        ticket
    );
    observations.borrow_and_update();
    let mut changed = Vec::new();
    changed.push(WaitOptions {
        mode: WaitMode::Any,
        ..options.clone()
    });
    changed.push(WaitOptions {
        timeout: None,
        ..options.clone()
    });
    changed.push(WaitOptions {
        timeout: Some(WaitTimeout {
            requested_ms: 2,
            effective_ms: 1_000,
        }),
        ..options.clone()
    });
    changed.push(WaitOptions {
        timeout: Some(WaitTimeout {
            requested_ms: 1,
            effective_ms: 2_000,
        }),
        ..options.clone()
    });
    changed.push(WaitOptions {
        progress_interval_ms: None,
        ..options.clone()
    });
    changed.push(WaitOptions {
        reload: None,
        ..options.clone()
    });
    changed.push(WaitOptions {
        compact: false,
        reload: None,
        ..options.clone()
    });
    for options in changed {
        let error = ledger
            .register_wait_with_options(caller, ticket.origin.cell().unwrap(), &options)
            .await
            .unwrap_err();
        assert_eq!(
            error.downcast_ref::<WaitError>(),
            Some(&WaitError::ConflictingCell)
        );
        assert!(!observations.has_changed()?);
    }
    assert_eq!(ledger.current_wait(root).await?, Some(ticket.clone()));
    assert!(ledger.claim_ready_wait(caller, ticket.id).await?.is_none());
    assert!(!observations.has_changed()?);
    ledger
        .cancel_wait(caller, ticket.id, ticket.origin.cell().unwrap())
        .await?;
    assert!(observations.has_changed()?);
    observations.borrow_and_update();
    let invalid = WaitOptions {
        compact: false,
        ..options
    };
    assert_eq!(
        ledger
            .register_wait_with_options(caller, &cell(), &invalid)
            .await
            .unwrap_err()
            .downcast_ref::<WaitError>(),
        Some(&WaitError::InvalidOptions)
    );
    assert!(ledger.current_wait(root).await?.is_none());
    assert!(!observations.has_changed()?);
    let ordinary = ledger.register_wait(caller, &cell(), WaitMode::Any).await?;
    assert_eq!(ordinary.deadline_unix_ms, None);
    assert_eq!(ordinary.options.reload, None);
    assert_eq!(ordinary.reset, WaitResetState::NotRequested);
    Ok(())
}

#[tokio::test]
async fn native_registry_snapshots_transitive_descendants_without_stealing_their_mail()
-> anyhow::Result<()> {
    let directory = TempDir::new()?;
    let state = StateRuntime::init(
        codex_state::SqliteConfig::new_for_testing(directory.path().abs()),
        "test-provider".to_owned(),
    )
    .await?;
    let runtime = LocalAgentRuntime::new(
        Weak::default(),
        default_thread_id_generator(),
        None,
        Some(state),
    );
    let root = ThreadId::new();
    runtime.registry.register_root_thread(root);
    let branch = register(&runtime, "/root/branch");
    let leaf = register(&runtime, "/root/branch/leaf");
    let deep = register(&runtime, "/root/branch/leaf/deep");
    let sibling = register(&runtime, "/root/branch2");
    let ledger = runtime.work_ledger()?;
    let caller = admit(&ledger, root, branch).await?;
    let leaf_work = admit(&ledger, root, leaf).await?;
    admit(&ledger, root, sibling).await?;
    let leaf_result = terminal(&ledger, leaf_work, branch).await?;
    // Later descendant work remains visible beneath its already-settled branch.
    let deep_work = admit(&ledger, root, deep).await?;
    let ticket = ledger.register_wait(caller, &cell(), WaitMode::All).await?;
    assert_eq!(ticket.selected.len(), 2);
    assert!(ticket.selected.contains(&leaf_work));
    assert!(ticket.selected.contains(&deep_work));
    assert_eq!(ledger.current_wait(branch).await?, Some(ticket.clone()));
    let late = register(&runtime, "/root/branch/late");
    admit(&ledger, root, late).await?;
    assert!(ledger.claim_ready_wait(caller, ticket.id).await?.is_none());
    let deep_result = terminal(&ledger, deep_work, leaf).await?;
    let claim = ledger.claim_ready_wait(caller, ticket.id).await?.unwrap();
    assert_eq!(claim.delivery_ids, vec![leaf_result]);
    assert_eq!(claim.outcomes.len(), 2);
    assert_eq!(
        ledger
            .observe_delivery(leaf, deep_result)
            .await?
            .unwrap()
            .claim_id,
        None
    );
    ledger
        .state()
        .coordination_consume_awaited_wait(
            caller,
            runtime.tree_id(),
            ticket.id,
            claim.id,
            Uuid::now_v7(),
        )
        .await?;
    assert_eq!(ledger.current_wait(branch).await?, None);
    assert!(
        !ledger
            .observe_delivery(leaf, deep_result)
            .await?
            .unwrap()
            .consumed
    );
    assert!(
        ledger
            .observe_delivery(branch, leaf_result)
            .await?
            .unwrap()
            .consumed
    );
    let next = ledger.register_wait(caller, &cell(), WaitMode::Any).await?;
    assert_eq!(next.selected.len(), 1);
    assert_eq!(next.selected[0].actor_id, late);
    Ok(())
}

#[tokio::test]
async fn unrelated_tree_work_and_mail_cannot_make_an_empty_child_wait_eligible()
-> anyhow::Result<()> {
    let directory = TempDir::new()?;
    let state = StateRuntime::init(
        codex_state::SqliteConfig::new_for_testing(directory.path().abs()),
        "test-provider".to_owned(),
    )
    .await?;
    let runtime = LocalAgentRuntime::new(
        Weak::default(),
        default_thread_id_generator(),
        None,
        Some(state),
    );
    let root = ThreadId::new();
    runtime.registry.register_root_thread(root);
    let branch = register(&runtime, "/root/branch");
    let sibling = register(&runtime, "/root/branch2");
    let ledger = runtime.work_ledger()?;
    let caller = admit(&ledger, root, branch).await?;
    let unrelated = admit(&ledger, root, sibling).await?;
    terminal(&ledger, unrelated, root).await?;
    let error = ledger
        .register_wait(caller, &cell(), WaitMode::Any)
        .await
        .unwrap_err();
    assert_eq!(
        error.downcast_ref::<WaitError>(),
        Some(&WaitError::EmptySelection)
    );
    assert_eq!(ledger.current_wait(branch).await?, None);
    let message = PublishDelivery {
        id: Uuid::now_v7(),
        root_id: root,
        sender_id: Some(sibling),
        recipient_id: branch,
        work: None,
        kind: DeliveryKind::Message,
        payload: "retained addressed message".to_owned(),
    };
    ledger.publish_delivery(&message).await?;
    let ticket = ledger.register_wait(caller, &cell(), WaitMode::All).await?;
    assert!(ticket.selected.is_empty());
    assert_eq!(
        ledger
            .claim_ready_wait(caller, ticket.id)
            .await?
            .unwrap()
            .delivery_ids,
        vec![message.id]
    );
    ledger
        .cancel_wait(caller, ticket.id, ticket.origin.cell().unwrap())
        .await?;
    assert!(
        !ledger
            .observe_delivery(branch, message.id)
            .await?
            .unwrap()
            .consumed
    );
    Ok(())
}

#[tokio::test]
async fn restored_native_registry_can_observe_but_not_rearm_retained_waits() -> anyhow::Result<()> {
    let directory = TempDir::new()?;
    let state = StateRuntime::init(
        codex_state::SqliteConfig::new_for_testing(directory.path().abs()),
        "test-provider".to_owned(),
    )
    .await?;
    let root = ThreadId::new();
    let first = LocalAgentRuntime::new(
        Weak::default(),
        default_thread_id_generator(),
        None,
        Some(Arc::clone(&state)),
    );
    first.registry.register_root_thread(root);
    let child = register(&first, "/root/child");
    let ledger = first.work_ledger()?;
    let caller = admit(&ledger, root, root).await?;
    admit(&ledger, root, child).await?;
    let ticket = ledger.register_wait(caller, &cell(), WaitMode::Any).await?;
    let retained = ledger.observe_wait(caller, ticket.id).await?;
    let restored = LocalAgentRuntime::new(
        Weak::default(),
        default_thread_id_generator(),
        None,
        Some(state),
    );
    restored.registry.register_root_thread(root);
    let cold = restored.work_ledger()?;
    assert_eq!(cold.current_wait(root).await?, retained);
    assert_eq!(
        cold.observe_work(caller).await?.unwrap().disposition,
        WorkDisposition::Paused
    );
    assert!(cold.claim_ready_wait(caller, ticket.id).await.is_err());
    assert!(
        cold.cancel_wait(caller, ticket.id, ticket.origin.cell().unwrap())
            .await
            .is_err()
    );
    assert_eq!(cold.observe_wait(caller, ticket.id).await?, retained);
    Ok(())
}

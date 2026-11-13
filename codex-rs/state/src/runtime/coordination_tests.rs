use super::*;
use codex_utils_absolute_path::test_support::PathExt;
use pretty_assertions::assert_eq;
use std::sync::Arc;
use tempfile::TempDir;

async fn fixture() -> anyhow::Result<(TempDir, Arc<StateRuntime>)> {
    let directory = TempDir::new()?;
    let state = StateRuntime::init(
        crate::SqliteConfig::new_for_testing(directory.path().abs()),
        "test-provider".to_owned(),
    )
    .await?;
    Ok((directory, state))
}

fn initial(root: ThreadId, actor: ThreadId) -> (WorkStamp, PublishDelivery) {
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
        payload: "human assignment with frozen sources".to_owned(),
    };
    (stamp, input)
}

fn message(root: ThreadId, recipient: ThreadId) -> PublishDelivery {
    PublishDelivery {
        id: Uuid::now_v7(),
        root_id: root,
        sender_id: Some(root),
        recipient_id: recipient,
        work: None,
        kind: DeliveryKind::Message,
        payload: "addressed message".to_owned(),
    }
}

fn assert_conflict(error: anyhow::Error, expected: CoordinationError) {
    assert_eq!(error.downcast_ref::<CoordinationError>(), Some(&expected));
}

#[tokio::test]
async fn admitted_input_retains_its_receipt_without_replacing_work_identity() -> anyhow::Result<()>
{
    let (_directory, state) = fixture().await?;
    let root = ThreadId::new();
    let actor = ThreadId::new();
    let runtime = Uuid::now_v7();
    let (stamp, input) = initial(root, actor);
    let accepted = state
        .coordination_admit_input(&input, runtime, true, None, None, &[], None)
        .await?;
    assert_eq!(accepted.disposition, WorkDisposition::PendingStart);
    let repeated = state
        .coordination_repeated_delivery(root, actor, input.id)
        .await?
        .unwrap();
    assert_eq!(repeated.publication, input);
    assert_eq!(repeated.dispatch_id, Some(input.id));
    let running = state
        .coordination_transition_work(stamp, accepted.revision, runtime, WorkDisposition::Running)
        .await?;
    let waiting = state
        .coordination_transition_work(stamp, running.revision, runtime, WorkDisposition::Waiting)
        .await?;
    let resumed = state
        .coordination_transition_work(stamp, waiting.revision, runtime, WorkDisposition::Running)
        .await?;
    assert_eq!(resumed.stamp, stamp);
    assert_conflict(
        state
            .coordination_transition_work(
                stamp,
                accepted.revision,
                runtime,
                WorkDisposition::Paused,
            )
            .await
            .unwrap_err(),
        CoordinationError::StaleWork,
    );
    let (_, next_input) = initial(root, actor);
    assert_conflict(
        state
            .coordination_admit_input(&next_input, runtime, true, Some(stamp), None, &[], None)
            .await
            .unwrap_err(),
        CoordinationError::InvalidDelivery,
    );
    assert!(
        state
            .coordination_delivery(root, actor, next_input.id)
            .await?
            .is_none()
    );
    assert_eq!(
        state.coordination_current_work(root, actor).await?,
        Some(resumed)
    );
    Ok(())
}

#[tokio::test]
async fn terminal_payload_and_fact_commit_together_and_survive_notification_consumption()
-> anyhow::Result<()> {
    for collision in [false, true] {
        let (_directory, state) = fixture().await?;
        let root = ThreadId::new();
        let actor = ThreadId::new();
        let runtime = Uuid::now_v7();
        let (stamp, input) = initial(root, actor);
        state
            .coordination_admit_input(&input, runtime, true, None, None, &[], None)
            .await?;
        let candidate = state
            .coordination_stage_completion(
                stamp,
                runtime,
                "final-turn",
                root,
                WorkDisposition::Completed,
                None,
                &[],
            )
            .await?
            .unwrap();
        let id = candidate.delivery_id;
        state.coordination_complete_capture(stamp, id, None).await?;
        state
            .coordination_completion_settlement(stamp, runtime, id, true)
            .await?;
        let payload = "ordered final answers and prepared capture evidence";
        if collision {
            let mut conflicting = message(root, root);
            conflicting.id = id;
            state.coordination_publish_delivery(&conflicting).await?;
            assert_conflict(
                state
                    .coordination_finish_completion(stamp, runtime, id, payload)
                    .await
                    .unwrap_err(),
                CoordinationError::DeliveryConflict,
            );
            assert_eq!(
                state.coordination_work(stamp).await?.unwrap().disposition,
                WorkDisposition::Waiting
            );
            continue;
        }
        let completed = state
            .coordination_finish_completion(stamp, runtime, id, payload)
            .await?
            .unwrap();
        assert_eq!(completed.disposition, WorkDisposition::Completed);
        assert!(
            state
                .coordination_finish_completion(stamp, runtime, id, payload)
                .await?
                .is_none()
        );
        let dispatch = Uuid::now_v7();
        assert!(
            state
                .coordination_begin_delivery(root, root, &[id], None, dispatch)
                .await?
        );
        state
            .coordination_confirm_delivery(root, root, &[id], dispatch, "native-input-1")
            .await?;
        for _ in 0..2 {
            state
                .coordination_consume_deliveries(root, root, &[id], "native-input-1")
                .await?;
        }
        assert_eq!(
            state.coordination_work(stamp).await?,
            Some(completed.clone())
        );
        let (next, next_input) = initial(root, actor);
        state
            .coordination_admit_input(&next_input, runtime, true, Some(stamp), None, &[], None)
            .await?;
        assert!(
            state
                .coordination_finish_completion(stamp, runtime, id, payload)
                .await?
                .is_none()
        );
        assert_eq!(
            state
                .coordination_current_work(root, actor)
                .await?
                .unwrap()
                .stamp,
            next
        );
        assert_eq!(state.coordination_work(stamp).await?, Some(completed));
    }
    Ok(())
}

#[tokio::test]
async fn dropped_wait_transaction_rolls_back_claims_without_consumption() -> anyhow::Result<()> {
    let (_directory, state) = fixture().await?;
    let root = ThreadId::new();
    let event = message(root, root);
    state.coordination_publish_delivery(&event).await?;
    let foreign = message(root, ThreadId::new());
    state.coordination_publish_delivery(&foreign).await?;
    let mut tx = state.pool.begin_immediate().await?;
    assert_conflict(
        claim_deliveries(&mut tx, root, root, &[event.id, foreign.id], Uuid::now_v7())
            .await
            .unwrap_err(),
        CoordinationError::UnknownDelivery,
    );
    drop(tx);
    assert!(
        state
            .coordination_delivery(root, root, event.id)
            .await?
            .unwrap()
            .claim_id
            .is_none()
    );
    let mut tx = state.pool.begin_immediate().await?;
    claim_deliveries(&mut tx, root, root, &[event.id], Uuid::now_v7()).await?;
    // W2 can add ticket/selection writes to this transaction. Dropping an
    // uncommitted future/transaction cannot leave a durable partial claim.
    drop(tx);
    let retained = state
        .coordination_delivery(root, root, event.id)
        .await?
        .unwrap();
    assert_eq!(retained.claim_id, None);
    assert!(!retained.consumed);
    Ok(())
}

#[tokio::test]
async fn reopen_retains_payload_and_uncertain_dispatch_without_resuming_work() -> anyhow::Result<()>
{
    let (directory, state) = fixture().await?;
    let root = ThreadId::new();
    let actor = ThreadId::new();
    let previous_runtime = Uuid::now_v7();
    let (stamp, input) = initial(root, actor);
    state
        .coordination_admit_input(&input, previous_runtime, true, None, None, &[], None)
        .await?;
    let waiting = state
        .coordination_transition_work(stamp, 1, previous_runtime, WorkDisposition::Waiting)
        .await?;
    let dispatch = input.id;
    state
        .coordination_begin_delivery(root, actor, &[input.id], None, dispatch)
        .await?;
    drop(state);
    let reopened = StateRuntime::init(
        crate::SqliteConfig::new_for_testing(directory.path().abs()),
        "test-provider".to_owned(),
    )
    .await?;
    let new_runtime = Uuid::now_v7();
    let retained = reopened.coordination_work(stamp).await?.unwrap();
    assert_eq!(retained, waiting);
    assert_eq!(
        retained.clone().observed_in(new_runtime).disposition,
        WorkDisposition::Paused
    );
    assert_eq!(
        reopened.coordination_work(stamp).await?,
        Some(waiting.clone())
    );
    assert_eq!(
        reopened
            .coordination_delivery(root, actor, input.id)
            .await?
            .unwrap()
            .publication,
        input
    );
    assert!(
        !reopened
            .coordination_begin_delivery(root, actor, &[input.id], None, dispatch)
            .await?
    );
    assert_conflict(
        reopened
            .coordination_transition_work(
                stamp,
                waiting.revision,
                new_runtime,
                WorkDisposition::Running,
            )
            .await
            .unwrap_err(),
        CoordinationError::StaleWork,
    );
    let paused = reopened
        .coordination_reconcile_work(stamp, waiting.revision, new_runtime)
        .await?;
    assert_eq!(paused.disposition, WorkDisposition::Paused);
    assert_eq!(paused.stamp, stamp);
    let uncertain = reopened
        .coordination_transition_work(
            stamp,
            paused.revision,
            new_runtime,
            WorkDisposition::Uncertain,
        )
        .await?;
    assert_eq!(
        uncertain.observed_in(Uuid::now_v7()).disposition,
        WorkDisposition::Uncertain
    );
    Ok(())
}

#[tokio::test]
async fn recipient_pages_and_current_work_use_indexes_not_unrelated_history() -> anyhow::Result<()>
{
    let (_directory, state) = fixture().await?;
    let root = ThreadId::new();
    let wanted = ThreadId::new();
    let unrelated = ThreadId::new();
    // Grow unrelated retained data without thousands of independent commit fsyncs.
    let mut tx = state.pool.begin_immediate().await?;
    for _ in 0..512 {
        insert_delivery(&mut tx, &message(root, unrelated)).await?;
    }
    let first = insert_delivery(&mut tx, &message(root, wanted)).await?;
    let second = insert_delivery(&mut tx, &message(root, wanted)).await?;
    tx.commit().await?;
    assert_eq!(
        state
            .coordination_unread_deliveries(root, wanted, 0, 1)
            .await?,
        vec![first.clone()]
    );
    assert_eq!(
        state
            .coordination_unread_deliveries(root, wanted, first.sequence, 1)
            .await?,
        vec![second]
    );
    let plan = sqlx::query(
        "EXPLAIN QUERY PLAN SELECT * FROM agent_deliveries
         WHERE root_id=? AND recipient_id=? AND consumed=0 AND sequence>?
         ORDER BY sequence LIMIT ?",
    )
    .bind(root.to_string())
    .bind(wanted.to_string())
    .bind(0_i64)
    .bind(1_i64)
    .fetch_all(state.pool.as_ref())
    .await?;
    let details: Vec<String> = plan.into_iter().map(|row| row.get("detail")).collect();
    assert!(
        details
            .iter()
            .any(|detail| detail.contains("agent_deliveries_unread")),
        "{details:?}"
    );
    assert!(
        !details
            .iter()
            .any(|detail| detail.contains("SCAN agent_deliveries")),
        "{details:?}"
    );
    let plan = sqlx::query(
        "EXPLAIN QUERY PLAN SELECT w.* FROM agent_current_work c JOIN agent_work w
         USING(root_id,actor_id,work_id) WHERE c.root_id=? AND c.actor_id=?",
    )
    .bind(root.to_string())
    .bind(wanted.to_string())
    .fetch_all(state.pool.as_ref())
    .await?;
    let details: Vec<String> = plan.into_iter().map(|row| row.get("detail")).collect();
    assert!(
        details.iter().all(|detail| !detail.starts_with("SCAN ")),
        "{details:?}"
    );
    Ok(())
}

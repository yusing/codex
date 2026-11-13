use super::*;
use crate::coordination_wait::WaitCell;
use crate::coordination_wait::WaitMode;
use codex_utils_absolute_path::test_support::PathExt;
use pretty_assertions::assert_eq;
use std::sync::Arc;
use tempfile::TempDir;

async fn fixture() -> anyhow::Result<(TempDir, Arc<StateRuntime>, ThreadId, Uuid)> {
    let dir = TempDir::new()?;
    let state = StateRuntime::init(
        crate::SqliteConfig::new_for_testing(dir.path().abs()),
        "test-provider".to_owned(),
    )
    .await?;
    Ok((dir, state, ThreadId::new(), Uuid::now_v7()))
}

fn input(stamp: WorkStamp, id: Uuid) -> PublishDelivery {
    PublishDelivery {
        id,
        root_id: stamp.root_id,
        sender_id: None,
        recipient_id: stamp.actor_id,
        work: Some(stamp),
        kind: DeliveryKind::Message,
        payload: "prepared human input".to_owned(),
    }
}

async fn admit(
    state: &StateRuntime,
    root: ThreadId,
    actor: ThreadId,
    runtime: Uuid,
    ancestors: &[ThreadId],
) -> anyhow::Result<WorkRecord> {
    let stamp = WorkStamp {
        root_id: root,
        actor_id: actor,
        work_id: Uuid::now_v7(),
    };
    let accepted = state
        .coordination_admit_input(
            &input(stamp, stamp.work_id),
            runtime,
            true,
            None,
            None,
            ancestors,
            None,
        )
        .await?;
    state
        .coordination_transition_work(stamp, accepted.revision, runtime, WorkDisposition::Running)
        .await
}

fn cell() -> WaitCell {
    WaitCell {
        turn_id: "turn".to_owned(),
        item_id: "item".to_owned(),
        window_id: "window".to_owned(),
        cell_id: "cell".to_owned(),
    }
}

#[tokio::test]
async fn captured_parent_final_waits_for_execution_settlement_after_children_finish()
-> anyhow::Result<()> {
    let (_dir, state, root, runtime) = fixture().await?;
    let parent = admit(&state, root, root, runtime, &[]).await?;
    let child = admit(&state, root, ThreadId::new(), runtime, &[root]).await?;
    state
        .coordination_record_answer(parent.stamp, runtime, "answer", "original final")
        .await?;
    let final_fact = state
        .coordination_stage_completion(
            parent.stamp,
            runtime,
            "final-turn",
            root,
            WorkDisposition::Completed,
            None,
            &[child.stamp.actor_id],
        )
        .await?
        .unwrap();
    state
        .coordination_completion_settlement(parent.stamp, runtime, final_fact.delivery_id, false)
        .await?;
    state
        .coordination_complete_capture(parent.stamp, final_fact.delivery_id, Some("capture"))
        .await?;
    let child_final = state
        .coordination_stage_completion(
            child.stamp,
            runtime,
            "child-turn",
            root,
            WorkDisposition::Failed,
            Some("failed"),
            &[],
        )
        .await?
        .unwrap();
    state
        .coordination_finish_completion(
            child.stamp,
            runtime,
            child_final.delivery_id,
            "child failure",
        )
        .await?
        .unwrap();
    assert_eq!(
        state
            .coordination_completion_dependents(child.stamp)
            .await?,
        vec![parent.stamp]
    );
    assert!(
        state
            .coordination_finish_completion(
                parent.stamp,
                runtime,
                final_fact.delivery_id,
                "too early"
            )
            .await?
            .is_none()
    );
    assert!(
        state
            .coordination_completion_settlement(parent.stamp, runtime, final_fact.delivery_id, true)
            .await?
    );
    state
        .coordination_finish_completion(
            parent.stamp,
            runtime,
            final_fact.delivery_id,
            "original frozen final",
        )
        .await?
        .unwrap();
    assert_eq!(
        state
            .coordination_terminal_delivery(parent.stamp)
            .await?
            .unwrap()
            .publication
            .payload,
        "original frozen final"
    );
    assert!(
        state
            .coordination_finish_completion(
                parent.stamp,
                runtime,
                final_fact.delivery_id,
                "duplicate"
            )
            .await?
            .is_none()
    );
    Ok(())
}

#[tokio::test]
async fn explicit_input_reuses_work_and_atomically_supersedes_only_undispatched_wait()
-> anyhow::Result<()> {
    let (_dir, state, root, runtime) = fixture().await?;
    let caller = admit(&state, root, root, runtime, &[]).await?;
    let child = admit(&state, root, ThreadId::new(), runtime, &[root]).await?;
    let ticket = state
        .coordination_register_wait(
            caller.stamp,
            runtime,
            &cell(),
            WaitMode::Any,
            &[child.stamp.actor_id],
        )
        .await?;
    let message = input(caller.stamp, Uuid::now_v7());
    let continued = state
        .coordination_admit_input(
            &message,
            runtime,
            false,
            Some(caller.stamp),
            Some(&ticket),
            &[],
            None,
        )
        .await?;
    assert_eq!(continued.stamp, caller.stamp);
    assert!(state.coordination_current_wait(root, root).await?.is_none());
    let closed = state
        .coordination_observe_wait(caller.stamp, ticket.id)
        .await?
        .unwrap();
    assert_eq!(
        closed.cancellation,
        Some(
            crate::coordination_wait::WaitCancellation::InputSuperseded {
                delivery_id: message.id
            }
        )
    );
    assert_eq!(closed.origin, ticket.origin);
    let retained = state
        .coordination_delivery(root, root, message.id)
        .await?
        .unwrap();
    assert_eq!(retained.dispatch_id, Some(message.id));
    assert_eq!(retained.accepted_receipt, None);
    // A steer cannot manufacture work for an unknown/idle actor.
    let idle = WorkStamp {
        root_id: root,
        actor_id: ThreadId::new(),
        work_id: Uuid::now_v7(),
    };
    assert!(
        state
            .coordination_admit_input(
                &input(idle, idle.work_id),
                runtime,
                false,
                None,
                None,
                &[],
                None
            )
            .await
            .is_err()
    );
    assert!(state.coordination_work(idle).await?.is_none());
    Ok(())
}

#[tokio::test]
async fn known_refusal_and_unconfirmed_dispatch_are_not_accepted_or_consumed() -> anyhow::Result<()>
{
    let (_dir, state, root, runtime) = fixture().await?;
    let work = admit(&state, root, root, runtime, &[]).await?;
    let id = work.stamp.work_id;
    let pending = state.coordination_delivery(root, root, id).await?.unwrap();
    assert_eq!(pending.accepted_receipt, None);
    assert_eq!(pending.refused_reason, None);
    state
        .coordination_refuse_delivery(root, root, id, "native admission refused")
        .await?;
    let refused = state.coordination_delivery(root, root, id).await?.unwrap();
    assert_eq!(
        refused.refused_reason.as_deref(),
        Some("native admission refused")
    );
    assert!(
        state
            .coordination_confirm_delivery(root, root, &[id], id, "input")
            .await
            .is_err()
    );
    assert!(
        state
            .coordination_consume_sampling(root, root, &[id])
            .await
            .is_err()
    );
    assert!(
        !state
            .coordination_delivery(root, root, id)
            .await?
            .unwrap()
            .consumed
    );
    Ok(())
}

#[tokio::test]
async fn completion_waits_for_capture_and_transitive_active_work_including_late_admission()
-> anyhow::Result<()> {
    let (_dir, state, root, runtime) = fixture().await?;
    let parent = admit(&state, root, root, runtime, &[]).await?;
    let child = admit(&state, root, ThreadId::new(), runtime, &[root]).await?;
    let paused = admit(&state, root, ThreadId::new(), runtime, &[root]).await?;
    state
        .coordination_transition_work(
            paused.stamp,
            paused.revision,
            runtime,
            WorkDisposition::Paused,
        )
        .await?;
    state
        .coordination_record_answer(parent.stamp, runtime, "native-message-1", "first")
        .await?;
    state
        .coordination_record_answer(parent.stamp, runtime, "empty-partial", "")
        .await?;
    state
        .coordination_record_answer(parent.stamp, runtime, "native-message-2", "second")
        .await?;
    state
        .coordination_record_answer(parent.stamp, runtime, "empty-final", "")
        .await?;
    state
        .coordination_record_answer(parent.stamp, runtime, "native-message-1", "first")
        .await?;
    let candidate = state
        .coordination_stage_completion(
            parent.stamp,
            runtime,
            "parent-turn",
            root,
            WorkDisposition::Completed,
            None,
            &[child.stamp.actor_id, paused.stamp.actor_id],
        )
        .await?
        .unwrap();
    assert_eq!(candidate.answers, vec!["first", "second"]);
    assert!(!candidate.capture_ready);
    assert!(
        state
            .coordination_finish_completion(
                parent.stamp,
                runtime,
                candidate.delivery_id,
                "complete payload"
            )
            .await?
            .is_none()
    );
    state
        .coordination_complete_capture(parent.stamp, candidate.delivery_id, Some("frozen capture"))
        .await?;
    assert!(
        state
            .coordination_finish_completion(
                parent.stamp,
                runtime,
                candidate.delivery_id,
                "complete payload"
            )
            .await?
            .is_none()
    );
    // A descendant admitted after the completion attempt joins only affected guards.
    let late = admit(
        &state,
        root,
        ThreadId::new(),
        runtime,
        &[child.stamp.actor_id, root],
    )
    .await?;
    let child_terminal = state
        .coordination_stage_completion(
            child.stamp,
            runtime,
            "child-turn",
            root,
            WorkDisposition::Failed,
            Some("failure"),
            &[],
        )
        .await?
        .unwrap();
    state
        .coordination_finish_completion(
            child.stamp,
            runtime,
            child_terminal.delivery_id,
            "failure payload",
        )
        .await?
        .unwrap();
    assert!(
        state
            .coordination_finish_completion(
                parent.stamp,
                runtime,
                candidate.delivery_id,
                "complete payload"
            )
            .await?
            .is_none()
    );
    assert_eq!(
        state.coordination_completion_dependents(late.stamp).await?,
        vec![parent.stamp]
    );
    let late_terminal = state
        .coordination_stage_completion(
            late.stamp,
            runtime,
            "late-turn",
            child.stamp.actor_id,
            WorkDisposition::Interrupted,
            None,
            &[],
        )
        .await?
        .unwrap();
    state
        .coordination_finish_completion(
            late.stamp,
            runtime,
            late_terminal.delivery_id,
            "interruption payload",
        )
        .await?
        .unwrap();
    let completed = state
        .coordination_finish_completion(
            parent.stamp,
            runtime,
            candidate.delivery_id,
            "ordered frozen final payload",
        )
        .await?
        .unwrap();
    assert_eq!(completed.disposition, WorkDisposition::Completed);
    let result = state
        .coordination_terminal_delivery(parent.stamp)
        .await?
        .unwrap();
    assert_eq!(result.publication.payload, "ordered frozen final payload");
    assert!(
        state
            .coordination_pending_completion(parent.stamp)
            .await?
            .is_none()
    );
    let remaining_answers: i64 =
        sqlx::query_scalar("SELECT count(*) FROM agent_work_answers WHERE work_id=?")
            .bind(parent.stamp.work_id.to_string())
            .fetch_one(state.pool.as_ref())
            .await?;
    assert_eq!(remaining_answers, 0);
    Ok(())
}

#[tokio::test]
async fn native_turn_binding_cannot_relabel_new_work_or_auxiliary_reset() -> anyhow::Result<()> {
    let (_dir, state, root, runtime) = fixture().await?;
    let first = admit(&state, root, root, runtime, &[]).await?;
    state
        .coordination_bind_turn(first.stamp, runtime, "actual-turn")
        .await?;
    assert_eq!(
        state
            .coordination_work_for_turn(root, root, "actual-turn")
            .await?,
        Some(first.stamp)
    );
    assert_eq!(
        state
            .coordination_work_for_turn(root, root, "auxiliary-reset")
            .await?,
        None
    );
    let final_fact = state
        .coordination_stage_completion(
            first.stamp,
            runtime,
            "actual-turn",
            root,
            WorkDisposition::Interrupted,
            None,
            &[],
        )
        .await?
        .unwrap();
    state
        .coordination_finish_completion(first.stamp, runtime, final_fact.delivery_id, "interrupted")
        .await?
        .unwrap();
    let next = WorkStamp {
        root_id: root,
        actor_id: root,
        work_id: Uuid::now_v7(),
    };
    state
        .coordination_admit_input(
            &input(next, next.work_id),
            runtime,
            true,
            Some(first.stamp),
            None,
            &[],
            None,
        )
        .await?;
    assert!(
        state
            .coordination_bind_turn(next, runtime, "actual-turn")
            .await
            .is_err()
    );
    assert_eq!(
        state
            .coordination_work_for_turn(root, root, "actual-turn")
            .await?,
        Some(first.stamp)
    );
    assert!(
        !state.coordination_has_pending_messages(root, root).await?,
        "root completion is not self-addressed wake input"
    );
    Ok(())
}

#[tokio::test]
async fn idle_assignment_gets_new_identity_and_atomically_settles_superseded_work()
-> anyhow::Result<()> {
    let (_dir, state, root, runtime) = fixture().await?;
    let previous = admit(&state, root, root, runtime, &[]).await?;
    let child = admit(&state, root, ThreadId::new(), runtime, &[root]).await?;
    let wait = state
        .coordination_register_wait(
            previous.stamp,
            runtime,
            &cell(),
            WaitMode::All,
            &[child.stamp.actor_id],
        )
        .await?;
    let next = WorkStamp {
        root_id: root,
        actor_id: root,
        work_id: Uuid::now_v7(),
    };
    let terminal = PublishDelivery {
        id: Uuid::now_v7(),
        root_id: root,
        sender_id: Some(root),
        recipient_id: root,
        work: Some(previous.stamp),
        kind: DeliveryKind::Terminal,
        payload: "superseded work".to_owned(),
    };
    let accepted = state
        .coordination_admit_input(
            &input(next, next.work_id),
            runtime,
            true,
            Some(previous.stamp),
            Some(&wait),
            &[],
            Some(&terminal),
        )
        .await?;
    assert_eq!(accepted.stamp, next);
    assert_ne!(accepted.stamp, previous.stamp);
    assert_eq!(
        state
            .coordination_work(previous.stamp)
            .await?
            .unwrap()
            .disposition,
        WorkDisposition::Interrupted
    );
    assert_eq!(
        state
            .coordination_terminal_delivery(previous.stamp)
            .await?
            .unwrap()
            .publication
            .id,
        terminal.id
    );
    assert!(state.coordination_current_wait(root, root).await?.is_none());
    Ok(())
}

#[tokio::test]
async fn durable_messages_do_not_leak_into_ordinary_input_during_wait() -> anyhow::Result<()> {
    let (_dir, state, root, runtime) = fixture().await?;
    let caller = admit(&state, root, root, runtime, &[]).await?;
    let child = admit(&state, root, ThreadId::new(), runtime, &[root]).await?;
    let ticket = state
        .coordination_register_wait(
            caller.stamp,
            runtime,
            &cell(),
            WaitMode::Any,
            &[child.stamp.actor_id],
        )
        .await?;
    let message = PublishDelivery {
        id: Uuid::now_v7(),
        root_id: root,
        sender_id: Some(child.stamp.actor_id),
        recipient_id: root,
        work: None,
        kind: DeliveryKind::Message,
        payload: "retained attributed message".to_owned(),
    };
    state.coordination_publish_delivery(&message).await?;
    assert!(!state.coordination_has_pending_messages(root, root).await?);
    assert!(
        state
            .coordination_take_messages(root, root)
            .await?
            .is_empty()
    );
    let claim = state
        .coordination_claim_ready_wait(caller.stamp, runtime, ticket.id)
        .await?
        .unwrap();
    assert_eq!(claim.delivery_ids, vec![message.id]);
    assert!(
        state
            .coordination_consume_sampling(root, root, &[message.id])
            .await?
            .is_empty()
    );
    assert!(
        !state
            .coordination_delivery(root, root, message.id)
            .await?
            .unwrap()
            .consumed
    );
    state
        .coordination_cancel_wait(
            caller.stamp,
            runtime,
            ticket.id,
            ticket.origin.cell().unwrap(),
        )
        .await?;
    assert!(state.coordination_has_pending_messages(root, root).await?);
    let ordinary = state.coordination_take_messages(root, root).await?;
    assert_eq!(ordinary.len(), 1);
    assert_eq!(ordinary[0].publication.id, message.id);
    assert!(!ordinary[0].consumed);
    state
        .coordination_consume_sampling(root, root, &[message.id])
        .await?;
    assert!(!state.coordination_has_pending_messages(root, root).await?);
    Ok(())
}

#[tokio::test]
async fn completion_feedback_dispatch_rechecks_active_work_and_retains_one_input()
-> anyhow::Result<()> {
    for receipt in [None, Some("guard-turn")] {
        let (_dir, state, root, runtime) = fixture().await?;
        let parent = admit(&state, root, root, runtime, &[]).await?;
        let child = admit(&state, root, ThreadId::new(), runtime, &[root]).await?;
        let nested = admit(
            &state,
            root,
            ThreadId::new(),
            runtime,
            &[root, child.stamp.actor_id],
        )
        .await?;
        state
            .coordination_record_answer(parent.stamp, runtime, "original-answer", "original final")
            .await?;
        let candidate = state
            .coordination_stage_completion(
                parent.stamp,
                runtime,
                "final-turn",
                root,
                WorkDisposition::Completed,
                None,
                &[child.stamp.actor_id, nested.stamp.actor_id],
            )
            .await?
            .unwrap();
        state
            .coordination_completion_settlement(parent.stamp, runtime, candidate.delivery_id, false)
            .await?;
        assert!(
            state
                .coordination_completion_blockers(parent.stamp, runtime, candidate.delivery_id)
                .await?
                .is_none()
        );
        state
            .coordination_complete_capture(parent.stamp, candidate.delivery_id, None)
            .await?;
        state
            .coordination_completion_settlement(parent.stamp, runtime, candidate.delivery_id, true)
            .await?;
        assert!(
            state
                .coordination_completion_blockers(
                    parent.stamp,
                    Uuid::now_v7(),
                    candidate.delivery_id
                )
                .await?
                .is_none(),
            "cold ownership cannot dispatch a guard"
        );
        let before = state
            .coordination_completion_blockers(parent.stamp, runtime, candidate.delivery_id)
            .await?
            .unwrap();
        assert_eq!(before.len(), 2);
        state
            .coordination_transition_work(
                child.stamp,
                child.revision,
                runtime,
                WorkDisposition::Paused,
            )
            .await?;
        let publication = input(parent.stamp, Uuid::now_v7());
        assert!(
            !state
                .coordination_begin_completion_continuation(
                    parent.stamp,
                    runtime,
                    candidate.delivery_id,
                    &before,
                    &publication,
                    receipt
                )
                .await?,
            "stale feedback cannot name a stopped descendant"
        );
        assert!(
            state
                .coordination_delivery(root, root, publication.id)
                .await?
                .is_none()
        );
        let remaining = state
            .coordination_completion_blockers(parent.stamp, runtime, candidate.delivery_id)
            .await?
            .unwrap();
        assert_eq!(remaining, vec![nested.stamp]);
        assert!(
            state
                .coordination_begin_completion_continuation(
                    parent.stamp,
                    runtime,
                    candidate.delivery_id,
                    &remaining,
                    &publication,
                    receipt
                )
                .await?
        );
        assert!(
            !state
                .coordination_begin_completion_continuation(
                    parent.stamp,
                    runtime,
                    candidate.delivery_id,
                    &remaining,
                    &publication,
                    receipt
                )
                .await?,
            "one candidate admits only one guard input"
        );
        let delivered = state
            .coordination_delivery(root, root, publication.id)
            .await?
            .unwrap();
        assert_eq!(delivered.dispatch_id, Some(publication.id));
        assert_eq!(delivered.accepted_receipt.as_deref(), receipt);
        assert!(!delivered.consumed);
        assert!(
            state
                .coordination_pending_completion(parent.stamp)
                .await?
                .is_none()
        );
        assert_eq!(
            state
                .coordination_current_work(root, root)
                .await?
                .unwrap()
                .stamp,
            parent.stamp
        );
        if let Some(receipt) = receipt {
            assert_eq!(
                state
                    .coordination_work_for_turn(root, root, receipt)
                    .await?,
                Some(parent.stamp)
            );
        }
        let next = state
            .coordination_stage_completion(
                parent.stamp,
                runtime,
                "later-final",
                root,
                WorkDisposition::Completed,
                None,
                &[nested.stamp.actor_id],
            )
            .await?
            .unwrap();
        assert_eq!(
            next.answers,
            vec!["original final"],
            "guard feedback does not discard ordered ordinary answers"
        );
    }
    Ok(())
}

#[tokio::test]
async fn stopped_blockers_before_feedback_dispatch_preserve_original_final() -> anyhow::Result<()> {
    for disposition in [WorkDisposition::Paused, WorkDisposition::Uncertain] {
        let (_dir, state, root, runtime) = fixture().await?;
        let parent = admit(&state, root, root, runtime, &[]).await?;
        let child = admit(&state, root, ThreadId::new(), runtime, &[root]).await?;
        let candidate = state
            .coordination_stage_completion(
                parent.stamp,
                runtime,
                "final-turn",
                root,
                WorkDisposition::Completed,
                None,
                &[child.stamp.actor_id],
            )
            .await?
            .unwrap();
        state
            .coordination_complete_capture(
                parent.stamp,
                candidate.delivery_id,
                Some("frozen original capture"),
            )
            .await?;
        let selected = state
            .coordination_completion_blockers(parent.stamp, runtime, candidate.delivery_id)
            .await?
            .unwrap();
        state
            .coordination_transition_work(child.stamp, child.revision, runtime, disposition)
            .await?;
        let publication = input(parent.stamp, Uuid::now_v7());
        assert!(
            !state
                .coordination_begin_completion_continuation(
                    parent.stamp,
                    runtime,
                    candidate.delivery_id,
                    &selected,
                    &publication,
                    Some("unused-turn")
                )
                .await?
        );
        assert!(
            state
                .coordination_completion_blockers(parent.stamp, runtime, candidate.delivery_id)
                .await?
                .unwrap()
                .is_empty()
        );
        assert!(
            state
                .coordination_delivery(root, root, publication.id)
                .await?
                .is_none()
        );
        state
            .coordination_finish_completion(
                parent.stamp,
                runtime,
                candidate.delivery_id,
                "original final with frozen capture",
            )
            .await?
            .unwrap();
        assert_eq!(
            state
                .coordination_terminal_delivery(parent.stamp)
                .await?
                .unwrap()
                .publication
                .id,
            candidate.delivery_id
        );
        assert_eq!(
            state
                .coordination_work(child.stamp)
                .await?
                .unwrap()
                .disposition,
            disposition
        );
    }
    Ok(())
}

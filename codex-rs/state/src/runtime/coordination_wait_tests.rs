use super::*;
use crate::coordination::DeliveryKind;
use crate::coordination::PublishDelivery;
use crate::coordination::WorkDisposition;
use codex_utils_absolute_path::test_support::PathExt;
use pretty_assertions::assert_eq;
use std::sync::Arc;
use tempfile::TempDir;

struct Fixture {
    directory: TempDir,
    state: Arc<StateRuntime>,
    runtime: Uuid,
    caller: WorkStamp,
}

impl Fixture {
    async fn new() -> anyhow::Result<Self> {
        let directory = TempDir::new()?;
        let state = StateRuntime::init(
            crate::SqliteConfig::new_for_testing(directory.path().abs()),
            "test-provider".to_owned(),
        )
        .await?;
        let root = ThreadId::new();
        let caller = WorkStamp {
            root_id: root,
            actor_id: root,
            work_id: Uuid::now_v7(),
        };
        let fixture = Self {
            directory,
            state,
            runtime: Uuid::now_v7(),
            caller,
        };
        fixture.admit(caller, None).await?;
        Ok(fixture)
    }

    async fn admit(&self, stamp: WorkStamp, previous: Option<WorkStamp>) -> anyhow::Result<()> {
        let input = PublishDelivery {
            id: stamp.work_id,
            root_id: stamp.root_id,
            sender_id: None,
            recipient_id: stamp.actor_id,
            work: Some(stamp),
            kind: DeliveryKind::Message,
            payload: "accepted input, not queued composer input".to_owned(),
        };
        self.state
            .coordination_admit_input(&input, self.runtime, true, previous, None, &[], None)
            .await?;
        // Exercise the actual input acceptance path. An initial assignment is
        // already consumed by the time that assignment invokes a wait.
        let dispatch = input.id;
        self.state
            .coordination_confirm_delivery(
                stamp.root_id,
                stamp.actor_id,
                &[input.id],
                dispatch,
                "input",
            )
            .await?;
        self.state
            .coordination_consume_deliveries(stamp.root_id, stamp.actor_id, &[input.id], "input")
            .await?;
        Ok(())
    }

    async fn child(&self) -> anyhow::Result<WorkStamp> {
        let stamp = WorkStamp {
            root_id: self.caller.root_id,
            actor_id: ThreadId::new(),
            work_id: Uuid::now_v7(),
        };
        self.admit(stamp, None).await?;
        Ok(stamp)
    }

    async fn terminal(
        &self,
        stamp: WorkStamp,
        recipient: ThreadId,
        disposition: WorkDisposition,
    ) -> anyhow::Result<Uuid> {
        let candidate = self
            .state
            .coordination_stage_completion(
                stamp,
                self.runtime,
                "terminal-turn",
                recipient,
                disposition,
                None,
                &[],
            )
            .await?
            .unwrap();
        if disposition == WorkDisposition::Completed {
            self.state
                .coordination_complete_capture(stamp, candidate.delivery_id, None)
                .await?;
        }
        self.state
            .coordination_completion_settlement(stamp, self.runtime, candidate.delivery_id, true)
            .await?;
        self.state
            .coordination_finish_completion(
                stamp,
                self.runtime,
                candidate.delivery_id,
                "one prepared immutable terminal payload",
            )
            .await?;
        Ok(candidate.delivery_id)
    }

    async fn message(&self) -> anyhow::Result<Uuid> {
        let publication = PublishDelivery {
            id: Uuid::now_v7(),
            root_id: self.caller.root_id,
            sender_id: Some(ThreadId::new()),
            recipient_id: self.caller.actor_id,
            work: None,
            kind: DeliveryKind::Message,
            payload: "addressed message with frozen source payload".to_owned(),
        };
        self.state
            .coordination_publish_delivery(&publication)
            .await?;
        Ok(publication.id)
    }

    async fn register(
        &self,
        mode: WaitMode,
        descendants: &[ThreadId],
    ) -> anyhow::Result<WaitTicket> {
        self.state
            .coordination_register_wait(self.caller, self.runtime, &cell(), mode, descendants)
            .await
    }

    async fn claim(&self, ticket: &WaitTicket) -> anyhow::Result<Option<WaitClaim>> {
        self.state
            .coordination_claim_ready_wait(self.caller, self.runtime, ticket.id)
            .await
    }

    async fn consume(&self, ticket: &WaitTicket) -> anyhow::Result<()> {
        let claim = self
            .state
            .coordination_observe_wait(self.caller, ticket.id)
            .await?
            .unwrap()
            .claim
            .unwrap();
        self.state
            .coordination_consume_awaited_wait(
                self.caller,
                self.runtime,
                ticket.id,
                claim.id,
                Uuid::now_v7(),
            )
            .await
    }
}

fn cell() -> WaitCell {
    WaitCell {
        turn_id: "original-turn".to_owned(),
        item_id: Uuid::now_v7().to_string(),
        window_id: "original-window".to_owned(),
        cell_id: "native-cell".to_owned(),
    }
}

#[tokio::test]
async fn awaited_wait_claims_registered_selection_without_dispatch_and_consumes_exact_receipt()
-> anyhow::Result<()> {
    let f = Fixture::new().await?;
    let child = f.child().await?;
    let wait = f.register(WaitMode::All, &[child.actor_id]).await?;
    assert_eq!(wait.phase, WaitPhase::Registered);
    let facts = f
        .state
        .coordination_observe(f.caller.root_id, &[f.caller.actor_id])
        .await?;
    let observed = facts[0].wait.as_ref().unwrap();
    assert_eq!(observed.reason, None);
    assert_eq!(observed.timeout_ms, None);
    assert_eq!(observed.deadline_unix_ms, None);
    assert!(f.claim(&wait).await?.is_none());
    let message = f.message().await?;
    let claim = f.claim(&wait).await?.unwrap();
    assert_eq!(claim.delivery_ids, vec![message]);
    let facts = f
        .state
        .coordination_observe(f.caller.root_id, &[f.caller.actor_id])
        .await?;
    assert_eq!(
        facts[0].work.as_ref().unwrap().disposition,
        WorkDisposition::Running
    );
    assert_eq!(facts[0].wait.as_ref().unwrap().phase, WaitPhase::Claimed);
    assert!(
        !f.state
            .coordination_delivery(f.caller.root_id, f.caller.actor_id, message)
            .await?
            .unwrap()
            .consumed
    );
    let context = Uuid::now_v7();
    assert!(
        f.state
            .coordination_consume_awaited_wait(
                f.caller,
                f.runtime,
                wait.id,
                Uuid::now_v7(),
                context,
            )
            .await
            .is_err()
    );
    let retained = f
        .state
        .coordination_observe_wait(f.caller, wait.id)
        .await?
        .unwrap();
    assert_eq!(retained.phase, WaitPhase::Claimed);
    assert!(retained.dispatch_id.is_none());
    for _ in 0..2 {
        f.state
            .coordination_consume_awaited_wait(f.caller, f.runtime, wait.id, claim.id, context)
            .await?;
    }
    assert_eq!(
        f.state
            .coordination_observe_wait(f.caller, wait.id)
            .await?
            .unwrap()
            .phase,
        WaitPhase::Consumed
    );
    assert_eq!(
        f.state
            .coordination_work(f.caller)
            .await?
            .unwrap()
            .disposition,
        WorkDisposition::Running
    );
    assert!(
        f.state
            .coordination_delivery(f.caller.root_id, f.caller.actor_id, message)
            .await?
            .unwrap()
            .consumed
    );
    Ok(())
}

#[tokio::test]
async fn passive_wait_observation_retains_provider_reason_and_captured_deadline()
-> anyhow::Result<()> {
    let f = Fixture::new().await?;
    f.state
        .coordination_bind_turn(f.caller, f.runtime, "provider-turn")
        .await?;
    let wait = f
        .state
        .coordination_commit_turn_wait(
            f.caller,
            f.runtime,
            "provider-turn",
            TurnWaitReason::ProviderCapacity,
            Some(5000),
            1000,
            &[],
        )
        .await?
        .unwrap();
    let facts = f
        .state
        .coordination_observe(f.caller.root_id, &[f.caller.actor_id])
        .await?;
    let observed = facts[0].wait.as_ref().unwrap();
    assert_eq!(observed.id, wait.id);
    assert_eq!(observed.reason, Some(TurnWaitReason::ProviderCapacity));
    assert_eq!(observed.deadline_unix_ms, Some(5000));
    assert_eq!(observed.timeout_ms, None);
    Ok(())
}

#[tokio::test]
async fn turn_recovery_requires_binding_and_fresh_unread_outcomes() -> anyhow::Result<()> {
    let f = Fixture::new().await?;
    let child = f.child().await?;
    assert!(
        f.state
            .coordination_commit_turn_wait(
                f.caller,
                f.runtime,
                "unbound",
                TurnWaitReason::DescendantRecovery,
                None,
                1000,
                &[child.actor_id]
            )
            .await
            .is_err()
    );
    f.state
        .coordination_bind_turn(f.caller, f.runtime, "failed-request")
        .await?;
    let ticket = f
        .state
        .coordination_commit_turn_wait(
            f.caller,
            f.runtime,
            "failed-request",
            TurnWaitReason::DescendantRecovery,
            None,
            1000,
            &[child.actor_id],
        )
        .await?
        .unwrap();
    assert!(ticket.origin.cell().is_none());
    assert_eq!(ticket.reset, WaitResetState::NotRequested);
    assert_eq!(ticket.options, WaitOptions::default());
    let delivery = f
        .terminal(child, f.caller.actor_id, WorkDisposition::Failed)
        .await?;
    let dispatch = Uuid::now_v7();
    f.state
        .coordination_begin_delivery(
            f.caller.root_id,
            f.caller.actor_id,
            &[delivery],
            None,
            dispatch,
        )
        .await?;
    f.state
        .coordination_confirm_delivery(
            f.caller.root_id,
            f.caller.actor_id,
            &[delivery],
            dispatch,
            "other",
        )
        .await?;
    f.state
        .coordination_consume_deliveries(f.caller.root_id, f.caller.actor_id, &[delivery], "other")
        .await?;
    assert!(
        f.claim(&ticket).await?.is_none(),
        "status alone cannot retry an exhausted request"
    );
    let message = f.message().await?;
    assert_eq!(f.claim(&ticket).await?.unwrap().delivery_ids, vec![message]);
    Ok(())
}

#[tokio::test]
async fn recovery_observes_nested_result_once_and_unread_stopped_sibling() -> anyhow::Result<()> {
    let f = Fixture::new().await?;
    let branch = f.child().await?;
    let leaf = f.child().await?;
    let active = f.child().await?;
    f.terminal(branch, f.caller.actor_id, WorkDisposition::Interrupted)
        .await?;
    let nested = f
        .terminal(leaf, branch.actor_id, WorkDisposition::Completed)
        .await?;
    f.state
        .coordination_bind_turn(f.caller, f.runtime, "failed-request")
        .await?;
    let descendants = [branch.actor_id, leaf.actor_id, active.actor_id];
    let ticket = f
        .state
        .coordination_commit_turn_wait(
            f.caller,
            f.runtime,
            "failed-request",
            TurnWaitReason::DescendantRecovery,
            None,
            1000,
            &descendants,
        )
        .await?
        .unwrap();
    assert_eq!(f.claim(&ticket).await?.unwrap().outcomes[0].stamp, branch);
    f.consume(&ticket).await?;
    f.state
        .coordination_bind_turn(f.caller, f.runtime, "sampling-input")
        .await?;
    let next = f
        .state
        .coordination_commit_turn_wait(
            f.caller,
            f.runtime,
            "sampling-input",
            TurnWaitReason::DescendantRecovery,
            None,
            1000,
            &descendants,
        )
        .await?
        .unwrap();
    let claim = f.claim(&next).await?.unwrap();
    assert_eq!(claim.outcomes[0].stamp, leaf);
    assert!(claim.delivery_ids.is_empty());
    f.consume(&next).await?;
    assert!(
        !f.state
            .coordination_delivery(f.caller.root_id, branch.actor_id, nested)
            .await?
            .unwrap()
            .consumed
    );
    f.state
        .coordination_bind_turn(f.caller, f.runtime, "next-failure")
        .await?;
    let last = f
        .state
        .coordination_commit_turn_wait(
            f.caller,
            f.runtime,
            "next-failure",
            TurnWaitReason::DescendantRecovery,
            None,
            1000,
            &descendants,
        )
        .await?
        .unwrap();
    assert_eq!(last.selected, vec![active]);
    assert!(f.claim(&last).await?.is_none());
    Ok(())
}

#[tokio::test]
async fn capacity_deadlines_ignore_mail_and_fallback_is_work_scoped() -> anyhow::Result<()> {
    let f = Fixture::new().await?;
    let message = f.message().await?;
    let turn = "original-capacity-turn";
    f.state
        .coordination_bind_turn(f.caller, f.runtime, turn)
        .await?;
    for seconds in [1, 2, 4, 8, 16, 30, 30] {
        let ticket = f
            .state
            .coordination_commit_turn_wait(
                f.caller,
                f.runtime,
                turn,
                TurnWaitReason::ProviderCapacity,
                None,
                1000,
                &[],
            )
            .await?
            .unwrap();
        assert_eq!(ticket.deadline_unix_ms, Some(1000 + seconds * 1000));
        assert_eq!(
            f.state
                .coordination_commit_turn_wait(
                    f.caller,
                    f.runtime,
                    turn,
                    TurnWaitReason::ProviderCapacity,
                    None,
                    9999,
                    &[]
                )
                .await?,
            Some(ticket.clone())
        );
        assert!(
            f.claim(&ticket).await?.is_none(),
            "messages do not bypass provider timing"
        );
        let claim = f
            .state
            .coordination_claim_ready_wait_at(
                f.caller,
                f.runtime,
                ticket.id,
                WaitReadiness { deadline_due: true },
            )
            .await?
            .unwrap();
        assert_eq!(claim.wake, WaitWake::Deadline);
        assert!(claim.delivery_ids.is_empty());
        assert!(claim.outcomes.is_empty());
        f.state
            .coordination_consume_awaited_wait(
                f.caller,
                f.runtime,
                ticket.id,
                claim.id,
                Uuid::now_v7(),
            )
            .await?;
    }
    assert!(
        !f.state
            .coordination_delivery(f.caller.root_id, f.caller.actor_id, message)
            .await?
            .unwrap()
            .consumed
    );
    assert!(
        f.state
            .coordination_provider_recovered(f.caller, f.runtime, "unbound")
            .await
            .is_err()
    );
    f.state
        .coordination_provider_recovered(f.caller, f.runtime, turn)
        .await?;
    let ticket = f
        .state
        .coordination_commit_turn_wait(
            f.caller,
            f.runtime,
            turn,
            TurnWaitReason::ProviderCapacity,
            None,
            9000,
            &[],
        )
        .await?
        .unwrap();
    assert_eq!(ticket.deadline_unix_ms, Some(10000));
    Ok(())
}

#[tokio::test]
async fn untimed_usage_pause_has_no_automatic_wake_and_timing_advice_wins() -> anyhow::Result<()> {
    let f = Fixture::new().await?;
    f.message().await?;
    f.state
        .coordination_bind_turn(f.caller, f.runtime, "usage")
        .await?;
    let ticket = f
        .state
        .coordination_commit_turn_wait(
            f.caller,
            f.runtime,
            "usage",
            TurnWaitReason::ProviderUsageLimit,
            None,
            1000,
            &[],
        )
        .await?
        .unwrap();
    assert_eq!(
        f.state
            .coordination_work(f.caller)
            .await?
            .unwrap()
            .disposition,
        WorkDisposition::Paused
    );
    assert!(
        f.state
            .coordination_claim_ready_wait_at(
                f.caller,
                f.runtime,
                ticket.id,
                WaitReadiness { deadline_due: true }
            )
            .await?
            .is_none()
    );
    f.state
        .coordination_cancel_wait_origin(f.caller, f.runtime, ticket.id, &ticket.origin)
        .await?;
    f.state
        .coordination_bind_turn(f.caller, f.runtime, "explicit-retry")
        .await?;
    let timed = f
        .state
        .coordination_commit_turn_wait(
            f.caller,
            f.runtime,
            "explicit-retry",
            TurnWaitReason::ProviderCapacity,
            Some(500),
            1000,
            &[],
        )
        .await?
        .unwrap();
    assert_eq!(
        timed.deadline_unix_ms,
        Some(500),
        "expired advice is not missing advice"
    );
    assert!(
        f.claim(&timed).await?.is_none(),
        "wall time is not live wake authority"
    );
    assert_eq!(
        f.state
            .coordination_claim_ready_wait_at(
                f.caller,
                f.runtime,
                timed.id,
                WaitReadiness { deadline_due: true }
            )
            .await?
            .unwrap()
            .wake,
        WaitWake::Deadline
    );
    Ok(())
}

#[tokio::test]
async fn explicit_deadline_obeys_reset_barrier_and_addressed_message_priority() -> anyhow::Result<()>
{
    let f = Fixture::new().await?;
    let child = f.child().await?;
    let options = WaitOptions {
        mode: WaitMode::All,
        compact: true,
        timeout: Some(crate::coordination_wait::WaitTimeout {
            requested_ms: 0,
            effective_ms: 0,
        }),
        ..Default::default()
    };
    let ticket = f
        .state
        .coordination_register_wait_at(
            f.caller,
            f.runtime,
            &cell(),
            &options,
            &[child.actor_id],
            1000,
        )
        .await?;

    let due = WaitReadiness { deadline_due: true };
    assert!(
        f.state
            .coordination_claim_ready_wait_at(f.caller, f.runtime, ticket.id, due)
            .await?
            .is_none()
    );
    assert!(
        f.state
            .coordination_begin_wait_reset(
                f.caller,
                f.runtime,
                ticket.id,
                ticket.origin.cell().unwrap()
            )
            .await?
    );
    let message = f.message().await?;
    assert!(
        f.state
            .coordination_claim_ready_wait_at(f.caller, f.runtime, ticket.id, due)
            .await?
            .is_none()
    );
    f.state
        .coordination_finish_wait_reset(
            f.caller,
            f.runtime,
            ticket.id,
            ticket.origin.cell().unwrap(),
            Some("reset failed"),
        )
        .await?;
    let claim = f
        .state
        .coordination_claim_ready_wait_at(f.caller, f.runtime, ticket.id, due)
        .await?
        .unwrap();
    assert_eq!(claim.wake, WaitWake::Events);
    assert_eq!(claim.delivery_ids, vec![message]);
    assert!(claim.outcomes.is_empty());
    f.consume(&ticket).await?;
    let timeout = f
        .state
        .coordination_register_wait_at(
            f.caller,
            f.runtime,
            &cell(),
            &WaitOptions {
                compact: false,
                ..options
            },
            &[child.actor_id],
            1000,
        )
        .await?;

    let expired = f
        .state
        .coordination_claim_ready_wait_at(f.caller, f.runtime, timeout.id, due)
        .await?
        .unwrap();
    assert_eq!(expired.wake, WaitWake::Deadline);
    assert!(expired.outcomes.is_empty());
    assert!(expired.delivery_ids.is_empty());
    Ok(())
}

#[tokio::test]
async fn terminal_work_retires_wait_without_releasing_uncertain_dispatch_receipts()
-> anyhow::Result<()> {
    let f = Fixture::new().await?;
    let message = f.message().await?;
    let ticket = f.register(WaitMode::Any, &[]).await?;

    let claim = f.claim(&ticket).await?.unwrap();
    let dispatch = Uuid::now_v7();
    // Retained pre-native-await receipt evidence remains readable and inert.
    sqlx::query("UPDATE agent_waits SET phase='accepted',dispatch_id=?,accepted_receipt='accepted-not-sampled' WHERE wait_id=?")
        .bind(dispatch.to_string())
        .bind(ticket.id.to_string())
        .execute(f.state.pool.as_ref())
        .await?;
    sqlx::query("UPDATE agent_deliveries SET dispatch_id=?,accepted_receipt='accepted-not-sampled' WHERE delivery_id=?")
        .bind(dispatch.to_string())
        .bind(message.to_string())
        .execute(f.state.pool.as_ref())
        .await?;
    let final_fact = f
        .state
        .coordination_stage_completion(
            f.caller,
            f.runtime,
            "accepted-not-sampled",
            f.caller.actor_id,
            WorkDisposition::Interrupted,
            None,
            &[],
        )
        .await?
        .unwrap();
    f.state
        .coordination_finish_completion(f.caller, f.runtime, final_fact.delivery_id, "interrupted")
        .await?
        .unwrap();
    assert!(
        f.state
            .coordination_current_wait(f.caller.root_id, f.caller.actor_id)
            .await?
            .is_none()
    );
    let retained = f
        .state
        .coordination_observe_wait(f.caller, ticket.id)
        .await?
        .unwrap();
    assert_eq!(retained.phase, WaitPhase::Cancelled);
    assert_eq!(retained.claim, Some(claim));
    assert_eq!(retained.dispatch_id, Some(dispatch));
    assert_eq!(
        retained.accepted_receipt.as_deref(),
        Some("accepted-not-sampled")
    );
    let input = f
        .state
        .coordination_delivery(f.caller.root_id, f.caller.actor_id, message)
        .await?
        .unwrap();
    assert!(!input.consumed);
    assert_eq!(input.dispatch_id, Some(dispatch));
    assert_eq!(
        input.accepted_receipt.as_deref(),
        Some("accepted-not-sampled")
    );
    Ok(())
}

#[tokio::test]
async fn zero_duration_requires_captured_clock_readiness_not_a_cell_checkpoint()
-> anyhow::Result<()> {
    let f = Fixture::new().await?;
    let child = f.child().await?;
    let options = WaitOptions {
        timeout: Some(WaitTimeout {
            requested_ms: 0,
            effective_ms: 0,
        }),
        ..Default::default()
    };
    let before = chrono::Utc::now().timestamp_millis();
    let ticket = f
        .state
        .coordination_register_wait_with_options(
            f.caller,
            f.runtime,
            &cell(),
            &options,
            &[child.actor_id],
        )
        .await?;
    let after = chrono::Utc::now().timestamp_millis();
    assert!((before..=after).contains(&ticket.deadline_unix_ms.unwrap()));
    assert_eq!(ticket.options.timeout, options.timeout);
    assert_eq!(ticket.phase, WaitPhase::Registered);
    assert!(f.claim(&ticket).await?.is_none());
    assert_eq!(
        f.state
            .coordination_register_wait_with_options(
                f.caller,
                f.runtime,
                ticket.origin.cell().unwrap(),
                &options,
                &[],
            )
            .await?,
        ticket
    );
    // Deadline execution belongs to the timer slice; registration supplies no event.
    assert!(f.claim(&ticket).await?.is_none());
    assert_eq!(
        f.state
            .coordination_claim_ready_wait_at(
                f.caller,
                f.runtime,
                ticket.id,
                WaitReadiness { deadline_due: true },
            )
            .await?
            .unwrap()
            .wake,
        WaitWake::Deadline
    );
    Ok(())
}

#[tokio::test]
async fn reset_barrier_retains_ready_message_and_failure_does_not_manufacture_readiness()
-> anyhow::Result<()> {
    for ready in [false, true] {
        let f = Fixture::new().await?;
        let child = f.child().await?;
        let message = if ready {
            Some(f.message().await?)
        } else {
            None
        };
        let options = WaitOptions {
            compact: true,
            ..Default::default()
        };
        let ticket = f
            .state
            .coordination_register_wait_with_options(
                f.caller,
                f.runtime,
                &cell(),
                &options,
                &[child.actor_id],
            )
            .await?;
        assert!(f.claim(&ticket).await?.is_none());
        assert!(
            f.state
                .coordination_begin_wait_reset(
                    f.caller,
                    f.runtime,
                    ticket.id,
                    ticket.origin.cell().unwrap()
                )
                .await?
        );
        assert!(f.claim(&ticket).await?.is_none());
        // A second observer cannot run a second reset while the first is uncertain.
        assert!(
            !f.state
                .coordination_begin_wait_reset(
                    f.caller,
                    f.runtime,
                    ticket.id,
                    ticket.origin.cell().unwrap()
                )
                .await?
        );
        let settled = f
            .state
            .coordination_finish_wait_reset(
                f.caller,
                f.runtime,
                ticket.id,
                ticket.origin.cell().unwrap(),
                Some("source acquisition failed"),
            )
            .await?;
        assert_eq!(
            settled.reset,
            WaitResetState::Failed {
                diagnostic: "source acquisition failed".to_owned()
            }
        );
        assert!(
            !f.state
                .coordination_begin_wait_reset(
                    f.caller,
                    f.runtime,
                    ticket.id,
                    ticket.origin.cell().unwrap()
                )
                .await?
        );
        assert!(
            f.state
                .coordination_finish_wait_reset(
                    f.caller,
                    f.runtime,
                    ticket.id,
                    ticket.origin.cell().unwrap(),
                    None
                )
                .await
                .is_err()
        );
        if let Some(message) = message {
            assert_eq!(f.claim(&ticket).await?.unwrap().delivery_ids, vec![message]);
            assert!(
                !f.state
                    .coordination_delivery(f.caller.root_id, f.caller.actor_id, message)
                    .await?
                    .unwrap()
                    .consumed
            );
        } else {
            assert!(f.claim(&ticket).await?.is_none());
            f.terminal(child, f.caller.actor_id, WorkDisposition::Failed)
                .await?;
            assert!(f.claim(&ticket).await?.is_some());
        }
        assert_eq!(
            f.state
                .coordination_work(f.caller)
                .await?
                .unwrap()
                .disposition,
            WorkDisposition::Running
        );
    }
    Ok(())
}

#[tokio::test]
async fn canceled_cell_cannot_accept_reset_and_late_reset_outcome_does_not_revive_wait()
-> anyhow::Result<()> {
    let f = Fixture::new().await?;
    let message = f.message().await?;
    let options = WaitOptions {
        compact: true,
        ..Default::default()
    };
    for started in [false, true] {
        let ticket = f
            .state
            .coordination_register_wait_with_options(f.caller, f.runtime, &cell(), &options, &[])
            .await?;
        if started {
            assert!(
                f.state
                    .coordination_begin_wait_reset(
                        f.caller,
                        f.runtime,
                        ticket.id,
                        ticket.origin.cell().unwrap()
                    )
                    .await?
            );
        }
        f.state
            .coordination_cancel_wait(
                f.caller,
                f.runtime,
                ticket.id,
                ticket.origin.cell().unwrap(),
            )
            .await?;
        assert!(
            f.state
                .coordination_begin_wait_reset(
                    f.caller,
                    f.runtime,
                    ticket.id,
                    ticket.origin.cell().unwrap()
                )
                .await
                .is_err()
        );
        if started {
            let retained = f
                .state
                .coordination_finish_wait_reset(
                    f.caller,
                    f.runtime,
                    ticket.id,
                    ticket.origin.cell().unwrap(),
                    None,
                )
                .await?;
            assert_eq!(retained.phase, WaitPhase::Cancelled);
            assert_eq!(retained.reset, WaitResetState::Completed);
        } else {
            assert!(
                f.state
                    .coordination_finish_wait_reset(
                        f.caller,
                        f.runtime,
                        ticket.id,
                        ticket.origin.cell().unwrap(),
                        None
                    )
                    .await
                    .is_err()
            );
        }
        assert!(f.claim(&ticket).await?.is_none());
        let delivery = f
            .state
            .coordination_delivery(f.caller.root_id, f.caller.actor_id, message)
            .await?
            .unwrap();
        assert!(!delivery.consumed);
        assert!(delivery.claim_id.is_none());
    }
    Ok(())
}

#[tokio::test]
async fn any_snapshots_old_and_current_work_but_not_later_assignments() -> anyhow::Result<()> {
    let f = Fixture::new().await?;
    let first = f.child().await?;
    let outcome = f
        .terminal(first, f.caller.actor_id, WorkDisposition::Failed)
        .await?;
    let second = WorkStamp {
        work_id: Uuid::now_v7(),
        ..first
    };
    f.admit(second, Some(first)).await?;
    let ticket = f.register(WaitMode::Any, &[first.actor_id]).await?;
    assert_eq!(ticket.selected.len(), 2);
    assert!(ticket.selected.contains(&first));
    assert!(ticket.selected.contains(&second));
    assert_eq!(f.claim(&ticket).await?.unwrap().delivery_ids, vec![outcome]);
    let late = f.child().await?;
    f.terminal(late, f.caller.actor_id, WorkDisposition::Completed)
        .await?;

    let claim = f.claim(&ticket).await?.unwrap();
    assert_eq!(claim.delivery_ids, vec![outcome]);
    assert_eq!(
        claim
            .outcomes
            .iter()
            .map(|work| work.stamp)
            .collect::<Vec<_>>(),
        vec![first]
    );
    assert_eq!(claim.outcomes[0].disposition, WorkDisposition::Failed);
    assert_eq!(f.claim(&ticket).await?, Some(claim.clone()));
    f.consume(&ticket).await?;
    let next = f.register(WaitMode::Any, &[first.actor_id]).await?;
    assert_eq!(next.selected, vec![second]);

    assert_eq!(f.claim(&next).await?, None);
    let interrupted = f
        .terminal(second, f.caller.actor_id, WorkDisposition::Interrupted)
        .await?;
    assert_eq!(
        f.claim(&next).await?.unwrap().delivery_ids,
        vec![interrupted]
    );
    Ok(())
}

#[tokio::test]
async fn all_message_priority_preserves_every_selected_terminal_notification() -> anyhow::Result<()>
{
    let f = Fixture::new().await?;
    let first = f.child().await?;
    let second = f.child().await?;
    let ticket = f
        .register(WaitMode::All, &[first.actor_id, second.actor_id])
        .await?;

    let completed = f
        .terminal(first, f.caller.actor_id, WorkDisposition::Completed)
        .await?;
    assert_eq!(f.claim(&ticket).await?, None);
    let message = f.message().await?;
    let interrupted = f
        .terminal(second, f.caller.actor_id, WorkDisposition::Interrupted)
        .await?;
    let claim = f.claim(&ticket).await?.unwrap();
    assert_eq!(claim.delivery_ids, vec![message]);
    assert!(claim.outcomes.is_empty());
    f.consume(&ticket).await?;
    for id in [completed, interrupted] {
        let delivery = f
            .state
            .coordination_delivery(f.caller.root_id, f.caller.actor_id, id)
            .await?
            .unwrap();
        assert!(!delivery.consumed);
        assert_eq!(delivery.claim_id, None);
    }
    let all = f
        .register(WaitMode::All, &[first.actor_id, second.actor_id])
        .await?;

    let claim = f.claim(&all).await?.unwrap();
    assert_eq!(claim.delivery_ids, vec![completed, interrupted]);
    assert_eq!(claim.outcomes.len(), 2);
    f.consume(&all).await?;
    let error = f
        .register(WaitMode::Any, &[first.actor_id, second.actor_id])
        .await
        .unwrap_err();
    assert_eq!(
        error.downcast_ref::<WaitError>(),
        Some(&WaitError::EmptySelection)
    );
    assert_eq!(
        f.state
            .coordination_current_wait(f.caller.root_id, f.caller.actor_id)
            .await?,
        None
    );
    Ok(())
}

#[tokio::test]
async fn transitive_terminal_observation_does_not_steal_intermediate_mailbox() -> anyhow::Result<()>
{
    let f = Fixture::new().await?;
    let intermediate = f.child().await?;
    let descendant = f.child().await?;
    let delivery = f
        .terminal(
            descendant,
            intermediate.actor_id,
            WorkDisposition::Completed,
        )
        .await?;
    let ticket = f
        .register(WaitMode::Any, &[intermediate.actor_id, descendant.actor_id])
        .await?;

    let claim = f.claim(&ticket).await?.unwrap();
    assert!(claim.delivery_ids.is_empty());
    assert_eq!(claim.outcomes[0].stamp, descendant);
    f.consume(&ticket).await?;
    let notification = f
        .state
        .coordination_delivery(f.caller.root_id, intermediate.actor_id, delivery)
        .await?
        .unwrap();
    assert!(!notification.consumed);
    assert_eq!(notification.claim_id, None);
    assert_eq!(notification.dispatch_id, None);
    let next = f
        .register(WaitMode::Any, &[intermediate.actor_id, descendant.actor_id])
        .await?;
    assert_eq!(next.selected, vec![intermediate]);
    // The intermediate can independently select and consume its own notification.
    let inner = f
        .state
        .coordination_register_wait(
            intermediate,
            f.runtime,
            &cell(),
            WaitMode::Any,
            &[descendant.actor_id],
        )
        .await?;
    assert_eq!(
        f.state
            .coordination_claim_ready_wait(intermediate, f.runtime, inner.id)
            .await?
            .unwrap()
            .delivery_ids,
        vec![delivery]
    );
    Ok(())
}

#[tokio::test]
async fn selected_terminal_still_settles_after_notification_was_admitted_elsewhere()
-> anyhow::Result<()> {
    let f = Fixture::new().await?;
    let child = f.child().await?;
    let ticket = f.register(WaitMode::All, &[child.actor_id]).await?;
    let delivery = f
        .terminal(child, f.caller.actor_id, WorkDisposition::Failed)
        .await?;
    let dispatch = Uuid::now_v7();
    f.state
        .coordination_begin_delivery(
            f.caller.root_id,
            f.caller.actor_id,
            &[delivery],
            None,
            dispatch,
        )
        .await?;
    f.state
        .coordination_confirm_delivery(
            f.caller.root_id,
            f.caller.actor_id,
            &[delivery],
            dispatch,
            "other-input",
        )
        .await?;
    f.state
        .coordination_consume_deliveries(
            f.caller.root_id,
            f.caller.actor_id,
            &[delivery],
            "other-input",
        )
        .await?;

    let claim = f.claim(&ticket).await?.unwrap();
    assert!(
        claim.delivery_ids.is_empty(),
        "do not repeat already delivered answer text"
    );
    assert_eq!(claim.outcomes[0].disposition, WorkDisposition::Failed);
    f.consume(&ticket).await?;
    Ok(())
}

#[tokio::test]
async fn exact_cell_replay_failure_and_supersession_preserve_unconsumed_events()
-> anyhow::Result<()> {
    let f = Fixture::new().await?;
    let message = f.message().await?;
    let ticket = f.register(WaitMode::All, &[]).await?;
    assert!(ticket.selected.is_empty());
    assert_eq!(
        f.state
            .coordination_register_wait(
                f.caller,
                f.runtime,
                ticket.origin.cell().unwrap(),
                WaitMode::All,
                &[]
            )
            .await?,
        ticket
    );
    assert_eq!(f.claim(&ticket).await?.unwrap().delivery_ids, vec![message]);
    assert!(f.register(WaitMode::All, &[]).await.is_err());
    assert!(
        f.state
            .coordination_register_wait(
                f.caller,
                f.runtime,
                ticket.origin.cell().unwrap(),
                WaitMode::Any,
                &[]
            )
            .await
            .is_err()
    );
    let mut wrong_cell = ticket.origin.cell().unwrap().clone();
    wrong_cell.window_id = "resumed-window-is-not-original".to_owned();
    assert!(
        f.state
            .coordination_cancel_wait(f.caller, f.runtime, ticket.id, &wrong_cell)
            .await
            .is_err()
    );
    f.state
        .coordination_cancel_wait(
            f.caller,
            f.runtime,
            ticket.id,
            ticket.origin.cell().unwrap(),
        )
        .await?;
    let next = f.register(WaitMode::All, &[]).await?;

    let claim = f.claim(&next).await?.unwrap();
    assert_eq!(claim.delivery_ids, vec![message]);
    // A duplicate old cancel must not release the new wait's claim.
    f.state
        .coordination_cancel_wait(
            f.caller,
            f.runtime,
            ticket.id,
            ticket.origin.cell().unwrap(),
        )
        .await?;
    assert_eq!(
        f.state
            .coordination_delivery(f.caller.root_id, f.caller.actor_id, message)
            .await?
            .unwrap()
            .claim_id,
        Some(claim.id)
    );
    f.state
        .coordination_cancel_wait(f.caller, f.runtime, next.id, next.origin.cell().unwrap())
        .await?;
    let notification = f
        .state
        .coordination_delivery(f.caller.root_id, f.caller.actor_id, message)
        .await?
        .unwrap();
    assert_eq!(notification.claim_id, None);
    assert!(!notification.consumed);
    assert_eq!(
        f.state
            .coordination_work(f.caller)
            .await?
            .unwrap()
            .disposition,
        WorkDisposition::Running
    );
    assert!(
        f.state
            .coordination_current_wait(f.caller.root_id, f.caller.actor_id)
            .await?
            .is_none()
    );
    Ok(())
}

#[tokio::test]
async fn reopening_preserves_ticket_but_cannot_rearm_or_replay_it() -> anyhow::Result<()> {
    let f = Fixture::new().await?;
    let message = f.message().await?;
    let ticket = f.register(WaitMode::Any, &[]).await?;

    let claim = f.claim(&ticket).await?.unwrap();
    let dispatch = Uuid::now_v7();
    // Historical dispatched waits survive cold observation without a mutation API.
    sqlx::query("UPDATE agent_waits SET phase='dispatched',dispatch_id=? WHERE wait_id=?")
        .bind(dispatch.to_string())
        .bind(ticket.id.to_string())
        .execute(f.state.pool.as_ref())
        .await?;
    sqlx::query("UPDATE agent_deliveries SET dispatch_id=? WHERE delivery_id=?")
        .bind(dispatch.to_string())
        .bind(message.to_string())
        .execute(f.state.pool.as_ref())
        .await?;
    let retained = f
        .state
        .coordination_observe_wait(f.caller, ticket.id)
        .await?
        .unwrap();
    let caller = f.caller;
    let directory = f.directory;
    drop(f.state);
    let state = StateRuntime::init(
        crate::SqliteConfig::new_for_testing(directory.path().abs()),
        "test-provider".to_owned(),
    )
    .await?;
    let cold = Uuid::now_v7();
    assert_eq!(
        state
            .coordination_current_wait(caller.root_id, caller.actor_id)
            .await?,
        Some(retained.clone())
    );
    assert_eq!(retained.claim, Some(claim));
    assert_eq!(retained.phase, WaitPhase::Dispatched);
    assert!(
        state
            .coordination_claim_ready_wait(caller, cold, ticket.id)
            .await
            .is_err()
    );
    assert!(
        state
            .coordination_cancel_wait(caller, cold, ticket.id, ticket.origin.cell().unwrap())
            .await
            .is_err()
    );
    assert_eq!(
        state.coordination_observe_wait(caller, ticket.id).await?,
        Some(retained)
    );
    Ok(())
}

#[tokio::test]
async fn uncertain_and_empty_selection_do_not_create_waits() -> anyhow::Result<()> {
    let f = Fixture::new().await?;
    let uncertain = f.child().await?;
    f.state
        .coordination_transition_work(uncertain, 1, f.runtime, WorkDisposition::Uncertain)
        .await?;
    let error = f
        .register(WaitMode::All, &[uncertain.actor_id])
        .await
        .unwrap_err();
    assert_eq!(
        error.downcast_ref::<WaitError>(),
        Some(&WaitError::EmptySelection)
    );
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM agent_waits")
        .fetch_one(f.state.pool.as_ref())
        .await?;
    assert_eq!(count, 0);
    Ok(())
}

#[tokio::test]
async fn any_prefers_addressed_message_even_when_selected_outcome_is_already_ready()
-> anyhow::Result<()> {
    let f = Fixture::new().await?;
    let child = f.child().await?;
    let terminal = f
        .terminal(child, f.caller.actor_id, WorkDisposition::Completed)
        .await?;
    let message = f.message().await?;
    let ticket = f.register(WaitMode::Any, &[child.actor_id]).await?;

    let claim = f.claim(&ticket).await?.unwrap();
    assert_eq!(claim.delivery_ids, vec![message]);
    assert!(claim.outcomes.is_empty());
    f.consume(&ticket).await?;
    let next = f.register(WaitMode::Any, &[child.actor_id]).await?;

    assert_eq!(f.claim(&next).await?.unwrap().delivery_ids, vec![terminal]);
    Ok(())
}

#[tokio::test]
async fn terminal_of_selected_generation_settles_all_even_when_actor_is_working_again()
-> anyhow::Result<()> {
    let f = Fixture::new().await?;
    let selected = f.child().await?;
    let ticket = f.register(WaitMode::All, &[selected.actor_id]).await?;

    let result = f
        .terminal(selected, f.caller.actor_id, WorkDisposition::Completed)
        .await?;
    let later = WorkStamp {
        work_id: Uuid::now_v7(),
        ..selected
    };
    f.admit(later, Some(selected)).await?;
    let claim = f.claim(&ticket).await?.unwrap();
    assert_eq!(claim.delivery_ids, vec![result]);
    assert_eq!(claim.outcomes[0].stamp, selected);
    assert_eq!(
        f.state
            .coordination_current_work(later.root_id, later.actor_id)
            .await?
            .unwrap()
            .stamp,
        later
    );
    Ok(())
}

#[tokio::test]
async fn concurrent_registration_and_claim_share_one_durable_ticket() -> anyhow::Result<()> {
    let f = Fixture::new().await?;
    f.message().await?;
    let original = cell();
    let other = cell();
    let (first, second) = tokio::join!(
        f.state
            .coordination_register_wait(f.caller, f.runtime, &original, WaitMode::Any, &[]),
        f.state
            .coordination_register_wait(f.caller, f.runtime, &other, WaitMode::Any, &[]),
    );
    let ticket = match (first, second) {
        (Ok(ticket), Err(error)) | (Err(error), Ok(ticket)) => {
            assert_eq!(
                error.downcast_ref::<WaitError>(),
                Some(&WaitError::ConflictingCell)
            );
            ticket
        }
        outcomes => panic!("exactly one original cell must be accepted: {outcomes:?}"),
    };

    let (first, second) = tokio::join!(f.claim(&ticket), f.claim(&ticket));
    assert_eq!(first?.unwrap(), second?.unwrap());
    Ok(())
}

#[tokio::test]
async fn wait_cannot_overwrite_a_delivery_fenced_through_the_shared_input_owner()
-> anyhow::Result<()> {
    let f = Fixture::new().await?;
    let message = f.message().await?;
    let ticket = f.register(WaitMode::Any, &[]).await?;

    let claim = f.claim(&ticket).await?.unwrap();
    let other_dispatch = Uuid::now_v7();
    f.state
        .coordination_begin_delivery(
            f.caller.root_id,
            f.caller.actor_id,
            &[message],
            Some(claim.id),
            other_dispatch,
        )
        .await?;
    assert!(
        f.state
            .coordination_cancel_wait(
                f.caller,
                f.runtime,
                ticket.id,
                ticket.origin.cell().unwrap()
            )
            .await
            .is_err()
    );
    let retained = f
        .state
        .coordination_observe_wait(f.caller, ticket.id)
        .await?
        .unwrap();
    assert_eq!(retained.phase, WaitPhase::Claimed);
    assert_eq!(retained.dispatch_id, None);
    assert_eq!(
        f.state
            .coordination_delivery(f.caller.root_id, f.caller.actor_id, message)
            .await?
            .unwrap()
            .dispatch_id,
        Some(other_dispatch)
    );
    Ok(())
}

#[tokio::test]
async fn enclosing_input_transaction_can_roll_back_wait_cancellation_and_claim_release()
-> anyhow::Result<()> {
    let f = Fixture::new().await?;
    let message = f.message().await?;
    let ticket = f.register(WaitMode::Any, &[]).await?;

    let claim = f.claim(&ticket).await?.unwrap();
    let retained = f
        .state
        .coordination_observe_wait(f.caller, ticket.id)
        .await?;
    let work = f.state.coordination_work(f.caller).await?;

    let mut tx = f.state.pool.begin_immediate().await?;
    cancel_wait_in_transaction(&mut tx, f.caller, f.runtime, ticket.id, &ticket.origin).await?;
    assert_eq!(
        required_wait(&mut tx, f.caller, ticket.id).await?.phase,
        WaitPhase::Cancelled
    );
    let released: Option<String> =
        sqlx::query_scalar("SELECT claim_id FROM agent_deliveries WHERE delivery_id=?")
            .bind(message.to_string())
            .fetch_one(&mut *tx)
            .await?;
    assert_eq!(released, None);
    // The enclosing admission owns commit. Refusal must leave its wait intact.
    tx.rollback().await?;

    assert_eq!(
        f.state
            .coordination_observe_wait(f.caller, ticket.id)
            .await?,
        retained
    );
    assert_eq!(f.state.coordination_work(f.caller).await?, work);
    assert_eq!(
        f.state
            .coordination_delivery(f.caller.root_id, f.caller.actor_id, message)
            .await?
            .unwrap()
            .claim_id,
        Some(claim.id)
    );
    // The ordinary owner wrapper still commits the same transition.
    f.state
        .coordination_cancel_wait(
            f.caller,
            f.runtime,
            ticket.id,
            ticket.origin.cell().unwrap(),
        )
        .await?;
    assert_eq!(
        f.state
            .coordination_current_wait(f.caller.root_id, f.caller.actor_id)
            .await?,
        None
    );
    assert_eq!(
        f.state
            .coordination_delivery(f.caller.root_id, f.caller.actor_id, message)
            .await?
            .unwrap()
            .claim_id,
        None
    );
    Ok(())
}

fn progress_capture(
    child: WorkStamp,
    first: u64,
    last: u64,
) -> crate::coordination_progress::FrozenProgress {
    use crate::coordination_progress::*;
    FrozenProgress {
        snapshot_ref: "immutable-publication".into(),
        publication_revision: 7,
        coverage: CaptureCoverage {
            status: "complete".into(),
            diagnostics: Vec::new(),
        },
        agents: vec![FrozenAgentCapture {
            actor: ProgressActor {
                thread_id: child.actor_id,
                work_id: child.work_id,
                task_name: "agent:main/child".into(),
            },
            intervals: vec![CaptureInterval {
                key: "opaque-record-key".into(),
                change_id: "amber1".into(),
                record_order: 3,
                legacy_record_key: None,
                first_chunk: first,
                last_chunk: last,
            }],
        }],
    }
}

async fn progress_wait(
    f: &Fixture,
    selected: &[WorkStamp],
    mode: WaitMode,
) -> anyhow::Result<WaitTicket> {
    for target in selected {
        let work = f.state.coordination_work(*target).await?.unwrap();
        f.state
            .coordination_transition_work(
                *target,
                work.revision,
                f.runtime,
                WorkDisposition::Running,
            )
            .await?;
    }
    let ticket = f
        .state
        .coordination_register_wait_with_options(
            f.caller,
            f.runtime,
            &cell(),
            &WaitOptions {
                mode,
                progress_interval_ms: Some(300_000),
                timeout: Some(WaitTimeout {
                    requested_ms: 600_000,
                    effective_ms: 600_000,
                }),
                ..Default::default()
            },
            &selected
                .iter()
                .map(|work| work.actor_id)
                .collect::<Vec<_>>(),
        )
        .await?;

    Ok(ticket)
}

async fn freeze_progress(
    f: &Fixture,
    ticket: &WaitTicket,
    context: Uuid,
    frozen: &crate::coordination_progress::FrozenProgress,
) -> anyhow::Result<crate::coordination_progress::ProgressBatch> {
    let batch = f
        .state
        .coordination_begin_progress(f.caller, f.runtime, ticket.id, context)
        .await?
        .unwrap();
    assert!(
        f.state
            .coordination_freeze_progress(f.caller, f.runtime, ticket.id, batch.batch_id, frozen)
            .await?
    );
    Ok(batch)
}

async fn ready_progress(
    f: &Fixture,
    ticket: &WaitTicket,
    batch: &crate::coordination_progress::ProgressBatch,
    frozen: &crate::coordination_progress::FrozenProgress,
    delivered: bool,
) -> anyhow::Result<()> {
    use crate::coordination_progress::*;
    for target in &frozen.agents {
        let target = WorkStamp {
            root_id: f.caller.root_id,
            actor_id: target.actor.thread_id,
            work_id: target.actor.work_id,
        };
        for probe in f
            .state
            .coordination_take_progress_probes(target, f.runtime)
            .await?
        {
            assert!(
                f.state
                    .coordination_resolve_progress(
                        f.runtime,
                        &probe,
                        &SummaryResult::Available {
                            text: "Changed the owner transaction; focused checks are not run."
                                .into()
                        }
                    )
                    .await?
            );
        }
        assert!(
            f.state
                .coordination_take_progress_probes(target, f.runtime)
                .await?
                .is_empty(),
            "a slot samples at most once"
        );
    }
    assert!(
        f.state
            .coordination_progress_agents(f.caller, f.runtime, ticket.id, batch.batch_id)
            .await?
            .is_some()
    );
    let output = PreparedProgressOutput {
        text: if delivered {
            "complete frozen interval and summary"
        } else {
            "recovery only: retained full output"
        }
        .into(),
        recovery_ref: (!delivered).then(|| "immutable-recovery".into()),
        delivered_intervals: if delivered {
            frozen
                .agents
                .iter()
                .flat_map(|agent| agent.intervals.clone())
                .collect()
        } else {
            Vec::new()
        },
    };
    assert!(
        f.state
            .coordination_prepare_progress_output(
                f.caller,
                f.runtime,
                ticket.id,
                batch.batch_id,
                &output
            )
            .await?
    );
    Ok(())
}

#[tokio::test]
async fn unchanged_progress_needs_no_resource_slot_claim_or_cursor() -> anyhow::Result<()> {
    let f = Fixture::new().await?;
    let child = f.child().await?;
    let ticket = progress_wait(&f, &[child], WaitMode::Any).await?;
    let context = Uuid::now_v7();
    let batch = f
        .state
        .coordination_begin_progress(f.caller, f.runtime, ticket.id, context)
        .await?
        .unwrap();
    let mut frozen = progress_capture(child, 1, 1);
    frozen.snapshot_ref.clear();
    frozen.agents.clear();
    assert!(
        !f.state
            .coordination_freeze_progress(f.caller, f.runtime, ticket.id, batch.batch_id, &frozen)
            .await?
    );
    assert!(
        f.state
            .coordination_progress_batch(f.caller, f.runtime, ticket.id)
            .await?
            .is_none()
    );
    assert!(
        f.state
            .coordination_take_progress_probes(child, f.runtime)
            .await?
            .is_empty()
    );
    assert!(f.claim(&ticket).await?.is_none());
    assert!(
        f.state
            .coordination_capture_cursors(f.caller.root_id, f.caller.actor_id, context)
            .await?
            .is_empty()
    );
    // Coverage failure is not an unchanged-success shortcut and cannot claim
    // a nonexistent resource. A real diagnostic resource needs no actor inference.
    let batch = f
        .state
        .coordination_begin_progress(f.caller, f.runtime, ticket.id, context)
        .await?
        .unwrap();
    frozen.coverage.status = "unavailable".into();
    frozen
        .coverage
        .diagnostics
        .push("capture owner unavailable".into());
    assert!(
        f.state
            .coordination_freeze_progress(f.caller, f.runtime, ticket.id, batch.batch_id, &frozen)
            .await
            .is_err()
    );
    frozen.snapshot_ref = "immutable-diagnostic".into();
    assert!(
        f.state
            .coordination_freeze_progress(f.caller, f.runtime, ticket.id, batch.batch_id, &frozen)
            .await?
    );
    assert!(
        f.state
            .coordination_take_progress_probes(child, f.runtime)
            .await?
            .is_empty()
    );
    assert_eq!(
        f.state
            .coordination_progress_agents(f.caller, f.runtime, ticket.id, batch.batch_id)
            .await?,
        Some(Vec::new())
    );
    Ok(())
}

#[tokio::test]
async fn progress_capture_rejects_unselected_work_and_unfrozen_acknowledgement()
-> anyhow::Result<()> {
    use crate::coordination_progress::*;
    let f = Fixture::new().await?;
    let selected = f.child().await?;
    let foreign = f.child().await?;
    let ticket = progress_wait(&f, &[selected], WaitMode::Any).await?;
    let batch = f
        .state
        .coordination_begin_progress(f.caller, f.runtime, ticket.id, Uuid::now_v7())
        .await?
        .unwrap();
    assert!(
        f.state
            .coordination_freeze_progress(
                f.caller,
                f.runtime,
                ticket.id,
                batch.batch_id,
                &progress_capture(foreign, 1, 2)
            )
            .await
            .is_err()
    );
    assert!(
        f.state
            .coordination_progress_batch(f.caller, f.runtime, ticket.id)
            .await?
            .unwrap()
            .frozen
            .is_none()
    );
    let frozen = progress_capture(selected, 1, 2);
    assert!(
        f.state
            .coordination_freeze_progress(f.caller, f.runtime, ticket.id, batch.batch_id, &frozen)
            .await?
    );
    let probes = f
        .state
        .coordination_take_progress_probes(selected, f.runtime)
        .await?;
    assert_eq!(probes.len(), 1);
    f.state
        .coordination_resolve_progress(
            f.runtime,
            &probes[0],
            &SummaryResult::Unavailable {
                reason: "provider refused auxiliary request".into(),
            },
        )
        .await?;
    let mut interval = frozen.agents[0].intervals[0].clone();
    interval.last_chunk += 1;
    assert!(
        f.state
            .coordination_prepare_progress_output(
                f.caller,
                f.runtime,
                ticket.id,
                batch.batch_id,
                &PreparedProgressOutput {
                    text: "invented late interval".into(),
                    recovery_ref: None,
                    delivered_intervals: vec![interval]
                }
            )
            .await
            .is_err()
    );
    assert!(
        f.state
            .coordination_progress_batch(f.caller, f.runtime, ticket.id)
            .await?
            .unwrap()
            .output
            .is_none()
    );
    Ok(())
}

#[tokio::test]
async fn progress_cursor_requires_consumed_complete_output_in_its_live_context()
-> anyhow::Result<()> {
    for (delivered, replaced) in [(true, false), (false, false), (true, true)] {
        let f = Fixture::new().await?;
        let child = f.child().await?;
        let ticket = progress_wait(&f, &[child], WaitMode::Any).await?;
        let context = Uuid::now_v7();
        let frozen = progress_capture(child, 1, 2);
        let batch = freeze_progress(&f, &ticket, context, &frozen).await?;
        ready_progress(&f, &ticket, &batch, &frozen, delivered).await?;
        let claim = f.claim(&ticket).await?.unwrap();
        assert_eq!(claim.wake, WaitWake::Progress);
        assert!(claim.delivery_ids.is_empty() && claim.outcomes.is_empty());
        assert!(
            f.state
                .coordination_capture_cursors(f.caller.root_id, f.caller.actor_id, context)
                .await?
                .is_empty(),
            "preparing a result cannot acknowledge capture"
        );
        let receiving = if replaced { Uuid::now_v7() } else { context };
        for _ in 0..2 {
            f.state
                .coordination_consume_awaited_wait(
                    f.caller, f.runtime, ticket.id, claim.id, receiving,
                )
                .await?;
        }
        let cursors = f
            .state
            .coordination_capture_cursors(f.caller.root_id, f.caller.actor_id, receiving)
            .await?;
        assert_eq!(cursors.len(), usize::from(delivered && !replaced));
        if delivered && !replaced {
            assert_eq!(cursors[0].through_chunk, 2);
        }
        assert!(
            f.state
                .coordination_capture_cursors(f.caller.root_id, f.caller.actor_id, Uuid::now_v7())
                .await?
                .is_empty(),
            "reset, rewind and reload tokens cannot inherit suppression"
        );
        assert!(
            !f.state
                .coordination_work(child)
                .await?
                .unwrap()
                .disposition
                .is_terminal(),
            "progress does not settle primary work"
        );
    }
    Ok(())
}

#[tokio::test]
async fn normal_events_and_deadline_preempt_progress_during_collection_and_after_claim()
-> anyhow::Result<()> {
    for (message, claim_first) in [(true, false), (true, true), (false, true)] {
        let f = Fixture::new().await?;
        let child = f.child().await?;
        let ticket = progress_wait(&f, &[child], WaitMode::Any).await?;
        let frozen = progress_capture(child, 1, 1);
        let batch = freeze_progress(&f, &ticket, Uuid::now_v7(), &frozen).await?;
        if claim_first {
            ready_progress(&f, &ticket, &batch, &frozen, true).await?;
            assert_eq!(f.claim(&ticket).await?.unwrap().wake, WaitWake::Progress);
        }
        let message_id = if message {
            Some(f.message().await?)
        } else {
            None
        };
        let readiness = WaitReadiness {
            deadline_due: !message,
        };
        let claim = f
            .state
            .coordination_claim_ready_wait_at(f.caller, f.runtime, ticket.id, readiness)
            .await?
            .unwrap();
        assert_eq!(
            claim.wake,
            if message {
                WaitWake::Events
            } else {
                WaitWake::Deadline
            }
        );
        assert_eq!(
            claim.delivery_ids,
            message_id.into_iter().collect::<Vec<_>>()
        );
        assert!(
            f.state
                .coordination_take_progress_probes(child, f.runtime)
                .await?
                .is_empty()
        );
    }
    Ok(())
}

#[tokio::test]
async fn all_wait_terminal_resolves_only_its_progress_slot_and_cold_slots_never_restart()
-> anyhow::Result<()> {
    use crate::coordination_progress::SummaryResult;
    let f = Fixture::new().await?;
    let left = f.child().await?;
    let right = f.child().await?;
    let ticket = progress_wait(&f, &[left, right], WaitMode::All).await?;
    let frozen = progress_capture(left, 1, 1);
    let batch = freeze_progress(&f, &ticket, Uuid::now_v7(), &frozen).await?;
    assert!(
        f.state
            .coordination_take_progress_probes(left, Uuid::now_v7())
            .await?
            .is_empty()
    );
    assert!(
        f.state
            .coordination_begin_progress(f.caller, Uuid::now_v7(), ticket.id, Uuid::now_v7())
            .await
            .is_err()
    );
    f.terminal(left, f.caller.actor_id, WorkDisposition::Completed)
        .await?;
    assert!(
        f.claim(&ticket).await?.is_none(),
        "one ordinary final cannot complete an all-wait"
    );
    let agents = f
        .state
        .coordination_progress_agents(f.caller, f.runtime, ticket.id, batch.batch_id)
        .await?
        .unwrap();
    assert!(matches!(
        agents[0].summary,
        SummaryResult::Unavailable { .. }
    ));
    assert!(
        f.state
            .coordination_take_progress_probes(left, f.runtime)
            .await?
            .is_empty()
    );
    Ok(())
}

#[tokio::test]
async fn late_chunks_under_the_same_capture_key_advance_only_the_new_prefix() -> anyhow::Result<()>
{
    let f = Fixture::new().await?;
    let child = f.child().await?;
    let context = Uuid::now_v7();
    for (first, last) in [(1, 2), (3, 4)] {
        let ticket = progress_wait(&f, &[child], WaitMode::Any).await?;
        let frozen = progress_capture(child, first, last);
        let batch = freeze_progress(&f, &ticket, context, &frozen).await?;
        ready_progress(&f, &ticket, &batch, &frozen, true).await?;
        let claim = f.claim(&ticket).await?.unwrap();
        assert_eq!(claim.wake, WaitWake::Progress);
        f.state
            .coordination_consume_awaited_wait(f.caller, f.runtime, ticket.id, claim.id, context)
            .await?;
        let cursors = f
            .state
            .coordination_capture_cursors(f.caller.root_id, f.caller.actor_id, context)
            .await?;
        assert_eq!(cursors.len(), 1);
        assert_eq!(cursors[0].through_chunk, last);
    }
    Ok(())
}

#[tokio::test]
async fn wait_closure_keeps_actual_terminal_or_execution_failure_cause() -> anyhow::Result<()> {
    for terminal in [
        WorkDisposition::Completed,
        WorkDisposition::Failed,
        WorkDisposition::Interrupted,
    ] {
        let f = Fixture::new().await?;
        let child = f.child().await?;
        let wait = f.register(WaitMode::All, &[child.actor_id]).await?;

        let delivery = f.terminal(f.caller, f.caller.actor_id, terminal).await?;
        let closed = f
            .state
            .coordination_observe_wait(f.caller, wait.id)
            .await?
            .unwrap();
        assert_eq!(closed.phase, WaitPhase::Cancelled);
        assert_eq!(
            closed.cancellation,
            Some(WaitCancellation::WorkTerminated {
                delivery_id: delivery
            })
        );
        assert_eq!(closed.origin, wait.origin);
        assert_eq!(closed.options.mode, WaitMode::All);
        assert_eq!(
            f.state
                .coordination_delivery(f.caller.root_id, f.caller.actor_id, delivery)
                .await?
                .unwrap()
                .publication
                .work,
            Some(f.caller)
        );
    }
    let f = Fixture::new().await?;
    let child = f.child().await?;
    let wait = f.register(WaitMode::Any, &[child.actor_id]).await?;
    f.state
        .coordination_cancel_wait_with_cause(
            f.caller,
            f.runtime,
            wait.id,
            &wait.origin,
            &WaitCancellation::ExecutionFailed,
        )
        .await?;
    assert_eq!(
        f.state
            .coordination_observe_wait(f.caller, wait.id)
            .await?
            .unwrap()
            .cancellation,
        Some(WaitCancellation::ExecutionFailed)
    );
    Ok(())
}

#[tokio::test]
async fn shell_output_closure_retains_actual_turn_and_rejects_empty_evidence() -> anyhow::Result<()>
{
    let f = Fixture::new().await?;
    let child = f.child().await?;
    let wait = f.register(WaitMode::All, &[child.actor_id]).await?;

    let before = f.state.coordination_observe_wait(f.caller, wait.id).await?;
    assert!(
        f.state
            .coordination_cancel_wait_with_cause(
                f.caller,
                f.runtime,
                wait.id,
                &wait.origin,
                &WaitCancellation::ShellOutput {
                    turn_id: String::new()
                }
            )
            .await
            .is_err()
    );
    assert_eq!(
        f.state.coordination_observe_wait(f.caller, wait.id).await?,
        before
    );
    let cause = WaitCancellation::ShellOutput {
        turn_id: "persisted-shell-turn".into(),
    };
    f.state
        .coordination_cancel_wait_with_cause(f.caller, f.runtime, wait.id, &wait.origin, &cause)
        .await?;
    let closed = f
        .state
        .coordination_observe_wait(f.caller, wait.id)
        .await?
        .unwrap();
    assert_eq!(closed.cancellation, Some(cause));
    assert_eq!(closed.caller, wait.caller);
    assert_eq!(closed.selected, wait.selected);
    assert_eq!(closed.origin, wait.origin);
    assert_eq!(closed.phase, WaitPhase::Cancelled);
    Ok(())
}

#[tokio::test]
async fn paused_targets_settle_any_and_all_without_becoming_terminal_deliveries()
-> anyhow::Result<()> {
    for mode in [WaitMode::Any, WaitMode::All] {
        for before in [false, true] {
            let f = Fixture::new().await?;
            let paused = f.child().await?;
            let other = f.child().await?;
            if before {
                let work = f.state.coordination_work(paused).await?.unwrap();
                f.state
                    .coordination_transition_work(
                        paused,
                        work.revision,
                        f.runtime,
                        WorkDisposition::Paused,
                    )
                    .await?;
            }
            let wait = f.register(mode, &[paused.actor_id, other.actor_id]).await?;
            assert_eq!(
                wait.selected,
                vec![paused, other],
                "already-paused work remains a selected stopped target"
            );

            if !before {
                assert!(f.claim(&wait).await?.is_none());
                let work = f.state.coordination_work(paused).await?.unwrap();
                f.state
                    .coordination_transition_work(
                        paused,
                        work.revision,
                        f.runtime,
                        WorkDisposition::Paused,
                    )
                    .await?;
            }
            if mode == WaitMode::All {
                assert!(
                    f.claim(&wait).await?.is_none(),
                    "one pause cannot settle another running target"
                );
                let work = f.state.coordination_work(other).await?.unwrap();
                f.state
                    .coordination_transition_work(
                        other,
                        work.revision,
                        f.runtime,
                        WorkDisposition::Uncertain,
                    )
                    .await?;
                assert!(
                    f.claim(&wait).await?.is_none(),
                    "uncertain execution is not stopped proof"
                );
                let work = f.state.coordination_work(other).await?.unwrap();
                f.state
                    .coordination_transition_work(
                        other,
                        work.revision,
                        f.runtime,
                        WorkDisposition::Paused,
                    )
                    .await?;
            }
            let claim = f.claim(&wait).await?.unwrap();
            assert_eq!(claim.wake, WaitWake::Events);
            assert!(claim.delivery_ids.is_empty());
            assert_eq!(
                claim.outcomes.len(),
                if mode == WaitMode::Any { 1 } else { 2 }
            );
            assert!(
                claim
                    .outcomes
                    .iter()
                    .all(|outcome| outcome.disposition == WorkDisposition::Paused)
            );
            for outcome in &claim.outcomes {
                let work = f.state.coordination_work(outcome.stamp).await?.unwrap();
                assert!(!work.disposition.is_terminal());
                assert!(work.terminal_delivery_id.is_none());
                f.state
                    .coordination_transition_work(
                        outcome.stamp,
                        work.revision,
                        f.runtime,
                        WorkDisposition::Running,
                    )
                    .await?;
            }
            let reopened = StateRuntime::init(
                crate::SqliteConfig::new_for_testing(f.directory.path().abs()),
                "test-provider".into(),
            )
            .await?;
            assert_eq!(
                reopened
                    .coordination_observe_wait(f.caller, wait.id)
                    .await?
                    .unwrap()
                    .claim,
                Some(claim.clone()),
                "resuming the same work cannot rewrite a persisted paused claim"
            );
            f.consume(&wait).await?;
            let final_id = f
                .terminal(paused, f.caller.actor_id, WorkDisposition::Completed)
                .await?;
            let later = f.register(WaitMode::Any, &[paused.actor_id]).await?;

            let terminal = f.claim(&later).await?.unwrap();
            assert_eq!(
                terminal.delivery_ids,
                vec![final_id],
                "consuming paused status does not consume a later final"
            );
            assert_eq!(terminal.outcomes[0].disposition, WorkDisposition::Completed);
            assert_eq!(
                reopened
                    .coordination_observe_wait(f.caller, wait.id)
                    .await?
                    .unwrap()
                    .claim,
                Some(claim)
            );
        }
    }
    Ok(())
}

#[tokio::test]
async fn mixed_paused_and_terminal_claim_preserves_publication_order_and_mail_ownership()
-> anyhow::Result<()> {
    let f = Fixture::new().await?;
    let paused = f.child().await?;
    let first = f.child().await?;
    let second = f.child().await?;
    let work = f.state.coordination_work(paused).await?.unwrap();
    f.state
        .coordination_transition_work(paused, work.revision, f.runtime, WorkDisposition::Paused)
        .await?;
    let wait = f
        .register(
            WaitMode::All,
            &[paused.actor_id, first.actor_id, second.actor_id],
        )
        .await?;

    let second_id = f
        .terminal(second, f.caller.actor_id, WorkDisposition::Failed)
        .await?;
    let first_id = f
        .terminal(first, f.caller.actor_id, WorkDisposition::Completed)
        .await?;
    let claim = f.claim(&wait).await?.unwrap();
    assert_eq!(claim.delivery_ids, vec![second_id, first_id]);
    assert_eq!(
        claim
            .outcomes
            .iter()
            .map(|outcome| outcome.stamp)
            .collect::<Vec<_>>(),
        vec![second, first, paused]
    );
    assert_eq!(
        f.state
            .coordination_observe_wait(f.caller, wait.id)
            .await?
            .unwrap()
            .claim,
        Some(claim)
    );
    assert!(
        f.state
            .coordination_work(paused)
            .await?
            .unwrap()
            .terminal_delivery_id
            .is_none()
    );
    Ok(())
}

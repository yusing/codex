use super::*;
use crate::agent::control::work::DeliveryKind;
use crate::agent::control::work::PublishDelivery;
use crate::thread_manager::default_thread_id_generator;
use codex_protocol::error::RetryLimitReachedError;
use codex_state::coordination_wait::WaitMode;
use codex_state::coordination_wait::WaitOrigin;
use codex_state::coordination_wait::WaitResetState;
use codex_state::coordination_wait::WaitTimeout;
use codex_utils_absolute_path::test_support::PathExt;
use std::sync::Weak;
use uuid::Uuid;

fn runtime() -> LocalAgentRuntime {
    LocalAgentRuntime::new(Weak::default(), default_thread_id_generator(), None, None)
}

fn ticket(runtime: &LocalAgentRuntime, actor: ThreadId, deadline: i64) -> WaitTicket {
    runtime.for_actor_start(actor, None).unwrap();
    WaitTicket {
        id: Uuid::now_v7(),
        caller: WorkStamp {
            root_id: actor,
            actor_id: actor,
            work_id: Uuid::now_v7(),
        },
        runtime_id: runtime.tree_id(),
        origin: WaitOrigin::Turn {
            turn_id: "exhausted-request".into(),
            reason: TurnWaitReason::ProviderCapacity,
        },
        options: WaitOptions::default(),
        deadline_unix_ms: Some(deadline),
        reset: WaitResetState::NotRequested,
        phase: WaitPhase::Registered,
        selected: Vec::new(),
        claim: None,
        dispatch_id: None,
        accepted_receipt: None,
        cancellation: None,
    }
}

#[test]
fn exhausted_classifier_keeps_provenance_and_known_client_failures_distinct() {
    assert_eq!(
        exhausted_reason(&CodexErr::ServerOverloaded),
        Some(TurnWaitReason::DescendantRecovery)
    );
    assert_eq!(
        exhausted_reason(
            &CodexErr::ServerOverloaded.with_provider_wait_kind(ProviderWaitKind::Capacity)
        ),
        Some(TurnWaitReason::ProviderCapacity)
    );
    assert_eq!(
        exhausted_reason(&CodexErr::QuotaExceeded),
        Some(TurnWaitReason::ProviderUsageLimit)
    );
    assert_eq!(
        exhausted_reason(&CodexErr::InternalServerError),
        Some(TurnWaitReason::DescendantRecovery)
    );
    for status in [400, 401, 403, 404, 429] {
        let error = CodexErr::RetryLimit(RetryLimitReachedError {
            status: http::StatusCode::from_u16(status).unwrap(),
            request_id: None,
        });
        assert_eq!(exhausted_reason(&error), None);
    }
    assert_eq!(
        exhausted_reason(&CodexErr::RetryLimit(RetryLimitReachedError {
            status: http::StatusCode::REQUEST_TIMEOUT,
            request_id: None,
        })),
        Some(TurnWaitReason::DescendantRecovery)
    );
    assert_eq!(exhausted_reason(&CodexErr::TurnAborted), None);
    assert_eq!(
        exhausted_reason(&CodexErr::InvalidRequest("capacity-looking prose".into())),
        None
    );
}

#[tokio::test(start_paused = true)]
async fn deadline_is_one_shot_exact_and_joined_by_tree_shutdown() -> anyhow::Result<()> {
    let runtime = runtime();
    let actor = ThreadId::new();
    let clock = ClockSample::now();
    let ticket = ticket(&runtime, actor, clock.unix_ms + 1000);
    runtime.remember_deadline(&ticket, clock, None)?;
    assert!(!runtime.wait_readiness(&ticket).deadline_due);
    runtime.arm_wait_deadline(&ticket)?;
    tokio::time::advance(Duration::from_millis(500)).await;
    // A replay cannot restart the original duration or replace its clock.
    runtime.remember_deadline(
        &ticket,
        ClockSample::now(),
        Some(Instant::now() + Duration::from_secs(50)),
    )?;
    runtime.arm_wait_deadline(&ticket)?;
    tokio::time::advance(Duration::from_millis(500)).await;
    tokio::task::yield_now().await;
    assert!(runtime.wait_readiness(&ticket).deadline_due);
    assert_eq!(
        runtime.live_wait_deadline(actor),
        Some(ticket.id),
        "fired clock remains authoritative"
    );
    runtime.retire_wait_deadline(actor, Uuid::now_v7());
    assert!(runtime.wait_readiness(&ticket).deadline_due);
    runtime.retire_wait_deadline(actor, ticket.id);
    assert!(!runtime.wait_readiness(&ticket).deadline_due);
    let pending = self::ticket(&runtime, actor, ClockSample::now().unix_ms + 60_000);
    runtime.remember_deadline(&pending, ClockSample::now(), None)?;
    runtime.arm_wait_deadline(&pending)?;
    runtime.request_shutdown().wait().await?;
    assert!(runtime.admit_start().is_err());
    Ok(())
}

#[tokio::test(start_paused = true)]
async fn cold_observation_does_not_restore_deadline_authority() -> anyhow::Result<()> {
    let live = runtime();
    let actor = ThreadId::new();
    let clock = ClockSample::now();
    let retained = ticket(&live, actor, clock.unix_ms - 1);
    let cold = runtime();
    cold.remember_deadline(&retained, clock, None)?;
    assert_eq!(cold.live_wait_deadline(actor), None);
    assert!(!cold.wait_readiness(&retained).deadline_due);
    assert!(cold.arm_wait_deadline(&retained).is_err());
    live.remember_deadline(
        &retained,
        clock,
        Some(clock.monotonic - Duration::from_secs(1)),
    )?;
    live.arm_wait_deadline(&retained)?;
    assert!(live.wait_readiness(&retained).deadline_due);
    live.request_shutdown().wait().await?;
    cold.request_shutdown().wait().await?;
    Ok(())
}

#[tokio::test]
async fn native_cell_registration_uses_original_deadline_and_cancel_retires_it()
-> anyhow::Result<()> {
    let directory = tempfile::tempdir()?;
    let state = codex_state::StateRuntime::init(
        codex_state::SqliteConfig::new_for_testing(directory.path().abs()),
        "test-provider".into(),
    )
    .await?;
    let runtime = LocalAgentRuntime::new(
        Weak::default(),
        default_thread_id_generator(),
        None,
        Some(state),
    );
    let actor = ThreadId::new();
    runtime.registry.register_root_thread(actor);
    runtime.for_actor_start(actor, None)?;
    let ledger = runtime.work_ledger()?;
    let work = WorkStamp {
        root_id: actor,
        actor_id: actor,
        work_id: Uuid::now_v7(),
    };
    ledger
        .admit_work(
            work,
            None,
            &PublishDelivery {
                id: work.work_id,
                root_id: actor,
                sender_id: None,
                recipient_id: actor,
                work: Some(work),
                kind: DeliveryKind::Message,
                payload: "accepted assignment".into(),
            },
        )
        .await?;
    ledger
        .publish_delivery(&PublishDelivery {
            id: Uuid::now_v7(),
            root_id: actor,
            sender_id: Some(actor),
            recipient_id: actor,
            work: None,
            kind: DeliveryKind::Message,
            payload: "unread addressed message".into(),
        })
        .await?;
    let cell = WaitCell {
        turn_id: "turn".into(),
        item_id: "item".into(),
        window_id: "window".into(),
        cell_id: "cell".into(),
    };
    let options = WaitOptions {
        mode: WaitMode::Any,
        timeout: Some(WaitTimeout {
            requested_ms: 1000,
            effective_ms: 1000,
        }),
        ..Default::default()
    };
    let ticket = runtime.register_native_wait(work, &cell, &options).await?;
    runtime.arm_wait_deadline(&ticket)?;
    tokio::time::pause();
    tokio::time::advance(Duration::from_secs(1)).await;
    tokio::time::resume();
    assert_eq!(
        runtime.register_native_wait(work, &cell, &options).await?,
        ticket
    );
    assert!(runtime.wait_readiness(&ticket).deadline_due);
    runtime.cancel_native_wait(&ticket).await?;
    assert_eq!(runtime.live_wait_deadline(actor), None);
    assert_eq!(
        ledger.observe_wait(work, ticket.id).await?.unwrap().phase,
        WaitPhase::Cancelled
    );
    runtime.request_shutdown().wait().await?;
    Ok(())
}

#[tokio::test]
async fn final_capture_distinguishes_no_selection_from_unavailable_owner() -> anyhow::Result<()> {
    for (answer, ready) in [
        ("ordinary final answer", true),
        ("selected mchanges:amber1", false),
    ] {
        let directory = tempfile::tempdir()?;
        let state = codex_state::StateRuntime::init(
            codex_state::SqliteConfig::new_for_testing(directory.path().abs()),
            "test-provider".into(),
        )
        .await?;
        // No manager and no callback: no-selection finals must not observe cwd or restore anything.
        let runtime = LocalAgentRuntime::new(
            Weak::default(),
            default_thread_id_generator(),
            None,
            Some(state),
        );
        let root = ThreadId::new();
        runtime.registry.register_root_thread(root);
        runtime.for_actor_start(root, None)?;
        let ledger = runtime.work_ledger()?;
        let work = WorkStamp {
            root_id: root,
            actor_id: root,
            work_id: Uuid::now_v7(),
        };
        ledger
            .admit_work(
                work,
                None,
                &PublishDelivery {
                    id: work.work_id,
                    root_id: root,
                    sender_id: None,
                    recipient_id: root,
                    work: Some(work),
                    kind: DeliveryKind::Message,
                    payload: "accepted input".into(),
                },
            )
            .await?;
        ledger.bind_turn(work, "final-turn").await?;
        let checkpoint = runtime
            .prepare_successful_completion(root, "final-turn", Some(answer))
            .await?
            .unwrap();
        let acquired = runtime.prepare_completion_capture(&checkpoint).await;
        assert_eq!(acquired.is_ok(), ready);
        let pending = ledger.pending_completion(work).await?.unwrap();
        assert_eq!(pending.capture_ready, ready);
        assert_eq!(pending.answers, vec![answer]);
        assert_eq!(pending.capture, None);
        if !ready {
            assert!(pending.detail.as_deref().unwrap().contains("not connected"));
        }
        runtime.finish_ready_completions(work).await?;
        assert!(
            ledger.terminal_delivery(work).await?.is_none(),
            "capture readiness cannot bypass execution settlement"
        );
        let settled = runtime.settle_completion_checkpoint(&checkpoint).await?;
        assert!(matches!(
            (settled, ready),
            (CompletionSettlement::Published(_), true) | (CompletionSettlement::Pending, false)
        ));
        runtime.request_shutdown().wait().await?;
    }
    Ok(())
}

#[tokio::test]
async fn exhausted_provider_advice_keeps_its_original_clock_through_handoff() -> anyhow::Result<()>
{
    let directory = tempfile::tempdir()?;
    let state = codex_state::StateRuntime::init(
        codex_state::SqliteConfig::new_for_testing(directory.path().abs()),
        "test-provider".into(),
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
    runtime.for_actor_start(root, None)?;
    let ledger = runtime.work_ledger()?;
    let work = WorkStamp {
        root_id: root,
        actor_id: root,
        work_id: Uuid::now_v7(),
    };
    ledger
        .admit_work(
            work,
            None,
            &PublishDelivery {
                id: work.work_id,
                root_id: root,
                sender_id: None,
                recipient_id: root,
                work: Some(work),
                kind: DeliveryKind::Message,
                payload: "input".into(),
            },
        )
        .await?;
    ledger.bind_turn(work, "failed-request").await?;
    let advice = codex_http_client::RetryAfter::from_delay(Duration::from_secs(10)).unwrap();
    let deadline = advice.deadline();
    tokio::time::pause();
    tokio::time::advance(Duration::from_secs(4)).await;
    tokio::time::resume();
    let error = CodexErr::ServerOverloaded
        .with_provider_wait_kind(ProviderWaitKind::Capacity)
        .with_retry_after(advice);
    let ticket = runtime
        .recover_exhausted_turn(root, "failed-request", &error, None)
        .await?
        .unwrap();
    assert_eq!(
        runtime
            .wait_deadlines
            .entries
            .lock()
            .unwrap()
            .get(&root)
            .unwrap()
            .deadline,
        Some(deadline)
    );
    assert!(!runtime.wait_readiness(&ticket).deadline_due);
    tokio::time::pause();
    tokio::time::advance(Duration::from_secs(6)).await;
    tokio::time::resume();
    assert!(runtime.wait_readiness(&ticket).deadline_due);
    runtime.cancel_native_wait(&ticket).await?;
    ledger.bind_turn(work, "expired-advice").await?;
    tokio::time::pause();
    tokio::time::advance(Duration::from_secs(2)).await;
    tokio::time::resume();
    let expired = runtime
        .recover_exhausted_turn(root, "expired-advice", &error, None)
        .await?
        .unwrap();
    assert_eq!(
        runtime
            .wait_deadlines
            .entries
            .lock()
            .unwrap()
            .get(&root)
            .unwrap()
            .deadline,
        Some(deadline)
    );
    assert!(runtime.wait_readiness(&expired).deadline_due);
    let sample = ClockSample::now();
    assert_eq!(
        sample.unix_at(sample.monotonic - Duration::from_secs(2))?,
        sample.unix_ms - 2000
    );
    runtime.request_shutdown().wait().await?;
    Ok(())
}

#[tokio::test(start_paused = true)]
async fn progress_only_clock_keeps_registration_origin_and_never_rearms_on_cold_load()
-> anyhow::Result<()> {
    let runtime = runtime();
    let actor = ThreadId::new();
    let clock = ClockSample::now();
    let mut ticket = ticket(&runtime, actor, clock.unix_ms + 1000);
    ticket.deadline_unix_ms = None;
    ticket.options.progress_interval_ms = Some(300_000);
    runtime.remember_deadline(&ticket, clock, None)?;
    let origin = runtime
        .wait_deadlines
        .entries
        .lock()
        .unwrap()
        .get(&actor)
        .unwrap()
        .next_progress;
    assert_eq!(origin, Some(clock.monotonic + Duration::from_secs(300)));
    tokio::time::advance(Duration::from_secs(400)).await;
    runtime.remember_deadline(&ticket, ClockSample::now(), None)?;
    assert_eq!(
        runtime
            .wait_deadlines
            .entries
            .lock()
            .unwrap()
            .get(&actor)
            .unwrap()
            .next_progress,
        origin,
        "delayed enclosing execution/reset settlement cannot restart the interval"
    );
    let cold = self::runtime();
    cold.remember_deadline(&ticket, ClockSample::now(), None)?;
    assert!(cold.live_wait_deadline(actor).is_none());
    assert!(cold.arm_wait_deadline(&ticket).is_err());
    runtime.stop_wait_collection(actor, ticket.id);
    assert!(
        runtime
            .progress_cancellation(actor, ticket.id)
            .unwrap()
            .is_cancelled()
    );
    assert_eq!(
        runtime.live_wait_deadline(actor),
        Some(ticket.id),
        "priority cancellation retains original clock evidence until dispatch"
    );
    runtime.request_shutdown().wait().await?;
    cold.request_shutdown().wait().await?;
    Ok(())
}

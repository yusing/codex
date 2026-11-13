use super::*;
use crate::session::completed_session_loop_termination;
use codex_protocol::error::CodexErrorDetails;
use codex_protocol::protocol::AgentStatus;
use codex_protocol::protocol::ErrorEvent;
use codex_protocol::protocol::Event;
use codex_protocol::protocol::TurnAbortReason;
use codex_protocol::protocol::TurnAbortedEvent;
use codex_protocol::protocol::TurnCompleteEvent;
use futures::FutureExt;
use pretty_assertions::assert_eq;
use std::task::Poll;
use std::time::Duration;
use tokio::sync::watch;

fn user_message(text: &str) -> ResponseItem {
    ResponseItem::Message {
        id: None,
        role: "user".to_owned(),
        content: vec![ContentItem::InputText {
            text: text.to_owned(),
        }],
        status: None,
        encrypted_content: None,
        phase: None,
        internal_chat_message_metadata_passthrough: None,
    }
}

fn completed(answer: Option<&str>, error: Option<ErrorEvent>) -> EventMsg {
    EventMsg::TurnComplete(TurnCompleteEvent {
        root_turn_id: None,
        turn_id: "answer-turn".to_owned(),
        last_agent_message: answer.map(str::to_owned),
        error,
        started_at: None,
        completed_at: None,
        duration_ms: None,
        time_to_first_token_ms: None,
    })
}

fn answer_io() -> (async_channel::Sender<Event>, SessionIo) {
    let (events_tx, events_rx) = async_channel::bounded(4);
    let (tx_sub, rx_sub) = async_channel::bounded(1);
    drop(rx_sub);
    let (_status_tx, agent_status) = watch::channel(AgentStatus::PendingInit);
    (
        events_tx,
        SessionIo {
            tx_sub,
            rx_event: events_rx,
            agent_status,
            session_loop_termination: completed_session_loop_termination(),
        },
    )
}

#[tokio::test]
async fn preparation_freezes_native_history_and_preserves_original_input() {
    let (target, context, _events) =
        crate::session::tests::make_session_and_context_with_rx().await;
    target
        .record_conversation_items(
            &context,
            context.model_info(),
            &[user_message("existing target work")],
        )
        .await;
    let before = target.clone_history().await;
    let question = "question with @\"file name\" and literal $()";
    let frozen = "already acquired bytes, including a failed read";
    let prepared = prepare_side_question(
        &target,
        &context,
        question.to_owned(),
        Some(frozen.to_owned()),
        "test boundary instructions".to_owned(),
    )
    .await;
    assert!(prepared.config.ephemeral);
    assert_eq!(
        prepared.config.permissions.approval_policy.value(),
        AskForApproval::Never,
    );
    assert_eq!(
        prepared.input,
        vec![
            UserInput::Text {
                text: question.to_owned(),
                text_elements: Vec::new()
            },
            UserInput::Text {
                text: frozen.to_owned(),
                text_elements: Vec::new()
            },
        ],
    );
    assert_eq!(
        target.clone_history().await.history_version(),
        before.history_version(),
    );
    assert_eq!(
        target.clone_history().await.annotated_items(),
        before.annotated_items(),
    );
    target
        .record_conversation_items(
            &context,
            context.model_info(),
            &[user_message("later target input")],
        )
        .await;
    let InitialHistory::Forked(items) = prepared.history else {
        panic!("side question must use native fork initialization");
    };
    let history: Vec<_> = items
        .iter()
        .filter_map(|item| match item {
            RolloutItem::ResponseItem(envelope) => Some(envelope),
            _ => None,
        })
        .collect();
    assert_eq!(history.len(), before.annotated_items().len() + 1);
    for (actual, expected) in history.iter().zip(before.annotated_items()) {
        assert_eq!(*actual, expected);
    }
    assert!(matches!(
        &history.last().unwrap().item,
        ResponseItem::Message { role, .. } if role == "developer"
    ));
    let second = prepare_side_question(
        &target,
        &context,
        "second independent question".to_owned(),
        None,
        "second boundary".to_owned(),
    )
    .await;
    assert_eq!(second.input.len(), 1);
    assert_eq!(
        second.history.get_rollout_items().len(),
        target.clone_history().await.annotated_items().len() + 1,
    );
}

#[tokio::test]
async fn terminal_answer_waits_for_private_execution_to_join() {
    let (events, mut io) = answer_io();
    let (joined_tx, joined_rx) = oneshot::channel();
    io.session_loop_termination = async move { joined_rx.await.unwrap() }.boxed().shared();
    events
        .send(Event {
            id: "terminal".to_owned(),
            msg: completed(Some("answer"), None),
        })
        .await
        .unwrap();
    let mut answer = Box::pin(collect_answer(io));
    assert!(matches!(futures::poll!(answer.as_mut()), Poll::Pending));
    joined_tx.send(()).unwrap();
    assert_eq!(answer.await.unwrap(), "answer");
}

#[tokio::test]
async fn failed_partial_and_missing_answers_are_not_successful_results() {
    for event in [
        completed(
            Some("partial"),
            Some(ErrorEvent {
                message: "provider failed".to_owned(),
                codex_error_info: None,
                rejected_audio_file_ids: Vec::new(),
                misalignment: None,
            }),
        ),
        completed(None, None),
        completed(Some("  \n"), None),
        EventMsg::TurnAborted(TurnAbortedEvent {
            root_turn_id: None,
            turn_id: Some("answer-turn".to_owned()),
            started_at: None,
            reason: TurnAbortReason::Interrupted,
            error: None,
            completed_at: None,
            duration_ms: None,
        }),
    ] {
        let (events, io) = answer_io();
        events
            .send(Event {
                id: "terminal".to_owned(),
                msg: event,
            })
            .await
            .unwrap();
        assert!(collect_answer(io).await.is_err());
    }
    let (events, io) = answer_io();
    drop(events);
    assert!(matches!(
        collect_answer(io).await.unwrap_err().details(),
        CodexErrorDetails::InternalAgentDied,
    ));
}

#[tokio::test]
async fn accepted_answer_remains_tree_owned_after_caller_abandonment() {
    let (target, context, _events) =
        crate::session::tests::make_session_and_context_with_rx().await;
    let before = target.clone_history().await.history_version();
    let answer = start_side_question(
        Arc::clone(&target),
        context,
        "question".to_owned(),
        None,
        "test boundary".to_owned(),
        SideQuestionOperation {
            caller: target.thread_id(),
            operation_id: "abandoned".into(),
            target_stamp: None,
        },
    )
    .await
    .expect("accept answer");
    drop(answer);
    let shutdown = target.services.local_agent_runtime.request_shutdown();
    tokio::time::timeout(Duration::from_secs(1), shutdown.wait())
        .await
        .expect("shutdown must join the abandoned accepted operation")
        .expect("cancellation should clean up without an abandoned tree guard");
    assert_eq!(target.clone_history().await.history_version(), before);
}

#[tokio::test]
async fn shutdown_fence_rejects_side_question_admission() {
    let (target, context, _events) =
        crate::session::tests::make_session_and_context_with_rx().await;
    let shutdown = target.services.local_agent_runtime.request_shutdown();
    let caller = target.thread_id();
    let result = start_side_question(
        target,
        context,
        "question".to_owned(),
        None,
        "test boundary".to_owned(),
        SideQuestionOperation {
            caller,
            operation_id: "shutdown".into(),
            target_stamp: None,
        },
    )
    .await;
    assert!(result.is_err());
    shutdown.wait().await.unwrap();
}

#[tokio::test]
async fn result_finalization_rejects_changed_work_without_starting_or_consuming_it()
-> anyhow::Result<()> {
    use crate::agent::control::LocalAgentRuntime;
    use crate::agent::control::work::DeliveryKind;
    use crate::agent::control::work::PublishDelivery;
    use crate::agent::control::work::WorkStamp;
    use codex_utils_absolute_path::test_support::PathExt;

    let directory = tempfile::tempdir()?;
    let state = codex_state::StateRuntime::init(
        codex_state::SqliteConfig::new_for_testing(directory.path().abs()),
        "test-provider".into(),
    )
    .await?;
    let runtime = LocalAgentRuntime::new(
        std::sync::Weak::default(),
        crate::thread_manager::default_thread_id_generator(),
        None,
        Some(state),
    );
    let target = codex_protocol::ThreadId::new();
    runtime.registry.register_root_thread(target);
    let operation = SideQuestionOperation {
        caller: target,
        operation_id: "answer".into(),
        target_stamp: None,
    };
    assert_eq!(
        finish_side_question(&runtime, target, &operation, Ok("independent".into())).await?,
        "independent"
    );
    let ledger = runtime.work_ledger()?;
    assert_eq!(ledger.work_stamp(target).await?, None);
    let stamp = WorkStamp {
        root_id: target,
        actor_id: target,
        work_id: uuid::Uuid::now_v7(),
    };
    ledger
        .admit_work(
            stamp,
            None,
            &PublishDelivery {
                id: stamp.work_id,
                root_id: target,
                sender_id: None,
                recipient_id: target,
                work: Some(stamp),
                kind: DeliveryKind::Message,
                payload: "new accepted work".into(),
            },
        )
        .await?;
    let before = ledger.observe_current(target).await?;
    let error = finish_side_question(&runtime, target, &operation, Ok("stale".into()))
        .await
        .unwrap_err();
    assert!(error.to_string().contains("target work changed"));
    assert_eq!(ledger.observe_current(target).await?, before);
    assert!(
        !ledger
            .observe_delivery(target, stamp.work_id)
            .await?
            .unwrap()
            .consumed
    );
    let current = SideQuestionOperation {
        target_stamp: Some(stamp),
        ..operation
    };
    assert_eq!(
        finish_side_question(&runtime, target, &current, Ok("current".into())).await?,
        "current"
    );
    assert!(
        finish_side_question(&runtime, target, &current, Err(CodexErr::TurnAborted))
            .await
            .is_err()
    );
    Ok(())
}

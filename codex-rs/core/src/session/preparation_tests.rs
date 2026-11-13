use super::*;
use crate::session::tests::make_session_and_context_with_rx;
use crate::state::ActiveTurn;
use codex_protocol::approvals::ExecApprovalKind;
use codex_protocol::protocol::ExecApprovalRequestEvent;
use codex_protocol::protocol::ReviewDecision;

fn request(
    session: &Arc<Session>,
    turn: &Arc<TurnContext>,
) -> tokio::task::JoinHandle<ReviewDecision> {
    let session = Arc::clone(session);
    let turn = Arc::clone(turn);
    tokio::spawn(async move {
        #[allow(deprecated)]
        let cwd = turn.cwd.clone();
        session
            .request_command_approval(
                &turn,
                ExecApprovalKind::HumanCommand,
                crate::guardian::GuardianReviewContext::from(Arc::clone(&turn)).model_context(),
                "source-reader".into(),
                None,
                None,
                None,
                vec!["mchanges".into(), "receipt1".into()],
                cwd.into(),
                None,
                None,
                None,
                None,
                None,
                None,
            )
            .await
    })
}

async fn prompt(events: &async_channel::Receiver<Event>) -> ExecApprovalRequestEvent {
    loop {
        match events.recv().await.expect("open event receiver").msg {
            EventMsg::ExecApprovalRequest(event) => return event,
            EventMsg::CommandApprovalResolved { .. } => {}
            event => panic!("unexpected event: {event:?}"),
        }
    }
}

#[tokio::test]
async fn preparation_approval_needs_no_active_turn_and_requires_exact_context() {
    let (session, turn, events) = make_session_and_context_with_rx().await;
    let cancellation = CancellationToken::new();
    let preparation = session.register_preparation(&turn, &cancellation).unwrap();
    let request = request(&session, &turn);
    let event = prompt(&events).await;
    assert!(session.active_turn.lock().await.is_none());
    session
        .resolve_exec_approval(event.call_id.clone(), None, ReviewDecision::Abort)
        .await;
    session
        .resolve_exec_approval(
            event.call_id.clone(),
            Some("different-preparation".into()),
            ReviewDecision::Abort,
        )
        .await;
    assert!(!request.is_finished());
    session
        .resolve_exec_approval(event.call_id, Some(event.turn_id), ReviewDecision::Approved)
        .await;
    assert_eq!(request.await.unwrap(), ReviewDecision::Approved);
    preparation.finish().await;
    assert!(!cancellation.is_cancelled());
    assert!(session.active_turn.lock().await.is_none());
}

#[tokio::test]
async fn unowned_command_does_not_publish_a_doomed_prompt() {
    let (session, turn, events) = make_session_and_context_with_rx().await;
    assert_eq!(
        request(&session, &turn).await.unwrap(),
        ReviewDecision::Abort
    );
    assert!(events.try_recv().is_err());
}

#[tokio::test]
async fn cancel_preparation_preserves_the_primary_turn_and_late_response_cannot_approve_next() {
    let (session, turn, events) = make_session_and_context_with_rx().await;
    *session.active_turn.lock().await = Some(ActiveTurn::default());
    let primary_state = session
        .active_turn
        .lock()
        .await
        .as_ref()
        .unwrap()
        .turn_state
        .clone();
    let cancellation = CancellationToken::new();
    let preparation = session.register_preparation(&turn, &cancellation).unwrap();
    let first = request(&session, &turn);
    let old = prompt(&events).await;
    session
        .resolve_exec_approval(
            old.call_id.clone(),
            Some(old.turn_id.clone()),
            ReviewDecision::Abort,
        )
        .await;
    assert_eq!(first.await.unwrap(), ReviewDecision::Abort);
    assert!(preparation.cancellation().is_cancelled());
    assert!(!cancellation.is_cancelled());
    assert!(Arc::ptr_eq(
        &primary_state,
        &session
            .active_turn
            .lock()
            .await
            .as_ref()
            .unwrap()
            .turn_state,
    ));
    preparation.finish().await;

    let next_turn = session.new_default_turn().await;
    assert_ne!(next_turn.sub_id, old.turn_id);
    let next_preparation = session
        .register_preparation(&next_turn, &cancellation)
        .unwrap();
    let next = request(&session, &next_turn);
    let event = prompt(&events).await;
    session
        .resolve_exec_approval(old.call_id, Some(old.turn_id), ReviewDecision::Approved)
        .await;
    assert!(!next.is_finished());
    let denied = ReviewDecision::denied("source denied");
    session
        .resolve_exec_approval(event.call_id, Some(event.turn_id), denied.clone())
        .await;
    assert_eq!(next.await.unwrap(), denied);
    assert!(!next_preparation.cancellation().is_cancelled());
    next_preparation.finish().await;
}

#[tokio::test]
async fn dropped_preparation_retires_prompt_and_shutdown_joins_execution_cleanup() {
    let (session, turn, events) = make_session_and_context_with_rx().await;
    let preparation = session
        .register_preparation(&turn, &CancellationToken::new())
        .unwrap();
    let execution = session.preparation_execution_token(&turn.sub_id).unwrap();
    let request = request(&session, &turn);
    let event = prompt(&events).await;
    drop(preparation);
    assert_eq!(request.await.unwrap(), ReviewDecision::Abort);
    let resolution = events.recv().await.unwrap();
    assert_eq!(resolution.id, event.turn_id);
    assert!(
        matches!(resolution.msg, EventMsg::CommandApprovalResolved { approval_id } if approval_id == event.call_id)
    );
    let shutdown = session.shutdown_preparations();
    tokio::pin!(shutdown);
    assert!(futures::poll!(shutdown.as_mut()).is_pending());
    drop(execution);
    shutdown.await;
    assert!(
        session
            .register_preparation(&turn, &CancellationToken::new())
            .is_err()
    );
}

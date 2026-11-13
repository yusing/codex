use super::*;
use crate::agent::LocalAgentControl;
use crate::session::session::Session;
use crate::session::step_context::StepContext;
use crate::session::tests::make_session_and_context;
use crate::session::turn_context::TurnContext;
use crate::thread_manager::default_thread_id_generator;
use crate::tools::context::ToolCallOrigin;
use crate::turn_diff_tracker::TurnDiffTracker;
use codex_protocol::ResponseItemId;
use codex_state::StateRuntime;
use codex_state::coordination::DeliveryKind;
use codex_state::coordination::PublishDelivery;
use codex_state::coordination::WorkStamp;
use codex_state::coordination_wait::WaitPhase;
use codex_utils_absolute_path::test_support::PathExt;
use serde_json::json;
use std::sync::Arc;
use std::sync::Weak;
use tempfile::TempDir;
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

async fn fixture() -> (TempDir, Arc<Session>, Arc<TurnContext>) {
    let directory = TempDir::new().unwrap();
    let state = StateRuntime::init(
        codex_state::SqliteConfig::new_for_testing(directory.path().abs()),
        "test-provider".to_owned(),
    )
    .await
    .unwrap();
    let control = LocalAgentControl::new(
        Weak::default(),
        default_thread_id_generator(),
        None,
        Some(state),
    );
    let (mut session, mut turn) = make_session_and_context().await;
    control
        .runtime
        .register_session_root(session.thread_id, None);
    session.services.local_agent_runtime = control.runtime.clone();
    session.services.agent_control = Arc::new(control);
    let mut config = (*turn.config).clone();
    config.multi_agent_v2.min_wait_timeout_ms = 50;
    config.multi_agent_v2.max_wait_timeout_ms = 1_000;
    config.multi_agent_v2.default_wait_timeout_ms = 100;
    turn.config = Arc::new(config);
    let ledger = session.services.local_agent_runtime.work_ledger().unwrap();
    let caller = WorkStamp {
        root_id: session.thread_id,
        actor_id: session.thread_id,
        work_id: Uuid::now_v7(),
    };
    let input = PublishDelivery {
        id: caller.work_id,
        root_id: caller.root_id,
        sender_id: None,
        recipient_id: caller.actor_id,
        work: Some(caller),
        kind: DeliveryKind::Message,
        payload: "accepted input".to_owned(),
    };
    ledger.admit_work(caller, None, &input).await.unwrap();
    let dispatch = input.id;
    ledger
        .begin_delivery(caller.actor_id, &[input.id], None, dispatch)
        .await
        .unwrap();
    ledger
        .confirm_delivery(caller.actor_id, &[input.id], dispatch, "initial-input")
        .await
        .unwrap();
    ledger
        .consume_deliveries(caller.actor_id, &[input.id], "initial-input")
        .await
        .unwrap();
    (directory, Arc::new(session), Arc::new(turn))
}

fn invocation(
    session: &Arc<Session>,
    turn: &Arc<TurnContext>,
    cell: &str,
    args: JsonValue,
) -> ToolInvocation {
    session
        .services
        .code_mode_service
        .mark_cell_ready_for_dispatch(
            &CellId::new(cell.to_owned()),
            Some(ToolCallOrigin {
                item_id: Some(ResponseItemId::from_server(format!("item-{cell}"))),
                window_id: "original-window".to_owned(),
            }),
        );
    ToolInvocation {
        session: Arc::clone(session),
        turn: Arc::clone(turn),
        step_context: StepContext::for_test(Arc::clone(turn)),
        cancellation_token: CancellationToken::new(),
        tracker: Arc::new(Mutex::new(TurnDiffTracker::default())),
        call_id: format!("call-{cell}"),
        tool_name: ToolName::plain("wait_agent"),
        source: ToolCallSource::CodeMode {
            cell_id: cell.to_owned(),
            runtime_tool_call_id: "nested-call".to_owned(),
        },
        payload: ToolPayload::Function {
            arguments: args.to_string(),
        },
        native_call: None,
        environments: Default::default(),
    }
}

async fn addressed_message(session: &Session) -> Uuid {
    let id = Uuid::now_v7();
    session
        .services
        .local_agent_runtime
        .work_ledger()
        .unwrap()
        .publish_delivery(&PublishDelivery {
            id,
            root_id: session.thread_id,
            sender_id: None,
            recipient_id: session.thread_id,
            work: None,
            kind: DeliveryKind::Message,
            payload: serde_json::to_string(
                &crate::agent::control::work_lifecycle::CoordinationPayload::UserInput {
                    content: vec![codex_protocol::user_input::UserInput::Text {
                        text: "retained addressed content".to_owned(),
                        text_elements: Vec::new(),
                    }],
                },
            )
            .unwrap(),
        })
        .await
        .unwrap();
    id
}

#[tokio::test]
async fn ready_mail_returns_on_original_call_without_consuming_it() {
    let (_directory, session, turn) = fixture().await;
    let id = addressed_message(&session).await;
    let output = Handler::default()
        .handle(invocation(&session, &turn, "cell", json!({})))
        .await
        .unwrap();
    let result: serde_json::Value = serde_json::from_str(&output.log_output()).unwrap();
    assert!(result.to_string().contains("retained addressed content"));
    let ledger = session.services.local_agent_runtime.work_ledger().unwrap();
    let ticket = ledger
        .current_wait(session.thread_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(ticket.phase, WaitPhase::Claimed);
    assert_eq!(ticket.options.mode, WaitMode::Any);
    assert_eq!(ticket.options.timeout, None);
    assert_eq!(ticket.deadline_unix_ms, None);
    let cell = ticket.origin.cell().expect("native cell origin");
    assert_eq!(cell.item_id, "item-cell");
    assert_eq!(cell.window_id, "original-window");
    assert!(
        !ledger
            .observe_delivery(session.thread_id, id)
            .await
            .unwrap()
            .unwrap()
            .consumed
    );
    // A second invocation in the same native cell reuses the durable intent.
    Handler::default()
        .handle(invocation(&session, &turn, "cell", json!({})))
        .await
        .unwrap();
    assert_eq!(
        ledger.current_wait(session.thread_id).await.unwrap(),
        Some(ticket.clone())
    );
    let error = Handler::default()
        .handle(invocation(&session, &turn, "different", json!({})))
        .await
        .err()
        .unwrap();
    assert!(error.to_string().contains("different native cell"));
    assert_eq!(
        ledger.current_wait(session.thread_id).await.unwrap(),
        Some(ticket)
    );
}

#[tokio::test]
async fn zero_effective_timeout_returns_on_original_call() {
    let (_directory, session, turn) = fixture().await;
    let mut turn = Arc::try_unwrap(turn).expect("fixture owns turn");
    let config = Arc::make_mut(&mut turn.config);
    config.multi_agent_v2.min_wait_timeout_ms = 0;
    config.multi_agent_v2.max_wait_timeout_ms = 0;
    let turn = Arc::new(turn);
    addressed_message(&session).await;
    Handler::default()
        .handle(invocation(&session, &turn, "cell", json!({"timeout_ms":0})))
        .await
        .unwrap();
    let ticket = session
        .services
        .local_agent_runtime
        .work_ledger()
        .unwrap()
        .current_wait(session.thread_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(ticket.options.timeout.unwrap().effective_ms, 0);
    assert_eq!(ticket.phase, WaitPhase::Claimed);
}

#[tokio::test]
async fn compact_and_progress_options_preserve_empty_reload_selection() {
    let (_directory, session, turn) = fixture().await;
    addressed_message(&session).await;
    Handler::default()
        .handle(invocation(
            &session,
            &turn,
            "cell",
            json!({
                "compact":true, "reload":{"sources":[]}, "yield_time_ms":300_000,
            }),
        ))
        .await
        .unwrap();
    let ticket = session
        .services
        .local_agent_runtime
        .work_ledger()
        .unwrap()
        .current_wait(session.thread_id)
        .await
        .unwrap()
        .unwrap();
    assert!(ticket.options.compact);
    assert_eq!(
        ticket.options.reload,
        Some(ReloadSelection {
            sources: Vec::new()
        })
    );
    assert_eq!(ticket.options.progress_interval_ms, Some(300_000));
    assert_eq!(ticket.phase, WaitPhase::Claimed);
}

#[tokio::test]
async fn rejected_options_and_empty_selection_leave_no_wait() {
    for (args, diagnostic) in [
        (
            json!({"timeout_ms":1_001}),
            "timeout_ms must be at most 1000",
        ),
        (
            json!({"yield_time_ms":299_999}),
            "yield_time_ms must be at least 300000",
        ),
        (
            json!({"reload":{"sources":[]}}),
            "reload requires compact: true",
        ),
        (
            json!({}),
            "no eligible descendant work or addressed message",
        ),
    ] {
        let (_directory, session, turn) = fixture().await;
        let error = Handler::default()
            .handle(invocation(&session, &turn, "cell", args))
            .await
            .err()
            .unwrap();
        assert!(error.to_string().contains(diagnostic), "{error}");
        assert!(
            session
                .services
                .local_agent_runtime
                .work_ledger()
                .unwrap()
                .current_wait(session.thread_id)
                .await
                .unwrap()
                .is_none()
        );
    }
}

#[tokio::test]
async fn non_cell_and_cancelled_invocations_cannot_register_waits() {
    let (_directory, session, turn) = fixture().await;
    addressed_message(&session).await;
    let mut direct = invocation(&session, &turn, "direct", json!({}));
    direct.source = ToolCallSource::Direct;
    let error = Handler::default().handle(direct).await.err().unwrap();
    assert!(
        error
            .to_string()
            .contains("enclosing JavaScript execution cell")
    );
    let canceled = invocation(&session, &turn, "cancelled", json!({}));
    canceled.cancellation_token.cancel();
    let error = Handler::default().handle(canceled).await.err().unwrap();
    assert!(error.to_string().contains("wait registration canceled"));
    assert!(
        session
            .services
            .local_agent_runtime
            .work_ledger()
            .unwrap()
            .current_wait(session.thread_id)
            .await
            .unwrap()
            .is_none()
    );
}

//! Pending-frontier observations use the same paused runtime over both transports.

#![allow(clippy::expect_used)]

use std::sync::Arc;

use codex_code_mode::CellId;
use codex_code_mode::CodeModeNestedToolCall;
use codex_code_mode::CodeModeOutputTruncation;
use codex_code_mode::CodeModeSession;
use codex_code_mode::CodeModeSessionDelegate;
use codex_code_mode::CodeModeSessionOptions;
use codex_code_mode::CodeModeSessionProvider;
use codex_code_mode::CodeModeToolKind;
use codex_code_mode::ExecuteRequest;
use codex_code_mode::ExecuteToPendingOutcome;
use codex_code_mode::FunctionCallOutputContentItem;
use codex_code_mode::GrpcCodeModeSessionProvider;
use codex_code_mode::NotificationFuture;
use codex_code_mode::ProcessOwnedCodeModeSessionProvider;
use codex_code_mode::RuntimeResponse;
use codex_code_mode::ToolDefinition;
use codex_code_mode::ToolInvocationFuture;
use codex_code_mode::WaitOutcome;
use codex_code_mode::WaitRequest;
use codex_code_mode::WaitToPendingOutcome;
use codex_code_mode::WaitToPendingRequest;
use codex_protocol::ToolName;
use pretty_assertions::assert_eq;
use tokio::sync::Semaphore;
use tokio_util::sync::CancellationToken;

#[path = "support/host.rs"]
mod host;

struct FrontierDelegate {
    release: Semaphore,
    entered: Semaphore,
    notified: CancellationToken,
}

impl Default for FrontierDelegate {
    fn default() -> Self {
        Self {
            release: Semaphore::new(0),
            entered: Semaphore::new(0),
            notified: CancellationToken::new(),
        }
    }
}

impl CodeModeSessionDelegate for FrontierDelegate {
    fn invoke_tool<'a>(
        &'a self,
        _invocation: CodeModeNestedToolCall,
        cancellation: CancellationToken,
    ) -> ToolInvocationFuture<'a> {
        self.entered.add_permits(1);
        Box::pin(async move {
            tokio::select! {
                permit = self.release.acquire() => {
                    permit.map_err(|error| error.to_string())?.forget();
                    Ok(serde_json::Value::Null)
                }
                _ = cancellation.cancelled() => Err("cancelled".to_string()),
            }
        })
    }

    fn notify<'a>(
        &'a self,
        _call_id: String,
        _cell_id: CellId,
        _text: String,
        _cancellation: CancellationToken,
    ) -> NotificationFuture<'a> {
        self.notified.cancel();
        Box::pin(async { Ok(()) })
    }

    fn cell_closed(&self, _cell_id: &CellId) {}
}

fn request(source: &str) -> ExecuteRequest {
    ExecuteRequest {
        tool_call_id: "pending-frontier".to_string(),
        enabled_tools: vec![ToolDefinition {
            name: "hold".to_string(),
            tool_name: ToolName::plain("hold"),
            description: String::new(),
            kind: CodeModeToolKind::Function,
            input_schema: None,
            input_schema_max_bytes: None,
            output_schema: None,
        }],
        source: source.to_string(),
        yield_time_ms: Some(60_000),
        max_output_tokens: None,
    }
}

async fn start_held_cell(
    session: &dyn CodeModeSession,
    delegate: Arc<FrontierDelegate>,
    source: &str,
) -> CellId {
    let preempt = CancellationToken::new();
    let started = session
        .execute(request(source), delegate.clone(), Some(preempt.clone()))
        .await
        .expect("start cell");
    delegate
        .entered
        .acquire()
        .await
        .expect("tool entered")
        .forget();
    preempt.cancel();
    let cell_id = started.cell_id.clone();
    assert!(matches!(
        started
            .initial_response()
            .await
            .expect("preempt at held callback"),
        RuntimeResponse::Yielded { .. }
    ));
    cell_id
}

async fn assert_pending_frontiers(provider: &dyn CodeModeSessionProvider) {
    let session = provider
        .create_session_with_options(CodeModeSessionOptions {
            output_truncation: CodeModeOutputTruncation::Server,
            ..CodeModeSessionOptions::default()
        })
        .await
        .expect("create server-budgeted session");
    let delegate = Arc::new(FrontierDelegate::default());
    let cell_id = start_held_cell(
        session.as_ref(),
        delegate.clone(),
        r#"await tools.hold({}); text("x".repeat(1000)); await tools.hold({}); text("after");"#,
    )
    .await;
    let observation = || WaitToPendingRequest {
        cell_id: cell_id.clone(),
        max_output_tokens: Some(16),
    };
    let first = session
        .wait_to_pending(observation())
        .await
        .expect("first frontier");
    let WaitToPendingOutcome::LiveCell(ExecuteToPendingOutcome::Pending {
        cell_id: first_cell,
        content_items,
        pending_tool_call_ids: first_calls,
    }) = first
    else {
        panic!("expected first pending frontier: {first:?}");
    };
    assert_eq!(first_cell, cell_id);
    assert!(content_items.is_empty());
    // A tool already admitted before the observation may precede this frontier's
    // callback-ID batch; it remains pending in the same live cell.
    assert!(first_calls.len() <= 1);

    delegate.release.add_permits(1);
    let mut content_items = Vec::new();
    let mut second_calls = Vec::new();
    let second_cell = loop {
        let second = session
            .wait_to_pending(observation())
            .await
            .expect("next frontier");
        let WaitToPendingOutcome::LiveCell(ExecuteToPendingOutcome::Pending {
            cell_id: second_cell,
            content_items: items,
            pending_tool_call_ids,
        }) = second
        else {
            panic!("expected pending frontier: {second:?}");
        };
        content_items.extend(items);
        second_calls.extend(pending_tool_call_ids);
        if !content_items.is_empty() && !second_calls.is_empty() {
            break second_cell;
        }
    };
    assert_eq!(second_cell, cell_id);
    assert_eq!(second_calls.len(), 1);
    assert_ne!(second_calls, first_calls);
    assert!(content_items.iter().any(|item| matches!(item,
        FunctionCallOutputContentItem::InputText { text } if text.starts_with("Warning: truncated output")
    )));

    delegate.release.add_permits(1);
    let completed = loop {
        let outcome = session
            .wait_to_pending(observation())
            .await
            .expect("completion frontier");
        match outcome {
            WaitToPendingOutcome::LiveCell(ExecuteToPendingOutcome::Pending {
                content_items,
                pending_tool_call_ids,
                ..
            }) => {
                assert!(content_items.is_empty());
                assert!(pending_tool_call_ids.is_empty());
            }
            completed => break completed,
        }
    };
    let WaitToPendingOutcome::LiveCell(ExecuteToPendingOutcome::Completed(
        RuntimeResponse::Result {
            cell_id: completed_cell,
            content_items,
            error_text,
            code_mode_host_duration,
            ..
        },
    )) = completed
    else {
        panic!("expected completed cell: {completed:?}");
    };
    assert_eq!(completed_cell, cell_id);
    assert_eq!(error_text, None);
    assert!(code_mode_host_duration.is_some());
    assert_eq!(
        content_items,
        vec![FunctionCallOutputContentItem::InputText {
            text: "after".to_string()
        }]
    );
    assert!(matches!(
        session
            .wait_to_pending(observation())
            .await
            .expect("missing cell"),
        WaitToPendingOutcome::MissingCell(_)
    ));
    session.shutdown().await.expect("shutdown session");
}

async fn assert_cancel_pending_observation(provider: &dyn CodeModeSessionProvider) {
    let session = provider.create_session().await.expect("create session");
    let delegate = Arc::new(FrontierDelegate::default());
    let cell_id = start_held_cell(
        session.as_ref(),
        delegate.clone(),
        r#"await tools.hold({}); notify("running"); for (;;) {}"#,
    )
    .await;
    assert!(matches!(
        session
            .wait_to_pending(WaitToPendingRequest {
                cell_id: cell_id.clone(),
                max_output_tokens: None,
            })
            .await
            .expect("pause before running JavaScript"),
        WaitToPendingOutcome::LiveCell(ExecuteToPendingOutcome::Pending { .. })
    ));
    delegate.release.add_permits(1);
    loop {
        let mut pending = session.wait_to_pending(WaitToPendingRequest {
            cell_id: cell_id.clone(),
            max_output_tokens: None,
        });
        tokio::select! {
            _ = delegate.notified.cancelled() => {
                // Notification follows this observation resuming JavaScript.
                // Cancel only this active wait, not its CPU-running cell.
                drop(pending);
                break;
            }
            outcome = &mut pending => assert!(matches!(outcome.expect("frontier before notification"), WaitToPendingOutcome::LiveCell(ExecuteToPendingOutcome::Pending { .. }))),
        }
    }
    assert!(matches!(
        session
            .wait(
                WaitRequest {
                    cell_id: cell_id.clone(),
                    yield_time_ms: 0,
                    max_output_tokens: None,
                },
                None
            )
            .await
            .expect("observe after canceled frontier"),
        WaitOutcome::LiveCell(RuntimeResponse::Yielded { .. })
    ));
    assert!(matches!(
        session
            .terminate(cell_id.into())
            .await
            .expect("terminate after canceled observer"),
        WaitOutcome::LiveCell(RuntimeResponse::Terminated { .. })
    ));
    session.shutdown().await.expect("shutdown session");
}

#[tokio::test]
async fn pending_frontiers_over_stdio() {
    let provider = ProcessOwnedCodeModeSessionProvider::with_host_program(
        codex_utils_cargo_bin::cargo_bin("codex-code-mode-host").expect("host binary"),
    );
    assert_pending_frontiers(&provider).await;
    assert_cancel_pending_observation(&provider).await;
}

#[tokio::test]
async fn pending_frontiers_over_grpc() {
    let host = host::HostHarness::start("grpc://127.0.0.1:0")
        .await
        .expect("start gRPC host");
    let provider = GrpcCodeModeSessionProvider::new(host.endpoint);
    assert_pending_frontiers(&provider).await;
    assert_cancel_pending_observation(&provider).await;
}

use anyhow::Result;
use app_test_support::MockResponsesConfig;
use app_test_support::TestAppServer;
use app_test_support::create_escalated_command_execution_sse_response;
use app_test_support::create_final_assistant_message_sse_response;
use app_test_support::create_mock_responses_server_sequence;
use app_test_support::format_with_current_shell_display;
use codex_app_server::INVALID_PARAMS_ERROR_CODE;
use codex_app_server_protocol::ClientRequest;
use codex_app_server_protocol::CommandExecutionApprovalDecision;
use codex_app_server_protocol::CommandExecutionOutputDeltaNotification;
use codex_app_server_protocol::CommandExecutionRequestApprovalResponse;
use codex_app_server_protocol::CommandExecutionSource;
use codex_app_server_protocol::CommandExecutionStatus;
use codex_app_server_protocol::ItemCompletedNotification;
use codex_app_server_protocol::ItemStartedNotification;
use codex_app_server_protocol::RequestId;
use codex_app_server_protocol::ServerRequest;
use codex_app_server_protocol::SortDirection;
use codex_app_server_protocol::ThreadForkParams;
use codex_app_server_protocol::ThreadForkResponse;
use codex_app_server_protocol::ThreadHistoryMode;
use codex_app_server_protocol::ThreadItemPayload;
use codex_app_server_protocol::ThreadReadParams;
use codex_app_server_protocol::ThreadReadResponse;
use codex_app_server_protocol::ThreadShellCommandParams;
use codex_app_server_protocol::ThreadShellCommandResponse;
use codex_app_server_protocol::ThreadStartParams;
use codex_app_server_protocol::ThreadStartResponse;
use codex_app_server_protocol::ThreadStatus;
use codex_app_server_protocol::ThreadTurnsListParams;
use codex_app_server_protocol::ThreadTurnsListResponse;
use codex_app_server_protocol::TurnCompletedNotification;
use codex_app_server_protocol::TurnStartParams;
use codex_app_server_protocol::TurnStartResponse;
use codex_app_server_protocol::TurnStartedNotification;
use codex_app_server_protocol::TurnStatus;
use codex_app_server_protocol::UserInput as V2UserInput;
use codex_core::shell::default_user_shell;
use codex_exec_server::CODEX_EXEC_SERVER_URL_ENV_VAR;
use pretty_assertions::assert_eq;
use serde_json::json;
use tempfile::TempDir;
use tokio::time::timeout;

const DEFAULT_READ_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

#[cfg(target_os = "linux")]
#[tokio::test]
async fn shell_changes_update_before_completion_and_settle_to_retained_evidence() -> Result<()> {
    use codex_app_server_protocol::CommandExecutionChangesUpdatedNotification;
    use codex_app_server_protocol::LiveCommandChanges;
    use codex_app_server_protocol::PatchChangeKind;
    use codex_app_server_protocol::ThreadChangesLiveParams;
    use codex_app_server_protocol::ThreadChangesLiveResponse;
    use codex_app_server_protocol::ThreadChangesReadParams;
    use codex_app_server_protocol::ThreadChangesReadResponse;
    use std::io::Write;

    async fn read_live(
        app: &mut TestAppServer,
        thread_id: &str,
    ) -> Result<Vec<LiveCommandChanges>> {
        let response: ThreadChangesLiveResponse = app
            .request(|request_id| ClientRequest::ThreadChangesLive {
                request_id,
                params: ThreadChangesLiveParams {
                    thread_id: thread_id.to_owned(),
                },
            })
            .await?;
        Ok(response.data)
    }

    let codex_home = TempDir::new()?;
    let workspace = TempDir::new()?;
    let target = workspace.path().join("observed.txt");
    std::fs::write(&target, "before\n")?;
    let fifo = workspace.path().join("release");
    assert!(
        std::process::Command::new("mkfifo")
            .arg(&fifo)
            .status()?
            .success()
    );
    let mut release = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&fifo)?;

    let server = create_mock_responses_server_sequence(vec![]).await;
    MockResponsesConfig::new(&server.uri()).write(codex_home.path())?;
    let mut app = TestAppServer::builder()
        .with_codex_home(codex_home.path())
        .without_auto_env()
        .build_initialized()
        .await?;
    let ThreadStartResponse { thread, .. } = app
        .request(|request_id| ClientRequest::ThreadStart {
            request_id,
            params: ThreadStartParams {
                cwd: Some(workspace.path().display().to_string()),
                ..Default::default()
            },
        })
        .await?;
    assert_eq!(read_live(&mut app, &thread.id).await?, vec![]);
    let command = "TOKEN='redactionfixture'; echo live > observed.txt; read line < release; \
                   echo before > observed.txt; read line < release; \
                   echo saved > observed.txt; read line < release";
    let _: ThreadShellCommandResponse = app
        .request(|request_id| ClientRequest::ThreadShellCommand {
            request_id,
            params: ThreadShellCommandParams {
                thread_id: thread.id.clone(),
                command: command.into(),
                timeout_ms: Some(30_000),
            },
        })
        .await?;

    let started = timeout(
        DEFAULT_READ_TIMEOUT,
        wait_for_command_execution_started(&mut app, None),
    )
    .await??;
    let item_id = started.item.id();
    assert_eq!(read_live(&mut app, &thread.id).await?, vec![]);

    writeln!(release, "restore")?;
    writeln!(release, "write final change")?;
    writeln!(release, "finish")?;
    let completed = timeout(
        DEFAULT_READ_TIMEOUT,
        wait_for_command_execution_completed(&mut app, Some(item_id)),
    )
    .await??;
    assert!(matches!(
        completed.item.payload,
        ThreadItemPayload::CommandExecution {
            status: CommandExecutionStatus::Completed,
            exit_code: Some(0),
            ..
        }
    ));
    assert_eq!(read_live(&mut app, &thread.id).await?, vec![]);

    let saved: ThreadChangesReadResponse = app
        .request(|request_id| ClientRequest::ThreadChangesRead {
            request_id,
            params: ThreadChangesReadParams {
                thread_id: thread.id,
                include_external: None,
                include_ephemeral: Some(true),
                caller_thread_id: None,
                item_id: Some(item_id.to_owned()),
                receipt_item_ids: None,
                per_capture: false,
                limit: None,
                cursor: None,
            },
        })
        .await?;
    assert_eq!(saved.file_count, 1);
    assert_eq!(saved.changes[0].turn_id, started.turn_id);
    assert_eq!(saved.changes[0].source_kind, "UserShell");
    assert_eq!(saved.changes[0].source_label, "User shell");
    assert_eq!(saved.changes[0].author_thread_id, None);
    assert!(saved.files[0].unified_diff.contains("-before\n+saved\n"));
    Ok(())
}

#[tokio::test]
async fn saved_shell_caller_stats_count_net_workspace_edits() -> Result<()> {
    use codex_app_server_protocol::ThreadChangeCallerCounts;
    use codex_app_server_protocol::ThreadChangesReadParams;
    use codex_app_server_protocol::ThreadChangesReadResponse;
    use codex_change_history::ChangeHistoryStore;
    use codex_change_history::ChangeMetadata;
    use codex_change_history::ChangeScope;
    use codex_change_history::ChangeSourceKind;
    use codex_change_history::CoverageStatus;
    use codex_change_history::FileSnapshot;
    use codex_change_history::PendingFileChange;
    use codex_protocol::ThreadId;

    core_test_support::skip_if_remote!(Ok(()), "seeds the app-server host change store");
    let codex_home = TempDir::new()?;
    let workspace = TempDir::new()?;
    let project = workspace.path().join("project");
    std::fs::create_dir(&project)?;
    let server = create_mock_responses_server_sequence(vec![]).await;
    MockResponsesConfig::new(&server.uri()).write(codex_home.path())?;
    let mut app = TestAppServer::builder()
        .with_codex_home(codex_home.path())
        .build_initialized()
        .await?;
    let ThreadStartResponse { thread, .. } = app
        .request(|request_id| ClientRequest::ThreadStart {
            request_id,
            params: ThreadStartParams {
                cwd: Some(project.display().to_string()),
                ..Default::default()
            },
        })
        .await?;
    let store_dir = codex_protocol::shell_environment::session_tmp_path(
        ThreadId::from_string(&thread.id)?.into(),
    )
    .join("change-history");
    let store = ChangeHistoryStore::open(store_dir)?;
    let record = |call_id: &str, root: &std::path::Path, files| {
        store.record_change(
            ChangeMetadata {
                thread_id: thread.id.clone(),
                session_id: thread.id.clone(),
                turn_id: "turn".into(),
                call_id: call_id.into(),
                author: None,
                source_kind: ChangeSourceKind::ShellWrite,
                workspace_root: root.to_path_buf(),
                created_at: 1,
                exit_code: Some(0),
                summary: None,
                claim_key: None,
            },
            files,
        )
    };
    let edit = |name: &str, before: &[u8], after: &[u8]| PendingFileChange {
        rel_path: name.into(),
        abs_path: project.join(name),
        moved_from: None,
        before: FileSnapshot::Present(before.into()),
        after: FileSnapshot::Present(after.into()),
        coverage: CoverageStatus::Complete,
        scope: ChangeScope::Workspace,
    };
    let mut outside = edit("outside.txt", b"old\n", b"draft\n");
    outside.abs_path = workspace.path().join("outside.txt");
    record(
        "first",
        workspace.path(),
        vec![
            edit("text.txt", b"base\n", b"draft\n"),
            edit("unchanged.txt", b"base\n", b"draft\n"),
            outside.clone(),
        ],
    )?;
    outside.before = outside.after;
    outside.after = FileSnapshot::Present(b"result\n".to_vec());
    outside.scope = ChangeScope::External;
    let mut partial = edit("partial.txt", b"base\n", b"draft\n");
    partial.coverage = CoverageStatus::Partial {
        reason: "fixture baseline".into(),
    };
    record(
        "second",
        &project,
        vec![
            edit("text.txt", b"draft\n", b"base\nadded\n"),
            edit("unchanged.txt", b"draft\n", b"base\n"),
            edit("binary", b"\0before", b"\0after"),
            outside,
            partial,
        ],
    )?;

    let saved: ThreadChangesReadResponse = app
        .request(|request_id| ClientRequest::ThreadChangesRead {
            request_id,
            params: ThreadChangesReadParams {
                thread_id: thread.id.clone(),
                include_external: None,
                include_ephemeral: None,
                caller_thread_id: None,
                item_id: None,
                receipt_item_ids: None,
                per_capture: false,
                limit: Some(1),
                cursor: None,
            },
        })
        .await?;
    assert_eq!(saved.files.len(), 1);
    assert_eq!(
        saved.callers,
        vec![ThreadChangeCallerCounts {
            thread_id: thread.id,
            file_count: 4,
            additions: 1,
            deletions: 0,
        }]
    );
    Ok(())
}

#[tokio::test]
async fn thread_shell_command_history_responses_exclude_persisted_command_executions() -> Result<()>
{
    let tmp = TempDir::new()?;
    let codex_home = tmp.path().join("codex_home");
    std::fs::create_dir(&codex_home)?;
    let workspace = tmp.path().join("workspace");
    std::fs::create_dir(&workspace)?;

    let server = create_mock_responses_server_sequence(vec![]).await;
    MockResponsesConfig::new(&server.uri()).write(&codex_home)?;

    let mut mcp = TestAppServer::builder()
        .with_codex_home(codex_home.as_path())
        // thread/shellCommand intentionally executes on the app-server host.
        .without_auto_env()
        .build_initialized()
        .await?;
    let ThreadStartResponse { thread, .. } = mcp
        .request(|request_id| ClientRequest::ThreadStart {
            request_id,
            params: ThreadStartParams {
                history_mode: Some(ThreadHistoryMode::Legacy),
                ..Default::default()
            },
        })
        .await?;
    let (shell_command, expected_output) = current_shell_output_command("hello from bang")?;

    let _: ThreadShellCommandResponse = mcp
        .request(|request_id| ClientRequest::ThreadShellCommand {
            request_id,
            params: ThreadShellCommandParams {
                thread_id: thread.id.clone(),
                command: shell_command,
                timeout_ms: None,
            },
        })
        .await?;

    let started = wait_for_command_execution_started(&mut mcp, /*expected_id*/ None).await?;
    let ThreadItemPayload::CommandExecution {
        id, source, status, ..
    } = &started.item.payload
    else {
        unreachable!("helper returns command execution item");
    };
    let command_id = id.clone();
    assert_eq!(source, &CommandExecutionSource::UserShell);
    assert_eq!(status, &CommandExecutionStatus::InProgress);

    let delta = wait_for_command_execution_output_delta(&mut mcp, &command_id).await?;
    assert_eq!(
        delta.delta.trim_end_matches(['\r', '\n']),
        expected_output.trim_end_matches(['\r', '\n'])
    );

    let completed = wait_for_command_execution_completed(&mut mcp, Some(&command_id)).await?;
    let ThreadItemPayload::CommandExecution {
        id,
        source,
        status,
        aggregated_output,
        exit_code,
        ..
    } = &completed.item.payload
    else {
        unreachable!("helper returns command execution item");
    };
    assert_eq!(id, &command_id);
    assert_eq!(source, &CommandExecutionSource::UserShell);
    assert_eq!(status, &CommandExecutionStatus::Completed);
    assert_eq!(aggregated_output.as_deref(), Some(expected_output.as_str()));
    assert_eq!(*exit_code, Some(0));

    timeout(
        DEFAULT_READ_TIMEOUT,
        mcp.read_stream_until_notification_message("turn/completed"),
    )
    .await??;

    let ThreadReadResponse { thread, .. } = mcp
        .request(|request_id| ClientRequest::ThreadRead {
            request_id,
            params: ThreadReadParams {
                thread_id: thread.id.clone(),
                include_turns: true,
            },
        })
        .await?;
    assert_eq!(thread.turns.len(), 1);
    assert_no_command_executions(&thread.turns[0].items, "thread/read");

    let ThreadTurnsListResponse { data, .. } = mcp
        .request(|request_id| ClientRequest::ThreadTurnsList {
            request_id,
            params: ThreadTurnsListParams {
                thread_id: thread.id.clone(),
                cursor: None,
                limit: None,
                sort_direction: Some(SortDirection::Asc),
                items_view: None,
            },
        })
        .await?;
    assert_eq!(data.len(), 1);
    assert_no_command_executions(&data[0].items, "thread/turns/list");

    let ThreadForkResponse { thread, .. } = mcp
        .request(|request_id| ClientRequest::ThreadFork {
            request_id,
            params: ThreadForkParams {
                thread_id: thread.id,
                ..Default::default()
            },
        })
        .await?;
    assert_eq!(thread.turns.len(), 1);
    assert_no_command_executions(&thread.turns[0].items, "thread/fork");

    Ok(())
}

#[tokio::test]
async fn thread_shell_command_returns_error_when_local_environment_is_disabled() -> Result<()> {
    let tmp = TempDir::new()?;
    let codex_home = tmp.path().join("codex_home");
    std::fs::create_dir(&codex_home)?;
    let server = create_mock_responses_server_sequence(vec![]).await;
    MockResponsesConfig::new(&server.uri()).write(&codex_home)?;

    let mut mcp = TestAppServer::builder()
        .with_codex_home(codex_home.as_path())
        // This test intentionally exercises thread/shellCommand without a local host environment.
        .without_auto_env()
        .with_env_overrides(&[(CODEX_EXEC_SERVER_URL_ENV_VAR, Some("none"))])
        .build_initialized()
        .await?;
    let ThreadStartResponse { thread, .. } = mcp
        .request(|request_id| ClientRequest::ThreadStart {
            request_id,
            params: ThreadStartParams::default(),
        })
        .await?;
    let shell_id = mcp
        .send_thread_shell_command_request(ThreadShellCommandParams {
            thread_id: thread.id,
            command: "pwd".to_string(),
            timeout_ms: None,
        })
        .await?;
    let error = mcp
        .read_stream_until_error_message(RequestId::Integer(shell_id))
        .await?;
    assert_eq!(error.error.message, "local environment is not configured");

    Ok(())
}

#[tokio::test]
async fn thread_shell_command_uses_existing_active_turn() -> Result<()> {
    check_thread_shell_command_in_active_turn(/*timeout_ms*/ None).await
}

#[tokio::test]
async fn thread_shell_command_timeout_preserves_active_turn() -> Result<()> {
    check_thread_shell_command_in_active_turn(Some(3_000)).await
}

async fn check_thread_shell_command_in_active_turn(timeout_ms: Option<i64>) -> Result<()> {
    let tmp = TempDir::new()?;
    let codex_home = tmp.path().join("codex_home");
    std::fs::create_dir(&codex_home)?;
    let workspace = tmp.path().join("workspace");
    std::fs::create_dir(&workspace)?;

    let responses = vec![
        create_escalated_command_execution_sse_response(
            vec![
                "python3".to_string(),
                "-c".to_string(),
                "print(42)".to_string(),
            ],
            /*workdir*/ None,
            Some(5000),
            "call-approve",
        )?,
        create_final_assistant_message_sse_response("done")?,
    ];
    let server = create_mock_responses_server_sequence(responses).await;
    MockResponsesConfig::new(&server.uri())
        .with_approval_policy("on-request")
        .write(&codex_home)?;

    let mut mcp = TestAppServer::builder()
        .with_codex_home(codex_home.as_path())
        // thread/shellCommand intentionally joins the app-server's host-local active turn.
        .without_auto_env()
        .build_initialized()
        .await?;
    let ThreadStartResponse { thread, .. } = mcp
        .request(|request_id| ClientRequest::ThreadStart {
            request_id,
            params: ThreadStartParams {
                history_mode: Some(ThreadHistoryMode::Legacy),
                ..Default::default()
            },
        })
        .await?;
    let (shell_command, expected_output) = if timeout_ms.is_some() {
        current_shell_slow_command()?
    } else {
        current_shell_output_command("active turn bang")?
    };

    let TurnStartResponse { turn } = mcp
        .request(|request_id| ClientRequest::TurnStart {
            request_id,
            params: TurnStartParams {
                thread_id: thread.id.clone(),
                client_user_message_id: None,
                input: vec![V2UserInput::Text {
                    text: "run python".to_string(),
                    text_elements: Vec::new(),
                }],
                cwd: Some(workspace.clone()),
                ..Default::default()
            },
        })
        .await?;

    let started: TurnStartedNotification = mcp.read_notification("turn/started").await?;
    assert_eq!(started.turn.id, turn.id);
    let agent_started = wait_for_command_execution_started(&mut mcp, Some("call-approve")).await?;
    let ThreadItemPayload::CommandExecution {
        command, source, ..
    } = &agent_started.item.payload
    else {
        unreachable!("helper returns command execution item");
    };
    assert_eq!(source, &CommandExecutionSource::Agent);
    assert_eq!(
        command,
        &format_with_current_shell_display("python3 -c 'print(42)'")
    );

    let server_req = timeout(
        DEFAULT_READ_TIMEOUT,
        mcp.read_stream_until_request_message(),
    )
    .await??;
    let ServerRequest::CommandExecutionRequestApproval { request_id, .. } = server_req else {
        panic!("expected approval request");
    };

    let _: ThreadShellCommandResponse = mcp
        .request(|request_id| ClientRequest::ThreadShellCommand {
            request_id,
            params: ThreadShellCommandParams {
                thread_id: thread.id.clone(),
                command: shell_command,
                timeout_ms,
            },
        })
        .await?;

    let started =
        wait_for_command_execution_started_by_source(&mut mcp, CommandExecutionSource::UserShell)
            .await?;
    assert_eq!(started.turn_id, turn.id);
    let command_id = match &started.item.payload {
        ThreadItemPayload::CommandExecution { id, .. } => id.clone(),
        _ => unreachable!("helper returns command execution item"),
    };
    let completed = wait_for_command_execution_completed(&mut mcp, Some(&command_id)).await?;
    assert_eq!(completed.turn_id, turn.id);
    let ThreadItemPayload::CommandExecution {
        source,
        aggregated_output,
        status,
        exit_code,
        ..
    } = &completed.item.payload
    else {
        unreachable!("helper returns command execution item");
    };
    assert_eq!(source, &CommandExecutionSource::UserShell);
    if timeout_ms.is_some() {
        assert_eq!(status, &CommandExecutionStatus::Failed);
        assert_eq!(*exit_code, Some(-1));
        let output = aggregated_output.as_deref().expect("timeout output");
        assert!(output.contains("Timeout"));
        assert!(output.contains(expected_output.trim()));
    } else {
        assert_eq!(status, &CommandExecutionStatus::Completed);
        assert_eq!(*exit_code, Some(0));
        assert_eq!(aggregated_output.as_deref(), Some(expected_output.as_str()));
    }

    let ThreadReadResponse {
        thread: active_thread,
        ..
    } = mcp
        .request(|request_id| ClientRequest::ThreadRead {
            request_id,
            params: ThreadReadParams {
                thread_id: thread.id.clone(),
                include_turns: true,
            },
        })
        .await?;
    assert_eq!(active_thread.turns.len(), 1);
    assert_eq!(active_thread.turns[0].id, turn.id);
    assert_eq!(active_thread.turns[0].status, TurnStatus::InProgress);
    assert!(
        mcp.pending_notification_methods()
            .iter()
            .all(|method| { method != "turn/started" && method != "turn/completed" })
    );

    mcp.send_response(
        request_id,
        serde_json::to_value(CommandExecutionRequestApprovalResponse {
            decision: CommandExecutionApprovalDecision::Decline,
        })?,
    )
    .await?;
    let completed: TurnCompletedNotification = timeout(
        DEFAULT_READ_TIMEOUT,
        mcp.read_notification("turn/completed"),
    )
    .await??;
    assert_eq!(completed.turn.id, turn.id);
    assert_eq!(completed.turn.status, TurnStatus::Completed);
    let ThreadReadResponse { thread, .. } = mcp
        .request(|request_id| ClientRequest::ThreadRead {
            request_id,
            params: ThreadReadParams {
                thread_id: thread.id,
                include_turns: true,
            },
        })
        .await?;
    assert_eq!(thread.turns.len(), 1);
    assert_no_command_executions(&thread.turns[0].items, "thread/read");
    assert!(
        mcp.pending_notification_methods()
            .iter()
            .all(|method| { method != "turn/started" && method != "turn/completed" })
    );

    Ok(())
}

#[tokio::test]
async fn thread_shell_command_honors_optional_timeout() -> Result<()> {
    let codex_home = TempDir::new()?;
    let server = create_mock_responses_server_sequence(vec![]).await;
    MockResponsesConfig::new(&server.uri()).write(codex_home.path())?;
    let mut mcp = TestAppServer::builder()
        .with_codex_home(codex_home.path())
        // thread/shellCommand always executes on the app-server host.
        .without_auto_env()
        .build_initialized()
        .await?;
    let ThreadStartResponse { thread, .. } = mcp
        .request(|request_id| ClientRequest::ThreadStart {
            request_id,
            params: ThreadStartParams::default(),
        })
        .await?;

    for (timeout_ms, expires) in [
        (None, false),
        (Some(json!(null)), false),
        (Some(json!(28_800_000)), false),
        (Some(json!(3_000)), true),
        (Some(json!(0)), true),
    ] {
        let expect_output = timeout_ms != Some(json!(0));
        let (command, expected_output) = if expires {
            current_shell_slow_command()?
        } else {
            current_shell_output_command("shell-timeout-ready")?
        };
        let mut params = json!({ "threadId": thread.id, "command": command });
        if let Some(timeout_ms) = timeout_ms {
            params["timeoutMs"] = timeout_ms;
        }
        let request_id = mcp
            .send_request("thread/shellCommand", Some(params))
            .await?;
        let _: ThreadShellCommandResponse =
            timeout(DEFAULT_READ_TIMEOUT, mcp.read_response(request_id)).await??;
        let started: TurnStartedNotification = mcp.read_notification("turn/started").await?;
        let completed =
            wait_for_command_execution_completed(&mut mcp, /*expected_id*/ None).await?;
        assert_eq!(completed.turn_id, started.turn.id);
        let ThreadItemPayload::CommandExecution {
            status,
            exit_code,
            aggregated_output,
            ..
        } = completed.item.payload
        else {
            unreachable!("helper returns command execution item");
        };
        let output = aggregated_output.expect("shell output");
        if expires {
            assert_eq!(status, CommandExecutionStatus::Failed);
            assert_eq!(exit_code, Some(-1));
            assert!(output.contains("Timeout"));
            if expect_output {
                assert!(output.contains(expected_output.trim()));
            }
        } else {
            assert_eq!(status, CommandExecutionStatus::Completed);
            assert_eq!(exit_code, Some(0));
            assert_eq!(output, expected_output);
        }
        let completed: TurnCompletedNotification = timeout(
            DEFAULT_READ_TIMEOUT,
            mcp.read_notification("turn/completed"),
        )
        .await??;
        assert_eq!(completed.turn.id, started.turn.id);
        assert_eq!(completed.turn.status, TurnStatus::Completed);
    }
    Ok(())
}

#[tokio::test]
async fn thread_shell_command_rejects_invalid_timeouts_before_execution() -> Result<()> {
    let codex_home = TempDir::new()?;
    let server = create_mock_responses_server_sequence(vec![]).await;
    MockResponsesConfig::new(&server.uri()).write(codex_home.path())?;
    let mut mcp = TestAppServer::builder()
        .with_codex_home(codex_home.path())
        .without_auto_env()
        .build_initialized()
        .await?;
    let ThreadStartResponse { thread, .. } = mcp
        .request(|request_id| ClientRequest::ThreadStart {
            request_id,
            params: ThreadStartParams::default(),
        })
        .await?;
    for timeout_ms in [json!(-1), json!(1.5), json!("1000"), json!(u64::MAX)] {
        let request_id = mcp
            .send_request(
                "thread/shellCommand",
                Some(json!({
                    "threadId": thread.id,
                    "command": "echo unexpected",
                    "timeoutMs": timeout_ms,
                })),
            )
            .await?;
        let error = timeout(
            DEFAULT_READ_TIMEOUT,
            mcp.read_stream_until_error_message(RequestId::Integer(request_id)),
        )
        .await??;
        if timeout_ms == json!(-1) {
            assert_eq!(error.error.code, INVALID_PARAMS_ERROR_CODE);
            assert_eq!(
                error.error.message,
                "thread/shellCommand timeoutMs must be non-negative, got -1"
            );
        }
    }
    let ThreadReadResponse { thread, .. } = mcp
        .request(|request_id| ClientRequest::ThreadRead {
            request_id,
            params: ThreadReadParams {
                thread_id: thread.id,
                include_turns: false,
            },
        })
        .await?;
    assert_eq!(thread.status, ThreadStatus::Idle);
    assert!(
        mcp.pending_notification_methods()
            .iter()
            .all(|method| { method != "turn/started" && method != "item/started" })
    );
    Ok(())
}

fn current_shell_slow_command() -> Result<(String, String)> {
    let (command, expected_output) = current_shell_output_command("shell-timeout-ready")?;
    let suffix = match default_user_shell().name() {
        "powershell" => "; Start-Sleep -Seconds 60",
        "cmd" => " & ping -n 61 127.0.0.1 > nul",
        _ => "; exec sleep 60",
    };
    Ok((format!("{command}{suffix}"), expected_output))
}

fn assert_no_command_executions(items: &[codex_app_server_protocol::ThreadItem], context: &str) {
    assert!(
        items
            .iter()
            .all(|entry| !matches!(&entry.payload, ThreadItemPayload::CommandExecution { .. })),
        "{context} should always exclude command executions from returned turns"
    );
}

fn current_shell_output_command(text: &str) -> Result<(String, String)> {
    let command_and_output = match default_user_shell().name() {
        "powershell" => {
            let escaped_text = text.replace('\'', "''");
            (
                format!("Write-Output '{escaped_text}'"),
                format!("{text}\r\n"),
            )
        }
        "cmd" => (format!("echo {text}"), format!("{text}\r\n")),
        _ => {
            let quoted_text = shlex::try_quote(text)?;
            (format!("printf '%s\\n' {quoted_text}"), format!("{text}\n"))
        }
    };
    Ok(command_and_output)
}

async fn wait_for_command_execution_started(
    mcp: &mut TestAppServer,
    expected_id: Option<&str>,
) -> Result<ItemStartedNotification> {
    loop {
        let started: ItemStartedNotification = mcp.read_notification("item/started").await?;
        let ThreadItemPayload::CommandExecution { id, .. } = &started.item.payload else {
            continue;
        };
        if expected_id.is_none() || expected_id == Some(id.as_str()) {
            return Ok(started);
        }
    }
}

async fn wait_for_command_execution_started_by_source(
    mcp: &mut TestAppServer,
    expected_source: CommandExecutionSource,
) -> Result<ItemStartedNotification> {
    loop {
        let started = wait_for_command_execution_started(mcp, /*expected_id*/ None).await?;
        let ThreadItemPayload::CommandExecution { source, .. } = &started.item.payload else {
            continue;
        };
        if source == &expected_source {
            return Ok(started);
        }
    }
}

async fn wait_for_command_execution_completed(
    mcp: &mut TestAppServer,
    expected_id: Option<&str>,
) -> Result<ItemCompletedNotification> {
    loop {
        let completed: ItemCompletedNotification = mcp.read_notification("item/completed").await?;
        let ThreadItemPayload::CommandExecution { id, .. } = &completed.item.payload else {
            continue;
        };
        if expected_id.is_none() || expected_id == Some(id.as_str()) {
            return Ok(completed);
        }
    }
}

async fn wait_for_command_execution_output_delta(
    mcp: &mut TestAppServer,
    item_id: &str,
) -> Result<CommandExecutionOutputDeltaNotification> {
    loop {
        let delta: CommandExecutionOutputDeltaNotification = mcp
            .read_notification("item/commandExecution/outputDelta")
            .await?;
        if delta.item_id == item_id {
            return Ok(delta);
        }
    }
}

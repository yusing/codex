use super::*;
use crate::shell::Shell;
use crate::shell::ShellType;
use codex_state::coordination::WorkDisposition;
use codex_state::coordination_wait::WaitCancellation;
use codex_state::coordination_wait::WaitPhase;
use codex_state::coordination_wait::WaitTicket;
use core_test_support::PathExt;
use pretty_assertions::assert_eq;
use std::path::PathBuf;
use std::process::Command;

fn shell_with_snapshot(
    shell_type: ShellType,
    shell_path: &str,
    snapshot_path: AbsolutePathBuf,
) -> (Shell, AbsolutePathBuf) {
    (
        Shell {
            shell_type,
            shell_path: PathBuf::from(shell_path),
        },
        snapshot_path,
    )
}

#[test]
fn user_shell_snapshot_preserves_package_path_prepend() {
    let dir = tempfile::tempdir().expect("create temp dir");
    let snapshot_path = dir.path().join("snapshot.sh");
    std::fs::write(
        &snapshot_path,
        "# Snapshot file\nexport PATH='/snapshot/bin'\n",
    )
    .expect("write snapshot");
    let (session_shell, shell_snapshot) =
        shell_with_snapshot(ShellType::Bash, "/bin/bash", snapshot_path.abs());
    let command = vec![
        "/bin/bash".to_string(),
        "-lc".to_string(),
        "printf '%s' \"$PATH\"".to_string(),
    ];
    let package_path_dir = dir.path().join("codex-path");
    let mut env = HashMap::from([("PATH".to_string(), "/worktree/bin".to_string())]);
    let rewritten = prepare_user_shell_exec_command_with_path_prepend(
        &command,
        &session_shell,
        Some(&shell_snapshot),
        &HashMap::new(),
        &mut env,
        |env, runtime_path_prepends| {
            runtime_path_prepends.prepend(env, package_path_dir.as_path());
        },
    );
    let output = Command::new(&rewritten[0])
        .args(&rewritten[1..])
        .env("PATH", env.get("PATH").expect("PATH should be set"))
        .output()
        .expect("run rewritten command");

    assert!(output.status.success(), "command failed: {output:?}");
    assert_eq!(
        String::from_utf8_lossy(&output.stdout),
        format!("{}:/snapshot/bin", package_path_dir.display())
    );
}

async fn waiting_shell_thread(
    server: &wiremock::MockServer,
) -> anyhow::Result<(
    tempfile::TempDir,
    crate::ThreadManager,
    crate::thread_manager::NewThread,
    WaitTicket,
)> {
    use codex_features::Feature;
    use codex_state::coordination_wait::TurnWaitReason;

    let directory = tempfile::tempdir()?;
    let mut config = crate::config::test_config().await;
    config.codex_home = directory.path().abs();
    config.cwd = directory.path().abs();
    config.model_provider.base_url = Some(format!("{}/v1", server.uri()));
    config.features.enable(Feature::Collab)?;
    config.features.enable(Feature::MultiAgentV2)?;
    config.features.enable(Feature::Sqlite)?;
    let state = codex_state::StateRuntime::init(
        codex_state::SqliteConfig::new_for_testing(directory.path().abs()),
        "test-provider".to_owned(),
    )
    .await?;
    let manager = crate::ThreadManager::with_models_provider_and_home_for_tests(
        codex_login::CodexAuth::from_api_key("dummy"),
        config.model_provider.clone(),
        config.codex_home.to_path_buf(),
        Arc::new(codex_exec_server::EnvironmentManager::default_for_tests()),
    )
    .with_work_state(state);
    let thread = manager
        .start_thread(crate::StartThreadOptions::new(config))
        .await?;
    let session = &thread.thread.session;
    let mut input = TurnInput::UserInput {
        content: vec![codex_protocol::user_input::UserInput::Text {
            text: "existing assignment".to_owned(),
            text_elements: Vec::new(),
        }],
        client_id: None,
        metadata: Default::default(),
    };
    session
        .admit_coordination_input(&mut input, "initial", true)
        .await?;
    session
        .confirm_coordination_input(&input, "initial")
        .await?;
    let ledger = session.services.local_agent_runtime.work_ledger()?;
    let stamp = ledger.work_stamp(thread.thread_id).await?.unwrap();
    let wait = ledger
        .commit_turn_wait(
            stamp,
            "initial",
            TurnWaitReason::ProviderCapacity,
            None,
            chrono::Utc::now().timestamp_millis(),
        )
        .await?
        .unwrap();
    Ok((directory, manager, thread, wait))
}

#[tokio::test]
async fn shell_wait_output_and_ready_message_share_one_native_inference() -> anyhow::Result<()> {
    use crate::agent::control::work::DeliveryKind;
    use crate::agent::control::work::PublishDelivery;
    use crate::agent::control::work_lifecycle::CoordinationPayload;
    use codex_protocol::AgentPath;
    use codex_protocol::protocol::InterAgentCommunication;
    use core_test_support::responses;

    for addressed_message in [false, true] {
        let server = responses::start_mock_server().await;
        let mock = responses::mount_sse_once(
            &server,
            responses::sse(vec![
                responses::ev_response_created("shell-continuation"),
                responses::ev_assistant_message("shell-answer", "continued once"),
                responses::ev_completed("shell-continuation"),
            ]),
        )
        .await;
        let (directory, manager, thread, wait) = waiting_shell_thread(&server).await?;
        let fifo = directory.path().join("shell-gate");
        assert!(Command::new("mkfifo").arg(&fifo).status()?.success());
        thread
            .thread
            .start_user_shell_command(
                "read gate < shell-gate; printf shell-output; exit 7".to_owned(),
                None,
                None,
            )
            .await?;
        let session = &thread.thread.session;
        let ledger = session.services.local_agent_runtime.work_ledger()?;
        let mut starts = 0;
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                match thread.thread.next_event().await?.msg {
                    EventMsg::TurnStarted(_) => starts += 1,
                    EventMsg::ExecCommandBegin(_) => break,
                    EventMsg::TurnComplete(_) => panic!("shell completed waiting logical work"),
                    _ => {}
                }
            }
            anyhow::Ok(())
        })
        .await??;
        assert_eq!(
            ledger.current_wait(thread.thread_id).await?.unwrap().id,
            wait.id
        );
        assert_eq!(
            ledger
                .observe_current(thread.thread_id)
                .await?
                .unwrap()
                .disposition,
            WorkDisposition::Waiting
        );
        assert!(mock.requests().is_empty());
        if addressed_message {
            ledger
                .publish_delivery(&PublishDelivery {
                    id: Uuid::now_v7(),
                    root_id: thread.thread_id,
                    sender_id: Some(thread.thread_id),
                    recipient_id: thread.thread_id,
                    work: None,
                    kind: DeliveryKind::Message,
                    payload: serde_json::to_string(&CoordinationPayload::Message {
                        communication: InterAgentCommunication::new(
                            AgentPath::root(),
                            AgentPath::root(),
                            Vec::new(),
                            "ready note".to_owned(),
                            false,
                        ),
                    })?,
                })
                .await?;
        }
        session.maybe_start_turn_for_pending_work().await;
        assert!(
            mock.requests().is_empty(),
            "ready wait cannot overtake shell output"
        );
        tokio::fs::write(&fifo, b"release\n").await?;
        let mut output_seen = false;
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                match thread.thread.next_event().await?.msg {
                    EventMsg::TurnStarted(_) => starts += 1,
                    EventMsg::ExecCommandEnd(event) => {
                        assert_eq!(event.exit_code, 7);
                        assert!(event.aggregated_output.contains("shell-output"));
                        output_seen = true;
                    }
                    EventMsg::TurnComplete(_) => break,
                    EventMsg::TurnAborted(_) => panic!("shell continuation was interrupted"),
                    _ => {}
                }
            }
            anyhow::Ok(())
        })
        .await??;
        assert!(output_seen);
        assert_eq!(
            starts, 1,
            "shell and inference share the admitted host turn"
        );
        let request = mock.single_request();
        assert!(request.body_contains_text("shell-output"));
        assert!(request.body_contains_text("Exit code: 7"));
        assert_eq!(request.body_contains_text("ready note"), addressed_message);
        assert_eq!(
            request
                .message_input_texts("user")
                .iter()
                .filter(|text| text.contains("<user_shell_command>"))
                .count(),
            1
        );
        let settled = ledger.observe_current(thread.thread_id).await?.unwrap();
        assert_eq!(settled.stamp, wait.caller);
        assert_eq!(settled.disposition, WorkDisposition::Completed);
        let retained = ledger.observe_wait(wait.caller, wait.id).await?.unwrap();
        if addressed_message {
            assert_eq!(retained.phase, WaitPhase::Consumed);
        } else {
            assert!(matches!(
                retained.cancellation,
                Some(WaitCancellation::ShellOutput { .. })
            ));
        }
        manager
            .request_agent_tree_shutdown(thread.thread_id)
            .await?
            .wait()
            .await?;
    }
    Ok(())
}

#[tokio::test]
async fn pending_model_admission_rejects_shell_and_interrupt_during_shell_never_resumes()
-> anyhow::Result<()> {
    let server = core_test_support::responses::start_mock_server().await;
    let (_directory, manager, thread, wait) = waiting_shell_thread(&server).await?;
    let session = &thread.thread.session;
    let reservation = Arc::clone(
        &session
            .active_turn
            .lock()
            .await
            .get_or_insert_with(ActiveTurn::default)
            .turn_state,
    );
    assert!(
        thread
            .thread
            .start_user_shell_command("printf must-not-run".to_owned(), None, None)
            .await
            .is_err()
    );
    let ledger = session.services.local_agent_runtime.work_ledger()?;
    assert_eq!(
        ledger.current_wait(thread.thread_id).await?.unwrap().id,
        wait.id
    );
    session.clear_reserved_idle_turn(&reservation).await;
    thread
        .thread
        .start_user_shell_command("sleep 60".to_owned(), None, None)
        .await?;
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if matches!(
                thread.thread.next_event().await?.msg,
                EventMsg::ExecCommandBegin(_)
            ) {
                break;
            }
        }
        anyhow::Ok(())
    })
    .await??;
    assert_eq!(
        session.interrupt_task_with_receipt().await?,
        Some(wait.caller)
    );
    assert!(ledger.current_wait(thread.thread_id).await?.is_none());
    assert_eq!(
        ledger
            .observe_current(thread.thread_id)
            .await?
            .unwrap()
            .disposition,
        WorkDisposition::Interrupted
    );
    assert!(
        !server
            .received_requests()
            .await
            .unwrap()
            .iter()
            .any(|request| request.url.path().ends_with("/responses")),
        "interrupted shell must not resume inference"
    );
    manager
        .request_agent_tree_shutdown(thread.thread_id)
        .await?
        .wait()
        .await?;
    Ok(())
}

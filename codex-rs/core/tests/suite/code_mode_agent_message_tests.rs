//! Exec-spawned tasks reach the child model as attributed plaintext.

use anyhow::Result;
use codex_core::TurnInputRequest;
use codex_features::Feature;
use codex_protocol::context_reset::ContextResetStatus;
use codex_protocol::items::CollabAgentTool;
use codex_protocol::items::TurnItem;
use codex_protocol::openai_models::ToolMode;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::Op;
use codex_protocol::protocol::ReloadSelection;
use codex_protocol::user_input::UserInput;
use core_test_support::responses::ev_assistant_message;
use core_test_support::responses::ev_completed;
use core_test_support::responses::ev_completed_with_tokens;
use core_test_support::responses::ev_custom_tool_call;
use core_test_support::responses::ev_response_created;
use core_test_support::responses::mount_sse_once_match;
use core_test_support::responses::sse;
use core_test_support::responses::start_mock_server;
use core_test_support::responses::strip_metadata_from_json;
use core_test_support::responses::strip_response_item_ids_from_json;
use core_test_support::skip_if_no_network;
use core_test_support::streaming_sse::StreamingSseChunk;
use core_test_support::streaming_sse::start_streaming_sse_server;
use core_test_support::test_codex::test_codex;
use core_test_support::wait_for_event;
use pretty_assertions::assert_eq;
use serde_json::Value;
use serde_json::json;
use wiremock::matchers::header;

#[test_case::test_case(false, false, 4096, "worker-1", false, None; "ordinary_wait")]
#[test_case::test_case(true, false, 4096, "worker-1", false, None; "manual_reset")]
#[test_case::test_case(false, true, 4096, "worker-1", false, None; "compact_and_wait")]
#[test_case::test_case(false, false, 5, "worker-1", false, None; "whole_call_output_budget")]
#[test_case::test_case(false, false, 4096, "root", false, None; "root_child_identity")]
#[test_case::test_case(false, true, 4096, "worker-1", true, None; "interrupt_after_compact")]
#[test_case::test_case(true, false, 4096, "worker-1", true, None; "interrupt_after_manual_reset")]
#[test_case::test_case(false, false, 4096, "worker-1", false, Some((0, false)); "shell_success")]
#[test_case::test_case(false, false, 4096, "worker-1", false, Some((7, false)); "shell_nonzero")]
#[test_case::test_case(false, false, 4096, "worker-1", false, Some((7, true)); "child_ready_during_shell")]
#[test_case::test_case(false, false, 4096, "worker-1", true, Some((7, true)); "interrupt_shell_resume_barrier")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn code_mode_wait_preserves_original_call_and_result(
    manual_reset: bool,
    compact: bool,
    max_output_tokens: usize,
    task_name: &'static str,
    interrupt: bool,
    shell: Option<(i32, bool)>,
) -> Result<()> {
    skip_if_no_network!(Ok(()));
    let reload = tempfile::NamedTempFile::new()?;
    std::fs::write(reload.path(), "native_wait_reload_marker")?;
    let selection = ReloadSelection {
        sources: vec![format!("@{}", reload.path().display())],
    };
    let options = if compact {
        json!({"wait_mode":"all", "compact":true, "reload":selection})
    } else {
        json!({"wait_mode":"all"})
    };
    let seed_history = manual_reset || compact;
    let bounded_output = max_output_tokens == 5;
    let before = if bounded_output {
        "01234567890123456789"
    } else {
        "before_wait_marker"
    };
    let after = if bounded_output {
        "01234567890123456789"
    } else {
        "after_wait_marker"
    };
    let emit_wait = if bounded_output { "" } else { "text(result);" };
    let expect_child = shell.is_none_or(|(_, child_ready)| child_ready);
    let wait_source = if expect_child {
        format!(
            r#"const result = await tools.collaboration__wait_agent({options});
if (!JSON.stringify(result).includes("child_result_marker")) throw new Error("Missing child result");"#
        )
    } else {
        format!(
            r#"let result;
try {{ result = await tools.collaboration__wait_agent({options}); }}
catch (error) {{ result = String(error); }}
if (!JSON.stringify(result).includes("abort")) throw new Error("Missing wait cancellation: " + JSON.stringify(result));"#
        )
    };
    let source = format!(
        r#"// @exec: {{"yield_time_ms":60000,"max_output_tokens":{max_output_tokens}}}
await tools.collaboration__spawn_agent({{task_name:"{task_name}",message:"Return the requested result.",fork_turns:"none"}});
text("{before}");
{wait_source}
{emit_wait}
text("{after}");"#
    );
    let (release, gate) = tokio::sync::oneshot::channel();
    let (finish, final_gate) = tokio::sync::oneshot::channel();
    let (complete_request, completion_gate) = tokio::sync::oneshot::channel();
    let mut streams = vec![
        vec![
            StreamingSseChunk {
                gate: None,
                body: sse(vec![
                    ev_response_created("root-exec"),
                    ev_custom_tool_call("original-exec", "exec", &source),
                ]),
            },
            StreamingSseChunk {
                gate: seed_history.then_some(completion_gate),
                body: sse(vec![ev_completed_with_tokens("root-exec", 120)]),
            },
        ],
        vec![StreamingSseChunk {
            gate: Some(gate),
            body: sse(vec![
                ev_response_created("child-final"),
                ev_assistant_message("child-answer", "child_result_marker"),
                ev_completed("child-final"),
            ]),
        }],
        vec![StreamingSseChunk {
            gate: Some(final_gate),
            body: sse(vec![
                ev_response_created("root-final"),
                ev_assistant_message("root-answer", "Done."),
                ev_completed("root-final"),
            ]),
        }],
    ];
    if seed_history {
        let mut seed = vec![ev_response_created("seed")];
        for index in 0..6 {
            seed.push(ev_assistant_message(
                &format!("seed-{index}"),
                "prior history",
            ));
        }
        seed.push(ev_completed_with_tokens("seed", 120));
        streams.insert(
            0,
            vec![StreamingSseChunk {
                gate: None,
                body: sse(seed),
            }],
        );
    }
    let (streaming, _) = start_streaming_sse_server(streams).await;
    let config_server = start_mock_server().await;
    let base_url = format!("{}/v1", streaming.uri());
    let test = test_codex()
        .with_model("gpt-5.6-sol")
        .with_model_info_override("gpt-5.6-sol", |model| {
            model.tool_mode = Some(ToolMode::CodeMode);
            model.use_responses_lite = true;
        })
        .with_config(move |config| {
            config.model_provider.base_url = Some(base_url);
            config.model_provider.supports_websockets = false;
            for feature in [
                Feature::Collab,
                Feature::MultiAgentV2,
                Feature::CodeMode,
                Feature::TokenBudget,
            ] {
                config.features.enable(feature).unwrap();
            }
            config
                .features
                .disable(Feature::EnableRequestCompression)
                .unwrap();
            config.multi_agent_v2.non_code_mode_only = false;
        })
        .build_with_auto_env(&config_server)
        .await?;
    if seed_history {
        test.codex
            .start_or_steer_turn(TurnInputRequest::user_input(vec![UserInput::Text {
                text: "Build prior history".into(),
                text_elements: Vec::new(),
            }]))
            .await?;
        wait_for_event(&test.codex, |event| {
            matches!(event, EventMsg::TurnComplete(_))
        })
        .await;
    }
    test.codex
        .start_or_steer_turn(TurnInputRequest::user_input(vec![UserInput::Text {
            text: "Delegate and wait in this execution.".into(),
            text_elements: Vec::new(),
        }]))
        .await?;
    wait_for_event(&test.codex, |event| {
        if let EventMsg::Error(error) = event {
            panic!("original wait failed: {error:?}");
        }
        if let EventMsg::RawResponseItem(item) = event
            && let codex_protocol::models::ResponseItem::CustomToolCallOutput { output, .. } = &item.item
        {
            panic!("original call returned before registering its wait: {output:?}");
        }
        assert!(!matches!(event, EventMsg::TurnComplete(_) | EventMsg::TurnCheckpointed(_)), "{event:?}");
        matches!(event, EventMsg::ItemStarted(item) if matches!(&item.item, TurnItem::CollabAgentToolCall(call) if call.tool == CollabAgentTool::Wait))
    }).await;
    if manual_reset {
        test.codex
            .submit(Op::Compact {
                reload: Some(selection),
                context_reset_request_id: Some("manual-wait-reset".into()),
            })
            .await?;
    }
    if manual_reset || compact {
        tokio::time::timeout(std::time::Duration::from_secs(5), wait_for_event(&test.codex, |event| {
            assert!(!matches!(event, EventMsg::TurnComplete(_) | EventMsg::TurnAborted(_) | EventMsg::TurnCheckpointed(_)), "{event:?}");
            matches!(event, EventMsg::ContextReset(reset) if reset.status == ContextResetStatus::Completed)
        })).await?;
    }
    if seed_history {
        complete_request
            .send(())
            .expect("old response usage is still pending");
    }
    let request_offset = usize::from(seed_history);
    tokio::time::timeout(
        std::time::Duration::from_secs(5),
        streaming.wait_for_request_count(2 + request_offset),
    )
    .await?;
    let waiting_requests = streaming.requests().await;
    let child_request: Value = serde_json::from_slice(&waiting_requests[1 + request_offset])?;
    assert!(
        child_request
            .to_string()
            .contains(&format!("Task name: /root/{task_name}")),
        "the real child receives its unchanged canonical identity"
    );
    assert!(
        child_request["input"]
            .as_array()
            .unwrap()
            .iter()
            .all(|item| item["call_id"] != "original-exec"),
        "the pending request belongs to the fresh child, not a parent continuation"
    );
    assert_eq!(
        waiting_requests.len(),
        2 + request_offset,
        "no provider request while original call waits"
    );
    if interrupt && shell.is_none() {
        test.codex.submit(Op::Interrupt).await?;
        tokio::time::timeout(
            std::time::Duration::from_secs(5),
            wait_for_event(&test.codex, |event| {
                assert!(
                    !matches!(
                        event,
                        EventMsg::TurnComplete(_) | EventMsg::TurnCheckpointed(_)
                    ),
                    "{event:?}"
                );
                matches!(event, EventMsg::TurnAborted(_))
            }),
        )
        .await?;
        assert_eq!(streaming.requests().await.len(), 2 + request_offset);
        test.codex.shutdown_and_wait().await?;
        streaming.shutdown().await;
        return Ok(());
    }
    if let Some((exit_code, child_ready)) = shell {
        let directory = tempfile::tempdir()?;
        let fifo = directory.path().join("shell-gate");
        assert!(
            std::process::Command::new("mkfifo")
                .arg(&fifo)
                .status()?
                .success()
        );
        test.codex
            .submit(Op::RunUserShellCommand {
                command: format!(
                    "read value < '{}'; printf native_shell_output_marker; exit {exit_code}",
                    fifo.display()
                ),
                timeout_ms: None,
            })
            .await?;
        wait_for_event(&test.codex, |event| {
            assert!(
                !matches!(
                    event,
                    EventMsg::TurnStarted(_) | EventMsg::TurnAborted(_) | EventMsg::TurnComplete(_)
                ),
                "{event:?}"
            );
            matches!(event, EventMsg::ExecCommandBegin(_))
        })
        .await;
        if child_ready {
            release.send(()).expect("child response is still pending");
            wait_for_event(&test.codex, |event| {
                assert!(!matches!(event, EventMsg::TurnStarted(_) | EventMsg::TurnAborted(_) | EventMsg::TurnComplete(_)), "{event:?}");
                matches!(event, EventMsg::ItemCompleted(item) if matches!(&item.item, TurnItem::CollabAgentToolCall(call) if call.tool == CollabAgentTool::Wait))
            }).await;
            assert!(
                tokio::time::timeout(
                    std::time::Duration::from_millis(100),
                    streaming.wait_for_request_count(3)
                )
                .await
                .is_err(),
                "selected wake must wait for shell output"
            );
        }
        if interrupt {
            test.codex.submit(Op::Interrupt).await?;
            tokio::time::timeout(
                std::time::Duration::from_secs(5),
                wait_for_event(&test.codex, |event| {
                    assert!(
                        !matches!(event, EventMsg::TurnStarted(_) | EventMsg::TurnComplete(_)),
                        "{event:?}"
                    );
                    matches!(event, EventMsg::TurnAborted(_))
                }),
            )
            .await?;
            assert_eq!(streaming.requests().await.len(), 2);
            test.codex.shutdown_and_wait().await?;
            streaming.shutdown().await;
            return Ok(());
        }
        tokio::fs::write(&fifo, b"release\n").await?;
        wait_for_event(&test.codex, |event| {
            assert!(!matches!(event, EventMsg::TurnStarted(_) | EventMsg::TurnAborted(_) | EventMsg::TurnComplete(_)), "{event:?}");
            matches!(event, EventMsg::RawResponseItem(item) if matches!(&item.item, codex_protocol::models::ResponseItem::CustomToolCallOutput { call_id, .. } if call_id == "original-exec"))
        }).await;
    } else {
        release.send(()).expect("child response is still pending");
    }
    tokio::time::timeout(
        std::time::Duration::from_secs(5),
        streaming.wait_for_request_count(3 + request_offset),
    )
    .await?;
    let requests = streaming.requests().await;
    assert_eq!(requests.len(), 3 + request_offset);
    let followup: Value = serde_json::from_slice(&requests[2 + request_offset])?;
    let input = followup["input"].as_array().expect("provider input");
    let calls = input
        .iter()
        .filter(|item| item["type"] == "custom_tool_call" && item["call_id"] == "original-exec")
        .collect::<Vec<_>>();
    let outputs = input
        .iter()
        .filter(|item| {
            item["type"] == "custom_tool_call_output" && item["call_id"] == "original-exec"
        })
        .collect::<Vec<_>>();
    assert_eq!(calls.len(), 1, "reset preserves the original calling item");
    assert_eq!(outputs.len(), 1, "one result for the original call");
    let output = outputs[0]["output"].to_string();
    assert!(output.contains("Script completed"), "{output}");
    if bounded_output {
        assert!(output.contains("…6 tokens truncated…"), "{output}");
        assert_eq!(output.matches("0123456789").count(), 2, "{output}");
        assert!(!output.contains("01234567890123456789"), "{output}");
    } else {
        for marker in ["before_wait_marker", "after_wait_marker"] {
            assert_eq!(output.matches(marker).count(), 1, "{output}");
        }
    }
    assert!(!output.contains("Script running"), "{output}");
    if !bounded_output {
        assert_eq!(
            output.matches("child_result_marker").count(),
            usize::from(expect_child),
            "{output}"
        );
    }
    if let Some((exit_code, _)) = shell {
        let text = followup.to_string();
        assert!(text.contains("native_shell_output_marker"), "{text}");
        assert!(text.contains(&format!("Exit code: {exit_code}")), "{text}");
    }
    if manual_reset || compact {
        assert!(followup.to_string().contains("native_wait_reload_marker"));
    }
    if !bounded_output && shell.is_none() {
        finish.send(()).expect("parent response is still pending");
        wait_for_event(&test.codex, |event| {
            matches!(event, EventMsg::TurnComplete(_))
        })
        .await;
    }
    test.codex.shutdown_and_wait().await?;
    streaming.shutdown().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn code_mode_spawn_delivers_plaintext() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = start_mock_server().await;
    let test = test_codex()
        .with_model("gpt-5.6-sol")
        .with_model_info_override("gpt-5.6-sol", |model| {
            model.tool_mode = Some(ToolMode::CodeMode);
            model.use_responses_lite = true;
        })
        .with_config(|config| {
            for feature in [
                Feature::Collab,
                Feature::MultiAgentV2,
                Feature::CodeMode,
                Feature::CodeModeHostGrpc,
            ] {
                config
                    .features
                    .enable(feature)
                    .expect("enable test feature");
            }
            config.multi_agent_v2.non_code_mode_only = false;
        })
        .build_with_auto_env(&server)
        .await?;
    let root_header = test.session_configured.thread_id.to_string();
    let mut created = test.thread_manager.subscribe_thread_created();

    mount_sse_once_match(
        &server,
        header("thread-id", root_header.as_str()),
        sse(vec![
            ev_response_created("spawn"),
            ev_custom_tool_call(
                "spawn-worker",
                "exec",
                r#"text(await tools.collaboration__spawn_agent({task_name: "worker", message: "Summarize the three stories.", fork_turns: "all"}));"#,
            ),
            ev_completed("spawn"),
        ]),
    )
    .await;
    mount_sse_once_match(
        &server,
        header("thread-id", root_header.as_str()),
        sse(vec![
            ev_assistant_message("delegated", "The worker started."),
            ev_completed("delegated"),
        ]),
    )
    .await;
    let child_requests = mount_sse_once_match(
        &server,
        move |request: &wiremock::Request| {
            request
                .headers
                .get("thread-id")
                .is_some_and(|id| id != root_header.as_str())
        },
        sse(vec![ev_response_created("child"), ev_completed("child")]),
    )
    .await;
    test.submit_turn("Delegate the summary task.").await?;
    let child_id = created.try_recv()?;
    let child = test.thread_manager.get_thread(child_id).await?;
    wait_for_event(&child, |event| matches!(event, EventMsg::TurnComplete(_))).await;

    let child_request = child_requests
        .requests()
        .into_iter()
        .find(|request| request.header("thread-id") == Some(child_id.to_string()))
        .expect("child receives its task");
    assert_eq!(
        strip_response_item_ids_from_json(strip_metadata_from_json(Value::Array(
            child_request.inputs_of_type("agent_message"),
        ))),
        json!([{
            "type": "agent_message",
            "author": "/root",
            "recipient": "/root/worker",
            "content": [{
                "type": "input_text",
                "text": "Message Type: NEW_TASK\nTask name: /root/worker\nSender: /root\nPayload:\nSummarize the three stories.",
            }],
        }]),
    );
    assert_eq!(test.thread_manager.list_thread_ids().await.len(), 2);
    Ok(())
}

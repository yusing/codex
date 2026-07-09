use anyhow::Result;
use codex_core::config::Config;
use codex_features::Feature;
use codex_model_provider_info::ModelProviderInfo;
use codex_model_provider_info::WireApi;
use codex_protocol::config_types::CollaborationMode;
use codex_protocol::config_types::ModeKind;
use codex_protocol::config_types::Settings;
use codex_protocol::openai_models::ModelInfo;
use codex_protocol::openai_models::ReasoningEffort;
use codex_protocol::openai_models::ReasoningEffortPreset;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::MULTI_AGENT_MODE_OPEN_TAG;
use codex_protocol::protocol::Op;
use codex_protocol::protocol::ThreadSettingsOverrides;
use codex_protocol::user_input::UserInput;
use core_test_support::responses::ev_assistant_message;
use core_test_support::responses::ev_completed;
use core_test_support::responses::ev_completed_with_tokens;
use core_test_support::responses::ev_function_call_with_namespace;
use core_test_support::responses::ev_message_item_added;
use core_test_support::responses::ev_output_text_delta;
use core_test_support::responses::ev_response_created;
use core_test_support::responses::mount_sse_once;
use core_test_support::responses::mount_sse_once_match;
use core_test_support::responses::mount_sse_sequence;
use core_test_support::responses::sse;
use core_test_support::responses::start_mock_server;
use core_test_support::skip_if_no_network;
use core_test_support::streaming_sse::StreamingSseChunk;
use core_test_support::streaming_sse::start_streaming_sse_server;
use core_test_support::test_codex::test_codex;
use core_test_support::wait_for_event;
use pretty_assertions::assert_eq;
use serde_json::Value;
use serde_json::json;
use tokio::sync::oneshot;

const NO_SPAWN_TEXT: &str = "Do not spawn sub-agents unless the user or applicable AGENTS.md/skill instructions explicitly ask for sub-agents, delegation, or parallel agent work.";
const PROACTIVE_TEXT: &str = "Proactive multi-agent delegation is active.";
const CUSTOM_MODE_HINT_TEXT: &str = "Use the configured delegation policy.";
const MULTI_AGENT_V2_NAMESPACE: &str = "collaboration";

fn add_ultra_reasoning(model_info: &mut ModelInfo) {
    model_info.supports_reasoning_summaries = true;
    model_info
        .supported_reasoning_levels
        .push(ReasoningEffortPreset {
            effort: ReasoningEffort::Ultra,
            description: "Ultra".to_string(),
        });
}

fn configure_multi_agent_v2(config: &mut Config) {
    config
        .features
        .enable(Feature::MultiAgentV2)
        .expect("test config should allow feature update");
}

// Configuring a custom mode hint also enables multi-agent V2 for the test.
fn configure_custom_mode_hint(config: &mut Config) {
    configure_multi_agent_v2(config);
    config.multi_agent_v2.multi_agent_mode_hint_text = Some(CUSTOM_MODE_HINT_TEXT.to_string());
}

fn configure_ultra(config: &mut Config) {
    configure_multi_agent_v2(config);
    config.model_reasoning_effort = Some(ReasoningEffort::Ultra);
}

fn developer_texts(input: &[Value]) -> Vec<&str> {
    message_texts(input, "developer")
}

fn message_texts<'a>(input: &'a [Value], role: &str) -> Vec<&'a str> {
    input
        .iter()
        .filter(|item| item.get("role").and_then(Value::as_str) == Some(role))
        .filter_map(|item| item.get("content")?.as_array())
        .flatten()
        .filter_map(|content| content.get("text")?.as_str())
        .collect()
}

fn count_containing(texts: &[&str], target: &str) -> usize {
    texts.iter().filter(|text| text.contains(target)).count()
}

fn request_contains(request: &wiremock::Request, text: &str) -> bool {
    serde_json::from_slice::<Value>(&request.body).is_ok_and(|body| {
        body.get("input")
            .and_then(Value::as_array)
            .is_some_and(|input| {
                ["developer", "user", "assistant"]
                    .into_iter()
                    .flat_map(|role| message_texts(input, role))
                    .any(|message| message.contains(text))
            })
    })
}

fn request_has_function_call_output(request: &wiremock::Request, call_id: &str) -> bool {
    serde_json::from_slice::<Value>(&request.body)
        .is_ok_and(|body| body_has_function_call_output(&body, call_id))
}

fn body_has_function_call_output(body: &Value, call_id: &str) -> bool {
    body.get("input")
        .and_then(Value::as_array)
        .is_some_and(|items| {
            items.iter().any(|item| {
                item.get("type").and_then(Value::as_str) == Some("function_call_output")
                    && item.get("call_id").and_then(Value::as_str) == Some(call_id)
            })
        })
}

fn request_tool_names(body: &Value) -> Vec<String> {
    let mut names = Vec::new();
    if let Some(tools) = body["tools"].as_array() {
        for tool in tools {
            if let Some(name) = tool.get("name").and_then(Value::as_str) {
                names.push(name.to_string());
            }
            if let Some(namespace_tools) = tool.get("tools").and_then(Value::as_array) {
                names.extend(
                    namespace_tools
                        .iter()
                        .filter_map(|namespace_tool| namespace_tool.get("name")?.as_str())
                        .map(str::to_string),
                );
            }
        }
    }
    names
}

fn request_has_output_schema(body: &Value) -> bool {
    !body["text"]["format"].is_null()
}

async fn submit_turn(
    codex: &codex_core::CodexThread,
    prompt: &str,
    effort: Option<ReasoningEffort>,
) -> Result<()> {
    codex
        .submit(Op::UserInput {
            items: vec![UserInput::Text {
                text: prompt.to_string(),
                text_elements: Vec::new(),
            }],
            final_output_json_schema: None,
            responsesapi_client_metadata: None,
            additional_context: Default::default(),
            thread_settings: ThreadSettingsOverrides {
                effort: effort.map(Some),
                ..Default::default()
            },
        })
        .await?;
    wait_for_event(codex, |event| matches!(event, EventMsg::TurnComplete(_))).await;
    Ok(())
}

fn streaming_chunk(events: Vec<Value>) -> StreamingSseChunk {
    StreamingSseChunk {
        gate: None,
        body: sse(events),
    }
}

fn incomplete_stream_chunk() -> StreamingSseChunk {
    StreamingSseChunk {
        gate: None,
        body: sse(vec![json!({
            "type": "response.output_item.done",
        })]),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn orchestrated_mode_uses_internal_roles_without_proactive_subagent_text() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = start_mock_server().await;
    let responses = mount_sse_sequence(
        &server,
        (1..=3)
            .map(|index| {
                sse(vec![
                    ev_response_created(&format!("resp-{index}")),
                    ev_completed(&format!("resp-{index}")),
                ])
            })
            .collect(),
    )
    .await;
    let test = test_codex()
        .with_config(configure_multi_agent_v2)
        .build(&server)
        .await?;

    test.codex
        .submit(Op::UserInput {
            items: vec![UserInput::Text {
                text: "hello".to_string(),
                text_elements: Vec::new(),
            }],
            final_output_json_schema: None,
            responsesapi_client_metadata: None,
            additional_context: Default::default(),
            thread_settings: ThreadSettingsOverrides {
                collaboration_mode: Some(CollaborationMode {
                    mode: ModeKind::Orchestrated,
                    settings: Settings {
                        model: test.session_configured.model.clone(),
                        reasoning_effort: Some(ReasoningEffort::High),
                        developer_instructions: None,
                    },
                }),
                effort: Some(Some(ReasoningEffort::High)),
                ..Default::default()
            },
        })
        .await?;
    wait_for_event(&test.codex, |event| {
        matches!(event, EventMsg::TurnComplete(_))
    })
    .await;

    let requests = responses.requests();
    assert_eq!(requests.len(), 3);
    let request = &requests[2];
    assert_eq!(
        request.body_json()["reasoning"]["effort"].as_str(),
        Some("high")
    );
    let input = request.input();
    let texts = developer_texts(&input);
    assert_eq!(count_containing(&texts, NO_SPAWN_TEXT), 1);
    assert_eq!(count_containing(&texts, PROACTIVE_TEXT), 0);
    let assistant_texts = message_texts(&input, "assistant");
    assert_eq!(
        (
            count_containing(&assistant_texts, "explorer: no final packet produced"),
            count_containing(&assistant_texts, "worker: no final packet produced"),
        ),
        (1, 1)
    );

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn orchestrated_mode_spawned_subagent_inherits_orchestrated_mode() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = start_mock_server().await;
    mount_sse_once_match(
        &server,
        |request: &wiremock::Request| {
            request_contains(request, "spawn child in orchestrated mode")
                && request_contains(request, "You are the explorer role in Orchestrated mode.")
        },
        sse(vec![
            ev_response_created("parent-explorer"),
            ev_assistant_message("parent-explorer-msg", "explorer: parent scan"),
            ev_completed("parent-explorer"),
        ]),
    )
    .await;
    mount_sse_once_match(
        &server,
        |request: &wiremock::Request| {
            request_contains(request, "spawn child in orchestrated mode")
                && request_contains(request, "You are the worker role in Orchestrated mode.")
        },
        sse(vec![
            ev_response_created("parent-worker"),
            ev_assistant_message("parent-worker-msg", "worker: parent prepared spawn"),
            ev_completed("parent-worker"),
        ]),
    )
    .await;
    let spawn_args = serde_json::to_string(&json!({
        "message": "child inherited orchestration task",
        "task_name": "child",
    }))?;
    mount_sse_once_match(
        &server,
        |request: &wiremock::Request| {
            request_contains(request, "spawn child in orchestrated mode")
                && request_contains(
                    request,
                    "You are the orchestrator role for the remainder of this Orchestrated-mode turn.",
                )
                && !request_has_function_call_output(request, "spawn-child")
        },
        sse(vec![
            ev_response_created("parent-orchestrator"),
            ev_function_call_with_namespace(
                "spawn-child",
                MULTI_AGENT_V2_NAMESPACE,
                "spawn_agent",
                &spawn_args,
            ),
            ev_completed("parent-orchestrator"),
        ]),
    )
    .await;
    let parent_followup = mount_sse_once_match(
        &server,
        |request: &wiremock::Request| request_has_function_call_output(request, "spawn-child"),
        sse(vec![
            ev_response_created("parent-followup"),
            ev_assistant_message("parent-followup-msg", "orc: spawned child"),
            ev_completed("parent-followup"),
        ]),
    )
    .await;
    let _child_explorer = mount_sse_once_match(
        &server,
        |request: &wiremock::Request| {
            request_contains(request, "child inherited orchestration task")
                && request_contains(request, "You are the explorer role in Orchestrated mode.")
        },
        sse(vec![
            ev_response_created("child-explorer"),
            ev_assistant_message("child-explorer-msg", "explorer: child scan"),
            ev_completed("child-explorer"),
        ]),
    )
    .await;
    let _child_worker = mount_sse_once_match(
        &server,
        |request: &wiremock::Request| {
            request_contains(request, "child inherited orchestration task")
                && request_contains(request, "You are the worker role in Orchestrated mode.")
        },
        sse(vec![
            ev_response_created("child-worker"),
            ev_assistant_message("child-worker-msg", "worker: child result"),
            ev_completed("child-worker"),
        ]),
    )
    .await;
    let _child_orchestrator = mount_sse_once_match(
        &server,
        |request: &wiremock::Request| {
            request_contains(request, "child inherited orchestration task")
                && request_contains(
                    request,
                    "You are the orchestrator role for the remainder of this Orchestrated-mode turn.",
                )
        },
        sse(vec![
            ev_response_created("child-orchestrator"),
            ev_assistant_message("child-orchestrator-msg", "orc: child done"),
            ev_completed("child-orchestrator"),
        ]),
    )
    .await;

    let test = test_codex()
        .with_config(|config| {
            configure_multi_agent_v2(config);
            config
                .features
                .enable(Feature::Collab)
                .expect("test config should allow feature update");
        })
        .build(&server)
        .await?;
    test.codex
        .submit(Op::UserInput {
            items: vec![UserInput::Text {
                text: "spawn child in orchestrated mode".to_string(),
                text_elements: Vec::new(),
            }],
            final_output_json_schema: None,
            responsesapi_client_metadata: None,
            additional_context: Default::default(),
            thread_settings: ThreadSettingsOverrides {
                collaboration_mode: Some(CollaborationMode {
                    mode: ModeKind::Orchestrated,
                    settings: Settings {
                        model: test.session_configured.model.clone(),
                        reasoning_effort: Some(ReasoningEffort::High),
                        developer_instructions: None,
                    },
                }),
                effort: Some(Some(ReasoningEffort::High)),
                ..Default::default()
            },
        })
        .await?;
    wait_for_event(&test.codex, |event| {
        matches!(event, EventMsg::TurnComplete(_))
    })
    .await;
    let spawn_output = parent_followup
        .function_call_output_text("spawn-child")
        .expect("parent should receive spawn tool output");
    assert!(
        spawn_output.contains("task_name") || spawn_output.contains("agent_id"),
        "unexpected spawn output: {spawn_output}"
    );
    let child_thread_id = test
        .thread_manager
        .list_thread_ids()
        .await
        .into_iter()
        .find(|thread_id| *thread_id != test.session_configured.thread_id)
        .expect("spawned child thread");
    let child_thread = test.thread_manager.get_thread(child_thread_id).await?;
    let child_snapshot = child_thread.config_snapshot().await;
    assert_eq!(
        child_snapshot.collaboration_mode.mode,
        ModeKind::Orchestrated
    );

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn orchestrated_mode_runs_internal_roles_before_orchestrator() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = start_mock_server().await;
    let responses = mount_sse_sequence(
        &server,
        vec![
            sse(vec![
                ev_response_created("resp-explorer"),
                ev_assistant_message("msg-explorer", "explorer: inspect multi-agent mode"),
                ev_completed_with_tokens("resp-explorer", /*total_tokens*/ 10),
            ]),
            sse(vec![
                ev_response_created("resp-worker"),
                ev_function_call_with_namespace(
                    "worker-list-agents",
                    MULTI_AGENT_V2_NAMESPACE,
                    "list_agents",
                    "{}",
                ),
                ev_completed_with_tokens("resp-worker", /*total_tokens*/ 20),
            ]),
            sse(vec![
                ev_response_created("resp-worker-followup"),
                ev_assistant_message("msg-worker", "worker: patch the orchestrated flow"),
                ev_completed_with_tokens("resp-worker-followup", /*total_tokens*/ 25),
            ]),
            sse(vec![
                ev_response_created("resp-orchestrator"),
                ev_assistant_message("msg-orchestrator", "orc: done"),
                ev_completed_with_tokens("resp-orchestrator", /*total_tokens*/ 30),
            ]),
        ],
    )
    .await;
    let test = test_codex()
        .with_config(|config| {
            configure_multi_agent_v2(config);
            config.orchestrated_mode.explorer_model = Some("gpt-5.4-mini".to_string());
            config.orchestrated_mode.explorer_reasoning_effort = Some(ReasoningEffort::Low);
            config.orchestrated_mode.worker_model = Some("gpt-5.2".to_string());
            config.orchestrated_mode.worker_reasoning_effort = Some(ReasoningEffort::Medium);
        })
        .build(&server)
        .await?;

    test.codex
        .submit(Op::UserInput {
            items: vec![UserInput::Text {
                text: "add orchestrated coverage".to_string(),
                text_elements: Vec::new(),
            }],
            final_output_json_schema: Some(json!({
                "type": "object",
                "properties": {
                    "summary": { "type": "string" }
                },
                "required": ["summary"],
                "additionalProperties": false
            })),
            responsesapi_client_metadata: None,
            additional_context: Default::default(),
            thread_settings: ThreadSettingsOverrides {
                collaboration_mode: Some(CollaborationMode {
                    mode: ModeKind::Orchestrated,
                    settings: Settings {
                        model: test.session_configured.model.clone(),
                        reasoning_effort: Some(ReasoningEffort::High),
                        developer_instructions: None,
                    },
                }),
                effort: Some(Some(ReasoningEffort::High)),
                ..Default::default()
            },
        })
        .await?;

    let mut active_role_events = Vec::new();
    let mut token_events = Vec::new();
    loop {
        let event = test
            .codex
            .next_event()
            .await
            .expect("stream ended unexpectedly")
            .msg;
        match event {
            EventMsg::OrchestratedRoleUpdated(event) => active_role_events.push(event.role),
            EventMsg::TokenCount(event) => token_events.push(event),
            EventMsg::TurnComplete(_) => break,
            _ => {}
        }
    }
    assert_eq!(
        active_role_events,
        vec![
            Some("explorer".to_string()),
            Some("worker".to_string()),
            None,
        ]
    );
    assert_eq!(token_events.len(), 4);
    let final_token_event = token_events.last().expect("final token event");

    let requests = responses.requests();
    assert_eq!(requests.len(), 4);

    let explorer_request = requests[0].body_json();
    assert_eq!(explorer_request["model"].as_str(), Some("gpt-5.4-mini"));
    assert_eq!(
        explorer_request["reasoning"]["effort"].as_str(),
        Some("low")
    );
    assert!(
        explorer_request["tools"]
            .as_array()
            .is_some_and(|tools| !tools.is_empty())
    );
    assert!(!request_has_output_schema(&explorer_request));
    let explorer_tools = request_tool_names(&explorer_request);
    assert!(
        !explorer_tools.iter().any(|tool| tool == "apply_patch"),
        "explorer should not receive direct edit tool: {explorer_tools:?}"
    );
    assert!(
        !explorer_tools.iter().any(|tool| matches!(
            tool.as_str(),
            "exec_command" | "shell_command" | "write_stdin"
        )),
        "explorer should not receive shell mutation tools: {explorer_tools:?}"
    );
    assert!(
        !explorer_tools.iter().any(|tool| tool == "spawn_agent"),
        "explorer should not receive recursive spawn tool: {explorer_tools:?}"
    );

    let worker_request = requests[1].body_json();
    assert_eq!(worker_request["model"].as_str(), Some("gpt-5.2"));
    assert_eq!(
        worker_request["reasoning"]["effort"].as_str(),
        Some("medium")
    );
    assert!(
        worker_request["tools"]
            .as_array()
            .is_some_and(|tools| !tools.is_empty())
    );
    assert!(!request_has_output_schema(&worker_request));
    let worker_tools = request_tool_names(&worker_request);
    assert!(
        worker_tools.iter().any(|tool| tool == "list_agents"),
        "worker should retain agent inspection tool: {worker_tools:?}"
    );
    assert!(
        !worker_tools.iter().any(|tool| tool == "spawn_agent"),
        "worker should not receive recursive spawn tool: {worker_tools:?}"
    );
    let worker_input = requests[1].input();
    assert_eq!(
        count_containing(
            &message_texts(&worker_input, "assistant"),
            "explorer: inspect multi-agent mode"
        ),
        1
    );
    requests[2].function_call_output("worker-list-agents");

    let orchestrator_request = requests[3].body_json();
    assert_eq!(
        orchestrator_request["model"].as_str(),
        Some(test.session_configured.model.as_str())
    );
    assert_eq!(
        orchestrator_request["reasoning"]["effort"].as_str(),
        Some("high")
    );
    assert!(request_has_output_schema(&orchestrator_request));
    assert!(
        !body_has_function_call_output(&orchestrator_request, "worker-list-agents"),
        "orchestrator should receive compact role packets, not worker tool outputs"
    );
    let orchestrator_input = requests[3].input();
    let orchestrator_assistant_texts = message_texts(&orchestrator_input, "assistant");
    assert_eq!(
        (
            count_containing(
                &orchestrator_assistant_texts,
                "explorer: inspect multi-agent mode",
            ),
            count_containing(
                &orchestrator_assistant_texts,
                "worker: patch the orchestrated flow",
            ),
        ),
        (1, 1)
    );
    assert_eq!(
        count_containing(
            &developer_texts(&orchestrator_input),
            "You are the orchestrator role for the remainder of this Orchestrated-mode turn.",
        ),
        1
    );

    let token_info = final_token_event.info.as_ref().expect("token usage info");
    assert_eq!(token_info.total_token_usage.total_tokens, 85);
    assert_eq!(token_info.last_token_usage.total_tokens, 30);
    let role_usage = token_info
        .orchestrated_role_token_usage
        .iter()
        .map(|usage| {
            (
                usage.role.as_str(),
                usage.model.as_str(),
                usage.token_usage.total_tokens,
            )
        })
        .collect::<Vec<_>>();
    assert_eq!(
        role_usage,
        [
            ("explorer", "gpt-5.4-mini", 10),
            ("worker", "gpt-5.2", 45),
            ("orchestrator", test.session_configured.model.as_str(), 30),
        ]
    );

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn orchestrated_mode_internal_roles_hide_legacy_collaboration_tools() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = start_mock_server().await;
    let responses = mount_sse_sequence(
        &server,
        (1..=3)
            .map(|index| {
                sse(vec![
                    ev_response_created(&format!("resp-legacy-{index}")),
                    ev_completed(&format!("resp-legacy-{index}")),
                ])
            })
            .collect(),
    )
    .await;
    let test = test_codex()
        .with_config(|config| {
            config
                .features
                .enable(Feature::Collab)
                .expect("test config should allow feature update");
        })
        .build(&server)
        .await?;

    test.codex
        .submit(Op::UserInput {
            items: vec![UserInput::Text {
                text: "legacy collaboration should not recurse".to_string(),
                text_elements: Vec::new(),
            }],
            final_output_json_schema: None,
            responsesapi_client_metadata: None,
            additional_context: Default::default(),
            thread_settings: ThreadSettingsOverrides {
                collaboration_mode: Some(CollaborationMode {
                    mode: ModeKind::Orchestrated,
                    settings: Settings {
                        model: test.session_configured.model.clone(),
                        reasoning_effort: Some(ReasoningEffort::High),
                        developer_instructions: None,
                    },
                }),
                effort: Some(Some(ReasoningEffort::High)),
                ..Default::default()
            },
        })
        .await?;
    wait_for_event(&test.codex, |event| {
        matches!(event, EventMsg::TurnComplete(_))
    })
    .await;

    let requests = responses.requests();
    assert_eq!(requests.len(), 3);
    for request in requests.iter().take(2) {
        let tool_names = request_tool_names(&request.body_json());
        assert!(
            !tool_names.iter().any(|tool| {
                matches!(
                    tool.as_str(),
                    "spawn_agent" | "send_input" | "resume_agent" | "wait_agent" | "close_agent"
                )
            }),
            "internal Orchestrated role should not receive legacy collaboration tools: {tool_names:?}"
        );
    }

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn orchestrated_mode_runs_internal_roles_for_queued_user_input() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let (root_complete_tx, root_complete_rx) = oneshot::channel();
    let (server, _completions) = start_streaming_sse_server(vec![
        vec![streaming_chunk(vec![
            ev_response_created("resp-explorer-1"),
            ev_assistant_message("msg-explorer-1", "explorer: first scan"),
            ev_completed_with_tokens("resp-explorer-1", /*total_tokens*/ 10),
        ])],
        vec![streaming_chunk(vec![
            ev_response_created("resp-worker-1"),
            ev_assistant_message("msg-worker-1", "worker: first patch"),
            ev_completed_with_tokens("resp-worker-1", /*total_tokens*/ 20),
        ])],
        vec![
            streaming_chunk(vec![
                ev_response_created("resp-orchestrator-1"),
                ev_message_item_added("msg-orchestrator-1", ""),
                ev_output_text_delta("orc: first answer"),
            ]),
            StreamingSseChunk {
                gate: Some(root_complete_rx),
                body: sse(vec![
                    ev_assistant_message("msg-orchestrator-1", "orc: first answer"),
                    ev_completed_with_tokens("resp-orchestrator-1", /*total_tokens*/ 30),
                ]),
            },
        ],
        vec![streaming_chunk(vec![
            ev_response_created("resp-explorer-2"),
            ev_assistant_message("msg-explorer-2", "explorer: second scan"),
            ev_completed_with_tokens("resp-explorer-2", /*total_tokens*/ 40),
        ])],
        vec![streaming_chunk(vec![
            ev_response_created("resp-worker-2"),
            ev_assistant_message("msg-worker-2", "worker: second patch"),
            ev_completed_with_tokens("resp-worker-2", /*total_tokens*/ 50),
        ])],
        vec![streaming_chunk(vec![
            ev_response_created("resp-orchestrator-2"),
            ev_assistant_message("msg-orchestrator-2", "orc: second answer"),
            ev_completed_with_tokens("resp-orchestrator-2", /*total_tokens*/ 60),
        ])],
    ])
    .await;
    let test = test_codex()
        .with_config(configure_multi_agent_v2)
        .build_with_streaming_server(&server)
        .await?;

    test.codex
        .submit(Op::UserInput {
            items: vec![UserInput::Text {
                text: "first prompt".to_string(),
                text_elements: Vec::new(),
            }],
            final_output_json_schema: None,
            responsesapi_client_metadata: None,
            additional_context: Default::default(),
            thread_settings: ThreadSettingsOverrides {
                collaboration_mode: Some(CollaborationMode {
                    mode: ModeKind::Orchestrated,
                    settings: Settings {
                        model: test.session_configured.model.clone(),
                        reasoning_effort: Some(ReasoningEffort::High),
                        developer_instructions: None,
                    },
                }),
                effort: Some(Some(ReasoningEffort::High)),
                ..Default::default()
            },
        })
        .await?;

    wait_for_event(&test.codex, |event| {
        matches!(event, EventMsg::AgentMessageContentDelta(_))
    })
    .await;

    test.codex
        .submit(Op::UserInput {
            items: vec![UserInput::Text {
                text: "second prompt".to_string(),
                text_elements: Vec::new(),
            }],
            final_output_json_schema: None,
            responsesapi_client_metadata: None,
            additional_context: Default::default(),
            thread_settings: Default::default(),
        })
        .await?;

    let _ = root_complete_tx.send(());
    wait_for_event(&test.codex, |event| {
        matches!(event, EventMsg::TurnComplete(_))
    })
    .await;

    let requests = server.requests().await;
    assert_eq!(requests.len(), 6);
    let request_bodies = requests
        .iter()
        .map(|request| serde_json::from_slice::<Value>(request))
        .collect::<serde_json::Result<Vec<_>>>()?;
    let developer_prompt_counts = |index: usize| {
        let input = request_bodies[index]
            .get("input")
            .and_then(Value::as_array)
            .expect("request input");
        let texts = developer_texts(input);
        (
            count_containing(&texts, "You are the explorer role in Orchestrated mode."),
            count_containing(&texts, "You are the worker role in Orchestrated mode."),
            count_containing(
                &texts,
                "You are the orchestrator role for the remainder of this Orchestrated-mode turn.",
            ),
        )
    };
    assert_eq!(developer_prompt_counts(0), (1, 0, 0));
    assert_eq!(developer_prompt_counts(1), (0, 1, 0));
    assert_eq!(developer_prompt_counts(2), (0, 0, 1));
    assert_eq!(developer_prompt_counts(3), (1, 0, 0));
    assert_eq!(developer_prompt_counts(4), (0, 1, 0));
    assert_eq!(developer_prompt_counts(5), (0, 0, 1));

    let second_explorer_input = request_bodies[3]
        .get("input")
        .and_then(Value::as_array)
        .expect("second explorer input");
    assert_eq!(
        count_containing(
            &message_texts(second_explorer_input, "user"),
            "second prompt"
        ),
        1
    );
    let final_orchestrator_input = request_bodies[5]
        .get("input")
        .and_then(Value::as_array)
        .expect("final orchestrator input");
    assert_eq!(
        (
            count_containing(
                &message_texts(final_orchestrator_input, "assistant"),
                "explorer: second scan",
            ),
            count_containing(
                &message_texts(final_orchestrator_input, "assistant"),
                "worker: second patch",
            ),
        ),
        (1, 1)
    );

    server.shutdown().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn orchestrated_mode_retry_preserves_role_instruction() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let (server, _completions) = start_streaming_sse_server(vec![
        vec![incomplete_stream_chunk()],
        vec![streaming_chunk(vec![
            ev_response_created("resp-explorer"),
            ev_assistant_message("msg-explorer", "explorer: retry scan"),
            ev_completed("resp-explorer"),
        ])],
        vec![streaming_chunk(vec![
            ev_response_created("resp-worker"),
            ev_assistant_message("msg-worker", "worker: retry result"),
            ev_completed("resp-worker"),
        ])],
        vec![streaming_chunk(vec![
            ev_response_created("resp-orchestrator"),
            ev_assistant_message("msg-orchestrator", "orc: retry done"),
            ev_completed("resp-orchestrator"),
        ])],
    ])
    .await;
    let model_provider = ModelProviderInfo {
        name: "openai".into(),
        base_url: Some(format!("{}/v1", server.uri())),
        env_key: Some("PATH".into()),
        env_key_instructions: None,
        experimental_bearer_token: None,
        auth: None,
        aws: None,
        wire_api: WireApi::Responses,
        query_params: None,
        http_headers: None,
        env_http_headers: None,
        request_max_retries: Some(0),
        stream_max_retries: Some(1),
        stream_idle_timeout_ms: Some(2000),
        websocket_connect_timeout_ms: None,
        requires_openai_auth: false,
        supports_websockets: false,
    };
    let test = test_codex()
        .with_config(move |config| {
            configure_multi_agent_v2(config);
            config.model_provider = model_provider;
        })
        .build_with_streaming_server(&server)
        .await?;

    test.codex
        .submit(Op::UserInput {
            items: vec![UserInput::Text {
                text: "retry orchestrated role".to_string(),
                text_elements: Vec::new(),
            }],
            final_output_json_schema: None,
            responsesapi_client_metadata: None,
            additional_context: Default::default(),
            thread_settings: ThreadSettingsOverrides {
                collaboration_mode: Some(CollaborationMode {
                    mode: ModeKind::Orchestrated,
                    settings: Settings {
                        model: test.session_configured.model.clone(),
                        reasoning_effort: Some(ReasoningEffort::High),
                        developer_instructions: None,
                    },
                }),
                effort: Some(Some(ReasoningEffort::High)),
                ..Default::default()
            },
        })
        .await?;
    wait_for_event(&test.codex, |event| {
        matches!(event, EventMsg::TurnComplete(_))
    })
    .await;

    let request_bodies = server
        .requests()
        .await
        .iter()
        .map(|request| serde_json::from_slice::<Value>(request))
        .collect::<serde_json::Result<Vec<_>>>()?;
    assert_eq!(request_bodies.len(), 4);
    for request in request_bodies.iter().take(2) {
        let input = request
            .get("input")
            .and_then(Value::as_array)
            .expect("request input");
        assert_eq!(
            count_containing(
                &developer_texts(input),
                "You are the explorer role in Orchestrated mode.",
            ),
            1
        );
    }

    server.shutdown().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ultra_reasoning_uses_max_and_proactive_mode() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = start_mock_server().await;
    let response = mount_sse_once(
        &server,
        sse(vec![ev_response_created("resp-1"), ev_completed("resp-1")]),
    )
    .await;
    let test = test_codex()
        .with_model_info_override("gpt-5.4", add_ultra_reasoning)
        .with_config(configure_ultra)
        .build(&server)
        .await?;

    submit_turn(&test.codex, "hello", /*effort*/ None).await?;

    let request = response.single_request();
    assert_eq!(
        request.body_json()["reasoning"]["effort"].as_str(),
        Some("max")
    );
    let input = request.input();
    let texts = developer_texts(&input);
    assert_eq!(
        (
            count_containing(&texts, NO_SPAWN_TEXT),
            count_containing(&texts, PROACTIVE_TEXT),
        ),
        (0, 1)
    );

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn configured_mode_hint_uses_custom_mode_across_reasoning_efforts() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = start_mock_server().await;
    let responses = mount_sse_sequence(
        &server,
        (1..=2)
            .map(|index| {
                sse(vec![
                    ev_response_created(&format!("resp-{index}")),
                    ev_completed(&format!("resp-{index}")),
                ])
            })
            .collect(),
    )
    .await;
    let test = test_codex()
        .with_model_info_override("gpt-5.4", add_ultra_reasoning)
        .with_config(configure_custom_mode_hint)
        .build(&server)
        .await?;
    let rollout_path = test
        .session_configured
        .rollout_path
        .clone()
        .expect("rollout path");

    submit_turn(&test.codex, "explicit", Some(ReasoningEffort::High)).await?;
    submit_turn(&test.codex, "proactive", Some(ReasoningEffort::Ultra)).await?;

    let requests = responses.requests();
    let first_input = requests[0].input();
    let first_texts = developer_texts(&first_input);
    let second_input = requests[1].input();
    let second_texts = developer_texts(&second_input);
    let instruction_counts = |texts: &[&str]| {
        (
            count_containing(texts, CUSTOM_MODE_HINT_TEXT),
            count_containing(texts, NO_SPAWN_TEXT),
            count_containing(texts, PROACTIVE_TEXT),
        )
    };
    assert_eq!(instruction_counts(&first_texts), (1, 0, 0));
    assert_eq!(instruction_counts(&second_texts), (1, 0, 0));
    let rollout_values = std::fs::read_to_string(rollout_path)?
        .lines()
        .map(serde_json::from_str::<Value>)
        .collect::<serde_json::Result<Vec<_>>>()?;
    let recorded_modes = rollout_values
        .iter()
        .filter(|value| value.get("type").and_then(Value::as_str) == Some("turn_context"))
        .filter_map(|value| value.pointer("/payload/multi_agent_mode").cloned())
        .collect::<Vec<_>>();
    assert_eq!(
        recorded_modes,
        [
            json!({"custom": CUSTOM_MODE_HINT_TEXT}),
            json!({"custom": CUSTOM_MODE_HINT_TEXT}),
        ]
    );

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn empty_configured_mode_hint_suppresses_builtin_text() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = start_mock_server().await;
    let response = mount_sse_once(
        &server,
        sse(vec![ev_response_created("resp-1"), ev_completed("resp-1")]),
    )
    .await;
    let test = test_codex()
        .with_config(|config| {
            configure_multi_agent_v2(config);
            config.multi_agent_v2.multi_agent_mode_hint_text = Some(String::new());
        })
        .build(&server)
        .await?;

    submit_turn(&test.codex, "hello", Some(ReasoningEffort::High)).await?;

    let input = response.single_request().input();
    let texts = developer_texts(&input);
    assert_eq!(
        (
            count_containing(&texts, MULTI_AGENT_MODE_OPEN_TAG),
            count_containing(&texts, NO_SPAWN_TEXT),
            count_containing(&texts, PROACTIVE_TEXT),
        ),
        (1, 0, 0)
    );

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn leaving_ultra_after_cold_resume_emits_explicit_mode() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = start_mock_server().await;
    let responses = mount_sse_sequence(
        &server,
        (1..=2)
            .map(|index| {
                sse(vec![
                    ev_response_created(&format!("resp-{index}")),
                    ev_completed(&format!("resp-{index}")),
                ])
            })
            .collect(),
    )
    .await;
    let initial = test_codex()
        .with_model_info_override("gpt-5.4", add_ultra_reasoning)
        .with_config(configure_ultra)
        .build(&server)
        .await?;
    let home = initial.home.clone();
    let rollout_path = initial
        .session_configured
        .rollout_path
        .clone()
        .expect("rollout path");

    submit_turn(&initial.codex, "before resume", /*effort*/ None).await?;
    drop(initial);

    let mut resume_builder = test_codex()
        .with_model_info_override("gpt-5.4", add_ultra_reasoning)
        .with_config(configure_ultra);
    let resumed = resume_builder.resume(&server, home, rollout_path).await?;
    submit_turn(&resumed.codex, "after resume", Some(ReasoningEffort::High)).await?;

    let requests = responses.requests();
    assert_eq!(
        (
            requests[0].body_json()["reasoning"]["effort"]
                .as_str()
                .map(str::to_string),
            requests[1].body_json()["reasoning"]["effort"]
                .as_str()
                .map(str::to_string),
        ),
        (Some("max".to_string()), Some("high".to_string()))
    );
    let resumed_input = requests[1].input();
    let texts = developer_texts(&resumed_input);
    assert_eq!(
        (
            count_containing(&texts, MULTI_AGENT_MODE_OPEN_TAG),
            count_containing(&texts, NO_SPAWN_TEXT),
            count_containing(&texts, PROACTIVE_TEXT),
        ),
        (2, 1, 1)
    );

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ultra_on_multi_agent_v1_uses_max_without_mode_instructions() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = start_mock_server().await;
    let response = mount_sse_once(
        &server,
        sse(vec![ev_response_created("resp-1"), ev_completed("resp-1")]),
    )
    .await;
    let test = test_codex()
        .with_model_info_override("gpt-5.4", add_ultra_reasoning)
        .with_config(|config| {
            config.model_reasoning_effort = Some(ReasoningEffort::Ultra);
        })
        .build(&server)
        .await?;

    submit_turn(&test.codex, "hello", /*effort*/ None).await?;

    let request = response.single_request();
    assert_eq!(
        request.body_json()["reasoning"]["effort"].as_str(),
        Some("max")
    );
    let input = request.input();
    let texts = developer_texts(&input);
    assert_eq!(count_containing(&texts, MULTI_AGENT_MODE_OPEN_TAG), 0);

    Ok(())
}

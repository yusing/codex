use anyhow::Result;
use codex_core::config::Config;
use codex_core::config::Constrained;
use codex_features::Feature;
use codex_model_provider_info::ModelProviderInfo;
use codex_model_provider_info::WireApi;
use codex_protocol::config_types::CollaborationMode;
use codex_protocol::config_types::ModeKind;
use codex_protocol::config_types::Settings;
use codex_protocol::items::AgentMessageContent;
use codex_protocol::items::AgentMessageItem;
use codex_protocol::items::TurnItem;
use codex_protocol::models::ContentItem;
use codex_protocol::models::PermissionProfile;
use codex_protocol::openai_models::ModelInfo;
use codex_protocol::openai_models::ReasoningEffort;
use codex_protocol::openai_models::ReasoningEffortPreset;
use codex_protocol::protocol::AskForApproval;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::MULTI_AGENT_MODE_OPEN_TAG;
use codex_protocol::protocol::Op;
use codex_protocol::protocol::ThreadSettingsOverrides;
use codex_protocol::user_input::UserInput;
use codex_utils_output_truncation::approx_token_count;
use core_test_support::responses::ev_assistant_message;
use core_test_support::responses::ev_completed;
use core_test_support::responses::ev_completed_with_tokens;
use core_test_support::responses::ev_function_call;
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
use core_test_support::skip_if_sandbox;
use core_test_support::streaming_sse::StreamingSseChunk;
use core_test_support::streaming_sse::start_streaming_sse_server;
use core_test_support::test_codex::test_codex;
use core_test_support::wait_for_event;
use pretty_assertions::assert_eq;
use serde_json::Value;
use serde_json::json;
use std::time::Duration;
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

async fn submit_orchestrated_user_input(
    test: &core_test_support::test_codex::TestCodex,
    text: String,
) -> Result<()> {
    test.codex
        .submit(Op::UserInput {
            items: vec![UserInput::Text {
                text,
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
    Ok(())
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

fn agent_message_text(message: &AgentMessageItem) -> String {
    message
        .content
        .iter()
        .map(|content| match content {
            AgentMessageContent::Text { text } => text.as_str(),
        })
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

fn request_last_developer_message_contains(request: &wiremock::Request, text: &str) -> bool {
    serde_json::from_slice::<Value>(&request.body).is_ok_and(|body| {
        body.get("input")
            .and_then(Value::as_array)
            .and_then(|input| message_texts(input, "developer").last().copied())
            .is_some_and(|message| message.contains(text))
    })
}

fn request_is_collab_spawn(request: &wiremock::Request) -> bool {
    serde_json::from_slice::<Value>(&request.body).is_ok_and(|body| {
        body["client_metadata"]["x-openai-subagent"].as_str() == Some("collab_spawn")
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

fn assistant_sse(id: &str, text: &str) -> String {
    sse(vec![
        ev_response_created(id),
        ev_assistant_message(&format!("{id}-msg"), text),
        ev_completed(id),
    ])
}

fn function_sse(id: &str, name: &str, arguments: &str) -> String {
    sse(vec![
        ev_response_created(id),
        ev_function_call(id, name, arguments),
        ev_completed(id),
    ])
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
        vec![
            sse(vec![
                ev_response_created("resp-contract"),
                ev_completed("resp-contract"),
            ]),
            sse(vec![
                ev_response_created("resp-explorer"),
                ev_completed("resp-explorer"),
            ]),
            sse(vec![
                ev_response_created("resp-worker-plan"),
                ev_completed("resp-worker-plan"),
            ]),
            sse(vec![
                ev_response_created("resp-plan-review"),
                ev_assistant_message("msg-plan-review", "plan-review: approved"),
                ev_completed("resp-plan-review"),
            ]),
            sse(vec![
                ev_response_created("resp-worker"),
                ev_assistant_message("msg-worker", "worker: complete\nno changes required"),
                ev_completed("resp-worker"),
            ]),
            sse(vec![
                ev_response_created("resp-result-review"),
                ev_assistant_message("msg-result-review", "result-review: approved\nverified"),
                ev_completed("resp-result-review"),
            ]),
            sse(vec![
                ev_response_created("resp-orchestrator"),
                ev_completed("resp-orchestrator"),
            ]),
        ],
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
    assert_eq!(requests.len(), 7);
    let request = &requests[6];
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
            count_containing(&assistant_texts, "task-contract: no final packet produced"),
            count_containing(&assistant_texts, "explorer: no final packet produced"),
            count_containing(&assistant_texts, "worker-plan: no final packet produced"),
            count_containing(&assistant_texts, "plan-review: approved"),
            count_containing(&assistant_texts, "worker: complete\nno changes required"),
            count_containing(&assistant_texts, "result-review: approved\nverified"),
        ),
        (1, 1, 1, 1, 1, 1)
    );

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn orchestrated_mode_accepts_orchestrator_prefix_on_completed_worker_packet() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = start_mock_server().await;
    let responses = mount_sse_sequence(
        &server,
        vec![
            assistant_sse("resp-contract", "task-contract: update repository"),
            assistant_sse("resp-explorer", "explorer: repository inspected"),
            assistant_sse("resp-worker-plan", "worker-plan: update safely"),
            assistant_sse("resp-plan-review", "plan-review: approved"),
            assistant_sse(
                "resp-worker",
                "orc: worker: complete\nrepository updated and verified",
            ),
            assistant_sse("resp-result-review", "result-review: approved"),
            assistant_sse("resp-orchestrator", "orc: done"),
        ],
    )
    .await;
    let test = test_codex()
        .with_config(configure_multi_agent_v2)
        .build(&server)
        .await?;

    submit_orchestrated_user_input(&test, "update repository".to_string()).await?;
    wait_for_event(&test.codex, |event| {
        matches!(event, EventMsg::TurnComplete(_))
    })
    .await;

    let requests = responses.requests();
    assert_eq!(requests.len(), 7);
    assert_eq!(
        count_containing(
            &developer_texts(&requests[6].input()),
            "You are the orchestrator role for the remainder of this Orchestrated-mode turn.",
        ),
        1
    );

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn orchestrated_mode_runs_direct_contract_without_planning_or_review() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = start_mock_server().await;
    let responses = mount_sse_sequence(
        &server,
        vec![
            assistant_sse(
                "resp-contract",
                "task-contract: direct\nfix the already-proven documentation mismatch",
            ),
            assistant_sse(
                "resp-worker",
                "worker: complete\ndocumentation fixed and verified",
            ),
            assistant_sse("resp-orchestrator", "orc: Complete."),
        ],
    )
    .await;
    let test = test_codex()
        .with_config(configure_multi_agent_v2)
        .build_with_auto_env(&server)
        .await?;

    submit_orchestrated_user_input(&test, "fix".to_string()).await?;
    wait_for_event(&test.codex, |event| {
        matches!(event, EventMsg::TurnComplete(_))
    })
    .await;

    let requests = responses.requests();
    assert_eq!(requests.len(), 3);
    assert_eq!(
        count_containing(
            &developer_texts(&requests[1].input()),
            "You are the worker-execution phase in Orchestrated mode.",
        ),
        1
    );
    assert_eq!(
        count_containing(
            &developer_texts(&requests[2].input()),
            "You are the orchestrator role for the remainder of this Orchestrated-mode turn.",
        ),
        1
    );
    assert_eq!(
        count_containing(
            &developer_texts(&requests[2].input()),
            "only when the latest worker packet begins `worker: complete` and is not marked truncated",
        ),
        1
    );

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn orchestrated_mode_retries_malformed_worker_before_result_review() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = start_mock_server().await;
    let responses = mount_sse_sequence(
        &server,
        vec![
            assistant_sse("resp-contract", "task-contract: update repository"),
            assistant_sse("resp-explorer", "explorer: repository inspected"),
            assistant_sse("resp-worker-plan", "worker-plan: update safely"),
            assistant_sse("resp-plan-review", "plan-review: approved"),
            assistant_sse("resp-worker-malformed", "orc: repository updated"),
            assistant_sse(
                "resp-worker-complete",
                "worker: complete\nrepository updated and verified",
            ),
            assistant_sse("resp-result-review", "result-review: approved"),
            assistant_sse("resp-orchestrator", "orc: Complete."),
        ],
    )
    .await;
    let test = test_codex()
        .with_config(configure_multi_agent_v2)
        .build_with_auto_env(&server)
        .await?;

    submit_orchestrated_user_input(&test, "update repository".to_string()).await?;
    wait_for_event(&test.codex, |event| {
        matches!(event, EventMsg::TurnComplete(_))
    })
    .await;

    let requests = responses.requests();
    assert_eq!(requests.len(), 8);
    assert_eq!(
        count_containing(
            &developer_texts(&requests[5].input()),
            "You are the worker-execution phase in Orchestrated mode.",
        ),
        1
    );
    assert_eq!(
        count_containing(
            &developer_texts(&requests[6].input()),
            "You are the orchestrator result-review phase in Orchestrated mode.",
        ),
        1
    );

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn orchestrated_mode_retains_and_suppresses_repeated_unavailable_executable() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = start_mock_server().await;
    let executable = "codex_missing_orchestrated_test_executable";
    let args = serde_json::to_string(&json!({
        "cmd": format!("{executable} --password super-secret-value"),
        "yield_time_ms": 1_000,
    }))?;
    let responses = mount_sse_sequence(
        &server,
        vec![
            assistant_sse(
                "execution-facts-contract",
                "task-contract: fix retry behavior",
            ),
            assistant_sse("execution-facts-explorer", "explorer: retry path located"),
            assistant_sse(
                "execution-facts-plan",
                "worker-plan: verify unavailable command",
            ),
            assistant_sse("execution-facts-plan-review", "plan-review: approved"),
            function_sse("execution-facts-call-1", "exec_command", &args),
            function_sse("execution-facts-call-2", "exec_command", &args),
            assistant_sse(
                "execution-facts-worker-result-1",
                "worker: incomplete; required executable unavailable",
            ),
            assistant_sse(
                "execution-facts-result-review-1",
                "result-review: revise\nowner: worker\nretry required command",
            ),
            function_sse("execution-facts-list-call", "list_agents", "{}"),
            function_sse("execution-facts-call-3", "exec_command", &args),
            assistant_sse(
                "execution-facts-worker-result-2",
                "worker: incomplete; required executable unavailable",
            ),
            assistant_sse(
                "execution-facts-result-review-2",
                "result-review: revise\nowner: worker\nretry required command",
            ),
            assistant_sse("execution-facts-root", "orc: executable unavailable"),
        ],
    )
    .await;
    let test = test_codex()
        .with_config(configure_multi_agent_v2)
        .build_with_auto_env(&server)
        .await?;

    submit_orchestrated_user_input(&test, "fix repeated command retries".to_string()).await?;
    wait_for_event(&test.codex, |event| {
        matches!(event, EventMsg::TurnComplete(_))
    })
    .await;

    let requests = responses.requests();
    assert_eq!(requests.len(), 13);
    assert!(!body_has_function_call_output(
        &requests[7].body_json(),
        "execution-facts-call-1"
    ));
    let first_review_user_text = message_texts(requests[7].input().as_slice(), "user").join("\n");
    assert!(first_review_user_text.contains("outcome=executableUnavailable"));
    assert!(first_review_user_text.contains(&format!("executable=\"{executable}\"")));
    assert!(!first_review_user_text.contains("super-secret-value"));

    let retry_input = requests[8].input();
    assert!(
        message_texts(&retry_input, "user")
            .iter()
            .any(|text| text.contains("outcome=executableUnavailable"))
    );
    let repeated_output = responses
        .function_call_output_text("execution-facts-call-2")
        .expect("repeated command output");
    assert!(repeated_output.contains("suppressed unchanged deterministic failure"));
    let revalidated_output = responses
        .function_call_output_text("execution-facts-call-3")
        .expect("revalidated command output");
    assert!(!revalidated_output.contains("suppressed unchanged deterministic failure"));

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn orchestrated_mode_routes_root_owned_correction_without_worker_retry() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = start_mock_server().await;
    let responses = mount_sse_sequence(
        &server,
        vec![
            assistant_sse(
                "root-owner-contract",
                "task-contract: review implementation",
            ),
            assistant_sse("root-owner-explorer", "explorer: implementation located"),
            assistant_sse("root-owner-plan", "worker-plan: implement and review"),
            assistant_sse("root-owner-plan-review", "plan-review: approved"),
            assistant_sse(
                "root-owner-worker",
                "worker: incomplete; required post-work subagent review unavailable",
            ),
            assistant_sse(
                "root-owner-result-review",
                "result-review: revise\nowner: root\nrun required post-work subagent review",
            ),
            assistant_sse("root-owner-root", "orc: root-owned review required"),
        ],
    )
    .await;
    let test = test_codex()
        .with_config(configure_multi_agent_v2)
        .build_with_auto_env(&server)
        .await?;

    submit_orchestrated_user_input(&test, "review implementation".to_string()).await?;
    wait_for_event(&test.codex, |event| {
        matches!(event, EventMsg::TurnComplete(_))
    })
    .await;

    let requests = responses.requests();
    assert_eq!(requests.len(), 7);
    assert_eq!(
        count_containing(
            &developer_texts(&requests[6].input()),
            "You are the worker-execution phase in Orchestrated mode.",
        ),
        0
    );
    assert_eq!(
        count_containing(
            &message_texts(&requests[6].input(), "assistant"),
            "owner: root",
        ),
        1
    );

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn orchestrated_mode_routes_explorer_owned_correction_back_to_worker() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = start_mock_server().await;
    let responses = mount_sse_sequence(
        &server,
        vec![
            assistant_sse(
                "explorer-owner-contract",
                "task-contract: implement bounded change",
            ),
            assistant_sse(
                "explorer-owner-initial-explorer",
                "explorer: initial implementation evidence",
            ),
            assistant_sse(
                "explorer-owner-plan",
                "worker-plan: implement using established evidence",
            ),
            assistant_sse("explorer-owner-plan-review", "plan-review: approved"),
            assistant_sse(
                "explorer-owner-worker-1",
                "worker: incomplete\nevidence-needed: locate the structured error owner",
            ),
            assistant_sse(
                "explorer-owner-result-review-1",
                "result-review: revise\nowner: explorer\nquestion: Which type owns structured errors?\nscope: core/src/tools",
            ),
            assistant_sse(
                "explorer-owner-correction-evidence",
                "explorer: structured errors are owned by core/src/tools/context.rs:80",
            ),
            assistant_sse(
                "explorer-owner-worker-2",
                "worker: complete\nimplemented with correction evidence",
            ),
            assistant_sse(
                "explorer-owner-result-review-2",
                "result-review: approved",
            ),
            assistant_sse("explorer-owner-root", "orc: completed"),
        ],
    )
    .await;
    let test = test_codex()
        .with_config(configure_multi_agent_v2)
        .build_with_auto_env(&server)
        .await?;

    submit_orchestrated_user_input(&test, "implement with focused evidence".to_string()).await?;
    wait_for_event(&test.codex, |event| {
        matches!(event, EventMsg::TurnComplete(_))
    })
    .await;

    let requests = responses.requests();
    assert_eq!(requests.len(), 10);
    assert_eq!(
        count_containing(
            &developer_texts(&requests[6].input()),
            "You are the explorer phase in Orchestrated mode.",
        ),
        1
    );
    assert_eq!(
        count_containing(
            &message_texts(&requests[6].input(), "assistant"),
            "owner: explorer",
        ),
        1
    );
    assert_eq!(
        count_containing(
            &developer_texts(&requests[7].input()),
            "You are the worker-execution phase in Orchestrated mode.",
        ),
        1
    );
    assert_eq!(
        count_containing(
            &message_texts(&requests[7].input(), "assistant"),
            "explorer: structured errors are owned by core/src/tools/context.rs:80",
        ),
        1
    );

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn orchestrated_mode_does_not_default_missing_correction_owner_to_worker() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = start_mock_server().await;
    let responses = mount_sse_sequence(
        &server,
        vec![
            assistant_sse("missing-owner-contract", "task-contract: bounded change"),
            assistant_sse("missing-owner-explorer", "explorer: evidence"),
            assistant_sse("missing-owner-plan", "worker-plan: implement"),
            assistant_sse("missing-owner-plan-review", "plan-review: approved"),
            assistant_sse(
                "missing-owner-worker",
                "worker: incomplete\nevidence-needed: locate behavior owner",
            ),
            assistant_sse(
                "missing-owner-result-review",
                "result-review: revise\nlocate behavior owner",
            ),
            assistant_sse("missing-owner-root", "orc: correction owner was missing"),
        ],
    )
    .await;
    let test = test_codex()
        .with_config(configure_multi_agent_v2)
        .build_with_auto_env(&server)
        .await?;

    submit_orchestrated_user_input(&test, "do not infer correction ownership".to_string()).await?;
    wait_for_event(&test.codex, |event| {
        matches!(event, EventMsg::TurnComplete(_))
    })
    .await;

    let requests = responses.requests();
    assert_eq!(requests.len(), 7);
    assert_eq!(
        count_containing(
            &developer_texts(&requests[6].input()),
            "You are the worker-execution phase in Orchestrated mode.",
        ),
        0
    );
    assert_eq!(
        count_containing(
            &developer_texts(&requests[6].input()),
            "You are the orchestrator role for the remainder of this Orchestrated-mode turn.",
        ),
        1
    );

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn orchestrated_mode_routes_direct_worker_evidence_request_through_explorer() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = start_mock_server().await;
    let responses = mount_sse_sequence(
        &server,
        vec![
            assistant_sse(
                "direct-explorer-contract",
                "task-contract: direct\napply established correction",
            ),
            assistant_sse(
                "direct-explorer-worker-1",
                "worker: incomplete\nevidence-needed: confirm the exact call site\nscope: core/src/session",
            ),
            assistant_sse(
                "direct-explorer-evidence",
                "explorer: exact call site is core/src/session/orchestrated.rs:226",
            ),
            assistant_sse(
                "direct-explorer-worker-2",
                "worker: complete\napplied established correction",
            ),
            assistant_sse("direct-explorer-root", "orc: completed"),
        ],
    )
    .await;
    let test = test_codex()
        .with_config(configure_multi_agent_v2)
        .build_with_auto_env(&server)
        .await?;

    submit_orchestrated_user_input(&test, "apply established correction".to_string()).await?;
    wait_for_event(&test.codex, |event| {
        matches!(event, EventMsg::TurnComplete(_))
    })
    .await;

    let requests = responses.requests();
    assert_eq!(requests.len(), 5);
    assert_eq!(
        count_containing(
            &developer_texts(&requests[2].input()),
            "You are the explorer phase in Orchestrated mode.",
        ),
        1
    );
    assert_eq!(
        count_containing(
            &developer_texts(&requests[3].input()),
            "You are the worker-execution phase in Orchestrated mode.",
        ),
        1
    );
    assert_eq!(
        count_containing(
            &message_texts(&requests[3].input(), "assistant"),
            "explorer: exact call site is core/src/session/orchestrated.rs:226",
        ),
        1
    );

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn orchestrated_mode_stops_stagnant_direct_worker_retries() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = start_mock_server().await;
    let responses = mount_sse_sequence(
        &server,
        vec![
            assistant_sse("resp-contract", "task-contract: direct\nfix known typo"),
            assistant_sse(
                "resp-worker-incomplete-1",
                "worker: incomplete\nverification failed",
            ),
            assistant_sse(
                "resp-worker-incomplete-2",
                "worker: incomplete\nverification failed",
            ),
            assistant_sse("resp-orchestrator", "orc: Work was not completed."),
        ],
    )
    .await;
    let test = test_codex()
        .with_config(configure_multi_agent_v2)
        .build_with_auto_env(&server)
        .await?;

    submit_orchestrated_user_input(&test, "fix".to_string()).await?;
    wait_for_event(&test.codex, |event| {
        matches!(event, EventMsg::TurnComplete(_))
    })
    .await;

    let requests = responses.requests();
    assert_eq!(requests.len(), 4);
    for request in requests.iter().take(3).skip(1) {
        assert_eq!(
            count_containing(
                &developer_texts(&request.input()),
                "You are the worker-execution phase in Orchestrated mode.",
            ),
            1
        );
    }
    assert_eq!(
        count_containing(
            &message_texts(&requests[3].input(), "assistant"),
            "worker: incomplete\nverification failed",
        ),
        2
    );
    assert_eq!(
        count_containing(
            &developer_texts(&requests[3].input()),
            "only when the latest worker packet begins `worker: complete` and is not marked truncated",
        ),
        1
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
                && request_last_developer_message_contains(
                    request,
                    "You are the task-contract phase in Orchestrated mode.",
                )
        },
        sse(vec![
            ev_response_created("parent-contract"),
            ev_assistant_message("parent-contract-msg", "task-contract: spawn child"),
            ev_completed("parent-contract"),
        ]),
    )
    .await;
    mount_sse_once_match(
        &server,
        |request: &wiremock::Request| {
            request_contains(request, "spawn child in orchestrated mode")
                && request_last_developer_message_contains(
                    request,
                    "You are the explorer phase in Orchestrated mode.",
                )
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
                && request_last_developer_message_contains(
                    request,
                    "You are the worker-plan phase in Orchestrated mode.",
                )
        },
        sse(vec![
            ev_response_created("parent-worker-plan"),
            ev_assistant_message(
                "parent-worker-plan-msg",
                "worker-plan: parent prepared spawn",
            ),
            ev_completed("parent-worker-plan"),
        ]),
    )
    .await;
    mount_sse_once_match(
        &server,
        |request: &wiremock::Request| {
            request_contains(request, "spawn child in orchestrated mode")
                && request_last_developer_message_contains(
                    request,
                    "You are the orchestrator plan-review phase in Orchestrated mode.",
                )
        },
        sse(vec![
            ev_response_created("parent-plan-review"),
            ev_assistant_message(
                "parent-plan-review-msg",
                "plan-review: approved parent spawn",
            ),
            ev_completed("parent-plan-review"),
        ]),
    )
    .await;
    mount_sse_once_match(
        &server,
        |request: &wiremock::Request| {
            request_contains(request, "spawn child in orchestrated mode")
                && request_last_developer_message_contains(
                    request,
                    "You are the worker-execution phase in Orchestrated mode.",
                )
        },
        sse(vec![
            ev_response_created("parent-worker"),
            ev_assistant_message("parent-worker-msg", "worker: complete; parent ready"),
            ev_completed("parent-worker"),
        ]),
    )
    .await;
    mount_sse_once_match(
        &server,
        |request: &wiremock::Request| {
            request_contains(request, "spawn child in orchestrated mode")
                && request_last_developer_message_contains(
                    request,
                    "You are the orchestrator result-review phase in Orchestrated mode.",
                )
        },
        sse(vec![
            ev_response_created("parent-result-review"),
            ev_assistant_message(
                "parent-result-review-msg",
                "result-review: approved parent result",
            ),
            ev_completed("parent-result-review"),
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
                && request_last_developer_message_contains(
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
    let _child_contract = mount_sse_once_match(
        &server,
        |request: &wiremock::Request| {
            request_is_collab_spawn(request)
                && request_last_developer_message_contains(
                    request,
                    "You are the task-contract phase in Orchestrated mode.",
                )
        },
        sse(vec![
            ev_response_created("child-contract"),
            ev_assistant_message("child-contract-msg", "task-contract: child task"),
            ev_completed("child-contract"),
        ]),
    )
    .await;
    let _child_explorer = mount_sse_once_match(
        &server,
        |request: &wiremock::Request| {
            request_is_collab_spawn(request)
                && request_last_developer_message_contains(
                    request,
                    "You are the explorer phase in Orchestrated mode.",
                )
        },
        sse(vec![
            ev_response_created("child-explorer"),
            ev_assistant_message("child-explorer-msg", "explorer: child scan"),
            ev_completed("child-explorer"),
        ]),
    )
    .await;
    let _child_worker_plan = mount_sse_once_match(
        &server,
        |request: &wiremock::Request| {
            request_is_collab_spawn(request)
                && request_last_developer_message_contains(
                    request,
                    "You are the worker-plan phase in Orchestrated mode.",
                )
        },
        sse(vec![
            ev_response_created("child-worker-plan"),
            ev_assistant_message("child-worker-plan-msg", "worker-plan: child plan"),
            ev_completed("child-worker-plan"),
        ]),
    )
    .await;
    let _child_plan_review = mount_sse_once_match(
        &server,
        |request: &wiremock::Request| {
            request_is_collab_spawn(request)
                && request_last_developer_message_contains(
                    request,
                    "You are the orchestrator plan-review phase in Orchestrated mode.",
                )
        },
        sse(vec![
            ev_response_created("child-plan-review"),
            ev_assistant_message("child-plan-review-msg", "plan-review: approved child plan"),
            ev_completed("child-plan-review"),
        ]),
    )
    .await;
    let child_worker = mount_sse_once_match(
        &server,
        |request: &wiremock::Request| {
            request_is_collab_spawn(request)
                && request_last_developer_message_contains(
                    request,
                    "You are the worker-execution phase in Orchestrated mode.",
                )
        },
        sse(vec![
            ev_response_created("child-worker"),
            ev_assistant_message("child-worker-msg", "worker: complete; child result"),
            ev_completed("child-worker"),
        ]),
    )
    .await;
    let child_result_review = mount_sse_once_match(
        &server,
        |request: &wiremock::Request| {
            request_is_collab_spawn(request)
                && request_last_developer_message_contains(
                    request,
                    "You are the orchestrator result-review phase in Orchestrated mode.",
                )
        },
        sse(vec![
            ev_response_created("child-result-review"),
            ev_assistant_message(
                "child-result-review-msg",
                "result-review: approved child result",
            ),
            ev_completed("child-result-review"),
        ]),
    )
    .await;
    let child_orchestrator = mount_sse_once_match(
        &server,
        |request: &wiremock::Request| {
            request_is_collab_spawn(request)
                && request_last_developer_message_contains(
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
    let child_orchestrator_request = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if let Some(request) = child_orchestrator.last_request() {
                let body = request.body_json();
                if body["client_metadata"]["x-openai-subagent"].as_str()
                    == Some("collab_spawn")
                    && body
                        .get("input")
                        .and_then(Value::as_array)
                        .and_then(|input| message_texts(input, "developer").last().copied())
                        .is_some_and(|message| {
                            message.contains(
                                "You are the orchestrator role for the remainder of this Orchestrated-mode turn.",
                            )
                        })
                {
                    break body;
                }
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("timed out waiting for spawned Orchestrated execution requests");
    let child_worker_request = child_worker
        .last_request()
        .expect("spawned Orchestrated worker request")
        .body_json();
    let child_worker_tools = request_tool_names(&child_worker_request);
    assert!(
        !child_worker_tools.is_empty(),
        "spawned Orchestrated worker should receive tools"
    );
    let child_orchestrator_tools = request_tool_names(&child_orchestrator_request);
    assert!(
        !child_orchestrator_tools.is_empty(),
        "spawned Orchestrated root should receive collaboration tools after plan approval"
    );
    assert!(
        !child_orchestrator_tools.iter().any(|tool| {
            matches!(
                tool.as_str(),
                "apply_patch" | "exec_command" | "shell_command" | "write_stdin"
            )
        }),
        "spawned Orchestrated root should not execute worker tools: {child_orchestrator_tools:?}"
    );
    assert!(
        child_result_review.last_request().is_some(),
        "spawned child should run result review"
    );

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn orchestrated_mode_runs_internal_roles_before_orchestrator() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = start_mock_server().await;
    let worker_packet =
        "worker: complete; patch the orchestrated flow\nevidence: /tmp/orchestrated-worker.log";
    let responses = mount_sse_sequence(
        &server,
        vec![
            sse(vec![
                ev_response_created("resp-contract"),
                ev_assistant_message("msg-contract", "task-contract: add orchestrated coverage"),
                ev_completed_with_tokens("resp-contract", /*total_tokens*/ 5),
            ]),
            sse(vec![
                ev_response_created("resp-explorer"),
                ev_assistant_message("msg-explorer", "explorer: inspect multi-agent mode"),
                ev_completed_with_tokens("resp-explorer", /*total_tokens*/ 10),
            ]),
            sse(vec![
                ev_response_created("resp-worker-plan"),
                ev_assistant_message("msg-worker-plan", "worker-plan: update flow and tests"),
                ev_completed_with_tokens("resp-worker-plan", /*total_tokens*/ 15),
            ]),
            sse(vec![
                ev_response_created("resp-plan-review"),
                ev_assistant_message("msg-plan-review", "plan-review: approved; aligned"),
                ev_completed_with_tokens("resp-plan-review", /*total_tokens*/ 20),
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
                ev_assistant_message("msg-worker", worker_packet),
                ev_completed_with_tokens("resp-worker-followup", /*total_tokens*/ 25),
            ]),
            sse(vec![
                ev_response_created("resp-result-review"),
                ev_assistant_message("msg-result-review", "result-review: approved; verified"),
                ev_completed_with_tokens("resp-result-review", /*total_tokens*/ 27),
            ]),
            sse(vec![
                ev_response_created("resp-orchestrator"),
                ev_assistant_message("msg-orchestrator", "orc: done"),
                ev_completed_with_tokens("resp-orchestrator", /*total_tokens*/ 30),
            ]),
            sse(vec![
                ev_response_created("resp-resumed"),
                ev_assistant_message("msg-resumed", "resumed"),
                ev_completed("resp-resumed"),
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
    let rollout_path = test
        .session_configured
        .rollout_path
        .clone()
        .expect("rollout path");

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
            Some("task-contract".to_string()),
            Some("explorer".to_string()),
            Some("worker-plan".to_string()),
            Some("plan-review".to_string()),
            Some("worker".to_string()),
            Some("result-review".to_string()),
            None,
        ]
    );
    assert_eq!(token_events.len(), 8);
    let final_token_event = token_events.last().expect("final token event");

    let requests = responses.requests();
    assert_eq!(requests.len(), 8);

    let contract_request = requests[0].body_json();
    assert_eq!(
        contract_request["model"].as_str(),
        Some(test.session_configured.model.as_str())
    );
    assert_eq!(
        contract_request["reasoning"]["effort"].as_str(),
        Some("high")
    );
    assert_eq!(request_tool_names(&contract_request), Vec::<String>::new());

    let explorer_request = requests[1].body_json();
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
        explorer_tools
            .iter()
            .any(|tool| matches!(tool.as_str(), "exec_command" | "shell_command")),
        "explorer should receive shell access: {explorer_tools:?}"
    );
    assert!(
        explorer_tools.iter().any(|tool| tool == "write_stdin"),
        "explorer should receive stdin transport: {explorer_tools:?}"
    );
    assert!(
        !explorer_tools.iter().any(|tool| tool == "spawn_agent"),
        "explorer should not receive recursive spawn tool: {explorer_tools:?}"
    );
    assert_eq!(
        count_containing(
            &developer_texts(&requests[1].input()),
            "For each task-relevant candidate, use a side-effect-free host lookup",
        ),
        1
    );

    let worker_plan_request = requests[2].body_json();
    assert_eq!(worker_plan_request["model"].as_str(), Some("gpt-5.2"));
    assert_eq!(
        worker_plan_request["reasoning"]["effort"].as_str(),
        Some("medium")
    );
    assert_eq!(
        request_tool_names(&worker_plan_request),
        Vec::<String>::new()
    );
    let worker_plan_input = requests[2].input();
    assert_eq!(
        count_containing(
            &message_texts(&worker_plan_input, "assistant"),
            "explorer: inspect multi-agent mode"
        ),
        1
    );
    assert_eq!(
        count_containing(
            &developer_texts(&requests[2].input()),
            "Reconcile every planned executable with Explorer's host-availability evidence.",
        ),
        1
    );

    let plan_review_request = requests[3].body_json();
    assert_eq!(
        plan_review_request["model"].as_str(),
        Some(test.session_configured.model.as_str())
    );
    assert_eq!(
        plan_review_request["reasoning"]["effort"].as_str(),
        Some("high")
    );
    assert_eq!(
        request_tool_names(&plan_review_request),
        Vec::<String>::new()
    );
    assert_eq!(
        count_containing(
            &developer_texts(&requests[3].input()),
            "Treat every planned external executable without Explorer or plan-evidence availability evidence as unanswered.",
        ),
        1
    );

    let worker_request = requests[4].body_json();
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
    assert_eq!(
        count_containing(
            &developer_texts(&requests[4].input()),
            "Do not probe a known-unavailable executable or guess substitutes",
        ),
        1
    );
    let worker_input = requests[4].input();
    assert_eq!(
        count_containing(
            &message_texts(&worker_input, "assistant"),
            "plan-review: approved; aligned"
        ),
        1
    );
    requests[5].function_call_output("worker-list-agents");

    let result_review_request = requests[6].body_json();
    assert_eq!(
        result_review_request["model"].as_str(),
        Some(test.session_configured.model.as_str())
    );
    assert_eq!(
        request_tool_names(&result_review_request),
        Vec::<String>::new()
    );

    let orchestrator_request = requests[7].body_json();
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
    assert_eq!(
        count_containing(
            &developer_texts(&requests[7].input()),
            "A later delegated review or check finding supersedes earlier result-review approval.",
        ),
        1
    );
    assert_eq!(
        count_containing(
            &developer_texts(&requests[7].input()),
            "delegate missing repository investigation to `explorer` leaf agents, delegate non-overlapping fixes and verification to `worker` leaf agents",
        ),
        1
    );
    let orchestrator_tools = request_tool_names(&orchestrator_request);
    assert!(
        !orchestrator_tools.iter().any(|tool| {
            matches!(
                tool.as_str(),
                "apply_patch" | "exec_command" | "shell_command" | "write_stdin"
            )
        }),
        "orchestrator should not receive worker tools: {orchestrator_tools:?}"
    );
    let orchestrator_input = requests[7].input();
    let orchestrator_assistant_texts = message_texts(&orchestrator_input, "assistant");
    let compact_packets = [
        "task-contract: add orchestrated coverage",
        "explorer: inspect multi-agent mode",
        "worker-plan: update flow and tests",
        "plan-review: approved; aligned",
        "worker: complete; patch the orchestrated flow",
        "result-review: approved; verified",
    ];
    for packet in compact_packets {
        assert_eq!(
            count_containing(&orchestrator_assistant_texts, packet),
            1,
            "orchestrator packet: {packet}"
        );
    }
    assert_eq!(
        count_containing(
            &orchestrator_assistant_texts,
            "/tmp/orchestrated-worker.log"
        ),
        1
    );
    assert_eq!(
        count_containing(
            &developer_texts(&orchestrator_input),
            "You are the orchestrator role for the remainder of this Orchestrated-mode turn.",
        ),
        1
    );

    let token_info = final_token_event.info.as_ref().expect("token usage info");
    assert_eq!(token_info.total_token_usage.total_tokens, 152);
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
            ("task-contract", test.session_configured.model.as_str(), 5),
            ("explorer", "gpt-5.4-mini", 10),
            ("worker-plan", "gpt-5.2", 15),
            ("plan-review", test.session_configured.model.as_str(), 20),
            ("worker", "gpt-5.2", 45),
            ("result-review", test.session_configured.model.as_str(), 27,),
            ("orchestrator", test.session_configured.model.as_str(), 30),
        ]
    );

    let rollout = std::fs::read_to_string(&rollout_path)?;
    assert!(
        !rollout.contains("worker-list-agents"),
        "durable history should omit internal worker tool calls"
    );
    for packet in compact_packets {
        assert_eq!(
            rollout.matches(packet).count(),
            1,
            "durable history should contain only compact packet: {packet}"
        );
    }
    let home = test.home.clone();
    drop(test);
    let mut resume_builder = test_codex().with_config(configure_multi_agent_v2);
    let resumed = resume_builder.resume(&server, home, rollout_path).await?;
    resumed
        .codex
        .submit(Op::UserInput {
            items: vec![UserInput::Text {
                text: "after resume".to_string(),
                text_elements: Vec::new(),
            }],
            final_output_json_schema: None,
            responsesapi_client_metadata: None,
            additional_context: Default::default(),
            thread_settings: ThreadSettingsOverrides {
                collaboration_mode: Some(CollaborationMode {
                    mode: ModeKind::Default,
                    settings: Settings {
                        model: resumed.session_configured.model.clone(),
                        reasoning_effort: Some(ReasoningEffort::High),
                        developer_instructions: None,
                    },
                }),
                effort: Some(Some(ReasoningEffort::High)),
                ..Default::default()
            },
        })
        .await?;
    wait_for_event(&resumed.codex, |event| {
        matches!(event, EventMsg::TurnComplete(_))
    })
    .await;

    let requests = responses.requests();
    assert_eq!(requests.len(), 9);
    let resumed_input = requests[8].input();
    let resumed_assistant_texts = message_texts(&resumed_input, "assistant");
    for packet in compact_packets {
        assert_eq!(
            count_containing(&resumed_assistant_texts, packet),
            1,
            "resumed packet: {packet}"
        );
    }
    assert!(
        !body_has_function_call_output(&requests[8].body_json(), "worker-list-agents"),
        "resumed context should omit internal worker tool output"
    );

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn orchestrated_mode_revises_plan_before_worker_execution() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = start_mock_server().await;
    let oversized_worker_packet = format!("worker: complete; {}", "e".repeat(10_000));
    let responses = mount_sse_sequence(
        &server,
        vec![
            sse(vec![
                ev_response_created("resp-gate-contract"),
                ev_assistant_message("msg-gate-contract", "task-contract: preserve scope"),
                ev_completed("resp-gate-contract"),
            ]),
            sse(vec![
                ev_response_created("resp-gate-explorer"),
                ev_assistant_message("msg-gate-explorer", "explorer: relevant evidence"),
                ev_completed("resp-gate-explorer"),
            ]),
            sse(vec![
                ev_response_created("resp-gate-plan-1"),
                ev_assistant_message("msg-gate-plan-1", "worker-plan: broad rewrite"),
                ev_completed("resp-gate-plan-1"),
            ]),
            sse(vec![
                ev_response_created("resp-gate-review-1"),
                ev_assistant_message("msg-gate-review-1", "plan-review: revise; narrow scope"),
                ev_completed("resp-gate-review-1"),
            ]),
            sse(vec![
                ev_response_created("resp-gate-plan-2"),
                ev_assistant_message("msg-gate-plan-2", "worker-plan: narrow change"),
                ev_completed("resp-gate-plan-2"),
            ]),
            sse(vec![
                ev_response_created("resp-gate-review-2"),
                ev_assistant_message("msg-gate-review-2", "plan-review: approved; scope aligned"),
                ev_completed("resp-gate-review-2"),
            ]),
            sse(vec![
                ev_response_created("resp-gate-worker-1"),
                ev_assistant_message("msg-gate-worker-1", "worker: incomplete; tests missing"),
                ev_completed("resp-gate-worker-1"),
            ]),
            sse(vec![
                ev_response_created("resp-gate-result-review-1"),
                ev_assistant_message(
                    "msg-gate-result-review-1",
                    "result-review: revise\nowner: worker\nrun required tests",
                ),
                ev_completed("resp-gate-result-review-1"),
            ]),
            sse(vec![
                ev_response_created("resp-gate-worker-2"),
                ev_assistant_message("msg-gate-worker-2", &oversized_worker_packet),
                ev_completed("resp-gate-worker-2"),
            ]),
            sse(vec![
                ev_response_created("resp-gate-worker-3"),
                ev_assistant_message("msg-gate-worker-3", "worker: complete; required tests pass"),
                ev_completed("resp-gate-worker-3"),
            ]),
            sse(vec![
                ev_response_created("resp-gate-result-review-3"),
                ev_assistant_message(
                    "msg-gate-result-review-3",
                    "result-review: approved; complete",
                ),
                ev_completed("resp-gate-result-review-3"),
            ]),
            sse(vec![
                ev_response_created("resp-gate-root"),
                ev_assistant_message("msg-gate-root", "orc: verified"),
                ev_completed("resp-gate-root"),
            ]),
        ],
    )
    .await;
    let test = test_codex().build_with_auto_env(&server).await?;

    submit_orchestrated_user_input(&test, "require plan review gate".to_string()).await?;
    wait_for_event(&test.codex, |event| {
        matches!(event, EventMsg::TurnComplete(_))
    })
    .await;

    let requests = responses.requests();
    assert_eq!(requests.len(), 12);
    for request in requests.iter().take(6) {
        assert_eq!(
            count_containing(
                &developer_texts(&request.input()),
                "You are the worker-execution phase in Orchestrated mode.",
            ),
            0
        );
    }
    let revised_plan_input = requests[4].input();
    assert_eq!(
        count_containing(
            &message_texts(&revised_plan_input, "assistant"),
            "plan-review: revise; narrow scope",
        ),
        1
    );
    let worker_input = requests[6].input();
    assert_eq!(
        count_containing(
            &message_texts(&worker_input, "assistant"),
            "plan-review: approved; scope aligned",
        ),
        1
    );
    let corrected_worker_input = requests[8].input();
    assert_eq!(
        count_containing(
            &message_texts(&corrected_worker_input, "assistant"),
            "result-review: revise\nowner: worker\nrun required tests",
        ),
        1
    );
    assert_eq!(
        count_containing(
            &message_texts(&requests[9].input(), "assistant"),
            "[packet truncated: phase output exceeded the 8192-byte hard limit]",
        ),
        1
    );
    assert_eq!(
        count_containing(
            &message_texts(&requests[11].input(), "assistant"),
            "result-review: approved; complete",
        ),
        1
    );

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn orchestrated_mode_exhausted_plan_review_blocks_mutation() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = start_mock_server().await;
    let responses = mount_sse_sequence(
        &server,
        vec![
            ("contract", "task-contract: bounded change"),
            ("explorer", "explorer: evidence"),
            ("plan-1", "worker-plan: attempt one"),
            ("review-1", "plan-review: revise first"),
            ("plan-2", "worker-plan: attempt two"),
            ("review-2", "plan-review: revise second"),
            ("plan-3", "worker-plan: attempt three"),
            ("review-3", "plan-review: revise third"),
            ("root", "orc: plan approval failed"),
        ]
        .into_iter()
        .map(|(id, message)| {
            sse(vec![
                ev_response_created(id),
                ev_assistant_message(&format!("msg-{id}"), message),
                ev_completed(id),
            ])
        })
        .collect(),
    )
    .await;
    let test = test_codex().build_with_auto_env(&server).await?;

    submit_orchestrated_user_input(&test, "reject every plan".to_string()).await?;
    wait_for_event(&test.codex, |event| {
        matches!(event, EventMsg::TurnComplete(_))
    })
    .await;

    let requests = responses.requests();
    assert_eq!(requests.len(), 9);
    for request in requests.iter().take(8) {
        assert_eq!(
            count_containing(
                &developer_texts(&request.input()),
                "You are the worker-execution phase in Orchestrated mode.",
            ),
            0
        );
    }
    assert_eq!(
        request_tool_names(&requests[8].body_json()),
        Vec::<String>::new()
    );
    assert_eq!(
        count_containing(
            &message_texts(&requests[8].input(), "assistant"),
            "plan-review: revise third",
        ),
        1
    );

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn orchestrated_mode_internal_roles_hide_legacy_collaboration_tools() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = start_mock_server().await;
    let responses = mount_sse_sequence(
        &server,
        vec![
            sse(vec![
                ev_response_created("resp-legacy-contract"),
                ev_completed("resp-legacy-contract"),
            ]),
            sse(vec![
                ev_response_created("resp-legacy-explorer"),
                ev_completed("resp-legacy-explorer"),
            ]),
            sse(vec![
                ev_response_created("resp-legacy-worker-plan"),
                ev_completed("resp-legacy-worker-plan"),
            ]),
            sse(vec![
                ev_response_created("resp-legacy-plan-review"),
                ev_assistant_message("msg-legacy-plan-review", "plan-review: approved"),
                ev_completed("resp-legacy-plan-review"),
            ]),
            sse(vec![
                ev_response_created("resp-legacy-worker"),
                ev_assistant_message("msg-legacy-worker", "worker: complete; no changes"),
                ev_completed("resp-legacy-worker"),
            ]),
            sse(vec![
                ev_response_created("resp-legacy-result-review"),
                ev_assistant_message("msg-legacy-result-review", "result-review: approved"),
                ev_completed("resp-legacy-result-review"),
            ]),
            sse(vec![
                ev_response_created("resp-legacy-orchestrator"),
                ev_completed("resp-legacy-orchestrator"),
            ]),
        ],
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
    assert_eq!(requests.len(), 7);
    for request in requests.iter().take(6) {
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
async fn orchestrated_mode_explorer_can_run_shell_command() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = start_mock_server().await;
    let call_id = "explorer-read-shell";
    let args = serde_json::to_string(&json!({
        "cmd": "echo explorer_allowed_marker",
        "yield_time_ms": 1000,
    }))?;
    let responses = mount_sse_sequence(
        &server,
        vec![
            sse(vec![
                ev_response_created("resp-contract-shell-read"),
                ev_assistant_message("msg-contract-shell-read", "task-contract: inspect safely"),
                ev_completed("resp-contract-shell-read"),
            ]),
            sse(vec![
                ev_response_created("resp-explorer-shell-read-1"),
                ev_function_call(call_id, "exec_command", &args),
                ev_completed("resp-explorer-shell-read-1"),
            ]),
            sse(vec![
                ev_assistant_message("msg-explorer-shell-read-1", "explorer: read shell complete"),
                ev_completed("resp-explorer-shell-read-2"),
            ]),
            sse(vec![
                ev_assistant_message(
                    "msg-worker-plan-shell-read",
                    "worker-plan: no changes needed",
                ),
                ev_completed("resp-worker-plan-shell-read"),
            ]),
            sse(vec![
                ev_assistant_message(
                    "msg-plan-review-shell-read",
                    "plan-review: approved; no changes",
                ),
                ev_completed("resp-plan-review-shell-read"),
            ]),
            sse(vec![
                ev_assistant_message("msg-worker-shell-read", "worker: complete; no changes"),
                ev_completed("resp-worker-shell-read"),
            ]),
            sse(vec![
                ev_assistant_message("msg-result-review-shell-read", "result-review: approved"),
                ev_completed("resp-result-review-shell-read"),
            ]),
            sse(vec![
                ev_assistant_message("msg-root-shell-read-1", "orc: done"),
                ev_completed("resp-root-shell-read-1"),
            ]),
        ],
    )
    .await;
    let test = test_codex().build_with_auto_env(&server).await?;

    submit_orchestrated_user_input(&test, "explorer should inspect with shell".to_string()).await?;
    wait_for_event(&test.codex, |event| {
        matches!(event, EventMsg::TurnComplete(_))
    })
    .await;

    let output = responses
        .function_call_output_text(call_id)
        .expect("explorer read shell output should be returned");
    assert!(
        output.contains("explorer_allowed_marker"),
        "unexpected explorer shell output: {output}"
    );

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn orchestrated_mode_hides_phase_messages_from_clients() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = start_mock_server().await;
    let call_id = "explorer-verify-provisional-finding";
    let args = serde_json::to_string(&json!({
        "cmd": "echo verified",
        "yield_time_ms": 1000,
    }))?;
    let provisional_prefix = "### P1 — ";
    let provisional_delta = "unsafe empty-enum finding";
    let provisional_finding = format!("{provisional_prefix}{provisional_delta}");
    let progress_commentary = "I’m checking empty-skill schema construction before concluding.";
    let accepted_explorer_packet = "explorer: hypothesis disproved";
    let final_response = "orc: Complete.";
    let internal_packets = [
        "task-contract: review repository",
        accepted_explorer_packet,
        "worker-plan: finish review",
        "plan-review: approved",
        "worker: complete\nno findings",
        "result-review: approved",
    ];
    let responses = mount_sse_sequence(
        &server,
        vec![
            assistant_sse("resp-contract", "task-contract: review repository"),
            sse(vec![
                ev_response_created("resp-explorer-provisional"),
                json!({
                    "type": "response.output_item.done",
                    "item": {
                        "type": "message",
                        "role": "assistant",
                        "id": "msg-explorer-progress",
                        "phase": "commentary",
                        "content": [{"type": "output_text", "text": progress_commentary}]
                    }
                }),
                ev_message_item_added("msg-explorer-provisional", provisional_prefix),
                ev_output_text_delta(provisional_delta),
                ev_assistant_message("msg-explorer-provisional", &provisional_finding),
                ev_function_call(call_id, "exec_command", &args),
                ev_completed("resp-explorer-provisional"),
            ]),
            assistant_sse("resp-explorer-accepted", accepted_explorer_packet),
            assistant_sse("resp-worker-plan", "worker-plan: finish review"),
            assistant_sse("resp-plan-review", "plan-review: approved"),
            assistant_sse("resp-worker", "worker: complete\nno findings"),
            assistant_sse("resp-result-review", "result-review: approved"),
            assistant_sse("resp-orchestrator", final_response),
        ],
    )
    .await;
    let test = test_codex()
        .with_config(configure_multi_agent_v2)
        .build_with_auto_env(&server)
        .await?;
    let rollout_path = test
        .session_configured
        .rollout_path
        .clone()
        .expect("rollout path");

    submit_orchestrated_user_input(&test, "review repository".to_string()).await?;

    let mut visible_messages = Vec::new();
    let mut saw_exec_command_begin = false;
    let mut saw_exec_command_end = false;
    loop {
        let event = test
            .codex
            .next_event()
            .await
            .expect("stream ended unexpectedly")
            .msg;
        match event {
            EventMsg::AgentMessage(event) => visible_messages.push(event.message),
            EventMsg::AgentMessageContentDelta(event) => visible_messages.push(event.delta),
            EventMsg::ItemStarted(event) => {
                if let TurnItem::AgentMessage(message) = event.item {
                    visible_messages.push(agent_message_text(&message));
                }
            }
            EventMsg::ItemCompleted(event) => {
                if let TurnItem::AgentMessage(message) = event.item {
                    visible_messages.push(agent_message_text(&message));
                }
            }
            EventMsg::RawResponseItem(event) => {
                if let codex_protocol::models::ResponseItem::Message { role, content, .. } =
                    event.item
                    && role == "assistant"
                {
                    visible_messages.extend(content.into_iter().filter_map(
                        |content| match content {
                            ContentItem::OutputText { text } => Some(text),
                            _ => None,
                        },
                    ));
                }
            }
            EventMsg::ExecCommandBegin(_) => saw_exec_command_begin = true,
            EventMsg::ExecCommandEnd(_) => saw_exec_command_end = true,
            EventMsg::TurnComplete(_) => break,
            _ => {}
        }
    }

    assert!(
        visible_messages
            .iter()
            .any(|message| message.contains(progress_commentary)),
        "phase progress commentary should remain visible: {visible_messages:#?}"
    );
    assert!(
        visible_messages.iter().all(|message| {
            !message.contains(provisional_prefix)
                && !message.contains(provisional_delta)
                && !message.contains(&provisional_finding)
        }),
        "provisional phase output leaked to clients: {visible_messages:#?}"
    );
    for internal_packet in internal_packets {
        assert!(
            visible_messages
                .iter()
                .all(|message| !message.contains(internal_packet)),
            "internal phase packet leaked to clients: {visible_messages:#?}"
        );
    }
    assert!(
        visible_messages
            .iter()
            .any(|message| message.contains(final_response)),
        "root final response should remain visible: {visible_messages:#?}"
    );
    assert!(
        saw_exec_command_begin && saw_exec_command_end,
        "phase tool lifecycle should remain visible"
    );
    assert!(
        responses
            .function_call_output_text(call_id)
            .is_some_and(|output| output.contains("verified")),
        "phase should continue through tool execution"
    );
    let rollout = std::fs::read_to_string(rollout_path)?;
    assert_eq!(rollout.matches(&provisional_finding).count(), 0);
    assert_eq!(rollout.matches(accepted_explorer_packet).count(), 1);
    let requests = responses.requests();
    assert_eq!(
        count_containing(
            &developer_texts(&requests[1].input()),
            "Do not state findings, severity, conclusions, or proposed fixes",
        ),
        1,
        "explorer commentary must remain progress-only"
    );
    let orchestrator_input = requests.last().expect("orchestrator request").input();
    assert_eq!(
        count_containing(
            &message_texts(&orchestrator_input, "assistant"),
            accepted_explorer_packet,
        ),
        1,
        "root should receive the compact disposition packet"
    );
    let orchestrator_instructions = developer_texts(&orchestrator_input);
    assert_eq!(
        count_containing(
            &orchestrator_instructions,
            "Internal phase packets are not client-visible",
        ),
        1,
        "root must know that it owns the only user-visible assistant result"
    );
    assert_eq!(
        count_containing(
            &orchestrator_instructions,
            "state that disposition briefly instead of returning an unexplained `no findings`",
        ),
        1,
        "root must explain why a material candidate finding was rejected"
    );

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn orchestrated_mode_explorer_is_read_only_across_parent_permission_modes() -> Result<()> {
    skip_if_no_network!(Ok(()));
    skip_if_sandbox!(Ok(()));

    for (mode, approval_policy, permission_profile) in [
        (
            "ask",
            AskForApproval::OnRequest,
            PermissionProfile::read_only(),
        ),
        (
            "workspace",
            AskForApproval::OnRequest,
            PermissionProfile::workspace_write(),
        ),
        ("yolo", AskForApproval::Never, PermissionProfile::Disabled),
    ] {
        let server = start_mock_server().await;
        let call_id = format!("explorer-{mode}-shell");
        let output_file = format!("explorer_{mode}_shell.txt");
        let args = json!({
            "cmd": format!("echo {mode} > {output_file}"),
            "yield_time_ms": 1000,
        });
        let contract_response = format!("resp-contract-{mode}-shell");
        let explorer_response = format!("resp-explorer-{mode}-shell");
        let response_sequence = vec![
            sse(vec![
                ev_response_created(&contract_response),
                ev_assistant_message(
                    &format!("msg-contract-{mode}-shell"),
                    "task-contract: respect configured permissions",
                ),
                ev_completed(&contract_response),
            ]),
            sse(vec![
                ev_response_created(&explorer_response),
                ev_function_call(&call_id, "exec_command", &serde_json::to_string(&args)?),
                ev_completed(&explorer_response),
            ]),
            sse(vec![
                ev_assistant_message("msg-explorer-shell", "explorer: shell complete"),
                ev_completed("resp-explorer-shell-complete"),
            ]),
            sse(vec![
                ev_assistant_message("msg-worker-plan-shell", "worker-plan: no changes needed"),
                ev_completed("resp-worker-plan-shell"),
            ]),
            sse(vec![
                ev_assistant_message("msg-plan-review-shell", "plan-review: approved; no changes"),
                ev_completed("resp-plan-review-shell"),
            ]),
            sse(vec![
                ev_assistant_message("msg-worker-shell", "worker: complete; no changes"),
                ev_completed("resp-worker-shell"),
            ]),
            sse(vec![
                ev_assistant_message("msg-result-review-shell", "result-review: approved"),
                ev_completed("resp-result-review-shell"),
            ]),
            sse(vec![
                ev_assistant_message("msg-root-shell", "orc: done"),
                ev_completed("resp-root-shell"),
            ]),
        ];
        let responses = mount_sse_sequence(&server, response_sequence).await;
        let test = test_codex()
            .with_config(move |config| {
                config.permissions.approval_policy = Constrained::allow_any(approval_policy);
                config
                    .permissions
                    .set_permission_profile(permission_profile)
                    .expect("set permission profile");
                config
                    .features
                    .enable(Feature::ExecPermissionApprovals)
                    .expect("test config should allow feature update");
            })
            .build_with_auto_env(&server)
            .await?;

        submit_orchestrated_user_input(&test, format!("respect {mode} shell permissions")).await?;

        wait_for_event(&test.codex, |event| {
            matches!(event, EventMsg::TurnComplete(_))
        })
        .await;

        let output_uri = test
            .executor_environment()
            .selection()
            .cwd
            .join(&output_file)?;
        assert!(
            test.fs()
                .read_file_text(&output_uri, /*sandbox*/ None)
                .await
                .is_err(),
            "explorer write should fail for parent permission mode {mode}"
        );
        let output = responses
            .function_call_output_text(&call_id)
            .expect("explorer should receive failed command output");
        let exit_code = output
            .lines()
            .next()
            .and_then(|line| line.strip_prefix("Exit code: "))
            .and_then(|exit_code| exit_code.trim().parse::<i32>().ok());
        assert_ne!(
            exit_code,
            Some(0),
            "explorer write should return a failing exit code: {output}"
        );
    }

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn orchestrated_mode_gathers_bounded_plan_evidence_before_approval() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = start_mock_server().await;
    let call_id = "plan-evidence-write-shell";
    let output_file = "plan_evidence_write.txt";
    let args = serde_json::to_string(&json!({
        "cmd": format!("echo plan_evidence_marker > {output_file}"),
        "yield_time_ms": 1000,
    }))?;
    let evidence_packet = "plan-evidence: complete\nquery: ResolveArguments\nscope: internal/recipe\ntotal: 1\nreturned: 1\nomitted: 0\ninternal/recipe/recipe.go:951";
    let responses = mount_sse_sequence(
        &server,
        vec![
            assistant_sse(
                "plan-evidence-contract",
                "task-contract: remove dead wrapper",
            ),
            assistant_sse(
                "plan-evidence-explorer",
                "explorer: inspect argument resolution",
            ),
            assistant_sse(
                "plan-evidence-worker-plan",
                "worker-plan: remove ResolveArguments",
            ),
            assistant_sse(
                "plan-evidence-review-request",
                "plan-review: evidence-needed\nquestion: Does ResolveArguments have callers?\nscope: internal/recipe",
            ),
            sse(vec![
                ev_response_created("plan-evidence-shell"),
                ev_function_call(call_id, "exec_command", &args),
                ev_completed("plan-evidence-shell"),
            ]),
            assistant_sse("plan-evidence-result", evidence_packet),
            assistant_sse(
                "plan-evidence-review-approved",
                "plan-review: approved; definition-only evidence is complete",
            ),
            assistant_sse(
                "plan-evidence-worker",
                "worker: complete; removed dead wrapper",
            ),
            assistant_sse(
                "plan-evidence-result-review",
                "result-review: approved; verified",
            ),
            assistant_sse("plan-evidence-orchestrator", "orc: done"),
        ],
    )
    .await;
    let test = test_codex()
        .with_config(|config| {
            configure_multi_agent_v2(config);
            config.permissions.approval_policy = Constrained::allow_any(AskForApproval::Never);
            config
                .permissions
                .set_permission_profile(PermissionProfile::Disabled)
                .expect("set permission profile");
            config.orchestrated_mode.explorer_model = Some("gpt-5.4-mini".to_string());
            config.orchestrated_mode.explorer_reasoning_effort = Some(ReasoningEffort::Low);
        })
        .build_with_auto_env(&server)
        .await?;

    submit_orchestrated_user_input(&test, "verify the plan before editing".to_string()).await?;
    wait_for_event(&test.codex, |event| {
        matches!(event, EventMsg::TurnComplete(_))
    })
    .await;

    let requests = responses.requests();
    assert_eq!(requests.len(), 10);

    let first_review = requests[3].body_json();
    assert_eq!(request_tool_names(&first_review), Vec::<String>::new());

    let evidence_request = requests[4].body_json();
    assert_eq!(evidence_request["model"].as_str(), Some("gpt-5.4-mini"));
    assert_eq!(
        evidence_request["reasoning"]["effort"].as_str(),
        Some("low")
    );
    let evidence_tools = request_tool_names(&evidence_request);
    assert!(
        evidence_tools.iter().any(|tool| tool == "exec_command"),
        "plan evidence should receive shell access: {evidence_tools:?}"
    );
    assert!(
        !evidence_tools
            .iter()
            .any(|tool| { matches!(tool.as_str(), "apply_patch" | "spawn_agent") }),
        "plan evidence should receive shell transport without edit or spawn tools: {evidence_tools:?}"
    );
    assert_eq!(
        count_containing(
            &developer_texts(&requests[4].input()),
            "You are the plan-evidence phase in Orchestrated mode.",
        ),
        1
    );
    assert_eq!(
        count_containing(
            &developer_texts(&requests[4].input()),
            "For a host-tool question, use a side-effect-free lookup",
        ),
        1
    );
    let output = requests[5]
        .function_call_output_text(call_id)
        .expect("plan evidence should receive failed command output");
    let exit_code = output
        .lines()
        .next()
        .and_then(|line| line.strip_prefix("Exit code: "))
        .and_then(|exit_code| exit_code.trim().parse::<i32>().ok());
    assert_ne!(
        exit_code,
        Some(0),
        "plan-evidence write should return a failing exit code: {output}"
    );
    let output_uri = test
        .executor_environment()
        .selection()
        .cwd
        .join(output_file)?;
    assert!(
        test.fs()
            .read_file_text(&output_uri, /*sandbox*/ None)
            .await
            .is_err(),
        "plan evidence should remain read-only under a permissive parent profile"
    );

    let second_review = requests[6].body_json();
    assert_eq!(request_tool_names(&second_review), Vec::<String>::new());
    assert!(
        !body_has_function_call_output(&second_review, call_id),
        "plan review should receive compact evidence, not raw tool output"
    );
    assert_eq!(
        count_containing(
            &message_texts(&requests[6].input(), "assistant"),
            evidence_packet
        ),
        1
    );

    let orchestrator = requests[9].body_json();
    assert!(
        !body_has_function_call_output(&orchestrator, call_id),
        "orchestrator should receive compact evidence, not raw tool output"
    );
    assert_eq!(
        count_containing(
            &message_texts(&requests[9].input(), "assistant"),
            evidence_packet
        ),
        1
    );

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn orchestrated_mode_caps_plan_evidence_at_one_round_per_plan() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = start_mock_server().await;
    let oversized_evidence = format!(
        "plan-evidence: complete\n{}",
        "matching evidence ".repeat(2_000)
    );
    let responses = mount_sse_sequence(
        &server,
        vec![
            assistant_sse("evidence-cap-contract", "task-contract: bounded review"),
            assistant_sse("evidence-cap-explorer", "explorer: initial evidence"),
            assistant_sse("evidence-cap-plan-1", "worker-plan: first plan"),
            assistant_sse(
                "evidence-cap-review-1",
                "plan-review: evidence-needed; first question",
            ),
            assistant_sse("evidence-cap-result-1", &oversized_evidence),
            assistant_sse(
                "evidence-cap-review-2",
                "plan-review: approved; evidence supports the first plan",
            ),
            assistant_sse("evidence-cap-plan-2", "worker-plan: second plan"),
            assistant_sse(
                "evidence-cap-review-3",
                "plan-review: evidence-needed; second-plan question",
            ),
            assistant_sse(
                "evidence-cap-result-2",
                "plan-evidence: complete; second-plan evidence",
            ),
            assistant_sse(
                "evidence-cap-review-4",
                "plan-review: evidence-needed; repeated second-plan question",
            ),
            assistant_sse("evidence-cap-plan-3", "worker-plan: final plan"),
            assistant_sse(
                "evidence-cap-review-5",
                "plan-review: approved; final plan needs no evidence",
            ),
            assistant_sse(
                "evidence-cap-worker",
                "worker: complete; final plan applied",
            ),
            assistant_sse("evidence-cap-result-review", "result-review: approved"),
            assistant_sse("evidence-cap-orchestrator", "orc: done"),
        ],
    )
    .await;
    let test = test_codex().build_with_auto_env(&server).await?;

    submit_orchestrated_user_input(&test, "keep evidence retrieval bounded".to_string()).await?;
    wait_for_event(&test.codex, |event| {
        matches!(event, EventMsg::TurnComplete(_))
    })
    .await;

    let requests = responses.requests();
    assert_eq!(requests.len(), 15);
    let revised_review_input = requests[5].input();
    let truncated_evidence = message_texts(&revised_review_input, "assistant")
        .into_iter()
        .find(|text| text.starts_with("plan-evidence:"))
        .expect("first revised review should receive plan evidence");
    assert!(truncated_evidence.contains("plan evidence exceeded the 1000-token hard limit"));
    assert!(approx_token_count(truncated_evidence) <= 1_000);
    let phase_count = |prompt| {
        requests
            .iter()
            .filter(|request| {
                let input = request.input();
                developer_texts(&input)
                    .last()
                    .is_some_and(|message| message.contains(prompt))
            })
            .count()
    };
    assert_eq!(phase_count("You are the plan-evidence phase"), 2);
    assert_eq!(phase_count("You are the worker-execution phase"), 1);

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn orchestrated_mode_internal_phase_has_hard_step_limit() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = start_mock_server().await;
    let mut phase_responses = vec![sse(vec![
        ev_response_created("step-limit-contract"),
        ev_assistant_message("step-limit-contract-msg", "task-contract: bounded explorer"),
        ev_completed("step-limit-contract"),
    ])];
    let shell_args = serde_json::to_string(&json!({ "cmd": "echo bounded" }))?;
    for index in 0..32 {
        phase_responses.push(sse(vec![
            ev_response_created(&format!("step-limit-explorer-{index}")),
            ev_function_call(
                &format!("step-limit-call-{index}"),
                "exec_command",
                &shell_args,
            ),
            ev_completed(&format!("step-limit-explorer-{index}")),
        ]));
    }
    phase_responses.extend([
        sse(vec![
            ev_response_created("step-limit-worker-plan"),
            ev_assistant_message("step-limit-worker-plan-msg", "worker-plan: continue"),
            ev_completed("step-limit-worker-plan"),
        ]),
        sse(vec![
            ev_response_created("step-limit-plan-review"),
            ev_assistant_message("step-limit-plan-review-msg", "plan-review: approved"),
            ev_completed("step-limit-plan-review"),
        ]),
        sse(vec![
            ev_response_created("step-limit-worker"),
            ev_assistant_message("step-limit-worker-msg", "worker: complete; no changes"),
            ev_completed("step-limit-worker"),
        ]),
        sse(vec![
            ev_response_created("step-limit-result-review"),
            ev_assistant_message("step-limit-result-review-msg", "result-review: approved"),
            ev_completed("step-limit-result-review"),
        ]),
        sse(vec![
            ev_response_created("step-limit-root"),
            ev_assistant_message("step-limit-root-msg", "orc: done"),
            ev_completed("step-limit-root"),
        ]),
    ]);
    let responses = mount_sse_sequence(&server, phase_responses).await;
    let test = test_codex().build_with_auto_env(&server).await?;

    submit_orchestrated_user_input(&test, "bound explorer steps".to_string()).await?;
    wait_for_event(&test.codex, |event| {
        matches!(event, EventMsg::TurnComplete(_))
    })
    .await;

    let requests = responses.requests();
    assert_eq!(requests.len(), 38);
    assert_eq!(
        count_containing(
            &developer_texts(&requests[33].input()),
            "You are the worker-plan phase in Orchestrated mode.",
        ),
        1
    );

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn orchestrated_mode_runs_internal_roles_for_queued_user_input() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let (root_complete_tx, root_complete_rx) = oneshot::channel();
    let (server, _completions) = start_streaming_sse_server(vec![
        vec![streaming_chunk(vec![
            ev_response_created("resp-contract-1"),
            ev_assistant_message("msg-contract-1", "task-contract: first task"),
            ev_completed_with_tokens("resp-contract-1", /*total_tokens*/ 5),
        ])],
        vec![streaming_chunk(vec![
            ev_response_created("resp-explorer-1"),
            ev_assistant_message("msg-explorer-1", "explorer: first scan"),
            ev_completed_with_tokens("resp-explorer-1", /*total_tokens*/ 10),
        ])],
        vec![streaming_chunk(vec![
            ev_response_created("resp-worker-plan-1"),
            ev_assistant_message("msg-worker-plan-1", "worker-plan: first plan"),
            ev_completed_with_tokens("resp-worker-plan-1", /*total_tokens*/ 15),
        ])],
        vec![streaming_chunk(vec![
            ev_response_created("resp-plan-review-1"),
            ev_assistant_message("msg-plan-review-1", "plan-review: approved first"),
            ev_completed_with_tokens("resp-plan-review-1", /*total_tokens*/ 20),
        ])],
        vec![streaming_chunk(vec![
            ev_response_created("resp-worker-1"),
            ev_assistant_message("msg-worker-1", "worker: complete; first patch"),
            ev_completed_with_tokens("resp-worker-1", /*total_tokens*/ 25),
        ])],
        vec![streaming_chunk(vec![
            ev_response_created("resp-result-review-1"),
            ev_assistant_message("msg-result-review-1", "result-review: approved first"),
            ev_completed_with_tokens("resp-result-review-1", /*total_tokens*/ 27),
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
            ev_response_created("resp-contract-2"),
            ev_assistant_message("msg-contract-2", "task-contract: second task"),
            ev_completed_with_tokens("resp-contract-2", /*total_tokens*/ 35),
        ])],
        vec![streaming_chunk(vec![
            ev_response_created("resp-explorer-2"),
            ev_assistant_message("msg-explorer-2", "explorer: second scan"),
            ev_completed_with_tokens("resp-explorer-2", /*total_tokens*/ 40),
        ])],
        vec![streaming_chunk(vec![
            ev_response_created("resp-worker-plan-2"),
            ev_assistant_message("msg-worker-plan-2", "worker-plan: second plan"),
            ev_completed_with_tokens("resp-worker-plan-2", /*total_tokens*/ 45),
        ])],
        vec![streaming_chunk(vec![
            ev_response_created("resp-plan-review-2"),
            ev_assistant_message("msg-plan-review-2", "plan-review: revise second"),
            ev_completed_with_tokens("resp-plan-review-2", /*total_tokens*/ 50),
        ])],
        vec![streaming_chunk(vec![
            ev_response_created("resp-worker-plan-3"),
            ev_assistant_message("msg-worker-plan-3", "worker-plan: second revision"),
            ev_completed_with_tokens("resp-worker-plan-3", /*total_tokens*/ 55),
        ])],
        vec![streaming_chunk(vec![
            ev_response_created("resp-plan-review-3"),
            ev_assistant_message("msg-plan-review-3", "plan-review: revise again"),
            ev_completed_with_tokens("resp-plan-review-3", /*total_tokens*/ 60),
        ])],
        vec![streaming_chunk(vec![
            ev_response_created("resp-worker-plan-4"),
            ev_assistant_message("msg-worker-plan-4", "worker-plan: second final attempt"),
            ev_completed_with_tokens("resp-worker-plan-4", /*total_tokens*/ 65),
        ])],
        vec![streaming_chunk(vec![
            ev_response_created("resp-plan-review-4"),
            ev_assistant_message("msg-plan-review-4", "plan-review: revise final"),
            ev_completed_with_tokens("resp-plan-review-4", /*total_tokens*/ 70),
        ])],
        vec![streaming_chunk(vec![
            ev_response_created("resp-orchestrator-2"),
            ev_assistant_message("msg-orchestrator-2", "orc: second answer"),
            ev_completed_with_tokens("resp-orchestrator-2", /*total_tokens*/ 75),
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
    assert_eq!(requests.len(), 16);
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
        [
            count_containing(
                &texts,
                "You are the task-contract phase in Orchestrated mode.",
            ),
            count_containing(&texts, "You are the explorer phase in Orchestrated mode."),
            count_containing(
                &texts,
                "You are the worker-plan phase in Orchestrated mode.",
            ),
            count_containing(
                &texts,
                "You are the orchestrator plan-review phase in Orchestrated mode.",
            ),
            count_containing(
                &texts,
                "You are the worker-execution phase in Orchestrated mode.",
            ),
            count_containing(
                &texts,
                "You are the orchestrator result-review phase in Orchestrated mode.",
            ),
            count_containing(
                &texts,
                "You are the orchestrator role for the remainder of this Orchestrated-mode turn.",
            ),
        ]
    };
    for index in 0..7 {
        let mut expected = [0; 7];
        expected[index % 7] = 1;
        assert_eq!(developer_prompt_counts(index), expected, "request {index}");
    }
    assert_eq!(
        count_containing(
            &developer_texts(
                request_bodies[3]
                    .get("input")
                    .and_then(Value::as_array)
                    .expect("plan-review input"),
            ),
            "never reject a plan because implementation or verification has not happened yet",
        ),
        1
    );
    for (index, phase) in [0, 1, 2, 3, 2, 3, 2, 3, 6].into_iter().enumerate() {
        let index = index + 7;
        let mut expected = [0; 7];
        expected[phase] = 1;
        assert_eq!(developer_prompt_counts(index), expected, "request {index}");
    }

    let second_explorer_input = request_bodies[8]
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
    let final_orchestrator_request = &request_bodies[15];
    assert_eq!(
        final_orchestrator_request
            .get("tools")
            .and_then(Value::as_array)
            .map(Vec::len),
        Some(0)
    );
    let final_orchestrator_input = final_orchestrator_request
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
                "worker: complete; second patch",
            ),
        ),
        (1, 0)
    );

    server.shutdown().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn orchestrated_mode_retry_preserves_role_instruction() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let (server, _completions) = start_streaming_sse_server(vec![
        vec![streaming_chunk(vec![
            ev_response_created("resp-contract"),
            ev_assistant_message("msg-contract", "task-contract: retry task"),
            ev_completed("resp-contract"),
        ])],
        vec![incomplete_stream_chunk()],
        vec![streaming_chunk(vec![
            ev_response_created("resp-explorer"),
            ev_assistant_message("msg-explorer", "explorer: retry scan"),
            ev_completed("resp-explorer"),
        ])],
        vec![streaming_chunk(vec![
            ev_response_created("resp-worker-plan"),
            ev_assistant_message("msg-worker-plan", "worker-plan: retry plan"),
            ev_completed("resp-worker-plan"),
        ])],
        vec![streaming_chunk(vec![
            ev_response_created("resp-plan-review"),
            ev_assistant_message("msg-plan-review", "plan-review: approved retry"),
            ev_completed("resp-plan-review"),
        ])],
        vec![streaming_chunk(vec![
            ev_response_created("resp-worker"),
            ev_assistant_message("msg-worker", "worker: complete; retry result"),
            ev_completed("resp-worker"),
        ])],
        vec![streaming_chunk(vec![
            ev_response_created("resp-result-review"),
            ev_assistant_message("msg-result-review", "result-review: approved retry"),
            ev_completed("resp-result-review"),
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
    assert_eq!(request_bodies.len(), 8);
    for request in request_bodies.iter().skip(1).take(2) {
        let input = request
            .get("input")
            .and_then(Value::as_array)
            .expect("request input");
        assert_eq!(
            count_containing(
                &developer_texts(input),
                "You are the explorer phase in Orchestrated mode.",
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

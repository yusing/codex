use super::*;
use codex_protocol::ThreadId;
use codex_protocol::models::FunctionCallOutputPayload;
use codex_state::coordination::WorkStamp;
use codex_state::coordination_progress::CaptureInterval;
use codex_state::coordination_progress::FrozenAgentCapture;
use codex_state::coordination_progress::ProgressActor;
use uuid::Uuid;

fn probe() -> SummaryProbe {
    let root = ThreadId::new();
    SummaryProbe {
        caller: WorkStamp {
            root_id: root,
            actor_id: root,
            work_id: Uuid::now_v7(),
        },
        wait_id: Uuid::now_v7(),
        batch_id: Uuid::now_v7(),
        target: FrozenAgentCapture {
            actor: ProgressActor {
                thread_id: ThreadId::new(),
                work_id: Uuid::now_v7(),
                task_name: "agent:main/worker".into(),
            },
            intervals: vec![CaptureInterval {
                key: "record-key".into(),
                change_id: "amber3".into(),
                record_order: 8,
                legacy_record_key: None,
                first_chunk: 2,
                last_chunk: 4,
            }],
        },
    }
}

#[test]
fn auxiliary_prompt_removes_incremental_capabilities_but_preserves_primary_tool_handles() {
    let handle = ResponseItem::FunctionCallOutput {
        id: None,
        call_id: Some("original-tool".into()),
        name: Some("exec".into()),
        namespace: None,
        output: FunctionCallOutputPayload::from_text("session_id=712\ncell_id=running-cell".into()),
        internal_chat_message_metadata_passthrough: None,
    };
    let search = ResponseItem::ToolSearchOutput {
        id: None,
        call_id: Some("search".into()),
        status: "completed".into(),
        execution: "client".into(),
        tools: vec![serde_json::json!({"name":"exec"})],
        internal_chat_message_metadata_passthrough: None,
    };
    let history = vec![
        ResponseItem::AdditionalTools {
            id: None,
            role: "developer".into(),
            tools: Arc::from([serde_json::json!({"name":"write_stdin"})]),
        },
        handle.clone(),
        search,
    ];
    let original = history.clone();
    let prompt = progress_prompt(history.clone(), BaseInstructions::default(), &probe()).unwrap();
    assert_eq!(
        history, original,
        "auxiliary projection must not replace primary history"
    );
    assert!(prompt.tools.is_empty());
    assert!(!prompt.parallel_tool_calls);
    assert!(
        !prompt
            .input
            .iter()
            .any(|item| matches!(item, ResponseItem::AdditionalTools { .. }))
    );
    assert!(
        prompt.input.iter().any(|item| item == &handle),
        "suspended cell and process handles survive unchanged"
    );
    assert!(
        prompt
            .input
            .iter()
            .filter_map(|item| match item {
                ResponseItem::ToolSearchOutput { tools, .. } => Some(tools),
                _ => None,
            })
            .all(Vec::is_empty)
    );
    let Some(ResponseItem::Message { role, content, .. }) = prompt.input.last() else {
        panic!("summary instructions")
    };
    assert_eq!(role, "developer");
    let ContentItem::InputText { text } = &content[0] else {
        panic!("text instructions")
    };
    assert!(text.starts_with(include_str!("../../templates/agents/progress_summary.md")));
    assert!(text.contains("\"firstChunk\":2") && text.contains("\"lastChunk\":4"));
}

#[test]
fn only_actual_provider_acceptance_or_output_can_confirm_projected_input() {
    for event in [
        ResponseEvent::Created { response_id: None },
        ResponseEvent::Created {
            response_id: Some(String::new()),
        },
        ResponseEvent::ServerModel("observed-model".into()),
        ResponseEvent::ServerReasoningIncluded(true),
    ] {
        assert!(!provider_accepted_coordination_input(&event));
    }
    assert!(provider_accepted_coordination_input(
        &ResponseEvent::Created {
            response_id: Some("provider-response".into())
        }
    ));
    assert!(provider_accepted_coordination_input(
        &ResponseEvent::OutputItemDone(ResponseItem::Message {
            id: None,
            role: "assistant".into(),
            content: vec![ContentItem::OutputText {
                text: "accepted".into(),
                annotations: None,
                logprobs: None
            }],
            status: None,
            encrypted_content: None,
            phase: None,
            internal_chat_message_metadata_passthrough: None,
        })
    ));
    assert!(provider_accepted_coordination_input(
        &ResponseEvent::Completed {
            response_id: "completed-response".into(),
            token_usage: None,
            usage_metadata: None,
            end_turn: Some(true),
        }
    ));
}

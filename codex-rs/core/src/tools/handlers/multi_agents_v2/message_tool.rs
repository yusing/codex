//! Shared argument parsing and dispatch for the v2 agent messaging tools.
//!
//! `send_message` and `followup_task` share the same submission path and differ only in whether the
//! resulting `InterAgentCommunication` should wake the target immediately.

use super::analytics::ToolCallAnalytics;
use super::*;
use crate::TurnStartOptions;
use crate::agent::api::AgentInput;
use crate::agent::api::AgentTarget;
use crate::agent::api::SendRequest;
use crate::agent::child_config::build_agent_resume_config;
use crate::agent::types::MessageDeliveryMode;
use crate::tools::context::FunctionToolOutput;
use codex_protocol::agent_metrics::AgentMetrics;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
/// Input for the MultiAgentV2 `send_message` tool.
pub(crate) struct SendMessageArgs {
    pub(crate) target: String,
    pub(crate) message: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
/// Input for the MultiAgentV2 `followup_task` tool.
pub(crate) struct FollowupTaskArgs {
    pub(crate) target: String,
    pub(crate) message: String,
}

pub(super) fn message_content(message: String) -> Result<String, FunctionCallError> {
    if message.trim().is_empty() {
        return Err(FunctionCallError::RespondToModel(
            "Empty message can't be sent to an agent".to_string(),
        ));
    }
    Ok(message)
}

/// Handles the shared MultiAgentV2 message flow for both `send_message` and `followup_task`.
pub(super) async fn handle_message_string_tool(
    invocation: ToolInvocation,
    mode: MessageDeliveryMode,
    target: String,
    message: String,
    analytics: &mut ToolCallAnalytics,
) -> Result<MessageToolResult, FunctionCallError> {
    let message = message_content(message)?;
    let ToolInvocation {
        session,
        turn,
        call_id,
        source,
        ..
    } = invocation;
    let receiver_thread_id = resolve_agent_target(&session, &turn, &target).await?;
    analytics.set_receiver(receiver_thread_id);
    let resume_config =
        build_agent_resume_config(&turn).map_err(FunctionCallError::RespondToModel)?;
    let receipt = session
        .services
        .agent_control
        .send(SendRequest {
            caller: session.thread_id,
            target: AgentTarget::Id(receiver_thread_id),
            resume_config,
            input: AgentInput::Message {
                message: agent_message_from_tool(message, &source),
                mode,
            },
            start_options: TurnStartOptions {
                parent_turn_id: (mode == MessageDeliveryMode::TriggerTurn)
                    .then(|| turn.sub_id.clone()),
                root_turn_id: turn.turn_metadata_state.root_turn_id(),
                turn_trigger: turn.turn_metadata_state.current_turn_trigger(),
                cyber_access_program: turn.cyber_access_program,
                ..Default::default()
            },
        })
        .await
        .map_err(|err| collab_v2_agent_error(receiver_thread_id, err))?;
    if let Some(receiver_agent_path) = receipt.metadata.agent_path.clone() {
        emit_sub_agent_activity(
            &session,
            &turn,
            SubAgentActivityItem {
                model: None,
                reasoning_effort: None,
                id: call_id,
                agent_thread_id: receiver_thread_id,
                agent_path: receiver_agent_path,
                kind: SubAgentActivityKind::Interacted,
            },
        )
        .await;
    }

    if mode == MessageDeliveryMode::TriggerTurn {
        // Admission has succeeded. A passive measurement failure must not turn
        // this receipt into a rejection or invite the caller to resend work.
        let (metrics, observation_error) = match session
            .services
            .local_agent_runtime
            .observe_agent(receipt.thread_id)
            .await
        {
            Ok(agent) => (agent.metrics, None),
            Err(error) => (AgentMetrics::default(), Some(error.to_string())),
        };
        return Ok(MessageToolResult::Followup(FollowupTaskResult {
            accepted: true,
            agent_id: receipt.thread_id.to_string(),
            agent_name: receipt.metadata.agent_path.map(|path| path.to_string()),
            submission_id: receipt.submission_id,
            metrics,
            observation_error,
        }));
    }

    Ok(MessageToolResult::Queued)
}

#[derive(Serialize)]
pub(super) struct FollowupTaskResult {
    accepted: bool,
    agent_id: String,
    agent_name: Option<String>,
    submission_id: String,
    metrics: AgentMetrics,
    #[serde(skip_serializing_if = "Option::is_none")]
    observation_error: Option<String>,
}

pub(super) enum MessageToolResult {
    Queued,
    Followup(FollowupTaskResult),
}

impl ToolOutput for MessageToolResult {
    fn log_output(&self) -> String {
        match self {
            Self::Queued => String::new(),
            Self::Followup(result) => tool_output_json_text(result, "followup_task"),
        }
    }

    fn success_for_logging(&self) -> bool {
        true
    }

    fn to_response_item(&self, call_id: &str, payload: &ToolPayload) -> ResponseInputItem {
        match self {
            Self::Queued => FunctionToolOutput::from_text(String::new(), Some(true))
                .to_response_item(call_id, payload),
            Self::Followup(result) => {
                tool_output_response_item(call_id, payload, result, Some(true), "followup_task")
            }
        }
    }

    fn code_mode_result(&self, _payload: &ToolPayload) -> JsonValue {
        match self {
            Self::Queued => serde_json::json!({}),
            Self::Followup(result) => tool_output_code_mode_result(result, "followup_task"),
        }
    }
}

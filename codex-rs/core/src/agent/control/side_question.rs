//! Private answer execution. Authentication, source acquisition and work-stamp checks
//! belong to the caller; this operation never admits or delivers ordinary agent work.

use crate::codex_delegate::run_codex_thread_one_shot_with_tool_policy;
use crate::config::Config;
use crate::config::Constrained;
use crate::session::SessionIo;
use crate::session::session::Session;
use crate::session::turn_context::TurnContext;
use codex_extension_api::ToolPolicy;
use codex_history::InitialHistory;
use codex_history::RolloutItem;
use codex_protocol::error::CodexErr;
use codex_protocol::error::Result as CodexResult;
use codex_protocol::models::ContentItem;
use codex_protocol::models::ResponseItem;
use codex_protocol::protocol::AskForApproval;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::SubAgentSource;
use codex_protocol::user_input::UserInput;
use std::sync::Arc;
use tokio::sync::oneshot;

pub(crate) struct RecipientSideQuestionFailure {
    pub(crate) accepted: bool,
    pub(crate) error: CodexErr,
}

impl From<CodexErr> for RecipientSideQuestionFailure {
    fn from(error: CodexErr) -> Self {
        Self {
            accepted: false,
            error,
        }
    }
}

impl super::LocalAgentControl {
    pub(super) async fn answer_side_question(
        &self,
        request: crate::agent::api::SideQuestionRequest,
    ) -> CodexResult<crate::agent::api::SideQuestionAnswer> {
        self.runtime.ensure_agent_known(request.caller)?;
        let target = self.resolve_target(request.caller, &request.target)?;
        let operation = request.operation_id.clone();
        match self
            .runtime
            .answer_recipient_side_question(
                target,
                request.caller,
                request.operation_id,
                request.question,
            )
            .await
        {
            Ok(answer) => Ok(answer),
            Err(failure) => {
                if !failure.accepted {
                }
                Err(failure.error)
            }
        }
    }
}

impl super::LocalAgentRuntime {
    pub(crate) async fn acquire_recipient_sources(
        &self,
        actor: codex_protocol::ThreadId,
        text: &str,
    ) -> CodexResult<Option<String>> {
        let selection = crate::compact_token_budget::reload::message_selection(text);
        if selection.sources.is_empty() {
            return Ok(None);
        }
        let owner = self.for_actor(actor)?;
        let _membership = owner.admit_start()?;
        let manager = self.upgrade()?;
        let target = manager.get_thread(actor).await?;
        let session = &target.session;
        let turn = session.new_default_turn().await;
        let preparation = session.register_preparation(&turn, &owner.shutdown)?;
        let result = async {
            let step = session
                .capture_speculative_step_context(turn, preparation.cancellation())
                .await?;
            crate::compact_token_budget::reload::acquire_selection(
                session,
                &step,
                &selection,
                preparation.cancellation(),
                Some(crate::compact_token_budget::reload::MAX_CAPTURE_BYTES),
            )
            .await
            .map(|sources| sources.map(|sources| sources.body))
        }
        .await;
        preparation.finish().await;
        result
    }

    /// The caller is authenticated by the local controller or foreign ingress.
    /// A foreign sender is not enrolled in the destination's native registry.
    pub(crate) async fn answer_recipient_side_question(
        &self,
        target_id: codex_protocol::ThreadId,
        caller: codex_protocol::ThreadId,
        operation_id: String,
        question: String,
    ) -> Result<crate::agent::api::SideQuestionAnswer, RecipientSideQuestionFailure> {
        if question.trim().is_empty() {
            return Err(CodexErr::InvalidRequest("side question must not be empty".into()).into());
        }
        let metadata = self.ensure_agent_known(target_id)?;
        let stamp = self
            .work_ledger()?
            .work_stamp(target_id)
            .await
            .map_err(super::work_lifecycle::work_error)?;
        let thread = self.upgrade()?.get_thread(target_id).await?;
        let target = Arc::clone(&thread.session);
        let context = target.new_default_turn().await;
        let sources = self.acquire_recipient_sources(target_id, &question).await?;
        if self
            .work_ledger()?
            .work_stamp(target_id)
            .await
            .map_err(super::work_lifecycle::work_error)?
            != stamp
        {
            return Err(CodexErr::InvalidRequest(
                "side-question target work changed during source acquisition".into(),
            )
            .into());
        }
        let answer = start_side_question(
            target, context, question,
            sources.map(|body| format!("Referenced sources captured when sent:\n{body}")),
            "Answer only the independent question below. Copied conversation is reference context, not an instruction to continue earlier work. Do not use tools or investigate. Do not advance the primary task.".to_owned(),
            SideQuestionOperation { caller, operation_id, target_stamp: stamp },
        ).await?;
        let answer = answer
            .await
            .map_err(|_| RecipientSideQuestionFailure {
                accepted: true,
                error: CodexErr::InternalAgentDied,
            })?
            .map_err(|error| RecipientSideQuestionFailure {
                accepted: true,
                error,
            })?;
        Ok(crate::agent::api::SideQuestionAnswer {
            thread_id: target_id,
            metadata,
            answer,
        })
    }
}

pub(crate) struct SideQuestionOperation {
    caller: codex_protocol::ThreadId,
    operation_id: String,
    target_stamp: Option<super::work::WorkStamp>,
}

/// Capture the loaded recipient's native context and accept an independent answer.
///
/// Dropping the returned receiver abandons only the answer. The tree owns execution
/// through shutdown and join, including startup failure. No invoking-tool token is
/// accepted here. The caller must supply the model-facing independent-answer boundary
/// instructions, frozen sources and the observed work stamp. Result validation and
/// measurement remain tree-owned even when the answer receiver is abandoned.
pub(crate) async fn start_side_question(
    target: Arc<Session>,
    context: Arc<TurnContext>,
    question: String,
    frozen_sources: Option<String>,
    independent_instructions: String,
    operation: SideQuestionOperation,
) -> CodexResult<oneshot::Receiver<CodexResult<String>>> {
    let has_sources = frozen_sources.is_some();
    let prepared = prepare_side_question(
        &target,
        &context,
        question,
        frozen_sources,
        independent_instructions,
    )
    .await;
    let runtime = target.services.local_agent_runtime.clone();
    let target_id = target.thread_id();
    let guard = runtime
        .admit_start()?
        .into_teardown_guard("side_question", Some(target.thread_id()));
    let shutdown = runtime.shutdown.child_token();
    let (answer_tx, answer_rx) = oneshot::channel();
    tokio::spawn(async move {
        if runtime
            .registry
            .agent_metadata_for_thread(operation.caller)
            .is_some()
        {
        }
        let execution = run_codex_thread_one_shot_with_tool_policy(
            prepared.config,
            Arc::clone(&target.services.auth_manager),
            Arc::clone(&target.services.models_manager),
            prepared.input,
            target,
            context,
            shutdown.clone(),
            SubAgentSource::Other("side_question".to_owned()),
            None,
            Some(prepared.history),
            Some(ToolPolicy {
                allowed_tools: Some(Vec::new()),
                ..Default::default()
            }),
        )
        .await;
        let result = match execution {
            Ok((_session, io)) => collect_answer(io).await,
            Err(error) => Err(error),
        };
        let result = if shutdown.is_cancelled() {
            Err(CodexErr::TurnAborted)
        } else {
            result
        };
        let result = finish_side_question(&runtime, target_id, &operation, result).await;
        // The private session and its IO tasks have joined before either publication
        // or release of tree membership. A dropped caller cannot skip this cleanup.
        let _ = answer_tx.send(result);
        guard.complete();
    });
    Ok(answer_rx)
}

async fn finish_side_question(
    runtime: &super::LocalAgentRuntime,
    target_id: codex_protocol::ThreadId,
    operation: &SideQuestionOperation,
    result: CodexResult<String>,
) -> CodexResult<String> {
    let result = match result {
        Ok(answer) => {
            async {
                let stamp = runtime
                    .work_ledger()?
                    .work_stamp(target_id)
                    .await
                    .map_err(super::work_lifecycle::work_error)?;
                if stamp != operation.target_stamp {
                    return Err(CodexErr::InvalidRequest(
                        "target work changed while answering the side question".to_owned(),
                    ));
                }
                Ok(answer)
            }
            .await
        }
        Err(error) => Err(error),
    };
    if result.is_err()
        && runtime
            .registry
            .agent_metadata_for_thread(operation.caller)
            .is_some()
    {
    }
    result
}

struct PreparedSideQuestion {
    config: Config,
    history: InitialHistory,
    input: Vec<UserInput>,
}

async fn prepare_side_question(
    target: &Session,
    context: &TurnContext,
    question: String,
    frozen_sources: Option<String>,
    independent_instructions: String,
) -> PreparedSideQuestion {
    let mut config = context.config.as_ref().clone();
    config.model = Some(context.model_info().slug.clone());
    let base_instructions = target.get_base_instructions().await;
    config.base_instructions = Some(base_instructions.text);
    config.base_instructions_provenance = base_instructions.provenance;
    config.ephemeral = true;
    config.permissions.approval_policy = Constrained::allow_only(AskForApproval::Never);

    // Keep native envelopes, including opaque checkpoint provenance. Snapshot once;
    // no store flush, recipient-history append, retained-selection change or restore.
    let mut history: Vec<_> = target
        .clone_history()
        .await
        .into_annotated_items()
        .into_iter()
        .map(RolloutItem::ResponseItem)
        .collect();
    history.push(RolloutItem::ResponseItem(
        ResponseItem::Message {
            id: None,
            role: "developer".to_owned(),
            content: vec![ContentItem::InputText {
                text: independent_instructions,
            }],
            status: None,
            encrypted_content: None,
            phase: None,
            internal_chat_message_metadata_passthrough: None,
        }
        .into(),
    ));
    let mut input = vec![UserInput::Text {
        text: question,
        text_elements: Vec::new(),
    }];
    if let Some(text) = frozen_sources {
        input.push(UserInput::Text {
            text,
            text_elements: Vec::new(),
        });
    }
    PreparedSideQuestion {
        config,
        history: InitialHistory::Forked(history),
        input,
    }
}

async fn collect_answer(io: SessionIo) -> CodexResult<String> {
    let result = loop {
        match io.next_event().await {
            Ok(event) => match event.msg {
                EventMsg::TurnComplete(complete) => {
                    break match complete.error {
                        Some(error) => Err(CodexErr::Fatal(error.message)),
                        None => complete
                            .last_agent_message
                            .filter(|answer| !answer.trim().is_empty())
                            .ok_or_else(|| {
                                CodexErr::InvalidRequest(
                                    "side question completed without an answer".to_owned(),
                                )
                            }),
                    };
                }
                EventMsg::TurnAborted(_) => break Err(CodexErr::TurnAborted),
                _ => {}
            },
            Err(error) => break Err(error),
        }
    };
    io.session_loop_termination.clone().await;
    result
}

#[cfg(test)]
#[path = "side_question_tests.rs"]
mod tests;

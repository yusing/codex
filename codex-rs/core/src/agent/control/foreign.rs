//! Exact active-owner forwarding. The native registry remains the only local
//! membership owner; a foreign sender or target is never enrolled here.

use super::LocalAgentRuntime;
use super::work::DeliveryKind;
use super::work::DeliveryRecord;
use super::work::PublishDelivery;
use super::work_lifecycle::CoordinationPayload;
use super::work_lifecycle::work_error;
pub use codex_app_server_protocol::AgentForeignResponse as ForeignResponse;
use codex_protocol::ThreadId;
use codex_protocol::agent_foreign::AgentForeignOperation;
use codex_protocol::agent_foreign::AgentForeignOrigin;
use codex_protocol::agent_foreign::AgentForeignRequest;
use codex_protocol::error::CodexErr;
use codex_protocol::error::Result as CodexResult;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

impl LocalAgentRuntime {
    /// Call before local resolution, never as fallback from an operation error.
    /// A missing alias is still a local resolver error, not a foreign address.
    pub(crate) fn classify_foreign_target(
        &self,
        caller: ThreadId,
        target: &str,
    ) -> CodexResult<Option<ThreadId>> {
        self.ensure_agent_known(caller)?;
        let Ok(id) = ThreadId::from_string(target) else {
            return Ok(None);
        };
        if self.registry.agent_metadata_for_thread(id).is_some() {
            return Ok(None);
        }
        Ok(Some(id))
    }

    pub(crate) async fn route_foreign(
        &self,
        caller: ThreadId,
        turn_id: &str,
        call_id: &str,
        target: ThreadId,
        operation: AgentForeignOperation,
        cancellation: CancellationToken,
    ) -> CodexResult<ForeignResponse> {
        let metadata = self.ensure_agent_known(caller)?;
        if self.registry.agent_metadata_for_thread(target).is_some() {
            return Err(invalid(
                "local targets cannot be forwarded to another owner",
            ));
        }
        let root = self
            .observation_root(caller)
            .ok_or_else(|| invalid("caller has no admitted root"))?;
        let source = AgentForeignOrigin {
            root_thread_id: root,
            actor_id: caller,
            agent_path: metadata
                .agent_path
                .ok_or_else(|| invalid("caller has no admitted path"))?,
            turn_id: turn_id.to_owned(),
            call_id: call_id.to_owned(),
        };
        validate_origin(&source)?;
        let sources = match &operation {
            AgentForeignOperation::Message { message } => has_sources(message),
            AgentForeignOperation::SideQuestion { question } => has_sources(question),
            _ => false,
        };
        let operation_id = delivery_id(&source)?.to_string();
        let request = AgentForeignRequest {
            source,
            target_thread_id: target,
            operation,
        };
        let callback = self
            .host_callbacks
            .get()
            .map(|host| &host.route_foreign)
            .ok_or_else(|| {
                CodexErr::UnsupportedOperation("active foreign routing is unavailable".to_owned())
            })?;
        let cancellation = cancellation.child_token();
        let _cancel_on_drop = cancellation.clone().drop_guard();
        let response = tokio::select! {
            _ = self.shutdown.cancelled() => return Err(CodexErr::TurnAborted),
            _ = cancellation.cancelled() => return Err(CodexErr::TurnAborted),
            result = callback(request.clone(), cancellation.clone()) => result?,
        };
        validate_response(&request, &response)?;
        match &response {
            ForeignResponse::MessageAccepted { .. } => {
            }
            ForeignResponse::SideQuestionAnswered { .. } => {
            }
            ForeignResponse::SideQuestionFailed {
                accepted, error, ..
            } => {
                if *accepted {
                } else {
                }
                return Err(invalid(error));
            }
            _ => {}
        }
        Ok(response)
    }

    /// The app-server admits this only from its owning host connection, after
    /// the Hub has authenticated the active source and exact destination.
    pub(crate) async fn receive_foreign(
        &self,
        request: AgentForeignRequest,
        cancellation: CancellationToken,
    ) -> CodexResult<ForeignResponse> {
        validate_origin(&request.source)?;
        let target = request.target_thread_id;
        let metadata = self.ensure_agent_known(target)?;
        let root = self
            .observation_root(target)
            .ok_or_else(|| invalid("recipient has no admitted root"))?;
        if root == request.source.root_thread_id
            || self
                .registry
                .agent_metadata_for_thread(request.source.actor_id)
                .is_some()
        {
            return Err(invalid(
                "foreign input must originate outside the recipient tree",
            ));
        }
        if cancellation.is_cancelled() || self.shutdown.is_cancelled() {
            return Err(CodexErr::TurnAborted);
        }
        match request.operation {
            AgentForeignOperation::Message { message } => {
                self.receive_foreign_message(request.source, target, message, cancellation)
                    .await
            }
            AgentForeignOperation::SideQuestion { question } => {
                if question.trim().is_empty() {
                    return Ok(ForeignResponse::SideQuestionFailed {
                        target_thread_id: target,
                        accepted: false,
                        error: "side question must not be empty".to_owned(),
                    });
                }
                let operation_id = delivery_id(&request.source)?.to_string();
                let question = attributed_foreign_text(&request.source, &question);
                match self
                    .answer_recipient_side_question(
                        target,
                        request.source.actor_id,
                        operation_id,
                        question,
                    )
                    .await
                {
                    Ok(answer) => Ok(ForeignResponse::SideQuestionAnswered {
                        target_thread_id: target,
                        answer: answer.answer,
                    }),
                    Err(failure) => Ok(ForeignResponse::SideQuestionFailed {
                        target_thread_id: target,
                        accepted: failure.accepted,
                        error: failure.error.to_string(),
                    }),
                }
            }
            AgentForeignOperation::Inspect {} => Ok(ForeignResponse::Inspected {
                agent: self.observe_agent(target).await?,
            }),
            AgentForeignOperation::List { path_prefix } => {
                let prefix = path_prefix
                    .as_deref()
                    .map(|prefix| {
                        metadata
                            .agent_path
                            .as_ref()
                            .ok_or_else(|| invalid("recipient has no admitted path"))?
                            .resolve(prefix)
                            .map_err(invalid)
                    })
                    .transpose()?;
                let mut tree = self.observe_agents(root).await?;
                if let Some(prefix) = prefix {
                    tree.agents.retain(|agent| {
                        prefix.is_root()
                            || agent.path.as_deref().is_some_and(|path| {
                                path == prefix.as_str()
                                    || path
                                        .strip_prefix(prefix.as_str())
                                        .is_some_and(|suffix| suffix.starts_with('/'))
                            })
                    });
                }
                Ok(ForeignResponse::Listed { tree })
            }
        }
    }

    async fn receive_foreign_message(
        &self,
        origin: AgentForeignOrigin,
        target: ThreadId,
        message: String,
        cancellation: CancellationToken,
    ) -> CodexResult<ForeignResponse> {
        if message.trim().is_empty() {
            return Err(invalid("foreign message must not be empty"));
        }
        let id = delivery_id(&origin)?;
        let ledger = self.work_ledger()?;
        if let Some(retained) = ledger
            .observe_repeated_delivery(target, id)
            .await
            .map_err(work_error)?
        {
            return repeated_message(&retained, &origin, &message);
        }
        let _admission = self.admit_start()?;
        let root = self
            .observation_root(target)
            .ok_or_else(|| invalid("recipient has no admitted root"))?;
        let frozen_sources = self.acquire_recipient_sources(target, &message).await?;
        if cancellation.is_cancelled() || self.shutdown.is_cancelled() {
            return Err(CodexErr::TurnAborted);
        }
        let publication = PublishDelivery {
            id,
            root_id: root,
            sender_id: Some(origin.actor_id),
            recipient_id: target,
            work: None,
            kind: DeliveryKind::Message,
            payload: serde_json::to_string(&CoordinationPayload::ForeignMessage {
                origin: origin.clone(),
                original_message: message.clone(),
                frozen_sources,
            })
            .map_err(work_error)?,
        };
        if let Err(error) = ledger.publish_delivery(&publication).await {
            // Concurrent identical calls can finish source reads with different
            // bytes. The first accepted snapshot wins; compare original input.
            if error.downcast_ref::<super::work::CoordinationError>()
                == Some(&super::work::CoordinationError::DeliveryConflict)
                && let Some(retained) = ledger
                    .observe_repeated_delivery(target, id)
                    .await
                    .map_err(work_error)?
            {
                return repeated_message(&retained, &origin, &message);
            }
            return Err(work_error(error));
        }
        self.notify_coordination(target).await;
        Ok(ForeignResponse::MessageAccepted {
            target_thread_id: target,
            delivery_id: id,
        })
    }
}

fn validate_origin(origin: &AgentForeignOrigin) -> CodexResult<()> {
    if origin.turn_id.trim().is_empty() || origin.call_id.trim().is_empty() {
        return Err(invalid(
            "foreign input requires the original turn and call identity",
        ));
    }
    Ok(())
}

pub(crate) fn attributed_foreign_text(origin: &AgentForeignOrigin, text: &str) -> String {
    format!(
        "From {} (session {}, thread {}):\n{text}",
        origin.agent_path, origin.root_thread_id, origin.actor_id
    )
}

fn delivery_id(origin: &AgentForeignOrigin) -> CodexResult<Uuid> {
    let identity = serde_json::to_vec(&(
        "mekugi.foreign.operation",
        origin.root_thread_id,
        origin.actor_id,
        &origin.turn_id,
        &origin.call_id,
    ))
    .map_err(work_error)?;
    Ok(Uuid::new_v5(&Uuid::NAMESPACE_OID, &identity))
}

fn repeated_message(
    retained: &DeliveryRecord,
    origin: &AgentForeignOrigin,
    original_message: &str,
) -> CodexResult<ForeignResponse> {
    let payload: CoordinationPayload =
        serde_json::from_str(&retained.publication.payload).map_err(work_error)?;
    if let CoordinationPayload::ForeignMessage {
        origin: old,
        original_message: old_message,
        ..
    } = payload
        && &old == origin
        && old_message == original_message
        && retained.publication.sender_id == Some(origin.actor_id)
        && retained.publication.kind == DeliveryKind::Message
    {
        return Ok(ForeignResponse::MessageAccepted {
            target_thread_id: retained.publication.recipient_id,
            delivery_id: retained.publication.id,
        });
    }
    Err(invalid("conflicting repeated foreign message"))
}

fn validate_response(request: &AgentForeignRequest, response: &ForeignResponse) -> CodexResult<()> {
    let target = request.target_thread_id;
    let matches = match (&request.operation, response) {
        (
            AgentForeignOperation::Message { .. },
            ForeignResponse::MessageAccepted {
                target_thread_id,
                delivery_id: accepted_id,
            },
        ) => *target_thread_id == target && *accepted_id == delivery_id(&request.source)?,
        (
            AgentForeignOperation::SideQuestion { .. },
            ForeignResponse::SideQuestionAnswered {
                target_thread_id, ..
            },
        )
        | (
            AgentForeignOperation::SideQuestion { .. },
            ForeignResponse::SideQuestionFailed {
                target_thread_id, ..
            },
        ) => *target_thread_id == target,
        (AgentForeignOperation::Inspect {}, ForeignResponse::Inspected { agent }) => {
            agent.thread_id == target.to_string()
        }
        (AgentForeignOperation::List { .. }, ForeignResponse::Listed { .. }) => true,
        _ => false,
    };
    if !matches {
        return Err(invalid(
            "foreign owner returned a mismatched operation receipt",
        ));
    }
    Ok(())
}

fn has_sources(message: &str) -> bool {
    !crate::compact_token_budget::reload::message_selection(message)
        .sources
        .is_empty()
}

fn invalid(message: impl Into<String>) -> CodexErr {
    CodexErr::InvalidRequest(message.into())
}

#[cfg(test)]
#[path = "foreign_tests.rs"]
mod tests;

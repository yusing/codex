//! Human controls retain the ordinary turn/interrupt attached-client policy.
//! Original-host ownership guards external/foreign machine ingress, not human
//! clients. The native owner still validates root and target tree membership.

use super::thread_processor::ThreadRequestProcessor;
use crate::error_code::internal_error;
use crate::error_code::invalid_params;
use crate::error_code::invalid_request;
use codex_app_server_protocol::AgentEndParams;
use codex_app_server_protocol::AgentEndResponse;
use codex_app_server_protocol::AgentInterruptParams;
use codex_app_server_protocol::AgentInterruptResponse;
use codex_app_server_protocol::ClientResponsePayload;
use codex_app_server_protocol::JSONRPCErrorError;
use codex_protocol::ThreadId;
use codex_protocol::error::CodexErr;
use codex_protocol::error::CodexErrorDetails;

impl ThreadRequestProcessor {
    pub(crate) async fn agent_end(
        &self,
        params: AgentEndParams,
    ) -> Result<Option<ClientResponsePayload>, JSONRPCErrorError> {
        let (root, target) = target_ids(&params.root_thread_id, &params.thread_id)?;
        let ended = self
            .thread_manager
            .end_agent_subtree(root, target)
            .await
            .map_err(control_error)?;
        Ok(Some(
            AgentEndResponse {
                thread_ids: ended.into_iter().map(|id| id.to_string()).collect(),
            }
            .into(),
        ))
    }

    pub(crate) async fn agent_interrupt(
        &self,
        params: AgentInterruptParams,
    ) -> Result<Option<ClientResponsePayload>, JSONRPCErrorError> {
        let (root, target) = target_ids(&params.root_thread_id, &params.thread_id)?;
        self.thread_manager
            .interrupt_agent(root, target)
            .await
            .map_err(control_error)?;
        Ok(Some(AgentInterruptResponse {}.into()))
    }
}

fn target_ids(root: &str, target: &str) -> Result<(ThreadId, ThreadId), JSONRPCErrorError> {
    let root = ThreadId::from_string(root).map_err(|error| invalid_params(error.to_string()))?;
    let target =
        ThreadId::from_string(target).map_err(|error| invalid_params(error.to_string()))?;
    Ok((root, target))
}

fn control_error(error: CodexErr) -> JSONRPCErrorError {
    match error.details() {
        CodexErrorDetails::InvalidRequest(_)
        | CodexErrorDetails::ThreadNotFound(_)
        | CodexErrorDetails::UnsupportedOperation(_) => invalid_request(error.to_string()),
        _ => internal_error(error.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exact_targets_reject_malformed_ids_but_allow_root_interrupt_identity() {
        let root = ThreadId::new().to_string();
        let child = ThreadId::new().to_string();
        assert!(target_ids(&root, &root).is_ok());
        assert!(target_ids(&root, "/root/worker").is_err());
        assert!(target_ids("", &child).is_err());
        assert_eq!(
            target_ids(&root, &child).unwrap(),
            (
                ThreadId::from_string(&root).unwrap(),
                ThreadId::from_string(&child).unwrap(),
            )
        );
    }
}

//! Ingress is authority from the original host, never from tool arguments.

use super::thread_processor::ThreadRequestProcessor;
use crate::error_code::internal_error;
use crate::error_code::invalid_request;
use crate::outgoing_message::ConnectionId;
use codex_app_server_protocol::AgentForeignRequest;
use codex_app_server_protocol::ClientResponsePayload;
use codex_app_server_protocol::JSONRPCErrorError;
use codex_protocol::error::CodexErr;
use codex_protocol::error::CodexErrorDetails;
use tokio_util::sync::CancellationToken;

impl ThreadRequestProcessor {
    pub(crate) async fn agent_foreign_receive(
        &self,
        connection: ConnectionId,
        params: AgentForeignRequest,
        cancellation: CancellationToken,
    ) -> Result<Option<ClientResponsePayload>, JSONRPCErrorError> {
        self.thread_manager
            .authorize_agent_host(params.target_thread_id, connection.0 as u64)
            .await
            .map_err(host_error)?;
        let response = self
            .thread_manager
            .receive_foreign_agent(params, cancellation)
            .await
            .map_err(host_error)?;
        Ok(Some(response.into()))
    }
}

fn host_error(error: CodexErr) -> JSONRPCErrorError {
    match error.details() {
        CodexErrorDetails::InvalidRequest(_)
        | CodexErrorDetails::ThreadNotFound(_)
        | CodexErrorDetails::UnsupportedOperation(_) => invalid_request(error.to_string()),
        _ => internal_error(error.to_string()),
    }
}

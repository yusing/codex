use super::thread_processor::ThreadRequestProcessor;
use crate::error_code::internal_error;
use crate::error_code::invalid_params;
use codex_app_server_protocol::AgentObserveParams;
use codex_app_server_protocol::AgentObserveResponse;
use codex_app_server_protocol::AgentObserveTreeParams;
use codex_app_server_protocol::AgentObserveTreeResponse;
use codex_app_server_protocol::ClientResponsePayload;
use codex_app_server_protocol::JSONRPCErrorError;
use codex_protocol::ThreadId;
use codex_protocol::error::CodexErr;
use codex_protocol::error::CodexErrorDetails;

impl ThreadRequestProcessor {
    pub(crate) async fn agent_observe(
        &self,
        params: AgentObserveParams,
    ) -> Result<Option<ClientResponsePayload>, JSONRPCErrorError> {
        let id = ThreadId::from_string(&params.thread_id)
            .map_err(|error| invalid_params(error.to_string()))?;
        let agent = self
            .thread_manager
            .observe_agent(id)
            .await
            .map_err(observation_error)?;
        Ok(Some(AgentObserveResponse { agent }.into()))
    }

    pub(crate) async fn agent_observe_tree(
        &self,
        params: AgentObserveTreeParams,
    ) -> Result<Option<ClientResponsePayload>, JSONRPCErrorError> {
        let root = ThreadId::from_string(&params.root_thread_id)
            .map_err(|error| invalid_params(error.to_string()))?;
        // Registration precedes the snapshot so concurrent membership/work
        // mutations remain visible even if their notification arrives first.
        let (changes, shutdown) = self
            .thread_manager
            .subscribe_agent_observations(root)
            .await
            .map_err(observation_error)?;
        self.thread_watch_manager
            .watch_agent_tree(root, changes, shutdown, &self.background_tasks)
            .await;
        let tree = self
            .thread_manager
            .observe_agents(root)
            .await
            .map_err(observation_error)?;
        Ok(Some(AgentObserveTreeResponse { tree }.into()))
    }
}

fn observation_error(error: CodexErr) -> JSONRPCErrorError {
    match error.details() {
        CodexErrorDetails::ThreadNotFound(_) => invalid_params(error.to_string()),
        _ => internal_error(error.to_string()),
    }
}

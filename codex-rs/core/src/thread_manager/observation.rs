use super::*;
use crate::agent::control::LocalAgentRuntime;
use codex_app_server_protocol::AgentObservation;
use codex_app_server_protocol::AgentTreeObservation;
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

impl ThreadManager {
    pub async fn receive_foreign_agent(
        &self,
        request: codex_protocol::agent_foreign::AgentForeignRequest,
        cancellation: CancellationToken,
    ) -> CodexResult<crate::agent::control::foreign::ForeignResponse> {
        self.observation_runtime(request.target_thread_id)
            .await?
            .receive_foreign(request, cancellation)
            .await
    }

    /// Validate the immutable live host owner. Observation/subscription does not
    /// grant mutation authority and this lookup never cold-restores an actor.
    pub async fn authorize_agent_host(
        &self,
        actor: ThreadId,
        host_connection_id: u64,
    ) -> CodexResult<()> {
        let runtime = self.observation_runtime(actor).await?;
        match runtime.host_callbacks.get() {
            Some(owner) if owner.host_connection_id == host_connection_id => Ok(()),
            _ => Err(CodexErr::InvalidRequest(
                "connection does not own this native agent host".to_owned(),
            )),
        }
    }

    /// Only an already admitted active owner can supply observations. This does
    /// not restore a root or a child, even when durable history exists.
    pub(super) async fn observation_runtime(
        &self,
        actor: ThreadId,
    ) -> CodexResult<LocalAgentRuntime> {
        let threads = self.state.threads.read().await;
        for thread in threads.values() {
            let runtime = &thread.session.services.local_agent_runtime;
            if runtime.observation_root(actor).is_some() {
                return Ok(runtime.tree_runtime());
            }
        }
        drop(threads);
        Err(CodexErr::ThreadNotFound(actor))
    }

    pub async fn observe_agent(&self, thread_id: ThreadId) -> CodexResult<AgentObservation> {
        self.observation_runtime(thread_id)
            .await?
            .observe_agent(thread_id)
            .await
    }

    pub async fn observe_agents(&self, root: ThreadId) -> CodexResult<AgentTreeObservation> {
        self.observation_runtime(root)
            .await?
            .observe_agents(root)
            .await
    }

    /// The caller subscribes before requesting its initial snapshot and owns the
    /// notification task; tree shutdown terminates that task without inference.
    pub async fn subscribe_agent_observations(
        &self,
        root: ThreadId,
    ) -> CodexResult<(watch::Receiver<u64>, CancellationToken)> {
        let runtime = self.observation_runtime(root).await?;
        runtime.subscribe_observations(root)
    }
}

impl ThreadManagerState {
    pub(crate) async fn observed_compactions(
        &self,
        thread_id: ThreadId,
    ) -> CodexResult<Option<u64>> {
        self.thread_store
            .completed_compactions(thread_id)
            .await
            .map_err(|error| {
                CodexErr::Fatal(format!(
                    "failed to observe compactions for {thread_id}: {error}"
                ))
            })
    }

    pub(crate) async fn observed_model_context(
        &self,
        thread_id: ThreadId,
    ) -> CodexResult<Option<StoredModelContext>> {
        match self
            .thread_store
            .load_latest_model_context(LoadThreadHistoryParams {
                thread_id,
                include_archived: true,
            })
            .await
        {
            Ok(context) => Ok(context.revision.is_some().then_some(context)),
            Err(ThreadStoreError::Unsupported { .. } | ThreadStoreError::ThreadNotFound { .. }) => {
                Ok(None)
            }
            Err(error) => Err(CodexErr::Fatal(format!(
                "failed to observe model context for {thread_id}: {error}"
            ))),
        }
    }
}

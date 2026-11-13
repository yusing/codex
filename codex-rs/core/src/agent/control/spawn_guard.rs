//! Owns a provisional child until the spawn result can be returned without yielding.
//! Graph publication stays owned through cancellation and settles before cleanup closes its edge.

use super::AgentTreeMembership;
use super::LocalAgentRuntime;
use crate::agent::registry::AgentRegistry;
use crate::thread_manager::ThreadManagerState;
use crate::thread_manager::thread_store_error_kind;
use codex_agent_graph_store::AgentGraphStoreError;
use codex_agent_graph_store::ThreadSpawnEdgeStatus;
use codex_protocol::ThreadId;
use codex_protocol::error::CodexErrKind;
use codex_protocol::error::Result as CodexResult;
use codex_thread_store::ThreadStoreError;
use std::sync::Arc;
use tokio::task::JoinHandle;
use tracing::warn;

pub(in crate::agent) enum SpawnOwner {
    Local,
    Host,
}

pub(in crate::agent) struct PendingSpawn {
    state: Arc<ThreadManagerState>,
    pending: Option<(ThreadId, AgentTreeMembership)>,
    edge_write: Option<JoinHandle<CodexResult<bool>>>,
    close_spawn_edge: bool,
    owner: SpawnOwner,
    registered: Option<Arc<AgentRegistry>>,
    runtime: Option<LocalAgentRuntime>,
}

impl PendingSpawn {
    pub(in crate::agent) fn new(
        state: Arc<ThreadManagerState>,
        child: ThreadId,
        membership: AgentTreeMembership,
        owner: SpawnOwner,
    ) -> Self {
        Self {
            state,
            pending: Some((child, membership)),
            edge_write: None,
            close_spawn_edge: false,
            owner,
            registered: None,
            runtime: None,
        }
    }

    pub(super) fn set_edge_write(&mut self, edge_write: JoinHandle<CodexResult<bool>>) {
        self.edge_write = Some(edge_write);
    }

    /// Move provisional cleanup under the published child's exact generation.
    /// Acquisition of the replacement precedes release of parent membership.
    pub(in crate::agent) fn bind_child_runtime(
        &mut self,
        runtime: LocalAgentRuntime,
    ) -> CodexResult<()> {
        let membership = runtime.admit_start()?;
        if let Some((_, current)) = self.pending.as_mut() {
            *current = membership;
        }
        self.runtime = Some(runtime);
        Ok(())
    }

    /// Arm rollback before publishing validated provisional registry identity.
    pub(super) fn retain_registry_membership(&mut self, registry: Arc<AgentRegistry>) {
        self.registered = Some(registry);
    }

    pub(super) async fn wait_for_edge(&mut self) -> CodexResult<()> {
        let Some(edge_write) = self.edge_write.as_mut() else {
            return Ok(());
        };
        // Keep ownership while waiting, then clear the completed handle before returning errors.
        let result = edge_write.await;
        self.edge_write = None;
        // An error may follow a committed write; only an explicit skip rules it out.
        self.close_spawn_edge = !matches!(&result, Ok(Ok(false)));
        result??;
        Ok(())
    }

    pub(in crate::agent) fn disarm(mut self) -> AgentTreeMembership {
        let Some((child, membership)) = self.pending.take() else {
            unreachable!("pending spawn must own agent-tree membership");
        };
        if let Some(registry) = self.registered.take() {
            registry.finish_provisional_admission(child);
        }
        membership
    }
}

impl PendingSpawn {
    /// Known rejection waits for the same rollback that Drop schedules on caller cancellation.
    pub(in crate::agent) async fn reject(mut self) {
        if let Some(cleanup) = self.start_cleanup() {
            if let Err(error) = cleanup.await {
                warn!("cancelled child spawn cleanup task failed: {error}");
            }
        }
    }

    fn start_cleanup(&mut self) -> Option<JoinHandle<()>> {
        let (child, membership) = self.pending.take()?;
        let state = Arc::clone(&self.state);
        let runtime = self.runtime.take();
        let joining = runtime.as_ref().and_then(|runtime| {
            runtime.request_actor_shutdown(child).ok().map(|scope| {
                let ancestors = membership
                    .ancestor_membership()
                    .into_teardown_guard("child_spawn_join", Some(child));
                (scope, ancestors)
            })
        });
        let teardown = membership.into_teardown_guard("child_spawn_cleanup", Some(child));
        let edge_write = self.edge_write.take();
        let close_spawn_edge = self.close_spawn_edge;
        let local_owner = matches!(self.owner, SpawnOwner::Local);
        let registered = self.registered.take();
        Some(tokio::spawn(async move {
            if let Some(thread) = state.remove_thread(&child).await {
                thread
                    .session
                    .send_event_raw(codex_protocol::protocol::Event {
                        id: String::new(),
                        msg: codex_protocol::protocol::EventMsg::ProvisionalSpawnRejected,
                    })
                    .await;
                if let Err(error) = thread.shutdown_and_wait().await {
                    teardown
                        .record_shutdown_failure("stop_child", CodexErrKind::from(&error).into());
                    warn!("failed to stop cancelled child spawn: {error}");
                }
                if let Some(live_thread) = thread.session.live_thread() {
                    match live_thread.discard().await {
                        Ok(()) | Err(ThreadStoreError::ThreadNotFound { .. }) => {}
                        Err(error) => {
                            teardown.record_shutdown_failure(
                                "discard_child_persistence",
                                thread_store_error_kind(&error),
                            );
                            warn!("failed to discard cancelled child spawn: {error}");
                        }
                    }
                }
            }
            // A pending Open write must finish before cleanup writes Closed.
            let close_spawn_edge = match edge_write {
                Some(edge_write) => !matches!(edge_write.await, Ok(Ok(false))),
                None => close_spawn_edge,
            };
            if local_owner
                && close_spawn_edge
                && let Some(store) = state.agent_graph_store()
                && let Err(error) = store
                    .set_thread_spawn_edge_status(child, ThreadSpawnEdgeStatus::Closed)
                    .await
            {
                let error_kind = match &error {
                    AgentGraphStoreError::InvalidRequest { .. } => {
                        "agent_graph_store_invalid_request"
                    }
                    AgentGraphStoreError::Internal { .. } => "agent_graph_store_internal",
                };
                teardown.record_shutdown_failure("close_spawn_edge", error_kind);
                warn!("failed to close cancelled child spawn edge: {error}");
            }
            if let Some(registry) = registered {
                registry.release_spawned_thread(child);
            }
            teardown.complete();
            if let Some((scope, ancestors)) = joining {
                if let Err(error) = scope.wait().await {
                    ancestors
                        .record_shutdown_failure("join_subtree", CodexErrKind::from(&error).into());
                }
                if let Some(runtime) = runtime {
                    runtime.complete_actor_shutdown(&scope);
                    runtime.retire_rejected_scope(&scope);
                }
                ancestors.complete();
            }
        }))
    }
}

impl Drop for PendingSpawn {
    fn drop(&mut self) {
        // The tree teardown guard remains owned by cleanup even if the caller disappears.
        drop(self.start_cleanup());
    }
}

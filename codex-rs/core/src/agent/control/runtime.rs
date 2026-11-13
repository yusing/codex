//! Shared state and startup bindings for one local agent tree.
//! Registry identity is allocation identity; cloning this handle preserves ownership checks.

use super::LocalAgentControl;
use super::execution::AgentExecutionLimiter;
use super::residency::V2Residency;
use crate::agent::api::AgentControl;
use crate::agent::registry::AgentRegistry;
use crate::config::RolloutBudgetConfig;
use crate::rollout_budget::RolloutBudget;
use crate::session::session::Session;
use crate::state::ContextWindowIdGenerator;
use crate::thread_manager::AgentTreeShutdownFailure;
use crate::thread_manager::AgentTreeShutdownFailureReason;
use crate::thread_manager::AgentTreeShutdownReport;
use crate::thread_manager::ThreadIdGenerator;
use crate::thread_manager::ThreadManagerState;
use arc_swap::ArcSwapOption;
use codex_extension_api::ThreadInstructionsProvider;
use codex_protocol::AgentPath;
use codex_protocol::ItemIdGenerator;
use codex_protocol::SessionId;
use codex_protocol::ThreadId;
use codex_protocol::error::AgentErrorContext;
use codex_protocol::error::CodexErr;
use codex_protocol::error::Result as CodexResult;
use codex_protocol::protocol::SessionSource;
use codex_protocol::protocol::SubAgentSource;
use codex_rollout_trace::ThreadTraceContext;
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::OnceLock;
use std::sync::PoisonError;
use std::sync::Weak;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;
use tokio_util::task::task_tracker::TaskTrackerToken;
use uuid::Uuid;

// Avoid retaining an unbounded number of failures during a long-lived tree's lifetime.
const MAX_RETAINED_SHUTDOWN_FAILURES: usize = 64;

#[derive(Debug)]
pub(crate) struct AgentTreeShutdownState {
    members: TaskTracker,
    report: Mutex<AgentTreeShutdownReport>,
    cancellation: CancellationToken,
}

impl Default for AgentTreeShutdownState {
    fn default() -> Self {
        Self {
            members: TaskTracker::new(),
            cancellation: CancellationToken::new(),
            report: Mutex::new(AgentTreeShutdownReport {
                tree_id: Uuid::now_v7(),
                failures: Vec::new(),
                omitted_failures: 0,
            }),
        }
    }
}

impl AgentTreeShutdownState {
    pub(crate) async fn wait(&self) -> CodexResult<()> {
        self.wait_detailed()
            .await
            .map_err(|_| CodexErr::Fatal("agent tree shutdown did not complete cleanly".to_owned()))
    }

    pub(crate) async fn wait_detailed(&self) -> Result<(), AgentTreeShutdownReport> {
        self.members.wait().await;
        // Reporters hold a membership until after recording failures. Once the tracker drains,
        // no admitted operation can add a failure after the snapshot.
        let report = self
            .report
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();
        if report.failures.is_empty() && report.omitted_failures == 0 {
            Ok(())
        } else {
            Err(report)
        }
    }

    fn record_failure(&self, failure: AgentTreeShutdownFailure) {
        let tree_id = {
            let mut report = self.report.lock().unwrap_or_else(PoisonError::into_inner);
            if report.failures.len() < MAX_RETAINED_SHUTDOWN_FAILURES {
                report.failures.push(failure.clone());
            } else {
                report.omitted_failures = report.omitted_failures.saturating_add(/*rhs*/ 1);
            }
            report.tree_id
        };
        let (reason, phase, error_kind) = match &failure.reason {
            AgentTreeShutdownFailureReason::OperationFailed { phase, error_kind } => {
                ("operation_failed", Some(*phase), Some(*error_kind))
            }
            AgentTreeShutdownFailureReason::GuardAbandoned => ("guard_abandoned", None, None),
        };
        let thread_id = failure.thread_id.map(tracing::field::display);
        tracing::warn!(
            %tree_id,
            operation = failure.operation,
            phase,
            thread_id,
            reason,
            error_kind,
            "agent tree shutdown failure recorded"
        );
    }
}

#[derive(Clone)]
pub(crate) struct AgentTreeMembership {
    scopes: Vec<(Arc<AgentTreeShutdownState>, TaskTrackerToken)>,
}

impl AgentTreeMembership {
    /// A subtree cleanup waiter must remain owned by its ancestors without
    /// retaining a token in the exact scope it is joining.
    pub(in crate::agent) fn ancestor_membership(&self) -> Self {
        Self {
            scopes: self.scopes[..self.scopes.len().saturating_sub(1)].to_vec(),
        }
    }

    fn record_failure(&self, failure: AgentTreeShutdownFailure) {
        for (state, _) in &self.scopes {
            state.record_failure(failure.clone());
        }
    }

    pub(crate) fn into_teardown_guard(
        self,
        operation: &'static str,
        thread_id: Option<ThreadId>,
    ) -> AgentTreeTeardownGuard {
        AgentTreeTeardownGuard {
            membership: self,
            operation,
            thread_id,
            completed: false,
        }
    }
}

/// Marks tree shutdown as failed if teardown work exits without completing.
pub(crate) struct AgentTreeTeardownGuard {
    membership: AgentTreeMembership,
    operation: &'static str,
    thread_id: Option<ThreadId>,
    completed: bool,
}

impl AgentTreeTeardownGuard {
    pub(crate) fn clone_for_teardown(
        &self,
        operation: &'static str,
        thread_id: Option<ThreadId>,
    ) -> Self {
        self.membership
            .clone()
            .into_teardown_guard(operation, thread_id)
    }

    pub(crate) fn set_thread_id(&mut self, thread_id: ThreadId) {
        self.thread_id = Some(thread_id);
    }

    pub(crate) fn record_shutdown_failure(&self, phase: &'static str, error_kind: &'static str) {
        self.membership
            .record_failure(AgentTreeShutdownFailure::operation_failed(
                self.operation,
                phase,
                self.thread_id,
                error_kind,
            ));
    }

    pub(crate) fn complete(mut self) {
        self.completed = true;
    }
}

impl Drop for AgentTreeTeardownGuard {
    fn drop(&mut self) {
        if !self.completed {
            // Record before membership is dropped, so a waiter cannot snapshot first.
            self.membership.record_failure(AgentTreeShutdownFailure {
                operation: self.operation,
                thread_id: self.thread_id,
                reason: AgentTreeShutdownFailureReason::GuardAbandoned,
            });
        }
    }
}

/// A runtime generation, not logical work or graph identity. Every operation holds
/// membership in its own generation and all ancestors until its cleanup is joined.
pub(crate) struct ActorRuntimeScope {
    pub(crate) actor: ThreadId,
    parent: Option<ThreadId>,
    states: Vec<Arc<AgentTreeShutdownState>>,
    restart_allowed: AtomicBool,
    pub(crate) ending: tokio::sync::Mutex<()>,
}

impl ActorRuntimeScope {
    fn state(&self) -> &Arc<AgentTreeShutdownState> {
        self.states
            .last()
            .expect("actor scope includes its own lifetime")
    }

    pub(crate) async fn wait(&self) -> CodexResult<()> {
        self.state().wait().await
    }

    pub(crate) fn contains(&self, other: &Self) -> bool {
        other
            .states
            .iter()
            .any(|state| Arc::ptr_eq(state, self.state()))
    }

    pub(crate) fn shutdown_completed(&self) -> bool {
        self.restart_allowed.load(Ordering::Acquire)
    }

    pub(crate) fn record_failure(&self, failure: AgentTreeShutdownFailure) {
        for state in &self.states {
            state.record_failure(failure.clone());
        }
    }
}

/// Local tree state, kept separate from the shared agent operation interface.
#[derive(Clone)]
pub(crate) struct LocalAgentRuntime {
    /// Weak handle back to the global thread registry/state.
    /// This is `Weak` to avoid reference cycles and shadow persistence of the form
    /// `ThreadManagerState -> CodexThread -> Session -> SessionServices -> ThreadManagerState`.
    pub(super) manager: Weak<ThreadManagerState>,
    /// Captured at construction so delegates retain their manager's allocation policy.
    pub(super) thread_id_generator: ThreadIdGenerator,
    pub(crate) item_id_generator: ItemIdGenerator,
    pub(crate) context_window_id_generator: ContextWindowIdGenerator,
    pub(super) agent_execution_limiter: Arc<AgentExecutionLimiter>,
    /// Session-scoped state shared by the root thread and every cloned sub-agent control handle.
    pub(super) rollout_budget: Arc<RolloutBudget>,
    /// The user-selected root routing tier, shared by the entire agent tree.
    pub(super) root_service_tier: Arc<ArcSwapOption<String>>,
    /// Retains the root's opt-in instruction provider even when the root is unloaded.
    pub(super) shared_thread_instructions_provider:
        Arc<OnceLock<Arc<dyn ThreadInstructionsProvider>>>,
    pub(super) registry: Arc<AgentRegistry>,
    pub(super) residency: Arc<V2Residency>,
    pub(super) mailboxes: Arc<super::mailbox::Mailboxes>,
    pub(crate) work: Option<Arc<super::work::WorkLedger>>,
    pub(crate) host_callbacks: Arc<OnceLock<crate::agent::host_resources::AgentHostCallbacks>>,
    pub(super) wait_deadlines: Arc<super::recovery::WaitDeadlines>,
    /// Shared by every session in this tree, including private delegates.
    pub(crate) shutdown: CancellationToken,
    shutdown_state: Arc<AgentTreeShutdownState>,
    actor_scopes: Arc<Mutex<HashMap<ThreadId, Arc<ActorRuntimeScope>>>>,
    actor_scope: Option<Arc<ActorRuntimeScope>>,
}

impl LocalAgentRuntime {
    /// Management/observation borrows the real tree lifetime, never whichever
    /// child's live session happened to supply the shared owner.
    pub(crate) fn tree_runtime(&self) -> Self {
        let mut runtime = self.clone();
        runtime.actor_scope = None;
        runtime.shutdown = self.shutdown_state.cancellation.clone();
        runtime
    }

    pub(crate) fn registered_parent(&self, actor: ThreadId) -> Option<ThreadId> {
        let scopes = self
            .actor_scopes
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        self.registered_parent_in(&scopes, actor)
    }

    fn registered_parent_in(
        &self,
        scopes: &HashMap<ThreadId, Arc<ActorRuntimeScope>>,
        actor: ThreadId,
    ) -> Option<ThreadId> {
        if let Some(scope) = scopes.get(&actor) {
            return scope.parent;
        }
        let metadata = self.registry.agent_metadata_for_thread(actor)?;
        let path = metadata.agent_path?;
        let (parent, _) = path.as_str().rsplit_once('/')?;
        self.registry
            .agent_id_for_path(&AgentPath::try_from(parent).ok()?)
    }

    #[cfg(test)]
    pub(crate) fn root_actor(&self) -> Option<ThreadId> {
        self.registry.agent_id_for_path(&AgentPath::root())
    }

    pub(crate) fn validate_child_target(
        &self,
        root: ThreadId,
        target: ThreadId,
    ) -> CodexResult<()> {
        if self.observation_root(root) != Some(root) || self.observation_root(target) != Some(root)
        {
            return Err(CodexErr::InvalidRequest(
                "target is not admitted to the requested agent tree".to_owned(),
            ));
        }
        if root == target || self.registered_parent(target).is_none() {
            return Err(CodexErr::InvalidRequest(
                "root is not an endable child".to_owned(),
            ));
        }
        Ok(())
    }

    pub(crate) fn registered_subtree(&self, target: ThreadId) -> CodexResult<Vec<ThreadId>> {
        let path = self.ensure_agent_known(target)?.agent_path.ok_or_else(|| {
            CodexErr::InvalidRequest("target is missing its admitted agent path".to_owned())
        })?;
        let prefix = format!("{path}/");
        let mut members: Vec<_> = self
            .registry
            .snapshot()
            .agents
            .into_iter()
            .filter_map(|metadata| Some((metadata.agent_path?, metadata.agent_id?)))
            .filter(|(member, _)| member == &path || member.as_str().starts_with(&prefix))
            .collect();
        members.sort_by(|left, right| left.0.cmp(&right.0));
        Ok(members.into_iter().map(|(_, actor)| actor).collect())
    }

    pub(super) fn new(
        manager: Weak<ThreadManagerState>,
        thread_id_generator: ThreadIdGenerator,
        rollout_budget: Option<RolloutBudgetConfig>,
        work_state: Option<Arc<codex_state::StateRuntime>>,
    ) -> Self {
        let shutdown_state = Arc::new(AgentTreeShutdownState::default());
        let mut runtime = Self {
            manager,
            thread_id_generator,
            item_id_generator: Session::new_item_id_generator(),
            context_window_id_generator: Arc::new(codex_protocol::ContextWindowId::new_uuid_v7),
            registry: Arc::default(),
            residency: Arc::default(),
            mailboxes: Arc::default(),
            work: None,
            host_callbacks: Arc::default(),
            wait_deadlines: Arc::default(),
            shutdown: shutdown_state.cancellation.clone(),
            shutdown_state,
            actor_scopes: Arc::default(),
            actor_scope: None,
            agent_execution_limiter: Arc::default(),
            rollout_budget: Arc::default(),
            root_service_tier: Arc::new(ArcSwapOption::from(None)),
            shared_thread_instructions_provider: Arc::default(),
        };
        runtime.work = work_state.map(|state| {
            Arc::new(super::work::WorkLedger::new(
                state,
                Arc::clone(&runtime.registry),
                runtime.tree_id(),
            ))
        });
        if let Some(rollout_budget) = rollout_budget {
            runtime.rollout_budget.configure(rollout_budget);
        }
        runtime
    }

    pub(crate) fn work_ledger(&self) -> CodexResult<&super::work::WorkLedger> {
        self.work.as_deref().ok_or_else(|| {
            CodexErr::UnsupportedOperation("durable agent work storage was not supplied".to_owned())
        })
    }

    pub(crate) fn tree_id(&self) -> Uuid {
        self.shutdown_state
            .report
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .tree_id
    }

    /// Bind local startup to the same tree state with this session's identity.
    pub(crate) fn control(&self, session_id: SessionId) -> LocalAgentControl {
        LocalAgentControl {
            session_id,
            runtime: self.clone(),
        }
    }
}

/// Local construction binds identity after reading history. Hosts and internal children
/// provide an already-bound controller without selecting a backend again.
#[derive(Clone)]
pub(crate) enum AgentControlInit {
    Local(LocalAgentControl),
    Provided {
        control: Arc<dyn AgentControl>,
        runtime: LocalAgentRuntime,
    },
}

impl From<LocalAgentControl> for AgentControlInit {
    fn from(control: LocalAgentControl) -> Self {
        Self::Local(control)
    }
}

impl AgentControlInit {
    pub(crate) fn with_runtime(self, runtime: LocalAgentRuntime) -> Self {
        match self {
            Self::Local(mut control) => {
                control.runtime = runtime;
                Self::Local(control)
            }
            Self::Provided { control, .. } => Self::Provided { control, runtime },
        }
    }

    pub(crate) fn runtime(&self) -> &LocalAgentRuntime {
        match self {
            Self::Local(control) => &control.runtime,
            Self::Provided { runtime, .. } => runtime,
        }
    }

    pub(crate) fn control(&self) -> &dyn AgentControl {
        match self {
            Self::Local(control) => control,
            Self::Provided { control, .. } => control.as_ref(),
        }
    }
}

impl LocalAgentRuntime {
    /// Borrow an existing actor lifetime without loading, renewing or starting it.
    pub(crate) fn for_actor(&self, actor: ThreadId) -> CodexResult<Self> {
        let scopes = self
            .actor_scopes
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let scope = scopes.get(&actor).ok_or(CodexErr::ThreadNotFound(actor))?;
        Ok(self.with_actor_scope(Arc::clone(scope)))
    }

    fn with_actor_scope(&self, scope: Arc<ActorRuntimeScope>) -> Self {
        let mut runtime = self.clone();
        runtime.shutdown = scope.state().cancellation.clone();
        runtime.actor_scope = Some(scope);
        runtime
    }

    /// Explicit admission may renew an ended generation only after its prior end
    /// operation joined. Old runtime handles retain their cancelled generation.
    /// No session or logical work is created by establishing this lifetime scope.
    pub(crate) fn for_actor_start(
        &self,
        actor: ThreadId,
        parent: Option<ThreadId>,
    ) -> CodexResult<Self> {
        if self.shutdown.is_cancelled() {
            return Err(runtime_shutting_down());
        }
        let mut scopes = self
            .actor_scopes
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if self.shutdown_state.members.is_closed() {
            return Err(runtime_shutting_down());
        }
        let scope = self.start_actor_scope(&mut scopes, actor, parent)?;
        Ok(self.with_actor_scope(scope))
    }

    fn start_actor_scope(
        &self,
        scopes: &mut HashMap<ThreadId, Arc<ActorRuntimeScope>>,
        actor: ThreadId,
        parent: Option<ThreadId>,
    ) -> CodexResult<Arc<ActorRuntimeScope>> {
        if parent == Some(actor) {
            return Err(CodexErr::InvalidRequest(
                "an actor cannot own itself".to_owned(),
            ));
        }
        if let Some(scope) = scopes.get(&actor) {
            if scope.parent != parent {
                return Err(CodexErr::InvalidRequest(
                    "actor lifetime parent does not match admitted ownership".to_owned(),
                ));
            }
            if !scope.state().cancellation.is_cancelled() {
                return Ok(Arc::clone(scope));
            }
            if !scope.restart_allowed.load(Ordering::Acquire) || !scope.state().members.is_empty() {
                return Err(runtime_shutting_down());
            }
        }
        let states = if let Some(parent) = parent {
            // A retained but unloaded ancestor needs only a lifetime scope, never
            // a synthetic session or automatically restarted assignment.
            let parent_parent = self.registered_parent_in(scopes, parent);
            let parent_scope = self.start_actor_scope(scopes, parent, parent_parent)?;
            let mut states = parent_scope.states.clone();
            let mut state = AgentTreeShutdownState::default();
            state.cancellation = parent_scope.state().cancellation.child_token();
            states.push(Arc::new(state));
            states
        } else {
            vec![Arc::clone(&self.shutdown_state)]
        };
        let scope = Arc::new(ActorRuntimeScope {
            actor,
            parent,
            states,
            restart_allowed: AtomicBool::new(false),
            ending: tokio::sync::Mutex::new(()),
        });
        scopes.insert(actor, Arc::clone(&scope));
        Ok(scope)
    }

    pub(crate) fn admit_start(&self) -> CodexResult<AgentTreeMembership> {
        // TaskTracker::close alone does not reject new tokens. The same owner lock
        // linearizes scope admission and both subtree and whole-tree fences.
        let _scopes = self
            .actor_scopes
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let states = self.actor_scope.as_ref().map_or_else(
            || std::slice::from_ref(&self.shutdown_state),
            |scope| scope.states.as_slice(),
        );
        if states
            .iter()
            .any(|state| state.members.is_closed() || state.cancellation.is_cancelled())
        {
            return Err(runtime_shutting_down());
        }
        Ok(AgentTreeMembership {
            scopes: states
                .iter()
                .map(|state| (Arc::clone(state), state.members.token()))
                .collect(),
        })
    }

    pub(crate) fn request_shutdown(&self) -> Arc<AgentTreeShutdownState> {
        let _scopes = self
            .actor_scopes
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        self.shutdown_state.members.close();
        self.shutdown_state.cancellation.cancel();
        self.mailboxes.close();
        Arc::clone(&self.shutdown_state)
    }

    /// The caller must hold tree membership until this record has been written. This keeps
    /// the completed shutdown report stable for every waiter.
    pub(crate) fn record_shutdown_failure(&self, failure: AgentTreeShutdownFailure) {
        if let Some(scope) = &self.actor_scope {
            scope.record_failure(failure);
        } else {
            self.shutdown_state.record_failure(failure);
        }
    }

    pub(crate) fn request_actor_shutdown(
        &self,
        actor: ThreadId,
    ) -> CodexResult<Arc<ActorRuntimeScope>> {
        let scopes = self
            .actor_scopes
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let scope = scopes.get(&actor).ok_or(CodexErr::ThreadNotFound(actor))?;
        if scope.parent.is_none() {
            return Err(CodexErr::InvalidRequest(
                "root is not an endable child".to_owned(),
            ));
        }
        for child in scopes.values().filter(|child| scope.contains(child)) {
            child.state().members.close();
        }
        scope.state().cancellation.cancel();
        Ok(Arc::clone(scope))
    }

    /// An end operation joins the selected scope while remaining admitted to
    /// every ancestor. A concurrent parent end must also join this cleanup.
    pub(crate) fn admit_actor_teardown(&self, actor: ThreadId) -> CodexResult<AgentTreeMembership> {
        let scopes = self
            .actor_scopes
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let scope = scopes.get(&actor).ok_or(CodexErr::ThreadNotFound(actor))?;
        let states = &scope.states[..scope.states.len().saturating_sub(1)];
        if states.is_empty()
            || states
                .iter()
                .any(|state| state.members.is_closed() || state.cancellation.is_cancelled())
        {
            return Err(runtime_shutting_down());
        }
        Ok(AgentTreeMembership {
            scopes: states
                .iter()
                .map(|state| (Arc::clone(state), state.members.token()))
                .collect(),
        })
    }

    /// Called only after the fenced generation and cleanup have actually joined.
    pub(crate) fn complete_actor_shutdown(&self, scope: &ActorRuntimeScope) {
        let scopes = self
            .actor_scopes
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        for child in scopes.values().filter(|child| scope.contains(child)) {
            child.restart_allowed.store(true, Ordering::Release);
        }
    }

    /// Failed provisional identities have no retained graph contract. Remove
    /// their joined generation, but never a replacement or an admitted actor.
    pub(in crate::agent) fn retire_rejected_scope(&self, scope: &ActorRuntimeScope) {
        let mut scopes = self
            .actor_scopes
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        scopes.retain(|actor, child| {
            !scope.contains(child)
                || !child.state().members.is_empty()
                || self.registry.agent_metadata_for_thread(*actor).is_some()
        });
    }

    pub(crate) fn actor_scope(&self) -> Option<&Arc<ActorRuntimeScope>> {
        self.actor_scope.as_ref()
    }

    pub(crate) fn actors_in_scope(&self, scope: &ActorRuntimeScope) -> Vec<ThreadId> {
        self.actor_scopes
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .values()
            .filter(|child| scope.contains(child))
            .map(|child| child.actor)
            .collect()
    }

    pub(crate) fn generate_thread_id(&self) -> ThreadId {
        (self.thread_id_generator)()
    }

    /// Shares the tree's trace with a new or reloaded child runtime.
    pub(crate) async fn parent_rollout_thread_trace(
        &self,
        session_source: &SessionSource,
    ) -> ThreadTraceContext {
        let SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
            parent_thread_id, ..
        }) = session_source
        else {
            return ThreadTraceContext::disabled();
        };
        let Some(manager) = self.manager.upgrade() else {
            return ThreadTraceContext::disabled();
        };
        // Reattach reloaded children through the v2 root: their immediate parent
        // may also be evicted. V1 has no registered root, so it uses the parent.
        let trace_owner = self
            .registry
            .agent_id_for_path(&AgentPath::root())
            .unwrap_or(*parent_thread_id);
        manager
            .get_thread(trace_owner)
            .await
            .ok()
            .filter(|thread| thread.is_running())
            .map(|thread| thread.session.services.rollout_thread_trace.clone())
            .unwrap_or_else(ThreadTraceContext::disabled)
    }

    pub(crate) fn root_thread_instructions_provider(
        &self,
        root_thread_id: ThreadId,
        provider: Option<Arc<dyn ThreadInstructionsProvider>>,
    ) -> Option<Arc<dyn ThreadInstructionsProvider>> {
        let provider = match self.manager.upgrade() {
            Some(manager) => manager.shared_thread_instructions_provider(root_thread_id, provider),
            None => provider,
        };
        if let Some(provider) = provider
            .as_ref()
            .filter(|provider| provider.share_with_subagents())
        {
            let _ = self
                .shared_thread_instructions_provider
                .set(Arc::clone(provider));
        }
        provider
    }
}

pub(crate) fn runtime_shutting_down() -> CodexErr {
    CodexErr::InvalidRequest("agent runtime is shutting down".to_owned())
        .with_agent_context(AgentErrorContext::RuntimeShutdown)
}

#[cfg(test)]
#[path = "runtime_tests.rs"]
mod tests;

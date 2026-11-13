//! Tracks local running capacity and releases reservations through the shared guard.
//! The local permit owns the running count; root and MAv1 turns remain unrestricted.

use super::LocalAgentControl;
use crate::agent::types::AgentExecutionGuard;
use codex_protocol::error::AgentErrorContext;
use codex_protocol::error::CodexErr;
use codex_protocol::error::CodexErrorDetails;
use codex_protocol::error::Result as CodexResult;
use codex_protocol::protocol::MultiAgentVersion;
use codex_protocol::protocol::SessionSource;
use std::sync::Arc;
use std::sync::OnceLock;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use tokio::sync::Notify;

#[derive(Default)]
pub(super) struct AgentExecutionLimiter {
    active: AtomicUsize,
    max_threads: OnceLock<Option<usize>>,
    released: Notify,
}

struct LocalExecutionPermit {
    limiter: Arc<AgentExecutionLimiter>,
}

impl Drop for LocalExecutionPermit {
    fn drop(&mut self) {
        self.limiter.active.fetch_sub(1, Ordering::AcqRel);
        self.limiter.released.notify_waiters();
    }
}

impl LocalAgentControl {
    pub(crate) fn ensure_execution_capacity(
        &self,
        multi_agent_version: MultiAgentVersion,
        session_source: &SessionSource,
    ) -> CodexResult<()> {
        if !is_execution_limited(multi_agent_version, session_source) {
            return Ok(());
        }
        let Some(max_threads) = self.runtime.agent_execution_limiter.max_threads() else {
            return Ok(());
        };
        if self
            .runtime
            .agent_execution_limiter
            .active
            .load(Ordering::Acquire)
            < max_threads
        {
            Ok(())
        } else {
            Err(
                CodexErr::new(CodexErrorDetails::AgentLimitReached { max_threads })
                    .with_agent_context(AgentErrorContext::ExecutionCapacity),
            )
        }
    }

    pub(crate) fn execution_guard(
        &self,
        multi_agent_version: MultiAgentVersion,
        session_source: &SessionSource,
    ) -> Option<AgentExecutionGuard> {
        is_execution_limited(multi_agent_version, session_source)
            .then(|| Arc::clone(&self.runtime.agent_execution_limiter).guard())
    }
}

impl AgentExecutionLimiter {
    pub(super) fn initialize(&self, max_threads: Option<usize>) {
        self.max_threads.get_or_init(|| max_threads);
    }

    fn max_threads(&self) -> Option<usize> {
        self.max_threads.get().copied().flatten()
    }

    fn guard(self: Arc<Self>) -> AgentExecutionGuard {
        self.active.fetch_add(1, Ordering::AcqRel);
        self.reserved_guard()
    }

    fn reserved_guard(self: Arc<Self>) -> AgentExecutionGuard {
        let limiter = Arc::clone(&self);
        AgentExecutionGuard::resumable(LocalExecutionPermit { limiter: self }, move || {
            Box::pin(async move { Ok(limiter.resume().await) })
        })
    }

    async fn resume(self: Arc<Self>) -> AgentExecutionGuard {
        loop {
            let released = self.released.notified();
            tokio::pin!(released);
            // Register before inspecting the counter so a concurrent release cannot be lost.
            released.as_mut().enable();
            if self
                .active
                .fetch_update(Ordering::AcqRel, Ordering::Acquire, |active| {
                    self.max_threads()
                        .is_none_or(|limit| active < limit)
                        .then_some(active + 1)
                })
                .is_ok()
            {
                return Arc::clone(&self).reserved_guard();
            }
            released.await;
        }
    }
}

fn is_execution_limited(
    multi_agent_version: MultiAgentVersion,
    session_source: &SessionSource,
) -> bool {
    multi_agent_version == MultiAgentVersion::V2
        && matches!(session_source, SessionSource::SubAgent(_))
}

#[cfg(test)]
#[path = "execution_tests.rs"]
mod tests;

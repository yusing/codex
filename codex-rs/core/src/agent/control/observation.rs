//! Read-only projection of the registry and its existing work/store owners.

use super::LocalAgentRuntime;
use crate::agent::types::AgentMetadata;
use codex_app_server_protocol::AgentObservation;
use codex_app_server_protocol::AgentRuntimeStatus;
use codex_app_server_protocol::AgentTreeObservation;
use codex_app_server_protocol::AgentWaitMode;
use codex_app_server_protocol::AgentWaitObservation;
use codex_app_server_protocol::AgentWaitPhase;
use codex_app_server_protocol::AgentWaitTarget;
use codex_app_server_protocol::AgentWorkDisposition;
use codex_app_server_protocol::AgentWorkObservation;
use codex_protocol::AgentPath;
use codex_protocol::ThreadId;
use codex_protocol::agent_metrics::AgentMetrics;
use codex_protocol::error::CodexErr;
use codex_protocol::error::CodexErrorDetails;
use codex_protocol::error::Result as CodexResult;
use codex_protocol::protocol::AgentStatus;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::TokenUsageInfo;
use codex_rollout::RolloutItem;
use codex_state::coordination::ActorWorkObservation;
use codex_state::coordination::WorkDisposition;
use codex_state::coordination_wait::WaitMode;
use codex_state::coordination_wait::WaitPhase;
use codex_thread_store::ReadThreadParams;
use std::collections::HashMap;
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

impl LocalAgentRuntime {
    pub(crate) fn observation_root(&self, actor: ThreadId) -> Option<ThreadId> {
        self.registry.agent_metadata_for_thread(actor)?;
        self.registry.agent_id_for_path(&AgentPath::root())
    }
}

impl LocalAgentRuntime {
    pub(crate) fn subscribe_observations(
        &self,
        root: ThreadId,
    ) -> CodexResult<(watch::Receiver<u64>, CancellationToken)> {
        if self.observation_root(root) != Some(root) {
            return Err(CodexErr::ThreadNotFound(root));
        }
        Ok((self.registry.subscribe(), self.shutdown.clone()))
    }

    pub(crate) async fn observe_agents(&self, root: ThreadId) -> CodexResult<AgentTreeObservation> {
        if self.observation_root(root) != Some(root) {
            return Err(CodexErr::ThreadNotFound(root));
        }
        let snapshot = self.registry.snapshot();
        let ids: Vec<_> = snapshot
            .agents
            .iter()
            .filter_map(|agent| agent.agent_id)
            .collect();
        let mut facts = self.observed_work(&ids).await?;
        let mut agents = Vec::with_capacity(snapshot.agents.len());
        for metadata in snapshot.agents {
            let Some(id) = metadata.agent_id else {
                continue;
            };
            let mut agent = self.observe_member(root, id, metadata, false).await?;
            apply_work(&mut agent, facts.remove(&id));
            agents.push(agent);
        }
        agents.sort_by(|left, right| {
            left.path
                .cmp(&right.path)
                .then_with(|| left.thread_id.cmp(&right.thread_id))
        });
        Ok(AgentTreeObservation {
            root_thread_id: root.to_string(),
            observation_revision: snapshot.revision,
            agents,
        })
    }

    pub(crate) async fn observe_agent(&self, id: ThreadId) -> CodexResult<AgentObservation> {
        let root = self
            .observation_root(id)
            .ok_or_else(|| CodexErr::ThreadNotFound(id))?;
        let metadata = self
            .registry
            .agent_metadata_for_thread(id)
            .ok_or_else(|| CodexErr::ThreadNotFound(id))?;
        let mut facts = self.observed_work(&[id]).await?;
        let mut agent = self.observe_member(root, id, metadata, true).await?;
        apply_work(&mut agent, facts.remove(&id));
        Ok(agent)
    }

    async fn observed_work(
        &self,
        actors: &[ThreadId],
    ) -> CodexResult<HashMap<ThreadId, ActorWorkObservation>> {
        let Some(work) = self.work.as_ref() else {
            return Ok(HashMap::new());
        };
        let facts = work
            .observe_members(actors)
            .await
            .map_err(|error| CodexErr::Fatal(format!("failed to observe agent work: {error}")))?;
        Ok(facts
            .into_iter()
            .map(|fact| (fact.actor_id, fact))
            .collect())
    }

    async fn observe_member(
        &self,
        root: ThreadId,
        id: ThreadId,
        metadata: AgentMetadata,
        inspect: bool,
    ) -> CodexResult<AgentObservation> {
        let parent = metadata
            .agent_path
            .as_ref()
            .and_then(|path| path.as_str().rsplit_once('/'))
            .and_then(|(parent, _)| AgentPath::try_from(parent).ok())
            .and_then(|path| self.registry.agent_id_for_path(&path));
        let mut observed = AgentObservation {
            thread_id: id.to_string(),
            root_thread_id: root.to_string(),
            parent_thread_id: parent.map(|id| id.to_string()),
            path: metadata.agent_path.map(|path| path.to_string()),
            nickname: metadata.agent_nickname,
            role: metadata.agent_role,
            backend: None,
            attachable: None,
            loaded: false,
            status: None,
            cwd: None,
            model: None,
            reasoning_effort: None,
            service_tier: None,
            metrics: AgentMetrics::default(),
            work: None,
            wait: None,
        };
        let manager = self.upgrade()?;
        match manager.get_thread(id).await {
            Ok(thread) => {
                let config = thread.config_snapshot().await;
                observed.loaded = true;
                observed.backend = Some("codex".to_owned());
                observed.attachable = Some(true);
                observed.status = Some(match thread.agent_status().await {
                    AgentStatus::PendingInit => AgentRuntimeStatus::PendingInit,
                    AgentStatus::Running => AgentRuntimeStatus::Running,
                    AgentStatus::Interrupted => AgentRuntimeStatus::Interrupted,
                    AgentStatus::Completed(_) => AgentRuntimeStatus::Completed,
                    AgentStatus::Errored(_) => AgentRuntimeStatus::Errored,
                    AgentStatus::Shutdown => AgentRuntimeStatus::Shutdown,
                    AgentStatus::NotFound => AgentRuntimeStatus::NotFound,
                });
                observed.cwd = Some(config.cwd().to_path_buf());
                let (model, effort) = thread.observed_model_settings().await;
                observed.model = Some(model);
                observed.reasoning_effort = effort;
                observed.service_tier = config.service_tier;
                if let Some(usage) = thread.token_usage_info().await {
                    apply_usage(&mut observed.metrics, &usage);
                }
            }
            Err(error) if matches!(error.details(), CodexErrorDetails::ThreadNotFound(_)) => {
                match manager
                    .read_stored_thread(ReadThreadParams {
                        thread_id: id,
                        include_archived: true,
                        include_history: false,
                    })
                    .await
                {
                    Ok(stored) => {
                        observed.backend = Some("codex".to_owned());
                        observed.attachable = Some(true);
                        observed.cwd = Some(stored.cwd);
                        observed.model = stored.model;
                        observed.reasoning_effort = stored.reasoning_effort;
                        if inspect && let Some(context) = manager.observed_model_context(id).await?
                        {
                            apply_retained_context(&mut observed, &context.items);
                        }
                    }
                    Err(error)
                        if matches!(error.details(), CodexErrorDetails::ThreadNotFound(_)) => {}
                    Err(error) => return Err(error),
                }
            }
            Err(error) => return Err(error),
        }
        if inspect && observed.backend.as_deref() == Some("codex") {
            observed.metrics.completed_compactions = manager.observed_compactions(id).await?;
        }
        Ok(observed)
    }
}

fn apply_usage(metrics: &mut AgentMetrics, usage: &TokenUsageInfo) {
    metrics.context_tokens = Some(usage.last_token_usage.total_tokens);
    metrics.context_window = usage.model_context_window;
}

fn apply_retained_context(observed: &mut AgentObservation, items: &[RolloutItem]) {
    for item in items {
        match item {
            RolloutItem::EventMsg(EventMsg::TokenCount(event)) => {
                if let Some(usage) = &event.info {
                    apply_usage(&mut observed.metrics, usage);
                }
            }
            RolloutItem::EventMsg(EventMsg::SessionConfigured(event))
                if event.thread_id.to_string() == observed.thread_id
                    && observed.model.as_deref() == Some(event.model.as_str()) =>
            {
                observed.service_tier = event.service_tier.clone();
            }
            RolloutItem::EventMsg(EventMsg::ThreadSettingsApplied(event))
                if event
                    .thread_id
                    .is_none_or(|id| id.to_string() == observed.thread_id) =>
            {
                // Persisted settings retain the selection, not the normalized
                // request tier. Do not report it as applied without a supplier.
                observed.service_tier = None;
            }
            _ => {}
        }
    }
}

fn apply_work(observed: &mut AgentObservation, facts: Option<ActorWorkObservation>) {
    let Some(facts) = facts else {
        return;
    };
    observed.work = facts.work.map(|work| AgentWorkObservation {
        work_id: work.stamp.work_id.to_string(),
        revision: work.revision,
        disposition: match work.disposition {
            WorkDisposition::PendingStart => AgentWorkDisposition::PendingStart,
            WorkDisposition::Running => AgentWorkDisposition::Running,
            WorkDisposition::Waiting => AgentWorkDisposition::Waiting,
            WorkDisposition::Paused => AgentWorkDisposition::Paused,
            WorkDisposition::Uncertain => AgentWorkDisposition::Uncertain,
            WorkDisposition::Completed => AgentWorkDisposition::Completed,
            WorkDisposition::Failed => AgentWorkDisposition::Failed,
            WorkDisposition::Interrupted => AgentWorkDisposition::Interrupted,
        },
        terminal_delivery_id: work.terminal_delivery_id.map(|id| id.to_string()),
    });
    observed.wait = facts.wait.map(|wait| AgentWaitObservation {
        wait_id: wait.id.to_string(),
        mode: match wait.mode {
            WaitMode::Any => AgentWaitMode::Any,
            WaitMode::All => AgentWaitMode::All,
        },
        phase: match wait.phase {
            WaitPhase::Registered => AgentWaitPhase::Registered,
            WaitPhase::Claimed => AgentWaitPhase::Claimed,
            WaitPhase::Dispatched => AgentWaitPhase::Dispatched,
            WaitPhase::Accepted => AgentWaitPhase::Accepted,
            WaitPhase::Consumed => AgentWaitPhase::Consumed,
            WaitPhase::Cancelled => AgentWaitPhase::Cancelled,
        },
        reason: wait.reason.map(|reason| {
            use codex_protocol::agent_coordination::CoordinationTurnWaitReason;
            use codex_state::coordination_wait::TurnWaitReason;
            match reason {
                TurnWaitReason::DescendantRecovery => {
                    CoordinationTurnWaitReason::DescendantRecovery
                }
                TurnWaitReason::ProviderCapacity => CoordinationTurnWaitReason::ProviderCapacity,
                TurnWaitReason::ProviderUsageLimit => {
                    CoordinationTurnWaitReason::ProviderUsageLimit
                }
            }
        }),
        timeout_ms: wait.timeout_ms,
        deadline_unix_ms: wait.deadline_unix_ms,
        selected: wait
            .selected
            .into_iter()
            .map(|stamp| AgentWaitTarget {
                thread_id: stamp.actor_id.to_string(),
                work_id: stamp.work_id.to_string(),
            })
            .collect(),
    });
}

#[cfg(test)]
#[path = "observation_tests.rs"]
mod tests;

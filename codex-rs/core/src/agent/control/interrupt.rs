//! Applies V2 interruption rules to a registered agent without loading its runtime.
//!
//! Root and self targets are rejected. An unloaded or already-dead runtime is a successful
//! interruption; this operation never reloads it.

use super::LocalAgentControl;
use super::LocalAgentRuntime;
use crate::agent::api::AgentInfo;
use codex_protocol::AgentPath;
use codex_protocol::ThreadId;
use codex_protocol::error::CodexErr;
use codex_protocol::error::CodexErrorDetails;
use codex_protocol::error::Result as CodexResult;

impl LocalAgentControl {
    /// Interrupts a spawned agent's current task, preserving the status observed before dispatch.
    pub(crate) async fn interrupt_spawned_agent(
        &self,
        caller: ThreadId,
        target: ThreadId,
    ) -> CodexResult<AgentInfo> {
        self.interrupt_spawned_agent_with_receipt(caller, target)
            .await
            .map(|receipt| receipt.snapshot)
    }

    pub(crate) async fn interrupt_spawned_agent_with_receipt(
        &self,
        caller: ThreadId,
        target: ThreadId,
    ) -> CodexResult<crate::agent::api::AgentInterruptReceipt> {
        let receiver_agent = self.runtime.ensure_agent_known(target)?;
        if receiver_agent
            .agent_path
            .as_ref()
            .is_some_and(AgentPath::is_root)
        {
            return Err(CodexErr::UnsupportedOperation(
                "root is not a spawned agent".to_string(),
            ));
        }
        if target == caller {
            return Err(CodexErr::UnsupportedOperation(
                "an agent cannot interrupt itself; return your result and let the parent interrupt you if needed"
                    .to_string(),
            ));
        }
        receiver_agent.agent_path.as_ref().ok_or_else(|| {
            CodexErr::UnsupportedOperation("target agent is missing an agent_path".to_string())
        })?;
        let snapshot = self.inspect_agent(target).await?;
        if self.runtime.work.is_some() {
            let requested_work = self
                .runtime
                .interrupt_owned_actor_with_receipt(target)
                .await?;
            return Ok(crate::agent::api::AgentInterruptReceipt {
                snapshot,
                requested_work,
            });
        }
        match self.interrupt_agent(target).await {
            Ok(_) => {}
            Err(err)
                if matches!(
                    err.details(),
                    CodexErrorDetails::ThreadNotFound(_) | CodexErrorDetails::InternalAgentDied
                ) => {}
            Err(err) => return Err(err),
        }
        Ok(crate::agent::api::AgentInterruptReceipt {
            snapshot,
            requested_work: None,
        })
    }
}

impl LocalAgentRuntime {
    /// Project the owner's exact request receipt. Terminal work facts remain a
    /// separate event and may already have arrived before this acknowledgement.
    pub(crate) async fn publish_interrupt_request(
        &self,
        caller: ThreadId,
        turn_id: String,
        operation_id: String,
        receipt: &crate::agent::api::AgentInterruptReceipt,
    ) {
        use codex_protocol::agent_coordination::AgentCoordinationEvent;
        use codex_protocol::agent_coordination::CoordinationInterruptRequest;
        use codex_protocol::agent_coordination::CoordinationProjection;
        use codex_protocol::agent_coordination::CoordinationTarget;
        let Some(work) = receipt.requested_work else {
            return;
        };
        let Some(task_name) = receipt.snapshot.metadata().agent_path.clone() else {
            return;
        };
        let event = AgentCoordinationEvent {
            thread_id: caller,
            turn_id,
            projection: CoordinationProjection {
                interrupt: Some(CoordinationInterruptRequest {
                    operation_id,
                    target: CoordinationTarget {
                        thread_id: work.actor_id,
                        task_name,
                        work_id: work.work_id,
                    },
                }),
                ..Default::default()
            },
        };
        if let Some(manager) = self.manager.upgrade()
            && let Ok(thread) = manager.get_thread(caller).await
        {
            thread
                .session
                .send_event_raw(codex_protocol::protocol::Event {
                    id: event.turn_id.clone(),
                    msg: codex_protocol::protocol::EventMsg::AgentCoordination(event),
                })
                .await;
        } else if let Some(host) = self.host_callbacks.get() {
            (host.publish_coordination)(event).await;
        }
    }

    /// Trusted human control may interrupt the root's logical wait too. Model
    /// tools retain their root/self checks above; this action invents no host turn.
    pub(crate) async fn interrupt_owned_actor(&self, target: ThreadId) -> CodexResult<()> {
        self.interrupt_owned_actor_with_receipt(target)
            .await
            .map(drop)
    }

    pub(crate) async fn interrupt_owned_actor_with_receipt(
        &self,
        target: ThreadId,
    ) -> CodexResult<Option<super::work::WorkStamp>> {
        self.ensure_agent_known(target)?;
        let manager = self.upgrade()?;
        if let Ok(thread) = manager.get_thread(target).await {
            // The native task owner joins active cancellation and settles idle
            // logical waits. Queue acknowledgement is not a terminal status.
            return thread.session.interrupt_task_with_receipt().await;
        } else if let Some(work) = self
            .work_ledger()?
            .observe_current(target)
            .await
            .map_err(super::work_lifecycle::work_error)?
            .filter(|work| !work.disposition.is_terminal())
        {
            self.interrupt_idle_work(work.stamp).await?;
            return Ok(Some(work.stamp));
        }
        Ok(None)
    }
}

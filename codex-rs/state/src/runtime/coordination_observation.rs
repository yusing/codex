use super::StateRuntime;
use super::work_from_row;
use crate::coordination::ActorWorkObservation;
use crate::coordination::WaitObservation;
use crate::coordination::WorkStamp;
use crate::coordination_wait::WaitMode;
use crate::coordination_wait::WaitPhase;
use codex_protocol::ThreadId;
use sqlx::Row;
use std::collections::HashMap;
use uuid::Uuid;

impl StateRuntime {
    pub async fn coordination_observe(
        &self,
        root: ThreadId,
        actors: &[ThreadId],
    ) -> anyhow::Result<Vec<ActorWorkObservation>> {
        let mut facts: HashMap<_, _> = actors
            .iter()
            .map(|actor| {
                (
                    *actor,
                    ActorWorkObservation {
                        actor_id: *actor,
                        work: None,
                        wait: None,
                    },
                )
            })
            .collect();
        let actors_json = serde_json::to_string(actors)?;
        let mut transaction = self.pool.begin().await?;
        let rows = sqlx::query("SELECT w.* FROM agent_current_work c JOIN agent_work w ON w.root_id=c.root_id AND w.actor_id=c.actor_id AND w.work_id=c.work_id WHERE c.root_id=? AND c.actor_id IN (SELECT value FROM json_each(?))")
            .bind(root.to_string()).bind(&actors_json).fetch_all(&mut *transaction).await?;
        for row in rows {
            let work = work_from_row(row)?;
            if let Some(fact) = facts.get_mut(&work.stamp.actor_id) {
                fact.work = Some(work);
            }
        }
        let rows = sqlx::query("SELECT wait_id,caller_id,mode,phase,turn_reason,timeout_effective_ms,deadline_unix_ms FROM agent_waits WHERE root_id=? AND caller_id IN (SELECT value FROM json_each(?)) AND phase NOT IN ('consumed','cancelled')")
            .bind(root.to_string()).bind(&actors_json).fetch_all(&mut *transaction).await?;
        let mut callers = HashMap::new();
        for row in rows {
            let actor = ThreadId::from_string(row.try_get("caller_id")?)?;
            let Some(fact) = facts.get_mut(&actor) else {
                continue;
            };
            let id = Uuid::parse_str(row.try_get("wait_id")?)?;
            callers.insert(id, actor);
            fact.wait = Some(WaitObservation {
                id,
                mode: match row.try_get::<&str, _>("mode")? {
                    "any" => WaitMode::Any,
                    "all" => WaitMode::All,
                    value => anyhow::bail!("invalid persisted wait mode: {value}"),
                },
                phase: WaitPhase::parse(row.try_get("phase")?)?,
                reason: row
                    .try_get::<Option<&str>, _>("turn_reason")?
                    .map(crate::coordination_wait::TurnWaitReason::parse)
                    .transpose()?,
                timeout_ms: row.try_get("timeout_effective_ms")?,
                deadline_unix_ms: row.try_get("deadline_unix_ms")?,
                selected: Vec::new(),
            });
        }
        let rows = sqlx::query("SELECT s.wait_id,s.actor_id,s.work_id FROM agent_wait_selection s JOIN agent_waits w ON w.wait_id=s.wait_id WHERE w.root_id=? AND w.caller_id IN (SELECT value FROM json_each(?)) AND w.phase NOT IN ('consumed','cancelled') ORDER BY s.wait_id,s.ordinal")
            .bind(root.to_string()).bind(&actors_json).fetch_all(&mut *transaction).await?;
        for row in rows {
            let id = Uuid::parse_str(row.try_get("wait_id")?)?;
            if let Some(wait) = callers
                .get(&id)
                .and_then(|actor| facts.get_mut(actor))
                .and_then(|fact| fact.wait.as_mut())
            {
                wait.selected.push(WorkStamp {
                    root_id: root,
                    actor_id: ThreadId::from_string(row.try_get("actor_id")?)?,
                    work_id: Uuid::parse_str(row.try_get("work_id")?)?,
                });
            }
        }
        transaction.commit().await?;
        Ok(actors
            .iter()
            .filter_map(|actor| facts.remove(actor))
            .collect())
    }
}

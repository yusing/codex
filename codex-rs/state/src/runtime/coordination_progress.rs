//! Progress facts extend the existing wait transaction; no independent scheduler.

use super::StateRuntime;
use super::coordination::current_work;
use super::coordination_wait::live_wait;
use crate::coordination::WorkDisposition;
use crate::coordination::WorkStamp;
use crate::coordination_progress::*;
use crate::coordination_wait::WaitError;
use crate::coordination_wait::WaitPhase;
use crate::sqlite::SqlitePoolWriteExt;
use codex_protocol::ThreadId;
use sqlx::Row;
use sqlx::SqliteConnection;
use std::collections::HashSet;
use uuid::Uuid;

impl StateRuntime {
    pub async fn coordination_capture_cursors(
        &self,
        root: ThreadId,
        recipient: ThreadId,
        context: Uuid,
    ) -> anyhow::Result<Vec<CaptureCursor>> {
        let rows = sqlx::query(
            "SELECT capture_key,through_chunk FROM agent_capture_cursors
            WHERE root_id=? AND recipient_id=? AND context_id=? ORDER BY capture_key",
        )
        .bind(root.to_string())
        .bind(recipient.to_string())
        .bind(context.to_string())
        .fetch_all(self.pool.as_ref())
        .await?;
        rows.into_iter()
            .map(|row| {
                Ok(CaptureCursor {
                    key: row.try_get("capture_key")?,
                    through_chunk: u64::try_from(row.try_get::<i64, _>("through_chunk")?)?,
                })
            })
            .collect()
    }

    /// Reserve before host I/O so duplicate scheduler notifications cannot freeze
    /// different publications or issue duplicate inference for the same interval.
    pub async fn coordination_begin_progress(
        &self,
        caller: WorkStamp,
        runtime: Uuid,
        wait: Uuid,
        context: Uuid,
    ) -> anyhow::Result<Option<ProgressBatch>> {
        let mut tx = self.pool.begin_immediate().await?;
        let ticket = live_wait(&mut tx, caller, runtime, wait).await?;
        if !matches!(ticket.phase, WaitPhase::Registered)
            || ticket.reset.blocks_delivery()
            || ticket.options.progress_interval_ms.is_none()
        {
            return Ok(None);
        }
        if read_batch(&mut tx, wait).await?.is_some() {
            return Ok(None);
        }
        let batch = ProgressBatch {
            wait_id: wait,
            batch_id: Uuid::now_v7(),
            recipient_context_id: context,
            frozen: None,
            output: None,
        };
        sqlx::query(
            "INSERT INTO agent_progress_batches(wait_id,batch_id,context_id) VALUES(?,?,?)",
        )
        .bind(wait.to_string())
        .bind(batch.batch_id.to_string())
        .bind(context.to_string())
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(Some(batch))
    }

    pub async fn coordination_progress_batch(
        &self,
        caller: WorkStamp,
        runtime: Uuid,
        wait: Uuid,
    ) -> anyhow::Result<Option<ProgressBatch>> {
        let mut tx = self.pool.begin().await?;
        live_wait(&mut tx, caller, runtime, wait).await?;
        read_batch(&mut tx, wait).await
    }

    /// Only selected work may contribute. Empty intervals do not create slots.
    pub async fn coordination_freeze_progress(
        &self,
        caller: WorkStamp,
        runtime: Uuid,
        wait: Uuid,
        batch: Uuid,
        frozen: &FrozenProgress,
    ) -> anyhow::Result<bool> {
        let mut tx = self.pool.begin_immediate().await?;
        let ticket = live_wait(&mut tx, caller, runtime, wait).await?;
        if !matches!(ticket.phase, WaitPhase::Registered) || ticket.reset.blocks_delivery() {
            return Ok(false);
        }
        let Some(retained) = read_batch(&mut tx, wait).await? else {
            return Ok(false);
        };
        if retained.batch_id != batch || retained.frozen.is_some() {
            return Ok(false);
        }
        if frozen.agents.is_empty()
            && frozen.coverage.status == "complete"
            && frozen.coverage.diagnostics.is_empty()
        {
            sqlx::query("DELETE FROM agent_progress_batches WHERE wait_id=?")
                .bind(wait.to_string())
                .execute(&mut *tx)
                .await?;
            tx.commit().await?;
            return Ok(false);
        }
        anyhow::ensure!(
            !frozen.snapshot_ref.is_empty(),
            "empty progress snapshot reference"
        );
        let mut actors = HashSet::new();
        let mut keys = HashSet::new();
        for agent in &frozen.agents {
            anyhow::ensure!(
                !agent.intervals.is_empty(),
                "progress actor has no changed capture intervals"
            );
            anyhow::ensure!(
                ticket
                    .selected
                    .iter()
                    .any(|work| work.actor_id == agent.actor.thread_id
                        && work.work_id == agent.actor.work_id),
                "progress resource returned unselected work"
            );
            anyhow::ensure!(
                actors.insert((agent.actor.thread_id, agent.actor.work_id)),
                "duplicate progress actor"
            );
            for interval in &agent.intervals {
                anyhow::ensure!(
                    !interval.key.is_empty()
                        && !interval.change_id.is_empty()
                        && interval.first_chunk > 0
                        && interval.last_chunk >= interval.first_chunk
                        && i64::try_from(interval.last_chunk).is_ok()
                        && keys.insert(&interval.key),
                    "invalid or duplicate capture interval"
                );
            }
        }
        sqlx::query("UPDATE agent_progress_batches SET frozen=? WHERE batch_id=?")
            .bind(serde_json::to_string(frozen)?)
            .bind(batch.to_string())
            .execute(&mut *tx)
            .await?;
        for (ordinal, agent) in frozen
            .agents
            .iter()
            .filter(|agent| !agent.intervals.is_empty())
            .enumerate()
        {
            sqlx::query("INSERT INTO agent_progress_slots(batch_id,actor_id,work_id,ordinal) VALUES(?,?,?,?)")
                .bind(batch.to_string()).bind(agent.actor.thread_id.to_string()).bind(agent.actor.work_id.to_string())
                .bind(i64::try_from(ordinal)?).execute(&mut *tx).await?;
        }
        tx.commit().await?;
        Ok(true)
    }

    pub async fn coordination_retire_progress(
        &self,
        caller: WorkStamp,
        runtime: Uuid,
        wait: Uuid,
        batch: Uuid,
    ) -> anyhow::Result<()> {
        let mut tx = self.pool.begin_immediate().await?;
        let ticket = live_wait(&mut tx, caller, runtime, wait).await?;
        anyhow::ensure!(ticket.dispatch_id.is_none(), WaitError::InvalidPhase);
        sqlx::query("DELETE FROM agent_progress_batches WHERE wait_id=? AND batch_id=?")
            .bind(wait.to_string())
            .bind(batch.to_string())
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(())
    }

    /// Called only at the actual target's safe post-tool boundary. The slot's
    /// started bit is an execution receipt, not a restart instruction on load.
    pub async fn coordination_take_progress_probes(
        &self,
        target: WorkStamp,
        runtime: Uuid,
    ) -> anyhow::Result<Vec<SummaryProbe>> {
        let mut tx = self.pool.begin_immediate().await?;
        let work = current_work(&mut tx, target.root_id, target.actor_id).await?;
        if work.is_none_or(|work| {
            work.stamp != target
                || work.runtime_id != runtime
                || !matches!(
                    work.disposition,
                    WorkDisposition::Running | WorkDisposition::Waiting
                )
        }) {
            return Ok(Vec::new());
        }
        let completing: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM agent_work_completion WHERE root_id=? AND actor_id=? AND work_id=?)")
            .bind(target.root_id.to_string()).bind(target.actor_id.to_string()).bind(target.work_id.to_string())
            .fetch_one(&mut *tx).await?;
        if completing {
            return Ok(Vec::new());
        }
        let rows = sqlx::query(
            "SELECT b.wait_id,b.batch_id,b.frozen,w.caller_id,w.caller_work_id
            FROM agent_progress_slots s JOIN agent_progress_batches b ON b.batch_id=s.batch_id
            JOIN agent_waits w ON w.wait_id=b.wait_id WHERE s.actor_id=? AND s.work_id=?
            AND s.started=0 AND s.result IS NULL AND w.root_id=? AND w.runtime_id=?
            AND w.phase='registered' AND w.reset_state IN ('not_requested','completed','failed')",
        )
        .bind(target.actor_id.to_string())
        .bind(target.work_id.to_string())
        .bind(target.root_id.to_string())
        .bind(runtime.to_string())
        .fetch_all(&mut *tx)
        .await?;
        let mut probes = Vec::with_capacity(rows.len());
        for row in rows {
            let batch = Uuid::parse_str(row.try_get("batch_id")?)?;
            let frozen: FrozenProgress = serde_json::from_str(row.try_get("frozen")?)?;
            let agent = frozen
                .agents
                .into_iter()
                .find(|agent| {
                    agent.actor.thread_id == target.actor_id
                        && agent.actor.work_id == target.work_id
                })
                .ok_or_else(|| anyhow::anyhow!("progress slot has no frozen actor"))?;
            probes.push(SummaryProbe {
                caller: WorkStamp {
                    root_id: target.root_id,
                    actor_id: ThreadId::from_string(row.try_get("caller_id")?)?,
                    work_id: Uuid::parse_str(row.try_get("caller_work_id")?)?,
                },
                wait_id: Uuid::parse_str(row.try_get("wait_id")?)?,
                batch_id: batch,
                target: agent,
            });
            sqlx::query("UPDATE agent_progress_slots SET started=1 WHERE batch_id=? AND actor_id=? AND work_id=?")
                .bind(batch.to_string()).bind(target.actor_id.to_string()).bind(target.work_id.to_string())
                .execute(&mut *tx).await?;
        }
        tx.commit().await?;
        Ok(probes)
    }

    pub async fn coordination_resolve_progress(
        &self,
        runtime: Uuid,
        probe: &SummaryProbe,
        result: &SummaryResult,
    ) -> anyhow::Result<bool> {
        match result {
            SummaryResult::Available { text } => {
                anyhow::ensure!(!text.trim().is_empty(), "empty progress summary")
            }
            SummaryResult::Unavailable { reason } => {
                anyhow::ensure!(!reason.trim().is_empty(), "empty progress failure")
            }
        }
        let mut tx = self.pool.begin_immediate().await?;
        let ticket = live_wait(&mut tx, probe.caller, runtime, probe.wait_id).await?;
        if !matches!(ticket.phase, WaitPhase::Registered) {
            return Ok(false);
        }
        let Some(batch) = read_batch(&mut tx, probe.wait_id).await? else {
            return Ok(false);
        };
        if batch.batch_id != probe.batch_id {
            return Ok(false);
        }
        let target =
            current_work(&mut tx, probe.caller.root_id, probe.target.actor.thread_id).await?;
        let unavailable = SummaryResult::Unavailable {
            reason: "selected work settled or was replaced".into(),
        };
        let result = if target.is_none_or(|work| {
            work.stamp.work_id != probe.target.actor.work_id
                || !matches!(
                    work.disposition,
                    WorkDisposition::Running | WorkDisposition::Waiting
                )
        }) {
            &unavailable
        } else {
            result
        };
        let changed = sqlx::query(
            "UPDATE agent_progress_slots SET result=?
            WHERE batch_id=? AND actor_id=? AND work_id=? AND result IS NULL",
        )
        .bind(serde_json::to_string(result)?)
        .bind(probe.batch_id.to_string())
        .bind(probe.target.actor.thread_id.to_string())
        .bind(probe.target.actor.work_id.to_string())
        .execute(&mut *tx)
        .await?
        .rows_affected();
        tx.commit().await?;
        Ok(changed != 0)
    }

    /// Terminal/replaced work settles only its slot. In an all-wait it cannot
    /// accidentally settle other selected work or claim the wait itself.
    pub async fn coordination_progress_agents(
        &self,
        caller: WorkStamp,
        runtime: Uuid,
        wait: Uuid,
        batch_id: Uuid,
    ) -> anyhow::Result<Option<Vec<PreparedProgressAgent>>> {
        let mut tx = self.pool.begin_immediate().await?;
        let ticket = live_wait(&mut tx, caller, runtime, wait).await?;
        if !matches!(ticket.phase, WaitPhase::Registered) || ticket.reset.blocks_delivery() {
            return Ok(None);
        }
        let Some(batch) = read_batch(&mut tx, wait).await? else {
            return Ok(None);
        };
        if batch.batch_id != batch_id {
            return Ok(None);
        }
        let Some(frozen) = batch.frozen else {
            return Ok(None);
        };
        let mut prepared = Vec::new();
        let mut pending = false;
        for agent in frozen
            .agents
            .into_iter()
            .filter(|agent| !agent.intervals.is_empty())
        {
            let result: Option<String> = sqlx::query_scalar(
                "SELECT result FROM agent_progress_slots
                WHERE batch_id=? AND actor_id=? AND work_id=?",
            )
            .bind(batch_id.to_string())
            .bind(agent.actor.thread_id.to_string())
            .bind(agent.actor.work_id.to_string())
            .fetch_one(&mut *tx)
            .await?;
            let result = if let Some(result) = result {
                serde_json::from_str(&result)?
            } else {
                let current = current_work(&mut tx, caller.root_id, agent.actor.thread_id).await?;
                let completing: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM agent_work_completion WHERE root_id=? AND actor_id=? AND work_id=?)")
                    .bind(caller.root_id.to_string()).bind(agent.actor.thread_id.to_string()).bind(agent.actor.work_id.to_string())
                    .fetch_one(&mut *tx).await?;
                if !completing
                    && current.is_some_and(|work| {
                        work.stamp.work_id == agent.actor.work_id
                            && matches!(
                                work.disposition,
                                WorkDisposition::PendingStart
                                    | WorkDisposition::Running
                                    | WorkDisposition::Waiting
                            )
                    })
                {
                    pending = true;
                    continue;
                }
                let result = SummaryResult::Unavailable {
                    reason: if completing {
                        "selected work already authored its final"
                    } else {
                        "selected work settled or was replaced"
                    }
                    .into(),
                };
                sqlx::query("UPDATE agent_progress_slots SET result=? WHERE batch_id=? AND actor_id=? AND work_id=?")
                    .bind(serde_json::to_string(&result)?).bind(batch_id.to_string())
                    .bind(agent.actor.thread_id.to_string()).bind(agent.actor.work_id.to_string())
                    .execute(&mut *tx).await?;
                result
            };
            prepared.push(PreparedProgressAgent {
                actor: agent.actor,
                intervals: agent.intervals,
                summary: result,
            });
        }
        tx.commit().await?;
        Ok((!pending).then_some(prepared))
    }

    pub async fn coordination_prepare_progress_output(
        &self,
        caller: WorkStamp,
        runtime: Uuid,
        wait: Uuid,
        batch_id: Uuid,
        output: &PreparedProgressOutput,
    ) -> anyhow::Result<bool> {
        let mut tx = self.pool.begin_immediate().await?;
        let ticket = live_wait(&mut tx, caller, runtime, wait).await?;
        if !matches!(ticket.phase, WaitPhase::Registered) || ticket.reset.blocks_delivery() {
            return Ok(false);
        }
        let Some(batch) = read_batch(&mut tx, wait).await? else {
            return Ok(false);
        };
        if batch.batch_id != batch_id || batch.output.is_some() {
            return Ok(false);
        }
        let Some(frozen) = batch.frozen else {
            return Ok(false);
        };
        let unresolved: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM agent_progress_slots WHERE batch_id=? AND result IS NULL)",
        )
        .bind(batch_id.to_string())
        .fetch_one(&mut *tx)
        .await?;
        anyhow::ensure!(
            !unresolved && !output.text.is_empty(),
            "progress output is not ready"
        );
        let intervals: Vec<_> = frozen
            .agents
            .iter()
            .flat_map(|agent| &agent.intervals)
            .collect();
        anyhow::ensure!(
            output
                .delivered_intervals
                .iter()
                .all(|interval| intervals.contains(&interval)),
            "progress acknowledgement exceeds frozen evidence"
        );
        sqlx::query("UPDATE agent_progress_batches SET output=? WHERE batch_id=?")
            .bind(serde_json::to_string(output)?)
            .bind(batch_id.to_string())
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(true)
    }
}

pub(super) async fn read_batch(
    conn: &mut SqliteConnection,
    wait: Uuid,
) -> anyhow::Result<Option<ProgressBatch>> {
    let row = sqlx::query("SELECT * FROM agent_progress_batches WHERE wait_id=?")
        .bind(wait.to_string())
        .fetch_optional(conn)
        .await?;
    row.map(|row| {
        Ok(ProgressBatch {
            wait_id: wait,
            batch_id: Uuid::parse_str(row.try_get("batch_id")?)?,
            recipient_context_id: Uuid::parse_str(row.try_get("context_id")?)?,
            frozen: row
                .try_get::<Option<&str>, _>("frozen")?
                .map(serde_json::from_str)
                .transpose()?,
            output: row
                .try_get::<Option<&str>, _>("output")?
                .map(serde_json::from_str)
                .transpose()?,
        })
    })
    .transpose()
}

/// The caller already proved the complete frozen item and the provider accepted
/// this attempt. The live context token must still equal its materialization.
pub(super) async fn acknowledge_progress(
    conn: &mut SqliteConnection,
    caller: WorkStamp,
    wait: Uuid,
    context: Uuid,
) -> anyhow::Result<()> {
    let batch = read_batch(conn, wait)
        .await?
        .ok_or(WaitError::InvalidPhase)?;
    // An accepted old-context projection cannot establish suppression in a new
    // context. Consuming it is safe; the next interval still sees the full prefix.
    if batch.recipient_context_id != context {
        return Ok(());
    }
    let output = batch.output.ok_or(WaitError::InvalidPhase)?;
    for interval in output.delivered_intervals {
        // Never skip an unseen prefix, even if a malformed host response claims
        // a later interval. The resource selection must continue an exact cursor.
        let previous: Option<i64> = sqlx::query_scalar(
            "SELECT through_chunk FROM agent_capture_cursors
            WHERE root_id=? AND recipient_id=? AND context_id=? AND capture_key=?",
        )
        .bind(caller.root_id.to_string())
        .bind(caller.actor_id.to_string())
        .bind(context.to_string())
        .bind(&interval.key)
        .fetch_optional(&mut *conn)
        .await?;
        let previous = u64::try_from(previous.unwrap_or(0))?;
        anyhow::ensure!(
            interval.first_chunk <= previous.saturating_add(1),
            "capture acknowledgement skips an unseen prefix"
        );
        sqlx::query("INSERT INTO agent_capture_cursors(root_id,recipient_id,context_id,capture_key,through_chunk)
            VALUES(?,?,?,?,?) ON CONFLICT(root_id,recipient_id,context_id,capture_key)
            DO UPDATE SET through_chunk=MAX(through_chunk,excluded.through_chunk)")
            .bind(caller.root_id.to_string()).bind(caller.actor_id.to_string()).bind(context.to_string())
            .bind(&interval.key).bind(i64::try_from(interval.last_chunk)?).execute(&mut *conn).await?;
    }
    Ok(())
}

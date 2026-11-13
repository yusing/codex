use super::StateRuntime;
use super::coordination::current_work;
use super::coordination::insert_delivery;
use super::coordination::read_work;
use super::coordination::write_work;
use crate::coordination::CompletionCandidate;
use crate::coordination::CoordinationError;
use crate::coordination::DeliveryKind;
use crate::coordination::DeliveryRecord;
use crate::coordination::PublishDelivery;
use crate::coordination::WorkDisposition;
use crate::coordination::WorkRecord;
use crate::coordination::WorkStamp;
use crate::coordination_wait::WaitTicket;
use crate::sqlite::SqlitePoolWriteExt;
use codex_protocol::ThreadId;
use sqlx::Row;
use sqlx::SqliteConnection;
use uuid::Uuid;

impl StateRuntime {
    /// Observe the pending candidate's current blockers without admitting input.
    pub async fn coordination_completion_blockers(
        &self,
        stamp: WorkStamp,
        runtime: Uuid,
        id: Uuid,
    ) -> anyhow::Result<Option<Vec<WorkStamp>>> {
        let mut tx = self.pool.begin().await?;
        if !completion_continuation_ready(&mut tx, stamp, runtime, id).await? {
            return Ok(None);
        }
        Ok(Some(active_completion_blockers(&mut tx, stamp).await?))
    }

    /// The same native delivery fence used by ordinary inputs. A changed blocker
    /// snapshot retries preparation; settled blockers preserve the original final.
    pub async fn coordination_begin_completion_continuation(
        &self,
        stamp: WorkStamp,
        runtime: Uuid,
        id: Uuid,
        expected: &[WorkStamp],
        publication: &PublishDelivery,
        receipt: Option<&str>,
    ) -> anyhow::Result<bool> {
        anyhow::ensure!(
            !expected.is_empty()
                && publication.root_id == stamp.root_id
                && publication.recipient_id == stamp.actor_id
                && publication.sender_id.is_none()
                && publication.work == Some(stamp)
                && publication.kind == DeliveryKind::Message
                && receipt.is_none_or(|receipt| !receipt.is_empty()),
            CoordinationError::InvalidDelivery
        );
        let mut tx = self.pool.begin_immediate().await?;
        if !completion_continuation_ready(&mut tx, stamp, runtime, id).await?
            || active_completion_blockers(&mut tx, stamp).await? != expected
        {
            return Ok(false);
        }
        insert_delivery(&mut tx, publication).await?;
        sqlx::query(
            "UPDATE agent_deliveries SET dispatch_id=?,accepted_receipt=? WHERE delivery_id=?",
        )
        .bind(publication.id.to_string())
        .bind(receipt)
        .bind(publication.id.to_string())
        .execute(&mut *tx)
        .await?;
        sqlx::query(
            "DELETE FROM agent_work_completion WHERE root_id=? AND actor_id=? AND work_id=?",
        )
        .bind(stamp.root_id.to_string())
        .bind(stamp.actor_id.to_string())
        .bind(stamp.work_id.to_string())
        .execute(&mut *tx)
        .await?;
        if let Some(receipt) = receipt {
            bind_turn_in_transaction(&mut tx, stamp, receipt).await?;
            let mut work = current_work(&mut tx, stamp.root_id, stamp.actor_id)
                .await?
                .ok_or(CoordinationError::UnknownWork)?;
            work.disposition = WorkDisposition::Running;
            write_work(&mut tx, &mut work).await?;
        }
        tx.commit().await?;
        Ok(true)
    }

    pub async fn coordination_bind_turn(
        &self,
        stamp: WorkStamp,
        runtime_id: Uuid,
        turn_id: &str,
    ) -> anyhow::Result<()> {
        let mut tx = self.pool.begin_immediate().await?;
        let work = current_work(&mut tx, stamp.root_id, stamp.actor_id)
            .await?
            .ok_or(CoordinationError::UnknownWork)?;
        anyhow::ensure!(
            work.stamp == stamp && work.runtime_id == runtime_id && !work.disposition.is_terminal(),
            CoordinationError::StaleWork
        );
        bind_turn_in_transaction(&mut tx, stamp, turn_id).await?;
        tx.commit().await?;
        Ok(())
    }

    pub async fn coordination_work_for_turn(
        &self,
        root: ThreadId,
        actor: ThreadId,
        turn_id: &str,
    ) -> anyhow::Result<Option<WorkStamp>> {
        let work: Option<String> = sqlx::query_scalar(
            "SELECT work_id FROM agent_work_turns WHERE root_id=? AND actor_id=? AND turn_id=?",
        )
        .bind(root.to_string())
        .bind(actor.to_string())
        .bind(turn_id)
        .fetch_optional(self.pool.as_ref())
        .await?;
        work.map(|id| {
            Ok(WorkStamp {
                root_id: root,
                actor_id: actor,
                work_id: Uuid::parse_str(&id)?,
            })
        })
        .transpose()
    }

    /// Admit the exact authenticated IDs present in one concrete sampling input.
    /// Foreign/forked history cannot consume another recipient's deliveries.
    pub async fn coordination_consume_sampling(
        &self,
        root: ThreadId,
        actor: ThreadId,
        ids: &[Uuid],
    ) -> anyhow::Result<Vec<DeliveryRecord>> {
        let mut tx = self.pool.begin_immediate().await?;
        let mut result = Vec::with_capacity(ids.len());
        for id in ids {
            let Some(delivery) =
                super::coordination::read_delivery(&mut tx, root, actor, *id).await?
            else {
                continue;
            };
            // Frozen wait results are consumed only by the wait transaction after
            // the scheduler proves the complete claim-bearing output was included.
            if delivery.claim_id.is_some() {
                continue;
            }
            anyhow::ensure!(
                delivery.refused_reason.is_none(),
                CoordinationError::DeliveryConflict
            );
            let receipt = match &delivery.accepted_receipt {
                Some(receipt) => receipt.clone(),
                None => {
                    anyhow::ensure!(
                        delivery.dispatch_id == Some(*id),
                        CoordinationError::DeliveryConflict
                    );
                    id.to_string()
                }
            };
            sqlx::query(
                "UPDATE agent_deliveries SET accepted_receipt=?,consumed=1 WHERE delivery_id=?",
            )
            .bind(receipt)
            .bind(id.to_string())
            .execute(&mut *tx)
            .await?;
            result.push(delivery);
        }
        tx.commit().await?;
        Ok(result)
    }

    pub async fn coordination_has_pending_messages(
        &self,
        root: ThreadId,
        actor: ThreadId,
    ) -> anyhow::Result<bool> {
        Ok(sqlx::query_scalar(
            "SELECT NOT EXISTS(SELECT 1 FROM agent_waits WHERE root_id=? AND caller_id=? AND phase NOT IN ('consumed','cancelled'))
             AND EXISTS(SELECT 1 FROM agent_deliveries WHERE root_id=? AND recipient_id=? AND consumed=0 AND claim_id IS NULL AND dispatch_id IS NULL AND (kind='message' OR work_actor_id<>recipient_id))",
        ).bind(root.to_string()).bind(actor.to_string()).bind(root.to_string()).bind(actor.to_string())
            .fetch_one(self.pool.as_ref()).await?)
    }

    /// Normal native sampling and wait selection share the same transaction fence.
    /// This is not a second mailbox: retained payloads leave only by exact receipt.
    pub async fn coordination_take_messages(
        &self,
        root: ThreadId,
        actor: ThreadId,
    ) -> anyhow::Result<Vec<DeliveryRecord>> {
        let mut tx = self.pool.begin_immediate().await?;
        let waiting: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM agent_waits WHERE root_id=? AND caller_id=? AND phase NOT IN ('consumed','cancelled'))")
            .bind(root.to_string()).bind(actor.to_string()).fetch_one(&mut *tx).await?;
        if waiting {
            return Ok(Vec::new());
        }
        let ids: Vec<String> = sqlx::query_scalar("SELECT delivery_id FROM agent_deliveries WHERE root_id=? AND recipient_id=? AND consumed=0 AND claim_id IS NULL AND dispatch_id IS NULL AND (kind='message' OR work_actor_id<>recipient_id) ORDER BY sequence LIMIT 64")
            .bind(root.to_string()).bind(actor.to_string()).fetch_all(&mut *tx).await?;
        let mut result = Vec::with_capacity(ids.len());
        for id in ids {
            sqlx::query("UPDATE agent_deliveries SET dispatch_id=delivery_id,accepted_receipt=delivery_id WHERE delivery_id=?")
                .bind(&id).execute(&mut *tx).await?;
            result.push(
                super::coordination::read_delivery(&mut tx, root, actor, Uuid::parse_str(&id)?)
                    .await?
                    .ok_or(CoordinationError::UnknownDelivery)?,
            );
        }
        tx.commit().await?;
        Ok(result)
    }

    /// A concrete native admission decision, not a queue observation. Work creation,
    /// wait supersession, payload retention and the dispatch fence commit together.
    pub async fn coordination_admit_input(
        &self,
        input: &PublishDelivery,
        runtime_id: Uuid,
        starts_work: bool,
        expected_current: Option<WorkStamp>,
        wait: Option<&WaitTicket>,
        ancestors: &[ThreadId],
        superseded: Option<&PublishDelivery>,
    ) -> anyhow::Result<WorkRecord> {
        let stamp = input.work.ok_or(CoordinationError::InvalidDelivery)?;
        anyhow::ensure!(
            input.root_id == stamp.root_id
                && input.recipient_id == stamp.actor_id
                && input.kind == DeliveryKind::Message,
            CoordinationError::InvalidDelivery
        );
        let mut tx = self.pool.begin_immediate().await?;
        let current = current_work(&mut tx, stamp.root_id, stamp.actor_id).await?;
        anyhow::ensure!(
            current.as_ref().map(|work| work.stamp) == expected_current,
            CoordinationError::StaleWork
        );
        let new_work = starts_work
            || current
                .as_ref()
                .is_none_or(|work| work.disposition.is_terminal());
        if let Some(current) = current
            .as_ref()
            .filter(|work| !work.disposition.is_terminal())
        {
            anyhow::ensure!(
                current.runtime_id == runtime_id,
                CoordinationError::StaleWork
            );
            anyhow::ensure!(
                current.disposition != WorkDisposition::Uncertain,
                CoordinationError::ActiveWork
            );
        }
        if new_work {
            anyhow::ensure!(
                starts_work && stamp.work_id == input.id,
                CoordinationError::InvalidTransition
            );
        } else {
            let current = current.as_ref().ok_or(CoordinationError::UnknownWork)?;
            anyhow::ensure!(
                current.stamp == stamp && current.runtime_id == runtime_id,
                CoordinationError::StaleWork
            );
            anyhow::ensure!(
                current.disposition != WorkDisposition::Uncertain,
                CoordinationError::ActiveWork
            );
        }
        // The observed wait must still be exactly current. New or dispatched waits
        // reject rather than accepting input beside an unaccounted continuation.
        let active_wait: Option<String> = sqlx::query_scalar(
            "SELECT wait_id FROM agent_waits WHERE root_id=? AND caller_id=? AND phase NOT IN ('consumed','cancelled')",
        ).bind(stamp.root_id.to_string()).bind(stamp.actor_id.to_string())
            .fetch_optional(&mut *tx).await?;
        anyhow::ensure!(
            active_wait == wait.map(|wait| wait.id.to_string()),
            CoordinationError::StaleWork
        );
        if let Some(wait) = wait {
            super::coordination_wait::cancel_wait_with_cause_in_transaction(
                &mut tx,
                wait.caller,
                runtime_id,
                wait.id,
                &wait.origin,
                &crate::coordination_wait::WaitCancellation::InputSuperseded {
                    delivery_id: input.id,
                },
            )
            .await?;
        }
        if starts_work
            && let Some(previous) = current
                .as_ref()
                .filter(|work| !work.disposition.is_terminal())
        {
            let terminal = superseded.ok_or(CoordinationError::InvalidDelivery)?;
            anyhow::ensure!(
                previous.stamp != stamp
                    && terminal.work == Some(previous.stamp)
                    && terminal.root_id == stamp.root_id
                    && terminal.sender_id == Some(stamp.actor_id)
                    && terminal.kind == DeliveryKind::Terminal,
                CoordinationError::InvalidDelivery
            );
            insert_delivery(&mut tx, terminal).await?;
            let mut previous = previous.clone();
            previous.disposition = WorkDisposition::Interrupted;
            previous.terminal_delivery_id = Some(terminal.id);
            write_work(&mut tx, &mut previous).await?;
            sqlx::query(
                "DELETE FROM agent_work_completion WHERE root_id=? AND actor_id=? AND work_id=?",
            )
            .bind(previous.stamp.root_id.to_string())
            .bind(previous.stamp.actor_id.to_string())
            .bind(previous.stamp.work_id.to_string())
            .execute(&mut *tx)
            .await?;
            sqlx::query(
                "DELETE FROM agent_work_answers WHERE root_id=? AND actor_id=? AND work_id=?",
            )
            .bind(previous.stamp.root_id.to_string())
            .bind(previous.stamp.actor_id.to_string())
            .bind(previous.stamp.work_id.to_string())
            .execute(&mut *tx)
            .await?;
        }
        let delivery = insert_delivery(&mut tx, input).await?;
        // A duplicate prepared dispatch never authorizes another enqueue.
        anyhow::ensure!(
            delivery.dispatch_id.is_none(),
            CoordinationError::DeliveryConflict
        );
        sqlx::query("UPDATE agent_deliveries SET dispatch_id=? WHERE delivery_id=?")
            .bind(input.id.to_string())
            .bind(input.id.to_string())
            .execute(&mut *tx)
            .await?;
        let work = if new_work {
            sqlx::query("INSERT INTO agent_work(root_id,actor_id,work_id,runtime_id,revision,disposition) VALUES(?,?,?,?,1,'pending_start')")
                .bind(stamp.root_id.to_string()).bind(stamp.actor_id.to_string()).bind(stamp.work_id.to_string())
                .bind(runtime_id.to_string()).execute(&mut *tx).await?;
            sqlx::query("INSERT INTO agent_current_work(root_id,actor_id,work_id) VALUES(?,?,?) ON CONFLICT(root_id,actor_id) DO UPDATE SET work_id=excluded.work_id")
                .bind(stamp.root_id.to_string()).bind(stamp.actor_id.to_string()).bind(stamp.work_id.to_string())
                .execute(&mut *tx).await?;
            WorkRecord {
                stamp,
                runtime_id,
                revision: 1,
                disposition: WorkDisposition::PendingStart,
                terminal_delivery_id: None,
            }
        } else {
            // Explicit input supersedes an undispatched completion attempt but not
            // its ordered ordinary answers. Checkpoints/wakes do not use this path.
            sqlx::query(
                "DELETE FROM agent_work_completion WHERE root_id=? AND actor_id=? AND work_id=?",
            )
            .bind(stamp.root_id.to_string())
            .bind(stamp.actor_id.to_string())
            .bind(stamp.work_id.to_string())
            .execute(&mut *tx)
            .await?;
            current.ok_or(CoordinationError::UnknownWork)?
        };
        for ancestor in ancestors {
            sqlx::query(
                "INSERT OR IGNORE INTO agent_completion_blockers(root_id,actor_id,work_id,blocker_actor_id,blocker_work_id)
                 SELECT root_id,actor_id,work_id,?,? FROM agent_work_completion WHERE root_id=? AND actor_id=? AND disposition='completed'",
            ).bind(stamp.actor_id.to_string()).bind(stamp.work_id.to_string())
                .bind(stamp.root_id.to_string()).bind(ancestor.to_string()).execute(&mut *tx).await?;
        }
        tx.commit().await?;
        Ok(work)
    }

    pub async fn coordination_record_answer(
        &self,
        stamp: WorkStamp,
        runtime_id: Uuid,
        item_id: &str,
        content: &str,
    ) -> anyhow::Result<()> {
        let mut tx = self.pool.begin_immediate().await?;
        let work = current_work(&mut tx, stamp.root_id, stamp.actor_id)
            .await?
            .ok_or(CoordinationError::UnknownWork)?;
        anyhow::ensure!(
            work.stamp == stamp && work.runtime_id == runtime_id && !work.disposition.is_terminal(),
            CoordinationError::StaleWork
        );
        if content.is_empty() {
            return Ok(());
        }
        let previous: Option<String> = sqlx::query_scalar(
            "SELECT content FROM agent_work_answers WHERE root_id=? AND actor_id=? AND work_id=? AND item_id=?",
        ).bind(stamp.root_id.to_string()).bind(stamp.actor_id.to_string()).bind(stamp.work_id.to_string()).bind(item_id)
            .fetch_optional(&mut *tx).await?;
        if let Some(previous) = previous {
            anyhow::ensure!(previous == content, CoordinationError::DeliveryConflict);
            return Ok(());
        }
        sqlx::query("INSERT INTO agent_work_answers(root_id,actor_id,work_id,item_id,content) VALUES(?,?,?,?,?)")
            .bind(stamp.root_id.to_string()).bind(stamp.actor_id.to_string()).bind(stamp.work_id.to_string())
            .bind(item_id).bind(content).execute(&mut *tx).await?;
        tx.commit().await?;
        Ok(())
    }

    pub async fn coordination_stage_completion(
        &self,
        stamp: WorkStamp,
        runtime_id: Uuid,
        turn_id: &str,
        recipient: ThreadId,
        disposition: WorkDisposition,
        detail: Option<&str>,
        descendants: &[ThreadId],
    ) -> anyhow::Result<Option<CompletionCandidate>> {
        anyhow::ensure!(
            disposition.is_terminal(),
            CoordinationError::InvalidTransition
        );
        let mut tx = self.pool.begin_immediate().await?;
        let work = current_work(&mut tx, stamp.root_id, stamp.actor_id)
            .await?
            .ok_or(CoordinationError::UnknownWork)?;
        anyhow::ensure!(
            work.stamp == stamp && work.runtime_id == runtime_id,
            CoordinationError::StaleWork
        );
        if work.disposition.is_terminal() {
            return Ok(None);
        }
        if let Some(candidate) = candidate(&mut tx, stamp).await? {
            if candidate.disposition == WorkDisposition::Completed
                && disposition != WorkDisposition::Completed
            {
                sqlx::query("DELETE FROM agent_work_completion WHERE root_id=? AND actor_id=? AND work_id=?")
                    .bind(stamp.root_id.to_string()).bind(stamp.actor_id.to_string()).bind(stamp.work_id.to_string())
                    .execute(&mut *tx).await?;
            } else {
                anyhow::ensure!(
                    candidate.turn_id == turn_id && candidate.disposition == disposition,
                    CoordinationError::StaleWork
                );
                return Ok(Some(candidate));
            }
        }
        sqlx::query("INSERT INTO agent_work_completion(root_id,actor_id,work_id,delivery_id,recipient_id,turn_id,disposition,detail,capture_ready) VALUES(?,?,?,?,?,?,?,?,?)")
            .bind(stamp.root_id.to_string()).bind(stamp.actor_id.to_string()).bind(stamp.work_id.to_string())
            .bind(Uuid::now_v7().to_string()).bind(recipient.to_string()).bind(turn_id)
            .bind(disposition.as_str()).bind(detail).bind(disposition != WorkDisposition::Completed)
            .execute(&mut *tx).await?;
        if disposition == WorkDisposition::Completed {
            for descendant in descendants {
                sqlx::query(
                    "INSERT INTO agent_completion_blockers(root_id,actor_id,work_id,blocker_actor_id,blocker_work_id)
                     SELECT ?,?,?,actor_id,work_id FROM agent_current_work c JOIN agent_work w USING(root_id,actor_id,work_id)
                     WHERE c.root_id=? AND c.actor_id=? AND w.disposition IN ('pending_start','running','waiting')",
                ).bind(stamp.root_id.to_string()).bind(stamp.actor_id.to_string()).bind(stamp.work_id.to_string())
                    .bind(stamp.root_id.to_string()).bind(descendant.to_string()).execute(&mut *tx).await?;
            }
            if work.disposition != WorkDisposition::Waiting {
                let mut work = work;
                work.disposition = WorkDisposition::Waiting;
                write_work(&mut tx, &mut work).await?;
            }
        }
        let result = candidate(&mut tx, stamp).await?;
        tx.commit().await?;
        Ok(result)
    }

    pub async fn coordination_pending_completion(
        &self,
        stamp: WorkStamp,
    ) -> anyhow::Result<Option<CompletionCandidate>> {
        let mut conn = self.pool.acquire().await?;
        candidate(&mut conn, stamp).await
    }

    pub async fn coordination_refuse_delivery(
        &self,
        root: ThreadId,
        recipient: ThreadId,
        id: Uuid,
        reason: &str,
    ) -> anyhow::Result<()> {
        let changed = sqlx::query("UPDATE agent_deliveries SET refused_reason=? WHERE root_id=? AND recipient_id=? AND delivery_id=? AND dispatch_id=? AND accepted_receipt IS NULL AND (refused_reason IS NULL OR refused_reason=?)")
            .bind(reason).bind(root.to_string()).bind(recipient.to_string()).bind(id.to_string())
            .bind(id.to_string()).bind(reason).execute(self.pool.as_ref()).await?.rows_affected();
        anyhow::ensure!(changed == 1, CoordinationError::DeliveryConflict);
        Ok(())
    }

    pub async fn coordination_complete_capture(
        &self,
        stamp: WorkStamp,
        delivery_id: Uuid,
        capture: Option<&str>,
    ) -> anyhow::Result<()> {
        let changed = sqlx::query("UPDATE agent_work_completion SET capture_ready=1,capture=? WHERE root_id=? AND actor_id=? AND work_id=? AND delivery_id=? AND capture_ready=0")
            .bind(capture).bind(stamp.root_id.to_string()).bind(stamp.actor_id.to_string()).bind(stamp.work_id.to_string())
            .bind(delivery_id.to_string()).execute(self.pool.as_ref()).await?.rows_affected();
        anyhow::ensure!(changed == 1, CoordinationError::StaleWork);
        Ok(())
    }

    pub async fn coordination_completion_settlement(
        &self,
        stamp: WorkStamp,
        runtime_id: Uuid,
        delivery_id: Uuid,
        settled: bool,
    ) -> anyhow::Result<bool> {
        let mut tx = self.pool.begin_immediate().await?;
        let work = current_work(&mut tx, stamp.root_id, stamp.actor_id)
            .await?
            .ok_or(CoordinationError::UnknownWork)?;
        anyhow::ensure!(
            work.stamp == stamp && work.runtime_id == runtime_id,
            CoordinationError::StaleWork
        );
        let Some(candidate) = candidate(&mut tx, stamp).await? else {
            return Ok(false);
        };
        anyhow::ensure!(
            candidate.delivery_id == delivery_id,
            CoordinationError::StaleWork
        );
        sqlx::query("UPDATE agent_work_completion SET execution_settled=? WHERE root_id=? AND actor_id=? AND work_id=?")
            .bind(settled).bind(stamp.root_id.to_string()).bind(stamp.actor_id.to_string()).bind(stamp.work_id.to_string())
            .execute(&mut *tx).await?;
        if !settled
            && !work.disposition.is_terminal()
            && work.disposition != WorkDisposition::Waiting
        {
            let mut work = work;
            work.disposition = WorkDisposition::Waiting;
            write_work(&mut tx, &mut work).await?;
        }
        tx.commit().await?;
        Ok(true)
    }

    pub async fn coordination_capture_failed(
        &self,
        stamp: WorkStamp,
        id: Uuid,
        diagnostic: &str,
    ) -> anyhow::Result<()> {
        sqlx::query(
            "UPDATE agent_work_completion SET detail=? WHERE root_id=? AND actor_id=? AND work_id=?
             AND delivery_id=? AND disposition='completed' AND capture_ready=0",
        )
        .bind(diagnostic)
        .bind(stamp.root_id.to_string())
        .bind(stamp.actor_id.to_string())
        .bind(stamp.work_id.to_string())
        .bind(id.to_string())
        .execute(self.pool.as_ref())
        .await?;
        Ok(())
    }

    pub async fn coordination_finish_completion(
        &self,
        stamp: WorkStamp,
        runtime_id: Uuid,
        delivery_id: Uuid,
        payload: &str,
    ) -> anyhow::Result<Option<WorkRecord>> {
        let mut tx = self.pool.begin_immediate().await?;
        let Some(candidate) = candidate(&mut tx, stamp).await? else {
            return Ok(None);
        };
        anyhow::ensure!(
            candidate.delivery_id == delivery_id,
            CoordinationError::StaleWork
        );
        if !candidate.capture_ready {
            return Ok(None);
        }
        let execution_settled: bool = sqlx::query_scalar(
            "SELECT execution_settled FROM agent_work_completion WHERE root_id=? AND actor_id=? AND work_id=?",
        ).bind(stamp.root_id.to_string()).bind(stamp.actor_id.to_string()).bind(stamp.work_id.to_string())
            .fetch_one(&mut *tx).await?;
        if !execution_settled {
            return Ok(None);
        }
        let blocked: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM agent_completion_blockers b JOIN agent_work w
             ON w.root_id=b.root_id AND w.actor_id=b.blocker_actor_id AND w.work_id=b.blocker_work_id
             WHERE b.root_id=? AND b.actor_id=? AND b.work_id=? AND w.disposition IN ('pending_start','running','waiting'))",
        ).bind(stamp.root_id.to_string()).bind(stamp.actor_id.to_string()).bind(stamp.work_id.to_string())
            .fetch_one(&mut *tx).await?;
        if blocked {
            return Ok(None);
        }
        let mut work = current_work(&mut tx, stamp.root_id, stamp.actor_id)
            .await?
            .ok_or(CoordinationError::UnknownWork)?;
        anyhow::ensure!(
            work.stamp == stamp && work.runtime_id == runtime_id && !work.disposition.is_terminal(),
            CoordinationError::StaleWork
        );
        insert_delivery(
            &mut tx,
            &PublishDelivery {
                id: delivery_id,
                root_id: stamp.root_id,
                sender_id: Some(stamp.actor_id),
                recipient_id: candidate.recipient_id,
                work: Some(stamp),
                kind: DeliveryKind::Terminal,
                payload: payload.to_owned(),
            },
        )
        .await?;
        super::coordination_wait::retire_work_wait_in_transaction(
            &mut tx,
            stamp,
            runtime_id,
            delivery_id,
        )
        .await?;
        work.disposition = candidate.disposition;
        work.terminal_delivery_id = Some(delivery_id);
        write_work(&mut tx, &mut work).await?;
        sqlx::query(
            "DELETE FROM agent_work_completion WHERE root_id=? AND actor_id=? AND work_id=?",
        )
        .bind(stamp.root_id.to_string())
        .bind(stamp.actor_id.to_string())
        .bind(stamp.work_id.to_string())
        .execute(&mut *tx)
        .await?;
        sqlx::query("DELETE FROM agent_work_answers WHERE root_id=? AND actor_id=? AND work_id=?")
            .bind(stamp.root_id.to_string())
            .bind(stamp.actor_id.to_string())
            .bind(stamp.work_id.to_string())
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(Some(work))
    }

    pub async fn coordination_completion_dependents(
        &self,
        stamp: WorkStamp,
    ) -> anyhow::Result<Vec<WorkStamp>> {
        let rows = sqlx::query("SELECT actor_id,work_id FROM agent_completion_blockers WHERE root_id=? AND blocker_actor_id=? AND blocker_work_id=?")
            .bind(stamp.root_id.to_string()).bind(stamp.actor_id.to_string()).bind(stamp.work_id.to_string())
            .fetch_all(self.pool.as_ref()).await?;
        rows.into_iter()
            .map(|row| {
                Ok(WorkStamp {
                    root_id: stamp.root_id,
                    actor_id: ThreadId::from_string(row.try_get("actor_id")?)?,
                    work_id: Uuid::parse_str(row.try_get("work_id")?)?,
                })
            })
            .collect()
    }

    /// Observes an exact terminal result without claiming its addressed mailbox.
    pub async fn coordination_terminal_delivery(
        &self,
        stamp: WorkStamp,
    ) -> anyhow::Result<Option<DeliveryRecord>> {
        let mut conn = self.pool.acquire().await?;
        let Some(work) = read_work(&mut conn, stamp).await? else {
            return Ok(None);
        };
        let Some(id) = work.terminal_delivery_id else {
            return Ok(None);
        };
        let recipient: String = sqlx::query_scalar("SELECT recipient_id FROM agent_deliveries WHERE delivery_id=? AND root_id=? AND work_actor_id=? AND work_id=?")
            .bind(id.to_string()).bind(stamp.root_id.to_string()).bind(stamp.actor_id.to_string()).bind(stamp.work_id.to_string())
            .fetch_one(&mut *conn).await?;
        super::coordination::read_delivery(
            &mut conn,
            stamp.root_id,
            ThreadId::from_string(&recipient)?,
            id,
        )
        .await
    }
}

async fn completion_continuation_ready(
    conn: &mut SqliteConnection,
    stamp: WorkStamp,
    runtime: Uuid,
    id: Uuid,
) -> anyhow::Result<bool> {
    Ok(sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM agent_work_completion c
        JOIN agent_current_work cw USING(root_id,actor_id,work_id)
        JOIN agent_work w USING(root_id,actor_id,work_id)
        WHERE c.root_id=? AND c.actor_id=? AND c.work_id=? AND c.delivery_id=?
        AND c.disposition='completed' AND c.execution_settled=1 AND w.runtime_id=?
        AND w.disposition='waiting')",
    )
    .bind(stamp.root_id.to_string())
    .bind(stamp.actor_id.to_string())
    .bind(stamp.work_id.to_string())
    .bind(id.to_string())
    .bind(runtime.to_string())
    .fetch_one(conn)
    .await?)
}

async fn active_completion_blockers(
    conn: &mut SqliteConnection,
    stamp: WorkStamp,
) -> anyhow::Result<Vec<WorkStamp>> {
    let rows = sqlx::query("SELECT b.blocker_actor_id,b.blocker_work_id FROM agent_completion_blockers b
        JOIN agent_work w ON w.root_id=b.root_id AND w.actor_id=b.blocker_actor_id AND w.work_id=b.blocker_work_id
        WHERE b.root_id=? AND b.actor_id=? AND b.work_id=? AND w.disposition IN ('pending_start','running','waiting')
        ORDER BY b.blocker_actor_id,b.blocker_work_id")
        .bind(stamp.root_id.to_string()).bind(stamp.actor_id.to_string()).bind(stamp.work_id.to_string())
        .fetch_all(conn).await?;
    rows.into_iter()
        .map(|row| {
            Ok(WorkStamp {
                root_id: stamp.root_id,
                actor_id: ThreadId::from_string(row.try_get("blocker_actor_id")?)?,
                work_id: Uuid::parse_str(row.try_get("blocker_work_id")?)?,
            })
        })
        .collect()
}

pub(super) async fn bind_turn_in_transaction(
    conn: &mut SqliteConnection,
    stamp: WorkStamp,
    turn_id: &str,
) -> anyhow::Result<()> {
    anyhow::ensure!(!turn_id.is_empty(), CoordinationError::InvalidDelivery);
    sqlx::query(
        "INSERT OR IGNORE INTO agent_work_turns(root_id,actor_id,turn_id,work_id) VALUES(?,?,?,?)",
    )
    .bind(stamp.root_id.to_string())
    .bind(stamp.actor_id.to_string())
    .bind(turn_id)
    .bind(stamp.work_id.to_string())
    .execute(&mut *conn)
    .await?;
    let retained: String = sqlx::query_scalar(
        "SELECT work_id FROM agent_work_turns WHERE root_id=? AND actor_id=? AND turn_id=?",
    )
    .bind(stamp.root_id.to_string())
    .bind(stamp.actor_id.to_string())
    .bind(turn_id)
    .fetch_one(conn)
    .await?;
    anyhow::ensure!(
        retained == stamp.work_id.to_string(),
        CoordinationError::StaleWork
    );
    Ok(())
}

async fn candidate(
    conn: &mut SqliteConnection,
    stamp: WorkStamp,
) -> anyhow::Result<Option<CompletionCandidate>> {
    let Some(row) = sqlx::query(
        "SELECT * FROM agent_work_completion WHERE root_id=? AND actor_id=? AND work_id=?",
    )
    .bind(stamp.root_id.to_string())
    .bind(stamp.actor_id.to_string())
    .bind(stamp.work_id.to_string())
    .fetch_optional(&mut *conn)
    .await?
    else {
        return Ok(None);
    };
    let answers = sqlx::query_scalar("SELECT content FROM agent_work_answers WHERE root_id=? AND actor_id=? AND work_id=? ORDER BY sequence")
        .bind(stamp.root_id.to_string()).bind(stamp.actor_id.to_string()).bind(stamp.work_id.to_string())
        .fetch_all(conn).await?;
    Ok(Some(CompletionCandidate {
        stamp,
        delivery_id: Uuid::parse_str(row.try_get("delivery_id")?)?,
        recipient_id: ThreadId::from_string(row.try_get("recipient_id")?)?,
        turn_id: row.try_get("turn_id")?,
        disposition: WorkDisposition::parse(row.try_get("disposition")?)?,
        detail: row.try_get("detail")?,
        answers,
        capture_ready: row.try_get("capture_ready")?,
        capture: row.try_get("capture")?,
    }))
}

#[cfg(test)]
#[path = "coordination_lifecycle_tests.rs"]
mod tests;

use super::StateRuntime;
use crate::coordination::CoordinationError;
use crate::coordination::DeliveryId;
use crate::coordination::DeliveryKind;
use crate::coordination::DeliveryRecord;
use crate::coordination::PublishDelivery;
use crate::coordination::WorkDisposition;
use crate::coordination::WorkRecord;
use crate::coordination::WorkStamp;
use crate::sqlite::SqlitePoolWriteExt;
use codex_protocol::ThreadId;
use sqlx::Row;
use sqlx::SqliteConnection;
use sqlx::sqlite::SqliteRow;
use uuid::Uuid;

#[path = "coordination_observation.rs"]
mod observation;

impl StateRuntime {
    pub async fn coordination_current_work(
        &self,
        root: ThreadId,
        actor: ThreadId,
    ) -> anyhow::Result<Option<WorkRecord>> {
        let mut conn = self.pool.acquire().await?;
        current_work(&mut conn, root, actor).await
    }

    pub async fn coordination_work(&self, stamp: WorkStamp) -> anyhow::Result<Option<WorkRecord>> {
        let mut conn = self.pool.acquire().await?;
        read_work(&mut conn, stamp).await
    }

    /// A turn/checkpoint/reset caller can update a disposition, never allocate work.
    /// Terminal transitions must atomically publish their immutable result instead.
    pub async fn coordination_transition_work(
        &self,
        stamp: WorkStamp,
        expected_revision: i64,
        runtime_id: Uuid,
        disposition: WorkDisposition,
    ) -> anyhow::Result<WorkRecord> {
        anyhow::ensure!(
            !disposition.is_terminal(),
            CoordinationError::InvalidTransition
        );
        let mut tx = self.pool.begin_immediate().await?;
        let mut work = checked_current_work(&mut tx, stamp, expected_revision).await?;
        anyhow::ensure!(work.runtime_id == runtime_id, CoordinationError::StaleWork);
        anyhow::ensure!(
            !work.disposition.is_terminal(),
            CoordinationError::InvalidTransition
        );
        if work.disposition != disposition {
            work.disposition = disposition;
            write_work(&mut tx, &mut work).await?;
        }
        tx.commit().await?;
        Ok(work)
    }

    /// Explicit reconciliation fences old-runtime callbacks but does not resume work.
    /// The caller reconciles execution receipts before a subsequent running transition.
    pub async fn coordination_reconcile_work(
        &self,
        stamp: WorkStamp,
        expected_revision: i64,
        runtime_id: Uuid,
    ) -> anyhow::Result<WorkRecord> {
        let mut tx = self.pool.begin_immediate().await?;
        let mut work = checked_current_work(&mut tx, stamp, expected_revision).await?;
        anyhow::ensure!(
            !work.disposition.is_terminal(),
            CoordinationError::InvalidTransition
        );
        work.runtime_id = runtime_id;
        if work.disposition != WorkDisposition::Uncertain {
            work.disposition = WorkDisposition::Paused;
        }
        write_work(&mut tx, &mut work).await?;
        tx.commit().await?;
        Ok(work)
    }

    /// Publish prepared addressed content without starting work or consuming input.
    pub async fn coordination_publish_delivery(
        &self,
        publication: &PublishDelivery,
    ) -> anyhow::Result<DeliveryRecord> {
        anyhow::ensure!(
            publication.kind == DeliveryKind::Message,
            CoordinationError::InvalidDelivery
        );
        let mut tx = self.pool.begin_immediate().await?;
        if let Some(stamp) = publication.work {
            anyhow::ensure!(
                read_work(&mut tx, stamp).await?.is_some(),
                CoordinationError::UnknownWork
            );
        }
        let result = insert_delivery(&mut tx, publication).await?;
        tx.commit().await?;
        Ok(result)
    }

    pub async fn coordination_delivery(
        &self,
        root: ThreadId,
        recipient: ThreadId,
        id: DeliveryId,
    ) -> anyhow::Result<Option<DeliveryRecord>> {
        let mut conn = self.pool.acquire().await?;
        read_delivery(&mut conn, root, recipient, id).await
    }

    /// Reconcile an original operation before reacquiring its input resources.
    /// A globally unique delivery ID cannot be redirected to another recipient
    /// or root. No content from the conflicting destination is returned.
    pub async fn coordination_repeated_delivery(
        &self,
        root: ThreadId,
        recipient: ThreadId,
        id: DeliveryId,
    ) -> anyhow::Result<Option<DeliveryRecord>> {
        let row = sqlx::query("SELECT * FROM agent_deliveries WHERE delivery_id=?")
            .bind(id.to_string())
            .fetch_optional(self.pool.as_ref())
            .await?;
        let Some(row) = row else {
            return Ok(None);
        };
        anyhow::ensure!(
            row.try_get::<String, _>("root_id")? == root.to_string()
                && row.try_get::<String, _>("recipient_id")? == recipient.to_string(),
            CoordinationError::DeliveryConflict
        );
        Ok(Some(delivery_from_row(row)?))
    }

    /// Indexed recipient-local paging, including claimed/accepted but unread items.
    /// A page is an observation, not an atomic wait selection or claim.
    pub async fn coordination_unread_deliveries(
        &self,
        root: ThreadId,
        recipient: ThreadId,
        after_sequence: i64,
        limit: u32,
    ) -> anyhow::Result<Vec<DeliveryRecord>> {
        anyhow::ensure!((1..=256).contains(&limit), CoordinationError::InvalidPage);
        sqlx::query(
            "SELECT * FROM agent_deliveries
             WHERE root_id=? AND recipient_id=? AND consumed=0 AND sequence>?
             ORDER BY sequence LIMIT ?",
        )
        .bind(root.to_string())
        .bind(recipient.to_string())
        .bind(after_sequence)
        .bind(i64::from(limit))
        .fetch_all(self.pool.as_ref())
        .await?
        .into_iter()
        .map(delivery_from_row)
        .collect()
    }

    /// Fence replay before execution I/O. A None claim is direct admitted input
    /// and cannot steal an event frozen by a wait. Exact retries only observe the
    /// retained dispatch; callers must not interpret them as permission to resend.
    pub async fn coordination_begin_delivery(
        &self,
        root: ThreadId,
        recipient: ThreadId,
        ids: &[DeliveryId],
        claim_id: Option<Uuid>,
        dispatch_id: Uuid,
    ) -> anyhow::Result<bool> {
        anyhow::ensure!(
            !ids.is_empty()
                && ids.iter().collect::<std::collections::HashSet<_>>().len() == ids.len(),
            CoordinationError::InvalidDelivery
        );
        let mut tx = self.pool.begin_immediate().await?;
        let previous_count: i64 =
            sqlx::query_scalar("SELECT count(*) FROM agent_deliveries WHERE dispatch_id=?")
                .bind(dispatch_id.to_string())
                .fetch_one(&mut *tx)
                .await?;
        anyhow::ensure!(
            previous_count == 0 || usize::try_from(previous_count)? == ids.len(),
            CoordinationError::DeliveryConflict
        );
        let mut started = false;
        let mut retained = false;
        for id in ids {
            let delivery = required_delivery(&mut tx, root, recipient, *id).await?;
            anyhow::ensure!(
                delivery.claim_id == claim_id
                    && delivery
                        .dispatch_id
                        .is_none_or(|value| value == dispatch_id),
                CoordinationError::DeliveryConflict
            );
            started |= delivery.dispatch_id.is_none();
            retained |= delivery.dispatch_id.is_some();
            anyhow::ensure!(!(started && retained), CoordinationError::DeliveryConflict);
            anyhow::ensure!(
                previous_count == 0 || retained,
                CoordinationError::DeliveryConflict
            );
            sqlx::query("UPDATE agent_deliveries SET dispatch_id=? WHERE delivery_id=?")
                .bind(dispatch_id.to_string())
                .bind(id.to_string())
                .execute(&mut *tx)
                .await?;
        }
        tx.commit().await?;
        Ok(started)
    }

    /// Record exact native/external acceptance, not consumption. Dispatch identity
    /// remains retained even if the host acknowledgment was lost or delayed.
    pub async fn coordination_confirm_delivery(
        &self,
        root: ThreadId,
        recipient: ThreadId,
        ids: &[DeliveryId],
        dispatch_id: Uuid,
        receipt: &str,
    ) -> anyhow::Result<()> {
        anyhow::ensure!(!receipt.is_empty(), CoordinationError::InvalidDelivery);
        let mut tx = self.pool.begin_immediate().await?;
        for id in ids {
            let delivery = required_delivery(&mut tx, root, recipient, *id).await?;
            anyhow::ensure!(
                delivery.dispatch_id == Some(dispatch_id)
                    && delivery.refused_reason.is_none()
                    && delivery
                        .accepted_receipt
                        .as_deref()
                        .is_none_or(|value| value == receipt),
                CoordinationError::DeliveryConflict
            );
            sqlx::query("UPDATE agent_deliveries SET accepted_receipt=? WHERE delivery_id=?")
                .bind(receipt)
                .bind(id.to_string())
                .execute(&mut *tx)
                .await?;
        }
        tx.commit().await?;
        Ok(())
    }

    /// Only IDs included in the accepted sampling input/frozen wait result are
    /// consumed. A repeated matching receipt is harmless; other claims stay unread.
    pub async fn coordination_consume_deliveries(
        &self,
        root: ThreadId,
        recipient: ThreadId,
        ids: &[DeliveryId],
        receipt: &str,
    ) -> anyhow::Result<()> {
        let mut tx = self.pool.begin_immediate().await?;
        for id in ids {
            let delivery = required_delivery(&mut tx, root, recipient, *id).await?;
            anyhow::ensure!(
                delivery.accepted_receipt.as_deref() == Some(receipt),
                CoordinationError::DeliveryConflict
            );
            sqlx::query("UPDATE agent_deliveries SET consumed=1 WHERE delivery_id=?")
                .bind(id.to_string())
                .execute(&mut *tx)
                .await?;
        }
        tx.commit().await?;
        Ok(())
    }
}

// Focused transaction helpers stay inside StateRuntime. Later wait registration
// uses these on its own transaction, not an exposed generic persistence framework.
pub(super) async fn claim_deliveries(
    conn: &mut SqliteConnection,
    root: ThreadId,
    recipient: ThreadId,
    ids: &[DeliveryId],
    claim_id: Uuid,
) -> anyhow::Result<()> {
    for id in ids {
        let delivery = required_delivery(conn, root, recipient, *id).await?;
        anyhow::ensure!(
            !delivery.consumed
                && delivery.dispatch_id.is_none()
                && delivery.claim_id.is_none_or(|value| value == claim_id),
            CoordinationError::DeliveryConflict
        );
        sqlx::query("UPDATE agent_deliveries SET claim_id=? WHERE delivery_id=?")
            .bind(claim_id.to_string())
            .bind(id.to_string())
            .execute(&mut *conn)
            .await?;
    }
    Ok(())
}

pub(super) async fn current_work(
    conn: &mut SqliteConnection,
    root: ThreadId,
    actor: ThreadId,
) -> anyhow::Result<Option<WorkRecord>> {
    sqlx::query(
        "SELECT w.* FROM agent_current_work c JOIN agent_work w
         USING(root_id,actor_id,work_id) WHERE c.root_id=? AND c.actor_id=?",
    )
    .bind(root.to_string())
    .bind(actor.to_string())
    .fetch_optional(conn)
    .await?
    .map(work_from_row)
    .transpose()
}

pub(super) async fn read_work(
    conn: &mut SqliteConnection,
    stamp: WorkStamp,
) -> anyhow::Result<Option<WorkRecord>> {
    sqlx::query("SELECT * FROM agent_work WHERE root_id=? AND actor_id=? AND work_id=?")
        .bind(stamp.root_id.to_string())
        .bind(stamp.actor_id.to_string())
        .bind(stamp.work_id.to_string())
        .fetch_optional(conn)
        .await?
        .map(work_from_row)
        .transpose()
}

pub(super) async fn checked_current_work(
    conn: &mut SqliteConnection,
    stamp: WorkStamp,
    revision: i64,
) -> anyhow::Result<WorkRecord> {
    let work = current_work(conn, stamp.root_id, stamp.actor_id)
        .await?
        .ok_or(CoordinationError::UnknownWork)?;
    anyhow::ensure!(
        work.stamp == stamp && work.revision == revision,
        CoordinationError::StaleWork
    );
    Ok(work)
}

pub(super) async fn write_work(
    conn: &mut SqliteConnection,
    work: &mut WorkRecord,
) -> anyhow::Result<()> {
    work.revision += 1;
    sqlx::query(
        "UPDATE agent_work SET runtime_id=?,revision=?,disposition=?,terminal_delivery_id=?
         WHERE root_id=? AND actor_id=? AND work_id=?",
    )
    .bind(work.runtime_id.to_string())
    .bind(work.revision)
    .bind(work.disposition.as_str())
    .bind(work.terminal_delivery_id.map(|id| id.to_string()))
    .bind(work.stamp.root_id.to_string())
    .bind(work.stamp.actor_id.to_string())
    .bind(work.stamp.work_id.to_string())
    .execute(conn)
    .await?;
    Ok(())
}

pub(super) async fn insert_delivery(
    conn: &mut SqliteConnection,
    publication: &PublishDelivery,
) -> anyhow::Result<DeliveryRecord> {
    anyhow::ensure!(
        publication
            .work
            .is_none_or(|work| work.root_id == publication.root_id),
        CoordinationError::InvalidDelivery
    );
    // Look up globally first: an ID cannot silently move between roots/recipients.
    if let Some(row) = sqlx::query("SELECT * FROM agent_deliveries WHERE delivery_id=?")
        .bind(publication.id.to_string())
        .fetch_optional(&mut *conn)
        .await?
    {
        let retained = delivery_from_row(row)?;
        anyhow::ensure!(
            retained.publication == *publication,
            CoordinationError::DeliveryConflict
        );
        return Ok(retained);
    }
    let result = sqlx::query(
        "INSERT INTO agent_deliveries
         (delivery_id,root_id,sender_id,recipient_id,work_actor_id,work_id,kind,payload)
         VALUES(?,?,?,?,?,?,?,?)",
    )
    .bind(publication.id.to_string())
    .bind(publication.root_id.to_string())
    .bind(publication.sender_id.map(|id| id.to_string()))
    .bind(publication.recipient_id.to_string())
    .bind(publication.work.map(|work| work.actor_id.to_string()))
    .bind(publication.work.map(|work| work.work_id.to_string()))
    .bind(publication.kind.as_str())
    .bind(&publication.payload)
    .execute(conn)
    .await?;
    Ok(DeliveryRecord {
        sequence: result.last_insert_rowid(),
        publication: publication.clone(),
        claim_id: None,
        dispatch_id: None,
        accepted_receipt: None,
        refused_reason: None,
        consumed: false,
    })
}

pub(super) async fn read_delivery(
    conn: &mut SqliteConnection,
    root: ThreadId,
    recipient: ThreadId,
    id: DeliveryId,
) -> anyhow::Result<Option<DeliveryRecord>> {
    sqlx::query(
        "SELECT * FROM agent_deliveries WHERE delivery_id=? AND root_id=? AND recipient_id=?",
    )
    .bind(id.to_string())
    .bind(root.to_string())
    .bind(recipient.to_string())
    .fetch_optional(conn)
    .await?
    .map(delivery_from_row)
    .transpose()
}

struct DeliveryDisposition {
    claim_id: Option<Uuid>,
    dispatch_id: Option<Uuid>,
    accepted_receipt: Option<String>,
    refused_reason: Option<String>,
    consumed: bool,
}

async fn required_delivery(
    conn: &mut SqliteConnection,
    root: ThreadId,
    recipient: ThreadId,
    id: DeliveryId,
) -> anyhow::Result<DeliveryDisposition> {
    // Claims/receipts touch metadata only, never decode retained source/final payloads.
    let row = sqlx::query(
        "SELECT claim_id,dispatch_id,accepted_receipt,refused_reason,consumed FROM agent_deliveries
         WHERE delivery_id=? AND root_id=? AND recipient_id=?",
    )
    .bind(id.to_string())
    .bind(root.to_string())
    .bind(recipient.to_string())
    .fetch_optional(conn)
    .await?
    .ok_or(CoordinationError::UnknownDelivery)?;
    Ok(DeliveryDisposition {
        claim_id: row
            .try_get::<Option<&str>, _>("claim_id")?
            .map(Uuid::parse_str)
            .transpose()?,
        dispatch_id: row
            .try_get::<Option<&str>, _>("dispatch_id")?
            .map(Uuid::parse_str)
            .transpose()?,
        accepted_receipt: row.try_get("accepted_receipt")?,
        refused_reason: row.try_get("refused_reason")?,
        consumed: row.try_get("consumed")?,
    })
}

fn work_from_row(row: SqliteRow) -> anyhow::Result<WorkRecord> {
    Ok(WorkRecord {
        stamp: WorkStamp {
            root_id: ThreadId::from_string(row.try_get("root_id")?)?,
            actor_id: ThreadId::from_string(row.try_get("actor_id")?)?,
            work_id: Uuid::parse_str(row.try_get("work_id")?)?,
        },
        runtime_id: Uuid::parse_str(row.try_get("runtime_id")?)?,
        revision: row.try_get("revision")?,
        disposition: WorkDisposition::parse(row.try_get("disposition")?)?,
        terminal_delivery_id: row
            .try_get::<Option<&str>, _>("terminal_delivery_id")?
            .map(Uuid::parse_str)
            .transpose()?,
    })
}

fn delivery_from_row(row: SqliteRow) -> anyhow::Result<DeliveryRecord> {
    let root_id = ThreadId::from_string(row.try_get("root_id")?)?;
    let work = match row.try_get::<Option<&str>, _>("work_id")? {
        Some(work_id) => Some(WorkStamp {
            root_id,
            actor_id: ThreadId::from_string(row.try_get("work_actor_id")?)?,
            work_id: Uuid::parse_str(work_id)?,
        }),
        None => None,
    };
    Ok(DeliveryRecord {
        sequence: row.try_get("sequence")?,
        publication: PublishDelivery {
            id: Uuid::parse_str(row.try_get("delivery_id")?)?,
            root_id,
            sender_id: row
                .try_get::<Option<&str>, _>("sender_id")?
                .map(ThreadId::from_string)
                .transpose()?,
            recipient_id: ThreadId::from_string(row.try_get("recipient_id")?)?,
            work,
            kind: match row.try_get::<&str, _>("kind")? {
                "message" => DeliveryKind::Message,
                "terminal" => DeliveryKind::Terminal,
                value => anyhow::bail!("invalid persisted delivery kind: {value}"),
            },
            payload: row.try_get("payload")?,
        },
        claim_id: row
            .try_get::<Option<&str>, _>("claim_id")?
            .map(Uuid::parse_str)
            .transpose()?,
        dispatch_id: row
            .try_get::<Option<&str>, _>("dispatch_id")?
            .map(Uuid::parse_str)
            .transpose()?,
        accepted_receipt: row.try_get("accepted_receipt")?,
        refused_reason: row.try_get("refused_reason")?,
        consumed: row.try_get("consumed")?,
    })
}

#[cfg(test)]
#[path = "coordination_tests.rs"]
mod tests;

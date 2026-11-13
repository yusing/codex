use super::StateRuntime;
use super::coordination::claim_deliveries;
use super::coordination::current_work;
use super::coordination::read_work;
use crate::coordination::CoordinationError;
use crate::coordination::WorkDisposition;
use crate::coordination::WorkStamp;
use crate::coordination_wait::*;
use crate::sqlite::SqlitePoolWriteExt;
use codex_protocol::ThreadId;
use codex_protocol::agent_metrics::AgentMetrics;
use sqlx::Row;
use sqlx::SqliteConnection;
use std::collections::BTreeMap;
use uuid::Uuid;

impl StateRuntime {
    /// Observation of a retained frozen item, including already-consumed claims.
    /// This grants no dispatch/replay authority in a new runtime or context.
    pub async fn coordination_observe_wait_claim(
        &self,
        root: ThreadId,
        actor: ThreadId,
        claim: Uuid,
    ) -> anyhow::Result<Option<WaitTicket>> {
        let mut tx = self.pool.begin().await?;
        let row = sqlx::query("SELECT wait_id,caller_work_id FROM agent_waits WHERE root_id=? AND caller_id=? AND claim_id=?")
            .bind(root.to_string()).bind(actor.to_string()).bind(claim.to_string()).fetch_optional(&mut *tx).await?;
        let Some(row) = row else { return Ok(None) };
        read_wait(
            &mut tx,
            WorkStamp {
                root_id: root,
                actor_id: actor,
                work_id: Uuid::parse_str(row.try_get("caller_work_id")?)?,
            },
            Uuid::parse_str(row.try_get("wait_id")?)?,
        )
        .await
    }

    /// The registry supplies one authenticated topology snapshot. Work identity,
    /// unread eligibility and ticket publication share this write transaction.
    pub async fn coordination_register_wait(
        &self,
        caller: WorkStamp,
        runtime_id: Uuid,
        cell: &WaitCell,
        mode: WaitMode,
        descendants: &[ThreadId],
    ) -> anyhow::Result<WaitTicket> {
        self.coordination_register_wait_with_options(
            caller,
            runtime_id,
            cell,
            &WaitOptions {
                mode,
                ..Default::default()
            },
            descendants,
        )
        .await
    }

    pub async fn coordination_register_wait_with_options(
        &self,
        caller: WorkStamp,
        runtime_id: Uuid,
        cell: &WaitCell,
        options: &WaitOptions,
        descendants: &[ThreadId],
    ) -> anyhow::Result<WaitTicket> {
        self.coordination_register_wait_at(
            caller,
            runtime_id,
            cell,
            options,
            descendants,
            chrono::Utc::now().timestamp_millis(),
        )
        .await
    }

    /// The native owner pairs this wall sample with one monotonic sample. Neither
    /// duplicate registration nor later cell settlement restarts its timeout.
    pub async fn coordination_register_wait_at(
        &self,
        caller: WorkStamp,
        runtime_id: Uuid,
        cell: &WaitCell,
        options: &WaitOptions,
        descendants: &[ThreadId],
        now_unix_ms: i64,
    ) -> anyhow::Result<WaitTicket> {
        self.coordination_register_wait_origin_at(
            caller,
            runtime_id,
            &WaitOrigin::Cell { cell: cell.clone() },
            options,
            descendants,
            now_unix_ms,
        )
        .await
    }

    pub async fn coordination_register_wait_origin_at(
        &self,
        caller: WorkStamp,
        runtime_id: Uuid,
        origin: &WaitOrigin,
        options: &WaitOptions,
        descendants: &[ThreadId],
        now_unix_ms: i64,
    ) -> anyhow::Result<WaitTicket> {
        let (kind, turn, item, window, cell) = match origin {
            WaitOrigin::Cell { cell } => {
                anyhow::ensure!(
                    !cell.turn_id.is_empty()
                        && !cell.item_id.is_empty()
                        && !cell.window_id.is_empty()
                        && !cell.cell_id.is_empty(),
                    WaitError::ConflictingCell
                );
                (
                    "cell",
                    cell.turn_id.as_str(),
                    cell.item_id.as_str(),
                    Some(cell.window_id.as_str()),
                    Some(cell.cell_id.as_str()),
                )
            }
            WaitOrigin::Turn { .. } => return Err(WaitError::InvalidOptions.into()),
        };
        anyhow::ensure!(
            options.compact || options.reload.is_none(),
            WaitError::InvalidOptions
        );
        anyhow::ensure!(
            options
                .timeout
                .as_ref()
                .is_none_or(|timeout| timeout.effective_ms >= 0),
            WaitError::InvalidOptions
        );
        anyhow::ensure!(
            options
                .progress_interval_ms
                .is_none_or(|ms| ms > 0 && i64::try_from(ms).is_ok()),
            WaitError::InvalidOptions
        );
        let mut tx = self.pool.begin_immediate().await?;
        live_caller(&mut tx, caller, runtime_id).await?;
        let retained: Option<String> = sqlx::query_scalar(
            "SELECT wait_id FROM agent_waits WHERE root_id=? AND caller_id=?
             AND caller_work_id=? AND origin=? AND turn_id=? AND item_id=? AND window_id IS ? AND cell_id IS ?",
        )
        .bind(caller.root_id.to_string()).bind(caller.actor_id.to_string())
        .bind(caller.work_id.to_string()).bind(kind).bind(turn).bind(item)
        .bind(window).bind(cell).fetch_optional(&mut *tx).await?;
        if let Some(id) = retained {
            let ticket = required_wait(&mut tx, caller, Uuid::parse_str(&id)?).await?;
            anyhow::ensure!(ticket.runtime_id == runtime_id, WaitError::ColdRuntime);
            anyhow::ensure!(ticket.options == *options, WaitError::ConflictingCell);
            tx.commit().await?;
            return Ok(ticket);
        }
        let active: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM agent_waits WHERE root_id=? AND caller_id=?
             AND phase NOT IN ('consumed','cancelled'))",
        )
        .bind(caller.root_id.to_string())
        .bind(caller.actor_id.to_string())
        .fetch_one(&mut *tx)
        .await?;
        anyhow::ensure!(!active, WaitError::ConflictingCell);

        let selected = select_descendants(&mut tx, caller, runtime_id, descendants).await?;
        anyhow::ensure!(
            !selected.is_empty() || next_message(&mut tx, caller).await?.is_some(),
            WaitError::EmptySelection
        );
        let id = Uuid::now_v7();
        let deadline = options
            .timeout
            .as_ref()
            .map(|timeout| {
                now_unix_ms
                    .checked_add(timeout.effective_ms)
                    .ok_or(WaitError::InvalidOptions)
            })
            .transpose()?;
        sqlx::query(
            "INSERT INTO agent_waits
             (wait_id,root_id,caller_id,caller_work_id,runtime_id,origin,turn_id,item_id,window_id,cell_id,mode,phase,
              timeout_requested_ms,timeout_effective_ms,deadline_unix_ms,progress_interval_ms,compact,reload_selection,reset_state)
             VALUES(?,?,?,?,?,?,?,?,?,?,?,'registered',?,?,?,?,?,?,?)",
        ).bind(id.to_string()).bind(caller.root_id.to_string()).bind(caller.actor_id.to_string())
            .bind(caller.work_id.to_string()).bind(runtime_id.to_string()).bind(kind).bind(turn)
            .bind(item).bind(window).bind(cell)
            .bind(match options.mode { WaitMode::Any => "any", WaitMode::All => "all" })
            .bind(options.timeout.as_ref().map(|timeout| timeout.requested_ms))
            .bind(options.timeout.as_ref().map(|timeout| timeout.effective_ms))
            .bind(deadline)
            .bind(options.progress_interval_ms.map(i64::try_from).transpose()?)
            .bind(options.compact)
            .bind(options.reload.as_ref().map(serde_json::to_string).transpose()?)
            .bind(if options.compact { "pending" } else { "not_requested" })
            .execute(&mut *tx).await?;
        insert_selection(&mut tx, id, &selected).await?;
        set_wait_work_disposition(&mut tx, caller, WorkDisposition::Waiting).await?;
        let ticket = required_wait(&mut tx, caller, id).await?;
        tx.commit().await?;
        Ok(ticket)
    }

    /// Only successful native-cell settlement may acquire this once-only reset.
    pub async fn coordination_begin_wait_reset(
        &self,
        caller: WorkStamp,
        runtime_id: Uuid,
        id: WaitId,
        cell: &WaitCell,
    ) -> anyhow::Result<bool> {
        self.coordination_begin_wait_reset_origin(
            caller,
            runtime_id,
            id,
            &WaitOrigin::Cell { cell: cell.clone() },
        )
        .await
    }

    pub async fn coordination_begin_wait_reset_origin(
        &self,
        caller: WorkStamp,
        runtime_id: Uuid,
        id: WaitId,
        origin: &WaitOrigin,
    ) -> anyhow::Result<bool> {
        let mut tx = self.pool.begin_immediate().await?;
        let ticket = live_wait(&mut tx, caller, runtime_id, id).await?;
        anyhow::ensure!(ticket.origin == *origin, WaitError::ConflictingCell);
        anyhow::ensure!(
            matches!(ticket.phase, WaitPhase::Registered),
            WaitError::InvalidPhase
        );
        let begin = match ticket.reset {
            WaitResetState::Pending => true,
            WaitResetState::Running | WaitResetState::Completed | WaitResetState::Failed { .. } => {
                false
            }
            _ => return Err(WaitError::InvalidPhase.into()),
        };
        if begin {
            sqlx::query("UPDATE agent_waits SET reset_state='running' WHERE wait_id=?")
                .bind(id.to_string())
                .execute(&mut *tx)
                .await?;
        }
        tx.commit().await?;
        Ok(begin)
    }

    /// Preserve an observed reset outcome even if input superseded this wait
    /// during acquisition. It never changes the current work or delivery phase.
    pub async fn coordination_finish_wait_reset(
        &self,
        caller: WorkStamp,
        runtime_id: Uuid,
        id: WaitId,
        cell: &WaitCell,
        diagnostic: Option<&str>,
    ) -> anyhow::Result<WaitTicket> {
        self.coordination_finish_wait_reset_origin(
            caller,
            runtime_id,
            id,
            &WaitOrigin::Cell { cell: cell.clone() },
            diagnostic,
        )
        .await
    }

    pub async fn coordination_finish_wait_reset_origin(
        &self,
        caller: WorkStamp,
        runtime_id: Uuid,
        id: WaitId,
        origin: &WaitOrigin,
        diagnostic: Option<&str>,
    ) -> anyhow::Result<WaitTicket> {
        let mut tx = self.pool.begin_immediate().await?;
        let ticket = required_wait(&mut tx, caller, id).await?;
        anyhow::ensure!(ticket.runtime_id == runtime_id, WaitError::ColdRuntime);
        anyhow::ensure!(ticket.origin == *origin, WaitError::ConflictingCell);
        let outcome = diagnostic.map_or(WaitResetState::Completed, |diagnostic| {
            WaitResetState::Failed {
                diagnostic: diagnostic.to_owned(),
            }
        });
        if ticket.reset != outcome {
            anyhow::ensure!(
                ticket.reset == WaitResetState::Running,
                WaitError::InvalidPhase
            );
            sqlx::query("UPDATE agent_waits SET reset_state=?,reset_diagnostic=? WHERE wait_id=?")
                .bind(if diagnostic.is_some() {
                    "failed"
                } else {
                    "completed"
                })
                .bind(diagnostic)
                .bind(id.to_string())
                .execute(&mut *tx)
                .await?;
        }
        let ticket = required_wait(&mut tx, caller, id).await?;
        tx.commit().await?;
        Ok(ticket)
    }

    /// Freeze exactly one continuation. Messages win without claiming outcomes.
    /// Repeated calls before consumption return the same claim, never a new event set.
    pub async fn coordination_claim_ready_wait(
        &self,
        caller: WorkStamp,
        runtime_id: Uuid,
        id: WaitId,
    ) -> anyhow::Result<Option<WaitClaim>> {
        self.coordination_claim_ready_wait_at(caller, runtime_id, id, WaitReadiness::default())
            .await
    }

    pub async fn coordination_claim_ready_wait_at(
        &self,
        caller: WorkStamp,
        runtime_id: Uuid,
        id: WaitId,
        readiness: WaitReadiness,
    ) -> anyhow::Result<Option<WaitClaim>> {
        let mut tx = self.pool.begin_immediate().await?;
        let ticket = live_wait(&mut tx, caller, runtime_id, id).await?;
        if ticket.phase == WaitPhase::Claimed {
            if ticket
                .claim
                .as_ref()
                .is_none_or(|claim| claim.wake != WaitWake::Progress)
                || higher_priority_wake(&mut tx, caller, &ticket, readiness)
                    .await?
                    .is_none()
            {
                return Ok(ticket.claim);
            }
            sqlx::query("UPDATE agent_waits SET phase='registered',claim_id=NULL,claim_wake=NULL,claim_metrics=NULL WHERE wait_id=?")
                .bind(id.to_string()).execute(&mut *tx).await?;
        }
        if !matches!(ticket.phase, WaitPhase::Registered | WaitPhase::Claimed)
            || ticket.reset.blocks_delivery()
        {
            return Ok(None);
        }
        let ready = higher_priority_wake(&mut tx, caller, &ticket, readiness).await?;
        let (wake, delivery_ids, outcomes) = match ready {
            Some(ready) => ready,
            None if super::coordination_progress::read_batch(&mut tx, id)
                .await?
                .is_some_and(|batch| batch.output.is_some()) =>
            {
                (WaitWake::Progress, Vec::new(), Vec::new())
            }
            None => return Ok(None),
        };
        let claim_id = Uuid::now_v7();
        claim_deliveries(
            &mut tx,
            caller.root_id,
            caller.actor_id,
            &delivery_ids,
            claim_id,
        )
        .await?;
        for delivery in &delivery_ids {
            sqlx::query("INSERT INTO agent_wait_deliveries(wait_id,delivery_id) VALUES(?,?)")
                .bind(id.to_string())
                .bind(delivery.to_string())
                .execute(&mut *tx)
                .await?;
        }
        for (ordinal, (_, work, _)) in outcomes.iter().enumerate() {
            sqlx::query("UPDATE agent_wait_selection SET claimed=1,claimed_disposition=?,claimed_ordinal=? WHERE wait_id=? AND actor_id=? AND work_id=?")
                .bind(work.disposition.as_str()).bind(i64::try_from(ordinal)?).bind(id.to_string()).bind(work.stamp.actor_id.to_string()).bind(work.stamp.work_id.to_string())
                .execute(&mut *tx).await?;
        }
        sqlx::query(
            "UPDATE agent_waits SET phase='claimed',claim_id=?,claim_wake=? WHERE wait_id=?",
        )
        .bind(claim_id.to_string())
        .bind(match wake {
            WaitWake::Events => "events",
            WaitWake::Deadline => "deadline",
            WaitWake::Progress => "progress",
        })
        .bind(id.to_string())
        .execute(&mut *tx)
        .await?;
        if matches!(ticket.origin, WaitOrigin::Cell { .. }) {
            // The original callback resumes now. Its frozen result remains
            // unconsumed until the actual provider receipt.
            set_wait_work_disposition(&mut tx, caller, WorkDisposition::Running).await?;
        }
        let claim = required_wait(&mut tx, caller, id).await?.claim;
        tx.commit().await?;
        Ok(claim)
    }

    pub async fn coordination_freeze_wait_metrics(
        &self,
        caller: WorkStamp,
        runtime_id: Uuid,
        id: WaitId,
        claim_id: Uuid,
        metrics: &BTreeMap<String, AgentMetrics>,
    ) -> anyhow::Result<BTreeMap<String, AgentMetrics>> {
        let mut tx = self.pool.begin_immediate().await?;
        let ticket = live_wait(&mut tx, caller, runtime_id, id).await?;
        let claim = ticket.claim.ok_or(WaitError::InvalidPhase)?;
        anyhow::ensure!(claim.id == claim_id, WaitError::InvalidPhase);
        if let Some(frozen) = claim.metrics {
            return Ok(frozen);
        }
        anyhow::ensure!(ticket.phase == WaitPhase::Claimed, WaitError::InvalidPhase);
        let actors: std::collections::HashSet<_> =
            ticket.selected.iter().map(|work| work.actor_id).collect();
        anyhow::ensure!(
            metrics.len() == actors.len()
                && ticket
                    .selected
                    .iter()
                    .all(|work| metrics.contains_key(&work.actor_id.to_string())),
            WaitError::InvalidOptions
        );
        sqlx::query("UPDATE agent_waits SET claim_metrics=? WHERE wait_id=?")
            .bind(serde_json::to_string(metrics)?)
            .bind(id.to_string())
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(metrics.clone())
    }

    /// Exact original tool output was included in a provider-accepted request.
    /// No replacement turn is dispatched for an awaited invocation.
    pub async fn coordination_consume_awaited_wait(
        &self,
        caller: WorkStamp,
        runtime_id: Uuid,
        id: WaitId,
        claim_id: Uuid,
        context: Uuid,
    ) -> anyhow::Result<()> {
        let mut tx = self.pool.begin_immediate().await?;
        let ticket = live_wait(&mut tx, caller, runtime_id, id).await?;
        anyhow::ensure!(
            matches!(ticket.phase, WaitPhase::Claimed | WaitPhase::Consumed),
            WaitError::InvalidPhase
        );
        let claim = ticket.claim.as_ref().ok_or(WaitError::InvalidPhase)?;
        anyhow::ensure!(
            claim.id == claim_id && ticket.dispatch_id.is_none(),
            CoordinationError::DeliveryConflict
        );
        if ticket.phase == WaitPhase::Consumed {
            return Ok(());
        }
        let changed = sqlx::query(
            "UPDATE agent_deliveries SET consumed=1 WHERE delivery_id IN
            (SELECT delivery_id FROM agent_wait_deliveries WHERE wait_id=?)
            AND claim_id=? AND dispatch_id IS NULL AND consumed=0",
        )
        .bind(id.to_string())
        .bind(claim_id.to_string())
        .execute(&mut *tx)
        .await?
        .rows_affected();
        anyhow::ensure!(
            changed == u64::try_from(claim.delivery_ids.len())?,
            CoordinationError::DeliveryConflict
        );
        if claim.wake == WaitWake::Progress {
            super::coordination_progress::acknowledge_progress(&mut tx, caller, id, context)
                .await?;
        }
        set_phase(&mut tx, id, "consumed").await?;
        set_wait_work_disposition(&mut tx, caller, WorkDisposition::Running).await?;
        tx.commit().await?;
        Ok(())
    }

    pub async fn coordination_cancel_wait_with_cause(
        &self,
        caller: WorkStamp,
        runtime: Uuid,
        id: WaitId,
        origin: &WaitOrigin,
        cause: &WaitCancellation,
    ) -> anyhow::Result<()> {
        // Causal input/terminal evidence belongs to its owner transaction;
        // historical dispatch refusals cannot be asserted through this API.
        anyhow::ensure!(
            matches!(
                cause,
                WaitCancellation::Cancelled | WaitCancellation::ExecutionFailed
            ) || matches!(cause, WaitCancellation::ShellOutput { turn_id } if !turn_id.is_empty()),
            CoordinationError::InvalidTransition
        );
        let mut tx = self.pool.begin_immediate().await?;
        cancel_wait_with_cause_in_transaction(&mut tx, caller, runtime, id, origin, cause).await?;
        tx.commit().await?;
        Ok(())
    }

    /// Cancel only an undispatched intent. A lost acceptance cannot be treated
    /// as rejection or release another continuation's retained delivery fence.
    pub async fn coordination_cancel_wait(
        &self,
        caller: WorkStamp,
        runtime_id: Uuid,
        id: WaitId,
        cell: &WaitCell,
    ) -> anyhow::Result<()> {
        let mut tx = self.pool.begin_immediate().await?;
        cancel_wait_in_transaction(
            &mut tx,
            caller,
            runtime_id,
            id,
            &WaitOrigin::Cell { cell: cell.clone() },
        )
        .await?;
        tx.commit().await?;
        Ok(())
    }

    pub async fn coordination_cancel_wait_origin(
        &self,
        caller: WorkStamp,
        runtime_id: Uuid,
        id: WaitId,
        origin: &WaitOrigin,
    ) -> anyhow::Result<()> {
        let mut tx = self.pool.begin_immediate().await?;
        cancel_wait_in_transaction(&mut tx, caller, runtime_id, id, origin).await?;
        tx.commit().await?;
        Ok(())
    }

    /// Passive retained projection. No reconciliation, rearm or receipt replay.
    pub async fn coordination_observe_wait(
        &self,
        caller: WorkStamp,
        id: WaitId,
    ) -> anyhow::Result<Option<WaitTicket>> {
        let mut tx = self.pool.begin().await?;
        let ticket = read_wait(&mut tx, caller, id).await?;
        tx.commit().await?;
        Ok(ticket)
    }

    pub async fn coordination_current_wait(
        &self,
        root: ThreadId,
        actor: ThreadId,
    ) -> anyhow::Result<Option<WaitTicket>> {
        let mut tx = self.pool.begin().await?;
        let row = sqlx::query(
            "SELECT wait_id,caller_work_id FROM agent_waits WHERE root_id=? AND caller_id=?
             AND phase NOT IN ('consumed','cancelled')",
        )
        .bind(root.to_string())
        .bind(actor.to_string())
        .fetch_optional(&mut *tx)
        .await?;
        let Some(row) = row else { return Ok(None) };
        let caller = WorkStamp {
            root_id: root,
            actor_id: actor,
            work_id: Uuid::parse_str(row.try_get("caller_work_id")?)?,
        };
        let ticket = read_wait(&mut tx, caller, Uuid::parse_str(row.try_get("wait_id")?)?).await?;
        tx.commit().await?;
        Ok(ticket)
    }

    /// Exhausted native requests already have a turn/work acceptance. Their
    /// checkpoint is not an authored tool cell and has no reset/tool options.
    pub async fn coordination_commit_turn_wait(
        &self,
        caller: WorkStamp,
        runtime_id: Uuid,
        turn_id: &str,
        reason: TurnWaitReason,
        deadline_unix_ms: Option<i64>,
        now_unix_ms: i64,
        descendants: &[ThreadId],
    ) -> anyhow::Result<Option<WaitTicket>> {
        anyhow::ensure!(
            reason.is_provider() || deadline_unix_ms.is_none(),
            WaitError::InvalidOptions
        );
        let mut tx = self.pool.begin_immediate().await?;
        live_caller(&mut tx, caller, runtime_id).await?;
        bound_turn(&mut tx, caller, turn_id).await?;
        let previous: Option<String> = sqlx::query_scalar(
            "SELECT wait_id FROM agent_waits WHERE root_id=? AND caller_id=? AND caller_work_id=?
             AND origin='turn' AND turn_id=? AND turn_reason=?
             AND phase NOT IN ('consumed','cancelled')",
        )
        .bind(caller.root_id.to_string())
        .bind(caller.actor_id.to_string())
        .bind(caller.work_id.to_string())
        .bind(turn_id)
        .bind(reason.as_str())
        .fetch_optional(&mut *tx)
        .await?;
        if let Some(id) = previous {
            let ticket = required_wait(&mut tx, caller, Uuid::parse_str(&id)?).await?;
            anyhow::ensure!(ticket.runtime_id == runtime_id, WaitError::ColdRuntime);
            return Ok(Some(ticket));
        }
        let active: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM agent_waits WHERE root_id=? AND caller_id=?
             AND phase NOT IN ('consumed','cancelled'))",
        )
        .bind(caller.root_id.to_string())
        .bind(caller.actor_id.to_string())
        .fetch_one(&mut *tx)
        .await?;
        anyhow::ensure!(!active, WaitError::InvalidPhase);

        let selected = if reason == TurnWaitReason::DescendantRecovery {
            let selected = select_descendants(&mut tx, caller, runtime_id, descendants).await?;
            let mut active = false;
            for stamp in &selected {
                let work = read_work(&mut tx, *stamp)
                    .await?
                    .ok_or(CoordinationError::UnknownWork)?;
                active |= work.runtime_id == runtime_id
                    && matches!(
                        work.disposition,
                        WorkDisposition::PendingStart
                            | WorkDisposition::Running
                            | WorkDisposition::Waiting
                    );
            }
            if !active {
                return Ok(None);
            }
            selected
        } else {
            Vec::new()
        };

        let deadline = if reason == TurnWaitReason::ProviderCapacity && deadline_unix_ms.is_none() {
            let prior: i64 = sqlx::query_scalar(
                "SELECT capacity_retry_seconds FROM agent_work WHERE root_id=? AND actor_id=? AND work_id=?",
            ).bind(caller.root_id.to_string()).bind(caller.actor_id.to_string()).bind(caller.work_id.to_string())
                .fetch_one(&mut *tx).await?;
            let next = if prior == 0 { 1 } else { (prior * 2).min(30) };
            sqlx::query("UPDATE agent_work SET capacity_retry_seconds=? WHERE root_id=? AND actor_id=? AND work_id=?")
                .bind(next).bind(caller.root_id.to_string()).bind(caller.actor_id.to_string()).bind(caller.work_id.to_string())
                .execute(&mut *tx).await?;
            Some(
                now_unix_ms
                    .checked_add(next * 1000)
                    .ok_or(WaitError::InvalidOptions)?,
            )
        } else {
            deadline_unix_ms
        };
        let id = Uuid::now_v7();
        sqlx::query(
            "INSERT INTO agent_waits
             (wait_id,root_id,caller_id,caller_work_id,runtime_id,origin,turn_id,turn_reason,mode,phase,deadline_unix_ms)
             VALUES(?,?,?,?,?,'turn',?,?,'any','registered',?)",
        ).bind(id.to_string()).bind(caller.root_id.to_string()).bind(caller.actor_id.to_string())
            .bind(caller.work_id.to_string()).bind(runtime_id.to_string()).bind(turn_id)
            .bind(reason.as_str()).bind(deadline).execute(&mut *tx).await?;
        insert_selection(&mut tx, id, &selected).await?;
        set_wait_work_disposition(
            &mut tx,
            caller,
            if reason == TurnWaitReason::ProviderUsageLimit && deadline.is_none() {
                WorkDisposition::Paused
            } else {
                WorkDisposition::Waiting
            },
        )
        .await?;
        let ticket = required_wait(&mut tx, caller, id).await?;
        tx.commit().await?;
        Ok(Some(ticket))
    }

    /// Only successful inference in the accepted current turn resets fallback.
    pub async fn coordination_provider_recovered(
        &self,
        caller: WorkStamp,
        runtime_id: Uuid,
        turn_id: &str,
    ) -> anyhow::Result<()> {
        let mut tx = self.pool.begin_immediate().await?;
        live_caller(&mut tx, caller, runtime_id).await?;
        bound_turn(&mut tx, caller, turn_id).await?;
        sqlx::query(
            "UPDATE agent_work SET capacity_retry_seconds=0 WHERE root_id=? AND actor_id=? AND work_id=?
             AND capacity_retry_seconds!=0",
        ).bind(caller.root_id.to_string()).bind(caller.actor_id.to_string()).bind(caller.work_id.to_string())
            .execute(&mut *tx).await?;
        tx.commit().await?;
        Ok(())
    }
}

async fn higher_priority_wake(
    tx: &mut SqliteConnection,
    caller: WorkStamp,
    ticket: &WaitTicket,
    readiness: WaitReadiness,
) -> anyhow::Result<
    Option<(
        WaitWake,
        Vec<Uuid>,
        Vec<(i64, crate::coordination::WorkRecord, bool)>,
    )>,
> {
    let mut delivery_ids = Vec::new();
    let mut outcomes = Vec::new();
    let mut wake = WaitWake::Events;
    let provider = ticket
        .origin
        .reason()
        .is_some_and(TurnWaitReason::is_provider);
    let deadline_due = readiness.deadline_due && ticket.deadline_unix_ms.is_some();
    if provider {
        if !deadline_due {
            return Ok(None);
        }
        wake = WaitWake::Deadline;
    } else if let Some(message) = next_message(tx, caller).await? {
        delivery_ids.push(message);
    } else {
        let mut unsettled = false;
        for stamp in &ticket.selected {
            let work = read_work(tx, *stamp)
                .await?
                .ok_or(CoordinationError::UnknownWork)?;
            if work.disposition == WorkDisposition::Paused {
                // A stopped selected target settles without inventing mail or
                // a terminal final. Ordered terminal publications retain priority.
                outcomes.push((i64::MAX, work, false));
                continue;
            }
            if !work.disposition.is_terminal() {
                if ticket.options.mode == WaitMode::All {
                    unsettled = true;
                }
                continue;
            }
            // A selected terminal still settles even if another admitted
            // input consumed its notification after this wait's snapshot.
            let row = sqlx::query(
                "SELECT sequence,delivery_id,recipient_id,consumed,claim_id,dispatch_id
                     FROM agent_deliveries WHERE delivery_id=?",
            )
            .bind(work.terminal_delivery_id.map(|value| value.to_string()))
            .fetch_one(&mut *tx)
            .await?;
            let sequence: i64 = row.try_get("sequence")?;
            let available = row.try_get::<&str, _>("recipient_id")? == caller.actor_id.to_string()
                && !row.try_get::<bool, _>("consumed")?
                && row.try_get::<Option<String>, _>("claim_id")?.is_none()
                && row.try_get::<Option<String>, _>("dispatch_id")?.is_none();
            if ticket.origin.reason() == Some(TurnWaitReason::DescendantRecovery) {
                let own_notification =
                    row.try_get::<&str, _>("recipient_id")? == caller.actor_id.to_string();
                if (own_notification && !available)
                    || outcome_observed(tx, caller, work.stamp).await?
                {
                    continue;
                }
            }
            outcomes.push((sequence, work, available));
        }
        if unsettled {
            outcomes.clear();
        }
        outcomes.sort_by_key(|(sequence, _, _)| *sequence);
        if ticket.options.mode == WaitMode::Any {
            outcomes.truncate(1);
        }
        if outcomes.is_empty() {
            if !deadline_due {
                return Ok(None);
            }
            wake = WaitWake::Deadline;
        }
        for (_, work, available) in &outcomes {
            if *available && let Some(delivery) = work.terminal_delivery_id {
                delivery_ids.push(delivery);
            }
        }
    }
    Ok(Some((wake, delivery_ids, outcomes)))
}

async fn bound_turn(
    conn: &mut SqliteConnection,
    caller: WorkStamp,
    turn_id: &str,
) -> anyhow::Result<()> {
    let work: Option<String> = sqlx::query_scalar(
        "SELECT work_id FROM agent_work_turns WHERE root_id=? AND actor_id=? AND turn_id=?",
    )
    .bind(caller.root_id.to_string())
    .bind(caller.actor_id.to_string())
    .bind(turn_id)
    .fetch_optional(conn)
    .await?;
    anyhow::ensure!(
        work.as_deref() == Some(caller.work_id.to_string().as_str()),
        CoordinationError::StaleWork
    );
    Ok(())
}

async fn select_descendants(
    conn: &mut SqliteConnection,
    caller: WorkStamp,
    runtime_id: Uuid,
    descendants: &[ThreadId],
) -> anyhow::Result<Vec<WorkStamp>> {
    let mut selected = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for actor in descendants {
        anyhow::ensure!(
            *actor != caller.actor_id,
            CoordinationError::InvalidDelivery
        );
        if !seen.insert(*actor) {
            continue;
        }
        let ids: Vec<String> = sqlx::query_scalar(
            "SELECT w.work_id FROM agent_work w
             LEFT JOIN agent_deliveries d ON d.delivery_id=w.terminal_delivery_id
             WHERE w.root_id=? AND w.actor_id=? AND
             ((w.runtime_id=? AND w.disposition IN ('pending_start','running','waiting','paused')) OR
              (w.terminal_delivery_id IS NOT NULL
               AND NOT EXISTS(SELECT 1 FROM agent_wait_selection s JOIN agent_waits p USING(wait_id)
                 WHERE s.root_id=w.root_id AND s.actor_id=w.actor_id AND s.work_id=w.work_id
                   AND s.claimed=1 AND s.claimed_disposition!='paused'
                   AND p.caller_id=? AND p.phase='consumed')
               AND (d.recipient_id!=? OR d.consumed=0))) ORDER BY w.work_id",
        ).bind(caller.root_id.to_string()).bind(actor.to_string()).bind(runtime_id.to_string())
            .bind(caller.actor_id.to_string()).bind(caller.actor_id.to_string())
            .fetch_all(&mut *conn).await?;
        for id in ids {
            selected.push(WorkStamp {
                root_id: caller.root_id,
                actor_id: *actor,
                work_id: Uuid::parse_str(&id)?,
            });
        }
    }
    Ok(selected)
}

async fn insert_selection(
    conn: &mut SqliteConnection,
    id: WaitId,
    selected: &[WorkStamp],
) -> anyhow::Result<()> {
    for (ordinal, stamp) in selected.iter().enumerate() {
        sqlx::query("INSERT INTO agent_wait_selection(wait_id,ordinal,root_id,actor_id,work_id) VALUES(?,?,?,?,?)")
            .bind(id.to_string()).bind(i64::try_from(ordinal)?).bind(stamp.root_id.to_string())
            .bind(stamp.actor_id.to_string()).bind(stamp.work_id.to_string()).execute(&mut *conn).await?;
    }
    Ok(())
}

async fn outcome_observed(
    conn: &mut SqliteConnection,
    caller: WorkStamp,
    outcome: WorkStamp,
) -> anyhow::Result<bool> {
    Ok(sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM agent_wait_selection s JOIN agent_waits p USING(wait_id)
         WHERE s.root_id=? AND s.actor_id=? AND s.work_id=? AND s.claimed=1
           AND s.claimed_disposition!='paused' AND p.caller_id=? AND p.phase='consumed')",
    )
    .bind(outcome.root_id.to_string())
    .bind(outcome.actor_id.to_string())
    .bind(outcome.work_id.to_string())
    .bind(caller.actor_id.to_string())
    .fetch_one(conn)
    .await?)
}

/// The admitted-input owner calls this inside its own immediate transaction.
/// A later refused input rolls back both cancellation and delivery-claim release.
pub(super) async fn cancel_wait_in_transaction(
    conn: &mut SqliteConnection,
    caller: WorkStamp,
    runtime_id: Uuid,
    ticket_id: WaitId,
    origin: &WaitOrigin,
) -> anyhow::Result<()> {
    cancel_wait_with_cause_in_transaction(
        conn,
        caller,
        runtime_id,
        ticket_id,
        origin,
        &WaitCancellation::Cancelled,
    )
    .await
}

pub(super) async fn cancel_wait_with_cause_in_transaction(
    conn: &mut SqliteConnection,
    caller: WorkStamp,
    runtime_id: Uuid,
    ticket_id: WaitId,
    origin: &WaitOrigin,
    cause: &WaitCancellation,
) -> anyhow::Result<()> {
    // An explicit work reconciliation may retire an old, undispatched wait. It
    // may not rearm it or release a retained/uncertain dispatch.
    live_caller(conn, caller, runtime_id).await?;
    let ticket = required_wait(conn, caller, ticket_id).await?;
    anyhow::ensure!(ticket.origin == *origin, WaitError::ConflictingCell);
    anyhow::ensure!(ticket.dispatch_id.is_none(), WaitError::InvalidPhase);
    if ticket.phase == WaitPhase::Cancelled {
        return Ok(());
    }
    let changed = sqlx::query(
        "UPDATE agent_deliveries SET claim_id=NULL WHERE delivery_id IN
         (SELECT delivery_id FROM agent_wait_deliveries WHERE wait_id=?)
         AND claim_id=? AND dispatch_id IS NULL AND accepted_receipt IS NULL AND consumed=0",
    )
    .bind(ticket_id.to_string())
    .bind(ticket.claim.as_ref().map(|claim| claim.id.to_string()))
    .execute(&mut *conn)
    .await?
    .rows_affected();
    let expected = ticket
        .claim
        .as_ref()
        .map_or(0, |claim| claim.delivery_ids.len());
    anyhow::ensure!(
        changed == u64::try_from(expected)?,
        CoordinationError::DeliveryConflict
    );
    sqlx::query("UPDATE agent_waits SET phase='cancelled',cancellation=? WHERE wait_id=?")
        .bind(serde_json::to_string(cause)?)
        .bind(ticket_id.to_string())
        .execute(&mut *conn)
        .await?;
    // Retiring an original invocation resumes its existing work. Superseding
    // input and terminal transactions own their work transition themselves.
    if matches!(
        cause,
        WaitCancellation::Cancelled
            | WaitCancellation::ExecutionFailed
            | WaitCancellation::ShellOutput { .. }
    ) {
        sqlx::query(
            "UPDATE agent_work SET disposition='running',revision=revision+1
            WHERE root_id=? AND actor_id=? AND work_id=? AND disposition='waiting'",
        )
        .bind(caller.root_id.to_string())
        .bind(caller.actor_id.to_string())
        .bind(caller.work_id.to_string())
        .execute(conn)
        .await?;
    }
    Ok(())
}

/// Authoritative work termination closes its subscription in the same work
/// transaction. A dispatched claim keeps every receipt/fence; termination is
/// not permission to release or replay accepted-but-unsampled input.
pub(super) async fn retire_work_wait_in_transaction(
    conn: &mut SqliteConnection,
    caller: WorkStamp,
    runtime_id: Uuid,
    terminal_delivery: Uuid,
) -> anyhow::Result<()> {
    let id: Option<String> = sqlx::query_scalar(
        "SELECT wait_id FROM agent_waits WHERE root_id=? AND caller_id=? AND caller_work_id=?
         AND phase NOT IN ('consumed','cancelled')",
    )
    .bind(caller.root_id.to_string())
    .bind(caller.actor_id.to_string())
    .bind(caller.work_id.to_string())
    .fetch_optional(&mut *conn)
    .await?;
    if let Some(id) = id {
        let id = Uuid::parse_str(&id)?;
        let ticket = required_wait(conn, caller, id).await?;
        let cause = WaitCancellation::WorkTerminated {
            delivery_id: terminal_delivery,
        };
        if ticket.dispatch_id.is_none() {
            cancel_wait_with_cause_in_transaction(
                conn,
                caller,
                runtime_id,
                id,
                &ticket.origin,
                &cause,
            )
            .await?;
        } else {
            sqlx::query("UPDATE agent_waits SET phase='cancelled',cancellation=? WHERE wait_id=?")
                .bind(serde_json::to_string(&cause)?)
                .bind(id.to_string())
                .execute(conn)
                .await?;
        }
    }
    Ok(())
}

async fn next_message(
    conn: &mut SqliteConnection,
    caller: WorkStamp,
) -> anyhow::Result<Option<Uuid>> {
    let id: Option<String> = sqlx::query_scalar(
        "SELECT delivery_id FROM agent_deliveries WHERE root_id=? AND recipient_id=?
         AND kind='message' AND consumed=0 AND claim_id IS NULL AND dispatch_id IS NULL
         ORDER BY sequence LIMIT 1",
    )
    .bind(caller.root_id.to_string())
    .bind(caller.actor_id.to_string())
    .fetch_optional(conn)
    .await?;
    id.map(|id| Uuid::parse_str(&id).map_err(Into::into))
        .transpose()
}

async fn live_caller(
    conn: &mut SqliteConnection,
    caller: WorkStamp,
    runtime_id: Uuid,
) -> anyhow::Result<()> {
    let work = current_work(conn, caller.root_id, caller.actor_id)
        .await?
        .ok_or(CoordinationError::UnknownWork)?;
    anyhow::ensure!(
        work.stamp == caller && !work.disposition.is_terminal(),
        CoordinationError::StaleWork
    );
    anyhow::ensure!(work.runtime_id == runtime_id, WaitError::ColdRuntime);
    Ok(())
}

pub(super) async fn live_wait(
    conn: &mut SqliteConnection,
    caller: WorkStamp,
    runtime_id: Uuid,
    id: WaitId,
) -> anyhow::Result<WaitTicket> {
    live_caller(conn, caller, runtime_id).await?;
    let ticket = required_wait(conn, caller, id).await?;
    anyhow::ensure!(ticket.runtime_id == runtime_id, WaitError::ColdRuntime);
    Ok(ticket)
}

async fn required_wait(
    conn: &mut SqliteConnection,
    caller: WorkStamp,
    id: WaitId,
) -> anyhow::Result<WaitTicket> {
    read_wait(conn, caller, id)
        .await?
        .ok_or_else(|| WaitError::UnknownWait.into())
}

async fn read_wait(
    conn: &mut SqliteConnection,
    caller: WorkStamp,
    id: WaitId,
) -> anyhow::Result<Option<WaitTicket>> {
    let row = sqlx::query(
        "SELECT * FROM agent_waits WHERE wait_id=? AND root_id=? AND caller_id=? AND caller_work_id=?",
    ).bind(id.to_string()).bind(caller.root_id.to_string()).bind(caller.actor_id.to_string())
        .bind(caller.work_id.to_string()).fetch_optional(&mut *conn).await?;
    let Some(row) = row else { return Ok(None) };
    let selection = sqlx::query(
        "SELECT actor_id,work_id,claimed,claimed_disposition,claimed_ordinal FROM agent_wait_selection WHERE wait_id=? ORDER BY ordinal",
    ).bind(id.to_string()).fetch_all(&mut *conn).await?;
    let mut selected = Vec::with_capacity(selection.len());
    let mut outcomes = Vec::new();
    for item in selection {
        let stamp = WorkStamp {
            root_id: caller.root_id,
            actor_id: ThreadId::from_string(item.try_get("actor_id")?)?,
            work_id: Uuid::parse_str(item.try_get("work_id")?)?,
        };
        selected.push(stamp);
        if item.try_get::<bool, _>("claimed")? {
            outcomes.push((
                item.try_get::<i64, _>("claimed_ordinal")?,
                WaitOutcome {
                    stamp,
                    disposition: WorkDisposition::parse(item.try_get("claimed_disposition")?)?,
                },
            ));
        }
    }
    outcomes.sort_by_key(|(ordinal, _)| *ordinal);
    let outcomes = outcomes.into_iter().map(|(_, outcome)| outcome).collect();
    let claim_id = row
        .try_get::<Option<&str>, _>("claim_id")?
        .map(Uuid::parse_str)
        .transpose()?;
    let claim = if let Some(claim_id) = claim_id {
        let ids: Vec<String> = sqlx::query_scalar(
            "SELECT d.delivery_id FROM agent_wait_deliveries wd JOIN agent_deliveries d
             ON d.delivery_id=wd.delivery_id WHERE wd.wait_id=? ORDER BY d.sequence",
        )
        .bind(id.to_string())
        .fetch_all(&mut *conn)
        .await?;
        Some(WaitClaim {
            id: claim_id,
            wake: match row.try_get::<&str, _>("claim_wake")? {
                "events" => WaitWake::Events,
                "deadline" => WaitWake::Deadline,
                "progress" => WaitWake::Progress,
                value => anyhow::bail!("invalid persisted wait wake: {value}"),
            },
            delivery_ids: ids
                .iter()
                .map(|id| Uuid::parse_str(id))
                .collect::<Result<_, _>>()?,
            outcomes,
            metrics: row
                .try_get::<Option<&str>, _>("claim_metrics")?
                .map(serde_json::from_str)
                .transpose()?,
        })
    } else {
        None
    };
    Ok(Some(WaitTicket {
        id,
        caller,
        runtime_id: Uuid::parse_str(row.try_get("runtime_id")?)?,
        origin: match row.try_get::<&str, _>("origin")? {
            "cell" => WaitOrigin::Cell {
                cell: WaitCell {
                    turn_id: row.try_get("turn_id")?,
                    item_id: row.try_get("item_id")?,
                    window_id: row.try_get("window_id")?,
                    cell_id: row.try_get("cell_id")?,
                },
            },
            "turn" => WaitOrigin::Turn {
                turn_id: row.try_get("turn_id")?,
                reason: TurnWaitReason::parse(row.try_get("turn_reason")?)?,
            },
            value => anyhow::bail!("invalid persisted wait origin: {value}"),
        },
        options: WaitOptions {
            mode: match row.try_get::<&str, _>("mode")? {
                "any" => WaitMode::Any,
                "all" => WaitMode::All,
                value => anyhow::bail!("invalid persisted wait mode: {value}"),
            },
            timeout: row
                .try_get::<Option<i64>, _>("timeout_requested_ms")?
                .map(|requested_ms| -> anyhow::Result<_> {
                    Ok(WaitTimeout {
                        requested_ms,
                        effective_ms: row.try_get("timeout_effective_ms")?,
                    })
                })
                .transpose()?,
            progress_interval_ms: row
                .try_get::<Option<i64>, _>("progress_interval_ms")?
                .map(u64::try_from)
                .transpose()?,
            compact: row.try_get("compact")?,
            reload: row
                .try_get::<Option<&str>, _>("reload_selection")?
                .map(serde_json::from_str)
                .transpose()?,
        },
        deadline_unix_ms: row.try_get("deadline_unix_ms")?,
        reset: match row.try_get::<&str, _>("reset_state")? {
            "not_requested" => WaitResetState::NotRequested,
            "pending" => WaitResetState::Pending,
            "running" => WaitResetState::Running,
            "completed" => WaitResetState::Completed,
            "failed" => WaitResetState::Failed {
                diagnostic: row.try_get("reset_diagnostic")?,
            },
            value => anyhow::bail!("invalid persisted wait reset state: {value}"),
        },
        phase: WaitPhase::parse(row.try_get("phase")?)?,
        selected,
        claim,
        dispatch_id: row
            .try_get::<Option<&str>, _>("dispatch_id")?
            .map(Uuid::parse_str)
            .transpose()?,
        accepted_receipt: row.try_get("accepted_receipt")?,
        cancellation: row
            .try_get::<Option<&str>, _>("cancellation")?
            .map(serde_json::from_str)
            .transpose()?,
    }))
}

async fn set_phase(conn: &mut SqliteConnection, id: WaitId, phase: &str) -> anyhow::Result<()> {
    sqlx::query("UPDATE agent_waits SET phase=? WHERE wait_id=?")
        .bind(phase)
        .bind(id.to_string())
        .execute(conn)
        .await?;
    Ok(())
}

// live_wait checked current identity/runtime/nonterminal state in this same
// immediate transaction. Keep these coupled changes off a second transaction.
async fn set_wait_work_disposition(
    conn: &mut SqliteConnection,
    caller: WorkStamp,
    disposition: WorkDisposition,
) -> anyhow::Result<()> {
    sqlx::query(
        "UPDATE agent_work SET disposition=?,revision=revision+1
         WHERE root_id=? AND actor_id=? AND work_id=? AND disposition!=?",
    )
    .bind(disposition.as_str())
    .bind(caller.root_id.to_string())
    .bind(caller.actor_id.to_string())
    .bind(caller.work_id.to_string())
    .bind(disposition.as_str())
    .execute(conn)
    .await?;
    Ok(())
}

#[cfg(test)]
#[path = "coordination_wait_tests.rs"]
mod tests;

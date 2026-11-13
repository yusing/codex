//! Derives owned completed identities at the existing history projection boundary.
//! A strict bootstrap handles old, compressed and inherited histories. Unchanged
//! observations reuse that boundary; ordinary appends update it with item projection.

use super::LocalThreadStore;
use super::rollout_lineage::AncestorReads;
use super::rollout_lineage::RolloutLineage;
use crate::ThreadStoreError;
use crate::ThreadStoreResult;
use codex_protocol::ThreadId;
use codex_protocol::items::TurnItem;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::ThreadHistoryMode;
use codex_rollout::RolloutItem;
use codex_rollout::parse_rollout_line_bytes;
use codex_state::SqlitePoolWriteExt;
use std::collections::HashSet;
use std::io::BufRead;
use std::io::BufReader;
use std::io::Read;
use std::io::Seek;
use std::io::SeekFrom;
use std::path::Path;
use std::path::PathBuf;

struct Segment {
    rollout_id: ThreadId,
    path: PathBuf,
    end: Option<codex_protocol::protocol::HistoryPosition>,
}

#[derive(Default)]
struct Counted {
    count: Option<u64>,
    byte_offset: Option<u64>,
    next_ordinal: Option<u64>,
    completions: Vec<(ThreadId, String, String)>,
}

pub(super) async fn read(
    store: &LocalThreadStore,
    thread_id: ThreadId,
) -> ThreadStoreResult<Option<u64>> {
    let Some(source) =
        super::thread_rollout_resolver::resolve_current_including_archived(store, thread_id)
            .await?
    else {
        return Ok(None);
    };
    let before = super::history_revision::read(&source.path).await;
    let pool = if store.state_db.is_some() {
        Some(store.thread_history_db().await?)
    } else {
        None
    };
    if let Some(pool) = pool {
        let cached = sqlx::query_as::<_, (Option<String>, Option<i64>, Option<i64>, Option<String>)>(
            "SELECT compaction_owner_id, completed_compactions, compaction_byte_offset, compaction_revision FROM thread_history_projection_state WHERE thread_id = ?",
        ).bind(source.rollout_id.to_string()).fetch_optional(pool).await.map_err(error)?;
        if let Some((Some(owner), count, offset, revision)) = cached
            && owner == thread_id.to_string()
        {
            if before.is_some() && revision == before {
                return count
                    .map(|value| u64::try_from(value).map_err(error))
                    .transpose();
            }
            // Native projection has already processed this complete plain prefix.
            // Compressed representations instead reuse their opaque revision above.
            if revision.is_none()
                && count.is_some()
                && codex_rollout::plain_rollout_path(&source.path) == source.path
                && let Ok(metadata) = tokio::fs::metadata(&source.path).await
                && offset.and_then(|value| u64::try_from(value).ok()) == Some(metadata.len())
                && before.is_some()
                && before == super::history_revision::read(&source.path).await
            {
                return count
                    .map(|value| u64::try_from(value).map_err(error))
                    .transpose();
            }
        }
    }
    let meta = codex_rollout::read_session_meta_line(&source.path)
        .await
        .map_err(error)?
        .meta;
    if meta.id != thread_id {
        return Err(ThreadStoreError::InvalidRequest {
            message: "compaction source belongs to another thread".to_owned(),
        });
    }
    let initial_ordinal = meta
        .history_base
        .map_or(0, |base| base.end_ordinal_exclusive);
    let counted = if meta.history_mode == ThreadHistoryMode::Paginated {
        store
            .resolve_rollout_lineage(thread_id, Some(source.path.clone()))
            .await?
            .read_with_fallback(store, |lineage| scan_lineage(thread_id, lineage))
            .await?
    } else {
        scan(
            thread_id,
            vec![Segment {
                rollout_id: source.rollout_id,
                path: source.path.clone(),
                end: None,
            }],
            AncestorReads::default(),
            false,
        )
        .await?
    };
    if before.is_none() || before != super::history_revision::read(&source.path).await {
        return Ok(None);
    }
    if let Some(pool) = pool {
        // Do not serialize disk reads with projection writes. Publish only after
        // reacquiring the writer and checking the exact source snapshot again.
        let _writer = store.thread_history_writer.acquire().await.map_err(error)?;
        if before != super::history_revision::read(&source.path).await {
            return Ok(None);
        }
        let mut transaction = pool.begin_immediate().await.map_err(error)?;
        sqlx::query("INSERT INTO thread_history_projection_state (thread_id, next_rollout_byte_offset, next_rollout_ordinal) VALUES (?, 0, ?) ON CONFLICT(thread_id) DO NOTHING")
            .bind(source.rollout_id.to_string()).bind(i64::try_from(initial_ordinal).map_err(error)?)
            .execute(&mut *transaction).await.map_err(error)?;
        for (rollout, turn, item) in &counted.completions {
            sqlx::query("UPDATE thread_items SET compaction_actor_id = ? WHERE thread_id = ? AND turn_id = ? AND item_id = ? AND item_type = 'contextCompaction' AND completed_at_ms IS NOT NULL")
                .bind(thread_id.to_string()).bind(rollout.to_string()).bind(turn).bind(item)
                .execute(&mut *transaction).await.map_err(error)?;
        }
        sqlx::query("UPDATE thread_history_projection_state SET completed_compactions = ?, compaction_owner_id = ?, compaction_byte_offset = ?, compaction_next_ordinal = ?, compaction_revision = ? WHERE thread_id = ?")
            .bind(counted.count.map(i64::try_from).transpose().map_err(error)?)
            .bind(thread_id.to_string())
            .bind(counted.byte_offset.map(i64::try_from).transpose().map_err(error)?)
            .bind(counted.next_ordinal.map(i64::try_from).transpose().map_err(error)?)
            .bind(before.as_deref()).bind(source.rollout_id.to_string())
            .execute(&mut *transaction).await.map_err(error)?;
        if before != super::history_revision::read(&source.path).await {
            return Ok(None);
        }
        transaction.commit().await.map_err(error)?;
    }
    Ok(counted.count)
}

async fn scan_lineage(thread_id: ThreadId, lineage: RolloutLineage) -> ThreadStoreResult<Counted> {
    let segments = lineage
        .segments()
        .iter()
        .map(|segment| Segment {
            rollout_id: segment.rollout_id,
            path: segment.rollout_path.clone(),
            end: segment.end,
        })
        .collect();
    scan(thread_id, segments, lineage.reads, true).await
}

async fn scan(
    thread_id: ThreadId,
    segments: Vec<Segment>,
    reads: AncestorReads,
    paginated: bool,
) -> ThreadStoreResult<Counted> {
    let (stop, keep_running) = tokio::sync::oneshot::channel::<()>();
    let result = tokio::task::spawn_blocking(move || {
        let mut completed = HashSet::new();
        let mut result = Counted::default();
        for segment in segments {
            let mut file = reads
                .open(&segment.path, || stop.is_closed())
                .map_err(error)?;
            let end = match segment.end {
                Some(end) => end.end_byte_offset,
                None => file.seek(SeekFrom::End(0)).map_err(error)?,
            };
            file.rewind().map_err(error)?;
            let mut reader = BufReader::new(file.take(end));
            let mut bytes = Vec::new();
            let mut offset = 0_u64;
            let mut next_ordinal = None;
            let mut owner = None;
            loop {
                if stop.is_closed() {
                    return Err(error("compaction observation cancelled"));
                }
                bytes.clear();
                let length = reader.read_until(b'\n', &mut bytes).map_err(|failure| {
                    reads.record_failure(&segment.path);
                    error(failure)
                })?;
                if length == 0 {
                    break;
                }
                if !bytes.ends_with(b"\n") {
                    return incomplete(&reads, &segment.path);
                }
                offset += length as u64;
                if bytes.iter().all(u8::is_ascii_whitespace) {
                    continue;
                }
                let line = match parse_rollout_line_bytes(&bytes) {
                    Ok(line) => line,
                    Err(_) => return incomplete(&reads, &segment.path),
                };
                if paginated {
                    let Some(ordinal) = line.ordinal else {
                        return incomplete(&reads, &segment.path);
                    };
                    if next_ordinal.is_some_and(|expected| expected != ordinal) {
                        return incomplete(&reads, &segment.path);
                    }
                    next_ordinal = ordinal.checked_add(1);
                }
                match &line.item {
                    RolloutItem::SessionMeta(meta) => owner = Some(meta.meta.id),
                    RolloutItem::EventMsg(EventMsg::ThreadSettingsApplied(event))
                        if event.thread_id == Some(thread_id) =>
                    {
                        owner = Some(thread_id)
                    }
                    RolloutItem::EventMsg(event @ EventMsg::ContextCompacted(_))
                        if !paginated && owner == Some(thread_id) =>
                    {
                        // Legacy storage omits the modern completion item. Reuse
                        // its native canonicalizer; decoded record coordinates
                        // identify the retained completion, not a context window.
                        let item = super::rollout_migration::legacy_event::completed_item(
                            event,
                            &mut || {
                                Ok(format!("legacy-compaction:{}:{offset}", segment.rollout_id))
                            },
                        )?;
                        if let Some((TurnItem::ContextCompaction(item), _)) = item {
                            completed.insert(item.id);
                        }
                    }
                    _ => {}
                }
                if let RolloutItem::EventMsg(EventMsg::ItemCompleted(event)) = line.item
                    && event.thread_id == thread_id
                    && let TurnItem::ContextCompaction(item) = event.item
                    && !item.id.is_empty()
                {
                    completed.insert(item.id.clone());
                    result
                        .completions
                        .push((segment.rollout_id, event.turn_id, item.id));
                }
            }
            if offset != end {
                reads.record_failure(&segment.path);
                return Err(error("compaction source ended before its frozen boundary"));
            }
            if segment
                .end
                .is_some_and(|end| next_ordinal != Some(end.end_ordinal_exclusive))
            {
                return incomplete(&reads, &segment.path);
            }
            result.byte_offset = Some(offset);
            result.next_ordinal = next_ordinal;
        }
        result.count = Some(completed.len() as u64);
        Ok(result)
    })
    .await
    .map_err(error)?;
    drop(keep_running);
    result
}

fn incomplete(reads: &AncestorReads, path: &Path) -> ThreadStoreResult<Counted> {
    if reads.tracks(path) {
        reads.record_failure(path);
        return Err(error("incomplete frozen ancestor compaction evidence"));
    }
    Ok(Counted::default())
}

fn error(failure: impl std::fmt::Display) -> ThreadStoreError {
    ThreadStoreError::Internal {
        message: format!("failed to observe completed compactions: {failure}"),
    }
}

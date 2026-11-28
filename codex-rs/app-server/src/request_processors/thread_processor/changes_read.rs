use std::collections::BTreeMap;
use std::collections::HashMap;
use std::collections::HashSet;
use std::io;
use std::path::PathBuf;

use codex_app_server_protocol::ClientResponsePayload;
use codex_app_server_protocol::JSONRPCErrorError;
use codex_app_server_protocol::ThreadChangeCallerCounts;
use codex_app_server_protocol::ThreadChangeFileDiff;
use codex_app_server_protocol::ThreadChangeOrigin;
use codex_app_server_protocol::ThreadChangesLiveParams;
use codex_app_server_protocol::ThreadChangesLiveResponse;
use codex_app_server_protocol::ThreadChangesReadParams;
use codex_app_server_protocol::ThreadChangesReadResponse;
use codex_change_history::ChangeAuthor;
use codex_change_history::ChangeHistoryStore;
use codex_change_history::ChangeRecord;
use codex_change_history::ComposedFileDiff;
use codex_change_history::CoverageStatus;
use codex_change_history::FileEvidence;
use codex_change_history::captured_diff_files;
use codex_change_history::captured_diff_range_by;
use codex_change_history::captured_file_stats;
use codex_change_history::compose_diff;
use codex_change_history::compose_file_stats;
use codex_protocol::ThreadId;

use super::ThreadRequestProcessor;
use super::thread_read_view_error;
use crate::error_code::internal_error;
use crate::error_code::invalid_request;

// Account for JSON escaping, provenance and the JSON-RPC envelope, not just
// the bytes of unified diff text.
const CAPTURE_PAGE_BYTES: usize = 4 << 20;
const CAPTURE_PATCH_BYTES: usize = 512 << 10;
const MAX_RESPONSE_BYTES: usize = 60 << 20;

#[cfg(test)]
#[path = "changes_read_tests.rs"]
mod tests;

impl ThreadRequestProcessor {
    #[tracing::instrument(level = "debug", skip(self))]
    pub(crate) async fn thread_changes_live(
        &self,
        params: ThreadChangesLiveParams,
    ) -> Result<Option<ClientResponsePayload>, JSONRPCErrorError> {
        let id = ThreadId::from_string(&params.thread_id)
            .map_err(|error| invalid_request(format!("invalid thread ID: {error}")))?;
        let data = if let Ok(thread) = self.thread_manager.get_thread(id).await {
            self.thread_state_manager
                .live_command_changes(id, &thread)
                .await
        } else {
            self.read_stored_thread_for_read(id, /*include_history*/ false)
                .await
                .map_err(thread_read_view_error)?
                .ok_or_else(|| invalid_request(format!("unknown thread: {id}")))?;
            Vec::new()
        };
        Ok(Some(ThreadChangesLiveResponse { data }.into()))
    }

    pub(crate) async fn thread_changes_read(
        &self,
        params: ThreadChangesReadParams,
    ) -> Result<Option<ClientResponsePayload>, JSONRPCErrorError> {
        let id = ThreadId::from_string(&params.thread_id)
            .map_err(|error| invalid_request(format!("invalid thread ID: {error}")))?;
        let (root, workspace) = if let Ok(thread) = self.thread_manager.get_thread(id).await {
            (
                thread.agent_root_id(),
                Some(thread.config().await.cwd.display().to_string()),
            )
        } else {
            let mut root = id;
            let mut workspace = None;
            loop {
                let stored = self
                    .read_stored_thread_for_read(root, /*include_history*/ false)
                    .await
                    .map_err(thread_read_view_error)?
                    .ok_or_else(|| invalid_request(format!("unknown thread: {root}")))?;
                if workspace.is_none() {
                    workspace = Some(stored.cwd.display().to_string());
                }
                match stored.parent_thread_id {
                    Some(parent) => root = parent,
                    None => break,
                }
            }
            (root, workspace)
        };
        let member_scope =
            if root == id && params.item_id.is_none() && params.receipt_item_ids.is_none() {
                None
            } else {
                Some(id.to_string())
            };
        let dir =
            codex_protocol::shell_environment::session_tmp_path(root.into()).join("change-history");
        let cursor = params.cursor.as_deref().map(parse_cursor).transpose()?;
        let response = tokio::task::spawn_blocking(move || {
            read_changes(dir, workspace, member_scope, params, cursor)
        })
        .await
        .map_err(|error| internal_error(format!("could not read captured edits: {error}")))??;
        Ok(Some(ClientResponsePayload::ThreadChangesRead(response)))
    }
}

fn parse_cursor(value: &str) -> Result<(u64, usize, usize), JSONRPCErrorError> {
    let Some((sequence, offset)) = value.split_once(':') else {
        return Err(invalid_request("invalid captured edit cursor"));
    };
    let (offset, patch) = offset.split_once(':').unwrap_or((offset, "0"));
    sequence
        .parse()
        .ok()
        .zip(offset.parse().ok())
        .zip(patch.parse().ok())
        .map(|((sequence, offset), patch)| (sequence, offset, patch))
        .ok_or_else(|| invalid_request("invalid captured edit cursor"))
}

fn read_changes(
    dir: PathBuf,
    workspace: Option<String>,
    member_scope: Option<String>,
    params: ThreadChangesReadParams,
    cursor: Option<(u64, usize, usize)>,
) -> Result<ThreadChangesReadResponse, JSONRPCErrorError> {
    let read_error = |error| internal_error(format!("could not read captured edits: {error}"));
    let mut response = ThreadChangesReadResponse {
        thread_id: params.thread_id,
        workspace,
        changes: Vec::new(),
        files: Vec::new(),
        file_count: 0,
        next_cursor: None,
        incomplete: false,
        callers: Vec::new(),
        total_additions: 0,
        total_deletions: 0,
    };
    match std::fs::metadata(&dir) {
        Ok(_) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(response),
        Err(error) => {
            return Err(internal_error(format!(
                "could not inspect captured edits: {error}"
            )));
        }
    }
    let store = ChangeHistoryStore::open_existing(&dir).map_err(read_error)?;
    let entries = store.list_entries().map_err(read_error)?;
    let (sequence, offset, patch_offset) =
        cursor.unwrap_or_else(|| (entries.last().map_or(0, |entry| entry.sequence), 0, 0));
    let receipt_ids = params
        .receipt_item_ids
        .as_ref()
        .map(|ids| ids.iter().map(String::as_str).collect::<HashSet<_>>());
    let receipts = receipt_ids.is_some();
    let capture_page = if receipts {
        Some(params.limit.unwrap_or(100))
    } else {
        params.limit.filter(|_| params.per_capture)
    };
    if patch_offset != 0 && (receipts || capture_page.is_none()) {
        return Err(invalid_request("invalid captured edit cursor"));
    }
    let include_external = !receipts && params.include_external.unwrap_or(true);
    let include_ephemeral = !receipts && params.include_ephemeral.unwrap_or(false);
    let ids: Vec<_> = entries
        .into_iter()
        .filter(|entry| {
            entry.sequence <= sequence
                && entry.source_kind.is_authored()
                && (include_ephemeral || !entry.ephemeral_only)
                && member_scope
                    .as_ref()
                    .is_none_or(|id| *id == entry.thread_id)
                && params
                    .item_id
                    .as_ref()
                    .is_none_or(|id| *id == entry.call_id)
                && receipt_ids
                    .as_ref()
                    .is_none_or(|ids| ids.contains(entry.call_id.as_str()))
        })
        .map(|entry| entry.id)
        .collect();
    if ids.is_empty() {
        return Ok(response);
    }
    let mut records = store.load_records(&ids).map_err(read_error)?;
    records.retain_mut(|record| {
        if record.is_retained_vcs_diagnostic() {
            return false;
        }
        record.files.retain(|file| match file.scope {
            codex_change_history::ChangeScope::Workspace => true,
            codex_change_history::ChangeScope::External => include_external,
            codex_change_history::ChangeScope::Ephemeral => include_ephemeral,
        });
        !record.files.is_empty()
    });
    if let Some(caller) = params.caller_thread_id.as_ref() {
        records.retain(|record| author_thread(record).as_ref() == Some(caller));
    }
    if let Some(limit) = capture_page {
        read_capture_page(
            &store,
            &records,
            &mut response,
            (sequence, offset, patch_offset),
            limit.clamp(1, 100) as usize,
            receipts,
        )?;
        return Ok(response);
    }
    let separate = params.per_capture || params.caller_thread_id.is_some();
    let mut composed = Vec::new();
    if separate {
        for record in records.iter().rev() {
            let effects =
                compose_diff(&store, std::slice::from_ref(record), &[]).map_err(read_error)?;
            composed.extend(effects);
        }
    } else {
        composed = compose_diff(&store, &records, &[]).map_err(read_error)?;
        let sequence_by_id: BTreeMap<_, _> = records
            .iter()
            .map(|record| (&record.id, record.sequence))
            .collect();
        composed.sort_by_cached_key(|file| {
            std::cmp::Reverse(
                file.stats
                    .change_ids
                    .last()
                    .and_then(|id| sequence_by_id.get(id)),
            )
        });
    }
    response.file_count = composed.len() as u32;
    response.total_additions = composed
        .iter()
        .map(|diff| diff.stats.additions as i64)
        .sum();
    response.total_deletions = composed
        .iter()
        .map(|diff| diff.stats.deletions as i64)
        .sum();
    response.incomplete = composed
        .iter()
        .any(|diff| !diff.stats.coverage_notes.is_empty());
    // The upper sequence keeps later pages on the same append-only snapshot.
    let limit = params
        .limit
        .map_or(usize::MAX, |limit| limit.clamp(1, 100) as usize);
    let end = offset.saturating_add(limit);
    if end < composed.len() {
        response.next_cursor = Some(format!("{sequence}:{end}"));
    }
    let mut page_ids = HashSet::new();
    for diff in composed.into_iter().skip(offset).take(limit) {
        page_ids.extend(diff.stats.change_ids.iter().cloned());
        response.files.push(wire_file(diff));
    }
    let all_origins = params.limit.is_none() && cursor.is_none();
    let caller_index = cursor.is_none() && !separate && params.item_id.is_none();
    let mut callers = BTreeMap::<String, Vec<ChangeRecord>>::new();
    for record in records {
        let author = author_thread(&record);
        let show = all_origins || page_ids.contains(&record.id);
        if show {
            response
                .changes
                .push(wire_origin(&record, origin_paths(&record)));
        }
        if caller_index && let Some(author) = author {
            callers.entry(author).or_default().push(record);
        }
    }
    for (thread_id, records) in callers {
        let workspaces: BTreeMap<_, _> = records
            .iter()
            .map(|record| (&record.id, &record.metadata.workspace_root))
            .collect();
        let files = compose_file_stats(&store, &records, &[]).map_err(read_error)?;
        let (mut additions, mut deletions) = (0, 0);
        for file in &files {
            if file.coverage_notes.is_empty()
                && file
                    .change_ids
                    .last()
                    .and_then(|id| workspaces.get(id))
                    .is_some_and(|workspace| file.abs_path.starts_with(workspace))
            {
                additions += file.additions as i64;
                deletions += file.deletions as i64;
            }
        }
        response.callers.push(ThreadChangeCallerCounts {
            thread_id,
            file_count: files.len() as u32,
            additions,
            deletions,
        });
    }
    if params.limit.is_some() {
        bound_review_page(&mut response, sequence, offset)?;
    } else if serde_json::to_writer(JsonBudget(MAX_RESPONSE_BYTES), &response).is_err() {
        return Err(invalid_request(
            "Saved Diff exceeds the response limit; request a smaller page or capture",
        ));
    }
    Ok(response)
}

fn bound_review_page(
    response: &mut ThreadChangesReadResponse,
    sequence: u64,
    offset: usize,
) -> Result<(), JSONRPCErrorError> {
    let files = std::mem::take(&mut response.files);
    let origins = std::mem::take(&mut response.changes);
    let origin_indices: HashMap<_, _> = origins
        .iter()
        .enumerate()
        .map(|(index, origin)| (origin.change_id.as_str(), index))
        .collect();
    let mut included = HashSet::new();
    let metadata_error = || invalid_request("Saved Diff metadata exceeds the response limit");
    // Account for the summary once, then each file and each distinct origin
    // that would be added. Leave room for a replacement cursor and the envelope.
    let mut budget = JsonBudget(MAX_RESPONSE_BYTES - 1024);
    serde_json::to_writer(&mut budget, &*response).map_err(|_| metadata_error())?;
    for (index, mut file) in files.into_iter().enumerate() {
        let added: Vec<_> = file
            .change_ids
            .iter()
            .filter_map(|id| origin_indices.get(id.as_str()).copied())
            .filter(|index| !included.contains(index))
            .collect();
        let new_origins: Vec<_> = added.iter().map(|&index| &origins[index]).collect();
        let mut remaining = budget;
        if serde_json::to_writer(&mut remaining, &(&file, &new_origins)).is_err() {
            if !response.files.is_empty() {
                response.next_cursor = Some(format!("{sequence}:{}", offset + index));
                break;
            }
            // A patch that cannot fit on its own must not block the rest of
            // the review. Its provenance and counts remain available.
            file.unified_diff = String::new();
            file.coverage_notes
                .push("Patch exceeds the Saved Diff display limit.".to_string());
            remaining = budget;
            serde_json::to_writer(&mut remaining, &(&file, &new_origins))
                .map_err(|_| metadata_error())?;
        }
        budget = remaining;
        included.extend(added);
        response.files.push(file);
    }
    response.changes = origins
        .into_iter()
        .enumerate()
        .filter_map(|(index, origin)| included.contains(&index).then_some(origin))
        .collect();
    Ok(())
}

fn wire_file(diff: ComposedFileDiff) -> ThreadChangeFileDiff {
    let stats = diff.stats;
    ThreadChangeFileDiff {
        path: stats.rel_path,
        absolute_path: stats.abs_path.display().to_string(),
        change_ids: stats
            .change_ids
            .into_iter()
            .map(|id| id.to_string())
            .collect(),
        moved_from: stats.moved_from,
        status: stats.status.to_string(),
        unified_diff: diff.unified_diff,
        additions: stats.additions as i64,
        deletions: stats.deletions as i64,
        is_binary: stats.is_binary,
        coverage_notes: stats.coverage_notes,
        scope: stats.scope.as_str().to_string(),
    }
}

fn wire_origin(record: &ChangeRecord, paths: Vec<String>) -> ThreadChangeOrigin {
    let (source_kind, source_label) = record.metadata.source_kind.review_source();
    ThreadChangeOrigin {
        change_id: record.id.to_string(),
        sequence: record.sequence,
        thread_id: record.metadata.thread_id.clone(),
        author_thread_id: author_thread(record),
        turn_id: record.metadata.turn_id.clone(),
        item_id: record.metadata.call_id.clone(),
        workspace: record.metadata.workspace_root.display().to_string(),
        source_kind: source_kind.to_string(),
        source_label: source_label.to_string(),
        created_at: record.metadata.created_at,
        paths,
        incomplete: record.files.iter().any(|file| !file.coverage.is_complete()),
    }
}

fn origin_paths(record: &ChangeRecord) -> Vec<String> {
    let mut seen = HashSet::new();
    record
        .files
        .iter()
        .filter_map(|file| {
            seen.insert(file.rel_path.as_str())
                .then(|| file.rel_path.clone())
        })
        .collect()
}

fn format_capture_cursor(sequence: u64, index: usize, patch_offset: usize) -> String {
    if patch_offset == 0 {
        format!("{sequence}:{index}")
    } else {
        format!("{sequence}:{index}:{patch_offset}")
    }
}

fn json_escaped_char_len(ch: char) -> usize {
    match ch {
        '"' | '\\' | '\x08' | '\x0c' | '\n' | '\r' | '\t' => 2,
        ch if (ch as u32) < 0x20 => 6,
        ch => ch.len_utf8(),
    }
}

fn json_encoded_len<T: serde::Serialize>(value: &T) -> Option<usize> {
    let mut counter = JsonBudget(usize::MAX);
    serde_json::to_writer(&mut counter, value).ok()?;
    Some(usize::MAX - counter.0)
}

fn estimate_file_metadata_json_len(record: &ChangeRecord, files: &[&FileEvidence]) -> usize {
    let first = files[0];
    let coverage_notes = files
        .iter()
        .filter_map(|file| match &file.coverage {
            CoverageStatus::Complete => None,
            CoverageStatus::Partial { reason } => {
                Some(format!("{}: partial ({reason})", record.id))
            }
            CoverageStatus::MissingBaseline { reason } => {
                Some(format!("{}: missing baseline ({reason})", record.id))
            }
        })
        .collect();
    let probe = ThreadChangeFileDiff {
        path: first.rel_path.clone(),
        absolute_path: first.abs_path.display().to_string(),
        change_ids: vec![record.id.to_string()],
        moved_from: first.moved_from.clone(),
        status: "R".to_string(),
        unified_diff: String::new(),
        additions: i64::MAX,
        deletions: i64::MAX,
        is_binary: false,
        coverage_notes,
        scope: first.scope.as_str().to_string(),
    };
    json_encoded_len(&probe)
        .unwrap_or(usize::MAX)
        .saturating_add(64)
}

fn read_capture_page(
    store: &ChangeHistoryStore,
    records: &[ChangeRecord],
    response: &mut ThreadChangesReadResponse,
    cursor: (u64, usize, usize),
    limit: usize,
    receipts: bool,
) -> Result<(), JSONRPCErrorError> {
    let (sequence, offset, patch_offset) = cursor;
    let captured_files: Vec<_> = records
        .iter()
        .rev()
        .flat_map(|record| captured_diff_files(record).map(move |files| (record, files)))
        .collect();
    response.file_count = captured_files.len() as u32;
    if patch_offset != 0 && offset >= captured_files.len() {
        return Err(invalid_request("invalid captured edit cursor"));
    }
    // Leave room for the continuation, page totals and RPC envelope.
    let mut budget = JsonBudget(CAPTURE_PAGE_BYTES - 1024);
    serde_json::to_writer(&mut budget, &*response)
        .map_err(|_| invalid_request("captured edit metadata exceeds the response limit"))?;
    let mut last_origin_is_full = false;
    for (index, (record, files)) in captured_files.into_iter().enumerate().skip(offset) {
        let start = if index == offset { patch_offset } else { 0 };
        if response.files.len() == limit {
            response.next_cursor = Some(format_capture_cursor(sequence, index, start));
            break;
        }
        let need_origin = response
            .changes
            .last()
            .filter(|previous| previous.change_id == record.id.as_str())
            .is_none();
        let file_meta_reserve = estimate_file_metadata_json_len(record, &files);
        let (origin, origin_is_full, origin_cost) = if need_origin {
            let full = wire_origin(record, origin_paths(record));
            let full_cost = json_encoded_len(&full).unwrap_or(usize::MAX);
            if full_cost
                .saturating_add(file_meta_reserve)
                .saturating_add(64)
                <= budget.0
            {
                (Some(full), true, full_cost)
            } else if response.files.is_empty() {
                // If a capture's complete path list alone exceeds the page budget on a fresh
                // page, fall back to per-page paths so Saved Diff can still paginate and
                // clients can merge origin.paths across pages.
                let minimal = wire_origin(record, vec![files[0].rel_path.clone()]);
                let minimal_cost = json_encoded_len(&minimal).unwrap_or(usize::MAX);
                (Some(minimal), false, minimal_cost)
            } else {
                response.next_cursor = Some(format_capture_cursor(sequence, index, start));
                break;
            }
        } else {
            let path_cost = if last_origin_is_full {
                0
            } else {
                json_encoded_len(&files[0].rel_path).unwrap_or(usize::MAX) + 2
            };
            (None, last_origin_is_full, path_cost)
        };
        let Some(available_diff_budget) = budget
            .0
            .checked_sub(origin_cost.saturating_add(file_meta_reserve))
        else {
            if response.files.is_empty() {
                return Err(invalid_request(
                    "captured edit metadata exceeds the response limit",
                ));
            }
            response.next_cursor = Some(format_capture_cursor(sequence, index, start));
            break;
        };
        if available_diff_budget < 64 && !response.files.is_empty() {
            response.next_cursor = Some(format_capture_cursor(sequence, index, start));
            break;
        }
        let capture = if receipts {
            captured_file_stats(store, record, &files).map(|stats| {
                stats.map(|stats| {
                    (
                        ComposedFileDiff {
                            stats,
                            unified_diff: String::new(),
                        },
                        None,
                    )
                })
            })
        } else {
            captured_diff_range_by(
                store,
                record,
                &files,
                start,
                available_diff_budget,
                json_escaped_char_len,
            )
        };
        let Some((diff, next_patch)) = capture
            .map_err(|error| internal_error(format!("could not read captured edits: {error}")))?
        else {
            return Err(invalid_request("invalid captured edit cursor"));
        };
        if next_patch.is_some()
            && diff.unified_diff.len() < CAPTURE_PATCH_BYTES
            && !response.files.is_empty()
        {
            // Avoid splitting a small or escape-heavy file at the tail of a non-empty page
            // when it cannot fit even one slice; start it cleanly on the next page instead.
            response.next_cursor = Some(format_capture_cursor(sequence, index, start));
            break;
        }
        if next_patch.is_some() && diff.unified_diff.is_empty() {
            return Err(invalid_request(
                "captured edit metadata exceeds the response limit",
            ));
        }
        let file = wire_file(diff);
        let mut next_budget = budget;
        // Reserve punctuation as well as the encoded file and its origin.
        let encoded = match origin.as_ref() {
            Some(origin) => serde_json::to_writer(&mut next_budget, &(&file, origin)),
            None if origin_is_full => serde_json::to_writer(&mut next_budget, &(&file, "")),
            None => serde_json::to_writer(&mut next_budget, &(&file, &file.path)),
        };
        if encoded.is_err() {
            if response.files.is_empty() {
                return Err(invalid_request(
                    "captured edit metadata exceeds the response limit",
                ));
            }
            response.next_cursor = Some(format_capture_cursor(sequence, index, start));
            break;
        }
        budget = next_budget;
        if let Some(origin) = origin {
            response.changes.push(origin);
            last_origin_is_full = origin_is_full;
        } else if !last_origin_is_full
            && let Some(previous) = response.changes.last_mut()
            && !previous.paths.iter().any(|path| path == &file.path)
        {
            previous.paths.push(file.path.clone());
        }
        response.incomplete |= !file.coverage_notes.is_empty();
        if start == 0 {
            response.total_additions += file.additions;
            response.total_deletions += file.deletions;
        }
        response.files.push(file);
        if let Some(next) = next_patch {
            response.next_cursor = Some(format_capture_cursor(sequence, index, next));
            break;
        }
    }
    Ok(())
}

#[derive(Clone, Copy)]
struct JsonBudget(usize);

impl io::Write for JsonBudget {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.0 = self
            .0
            .checked_sub(bytes.len())
            .ok_or_else(|| io::Error::other("saved diff response exceeds its byte budget"))?;
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn author_thread(record: &ChangeRecord) -> Option<String> {
    match record.metadata.effective_author() {
        ChangeAuthor::Agent { thread_id } => Some(thread_id),
        ChangeAuthor::Unattributed => None,
    }
}

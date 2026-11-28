use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::fmt;
use std::fmt::Write as _;
use std::path::Component;
use std::path::Path;
use std::path::PathBuf;

use similar::DiffTag;
use similar::TextDiff;

use crate::error::ChangeHistoryError;
use crate::identity::ChangeId;
use crate::model::ChangeRecord;
use crate::model::ChangeScope;
use crate::model::CoverageStatus;
use crate::model::FileEvidence;
use crate::model::FileSnapshot;
use crate::store::ChangeHistoryStore;

/// Per-file statistics and provenance in a composed summary.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FileDiffStats {
    pub rel_path: String,
    pub abs_path: PathBuf,
    pub change_ids: Vec<ChangeId>,
    pub moved_from: Option<String>,
    pub status: char,
    pub additions: usize,
    pub deletions: usize,
    pub is_binary: bool,
    pub coverage_notes: Vec<String>,
    pub scope: ChangeScope,
}

/// Composed summary across one or more [`ChangeRecord`]s.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ComposedSummary {
    pub ids: Vec<ChangeId>,
    pub files: Vec<FileDiffStats>,
    pub total_additions: usize,
    pub total_deletions: usize,
}

/// Composed diff across one or more [`ChangeRecord`]s.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ComposedFileDiff {
    pub unified_diff: String,
    pub stats: FileDiffStats,
}

pub(crate) struct ComposedFileTransition {
    pub(crate) rel_path: String,
    pub(crate) abs_path: PathBuf,
    pub(crate) moved_from: Option<String>,
    pub(crate) before: FileSnapshot,
    pub(crate) after: FileSnapshot,
    pub(crate) coverage_notes: Vec<String>,
    pub(crate) scope: ChangeScope,
    change_ids: Vec<ChangeId>,
}

pub(crate) fn normalize_filter_path(path: &str) -> String {
    let trimmed = path.trim().trim_start_matches("./");
    trimmed.replace('\\', "/")
}

pub(crate) fn path_matches_filters(rel_path: &str, filters: &[String]) -> bool {
    if filters.is_empty() {
        return true;
    }
    let norm_rel = normalize_filter_path(rel_path);
    filters.iter().any(|filter| {
        let norm_filter = normalize_filter_path(filter);
        let norm_filter = norm_filter.trim_end_matches('/');
        norm_rel == norm_filter
            || norm_rel
                .strip_prefix(norm_filter)
                .is_some_and(|rest| rest.starts_with('/'))
    })
}

fn normalize_abs_path(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for comp in path.components() {
        match comp {
            Component::CurDir => {}
            Component::ParentDir => {
                if !out.pop() {
                    out.push(comp.as_os_str());
                }
            }
            Component::Prefix(_) | Component::RootDir | Component::Normal(_) => {
                out.push(comp.as_os_str());
            }
        }
    }
    out
}

/// Resolve user-supplied path_filters into exact normalized absolute paths,
/// expanding rename counterparts (including chains) within each record's own
/// workspace root so a rename in pkg/a never matches a same-named file in pkg/b.
fn select_matching_abs_paths(
    records: &[&ChangeRecord],
    path_filters: &[String],
) -> Option<BTreeSet<PathBuf>> {
    if path_filters.is_empty() {
        return None;
    }
    let mut matched = BTreeSet::new();
    for record in records {
        for file in &record.files {
            let dest_abs = normalize_abs_path(&file.abs_path);
            if path_matches_filters(&file.rel_path, path_filters) {
                matched.insert(dest_abs.clone());
            }
            if let Some(from) = file.moved_from.as_deref()
                && path_matches_filters(from, path_filters)
            {
                matched.insert(dest_abs);
                matched.insert(normalize_abs_path(
                    &record.metadata.workspace_root.join(from),
                ));
            }
        }
    }
    loop {
        let mut added = false;
        for record in records {
            for file in &record.files {
                let Some(from) = file.moved_from.as_deref() else {
                    continue;
                };
                let dest_abs = normalize_abs_path(&file.abs_path);
                let src_abs = normalize_abs_path(&record.metadata.workspace_root.join(from));
                if (matched.contains(&dest_abs) && matched.insert(src_abs.clone()))
                    || (matched.contains(&src_abs) && matched.insert(dest_abs))
                {
                    added = true;
                }
            }
        }
        if !added {
            break;
        }
    }
    Some(matched)
}

pub(crate) fn compose_transitions(
    store: &ChangeHistoryStore,
    records: &[ChangeRecord],
    path_filters: &[String],
) -> Result<Vec<ComposedFileTransition>, ChangeHistoryError> {
    compose_transitions_with_continuity(store, records, path_filters, false)
}

pub(crate) fn compose_transitions_with_continuity(
    store: &ChangeHistoryStore,
    records: &[ChangeRecord],
    path_filters: &[String],
    require_continuity: bool,
) -> Result<Vec<ComposedFileTransition>, ChangeHistoryError> {
    let mut ordered: Vec<&ChangeRecord> = records.iter().collect();
    ordered.sort_by_key(|r| r.sequence);
    let matched_abs = select_matching_abs_paths(&ordered, path_filters);

    let mut by_path: BTreeMap<PathBuf, (u64, ComposedFileTransition)> = BTreeMap::new();
    for record in ordered {
        for file in &record.files {
            let key = normalize_abs_path(&file.abs_path);
            if let Some(allowed) = &matched_abs
                && !allowed.contains(&key)
            {
                continue;
            }
            let before = store.read_file_state(&record.id, &file.before)?;
            let after = store.read_file_state(&record.id, &file.after)?;
            let note = match &file.coverage {
                CoverageStatus::Complete => None,
                CoverageStatus::Partial { reason } => {
                    Some(format!("{}: partial ({reason})", record.id))
                }
                CoverageStatus::MissingBaseline { reason } => {
                    Some(format!("{}: missing baseline ({reason})", record.id))
                }
            };
            if let Some((last_seq, existing)) = by_path.get_mut(&key) {
                if *last_seq == record.sequence {
                    // Duplicate path spelling within the same record: coalesce without
                    // treating the two snapshots as sequential edits.
                    if let Some(note) = note {
                        existing.coverage_notes.push(note);
                    }
                    continue;
                }
                if existing.after != before {
                    if require_continuity {
                        return Err(ChangeHistoryError::DriftConflict {
                            path: file.abs_path.clone(),
                            reason: format!(
                                "discontinuous selection for `{}`: skipped intermediate edits before `{}`",
                                file.rel_path, record.id
                            ),
                        });
                    }
                    existing.coverage_notes.push(format!(
                        "{}: discontinuous baseline (unobserved write before this change)",
                        record.id
                    ));
                }
                *last_seq = record.sequence;
                existing.change_ids.push(record.id.clone());
                existing.after = after;
                existing.rel_path = file.rel_path.clone();
                existing.abs_path = file.abs_path.clone();
                if existing.moved_from.is_none() {
                    existing.moved_from = file.moved_from.clone();
                }
                if let Some(note) = note {
                    existing.coverage_notes.push(note);
                }
            } else {
                let mut coverage_notes = Vec::new();
                if let Some(note) = note {
                    coverage_notes.push(note);
                }
                by_path.insert(
                    key,
                    (
                        record.sequence,
                        ComposedFileTransition {
                            rel_path: file.rel_path.clone(),
                            abs_path: file.abs_path.clone(),
                            moved_from: file.moved_from.clone(),
                            before,
                            after,
                            coverage_notes,
                            scope: file.scope,
                            change_ids: vec![record.id.clone()],
                        },
                    ),
                );
            }
        }
    }

    Ok(by_path.into_values().map(|(_, tr)| tr).collect())
}

/// Compose unified diffs across `records` (in capture order), optionally filtered by `path_filters`.
pub fn compose_diff(
    store: &ChangeHistoryStore,
    records: &[ChangeRecord],
    path_filters: &[String],
) -> Result<Vec<ComposedFileDiff>, ChangeHistoryError> {
    Ok(compose_changed_files(store, records, path_filters)?
        .map(|tr| {
            let diff = tr.text_diff();
            let unified_diff = render_transition_diff(&tr, diff.as_ref());
            let text_counts = diff.as_ref().map(count_lines);
            ComposedFileDiff {
                unified_diff,
                stats: tr.into_stats(text_counts),
            }
        })
        .collect())
}

/// Reviewable files within one capture, in the same order as its composed diff.
/// Group equivalent path spellings without reading any snapshot bytes.
pub fn captured_diff_files(record: &ChangeRecord) -> impl Iterator<Item = Vec<&FileEvidence>> {
    let mut by_path = BTreeMap::<_, Vec<_>>::new();
    for file in &record.files {
        by_path
            .entry(normalize_abs_path(&file.abs_path))
            .or_default()
            .push(file);
    }
    by_path.into_values().filter(|files| {
        let first = files[0];
        first.before != first.after
            || first.moved_from.is_some()
            || files.iter().any(|file| !file.coverage.is_complete())
    })
}

/// Render a UTF-8 range of one captured file's patch without allocating its
/// entire rendered text. The returned cursor is a byte offset in that patch.
pub fn captured_diff_range(
    store: &ChangeHistoryStore,
    record: &ChangeRecord,
    files: &[&FileEvidence],
    offset: usize,
    max_bytes: usize,
) -> Result<Option<(ComposedFileDiff, Option<usize>)>, ChangeHistoryError> {
    captured_diff_range_by(store, record, files, offset, max_bytes, char::len_utf8)
}

/// Render a UTF-8 range of one captured file's patch in a single diff pass,
/// charging each written character against `max_budget` via `char_cost`.
/// The returned cursor is a raw UTF-8 byte offset in that patch.
pub fn captured_diff_range_by(
    store: &ChangeHistoryStore,
    record: &ChangeRecord,
    files: &[&FileEvidence],
    offset: usize,
    max_budget: usize,
    char_cost: impl Fn(char) -> usize,
) -> Result<Option<(ComposedFileDiff, Option<usize>)>, ChangeHistoryError> {
    let Some(transition) = captured_file_transition(store, record, files)? else {
        return Ok(None);
    };
    let mut output = BudgetedDiffRange {
        text: String::new(),
        skip: offset,
        remaining: max_budget,
        char_cost,
    };
    let diff = transition.text_diff();
    let stopped = write_transition_diff(&mut output, &transition, diff.as_ref()).is_err();
    let text_counts = diff.as_ref().map(count_lines);
    if output.skip != 0 || (offset != 0 && output.text.is_empty()) {
        return Ok(None);
    }
    let next = stopped.then_some(offset + output.text.len());
    Ok(Some((
        ComposedFileDiff {
            unified_diff: output.text,
            stats: transition.into_stats(text_counts),
        },
        next,
    )))
}

/// Compute a captured file's status and line counts without rendering patch text.
pub fn captured_file_stats(
    store: &ChangeHistoryStore,
    record: &ChangeRecord,
    files: &[&FileEvidence],
) -> Result<Option<FileDiffStats>, ChangeHistoryError> {
    Ok(
        captured_file_transition(store, record, files)?.map(|transition| {
            let counts = transition.text_diff().as_ref().map(count_lines);
            transition.into_stats(counts)
        }),
    )
}

fn captured_file_transition(
    store: &ChangeHistoryStore,
    record: &ChangeRecord,
    files: &[&FileEvidence],
) -> Result<Option<ComposedFileTransition>, ChangeHistoryError> {
    let selected = ChangeRecord {
        schema_version: record.schema_version,
        id: record.id.clone(),
        sequence: record.sequence,
        recorded_at_ms: record.recorded_at_ms,
        metadata: record.metadata.clone(),
        files: files.iter().map(|file| (*file).clone()).collect(),
    };
    let transition = compose_changed_files(store, std::slice::from_ref(&selected), &[])?.next();
    Ok(transition)
}

/// Compute per-file statistics across `records` without rendering unified diffs.
pub fn compose_file_stats(
    store: &ChangeHistoryStore,
    records: &[ChangeRecord],
    path_filters: &[String],
) -> Result<Vec<FileDiffStats>, ChangeHistoryError> {
    Ok(compose_changed_files(store, records, path_filters)?
        .map(|tr| {
            let text_counts = tr.text_diff().as_ref().map(count_lines);
            tr.into_stats(text_counts)
        })
        .collect())
}

/// Compute a composed summary across `records`, optionally filtered by `path_filters`.
pub fn compose_summary(
    store: &ChangeHistoryStore,
    records: &[ChangeRecord],
    path_filters: &[String],
) -> Result<ComposedSummary, ChangeHistoryError> {
    let files = compose_file_stats(store, records, path_filters)?;
    let total_additions = files.iter().map(|file| file.additions).sum();
    let total_deletions = files.iter().map(|file| file.deletions).sum();
    let ids = records.iter().map(|r| r.id.clone()).collect();
    Ok(ComposedSummary {
        ids,
        files,
        total_additions,
        total_deletions,
    })
}

fn compose_changed_files(
    store: &ChangeHistoryStore,
    records: &[ChangeRecord],
    path_filters: &[String],
) -> Result<impl Iterator<Item = ComposedFileTransition>, ChangeHistoryError> {
    Ok(compose_transitions(store, records, path_filters)?
        .into_iter()
        .filter(|tr| {
            tr.before != tr.after || tr.moved_from.is_some() || !tr.coverage_notes.is_empty()
        }))
}

/// Render chronological per-record history (including diagnostic records and exit codes).
pub fn render_history(records: &[ChangeRecord], path_filters: &[String]) -> String {
    let mut ordered: Vec<&ChangeRecord> = records.iter().collect();
    ordered.sort_by_key(|r| r.sequence);
    let matched_abs = select_matching_abs_paths(&ordered, path_filters);

    let mut out = String::new();
    for record in ordered {
        let matched_files: Vec<_> = record
            .files
            .iter()
            .filter(|f| match &matched_abs {
                Some(allowed) => allowed.contains(&normalize_abs_path(&f.abs_path)),
                None => true,
            })
            .collect();
        if !path_filters.is_empty() && matched_files.is_empty() {
            continue;
        }
        let exit_str = match record.metadata.exit_code {
            Some(code) => format!(" exit={code}"),
            None => String::new(),
        };
        let _ = writeln!(
            &mut out,
            "{} [{}] thread={} call={}{}",
            record.id,
            record.metadata.source_kind.label(),
            record.metadata.thread_id,
            record.metadata.call_id,
            exit_str
        );
        if let Some(summary) = &record.metadata.summary {
            let _ = writeln!(&mut out, "  summary: {summary}");
        }
        for file in matched_files {
            let coverage_suffix = match &file.coverage {
                CoverageStatus::Complete => String::new(),
                CoverageStatus::Partial { reason } => format!(" [partial: {reason}]"),
                CoverageStatus::MissingBaseline { reason } => {
                    format!(" [missing baseline: {reason}]")
                }
            };
            let _ = writeln!(
                &mut out,
                "  {:?} {}{}",
                file.kind, file.rel_path, coverage_suffix
            );
        }
    }
    out
}

impl ComposedFileTransition {
    fn text_diff(&self) -> Option<TextDiff<'_, '_, '_, str>> {
        fn text(snapshot: &FileSnapshot) -> Option<&str> {
            match snapshot {
                FileSnapshot::Missing => Some(""),
                FileSnapshot::Present(bytes) => FileSnapshot::decode_text(bytes),
            }
        }

        Some(TextDiff::from_lines(
            text(&self.before)?,
            text(&self.after)?,
        ))
    }

    fn into_stats(self, text_counts: Option<(usize, usize)>) -> FileDiffStats {
        let status = if self.moved_from.is_some() {
            'R'
        } else {
            match (&self.before, &self.after) {
                (FileSnapshot::Missing, FileSnapshot::Present(_)) => 'A',
                (FileSnapshot::Present(_), FileSnapshot::Missing) => 'D',
                (FileSnapshot::Present(_), FileSnapshot::Present(_))
                | (FileSnapshot::Missing, FileSnapshot::Missing) => 'M',
            }
        };
        let (additions, deletions) = text_counts.unwrap_or_default();
        FileDiffStats {
            rel_path: self.rel_path,
            abs_path: self.abs_path,
            change_ids: self.change_ids,
            moved_from: self.moved_from,
            status,
            additions,
            deletions,
            is_binary: text_counts.is_none(),
            coverage_notes: self.coverage_notes,
            scope: self.scope,
        }
    }
}

fn count_lines(diff: &TextDiff<'_, '_, '_, str>) -> (usize, usize) {
    let mut additions = 0;
    let mut deletions = 0;
    for op in diff.ops() {
        let (tag, old, new) = op.as_tag_tuple();
        if tag != DiffTag::Equal {
            additions += new.len();
            deletions += old.len();
        }
    }
    (additions, deletions)
}

fn render_transition_diff<'a>(
    tr: &ComposedFileTransition,
    diff: Option<&'a TextDiff<'a, 'a, 'a, str>>,
) -> String {
    let mut out = String::new();
    let _ = write_transition_diff(&mut out, tr, diff);
    out
}

fn write_transition_diff<'a>(
    out: &mut impl fmt::Write,
    tr: &ComposedFileTransition,
    diff: Option<&'a TextDiff<'a, 'a, 'a, str>>,
) -> fmt::Result {
    let from_header = tr.moved_from.as_deref().unwrap_or(tr.rel_path.as_str());
    writeln!(out, "diff --git a/{from_header} b/{}", tr.rel_path)?;
    if let Some(from) = &tr.moved_from {
        writeln!(out, "rename from {from}")?;
        writeln!(out, "rename to {}", tr.rel_path)?;
    }
    for note in &tr.coverage_notes {
        writeln!(out, "# coverage: {note}")?;
    }

    if let Some(diff) = diff {
        write_text_hunks(
            out,
            diff,
            matches!(tr.before, FileSnapshot::Present(_)).then_some(from_header),
            matches!(tr.after, FileSnapshot::Present(_)).then_some(tr.rel_path.as_str()),
        )
    } else {
        let old_label = match &tr.before {
            FileSnapshot::Missing => "/dev/null".to_string(),
            FileSnapshot::Present(_) => format!("a/{from_header}"),
        };
        let new_label = match &tr.after {
            FileSnapshot::Missing => "/dev/null".to_string(),
            FileSnapshot::Present(_) => format!("b/{}", tr.rel_path),
        };
        writeln!(out, "Binary files {old_label} and {new_label} differ")
    }
}

struct BudgetedDiffRange<F> {
    text: String,
    skip: usize,
    remaining: usize,
    char_cost: F,
}

impl<F: Fn(char) -> usize> fmt::Write for BudgetedDiffRange<F> {
    fn write_str(&mut self, value: &str) -> fmt::Result {
        if self.skip >= value.len() {
            self.skip -= value.len();
            return Ok(());
        }
        let value = value.get(self.skip..).ok_or(fmt::Error)?;
        self.skip = 0;
        let mut take = 0;
        for (idx, ch) in value.char_indices() {
            let cost = (self.char_cost)(ch);
            if cost > self.remaining {
                self.text.push_str(&value[..take]);
                return Err(fmt::Error);
            }
            self.remaining -= cost;
            take = idx + ch.len_utf8();
        }
        self.text.push_str(value);
        Ok(())
    }
}

fn write_text_hunks<'a>(
    out: &mut impl fmt::Write,
    diff: &'a TextDiff<'a, 'a, 'a, str>,
    from: Option<&str>,
    to: Option<&str>,
) -> fmt::Result {
    let old_header = from.map_or_else(|| "/dev/null".to_string(), |path| format!("a/{path}"));
    let new_header = to.map_or_else(|| "/dev/null".to_string(), |path| format!("b/{path}"));
    write!(
        out,
        "{}",
        diff.unified_diff()
            .context_radius(3)
            .header(&old_header, &new_header)
    )
}

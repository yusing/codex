use std::fs;
use std::path::Path;
use std::path::PathBuf;

use serde::Deserialize;
use serde::Serialize;

use crate::identity::ChangeId;

pub(crate) const CURRENT_SCHEMA_VERSION: u32 = 1;

/// Attribution owner for a captured change record.
#[derive(Clone, Debug, Eq, PartialEq, Hash, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ChangeAuthor {
    /// Authored by a specific agent thread (gets its own stream, e.g. `amber1`).
    Agent { thread_id: String },
    /// User shell mode (`!` / `/shell`): emits a change ID (`shell1`) attributed to nobody.
    Unattributed,
}

impl ChangeAuthor {
    pub fn from_thread_id(thread_id: &str) -> Self {
        if thread_id.is_empty() {
            Self::Unattributed
        } else {
            Self::Agent {
                thread_id: thread_id.to_string(),
            }
        }
    }

    pub fn is_unattributed(&self) -> bool {
        matches!(self, Self::Unattributed)
    }
}

/// Filesystem scope classification for an observed file change.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChangeScope {
    /// Inside `workspace_root` / cwd.
    #[default]
    Workspace,
    /// Outside `workspace_root`, non-ephemeral (e.g. sibling repo, `~/.config`).
    External,
    /// Ephemeral temp storage (`/tmp`, `$TMPDIR`, `$SESSION_TMP`).
    /// Visible and clickable on the transcript, excluded from agent change history and saved Diff.
    Ephemeral,
}

impl ChangeScope {
    pub fn is_ephemeral(self) -> bool {
        matches!(self, Self::Ephemeral)
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Workspace => "workspace",
            Self::External => "external",
            Self::Ephemeral => "ephemeral",
        }
    }
}

/// Classify an absolute path as `Workspace`, `External`, or `Ephemeral`.
///
/// `session_tmp` is always `Ephemeral`. Otherwise, paths inside `workspace_root`
/// are `Workspace` (even if `workspace_root` itself lives under `/tmp`), paths
/// under system temp roots outside `workspace_root` are `Ephemeral`, and all
/// other paths outside `workspace_root` are `External`.
pub fn classify_change_scope(
    abs_path: &Path,
    workspace_root: &Path,
    session_tmp: Option<&Path>,
) -> ChangeScope {
    let norm_path = canonical_or_self(abs_path);
    if let Some(stmp) = session_tmp {
        let norm_stmp = canonical_or_self(stmp);
        if norm_path.starts_with(&norm_stmp) || abs_path.starts_with(stmp) {
            return ChangeScope::Ephemeral;
        }
    }
    let norm_ws = canonical_or_self(workspace_root);
    if norm_path.starts_with(&norm_ws) || abs_path.starts_with(workspace_root) {
        return ChangeScope::Workspace;
    }
    for root in ephemeral_temp_roots() {
        let norm_root = canonical_or_self(&root);
        if norm_path.starts_with(&norm_root) || abs_path.starts_with(&root) {
            return ChangeScope::Ephemeral;
        }
    }
    ChangeScope::External
}

fn ephemeral_temp_roots() -> Vec<PathBuf> {
    let mut roots = Vec::with_capacity(4);
    if let Ok(session_tmp) = std::env::var("SESSION_TMP")
        && !session_tmp.is_empty()
    {
        roots.push(PathBuf::from(session_tmp));
    }
    if let Ok(tmpdir) = std::env::var("TMPDIR")
        && !tmpdir.is_empty()
    {
        roots.push(PathBuf::from(tmpdir));
    }
    roots.push(std::env::temp_dir());
    #[cfg(unix)]
    {
        roots.push(PathBuf::from("/tmp"));
        roots.push(PathBuf::from("/private/tmp"));
    }
    roots
}

fn canonical_or_self(path: &Path) -> PathBuf {
    if let Ok(canon) = fs::canonicalize(path) {
        return canon;
    }
    if let Some(parent) = path.parent()
        && let Some(name) = path.file_name()
        && let Ok(canon_parent) = fs::canonicalize(parent)
    {
        return canon_parent.join(name);
    }
    path.to_path_buf()
}

/// Provenance of a captured change record.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChangeSourceKind {
    /// Captured from stock `apply_patch` (direct, intercepted, or inside JS `exec`).
    ApplyPatch,
    /// Captured from shell-write observation (MEKUGI-801).
    ShellWrite,
    /// Captured from interactive user-shell (`!`) execution.
    UserShell,
    /// Captured from `mchanges revert`.
    MchangesRevert,
    /// Captured from `mchanges apply`.
    MchangesApply,
    /// Diagnostic VCS mutation (e.g. `git checkout`/`merge`); visible in `--history` only.
    VcsDiagnostic,
}

impl ChangeSourceKind {
    /// Whether this record counts as an authored edit (included in default review and counts).
    pub fn is_authored(self) -> bool {
        match self {
            Self::ApplyPatch
            | Self::ShellWrite
            | Self::UserShell
            | Self::MchangesRevert
            | Self::MchangesApply => true,
            Self::VcsDiagnostic => false,
        }
    }

    /// Short human-readable label for CLI listings and history output.
    pub fn label(self) -> &'static str {
        match self {
            Self::ApplyPatch => "apply_patch",
            Self::ShellWrite => "shell_write",
            Self::UserShell => "user_shell",
            Self::MchangesRevert => "mchanges_revert",
            Self::MchangesApply => "mchanges_apply",
            Self::VcsDiagnostic => "vcs_diagnostic",
        }
    }

    /// Stable saved-review source kind and its user-facing label, in that order.
    pub fn review_source(self) -> (&'static str, &'static str) {
        match self {
            Self::ApplyPatch => ("ApplyPatch", "apply_patch"),
            Self::ShellWrite => ("ShellWrite", "Shell"),
            Self::UserShell => ("UserShell", "User shell"),
            Self::MchangesRevert => ("MchangesRevert", "mchanges revert"),
            Self::MchangesApply => ("MchangesApply", "mchanges apply"),
            Self::VcsDiagnostic => ("VcsDiagnostic", "VCS diagnostic"),
        }
    }
}

/// Honesty marker for per-file baseline/snapshot coverage.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum CoverageStatus {
    Complete,
    Partial { reason: String },
    MissingBaseline { reason: String },
}

impl CoverageStatus {
    pub fn is_complete(&self) -> bool {
        matches!(self, Self::Complete)
    }

    /// Combine pre-edit and post-edit coverage honesty markers.
    pub fn merge(self, after: Self) -> Self {
        match (self, after) {
            (Self::Complete, Self::Complete) => Self::Complete,
            (Self::MissingBaseline { reason }, _) => Self::MissingBaseline { reason },
            (_, Self::MissingBaseline { reason }) => Self::Partial { reason },
            (Self::Partial { reason }, _) | (_, Self::Partial { reason }) => {
                Self::Partial { reason }
            }
        }
    }
}

/// Raw in-memory file bytes observed before or after an edit.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum FileSnapshot {
    Missing,
    Present(Vec<u8>),
}

impl FileSnapshot {
    pub fn is_binary(bytes: &[u8]) -> bool {
        Self::decode_text(bytes).is_none()
    }

    /// Decode file bytes using the same text/binary cutoff as durable evidence.
    pub fn decode_text(bytes: &[u8]) -> Option<&str> {
        let sample_len = bytes.len().min(8192);
        if bytes[..sample_len].contains(&0) {
            return None;
        }
        std::str::from_utf8(bytes).ok()
    }
}

/// Durable content-addressed reference to a persisted file snapshot.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum FileState {
    Missing,
    Present {
        blob_hash: String,
        byte_len: u64,
        is_binary: bool,
    },
}

/// High-level operation observed on a single path.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum FileChangeKind {
    Add,
    Modify,
    Delete,
    Move { from: String },
}

/// Input describing one observed per-file change to persist.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PendingFileChange {
    /// Display/filter path (workspace-relative with forward slashes when inside workspace).
    pub rel_path: String,
    /// Absolute filesystem path where the file was observed.
    pub abs_path: PathBuf,
    /// Optional previous relative path if this file was moved/renamed.
    pub moved_from: Option<String>,
    /// Pre-edit baseline bytes observed before the tool executed.
    pub before: FileSnapshot,
    /// Post-edit bytes observed after the tool executed.
    pub after: FileSnapshot,
    /// Coverage honesty marker for this file.
    pub coverage: CoverageStatus,
    /// Filesystem scope classification (`Workspace`, `External`, or `Ephemeral`).
    pub scope: ChangeScope,
}

/// Immutable persisted per-file change evidence inside a [`ChangeRecord`].
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct FileEvidence {
    pub rel_path: String,
    pub abs_path: PathBuf,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub moved_from: Option<String>,
    pub kind: FileChangeKind,
    pub before: FileState,
    pub after: FileState,
    pub coverage: CoverageStatus,
    #[serde(default)]
    pub scope: ChangeScope,
}

/// Attribution and context metadata for a [`ChangeRecord`].
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ChangeMetadata {
    pub thread_id: String,
    pub session_id: String,
    pub turn_id: String,
    pub call_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub author: Option<ChangeAuthor>,
    pub source_kind: ChangeSourceKind,
    pub workspace_root: PathBuf,
    /// Unix timestamp in seconds.
    pub created_at: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
    /// Optional deduplication/claim key shared across patch and shell-write observers.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub claim_key: Option<String>,
}

impl ChangeMetadata {
    /// Effective attribution owner, preserving compatibility with records written before `author`.
    pub fn effective_author(&self) -> ChangeAuthor {
        if let Some(author) = &self.author {
            return author.clone();
        }
        ChangeAuthor::from_thread_id(&self.thread_id)
    }

    pub fn is_unattributed(&self) -> bool {
        self.effective_author().is_unattributed()
    }
}

/// Immutable published record for a single captured change event.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ChangeRecord {
    pub schema_version: u32,
    pub id: ChangeId,
    pub sequence: u64,
    /// Unix milliseconds when the completed capture was published.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recorded_at_ms: Option<i64>,
    pub metadata: ChangeMetadata,
    pub files: Vec<FileEvidence>,
}

impl ChangeRecord {
    /// Conservatively classifies retained pre-fix shell captures whose authored
    /// command consists solely of git worktree setup/cleanup (and optional
    /// read-only/branch housekeeping) without any other file-writing commands.
    /// Never blacklists paths such as `.worktrees/` and never hides mixed
    /// commands that also perform unrelated writes.
    pub fn is_retained_vcs_diagnostic(&self) -> bool {
        if !matches!(
            self.metadata.source_kind,
            ChangeSourceKind::ShellWrite | ChangeSourceKind::UserShell
        ) {
            return false;
        }
        let Some(script) = self.metadata.summary.as_deref() else {
            return false;
        };
        is_pure_git_worktree_script(script)
    }
}

fn is_pure_git_worktree_script(script: &str) -> bool {
    let Some(commands) = codex_shell_command::bash::parse_sequential_shell_commands(script) else {
        return false;
    };
    let mut saw_worktree_mutation = false;
    for tokens in commands {
        if is_git_worktree_mutation(&tokens) {
            saw_worktree_mutation = true;
        } else if !is_benign_worktree_companion(&tokens) {
            return false;
        }
    }
    saw_worktree_mutation
}

fn git_subcommand(tokens: &[String]) -> Option<(&str, &[String])> {
    let mut idx = 0;
    if *tokens.get(idx)? != "git" {
        return None;
    }
    idx += 1;
    while idx < tokens.len() {
        let tok = tokens[idx].as_str();
        if matches!(
            tok,
            "-C" | "-c" | "--git-dir" | "--work-tree" | "--namespace"
        ) {
            idx += 2;
            continue;
        }
        if tok.starts_with('-') {
            idx += 1;
            continue;
        }
        return Some((tok, &tokens[idx + 1..]));
    }
    None
}

fn is_git_worktree_mutation(tokens: &[String]) -> bool {
    let Some((sub, rest)) = git_subcommand(tokens) else {
        return false;
    };
    if sub != "worktree" {
        return false;
    }
    rest.first().is_some_and(|op| {
        matches!(
            op.as_str(),
            "add" | "remove" | "move" | "repair" | "prune" | "lock" | "unlock"
        )
    })
}

fn is_benign_worktree_companion(tokens: &[String]) -> bool {
    if matches!(
        tokens.first().map(String::as_str),
        Some("cd" | "pwd" | "true")
    ) {
        return true;
    }
    let Some((sub, _)) = git_subcommand(tokens) else {
        return false;
    };
    matches!(sub, "branch" | "status" | "rev-parse" | "worktree")
}

/// Lightweight index entry stored in `index.json` for fast listing and lookup.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ChangeIndexEntry {
    pub id: ChangeId,
    pub sequence: u64,
    pub thread_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub author: Option<ChangeAuthor>,
    pub call_id: String,
    pub source_kind: ChangeSourceKind,
    pub created_at: i64,
    pub exit_code: Option<i32>,
    pub file_paths: Vec<String>,
    pub has_incomplete_coverage: bool,
    /// True when every file in this record is `ChangeScope::Ephemeral` (e.g. `/tmp`).
    #[serde(default)]
    pub ephemeral_only: bool,
}

impl ChangeIndexEntry {
    pub fn effective_author(&self) -> ChangeAuthor {
        if let Some(author) = &self.author {
            return author.clone();
        }
        ChangeAuthor::from_thread_id(&self.thread_id)
    }

    pub fn is_unattributed(&self) -> bool {
        self.effective_author().is_unattributed()
    }
}

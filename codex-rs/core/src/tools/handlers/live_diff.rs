//! Render throttled edit projections against the selected executor's pre-edit text.
use std::collections::HashMap;
use std::fmt::Write;
use std::path::{Path, PathBuf};

use super::resolve_tool_environment;
use crate::environment_selection::TurnEnvironmentSnapshot;
use crate::session::turn_context::TurnEnvironment;
use codex_apply_patch::{Hunk, LIVE_DIFF_CONTEXT, UpdateFileChunk, apply_chunks_to_text};
use codex_protocol::protocol::{
    CodeModeInputDeltaEvent, EventMsg, FileChange, PatchApplyUpdatedEvent,
};
use codex_utils_path_uri::PathUri;
use futures::StreamExt;
use tokio_util::sync::CancellationToken;

pub(crate) const MAX_STREAM_FILES: usize = 32;
pub(crate) const MAX_STREAM_FILE_BYTES: usize = 128 * 1024;

pub(crate) enum ArgumentPreview {
    Patch(PatchPreview),
    Code(CodeModeInputDeltaEvent),
}

pub(crate) struct PatchPreview {
    pub(crate) call_id: String,
    pub(crate) changes: HashMap<PathBuf, ProjectedEdit>,
}

impl PatchPreview {
    pub(crate) fn patch(
        call_id: String,
        hunks: Vec<Hunk>,
        environment_id: Option<&str>,
        base: Option<&Path>,
    ) -> Self {
        Self {
            call_id,
            changes: patch_edits(hunks, environment_id, base),
        }
    }

    #[cfg(test)]
    pub(crate) fn provisional(self) -> PatchApplyUpdatedEvent {
        PatchApplyUpdatedEvent {
            call_id: self.call_id,
            changes: self
                .changes
                .into_iter()
                .map(|(path, edit)| (path, edit.kind.render(None)))
                .collect(),
        }
    }
}

pub(crate) fn patch_edits(
    hunks: Vec<Hunk>,
    environment_id: Option<&str>,
    base: Option<&Path>,
) -> HashMap<PathBuf, ProjectedEdit> {
    hunks
        .into_iter()
        .map(|hunk| {
            let (path, kind) = match hunk {
                Hunk::AddFile { path, contents } => (
                    path,
                    EditKind::Write {
                        content: contents,
                        append: false,
                    },
                ),
                Hunk::DeleteFile { path } => (path, EditKind::Delete),
                Hunk::UpdateFile {
                    path,
                    move_path,
                    chunks,
                } => (path, EditKind::Patch { chunks, move_path }),
            };
            let key = base.map_or_else(|| path.clone(), |base| base.join(&path));
            (
                key,
                ProjectedEdit {
                    source: Some((environment_id.map(str::to_owned), path)),
                    kind,
                },
            )
        })
        .collect()
}

#[derive(Clone)]
pub(crate) struct ProjectedEdit {
    // Kept separate from the provisional display path: a trailing workdir or
    // environment argument can change the selected file.
    pub(crate) source: Option<(Option<String>, PathBuf)>,
    pub(crate) kind: EditKind,
}

#[derive(Clone)]
pub(crate) enum EditKind {
    Write {
        content: String,
        append: bool,
    },
    Delete,
    Patch {
        chunks: Vec<UpdateFileChunk>,
        move_path: Option<PathBuf>,
    },
    Replace(Vec<(String, String)>),
}

fn authored_lines(out: &mut String, prefix: char, lines: &[String]) {
    for line in lines {
        let _ = writeln!(out, "{prefix}{line}");
    }
}

impl EditKind {
    pub(crate) fn render(self, original: Option<&str>) -> FileChange {
        let diff = |before: &str, after: &str| {
            similar::TextDiff::from_lines(before, after)
                .unified_diff()
                .context_radius(LIVE_DIFF_CONTEXT)
                .to_string()
        };
        match self {
            Self::Write { content, append } => match original {
                Some(before) => {
                    let after = if append {
                        format!("{before}{content}")
                    } else {
                        content
                    };
                    FileChange::Update {
                        unified_diff: diff(before, &after),
                        move_path: None,
                    }
                }
                None => FileChange::Add { content },
            },
            Self::Delete => FileChange::Delete {
                content: original.unwrap_or_default().to_owned(),
            },
            Self::Patch { chunks, move_path } => {
                let unified_diff = original
                    .and_then(|before| {
                        apply_chunks_to_text("preview", before, &chunks)
                            .ok()
                            .map(|after| diff(before, &after))
                    })
                    .unwrap_or_else(|| {
                        let mut out = String::new();
                        for c in chunks {
                            out.push_str("@@");
                            if let Some(ctx) = c.change_context {
                                let _ = write!(out, " {ctx}");
                            }
                            out.push('\n');
                            let (mut old, mut new) = (0, 0);
                            for (i, j) in c.context_line_indices {
                                authored_lines(&mut out, '-', &c.old_lines[old..i]);
                                authored_lines(&mut out, '+', &c.new_lines[new..j]);
                                let _ = writeln!(out, " {}", c.old_lines[i]);
                                old = i + 1;
                                new = j + 1;
                            }
                            authored_lines(&mut out, '-', &c.old_lines[old..]);
                            authored_lines(&mut out, '+', &c.new_lines[new..]);
                        }
                        out
                    });
                FileChange::Update {
                    unified_diff,
                    move_path,
                }
            }
            Self::Replace(edits) => {
                let after = original.and_then(|before| {
                    let mut text = before.to_owned();
                    for (old, new) in &edits {
                        if old.is_empty() {
                            return None;
                        }
                        let mut matches = text.match_indices(old.as_str());
                        let (start, _) = matches.next()?;
                        if matches.next().is_some() {
                            return None;
                        }
                        text.replace_range(start..start + old.len(), new);
                    }
                    Some(text)
                });
                let unified_diff = match (original, after) {
                    (Some(before), Some(after)) => diff(before, &after),
                    _ => {
                        let mut out = String::new();
                        for (old, new) in edits {
                            out.push_str("@@\n");
                            for line in old.lines() {
                                let _ = writeln!(out, "-{line}");
                            }
                            for line in new.lines() {
                                let _ = writeln!(out, "+{line}");
                            }
                        }
                        out
                    }
                };
                FileChange::Update {
                    unified_diff,
                    move_path: None,
                }
            }
        }
    }
}

#[derive(Default)]
pub(crate) struct PreviewBaselines {
    files: HashMap<(String, PathUri), Option<String>>,
}

impl PreviewBaselines {
    pub(crate) async fn render(
        &mut self,
        preview: ArgumentPreview,
        environments: &TurnEnvironmentSnapshot,
        cancel: &CancellationToken,
    ) -> EventMsg {
        let patch = match preview {
            ArgumentPreview::Code(event) => return EventMsg::CodeModeInputDelta(event),
            ArgumentPreview::Patch(patch) => patch,
        };
        let mut changes = HashMap::with_capacity(patch.changes.len());
        for (path, edit) in patch.changes {
            let mut original = None;
            if let Some((env_id, raw)) = edit.source
                && let Ok(env) = resolve_tool_environment(environments, env_id.as_deref())
                && let Some(raw) = raw.to_str()
                && let Ok(uri) = env.selection.cwd.join(raw)
            {
                let key = (env.selection.environment_id.clone(), uri);
                if !self.files.contains_key(&key) && self.files.len() < MAX_STREAM_FILES {
                    let baseline = tokio::select! {
                        biased;
                        _=cancel.cancelled()=>None,
                        result=read_baseline(env,&key.1)=>result
                    };
                    self.files.insert(key.clone(), baseline);
                }
                original = self.files.get(&key).and_then(Option::as_deref);
            }
            changes.insert(path, edit.kind.render(original));
        }
        EventMsg::PatchApplyUpdated(PatchApplyUpdatedEvent {
            call_id: patch.call_id,
            changes,
        })
    }
}

async fn read_baseline(env: &TurnEnvironment, path: &PathUri) -> Option<String> {
    let fs = env.environment().get_filesystem();
    let sandbox = env.sandbox_context(None);
    let meta = fs
        .get_metadata(path, Default::default(), Some(&sandbox))
        .await
        .ok()?;
    if !meta.is_file || meta.size > MAX_STREAM_FILE_BYTES as u64 {
        return None;
    }
    let mut stream = fs.read_file_stream(path, Some(&sandbox)).await.ok()?;
    let mut bytes = Vec::with_capacity(meta.size as usize);
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.ok()?;
        if bytes.len() + chunk.len() > MAX_STREAM_FILE_BYTES || chunk.contains(&0) {
            return None;
        }
        bytes.extend_from_slice(&chunk);
    }
    String::from_utf8(bytes).ok()
}

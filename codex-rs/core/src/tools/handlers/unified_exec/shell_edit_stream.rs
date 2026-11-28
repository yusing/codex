//! Incremental model-token-stream projector for authored shell, Python, and
//! Code Mode edits.
//!
//! Projects streaming tool argument deltas (direct `exec_command` JSON and
//! Code Mode `exec` JavaScript calling `tools.exec_command` / `tools.apply_patch`)
//! into `EventMsg::PatchApplyUpdated` so they share the exact same Live Diff
//! rendering pipeline as `apply_patch`.
//!
//! Non-streaming writes whose bytes come from disk rather than the model's
//! token stream (`cat < big > out`, `cp`, `shutil.copy*`, etc.) emit nothing.

use std::collections::HashMap;
use std::collections::HashSet;
use std::path::Path;
use std::path::PathBuf;
use std::time::Duration;
use std::time::Instant;

use crate::tools::handlers::live_diff::{
    ArgumentPreview, EditKind, MAX_STREAM_FILE_BYTES, MAX_STREAM_FILES, PatchPreview,
    ProjectedEdit, patch_edits,
};
use codex_apply_patch::StreamingPatchParser;

use crate::function_tool::FunctionCallError;
use crate::session::step_context::StepContext;
use crate::tools::registry::ToolArgumentDiffConsumer;

const SHELL_EDIT_STREAM_BUFFER_INTERVAL: Duration = Duration::from_millis(100);

#[derive(Default)]
struct ThrottledPatchEmitter {
    call_id: Option<String>,
    last_sent_at: Option<Instant>,
    dirty: bool,
}

impl ThrottledPatchEmitter {
    fn set_call_id(&mut self, call_id: String) {
        self.call_id = Some(call_id);
    }

    fn mark_dirty(&mut self) {
        self.dirty = true;
    }

    fn emit(
        &mut self,
        force: bool,
        snapshot: impl FnOnce() -> HashMap<PathBuf, ProjectedEdit>,
    ) -> Option<PatchPreview> {
        if !self.dirty {
            return None;
        }
        let now = Instant::now();
        if !force
            && let Some(last) = self.last_sent_at
            && now.duration_since(last) < SHELL_EDIT_STREAM_BUFFER_INTERVAL
        {
            return None;
        }
        let call_id = self.call_id.clone()?;
        self.dirty = false;
        let changes = snapshot();
        if changes.is_empty() && self.last_sent_at.is_none() {
            return None;
        }
        self.last_sent_at = Some(now);
        Some(PatchPreview { call_id, changes })
    }
}

#[derive(Default)]
pub(crate) struct ExecCommandArgumentDiffConsumer {
    json: JsonCmdExtractor,
    projector: Option<ShellEditProjector>,
    emitter: ThrottledPatchEmitter,
}

impl ToolArgumentDiffConsumer for ExecCommandArgumentDiffConsumer {
    fn consume_diff(
        &mut self,
        step: &StepContext,
        call_id: String,
        diff: &str,
    ) -> Vec<ArgumentPreview> {
        self.emitter.set_call_id(call_id);
        let projector = self.projector.get_or_insert_with(|| {
            ShellEditProjector::new(
                &step
                    .environments
                    .primary()
                    .map(|env| env.selection.cwd.to_path_buf())
                    .unwrap_or_default(),
            )
        });
        let (cmd_chunk, workdir) = self.json.push(diff);
        if let Some(dir) = workdir {
            projector.set_workdir(Path::new(&dir));
        }
        if self.json.state == JsonState::Complete {
            projector
                .environment_id
                .clone_from(&self.json.environment_id);
            projector.resolve_location();
        }
        if !cmd_chunk.is_empty() {
            projector.push(&cmd_chunk);
        }
        if projector.take_dirty() {
            self.emitter.mark_dirty();
        }
        let Some(projector) = self.projector.as_ref() else {
            return Vec::new();
        };
        self.emitter
            .emit(false, || projector.snapshot_entries())
            .map(ArgumentPreview::Patch)
            .into_iter()
            .collect()
    }

    fn finish(&mut self) -> Result<Vec<ArgumentPreview>, FunctionCallError> {
        if let Some(projector) = self.projector.as_mut() {
            projector.finish();
            if projector.take_dirty() {
                self.emitter.mark_dirty();
            }
        }
        let Some(projector) = self.projector.as_ref() else {
            return Ok(Vec::new());
        };
        Ok(self
            .emitter
            .emit(true, || projector.snapshot_entries())
            .map(ArgumentPreview::Patch)
            .into_iter()
            .collect())
    }
}

// ---------------------------------------------------------------------------
// Code Mode (`exec`) nested edit stream projector
// ---------------------------------------------------------------------------

enum ActiveCodeModeOp {
    Shell {
        projector: ShellEditProjector,
        aborted: bool,
    },
    Patch {
        parser: StreamingPatchParser,
        changes: HashMap<PathBuf, ProjectedEdit>,
        aborted: bool,
    },
}

#[derive(Default)]
pub(crate) struct CodeModeEditProjector {
    base_cwd: Option<PathBuf>,
    emitter: ThrottledPatchEmitter,
    js: JsNestedEditScanner,
    committed: HashMap<PathBuf, ProjectedEdit>,
    active: Option<ActiveCodeModeOp>,
}

impl CodeModeEditProjector {
    fn active_shell_mut(&mut self) -> Option<&mut ShellEditProjector> {
        match self.active.as_mut() {
            Some(ActiveCodeModeOp::Shell {
                projector,
                aborted: false,
            }) => Some(projector),
            _ => None,
        }
    }

    pub(crate) fn consume_diff(
        &mut self,
        cwd: &Path,
        call_id: String,
        diff: &str,
    ) -> Option<PatchPreview> {
        self.emitter.set_call_id(call_id);
        self.base_cwd.get_or_insert_with(|| cwd.to_path_buf());
        for event in self.js.push(diff) {
            match event {
                JsEditEvent::ExecCallStart => {
                    self.finalize_active();
                    let base = self.base_cwd.as_deref().unwrap_or(cwd);
                    self.active = Some(ActiveCodeModeOp::Shell {
                        projector: ShellEditProjector::new(base),
                        aborted: false,
                    });
                }
                JsEditEvent::ExecCmdStart => {
                    if let Some(projector) = self.active_shell_mut() {
                        projector.reset_script_state();
                    }
                }
                JsEditEvent::ExecCmdChunk(chunk) => {
                    if let Some(projector) = self.active_shell_mut() {
                        projector.push(&chunk);
                    }
                }
                JsEditEvent::ExecWorkdir(dir) => {
                    if let Some(projector) = self.active_shell_mut() {
                        projector.set_workdir(Path::new(&dir));
                    }
                }
                JsEditEvent::ExecCmdEnd => {
                    if let Some(projector) = self.active_shell_mut() {
                        projector.finish();
                    }
                }
                JsEditEvent::ExecCallAbort => {
                    if let Some(ActiveCodeModeOp::Shell { aborted, .. }) = self.active.as_mut() {
                        *aborted = true;
                        self.emitter.mark_dirty();
                    }
                }
                JsEditEvent::ExecCallEnd => {
                    if let Some(projector) = self.active_shell_mut() {
                        projector.resolve_location();
                    }
                    self.finalize_active();
                }
                JsEditEvent::ExecEnvironment(id) => {
                    if let Some(projector) = self.active_shell_mut() {
                        projector.environment_id = Some(id);
                    }
                }
                JsEditEvent::ExecUnresolved => {
                    if let Some(projector) = self.active_shell_mut() {
                        projector.ambiguous_location = true;
                    }
                }
                JsEditEvent::ApplyPatchStart => {
                    self.finalize_active();
                    self.active = Some(ActiveCodeModeOp::Patch {
                        parser: StreamingPatchParser::default(),
                        changes: HashMap::new(),
                        aborted: false,
                    });
                }
                JsEditEvent::ApplyPatchChunk(chunk) => {
                    let base = self.base_cwd.as_deref().unwrap_or(cwd);
                    if let Some(ActiveCodeModeOp::Patch {
                        parser,
                        changes,
                        aborted: false,
                    }) = self.active.as_mut()
                        && let Ok(hunks) = parser.push_delta(&chunk)
                        && !hunks.is_empty()
                    {
                        for (path, change) in
                            patch_edits(hunks, parser.environment_id(), Some(base))
                        {
                            changes.insert(path, change);
                        }
                        self.emitter.mark_dirty();
                    }
                }
                JsEditEvent::ApplyPatchAbort => {
                    if let Some(ActiveCodeModeOp::Patch { aborted, .. }) = self.active.as_mut() {
                        *aborted = true;
                        self.emitter.mark_dirty();
                    }
                }
                JsEditEvent::ApplyPatchEnd => {
                    self.finalize_active();
                }
            }
        }
        if let Some(projector) = self.active_shell_mut()
            && projector.take_dirty()
        {
            self.emitter.mark_dirty();
        }
        let committed = &self.committed;
        let active = &self.active;
        self.emitter
            .emit(false, || snapshot_code_mode(committed, active))
    }

    pub(crate) fn finish(&mut self) -> Option<PatchPreview> {
        self.finalize_active();
        let committed = &self.committed;
        let active = &self.active;
        self.emitter
            .emit(true, || snapshot_code_mode(committed, active))
    }

    fn finalize_active(&mut self) {
        let Some(active) = self.active.take() else {
            return;
        };
        match active {
            ActiveCodeModeOp::Shell {
                mut projector,
                aborted,
            } => {
                if aborted {
                    return;
                }
                projector.finish();
                if projector.take_dirty() {
                    self.emitter.mark_dirty();
                }
                merge_shell_snapshot(&mut self.committed, &projector);
            }
            ActiveCodeModeOp::Patch {
                mut parser,
                mut changes,
                aborted,
            } => {
                if aborted {
                    return;
                }
                if let Ok(hunks) = parser.finish()
                    && !hunks.is_empty()
                {
                    let base = self.base_cwd.as_deref().unwrap_or_else(|| Path::new(""));
                    for (path, change) in patch_edits(hunks, parser.environment_id(), Some(base)) {
                        changes.insert(path, change);
                    }
                    self.emitter.mark_dirty();
                }
                if !changes.is_empty() {
                    merge_capped_changes(&mut self.committed, changes);
                }
            }
        }
    }
}

fn snapshot_code_mode(
    committed: &HashMap<PathBuf, ProjectedEdit>,
    active: &Option<ActiveCodeModeOp>,
) -> HashMap<PathBuf, ProjectedEdit> {
    let mut out = committed.clone();
    match active {
        Some(ActiveCodeModeOp::Shell {
            projector,
            aborted: false,
        }) => {
            merge_shell_snapshot(&mut out, projector);
        }
        Some(ActiveCodeModeOp::Patch {
            changes,
            aborted: false,
            ..
        }) => {
            merge_capped_changes(&mut out, changes.clone());
        }
        _ => {}
    }
    out
}

fn merge_shell_snapshot(target: &mut HashMap<PathBuf, ProjectedEdit>, shell: &ShellEditProjector) {
    let entries = shell.snapshot_entries();
    let mut ordered: Vec<_> = entries.into_iter().collect();
    if target.len() + ordered.len() > MAX_STREAM_FILES {
        ordered.sort_by(|(a, _), (b, _)| a.cmp(b));
    }
    for (path, change) in ordered {
        if let EditKind::Write {
            content: added,
            append: true,
        } = &change.kind
            && let Some(ProjectedEdit {
                kind: EditKind::Write {
                    content: existing, ..
                },
                source,
            }) = target.get_mut(&path)
            && *source == change.source
        {
            let remaining = MAX_STREAM_FILE_BYTES.saturating_sub(existing.len());
            if added.len() <= remaining {
                existing.push_str(added);
            } else if remaining > 0 {
                let mut cut = remaining;
                while cut > 0 && !added.is_char_boundary(cut) {
                    cut -= 1;
                }
                existing.push_str(&added[..cut]);
            }
            continue;
        }
        if target.contains_key(&path) || target.len() < MAX_STREAM_FILES {
            target.insert(path, change);
        }
    }
}

fn merge_capped_changes(
    target: &mut HashMap<PathBuf, ProjectedEdit>,
    incoming: HashMap<PathBuf, ProjectedEdit>,
) {
    let mut ordered: Vec<_> = incoming.into_iter().collect();
    if target.len() + ordered.len() > MAX_STREAM_FILES {
        ordered.sort_by(|(a, _), (b, _)| a.cmp(b));
    }
    for (path, change) in ordered {
        if target.contains_key(&path) || target.len() < MAX_STREAM_FILES {
            target.insert(path, change);
        }
    }
}

fn join_path(base: &Path, target: &Path) -> PathBuf {
    if target.is_absolute() || base.as_os_str().is_empty() {
        target.to_path_buf()
    } else {
        base.join(target)
    }
}

enum JsEditEvent {
    ExecCallStart,
    ExecCmdStart,
    ExecCmdChunk(String),
    ExecWorkdir(String),
    ExecEnvironment(String),
    ExecUnresolved,
    ExecCmdEnd,
    ExecCallAbort,
    ExecCallEnd,
    ApplyPatchStart,
    ApplyPatchChunk(String),
    ApplyPatchAbort,
    ApplyPatchEnd,
}

#[derive(Default, Clone, Copy, PartialEq, Eq)]
enum JsCommentState {
    #[default]
    None,
    SlashPending,
    Line,
    Block,
    BlockStar,
    Regex {
        escaped: bool,
        in_class: bool,
    },
}

struct JsNestedEditScanner {
    ident: String,
    prev_ident: String,
    after_dot: bool,
    regex_allowed: bool,
    state: JsScanState,
    comment: JsCommentState,
    escape: EscapeDecoder,
}

impl Default for JsNestedEditScanner {
    fn default() -> Self {
        Self {
            ident: String::new(),
            prev_ident: String::new(),
            after_dot: false,
            regex_allowed: true,
            state: JsScanState::Normal,
            comment: JsCommentState::None,
            escape: EscapeDecoder::default(),
        }
    }
}

enum JsStringTarget {
    Patch,
    ExecField {
        brace_depth: usize,
        field: TargetField,
        workdir_buf: String,
    },
}

#[derive(Default)]
enum JsScanState {
    #[default]
    Normal,
    SkipString {
        quote: char,
    },
    AwaitPatchArg,
    InExecArgs {
        brace_depth: usize,
        expect_value_for: Option<TargetField>,
        pending_key: String,
    },
    InString {
        quote: char,
        target: JsStringTarget,
        pending_dollar: bool,
        interpolated: bool,
        interp_brace_depth: usize,
    },
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum EscapeDialect {
    Json,
    Js,
    Python,
}

#[derive(Default, Clone, Copy)]
enum EscapeState {
    #[default]
    None,
    Backslash,
    Hex {
        remaining: u8,
        acc: u32,
        is_u16: bool,
    },
}

#[derive(Default, Clone, Copy)]
struct EscapeDecoder {
    state: EscapeState,
    high_surrogate: Option<u16>,
}

enum DecodedEscape {
    None,
    CloseQuote,
    Char(char),
    TwoChars(char, char),
}

impl EscapeDecoder {
    fn is_idle(&self) -> bool {
        matches!(self.state, EscapeState::None)
    }

    fn begin_backslash(&mut self) {
        self.state = EscapeState::Backslash;
    }

    fn feed(&mut self, dialect: EscapeDialect, ch: char, quote: char) -> DecodedEscape {
        match self.state {
            EscapeState::None => {
                if ch == '\\' {
                    self.state = EscapeState::Backslash;
                    DecodedEscape::None
                } else if ch == quote {
                    self.high_surrogate = None;
                    DecodedEscape::CloseQuote
                } else {
                    self.high_surrogate = None;
                    DecodedEscape::Char(ch)
                }
            }
            EscapeState::Backslash => {
                self.state = EscapeState::None;
                match (dialect, ch) {
                    (_, 'n') => DecodedEscape::Char('\n'),
                    (_, 'r') => DecodedEscape::Char('\r'),
                    (_, 't') => DecodedEscape::Char('\t'),
                    (_, '\\') => DecodedEscape::Char('\\'),
                    (_, '"') => DecodedEscape::Char('"'),
                    (EscapeDialect::Json, '/') => DecodedEscape::Char('/'),
                    (EscapeDialect::Json | EscapeDialect::Python, 'b') => {
                        DecodedEscape::Char('\u{0008}')
                    }
                    (EscapeDialect::Json | EscapeDialect::Python, 'f') => {
                        DecodedEscape::Char('\u{000C}')
                    }
                    (EscapeDialect::Js | EscapeDialect::Python, '\'') => DecodedEscape::Char('\''),
                    (EscapeDialect::Js, '`') => DecodedEscape::Char('`'),
                    (EscapeDialect::Js, '$') => DecodedEscape::Char('$'),
                    (EscapeDialect::Python, 'a') => DecodedEscape::Char('\u{0007}'),
                    (EscapeDialect::Python, 'v') => DecodedEscape::Char('\u{000B}'),
                    (EscapeDialect::Python, '0') => DecodedEscape::Char('\0'),
                    (EscapeDialect::Js | EscapeDialect::Python, '\n') => DecodedEscape::None,
                    (EscapeDialect::Js | EscapeDialect::Python, 'x') => {
                        self.state = EscapeState::Hex {
                            remaining: 2,
                            acc: 0,
                            is_u16: false,
                        };
                        DecodedEscape::None
                    }
                    (_, 'u') => {
                        self.state = EscapeState::Hex {
                            remaining: 4,
                            acc: 0,
                            is_u16: dialect != EscapeDialect::Python,
                        };
                        DecodedEscape::None
                    }
                    (EscapeDialect::Python, 'U') => {
                        self.state = EscapeState::Hex {
                            remaining: 8,
                            acc: 0,
                            is_u16: false,
                        };
                        DecodedEscape::None
                    }
                    (EscapeDialect::Python, other) => DecodedEscape::TwoChars('\\', other),
                    (_, other) => DecodedEscape::Char(other),
                }
            }
            EscapeState::Hex {
                remaining,
                acc,
                is_u16,
            } => {
                let digit = match (dialect, ch.to_digit(16)) {
                    (_, Some(d)) => d,
                    (EscapeDialect::Json, None) => 0,
                    (_, None) => {
                        self.state = EscapeState::None;
                        return DecodedEscape::Char(ch);
                    }
                };
                let next = (acc << 4) | digit;
                if remaining > 1 {
                    self.state = EscapeState::Hex {
                        remaining: remaining - 1,
                        acc: next,
                        is_u16,
                    };
                    DecodedEscape::None
                } else {
                    self.state = EscapeState::None;
                    if is_u16 && (0xD800..=0xDBFF).contains(&next) {
                        self.high_surrogate = Some(next as u16);
                        DecodedEscape::None
                    } else if is_u16 && (0xDC00..=0xDFFF).contains(&next) {
                        if let Some(hi) = self.high_surrogate.take() {
                            let cp = 0x10000 + (((hi as u32 - 0xD800) << 10) | (next - 0xDC00));
                            char::from_u32(cp).map_or(DecodedEscape::None, DecodedEscape::Char)
                        } else {
                            DecodedEscape::Char('\u{FFFD}')
                        }
                    } else {
                        self.high_surrogate = None;
                        char::from_u32(next).map_or(DecodedEscape::None, DecodedEscape::Char)
                    }
                }
            }
        }
    }
}

fn push_js_string_char(
    target: &mut JsStringTarget,
    cmd_buf: &mut String,
    patch_buf: &mut String,
    ch: char,
) {
    match target {
        JsStringTarget::Patch => patch_buf.push(ch),
        JsStringTarget::ExecField {
            field, workdir_buf, ..
        } => match field {
            TargetField::Cmd => cmd_buf.push(ch),
            TargetField::Workdir | TargetField::Environment => workdir_buf.push(ch),
            TargetField::Other => {}
        },
    }
}

fn abort_js_string_target(
    target: &mut JsStringTarget,
    cmd_buf: &mut String,
    patch_buf: &mut String,
    events: &mut Vec<JsEditEvent>,
) {
    match target {
        JsStringTarget::Patch => {
            patch_buf.clear();
            events.push(JsEditEvent::ApplyPatchAbort);
        }
        JsStringTarget::ExecField {
            field, workdir_buf, ..
        } => match field {
            TargetField::Cmd => {
                cmd_buf.clear();
                events.push(JsEditEvent::ExecCallAbort);
            }
            TargetField::Workdir | TargetField::Environment => {
                workdir_buf.clear();
                events.push(JsEditEvent::ExecCallAbort);
            }
            TargetField::Other => {}
        },
    }
}

impl JsNestedEditScanner {
    fn push(&mut self, delta: &str) -> Vec<JsEditEvent> {
        let mut events = Vec::new();
        let mut cmd_buf = String::new();
        let mut patch_buf = String::new();

        for ch in delta.chars() {
            let in_string = matches!(
                self.state,
                JsScanState::SkipString { .. } | JsScanState::InString { .. }
            );
            if !in_string {
                match self.comment {
                    JsCommentState::Line => {
                        if ch == '\n' {
                            self.comment = JsCommentState::None;
                        }
                        continue;
                    }
                    JsCommentState::Block => {
                        if ch == '*' {
                            self.comment = JsCommentState::BlockStar;
                        }
                        continue;
                    }
                    JsCommentState::BlockStar => {
                        if ch == '/' {
                            self.comment = JsCommentState::None;
                        } else if ch != '*' {
                            self.comment = JsCommentState::Block;
                        }
                        continue;
                    }
                    JsCommentState::Regex { escaped, in_class } => {
                        if escaped {
                            self.comment = JsCommentState::Regex {
                                escaped: false,
                                in_class,
                            };
                        } else if ch == '\\' {
                            self.comment = JsCommentState::Regex {
                                escaped: true,
                                in_class,
                            };
                        } else if in_class {
                            if ch == ']' {
                                self.comment = JsCommentState::Regex {
                                    escaped: false,
                                    in_class: false,
                                };
                            }
                        } else if ch == '[' {
                            self.comment = JsCommentState::Regex {
                                escaped: false,
                                in_class: true,
                            };
                        } else if ch == '/' || ch == '\n' {
                            self.comment = JsCommentState::None;
                            self.regex_allowed = ch == '\n';
                        }
                        continue;
                    }
                    JsCommentState::SlashPending => {
                        if ch == '/' {
                            self.comment = JsCommentState::Line;
                            continue;
                        } else if ch == '*' {
                            self.comment = JsCommentState::Block;
                            continue;
                        } else if self.regex_allowed && ch != '\n' {
                            self.comment = JsCommentState::Regex {
                                escaped: ch == '\\',
                                in_class: ch == '[',
                            };
                            continue;
                        } else {
                            self.comment = JsCommentState::None;
                            if matches!(self.state, JsScanState::Normal) {
                                self.after_dot = false;
                                self.prev_ident.clear();
                            }
                            self.regex_allowed = true;
                        }
                    }
                    JsCommentState::None => {
                        if ch == '/' {
                            if matches!(self.state, JsScanState::Normal) && !self.ident.is_empty() {
                                self.regex_allowed = js_keyword_allows_regex(&self.ident);
                                self.prev_ident = std::mem::take(&mut self.ident);
                            }
                            self.comment = JsCommentState::SlashPending;
                            continue;
                        }
                    }
                }
            }

            match &mut self.state {
                JsScanState::SkipString { quote } => {
                    let q = *quote;
                    if matches!(
                        self.escape.feed(EscapeDialect::Js, ch, q),
                        DecodedEscape::CloseQuote
                    ) {
                        self.state = JsScanState::Normal;
                        self.regex_allowed = false;
                    }
                    continue;
                }
                JsScanState::InString {
                    quote,
                    target,
                    pending_dollar,
                    interpolated,
                    interp_brace_depth,
                } => {
                    let q = *quote;
                    if *interpolated && *interp_brace_depth > 0 {
                        match ch {
                            '{' => *interp_brace_depth += 1,
                            '}' => *interp_brace_depth -= 1,
                            _ => {}
                        }
                        continue;
                    }
                    let was_escaped = !self.escape.is_idle();
                    match self.escape.feed(EscapeDialect::Js, ch, q) {
                        DecodedEscape::Char(decoded) => {
                            if *interpolated {
                                if q == '`' && !was_escaped {
                                    if *pending_dollar && ch == '{' {
                                        *pending_dollar = false;
                                        *interp_brace_depth = 1;
                                    } else {
                                        *pending_dollar = ch == '$';
                                    }
                                } else {
                                    *pending_dollar = false;
                                }
                                continue;
                            }
                            if q == '`' && !was_escaped {
                                if *pending_dollar {
                                    if ch == '{' {
                                        *pending_dollar = false;
                                        *interpolated = true;
                                        *interp_brace_depth = 1;
                                        abort_js_string_target(
                                            target,
                                            &mut cmd_buf,
                                            &mut patch_buf,
                                            &mut events,
                                        );
                                        continue;
                                    }
                                    push_js_string_char(target, &mut cmd_buf, &mut patch_buf, '$');
                                    *pending_dollar = false;
                                }
                                if ch == '$' {
                                    *pending_dollar = true;
                                    continue;
                                }
                            } else if *pending_dollar {
                                push_js_string_char(target, &mut cmd_buf, &mut patch_buf, '$');
                                *pending_dollar = false;
                            }
                            push_js_string_char(target, &mut cmd_buf, &mut patch_buf, decoded);
                        }
                        DecodedEscape::CloseQuote => {
                            let was_interpolated = *interpolated;
                            let had_dollar = *pending_dollar;
                            match target {
                                JsStringTarget::Patch => {
                                    if !was_interpolated {
                                        if had_dollar {
                                            patch_buf.push('$');
                                        }
                                        if !patch_buf.is_empty() {
                                            events.push(JsEditEvent::ApplyPatchChunk(
                                                std::mem::take(&mut patch_buf),
                                            ));
                                        }
                                    }
                                    events.push(JsEditEvent::ApplyPatchEnd);
                                    self.state = JsScanState::Normal;
                                }
                                JsStringTarget::ExecField {
                                    brace_depth,
                                    field,
                                    workdir_buf,
                                } => {
                                    let depth = *brace_depth;
                                    if !was_interpolated {
                                        if had_dollar {
                                            match field {
                                                TargetField::Cmd => cmd_buf.push('$'),
                                                TargetField::Workdir | TargetField::Environment => {
                                                    workdir_buf.push('$')
                                                }
                                                TargetField::Other => {}
                                            }
                                        }
                                        if *field == TargetField::Cmd {
                                            if !cmd_buf.is_empty() {
                                                events.push(JsEditEvent::ExecCmdChunk(
                                                    std::mem::take(&mut cmd_buf),
                                                ));
                                            }
                                            events.push(JsEditEvent::ExecCmdEnd);
                                        } else if *field == TargetField::Environment {
                                            events.push(JsEditEvent::ExecEnvironment(
                                                std::mem::take(workdir_buf),
                                            ));
                                        } else if *field == TargetField::Workdir
                                            && !workdir_buf.is_empty()
                                        {
                                            let dir = std::mem::take(workdir_buf);
                                            events.push(JsEditEvent::ExecWorkdir(dir));
                                        }
                                    }
                                    self.state = JsScanState::InExecArgs {
                                        brace_depth: depth,
                                        expect_value_for: None,
                                        pending_key: String::new(),
                                    };
                                }
                            }
                        }
                        DecodedEscape::None | DecodedEscape::TwoChars(_, _) => {}
                    }
                    continue;
                }
                JsScanState::AwaitPatchArg => {
                    if ch.is_whitespace() {
                        continue;
                    }
                    if matches!(ch, '"' | '\'' | '`') {
                        events.push(JsEditEvent::ApplyPatchStart);
                        self.state = JsScanState::InString {
                            quote: ch,
                            target: JsStringTarget::Patch,
                            pending_dollar: false,
                            interpolated: false,
                            interp_brace_depth: 0,
                        };
                    } else {
                        self.state = JsScanState::Normal;
                    }
                    continue;
                }
                JsScanState::InExecArgs {
                    brace_depth,
                    expect_value_for,
                    pending_key,
                } => {
                    if ch.is_ascii_alphanumeric() || ch == '_' || ch == '$' {
                        if *brace_depth == 1
                            && matches!(
                                expect_value_for,
                                Some(TargetField::Workdir | TargetField::Environment)
                            )
                        {
                            events.push(JsEditEvent::ExecUnresolved);
                        }
                        if *brace_depth == 1 && expect_value_for.is_none() {
                            pending_key.push(ch);
                        }
                        continue;
                    }
                    match ch {
                        '{' => {
                            *brace_depth += 1;
                            pending_key.clear();
                        }
                        '}' => {
                            *brace_depth = brace_depth.saturating_sub(1);
                            pending_key.clear();
                            *expect_value_for = None;
                            if *brace_depth == 0 {
                                events.push(JsEditEvent::ExecCallEnd);
                                self.state = JsScanState::Normal;
                            }
                        }
                        ':' if *brace_depth == 1 && expect_value_for.is_none() => {
                            let key = std::mem::take(pending_key);
                            *expect_value_for = Some(match key.as_str() {
                                "cmd" => TargetField::Cmd,
                                "workdir" => TargetField::Workdir,
                                "environment_id" => TargetField::Environment,
                                _ => TargetField::Other,
                            });
                        }
                        ',' if *brace_depth == 1 => {
                            pending_key.clear();
                            *expect_value_for = None;
                        }
                        '"' | '\'' | '`' => {
                            let depth = *brace_depth;
                            if depth == 1
                                && let Some(field) = expect_value_for.take()
                            {
                                if field == TargetField::Cmd {
                                    events.push(JsEditEvent::ExecCmdStart);
                                }
                                self.state = JsScanState::InString {
                                    quote: ch,
                                    target: JsStringTarget::ExecField {
                                        brace_depth: depth,
                                        field,
                                        workdir_buf: String::new(),
                                    },
                                    pending_dollar: false,
                                    interpolated: false,
                                    interp_brace_depth: 0,
                                };
                            } else if depth == 1 && expect_value_for.is_none() {
                                // Quoted key like "cmd": ... — allow chars into pending_key
                            } else {
                                self.state = JsScanState::InString {
                                    quote: ch,
                                    target: JsStringTarget::ExecField {
                                        brace_depth: depth,
                                        field: TargetField::Other,
                                        workdir_buf: String::new(),
                                    },
                                    pending_dollar: false,
                                    interpolated: false,
                                    interp_brace_depth: 0,
                                };
                            }
                        }
                        _ if ch.is_whitespace() => {}
                        _ => {
                            if *brace_depth == 1
                                && (ch == '.'
                                    || matches!(
                                        expect_value_for,
                                        Some(TargetField::Workdir | TargetField::Environment)
                                    ))
                            {
                                events.push(JsEditEvent::ExecUnresolved);
                            }
                            if *brace_depth == 1 && expect_value_for.is_some() {
                                *expect_value_for = None;
                            }
                        }
                    }
                    continue;
                }
                JsScanState::Normal => {}
            }

            if ch.is_ascii_alphanumeric() || ch == '_' || ch == '$' {
                self.ident.push(ch);
                continue;
            }
            if !self.ident.is_empty() {
                let name = std::mem::take(&mut self.ident);
                self.regex_allowed = js_keyword_allows_regex(&name);
                if self.after_dot {
                    self.after_dot = false;
                    if ch == '(' {
                        if self.prev_ident == "tools" && name == "exec_command" {
                            self.prev_ident.clear();
                            self.regex_allowed = true;
                            events.push(JsEditEvent::ExecCallStart);
                            self.state = JsScanState::InExecArgs {
                                brace_depth: 0,
                                expect_value_for: None,
                                pending_key: String::new(),
                            };
                            continue;
                        }
                        if self.prev_ident == "tools" && name == "apply_patch" {
                            self.prev_ident.clear();
                            self.regex_allowed = true;
                            self.state = JsScanState::AwaitPatchArg;
                            continue;
                        }
                    }
                    self.prev_ident = name;
                } else {
                    self.prev_ident = name;
                }
            }
            match ch {
                '.' => {
                    self.after_dot = true;
                    self.regex_allowed = false;
                }
                '"' | '\'' | '`' => {
                    self.after_dot = false;
                    self.prev_ident.clear();
                    self.state = JsScanState::SkipString { quote: ch };
                }
                ')' | ']' => {
                    self.after_dot = false;
                    self.prev_ident.clear();
                    self.regex_allowed = false;
                }
                _ if ch.is_whitespace() => {}
                _ => {
                    self.after_dot = false;
                    self.prev_ident.clear();
                    self.regex_allowed = true;
                }
            }
        }

        if !cmd_buf.is_empty() {
            events.push(JsEditEvent::ExecCmdChunk(cmd_buf));
        }
        if !patch_buf.is_empty() {
            events.push(JsEditEvent::ApplyPatchChunk(patch_buf));
        }
        events
    }
}

fn js_keyword_allows_regex(ident: &str) -> bool {
    matches!(
        ident,
        "return"
            | "throw"
            | "case"
            | "in"
            | "of"
            | "typeof"
            | "void"
            | "delete"
            | "await"
            | "yield"
            | "new"
            | "else"
            | "do"
    )
}

// ---------------------------------------------------------------------------
// Incremental JSON string field extractor for {"cmd": "...", "workdir": "..."}
// ---------------------------------------------------------------------------

#[derive(Default)]
struct JsonCmdExtractor {
    environment_id: Option<String>,
    depth: usize,
    state: JsonState,
    key_buf: String,
    workdir_buf: String,
    escape: EscapeDecoder,
}

#[derive(Default, Clone, Copy, PartialEq, Eq)]
enum JsonState {
    #[default]
    Start,
    ExpectKey,
    InKey,
    AfterKey,
    ExpectValue(TargetField),
    InTargetString(TargetField),
    SkipValue,
    InSkipString,
    Complete,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum TargetField {
    Cmd,
    Workdir,
    Environment,
    Other,
}

impl JsonCmdExtractor {
    fn push(&mut self, delta: &str) -> (String, Option<String>) {
        let mut cmd_out = String::new();
        let mut completed_workdir = None;
        for ch in delta.chars() {
            match self.state {
                JsonState::Start => {
                    if ch == '{' {
                        self.depth = 1;
                        self.state = JsonState::ExpectKey;
                    }
                }
                JsonState::ExpectKey => match ch {
                    '"' => {
                        self.key_buf.clear();
                        self.state = JsonState::InKey;
                    }
                    '}' => {
                        self.depth = self.depth.saturating_sub(1);
                        self.state = JsonState::Complete;
                    }
                    _ => {}
                },
                JsonState::InKey => match self.escape.feed(EscapeDialect::Json, ch, '"') {
                    DecodedEscape::Char(decoded) => self.key_buf.push(decoded),
                    DecodedEscape::CloseQuote => self.state = JsonState::AfterKey,
                    DecodedEscape::None | DecodedEscape::TwoChars(_, _) => {}
                },
                JsonState::AfterKey => {
                    if ch == ':' {
                        let field = match self.key_buf.as_str() {
                            "cmd" => TargetField::Cmd,
                            "workdir" => TargetField::Workdir,
                            "environment_id" => TargetField::Environment,
                            _ => TargetField::Other,
                        };
                        self.state = JsonState::ExpectValue(field);
                    }
                }
                JsonState::ExpectValue(field) => {
                    if ch.is_whitespace() {
                        continue;
                    }
                    match (field, ch) {
                        (
                            TargetField::Cmd | TargetField::Workdir | TargetField::Environment,
                            '"',
                        ) => {
                            if matches!(field, TargetField::Workdir | TargetField::Environment) {
                                self.workdir_buf.clear();
                            }
                            self.state = JsonState::InTargetString(field);
                        }
                        (_, '"') => {
                            self.state = JsonState::InSkipString;
                        }
                        (_, '{' | '[') => {
                            self.depth += 1;
                            self.state = JsonState::SkipValue;
                        }
                        _ => {
                            self.state = JsonState::SkipValue;
                        }
                    }
                }
                JsonState::InTargetString(field) => {
                    match self.escape.feed(EscapeDialect::Json, ch, '"') {
                        DecodedEscape::Char(decoded) => match field {
                            TargetField::Cmd => cmd_out.push(decoded),
                            TargetField::Workdir | TargetField::Environment => {
                                self.workdir_buf.push(decoded)
                            }
                            TargetField::Other => {}
                        },
                        DecodedEscape::CloseQuote => {
                            if field == TargetField::Environment {
                                self.environment_id = Some(std::mem::take(&mut self.workdir_buf));
                            } else if field == TargetField::Workdir && !self.workdir_buf.is_empty()
                            {
                                completed_workdir = Some(std::mem::take(&mut self.workdir_buf));
                            }
                            self.state = JsonState::SkipValue;
                        }
                        DecodedEscape::None | DecodedEscape::TwoChars(_, _) => {}
                    }
                }
                JsonState::InSkipString => {
                    if matches!(
                        self.escape.feed(EscapeDialect::Json, ch, '"'),
                        DecodedEscape::CloseQuote
                    ) {
                        self.state = JsonState::SkipValue;
                    }
                }
                JsonState::SkipValue => match ch {
                    '"' => self.state = JsonState::InSkipString,
                    '{' | '[' => self.depth += 1,
                    '}' | ']' => {
                        self.depth = self.depth.saturating_sub(1);
                        if self.depth == 0 {
                            self.state = JsonState::Complete;
                        }
                    }
                    ',' if self.depth == 1 => {
                        self.state = JsonState::ExpectKey;
                    }
                    _ => {}
                },
                JsonState::Complete => {}
            }
        }
        (cmd_out, completed_workdir)
    }
}

// ---------------------------------------------------------------------------
// Shell edit stream projector (cat/tee heredocs + inline Python text edits)
// ---------------------------------------------------------------------------

pub(crate) struct ShellEditProjector {
    workdir: PathBuf,
    location_resolved: bool,
    ambiguous_location: bool,
    environment_id: Option<String>,
    base_cwd: PathBuf,
    script_cwd: PathBuf,
    mode: ShellMode,
    files: HashMap<PathBuf, StreamedFile>,
    dirty: bool,
}

enum StreamedFile {
    Add {
        committed: String,
        partial: String,
        allow_empty: bool,
        append_only: bool,
    },
    Update {
        committed: Vec<(String, String)>,
        bytes: usize,
        allow_context: bool,
        partial: Option<(String, String)>,
    },
}

impl StreamedFile {
    fn edit_kind(&self, extra_tail: Option<&str>) -> Option<EditKind> {
        match self {
            Self::Add {
                committed,
                partial,
                allow_empty,
                append_only,
            } => {
                let tail = extra_tail.unwrap_or("");
                if !*allow_empty && committed.is_empty() && partial.is_empty() && tail.is_empty() {
                    return None;
                }
                let mut content =
                    String::with_capacity(committed.len() + partial.len() + tail.len());
                content.push_str(committed);
                content.push_str(partial);
                content.push_str(tail);
                Some(EditKind::Write {
                    content,
                    append: *append_only,
                })
            }
            Self::Update {
                committed, partial, ..
            } => Some(EditKind::Replace(
                committed.iter().chain(partial.iter()).cloned().collect(),
            )),
        }
    }
}

enum ShellMode {
    Scan {
        line: String,
    },
    Heredoc {
        path: PathBuf,
        delimiter: String,
        strip_tabs: bool,
        overwrite: bool,
        wrote_any: bool,
        line_buf: String,
    },
    PythonHeredoc {
        delimiter: String,
        strip_tabs: bool,
        line_buf: String,
        py: PythonEditProjector,
    },
    PythonInline {
        quote: char,
        escaped: bool,
        py: PythonEditProjector,
    },
}

impl ShellEditProjector {
    pub(crate) fn new(base_cwd: &Path) -> Self {
        Self {
            base_cwd: base_cwd.to_path_buf(),
            workdir: PathBuf::new(),
            location_resolved: false,
            ambiguous_location: false,
            environment_id: None,
            script_cwd: PathBuf::new(),
            mode: ShellMode::Scan {
                line: String::new(),
            },
            files: HashMap::new(),
            dirty: false,
        }
    }

    pub(crate) fn reset_script_state(&mut self) {
        self.script_cwd.clear();
        self.mode = ShellMode::Scan {
            line: String::new(),
        };
    }

    pub(crate) fn set_workdir(&mut self, dir: &Path) {
        self.workdir = dir.to_path_buf();
        self.mark_location_dirty();
    }

    fn resolve_location(&mut self) {
        if !self.location_resolved {
            self.location_resolved = true;
            self.mark_location_dirty();
        }
    }

    fn mark_location_dirty(&mut self) {
        self.dirty |= !self.files.is_empty() || matches!(self.mode, ShellMode::Heredoc { .. });
    }

    pub(crate) fn push(&mut self, chunk: &str) {
        for ch in chunk.chars() {
            self.push_char(ch);
        }
    }

    pub(crate) fn finish(&mut self) {
        match &mut self.mode {
            ShellMode::Scan { line } => {
                let pending = std::mem::take(line);
                if !pending.trim().is_empty() {
                    self.handle_scan_line(&pending);
                }
            }
            ShellMode::Heredoc {
                path,
                delimiter,
                strip_tabs,
                overwrite,
                wrote_any,
                line_buf,
            } => {
                let tail = std::mem::take(line_buf);
                let trimmed = trim_heredoc_line(&tail, *strip_tabs);
                if trimmed == delimiter.as_str() {
                    if *overwrite && !*wrote_any {
                        let path = path.clone();
                        self.record_empty_overwrite(path);
                    }
                } else if !tail.is_empty() {
                    let path = path.clone();
                    let content = trimmed.to_owned();
                    let is_overwrite = *overwrite;
                    *wrote_any = true;
                    self.commit_add_line(&path, &content, false, !is_overwrite);
                }
            }
            ShellMode::PythonHeredoc {
                delimiter,
                strip_tabs,
                line_buf,
                py,
            } => {
                let tail = std::mem::take(line_buf);
                let trimmed = trim_heredoc_line(&tail, *strip_tabs);
                if !tail.is_empty() && trimmed != delimiter.as_str() {
                    py.push(trimmed, &mut self.files, &mut self.dirty);
                }
                py.finish(&mut self.files, &mut self.dirty);
            }
            ShellMode::PythonInline { py, .. } => {
                py.finish(&mut self.files, &mut self.dirty);
            }
        }
    }

    pub(crate) fn take_dirty(&mut self) -> bool {
        std::mem::replace(&mut self.dirty, false)
    }

    #[cfg(test)]
    fn snapshot(&self) -> HashMap<PathBuf, codex_protocol::protocol::FileChange> {
        self.snapshot_entries()
            .into_iter()
            .map(|(p, e)| (p, e.kind.render(None)))
            .collect()
    }

    fn snapshot_entries(&self) -> HashMap<PathBuf, ProjectedEdit> {
        let active_heredoc: Option<(&Path, &str, bool)> = match &self.mode {
            ShellMode::Heredoc {
                path,
                delimiter,
                strip_tabs,
                overwrite,
                line_buf,
                ..
            } => {
                let candidate = trim_heredoc_line(line_buf, *strip_tabs);
                if !candidate.is_empty() && !delimiter.starts_with(candidate) {
                    Some((path.as_path(), candidate, !*overwrite))
                } else {
                    None
                }
            }
            _ => None,
        };

        let mut out = HashMap::new();
        for (raw_path, file) in &self.files {
            let tail = active_heredoc.and_then(|(p, t, _)| (p == raw_path.as_path()).then_some(t));
            if let Some(kind) = file.edit_kind(tail) {
                let mut edit = self.project_edit(raw_path, kind);
                if matches!(
                    file,
                    StreamedFile::Update {
                        allow_context: false,
                        ..
                    }
                ) {
                    edit.source = None;
                }
                out.insert(self.resolve_abs(raw_path), edit);
            }
        }
        if let Some((raw_path, tail, append_only)) = active_heredoc
            && !self.files.contains_key(raw_path)
            && self.files.len() < MAX_STREAM_FILES
        {
            out.insert(
                self.resolve_abs(raw_path),
                self.project_edit(
                    raw_path,
                    EditKind::Write {
                        content: tail.to_owned(),
                        append: append_only,
                    },
                ),
            );
        }
        out
    }

    fn resolve_abs(&self, raw_path: &Path) -> PathBuf {
        let path = self.workdir.join(raw_path);
        if self.environment_id.is_some() {
            path
        } else {
            join_path(&self.base_cwd, &path)
        }
    }

    fn project_edit(&self, path: &Path, kind: EditKind) -> ProjectedEdit {
        // A later environment argument can change the baseline even for absolute paths.
        let source = (!self.ambiguous_location && self.location_resolved)
            .then(|| (self.environment_id.clone(), self.workdir.join(path)));
        ProjectedEdit { source, kind }
    }

    fn resolve_script_target(&self, target: &str) -> PathBuf {
        join_path(&self.script_cwd, Path::new(target))
    }

    fn record_empty_overwrite(&mut self, path: PathBuf) {
        self.files.insert(
            path,
            StreamedFile::Add {
                committed: String::new(),
                partial: String::new(),
                allow_empty: true,
                append_only: false,
            },
        );
        self.dirty = true;
    }

    fn push_char(&mut self, ch: char) {
        enum Transition {
            None,
            ToScan,
            ToPythonInline(char, Vec<String>),
        }
        let mut transition = Transition::None;
        match &mut self.mode {
            ShellMode::Scan { line } => {
                if ch == '\n' {
                    let completed = std::mem::take(line);
                    self.handle_scan_line(&completed);
                    return;
                }
                line.push(ch);
                if (ch == '"' || ch == '\'')
                    && let Some(quote) = detect_python_inline_start(line)
                {
                    let prefix = &line[..line.len() - ch.len_utf8()];
                    let mut cds = Vec::new();
                    for seg in split_shell_chain(prefix) {
                        let toks = tokenize_shell_segment(seg);
                        if toks.len() == 2
                            && toks[0] == "cd"
                            && !toks[1].starts_with('-')
                            && is_literal_path(&toks[1])
                        {
                            cds.push(strip_shell_quotes(&toks[1]));
                        }
                    }
                    line.clear();
                    transition = Transition::ToPythonInline(quote, cds);
                }
            }
            ShellMode::Heredoc {
                path,
                delimiter,
                strip_tabs,
                overwrite,
                wrote_any,
                line_buf,
            } => {
                if ch == '\n' {
                    let raw_line = std::mem::take(line_buf);
                    let line = trim_heredoc_line(&raw_line, *strip_tabs);
                    let line = line.strip_suffix('\r').unwrap_or(line);
                    if line == delimiter.as_str() {
                        if *overwrite && !*wrote_any {
                            let path = path.clone();
                            self.record_empty_overwrite(path);
                        }
                        transition = Transition::ToScan;
                    } else {
                        let path = path.clone();
                        let owned = line.to_owned();
                        let is_overwrite = *overwrite;
                        *wrote_any = true;
                        self.commit_add_line(&path, &owned, true, !is_overwrite);
                    }
                } else if line_buf.len() < MAX_STREAM_FILE_BYTES {
                    line_buf.push(ch);
                    let candidate = trim_heredoc_line(line_buf, *strip_tabs);
                    if !delimiter.starts_with(candidate) {
                        self.dirty = true;
                    }
                }
            }
            ShellMode::PythonHeredoc {
                delimiter,
                strip_tabs,
                line_buf,
                py,
            } => {
                if ch == '\n' {
                    let raw_line = std::mem::take(line_buf);
                    let line = trim_heredoc_line(&raw_line, *strip_tabs);
                    let line = line.strip_suffix('\r').unwrap_or(line);
                    if line == delimiter.as_str() {
                        py.finish(&mut self.files, &mut self.dirty);
                        transition = Transition::ToScan;
                    } else {
                        py.push(line, &mut self.files, &mut self.dirty);
                        py.push("\n", &mut self.files, &mut self.dirty);
                    }
                } else if line_buf.len() < MAX_STREAM_FILE_BYTES {
                    line_buf.push(ch);
                }
            }
            ShellMode::PythonInline { quote, escaped, py } => {
                if *quote == '"' {
                    if *escaped {
                        *escaped = false;
                        let decoded = match ch {
                            '"' => '"',
                            '\\' => '\\',
                            '$' => '$',
                            '`' => '`',
                            other => {
                                py.push_char('\\', &mut self.files, &mut self.dirty);
                                other
                            }
                        };
                        py.push_char(decoded, &mut self.files, &mut self.dirty);
                    } else if ch == '\\' {
                        *escaped = true;
                    } else if ch == '"' {
                        py.finish(&mut self.files, &mut self.dirty);
                        transition = Transition::ToScan;
                    } else {
                        py.push_char(ch, &mut self.files, &mut self.dirty);
                    }
                } else if ch == '\'' {
                    py.finish(&mut self.files, &mut self.dirty);
                    transition = Transition::ToScan;
                } else {
                    py.push_char(ch, &mut self.files, &mut self.dirty);
                }
            }
        }
        match transition {
            Transition::None => {}
            Transition::ToScan => {
                self.mode = ShellMode::Scan {
                    line: String::new(),
                };
            }
            Transition::ToPythonInline(quote, cds) => {
                for dir in cds {
                    self.apply_cd(&dir);
                }
                self.mode = ShellMode::PythonInline {
                    quote,
                    escaped: false,
                    py: PythonEditProjector::new(self.script_cwd.clone()),
                };
            }
        }
    }

    fn apply_cd(&mut self, dir: &str) {
        self.script_cwd = join_path(&self.script_cwd, Path::new(dir));
    }

    fn handle_scan_line(&mut self, line: &str) {
        for segment in split_shell_chain(line) {
            let tokens = tokenize_shell_segment(segment);
            if tokens.is_empty() {
                continue;
            }
            if tokens[0] == "cd" && tokens.len() == 2 && !tokens[1].starts_with('-') {
                if is_literal_path(&tokens[1]) {
                    self.apply_cd(&strip_shell_quotes(&tokens[1]));
                }
                continue;
            }
            if let Some(heredoc) = parse_heredoc_header(&tokens) {
                match heredoc {
                    HeredocHeader::FileWrite {
                        target,
                        delimiter,
                        strip_tabs,
                        overwrite,
                    } => {
                        let path = self.resolve_script_target(&target);
                        if overwrite && self.files.remove(&path).is_some() {
                            self.dirty = true;
                        }
                        self.mode = ShellMode::Heredoc {
                            path,
                            delimiter,
                            strip_tabs,
                            overwrite,
                            wrote_any: false,
                            line_buf: String::new(),
                        };
                        return;
                    }
                    HeredocHeader::Python {
                        delimiter,
                        strip_tabs,
                    } => {
                        self.mode = ShellMode::PythonHeredoc {
                            delimiter,
                            strip_tabs,
                            line_buf: String::new(),
                            py: PythonEditProjector::new(self.script_cwd.clone()),
                        };
                        return;
                    }
                }
            }
        }
    }

    fn commit_add_line(
        &mut self,
        path: &Path,
        line: &str,
        trailing_newline: bool,
        append_only: bool,
    ) {
        if !self.files.contains_key(path) && self.files.len() >= MAX_STREAM_FILES {
            return;
        }
        let entry = self
            .files
            .entry(path.to_path_buf())
            .or_insert_with(|| StreamedFile::Add {
                committed: String::new(),
                partial: String::new(),
                allow_empty: false,
                append_only,
            });
        if let StreamedFile::Add {
            committed,
            partial,
            append_only: existing_append,
            ..
        } = entry
        {
            if !append_only {
                *existing_append = false;
            }
            partial.clear();
            if committed.len() + line.len() + usize::from(trailing_newline) <= MAX_STREAM_FILE_BYTES
            {
                committed.push_str(line);
                if trailing_newline {
                    committed.push('\n');
                }
                self.dirty = true;
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Shell header helpers (heredoc + python -c detection)
// ---------------------------------------------------------------------------

fn trim_heredoc_line(s: &str, strip_tabs: bool) -> &str {
    if strip_tabs {
        s.trim_start_matches('\t')
    } else {
        s
    }
}

enum HeredocHeader {
    FileWrite {
        target: String,
        delimiter: String,
        strip_tabs: bool,
        overwrite: bool,
    },
    Python {
        delimiter: String,
        strip_tabs: bool,
    },
}

fn detect_python_inline_start(line: &str) -> Option<char> {
    let quote = line.chars().last()?;
    if quote != '"' && quote != '\'' {
        return None;
    }
    let prefix = &line[..line.len() - quote.len_utf8()];
    let last_segment = split_shell_chain(prefix).last().copied().unwrap_or(prefix);
    let tokens = tokenize_shell_segment(last_segment);
    if tokens.len() == 2 && is_python_bin(&tokens[0]) && tokens[1] == "-c" {
        Some(quote)
    } else {
        None
    }
}

fn is_python_bin(token: &str) -> bool {
    let name = Path::new(token)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or(token);
    matches!(name, "python" | "python3")
}

fn parse_heredoc_header(tokens: &[String]) -> Option<HeredocHeader> {
    let cmd = Path::new(&tokens[0])
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or(&tokens[0]);
    let mut delimiter: Option<(String, bool)> = None;
    let mut output_target: Option<(String, bool)> = None;
    let mut positional: Vec<&str> = Vec::new();

    let mut i = 1;
    while i < tokens.len() {
        let tok = tokens[i].as_str();
        if tok == "<" || tok == "0<" || (tok.starts_with('<') && !tok.starts_with("<<")) {
            return None;
        }
        if let Some(rest) = tok.strip_prefix("<<") {
            let (strip_tabs, raw_delim) = match rest.strip_prefix('-') {
                Some(after) => (true, after),
                None => (false, rest),
            };
            let delim = if raw_delim.is_empty() {
                i += 1;
                tokens.get(i)?.as_str()
            } else {
                raw_delim
            };
            let clean = strip_shell_quotes(delim);
            if clean.is_empty() || delimiter.is_some() {
                return None;
            }
            delimiter = Some((clean, strip_tabs));
        } else if matches!(tok, ">" | ">|" | "1>" | ">>" | "1>>") {
            let overwrite = matches!(tok, ">" | ">|" | "1>");
            i += 1;
            let target = tokens.get(i)?;
            if !is_literal_path(target) || output_target.is_some() {
                return None;
            }
            output_target = Some((strip_shell_quotes(target), overwrite));
        } else {
            positional.push(tok);
        }
        i += 1;
    }

    let (delimiter, strip_tabs) = delimiter?;
    match cmd {
        "cat" => {
            if !positional.is_empty() {
                return None;
            }
            let (target, overwrite) = output_target?;
            Some(HeredocHeader::FileWrite {
                target,
                delimiter,
                strip_tabs,
                overwrite,
            })
        }
        "tee" => {
            if output_target.is_some() {
                return None;
            }
            let append = positional.iter().any(|a| *a == "-a" || *a == "--append");
            let files: Vec<&str> = positional
                .into_iter()
                .filter(|arg| *arg != "-a" && *arg != "--append")
                .collect();
            if files.len() != 1 || !is_literal_path(files[0]) {
                return None;
            }
            Some(HeredocHeader::FileWrite {
                target: strip_shell_quotes(files[0]),
                delimiter,
                strip_tabs,
                overwrite: !append,
            })
        }
        _ if is_python_bin(cmd) => {
            if output_target.is_some() {
                return None;
            }
            if positional.is_empty() || positional == ["-"] {
                Some(HeredocHeader::Python {
                    delimiter,
                    strip_tabs,
                })
            } else {
                None
            }
        }
        _ => None,
    }
}

fn split_shell_chain(line: &str) -> Vec<&str> {
    let mut segments = Vec::new();
    let mut start = 0;
    let mut in_single = false;
    let mut in_double = false;
    let mut escaped = false;
    let bytes = line.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        if escaped {
            escaped = false;
            i += 1;
            continue;
        }
        if in_single {
            if b == b'\'' {
                in_single = false;
            }
            i += 1;
            continue;
        }
        if in_double {
            if b == b'\\' {
                escaped = true;
            } else if b == b'"' {
                in_double = false;
            }
            i += 1;
            continue;
        }
        match b {
            b'\\' => escaped = true,
            b'\'' => in_single = true,
            b'"' => in_double = true,
            b';' => {
                segments.push(&line[start..i]);
                start = i + 1;
            }
            b'&' if i + 1 < bytes.len() && bytes[i + 1] == b'&' => {
                segments.push(&line[start..i]);
                i += 1;
                start = i + 1;
            }
            _ => {}
        }
        i += 1;
    }
    segments.push(&line[start..]);
    segments
}

fn tokenize_shell_segment(segment: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut current = String::new();
    let mut in_single = false;
    let mut in_double = false;
    let mut escaped = false;
    let chars: Vec<char> = segment.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        let ch = chars[i];
        if escaped {
            escaped = false;
            current.push(ch);
            i += 1;
            continue;
        }
        if in_single {
            if ch == '\'' {
                in_single = false;
            }
            current.push(ch);
            i += 1;
            continue;
        }
        if in_double {
            if ch == '\\' {
                escaped = true;
            } else if ch == '"' {
                in_double = false;
            }
            current.push(ch);
            i += 1;
            continue;
        }
        if ch.is_whitespace() {
            if !current.is_empty() {
                tokens.push(std::mem::take(&mut current));
            }
            i += 1;
            continue;
        }
        if ch == '<' || ch == '>' {
            let mut op = String::new();
            if (current == "0" && ch == '<') || (current == "1" && ch == '>') {
                op.push_str(&std::mem::take(&mut current));
            } else if !current.is_empty() {
                tokens.push(std::mem::take(&mut current));
            }
            op.push(ch);
            i += 1;
            if i < chars.len() {
                match (ch, chars[i]) {
                    ('<', '<') => {
                        op.push('<');
                        i += 1;
                        if i < chars.len() && chars[i] == '-' {
                            op.push('-');
                            i += 1;
                        }
                    }
                    ('>', '>') | ('>', '|') => {
                        op.push(chars[i]);
                        i += 1;
                    }
                    _ => {}
                }
            }
            tokens.push(op);
            continue;
        }
        match ch {
            '\\' => escaped = true,
            '\'' => {
                in_single = true;
                current.push(ch);
            }
            '"' => {
                in_double = true;
                current.push(ch);
            }
            _ => current.push(ch),
        }
        i += 1;
    }
    if !current.is_empty() {
        tokens.push(current);
    }
    tokens
}

fn strip_shell_quotes(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut in_single = false;
    let mut in_double = false;
    for ch in s.chars() {
        match ch {
            '\'' if !in_double => in_single = !in_single,
            '"' if !in_single => in_double = !in_double,
            other => out.push(other),
        }
    }
    out
}

fn is_literal_path(raw: &str) -> bool {
    if raw.is_empty() {
        return false;
    }
    let clean = strip_shell_quotes(raw);
    if clean.is_empty() {
        return false;
    }
    if !raw.starts_with('\'') && clean.contains(['$', '`', '*', '?', '[']) {
        return false;
    }
    true
}

// ---------------------------------------------------------------------------
// Incremental Python edit projector (open/read_text -> replace -> write_text)
// ---------------------------------------------------------------------------

struct PythonEditProjector {
    script_cwd: PathBuf,
    path_vars: HashMap<String, PathBuf>,
    file_vars: HashMap<String, PathBuf>,
    text_vars: HashMap<String, PathBuf>,
    call_stack: Vec<PyCallContext>,
    replace_touched: HashSet<PathBuf>,
    written_paths: HashSet<PathBuf>,
    unmatched_destination: HashSet<PathBuf>,
    stmt: Vec<PyTok>,
    paren_depth: usize,
    lex: PyLexState,
    active_op: Option<PyStreamOp>,
}

#[derive(Clone)]
enum PyCallContext {
    Neutral,
    ReadOnlySink,
    WriteTarget(PathBuf),
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum PyTok {
    Ident(String),
    Str(String),
    Dot,
    LParen,
    RParen,
    Comma,
    Eq,
    Colon,
    Other,
}

enum PyLexState {
    Normal {
        ident: String,
    },
    Comment,
    String {
        quote: char,
        triple: bool,
        raw: bool,
        pending_quotes: u8,
        checking_open: bool,
        escape: EscapeDecoder,
        buf: String,
        streaming: bool,
    },
}

enum PyStreamOp {
    WriteLiteral {
        path: PathBuf,
    },
    ReplaceAwaitOld {
        path: PathBuf,
    },
    ReplaceOld {
        path: PathBuf,
        old: String,
    },
    ReplaceAwaitNew {
        path: PathBuf,
        old: String,
    },
    ReplaceNew {
        path: PathBuf,
        old: String,
        new: String,
    },
}

impl PythonEditProjector {
    fn new(script_cwd: PathBuf) -> Self {
        Self {
            script_cwd,
            path_vars: HashMap::new(),
            file_vars: HashMap::new(),
            text_vars: HashMap::new(),
            call_stack: Vec::new(),
            replace_touched: HashSet::new(),
            written_paths: HashSet::new(),
            unmatched_destination: HashSet::new(),
            stmt: Vec::new(),
            paren_depth: 0,
            lex: PyLexState::Normal {
                ident: String::new(),
            },
            active_op: None,
        }
    }

    fn push(&mut self, chunk: &str, files: &mut HashMap<PathBuf, StreamedFile>, dirty: &mut bool) {
        for ch in chunk.chars() {
            self.push_char(ch, files, dirty);
        }
    }

    fn finish(&mut self, files: &mut HashMap<PathBuf, StreamedFile>, dirty: &mut bool) {
        self.flush_ident();
        self.eval_statement();
        if let Some(op) = self.active_op.take() {
            match op {
                PyStreamOp::WriteLiteral { path } => {
                    commit_partial(files, &path, dirty);
                }
                PyStreamOp::ReplaceOld { path, old }
                | PyStreamOp::ReplaceAwaitNew { path, old } => {
                    write_replace_hunk(
                        files,
                        &self.unmatched_destination,
                        &path,
                        &old,
                        "",
                        true,
                        dirty,
                    );
                }
                PyStreamOp::ReplaceNew { path, old, new } => {
                    write_replace_hunk(
                        files,
                        &self.unmatched_destination,
                        &path,
                        &old,
                        &new,
                        true,
                        dirty,
                    );
                }
                PyStreamOp::ReplaceAwaitOld { .. } => {}
            }
        }
        if self.written_paths.is_empty() {
            for path in self.replace_touched.drain() {
                if files.remove(&path).is_some() {
                    *dirty = true;
                }
            }
        }
    }

    fn in_readonly_sink(&self) -> bool {
        self.call_stack
            .iter()
            .any(|ctx| matches!(ctx, PyCallContext::ReadOnlySink))
    }

    fn enclosing_write_target(&self) -> Option<PathBuf> {
        self.call_stack.iter().rev().find_map(|ctx| match ctx {
            PyCallContext::WriteTarget(path) => Some(path.clone()),
            _ => None,
        })
    }

    fn push_char(
        &mut self,
        ch: char,
        files: &mut HashMap<PathBuf, StreamedFile>,
        dirty: &mut bool,
    ) {
        let mut replay_char: Option<char> = None;
        let mut finished_string: Option<(String, bool)> = None;
        match &mut self.lex {
            PyLexState::Comment => {
                if ch == '\n' {
                    self.lex = PyLexState::Normal {
                        ident: String::new(),
                    };
                    if self.paren_depth == 0 {
                        self.eval_statement();
                    }
                }
                return;
            }
            PyLexState::String {
                quote,
                triple,
                raw,
                pending_quotes,
                checking_open,
                escape,
                buf,
                streaming,
            } => {
                let q = *quote;
                let is_streaming = *streaming;
                if *checking_open {
                    if ch == q {
                        *pending_quotes += 1;
                        if *pending_quotes == 3 {
                            *triple = true;
                            *checking_open = false;
                            *pending_quotes = 0;
                        }
                        return;
                    } else if *pending_quotes == 2 {
                        finished_string = Some((std::mem::take(buf), is_streaming));
                        replay_char = Some(ch);
                    } else {
                        *checking_open = false;
                        *pending_quotes = 0;
                    }
                }
                if replay_char.is_none() {
                    if !escape.is_idle() {
                        match escape.feed(EscapeDialect::Python, ch, q) {
                            DecodedEscape::Char(c) => {
                                let mut tmp = [0u8; 4];
                                push_py_str_slice(
                                    buf,
                                    c.encode_utf8(&mut tmp),
                                    is_streaming,
                                    &mut self.active_op,
                                    &self.unmatched_destination,
                                    files,
                                    dirty,
                                );
                            }
                            DecodedEscape::TwoChars(c1, c2) => {
                                let mut tmp = [0u8; 8];
                                let n1 = c1.encode_utf8(&mut tmp).len();
                                let n2 = c2.encode_utf8(&mut tmp[n1..]).len();
                                let slice = std::str::from_utf8(&tmp[..n1 + n2]).unwrap_or("");
                                push_py_str_slice(
                                    buf,
                                    slice,
                                    is_streaming,
                                    &mut self.active_op,
                                    &self.unmatched_destination,
                                    files,
                                    dirty,
                                );
                            }
                            DecodedEscape::None | DecodedEscape::CloseQuote => {}
                        }
                        return;
                    }
                    if *triple {
                        if ch == q {
                            *pending_quotes += 1;
                            if *pending_quotes == 3 {
                                finished_string = Some((std::mem::take(buf), is_streaming));
                            } else {
                                return;
                            }
                        }
                        if finished_string.is_none() && *pending_quotes > 0 {
                            let mut tmp = [0u8; 8];
                            let mut len = 0;
                            for _ in 0..*pending_quotes {
                                len += q.encode_utf8(&mut tmp[len..]).len();
                            }
                            let slice = std::str::from_utf8(&tmp[..len]).unwrap_or("");
                            *pending_quotes = 0;
                            push_py_str_slice(
                                buf,
                                slice,
                                is_streaming,
                                &mut self.active_op,
                                &self.unmatched_destination,
                                files,
                                dirty,
                            );
                        }
                    } else if ch == q {
                        finished_string = Some((std::mem::take(buf), is_streaming));
                    }
                    if finished_string.is_none() {
                        if ch == '\\' && !*raw {
                            escape.begin_backslash();
                            return;
                        }
                        let mut tmp = [0u8; 4];
                        push_py_str_slice(
                            buf,
                            ch.encode_utf8(&mut tmp),
                            is_streaming,
                            &mut self.active_op,
                            &self.unmatched_destination,
                            files,
                            dirty,
                        );
                        return;
                    }
                }
            }
            PyLexState::Normal { .. } => {}
        }
        if let Some((finished, is_streaming)) = finished_string {
            self.lex = PyLexState::Normal {
                ident: String::new(),
            };
            self.on_string_end(finished, is_streaming, files, dirty);
            if replay_char.is_none() {
                return;
            }
        }

        let ch = replay_char.unwrap_or(ch);
        if ch.is_ascii_alphanumeric() || ch == '_' {
            if let PyLexState::Normal { ident } = &mut self.lex {
                ident.push(ch);
            }
            return;
        }

        if ch == '"' || ch == '\'' {
            let prefix = if let PyLexState::Normal { ident } = &mut self.lex {
                std::mem::take(ident)
            } else {
                String::new()
            };
            let lower = prefix.to_ascii_lowercase();
            let is_valid_prefix =
                lower.is_empty() || matches!(lower.as_str(), "r" | "u" | "b" | "rb" | "br");
            if !is_valid_prefix && !prefix.is_empty() {
                self.stmt.push(PyTok::Ident(prefix));
                self.cancel_pending_op();
            }
            let raw = lower.contains('r');
            let streaming = is_valid_prefix && self.begin_streaming_string();
            self.lex = PyLexState::String {
                quote: ch,
                triple: false,
                raw,
                pending_quotes: 1,
                checking_open: true,
                escape: EscapeDecoder::default(),
                buf: String::new(),
                streaming,
            };
            return;
        }

        self.flush_ident();

        match ch {
            '#' => {
                self.lex = PyLexState::Comment;
            }
            '(' => {
                self.paren_depth += 1;
                self.stmt.push(PyTok::LParen);
                self.check_call_open(files, dirty);
            }
            ')' => {
                self.paren_depth = self.paren_depth.saturating_sub(1);
                self.call_stack.pop();
                self.stmt.push(PyTok::RParen);
                if matches!(
                    self.active_op,
                    Some(PyStreamOp::WriteLiteral { .. } | PyStreamOp::ReplaceAwaitOld { .. })
                ) {
                    self.active_op = None;
                }
            }
            '.' => self.stmt.push(PyTok::Dot),
            ',' => self.stmt.push(PyTok::Comma),
            '=' => self.stmt.push(PyTok::Eq),
            ':' => {
                self.stmt.push(PyTok::Colon);
                if self.paren_depth == 0 {
                    self.eval_statement();
                }
            }
            '\n' | ';' => {
                if self.paren_depth == 0 {
                    self.eval_statement();
                }
            }
            c if c.is_whitespace() => {}
            _ => {
                self.cancel_pending_op();
                self.stmt.push(PyTok::Other);
            }
        }
    }

    fn flush_ident(&mut self) {
        if let PyLexState::Normal { ident } = &mut self.lex
            && !ident.is_empty()
        {
            let name = std::mem::take(ident);
            self.cancel_pending_op();
            self.stmt.push(PyTok::Ident(name));
        }
    }

    fn cancel_pending_op(&mut self) {
        if matches!(
            self.active_op,
            Some(
                PyStreamOp::WriteLiteral { .. }
                    | PyStreamOp::ReplaceAwaitOld { .. }
                    | PyStreamOp::ReplaceAwaitNew { .. }
            )
        ) {
            self.active_op = None;
        }
    }

    fn check_call_open(&mut self, files: &mut HashMap<PathBuf, StreamedFile>, dirty: &mut bool) {
        let n = self.stmt.len();
        let mut ctx = PyCallContext::Neutral;
        if n >= 2
            && let PyTok::Ident(func) = &self.stmt[n - 2]
            && (n < 3 || self.stmt[n - 3] != PyTok::Dot)
            && matches!(func.as_str(), "print" | "len" | "repr" | "str" | "hash")
        {
            ctx = PyCallContext::ReadOnlySink;
        } else if n >= 3
            && let PyTok::Ident(method) = &self.stmt[n - 2]
            && self.stmt[n - 3] == PyTok::Dot
        {
            let receiver_tokens = &self.stmt[..n - 3];
            match method.as_str() {
                "write_text" => {
                    if let Some(path) = self.resolve_path_expr(receiver_tokens) {
                        self.written_paths.insert(path.clone());
                        ctx = PyCallContext::WriteTarget(path.clone());
                        self.active_op = Some(PyStreamOp::WriteLiteral { path });
                    }
                }
                "write" => {
                    if let Some(path) = self.resolve_file_expr(receiver_tokens) {
                        self.written_paths.insert(path.clone());
                        ctx = PyCallContext::WriteTarget(path.clone());
                        self.active_op = Some(PyStreamOp::WriteLiteral { path });
                    }
                }
                "replace" => {
                    if !self.in_readonly_sink() {
                        let source = self.resolve_text_expr(receiver_tokens);
                        let destination = self.enclosing_write_target();
                        let target = destination.clone().or_else(|| source.clone());
                        if let Some(dest) = destination
                            && Some(&dest) != source.as_ref()
                        {
                            if let Some(StreamedFile::Update { allow_context, .. }) =
                                files.get_mut(&dest)
                            {
                                *dirty |= std::mem::replace(allow_context, false);
                            }
                            self.unmatched_destination.insert(dest);
                        }
                        if let Some(path) = target {
                            self.replace_touched.insert(path.clone());
                            self.active_op = Some(PyStreamOp::ReplaceAwaitOld { path });
                        }
                    }
                }
                _ => {}
            }
        }
        self.call_stack.push(ctx);
    }

    fn begin_streaming_string(&mut self) -> bool {
        match self.active_op.take() {
            Some(PyStreamOp::WriteLiteral { path }) => {
                self.active_op = Some(PyStreamOp::WriteLiteral { path });
                true
            }
            Some(PyStreamOp::ReplaceAwaitOld { path }) => {
                self.active_op = Some(PyStreamOp::ReplaceOld {
                    path,
                    old: String::new(),
                });
                true
            }
            Some(PyStreamOp::ReplaceAwaitNew { path, old }) => {
                self.active_op = Some(PyStreamOp::ReplaceNew {
                    path,
                    old,
                    new: String::new(),
                });
                true
            }
            other => {
                self.active_op = other;
                false
            }
        }
    }

    fn on_string_end(
        &mut self,
        value: String,
        was_streaming: bool,
        files: &mut HashMap<PathBuf, StreamedFile>,
        dirty: &mut bool,
    ) {
        if was_streaming {
            match self.active_op.take() {
                Some(PyStreamOp::WriteLiteral { path }) => {
                    commit_partial(files, &path, dirty);
                }
                Some(PyStreamOp::ReplaceOld { path, old }) => {
                    self.active_op = Some(PyStreamOp::ReplaceAwaitNew { path, old });
                }
                Some(PyStreamOp::ReplaceNew { path, old, new }) => {
                    write_replace_hunk(
                        files,
                        &self.unmatched_destination,
                        &path,
                        &old,
                        &new,
                        true,
                        dirty,
                    );
                }
                other => {
                    self.active_op = other;
                }
            }
        }
        self.stmt.push(PyTok::Str(value));
    }

    fn resolve_path_expr(&self, tokens: &[PyTok]) -> Option<PathBuf> {
        match tokens.last()? {
            PyTok::Ident(name) => self.path_vars.get(name).cloned(),
            PyTok::RParen => extract_path_call(tokens, &self.script_cwd),
            _ => None,
        }
    }

    fn resolve_file_expr(&self, tokens: &[PyTok]) -> Option<PathBuf> {
        match tokens.last()? {
            PyTok::Ident(name) => self.file_vars.get(name).cloned(),
            PyTok::RParen => extract_open_call(tokens, &self.script_cwd, &self.path_vars),
            _ => None,
        }
    }

    fn resolve_text_expr(&self, tokens: &[PyTok]) -> Option<PathBuf> {
        match tokens.last()? {
            PyTok::Ident(name) => self.text_vars.get(name).cloned(),
            PyTok::RParen => extract_read_text_chain(
                tokens,
                &self.script_cwd,
                &self.path_vars,
                &self.file_vars,
                &self.text_vars,
            ),
            _ => None,
        }
    }

    fn eval_statement(&mut self) {
        let stmt = std::mem::take(&mut self.stmt);
        if stmt.is_empty() {
            return;
        }
        if matches!(stmt.first(), Some(PyTok::Ident(k)) if k == "with")
            && let Some(as_pos) = stmt
                .iter()
                .position(|t| matches!(t, PyTok::Ident(k) if k == "as"))
            && let Some(PyTok::Ident(var)) = stmt.get(as_pos + 1)
            && let Some(path) =
                extract_open_call(&stmt[1..as_pos], &self.script_cwd, &self.path_vars)
        {
            self.file_vars.insert(var.clone(), path);
            return;
        }

        if let [PyTok::Ident(var), PyTok::Eq, rhs @ ..] = stmt.as_slice() {
            if let [PyTok::Str(lit)] = rhs {
                self.path_vars
                    .insert(var.clone(), join_path(&self.script_cwd, Path::new(lit)));
            } else if let Some(path) = extract_path_call(rhs, &self.script_cwd) {
                self.path_vars.insert(var.clone(), path);
            } else if let Some(path) = extract_open_call(rhs, &self.script_cwd, &self.path_vars) {
                self.file_vars.insert(var.clone(), path);
            } else if let Some(path) = extract_read_text_chain(
                rhs,
                &self.script_cwd,
                &self.path_vars,
                &self.file_vars,
                &self.text_vars,
            ) {
                self.text_vars.insert(var.clone(), path);
            }
        }
    }
}

fn extract_path_call(tokens: &[PyTok], script_cwd: &Path) -> Option<PathBuf> {
    let n = tokens.len();
    if n >= 4
        && matches!(&tokens[n - 4], PyTok::Ident(name) if name == "Path")
        && tokens[n - 3] == PyTok::LParen
        && let PyTok::Str(path) = &tokens[n - 2]
        && tokens[n - 1] == PyTok::RParen
    {
        return Some(join_path(script_cwd, Path::new(path)));
    }
    None
}

fn extract_open_call(
    tokens: &[PyTok],
    script_cwd: &Path,
    path_vars: &HashMap<String, PathBuf>,
) -> Option<PathBuf> {
    let open_pos = tokens
        .iter()
        .rposition(|t| matches!(t, PyTok::Ident(name) if name == "open"))?;
    if tokens.get(open_pos + 1) != Some(&PyTok::LParen) {
        return None;
    }
    match tokens.get(open_pos + 2)? {
        PyTok::Str(path) => Some(join_path(script_cwd, Path::new(path))),
        PyTok::Ident(var) => path_vars.get(var).cloned(),
        _ => None,
    }
}

fn extract_read_text_chain(
    tokens: &[PyTok],
    script_cwd: &Path,
    path_vars: &HashMap<String, PathBuf>,
    file_vars: &HashMap<String, PathBuf>,
    text_vars: &HashMap<String, PathBuf>,
) -> Option<PathBuf> {
    for (idx, tok) in tokens.iter().enumerate() {
        if let PyTok::Ident(method) = tok
            && idx >= 2
            && tokens[idx - 1] == PyTok::Dot
        {
            let recv = &tokens[..idx - 1];
            match method.as_str() {
                "read_text" => {
                    if let Some(PyTok::Ident(var)) = recv.last()
                        && let Some(p) = path_vars.get(var)
                    {
                        return Some(p.clone());
                    }
                    if let Some(p) = extract_path_call(recv, script_cwd) {
                        return Some(p);
                    }
                }
                "read" => {
                    if let Some(PyTok::Ident(var)) = recv.last()
                        && let Some(p) = file_vars.get(var)
                    {
                        return Some(p.clone());
                    }
                    if let Some(p) = extract_open_call(recv, script_cwd, path_vars) {
                        return Some(p);
                    }
                }
                "replace" => {
                    if let Some(PyTok::Ident(var)) = recv.last()
                        && let Some(p) = text_vars.get(var)
                    {
                        return Some(p.clone());
                    }
                }
                _ => {}
            }
        }
    }
    None
}

fn push_py_str_slice(
    buf: &mut String,
    slice: &str,
    is_streaming: bool,
    active_op: &mut Option<PyStreamOp>,
    unmatched_destination: &HashSet<PathBuf>,
    files: &mut HashMap<PathBuf, StreamedFile>,
    dirty: &mut bool,
) {
    if buf.len() + slice.len() <= MAX_STREAM_FILE_BYTES {
        buf.push_str(slice);
    }
    if !is_streaming {
        return;
    }
    match active_op {
        Some(PyStreamOp::WriteLiteral { path }) => {
            if !files.contains_key(path) && files.len() >= MAX_STREAM_FILES {
                return;
            }
            let entry = files
                .entry(path.clone())
                .or_insert_with(|| StreamedFile::Add {
                    committed: String::new(),
                    partial: String::new(),
                    allow_empty: false,
                    append_only: false,
                });
            if let StreamedFile::Add {
                committed, partial, ..
            } = entry
                && committed.len() + partial.len() + slice.len() <= MAX_STREAM_FILE_BYTES
            {
                partial.push_str(slice);
                *dirty = true;
            }
        }
        Some(PyStreamOp::ReplaceOld { path, old }) => {
            if old.len() + slice.len() <= MAX_STREAM_FILE_BYTES {
                old.push_str(slice);
                write_replace_hunk(files, unmatched_destination, path, old, "", false, dirty);
            }
        }
        Some(PyStreamOp::ReplaceNew { path, old, new }) => {
            if new.len() + slice.len() <= MAX_STREAM_FILE_BYTES {
                new.push_str(slice);
                write_replace_hunk(files, unmatched_destination, path, old, new, false, dirty);
            }
        }
        _ => {}
    }
}

fn write_replace_hunk(
    files: &mut HashMap<PathBuf, StreamedFile>,
    unmatched_destination: &HashSet<PathBuf>,
    path: &Path,
    old: &str,
    new: &str,
    commit: bool,
    dirty: &mut bool,
) {
    if !files.contains_key(path) && files.len() >= MAX_STREAM_FILES {
        return;
    }
    let entry = files
        .entry(path.to_path_buf())
        .or_insert_with(|| StreamedFile::Update {
            committed: Vec::new(),
            partial: None,
            bytes: 0,
            allow_context: !unmatched_destination.contains(path),
        });
    if let StreamedFile::Update {
        committed,
        partial,
        bytes,
        ..
    } = entry
    {
        let size = old.len() + new.len() + 1;
        if *bytes + size > MAX_STREAM_FILE_BYTES {
            return;
        }
        if commit {
            partial.take();
            committed.push((old.to_owned(), new.to_owned()));
            *bytes += size;
        } else {
            *partial = Some((old.to_owned(), new.to_owned()));
        }
        *dirty = true;
    }
}

fn commit_partial(files: &mut HashMap<PathBuf, StreamedFile>, path: &Path, dirty: &mut bool) {
    if let Some(StreamedFile::Add {
        committed, partial, ..
    }) = files.get_mut(path)
        && !partial.is_empty()
    {
        committed.push_str(&std::mem::take(partial));
        *dirty = true;
    }
}

#[cfg(test)]
#[path = "shell_edit_stream_tests.rs"]
mod tests;

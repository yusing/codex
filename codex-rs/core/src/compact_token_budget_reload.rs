//! Source acquisition for a replacement context. Files and skills use the selected native
//! filesystem; managed skills and captured changes retain their external owners.

use std::collections::BTreeMap;
use std::collections::HashMap;
use std::collections::HashSet;
use std::collections::VecDeque;
use std::collections::hash_map::Entry;
use std::sync::Arc;

use codex_file_system::EnvironmentAccess;
use codex_file_system::EnvironmentAccessExt;
use codex_file_system::GetMetadataOptions;
use codex_file_system::ReadFileOptions;
use codex_protocol::error::CodexErr;
use codex_protocol::error::Result as CodexResult;
use codex_protocol::protocol::ReloadSelection;
use codex_protocol::source_acquisition::{AcquiredSource, SourceAcquisition, SourceAcquisitionPurpose, SourceTarget};
use codex_history::{CodexHarnessMetadata, ResponseItemEnvelope};
use codex_protocol::models::{FunctionCallOutputPayload, ResponseItem};
use codex_skills_extension::HostSkillsSnapshot;
use codex_utils_path_uri::PathUri;
use tokio_util::sync::CancellationToken;

use crate::session::session::Session;
use crate::session::step_context::StepContext;
use crate::session::turn_context::TurnEnvironment;
use crate::tools::handlers::resolve_tool_environment;
use crate::tools::handlers::unified_exec::preparation::ExecCommandLifetime;
use crate::tools::handlers::unified_exec::preparation::ParsedExecCommand;
use crate::unified_exec::ExecCommandRequest;
use crate::unified_exec::UnifiedExecContext;
use crate::unified_exec::UnifiedExecProcessManager;

const MAX_REFERENCE_DEPTH: usize = 3;
// Shared capture/retained-record capacity, not a per-source limit.
pub(crate) const MAX_CAPTURE_BYTES: usize = 32 << 20;
pub(crate) const CONTEXT_INTRODUCTION: &str = "Reloaded context (tool output). These source bodies are already read context; reread only missing or changed content. Failed reads are not complete context.";

enum Target {
    File(String),
    Skill(String),
    Changes(Vec<String>),
}

impl Target {
    fn fact(&self, path: Option<&PathUri>) -> SourceTarget {
        match self {
            Self::File(original) => SourceTarget::File { path: path.map(PathUri::inferred_native_path_string).unwrap_or_else(|| original.clone()) },
            Self::Skill(name) => SourceTarget::Skill { name: name.clone() },
            Self::Changes(selection) => SourceTarget::Changes { selection: selection.clone() },
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct AcquiredSources {
    pub body: String,
    pub sources: Vec<AcquiredSource>,
}

impl AcquiredSources {
    pub(crate) fn into_reload_item(self, purpose: SourceAcquisitionPurpose) -> ResponseItemEnvelope {
        let text = format!("{CONTEXT_INTRODUCTION}\n{}", self.body);
        let history_truncation_token_limit = codex_utils_output_truncation::approx_token_count(&text);
        ResponseItemEnvelope {
            item: ResponseItem::FunctionCallOutput {
                id: None, call_id: None, name: Some("reload".to_string()),
                namespace: Some("context".to_string()),
                output: FunctionCallOutputPayload::from_text(text),
                internal_chat_message_metadata_passthrough: None,
            },
            metadata: Some(CodexHarnessMetadata {
                history_truncation_token_limit: Some(history_truncation_token_limit),
                source_acquisition: Some(SourceAcquisition {
                    id: uuid::Uuid::now_v7().to_string(), purpose, sources: self.sources,
                }),
                ..Default::default()
            }),
        }
    }
}

struct Source {
    target: Target,
    ranges: Vec<(usize, usize)>,
}

pub(super) fn has_preload(selection: &codex_protocol::protocol::ReloadSelection) -> bool {
    selection.sources.iter().any(|reference| {
        let path = match parse_reference(reference) {
            Ok(Source {
                target: Target::File(path),
                ..
            }) => path,
            Ok(Source {
                target: Target::Skill(skill),
                ..
            }) => {
                let Some((_, path)) = skill.split_once('/') else {
                    return true;
                };
                path.to_string()
            }
            _ => return true,
        };
        std::path::Path::new(&path)
            .file_name()
            .is_none_or(|name| name != "AGENTS.md")
    })
}

pub(super) async fn acquire(
    session: &Arc<Session>,
    step: &Arc<StepContext>,
    cancellation: &CancellationToken,
) -> CodexResult<Option<ResponseItemEnvelope>> {
    let Some(selection) = session
        .reload_selection()
        .await
        .filter(|s| !s.sources.is_empty())
    else {
        return Ok(None);
    };
    Ok(
        acquire_selection(session, step, &selection, cancellation, None)
            .await?
            .map(|body| body.into_reload_item(SourceAcquisitionPurpose::Reload)),
    )
}

/// Acquire an explicit selection using the recipient's native context, without reading or
/// replacing its retained reset selection. The caller owns publication of the frozen result.
/// A bounded operation charges framing, diagnostics and bodies, and also bounds cached reads.
pub(crate) async fn acquire_selection(
    session: &Arc<Session>,
    step: &Arc<StepContext>,
    selection: &ReloadSelection,
    cancellation: &CancellationToken,
    max_bytes: Option<usize>,
) -> CodexResult<Option<AcquiredSources>> {
    if selection.sources.is_empty() {
        return Ok(None);
    }
    let environment = resolve_tool_environment(&step.environments, None)
        .map_err(|error| CodexErr::Fatal(format!("Reload acquisition: {error}")))?;
    // No partially acquired result escapes cancellation or a capacity failure.
    tokio::select! {
        biased;
        _ = cancellation.cancelled() => Err(CodexErr::TurnAborted),
        body = read_sources(session, step, environment, &selection.sources, cancellation, max_bytes) => {
            let body = body?;
            Ok((!body.body.is_empty()).then_some(body))
        }
    }
}

async fn read_sources(
    session: &Arc<Session>,
    step: &Arc<StepContext>,
    environment: &TurnEnvironment,
    sources: &[String],
    cancellation: &CancellationToken,
    max_bytes: Option<usize>,
) -> CodexResult<AcquiredSources> {
    let access = step.fs_accessor(environment);
    let skills = HostSkillsSnapshot::for_step(&step.extension_data, &step.turn.extension_data);
    let mut pending = sources
        .iter()
        .cloned()
        .map(|source| (source, environment.cwd().clone(), 1))
        .collect::<VecDeque<_>>();
    let mut files: HashMap<(PathUri, bool), Result<String, String>> = HashMap::new();
    let mut managed_skills = HashMap::new();
    let mut seen = HashSet::new();
    let mut changes_seen = HashSet::new();
    let mut output = SourceOutput::new(max_bytes);
    let mut cached_bytes = 0usize;
    // Breadth-first acquisition encounters the shortest reference depth first, so cycles and
    // repeated references need neither rereads nor a second expansion pass.
    while let Some((reference, base, depth)) = pending.pop_front() {
        if cancellation.is_cancelled() {
            return Err(CodexErr::TurnAborted);
        }
        let source = match parse_reference(&reference) {
            Ok(source) => source,
            Err(error) => {
                output.failure(&reference, SourceTarget::Selection, &error)?;
                continue;
            }
        };
        if let Target::Changes(ids) = &source.target {
            if !changes_seen.insert(ids.clone()) {
                continue;
            }
            if depth > MAX_REFERENCE_DEPTH {
                output.failure(&reference, source.target.fact(None), "nested source depth limit 3 exceeded")?;
                continue;
            }
            let command = std::iter::once("mchanges".to_string())
                .chain(ids.iter().cloned())
                .collect();
            match read_external(session, step, environment, command, cancellation).await {
                Ok(body) => {
                    output.body(&reference, source.target.fact(None), &body)?;
                }
                Err(error) => output.failure(&reference, source.target.fact(None), &error)?,
            }
            continue;
        }
        let (path, skill_root) = match source_path(&source.target, &base, skills.as_deref()) {
            Ok(path) => path,
            Err(error) => {
                output.failure(&reference, source.target.fact(None), &error)?;
                continue;
            }
        };
        if path.basename().as_deref() == Some("AGENTS.md")
            || !seen.insert((
                path.clone(),
                matches!(&source.target, Target::Skill(_)),
                source.ranges.clone(),
            ))
        {
            continue;
        }
        if depth > MAX_REFERENCE_DEPTH {
            output.failure(&reference, source.target.fact(Some(&path)), "nested source depth limit 3 exceeded")?;
            continue;
        }
        let managed = if let Some(root) = skill_root {
            let managed = match managed_skills.entry(root) {
                Entry::Occupied(entry) => entry.into_mut(),
                Entry::Vacant(entry) => {
                    let result = match entry.key().join_descendant(".skills-mgr-placeholder") {
                        Ok(marker) => match access
                            .get_metadata(&marker, GetMetadataOptions::default())
                            .await
                        {
                            Ok(_) => Ok(true),
                            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
                            Err(error) => Err(error.to_string()),
                        },
                        Err(error) => Err(error.to_string()),
                    };
                    entry.insert(result)
                }
            };
            match managed {
                Ok(managed) => *managed,
                Err(error) => {
                    output.failure(&reference, source.target.fact(Some(&path)), error)?;
                    continue;
                }
            }
        } else {
            false
        };
        let body = match files.entry((path.clone(), managed)) {
            Entry::Occupied(entry) => entry.into_mut(),
            Entry::Vacant(entry) => {
                let result = if managed && let Target::Skill(skill) = &source.target {
                    read_external(
                        session,
                        step,
                        environment,
                        vec![
                            "skills-mgr".to_string(),
                            "get".to_string(),
                            "--codex".to_string(),
                            skill.clone(),
                        ],
                        cancellation,
                    )
                    .await
                } else {
                    read_file(&access, &path).await
                };
                cached_bytes = cached_bytes.saturating_add(match &result {
                    Ok(body) => body.len(),
                    Err(error) => error.len(),
                });
                check_capacity(cached_bytes, max_bytes)?;
                entry.insert(result)
            }
        };
        let body = match body {
            Ok(body) => body,
            Err(error) => {
                output.failure(&reference, source.target.fact(Some(&path)), error)?;
                continue;
            }
        };
        let body = match selected_lines(body, &source.ranges) {
            Ok(body) => body,
            Err(error) => {
                output.failure(&reference, source.target.fact(Some(&path)), &error)?;
                continue;
            }
        };
        output.body(&reference, source.target.fact(Some(&path)), &body)?;
        let nested_base = if managed {
            environment.cwd().clone()
        } else {
            path.parent().unwrap_or_else(|| base.clone())
        };
        for nested in references(&body) {
            pending.push_back((nested, nested_base.clone(), depth + 1));
        }
    }
    Ok(output.finish())
}

async fn read_file(access: &dyn EnvironmentAccess, path: &PathUri) -> Result<String, String> {
    let metadata = access
        .get_metadata(path, GetMetadataOptions::default())
        .await
        .map_err(|error| error.to_string())?;
    if !metadata.is_file {
        return Err("source is not a regular file".to_string());
    }
    // The accessor retains the selected environment's current permissions and native read limits.
    access
        .read_file_text(path, ReadFileOptions::default())
        .await
        .map_err(|error| error.to_string())
}

fn source_path(
    target: &Target,
    base: &PathUri,
    skills: Option<&HostSkillsSnapshot>,
) -> Result<(PathUri, Option<PathUri>), String> {
    match target {
        Target::File(path) => base
            .join(path)
            .map(|path| (path, None))
            .map_err(|error| error.to_string()),
        Target::Skill(reference) => {
            let (name, relative) = reference
                .split_once('/')
                .map_or((reference.as_str(), None), |(name, path)| {
                    (name, Some(path))
                });
            let Some(skills) = skills else {
                return Err(format!("native skill {name:?} is unavailable"));
            };
            let mut matches = skills
                .outcome()
                .skills
                .iter()
                .filter(|skill| skill.name == name && skills.outcome().is_skill_enabled(skill))
                .map(|skill| &skill.path_to_skills_md);
            let Some(path) = matches.next() else {
                return Err(format!("native skill {name:?} is unavailable"));
            };
            if matches.any(|other| other != path) {
                return Err(format!(
                    "ambiguous native skill {name:?}; select its file explicitly"
                ));
            }
            let root = path
                .parent()
                .ok_or_else(|| "skill has no parent directory".to_string())?;
            let path = match relative {
                None => path.clone(),
                Some(relative) => root
                    .join_descendant(relative)
                    .map_err(|error| error.to_string())?,
            };
            Ok((path, Some(root)))
        }
        Target::Changes(_) => Err("captured changes are not filesystem paths".to_string()),
    }
}

fn selected_lines<'a>(
    body: &'a str,
    ranges: &[(usize, usize)],
) -> Result<std::borrow::Cow<'a, str>, String> {
    if ranges.is_empty() {
        return Ok(std::borrow::Cow::Borrowed(body));
    }
    // Store only requested boundaries, not one pointer for every line in a large file.
    let mut offsets = BTreeMap::new();
    let mut last = 0;
    for &(start, end) in ranges {
        offsets.insert(start - 1, 0);
        offsets.insert(end, 0);
        last = last.max(end);
    }
    let mut position = 0;
    let mut lines = 0;
    for line in body.split_inclusive('\n').take(last) {
        position += line.len();
        lines += 1;
        if let Some(offset) = offsets.get_mut(&lines) {
            *offset = position;
        }
    }
    let mut selected = String::new();
    for &(start, end) in ranges {
        if end > lines {
            return Err(format!(
                "requested lines {start}-{end}; source has {lines} lines"
            ));
        }
        if !selected.is_empty() && !selected.ends_with('\n') {
            selected.push('\n');
        }
        selected.push_str(&body[offsets[&(start - 1)]..offsets[&end]]);
    }
    Ok(std::borrow::Cow::Owned(selected))
}

async fn read_external(
    session: &Arc<Session>,
    step: &Arc<StepContext>,
    environment: &TurnEnvironment,
    command: Vec<String>,
    cancellation: &CancellationToken,
) -> Result<String, String> {
    let display =
        shlex::try_join(command.iter().map(String::as_str)).map_err(|error| error.to_string())?;
    let arguments = serde_json::json!({"cmd": display, "login": false}).to_string();
    let mut prepared = ParsedExecCommand::parse(&arguments, session, step, environment)
        .and_then(|parsed| parsed.prepare(ExecCommandLifetime::OneShot))
        .map_err(|error| error.to_string())?;
    // Approval receives the actual argv. The shell-shaped metadata is only its display text.
    prepared.resolved.command = command;
    prepared.completion_timeout = None;
    let context = UnifiedExecContext::new(
        Arc::clone(session),
        Arc::clone(step),
        cancellation.child_token(),
        format!("reload-source-{}", uuid::Uuid::now_v7()),
    );
    let request = ExecCommandRequest {
        prepared,
        process_id: session
            .services
            .unified_exec_manager
            .allocate_process_id()
            .await,
        network: step.turn.network.clone(),
    };
    let output =
        UnifiedExecProcessManager::exec_command_to_completion_without_timeout(request, &context)
            .await
            .map_err(|error| error.to_string())?;
    if output.raw_output.omitted_bytes != 0 {
        return Err(
            "source exceeded the native capture limit; no partial body attached".to_string(),
        );
    }
    match output.exit_code {
        Some(0) => Ok(output.raw_output.text),
        Some(code) => Err(format!(
            "source reader exited with code {code}:\n{}",
            output.raw_output.text
        )),
        None => Err("source reader did not complete".to_string()),
    }
}

fn check_capacity(bytes: usize, max_bytes: Option<usize>) -> CodexResult<()> {
    if max_bytes.is_some_and(|limit| bytes > limit) {
        return Err(CodexErr::InvalidRequest(
            "source selection exceeds retained record capacity; select smaller ranges".to_string(),
        ));
    }
    Ok(())
}

/// Count the final envelope too, without rebuilding it for every source.
struct SourceOutput {
    sources: Vec<AcquiredSource>,
    text: String,
    ranges: String,
    max_bytes: Option<usize>,
}

impl SourceOutput {
    fn new(max_bytes: Option<usize>) -> Self {
        Self {
            sources: Vec::new(),
            text: String::new(),
            ranges: String::new(),
            max_bytes,
        }
    }

    fn check_append(&self, bytes: usize, range: &str) -> CodexResult<()> {
        let text_len = self.text.len().saturating_add(bytes);
        let ranges_len = self.ranges.len() + range.len();
        let framed_len = if ranges_len == 0 {
            text_len
        } else {
            text_len.saturating_add(ranges_len).saturating_add(
                format!(
                    "<mekugi-source-v1 bytes=\"{text_len}\" bodies=\"\">\n\n</mekugi-source-v1>"
                )
                .len(),
            )
        };
        check_capacity(framed_len, self.max_bytes)
    }

    fn body(&mut self, source: &str, target: SourceTarget, body: &str) -> CodexResult<()> {
        let prefix = format!("\nSource {source:?}:\n");
        let start = self.text.len() + prefix.len();
        let end = start + body.len();
        let separator = if self.ranges.is_empty() { "" } else { "," };
        let range = format!("{separator}{start}:{end}");
        self.check_append(prefix.len() + body.len() + 1, &range)?;
        self.text.push_str(&prefix);
        self.text.push_str(body);
        self.text.push('\n');
        self.ranges.push_str(&range);
        self.sources.push(AcquiredSource { reference: source.to_string(), target, success: true, diagnostic: None });
        Ok(())
    }

    fn failure(&mut self, source: &str, target: SourceTarget, error: &str) -> CodexResult<()> {
        let record = format!("\nSource {source:?}: CONTENT NOT ATTACHED: {error}\n");
        self.check_append(record.len(), "")?;
        self.text.push_str(&record);
        self.sources.push(AcquiredSource { reference: source.to_string(), target, success: false, diagnostic: Some(error.to_string()) });
        Ok(())
    }

    fn finish(self) -> AcquiredSources {
        // Only successful source bytes are deduplication sources, not framing or failures.
        let body = if self.ranges.is_empty() { self.text } else { format!(
            "<mekugi-source-v1 bytes=\"{}\" bodies=\"{}\">\n{}\n</mekugi-source-v1>",
            self.text.len(),
            self.ranges,
            self.text
        ) };
        AcquiredSources { body, sources: self.sources }
    }
}

/// Coordination prose selects files only. Nested acquired text retains the shared grammar.
pub(crate) fn message_selection(text: &str) -> ReloadSelection {
    let mut seen = HashSet::new();
    ReloadSelection {
        sources: references(text)
            .into_iter()
            .filter(|reference| reference.starts_with('@') && seen.insert(reference.clone()))
            .collect(),
    }
}

/// Automatic successful-final attachments select captures only, using the same
/// reference grammar as explicit reloads. Malformed references retain diagnostics.
pub(crate) fn completion_selection(text: &str) -> ReloadSelection {
    let mut seen = HashSet::new();
    ReloadSelection {
        sources: references(text)
            .into_iter()
            .filter(|reference| {
                reference.starts_with("mchanges:") && seen.insert(reference.clone())
            })
            .collect(),
    }
}

fn parse_reference(reference: &str) -> Result<Source, String> {
    let invalid = || format!("invalid source reference {reference:?}");
    if reference.contains(['\0', '\r', '\n']) {
        return Err(invalid());
    }
    if let Some(changes) = reference.strip_prefix("mchanges:") {
        let ids = changes.split(',').map(str::to_string).collect::<Vec<_>>();
        for id in &ids {
            let (first, last) = id
                .split_once("..")
                .map_or((id.as_str(), None), |(first, last)| (first, Some(last)));
            if !change_id(first, false) || last.is_some_and(|last| !change_id(last, true)) {
                return Err(invalid());
            }
        }
        return Ok(Source {
            target: Target::Changes(ids),
            ranges: Vec::new(),
        });
    }
    let (target, suffix) = if let Some(path) = reference.strip_prefix('@') {
        let (path, suffix) = if path.starts_with(['\'', '"']) {
            let (decoded, end) = quoted(path)?;
            (decoded, &path[end..])
        } else {
            let (path, suffix) = split_ranges(path);
            (path.to_string(), suffix)
        };
        if path.is_empty() || path.starts_with('@') || path.contains(['\0', '\r', '\n']) {
            return Err(invalid());
        }
        (Target::File(path), suffix)
    } else if let Some(skill) = reference.strip_prefix('$') {
        let (skill, suffix) = split_ranges(skill);
        if skill.is_empty()
            || skill.starts_with('-')
            || skill
                .chars()
                .any(|c| c.is_whitespace() || "\\\"'`$#()[]{}<>,;".contains(c))
            || skill.split('/').any(|part| matches!(part, "" | "." | ".."))
        {
            return Err(invalid());
        }
        (Target::Skill(skill.to_string()), suffix)
    } else {
        return Err(invalid());
    };
    let mut ranges = Vec::new();
    if !suffix.is_empty() {
        for span in suffix.strip_prefix('#').ok_or_else(invalid)?.split(',') {
            let span = span.strip_prefix('L').ok_or_else(invalid)?;
            let (first, last) = span.split_once("-L").unwrap_or((span, span));
            let start = first.parse::<usize>().map_err(|_| invalid())?;
            let end = last.parse::<usize>().map_err(|_| invalid())?;
            if start == 0 || end < start || start.to_string() != first || end.to_string() != last {
                return Err(invalid());
            }
            ranges.push((start, end));
        }
    }
    Ok(Source { target, ranges })
}

fn split_ranges(reference: &str) -> (&str, &str) {
    reference
        .rfind("#L")
        .map_or((reference, ""), |index| reference.split_at(index))
}

fn change_id(id: &str, abbreviated: bool) -> bool {
    let prefix = id.bytes().take_while(u8::is_ascii_lowercase).count();
    let number = &id[prefix..];
    (prefix > 0 || abbreviated)
        && number.starts_with(|c: char| ('1'..='9').contains(&c))
        && number.bytes().all(|byte| byte.is_ascii_digit())
}

fn quoted(text: &str) -> Result<(String, usize), String> {
    if let Some(rest) = text.strip_prefix('\'') {
        let end = rest
            .find('\'')
            .ok_or_else(|| "unclosed source path quote".to_string())?;
        return Ok((rest[..end].to_string(), end + 2));
    }
    let mut stream = serde_json::Deserializer::from_str(text).into_iter::<String>();
    let value = stream
        .next()
        .ok_or_else(|| "missing quoted source path".to_string())?
        .map_err(|error| error.to_string())?;
    Ok((value, stream.byte_offset()))
}

/// Recognize explicit references, not bare paths, escaped markers, email addresses or shell vars.
fn references(text: &str) -> Vec<String> {
    let mut found = Vec::new();
    let mut index = 0;
    while index < text.len() {
        let tail = &text[index..];
        let marker = text.as_bytes()[index];
        let changes = tail.starts_with("mchanges:");
        if !matches!(marker, b'@' | b'$') && !changes {
            index += tail.chars().next().map_or(1, char::len_utf8);
            continue;
        }
        if let Some(previous) = text[..index].chars().next_back()
            && !previous.is_whitespace()
            && !"\"'`([{ :".contains(previous)
        {
            index += 1;
            continue;
        }
        let mut exact_json = false;
        if index > 0
            && text.as_bytes()[index - 1] == b'"'
            && let Ok((decoded, size)) = quoted(&text[index - 1..])
        {
            exact_json = true;
            if parse_reference(&decoded).is_ok() {
                found.push(decoded);
                index += size - 1;
                continue;
            }
        }
        let start = index;
        let mut end = index + if changes { "mchanges:".len() } else { 1 };
        if end == text.len()
            || marker == b'@' && text.as_bytes()[end] == b'@'
            || marker == b'$' && b"${(".contains(&text.as_bytes()[end])
        {
            index = end;
            continue;
        }
        if marker == b'@' && matches!(text.as_bytes()[end], b'\'' | b'"') {
            match quoted(&text[end..]) {
                Ok((_, size)) => end += size,
                Err(_) => {
                    index = end + 1;
                    continue;
                }
            }
            if !text[end..].starts_with("#L") {
                found.push(text[start..end].to_string());
                index = end;
                continue;
            }
        }
        let mut ranged = false;
        while end < text.len() {
            ranged |= text[end..].starts_with("#L");
            let Some(character) = text[end..].chars().next() else {
                break;
            };
            if character.is_whitespace()
                || "\"'`()[]{}<>;".contains(character)
                || character == ',' && !changes && !ranged
            {
                break;
            }
            end += character.len_utf8();
        }
        index = end.max(start + 1);
        while !exact_json && end > start + 1 && b".,:!?".contains(&text.as_bytes()[end - 1]) {
            if changes && (text[start..end].ends_with("..") || end == start + "mchanges:".len()) {
                break;
            }
            end -= 1;
        }
        if end > start + 1 {
            found.push(text[start..end].to_string());
        }
    }
    found
}

#[cfg(test)]
mod tests {
    #[test]
    fn final_capture_selection_reuses_reference_grammar_without_files_or_skills() {
        for (text, expected) in [
            (
                "Finished mchanges:amber1..3,birch2. Again `mchanges:amber1..3,birch2`.",
                vec!["mchanges:amber1..3,birch2"],
            ),
            (
                "See @source.go, $skill and mchanges:amber1, then mchanges:birch2..4!",
                vec!["mchanges:amber1", "mchanges:birch2..4"],
            ),
            (
                "Malformed mchanges:amber1.. retains a diagnostic.",
                vec!["mchanges:amber1.."],
            ),
            (
                r"Escaped \mchanges:amber1; @source.go and $skill are not final captures.",
                vec![],
            ),
            (
                r#"{"sources":["mchanges:amber1"]}"#,
                vec!["mchanges:amber1"],
            ),
        ] {
            assert_eq!(super::completion_selection(text).sources, expected);
        }
    }

    use super::*;
    use codex_history::RetainedContextEvent;
    use codex_login::CodexAuth;
    use codex_protocol::error::CodexErrorDetails;
    use codex_utils_absolute_path::AbsolutePathBuf;
    use pretty_assertions::assert_eq;

    async fn fixture() -> (tempfile::TempDir, Arc<Session>, Arc<StepContext>) {
        let directory = tempfile::tempdir().expect("recipient workspace");
        let (session, turn, _events) =
            crate::session::tests::make_session_and_context_with_auth_and_config_and_rx(
                CodexAuth::from_api_key("test"),
                Vec::new(),
                |config| {
                    config.cwd = AbsolutePathBuf::from_absolute_path(directory.path())
                        .expect("absolute workspace");
                },
            )
            .await;
        let step = StepContext::for_test(turn);
        (directory, session, step)
    }

    #[test]
    fn message_references_preserve_explicit_forms_and_ignore_literal_markers() {
        let selection = message_selection(
            r#"Read @correction.md. @source.txt! @source.txt @"with spaces.txt", @"punctuation.txt.". {"sources":["@json.txt."]} @missing.txt? \@literal user@example.com @AGENTS.md; markers @ @@ @@@ $skill mchanges:amber1"#,
        );
        assert_eq!(
            selection.sources,
            vec![
                "@correction.md",
                "@source.txt",
                "@\"with spaces.txt\"",
                "@\"punctuation.txt.\"",
                "@json.txt.",
                "@missing.txt",
                "@AGENTS.md",
            ]
        );
        assert_eq!(
            message_selection(r##"Selected @'space name'#L2-L3,L5 and "@\"json space\"#L1"."##)
                .sources,
            vec!["@'space name'#L2-L3,L5", "@\"json space\"#L1"]
        );
    }

    #[test]
    fn budget_counts_diagnostics_framing_and_utf8_body_boundaries() {
        let mut output = SourceOutput::new(None);
        output.failure("@missing", SourceTarget::Selection, "not found").unwrap();
        output.body("@one", SourceTarget::Selection, "é\n").unwrap();
        output.body("@two", SourceTarget::Selection, "<mekugi-source-v1>").unwrap();
        let text = output.text.clone();
        let ranges = output.ranges.clone();
        let expected = output.finish();
        let mut bounded = SourceOutput::new(Some(expected.body.len()));
        bounded.failure("@missing", SourceTarget::Selection, "not found").unwrap();
        bounded.body("@one", SourceTarget::Selection, "é\n").unwrap();
        bounded.body("@two", SourceTarget::Selection, "<mekugi-source-v1>").unwrap();
        assert!(bounded.failure("@extra", SourceTarget::Selection, "denied").is_err());
        assert_eq!(bounded.finish(), expected);
        let bodies = ranges
            .split(',')
            .map(|range| {
                let (start, end) = range.split_once(':').unwrap();
                &text[start.parse::<usize>().unwrap()..end.parse::<usize>().unwrap()]
            })
            .collect::<Vec<_>>();
        assert_eq!(bodies, vec!["é\n", "<mekugi-source-v1>"]);
        let mut too_small = SourceOutput::new(Some(expected.body.len() - 1));
        too_small.failure("@missing", SourceTarget::Selection, "not found").unwrap();
        too_small.body("@one", SourceTarget::Selection, "é\n").unwrap();
        assert!(too_small.body("@two", SourceTarget::Selection, "<mekugi-source-v1>").is_err());
    }

    #[tokio::test]
    async fn explicit_acquisition_is_frozen_and_preserves_retained_reset_selection() {
        let (directory, session, step) = fixture().await;
        for (name, body) in [
            ("root.txt", "ROOT @nested.txt @AGENTS.md"),
            ("nested.txt", "NESTED @third.txt"),
            ("third.txt", "THIRD @fourth.txt"),
            ("fourth.txt", "MUST_NOT_READ"),
            ("AGENTS.md", "HOST_ONLY"),
            ("reset.txt", "RETAINED_RESET"),
        ] {
            std::fs::write(directory.path().join(name), body).unwrap();
        }
        let retained = ReloadSelection {
            sources: vec!["@reset.txt".to_string()],
        };
        session
            .record_retained_context(RetainedContextEvent::ReloadSelection {
                selection: retained.clone(),
            })
            .await;
        let selection = ReloadSelection {
            sources: vec![
                "@root.txt".to_string(),
                "@root.txt".to_string(),
                "@missing.txt".to_string(),
                "$missing-skill".to_string(),
            ],
        };
        let cancellation = CancellationToken::new();
        let frozen = acquire_selection(&session, &step, &selection, &cancellation, Some(32 << 20))
            .await
            .unwrap()
            .unwrap();
        for expected in [
            "ROOT",
            "NESTED",
            "THIRD",
            "missing.txt",
            "CONTENT NOT ATTACHED",
            "depth limit 3",
        ] {
            assert!(frozen.body.contains(expected), "missing {expected}: {frozen:?}");
        }
        for excluded in ["MUST_NOT_READ", "HOST_ONLY", "RETAINED_RESET"] {
            assert!(
                !frozen.body.contains(excluded),
                "unexpected {excluded}: {frozen:?}"
            );
        }
        assert_eq!(frozen.body.matches("ROOT").count(), 1);
        assert_eq!(frozen.sources.len(), 6);
        assert!(frozen.sources.iter().any(|source| source.success && source.target == SourceTarget::File {
            path: directory.path().join("root.txt").to_string_lossy().into_owned(),
        }));
        assert!(frozen.sources.iter().any(|source| !source.success && source.target == SourceTarget::Skill { name: "missing-skill".to_string() }));
        assert!(frozen.sources.iter().any(|source| source.reference == "@fourth.txt" && !source.success && source.diagnostic.as_ref().unwrap().contains("depth limit")));

        assert_eq!(session.reload_selection().await, Some(retained.clone()));
        std::fs::write(directory.path().join("root.txt"), "LATER_EDIT").unwrap();
        assert!(!frozen.body.contains("LATER_EDIT"));
        let reset = acquire(&session, &step, &cancellation)
            .await
            .unwrap()
            .unwrap();
        let reset = match reset.item { ResponseItem::FunctionCallOutput { output, .. } => output.text_content().unwrap().to_owned(), _ => panic!("reload output") };
        assert!(reset.starts_with("Reloaded context (tool output)."));
        assert!(reset.contains("RETAINED_RESET"));
        assert!(!reset.contains("LATER_EDIT"));
        assert_eq!(session.reload_selection().await, Some(retained));
    }

    #[tokio::test]
    async fn ranges_scan_only_selected_lines_and_failures_do_not_discard_other_sources() {
        let (directory, session, step) = fixture().await;
        std::fs::create_dir(directory.path().join("sub")).unwrap();
        std::fs::write(
            directory.path().join("sub/root.txt"),
            "OMITTED @missing-unselected\nSELECTED @child.txt\n",
        )
        .unwrap();
        std::fs::write(directory.path().join("sub/child.txt"), "BESIDE_SOURCE").unwrap();
        let selection = ReloadSelection {
            sources: vec![
                "@sub/root.txt#L2".to_string(),
                "@sub/root.txt#L9".to_string(),
            ],
        };
        let body = acquire_selection(
            &session,
            &step,
            &selection,
            &CancellationToken::new(),
            Some(4096),
        )
        .await
        .unwrap()
        .unwrap();
        assert!(body.body.contains("BESIDE_SOURCE"));
        assert!(body.body.contains("requested lines 9-9"));
        assert!(!body.body.contains("missing-unselected"));
        assert!(!body.body.contains("OMITTED"));
    }

    #[tokio::test]
    async fn recipient_permissions_fail_closed_without_sandbox_runtime_paths() {
        use crate::config::PermissionProfileSnapshot;
        use crate::environment_selection::TurnEnvironmentState;
        use codex_protocol::models::PermissionProfile;
        use codex_protocol::permissions::FileSystemAccessMode;
        use codex_protocol::permissions::FileSystemPath;
        use codex_protocol::permissions::FileSystemSandboxEntry;

        let (directory, session, mut step) = fixture().await;
        std::fs::write(directory.path().join("allowed.txt"), "ALLOWED").unwrap();
        std::fs::write(directory.path().join("secret.env"), "DENIED_BYTES").unwrap();
        let profile = PermissionProfile::read_only();
        let mut policy = profile.file_system_sandbox_policy();
        policy.entries.push(FileSystemSandboxEntry {
            path: FileSystemPath::GlobPattern {
                pattern: format!("{}/*.env", directory.path().display()),
            },
            access: FileSystemAccessMode::Deny,
            missing_path_behavior: None,
        });
        let TurnEnvironmentState::Ready(environment) =
            &mut Arc::get_mut(&mut step).unwrap().environments.environments[0]
        else {
            panic!("ready recipient environment");
        };
        environment.config_mut().permission_profile = PermissionProfileSnapshot::legacy(
            PermissionProfile::from_runtime_permissions_with_enforcement(
                profile.enforcement(),
                &policy,
                profile.network_sandbox_policy(),
            ),
        );
        let selection = ReloadSelection {
            sources: vec!["@secret.env".to_string(), "@allowed.txt".to_string()],
        };
        let body = acquire_selection(
            &session,
            &step,
            &selection,
            &CancellationToken::new(),
            Some(4096),
        )
        .await
        .unwrap()
        .unwrap();
        assert!(!body.body.contains("ALLOWED"), "{body:?}");
        assert!(body.body.contains("secret.env\": CONTENT NOT ATTACHED:"));
        assert!(!body.body.contains("DENIED_BYTES"));
        assert_eq!(body.sources.len(), 2);
        for source in body.sources {
            assert!(!source.success);
            assert_eq!(source.diagnostic.as_deref(), Some("sandboxed filesystem operations require configured runtime paths"));
        }
    }

    #[tokio::test]
    async fn operation_budget_and_cancellation_reject_without_partial_result_or_selection_change() {
        let (directory, session, step) = fixture().await;
        std::fs::write(directory.path().join("a"), "a".repeat(128)).unwrap();
        std::fs::write(directory.path().join("b"), "b".repeat(128)).unwrap();
        let selection = ReloadSelection {
            sources: vec!["@a".to_string(), "@b".to_string()],
        };
        let error = acquire_selection(
            &session,
            &step,
            &selection,
            &CancellationToken::new(),
            Some(256),
        )
        .await
        .expect_err("aggregate bytes including framing exceed budget");
        assert!(matches!(
            error.details(),
            CodexErrorDetails::InvalidRequest(_)
        ));
        let cancellation = CancellationToken::new();
        cancellation.cancel();
        let error = acquire_selection(&session, &step, &selection, &cancellation, Some(4096))
            .await
            .expect_err("cancelled preparation");
        assert!(matches!(error.details(), CodexErrorDetails::TurnAborted));
        assert_eq!(session.reload_selection().await, None);
    }
}

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

enum Target {
    File(String),
    Skill(String),
    Changes(Vec<String>),
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
) -> CodexResult<Option<String>> {
    let Some(selection) = session
        .reload_selection()
        .await
        .filter(|s| !s.sources.is_empty())
    else {
        return Ok(None);
    };
    let environment = resolve_tool_environment(&step.environments, None)
        .map_err(|error| CodexErr::Fatal(format!("Reload acquisition: {error}")))?;
    // Dropping a native read cannot install a partially acquired replacement context.
    tokio::select! {
        biased;
        _ = cancellation.cancelled() => Err(CodexErr::TurnAborted),
        body = read_sources(session, step, environment, selection.sources, cancellation) => {
            Ok((!body.is_empty()).then(|| format!(
                "Reloaded context (tool output). These source bodies are already read context; reread only missing or changed content. Failed reads are not complete context.\n{body}"
            )))
        }
    }
}

async fn read_sources(
    session: &Arc<Session>,
    step: &Arc<StepContext>,
    environment: &TurnEnvironment,
    sources: Vec<String>,
    cancellation: &CancellationToken,
) -> String {
    let access = step.fs_accessor(environment);
    let skills = HostSkillsSnapshot::for_step(&step.extension_data, &step.turn.extension_data);
    let mut pending = sources
        .into_iter()
        .map(|source| (source, environment.cwd().clone(), 1))
        .collect::<VecDeque<_>>();
    let mut files: HashMap<(PathUri, bool), Result<String, String>> = HashMap::new();
    let mut managed_skills = HashMap::new();
    let mut seen = HashSet::new();
    let mut changes_seen = HashSet::new();
    let mut output = String::new();
    let mut bodies = Vec::new();
    // Breadth-first acquisition encounters the shortest reference depth first, so cycles and
    // repeated references need neither rereads nor a second expansion pass.
    while let Some((reference, base, depth)) = pending.pop_front() {
        let source = match parse_reference(&reference) {
            Ok(source) => source,
            Err(error) => {
                append_failure(&mut output, &reference, &error);
                continue;
            }
        };
        if let Target::Changes(ids) = &source.target {
            if !changes_seen.insert(ids.clone()) {
                continue;
            }
            if depth > MAX_REFERENCE_DEPTH {
                append_failure(
                    &mut output,
                    &reference,
                    "nested source depth limit 3 exceeded",
                );
                continue;
            }
            let command = std::iter::once("mchanges".to_string())
                .chain(ids.iter().cloned())
                .collect();
            match read_external(session, step, environment, command, cancellation).await {
                Ok(body) => {
                    append_body(&mut output, &reference, &body);
                    bodies.push((output.len() - body.len() - 1, output.len() - 1));
                }
                Err(error) => append_failure(&mut output, &reference, &error),
            }
            continue;
        }
        let (path, skill_root) = match source_path(&source.target, &base, skills.as_deref()) {
            Ok(path) => path,
            Err(error) => {
                append_failure(&mut output, &reference, &error);
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
            append_failure(
                &mut output,
                &reference,
                "nested source depth limit 3 exceeded",
            );
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
                    append_failure(&mut output, &reference, error);
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
                entry.insert(result)
            }
        };
        let body = match body {
            Ok(body) => body,
            Err(error) => {
                append_failure(&mut output, &reference, error);
                continue;
            }
        };
        let body = match selected_lines(body, &source.ranges) {
            Ok(body) => body,
            Err(error) => {
                append_failure(&mut output, &reference, &error);
                continue;
            }
        };
        append_body(&mut output, &reference, &body);
        bodies.push((output.len() - body.len() - 1, output.len() - 1));
        let nested_base = if managed {
            environment.cwd().clone()
        } else {
            path.parent().unwrap_or_else(|| base.clone())
        };
        for nested in references(&body) {
            pending.push_back((nested, nested_base.clone(), depth + 1));
        }
    }
    if bodies.is_empty() {
        return output;
    }
    // The source envelope preserves byte boundaries for Mekugi's existing output projection.
    // Framing and failures are not source bytes, even when they contain marker lookalikes.
    let ranges = bodies
        .into_iter()
        .map(|(start, end)| format!("{start}:{end}"))
        .collect::<Vec<_>>()
        .join(",");
    format!(
        "<mekugi-source-v1 bytes=\"{}\" bodies=\"{ranges}\">\n{output}\n</mekugi-source-v1>",
        output.len()
    )
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

fn append_body(output: &mut String, source: &str, body: &str) {
    output.push_str(&format!("\nSource {source:?}:\n{body}\n"));
}

fn append_failure(output: &mut String, source: &str, error: &str) {
    output.push_str(&format!(
        "\nSource {source:?}: CONTENT NOT ATTACHED: {error}\n"
    ));
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

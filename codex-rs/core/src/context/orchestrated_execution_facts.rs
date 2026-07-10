use sha1::Digest;
use sha1::Sha1;

use super::ContextualUserFragment;
use crate::shell::ShellType;
const MAX_VALUE_BYTES: usize = 120;
/// Bounded, redacted execution evidence retained after orchestrated phase compaction.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct OrchestratedExecutionFacts {
    facts: Vec<OrchestratedExecutionFact>,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct OrchestratedExecutionFact {
    generation: u64,
    fingerprint: String,
    cwd: String,
    outcome: OrchestratedExecutionOutcome,
    executions: u8,
    suppressed_retries: u8,
}
#[derive(Debug, Clone, PartialEq, Eq)]
enum OrchestratedExecutionOutcome {
    ExecutableUnavailable { executable: String },
    InvalidWorkingDirectory { path: String },
    MissingPath { path: String },
    ExitFailure { code: i32 },
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct OrchestratedCommandKey {
    fingerprint: String,
    cwd: String,
    executable: Option<String>,
    checked_path: Option<String>,
    shell_type: Option<ShellType>,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum OrchestratedCommandStart {
    Execute(u64),
    Suppress(OrchestratedExecutionFact),
}
/// Per-turn failure ledger shared by every model override in an orchestrated flow.
#[derive(Debug, Default)]
pub(crate) struct OrchestratedExecutionLedger {
    generation: u64,
    update_pending: bool,
    fact: Option<OrchestratedExecutionFact>,
}

impl OrchestratedCommandKey {
    pub(crate) fn new(command: &str, cwd: &str, shell_type: Option<ShellType>) -> Self {
        let digest = format!("{:x}", Sha1::digest(format!("{command}\0{cwd}").as_bytes()));
        let (executable, checked_path) = command_metadata(command);
        Self {
            fingerprint: digest[..16].to_string(),
            cwd: safe_path(cwd),
            executable,
            checked_path,
            shell_type,
        }
    }
}
impl OrchestratedExecutionFact {
    fn from_exit(key: OrchestratedCommandKey, code: i32, output: &[u8]) -> Self {
        let outcome = if code == 1
            && let Some(path) = key.checked_path.clone()
        {
            OrchestratedExecutionOutcome::MissingPath { path }
        } else if match key.shell_type {
            Some(ShellType::Zsh | ShellType::Bash | ShellType::Sh) | None => code == 127,
            Some(ShellType::PowerShell) => {
                code == 1 && String::from_utf8_lossy(output).contains("CommandNotFoundException")
            }
            Some(ShellType::Cmd) => {
                code == 1
                    && String::from_utf8_lossy(output)
                        .contains("is not recognized as an internal or external command")
            }
        } {
            key.executable.clone().map_or(
                OrchestratedExecutionOutcome::ExitFailure { code },
                |executable| OrchestratedExecutionOutcome::ExecutableUnavailable { executable },
            )
        } else {
            OrchestratedExecutionOutcome::ExitFailure { code }
        };
        Self::new(key, outcome)
    }
    fn new(key: OrchestratedCommandKey, outcome: OrchestratedExecutionOutcome) -> Self {
        Self {
            generation: 0,
            fingerprint: key.fingerprint,
            cwd: key.cwd,
            outcome,
            executions: 1,
            suppressed_retries: 0,
        }
    }
    pub(crate) fn progress_signature(&self) -> String {
        format!(
            "{}:{}:{}",
            self.fingerprint,
            self.outcome.label(),
            self.executions
        )
    }
    pub(crate) fn exit_code(&self) -> i32 {
        match self.outcome {
            OrchestratedExecutionOutcome::ExecutableUnavailable { .. } => 127,
            OrchestratedExecutionOutcome::ExitFailure { code } => code,
            OrchestratedExecutionOutcome::InvalidWorkingDirectory { .. }
            | OrchestratedExecutionOutcome::MissingPath { .. } => 1,
        }
    }
    pub(crate) fn suppression_diagnostic(&self) -> String {
        format!(
            "suppressed unchanged deterministic failure: {}",
            self.outcome.label()
        )
    }
}
impl OrchestratedExecutionOutcome {
    fn label(&self) -> &'static str {
        match self {
            Self::ExecutableUnavailable { .. } => "executableUnavailable",
            Self::InvalidWorkingDirectory { .. } => "invalidWorkingDirectory",
            Self::MissingPath { .. } => "missingPath",
            Self::ExitFailure { .. } => "exitFailure",
        }
    }
}
impl OrchestratedExecutionLedger {
    pub(crate) fn begin_command(
        &mut self,
        key: &OrchestratedCommandKey,
    ) -> OrchestratedCommandStart {
        if let Some(entry) = self.fact.as_mut().filter(|fact| {
            fact.generation == self.generation && fact.fingerprint == key.fingerprint
        }) {
            if !matches!(
                entry.outcome,
                OrchestratedExecutionOutcome::ExitFailure { .. }
            ) {
                entry.suppressed_retries = entry.suppressed_retries.saturating_add(1);
                self.update_pending = true;
                return OrchestratedCommandStart::Suppress(entry.clone());
            }
            self.generation = self.generation.saturating_add(1);
        } else {
            self.invalidate();
        }
        OrchestratedCommandStart::Execute(self.generation)
    }
    pub(crate) fn record_exit(
        &mut self,
        generation: u64,
        key: OrchestratedCommandKey,
        code: i32,
        output: &[u8],
    ) {
        self.record(
            generation,
            OrchestratedExecutionFact::from_exit(key, code, output),
        );
    }
    pub(crate) fn record_invalid_working_directory(
        &mut self,
        generation: u64,
        key: OrchestratedCommandKey,
    ) {
        let path = key.cwd.clone();
        let outcome = OrchestratedExecutionOutcome::InvalidWorkingDirectory { path };
        self.record(generation, OrchestratedExecutionFact::new(key, outcome));
    }
    pub(crate) fn invalidate(&mut self) {
        self.generation = self.generation.saturating_add(1);
        if self.fact.take().is_some() {
            self.update_pending = true;
        }
    }
    pub(crate) fn facts(&self) -> OrchestratedExecutionFacts {
        OrchestratedExecutionFacts {
            facts: self.fact.clone().into_iter().collect(),
        }
    }
    pub(crate) fn take_update(&mut self) -> Option<OrchestratedExecutionFacts> {
        if !std::mem::take(&mut self.update_pending) {
            return None;
        }
        Some(self.facts())
    }
    fn record(&mut self, generation: u64, mut fact: OrchestratedExecutionFact) {
        if let Some(entry) = self
            .fact
            .as_mut()
            .filter(|entry| entry.fingerprint == fact.fingerprint && entry.outcome == fact.outcome)
        {
            entry.generation = generation;
            entry.executions = entry.executions.saturating_add(1);
            self.update_pending = true;
            return;
        }
        fact.generation = generation;
        self.fact = Some(fact);
        self.update_pending = true;
    }
}
impl OrchestratedExecutionFacts {
    pub(crate) fn progress_signature(&self) -> String {
        self.facts
            .iter()
            .map(OrchestratedExecutionFact::progress_signature)
            .collect::<Vec<_>>()
            .join("|")
    }
}
impl ContextualUserFragment for OrchestratedExecutionFacts {
    fn role(&self) -> &'static str {
        "user"
    }

    fn markers(&self) -> (&'static str, &'static str) {
        Self::type_markers()
    }

    fn type_markers() -> (&'static str, &'static str) {
        (
            "<orchestrated_execution_facts>",
            "</orchestrated_execution_facts>",
        )
    }

    fn body(&self) -> String {
        let mut body = String::from(
            "\nLatest bounded redacted execution-fact snapshot for this user turn. It supersedes earlier snapshots in the same turn; an empty snapshot clears them. Raw commands and tool output were discarded.\n",
        );
        if self.facts.is_empty() {
            body.push_str("- none\n");
        }
        for fact in &self.facts {
            let mut line = format!(
                "- phase=worker tool=exec_command commandFingerprint={} effectiveCwd={} outcome={}",
                fact.fingerprint,
                quoted(&fact.cwd),
                fact.outcome.label()
            );
            match &fact.outcome {
                OrchestratedExecutionOutcome::ExecutableUnavailable { executable } => {
                    line.push_str(&format!(" executable={}", quoted(executable)));
                }
                OrchestratedExecutionOutcome::InvalidWorkingDirectory { path }
                | OrchestratedExecutionOutcome::MissingPath { path } => {
                    line.push_str(&format!(" path={}", quoted(path)));
                }
                OrchestratedExecutionOutcome::ExitFailure { code } => {
                    line.push_str(&format!(" code={code}"));
                }
            }
            line.push_str(&format!(
                " executions={} suppressedRetries={}\n",
                fact.executions, fact.suppressed_retries
            ));
            body.push_str(&line);
        }
        body
    }
}

fn command_metadata(command: &str) -> (Option<String>, Option<String>) {
    if command.contains("$(")
        || command
            .chars()
            .any(|character| matches!(character, '|' | ';' | '&' | '<' | '>' | '\n' | '\r' | '`'))
    {
        return (None, None);
    }
    let tokens = shlex::split(command).unwrap_or_default();
    let executable = tokens.first().and_then(|token| {
        let name = token.rsplit(['/', '\\']).next()?;
        (!token.contains('=')
            && !name.is_empty()
            && name
                .chars()
                .all(|character| character.is_ascii_alphanumeric() || "._+-".contains(character)))
        .then(|| safe_path(name))
    });
    let checked_path = match tokens.as_slice() {
        [executable, predicate, path]
            if executable == "test" && matches!(predicate.as_str(), "-e" | "-f" | "-d") =>
        {
            Some(safe_path(path))
        }
        _ => None,
    };
    (executable, checked_path)
}

fn safe_path(value: &str) -> String {
    if value.contains("://")
        || value.contains('@')
        || value
            .chars()
            .any(|character| matches!(character, '?' | '#' | '='))
    {
        return "<redacted>".to_string();
    }
    let value = value.replace(|character: char| character.is_control(), " ");
    let mut end = value.len().min(MAX_VALUE_BYTES);
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    if end == value.len() {
        value
    } else {
        format!("{}…", &value[..end])
    }
}

fn quoted(value: &str) -> String {
    serde_json::to_string(value).unwrap_or_else(|_| "\"<redacted>\"".to_string())
}

#[cfg(test)]
#[path = "orchestrated_execution_facts_tests.rs"]
mod tests;

use super::*;
use pretty_assertions::assert_eq;

fn record_exit(
    command: &str,
    cwd: &str,
    shell_type: ShellType,
    code: i32,
    output: &[u8],
) -> (OrchestratedExecutionLedger, OrchestratedCommandKey) {
    let key = OrchestratedCommandKey::new(command, cwd, Some(shell_type));
    let mut ledger = OrchestratedExecutionLedger::default();
    let OrchestratedCommandStart::Execute(generation) = ledger.begin_command(&key) else {
        panic!("first command should execute");
    };
    ledger.record_exit(generation, key.clone(), code, output);
    (ledger, key)
}
#[test]
fn renders_bounded_redacted_unavailable_executable() {
    let (mut ledger, key) = record_exit(
        "psql postgresql://user:secret@example.test/db",
        "/repo",
        ShellType::Bash,
        127,
        b"command not found",
    );
    let facts = ledger.facts();
    let rendered = facts.render();
    assert_eq!(facts.facts.len(), 1);
    assert!(rendered.contains("outcome=executableUnavailable"));
    assert!(rendered.contains("executable=\"psql\""));
    assert!(!rendered.contains("postgresql://"));
    assert!(!rendered.contains("secret"));
    assert!(rendered.len() < 4_000);
    let OrchestratedCommandStart::Suppress(fact) = ledger.begin_command(&key) else {
        panic!("unchanged deterministic failure should be suppressed");
    };
    assert_eq!(fact.exit_code(), 127);
    ledger.invalidate();
    assert!(ledger.facts().facts.is_empty());
    let cleared = ledger.take_update().expect("cleared snapshot").render();
    assert!(cleared.contains("- none"));
    assert!(matches!(
        ledger.begin_command(&key),
        OrchestratedCommandStart::Execute(_)
    ));
}
#[test]
fn retries_nondeterministic_exit_failure() {
    let (mut ledger, key) =
        record_exit("cargo test", "/repo", ShellType::Bash, 101, b"test failed");
    assert!(matches!(
        ledger.begin_command(&key),
        OrchestratedCommandStart::Execute(_)
    ));
}
#[test]
fn recognizes_windows_shell_unavailable_executable_output() {
    for (shell_type, output) in [
        (ShellType::PowerShell, "CommandNotFoundException"),
        (
            ShellType::Cmd,
            "is not recognized as an internal or external command",
        ),
    ] {
        let (ledger, _) = record_exit("missing-tool", "C:/repo", shell_type, 1, output.as_bytes());
        assert!(ledger.facts().render().contains("executableUnavailable"));
    }
}
#[test]
fn records_invalid_working_directory_without_tool_output() {
    let key = OrchestratedCommandKey::new("pwd", "/missing/repo", Some(ShellType::Bash));
    let mut ledger = OrchestratedExecutionLedger::default();
    let OrchestratedCommandStart::Execute(generation) = ledger.begin_command(&key) else {
        panic!("first command should execute");
    };
    ledger.record_invalid_working_directory(generation, key);

    let rendered = ledger.facts().render();
    assert!(rendered.contains("outcome=invalidWorkingDirectory"));
    assert!(rendered.contains("path=\"/missing/repo\""));
}
#[test]
fn bounds_values_and_rendered_body() {
    let cwd = format!("/{}", "x".repeat(200));
    let (ledger, _) = record_exit("cargo test", &cwd, ShellType::Bash, 2, b"failed");
    let facts = ledger.facts();

    assert_eq!(facts.facts.len(), 1);
    assert!(facts.render().len() < 4_000);
    assert!(facts.facts[0].cwd.len() <= 123);
}

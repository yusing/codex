use std::sync::Arc;

use codex_tools::ToolName;
use pretty_assertions::assert_eq;
use serde_json::json;
use tokio::sync::Mutex;

use super::*;
use crate::session::step_context::StepContext;
use crate::session::tests::make_session_and_context;
use crate::tools::context::ToolCallSource;
use crate::tools::handlers::ExecCommandHandler;
use crate::turn_diff_tracker::TurnDiffTracker;

#[test]
fn git_agent_classifier_allows_search_only() {
    for command in [
        "git-agent search --agent orchestrated explorer shell",
        "git agent search --agent orchestrated explorer shell",
    ] {
        let resolved = ["bash".to_string(), "-c".to_string(), command.to_string()];
        assert!(is_git_agent_search_command(&resolved));
    }

    for command in [
        "git-agent index .",
        "git-agent search --index orchestrated explorer shell",
        "git-agent search --remote https://example.com/repo.git orchestrated",
        "git-agent search foo > out",
        "git-agent search foo && touch out",
    ] {
        let resolved = ["bash".to_string(), "-c".to_string(), command.to_string()];
        assert!(!is_git_agent_search_command(&resolved));
    }
}

#[test]
fn simple_powershell_classifier_is_platform_independent() {
    for command in [
        "Get-ChildItem -Recurse C:\\repo",
        "Get-Content README.md",
        "rg orchestrated_role core/src",
        "git status",
        "git-agent search --agent orchestrated explorer shell",
    ] {
        assert!(is_safe_simple_powershell_command(command), "{command}");
    }

    for command in [
        "Get-ChildItem; Remove-Item README.md",
        "Get-Content $(Remove-Item README.md)",
        "rg --pre cat pattern",
        "git-agent search --remote https://example.com/repo.git query",
    ] {
        assert!(!is_safe_simple_powershell_command(command), "{command}");
    }
}

#[test]
fn payload_rejects_execution_widening_options() {
    for arguments in [
        json!({ "cmd": "ls", "shell": "./bash" }),
        json!({ "cmd": "ls", "login": true }),
        json!({ "cmd": "ls", "sandbox_permissions": "require_escalated" }),
        json!({ "cmd": "ls", "additional_permissions": { "network": true } }),
        json!({ "cmd": "ls", "justification": "please" }),
        json!({ "cmd": "ls", "prefix_rule": ["ls"] }),
        json!({ "cmd": "ls", "tty": true }),
    ] {
        let Value::Object(arguments) = arguments else {
            unreachable!();
        };
        let err = reject_forbidden_options(&ToolName::plain("exec_command"), &arguments)
            .expect_err("widening exec option should be rejected");
        assert_eq!(
            err.to_string(),
            "explorer role cannot use shell options that widen execution"
        );
    }
}

#[tokio::test]
async fn runtime_skips_pre_hook_for_invalid_or_widening_payloads() {
    let runtime = wrap_read_only_shell_runtime(
        Arc::new(ExecCommandHandler::default()),
        /*read_only_shell*/ true,
    );

    for arguments in [
        json!({ "cmd": "ls", "sandbox_permissions": "require_escalated" }),
        json!({ "cmd": "ls", "yield_time_ms": "invalid" }),
    ] {
        let (session, turn) = make_session_and_context().await;
        let turn = Arc::new(turn);
        let invocation = ToolInvocation {
            session: session.into(),
            step_context: StepContext::for_test(Arc::clone(&turn)),
            turn: Arc::clone(&turn),
            cancellation_token: tokio_util::sync::CancellationToken::new(),
            tracker: Arc::new(Mutex::new(TurnDiffTracker::new())),
            call_id: "explorer-pre-hook-invalid".to_string(),
            tool_name: ToolName::plain("exec_command"),
            source: ToolCallSource::Direct,
            payload: ToolPayload::Function {
                arguments: serde_json::to_string(&arguments).expect("serialize exec command args"),
            },
        };
        assert!(runtime.pre_tool_use_payload(&invocation).is_none());
    }
}

#[tokio::test]
async fn runtime_blocks_hook_rewrite_to_mutating_command() {
    let (session, turn) = make_session_and_context().await;
    let turn = Arc::new(turn);
    let invocation = ToolInvocation {
        session: session.into(),
        step_context: StepContext::for_test(Arc::clone(&turn)),
        turn,
        cancellation_token: tokio_util::sync::CancellationToken::new(),
        tracker: Arc::new(Mutex::new(TurnDiffTracker::new())),
        call_id: "explorer-hook-rewrite".to_string(),
        tool_name: ToolName::plain("exec_command"),
        source: ToolCallSource::Direct,
        payload: ToolPayload::Function {
            arguments: serde_json::to_string(&json!({ "cmd": "ls" }))
                .expect("serialize exec command args"),
        },
    };
    let runtime = wrap_read_only_shell_runtime(
        Arc::new(ExecCommandHandler::default()),
        /*read_only_shell*/ true,
    );

    let err =
        match runtime.with_updated_hook_input(invocation, json!({ "command": "echo hi > out" })) {
            Ok(_) => panic!("mutating hook rewrite should be blocked"),
            Err(err) => err,
        };

    assert_eq!(
        err.to_string(),
        "explorer role can only run read-only shell commands"
    );
}

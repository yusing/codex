use std::sync::Arc;

use codex_protocol::models::ShellCommandToolCallParams;
use codex_tools::ToolExecutor;
use codex_tools::ToolName;
use codex_tools::ToolSearchInfo;
use codex_tools::ToolSpec;
use futures::future::BoxFuture;
use serde_json::Map;
use serde_json::Value;

use crate::function_tool::FunctionCallError;
use crate::shell::ShellType;
use crate::tools::context::ToolInvocation;
use crate::tools::context::ToolOutput;
use crate::tools::context::ToolPayload;
use crate::tools::hook_names::HookToolName;
use crate::tools::registry::CoreToolRuntime;
use crate::tools::registry::PostToolUsePayload;
use crate::tools::registry::PreToolUsePayload;
use crate::tools::registry::ToolArgumentDiffConsumer;
use crate::tools::registry::ToolExposure;
use crate::tools::registry::ToolTelemetryTags;

use super::parse_arguments;
use super::resolve_tool_environment;
use super::unified_exec::ExecCommandArgs;
use super::unified_exec::ExecCommandEnvironmentArgs;
use super::unified_exec::get_command;
use super::unified_exec::shell_mode_for_environment;

type PlannedRuntime = Arc<dyn CoreToolRuntime>;

pub(crate) fn wrap_explorer_shell_runtime(
    handler: PlannedRuntime,
    explorer_shell: bool,
) -> PlannedRuntime {
    if explorer_shell {
        Arc::new(ExplorerShellRuntime { handler })
    } else {
        handler
    }
}

struct ExplorerShellRuntime {
    handler: PlannedRuntime,
}

impl ToolExecutor<ToolInvocation> for ExplorerShellRuntime {
    fn tool_name(&self) -> ToolName {
        self.handler.tool_name()
    }

    fn spec(&self) -> ToolSpec {
        explorer_shell_runtime_spec(self.handler.spec())
    }

    fn exposure(&self) -> ToolExposure {
        self.handler.exposure()
    }

    fn supports_parallel_tool_calls(&self) -> bool {
        self.handler.supports_parallel_tool_calls()
    }

    fn search_info(&self) -> Option<ToolSearchInfo> {
        self.handler.search_info()
    }

    fn handle(&self, invocation: ToolInvocation) -> codex_tools::ToolExecutorFuture<'_> {
        match validate_explorer_shell_invocation(&invocation) {
            Ok(_) => self.handler.handle(invocation),
            Err(err) => Box::pin(async move { Err(err) }),
        }
    }
}

impl CoreToolRuntime for ExplorerShellRuntime {
    fn matches_kind(&self, payload: &ToolPayload) -> bool {
        self.handler.matches_kind(payload)
    }

    fn waits_for_runtime_cancellation(&self) -> bool {
        self.handler.waits_for_runtime_cancellation()
    }

    fn telemetry_tags<'a>(
        &'a self,
        invocation: &'a ToolInvocation,
    ) -> BoxFuture<'a, ToolTelemetryTags> {
        self.handler.telemetry_tags(invocation)
    }

    fn post_tool_use_payload(
        &self,
        invocation: &ToolInvocation,
        result: &dyn ToolOutput,
    ) -> Option<PostToolUsePayload> {
        self.handler.post_tool_use_payload(invocation, result)
    }

    fn pre_tool_use_payload(&self, invocation: &ToolInvocation) -> Option<PreToolUsePayload> {
        let command = validate_explorer_shell_invocation(invocation).ok()?;
        Some(PreToolUsePayload {
            tool_name: HookToolName::bash(),
            tool_input: serde_json::json!({ "command": command }),
        })
    }

    fn with_updated_hook_input(
        &self,
        invocation: ToolInvocation,
        updated_input: Value,
    ) -> Result<ToolInvocation, FunctionCallError> {
        let invocation = self
            .handler
            .with_updated_hook_input(invocation, updated_input)?;
        validate_explorer_shell_invocation(&invocation)?;
        Ok(invocation)
    }

    fn create_diff_consumer(&self) -> Option<Box<dyn ToolArgumentDiffConsumer>> {
        self.handler.create_diff_consumer()
    }
}

fn explorer_shell_runtime_spec(spec: ToolSpec) -> ToolSpec {
    let ToolSpec::Function(mut tool) = spec else {
        return spec;
    };

    if matches!(tool.name.as_str(), "exec_command" | "shell_command")
        && let Some(properties) = tool.parameters.properties.as_mut()
    {
        for forbidden in [
            "shell",
            "tty",
            "login",
            "sandbox_permissions",
            "additional_permissions",
            "justification",
            "prefix_rule",
        ] {
            properties.remove(forbidden);
        }
    }

    ToolSpec::Function(tool)
}

fn validate_explorer_shell_invocation(
    invocation: &ToolInvocation,
) -> Result<String, FunctionCallError> {
    let arguments = explorer_shell_arguments(&invocation.payload)?;
    reject_forbidden_options(&invocation.tool_name, &arguments)?;
    let resolved = resolve_explorer_shell_command(invocation)?;

    if codex_shell_command::is_safe_command::is_known_safe_command(&resolved.argv)
        || is_git_agent_search_command(&resolved.argv)
        || (resolved.shell_type == ShellType::PowerShell
            && is_safe_simple_powershell_command(&resolved.command))
    {
        Ok(resolved.command)
    } else {
        Err(FunctionCallError::RespondToModel(
            "explorer role can only run read-only shell commands".to_string(),
        ))
    }
}

fn explorer_shell_arguments(
    payload: &ToolPayload,
) -> Result<Map<String, Value>, FunctionCallError> {
    let ToolPayload::Function { arguments } = payload else {
        return Err(FunctionCallError::RespondToModel(
            "explorer role requires a function shell payload".to_string(),
        ));
    };
    let arguments: Value = serde_json::from_str(arguments).map_err(|_| {
        FunctionCallError::RespondToModel("explorer role received invalid shell arguments".into())
    })?;
    let Value::Object(arguments) = arguments else {
        return Err(FunctionCallError::RespondToModel(
            "explorer role received invalid shell arguments".to_string(),
        ));
    };
    Ok(arguments)
}

fn reject_forbidden_options(
    tool_name: &ToolName,
    arguments: &Map<String, Value>,
) -> Result<(), FunctionCallError> {
    let forbidden = match (tool_name.namespace.as_deref(), tool_name.name.as_str()) {
        (None, "exec_command") => [
            "shell",
            "sandbox_permissions",
            "additional_permissions",
            "justification",
            "prefix_rule",
        ]
        .as_slice(),
        (None, "shell_command") => [
            "sandbox_permissions",
            "additional_permissions",
            "justification",
            "prefix_rule",
            "shell",
        ]
        .as_slice(),
        _ => {
            return Err(FunctionCallError::RespondToModel(format!(
                "explorer role cannot run `{tool_name}`"
            )));
        }
    };
    let has_forbidden = forbidden
        .iter()
        .any(|field| arguments.get(*field).is_some_and(|value| !value.is_null()))
        || arguments
            .get("login")
            .and_then(Value::as_bool)
            .unwrap_or(false)
        || arguments
            .get("tty")
            .and_then(Value::as_bool)
            .unwrap_or(false);
    if has_forbidden {
        Err(FunctionCallError::RespondToModel(
            "explorer role cannot use shell options that widen execution".to_string(),
        ))
    } else {
        Ok(())
    }
}

struct ResolvedExplorerShellCommand {
    command: String,
    argv: Vec<String>,
    shell_type: ShellType,
}

fn resolve_explorer_shell_command(
    invocation: &ToolInvocation,
) -> Result<ResolvedExplorerShellCommand, FunctionCallError> {
    let ToolPayload::Function { arguments } = &invocation.payload else {
        unreachable!("validated function payload");
    };
    match (
        invocation.tool_name.namespace.as_deref(),
        invocation.tool_name.name.as_str(),
    ) {
        (None, "exec_command") => {
            let environment_args: ExecCommandEnvironmentArgs = parse_arguments(arguments)?;
            let Some(turn_environment) = resolve_tool_environment(
                &invocation.step_context.environments,
                environment_args.environment_id.as_deref(),
            )?
            else {
                return Err(FunctionCallError::RespondToModel(
                    "unified exec is unavailable in this session".to_string(),
                ));
            };
            let args: ExecCommandArgs = parse_arguments(arguments)?;
            let shell_mode = shell_mode_for_environment(
                &invocation.turn.unified_exec_shell_mode,
                turn_environment.environment.as_ref(),
            );
            let shell = turn_environment
                .shell
                .clone()
                .map(Arc::new)
                .unwrap_or_else(|| invocation.session.user_shell());
            let command = args.cmd.clone();
            let resolved = get_command(&args, shell, &shell_mode, /*allow_login_shell*/ false)
                .map_err(FunctionCallError::RespondToModel)?;
            Ok(ResolvedExplorerShellCommand {
                command,
                argv: resolved.command,
                shell_type: resolved.shell_type,
            })
        }
        (None, "shell_command") => {
            let params: ShellCommandToolCallParams = parse_arguments(arguments)?;
            let Some(turn_environment) = invocation.step_context.environments.primary() else {
                return Err(FunctionCallError::RespondToModel(
                    "shell is unavailable in this session".to_string(),
                ));
            };
            let shell = turn_environment.shell.as_ref().map_or_else(
                || invocation.session.user_shell(),
                |shell| Arc::new(shell.clone()),
            );
            let command = params.command;
            let argv = shell.derive_exec_args(&command, /*use_login_shell*/ false);
            Ok(ResolvedExplorerShellCommand {
                command,
                argv,
                shell_type: shell.shell_type,
            })
        }
        _ => unreachable!("validated explorer shell tool name"),
    }
}

fn is_safe_simple_powershell_command(command: &str) -> bool {
    if command.is_empty()
        || !command.chars().all(|character| {
            character.is_ascii_alphanumeric()
                || character.is_ascii_whitespace()
                || matches!(
                    character,
                    '.' | '_' | '-' | '/' | '\\' | ':' | '*' | '?' | '[' | ']' | ','
                )
        })
    {
        return false;
    }

    let words = command
        .split_ascii_whitespace()
        .map(ToString::to_string)
        .collect::<Vec<_>>();
    codex_shell_command::is_safe_command::is_safe_powershell_words(&words)
        || git_agent_search_args(&words).is_some_and(git_agent_search_args_are_safe)
}

fn is_git_agent_search_command(command: &[String]) -> bool {
    codex_shell_command::bash::parse_shell_lc_plain_commands(command).is_some_and(|commands| {
        !commands.is_empty()
            && commands.iter().all(|command| {
                git_agent_search_args(command).is_some_and(git_agent_search_args_are_safe)
            })
    })
}

fn git_agent_search_args(command: &[String]) -> Option<&[String]> {
    match command {
        [program, subcommand, args @ ..] if program == "git-agent" && subcommand == "search" => {
            Some(args)
        }
        [program, agent, subcommand, args @ ..]
            if program == "git" && agent == "agent" && subcommand == "search" =>
        {
            Some(args)
        }
        _ => None,
    }
}

fn git_agent_search_args_are_safe(args: &[String]) -> bool {
    let mut index = 0;
    while index < args.len() {
        let arg = args[index].as_str();
        if !arg.starts_with('-') {
            index += 1;
            continue;
        }

        if matches!(arg, "--agent" | "--code" | "--no-tests") {
            index += 1;
            continue;
        }

        if matches!(arg, "--rev" | "--scope" | "--min-relatedness" | "--limit") {
            if index + 1 >= args.len() {
                return false;
            }
            index += 2;
            continue;
        }

        if arg.starts_with("--rev=")
            || arg.starts_with("--scope=")
            || arg.starts_with("--min-relatedness=")
            || arg.starts_with("--limit=")
        {
            index += 1;
            continue;
        }

        return false;
    }
    true
}

#[cfg(test)]
#[path = "explorer_shell_tests.rs"]
mod tests;

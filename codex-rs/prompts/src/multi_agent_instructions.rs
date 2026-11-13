//! Assembles multi-agent role instructions from selected text and runtime capabilities.
//! The segment owns rendering and attribution; consumers select and capture its inputs.

use crate::without_update_plan_instructions;
use codex_context_fragments::ContextualUserFragment;
use codex_protocol::models::ContentItem;
use codex_protocol::models::ContentItemKind;
use codex_protocol::models::input_text;

const DEFAULT_MULTI_AGENT_V2_MODEL_OVERRIDE_USAGE_HINT_TEXT: &str = "Full-history forks (`fork_turns` omitted or `\"all\"`) inherit the parent model and reasoning effort and do not accept overrides. Only set `model` or `reasoning_effort` when explicitly requested by the user, applicable `AGENTS.md` instructions, or skill instructions; when doing so, set `fork_turns` to `\"none\"`.";
const DEFAULT_MULTI_AGENT_V2_WAIT_AGENT_USAGE_HINT_TEXT: &str = "Call wait_agent alone and let functions.exec finish. Omit timeout_ms unless there is a real deadline; the outcome arrives after the logical checkpoint.";
const DEFAULT_MULTI_AGENT_V2_SHARED_USAGE_HINT_TEXT: &str = "Call collaboration tools inside functions.exec using their tools.* declarations. Agents share the filesystem; each has its own working directory. To attach retained changes to your parent, include explicit mchanges source references in your ordinary final answer.";
const SUBAGENT_AUTHORITY_HINT: &str = "As a delegate, an explicit delegation request in your authenticated parent assignment carries the parent's authority within that assignment, subject to higher-priority limits. Ask the user directly for clarification with functions.request_user_input_async.";

/// Multi-agent role text and the captured capabilities used to render its context segment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MultiAgentRoleInstructions {
    /// Complete configured instructions, emitted verbatim without catalog markers.
    Configured(String),
    /// Selected catalog or bundled text, composed with captured runtime guidance.
    Composed {
        base: String,
        subagent: bool,
        marked: bool,
        omit_update_plan_instructions: bool,
        max_concurrency: Option<usize>,
        wait_agent_enabled: bool,
        expose_model_overrides: bool,
    },
}

impl MultiAgentRoleInstructions {
    /// Content classification shared by role producers and inherited-history filtering.
    pub const KIND: &str = "multi_agent.role_instructions";
}

impl ContextualUserFragment for MultiAgentRoleInstructions {
    fn content_kind(&self) -> ContentItemKind {
        ContentItemKind(Self::KIND.to_string())
    }

    fn role(&self) -> &'static str {
        "developer"
    }

    fn markers(&self) -> (&'static str, &'static str) {
        match self {
            Self::Composed { marked: true, .. } => Self::type_markers(),
            Self::Configured(_) | Self::Composed { marked: false, .. } => ("", ""),
        }
    }

    fn type_markers() -> (&'static str, &'static str) {
        ("<multi_agent_role>", "</multi_agent_role>")
    }

    fn body(&self) -> ContentItem {
        input_text(match self {
            Self::Configured(text) => text.clone(),
            Self::Composed {
                base,
                subagent,
                omit_update_plan_instructions,
                max_concurrency,
                wait_agent_enabled,
                expose_model_overrides,
                ..
            } => {
                let base = if *omit_update_plan_instructions {
                    without_update_plan_instructions(base)
                } else {
                    base.clone()
                };
                let wait_agent_guidance = if *wait_agent_enabled {
                    format!("{DEFAULT_MULTI_AGENT_V2_WAIT_AGENT_USAGE_HINT_TEXT}\n\n")
                } else {
                    String::new()
                };
                let shared = DEFAULT_MULTI_AGENT_V2_SHARED_USAGE_HINT_TEXT;
                let mut text = format!("{base}\n{shared}\n{wait_agent_guidance}");
                if let Some(max_concurrency) = max_concurrency {
                    text.push_str(&format!("There are {max_concurrency} available concurrency slots, meaning that up to {max_concurrency} agents can be active at once, including you."));
                }
                if *expose_model_overrides {
                    text.push_str("\n\n");
                    text.push_str(DEFAULT_MULTI_AGENT_V2_MODEL_OVERRIDE_USAGE_HINT_TEXT);
                }
                if *subagent {
                    text.push_str("\n\n");
                    text.push_str(SUBAGENT_AUTHORITY_HINT);
                }
                text
            }
        })
    }
}

#[cfg(test)]
#[path = "multi_agent_instructions_tests.rs"]
mod tests;

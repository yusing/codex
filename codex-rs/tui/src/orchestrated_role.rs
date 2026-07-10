use ratatui::prelude::*;
use ratatui::style::Stylize;

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) enum Attribution {
    #[default]
    Unattributed,
    OrchestratedRole(String),
}

pub(crate) fn packet_role_prefix(text: &str) -> Option<(&'static str, &str)> {
    [
        ("task-contract", "task-contract:"),
        ("explorer", "explorer:"),
        ("worker-plan", "worker-plan:"),
        ("plan-review", "plan-review:"),
        ("result-review", "result-review:"),
        ("worker", "worker:"),
        ("orchestrator", "orc:"),
    ]
    .iter()
    .find_map(|(role, prefix)| {
        text.strip_prefix(prefix)
            .map(|remainder| (*role, remainder))
    })
}

pub(crate) fn role_label(role: &str) -> Span<'static> {
    match role {
        "task-contract" => "Task Contract".cyan().bold(),
        "explorer" => "Explorer".cyan().bold(),
        "worker-plan" => "Worker Plan".cyan().bold(),
        "plan-review" => "Plan Review".cyan().bold(),
        "result-review" => "Result Review".magenta().bold(),
        "worker" => "Worker".cyan().bold(),
        "orchestrator" => "Orchestrator".magenta().bold(),
        role => role.to_string().cyan().bold(),
    }
}

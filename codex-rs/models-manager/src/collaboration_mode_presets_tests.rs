use super::*;
use pretty_assertions::assert_eq;

#[test]
fn preset_names_use_mode_display_names() {
    assert_eq!(plan_preset().name, ModeKind::Plan.display_name());
    assert_eq!(
        orchestrated_preset().name,
        ModeKind::Orchestrated.display_name()
    );
    assert_eq!(default_preset().name, ModeKind::Default.display_name());
    assert_eq!(plan_preset().model, None);
    assert_eq!(
        plan_preset().reasoning_effort,
        Some(Some(ReasoningEffort::Medium))
    );
    assert_eq!(orchestrated_preset().model, None);
    assert_eq!(orchestrated_preset().reasoning_effort, None);
    assert_eq!(default_preset().model, None);
    assert_eq!(default_preset().reasoning_effort, None);
}

#[test]
fn builtin_presets_include_orchestrated_between_plan_and_default() {
    let presets = builtin_collaboration_mode_presets();
    let modes = presets.iter().map(|preset| preset.mode).collect::<Vec<_>>();
    assert_eq!(
        modes,
        vec![
            Some(ModeKind::Plan),
            Some(ModeKind::Orchestrated),
            Some(ModeKind::Default),
        ]
    );
}

#[test]
fn orchestrated_mode_instructions_describe_role_defaults() {
    let instructions = orchestrated_preset()
        .developer_instructions
        .expect("orchestrated preset should include instructions")
        .expect("orchestrated instructions should be set");

    assert!(instructions.contains("internal explorer and worker role passes"));
    assert!(instructions.contains("`explorer:` and `worker:` notes"));
    assert!(
        instructions.contains("Do not spawn subagents just because Orchestrated mode is active")
    );
    assert!(instructions.contains("`orc:`"));
}

#[test]
fn default_mode_instructions_replace_mode_names_placeholder() {
    let default_instructions = default_preset()
        .developer_instructions
        .expect("default preset should include instructions")
        .expect("default instructions should be set");

    assert!(!default_instructions.contains("{{KNOWN_MODE_NAMES}}"));

    let known_mode_names = format_mode_names(&TUI_VISIBLE_COLLABORATION_MODES);
    let expected_snippet = format!("Known mode names are {known_mode_names}.");
    assert!(default_instructions.contains(&expected_snippet));

    assert!(default_instructions.contains(
        "Use the `request_user_input` tool only when it is listed in the available tools"
    ));
    assert!(
        default_instructions.contains("ask the user directly with a concise plain-text question")
    );
}

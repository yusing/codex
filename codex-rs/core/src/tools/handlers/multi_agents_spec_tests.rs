use super::*;
use codex_tools::JsonSchemaPrimitiveType;
use codex_tools::JsonSchemaType;
use pretty_assertions::assert_eq;
use serde_json::json;

#[test]
fn spawn_agent_tool_v2_requires_task_name() {
    let tool = create_spawn_agent_tool_v2(
        SpawnAgentToolOptions {
            agent_type_description: "role help".to_string(),
            expose_agent_type: true,
            hide_agent_type_model_reasoning: false,
            expose_spawn_agent_model_overrides: true,
            usage_hint_text: None,
            ..Default::default()
        },
        /*description_override*/ None,
    );

    let ToolSpec::Function(ResponsesApiTool {
        description,
        parameters,
        output_schema,
        ..
    }) = tool
    else {
        panic!("spawn_agent should be a function tool");
    };
    assert_eq!(
        parameters.schema_type,
        Some(JsonSchemaType::Single(JsonSchemaPrimitiveType::Object))
    );
    let properties = parameters
        .properties
        .as_ref()
        .expect("spawn_agent should use object params");
    assert!(description.contains("return its confirmed thread ID and canonical task path"));
    assert!(description.contains("Children can use tools and delegate"));
    assert!(description.contains("rejects model, effort and preload overrides"));
    assert!(!description.contains("max_concurrent_threads_per_session"));
    assert!(description.contains(SPAWN_AGENT_INHERITED_MODEL_GUIDANCE));
    assert!(properties.contains_key("task_name"));
    assert!(properties.contains_key("message"));
    assert_eq!(
        properties
            .get("message")
            .and_then(|schema| schema.encrypted),
        Some(true)
    );
    assert!(properties.contains_key("fork_turns"));
    assert!(properties.contains_key("cwd"));
    assert_eq!(
        properties["preload"].required.as_deref(),
        Some(["sources".to_string()].as_slice())
    );
    assert!(!properties.contains_key("items"));
    assert!(!properties.contains_key("fork_context"));
    assert_eq!(
        properties
            .get("model")
            .and_then(|schema| schema.description.as_deref()),
        Some(SPAWN_AGENT_MODEL_OVERRIDE_DESCRIPTION)
    );
    assert_eq!(
        properties
            .get("reasoning_effort")
            .and_then(|schema| schema.description.as_deref()),
        Some(
            "Reasoning effort override for fork_turns=none; otherwise inherited. Set only when requested by the user or applicable instructions."
        )
    );
    assert!(!properties.contains_key("service_tier"));
    assert_eq!(
        parameters.required.as_deref(),
        Some(["task_name".to_string(), "message".to_string()].as_slice())
    );
    assert_eq!(
        output_schema.expect("spawn_agent output schema").to_value()["required"],
        json!(["task_name", "thread_id", "nickname"])
    );
}

#[test]
fn spawn_agent_catalog_description_preserves_generated_context() {
    let options = SpawnAgentToolOptions {
        agent_type_description: "Available agent roles: explorer".to_string(),
        expose_spawn_agent_model_overrides: true,
        usage_hint_text: Some("Local usage hint.".to_string()),
        ..Default::default()
    };
    let ToolSpec::Function(default_tool) =
        create_spawn_agent_tool_v2(options.clone(), /*description_override*/ None)
    else {
        panic!("spawn_agent should be a function tool");
    };
    let ToolSpec::Function(mut configured_tool) =
        create_spawn_agent_tool_v2(options, Some("Catalog spawning guidance."))
    else {
        panic!("spawn_agent should be a function tool");
    };
    assert!(
        configured_tool
            .description
            .contains("Catalog spawning guidance.")
    );
    assert!(
        configured_tool
            .description
            .contains(SPAWN_AGENT_INHERITED_MODEL_GUIDANCE)
    );
    assert!(configured_tool.description.ends_with("Local usage hint."));
    assert!(
        !configured_tool
            .description
            .contains("Spawn an attributed child for a task")
    );
    configured_tool.description = default_tool.description.clone();
    assert_eq!(configured_tool, default_tool);
}

#[test]
fn spawn_agent_tool_v1_keeps_legacy_fork_context_field() {
    let tool = create_spawn_agent_tool_v1(SpawnAgentToolOptions {
        agent_type_description: "role help".to_string(),
        expose_agent_type: true,
        hide_agent_type_model_reasoning: false,
        expose_spawn_agent_model_overrides: true,
        usage_hint_text: None,
        ..Default::default()
    });

    let ToolSpec::Namespace(namespace) = tool else {
        panic!("spawn_agent v1 should be a namespace tool");
    };
    assert_eq!(namespace.name, MULTI_AGENT_V1_NAMESPACE);
    let Some(ResponsesApiNamespaceTool::Function(ResponsesApiTool { parameters, .. })) =
        namespace.tools.first()
    else {
        panic!("spawn_agent should be a namespace function tool");
    };
    assert_eq!(
        parameters.schema_type.clone(),
        Some(JsonSchemaType::Single(JsonSchemaPrimitiveType::Object))
    );
    let properties = parameters
        .properties
        .as_ref()
        .expect("spawn_agent should use object params");

    assert!(properties.contains_key("fork_context"));
    assert!(!properties.contains_key("fork_turns"));
    assert_eq!(
        properties.get("agent_type"),
        Some(&JsonSchema::string(Some(format!(
            "{SPAWN_AGENT_TYPE_OVERRIDE_DESCRIPTION_V1}\nrole help"
        ))))
    );
    assert_eq!(
        properties
            .get("message")
            .and_then(|schema| schema.encrypted),
        None
    );
    assert_eq!(
        properties
            .get("model")
            .and_then(|schema| schema.description.as_deref()),
        Some(SPAWN_AGENT_MODEL_OVERRIDE_DESCRIPTION)
    );
    assert!(!properties.contains_key("service_tier"));
}

#[test_case::test_case(false; "inline_catalog")]
#[test_case::test_case(true; "context_catalog")]
fn spawn_agent_tool_keeps_model_controls_when_spawn_metadata_is_hidden(
    model_catalog_in_context: bool,
) {
    let tool = create_spawn_agent_tool_v2(
        SpawnAgentToolOptions {
            agent_type_description: "role help".to_string(),
            expose_agent_type: false,
            hide_agent_type_model_reasoning: true,
            expose_spawn_agent_model_overrides: true,
            model_catalog_in_context,
            usage_hint_text: None,
            ..Default::default()
        },
        /*description_override*/ None,
    );

    let ToolSpec::Function(ResponsesApiTool {
        description,
        parameters,
        ..
    }) = tool
    else {
        panic!("spawn_agent should be a function tool");
    };
    let properties = parameters
        .properties
        .as_ref()
        .expect("spawn_agent should use object params");

    assert!(!properties.contains_key("agent_type"));
    assert!(properties.contains_key("model"));
    assert!(properties.contains_key("reasoning_effort"));
    assert!(!properties.contains_key("service_tier"));
    assert!(!description.contains(SPAWN_AGENT_INHERITED_MODEL_GUIDANCE));
    assert_eq!(
        description.contains(SPAWN_AGENT_INHERITED_MODEL_GUIDANCE_V2),
        model_catalog_in_context
    );
}

#[test]
fn spawn_agent_tool_hides_model_controls_without_override_exposure() {
    let tool = create_spawn_agent_tool_v2(
        SpawnAgentToolOptions {
            agent_type_description: "role help".to_string(),
            expose_agent_type: false,
            hide_agent_type_model_reasoning: true,
            expose_spawn_agent_model_overrides: false,
            usage_hint_text: None,
            ..Default::default()
        },
        Some(""),
    );

    let ToolSpec::Function(ResponsesApiTool {
        description,
        parameters,
        ..
    }) = tool
    else {
        panic!("spawn_agent should be a function tool");
    };
    let properties = parameters
        .properties
        .as_ref()
        .expect("spawn_agent should use object params");

    for property in ["agent_type", "model", "reasoning_effort", "service_tier"] {
        assert!(!properties.contains_key(property));
    }
    assert!(!description.contains(SPAWN_AGENT_INHERITED_MODEL_GUIDANCE));
    assert!(!description.contains("Available model overrides"));
}

#[test]
fn send_message_tool_requires_message_and_has_no_output_schema() {
    let ToolSpec::Function(ResponsesApiTool {
        parameters,
        output_schema,
        ..
    }) = create_send_message_tool()
    else {
        panic!("send_message should be a function tool");
    };
    assert_eq!(
        parameters.schema_type,
        Some(JsonSchemaType::Single(JsonSchemaPrimitiveType::Object))
    );
    let properties = parameters
        .properties
        .as_ref()
        .expect("send_message should use object params");
    assert!(properties.contains_key("target"));
    assert!(properties.contains_key("message"));
    assert_eq!(
        properties["btw"].schema_type,
        Some(JsonSchemaType::Single(JsonSchemaPrimitiveType::Boolean))
    );
    assert_eq!(
        properties
            .get("message")
            .and_then(|schema| schema.encrypted),
        Some(true)
    );
    assert!(!properties.contains_key("interrupt"));
    assert!(!properties.contains_key("items"));
    assert_eq!(
        properties
            .get("target")
            .and_then(|schema| schema.description.as_deref()),
        Some("Canonical task path, unambiguous relative name, or exact thread ID.")
    );
    assert_eq!(
        parameters.required.as_deref(),
        Some(["target".to_string(), "message".to_string()].as_slice())
    );
    assert_eq!(output_schema, None);
}

#[test]
fn followup_task_tool_requires_message_and_has_no_output_schema() {
    let ToolSpec::Function(ResponsesApiTool {
        name,
        description,
        parameters,
        output_schema,
        ..
    }) = create_followup_task_tool()
    else {
        panic!("followup_task should be a function tool");
    };
    assert_eq!(name, "followup_task");
    assert_eq!(
        description,
        "Send a follow-up task to an existing non-root target agent and trigger a turn if it is idle. If the target is already running, deliver the task promptly at message boundaries while sampling, or after the pending tool call completes."
    );
    assert_eq!(
        parameters.schema_type,
        Some(JsonSchemaType::Single(JsonSchemaPrimitiveType::Object))
    );
    let properties = parameters
        .properties
        .as_ref()
        .expect("followup_task should use object params");
    assert!(properties.contains_key("target"));
    assert!(properties.contains_key("message"));
    assert_eq!(
        properties
            .get("message")
            .and_then(|schema| schema.encrypted),
        Some(true)
    );
    assert!(!properties.contains_key("items"));
    assert_eq!(
        parameters.required.as_deref(),
        Some(["target".to_string(), "message".to_string()].as_slice())
    );
    assert_eq!(output_schema, None);
}

#[test]
fn wait_agent_tool_v2_describes_checkpoint_and_deferred_delivery() {
    let ToolSpec::Function(ResponsesApiTool {
        description,
        parameters,
        output_schema,
        ..
    }) = create_wait_agent_tool_v2(WaitAgentTimeoutOptions {
        default_timeout_ms: 30_000,
        min_timeout_ms: 10_000,
        max_timeout_ms: 3_600_000,
    })
    else {
        panic!("wait_agent should be a function tool");
    };
    assert_eq!(
        parameters.schema_type,
        Some(JsonSchemaType::Single(JsonSchemaPrimitiveType::Object))
    );
    let properties = parameters
        .properties
        .as_ref()
        .expect("wait_agent should use object params");
    assert!(!properties.contains_key("targets"));
    assert!(properties.contains_key("timeout_ms"));
    for field in ["wait_mode", "compact", "reload", "yield_time_ms"] {
        assert!(properties.contains_key(field));
    }
    assert!(description.contains("stays pending"));
    assert!(description.contains("same call"));
    assert!(description.contains("queued composer input does not"));
    assert_eq!(
        properties
            .get("timeout_ms")
            .and_then(|schema| schema.description.as_deref()),
        Some(
            "Omit for an indefinite completion wait; set only for a real deadline. Values below 10000 clamp; values above 3600000 reject. The deadline starts at registration."
        )
    );
    assert_eq!(parameters.required.as_ref(), None);
    assert_eq!(output_schema, None);
}

#[test]
fn list_agents_tool_includes_path_prefix_for_retained_members() {
    let ToolSpec::Function(ResponsesApiTool {
        parameters,
        output_schema,
        ..
    }) = create_list_agents_tool()
    else {
        panic!("list_agents should be a function tool");
    };
    assert_eq!(
        parameters.schema_type,
        Some(JsonSchemaType::Single(JsonSchemaPrimitiveType::Object))
    );
    let properties = parameters
        .properties
        .as_ref()
        .expect("list_agents should use object params");
    assert!(properties.contains_key("path_prefix"));
    assert!(properties.contains_key("session_id"));
    assert_eq!(
        properties
            .get("path_prefix")
            .and_then(|schema| schema.description.as_deref()),
        Some("Task-path prefix filter without a trailing slash. Omit to list the admitted tree.")
    );
    assert_eq!(output_schema, None);
}

#[test]
fn inspect_agent_tool_requires_a_target_and_preserves_unavailable_observations() {
    let ToolSpec::Function(ResponsesApiTool {
        parameters,
        description,
        ..
    }) = create_inspect_agent_tool()
    else {
        panic!("inspect_agent should be a function tool");
    };
    assert_eq!(
        parameters.required.as_deref(),
        Some(["target".to_string()].as_slice())
    );
    assert!(description.contains("without starting work"));
    assert!(description.contains("unavailable observations are null"));
}

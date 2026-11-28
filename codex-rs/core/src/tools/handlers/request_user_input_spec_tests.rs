use super::*;
use pretty_assertions::assert_eq;
use serde_json::json;
use test_case::test_case;

#[test]
fn request_user_input_tool_includes_shared_questions_schema() {
    let ToolSpec::Function(tool) = create_request_user_input_tool("Ask".into()) else {
        panic!("function tool")
    };
    let schema = serde_json::to_value(tool.parameters).unwrap();
    let question = &schema["properties"]["questions"]["items"];
    assert_eq!(question["required"], json!(["question"]));
    assert_eq!(question["properties"].get("id"), None);
    assert_eq!(
        question["properties"]["options"]["items"]["required"],
        json!(["label"])
    );
    assert_eq!(
        question["properties"]["options"]["items"]["properties"]["recommended"]["type"],
        "boolean"
    );
    assert_eq!(
        question["properties"]["options"]["items"]["properties"]["preview"]["type"],
        "string"
    );
    let preview_description =
        question["properties"]["options"]["items"]["properties"]["preview"]["description"]
            .as_str()
            .expect("preview description");
    assert!(preview_description.contains("picker"));
    assert!(preview_description.contains("appearance or interaction choices"));
    assert!(preview_description.contains("same concrete example under each option"));
}

#[test_case(json!({"questions": []}), "questions must not be empty"; "empty")]
#[test_case(json!({"questions": [{"question":" "}]}), "questions must contain non-empty question text"; "blank")]
#[test_case(json!({"questions": [{"question":"Which?", "options":[]}]}), "options must contain at least one non-empty label"; "empty options")]
#[test_case(json!({"questions": [{"question":"Which?", "options":[{"label":" "}]}]}), "options must contain at least one non-empty label"; "blank label")]
#[test_case(json!({"questions": [{"question":"Which?", "options":[{"label":"A","recommended":true},{"label":"B","recommended":true}]}]}), "at most one option may be recommended per question"; "two recommendations")]
fn normalize_request_user_input_rejects_invalid(value: serde_json::Value, expected: &str) {
    let args = serde_json::from_value(value).unwrap();
    assert_eq!(
        normalize_request_user_input_tool_args(args).unwrap_err(),
        expected
    );
}

#[test]
fn normalize_request_user_input_accepts_absent_options_and_preserves_order() {
    let value = json!({"questions":[
        {"question":"Describe it"},
        {"question":"Which?", "options":[{"label":"A"},{"label":"B","recommended":true,"preview":"diff"}]}
    ]});
    let args: RequestUserInputToolArgs = serde_json::from_value(value).unwrap();
    assert_eq!(
        normalize_request_user_input_tool_args(args.clone()),
        Ok(args.clone())
    );
    let question = args.questions[1].clone().with_id("q2".into());
    assert!(question.is_other);
    let options = question.options.unwrap();
    assert_eq!(options[0].label, "A");
    assert_eq!(options[1].label, "B");
    assert!(options[1].recommended);
}

#[test]
fn request_user_input_rejects_authored_ids_and_host_only_fields() {
    for field in ["id", "isSecret", "isOther"] {
        let mut question = json!({"question":"Which?"});
        question[field] = if field == "id" {
            json!("authored")
        } else {
            json!(true)
        };
        assert!(
            serde_json::from_value::<RequestUserInputToolArgs>(json!({"questions":[question]}))
                .is_err()
        );
    }
}

#[test]
fn request_user_input_unavailable_message_and_description() {
    assert_eq!(
        request_user_input_unavailable_message(ModeKind::Plan, &[ModeKind::Plan]),
        None
    );
    assert_eq!(
        request_user_input_unavailable_message(ModeKind::Default, &[ModeKind::Plan]),
        Some("request_user_input is unavailable in Default mode".into())
    );
    let description = request_user_input_tool_description(&[ModeKind::Default, ModeKind::Plan]);
    assert!(description.contains("Default or Plan mode"));
    assert!(description.contains("recommended: true"));
    assert!(!description.contains("(Recommended)"));
}

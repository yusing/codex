use super::unsupported_code_mode_warning;
use codex_models_manager::model_info::model_info_from_slug;
use codex_protocol::openai_models::CatalogModel;
use codex_protocol::openai_models::ModelOrigin;
use codex_protocol::openai_models::ToolMode;
use pretty_assertions::assert_eq;

const MODEL_SLUG: &str = "test-model";

fn known_model_info() -> CatalogModel {
    CatalogModel {
        model: model_info_from_slug(MODEL_SLUG).into(),
        origin: ModelOrigin::Provider,
    }
}

#[test]
fn warns_when_code_mode_is_enabled_without_model_selector() {
    assert_eq!(
        unsupported_code_mode_warning(&known_model_info()),
        Some(format!(
            "Mekugi uses JavaScript execution, but model `{MODEL_SLUG}` does not advertise Code Mode support. This may degrade model performance. Select a model whose metadata advertises Code Mode."
        ))
    );
}

#[test]
fn does_not_warn_when_model_has_tool_mode_selector() {
    for tool_mode in [ToolMode::CodeMode, ToolMode::CodeModeOnly] {
        let mut model_info = known_model_info();
        model_info.tool_mode = Some(tool_mode);
        assert_eq!(unsupported_code_mode_warning(&model_info), None);
    }
}

#[test]
fn fallback_metadata_only_uses_existing_warning() {
    assert_eq!(
        unsupported_code_mode_warning(&CatalogModel {
            model: model_info_from_slug(MODEL_SLUG).into(),
            origin: ModelOrigin::Synthetic
        }),
        None
    );
}

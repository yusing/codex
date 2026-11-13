use codex_protocol::openai_models::CatalogModel;
use codex_protocol::openai_models::ModelOrigin;
use codex_protocol::openai_models::ToolMode;

pub(super) fn unsupported_code_mode_warning(model_info: &CatalogModel) -> Option<String> {
    if matches!(
        model_info.tool_mode,
        Some(ToolMode::CodeMode | ToolMode::CodeModeOnly)
    ) || model_info.origin == ModelOrigin::Synthetic
    {
        return None;
    }

    let model = &model_info.slug;
    Some(format!(
        "Mekugi uses JavaScript execution, but model `{model}` does not advertise Code Mode support. This may degrade model performance. Select a model whose metadata advertises Code Mode."
    ))
}

#[cfg(test)]
#[path = "code_mode_warning_tests.rs"]
mod tests;

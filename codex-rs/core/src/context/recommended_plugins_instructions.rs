use super::ContextualUserFragment;
use codex_protocol::models::ContentItem;
use codex_protocol::models::ContentItemKind;
use codex_protocol::models::input_text;

// Retain marker recognition for historical context; new turns use explicit plugin discovery.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct RecommendedPluginsInstructions;

impl ContextualUserFragment for RecommendedPluginsInstructions {
    fn content_kind(&self) -> ContentItemKind {
        ContentItemKind("plugins.recommendations".to_string())
    }

    fn role(&self) -> &'static str {
        "developer"
    }

    fn markers(&self) -> (&'static str, &'static str) {
        Self::type_markers()
    }

    fn type_markers() -> (&'static str, &'static str) {
        ("<recommended_plugins>", "</recommended_plugins>")
    }

    fn body(&self) -> ContentItem {
        input_text(String::new())
    }
}

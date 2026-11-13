use schemars::JsonSchema;
use serde::Deserialize;
use serde::Serialize;
use ts_rs::TS;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(rename_all = "snake_case")]
pub enum ContextResetStatus {
    Accepted,
    Started,
    Completed,
    Failed,
    Canceled,
}

/// Exact reset correlation and retained outcome. A reset covers only generations captured
/// before acquisition starts; a later request remains pending independently.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ContextReset {
    pub thread_id: String,
    pub turn_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_id: Option<String>,
    pub generation: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reset_id: Option<String>,
    pub status: ContextResetStatus,
    pub inline: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub diagnostic: Option<String>,
}

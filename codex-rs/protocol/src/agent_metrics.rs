//! Passive context facts shared by observation and claim-frozen delivery.

use schemars::JsonSchema;
use serde::Deserialize;
use serde::Serialize;
use ts_rs::TS;

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct AgentMetrics {
    #[ts(type = "number | null")]
    pub context_tokens: Option<i64>,
    #[ts(type = "number | null")]
    pub context_window: Option<i64>,
    #[ts(type = "number | null")]
    pub completed_compactions: Option<u64>,
}

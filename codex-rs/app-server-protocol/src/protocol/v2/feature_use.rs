use crate::{JsonSchema, TS};
pub use codex_protocol::feature_use::{FeatureUseCounts, FeatureUseEvent, FeatureUseFact};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema, TS)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[ts(export_to = "v2/")]
pub struct FeatureUseReadParams {
    pub threads: Vec<String>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct FeatureUseReadResponse {
    pub threads: BTreeMap<String, FeatureUseCounts>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema, TS)]
#[ts(export_to = "v2/")]
pub struct FeatureUseRecordResponse {}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct FeatureUseUnavailableNotification {
    pub thread_id: String,
    pub message: String,
}

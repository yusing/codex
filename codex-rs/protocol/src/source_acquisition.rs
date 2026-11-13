//! Frozen source acquisition outcomes for live and retained presentation.
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use ts_rs::TS;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema, TS)]
#[ts(export_to = "v2/")]
pub struct SourceAcquisition {
    pub id: String,
    pub purpose: SourceAcquisitionPurpose,
    pub sources: Vec<AcquiredSource>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(rename_all = "snake_case")]
#[ts(export_to = "v2/")]
pub enum SourceAcquisitionPurpose { Preload, Reload }

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema, TS)]
#[ts(export_to = "v2/")]
pub struct AcquiredSource {
    pub reference: String,
    #[serde(flatten)]
    pub target: SourceTarget,
    pub success: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub diagnostic: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(tag = "kind", rename_all = "snake_case")]
#[ts(export_to = "v2/")]
pub enum SourceTarget {
    File { path: String },
    Skill { name: String },
    Changes { selection: Vec<String> },
    Selection,
}

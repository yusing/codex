//! Application resources requested by native admission and completion authority.

use crate::JsonSchema;
use crate::TS;
use serde::Deserialize;
use serde::Serialize;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct AgentPrepareFinalCaptureParams {
    pub root_thread_id: String,
    pub thread_id: String,
    pub work_id: String,
    pub recipient_thread_id: String,
    pub recipient_cwd: Option<std::path::PathBuf>,
    /// Explicit captured-change references selected by the native successful-final owner.
    pub sources: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct AgentPrepareFinalCaptureResponse {
    /// Frozen capture bodies and acquisition diagnostics; null when none were selected.
    pub capture: Option<AgentFrozenFinalCapture>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct AgentFrozenFinalCapture {
    pub resource_ref: String,
    pub full_text: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct AgentCaptureCursor {
    pub key: String,
    #[ts(type = "number")]
    pub through_chunk: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct AgentCaptureInterval {
    pub key: String,
    pub change_id: String,
    #[ts(type = "number")]
    pub record_order: u64,
    pub legacy_record_key: Option<String>,
    #[ts(type = "number")]
    pub first_chunk: u64,
    #[ts(type = "number")]
    pub last_chunk: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct AgentCaptureCoverage {
    pub status: String,
    pub diagnostics: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct AgentProgressActor {
    pub thread_id: String,
    pub work_id: String,
    pub task_name: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct AgentFrozenProgressCapture {
    pub actor: AgentProgressActor,
    pub intervals: Vec<AgentCaptureInterval>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(tag = "status", rename_all = "snake_case")]
#[ts(export_to = "v2/")]
pub enum AgentProgressSummary {
    Available { text: String },
    Unavailable { reason: String },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct AgentPreparedProgress {
    pub actor: AgentProgressActor,
    pub intervals: Vec<AgentCaptureInterval>,
    pub summary: AgentProgressSummary,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct AgentFreezeProgressParams {
    pub root_thread_id: String,
    pub caller_thread_id: String,
    pub caller_work_id: String,
    pub wait_id: String,
    pub batch_id: String,
    pub recipient_context_id: String,
    pub selected: Vec<AgentProgressActor>,
    pub seen: Vec<AgentCaptureCursor>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct AgentFreezeProgressResponse {
    pub snapshot_ref: String,
    #[ts(type = "number")]
    pub publication_revision: u64,
    pub agents: Vec<AgentFrozenProgressCapture>,
    pub coverage: AgentCaptureCoverage,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct AgentPrepareProgressOutputParams {
    pub root_thread_id: String,
    pub caller_thread_id: String,
    pub caller_work_id: String,
    pub wait_id: String,
    pub batch_id: String,
    pub recipient_context_id: String,
    pub snapshot_ref: String,
    pub agents: Vec<AgentPreparedProgress>,
    pub coverage: AgentCaptureCoverage,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct AgentPrepareProgressOutputResponse {
    pub text: String,
    pub recovery_ref: Option<String>,
    /// Only intervals represented in full in text, not merely available through recovery.
    pub delivered_intervals: Vec<AgentCaptureInterval>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct AgentProjectFinalCaptureParams {
    pub root_thread_id: String,
    pub thread_id: String,
    pub work_id: String,
    pub recipient_thread_id: String,
    pub recipient_cwd: Option<std::path::PathBuf>,
    pub recipient_context_id: String,
    pub resource_ref: String,
    pub seen: Vec<AgentCaptureCursor>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct AgentProjectFinalCaptureResponse {
    pub text: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct AgentPrepareHostAdmissionParams {
    pub root_thread_id: String,
    pub parent_thread_id: String,
    pub thread_id: String,
    pub local_cwd: Option<std::path::PathBuf>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct AgentPrepareHostAdmissionResponse {
    pub thread_id: String,
}

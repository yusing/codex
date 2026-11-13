//! Frozen capture resources and context-local progress delivery evidence.

use codex_protocol::ThreadId;
use serde::Deserialize;
use serde::Serialize;
use uuid::Uuid;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FrozenFinalCapture {
    pub resource_ref: String,
    pub full_text: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CaptureCursor {
    pub key: String,
    pub through_chunk: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CaptureInterval {
    pub key: String,
    pub change_id: String,
    pub record_order: u64,
    pub legacy_record_key: Option<String>,
    pub first_chunk: u64,
    pub last_chunk: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CaptureCoverage {
    /// Resource-owner vocabulary; native does not reinterpret coverage policy.
    pub status: String,
    pub diagnostics: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProgressActor {
    pub thread_id: ThreadId,
    pub work_id: Uuid,
    pub task_name: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FrozenAgentCapture {
    pub actor: ProgressActor,
    pub intervals: Vec<CaptureInterval>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum SummaryResult {
    Available { text: String },
    Unavailable { reason: String },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PreparedProgressAgent {
    pub actor: ProgressActor,
    pub intervals: Vec<CaptureInterval>,
    pub summary: SummaryResult,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FrozenProgress {
    pub snapshot_ref: String,
    pub publication_revision: u64,
    pub agents: Vec<FrozenAgentCapture>,
    pub coverage: CaptureCoverage,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PreparedProgressOutput {
    pub text: String,
    pub recovery_ref: Option<String>,
    /// Only evidence represented in full in text, never recovery-only evidence.
    pub delivered_intervals: Vec<CaptureInterval>,
}

#[derive(Clone, Debug)]
pub struct ProgressBatch {
    pub wait_id: Uuid,
    pub batch_id: Uuid,
    pub recipient_context_id: Uuid,
    pub frozen: Option<FrozenProgress>,
    pub output: Option<PreparedProgressOutput>,
}

#[derive(Clone, Debug)]
pub struct SummaryProbe {
    pub caller: crate::coordination::WorkStamp,
    pub wait_id: Uuid,
    pub batch_id: Uuid,
    pub target: FrozenAgentCapture,
}

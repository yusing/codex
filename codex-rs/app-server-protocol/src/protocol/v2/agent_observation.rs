//! Passive facts about an admitted native tree. Missing observations are null.

use crate::JsonSchema;
use crate::TS;
pub use codex_protocol::agent_metrics::AgentMetrics;
use serde::Deserialize;
use serde::Serialize;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct AgentWorkObservation {
    pub work_id: String,
    #[ts(type = "number")]
    pub revision: i64,
    pub disposition: AgentWorkDisposition,
    pub terminal_delivery_id: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(rename_all = "snake_case")]
#[ts(export_to = "v2/")]
pub enum AgentWorkDisposition {
    PendingStart,
    Running,
    Waiting,
    Paused,
    Uncertain,
    Completed,
    Failed,
    Interrupted,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct AgentWaitObservation {
    pub wait_id: String,
    pub mode: AgentWaitMode,
    pub phase: AgentWaitPhase,
    pub reason: Option<codex_protocol::agent_coordination::CoordinationTurnWaitReason>,
    #[ts(type = "number | null")]
    pub timeout_ms: Option<i64>,
    #[ts(type = "number | null")]
    pub deadline_unix_ms: Option<i64>,
    pub selected: Vec<AgentWaitTarget>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct AgentWaitTarget {
    pub thread_id: String,
    pub work_id: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(rename_all = "snake_case")]
#[ts(export_to = "v2/")]
pub enum AgentWaitMode {
    Any,
    All,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(rename_all = "snake_case")]
#[ts(export_to = "v2/")]
pub enum AgentWaitPhase {
    Registered,
    Claimed,
    Dispatched,
    Accepted,
    Consumed,
    Cancelled,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(rename_all = "snake_case")]
#[ts(export_to = "v2/")]
pub enum AgentRuntimeStatus {
    PendingInit,
    Running,
    Interrupted,
    Completed,
    Errored,
    Shutdown,
    NotFound,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct AgentObservation {
    pub thread_id: String,
    pub root_thread_id: String,
    pub parent_thread_id: Option<String>,
    pub path: Option<String>,
    pub nickname: Option<String>,
    pub role: Option<String>,
    pub backend: Option<String>,
    pub attachable: Option<bool>,
    pub loaded: bool,
    pub status: Option<AgentRuntimeStatus>,
    pub cwd: Option<std::path::PathBuf>,
    pub model: Option<String>,
    pub reasoning_effort: Option<codex_protocol::openai_models::ReasoningEffort>,
    pub service_tier: Option<String>,
    pub metrics: AgentMetrics,
    pub work: Option<AgentWorkObservation>,
    pub wait: Option<AgentWaitObservation>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct AgentTreeObservation {
    pub root_thread_id: String,
    /// Transient invalidation revision, not an atomic durable work revision.
    #[ts(type = "number")]
    pub observation_revision: u64,
    pub agents: Vec<AgentObservation>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct AgentObserveParams {
    pub thread_id: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct AgentObserveTreeParams {
    pub root_thread_id: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct AgentObserveResponse {
    pub agent: AgentObservation,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct AgentObserveTreeResponse {
    pub tree: AgentTreeObservation,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct AgentTreeChangedNotification {
    pub root_thread_id: String,
    #[ts(type = "number")]
    pub observation_revision: u64,
}

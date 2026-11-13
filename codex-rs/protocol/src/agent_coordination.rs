//! Immutable presentation facts. Native work and receipt rows remain authoritative.

use crate::AgentPath;
use crate::ThreadId;
use schemars::JsonSchema;
use serde::Deserialize;
use serde::Serialize;
use ts_rs::TS;
use uuid::Uuid;

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct CoordinationProjection {
    pub deliveries: Vec<CoordinationTerminalDelivery>,
    pub wait: Option<CoordinationWaitSettlement>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub interrupt: Option<CoordinationInterruptRequest>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct CoordinationInterruptRequest {
    pub operation_id: String,
    pub target: CoordinationTarget,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct CoordinationTarget {
    pub thread_id: ThreadId,
    pub task_name: AgentPath,
    #[schemars(with = "String")]
    pub work_id: Uuid,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub enum CoordinationDisposition {
    Completed,
    Failed,
    Interrupted,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct CoordinationTerminalDelivery {
    #[schemars(with = "String")]
    pub delivery_id: Uuid,
    pub source: CoordinationTarget,
    pub disposition: CoordinationDisposition,
    pub recipient: AgentPath,
    /// Exact displayed payload, including the frozen capture projection, without attribution headers.
    pub body: String,
    pub capture_resource_ref: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct CoordinationWaitSettlement {
    #[schemars(with = "String")]
    pub wait_id: Uuid,
    pub caller_thread_id: ThreadId,
    #[schemars(with = "String")]
    pub caller_work_id: Uuid,
    pub origin: CoordinationWaitOrigin,
    pub mode: CoordinationWaitMode,
    pub selected: Vec<CoordinationTarget>,
    #[schemars(with = "Option<String>")]
    pub claim_id: Option<Uuid>,
    pub result: CoordinationWaitResult,
    pub messages: Vec<CoordinationWaitMessage>,
    pub outcomes: Vec<CoordinationWaitOutcome>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(tag = "kind", rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub enum CoordinationWaitOrigin {
    #[serde(rename_all = "camelCase")]
    Cell {
        turn_id: String,
        item_id: String,
        window_id: String,
        cell_id: String,
    },
    #[serde(rename_all = "camelCase")]
    Turn {
        turn_id: String,
        reason: CoordinationTurnWaitReason,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub enum CoordinationTurnWaitReason {
    DescendantRecovery,
    ProviderCapacity,
    ProviderUsageLimit,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub enum CoordinationWaitMode {
    Any,
    All,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub enum CoordinationWaitWake {
    Events,
    Deadline,
    Progress,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(tag = "kind", rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub enum CoordinationWaitResult {
    /// The frozen result is exposed to the caller, not proof of provider consumption.
    Released {
        wake: CoordinationWaitWake,
    },
    Cancelled {
        cause: CoordinationWaitCancellation,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(tag = "kind", rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub enum CoordinationWaitCancellation {
    Cancelled,
    ExecutionFailed,
    #[serde(rename_all = "camelCase")]
    InputSuperseded {
        #[schemars(with = "String")]
        delivery_id: Uuid,
    },
    #[serde(rename_all = "camelCase")]
    WorkTerminated {
        #[schemars(with = "String")]
        delivery_id: Uuid,
    },
    #[serde(rename_all = "camelCase")]
    ShellOutput {
        turn_id: String,
    },
    DispatchRejected {
        diagnostic: Option<String>,
    },
    NotDispatched {
        diagnostic: Option<String>,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct CoordinationWaitMessage {
    #[schemars(with = "String")]
    pub delivery_id: Uuid,
    pub sender_thread_id: Option<ThreadId>,
    pub sender_task_name: Option<AgentPath>,
    pub recipient: AgentPath,
    /// Exact message payload and frozen sources, without native attribution headers.
    pub body: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct CoordinationWaitOutcome {
    pub target: CoordinationTarget,
    pub disposition: CoordinationWaitDisposition,
}

/// A stopped wait target need not have authored a terminal final.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub enum CoordinationWaitDisposition {
    Completed,
    Failed,
    Interrupted,
    Paused,
}

/// A semantic history event for a settlement without an associated response item.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct AgentCoordinationEvent {
    pub thread_id: ThreadId,
    pub turn_id: String,
    pub projection: CoordinationProjection,
}

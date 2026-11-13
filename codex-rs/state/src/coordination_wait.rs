//! Wait tickets reference durable work and deliveries, never copies of history.

use crate::coordination::DeliveryId;
use crate::coordination::WorkDisposition;
use crate::coordination::WorkStamp;
use codex_protocol::agent_metrics::AgentMetrics;
use codex_protocol::protocol::ReloadSelection;
use serde::Deserialize;
use serde::Serialize;
use std::collections::BTreeMap;
use uuid::Uuid;

pub type WaitId = Uuid;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum WaitMode {
    #[default]
    Any,
    All,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct WaitTimeout {
    pub requested_ms: i64,
    pub effective_ms: i64,
}

/// Immutable registration options. The absolute deadline is captured by the
/// transaction, not recomputed when another invocation joins the same cell.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct WaitOptions {
    pub mode: WaitMode,
    pub timeout: Option<WaitTimeout>,
    pub progress_interval_ms: Option<u64>,
    pub compact: bool,
    /// None reuses retained selection; Some with no sources clears it.
    pub reload: Option<ReloadSelection>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum WaitResetState {
    NotRequested,
    Pending,
    Running,
    Completed,
    Failed { diagnostic: String },
}

impl WaitResetState {
    pub fn blocks_delivery(&self) -> bool {
        matches!(self, Self::Pending | Self::Running)
    }
}

/// Identity from the native cell broker, including the original history window.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct WaitCell {
    pub turn_id: String,
    pub item_id: String,
    pub window_id: String,
    pub cell_id: String,
}

/// Only an authored cell requires cell-success evidence. Exhausted native turns
/// use their accepted turn/work binding and cannot invent tool-call identities.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum WaitOrigin {
    Cell {
        cell: WaitCell,
    },
    Turn {
        turn_id: String,
        reason: TurnWaitReason,
    },
}

impl WaitOrigin {
    pub fn cell(&self) -> Option<&WaitCell> {
        match self {
            Self::Cell { cell } => Some(cell),
            Self::Turn { .. } => None,
        }
    }

    pub fn turn_id(&self) -> &str {
        match self {
            Self::Cell { cell } => &cell.turn_id,
            Self::Turn { turn_id, .. } => turn_id,
        }
    }

    pub fn reason(&self) -> Option<TurnWaitReason> {
        match self {
            Self::Cell { .. } => None,
            Self::Turn { reason, .. } => Some(*reason),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TurnWaitReason {
    DescendantRecovery,
    ProviderCapacity,
    ProviderUsageLimit,
}

impl TurnWaitReason {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::DescendantRecovery => "descendant_recovery",
            Self::ProviderCapacity => "provider_capacity",
            Self::ProviderUsageLimit => "provider_usage_limit",
        }
    }

    pub(crate) fn parse(value: &str) -> anyhow::Result<Self> {
        Ok(match value {
            "descendant_recovery" => Self::DescendantRecovery,
            "provider_capacity" => Self::ProviderCapacity,
            "provider_usage_limit" => Self::ProviderUsageLimit,
            _ => anyhow::bail!("invalid persisted turn-wait reason: {value}"),
        })
    }

    pub fn is_provider(self) -> bool {
        matches!(self, Self::ProviderCapacity | Self::ProviderUsageLimit)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WaitWake {
    Events,
    Deadline,
    Progress,
}

/// Supplied only by the live owner using its original monotonic deadline.
/// Reading a retained wall-clock deadline never grants cold wake authority.
#[derive(Clone, Copy, Debug, Default)]
pub struct WaitReadiness {
    pub deadline_due: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WaitPhase {
    Registered,
    Claimed,
    Dispatched,
    Accepted,
    Consumed,
    Cancelled,
}

impl WaitPhase {
    pub(crate) fn parse(value: &str) -> anyhow::Result<Self> {
        Ok(match value {
            "registered" => Self::Registered,
            "claimed" => Self::Claimed,
            "dispatched" => Self::Dispatched,
            "accepted" => Self::Accepted,
            "consumed" => Self::Consumed,
            "cancelled" => Self::Cancelled,
            _ => anyhow::bail!("invalid persisted wait phase: {value}"),
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WaitOutcome {
    pub stamp: WorkStamp,
    pub disposition: WorkDisposition,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WaitClaim {
    pub id: Uuid,
    pub wake: WaitWake,
    /// Only notifications addressed to this caller. Transitive terminal facts
    /// do not transfer ownership of an intermediate coordinator's mailbox.
    pub delivery_ids: Vec<DeliveryId>,
    pub outcomes: Vec<WaitOutcome>,
    /// Selected actor IDs, frozen before dispatch. None means not acquired yet;
    /// unavailable individual values remain null, never fabricated zeros.
    pub metrics: Option<BTreeMap<String, AgentMetrics>>,
}

/// Known execution refusal, authenticated against the exact retained dispatch.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum WaitDispatchRefusal {
    Rejected { diagnostic: Option<String> },
    NotDispatched { diagnostic: Option<String> },
}

/// Only irreducible closure evidence. Work/origin/mode/claim remain on the wait;
/// causal delivery IDs retain exact input or terminal ownership across replay.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum WaitCancellation {
    Cancelled,
    ExecutionFailed,
    ShellOutput { turn_id: String },
    InputSuperseded { delivery_id: Uuid },
    WorkTerminated { delivery_id: Uuid },
    DispatchRefused { refusal: WaitDispatchRefusal },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WaitTicket {
    pub id: WaitId,
    pub caller: WorkStamp,
    pub runtime_id: Uuid,
    pub origin: WaitOrigin,
    pub options: WaitOptions,
    pub deadline_unix_ms: Option<i64>,
    pub reset: WaitResetState,
    pub phase: WaitPhase,
    pub selected: Vec<WorkStamp>,
    pub claim: Option<WaitClaim>,
    pub dispatch_id: Option<Uuid>,
    pub accepted_receipt: Option<String>,
    pub cancellation: Option<WaitCancellation>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WaitError {
    EmptySelection,
    ConflictingCell,
    UnknownWait,
    InvalidPhase,
    ColdRuntime,
    InvalidOptions,
}

impl std::fmt::Display for WaitError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::EmptySelection => "no eligible descendant work or addressed message",
            Self::ConflictingCell => "wait belongs to a different native cell or options",
            Self::UnknownWait => "wait is not retained for this caller work",
            Self::InvalidPhase => "wait transition is not allowed in its current phase",
            Self::ColdRuntime => "retained wait is observation-only in this runtime",
            Self::InvalidOptions => "invalid wait options",
        })
    }
}

impl std::error::Error for WaitError {}

//! Durable coordination facts, independent of thread history and execution bindings.
//! The native agent registry authenticates membership; these rows do not register actors.

use codex_protocol::ThreadId;
use uuid::Uuid;

pub type DeliveryId = Uuid;

/// The accepted initial input's delivery ID is the work ID. Turns, steering,
/// checkpoints and auxiliary resets never allocate a replacement identity.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct WorkStamp {
    pub root_id: ThreadId,
    pub actor_id: ThreadId,
    pub work_id: DeliveryId,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WorkDisposition {
    PendingStart,
    Running,
    Waiting,
    Paused,
    Uncertain,
    Completed,
    Failed,
    Interrupted,
}

impl WorkDisposition {
    pub fn is_terminal(self) -> bool {
        matches!(self, Self::Completed | Self::Failed | Self::Interrupted)
    }

    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::PendingStart => "pending_start",
            Self::Running => "running",
            Self::Waiting => "waiting",
            Self::Paused => "paused",
            Self::Uncertain => "uncertain",
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::Interrupted => "interrupted",
        }
    }

    pub(crate) fn parse(value: &str) -> anyhow::Result<Self> {
        Ok(match value {
            "pending_start" => Self::PendingStart,
            "running" => Self::Running,
            "waiting" => Self::Waiting,
            "paused" => Self::Paused,
            "uncertain" => Self::Uncertain,
            "completed" => Self::Completed,
            "failed" => Self::Failed,
            "interrupted" => Self::Interrupted,
            _ => anyhow::bail!("invalid persisted work disposition: {value}"),
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorkRecord {
    pub stamp: WorkStamp,
    pub runtime_id: Uuid,
    pub revision: i64,
    pub disposition: WorkDisposition,
    pub terminal_delivery_id: Option<DeliveryId>,
}

/// Payload-free facts read under one SQLite snapshot for admitted actors.
#[derive(Clone, Debug)]
pub struct ActorWorkObservation {
    pub actor_id: ThreadId,
    pub work: Option<WorkRecord>,
    pub wait: Option<WaitObservation>,
}

#[derive(Clone, Debug)]
pub struct WaitObservation {
    pub id: Uuid,
    pub mode: crate::coordination_wait::WaitMode,
    pub phase: crate::coordination_wait::WaitPhase,
    pub reason: Option<crate::coordination_wait::TurnWaitReason>,
    pub timeout_ms: Option<i64>,
    pub deadline_unix_ms: Option<i64>,
    pub selected: Vec<WorkStamp>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CompletionCandidate {
    pub stamp: WorkStamp,
    pub delivery_id: DeliveryId,
    pub recipient_id: ThreadId,
    pub turn_id: String,
    pub disposition: WorkDisposition,
    pub detail: Option<String>,
    pub answers: Vec<String>,
    pub capture_ready: bool,
    pub capture: Option<String>,
}

impl WorkRecord {
    /// Observation is passive. A new runtime cannot infer surviving execution,
    /// dispatch a continuation or rearm a timer from an old runtime's row.
    pub fn observed_in(mut self, runtime_id: Uuid) -> Self {
        if self.runtime_id != runtime_id
            && !self.disposition.is_terminal()
            && self.disposition != WorkDisposition::Uncertain
        {
            self.disposition = WorkDisposition::Paused;
        }
        self
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DeliveryKind {
    Message,
    Terminal,
}

impl DeliveryKind {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Message => "message",
            Self::Terminal => "terminal",
        }
    }
}

/// Fully prepared immutable content, including source/capture diagnostics.
/// Publication stores it once; selections and claims retain only delivery IDs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PublishDelivery {
    pub id: DeliveryId,
    pub root_id: ThreadId,
    /// None denotes human or native harness input, not a synthetic agent sender.
    pub sender_id: Option<ThreadId>,
    pub recipient_id: ThreadId,
    pub work: Option<WorkStamp>,
    pub kind: DeliveryKind,
    pub payload: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DeliveryRecord {
    pub sequence: i64,
    pub publication: PublishDelivery,
    pub claim_id: Option<Uuid>,
    /// Persisted before invoking execution. Without an acceptance receipt its
    /// outcome is uncertain; it cannot be released and blindly dispatched again.
    pub dispatch_id: Option<Uuid>,
    /// Native input/dispatch acceptance is not model consumption.
    pub accepted_receipt: Option<String>,
    pub refused_reason: Option<String>,
    pub consumed: bool,
}

/// Domain conflicts remain distinguishable from database failures via downcast.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CoordinationError {
    StaleWork,
    ActiveWork,
    UnknownWork,
    InvalidTransition,
    InvalidDelivery,
    DeliveryConflict,
    UnknownDelivery,
    InvalidPage,
}

impl std::fmt::Display for CoordinationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::StaleWork => "work identity or revision changed",
            Self::ActiveWork => "existing work must settle before accepting another assignment",
            Self::UnknownWork => "work is not retained",
            Self::InvalidTransition => "invalid work transition",
            Self::InvalidDelivery => "delivery does not match the work or recipient",
            Self::DeliveryConflict => "delivery identity, claim or acceptance changed",
            Self::UnknownDelivery => "delivery is not retained for this recipient",
            Self::InvalidPage => "delivery page limit must be between 1 and 256",
        })
    }
}

impl std::error::Error for CoordinationError {}

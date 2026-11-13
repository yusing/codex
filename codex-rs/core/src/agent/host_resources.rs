//! Application-owned resources borrowed by the native coordination owner.

use super::control::work_lifecycle::FinalCapturePreparer;
use codex_protocol::ThreadId;
use codex_protocol::error::Result as CodexResult;
use codex_state::coordination::WorkStamp;
use codex_state::coordination_progress::*;
use futures::future::BoxFuture;
use std::path::PathBuf;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

pub struct FreezeProgressRequest {
    pub caller: WorkStamp,
    pub wait_id: Uuid,
    pub batch_id: Uuid,
    pub recipient_context_id: Uuid,
    pub selected: Vec<ProgressActor>,
    pub seen: Vec<CaptureCursor>,
}

pub struct PrepareProgressOutputRequest {
    pub caller: WorkStamp,
    pub wait_id: Uuid,
    pub batch_id: Uuid,
    pub recipient_context_id: Uuid,
    pub snapshot_ref: String,
    pub agents: Vec<PreparedProgressAgent>,
    pub coverage: CaptureCoverage,
}

pub struct ProjectFinalCaptureRequest {
    pub work: WorkStamp,
    pub recipient: ThreadId,
    pub recipient_cwd: Option<PathBuf>,
    pub recipient_context_id: Uuid,
    pub resource_ref: String,
    pub seen: Vec<CaptureCursor>,
}

pub struct ProjectedFinalCapture {
    pub text: String,
}

pub type ProgressFreezer = Arc<
    dyn Fn(
            FreezeProgressRequest,
            CancellationToken,
        ) -> BoxFuture<'static, CodexResult<FrozenProgress>>
        + Send
        + Sync,
>;
pub type ProgressOutputPreparer = Arc<
    dyn Fn(
            PrepareProgressOutputRequest,
            CancellationToken,
        ) -> BoxFuture<'static, CodexResult<PreparedProgressOutput>>
        + Send
        + Sync,
>;
pub type FinalCaptureProjector = Arc<
    dyn Fn(
            ProjectFinalCaptureRequest,
            CancellationToken,
        ) -> BoxFuture<'static, CodexResult<ProjectedFinalCapture>>
        + Send
        + Sync,
>;

pub struct ChildHostAdmissionRequest {
    pub root_id: ThreadId,
    pub parent_id: ThreadId,
    pub thread_id: ThreadId,
    pub local_cwd: Option<PathBuf>,
}

pub type ChildHostAdmission = Arc<
    dyn Fn(ChildHostAdmissionRequest, CancellationToken) -> BoxFuture<'static, CodexResult<()>>
        + Send
        + Sync,
>;

/// One live host owns these callbacks; renderer subscriptions cannot replace it.
pub struct AgentHostCallbacks {
    /// Actual transport owner, supplied by the host rather than request arguments.
    pub host_connection_id: u64,
    /// Presentation-only facts for actors without a loaded native Session.
    pub publish_coordination: Arc<
        dyn Fn(codex_protocol::agent_coordination::AgentCoordinationEvent) -> BoxFuture<'static, ()>
            + Send
            + Sync,
    >,
    /// Borrows the app-server launch's existing reducer, independent of Session event transport.
    pub record_feature_use: Arc<dyn Fn(codex_protocol::feature_use::FeatureUseFact) + Send + Sync>,
    pub route_foreign: Arc<
        dyn Fn(
                codex_protocol::agent_foreign::AgentForeignRequest,
                CancellationToken,
            )
                -> BoxFuture<'static, CodexResult<super::control::foreign::ForeignResponse>>
            + Send
            + Sync,
    >,
    pub prepare_final_capture: FinalCapturePreparer,
    pub admit_child: ChildHostAdmission,
    pub freeze_progress: ProgressFreezer,
    pub prepare_progress_output: ProgressOutputPreparer,
    pub project_final_capture: FinalCaptureProjector,
}

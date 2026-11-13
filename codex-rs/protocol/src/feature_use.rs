//! Launch-local accepted coordination and native reset facts, independent of analytics export.
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use ts_rs::TS;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[ts(export_to = "v2/")]
pub struct FeatureUseFact {
    pub thread_id: String,
    pub operation: String,
    pub event: FeatureUseEvent,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(tag = "type", rename_all = "camelCase", deny_unknown_fields)]
#[ts(export_to = "v2/")]
pub enum FeatureUseEvent {
    ResetCompleted,
    SelfResetRequested {
        #[serde(default)]
        preload: bool,
    },
    SelfResetCompleted,
    SelfResetFailed,
    ChildAdmitted {
        #[serde(default)]
        preload: bool,
        #[serde(default)]
        sources: bool,
    },
    PreloadRejected,
    FinalDelivered {
        #[serde(default)]
        changes: bool,
    },
    MessageAccepted {
        #[serde(default)]
        sources: bool,
        #[serde(default)]
        btw: bool,
    },
    SideQuestionRejected,
    SideQuestionFailed,
    WaitRegistered {
        #[serde(default)]
        all: bool,
        #[serde(default)]
        progress: bool,
    },
    ProgressDelivered,
    AssignmentDelivered {
        #[serde(default)]
        sources: bool,
    },
}

#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct FeatureUseCounts {
    pub compactions: u64,
    pub self_compactions: u64,
    pub compaction_preloads: u64,
    pub spawns: u64,
    pub spawn_preloads: u64,
    pub preload_rejections: u64,
    pub finals: u64,
    pub completion_changes: u64,
    pub messages: u64,
    pub side_questions: u64,
    pub side_question_rejections: u64,
    pub side_question_failures: u64,
    pub waits: u64,
    pub all_waits: u64,
    pub progress_waits: u64,
    pub progress_yields: u64,
    pub assignment_sources: u64,
    pub message_sources: u64,
    pub self_compactions_completed: u64,
    pub self_compaction_failures: u64,
}

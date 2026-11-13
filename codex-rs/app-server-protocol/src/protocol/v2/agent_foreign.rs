//! Forwarding between authenticated active owners without tree enrollment.

use super::AgentObservation;
use super::AgentTreeObservation;
use crate::JsonSchema;
use crate::TS;
use codex_protocol::ThreadId;
pub use codex_protocol::agent_foreign::*;
use serde::Deserialize;
use serde::Serialize;
use uuid::Uuid;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(
    tag = "type",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
#[ts(export_to = "v2/")]
pub enum AgentForeignResponse {
    MessageAccepted {
        target_thread_id: ThreadId,
        #[schemars(with = "String")]
        delivery_id: Uuid,
    },
    SideQuestionAnswered {
        target_thread_id: ThreadId,
        answer: String,
    },
    SideQuestionFailed {
        target_thread_id: ThreadId,
        accepted: bool,
        error: String,
    },
    Listed {
        tree: AgentTreeObservation,
    },
    Inspected {
        agent: AgentObservation,
    },
}

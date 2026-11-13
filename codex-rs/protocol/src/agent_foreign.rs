//! Authenticated native forwarding between active owners, never tree enrollment.

use crate::AgentPath;
use crate::ThreadId;
use schemars::JsonSchema;
use serde::Deserialize;
use serde::Serialize;
use ts_rs::TS;

/// Native constructs this attribution from an admitted tool invocation. The
/// root/actor/turn/call tuple is the original operation identity across retries.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[ts(export_to = "v2/")]
pub struct AgentForeignOrigin {
    pub root_thread_id: ThreadId,
    pub actor_id: ThreadId,
    pub agent_path: AgentPath,
    pub turn_id: String,
    pub call_id: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(tag = "type", rename_all = "camelCase", deny_unknown_fields)]
#[ts(export_to = "v2/")]
pub enum AgentForeignOperation {
    Message {
        message: String,
    },
    SideQuestion {
        question: String,
    },
    #[serde(rename_all = "camelCase")]
    List {
        path_prefix: Option<String>,
    },
    Inspect {},
}

/// Only the owning native connection may submit this envelope. Its fields are
/// evidence supplied by the source owner, not public caller authority.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[ts(export_to = "v2/")]
pub struct AgentForeignRequest {
    pub source: AgentForeignOrigin,
    pub target_thread_id: ThreadId,
    pub operation: AgentForeignOperation,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn foreign_operations_have_no_control_or_caller_override_variants() {
        for operation in [
            json!({"type": "followupTask", "message": "start"}),
            json!({"type": "spawn"}),
            json!({"type": "interrupt"}),
            json!({"type": "compact"}),
            json!({"type": "wait"}),
            json!({"type": "message", "message": "note", "triggerTurn": true}),
            json!({"type": "inspect", "actorId": ThreadId::new()}),
        ] {
            assert!(serde_json::from_value::<AgentForeignOperation>(operation).is_err());
        }
    }

    #[test]
    fn foreign_request_requires_exact_thread_ids() {
        let source = AgentForeignOrigin {
            root_thread_id: ThreadId::new(),
            actor_id: ThreadId::new(),
            agent_path: AgentPath::root().join("reviewer").unwrap(),
            turn_id: "turn".to_owned(),
            call_id: "call".to_owned(),
        };
        let request = AgentForeignRequest {
            source,
            target_thread_id: ThreadId::new(),
            operation: AgentForeignOperation::SideQuestion {
                question: "status?".to_owned(),
            },
        };
        let mut value = serde_json::to_value(&request).unwrap();
        assert_eq!(value["operation"]["type"], "sideQuestion");
        assert_eq!(
            serde_json::from_value::<AgentForeignRequest>(value.clone()).unwrap(),
            request
        );
        value["targetThreadId"] = json!("reviewer");
        assert!(serde_json::from_value::<AgentForeignRequest>(value).is_err());
    }
}

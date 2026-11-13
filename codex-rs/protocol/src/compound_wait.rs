use crate::protocol::ReloadSelection;
use schemars::JsonSchema;
use serde::Deserialize;
use serde::Serialize;
use ts_rs::TS;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CompoundWaitTicket {
    pub wait_id: String,
    pub generation: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(rename_all = "snake_case")]
pub enum CompoundExecutionResult {
    Yielded,
    Succeeded,
    Failed,
    Terminated,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CompoundWaitSettlement {
    pub thread_id: String,
    pub turn_id: String,
    pub item_id: String,
    pub window_id: String,
    pub cell_id: String,
    pub ticket: CompoundWaitTicket,
    pub result: CompoundExecutionResult,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(rename_all = "snake_case")]
pub enum CompoundWaitAction {
    Retain,
    Retire,
    Superseded,
    Commit,
    Uncertain,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CompoundWaitDisposition {
    pub action: CompoundWaitAction,
    pub ticket: CompoundWaitTicket,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reload: Option<ReloadSelection>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub diagnostic: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(rename_all = "snake_case")]
pub enum CompoundResetOutcome {
    Completed,
    Failed,
    Canceled,
    Uncertain,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CompoundWaitReset {
    pub thread_id: String,
    pub turn_id: String,
    pub item_id: String,
    pub window_id: String,
    pub cell_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ticket: Option<CompoundWaitTicket>,
    pub reset: CompoundResetOutcome,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub diagnostic: Option<String>,
}

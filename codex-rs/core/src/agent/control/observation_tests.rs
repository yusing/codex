use super::*;
use crate::agent::control::LocalAgentControl;
use codex_protocol::protocol::TokenUsage;

#[test]
fn context_metrics_are_last_request_not_cumulative_usage() {
    let usage = TokenUsageInfo {
        total_token_usage: TokenUsage {
            total_tokens: 98_000,
            ..Default::default()
        },
        last_token_usage: TokenUsage {
            total_tokens: 7_000,
            ..Default::default()
        },
        last_usage_includes_reasoning: None,
        model_context_window: Some(128_000),
    };
    let mut metrics = AgentMetrics::default();
    apply_usage(&mut metrics, &usage);
    assert_eq!(metrics.context_tokens, Some(7_000));
    assert_eq!(metrics.context_window, Some(128_000));
    assert_eq!(metrics.completed_compactions, None);
}

#[test]
fn missing_observation_is_null_not_zero() {
    let value = serde_json::to_value(AgentMetrics::default()).unwrap();
    assert!(value["contextTokens"].is_null());
    assert!(value["contextWindow"].is_null());
    assert!(value["completedCompactions"].is_null());
}

#[tokio::test]
async fn observation_rejects_unadmitted_identity_without_loading() {
    let control = LocalAgentControl::default();
    let id = ThreadId::new();
    let error = control.runtime.observe_agent(id).await.unwrap_err();
    assert!(matches!(error.details(), CodexErrorDetails::ThreadNotFound(found) if *found == id));
}

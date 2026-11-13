use super::*;
use pretty_assertions::assert_eq;

#[tokio::test]
async fn followup_keeps_accepted_receipt_when_observation_is_unavailable() {
    let (mut session, mut turn) = make_session_and_context().await;
    let manager = thread_manager();
    let mut config = (*turn.config).clone();
    config.features.enable(Feature::MultiAgentV2).unwrap();
    set_turn_config(&mut turn, config.clone());
    let root = manager
        .start_thread(StartThreadOptions::new(config))
        .await
        .unwrap();
    root.thread.session.new_default_turn().await;
    set_agent_control(&mut session, manager.agent_control());
    session.thread_id = root.thread_id;
    // Delivery still uses its actual controller. Its optional observation owner
    // is unavailable, so the accepted input must not become a failed tool call.
    session.services.local_agent_runtime = crate::agent::LocalAgentControl::default().runtime;
    let session = Arc::new(session);
    let turn = Arc::new(turn);
    SpawnAgentHandlerV2::default()
        .handle(invocation(
            session.clone(),
            turn.clone(),
            "spawn_agent",
            function_payload(json!({"task_name": "worker", "message": "initial work"})),
        ))
        .await
        .unwrap();
    let child = session
        .services
        .agent_control
        .resolve(
            session.thread_id,
            turn.parent_thread_id,
            &turn.session_source,
            "worker",
        )
        .await
        .unwrap();
    let output = FollowupTaskHandlerV2
        .handle(invocation(
            session,
            turn,
            "followup_task",
            function_payload(json!({"target": child.to_string(), "message": "next work"})),
        ))
        .await
        .unwrap();
    let receipt = output.code_mode_result(&function_payload(json!({})));
    assert_eq!(receipt["accepted"], true);
    assert_eq!(receipt["agent_id"], child.to_string());
    assert_eq!(receipt["agent_name"], "/root/worker");
    assert!(
        receipt["submission_id"]
            .as_str()
            .is_some_and(|id| !id.is_empty())
    );
    assert_eq!(
        receipt["metrics"],
        json!({
            "contextTokens": null, "contextWindow": null, "completedCompactions": null,
        })
    );
    assert!(receipt["observation_error"].as_str().is_some());
    assert!(manager.captured_ops().iter().any(|(id, op)| {
        *id == child && matches!(op, Op::InterAgentCommunication { communication, .. }
            if communication.encrypted_content.as_deref() == Some("next work") && communication.trigger_turn)
    }));
    assert_eq!(expect_text_output(output).1, Some(true));
}

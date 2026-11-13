use super::*;
use pretty_assertions::assert_eq;

#[tokio::test]
async fn inspect_reports_applied_settings_and_last_context_without_starting_work() {
    let (mut session, mut turn) = make_session_and_context().await;
    let manager = thread_manager();
    let mut config = (*turn.config).clone();
    config.features.enable(Feature::MultiAgentV2).unwrap();
    config.model_reasoning_effort = Some(ReasoningEffort::High);
    set_turn_config(&mut turn, config.clone());
    let root = manager
        .start_thread(StartThreadOptions::new(config))
        .await
        .unwrap();
    set_agent_control(&mut session, manager.agent_control());
    session.thread_id = root.thread_id;
    let root_turn = root.thread.session.new_default_turn().await;
    for total_tokens in [7_300, 4_200] {
        root.thread
            .session
            .update_token_usage_info(
                &root_turn,
                Some(&codex_protocol::protocol::TokenUsage {
                    total_tokens,
                    ..Default::default()
                }),
                false,
            )
            .await
            .unwrap();
    }
    root.thread
        .update_thread_settings(codex_protocol::protocol::ThreadSettingsOverrides {
            effort: Some(Some(ReasoningEffort::Low)),
            ..Default::default()
        })
        .await
        .unwrap();
    let usage = root.thread.token_usage_info().await.unwrap();
    let session = Arc::new(session);
    let turn = Arc::new(turn);
    let before_ops = manager.captured_ops().len();
    for target in ["/root".to_owned(), root.thread_id.to_string()] {
        let output = InspectAgentHandler
            .handle(invocation(
                session.clone(),
                turn.clone(),
                "inspect_agent",
                function_payload(json!({"target": target})),
            ))
            .await
            .unwrap();
        let result = output.code_mode_result(&function_payload(json!({})));
        assert_eq!(result["threadId"], root.thread_id.to_string());
        assert_eq!(result["reasoningEffort"], "low");
        assert!(
            result["model"]
                .as_str()
                .is_some_and(|model| !model.is_empty())
        );
        assert_eq!(result["metrics"]["contextTokens"], 4_200);
        assert_eq!(
            result["metrics"]["contextWindow"],
            json!(usage.model_context_window)
        );
        assert!(
            result["metrics"]
                .as_object()
                .unwrap()
                .contains_key("completedCompactions")
        );
        let (content, success) = expect_text_output(output);
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&content).unwrap(),
            result
        );
        assert_eq!(success, Some(true));
    }
    let unknown = ThreadId::new();
    let error = InspectAgentHandler
        .handle(invocation(
            session,
            turn,
            "inspect_agent",
            function_payload(json!({"target": unknown.to_string()})),
        ))
        .await;
    assert!(matches!(error, Err(FunctionCallError::RespondToModel(_))));
    assert!(manager.get_thread(unknown).await.is_err());
    assert_eq!(manager.captured_ops().len(), before_ops);
    assert!(root.thread.session.active_turn.lock().await.is_none());
}

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

#[tokio::test]
async fn native_model_foreign_message_list_and_inspect_use_destination_owner() {
    let (mut session, mut turn) = make_session_and_context().await;
    let source_manager = thread_manager();
    let mut config = (*turn.config).clone();
    config.features.enable(Feature::MultiAgentV2).unwrap();
    set_turn_config(&mut turn, config.clone());
    let source_root = source_manager
        .start_thread(StartThreadOptions::new(config.clone()))
        .await
        .unwrap();
    set_agent_control(&mut session, source_manager.agent_control());
    session.thread_id = source_root.thread_id;

    let destination_home = tempfile::tempdir().unwrap();
    config.codex_home = destination_home.path().to_path_buf().try_into().unwrap();
    config.features.enable(Feature::Sqlite).unwrap();
    let state = init_state_db(&config).await.expect("destination state");
    let destination = Arc::new(
        ThreadManager::with_models_provider_home_and_state_for_tests(
            CodexAuth::from_api_key("dummy"),
            config.model_provider.clone(),
            config.codex_home.to_path_buf(),
            Arc::new(codex_exec_server::EnvironmentManager::default_for_tests()),
            Some(state),
        ),
    );
    let destination_root = destination
        .start_thread(StartThreadOptions::new(config))
        .await
        .unwrap();
    let target = destination_root.thread_id;
    let routed = Arc::new(std::sync::Mutex::new(Vec::new()));
    assert!(
        source_root
            .thread
            .install_host_callbacks(crate::AgentHostCallbacks {
                host_connection_id: 23,
                publish_coordination: Arc::new(|_| Box::pin(async {})),
                route_foreign: Arc::new({
                    let destination = Arc::clone(&destination);
                    let routed = Arc::clone(&routed);
                    move |request, cancellation| {
                        routed.lock().unwrap().push(request.clone());
                        let destination = Arc::clone(&destination);
                        Box::pin(async move {
                            destination
                                .receive_foreign_agent(request, cancellation)
                                .await
                        })
                    }
                }),
                prepare_final_capture: Arc::new(|_, _| Box::pin(async {
                    panic!("unexpected final capture")
                })),
                admit_child: Arc::new(|_, _| Box::pin(async {
                    panic!("foreign actor must not be admitted locally")
                })),
                freeze_progress: Arc::new(|_, _| Box::pin(async { panic!("unexpected progress") })),
                prepare_progress_output: Arc::new(|_, _| Box::pin(async {
                    panic!("unexpected progress")
                })),
                project_final_capture: Arc::new(|_, _| Box::pin(async {
                    panic!("unexpected final projection")
                })),
            })
    );
    let session = Arc::new(session);
    let turn = Arc::new(turn);
    let mut first = None;
    for _ in 0..2 {
        let mut call = invocation(
            session.clone(),
            turn.clone(),
            "send_message",
            function_payload(
                json!({"target": target.to_string(), "message": "retained foreign note"}),
            ),
        );
        call.call_id = "original-message".to_owned();
        let output = SendMessageHandlerV2.handle(call).await.unwrap();
        let result = output.code_mode_result(&function_payload(json!({})));
        assert_eq!(result["type"], "messageAccepted");
        assert_eq!(result["targetThreadId"], target.to_string());
        if let Some(first) = &first {
            assert_eq!(&result, first);
        }
        first = Some(result);
    }
    let mut call = invocation(
        session.clone(),
        turn.clone(),
        "inspect_agent",
        function_payload(json!({"target": target.to_string()})),
    );
    call.call_id = "inspect-foreign".to_owned();
    let inspected = InspectAgentHandler
        .handle(call)
        .await
        .unwrap()
        .code_mode_result(&function_payload(json!({})));
    assert_eq!(inspected["threadId"], target.to_string());
    let mut call = invocation(
        session.clone(),
        turn.clone(),
        "list_agents",
        function_payload(json!({"session_id": target.to_string(), "path_prefix": "/root"})),
    );
    call.call_id = "list-foreign".to_owned();
    let listed = ListAgentsHandlerV2
        .handle(call)
        .await
        .unwrap()
        .code_mode_result(&function_payload(json!({})));
    assert_eq!(listed["agents"].as_array().unwrap().len(), 1);
    assert_eq!(listed["agents"][0]["agent_id"], target.to_string());
    assert!(listed["agents"][0].get("metrics").is_none());

    assert!(
        InspectAgentHandler
            .handle(invocation(
                session.clone(),
                turn.clone(),
                "inspect_agent",
                function_payload(json!({"target": "missing-alias"}))
            ))
            .await
            .is_err()
    );
    assert!(
        ListAgentsHandlerV2
            .handle(invocation(
                session.clone(),
                turn.clone(),
                "list_agents",
                function_payload(json!({"session_id": "missing-alias"}))
            ))
            .await
            .is_err()
    );
    assert!(
        FollowupTaskHandlerV2
            .handle(invocation(
                session.clone(),
                turn.clone(),
                "followup_task",
                function_payload(
                    json!({"target": target.to_string(), "message": "must not start foreign work"})
                )
            ))
            .await
            .is_err()
    );
    assert!(
        crate::tools::handlers::multi_agents_v2::WaitAgentHandler::default()
            .handle(invocation(
                session.clone(),
                turn.clone(),
                "wait_agent",
                function_payload(json!({"target": [target.to_string()]})),
            ))
            .await
            .is_err()
    );
    let requests = routed.lock().unwrap().clone();
    assert_eq!(
        requests.len(),
        4,
        "local validation/control errors must never fall back to foreign routing"
    );
    assert!(
        requests
            .iter()
            .all(|request| request.source.actor_id == source_root.thread_id
                && request.source.root_thread_id == source_root.thread_id
                && request.source.turn_id == turn.sub_id)
    );
    assert_eq!(requests[0].source.call_id, "original-message");
    assert_eq!(requests[0], requests[1]);
    let destination_runtime = &destination_root.thread.session.services.local_agent_runtime;
    assert_eq!(
        destination_runtime
            .work_ledger()
            .unwrap()
            .unread_deliveries(target, 0, 256)
            .await
            .unwrap()
            .len(),
        1
    );
    assert!(
        destination_runtime
            .work_ledger()
            .unwrap()
            .work_stamp(target)
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        destination_root
            .thread
            .session
            .active_turn
            .lock()
            .await
            .is_none()
    );
    assert_eq!(
        source_manager
            .observe_agents(source_root.thread_id)
            .await
            .unwrap()
            .agents
            .len(),
        1
    );
    destination_root.thread.shutdown_and_wait().await.unwrap();
    source_root.thread.shutdown_and_wait().await.unwrap();
}

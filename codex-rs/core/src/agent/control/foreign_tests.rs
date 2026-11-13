use super::*;
use crate::agent::types::AgentMetadata;
use crate::session::TurnInput;
use crate::thread_manager::default_thread_id_generator;
use codex_protocol::AgentPath;
use codex_utils_absolute_path::test_support::PathExt;
use std::sync::Arc;
use std::sync::Weak;

async fn fixture() -> anyhow::Result<(tempfile::TempDir, LocalAgentRuntime, ThreadId, ThreadId)> {
    let directory = tempfile::tempdir()?;
    let state = codex_state::StateRuntime::init(
        codex_state::SqliteConfig::new_for_testing(directory.path().abs()),
        "test".to_owned(),
    )
    .await?;
    let runtime = LocalAgentRuntime::new(
        Weak::default(),
        default_thread_id_generator(),
        None,
        Some(state),
    );
    let root = ThreadId::new();
    runtime.registry.register_root_thread(root);
    let target = add_member(&runtime, "recipient");
    Ok((directory, runtime, root, target))
}

fn add_member(runtime: &LocalAgentRuntime, name: &str) -> ThreadId {
    let actor = ThreadId::new();
    runtime
        .registry
        .reserve_spawn_slot(None)
        .unwrap()
        .commit(AgentMetadata {
            agent_id: Some(actor),
            agent_path: Some(AgentPath::root().join(name).unwrap()),
            ..Default::default()
        });
    actor
}

fn origin() -> AgentForeignOrigin {
    AgentForeignOrigin {
        root_thread_id: ThreadId::new(),
        actor_id: ThreadId::new(),
        agent_path: AgentPath::root().join("sender").unwrap(),
        turn_id: "original-turn".to_owned(),
        call_id: "original-call".to_owned(),
    }
}

fn message_request(
    source: AgentForeignOrigin,
    target: ThreadId,
    message: &str,
) -> AgentForeignRequest {
    AgentForeignRequest {
        source,
        target_thread_id: target,
        operation: AgentForeignOperation::Message {
            message: message.to_owned(),
        },
    }
}

#[tokio::test]
async fn only_unknown_exact_ids_are_foreign_candidates() -> anyhow::Result<()> {
    let (_directory, runtime, root, target) = fixture().await?;
    let foreign = ThreadId::new();
    assert_eq!(
        runtime.classify_foreign_target(root, &foreign.to_string())?,
        Some(foreign)
    );
    assert_eq!(
        runtime.classify_foreign_target(root, &foreign.to_string().to_uppercase())?,
        Some(foreign)
    );
    for reference in [
        target.to_string(),
        root.to_string(),
        "/root/recipient".to_owned(),
        "recipient".to_owned(),
        "missing-alias".to_owned(),
        "../invalid".to_owned(),
    ] {
        assert_eq!(runtime.classify_foreign_target(root, &reference)?, None);
    }
    assert!(
        runtime
            .classify_foreign_target(ThreadId::new(), &foreign.to_string())
            .is_err()
    );
    assert!(
        runtime
            .registry
            .agent_metadata_for_thread(foreign)
            .is_none()
    );
    Ok(())
}

#[tokio::test]
async fn foreign_message_is_retained_once_without_enrollment_or_idle_start() -> anyhow::Result<()> {
    let (_directory, runtime, root, target) = fixture().await?;
    let source = origin();
    let request = message_request(source.clone(), target, "retained note");
    let first = runtime
        .receive_foreign(request.clone(), CancellationToken::new())
        .await?;
    let second = runtime
        .receive_foreign(request, CancellationToken::new())
        .await?;
    assert_eq!(first, second);
    let ledger = runtime.work_ledger()?;
    let rows = ledger.unread_deliveries(target, 0, 256).await?;
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].publication.sender_id, Some(source.actor_id));
    assert!(!rows[0].consumed);
    assert!(ledger.work_stamp(root).await?.is_none());
    assert!(ledger.work_stamp(target).await?.is_none());
    assert!(
        runtime
            .registry
            .agent_metadata_for_thread(source.actor_id)
            .is_none()
    );
    assert!(
        runtime
            .registry
            .agent_metadata_for_thread(source.root_thread_id)
            .is_none()
    );
    // No thread manager exists in this fixture. A restore or executor lookup
    // would fail, while retained-only delivery remains available.
    let TurnInput::InterAgentCommunication(message) = super::super::work_lifecycle::delivery_input(
        &rows[0],
        None,
        AgentPath::root().join("recipient").unwrap(),
    )?
    else {
        panic!("foreign message lost attributed delivery");
    };
    assert!(!message.trigger_turn);
    assert_eq!(message.author, source.agent_path);
    assert_eq!(
        message.coordination_delivery_ids(),
        &[rows[0].publication.id]
    );
    assert!(message.content.contains(&source.root_thread_id.to_string()));
    assert!(message.content.contains(&source.actor_id.to_string()));
    assert!(message.content.ends_with("retained note"));
    Ok(())
}

#[tokio::test]
async fn foreign_message_wakes_the_existing_wait_without_consuming_unrelated_work()
-> anyhow::Result<()> {
    use super::super::work::WorkStamp;
    use codex_state::coordination_wait::WaitCell;
    use codex_state::coordination_wait::WaitMode;
    let (_directory, runtime, root, child) = fixture().await?;
    let ledger = runtime.work_ledger()?;
    let mut work = Vec::new();
    for actor in [root, child] {
        let stamp = WorkStamp {
            root_id: root,
            actor_id: actor,
            work_id: Uuid::now_v7(),
        };
        ledger
            .admit_work(
                stamp,
                None,
                &PublishDelivery {
                    id: stamp.work_id,
                    root_id: root,
                    sender_id: None,
                    recipient_id: actor,
                    work: Some(stamp),
                    kind: DeliveryKind::Message,
                    payload: serde_json::to_string(&CoordinationPayload::UserInput {
                        content: Vec::new(),
                    })?,
                },
            )
            .await?;
        let dispatch = stamp.work_id;
        ledger
            .begin_delivery(actor, &[stamp.work_id], None, dispatch)
            .await?;
        ledger
            .confirm_delivery(actor, &[stamp.work_id], dispatch, "initial")
            .await?;
        ledger
            .consume_deliveries(actor, &[stamp.work_id], "initial")
            .await?;
        work.push(stamp);
    }
    let cell = WaitCell {
        turn_id: "turn".into(),
        item_id: "item".into(),
        window_id: "window".into(),
        cell_id: "cell".into(),
    };
    let mut ticket = ledger.register_wait(work[0], &cell, WaitMode::All).await?;
    let source = origin();
    let id = delivery_id(&source)?;
    runtime
        .receive_foreign(
            message_request(source.clone(), root, "foreign progress"),
            CancellationToken::new(),
        )
        .await?;
    ticket.claim = ledger.claim_ready_wait(work[0], ticket.id).await?;
    assert_eq!(ticket.claim.as_ref().unwrap().delivery_ids, vec![id]);
    assert_eq!(ticket.selected, vec![work[1]]);
    let envelope = runtime.wait_continuation_input(&ticket).await?;
    let codex_protocol::models::ResponseItem::FunctionCallOutput { output, .. } = &envelope.item
    else {
        panic!("expected native function output");
    };
    let value: serde_json::Value = serde_json::from_str(&output.text_content().unwrap())?;
    assert_eq!(
        value["messages"][0]["sender_thread_id"],
        source.actor_id.to_string()
    );
    assert_eq!(
        value["messages"][0]["payload"]["origin"]["rootThreadId"],
        source.root_thread_id.to_string()
    );
    assert_eq!(
        value["messages"][0]["payload"]["original_message"],
        "foreign progress"
    );
    assert!(value["outcomes"].as_array().unwrap().is_empty());
    assert!(!ledger.observe_delivery(root, id).await?.unwrap().consumed);
    assert_eq!(ledger.work_stamp(child).await?, Some(work[1]));
    assert!(
        runtime
            .registry
            .agent_metadata_for_thread(source.actor_id)
            .is_none()
    );
    Ok(())
}

#[tokio::test]
async fn retained_snapshot_wins_and_conflicts_reject_before_resource_acquisition()
-> anyhow::Result<()> {
    let (_directory, runtime, root, target) = fixture().await?;
    let other = add_member(&runtime, "other");
    let source = origin();
    let id = delivery_id(&source)?;
    let original = "Read @recipient-only.txt";
    let publication = PublishDelivery {
        id,
        root_id: root,
        sender_id: Some(source.actor_id),
        recipient_id: target,
        work: None,
        kind: DeliveryKind::Message,
        payload: serde_json::to_string(&CoordinationPayload::ForeignMessage {
            origin: source.clone(),
            original_message: original.to_owned(),
            frozen_sources: Some("previously frozen authorized bytes".to_owned()),
        })?,
    };
    runtime
        .work_ledger()?
        .publish_delivery(&publication)
        .await?;
    let result = runtime
        .receive_foreign(
            message_request(source.clone(), target, original),
            CancellationToken::new(),
        )
        .await?;
    assert_eq!(
        result,
        ForeignResponse::MessageAccepted {
            target_thread_id: target,
            delivery_id: id
        }
    );
    // A second source acquisition cannot succeed: this retained fixture has no
    // loaded recipient/permissions context. It must use the accepted bytes.
    assert_eq!(
        runtime
            .work_ledger()?
            .observe_delivery(target, id)
            .await?
            .unwrap()
            .publication,
        publication
    );
    let error = runtime
        .receive_foreign(
            message_request(source.clone(), target, "Read @different.txt"),
            CancellationToken::new(),
        )
        .await
        .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("conflicting repeated foreign message")
    );
    let error = runtime
        .receive_foreign(
            message_request(source.clone(), other, original),
            CancellationToken::new(),
        )
        .await
        .unwrap_err();
    assert!(error.to_string().contains("delivery identity"));
    let mut changed_origin = source;
    changed_origin.agent_path = AgentPath::root().join("impostor").unwrap();
    assert!(
        runtime
            .receive_foreign(
                message_request(changed_origin, target, original),
                CancellationToken::new()
            )
            .await
            .is_err()
    );
    assert!(
        runtime
            .work_ledger()?
            .unread_deliveries(other, 0, 256)
            .await?
            .is_empty()
    );
    assert_eq!(
        runtime
            .work_ledger()?
            .observe_delivery(target, id)
            .await?
            .unwrap()
            .publication,
        publication
    );
    Ok(())
}

#[tokio::test]
async fn message_sources_are_frozen_in_recipient_scope_and_reused_on_repeat() -> anyhow::Result<()>
{
    let directory = tempfile::tempdir()?;
    let recipient = directory.path().join("recipient");
    let sender = directory.path().join("sender");
    tokio::fs::create_dir_all(&recipient).await?;
    tokio::fs::create_dir_all(&sender).await?;
    tokio::fs::write(recipient.join("selected.txt"), "recipient-authorized body").await?;
    tokio::fs::write(
        sender.join("selected.txt"),
        "sender body must not be captured",
    )
    .await?;
    let mut config = crate::config::test_config().await;
    config.codex_home = directory.path().join("codex").try_into()?;
    tokio::fs::create_dir_all(&config.codex_home).await?;
    config.cwd = recipient.try_into()?;
    let state = codex_state::StateRuntime::init(
        codex_state::SqliteConfig::new_for_testing(directory.path().abs()),
        "test".to_owned(),
    )
    .await?;
    let manager = crate::ThreadManager::with_models_provider_home_and_state_for_tests(
        codex_login::CodexAuth::from_api_key("dummy"),
        config.model_provider.clone(),
        config.codex_home.to_path_buf(),
        Arc::new(codex_exec_server::EnvironmentManager::default_for_tests()),
        Some(state),
    );
    let root = manager
        .start_thread(crate::StartThreadOptions::new(config))
        .await?;
    let runtime = &root.thread.session.services.local_agent_runtime;
    let source = origin();
    let request = message_request(source.clone(), root.thread_id, "Read @selected.txt");
    let first = runtime
        .receive_foreign(request.clone(), CancellationToken::new())
        .await?;
    let id = delivery_id(&source)?;
    let retained = runtime
        .work_ledger()?
        .observe_delivery(root.thread_id, id)
        .await?
        .unwrap();
    assert!(
        retained
            .publication
            .payload
            .contains("recipient-authorized body")
    );
    assert!(
        !retained
            .publication
            .payload
            .contains("sender body must not be captured")
    );
    tokio::fs::write(
        directory.path().join("recipient/selected.txt"),
        "changed after acceptance",
    )
    .await?;
    let repeated = runtime
        .receive_foreign(request, CancellationToken::new())
        .await?;
    assert_eq!(repeated, first);
    assert_eq!(
        runtime
            .work_ledger()?
            .observe_delivery(root.thread_id, id)
            .await?
            .unwrap(),
        retained
    );
    assert!(
        runtime
            .work_ledger()?
            .work_stamp(root.thread_id)
            .await?
            .is_none()
    );
    assert!(root.thread.session.active_turn.lock().await.is_none());
    root.thread.shutdown_and_wait().await?;
    Ok(())
}

#[tokio::test]
async fn foreign_reads_use_passive_native_observations_and_btw_rejection_does_not_start_work()
-> anyhow::Result<()> {
    let directory = tempfile::tempdir()?;
    let mut config = crate::config::test_config().await;
    config.codex_home = directory.path().to_path_buf().try_into()?;
    config.cwd = directory.path().to_path_buf().try_into()?;
    let manager = crate::ThreadManager::with_models_provider_and_home_for_tests(
        codex_login::CodexAuth::from_api_key("dummy"),
        config.model_provider.clone(),
        config.codex_home.to_path_buf(),
        Arc::new(codex_exec_server::EnvironmentManager::default_for_tests()),
    );
    let root = manager
        .start_thread(crate::StartThreadOptions::new(config))
        .await?;
    let runtime = &root.thread.session.services.local_agent_runtime;
    let retained = add_member(runtime, "retained");
    let revision = runtime.registry.snapshot().revision;
    let source = origin();
    let inspect = runtime
        .receive_foreign(
            AgentForeignRequest {
                source: source.clone(),
                target_thread_id: retained,
                operation: AgentForeignOperation::Inspect {},
            },
            CancellationToken::new(),
        )
        .await?;
    let ForeignResponse::Inspected { agent } = inspect else {
        panic!("expected native observation");
    };
    assert_eq!(agent.thread_id, retained.to_string());
    assert!(!agent.loaded);
    assert!(agent.model.is_none());
    assert!(agent.metrics.context_tokens.is_none());
    let list = runtime
        .receive_foreign(
            AgentForeignRequest {
                source: source.clone(),
                target_thread_id: root.thread_id,
                operation: AgentForeignOperation::List {
                    path_prefix: Some("retained".to_owned()),
                },
            },
            CancellationToken::new(),
        )
        .await?;
    let ForeignResponse::Listed { tree } = list else {
        panic!("expected native tree");
    };
    assert_eq!(tree.agents.len(), 1);
    assert_eq!(tree.agents[0].thread_id, retained.to_string());
    assert_eq!(runtime.registry.snapshot().revision, revision);
    assert!(manager.get_thread(retained).await.is_err());
    assert!(root.thread.session.active_turn.lock().await.is_none());
    let answer = runtime
        .receive_foreign(
            AgentForeignRequest {
                source,
                target_thread_id: retained,
                operation: AgentForeignOperation::SideQuestion {
                    question: String::new(),
                },
            },
            CancellationToken::new(),
        )
        .await?;
    assert!(matches!(
        answer,
        ForeignResponse::SideQuestionFailed {
            accepted: false,
            ..
        }
    ));
    assert!(manager.get_thread(retained).await.is_err());
    assert!(root.thread.session.active_turn.lock().await.is_none());
    root.thread.shutdown_and_wait().await?;
    Ok(())
}

#[test]
fn response_must_match_original_operation_target_and_delivery_identity() {
    let source = origin();
    let target = ThreadId::new();
    let request = message_request(source.clone(), target, "note");
    let accepted = ForeignResponse::MessageAccepted {
        target_thread_id: target,
        delivery_id: delivery_id(&source).unwrap(),
    };
    assert!(validate_response(&request, &accepted).is_ok());
    assert!(
        validate_response(
            &request,
            &ForeignResponse::MessageAccepted {
                target_thread_id: target,
                delivery_id: Uuid::now_v7()
            }
        )
        .is_err()
    );
    assert!(
        validate_response(
            &request,
            &ForeignResponse::MessageAccepted {
                target_thread_id: ThreadId::new(),
                delivery_id: delivery_id(&source).unwrap()
            }
        )
        .is_err()
    );
    assert!(
        validate_response(
            &request,
            &ForeignResponse::SideQuestionAnswered {
                target_thread_id: target,
                answer: "wrong operation".to_owned()
            }
        )
        .is_err()
    );
}

#[tokio::test]
async fn source_counts_known_btw_admission_and_never_invents_a_transport_receipt()
-> anyhow::Result<()> {
    let (_directory, runtime, root, _target) = fixture().await?;
    let foreign = ThreadId::new();
    let facts = Arc::new(std::sync::Mutex::new(Vec::new()));
    assert!(
        runtime
            .host_callbacks
            .set(crate::AgentHostCallbacks {
                host_connection_id: 17,
                publish_coordination: Arc::new(|_| Box::pin(async {})),
                record_feature_use: Arc::new({
                    let facts = Arc::clone(&facts);
                    move |fact| facts.lock().unwrap().push(fact)
                }),
                route_foreign: Arc::new(move |request, _| Box::pin(async move {
                    assert_eq!(request.source.root_thread_id, root);
                    assert_eq!(request.source.actor_id, root);
                    assert_eq!(request.target_thread_id, foreign);
                    let AgentForeignOperation::SideQuestion { question } = request.operation else {
                        panic!("wrong operation");
                    };
                    match question.as_str() {
                        "unknown transport" => Err(CodexErr::Fatal(
                            "destination receipt unavailable".to_owned(),
                        )),
                        "known rejection" => Ok(ForeignResponse::SideQuestionFailed {
                            target_thread_id: foreign,
                            accepted: false,
                            error: "recipient rejected".to_owned(),
                        }),
                        "known execution failure" => Ok(ForeignResponse::SideQuestionFailed {
                            target_thread_id: foreign,
                            accepted: true,
                            error: "execution failed".to_owned(),
                        }),
                        "answer" => Ok(ForeignResponse::SideQuestionAnswered {
                            target_thread_id: foreign,
                            answer: "independent answer".to_owned(),
                        }),
                        _ => panic!("unexpected question"),
                    }
                })),
                prepare_final_capture: Arc::new(|_, _| Box::pin(async {
                    panic!("unexpected final capture")
                })),
                admit_child: Arc::new(|_, _| Box::pin(async {
                    panic!("foreign actor must not be enrolled")
                })),
                freeze_progress: Arc::new(|_, _| Box::pin(async { panic!("unexpected progress") })),
                prepare_progress_output: Arc::new(|_, _| Box::pin(async {
                    panic!("unexpected progress")
                })),
                project_final_capture: Arc::new(|_, _| Box::pin(async {
                    panic!("unexpected final projection")
                })),
            })
            .is_ok()
    );
    for question in [
        "unknown transport",
        "known rejection",
        "known execution failure",
        "answer",
    ] {
        let result = runtime
            .route_foreign(
                root,
                "turn",
                question,
                foreign,
                AgentForeignOperation::SideQuestion {
                    question: question.to_owned(),
                },
                CancellationToken::new(),
            )
            .await;
        assert_eq!(result.is_ok(), question == "answer");
    }
    let facts = facts.lock().unwrap();
    assert!(facts.iter().all(|fact| fact.thread_id == root.to_string()));
    assert_eq!(
        facts
            .iter()
            .map(|fact| fact.event.clone())
            .collect::<Vec<_>>(),
        vec![
            FeatureUseEvent::SideQuestionRejected,
            FeatureUseEvent::MessageAccepted {
                sources: false,
                btw: true
            },
            FeatureUseEvent::SideQuestionFailed,
            FeatureUseEvent::MessageAccepted {
                sources: false,
                btw: true
            },
        ]
    );
    assert!(
        runtime
            .registry
            .agent_metadata_for_thread(foreign)
            .is_none()
    );
    Ok(())
}

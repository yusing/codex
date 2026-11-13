use super::*;
use crate::agent::control::work::DeliveryKind;
use crate::agent::control::work::PublishDelivery;
use crate::agent::control::work::WorkDisposition;
use crate::agent::control::work::WorkLedger;
use crate::agent::control::work::WorkStamp;
use crate::agent::types::AgentMetadata;
use crate::thread_manager::default_thread_id_generator;
use codex_state::coordination_wait::WaitCell;
use codex_state::coordination_wait::WaitMode;
use codex_utils_absolute_path::test_support::PathExt;
use std::sync::Weak;

fn register(runtime: &LocalAgentRuntime, path: &str) -> ThreadId {
    let actor = ThreadId::new();
    runtime
        .registry
        .reserve_spawn_slot(None)
        .unwrap()
        .commit(AgentMetadata {
            agent_id: Some(actor),
            agent_path: Some(AgentPath::try_from(path).unwrap()),
            ..Default::default()
        });
    actor
}

async fn admit(ledger: &WorkLedger, root: ThreadId, actor: ThreadId) -> anyhow::Result<WorkStamp> {
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
        .confirm_delivery(actor, &[stamp.work_id], dispatch, "initial")
        .await?;
    ledger
        .consume_deliveries(actor, &[stamp.work_id], "initial")
        .await?;
    Ok(stamp)
}

async fn terminal(
    ledger: &WorkLedger,
    work: WorkStamp,
    recipient: ThreadId,
) -> anyhow::Result<Uuid> {
    let candidate = ledger
        .stage_completion(
            work,
            "terminal-turn",
            recipient,
            WorkDisposition::Completed,
            None,
        )
        .await?
        .unwrap();
    ledger
        .complete_capture(work, candidate.delivery_id, None)
        .await?;
    ledger
        .completion_settlement(work, candidate.delivery_id, true)
        .await?;
    ledger
        .finish_completion(
            work,
            candidate.delivery_id,
            &serde_json::to_string(&CoordinationPayload::Terminal {
                status: AgentStatus::Completed(None),
                answers: vec!["first answer".into(), "second answer".into()],
                capture: Some(codex_state::coordination_progress::FrozenFinalCapture {
                    resource_ref: "frozen-resource".into(),
                    full_text: "frozen final capture".into(),
                }),
            })?,
        )
        .await?
        .expect("selected fixture work is complete");
    Ok(candidate.delivery_id)
}

fn cell() -> WaitCell {
    WaitCell {
        turn_id: "turn".into(),
        item_id: "item".into(),
        window_id: "window".into(),
        cell_id: "cell".into(),
    }
}

fn output(envelope: &ResponseItemEnvelope) -> (&CodexHarnessMetadata, serde_json::Value) {
    let ResponseItem::FunctionCallOutput { output, .. } = &envelope.item else {
        panic!("tool output")
    };
    (
        envelope.metadata.as_ref().unwrap(),
        serde_json::from_str(&output.text_content().unwrap()).unwrap(),
    )
}

#[tokio::test]
async fn exhausted_turn_continuation_is_a_harness_message_not_a_fabricated_tool_call()
-> anyhow::Result<()> {
    use codex_state::coordination_wait::TurnWaitReason;
    use codex_state::coordination_wait::WaitReadiness;
    let directory = tempfile::tempdir()?;
    let state = codex_state::StateRuntime::init(
        codex_state::SqliteConfig::new_for_testing(directory.path().abs()),
        "test-provider".into(),
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
    let ledger = runtime.work_ledger()?;
    let caller = admit(ledger, root, root).await?;
    ledger.bind_turn(caller, "failed-request").await?;
    let mut ticket = ledger
        .commit_turn_wait(
            caller,
            "failed-request",
            TurnWaitReason::ProviderCapacity,
            Some(10),
            20,
        )
        .await?
        .unwrap();
    ticket.claim = ledger
        .claim_ready_wait_at(caller, ticket.id, WaitReadiness { deadline_due: true })
        .await?;
    let envelope = runtime.wait_continuation_input(&ticket).await?;
    let ResponseItem::Message { role, content, .. } = &envelope.item else {
        panic!("not a function output")
    };
    assert_eq!(role, "developer");
    let metadata = envelope.metadata.as_ref().unwrap();
    assert_eq!(
        metadata.coordination_wait_claim_id,
        ticket.claim.as_ref().map(|claim| claim.id)
    );
    assert!(metadata.coordination_delivery_ids.is_empty());
    let projection = metadata.coordination.as_ref().unwrap();
    assert!(projection.deliveries.is_empty());
    let settlement = projection.wait.as_ref().unwrap();
    assert_eq!(
        settlement.result,
        CoordinationWaitResult::Released {
            wake: CoordinationWaitWake::Deadline
        }
    );
    assert_eq!(
        settlement.origin,
        CoordinationWaitOrigin::Turn {
            turn_id: "failed-request".into(),
            reason: CoordinationTurnWaitReason::ProviderCapacity,
        }
    );
    assert_eq!(settlement.caller_work_id, caller.work_id);
    let codex_protocol::models::ContentItem::InputText { text } = &content[0] else {
        panic!("text")
    };
    assert!(text.contains("provider_capacity"));
    assert!(text.contains("deadline"));
    assert!(metadata.history_truncation_token_limit.is_some());
    Ok(())
}

#[tokio::test]
async fn transitive_only_result_has_claim_proof_without_stealing_intermediate_mail()
-> anyhow::Result<()> {
    let directory = tempfile::tempdir()?;
    let state = codex_state::StateRuntime::init(
        codex_state::SqliteConfig::new_for_testing(directory.path().abs()),
        "test-provider".into(),
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
    let branch = register(&runtime, "/root/branch");
    let leaf = register(&runtime, "/root/branch/leaf");
    let ledger = runtime.work_ledger()?;
    let caller = admit(ledger, root, root).await?;
    admit(ledger, root, branch).await?;
    let leaf_work = admit(ledger, root, leaf).await?;
    let notification = terminal(ledger, leaf_work, branch).await?;
    let mut ticket = ledger.register_wait(caller, &cell(), WaitMode::Any).await?;
    ticket.claim = ledger.claim_ready_wait(caller, ticket.id).await?;
    let claim = ticket.claim.as_ref().unwrap();
    assert!(claim.delivery_ids.is_empty());
    let input = runtime.wait_continuation_input(&ticket).await?;
    let (metadata, value) = output(&input);
    assert_eq!(metadata.coordination_wait_claim_id, Some(claim.id));
    assert!(metadata.coordination_delivery_ids.is_empty());
    let projection = metadata.coordination.as_ref().unwrap();
    assert!(projection.deliveries.is_empty());
    let settlement = projection.wait.as_ref().unwrap();
    assert_eq!(
        settlement
            .selected
            .iter()
            .map(|target| target.work_id)
            .collect::<Vec<_>>(),
        ticket
            .selected
            .iter()
            .map(|work| work.work_id)
            .collect::<Vec<_>>()
    );
    assert_eq!(
        settlement.outcomes,
        vec![CoordinationWaitOutcome {
            target: CoordinationTarget {
                thread_id: leaf,
                task_name: AgentPath::try_from("/root/branch/leaf").unwrap(),
                work_id: leaf_work.work_id
            },
            disposition: CoordinationWaitDisposition::Completed,
        }]
    );
    assert_eq!(value["outcomes"][0]["thread_id"], leaf.to_string());
    assert_eq!(value["outcomes"][0]["task_name"], "/root/branch/leaf");
    assert!(
        value["outcomes"][0]["answers"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    assert!(value["outcomes"][0]["capture"].is_null());
    assert!(value["messages"].as_array().unwrap().is_empty());
    runtime.finish_awaited_wait(&ticket, Uuid::now_v7()).await?;
    let intermediate = ledger
        .observe_delivery(branch, notification)
        .await?
        .unwrap();
    assert!(!intermediate.consumed);
    assert_eq!(intermediate.claim_id, None);
    Ok(())
}

#[tokio::test]
async fn addressed_source_message_wins_without_delivering_an_unselected_final() -> anyhow::Result<()>
{
    let directory = tempfile::tempdir()?;
    let state = codex_state::StateRuntime::init(
        codex_state::SqliteConfig::new_for_testing(directory.path().abs()),
        "test-provider".into(),
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
    let child = register(&runtime, "/root/child");
    let ledger = runtime.work_ledger()?;
    let caller = admit(ledger, root, root).await?;
    let child_work = admit(ledger, root, child).await?;
    let final_id = terminal(ledger, child_work, root).await?;
    let message_id = Uuid::now_v7();
    let content = "Original @source.txt\nReferenced sources captured when sent:\nFROZEN_BYTES\nCONTENT NOT ATTACHED: denied";
    let communication = crate::agent::types::AgentMessage::Plaintext(content.into())
        .into_communication(
            AgentPath::try_from("/root/child").unwrap(),
            AgentPath::root(),
            crate::agent::types::MessageDeliveryMode::QueueOnly,
        );
    ledger
        .publish_delivery(&PublishDelivery {
            id: message_id,
            root_id: root,
            sender_id: Some(child),
            recipient_id: root,
            work: None,
            kind: DeliveryKind::Message,
            payload: serde_json::to_string(&CoordinationPayload::Message { communication })?,
        })
        .await?;
    let mut ticket = ledger
        .register_wait_with_options(
            caller,
            &cell(),
            &codex_state::coordination_wait::WaitOptions {
                mode: WaitMode::All,
                compact: true,
                ..Default::default()
            },
        )
        .await?;
    ledger.begin_wait_reset_origin(&ticket).await?;
    ticket = ledger
        .finish_wait_reset_origin(&ticket, Some("source acquisition failed"))
        .await?;
    ticket.claim = ledger.claim_ready_wait(caller, ticket.id).await?;
    let input = runtime.wait_continuation_input(&ticket).await?;
    let (metadata, value) = output(&input);
    assert_eq!(metadata.coordination_delivery_ids, vec![message_id]);
    let settlement = metadata
        .coordination
        .as_ref()
        .unwrap()
        .wait
        .as_ref()
        .unwrap();
    assert_eq!(
        settlement.messages,
        vec![CoordinationWaitMessage {
            delivery_id: message_id,
            sender_thread_id: Some(child),
            sender_task_name: Some(AgentPath::try_from("/root/child").unwrap()),
            recipient: AgentPath::root(),
            body: content.into(),
        }]
    );
    assert!(
        metadata
            .coordination
            .as_ref()
            .unwrap()
            .deliveries
            .is_empty()
    );
    assert_eq!(settlement.messages.len(), 1);
    let record = ledger.observe_delivery(root, message_id).await?.unwrap();
    let TurnInput::InterAgentCommunication(ordinary) =
        super::super::work_lifecycle::delivery_input(
            &record,
            Some(AgentPath::try_from("/root/child").unwrap()),
            AgentPath::root(),
        )?
    else {
        panic!("ordinary addressed message")
    };
    assert!(ordinary.content.ends_with(&settlement.messages[0].body));
    assert!(ordinary.content.starts_with("Message Type: MESSAGE"));
    assert!(!settlement.messages[0].body.starts_with("Message Type:"));
    assert_eq!(settlement.mode, CoordinationWaitMode::All);
    assert!(settlement.outcomes.is_empty());
    assert_eq!(
        value["messages"][0]["payload"]["communication"]["content"],
        ordinary.content
    );
    assert_eq!(value["reset"]["status"], "failed");
    assert_eq!(value["reset"]["diagnostic"], "source acquisition failed");
    assert_eq!(
        value["metrics"]["/root/child"],
        serde_json::json!({
            "contextTokens": null, "contextWindow": null, "completedCompactions": null,
        })
    );
    assert!(value["outcomes"].as_array().unwrap().is_empty());
    assert!(
        !ledger
            .observe_delivery(root, final_id)
            .await?
            .unwrap()
            .consumed
    );
    assert_eq!(runtime.wait_continuation_input(&ticket).await?, input);
    let frozen = ledger
        .observe_wait(caller, ticket.id)
        .await?
        .unwrap()
        .claim
        .unwrap()
        .metrics
        .unwrap();
    assert_eq!(frozen[&child.to_string()], AgentMetrics::default());
    let later = BTreeMap::from([(
        child.to_string(),
        AgentMetrics {
            context_tokens: Some(10),
            context_window: Some(20),
            completed_compactions: Some(2),
        },
    )]);
    assert_eq!(
        ledger
            .freeze_wait_metrics(caller, ticket.id, ticket.claim.as_ref().unwrap().id, &later)
            .await?,
        frozen
    );
    assert!(
        ledger
            .freeze_wait_metrics(caller, ticket.id, Uuid::now_v7(), &later)
            .await
            .is_err()
    );
    assert_eq!(runtime.wait_continuation_input(&ticket).await?, input);
    Ok(())
}

#[tokio::test]
async fn direct_final_delivers_ordered_text_and_capture_only_when_claimed() -> anyhow::Result<()> {
    for already_consumed in [false, true] {
        let directory = tempfile::tempdir()?;
        let state = codex_state::StateRuntime::init(
            codex_state::SqliteConfig::new_for_testing(directory.path().abs()),
            "test-provider".into(),
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
        let child = register(&runtime, "/root/child");
        let ledger = runtime.work_ledger()?;
        let caller = admit(ledger, root, root).await?;
        let child_work = admit(ledger, root, child).await?;
        let mut ticket = ledger.register_wait(caller, &cell(), WaitMode::All).await?;
        let final_id = terminal(ledger, child_work, root).await?;
        if already_consumed {
            let dispatch = Uuid::now_v7();
            ledger
                .begin_delivery(root, &[final_id], None, dispatch)
                .await?;
            ledger
                .confirm_delivery(root, &[final_id], dispatch, "other-input")
                .await?;
            ledger
                .consume_deliveries(root, &[final_id], "other-input")
                .await?;
        }
        ticket.claim = ledger.claim_ready_wait(caller, ticket.id).await?;
        let input = runtime.wait_continuation_input(&ticket).await?;
        let (metadata, value) = output(&input);
        let outcome = &value["outcomes"][0];
        let projection = metadata.coordination.as_ref().unwrap();
        assert_eq!(projection.wait.as_ref().unwrap().outcomes.len(), 1);
        if already_consumed {
            assert!(
                projection.deliveries.is_empty(),
                "already-read final stays status-only"
            );
            assert!(metadata.coordination_delivery_ids.is_empty());
            assert!(outcome["answers"].as_array().unwrap().is_empty());
            assert!(outcome["capture"].is_null());
        } else {
            assert_eq!(
                projection.deliveries,
                vec![CoordinationTerminalDelivery {
                    delivery_id: final_id,
                    source: CoordinationTarget {
                        thread_id: child,
                        task_name: AgentPath::try_from("/root/child").unwrap(),
                        work_id: child_work.work_id
                    },
                    recipient: AgentPath::root(),
                    disposition: CoordinationDisposition::Completed,
                    body: "first answer\n\nsecond answer\n\nfrozen final capture".into(),
                    capture_resource_ref: Some("frozen-resource".into()),
                }]
            );
            assert_eq!(metadata.coordination_delivery_ids, vec![final_id]);
            assert_eq!(
                outcome["answers"],
                serde_json::json!(["first answer", "second answer"])
            );
            assert_eq!(outcome["capture"], "frozen final capture");
        }
    }
    Ok(())
}

#[tokio::test]
async fn idle_wait_interruption_closes_with_the_actual_terminal_delivery() -> anyhow::Result<()> {
    let directory = tempfile::tempdir()?;
    let state = codex_state::StateRuntime::init(
        codex_state::SqliteConfig::new_for_testing(directory.path().abs()),
        "test-provider".into(),
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
    let child = register(&runtime, "/root/child");
    let ledger = runtime.work_ledger()?;
    let caller = admit(ledger, root, root).await?;
    admit(ledger, root, child).await?;
    let wait = ledger.register_wait(caller, &cell(), WaitMode::All).await?;
    let events = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    assert!(
        runtime
            .host_callbacks
            .set(capture_coordination(std::sync::Arc::clone(&events)))
            .is_ok()
    );
    runtime.interrupt_idle_work(caller).await?;
    let closed = ledger.observe_wait(caller, wait.id).await?.unwrap();
    let terminal = ledger.terminal_delivery(caller).await?.unwrap();
    assert_eq!(
        closed.cancellation,
        Some(
            codex_state::coordination_wait::WaitCancellation::WorkTerminated {
                delivery_id: terminal.publication.id,
            }
        )
    );
    let payload: CoordinationPayload = serde_json::from_str(&terminal.publication.payload)?;
    assert!(matches!(
        payload,
        CoordinationPayload::Terminal {
            status: AgentStatus::Interrupted,
            ..
        }
    ));
    assert_eq!(closed.origin, wait.origin);
    assert_eq!(closed.options.mode, WaitMode::All);
    let events = events.lock().unwrap();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].thread_id, root);
    assert_eq!(events[0].turn_id, wait.origin.turn_id());
    let settlement = events[0].projection.wait.as_ref().unwrap();
    assert_eq!(settlement.wait_id, wait.id);
    assert_eq!(settlement.caller_work_id, caller.work_id);
    assert_eq!(
        settlement.result,
        CoordinationWaitResult::Cancelled {
            cause: CoordinationWaitCancellation::WorkTerminated {
                delivery_id: terminal.publication.id
            },
        }
    );
    assert_eq!(settlement.mode, CoordinationWaitMode::All);
    assert_eq!(settlement.selected.len(), 1);
    assert!(settlement.claim_id.is_none());
    Ok(())
}

fn capture_coordination(
    events: std::sync::Arc<std::sync::Mutex<Vec<AgentCoordinationEvent>>>,
) -> crate::AgentHostCallbacks {
    use std::sync::Arc;
    crate::AgentHostCallbacks {
        host_connection_id: 1,
        publish_coordination: Arc::new(move |event| {
            events.lock().unwrap().push(event);
            Box::pin(async {})
        }),
        route_foreign: Arc::new(|_, _| Box::pin(async { Err(work_error("unused route")) })),
        prepare_final_capture: Arc::new(|_, _| {
            Box::pin(async { Err(work_error("unused capture")) })
        }),
        admit_child: Arc::new(|_, _| Box::pin(async { Err(work_error("unused admission")) })),
        freeze_progress: Arc::new(|_, _| Box::pin(async { Err(work_error("unused progress")) })),
        prepare_progress_output: Arc::new(|_, _| {
            Box::pin(async { Err(work_error("unused progress")) })
        }),
        project_final_capture: Arc::new(|_, _| {
            Box::pin(async { Err(work_error("unused projection")) })
        }),
    }
}

#[tokio::test]
async fn foreign_wait_projection_retains_original_sender_not_local_forwarder() -> anyhow::Result<()>
{
    let directory = tempfile::tempdir()?;
    let state = codex_state::StateRuntime::init(
        codex_state::SqliteConfig::new_for_testing(directory.path().abs()),
        "test-provider".into(),
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
    let ledger = runtime.work_ledger()?;
    let caller = admit(ledger, root, root).await?;
    let origin = codex_protocol::agent_foreign::AgentForeignOrigin {
        root_thread_id: ThreadId::new(),
        actor_id: ThreadId::new(),
        agent_path: AgentPath::root().join("foreign").unwrap(),
        turn_id: "foreign-turn".into(),
        call_id: "foreign-call".into(),
    };
    let id = Uuid::now_v7();
    ledger
        .publish_delivery(&PublishDelivery {
            id,
            root_id: root,
            sender_id: Some(root),
            recipient_id: root,
            work: None,
            kind: DeliveryKind::Message,
            payload: serde_json::to_string(&CoordinationPayload::ForeignMessage {
                origin: origin.clone(),
                original_message: "original @source.rs text".into(),
                frozen_sources: Some("FROZEN_SOURCE_BYTES\nCONTENT NOT ATTACHED: denied".into()),
            })?,
        })
        .await?;
    let mut ticket = ledger
        .register_wait_at(caller, &cell(), &Default::default(), 0)
        .await?;

    ticket.claim = ledger.claim_ready_wait(caller, ticket.id).await?;
    let envelope = runtime.wait_continuation_input(&ticket).await?;
    let settlement = envelope
        .metadata
        .as_ref()
        .unwrap()
        .coordination
        .as_ref()
        .unwrap()
        .wait
        .as_ref()
        .unwrap();
    let record = ledger.observe_delivery(root, id).await?.unwrap();
    let TurnInput::InterAgentCommunication(ordinary) =
        super::super::work_lifecycle::delivery_input(
            &record,
            Some(origin.agent_path.clone()),
            AgentPath::root(),
        )?
    else {
        panic!("ordinary foreign message")
    };
    assert_eq!(
        settlement.messages,
        vec![CoordinationWaitMessage {
            delivery_id: id,
            sender_thread_id: Some(origin.actor_id),
            sender_task_name: Some(origin.agent_path.clone()),
            recipient: AgentPath::root(),
            body: super::super::foreign::attributed_foreign_text(
                &origin,
                "original @source.rs text\n\nReferenced sources captured when sent:\nFROZEN_SOURCE_BYTES\nCONTENT NOT ATTACHED: denied"
            ),
        }]
    );
    assert_eq!(settlement.messages.len(), 1);
    assert!(ordinary.content.starts_with("Message Type: MESSAGE"));
    assert!(ordinary.content.ends_with(&settlement.messages[0].body));
    assert!(!settlement.messages[0].body.starts_with("Message Type:"));
    assert!(
        settlement.messages[0]
            .body
            .contains("original @source.rs text")
    );
    assert!(settlement.messages[0].body.contains("FROZEN_SOURCE_BYTES"));
    assert!(
        settlement.messages[0]
            .body
            .contains("CONTENT NOT ATTACHED: denied")
    );
    assert!(
        envelope
            .metadata
            .as_ref()
            .unwrap()
            .coordination
            .as_ref()
            .unwrap()
            .deliveries
            .is_empty()
    );
    assert!(settlement.outcomes.is_empty());
    assert_eq!(
        settlement.origin,
        CoordinationWaitOrigin::Cell {
            turn_id: cell().turn_id,
            item_id: cell().item_id,
            window_id: cell().window_id,
            cell_id: cell().cell_id,
        }
    );
    let events = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    assert!(
        runtime
            .host_callbacks
            .set(capture_coordination(std::sync::Arc::clone(&events)))
            .is_ok()
    );
    runtime.cancel_native_wait(&ticket).await?;
    let events = events.lock().unwrap();
    let cancelled = events[0].projection.wait.as_ref().unwrap();
    assert_eq!(
        cancelled.claim_id,
        ticket.claim.as_ref().map(|claim| claim.id)
    );
    assert_eq!(
        cancelled.result,
        CoordinationWaitResult::Cancelled {
            cause: CoordinationWaitCancellation::Cancelled
        }
    );
    assert!(cancelled.messages.is_empty());
    assert!(events[0].projection.deliveries.is_empty());
    drop(events);
    assert!(!ledger.observe_delivery(root, id).await?.unwrap().consumed);
    Ok(())
}

#[tokio::test]
async fn unread_wait_final_bodies_follow_publication_order_with_failure_and_interruption()
-> anyhow::Result<()> {
    let directory = tempfile::tempdir()?;
    let state = codex_state::StateRuntime::init(
        codex_state::SqliteConfig::new_for_testing(directory.path().abs()),
        "test-provider".into(),
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
    let left = register(&runtime, "/root/left");
    let right = register(&runtime, "/root/right");
    let ledger = runtime.work_ledger()?;
    let caller = admit(ledger, root, root).await?;
    let left = admit(ledger, root, left).await?;
    let right = admit(ledger, root, right).await?;
    let mut wait = ledger.register_wait(caller, &cell(), WaitMode::All).await?;
    let mut expected = Vec::new();
    // Publish opposite to path order, preserving exactly the selected work IDs.
    for (work, status, disposition) in [
        (
            right,
            AgentStatus::Interrupted,
            WorkDisposition::Interrupted,
        ),
        (
            left,
            AgentStatus::Errored("provider failed".into()),
            WorkDisposition::Failed,
        ),
    ] {
        let candidate = ledger
            .stage_completion(work, "terminal-turn", root, disposition, None)
            .await?
            .unwrap();
        let id = candidate.delivery_id;
        ledger.completion_settlement(work, id, true).await?;
        ledger
            .finish_completion(
                work,
                id,
                &serde_json::to_string(&CoordinationPayload::Terminal {
                    status,
                    answers: Vec::new(),
                    capture: None,
                })?,
            )
            .await?
            .unwrap();
        let record = ledger.observe_delivery(root, id).await?.unwrap();
        let path = runtime
            .ensure_agent_known(work.actor_id)?
            .agent_path
            .unwrap();
        let TurnInput::InterAgentCommunication(message) =
            super::super::work_lifecycle::delivery_input(&record, Some(path), AgentPath::root())?
        else {
            panic!("terminal message")
        };
        expected.push(message.coordination().unwrap().deliveries[0].clone());
    }
    wait.claim = ledger.claim_ready_wait(caller, wait.id).await?;
    let input = runtime.wait_continuation_input(&wait).await?;
    let (metadata, body) = output(&input);
    let projection = metadata.coordination.as_ref().unwrap();
    assert_eq!(
        projection.deliveries, expected,
        "wait and ordinary terminal messages share native payload rendering"
    );
    assert_eq!(projection.deliveries[0].source.work_id, right.work_id);
    assert_eq!(projection.deliveries[0].body, "Agent work settled.");
    assert_eq!(
        projection.deliveries[0].disposition,
        CoordinationDisposition::Interrupted
    );
    assert_eq!(projection.deliveries[1].source.work_id, left.work_id);
    assert!(
        projection.deliveries[1]
            .body
            .starts_with("Agent errored: provider failed")
    );
    assert_eq!(
        projection.deliveries[1].disposition,
        CoordinationDisposition::Failed
    );
    assert_eq!(projection.wait.as_ref().unwrap().outcomes.len(), 2);
    assert_eq!(body["outcomes"].as_array().unwrap().len(), 2);
    assert!(body["messages"].as_array().unwrap().is_empty());
    Ok(())
}

#[tokio::test]
async fn paused_claim_materializes_identical_model_and_ui_stop_without_a_final_body()
-> anyhow::Result<()> {
    for before in [false, true] {
        for mode in [WaitMode::Any, WaitMode::All] {
            let directory = tempfile::tempdir()?;
            let state = codex_state::StateRuntime::init(
                codex_state::SqliteConfig::new_for_testing(directory.path().abs()),
                "test-provider".into(),
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
            let child = register(&runtime, "/root/paused");
            let ledger = runtime.work_ledger()?;
            let caller = admit(ledger, root, root).await?;
            let target = admit(ledger, root, child).await?;
            if before {
                let work = ledger.observe_work(target).await?.unwrap();
                ledger
                    .transition_work(target, work.revision, WorkDisposition::Paused)
                    .await?;
            }
            let mut wait = ledger.register_wait(caller, &cell(), mode).await?;
            if !before {
                assert!(ledger.claim_ready_wait(caller, wait.id).await?.is_none());
                let work = ledger.observe_work(target).await?.unwrap();
                ledger
                    .transition_work(target, work.revision, WorkDisposition::Paused)
                    .await?;
                runtime.notify_work_paused(target).await?;
            }
            wait.claim = ledger.claim_ready_wait(caller, wait.id).await?;
            let input = runtime.wait_continuation_input(&wait).await?;
            let (metadata, model) = output(&input);
            let projection = metadata.coordination.as_ref().unwrap();
            assert!(projection.deliveries.is_empty());
            assert!(metadata.coordination_delivery_ids.is_empty());
            assert_eq!(model["outcomes"][0]["status"], "paused");
            assert_eq!(model["outcomes"][0]["work_id"], target.work_id.to_string());
            assert!(
                model["outcomes"][0]["answers"]
                    .as_array()
                    .unwrap()
                    .is_empty()
            );
            assert!(model["outcomes"][0]["capture"].is_null());
            let settlement = projection.wait.as_ref().unwrap();
            assert_eq!(
                settlement.result,
                CoordinationWaitResult::Released {
                    wake: CoordinationWaitWake::Events
                }
            );
            assert_eq!(
                settlement.outcomes,
                vec![CoordinationWaitOutcome {
                    target: CoordinationTarget {
                        thread_id: child,
                        task_name: AgentPath::try_from("/root/paused").unwrap(),
                        work_id: target.work_id
                    },
                    disposition: CoordinationWaitDisposition::Paused,
                }]
            );
            let work = ledger.observe_work(target).await?.unwrap();
            assert!(work.terminal_delivery_id.is_none());
            ledger
                .transition_work(target, work.revision, WorkDisposition::Running)
                .await?;
            let frozen = ledger.observe_wait(caller, wait.id).await?.unwrap();
            assert_eq!(runtime.wait_continuation_input(&frozen).await?, input);
        }
    }
    Ok(())
}

#[tokio::test]
async fn successful_completion_guard_names_active_transitive_work_and_allows_explicit_wait()
-> anyhow::Result<()> {
    let directory = tempfile::tempdir()?;
    let state = codex_state::StateRuntime::init(
        codex_state::SqliteConfig::new_for_testing(directory.path().abs()),
        "test-provider".into(),
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
    let worker = register(&runtime, "/root/worker");
    let nested = register(&runtime, "/root/worker/nested");
    let paused = register(&runtime, "/root/paused");
    let uncertain = register(&runtime, "/root/uncertain");
    let ledger = runtime.work_ledger()?;
    let caller = admit(ledger, root, root).await?;
    admit(ledger, root, worker).await?;
    admit(ledger, root, nested).await?;
    for (actor, disposition) in [
        (paused, WorkDisposition::Paused),
        (uncertain, WorkDisposition::Uncertain),
    ] {
        let work = admit(ledger, root, actor).await?;
        let current = ledger.observe_work(work).await?.unwrap();
        ledger
            .transition_work(work, current.revision, disposition)
            .await?;
    }
    ledger.bind_turn(caller, "premature-final").await?;
    let checkpoint = runtime
        .prepare_successful_completion(root, "premature-final", Some("PARENT_PREMATURE_FINAL"))
        .await?
        .unwrap();
    runtime.prepare_completion_capture(&checkpoint).await?;
    assert!(matches!(
        runtime.settle_completion_checkpoint(&checkpoint).await?,
        super::super::recovery::CompletionSettlement::Pending
    ));
    let input = runtime
        .begin_completion_continuation(&checkpoint, Some("guard-turn"))
        .await?
        .unwrap();
    let TurnInput::ResponseItem(envelope) = &input else {
        panic!("native harness feedback")
    };
    let ResponseItem::Message { role, content, .. } = &envelope.item else {
        panic!("feedback message")
    };
    assert_eq!(role, "developer");
    let ContentItem::InputText { text } = &content[0] else {
        panic!("feedback text")
    };
    assert_eq!(
        text,
        &include_str!("../../../templates/agents/completion_guard.md")
            .replace("{paths}", "/root/worker, /root/worker/nested")
    );
    let ids = super::super::work_lifecycle::input_delivery_ids(&input);
    assert_eq!(ids.len(), 1);
    let delivery = ledger.observe_delivery(root, ids[0]).await?.unwrap();
    assert_eq!(delivery.publication.work, Some(caller));
    assert_eq!(delivery.accepted_receipt.as_deref(), Some("guard-turn"));
    assert!(
        runtime
            .begin_completion_continuation(&checkpoint, Some("duplicate"))
            .await?
            .is_none()
    );
    assert_eq!(
        ledger.work_for_turn(root, "guard-turn").await?,
        Some(caller)
    );
    ledger.consume_deliveries(root, ids, "guard-turn").await?;
    let wait = ledger.register_wait(caller, &cell(), WaitMode::All).await?;
    assert_eq!(wait.caller, caller);
    assert!(wait.selected.iter().any(|work| work.actor_id == worker));
    assert!(wait.selected.iter().any(|work| work.actor_id == nested));
    Ok(())
}

#[tokio::test]
async fn paused_blocker_releases_original_completion_without_guard_input() -> anyhow::Result<()> {
    let directory = tempfile::tempdir()?;
    let state = codex_state::StateRuntime::init(
        codex_state::SqliteConfig::new_for_testing(directory.path().abs()),
        "test-provider".into(),
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
    let worker = register(&runtime, "/root/worker");
    let ledger = runtime.work_ledger()?;
    let caller = admit(ledger, root, root).await?;
    let child = admit(ledger, root, worker).await?;
    ledger.bind_turn(caller, "premature-final").await?;
    let checkpoint = runtime
        .prepare_successful_completion(root, "premature-final", Some("original final"))
        .await?
        .unwrap();
    runtime.prepare_completion_capture(&checkpoint).await?;
    runtime.settle_completion_checkpoint(&checkpoint).await?;
    let current = ledger.observe_work(child).await?.unwrap();
    ledger
        .transition_work(child, current.revision, WorkDisposition::Paused)
        .await?;
    runtime.notify_work_paused(child).await?;
    let terminal = ledger.terminal_delivery(caller).await?.unwrap();
    assert_eq!(terminal.publication.id, checkpoint.delivery_id);
    let CoordinationPayload::Terminal { answers, .. } =
        serde_json::from_str(&terminal.publication.payload)?
    else {
        panic!("original terminal")
    };
    assert_eq!(answers, vec!["original final"]);
    assert!(
        runtime
            .begin_completion_continuation(&checkpoint, Some("unused"))
            .await?
            .is_none()
    );
    assert_eq!(
        ledger.observe_work(child).await?.unwrap().disposition,
        WorkDisposition::Paused
    );
    Ok(())
}

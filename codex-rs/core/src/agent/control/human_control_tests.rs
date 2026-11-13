use super::*;
use crate::agent::control::work::DeliveryKind;
use crate::agent::control::work::PublishDelivery;
use crate::agent::control::work::WorkDisposition;
use crate::agent::control::work::WorkStamp;
use crate::agent::control::work::work_wait::WaitCell;
use crate::agent::control::work::work_wait::WaitMode;
use pretty_assertions::assert_eq;
use uuid::Uuid;

async fn native_harness() -> AgentControlHarness {
    let (home, mut config) = test_config().await;
    let _ = config.features.enable(Feature::MultiAgentV2);
    let _ = config.features.enable(Feature::Sqlite);
    config.multi_agent_v2.max_concurrent_threads_per_session = Some(8);
    AgentControlHarness::new_with_config(home, config).await
}

struct PreserveSideWait {
    started: Arc<tokio::sync::Notify>,
    ticket: codex_state::coordination_wait::WaitTicket,
}

impl crate::tasks::SessionTask for PreserveSideWait {
    fn kind(&self) -> crate::state::TaskKind {
        crate::state::TaskKind::Regular
    }

    fn span_name(&self) -> &'static str {
        "session_task.side_wait_test"
    }

    async fn run(
        self: Arc<Self>,
        session: Arc<crate::session::session::Session>,
        context: Arc<crate::session::turn_context::TurnContext>,
        _input: Vec<crate::session::TurnInput>,
        cancellation: CancellationToken,
    ) -> crate::tasks::SessionTaskResult {
        let step = session.capture_step_context(context, &cancellation).await?;
        self.started.notify_one();
        session
            .await_turn_wait(&step, &self.ticket, &cancellation)
            .await?;
        Ok(None)
    }
}

#[tokio::test]
async fn human_side_fork_awaits_its_own_root_and_interrupts_without_source_tree_enrollment() {
    use crate::session::TurnInput;
    use codex_state::coordination_wait::TurnWaitReason;
    use futures::FutureExt;

    let harness = native_harness().await;
    let (source_id, source) = harness.start_thread().await;
    source.ensure_rollout_materialized().await;
    source.flush_rollout().await.unwrap();
    let source_path = source.rollout_path().unwrap();
    let (history, _, _) = codex_rollout::RolloutRecorder::load_rollout_items(&source_path)
        .await
        .unwrap();
    let fork = harness
        .manager
        .fork_thread_from_history(
            ForkSnapshot::Interrupted,
            StartThreadOptions {
                thread_source: Some(codex_protocol::protocol::ThreadSource::Feature(
                    "mekugi_side".to_owned(),
                )),
                ..StartThreadOptions::new(harness.config.clone())
            },
            InitialHistory::Resumed(ResumedHistory {
                conversation_id: source_id,
                history_revision: None,
                history: Arc::new(history),
                rollout_path: Some(source_path),
            }),
        )
        .await
        .unwrap();
    let side = &fork.thread;
    let side_id = fork.thread_id;
    assert!(side.has_native_work_owner());
    assert!(!side.session_source.is_non_root_agent());
    assert!(fork.session_configured.parent_thread_id.is_none());
    assert_eq!(
        fork.session_configured
            .thread_source
            .as_ref()
            .map(|source| source.as_str()),
        Some("mekugi_side")
    );
    let side_runtime = &side.session.services.local_agent_runtime;
    assert_ne!(
        side_runtime.tree_id(),
        source.session.services.local_agent_runtime.tree_id()
    );
    assert_eq!(side_runtime.root_actor(), Some(side_id));
    assert_eq!(
        harness
            .manager
            .observe_agents(source_id)
            .await
            .unwrap()
            .agents
            .len(),
        1
    );
    assert_eq!(
        harness
            .manager
            .observe_agents(side_id)
            .await
            .unwrap()
            .agents
            .len(),
        1
    );

    // Exercise the normal human-input admission seam and retain the original
    // task while its own tree waits for provider recovery.
    let mut input = TurnInput::UserInput {
        content: text_input("continue independently"),
        client_id: None,
        metadata: Default::default(),
    };
    side.session
        .admit_coordination_input(&mut input, "side-human", true)
        .await
        .unwrap();
    side.session
        .confirm_coordination_input(&input, "side-human")
        .await
        .unwrap();
    let ledger = side_runtime.work_ledger().unwrap();
    let stamp = ledger.work_stamp(side_id).await.unwrap().unwrap();
    assert_eq!(stamp.root_id, side_id);
    let wait = ledger
        .commit_turn_wait(
            stamp,
            "side-human",
            TurnWaitReason::ProviderCapacity,
            None,
            chrono::Utc::now().timestamp_millis(),
        )
        .await
        .unwrap()
        .unwrap();
    let context = side.session.new_default_turn().await;
    let started = Arc::new(tokio::sync::Notify::new());
    side.session
        .spawn_task(
            context,
            Vec::new(),
            PreserveSideWait {
                started: Arc::clone(&started),
                ticket: wait.clone(),
            },
        )
        .await;
    timeout(Duration::from_secs(2), started.notified())
        .await
        .expect("original side task begins awaiting");
    {
        let active = side.session.active_turn.lock().await;
        assert!(
            !active
                .as_ref()
                .unwrap()
                .task
                .as_ref()
                .unwrap()
                .handle
                .is_finished()
        );
    }
    assert_eq!(
        ledger.current_wait(side_id).await.unwrap().unwrap().id,
        wait.id
    );
    while let Some(Ok(event)) = side.next_event().now_or_never() {
        assert!(!matches!(
            event.msg,
            EventMsg::TurnCheckpointed(_) | EventMsg::TurnComplete(_) | EventMsg::TurnAborted(_)
        ));
    }
    assert!(
        harness
            .manager
            .interrupt_agent(source_id, side_id)
            .await
            .is_err()
    );
    harness
        .manager
        .interrupt_agent(side_id, side_id)
        .await
        .unwrap();
    assert!(ledger.current_wait(side_id).await.unwrap().is_none());
    assert_eq!(
        ledger
            .observe_current(side_id)
            .await
            .unwrap()
            .unwrap()
            .disposition,
        WorkDisposition::Interrupted
    );
    assert!(source.is_running());
    assert!(side.is_running());
    assert!(
        source
            .session
            .services
            .local_agent_runtime
            .registry
            .agent_metadata_for_thread(side_id)
            .is_none()
    );
    harness
        .manager
        .request_agent_tree_shutdown(side_id)
        .await
        .unwrap()
        .wait()
        .await
        .unwrap();
    harness
        .manager
        .request_agent_tree_shutdown(source_id)
        .await
        .unwrap()
        .wait()
        .await
        .unwrap();
}

#[tokio::test]
async fn trusted_end_joins_only_selected_subtree_and_explicit_restore_preserves_identity() {
    let harness = native_harness().await;
    let (root_id, root) = harness.start_thread().await;
    let runtime = root.session.services.local_agent_runtime.tree_runtime();
    let control = runtime.control(root.session.session_id());
    let child = spawn_v2_reload_test_child(&control, harness.config.clone(), &root, "root").await;
    let child_thread = harness.manager.get_thread(child.thread_id).await.unwrap();
    let nested =
        spawn_v2_reload_test_child(&control, harness.config.clone(), &child_thread, "nested").await;
    let nested_thread = harness.manager.get_thread(nested.thread_id).await.unwrap();
    let sibling =
        spawn_v2_reload_test_child(&control, harness.config.clone(), &root, "sibling").await;
    let sibling_thread = harness.manager.get_thread(sibling.thread_id).await.unwrap();
    let metadata = runtime
        .registry
        .agent_metadata_for_thread(child.thread_id)
        .unwrap();
    assert_eq!(
        metadata.agent_path,
        Some(AgentPath::try_from("/root/root").unwrap())
    );
    assert_ne!(child.thread_id, root_id);
    let child_control = child_thread
        .session
        .services
        .local_agent_runtime
        .control(child_thread.session.session_id());
    assert!(
        child_control
            .interrupt_spawned_agent(child.thread_id, root_id)
            .await
            .is_err()
    );
    assert!(
        harness
            .manager
            .end_agent_subtree(child.thread_id, sibling.thread_id)
            .await
            .is_err()
    );
    assert!(
        runtime
            .for_actor_start(child.thread_id, Some(sibling.thread_id))
            .is_err()
    );
    assert_eq!(runtime.registered_parent(child.thread_id), Some(root_id));
    assert!(
        harness
            .manager
            .end_agent_subtree(root_id, root_id)
            .await
            .is_err()
    );
    assert!(
        harness
            .manager
            .end_agent_subtree(ThreadId::new(), child.thread_id)
            .await
            .is_err()
    );
    assert!(
        harness
            .manager
            .end_agent_subtree(root_id, ThreadId::new())
            .await
            .is_err()
    );
    assert!(root.is_running());
    let (ended, repeated) = tokio::join!(
        harness.manager.end_agent_subtree(root_id, child.thread_id),
        harness.manager.end_agent_subtree(root_id, child.thread_id),
    );
    let ended = ended.unwrap();
    assert_eq!(ended, repeated.unwrap());
    assert_eq!(
        ended.into_iter().collect::<std::collections::HashSet<_>>(),
        std::collections::HashSet::from([child.thread_id, nested.thread_id])
    );
    assert!(!child_thread.is_running());
    assert!(!nested_thread.is_running());
    assert!(root.is_running());
    assert!(sibling_thread.is_running());
    assert_thread_not_loaded(&harness.manager, child.thread_id).await;
    assert_thread_not_loaded(&harness.manager, nested.thread_id).await;
    assert_eq!(
        runtime
            .registry
            .agent_metadata_for_thread(child.thread_id)
            .unwrap()
            .agent_path,
        metadata.agent_path
    );
    assert!(runtime.registry.is_admitted(nested.thread_id));

    let pin = control
        .ensure_v2_agent_loaded(harness.config.clone(), child.thread_id, Some(root.clone()))
        .await
        .expect("explicit restore is permitted after actual end settlement");
    let restored = harness.manager.get_thread(child.thread_id).await.unwrap();
    assert!(restored.is_running());
    assert!(
        !restored
            .session
            .services
            .local_agent_runtime
            .shutdown
            .is_cancelled()
    );
    assert!(
        child_thread
            .session
            .services
            .local_agent_runtime
            .shutdown
            .is_cancelled()
    );
    assert_eq!(
        restored.session.services.local_agent_runtime.tree_id(),
        runtime.tree_id()
    );
    drop(pin);
    harness
        .manager
        .request_agent_tree_shutdown(root_id)
        .await
        .unwrap()
        .wait()
        .await
        .unwrap();
}

#[tokio::test]
async fn trusted_root_interrupt_settles_detached_logical_wait_without_a_host_turn() {
    let harness = native_harness().await;
    let (root_id, root) = harness.start_thread().await;
    let runtime = root.session.services.local_agent_runtime.tree_runtime();
    let child = ThreadId::new();
    runtime
        .registry
        .reserve_spawn_slot(None)
        .unwrap()
        .commit(AgentMetadata {
            agent_id: Some(child),
            agent_path: Some(AgentPath::root().join("waiting-child").unwrap()),
            ..Default::default()
        });
    let ledger = runtime.work_ledger().unwrap();
    let mut stamps = Vec::new();
    for actor in [root_id, child] {
        let stamp = WorkStamp {
            root_id,
            actor_id: actor,
            work_id: Uuid::now_v7(),
        };
        let input = PublishDelivery {
            id: stamp.work_id,
            root_id,
            sender_id: None,
            recipient_id: actor,
            work: Some(stamp),
            kind: DeliveryKind::Message,
            payload: "accepted assignment".to_owned(),
        };
        ledger.admit_work(stamp, None, &input).await.unwrap();
        let dispatch = input.id;
        ledger
            .begin_delivery(actor, &[input.id], None, dispatch)
            .await
            .unwrap();
        ledger
            .confirm_delivery(actor, &[input.id], dispatch, "accepted")
            .await
            .unwrap();
        ledger
            .consume_deliveries(actor, &[input.id], "accepted")
            .await
            .unwrap();
        stamps.push(stamp);
    }
    let cell = WaitCell {
        turn_id: "detached".to_owned(),
        item_id: "wait".to_owned(),
        window_id: "window".to_owned(),
        cell_id: "cell".to_owned(),
    };
    ledger
        .register_wait(stamps[0], &cell, WaitMode::All)
        .await
        .unwrap();
    let control = runtime.control(root.session.session_id());
    assert!(
        control
            .interrupt_spawned_agent(child, root_id)
            .await
            .is_err()
    );
    assert!(control.interrupt_spawned_agent(child, child).await.is_err());
    assert!(
        harness
            .manager
            .interrupt_agent(root_id, ThreadId::new())
            .await
            .is_err()
    );
    assert_eq!(
        root.session.interrupt_task_with_receipt().await.unwrap(),
        Some(stamps[0])
    );
    harness
        .manager
        .interrupt_agent(root_id, root_id)
        .await
        .unwrap();
    assert_eq!(
        root.session.interrupt_task_with_receipt().await.unwrap(),
        None
    );
    assert_eq!(
        ledger
            .observe_current(root_id)
            .await
            .unwrap()
            .unwrap()
            .disposition,
        WorkDisposition::Interrupted
    );
    assert!(ledger.current_wait(root_id).await.unwrap().is_none());
    assert!(
        !ledger
            .observe_current(child)
            .await
            .unwrap()
            .unwrap()
            .disposition
            .is_terminal()
    );
    assert!(root.is_running(), "interrupt does not unload the root");
    let receipt = control
        .interrupt_spawned_agent_with_receipt(root_id, child)
        .await
        .unwrap();
    assert_eq!(receipt.requested_work, Some(stamps[1]));
    assert_eq!(
        control
            .interrupt_spawned_agent_with_receipt(root_id, child)
            .await
            .unwrap()
            .requested_work,
        None
    );
    // Publishing after newer work admission must still identify the operation's
    // original target. The terminal event is deliberately already settled.
    let successor = WorkStamp {
        work_id: Uuid::now_v7(),
        ..stamps[1]
    };
    ledger
        .admit_work(
            successor,
            Some(stamps[1]),
            &PublishDelivery {
                id: successor.work_id,
                root_id,
                sender_id: Some(root_id),
                recipient_id: child,
                work: Some(successor),
                kind: DeliveryKind::Message,
                payload: "new assignment".to_owned(),
            },
        )
        .await
        .unwrap();
    runtime
        .publish_interrupt_request(
            root_id,
            "request-turn".to_owned(),
            "interrupt-call".to_owned(),
            &receipt,
        )
        .await;
    timeout(Duration::from_secs(2), async {
        loop {
            if let EventMsg::AgentCoordination(event) = root.next_event().await.unwrap().msg
                && let Some(request) = event.projection.interrupt
            {
                assert_eq!(event.thread_id, root_id);
                assert_eq!(event.turn_id, "request-turn");
                assert_eq!(request.operation_id, "interrupt-call");
                assert_eq!(request.target.thread_id, child);
                assert_eq!(request.target.work_id, stamps[1].work_id);
                assert_eq!(
                    request.target.task_name,
                    AgentPath::root().join("waiting-child").unwrap()
                );
                break;
            }
        }
    })
    .await
    .unwrap();
    assert_eq!(
        ledger.observe_current(child).await.unwrap().unwrap().stamp,
        successor
    );
    assert!(
        !ledger
            .observe_current(child)
            .await
            .unwrap()
            .unwrap()
            .disposition
            .is_terminal()
    );
    harness
        .manager
        .request_agent_tree_shutdown(root_id)
        .await
        .unwrap()
        .wait()
        .await
        .unwrap();
}

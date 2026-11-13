use super::*;
use crate::thread_manager::default_thread_id_generator;
use codex_protocol::error::CodexErrorDetails;
use codex_thread_store::ThreadStoreError;
use futures::future;
use pretty_assertions::assert_eq;
use tokio::sync::Barrier;
use tokio::sync::oneshot;

fn runtime() -> LocalAgentRuntime {
    LocalAgentRuntime::new(
        Weak::default(),
        default_thread_id_generator(),
        /*rollout_budget*/ None,
        /*work_state*/ None,
    )
}

#[tokio::test]
async fn lifetime_parent_survives_optional_paths_and_rejects_changed_ownership() {
    for named in [false, true] {
        let tree = runtime();
        let root = ThreadId::new();
        let child = ThreadId::new();
        let nested = ThreadId::new();
        tree.registry.register_root_thread(root);
        tree.for_actor_start(root, None).unwrap();
        if named {
            for (actor, path) in [(child, "/root/child"), (nested, "/root/child/nested")] {
                tree.registry.reserve_spawn_slot(None).unwrap().commit(
                    crate::agent::types::AgentMetadata {
                        agent_id: Some(actor),
                        agent_path: Some(AgentPath::try_from(path).unwrap()),
                        ..Default::default()
                    },
                );
            }
        } else {
            tree.for_actor_start(child, Some(root)).unwrap();
        }
        // Named retained metadata can establish its unloaded ancestor without
        // recursively acquiring the scope mutex. Anonymous actors retain source ownership.
        tree.for_actor_start(nested, Some(child)).unwrap();
        assert_eq!(tree.registered_parent(root), None);
        assert_eq!(tree.registered_parent(child), Some(root));
        assert_eq!(tree.registered_parent(nested), Some(child));
        assert!(tree.for_actor_start(nested, Some(root)).is_err());
        tree.for_actor_start(nested, tree.registered_parent(nested))
            .unwrap();
        assert_eq!(tree.registered_parent(nested), Some(child));
        tree.request_shutdown().wait().await.unwrap();
    }
}

#[tokio::test]
async fn subtree_shutdown_joins_startup_cleanup_and_auxiliaries_without_stopping_siblings() {
    let tree = runtime();
    let root = ThreadId::new();
    let child = ThreadId::new();
    let nested = ThreadId::new();
    let sibling = ThreadId::new();
    let root_runtime = tree.for_actor_start(root, None).unwrap();
    let child_runtime = tree.for_actor_start(child, Some(root)).unwrap();
    let nested_runtime = tree.for_actor_start(nested, Some(child)).unwrap();
    let sibling_runtime = tree.for_actor_start(sibling, Some(root)).unwrap();
    let root_live = root_runtime.admit_start().unwrap();
    let sibling_live = sibling_runtime.admit_start().unwrap();
    let startup = child_runtime.admit_start().unwrap();
    let cleanup = startup
        .clone()
        .into_teardown_guard("test_cleanup", Some(child));
    let auxiliary = nested_runtime.admit_start().unwrap();

    assert!(tree.request_actor_shutdown(root).is_err());
    assert!(tree.request_actor_shutdown(ThreadId::new()).is_err());
    let ending = tree.request_actor_shutdown(child).unwrap();
    assert!(child_runtime.shutdown.is_cancelled());
    assert!(nested_runtime.shutdown.is_cancelled());
    assert!(!root_runtime.shutdown.is_cancelled());
    assert!(!sibling_runtime.shutdown.is_cancelled());
    assert!(child_runtime.admit_start().is_err());
    assert!(tree.for_actor_start(child, Some(root)).is_err());
    assert!(
        nested_runtime
            .for_actor_start(ThreadId::new(), Some(nested))
            .is_err()
    );
    assert!(futures::poll!(Box::pin(ending.wait())).is_pending());
    drop(startup);
    cleanup.complete();
    assert!(futures::poll!(Box::pin(ending.wait())).is_pending());
    drop(auxiliary);
    ending.wait().await.unwrap();
    tree.complete_actor_shutdown(&ending);
    let resumed = tree.for_actor_start(child, Some(root)).unwrap();
    assert!(!resumed.shutdown.is_cancelled());
    assert!(
        child_runtime.shutdown.is_cancelled(),
        "old handles cannot admit into the renewed generation"
    );
    assert!(child_runtime.admit_start().is_err());
    assert!(tree.for_actor_start(child, Some(sibling)).is_err());
    let resumed_live = resumed.admit_start().unwrap();
    let shutdown = tree.request_shutdown();
    assert!(root_runtime.shutdown.is_cancelled());
    assert!(sibling_runtime.shutdown.is_cancelled());
    assert!(resumed.shutdown.is_cancelled());
    assert!(futures::poll!(Box::pin(shutdown.wait())).is_pending());
    drop((root_live, sibling_live, resumed_live));
    shutdown.wait().await.unwrap();
}

#[tokio::test]
async fn subtree_cleanup_failure_is_retained_for_tree_and_subtree_waiters() {
    let tree = runtime();
    let root = ThreadId::new();
    let child = ThreadId::new();
    tree.for_actor_start(root, None).unwrap();
    let child_runtime = tree.for_actor_start(child, Some(root)).unwrap();
    let cleanup = child_runtime
        .admit_start()
        .unwrap()
        .into_teardown_guard("test_cleanup", Some(child));
    let ending = tree.request_actor_shutdown(child).unwrap();
    cleanup.record_shutdown_failure("join", "test_failure");
    cleanup.complete();
    assert!(ending.wait().await.is_err());
    assert!(tree.request_shutdown().wait().await.is_err());
}

#[tokio::test]
async fn provisional_cleanup_joins_under_ancestor_ownership_and_retires_only_rejected_scope() {
    let tree = runtime();
    let root = ThreadId::new();
    let child = ThreadId::new();
    let nested = ThreadId::new();
    tree.registry.register_root_thread(root);
    tree.for_actor_start(root, None).unwrap();
    let child_runtime = tree.for_actor_start(child, Some(root)).unwrap();
    let child_cleanup = child_runtime.admit_start().unwrap();
    let ancestor_join = child_cleanup.ancestor_membership();
    let nested_runtime = tree.for_actor_start(nested, Some(child)).unwrap();
    let nested_live = nested_runtime.admit_start().unwrap();
    let ending = tree.request_actor_shutdown(child).unwrap();
    drop(child_cleanup);
    assert!(futures::poll!(Box::pin(ending.wait())).is_pending());
    drop(nested_live);
    ending.wait().await.unwrap();
    tree.complete_actor_shutdown(&ending);
    tree.retire_rejected_scope(&ending);
    assert!(tree.for_actor(child).is_err());
    assert!(tree.for_actor(nested).is_err());
    assert!(tree.for_actor(root).is_ok());
    let shutdown = tree.request_shutdown();
    assert!(
        futures::poll!(Box::pin(shutdown.wait())).is_pending(),
        "ancestor still owns cleanup bookkeeping"
    );
    drop(ancestor_join);
    shutdown.wait().await.unwrap();
}

#[tokio::test]
async fn parent_end_joins_already_admitted_nested_end_bookkeeping() {
    let tree = runtime();
    let root = ThreadId::new();
    let parent = ThreadId::new();
    let child = ThreadId::new();
    tree.for_actor_start(root, None).unwrap();
    tree.for_actor_start(parent, Some(root)).unwrap();
    tree.for_actor_start(child, Some(parent)).unwrap();
    let child_end = tree.admit_actor_teardown(child).unwrap();
    let child_scope = tree.request_actor_shutdown(child).unwrap();
    child_scope.wait().await.unwrap();
    let parent_scope = tree.request_actor_shutdown(parent).unwrap();
    assert!(futures::poll!(Box::pin(parent_scope.wait())).is_pending());
    assert!(tree.admit_actor_teardown(child).is_err());
    drop(child_end);
    parent_scope.wait().await.unwrap();
    tree.request_shutdown().wait().await.unwrap();
}

#[tokio::test]
async fn aborted_teardown_reports_operation_and_thread() {
    let runtime = runtime();
    let tree_id = runtime.tree_id();
    let thread_id = ThreadId::new();
    let teardown = runtime
        .admit_start()
        .expect("teardown should be admitted")
        .into_teardown_guard("session_startup", Some(thread_id));
    let (ready_tx, ready_rx) = oneshot::channel();
    let task = tokio::spawn(async move {
        let _teardown = teardown;
        let _ = ready_tx.send(());
        future::pending::<()>().await;
    });
    ready_rx.await.expect("teardown task should start");
    let shutdown = runtime.request_shutdown();

    task.abort();
    task.await.expect_err("teardown task should be aborted");

    let report = shutdown
        .wait_detailed()
        .await
        .expect_err("aborted teardown must fail tree shutdown");
    assert_eq!(
        report,
        AgentTreeShutdownReport {
            tree_id,
            failures: vec![AgentTreeShutdownFailure {
                operation: "session_startup",
                thread_id: Some(thread_id),
                reason: AgentTreeShutdownFailureReason::GuardAbandoned,
            }],
            omitted_failures: 0,
        }
    );
    assert_eq!(
        report.to_string(),
        format!(
            "agent tree shutdown did not complete cleanly: tree_id={}, failures=[{{operation=session_startup, thread_id={thread_id}, reason=guard_abandoned}}]; omitted_failures=0",
            report.tree_id,
        )
    );
    let error = shutdown
        .wait()
        .await
        .expect_err("legacy wait must fail after an abandoned teardown");
    assert!(matches!(
        error.details(),
        CodexErrorDetails::Fatal(message) if message == "agent tree shutdown did not complete cleanly"
    ));
}

#[tokio::test]
async fn completed_teardown_does_not_fail_tree_shutdown() {
    let runtime = runtime();
    let teardown = runtime
        .admit_start()
        .expect("teardown should be admitted")
        .into_teardown_guard("session_loop", /*thread_id*/ None);
    let shutdown = runtime.request_shutdown();
    teardown.complete();

    assert_eq!(shutdown.wait_detailed().await, Ok(()));
    assert!(shutdown.wait().await.is_ok());
}

#[tokio::test]
async fn report_display_includes_safe_cause_without_raw_error_payload() {
    let runtime = runtime();
    let thread_id = ThreadId::new();
    let teardown = runtime
        .admit_start()
        .expect("teardown should be admitted")
        .into_teardown_guard("session_startup", Some(thread_id));
    let raw_payload = "sensitive thread-store error payload";
    for error in [
        ThreadStoreError::Internal {
            message: raw_payload.to_owned(),
        },
        ThreadStoreError::Retryable {
            message: raw_payload.to_owned(),
        },
        ThreadStoreError::OutcomeUnknown {
            message: raw_payload.to_owned(),
        },
    ] {
        teardown.record_shutdown_failure(
            "discard_persistence",
            crate::thread_manager::thread_store_error_kind(&error),
        );
    }
    let shutdown = runtime.request_shutdown();
    teardown.complete();

    let report = shutdown
        .wait_detailed()
        .await
        .expect_err("recorded failure must fail shutdown");
    let displayed = report.to_string();
    assert_eq!(
        displayed,
        format!(
            "agent tree shutdown did not complete cleanly: tree_id={}, failures=[{{operation=session_startup, phase=discard_persistence, thread_id={thread_id}, reason=operation_failed, error_kind=thread_store_internal}}, {{operation=session_startup, phase=discard_persistence, thread_id={thread_id}, reason=operation_failed, error_kind=thread_store_retryable}}, {{operation=session_startup, phase=discard_persistence, thread_id={thread_id}, reason=operation_failed, error_kind=thread_store_outcome_unknown}}]; omitted_failures=0",
            report.tree_id,
        )
    );
    assert!(!displayed.contains(raw_payload));
}

#[tokio::test]
async fn cloned_guard_reports_its_own_context_before_releasing_membership() {
    let runtime = runtime();
    let original = runtime
        .admit_start()
        .expect("teardown should be admitted")
        .into_teardown_guard("startup", /*thread_id*/ None);
    let mut session = original.clone_for_teardown("session_loop", /*thread_id*/ None);
    let thread_id = ThreadId::new();
    session.set_thread_id(thread_id);
    let shutdown = runtime.request_shutdown();
    original.complete();

    let wait = shutdown.wait_detailed();
    tokio::pin!(wait);
    assert!(futures::poll!(&mut wait).is_pending());
    drop(session);
    let report = wait
        .await
        .expect_err("abandoned session must fail shutdown");
    assert_eq!(
        report,
        AgentTreeShutdownReport {
            tree_id: report.tree_id,
            failures: vec![AgentTreeShutdownFailure {
                operation: "session_loop",
                thread_id: Some(thread_id),
                reason: AgentTreeShutdownFailureReason::GuardAbandoned,
            }],
            omitted_failures: 0,
        }
    );
}

#[tokio::test]
async fn concurrent_failures_are_retained_for_every_waiter() {
    let runtime = runtime();
    let barrier = Arc::new(Barrier::new(/*n*/ 3));
    let thread_ids = [
        ThreadId::from_u128(/*value*/ 1),
        ThreadId::from_u128(/*value*/ 2),
    ];
    let mut tasks = Vec::new();
    for thread_id in thread_ids {
        let teardown = runtime
            .admit_start()
            .expect("teardown should be admitted")
            .into_teardown_guard("child_spawn", Some(thread_id));
        let barrier = Arc::clone(&barrier);
        tasks.push(tokio::spawn(async move {
            barrier.wait().await;
            teardown.record_shutdown_failure("discard_persistence", "thread_store_internal");
            teardown.complete();
        }));
    }
    let shutdown = runtime.request_shutdown();
    let first_waiter = {
        let shutdown = Arc::clone(&shutdown);
        tokio::spawn(async move { shutdown.wait_detailed().await })
    };
    let second_waiter = {
        let shutdown = Arc::clone(&shutdown);
        tokio::spawn(async move { shutdown.wait_detailed().await })
    };
    barrier.wait().await;
    for task in tasks {
        task.await.expect("cleanup task should finish");
    }
    let mut first = first_waiter
        .await
        .expect("first waiter should finish")
        .expect_err("recorded failures must fail shutdown");
    let second = second_waiter
        .await
        .expect("second waiter should finish")
        .expect_err("recorded failures must fail shutdown");
    assert_eq!(first, second);
    assert_eq!(shutdown.wait_detailed().await, Err(first.clone()));

    first
        .failures
        .sort_by_key(|failure| failure.thread_id.map(|thread_id| thread_id.to_string()));
    assert_eq!(
        first,
        AgentTreeShutdownReport {
            tree_id: first.tree_id,
            failures: thread_ids
                .into_iter()
                .map(|thread_id| AgentTreeShutdownFailure::operation_failed(
                    "child_spawn",
                    "discard_persistence",
                    Some(thread_id),
                    "thread_store_internal",
                ))
                .collect(),
            omitted_failures: 0,
        }
    );
}

#[tokio::test]
async fn failure_report_counts_omitted_failures() {
    let runtime = runtime();
    let teardown = runtime
        .admit_start()
        .expect("teardown should be admitted")
        .into_teardown_guard("startup", /*thread_id*/ None);
    let failure = AgentTreeShutdownFailure::operation_failed(
        "startup",
        "stop_session",
        /*thread_id*/ None,
        "internal",
    );
    for _ in 0..MAX_RETAINED_SHUTDOWN_FAILURES + 3 {
        runtime.record_shutdown_failure(failure.clone());
    }
    let shutdown = runtime.request_shutdown();
    teardown.complete();

    let report = shutdown
        .wait_detailed()
        .await
        .expect_err("recorded failures must fail shutdown");
    assert_eq!(
        report,
        AgentTreeShutdownReport {
            tree_id: report.tree_id,
            failures: vec![failure; MAX_RETAINED_SHUTDOWN_FAILURES],
            omitted_failures: 3,
        }
    );
}

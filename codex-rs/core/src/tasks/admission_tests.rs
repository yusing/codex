use super::*;
use crate::session::tests::make_session_and_context_with_rx;
use crate::tasks::SessionTask;
use crate::tasks::SessionTaskResult;
use pretty_assertions::assert_eq;
use tokio::sync::Semaphore;

struct PendingTask(Option<Arc<tokio::sync::Notify>>);

impl SessionTask for PendingTask {
    fn kind(&self) -> TaskKind {
        TaskKind::Regular
    }

    fn span_name(&self) -> &'static str {
        "session_task.pending_capacity"
    }

    async fn run(
        self: Arc<Self>,
        _session: Arc<Session>,
        _context: Arc<TurnContext>,
        _input: Vec<TurnInput>,
        _cancellation: CancellationToken,
    ) -> SessionTaskResult {
        if let Some(started) = &self.0 {
            started.notify_one();
        }
        std::future::pending().await
    }
}

fn execution_guard(capacity: Arc<Semaphore>) -> BoxFuture<'static, AgentExecutionGuard> {
    Box::pin(async move {
        let permit = Arc::clone(&capacity).acquire_owned().await.unwrap();
        AgentExecutionGuard::resumable(permit, move || {
            Box::pin(async move { Ok(execution_guard(capacity).await) })
        })
    })
}

async fn running_task() -> (Arc<Session>, Arc<TurnContext>, Arc<Semaphore>) {
    let (session, context, _events) = make_session_and_context_with_rx().await;
    session
        .spawn_task(Arc::clone(&context), Vec::new(), PendingTask(None))
        .await;
    let capacity = Arc::new(Semaphore::new(1));
    let guard = execution_guard(Arc::clone(&capacity)).await;
    session
        .active_turn
        .lock()
        .await
        .as_mut()
        .unwrap()
        .task
        .as_mut()
        .unwrap()
        ._agent_execution_guard = Some(guard);
    (session, context, capacity)
}

struct HostAdmission {
    capacity: Arc<Semaphore>,
    requests: tokio::sync::mpsc::UnboundedSender<AgentTurnAdmissionRequest>,
}

impl AgentTurnAdmission for HostAdmission {
    fn admit_turn(
        &self,
        request: AgentTurnAdmissionRequest,
    ) -> BoxFuture<'_, Result<AgentExecutionGuard>> {
        Box::pin(async move {
            self.requests.send(request).unwrap();
            let permit = Arc::clone(&self.capacity)
                .try_acquire_owned()
                .map_err(|_| {
                    CodexErr::new(CodexErrorDetails::AgentLimitReached { max_threads: 1 })
                })?;
            Ok(AgentExecutionGuard::new(permit))
        })
    }
}

#[tokio::test]
async fn host_resume_reuses_admission_without_restarting_or_replaying_input() {
    for deny_resume in [false, true] {
        let (mut session, context, events) = make_session_and_context_with_rx().await;
        let capacity = Arc::new(Semaphore::new(1));
        let (requests, mut received) = tokio::sync::mpsc::unbounded_channel();
        let data = codex_extension_api::ExtensionData::new(session.thread_id.to_string());
        data.insert(crate::agent::host::AgentRuntimeOwnership::Host);
        data.insert(Arc::new(HostAdmission {
            capacity: Arc::clone(&capacity),
            requests,
        }) as Arc<dyn AgentTurnAdmission>);
        Arc::get_mut(&mut session).unwrap().agent_runtime =
            crate::agent::host::AgentRuntimeServices::from_extensions(&data).unwrap();
        let started = Arc::new(tokio::sync::Notify::new());
        session
            .spawn_task(
                Arc::clone(&context),
                Vec::new(),
                PendingTask(Some(Arc::clone(&started))),
            )
            .await;
        started.notified().await;
        let initial = received.recv().await.unwrap();
        assert_eq!(initial.thread_id, session.thread_id);
        assert_eq!(initial.turn_id, context.sub_id);
        while events.try_recv().is_ok() {}
        let paused = session.pause_turn_execution(&context).await.unwrap();
        assert_eq!(capacity.available_permits(), 1);
        let occupied = if deny_resume {
            Some(Arc::clone(&capacity).acquire_owned().await.unwrap())
        } else {
            None
        };
        let result = paused.resume().await;
        assert_eq!(received.recv().await.unwrap(), initial);
        assert!(
            events.try_recv().is_err(),
            "resume must not emit a second start"
        );
        if deny_resume {
            assert!(matches!(
                result.unwrap_err().details(),
                CodexErrorDetails::AgentLimitReached { .. }
            ));
        } else {
            result.unwrap();
            assert_eq!(capacity.available_permits(), 0);
        }
        drop(occupied);
        session.abort_all_tasks(TurnAbortReason::Replaced).await;
        assert_eq!(capacity.available_permits(), 1);
    }
}

#[tokio::test]
async fn paused_turn_retains_task_and_waits_cancellably_for_capacity() {
    let (session, context, capacity) = running_task().await;
    let paused = session.pause_turn_execution(&context).await.unwrap();
    assert_eq!(capacity.available_permits(), 1);
    {
        let active = session.active_turn.lock().await;
        let task = active.as_ref().unwrap().task.as_ref().unwrap();
        assert!(Arc::ptr_eq(&task.turn_context, &context));
        assert!(!task.handle.is_finished());
        assert!(task._agent_execution_guard.is_none());
    }
    let occupied = Arc::clone(&capacity).acquire_owned().await.unwrap();
    let resumed = paused.resume();
    tokio::pin!(resumed);
    assert!(futures::poll!(resumed.as_mut()).is_pending());
    drop(occupied);
    resumed.await.unwrap();
    assert_eq!(capacity.available_permits(), 0);
    session.abort_all_tasks(TurnAbortReason::Replaced).await;
    assert_eq!(capacity.available_permits(), 1);
}

#[tokio::test]
async fn root_pause_keeps_the_original_unrestricted_task() {
    let (session, context, _events) = make_session_and_context_with_rx().await;
    session
        .spawn_task(Arc::clone(&context), Vec::new(), PendingTask(None))
        .await;
    let paused = session.pause_turn_execution(&context).await.unwrap();
    paused.resume().await.unwrap();
    {
        let active = session.active_turn.lock().await;
        let task = active.as_ref().unwrap().task.as_ref().unwrap();
        assert!(Arc::ptr_eq(&task.turn_context, &context));
        assert!(task._agent_execution_guard.is_none());
        assert!(!task.handle.is_finished());
    }
    session.abort_all_tasks(TurnAbortReason::Replaced).await;
}

#[tokio::test]
async fn cancelled_reacquisition_releases_capacity_without_resuming_task() {
    let (session, context, capacity) = running_task().await;
    let paused = session.pause_turn_execution(&context).await.unwrap();
    let occupied = Arc::clone(&capacity).acquire_owned().await.unwrap();
    let resumed = paused.resume();
    tokio::pin!(resumed);
    assert!(futures::poll!(resumed.as_mut()).is_pending());
    session
        .active_turn
        .lock()
        .await
        .as_ref()
        .unwrap()
        .task
        .as_ref()
        .unwrap()
        .cancellation_token
        .cancel();
    assert!(matches!(
        resumed.await.unwrap_err().details(),
        CodexErrorDetails::TurnAborted
    ));
    drop(occupied);
    assert_eq!(capacity.available_permits(), 1);
    session.abort_all_tasks(TurnAbortReason::Replaced).await;
}

#[tokio::test]
async fn resumed_permit_never_attaches_to_replacement_context_with_same_turn_id() {
    let (session, context, capacity) = running_task().await;
    let paused = session.pause_turn_execution(&context).await.unwrap();
    let (_, mut replacement, _events) = make_session_and_context_with_rx().await;
    Arc::get_mut(&mut replacement).unwrap().sub_id = context.sub_id.clone();
    session
        .active_turn
        .lock()
        .await
        .as_mut()
        .unwrap()
        .task
        .as_mut()
        .unwrap()
        .turn_context = replacement;
    assert!(matches!(
        paused.resume().await.unwrap_err().details(),
        CodexErrorDetails::TurnAborted
    ));
    assert_eq!(capacity.available_permits(), 1);
    assert!(
        session
            .active_turn
            .lock()
            .await
            .as_ref()
            .unwrap()
            .task
            .as_ref()
            .unwrap()
            ._agent_execution_guard
            .is_none()
    );
    session.abort_all_tasks(TurnAbortReason::Replaced).await;
}

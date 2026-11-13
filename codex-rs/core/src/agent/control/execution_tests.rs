use crate::agent::LocalAgentControl;
use codex_protocol::error::CodexErrorDetails;
use codex_protocol::protocol::MultiAgentVersion;
use codex_protocol::protocol::SessionSource;
use codex_protocol::protocol::SubAgentSource;
use pretty_assertions::assert_eq;

fn control_with_limit(max_threads: usize) -> LocalAgentControl {
    let control = LocalAgentControl::default();
    control
        .runtime
        .agent_execution_limiter
        .initialize(Some(max_threads));
    control
}

#[test]
fn unbounded_execution_preserves_root_policy_and_admits_all_children() {
    let control = LocalAgentControl::default();
    control.runtime.agent_execution_limiter.initialize(None);
    // Explicit child settings cannot replace the root's unbounded policy.
    control.runtime.agent_execution_limiter.initialize(Some(1));
    let source = SessionSource::SubAgent(SubAgentSource::Other("worker".to_owned()));
    let mut guards = Vec::new();
    for _ in 0..4 {
        control
            .ensure_execution_capacity(MultiAgentVersion::V2, &source)
            .unwrap();
        guards.push(
            control
                .execution_guard(MultiAgentVersion::V2, &source)
                .unwrap(),
        );
    }
    control
        .ensure_execution_capacity(MultiAgentVersion::V2, &source)
        .unwrap();
    drop(guards);
}

#[test]
fn execution_guards_count_active_v2_subagent_turns() {
    let control = control_with_limit(/*max_threads*/ 1);
    // Child role configs cannot replace the root-derived session limit.
    control
        .runtime
        .agent_execution_limiter
        .initialize(/*max_threads*/ Some(2));
    let source = SessionSource::SubAgent(SubAgentSource::Other("worker".to_string()));

    control
        .ensure_execution_capacity(MultiAgentVersion::V2, &source)
        .expect("first active turn should fit");
    let first = control
        .execution_guard(MultiAgentVersion::V2, &source)
        .expect("v2 subagent execution should be counted");
    let Err(err) = control.ensure_execution_capacity(MultiAgentVersion::V2, &source) else {
        panic!("second active turn should exceed the derived non-root cap");
    };
    let CodexErrorDetails::AgentLimitReached { max_threads } = err.details() else {
        panic!("expected AgentLimitReached");
    };
    assert_eq!(*max_threads, 1);

    drop(first);
    control
        .ensure_execution_capacity(MultiAgentVersion::V2, &source)
        .expect("capacity should be released when the running task drops");
}

#[test]
fn execution_guards_ignore_root_and_v1_turns() {
    let control = control_with_limit(/*max_threads*/ 0);

    assert!(
        control
            .execution_guard(MultiAgentVersion::V2, &SessionSource::Cli)
            .is_none()
    );
    assert!(
        control
            .execution_guard(
                MultiAgentVersion::V1,
                &SessionSource::SubAgent(SubAgentSource::Other("worker".to_string())),
            )
            .is_none()
    );
}

#[tokio::test]
async fn paused_execution_releases_capacity_and_waits_for_reacquisition() {
    let control = control_with_limit(1);
    let source = SessionSource::SubAgent(SubAgentSource::Other("worker".into()));
    let guard = control
        .execution_guard(MultiAgentVersion::V2, &source)
        .unwrap();
    let mut resume = guard
        .suspend()
        .unwrap_or_else(|_| panic!("local guard is resumable"));
    control
        .ensure_execution_capacity(MultiAgentVersion::V2, &source)
        .unwrap();
    let other = control
        .execution_guard(MultiAgentVersion::V2, &source)
        .unwrap();
    assert!(futures::poll!(resume.as_mut()).is_pending());
    drop(other);
    let resumed = resume.await.unwrap();
    assert!(
        control
            .ensure_execution_capacity(MultiAgentVersion::V2, &source)
            .is_err()
    );
    drop(resumed);
    control
        .ensure_execution_capacity(MultiAgentVersion::V2, &source)
        .unwrap();
}

#[tokio::test]
async fn simultaneous_resumes_respect_the_current_limit() {
    let control = control_with_limit(1);
    let source = SessionSource::SubAgent(SubAgentSource::Other("worker".into()));
    let first = control
        .execution_guard(MultiAgentVersion::V2, &source)
        .unwrap();
    let mut first = first.suspend().unwrap_or_else(|_| panic!("resumable"));
    let second = control
        .execution_guard(MultiAgentVersion::V2, &source)
        .unwrap();
    let mut second = second.suspend().unwrap_or_else(|_| panic!("resumable"));
    let occupied = control
        .execution_guard(MultiAgentVersion::V2, &source)
        .unwrap();
    assert!(futures::poll!(first.as_mut()).is_pending());
    assert!(futures::poll!(second.as_mut()).is_pending());
    drop(occupied);
    let resumed = first.await.unwrap();
    assert!(futures::poll!(second.as_mut()).is_pending());
    // Cancelling a queued reacquisition must not reserve a later release.
    drop(second);
    drop(resumed);
    control
        .ensure_execution_capacity(MultiAgentVersion::V2, &source)
        .unwrap();
}

#[tokio::test]
async fn unbounded_execution_can_pause_and_resume_repeatedly() {
    let control = LocalAgentControl::default();
    control.runtime.agent_execution_limiter.initialize(None);
    let source = SessionSource::SubAgent(SubAgentSource::Other("worker".into()));
    let mut guard = control
        .execution_guard(MultiAgentVersion::V2, &source)
        .unwrap();
    for _ in 0..3 {
        guard = guard
            .suspend()
            .unwrap_or_else(|_| panic!("resumable"))
            .await
            .unwrap();
    }
    drop(guard);
    assert_eq!(
        control
            .runtime
            .agent_execution_limiter
            .active
            .load(std::sync::atomic::Ordering::Acquire),
        0
    );
}

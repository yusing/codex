use super::invalid_request;
use codex_app_server_protocol::JSONRPCErrorError;
use codex_core::CodexThread;
use codex_protocol::protocol::MultiAgentVersion;
use codex_protocol::protocol::SessionSource;
use codex_protocol::protocol::SubAgentSource;

pub(super) const DIRECT_INPUT_TO_MULTI_AGENT_V2_SUBAGENT_ERROR: &str =
    "direct app-server input is not allowed for multi-agent v2 sub-agents";

/// Structural ownership is independent of the human composer's input capability.
pub(super) fn is_parent_owned_v2_child(
    multi_agent_version: Option<MultiAgentVersion>,
    session_source: &SessionSource,
) -> bool {
    multi_agent_version == Some(MultiAgentVersion::V2)
        && matches!(
            session_source,
            SessionSource::SubAgent(SubAgentSource::ThreadSpawn { .. })
        )
}

/// Ordinary named V2 children admit human input through Core's durable work owner.
/// Legacy/unattributed children still need owner reconciliation before direct input.
pub(super) fn can_accept_direct_input(
    multi_agent_version: Option<MultiAgentVersion>,
    session_source: &SessionSource,
) -> bool {
    !is_parent_owned_v2_child(multi_agent_version, session_source)
        || matches!(
            session_source,
            SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
                agent_path: Some(path), ..
            }) if !path.is_root()
        )
}

pub(super) async fn ensure_human_input_allowed(
    thread: &CodexThread,
) -> Result<(), JSONRPCErrorError> {
    let snapshot = thread.config_snapshot().await;
    if !loaded_can_accept_direct_input(thread, &snapshot.session_source) {
        return Err(invalid_request(
            "human child input requires an admitted native work owner",
        ));
    }
    Ok(())
}

pub(super) fn loaded_can_accept_direct_input(thread: &CodexThread, source: &SessionSource) -> bool {
    can_accept_direct_input(thread.multi_agent_version(), source)
        && (!is_parent_owned_v2_child(thread.multi_agent_version(), source)
            || thread.has_native_work_owner())
}

/// Raw injection, local shell escapes, and replacement lifecycles are not human turn input.
pub(super) async fn ensure_direct_input_allowed(
    thread: &CodexThread,
) -> Result<(), JSONRPCErrorError> {
    let config_snapshot = thread.config_snapshot().await;
    if is_parent_owned_v2_child(
        thread.multi_agent_version(),
        &config_snapshot.session_source,
    ) {
        return Err(invalid_request(
            DIRECT_INPUT_TO_MULTI_AGENT_V2_SUBAGENT_ERROR,
        ));
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use codex_protocol::AgentPath;
    use codex_protocol::ThreadId;

    #[test]
    fn human_capability_does_not_change_parent_owned_restore() {
        let named = SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
            parent_thread_id: ThreadId::new(),
            depth: 1,
            agent_path: Some(AgentPath::root().join("worker").unwrap()),
            agent_nickname: None,
            agent_role: None,
        });
        assert!(is_parent_owned_v2_child(
            Some(MultiAgentVersion::V2),
            &named
        ));
        assert!(can_accept_direct_input(Some(MultiAgentVersion::V2), &named));

        let mut unattributed = named.clone();
        if let SessionSource::SubAgent(SubAgentSource::ThreadSpawn { agent_path, .. }) =
            &mut unattributed
        {
            *agent_path = None;
        }
        assert!(is_parent_owned_v2_child(
            Some(MultiAgentVersion::V2),
            &unattributed
        ));
        assert!(!can_accept_direct_input(
            Some(MultiAgentVersion::V2),
            &unattributed
        ));
        assert!(can_accept_direct_input(
            Some(MultiAgentVersion::V1),
            &unattributed
        ));
        assert!(!is_parent_owned_v2_child(
            Some(MultiAgentVersion::V1),
            &unattributed
        ));
        assert!(can_accept_direct_input(
            Some(MultiAgentVersion::V2),
            &SessionSource::Cli
        ));
        assert!(!is_parent_owned_v2_child(
            Some(MultiAgentVersion::V2),
            &SessionSource::Cli
        ));
    }
}

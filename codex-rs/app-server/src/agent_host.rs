//! Resource storage stays with the host; native work owns admission and publication.

use crate::outgoing_message::ConnectionId;
use crate::outgoing_message::OutgoingMessageSender;
use codex_app_server_protocol as wire;
use codex_app_server_protocol::AgentPrepareFinalCaptureParams;
use codex_app_server_protocol::AgentPrepareFinalCaptureResponse;
use codex_app_server_protocol::AgentPrepareHostAdmissionParams;
use codex_app_server_protocol::AgentPrepareHostAdmissionResponse;
use codex_app_server_protocol::ServerRequestPayload;
use codex_core::AgentHostCallbacks;
use codex_protocol::ThreadId;
use codex_protocol::error::CodexErr;
use codex_protocol::error::Result as CodexResult;
use codex_state::coordination_progress as progress;
use std::sync::Arc;
use std::sync::Weak;
use tokio_util::sync::CancellationToken;

pub(crate) fn host_callbacks(
    outgoing: &Arc<OutgoingMessageSender>,
    connection: ConnectionId,
    disconnect: CancellationToken,
    feature_use: Arc<std::sync::Mutex<crate::feature_use::FeatureUse>>,
) -> AgentHostCallbacks {
    let host = HostRequests {
        outgoing: Arc::downgrade(outgoing),
        connection,
        disconnect,
    };
    let captures = host.clone();
    let freezing = host.clone();
    let progress_output = host.clone();
    let final_projection = host.clone();
    let foreign = host.clone();
    let coordination = host.clone();
    AgentHostCallbacks {
        host_connection_id: connection.0 as u64,
        publish_coordination: Arc::new(move |event| {
            let host = coordination.clone();
            Box::pin(async move {
                let Some(outgoing) = host.outgoing.upgrade() else {
                    return;
                };
                let connections = [host.connection];
                tokio::select! {
                    biased;
                    _ = host.disconnect.cancelled() => {},
                    _ = outgoing.send_server_notification_to_connections(
                        &connections, wire::ServerNotification::AgentCoordination(event),
                    ) => {},
                }
            })
        }),
        record_feature_use: Arc::new(move |fact| feature_use.lock().unwrap().record(fact)),
        route_foreign: Arc::new(move |request, cancellation| {
            let host = foreign.clone();
            Box::pin(async move {
                let actor = request.source.actor_id;
                let result = host
                    .request(
                        ServerRequestPayload::AgentForeignRoute(request),
                        actor,
                        cancellation,
                    )
                    .await?;
                serde_json::from_value(result).map_err(|error| {
                    CodexErr::Fatal(format!("invalid foreign routing response: {error}"))
                })
            })
        }),
        prepare_final_capture: Arc::new(move |request, cancellation| {
            let host = captures.clone();
            Box::pin(async move {
                let result = host
                    .request(
                        ServerRequestPayload::AgentPrepareFinalCapture(
                            AgentPrepareFinalCaptureParams {
                                root_thread_id: request.work.root_id.to_string(),
                                thread_id: request.work.actor_id.to_string(),
                                work_id: request.work.work_id.to_string(),
                                recipient_thread_id: request.recipient.to_string(),
                                recipient_cwd: request.recipient_cwd,
                                sources: request.sources,
                            },
                        ),
                        request.work.actor_id,
                        cancellation,
                    )
                    .await?;
                let response: AgentPrepareFinalCaptureResponse = serde_json::from_value(result)
                    .map_err(|error| {
                        CodexErr::Fatal(format!("invalid final capture response: {error}"))
                    })?;
                Ok(response
                    .capture
                    .map(|capture| progress::FrozenFinalCapture {
                        resource_ref: capture.resource_ref,
                        full_text: capture.full_text,
                    }))
            })
        }),
        freeze_progress: Arc::new(move |request, cancellation| {
            let host = freezing.clone();
            Box::pin(async move {
                let result = host
                    .request(
                        ServerRequestPayload::AgentFreezeProgress(
                            wire::AgentFreezeProgressParams {
                                root_thread_id: request.caller.root_id.to_string(),
                                caller_thread_id: request.caller.actor_id.to_string(),
                                caller_work_id: request.caller.work_id.to_string(),
                                wait_id: request.wait_id.to_string(),
                                batch_id: request.batch_id.to_string(),
                                recipient_context_id: request.recipient_context_id.to_string(),
                                selected: request
                                    .selected
                                    .into_iter()
                                    .map(progress_actor)
                                    .collect(),
                                seen: request.seen.into_iter().map(capture_cursor).collect(),
                            },
                        ),
                        request.caller.actor_id,
                        cancellation,
                    )
                    .await?;
                serde_json::from_value(result).map_err(|error| {
                    CodexErr::Fatal(format!("invalid progress freeze response: {error}"))
                })
            })
        }),
        prepare_progress_output: Arc::new(move |request, cancellation| {
            let host = progress_output.clone();
            Box::pin(async move {
                let result = host
                    .request(
                        ServerRequestPayload::AgentPrepareProgressOutput(
                            wire::AgentPrepareProgressOutputParams {
                                root_thread_id: request.caller.root_id.to_string(),
                                caller_thread_id: request.caller.actor_id.to_string(),
                                caller_work_id: request.caller.work_id.to_string(),
                                wait_id: request.wait_id.to_string(),
                                batch_id: request.batch_id.to_string(),
                                recipient_context_id: request.recipient_context_id.to_string(),
                                snapshot_ref: request.snapshot_ref,
                                agents: request
                                    .agents
                                    .into_iter()
                                    .map(|agent| wire::AgentPreparedProgress {
                                        actor: progress_actor(agent.actor),
                                        intervals: agent
                                            .intervals
                                            .into_iter()
                                            .map(capture_interval)
                                            .collect(),
                                        summary: match agent.summary {
                                            progress::SummaryResult::Available { text } => {
                                                wire::AgentProgressSummary::Available { text }
                                            }
                                            progress::SummaryResult::Unavailable { reason } => {
                                                wire::AgentProgressSummary::Unavailable { reason }
                                            }
                                        },
                                    })
                                    .collect(),
                                coverage: wire::AgentCaptureCoverage {
                                    status: request.coverage.status,
                                    diagnostics: request.coverage.diagnostics,
                                },
                            },
                        ),
                        request.caller.actor_id,
                        cancellation,
                    )
                    .await?;
                serde_json::from_value(result).map_err(|error| {
                    CodexErr::Fatal(format!("invalid progress output response: {error}"))
                })
            })
        }),
        project_final_capture: Arc::new(move |request, cancellation| {
            let host = final_projection.clone();
            Box::pin(async move {
                let result = host
                    .request(
                        ServerRequestPayload::AgentProjectFinalCapture(
                            wire::AgentProjectFinalCaptureParams {
                                root_thread_id: request.work.root_id.to_string(),
                                thread_id: request.work.actor_id.to_string(),
                                work_id: request.work.work_id.to_string(),
                                recipient_thread_id: request.recipient.to_string(),
                                recipient_cwd: request.recipient_cwd,
                                recipient_context_id: request.recipient_context_id.to_string(),
                                resource_ref: request.resource_ref,
                                seen: request.seen.into_iter().map(capture_cursor).collect(),
                            },
                        ),
                        request.work.actor_id,
                        cancellation,
                    )
                    .await?;
                let response: wire::AgentProjectFinalCaptureResponse =
                    serde_json::from_value(result).map_err(|error| {
                        CodexErr::Fatal(format!("invalid final projection response: {error}"))
                    })?;
                Ok(codex_core::ProjectedFinalCapture {
                    text: response.text,
                })
            })
        }),
        admit_child: Arc::new(move |request, cancellation| {
            let host = host.clone();
            Box::pin(async move {
                let result = host
                    .request(
                        ServerRequestPayload::AgentPrepareHostAdmission(
                            AgentPrepareHostAdmissionParams {
                                root_thread_id: request.root_id.to_string(),
                                parent_thread_id: request.parent_id.to_string(),
                                thread_id: request.thread_id.to_string(),
                                local_cwd: request.local_cwd,
                            },
                        ),
                        request.parent_id,
                        cancellation,
                    )
                    .await?;
                let response: AgentPrepareHostAdmissionResponse = serde_json::from_value(result)
                    .map_err(|error| {
                        CodexErr::Fatal(format!("invalid child host admission response: {error}"))
                    })?;
                if response.thread_id != request.thread_id.to_string() {
                    return Err(CodexErr::InvalidRequest(
                        "host admitted a different child identity".to_owned(),
                    ));
                }
                Ok(())
            })
        }),
    }
}

fn progress_actor(actor: progress::ProgressActor) -> wire::AgentProgressActor {
    wire::AgentProgressActor {
        thread_id: actor.thread_id.to_string(),
        work_id: actor.work_id.to_string(),
        task_name: actor.task_name,
    }
}

fn capture_cursor(cursor: progress::CaptureCursor) -> wire::AgentCaptureCursor {
    wire::AgentCaptureCursor {
        key: cursor.key,
        through_chunk: cursor.through_chunk,
    }
}

fn capture_interval(interval: progress::CaptureInterval) -> wire::AgentCaptureInterval {
    wire::AgentCaptureInterval {
        key: interval.key,
        change_id: interval.change_id,
        record_order: interval.record_order,
        legacy_record_key: interval.legacy_record_key,
        first_chunk: interval.first_chunk,
        last_chunk: interval.last_chunk,
    }
}

#[derive(Clone)]
struct HostRequests {
    outgoing: Weak<OutgoingMessageSender>,
    connection: ConnectionId,
    disconnect: CancellationToken,
}

impl HostRequests {
    async fn request(
        self,
        payload: ServerRequestPayload,
        thread: ThreadId,
        cancellation: CancellationToken,
    ) -> CodexResult<serde_json::Value> {
        let outgoing = self
            .outgoing
            .upgrade()
            .ok_or_else(|| CodexErr::Fatal("resource host is unavailable".to_owned()))?;
        let (mut completed, result) = tokio::sync::oneshot::channel();
        let requests = Arc::clone(&outgoing);
        // Abandoning a tool/startup future must still retire its exact host request.
        // The actual request owner joins this cleanup when the server shuts down.
        outgoing.track_host_request(async move {
            if cancellation.is_cancelled() || self.disconnect.is_cancelled() || completed.is_closed() {
                let _ = completed.send(Err(CodexErr::TurnAborted));
                return;
            }
            // Allocate before sending so cancellation also covers backpressure on
            // the outgoing transport, without leaving a registered callback behind.
            let id = requests.next_request_id();
            let operation = async {
                let (_, response) = requests.send_request_with_lifetime(
                    id.clone(), Some(&[self.connection]), payload, Some(thread), false,
                ).await;
                response.await
                    .map_err(|_| CodexErr::Fatal("host resource request was canceled".to_owned()))
                    .and_then(|response| response.map_err(|error| CodexErr::Fatal(format!("resource host: {}", error.message))))
            };
            let result = tokio::select! {
                biased;
                _ = cancellation.cancelled() => Err(CodexErr::TurnAborted),
                _ = self.disconnect.cancelled() => Err(CodexErr::Fatal("resource host disconnected".to_owned())),
                _ = completed.closed() => Err(CodexErr::TurnAborted),
                response = operation => response,
            };
            if requests.cancel_request(&id).await && !self.disconnect.is_cancelled() {
                // Retire actual host acquisition too, not only the native response waiter.
                // This stays on the owning connection and is not replayable UI state.
                let notification = wire::ServerNotification::ServerRequestResolved(
                    wire::ServerRequestResolvedNotification {
                        thread_id: thread.to_string(), request_id: id,
                    },
                );
                let connection_ids = [self.connection];
                tokio::select! {
                    _ = self.disconnect.cancelled() => {},
                    _ = requests.send_server_notification_to_connections(&connection_ids, notification) => {},
                }
            }
            let _ = completed.send(result);
        });
        result
            .await
            .map_err(|_| CodexErr::Fatal("host resource cleanup stopped".to_owned()))?
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::outgoing_message::OutgoingEnvelope;
    use crate::outgoing_message::OutgoingMessage;
    use codex_app_server_protocol::ServerRequest;
    use codex_core::FinalCaptureRequest;
    use codex_protocol::ThreadId;
    use codex_state::coordination::WorkStamp;
    use pretty_assertions::assert_eq;
    use serde_json::json;
    use tokio::sync::mpsc;
    use uuid::Uuid;

    fn test_callbacks(
        outgoing: &Arc<OutgoingMessageSender>,
        owner: ConnectionId,
        disconnect: CancellationToken,
    ) -> AgentHostCallbacks {
        host_callbacks(
            outgoing,
            owner,
            disconnect,
            Arc::new(std::sync::Mutex::new(
                crate::feature_use::FeatureUse::default(),
            )),
        )
    }

    fn final_request() -> FinalCaptureRequest {
        FinalCaptureRequest {
            work: WorkStamp {
                root_id: ThreadId::new(),
                actor_id: ThreadId::new(),
                work_id: Uuid::now_v7(),
            },
            recipient: ThreadId::new(),
            recipient_cwd: Some("/recipient-workspace".into()),
            sources: vec!["mchanges:amber1".to_owned()],
        }
    }

    #[tokio::test]
    async fn detached_coordination_is_an_owner_notification_not_a_replayable_request() {
        use codex_protocol::agent_coordination::*;
        let (tx, mut wire) = mpsc::channel(2);
        let outgoing = Arc::new(OutgoingMessageSender::new(
            tx,
            codex_analytics::AnalyticsEventsClient::disabled(),
        ));
        let owner = ConnectionId(7);
        let disconnect = CancellationToken::new();
        let callbacks = test_callbacks(&outgoing, owner, disconnect.clone());
        let actor = ThreadId::new();
        let event = AgentCoordinationEvent {
            thread_id: actor,
            turn_id: "wait-turn".into(),
            projection: CoordinationProjection {
                deliveries: vec![],
                interrupt: None,
                wait: Some(CoordinationWaitSettlement {
                    wait_id: Uuid::now_v7(),
                    caller_thread_id: actor,
                    caller_work_id: Uuid::now_v7(),
                    origin: CoordinationWaitOrigin::Cell {
                        turn_id: "wait-turn".into(),
                        item_id: "wait-call".into(),
                        window_id: "window".into(),
                        cell_id: "cell".into(),
                    },
                    mode: CoordinationWaitMode::All,
                    selected: vec![],
                    claim_id: None,
                    result: CoordinationWaitResult::Cancelled {
                        cause: CoordinationWaitCancellation::ExecutionFailed,
                    },
                    messages: vec![],
                    outcomes: vec![],
                }),
            },
        };
        (callbacks.publish_coordination)(event.clone()).await;
        let Some(OutgoingEnvelope::ToConnection {
            connection_id,
            message: OutgoingMessage::AppServerNotification(envelope),
            ..
        }) = wire.recv().await
        else {
            panic!("missing owner notification");
        };
        assert_eq!(connection_id, owner);
        let wire::ServerNotification::AgentCoordination(actual) = envelope.notification else {
            panic!("coordination was not a semantic notification");
        };
        assert_eq!(actual, event);
        assert!(outgoing.pending_requests_for_thread(actor).await.is_empty());
        outgoing
            .replay_requests_to_connection_for_thread(ConnectionId(8), actor)
            .await;
        assert!(wire.try_recv().is_err());
        disconnect.cancel();
        (callbacks.publish_coordination)(event).await;
        assert!(wire.try_recv().is_err());
    }

    #[tokio::test]
    async fn final_capture_is_bound_to_owner_and_never_replayed_as_a_prompt() {
        let (tx, mut wire) = mpsc::channel(4);
        let outgoing = Arc::new(OutgoingMessageSender::new(
            tx,
            codex_analytics::AnalyticsEventsClient::disabled(),
        ));
        let owner = ConnectionId(7);
        let input = final_request();
        let sender = input.work.actor_id;
        let root = input.work.root_id;
        let preparer =
            test_callbacks(&outgoing, owner, CancellationToken::new()).prepare_final_capture;
        let task = tokio::spawn(preparer(input, CancellationToken::new()));
        let Some(OutgoingEnvelope::ToConnection {
            connection_id,
            message:
                OutgoingMessage::Request(ServerRequest::AgentPrepareFinalCapture { request_id, params }),
            ..
        }) = wire.recv().await
        else {
            panic!("missing owned capture request");
        };
        assert_eq!(connection_id, owner);
        assert_eq!(params.thread_id, sender.to_string());
        assert_eq!(params.root_thread_id, root.to_string());
        assert_eq!(params.sources, vec!["mchanges:amber1"]);
        assert_eq!(params.recipient_cwd, Some("/recipient-workspace".into()));
        assert!(
            outgoing
                .pending_requests_for_thread(sender)
                .await
                .is_empty()
        );
        outgoing
            .replay_requests_to_connection_for_thread(ConnectionId(8), sender)
            .await;
        assert!(wire.try_recv().is_err());
        outgoing
            .notify_client_response(
                ConnectionId(8),
                request_id.clone(),
                json!({"capture": {"resourceRef": "wrong", "fullText": "wrong connection"}}),
            )
            .await;
        outgoing
            .notify_client_response(
                owner,
                request_id,
                json!({"capture": {"resourceRef": "capture-1", "fullText": "frozen evidence"}}),
            )
            .await;
        assert_eq!(
            task.await.unwrap().unwrap(),
            Some(progress::FrozenFinalCapture {
                resource_ref: "capture-1".to_owned(),
                full_text: "frozen evidence".to_owned(),
            })
        );
    }

    #[tokio::test]
    async fn foreign_callbacks_preserve_authority_without_prompt_replay() {
        let (tx, mut wire) = mpsc::channel(4);
        let outgoing = Arc::new(OutgoingMessageSender::new(
            tx,
            codex_analytics::AnalyticsEventsClient::disabled(),
        ));
        let owner = ConnectionId(5);
        let callbacks = test_callbacks(&outgoing, owner, CancellationToken::new());
        let root = ThreadId::new();
        let actor = ThreadId::new();
        let target = ThreadId::new();
        let source = wire::AgentForeignOrigin {
            root_thread_id: root,
            actor_id: actor,
            agent_path: codex_protocol::AgentPath::root(),
            turn_id: "turn-1".into(),
            call_id: "call-1".into(),
        };
        let task = tokio::spawn((callbacks.route_foreign)(
            wire::AgentForeignRequest {
                source: source.clone(),
                target_thread_id: target,
                operation: wire::AgentForeignOperation::SideQuestion {
                    question: "Current finding?".into(),
                },
            },
            CancellationToken::new(),
        ));
        let Some(OutgoingEnvelope::ToConnection {
            connection_id,
            message:
                OutgoingMessage::Request(ServerRequest::AgentForeignRoute { request_id, params }),
            ..
        }) = wire.recv().await
        else {
            panic!("missing foreign routing request");
        };
        assert_eq!(connection_id, owner);
        assert_eq!(params.source, source);
        assert_eq!(params.target_thread_id, target);
        outgoing.notify_client_response(owner, request_id, json!({
            "type": "sideQuestionFailed", "targetThreadId": target, "accepted": true, "error": "provider failed"
        })).await;
        assert_eq!(
            task.await.unwrap().unwrap(),
            wire::AgentForeignResponse::SideQuestionFailed {
                target_thread_id: target,
                accepted: true,
                error: "provider failed".into(),
            }
        );
    }

    #[tokio::test]
    async fn final_capture_cancellation_retires_exact_host_request() {
        for disconnects in [false, true] {
            let (tx, mut wire) = mpsc::channel(1);
            let outgoing = Arc::new(OutgoingMessageSender::new(
                tx,
                codex_analytics::AnalyticsEventsClient::disabled(),
            ));
            let disconnect = CancellationToken::new();
            let cancellation = CancellationToken::new();
            let preparer = test_callbacks(&outgoing, ConnectionId(1), disconnect.clone())
                .prepare_final_capture;
            let task = tokio::spawn(preparer(final_request(), cancellation.clone()));
            let Some(OutgoingEnvelope::ToConnection {
                message: OutgoingMessage::Request(request),
                ..
            }) = wire.recv().await
            else {
                panic!("missing capture request");
            };
            if disconnects {
                disconnect.cancel();
            } else {
                cancellation.cancel();
            }
            assert!(task.await.unwrap().is_err());
            assert!(!outgoing.cancel_request(request.id()).await);
            if !disconnects {
                let Some(OutgoingEnvelope::ToConnection {
                    connection_id,
                    message: OutgoingMessage::AppServerNotification(envelope),
                    ..
                }) = wire.recv().await
                else {
                    panic!("host acquisition was not canceled");
                };
                assert_eq!(connection_id, ConnectionId(1));
                let wire::ServerNotification::ServerRequestResolved(resolved) =
                    envelope.notification
                else {
                    panic!("unexpected cancellation notification");
                };
                assert_eq!(&resolved.request_id, request.id());
            }
        }
    }

    #[tokio::test]
    async fn progress_callbacks_preserve_frozen_identity_and_explicit_delivery_evidence() {
        let (tx, mut wire) = mpsc::channel(1);
        let outgoing = Arc::new(OutgoingMessageSender::new(
            tx,
            codex_analytics::AnalyticsEventsClient::disabled(),
        ));
        let owner = ConnectionId(5);
        let callbacks = test_callbacks(&outgoing, owner, CancellationToken::new());
        let work = final_request().work;
        let actor = progress::ProgressActor {
            thread_id: ThreadId::new(),
            work_id: Uuid::now_v7(),
            task_name: "/root/developer".into(),
        };
        let wait_id = Uuid::now_v7();
        let batch_id = Uuid::now_v7();
        let context = Uuid::now_v7();
        let cursor = progress::CaptureCursor {
            key: "opaque-record".into(),
            through_chunk: 2,
        };
        let interval = progress::CaptureInterval {
            key: cursor.key.clone(),
            change_id: "amber1".into(),
            record_order: 7,
            legacy_record_key: None,
            first_chunk: 3,
            last_chunk: 4,
        };
        let coverage = progress::CaptureCoverage {
            status: "complete".into(),
            diagnostics: vec![],
        };
        let task = tokio::spawn((callbacks.freeze_progress)(
            codex_core::FreezeProgressRequest {
                caller: work,
                wait_id,
                batch_id,
                recipient_context_id: context,
                selected: vec![actor.clone()],
                seen: vec![cursor.clone()],
            },
            CancellationToken::new(),
        ));
        let Some(OutgoingEnvelope::ToConnection {
            connection_id,
            message:
                OutgoingMessage::Request(ServerRequest::AgentFreezeProgress { request_id, params }),
            ..
        }) = wire.recv().await
        else {
            panic!("missing progress freeze request");
        };
        assert_eq!(connection_id, owner);
        assert_eq!(
            (
                params.root_thread_id,
                params.caller_thread_id,
                params.caller_work_id
            ),
            (
                work.root_id.to_string(),
                work.actor_id.to_string(),
                work.work_id.to_string()
            )
        );
        assert_eq!(
            (params.wait_id, params.batch_id, params.recipient_context_id),
            (
                wait_id.to_string(),
                batch_id.to_string(),
                context.to_string()
            )
        );
        assert_eq!(params.selected, vec![progress_actor(actor.clone())]);
        assert_eq!(params.seen, vec![capture_cursor(cursor.clone())]);
        assert!(
            outgoing
                .pending_requests_for_thread(work.actor_id)
                .await
                .is_empty()
        );
        outgoing
            .notify_client_response(
                owner,
                request_id,
                serde_json::to_value(wire::AgentFreezeProgressResponse {
                    snapshot_ref: "frozen-1".into(),
                    publication_revision: 19,
                    agents: vec![wire::AgentFrozenProgressCapture {
                        actor: progress_actor(actor.clone()),
                        intervals: vec![capture_interval(interval.clone())],
                    }],
                    coverage: wire::AgentCaptureCoverage {
                        status: coverage.status.clone(),
                        diagnostics: vec![],
                    },
                })
                .unwrap(),
            )
            .await;
        let frozen = task.await.unwrap().unwrap();
        assert_eq!(
            frozen.agents,
            vec![progress::FrozenAgentCapture {
                actor: actor.clone(),
                intervals: vec![interval.clone()],
            }]
        );

        let task = tokio::spawn((callbacks.prepare_progress_output)(
            codex_core::PrepareProgressOutputRequest {
                caller: work,
                wait_id,
                batch_id,
                recipient_context_id: context,
                snapshot_ref: frozen.snapshot_ref,
                agents: vec![progress::PreparedProgressAgent {
                    actor: actor.clone(),
                    intervals: vec![interval.clone()],
                    summary: progress::SummaryResult::Available {
                        text: "Changed parser admission".into(),
                    },
                }],
                coverage,
            },
            CancellationToken::new(),
        ));
        let Some(OutgoingEnvelope::ToConnection {
            connection_id,
            message:
                OutgoingMessage::Request(ServerRequest::AgentPrepareProgressOutput {
                    request_id,
                    params,
                }),
            ..
        }) = wire.recv().await
        else {
            panic!("missing progress output request");
        };
        assert_eq!(connection_id, owner);
        assert_eq!(params.snapshot_ref, "frozen-1");
        assert_eq!(params.recipient_context_id, context.to_string());
        assert_eq!(params.agents[0].intervals, vec![capture_interval(interval)]);
        assert_eq!(
            params.agents[0].summary,
            wire::AgentProgressSummary::Available {
                text: "Changed parser admission".into()
            }
        );
        outgoing
            .notify_client_response(
                owner,
                request_id,
                serde_json::to_value(wire::AgentPrepareProgressOutputResponse {
                    text: "Read retained progress".into(),
                    recovery_ref: Some("recovery-1".into()),
                    delivered_intervals: vec![],
                })
                .unwrap(),
            )
            .await;
        let prepared = task.await.unwrap().unwrap();
        assert_eq!(prepared.recovery_ref.as_deref(), Some("recovery-1"));
        assert!(prepared.delivered_intervals.is_empty());

        let recipient = ThreadId::new();
        let task = tokio::spawn((callbacks.project_final_capture)(
            codex_core::ProjectFinalCaptureRequest {
                work,
                recipient,
                recipient_cwd: Some("/recipient".into()),
                recipient_context_id: context,
                resource_ref: "immutable-final-1".into(),
                seen: vec![cursor],
            },
            CancellationToken::new(),
        ));
        let Some(OutgoingEnvelope::ToConnection {
            connection_id,
            message:
                OutgoingMessage::Request(ServerRequest::AgentProjectFinalCapture { request_id, params }),
            ..
        }) = wire.recv().await
        else {
            panic!("missing final projection request");
        };
        assert_eq!(connection_id, owner);
        assert_eq!(params.resource_ref, "immutable-final-1");
        assert_eq!(params.recipient_thread_id, recipient.to_string());
        assert_eq!(params.recipient_context_id, context.to_string());
        assert_eq!(params.seen[0].through_chunk, 2);
        outgoing
            .notify_client_response(
                owner,
                request_id,
                json!({"text": "unacknowledged frozen suffix"}),
            )
            .await;
        assert_eq!(
            task.await.unwrap().unwrap().text,
            "unacknowledged frozen suffix"
        );
    }

    #[tokio::test]
    async fn abandoned_capture_is_joined_and_retires_its_callback() {
        let (tx, mut wire) = mpsc::channel(1);
        let outgoing = Arc::new(OutgoingMessageSender::new(
            tx,
            codex_analytics::AnalyticsEventsClient::disabled(),
        ));
        let preparer = test_callbacks(&outgoing, ConnectionId(1), CancellationToken::new())
            .prepare_final_capture;
        let task = tokio::spawn(preparer(final_request(), CancellationToken::new()));
        let Some(OutgoingEnvelope::ToConnection {
            message: OutgoingMessage::Request(request),
            ..
        }) = wire.recv().await
        else {
            panic!("missing capture request");
        };
        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());
        outgoing.drain_host_requests().await;
        assert!(!outgoing.cancel_request(request.id()).await);
    }

    #[tokio::test]
    async fn host_admission_requires_the_exact_child_acknowledgement() {
        for matches in [false, true] {
            let (tx, mut wire) = mpsc::channel(1);
            let outgoing = Arc::new(OutgoingMessageSender::new(
                tx,
                codex_analytics::AnalyticsEventsClient::disabled(),
            ));
            let owner = ConnectionId(3);
            let root = ThreadId::new();
            let parent = ThreadId::new();
            let child = ThreadId::new();
            let admit = test_callbacks(&outgoing, owner, CancellationToken::new()).admit_child;
            let task = tokio::spawn(admit(
                codex_core::ChildHostAdmissionRequest {
                    root_id: root,
                    parent_id: parent,
                    thread_id: child,
                    local_cwd: Some("/child-workspace".into()),
                },
                CancellationToken::new(),
            ));
            let Some(OutgoingEnvelope::ToConnection {
                connection_id,
                message:
                    OutgoingMessage::Request(ServerRequest::AgentPrepareHostAdmission {
                        request_id,
                        params,
                    }),
                ..
            }) = wire.recv().await
            else {
                panic!("missing child admission request");
            };
            assert_eq!(connection_id, owner);
            assert_eq!(
                (
                    params.root_thread_id,
                    params.parent_thread_id,
                    params.thread_id
                ),
                (root.to_string(), parent.to_string(), child.to_string())
            );
            assert_eq!(params.local_cwd, Some("/child-workspace".into()));
            assert!(
                outgoing
                    .pending_requests_for_thread(parent)
                    .await
                    .is_empty()
            );
            outgoing
                .notify_client_response(
                    ConnectionId(4),
                    request_id.clone(),
                    json!({"threadId": child.to_string()}),
                )
                .await;
            let acknowledged = if matches { child } else { ThreadId::new() };
            outgoing
                .notify_client_response(
                    owner,
                    request_id,
                    json!({"threadId": acknowledged.to_string()}),
                )
                .await;
            assert_eq!(task.await.unwrap().is_ok(), matches);
        }
    }
}

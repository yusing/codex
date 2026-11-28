use std::collections::HashMap;
use std::collections::HashSet;
use std::sync::Arc;
use std::sync::OnceLock;
use std::sync::atomic::AtomicI64;
use std::sync::atomic::Ordering;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

use codex_analytics::AnalyticsEventsClient;
use codex_app_server_protocol::ClientResponsePayload;
use codex_app_server_protocol::CommandExecutionApprovalDecision;
use codex_app_server_protocol::CommandExecutionApprovalKind;
use codex_app_server_protocol::JSONRPCErrorError;
use codex_app_server_protocol::RequestId;
use codex_app_server_protocol::Result;
use codex_app_server_protocol::ServerNotification;
use codex_app_server_protocol::ServerNotificationEnvelope;
use codex_app_server_protocol::ServerRequest;
use codex_app_server_protocol::ServerRequestPayload;
use codex_app_server_protocol::ServerRequestResolvedNotification;
use codex_app_server_protocol::ServerResponse;
use codex_app_server_protocol::ShellGuardApprovalKind;
use codex_diagnostics::Gauge;
use codex_diagnostics::GaugeGuard;
use codex_otel::span_w3c_trace_context;
use codex_protocol::ThreadId;
use codex_protocol::protocol::W3cTraceContext;
use codex_protocol::request_permissions::RequestPermissionsResponse;
use tokio::sync::Mutex;
use tokio::sync::mpsc;
use tokio::sync::oneshot;
use tracing::Instrument;
use tracing::Span;
use tracing::warn;

use crate::error_code::internal_error;
use crate::server_request_error::TURN_TRANSITION_PENDING_REQUEST_ERROR_REASON;
pub(crate) use codex_app_server_transport::ConnectionId;
pub(crate) use codex_app_server_transport::OutgoingError;
pub(crate) use codex_app_server_transport::OutgoingMessage;
pub(crate) use codex_app_server_transport::OutgoingResponse;
pub(crate) use codex_app_server_transport::QueuedOutgoingMessage;

#[cfg(test)]
use codex_protocol::account::PlanType;

pub(crate) type ClientRequestResult = std::result::Result<Result, JSONRPCErrorError>;

static IN_FLIGHT_REQUESTS: Gauge = Gauge::new("app.requests.in_flight");
static PENDING_SERVER_REQUESTS: Gauge = Gauge::new("app.server_requests.pending");

#[path = "account_notifications.rs"]
mod account_notifications;
pub(crate) use account_notifications::AccountNotification;

#[path = "user_verification_auth.rs"]
mod user_verification_auth;

/// Stable identifier for a client request scoped to a transport connection.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub(crate) struct ConnectionRequestId {
    pub(crate) connection_id: ConnectionId,
    pub(crate) request_id: RequestId,
}

/// Trace data and cancellation state retained until an incoming request's final response or error.
#[derive(Clone)]
pub(crate) struct RequestContext {
    request_id: ConnectionRequestId,
    pub(crate) cancellation: tokio_util::sync::CancellationToken,
    cancellation_scope: RequestCancellationScope,
    is_process_request: bool,
    span: Span,
    parent_trace: Option<W3cTraceContext>,
    _diagnostics_guard: Arc<GaugeGuard>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum RequestCancellationScope {
    Unavailable,
    UserVerification,
    ThreadSample,
    ThreadSearch,
    ThreadSearchStream,
}

impl RequestContext {
    pub(crate) fn new(
        request_id: ConnectionRequestId,
        method: &str,
        span: Span,
        parent_trace: Option<W3cTraceContext>,
    ) -> Self {
        Self {
            request_id,
            cancellation: tokio_util::sync::CancellationToken::new(),
            is_process_request: matches!(
                method,
                "process/spawn" | "process/writeStdin" | "process/resizePty" | "process/kill"
            ),
            cancellation_scope: match method {
                "userVerification/status"
                | "userVerification/enroll"
                | "userVerification/delete"
                | "userVerification/verify" => RequestCancellationScope::UserVerification,
                "thread/sample" => RequestCancellationScope::ThreadSample,
                "thread/search" => RequestCancellationScope::ThreadSearch,
                "thread/search/stream" => RequestCancellationScope::ThreadSearchStream,
                _ => RequestCancellationScope::Unavailable,
            },
            span,
            parent_trace,
            _diagnostics_guard: Arc::new(IN_FLIGHT_REQUESTS.track()),
        }
    }

    pub(crate) fn request_trace(&self) -> Option<W3cTraceContext> {
        span_w3c_trace_context(&self.span).or_else(|| self.parent_trace.clone())
    }

    pub(crate) fn span(&self) -> Span {
        self.span.clone()
    }

    fn record_turn_id(&self, turn_id: &str) {
        self.span.record("turn.id", turn_id);
    }
}

#[derive(Debug)]
pub(crate) enum OutgoingEnvelope {
    ToConnection {
        connection_id: ConnectionId,
        message: OutgoingMessage,
        write_complete_tx: Option<oneshot::Sender<()>>,
    },
    Broadcast {
        message: OutgoingMessage,
    },
}

pub(crate) enum NotificationTarget {
    Broadcast,
    Connection(ConnectionId),
}

/// Sends messages to the client and manages request callbacks.
pub(crate) struct OutgoingMessageSender {
    verification_auth: OnceLock<Arc<codex_login::AuthManager>>,
    verification_connections: Mutex<HashSet<ConnectionId>>,
    human_approval_connections: Mutex<HashSet<ConnectionId>>,
    remote_human_connections: Mutex<HashSet<ConnectionId>>,
    remembered_shell_guards: Mutex<HashMap<ConnectionId, HashSet<ShellGuardApprovalKey>>>,
    next_server_request_id: AtomicI64,
    sender: mpsc::Sender<OutgoingEnvelope>,
    process_reply_sender: mpsc::Sender<OutgoingEnvelope>,
    request_id_to_callback: Mutex<HashMap<RequestId, PendingCallbackEntry>>,
    /// Incoming requests that are still waiting on a final response or error.
    /// We keep them here because this is where responses, errors, and
    /// disconnect cleanup all get handled.
    request_contexts: Mutex<HashMap<ConnectionRequestId, RequestContext>>,
    analytics_events_client: AnalyticsEventsClient,
}

#[derive(Clone)]
pub(crate) struct ThreadScopedOutgoingMessageSender {
    outgoing: Arc<OutgoingMessageSender>,
    connection_ids: Arc<Vec<ConnectionId>>,
    thread_id: ThreadId,
}

struct PendingCallbackEntry {
    approval_recipients: HashSet<ConnectionId>,
    verification_owner: Option<ConnectionId>,
    verification_auth_revision: Option<u64>,
    verification_identity: Option<user_verification_auth::Identity>,
    callback: oneshot::Sender<ClientRequestResult>,
    thread_id: Option<ThreadId>,
    request: ServerRequest,
    _diagnostics_guard: GaugeGuard,
}

#[derive(PartialEq, Eq, Hash)]
struct ShellGuardApprovalKey {
    thread_id: ThreadId,
    cwd: String,
    executable: String,
    argv: Vec<String>,
}

impl ShellGuardApprovalKey {
    fn for_request(request: &ServerRequest, thread_id: Option<ThreadId>) -> Option<Self> {
        let ServerRequest::ShellGuardRequestApproval { params, .. } = request else {
            return None;
        };
        if params.kind != ShellGuardApprovalKind::RemoteWrite {
            return None;
        }
        Some(Self {
            thread_id: thread_id?,
            cwd: params.cwd.clone(),
            executable: params.executable.clone(),
            argv: params.argv.clone(),
        })
    }
}

impl ThreadScopedOutgoingMessageSender {
    pub(crate) fn new(
        outgoing: Arc<OutgoingMessageSender>,
        connection_ids: Vec<ConnectionId>,
        thread_id: ThreadId,
    ) -> Self {
        Self {
            outgoing,
            connection_ids: Arc::new(connection_ids),
            thread_id,
        }
    }

    pub(crate) async fn send_request(
        &self,
        payload: ServerRequestPayload,
    ) -> (RequestId, oneshot::Receiver<ClientRequestResult>) {
        self.outgoing
            .send_request_to_connections(
                Some(self.connection_ids.as_slice()),
                payload,
                Some(self.thread_id),
            )
            .await
    }

    pub(crate) fn track_effective_permissions_approval_response(
        &self,
        request_id: RequestId,
        response: RequestPermissionsResponse,
    ) {
        self.outgoing
            .analytics_events_client
            .track_effective_permissions_approval_response(
                now_unix_timestamp_ms(),
                request_id,
                response,
            );
    }

    pub(crate) fn track_realtime_session_updated(&self, realtime_session_id: String) {
        self.outgoing
            .analytics_events_client
            .track_realtime_session_updated(self.thread_id.to_string(), realtime_session_id);
    }

    pub(crate) async fn send_server_notification(&self, notification: ServerNotification) {
        self.outgoing
            .analytics_events_client
            .track_notification(&notification);
        if self.connection_ids.is_empty() {
            return;
        }
        self.outgoing
            .send_server_notification_to_connections(self.connection_ids.as_slice(), notification)
            .await;
    }

    pub(crate) async fn send_global_server_notification(&self, notification: ServerNotification) {
        self.outgoing.send_server_notification(notification).await;
    }

    pub(crate) fn try_send_optional_notification(&self, notification: ServerNotification) {
        let notifications = std::iter::repeat_n(notification, self.connection_ids.len());
        for (&connection_id, notification) in self.connection_ids.iter().zip(notifications) {
            if !self.outgoing.try_send_server_notification(
                NotificationTarget::Connection(connection_id),
                notification,
            ) {
                break;
            }
        }
    }

    pub(crate) async fn resolve_server_request(&self, request_id: RequestId) {
        self.outgoing.cancel_request(&request_id).await;
        self.send_server_notification(ServerNotification::ServerRequestResolved(
            ServerRequestResolvedNotification {
                thread_id: self.thread_id.to_string(),
                request_id,
            },
        ))
        .await;
    }

    pub(crate) async fn abort_pending_server_requests(&self) {
        self.outgoing
            .cancel_requests_for_thread(
                self.thread_id,
                Some({
                    let mut error = internal_error(
                        "client request resolved because the turn state was changed",
                    );
                    error.data = Some(serde_json::json!({
                        "reason": TURN_TRANSITION_PENDING_REQUEST_ERROR_REASON,
                    }));
                    error
                }),
            )
            .await
    }

    pub(crate) async fn send_response<T>(&self, request_id: ConnectionRequestId, response: T)
    where
        T: Into<ClientResponsePayload>,
    {
        self.outgoing.send_response(request_id, response).await;
    }
}

impl OutgoingMessageSender {
    pub(crate) fn new(
        sender: mpsc::Sender<OutgoingEnvelope>,
        analytics_events_client: AnalyticsEventsClient,
    ) -> Self {
        Self {
            verification_auth: OnceLock::new(),
            verification_connections: Mutex::new(HashSet::new()),
            human_approval_connections: Mutex::new(HashSet::new()),
            remote_human_connections: Mutex::new(HashSet::new()),
            remembered_shell_guards: Mutex::new(HashMap::new()),
            next_server_request_id: AtomicI64::new(0),
            process_reply_sender: sender.clone(),
            sender,
            request_id_to_callback: Mutex::new(HashMap::new()),
            request_contexts: Mutex::new(HashMap::new()),
            analytics_events_client,
        }
    }

    pub(crate) fn with_process_reply_sender(
        mut self,
        sender: mpsc::Sender<OutgoingEnvelope>,
    ) -> Self {
        self.process_reply_sender = sender;
        self
    }

    pub(crate) async fn register_request_context(&self, request_context: RequestContext) {
        let mut request_contexts = self.request_contexts.lock().await;
        if request_contexts
            .insert(request_context.request_id.clone(), request_context)
            .is_some()
        {
            warn!("replaced unresolved request context");
        }
    }

    pub(crate) async fn cancel_user_verification_request(&self, request_id: &ConnectionRequestId) {
        let contexts = self.request_contexts.lock().await;
        if let Some(context) = contexts.get(request_id)
            && context.cancellation_scope == RequestCancellationScope::UserVerification
        {
            context.cancellation.cancel();
        }
    }

    pub(crate) async fn cancel_thread_sample_request(&self, request_id: &ConnectionRequestId) {
        let contexts = self.request_contexts.lock().await;
        if let Some(context) = contexts.get(request_id)
            && context.cancellation_scope == RequestCancellationScope::ThreadSample
        {
            context.cancellation.cancel();
        }
    }

    /// Capture the exact target at ordered ingress; cancellation must not resolve a reused ID later.
    pub(crate) async fn search_cancellation_target(
        &self,
        request_id: &ConnectionRequestId,
        streaming: bool,
    ) -> Option<tokio_util::sync::CancellationToken> {
        let scope = if streaming {
            RequestCancellationScope::ThreadSearchStream
        } else {
            RequestCancellationScope::ThreadSearch
        };
        self.request_contexts
            .lock()
            .await
            .get(request_id)
            .filter(|context| context.cancellation_scope == scope)
            .map(|context| context.cancellation.clone())
    }

    pub(crate) async fn enable_human_approval_connection(&self, connection_id: ConnectionId) {
        self.human_approval_connections
            .lock()
            .await
            .insert(connection_id);
    }

    pub(crate) async fn register_remote_human_connection(&self, connection_id: ConnectionId) {
        self.remote_human_connections
            .lock()
            .await
            .insert(connection_id);
    }

    pub(crate) async fn connection_closed(&self, connection_id: ConnectionId) {
        self.human_approval_connections
            .lock()
            .await
            .remove(&connection_id);
        self.remote_human_connections
            .lock()
            .await
            .remove(&connection_id);
        {
            let mut callbacks = self.request_id_to_callback.lock().await;
            for entry in callbacks.values_mut() {
                entry.approval_recipients.remove(&connection_id);
            }
            self.remembered_shell_guards
                .lock()
                .await
                .remove(&connection_id);
        }
        self.disconnect_user_verification_connection(connection_id)
            .await;
        let mut request_contexts = self.request_contexts.lock().await;
        request_contexts.retain(|request_id, context| {
            if request_id.connection_id != connection_id {
                return true;
            }
            if matches!(
                context.cancellation_scope,
                RequestCancellationScope::ThreadSearch
                    | RequestCancellationScope::ThreadSearchStream
            ) {
                context.cancellation.cancel();
            }
            false
        });
    }

    #[tracing::instrument(skip_all)]
    pub(crate) async fn forget_shell_guard_approvals(
        &self,
        thread_id: ThreadId,
        connection_id: Option<ConnectionId>,
    ) {
        // Serialize grant removal with approval responses for the detached scope.
        let mut callbacks = self.request_id_to_callback.lock().await;
        for entry in callbacks
            .values_mut()
            .filter(|entry| entry.thread_id == Some(thread_id))
        {
            match connection_id {
                Some(id) => {
                    entry.approval_recipients.remove(&id);
                }
                None => entry.approval_recipients.clear(),
            }
        }
        self.remembered_shell_guards
            .lock()
            .await
            .retain(|id, grants| {
                if connection_id.is_none_or(|connection| *id == connection) {
                    grants.retain(|key| key.thread_id != thread_id);
                }
                !grants.is_empty()
            });
    }

    pub(crate) async fn request_trace_context(
        &self,
        request_id: &ConnectionRequestId,
    ) -> Option<W3cTraceContext> {
        let request_contexts = self.request_contexts.lock().await;
        request_contexts
            .get(request_id)
            .and_then(RequestContext::request_trace)
    }

    pub(crate) async fn record_request_turn_id(
        &self,
        request_id: &ConnectionRequestId,
        turn_id: &str,
    ) {
        let request_contexts = self.request_contexts.lock().await;
        if let Some(request_context) = request_contexts.get(request_id) {
            request_context.record_turn_id(turn_id);
        }
    }

    async fn take_request_context(
        &self,
        request_id: &ConnectionRequestId,
    ) -> Option<RequestContext> {
        let mut request_contexts = self.request_contexts.lock().await;
        request_contexts.remove(request_id)
    }

    #[cfg(test)]
    async fn request_context_count(&self) -> usize {
        self.request_contexts.lock().await.len()
    }

    pub(crate) async fn send_request(
        &self,
        request: ServerRequestPayload,
    ) -> (RequestId, oneshot::Receiver<ClientRequestResult>) {
        self.send_request_to_connections(
            /*connection_ids*/ None, request, /*thread_id*/ None,
        )
        .await
    }

    pub(crate) fn next_request_id(&self) -> RequestId {
        RequestId::Integer(self.next_server_request_id.fetch_add(1, Ordering::Relaxed))
    }

    pub(crate) async fn send_request_to_connections(
        &self,
        connection_ids: Option<&[ConnectionId]>,
        request: ServerRequestPayload,
        thread_id: Option<ThreadId>,
    ) -> (RequestId, oneshot::Receiver<ClientRequestResult>) {
        let id = self.next_request_id();
        let outgoing_message_id = id.clone();
        let request = request.request_with_id(outgoing_message_id.clone());
        let user_verification = matches!(
            &request,
            ServerRequest::McpServerElicitationRequest { params, .. }
                if matches!(&params.request, codex_app_server_protocol::McpServerElicitationRequest::UserVerification { .. })
        );
        // Snapshot before waiting on eligibility or callback locks. A request cannot inherit
        // whichever account happens to be current after an unrelated operation releases a lock.
        let verification_auth_revision = user_verification
            .then(|| self.verification_auth_revision())
            .flatten();
        let verification_identity = user_verification
            .then(|| self.verification_identity())
            .flatten();
        let auth_changed = || {
            user_verification
                && (verification_auth_revision != self.verification_auth_revision()
                    || verification_identity != self.verification_identity())
        };
        let (tx_approve, rx_approve) = oneshot::channel();
        let human_connections;
        let connection_ids = if requires_human_approval(&request) {
            let eligible = self.human_approval_connections.lock().await;
            human_connections = connection_ids
                .unwrap_or_default()
                .iter()
                .copied()
                .filter(|id| eligible.contains(id))
                .collect::<Vec<_>>();
            if human_connections.is_empty() {
                return (outgoing_message_id, rx_approve);
            }
            Some(human_connections.as_slice())
        } else {
            connection_ids
        };
        // One app owns this ceremony. Reconnect and other subscribers cannot answer it.
        let verification_owner = if user_verification {
            let eligible = self.verification_connections.lock().await;
            connection_ids
                .and_then(|ids| ids.iter().find(|id| eligible.contains(*id)))
                .copied()
        } else {
            None
        };
        if user_verification && (verification_owner.is_none() || auth_changed()) {
            return (outgoing_message_id, rx_approve);
        }
        let connection_ids = if user_verification {
            verification_owner.as_ref().map(std::slice::from_ref)
        } else {
            connection_ids
        };
        let local_secret_connections;
        let connection_ids = if requires_local_secret_authority(&request) {
            if let Some(ids) = connection_ids {
                let remote_human = self.remote_human_connections.lock().await;
                local_secret_connections = ids
                    .iter()
                    .copied()
                    .filter(|id| !remote_human.contains(id))
                    .collect::<Vec<_>>();
                Some(local_secret_connections.as_slice())
            } else {
                None
            }
        } else {
            connection_ids
        };
        if let Some(key) = ShellGuardApprovalKey::for_request(&request, thread_id)
            && let Some(ids) = connection_ids
            && {
                let grants = self.remembered_shell_guards.lock().await;
                ids.iter()
                    .any(|id| grants.get(id).is_some_and(|keys| keys.contains(&key)))
            }
        {
            let _ = tx_approve.send(Ok(serde_json::json!({ "decision": "accept" })));
            return (outgoing_message_id, rx_approve);
        }
        {
            let mut request_id_to_callback = self.request_id_to_callback.lock().await;
            request_id_to_callback.insert(
                id,
                PendingCallbackEntry {
                    approval_recipients: if tracks_approval_recipients(&request) {
                        connection_ids.unwrap_or_default().iter().copied().collect()
                    } else {
                        HashSet::new()
                    },
                    verification_owner,
                    verification_auth_revision,
                    verification_identity: verification_identity.clone(),
                    callback: tx_approve,
                    thread_id,
                    request: request.clone(),
                    _diagnostics_guard: PENDING_SERVER_REQUESTS.track(),
                },
            );
        }
        // Disconnect may finish its callback cleanup before registration acquires the lock.
        // Recheck afterward so that ordering cannot leave an orphaned verification callback.
        if let Some(owner) = verification_owner {
            let eligible = self.verification_connections.lock().await.contains(&owner);
            if !eligible || auth_changed() {
                self.request_id_to_callback
                    .lock()
                    .await
                    .remove(&outgoing_message_id);
                return (outgoing_message_id, rx_approve);
            }
        }

        let outgoing_message = OutgoingMessage::Request(request.clone());
        let send_result = match connection_ids {
            None => {
                self.sender
                    .send(OutgoingEnvelope::Broadcast {
                        message: outgoing_message,
                    })
                    .await
            }
            Some(connection_ids) => {
                let mut send_error = None;
                for connection_id in connection_ids {
                    if let Err(err) = self
                        .sender
                        .send(OutgoingEnvelope::ToConnection {
                            connection_id: *connection_id,
                            message: outgoing_message.clone(),
                            write_complete_tx: None,
                        })
                        .await
                    {
                        send_error = Some(err);
                        break;
                    } else {
                        self.analytics_events_client
                            .track_server_request(connection_id.0, request.clone());
                    }
                }
                match send_error {
                    Some(err) => Err(err),
                    None => Ok(()),
                }
            }
        };

        if let Err(err) = send_result {
            warn!("failed to send request {outgoing_message_id:?} to client: {err:?}");
            let mut request_id_to_callback = self.request_id_to_callback.lock().await;
            request_id_to_callback.remove(&outgoing_message_id);
        }
        (outgoing_message_id, rx_approve)
    }

    pub(crate) async fn replay_requests_to_connection_for_thread(
        &self,
        connection_id: ConnectionId,
        thread_id: ThreadId,
    ) {
        let requests = self.pending_requests_for_thread(thread_id).await;
        let human_approval = self
            .human_approval_connections
            .lock()
            .await
            .contains(&connection_id);
        let is_remote_human = self
            .remote_human_connections
            .lock()
            .await
            .contains(&connection_id);
        for request in requests {
            if requires_local_secret_authority(&request) && is_remote_human {
                continue;
            }
            if requires_human_approval(&request) && !human_approval {
                continue;
            }
            if tracks_approval_recipients(&request) {
                // Register the replay recipient before sending so its response
                // cannot race ahead of callback authorization.
                let mut callbacks = self.request_id_to_callback.lock().await;
                let Some(entry) = callbacks.get_mut(request.id()) else {
                    continue;
                };
                entry.approval_recipients.insert(connection_id);
            }
            if let Err(err) = self
                .sender
                .send(OutgoingEnvelope::ToConnection {
                    connection_id,
                    message: OutgoingMessage::Request(request),
                    write_complete_tx: None,
                })
                .await
            {
                warn!("failed to resend request to client: {err:?}");
            }
        }
    }

    pub(crate) async fn notify_client_response(
        &self,
        connection_id: ConnectionId,
        id: RequestId,
        result: Result,
    ) {
        let entry = self
            .take_connection_callback_for_response(connection_id, &id, Some(&result))
            .await;

        match entry {
            Some((id, entry)) => {
                let completed_at_ms = now_unix_timestamp_ms();
                // Do not clone image bytes into the response trace or analytics queue.
                let has_question_images =
                    matches!(entry.request, ServerRequest::ToolRequestUserInput { .. })
                        && result["answers"].as_array().is_some_and(|answers| {
                            answers.iter().any(|answer| answer.get("images").is_some())
                        });
                if entry.verification_owner.is_none()
                    && !has_question_images
                    && !matches!(entry.request, ServerRequest::ShellGuardRequestSecret { .. })
                    && let Ok(response) = entry.request.response_from_result(result.clone())
                {
                    tracing::info!("<- response: {response:?}");
                    if !matches!(response, ServerResponse::PermissionsRequestApproval { .. }) {
                        self.analytics_events_client
                            .track_server_response(completed_at_ms, response);
                    }
                }
                if entry.callback.send(Ok(result)).is_err() {
                    warn!("could not notify callback for {id:?}: receiver dropped");
                }
            }
            None => {
                warn!("could not find callback for {id:?}");
            }
        }
    }

    pub(crate) async fn notify_client_error(
        &self,
        connection_id: ConnectionId,
        id: RequestId,
        error: JSONRPCErrorError,
    ) {
        let entry = self.take_connection_callback(connection_id, &id).await;

        match entry {
            Some((id, entry)) => {
                // Don't log error messages or data because they may contain credentials.
                warn!(code = error.code, "client responded with error for {id:?}");
                self.analytics_events_client
                    .track_server_request_aborted(now_unix_timestamp_ms(), id.clone());
                if entry.callback.send(Err(error)).is_err() {
                    warn!("could not notify callback for {id:?}: receiver dropped");
                }
            }
            None => {
                warn!("could not find callback for {id:?}");
            }
        }
    }

    pub(crate) async fn cancel_request(&self, id: &RequestId) -> bool {
        let entry = self.take_request_callback(id).await;
        if let Some((request_id, _entry)) = entry {
            self.analytics_events_client
                .track_server_request_aborted(now_unix_timestamp_ms(), request_id);
            true
        } else {
            false
        }
    }

    pub(crate) async fn cancel_all_requests(&self, error: Option<JSONRPCErrorError>) {
        let entries = {
            let mut request_id_to_callback = self.request_id_to_callback.lock().await;
            request_id_to_callback
                .drain()
                .map(|(_, entry)| entry)
                .collect::<Vec<_>>()
        };

        for entry in entries {
            self.analytics_events_client
                .track_server_request_aborted(now_unix_timestamp_ms(), entry.request.id().clone());
            if let Some(error) = error.as_ref()
                && entry.callback.send(Err(error.clone())).is_err()
            {
                let request_id = entry.request.id();
                warn!("could not notify callback for {request_id:?}: receiver dropped");
            }
        }
    }

    async fn take_request_callback(
        &self,
        id: &RequestId,
    ) -> Option<(RequestId, PendingCallbackEntry)> {
        let mut request_id_to_callback = self.request_id_to_callback.lock().await;
        request_id_to_callback.remove_entry(id)
    }

    async fn take_connection_callback(
        &self,
        connection_id: ConnectionId,
        id: &RequestId,
    ) -> Option<(RequestId, PendingCallbackEntry)> {
        self.take_connection_callback_for_response(connection_id, id, /*result*/ None)
            .await
    }

    #[tracing::instrument(skip_all)]
    async fn take_connection_callback_for_response(
        &self,
        connection_id: ConnectionId,
        id: &RequestId,
        result: Option<&Result>,
    ) -> Option<(RequestId, PendingCallbackEntry)> {
        let human_approval = self
            .human_approval_connections
            .lock()
            .await
            .contains(&connection_id);
        let is_remote_human = self
            .remote_human_connections
            .lock()
            .await
            .contains(&connection_id);
        let mut callbacks = self.request_id_to_callback.lock().await;
        let entry = callbacks.get(id)?;
        if requires_local_secret_authority(&entry.request) && is_remote_human {
            warn!(
                ?connection_id,
                "rejecting remote-human response to local-only secret request {id:?}"
            );
            return None;
        }
        if is_remote_human
            && let Some(result) = result
            && is_disallowed_remote_human_approval_response(&entry.request, result)
        {
            warn!(
                ?connection_id,
                "rejecting persistent/session-wide approval decision from remote-human connection for {id:?}"
            );
            return None;
        }
        if requires_human_approval(&entry.request)
            && (!human_approval || !entry.approval_recipients.contains(&connection_id))
        {
            return None;
        }
        if let Some(owner) = entry.verification_owner {
            if owner != connection_id {
                return None;
            }
            if entry.verification_identity != self.verification_identity()
                || entry.verification_auth_revision != self.verification_auth_revision()
            {
                callbacks.remove(id);
                return None;
            }
        }
        if result.is_some_and(is_remember_approval) {
            let shell_key = ShellGuardApprovalKey::for_request(&entry.request, entry.thread_id);
            if (is_remote_human || shell_key.is_some())
                && !entry.approval_recipients.contains(&connection_id)
            {
                return None;
            }
            if let Some(key) = shell_key {
                self.remembered_shell_guards
                    .lock()
                    .await
                    .entry(connection_id)
                    .or_default()
                    .insert(key);
            }
        }
        callbacks.remove_entry(id)
    }

    pub(crate) async fn pending_requests_for_thread(
        &self,
        thread_id: ThreadId,
    ) -> Vec<ServerRequest> {
        let request_id_to_callback = self.request_id_to_callback.lock().await;
        let mut requests = request_id_to_callback
            .values()
            .filter_map(|entry| {
                (entry.thread_id == Some(thread_id) && entry.verification_owner.is_none())
                    .then_some(entry.request.clone())
            })
            .collect::<Vec<_>>();
        requests.sort_by(|left, right| left.id().cmp(right.id()));
        requests
    }

    pub(crate) async fn cancel_requests_for_thread(
        &self,
        thread_id: ThreadId,
        error: Option<JSONRPCErrorError>,
    ) {
        let entries = {
            let mut request_id_to_callback = self.request_id_to_callback.lock().await;
            let request_ids = request_id_to_callback
                .iter()
                .filter_map(|(request_id, entry)| {
                    (entry.thread_id == Some(thread_id)).then_some(request_id.clone())
                })
                .collect::<Vec<_>>();

            let mut entries = Vec::with_capacity(request_ids.len());
            for request_id in request_ids {
                if let Some(entry) = request_id_to_callback.remove(&request_id) {
                    entries.push(entry);
                }
            }
            entries
        };

        for entry in entries {
            self.analytics_events_client
                .track_server_request_aborted(now_unix_timestamp_ms(), entry.request.id().clone());
            if let Some(error) = error.as_ref()
                && entry.callback.send(Err(error.clone())).is_err()
            {
                let request_id = entry.request.id();
                warn!("could not notify callback for {request_id:?}: receiver dropped");
            }
        }
    }

    pub(crate) async fn send_response<T>(&self, request_id: ConnectionRequestId, response: T)
    where
        T: Into<ClientResponsePayload>,
    {
        self.send_response_as_inner(request_id, response.into(), /*thread_originator*/ None)
            .await;
    }

    pub(crate) async fn send_response_with_thread_originator<T>(
        &self,
        request_id: ConnectionRequestId,
        response: T,
        thread_originator: String,
    ) where
        T: Into<ClientResponsePayload>,
    {
        self.send_response_as_inner(request_id, response.into(), Some(thread_originator))
            .await;
    }

    pub(crate) async fn send_response_as(
        &self,
        request_id: ConnectionRequestId,
        response: ClientResponsePayload,
    ) {
        self.send_response_as_inner(request_id, response, /*thread_originator*/ None)
            .await;
    }

    /// Revalidates a sensitive result after reserving queue capacity, with no
    /// suspension between the identity check and handing off the response.
    pub(crate) async fn send_response_as_checked(
        &self,
        request_id: ConnectionRequestId,
        response: ClientResponsePayload,
        check: impl FnOnce() -> std::result::Result<(), JSONRPCErrorError>,
    ) {
        // Remain cancellable while waiting to deliver a proof, including after native work ends.
        let permit = self.sender.reserve().await;
        let _context = self.take_request_context(&request_id).await;
        let Ok(permit) = permit else {
            return;
        };
        let message = match check() {
            Ok(()) => {
                self.analytics_events_client.track_response(
                    request_id.connection_id.0,
                    request_id.request_id.clone(),
                    &response,
                );
                OutgoingMessage::Response(OutgoingResponse {
                    id: request_id.request_id,
                    result: Box::new(response),
                })
            }
            Err(error) => OutgoingMessage::Error(OutgoingError {
                id: request_id.request_id,
                error,
            }),
        };
        permit.send(OutgoingEnvelope::ToConnection {
            connection_id: request_id.connection_id,
            message,
            write_complete_tx: None,
        });
    }

    async fn send_response_as_inner(
        &self,
        request_id: ConnectionRequestId,
        response: ClientResponsePayload,
        thread_originator: Option<String>,
    ) {
        let connection_id = request_id.connection_id;
        let request_id_for_analytics = request_id.request_id.clone();
        match thread_originator {
            Some(thread_originator) => {
                self.analytics_events_client
                    .track_response_with_thread_originator(
                        connection_id.0,
                        request_id_for_analytics,
                        &response,
                        thread_originator,
                    );
            }
            None => {
                self.analytics_events_client.track_response(
                    connection_id.0,
                    request_id_for_analytics,
                    &response,
                );
            }
        }
        let response = Box::new(response);
        let request_context = self.take_request_context(&request_id).await;
        let outgoing_message = OutgoingMessage::Response(OutgoingResponse {
            id: request_id.request_id,
            result: response,
        });
        self.send_outgoing_message_to_connection(
            request_context,
            connection_id,
            outgoing_message,
            "response",
        )
        .await;
    }

    pub(crate) async fn send_server_notification(&self, notification: ServerNotification) {
        if matches!(
            notification,
            ServerNotification::ThreadArchived(_)
                | ServerNotification::ThreadClosed(_)
                | ServerNotification::ThreadUnarchived(_)
        ) {
            self.analytics_events_client
                .track_notification(&notification);
        }
        self.send_server_notification_to_connections(&[], notification)
            .await;
    }

    /// Queue an optional notification without delaying the request or other connections.
    pub(crate) fn try_send_server_notification(
        &self,
        target: NotificationTarget,
        notification: ServerNotification,
    ) -> bool {
        let permit = match self.sender.try_reserve() {
            Ok(permit) => permit,
            Err(error) => {
                if matches!(target, NotificationTarget::Broadcast) {
                    warn!("failed to queue server notification to clients: {error}");
                }
                return false;
            }
        };
        // Leave room for the authoritative response when progress fills the queue.
        if matches!(target, NotificationTarget::Connection(_)) && self.sender.capacity() == 0 {
            return false;
        }
        let message = timestamped_server_notification(notification);
        permit.send(match target {
            NotificationTarget::Broadcast => OutgoingEnvelope::Broadcast { message },
            NotificationTarget::Connection(connection_id) => OutgoingEnvelope::ToConnection {
                connection_id,
                message,
                write_complete_tx: None,
            },
        });
        true
    }

    pub(crate) async fn send_server_notification_to_connections(
        &self,
        connection_ids: &[ConnectionId],
        notification: ServerNotification,
    ) {
        tracing::trace!(
            targeted_connections = connection_ids.len(),
            "app-server event: {notification}"
        );
        let outgoing_message = timestamped_server_notification(notification);
        if connection_ids.is_empty() {
            if let Err(err) = self
                .sender
                .send(OutgoingEnvelope::Broadcast {
                    message: outgoing_message,
                })
                .await
            {
                warn!("failed to send server notification to client: {err:?}");
            }
            return;
        }
        let messages = std::iter::repeat_n(outgoing_message, connection_ids.len());
        for (connection_id, message) in connection_ids.iter().zip(messages) {
            if let Err(err) = self
                .sender
                .send(OutgoingEnvelope::ToConnection {
                    connection_id: *connection_id,
                    message,
                    write_complete_tx: None,
                })
                .await
            {
                warn!("failed to send server notification to client: {err:?}");
            }
        }
    }

    pub(crate) async fn send_server_notification_to_connection_and_wait(
        &self,
        connection_id: ConnectionId,
        notification: ServerNotification,
    ) -> bool {
        tracing::trace!("app-server event: {notification}");
        let outgoing_message = timestamped_server_notification(notification);
        let (write_complete_tx, write_complete_rx) = oneshot::channel();
        if let Err(err) = self
            .sender
            .send(OutgoingEnvelope::ToConnection {
                connection_id,
                message: outgoing_message,
                write_complete_tx: Some(write_complete_tx),
            })
            .await
        {
            warn!("failed to send server notification to client: {err:?}");
        }
        write_complete_rx.await.is_ok()
    }

    pub(crate) async fn send_error(
        &self,
        request_id: ConnectionRequestId,
        error: impl Into<JSONRPCErrorError>,
    ) {
        let request_context = self.take_request_context(&request_id).await;
        self.send_error_inner(request_context, request_id, error.into())
            .await;
    }

    pub(crate) async fn send_result<T, E>(
        &self,
        request_id: ConnectionRequestId,
        result: std::result::Result<T, E>,
    ) where
        T: Into<ClientResponsePayload>,
        E: Into<JSONRPCErrorError>,
    {
        match result {
            Ok(response) => {
                self.send_response(request_id, response).await;
            }
            Err(error) => self.send_error(request_id, error).await,
        }
    }

    async fn send_error_inner(
        &self,
        request_context: Option<RequestContext>,
        request_id: ConnectionRequestId,
        error: JSONRPCErrorError,
    ) {
        let outgoing_message = OutgoingMessage::Error(OutgoingError {
            id: request_id.request_id,
            error,
        });
        self.send_outgoing_message_to_connection(
            request_context,
            request_id.connection_id,
            outgoing_message,
            "error",
        )
        .await;
    }

    async fn send_outgoing_message_to_connection(
        &self,
        request_context: Option<RequestContext>,
        connection_id: ConnectionId,
        message: OutgoingMessage,
        message_kind: &'static str,
    ) {
        // Process requests serialize by handle. A blocked resize reply would also block the next kill.
        let sender = if request_context
            .as_ref()
            .is_some_and(|ctx| ctx.is_process_request)
        {
            &self.process_reply_sender
        } else {
            &self.sender
        };
        // The spawn reply must reach the embedder before the collectors can publish output.
        let (written_tx, written_rx) = (!sender.same_channel(&self.sender))
            .then(oneshot::channel)
            .unzip();
        let send_fut = sender.send(OutgoingEnvelope::ToConnection {
            connection_id,
            message,
            write_complete_tx: written_tx,
        });
        let send_result = if let Some(request_context) = request_context {
            send_fut.instrument(request_context.span()).await
        } else {
            send_fut.await
        };

        if let Err(err) = send_result {
            warn!("failed to send {message_kind} to client: {err:?}");
        } else if let Some(written_rx) = written_rx {
            let _ = written_rx.await;
        }
    }
}

fn now_unix_timestamp_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or_default()
}

fn timestamped_server_notification(notification: ServerNotification) -> OutgoingMessage {
    OutgoingMessage::AppServerNotification(ServerNotificationEnvelope {
        notification,
        emitted_at_ms: Some(now_unix_timestamp_ms().try_into().unwrap_or_default()),
    })
}

fn requires_human_approval(request: &ServerRequest) -> bool {
    matches!(request, ServerRequest::CommandExecutionRequestApproval { params, .. }
        if params.kind == CommandExecutionApprovalKind::HumanCommand)
}

fn tracks_approval_recipients(request: &ServerRequest) -> bool {
    matches!(
        request,
        ServerRequest::CommandExecutionRequestApproval { .. }
            | ServerRequest::FileChangeRequestApproval { .. }
            | ServerRequest::ShellGuardRequestApproval { .. }
    )
}

/// Private secret prompts (sudo password, GPG passphrase, and secret user-input
/// questions) require local native authority and are never delivered to or
/// answerable by remote-human connections.
pub(crate) fn requires_local_secret_authority(request: &ServerRequest) -> bool {
    match request {
        ServerRequest::ShellGuardRequestSecret { .. } => true,
        ServerRequest::ToolRequestUserInput { params, .. } => {
            params.questions.iter().any(|question| question.is_secret)
        }
        _ => false,
    }
}

fn is_remember_approval(result: &serde_json::Value) -> bool {
    result.get("decision").and_then(serde_json::Value::as_str) == Some("acceptForSession")
}

/// Allow session approval only for supported requests, never policy amendments
/// or general session permissions from a remote connection.
fn is_disallowed_remote_human_approval_response(
    request: &ServerRequest,
    result: &serde_json::Value,
) -> bool {
    if is_remember_approval(result) {
        return match request {
            ServerRequest::CommandExecutionRequestApproval { params, .. } => {
                params.kind != CommandExecutionApprovalKind::Command
                    || params.network_approval_context.is_some()
                    || params.additional_permissions.is_some()
                    || !params
                        .available_decisions
                        .as_ref()
                        .is_some_and(|decisions| {
                            decisions.contains(&CommandExecutionApprovalDecision::AcceptForSession)
                        })
            }
            ServerRequest::FileChangeRequestApproval { params, .. } => {
                params.reason.is_some() || params.grant_root.is_some()
            }
            ServerRequest::ShellGuardRequestApproval { params, .. } => {
                params.kind != ShellGuardApprovalKind::RemoteWrite
            }
            _ => true,
        };
    }
    let decision_is_persistent = |decision: &serde_json::Value| -> bool {
        match decision {
            serde_json::Value::String(value) => {
                matches!(value.as_str(), "allowForSession" | "approved_for_session")
            }
            serde_json::Value::Object(map) => {
                map.contains_key("acceptWithExecpolicyAmendment")
                    || map.contains_key("applyNetworkPolicyAmendment")
                    || map.contains_key("approved_execpolicy_amendment")
                    || map.contains_key("network_policy_amendment")
            }
            _ => false,
        }
    };
    match request {
        ServerRequest::CommandExecutionRequestApproval { .. }
        | ServerRequest::FileChangeRequestApproval { .. }
        | ServerRequest::ShellGuardRequestApproval { .. }
        | ServerRequest::ExecCommandApproval { .. }
        | ServerRequest::ApplyPatchApproval { .. } => {
            result.get("decision").is_some_and(decision_is_persistent)
        }
        ServerRequest::PermissionsRequestApproval { .. } => result
            .get("scope")
            .and_then(serde_json::Value::as_str)
            .is_some_and(|scope| scope.eq_ignore_ascii_case("session")),
        _ => false,
    }
}

#[cfg(test)]
#[path = "thread_sample_cancel_context_tests.rs"]
mod thread_sample_cancel_context_tests;

#[cfg(test)]
#[path = "user_verification_ownership_tests.rs"]
mod user_verification_ownership_tests;

#[cfg(test)]
#[path = "user_verification_cancel_context_tests.rs"]
mod user_verification_cancel_context_tests;

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use codex_app_server_protocol::AccountLoginCompletedNotification;
    use codex_app_server_protocol::AccountRateLimitsUpdatedNotification;
    use codex_app_server_protocol::AccountUpdatedNotification;
    use codex_app_server_protocol::ApplyPatchApprovalParams;
    use codex_app_server_protocol::AuthMode;
    use codex_app_server_protocol::CommandExecutionApprovalDecision;
    use codex_app_server_protocol::CommandExecutionRequestApprovalParams;
    use codex_app_server_protocol::ConfigWarningNotification;
    use codex_app_server_protocol::DynamicToolCallParams;
    use codex_app_server_protocol::FileChangeRequestApprovalParams;
    use codex_app_server_protocol::GuardianWarningNotification;
    use codex_app_server_protocol::ModelRerouteReason;
    use codex_app_server_protocol::ModelReroutedNotification;
    use codex_app_server_protocol::ModelVerification;
    use codex_app_server_protocol::ModelVerificationNotification;
    use codex_app_server_protocol::RateLimitSnapshot;
    use codex_app_server_protocol::RateLimitWindow;
    use codex_app_server_protocol::ServerResponse;
    use codex_app_server_protocol::ShellGuardRequestApprovalParams;
    use codex_app_server_protocol::ToolRequestUserInputParams;
    use codex_app_server_protocol::TurnModerationMetadataNotification;
    use codex_protocol::ThreadId;
    use pretty_assertions::assert_eq;
    use serde_json::json;
    use std::sync::Arc;
    use tokio::time::timeout;
    use uuid::Uuid;

    use super::*;

    #[test]
    fn verify_server_notification_serialization() {
        let notification =
            ServerNotification::AccountLoginCompleted(AccountLoginCompletedNotification {
                login_id: Some(Uuid::nil().to_string()),
                success: true,
                error: None,
                onboarding_entrypoint: None,
            });

        let jsonrpc_notification =
            OutgoingMessage::AppServerNotification(ServerNotificationEnvelope {
                notification,
                emitted_at_ms: Some(1_234),
            });
        assert_eq!(
            json!({
                "method": "account/login/completed",
                "params": {
                    "loginId": Uuid::nil().to_string(),
                    "success": true,
                    "error": null,
                    "onboardingEntrypoint": null,
                },
                "emittedAtMs": 1_234,
            }),
            serde_json::to_value(jsonrpc_notification)
                .expect("ensure the strum macros serialize the method field correctly"),
            "ensure the strum macros serialize the method field correctly"
        );
    }

    #[test]
    fn verify_account_login_completed_notification_serialization() {
        let notification =
            ServerNotification::AccountLoginCompleted(AccountLoginCompletedNotification {
                login_id: Some(Uuid::nil().to_string()),
                success: true,
                error: None,
                onboarding_entrypoint: None,
            });

        assert_eq!(
            json!({
                "method": "account/login/completed",
                "params": {
                    "loginId": Uuid::nil().to_string(),
                    "success": true,
                    "error": null,
                    "onboardingEntrypoint": null,
                },
            }),
            serde_json::to_value(notification)
                .expect("ensure the notification serializes correctly"),
            "ensure the notification serializes correctly"
        );
    }

    #[test]
    fn verify_account_rate_limits_notification_serialization() {
        let notification =
            ServerNotification::AccountRateLimitsUpdated(AccountRateLimitsUpdatedNotification {
                rate_limits: RateLimitSnapshot {
                    limit_id: Some("codex".to_string()),
                    limit_name: None,
                    normal_model_slug: None,
                    primary: Some(RateLimitWindow {
                        used_percent: 25,
                        window_duration_mins: Some(15),
                        resets_at: Some(123),
                    }),
                    secondary: None,
                    credits: None,
                    individual_limit: None,
                    spend_control_reached: None,
                    plan_type: Some(PlanType::SelfServeBusinessProLite),
                    rate_limit_reached_type: None,
                },
            });

        assert_eq!(
            json!({
                "method": "account/rateLimits/updated",
                "params": {
                        "rateLimits": {
                        "limitId": "codex",
                        "limitName": null,
                        "normalModelSlug": null,
                        "primary": {
                            "usedPercent": 25,
                            "windowDurationMins": 15,
                            "resetsAt": 123
                        },
                        "secondary": null,
                        "credits": null,
                        "individualLimit": null,
                        "spendControlReached": null,
                        "planType": "self_serve_business_prolite",
                        "rateLimitReachedType": null
                    }
                },
            }),
            serde_json::to_value(notification)
                .expect("ensure the notification serializes correctly"),
            "ensure the notification serializes correctly"
        );
    }

    #[test]
    fn verify_account_updated_notification_serialization() {
        let notification = ServerNotification::AccountUpdated(AccountUpdatedNotification {
            reason: None,
            auth_mode: Some(AuthMode::Chatgpt),
            plan_type: Some(PlanType::SelfServeBusinessProLite),
        });

        assert_eq!(
            json!({
                "method": "account/updated",
                "params": {
                    "authMode": "chatgpt",
                    "planType": "self_serve_business_prolite",
                    "reason": null
                },
            }),
            serde_json::to_value(notification)
                .expect("ensure the notification serializes correctly"),
            "ensure the notification serializes correctly"
        );
    }

    #[test]
    fn verify_config_warning_notification_serialization() {
        let notification = ServerNotification::ConfigWarning(ConfigWarningNotification {
            summary: "Config error: using defaults".to_string(),
            details: Some("error loading config: bad config".to_string()),
            path: None,
            range: None,
        });

        assert_eq!(
            json!( {
                "method": "configWarning",
                "params": {
                    "summary": "Config error: using defaults",
                    "details": "error loading config: bad config",
                },
            }),
            serde_json::to_value(notification)
                .expect("ensure the notification serializes correctly"),
            "ensure the notification serializes correctly"
        );
    }

    #[test]
    fn verify_guardian_warning_notification_serialization() {
        let notification = ServerNotification::GuardianWarning(GuardianWarningNotification {
            thread_id: "thread-1".to_string(),
            message: "Automatic approval review denied the requested action.".to_string(),
        });

        assert_eq!(
            json!({
                "method": "guardianWarning",
                "params": {
                    "threadId": "thread-1",
                    "message": "Automatic approval review denied the requested action.",
                },
            }),
            serde_json::to_value(notification)
                .expect("ensure the notification serializes correctly"),
            "ensure the notification serializes correctly"
        );
    }

    #[test]
    fn verify_model_rerouted_notification_serialization() {
        let notification = ServerNotification::ModelRerouted(ModelReroutedNotification {
            thread_id: "thread-1".to_string(),
            turn_id: "turn-1".to_string(),
            from_model: "gpt-5.3-codex".to_string(),
            to_model: "gpt-5.2".to_string(),
            reason: ModelRerouteReason::HighRiskCyberActivity,
        });

        assert_eq!(
            json!({
                "method": "model/rerouted",
                "params": {
                    "threadId": "thread-1",
                    "turnId": "turn-1",
                    "fromModel": "gpt-5.3-codex",
                    "toModel": "gpt-5.2",
                    "reason": "highRiskCyberActivity",
                },
            }),
            serde_json::to_value(notification)
                .expect("ensure the notification serializes correctly"),
            "ensure the notification serializes correctly"
        );
    }

    #[test]
    fn verify_model_verification_notification_serialization() {
        let notification = ServerNotification::ModelVerification(ModelVerificationNotification {
            thread_id: "thread-1".to_string(),
            turn_id: "turn-1".to_string(),
            verifications: vec![ModelVerification::TrustedAccessForCyber],
        });

        assert_eq!(
            json!({
                "method": "model/verification",
                "params": {
                    "threadId": "thread-1",
                    "turnId": "turn-1",
                    "verifications": ["trustedAccessForCyber"],
                },
            }),
            serde_json::to_value(notification)
                .expect("ensure the notification serializes correctly"),
            "ensure the notification serializes correctly"
        );
    }

    #[test]
    fn verify_turn_moderation_metadata_notification_serialization() {
        let notification =
            ServerNotification::TurnModerationMetadata(TurnModerationMetadataNotification {
                thread_id: "thread-1".to_string(),
                turn_id: "turn-1".to_string(),
                metadata: json!({"presentation": "inline"}),
            });

        assert_eq!(
            json!({
                "method": "turn/moderationMetadata",
                "params": {
                    "threadId": "thread-1",
                    "turnId": "turn-1",
                    "metadata": {"presentation": "inline"},
                },
            }),
            serde_json::to_value(notification)
                .expect("ensure the notification serializes correctly"),
            "ensure the notification serializes correctly"
        );
    }

    #[test]
    fn server_request_response_from_result_decodes_typed_response() {
        let request = ServerRequest::CommandExecutionRequestApproval {
            request_id: RequestId::Integer(7),
            params: CommandExecutionRequestApprovalParams {
                kind: Default::default(),
                thread_id: "thread-1".to_string(),
                turn_id: "turn-1".to_string(),
                item_id: "item-1".to_string(),
                started_at_ms: 0,
                approval_id: None,
                environment_id: None,
                reason: None,
                network_approval_context: None,
                command: Some("echo hi".to_string()),
                cwd: None,
                command_actions: None,
                additional_permissions: None,
                proposed_execpolicy_amendment: None,
                proposed_network_policy_amendments: None,
                available_decisions: None,
            },
        };

        let response = request
            .response_from_result(json!({
                "decision": "acceptForSession",
            }))
            .expect("decode typed server response");

        let ServerResponse::CommandExecutionRequestApproval {
            request_id,
            response,
        } = response
        else {
            panic!("expected command execution approval response");
        };
        assert_eq!(request_id, RequestId::Integer(7));
        assert_eq!(
            response.decision,
            CommandExecutionApprovalDecision::AcceptForSession
        );
    }
    #[tokio::test]
    async fn send_response_routes_to_target_connection() {
        let (tx, mut rx) = mpsc::channel::<OutgoingEnvelope>(4);
        let outgoing =
            OutgoingMessageSender::new(tx, codex_analytics::AnalyticsEventsClient::disabled());
        let request_id = ConnectionRequestId {
            connection_id: ConnectionId(42),
            request_id: RequestId::Integer(7),
        };

        outgoing
            .send_response(
                request_id.clone(),
                ClientResponsePayload::ThreadArchive(
                    codex_app_server_protocol::ThreadArchiveResponse {},
                ),
            )
            .await;

        let envelope = timeout(Duration::from_secs(1), rx.recv())
            .await
            .expect("should receive envelope before timeout")
            .expect("channel should contain one message");

        match envelope {
            OutgoingEnvelope::ToConnection {
                connection_id,
                message,
                ..
            } => {
                assert_eq!(connection_id, ConnectionId(42));
                let OutgoingMessage::Response(response) = message else {
                    panic!("expected response message");
                };
                assert_eq!(response.id, request_id.request_id);
                assert_eq!(
                    serde_json::to_value(response.result).expect("result should serialize"),
                    json!({})
                );
            }
            other => panic!("expected targeted response envelope, got: {other:?}"),
        }
    }

    /// Search cancellation is scoped to the owning connection, including on disconnect.
    #[tokio::test]
    async fn thread_search_cancellation_is_connection_scoped() {
        let (tx, _rx) = mpsc::channel::<OutgoingEnvelope>(4);
        let outgoing =
            OutgoingMessageSender::new(tx, codex_analytics::AnalyticsEventsClient::disabled());
        let request = |connection_id| ConnectionRequestId {
            connection_id: ConnectionId(connection_id),
            request_id: RequestId::String("bf76b226-4c49-4394-a28c-3ff976abb756".to_string()),
        };
        let first = RequestContext::new(
            request(1),
            "thread/search",
            Span::none(),
            /*parent_trace*/ None,
        );
        let second = RequestContext::new(
            request(2),
            "thread/search/stream",
            Span::none(),
            /*parent_trace*/ None,
        );
        outgoing.register_request_context(first.clone()).await;
        outgoing.register_request_context(second.clone()).await;

        assert!(
            outgoing
                .search_cancellation_target(&request(2), /*streaming*/ false)
                .await
                .is_none()
        );
        assert!(
            outgoing
                .search_cancellation_target(&request(1), /*streaming*/ true)
                .await
                .is_none()
        );
        assert!(!first.cancellation.is_cancelled());
        assert!(!second.cancellation.is_cancelled());

        outgoing
            .search_cancellation_target(&request(2), /*streaming*/ true)
            .await
            .expect("stream target")
            .cancel();
        assert!(second.cancellation.is_cancelled());
        assert!(!first.cancellation.is_cancelled());

        let captured = outgoing
            .search_cancellation_target(&request(1), /*streaming*/ false)
            .await
            .expect("original target");
        let replacement = RequestContext::new(
            request(1),
            "thread/search",
            Span::none(),
            /*parent_trace*/ None,
        );
        outgoing.register_request_context(replacement.clone()).await;
        captured.cancel();
        assert!(first.cancellation.is_cancelled());
        assert!(!replacement.cancellation.is_cancelled());
        outgoing.connection_closed(ConnectionId(1)).await;
        assert!(replacement.cancellation.is_cancelled());
    }

    #[tokio::test]
    async fn send_response_clears_registered_request_context() {
        let (tx, _rx) = mpsc::channel::<OutgoingEnvelope>(4);
        let outgoing =
            OutgoingMessageSender::new(tx, codex_analytics::AnalyticsEventsClient::disabled());
        let request_id = ConnectionRequestId {
            connection_id: ConnectionId(42),
            request_id: RequestId::Integer(7),
        };

        outgoing
            .register_request_context(RequestContext::new(
                request_id.clone(),
                "thread/start",
                tracing::info_span!("app_server.request", rpc.method = "thread/start"),
                /*parent_trace*/ None,
            ))
            .await;
        assert_eq!(outgoing.request_context_count().await, 1);

        outgoing
            .send_response(
                request_id,
                ClientResponsePayload::ThreadArchive(
                    codex_app_server_protocol::ThreadArchiveResponse {},
                ),
            )
            .await;

        assert_eq!(outgoing.request_context_count().await, 0);
    }

    #[tokio::test]
    async fn send_error_routes_to_target_connection() {
        let (tx, mut rx) = mpsc::channel::<OutgoingEnvelope>(4);
        let outgoing =
            OutgoingMessageSender::new(tx, codex_analytics::AnalyticsEventsClient::disabled());
        let request_id = ConnectionRequestId {
            connection_id: ConnectionId(9),
            request_id: RequestId::Integer(3),
        };
        let error = internal_error("boom");

        outgoing.send_error(request_id.clone(), error.clone()).await;

        let envelope = timeout(Duration::from_secs(1), rx.recv())
            .await
            .expect("should receive envelope before timeout")
            .expect("channel should contain one message");

        match envelope {
            OutgoingEnvelope::ToConnection {
                connection_id,
                message,
                ..
            } => {
                assert_eq!(connection_id, ConnectionId(9));
                let OutgoingMessage::Error(outgoing_error) = message else {
                    panic!("expected error message");
                };
                assert_eq!(outgoing_error.id, RequestId::Integer(3));
                assert_eq!(outgoing_error.error, error);
            }
            other => panic!("expected targeted error envelope, got: {other:?}"),
        }
    }

    /// Empty target lists broadcast; explicit targets preserve order, payload, and one timestamp.
    #[tokio::test]
    async fn send_server_notification_to_connections_reuses_timestamp() {
        for connection_ids in [
            vec![],
            vec![ConnectionId(1)],
            vec![ConnectionId(1), ConnectionId(2)],
        ] {
            let (tx, mut rx) = mpsc::channel::<OutgoingEnvelope>(2);
            let outgoing =
                OutgoingMessageSender::new(tx, codex_analytics::AnalyticsEventsClient::disabled());
            let notification = ServerNotification::ConfigWarning(ConfigWarningNotification {
                summary: "test".to_string(),
                details: None,
                path: None,
                range: None,
            });
            let expected = serde_json::to_value(&notification).expect("serialize notification");

            outgoing
                .send_server_notification_to_connections(&connection_ids, notification)
                .await;

            let mut timestamps = Vec::new();
            for index in 0..connection_ids.len().max(1) {
                let message = match rx.try_recv().expect("notification should be queued") {
                    OutgoingEnvelope::Broadcast { message } => {
                        assert!(connection_ids.is_empty());
                        message
                    }
                    OutgoingEnvelope::ToConnection {
                        connection_id,
                        message,
                        write_complete_tx,
                    } => {
                        assert_eq!(connection_id, connection_ids[index]);
                        assert!(write_complete_tx.is_none());
                        message
                    }
                };
                let OutgoingMessage::AppServerNotification(envelope) = message else {
                    panic!("expected server notification");
                };
                assert_eq!(
                    serde_json::to_value(&envelope.notification).unwrap(),
                    expected
                );
                timestamps.push(
                    envelope
                        .emitted_at_ms
                        .expect("notification has a timestamp"),
                );
            }
            assert!(
                timestamps
                    .iter()
                    .all(|timestamp| *timestamp == timestamps[0])
            );
            assert!(rx.try_recv().is_err());
        }
    }

    #[tokio::test]
    async fn send_server_notification_to_connection_and_wait_tracks_write_completion() {
        let (tx, mut rx) = mpsc::channel::<OutgoingEnvelope>(4);
        let outgoing =
            OutgoingMessageSender::new(tx, codex_analytics::AnalyticsEventsClient::disabled());
        let send_task = tokio::spawn(async move {
            outgoing
                .send_server_notification_to_connection_and_wait(
                    ConnectionId(42),
                    ServerNotification::ModelRerouted(ModelReroutedNotification {
                        thread_id: "thread-1".to_string(),
                        turn_id: "turn-1".to_string(),
                        from_model: "gpt-5.3-codex".to_string(),
                        to_model: "gpt-5.2".to_string(),
                        reason: ModelRerouteReason::HighRiskCyberActivity,
                    }),
                )
                .await
        });

        let envelope = timeout(Duration::from_secs(1), rx.recv())
            .await
            .expect("should receive envelope before timeout")
            .expect("channel should contain one message");
        let OutgoingEnvelope::ToConnection {
            connection_id,
            message,
            write_complete_tx,
        } = envelope
        else {
            panic!("expected targeted server notification envelope");
        };
        assert_eq!(connection_id, ConnectionId(42));
        let OutgoingMessage::AppServerNotification(envelope) = message else {
            panic!("expected app-server notification");
        };
        assert!(
            envelope
                .emitted_at_ms
                .is_some_and(|emitted_at_ms| emitted_at_ms > 0)
        );
        write_complete_tx
            .expect("write completion sender should be attached")
            .send(())
            .expect("receiver should still be waiting");

        timeout(Duration::from_secs(1), send_task)
            .await
            .expect("send task should finish after write completion is signaled")
            .expect("send task should not panic");
    }

    #[tokio::test]
    async fn connection_closed_clears_registered_request_contexts() {
        let (tx, _rx) = mpsc::channel::<OutgoingEnvelope>(4);
        let outgoing =
            OutgoingMessageSender::new(tx, codex_analytics::AnalyticsEventsClient::disabled());
        let closed_connection_request = ConnectionRequestId {
            connection_id: ConnectionId(9),
            request_id: RequestId::Integer(3),
        };
        let open_connection_request = ConnectionRequestId {
            connection_id: ConnectionId(10),
            request_id: RequestId::Integer(4),
        };

        outgoing
            .register_request_context(RequestContext::new(
                closed_connection_request,
                "turn/interrupt",
                tracing::info_span!("app_server.request", rpc.method = "turn/interrupt"),
                /*parent_trace*/ None,
            ))
            .await;
        outgoing
            .register_request_context(RequestContext::new(
                open_connection_request,
                "turn/start",
                tracing::info_span!("app_server.request", rpc.method = "turn/start"),
                /*parent_trace*/ None,
            ))
            .await;
        assert_eq!(outgoing.request_context_count().await, 2);

        outgoing.connection_closed(ConnectionId(9)).await;

        assert_eq!(outgoing.request_context_count().await, 1);
    }

    #[tokio::test]
    async fn notify_client_error_forwards_error_to_waiter() {
        let (tx, _rx) = mpsc::channel::<OutgoingEnvelope>(4);
        let outgoing =
            OutgoingMessageSender::new(tx, codex_analytics::AnalyticsEventsClient::disabled());

        let (request_id, wait_for_result) = outgoing
            .send_request(ServerRequestPayload::ApplyPatchApproval(
                ApplyPatchApprovalParams {
                    conversation_id: ThreadId::new(),
                    call_id: "call-id".to_string(),
                    file_changes: HashMap::new(),
                    reason: None,
                    grant_root: None,
                },
            ))
            .await;

        let error = internal_error("refresh failed");

        outgoing
            .notify_client_error(ConnectionId(1), request_id, error.clone())
            .await;

        let result = timeout(Duration::from_secs(1), wait_for_result)
            .await
            .expect("wait should not time out")
            .expect("waiter should receive a callback");
        assert_eq!(result, Err(error));
    }

    #[tokio::test]
    async fn pending_requests_for_thread_returns_thread_requests_in_request_id_order() {
        let (tx, _rx) = mpsc::channel::<OutgoingEnvelope>(8);
        let outgoing = Arc::new(OutgoingMessageSender::new(
            tx,
            codex_analytics::AnalyticsEventsClient::disabled(),
        ));
        let thread_id = ThreadId::new();
        let thread_outgoing = ThreadScopedOutgoingMessageSender::new(
            outgoing.clone(),
            vec![ConnectionId(1)],
            thread_id,
        );

        let (dynamic_tool_request_id, _dynamic_tool_waiter) = thread_outgoing
            .send_request(ServerRequestPayload::DynamicToolCall(
                DynamicToolCallParams {
                    thread_id: thread_id.to_string(),
                    turn_id: "turn-1".to_string(),
                    call_id: "call-0".to_string(),
                    namespace: None,
                    tool: "tool".to_string(),
                    arguments: json!({}),
                },
            ))
            .await;
        let (first_request_id, _first_waiter) = thread_outgoing
            .send_request(ServerRequestPayload::ToolRequestUserInput(
                ToolRequestUserInputParams {
                    thread_id: thread_id.to_string(),
                    turn_id: "turn-1".to_string(),
                    item_id: "call-1".to_string(),
                    questions: vec![],
                    is_blocking: true,
                    auto_resolution_ms: None,
                },
            ))
            .await;
        let (second_request_id, _second_waiter) = thread_outgoing
            .send_request(ServerRequestPayload::FileChangeRequestApproval(
                FileChangeRequestApprovalParams {
                    thread_id: thread_id.to_string(),
                    turn_id: "turn-1".to_string(),
                    item_id: "call-2".to_string(),
                    started_at_ms: 0,
                    reason: None,
                    grant_root: None,
                },
            ))
            .await;
        let pending_requests = outgoing.pending_requests_for_thread(thread_id).await;
        assert_eq!(
            pending_requests
                .iter()
                .map(ServerRequest::id)
                .collect::<Vec<_>>(),
            vec![
                &dynamic_tool_request_id,
                &first_request_id,
                &second_request_id
            ]
        );
    }

    #[tokio::test]
    async fn cancel_requests_for_thread_cancels_all_thread_requests() {
        let (tx, _rx) = mpsc::channel::<OutgoingEnvelope>(8);
        let outgoing = Arc::new(OutgoingMessageSender::new(
            tx,
            codex_analytics::AnalyticsEventsClient::disabled(),
        ));
        let thread_id = ThreadId::new();
        let thread_outgoing = ThreadScopedOutgoingMessageSender::new(
            outgoing.clone(),
            vec![ConnectionId(1)],
            thread_id,
        );

        let (_dynamic_tool_request_id, dynamic_tool_waiter) = thread_outgoing
            .send_request(ServerRequestPayload::DynamicToolCall(
                DynamicToolCallParams {
                    thread_id: thread_id.to_string(),
                    turn_id: "turn-1".to_string(),
                    call_id: "call-0".to_string(),
                    namespace: None,
                    tool: "tool".to_string(),
                    arguments: json!({}),
                },
            ))
            .await;
        let (_request_id, user_input_waiter) = thread_outgoing
            .send_request(ServerRequestPayload::ToolRequestUserInput(
                ToolRequestUserInputParams {
                    thread_id: thread_id.to_string(),
                    turn_id: "turn-1".to_string(),
                    item_id: "call-1".to_string(),
                    questions: vec![],
                    is_blocking: true,
                    auto_resolution_ms: None,
                },
            ))
            .await;
        let error = internal_error("tracked request cancelled");

        outgoing
            .cancel_requests_for_thread(thread_id, Some(error.clone()))
            .await;

        let dynamic_tool_result = timeout(Duration::from_secs(1), dynamic_tool_waiter)
            .await
            .expect("dynamic tool waiter should resolve")
            .expect("dynamic tool waiter should receive a callback");
        let user_input_result = timeout(Duration::from_secs(1), user_input_waiter)
            .await
            .expect("user input waiter should resolve")
            .expect("user input waiter should receive a callback");
        assert_eq!(dynamic_tool_result, Err(error.clone()));
        assert_eq!(user_input_result, Err(error));
        assert!(
            outgoing
                .pending_requests_for_thread(thread_id)
                .await
                .is_empty()
        );
    }

    #[tokio::test]
    async fn remote_human_connection_cannot_receive_or_answer_secret_prompts() {
        let (tx, mut rx) = mpsc::channel::<OutgoingEnvelope>(16);
        let outgoing =
            OutgoingMessageSender::new(tx, codex_analytics::AnalyticsEventsClient::disabled());
        let local_conn = ConnectionId(1);
        let remote_conn = ConnectionId(2);
        outgoing.register_remote_human_connection(remote_conn).await;
        let thread_id = ThreadId::new();

        // 1. ShellGuardRequestSecret (sudo password) is only delivered to local_conn.
        let (secret_id, mut secret_waiter) = outgoing
            .send_request_to_connections(
                Some(&[local_conn, remote_conn]),
                ServerRequestPayload::ShellGuardRequestSecret(
                    codex_app_server_protocol::ShellGuardRequestSecretParams {
                        thread_id: thread_id.to_string(),
                        turn_id: "turn-1".to_string(),
                        item_id: Some("item-1".to_string()),
                        started_at_ms: 0,
                        kind: codex_app_server_protocol::ShellGuardSecretKind::SudoPassword,
                        cwd: "/tmp".to_string(),
                        executable: "sudo".to_string(),
                        argv: vec!["sudo".to_string(), "ls".to_string()],
                    },
                ),
                Some(thread_id),
            )
            .await;

        let first = rx
            .try_recv()
            .expect("local connection should receive secret request");
        assert!(matches!(
            first,
            OutgoingEnvelope::ToConnection { connection_id, .. } if connection_id == local_conn
        ));
        assert!(
            rx.try_recv().is_err(),
            "remote-human connection must not receive ShellGuardRequestSecret"
        );

        // Replaying pending thread requests to remote_conn must also skip the secret request.
        outgoing
            .replay_requests_to_connection_for_thread(remote_conn, thread_id)
            .await;
        assert!(
            rx.try_recv().is_err(),
            "remote-human connection must not receive replayed secret request"
        );

        // Remote-human response attempt must be rejected without resolving the callback.
        outgoing
            .notify_client_response(
                remote_conn,
                secret_id.clone(),
                serde_json::json!({ "decision": "provide", "secret": "hunter2" }),
            )
            .await;
        assert!(secret_waiter.try_recv().is_err());

        // Local native connection can still answer the pending secret prompt.
        outgoing
            .notify_client_response(
                local_conn,
                secret_id,
                serde_json::json!({ "decision": "decline" }),
            )
            .await;
        assert!(
            secret_waiter
                .await
                .expect("local response should resolve")
                .is_ok()
        );

        // 2. Secret ToolRequestUserInput (is_secret: true) is also blocked from remote_conn.
        let (secret_q_id, mut secret_q_waiter) = outgoing
            .send_request_to_connections(
                Some(&[remote_conn]),
                ServerRequestPayload::ToolRequestUserInput(ToolRequestUserInputParams {
                    thread_id: thread_id.to_string(),
                    turn_id: "turn-2".to_string(),
                    item_id: "call-2".to_string(),
                    questions: vec![codex_app_server_protocol::ToolRequestUserInputQuestion {
                        id: "gpg".to_string(),
                        header: Some("GPG".to_string()),
                        question: "Passphrase?".to_string(),
                        is_other: false,
                        is_secret: true,
                        options: None,
                    }],
                    is_blocking: true,
                    auto_resolution_ms: None,
                }),
                Some(thread_id),
            )
            .await;
        assert!(rx.try_recv().is_err());
        outgoing
            .notify_client_response(remote_conn, secret_q_id.clone(), serde_json::json!({}))
            .await;
        assert!(secret_q_waiter.try_recv().is_err());
        outgoing.cancel_request(&secret_q_id).await;

        // 3. Non-secret ToolRequestUserInput IS delivered to and answerable by remote_conn.
        let (plain_q_id, plain_q_waiter) = outgoing
            .send_request_to_connections(
                Some(&[remote_conn]),
                ServerRequestPayload::ToolRequestUserInput(ToolRequestUserInputParams {
                    thread_id: thread_id.to_string(),
                    turn_id: "turn-3".to_string(),
                    item_id: "call-3".to_string(),
                    questions: vec![codex_app_server_protocol::ToolRequestUserInputQuestion {
                        id: "choice".to_string(),
                        header: Some("Mode".to_string()),
                        question: "Proceed?".to_string(),
                        is_other: false,
                        is_secret: false,
                        options: None,
                    }],
                    is_blocking: true,
                    auto_resolution_ms: None,
                }),
                Some(thread_id),
            )
            .await;
        let delivered = rx
            .try_recv()
            .expect("remote-human should receive non-secret question");
        assert!(matches!(
            delivered,
            OutgoingEnvelope::ToConnection { connection_id, .. } if connection_id == remote_conn
        ));
        outgoing
            .notify_client_response(
                remote_conn,
                plain_q_id,
                serde_json::json!({ "answers": {} }),
            )
            .await;
        assert!(
            plain_q_waiter
                .await
                .expect("remote non-secret answer resolves")
                .is_ok()
        );
    }

    #[tokio::test]
    async fn remote_human_only_remembers_offered_command_decisions() {
        let (tx, _rx) = mpsc::channel::<OutgoingEnvelope>(16);
        let outgoing =
            OutgoingMessageSender::new(tx, codex_analytics::AnalyticsEventsClient::disabled());
        let remote_conn = ConnectionId(42);
        let other_remote = ConnectionId(43);
        outgoing.register_remote_human_connection(remote_conn).await;
        outgoing
            .register_remote_human_connection(other_remote)
            .await;
        let thread_id = ThreadId::new();

        let mut params = CommandExecutionRequestApprovalParams {
            thread_id: thread_id.to_string(),
            turn_id: "turn-1".to_string(),
            item_id: "item-1".to_string(),
            approval_id: None,
            reason: None,
            network_approval_context: None,
            command: Some("ls".to_string()),
            cwd: None,
            command_actions: None,
            additional_permissions: None,
            proposed_execpolicy_amendment: None,
            proposed_network_policy_amendments: None,
            available_decisions: None,
            started_at_ms: 0,
            environment_id: None,
            kind: CommandExecutionApprovalKind::Command,
        };
        let (cmd_id, mut cmd_waiter) = outgoing
            .send_request_to_connections(
                Some(&[remote_conn]),
                ServerRequestPayload::CommandExecutionRequestApproval(params.clone()),
                Some(thread_id),
            )
            .await;

        for disallowed in [
            serde_json::json!({ "decision": "acceptForSession" }),
            serde_json::json!({ "decision": "allowForSession" }),
            serde_json::json!({
                "decision": {
                    "acceptWithExecpolicyAmendment": {
                        "execpolicy_amendment": ["ls"]
                    }
                }
            }),
            serde_json::json!({
                "decision": {
                    "applyNetworkPolicyAmendment": {
                        "network_policy_amendment": { "host": "example.com", "action": "allow" }
                    }
                }
            }),
        ] {
            outgoing
                .notify_client_response(remote_conn, cmd_id.clone(), disallowed)
                .await;
            assert!(
                cmd_waiter.try_recv().is_err(),
                "session-wide or policy-amendment approval must not resolve"
            );
        }

        // One-time approval still succeeds.
        outgoing
            .notify_client_response(
                remote_conn,
                cmd_id,
                serde_json::json!({ "decision": "accept" }),
            )
            .await;
        assert!(
            cmd_waiter
                .await
                .expect("one-time accept should resolve")
                .is_ok()
        );

        params.available_decisions = Some(vec![CommandExecutionApprovalDecision::AcceptForSession]);
        let (network_id, mut network_waiter) = outgoing
            .send_request_to_connections(
                Some(&[remote_conn]),
                ServerRequestPayload::CommandExecutionRequestApproval(
                    CommandExecutionRequestApprovalParams {
                        command: None,
                        network_approval_context: Some(
                            codex_app_server_protocol::NetworkApprovalContext {
                                host: "example.com".to_string(),
                                protocol: codex_app_server_protocol::NetworkApprovalProtocol::Https,
                            },
                        ),
                        ..params.clone()
                    },
                ),
                Some(thread_id),
            )
            .await;
        let remember = json!({ "decision": "acceptForSession" });
        outgoing
            .notify_client_response(remote_conn, network_id.clone(), remember.clone())
            .await;
        assert!(network_waiter.try_recv().is_err());
        let allow_once = json!({ "decision": "accept" });
        outgoing
            .notify_client_response(remote_conn, network_id, allow_once.clone())
            .await;
        assert_eq!(network_waiter.await.unwrap().unwrap(), allow_once);

        let (cmd_id, mut cmd_waiter) = outgoing
            .send_request_to_connections(
                Some(&[remote_conn]),
                ServerRequestPayload::CommandExecutionRequestApproval(params),
                Some(thread_id),
            )
            .await;
        outgoing
            .notify_client_response(other_remote, cmd_id.clone(), remember.clone())
            .await;
        assert!(cmd_waiter.try_recv().is_err());
        outgoing
            .notify_client_response(remote_conn, cmd_id, remember.clone())
            .await;
        assert_eq!(cmd_waiter.await.unwrap().unwrap(), remember);

        // Session-scoped permission grant is also rejected on remote-human.
        let (perm_id, mut perm_waiter) = outgoing
            .send_request_to_connections(
                Some(&[remote_conn]),
                ServerRequestPayload::PermissionsRequestApproval(
                    codex_app_server_protocol::PermissionsRequestApprovalParams {
                        thread_id: thread_id.to_string(),
                        turn_id: "turn-2".to_string(),
                        item_id: "item-2".to_string(),
                        reason: None,
                        cwd: codex_utils_absolute_path::AbsolutePathBuf::from_absolute_path("/tmp")
                            .expect("absolute path")
                            .into(),
                        environment_id: None,
                        started_at_ms: 0,
                        permissions: codex_app_server_protocol::RequestPermissionProfile {
                            network: None,
                            file_system: None,
                        },
                    },
                ),
                Some(thread_id),
            )
            .await;
        outgoing
            .notify_client_response(
                remote_conn,
                perm_id.clone(),
                serde_json::json!({ "permissions": {}, "scope": "session" }),
            )
            .await;
        assert!(perm_waiter.try_recv().is_err());
        outgoing
            .notify_client_response(
                remote_conn,
                perm_id,
                serde_json::json!({ "permissions": {}, "scope": "turn" }),
            )
            .await;
        assert!(
            perm_waiter
                .await
                .expect("turn-scoped grant should resolve")
                .is_ok()
        );
    }

    #[tokio::test]
    async fn remembered_shell_approval_requires_the_same_action_thread_and_attachment() {
        let (tx, mut rx) = mpsc::channel::<OutgoingEnvelope>(16);
        let outgoing =
            OutgoingMessageSender::new(tx, codex_analytics::AnalyticsEventsClient::disabled());
        let remote = ConnectionId(42);
        let other = ConnectionId(43);
        outgoing.register_remote_human_connection(remote).await;
        outgoing.register_remote_human_connection(other).await;
        let thread = ThreadId::new();
        let params = ShellGuardRequestApprovalParams {
            thread_id: thread.to_string(),
            turn_id: "turn-1".into(),
            item_id: None,
            started_at_ms: 0,
            kind: ShellGuardApprovalKind::RemoteWrite,
            cwd: "/workspace".into(),
            executable: "/usr/bin/git".into(),
            argv: vec!["push".into(), "origin".into()],
        };
        let send = |connection, thread, params| {
            let outgoing = &outgoing;
            async move {
                outgoing
                    .send_request_to_connections(
                        Some(&[connection]),
                        ServerRequestPayload::ShellGuardRequestApproval(params),
                        Some(thread),
                    )
                    .await
            }
        };
        let (id, waiter) = send(remote, thread, params.clone()).await;
        assert!(rx.try_recv().is_ok());
        let remember = json!({ "decision": "acceptForSession" });
        outgoing
            .notify_client_response(remote, id, remember.clone())
            .await;
        assert_eq!(waiter.await.unwrap().unwrap(), remember);

        let (_, mut waiter) = send(remote, thread, params.clone()).await;
        assert_eq!(
            waiter.try_recv().unwrap().unwrap(),
            json!({ "decision": "accept" })
        );
        assert!(rx.try_recv().is_err());

        let mut other_command = params.clone();
        other_command.argv.push("branch".into());
        let mut other_directory = params.clone();
        other_directory.cwd = "/somewhere-else".into();
        let mut other_executable = params.clone();
        other_executable.executable = "/somewhere-else/git".into();
        let other_thread = ThreadId::new();
        let mut other_thread_params = params.clone();
        other_thread_params.thread_id = other_thread.to_string();
        for (connection, thread, request) in [
            (remote, thread, other_command),
            (remote, thread, other_directory),
            (remote, thread, other_executable),
            (other, thread, params.clone()),
            (remote, other_thread, other_thread_params),
        ] {
            let (id, mut waiter) = send(connection, thread, request).await;
            assert!(rx.try_recv().is_ok());
            assert!(waiter.try_recv().is_err());
            outgoing
                .notify_client_response(connection, id, json!({ "decision": "decline" }))
                .await;
            assert!(waiter.await.unwrap().is_ok());
        }

        let mut sudo = params.clone();
        sudo.kind = ShellGuardApprovalKind::Sudo;
        let (sudo_id, mut sudo_waiter) = send(remote, thread, sudo).await;
        assert!(rx.try_recv().is_ok());
        outgoing
            .notify_client_response(remote, sudo_id, remember.clone())
            .await;
        assert!(sudo_waiter.try_recv().is_err());

        outgoing
            .forget_shell_guard_approvals(thread, Some(remote))
            .await;
        let (id, waiter) = send(remote, thread, params.clone()).await;
        assert!(rx.try_recv().is_ok());
        outgoing
            .notify_client_response(remote, id, remember.clone())
            .await;
        assert_eq!(waiter.await.unwrap().unwrap(), remember);
        outgoing.connection_closed(remote).await;
        outgoing.register_remote_human_connection(remote).await;
        let (_, mut waiter) = send(remote, thread, params).await;
        assert!(rx.try_recv().is_ok());
        assert!(waiter.try_recv().is_err());
    }
}

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use nomifun_common::{
    AgentKillReason, AgentType, AppError, Confirmation, ConversationStatus, ErrorChain, RemoteAgentProtocol,
    RemoteAgentId, RemoteAgentStatus, TimestampMs,
};
use serde_json::{Value, json};
use tokio::sync::{Mutex, RwLock, broadcast, watch};
use tracing::{error, info, warn};

use crate::manager::openclaw::connection::{AuthConfig, OpenClawConnection};
use crate::manager::openclaw::device_identity::DeviceIdentity;
use crate::manager::openclaw::event_mapper::{
    TextFallbackState, drain_events_for_run, is_openclaw_turn_event, map_openclaw_event, openclaw_event_run_id,
};
use crate::manager::openclaw::protocol::{
    ChatAbortParams, ChatSendParams, EventFrame, SessionsResetParams, SessionsResetResponse,
    SessionsResolveParams, SessionsResolveResponse,
};
use crate::manager::openclaw::teardown::{
    GatewayRunTurn, GatewayTeardownTarget, TeardownAttempt, TeardownCoordinator,
    request_abort_bounded, wait_for_terminal_proof,
};
use crate::runtime_state::{AgentRuntimeState, AgentRuntimeTurn};
use crate::protocol::events::AgentStreamEvent;
use crate::protocol::send_error::AgentSendError;
use crate::types::SendMessageData;

#[cfg(not(test))]
const REMOTE_TEARDOWN_RPC_TIMEOUT: Duration = Duration::from_secs(5);
#[cfg(test)]
const REMOTE_TEARDOWN_RPC_TIMEOUT: Duration = Duration::from_millis(200);
#[cfg(not(test))]
const REMOTE_TEARDOWN_TERMINAL_TIMEOUT: Duration = Duration::from_secs(5);
#[cfg(test)]
const REMOTE_TEARDOWN_TERMINAL_TIMEOUT: Duration = Duration::from_millis(200);

/// Internal mutable state for a remotely hosted agent session.
struct RemoteState {
    session_key: Option<String>,
    confirmations: Vec<Confirmation>,
    has_messages: bool,
    active_run_id: Option<String>,
    turn_generation: u64,
    runtime_turn: Option<AgentRuntimeTurn>,
    pending_run_events: Vec<EventFrame>,
    approval_memory: HashMap<String, bool>,
    connection_status: RemoteAgentStatus,
}

fn gateway_turn_is_current(state: &RemoteState, gateway_turn: &GatewayRunTurn) -> bool {
    state.active_run_id.as_deref() == Some(gateway_turn.run_id.as_str())
        && state.turn_generation == gateway_turn.turn_generation
        && state.runtime_turn == Some(gateway_turn.runtime_turn)
}

fn teardown_target_from_state(state: &RemoteState) -> Result<Option<GatewayTeardownTarget>, AppError> {
    match (state.runtime_turn, state.active_run_id.as_ref()) {
        (None, None) => Ok(None),
        (None, Some(run_id)) => Err(AppError::Internal(format!(
            "Remote OpenClaw lifecycle invariant violated: run {run_id} has no runtime turn"
        ))),
        (Some(runtime_turn), run_id) => {
            let session_key = state.session_key.clone().ok_or_else(|| {
                AppError::Conflict(
                    "Remote OpenClaw has an admitted turn but no session key; chat.abort cannot identify it".into(),
                )
            })?;
            Ok(Some(GatewayTeardownTarget {
                session_key,
                run_id: run_id.cloned(),
                turn_generation: state.turn_generation,
                runtime_turn,
            }))
        }
    }
}

async fn run_remote_teardown(
    connection: Arc<OpenClawConnection>,
    state: Arc<RwLock<RemoteState>>,
    terminal_rx: watch::Receiver<Option<GatewayRunTurn>>,
) -> Result<(), AppError> {
    let target = {
        let state = state.read().await;
        teardown_target_from_state(&state)?
    };
    let Some(target) = target else {
        connection.close().await;
        return Ok(());
    };

    let params = serde_json::to_value(ChatAbortParams {
        session_key: target.session_key.clone(),
        run_id: target.run_id.clone(),
    })
    .map_err(|error| AppError::Internal(format!("Failed to serialize remote chat.abort: {error}")))?;
    request_abort_bounded(
        async {
            connection
                .request::<Value>("chat.abort", params)
                .await
                .map(|_| ())
        },
        REMOTE_TEARDOWN_RPC_TIMEOUT,
        "Remote OpenClaw teardown",
    )
    .await?;
    wait_for_terminal_proof(
        &target,
        terminal_rx,
        REMOTE_TEARDOWN_TERMINAL_TIMEOUT,
        "Remote OpenClaw teardown",
    )
    .await?;

    // Socket close is cleanup only. It is deliberately after the exact
    // terminal proof because a closed client transport says nothing about
    // whether remote tools or knowledge write-back are still executing.
    connection.close().await;
    Ok(())
}

/// Configuration for connecting to a remote agent.
#[derive(Clone)]
pub struct RemoteAgentConfig {
    pub remote_agent_id: RemoteAgentId,
    pub protocol: RemoteAgentProtocol,
    pub url: String,
    pub auth_type: String,
    pub auth_token: Option<String>,
    pub device_token: Option<String>,
    pub allow_insecure: bool,
    pub resume_session_key: Option<String>,
    /// Per-remote-agent OpenClaw device identity persisted by the pairing
    /// service. Required so remote gateways never share the local OpenClaw
    /// process identity.
    pub device_identity: Option<DeviceIdentity>,
}

/// Manages a remote OpenClaw Gateway through the v4 protocol used by
/// the local OpenClaw integration.
///
/// `RemoteAgentProtocol::Acp` is intentionally not treated as "ACP over
/// WebSocket": ACP is a stdio protocol in NomiFun today. Hermes therefore
/// remains supported locally through `hermes acp`; its separate remote
/// JSON-RPC gateway needs its own adapter rather than being mislabeled as ACP.
pub struct RemoteAgentManager {
    runtime: AgentRuntimeState,
    remote_config: RemoteAgentConfig,
    connection: Arc<OpenClawConnection>,
    state: Arc<RwLock<RemoteState>>,
    text_state: Mutex<TextFallbackState>,
    _reader_handle: Mutex<Option<tokio::task::JoinHandle<()>>>,
    terminal_proof_tx: watch::Sender<Option<GatewayRunTurn>>,
    teardown: Arc<TeardownCoordinator>,
}

impl RemoteAgentManager {
    /// Establish the remote protocol connection and return a ready-to-use
    /// manager. Construction is eager so a conversation warmup fails early
    /// instead of accepting the first message and then reporting "not
    /// connected".
    pub async fn connect(
        conversation_id: String,
        workspace: String,
        remote_config: RemoteAgentConfig,
    ) -> Result<(Arc<Self>, Option<String>), AppError> {
        if remote_config.protocol != RemoteAgentProtocol::OpenClaw {
            return Err(AppError::BadRequest(format!(
                "Remote protocol '{}' is not implemented. Remote OpenClaw is supported; Hermes is available locally via `hermes acp`.",
                protocol_name(remote_config.protocol),
            )));
        }
        let identity = remote_config.device_identity.clone().ok_or_else(|| {
            AppError::Internal(
                "Remote OpenClaw configuration has no dedicated device identity; delete and re-create it".into(),
            )
        })?;
        let auth = match remote_config.auth_type.as_str() {
            "none" => remote_config.device_token.clone().map(|device_token| AuthConfig {
                token: None,
                device_token: Some(device_token),
                password: None,
            }),
            "bearer" => Some(AuthConfig {
                token: Some(require_remote_credential(&remote_config, "Bearer token")?),
                device_token: remote_config.device_token.clone(),
                password: None,
            }),
            "password" => Some(AuthConfig {
                token: None,
                device_token: remote_config.device_token.clone(),
                password: Some(require_remote_credential(&remote_config, "Password")?),
            }),
            other => {
                return Err(AppError::BadRequest(format!(
                    "Unsupported remote authentication type '{other}'"
                )));
            }
        };

        let (connection, hello) =
            OpenClawConnection::connect_with_options(&remote_config.url, auth, &identity, remote_config.allow_insecure)
                .await
                .inspect_err(|e| {
                error!(
                    conversation_id,
                    remote_agent_id = %remote_config.remote_agent_id,
                    url = %remote_config.url,
                    error = %ErrorChain(e),
                    "Failed to connect to remote OpenClaw gateway"
                );
            })?;

        let (terminal_proof_tx, _) = watch::channel(None);
        let manager = Arc::new(Self {
            runtime: AgentRuntimeState::new(conversation_id, workspace, 256),
            connection,
            state: Arc::new(RwLock::new(RemoteState {
                session_key: remote_config.resume_session_key.clone(),
                confirmations: Vec::new(),
                has_messages: false,
                active_run_id: None,
                turn_generation: 0,
                runtime_turn: None,
                pending_run_events: Vec::new(),
                approval_memory: HashMap::new(),
                connection_status: RemoteAgentStatus::Connected,
            })),
            remote_config,
            text_state: Mutex::new(TextFallbackState::new()),
            _reader_handle: Mutex::new(None),
            terminal_proof_tx,
            teardown: Arc::new(TeardownCoordinator::default()),
        });
        info!(
            conversation_id = %manager.runtime.conversation_id(),
            remote_agent_id = %manager.remote_config.remote_agent_id,
            url = %manager.remote_config.url,
            "Connected to remote OpenClaw gateway"
        );

        let issued_device_token = hello.auth.device_token;
        Ok((manager, issued_device_token))
    }

    pub(crate) async fn start_event_relay(self: &Arc<Self>) {
        let this = Arc::clone(self);
        let handle = tokio::spawn(async move {
            this.run_event_relay().await;
        });
        *self._reader_handle.lock().await = Some(handle);
    }

    async fn run_event_relay(self: Arc<Self>) {
        let mut event_rx = self.connection.subscribe_events();
        let mut close_rx = self.connection.subscribe_close();
        loop {
            tokio::select! {
                event = event_rx.recv() => match event {
                    Ok(event_frame) => {
                        self.runtime.bump_activity();
                        self.route_event_frame(event_frame).await;
                    }
                    Err(broadcast::error::RecvError::Lagged(n)) => {
                        warn!(
                            conversation_id = %self.runtime.conversation_id(),
                            lagged = n,
                            "Remote OpenClaw event relay lagged"
                        );
                        self.runtime.emit_stream_broken(format!(
                            "Remote OpenClaw event relay lost {n} buffered event(s)"
                        ));
                        break;
                    }
                    Err(broadcast::error::RecvError::Closed) => break,
                },
                _ = close_rx.recv() => break,
            }
        }

        {
            let mut state = self.state.write().await;
            state.connection_status = RemoteAgentStatus::Error;
        }
        if self.runtime.status() == Some(ConversationStatus::Running) {
            self.runtime
                .emit_stream_broken("Remote OpenClaw connection closed");
        } else {
            self.runtime.mark_transport_broken();
        }
    }

    async fn route_event_frame(&self, event_frame: EventFrame) {
        let gateway_turn = if is_openclaw_turn_event(&event_frame) {
            let Some(event_run_id) = openclaw_event_run_id(&event_frame).map(str::to_owned) else {
                warn!(
                    conversation_id = %self.runtime.conversation_id(),
                    event = %event_frame.event,
                    "Dropping turn-scoped remote OpenClaw event without runId"
                );
                return;
            };
            let mut state = self.state.write().await;
            match (state.active_run_id.as_deref(), state.runtime_turn) {
                (Some(active_run_id), Some(runtime_turn)) if active_run_id == event_run_id => {
                    Some(GatewayRunTurn {
                        run_id: event_run_id,
                        turn_generation: state.turn_generation,
                        runtime_turn,
                    })
                }
                (Some(active_run_id), _) => {
                    tracing::debug!(
                        conversation_id = %self.runtime.conversation_id(),
                        %event_run_id,
                        %active_run_id,
                        "Dropping delayed remote OpenClaw event from another run"
                    );
                    return;
                }
                (None, Some(_)) if self.runtime.status() == Some(ConversationStatus::Running) =>
                {
                    const MAX_PENDING_RUN_EVENTS: usize = 256;
                    if state.pending_run_events.len() < MAX_PENDING_RUN_EVENTS {
                        state.pending_run_events.push(event_frame);
                    } else {
                        drop(state);
                        self.runtime.emit_stream_broken(
                            "Remote OpenClaw produced too many events before acknowledging chat.send",
                        );
                    }
                    return;
                }
                (None, _) => return,
            }
        } else {
            None
        };
        self.process_event_frame(event_frame, gateway_turn).await;
    }

    async fn process_event_frame(&self, event_frame: EventFrame, gateway_turn: Option<GatewayRunTurn>) {
        let events = if let Some(gateway_turn) = gateway_turn.as_ref() {
            // Linearize run/token validation with mutation of the shared text
            // mapper. New-turn admission needs the write half of this guard,
            // so a delayed old frame cannot pass validation, wait, and then
            // contaminate the new turn's freshly reset fallback state.
            let state = self.state.read().await;
            if !gateway_turn_is_current(&state, gateway_turn) {
                return;
            }
            let session_key = state.session_key.clone();
            let mut text_state = self.text_state.lock().await;
            map_openclaw_event(&event_frame, &mut text_state, session_key.as_deref())
        } else {
            let session_key = self.state.read().await.session_key.clone();
            let mut text_state = self.text_state.lock().await;
            map_openclaw_event(&event_frame, &mut text_state, session_key.as_deref())
        };
        for event in events {
            self.update_state_from_event(&event, gateway_turn.as_ref()).await;
            if !matches!(event, AgentStreamEvent::Finish(_) | AgentStreamEvent::Error(_)) {
                if let Some(gateway_turn) = gateway_turn.as_ref() {
                    self.runtime.emit_for_turn(gateway_turn.runtime_turn, event);
                } else {
                    self.runtime.emit(event);
                }
            }
        }
    }

    async fn bind_run_to_active_turn(&self, runtime_turn: AgentRuntimeTurn, run_id: String) -> bool {
        let (pending, turn_generation) = {
            let mut state = self.state.write().await;
            if state.runtime_turn != Some(runtime_turn) {
                return false;
            }
            let turn_generation = state.turn_generation;
            self.text_state.lock().await.current_run_id = Some(run_id.clone());
            state.active_run_id = Some(run_id.clone());
            state.has_messages = true;
            (
                drain_events_for_run(&mut state.pending_run_events, &run_id),
                turn_generation,
            )
        };
        for event in pending {
            self.process_event_frame(
                event,
                Some(GatewayRunTurn {
                    run_id: run_id.clone(),
                    turn_generation,
                    runtime_turn,
                }),
            )
            .await;
        }
        true
    }

    async fn update_state_from_event(&self, event: &AgentStreamEvent, gateway_turn: Option<&GatewayRunTurn>) {
        match event {
            AgentStreamEvent::Start(data) => {
                if let (Some(gateway_turn), Some(sid)) = (gateway_turn, data.session_id.as_ref()) {
                    let mut state = self.state.write().await;
                    if gateway_turn_is_current(&state, gateway_turn) {
                        state.session_key = Some(sid.clone());
                    }
                }
            }
            AgentStreamEvent::Finish(data) => {
                let Some(gateway_turn) = gateway_turn else { return };
                let mut state = self.state.write().await;
                let is_same_run = gateway_turn_is_current(&state, gateway_turn);
                if is_same_run {
                    state.active_run_id = None;
                    state.runtime_turn = None;
                    if let Some(ref sid) = data.session_id {
                        state.session_key = Some(sid.clone());
                    }
                }
                drop(state);
                if is_same_run {
                    self.terminal_proof_tx.send_replace(Some(gateway_turn.clone()));
                }
                self.runtime.emit_finish_for_turn(
                    gateway_turn.runtime_turn,
                    data.session_id.clone(),
                    data.stop_reason,
                );
            }
            AgentStreamEvent::Error(data) => {
                let Some(gateway_turn) = gateway_turn else { return };
                let mut state = self.state.write().await;
                let is_same_run = gateway_turn_is_current(&state, gateway_turn);
                if is_same_run {
                    state.active_run_id = None;
                    state.runtime_turn = None;
                }
                drop(state);
                if is_same_run {
                    self.terminal_proof_tx.send_replace(Some(gateway_turn.clone()));
                }
                self.runtime
                    .emit_error_data_for_turn(gateway_turn.runtime_turn, data.clone());
            }
            AgentStreamEvent::AcpPermission(data) => {
                if let Some(conf) = data.as_confirmation() {
                    let mut state = self.state.write().await;
                    if let Some(existing) = state.confirmations.iter_mut().find(|c| c.call_id == conf.call_id) {
                        *existing = conf;
                    } else {
                        state.confirmations.push(conf);
                    }
                }
            }
            _ => {}
        }
    }

    async fn send_openclaw_message(
        &self,
        is_first: bool,
        runtime_turn: AgentRuntimeTurn,
        data: SendMessageData,
    ) -> Result<(), AppError> {
        if is_first {
            self.resolve_session().await?;
        }
        let session_key = self
            .state
            .read()
            .await
            .session_key
            .clone()
            .ok_or_else(|| AppError::Internal("Remote OpenClaw did not return a session key".into()))?;

        let params = ChatSendParams {
            session_key,
            message: data.content,
            idempotency_key: uuid::Uuid::new_v4().to_string(),
            attachments: if data.files.is_empty() {
                None
            } else {
                Some(data.files.into_iter().map(|file| json!(file)).collect())
            },
        };
        let response = self
            .connection
            .request::<Value>("chat.send", serde_json::to_value(params).unwrap_or_default())
            .await?;
        let active_run_id = response
            .get("runId")
            .or_else(|| response.get("run_id"))
            .and_then(Value::as_str)
            .filter(|run_id| !run_id.trim().is_empty())
            .map(ToOwned::to_owned)
            .ok_or_else(|| AppError::BadGateway("Remote OpenClaw chat.send returned no runId".into()))?;
        self.bind_run_to_active_turn(runtime_turn, active_run_id).await;
        Ok(())
    }

    async fn resolve_session(&self) -> Result<(), AppError> {
        let resume_key = self.state.read().await.session_key.clone();
        if let Some(ref key) = resume_key {
            match self
                .connection
                .request::<SessionsResolveResponse>(
                    "sessions.resolve",
                    serde_json::to_value(SessionsResolveParams { key: key.clone() }).unwrap_or_default(),
                )
                .await
            {
                Ok(resp) => {
                    if resp.ok == Some(false) {
                        warn!(
                            conversation_id = %self.runtime.conversation_id(),
                            "Remote sessions.resolve reported a missing session; creating a fresh session"
                        );
                    } else if let Some(resolved_key) = resp.key {
                        self.state.write().await.session_key = Some(resolved_key);
                        return Ok(());
                    } else {
                        warn!(
                            conversation_id = %self.runtime.conversation_id(),
                            "Remote sessions.resolve returned no key; creating a fresh session"
                        );
                    }
                }
                Err(error) => {
                    warn!(
                        conversation_id = %self.runtime.conversation_id(),
                        error = %ErrorChain(&error),
                        "Remote session resume failed; creating a fresh session"
                    );
                }
            }
        }

        let response: SessionsResetResponse = self
            .connection
            .request(
                "sessions.reset",
                serde_json::to_value(SessionsResetParams {
                    key: self.runtime.conversation_id().to_owned(),
                    reason: "new".into(),
                })
                .unwrap_or_default(),
            )
            .await?;
        let entry_session_id = response
            .entry
            .as_ref()
            .and_then(|entry| entry.get("sessionId"))
            .and_then(Value::as_str)
            .map(ToOwned::to_owned);
        let key = response
            .key
            .or(response.session_id)
            .or(entry_session_id)
            .ok_or_else(|| AppError::Internal("Remote OpenClaw sessions.reset returned no session key".into()))?;
        self.state.write().await.session_key = Some(key);
        Ok(())
    }

    pub async fn connection_status(&self) -> RemoteAgentStatus {
        self.state.read().await.connection_status
    }

    fn start_teardown_attempt(
        &self,
        reason: Option<AgentKillReason>,
    ) -> Result<TeardownAttempt, AppError> {
        info!(
            conversation_id = %self.runtime.conversation_id(),
            remote_agent_id = %self.remote_config.remote_agent_id,
            ?reason,
            "Starting ordered remote OpenClaw teardown"
        );
        let connection = Arc::clone(&self.connection);
        let state = Arc::clone(&self.state);
        let terminal_rx = self.terminal_proof_tx.subscribe();
        self.teardown
            .start_or_join(async move { run_remote_teardown(connection, state, terminal_rx).await })
    }
}

use crate::session::approval_key;

#[async_trait::async_trait]
impl crate::runtime_handle::AgentRuntimeControl for RemoteAgentManager {
    fn agent_type(&self) -> AgentType {
        AgentType::Remote
    }

    fn conversation_id(&self) -> &str {
        self.runtime.conversation_id()
    }

    fn workspace(&self) -> &str {
        self.runtime.workspace()
    }

    fn status(&self) -> Option<ConversationStatus> {
        self.runtime.status()
    }

    fn is_transport_healthy(&self) -> bool {
        self.runtime.is_transport_healthy()
    }

    fn last_activity_at(&self) -> TimestampMs {
        self.runtime.last_activity_at()
    }

    fn touch_activity(&self) {
        self.runtime.bump_activity();
    }

    fn subscribe(&self) -> broadcast::Receiver<AgentStreamEvent> {
        self.runtime.subscribe()
    }

    async fn send_message(&self, data: SendMessageData) -> Result<(), AgentSendError> {
        self.runtime.bump_activity();
        if !self.runtime.is_transport_healthy() {
            return Err(AgentSendError::stream_broken(
                "Remote OpenClaw's permanent connection relay is no longer running",
            ));
        }
        let runtime_turn = self.runtime.reset_for_new_turn(ConversationStatus::Running);
        if !self.runtime.is_transport_healthy() {
            let error = AgentSendError::stream_broken(
                "Remote OpenClaw's connection relay stopped during turn admission",
            );
            self.runtime
                .emit_error_data_for_turn(runtime_turn, error.stream_error().clone());
            return Err(error);
        }
        let is_first = {
            let mut state = self.state.write().await;
            state.turn_generation = state.turn_generation.wrapping_add(1);
            state.active_run_id = None;
            state.runtime_turn = Some(runtime_turn);
            state.pending_run_events.clear();
            !state.has_messages
        };
        {
            let mut text_state = self.text_state.lock().await;
            text_state.reset_for_new_turn();
        }

        match self.send_openclaw_message(is_first, runtime_turn, data).await {
            Ok(()) => {
                let mut state = self.state.write().await;
                if state.runtime_turn == Some(runtime_turn) {
                    state.has_messages = true;
                }
                Ok(())
            }
            Err(error) => {
                let mut state = self.state.write().await;
                if state.runtime_turn == Some(runtime_turn) {
                    state.active_run_id = None;
                    state.runtime_turn = None;
                    state.pending_run_events.clear();
                }
                drop(state);
                error!(
                    conversation_id = %self.runtime.conversation_id(),
                    error = %ErrorChain(&error),
                    "Remote OpenClaw send_message failed"
                );
                let send_error = AgentSendError::from_app_error(error);
                self.runtime
                    .emit_error_data_for_turn(runtime_turn, send_error.stream_error().clone());
                Err(send_error)
            }
        }
    }

    async fn cancel(&self) -> Result<(), AppError> {
        let target = {
            let state = self.state.read().await;
            teardown_target_from_state(&state)
        };
        let abort_result = if let Some(target) = target? {
            let params = ChatAbortParams {
                session_key: target.session_key,
                run_id: target.run_id,
            };
            self
                .connection
                .request::<Value>("chat.abort", serde_json::to_value(params).unwrap_or_default())
                .await
                .map(|_| ())
        } else {
            Ok(())
        };
        {
            let mut state = self.state.write().await;
            state.confirmations.clear();
        }

        // Only the matching gateway terminal clears the active run. A
        // timer-generated Finish would make a still-running remote task look
        // safely stopped and destroy the identity needed for teardown.
        abort_result
    }

    fn kill(&self, reason: Option<AgentKillReason>) -> Result<(), AppError> {
        self.start_teardown_attempt(reason)?;
        Ok(())
    }
}

impl RemoteAgentManager {
    pub fn kill_and_wait(
        &self,
        reason: Option<AgentKillReason>,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), AppError>> + Send>> {
        info!(
            conversation_id = %self.runtime.conversation_id(),
            ?reason,
            "Killing remote OpenClaw agent and waiting for connection close"
        );
        let attempt = self.start_teardown_attempt(reason);
        let teardown = Arc::clone(&self.teardown);
        Box::pin(async move {
            teardown
                .wait(attempt?, "Remote OpenClaw ordered teardown failed")
                .await
        })
    }

    /// Resolve a pending approval through the remote OpenClaw protocol.
    pub fn confirm(&self, _msg_id: &str, call_id: &str, data: Value, always_allow: bool) -> Result<(), AppError> {
        let request_id = match self.state.try_write() {
            Ok(mut state) => {
                let request_id = state
                    .confirmations
                    .iter()
                    .find(|confirmation| confirmation.call_id == call_id)
                    .map(|confirmation| confirmation.id.clone())
                    .ok_or_else(|| AppError::NotFound(format!("Remote approval '{call_id}' not found")))?;
                if always_allow
                    && let Some(conf) = state.confirmations.iter().find(|c| c.call_id == call_id)
                {
                    let key = approval_key(conf.action.as_deref(), conf.command_type.as_deref());
                    state.approval_memory.insert(key, true);
                }
                state.confirmations.retain(|c| c.call_id != call_id);
                request_id
            }
            Err(_) => return Err(AppError::Conflict("Remote approval state is busy".into())),
        };

        let decision = confirmation_option_id(&data)
            .unwrap_or_else(|| if always_allow { "allow-always" } else { "allow-once" }.to_owned());
        let decision = normalize_approval_decision(&decision);
        let connection = Arc::clone(&self.connection);
        tokio::spawn(async move {
            let params = json!({
                "id": request_id,
                "decision": decision,
            });
            if let Err(error) = connection.request::<Value>("exec.approval.resolve", params).await {
                warn!(error = %error, "Failed to send remote OpenClaw approval response");
            }
        });
        Ok(())
    }

    pub fn get_confirmations(&self) -> Vec<Confirmation> {
        self.state
            .try_read()
            .map(|state| state.confirmations.clone())
            .unwrap_or_default()
    }

    pub async fn clear_context(&self) -> Result<(), AppError> {
        let mut state = self.state.write().await;
        state.session_key = None;
        state.has_messages = false;
        state.active_run_id = None;
        state.runtime_turn = None;
        state.pending_run_events.clear();
        state.turn_generation = state.turn_generation.wrapping_add(1);
        state.confirmations.clear();
        Ok(())
    }

    pub fn check_approval(&self, action: &str, command_type: Option<&str>) -> bool {
        self.state
            .try_read()
            .map(|state| {
                let key = approval_key(Some(action), command_type);
                state.approval_memory.get(&key).copied().unwrap_or(false)
            })
            .unwrap_or(false)
    }

    pub fn get_session_key(&self) -> Option<String> {
        self.state.try_read().ok().and_then(|state| state.session_key.clone())
    }
}

fn require_remote_credential(config: &RemoteAgentConfig, label: &str) -> Result<String, AppError> {
    config
        .auth_token
        .clone()
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| AppError::BadRequest(format!("{label} is required for the selected remote authentication type")))
}

fn protocol_name(protocol: RemoteAgentProtocol) -> &'static str {
    match protocol {
        RemoteAgentProtocol::OpenClaw => "openclaw",
        RemoteAgentProtocol::ZeroClaw => "zeroclaw",
        RemoteAgentProtocol::Acp => "acp",
    }
}

fn confirmation_option_id(data: &Value) -> Option<String> {
    match data {
        Value::String(value) => Some(value.clone()),
        Value::Object(map) => map
            .get("option_id")
            .or_else(|| map.get("optionId"))
            .or_else(|| map.get("value"))
            .and_then(Value::as_str)
            .map(ToOwned::to_owned),
        _ => None,
    }
}

fn normalize_approval_decision(value: &str) -> String {
    match value {
        "allow_once" | "proceed_once" => "allow-once".to_owned(),
        "allow_always" | "proceed_always" | "proceed_always_server" | "proceed_always_tool" => {
            "allow-always".to_owned()
        }
        "deny_once" | "reject" | "cancel" => "deny".to_owned(),
        other => other.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use crate::manager::openclaw::device_identity::generate_identity;
    use crate::manager::openclaw::teardown::{
        TestAbortBehavior as AbortBehavior, spawn_test_gateway,
    };

    async fn connected_test_manager(
        behavior: AbortBehavior,
    ) -> (
        Arc<RemoteAgentManager>,
        Arc<AtomicUsize>,
        tokio::task::JoinHandle<()>,
    ) {
        let (url, abort_count, server) = spawn_test_gateway(behavior).await;
        let config = RemoteAgentConfig {
            remote_agent_id: RemoteAgentId::new(),
            protocol: RemoteAgentProtocol::OpenClaw,
            url,
            auth_type: "none".into(),
            auth_token: None,
            device_token: None,
            allow_insecure: false,
            resume_session_key: None,
            device_identity: Some(generate_identity()),
        };
        let (manager, _) = RemoteAgentManager::connect(
            "remote-teardown-test".into(),
            "/workspace".into(),
            config,
        )
        .await
        .unwrap();
        manager.start_event_relay().await;
        tokio::task::yield_now().await;
        (manager, abort_count, server)
    }

    async fn admit_test_run(manager: &RemoteAgentManager) {
        let runtime_turn = manager
            .runtime
            .reset_for_new_turn(ConversationStatus::Running);
        let mut state = manager.state.write().await;
        state.session_key = Some("session-1".into());
        state.active_run_id = Some("run-1".into());
        state.turn_generation = 1;
        state.runtime_turn = Some(runtime_turn);
        drop(state);
        let mut text_state = manager.text_state.lock().await;
        text_state.reset_for_new_turn();
        text_state.current_run_id = Some("run-1".into());
    }

    async fn finish_server(manager: &RemoteAgentManager, server: tokio::task::JoinHandle<()>) {
        manager.connection.close().await;
        tokio::time::timeout(Duration::from_secs(1), server)
            .await
            .expect("mock gateway did not observe connection close")
            .unwrap();
    }

    #[test]
    fn approval_key_formats_correctly() {
        assert_eq!(approval_key(Some("exec"), Some("curl")), "exec:curl");
        assert_eq!(approval_key(Some("exec"), None), "exec");
        assert_eq!(approval_key(None, None), "");
    }

    #[test]
    fn remote_agent_config_clone() {
        let config = RemoteAgentConfig {
            remote_agent_id: RemoteAgentId::new(),
            protocol: RemoteAgentProtocol::OpenClaw,
            url: "wss://example.com".into(),
            auth_type: "bearer".into(),
            auth_token: Some("token".into()),
            device_token: Some("device-token".into()),
            allow_insecure: false,
            resume_session_key: Some("session-1".into()),
            device_identity: None,
        };
        let cloned = config.clone();
        assert_eq!(cloned.remote_agent_id, config.remote_agent_id);
        assert_eq!(cloned.url, "wss://example.com");
        assert_eq!(cloned.resume_session_key.as_deref(), Some("session-1"));
        assert_eq!(cloned.device_token.as_deref(), Some("device-token"));
    }

    #[test]
    fn confirmation_option_accepts_common_shapes() {
        assert_eq!(
            confirmation_option_id(&json!({ "option_id": "allow_once" })).as_deref(),
            Some("allow_once")
        );
        assert_eq!(
            confirmation_option_id(&json!({ "optionId": "deny_once" })).as_deref(),
            Some("deny_once")
        );
        assert_eq!(normalize_approval_decision("proceed_once"), "allow-once");
        assert_eq!(normalize_approval_decision("proceed_always"), "allow-always");
        assert_eq!(normalize_approval_decision("cancel"), "deny");
    }

    #[tokio::test]
    async fn remote_teardown_abort_rpc_failure_returns_error_and_keeps_transport_for_retry() {
        let (manager, abort_count, server) =
            connected_test_manager(AbortBehavior::Reject).await;
        admit_test_run(&manager).await;

        let result = manager
            .kill_and_wait(Some(AgentKillReason::UserCancelled))
            .await;

        assert!(result.is_err());
        assert_eq!(abort_count.load(Ordering::SeqCst), 1);
        assert!(
            manager.connection.is_connected(),
            "failed teardown must retain the abort/terminal channel for quarantine retry"
        );
        finish_server(&manager, server).await;
    }

    #[tokio::test]
    async fn remote_teardown_without_terminal_proof_fails_closed() {
        let (manager, abort_count, server) =
            connected_test_manager(AbortBehavior::AcknowledgeOnly).await;
        admit_test_run(&manager).await;

        let result = manager
            .kill_and_wait(Some(AgentKillReason::UserCancelled))
            .await;

        assert!(result.is_err());
        assert_eq!(abort_count.load(Ordering::SeqCst), 1);
        assert!(manager.state.read().await.active_run_id.is_some());
        assert!(manager.connection.is_connected());
        finish_server(&manager, server).await;
    }

    #[tokio::test]
    async fn remote_teardown_accepts_exact_terminal_then_closes() {
        let (manager, abort_count, server) =
            connected_test_manager(AbortBehavior::AcknowledgeAndTerminate).await;
        admit_test_run(&manager).await;

        // Registry cancellation enters through synchronous kill first and then
        // joins with kill_and_wait. Both must share one abort attempt; closing
        // from kill before the abort would recreate the original race.
        crate::runtime_handle::AgentRuntimeControl::kill(
            manager.as_ref(),
            Some(AgentKillReason::UserCancelled),
        )
        .unwrap();
        manager
            .kill_and_wait(Some(AgentKillReason::UserCancelled))
            .await
            .unwrap();

        assert_eq!(abort_count.load(Ordering::SeqCst), 1);
        assert!(manager.state.read().await.active_run_id.is_none());
        assert!(!manager.connection.is_connected());
        tokio::time::timeout(Duration::from_secs(1), server)
            .await
            .expect("mock gateway did not observe successful close")
            .unwrap();
    }

    #[tokio::test]
    async fn idle_remote_teardown_closes_without_abort() {
        let (manager, abort_count, server) =
            connected_test_manager(AbortBehavior::AcknowledgeOnly).await;

        manager.kill_and_wait(None).await.unwrap();

        assert_eq!(abort_count.load(Ordering::SeqCst), 0);
        assert!(!manager.connection.is_connected());
        tokio::time::timeout(Duration::from_secs(1), server)
            .await
            .expect("mock gateway did not observe idle close")
            .unwrap();
    }
}

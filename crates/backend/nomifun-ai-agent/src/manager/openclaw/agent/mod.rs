use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use nomifun_common::{AgentKillReason, AgentType, AppError, Confirmation, ConversationStatus, ErrorChain, TimestampMs};
use serde_json::{Value, json};
use tokio::sync::{Mutex, RwLock, broadcast, watch};
use tracing::{debug, error, info, warn};

use crate::runtime_state::{AgentRuntimeState, AgentRuntimeTurn};
use crate::capability::cli_process::CliAgentProcess;
use crate::manager::process_registry::register_session_process;
use crate::protocol::events::AgentStreamEvent;
use crate::protocol::send_error::AgentSendError;
use crate::types::SendMessageData;
use nomifun_api_types::OpenClawBuildExtra;

use super::config::load_openclaw_config;
use super::connection::{AuthConfig, OpenClawConnection};
use super::device_identity::load_or_create_identity;
use super::event_mapper::{
    TextFallbackState, drain_events_for_run, is_openclaw_turn_event, map_openclaw_event, openclaw_event_run_id,
};
use super::protocol::{
    ChatAbortParams, ChatSendParams, EventFrame, SessionsResetParams, SessionsResetResponse,
    SessionsResolveParams, SessionsResolveResponse, normalize_ws_url,
};
use super::teardown::{
    GatewayRunTurn, GatewayTeardownTarget, TeardownAttempt, TeardownCoordinator,
    request_abort_bounded, wait_for_terminal_proof,
};

mod confirmations;
mod spawn_helpers;

use spawn_helpers::{build_spawn_config, is_port_listening, wait_for_gateway_ready};

pub const DEFAULT_GATEWAY_PORT: u16 = 18789;

const OPENCLAW_KILL_GRACE_MS: u64 = 1000;
pub(super) const GATEWAY_READY_TIMEOUT: Duration = Duration::from_secs(10);
pub(super) const GATEWAY_READY_POLL_INTERVAL: Duration = Duration::from_millis(200);
#[cfg(not(test))]
const OPENCLAW_TEARDOWN_RPC_TIMEOUT: Duration = Duration::from_secs(5);
#[cfg(test)]
const OPENCLAW_TEARDOWN_RPC_TIMEOUT: Duration = Duration::from_millis(200);
#[cfg(not(test))]
const OPENCLAW_TEARDOWN_TERMINAL_TIMEOUT: Duration = Duration::from_secs(5);
#[cfg(test)]
const OPENCLAW_TEARDOWN_TERMINAL_TIMEOUT: Duration = Duration::from_millis(200);

pub(super) struct OpenClawState {
    pub(super) session_key: Option<String>,
    pub(super) confirmations: Vec<Confirmation>,
    pub(super) has_messages: bool,
    pub(super) active_run_id: Option<String>,
    pub(super) turn_generation: u64,
    pub(super) runtime_turn: Option<AgentRuntimeTurn>,
    pending_run_events: Vec<EventFrame>,
    pub(super) approval_memory: HashMap<String, bool>,
}

fn gateway_turn_is_current(state: &OpenClawState, gateway_turn: &GatewayRunTurn) -> bool {
    state.active_run_id.as_deref() == Some(gateway_turn.run_id.as_str())
        && state.turn_generation == gateway_turn.turn_generation
        && state.runtime_turn == Some(gateway_turn.runtime_turn)
}

fn teardown_target_from_state(state: &OpenClawState) -> Result<Option<GatewayTeardownTarget>, AppError> {
    match (state.runtime_turn, state.active_run_id.as_ref()) {
        (None, None) => Ok(None),
        (None, Some(run_id)) => Err(AppError::Internal(format!(
            "OpenClaw lifecycle invariant violated: run {run_id} has no runtime turn"
        ))),
        (Some(runtime_turn), run_id) => {
            let session_key = state.session_key.clone().ok_or_else(|| {
                AppError::Conflict(
                    "OpenClaw has an admitted turn but no session key; chat.abort cannot identify it".into(),
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

async fn kill_owned_gateway_process(
    connection: &OpenClawConnection,
    process: Arc<CliAgentProcess>,
) -> Result<(), AppError> {
    connection.close().await;
    process
        .kill(Duration::from_millis(OPENCLAW_KILL_GRACE_MS))
        .await
}

async fn run_openclaw_teardown(
    connection: Arc<OpenClawConnection>,
    state: Arc<RwLock<OpenClawState>>,
    terminal_rx: watch::Receiver<Option<GatewayRunTurn>>,
    gateway_process: Option<Arc<CliAgentProcess>>,
) -> Result<(), AppError> {
    // A previously proven local process-tree exit remains authoritative on a
    // quarantine retry even if the first attempt had to report an abort RPC
    // error. This lets the registry release the slot on the next audit without
    // pretending the original protocol failure did not happen.
    if let Some(process) = gateway_process.as_ref()
        && process.exit_status().is_some()
    {
        connection.close().await;
        return Ok(());
    }

    let target = {
        let state = state.read().await;
        teardown_target_from_state(&state)
    };
    let target = match target {
        Ok(target) => target,
        Err(state_error) => {
            let Some(process) = gateway_process else {
                return Err(state_error);
            };
            return match kill_owned_gateway_process(&connection, process).await {
                Ok(()) => Err(state_error),
                Err(kill_error) => Err(AppError::Internal(format!(
                    "{state_error}; local OpenClaw process teardown also failed: {kill_error}"
                ))),
            };
        }
    };

    let Some(target) = target else {
        connection.close().await;
        if let Some(process) = gateway_process {
            process
                .kill(Duration::from_millis(OPENCLAW_KILL_GRACE_MS))
                .await?;
        }
        return Ok(());
    };

    let params = serde_json::to_value(ChatAbortParams {
        session_key: target.session_key.clone(),
        run_id: target.run_id.clone(),
    })
    .map_err(|error| AppError::Internal(format!("Failed to serialize OpenClaw chat.abort: {error}")))?;
    let abort_result = request_abort_bounded(
        async {
            connection
                .request::<Value>("chat.abort", params)
                .await
                .map(|_| ())
        },
        OPENCLAW_TEARDOWN_RPC_TIMEOUT,
        "OpenClaw teardown",
    )
    .await;

    if let Err(abort_error) = abort_result {
        let Some(process) = gateway_process else {
            // Keep the transport alive: an externally managed gateway may
            // still publish a real terminal, and a quarantine retry must retain
            // the ability to issue a fresh abort.
            return Err(abort_error);
        };
        return match kill_owned_gateway_process(&connection, process).await {
            Ok(()) => Err(abort_error),
            Err(kill_error) => Err(AppError::Internal(format!(
                "{abort_error}; local OpenClaw process teardown also failed: {kill_error}"
            ))),
        };
    }

    match wait_for_terminal_proof(
        &target,
        terminal_rx,
        OPENCLAW_TEARDOWN_TERMINAL_TIMEOUT,
        "OpenClaw teardown",
    )
    .await
    {
        Ok(()) => {
            connection.close().await;
            if let Some(process) = gateway_process {
                process
                    .kill(Duration::from_millis(OPENCLAW_KILL_GRACE_MS))
                    .await?;
            }
            Ok(())
        }
        Err(terminal_error) => {
            let Some(process) = gateway_process else {
                // Closing a socket does not stop work owned by an external
                // gateway. Preserve the connection and fail closed.
                return Err(terminal_error);
            };
            // For a gateway process spawned and exclusively owned by this
            // manager, exact process-tree exit is an independent proof that no
            // local tools or write-back work can continue.
            kill_owned_gateway_process(&connection, process).await
        }
    }
}

fn admit_gateway_turn(state: &mut OpenClawState, runtime_turn: AgentRuntimeTurn) -> bool {
    let is_first = !state.has_messages;
    state.active_run_id = None;
    state.turn_generation = state.turn_generation.wrapping_add(1);
    state.runtime_turn = Some(runtime_turn);
    state.pending_run_events.clear();
    is_first
}

fn abandon_gateway_turn(state: &mut OpenClawState, runtime_turn: AgentRuntimeTurn) {
    if state.runtime_turn == Some(runtime_turn) {
        state.active_run_id = None;
        state.runtime_turn = None;
        state.pending_run_events.clear();
    }
}

async fn map_event_for_gateway_turn(
    state: &RwLock<OpenClawState>,
    text_state: &Mutex<TextFallbackState>,
    event_frame: &EventFrame,
    gateway_turn: &GatewayRunTurn,
) -> Option<Vec<AgentStreamEvent>> {
    // The read guard is intentionally held across mapper mutation. New-turn
    // admission requires the write guard before it resets `text_state`, which
    // makes check+map and reset one linearized order.
    let state = state.read().await;
    if !gateway_turn_is_current(&state, gateway_turn) {
        return None;
    }
    let session_key = state.session_key.clone();
    let mut text_state = text_state.lock().await;
    Some(map_openclaw_event(
        event_frame,
        &mut text_state,
        session_key.as_deref(),
    ))
}

pub struct OpenClawAgentManager {
    runtime: AgentRuntimeState,
    config: OpenClawBuildExtra,
    gateway_process: Option<Arc<CliAgentProcess>>,
    pub(super) connection: Arc<OpenClawConnection>,
    pub(super) state: Arc<RwLock<OpenClawState>>,
    text_state: Mutex<TextFallbackState>,
    terminal_proof_tx: watch::Sender<Option<GatewayRunTurn>>,
    teardown: Arc<TeardownCoordinator>,
}

impl OpenClawAgentManager {
    pub async fn new(
        conversation_id: String,
        workspace: String,
        config: OpenClawBuildExtra,
        resume_session_key: Option<String>,
        data_dir: std::path::PathBuf,
    ) -> Result<Self, AppError> {
        let file_config = load_openclaw_config();

        let host = config.gateway.host.as_deref().unwrap_or("127.0.0.1");
        let port = config
            .gateway
            .port
            .or_else(|| {
                file_config
                    .as_ref()
                    .and_then(|c| c.gateway.as_ref())
                    .and_then(|g| g.port)
            })
            .unwrap_or(DEFAULT_GATEWAY_PORT);

        let gateway_process = if !config.gateway.use_external_gateway {
            let cli_path = config
                .gateway
                .cli_path
                .as_deref()
                .ok_or_else(|| AppError::BadRequest("OpenClaw CLI path is required".into()))?;

            if !is_port_listening(host, port).await {
                let spawn_config = build_spawn_config(cli_path, &workspace, &config.gateway);
                let command_preview = spawn_config.command.display().to_string();
                let process = Arc::new(CliAgentProcess::spawn(spawn_config).await?);
                register_session_process(
                    &data_dir,
                    Arc::clone(&process),
                    conversation_id.clone(),
                    AgentType::OpenclawGateway,
                    None,
                    Some(command_preview),
                )?;

                wait_for_gateway_ready(host, port).await?;

                info!(
                    conversation_id = %conversation_id,
                    port = port,
                    "OpenClaw gateway subprocess ready"
                );

                Some(process)
            } else {
                debug!(port = port, "OpenClaw gateway already listening, skipping spawn");
                None
            }
        } else {
            None
        };

        let ws_url = normalize_ws_url(host, port);

        let identity = load_or_create_identity(None)?;

        let shared_token = config
            .gateway
            .token
            .clone()
            .or_else(|| super::config::get_gateway_auth_token(file_config.as_ref()));
        let device_token =
            super::device_auth_store::load_device_auth_token(&identity.device_id, "operator").map(|entry| entry.token);
        let password = config
            .gateway
            .password
            .clone()
            .or_else(|| super::config::get_gateway_auth_password(file_config.as_ref()));

        let auth = if shared_token.is_some() || device_token.is_some() || password.is_some() {
            Some(AuthConfig {
                token: shared_token,
                device_token,
                password,
            })
        } else {
            None
        };

        let (connection, hello) = OpenClawConnection::connect(&ws_url, auth, &identity)
            .await
            .inspect_err(|e| {
                error!(
                    conversation_id = %conversation_id,
                    url = %ws_url,
                    error = %ErrorChain(e),
                    "Failed to connect to OpenClaw gateway"
                );
            })?;

        if let Some(ref device_token) = hello.auth.device_token
        {
            super::device_auth_store::store_device_auth_token(
                &identity.device_id,
                &hello.auth.role,
                device_token,
                &hello.auth.scopes,
            );
        }

        info!(
            conversation_id = %conversation_id,
            url = %ws_url,
            "Connected to OpenClaw gateway via WebSocket"
        );

        let has_resume_key = resume_session_key.is_some();
        if has_resume_key {
            info!(
                conversation_id = %conversation_id,
                "Resuming OpenClaw session with stored session key"
            );
        }

        let runtime = AgentRuntimeState::new(conversation_id, workspace, 256);

        let (terminal_proof_tx, _) = watch::channel(None);
        let manager = Self {
            runtime,
            config,
            gateway_process,
            connection: Arc::clone(&connection),
            state: Arc::new(RwLock::new(OpenClawState {
                session_key: resume_session_key,
                confirmations: Vec::new(),
                has_messages: has_resume_key,
                active_run_id: None,
                turn_generation: 0,
                runtime_turn: None,
                pending_run_events: Vec::new(),
                approval_memory: HashMap::new(),
            })),
            text_state: Mutex::new(TextFallbackState::new()),
            terminal_proof_tx,
            teardown: Arc::new(TeardownCoordinator::default()),
        };

        Ok(manager)
    }

    pub fn start_event_relay(self: &Arc<Self>) {
        let this = Arc::clone(self);
        tokio::spawn(async move {
            this.run_event_relay().await;
        });
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
                            "OpenClaw event relay lagged"
                        );
                        self.runtime.emit_stream_broken(format!(
                            "OpenClaw event relay lost {n} buffered event(s)"
                        ));
                        break;
                    }
                    Err(broadcast::error::RecvError::Closed) => break,
                },
                _ = close_rx.recv() => break,
            }
        }

        if self.runtime.status() == Some(ConversationStatus::Running) {
            self.runtime.emit_stream_broken("OpenClaw connection closed");
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
                    "Dropping turn-scoped OpenClaw event without runId"
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
                    debug!(
                        conversation_id = %self.runtime.conversation_id(),
                        %event_run_id,
                        %active_run_id,
                        "Dropping delayed OpenClaw event from another run"
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
                            "OpenClaw produced too many events before acknowledging chat.send",
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
        let stream_events = if let Some(gateway_turn) = gateway_turn.as_ref() {
            // Keep the run/token validation guard across mutation of the
            // shared mapper state. A new turn needs this state write lock
            // before resetting TextFallbackState, so an old frame can finish
            // mapping before that reset or be rejected after it—never write
            // into the new turn between check and map.
            let Some(events) = map_event_for_gateway_turn(
                &self.state,
                &self.text_state,
                &event_frame,
                gateway_turn,
            )
            .await
            else {
                return;
            };
            events
        } else {
            let session_key = self.state.read().await.session_key.clone();
            let mut text_state = self.text_state.lock().await;
            map_openclaw_event(&event_frame, &mut text_state, session_key.as_deref())
        };

        for stream_event in stream_events {
            self.update_state_from_event(&stream_event, gateway_turn.as_ref()).await;
            if !matches!(stream_event, AgentStreamEvent::Finish(_) | AgentStreamEvent::Error(_)) {
                if let Some(gateway_turn) = gateway_turn.as_ref() {
                    self.runtime.emit_for_turn(gateway_turn.runtime_turn, stream_event);
                } else {
                    self.runtime.emit(stream_event);
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
            // Lock order is always manager state -> text mapper state. Anchor
            // the mapper before making active_run_id visible to the relay.
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
                    let mut guard = self.state.write().await;
                    if let Some(existing) = guard.confirmations.iter_mut().find(|c| c.call_id == conf.call_id) {
                        *existing = conf;
                    } else {
                        guard.confirmations.push(conf);
                    }
                }
            }
            _ => {}
        }
    }

    async fn do_send_message(
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
            .ok_or_else(|| AppError::Internal("No active session key".into()))?;

        let params = ChatSendParams {
            session_key,
            message: data.content,
            idempotency_key: uuid::Uuid::new_v4().to_string(),
            attachments: if data.files.is_empty() {
                None
            } else {
                Some(data.files.into_iter().map(|f| json!(f)).collect())
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
            .ok_or_else(|| AppError::BadGateway("OpenClaw chat.send returned no runId".into()))?;
        self.bind_run_to_active_turn(runtime_turn, active_run_id).await;

        Ok(())
    }

    /// Resolve gateway session: try to resume an existing session first,
    /// then fall back to creating a new one via sessions.reset.
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
                            "OpenClaw sessions.resolve reported a missing session, falling back to sessions.reset"
                        );
                    } else if let Some(resolved_key) = resp.key {
                        self.state.write().await.session_key = Some(resolved_key.clone());
                        info!(
                            conversation_id = %self.runtime.conversation_id(),
                            session_key = %resolved_key,
                            "Resumed OpenClaw session via sessions.resolve"
                        );
                        return Ok(());
                    } else {
                        warn!(
                            conversation_id = %self.runtime.conversation_id(),
                            "OpenClaw sessions.resolve returned no key, falling back to sessions.reset"
                        );
                    }
                }
                Err(e) => {
                    warn!(
                        conversation_id = %self.runtime.conversation_id(),
                        error = %ErrorChain(&e),
                        "Failed to resume OpenClaw session, falling back to sessions.reset"
                    );
                }
            }
        }

        let resp: SessionsResetResponse = self
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

        let entry_session_id = resp
            .entry
            .as_ref()
            .and_then(|entry| entry.get("sessionId"))
            .and_then(Value::as_str)
            .map(ToOwned::to_owned);
        let key = resp
            .key
            .or(resp.session_id)
            .or(entry_session_id)
            .ok_or_else(|| AppError::Internal("OpenClaw sessions.reset returned no session key".into()))?;
        self.state.write().await.session_key = Some(key);

        Ok(())
    }

    /// Clear the conversation context ("release model context"): forget the
    /// gateway session key and pending confirmations so the next send is
    /// treated as a first message — `resolve_session` then falls straight to
    /// `sessions.reset`, allocating a brand-new gateway session with no
    /// history. Robust even when the gateway is momentarily disconnected: the
    /// reset happens lazily on the next send.
    pub async fn clear_context(&self) -> Result<(), AppError> {
        info!(
            conversation_id = %self.runtime.conversation_id(),
            "Clearing OpenClaw context"
        );
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

    pub async fn get_diagnostics(&self) -> Value {
        let state = self.state.read().await;
        let host = self.config.gateway.host.as_deref().unwrap_or("127.0.0.1");
        let port = self.config.gateway.port.unwrap_or(DEFAULT_GATEWAY_PORT);

        json!({
            "workspace": self.runtime.workspace(),
            "backend": serde_json::to_value(&self.config.backend).unwrap_or_default(),
            "agentName": self.config.agent_name,
            "cliPath": self.config.gateway.cli_path,
            "gatewayHost": host,
            "gatewayPort": port,
            "conversationId": self.runtime.conversation_id(),
            "isConnected": self.connection.is_connected(),
            "hasActiveSession": state.session_key.is_some(),
            "sessionKey": state.session_key,
        })
    }

    fn start_teardown_attempt(
        &self,
        reason: Option<AgentKillReason>,
    ) -> Result<TeardownAttempt, AppError> {
        info!(
            conversation_id = %self.runtime.conversation_id(),
            ?reason,
            "Starting ordered OpenClaw teardown"
        );
        let connection = Arc::clone(&self.connection);
        let state = Arc::clone(&self.state);
        let terminal_rx = self.terminal_proof_tx.subscribe();
        let gateway_process = self.gateway_process.clone();
        self.teardown.start_or_join(async move {
            run_openclaw_teardown(connection, state, terminal_rx, gateway_process).await
        })
    }
}

#[cfg(test)]
mod turn_lifecycle_tests {
    use super::*;

    fn state_for_turn(turn: AgentRuntimeTurn, run_id: Option<&str>) -> OpenClawState {
        OpenClawState {
            session_key: Some("session-1".into()),
            confirmations: Vec::new(),
            has_messages: run_id.is_some(),
            active_run_id: run_id.map(str::to_owned),
            turn_generation: 1,
            runtime_turn: Some(turn),
            pending_run_events: Vec::new(),
            approval_memory: HashMap::new(),
        }
    }

    #[test]
    fn first_send_failure_does_not_poison_next_send_admission() {
        let runtime = AgentRuntimeState::new("openclaw-first-send", "/workspace", 8);
        let first_turn = runtime.reset_for_new_turn(ConversationStatus::Running);
        let mut state = state_for_turn(first_turn, None);
        state.has_messages = false;

        assert!(admit_gateway_turn(&mut state, first_turn));
        assert!(!state.has_messages, "admission alone must not claim a successful message");
        abandon_gateway_turn(&mut state, first_turn);

        let second_turn = runtime.reset_for_new_turn(ConversationStatus::Running);
        assert!(
            admit_gateway_turn(&mut state, second_turn),
            "a failed first chat.send must retry session resolution on the next turn"
        );
    }

    #[tokio::test]
    async fn old_frame_mapping_is_linearized_before_new_turn_text_reset() {
        let runtime = AgentRuntimeState::new("openclaw-map-order", "/workspace", 8);
        let old_turn = runtime.reset_for_new_turn(ConversationStatus::Running);
        let state = Arc::new(RwLock::new(state_for_turn(old_turn, Some("run-old"))));
        let text_state = Arc::new(Mutex::new(TextFallbackState::new()));
        let held_text = text_state.lock().await;
        let old_binding = GatewayRunTurn {
            run_id: "run-old".into(),
            turn_generation: 1,
            runtime_turn: old_turn,
        };
        let old_frame = EventFrame {
            event: "chat".into(),
            payload: Some(json!({
                "runId": "run-old",
                "state": "delta",
                "deltaText": "stale"
            })),
            seq: None,
        };

        let state_for_old = Arc::clone(&state);
        let text_for_old = Arc::clone(&text_state);
        let old_mapper = tokio::spawn(async move {
            map_event_for_gateway_turn(&state_for_old, &text_for_old, &old_frame, &old_binding).await
        });

        // Wait until the mapper has acquired the state read guard and is
        // blocked on the text mutex we hold.
        let mut mapper_holds_state = false;
        for _ in 0..100 {
            if state.try_write().is_err() {
                mapper_holds_state = true;
                break;
            }
            tokio::task::yield_now().await;
        }
        assert!(mapper_holds_state, "old mapper never reached its linearization guard");

        let new_turn = runtime.reset_for_new_turn(ConversationStatus::Running);
        let state_for_new = Arc::clone(&state);
        let text_for_new = Arc::clone(&text_state);
        let new_admission = tokio::spawn(async move {
            let mut state = state_for_new.write().await;
            admit_gateway_turn(&mut state, new_turn);
            let mut text = text_for_new.lock().await;
            text.reset_for_new_turn();
            text.current_run_id = Some("run-new".into());
        });

        drop(held_text);
        assert!(old_mapper.await.unwrap().is_some());
        new_admission.await.unwrap();

        let text = text_state.lock().await;
        assert_eq!(text.current_run_id.as_deref(), Some("run-new"));
        assert!(text.accumulated_text.is_empty(), "old run text leaked past the new-turn reset");
    }
}

#[cfg(test)]
mod teardown_tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use nomifun_api_types::OpenClawGatewayConfig;

    use super::super::device_identity::generate_identity;
    use super::super::teardown::{
        TestAbortBehavior as AbortBehavior, spawn_test_gateway,
    };

    async fn connected_test_manager(
        behavior: AbortBehavior,
        active: bool,
    ) -> (
        Arc<OpenClawAgentManager>,
        Arc<AtomicUsize>,
        tokio::task::JoinHandle<()>,
    ) {
        let (url, abort_count, server) = spawn_test_gateway(behavior).await;
        let (connection, _) =
            OpenClawConnection::connect(&url, None, &generate_identity())
                .await
                .unwrap();
        let runtime = AgentRuntimeState::new("openclaw-teardown-test", "/workspace", 16);
        let runtime_turn =
            active.then(|| runtime.reset_for_new_turn(ConversationStatus::Running));
        let (terminal_proof_tx, _) = watch::channel(None);
        let manager = Arc::new(OpenClawAgentManager {
            runtime,
            config: OpenClawBuildExtra {
                backend: None,
                agent_name: None,
                gateway: OpenClawGatewayConfig::default(),
                skills: Vec::new(),
                preset_id: None,
                cron_job_id: None,
                session_key: None,
            },
            gateway_process: None,
            connection,
            state: Arc::new(RwLock::new(OpenClawState {
                session_key: Some("session-1".into()),
                confirmations: Vec::new(),
                has_messages: active,
                active_run_id: active.then(|| "run-1".into()),
                turn_generation: u64::from(active),
                runtime_turn,
                pending_run_events: Vec::new(),
                approval_memory: HashMap::new(),
            })),
            text_state: Mutex::new(TextFallbackState::new()),
            terminal_proof_tx,
            teardown: Arc::new(TeardownCoordinator::default()),
        });
        if active {
            let mut text_state = manager.text_state.lock().await;
            text_state.reset_for_new_turn();
            text_state.current_run_id = Some("run-1".into());
        }
        manager.start_event_relay();
        tokio::task::yield_now().await;
        (manager, abort_count, server)
    }

    async fn finish_server(
        manager: &OpenClawAgentManager,
        server: tokio::task::JoinHandle<()>,
    ) {
        manager.connection.close().await;
        tokio::time::timeout(Duration::from_secs(1), server)
            .await
            .expect("mock gateway did not observe connection close")
            .unwrap();
    }

    #[tokio::test]
    async fn openclaw_abort_rpc_failure_is_a_teardown_error() {
        let (manager, abort_count, server) =
            connected_test_manager(AbortBehavior::Reject, true).await;

        let result = manager
            .kill_and_wait(Some(AgentKillReason::UserCancelled))
            .await;

        assert!(result.is_err());
        assert_eq!(abort_count.load(Ordering::SeqCst), 1);
        assert!(manager.connection.is_connected());
        finish_server(&manager, server).await;
    }

    #[tokio::test]
    async fn openclaw_exact_terminal_allows_external_gateway_close() {
        let (manager, abort_count, server) =
            connected_test_manager(AbortBehavior::AcknowledgeAndTerminate, true).await;

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
    async fn idle_openclaw_teardown_closes_without_abort() {
        let (manager, abort_count, server) =
            connected_test_manager(AbortBehavior::AcknowledgeOnly, false).await;

        manager.kill_and_wait(None).await.unwrap();

        assert_eq!(abort_count.load(Ordering::SeqCst), 0);
        assert!(!manager.connection.is_connected());
        tokio::time::timeout(Duration::from_secs(1), server)
            .await
            .expect("mock gateway did not observe idle close")
            .unwrap();
    }
}

#[async_trait::async_trait]
impl crate::runtime_handle::AgentRuntimeControl for OpenClawAgentManager {
    fn agent_type(&self) -> AgentType {
        AgentType::OpenclawGateway
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
                "OpenClaw's permanent connection relay is no longer running",
            ));
        }

        let runtime_turn = self.runtime.reset_for_new_turn(ConversationStatus::Running);
        let is_first = {
            let mut state = self.state.write().await;
            admit_gateway_turn(&mut state, runtime_turn)
        };
        if !self.runtime.is_transport_healthy() {
            let error = AgentSendError::stream_broken(
                "OpenClaw's connection relay stopped during turn admission",
            );
            let mut state = self.state.write().await;
            abandon_gateway_turn(&mut state, runtime_turn);
            drop(state);
            self.runtime
                .emit_error_data_for_turn(runtime_turn, error.stream_error().clone());
            return Err(error);
        }

        {
            let mut text_state = self.text_state.lock().await;
            text_state.reset_for_new_turn();
        }

        match self.do_send_message(is_first, runtime_turn, data).await {
            Ok(()) => Ok(()),
            Err(err) => {
                let mut state = self.state.write().await;
                abandon_gateway_turn(&mut state, runtime_turn);
                drop(state);
                error!(
                    conversation_id = %self.runtime.conversation_id(),
                    error = %ErrorChain(&err),
                    "OpenClaw send_message failed, emitting terminal Error"
                );
                let send_error = AgentSendError::from_app_error(err);
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

        // The real gateway terminal event owns state clearing and Finish/Error
        // emission. A timer-generated Finish would erase the only run identity
        // teardown can use while the gateway may still be executing tools.
        abort_result
    }

    fn kill(&self, reason: Option<AgentKillReason>) -> Result<(), AppError> {
        self.start_teardown_attempt(reason)?;
        Ok(())
    }
}

impl OpenClawAgentManager {
    pub fn kill_and_wait(
        &self,
        reason: Option<AgentKillReason>,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), AppError>> + Send>> {
        info!(
            conversation_id = %self.runtime.conversation_id(),
            ?reason,
            "Killing OpenClaw agent and waiting for shutdown"
        );
        let attempt = self.start_teardown_attempt(reason);
        let teardown = Arc::clone(&self.teardown);
        Box::pin(async move {
            teardown
                .wait(attempt?, "OpenClaw ordered teardown failed")
                .await
        })
    }
}

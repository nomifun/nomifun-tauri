use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use nomifun_common::{
    AgentKillReason, AgentType, AppError, Confirmation, ConversationStatus, ErrorChain, RemoteAgentProtocol,
    RemoteAgentId, RemoteAgentStatus, TimestampMs,
};
use serde_json::{Value, json};
use tokio::sync::{Mutex, RwLock, broadcast};
use tracing::{error, info, warn};

use crate::manager::openclaw::connection::{AuthConfig, OpenClawConnection};
use crate::manager::openclaw::device_identity::DeviceIdentity;
use crate::manager::openclaw::event_mapper::{TextFallbackState, map_openclaw_event};
use crate::manager::openclaw::protocol::{
    ChatAbortParams, ChatSendParams, SessionsResetParams, SessionsResetResponse, SessionsResolveParams,
    SessionsResolveResponse,
};
use crate::runtime_state::AgentRuntimeState;
use crate::protocol::events::AgentStreamEvent;
use crate::protocol::send_error::AgentSendError;
use crate::types::SendMessageData;

const STOP_FINISH_FALLBACK_TIMEOUT: Duration = Duration::from_secs(5);

/// Internal mutable state for a remotely hosted agent session.
struct RemoteState {
    session_key: Option<String>,
    confirmations: Vec<Confirmation>,
    has_messages: bool,
    active_run_id: Option<String>,
    turn_generation: u64,
    approval_memory: HashMap<String, bool>,
    connection_status: RemoteAgentStatus,
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

        let manager = Arc::new(Self {
            runtime: AgentRuntimeState::new(conversation_id, workspace, 256),
            connection,
            state: Arc::new(RwLock::new(RemoteState {
                session_key: remote_config.resume_session_key.clone(),
                confirmations: Vec::new(),
                has_messages: false,
                active_run_id: None,
                turn_generation: 0,
                approval_memory: HashMap::new(),
                connection_status: RemoteAgentStatus::Connected,
            })),
            remote_config,
            text_state: Mutex::new(TextFallbackState::new()),
            _reader_handle: Mutex::new(None),
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
                        let session_key = self.state.read().await.session_key.clone();
                        let events = {
                            let mut text_state = self.text_state.lock().await;
                            map_openclaw_event(&event_frame, &mut text_state, session_key.as_deref())
                        };
                        for event in events {
                            self.update_state_from_event(&event).await;
                            if !matches!(event, AgentStreamEvent::Finish(_) | AgentStreamEvent::Error(_)) {
                                self.runtime.emit(event);
                            }
                        }
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

    async fn update_state_from_event(&self, event: &AgentStreamEvent) {
        match event {
            AgentStreamEvent::Start(data) => {
                self.runtime.reset_for_new_turn(ConversationStatus::Running);
                if let Some(ref sid) = data.session_id {
                    self.state.write().await.session_key = Some(sid.clone());
                }
            }
            AgentStreamEvent::Finish(data) => {
                let mut state = self.state.write().await;
                state.active_run_id = None;
                if let Some(ref sid) = data.session_id {
                    state.session_key = Some(sid.clone());
                }
                drop(state);
                self.runtime
                    .emit_finish_with_reason(data.session_id.clone(), data.stop_reason);
            }
            AgentStreamEvent::Error(data) => {
                self.state.write().await.active_run_id = None;
                self.runtime.emit_error_data(data.clone());
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

    async fn send_openclaw_message(&self, is_first: bool, data: SendMessageData) -> Result<(), AppError> {
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
        self.state.write().await.active_run_id = Some(active_run_id);
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
        self.runtime.reset_for_new_turn(ConversationStatus::Running);
        if !self.runtime.is_transport_healthy() {
            let error = AgentSendError::stream_broken(
                "Remote OpenClaw's connection relay stopped during turn admission",
            );
            self.runtime.emit_error_data(error.stream_error().clone());
            return Err(error);
        }
        let is_first = {
            let mut state = self.state.write().await;
            state.turn_generation = state.turn_generation.wrapping_add(1);
            state.active_run_id = None;
            !state.has_messages
        };
        {
            let mut text_state = self.text_state.lock().await;
            text_state.reset_for_new_turn();
        }

        match self.send_openclaw_message(is_first, data).await {
            Ok(()) => {
                self.state.write().await.has_messages = true;
                Ok(())
            }
            Err(error) => {
                self.state.write().await.active_run_id = None;
                error!(
                    conversation_id = %self.runtime.conversation_id(),
                    error = %ErrorChain(&error),
                    "Remote OpenClaw send_message failed"
                );
                let send_error = AgentSendError::from_app_error(error);
                self.runtime.emit_error_data(send_error.stream_error().clone());
                Err(send_error)
            }
        }
    }

    async fn cancel(&self) -> Result<(), AppError> {
        let (session_key, run_id, turn_generation) = {
            let state = self.state.read().await;
            (
                state.session_key.clone(),
                state.active_run_id.clone(),
                state.turn_generation,
            )
        };
        if let Some(session_key) = session_key {
            let params = ChatAbortParams {
                session_key,
                run_id,
            };
            let _ = self
                .connection
                .request::<Value>("chat.abort", serde_json::to_value(params).unwrap_or_default())
                .await;
        }
        {
            let mut state = self.state.write().await;
            state.confirmations.clear();
            state.active_run_id = None;
        }

        let runtime = self.runtime.clone();
        let state = Arc::clone(&self.state);
        let conversation_id = self.runtime.conversation_id().to_owned();
        tokio::spawn(async move {
            tokio::time::sleep(STOP_FINISH_FALLBACK_TIMEOUT).await;
            let is_same_turn = state.read().await.turn_generation == turn_generation;
            if is_same_turn && runtime.status() == Some(ConversationStatus::Running) {
                warn!(
                    conversation_id = %conversation_id,
                    "Remote Gateway did not send abort event within timeout, emitting fallback Finish"
                );
                runtime.emit_finish_with_reason(
                    None,
                    Some(crate::protocol::events::TurnStopReason::Cancelled),
                );
            }
        });
        Ok(())
    }

    fn kill(&self, reason: Option<AgentKillReason>) -> Result<(), AppError> {
        info!(
            conversation_id = %self.runtime.conversation_id(),
            ?reason,
            "Killing remote OpenClaw agent"
        );
        let connection = Arc::clone(&self.connection);
        tokio::spawn(async move {
            connection.close().await;
        });
        if reason == Some(AgentKillReason::UserCancelled) {
            self.runtime.emit_finish_with_reason(
                None,
                Some(crate::protocol::events::TurnStopReason::Cancelled),
            );
        } else if self.runtime.status() == Some(ConversationStatus::Running) {
            self.runtime
                .emit_error(format!("Remote OpenClaw agent was terminated ({reason:?})"));
        }
        Ok(())
    }
}

impl RemoteAgentManager {
    pub fn kill_and_wait(
        &self,
        reason: Option<AgentKillReason>,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>> {
        info!(
            conversation_id = %self.runtime.conversation_id(),
            ?reason,
            "Killing remote OpenClaw agent and waiting for connection close"
        );
        let connection = Arc::clone(&self.connection);
        Box::pin(async move {
            connection.close().await;
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
}

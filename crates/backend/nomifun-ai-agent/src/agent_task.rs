//! Minimal public contract for a running agent task.
//!
//! `IAgentTask` captures **only** the operations that every agent type
//! implements identically and that the generic task_manager / idle_scanner /
//! message-flow code actually needs. Anything that is type-specific
//! (session modes, session keys, model switching, config options, pending
//! confirmation lists, approval memory, ACP usage, OpenClaw diagnostics,
//! etc.) lives as **inherent** methods on each concrete `XxxAgentManager`
//! and is reached through the `AgentInstance` enum — forcing every callsite
//! to say out loud which agent type it is addressing.
//!
//! Replaces the old bloated `IAgentManager` trait + `as_any()` downcast
//! pattern (deleted in PR #8c).
use std::sync::Arc;

use nomifun_common::{AgentKillReason, AgentType, AppError, ConversationStatus, TimestampMs};
use tokio::sync::broadcast;

use crate::manager::acp::AcpAgentManager;
use crate::manager::nanobot::NanobotAgentManager;
use crate::manager::nomi::NomiAgentManager;
use crate::manager::openclaw::OpenClawAgentManager;
use crate::manager::remote::RemoteAgentManager;
use crate::protocol::events::AgentStreamEvent;
use crate::protocol::send_error::AgentSendError;
use crate::types::SendMessageData;

use nomifun_api_types::{
    GetModelInfoResponse, ModelInfoEntry, ModelInfoPayload, SideQuestionRequest, SideQuestionResponse, SlashCommandItem,
};

#[cfg(any(test, feature = "test-support"))]
use nomifun_common::Confirmation;

/// Ten-method public surface every agent type implements identically.
///
/// Object-safe by construction (no generic methods, no `Self` by value).
/// Used by generic lifecycle code (task_manager, idle_scanner, stream
/// fan-out) that genuinely does not care which agent type it is dealing
/// with. For type-specific operations, match on [`AgentInstance`] and
/// call the concrete manager's inherent methods.
#[async_trait::async_trait]
pub trait IAgentTask: Send + Sync {
    /// The type of agent this task controls.
    fn agent_type(&self) -> AgentType;

    /// Conversation ID this task is bound to.
    fn conversation_id(&self) -> &str;

    /// Working directory for this agent session.
    fn workspace(&self) -> &str;

    /// Current conversation status. `None` if the agent has not
    /// transitioned into a known status yet.
    fn status(&self) -> Option<ConversationStatus>;

    /// Timestamp (ms) of the last activity (message send, event received).
    fn last_activity_at(&self) -> TimestampMs;

    /// Subscribe to the agent's stream event channel.
    fn subscribe(&self) -> broadcast::Receiver<AgentStreamEvent>;

    /// Send a user message to the agent. Returns once the agent has
    /// accepted the turn; actual streaming proceeds on the broadcast
    /// channel returned by [`Self::subscribe`].
    async fn send_message(&self, data: SendMessageData) -> Result<(), AgentSendError>;

    /// Stop the current streaming response without killing the agent.
    async fn cancel(&self) -> Result<(), AppError>;

    /// Terminate the agent process.
    ///
    /// - `reason: Some(IdleTimeout)` — idle cleanup
    /// - `reason: None` — explicit user/system kill
    fn kill(&self, reason: Option<AgentKillReason>) -> Result<(), AppError>;
}

/// Extended trait used exclusively by the `AgentInstance::Mock` variant so
/// tests can inject richer fake behaviour (pending confirmations, approval
/// memory, fake session keys, etc.) without polluting the production
/// `IAgentTask` contract with trait-level defaults that would be lies for
/// at least one concrete manager.
///
/// Every method has a sensible identity-style default so simple mocks only
/// need to implement the ten `IAgentTask` methods and pick up nothing for
/// free.
#[cfg(any(test, feature = "test-support"))]
#[async_trait::async_trait]
pub trait IMockAgent: IAgentTask {
    fn get_confirmations(&self) -> Vec<Confirmation> {
        Vec::new()
    }
    fn check_approval(&self, _action: &str, _command_type: Option<&str>) -> bool {
        false
    }
    /// Mid-turn steering. Mirrors `AgentInstance::steer`: `Ok(true)` = queued
    /// into a live turn, `Ok(false)` = no live turn (caller sends normally).
    /// Defaults to `Ok(false)` so simple mocks report "not steerable"; tests
    /// that exercise the steering path override this.
    fn steer(&self, _text: String) -> Result<bool, AppError> {
        Ok(false)
    }
    fn confirm(
        &self,
        _msg_id: &str,
        _call_id: &str,
        _data: serde_json::Value,
        _always_allow: bool,
    ) -> Result<(), AppError> {
        Ok(())
    }
    fn get_session_key(&self) -> Option<String> {
        None
    }
    async fn mode(&self) -> Result<nomifun_api_types::AgentModeResponse, AppError> {
        Ok(nomifun_api_types::AgentModeResponse {
            mode: "default".into(),
            initialized: false,
        })
    }
    async fn set_mode(&self, _mode: &str) -> Result<(), AppError> {
        Err(AppError::BadRequest(
            "Mode switching is not supported for this mock".into(),
        ))
    }
    async fn get_model(&self) -> Result<GetModelInfoResponse, AppError> {
        Ok(GetModelInfoResponse { model_info: None })
    }
    async fn set_model(&self, _model_id: &str) -> Result<(), AppError> {
        Err(AppError::BadRequest(
            "Model switching is not supported for this mock".into(),
        ))
    }
    async fn get_usage(&self) -> Result<Option<serde_json::Value>, AppError> {
        Ok(None)
    }
    async fn get_slash_commands(&self) -> Result<Vec<SlashCommandItem>, AppError> {
        Ok(Vec::new())
    }
    async fn handle_side_question(&self, _req: SideQuestionRequest) -> Result<SideQuestionResponse, AppError> {
        Ok(SideQuestionResponse {
            status: "unsupported".into(),
            answer: None,
        })
    }
    async fn get_openclaw_runtime(&self) -> Result<serde_json::Value, AppError> {
        Ok(serde_json::Value::Null)
    }
}

/// Concrete, closed-set dispatcher for the five agent variants.
///
/// Every generic path holds an `AgentInstance` (not `Arc<dyn IAgentTask>`):
/// this gives us the `IAgentTask` ten-method surface via [`Self::as_task`]
/// **and** lets type-specific routes recover the concrete manager with a
/// single `match` — no `as_any` / `downcast_ref` anywhere. Adding a new
/// agent type means adding a new variant here; every `match` in the
/// codebase then fails to compile until it explicitly handles the new
/// type, which is the compile-time pressure we want.
#[derive(Clone)]
pub enum AgentInstance {
    Acp(Arc<AcpAgentManager>),
    Nomi(Arc<NomiAgentManager>),
    OpenClaw(Arc<OpenClawAgentManager>),
    Nanobot(Arc<NanobotAgentManager>),
    Remote(Arc<RemoteAgentManager>),
    /// Test-only trait-object escape hatch used by downstream crates
    /// (conversation/cron/team/app tests) to inject fake agents without
    /// spinning up a real CLI or WebSocket connection. Gated behind
    /// `#[cfg(any(test, feature = "test-support"))]`: production builds
    /// never see this variant, so every `match` in release code can
    /// rely on the five-variant closed set. The trait object is
    /// [`IMockAgent`] (extends `IAgentTask`) so mocks can also override
    /// the enum-level helpers — `get_confirmations`, `check_approval`,
    /// `confirm`, `get_session_key`, `get_mode`, `set_mode`.
    #[cfg(any(test, feature = "test-support"))]
    Mock(Arc<dyn IMockAgent>),
}

impl AgentInstance {
    /// Common `IAgentTask` view, regardless of variant.
    pub fn as_task(&self) -> &dyn IAgentTask {
        match self {
            Self::Acp(m) => m.as_ref(),
            Self::Nomi(m) => m.as_ref(),
            Self::OpenClaw(m) => m.as_ref(),
            Self::Nanobot(m) => m.as_ref(),
            Self::Remote(m) => m.as_ref(),
            #[cfg(any(test, feature = "test-support"))]
            Self::Mock(m) => m.as_ref(),
        }
    }

    // ── Convenience forwarders ───────────────────────────────────────
    //
    // These stay in the final API (not a migration crutch): they turn
    // `instance.agent_type()` into a direct vtable-free call on the
    // concrete `Arc<XxxManager>`, and they keep callsites terse.

    /// The type of agent this instance controls.
    pub fn agent_type(&self) -> AgentType {
        self.as_task().agent_type()
    }

    /// Conversation ID this task is bound to.
    pub fn conversation_id(&self) -> &str {
        self.as_task().conversation_id()
    }

    /// Working directory for this agent session.
    pub fn workspace(&self) -> &str {
        self.as_task().workspace()
    }

    /// Current conversation status.
    pub fn status(&self) -> Option<ConversationStatus> {
        self.as_task().status()
    }

    /// Timestamp (ms) of the last activity.
    pub fn last_activity_at(&self) -> TimestampMs {
        self.as_task().last_activity_at()
    }

    /// Subscribe to the stream event channel.
    pub fn subscribe(&self) -> broadcast::Receiver<AgentStreamEvent> {
        self.as_task().subscribe()
    }

    /// Send a user message to the agent.
    pub async fn send_message(&self, data: SendMessageData) -> Result<(), AgentSendError> {
        self.as_task().send_message(data).await
    }

    /// Cancel the current streaming response without killing the agent.
    pub async fn cancel(&self) -> Result<(), AppError> {
        self.as_task().cancel().await
    }

    /// Terminate the agent process.
    pub fn kill(&self, reason: Option<AgentKillReason>) -> Result<(), AppError> {
        self.as_task().kill(reason)
    }

    /// Terminate the agent process and return a future that resolves when the
    /// underlying OS process has exited.
    pub fn kill_and_wait(
        &self,
        reason: Option<AgentKillReason>,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>> {
        match self {
            Self::Acp(m) => m.kill_and_wait(reason),
            Self::OpenClaw(m) => m.kill_and_wait(reason),
            Self::Nanobot(m) => m.kill_and_wait(reason),
            Self::Nomi(m) => m.kill_and_wait(reason),
            Self::Remote(m) => m.kill_and_wait(reason),
            #[cfg(any(test, feature = "test-support"))]
            Self::Mock(_) => Box::pin(std::future::ready(())),
        }
    }

    // ── Cross-variant semi-specific helpers ──────────────────────────
    //
    // These fan out to inherent methods on concrete managers. Variants
    // that don't support the operation return a sensible zero-value
    // rather than an error: "no pending confirmations" and "no session
    // key" are honest statements about those variants.

    /// Pending confirmation items for this task.
    ///
    /// ACP surfaces pending permission prompts through its permission
    /// router. Nomi / OpenClaw / Remote maintain inline confirmation lists.
    /// Nanobot has no concept of confirmations.
    pub fn get_confirmations(&self) -> Vec<nomifun_common::Confirmation> {
        match self {
            Self::Acp(m) => m.get_confirmations(),
            Self::Nomi(m) => m.get_confirmations(),
            Self::OpenClaw(m) => m.get_confirmations(),
            Self::Nanobot(_) => Vec::new(),
            Self::Remote(m) => m.get_confirmations(),
            #[cfg(any(test, feature = "test-support"))]
            Self::Mock(m) => m.get_confirmations(),
        }
    }

    /// Submit a confirmation response for a pending tool call.
    pub fn confirm(
        &self,
        msg_id: &str,
        call_id: &str,
        data: serde_json::Value,
        always_allow: bool,
    ) -> Result<(), AppError> {
        match self {
            Self::Acp(m) => m.confirm(msg_id, call_id, data, always_allow),
            Self::Nomi(m) => m.confirm(msg_id, call_id, data, always_allow),
            Self::OpenClaw(m) => m.confirm(msg_id, call_id, data, always_allow),
            Self::Nanobot(m) => m.confirm(msg_id, call_id, data, always_allow),
            Self::Remote(m) => m.confirm(msg_id, call_id, data, always_allow),
            #[cfg(any(test, feature = "test-support"))]
            Self::Mock(m) => m.confirm(msg_id, call_id, data, always_allow),
        }
    }

    /// Check whether an action is auto-approved in this session.
    pub fn check_approval(&self, action: &str, command_type: Option<&str>) -> bool {
        match self {
            Self::Acp(_) => false,
            Self::Nomi(m) => m.check_approval(action, command_type),
            Self::OpenClaw(m) => m.check_approval(action, command_type),
            Self::Nanobot(_) => false,
            Self::Remote(m) => m.check_approval(action, command_type),
            #[cfg(any(test, feature = "test-support"))]
            Self::Mock(m) => m.check_approval(action, command_type),
        }
    }

    /// Session key for agent types that expose one (currently OpenClaw).
    pub fn get_session_key(&self) -> Option<String> {
        match self {
            Self::OpenClaw(m) => m.get_session_key(),
            Self::Acp(_) | Self::Nomi(_) | Self::Nanobot(_) | Self::Remote(_) => None,
            #[cfg(any(test, feature = "test-support"))]
            Self::Mock(m) => m.get_session_key(),
        }
    }

    /// Get the current session mode. Only ACP and Nomi model a mode;
    /// other variants report `mode = "default"`, `initialized = false`
    /// so cron / UI can skip mode reconciliation.
    pub async fn get_mode(&self) -> Result<nomifun_api_types::AgentModeResponse, AppError> {
        match self {
            Self::Acp(m) => m.mode().await,
            Self::Nomi(m) => m.mode().await,
            Self::OpenClaw(_) | Self::Nanobot(_) | Self::Remote(_) => Ok(nomifun_api_types::AgentModeResponse {
                mode: "default".into(),
                initialized: false,
            }),
            #[cfg(any(test, feature = "test-support"))]
            Self::Mock(m) => m.mode().await,
        }
    }

    /// Set the session mode. Unsupported for variants other than ACP /
    /// Nomi — returns a `BadRequest` so the caller can surface an
    /// actionable error rather than silently no-op.
    pub async fn set_mode(&self, mode: &str) -> Result<(), AppError> {
        match self {
            Self::Acp(m) => m.set_mode(mode).await,
            Self::Nomi(m) => m.set_mode(mode).await,
            Self::OpenClaw(_) | Self::Nanobot(_) | Self::Remote(_) => Err(AppError::BadRequest(
                "Mode switching is not supported for this agent type".into(),
            )),
            #[cfg(any(test, feature = "test-support"))]
            Self::Mock(m) => m.set_mode(mode).await,
        }
    }

    /// Clear the conversation context ("release model context") in place,
    /// keeping the agent/process alive. ACP rotates to a fresh `session/new`;
    /// Nomi empties its engine history; OpenClaw / Remote forget their gateway
    /// session key so the next send re-creates a clean session. Nanobot has no
    /// resumable session and returns a `BadRequest` the caller can surface.
    pub async fn clear_context(&self) -> Result<(), AppError> {
        match self {
            Self::Acp(m) => m.clear_context().await,
            Self::Nomi(m) => m.clear_context().await,
            Self::OpenClaw(m) => m.clear_context().await,
            Self::Remote(m) => m.clear_context().await,
            Self::Nanobot(_) => Err(AppError::BadRequest(
                "Clear context is not supported for this agent type".into(),
            )),
            #[cfg(any(test, feature = "test-support"))]
            Self::Mock(_) => Ok(()),
        }
    }

    /// Push a mid-turn steering interjection into the running turn. Only the
    /// Nomi native engine can inject mid-turn; every other variant is an
    /// external process that cannot be steered, so they return a `BadRequest`
    /// the service maps to `steer_unsupported` (client falls back to the
    /// pending queue). `Ok(true)` = queued into a live turn; `Ok(false)` = no
    /// turn running (caller should send normally).
    pub fn steer(&self, text: String) -> Result<bool, AppError> {
        match self {
            Self::Nomi(m) => m.steer(text),
            Self::Acp(_) | Self::OpenClaw(_) | Self::Nanobot(_) | Self::Remote(_) => Err(
                AppError::BadRequest("Steering is not supported for this agent type".into()),
            ),
            #[cfg(any(test, feature = "test-support"))]
            Self::Mock(m) => m.steer(text),
        }
    }

    /// Rewind the last user turn (edit & resubmit the most recent user message).
    /// Only the Nomi native engine can rewind its in-memory transcript; every
    /// other variant is an external process whose context cannot be rewound, so
    /// they return a `BadRequest` (the frontend never exposes the entry for
    /// non-Nomi conversations).
    pub async fn rewind_last_turn(&self) -> Result<(), AppError> {
        match self {
            Self::Nomi(m) => m.rewind_last_turn().await,
            Self::Acp(_) | Self::OpenClaw(_) | Self::Nanobot(_) | Self::Remote(_) => Err(
                AppError::BadRequest("Edit & resubmit is not supported for this agent type".into()),
            ),
            #[cfg(any(test, feature = "test-support"))]
            Self::Mock(_) => Ok(()),
        }
    }

    /// Get the current session model info. Only ACP exposes a model
    /// catalog; other variants report `model_info = None` so the UI can
    /// hide the model picker without an error.
    pub async fn get_model(&self) -> Result<GetModelInfoResponse, AppError> {
        match self {
            Self::Acp(m) => {
                let sdk_model = m.model().await;
                let sdk_info = sdk_model.map(map_sdk_model_to_payload);
                let cc_switch_info = if m.is_claude_backend() {
                    crate::cc_switch::read_claude_model_info()
                } else {
                    None
                };
                let model_info = merge_model_info(sdk_info, cc_switch_info);
                Ok(GetModelInfoResponse { model_info })
            }
            Self::Nomi(_) | Self::OpenClaw(_) | Self::Nanobot(_) | Self::Remote(_) => {
                Ok(GetModelInfoResponse { model_info: None })
            }
            #[cfg(any(test, feature = "test-support"))]
            Self::Mock(m) => m.get_model().await,
        }
    }

    /// Switch the active model. Unsupported for variants other than ACP —
    /// returns a `BadRequest` so the caller can surface an actionable
    /// error rather than silently no-op.
    pub async fn set_model(&self, model_id: &str) -> Result<(), AppError> {
        if model_id.trim().is_empty() {
            return Err(AppError::BadRequest("model_id must not be empty".into()));
        }
        match self {
            Self::Acp(m) => m.set_model(model_id).await,
            Self::Nomi(_) | Self::OpenClaw(_) | Self::Nanobot(_) | Self::Remote(_) => Err(AppError::BadRequest(
                "Model switching is not supported for this agent type".into(),
            )),
            #[cfg(any(test, feature = "test-support"))]
            Self::Mock(m) => m.set_model(model_id).await,
        }
    }

    /// Returns the cached session usage as a snake_case JSON object. The
    /// structure mirrors the ACP SDK `UsageUpdate` schema
    /// (`used` / `size` / `cost` / `_meta`), normalised via
    /// [`nomifun_common::normalize_keys_to_snake_case`] so keys land as
    /// `used` / `size` / `cost` to match the Nomi wire convention —
    /// `_meta` passes through verbatim.
    ///
    /// Non-ACP agents return `None`.
    pub async fn get_usage(&self) -> Result<Option<serde_json::Value>, AppError> {
        match self {
            Self::Acp(m) => {
                let Some(usage) = m.usage().await else { return Ok(None) };
                let mut value = serde_json::to_value(usage)
                    .map_err(|e| AppError::Internal(format!("Failed to serialize usage: {e}")))?;
                nomifun_common::normalize_keys_to_snake_case(&mut value);
                Ok(Some(value))
            }
            Self::Nomi(_) | Self::OpenClaw(_) | Self::Nanobot(_) | Self::Remote(_) => Ok(None),
            #[cfg(any(test, feature = "test-support"))]
            Self::Mock(m) => m.get_usage().await,
        }
    }

    /// Slash commands available in the current session. Only ACP exposes
    /// a slash-command catalog; other variants report an empty list
    /// (the UI renders "no commands").
    pub async fn get_slash_commands(&self) -> Result<Vec<SlashCommandItem>, AppError> {
        match self {
            Self::Acp(m) => m.load_slash_commands().await,
            Self::Nomi(m) => m.get_slash_commands().await,
            Self::OpenClaw(_) | Self::Nanobot(_) | Self::Remote(_) => Ok(Vec::new()),
            #[cfg(any(test, feature = "test-support"))]
            Self::Mock(m) => m.get_slash_commands().await,
        }
    }

    /// Dispatch a side-question to the agent. **Placeholder** — matches
    /// the current `AgentService::handle_side_question` behaviour: ACP
    /// agents whose behavior_policy enables side-questions return a stub
    /// "ok" response, everyone else returns `unsupported`.
    pub async fn handle_side_question(&self, req: SideQuestionRequest) -> Result<SideQuestionResponse, AppError> {
        if req.question.trim().is_empty() {
            return Err(AppError::BadRequest("question must not be empty".into()));
        }
        match self {
            Self::Acp(m) => {
                if !m.supports_side_question() {
                    return Ok(SideQuestionResponse {
                        status: "unsupported".into(),
                        answer: None,
                    });
                }
                Ok(SideQuestionResponse {
                    status: "ok".into(),
                    answer: Some("Side question support will be fully wired in app integration phase.".into()),
                })
            }
            Self::Nomi(_) | Self::OpenClaw(_) | Self::Nanobot(_) | Self::Remote(_) => Ok(SideQuestionResponse {
                status: "unsupported".into(),
                answer: None,
            }),
            #[cfg(any(test, feature = "test-support"))]
            Self::Mock(m) => m.handle_side_question(req).await,
        }
    }

    /// OpenClaw-specific runtime diagnostics. Only OpenClaw reports
    /// diagnostics; other variants report `Value::Null` so diagnostic
    /// UIs degrade gracefully.
    pub async fn get_openclaw_runtime(&self) -> Result<serde_json::Value, AppError> {
        match self {
            Self::OpenClaw(m) => Ok(m.get_diagnostics().await),
            Self::Acp(_) | Self::Nomi(_) | Self::Nanobot(_) | Self::Remote(_) => Ok(serde_json::Value::Null),
            #[cfg(any(test, feature = "test-support"))]
            Self::Mock(m) => m.get_openclaw_runtime().await,
        }
    }
}

/// Map the raw ACP SDK model state into the public API payload.
///
/// Kept private to this module: the only caller is
/// [`AgentInstance::get_model`]. Mirrors the helper formerly living in
/// `services/agent.rs`; do not duplicate — if the shape of
/// `ModelInfoPayload` changes, update it here.
fn map_sdk_model_to_payload(m: agent_client_protocol::schema::SessionModelState) -> ModelInfoPayload {
    let available: Vec<ModelInfoEntry> = m
        .available_models
        .iter()
        .map(|am| ModelInfoEntry {
            id: am.model_id.to_string(),
            label: am.name.clone(),
        })
        .collect();
    let current_id = m.current_model_id.to_string();
    let current_label = available
        .iter()
        .find(|e| e.id == current_id)
        .map(|e| e.label.clone())
        .unwrap_or_else(|| current_id.clone());
    ModelInfoPayload {
        current_model_id: Some(current_id),
        current_model_label: Some(current_label),
        available_models: available,
    }
}

fn merge_model_info(
    sdk_info: Option<ModelInfoPayload>,
    cc_switch_info: Option<ModelInfoPayload>,
) -> Option<ModelInfoPayload> {
    sdk_info.or(cc_switch_info)
}

#[cfg(test)]
mod cc_switch_model_merge_tests {
    use super::*;

    #[test]
    fn merge_prefers_sdk_model_over_cc_switch() {
        let sdk_payload = ModelInfoPayload {
            current_model_id: Some("default".into()),
            current_model_label: Some("Claude Sonnet 4.6".into()),
            available_models: vec![ModelInfoEntry {
                id: "default".into(),
                label: "Claude Sonnet 4.6".into(),
            }],
        };
        let cc_switch_payload = ModelInfoPayload {
            current_model_id: Some("default".into()),
            current_model_label: Some("DeepSeek V4".into()),
            available_models: vec![ModelInfoEntry {
                id: "default".into(),
                label: "DeepSeek V4".into(),
            }],
        };

        let result = merge_model_info(Some(sdk_payload), Some(cc_switch_payload));
        assert_eq!(
            result.unwrap().current_model_label.as_deref(),
            Some("Claude Sonnet 4.6")
        );
    }

    #[test]
    fn merge_falls_back_to_cc_switch_when_sdk_none() {
        let cc_switch_payload = ModelInfoPayload {
            current_model_id: Some("default".into()),
            current_model_label: Some("DeepSeek V4".into()),
            available_models: vec![ModelInfoEntry {
                id: "default".into(),
                label: "DeepSeek V4".into(),
            }],
        };

        let result = merge_model_info(None, Some(cc_switch_payload));
        assert_eq!(result.unwrap().current_model_label.as_deref(), Some("DeepSeek V4"));
    }

    #[test]
    fn merge_returns_none_when_both_none() {
        let result = merge_model_info(None, None);
        assert!(result.is_none());
    }
}

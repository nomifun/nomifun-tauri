use std::sync::Arc;

use nomifun_ai_agent::{AgentStreamEvent, IWorkerTaskManager};
use nomifun_api_types::{
    ConversationRuntimeStateKind, CreateConversationRequest, ListMessagesQuery, MessageResponse, SendMessageRequest,
};
use nomifun_common::{AgentType, ConversationSource, MessagePosition, MessageType};
use nomifun_conversation::ConversationService;
use nomifun_db::IChannelRepository;
use nomifun_db::models::AssistantSessionRow;
use tokio::sync::broadcast;
use tracing::{debug, info, warn};

use crate::channel_settings::{ChannelSettingsService, resolved_model_to_provider};
use crate::constants::{STREAM_THROTTLE_INTERVAL, TOOL_CONFIRM_TIMEOUT};
use crate::error::ChannelError;
use crate::types::{OutgoingMessageType, PluginType, UnifiedOutgoingMessage};

/// Profile of the desktop's master agent (the companion). The channel layer
/// resolves which companion greets a session via the channel row's own `companion_id`
/// binding first, falling back to this profile (`master_companion_id`: legacy
/// per-platform binding only — no default-companion fallback). `companion_model` is the **primary** model
/// source for a master-mode nomi session bound to a companion (唯一事实源
/// `profile.model`); the platform `defaultModel` is only a legacy fallback
/// when the companion has no model.
/// Implemented in `nomifun-app` over `CompanionService` + `ChannelSettingsService`
/// so the channel crate stays companion-agnostic.
#[async_trait::async_trait]
pub trait MasterAgentProfile: Send + Sync {
    /// The configured model of `companion_id`, `None` when the companion does not
    /// exist or its model is not configured.
    async fn companion_model(&self, companion_id: &str) -> Option<nomifun_common::ProviderWithModel>;
    /// The legacy per-platform fallback companion for `platform` (e.g. "telegram"):
    /// the platform binding when set (and alive), else `None`. There is **no
    /// default-companion fallback** — an unbound channel is hosted by no companion.
    async fn master_companion_id(&self, platform: &str) -> Option<String>;
    /// Whether `companion_id` names a live companion. Used to validate companion-binding
    /// writes and to skip dead channel bindings.
    async fn companion_exists(&self, companion_id: &str) -> bool;

    /// Display name of `companion_id`, `None` when it does not exist. Used only to
    /// render a friendlier "already bound to companion …" error (name over raw id).
    /// Default `None` keeps companion-only / test impls unaffected.
    async fn companion_name(&self, _companion_id: &str) -> Option<String> {
        None
    }

    /// Idempotently resolve (create-or-get) the companion's ONE persistent
    /// session conversation id. This is what unifies a companion's IM-channel
    /// turns into the SAME session the desktop bubble and chat tab use, instead
    /// of minting a separate per-chat channel-master conversation. Returns
    /// `None` when the companion cannot host a session yet (e.g. its chat model
    /// is not configured) — the caller then refuses the turn with a notice
    /// rather than leaking a standalone conversation.
    async fn ensure_companion_session(&self, companion_id: &str) -> Option<i64>;

    // ── 对外伙伴 / public agent (a platform bot serves EITHER a companion OR a
    //    public agent, never both) ──────────────────────────────────────────

    /// Whether public agent `id` is SERVABLE right now: it names a LIVE, ENABLED
    /// agent. A stale binding (deleted agent) or a disabled/paused agent resolves
    /// to `false` so the channel layer refuses the turn with a notice rather than
    /// serving a dead agent. The bot→agent binding itself is per-bot (the channel
    /// row's `public_agent_id`), so this is a pure by-id liveness check. Default
    /// `false` keeps companion-only / test impls unaffected.
    async fn public_agent_servable(&self, _id: &str) -> bool {
        false
    }

    /// Whether `id` names a live public agent (regardless of enabled/model
    /// state). Used to validate public-agent-binding writes against the roster,
    /// mirroring [`Self::companion_exists`]. Default `false` (no public-agent
    /// domain wired) — the binding route then rejects unknown ids.
    async fn public_agent_exists(&self, _id: &str) -> bool {
        false
    }

    /// Display name of public agent `id`, `None` when it does not exist. Used only
    /// to render a friendlier "already bound to public agent …" error. Default `None`.
    async fn public_agent_name(&self, _id: &str) -> Option<String> {
        None
    }

    /// The configured answering model of public agent `id`, `None` when the agent
    /// does not exist or its model is not configured. Used by the channel layer to
    /// pick the conversation model for a public-agent session. Default `None`.
    async fn public_agent_model(&self, _id: &str) -> Option<nomifun_common::ProviderWithModel> {
        None
    }

    /// Best-effort audit hook for an inbound public-agent turn. Called once per
    /// inbound turn routed into a public agent's per-chat session. Default no-op
    /// so non-audit impls (tests) are unaffected. Must never fail the turn.
    async fn record_public_agent_turn(&self, _id: &str, _platform: &str, _text: &str) {}
}

/// Resolves a workshop asset id (`wsa_…`) to raw bytes for outbound media.
///
/// Defined here (not in `nomifun-workshop`) so `nomifun-channel` stays free of
/// a workshop dependency; the concrete impl lives in `nomifun-app`
/// (`channel_asset_resolver.rs`), mirroring [`MasterAgentProfile`].
#[async_trait::async_trait]
pub trait AssetResolver: Send + Sync {
    /// Load `asset_id` as bytes + mime + a suggested filename. `None` when the
    /// asset can't be found or read (the relay then simply skips that image).
    async fn resolve(&self, asset_id: &str) -> Option<crate::types::OutgoingMedia>;
}

/// Bridges channel messages to the conversation + AI agent layer.
///
/// Responsibilities:
/// - Creating conversations for channel sessions
/// - Sending user messages to the AI agent
/// - Receiving stream events and converting them to outgoing messages
/// - Throttling editMessage calls for streaming responses
/// - Handling tool confirmation with timeout
pub struct ChannelMessageService {
    conversation_svc: Arc<ConversationService>,
    task_manager: Arc<dyn IWorkerTaskManager>,
    settings: Arc<ChannelSettingsService>,
    repo: Arc<dyn IChannelRepository>,
    owner_user_id: String,
    master_profile: Option<Arc<dyn MasterAgentProfile>>,
    /// Per-conversation store of the decision currently awaiting a numbered
    /// reply. Shared with each `ChannelStreamRelay` (writer) so the inbound
    /// reply can be resolved against the right `call_id`/option.
    pending_decisions: Arc<crate::pending_decision::PendingDecisionStore>,
    /// Optional resolver turning `wsa_…` ids into raw bytes for outbound media.
    /// `None` (default / tests) disables channel image sending gracefully.
    asset_resolver: Option<Arc<dyn AssetResolver>>,
}

impl ChannelMessageService {
    pub fn new(
        conversation_svc: Arc<ConversationService>,
        task_manager: Arc<dyn IWorkerTaskManager>,
        settings: Arc<ChannelSettingsService>,
        repo: Arc<dyn IChannelRepository>,
        owner_user_id: String,
    ) -> Self {
        Self {
            conversation_svc,
            task_manager,
            settings,
            repo,
            owner_user_id,
            master_profile: None,
            pending_decisions: crate::pending_decision::PendingDecisionStore::new(),
            asset_resolver: None,
        }
    }

    /// Wire the master-agent profile (the companion). Without it, master mode still
    /// grants the desktop gateway but cannot fall back to the companion's model.
    pub fn with_master_profile(mut self, profile: Arc<dyn MasterAgentProfile>) -> Self {
        self.master_profile = Some(profile);
        self
    }

    /// Wire the asset resolver so channel replies can send AI-generated images.
    /// Without it, image sending is disabled (text-only behaviour, unchanged).
    pub fn with_asset_resolver(mut self, resolver: Arc<dyn AssetResolver>) -> Self {
        self.asset_resolver = Some(resolver);
        self
    }

    /// The wired asset resolver, if any. The orchestrator hands this to each
    /// `ChannelStreamRelay` it spawns.
    pub fn asset_resolver(&self) -> Option<Arc<dyn AssetResolver>> {
        self.asset_resolver.clone()
    }

    /// The shared pending-decision store. The orchestrator hands this to each
    /// `ChannelStreamRelay` it spawns and reads it back when intercepting a
    /// numeric reply, so the relay (writer) and the orchestrator (reader) act
    /// on the same store.
    pub fn pending_decisions(&self) -> Arc<crate::pending_decision::PendingDecisionStore> {
        Arc::clone(&self.pending_decisions)
    }

    /// Whether the conversation's agent is currently working on a turn.
    ///
    /// Used by the orchestrator as a per-chat concurrency guard: a new
    /// channel message for a busy conversation is answered with a "still
    /// processing" notice instead of being queued as a second prompt.
    pub async fn is_conversation_busy(&self, conversation_id: &str) -> bool {
        let summary = self.conversation_svc.runtime_summary_for(conversation_id).await;
        matches!(
            summary.state,
            ConversationRuntimeStateKind::Starting | ConversationRuntimeStateKind::Running
        )
    }

    /// Submits a numbered-decision choice back through the confirm chain.
    ///
    /// `option_id` is sent as the bare `data` string accepted by
    /// `ConversationService::confirm` for ACP (`msg_id` is ignored there).
    /// `always_allow` is `false` — a numbered reply approves this one decision
    /// only, never a standing grant.
    pub async fn submit_decision(
        &self,
        conversation_id: &str,
        call_id: &str,
        option_id: &str,
    ) -> Result<(), ChannelError> {
        let req = nomifun_api_types::ConfirmRequest {
            msg_id: String::new(),
            data: serde_json::Value::String(option_id.to_owned()),
            always_allow: false,
        };
        self.conversation_svc
            .confirm(&self.owner_user_id, conversation_id, call_id, req, &self.task_manager)
            .await
            .map_err(|e| ChannelError::MessageSendFailed(e.to_string()))
    }

    /// Returns the most recent visible user message text of a conversation,
    /// used by `chat.regenerate` to resend the last prompt.
    ///
    /// Reads a single newest-first page; user turns alternate with assistant
    /// output, so 50 rows is far more than enough to reach the latest one.
    pub async fn last_user_text(&self, conversation_id: &str) -> Result<Option<String>, ChannelError> {
        let query = ListMessagesQuery {
            page: Some(1),
            page_size: Some(50),
            order: Some("DESC".into()),
            content_mode: None,
            cursor: None,
        };
        let result = self
            .conversation_svc
            .list_messages(&self.owner_user_id, conversation_id, query)
            .await
            .map_err(|e| ChannelError::MessageSendFailed(e.to_string()))?;
        Ok(extract_last_user_text(&result.items))
    }

    /// Sends a text message from a channel user to the AI agent.
    ///
    /// 1. Ensures the session has a backing conversation (creates one if needed)
    /// 2. Warms up the backing agent task so stream subscription is available
    /// 3. Sends the message via ConversationService
    /// 4. Returns the conversation_id and stream receiver for relay
    ///
    /// The caller is responsible for subscribing to stream events and
    /// relaying them to the IM platform.
    pub async fn send_to_agent(
        &self,
        session: &AssistantSessionRow,
        text: &str,
        platform: PluginType,
    ) -> Result<SendResult, ChannelError> {
        // 对外伙伴 (public agent) binding takes precedence over EVERYTHING. A
        // bot bound to a public agent serves strangers via an isolated per-chat,
        // PublicService-clamped nomi session — never a companion, and never the
        // desktop gateway. Resolved FIRST and PER-BOT (from the channel row's own
        // `public_agent_id`, via session.channel_id) so a companion-bound bot on
        // the same platform is unaffected: precedence is per-bot, and the hard
        // clamp is the boundary.
        if let Some(channel_id) = session.channel_id.as_deref()
            && let Some(row) = self.repo.get_plugin(channel_id).await?
            && let Some(pa_id) = row.public_agent_id.filter(|s| !s.trim().is_empty())
        {
            return self.send_to_public_agent(session, text, platform, &pa_id).await;
        }

        // Resolve the target conversation. A nomi channel turn bound to a
        // companion is routed into that companion's ONE persistent session, so
        // the desktop bubble, the chat tab, and every IM chat share a single
        // transcript (no more separate per-chat channel-master conversation
        // leaking into the homepage work list). Non-companion / ACP / unbound
        // channels keep a standalone per-session conversation. The FK is i64
        // (Option A); downstream services are string-keyed, so keep a String.
        let agent_type = parse_agent_type(&session.agent_type);
        let companion_id = if agent_type == AgentType::Nomi {
            self.resolve_session_companion(session, platform).await
        } else {
            None
        };
        let conversation_id = if let Some(cid) = companion_id.as_deref() {
            match self.master_profile.as_ref() {
                Some(profile) => match profile.ensure_companion_session(cid).await {
                    Some(id) => id.to_string(),
                    // Companion bound but no chat model → can't open its single
                    // session. Refuse with a notice instead of silently minting a
                    // leaking standalone channel conversation (reintroducing the bug).
                    None => {
                        return Err(ChannelError::CompanionNotReady(
                            "这个伙伴还没有配置对话模型，请先在桌面端为它选择模型后再聊天。".into(),
                        ));
                    }
                },
                None => {
                    return Err(ChannelError::MessageSendFailed(
                        "master agent profile not configured".into(),
                    ));
                }
            }
        } else {
            match &session.conversation_id {
                Some(cid) => cid.to_string(),
                None => self.create_conversation_for_session(session, platform).await?,
            }
        };

        // Tag this turn with its origin platform ONLY when it rides a
        // companion's shared single session (companion_id resolved): that
        // conversation row carries no `channelPlatform`, so the per-turn marker
        // is what lets the floating window render it as a remote IM turn.
        // Standalone channel conversations keep their extra-derived marker
        // (marker None → send_message falls back to extra).
        let channel_platform = companion_id.as_ref().map(|_| platform.to_string());
        self.dispatch_to_conversation(&session.id, conversation_id, text, channel_platform)
            .await
    }

    /// Routes an inbound turn on a 对外伙伴 (public-agent) bound platform into an
    /// isolated per-chat, `PublicService`-clamped nomi session. NEVER a companion
    /// and NEVER the desktop gateway — the session's `extra.public_agent_id` is
    /// what hard-clamps it to safe tools in the nomi factory, so a stranger can be
    /// auto-served safely.
    async fn send_to_public_agent(
        &self,
        session: &AssistantSessionRow,
        text: &str,
        platform: PluginType,
        public_agent_id: &str,
    ) -> Result<SendResult, ChannelError> {
        let profile = self.master_profile.as_ref().ok_or_else(|| {
            ChannelError::MessageSendFailed("master agent profile not configured".into())
        })?;

        // Servable = bound + alive + enabled. A disabled/missing public agent
        // refuses the turn with a friendly notice; we do NOT fall through to a
        // companion (the whole point of the binding is data isolation). The bind
        // is per-bot, so this is a pure by-id liveness check.
        if !profile.public_agent_servable(public_agent_id).await {
            return Err(ChannelError::CompanionNotReady(
                "这个对外服务当前不可用，请稍后再试。".into(),
            ));
        }

        // Per-chat isolation: reuse the session's bound conversation, or mint a
        // fresh per-chat public-agent conversation (NOT a shared companion session).
        let conversation_id = match &session.conversation_id {
            Some(cid) => cid.to_string(),
            None => {
                self.create_public_agent_conversation(session, platform, public_agent_id)
                    .await?
            }
        };

        // The public-agent conversation carries `channelPlatform` in its own extra,
        // so no per-turn marker is needed (None).
        let result = self
            .dispatch_to_conversation(&session.id, conversation_id, text, None)
            .await?;

        // Best-effort audit of the inbound turn (records only for the public agent;
        // never fails the turn).
        profile
            .record_public_agent_turn(public_agent_id, &platform.to_string(), text)
            .await;

        Ok(result)
    }

    /// Warms the conversation's agent, subscribes to its stream, and sends the
    /// user turn. Shared by the companion / standalone path and the public-agent
    /// path — the only per-path difference is the `channel_platform` per-turn
    /// marker (companion shared session ⇒ platform; public-agent / standalone ⇒
    /// None, the marker rides the conversation extra).
    async fn dispatch_to_conversation(
        &self,
        session_id: &str,
        conversation_id: String,
        text: &str,
        channel_platform: Option<String>,
    ) -> Result<SendResult, ChannelError> {
        // `msg_id` is server-generated inside the service; channel plugins that
        // need to correlate the user message back to the conversation should use
        // `conversation_id` + stream events instead of a client-provided id.
        let req = SendMessageRequest {
            content: text.to_owned(),
            files: vec![],
            inject_skills: vec![],
            hidden: false,
            origin: None,
            channel_platform,
        };

        let user_id = &self.owner_user_id;
        // Channel relays need a stream subscription before the agent starts
        // emitting. `ConversationService::send_message` returns immediately
        // and builds cold agents in the background, so warm the conversation
        // explicitly for channel traffic.
        self.conversation_svc
            .warmup(user_id, &conversation_id, &self.task_manager)
            .await
            .map_err(|e| ChannelError::MessageSendFailed(e.to_string()))?;

        let stream_rx = self
            .task_manager
            .get_task(&conversation_id)
            .map(|handle| handle.subscribe())
            .ok_or_else(|| {
                ChannelError::MessageSendFailed(format!(
                    "Agent task missing after warmup for conversation {conversation_id}"
                ))
            })?;

        self.conversation_svc
            .send_message(user_id, &conversation_id, req, &self.task_manager)
            .await
            .map_err(|e| match e {
                // A concurrent turn already holds the (now shared) session —
                // surface a distinct busy error so the orchestrator answers with
                // the friendly "still processing" notice instead of a raw failure
                // line. Covers the first-turn race the per-chat busy guard can't
                // see (it checks the pre-bind session id).
                nomifun_common::AppError::Conflict(_) => ChannelError::ConversationBusy,
                other => ChannelError::MessageSendFailed(other.to_string()),
            })?;

        info!(
            conversation_id = %conversation_id,
            session_id = %session_id,
            has_stream = true,
            "message sent to agent"
        );

        Ok(SendResult {
            conversation_id,
            stream_rx: Some(stream_rx),
        })
    }

    /// Creates a new conversation for a channel session.
    ///
    /// Sets `source` to the appropriate platform and `channel_chat_id`
    /// for per-chat isolation.
    async fn create_conversation_for_session(
        &self,
        session: &AssistantSessionRow,
        platform: PluginType,
    ) -> Result<String, ChannelError> {
        let source = platform_to_source(platform);
        let agent_type = parse_agent_type(&session.agent_type);

        let agent_config = self.settings.get_agent_config(platform).await?;
        let model_config = self.settings.get_model_config(platform).await?;

        // The companion greeting this session. Resolution order: the channel
        // row's own companion binding (per-bot, the multi-bot path) > the legacy
        // per-platform binding. NO default-companion fallback — an unbound channel
        // is hosted by no companion. Recorded in extra.companionId so the
        // persona/memory layers and gateway tools know which companion owns the
        // session. Nomi engine only. Every channel session is a companion master
        // session now — the former "Master Agent mode" on/off toggle was removed;
        // all companions control the desktop with full capabilities, there is no
        // "plain standalone session" path.
        let master_companion_id = if agent_type == AgentType::Nomi {
            self.resolve_session_companion(session, platform).await
        } else {
            None
        };

        // 模型解析顺序（绑定伙伴的 nomi 会话）：
        // 绑定伙伴的 profile.model 为主（唯一事实源）> 平台 defaultModel（遗留兜底）> 空。
        // 伙伴模型只作用于 nomi 引擎——ACP CLI 自带模型配置，仍走平台 defaultModel。
        let mut model = if agent_type == AgentType::Nomi
            && let Some(profile) = self.master_profile.as_ref()
            && let Some(companion_id) = master_companion_id.as_deref()
            && let Some(companion_model) = profile.companion_model(companion_id).await
        {
            companion_model
        } else {
            resolved_model_to_provider(model_config.as_ref())
        };

        // 遗留兜底：伙伴模型缺失（伙伴未配置或非 nomi/无绑定）时，
        // 回退到平台 defaultModel，保持非伙伴会话的既有行为。
        if model.provider_id.is_empty() {
            model = resolved_model_to_provider(model_config.as_ref());
        }

        let mut extra = Self::build_channel_extra(agent_config.backend.as_deref());
        apply_master_agent_extra(&mut extra, agent_type, platform, master_companion_id.as_deref());
        let name = channel_conversation_name(
            platform,
            &session.agent_type,
            agent_config.backend.as_deref(),
            session.chat_id.as_deref(),
        );

        // Top-level `model` is only accepted for nomi; other types pass via `extra`.
        let top_level_model = if agent_type == AgentType::Nomi {
            Some(model)
        } else {
            extra["model"] = serde_json::to_value(&model).unwrap_or_default();
            None
        };

        let req = CreateConversationRequest {
            r#type: agent_type,
            name: Some(name),
            model: top_level_model,
            source: Some(source),
            channel_chat_id: session.chat_id.clone(),
            extra,
        };

        let response = self
            .conversation_svc
            .create(&self.owner_user_id, req)
            .await
            .map_err(|e| ChannelError::MessageSendFailed(e.to_string()))?;

        debug!(
            conversation_id = %response.id,
            session_id = %session.id,
            "conversation created for channel session"
        );

        // Response id is i64; this helper returns a String id (Option A).
        Ok(response.id.to_string())
    }

    /// Creates a fresh per-chat conversation for a 对外伙伴 (public-agent) session.
    ///
    /// A public-agent session is ALWAYS a nomi conversation (the `PublicService`
    /// hard clamp lives in the nomi factory and keys off `extra.public_agent_id`),
    /// regardless of the platform's configured agent type. The extra carries
    /// `public_agent_id` + `channelPlatform` but deliberately NO `companionId` and
    /// NO `desktopGateway` — public agents get no gateway. The answering model
    /// comes from the public agent's own config.
    async fn create_public_agent_conversation(
        &self,
        session: &AssistantSessionRow,
        platform: PluginType,
        public_agent_id: &str,
    ) -> Result<String, ChannelError> {
        let profile = self.master_profile.as_ref().ok_or_else(|| {
            ChannelError::MessageSendFailed("master agent profile not configured".into())
        })?;

        // Resolve the answering model through the SINGLE authority
        // `public_agent_model`: the agent's OWN configured model wins, else the
        // app's default (first enabled provider + model, resolved from the
        // provider catalog). It returns `None` ONLY when THIS running instance
        // has no enabled model provider at all — so the error is truthful about
        // the one remaining cause and points at 模型管理, instead of misleading
        // the owner into thinking they must configure THIS agent's model (a fresh
        // agent answers as soon as any provider exists — no per-agent setup).
        //
        // NOTE on diagnosis: if this fires while the owner "clearly configured a
        // model", the provider lives in a DIFFERENT running instance/data-dir
        // (e.g. installed Nomi vs Nomi-dev) than the one serving this channel —
        // the catalog this turn reads is genuinely empty. The message says so.
        let model = profile.public_agent_model(public_agent_id).await.ok_or_else(|| {
            ChannelError::CompanionNotReady(
                "本机尚未启用任何对话模型，对外伙伴无法作答。请在桌面端「模型管理」中启用一个模型服务商后再试——对外伙伴会自动使用默认模型；也可在「对外服务」→ 选择该伙伴 →「身份 & 话术」→「对话模型」中为它单独指定。".into(),
            )
        })?;

        let extra = Self::build_public_agent_extra(platform, public_agent_id);
        let name = channel_conversation_name(platform, "nomi", None, session.chat_id.as_deref());

        let req = CreateConversationRequest {
            r#type: AgentType::Nomi,
            name: Some(name),
            // Public agents are nomi only → the model rides the top-level field.
            model: Some(model),
            source: Some(platform_to_source(platform)),
            channel_chat_id: session.chat_id.clone(),
            extra,
        };

        let response = self
            .conversation_svc
            .create(&self.owner_user_id, req)
            .await
            .map_err(|e| ChannelError::MessageSendFailed(e.to_string()))?;

        debug!(
            conversation_id = %response.id,
            session_id = %session.id,
            public_agent_id = %public_agent_id,
            "public-agent conversation created for channel session"
        );

        Ok(response.id.to_string())
    }

    /// Resolves which companion greets a channel session.
    ///
    /// The channel row's `companion_id` wins (each bot is bound to its own companion);
    /// a dead binding degrades to the profile fallback instead of pinning
    /// sessions to a ghost. Without a channel binding, the profile resolves
    /// the legacy per-platform binding only (no default-companion fallback).
    async fn resolve_session_companion(&self, session: &AssistantSessionRow, platform: PluginType) -> Option<String> {
        let profile = self.master_profile.as_ref()?;

        if let Some(channel_id) = session.channel_id.as_deref() {
            match self.repo.get_plugin(channel_id).await {
                Ok(Some(row)) => {
                    if let Some(companion_id) = row.companion_id.filter(|p| !p.trim().is_empty()) {
                        if profile.companion_exists(&companion_id).await {
                            return Some(companion_id);
                        }
                        warn!(
                            channel_id = %channel_id,
                            companion_id = %companion_id,
                            "channel companion binding names a missing companion; falling back"
                        );
                    }
                }
                Ok(None) => {}
                Err(e) => warn!(channel_id = %channel_id, error = %e, "failed to load channel row for companion resolution"),
            }
        }

        profile.master_companion_id(&platform.to_string()).await
    }

    /// Processes a stream event from the AI agent and converts it to
    /// an optional outgoing message for the IM platform.
    ///
    /// Returns `None` for events that don't need to be sent to the user
    /// (e.g., internal status updates, thinking traces).
    pub fn process_stream_event(event: &AgentStreamEvent) -> Option<StreamAction> {
        match event {
            AgentStreamEvent::Text(data) => Some(StreamAction::AppendText(data.content.clone())),
            AgentStreamEvent::Finish(_) => Some(StreamAction::Finish),
            AgentStreamEvent::Error(data) => Some(StreamAction::Error(data.message.clone())),
            AgentStreamEvent::Thinking(data) => Some(StreamAction::Thinking(data.content.clone())),
            AgentStreamEvent::ToolCall(data) => {
                // A completed tool call may carry produced workshop asset ids in
                // its output JSON (nomi_workshop_get_task/generate `result_asset_ids`).
                // Surface those as MediaProduced so the relay can send the picture;
                // otherwise keep the cosmetic {name,status} progress update.
                if matches!(
                    data.status,
                    nomifun_ai_agent::protocol::events::ToolCallStatus::Completed
                ) && let Some(output) = data.output.as_deref()
                {
                    let ids = crate::media_refs::asset_ids_from_tool_output(output);
                    if !ids.is_empty() {
                        return Some(StreamAction::MediaProduced(ids));
                    }
                }
                Some(StreamAction::ToolCall {
                    name: data.name.clone(),
                    status: format!("{:?}", data.status),
                })
            }
            // Blocking decisions: forward as a numbered text choice. A decision
            // with no options is unanswerable, so it is dropped (None).
            AgentStreamEvent::AcpPermission(data) => match data {
                nomifun_ai_agent::protocol::events::AcpPermissionEventData::Request(req) => {
                    let options: Vec<crate::types::DecisionOption> = req
                        .options
                        .iter()
                        .map(|o| crate::types::DecisionOption {
                            option_id: o.option_id.clone(),
                            label: o.name.clone(),
                        })
                        .collect();
                    if options.is_empty() {
                        return None;
                    }
                    Some(StreamAction::Decision {
                        call_id: req.tool_call.tool_call_id.clone(),
                        prompt: req
                            .tool_call
                            .title
                            .clone()
                            .unwrap_or_else(|| "请选择".to_owned()),
                        options,
                    })
                }
                nomifun_ai_agent::protocol::events::AcpPermissionEventData::Confirmation(conf) => {
                    confirmation_to_decision(conf)
                }
            },
            AgentStreamEvent::Permission(value) => serde_json::from_value::<nomifun_common::Confirmation>(value.clone())
                .ok()
                .and_then(|conf| confirmation_to_decision(&conf)),
            // Events that don't produce user-facing messages
            AgentStreamEvent::Start(_)
            | AgentStreamEvent::Tips(_)
            | AgentStreamEvent::ToolGroup(_)
            | AgentStreamEvent::AgentStatus(_)
            | AgentStreamEvent::Plan(_)
            | AgentStreamEvent::AcpToolCall(_)
            | AgentStreamEvent::AvailableCommands(_)
            | AgentStreamEvent::SkillSuggest(_)
            | AgentStreamEvent::CronTrigger(_)
            | AgentStreamEvent::AcpModelInfo(_)
            | AgentStreamEvent::AcpModeInfo(_)
            | AgentStreamEvent::AcpConfigOption(_)
            | AgentStreamEvent::AcpSessionInfo(_)
            | AgentStreamEvent::AcpContextUsage(_)
            | AgentStreamEvent::AcpPromptHookWarning(_)
            | AgentStreamEvent::TurnCompleted(_)
            | AgentStreamEvent::System(_)
            | AgentStreamEvent::RequestTrace(_)
            | AgentStreamEvent::SlashCommandsUpdated(_)
            | AgentStreamEvent::SessionAssigned(_) => None,
        }
    }

    /// Builds the "thinking" placeholder message sent immediately after
    /// receiving a user message, before the AI starts streaming.
    pub fn build_thinking_message() -> UnifiedOutgoingMessage {
        UnifiedOutgoingMessage {
            message_type: OutgoingMessageType::Text,
            text: Some("\u{23f3} Thinking...".into()),
            parse_mode: None,
            buttons: None,
            keyboard: None,
            image_url: None,
            file_url: None,
            file_name: None,
            media_actions: None,
            reply_to_message_id: None,
            silent: None,
        }
    }

    /// Builds the final message after streaming completes.
    ///
    /// The `Buttons` message type is retained purely as the "final turn" marker
    /// that channels key off to finalize a streaming card (e.g. DingTalk flips
    /// its AI Card to FINISHED on this type). No action buttons are attached:
    /// the Regenerate / Continue / New Session affordances were removed because
    /// they cluttered the reply and hurt readability across IM channels.
    pub fn build_final_message(text: &str) -> UnifiedOutgoingMessage {
        UnifiedOutgoingMessage {
            message_type: OutgoingMessageType::Buttons,
            text: Some(text.to_owned()),
            parse_mode: None,
            buttons: None,
            keyboard: None,
            image_url: None,
            file_url: None,
            file_name: None,
            media_actions: None,
            reply_to_message_id: None,
            silent: None,
        }
    }

    /// Builds an intermediate streaming message (for editMessage calls).
    pub fn build_streaming_message(text: &str) -> UnifiedOutgoingMessage {
        UnifiedOutgoingMessage {
            message_type: OutgoingMessageType::Text,
            text: Some(text.to_owned()),
            parse_mode: None,
            buttons: None,
            keyboard: None,
            image_url: None,
            file_url: None,
            file_name: None,
            media_actions: None,
            reply_to_message_id: None,
            silent: None,
        }
    }

    /// Builds the numbered-text rendering of a blocking decision.
    ///
    /// Portable across channels (no card-button dependency): the prompt, a
    /// numbered list of option labels, and an instruction to reply with the
    /// number. Plain `Text` with no buttons.
    pub fn build_decision_message(prompt: &str, options: &[crate::types::DecisionOption]) -> UnifiedOutgoingMessage {
        let mut text = format!("\u{26a0}\u{fe0f} 需要你的决策：\n{prompt}\n");
        for (idx, option) in options.iter().enumerate() {
            text.push_str(&format!("{}. {}\n", idx + 1, option.label));
        }
        text.push_str("回复编号选择（如 1）");

        UnifiedOutgoingMessage {
            message_type: OutgoingMessageType::Text,
            text: Some(text),
            parse_mode: None,
            buttons: None,
            keyboard: None,
            image_url: None,
            file_url: None,
            file_name: None,
            media_actions: None,
            reply_to_message_id: None,
            silent: None,
        }
    }

    /// Returns the stream throttle interval for editMessage calls.
    pub fn throttle_interval() -> std::time::Duration {
        STREAM_THROTTLE_INTERVAL
    }

    /// Returns the tool confirmation timeout duration.
    pub fn confirm_timeout() -> std::time::Duration {
        TOOL_CONFIRM_TIMEOUT
    }

    /// Build the `extra` JSON for channel conversations.
    ///
    /// Sets `session_mode` to `"yolo"` so the agent auto-approves tool calls —
    /// channel users have no interactive UI for confirmations.
    pub fn build_channel_extra(backend: Option<&str>) -> serde_json::Value {
        let mut extra = serde_json::json!({
            "session_mode": "yolo",
        });
        if let Some(b) = backend {
            extra["backend"] = serde_json::Value::String(b.to_owned());
        }
        extra
    }

    /// Build the `extra` JSON for a 对外伙伴 (public-agent) channel conversation.
    ///
    /// `session_mode: "yolo"` (no interactive confirmations on a channel), plus
    /// `public_agent_id` (the nomi factory keys the `PublicService` hard clamp off
    /// this) and `channelPlatform` (persona remote-context framing). Deliberately
    /// carries NO `companionId` and NO `desktopGateway` — public agents get no
    /// gateway; the clamp turns off gateway / computer / browser / spawn.
    pub fn build_public_agent_extra(platform: PluginType, public_agent_id: &str) -> serde_json::Value {
        let mut extra = Self::build_channel_extra(None);
        extra["public_agent_id"] = serde_json::Value::String(public_agent_id.to_owned());
        extra["channelPlatform"] = serde_json::Value::String(platform.to_string());
        extra
    }
}

/// Result of sending a message to the agent.
#[derive(Debug)]
pub struct SendResult {
    pub conversation_id: String,
    /// Agent event stream for the ChannelStreamRelay.
    /// `None` when the agent task could not be found after sending
    /// (should not happen in normal flow).
    pub stream_rx: Option<broadcast::Receiver<AgentStreamEvent>>,
}

/// Actions derived from agent stream events.
#[derive(Debug, Clone)]
pub enum StreamAction {
    /// Append text content to the current response.
    AppendText(String),
    /// Streaming finished.
    Finish,
    /// An error occurred.
    Error(String),
    /// Agent is thinking/reasoning.
    Thinking(String),
    /// Tool call status update.
    ToolCall { name: String, status: String },
    /// A blocking decision (permission / confirmation) the channel user must
    /// answer. Carried so the relay can forward a numbered list and the
    /// orchestrator can map a numeric reply back to `confirm`.
    Decision {
        call_id: String,
        prompt: String,
        options: Vec<crate::types::DecisionOption>,
    },
    /// One or more workshop asset ids produced by a *completed* tool call
    /// (e.g. `nomi_workshop_get_task` `result_asset_ids`). The relay resolves
    /// each to bytes and sends it as media after the final text.
    MediaProduced(Vec<String>),
}

/// Decorate a channel conversation's extra so the session is a companion master
/// session: it gets the Desktop Gateway tools (`extra.desktopGateway`, every
/// agent type), and on the nomi engine additionally the companion semantics —
/// persona system prompt (built fresh per agent build by the factory's
/// `CompanionPromptProvider`) + memory tools (`extra.companionSession`), with the
/// platform recorded for the persona's remote-context framing and the bound
/// companion pinned in `extra.companionId` (per-bot binding > legacy platform binding;
/// key omitted when no companion is bound — the session then has no companion persona).
///
/// Unconditional: EVERY channel session is a companion master session. The former
/// "Master Agent mode" on/off toggle (and its "plain standalone session" path) was
/// removed — all companions control the desktop with full capabilities.
fn apply_master_agent_extra(
    extra: &mut serde_json::Value,
    agent_type: AgentType,
    platform: PluginType,
    companion_id: Option<&str>,
) {
    extra["desktopGateway"] = serde_json::Value::Bool(true);
    if agent_type == AgentType::Nomi {
        extra["companionSession"] = serde_json::Value::Bool(true);
        extra["channelPlatform"] = serde_json::Value::String(platform.to_string());
        if let Some(pid) = companion_id.map(str::trim).filter(|s| !s.is_empty()) {
            extra["companionId"] = serde_json::Value::String(pid.to_owned());
        }
    }
}

/// Maps a `nomifun_common::Confirmation` to a `Decision` action.
///
/// Option values become option ids (ACP `confirm` accepts a bare option-id
/// string). A confirmation with no options is unanswerable and yields `None`.
fn confirmation_to_decision(conf: &nomifun_common::Confirmation) -> Option<StreamAction> {
    let options: Vec<crate::types::DecisionOption> = conf
        .options
        .iter()
        .map(|o| crate::types::DecisionOption {
            option_id: option_value_to_string(&o.value),
            label: o.label.clone(),
        })
        .collect();
    if options.is_empty() {
        return None;
    }
    Some(StreamAction::Decision {
        call_id: conf.call_id.clone(),
        prompt: conf.title.clone().unwrap_or_else(|| conf.description.clone()),
        options,
    })
}

/// Renders a confirmation option value as the option id string to submit
/// back through `confirm`. String values pass through verbatim; other JSON
/// values fall back to their compact serialization.
fn option_value_to_string(value: &serde_json::Value) -> String {
    value
        .as_str()
        .map(str::to_owned)
        .unwrap_or_else(|| value.to_string())
}

/// Picks the newest visible user-authored text from a newest-first message
/// page. User messages are persisted as `type: "text"`, `position: "right"`
/// with content `{"content": "..."}` (see `ConversationService::send_message`),
/// so this is the inverse of that write path.
fn extract_last_user_text(items: &[MessageResponse]) -> Option<String> {
    items
        .iter()
        .filter(|m| !m.hidden && m.r#type == MessageType::Text && m.position == Some(MessagePosition::Right))
        .find_map(|m| {
            m.content
                .get("content")
                .and_then(|v| v.as_str())
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_owned)
        })
}

/// Maps a PluginType to the corresponding ConversationSource.
fn platform_to_source(platform: PluginType) -> ConversationSource {
    match platform {
        PluginType::Telegram => ConversationSource::Telegram,
        PluginType::Lark => ConversationSource::Lark,
        PluginType::Dingtalk => ConversationSource::Dingtalk,
        PluginType::Weixin => ConversationSource::Weixin,
        // Reserved / new outbound channels default to Nomi source until a
        // dedicated ConversationSource variant is added per channel phase.
        PluginType::Slack
        | PluginType::Discord
        | PluginType::Matrix
        | PluginType::Mattermost
        | PluginType::Twitch
        | PluginType::Nostr
        | PluginType::Wecom
        | PluginType::Qqbot => ConversationSource::Nomifun,
    }
}

/// Parses an agent_type string to an AgentType enum.
///
/// Falls back to `AgentType::Acp` for unknown values.
fn parse_agent_type(s: &str) -> AgentType {
    match s {
        "acp" => AgentType::Acp,
        "openclaw-gateway" => AgentType::OpenclawGateway,
        "nanobot" => AgentType::Nanobot,
        "remote" => AgentType::Remote,
        "nomi" => AgentType::Nomi,
        _ => {
            warn!(agent_type = %s, "unknown agent type, defaulting to Acp");
            AgentType::Acp
        }
    }
}

fn channel_conversation_name(
    platform: PluginType,
    agent_type: &str,
    backend: Option<&str>,
    chat_id: Option<&str>,
) -> String {
    let short = match platform {
        PluginType::Telegram => "tg",
        PluginType::Lark => "lark",
        PluginType::Dingtalk => "ding",
        PluginType::Weixin => "wx",
        PluginType::Wecom => "wecom",
        PluginType::Slack => "slack",
        PluginType::Discord => "discord",
        PluginType::Matrix => "matrix",
        PluginType::Mattermost => "mm",
        PluginType::Twitch => "twitch",
        PluginType::Nostr => "nostr",
        PluginType::Qqbot => "qq",
    };

    let mut parts = vec![short.to_owned()];
    if !agent_type.is_empty() {
        parts.push(agent_type.to_owned());
    }
    if agent_type == "acp"
        && let Some(b) = backend
    {
        parts.push(b.to_owned());
    }
    if let Some(cid) = chat_id {
        let end = cid.len().min(8);
        parts.push(cid[..end].to_owned());
    }
    parts.join("-")
}

#[cfg(test)]
mod tests {
    use super::*;
    use nomifun_ai_agent::protocol::events::{
        ErrorEventData, FinishEventData, StartEventData, TextEventData, ThinkingEventData, ToolCallEventData,
        ToolCallStatus,
    };
    use nomifun_common::ProviderWithModel;

    // ── extract_last_user_text ────────────────────────────────────────

    fn make_history_message(
        id: &str,
        msg_type: MessageType,
        position: Option<MessagePosition>,
        content: serde_json::Value,
        hidden: bool,
    ) -> MessageResponse {
        MessageResponse {
            id: id.into(),
            conversation_id: 1,
            msg_id: Some(id.into()),
            r#type: msg_type,
            content,
            position,
            status: None,
            hidden,
            created_at: 0,
        }
    }

    #[test]
    fn extract_picks_newest_user_text_from_desc_page() {
        // Newest-first page: assistant reply, then the user prompt that
        // produced it, then an older user prompt.
        let items = vec![
            make_history_message(
                "m3",
                MessageType::Text,
                Some(MessagePosition::Left),
                serde_json::json!({ "content": "assistant says hi" }),
                false,
            ),
            make_history_message(
                "m2",
                MessageType::Text,
                Some(MessagePosition::Right),
                serde_json::json!({ "content": "newest user prompt" }),
                false,
            ),
            make_history_message(
                "m1",
                MessageType::Text,
                Some(MessagePosition::Right),
                serde_json::json!({ "content": "older user prompt" }),
                false,
            ),
        ];
        assert_eq!(extract_last_user_text(&items).as_deref(), Some("newest user prompt"));
    }

    #[test]
    fn extract_skips_hidden_and_non_text_messages() {
        let items = vec![
            make_history_message(
                "m3",
                MessageType::ToolCall,
                Some(MessagePosition::Right),
                serde_json::json!({ "content": "tool payload" }),
                false,
            ),
            make_history_message(
                "m2",
                MessageType::Text,
                Some(MessagePosition::Right),
                serde_json::json!({ "content": "hidden prompt" }),
                true,
            ),
            make_history_message(
                "m1",
                MessageType::Text,
                Some(MessagePosition::Right),
                serde_json::json!({ "content": "visible prompt" }),
                false,
            ),
        ];
        assert_eq!(extract_last_user_text(&items).as_deref(), Some("visible prompt"));
    }

    #[test]
    fn extract_returns_none_without_user_messages() {
        let items = vec![make_history_message(
            "m1",
            MessageType::Text,
            Some(MessagePosition::Left),
            serde_json::json!({ "content": "assistant only" }),
            false,
        )];
        assert_eq!(extract_last_user_text(&items), None);
        assert_eq!(extract_last_user_text(&[]), None);
    }

    #[test]
    fn extract_skips_blank_content() {
        let items = vec![
            make_history_message(
                "m2",
                MessageType::Text,
                Some(MessagePosition::Right),
                serde_json::json!({ "content": "   " }),
                false,
            ),
            make_history_message(
                "m1",
                MessageType::Text,
                Some(MessagePosition::Right),
                serde_json::json!({ "content": "real prompt" }),
                false,
            ),
        ];
        assert_eq!(extract_last_user_text(&items).as_deref(), Some("real prompt"));
    }

    // ── platform_to_source ─────────────────────────────────────────────

    #[test]
    fn platform_to_source_telegram() {
        assert_eq!(platform_to_source(PluginType::Telegram), ConversationSource::Telegram);
    }

    // ── apply_master_agent_extra ───────────────────────────────────────

    #[test]
    fn master_extra_nomi_gets_gateway_and_companion() {
        let mut extra = ChannelMessageService::build_channel_extra(None);
        apply_master_agent_extra(&mut extra, AgentType::Nomi, PluginType::Telegram, Some("companion_1"));
        assert_eq!(extra["desktopGateway"], serde_json::json!(true));
        assert_eq!(extra["companionSession"], serde_json::json!(true));
        assert_eq!(extra["channelPlatform"], serde_json::json!("telegram"));
        assert_eq!(extra["companionId"], serde_json::json!("companion_1"));
        // Existing channel semantics survive.
        assert_eq!(extra["session_mode"], serde_json::json!("yolo"));
    }

    #[test]
    fn master_extra_nomi_without_companion_omits_companion_id_key() {
        let mut extra = ChannelMessageService::build_channel_extra(None);
        apply_master_agent_extra(&mut extra, AgentType::Nomi, PluginType::Telegram, None);
        assert_eq!(extra["companionSession"], serde_json::json!(true));
        assert!(extra.get("companionId").is_none(), "no companion → no companionId key");

        // Blank companion id is treated the same as no companion.
        let mut extra = ChannelMessageService::build_channel_extra(None);
        apply_master_agent_extra(&mut extra, AgentType::Nomi, PluginType::Telegram, Some("  "));
        assert!(extra.get("companionId").is_none());
    }

    #[test]
    fn master_extra_acp_gets_gateway_only() {
        let mut extra = ChannelMessageService::build_channel_extra(Some("claude"));
        apply_master_agent_extra(&mut extra, AgentType::Acp, PluginType::Lark, Some("companion_1"));
        assert_eq!(extra["desktopGateway"], serde_json::json!(true));
        assert!(extra.get("companionSession").is_none());
        assert!(extra.get("channelPlatform").is_none());
        assert!(extra.get("companionId").is_none());
        assert_eq!(extra["backend"], serde_json::json!("claude"));
    }

    // ── build_public_agent_extra ───────────────────────────────────────

    #[test]
    fn public_agent_extra_marks_public_and_has_no_gateway_or_companion() {
        let extra = ChannelMessageService::build_public_agent_extra(PluginType::Telegram, "pubagent_1");
        // Public-agent marker + platform present.
        assert_eq!(extra["public_agent_id"], serde_json::json!("pubagent_1"));
        assert_eq!(extra["channelPlatform"], serde_json::json!("telegram"));
        // Yolo channel semantics preserved.
        assert_eq!(extra["session_mode"], serde_json::json!("yolo"));
        // NEVER a gateway, NEVER a companion — public agents get no gateway.
        assert!(extra.get("desktopGateway").is_none(), "public agent must not get the gateway");
        assert!(extra.get("companionId").is_none(), "public agent must not carry a companion");
        assert!(extra.get("companionSession").is_none());
    }

    #[test]
    fn platform_to_source_lark() {
        assert_eq!(platform_to_source(PluginType::Lark), ConversationSource::Lark);
    }

    #[test]
    fn platform_to_source_dingtalk() {
        assert_eq!(platform_to_source(PluginType::Dingtalk), ConversationSource::Dingtalk);
    }

    #[test]
    fn platform_to_source_weixin() {
        assert_eq!(platform_to_source(PluginType::Weixin), ConversationSource::Weixin);
    }

    #[test]
    fn platform_to_source_reserved_defaults_to_nomifun() {
        assert_eq!(platform_to_source(PluginType::Slack), ConversationSource::Nomifun);
        assert_eq!(platform_to_source(PluginType::Discord), ConversationSource::Nomifun);
    }

    // ── parse_agent_type ───────────────────────────────────────────────

    #[test]
    fn parse_known_agent_types() {
        assert_eq!(parse_agent_type("acp"), AgentType::Acp);
        assert_eq!(parse_agent_type("openclaw-gateway"), AgentType::OpenclawGateway);
        assert_eq!(parse_agent_type("nanobot"), AgentType::Nanobot);
        assert_eq!(parse_agent_type("remote"), AgentType::Remote);
        assert_eq!(parse_agent_type("nomi"), AgentType::Nomi);
    }

    #[test]
    fn parse_unknown_agent_type_defaults_to_acp() {
        assert_eq!(parse_agent_type("unknown"), AgentType::Acp);
        assert_eq!(parse_agent_type(""), AgentType::Acp);
    }

    // ── process_stream_event ───────────────────────────────────────────

    #[test]
    fn text_event_produces_append() {
        let event = AgentStreamEvent::Text(TextEventData {
            content: "Hello".into(),
        });
        let action = ChannelMessageService::process_stream_event(&event);
        match action {
            Some(StreamAction::AppendText(text)) => assert_eq!(text, "Hello"),
            _ => panic!("Expected AppendText"),
        }
    }

    #[test]
    fn finish_event_produces_finish() {
        let event = AgentStreamEvent::Finish(FinishEventData { session_id: None, stop_reason: None });
        let action = ChannelMessageService::process_stream_event(&event);
        assert!(matches!(action, Some(StreamAction::Finish)));
    }

    #[test]
    fn error_event_produces_error() {
        let event = AgentStreamEvent::Error(ErrorEventData::legacy("timeout", None));
        let action = ChannelMessageService::process_stream_event(&event);
        match action {
            Some(StreamAction::Error(msg)) => assert_eq!(msg, "timeout"),
            _ => panic!("Expected Error"),
        }
    }

    #[test]
    fn thinking_event_produces_thinking() {
        let event = AgentStreamEvent::Thinking(ThinkingEventData {
            content: "Analyzing...".into(),
            subject: None,
            duration: None,
            status: None,
        });
        let action = ChannelMessageService::process_stream_event(&event);
        match action {
            Some(StreamAction::Thinking(text)) => assert_eq!(text, "Analyzing..."),
            _ => panic!("Expected Thinking"),
        }
    }

    #[test]
    fn tool_call_event_produces_tool_call() {
        let event = AgentStreamEvent::ToolCall(ToolCallEventData {
            call_id: "c1".into(),
            name: "read_file".into(),
            args: serde_json::Value::Null,
            status: ToolCallStatus::Running,
            description: None,
            input: None,
            output: None,
        });
        let action = ChannelMessageService::process_stream_event(&event);
        match action {
            Some(StreamAction::ToolCall { name, status }) => {
                assert_eq!(name, "read_file");
                assert_eq!(status, "Running");
            }
            _ => panic!("Expected ToolCall"),
        }
    }

    #[test]
    fn start_event_produces_none() {
        let event = AgentStreamEvent::Start(StartEventData { session_id: None });
        assert!(ChannelMessageService::process_stream_event(&event).is_none());
    }

    #[test]
    fn completed_workshop_tool_call_produces_media() {
        let event = AgentStreamEvent::ToolCall(ToolCallEventData {
            call_id: "c1".into(),
            name: "nomi_workshop_get_task".into(),
            args: serde_json::Value::Null,
            status: ToolCallStatus::Completed,
            description: None,
            input: None,
            output: Some(r#"{"status":"succeeded","result_asset_ids":["wsa_1"]}"#.into()),
        });
        match ChannelMessageService::process_stream_event(&event) {
            Some(StreamAction::MediaProduced(ids)) => assert_eq!(ids, vec!["wsa_1"]),
            other => panic!("expected MediaProduced, got {other:?}"),
        }
    }

    #[test]
    fn running_tool_call_still_produces_tool_call_status() {
        let event = AgentStreamEvent::ToolCall(ToolCallEventData {
            call_id: "c1".into(),
            name: "nomi_workshop_generate".into(),
            args: serde_json::Value::Null,
            status: ToolCallStatus::Running,
            description: None,
            input: None,
            output: None,
        });
        assert!(matches!(
            ChannelMessageService::process_stream_event(&event),
            Some(StreamAction::ToolCall { .. })
        ));
    }

    #[test]
    fn completed_tool_call_without_asset_ids_stays_tool_call() {
        let event = AgentStreamEvent::ToolCall(ToolCallEventData {
            call_id: "c1".into(),
            name: "Read".into(),
            args: serde_json::Value::Null,
            status: ToolCallStatus::Completed,
            description: None,
            input: None,
            output: Some("just some text output".into()),
        });
        assert!(matches!(
            ChannelMessageService::process_stream_event(&event),
            Some(StreamAction::ToolCall { .. })
        ));
    }

    // ── process_stream_event → Decision ────────────────────────────────

    #[test]
    fn acp_permission_request_produces_decision() {
        use nomifun_ai_agent::protocol::events::{
            AcpPermissionEventData, AcpPermissionOptionData, AcpPermissionOptionKind, AcpPermissionRequestData,
            AcpPermissionToolCall,
        };

        let event = AgentStreamEvent::AcpPermission(AcpPermissionEventData::Request(AcpPermissionRequestData {
            session_id: "s1".into(),
            tool_call: AcpPermissionToolCall {
                tool_call_id: "call-7".into(),
                status: None,
                title: Some("Run rm -rf?".into()),
                kind: None,
                raw_input: None,
                raw_output: None,
                content: None,
                locations: None,
                meta: None,
            },
            options: vec![
                AcpPermissionOptionData {
                    option_id: "allow".into(),
                    name: "Allow once".into(),
                    kind: AcpPermissionOptionKind::AllowOnce,
                    meta: None,
                },
                AcpPermissionOptionData {
                    option_id: "reject".into(),
                    name: "Reject".into(),
                    kind: AcpPermissionOptionKind::RejectOnce,
                    meta: None,
                },
            ],
            meta: None,
        }));

        match ChannelMessageService::process_stream_event(&event) {
            Some(StreamAction::Decision { call_id, prompt, options }) => {
                assert_eq!(call_id, "call-7");
                assert_eq!(prompt, "Run rm -rf?");
                assert_eq!(
                    options,
                    vec![
                        crate::types::DecisionOption {
                            option_id: "allow".into(),
                            label: "Allow once".into()
                        },
                        crate::types::DecisionOption {
                            option_id: "reject".into(),
                            label: "Reject".into()
                        },
                    ]
                );
            }
            other => panic!("expected Decision, got {other:?}"),
        }
    }

    #[test]
    fn permission_value_confirmation_produces_decision() {
        // Legacy untyped Permission carrying a serialized `Confirmation`.
        let value = serde_json::json!({
            "id": "conf-1",
            "call_id": "call-9",
            "title": "Edit file?",
            "action": null,
            "description": "edits main.rs",
            "command_type": "edit",
            "options": [
                { "label": "Yes", "value": "yes" },
                { "label": "No", "value": "no" },
            ],
        });
        let event = AgentStreamEvent::Permission(value);

        match ChannelMessageService::process_stream_event(&event) {
            Some(StreamAction::Decision { call_id, prompt, options }) => {
                assert_eq!(call_id, "call-9");
                assert_eq!(prompt, "Edit file?");
                assert_eq!(
                    options,
                    vec![
                        crate::types::DecisionOption {
                            option_id: "yes".into(),
                            label: "Yes".into()
                        },
                        crate::types::DecisionOption {
                            option_id: "no".into(),
                            label: "No".into()
                        },
                    ]
                );
            }
            other => panic!("expected Decision, got {other:?}"),
        }
    }

    #[test]
    fn permission_with_empty_options_produces_none() {
        let value = serde_json::json!({
            "id": "conf-2",
            "call_id": "call-10",
            "title": "No choices",
            "description": "",
            "options": [],
        });
        let event = AgentStreamEvent::Permission(value);
        assert!(
            ChannelMessageService::process_stream_event(&event).is_none(),
            "an unanswerable decision (no options) must not surface"
        );
    }


    // ── build_thinking_message ─────────────────────────────────────────

    #[test]
    fn thinking_message_has_text() {
        let msg = ChannelMessageService::build_thinking_message();
        assert_eq!(msg.message_type, OutgoingMessageType::Text);
        let text = msg.text.unwrap();
        assert!(text.contains("Thinking"));
    }

    // ── build_final_message ────────────────────────────────────────────

    #[test]
    fn final_message_is_marked_final_without_buttons() {
        let msg = ChannelMessageService::build_final_message("Response text");
        // `Buttons` type is kept as the "final turn" marker (channels key off it
        // to finalize a streaming card), but no action buttons are attached.
        assert_eq!(msg.message_type, OutgoingMessageType::Buttons);
        assert_eq!(msg.text.as_deref(), Some("Response text"));
        assert!(msg.buttons.is_none(), "final reply must carry no action buttons");
    }

    // ── build_streaming_message ────────────────────────────────────────

    #[test]
    fn streaming_message_is_plain_text() {
        let msg = ChannelMessageService::build_streaming_message("partial...");
        assert_eq!(msg.message_type, OutgoingMessageType::Text);
        assert_eq!(msg.text.as_deref(), Some("partial..."));
        assert!(msg.buttons.is_none());
    }

    // ── build_decision_message ─────────────────────────────────────────

    #[test]
    fn decision_message_is_numbered_plain_text() {
        let options = vec![
            crate::types::DecisionOption {
                option_id: "a".into(),
                label: "Allow".into(),
            },
            crate::types::DecisionOption {
                option_id: "b".into(),
                label: "Deny".into(),
            },
        ];
        let msg = ChannelMessageService::build_decision_message("Proceed?", &options);

        assert_eq!(msg.message_type, OutgoingMessageType::Text);
        assert!(msg.buttons.is_none(), "decision is plain text, no buttons");
        let text = msg.text.expect("decision message must carry text");
        assert!(text.contains("Proceed?"), "prompt rendered: {text}");
        assert!(text.contains("1. Allow"), "first option numbered: {text}");
        assert!(text.contains("2. Deny"), "second option numbered: {text}");
        assert!(text.contains("回复编号"), "reply-by-number hint present: {text}");
    }

    // ── throttle & timeout constants ───────────────────────────────────

    #[test]
    fn throttle_interval_is_500ms() {
        assert_eq!(
            ChannelMessageService::throttle_interval(),
            std::time::Duration::from_millis(500)
        );
    }

    #[test]
    fn confirm_timeout_is_15s() {
        assert_eq!(
            ChannelMessageService::confirm_timeout(),
            std::time::Duration::from_secs(15)
        );
    }

    // ── build_channel_extra ───────────────────────────────────────────

    #[test]
    fn yolo_extra_contains_session_mode() {
        let extra = ChannelMessageService::build_channel_extra(None);
        assert_eq!(extra["session_mode"], "yolo");
        assert!(extra.get("backend").is_none());
    }

    #[test]
    fn yolo_extra_with_backend() {
        let extra = ChannelMessageService::build_channel_extra(Some("claude"));
        assert_eq!(extra["session_mode"], "yolo");
        assert_eq!(extra["backend"], "claude");
    }

    // ── model placement by agent_type (regression: non-nomi must not
    //    use top-level model) ──────────────────────────────────────────

    #[test]
    fn acp_model_goes_into_extra_not_top_level() {
        let agent_type = AgentType::Acp;
        let model = ProviderWithModel {
            provider_id: "prov1".into(),
            model: "claude-sonnet".into(),
            use_model: Some("global.anthropic.claude-sonnet-4-6".into()),
        };
        let mut extra = ChannelMessageService::build_channel_extra(Some("codex"));

        let top_level_model = if agent_type == AgentType::Nomi {
            Some(model.clone())
        } else {
            extra["model"] = serde_json::to_value(&model).unwrap();
            None
        };

        assert!(top_level_model.is_none(), "acp must not have top-level model");
        assert_eq!(extra["model"]["provider_id"], "prov1");
        assert_eq!(extra["model"]["use_model"], "global.anthropic.claude-sonnet-4-6");
    }

    #[test]
    fn nomi_model_stays_at_top_level() {
        let agent_type = AgentType::Nomi;
        let model = ProviderWithModel {
            provider_id: "prov2".into(),
            model: "gpt-4o".into(),
            use_model: None,
        };
        let mut extra = ChannelMessageService::build_channel_extra(None);

        let top_level_model = if agent_type == AgentType::Nomi {
            Some(model.clone())
        } else {
            extra["model"] = serde_json::to_value(&model).unwrap();
            None
        };

        assert!(top_level_model.is_some(), "nomi must use top-level model");
        assert!(extra.get("model").is_none() || extra["model"].is_null());
    }

    // ── channel_conversation_name ─────────────────────────────────────

    #[test]
    fn conv_name_telegram_acp_with_backend() {
        let name = channel_conversation_name(PluginType::Telegram, "acp", Some("claude"), Some("70880480"));
        assert_eq!(name, "tg-acp-claude-70880480");
    }

    #[test]
    fn conv_name_telegram_nomi() {
        let name = channel_conversation_name(PluginType::Telegram, "nomi", None, Some("70880480"));
        assert_eq!(name, "tg-nomi-70880480");
    }

    #[test]
    fn conv_name_lark_acp_no_backend() {
        let name = channel_conversation_name(PluginType::Lark, "acp", None, Some("abcdef12"));
        assert_eq!(name, "lark-acp-abcdef12");
    }

    #[test]
    fn conv_name_dingtalk_truncates_long_chat_id() {
        let name = channel_conversation_name(PluginType::Dingtalk, "acp", Some("vertex"), Some("123456789abcdef"));
        assert_eq!(name, "ding-acp-vertex-12345678");
    }

    #[test]
    fn conv_name_weixin_no_chat_id() {
        let name = channel_conversation_name(PluginType::Weixin, "acp", Some("gemini"), None);
        assert_eq!(name, "wx-acp-gemini");
    }

    #[test]
    fn conv_name_non_acp_ignores_backend() {
        let name = channel_conversation_name(PluginType::Telegram, "nomi", Some("claude"), Some("70880480"));
        assert_eq!(name, "tg-nomi-70880480");
    }
}

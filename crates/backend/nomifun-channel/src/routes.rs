use std::sync::Arc;

use axum::Router;
use axum::extract::rejection::JsonRejection;
use axum::extract::{Json, State};
use axum::routing::{get, post};
use tracing::warn;

use nomifun_api_types::{
    ApiResponse, ApprovePairingRequest, BridgeResponse, ChannelSessionResponse, ChannelUserResponse,
    DisablePluginRequest, EnablePluginRequest, PairingRequestResponse, PluginStatusResponse, RejectPairingRequest,
    RevokeUserRequest, SyncChannelSettingsRequest, TestPluginRequest, TestPluginResponse,
};
use nomifun_common::AppError;
use nomifun_db::IChannelRepository;
use nomifun_extension::{ExtensionRegistry, ResolvedChannelPlugin};
use serde::{Deserialize, Serialize};

use crate::channel_settings::ChannelSettingsService;
use crate::error::{ChannelError, ChannelOwner};
use crate::manager::{ChannelManager, EnableChannelSpec, PluginFactory};
use crate::message_service::MasterAgentProfile;
use crate::pairing::PairingService;
use crate::session::SessionManager;
use crate::types::{PluginConfig, PluginConfigOptions, PluginCredentials, PluginType};

use std::collections::HashMap;

// ---------------------------------------------------------------------------
// Router state
// ---------------------------------------------------------------------------

/// Shared state for channel route handlers.
#[derive(Clone)]
pub struct ChannelRouterState {
    pub manager: Arc<ChannelManager>,
    pub pairing_service: Arc<PairingService>,
    pub session_manager: Arc<SessionManager>,
    pub repo: Arc<dyn IChannelRepository>,
    pub plugin_factory: Arc<PluginFactory>,
    pub settings_service: Arc<ChannelSettingsService>,
    /// Master-agent profile (the companion), used to validate companion-binding writes
    /// against the live roster. `None` when the host wires channels without
    /// a companion domain — validation is then skipped, not failed.
    pub master_profile: Option<Arc<dyn MasterAgentProfile>>,
    pub extension_registry: ExtensionRegistry,
}

// ---------------------------------------------------------------------------
// Router builder
// ---------------------------------------------------------------------------

/// Build the channel router with all `/api/channel/*` routes.
///
/// All routes require authentication (applied by the caller).
pub fn channel_routes(state: ChannelRouterState) -> Router {
    let router = Router::new()
        // Plugin management
        .route("/api/channel/plugins", get(get_plugin_status))
        .route("/api/channel/plugins/enable", post(enable_plugin))
        .route("/api/channel/plugins/disable", post(disable_plugin))
        .route("/api/channel/plugins/delete", post(delete_plugin))
        .route("/api/channel/plugins/test", post(test_plugin))
        // Pairing management
        .route("/api/channel/pairings", get(get_pending_pairings))
        .route("/api/channel/pairings/approve", post(approve_pairing))
        .route("/api/channel/pairings/reject", post(reject_pairing))
        // User management
        .route("/api/channel/users", get(get_authorized_users))
        .route("/api/channel/users/revoke", post(revoke_user))
        // Session management
        .route("/api/channel/sessions", get(get_active_sessions))
        // Settings sync
        .route("/api/channel/settings/sync", post(sync_channel_settings))
        // Master-agent companion binding (persist + session reset in one step)
        .route("/api/channel/settings/companion", post(set_channel_master_companion))
        // 对外伙伴 (public agent) per-bot binding (persist + session reset in one
        // step). Mirrors the per-bot companion binding; row-level mutually
        // exclusive with it.
        .route("/api/channel/settings/public-agent", post(set_channel_public_agent));

    // WeChat QR login starter (feature-gated). Lives in the authenticated
    // channel group — the QR lifecycle then streams over the WebSocket as
    // `channel.weixin-login`, because `EventSource` can't carry the desktop's
    // local-trust header (an SSE stream here was rejected 403).
    #[cfg(feature = "weixin")]
    let router = router.route("/api/channel/weixin/login/start", post(start_weixin_login));

    router.with_state(state)
}

// ---------------------------------------------------------------------------
// Plugin management handlers
// ---------------------------------------------------------------------------

/// `GET /api/channel/plugins` — get status of all registered plugins.
///
/// Returns one entry per channel row (multiple bots may share a platform
/// type), plus a placeholder per builtin platform that has no rows yet and
/// per extension plugin that was never configured.
async fn get_plugin_status(
    State(state): State<ChannelRouterState>,
) -> Result<Json<ApiResponse<Vec<ChannelPluginStatusView>>>, AppError> {
    let statuses = state.manager.get_plugin_status().await?;
    let extension_plugins = state.extension_registry.get_channel_plugins().await;

    let extension_map: HashMap<String, ResolvedChannelPlugin> = extension_plugins
        .into_iter()
        .map(|plugin| (plugin.id.clone(), plugin))
        .collect();

    let builtin_names: [(&str, &str); 12] = [
        ("telegram", "Telegram"),
        ("lark", "Lark"),
        ("dingtalk", "DingTalk"),
        ("slack", "Slack"),
        ("discord", "Discord"),
        ("matrix", "Matrix"),
        ("mattermost", "Mattermost"),
        ("weixin", "WeChat"),
        ("wecom", "WeCom"),
        ("qqbot", "QQ Bot"),
        ("twitch", "Twitch"),
        ("nostr", "Nostr"),
    ];
    let builtin_types: std::collections::HashSet<&str> = builtin_names.iter().map(|(id, _)| *id).collect();

    // Rows are keyed by their own id — two lark bots are two entries.
    let mut views: Vec<ChannelPluginStatusView> = Vec::new();
    let mut seen_types: std::collections::HashSet<String> = std::collections::HashSet::new();

    for status in statuses {
        let plugin_type = status.plugin_type.clone();
        let is_extension = !builtin_types.contains(plugin_type.as_str());

        if is_extension && !extension_map.contains_key(&plugin_type) {
            continue;
        }

        seen_types.insert(plugin_type.clone());
        views.push(ChannelPluginStatusView::from_manager_status(
            status,
            is_extension
                .then(|| extension_map.get(&plugin_type).map(ChannelExtensionMetaView::from))
                .flatten(),
        ));
    }

    for plugin in extension_map.values() {
        if !seen_types.contains(&plugin.id) {
            views.push(ChannelPluginStatusView::extension_placeholder(plugin));
        }
    }

    for (plugin_type, display_name) in builtin_names {
        if !seen_types.contains(plugin_type) {
            views.push(ChannelPluginStatusView::builtin_placeholder(plugin_type, display_name));
        }
    }

    views.sort_by(|left, right| {
        left.plugin_type
            .cmp(&right.plugin_type)
            .then_with(|| left.plugin_id.cmp(&right.plugin_id))
    });

    Ok(Json(ApiResponse::ok(views)))
}

#[derive(Debug, Clone, Serialize)]
struct ChannelPluginStatusView {
    plugin_id: String,
    #[serde(rename = "type")]
    plugin_type: String,
    name: String,
    enabled: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    status: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    last_connected: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    companion_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    public_agent_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    bot_key: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    created_at: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    updated_at: Option<i64>,
    connected: bool,
    has_token: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    bot_username: Option<String>,
    active_users: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    is_extension: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    extension_meta: Option<ChannelExtensionMetaView>,
}

#[derive(Debug, Clone, Serialize)]
struct ChannelExtensionMetaView {
    #[serde(rename = "credentialFields")]
    #[serde(skip_serializing_if = "Vec::is_empty")]
    credential_fields: Vec<serde_json::Value>,
    #[serde(rename = "configFields")]
    #[serde(skip_serializing_if = "Vec::is_empty")]
    config_fields: Vec<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    description: Option<String>,
    #[serde(rename = "extensionName")]
    extension_name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    icon: Option<String>,
}

impl ChannelPluginStatusView {
    fn from_manager_status(status: PluginStatusResponse, extension_meta: Option<ChannelExtensionMetaView>) -> Self {
        Self {
            plugin_id: status.plugin_id,
            plugin_type: status.plugin_type,
            name: status.name,
            enabled: status.enabled,
            status: status.status,
            last_connected: status.last_connected,
            companion_id: status.companion_id,
            public_agent_id: status.public_agent_id,
            bot_key: status.bot_key,
            created_at: Some(status.created_at),
            updated_at: Some(status.updated_at),
            connected: status.connected,
            has_token: status.has_token,
            bot_username: status.bot_username,
            active_users: status.active_users,
            is_extension: extension_meta.as_ref().map(|_| true),
            extension_meta,
        }
    }

    fn extension_placeholder(plugin: &ResolvedChannelPlugin) -> Self {
        Self {
            plugin_id: plugin.id.clone(),
            plugin_type: plugin.id.clone(),
            name: plugin.name.clone(),
            enabled: false,
            status: Some("stopped".to_string()),
            last_connected: None,
            companion_id: None,
            public_agent_id: None,
            bot_key: None,
            created_at: None,
            updated_at: None,
            connected: false,
            has_token: false,
            bot_username: None,
            active_users: 0,
            is_extension: Some(true),
            extension_meta: Some(ChannelExtensionMetaView::from(plugin)),
        }
    }

    fn builtin_placeholder(plugin_type: &str, display_name: &str) -> Self {
        Self {
            plugin_id: plugin_type.to_string(),
            plugin_type: plugin_type.to_string(),
            name: display_name.to_string(),
            enabled: false,
            status: Some("stopped".to_string()),
            last_connected: None,
            companion_id: None,
            public_agent_id: None,
            bot_key: None,
            created_at: None,
            updated_at: None,
            connected: false,
            has_token: false,
            bot_username: None,
            active_users: 0,
            is_extension: Some(false),
            extension_meta: None,
        }
    }
}

impl From<&ResolvedChannelPlugin> for ChannelExtensionMetaView {
    fn from(plugin: &ResolvedChannelPlugin) -> Self {
        Self {
            credential_fields: plugin.credential_fields.clone(),
            config_fields: plugin.config_fields.clone(),
            description: plugin.description.clone(),
            extension_name: plugin.extension_name.clone(),
            icon: plugin.icon.clone(),
        }
    }
}

/// `POST /api/channel/plugins/enable` — enable a bot channel with config.
///
/// `plugin_id` updates an existing channel row (legacy callers pass the
/// platform name); absent `plugin_id` + `plugin_type` creates a new bot
/// channel. `companion_id` binds the bot to a companion (validated against the live
/// roster).
async fn enable_plugin(
    State(state): State<ChannelRouterState>,
    body: Result<Json<EnablePluginRequest>, JsonRejection>,
) -> Result<Json<ApiResponse<BridgeResponse>>, AppError> {
    let Json(req) = body.map_err(|e| AppError::BadRequest(e.to_string()))?;

    if let Some(plugin_id) = req.plugin_id.as_deref()
        && let Some(extension_plugin) = resolve_extension_channel_plugin(&state, plugin_id).await
    {
        let config = build_extension_config(&extension_plugin, &req.config)?;
        match state
            .manager
            .enable_extension_plugin(plugin_id, &extension_plugin.name, &config)
            .await
        {
            Ok(()) => {
                return Ok(Json(ApiResponse::ok(BridgeResponse {
                    success: true,
                    message: Some("Plugin enabled".into()),
                    error: None,
                })));
            }
            Err(e) => {
                warn!(plugin_id = %plugin_id, error = %e, "enable extension plugin failed");
                return Ok(Json(ApiResponse::ok(BridgeResponse {
                    success: false,
                    message: None,
                    error: Some(e.to_string()),
                })));
            }
        }
    }

    // A typo'd or already-deleted companion id must fail here instead of being
    // persisted onto the channel row.
    if let Some(companion_id) = req.companion_id.as_deref().filter(|s| !s.is_empty())
        && let Some(profile) = &state.master_profile
        && !profile.companion_exists(companion_id).await
    {
        return Err(AppError::BadRequest(format!("companion '{companion_id}' not found")));
    }

    // A non-empty public_agent_id must name a LIVE public agent (validated
    // against the roster, mirroring the companion check). Row-level mutual
    // exclusivity (public agent clears companion) is applied in the manager.
    if let Some(public_agent_id) = req.public_agent_id.as_deref().filter(|s| !s.is_empty())
        && let Some(profile) = &state.master_profile
        && !profile.public_agent_exists(public_agent_id).await
    {
        return Err(AppError::BadRequest(format!("public agent '{public_agent_id}' not found")));
    }

    let spec = EnableChannelSpec {
        plugin_id: req.plugin_id.clone().filter(|s| !s.is_empty()),
        plugin_type: req.plugin_type.clone(),
        companion_id: req.companion_id.clone(),
        public_agent_id: req.public_agent_id.clone(),
    };

    match state
        .manager
        .enable_plugin(&spec, &req.config, state.plugin_factory.as_ref())
        .await
    {
        Ok(channel_id) => Ok(Json(ApiResponse::ok(BridgeResponse {
            success: true,
            message: Some(channel_id),
            error: None,
        }))),
        Err(e) => {
            warn!(plugin_id = ?req.plugin_id, plugin_type = ?req.plugin_type, error = %e, "enable plugin failed");
            // Resolve the owner NAME for the "already bound" conflict — the manager
            // only knows the id, but the router holds the roster.
            let error = match &e {
                ChannelError::BotAlreadyBound(owner) => already_bound_message(&state, owner).await,
                _ => e.to_string(),
            };
            Ok(Json(ApiResponse::ok(BridgeResponse {
                success: false,
                message: None,
                error: Some(error),
            })))
        }
    }
}

/// Builds the user-facing "already bound" message, resolving the owner's display
/// NAME from the roster (the manager only knows the id). Falls back to the id
/// when the name can't be resolved (deleted owner, or no profile wired), so the
/// message is always actionable.
async fn already_bound_message(state: &ChannelRouterState, owner: &ChannelOwner) -> String {
    let profile = state.master_profile.as_deref();
    match owner {
        ChannelOwner::Companion(id) => {
            let who = match profile {
                Some(p) => p.companion_name(id).await,
                None => None,
            }
            .map(|name| format!("伙伴「{name}」"))
            .unwrap_or_else(|| format!("伙伴「{id}」"));
            format!("此机器人已绑定{who}，请先在该伙伴处解绑或删除后再在此使用。")
        }
        ChannelOwner::PublicAgent(id) => {
            let who = match profile {
                Some(p) => p.public_agent_name(id).await,
                None => None,
            }
            .map(|name| format!("对外伙伴「{name}」"))
            .unwrap_or_else(|| format!("对外伙伴「{id}」"));
            format!("此机器人已绑定{who}，请先在该对外伙伴处解绑或删除后再在此使用。")
        }
        ChannelOwner::Channel(id) => format!("此机器人已被渠道「{id}」占用。"),
    }
}

/// `POST /api/channel/plugins/disable` — disable a plugin.
async fn disable_plugin(
    State(state): State<ChannelRouterState>,
    body: Result<Json<DisablePluginRequest>, JsonRejection>,
) -> Result<Json<ApiResponse<BridgeResponse>>, AppError> {
    let Json(req) = body.map_err(|e| AppError::BadRequest(e.to_string()))?;

    if resolve_extension_channel_plugin(&state, &req.plugin_id).await.is_some()
        && state.repo.get_plugin(&req.plugin_id).await?.is_none()
    {
        return Ok(Json(ApiResponse::ok(BridgeResponse {
            success: true,
            message: Some("Plugin disabled".into()),
            error: None,
        })));
    }

    match state.manager.disable_plugin(&req.plugin_id).await {
        Ok(()) => Ok(Json(ApiResponse::ok(BridgeResponse {
            success: true,
            message: Some("Plugin disabled".into()),
            error: None,
        }))),
        Err(e) => {
            warn!(plugin_id = %req.plugin_id, error = %e, "disable plugin failed");
            Ok(Json(ApiResponse::ok(BridgeResponse {
                success: false,
                message: None,
                error: Some(e.to_string()),
            })))
        }
    }
}

/// `POST /api/channel/plugins/delete` — remove a bot channel entirely.
///
/// Stops the running instance, clears the channel's sessions, and deletes
/// the row. Conversations created through this bot survive.
async fn delete_plugin(
    State(state): State<ChannelRouterState>,
    body: Result<Json<DisablePluginRequest>, JsonRejection>,
) -> Result<Json<ApiResponse<BridgeResponse>>, AppError> {
    let Json(req) = body.map_err(|e| AppError::BadRequest(e.to_string()))?;

    match state.manager.delete_channel(&req.plugin_id).await {
        Ok(()) => Ok(Json(ApiResponse::ok(BridgeResponse {
            success: true,
            message: Some("Channel deleted".into()),
            error: None,
        }))),
        Err(e) => {
            warn!(plugin_id = %req.plugin_id, error = %e, "delete channel failed");
            Ok(Json(ApiResponse::ok(BridgeResponse {
                success: false,
                message: None,
                error: Some(e.to_string()),
            })))
        }
    }
}

/// `POST /api/channel/plugins/test` — test plugin credentials.
async fn test_plugin(
    State(state): State<ChannelRouterState>,
    body: Result<Json<TestPluginRequest>, JsonRejection>,
) -> Result<Json<ApiResponse<TestPluginResponse>>, AppError> {
    let Json(req) = body.map_err(|e| AppError::BadRequest(e.to_string()))?;

    if let Some(extension_plugin) = resolve_extension_channel_plugin(&state, &req.plugin_id).await {
        let _config = build_extension_test_config(&extension_plugin, &req)?;
        return Ok(Json(ApiResponse::ok(TestPluginResponse {
            success: true,
            bot_username: None,
            error: None,
        })));
    }

    let config = build_test_config(&req);

    match state
        .manager
        .test_plugin(&req.plugin_id, config, state.plugin_factory.as_ref())
        .await
    {
        Ok(bot_username) => Ok(Json(ApiResponse::ok(TestPluginResponse {
            success: true,
            bot_username,
            error: None,
        }))),
        Err(e) => Ok(Json(ApiResponse::ok(TestPluginResponse {
            success: false,
            bot_username: None,
            error: Some(e.to_string()),
        }))),
    }
}

// ---------------------------------------------------------------------------
// Pairing management handlers
// ---------------------------------------------------------------------------

/// `GET /api/channel/pairings` — get all pending pairing requests.
async fn get_pending_pairings(
    State(state): State<ChannelRouterState>,
) -> Result<Json<ApiResponse<Vec<PairingRequestResponse>>>, AppError> {
    let rows = state.pairing_service.get_pending_pairings().await?;
    let responses: Vec<PairingRequestResponse> = rows
        .into_iter()
        .map(|r| PairingRequestResponse {
            code: r.code,
            platform_user_id: r.platform_user_id,
            platform_type: r.platform_type,
            channel_id: r.channel_id,
            display_name: r.display_name,
            requested_at: r.requested_at,
            expires_at: r.expires_at,
        })
        .collect();
    Ok(Json(ApiResponse::ok(responses)))
}

/// `POST /api/channel/pairings/approve` — approve a pairing request.
async fn approve_pairing(
    State(state): State<ChannelRouterState>,
    body: Result<Json<ApprovePairingRequest>, JsonRejection>,
) -> Result<Json<ApiResponse<BridgeResponse>>, AppError> {
    let Json(req) = body.map_err(|e| AppError::BadRequest(e.to_string()))?;

    state.pairing_service.approve_pairing(&req.code).await?;

    Ok(Json(ApiResponse::ok(BridgeResponse {
        success: true,
        message: Some("Pairing approved".into()),
        error: None,
    })))
}

/// `POST /api/channel/pairings/reject` — reject a pairing request.
async fn reject_pairing(
    State(state): State<ChannelRouterState>,
    body: Result<Json<RejectPairingRequest>, JsonRejection>,
) -> Result<Json<ApiResponse<BridgeResponse>>, AppError> {
    let Json(req) = body.map_err(|e| AppError::BadRequest(e.to_string()))?;

    state.pairing_service.reject_pairing(&req.code).await?;

    Ok(Json(ApiResponse::ok(BridgeResponse {
        success: true,
        message: Some("Pairing rejected".into()),
        error: None,
    })))
}

// ---------------------------------------------------------------------------
// User management handlers
// ---------------------------------------------------------------------------

/// `GET /api/channel/users` — get all authorized users.
async fn get_authorized_users(
    State(state): State<ChannelRouterState>,
) -> Result<Json<ApiResponse<Vec<ChannelUserResponse>>>, AppError> {
    let rows = state.repo.get_all_users().await?;
    let responses: Vec<ChannelUserResponse> = rows
        .into_iter()
        .map(|r| ChannelUserResponse {
            id: r.id,
            platform_user_id: r.platform_user_id,
            platform_type: r.platform_type,
            channel_id: r.channel_id,
            display_name: r.display_name,
            authorized_at: r.authorized_at,
            last_active: r.last_active,
        })
        .collect();
    Ok(Json(ApiResponse::ok(responses)))
}

/// `POST /api/channel/users/revoke` — revoke a user's authorization.
///
/// Also cleans up the user's sessions.
async fn revoke_user(
    State(state): State<ChannelRouterState>,
    body: Result<Json<RevokeUserRequest>, JsonRejection>,
) -> Result<Json<ApiResponse<BridgeResponse>>, AppError> {
    let Json(req) = body.map_err(|e| AppError::BadRequest(e.to_string()))?;

    // Clean up sessions first
    state.session_manager.cleanup_user_sessions(&req.user_id).await?;

    // Delete user record
    state.repo.delete_user(&req.user_id).await?;

    Ok(Json(ApiResponse::ok(BridgeResponse {
        success: true,
        message: Some("User revoked".into()),
        error: None,
    })))
}

// ---------------------------------------------------------------------------
// Session management handlers
// ---------------------------------------------------------------------------

/// `GET /api/channel/sessions` — get all active sessions.
async fn get_active_sessions(
    State(state): State<ChannelRouterState>,
) -> Result<Json<ApiResponse<Vec<ChannelSessionResponse>>>, AppError> {
    let rows = state.session_manager.get_active_sessions().await?;
    let responses: Vec<ChannelSessionResponse> = rows
        .into_iter()
        .map(|r| ChannelSessionResponse {
            id: r.id,
            user_id: r.user_id,
            agent_type: r.agent_type,
            conversation_id: r.conversation_id,
            workspace: r.workspace,
            chat_id: r.chat_id,
            channel_id: r.channel_id,
            created_at: r.created_at,
            last_activity: r.last_activity,
        })
        .collect();
    Ok(Json(ApiResponse::ok(responses)))
}

// ---------------------------------------------------------------------------
// Settings sync handler
// ---------------------------------------------------------------------------

/// `POST /api/channel/settings/sync` — invalidate channel sessions.
///
/// Clears all sessions so they are recreated with the latest
/// agent/model configuration on the next incoming message.
/// Agent/model config is persisted separately via `PUT /api/settings/client`.
async fn sync_channel_settings(
    State(state): State<ChannelRouterState>,
    body: Result<Json<SyncChannelSettingsRequest>, JsonRejection>,
) -> Result<Json<ApiResponse<BridgeResponse>>, AppError> {
    let Json(req) = body.map_err(|e| AppError::BadRequest(e.to_string()))?;

    let _platform = PluginType::from_str_opt(&req.platform)
        .ok_or_else(|| AppError::BadRequest(format!("Invalid platform: {}", req.platform)))?;

    state.session_manager.clear_all_sessions().await?;

    Ok(Json(ApiResponse::ok(BridgeResponse {
        success: true,
        message: Some(format!("Sessions cleared for {}", req.platform)),
        error: None,
    })))
}

/// Request body for `POST /api/channel/settings/companion`.
///
/// `plugin_id` set → rebind one bot channel (the multi-bot path; only that
/// channel's sessions are cleared). `platform` set → legacy platform-level
/// binding (key `assistant.{platform}.companionId`, clears all sessions).
/// `companion_id: None` / empty string clears the binding.
#[derive(Debug, Deserialize)]
struct SetChannelCompanionRequest {
    #[serde(default, alias = "pluginId")]
    plugin_id: Option<String>,
    #[serde(default)]
    platform: Option<String>,
    #[serde(default, alias = "companionId")]
    companion_id: Option<String>,
}

/// `POST /api/channel/settings/companion` — bind (or clear) the companion that greets a
/// bot channel's master-agent sessions.
///
/// Per-channel writes go to `assistant_plugins.companion_id` and clear only that
/// channel's sessions; legacy platform writes keep the old preference key
/// and clear all sessions. Both fold write + reset into one step so the
/// reset cannot be skipped.
async fn set_channel_master_companion(
    State(state): State<ChannelRouterState>,
    body: Result<Json<SetChannelCompanionRequest>, JsonRejection>,
) -> Result<Json<ApiResponse<BridgeResponse>>, AppError> {
    let Json(req) = body.map_err(|e| AppError::BadRequest(e.to_string()))?;

    // Validate a non-empty binding against the live companion roster: a typo'd or
    // already-deleted companion id must 400 here instead of being persisted and
    // silently degrading every session to the default companion. Clearing the
    // binding (None / empty string) needs no validation.
    if let Some(companion_id) = req.companion_id.as_deref().filter(|s| !s.is_empty())
        && let Some(profile) = &state.master_profile
        && !profile.companion_exists(companion_id).await
    {
        return Err(AppError::BadRequest(format!("companion '{companion_id}' not found")));
    }

    let companion_id = req.companion_id.as_deref().map(str::trim).filter(|s| !s.is_empty());

    if let Some(plugin_id) = req.plugin_id.as_deref().filter(|s| !s.is_empty()) {
        state.manager.rebind_channel_companion(plugin_id, companion_id).await?;
        return Ok(Json(ApiResponse::ok(BridgeResponse {
            success: true,
            message: Some(format!("Companion binding updated for channel {plugin_id}; channel sessions cleared")),
            error: None,
        })));
    }

    let platform_str = req
        .platform
        .as_deref()
        .ok_or_else(|| AppError::BadRequest("either plugin_id or platform is required".into()))?;
    let platform = PluginType::from_str_opt(platform_str)
        .ok_or_else(|| AppError::BadRequest(format!("Invalid platform: {platform_str}")))?;

    state.settings_service.set_master_agent_companion_id(platform, companion_id).await?;

    // Same reset branch as the masterAgent switch: clear active sessions so
    // the next message starts a conversation under the new binding.
    state.session_manager.clear_all_sessions().await?;

    Ok(Json(ApiResponse::ok(BridgeResponse {
        success: true,
        message: Some(format!("Companion binding updated for {platform_str}; sessions cleared")),
        error: None,
    })))
}

// ---------------------------------------------------------------------------
// 对外伙伴 (public agent) per-bot binding handler
// ---------------------------------------------------------------------------

/// Request body for `POST /api/channel/settings/public-agent`.
///
/// Rebinds one bot channel's 对外伙伴 (public agent) — writes
/// `assistant_plugins.public_agent_id` and clears only that channel's sessions.
/// `public_agent_id: None` / empty string clears the binding (unbinds). Mirrors
/// the per-bot companion binding; row-level mutually exclusive with it.
#[derive(Debug, Deserialize)]
struct SetChannelPublicAgentRequest {
    #[serde(default, alias = "pluginId")]
    plugin_id: String,
    #[serde(default, alias = "publicAgentId")]
    public_agent_id: Option<String>,
}

/// `POST /api/channel/settings/public-agent` — bind (or clear) the 对外伙伴
/// (public agent) that serves a bot channel. A non-null id is validated against
/// the live roster and, on success, clears that bot's companion binding (row-level
/// mutual exclusivity, enforced in the repository layer). `null`/empty unbinds.
/// Clears the channel's sessions so the next inbound message recreates
/// conversations under the new binding.
async fn set_channel_public_agent(
    State(state): State<ChannelRouterState>,
    body: Result<Json<SetChannelPublicAgentRequest>, JsonRejection>,
) -> Result<Json<ApiResponse<BridgeResponse>>, AppError> {
    let Json(req) = body.map_err(|e| AppError::BadRequest(e.to_string()))?;

    let plugin_id = req.plugin_id.trim();
    if plugin_id.is_empty() {
        return Err(AppError::BadRequest("plugin_id is required".into()));
    }

    // Validate a non-empty binding against the live public-agent roster: a typo'd
    // or already-deleted id must 400 here instead of being persisted. Clearing
    // (null / empty) needs no validation. Skipped when no profile is wired.
    if let Some(public_agent_id) = req.public_agent_id.as_deref().map(str::trim).filter(|s| !s.is_empty())
        && let Some(profile) = &state.master_profile
        && !profile.public_agent_exists(public_agent_id).await
    {
        return Err(AppError::BadRequest(format!("public agent '{public_agent_id}' not found")));
    }

    let public_agent_id = req.public_agent_id.as_deref().map(str::trim).filter(|s| !s.is_empty());
    state.manager.rebind_channel_public_agent(plugin_id, public_agent_id).await?;

    Ok(Json(ApiResponse::ok(BridgeResponse {
        success: true,
        message: Some(format!(
            "Public agent binding updated for channel {plugin_id}; channel sessions cleared"
        )),
        error: None,
    })))
}

// ---------------------------------------------------------------------------
// WeChat login handler
// ---------------------------------------------------------------------------

/// `POST /api/channel/weixin/login/start` — begin the WeChat QR-code login.
///
/// Returns immediately; the QR lifecycle (`qr` → `scanned` → `done`/`error`)
/// streams over the WebSocket as `channel.weixin-login`. We use the WebSocket
/// rather than SSE because `EventSource` cannot carry the desktop's
/// `x-nomi-local-trust` header, so an SSE stream was rejected 403 by the auth
/// middleware and surfaced in the UI as an instant "WeChat login failed".
#[cfg(feature = "weixin")]
async fn start_weixin_login(State(state): State<ChannelRouterState>) -> Json<ApiResponse<BridgeResponse>> {
    state.manager.start_weixin_login();
    Json(ApiResponse::ok(BridgeResponse {
        success: true,
        message: None,
        error: None,
    }))
}

// ---------------------------------------------------------------------------
// Helper functions
// ---------------------------------------------------------------------------

/// Build a `PluginConfig` from a `TestPluginRequest`.
///
/// Maps the `token` and optional `extra_config` fields into the
/// correct credential fields based on the plugin type.
fn build_test_config(req: &TestPluginRequest) -> PluginConfig {
    let mut credentials = PluginCredentials::default();

    match req.plugin_id.as_str() {
        "lark" => {
            if let Some(ref extra) = req.extra_config {
                credentials.app_id = extra.app_id.clone();
                credentials.app_secret = extra.app_secret.clone();
            }
            credentials.token = Some(req.token.clone());
        }
        "dingtalk" => {
            credentials.client_id = Some(req.token.clone());
            if let Some(ref extra) = req.extra_config {
                credentials.client_secret = extra.app_secret.clone();
            }
        }
        "weixin" => {
            credentials.bot_token = Some(req.token.clone());
            if let Some(ref extra) = req.extra_config {
                credentials.account_id = extra.app_id.clone();
            }
        }
        "wecom" => {
            // Long-connection creds: bot_id in `token`, secret in extra.app_secret.
            credentials.bot_id = Some(req.token.clone());
            if let Some(ref extra) = req.extra_config {
                credentials.secret = extra.app_secret.clone();
            }
        }
        "slack" => {
            // Bot token (xoxb-) in `token`; app-level token (xapp-) in extra.
            credentials.token = Some(req.token.clone());
            if let Some(ref extra) = req.extra_config {
                credentials.app_token = extra.app_token.clone();
            }
        }
        "matrix" => {
            // Access token in `token`; homeserver + bot mxid in extra.
            credentials.access_token = Some(req.token.clone());
            if let Some(ref extra) = req.extra_config {
                credentials.homeserver_url = extra.homeserver_url.clone();
                credentials.user_id = extra.user_id.clone();
            }
        }
        "mattermost" => {
            // Bot token in `token`; server URL in extra.
            credentials.token = Some(req.token.clone());
            if let Some(ref extra) = req.extra_config {
                credentials.server_url = extra.server_url.clone();
            }
        }
        "twitch" => {
            // OAuth access token in `token` (channel only needed at run time).
            credentials.token = Some(req.token.clone());
        }
        "nostr" => {
            // Private key (nsec/hex) in `token`; relay list in extra.
            credentials.nostr_private_key = Some(req.token.clone());
            if let Some(ref extra) = req.extra_config {
                credentials.nostr_relays = extra.nostr_relays.clone();
            }
        }
        "qqbot" => {
            // appId in `token` (client_id); clientSecret in extra (client_secret).
            credentials.client_id = Some(req.token.clone());
            if let Some(ref extra) = req.extra_config {
                credentials.client_secret = extra.app_secret.clone();
            }
        }
        _ => {
            // Default: use token field (Telegram, Discord)
            credentials.token = Some(req.token.clone());
        }
    }

    PluginConfig {
        credentials,
        config: None,
    }
}

async fn resolve_extension_channel_plugin(
    state: &ChannelRouterState,
    plugin_id: &str,
) -> Option<ResolvedChannelPlugin> {
    state
        .extension_registry
        .get_channel_plugins()
        .await
        .into_iter()
        .find(|plugin| plugin.id == plugin_id)
}

fn build_extension_test_config(
    plugin: &ResolvedChannelPlugin,
    req: &TestPluginRequest,
) -> Result<PluginConfig, ChannelError> {
    let mut map = serde_json::Map::new();
    if !req.token.is_empty() {
        map.insert("token".to_string(), serde_json::Value::String(req.token.clone()));
    }
    if let Some(extra) = &req.extra_config {
        if let Some(app_id) = &extra.app_id {
            map.insert("appId".to_string(), serde_json::Value::String(app_id.clone()));
        }
        if let Some(app_secret) = &extra.app_secret {
            map.insert("appSecret".to_string(), serde_json::Value::String(app_secret.clone()));
        }
    }
    build_extension_config(plugin, &serde_json::Value::Object(map))
}

fn build_extension_config(
    plugin: &ResolvedChannelPlugin,
    raw: &serde_json::Value,
) -> Result<PluginConfig, ChannelError> {
    let object = raw
        .as_object()
        .ok_or_else(|| ChannelError::InvalidConfig("Extension plugin config must be an object".into()))?;

    let mut credentials = PluginCredentials::default();
    let mut config_extra = HashMap::new();

    let credential_keys: std::collections::HashSet<String> = plugin
        .credential_fields
        .iter()
        .filter_map(field_key)
        .map(ToOwned::to_owned)
        .collect();
    for field in &plugin.config_fields {
        if let Some((key, value)) = field_default_entry(field) {
            config_extra.entry(key.to_string()).or_insert(value);
        }
    }

    for (key, value) in object {
        if credential_keys.contains(key) {
            credentials.extra.insert(key.clone(), value.clone());
        } else {
            config_extra.insert(key.clone(), value.clone());
        }
    }

    Ok(PluginConfig {
        credentials,
        config: if config_extra.is_empty() {
            None
        } else {
            Some(PluginConfigOptions {
                mode: None,
                webhook_url: None,
                rate_limit: None,
                require_mention: None,
                extra: config_extra,
            })
        },
    })
}

fn field_key(value: &serde_json::Value) -> Option<&str> {
    value.get("key").and_then(serde_json::Value::as_str)
}

fn field_default_entry(value: &serde_json::Value) -> Option<(&str, serde_json::Value)> {
    let key = field_key(value)?;
    let default = value.get("default")?;
    Some((key, default.clone()))
}

// ---------------------------------------------------------------------------
// Unit tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use nomifun_api_types::TestPluginExtraConfig;

    #[test]
    fn build_test_config_telegram() {
        let req = TestPluginRequest {
            plugin_id: "telegram".into(),
            token: "bot123:ABC".into(),
            extra_config: None,
        };
        let config = build_test_config(&req);
        assert_eq!(config.credentials.token.as_deref(), Some("bot123:ABC"));
    }

    #[test]
    fn build_test_config_lark() {
        let req = TestPluginRequest {
            plugin_id: "lark".into(),
            token: "xxx".into(),
            extra_config: Some(TestPluginExtraConfig {
                app_id: Some("cli_abc".into()),
                app_secret: Some("secret".into()),
                ..Default::default()
            }),
        };
        let config = build_test_config(&req);
        assert_eq!(config.credentials.app_id.as_deref(), Some("cli_abc"));
        assert_eq!(config.credentials.app_secret.as_deref(), Some("secret"));
        assert_eq!(config.credentials.token.as_deref(), Some("xxx"));
    }

    #[test]
    fn build_test_config_dingtalk() {
        let req = TestPluginRequest {
            plugin_id: "dingtalk".into(),
            token: "client_id_123".into(),
            extra_config: Some(TestPluginExtraConfig {
                app_id: None,
                app_secret: Some("client_secret_456".into()),
                ..Default::default()
            }),
        };
        let config = build_test_config(&req);
        assert_eq!(config.credentials.client_id.as_deref(), Some("client_id_123"));
        assert_eq!(config.credentials.client_secret.as_deref(), Some("client_secret_456"));
    }

    #[test]
    fn build_test_config_weixin() {
        let req = TestPluginRequest {
            plugin_id: "weixin".into(),
            token: "bot_token_xyz".into(),
            extra_config: Some(TestPluginExtraConfig {
                app_id: Some("account_abc".into()),
                app_secret: None,
                ..Default::default()
            }),
        };
        let config = build_test_config(&req);
        assert_eq!(config.credentials.bot_token.as_deref(), Some("bot_token_xyz"));
        assert_eq!(config.credentials.account_id.as_deref(), Some("account_abc"));
    }
}

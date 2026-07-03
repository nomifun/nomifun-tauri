//! Channel-domain capabilities (registry form): IM bot lifecycle,
//! pairing/authorization management, and companion binding.
//!
//! These tools let the LLM agent configure remote IM channels on behalf of the
//! user — the headline use case is "set up a Telegram bot and bind it to my
//! work companion" spoken via conversation (no manual UI required).
//!
//! ## Assumed GatewayDeps field
//!
//! ```ignore
//! pub channel_state: nomifun_channel::ChannelRouterState,
//! ```
//!
//! The parent obtains this from `states.channel` (the `ModuleStates.channel`
//! field built by `build_module_states` in `nomifun_app::router::state`).
//! `ChannelRouterState` bundles `Arc<ChannelManager>`, `Arc<PairingService>`,
//! `Arc<SessionManager>`, `Arc<dyn IChannelRepository>`, `Arc<PluginFactory>`,
//! `Arc<ChannelSettingsService>`, `Option<Arc<dyn MasterAgentProfile>>`, and
//! `ExtensionRegistry`.

use std::sync::Arc;

use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::deps::GatewayDeps;
use crate::registry::{Capability, CapabilityMeta, DangerTier, Surface};
use crate::server::ok;

// ── param structs ────────────────────────────────────────────────────────

#[derive(Deserialize, JsonSchema)]
struct ListPluginsParams {}

#[derive(Deserialize, JsonSchema)]
struct EnablePluginParams {
    /// Platform type of the bot to create/update. Required when creating a new
    /// bot (omit `plugin_id`). Supported builtins: "telegram", "discord",
    /// "slack", "lark", "dingtalk", "weixin", "matrix", "mattermost",
    /// "twitch", "nostr", "qqbot". Extension plugins use their registered id.
    #[serde(default)]
    plugin_type: Option<String>,

    /// Existing channel row id to reconfigure. If omitted, a new bot is
    /// created (requires `plugin_type`). When provided, updates config in
    /// place.
    #[serde(default)]
    plugin_id: Option<String>,

    /// Companion id to bind this bot to. Messages arriving on the channel will
    /// be routed to this companion. Omit or pass null to use the default
    /// companion.
    #[serde(default)]
    companion_id: Option<String>,

    /// Platform-specific credentials and configuration as a JSON object.
    ///
    /// The shape is `{ "credentials": { ... }, "config": { ... } }` where:
    ///
    /// **credentials** (required fields depend on platform):
    /// - telegram/discord/twitch: `{ "token": "<bot_token>" }`
    /// - lark: `{ "token": "<verification_token>", "app_id": "...", "app_secret": "..." }`
    /// - dingtalk: `{ "client_id": "...", "client_secret": "..." }`
    /// - slack: `{ "token": "<xoxb-bot-token>", "app_token": "<xapp-token>" }`
    /// - weixin: `{ "bot_token": "...", "account_id": "..." }`
    /// - matrix: `{ "access_token": "...", "homeserver_url": "...", "user_id": "@bot:server" }`
    /// - mattermost: `{ "token": "...", "server_url": "https://..." }`
    /// - nostr: `{ "nostr_private_key": "<nsec/hex>", "nostr_relays": "wss://r1,wss://r2" }`
    /// - qqbot: `{ "client_id": "<appId>", "client_secret": "..." }`
    ///
    /// **config** (optional):
    /// - `mode`: connection mode if applicable
    /// - `webhook_url`: for platforms that support webhook mode
    /// - `require_mention`: whether bot responds only when mentioned
    /// - `rate_limit`: messages per minute cap
    ///
    /// Pass the full object; do NOT flatten credentials to the top level.
    config: Value,
}

#[derive(Deserialize, JsonSchema)]
struct DisablePluginParams {
    /// The channel row id (plugin_id) of the bot to disable. The bot is
    /// stopped but its configuration is retained for re-enabling.
    plugin_id: String,
}

#[derive(Deserialize, JsonSchema)]
struct DeletePluginParams {
    /// The channel row id (plugin_id) of the bot to permanently delete. This
    /// stops the bot, removes all its sessions, and deletes the database row.
    /// Conversations created through this bot are NOT deleted.
    plugin_id: String,
}

#[derive(Deserialize, JsonSchema)]
struct TestPluginParams {
    /// The platform identifier for the bot being tested (e.g. "telegram",
    /// "lark", "discord", etc.). For an existing channel, use the plugin_id
    /// from nomi_channel_list_plugins.
    plugin_id: String,

    /// Primary credential token for the platform. Meaning varies:
    /// - telegram/discord/twitch: bot token
    /// - lark: verification token
    /// - dingtalk/qqbot: client_id (appId)
    /// - slack: xoxb bot token
    /// - weixin: bot_token
    /// - matrix: access_token
    /// - mattermost: bot token
    /// - nostr: private key (nsec/hex)
    token: String,

    /// Additional platform-specific credentials for testing.
    #[serde(default)]
    extra_config: Option<TestExtraConfig>,
}

/// Additional credentials needed to test specific platforms beyond the primary
/// token.
#[derive(Deserialize, JsonSchema)]
struct TestExtraConfig {
    /// Lark/DingTalk/QQBot: app_id or related secondary credential.
    #[serde(default)]
    app_id: Option<String>,
    /// Lark: app_secret; DingTalk/QQBot: client_secret.
    #[serde(default)]
    app_secret: Option<String>,
    /// Slack: xapp- level app token for Socket Mode.
    #[serde(default)]
    app_token: Option<String>,
    /// Matrix: homeserver URL (e.g. "https://matrix.org").
    #[serde(default)]
    homeserver_url: Option<String>,
    /// Matrix: bot user id (e.g. "@bot:matrix.org").
    #[serde(default)]
    user_id: Option<String>,
    /// Mattermost: server URL.
    #[serde(default)]
    server_url: Option<String>,
    /// Nostr: comma-separated relay URLs.
    #[serde(default)]
    nostr_relays: Option<String>,
}

#[derive(Deserialize, JsonSchema)]
struct ListPairingsParams {}

#[derive(Deserialize, JsonSchema)]
struct ApprovePairingParams {
    /// The pairing code to approve (from nomi_channel_list_pairings).
    code: String,
}

#[derive(Deserialize, JsonSchema)]
struct RejectPairingParams {
    /// The pairing code to reject (from nomi_channel_list_pairings).
    code: String,
}

#[derive(Deserialize, JsonSchema)]
struct ListUsersParams {}

#[derive(Deserialize, JsonSchema)]
struct RevokeUserParams {
    /// The internal user id to revoke (from nomi_channel_list_users). This
    /// removes authorization and clears all sessions for this user.
    user_id: String,
}

#[derive(Deserialize, JsonSchema)]
struct SetCompanionParams {
    /// Target a specific bot/channel by its row id. Takes priority over
    /// `platform`. The companion binding is scoped to this single bot.
    #[serde(default)]
    plugin_id: Option<String>,

    /// Target all bots of a given platform type (legacy path). Used only when
    /// `plugin_id` is not provided. Supported: "telegram", "lark",
    /// "dingtalk", "slack", "discord", "weixin", "matrix", "mattermost",
    /// "twitch", "nostr", "qqbot".
    #[serde(default)]
    platform: Option<String>,

    /// Companion id to bind. Pass null or omit to clear the binding (reverts
    /// to the default companion).
    #[serde(default)]
    companion_id: Option<String>,
}

// ── handlers ─────────────────────────────────────────────────────────────

async fn list_plugins(deps: Arc<GatewayDeps>, _p: ListPluginsParams) -> Value {
    match deps.channel_state.manager.get_plugin_status().await {
        Ok(statuses) => ok(statuses),
        Err(e) => json!({ "error": e.to_string() }),
    }
}

async fn enable_plugin(deps: Arc<GatewayDeps>, p: EnablePluginParams) -> Value {
    use nomifun_channel::manager::EnableChannelSpec;

    // Validate companion binding if provided.
    if let Some(companion_id) = p.companion_id.as_deref().filter(|s| !s.is_empty()) {
        if let Some(profile) = &deps.channel_state.master_profile {
            if !profile.companion_exists(companion_id).await {
                return json!({ "error": format!("companion '{}' not found", companion_id) });
            }
        }
    }

    let spec = EnableChannelSpec {
        plugin_id: p.plugin_id.clone().filter(|s| !s.is_empty()),
        plugin_type: p.plugin_type.clone(),
        companion_id: p.companion_id.clone(),
        public_agent_id: None,
    };

    match deps
        .channel_state
        .manager
        .enable_plugin(&spec, &p.config, deps.channel_state.plugin_factory.as_ref())
        .await
    {
        Ok(channel_id) => ok(json!({
            "channel_id": channel_id,
            "note": "bot enabled; use nomi_channel_test_plugin to verify credentials connect successfully"
        })),
        Err(e) => json!({ "error": e.to_string() }),
    }
}

async fn disable_plugin(deps: Arc<GatewayDeps>, p: DisablePluginParams) -> Value {
    match deps.channel_state.manager.disable_plugin(&p.plugin_id).await {
        Ok(()) => ok(json!({ "disabled": true, "plugin_id": p.plugin_id })),
        Err(e) => json!({ "error": e.to_string() }),
    }
}

async fn delete_plugin(deps: Arc<GatewayDeps>, p: DeletePluginParams) -> Value {
    match deps.channel_state.manager.delete_channel(&p.plugin_id).await {
        Ok(()) => json!({ "result": format!("channel {} permanently deleted", p.plugin_id) }),
        Err(e) => json!({ "error": e.to_string() }),
    }
}

async fn test_plugin(deps: Arc<GatewayDeps>, p: TestPluginParams) -> Value {
    use nomifun_channel::types::{PluginConfig, PluginCredentials};

    let mut credentials = PluginCredentials::default();
    let extra = p.extra_config.as_ref();

    match p.plugin_id.as_str() {
        "lark" => {
            credentials.token = Some(p.token.clone());
            if let Some(e) = extra {
                credentials.app_id = e.app_id.clone();
                credentials.app_secret = e.app_secret.clone();
            }
        }
        "dingtalk" => {
            credentials.client_id = Some(p.token.clone());
            if let Some(e) = extra {
                credentials.client_secret = e.app_secret.clone();
            }
        }
        "weixin" => {
            credentials.bot_token = Some(p.token.clone());
            if let Some(e) = extra {
                credentials.account_id = e.app_id.clone();
            }
        }
        "slack" => {
            credentials.token = Some(p.token.clone());
            if let Some(e) = extra {
                credentials.app_token = e.app_token.clone();
            }
        }
        "matrix" => {
            credentials.access_token = Some(p.token.clone());
            if let Some(e) = extra {
                credentials.homeserver_url = e.homeserver_url.clone();
                credentials.user_id = e.user_id.clone();
            }
        }
        "mattermost" => {
            credentials.token = Some(p.token.clone());
            if let Some(e) = extra {
                credentials.server_url = e.server_url.clone();
            }
        }
        "twitch" => {
            credentials.token = Some(p.token.clone());
        }
        "nostr" => {
            credentials.nostr_private_key = Some(p.token.clone());
            if let Some(e) = extra {
                credentials.nostr_relays = e.nostr_relays.clone();
            }
        }
        "qqbot" => {
            credentials.client_id = Some(p.token.clone());
            if let Some(e) = extra {
                credentials.client_secret = e.app_secret.clone();
            }
        }
        _ => {
            // Default: telegram, discord, and others use generic token.
            credentials.token = Some(p.token.clone());
        }
    }

    let config = PluginConfig {
        credentials,
        config: None,
    };

    match deps
        .channel_state
        .manager
        .test_plugin(&p.plugin_id, config, deps.channel_state.plugin_factory.as_ref())
        .await
    {
        Ok(bot_username) => ok(json!({
            "success": true,
            "bot_username": bot_username,
        })),
        Err(e) => ok(json!({
            "success": false,
            "error": e.to_string(),
        })),
    }
}

async fn list_pairings(deps: Arc<GatewayDeps>, _p: ListPairingsParams) -> Value {
    match deps.channel_state.pairing_service.get_pending_pairings().await {
        Ok(rows) => {
            let pairings: Vec<Value> = rows
                .into_iter()
                .map(|r| {
                    json!({
                        "code": r.code,
                        "platform_user_id": r.platform_user_id,
                        "platform_type": r.platform_type,
                        "channel_id": r.channel_id,
                        "display_name": r.display_name,
                        "requested_at": r.requested_at,
                        "expires_at": r.expires_at,
                    })
                })
                .collect();
            ok(pairings)
        }
        Err(e) => json!({ "error": e.to_string() }),
    }
}

async fn approve_pairing(deps: Arc<GatewayDeps>, p: ApprovePairingParams) -> Value {
    match deps.channel_state.pairing_service.approve_pairing(&p.code).await {
        Ok(()) => ok(json!({ "approved": true, "code": p.code })),
        Err(e) => json!({ "error": e.to_string() }),
    }
}

async fn reject_pairing(deps: Arc<GatewayDeps>, p: RejectPairingParams) -> Value {
    match deps.channel_state.pairing_service.reject_pairing(&p.code).await {
        Ok(()) => ok(json!({ "rejected": true, "code": p.code })),
        Err(e) => json!({ "error": e.to_string() }),
    }
}

async fn list_users(deps: Arc<GatewayDeps>, _p: ListUsersParams) -> Value {
    match deps.channel_state.repo.get_all_users().await {
        Ok(rows) => {
            let users: Vec<Value> = rows
                .into_iter()
                .map(|r| {
                    json!({
                        "id": r.id,
                        "platform_user_id": r.platform_user_id,
                        "platform_type": r.platform_type,
                        "channel_id": r.channel_id,
                        "display_name": r.display_name,
                        "authorized_at": r.authorized_at,
                        "last_active": r.last_active,
                    })
                })
                .collect();
            ok(users)
        }
        Err(e) => json!({ "error": e.to_string() }),
    }
}

async fn revoke_user(deps: Arc<GatewayDeps>, p: RevokeUserParams) -> Value {
    // Clean up sessions first, then delete the user record.
    if let Err(e) = deps
        .channel_state
        .session_manager
        .cleanup_user_sessions(&p.user_id)
        .await
    {
        return json!({ "error": format!("failed to clean sessions: {}", e) });
    }
    match deps.channel_state.repo.delete_user(&p.user_id).await {
        Ok(()) => json!({ "result": format!("user {} revoked", p.user_id) }),
        Err(e) => json!({ "error": e.to_string() }),
    }
}

async fn set_companion(deps: Arc<GatewayDeps>, p: SetCompanionParams) -> Value {
    let companion_id = p.companion_id.as_deref().map(str::trim).filter(|s| !s.is_empty());

    // Validate companion existence if binding (not clearing).
    if let Some(cid) = companion_id {
        if let Some(profile) = &deps.channel_state.master_profile {
            if !profile.companion_exists(cid).await {
                return json!({ "error": format!("companion '{}' not found", cid) });
            }
        }
    }

    // Per-channel binding (preferred).
    if let Some(plugin_id) = p.plugin_id.as_deref().filter(|s| !s.is_empty()) {
        return match deps
            .channel_state
            .manager
            .rebind_channel_companion(plugin_id, companion_id)
            .await
        {
            Ok(()) => ok(json!({
                "bound": true,
                "plugin_id": plugin_id,
                "companion_id": companion_id,
                "note": "channel sessions cleared; next message starts fresh under new companion"
            })),
            Err(e) => json!({ "error": e.to_string() }),
        };
    }

    // Legacy platform-wide binding.
    let platform_str = match p.platform.as_deref().filter(|s| !s.is_empty()) {
        Some(s) => s,
        None => {
            return json!({ "error": "either plugin_id or platform is required" });
        }
    };

    use nomifun_channel::types::PluginType;
    let platform = match PluginType::from_str_opt(platform_str) {
        Some(pt) => pt,
        None => {
            return json!({ "error": format!("invalid platform: {}", platform_str) });
        }
    };

    if let Err(e) = deps
        .channel_state
        .settings_service
        .set_master_agent_companion_id(platform, companion_id)
        .await
    {
        return json!({ "error": e.to_string() });
    }

    // Clear all sessions so next message starts under the new companion.
    if let Err(e) = deps.channel_state.session_manager.clear_all_sessions().await {
        return json!({ "error": format!("binding updated but session reset failed: {}", e) });
    }

    ok(json!({
        "bound": true,
        "platform": platform_str,
        "companion_id": companion_id,
        "note": "platform-wide companion binding updated; all channel sessions cleared"
    }))
}

// ── registration ─────────────────────────────────────────────────────────

/// Register the channel-domain capabilities.
pub(crate) fn register(out: &mut Vec<Capability>) {
    // 1. List configured channel bots + status (read-only).
    out.push(Capability::new::<ListPluginsParams, _, _>(
        CapabilityMeta::new(
            "nomi_channel_list_plugins",
            "channel",
            "List all configured IM channel bots (telegram, discord, slack, lark, etc.) with their connection status, companion binding, and authorized user count.",
            DangerTier::Read,
        ),
        |deps, _ctx, p| list_plugins(deps, p),
    ));

    // 2. Enable/configure a bot channel (sensitive — writes credentials).
    out.push(Capability::new::<EnablePluginParams, _, _>(
        CapabilityMeta::new(
            "nomi_channel_enable_plugin",
            "channel",
            "Enable or reconfigure an IM bot channel with platform-specific credentials. Creates a new bot if plugin_id is omitted, updates existing if provided. Optionally binds to a companion.",
            DangerTier::Sensitive,
        )
        .deny_on(&[Surface::Channel, Surface::Remote]),
        |deps, _ctx, p| enable_plugin(deps, p),
    ));

    // 3. Disable a bot channel (write — config retained).
    out.push(Capability::new::<DisablePluginParams, _, _>(
        CapabilityMeta::new(
            "nomi_channel_disable_plugin",
            "channel",
            "Disable an IM bot channel. The bot is stopped but configuration is retained for re-enabling later.",
            DangerTier::Write,
        ),
        |deps, _ctx, p| disable_plugin(deps, p),
    ));

    // 4. Delete a bot channel permanently (destructive).
    out.push(Capability::new::<DeletePluginParams, _, _>(
        CapabilityMeta::new(
            "nomi_channel_delete_plugin",
            "channel",
            "Permanently delete a bot channel: stops the bot, removes all its sessions, and deletes the database row. Conversations created through this bot survive.",
            DangerTier::Destructive,
        )
        .deny_on(&[Surface::Channel]),
        |deps, _ctx, p| delete_plugin(deps, p),
    ));

    // 5. Test bot credentials (sensitive — sends a network probe).
    out.push(Capability::new::<TestPluginParams, _, _>(
        CapabilityMeta::new(
            "nomi_channel_test_plugin",
            "channel",
            "Test IM bot credentials by probing the remote platform API. Returns the resolved bot_username on success. Does NOT persist any config changes.",
            DangerTier::Sensitive,
        ),
        |deps, _ctx, p| test_plugin(deps, p),
    ));

    // 6. List pending pairing/authorization requests (read-only).
    out.push(Capability::new::<ListPairingsParams, _, _>(
        CapabilityMeta::new(
            "nomi_channel_list_pairings",
            "channel",
            "List pending pairing requests from IM users waiting to be authorized to interact with the bot.",
            DangerTier::Read,
        ),
        |deps, _ctx, p| list_pairings(deps, p),
    ));

    // 7. Approve a pairing request (write).
    out.push(Capability::new::<ApprovePairingParams, _, _>(
        CapabilityMeta::new(
            "nomi_channel_approve_pairing",
            "channel",
            "Approve a pending pairing request, granting the IM user authorization to interact with the bot.",
            DangerTier::Write,
        ),
        |deps, _ctx, p| approve_pairing(deps, p),
    ));

    // 8. Reject a pairing request (write).
    out.push(Capability::new::<RejectPairingParams, _, _>(
        CapabilityMeta::new(
            "nomi_channel_reject_pairing",
            "channel",
            "Reject a pending pairing request, denying the IM user access to the bot.",
            DangerTier::Write,
        ),
        |deps, _ctx, p| reject_pairing(deps, p),
    ));

    // 9. List authorized users (read-only).
    out.push(Capability::new::<ListUsersParams, _, _>(
        CapabilityMeta::new(
            "nomi_channel_list_users",
            "channel",
            "List all authorized IM users across all channel bots, including their platform info and last activity.",
            DangerTier::Read,
        ),
        |deps, _ctx, p| list_users(deps, p),
    ));

    // 10. Revoke an authorized user (destructive — deletes access + sessions).
    out.push(Capability::new::<RevokeUserParams, _, _>(
        CapabilityMeta::new(
            "nomi_channel_revoke_user",
            "channel",
            "Revoke an authorized user's access: cleans up all their sessions and deletes the authorization record.",
            DangerTier::Destructive,
        ),
        |deps, _ctx, p| revoke_user(deps, p),
    ));

    // 11. Bind a channel bot to a companion (write).
    out.push(Capability::new::<SetCompanionParams, _, _>(
        CapabilityMeta::new(
            "nomi_channel_set_companion",
            "channel",
            "Bind (or clear) the companion that handles a channel bot's conversations. Per-channel binding (plugin_id) is preferred; platform-wide binding is the legacy fallback. Clears sessions so the next message uses the new companion.",
            DangerTier::Write,
        ),
        |deps, _ctx, p| set_companion(deps, p),
    ));
}

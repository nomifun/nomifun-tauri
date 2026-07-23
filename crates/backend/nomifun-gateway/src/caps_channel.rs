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
//! `Arc<ChannelSettingsService>`, `Option<Arc<dyn ChannelAgentProfile>>`, and
//! `ExtensionRegistry`.

use std::sync::Arc;

use nomifun_common::{ChannelPluginId, ChannelUserId, CompanionId};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::deps::GatewayDeps;
use crate::registry::{Capability, CapabilityMeta, DangerTier, Surface};
use crate::server::ok;

// ── param structs ────────────────────────────────────────────────────────

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct ListPluginsParams {}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct EnablePluginParams {
    /// Platform type of the bot to create/update. Required when creating a new
    /// bot (omit `plugin_id`). Supported builtins: "telegram", "discord",
    /// "slack", "lark", "dingtalk", "weixin", "matrix", "mattermost",
    /// "twitch", "nostr", "qqbot". Extension plugins use their registered id.
    #[serde(default)]
    plugin_type: Option<String>,

    /// Existing channel business ID to reconfigure. If omitted, a new bot is
    /// created (requires `plugin_type`). When provided, updates config in
    /// place.
    #[serde(default)]
    #[schemars(schema_with = "crate::id_schema::optional_canonical_uuid_v7_schema")]
    plugin_id: Option<ChannelPluginId>,

    /// Companion id to bind this bot to. Messages arriving on the channel will
    /// be routed to this companion. Omit or pass null to use the default
    /// companion.
    #[serde(default)]
    #[schemars(schema_with = "crate::id_schema::optional_canonical_uuid_v7_schema")]
    companion_id: Option<CompanionId>,

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
#[serde(deny_unknown_fields)]
struct DisablePluginParams {
    /// The stable channel plugin ID of the bot to disable. The bot is
    /// stopped but its configuration is retained for re-enabling.
    #[schemars(schema_with = "crate::id_schema::canonical_uuid_v7_schema")]
    plugin_id: ChannelPluginId,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct DeletePluginParams {
    /// The stable channel plugin ID of the bot to permanently delete. This
    /// stops the bot, removes all its sessions, and deletes the database row.
    /// Conversations created through this bot are NOT deleted.
    #[schemars(schema_with = "crate::id_schema::canonical_uuid_v7_schema")]
    plugin_id: ChannelPluginId,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct TestPluginParams {
    /// Stable channel implementation key for the bot being tested (e.g.
    /// "telegram", "lark", "discord"). This is not a persisted row id.
    plugin_type: String,

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
#[serde(deny_unknown_fields)]
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
#[serde(deny_unknown_fields)]
struct ListPairingsParams {}

/// Send a local file/image to the user through the IM channel they are on.
#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct SendFileParams {
    /// Absolute path to the local file to send (e.g. an image you generated or
    /// found on disk). Images are delivered as photos, other files as documents.
    path: String,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct ApprovePairingParams {
    /// The pairing code to approve (from nomi_channel_list_pairings).
    code: String,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct RejectPairingParams {
    /// The pairing code to reject (from nomi_channel_list_pairings).
    code: String,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct ListUsersParams {}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct RevokeUserParams {
    /// The stable channel-user ID to revoke (from nomi_channel_list_users). This
    /// removes authorization and clears all sessions for this user.
    #[schemars(schema_with = "crate::id_schema::canonical_uuid_v7_schema")]
    channel_user_id: ChannelUserId,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct SetCompanionParams {
    /// Target one bot/channel by its stable business ID.
    #[schemars(schema_with = "crate::id_schema::canonical_uuid_v7_schema")]
    plugin_id: ChannelPluginId,

    /// Companion id to bind. Pass null or omit to clear the binding (reverts
    /// to the default companion).
    #[serde(default)]
    #[schemars(schema_with = "crate::id_schema::optional_canonical_uuid_v7_schema")]
    companion_id: Option<CompanionId>,
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
    if let Some(companion_id) = p.companion_id.as_ref().map(CompanionId::as_str) {
        if let Some(profile) = &deps.channel_state.channel_agent_profile {
            if !profile.companion_exists(companion_id).await {
                return json!({ "error": format!("companion '{}' not found", companion_id) });
            }
        }
    }

    let spec = EnableChannelSpec {
        plugin_id: p.plugin_id.map(ChannelPluginId::into_string),
        plugin_type: p.plugin_type.clone(),
        companion_id: p.companion_id.map(CompanionId::into_string),
        public_agent_id: None,
    };

    match deps
        .channel_state
        .manager
        .enable_plugin(&spec, &p.config, deps.channel_state.plugin_factory.as_ref())
        .await
    {
        Ok(plugin_id) => ok(json!({
            "plugin_id": plugin_id,
            "note": "bot enabled; use nomi_channel_test_plugin to verify credentials connect successfully"
        })),
        Err(e) => json!({ "error": e.to_string() }),
    }
}

async fn disable_plugin(deps: Arc<GatewayDeps>, p: DisablePluginParams) -> Value {
    match deps.channel_state.manager.disable_plugin(p.plugin_id.as_str()).await {
        Ok(()) => ok(json!({ "disabled": true, "plugin_id": p.plugin_id })),
        Err(e) => json!({ "error": e.to_string() }),
    }
}

async fn delete_plugin(deps: Arc<GatewayDeps>, p: DeletePluginParams) -> Value {
    match deps.channel_state.manager.delete_channel(p.plugin_id.as_str()).await {
        Ok(()) => json!({ "result": format!("channel {} permanently deleted", p.plugin_id) }),
        Err(e) => json!({ "error": e.to_string() }),
    }
}

async fn test_plugin(deps: Arc<GatewayDeps>, p: TestPluginParams) -> Value {
    use nomifun_channel::types::{PluginConfig, PluginCredentials};

    let mut credentials = PluginCredentials::default();
    let extra = p.extra_config.as_ref();

    match p.plugin_type.as_str() {
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
        "wecom" => {
            credentials.bot_id = Some(p.token.clone());
            if let Some(e) = extra {
                credentials.secret = e.app_secret.clone();
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
        .test_plugin(&p.plugin_type, config, deps.channel_state.plugin_factory.as_ref())
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
                        "channel_plugin_id": r.channel_plugin_id,
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

/// Ingest a local file as a workshop asset and return its id so the channel
/// relay delivers it to the user via the platform's media-send path — the SAME
/// pipeline that ships AI-generated images through the channel. Delivery only
/// happens when the current turn arrived over an IM channel (a relay is running
/// for it); on a plain desktop turn the asset is prepared but nothing is pushed.
async fn send_file(deps: Arc<GatewayDeps>, p: SendFileParams) -> Value {
    use std::path::Path;

    let path = p.path.trim();
    let meta = match std::fs::metadata(path) {
        Ok(m) => m,
        Err(e) => return json!({ "error": format!("cannot access file '{path}': {e}") }),
    };
    if !meta.is_file() {
        return json!({ "error": format!("'{path}' is not a file") });
    }
    // Platform media limits vary; 50MB is a generous upper bound.
    const MAX_BYTES: u64 = 50 * 1024 * 1024;
    if meta.len() > MAX_BYTES {
        return json!({ "error": format!("file too large: {} bytes (max {MAX_BYTES})", meta.len()) });
    }
    let bytes = match std::fs::read(path) {
        Ok(b) => b,
        Err(e) => return json!({ "error": format!("failed to read '{path}': {e}") }),
    };
    let file_name = Path::new(path)
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("file")
        .to_owned();
    let mime = mime_for_file_name(&file_name);
    let origin = json!({ "source": "nomi_channel_send_file", "path": path });

    match deps
        .workshop_service
        .ingest_asset_bytes(bytes, mime, &file_name, false, Some(origin))
        .await
    {
        // `result_asset_ids` is the SAME signal the channel relay already keys off
        // (it resolves the asset bytes by id and uploads them via the plugin's
        // media-send). The key name must match so the relay's detector picks it up.
        Ok(row) => send_file_success(row, file_name),
        Err(e) => json!({ "error": format!("failed to prepare file for sending: {e}") }),
    }
}

fn send_file_success(row: nomifun_db::WorkshopAssetRow, file_name: String) -> Value {
    ok(json!({
        "result_asset_ids": [row.asset_id],
        "file_name": file_name,
        "delivered": true,
        "note": "文件已通过当前渠道发送给用户。"
    }))
}

/// Best-effort MIME from a file-name extension — drives image-vs-document
/// delivery downstream (image/* → photo, else document). Unknown → octet-stream.
fn mime_for_file_name(name: &str) -> &'static str {
    match name.rsplit('.').next().unwrap_or("").to_ascii_lowercase().as_str() {
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "webp" => "image/webp",
        "gif" => "image/gif",
        "bmp" => "image/bmp",
        "svg" => "image/svg+xml",
        "pdf" => "application/pdf",
        "txt" | "log" | "md" => "text/plain",
        "json" => "application/json",
        "csv" => "text/csv",
        "zip" => "application/zip",
        "mp4" => "video/mp4",
        "webm" => "video/webm",
        "mp3" => "audio/mpeg",
        "wav" => "audio/wav",
        _ => "application/octet-stream",
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
                        "channel_user_id": r.channel_user_id,
                        "platform_user_id": r.platform_user_id,
                        "platform_type": r.platform_type,
                        "channel_plugin_id": r.channel_plugin_id,
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
        .cleanup_user_sessions(p.channel_user_id.as_str())
        .await
    {
        return json!({ "error": format!("failed to clean sessions: {}", e) });
    }
    match deps
        .channel_state
        .repo
        .delete_user(p.channel_user_id.as_str())
        .await
    {
        Ok(()) => json!({ "result": format!("user {} revoked", p.channel_user_id) }),
        Err(e) => json!({ "error": e.to_string() }),
    }
}

async fn set_companion(deps: Arc<GatewayDeps>, p: SetCompanionParams) -> Value {
    let companion_id = p.companion_id.as_ref().map(CompanionId::as_str);

    // Validate companion existence if binding (not clearing).
    if let Some(cid) = companion_id {
        if let Some(profile) = &deps.channel_state.channel_agent_profile {
            if !profile.companion_exists(cid).await {
                return json!({ "error": format!("companion '{}' not found", cid) });
            }
        }
    }

    match deps
        .channel_state
        .manager
        .rebind_channel_companion(p.plugin_id.as_str(), companion_id)
        .await
    {
        Ok(()) => ok(json!({
            "bound": true,
            "plugin_id": p.plugin_id,
            "companion_id": companion_id,
            "note": "channel sessions cleared; next message starts fresh under new companion"
        })),
        Err(e) => json!({ "error": e.to_string() }),
    }
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
            "Bind (or clear) the companion that handles one channel bot's conversations, addressed by its stable plugin_id. Clears that bot's sessions so the next message uses the new companion.",
            DangerTier::Write,
        ),
        |deps, _ctx, p| set_companion(deps, p),
    ));

    // 12. Send a local file/image to the user through the current IM channel (write).
    out.push(Capability::new::<SendFileParams, _, _>(
        CapabilityMeta::new(
            "nomi_channel_send_file",
            "channel",
            "Send a local file or image to the user you are chatting with, through their IM channel (WeChat/Telegram/etc.). Give an absolute file path; images are delivered as photos, other files as documents. Use THIS when the user asks you to send/deliver a file or picture to them — do NOT try to browse the web, download, or paste a file path as text. Only delivers when the current conversation arrived over an IM channel.",
            DangerTier::Write,
        ),
        |deps, _ctx, p| send_file(deps, p),
    ));
}

#[cfg(test)]
mod send_file_tests {
    use super::{mime_for_file_name, send_file_success};
    use crate::registry::{Registry, Surface};
    use nomifun_db::WorkshopAssetRow;

    #[test]
    fn send_file_is_visible_on_channel_and_desktop() {
        let reg = Registry::global();
        // Write-tier → allowed on Channel (where delivery happens) and Desktop.
        assert!(reg.tool_visible(Surface::Channel, "nomi_channel_send_file"));
        assert!(reg.tool_visible(Surface::Desktop, "nomi_channel_send_file"));
    }

    #[test]
    fn mime_mapping_drives_image_vs_document() {
        assert_eq!(mime_for_file_name("1.png"), "image/png");
        assert_eq!(mime_for_file_name("cat.JPG"), "image/jpeg");
        assert_eq!(mime_for_file_name("clip.webp"), "image/webp");
        assert_eq!(mime_for_file_name("report.pdf"), "application/pdf");
        assert_eq!(mime_for_file_name("noext"), "application/octet-stream");
        assert_eq!(mime_for_file_name("archive.ZIP"), "application/zip");
    }

    #[test]
    fn send_file_result_uses_asset_id_not_database_row_id() {
        let asset_id = "0190f5fe-7c00-7a00-8000-000000000001";
        let row = WorkshopAssetRow {
            id: 42,
            asset_id: asset_id.into(),
            kind: "image".into(),
            title: "file.png".into(),
            collection: None,
            tags: "[]".into(),
            rel_path: Some(format!("workshop/assets/{asset_id}.png")),
            thumb_rel_path: None,
            mime: Some("image/png".into()),
            width: None,
            height: None,
            bytes: Some(3),
            text_content: None,
            in_library: false,
            origin: None,
            created_at: 1,
            updated_at: 1,
        };
        let result = send_file_success(row, "file.png".into());

        assert_eq!(
            result["result"]["result_asset_ids"],
            serde_json::json!([asset_id])
        );
        assert_ne!(
            result["result"]["result_asset_ids"],
            serde_json::json!([42])
        );
        assert_eq!(result["result"]["file_name"], "file.png");
        assert_eq!(result["result"]["delivered"], true);
    }
}

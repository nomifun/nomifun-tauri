use nomifun_common::TimestampMs;
use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// A. Plugin management — Request DTOs
// ---------------------------------------------------------------------------

/// Request body for `POST /api/channel/plugins/enable`.
///
/// Enables a channel plugin with the given configuration. The `config`
/// field is a JSON object containing platform-specific credentials and
/// connection options (`{ credentials, config }`).
///
/// Addressing: a canonical `plugin_id` updates an existing channel row. An
/// absent `plugin_id` with `plugin_type` creates a new bot channel. Empty
/// strings are invalid IDs and are never treated as create mode.
/// `companion_id` binds the bot to a companion.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EnablePluginRequest {
    /// Stable channel plugin business identity.
    #[serde(
        default,
        deserialize_with = "crate::serde_util::deserialize_optional_channel_plugin_id"
    )]
    pub plugin_id: Option<String>,
    pub config: serde_json::Value,
    #[serde(default)]
    pub plugin_type: Option<String>,
    #[serde(
        default,
        deserialize_with = "crate::serde_util::deserialize_optional_companion_id"
    )]
    pub companion_id: Option<String>,
    /// 对外伙伴 (public agent) to bind this bot to. Mutually exclusive with
    /// `companion_id` — a bot serves EITHER a companion OR a public agent.
    #[serde(
        default,
        deserialize_with = "crate::serde_util::deserialize_optional_public_agent_id"
    )]
    pub public_agent_id: Option<String>,
}

/// Request body for `POST /api/channel/plugins/disable`.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DisablePluginRequest {
    #[serde(deserialize_with = "crate::serde_util::deserialize_channel_plugin_id")]
    pub plugin_id: String,
}

/// Request body for `POST /api/channel/plugins/test`.
///
/// Tests plugin credentials without persisting. For platforms that need
/// additional config (e.g., Lark requires `appId` + `appSecret`),
/// pass them in `extra_config`.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TestPluginRequest {
    /// Stable channel implementation key (for example `telegram` or `lark`).
    /// This is not a persisted channel entity ID.
    pub plugin_type: String,
    pub token: String,
    #[serde(default)]
    pub extra_config: Option<TestPluginExtraConfig>,
}

/// Extra configuration fields for plugin credential testing.
///
/// Used by platforms that require more than a single token
/// (e.g., Lark needs `app_id` + `app_secret`).
#[derive(Debug, Clone, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct TestPluginExtraConfig {
    #[serde(default)]
    pub app_id: Option<String>,
    #[serde(default)]
    pub app_secret: Option<String>,
    // Slack: app-level token (bot token goes in `token`).
    #[serde(default)]
    pub app_token: Option<String>,
    // Matrix: homeserver + bot mxid (access token goes in `token`).
    #[serde(default)]
    pub homeserver_url: Option<String>,
    #[serde(default)]
    pub user_id: Option<String>,
    // Mattermost: server base URL (bot token goes in `token`).
    #[serde(default)]
    pub server_url: Option<String>,
    // Nostr: comma-separated relay URLs (private key goes in `token`).
    #[serde(default)]
    pub nostr_relays: Option<String>,
}

// ---------------------------------------------------------------------------
// B. Pairing management — Request DTOs
// ---------------------------------------------------------------------------

/// Request body for `POST /api/channel/pairings/approve`.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ApprovePairingRequest {
    pub code: String,
}

/// Request body for `POST /api/channel/pairings/reject`.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RejectPairingRequest {
    pub code: String,
}

// ---------------------------------------------------------------------------
// C. User management — Request DTOs
// ---------------------------------------------------------------------------

/// Request body for `POST /api/channel/users/revoke`.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RevokeUserRequest {
    #[serde(deserialize_with = "crate::serde_util::deserialize_channel_user_id")]
    pub channel_user_id: String,
}

// ---------------------------------------------------------------------------
// D. Settings — Request DTOs
// ---------------------------------------------------------------------------

/// Request body for `POST /api/channel/settings/sync`.
///
/// Invalidates all channel sessions for the given platform so they
/// are recreated with the latest agent/model configuration from
/// `client_preferences` on the next incoming message.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SyncChannelSettingsRequest {
    pub platform: String,
}

// ---------------------------------------------------------------------------
// E. Plugin management — Response DTOs
// ---------------------------------------------------------------------------

/// Plugin status returned by `GET /api/channel/plugins`.
///
/// Corresponds to `IChannelPluginStatus` in the original TypeScript.
/// Excludes encrypted config data for security.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct PluginStatusResponse {
    #[serde(deserialize_with = "crate::serde_util::deserialize_channel_plugin_id")]
    pub plugin_id: String,
    #[serde(rename = "type")]
    pub plugin_type: String,
    pub name: String,
    pub enabled: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_connected: Option<TimestampMs>,
    /// Companion bound to this bot channel (one bot ↔ at most one companion).
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "crate::serde_util::deserialize_optional_companion_id"
    )]
    pub companion_id: Option<String>,
    /// 对外伙伴 (public agent) bound to this bot channel. Row-level mutually
    /// exclusive with `companion_id`.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "crate::serde_util::deserialize_optional_public_agent_id"
    )]
    pub public_agent_id: Option<String>,
    /// Platform-level bot identity (lark app_id, telegram bot id, ...).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bot_key: Option<String>,
    pub created_at: TimestampMs,
    pub updated_at: TimestampMs,
    pub connected: bool,
    pub has_token: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bot_username: Option<String>,
    pub active_users: i64,
}

/// Result of a plugin credential test.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct TestPluginResponse {
    pub success: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bot_username: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// Generic success/error response for channel bridge operations.
///
/// Used by enable/disable plugin, approve/reject pairing, revoke user,
/// and sync settings endpoints.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct BridgeResponse {
    pub success: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// Fixed response for `POST /api/channel/plugins/enable`.
///
/// The stable business ID is never smuggled through the human-readable
/// `message` field used by generic bridge operations.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct EnablePluginResponse {
    pub success: bool,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "crate::serde_util::deserialize_optional_channel_plugin_id"
    )]
    pub plugin_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

// ---------------------------------------------------------------------------
// F. Pairing — Response DTOs
// ---------------------------------------------------------------------------

/// Pending pairing request returned by `GET /api/channel/pairings`.
///
/// Corresponds to `IChannelPairingRequest`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct PairingRequestResponse {
    pub code: String,
    pub platform_user_id: String,
    pub platform_type: String,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "crate::serde_util::deserialize_optional_channel_plugin_id"
    )]
    pub channel_plugin_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
    pub requested_at: TimestampMs,
    pub expires_at: TimestampMs,
}

// ---------------------------------------------------------------------------
// G. User — Response DTOs
// ---------------------------------------------------------------------------

/// Authorized channel user returned by `GET /api/channel/users`.
///
/// Corresponds to `IChannelUser`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ChannelUserResponse {
    #[serde(deserialize_with = "crate::serde_util::deserialize_channel_user_id")]
    pub channel_user_id: String,
    pub platform_user_id: String,
    pub platform_type: String,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "crate::serde_util::deserialize_optional_channel_plugin_id"
    )]
    pub channel_plugin_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
    pub authorized_at: TimestampMs,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_active: Option<TimestampMs>,
}

// ---------------------------------------------------------------------------
// H. Session — Response DTOs
// ---------------------------------------------------------------------------

/// Active channel session returned by `GET /api/channel/sessions`.
///
/// Corresponds to `IChannelSession`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ChannelSessionResponse {
    /// Stable business identity (`channel_sessions.channel_session_id`).
    #[serde(deserialize_with = "crate::serde_util::deserialize_channel_session_id")]
    pub channel_session_id: String,
    #[serde(deserialize_with = "crate::serde_util::deserialize_channel_user_id")]
    pub channel_user_id: String,
    pub agent_type: String,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "crate::serde_util::deserialize_optional_conversation_id"
    )]
    pub conversation_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub workspace: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub chat_id: Option<String>,
    /// Channel business identity this session arrived through.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "crate::serde_util::deserialize_optional_channel_plugin_id"
    )]
    pub channel_plugin_id: Option<String>,
    pub created_at: TimestampMs,
    pub last_activity: TimestampMs,
}

// ---------------------------------------------------------------------------
// I. WebSocket event payloads
// ---------------------------------------------------------------------------

/// Payload for `channel.pairing-requested` WebSocket event.
///
/// Pushed when an IM user sends their first message and triggers the
/// pairing authorization flow.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct PairingRequestedPayload {
    pub code: String,
    pub platform_user_id: String,
    pub platform_type: String,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "crate::serde_util::deserialize_optional_channel_plugin_id"
    )]
    pub channel_plugin_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
    pub expires_at: TimestampMs,
}

/// Payload for `channel.plugin-status-changed` WebSocket event.
///
/// Pushed when a plugin starts, stops, or encounters an error.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct PluginStatusChangedPayload {
    #[serde(deserialize_with = "crate::serde_util::deserialize_channel_plugin_id")]
    pub plugin_id: String,
    pub status: PluginStatusResponse,
}

/// Payload for `channel.user-authorized` WebSocket event.
///
/// Pushed after a pairing code is approved and the user record is created.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct UserAuthorizedPayload {
    #[serde(deserialize_with = "crate::serde_util::deserialize_channel_user_id")]
    pub channel_user_id: String,
    pub platform_user_id: String,
    pub platform_type: String,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "crate::serde_util::deserialize_optional_channel_plugin_id"
    )]
    pub channel_plugin_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const CHANNEL_ID: &str = "018f1234-5678-7abc-8def-012345678990";
    const COMPANION_ID: &str = "018f1234-5678-7abc-8def-012345678991";
    const CHANNEL_USER_ID: &str = "018f1234-5678-7abc-8def-012345678992";
    const CHANNEL_SESSION_ID: &str = "018f1234-5678-7abc-8def-012345678993";
    const CONVERSATION_ID: &str = "0190f5fe-7c00-7a00-8000-000000000001";

    // -- A. Plugin management requests ----------------------------------------

    #[test]
    fn test_enable_plugin_request_deserialize() {
        let raw = json!({
            "plugin_id": CHANNEL_ID,
            "config": {
                "credentials": { "token": "bot123:ABC" },
                "config": { "mode": "polling" }
            }
        });
        let req: EnablePluginRequest = serde_json::from_value(raw).unwrap();
        assert_eq!(req.plugin_id, Some(CHANNEL_ID.into()));
        assert!(req.plugin_type.is_none());
        assert!(req.companion_id.is_none());
        assert_eq!(req.config["credentials"]["token"], "bot123:ABC");
        assert_eq!(req.config["config"]["mode"], "polling");
    }

    #[test]
    fn test_enable_plugin_request_missing_plugin_id_is_create_mode() {
        // plugin_id became optional: absent id + plugin_type/companion_id is the
        // per-companion create path. Deserialization must accept it.
        let raw = json!({ "config": {} });
        let req: EnablePluginRequest = serde_json::from_value(raw).unwrap();
        assert!(req.plugin_id.is_none());
        assert!(req.plugin_type.is_none());
        assert!(req.companion_id.is_none());
    }

    #[test]
    fn test_enable_plugin_request_create_mode_with_companion() {
        let raw = json!({
            "plugin_type": "lark",
            "companion_id": COMPANION_ID,
            "config": { "credentials": { "app_id": "cli_a" } }
        });
        let req: EnablePluginRequest = serde_json::from_value(raw).unwrap();
        assert!(req.plugin_id.is_none());
        assert_eq!(req.plugin_type.as_deref(), Some("lark"));
        assert_eq!(req.companion_id.as_deref(), Some(COMPANION_ID));
    }

    #[test]
    fn test_enable_plugin_request_missing_config() {
        let raw = json!({ "plugin_id": CHANNEL_ID });
        let result = serde_json::from_value::<EnablePluginRequest>(raw);
        assert!(result.is_err());
    }

    #[test]
    fn test_disable_plugin_request_deserialize() {
        let raw = json!({ "plugin_id": CHANNEL_ID });
        let req: DisablePluginRequest = serde_json::from_value(raw).unwrap();
        assert_eq!(req.plugin_id, CHANNEL_ID);
    }

    #[test]
    fn test_disable_plugin_request_missing_plugin_id() {
        let raw = json!({});
        let result = serde_json::from_value::<DisablePluginRequest>(raw);
        assert!(result.is_err());
    }

    #[test]
    fn test_test_plugin_request_telegram() {
        let raw = json!({
            "plugin_type": "telegram",
            "token": "bot123:ABC"
        });
        let req: TestPluginRequest = serde_json::from_value(raw).unwrap();
        assert_eq!(req.plugin_type, "telegram");
        assert_eq!(req.token, "bot123:ABC");
        assert!(req.extra_config.is_none());
    }

    #[test]
    fn test_test_plugin_request_lark_with_extra_config() {
        let raw = json!({
            "plugin_type": "lark",
            "token": "xxx",
            "extra_config": {
                "app_id": "cli_abc",
                "app_secret": "secret123"
            }
        });
        let req: TestPluginRequest = serde_json::from_value(raw).unwrap();
        assert_eq!(req.plugin_type, "lark");
        let extra = req.extra_config.unwrap();
        assert_eq!(extra.app_id.as_deref(), Some("cli_abc"));
        assert_eq!(extra.app_secret.as_deref(), Some("secret123"));
    }

    #[test]
    fn test_test_plugin_request_missing_token() {
        let raw = json!({ "plugin_type": "telegram" });
        let result = serde_json::from_value::<TestPluginRequest>(raw);
        assert!(result.is_err());
    }

    #[test]
    fn test_test_plugin_extra_config_partial() {
        let raw = json!({
            "plugin_type": "lark",
            "token": "xxx",
            "extra_config": { "app_id": "cli_abc" }
        });
        let req: TestPluginRequest = serde_json::from_value(raw).unwrap();
        let extra = req.extra_config.unwrap();
        assert_eq!(extra.app_id.as_deref(), Some("cli_abc"));
        assert!(extra.app_secret.is_none());
    }

    // -- B. Pairing requests --------------------------------------------------

    #[test]
    fn test_approve_pairing_request_deserialize() {
        let raw = json!({ "code": "123456" });
        let req: ApprovePairingRequest = serde_json::from_value(raw).unwrap();
        assert_eq!(req.code, "123456");
    }

    #[test]
    fn test_approve_pairing_request_missing_code() {
        let raw = json!({});
        let result = serde_json::from_value::<ApprovePairingRequest>(raw);
        assert!(result.is_err());
    }

    #[test]
    fn test_reject_pairing_request_deserialize() {
        let raw = json!({ "code": "654321" });
        let req: RejectPairingRequest = serde_json::from_value(raw).unwrap();
        assert_eq!(req.code, "654321");
    }

    // -- C. User management requests ------------------------------------------

    #[test]
    fn test_revoke_user_request_deserialize() {
        let raw = json!({ "channel_user_id": CHANNEL_USER_ID });
        let req: RevokeUserRequest = serde_json::from_value(raw).unwrap();
        assert_eq!(req.channel_user_id, CHANNEL_USER_ID);
    }

    #[test]
    fn test_revoke_user_request_missing_user_id() {
        let raw = json!({});
        let result = serde_json::from_value::<RevokeUserRequest>(raw);
        assert!(result.is_err());
    }

    // -- D. Settings requests -------------------------------------------------

    #[test]
    fn test_sync_settings_request_deserialize() {
        let raw = json!({ "platform": "telegram" });
        let req: SyncChannelSettingsRequest = serde_json::from_value(raw).unwrap();
        assert_eq!(req.platform, "telegram");
    }

    #[test]
    fn test_sync_settings_request_missing_platform() {
        let raw = json!({});
        let result = serde_json::from_value::<SyncChannelSettingsRequest>(raw);
        assert!(result.is_err());
    }

    // -- E. Plugin status response --------------------------------------------

    #[test]
    fn test_plugin_status_response_serde() {
        let resp = PluginStatusResponse {
            plugin_id: CHANNEL_ID.into(),
            plugin_type: "telegram".into(),
            name: "Telegram Bot".into(),
            enabled: true,
            status: Some("running".into()),
            last_connected: Some(1700000000000),
            companion_id: Some(COMPANION_ID.into()),
            public_agent_id: None,
            bot_key: Some("123456".into()),
            created_at: 1699000000000,
            updated_at: 1700000000000,
            connected: true,
            has_token: true,
            bot_username: Some("my_bot".into()),
            active_users: 5,
        };
        let json = serde_json::to_value(&resp).unwrap();
        assert_eq!(json["plugin_id"], CHANNEL_ID);
        assert_eq!(json["companion_id"], COMPANION_ID);
        assert_eq!(json["bot_key"], "123456");
        assert_eq!(json["type"], "telegram");
        assert_eq!(json["name"], "Telegram Bot");
        assert_eq!(json["enabled"], true);
        assert_eq!(json["status"], "running");
        assert_eq!(json["last_connected"], 1700000000000_i64);
        assert_eq!(json["created_at"], 1699000000000_i64);
        assert_eq!(json["updated_at"], 1700000000000_i64);
        assert_eq!(json["connected"], true);
        assert_eq!(json["has_token"], true);
        assert_eq!(json["bot_username"], "my_bot");
        assert_eq!(json["active_users"], 5);
    }

    #[test]
    fn test_plugin_status_response_optional_fields_omitted() {
        let resp = PluginStatusResponse {
            plugin_id: CHANNEL_ID.into(),
            plugin_type: "lark".into(),
            name: "Lark Bot".into(),
            enabled: false,
            status: None,
            last_connected: None,
            companion_id: None,
            public_agent_id: None,
            bot_key: None,
            created_at: 1699000000000,
            updated_at: 1699000000000,
            connected: false,
            has_token: false,
            bot_username: None,
            active_users: 0,
        };
        let json = serde_json::to_value(&resp).unwrap();
        assert!(json.get("status").is_none());
        assert!(json.get("last_connected").is_none());
        assert!(json.get("companion_id").is_none());
        assert!(json.get("bot_key").is_none());
        assert!(json.get("bot_username").is_none());
    }

    // -- E. Test plugin response ----------------------------------------------

    #[test]
    fn test_test_plugin_response_success() {
        let resp = TestPluginResponse {
            success: true,
            bot_username: Some("my_bot".into()),
            error: None,
        };
        let json = serde_json::to_value(&resp).unwrap();
        assert_eq!(json["success"], true);
        assert_eq!(json["bot_username"], "my_bot");
        assert!(json.get("error").is_none());
    }

    #[test]
    fn test_test_plugin_response_failure() {
        let resp = TestPluginResponse {
            success: false,
            bot_username: None,
            error: Some("Invalid token".into()),
        };
        let json = serde_json::to_value(&resp).unwrap();
        assert_eq!(json["success"], false);
        assert!(json.get("bot_username").is_none());
        assert_eq!(json["error"], "Invalid token");
    }

    // -- E. Bridge response ---------------------------------------------------

    #[test]
    fn test_bridge_response_success() {
        let resp = BridgeResponse {
            success: true,
            message: Some("Plugin enabled".into()),
            error: None,
        };
        let json = serde_json::to_value(&resp).unwrap();
        assert_eq!(json["success"], true);
        assert_eq!(json["message"], "Plugin enabled");
        assert!(json.get("error").is_none());
    }

    #[test]
    fn test_bridge_response_failure() {
        let resp = BridgeResponse {
            success: false,
            message: None,
            error: Some("Connection refused".into()),
        };
        let json = serde_json::to_value(&resp).unwrap();
        assert_eq!(json["success"], false);
        assert!(json.get("message").is_none());
        assert_eq!(json["error"], "Connection refused");
    }

    #[test]
    fn test_bridge_response_minimal() {
        let resp = BridgeResponse {
            success: true,
            message: None,
            error: None,
        };
        let json = serde_json::to_value(&resp).unwrap();
        assert_eq!(json["success"], true);
        assert!(json.get("message").is_none());
        assert!(json.get("error").is_none());
    }

    // -- F. Pairing response --------------------------------------------------

    #[test]
    fn test_pairing_request_response_serde() {
        let resp = PairingRequestResponse {
            code: "123456".into(),
            platform_user_id: "tg_user_42".into(),
            platform_type: "telegram".into(),
            channel_plugin_id: Some(CHANNEL_ID.into()),
            display_name: Some("Alice".into()),
            requested_at: 1700000000000,
            expires_at: 1700000600000,
        };
        let json = serde_json::to_value(&resp).unwrap();
        assert_eq!(json["code"], "123456");
        assert_eq!(json["platform_user_id"], "tg_user_42");
        assert_eq!(json["platform_type"], "telegram");
        assert_eq!(json["channel_plugin_id"], CHANNEL_ID);
        assert_eq!(json["display_name"], "Alice");
        assert_eq!(json["requested_at"], 1700000000000_i64);
        assert_eq!(json["expires_at"], 1700000600000_i64);
    }

    #[test]
    fn test_pairing_request_response_no_display_name() {
        let resp = PairingRequestResponse {
            code: "999999".into(),
            platform_user_id: "user_1".into(),
            platform_type: "lark".into(),
            channel_plugin_id: None,
            display_name: None,
            requested_at: 1700000000000,
            expires_at: 1700000600000,
        };
        let json = serde_json::to_value(&resp).unwrap();
        assert!(json.get("display_name").is_none());
        assert!(json.get("channel_plugin_id").is_none());
    }

    // -- G. User response -----------------------------------------------------

    #[test]
    fn test_channel_user_response_serde() {
        let resp = ChannelUserResponse {
            channel_user_id: CHANNEL_USER_ID.into(),
            platform_user_id: "tg_42".into(),
            platform_type: "telegram".into(),
            channel_plugin_id: Some(CHANNEL_ID.into()),
            display_name: Some("Bob".into()),
            authorized_at: 1700000000000,
            last_active: Some(1700001000000),
        };
        let json = serde_json::to_value(&resp).unwrap();
        assert_eq!(json["channel_user_id"], CHANNEL_USER_ID);
        assert_eq!(json["platform_user_id"], "tg_42");
        assert_eq!(json["platform_type"], "telegram");
        assert_eq!(json["channel_plugin_id"], CHANNEL_ID);
        assert_eq!(json["display_name"], "Bob");
        assert_eq!(json["authorized_at"], 1700000000000_i64);
        assert_eq!(json["last_active"], 1700001000000_i64);
    }

    #[test]
    fn test_channel_user_response_optional_fields_omitted() {
        let resp = ChannelUserResponse {
            channel_user_id: CHANNEL_USER_ID.into(),
            platform_user_id: "lark_1".into(),
            platform_type: "lark".into(),
            channel_plugin_id: None,
            display_name: None,
            authorized_at: 1700000000000,
            last_active: None,
        };
        let json = serde_json::to_value(&resp).unwrap();
        assert!(json.get("display_name").is_none());
        assert!(json.get("last_active").is_none());
        assert!(json.get("channel_plugin_id").is_none());
    }

    // -- H. Session response --------------------------------------------------

    #[test]
    fn test_channel_session_response_serde() {
        let resp = ChannelSessionResponse {
            channel_session_id: CHANNEL_SESSION_ID.into(),
            channel_user_id: CHANNEL_USER_ID.into(),
            agent_type: "gemini".into(),
            conversation_id: Some("0190f5fe-7c00-7a00-8000-000000000001".into()),
            workspace: Some("/workspace".into()),
            chat_id: Some("chat_123".into()),
            channel_plugin_id: Some(CHANNEL_ID.into()),
            created_at: 1700000000000,
            last_activity: 1700001000000,
        };
        let json = serde_json::to_value(&resp).unwrap();
        assert_eq!(json["channel_session_id"], CHANNEL_SESSION_ID);
        assert_eq!(json["channel_plugin_id"], CHANNEL_ID);
        assert_eq!(json["channel_user_id"], CHANNEL_USER_ID);
        assert_eq!(json["agent_type"], "gemini");
        assert_eq!(
            json["conversation_id"],
            "0190f5fe-7c00-7a00-8000-000000000001"
        );
        assert_eq!(json["workspace"], "/workspace");
        assert_eq!(json["chat_id"], "chat_123");
        assert_eq!(json["created_at"], 1700000000000_i64);
        assert_eq!(json["last_activity"], 1700001000000_i64);
    }

    #[test]
    fn test_channel_session_response_optional_fields_omitted() {
        let resp = ChannelSessionResponse {
            channel_session_id: CHANNEL_SESSION_ID.into(),
            channel_user_id: CHANNEL_USER_ID.into(),
            agent_type: "acp".into(),
            conversation_id: None,
            workspace: None,
            chat_id: None,
            channel_plugin_id: None,
            created_at: 1700000000000,
            last_activity: 1700000000000,
        };
        let json = serde_json::to_value(&resp).unwrap();
        assert!(json.get("conversation_id").is_none());
        assert!(json.get("workspace").is_none());
        assert!(json.get("chat_id").is_none());
        assert!(json.get("channel_plugin_id").is_none());
    }

    // -- I. WebSocket event payloads ------------------------------------------

    #[test]
    fn test_pairing_requested_payload_serde() {
        let payload = PairingRequestedPayload {
            code: "123456".into(),
            platform_user_id: "tg_42".into(),
            platform_type: "telegram".into(),
            channel_plugin_id: Some(CHANNEL_ID.into()),
            display_name: Some("Alice".into()),
            expires_at: 1700000600000,
        };
        let json = serde_json::to_value(&payload).unwrap();
        assert_eq!(json["code"], "123456");
        assert_eq!(json["platform_user_id"], "tg_42");
        assert_eq!(json["platform_type"], "telegram");
        assert_eq!(json["channel_plugin_id"], CHANNEL_ID);
        assert_eq!(json["display_name"], "Alice");
        assert_eq!(json["expires_at"], 1700000600000_i64);
    }

    #[test]
    fn test_pairing_requested_payload_no_display_name() {
        let payload = PairingRequestedPayload {
            code: "000001".into(),
            platform_user_id: "u1".into(),
            platform_type: "dingtalk".into(),
            channel_plugin_id: None,
            display_name: None,
            expires_at: 1700000600000,
        };
        let json = serde_json::to_value(&payload).unwrap();
        assert!(json.get("display_name").is_none());
        assert!(json.get("channel_plugin_id").is_none());
    }

    #[test]
    fn test_plugin_status_changed_payload_serde() {
        let payload = PluginStatusChangedPayload {
            plugin_id: CHANNEL_ID.into(),
            status: PluginStatusResponse {
                plugin_id: CHANNEL_ID.into(),
                plugin_type: "telegram".into(),
                name: "Telegram Bot".into(),
                enabled: true,
                status: Some("running".into()),
                last_connected: Some(1700000000000),
                companion_id: None,
                public_agent_id: None,
                bot_key: None,
                created_at: 1699000000000,
                updated_at: 1700000000000,
                connected: false,
                has_token: false,
                bot_username: None,
                active_users: 0,
            },
        };
        let json = serde_json::to_value(&payload).unwrap();
        assert_eq!(json["plugin_id"], CHANNEL_ID);
        assert_eq!(json["status"]["type"], "telegram");
        assert_eq!(json["status"]["status"], "running");
        assert_eq!(json["status"]["enabled"], true);
    }

    #[test]
    fn test_user_authorized_payload_serde() {
        let payload = UserAuthorizedPayload {
            channel_user_id: CHANNEL_USER_ID.into(),
            platform_user_id: "tg_42".into(),
            platform_type: "telegram".into(),
            channel_plugin_id: Some(CHANNEL_ID.into()),
            display_name: Some("Alice".into()),
        };
        let json = serde_json::to_value(&payload).unwrap();
        assert_eq!(json["channel_user_id"], CHANNEL_USER_ID);
        assert_eq!(json["platform_user_id"], "tg_42");
        assert_eq!(json["platform_type"], "telegram");
        assert_eq!(json["channel_plugin_id"], CHANNEL_ID);
        assert_eq!(json["display_name"], "Alice");
    }

    #[test]
    fn test_user_authorized_payload_no_display_name() {
        let payload = UserAuthorizedPayload {
            channel_user_id: CHANNEL_USER_ID.into(),
            platform_user_id: "lk_1".into(),
            platform_type: "lark".into(),
            channel_plugin_id: None,
            display_name: None,
        };
        let json = serde_json::to_value(&payload).unwrap();
        assert!(json.get("display_name").is_none());
        assert!(json.get("channel_plugin_id").is_none());
    }

    // -- Roundtrip tests ------------------------------------------------------

    #[test]
    fn test_plugin_status_response_roundtrip() {
        let resp = PluginStatusResponse {
            plugin_id: CHANNEL_ID.into(),
            plugin_type: "dingtalk".into(),
            name: "DingTalk Bot".into(),
            enabled: true,
            status: Some("ready".into()),
            last_connected: None,
            companion_id: Some(COMPANION_ID.into()),
            public_agent_id: None,
            bot_key: Some("cli_app".into()),
            created_at: 1699000000000,
            updated_at: 1699000000000,
            connected: false,
            has_token: false,
            bot_username: None,
            active_users: 0,
        };
        let json = serde_json::to_string(&resp).unwrap();
        let parsed: PluginStatusResponse = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, resp);
    }

    #[test]
    fn test_bridge_response_roundtrip() {
        let resp = BridgeResponse {
            success: true,
            message: Some("Done".into()),
            error: None,
        };
        let json = serde_json::to_string(&resp).unwrap();
        let parsed: BridgeResponse = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, resp);
    }

    #[test]
    fn test_channel_session_response_roundtrip() {
        let resp = ChannelSessionResponse {
            channel_session_id: CHANNEL_SESSION_ID.into(),
            channel_user_id: CHANNEL_USER_ID.into(),
            agent_type: "acp".into(),
            conversation_id: Some("0190f5fe-7c00-7a00-8000-000000000001".into()),
            workspace: None,
            chat_id: Some("ch1".into()),
            channel_plugin_id: Some(CHANNEL_ID.into()),
            created_at: 1000,
            last_activity: 2000,
        };
        let json = serde_json::to_string(&resp).unwrap();
        let parsed: ChannelSessionResponse = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, resp);
    }

    #[test]
    fn channel_plugin_dtos_reject_noncanonical_plugin_id_values() {
        let valid_status = json!({
            "plugin_id": CHANNEL_ID,
            "type": "telegram",
            "name": "Telegram Bot",
            "enabled": true,
            "created_at": 1,
            "updated_at": 2,
            "connected": false,
            "has_token": false,
            "active_users": 0
        });
        for plugin_id in [
            json!(42),
            json!("telegram"),
            json!("550e8400-e29b-41d4-a716-446655440000"),
            json!("018F1234-5678-7ABC-8DEF-012345678990"),
            json!("channel_018f1234-5678-7abc-8def-012345678990"),
        ] {
            assert!(serde_json::from_value::<EnablePluginRequest>(json!({
                "plugin_id": plugin_id.clone(),
                "config": {}
            })).is_err());
            assert!(serde_json::from_value::<DisablePluginRequest>(json!({
                "plugin_id": plugin_id.clone()
            })).is_err());

            let mut raw = valid_status.clone();
            raw["plugin_id"] = plugin_id;
            assert!(serde_json::from_value::<PluginStatusResponse>(raw).is_err());
        }
    }

    #[test]
    fn channel_user_dtos_reject_noncanonical_user_id_values() {
        let valid_user = json!({
            "channel_user_id": CHANNEL_USER_ID,
            "platform_user_id": "tg_42",
            "platform_type": "telegram",
            "authorized_at": 1
        });
        for channel_user_id in [
            json!(42),
            json!("channel-user-1"),
            json!("550e8400-e29b-41d4-a716-446655440000"),
            json!("018F1234-5678-7ABC-8DEF-012345678992"),
            json!("user_018f1234-5678-7abc-8def-012345678992"),
        ] {
            assert!(serde_json::from_value::<RevokeUserRequest>(json!({
                "channel_user_id": channel_user_id.clone()
            })).is_err());
            let mut raw = valid_user.clone();
            raw["channel_user_id"] = channel_user_id;
            assert!(serde_json::from_value::<ChannelUserResponse>(raw).is_err());
        }
    }

    #[test]
    fn channel_session_response_rejects_noncanonical_durable_ids_by_field() {
        let valid = json!({
            "channel_session_id": CHANNEL_SESSION_ID,
            "channel_user_id": CHANNEL_USER_ID,
            "agent_type": "acp",
            "conversation_id": CONVERSATION_ID,
            "channel_plugin_id": CHANNEL_ID,
            "created_at": 1000,
            "last_activity": 2000
        });
        for (field, value) in [
            ("channel_session_id", json!("session-1")),
            ("channel_user_id", json!("user-1")),
            ("conversation_id", json!("conversation-1")),
            ("channel_plugin_id", json!("telegram")),
        ] {
            let mut raw = valid.clone();
            raw[field] = value;
            assert!(serde_json::from_value::<ChannelSessionResponse>(raw).is_err());
        }
        for field in [
            "channel_session_id",
            "channel_user_id",
            "conversation_id",
            "channel_plugin_id",
        ] {
            let mut raw = valid.clone();
            raw[field] = json!("550e8400-e29b-41d4-a716-446655440000");
            assert!(serde_json::from_value::<ChannelSessionResponse>(raw).is_err());
        }
    }

    #[test]
    fn channel_session_response_rejects_legacy_id_field_names() {
        let raw = json!({
            "id": CHANNEL_SESSION_ID,
            "user_id": CHANNEL_USER_ID,
            "agent_type": "acp",
            "created_at": 1000,
            "last_activity": 2000
        });
        assert!(serde_json::from_value::<ChannelSessionResponse>(raw).is_err());
    }

    #[test]
    fn revoke_user_request_rejects_legacy_user_id_field_name() {
        let raw = json!({ "user_id": CHANNEL_USER_ID });
        assert!(serde_json::from_value::<RevokeUserRequest>(raw).is_err());
    }
}

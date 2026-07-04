use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fmt;

// ---------------------------------------------------------------------------
// A. Plugin Type
// ---------------------------------------------------------------------------

/// Platform type identifier for channel plugins.
///
/// Includes the four supported IM platforms and reserved variants
/// for future platforms (`slack`/`discord` per the `assistant_plugins.type`
/// CHECK constraint in the DB schema).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PluginType {
    Telegram,
    Lark,
    Dingtalk,
    Weixin,
    /// WeCom / 企业微信 intelligent bot, long-connection (WebSocket) mode
    /// authenticated with `bot_id` + `secret`. Distinct from `Weixin`
    /// (consumer WeChat iLink bot) — different endpoint, transport, and creds.
    Wecom,
    /// Reserved variant for future Slack integration.
    Slack,
    /// Reserved variant for future Discord integration.
    Discord,
    /// Matrix (open federated protocol, /sync long-poll; E2EE gated on dep compat).
    Matrix,
    /// Mattermost (self-hosted, WebSocket + REST).
    Mattermost,
    /// Twitch chat (IRC-over-WebSocket, outbound).
    Twitch,
    /// Nostr (outbound WebSocket to relays, NIP-04 encrypted DMs).
    Nostr,
    /// QQ Bot (official outbound WS gateway + REST; OAuth2 token).
    Qqbot,
}

impl fmt::Display for PluginType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Telegram => write!(f, "telegram"),
            Self::Lark => write!(f, "lark"),
            Self::Dingtalk => write!(f, "dingtalk"),
            Self::Weixin => write!(f, "weixin"),
            Self::Wecom => write!(f, "wecom"),
            Self::Slack => write!(f, "slack"),
            Self::Discord => write!(f, "discord"),
            Self::Matrix => write!(f, "matrix"),
            Self::Mattermost => write!(f, "mattermost"),
            Self::Twitch => write!(f, "twitch"),
            Self::Nostr => write!(f, "nostr"),
            Self::Qqbot => write!(f, "qqbot"),
        }
    }
}

impl PluginType {
    /// Parse from a string, returning `None` for unknown types.
    pub fn from_str_opt(s: &str) -> Option<Self> {
        match s {
            "telegram" => Some(Self::Telegram),
            "lark" => Some(Self::Lark),
            "dingtalk" => Some(Self::Dingtalk),
            "weixin" => Some(Self::Weixin),
            "wecom" => Some(Self::Wecom),
            "slack" => Some(Self::Slack),
            "discord" => Some(Self::Discord),
            "matrix" => Some(Self::Matrix),
            "mattermost" => Some(Self::Mattermost),
            "twitch" => Some(Self::Twitch),
            "nostr" => Some(Self::Nostr),
            "qqbot" => Some(Self::Qqbot),
            _ => None,
        }
    }
}

// ---------------------------------------------------------------------------
// B. Plugin Status (lifecycle state machine)
// ---------------------------------------------------------------------------

/// Plugin lifecycle status.
///
/// State machine:
/// ```text
/// created → initializing → ready → starting → running → stopping → stopped
///                ↓                    ↓           ↓
///              error ←←←←←←←←←←←←←←←←←←←←←←←←←←←
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PluginStatus {
    Created,
    Initializing,
    Ready,
    Starting,
    Running,
    Stopping,
    Stopped,
    Error,
}

impl fmt::Display for PluginStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Created => write!(f, "created"),
            Self::Initializing => write!(f, "initializing"),
            Self::Ready => write!(f, "ready"),
            Self::Starting => write!(f, "starting"),
            Self::Running => write!(f, "running"),
            Self::Stopping => write!(f, "stopping"),
            Self::Stopped => write!(f, "stopped"),
            Self::Error => write!(f, "error"),
        }
    }
}

impl PluginStatus {
    /// Parse from a string, returning `None` for unknown values.
    pub fn from_str_opt(s: &str) -> Option<Self> {
        match s {
            "created" => Some(Self::Created),
            "initializing" => Some(Self::Initializing),
            "ready" => Some(Self::Ready),
            "starting" => Some(Self::Starting),
            "running" => Some(Self::Running),
            "stopping" => Some(Self::Stopping),
            "stopped" => Some(Self::Stopped),
            "error" => Some(Self::Error),
            _ => None,
        }
    }
}

// ---------------------------------------------------------------------------
// C. Pairing Status
// ---------------------------------------------------------------------------

/// Status of a pairing code.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PairingStatus {
    Pending,
    Approved,
    Rejected,
    Expired,
}

impl fmt::Display for PairingStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Pending => write!(f, "pending"),
            Self::Approved => write!(f, "approved"),
            Self::Rejected => write!(f, "rejected"),
            Self::Expired => write!(f, "expired"),
        }
    }
}

impl PairingStatus {
    /// Parse from a string, returning `None` for unknown values.
    pub fn from_str_opt(s: &str) -> Option<Self> {
        match s {
            "pending" => Some(Self::Pending),
            "approved" => Some(Self::Approved),
            "rejected" => Some(Self::Rejected),
            "expired" => Some(Self::Expired),
            _ => None,
        }
    }
}

// ---------------------------------------------------------------------------
// D. Plugin Credentials & Config
// ---------------------------------------------------------------------------

/// Platform-specific plugin credentials.
///
/// Each platform uses a subset of fields:
/// - Telegram: `token`
/// - Lark: `app_id` + `app_secret` + optional `encrypt_key`/`verification_token`
/// - DingTalk: `client_id` + `client_secret`
/// - WeChat: `account_id` + `bot_token`
///
/// Remaining fields are captured in `extra` for extensibility
/// (API Spec `[key: string]: unknown`).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct PluginCredentials {
    // Telegram
    #[serde(skip_serializing_if = "Option::is_none")]
    pub token: Option<String>,

    // Lark
    #[serde(skip_serializing_if = "Option::is_none")]
    pub app_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub app_secret: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub encrypt_key: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub verification_token: Option<String>,

    // DingTalk
    #[serde(skip_serializing_if = "Option::is_none")]
    pub client_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub client_secret: Option<String>,

    // WeChat (iLink Bot)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub account_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bot_token: Option<String>,

    // WeCom (企业微信智能机器人 long-connection: bot_id + secret)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bot_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub secret: Option<String>,

    // Slack (Socket Mode: bot token reuses `token` (xoxb-); app-level token below)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub app_token: Option<String>,

    // Matrix (homeserver + bot mxid + access token)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub homeserver_url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub user_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub access_token: Option<String>,

    // Mattermost (bot token reuses `token`; server base URL below)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub server_url: Option<String>,

    // Twitch (OAuth access token reuses `token`, client_id reused; target channel below)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub twitch_channel: Option<String>,

    // Nostr (private key + relay list)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub nostr_private_key: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub nostr_relays: Option<String>,

    // Extensibility
    #[serde(flatten)]
    pub extra: HashMap<String, serde_json::Value>,
}

/// Plugin connection options.
///
/// Configures the connection mode, webhook URL, rate limiting,
/// and group-chat mention requirement.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PluginConfigOptions {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mode: Option<ConnectionMode>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub webhook_url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rate_limit: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub require_mention: Option<bool>,

    // Extensibility
    #[serde(flatten)]
    pub extra: HashMap<String, serde_json::Value>,
}

/// Connection mode for a plugin.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ConnectionMode {
    Polling,
    Webhook,
    Websocket,
}

/// Combined plugin configuration: credentials + options.
///
/// Stored as JSON in the `assistant_plugins.config` column (encrypted).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PluginConfig {
    pub credentials: PluginCredentials,
    #[serde(default)]
    pub config: Option<PluginConfigOptions>,
}

/// Extracts the platform-level bot identity from credentials.
///
/// This key is what makes two configs "the same bot": it backs the
/// `UNIQUE(type, bot_key)` constraint on `assistant_plugins`, which
/// structurally prevents one bot from being bound to more than one companion.
/// Only non-secret identifiers are used (the telegram token's numeric
/// bot-id prefix is public; secrets stay inside the encrypted config).
pub fn bot_key_for(plugin_type: PluginType, credentials: &PluginCredentials) -> Option<String> {
    let raw = match plugin_type {
        PluginType::Telegram | PluginType::Slack | PluginType::Discord => credentials
            .token
            .as_deref()
            .map(|t| t.split(':').next().unwrap_or(t).to_owned()),
        PluginType::Lark => credentials.app_id.clone(),
        PluginType::Dingtalk => credentials.client_id.clone(),
        PluginType::Weixin => credentials.account_id.clone().or_else(|| credentials.bot_token.clone()),
        // WeCom: the non-secret bot identity is its bot_id.
        PluginType::Wecom => credentials.bot_id.clone(),
        PluginType::Matrix => match (&credentials.homeserver_url, &credentials.user_id) {
            (Some(hs), Some(uid)) => Some(format!("{}|{}", hs.trim_end_matches('/'), uid)),
            _ => None,
        },
        PluginType::Mattermost => credentials
            .server_url
            .as_deref()
            .map(|s| s.trim_end_matches('/').to_owned()),
        // Twitch: one bot per joined channel (non-secret, user-provided).
        PluginType::Twitch => credentials.twitch_channel.as_deref().map(|c| c.trim_start_matches('#').to_lowercase()),
        // Nostr: the bot identity is its derived npub, which requires secp256k1
        // (not available here). Leave unset → no UNIQUE enforcement in v1.
        PluginType::Nostr => None,
        // QQ Bot: appId (client_id) is the non-secret bot identity (like DingTalk).
        PluginType::Qqbot => credentials.client_id.clone(),
    };
    raw.map(|s| s.trim().to_owned()).filter(|s| !s.is_empty())
}

// ---------------------------------------------------------------------------
// E. Bot Info
// ---------------------------------------------------------------------------

/// Information about the bot identity on a platform.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct BotInfo {
    pub id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub username: Option<String>,
    pub display_name: String,
}

// ---------------------------------------------------------------------------
// F. Unified Incoming Message
// ---------------------------------------------------------------------------

/// Message received from an IM platform, normalized to a common format.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct UnifiedIncomingMessage {
    pub id: String,
    pub platform: PluginType,
    pub chat_id: String,
    pub user: UnifiedUser,
    pub content: UnifiedMessageContent,
    pub timestamp: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reply_to_message_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub action: Option<UnifiedAction>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub raw: Option<serde_json::Value>,
}

/// An incoming message stamped with the `assistant_plugins` row it arrived
/// through. Plugins emit bare [`UnifiedIncomingMessage`]s; the manager's
/// per-instance forwarder adds the channel id so the orchestrator can route
/// sessions, replies and companion bindings per bot (not per platform).
#[derive(Debug, Clone)]
pub struct ChannelIncoming {
    pub channel_id: String,
    pub message: UnifiedIncomingMessage,
}

/// Sender identity from an IM platform.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct UnifiedUser {
    pub id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub username: Option<String>,
    pub display_name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub avatar_url: Option<String>,
}

/// Content of an incoming message.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct UnifiedMessageContent {
    #[serde(rename = "type")]
    pub content_type: MessageContentType,
    pub text: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub attachments: Option<Vec<UnifiedAttachment>>,
}

/// Type discriminant for message content.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MessageContentType {
    Text,
    Photo,
    Document,
    Voice,
    Audio,
    Video,
    Sticker,
    Action,
    Command,
}

/// File attachment in a unified message.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct UnifiedAttachment {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub file_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub file_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mime_type: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub file_size: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
}

// ---------------------------------------------------------------------------
// G. Unified Outgoing Message
// ---------------------------------------------------------------------------

/// Message to be sent to an IM platform.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct UnifiedOutgoingMessage {
    #[serde(rename = "type")]
    pub message_type: OutgoingMessageType,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parse_mode: Option<ParseMode>,
    /// Inline action buttons (rows x columns).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub buttons: Option<Vec<Vec<ActionButton>>>,
    /// Fixed keyboard buttons (rows x columns).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub keyboard: Option<Vec<Vec<ActionButton>>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub image_url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub file_url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub file_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub media_actions: Option<Vec<ChannelMediaAction>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reply_to_message_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub silent: Option<bool>,
}

/// Outgoing message type discriminant.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum OutgoingMessageType {
    Text,
    Image,
    File,
    Buttons,
}

/// Text formatting mode for platforms that support it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ParseMode {
    HTML,
    MarkdownV2,
    Markdown,
}

/// An interactive button in an outgoing message.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ActionButton {
    pub label: String,
    pub action: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub params: Option<HashMap<String, String>>,
}

/// Media action attached to an outgoing message.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ChannelMediaAction {
    pub url: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
}

// ---------------------------------------------------------------------------
// G2. Decision (blocking permission/confirmation relayed as numbered text)
// ---------------------------------------------------------------------------

/// One selectable option of a blocking decision (agent permission /
/// confirmation) relayed to a channel user as a numbered text list.
///
/// `option_id` is the value submitted back through
/// `ConversationService::confirm` (a bare option-id string for ACP); `label`
/// is the human-readable name rendered in the numbered list.
#[derive(Debug, Clone, PartialEq)]
pub struct DecisionOption {
    pub option_id: String,
    pub label: String,
}

// ---------------------------------------------------------------------------
// H. Action System
// ---------------------------------------------------------------------------

/// A routable action parsed from a button callback or command.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct UnifiedAction {
    /// Action identifier, e.g. "session.new", "chat.send".
    pub action: String,
    pub category: ActionCategory,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub params: Option<HashMap<String, String>>,
    pub context: ActionContext,
}

/// Action category (determines which handler group processes the action).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ActionCategory {
    Platform,
    System,
    Chat,
}

/// Context provided with every action for routing and state lookup.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ActionContext {
    pub platform: PluginType,
    pub user_id: String,
    pub chat_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
}

/// Response produced by an action handler.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ActionResponse {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parse_mode: Option<ParseMode>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub buttons: Option<Vec<Vec<ActionButton>>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub keyboard: Option<Vec<Vec<ActionButton>>>,
    pub behavior: ActionBehavior,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub toast: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub edit_message_id: Option<String>,
}

/// How the platform should deliver the action response.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ActionBehavior {
    /// Send as a new message.
    Send,
    /// Edit an existing message (identified by `edit_message_id`).
    Edit,
    /// Answer the callback query (inline toast).
    Answer,
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // -- A. PluginType -------------------------------------------------------

    #[test]
    fn plugin_type_serde_roundtrip() {
        let cases = [
            (PluginType::Telegram, "\"telegram\""),
            (PluginType::Lark, "\"lark\""),
            (PluginType::Dingtalk, "\"dingtalk\""),
            (PluginType::Weixin, "\"weixin\""),
            (PluginType::Slack, "\"slack\""),
            (PluginType::Discord, "\"discord\""),
            (PluginType::Matrix, "\"matrix\""),
            (PluginType::Mattermost, "\"mattermost\""),
            (PluginType::Twitch, "\"twitch\""),
            (PluginType::Nostr, "\"nostr\""),
            (PluginType::Qqbot, "\"qqbot\""),
        ];
        for (variant, expected_json) in cases {
            let json = serde_json::to_string(&variant).unwrap();
            assert_eq!(json, expected_json, "serialize {variant:?}");
            let parsed: PluginType = serde_json::from_str(&json).unwrap();
            assert_eq!(parsed, variant, "deserialize {expected_json}");
        }
    }

    #[test]
    fn plugin_type_display() {
        assert_eq!(PluginType::Telegram.to_string(), "telegram");
        assert_eq!(PluginType::Lark.to_string(), "lark");
        assert_eq!(PluginType::Dingtalk.to_string(), "dingtalk");
        assert_eq!(PluginType::Weixin.to_string(), "weixin");
        assert_eq!(PluginType::Slack.to_string(), "slack");
        assert_eq!(PluginType::Discord.to_string(), "discord");
        assert_eq!(PluginType::Matrix.to_string(), "matrix");
        assert_eq!(PluginType::Mattermost.to_string(), "mattermost");
        assert_eq!(PluginType::Twitch.to_string(), "twitch");
        assert_eq!(PluginType::Nostr.to_string(), "nostr");
        assert_eq!(PluginType::Qqbot.to_string(), "qqbot");
    }

    #[test]
    fn plugin_type_from_str_opt() {
        assert_eq!(PluginType::from_str_opt("telegram"), Some(PluginType::Telegram));
        assert_eq!(PluginType::from_str_opt("lark"), Some(PluginType::Lark));
        assert_eq!(PluginType::from_str_opt("unknown"), None);
    }

    #[test]
    fn plugin_type_unknown_deserialization_fails() {
        let result = serde_json::from_str::<PluginType>("\"whatsapp\"");
        assert!(result.is_err());
    }

    // -- B. PluginStatus -----------------------------------------------------

    #[test]
    fn plugin_status_serde_roundtrip() {
        let cases = [
            (PluginStatus::Created, "\"created\""),
            (PluginStatus::Initializing, "\"initializing\""),
            (PluginStatus::Ready, "\"ready\""),
            (PluginStatus::Starting, "\"starting\""),
            (PluginStatus::Running, "\"running\""),
            (PluginStatus::Stopping, "\"stopping\""),
            (PluginStatus::Stopped, "\"stopped\""),
            (PluginStatus::Error, "\"error\""),
        ];
        for (variant, expected_json) in cases {
            let json = serde_json::to_string(&variant).unwrap();
            assert_eq!(json, expected_json, "serialize {variant:?}");
            let parsed: PluginStatus = serde_json::from_str(&json).unwrap();
            assert_eq!(parsed, variant, "deserialize {expected_json}");
        }
    }

    #[test]
    fn plugin_status_display() {
        assert_eq!(PluginStatus::Created.to_string(), "created");
        assert_eq!(PluginStatus::Running.to_string(), "running");
        assert_eq!(PluginStatus::Error.to_string(), "error");
    }

    #[test]
    fn plugin_status_from_str_opt() {
        assert_eq!(PluginStatus::from_str_opt("running"), Some(PluginStatus::Running));
        assert_eq!(PluginStatus::from_str_opt("unknown"), None);
    }

    // -- C. PairingStatus ----------------------------------------------------

    #[test]
    fn pairing_status_serde_roundtrip() {
        let cases = [
            (PairingStatus::Pending, "\"pending\""),
            (PairingStatus::Approved, "\"approved\""),
            (PairingStatus::Rejected, "\"rejected\""),
            (PairingStatus::Expired, "\"expired\""),
        ];
        for (variant, expected_json) in cases {
            let json = serde_json::to_string(&variant).unwrap();
            assert_eq!(json, expected_json, "serialize {variant:?}");
            let parsed: PairingStatus = serde_json::from_str(&json).unwrap();
            assert_eq!(parsed, variant, "deserialize {expected_json}");
        }
    }

    #[test]
    fn pairing_status_display() {
        assert_eq!(PairingStatus::Pending.to_string(), "pending");
        assert_eq!(PairingStatus::Approved.to_string(), "approved");
    }

    #[test]
    fn pairing_status_from_str_opt() {
        assert_eq!(PairingStatus::from_str_opt("pending"), Some(PairingStatus::Pending));
        assert_eq!(PairingStatus::from_str_opt("nope"), None);
    }

    // -- D. Credentials & Config ---------------------------------------------

    #[test]
    fn plugin_credentials_telegram() {
        let creds = PluginCredentials {
            token: Some("bot123:ABC".into()),
            ..Default::default()
        };
        let json = serde_json::to_value(&creds).unwrap();
        assert_eq!(json["token"], "bot123:ABC");
        // Optional fields should be absent
        assert!(json.get("app_id").is_none());
    }

    #[test]
    fn plugin_credentials_lark() {
        let creds = PluginCredentials {
            app_id: Some("cli_abc".into()),
            app_secret: Some("secret".into()),
            encrypt_key: Some("ek".into()),
            verification_token: Some("vt".into()),
            ..Default::default()
        };
        let json = serde_json::to_value(&creds).unwrap();
        assert_eq!(json["app_id"], "cli_abc");
        assert_eq!(json["app_secret"], "secret");
        assert_eq!(json["encrypt_key"], "ek");
        assert_eq!(json["verification_token"], "vt");
    }

    #[test]
    fn plugin_credentials_extensible() {
        let raw = json!({
            "token": "xxx",
            "customField": "hello"
        });
        let creds: PluginCredentials = serde_json::from_value(raw).unwrap();
        assert_eq!(creds.token.as_deref(), Some("xxx"));
        assert_eq!(creds.extra.get("customField").unwrap(), "hello");
    }

    #[test]
    fn plugin_credentials_new_platform_fields() {
        let creds = PluginCredentials {
            app_token: Some("xapp-1-A".into()),
            homeserver_url: Some("https://matrix.org".into()),
            user_id: Some("@bot:matrix.org".into()),
            access_token: Some("syt_secret".into()),
            server_url: Some("https://mm.example.com".into()),
            ..Default::default()
        };
        let json = serde_json::to_value(&creds).unwrap();
        assert_eq!(json["app_token"], "xapp-1-A");
        assert_eq!(json["homeserver_url"], "https://matrix.org");
        assert_eq!(json["user_id"], "@bot:matrix.org");
        assert_eq!(json["access_token"], "syt_secret");
        assert_eq!(json["server_url"], "https://mm.example.com");
        // 未设置字段不出现
        assert!(json.get("token").is_none());
    }

    #[test]
    fn bot_key_for_new_platforms() {
        let matrix = PluginCredentials {
            homeserver_url: Some("https://matrix.org/".into()),
            user_id: Some("@bot:matrix.org".into()),
            access_token: Some("secret".into()),
            ..Default::default()
        };
        assert_eq!(
            bot_key_for(PluginType::Matrix, &matrix).as_deref(),
            Some("https://matrix.org|@bot:matrix.org")
        );

        let mm = PluginCredentials {
            server_url: Some("https://mm.example.com/".into()),
            token: Some("bot-token-secret".into()),
            ..Default::default()
        };
        // Mattermost v1：一服务器一 bot，bot_key=server_url（非密；不含 token）
        assert_eq!(
            bot_key_for(PluginType::Mattermost, &mm).as_deref(),
            Some("https://mm.example.com")
        );

        // 缺字段 → None
        assert_eq!(bot_key_for(PluginType::Matrix, &PluginCredentials::default()), None);
    }

    #[test]
    fn plugin_config_full() {
        let raw = json!({
            "credentials": { "token": "bot:123" },
            "config": {
                "mode": "polling",
                "rate_limit": 10,
                "require_mention": true
            }
        });
        let cfg: PluginConfig = serde_json::from_value(raw).unwrap();
        assert_eq!(cfg.credentials.token.as_deref(), Some("bot:123"));
        let opts = cfg.config.unwrap();
        assert_eq!(opts.mode, Some(ConnectionMode::Polling));
        assert_eq!(opts.rate_limit, Some(10));
        assert_eq!(opts.require_mention, Some(true));
    }

    #[test]
    fn plugin_config_minimal() {
        let raw = json!({
            "credentials": { "token": "bot:123" }
        });
        let cfg: PluginConfig = serde_json::from_value(raw).unwrap();
        assert!(cfg.config.is_none());
    }

    #[test]
    fn connection_mode_serde() {
        let cases = [
            (ConnectionMode::Polling, "\"polling\""),
            (ConnectionMode::Webhook, "\"webhook\""),
            (ConnectionMode::Websocket, "\"websocket\""),
        ];
        for (variant, expected) in cases {
            let json = serde_json::to_string(&variant).unwrap();
            assert_eq!(json, expected);
            let parsed: ConnectionMode = serde_json::from_str(&json).unwrap();
            assert_eq!(parsed, variant);
        }
    }

    // -- E. BotInfo ----------------------------------------------------------

    #[test]
    fn bot_info_serde() {
        let info = BotInfo {
            id: "bot_1".into(),
            username: Some("my_bot".into()),
            display_name: "My Bot".into(),
        };
        let json = serde_json::to_value(&info).unwrap();
        assert_eq!(json["id"], "bot_1");
        assert_eq!(json["username"], "my_bot");
        assert_eq!(json["display_name"], "My Bot");
    }

    #[test]
    fn bot_info_without_username() {
        let info = BotInfo {
            id: "bot_2".into(),
            username: None,
            display_name: "Bot 2".into(),
        };
        let json = serde_json::to_value(&info).unwrap();
        assert!(json.get("username").is_none());
    }

    // -- F. Incoming Message -------------------------------------------------

    #[test]
    fn unified_incoming_message_text() {
        let msg = UnifiedIncomingMessage {
            id: "msg_1".into(),
            platform: PluginType::Telegram,
            chat_id: "chat_42".into(),
            user: UnifiedUser {
                id: "user_1".into(),
                username: Some("alice".into()),
                display_name: "Alice".into(),
                avatar_url: None,
            },
            content: UnifiedMessageContent {
                content_type: MessageContentType::Text,
                text: "Hello".into(),
                attachments: None,
            },
            timestamp: 1700000000,
            reply_to_message_id: None,
            action: None,
            raw: None,
        };
        let json = serde_json::to_value(&msg).unwrap();
        assert_eq!(json["id"], "msg_1");
        assert_eq!(json["platform"], "telegram");
        assert_eq!(json["content"]["type"], "text");
        assert_eq!(json["content"]["text"], "Hello");
        assert_eq!(json["user"]["display_name"], "Alice");
    }

    #[test]
    fn message_content_type_serde() {
        let cases = [
            (MessageContentType::Text, "\"text\""),
            (MessageContentType::Photo, "\"photo\""),
            (MessageContentType::Document, "\"document\""),
            (MessageContentType::Voice, "\"voice\""),
            (MessageContentType::Audio, "\"audio\""),
            (MessageContentType::Video, "\"video\""),
            (MessageContentType::Sticker, "\"sticker\""),
            (MessageContentType::Action, "\"action\""),
            (MessageContentType::Command, "\"command\""),
        ];
        for (variant, expected) in cases {
            let json = serde_json::to_string(&variant).unwrap();
            assert_eq!(json, expected, "serialize {variant:?}");
        }
    }

    #[test]
    fn unified_attachment_serde() {
        let att = UnifiedAttachment {
            file_id: Some("file_1".into()),
            file_name: Some("photo.jpg".into()),
            mime_type: Some("image/jpeg".into()),
            file_size: Some(12345),
            url: None,
        };
        let json = serde_json::to_value(&att).unwrap();
        assert_eq!(json["file_id"], "file_1");
        assert_eq!(json["file_name"], "photo.jpg");
        assert_eq!(json["mime_type"], "image/jpeg");
        assert_eq!(json["file_size"], 12345);
        assert!(json.get("url").is_none());
    }

    // -- G. Outgoing Message -------------------------------------------------

    #[test]
    fn outgoing_message_text() {
        let msg = UnifiedOutgoingMessage {
            message_type: OutgoingMessageType::Text,
            text: Some("Hello back!".into()),
            parse_mode: None,
            buttons: None,
            keyboard: None,
            image_url: None,
            file_url: None,
            file_name: None,
            media_actions: None,
            reply_to_message_id: None,
            silent: None,
        };
        let json = serde_json::to_value(&msg).unwrap();
        assert_eq!(json["type"], "text");
        assert_eq!(json["text"], "Hello back!");
    }

    #[test]
    fn outgoing_message_with_buttons() {
        let msg = UnifiedOutgoingMessage {
            message_type: OutgoingMessageType::Buttons,
            text: Some("Choose:".into()),
            parse_mode: None,
            buttons: Some(vec![vec![
                ActionButton {
                    label: "Yes".into(),
                    action: "confirm.yes".into(),
                    params: None,
                },
                ActionButton {
                    label: "No".into(),
                    action: "confirm.no".into(),
                    params: None,
                },
            ]]),
            keyboard: None,
            image_url: None,
            file_url: None,
            file_name: None,
            media_actions: None,
            reply_to_message_id: None,
            silent: None,
        };
        let json = serde_json::to_value(&msg).unwrap();
        assert_eq!(json["type"], "buttons");
        assert_eq!(json["buttons"][0][0]["label"], "Yes");
        assert_eq!(json["buttons"][0][1]["action"], "confirm.no");
    }

    #[test]
    fn outgoing_message_type_serde() {
        let cases = [
            (OutgoingMessageType::Text, "\"text\""),
            (OutgoingMessageType::Image, "\"image\""),
            (OutgoingMessageType::File, "\"file\""),
            (OutgoingMessageType::Buttons, "\"buttons\""),
        ];
        for (variant, expected) in cases {
            let json = serde_json::to_string(&variant).unwrap();
            assert_eq!(json, expected);
        }
    }

    #[test]
    fn parse_mode_serde() {
        let cases = [
            (ParseMode::HTML, "\"HTML\""),
            (ParseMode::MarkdownV2, "\"MarkdownV2\""),
            (ParseMode::Markdown, "\"Markdown\""),
        ];
        for (variant, expected) in cases {
            let json = serde_json::to_string(&variant).unwrap();
            assert_eq!(json, expected);
            let parsed: ParseMode = serde_json::from_str(&json).unwrap();
            assert_eq!(parsed, variant);
        }
    }

    // -- H. Action System ----------------------------------------------------

    #[test]
    fn action_category_serde() {
        let cases = [
            (ActionCategory::Platform, "\"platform\""),
            (ActionCategory::System, "\"system\""),
            (ActionCategory::Chat, "\"chat\""),
        ];
        for (variant, expected) in cases {
            let json = serde_json::to_string(&variant).unwrap();
            assert_eq!(json, expected);
            let parsed: ActionCategory = serde_json::from_str(&json).unwrap();
            assert_eq!(parsed, variant);
        }
    }

    #[test]
    fn unified_action_serde() {
        let action = UnifiedAction {
            action: "session.new".into(),
            category: ActionCategory::System,
            params: None,
            context: ActionContext {
                platform: PluginType::Telegram,
                user_id: "tg_42".into(),
                chat_id: "chat_1".into(),
                message_id: Some("msg_99".into()),
                session_id: None,
            },
        };
        let json = serde_json::to_value(&action).unwrap();
        assert_eq!(json["action"], "session.new");
        assert_eq!(json["category"], "system");
        assert_eq!(json["context"]["platform"], "telegram");
        assert_eq!(json["context"]["user_id"], "tg_42");
    }

    #[test]
    fn action_behavior_serde() {
        let cases = [
            (ActionBehavior::Send, "\"send\""),
            (ActionBehavior::Edit, "\"edit\""),
            (ActionBehavior::Answer, "\"answer\""),
        ];
        for (variant, expected) in cases {
            let json = serde_json::to_string(&variant).unwrap();
            assert_eq!(json, expected);
        }
    }

    #[test]
    fn action_response_full() {
        let resp = ActionResponse {
            text: Some("Session created".into()),
            parse_mode: Some(ParseMode::HTML),
            buttons: Some(vec![vec![ActionButton {
                label: "Help".into(),
                action: "help.show".into(),
                params: None,
            }]]),
            keyboard: None,
            behavior: ActionBehavior::Send,
            toast: None,
            edit_message_id: None,
        };
        let json = serde_json::to_value(&resp).unwrap();
        assert_eq!(json["text"], "Session created");
        assert_eq!(json["parse_mode"], "HTML");
        assert_eq!(json["behavior"], "send");
        assert_eq!(json["buttons"][0][0]["label"], "Help");
    }

    #[test]
    fn action_response_edit() {
        let resp = ActionResponse {
            text: Some("Updated".into()),
            parse_mode: None,
            buttons: None,
            keyboard: None,
            behavior: ActionBehavior::Edit,
            toast: None,
            edit_message_id: Some("msg_42".into()),
        };
        let json = serde_json::to_value(&resp).unwrap();
        assert_eq!(json["behavior"], "edit");
        assert_eq!(json["edit_message_id"], "msg_42");
    }

    #[test]
    fn action_button_with_params() {
        let mut params = HashMap::new();
        params.insert("agentType".into(), "gemini".into());
        let btn = ActionButton {
            label: "Switch to Gemini".into(),
            action: "agent.select".into(),
            params: Some(params),
        };
        let json = serde_json::to_value(&btn).unwrap();
        assert_eq!(json["params"]["agentType"], "gemini");
    }

    // -- Roundtrip tests -----------------------------------------------------

    #[test]
    fn incoming_message_roundtrip() {
        let msg = UnifiedIncomingMessage {
            id: "m1".into(),
            platform: PluginType::Lark,
            chat_id: "c1".into(),
            user: UnifiedUser {
                id: "u1".into(),
                username: None,
                display_name: "Bob".into(),
                avatar_url: None,
            },
            content: UnifiedMessageContent {
                content_type: MessageContentType::Text,
                text: "test".into(),
                attachments: None,
            },
            timestamp: 1000,
            reply_to_message_id: None,
            action: None,
            raw: None,
        };
        let json = serde_json::to_string(&msg).unwrap();
        let parsed: UnifiedIncomingMessage = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, msg);
    }

    #[test]
    fn outgoing_message_roundtrip() {
        let msg = UnifiedOutgoingMessage {
            message_type: OutgoingMessageType::Text,
            text: Some("hello".into()),
            parse_mode: Some(ParseMode::Markdown),
            buttons: None,
            keyboard: None,
            image_url: None,
            file_url: None,
            file_name: None,
            media_actions: None,
            reply_to_message_id: None,
            silent: Some(true),
        };
        let json = serde_json::to_string(&msg).unwrap();
        let parsed: UnifiedOutgoingMessage = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, msg);
    }

    #[test]
    fn plugin_config_roundtrip() {
        let cfg = PluginConfig {
            credentials: PluginCredentials {
                token: Some("bot:abc".into()),
                ..Default::default()
            },
            config: Some(PluginConfigOptions {
                mode: Some(ConnectionMode::Polling),
                webhook_url: None,
                rate_limit: Some(5),
                require_mention: None,
                extra: HashMap::new(),
            }),
        };
        let json = serde_json::to_string(&cfg).unwrap();
        let parsed: PluginConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, cfg);
    }
}

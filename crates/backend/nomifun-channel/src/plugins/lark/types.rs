use serde::{Deserialize, Deserializer, Serialize};

/// Deserialize a string that may be null into an empty string.
fn deserialize_nullable_string<'de, D>(deserializer: D) -> Result<String, D::Error>
where
    D: Deserializer<'de>,
{
    Option::<String>::deserialize(deserializer).map(|opt| opt.unwrap_or_default())
}

// ---------------------------------------------------------------------------
// Tenant access token
// ---------------------------------------------------------------------------

/// Request body for obtaining a tenant access token.
#[derive(Debug, Clone, Serialize)]
pub(crate) struct TenantAccessTokenRequest {
    pub app_id: String,
    pub app_secret: String,
}

/// Response from the tenant access token endpoint.
#[derive(Debug, Clone, Deserialize)]
pub(crate) struct TenantAccessTokenResponse {
    pub code: i32,
    pub msg: String,
    pub tenant_access_token: Option<String>,
    pub expire: Option<i64>,
}

// ---------------------------------------------------------------------------
// Bot info
// ---------------------------------------------------------------------------

/// Response from the bot info endpoint.
#[derive(Debug, Clone, Deserialize)]
pub(crate) struct BotInfoResponse {
    pub code: i32,
    pub msg: String,
    pub bot: Option<BotInfoData>,
}

/// Bot identity data from the bot info endpoint.
#[derive(Debug, Clone, Deserialize)]
pub(crate) struct BotInfoData {
    pub app_name: String,
    #[serde(default)]
    pub open_id: String,
}

// ---------------------------------------------------------------------------
// WebSocket endpoint
// ---------------------------------------------------------------------------

/// Request body for the WebSocket endpoint URL request.
/// Note: This endpoint uses AppID/AppSecret directly, NOT Bearer token auth.
#[derive(Debug, Clone, Serialize)]
pub(crate) struct WsEndpointRequest {
    #[serde(rename = "AppID")]
    pub app_id: String,
    #[serde(rename = "AppSecret")]
    pub app_secret: String,
}

/// Response from the WebSocket endpoint URL request.
#[derive(Debug, Clone, Deserialize)]
pub(crate) struct WsEndpointResponse {
    pub code: i32,
    pub msg: String,
    pub data: Option<WsEndpointData>,
}

/// Data containing the WebSocket URL and client configuration.
#[derive(Debug, Clone, Deserialize)]
pub(crate) struct WsEndpointData {
    #[serde(rename = "URL")]
    pub url: String,
    #[serde(rename = "ClientConfig")]
    pub client_config: Option<WsClientConfig>,
}

/// Client configuration returned by the WS endpoint API.
#[derive(Debug, Clone, Deserialize)]
#[allow(dead_code)]
pub(crate) struct WsClientConfig {
    #[serde(rename = "ReconnectCount")]
    pub reconnect_count: Option<i32>,
    #[serde(rename = "ReconnectInterval")]
    pub reconnect_interval: Option<u64>,
    #[serde(rename = "ReconnectNonce")]
    pub reconnect_nonce: Option<u64>,
    #[serde(rename = "PingInterval")]
    pub ping_interval: Option<u64>,
}

// ---------------------------------------------------------------------------
// Message event (im.message.receive_v1)
// ---------------------------------------------------------------------------

/// Event payload for `im.message.receive_v1`.
#[derive(Debug, Clone, Deserialize)]
pub(crate) struct MessageEvent {
    pub sender: MessageSender,
    pub message: MessageBody,
}

/// Sender information in a message event.
#[derive(Debug, Clone, Deserialize)]
#[allow(dead_code)]
pub(crate) struct MessageSender {
    pub sender_id: SenderIdContainer,
    #[serde(default)]
    pub sender_type: String,
    #[serde(default)]
    pub tenant_key: String,
}

/// Container for various ID types.
#[derive(Debug, Clone, Deserialize)]
#[allow(dead_code)]
pub(crate) struct SenderIdContainer {
    #[serde(default, deserialize_with = "deserialize_nullable_string")]
    pub open_id: String,
    #[serde(default, deserialize_with = "deserialize_nullable_string")]
    pub user_id: String,
    #[serde(default, deserialize_with = "deserialize_nullable_string")]
    pub union_id: String,
}

/// Message body in a message event.
#[derive(Debug, Clone, Deserialize)]
#[allow(dead_code)]
pub(crate) struct MessageBody {
    #[serde(default, deserialize_with = "deserialize_nullable_string")]
    pub message_id: String,
    #[serde(default, deserialize_with = "deserialize_nullable_string")]
    pub chat_id: String,
    #[serde(default, deserialize_with = "deserialize_nullable_string")]
    pub chat_type: String,
    #[serde(default, deserialize_with = "deserialize_nullable_string")]
    pub message_type: String,
    #[serde(default, deserialize_with = "deserialize_nullable_string")]
    pub content: String,
    #[serde(default)]
    pub parent_id: Option<String>,
    #[serde(default)]
    pub root_id: Option<String>,
    #[serde(default)]
    pub create_time: Option<String>,
    #[serde(default)]
    pub mentions: Option<Vec<Mention>>,
}

/// A mention in a message.
#[derive(Debug, Clone, Deserialize)]
#[allow(dead_code)]
pub(crate) struct Mention {
    pub key: String,
    pub id: MentionId,
    pub name: String,
}

/// Mention ID container.
#[derive(Debug, Clone, Deserialize)]
#[allow(dead_code)]
pub(crate) struct MentionId {
    #[serde(default)]
    pub open_id: String,
    #[serde(default)]
    pub user_id: String,
    #[serde(default)]
    pub union_id: String,
}

// ---------------------------------------------------------------------------
// Message content types (JSON-encoded in `content` field)
// ---------------------------------------------------------------------------

/// Text message content: `{ "text": "..." }`.
#[derive(Debug, Clone, Deserialize)]
pub(crate) struct TextContent {
    pub text: String,
}

/// Image message content: `{ "image_key": "..." }`.
#[derive(Debug, Clone, Deserialize)]
pub(crate) struct ImageContent {
    pub image_key: String,
}

/// File message content: `{ "file_key": "...", "file_name": "..." }`.
#[derive(Debug, Clone, Deserialize)]
pub(crate) struct FileContent {
    pub file_key: String,
    #[serde(default)]
    pub file_name: Option<String>,
}

/// Audio message content: `{ "file_key": "..." }`.
#[derive(Debug, Clone, Deserialize)]
pub(crate) struct AudioContent {
    pub file_key: String,
}

// ---------------------------------------------------------------------------
// Card action (card.action.trigger)
// ---------------------------------------------------------------------------

/// Payload for `card.action.trigger` events.
#[derive(Debug, Clone, Deserialize)]
#[allow(dead_code)]
pub(crate) struct CardActionEvent {
    pub operator: CardOperator,
    pub action: CardAction,
    #[serde(default)]
    pub token: Option<String>,
    pub open_message_id: Option<String>,
    pub open_chat_id: Option<String>,
}

/// Operator (user) who triggered the card action.
#[derive(Debug, Clone, Deserialize)]
#[allow(dead_code)]
pub(crate) struct CardOperator {
    pub open_id: String,
    #[serde(default)]
    pub user_id: Option<String>,
}

/// Card action details.
#[derive(Debug, Clone, Deserialize)]
#[allow(dead_code)]
pub(crate) struct CardAction {
    pub tag: String,
    pub value: Option<serde_json::Value>,
}

// ---------------------------------------------------------------------------
// Bot menu event (application.bot.menu_v6)
// ---------------------------------------------------------------------------

/// Payload for `application.bot.menu_v6` events.
#[derive(Debug, Clone, Deserialize)]
#[allow(dead_code)]
pub(crate) struct BotMenuEvent {
    pub operator: BotMenuOperator,
    pub event_key: String,
    #[serde(default)]
    pub timestamp: Option<String>,
}

/// Operator of a bot menu event.
#[derive(Debug, Clone, Deserialize)]
pub(crate) struct BotMenuOperator {
    pub operator_id: OperatorId,
}

/// Operator ID container for bot menu events.
#[derive(Debug, Clone, Deserialize)]
#[allow(dead_code)]
pub(crate) struct OperatorId {
    #[serde(default)]
    pub open_id: String,
    #[serde(default)]
    pub user_id: String,
    #[serde(default)]
    pub union_id: String,
}

// ---------------------------------------------------------------------------
// Send / Update interactive card
// ---------------------------------------------------------------------------

/// Request body for sending an interactive card message.
#[derive(Debug, Clone, Serialize)]
pub(crate) struct SendCardRequest {
    pub receive_id: String,
    pub msg_type: String,
    pub content: String,
}

/// Response from the send message API.
#[derive(Debug, Clone, Deserialize)]
pub(crate) struct SendMessageResponse {
    pub code: i32,
    pub msg: String,
    pub data: Option<SendMessageData>,
}

/// Data from a successful send message response.
#[derive(Debug, Clone, Deserialize)]
pub(crate) struct SendMessageData {
    pub message_id: String,
}

/// Request body for updating (patching) a card message.
#[derive(Debug, Clone, Serialize)]
pub(crate) struct UpdateCardRequest {
    pub content: String,
}

/// Generic Lark API response for operations without specific return data.
#[derive(Debug, Clone, Deserialize)]
pub(crate) struct GenericResponse {
    pub code: i32,
    pub msg: String,
}

// ---------------------------------------------------------------------------
// Image / file upload (outbound media)
// ---------------------------------------------------------------------------

/// Response from the image upload endpoint (`/im/v1/images`).
#[derive(Debug, Clone, Deserialize)]
pub(crate) struct ImageUploadResponse {
    pub code: i32,
    pub msg: String,
    pub data: Option<ImageUploadData>,
}

/// Data from a successful image upload: the reusable `image_key`.
#[derive(Debug, Clone, Deserialize)]
pub(crate) struct ImageUploadData {
    pub image_key: String,
}

/// Response from the file upload endpoint (`/im/v1/files`).
#[derive(Debug, Clone, Deserialize)]
pub(crate) struct FileUploadResponse {
    pub code: i32,
    pub msg: String,
    pub data: Option<FileUploadData>,
}

/// Data from a successful file upload: the reusable `file_key`.
#[derive(Debug, Clone, Deserialize)]
pub(crate) struct FileUploadData {
    pub file_key: String,
}

// ---------------------------------------------------------------------------
// Interactive card building helpers
// ---------------------------------------------------------------------------

/// Build a Lark interactive card JSON string from text and optional buttons.
///
/// All Lark responses use interactive cards because Lark only supports
/// editing card messages (not plain text messages).
pub(crate) fn build_interactive_card(text: &str, buttons: Option<&[Vec<crate::types::ActionButton>]>) -> String {
    let mut elements = vec![serde_json::json!({
        "tag": "markdown",
        "content": text
    })];

    if let Some(button_rows) = buttons {
        let mut actions = Vec::new();
        for row in button_rows {
            for btn in row {
                let callback_value = format_lark_callback(&btn.action, btn.params.as_ref());
                actions.push(serde_json::json!({
                    "tag": "button",
                    "text": { "tag": "plain_text", "content": btn.label },
                    "type": "primary",
                    "value": { "action": callback_value }
                }));
            }
        }
        if !actions.is_empty() {
            elements.push(serde_json::json!({
                "tag": "action",
                "actions": actions
            }));
        }
    }

    let card = serde_json::json!({
        "elements": elements
    });
    card.to_string()
}

/// Encode an action + params into a Lark card button value string.
///
/// Format: `"category:action"` or `"category:action:k=v,k=v"`
/// (same wire format as Telegram callback_data for consistency).
fn format_lark_callback(action: &str, params: Option<&std::collections::HashMap<String, String>>) -> String {
    let category = action_category_prefix(action);
    match params {
        Some(p) if !p.is_empty() => {
            let encoded: Vec<String> = p.iter().map(|(k, v)| format!("{k}={v}")).collect();
            format!("{category}:{}:{}", action, encoded.join(","))
        }
        _ => format!("{category}:{action}"),
    }
}

/// Derive the category prefix from an action name.
/// Same logic as Telegram plugin for consistency.
fn action_category_prefix(action: &str) -> &'static str {
    if action == "system.confirm" {
        return "chat";
    }
    let prefix = action.split('.').next().unwrap_or("");
    match prefix {
        "pairing" => "platform",
        "chat" | "action" => "chat",
        _ => "system",
    }
}

/// Parsed callback data: (category, action, params).
pub(crate) type ParsedLarkCallback = (String, String, Option<std::collections::HashMap<String, String>>);

/// Parse a Lark card button value string back to (category, action, params).
///
/// Format: `"category:action"` or `"category:action:k=v,k=v"`.
pub(crate) fn parse_lark_callback(data: &str) -> Option<ParsedLarkCallback> {
    let parts: Vec<&str> = data.splitn(3, ':').collect();
    if parts.len() < 2 {
        return None;
    }

    let category = parts[0].to_string();
    let action = parts[1].to_string();

    // Validate category
    if !matches!(category.as_str(), "platform" | "system" | "chat") {
        return None;
    }

    let params = if parts.len() == 3 && !parts[2].is_empty() {
        let mut map = std::collections::HashMap::new();
        for pair in parts[2].split(',') {
            if let Some((k, v)) = pair.split_once('=') {
                map.insert(k.to_string(), v.to_string());
            }
        }
        if map.is_empty() { None } else { Some(map) }
    } else {
        None
    };

    Some((category, action, params))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // -- TenantAccessTokenResponse ------------------------------------------

    #[test]
    fn tenant_token_response_ok() {
        let raw = json!({
            "code": 0,
            "msg": "ok",
            "tenant_access_token": "t-abc123",
            "expire": 7200
        });
        let resp: TenantAccessTokenResponse = serde_json::from_value(raw).unwrap();
        assert_eq!(resp.code, 0);
        assert_eq!(resp.tenant_access_token.as_deref(), Some("t-abc123"));
        assert_eq!(resp.expire, Some(7200));
    }

    #[test]
    fn tenant_token_response_error() {
        let raw = json!({
            "code": 10003,
            "msg": "invalid app_id"
        });
        let resp: TenantAccessTokenResponse = serde_json::from_value(raw).unwrap();
        assert_eq!(resp.code, 10003);
        assert!(resp.tenant_access_token.is_none());
    }

    // -- BotInfoResponse ----------------------------------------------------

    #[test]
    fn bot_info_response_ok() {
        let raw = json!({
            "code": 0,
            "msg": "ok",
            "bot": {
                "app_name": "TestBot",
                "open_id": "ou_abc123"
            }
        });
        let resp: BotInfoResponse = serde_json::from_value(raw).unwrap();
        assert_eq!(resp.code, 0);
        let bot = resp.bot.unwrap();
        assert_eq!(bot.app_name, "TestBot");
        assert_eq!(bot.open_id, "ou_abc123");
    }

    // -- WsEndpointResponse -------------------------------------------------

    #[test]
    fn ws_endpoint_response_ok() {
        let raw = json!({
            "code": 0,
            "msg": "ok",
            "data": { "URL": "wss://open.feishu.cn/ws/xxx" }
        });
        let resp: WsEndpointResponse = serde_json::from_value(raw).unwrap();
        assert_eq!(resp.code, 0);
        let data = resp.data.unwrap();
        assert_eq!(data.url, "wss://open.feishu.cn/ws/xxx");
        assert!(data.client_config.is_none());
    }

    #[test]
    fn ws_endpoint_response_with_client_config() {
        let raw = json!({
            "code": 0,
            "msg": "success",
            "data": {
                "URL": "wss://open.feishu.cn/ws/abc?device_id=d1&service_id=7",
                "ClientConfig": {
                    "ReconnectCount": -1,
                    "ReconnectInterval": 120,
                    "ReconnectNonce": 30,
                    "PingInterval": 120
                }
            }
        });
        let resp: WsEndpointResponse = serde_json::from_value(raw).unwrap();
        assert_eq!(resp.code, 0);
        let data = resp.data.unwrap();
        assert_eq!(data.url, "wss://open.feishu.cn/ws/abc?device_id=d1&service_id=7");
        let config = data.client_config.unwrap();
        assert_eq!(config.reconnect_count, Some(-1));
        assert_eq!(config.ping_interval, Some(120));
    }

    // -- MessageEvent -------------------------------------------------------

    #[test]
    fn message_event_text_parses() {
        let raw = json!({
            "sender": {
                "sender_id": { "open_id": "ou_user1", "user_id": "u1", "union_id": "" },
                "sender_type": "user",
                "tenant_key": "tk_1"
            },
            "message": {
                "message_id": "om_msg1",
                "chat_id": "oc_chat1",
                "chat_type": "p2p",
                "message_type": "text",
                "content": "{\"text\":\"Hello\"}",
                "create_time": "1700000000000"
            }
        });
        let evt: MessageEvent = serde_json::from_value(raw).unwrap();
        assert_eq!(evt.sender.sender_id.open_id, "ou_user1");
        assert_eq!(evt.message.message_id, "om_msg1");
        assert_eq!(evt.message.chat_id, "oc_chat1");
        assert_eq!(evt.message.message_type, "text");
    }

    #[test]
    fn message_event_with_mentions_parses() {
        let raw = json!({
            "sender": {
                "sender_id": { "open_id": "ou_user2", "user_id": "", "union_id": "" },
                "sender_type": "user",
                "tenant_key": "tk_1"
            },
            "message": {
                "message_id": "om_msg2",
                "chat_id": "oc_chat2",
                "chat_type": "group",
                "message_type": "text",
                "content": "{\"text\":\"@_user_1 Hello\"}",
                "mentions": [
                    {
                        "key": "@_user_1",
                        "id": { "open_id": "ou_bot", "user_id": "", "union_id": "" },
                        "name": "MyBot"
                    }
                ]
            }
        });
        let evt: MessageEvent = serde_json::from_value(raw).unwrap();
        let mentions = evt.message.mentions.unwrap();
        assert_eq!(mentions.len(), 1);
        assert_eq!(mentions[0].key, "@_user_1");
        assert_eq!(mentions[0].name, "MyBot");
    }

    // -- TextContent / ImageContent / FileContent ---------------------------

    #[test]
    fn text_content_parses() {
        let raw = json!({ "text": "Hello World" });
        let tc: TextContent = serde_json::from_value(raw).unwrap();
        assert_eq!(tc.text, "Hello World");
    }

    #[test]
    fn image_content_parses() {
        let raw = json!({ "image_key": "img_key_123" });
        let ic: ImageContent = serde_json::from_value(raw).unwrap();
        assert_eq!(ic.image_key, "img_key_123");
    }

    #[test]
    fn file_content_parses() {
        let raw = json!({ "file_key": "file_key_123", "file_name": "doc.pdf" });
        let fc: FileContent = serde_json::from_value(raw).unwrap();
        assert_eq!(fc.file_key, "file_key_123");
        assert_eq!(fc.file_name.as_deref(), Some("doc.pdf"));
    }

    // -- CardActionEvent ----------------------------------------------------

    #[test]
    fn card_action_event_parses() {
        let raw = json!({
            "operator": { "open_id": "ou_user1", "user_id": "u1" },
            "action": { "tag": "button", "value": { "action": "system:session.new" } },
            "token": "tok_card",
            "open_message_id": "om_card1",
            "open_chat_id": "oc_chat1"
        });
        let evt: CardActionEvent = serde_json::from_value(raw).unwrap();
        assert_eq!(evt.operator.open_id, "ou_user1");
        assert_eq!(evt.action.tag, "button");
        assert_eq!(evt.open_message_id.as_deref(), Some("om_card1"));
    }

    // -- BotMenuEvent -------------------------------------------------------

    #[test]
    fn bot_menu_event_parses() {
        let raw = json!({
            "operator": {
                "operator_id": { "open_id": "ou_user1", "user_id": "u1", "union_id": "" }
            },
            "event_key": "help",
            "timestamp": "1700000000"
        });
        let evt: BotMenuEvent = serde_json::from_value(raw).unwrap();
        assert_eq!(evt.operator.operator_id.open_id, "ou_user1");
        assert_eq!(evt.event_key, "help");
    }

    // -- SendMessageResponse ------------------------------------------------

    #[test]
    fn send_message_response_ok() {
        let raw = json!({
            "code": 0,
            "msg": "ok",
            "data": { "message_id": "om_sent1" }
        });
        let resp: SendMessageResponse = serde_json::from_value(raw).unwrap();
        assert_eq!(resp.code, 0);
        assert_eq!(resp.data.unwrap().message_id, "om_sent1");
    }

    #[test]
    fn send_message_response_error() {
        let raw = json!({ "code": 230001, "msg": "send failed" });
        let resp: SendMessageResponse = serde_json::from_value(raw).unwrap();
        assert_eq!(resp.code, 230001);
        assert!(resp.data.is_none());
    }

    // -- build_interactive_card ---------------------------------------------

    #[test]
    fn build_card_text_only() {
        let card = build_interactive_card("Hello", None);
        let v: serde_json::Value = serde_json::from_str(&card).unwrap();
        assert_eq!(v["elements"][0]["tag"], "markdown");
        assert_eq!(v["elements"][0]["content"], "Hello");
        assert!(v["elements"].get(1).is_none());
    }

    #[test]
    fn build_card_with_buttons() {
        use crate::types::ActionButton;
        let buttons = vec![vec![
            ActionButton {
                label: "Yes".into(),
                action: "system.confirm".into(),
                params: None,
            },
            ActionButton {
                label: "No".into(),
                action: "chat.regenerate".into(),
                params: None,
            },
        ]];
        let card = build_interactive_card("Choose:", Some(&buttons));
        let v: serde_json::Value = serde_json::from_str(&card).unwrap();
        assert_eq!(v["elements"][0]["content"], "Choose:");
        let actions = &v["elements"][1]["actions"];
        assert_eq!(actions[0]["text"]["content"], "Yes");
        assert_eq!(actions[1]["text"]["content"], "No");
    }

    // -- format_lark_callback / parse_lark_callback -------------------------

    #[test]
    fn format_callback_no_params() {
        assert_eq!(format_lark_callback("help.show", None), "system:help.show");
        assert_eq!(format_lark_callback("pairing.show", None), "platform:pairing.show");
        assert_eq!(format_lark_callback("chat.regenerate", None), "chat:chat.regenerate");
        assert_eq!(format_lark_callback("system.confirm", None), "chat:system.confirm");
    }

    #[test]
    fn format_callback_with_params() {
        use std::collections::HashMap;
        let mut params = HashMap::new();
        params.insert("callId".into(), "abc".into());
        params.insert("value".into(), "yes".into());
        let result = format_lark_callback("system.confirm", Some(&params));
        assert!(result.starts_with("chat:system.confirm:"));
        assert!(result.contains("callId=abc"));
        assert!(result.contains("value=yes"));
    }

    #[test]
    fn parse_callback_roundtrip_no_params() {
        let encoded = format_lark_callback("session.new", None);
        let (cat, action, params) = parse_lark_callback(&encoded).unwrap();
        assert_eq!(cat, "system");
        assert_eq!(action, "session.new");
        assert!(params.is_none());
    }

    #[test]
    fn parse_callback_roundtrip_with_params() {
        use std::collections::HashMap;
        let mut p = HashMap::new();
        p.insert("agentType".into(), "gemini".into());
        let encoded = format_lark_callback("agent.select", Some(&p));
        let (cat, action, params) = parse_lark_callback(&encoded).unwrap();
        assert_eq!(cat, "system");
        assert_eq!(action, "agent.select");
        assert_eq!(params.unwrap().get("agentType").unwrap(), "gemini");
    }

    #[test]
    fn parse_callback_invalid() {
        assert!(parse_lark_callback("invalid").is_none());
        assert!(parse_lark_callback("unknown:action").is_none());
    }

    // -- GenericResponse ----------------------------------------------------

    #[test]
    fn generic_response_ok() {
        let raw = json!({ "code": 0, "msg": "ok" });
        let resp: GenericResponse = serde_json::from_value(raw).unwrap();
        assert_eq!(resp.code, 0);
    }

    // -- SendCardRequest serializes -----------------------------------------

    #[test]
    fn send_card_request_serializes() {
        let req = SendCardRequest {
            receive_id: "oc_chat1".into(),
            msg_type: "interactive".into(),
            content: "{}".into(),
        };
        let json = serde_json::to_value(&req).unwrap();
        assert_eq!(json["receive_id"], "oc_chat1");
        assert_eq!(json["msg_type"], "interactive");
    }

    // -- UpdateCardRequest serializes ---------------------------------------

    #[test]
    fn update_card_request_serializes() {
        let req = UpdateCardRequest {
            content: "{\"elements\":[]}".into(),
        };
        let json = serde_json::to_value(&req).unwrap();
        assert_eq!(json["content"], "{\"elements\":[]}");
    }

    // -- TenantAccessTokenRequest serializes --------------------------------

    #[test]
    fn tenant_token_request_serializes() {
        let req = TenantAccessTokenRequest {
            app_id: "cli_123".into(),
            app_secret: "secret_456".into(),
        };
        let json = serde_json::to_value(&req).unwrap();
        assert_eq!(json["app_id"], "cli_123");
        assert_eq!(json["app_secret"], "secret_456");
    }
}

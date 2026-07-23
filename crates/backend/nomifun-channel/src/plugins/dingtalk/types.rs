use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// Access token
// ---------------------------------------------------------------------------

/// Request body for obtaining a DingTalk access token.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct AccessTokenRequest {
    pub app_key: String,
    pub app_secret: String,
}

/// Response from the access token endpoint.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
#[allow(dead_code)]
pub(crate) struct AccessTokenResponse {
    #[serde(default)]
    pub access_token: Option<String>,
    #[serde(default)]
    pub expire_in: Option<i64>,
    #[serde(default)]
    pub errcode: Option<i64>,
    #[serde(default)]
    pub errmsg: Option<String>,
}

// ---------------------------------------------------------------------------
// Bot info (via /v1.0/robot/oToMessages)
// ---------------------------------------------------------------------------

/// Response from the robot info endpoint (GET /v1.0/im/robot/info).
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct RobotInfoResponse {
    #[serde(default)]
    pub nick: Option<String>,
    #[serde(default)]
    pub robot_user_id: Option<String>,
}

// ---------------------------------------------------------------------------
// WebSocket Stream registration
// ---------------------------------------------------------------------------

/// Request body for registering a WebSocket Stream connection.
///
/// Note: This endpoint uses clientId/clientSecret directly in the body,
/// NOT the access token header used by other DingTalk APIs.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct RegisterStreamRequest {
    pub client_id: String,
    pub client_secret: String,
    pub subscriptions: Vec<StreamSubscription>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ua: Option<String>,
}

/// A subscription entry for stream registration.
#[derive(Debug, Clone, Serialize)]
pub(crate) struct StreamSubscription {
    #[serde(rename = "type")]
    pub sub_type: String,
    pub topic: String,
}

/// Response from the stream registration endpoint.
#[derive(Debug, Clone, Deserialize)]
pub(crate) struct RegisterStreamResponse {
    pub endpoint: String,
    pub ticket: String,
}

// ---------------------------------------------------------------------------
// WebSocket Stream frame
// ---------------------------------------------------------------------------

/// A frame received over the DingTalk WebSocket Stream connection.
///
/// The DingTalk stream protocol uses a JSON envelope with `type` and
/// `headers`/`data` fields.
#[derive(Debug, Clone, Deserialize)]
pub(crate) struct StreamFrame {
    /// Frame type: "SYSTEM", "EVENT", "CALLBACK".
    #[serde(rename = "type")]
    pub frame_type: String,
    /// Protocol headers.
    #[serde(default)]
    pub headers: StreamHeaders,
    /// Payload (JSON-encoded string for EVENT/CALLBACK).
    #[serde(default)]
    pub data: Option<String>,
}

/// Headers in a stream frame.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
#[allow(dead_code)]
pub(crate) struct StreamHeaders {
    #[serde(default)]
    pub content_type: Option<String>,
    #[serde(default)]
    pub message_id: Option<String>,
    #[serde(default)]
    pub topic: Option<String>,
}

/// System-level stream events (e.g. CONNECTED, DISCONNECT).
#[derive(Debug, Clone, Deserialize)]
pub(crate) struct SystemEvent {
    #[serde(default)]
    pub code: Option<i64>,
    #[serde(default)]
    pub message: Option<String>,
}

/// Response to send back over the WebSocket to acknowledge a frame.
#[derive(Debug, Clone, Serialize)]
pub(crate) struct StreamAck {
    pub code: i64,
    pub headers: AckHeaders,
    pub message: String,
    pub data: String,
}

/// Headers in a stream acknowledgment.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct AckHeaders {
    pub content_type: String,
    pub message_id: String,
}

// ---------------------------------------------------------------------------
// Bot message callback (chat message from user)
// ---------------------------------------------------------------------------

/// Callback payload for incoming bot messages.
///
/// This is the decoded `data` field from a CALLBACK frame with topic
/// `/v1.0/im/bot/messages/get`.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
#[allow(dead_code)]
pub(crate) struct BotMessageCallback {
    /// Conversation ID (group chat) — empty for private chat.
    #[serde(default)]
    pub conversation_id: Option<String>,
    /// Unique message ID from DingTalk.
    #[serde(default)]
    pub msg_id: Option<String>,
    /// Message type: "text", "richText", "picture", "file", "audio", "video".
    #[serde(default)]
    pub msgtype: Option<String>,
    /// Text payload (for text messages).
    #[serde(default)]
    pub text: Option<TextPayload>,
    /// Sender information.
    #[serde(default)]
    pub sender_id: Option<String>,
    #[serde(default)]
    pub sender_nick: Option<String>,
    #[serde(default)]
    pub sender_staff_id: Option<String>,
    /// Session webhook URL for direct reply.
    #[serde(default)]
    pub session_webhook: Option<String>,
    #[serde(default)]
    pub session_webhook_expired_time: Option<i64>,
    /// Whether this is a group chat.
    #[serde(default)]
    pub conversation_type: Option<String>,
    /// Whether the bot was mentioned (at) in a group.
    #[serde(default)]
    pub is_in_at_list: Option<bool>,
    /// The at_users list.
    #[serde(default)]
    pub at_users: Option<Vec<AtUser>>,
    /// Timestamp (ms since epoch).
    #[serde(default)]
    pub create_at: Option<i64>,
    /// Robot code (App Key).
    #[serde(default)]
    pub robot_code: Option<String>,
}

/// Text payload in a bot message.
#[derive(Debug, Clone, Deserialize)]
pub(crate) struct TextPayload {
    #[serde(default)]
    pub content: Option<String>,
}

/// An at-mentioned user.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
#[allow(dead_code)]
pub(crate) struct AtUser {
    #[serde(default)]
    pub dingtalk_id: Option<String>,
    #[serde(default)]
    pub staff_id: Option<String>,
}

// ---------------------------------------------------------------------------
// Interactive card callback (card.action.trigger)
// ---------------------------------------------------------------------------

/// Callback payload for interactive card button clicks.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
#[allow(dead_code)]
pub(crate) struct CardActionCallback {
    /// The card's outgoing callback value JSON.
    #[serde(default)]
    pub card_private_data: Option<CardPrivateData>,
    /// User who clicked the button.
    #[serde(default)]
    pub user_id: Option<String>,
    /// Open conversation ID.
    #[serde(default)]
    pub open_conversation_id: Option<String>,
    /// Content (JSON string from the button action).
    #[serde(default)]
    pub content: Option<String>,
}

/// Private data in a card action callback.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
#[allow(dead_code)]
pub(crate) struct CardPrivateData {
    #[serde(default)]
    pub action_ids: Option<Vec<String>>,
    #[serde(default)]
    pub params: Option<serde_json::Value>,
}

// ---------------------------------------------------------------------------
// AI Card API types
// ---------------------------------------------------------------------------

/// Request to create an AI Card instance.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct CreateCardInstanceRequest {
    /// Card template ID.
    pub card_template_id: String,
    /// Client-generated tracking ID for the card instance.
    pub out_track_id: String,
    /// Callback type — must be "STREAM" for streaming cards.
    pub callback_type: String,
    /// Card data (initially empty for streaming cards).
    pub card_data: CardData,
    /// IM group space model (for group chats).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub im_group_open_space_model: Option<SpaceModel>,
    /// IM robot space model (for single chats).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub im_robot_open_space_model: Option<SpaceModel>,
}

/// Card data containing dynamic fields.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct CardData {
    /// Content body for the card (markdown).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub card_param_map: Option<serde_json::Value>,
}

/// Space model for card creation (supportForward flag).
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct SpaceModel {
    pub support_forward: bool,
}

/// IM group delivery settings.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ImGroupDeliverModel {
    /// Robot code (App Key).
    pub robot_code: String,
}

/// IM robot delivery settings (for single-chat cards).
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ImRobotDeliverModel {
    /// Space type for robot delivery.
    pub space_type: String,
}

/// Response from creating an AI Card instance.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct CreateCardInstanceResponse {
    /// Whether the request succeeded.
    #[serde(default)]
    pub success: Option<bool>,
}

/// Request to deliver a card instance to a chat.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct DeliverCardRequest {
    /// Card instance ID.
    pub out_track_id: String,
    /// Open space ID: `dtv1.card//IM_ROBOT.userId` or `dtv1.card//IM_GROUP.conversationId`.
    pub open_space_id: String,
    /// User ID type (1 = staffId).
    pub user_id_type: u8,
    /// IM group delivery model (for group chats).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub im_group_open_deliver_model: Option<ImGroupDeliverModel>,
    /// IM robot delivery model (for single chats).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub im_robot_open_deliver_model: Option<ImRobotDeliverModel>,
}

/// Response from delivering a card.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
#[allow(dead_code)]
pub(crate) struct DeliverCardResponse {
    #[serde(default)]
    pub success: Option<bool>,
    #[serde(default)]
    pub result: Option<serde_json::Value>,
}

/// Request to stream-write content to an AI Card.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct StreamingWriteRequest {
    /// Card instance ID.
    pub out_track_id: String,
    /// Streaming content key (use "msgContent" for AI Card streaming).
    pub key: String,
    /// Content to write.
    pub content: String,
    /// Whether this is a full replacement (true) or append (false).
    pub is_full: bool,
    /// Whether this is the final write (marks card streaming as complete).
    pub is_finalize: bool,
    /// Whether this is an error state.
    pub is_error: bool,
    /// Unique identifier for this write operation.
    pub guid: String,
}

/// Response from streaming write.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
#[allow(dead_code)]
pub(crate) struct StreamingWriteResponse {
    #[serde(default)]
    pub success: Option<bool>,
}

/// Request to update (finalize) a card instance.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct UpdateCardRequest {
    /// Card instance ID.
    pub out_track_id: String,
    /// Updated card data.
    pub card_data: CardData,
}

/// Response from updating a card.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
#[allow(dead_code)]
pub(crate) struct UpdateCardResponse {
    #[serde(default)]
    pub success: Option<bool>,
}

// ---------------------------------------------------------------------------
// Send message via session webhook (fallback)
// ---------------------------------------------------------------------------

/// Request body for sending a message via session webhook.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
#[allow(dead_code)]
pub(crate) struct SessionWebhookRequest {
    pub msgtype: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text: Option<WebhookText>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub markdown: Option<WebhookMarkdown>,
}

/// Text payload for session webhook messages.
#[derive(Debug, Clone, Serialize)]
#[allow(dead_code)]
pub(crate) struct WebhookText {
    pub content: String,
}

/// Markdown payload for session webhook messages.
#[derive(Debug, Clone, Serialize)]
#[allow(dead_code)]
pub(crate) struct WebhookMarkdown {
    pub title: String,
    pub text: String,
}

// ---------------------------------------------------------------------------
// Send message via Open API (fallback)
// ---------------------------------------------------------------------------

/// Request body for sending a message via DingTalk Open API.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct SendRobotMessageRequest {
    /// Message type: "sampleText", "sampleMarkdown".
    pub msg_key: String,
    /// JSON-encoded message content.
    pub msg_param: String,
    /// Robot code (App Key).
    pub robot_code: String,
    /// Open conversation ID (group) or user IDs (single).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub open_conversation_id: Option<String>,
    /// User ID list for single-chat messages.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub user_ids: Option<Vec<String>>,
}

/// Response from sending a robot message.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct SendRobotMessageResponse {
    #[serde(default)]
    pub process_query_key: Option<String>,
}

// ---------------------------------------------------------------------------
// Callback encoding / decoding helpers
// ---------------------------------------------------------------------------

/// Encode an action + params into a DingTalk card button value string.
///
/// Format: `"category:action"` or `"category:action:k=v,k=v"`
/// (same wire format as Telegram/Lark callback_data for consistency).
pub(crate) fn format_dingtalk_callback(
    action: &str,
    params: Option<&std::collections::HashMap<String, String>>,
) -> String {
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
pub(crate) type ParsedCallback = (String, String, Option<std::collections::HashMap<String, String>>);

/// Parse a DingTalk card button value string back to (category, action, params).
///
/// Format: `"category:action"` or `"category:action:k=v,k=v"`.
pub(crate) fn parse_dingtalk_callback(data: &str) -> Option<ParsedCallback> {
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

/// Encode a chat ID for DingTalk.
///
/// - Private chat (`conversationType == "1"`): `user:{staffId}`
/// - Group chat (`conversationType == "2"`): `group:{conversationId}`
///
/// DingTalk sends a `conversationId` for BOTH private and group chats,
/// so we must rely on `conversationType` to distinguish them.
pub(crate) fn encode_chat_id(
    conversation_type: Option<&str>,
    conversation_id: Option<&str>,
    sender_staff_id: &str,
) -> String {
    match conversation_type {
        Some("2") => {
            let cid = conversation_id.unwrap_or("");
            format!("group:{cid}")
        }
        _ => format!("user:{sender_staff_id}"),
    }
}

/// Decode a DingTalk chat ID into its components.
///
/// Returns `(is_group, raw_id)`.
pub(crate) fn decode_chat_id(chat_id: &str) -> (bool, &str) {
    if let Some(rest) = chat_id.strip_prefix("group:") {
        (true, rest)
    } else if let Some(rest) = chat_id.strip_prefix("user:") {
        (false, rest)
    } else {
        // Treat unknown format as user
        (false, chat_id)
    }
}

/// Build the open_space_id for card delivery.
///
/// - Group: `dtv1.card//IM_GROUP.{conversationId}`
/// - User: `dtv1.card//IM_ROBOT.{userId}`
pub(crate) fn build_open_space_id(chat_id: &str) -> String {
    let (is_group, raw_id) = decode_chat_id(chat_id);
    if is_group {
        format!("dtv1.card//IM_GROUP.{raw_id}")
    } else {
        format!("dtv1.card//IM_ROBOT.{raw_id}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // -- AccessTokenResponse ---------------------------------------------------

    #[test]
    fn access_token_response_ok() {
        let raw = json!({
            "accessToken": "at_123",
            "expireIn": 7200
        });
        let resp: AccessTokenResponse = serde_json::from_value(raw).unwrap();
        assert_eq!(resp.access_token.as_deref(), Some("at_123"));
        assert_eq!(resp.expire_in, Some(7200));
    }

    #[test]
    fn access_token_response_error() {
        let raw = json!({
            "errcode": 400100,
            "errmsg": "invalid appkey"
        });
        let resp: AccessTokenResponse = serde_json::from_value(raw).unwrap();
        assert_eq!(resp.errcode, Some(400100));
        assert!(resp.access_token.is_none());
    }

    // -- RobotInfoResponse ----------------------------------------------------

    #[test]
    fn robot_info_response_ok() {
        let raw = json!({
            "nick": "TestBot",
            "robotUserId": "robot_123"
        });
        let resp: RobotInfoResponse = serde_json::from_value(raw).unwrap();
        assert_eq!(resp.nick.as_deref(), Some("TestBot"));
        assert_eq!(resp.robot_user_id.as_deref(), Some("robot_123"));
    }

    // -- RegisterStreamResponse -----------------------------------------------

    #[test]
    fn register_stream_response_parses() {
        let raw = json!({
            "endpoint": "wss://stream.dingtalk.com/xxx",
            "ticket": "ticket_abc"
        });
        let resp: RegisterStreamResponse = serde_json::from_value(raw).unwrap();
        assert_eq!(resp.endpoint, "wss://stream.dingtalk.com/xxx");
        assert_eq!(resp.ticket, "ticket_abc");
    }

    // -- StreamFrame ----------------------------------------------------------

    #[test]
    fn stream_frame_system_parses() {
        let raw = json!({
            "type": "SYSTEM",
            "headers": {},
            "data": "{\"code\":200,\"message\":\"OK\"}"
        });
        let frame: StreamFrame = serde_json::from_value(raw).unwrap();
        assert_eq!(frame.frame_type, "SYSTEM");
        assert!(frame.data.is_some());
    }

    #[test]
    fn stream_frame_callback_parses() {
        let raw = json!({
            "type": "CALLBACK",
            "headers": {
                "contentType": "application/json",
                "messageId": "msg_123",
                "topic": "/v1.0/im/bot/messages/get"
            },
            "data": "{\"msgId\":\"abc\"}"
        });
        let frame: StreamFrame = serde_json::from_value(raw).unwrap();
        assert_eq!(frame.frame_type, "CALLBACK");
        assert_eq!(frame.headers.topic.as_deref(), Some("/v1.0/im/bot/messages/get"));
        assert_eq!(frame.headers.message_id.as_deref(), Some("msg_123"));
    }

    #[test]
    fn stream_frame_missing_data() {
        let raw = json!({ "type": "SYSTEM", "headers": {} });
        let frame: StreamFrame = serde_json::from_value(raw).unwrap();
        assert!(frame.data.is_none());
    }

    // -- BotMessageCallback ---------------------------------------------------

    #[test]
    fn bot_message_callback_text_parses() {
        let raw = json!({
            "conversationId": "cid_group1",
            "msgId": "msg_456",
            "msgtype": "text",
            "text": { "content": "Hello bot" },
            "senderId": "sender_1",
            "senderNick": "Alice",
            "senderStaffId": "staff_1",
            "sessionWebhook": "https://oapi.dingtalk.com/robot/sendBySession",
            "conversationType": "2",
            "isInAtList": true,
            "createAt": 1700000000000_i64,
            "robotCode": "my_app_key"
        });
        let cb: BotMessageCallback = serde_json::from_value(raw).unwrap();
        assert_eq!(cb.conversation_id.as_deref(), Some("cid_group1"));
        assert_eq!(cb.msgtype.as_deref(), Some("text"));
        assert_eq!(cb.text.as_ref().unwrap().content.as_deref(), Some("Hello bot"));
        assert_eq!(cb.sender_staff_id.as_deref(), Some("staff_1"));
        assert!(cb.session_webhook.is_some());
        assert_eq!(cb.conversation_type.as_deref(), Some("2"));
        assert_eq!(cb.is_in_at_list, Some(true));
    }

    #[test]
    fn bot_message_callback_private_chat() {
        let raw = json!({
            "msgId": "msg_789",
            "msgtype": "text",
            "text": { "content": "Hi" },
            "senderStaffId": "staff_2",
            "senderNick": "Bob",
            "conversationType": "1"
        });
        let cb: BotMessageCallback = serde_json::from_value(raw).unwrap();
        assert!(cb.conversation_id.is_none());
        assert_eq!(cb.conversation_type.as_deref(), Some("1"));
    }

    // -- CardActionCallback ---------------------------------------------------

    #[test]
    fn card_action_callback_parses() {
        let raw = json!({
            "cardPrivateData": {
                "actionIds": ["btn_1"],
                "params": { "action": "system:session.new" }
            },
            "userId": "user_123",
            "openConversationId": "conv_abc",
            "content": "{\"action\":\"system:session.new\"}"
        });
        let cb: CardActionCallback = serde_json::from_value(raw).unwrap();
        assert_eq!(cb.user_id.as_deref(), Some("user_123"));
        assert_eq!(cb.open_conversation_id.as_deref(), Some("conv_abc"));
        assert!(cb.content.is_some());
    }

    // -- CreateCardInstanceResponse -------------------------------------------

    #[test]
    fn create_card_instance_response_ok() {
        let raw = json!({
            "success": true,
            "result": { "outTrackId": "card_inst_1" }
        });
        let resp: CreateCardInstanceResponse = serde_json::from_value(raw).unwrap();
        assert_eq!(resp.success, Some(true));
    }

    // -- DeliverCardResponse --------------------------------------------------

    #[test]
    fn deliver_card_response_ok() {
        let raw = json!({ "success": true, "result": {} });
        let resp: DeliverCardResponse = serde_json::from_value(raw).unwrap();
        assert_eq!(resp.success, Some(true));
    }

    // -- StreamingWriteResponse -----------------------------------------------

    #[test]
    fn streaming_write_response_ok() {
        let raw = json!({ "success": true });
        let resp: StreamingWriteResponse = serde_json::from_value(raw).unwrap();
        assert_eq!(resp.success, Some(true));
    }

    // -- UpdateCardResponse ---------------------------------------------------

    #[test]
    fn update_card_response_ok() {
        let raw = json!({ "success": true });
        let resp: UpdateCardResponse = serde_json::from_value(raw).unwrap();
        assert_eq!(resp.success, Some(true));
    }

    // -- Serialization --------------------------------------------------------

    #[test]
    fn access_token_request_serializes() {
        let req = AccessTokenRequest {
            app_key: "key_1".into(),
            app_secret: "secret_1".into(),
        };
        let json = serde_json::to_value(&req).unwrap();
        assert_eq!(json["appKey"], "key_1");
        assert_eq!(json["appSecret"], "secret_1");
    }

    #[test]
    fn register_stream_request_serializes() {
        let req = RegisterStreamRequest {
            client_id: "key_1".into(),
            client_secret: "secret_1".into(),
            subscriptions: vec![StreamSubscription {
                sub_type: "CALLBACK".into(),
                topic: "/v1.0/im/bot/messages/get".into(),
            }],
            ua: None,
        };
        let json = serde_json::to_value(&req).unwrap();
        assert_eq!(json["clientId"], "key_1");
        assert_eq!(json["clientSecret"], "secret_1");
        assert_eq!(json["subscriptions"][0]["type"], "CALLBACK");
        assert_eq!(json["subscriptions"][0]["topic"], "/v1.0/im/bot/messages/get");
        assert!(json.get("ua").is_none());
    }

    #[test]
    fn register_stream_request_includes_credentials() {
        let req = RegisterStreamRequest {
            client_id: "my_client_id".into(),
            client_secret: "my_secret".into(),
            subscriptions: vec![
                StreamSubscription {
                    sub_type: "EVENT".into(),
                    topic: "*".into(),
                },
                StreamSubscription {
                    sub_type: "CALLBACK".into(),
                    topic: "/v1.0/im/bot/messages/get".into(),
                },
            ],
            ua: Some("nomicore".into()),
        };
        let json = serde_json::to_value(&req).unwrap();
        assert_eq!(json["clientId"], "my_client_id");
        assert_eq!(json["clientSecret"], "my_secret");
        assert_eq!(json["ua"], "nomicore");
        assert_eq!(json["subscriptions"][0]["type"], "EVENT");
        assert_eq!(json["subscriptions"][0]["topic"], "*");
        assert_eq!(json["subscriptions"][1]["type"], "CALLBACK");
    }

    #[test]
    fn stream_ack_serializes() {
        let ack = StreamAck {
            code: 200,
            headers: AckHeaders {
                content_type: "application/json".into(),
                message_id: "msg_1".into(),
            },
            message: "OK".into(),
            data: "{}".into(),
        };
        let json = serde_json::to_value(&ack).unwrap();
        assert_eq!(json["code"], 200);
        assert_eq!(json["headers"]["contentType"], "application/json");
        assert_eq!(json["headers"]["messageId"], "msg_1");
    }

    #[test]
    fn create_card_instance_request_serializes() {
        let req = CreateCardInstanceRequest {
            card_template_id: "382e4302-551d-4880-bf29-a30acfab2e71.schema".into(),
            out_track_id: "nomi_123_0".into(),
            callback_type: "STREAM".into(),
            card_data: CardData {
                card_param_map: Some(json!({})),
            },
            im_group_open_space_model: Some(SpaceModel { support_forward: true }),
            im_robot_open_space_model: Some(SpaceModel { support_forward: true }),
        };
        let json = serde_json::to_value(&req).unwrap();
        assert_eq!(json["cardTemplateId"], "382e4302-551d-4880-bf29-a30acfab2e71.schema");
        assert_eq!(json["outTrackId"], "nomi_123_0");
        assert_eq!(json["callbackType"], "STREAM");
        assert_eq!(json["imGroupOpenSpaceModel"]["supportForward"], true);
        assert_eq!(json["imRobotOpenSpaceModel"]["supportForward"], true);
    }

    #[test]
    fn streaming_write_request_serializes() {
        let req = StreamingWriteRequest {
            out_track_id: "card_1".into(),
            key: "msgContent".into(),
            content: "chunk text".into(),
            is_full: true,
            is_finalize: false,
            is_error: false,
            guid: "123_abc".into(),
        };
        let json = serde_json::to_value(&req).unwrap();
        assert_eq!(json["outTrackId"], "card_1");
        assert_eq!(json["key"], "msgContent");
        assert_eq!(json["content"], "chunk text");
        assert_eq!(json["isFull"], true);
        assert_eq!(json["isFinalize"], false);
        assert_eq!(json["isError"], false);
        assert_eq!(json["guid"], "123_abc");
    }

    #[test]
    fn session_webhook_request_serializes() {
        let req = SessionWebhookRequest {
            msgtype: "text".into(),
            text: Some(WebhookText { content: "Hi".into() }),
            markdown: None,
        };
        let json = serde_json::to_value(&req).unwrap();
        assert_eq!(json["msgtype"], "text");
        assert_eq!(json["text"]["content"], "Hi");
    }

    #[test]
    fn send_robot_message_request_serializes() {
        let req = SendRobotMessageRequest {
            msg_key: "sampleText".into(),
            msg_param: r#"{"content":"Hi"}"#.into(),
            robot_code: "app_key_1".into(),
            open_conversation_id: Some("conv_1".into()),
            user_ids: None,
        };
        let json = serde_json::to_value(&req).unwrap();
        assert_eq!(json["msgKey"], "sampleText");
        assert_eq!(json["robotCode"], "app_key_1");
        assert_eq!(json["openConversationId"], "conv_1");
    }

    // -- Callback encoding / decoding -----------------------------------------

    #[test]
    fn format_callback_no_params() {
        assert_eq!(format_dingtalk_callback("help.show", None), "system:help.show");
        assert_eq!(format_dingtalk_callback("pairing.show", None), "platform:pairing.show");
        assert_eq!(
            format_dingtalk_callback("chat.regenerate", None),
            "chat:chat.regenerate"
        );
        assert_eq!(format_dingtalk_callback("system.confirm", None), "chat:system.confirm");
    }

    #[test]
    fn format_callback_with_params() {
        let mut params = std::collections::HashMap::new();
        params.insert("callId".into(), "abc".into());
        params.insert("value".into(), "yes".into());
        let result = format_dingtalk_callback("system.confirm", Some(&params));
        assert!(result.starts_with("chat:system.confirm:"));
        assert!(result.contains("callId=abc"));
        assert!(result.contains("value=yes"));
    }

    #[test]
    fn parse_callback_roundtrip_no_params() {
        let encoded = format_dingtalk_callback("session.new", None);
        let (cat, action, params) = parse_dingtalk_callback(&encoded).unwrap();
        assert_eq!(cat, "system");
        assert_eq!(action, "session.new");
        assert!(params.is_none());
    }

    #[test]
    fn parse_callback_roundtrip_with_params() {
        let mut p = std::collections::HashMap::new();
        p.insert("agentType".into(), "acp".into());
        let encoded = format_dingtalk_callback("agent.select", Some(&p));
        let (cat, action, params) = parse_dingtalk_callback(&encoded).unwrap();
        assert_eq!(cat, "system");
        assert_eq!(action, "agent.select");
        assert_eq!(params.unwrap().get("agentType").unwrap(), "acp");
    }

    #[test]
    fn parse_callback_invalid() {
        assert!(parse_dingtalk_callback("invalid").is_none());
        assert!(parse_dingtalk_callback("unknown:action").is_none());
    }

    // -- chatId encoding / decoding -------------------------------------------

    #[test]
    fn encode_chat_id_group() {
        let result = encode_chat_id(Some("2"), Some("conv_123"), "staff_1");
        assert_eq!(result, "group:conv_123");
    }

    #[test]
    fn encode_chat_id_private() {
        let result = encode_chat_id(Some("1"), Some("cid_private_xyz"), "staff_1");
        assert_eq!(result, "user:staff_1");
    }

    #[test]
    fn encode_chat_id_no_conversation_type() {
        let result = encode_chat_id(None, Some("cid_whatever"), "staff_1");
        assert_eq!(result, "user:staff_1");
    }

    #[test]
    fn decode_chat_id_group() {
        let (is_group, id) = decode_chat_id("group:conv_123");
        assert!(is_group);
        assert_eq!(id, "conv_123");
    }

    #[test]
    fn decode_chat_id_user() {
        let (is_group, id) = decode_chat_id("user:staff_1");
        assert!(!is_group);
        assert_eq!(id, "staff_1");
    }

    #[test]
    fn decode_chat_id_unknown_prefix() {
        let (is_group, id) = decode_chat_id("raw_id");
        assert!(!is_group);
        assert_eq!(id, "raw_id");
    }

    // -- open_space_id --------------------------------------------------------

    #[test]
    fn build_open_space_id_group() {
        let result = build_open_space_id("group:conv_123");
        assert_eq!(result, "dtv1.card//IM_GROUP.conv_123");
    }

    #[test]
    fn build_open_space_id_user() {
        let result = build_open_space_id("user:staff_1");
        assert_eq!(result, "dtv1.card//IM_ROBOT.staff_1");
    }
}

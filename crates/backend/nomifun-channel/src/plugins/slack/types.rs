//! Serde structs for the Slack Web API and Socket Mode protocol.
//!
//! Only the subset of fields consumed by the plugin is modelled;
//! unknown fields are silently dropped by serde.

use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// Web API response envelope
// ---------------------------------------------------------------------------

/// Generic Slack Web API response wrapper.
///
/// All Slack Web API methods return JSON objects with `ok` + result fields.
#[derive(Debug, Clone, Deserialize)]
pub(crate) struct SlackResponse<T> {
    pub ok: bool,
    #[serde(flatten)]
    pub data: Option<T>,
    pub error: Option<String>,
}

// ---------------------------------------------------------------------------
// auth.test
// ---------------------------------------------------------------------------

/// Successful result from `auth.test`.
#[derive(Debug, Clone, Deserialize)]
#[allow(dead_code)]
pub(crate) struct AuthTestResult {
    pub user_id: Option<String>,
    pub user: Option<String>,
    pub team_id: Option<String>,
}

// ---------------------------------------------------------------------------
// apps.connections.open
// ---------------------------------------------------------------------------

/// Successful result from `apps.connections.open`.
#[derive(Debug, Clone, Deserialize)]
pub(crate) struct ConnectionOpenResult {
    pub url: Option<String>,
}

// ---------------------------------------------------------------------------
// chat.postMessage
// ---------------------------------------------------------------------------

/// Request body for `chat.postMessage`.
#[derive(Debug, Clone, Serialize)]
pub(crate) struct PostMessageRequest {
    pub channel: String,
    pub text: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thread_ts: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub blocks: Option<serde_json::Value>,
}

/// Result fields from `chat.postMessage`.
#[derive(Debug, Clone, Deserialize)]
pub(crate) struct PostMessageResult {
    pub ts: Option<String>,
}

// ---------------------------------------------------------------------------
// chat.update
// ---------------------------------------------------------------------------

/// Request body for `chat.update`.
#[derive(Debug, Clone, Serialize)]
pub(crate) struct UpdateMessageRequest {
    pub channel: String,
    pub ts: String,
    pub text: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub blocks: Option<serde_json::Value>,
}

/// Result fields from `chat.update` (we only check `ok`).
#[derive(Debug, Clone, Deserialize)]
#[allow(dead_code)]
pub(crate) struct UpdateMessageResult {
    pub ts: Option<String>,
}

// ---------------------------------------------------------------------------
// files.getUploadURLExternal (step 1 of the 3-step external upload flow)
// ---------------------------------------------------------------------------

/// Successful result from `files.getUploadURLExternal`.
#[derive(Debug, Clone, Deserialize)]
pub(crate) struct GetUploadUrlResult {
    pub upload_url: Option<String>,
    pub file_id: Option<String>,
}

// ---------------------------------------------------------------------------
// files.completeUploadExternal (step 3 of the 3-step external upload flow)
// ---------------------------------------------------------------------------

/// One entry in the `files` array of `files.completeUploadExternal`.
#[derive(Debug, Clone, Serialize)]
pub(crate) struct CompleteUploadFile {
    pub id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
}

/// Request body for `files.completeUploadExternal`.
#[derive(Debug, Clone, Serialize)]
pub(crate) struct CompleteUploadRequest {
    pub files: Vec<CompleteUploadFile>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub channel_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub initial_comment: Option<String>,
}

/// Result from `files.completeUploadExternal`.
#[derive(Debug, Clone, Deserialize)]
pub(crate) struct CompleteUploadResult {
    pub files: Option<Vec<CompletedFile>>,
}

/// A finalized file entry (we read its id as the returned handle).
#[derive(Debug, Clone, Deserialize)]
pub(crate) struct CompletedFile {
    pub id: Option<String>,
}

// ---------------------------------------------------------------------------
// Socket Mode envelopes
// ---------------------------------------------------------------------------

/// Top-level Socket Mode envelope received over the WebSocket.
///
/// The `type` field discriminates between `hello`, `events_api`,
/// `interactive`, and `disconnect`.
#[derive(Debug, Clone, Deserialize)]
pub(crate) struct SocketEnvelope {
    #[serde(rename = "type")]
    pub envelope_type: String,
    pub envelope_id: Option<String>,
    pub payload: Option<serde_json::Value>,
}

/// Socket Mode ACK — sent back immediately to acknowledge delivery.
#[derive(Debug, Clone, Serialize)]
pub(crate) struct SocketAck {
    pub envelope_id: String,
}

// ---------------------------------------------------------------------------
// Events API payload (inside SocketEnvelope.payload for type=events_api)
// ---------------------------------------------------------------------------

/// The `payload` of a Socket Mode `events_api` envelope.
#[derive(Debug, Clone, Deserialize)]
pub(crate) struct EventsApiPayload {
    pub event: Option<SlackEvent>,
}

/// A single Slack event (we handle `type=message`).
#[derive(Debug, Clone, Deserialize)]
pub(crate) struct SlackEvent {
    #[serde(rename = "type")]
    pub event_type: Option<String>,
    pub channel: Option<String>,
    pub channel_type: Option<String>,
    pub user: Option<String>,
    pub text: Option<String>,
    pub ts: Option<String>,
    pub thread_ts: Option<String>,
    pub subtype: Option<String>,
    pub bot_id: Option<String>,
}

// ---------------------------------------------------------------------------
// Interactive payload (inside SocketEnvelope.payload for type=interactive)
// ---------------------------------------------------------------------------

/// The `payload` of a Socket Mode `interactive` envelope.
#[derive(Debug, Clone, Deserialize)]
pub(crate) struct InteractivePayload {
    #[serde(rename = "type")]
    pub interaction_type: Option<String>,
    pub actions: Option<Vec<BlockAction>>,
    pub channel: Option<InteractiveChannel>,
    pub user: Option<InteractiveUser>,
    pub message: Option<InteractiveMessage>,
}

/// An action from `block_actions`.
#[derive(Debug, Clone, Deserialize)]
pub(crate) struct BlockAction {
    pub action_id: Option<String>,
    pub value: Option<String>,
}

/// Channel info in an interactive payload.
#[derive(Debug, Clone, Deserialize)]
pub(crate) struct InteractiveChannel {
    pub id: Option<String>,
}

/// User info in an interactive payload.
#[derive(Debug, Clone, Deserialize)]
pub(crate) struct InteractiveUser {
    pub id: Option<String>,
}

/// Message info in an interactive payload.
#[derive(Debug, Clone, Deserialize)]
pub(crate) struct InteractiveMessage {
    pub ts: Option<String>,
}

// ---------------------------------------------------------------------------
// Block Kit elements (for outgoing messages with buttons)
// ---------------------------------------------------------------------------

/// A Block Kit block (we only emit `actions` blocks).
#[derive(Debug, Clone, Serialize)]
pub(crate) struct ActionsBlock {
    #[serde(rename = "type")]
    pub block_type: String,
    pub elements: Vec<ButtonElement>,
}

/// A Block Kit button element.
#[derive(Debug, Clone, Serialize)]
pub(crate) struct ButtonElement {
    #[serde(rename = "type")]
    pub element_type: String,
    pub text: PlainTextObject,
    pub action_id: String,
    pub value: String,
}

/// Block Kit plain_text object.
#[derive(Debug, Clone, Serialize)]
pub(crate) struct PlainTextObject {
    #[serde(rename = "type")]
    pub text_type: String,
    pub text: String,
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn auth_test_result_parses() {
        let raw = json!({
            "ok": true,
            "user_id": "U12345",
            "user": "mybot",
            "team_id": "T12345"
        });
        let resp: SlackResponse<AuthTestResult> = serde_json::from_value(raw).unwrap();
        assert!(resp.ok);
        let data = resp.data.unwrap();
        assert_eq!(data.user_id.as_deref(), Some("U12345"));
        assert_eq!(data.user.as_deref(), Some("mybot"));
    }

    #[test]
    fn auth_test_error_parses() {
        let raw = json!({
            "ok": false,
            "error": "invalid_auth"
        });
        let resp: SlackResponse<AuthTestResult> = serde_json::from_value(raw).unwrap();
        assert!(!resp.ok);
        assert_eq!(resp.error.as_deref(), Some("invalid_auth"));
    }

    #[test]
    fn connection_open_parses() {
        let raw = json!({
            "ok": true,
            "url": "wss://wss-primary.slack.com/link/?ticket=xxx"
        });
        let resp: SlackResponse<ConnectionOpenResult> = serde_json::from_value(raw).unwrap();
        assert!(resp.ok);
        let data = resp.data.unwrap();
        assert_eq!(
            data.url.as_deref(),
            Some("wss://wss-primary.slack.com/link/?ticket=xxx")
        );
    }

    #[test]
    fn post_message_request_serializes() {
        let req = PostMessageRequest {
            channel: "C12345".into(),
            text: "Hello".into(),
            thread_ts: Some("1234567890.123456".into()),
            blocks: None,
        };
        let json = serde_json::to_value(&req).unwrap();
        assert_eq!(json["channel"], "C12345");
        assert_eq!(json["text"], "Hello");
        assert_eq!(json["thread_ts"], "1234567890.123456");
        assert!(json.get("blocks").is_none());
    }

    #[test]
    fn post_message_result_parses() {
        let raw = json!({
            "ok": true,
            "ts": "1234567890.123456"
        });
        let resp: SlackResponse<PostMessageResult> = serde_json::from_value(raw).unwrap();
        assert!(resp.ok);
        let data = resp.data.unwrap();
        assert_eq!(data.ts.as_deref(), Some("1234567890.123456"));
    }

    #[test]
    fn update_message_request_serializes() {
        let req = UpdateMessageRequest {
            channel: "C12345".into(),
            ts: "1234567890.123456".into(),
            text: "Updated".into(),
            blocks: None,
        };
        let json = serde_json::to_value(&req).unwrap();
        assert_eq!(json["channel"], "C12345");
        assert_eq!(json["ts"], "1234567890.123456");
        assert_eq!(json["text"], "Updated");
    }

    #[test]
    fn socket_envelope_hello_parses() {
        let raw = json!({"type": "hello"});
        let env: SocketEnvelope = serde_json::from_value(raw).unwrap();
        assert_eq!(env.envelope_type, "hello");
        assert!(env.envelope_id.is_none());
    }

    #[test]
    fn socket_envelope_events_api_parses() {
        let raw = json!({
            "type": "events_api",
            "envelope_id": "env_1",
            "payload": {
                "event": {
                    "type": "message",
                    "channel": "C12345",
                    "channel_type": "im",
                    "user": "U99999",
                    "text": "hi there",
                    "ts": "1700000000.000100"
                }
            }
        });
        let env: SocketEnvelope = serde_json::from_value(raw).unwrap();
        assert_eq!(env.envelope_type, "events_api");
        assert_eq!(env.envelope_id.as_deref(), Some("env_1"));
        let payload: EventsApiPayload =
            serde_json::from_value(env.payload.unwrap()).unwrap();
        let event = payload.event.unwrap();
        assert_eq!(event.event_type.as_deref(), Some("message"));
        assert_eq!(event.channel.as_deref(), Some("C12345"));
        assert_eq!(event.user.as_deref(), Some("U99999"));
        assert_eq!(event.text.as_deref(), Some("hi there"));
    }

    #[test]
    fn socket_envelope_interactive_parses() {
        let raw = json!({
            "type": "interactive",
            "envelope_id": "env_2",
            "payload": {
                "type": "block_actions",
                "actions": [{ "action_id": "system:session.new", "value": "system:session.new" }],
                "channel": { "id": "C12345" },
                "user": { "id": "U99999" },
                "message": { "ts": "1700000000.000200" }
            }
        });
        let env: SocketEnvelope = serde_json::from_value(raw).unwrap();
        assert_eq!(env.envelope_type, "interactive");
        let payload: InteractivePayload =
            serde_json::from_value(env.payload.unwrap()).unwrap();
        assert_eq!(payload.interaction_type.as_deref(), Some("block_actions"));
        let action = &payload.actions.unwrap()[0];
        assert_eq!(action.action_id.as_deref(), Some("system:session.new"));
    }

    #[test]
    fn socket_ack_serializes() {
        let ack = SocketAck {
            envelope_id: "env_1".into(),
        };
        let json = serde_json::to_value(&ack).unwrap();
        assert_eq!(json["envelope_id"], "env_1");
    }

    #[test]
    fn socket_envelope_disconnect_parses() {
        let raw = json!({"type": "disconnect"});
        let env: SocketEnvelope = serde_json::from_value(raw).unwrap();
        assert_eq!(env.envelope_type, "disconnect");
    }

    #[test]
    fn socket_event_with_bot_id_parses() {
        let raw = json!({
            "type": "message",
            "channel": "C12345",
            "user": "U12345",
            "text": "bot echo",
            "ts": "1700000000.000300",
            "bot_id": "B12345"
        });
        let event: SlackEvent = serde_json::from_value(raw).unwrap();
        assert_eq!(event.bot_id.as_deref(), Some("B12345"));
    }

    #[test]
    fn socket_event_with_subtype_parses() {
        let raw = json!({
            "type": "message",
            "channel": "C12345",
            "text": "bot says hi",
            "ts": "1700000000.000400",
            "subtype": "bot_message"
        });
        let event: SlackEvent = serde_json::from_value(raw).unwrap();
        assert_eq!(event.subtype.as_deref(), Some("bot_message"));
    }

    #[test]
    fn actions_block_serializes() {
        let block = ActionsBlock {
            block_type: "actions".into(),
            elements: vec![ButtonElement {
                element_type: "button".into(),
                text: PlainTextObject {
                    text_type: "plain_text".into(),
                    text: "Click me".into(),
                },
                action_id: "system:help.show".into(),
                value: "system:help.show".into(),
            }],
        };
        let json = serde_json::to_value(&block).unwrap();
        assert_eq!(json["type"], "actions");
        assert_eq!(json["elements"][0]["type"], "button");
        assert_eq!(json["elements"][0]["text"]["type"], "plain_text");
        assert_eq!(json["elements"][0]["text"]["text"], "Click me");
        assert_eq!(json["elements"][0]["action_id"], "system:help.show");
    }
}

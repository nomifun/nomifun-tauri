//! WeCom (企业微信智能机器人) long-connection protocol types and pure helpers.
//!
//! Transport is the "长连接 (WebSocket)" mode documented at
//! <https://developer.work.weixin.qq.com/document/path/101463>:
//! plain-text JSON frames of the shape
//! `{ "cmd": "...", "headers": { "req_id": "..." }, "body": { ... } }`.
//!
//! Unlike the "回调 (webhook)" mode, the channel itself is unencrypted (only
//! media downloads carry a per-resource `aeskey`), and there is no signature —
//! authentication happens by sending an `aibot_subscribe` command carrying
//! `bot_id` + `secret` right after the socket opens.
//!
//! Replies are sent with `aibot_send_msg` (active push): `chatid` is the sender
//! `userid` for single chats or the group `chatid` for groups, needs no
//! `chat_type` and no passthrough `req_id`, and is valid for 24h — so it is not
//! bound to the 5-second passive-reply window that `aibot_respond_msg` streams
//! are. Frame shapes verified against the official `@wecom/aibot-node-sdk`.
//!
//! Everything here is transport-agnostic and unit-tested; the socket loop lives
//! in [`super::plugin`].

use serde::Deserialize;
use serde_json::json;

use crate::types::{MessageContentType, PluginType, UnifiedIncomingMessage, UnifiedMessageContent, UnifiedUser};

/// Long-connection subscribe endpoint (single connection per bot; a new
/// connection kicks the previous one, which then receives `disconnected_event`).
pub const WECOM_WS_URL: &str = "wss://openws.work.weixin.qq.com";

/// Recommended application-level heartbeat interval (server drops idle sockets).
pub const WECOM_PING_INTERVAL_SECS: u64 = 30;

// ---------------------------------------------------------------------------
// Inbound envelope
// ---------------------------------------------------------------------------

/// Generic inbound frame. `body` is kept as raw JSON and re-parsed per `cmd`,
/// so an unknown command never breaks decoding of the ones we handle.
#[derive(Debug, Clone, Deserialize)]
pub struct WecomEnvelope {
    #[serde(default)]
    pub cmd: String,
    #[serde(default)]
    pub headers: WecomHeaders,
    #[serde(default)]
    pub body: serde_json::Value,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct WecomHeaders {
    #[serde(default)]
    pub req_id: String,
}

/// `aibot_msg_callback` body (the subset we consume in v1).
#[derive(Debug, Clone, Default, Deserialize)]
pub struct WecomMsgBody {
    #[serde(default)]
    pub msgid: String,
    /// Only present for group chats.
    #[serde(default)]
    pub chatid: String,
    /// "single" | "group".
    #[serde(default)]
    pub chattype: String,
    #[serde(default)]
    pub from: WecomFrom,
    #[serde(default)]
    pub msgtype: String,
    #[serde(default)]
    pub text: WecomText,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct WecomFrom {
    #[serde(default)]
    pub userid: String,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct WecomText {
    #[serde(default)]
    pub content: String,
}

/// `aibot_event_callback` body (`msgtype` is always `event`).
#[derive(Debug, Clone, Default, Deserialize)]
pub struct WecomEventBody {
    #[serde(default)]
    pub event: WecomEvent,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct WecomEvent {
    #[serde(default)]
    pub eventtype: String,
}

/// Commands the socket loop reacts to. Everything else is logged and ignored.
pub const CMD_MSG_CALLBACK: &str = "aibot_msg_callback";
pub const CMD_EVENT_CALLBACK: &str = "aibot_event_callback";
pub const CMD_SUBSCRIBE: &str = "aibot_subscribe";
pub const CMD_SEND_MSG: &str = "aibot_send_msg";
pub const CMD_PING: &str = "ping";

/// Event type that means another connection displaced ours — do NOT reconnect.
pub const EVENT_DISCONNECTED: &str = "disconnected_event";

// ---------------------------------------------------------------------------
// Parsing
// ---------------------------------------------------------------------------

/// Parse a raw WS text frame into an envelope (lenient: returns `None` only on
/// malformed JSON).
pub fn parse_envelope(text: &str) -> Option<WecomEnvelope> {
    serde_json::from_str::<WecomEnvelope>(text).ok()
}

/// The stable per-conversation key: `chatid` for groups, else the sender
/// `userid`. This doubles as the `aibot_send_msg` `chatid` at reply time.
pub fn chat_id_for(chattype: &str, chatid: &str, userid: &str) -> String {
    if chattype == "group" && !chatid.is_empty() {
        chatid.to_owned()
    } else {
        userid.to_owned()
    }
}

/// Outcome of decoding an `aibot_msg_callback` frame.
pub struct DecodedMessage {
    pub unified: UnifiedIncomingMessage,
    /// Deduplication key (`msgid`); empty when the platform omitted it.
    pub msgid: String,
}

/// Decode an `aibot_msg_callback` envelope into a unified message.
///
/// Returns `None` for message types we do not surface in v1 (anything other
/// than `text`) or when the body cannot be parsed.
pub fn decode_msg_callback(env: &WecomEnvelope, now: i64) -> Option<DecodedMessage> {
    let body: WecomMsgBody = serde_json::from_value(env.body.clone()).ok()?;

    // v1 handles text only. Media (image/file/voice/video) needs the per-URL
    // aeskey download+decrypt path and is deferred to v2.
    if body.msgtype != "text" {
        return None;
    }
    let text = body.text.content.trim().to_owned();
    if text.is_empty() {
        return None;
    }

    let userid = body.from.userid.clone();
    let chat_id = chat_id_for(&body.chattype, &body.chatid, &userid);
    if chat_id.is_empty() {
        return None;
    }

    let user = UnifiedUser {
        id: userid.clone(),
        username: None,
        display_name: if userid.is_empty() { "unknown".to_owned() } else { userid.clone() },
        avatar_url: None,
    };

    let content_type = if text.starts_with('/') {
        MessageContentType::Command
    } else {
        MessageContentType::Text
    };

    let unified = UnifiedIncomingMessage {
        id: if body.msgid.is_empty() { format!("wecom_{now}") } else { body.msgid.clone() },
        platform: PluginType::Wecom,
        chat_id,
        user,
        content: UnifiedMessageContent {
            content_type,
            text,
            attachments: None,
        },
        timestamp: now,
        reply_to_message_id: None,
        action: None,
        raw: None,
    };

    Some(DecodedMessage { unified, msgid: body.msgid })
}

/// Extract the event type from an `aibot_event_callback` envelope.
pub fn decode_event_type(env: &WecomEnvelope) -> Option<String> {
    let body: WecomEventBody = serde_json::from_value(env.body.clone()).ok()?;
    let ev = body.event.eventtype;
    if ev.is_empty() { None } else { Some(ev) }
}

// ---------------------------------------------------------------------------
// Outbound frame builders
// ---------------------------------------------------------------------------

/// `aibot_subscribe` — sent immediately after the socket opens to authenticate.
pub fn build_subscribe_frame(bot_id: &str, secret: &str, req_id: &str) -> String {
    json!({
        "cmd": CMD_SUBSCRIBE,
        "headers": { "req_id": req_id },
        "body": { "bot_id": bot_id, "secret": secret }
    })
    .to_string()
}

/// `ping` — application-level heartbeat.
pub fn build_ping_frame(req_id: &str) -> String {
    json!({
        "cmd": CMD_PING,
        "headers": { "req_id": req_id }
    })
    .to_string()
}

/// `aibot_send_msg` (active push, markdown) — the reply path.
///
/// `chatid` is the sender `userid` for single chats or the group `chatid` for
/// groups (i.e. exactly [`chat_id_for`]'s output). `req_id` is freshly
/// generated (not a passthrough). WeCom's active push has no `text` msgtype, so
/// plain text is delivered as markdown (which renders it verbatim).
pub fn build_send_msg_frame(chatid: &str, content: &str, req_id: &str) -> String {
    json!({
        "cmd": CMD_SEND_MSG,
        "headers": { "req_id": req_id },
        "body": {
            "chatid": chatid,
            "msgtype": "markdown",
            "markdown": { "content": content }
        }
    })
    .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chat_id_group_uses_chatid() {
        assert_eq!(chat_id_for("group", "wrkgrp1", "u1"), "wrkgrp1");
    }

    #[test]
    fn chat_id_single_uses_userid() {
        assert_eq!(chat_id_for("single", "", "u1"), "u1");
    }

    #[test]
    fn chat_id_group_without_chatid_falls_back_to_userid() {
        assert_eq!(chat_id_for("group", "", "u1"), "u1");
    }

    #[test]
    fn parse_envelope_ok() {
        let env = parse_envelope(r#"{"cmd":"ping","headers":{"req_id":"r1"},"body":{}}"#).unwrap();
        assert_eq!(env.cmd, "ping");
        assert_eq!(env.headers.req_id, "r1");
    }

    #[test]
    fn parse_envelope_missing_fields_defaults() {
        // Body/headers absent must not fail decoding.
        let env = parse_envelope(r#"{"cmd":"x"}"#).unwrap();
        assert_eq!(env.cmd, "x");
        assert_eq!(env.headers.req_id, "");
    }

    #[test]
    fn parse_envelope_invalid_json_none() {
        assert!(parse_envelope("not json").is_none());
    }

    #[test]
    fn decode_text_single_chat() {
        let env = parse_envelope(
            r#"{"cmd":"aibot_msg_callback","headers":{"req_id":"req-9"},
                "body":{"msgid":"m1","aibotid":"bot","chattype":"single",
                        "from":{"userid":"zhang"},"msgtype":"text",
                        "text":{"content":"hello robot"}}}"#,
        )
        .unwrap();
        let decoded = decode_msg_callback(&env, 1000).unwrap();
        assert_eq!(decoded.unified.id, "m1");
        assert_eq!(decoded.unified.chat_id, "zhang");
        assert_eq!(decoded.unified.user.id, "zhang");
        assert_eq!(decoded.unified.content.text, "hello robot");
        assert_eq!(decoded.unified.platform, PluginType::Wecom);
        assert_eq!(decoded.unified.content.content_type, MessageContentType::Text);
        assert_eq!(decoded.msgid, "m1");
    }

    #[test]
    fn decode_text_group_chat_uses_chatid() {
        let env = parse_envelope(
            r#"{"cmd":"aibot_msg_callback","headers":{"req_id":"r"},
                "body":{"msgid":"m2","chattype":"group","chatid":"grp42",
                        "from":{"userid":"li"},"msgtype":"text",
                        "text":{"content":"@Robot hi"}}}"#,
        )
        .unwrap();
        let decoded = decode_msg_callback(&env, 2000).unwrap();
        assert_eq!(decoded.unified.chat_id, "grp42");
        assert_eq!(decoded.unified.user.id, "li");
    }

    #[test]
    fn decode_slash_text_is_command() {
        let env = parse_envelope(
            r#"{"cmd":"aibot_msg_callback","headers":{"req_id":"r"},
                "body":{"msgid":"m","chattype":"single","from":{"userid":"u"},
                        "msgtype":"text","text":{"content":"/start"}}}"#,
        )
        .unwrap();
        let decoded = decode_msg_callback(&env, 1).unwrap();
        assert_eq!(decoded.unified.content.content_type, MessageContentType::Command);
        assert_eq!(decoded.unified.content.text, "/start");
    }

    #[test]
    fn decode_non_text_is_skipped() {
        let env = parse_envelope(
            r#"{"cmd":"aibot_msg_callback","headers":{"req_id":"r"},
                "body":{"msgid":"m","chattype":"single","from":{"userid":"u"},
                        "msgtype":"image","image":{"url":"http://x"}}}"#,
        )
        .unwrap();
        assert!(decode_msg_callback(&env, 1).is_none());
    }

    #[test]
    fn decode_empty_text_is_skipped() {
        let env = parse_envelope(
            r#"{"cmd":"aibot_msg_callback","headers":{"req_id":"r"},
                "body":{"msgid":"m","chattype":"single","from":{"userid":"u"},
                        "msgtype":"text","text":{"content":"   "}}}"#,
        )
        .unwrap();
        assert!(decode_msg_callback(&env, 1).is_none());
    }

    #[test]
    fn decode_missing_msgid_synthesizes_id() {
        let env = parse_envelope(
            r#"{"cmd":"aibot_msg_callback","headers":{"req_id":"r"},
                "body":{"chattype":"single","from":{"userid":"u"},
                        "msgtype":"text","text":{"content":"hi"}}}"#,
        )
        .unwrap();
        let decoded = decode_msg_callback(&env, 777).unwrap();
        assert_eq!(decoded.unified.id, "wecom_777");
        assert_eq!(decoded.msgid, "");
    }

    #[test]
    fn decode_event_type_ok() {
        let env = parse_envelope(
            r#"{"cmd":"aibot_event_callback","headers":{"req_id":"r"},
                "body":{"msgtype":"event","event":{"eventtype":"enter_chat"}}}"#,
        )
        .unwrap();
        assert_eq!(decode_event_type(&env).as_deref(), Some("enter_chat"));
    }

    #[test]
    fn build_subscribe_frame_shape() {
        let frame = build_subscribe_frame("botA", "secretB", "req-1");
        let v: serde_json::Value = serde_json::from_str(&frame).unwrap();
        assert_eq!(v["cmd"], CMD_SUBSCRIBE);
        assert_eq!(v["headers"]["req_id"], "req-1");
        assert_eq!(v["body"]["bot_id"], "botA");
        assert_eq!(v["body"]["secret"], "secretB");
    }

    #[test]
    fn build_ping_frame_shape() {
        let v: serde_json::Value = serde_json::from_str(&build_ping_frame("p1")).unwrap();
        assert_eq!(v["cmd"], CMD_PING);
        assert_eq!(v["headers"]["req_id"], "p1");
    }

    #[test]
    fn build_send_msg_frame_shape() {
        let v: serde_json::Value = serde_json::from_str(&build_send_msg_frame("zhang", "你好", "send-1")).unwrap();
        assert_eq!(v["cmd"], CMD_SEND_MSG);
        assert_eq!(v["headers"]["req_id"], "send-1");
        assert_eq!(v["body"]["chatid"], "zhang");
        assert_eq!(v["body"]["msgtype"], "markdown");
        assert_eq!(v["body"]["markdown"]["content"], "你好");
        // WeCom active push carries no chat_type field.
        assert!(v["body"].get("chat_type").is_none());
    }
}

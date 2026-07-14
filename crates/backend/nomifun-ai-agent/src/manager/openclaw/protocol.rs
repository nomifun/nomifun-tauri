use serde::{Deserialize, Serialize};
use serde_json::Value;

// Current operator/backend Gateway clients negotiate protocol v4. Older v3
// Gateways should be upgraded rather than silently downgrading the security
// and event contract used by remote control.
pub const OPENCLAW_MIN_PROTOCOL_VERSION: u32 = 4;
pub const OPENCLAW_MAX_PROTOCOL_VERSION: u32 = 4;

pub const CLIENT_ID: &str = "gateway-client";
pub const CLIENT_DISPLAY_NAME: &str = "Nomi-Backend";
pub const CLIENT_MODE: &str = "backend";
pub const CLIENT_VERSION: &str = "1.0.0";

// ── WebSocket Frame Types ───────────────────────────────────────────────

#[derive(Debug, Serialize)]
pub struct RequestFrame {
    #[serde(rename = "type")]
    pub type_: &'static str,
    pub id: String,
    pub method: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub params: Option<Value>,
}

#[derive(Debug, Deserialize)]
pub struct ResponseFrame {
    pub id: String,
    pub ok: bool,
    #[serde(default)]
    pub payload: Option<Value>,
    #[serde(default)]
    pub error: Option<ErrorShape>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct EventFrame {
    pub event: String,
    #[serde(default)]
    pub payload: Option<Value>,
    #[serde(default)]
    pub seq: Option<u64>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ErrorShape {
    pub code: String,
    pub message: String,
    #[serde(default)]
    pub details: Option<Value>,
    #[serde(default)]
    pub retryable: Option<bool>,
    #[serde(default, rename = "retryAfterMs")]
    pub retry_after_ms: Option<u64>,
}

/// Discriminator for incoming WebSocket messages.
#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum IncomingFrame {
    Res(ResponseFrame),
    Event(EventFrame),
}

// ── Connect Handshake ───────────────────────────────────────────────────

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ConnectParams {
    pub min_protocol: u32,
    pub max_protocol: u32,
    pub client: ClientInfo,
    pub caps: Vec<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub role: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub scopes: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub auth: Option<AuthParams>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub device: Option<DeviceAuthParams>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ClientInfo {
    pub id: &'static str,
    pub display_name: &'static str,
    pub version: &'static str,
    pub platform: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub device_family: Option<&'static str>,
    pub mode: &'static str,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AuthParams {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub token: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub device_token: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub password: Option<String>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DeviceAuthParams {
    pub id: String,
    pub public_key: String,
    pub signature: String,
    pub signed_at: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub nonce: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct HelloOk {
    #[serde(default, rename = "type")]
    pub type_: Option<String>,
    pub protocol: u32,
    pub server: ServerInfo,
    pub features: HelloFeatures,
    pub policy: PolicyInfo,
    pub auth: HelloAuthInfo,
}

#[derive(Debug, Deserialize)]
pub struct HelloFeatures {
    pub methods: Vec<String>,
    pub events: Vec<String>,
    #[serde(default)]
    pub capabilities: Option<Vec<String>>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ServerInfo {
    pub version: String,
    pub conn_id: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PolicyInfo {
    #[serde(default)]
    pub max_payload: Option<u64>,
    pub tick_interval_ms: u64,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HelloAuthInfo {
    #[serde(default)]
    pub device_token: Option<String>,
    pub role: String,
    pub scopes: Vec<String>,
    #[serde(default)]
    pub issued_at_ms: Option<i64>,
}

// ── Session Management ──────────────────────────────────────────────────

#[derive(Debug, Serialize)]
pub struct SessionsResolveParams {
    pub key: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionsResolveResponse {
    #[serde(default)]
    pub ok: Option<bool>,
    #[serde(default)]
    pub key: Option<String>,
    #[serde(default)]
    pub session_id: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct SessionsResetParams {
    pub key: String,
    pub reason: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionsResetResponse {
    #[serde(default)]
    pub ok: Option<bool>,
    #[serde(default)]
    pub key: Option<String>,
    #[serde(default)]
    pub session_id: Option<String>,
    #[serde(default)]
    pub entry: Option<Value>,
}

// ── Chat Operations ─────────────────────────────────────────────────────

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ChatSendParams {
    pub session_key: String,
    pub message: String,
    pub idempotency_key: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub attachments: Option<Vec<Value>>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ChatAbortParams {
    pub session_key: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub run_id: Option<String>,
}

// ── Gateway Events ──────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ChatEvent {
    #[serde(default)]
    pub run_id: Option<String>,
    #[serde(default)]
    pub session_key: Option<String>,
    #[serde(default)]
    pub seq: Option<u64>,
    pub state: ChatEventState,
    #[serde(default)]
    pub message: Option<Value>,
    /// v4-only: incremental delta text on `state == "delta"` frames. Required by the
    /// v4 schema (`ChatDeltaEventSchema`), absent on v3 Gateways. When present it is
    /// the authoritative delta — `message` may be missing or carry only metadata.
    #[serde(default)]
    pub delta_text: Option<String>,
    /// v4-only: when true the delta replaces the accumulated text instead of appending.
    #[serde(default)]
    pub replace: Option<bool>,
    #[serde(default)]
    pub error_message: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ChatEventState {
    Delta,
    Final,
    Aborted,
    Error,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentEvent {
    #[serde(default)]
    pub run_id: Option<String>,
    #[serde(default)]
    pub session_key: Option<String>,
    #[serde(default)]
    pub seq: Option<u64>,
    pub stream: String,
    #[serde(default)]
    pub data: Value,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ApprovalRequestedEvent {
    pub id: String,
    #[serde(default)]
    pub request: Option<ApprovalRequest>,
    /// Some Gateway versions emit the reviewer-safe request fields directly
    /// beside `id`; newer versions wrap them in `request`.
    #[serde(flatten)]
    pub direct_request: ApprovalRequest,
    #[serde(default)]
    pub created_at_ms: Option<i64>,
    #[serde(default)]
    pub expires_at_ms: Option<i64>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ApprovalRequest {
    #[serde(default)]
    pub command: Option<String>,
    #[serde(default)]
    pub command_preview: Option<String>,
    #[serde(default)]
    pub command_argv: Option<Vec<String>>,
    #[serde(default)]
    pub host: Option<String>,
    #[serde(default)]
    pub node_id: Option<String>,
    #[serde(default)]
    pub agent_id: Option<String>,
    #[serde(default)]
    pub session_key: Option<String>,
    #[serde(default)]
    pub warning_text: Option<String>,
    #[serde(default)]
    pub allowed_decisions: Option<Vec<String>>,
    #[serde(default)]
    pub unavailable_decisions: Option<Vec<String>>,
}

// ── Challenge Event ─────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct ChallengePayload {
    #[serde(default)]
    pub nonce: Option<String>,
}

// ── URL Normalization ───────────────────────────────────────────────────

pub fn normalize_ws_url(host: &str, port: u16) -> String {
    let raw = if host.contains("://") {
        format!("{host}:{port}")
    } else {
        format!("ws://{host}:{port}")
    };

    raw.replace("https://", "wss://").replace("http://", "ws://")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_ws_url_bare_host() {
        assert_eq!(normalize_ws_url("127.0.0.1", 18789), "ws://127.0.0.1:18789");
        assert_eq!(normalize_ws_url("localhost", 9999), "ws://localhost:9999");
    }

    #[test]
    fn normalize_ws_url_with_scheme() {
        assert_eq!(normalize_ws_url("https://remote.host", 443), "wss://remote.host:443");
        assert_eq!(normalize_ws_url("http://local.host", 8080), "ws://local.host:8080");
        assert_eq!(normalize_ws_url("ws://already.ws", 18789), "ws://already.ws:18789");
    }

    #[test]
    fn request_frame_serializes() {
        let frame = RequestFrame {
            type_: "req",
            id: "abc-123".into(),
            method: "connect".into(),
            params: Some(serde_json::json!({"key": "value"})),
        };
        let json = serde_json::to_value(&frame).unwrap();
        assert_eq!(json["type"], "req");
        assert_eq!(json["id"], "abc-123");
        assert_eq!(json["method"], "connect");
    }

    #[test]
    fn response_frame_deserializes_ok() {
        let json = serde_json::json!({
            "id": "abc-123",
            "ok": true,
            "payload": { "protocol": 3 }
        });
        let frame: ResponseFrame = serde_json::from_value(json).unwrap();
        assert!(frame.ok);
        assert_eq!(frame.id, "abc-123");
        assert!(frame.payload.is_some());
        assert!(frame.error.is_none());
    }

    #[test]
    fn response_frame_deserializes_error() {
        let json = serde_json::json!({
            "id": "abc-123",
            "ok": false,
            "error": { "code": "AUTH_FAILED", "message": "bad token" }
        });
        let frame: ResponseFrame = serde_json::from_value(json).unwrap();
        assert!(!frame.ok);
        let err = frame.error.unwrap();
        assert_eq!(err.code, "AUTH_FAILED");
    }

    #[test]
    fn incoming_frame_dispatch() {
        let res_json = serde_json::json!({
            "type": "res",
            "id": "x",
            "ok": true,
        });
        let parsed: IncomingFrame = serde_json::from_value(res_json).unwrap();
        assert!(matches!(parsed, IncomingFrame::Res(_)));

        let evt_json = serde_json::json!({
            "type": "event",
            "event": "chat",
            "payload": {},
        });
        let parsed: IncomingFrame = serde_json::from_value(evt_json).unwrap();
        assert!(matches!(parsed, IncomingFrame::Event(_)));
    }

    #[test]
    fn chat_event_state_deserializes() {
        let json = serde_json::json!({
            "state": "delta",
            "message": { "content": "hello" },
        });
        let event: ChatEvent = serde_json::from_value(json).unwrap();
        assert_eq!(event.state, ChatEventState::Delta);
        assert!(event.delta_text.is_none());
        assert!(event.replace.is_none());
    }

    #[test]
    fn chat_event_v4_delta_with_delta_text() {
        let json = serde_json::json!({
            "runId": "run-1",
            "sessionKey": "sk-1",
            "seq": 0,
            "state": "delta",
            "deltaText": "Hello",
            "replace": false,
        });
        let event: ChatEvent = serde_json::from_value(json).unwrap();
        assert_eq!(event.state, ChatEventState::Delta);
        assert_eq!(event.delta_text.as_deref(), Some("Hello"));
        assert_eq!(event.replace, Some(false));
        assert!(event.message.is_none());
    }

    #[test]
    fn connect_params_serializes() {
        let params = ConnectParams {
            min_protocol: OPENCLAW_MIN_PROTOCOL_VERSION,
            max_protocol: OPENCLAW_MAX_PROTOCOL_VERSION,
            client: ClientInfo {
                id: CLIENT_ID,
                display_name: CLIENT_DISPLAY_NAME,
                version: CLIENT_VERSION,
                platform: "darwin",
                device_family: Some("darwin"),
                mode: CLIENT_MODE,
            },
            caps: vec!["tool-events"],
            role: Some("operator".into()),
            scopes: Some(vec!["operator.admin".into()]),
            auth: None,
            device: None,
        };
        let json = serde_json::to_value(&params).unwrap();
        assert_eq!(json["minProtocol"], 4);
        assert_eq!(json["maxProtocol"], 4);
        assert_eq!(json["client"]["id"], "gateway-client");
        assert_eq!(json["client"]["deviceFamily"], "darwin");
        assert_eq!(json["caps"][0], "tool-events");
    }

    #[test]
    fn auth_params_serialize_device_token_as_camel_case() {
        let auth = AuthParams {
            token: Some("shared-token".into()),
            device_token: Some("device-token".into()),
            password: None,
        };
        let json = serde_json::to_value(auth).unwrap();

        assert_eq!(json["token"], "shared-token");
        assert_eq!(json["deviceToken"], "device-token");
        assert!(json.get("device_token").is_none());
    }

    #[test]
    fn hello_ok_rejects_incomplete_payload() {
        let json = serde_json::json!({});
        assert!(serde_json::from_value::<HelloOk>(json).is_err());
    }

    #[test]
    fn hello_ok_deserializes_full() {
        let json = serde_json::json!({
            "type": "hello-ok",
            "protocol": 4,
            "server": { "version": "1.2.0", "connId": "conn-1" },
            "features": {
                "methods": ["chat.send"],
                "events": ["chat"],
                "capabilities": ["chat.send.routing"]
            },
            "policy": { "maxPayload": 26214400, "tickIntervalMs": 30000 },
            "auth": {
                "deviceToken": "tok123",
                "role": "operator",
                "scopes": ["operator.admin"]
            },
        });
        let hello: HelloOk = serde_json::from_value(json).unwrap();
        assert_eq!(hello.protocol, 4);
        assert_eq!(hello.policy.tick_interval_ms, 30000);
        assert_eq!(hello.auth.device_token.as_deref(), Some("tok123"));
        assert_eq!(hello.features.methods, ["chat.send"]);
        assert_eq!(hello.features.events, ["chat"]);
        assert_eq!(
            hello
                .features
                .capabilities
                .as_ref()
                .and_then(|values| values.first())
                .map(String::as_str),
            Some("chat.send.routing")
        );
    }

    #[test]
    fn sessions_resolve_serializes() {
        let params = SessionsResolveParams { key: "sk-prev".into() };
        let json = serde_json::to_value(&params).unwrap();
        assert_eq!(json["key"], "sk-prev");
    }

    #[test]
    fn sessions_resolve_response_deserializes() {
        let json = serde_json::json!({
            "key": "sk-resolved",
            "sessionId": "sess-42"
        });
        let resp: SessionsResolveResponse = serde_json::from_value(json).unwrap();
        assert_eq!(resp.key.as_deref(), Some("sk-resolved"));
        assert_eq!(resp.session_id.unwrap(), "sess-42");
    }

    #[test]
    fn sessions_reset_serializes() {
        let params = SessionsResetParams {
            key: "conv-1".into(),
            reason: "new".into(),
        };
        let json = serde_json::to_value(&params).unwrap();
        assert_eq!(json["key"], "conv-1");
        assert_eq!(json["reason"], "new");
    }

    #[test]
    fn sessions_reset_response_deserializes_current_gateway_shape() {
        let json = serde_json::json!({
            "ok": true,
            "key": "agent:main:conv-1",
            "entry": {
                "sessionId": "sess-42"
            }
        });
        let resp: SessionsResetResponse = serde_json::from_value(json).unwrap();
        assert_eq!(resp.ok, Some(true));
        assert_eq!(resp.key.as_deref(), Some("agent:main:conv-1"));
        assert_eq!(
            resp.entry
                .as_ref()
                .and_then(|entry| entry.get("sessionId"))
                .and_then(Value::as_str),
            Some("sess-42")
        );
    }

    #[test]
    fn chat_send_params_serializes() {
        let params = ChatSendParams {
            session_key: "sk-1".into(),
            message: "hello".into(),
            idempotency_key: "idem-1".into(),
            attachments: None,
        };
        let json = serde_json::to_value(&params).unwrap();
        assert_eq!(json["sessionKey"], "sk-1");
        assert_eq!(json["message"], "hello");
        assert_eq!(json["idempotencyKey"], "idem-1");
        assert!(json.get("attachments").is_none());
    }

    #[test]
    fn approval_request_deserializes() {
        let json = serde_json::json!({
            "id": "req-1",
            "request": {
                "command": "git status",
                "commandArgv": ["git", "status"],
                "host": "gateway",
                "allowedDecisions": ["allow-once", "deny"]
            },
            "createdAtMs": 1,
            "expiresAtMs": 2
        });
        let event: ApprovalRequestedEvent = serde_json::from_value(json).unwrap();
        assert_eq!(event.id, "req-1");
        let request = event.request.unwrap();
        assert_eq!(request.command.as_deref(), Some("git status"));
        assert_eq!(request.allowed_decisions.unwrap().len(), 2);
    }

    #[test]
    fn approval_request_deserializes_direct_shape() {
        let json = serde_json::json!({
            "id": "req-2",
            "command": "cargo test",
            "sessionKey": "agent:main:test",
            "allowedDecisions": ["allow-once", "deny"],
            "expiresAtMs": 2
        });
        let event: ApprovalRequestedEvent = serde_json::from_value(json).unwrap();
        assert!(event.request.is_none());
        assert_eq!(event.direct_request.command.as_deref(), Some("cargo test"));
        assert_eq!(
            event.direct_request.session_key.as_deref(),
            Some("agent:main:test")
        );
    }
}

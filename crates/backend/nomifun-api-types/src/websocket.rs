use serde::{Deserialize, Serialize};

/// WebSocket message envelope.
///
/// All WebSocket communication follows this format: a `name` field
/// identifying the event type and a `data` field with the payload.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WebSocketMessage<T> {
    pub name: String,
    pub data: T,
}

impl<T> WebSocketMessage<T> {
    pub fn new(name: impl Into<String>, data: T) -> Self {
        Self {
            name: name.into(),
            data,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_websocket_message_new() {
        let msg = WebSocketMessage::new("chat:message", "hello");
        assert_eq!(msg.name, "chat:message");
        assert_eq!(msg.data, "hello");
    }

    #[test]
    fn test_websocket_message_serialization() {
        let msg = WebSocketMessage::new("status:update", json!({"online": true}));
        let json = serde_json::to_value(&msg).unwrap();
        assert_eq!(json["name"], "status:update");
        assert_eq!(json["data"]["online"], true);
    }

    #[test]
    fn test_websocket_message_deserialization() {
        let raw = r#"{"name":"ping","data":null}"#;
        let msg: WebSocketMessage<serde_json::Value> = serde_json::from_str(raw).unwrap();
        assert_eq!(msg.name, "ping");
        assert!(msg.data.is_null());
    }

    #[test]
    fn test_websocket_message_with_complex_data() {
        #[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
        struct Payload {
            count: u32,
            items: Vec<String>,
        }
        let payload = Payload {
            count: 2,
            items: vec!["a".into(), "b".into()],
        };
        let msg = WebSocketMessage::new("list:update", payload.clone());
        let serialized = serde_json::to_string(&msg).unwrap();
        let deserialized: WebSocketMessage<Payload> = serde_json::from_str(&serialized).unwrap();
        assert_eq!(deserialized.name, "list:update");
        assert_eq!(deserialized.data, payload);
    }

    #[test]
    fn test_websocket_message_rejects_unknown_envelope_fields() {
        let raw = r#"{"name":"ping","data":null,"event":"legacy"}"#;
        assert!(serde_json::from_str::<WebSocketMessage<serde_json::Value>>(raw).is_err());
    }
}

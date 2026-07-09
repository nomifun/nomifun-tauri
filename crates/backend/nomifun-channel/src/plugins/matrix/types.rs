//! Matrix Client-Server API v3 response types for the handwritten
//! (no-E2EE, reqwest-only) Route B implementation.
//!
//! Only the subset actually consumed by the plugin is modelled.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

// ---------------------------------------------------------------------------
// /sync response (subset)
// ---------------------------------------------------------------------------

/// Top-level response from `GET /_matrix/client/v3/sync`.
#[derive(Debug, Deserialize)]
pub struct SyncResponse {
    pub next_batch: String,
    #[serde(default)]
    pub rooms: Option<SyncRooms>,
}

/// Rooms section of a sync response.
#[derive(Debug, Deserialize)]
pub struct SyncRooms {
    #[serde(default)]
    pub join: HashMap<String, JoinedRoom>,
}

/// A joined room's sync payload.
#[derive(Debug, Deserialize)]
pub struct JoinedRoom {
    #[serde(default)]
    pub timeline: Option<Timeline>,
}

/// Timeline section within a joined room.
#[derive(Debug, Deserialize)]
pub struct Timeline {
    #[serde(default)]
    pub events: Vec<TimelineEvent>,
}

/// A single timeline event (room event envelope).
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct TimelineEvent {
    /// Matrix event type, e.g. `m.room.message`, `m.room.encrypted`.
    #[serde(rename = "type")]
    pub event_type: String,
    /// Unique event ID (`$...`).
    #[serde(default)]
    pub event_id: Option<String>,
    /// Sender MXID (`@user:server`).
    #[serde(default)]
    pub sender: Option<String>,
    /// Origin server timestamp (milliseconds since Unix epoch).
    #[serde(default)]
    pub origin_server_ts: Option<i64>,
    /// Event content (type-specific).
    #[serde(default)]
    pub content: Option<serde_json::Value>,
}

// ---------------------------------------------------------------------------
// m.room.message content
// ---------------------------------------------------------------------------

/// Parsed `content` of an `m.room.message` event.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct RoomMessageContent {
    /// Message type: `m.text`, `m.image`, `m.file`, etc.
    pub msgtype: String,
    /// Plain-text body (always present per spec).
    #[serde(default)]
    pub body: String,
    /// Optional HTML formatted body.
    #[serde(default)]
    pub formatted_body: Option<String>,
    /// Optional format field (e.g. `org.matrix.custom.html`).
    #[serde(default)]
    pub format: Option<String>,
    /// Relation metadata (`m.relates_to`).
    #[serde(rename = "m.relates_to", default)]
    pub relates_to: Option<RelatesTo>,
    /// For edits: the replacement content.
    #[serde(rename = "m.new_content", default)]
    pub new_content: Option<Box<NewContent>>,
}

/// `m.relates_to` relation metadata.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct RelatesTo {
    /// Relation type, e.g. `m.replace`, `m.thread`.
    #[serde(default)]
    pub rel_type: Option<String>,
    /// The event being related to.
    #[serde(default)]
    pub event_id: Option<String>,
}

/// `m.new_content` for edit events.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct NewContent {
    pub msgtype: String,
    #[serde(default)]
    pub body: String,
    #[serde(default)]
    pub formatted_body: Option<String>,
    #[serde(default)]
    pub format: Option<String>,
}

// ---------------------------------------------------------------------------
// /account/whoami response
// ---------------------------------------------------------------------------

/// Response from `GET /_matrix/client/v3/account/whoami`.
#[derive(Debug, Deserialize)]
pub struct WhoAmIResponse {
    pub user_id: String,
    #[serde(default)]
    pub device_id: Option<String>,
}

// ---------------------------------------------------------------------------
// Send message response
// ---------------------------------------------------------------------------

/// Response from `PUT /_matrix/client/v3/rooms/{roomId}/send/{eventType}/{txnId}`.
#[derive(Debug, Deserialize)]
pub struct SendEventResponse {
    pub event_id: String,
}

// ---------------------------------------------------------------------------
// Media upload response
// ---------------------------------------------------------------------------

/// Response from `POST /_matrix/media/v3/upload`.
#[derive(Debug, Deserialize)]
pub struct MediaUploadResponse {
    /// The `mxc://` content URI of the uploaded bytes.
    pub content_uri: String,
}

// ---------------------------------------------------------------------------
// Display name response (for bot info)
// ---------------------------------------------------------------------------

/// Response from `GET /_matrix/client/v3/profile/{userId}`.
#[derive(Debug, Deserialize)]
pub struct ProfileResponse {
    #[serde(default)]
    pub displayname: Option<String>,
    #[serde(default)]
    pub avatar_url: Option<String>,
}

// ---------------------------------------------------------------------------
// Helper: extract effective text from a message event
// ---------------------------------------------------------------------------

impl RoomMessageContent {
    /// Returns the effective text body of this message, accounting for edits.
    ///
    /// If this is an edit (`m.relates_to.rel_type == "m.replace"` with
    /// `m.new_content`), the replacement body is returned.  Otherwise the
    /// top-level `body` is used.
    pub fn effective_body(&self) -> &str {
        if let Some(rel) = &self.relates_to {
            if rel.rel_type.as_deref() == Some("m.replace") {
                if let Some(nc) = &self.new_content {
                    return &nc.body;
                }
            }
        }
        &self.body
    }

    /// Whether this message is an edit of another message.
    pub fn is_edit(&self) -> bool {
        self.relates_to
            .as_ref()
            .and_then(|r| r.rel_type.as_deref())
            == Some("m.replace")
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parse_sync_response_minimal() {
        let raw = json!({
            "next_batch": "s123_456",
        });
        let resp: SyncResponse = serde_json::from_value(raw).unwrap();
        assert_eq!(resp.next_batch, "s123_456");
        assert!(resp.rooms.is_none());
    }

    #[test]
    fn parse_sync_response_with_text_message() {
        let raw = json!({
            "next_batch": "s789",
            "rooms": {
                "join": {
                    "!room1:example.com": {
                        "timeline": {
                            "events": [
                                {
                                    "type": "m.room.message",
                                    "event_id": "$evt1",
                                    "sender": "@alice:example.com",
                                    "origin_server_ts": 1700000000000_i64,
                                    "content": {
                                        "msgtype": "m.text",
                                        "body": "Hello world"
                                    }
                                }
                            ]
                        }
                    }
                }
            }
        });
        let resp: SyncResponse = serde_json::from_value(raw).unwrap();
        assert_eq!(resp.next_batch, "s789");
        let rooms = resp.rooms.unwrap();
        let room = rooms.join.get("!room1:example.com").unwrap();
        let events = &room.timeline.as_ref().unwrap().events;
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].event_type, "m.room.message");
        assert_eq!(events[0].sender.as_deref(), Some("@alice:example.com"));

        let content: RoomMessageContent =
            serde_json::from_value(events[0].content.clone().unwrap()).unwrap();
        assert_eq!(content.msgtype, "m.text");
        assert_eq!(content.body, "Hello world");
        assert!(!content.is_edit());
        assert_eq!(content.effective_body(), "Hello world");
    }

    #[test]
    fn parse_edit_message() {
        let raw = json!({
            "msgtype": "m.text",
            "body": "* Updated text",
            "m.relates_to": {
                "rel_type": "m.replace",
                "event_id": "$original"
            },
            "m.new_content": {
                "msgtype": "m.text",
                "body": "Updated text"
            }
        });
        let content: RoomMessageContent = serde_json::from_value(raw).unwrap();
        assert!(content.is_edit());
        assert_eq!(content.effective_body(), "Updated text");
        assert_eq!(
            content.relates_to.as_ref().unwrap().event_id.as_deref(),
            Some("$original")
        );
    }

    #[test]
    fn parse_non_edit_message() {
        let raw = json!({
            "msgtype": "m.text",
            "body": "Plain message"
        });
        let content: RoomMessageContent = serde_json::from_value(raw).unwrap();
        assert!(!content.is_edit());
        assert_eq!(content.effective_body(), "Plain message");
    }

    #[test]
    fn parse_encrypted_event_type() {
        let raw = json!({
            "type": "m.room.encrypted",
            "event_id": "$enc1",
            "sender": "@bob:example.com",
            "origin_server_ts": 1700000000000_i64,
            "content": {
                "algorithm": "m.megolm.v1.aes-sha2",
                "ciphertext": "AwgAEn..."
            }
        });
        let event: TimelineEvent = serde_json::from_value(raw).unwrap();
        assert_eq!(event.event_type, "m.room.encrypted");
    }

    #[test]
    fn parse_whoami_response() {
        let raw = json!({
            "user_id": "@bot:matrix.org",
            "device_id": "ABCDEF"
        });
        let resp: WhoAmIResponse = serde_json::from_value(raw).unwrap();
        assert_eq!(resp.user_id, "@bot:matrix.org");
        assert_eq!(resp.device_id.as_deref(), Some("ABCDEF"));
    }

    #[test]
    fn parse_send_event_response() {
        let raw = json!({
            "event_id": "$newevt"
        });
        let resp: SendEventResponse = serde_json::from_value(raw).unwrap();
        assert_eq!(resp.event_id, "$newevt");
    }

    #[test]
    fn parse_profile_response() {
        let raw = json!({
            "displayname": "Bot User",
            "avatar_url": "mxc://example.com/abc"
        });
        let resp: ProfileResponse = serde_json::from_value(raw).unwrap();
        assert_eq!(resp.displayname.as_deref(), Some("Bot User"));
        assert_eq!(resp.avatar_url.as_deref(), Some("mxc://example.com/abc"));
    }

    #[test]
    fn parse_profile_response_minimal() {
        let raw = json!({});
        let resp: ProfileResponse = serde_json::from_value(raw).unwrap();
        assert!(resp.displayname.is_none());
        assert!(resp.avatar_url.is_none());
    }

    #[test]
    fn sync_response_empty_rooms() {
        let raw = json!({
            "next_batch": "s0",
            "rooms": {
                "join": {}
            }
        });
        let resp: SyncResponse = serde_json::from_value(raw).unwrap();
        assert!(resp.rooms.unwrap().join.is_empty());
    }

    #[test]
    fn message_with_html_format() {
        let raw = json!({
            "msgtype": "m.text",
            "body": "Hello",
            "format": "org.matrix.custom.html",
            "formatted_body": "<b>Hello</b>"
        });
        let content: RoomMessageContent = serde_json::from_value(raw).unwrap();
        assert_eq!(content.format.as_deref(), Some("org.matrix.custom.html"));
        assert_eq!(content.formatted_body.as_deref(), Some("<b>Hello</b>"));
    }
}

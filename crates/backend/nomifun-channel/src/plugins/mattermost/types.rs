//! Mattermost API types: WebSocket events, REST request/response bodies.
//!
//! The Mattermost API v4 WebSocket delivers events as JSON frames.  The
//! `"posted"` event embeds the post payload as a JSON-encoded *string* inside
//! `data.post`, which must be double-deserialized.

use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// REST: GET /api/v4/users/me
// ---------------------------------------------------------------------------

/// Response from `GET /api/v4/users/me`.
#[derive(Debug, Clone, Deserialize)]
pub(crate) struct MmUser {
    pub id: String,
    pub username: String,
}

// ---------------------------------------------------------------------------
// REST: POST /api/v4/posts  (create)
// ---------------------------------------------------------------------------

/// Request body for `POST /api/v4/posts`.
#[derive(Debug, Clone, Serialize)]
pub(crate) struct CreatePostRequest {
    pub channel_id: String,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub root_id: Option<String>,
    /// Ids of previously uploaded files to attach (omitted when empty).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub file_ids: Vec<String>,
}

/// Response from `POST /api/v4/posts`.
#[derive(Debug, Clone, Deserialize)]
pub(crate) struct CreatePostResponse {
    pub id: String,
}

// ---------------------------------------------------------------------------
// REST: POST /api/v4/files  (upload)
// ---------------------------------------------------------------------------

/// Response from `POST /api/v4/files`. Only the uploaded file ids are needed.
#[derive(Debug, Clone, Deserialize)]
pub(crate) struct FileUploadResponse {
    pub file_infos: Vec<FileInfo>,
}

/// A single uploaded file's metadata.
#[derive(Debug, Clone, Deserialize)]
pub(crate) struct FileInfo {
    pub id: String,
}

// ---------------------------------------------------------------------------
// REST: PUT /api/v4/posts/{post_id}  (update)
// ---------------------------------------------------------------------------

/// Request body for `PUT /api/v4/posts/{post_id}`.
#[derive(Debug, Clone, Serialize)]
pub(crate) struct UpdatePostRequest {
    pub id: String,
    pub message: String,
}

// ---------------------------------------------------------------------------
// WebSocket: authentication challenge
// ---------------------------------------------------------------------------

/// Outgoing auth frame sent immediately after WS connect.
///
/// ```json
/// {"seq":1,"action":"authentication_challenge","data":{"token":"<token>"}}
/// ```
#[derive(Debug, Clone, Serialize)]
pub(crate) struct WsAuthChallenge {
    pub seq: u64,
    pub action: &'static str,
    pub data: WsAuthData,
}

/// `data` field of the auth challenge frame.
#[derive(Debug, Clone, Serialize)]
pub(crate) struct WsAuthData {
    pub token: String,
}

impl WsAuthChallenge {
    pub fn new(token: &str) -> Self {
        Self {
            seq: 1,
            action: "authentication_challenge",
            data: WsAuthData {
                token: token.to_owned(),
            },
        }
    }
}

// ---------------------------------------------------------------------------
// WebSocket: inbound event envelope
// ---------------------------------------------------------------------------

/// Top-level WS event frame.
///
/// Not all events carry `data`; we only care about `"posted"`.
#[derive(Debug, Clone, Deserialize)]
pub(crate) struct WsEvent {
    pub event: Option<String>,
    pub data: Option<WsEventData>,
}

/// Typed `data` payload for a `"posted"` event.
///
/// `post` is a JSON-encoded *string* that must be double-deserialized into
/// [`MmPost`].  `channel_type` distinguishes DMs (`"D"`) from open/private/
/// group channels.  `mentions` is a JSON-encoded array of user ids who were
/// mentioned.
#[derive(Debug, Clone, Deserialize)]
pub(crate) struct WsEventData {
    /// JSON-encoded string of the post body.
    pub post: Option<String>,
    /// Channel type: `"D"` (DM), `"O"` (open), `"P"` (private), `"G"` (group DM).
    pub channel_type: Option<String>,
    /// Display name of the sender.
    pub sender_name: Option<String>,
    /// JSON-encoded array of user-id strings who were `@`-mentioned.
    pub mentions: Option<String>,
}

// ---------------------------------------------------------------------------
// WebSocket: Post (double-deserialized from data.post)
// ---------------------------------------------------------------------------

/// A Mattermost post object, deserialized from the JSON string inside
/// `WsEventData.post`.
#[derive(Debug, Clone, Deserialize)]
#[allow(dead_code)]
pub(crate) struct MmPost {
    pub id: String,
    pub channel_id: String,
    pub user_id: String,
    #[serde(default)]
    pub message: String,
    /// Root post id when this post is a reply in a thread.
    #[serde(default)]
    pub root_id: String,
    /// List of attached file ids (empty vec if none).
    #[serde(default)]
    pub file_ids: Vec<String>,
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn mm_user_deserializes() {
        let raw = json!({"id": "abc123", "username": "nomibot"});
        let user: MmUser = serde_json::from_value(raw).unwrap();
        assert_eq!(user.id, "abc123");
        assert_eq!(user.username, "nomibot");
    }

    #[test]
    fn create_post_request_serializes() {
        let req = CreatePostRequest {
            channel_id: "chan1".into(),
            message: "Hello".into(),
            root_id: Some("root1".into()),
            file_ids: vec![],
        };
        let json = serde_json::to_value(&req).unwrap();
        assert_eq!(json["channel_id"], "chan1");
        assert_eq!(json["message"], "Hello");
        assert_eq!(json["root_id"], "root1");
    }

    #[test]
    fn create_post_request_no_root_id_omits_field() {
        let req = CreatePostRequest {
            channel_id: "chan1".into(),
            message: "Hello".into(),
            root_id: None,
            file_ids: vec![],
        };
        let json = serde_json::to_value(&req).unwrap();
        assert!(json.get("root_id").is_none());
    }

    #[test]
    fn create_post_request_with_files_serializes() {
        let req = CreatePostRequest {
            channel_id: "chan1".into(),
            message: String::new(),
            root_id: None,
            file_ids: vec!["file1".into(), "file2".into()],
        };
        let json = serde_json::to_value(&req).unwrap();
        assert_eq!(json["file_ids"], serde_json::json!(["file1", "file2"]));
    }

    #[test]
    fn create_post_request_empty_files_omits_field() {
        let req = CreatePostRequest {
            channel_id: "chan1".into(),
            message: "Hi".into(),
            root_id: None,
            file_ids: vec![],
        };
        let json = serde_json::to_value(&req).unwrap();
        assert!(json.get("file_ids").is_none());
    }

    #[test]
    fn file_upload_response_deserializes() {
        let raw = json!({"file_infos": [{"id": "f1"}, {"id": "f2"}]});
        let resp: FileUploadResponse = serde_json::from_value(raw).unwrap();
        let ids: Vec<_> = resp.file_infos.into_iter().map(|f| f.id).collect();
        assert_eq!(ids, vec!["f1", "f2"]);
    }

    #[test]
    fn create_post_response_deserializes() {
        let raw = json!({"id": "post123"});
        let resp: CreatePostResponse = serde_json::from_value(raw).unwrap();
        assert_eq!(resp.id, "post123");
    }

    #[test]
    fn update_post_request_serializes() {
        let req = UpdatePostRequest {
            id: "post1".into(),
            message: "Updated".into(),
        };
        let json = serde_json::to_value(&req).unwrap();
        assert_eq!(json["id"], "post1");
        assert_eq!(json["message"], "Updated");
    }

    #[test]
    fn ws_auth_challenge_serializes() {
        let auth = WsAuthChallenge::new("my-token");
        let json = serde_json::to_value(&auth).unwrap();
        assert_eq!(json["seq"], 1);
        assert_eq!(json["action"], "authentication_challenge");
        assert_eq!(json["data"]["token"], "my-token");
    }

    #[test]
    fn ws_event_posted_deserializes() {
        let post_json = serde_json::to_string(&json!({
            "id": "post1",
            "channel_id": "chan1",
            "user_id": "user1",
            "message": "Hello world",
            "root_id": "",
            "file_ids": []
        }))
        .unwrap();

        let raw = json!({
            "event": "posted",
            "data": {
                "post": post_json,
                "channel_type": "D",
                "sender_name": "@alice",
                "mentions": "[\"user1\"]"
            }
        });
        let evt: WsEvent = serde_json::from_value(raw).unwrap();
        assert_eq!(evt.event.as_deref(), Some("posted"));
        let data = evt.data.unwrap();
        assert_eq!(data.channel_type.as_deref(), Some("D"));
        assert_eq!(data.sender_name.as_deref(), Some("@alice"));

        // Double-deserialize the post string
        let post: MmPost = serde_json::from_str(data.post.as_ref().unwrap()).unwrap();
        assert_eq!(post.id, "post1");
        assert_eq!(post.channel_id, "chan1");
        assert_eq!(post.user_id, "user1");
        assert_eq!(post.message, "Hello world");
        assert!(post.root_id.is_empty());
        assert!(post.file_ids.is_empty());
    }

    #[test]
    fn ws_event_non_posted_deserializes() {
        let raw = json!({"event": "typing", "data": {}});
        let evt: WsEvent = serde_json::from_value(raw).unwrap();
        assert_eq!(evt.event.as_deref(), Some("typing"));
    }

    #[test]
    fn ws_event_no_event_field() {
        // Auth response frames have no "event" field
        let raw = json!({"status": "OK", "seq_reply": 1});
        let evt: WsEvent = serde_json::from_value(raw).unwrap();
        assert!(evt.event.is_none());
    }

    #[test]
    fn mm_post_with_file_ids() {
        let raw = json!({
            "id": "p1",
            "channel_id": "c1",
            "user_id": "u1",
            "message": "",
            "root_id": "root1",
            "file_ids": ["f1", "f2"]
        });
        let post: MmPost = serde_json::from_value(raw).unwrap();
        assert_eq!(post.root_id, "root1");
        assert_eq!(post.file_ids, vec!["f1", "f2"]);
    }

    #[test]
    fn mm_post_defaults() {
        // Minimal post — missing optional fields should default
        let raw = json!({
            "id": "p1",
            "channel_id": "c1",
            "user_id": "u1"
        });
        let post: MmPost = serde_json::from_value(raw).unwrap();
        assert!(post.message.is_empty());
        assert!(post.root_id.is_empty());
        assert!(post.file_ids.is_empty());
    }

    #[test]
    fn mentions_json_string_parses_as_vec() {
        let mentions_str = "[\"user1\",\"user2\"]";
        let parsed: Vec<String> = serde_json::from_str(mentions_str).unwrap();
        assert_eq!(parsed, vec!["user1", "user2"]);
    }
}

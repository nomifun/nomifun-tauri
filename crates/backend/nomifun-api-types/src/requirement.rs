use nomifun_common::TimestampMs;
use serde::{Deserialize, Serialize};

/// Requirement status, serialized as a lowercase string matching the DB column.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RequirementStatus {
    Pending,
    InProgress,
    Done,
    Failed,
    Cancelled,
    /// The turn ended cleanly but the agent did NOT explicitly declare the
    /// requirement done or failed (via its completion tool / terminal marker).
    /// Rather than silently assuming success, the platform parks it here for a
    /// human to verify. Not claimable; not frozen (a human can move it on).
    NeedsReview,
}

impl RequirementStatus {
    /// DB string form (matches serde `snake_case`).
    pub fn as_db(&self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::InProgress => "in_progress",
            Self::Done => "done",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
            Self::NeedsReview => "needs_review",
        }
    }

    /// Parse from a DB string; unknown values map to `Pending`.
    pub fn from_db(s: &str) -> Self {
        match s {
            "in_progress" => Self::InProgress,
            "done" => Self::Done,
            "failed" => Self::Failed,
            "cancelled" => Self::Cancelled,
            "needs_review" => Self::NeedsReview,
            _ => Self::Pending,
        }
    }
}

/// One attachment on a requirement (API view of an `attachments` row).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AttachmentDto {
    /// Stable attachment business identity (bare UUIDv7).
    #[serde(deserialize_with = "crate::serde_util::deserialize_attachment_id")]
    pub attachment_id: String,
    pub file_name: String,
    pub mime: String,
    pub size_bytes: i64,
    pub created_at: TimestampMs,
    /// Absolute path resolved at read time (`data_dir` + `rel_path`) so the
    /// desktop frontend can render it via the image-base64 endpoint. Never
    /// persisted — the DB stores only the relative path.
    pub abs_path: String,
}

/// A freshly-uploaded file to bind as an attachment. `source_path` is the
/// absolute path returned by `POST /api/fs/upload` and MUST sit inside the
/// temp upload root (`<OS temp>/nomifun/`) — the service rejects anything else.
#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct NewAttachmentRef {
    pub source_path: String,
    pub file_name: String,
}

/// Requirement response object (the API view of a `RequirementRow`).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Requirement {
    /// Stable requirement business identity (canonical bare UUIDv7).
    #[serde(deserialize_with = "crate::serde_util::deserialize_requirement_id")]
    pub requirement_id: String,
    /// Compact, immutable identifier shown to people as `#N`.
    pub display_no: i64,
    pub title: String,
    pub content: String,
    pub tag: String,
    pub order_key: String,
    pub status: RequirementStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub completion_note: Option<String>,
    /// Executing session id: a conversation OR a terminal, discriminated by
    /// `owner_kind`. Replaces the former `conversation_id` (which assumed a
    /// single conversation domain).
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "crate::serde_util::deserialize_optional_conversation_id"
    )]
    pub owner_conversation_id: Option<String>,
    /// `'conversation'` | `'terminal'` | None (when unowned). Paired with
    /// `owner_session_id` — both null together, both set together.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "crate::serde_util::deserialize_optional_terminal_id"
    )]
    pub owner_terminal_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started_at: Option<TimestampMs>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub completed_at: Option<TimestampMs>,
    pub attempt_count: i64,
    pub created_by: String,
    pub created_at: TimestampMs,
    pub updated_at: TimestampMs,
    /// Image attachments. Populated on get/create/update responses; list/board
    /// rows and claim/status events carry an empty list for leanness.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub attachments: Vec<AttachmentDto>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CreateRequirementRequest {
    pub title: String,
    #[serde(default)]
    pub content: String,
    pub tag: String,
    #[serde(default)]
    pub order_key: Option<String>,
    #[serde(default)]
    pub status: Option<RequirementStatus>,
    /// 'user' (default) | 'agent'.
    #[serde(default)]
    pub created_by: Option<String>,
    /// Images to bind on create (uploaded beforehand via `POST /api/fs/upload`).
    #[serde(default)]
    pub attachments: Vec<NewAttachmentRef>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UpdateRequirementRequest {
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub content: Option<String>,
    #[serde(default)]
    pub tag: Option<String>,
    #[serde(default)]
    pub order_key: Option<String>,
    #[serde(default)]
    pub status: Option<RequirementStatus>,
    #[serde(default)]
    pub completion_note: Option<String>,
    /// Images to add (uploaded beforehand via `POST /api/fs/upload`).
    #[serde(default)]
    pub add_attachments: Vec<NewAttachmentRef>,
    /// Existing attachment ids to remove (rows + files).
    #[serde(default, deserialize_with = "crate::serde_util::deserialize_attachment_ids")]
    pub remove_attachment_ids: Vec<String>,
}

/// Bulk delete by requirement id (used by the list page's batch-delete action).
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BatchDeleteRequest {
    #[serde(deserialize_with = "crate::serde_util::deserialize_requirement_ids")]
    pub requirement_ids: Vec<String>,
}

/// Result of a bulk delete.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct BatchDeleteResponse {
    pub deleted: u64,
}

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct ListRequirementsQuery {
    #[serde(default)]
    pub tag: Option<String>,
    #[serde(default)]
    pub status: Option<RequirementStatus>,
    #[serde(
        default,
        deserialize_with = "crate::serde_util::deserialize_optional_conversation_id"
    )]
    pub conversation_id: Option<String>,
    #[serde(default)]
    pub q: Option<String>,
    /// Sort column. Whitelisted server-side to `display_no | requirement_id | created_at |
    /// updated_at | status`; any other value falls back to the default queue order.
    #[serde(default)]
    pub order_by: Option<String>,
    /// Sort direction: `asc | desc` (default `desc` for an explicit `order_by`).
    #[serde(default)]
    pub order: Option<String>,
    #[serde(default)]
    pub page: Option<u32>,
    #[serde(default)]
    pub page_size: Option<u32>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UpdateStatusRequest {
    pub status: RequirementStatus,
    #[serde(default)]
    pub note: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CompleteRequest {
    #[serde(default)]
    pub completion_note: Option<String>,
}

/// Per-status counts for a single tag.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct TagSummary {
    pub tag: String,
    pub pending: i64,
    pub in_progress: i64,
    pub done: i64,
    pub failed: i64,
    pub cancelled: i64,
    #[serde(default)]
    pub needs_review: i64,
    pub total: i64,
    /// AutoWork is paused for this tag (a requirement exhausted its retries).
    /// While true, the AutoWork runner does not claim the tag's requirements until
    /// it is resumed. `#[serde(default)]` keeps older payloads parseable.
    #[serde(default)]
    pub paused: bool,
    /// Why the tag was paused (`requirement_failed` | `manual` | …), if paused.
    #[serde(default)]
    pub paused_reason: Option<String>,
}

/// Request body for `POST /api/requirements/tags/{tag}/resume`. Body is
/// optional; an empty body resumes without re-queuing anything.
#[derive(Debug, Clone, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct ResumeTagRequest {
    /// Re-queue ALL failed requirements in the tag back to pending.
    #[serde(default)]
    pub requeue_failed: bool,
    /// Re-queue these specific failed requirement ids back to pending.
    #[serde(default, deserialize_with = "crate::serde_util::deserialize_requirement_ids")]
    pub requeue_requirement_ids: Vec<String>,
}

/// Broadcast (`autowork.tagPaused`) when AutoWork pauses a tag after a
/// requirement exhausts its retries, so the UI can surface "needs attention".
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct TagPausedPayload {
    pub tag: String,
    pub reason: String,
    #[serde(
        default,
        deserialize_with = "crate::serde_util::deserialize_optional_requirement_id"
    )]
    pub requirement_id: Option<String>,
}

/// Kanban board for a tag: requirements grouped by status.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct BoardResponse {
    pub tag: String,
    pub pending: Vec<Requirement>,
    pub in_progress: Vec<Requirement>,
    pub done: Vec<Requirement>,
    pub failed: Vec<Requirement>,
    pub cancelled: Vec<Requirement>,
    #[serde(default)]
    pub needs_review: Vec<Requirement>,
}

/// What an AutoWork loop drives: a chat conversation's agent, or a terminal
/// session's PTY (running a vendor CLI). `target_id` is the canonical bare
/// UUIDv7 for the selected entity. The explicit `kind` is part of the key:
/// equal UUID text in different domains must remain distinct without relying
/// on a textual prefix.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum AutoWorkTargetKind {
    #[default]
    Conversation,
    Terminal,
}

impl AutoWorkTargetKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Conversation => "conversation",
            Self::Terminal => "terminal",
        }
    }

    /// Parse a path/segment value; `None` for anything unrecognized.
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "conversation" => Some(Self::Conversation),
            "terminal" => Some(Self::Terminal),
            _ => None,
        }
    }
}

/// The display status of an AutoWork switch — drives the UI dot/colour.
/// `off` (not enabled) / `idle` (enabled, between or awaiting work) /
/// `active` (enabled, a requirement is in flight).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AutoWorkRunState {
    Off,
    Idle,
    Active,
}

fn default_conversation_kind() -> AutoWorkTargetKind {
    AutoWorkTargetKind::Conversation
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AutoWorkConfigRequest {
    /// Defaults to `conversation` when the caller omits the discriminator.
    #[serde(default = "default_conversation_kind")]
    pub kind: AutoWorkTargetKind,
    /// Canonical conversation or terminal UUIDv7. `kind` is explicit and is
    /// never inferred from an ID spelling.
    #[serde(deserialize_with = "crate::serde_util::deserialize_session_target_id")]
    pub target_id: String,
    pub enabled: bool,
    #[serde(default)]
    pub tag: Option<String>,
    #[serde(default)]
    pub max_requirements: Option<u32>,
    /// Set by the AutoWork admin (标签会话管理). When `true`, a disable request
    /// targeting an actively-executing session is rejected — the user must stop
    /// it from the session page so a live turn is not interrupted. Session-page
    /// toggles leave this unset (`false`) and may always disable.
    #[serde(default)]
    pub from_admin: bool,
}

/// Persisted-and-live AutoWork state for a session (conversation or terminal).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AutoWorkState {
    pub kind: AutoWorkTargetKind,
    #[serde(deserialize_with = "crate::serde_util::deserialize_session_target_id")]
    pub target_id: String,
    pub enabled: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tag: Option<String>,
    pub running: bool,
    pub run_state: AutoWorkRunState,
    #[serde(
        default,
        deserialize_with = "crate::serde_util::deserialize_optional_requirement_id"
    )]
    pub current_requirement_id: Option<String>,
    pub completed_count: u32,
}

impl AutoWorkState {
    /// Compute the tri-state display status from the persisted/live flags.
    pub fn run_state(enabled: bool, current_requirement_id: Option<&str>) -> AutoWorkRunState {
        if !enabled {
            AutoWorkRunState::Off
        } else if current_requirement_id.is_some() {
            AutoWorkRunState::Active
        } else {
            AutoWorkRunState::Idle
        }
    }
}

/// WS payload for `requirement.deleted`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RequirementDeletedPayload {
    #[serde(deserialize_with = "crate::serde_util::deserialize_requirement_id")]
    pub requirement_id: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn attachment_wire_uses_only_the_business_id() {
        let id = "0190f5fe-7c00-7a00-8000-000000000007";
        let attachment = AttachmentDto {
            attachment_id: id.to_string(),
            file_name: "image.png".into(),
            mime: "image/png".into(),
            size_bytes: 7,
            created_at: 1,
            abs_path: "/tmp/image.png".into(),
        };
        let wire = serde_json::to_value(&attachment).expect("attachment must serialize");
        assert_eq!(wire["attachment_id"], id);
        assert!(wire.get("id").is_none());
        assert!(wire["attachment_id"].is_string());

        let parsed: AttachmentDto =
            serde_json::from_value(wire).expect("canonical business id must deserialize");
        assert_eq!(parsed.attachment_id, id);
        assert!(
            serde_json::from_value::<AttachmentDto>(serde_json::json!({
                "attachment_id": 7,
                "file_name": "image.png",
                "mime": "image/png",
                "size_bytes": 7,
                "created_at": 1,
                "abs_path": "/tmp/image.png"
            }))
            .is_err(),
            "technical SQLite row ids must not be accepted on the attachment wire"
        );
        assert!(
            serde_json::from_value::<AttachmentDto>(serde_json::json!({
                "id": id,
                "file_name": "image.png",
                "mime": "image/png",
                "size_bytes": 7,
                "created_at": 1,
                "abs_path": "/tmp/image.png"
            }))
            .is_err(),
            "the removed generic id field must not deserialize"
        );
    }

    #[test]
    fn requirement_request_ids_are_canonical_business_ids() {
        let id = "0190f5fe-7c00-7a00-8000-000000000007";
        let request: BatchDeleteRequest =
            serde_json::from_str(&format!(r#"{{"requirement_ids":["{id}"]}}"#))
                .expect("canonical UUIDv7 requirement ids are valid");
        assert_eq!(request.requirement_ids, vec![id]);
        assert!(serde_json::from_str::<BatchDeleteRequest>(r#"{"requirement_ids":[7]}"#).is_err());
        assert!(serde_json::from_str::<BatchDeleteRequest>(r#"{"requirement_ids":["7"]}"#).is_err());
        assert!(
            serde_json::from_str::<BatchDeleteRequest>(r#"{"requirement_ids":["req_7"]}"#).is_err()
        );
        assert!(serde_json::from_str::<BatchDeleteRequest>(r#"{"ids":[
            "0190f5fe-7c00-7a00-8000-000000000007"
        ]}"#).is_err());
        assert!(
            serde_json::from_str::<UpdateRequirementRequest>(
                r#"{"remove_attachment_ids":["att_7"]}"#
            )
            .is_err()
        );
        assert!(
            serde_json::from_str::<ResumeTagRequest>(r#"{"requeue_requirement_ids":["req_7"]}"#)
                .is_err()
        );
        assert!(
            serde_json::from_str::<ResumeTagRequest>(r#"{"requeue_ids":[
                "0190f5fe-7c00-7a00-8000-000000000007"
            ]}"#)
                .is_err()
        );
    }

    #[test]
    fn requirement_wire_serializes_named_id_and_rejects_legacy_id() {
        let requirement = Requirement {
            requirement_id: "0190f5fe-7c00-7a00-8000-000000000007".into(),
            display_no: 7,
            title: "Title".into(),
            content: "Content".into(),
            tag: "tag".into(),
            order_key: "1".into(),
            status: RequirementStatus::Pending,
            completion_note: None,
            owner_conversation_id: None,
            owner_terminal_id: None,
            started_at: None,
            completed_at: None,
            attempt_count: 0,
            created_by: "user".into(),
            created_at: 0,
            updated_at: 0,
            attachments: vec![],
        };
        let wire = serde_json::to_value(&requirement).expect("requirement must serialize");
        assert_eq!(
            wire["requirement_id"],
            "0190f5fe-7c00-7a00-8000-000000000007"
        );
        assert!(wire.get("id").is_none());
        assert!(
            serde_json::from_value::<Requirement>(serde_json::json!({
                "id": "0190f5fe-7c00-7a00-8000-000000000007",
                "display_no": 7,
                "title": "Title",
                "content": "Content",
                "tag": "tag",
                "order_key": "1",
                "status": "pending",
                "attempt_count": 0,
                "created_by": "user",
                "created_at": 0,
                "updated_at": 0
            }))
            .is_err()
        );
    }

    #[test]
    fn conversation_filters_reject_noncanonical_ids() {
        assert!(
            serde_json::from_str::<ListRequirementsQuery>(r#"{"conversation_id":"7"}"#)
                .is_err()
        );
    }

    #[test]
    fn autowork_request_rejects_numeric_target_id() {
        let body = r#"{"kind":"conversation","target_id":12345,"enabled":true,"tag":"t"}"#;
        assert!(serde_json::from_str::<AutoWorkConfigRequest>(body).is_err());
    }

    #[test]
    fn autowork_request_accepts_string_target_id() {
        let body = r#"{"kind":"terminal","target_id":"0190f5fe-7c00-7a00-8000-000000000007","enabled":false}"#;
        let req: AutoWorkConfigRequest = serde_json::from_str(body).expect("string target_id must deserialize");
        assert_eq!(
            req.target_id,
            "0190f5fe-7c00-7a00-8000-000000000007"
        );
        assert_eq!(req.kind, AutoWorkTargetKind::Terminal);
        assert!(!req.enabled);
    }

    #[test]
    fn autowork_request_rejects_non_canonical_entity_string() {
        let body = r#"{"kind":"terminal","target_id":"term_7","enabled":false}"#;
        assert!(serde_json::from_str::<AutoWorkConfigRequest>(body).is_err());
    }

    #[test]
    fn autowork_request_defaults_kind_with_string_target_id() {
        let body = r#"{"target_id":"0190f5fe-7c00-7a00-8000-000000000042","enabled":true}"#;
        let req: AutoWorkConfigRequest = serde_json::from_str(body).expect("must deserialize");
        assert_eq!(req.target_id, "0190f5fe-7c00-7a00-8000-000000000042");
        assert_eq!(req.kind, AutoWorkTargetKind::Conversation);
    }
}

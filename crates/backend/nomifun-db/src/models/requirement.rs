use nomifun_common::TimestampMs;
use serde::{Deserialize, Serialize};
use std::fmt;

/// Row in the `requirements` table.
#[derive(Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct RequirementRow {
    pub id: i64,
    /// Stable business identity (canonical bare UUIDv7).
    pub requirement_id: String,
    /// Compact, immutable identifier shown to people as `#N`.
    pub display_no: i64,
    pub title: String,
    pub content: String,
    pub tag: String,
    pub order_key: String,
    pub sort_seq: String,
    pub status: String,
    pub priority: i64,
    pub completion_note: Option<String>,
    /// Executing session: a conversation id OR a terminal id, discriminated by
    /// `owner_kind`. No FK (dual-domain). Replaces the former `conversation_id`
    /// + redundant `claimed_by` columns.
    pub owner_conversation_id: Option<String>,
    /// `'conversation'` | `'terminal'` | NULL (when unowned).
    pub owner_terminal_id: Option<String>,
    pub active_turn_started_at: Option<TimestampMs>,
    pub lease_expires_at: Option<TimestampMs>,
    pub started_at: Option<TimestampMs>,
    pub completed_at: Option<TimestampMs>,
    pub attempt_count: i64,
    /// Monotonic durable identity of a claim generation. Unlike the retry
    /// budget, this value is never decremented or reset.
    pub claim_generation: i64,
    /// Opaque 256-bit capability for the current active claim. Internal only;
    /// never map this field into the public Requirement DTO.
    #[serde(skip_serializing, default)]
    pub claim_token: Option<String>,
    pub created_by: String,
    /// JSON object, forward-compat.
    pub extra: String,
    pub created_at: TimestampMs,
    pub updated_at: TimestampMs,
}

impl fmt::Debug for RequirementRow {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("RequirementRow")
            .field("id", &self.id)
            .field("requirement_id", &self.requirement_id)
            .field("display_no", &self.display_no)
            .field("title", &self.title)
            .field("content", &self.content)
            .field("tag", &self.tag)
            .field("order_key", &self.order_key)
            .field("sort_seq", &self.sort_seq)
            .field("status", &self.status)
            .field("priority", &self.priority)
            .field("completion_note", &self.completion_note)
            .field("owner_conversation_id", &self.owner_conversation_id)
            .field("owner_terminal_id", &self.owner_terminal_id)
            .field("active_turn_started_at", &self.active_turn_started_at)
            .field("lease_expires_at", &self.lease_expires_at)
            .field("started_at", &self.started_at)
            .field("completed_at", &self.completed_at)
            .field("attempt_count", &self.attempt_count)
            .field("claim_generation", &self.claim_generation)
            .field(
                "claim_token",
                &self.claim_token.as_ref().map(|_| "<redacted>"),
            )
            .field("created_by", &self.created_by)
            .field("extra", &self.extra)
            .field("created_at", &self.created_at)
            .field("updated_at", &self.updated_at)
            .finish()
    }
}

/// A requirement before its human-facing display number is allocated.
///
/// The SQLite repository allocates `display_no` from a durable singleton
/// sequence in the same transaction that inserts this row, then returns the
/// fully persisted [`RequirementRow`].
#[derive(Debug, Clone)]
pub struct NewRequirementRow {
    pub title: String,
    pub content: String,
    pub tag: String,
    pub order_key: String,
    pub sort_seq: String,
    pub status: String,
    pub priority: i64,
    pub completion_note: Option<String>,
    pub owner_conversation_id: Option<String>,
    pub owner_terminal_id: Option<String>,
    pub active_turn_started_at: Option<TimestampMs>,
    pub lease_expires_at: Option<TimestampMs>,
    pub started_at: Option<TimestampMs>,
    pub completed_at: Option<TimestampMs>,
    pub attempt_count: i64,
    pub created_by: String,
    pub extra: String,
    pub created_at: TimestampMs,
    pub updated_at: TimestampMs,
}

/// Partial update for a requirement row.
///
/// All fields are optional; `None` means "keep the current value".
/// Nullable columns use `Option<Option<T>>`: outer = "change?", inner = "set value or NULL".
#[derive(Debug, Clone, Default)]
pub struct RequirementRowUpdate {
    pub title: Option<String>,
    pub content: Option<String>,
    pub tag: Option<String>,
    pub order_key: Option<String>,
    pub sort_seq: Option<String>,
    pub status: Option<String>,
    pub priority: Option<i64>,
    pub completion_note: Option<Option<String>>,
    pub owner_conversation_id: Option<Option<String>>,
    pub owner_terminal_id: Option<Option<String>>,
    pub active_turn_started_at: Option<Option<TimestampMs>>,
    pub lease_expires_at: Option<Option<TimestampMs>>,
    pub started_at: Option<Option<TimestampMs>>,
    pub completed_at: Option<Option<TimestampMs>>,
    pub attempt_count: Option<i64>,
    pub extra: Option<String>,
}

/// Row in the `requirement_tags` table: AutoWork tag-level pause state.
/// A tag with no row is treated as not paused.
#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct RequirementTagRow {
    pub id: i64,
    pub tag: String,
    /// 0 = active, 1 = paused (SQLite has no bool; stored as INTEGER).
    pub paused: i64,
    pub paused_reason: Option<String>,
    pub paused_requirement_id: Option<String>,
    pub paused_at: Option<TimestampMs>,
}

impl RequirementTagRow {
    pub fn is_paused(&self) -> bool {
        self.paused != 0
    }
}

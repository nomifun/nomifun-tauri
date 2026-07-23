use nomifun_common::{MessageId, PaginatedResult, TimestampMs};
use serde::{Deserialize, Serialize};

use crate::error::DbError;
use crate::models::{
    ConversationArtifactRow, ConversationDeliveryReceiptRow, ConversationRow, MessageRow,
};

/// Result of atomically projecting a trusted assistant message into a
/// Conversation under a stable receiver-side operation identity.
#[derive(Debug, Clone)]
pub struct ConversationMessageProjection {
    /// `true` only for the transaction which inserted the durable message.
    pub inserted: bool,
    /// The canonical persisted row. Replays return the original row rather
    /// than trusting a newly constructed candidate message.
    pub message: MessageRow,
}

/// One terminal artifact-producing tool message to publish when its enclosing
/// turn has finished successfully.
///
/// Conversation and turn ownership, terminal status, position, visibility and
/// creation time are deliberately supplied by the repository boundary rather
/// than repeated by every item. This keeps the commit surface narrow enough to
/// reject cross-turn or non-tool projections as one atomic batch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TurnArtifactMessageCommit {
    /// Canonical durable message identity claimed for the provider tool call.
    pub message_id: String,
    /// Exactly `tool_call` or `acp_tool_call`.
    pub message_type: String,
    /// Final JSON object containing a committed, completed artifact delivery.
    pub content: String,
}

/// Conversation + message data access abstraction.
///
/// Covers conversation CRUD, extended queries (source/chat, cron-job,
/// associated workspace), and message operations (list, insert, update,
/// delete, search).
///
/// Object-safe via `async_trait` to support `Arc<dyn IConversationRepository>`.
#[async_trait::async_trait]
pub trait IConversationRepository: Send + Sync {
    // ── Conversation CRUD ───────────────────────────────────────────

    /// Returns a conversation by ID, or `None` if not found.
    async fn get(&self, conversation_id: &str) -> Result<Option<ConversationRow>, DbError>;

    /// Inserts a new conversation row using its caller-minted global ID.
    async fn create(&self, row: &ConversationRow) -> Result<String, DbError>;

    /// Trusted internal creation boundary with a stable operation key.  The
    /// public Conversation API never supplies this value.  Returns
    /// `(conversation_id, created_now)` so callers do not repeat post-create
    /// materialization when recovering an already committed operation.
    async fn create_idempotent(
        &self,
        row: &ConversationRow,
        _creation_key: &str,
    ) -> Result<(String, bool), DbError> {
        let id = self.create(row).await?;
        Ok((id, true))
    }

    /// Resolve a trusted internal creation identity. Public conversation reads
    /// continue to address rows by their stable `conversation_id`.
    async fn find_by_creation_key(
        &self,
        _user_id: &str,
        _creation_key: &str,
    ) -> Result<Option<ConversationRow>, DbError> {
        Ok(None)
    }

    /// Atomically register or load a receiver-side idempotency receipt.
    async fn claim_delivery_receipt(
        &self,
        _user_id: &str,
        _conversation_id: &str,
        _operation_id: &str,
        _kind: &str,
        _request_payload: &str,
        _now: i64,
    ) -> Result<ConversationDeliveryReceiptRow, DbError> {
        Err(DbError::Init(
            "conversation delivery receipts are not supported".to_owned(),
        ))
    }

    async fn get_delivery_receipt(
        &self,
        _user_id: &str,
        _conversation_id: &str,
        _operation_id: &str,
    ) -> Result<Option<ConversationDeliveryReceiptRow>, DbError> {
        Ok(None)
    }

    async fn complete_delivery_receipt(
        &self,
        _user_id: &str,
        _conversation_id: &str,
        _operation_id: &str,
        _result_ok: bool,
        _result_text: Option<&str>,
        _result_error: Option<&str>,
        _completed_at: i64,
    ) -> Result<bool, DbError> {
        Ok(false)
    }

    /// Atomically inserts one trusted assistant message and completes its
    /// idempotency receipt. A replay with the same owner, Conversation, kind,
    /// and request payload returns the original persisted message with
    /// `inserted = false`; reusing the operation identity for any other input
    /// is a conflict.
    async fn project_assistant_message_with_receipt(
        &self,
        _user_id: &str,
        _conversation_id: &str,
        _operation_id: &str,
        _kind: &str,
        _request_payload: &str,
        _message: &MessageRow,
        _now: i64,
    ) -> Result<ConversationMessageProjection, DbError> {
        Err(DbError::Init(
            "atomic Conversation message projection is not supported".to_owned(),
        ))
    }

    /// Partially updates a conversation. Returns `DbError::NotFound` if ID is missing.
    async fn update(
        &self,
        conversation_id: &str,
        updates: &ConversationRowUpdate,
    ) -> Result<(), DbError>;

    /// Atomically replaces the persisted IDMM configuration inside
    /// `conversation.extra`. Implementations must validate and logically lock
    /// every per-watch `bypass_model.provider_id` before writing the JSON.
    async fn update_idmm(
        &self,
        _conversation_id: &str,
        _idmm: Option<&str>,
    ) -> Result<(), DbError> {
        Err(DbError::Init(
            "conversation IDMM persistence is not supported".to_owned(),
        ))
    }

    /// Deletes a conversation and explicitly removes repository-owned
    /// dependent rows.
    /// Returns `DbError::NotFound` if ID is missing.
    async fn delete(&self, conversation_id: &str) -> Result<(), DbError>;

    /// Deletes a Conversation aggregate and returns identities needed for
    /// non-database cleanup after the transaction commits.
    ///
    /// The default preserves compatibility for repositories whose aggregate
    /// has no process-local dependents. SQLite overrides this to capture and
    /// delete linked Cron jobs and runs atomically with the Conversation.
    async fn delete_with_cleanup(
        &self,
        conversation_id: &str,
    ) -> Result<Vec<String>, DbError> {
        self.delete(conversation_id).await?;
        Ok(Vec::new())
    }

    /// Lists conversations with cursor-based pagination and optional filters.
    async fn list_paginated(
        &self,
        user_id: &str,
        filters: &ConversationFilters,
    ) -> Result<PaginatedResult<ConversationRow>, DbError>;

    // ── Extended queries ────────────────────────────────────────────

    /// Finds a conversation by source, channel chat ID, and agent type.
    async fn find_by_source_and_chat(
        &self,
        user_id: &str,
        source: &str,
        chat_id: &str,
        agent_type: &str,
    ) -> Result<Option<ConversationRow>, DbError>;

    /// Lists conversations created by the given cron job (`cron_job_id` column).
    async fn list_by_cron_job(
        &self,
        user_id: &str,
        cron_job_id: &str,
    ) -> Result<Vec<ConversationRow>, DbError>;

    /// Lists conversations sharing the same `extra.workspace` value.
    /// The conversation identified by `conversation_id` is excluded.
    async fn list_associated(
        &self,
        user_id: &str,
        conversation_id: &str,
    ) -> Result<Vec<ConversationRow>, DbError>;

    /// Lists every retained Conversation whose top-level current model is
    /// logically bound to `provider_id`. Deletion policy is enforced by the
    /// service layer.
    async fn list_conversations_using_model_provider(
        &self,
        _provider_id: &str,
    ) -> Result<Vec<(String, String)>, DbError> {
        Err(DbError::Init(
            "conversation model-provider usage scan is not supported".to_owned(),
        ))
    }

    // ── conversation_mcp_servers junction ───────────────────────────

    /// Returns the MCP server IDs selected for a conversation, ordered by
    /// `sort_order`. The v3 junction table is the only durable source of truth.
    async fn list_mcp_server_ids(&self, _conversation_id: &str) -> Result<Vec<String>, DbError> {
        Ok(Vec::new())
    }

    /// Replaces the conversation's selected MCP server set with `ids`, preserving
    /// order via `sort_order`. Implemented as a single DELETE + ordered INSERT
    /// transaction; open-ended `extra` data is never used as durable selection
    /// storage.
    async fn set_mcp_server_ids(
        &self,
        _conversation_id: &str,
        _mcp_server_ids: &[String],
    ) -> Result<(), DbError> {
        Ok(())
    }

    // ── Message operations ──────────────────────────────────────────

    /// Returns paginated messages for a conversation, ordered by `created_at`.
    async fn get_messages(
        &self,
        conversation_id: &str,
        page: u32,
        page_size: u32,
        order: SortOrder,
    ) -> Result<PaginatedResult<MessageRow>, DbError>;

    /// Keyset (cursor) pagination: returns up to `limit` messages strictly OLDER
    /// than `before` `(created_at, message_id)`, newest-first
    /// (`created_at DESC, message_id DESC`);
    /// `before: None` returns the newest `limit`. `has_more` means an older page
    /// exists. Used to incrementally load an ever-growing conversation (e.g. a
    /// companion's single session) without fetching the whole transcript, and is
    /// stable under concurrent appends (unlike OFFSET). `total` is not computed
    /// (returned as 0). Default returns empty so mock repos compile; the SQLite
    /// repo overrides it.
    async fn get_messages_keyset(
        &self,
        _conversation_id: &str,
        _before: Option<(i64, String)>,
        _limit: u32,
    ) -> Result<PaginatedResult<MessageRow>, DbError> {
        Ok(PaginatedResult {
            items: Vec::new(),
            total: 0,
            has_more: false,
        })
    }

    /// Returns a single message scoped to a conversation.
    async fn get_message(
        &self,
        _conversation_id: &str,
        _message_id: &str,
    ) -> Result<Option<MessageRow>, DbError> {
        Ok(None)
    }

    /// Inserts a new message row.
    async fn insert_message(&self, message: &MessageRow) -> Result<(), DbError>;

    /// Atomically commits every artifact-producing tool message from one
    /// successfully completed turn.
    ///
    /// Implementations must insert missing rows, promote only matching `work`
    /// rows to `finish`, accept an exactly identical finished replay, and roll
    /// back the entire batch on any identity, lifecycle or content conflict.
    /// The default is intentionally unsupported: a repository without a real
    /// transaction must never approximate this with sequential updates.
    async fn commit_turn_artifact_messages(
        &self,
        _conversation_id: &str,
        _turn_message_id: &str,
        _messages: &[TurnArtifactMessageCommit],
        _committed_at: TimestampMs,
    ) -> Result<Vec<MessageRow>, DbError> {
        Err(DbError::Init(
            "atomic turn artifact message commits are not supported".to_owned(),
        ))
    }

    /// Atomically resolves a protocol correlation key to one durable canonical
    /// message ID. Correlation keys are scoped by Conversation, wire prompt
    /// owner token, and message type; neither the key nor the owner token is a
    /// parent-row identity.
    async fn claim_message_correlation(
        &self,
        _conversation_id: &str,
        _turn_message_id: &str,
        _message_type: &str,
        _correlation_key: &str,
    ) -> Result<String, DbError> {
        Ok(MessageId::new().into_string())
    }

    /// Partially updates a message. Returns `DbError::NotFound` if ID is missing.
    async fn update_message(
        &self,
        message_id: &str,
        updates: &MessageRowUpdate,
    ) -> Result<(), DbError>;

    /// Deletes all messages belonging to a conversation.
    async fn delete_messages_by_conversation(
        &self,
        conversation_id: &str,
    ) -> Result<(), DbError>;

    /// Deletes the message at the `(created_at, message_id)` keyset cursor
    /// (inclusive) and every newer message in the conversation. Returns the
    /// number of rows deleted. Default no-op so mock repos compile; SQLite
    /// overrides it.
    async fn delete_messages_from(
        &self,
        _conversation_id: &str,
        _from_created_at: i64,
        _from_message_id: &str,
    ) -> Result<u64, DbError> {
        Ok(0)
    }

    /// Finds a message by (conversation_id, msg_id, type) triple.
    async fn get_message_by_msg_id(
        &self,
        conversation_id: &str,
        msg_id: &str,
        msg_type: &str,
    ) -> Result<Option<MessageRow>, DbError>;

    /// Full-text search across messages, joining conversation name.
    async fn search_messages(
        &self,
        user_id: &str,
        keyword: &str,
        page: u32,
        page_size: u32,
    ) -> Result<PaginatedResult<MessageSearchRow>, DbError>;

    /// Returns persisted conversation artifacts ordered by `created_at`.
    async fn list_artifacts(
        &self,
        _conversation_id: &str,
    ) -> Result<Vec<ConversationArtifactRow>, DbError> {
        Ok(Vec::new())
    }

    /// Returns a conversation artifact by ID scoped to a conversation.
    async fn get_artifact(
        &self,
        _conversation_id: &str,
        _conversation_artifact_id: &str,
    ) -> Result<Option<ConversationArtifactRow>, DbError> {
        Ok(None)
    }

    /// Inserts or updates a conversation artifact.
    ///
    /// Idempotency is keyed by `kind`:
    /// - `cron_trigger`: always a fresh INSERT (one row per trigger), returning
    ///   the row with its caller-minted `conversation_artifact_id`.
    /// - `skill_suggest`: upsert against the partial UNIQUE
    ///   `(conversation_id, cron_job_id) WHERE kind = 'skill_suggest'`.
    ///
    /// The existing business UUID is preserved when a `skill_suggest` row is
    /// updated. SQLite's AUTOINCREMENT `id` remains repository-internal.
    async fn upsert_artifact(
        &self,
        artifact: &ConversationArtifactRow,
    ) -> Result<ConversationArtifactRow, DbError> {
        Ok(artifact.clone())
    }

    /// Updates artifact status and returns the updated row if found.
    async fn update_artifact_status(
        &self,
        _conversation_id: &str,
        _conversation_artifact_id: &str,
        _status: &str,
        _updated_at: TimestampMs,
    ) -> Result<Option<ConversationArtifactRow>, DbError> {
        Ok(None)
    }

    /// Marks this owner's skill suggestion artifacts for a cron job as saved.
    /// Implementations must scope both the mutation and returned rows before
    /// changing state; checking ownership after an unscoped UPDATE is unsafe.
    async fn mark_skill_suggest_artifacts_saved(
        &self,
        _user_id: &str,
        _cron_job_id: &str,
        _updated_at: TimestampMs,
    ) -> Result<Vec<ConversationArtifactRow>, DbError> {
        Ok(Vec::new())
    }

    /// Deletes all artifacts belonging to a conversation.
    async fn delete_artifacts_by_conversation(
        &self,
        _conversation_id: &str,
    ) -> Result<(), DbError> {
        Ok(())
    }
}

// ── Supporting types ────────────────────────────────────────────────

/// Sort direction for message listing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SortOrder {
    #[default]
    Asc,
    Desc,
}

impl SortOrder {
    pub fn as_sql(&self) -> &'static str {
        match self {
            SortOrder::Asc => "ASC",
            SortOrder::Desc => "DESC",
        }
    }
}

/// Filters for paginated conversation listing.
#[derive(Debug, Clone, Default)]
pub struct ConversationFilters {
    /// Cursor: the ID of the last conversation from the previous page.
    pub cursor: Option<String>,
    /// Max items per page (default 20).
    pub limit: u32,
    /// Filter by conversation source.
    pub source: Option<String>,
    /// Filter by `cron_job_id` column.
    pub cron_job_id: Option<String>,
    /// Filter by pinned status.
    pub pinned: Option<bool>,
    /// Exclude companion companion (work-partner) sessions — rows whose
    /// `extra.companion_session` is `1`. Used by the companion's own conversation
    /// listing/count so its single companion thread does not inflate the
    /// "how many conversations" total. Default `false` (companion rows
    /// returned, matching the normal `/api/conversations` behavior).
    pub exclude_companion_companion: bool,
}

impl ConversationFilters {
    pub fn effective_limit(&self) -> u32 {
        if self.limit == 0 { 20 } else { self.limit }
    }
}

/// Partial update payload for a conversation row.
///
/// `None` = keep existing value; `Some(v)` = set to `v`.
#[derive(Debug, Clone, Default)]
pub struct ConversationRowUpdate {
    pub name: Option<String>,
    pub pinned: Option<bool>,
    pub pinned_at: Option<Option<TimestampMs>>,
    pub model: Option<Option<String>>,
    pub extra: Option<String>,
    pub delegation_policy: Option<String>,
    pub execution_model_pool: Option<Option<String>>,
    pub decision_policy: Option<String>,
    pub execution_template_id: Option<Option<String>>,
    pub status: Option<String>,
    /// Set/clear the owning cron job. `Some(Some(id))` sets, `Some(None)` clears
    /// (used by the cron executor's post-create binding for `new_conversation`).
    pub cron_job_id: Option<Option<String>>,
    pub preset_id: Option<Option<String>>,
    pub preset_revision: Option<Option<i64>>,
    pub preset_snapshot: Option<Option<String>>,
    pub updated_at: Option<TimestampMs>,
}

/// Partial update payload for a message row.
#[derive(Debug, Clone, Default)]
pub struct MessageRowUpdate {
    pub content: Option<String>,
    pub status: Option<Option<String>>,
    pub hidden: Option<bool>,
}

/// A single result row from cross-conversation message search.
/// Includes full conversation fields for building nested response.
#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct MessageSearchRow {
    // Message fields
    pub message_id: String,
    #[sqlx(rename = "type")]
    pub r#type: String,
    pub content: String,
    pub created_at: TimestampMs,
    // Conversation fields
    pub conversation_id: String,
    pub conversation_name: String,
    pub conversation_type: String,
    pub conversation_extra: String,
    pub conversation_delegation_policy: String,
    pub conversation_execution_model_pool: Option<String>,
    pub conversation_decision_policy: String,
    pub conversation_execution_template_id: Option<String>,
    pub conversation_model: Option<String>,
    pub conversation_status: Option<String>,
    pub conversation_source: Option<String>,
    pub conversation_channel_chat_id: Option<String>,
    pub conversation_pinned: bool,
    pub conversation_pinned_at: Option<TimestampMs>,
    pub conversation_created_at: TimestampMs,
    pub conversation_updated_at: TimestampMs,
}

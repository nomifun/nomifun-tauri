use crate::error::DbError;
use crate::models::{CreateKnowledgeTagParams, KnowledgeBaseRow, KnowledgeBindingRow, KnowledgeTagRow, UpdateKnowledgeTagParams};

/// Data access abstraction for the `knowledge_bases` / `knowledge_bindings` /
/// `knowledge_binding_bases` tables.
///
/// Bases are global (not per-user), mirroring webhooks: a shared pool of
/// knowledge directories reused across sessions. Bindings are addressed by
/// `(target_kind, target_id)`; internally the former composite PK + JSON
/// `kb_ids` array are redesigned into a local technical `id`, a stable
/// `knowledge_binding_id` UUIDv7 business identity, type-discriminated target
/// columns, and the `knowledge_binding_bases` logical junction.
#[async_trait::async_trait]
pub trait IKnowledgeRepository: Send + Sync {
    /// Insert a new knowledge base row.
    async fn insert_base(&self, row: &KnowledgeBaseRow) -> Result<(), DbError>;

    /// Replace the mutable columns (name/description/extra/updated_at) of an
    /// existing base. Returns `DbError::NotFound` if absent.
    async fn update_base(&self, row: &KnowledgeBaseRow) -> Result<(), DbError>;

    /// Delete a base by id. Returns `DbError::NotFound` if absent.
    async fn delete_base(&self, id: &str) -> Result<(), DbError>;

    /// Return a single base by id, or `None`.
    async fn get_base(&self, id: &str) -> Result<Option<KnowledgeBaseRow>, DbError>;

    /// Return all bases ordered by creation time ascending (stable list order).
    async fn list_bases(&self) -> Result<Vec<KnowledgeBaseRow>, DbError>;

    /// Return the binding for a target (the `knowledge_bindings` row plus its
    /// ordered `kb_id` list from the junction), or `None` when never
    /// configured. The `Vec<String>` is sorted by `knowledge_binding_bases.position`.
    async fn get_binding(
        &self,
        target_kind: &str,
        target_id: &str,
    ) -> Result<Option<(KnowledgeBindingRow, Vec<String>)>, DbError>;

    /// Insert-or-replace the binding for a target in one transaction:
    ///   1. upsert the `knowledge_bindings` row (the `target_id` is written to
    ///      the column selected by `target_kind`), preserving its UUIDv7
    ///      `knowledge_binding_id`;
    ///   2. clear and re-insert `knowledge_binding_bases` for `kb_ids`,
    ///      preserving order via `position`.
    /// Returns the stable UUIDv7 `knowledge_binding_id`.
    #[allow(clippy::too_many_arguments)]
    async fn set_binding(
        &self,
        target_kind: &str,
        target_id: &str,
        kb_ids: &[String],
        enabled: bool,
        writeback: bool,
        writeback_mode: &str,
        writeback_eagerness: &str,
        channel_write_enabled: bool,
        updated_at: nomifun_common::TimestampMs,
    ) -> Result<String, DbError>;

    /// Delete the binding for a target (no-op when absent). Used by the
    /// conversation-delete hook so bindings don't accumulate as orphans.
    /// The repository removes `knowledge_binding_bases` explicitly.
    async fn delete_binding(&self, target_kind: &str, target_id: &str) -> Result<(), DbError>;

    /// All bindings that reference `kb_id` (via the `knowledge_binding_bases`
    /// junction), enabled or not. Powers the "who is using this base?"
    /// consumers view. Ordered by `target_kind` then `knowledge_binding_id`
    /// for stable display.
    async fn list_bindings_using_kb(&self, kb_id: &str) -> Result<Vec<KnowledgeBindingRow>, DbError>;

    // ── Knowledge tags (user-defined tag palette) ─────────────────────────

    /// Return all tag definitions ordered by `sort_order` ascending, then `key`.
    async fn list_knowledge_tags(&self) -> Result<Vec<KnowledgeTagRow>, DbError>;

    /// Insert a new tag definition.
    async fn create_knowledge_tag(&self, params: CreateKnowledgeTagParams) -> Result<(), DbError>;

    /// Update mutable fields of an existing tag. Returns `DbError::NotFound`
    /// if no tag with `key` exists.
    async fn update_knowledge_tag(&self, key: &str, params: UpdateKnowledgeTagParams) -> Result<(), DbError>;

    /// Delete a tag by key. Returns `DbError::NotFound` if absent.
    async fn delete_knowledge_tag(&self, key: &str) -> Result<(), DbError>;
}

use crate::error::DbError;
use crate::models::AttachmentRow;

/// Data access abstraction for the `attachments` table (requirement images).
#[async_trait::async_trait]
pub trait IAttachmentRepository: Send + Sync {
    /// Insert a row and return it with its SQLite-allocated technical `id`.
    async fn insert(&self, row: &AttachmentRow) -> Result<AttachmentRow, DbError>;

    async fn get_by_id(&self, id: i64) -> Result<Option<AttachmentRow>, DbError>;

    async fn get_by_attachment_id(
        &self,
        attachment_id: &str,
    ) -> Result<Option<AttachmentRow>, DbError>;

    /// All attachments for a requirement, oldest first.
    async fn list_for_requirement(
        &self,
        requirement_id: &str,
    ) -> Result<Vec<AttachmentRow>, DbError>;

    /// Delete by id. Returns whether a row was deleted (absent id is not an
    /// error — callers do best-effort cleanup).
    async fn delete(&self, id: i64) -> Result<bool, DbError>;

    /// Delete every attachment row for one requirement in a single database
    /// transaction boundary. Files are staged separately by `AttachmentStore`
    /// before this method runs, so a database failure can restore them without
    /// leaving rows that point at already-removed files.
    async fn delete_for_requirement(&self, requirement_id: &str) -> Result<u64, DbError>;
}

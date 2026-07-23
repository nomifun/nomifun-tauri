use nomifun_common::RequirementId;
use sqlx::SqlitePool;

use crate::error::DbError;
use crate::models::AttachmentRow;
use crate::repository::attachment::IAttachmentRepository;

#[derive(Clone, Debug)]
pub struct SqliteAttachmentRepository {
    pool: SqlitePool,
}

impl SqliteAttachmentRepository {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }
}

#[async_trait::async_trait]
impl IAttachmentRepository for SqliteAttachmentRepository {
    async fn insert(&self, row: &AttachmentRow) -> Result<AttachmentRow, DbError> {
        let mut tx = self.pool.begin().await?;
        let requirement_id = RequirementId::parse(&row.requirement_id).map_err(|error| {
            DbError::Conflict(format!("invalid requirement id '{}': {error}", row.requirement_id))
        })?;
        let parent = sqlx::query(
            "UPDATE requirements SET updated_at = updated_at WHERE requirement_id = ?",
        )
        .bind(requirement_id.as_str())
        .execute(&mut *tx)
        .await?;
        if parent.rows_affected() == 0 {
            return Err(DbError::Conflict(format!(
                "attachment requirement '{}' does not exist",
                row.requirement_id
            )));
        }
        let inserted = sqlx::query_as::<_, AttachmentRow>(
            "INSERT INTO attachments (\
                attachment_id, requirement_id, file_name, rel_path, mime, size_bytes, created_by, created_at\
            ) VALUES (?, ?, ?, ?, ?, ?, ?, ?) RETURNING *",
        )
        .bind(&row.attachment_id)
        .bind(&row.requirement_id)
        .bind(&row.file_name)
        .bind(&row.rel_path)
        .bind(&row.mime)
        .bind(row.size_bytes)
        .bind(&row.created_by)
        .bind(row.created_at)
        .fetch_one(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(inserted)
    }

    async fn get_by_id(&self, id: i64) -> Result<Option<AttachmentRow>, DbError> {
        let row = sqlx::query_as::<_, AttachmentRow>("SELECT * FROM attachments WHERE id = ?")
            .bind(id)
            .fetch_optional(&self.pool)
            .await?;
        Ok(row)
    }

    async fn get_by_attachment_id(
        &self,
        attachment_id: &str,
    ) -> Result<Option<AttachmentRow>, DbError> {
        let row = sqlx::query_as::<_, AttachmentRow>(
            "SELECT * FROM attachments WHERE attachment_id = ?",
        )
        .bind(attachment_id)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row)
    }

    async fn list_for_requirement(
        &self,
        requirement_id: &str,
    ) -> Result<Vec<AttachmentRow>, DbError> {
        let requirement_id = RequirementId::parse(requirement_id).map_err(|error| {
            DbError::Conflict(format!("invalid requirement id '{requirement_id}': {error}"))
        })?;
        let rows = sqlx::query_as::<_, AttachmentRow>(
            "SELECT * FROM attachments WHERE requirement_id = ? ORDER BY created_at ASC, id ASC",
        )
        .bind(requirement_id.as_str())
        .fetch_all(&self.pool)
        .await?;
        Ok(rows)
    }

    async fn delete(&self, id: i64) -> Result<bool, DbError> {
        let result = sqlx::query("DELETE FROM attachments WHERE id = ?")
            .bind(id)
            .execute(&self.pool)
            .await?;
        Ok(result.rows_affected() > 0)
    }

    async fn delete_for_requirement(&self, requirement_id: &str) -> Result<u64, DbError> {
        let requirement_id = RequirementId::parse(requirement_id).map_err(|error| {
            DbError::Conflict(format!("invalid requirement id '{requirement_id}': {error}"))
        })?;
        let result = sqlx::query("DELETE FROM attachments WHERE requirement_id = ?")
            .bind(requirement_id.as_str())
            .execute(&self.pool)
            .await?;
        Ok(result.rows_affected())
    }
}

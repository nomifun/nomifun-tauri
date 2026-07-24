use nomifun_common::{CompanionId, ConversationId, KnowledgeBindingId, TerminalId};
use sqlx::{Sqlite, SqlitePool, Transaction};

use crate::error::DbError;
use crate::models::{CreateKnowledgeTagParams, KnowledgeBaseRow, KnowledgeBindingRow, KnowledgeTagRow, UpdateKnowledgeTagParams};
use crate::repository::knowledge::IKnowledgeRepository;

#[derive(Clone, Debug)]
pub struct SqliteKnowledgeRepository {
    pool: SqlitePool,
}

impl SqliteKnowledgeRepository {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }
}


/// Map a binding `target_kind` to the `knowledge_bindings` column that carries
/// its `target_id`. Returns `None` for an unrecognized kind so callers can
/// reject it without risking a write to the wrong column.
fn target_column(target_kind: &str) -> Option<&'static str> {
    match target_kind {
        "workpath" => Some("target_workpath"),
        "conversation" => Some("target_conversation_id"),
        "terminal" => Some("target_terminal_id"),
        "companion" => Some("target_companion_id"),
        _ => None,
    }
}

async fn lock_binding_target(
    tx: &mut Transaction<'_, Sqlite>,
    target_kind: &str,
    target_id: &str,
) -> Result<String, DbError> {
    match target_kind {
        "conversation" => {
            let target_id = ConversationId::parse(target_id).map_err(|error| {
                DbError::Conflict(format!(
                    "knowledge conversation target '{target_id}' is not a canonical UUIDv7: {error}"
                ))
            })?;
            let parent = sqlx::query(
                "UPDATE conversations SET updated_at = updated_at WHERE conversation_id = ?",
            )
            .bind(target_id.as_str())
            .execute(&mut **tx)
            .await?;
            if parent.rows_affected() == 0 {
                return Err(DbError::Conflict(format!(
                    "knowledge conversation target '{}' does not exist",
                    target_id
                )));
            }
            Ok(target_id.into_string())
        }
        "terminal" => {
            let target_id = TerminalId::parse(target_id).map_err(|error| {
                DbError::Conflict(format!(
                    "knowledge terminal target '{target_id}' is not a canonical UUIDv7: {error}"
                ))
            })?;
            let parent = sqlx::query(
                "UPDATE terminal_sessions SET updated_at = updated_at WHERE terminal_id = ?",
            )
            .bind(target_id.as_str())
            .execute(&mut **tx)
            .await?;
            if parent.rows_affected() == 0 {
                return Err(DbError::Conflict(format!(
                    "knowledge terminal target '{}' does not exist",
                    target_id
                )));
            }
            Ok(target_id.into_string())
        }
        "companion" => CompanionId::parse(target_id)
            .map(|id| id.into_string())
            .map_err(|error| {
                DbError::Conflict(format!(
                    "knowledge companion target '{target_id}' is not a canonical UUIDv7: {error}"
                ))
            }),
        "workpath" if !target_id.trim().is_empty() && target_id.trim() == target_id => {
            Ok(target_id.to_owned())
        }
        "workpath" => Err(DbError::Conflict(
            "knowledge workpath target must be non-empty and trimmed".into(),
        )),
        _ => Err(DbError::NotFound(format!(
            "unknown knowledge binding kind {target_kind}"
        ))),
    }
}

async fn lock_knowledge_bases(
    tx: &mut Transaction<'_, Sqlite>,
    kb_ids: &[String],
) -> Result<(), DbError> {
    let mut seen = std::collections::HashSet::with_capacity(kb_ids.len());
    for kb_id in kb_ids {
        let kb_id = nomifun_common::KnowledgeBaseId::parse(kb_id).map_err(|error| {
            DbError::Conflict(format!(
                "knowledge base id '{kb_id}' is not a canonical UUIDv7: {error}"
            ))
        })?;
        if !seen.insert(kb_id.as_str().to_owned()) {
            return Err(DbError::Conflict(format!(
                "knowledge binding contains duplicate base '{}'",
                kb_id
            )));
        }
        let parent = sqlx::query(
            "UPDATE knowledge_bases SET updated_at = updated_at WHERE knowledge_base_id = ?",
        )
        .bind(kb_id.as_str())
        .execute(&mut **tx)
        .await?;
        if parent.rows_affected() == 0 {
            return Err(DbError::Conflict(format!(
                "knowledge base '{}' does not exist",
                kb_id
            )));
        }
    }
    Ok(())
}

#[async_trait::async_trait]
impl IKnowledgeRepository for SqliteKnowledgeRepository {
    async fn insert_base(&self, row: &KnowledgeBaseRow) -> Result<(), DbError> {
        sqlx::query(
            "INSERT INTO knowledge_bases (\
                knowledge_base_id, name, description, root_path, managed, extra, created_at, updated_at, tags\
            ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(&row.knowledge_base_id)
        .bind(&row.name)
        .bind(&row.description)
        .bind(&row.root_path)
        .bind(row.managed)
        .bind(&row.extra)
        .bind(row.created_at)
        .bind(row.updated_at)
        .bind(&row.tags)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    async fn update_base(&self, row: &KnowledgeBaseRow) -> Result<(), DbError> {
        let result = sqlx::query(
            "UPDATE knowledge_bases SET name = ?, description = ?, extra = ?, tags = ?, updated_at = ? \
             WHERE knowledge_base_id = ?",
        )
        .bind(&row.name)
        .bind(&row.description)
        .bind(&row.extra)
        .bind(&row.tags)
        .bind(row.updated_at)
        .bind(&row.knowledge_base_id)
        .execute(&self.pool)
        .await?;
        if result.rows_affected() == 0 {
            return Err(DbError::NotFound(format!(
                "knowledge base {}",
                row.knowledge_base_id
            )));
        }
        Ok(())
    }

    async fn delete_base(&self, id: &str) -> Result<(), DbError> {
        let mut transaction = self.pool.begin().await?;

        // Serialize the logical-reference check and delete under SQLite's
        // writer lock.
        let locked = sqlx::query(
            "UPDATE knowledge_bases \
             SET updated_at = updated_at \
             WHERE knowledge_base_id = ?",
        )
        .bind(id)
        .execute(&mut *transaction)
        .await?;
        if locked.rows_affected() == 0 {
            return Err(DbError::NotFound(format!("knowledge base {id}")));
        }

        // RESTRICT: presets are durable configuration and must be
        // explicitly edited before a referenced knowledge base can disappear.
        let preset_reference_exists: bool = sqlx::query_scalar(
            "SELECT EXISTS(\
                SELECT 1 FROM preset_knowledge_bases \
                WHERE knowledge_base_id = ?\
             )",
        )
        .bind(id)
        .fetch_one(&mut *transaction)
        .await?;
        if preset_reference_exists {
            return Err(DbError::Conflict(format!(
                "knowledge base {id} is still referenced by a preset"
            )));
        }

        // CASCADE: remove this base from every session/workpath binding. The
        // binding row itself remains and may still contain other ordered bases.
        sqlx::query(
            "DELETE FROM knowledge_binding_bases \
             WHERE knowledge_base_id = ?",
        )
        .bind(id)
        .execute(&mut *transaction)
        .await?;

        sqlx::query("DELETE FROM knowledge_bases WHERE knowledge_base_id = ?")
            .bind(id)
            .execute(&mut *transaction)
            .await?;
        transaction.commit().await?;
        Ok(())
    }

    async fn get_base(&self, id: &str) -> Result<Option<KnowledgeBaseRow>, DbError> {
        let row = sqlx::query_as::<_, KnowledgeBaseRow>(
            "SELECT * FROM knowledge_bases WHERE knowledge_base_id = ?",
        )
            .bind(id)
            .fetch_optional(&self.pool)
            .await?;
        Ok(row)
    }

    async fn list_bases(&self) -> Result<Vec<KnowledgeBaseRow>, DbError> {
        let rows = sqlx::query_as::<_, KnowledgeBaseRow>("SELECT * FROM knowledge_bases ORDER BY created_at ASC")
            .fetch_all(&self.pool)
            .await?;
        Ok(rows)
    }

    async fn get_binding(
        &self,
        target_kind: &str,
        target_id: &str,
    ) -> Result<Option<(KnowledgeBindingRow, Vec<String>)>, DbError> {
        let Some(column) = target_column(target_kind) else {
            return Ok(None);
        };
        // The kind is fixed to a static column name above, never user input,
        // so this format! cannot inject. Also filter on target_kind so a stray
        // value in the wrong column can never satisfy the lookup.
        let sql = format!(
            "SELECT * FROM knowledge_bindings WHERE target_kind = ? AND {column} = ?"
        );
        let row = sqlx::query_as::<_, KnowledgeBindingRow>(&sql)
            .bind(target_kind)
            .bind(target_id)
            .fetch_optional(&self.pool)
            .await?;
        let Some(row) = row else {
            return Ok(None);
        };
        let kb_ids = self.fetch_kb_ids(&row.knowledge_binding_id).await?;
        Ok(Some((row, kb_ids)))
    }

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
    ) -> Result<String, DbError> {
        let Some(column) = target_column(target_kind) else {
            return Err(DbError::NotFound(format!(
                "unknown knowledge binding kind {target_kind}"
            )));
        };

        let mut tx = self.pool.begin().await?;
        let target_id = lock_binding_target(&mut tx, target_kind, target_id).await?;
        lock_knowledge_bases(&mut tx, kb_ids).await?;

        // 1. Upsert the main row. Preserve the stable UUIDv7 business ID when
        //    the target already has a binding; the local integer `id` remains
        //    private to the table and is never used as a relationship key.
        let select_sql = format!(
            "SELECT knowledge_binding_id FROM knowledge_bindings \
             WHERE target_kind = ? AND {column} = ?"
        );
        let existing: Option<String> = sqlx::query_scalar(&select_sql)
            .bind(target_kind)
            .bind(&target_id)
            .fetch_optional(&mut *tx)
            .await?;

        let knowledge_binding_id = if let Some(knowledge_binding_id) = existing {
            let knowledge_binding_id =
                KnowledgeBindingId::parse(knowledge_binding_id).map_err(|error| {
                    DbError::Conflict(format!(
                        "stored knowledge_binding_id is not a canonical UUIDv7: {error}"
                    ))
                })?;
            sqlx::query(
                "UPDATE knowledge_bindings \
                 SET enabled = ?, writeback = ?, writeback_mode = ?, writeback_eagerness = ?, \
                     channel_write_enabled = ?, updated_at = ? \
                 WHERE knowledge_binding_id = ?",
            )
            .bind(enabled)
            .bind(writeback)
            .bind(writeback_mode)
            .bind(writeback_eagerness)
            .bind(channel_write_enabled)
            .bind(updated_at)
            .bind(knowledge_binding_id.as_str())
            .execute(&mut *tx)
            .await?;
            knowledge_binding_id
        } else {
            // The other three target columns stay NULL; the CHECK enforces
            // exactly-one-non-null matching target_kind.
            let knowledge_binding_id = KnowledgeBindingId::new();
            let insert_sql = format!(
                "INSERT INTO knowledge_bindings \
                    (knowledge_binding_id, target_kind, {column}, enabled, writeback, \
                     writeback_mode, writeback_eagerness, channel_write_enabled, updated_at) \
                 VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)"
            );
            sqlx::query(&insert_sql)
                .bind(knowledge_binding_id.as_str())
                .bind(target_kind)
                .bind(&target_id)
                .bind(enabled)
            .bind(writeback)
            .bind(writeback_mode)
            .bind(writeback_eagerness)
                .bind(channel_write_enabled)
                .bind(updated_at)
                .execute(&mut *tx)
                .await?;
            knowledge_binding_id
        };

        // 2. Replace the junction rows for this binding, preserving kb_ids order.
        sqlx::query(
            "DELETE FROM knowledge_binding_bases WHERE knowledge_binding_id = ?",
        )
            .bind(knowledge_binding_id.as_str())
            .execute(&mut *tx)
            .await?;
        for (position, kb_id) in kb_ids.iter().enumerate() {
            sqlx::query(
                "INSERT INTO knowledge_binding_bases \
                    (knowledge_binding_id, knowledge_base_id, position) VALUES (?, ?, ?)",
            )
            .bind(knowledge_binding_id.as_str())
            .bind(kb_id)
            .bind(position as i64)
            .execute(&mut *tx)
            .await?;
        }

        tx.commit().await?;
        Ok(knowledge_binding_id.into_string())
    }

    async fn delete_binding(&self, target_kind: &str, target_id: &str) -> Result<(), DbError> {
        let Some(column) = target_column(target_kind) else {
            return Ok(());
        };
        // Remove the junction rows and binding atomically by the stable
        // business ID. The local technical `id` never crosses tables.
        let select_sql = format!(
            "SELECT knowledge_binding_id FROM knowledge_bindings \
             WHERE target_kind = ? AND {column} = ?"
        );
        let mut tx = self.pool.begin().await?;
        let knowledge_binding_id: Option<String> = sqlx::query_scalar(&select_sql)
            .bind(target_kind)
            .bind(target_id)
            .fetch_optional(&mut *tx)
            .await?;
        if let Some(knowledge_binding_id) = knowledge_binding_id {
            KnowledgeBindingId::parse(knowledge_binding_id.clone()).map_err(|error| {
                DbError::Conflict(format!(
                    "stored knowledge_binding_id is not a canonical UUIDv7: {error}"
                ))
            })?;
            sqlx::query(
                "DELETE FROM knowledge_binding_bases WHERE knowledge_binding_id = ?",
            )
                .bind(knowledge_binding_id)
                .execute(&mut *tx)
                .await?;
        }
        let sql = format!(
            "DELETE FROM knowledge_bindings WHERE target_kind = ? AND {column} = ?"
        );
        sqlx::query(&sql)
            .bind(target_kind)
            .bind(target_id)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(())
    }

    async fn list_bindings_using_kb(&self, kb_id: &str) -> Result<Vec<KnowledgeBindingRow>, DbError> {
        let rows = sqlx::query_as::<_, KnowledgeBindingRow>(
            "SELECT b.* FROM knowledge_bindings b \
             JOIN knowledge_binding_bases j \
               ON j.knowledge_binding_id = b.knowledge_binding_id \
             WHERE j.knowledge_base_id = ? \
             ORDER BY b.target_kind ASC, b.knowledge_binding_id ASC",
        )
        .bind(kb_id)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows)
    }

    // ── Knowledge tags ────────────────────────────────────────────────────

    async fn list_knowledge_tags(&self) -> Result<Vec<KnowledgeTagRow>, DbError> {
        let rows = sqlx::query_as::<_, KnowledgeTagRow>(
            "SELECT * FROM knowledge_tags ORDER BY sort_order ASC, key ASC",
        )
        .fetch_all(&self.pool)
        .await?;
        Ok(rows)
    }

    async fn create_knowledge_tag(&self, params: CreateKnowledgeTagParams) -> Result<(), DbError> {
        sqlx::query(
            "INSERT INTO knowledge_tags (key, label, color, sort_order, created_at) VALUES (?, ?, ?, ?, ?)",
        )
        .bind(&params.key)
        .bind(&params.label)
        .bind(&params.color)
        .bind(params.sort_order)
        .bind(params.created_at)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    async fn update_knowledge_tag(&self, key: &str, params: UpdateKnowledgeTagParams) -> Result<(), DbError> {
        // Build a dynamic SET clause from the provided fields.
        let mut sets: Vec<&str> = Vec::new();
        if params.label.is_some() {
            sets.push("label = ?");
        }
        if params.color.is_some() {
            sets.push("color = ?");
        }
        if params.sort_order.is_some() {
            sets.push("sort_order = ?");
        }
        if sets.is_empty() {
            // Nothing to update; verify the key exists.
            let exists = sqlx::query_scalar::<_, i64>(
                "SELECT COUNT(*) FROM knowledge_tags WHERE key = ?",
            )
            .bind(key)
            .fetch_one(&self.pool)
            .await?;
            if exists == 0 {
                return Err(DbError::NotFound(format!("knowledge tag {key}")));
            }
            return Ok(());
        }
        let sql = format!("UPDATE knowledge_tags SET {} WHERE key = ?", sets.join(", "));
        let mut query = sqlx::query(&sql);
        if let Some(ref label) = params.label {
            query = query.bind(label);
        }
        if let Some(ref color) = params.color {
            query = query.bind(color.as_deref());
        }
        if let Some(sort_order) = params.sort_order {
            query = query.bind(sort_order);
        }
        query = query.bind(key);
        let result = query.execute(&self.pool).await?;
        if result.rows_affected() == 0 {
            return Err(DbError::NotFound(format!("knowledge tag {key}")));
        }
        Ok(())
    }

    async fn delete_knowledge_tag(&self, key: &str) -> Result<(), DbError> {
        let result = sqlx::query("DELETE FROM knowledge_tags WHERE key = ?")
            .bind(key)
            .execute(&self.pool)
            .await?;
        if result.rows_affected() == 0 {
            return Err(DbError::NotFound(format!("knowledge tag {key}")));
        }
        Ok(())
    }
}

impl SqliteKnowledgeRepository {
    /// Reassemble a binding's knowledge-base list through its UUIDv7 logical
    /// reference, ordered by the original position.
    async fn fetch_kb_ids(
        &self,
        knowledge_binding_id: &KnowledgeBindingId,
    ) -> Result<Vec<String>, DbError> {
        let kb_ids = sqlx::query_scalar::<_, String>(
            "SELECT knowledge_base_id FROM knowledge_binding_bases \
             WHERE knowledge_binding_id = ? \
             ORDER BY position ASC, knowledge_base_id ASC",
        )
        .bind(knowledge_binding_id.as_str())
        .fetch_all(&self.pool)
        .await?;
        Ok(kb_ids)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::database::init_database_memory;

    const CONVERSATION_ID: &str = "019abcde-f012-7abc-8abc-0123456789ab";
    const OTHER_CONVERSATION_ID: &str = "019abcde-f012-7abc-8abc-0123456789ac";
    const KB_A: &str = "019abcde-f012-7abc-8abc-0123456789ad";
    const KB_B: &str = "019abcde-f012-7abc-8abc-0123456789ae";
    const KB_T: &str = "019abcde-f012-7abc-8abc-0123456789af";
    const KB_MISSING: &str = "019abcde-f012-7abc-8abc-0123456789b0";

    fn make_base(id: &str) -> KnowledgeBaseRow {
        KnowledgeBaseRow {
            id: 0,
            knowledge_base_id: id.to_owned(),
            name: format!("kb-{id}"),
            description: String::new(),
            root_path: format!("/tmp/{id}"),
            managed: true,
            extra: "{}".into(),
            created_at: 1,
            updated_at: 1,
            tags: None,
        }
    }

    #[tokio::test]
    async fn base_crud_roundtrip() {
        let db = init_database_memory().await.unwrap();
        let repo = SqliteKnowledgeRepository::new(db.pool().clone());

        repo.insert_base(&make_base(KB_A)).await.unwrap();
        repo.insert_base(&make_base(KB_B)).await.unwrap();
        assert_eq!(repo.list_bases().await.unwrap().len(), 2);

        let mut row = repo.get_base(KB_A).await.unwrap().unwrap();
        row.name = "renamed".into();
        // `extra` is mutable through update (URL-source config lives there).
        row.extra = r#"{"source":{"kind":"url","mode":"live"}}"#.into();
        row.updated_at = 2;
        repo.update_base(&row).await.unwrap();
        let got = repo.get_base(KB_A).await.unwrap().unwrap();
        assert_eq!(got.name, "renamed");
        assert_eq!(got.extra, r#"{"source":{"kind":"url","mode":"live"}}"#);

        repo.delete_base(KB_A).await.unwrap();
        assert!(repo.get_base(KB_A).await.unwrap().is_none());
        assert!(matches!(repo.delete_base(KB_A).await, Err(DbError::NotFound(_))));
    }

    #[tokio::test]
    async fn delete_base_cascades_binding_membership_but_restricts_preset_usage() {
        let db = init_database_memory().await.unwrap();
        let repo = SqliteKnowledgeRepository::new(db.pool().clone());
        repo.insert_base(&make_base(KB_A)).await.unwrap();
        repo.insert_base(&make_base(KB_B)).await.unwrap();
        repo.set_binding(
            "workpath",
            "/project",
            &[KB_A.to_owned(), KB_B.to_owned()],
            true,
            false,
            "staged",
            "conservative",
            false,
            1,
        )
        .await
        .unwrap();

        repo.delete_base(KB_A).await.unwrap();
        let (_, remaining) = repo
            .get_binding("workpath", "/project")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(remaining, vec![KB_B.to_owned()]);

        let preset_id = nomifun_common::PresetId::new();
        assert!(nomifun_common::validate_uuidv7(preset_id.as_str()).is_ok());
        sqlx::query(
            "INSERT INTO presets \
             (preset_id, source_kind, source_key, revision, name, fallback_allowed, created_at, updated_at) \
             VALUES (?, 'builtin', 'fixture-preset', 1, 'Preset', 1, 1, 1)",
        )
        .bind(preset_id.as_str())
        .execute(db.pool())
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO preset_knowledge_bases \
             (preset_id, knowledge_base_id, sort_order, required) \
             VALUES (?, ?, 0, 1)",
        )
        .bind(preset_id.as_str())
        .bind(KB_B)
        .execute(db.pool())
        .await
        .unwrap();

        let error = repo.delete_base(KB_B).await.unwrap_err();
        assert!(matches!(error, DbError::Conflict(_)));
        assert!(repo.get_base(KB_B).await.unwrap().is_some());
        let (_, still_bound) = repo
            .get_binding("workpath", "/project")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(still_bound, vec![KB_B.to_owned()]);
    }

    /// Insert a conversation so the conversation-kind binding has a valid
    /// logical target for the repository-level test.
    async fn seed_conversation(pool: &SqlitePool, id: &str) {
        let installation_owner = crate::installation_owner_id(pool).await.unwrap();
        sqlx::query(
            "INSERT INTO conversations (conversation_id, user_id, name, type, status, created_at, updated_at) \
             VALUES (?, ?, 'c', 'nomi', 'pending', 1, 1)",
        )
        .bind(id)
        .bind(installation_owner)
        .execute(pool)
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn binding_set_get_roundtrip() {
        let db = init_database_memory().await.unwrap();
        let repo = SqliteKnowledgeRepository::new(db.pool().clone());
        seed_conversation(db.pool(), CONVERSATION_ID).await;
        repo.insert_base(&make_base(KB_A)).await.unwrap();
        repo.insert_base(&make_base(KB_B)).await.unwrap();

        assert!(repo
            .get_binding("conversation", CONVERSATION_ID)
            .await
            .unwrap()
            .is_none());

        // Initial set: one base, disabled writeback, staged mode.
        let id1 = repo
            .set_binding(
                "conversation",
                CONVERSATION_ID,
                &[KB_A.to_owned()],
                true,
                false,
                "staged",
                "conservative",
                false,
                1,
            )
            .await
            .unwrap();
        let knowledge_binding_id = KnowledgeBindingId::parse(id1.clone()).unwrap();

        let (row, kb_ids) = repo
            .get_binding("conversation", CONVERSATION_ID)
            .await
            .unwrap()
            .unwrap();
        assert!(row.id > 0, "SQLite must allocate a private technical id");
        assert_eq!(row.knowledge_binding_id, knowledge_binding_id);
        assert_eq!(row.target_kind, "conversation");
        assert_eq!(row.target_id().as_deref(), Some(CONVERSATION_ID));
        assert_eq!(
            row.target_conversation_id
                .as_ref()
                .map(ToString::to_string)
                .as_deref(),
            Some(CONVERSATION_ID)
        );
        assert!(
            row.target_workpath.is_none()
                && row.target_terminal_id.is_none()
                && row.target_companion_id.is_none()
        );
        assert!(row.enabled);
        assert!(!row.writeback);
        assert_eq!(row.writeback_mode, "staged");
        assert_eq!(row.writeback_eagerness, "conservative");
        assert_eq!(kb_ids, vec![KB_A.to_owned()]);

        // Update: same target reuses the UUIDv7 business ID; junction replaced
        // and reordered while the technical row key remains private.
        let id2 = repo
            .set_binding(
                "conversation",
                CONVERSATION_ID,
                &[KB_B.to_owned(), KB_A.to_owned()],
                true,
                true,
                "direct",
                "aggressive",
                false,
                2,
            )
            .await
            .unwrap();
        assert_eq!(
            id2, id1,
            "same target must preserve knowledge_binding_id"
        );

        let (row, kb_ids) = repo
            .get_binding("conversation", CONVERSATION_ID)
            .await
            .unwrap()
            .unwrap();
        assert!(row.writeback);
        assert_eq!(row.writeback_mode, "direct");
        assert_eq!(row.writeback_eagerness, "aggressive");
        assert_eq!(row.updated_at, 2);
        // Order from kb_ids slice is preserved via position.
        assert_eq!(kb_ids, vec![KB_B.to_owned(), KB_A.to_owned()]);

        repo.delete_binding("conversation", CONVERSATION_ID)
            .await
            .unwrap();
        assert!(repo
            .get_binding("conversation", CONVERSATION_ID)
            .await
            .unwrap()
            .is_none());
        // Deleting an absent binding is a no-op, not an error.
        repo.delete_binding("conversation", CONVERSATION_ID)
            .await
            .unwrap();
    }

    /// A workpath binding is keyed by a path string rather than another
    /// entity, so it exercises the workpath target column and partial UNIQUE.
    #[tokio::test]
    async fn binding_workpath_kind_and_empty_kb_ids() {
        let db = init_database_memory().await.unwrap();
        let repo = SqliteKnowledgeRepository::new(db.pool().clone());

        let bid = repo
            .set_binding("workpath", "/work/proj", &[], false, false, "staged", "conservative", false, 5)
            .await
            .unwrap();
        assert!(KnowledgeBindingId::parse(&bid).is_ok());

        let (row, kb_ids) = repo.get_binding("workpath", "/work/proj").await.unwrap().unwrap();
        assert_eq!(row.target_kind, "workpath");
        assert_eq!(row.target_workpath.as_deref(), Some("/work/proj"));
        assert!(row.target_conversation_id.is_none());
        assert!(!row.enabled);
        assert!(kb_ids.is_empty(), "empty kb_ids slice yields no junction rows");

        // A different target_id is an independent binding.
        assert!(repo.get_binding("workpath", "/other").await.unwrap().is_none());
    }

    /// Raw conversation deletion leaves logically related rows unchanged.
    /// Repository-owned cleanup removes the binding and its junction rows.
    #[tokio::test]
    async fn deleting_conversation_requires_explicit_binding_cleanup() {
        let db = init_database_memory().await.unwrap();
        let repo = SqliteKnowledgeRepository::new(db.pool().clone());
        seed_conversation(db.pool(), OTHER_CONVERSATION_ID).await;
        repo.insert_base(&make_base(KB_A)).await.unwrap();

        let bid = repo
            .set_binding("conversation", OTHER_CONVERSATION_ID, &[KB_A.to_owned()], true, false, "staged", "conservative", false, 1)
            .await
            .unwrap();

        sqlx::query("DELETE FROM conversations WHERE conversation_id = ?")
            .bind(OTHER_CONVERSATION_ID)
            .execute(db.pool())
            .await
            .unwrap();

        assert!(repo
            .get_binding("conversation", OTHER_CONVERSATION_ID)
            .await
            .unwrap()
            .is_some());
        let orphans = sqlx::query_scalar::<_, i64>(
            "SELECT COUNT(*) FROM knowledge_binding_bases \
             WHERE knowledge_binding_id = ?",
        )
        .bind(&bid)
        .fetch_one(db.pool())
        .await
        .unwrap();
        assert_eq!(orphans, 1, "logical links remain until repository cleanup");

        repo.delete_binding("conversation", OTHER_CONVERSATION_ID)
            .await
            .unwrap();
        assert!(repo
            .get_binding("conversation", OTHER_CONVERSATION_ID)
            .await
            .unwrap()
            .is_none());
        let cleaned = sqlx::query_scalar::<_, i64>(
            "SELECT COUNT(*) FROM knowledge_binding_bases \
             WHERE knowledge_binding_id = ?",
        )
        .bind(&bid)
        .fetch_one(db.pool())
        .await
        .unwrap();
        assert_eq!(cleaned, 0, "repository cleanup removes junction rows explicitly");
    }

    /// An unknown kind is rejected on write and resolves to None on read,
    /// never silently writing to or matching the wrong column.
    #[tokio::test]
    async fn unknown_kind_is_rejected() {
        let db = init_database_memory().await.unwrap();
        let repo = SqliteKnowledgeRepository::new(db.pool().clone());

        assert!(matches!(
            repo.set_binding("bogus", "x", &[], true, false, "staged", "conservative", false, 1).await,
            Err(DbError::NotFound(_))
        ));
        assert!(repo.get_binding("bogus", "x").await.unwrap().is_none());
        // delete of an unknown kind is a no-op.
        repo.delete_binding("bogus", "x").await.unwrap();
    }

    /// The v3 `channel_write_enabled` field persists and updates.
    #[tokio::test]
    async fn binding_persists_channel_write_enabled() {
        let db = init_database_memory().await.unwrap();
        let repo = SqliteKnowledgeRepository::new(db.pool().clone());
        repo.insert_base(&make_base(KB_A)).await.unwrap();

        // Default (false) on a write without the flag set.
        repo.set_binding("workpath", "/wp", &[KB_A.to_owned()], true, true, "staged", "conservative", false, 1)
            .await
            .unwrap();
        let (row, _) = repo.get_binding("workpath", "/wp").await.unwrap().unwrap();
        assert!(!row.channel_write_enabled);

        // Re-enable on update.
        repo.set_binding("workpath", "/wp", &[KB_A.to_owned()], true, true, "staged", "conservative", true, 2)
            .await
            .unwrap();
        let (row, _) = repo.get_binding("workpath", "/wp").await.unwrap().unwrap();
        assert!(row.channel_write_enabled, "channel_write_enabled must persist + update");
    }

    #[tokio::test]
    async fn list_bindings_using_kb_returns_all_consumers() {
        let db = init_database_memory().await.unwrap();
        let repo = SqliteKnowledgeRepository::new(db.pool().clone());
        repo.insert_base(&make_base(KB_A)).await.unwrap();
        repo.insert_base(&make_base(KB_B)).await.unwrap();

        // Two workpath bindings use kb_a (one enabled, one disabled); one uses kb_b only.
        repo.set_binding("workpath", "/p1", &[KB_A.to_owned()], true, false, "staged", "conservative", false, 1)
            .await
            .unwrap();
        repo.set_binding("workpath", "/p2", &[KB_A.to_owned(), KB_B.to_owned()], false, false, "staged", "conservative", false, 1)
            .await
            .unwrap();
        repo.set_binding("workpath", "/p3", &[KB_B.to_owned()], true, false, "staged", "conservative", false, 1)
            .await
            .unwrap();

        let mut using_a = repo.list_bindings_using_kb(KB_A).await.unwrap();
        assert_eq!(using_a.len(), 2, "p1 + p2 mount kb_a");
        using_a.sort_by(|x, y| x.target_workpath.cmp(&y.target_workpath));
        assert_eq!(using_a[0].target_workpath.as_deref(), Some("/p1"));
        assert!(using_a[0].enabled);
        assert_eq!(using_a[1].target_workpath.as_deref(), Some("/p2"));
        assert!(!using_a[1].enabled, "disabled binding still listed");

        assert_eq!(repo.list_bindings_using_kb(KB_B).await.unwrap().len(), 2, "p2 + p3 mount kb_b");
        assert!(repo.list_bindings_using_kb(KB_MISSING).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn knowledge_tags_crud_roundtrip() {
        use crate::models::{CreateKnowledgeTagParams, UpdateKnowledgeTagParams};

        let db = init_database_memory().await.unwrap();
        let repo = SqliteKnowledgeRepository::new(db.pool().clone());

        // Initially empty.
        assert!(repo.list_knowledge_tags().await.unwrap().is_empty());

        // Create.
        repo.create_knowledge_tag(CreateKnowledgeTagParams {
            key: "research".into(),
            label: "研发".into(),
            color: Some("#4d9fff".into()),
            sort_order: 0,
            created_at: 1,
        })
        .await
        .unwrap();

        let tags = repo.list_knowledge_tags().await.unwrap();
        assert_eq!(tags.len(), 1);
        assert_eq!(tags[0].key, "research");
        assert_eq!(tags[0].label, "研发");
        assert_eq!(tags[0].color.as_deref(), Some("#4d9fff"));
        assert_eq!(tags[0].sort_order, 0);
        assert_eq!(tags[0].created_at, 1);

        // Update label only.
        repo.update_knowledge_tag("research", UpdateKnowledgeTagParams {
            label: Some("研发线".into()),
            ..Default::default()
        })
        .await
        .unwrap();
        let tags = repo.list_knowledge_tags().await.unwrap();
        assert_eq!(tags[0].label, "研发线");
        assert_eq!(tags[0].color.as_deref(), Some("#4d9fff"), "untouched field preserved");

        // Update color to None.
        repo.update_knowledge_tag("research", UpdateKnowledgeTagParams {
            color: Some(None),
            ..Default::default()
        })
        .await
        .unwrap();
        let tags = repo.list_knowledge_tags().await.unwrap();
        assert!(tags[0].color.is_none(), "color cleared");

        // Delete.
        repo.delete_knowledge_tag("research").await.unwrap();
        assert!(repo.list_knowledge_tags().await.unwrap().is_empty());

        // Delete absent → NotFound.
        assert!(matches!(
            repo.delete_knowledge_tag("research").await,
            Err(DbError::NotFound(_))
        ));

        // Update absent → NotFound.
        assert!(matches!(
            repo.update_knowledge_tag("missing", UpdateKnowledgeTagParams::default()).await,
            Err(DbError::NotFound(_))
        ));
    }

    /// The `tags` column on `knowledge_bases` is read/written through the
    /// existing base CRUD methods.
    #[tokio::test]
    async fn base_tags_column_roundtrip() {
        let db = init_database_memory().await.unwrap();
        let repo = SqliteKnowledgeRepository::new(db.pool().clone());

        // Insert with an explicitly absent optional tags value.
        repo.insert_base(&make_base(KB_T)).await.unwrap();
        let row = repo.get_base(KB_T).await.unwrap().unwrap();
        assert!(row.tags.is_none(), "NULL maps to None");

        // Update with a JSON tags value.
        let mut row = row;
        row.tags = Some(r#"["research","ops"]"#.into());
        row.updated_at = 2;
        repo.update_base(&row).await.unwrap();
        let got = repo.get_base(KB_T).await.unwrap().unwrap();
        assert_eq!(got.tags.as_deref(), Some(r#"["research","ops"]"#));

        // list_bases also returns the tags column.
        let all = repo.list_bases().await.unwrap();
        assert_eq!(all[0].tags.as_deref(), Some(r#"["research","ops"]"#));
    }

    #[tokio::test]
    async fn malformed_stored_knowledge_entity_ids_are_rejected_on_read() {
        let db = init_database_memory().await.unwrap();
        sqlx::query("PRAGMA ignore_check_constraints = ON")
            .execute(db.pool())
            .await
            .unwrap();
        sqlx::query(
            "INSERT INTO knowledge_bases \
             (knowledge_base_id, name, description, root_path, managed, extra, created_at, updated_at) \
             VALUES ('kb_1', 'bad', '', '/tmp/bad', 0, '{}', 1, 1)",
        )
        .execute(db.pool())
        .await
        .unwrap();
        sqlx::query("PRAGMA ignore_check_constraints = OFF")
            .execute(db.pool())
            .await
            .unwrap();

        let repo = SqliteKnowledgeRepository::new(db.pool().clone());
        assert!(repo.list_bases().await.is_err());
    }
}

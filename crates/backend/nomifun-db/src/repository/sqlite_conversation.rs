use sqlx::SqlitePool;

use nomifun_common::PaginatedResult;

use crate::error::DbError;
use crate::models::{ConversationArtifactRow, ConversationRow, MessageRow};
use crate::repository::bind::{BindValue, bind_value, bind_value_as};
use crate::repository::conversation::{
    ConversationFilters, ConversationRowUpdate, IConversationRepository, MessageRowUpdate, MessageSearchRow, SortOrder,
};

/// SQLite-backed implementation of [`IConversationRepository`].
#[derive(Clone, Debug)]
pub struct SqliteConversationRepository {
    pool: SqlitePool,
}

impl SqliteConversationRepository {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }
}

#[async_trait::async_trait]
impl IConversationRepository for SqliteConversationRepository {
    // ── Conversation CRUD ───────────────────────────────────────────

    async fn get(&self, id: i64) -> Result<Option<ConversationRow>, DbError> {
        let row = sqlx::query_as::<_, ConversationRow>("SELECT * FROM conversations WHERE id = ?")
            .bind(id)
            .fetch_optional(&self.pool)
            .await?;

        Ok(row)
    }

    async fn create(&self, row: &ConversationRow) -> Result<i64, DbError> {
        // `id` is allocated by SQLite (INTEGER PK AUTOINCREMENT) and never bound;
        // the caller receives the assigned id via last_insert_rowid().
        let result = sqlx::query(
            "INSERT INTO conversations \
                (user_id, name, type, extra, model, status, source, \
                 channel_chat_id, pinned, pinned_at, cron_job_id, created_at, updated_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(&row.user_id)
        .bind(&row.name)
        .bind(&row.r#type)
        .bind(&row.extra)
        .bind(&row.model)
        .bind(&row.status)
        .bind(&row.source)
        .bind(&row.channel_chat_id)
        .bind(row.pinned)
        .bind(row.pinned_at)
        .bind(&row.cron_job_id)
        .bind(row.created_at)
        .bind(row.updated_at)
        .execute(&self.pool)
        .await?;

        Ok(result.last_insert_rowid())
    }

    async fn update(&self, id: i64, updates: &ConversationRowUpdate) -> Result<(), DbError> {
        // Build dynamic SET clause
        let mut set_parts: Vec<String> = Vec::new();
        let mut binds: Vec<BindValue> = Vec::new();

        if let Some(ref name) = updates.name {
            set_parts.push("name = ?".to_string());
            binds.push(BindValue::Str(name.clone()));
        }
        if let Some(pinned) = updates.pinned {
            set_parts.push("pinned = ?".to_string());
            binds.push(BindValue::Bool(pinned));
        }
        if let Some(ref pinned_at) = updates.pinned_at {
            set_parts.push("pinned_at = ?".to_string());
            binds.push(BindValue::OptI64(*pinned_at));
        }
        if let Some(ref model) = updates.model {
            set_parts.push("model = ?".to_string());
            binds.push(BindValue::OptStr(model.clone()));
        }
        if let Some(ref extra) = updates.extra {
            set_parts.push("extra = ?".to_string());
            binds.push(BindValue::Str(extra.clone()));
        }
        if let Some(ref status) = updates.status {
            set_parts.push("status = ?".to_string());
            binds.push(BindValue::Str(status.clone()));
        }
        if let Some(ref cron_job_id) = updates.cron_job_id {
            set_parts.push("cron_job_id = ?".to_string());
            binds.push(BindValue::OptStr(cron_job_id.clone()));
        }
        if let Some(updated_at) = updates.updated_at {
            set_parts.push("updated_at = ?".to_string());
            binds.push(BindValue::I64(updated_at));
        }

        if set_parts.is_empty() {
            return Ok(());
        }

        let sql = format!("UPDATE conversations SET {} WHERE id = ?", set_parts.join(", "));

        let mut query = sqlx::query(&sql);
        for bind in &binds {
            query = bind_value(query, bind);
        }
        query = query.bind(id);

        let result = query.execute(&self.pool).await?;

        if result.rows_affected() == 0 {
            return Err(DbError::NotFound(format!("Conversation '{id}' not found")));
        }

        Ok(())
    }

    async fn delete(&self, id: i64) -> Result<(), DbError> {
        let result = sqlx::query("DELETE FROM conversations WHERE id = ?")
            .bind(id)
            .execute(&self.pool)
            .await?;

        if result.rows_affected() == 0 {
            return Err(DbError::NotFound(format!("Conversation '{id}' not found")));
        }

        Ok(())
    }

    async fn list_paginated(
        &self,
        user_id: &str,
        filters: &ConversationFilters,
    ) -> Result<PaginatedResult<ConversationRow>, DbError> {
        let limit = filters.effective_limit();
        // Fetch one extra row to determine hasMore
        let fetch_limit = limit + 1;

        let mut where_parts = vec!["c.user_id = ?".to_string()];
        let mut binds: Vec<BindValue> = vec![BindValue::Str(user_id.to_string())];

        // Cursor-based pagination: use updated_at of the cursor row
        if let Some(cursor_id) = filters.cursor {
            where_parts.push(
                "(c.updated_at < (SELECT updated_at FROM conversations WHERE id = ?) \
                 OR (c.updated_at = (SELECT updated_at FROM conversations WHERE id = ?) \
                     AND c.id < ?))"
                    .to_string(),
            );
            binds.push(BindValue::I64(cursor_id));
            binds.push(BindValue::I64(cursor_id));
            binds.push(BindValue::I64(cursor_id));
        }

        append_filter_conditions(filters, &mut where_parts, &mut binds);

        let where_clause = where_parts.join(" AND ");

        // Count total matching rows (without cursor filter for total)
        let count_sql = build_count_sql(user_id, filters);
        let total = execute_count(&self.pool, &count_sql.0, &count_sql.1).await?;

        // Fetch page
        let sql = format!(
            "SELECT c.* FROM conversations c \
             WHERE {where_clause} \
             ORDER BY c.updated_at DESC, c.id DESC \
             LIMIT ?"
        );

        let mut query = sqlx::query_as::<_, ConversationRow>(&sql);
        for bind in &binds {
            query = bind_value_as(query, bind);
        }
        query = query.bind(fetch_limit);

        let mut rows = query.fetch_all(&self.pool).await?;

        let has_more = rows.len() as u32 > limit;
        if has_more {
            rows.pop();
        }

        Ok(PaginatedResult {
            items: rows,
            total,
            has_more,
        })
    }

    // ── Extended queries ────────────────────────────────────────────

    async fn find_by_source_and_chat(
        &self,
        user_id: &str,
        source: &str,
        chat_id: &str,
        agent_type: &str,
    ) -> Result<Option<ConversationRow>, DbError> {
        let row = sqlx::query_as::<_, ConversationRow>(
            "SELECT * FROM conversations \
             WHERE user_id = ? AND source = ? AND channel_chat_id = ? AND type = ? \
             ORDER BY updated_at DESC \
             LIMIT 1",
        )
        .bind(user_id)
        .bind(source)
        .bind(chat_id)
        .bind(agent_type)
        .fetch_optional(&self.pool)
        .await?;

        Ok(row)
    }

    async fn list_by_cron_job(&self, user_id: &str, cron_job_id: &str) -> Result<Vec<ConversationRow>, DbError> {
        let rows = sqlx::query_as::<_, ConversationRow>(
            "SELECT * FROM conversations \
             WHERE user_id = ? \
             AND cron_job_id = ? \
             ORDER BY updated_at DESC",
        )
        .bind(user_id)
        .bind(cron_job_id)
        .fetch_all(&self.pool)
        .await?;

        Ok(rows)
    }

    async fn list_associated(&self, user_id: &str, conversation_id: i64) -> Result<Vec<ConversationRow>, DbError> {
        // First get the target conversation's workspace
        let target = sqlx::query_as::<_, ConversationRow>("SELECT * FROM conversations WHERE id = ? AND user_id = ?")
            .bind(conversation_id)
            .bind(user_id)
            .fetch_optional(&self.pool)
            .await?
            .ok_or_else(|| DbError::NotFound(format!("Conversation '{conversation_id}' not found")))?;

        // Extract workspace from extra JSON
        let workspace: Option<String> = serde_json::from_str::<serde_json::Value>(&target.extra)
            .ok()
            .and_then(|v: serde_json::Value| v.get("workspace")?.as_str().map(String::from));

        let Some(ref workspace) = workspace else {
            return Ok(Vec::new());
        };

        if workspace.is_empty() {
            return Ok(Vec::new());
        }

        // Find other conversations with the same workspace
        let rows = sqlx::query_as::<_, ConversationRow>(
            "SELECT * FROM conversations \
             WHERE user_id = ? \
               AND id != ? \
               AND json_extract(extra, '$.workspace') = ? \
             ORDER BY updated_at DESC",
        )
        .bind(user_id)
        .bind(conversation_id)
        .bind(workspace)
        .fetch_all(&self.pool)
        .await?;

        Ok(rows)
    }

    // ── Message operations ──────────────────────────────────────────

    async fn get_messages(
        &self,
        conv_id: i64,
        page: u32,
        page_size: u32,
        order: SortOrder,
    ) -> Result<PaginatedResult<MessageRow>, DbError> {
        let effective_page = if page == 0 { 1 } else { page };
        let effective_size = if page_size == 0 { 50 } else { page_size };
        let offset = (effective_page - 1) * effective_size;
        let fetch_limit = effective_size + 1;

        let count_row: (i64,) = sqlx::query_as(
            "SELECT COUNT(*) FROM messages \
                 WHERE conversation_id = ? \
                   AND type NOT IN ('cron_trigger', 'skill_suggest')",
        )
        .bind(conv_id)
        .fetch_one(&self.pool)
        .await?;
        let total = count_row.0 as u64;

        let sql = format!(
            "SELECT * FROM messages \
             WHERE conversation_id = ? \
               AND type NOT IN ('cron_trigger', 'skill_suggest') \
             ORDER BY created_at {}, id {} \
             LIMIT ? OFFSET ?",
            order.as_sql(),
            order.as_sql()
        );

        let mut rows = sqlx::query_as::<_, MessageRow>(&sql)
            .bind(conv_id)
            .bind(fetch_limit)
            .bind(offset)
            .fetch_all(&self.pool)
            .await?;

        let has_more = rows.len() as u32 > effective_size;
        if has_more {
            rows.pop();
        }

        Ok(PaginatedResult {
            items: rows,
            total,
            has_more,
        })
    }

    async fn get_messages_keyset(
        &self,
        conv_id: i64,
        before: Option<(i64, String)>,
        limit: u32,
    ) -> Result<PaginatedResult<MessageRow>, DbError> {
        let effective_limit = if limit == 0 { 40 } else { limit };
        let fetch_limit = effective_limit + 1;

        // Newest-first window; the keyset predicate `(created_at, id) < cursor`
        // is covered by idx_messages_conv_created_id. `id` (msg_{uuidv7}, time-
        // ordered) is the stable tiebreaker for rows sharing a `created_at` ms.
        let mut rows = if let Some((before_created_at, before_id)) = before {
            sqlx::query_as::<_, MessageRow>(
                "SELECT * FROM messages \
                 WHERE conversation_id = ? \
                   AND type NOT IN ('cron_trigger', 'skill_suggest') \
                   AND (created_at < ? OR (created_at = ? AND id < ?)) \
                 ORDER BY created_at DESC, id DESC \
                 LIMIT ?",
            )
            .bind(conv_id)
            .bind(before_created_at)
            .bind(before_created_at)
            .bind(&before_id)
            .bind(fetch_limit)
            .fetch_all(&self.pool)
            .await?
        } else {
            sqlx::query_as::<_, MessageRow>(
                "SELECT * FROM messages \
                 WHERE conversation_id = ? \
                   AND type NOT IN ('cron_trigger', 'skill_suggest') \
                 ORDER BY created_at DESC, id DESC \
                 LIMIT ?",
            )
            .bind(conv_id)
            .bind(fetch_limit)
            .fetch_all(&self.pool)
            .await?
        };

        let has_more = rows.len() as u32 > effective_limit;
        if has_more {
            rows.pop();
        }

        Ok(PaginatedResult {
            items: rows,
            total: 0, // keyset windows don't compute a full count
            has_more,
        })
    }

    async fn get_message(&self, conv_id: i64, message_id: &str) -> Result<Option<MessageRow>, DbError> {
        let row = sqlx::query_as::<_, MessageRow>(
            "SELECT * FROM messages \
             WHERE conversation_id = ? \
               AND id = ? \
               AND type NOT IN ('cron_trigger', 'skill_suggest')",
        )
        .bind(conv_id)
        .bind(message_id)
        .fetch_optional(&self.pool)
        .await?;

        Ok(row)
    }

    async fn insert_message(&self, message: &MessageRow) -> Result<(), DbError> {
        sqlx::query(
            "INSERT INTO messages \
                (id, conversation_id, msg_id, type, content, position, \
                 status, hidden, created_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(&message.id)
        .bind(&message.conversation_id)
        .bind(&message.msg_id)
        .bind(&message.r#type)
        .bind(&message.content)
        .bind(&message.position)
        .bind(&message.status)
        .bind(message.hidden)
        .bind(message.created_at)
        .execute(&self.pool)
        .await?;

        Ok(())
    }

    async fn update_message(&self, id: &str, updates: &MessageRowUpdate) -> Result<(), DbError> {
        let mut set_parts: Vec<String> = Vec::new();
        let mut binds: Vec<BindValue> = Vec::new();

        if let Some(ref content) = updates.content {
            set_parts.push("content = ?".to_string());
            binds.push(BindValue::Str(content.clone()));
        }
        if let Some(ref status) = updates.status {
            set_parts.push("status = ?".to_string());
            binds.push(BindValue::OptStr(status.clone()));
        }
        if let Some(hidden) = updates.hidden {
            set_parts.push("hidden = ?".to_string());
            binds.push(BindValue::Bool(hidden));
        }

        if set_parts.is_empty() {
            return Ok(());
        }

        let sql = format!("UPDATE messages SET {} WHERE id = ?", set_parts.join(", "));

        let mut query = sqlx::query(&sql);
        for bind in &binds {
            query = bind_value(query, bind);
        }
        query = query.bind(id);

        let result = query.execute(&self.pool).await?;

        if result.rows_affected() == 0 {
            return Err(DbError::NotFound(format!("Message '{id}' not found")));
        }

        Ok(())
    }

    async fn delete_messages_by_conversation(&self, conv_id: i64) -> Result<(), DbError> {
        sqlx::query("DELETE FROM messages WHERE conversation_id = ?")
            .bind(conv_id)
            .execute(&self.pool)
            .await?;

        Ok(())
    }

    async fn delete_messages_from(
        &self,
        conv_id: i64,
        from_created_at: i64,
        from_id: &str,
    ) -> Result<u64, DbError> {
        // Keyset 截断：删除 (created_at, id) >= 游标 的所有消息。
        // 命中复合索引 idx_messages_conv_created_id。
        let result = sqlx::query(
            "DELETE FROM messages \
             WHERE conversation_id = ? \
               AND (created_at > ? OR (created_at = ? AND id >= ?))",
        )
        .bind(conv_id)
        .bind(from_created_at)
        .bind(from_created_at)
        .bind(from_id)
        .execute(&self.pool)
        .await?;

        Ok(result.rows_affected())
    }

    async fn get_message_by_msg_id(
        &self,
        conv_id: i64,
        msg_id: &str,
        msg_type: &str,
    ) -> Result<Option<MessageRow>, DbError> {
        let row = sqlx::query_as::<_, MessageRow>(
            "SELECT * FROM messages \
             WHERE conversation_id = ? AND msg_id = ? AND type = ?",
        )
        .bind(conv_id)
        .bind(msg_id)
        .bind(msg_type)
        .fetch_optional(&self.pool)
        .await?;

        Ok(row)
    }

    async fn search_messages(
        &self,
        user_id: &str,
        keyword: &str,
        page: u32,
        page_size: u32,
    ) -> Result<PaginatedResult<MessageSearchRow>, DbError> {
        let effective_page = if page == 0 { 1 } else { page };
        let effective_size = if page_size == 0 { 20 } else { page_size };
        let offset = (effective_page - 1) * effective_size;
        let fetch_limit = effective_size + 1;

        let like_pattern = format!("%{keyword}%");

        let count_row: (i64,) = sqlx::query_as(
            "SELECT COUNT(*) FROM messages m \
             INNER JOIN conversations c ON m.conversation_id = c.id \
             WHERE c.user_id = ? AND m.content LIKE ?",
        )
        .bind(user_id)
        .bind(&like_pattern)
        .fetch_one(&self.pool)
        .await?;
        let total = count_row.0 as u64;

        let rows = sqlx::query_as::<_, MessageSearchRow>(
            "SELECT \
                m.id AS message_id, \
                m.type, \
                m.content, \
                m.created_at, \
                c.id AS conversation_id, \
                c.name AS conversation_name, \
                c.type AS conversation_type, \
                c.extra AS conversation_extra, \
                c.model AS conversation_model, \
                c.status AS conversation_status, \
                c.source AS conversation_source, \
                c.channel_chat_id AS conversation_channel_chat_id, \
                c.pinned AS conversation_pinned, \
                c.pinned_at AS conversation_pinned_at, \
                c.created_at AS conversation_created_at, \
                c.updated_at AS conversation_updated_at \
             FROM messages m \
             INNER JOIN conversations c ON m.conversation_id = c.id \
             WHERE c.user_id = ? AND m.content LIKE ? \
             ORDER BY m.created_at DESC \
             LIMIT ? OFFSET ?",
        )
        .bind(user_id)
        .bind(&like_pattern)
        .bind(fetch_limit)
        .bind(offset)
        .fetch_all(&self.pool)
        .await?;

        let has_more = rows.len() as u32 > effective_size;
        let items = if has_more {
            rows[..effective_size as usize].to_vec()
        } else {
            rows
        };

        Ok(PaginatedResult { items, total, has_more })
    }

    async fn list_artifacts(&self, conversation_id: i64) -> Result<Vec<ConversationArtifactRow>, DbError> {
        let rows = sqlx::query_as::<_, ConversationArtifactRow>(
            "SELECT * FROM conversation_artifacts \
             WHERE conversation_id = ? \
             ORDER BY created_at ASC, id ASC",
        )
        .bind(conversation_id)
        .fetch_all(&self.pool)
        .await?;

        Ok(rows)
    }

    async fn get_artifact(
        &self,
        conversation_id: i64,
        artifact_id: i64,
    ) -> Result<Option<ConversationArtifactRow>, DbError> {
        let row = sqlx::query_as::<_, ConversationArtifactRow>(
            "SELECT * FROM conversation_artifacts WHERE conversation_id = ? AND id = ?",
        )
        .bind(conversation_id)
        .bind(artifact_id)
        .fetch_optional(&self.pool)
        .await?;

        Ok(row)
    }

    async fn upsert_artifact(&self, artifact: &ConversationArtifactRow) -> Result<ConversationArtifactRow, DbError> {
        // `id` is allocated by SQLite (INTEGER PK AUTOINCREMENT) and never bound.
        // Idempotency depends on `kind`:
        //   - skill_suggest: upsert against the partial UNIQUE index
        //     uq_conversation_artifacts_skill_suggest
        //     ON (conversation_id, cron_job_id) WHERE kind = 'skill_suggest'.
        //     The ON CONFLICT target must repeat the same WHERE predicate.
        //   - cron_trigger: plain INSERT, one row per trigger (no unique
        //     constraint, no ON CONFLICT clause).
        let id = if artifact.kind == "skill_suggest" {
            sqlx::query_scalar::<_, i64>(
                "INSERT INTO conversation_artifacts \
                    (conversation_id, cron_job_id, kind, status, payload, created_at, updated_at) \
                 VALUES (?, ?, ?, ?, ?, ?, ?) \
                 ON CONFLICT(conversation_id, cron_job_id) WHERE kind = 'skill_suggest' DO UPDATE SET \
                    status = excluded.status, \
                    payload = excluded.payload, \
                    updated_at = excluded.updated_at \
                 RETURNING id",
            )
            .bind(&artifact.conversation_id)
            .bind(&artifact.cron_job_id)
            .bind(&artifact.kind)
            .bind(&artifact.status)
            .bind(&artifact.payload)
            .bind(artifact.created_at)
            .bind(artifact.updated_at)
            .fetch_one(&self.pool)
            .await?
        } else {
            let result = sqlx::query(
                "INSERT INTO conversation_artifacts \
                    (conversation_id, cron_job_id, kind, status, payload, created_at, updated_at) \
                 VALUES (?, ?, ?, ?, ?, ?, ?)",
            )
            .bind(&artifact.conversation_id)
            .bind(&artifact.cron_job_id)
            .bind(&artifact.kind)
            .bind(&artifact.status)
            .bind(&artifact.payload)
            .bind(artifact.created_at)
            .bind(artifact.updated_at)
            .execute(&self.pool)
            .await?;
            result.last_insert_rowid()
        };

        self.get_artifact(artifact.conversation_id, id)
            .await?
            .ok_or_else(|| DbError::Init(format!("upsert artifact did not produce row for id '{id}'")))
    }

    async fn update_artifact_status(
        &self,
        conversation_id: i64,
        artifact_id: i64,
        status: &str,
        updated_at: i64,
    ) -> Result<Option<ConversationArtifactRow>, DbError> {
        let result = sqlx::query(
            "UPDATE conversation_artifacts \
             SET status = ?, updated_at = ? \
             WHERE conversation_id = ? AND id = ?",
        )
        .bind(status)
        .bind(updated_at)
        .bind(conversation_id)
        .bind(artifact_id)
        .execute(&self.pool)
        .await?;

        if result.rows_affected() == 0 {
            return Ok(None);
        }

        self.get_artifact(conversation_id, artifact_id).await
    }

    async fn mark_skill_suggest_artifacts_saved(
        &self,
        cron_job_id: &str,
        updated_at: i64,
    ) -> Result<Vec<ConversationArtifactRow>, DbError> {
        sqlx::query(
            "UPDATE conversation_artifacts \
             SET status = 'saved', updated_at = ? \
             WHERE kind = 'skill_suggest' AND cron_job_id = ? AND status != 'saved'",
        )
        .bind(updated_at)
        .bind(cron_job_id)
        .execute(&self.pool)
        .await?;

        let rows = sqlx::query_as::<_, ConversationArtifactRow>(
            "SELECT * FROM conversation_artifacts \
             WHERE kind = 'skill_suggest' AND cron_job_id = ? \
             ORDER BY created_at ASC, id ASC",
        )
        .bind(cron_job_id)
        .fetch_all(&self.pool)
        .await?;

        Ok(rows)
    }

    async fn delete_artifacts_by_conversation(&self, conversation_id: i64) -> Result<(), DbError> {
        sqlx::query("DELETE FROM conversation_artifacts WHERE conversation_id = ?")
            .bind(conversation_id)
            .execute(&self.pool)
            .await?;

        Ok(())
    }

    async fn list_legacy_cron_trigger_messages(&self, conversation_id: i64) -> Result<Vec<MessageRow>, DbError> {
        let rows = sqlx::query_as::<_, MessageRow>(
            "SELECT * FROM messages \
             WHERE conversation_id = ? AND type = 'cron_trigger' \
             ORDER BY created_at ASC, id ASC",
        )
        .bind(conversation_id)
        .fetch_all(&self.pool)
        .await?;

        Ok(rows)
    }

    // ── conversation_mcp_servers junction ───────────────────────────

    async fn list_mcp_server_ids(&self, conversation_id: i64) -> Result<Vec<i64>, DbError> {
        let ids = sqlx::query_scalar::<_, i64>(
            "SELECT mcp_server_id FROM conversation_mcp_servers \
             WHERE conversation_id = ? \
             ORDER BY sort_order ASC, mcp_server_id ASC",
        )
        .bind(conversation_id)
        .fetch_all(&self.pool)
        .await?;

        Ok(ids)
    }

    async fn set_mcp_server_ids(&self, conversation_id: i64, ids: &[i64]) -> Result<(), DbError> {
        let mut tx = self.pool.begin().await?;

        sqlx::query("DELETE FROM conversation_mcp_servers WHERE conversation_id = ?")
            .bind(conversation_id)
            .execute(&mut *tx)
            .await?;

        for (sort_order, mcp_server_id) in ids.iter().enumerate() {
            sqlx::query(
                "INSERT INTO conversation_mcp_servers (conversation_id, mcp_server_id, sort_order) \
                 VALUES (?, ?, ?)",
            )
            .bind(conversation_id)
            .bind(mcp_server_id)
            .bind(sort_order as i64)
            .execute(&mut *tx)
            .await?;
        }

        tx.commit().await?;
        Ok(())
    }
}

// ── Dynamic bind helpers ────────────────────────────────────────────

/// Appends shared filter conditions (source, cron_job_id, pinned) to WHERE
/// clause parts and bind values. Used by both `list_paginated` and the count
/// query to keep filter logic in one place.
fn append_filter_conditions(filters: &ConversationFilters, where_parts: &mut Vec<String>, binds: &mut Vec<BindValue>) {
    if let Some(ref source) = filters.source {
        where_parts.push("c.source = ?".to_string());
        binds.push(BindValue::Str(source.clone()));
    }
    if let Some(ref cron_job_id) = filters.cron_job_id {
        where_parts.push("c.cron_job_id = ?".to_string());
        binds.push(BindValue::Str(cron_job_id.clone()));
    }
    if let Some(pinned) = filters.pinned {
        where_parts.push("c.pinned = ?".to_string());
        binds.push(BindValue::Bool(pinned));
    }
    // Companion companion (work-partner) 单会话不计入普通会话列表/计数。
    // `extra.companionSession` 为 1 的行被排除;`IS NOT 1` 同时覆盖缺失/为 0
    // 的普通会话(json_extract 返回 NULL 时 `NULL IS NOT 1` 为真)。
    if filters.exclude_companion_companion {
        where_parts.push("json_extract(c.extra, '$.companionSession') IS NOT 1".to_string());
    }
    // 智能编排 (orchestrator) WORKER 会话由 Run 引擎为执行编队任务而创建,
    // 带有 `extra.orchestrator_task_id` 标记 (build_worker_extra 写入 run_id+task_id)。
    // 它们只属于编排 Run 视图,绝不出现在普通会话列表中。与 companion 不同,这一
    // 排除是无条件的:没有任何 ConversationService::list 的消费者需要看到 worker
    // 会话(编排器按 id 经 get/list_messages 读取各 worker)。键在 worker 专属的
    // `orchestrator_task_id`(而非 run_id):lead 会话(nomi_run_create 写回)只带
    // `orchestrator_run_id`、无 task_id,必须保持可见以承载 DAG 右栏。`IS NULL`
    // 同时覆盖缺失键的普通/lead 会话(json_extract 缺失时返回 NULL → 保留)。
    where_parts.push("json_extract(c.extra, '$.orchestrator_task_id') IS NULL".to_string());
}

/// Builds a count query and bind values for the total (ignoring cursor).
fn build_count_sql(user_id: &str, filters: &ConversationFilters) -> (String, Vec<BindValue>) {
    let mut where_parts = vec!["c.user_id = ?".to_string()];
    let mut binds: Vec<BindValue> = vec![BindValue::Str(user_id.to_string())];

    append_filter_conditions(filters, &mut where_parts, &mut binds);

    let sql = format!(
        "SELECT COUNT(*) FROM conversations c WHERE {}",
        where_parts.join(" AND ")
    );

    (sql, binds)
}

/// Executes a dynamic count query.
async fn execute_count(pool: &SqlitePool, sql: &str, binds: &[BindValue]) -> Result<u64, DbError> {
    let mut query = sqlx::query_as::<_, (i64,)>(sql);
    for bind in binds {
        query = bind_value_as(query, bind);
    }
    let row = query.fetch_one(pool).await?;
    Ok(row.0 as u64)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::init_database_memory;

    async fn setup() -> (SqliteConversationRepository, crate::Database) {
        let db = init_database_memory().await.unwrap();
        let repo = SqliteConversationRepository::new(db.pool().clone());
        (repo, db)
    }

    fn sample_conversation(user_id: &str) -> ConversationRow {
        let now = nomifun_common::now_ms();
        ConversationRow {
            // id is allocated by SQLite on create(); the value here is ignored.
            id: 0,
            user_id: user_id.to_string(),
            name: "Test Conversation".to_string(),
            r#type: "gemini".to_string(),
            extra: r#"{"workspace":"/home/user/project"}"#.to_string(),
            model: Some(r#"{"providerId":"prov_1","model":"claude-sonnet-4-20250514"}"#.to_string()),
            status: Some("pending".to_string()),
            source: Some("nomifun".to_string()),
            channel_chat_id: None,
            pinned: false,
            pinned_at: None,
            cron_job_id: None,
            created_at: now,
            updated_at: now,
        }
    }

    fn sample_message(conv_id: i64) -> MessageRow {
        let now = nomifun_common::now_ms();
        MessageRow {
            id: nomifun_common::generate_prefixed_id("msg"),
            conversation_id: conv_id,
            msg_id: Some("client_msg_1".to_string()),
            r#type: "text".to_string(),
            content: r#"{"content":"Hello world"}"#.to_string(),
            position: Some("right".to_string()),
            status: Some("finish".to_string()),
            hidden: false,
            created_at: now,
        }
    }

    const SYSTEM_USER_ID: &str = "system_default_user";

    /// Inserts a minimal valid `cron_jobs` row so conversations can reference it
    /// via the `cron_job_id` FK (foreign_keys is ON in the test pool).
    async fn insert_cron_job(pool: &SqlitePool, id: &str) {
        let now = nomifun_common::now_ms();
        sqlx::query(
            "INSERT INTO cron_jobs \
                (id, name, schedule_kind, schedule_value, payload_message, agent_type, created_by, created_at, updated_at) \
             VALUES (?, ?, 'every', '60', '', 'gemini', 'user', ?, ?)",
        )
        .bind(id)
        .bind(format!("job {id}"))
        .bind(now)
        .bind(now)
        .execute(pool)
        .await
        .unwrap();
    }

    /// Inserts a minimal valid `mcp_servers` row and returns its auto id so the
    /// junction can reference it via the `mcp_server_id` FK.
    async fn insert_mcp_server(pool: &SqlitePool, name: &str) -> i64 {
        let now = nomifun_common::now_ms();
        let result = sqlx::query(
            "INSERT INTO mcp_servers \
                (name, transport_type, transport_config, created_at, updated_at) \
             VALUES (?, 'stdio', '{}', ?, ?)",
        )
        .bind(name)
        .bind(now)
        .bind(now)
        .execute(pool)
        .await
        .unwrap();
        result.last_insert_rowid()
    }

    fn sample_artifact(conversation_id: i64, kind: &str, cron_job_id: Option<&str>) -> ConversationArtifactRow {
        let now = nomifun_common::now_ms();
        ConversationArtifactRow {
            // id is allocated by SQLite; the value here is ignored by upsert.
            id: 0,
            conversation_id,
            cron_job_id: cron_job_id.map(str::to_string),
            kind: kind.to_string(),
            status: "active".to_string(),
            payload: r#"{}"#.to_string(),
            created_at: now,
            updated_at: now,
        }
    }

    // ── Conversation CRUD tests ─────────────────────────────────────

    #[tokio::test]
    async fn create_and_get_conversation() {
        let (repo, _db) = setup().await;
        let mut conv = sample_conversation(SYSTEM_USER_ID);

        conv.id = repo.create(&conv).await.unwrap();
        assert!(conv.id > 0);
        let found = repo.get(conv.id).await.unwrap().unwrap();

        assert_eq!(found.id, conv.id);
        assert_eq!(found.name, "Test Conversation");
        assert_eq!(found.r#type, "gemini");
        assert_eq!(found.status.as_deref(), Some("pending"));
        assert!(!found.pinned);
    }

    #[tokio::test]
    async fn get_nonexistent_returns_none() {
        let (repo, _db) = setup().await;
        assert!(repo.get(999_999).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn update_conversation_name() {
        let (repo, _db) = setup().await;
        let mut conv = sample_conversation(SYSTEM_USER_ID);
        conv.id = repo.create(&conv).await.unwrap();

        let now = nomifun_common::now_ms();
        repo.update(
            conv.id,
            &ConversationRowUpdate {
                name: Some("Updated Name".to_string()),
                updated_at: Some(now),
                ..Default::default()
            },
        )
        .await
        .unwrap();

        let found = repo.get(conv.id).await.unwrap().unwrap();
        assert_eq!(found.name, "Updated Name");
        assert!(found.updated_at >= conv.updated_at);
    }

    #[tokio::test]
    async fn update_conversation_pinned() {
        let (repo, _db) = setup().await;
        let mut conv = sample_conversation(SYSTEM_USER_ID);
        conv.id = repo.create(&conv).await.unwrap();

        let pin_time = nomifun_common::now_ms();
        repo.update(
            conv.id,
            &ConversationRowUpdate {
                pinned: Some(true),
                pinned_at: Some(Some(pin_time)),
                updated_at: Some(pin_time),
                ..Default::default()
            },
        )
        .await
        .unwrap();

        let found = repo.get(conv.id).await.unwrap().unwrap();
        assert!(found.pinned);
        assert_eq!(found.pinned_at, Some(pin_time));
    }

    #[tokio::test]
    async fn update_nonexistent_returns_not_found() {
        let (repo, _db) = setup().await;
        let err = repo
            .update(
                999_999,
                &ConversationRowUpdate {
                    name: Some("x".to_string()),
                    ..Default::default()
                },
            )
            .await
            .unwrap_err();
        assert!(matches!(err, DbError::NotFound(_)));
    }

    #[tokio::test]
    async fn update_empty_is_noop() {
        let (repo, _db) = setup().await;
        let mut conv = sample_conversation(SYSTEM_USER_ID);
        conv.id = repo.create(&conv).await.unwrap();

        // Empty update should succeed without error
        repo.update(conv.id, &ConversationRowUpdate::default()).await.unwrap();
    }

    #[tokio::test]
    async fn delete_conversation() {
        let (repo, _db) = setup().await;
        let mut conv = sample_conversation(SYSTEM_USER_ID);
        conv.id = repo.create(&conv).await.unwrap();

        repo.delete(conv.id).await.unwrap();
        assert!(repo.get(conv.id).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn delete_cascades_messages() {
        let (repo, _db) = setup().await;
        let mut conv = sample_conversation(SYSTEM_USER_ID);
        conv.id = repo.create(&conv).await.unwrap();

        let msg = sample_message(conv.id);
        repo.insert_message(&msg).await.unwrap();

        repo.delete(conv.id).await.unwrap();

        // Messages should be gone due to CASCADE
        let result = repo.get_messages(conv.id, 1, 50, SortOrder::Desc).await.unwrap();
        assert!(result.items.is_empty());
    }

    #[tokio::test]
    async fn delete_nonexistent_returns_not_found() {
        let (repo, _db) = setup().await;
        let err = repo.delete(999_999).await.unwrap_err();
        assert!(matches!(err, DbError::NotFound(_)));
    }

    // ── Pagination tests ────────────────────────────────────────────

    #[tokio::test]
    async fn list_empty() {
        let (repo, _db) = setup().await;
        let result = repo
            .list_paginated(
                SYSTEM_USER_ID,
                &ConversationFilters {
                    limit: 20,
                    ..Default::default()
                },
            )
            .await
            .unwrap();

        assert!(result.items.is_empty());
        assert_eq!(result.total, 0);
        assert!(!result.has_more);
    }

    #[tokio::test]
    async fn list_ordered_by_updated_at_desc() {
        let (repo, _db) = setup().await;

        let mut c1 = sample_conversation(SYSTEM_USER_ID);
        c1.name = "First".to_string();
        c1.updated_at = 1000;
        repo.create(&c1).await.unwrap();

        let mut c2 = sample_conversation(SYSTEM_USER_ID);
        c2.name = "Second".to_string();
        c2.updated_at = 2000;
        repo.create(&c2).await.unwrap();

        let mut c3 = sample_conversation(SYSTEM_USER_ID);
        c3.name = "Third".to_string();
        c3.updated_at = 3000;
        repo.create(&c3).await.unwrap();

        let result = repo
            .list_paginated(
                SYSTEM_USER_ID,
                &ConversationFilters {
                    limit: 20,
                    ..Default::default()
                },
            )
            .await
            .unwrap();

        assert_eq!(result.items.len(), 3);
        assert_eq!(result.total, 3);
        assert_eq!(result.items[0].name, "Third");
        assert_eq!(result.items[1].name, "Second");
        assert_eq!(result.items[2].name, "First");
    }

    #[tokio::test]
    async fn list_cursor_pagination() {
        let (repo, _db) = setup().await;

        let mut convs = Vec::new();
        for i in 0..5 {
            let mut c = sample_conversation(SYSTEM_USER_ID);
            c.name = format!("Conv {i}");
            c.updated_at = (i + 1) as i64 * 1000;
            repo.create(&c).await.unwrap();
            convs.push(c);
        }

        // Page 1: limit 2 → items[4,3], hasMore=true
        let page1 = repo
            .list_paginated(
                SYSTEM_USER_ID,
                &ConversationFilters {
                    limit: 2,
                    ..Default::default()
                },
            )
            .await
            .unwrap();

        assert_eq!(page1.items.len(), 2);
        assert!(page1.has_more);
        assert_eq!(page1.items[0].name, "Conv 4");
        assert_eq!(page1.items[1].name, "Conv 3");

        // Page 2: cursor = last item of page 1
        let cursor = page1.items.last().unwrap().id;
        let page2 = repo
            .list_paginated(
                SYSTEM_USER_ID,
                &ConversationFilters {
                    cursor: Some(cursor),
                    limit: 2,
                    ..Default::default()
                },
            )
            .await
            .unwrap();

        assert_eq!(page2.items.len(), 2);
        assert!(page2.has_more);
        assert_eq!(page2.items[0].name, "Conv 2");
        assert_eq!(page2.items[1].name, "Conv 1");

        // Page 3: cursor = last item of page 2
        let cursor = page2.items.last().unwrap().id;
        let page3 = repo
            .list_paginated(
                SYSTEM_USER_ID,
                &ConversationFilters {
                    cursor: Some(cursor),
                    limit: 2,
                    ..Default::default()
                },
            )
            .await
            .unwrap();

        assert_eq!(page3.items.len(), 1);
        assert!(!page3.has_more);
        assert_eq!(page3.items[0].name, "Conv 0");
    }

    #[tokio::test]
    async fn list_filter_by_source() {
        let (repo, _db) = setup().await;

        let mut c1 = sample_conversation(SYSTEM_USER_ID);
        c1.source = Some("nomifun".to_string());
        repo.create(&c1).await.unwrap();

        let mut c2 = sample_conversation(SYSTEM_USER_ID);
        c2.source = Some("telegram".to_string());
        repo.create(&c2).await.unwrap();

        let result = repo
            .list_paginated(
                SYSTEM_USER_ID,
                &ConversationFilters {
                    source: Some("telegram".to_string()),
                    limit: 20,
                    ..Default::default()
                },
            )
            .await
            .unwrap();

        assert_eq!(result.items.len(), 1);
        assert_eq!(result.total, 1);
        assert_eq!(result.items[0].source.as_deref(), Some("telegram"));
    }

    #[tokio::test]
    async fn list_filter_by_cron_job_id() {
        let (repo, _db) = setup().await;

        insert_cron_job(&repo.pool, "cron_abc").await;

        let mut c1 = sample_conversation(SYSTEM_USER_ID);
        c1.cron_job_id = Some("cron_abc".to_string());
        c1.id = repo.create(&c1).await.unwrap();

        let c2 = sample_conversation(SYSTEM_USER_ID);
        repo.create(&c2).await.unwrap();

        let result = repo
            .list_paginated(
                SYSTEM_USER_ID,
                &ConversationFilters {
                    cron_job_id: Some("cron_abc".to_string()),
                    limit: 20,
                    ..Default::default()
                },
            )
            .await
            .unwrap();

        assert_eq!(result.items.len(), 1);
        assert_eq!(result.total, 1);
        assert_eq!(result.items[0].id, c1.id);
    }

    #[tokio::test]
    async fn list_filter_by_pinned() {
        let (repo, _db) = setup().await;

        let mut c1 = sample_conversation(SYSTEM_USER_ID);
        c1.pinned = true;
        c1.pinned_at = Some(nomifun_common::now_ms());
        repo.create(&c1).await.unwrap();

        let mut c2 = sample_conversation(SYSTEM_USER_ID);
        c2.pinned = false;
        repo.create(&c2).await.unwrap();

        let result = repo
            .list_paginated(
                SYSTEM_USER_ID,
                &ConversationFilters {
                    pinned: Some(true),
                    limit: 20,
                    ..Default::default()
                },
            )
            .await
            .unwrap();

        assert_eq!(result.items.len(), 1);
        assert_eq!(result.total, 1);
        assert!(result.items[0].pinned);
    }

    /// 智能编排 worker 会话 (extra.orchestrator_run_id 非空) 无条件从普通列表排除;
    /// 不带该标记的普通会话保留。镜像 companion 排除机制 (json_extract WHERE)。
    #[tokio::test]
    async fn list_excludes_orchestrator_worker_conversations() {
        let (repo, _db) = setup().await;

        // Worker conversation — stamped with orchestrator_run_id (build_worker_extra).
        let mut worker = sample_conversation(SYSTEM_USER_ID);
        worker.extra = r#"{"orchestrator_run_id":"run_abc","orchestrator_task_id":"task_1"}"#.to_string();
        let worker_id = repo.create(&worker).await.unwrap();

        // Normal conversation — no orchestrator marker.
        let mut normal = sample_conversation(SYSTEM_USER_ID);
        normal.extra = r#"{"workspace":"/project"}"#.to_string();
        let normal_id = repo.create(&normal).await.unwrap();

        let result = repo
            .list_paginated(
                SYSTEM_USER_ID,
                &ConversationFilters {
                    limit: 20,
                    ..Default::default()
                },
            )
            .await
            .unwrap();

        // Only the normal conversation is returned; the worker is filtered from
        // both the page and the total count.
        assert_eq!(result.items.len(), 1);
        assert_eq!(result.total, 1);
        assert_eq!(result.items[0].id, normal_id);
        assert!(result.items.iter().all(|c| c.id != worker_id));
    }

    /// LEAD 会话与 WORKER 会话的区分:lead (P1 `nomi_run_create` 写回) 的 extra
    /// 只带 `orchestrator_run_id`,worker (`build_worker_extra`) 同时带
    /// `orchestrator_run_id` + `orchestrator_task_id`。隐藏过滤器键在 worker 专属的
    /// `orchestrator_task_id`,因此 lead 必须保持可见(它是承载 DAG 右栏的主会话),
    /// 只有 worker 被排除。普通会话(无任何标记)同样保留。
    #[tokio::test]
    async fn list_keeps_lead_and_excludes_only_worker() {
        let (repo, _db) = setup().await;

        // (a) Plain conversation — no orchestrator extra at all.
        let mut plain = sample_conversation(SYSTEM_USER_ID);
        plain.extra = r#"{"workspace":"/project"}"#.to_string();
        let plain_id = repo.create(&plain).await.unwrap();

        // (b) LEAD conversation — only orchestrator_run_id (nomi_run_create write-back),
        //     NO orchestrator_task_id. Must stay visible.
        let mut lead = sample_conversation(SYSTEM_USER_ID);
        lead.extra = r#"{"orchestrator_run_id":"run_lead"}"#.to_string();
        let lead_id = repo.create(&lead).await.unwrap();

        // (c) WORKER conversation — both run_id AND task_id (build_worker_extra).
        //     Must be excluded.
        let mut worker = sample_conversation(SYSTEM_USER_ID);
        worker.extra = r#"{"orchestrator_run_id":"run_lead","orchestrator_task_id":"task_1"}"#.to_string();
        let worker_id = repo.create(&worker).await.unwrap();

        let result = repo
            .list_paginated(
                SYSTEM_USER_ID,
                &ConversationFilters {
                    limit: 20,
                    ..Default::default()
                },
            )
            .await
            .unwrap();

        // Plain + lead are returned (count 2); the worker is filtered from both
        // the page and the total count.
        assert_eq!(result.total, 2);
        assert_eq!(result.items.len(), 2);
        assert!(
            result.items.iter().any(|c| c.id == plain_id),
            "plain conversation must remain visible"
        );
        assert!(
            result.items.iter().any(|c| c.id == lead_id),
            "lead conversation must remain visible (hosts the DAG rail)"
        );
        assert!(
            result.items.iter().all(|c| c.id != worker_id),
            "worker conversation must be excluded"
        );
    }

    // ── Extended query tests ────────────────────────────────────────
    #[tokio::test]
    async fn find_by_source_and_chat() {
        let (repo, _db) = setup().await;

        let mut c = sample_conversation(SYSTEM_USER_ID);
        c.source = Some("telegram".to_string());
        c.channel_chat_id = Some("user:123".to_string());
        c.r#type = "gemini".to_string();
        c.id = repo.create(&c).await.unwrap();

        let found = repo
            .find_by_source_and_chat(SYSTEM_USER_ID, "telegram", "user:123", "gemini")
            .await
            .unwrap();
        assert!(found.is_some());
        assert_eq!(found.unwrap().id, c.id);

        // Different chat ID → not found
        let not_found = repo
            .find_by_source_and_chat(SYSTEM_USER_ID, "telegram", "user:999", "gemini")
            .await
            .unwrap();
        assert!(not_found.is_none());
    }

    #[tokio::test]
    async fn list_by_cron_job() {
        let (repo, _db) = setup().await;

        insert_cron_job(&repo.pool, "cron_1").await;
        insert_cron_job(&repo.pool, "cron_2").await;

        let mut c1 = sample_conversation(SYSTEM_USER_ID);
        c1.cron_job_id = Some("cron_1".to_string());
        repo.create(&c1).await.unwrap();

        let mut c2 = sample_conversation(SYSTEM_USER_ID);
        c2.cron_job_id = Some("cron_1".to_string());
        repo.create(&c2).await.unwrap();

        let mut c3 = sample_conversation(SYSTEM_USER_ID);
        c3.cron_job_id = Some("cron_2".to_string());
        repo.create(&c3).await.unwrap();

        let result = repo.list_by_cron_job(SYSTEM_USER_ID, "cron_1").await.unwrap();
        assert_eq!(result.len(), 2);
    }

    #[tokio::test]
    async fn list_associated_by_workspace() {
        let (repo, _db) = setup().await;

        let mut c1 = sample_conversation(SYSTEM_USER_ID);
        c1.extra = r#"{"workspace":"/shared/project"}"#.to_string();
        c1.id = repo.create(&c1).await.unwrap();

        let mut c2 = sample_conversation(SYSTEM_USER_ID);
        c2.extra = r#"{"workspace":"/shared/project"}"#.to_string();
        c2.id = repo.create(&c2).await.unwrap();

        let mut c3 = sample_conversation(SYSTEM_USER_ID);
        c3.extra = r#"{"workspace":"/other/project"}"#.to_string();
        repo.create(&c3).await.unwrap();

        let associated = repo.list_associated(SYSTEM_USER_ID, c1.id).await.unwrap();
        assert_eq!(associated.len(), 1);
        assert_eq!(associated[0].id, c2.id);
    }

    #[tokio::test]
    async fn list_associated_no_workspace() {
        let (repo, _db) = setup().await;

        let mut c = sample_conversation(SYSTEM_USER_ID);
        c.extra = r#"{}"#.to_string();
        c.id = repo.create(&c).await.unwrap();

        let associated = repo.list_associated(SYSTEM_USER_ID, c.id).await.unwrap();
        assert!(associated.is_empty());
    }

    #[tokio::test]
    async fn list_associated_not_found() {
        let (repo, _db) = setup().await;
        let err = repo.list_associated(SYSTEM_USER_ID, 999_999).await.unwrap_err();
        assert!(matches!(err, DbError::NotFound(_)));
    }

    #[tokio::test]
    async fn cron_job_id_roundtrips_as_column() {
        let (repo, _db) = setup().await;

        insert_cron_job(&repo.pool, "cron_x").await;

        let mut conv = sample_conversation(SYSTEM_USER_ID);
        conv.cron_job_id = Some("cron_x".to_string());
        conv.id = repo.create(&conv).await.unwrap();

        let found = repo.get(conv.id).await.unwrap().unwrap();
        assert_eq!(found.cron_job_id.as_deref(), Some("cron_x"));

        // Clearing via update sets the column to NULL.
        repo.update(
            conv.id,
            &ConversationRowUpdate {
                cron_job_id: Some(None),
                updated_at: Some(nomifun_common::now_ms()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        let cleared = repo.get(conv.id).await.unwrap().unwrap();
        assert_eq!(cleared.cron_job_id, None);
    }

    // ── Artifact tests ──────────────────────────────────────────────

    #[tokio::test]
    async fn cron_trigger_artifacts_insert_distinct_rows() {
        let (repo, _db) = setup().await;
        let mut conv = sample_conversation(SYSTEM_USER_ID);
        conv.id = repo.create(&conv).await.unwrap();
        insert_cron_job(&repo.pool, "cron_t").await;

        // cron_trigger has no unique constraint: each upsert is a fresh row with
        // a distinct auto-assigned i64 id.
        let a1 = repo
            .upsert_artifact(&sample_artifact(conv.id, "cron_trigger", Some("cron_t")))
            .await
            .unwrap();
        let a2 = repo
            .upsert_artifact(&sample_artifact(conv.id, "cron_trigger", Some("cron_t")))
            .await
            .unwrap();

        assert!(a1.id > 0);
        assert!(a2.id > 0);
        assert_ne!(a1.id, a2.id);

        let listed = repo.list_artifacts(conv.id).await.unwrap();
        assert_eq!(listed.len(), 2);
    }

    #[tokio::test]
    async fn skill_suggest_artifacts_upsert_is_idempotent() {
        let (repo, _db) = setup().await;
        let mut conv = sample_conversation(SYSTEM_USER_ID);
        conv.id = repo.create(&conv).await.unwrap();
        insert_cron_job(&repo.pool, "cron_s").await;

        let first = repo
            .upsert_artifact(&sample_artifact(conv.id, "skill_suggest", Some("cron_s")))
            .await
            .unwrap();

        // Second upsert for the same (conversation_id, cron_job_id) collides on the
        // partial UNIQUE index → updates in place, keeping the same id.
        let mut updated_input = sample_artifact(conv.id, "skill_suggest", Some("cron_s"));
        updated_input.payload = r#"{"v":2}"#.to_string();
        let second = repo.upsert_artifact(&updated_input).await.unwrap();

        assert_eq!(first.id, second.id);
        assert_eq!(second.payload, r#"{"v":2}"#);

        let listed = repo.list_artifacts(conv.id).await.unwrap();
        assert_eq!(listed.len(), 1);
    }

    #[tokio::test]
    async fn get_and_update_artifact_status_by_i64_id() {
        let (repo, _db) = setup().await;
        let mut conv = sample_conversation(SYSTEM_USER_ID);
        conv.id = repo.create(&conv).await.unwrap();
        insert_cron_job(&repo.pool, "cron_u").await;

        let inserted = repo
            .upsert_artifact(&sample_artifact(conv.id, "cron_trigger", Some("cron_u")))
            .await
            .unwrap();

        let fetched = repo.get_artifact(conv.id, inserted.id).await.unwrap().unwrap();
        assert_eq!(fetched.id, inserted.id);

        let updated = repo
            .update_artifact_status(conv.id, inserted.id, "dismissed", nomifun_common::now_ms())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(updated.status, "dismissed");

        // Missing id → None.
        let missing = repo
            .update_artifact_status(conv.id, 999_999, "dismissed", nomifun_common::now_ms())
            .await
            .unwrap();
        assert!(missing.is_none());
    }

    // ── conversation_mcp_servers junction tests ─────────────────────

    #[tokio::test]
    async fn set_and_list_mcp_server_ids_preserves_order() {
        let (repo, _db) = setup().await;
        let mut conv = sample_conversation(SYSTEM_USER_ID);
        conv.id = repo.create(&conv).await.unwrap();

        let a = insert_mcp_server(&repo.pool, "srv_a").await;
        let b = insert_mcp_server(&repo.pool, "srv_b").await;
        let c = insert_mcp_server(&repo.pool, "srv_c").await;

        // Empty by default.
        assert!(repo.list_mcp_server_ids(conv.id).await.unwrap().is_empty());

        // Order is preserved via sort_order, not numeric id order.
        repo.set_mcp_server_ids(conv.id, &[c, a, b]).await.unwrap();
        assert_eq!(repo.list_mcp_server_ids(conv.id).await.unwrap(), vec![c, a, b]);

        // set replaces the whole set (DELETE + ordered INSERT).
        repo.set_mcp_server_ids(conv.id, &[b]).await.unwrap();
        assert_eq!(repo.list_mcp_server_ids(conv.id).await.unwrap(), vec![b]);

        // Empty slice clears the selection.
        repo.set_mcp_server_ids(conv.id, &[]).await.unwrap();
        assert!(repo.list_mcp_server_ids(conv.id).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn deleting_conversation_cascades_mcp_junction() {
        let (repo, _db) = setup().await;
        let mut conv = sample_conversation(SYSTEM_USER_ID);
        conv.id = repo.create(&conv).await.unwrap();
        let a = insert_mcp_server(&repo.pool, "srv_cascade").await;

        repo.set_mcp_server_ids(conv.id, &[a]).await.unwrap();
        repo.delete(conv.id).await.unwrap();

        // Junction rows are removed via ON DELETE CASCADE.
        let remaining = repo.list_mcp_server_ids(conv.id).await.unwrap();
        assert!(remaining.is_empty());
    }

    // ── Message tests ───────────────────────────────────────────────

    #[tokio::test]
    async fn insert_and_get_messages() {
        let (repo, _db) = setup().await;
        let mut conv = sample_conversation(SYSTEM_USER_ID);
        conv.id = repo.create(&conv).await.unwrap();

        let msg = sample_message(conv.id);
        repo.insert_message(&msg).await.unwrap();

        let result = repo.get_messages(conv.id, 1, 50, SortOrder::Desc).await.unwrap();
        assert_eq!(result.items.len(), 1);
        assert_eq!(result.total, 1);
        assert_eq!(result.items[0].id, msg.id);
    }

    #[tokio::test]
    async fn get_messages_pagination() {
        let (repo, _db) = setup().await;
        let mut conv = sample_conversation(SYSTEM_USER_ID);
        conv.id = repo.create(&conv).await.unwrap();

        for i in 0..10 {
            let mut msg = sample_message(conv.id);
            msg.id = nomifun_common::generate_prefixed_id("msg");
            msg.created_at = (i + 1) * 1000;
            repo.insert_message(&msg).await.unwrap();
        }

        let page1 = repo.get_messages(conv.id, 1, 3, SortOrder::Desc).await.unwrap();
        assert_eq!(page1.items.len(), 3);
        assert_eq!(page1.total, 10);
        assert!(page1.has_more);
        // DESC: most recent first
        assert!(page1.items[0].created_at > page1.items[1].created_at);
    }

    #[tokio::test]
    async fn get_messages_asc_order() {
        let (repo, _db) = setup().await;
        let mut conv = sample_conversation(SYSTEM_USER_ID);
        conv.id = repo.create(&conv).await.unwrap();

        for i in 0..3 {
            let mut msg = sample_message(conv.id);
            msg.id = nomifun_common::generate_prefixed_id("msg");
            msg.created_at = (i + 1) * 1000;
            repo.insert_message(&msg).await.unwrap();
        }

        let result = repo.get_messages(conv.id, 1, 50, SortOrder::Asc).await.unwrap();
        assert!(result.items[0].created_at < result.items[1].created_at);
    }

    #[tokio::test]
    async fn update_message_content() {
        let (repo, _db) = setup().await;
        let mut conv = sample_conversation(SYSTEM_USER_ID);
        conv.id = repo.create(&conv).await.unwrap();

        let msg = sample_message(conv.id);
        repo.insert_message(&msg).await.unwrap();

        repo.update_message(
            &msg.id,
            &MessageRowUpdate {
                content: Some(r#"{"content":"Updated"}"#.to_string()),
                ..Default::default()
            },
        )
        .await
        .unwrap();

        let result = repo.get_messages(conv.id, 1, 50, SortOrder::Desc).await.unwrap();
        assert_eq!(result.items[0].content, r#"{"content":"Updated"}"#);
    }

    #[tokio::test]
    async fn update_message_not_found() {
        let (repo, _db) = setup().await;
        let err = repo
            .update_message(
                "no_id",
                &MessageRowUpdate {
                    hidden: Some(true),
                    ..Default::default()
                },
            )
            .await
            .unwrap_err();
        assert!(matches!(err, DbError::NotFound(_)));
    }

    #[tokio::test]
    async fn delete_messages_by_conversation() {
        let (repo, _db) = setup().await;
        let mut conv = sample_conversation(SYSTEM_USER_ID);
        conv.id = repo.create(&conv).await.unwrap();

        for _ in 0..3 {
            let mut msg = sample_message(conv.id);
            msg.id = nomifun_common::generate_prefixed_id("msg");
            repo.insert_message(&msg).await.unwrap();
        }

        repo.delete_messages_by_conversation(conv.id).await.unwrap();

        let result = repo.get_messages(conv.id, 1, 50, SortOrder::Desc).await.unwrap();
        assert!(result.items.is_empty());
        assert_eq!(result.total, 0);
    }

    #[tokio::test]
    async fn get_message_by_msg_id() {
        let (repo, _db) = setup().await;
        let mut conv = sample_conversation(SYSTEM_USER_ID);
        conv.id = repo.create(&conv).await.unwrap();

        let msg = sample_message(conv.id);
        repo.insert_message(&msg).await.unwrap();

        let found = repo
            .get_message_by_msg_id(conv.id, "client_msg_1", "text")
            .await
            .unwrap();
        assert!(found.is_some());
        assert_eq!(found.unwrap().id, msg.id);

        // Wrong type → not found
        let not_found = repo
            .get_message_by_msg_id(conv.id, "client_msg_1", "tips")
            .await
            .unwrap();
        assert!(not_found.is_none());
    }

    #[tokio::test]
    async fn search_messages_by_keyword() {
        let (repo, _db) = setup().await;
        let mut conv = sample_conversation(SYSTEM_USER_ID);
        conv.id = repo.create(&conv).await.unwrap();

        let mut msg1 = sample_message(conv.id);
        msg1.content = r#"{"content":"Rust 审查报告"}"#.to_string();
        repo.insert_message(&msg1).await.unwrap();

        let mut msg2 = sample_message(conv.id);
        msg2.id = nomifun_common::generate_prefixed_id("msg");
        msg2.content = r#"{"content":"Python 测试"}"#.to_string();
        repo.insert_message(&msg2).await.unwrap();

        let result = repo.search_messages(SYSTEM_USER_ID, "审查", 1, 20).await.unwrap();
        assert_eq!(result.items.len(), 1);
        assert_eq!(result.total, 1);
        assert_eq!(result.items[0].conversation_name, "Test Conversation");
    }

    #[tokio::test]
    async fn search_messages_no_match() {
        let (repo, _db) = setup().await;
        let mut conv = sample_conversation(SYSTEM_USER_ID);
        conv.id = repo.create(&conv).await.unwrap();

        let msg = sample_message(conv.id);
        repo.insert_message(&msg).await.unwrap();

        let result = repo
            .search_messages(SYSTEM_USER_ID, "xxxxnotexist", 1, 20)
            .await
            .unwrap();
        assert!(result.items.is_empty());
        assert_eq!(result.total, 0);
    }

    #[tokio::test]
    async fn search_messages_pagination() {
        let (repo, _db) = setup().await;
        let mut conv = sample_conversation(SYSTEM_USER_ID);
        conv.id = repo.create(&conv).await.unwrap();

        for i in 0..5 {
            let mut msg = sample_message(conv.id);
            msg.id = nomifun_common::generate_prefixed_id("msg");
            msg.content = format!(r#"{{"content":"match keyword item {i}"}}"#);
            msg.created_at = (i + 1) * 1000;
            repo.insert_message(&msg).await.unwrap();
        }

        let result = repo.search_messages(SYSTEM_USER_ID, "keyword", 1, 2).await.unwrap();
        assert_eq!(result.items.len(), 2);
        assert_eq!(result.total, 5);
        assert!(result.has_more);
    }

    // ── Sort order tests ────────────────────────────────────────────

    #[test]
    fn sort_order_sql_representation() {
        assert_eq!(SortOrder::Asc.as_sql(), "ASC");
        assert_eq!(SortOrder::Desc.as_sql(), "DESC");
    }

    #[test]
    fn default_sort_order_is_asc() {
        assert_eq!(SortOrder::default(), SortOrder::Asc);
    }

    // ── Filters tests ───────────────────────────────────────────────

    #[test]
    fn effective_limit_default() {
        let f = ConversationFilters::default();
        assert_eq!(f.effective_limit(), 20);
    }

    #[test]
    fn effective_limit_custom() {
        let f = ConversationFilters {
            limit: 50,
            ..Default::default()
        };
        assert_eq!(f.effective_limit(), 50);
    }

    #[tokio::test]
    async fn delete_messages_from_removes_cursor_and_newer() {
        let (repo, _db) = setup().await;
        let mut conv = sample_conversation(SYSTEM_USER_ID);
        conv.id = repo.create(&conv).await.unwrap();

        let mk = |id: &str, created_at: i64| MessageRow {
            id: id.to_string(),
            conversation_id: conv.id,
            msg_id: Some(id.to_string()),
            r#type: "text".to_string(),
            content: r#"{"content":"x"}"#.to_string(),
            position: Some("right".to_string()),
            status: Some("finish".to_string()),
            hidden: false,
            created_at,
        };
        // 三条：t=100,200,300
        repo.insert_message(&mk("m1", 100)).await.unwrap();
        repo.insert_message(&mk("m2", 200)).await.unwrap();
        repo.insert_message(&mk("m3", 300)).await.unwrap();

        // 从 m2 (t=200) 起（含）删除 → 删 m2、m3，留 m1
        let deleted = repo.delete_messages_from(conv.id, 200, "m2").await.unwrap();
        assert_eq!(deleted, 2);

        assert!(repo.get_message(conv.id, "m1").await.unwrap().is_some());
        assert!(repo.get_message(conv.id, "m2").await.unwrap().is_none());
        assert!(repo.get_message(conv.id, "m3").await.unwrap().is_none());
    }
}

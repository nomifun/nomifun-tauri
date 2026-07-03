use sqlx::SqlitePool;

use crate::error::DbError;
use crate::models::{AssistantSessionRow, AssistantUserRow, ChannelPluginRow, PairingCodeRow};
use crate::repository::channel::{IChannelRepository, UpdatePluginStatusParams};

/// SQLite-backed implementation of [`IChannelRepository`].
#[derive(Clone, Debug)]
pub struct SqliteChannelRepository {
    pool: SqlitePool,
}

impl SqliteChannelRepository {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }
}

#[async_trait::async_trait]
impl IChannelRepository for SqliteChannelRepository {
    // ── Plugin CRUD ──────────────────────────────────────────────────

    async fn get_all_plugins(&self) -> Result<Vec<ChannelPluginRow>, DbError> {
        let rows = sqlx::query_as::<_, ChannelPluginRow>("SELECT * FROM assistant_plugins ORDER BY created_at ASC")
            .fetch_all(&self.pool)
            .await?;
        Ok(rows)
    }

    async fn get_plugin(&self, id: &str) -> Result<Option<ChannelPluginRow>, DbError> {
        let row = sqlx::query_as::<_, ChannelPluginRow>("SELECT * FROM assistant_plugins WHERE id = ?")
            .bind(id)
            .fetch_optional(&self.pool)
            .await?;
        Ok(row)
    }

    async fn upsert_plugin(&self, row: &ChannelPluginRow) -> Result<(), DbError> {
        sqlx::query(
            "INSERT INTO assistant_plugins \
                (id, type, name, enabled, config, status, last_connected, companion_id, public_agent_id, bot_key, created_at, updated_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?) \
             ON CONFLICT(id) DO UPDATE SET \
                type = excluded.type, \
                name = excluded.name, \
                enabled = excluded.enabled, \
                config = excluded.config, \
                status = excluded.status, \
                last_connected = excluded.last_connected, \
                companion_id = excluded.companion_id, \
                public_agent_id = excluded.public_agent_id, \
                bot_key = excluded.bot_key, \
                updated_at = excluded.updated_at",
        )
        .bind(&row.id)
        .bind(&row.r#type)
        .bind(&row.name)
        .bind(row.enabled)
        .bind(&row.config)
        .bind(&row.status)
        .bind(row.last_connected)
        .bind(&row.companion_id)
        .bind(&row.public_agent_id)
        .bind(&row.bot_key)
        .bind(row.created_at)
        .bind(row.updated_at)
        .execute(&self.pool)
        .await
        .map_err(|e| {
            if is_unique_violation(&e) {
                DbError::Conflict(format!(
                    "Bot '{}' on platform '{}' is already connected",
                    row.bot_key.as_deref().unwrap_or("?"),
                    row.r#type
                ))
            } else {
                DbError::Query(e)
            }
        })?;
        Ok(())
    }

    async fn update_plugin_status(&self, id: &str, params: &UpdatePluginStatusParams) -> Result<(), DbError> {
        let mut set_clauses = Vec::new();
        if params.status.is_some() {
            set_clauses.push("status = ?");
        }
        if params.last_connected.is_some() {
            set_clauses.push("last_connected = ?");
        }
        if params.enabled.is_some() {
            set_clauses.push("enabled = ?");
        }

        if set_clauses.is_empty() {
            return Ok(());
        }

        set_clauses.push("updated_at = ?");
        let sql = format!("UPDATE assistant_plugins SET {} WHERE id = ?", set_clauses.join(", "));

        let now = nomifun_common::now_ms();
        let mut query = sqlx::query(&sql);

        if let Some(ref status) = params.status {
            query = query.bind(status);
        }
        if let Some(last_connected) = params.last_connected {
            query = query.bind(last_connected);
        }
        if let Some(enabled) = params.enabled {
            query = query.bind(enabled);
        }
        query = query.bind(now);
        query = query.bind(id);

        let result = query.execute(&self.pool).await?;
        if result.rows_affected() == 0 {
            return Err(DbError::NotFound(format!("Plugin '{id}' not found")));
        }
        Ok(())
    }

    async fn update_plugin_companion(&self, id: &str, companion_id: Option<&str>) -> Result<(), DbError> {
        // Row-level mutual exclusivity: binding a companion (non-null) clears any
        // public-agent binding on the same row. Clearing (`None`) leaves the
        // public-agent binding untouched.
        let result = sqlx::query(
            "UPDATE assistant_plugins \
             SET companion_id = ?, \
                 public_agent_id = CASE WHEN ? IS NOT NULL THEN NULL ELSE public_agent_id END, \
                 updated_at = ? \
             WHERE id = ?",
        )
        .bind(companion_id)
        .bind(companion_id)
        .bind(nomifun_common::now_ms())
        .bind(id)
        .execute(&self.pool)
        .await?;
        if result.rows_affected() == 0 {
            return Err(DbError::NotFound(format!("Plugin '{id}' not found")));
        }
        Ok(())
    }

    async fn update_plugin_public_agent(&self, id: &str, public_agent_id: Option<&str>) -> Result<(), DbError> {
        // Row-level mutual exclusivity: binding a public agent (non-null) clears
        // any companion binding on the same row. Clearing (`None`) leaves the
        // companion binding untouched.
        let result = sqlx::query(
            "UPDATE assistant_plugins \
             SET public_agent_id = ?, \
                 companion_id = CASE WHEN ? IS NOT NULL THEN NULL ELSE companion_id END, \
                 updated_at = ? \
             WHERE id = ?",
        )
        .bind(public_agent_id)
        .bind(public_agent_id)
        .bind(nomifun_common::now_ms())
        .bind(id)
        .execute(&self.pool)
        .await?;
        if result.rows_affected() == 0 {
            return Err(DbError::NotFound(format!("Plugin '{id}' not found")));
        }
        Ok(())
    }

    async fn update_plugin_bot_key(&self, id: &str, bot_key: &str) -> Result<(), DbError> {
        let result = sqlx::query("UPDATE assistant_plugins SET bot_key = ?, updated_at = ? WHERE id = ?")
            .bind(bot_key)
            .bind(nomifun_common::now_ms())
            .bind(id)
            .execute(&self.pool)
            .await
            .map_err(|e| {
                if is_unique_violation(&e) {
                    DbError::Conflict(format!("Bot '{bot_key}' is already connected"))
                } else {
                    DbError::Query(e)
                }
            })?;
        if result.rows_affected() == 0 {
            return Err(DbError::NotFound(format!("Plugin '{id}' not found")));
        }
        Ok(())
    }

    async fn delete_plugin(&self, id: &str) -> Result<(), DbError> {
        let result = sqlx::query("DELETE FROM assistant_plugins WHERE id = ?")
            .bind(id)
            .execute(&self.pool)
            .await?;
        if result.rows_affected() == 0 {
            return Err(DbError::NotFound(format!("Plugin '{id}' not found")));
        }
        Ok(())
    }

    // ── User CRUD ────────────────────────────────────────────────────

    async fn get_all_users(&self) -> Result<Vec<AssistantUserRow>, DbError> {
        let rows = sqlx::query_as::<_, AssistantUserRow>("SELECT * FROM assistant_users ORDER BY authorized_at DESC")
            .fetch_all(&self.pool)
            .await?;
        Ok(rows)
    }

    async fn get_user_by_platform(
        &self,
        platform_user_id: &str,
        platform_type: &str,
        channel_id: &str,
    ) -> Result<Option<AssistantUserRow>, DbError> {
        let row = sqlx::query_as::<_, AssistantUserRow>(
            "SELECT * FROM assistant_users \
             WHERE platform_user_id = ? AND platform_type = ? AND channel_id = ?",
        )
        .bind(platform_user_id)
        .bind(platform_type)
        .bind(channel_id)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row)
    }

    async fn create_user(&self, row: &AssistantUserRow) -> Result<(), DbError> {
        sqlx::query(
            "INSERT INTO assistant_users \
                (id, platform_user_id, platform_type, channel_id, display_name, \
                 authorized_at, last_active, session_id) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(&row.id)
        .bind(&row.platform_user_id)
        .bind(&row.platform_type)
        .bind(&row.channel_id)
        .bind(&row.display_name)
        .bind(row.authorized_at)
        .bind(row.last_active)
        .bind(&row.session_id)
        .execute(&self.pool)
        .await
        .map_err(|e| {
            if is_unique_violation(&e) {
                DbError::Conflict(format!(
                    "User '{}' on platform '{}' already exists",
                    row.platform_user_id, row.platform_type
                ))
            } else {
                DbError::Query(e)
            }
        })?;
        Ok(())
    }

    async fn update_user_last_active(&self, id: &str, last_active: nomifun_common::TimestampMs) -> Result<(), DbError> {
        let result = sqlx::query("UPDATE assistant_users SET last_active = ? WHERE id = ?")
            .bind(last_active)
            .bind(id)
            .execute(&self.pool)
            .await?;
        if result.rows_affected() == 0 {
            return Err(DbError::NotFound(format!("User '{id}' not found")));
        }
        Ok(())
    }

    async fn delete_user(&self, id: &str) -> Result<(), DbError> {
        let result = sqlx::query("DELETE FROM assistant_users WHERE id = ?")
            .bind(id)
            .execute(&self.pool)
            .await?;
        if result.rows_affected() == 0 {
            return Err(DbError::NotFound(format!("User '{id}' not found")));
        }
        Ok(())
    }

    // ── Session CRUD ─────────────────────────────────────────────────

    async fn get_all_sessions(&self) -> Result<Vec<AssistantSessionRow>, DbError> {
        let rows =
            sqlx::query_as::<_, AssistantSessionRow>("SELECT * FROM assistant_sessions ORDER BY last_activity DESC")
                .fetch_all(&self.pool)
                .await?;
        Ok(rows)
    }

    async fn get_session(&self, id: &str) -> Result<Option<AssistantSessionRow>, DbError> {
        let row = sqlx::query_as::<_, AssistantSessionRow>("SELECT * FROM assistant_sessions WHERE id = ?")
            .bind(id)
            .fetch_optional(&self.pool)
            .await?;
        Ok(row)
    }

    async fn get_or_create_session(
        &self,
        user_id: &str,
        chat_id: &str,
        channel_id: &str,
        new_row: &AssistantSessionRow,
    ) -> Result<AssistantSessionRow, DbError> {
        // Try to find an existing session first.
        let existing = sqlx::query_as::<_, AssistantSessionRow>(
            "SELECT * FROM assistant_sessions \
             WHERE user_id = ? AND chat_id = ? AND channel_id = ?",
        )
        .bind(user_id)
        .bind(chat_id)
        .bind(channel_id)
        .fetch_optional(&self.pool)
        .await?;

        if let Some(row) = existing {
            // Touch last_activity.
            let now = nomifun_common::now_ms();
            sqlx::query("UPDATE assistant_sessions SET last_activity = ? WHERE id = ?")
                .bind(now)
                .bind(&row.id)
                .execute(&self.pool)
                .await?;

            return Ok(AssistantSessionRow {
                last_activity: now,
                ..row
            });
        }

        // Insert new session.
        sqlx::query(
            "INSERT INTO assistant_sessions \
                (id, user_id, agent_type, conversation_id, workspace, \
                 chat_id, channel_id, created_at, last_activity) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(&new_row.id)
        .bind(&new_row.user_id)
        .bind(&new_row.agent_type)
        .bind(&new_row.conversation_id)
        .bind(&new_row.workspace)
        .bind(&new_row.chat_id)
        .bind(&new_row.channel_id)
        .bind(new_row.created_at)
        .bind(new_row.last_activity)
        .execute(&self.pool)
        .await?;

        Ok(new_row.clone())
    }

    async fn update_session_activity(
        &self,
        id: &str,
        last_activity: nomifun_common::TimestampMs,
    ) -> Result<(), DbError> {
        let result = sqlx::query("UPDATE assistant_sessions SET last_activity = ? WHERE id = ?")
            .bind(last_activity)
            .bind(id)
            .execute(&self.pool)
            .await?;
        if result.rows_affected() == 0 {
            return Err(DbError::NotFound(format!("Session '{id}' not found")));
        }
        Ok(())
    }

    async fn update_session_conversation(&self, id: &str, conversation_id: i64) -> Result<(), DbError> {
        let now = nomifun_common::now_ms();
        let result = sqlx::query(
            "UPDATE assistant_sessions \
             SET conversation_id = ?, last_activity = ? \
             WHERE id = ?",
        )
        .bind(conversation_id)
        .bind(now)
        .bind(id)
        .execute(&self.pool)
        .await?;
        if result.rows_affected() == 0 {
            return Err(DbError::NotFound(format!("Session '{id}' not found")));
        }
        Ok(())
    }

    async fn update_session_agent_type(&self, id: &str, agent_type: &str) -> Result<(), DbError> {
        let now = nomifun_common::now_ms();
        let result = sqlx::query(
            "UPDATE assistant_sessions \
             SET agent_type = ?, last_activity = ? \
             WHERE id = ?",
        )
        .bind(agent_type)
        .bind(now)
        .bind(id)
        .execute(&self.pool)
        .await?;
        if result.rows_affected() == 0 {
            return Err(DbError::NotFound(format!("Session '{id}' not found")));
        }
        Ok(())
    }

    async fn delete_sessions_by_user(&self, user_id: &str) -> Result<(), DbError> {
        sqlx::query("DELETE FROM assistant_sessions WHERE user_id = ?")
            .bind(user_id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    async fn delete_sessions_by_channel(&self, channel_id: &str) -> Result<(), DbError> {
        sqlx::query("DELETE FROM assistant_sessions WHERE channel_id = ?")
            .bind(channel_id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    async fn delete_session_by_user_chat(
        &self,
        user_id: &str,
        chat_id: &str,
        channel_id: &str,
    ) -> Result<(), DbError> {
        sqlx::query(
            "DELETE FROM assistant_sessions \
             WHERE user_id = ? AND chat_id = ? AND channel_id = ?",
        )
        .bind(user_id)
        .bind(chat_id)
        .bind(channel_id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    // ── Pairing Codes ────────────────────────────────────────────────

    async fn create_pairing(&self, row: &PairingCodeRow) -> Result<(), DbError> {
        sqlx::query(
            "INSERT INTO assistant_pairing_codes \
                (code, platform_user_id, platform_type, channel_id, display_name, \
                 requested_at, expires_at, status) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(&row.code)
        .bind(&row.platform_user_id)
        .bind(&row.platform_type)
        .bind(&row.channel_id)
        .bind(&row.display_name)
        .bind(row.requested_at)
        .bind(row.expires_at)
        .bind(&row.status)
        .execute(&self.pool)
        .await
        .map_err(|e| {
            if is_unique_violation(&e) {
                DbError::Conflict(format!("Pairing code '{}' already exists", row.code))
            } else {
                DbError::Query(e)
            }
        })?;
        Ok(())
    }

    async fn get_pending_pairings(&self) -> Result<Vec<PairingCodeRow>, DbError> {
        let rows = sqlx::query_as::<_, PairingCodeRow>(
            "SELECT * FROM assistant_pairing_codes \
             WHERE status = 'pending' \
             ORDER BY requested_at DESC",
        )
        .fetch_all(&self.pool)
        .await?;
        Ok(rows)
    }

    async fn get_pairing_by_code(&self, code: &str) -> Result<Option<PairingCodeRow>, DbError> {
        let row = sqlx::query_as::<_, PairingCodeRow>("SELECT * FROM assistant_pairing_codes WHERE code = ?")
            .bind(code)
            .fetch_optional(&self.pool)
            .await?;
        Ok(row)
    }

    async fn update_pairing_status(&self, code: &str, status: &str) -> Result<(), DbError> {
        let result = sqlx::query("UPDATE assistant_pairing_codes SET status = ? WHERE code = ?")
            .bind(status)
            .bind(code)
            .execute(&self.pool)
            .await?;
        if result.rows_affected() == 0 {
            return Err(DbError::NotFound(format!("Pairing code '{code}' not found")));
        }
        Ok(())
    }

    async fn cleanup_expired_pairings(&self, now: nomifun_common::TimestampMs) -> Result<u64, DbError> {
        let result = sqlx::query(
            "UPDATE assistant_pairing_codes \
             SET status = 'expired' \
             WHERE status = 'pending' AND expires_at <= ?",
        )
        .bind(now)
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected())
    }
}

/// Checks whether a sqlx error indicates a UNIQUE constraint violation.
fn is_unique_violation(err: &sqlx::Error) -> bool {
    match err {
        sqlx::Error::Database(db_err) => db_err.message().contains("UNIQUE constraint failed"),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::init_database_memory;

    async fn setup() -> (SqliteChannelRepository, crate::Database) {
        let db = init_database_memory().await.unwrap();
        let repo = SqliteChannelRepository::new(db.pool().clone());
        (repo, db)
    }

    fn sample_plugin() -> ChannelPluginRow {
        let now = nomifun_common::now_ms();
        ChannelPluginRow {
            id: "tg-1".into(),
            r#type: "telegram".into(),
            name: "My Telegram Bot".into(),
            enabled: false,
            config: r#"{"credentials":{"token":"enc_xxx"}}"#.into(),
            status: None,
            last_connected: None,
            companion_id: None,
            public_agent_id: None,
            bot_key: None,
            created_at: now,
            updated_at: now,
        }
    }

    fn sample_user() -> AssistantUserRow {
        let now = nomifun_common::now_ms();
        AssistantUserRow {
            id: "usr-1".into(),
            platform_user_id: "tg_12345".into(),
            platform_type: "telegram".into(),
            channel_id: Some("tg-1".into()),
            display_name: Some("Alice".into()),
            authorized_at: now,
            last_active: None,
            session_id: None,
        }
    }

    fn sample_session(user_id: &str) -> AssistantSessionRow {
        let now = nomifun_common::now_ms();
        AssistantSessionRow {
            id: "sess-1".into(),
            user_id: user_id.into(),
            agent_type: "gemini".into(),
            conversation_id: None,
            workspace: None,
            chat_id: Some("chat-abc".into()),
            channel_id: Some("tg-1".into()),
            created_at: now,
            last_activity: now,
        }
    }

    fn sample_pairing() -> PairingCodeRow {
        let now = nomifun_common::now_ms();
        PairingCodeRow {
            code: "123456".into(),
            platform_user_id: "tg_99".into(),
            platform_type: "telegram".into(),
            channel_id: None,
            display_name: Some("Bob".into()),
            requested_at: now,
            expires_at: now + 600_000,
            status: "pending".into(),
        }
    }

    // ── Plugin tests ─────────────────────────────────────────────────

    #[tokio::test]
    async fn get_all_plugins_empty() {
        let (repo, _db) = setup().await;
        let plugins = repo.get_all_plugins().await.unwrap();
        assert!(plugins.is_empty());
    }

    #[tokio::test]
    async fn upsert_and_get_plugin() {
        let (repo, _db) = setup().await;
        let plugin = sample_plugin();
        repo.upsert_plugin(&plugin).await.unwrap();

        let found = repo.get_plugin("tg-1").await.unwrap().unwrap();
        assert_eq!(found.id, "tg-1");
        assert_eq!(found.r#type, "telegram");
        assert_eq!(found.name, "My Telegram Bot");
        assert!(!found.enabled);
    }

    #[tokio::test]
    async fn upsert_plugin_updates_existing() {
        let (repo, _db) = setup().await;
        let plugin = sample_plugin();
        repo.upsert_plugin(&plugin).await.unwrap();

        let updated = ChannelPluginRow {
            name: "Updated Bot".into(),
            enabled: true,
            updated_at: nomifun_common::now_ms(),
            ..plugin
        };
        repo.upsert_plugin(&updated).await.unwrap();

        let found = repo.get_plugin("tg-1").await.unwrap().unwrap();
        assert_eq!(found.name, "Updated Bot");
        assert!(found.enabled);
    }

    #[tokio::test]
    async fn get_all_plugins_returns_multiple() {
        let (repo, _db) = setup().await;
        repo.upsert_plugin(&sample_plugin()).await.unwrap();

        let now = nomifun_common::now_ms();
        let lark = ChannelPluginRow {
            id: "lark-1".into(),
            r#type: "lark".into(),
            name: "Lark Bot".into(),
            enabled: true,
            config: "{}".into(),
            status: Some("running".into()),
            last_connected: Some(now),
            companion_id: None,
            public_agent_id: None,
            bot_key: None,
            created_at: now,
            updated_at: now,
        };
        repo.upsert_plugin(&lark).await.unwrap();

        let all = repo.get_all_plugins().await.unwrap();
        assert_eq!(all.len(), 2);
    }

    #[tokio::test]
    async fn update_plugin_status_sets_fields() {
        let (repo, _db) = setup().await;
        repo.upsert_plugin(&sample_plugin()).await.unwrap();

        let now = nomifun_common::now_ms();
        repo.update_plugin_status(
            "tg-1",
            &UpdatePluginStatusParams {
                status: Some("running".into()),
                last_connected: Some(now),
                enabled: Some(true),
            },
        )
        .await
        .unwrap();

        let found = repo.get_plugin("tg-1").await.unwrap().unwrap();
        assert_eq!(found.status.as_deref(), Some("running"));
        assert_eq!(found.last_connected, Some(now));
        assert!(found.enabled);
    }

    #[tokio::test]
    async fn update_plugin_status_not_found() {
        let (repo, _db) = setup().await;
        let err = repo
            .update_plugin_status(
                "nope",
                &UpdatePluginStatusParams {
                    status: Some("error".into()),
                    ..Default::default()
                },
            )
            .await
            .unwrap_err();
        assert!(matches!(err, DbError::NotFound(_)));
    }

    #[tokio::test]
    async fn update_plugin_status_empty_params_is_noop() {
        let (repo, _db) = setup().await;
        repo.upsert_plugin(&sample_plugin()).await.unwrap();
        // No fields to update → no-op, no error.
        repo.update_plugin_status("tg-1", &UpdatePluginStatusParams::default())
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn delete_plugin_removes_row() {
        let (repo, _db) = setup().await;
        repo.upsert_plugin(&sample_plugin()).await.unwrap();
        repo.delete_plugin("tg-1").await.unwrap();
        assert!(repo.get_plugin("tg-1").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn delete_plugin_not_found() {
        let (repo, _db) = setup().await;
        let err = repo.delete_plugin("nope").await.unwrap_err();
        assert!(matches!(err, DbError::NotFound(_)));
    }

    #[tokio::test]
    async fn same_bot_key_on_two_rows_conflicts() {
        let (repo, _db) = setup().await;
        let now = nomifun_common::now_ms();
        let bot = |id: &str, companion: &str| ChannelPluginRow {
            id: id.into(),
            r#type: "lark".into(),
            name: "Lark Bot".into(),
            enabled: true,
            config: "enc".into(),
            status: None,
            last_connected: None,
            companion_id: Some(companion.into()),
            public_agent_id: None,
            bot_key: Some("cli_same_app".into()),
            created_at: now,
            updated_at: now,
        };
        repo.upsert_plugin(&bot("achn_1", "companion_a")).await.unwrap();

        // Same lark app on a second row (= bound to another companion) must fail.
        let err = repo.upsert_plugin(&bot("achn_2", "companion_b")).await.unwrap_err();
        assert!(matches!(err, DbError::Conflict(_)));

        // Re-upserting the same row keeps working.
        repo.upsert_plugin(&bot("achn_1", "companion_b")).await.unwrap();
    }

    #[tokio::test]
    async fn different_bot_keys_same_platform_coexist() {
        let (repo, _db) = setup().await;
        let now = nomifun_common::now_ms();
        for (id, key) in [("achn_1", "cli_app_a"), ("achn_2", "cli_app_b")] {
            repo.upsert_plugin(&ChannelPluginRow {
                id: id.into(),
                r#type: "lark".into(),
                name: "Lark Bot".into(),
                enabled: true,
                config: "enc".into(),
                status: None,
                last_connected: None,
                companion_id: None,
                public_agent_id: None,
                bot_key: Some(key.into()),
                created_at: now,
                updated_at: now,
            })
            .await
            .unwrap();
        }
        assert_eq!(repo.get_all_plugins().await.unwrap().len(), 2);
    }

    #[tokio::test]
    async fn update_plugin_companion_roundtrip_and_clear() {
        let (repo, _db) = setup().await;
        repo.upsert_plugin(&sample_plugin()).await.unwrap();

        repo.update_plugin_companion("tg-1", Some("companion_x")).await.unwrap();
        assert_eq!(
            repo.get_plugin("tg-1").await.unwrap().unwrap().companion_id.as_deref(),
            Some("companion_x")
        );

        repo.update_plugin_companion("tg-1", None).await.unwrap();
        assert!(repo.get_plugin("tg-1").await.unwrap().unwrap().companion_id.is_none());

        let err = repo.update_plugin_companion("nope", Some("companion_x")).await.unwrap_err();
        assert!(matches!(err, DbError::NotFound(_)));
    }

    #[tokio::test]
    async fn update_plugin_public_agent_roundtrip_and_clear() {
        let (repo, _db) = setup().await;
        repo.upsert_plugin(&sample_plugin()).await.unwrap();

        repo.update_plugin_public_agent("tg-1", Some("pubagent_x")).await.unwrap();
        assert_eq!(
            repo.get_plugin("tg-1").await.unwrap().unwrap().public_agent_id.as_deref(),
            Some("pubagent_x")
        );

        repo.update_plugin_public_agent("tg-1", None).await.unwrap();
        assert!(repo.get_plugin("tg-1").await.unwrap().unwrap().public_agent_id.is_none());

        let err = repo.update_plugin_public_agent("nope", Some("pubagent_x")).await.unwrap_err();
        assert!(matches!(err, DbError::NotFound(_)));
    }

    /// A bot row serves EITHER a companion OR a public agent, never both:
    /// setting one binding clears the other, in both directions.
    #[tokio::test]
    async fn companion_and_public_agent_bindings_are_mutually_exclusive_on_a_row() {
        let (repo, _db) = setup().await;
        repo.upsert_plugin(&sample_plugin()).await.unwrap();

        // Bind a companion, then bind a public agent → companion is cleared.
        repo.update_plugin_companion("tg-1", Some("companion_1")).await.unwrap();
        repo.update_plugin_public_agent("tg-1", Some("pubagent_1")).await.unwrap();
        let row = repo.get_plugin("tg-1").await.unwrap().unwrap();
        assert_eq!(row.public_agent_id.as_deref(), Some("pubagent_1"));
        assert!(row.companion_id.is_none(), "binding a public agent must clear the companion");

        // Bind a companion again → public agent is cleared.
        repo.update_plugin_companion("tg-1", Some("companion_2")).await.unwrap();
        let row = repo.get_plugin("tg-1").await.unwrap().unwrap();
        assert_eq!(row.companion_id.as_deref(), Some("companion_2"));
        assert!(row.public_agent_id.is_none(), "binding a companion must clear the public agent");

        // Clearing one binding does NOT touch the other.
        repo.update_plugin_public_agent("tg-1", Some("pubagent_2")).await.unwrap();
        repo.update_plugin_public_agent("tg-1", None).await.unwrap();
        let row = repo.get_plugin("tg-1").await.unwrap().unwrap();
        assert!(row.public_agent_id.is_none());
        assert!(row.companion_id.is_none());
    }

    #[tokio::test]
    async fn update_plugin_bot_key_backfills() {
        let (repo, _db) = setup().await;
        repo.upsert_plugin(&sample_plugin()).await.unwrap();

        repo.update_plugin_bot_key("tg-1", "123456").await.unwrap();
        assert_eq!(
            repo.get_plugin("tg-1").await.unwrap().unwrap().bot_key.as_deref(),
            Some("123456")
        );
    }

    // ── User tests ───────────────────────────────────────────────────

    #[tokio::test]
    async fn get_all_users_empty() {
        let (repo, _db) = setup().await;
        let users = repo.get_all_users().await.unwrap();
        assert!(users.is_empty());
    }

    #[tokio::test]
    async fn create_and_get_user_by_platform() {
        let (repo, _db) = setup().await;
        seed_channel(&repo, "tg-1").await;
        let user = sample_user();
        repo.create_user(&user).await.unwrap();

        let found = repo
            .get_user_by_platform("tg_12345", "telegram", "tg-1")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(found.id, "usr-1");
        assert_eq!(found.display_name.as_deref(), Some("Alice"));
    }

    #[tokio::test]
    async fn create_duplicate_user_returns_conflict() {
        let (repo, _db) = setup().await;
        seed_channel(&repo, "tg-1").await;
        repo.create_user(&sample_user()).await.unwrap();

        let dup = AssistantUserRow {
            id: "usr-2".into(),
            ..sample_user()
        };
        let err = repo.create_user(&dup).await.unwrap_err();
        assert!(matches!(err, DbError::Conflict(_)));
    }

    #[tokio::test]
    async fn get_user_by_platform_not_found() {
        let (repo, _db) = setup().await;
        assert!(
            repo.get_user_by_platform("nope", "telegram", "tg-1")
                .await
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    async fn same_platform_user_two_channels_are_independent() {
        let (repo, _db) = setup().await;
        seed_channel(&repo, "lark-1").await;
        seed_channel(&repo, "lark-2").await;

        let mk = |id: &str, chan: &str| AssistantUserRow {
            id: id.into(),
            platform_user_id: "ou_same".into(),
            platform_type: "lark".into(),
            channel_id: Some(chan.into()),
            display_name: None,
            authorized_at: nomifun_common::now_ms(),
            last_active: None,
            session_id: None,
        };
        repo.create_user(&mk("u1", "lark-1")).await.unwrap();
        repo.create_user(&mk("u2", "lark-2")).await.unwrap();

        assert_eq!(repo.get_user_by_platform("ou_same", "lark", "lark-1").await.unwrap().unwrap().id, "u1");
        assert_eq!(repo.get_user_by_platform("ou_same", "lark", "lark-2").await.unwrap().unwrap().id, "u2");
    }

    #[tokio::test]
    async fn deleting_channel_cascades_its_users() {
        let (repo, _db) = setup().await;
        seed_channel(&repo, "lark-1").await;
        repo.create_user(&AssistantUserRow {
            id: "u1".into(),
            platform_user_id: "ou_x".into(),
            platform_type: "lark".into(),
            channel_id: Some("lark-1".into()),
            display_name: None,
            authorized_at: nomifun_common::now_ms(),
            last_active: None,
            session_id: None,
        }).await.unwrap();

        repo.delete_plugin("lark-1").await.unwrap();
        assert!(repo.get_user_by_platform("ou_x", "lark", "lark-1").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn update_user_last_active_updates_timestamp() {
        let (repo, _db) = setup().await;
        seed_channel(&repo, "tg-1").await;
        repo.create_user(&sample_user()).await.unwrap();

        let new_ts = nomifun_common::now_ms() + 5000;
        repo.update_user_last_active("usr-1", new_ts).await.unwrap();

        let found = repo
            .get_user_by_platform("tg_12345", "telegram", "tg-1")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(found.last_active, Some(new_ts));
    }

    #[tokio::test]
    async fn update_user_last_active_not_found() {
        let (repo, _db) = setup().await;
        let err = repo.update_user_last_active("nope", 123).await.unwrap_err();
        assert!(matches!(err, DbError::NotFound(_)));
    }

    #[tokio::test]
    async fn delete_user_removes_row() {
        let (repo, _db) = setup().await;
        seed_channel(&repo, "tg-1").await;
        repo.create_user(&sample_user()).await.unwrap();
        repo.delete_user("usr-1").await.unwrap();
        assert!(
            repo.get_user_by_platform("tg_12345", "telegram", "tg-1")
                .await
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    async fn delete_user_not_found() {
        let (repo, _db) = setup().await;
        let err = repo.delete_user("nope").await.unwrap_err();
        assert!(matches!(err, DbError::NotFound(_)));
    }

    #[tokio::test]
    async fn delete_user_cascades_sessions() {
        let (repo, _db) = setup().await;
        seed_channel(&repo, "tg-1").await;
        repo.create_user(&sample_user()).await.unwrap();

        let session = sample_session("usr-1");
        repo.get_or_create_session("usr-1", "chat-abc", "tg-1", &session).await.unwrap();

        // Sessions exist before delete.
        assert_eq!(repo.get_all_sessions().await.unwrap().len(), 1);

        repo.delete_user("usr-1").await.unwrap();

        // Sessions cascade-deleted.
        assert!(repo.get_all_sessions().await.unwrap().is_empty());
    }

    // ── Session tests ────────────────────────────────────────────────

    #[tokio::test]
    async fn get_all_sessions_empty() {
        let (repo, _db) = setup().await;
        assert!(repo.get_all_sessions().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn get_or_create_session_creates_new() {
        let (repo, _db) = setup().await;
        seed_channel(&repo, "tg-1").await;
        repo.create_user(&sample_user()).await.unwrap();

        let new = sample_session("usr-1");
        let result = repo.get_or_create_session("usr-1", "chat-abc", "tg-1", &new).await.unwrap();
        assert_eq!(result.id, "sess-1");
        assert_eq!(result.user_id, "usr-1");
        assert_eq!(result.chat_id.as_deref(), Some("chat-abc"));
    }

    #[tokio::test]
    async fn get_or_create_session_reuses_existing() {
        let (repo, _db) = setup().await;
        seed_channel(&repo, "tg-1").await;
        repo.create_user(&sample_user()).await.unwrap();

        let new = sample_session("usr-1");
        let first = repo.get_or_create_session("usr-1", "chat-abc", "tg-1", &new).await.unwrap();

        // Second call with different new_row id should still return the first.
        let another = AssistantSessionRow {
            id: "sess-2".into(),
            ..new
        };
        let second = repo.get_or_create_session("usr-1", "chat-abc", "tg-1", &another).await.unwrap();
        assert_eq!(second.id, first.id);
        // last_activity should be updated.
        assert!(second.last_activity >= first.last_activity);
    }

    #[tokio::test]
    async fn per_chat_isolation_different_chats() {
        let (repo, _db) = setup().await;
        seed_channel(&repo, "tg-1").await;
        repo.create_user(&sample_user()).await.unwrap();

        let s1 = sample_session("usr-1");
        repo.get_or_create_session("usr-1", "chat-abc", "tg-1", &s1).await.unwrap();

        let s2 = AssistantSessionRow {
            id: "sess-2".into(),
            chat_id: Some("chat-xyz".into()),
            ..sample_session("usr-1")
        };
        repo.get_or_create_session("usr-1", "chat-xyz", "tg-1", &s2).await.unwrap();

        assert_eq!(repo.get_all_sessions().await.unwrap().len(), 2);
    }

    #[tokio::test]
    async fn get_session_by_id() {
        let (repo, _db) = setup().await;
        seed_channel(&repo, "tg-1").await;
        repo.create_user(&sample_user()).await.unwrap();

        let new = sample_session("usr-1");
        repo.get_or_create_session("usr-1", "chat-abc", "tg-1", &new).await.unwrap();

        let found = repo.get_session("sess-1").await.unwrap().unwrap();
        assert_eq!(found.agent_type, "gemini");
    }

    #[tokio::test]
    async fn get_session_not_found() {
        let (repo, _db) = setup().await;
        assert!(repo.get_session("nope").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn update_session_activity_updates_timestamp() {
        let (repo, _db) = setup().await;
        seed_channel(&repo, "tg-1").await;
        repo.create_user(&sample_user()).await.unwrap();

        let new = sample_session("usr-1");
        repo.get_or_create_session("usr-1", "chat-abc", "tg-1", &new).await.unwrap();

        let new_ts = nomifun_common::now_ms() + 5000;
        repo.update_session_activity("sess-1", new_ts).await.unwrap();

        let found = repo.get_session("sess-1").await.unwrap().unwrap();
        assert_eq!(found.last_activity, new_ts);
    }

    #[tokio::test]
    async fn update_session_activity_not_found() {
        let (repo, _db) = setup().await;
        let err = repo.update_session_activity("nope", 123).await.unwrap_err();
        assert!(matches!(err, DbError::NotFound(_)));
    }

    #[tokio::test]
    async fn delete_sessions_by_user_removes_all() {
        let (repo, _db) = setup().await;
        seed_channel(&repo, "tg-1").await;
        repo.create_user(&sample_user()).await.unwrap();

        let s1 = sample_session("usr-1");
        repo.get_or_create_session("usr-1", "chat-abc", "tg-1", &s1).await.unwrap();

        let s2 = AssistantSessionRow {
            id: "sess-2".into(),
            chat_id: Some("chat-xyz".into()),
            ..sample_session("usr-1")
        };
        repo.get_or_create_session("usr-1", "chat-xyz", "tg-1", &s2).await.unwrap();

        repo.delete_sessions_by_user("usr-1").await.unwrap();
        assert!(repo.get_all_sessions().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn delete_sessions_by_user_no_sessions_is_ok() {
        let (repo, _db) = setup().await;
        // No sessions exist for this user — should not error.
        repo.delete_sessions_by_user("usr-1").await.unwrap();
    }

    /// Seeds an `assistant_plugins` row so `assistant_sessions.channel_id`
    /// (FK → assistant_plugins(id), added in the seq/primary-key refactor) can
    /// reference it. `channel_id` is used as a routing key bound verbatim in
    /// `get_or_create_session`, so it cannot be nulled out without breaking the
    /// reuse-matching semantics the session tests exercise — the parent row
    /// must exist instead. Idempotent via the upsert path.
    async fn seed_channel(repo: &SqliteChannelRepository, id: &str) {
        let now = nomifun_common::now_ms();
        repo.upsert_plugin(&ChannelPluginRow {
            id: id.into(),
            r#type: "telegram".into(),
            name: "Stub Bot".into(),
            enabled: false,
            config: "{}".into(),
            status: None,
            last_connected: None,
            companion_id: None,
            public_agent_id: None,
            bot_key: None,
            created_at: now,
            updated_at: now,
        })
        .await
        .unwrap();
    }

    /// Helper to create a stub conversation for FK-constrained tests. The
    /// explicit integer id is a valid AUTOINCREMENT rowid.
    async fn create_stub_conversation(pool: &SqlitePool, conv_id: i64) {
        let now = nomifun_common::now_ms();
        // Create prerequisite user first (matches `users` table schema)
        sqlx::query(
            "INSERT OR IGNORE INTO users \
                (id, username, password_hash, created_at, updated_at) \
             VALUES ('sys_test', 'test_user', 'hash', ?1, ?1)",
        )
        .bind(now)
        .execute(pool)
        .await
        .unwrap();

        sqlx::query(
            "INSERT INTO conversations (id, user_id, name, type, created_at, updated_at) \
             VALUES (?1, 'sys_test', 'Test Conv', 'chat', ?2, ?2)",
        )
        .bind(conv_id)
        .bind(now)
        .execute(pool)
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn update_session_conversation_persists() {
        let (repo, db) = setup().await;
        seed_channel(&repo, "tg-1").await;
        repo.create_user(&sample_user()).await.unwrap();

        let new = sample_session("usr-1");
        repo.get_or_create_session("usr-1", "chat-abc", "tg-1", &new).await.unwrap();

        create_stub_conversation(db.pool(), 42).await;

        repo.update_session_conversation("sess-1", 42).await.unwrap();

        let found = repo.get_session("sess-1").await.unwrap().unwrap();
        assert_eq!(found.conversation_id, Some(42));
    }

    #[tokio::test]
    async fn update_session_conversation_not_found() {
        let (repo, _db) = setup().await;
        let err = repo.update_session_conversation("nope", 1).await.unwrap_err();
        assert!(matches!(err, DbError::NotFound(_)));
    }

    #[tokio::test]
    async fn update_session_agent_type_persists() {
        let (repo, _db) = setup().await;
        seed_channel(&repo, "tg-1").await;
        repo.create_user(&sample_user()).await.unwrap();

        let new = sample_session("usr-1");
        repo.get_or_create_session("usr-1", "chat-abc", "tg-1", &new).await.unwrap();

        assert_eq!(repo.get_session("sess-1").await.unwrap().unwrap().agent_type, "gemini");

        repo.update_session_agent_type("sess-1", "acp").await.unwrap();

        let found = repo.get_session("sess-1").await.unwrap().unwrap();
        assert_eq!(found.agent_type, "acp");
    }

    #[tokio::test]
    async fn update_session_agent_type_not_found() {
        let (repo, _db) = setup().await;
        let err = repo.update_session_agent_type("nope", "acp").await.unwrap_err();
        assert!(matches!(err, DbError::NotFound(_)));
    }

    #[tokio::test]
    async fn delete_session_by_user_chat_removes_only_target() {
        let (repo, _db) = setup().await;
        seed_channel(&repo, "tg-1").await;
        repo.create_user(&sample_user()).await.unwrap();

        let s1 = sample_session("usr-1");
        repo.get_or_create_session("usr-1", "chat-abc", "tg-1", &s1).await.unwrap();

        let s2 = AssistantSessionRow {
            id: "sess-2".into(),
            chat_id: Some("chat-xyz".into()),
            ..sample_session("usr-1")
        };
        repo.get_or_create_session("usr-1", "chat-xyz", "tg-1", &s2).await.unwrap();

        repo.delete_session_by_user_chat("usr-1", "chat-abc", "tg-1").await.unwrap();

        let remaining = repo.get_all_sessions().await.unwrap();
        assert_eq!(remaining.len(), 1);
        assert_eq!(remaining[0].chat_id.as_deref(), Some("chat-xyz"));
    }

    #[tokio::test]
    async fn delete_session_by_user_chat_no_match_is_ok() {
        let (repo, _db) = setup().await;
        // No sessions exist — should not error.
        repo.delete_session_by_user_chat("usr-1", "chat-abc", "tg-1").await.unwrap();
    }

    #[tokio::test]
    async fn same_chat_two_channels_get_isolated_sessions() {
        let (repo, _db) = setup().await;
        seed_channel(&repo, "tg-1").await;
        repo.create_user(&sample_user()).await.unwrap();
        seed_channel(&repo, "achn_2").await;

        let s1 = sample_session("usr-1");
        repo.get_or_create_session("usr-1", "chat-abc", "tg-1", &s1).await.unwrap();

        // Same user + same chat through another bot → a second session.
        let s2 = AssistantSessionRow {
            id: "sess-2".into(),
            channel_id: Some("achn_2".into()),
            ..sample_session("usr-1")
        };
        let created = repo
            .get_or_create_session("usr-1", "chat-abc", "achn_2", &s2)
            .await
            .unwrap();
        assert_eq!(created.id, "sess-2");
        assert_eq!(repo.get_all_sessions().await.unwrap().len(), 2);

        // Reuse matches per channel.
        let reused = repo
            .get_or_create_session("usr-1", "chat-abc", "tg-1", &s2)
            .await
            .unwrap();
        assert_eq!(reused.id, "sess-1");
    }

    #[tokio::test]
    async fn delete_sessions_by_channel_only_hits_that_channel() {
        let (repo, _db) = setup().await;
        seed_channel(&repo, "tg-1").await;
        repo.create_user(&sample_user()).await.unwrap();
        seed_channel(&repo, "achn_2").await;

        let s1 = sample_session("usr-1");
        repo.get_or_create_session("usr-1", "chat-abc", "tg-1", &s1).await.unwrap();
        let s2 = AssistantSessionRow {
            id: "sess-2".into(),
            channel_id: Some("achn_2".into()),
            ..sample_session("usr-1")
        };
        repo.get_or_create_session("usr-1", "chat-abc", "achn_2", &s2)
            .await
            .unwrap();

        repo.delete_sessions_by_channel("tg-1").await.unwrap();

        let remaining = repo.get_all_sessions().await.unwrap();
        assert_eq!(remaining.len(), 1);
        assert_eq!(remaining[0].channel_id.as_deref(), Some("achn_2"));
    }

    // ── Pairing tests ────────────────────────────────────────────────

    #[tokio::test]
    async fn create_and_get_pairing() {
        let (repo, _db) = setup().await;
        let pairing = sample_pairing();
        repo.create_pairing(&pairing).await.unwrap();

        let found = repo.get_pairing_by_code("123456").await.unwrap().unwrap();
        assert_eq!(found.platform_user_id, "tg_99");
        assert_eq!(found.status, "pending");
    }

    #[tokio::test]
    async fn create_duplicate_pairing_returns_conflict() {
        let (repo, _db) = setup().await;
        repo.create_pairing(&sample_pairing()).await.unwrap();
        let err = repo.create_pairing(&sample_pairing()).await.unwrap_err();
        assert!(matches!(err, DbError::Conflict(_)));
    }

    #[tokio::test]
    async fn get_pending_pairings_filters_by_status() {
        let (repo, _db) = setup().await;
        let p1 = sample_pairing();
        repo.create_pairing(&p1).await.unwrap();

        let p2 = PairingCodeRow {
            code: "654321".into(),
            status: "approved".into(),
            ..sample_pairing()
        };
        repo.create_pairing(&p2).await.unwrap();

        let pending = repo.get_pending_pairings().await.unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].code, "123456");
    }

    #[tokio::test]
    async fn get_pairing_by_code_not_found() {
        let (repo, _db) = setup().await;
        assert!(repo.get_pairing_by_code("000000").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn update_pairing_status_changes_status() {
        let (repo, _db) = setup().await;
        repo.create_pairing(&sample_pairing()).await.unwrap();

        repo.update_pairing_status("123456", "approved").await.unwrap();

        let found = repo.get_pairing_by_code("123456").await.unwrap().unwrap();
        assert_eq!(found.status, "approved");
    }

    #[tokio::test]
    async fn update_pairing_status_not_found() {
        let (repo, _db) = setup().await;
        let err = repo.update_pairing_status("000000", "approved").await.unwrap_err();
        assert!(matches!(err, DbError::NotFound(_)));
    }

    #[tokio::test]
    async fn cleanup_expired_pairings_marks_expired() {
        let (repo, _db) = setup().await;
        let now = nomifun_common::now_ms();

        // Create an already-expired pairing.
        let expired = PairingCodeRow {
            code: "111111".into(),
            expires_at: now - 1000,
            ..sample_pairing()
        };
        repo.create_pairing(&expired).await.unwrap();

        // Create a still-valid pairing.
        let valid = PairingCodeRow {
            code: "222222".into(),
            expires_at: now + 600_000,
            ..sample_pairing()
        };
        repo.create_pairing(&valid).await.unwrap();

        let cleaned = repo.cleanup_expired_pairings(now).await.unwrap();
        assert_eq!(cleaned, 1);

        let found_expired = repo.get_pairing_by_code("111111").await.unwrap().unwrap();
        assert_eq!(found_expired.status, "expired");

        let found_valid = repo.get_pairing_by_code("222222").await.unwrap().unwrap();
        assert_eq!(found_valid.status, "pending");
    }

    #[tokio::test]
    async fn cleanup_expired_pairings_skips_non_pending() {
        let (repo, _db) = setup().await;
        let now = nomifun_common::now_ms();

        // Create an expired pairing that is already approved.
        let approved = PairingCodeRow {
            code: "333333".into(),
            expires_at: now - 1000,
            status: "approved".into(),
            ..sample_pairing()
        };
        repo.create_pairing(&approved).await.unwrap();

        let cleaned = repo.cleanup_expired_pairings(now).await.unwrap();
        assert_eq!(cleaned, 0);

        let found = repo.get_pairing_by_code("333333").await.unwrap().unwrap();
        assert_eq!(found.status, "approved");
    }
}

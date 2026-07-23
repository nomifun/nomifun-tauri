use nomifun_common::{
    ChannelPluginId, ChannelSessionId, ChannelUserId, CompanionId, ConversationId,
    PublicAgentId, generate_id,
};
use sqlx::{Sqlite, SqlitePool, Transaction};

use crate::error::DbError;
use crate::models::{
    ChannelPairingCodeRow, ChannelPluginRow, ChannelSessionRow, ChannelUserRow,
    NewChannelPairingCodeRow, NewChannelPluginRow, NewChannelSessionRow, NewChannelUserRow,
};
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

async fn lock_channel_plugin(
    tx: &mut Transaction<'_, Sqlite>,
    channel_plugin_id: Option<&str>,
    context: &str,
) -> Result<(), DbError> {
    let Some(channel_plugin_id) = channel_plugin_id else {
        return Ok(());
    };
    let channel_plugin_id = ChannelPluginId::parse(channel_plugin_id).map_err(|error| {
        DbError::Conflict(format!(
            "{context} channel plugin '{channel_plugin_id}' is not a canonical UUIDv7: {error}"
        ))
    })?;
    let parent = sqlx::query(
        "UPDATE channel_plugins SET updated_at = updated_at WHERE channel_plugin_id = ?",
    )
    .bind(channel_plugin_id.as_str())
    .execute(&mut **tx)
    .await?;
    if parent.rows_affected() == 0 {
        return Err(DbError::Conflict(format!(
            "{context} channel plugin '{channel_plugin_id}' does not exist"
        )));
    }
    Ok(())
}

async fn lock_conversation(
    tx: &mut Transaction<'_, Sqlite>,
    conversation_id: Option<&str>,
    context: &str,
) -> Result<Option<String>, DbError> {
    let Some(conversation_id) = conversation_id else {
        return Ok(None);
    };
    let conversation_id = ConversationId::parse(conversation_id).map_err(|error| {
        DbError::Conflict(format!(
            "{context} conversation '{conversation_id}' is not a canonical UUIDv7: {error}"
        ))
    })?;
    let parent = sqlx::query(
        "UPDATE conversations SET updated_at = updated_at WHERE conversation_id = ?",
    )
    .bind(conversation_id.as_str())
    .execute(&mut **tx)
    .await?;
    if parent.rows_affected() == 0 {
        return Err(DbError::Conflict(format!(
            "{context} conversation '{}' does not exist",
            conversation_id
        )));
    }
    Ok(Some(conversation_id.into_string()))
}

fn canonical_plugin_binding_ids(
    companion_id: Option<&str>,
    public_agent_id: Option<&str>,
) -> Result<(Option<String>, Option<String>), DbError> {
    if companion_id.is_some() && public_agent_id.is_some() {
        return Err(DbError::Conflict(
            "channel plugin cannot bind both a companion and a public agent".into(),
        ));
    }
    let companion_id = companion_id
        .map(|value| {
            CompanionId::parse(value)
                .map(CompanionId::into_string)
                .map_err(|error| {
                    DbError::Conflict(format!(
                        "channel plugin companion_id '{value}' is not a canonical UUIDv7: {error}"
                    ))
                })
        })
        .transpose()?;
    let public_agent_id = public_agent_id
        .map(|value| {
            PublicAgentId::parse(value)
                .map(PublicAgentId::into_string)
                .map_err(|error| {
                    DbError::Conflict(format!(
                        "channel plugin public_agent_id '{value}' is not a canonical UUIDv7: {error}"
                    ))
                })
        })
        .transpose()?;
    Ok((companion_id, public_agent_id))
}

fn validate_agent_type(agent_type: &str, context: &str) -> Result<(), DbError> {
    match agent_type {
        "acp" | "openclaw-gateway" | "nanobot" | "remote" | "nomi" => Ok(()),
        _ => Err(DbError::Conflict(format!(
            "{context} agent type '{agent_type}' is not supported"
        ))),
    }
}

#[async_trait::async_trait]
impl IChannelRepository for SqliteChannelRepository {
    // -- Plugin CRUD --------------------------------------------------

    async fn get_all_plugins(&self) -> Result<Vec<ChannelPluginRow>, DbError> {
        let rows = sqlx::query_as::<_, ChannelPluginRow>("SELECT * FROM channel_plugins ORDER BY created_at ASC")
            .fetch_all(&self.pool)
            .await?;
        Ok(rows)
    }

    async fn get_plugin(
        &self,
        channel_plugin_id: &str,
    ) -> Result<Option<ChannelPluginRow>, DbError> {
        let row = sqlx::query_as::<_, ChannelPluginRow>(
            "SELECT * FROM channel_plugins WHERE channel_plugin_id = ?",
        )
        .bind(channel_plugin_id)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row)
    }

    async fn create_plugin(&self, row: &NewChannelPluginRow) -> Result<ChannelPluginRow, DbError> {
        let channel_plugin_id = generate_id();
        let (companion_id, public_agent_id) = canonical_plugin_binding_ids(
            row.companion_id.as_deref(),
            row.public_agent_id.as_deref(),
        )?;
        sqlx::query_as::<_, ChannelPluginRow>(
            "INSERT INTO channel_plugins \
                (channel_plugin_id, type, name, enabled, config, status, last_connected, \
                 companion_id, public_agent_id, bot_key, created_at, updated_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?) \
             RETURNING *",
        )
        .bind(channel_plugin_id)
        .bind(&row.r#type)
        .bind(&row.name)
        .bind(row.enabled)
        .bind(&row.config)
        .bind(&row.status)
        .bind(row.last_connected)
        .bind(&companion_id)
        .bind(&public_agent_id)
        .bind(&row.bot_key)
        .bind(row.created_at)
        .bind(row.updated_at)
        .fetch_one(&self.pool)
        .await
        .map_err(|e| {
            if is_unique_violation(&e) {
                DbError::Conflict(format!(
                    "Bot '{}' on platform '{}' is already configured",
                    row.bot_key.as_deref().unwrap_or("?"),
                    row.r#type
                ))
            } else {
                DbError::Query(e)
            }
        })
    }

    async fn update_plugin(&self, row: &ChannelPluginRow) -> Result<ChannelPluginRow, DbError> {
        ChannelPluginId::parse(&row.channel_plugin_id).map_err(|error| {
            DbError::Conflict(format!(
                "channel plugin id '{}' is not a canonical UUIDv7: {error}",
                row.channel_plugin_id
            ))
        })?;
        let (companion_id, public_agent_id) = canonical_plugin_binding_ids(
            row.companion_id.as_deref(),
            row.public_agent_id.as_deref(),
        )?;
        let updated = sqlx::query_as::<_, ChannelPluginRow>(
            "UPDATE channel_plugins SET \
                type = ?, name = ?, enabled = ?, config = ?, status = ?, \
                last_connected = ?, companion_id = ?, public_agent_id = ?, \
                bot_key = ?, updated_at = ? \
             WHERE channel_plugin_id = ? \
             RETURNING *",
        )
        .bind(&row.r#type)
        .bind(&row.name)
        .bind(row.enabled)
        .bind(&row.config)
        .bind(&row.status)
        .bind(row.last_connected)
        .bind(&companion_id)
        .bind(&public_agent_id)
        .bind(&row.bot_key)
        .bind(row.updated_at)
        .bind(&row.channel_plugin_id)
        .fetch_optional(&self.pool)
        .await
        .map_err(|e| {
            if is_unique_violation(&e) {
                DbError::Conflict(format!(
                    "Bot '{}' on platform '{}' is already configured",
                    row.bot_key.as_deref().unwrap_or("?"),
                    row.r#type
                ))
            } else {
                DbError::Query(e)
            }
        })?;
        updated.ok_or_else(|| {
            DbError::NotFound(format!(
                "Plugin '{}' not found",
                row.channel_plugin_id
            ))
        })
    }

    async fn update_plugin_status(
        &self,
        channel_plugin_id: &str,
        params: &UpdatePluginStatusParams,
    ) -> Result<(), DbError> {
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
        let sql = format!(
            "UPDATE channel_plugins SET {} WHERE channel_plugin_id = ?",
            set_clauses.join(", ")
        );

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
        query = query.bind(channel_plugin_id);

        let result = query.execute(&self.pool).await?;
        if result.rows_affected() == 0 {
            return Err(DbError::NotFound(format!(
                "Plugin '{channel_plugin_id}' not found"
            )));
        }
        Ok(())
    }

    async fn update_plugin_companion(
        &self,
        channel_plugin_id: &str,
        companion_id: Option<&str>,
    ) -> Result<(), DbError> {
        let companion_id = companion_id
            .map(|value| {
                CompanionId::parse(value)
                    .map(CompanionId::into_string)
                    .map_err(|error| {
                        DbError::Conflict(format!(
                            "channel plugin companion_id '{value}' is not a canonical UUIDv7: {error}"
                        ))
                    })
            })
            .transpose()?;
        // Row-level mutual exclusivity: binding a companion (non-null) clears any
        // public-agent binding on the same row. Clearing (`None`) leaves the
        // public-agent binding untouched.
        let result = sqlx::query(
            "UPDATE channel_plugins \
             SET companion_id = ?, \
                 public_agent_id = CASE WHEN ? IS NOT NULL THEN NULL ELSE public_agent_id END, \
                 updated_at = ? \
             WHERE channel_plugin_id = ?",
        )
        .bind(companion_id.as_deref())
        .bind(companion_id.as_deref())
        .bind(nomifun_common::now_ms())
        .bind(channel_plugin_id)
        .execute(&self.pool)
        .await?;
        if result.rows_affected() == 0 {
            return Err(DbError::NotFound(format!(
                "Plugin '{channel_plugin_id}' not found"
            )));
        }
        Ok(())
    }

    async fn update_plugin_public_agent(
        &self,
        channel_plugin_id: &str,
        public_agent_id: Option<&str>,
    ) -> Result<(), DbError> {
        let public_agent_id = public_agent_id
            .map(|value| {
                PublicAgentId::parse(value)
                    .map(PublicAgentId::into_string)
                    .map_err(|error| {
                        DbError::Conflict(format!(
                            "channel plugin public_agent_id '{value}' is not a canonical UUIDv7: {error}"
                        ))
                    })
            })
            .transpose()?;
        // Row-level mutual exclusivity: binding a public agent (non-null) clears
        // any companion binding on the same row. Clearing (`None`) leaves the
        // companion binding untouched.
        let result = sqlx::query(
            "UPDATE channel_plugins \
             SET public_agent_id = ?, \
                 companion_id = CASE WHEN ? IS NOT NULL THEN NULL ELSE companion_id END, \
                 updated_at = ? \
             WHERE channel_plugin_id = ?",
        )
        .bind(public_agent_id.as_deref())
        .bind(public_agent_id.as_deref())
        .bind(nomifun_common::now_ms())
        .bind(channel_plugin_id)
        .execute(&self.pool)
        .await?;
        if result.rows_affected() == 0 {
            return Err(DbError::NotFound(format!(
                "Plugin '{channel_plugin_id}' not found"
            )));
        }
        Ok(())
    }

    async fn update_plugin_bot_key(
        &self,
        channel_plugin_id: &str,
        bot_key: &str,
    ) -> Result<(), DbError> {
        let bot_key = bot_key.trim();
        if bot_key.is_empty() {
            return Err(DbError::Conflict(
                "channel plugin bot_key must not be empty".to_owned(),
            ));
        }
        let result = sqlx::query(
            "UPDATE channel_plugins \
             SET bot_key = ?, updated_at = ? \
             WHERE channel_plugin_id = ?",
        )
        .bind(bot_key)
        .bind(nomifun_common::now_ms())
        .bind(channel_plugin_id)
        .execute(&self.pool)
        .await
        .map_err(|error| {
            if is_unique_violation(&error) {
                DbError::Conflict(format!(
                    "Bot '{bot_key}' is already configured for this platform"
                ))
            } else {
                DbError::Query(error)
            }
        })?;
        if result.rows_affected() == 0 {
            return Err(DbError::NotFound(format!(
                "Plugin '{channel_plugin_id}' not found"
            )));
        }
        Ok(())
    }

    async fn delete_plugin(&self, channel_plugin_id: &str) -> Result<(), DbError> {
        let mut tx = self.pool.begin().await?;
        let locked = sqlx::query(
            "UPDATE channel_plugins \
             SET updated_at = updated_at \
             WHERE channel_plugin_id = ?",
        )
        .bind(channel_plugin_id)
        .execute(&mut *tx)
        .await?;
        if locked.rows_affected() == 0 {
            return Err(DbError::NotFound(format!(
                "Plugin '{channel_plugin_id}' not found"
            )));
        }

        // Deleting plugin-owned users cascades their authoritative sessions in
        // the same transaction.
        sqlx::query(
            "DELETE FROM channel_sessions \
             WHERE channel_user_id IN (\
                 SELECT channel_user_id FROM channel_users WHERE channel_plugin_id = ?\
             )",
        )
        .bind(channel_plugin_id)
        .execute(&mut *tx)
        .await?;
        sqlx::query("DELETE FROM channel_users WHERE channel_plugin_id = ?")
            .bind(channel_plugin_id)
            .execute(&mut *tx)
            .await?;
        sqlx::query("DELETE FROM channel_pairing_codes WHERE channel_plugin_id = ?")
            .bind(channel_plugin_id)
            .execute(&mut *tx)
            .await?;

        // Sessions not owned by a cascaded user retain their history but no
        // longer point at the removed plugin.
        sqlx::query(
            "UPDATE channel_sessions \
             SET channel_plugin_id = NULL \
             WHERE channel_plugin_id = ?",
        )
        .bind(channel_plugin_id)
        .execute(&mut *tx)
        .await?;

        sqlx::query("DELETE FROM channel_plugins WHERE channel_plugin_id = ?")
            .bind(channel_plugin_id)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(())
    }

    // -- User CRUD ----------------------------------------------------

    async fn get_all_users(&self) -> Result<Vec<ChannelUserRow>, DbError> {
        let rows = sqlx::query_as::<_, ChannelUserRow>("SELECT * FROM channel_users ORDER BY authorized_at DESC")
            .fetch_all(&self.pool)
            .await?;
        Ok(rows)
    }

    async fn get_user_by_platform(
        &self,
        platform_user_id: &str,
        platform_type: &str,
        channel_plugin_id: &str,
    ) -> Result<Option<ChannelUserRow>, DbError> {
        let row = sqlx::query_as::<_, ChannelUserRow>(
            "SELECT * FROM channel_users \
             WHERE platform_user_id = ? AND platform_type = ? AND channel_plugin_id = ?",
        )
        .bind(platform_user_id)
        .bind(platform_type)
        .bind(channel_plugin_id)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row)
    }

    async fn create_user(&self, row: &NewChannelUserRow) -> Result<ChannelUserRow, DbError> {
        let mut tx = self.pool.begin().await?;
        lock_channel_plugin(
            &mut tx,
            row.channel_plugin_id.as_deref(),
            "channel user",
        )
        .await?;
        let channel_user_id = generate_id();
        let inserted = sqlx::query_as::<_, ChannelUserRow>(
            "INSERT INTO channel_users \
                (channel_user_id, platform_user_id, platform_type, channel_plugin_id, \
                 display_name, authorized_at, last_active) \
             VALUES (?, ?, ?, ?, ?, ?, ?) \
             RETURNING *",
        )
        .bind(channel_user_id)
        .bind(&row.platform_user_id)
        .bind(&row.platform_type)
        .bind(&row.channel_plugin_id)
        .bind(&row.display_name)
        .bind(row.authorized_at)
        .bind(row.last_active)
        .fetch_one(&mut *tx)
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
        tx.commit().await?;
        Ok(inserted)
    }

    async fn update_user_last_active(
        &self,
        channel_user_id: &str,
        last_active: nomifun_common::TimestampMs,
    ) -> Result<(), DbError> {
        let result = sqlx::query(
            "UPDATE channel_users SET last_active = ? WHERE channel_user_id = ?",
        )
            .bind(last_active)
            .bind(channel_user_id)
            .execute(&self.pool)
            .await?;
        if result.rows_affected() == 0 {
            return Err(DbError::NotFound(format!(
                "User '{channel_user_id}' not found"
            )));
        }
        Ok(())
    }

    async fn delete_user(&self, channel_user_id: &str) -> Result<(), DbError> {
        let mut tx = self.pool.begin().await?;
        let locked = sqlx::query(
            "UPDATE channel_users \
             SET display_name = display_name \
             WHERE channel_user_id = ?",
        )
        .bind(channel_user_id)
        .execute(&mut *tx)
        .await?;
        if locked.rows_affected() == 0 {
            return Err(DbError::NotFound(format!(
                "User '{channel_user_id}' not found"
            )));
        }

        sqlx::query("DELETE FROM channel_sessions WHERE channel_user_id = ?")
            .bind(channel_user_id)
            .execute(&mut *tx)
            .await?;
        sqlx::query("DELETE FROM channel_users WHERE channel_user_id = ?")
            .bind(channel_user_id)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(())
    }

    // -- Session CRUD -----------------------------------------------------

    async fn get_all_sessions(&self) -> Result<Vec<ChannelSessionRow>, DbError> {
        let rows =
            sqlx::query_as::<_, ChannelSessionRow>("SELECT * FROM channel_sessions ORDER BY last_activity DESC")
                .fetch_all(&self.pool)
                .await?;
        Ok(rows)
    }

    async fn get_session(&self, channel_session_id: &str) -> Result<Option<ChannelSessionRow>, DbError> {
        let row = sqlx::query_as::<_, ChannelSessionRow>(
            "SELECT * FROM channel_sessions WHERE channel_session_id = ?",
        )
            .bind(channel_session_id)
            .fetch_optional(&self.pool)
            .await?;
        Ok(row)
    }

    async fn get_or_create_session(
        &self,
        channel_user_id: &str,
        chat_id: &str,
        channel_plugin_id: &str,
        new_row: &NewChannelSessionRow,
    ) -> Result<ChannelSessionRow, DbError> {
        validate_agent_type(&new_row.agent_type, "channel session")?;
        if new_row.channel_user_id != channel_user_id
            || new_row.chat_id.as_deref() != Some(chat_id)
            || new_row.channel_plugin_id.as_deref() != Some(channel_plugin_id)
        {
            return Err(DbError::Conflict(
                "channel session lookup keys must match the inserted row".into(),
            ));
        }
        let session_id = ChannelSessionId::parse(&new_row.channel_session_id).map_err(|error| {
            DbError::Conflict(format!(
                "channel session id '{}' is not a canonical UUIDv7: {error}",
                new_row.channel_session_id
            ))
        })?;
        let mut tx = self.pool.begin().await?;
        lock_channel_plugin(&mut tx, Some(channel_plugin_id), "channel session").await?;
        let channel_user_id = ChannelUserId::parse(channel_user_id).map_err(|error| {
            DbError::Conflict(format!(
                "channel session user '{channel_user_id}' is not a canonical UUIDv7: {error}"
            ))
        })?;
        let user_plugin_id: Option<String> = sqlx::query_scalar(
            "UPDATE channel_users SET last_active = last_active WHERE channel_user_id = ? \
             RETURNING channel_plugin_id",
        )
        .bind(channel_user_id.as_str())
        .fetch_optional(&mut *tx)
        .await?
        .ok_or_else(|| {
            DbError::Conflict(format!(
                "channel session user '{channel_user_id}' does not exist"
            ))
        })?;
        if user_plugin_id.as_deref().is_some()
            && user_plugin_id.as_deref() != Some(channel_plugin_id)
        {
            return Err(DbError::Conflict(
                "channel session plugin does not match its channel user".into(),
            ));
        }
        let conversation_id = lock_conversation(
            &mut tx,
            new_row.conversation_id.as_deref(),
            "channel session",
        )
        .await?;

        // Try to find an existing session under the same writer transaction.
        let existing = sqlx::query_as::<_, ChannelSessionRow>(
            "SELECT * FROM channel_sessions \
             WHERE channel_user_id = ? AND chat_id = ? AND channel_plugin_id = ?",
        )
        .bind(channel_user_id.as_str())
        .bind(chat_id)
        .bind(channel_plugin_id)
        .fetch_optional(&mut *tx)
        .await?;

        if let Some(row) = existing {
            // Touch last_activity.
            let now = nomifun_common::now_ms();
            sqlx::query(
                "UPDATE channel_sessions SET last_activity = ? WHERE channel_session_id = ?",
            )
                .bind(now)
                .bind(&row.channel_session_id)
                .execute(&mut *tx)
                .await?;
            tx.commit().await?;
            return Ok(ChannelSessionRow {
                last_activity: now,
                ..row
            });
        }

        // Insert new session.
        let inserted = sqlx::query_as::<_, ChannelSessionRow>(
            "INSERT INTO channel_sessions \
                (channel_session_id, channel_user_id, agent_type, conversation_id, workspace, \
                 chat_id, channel_plugin_id, created_at, last_activity) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?) \
             RETURNING *",
        )
        .bind(session_id.as_str())
        .bind(&new_row.channel_user_id)
        .bind(&new_row.agent_type)
        .bind(&conversation_id)
        .bind(&new_row.workspace)
        .bind(&new_row.chat_id)
        .bind(&new_row.channel_plugin_id)
        .bind(new_row.created_at)
        .bind(new_row.last_activity)
        .fetch_one(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(inserted)
    }

    async fn update_session_activity(
        &self,
        channel_session_id: &str,
        last_activity: nomifun_common::TimestampMs,
    ) -> Result<(), DbError> {
        let result = sqlx::query(
            "UPDATE channel_sessions SET last_activity = ? WHERE channel_session_id = ?",
        )
            .bind(last_activity)
            .bind(channel_session_id)
            .execute(&self.pool)
            .await?;
        if result.rows_affected() == 0 {
            return Err(DbError::NotFound(format!("Session '{channel_session_id}' not found")));
        }
        Ok(())
    }

    async fn update_session_conversation(&self, channel_session_id: &str, conversation_id: &str) -> Result<(), DbError> {
        let now = nomifun_common::now_ms();
        let mut tx = self.pool.begin().await?;
        let conversation_id =
            lock_conversation(&mut tx, Some(conversation_id), "channel session update")
                .await?
                .expect("Some input returns Some");
        let result = sqlx::query(
            "UPDATE channel_sessions \
             SET conversation_id = ?, last_activity = ? \
             WHERE channel_session_id = ?",
        )
        .bind(conversation_id)
        .bind(now)
        .bind(channel_session_id)
        .execute(&mut *tx)
        .await?;
        if result.rows_affected() == 0 {
            return Err(DbError::NotFound(format!("Session '{channel_session_id}' not found")));
        }
        tx.commit().await?;
        Ok(())
    }

    async fn update_session_agent_type(&self, channel_session_id: &str, agent_type: &str) -> Result<(), DbError> {
        validate_agent_type(agent_type, "channel session update")?;
        let now = nomifun_common::now_ms();
        let result = sqlx::query(
            "UPDATE channel_sessions \
             SET agent_type = ?, last_activity = ? \
             WHERE channel_session_id = ?",
        )
        .bind(agent_type)
        .bind(now)
        .bind(channel_session_id)
        .execute(&self.pool)
        .await?;
        if result.rows_affected() == 0 {
            return Err(DbError::NotFound(format!("Session '{channel_session_id}' not found")));
        }
        Ok(())
    }

    async fn delete_sessions_by_user(&self, channel_user_id: &str) -> Result<(), DbError> {
        let mut tx = self.pool.begin().await?;
        sqlx::query("DELETE FROM channel_sessions WHERE channel_user_id = ?")
            .bind(channel_user_id)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(())
    }

    async fn delete_sessions_by_channel(
        &self,
        channel_plugin_id: &str,
    ) -> Result<(), DbError> {
        let mut tx = self.pool.begin().await?;
        sqlx::query("DELETE FROM channel_sessions WHERE channel_plugin_id = ?")
            .bind(channel_plugin_id)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(())
    }

    async fn delete_session_by_user_chat(
        &self,
        channel_user_id: &str,
        chat_id: &str,
        channel_plugin_id: &str,
    ) -> Result<(), DbError> {
        let mut tx = self.pool.begin().await?;
        sqlx::query(
            "DELETE FROM channel_sessions \
             WHERE channel_user_id = ? AND chat_id = ? AND channel_plugin_id = ?",
        )
        .bind(channel_user_id)
        .bind(chat_id)
        .bind(channel_plugin_id)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(())
    }

    // -- Pairing Codes ------------------------------------------------

    async fn create_pairing(&self, row: &NewChannelPairingCodeRow) -> Result<ChannelPairingCodeRow, DbError> {
        let mut tx = self.pool.begin().await?;
        lock_channel_plugin(
            &mut tx,
            row.channel_plugin_id.as_deref(),
            "channel pairing",
        )
        .await?;
        let inserted = sqlx::query_as::<_, ChannelPairingCodeRow>(
            "INSERT INTO channel_pairing_codes \
                (code, platform_user_id, platform_type, channel_plugin_id, display_name, \
                 requested_at, expires_at, status) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?) \
             RETURNING *",
        )
        .bind(&row.code)
        .bind(&row.platform_user_id)
        .bind(&row.platform_type)
        .bind(&row.channel_plugin_id)
        .bind(&row.display_name)
        .bind(row.requested_at)
        .bind(row.expires_at)
        .bind(&row.status)
        .fetch_one(&mut *tx)
        .await
        .map_err(|e| {
            if is_unique_violation(&e) {
                DbError::Conflict(format!("Pairing code '{}' already exists", row.code))
            } else {
                DbError::Query(e)
            }
        })?;
        tx.commit().await?;
        Ok(inserted)
    }

    async fn get_pending_pairings(&self) -> Result<Vec<ChannelPairingCodeRow>, DbError> {
        let rows = sqlx::query_as::<_, ChannelPairingCodeRow>(
            "SELECT * FROM channel_pairing_codes \
             WHERE status = 'pending' \
             ORDER BY requested_at DESC",
        )
        .fetch_all(&self.pool)
        .await?;
        Ok(rows)
    }

    async fn get_pairing_by_code(&self, code: &str) -> Result<Option<ChannelPairingCodeRow>, DbError> {
        let row = sqlx::query_as::<_, ChannelPairingCodeRow>("SELECT * FROM channel_pairing_codes WHERE code = ?")
            .bind(code)
            .fetch_optional(&self.pool)
            .await?;
        Ok(row)
    }

    async fn update_pairing_status(&self, code: &str, status: &str) -> Result<(), DbError> {
        let result = sqlx::query("UPDATE channel_pairing_codes SET status = ? WHERE code = ?")
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
            "UPDATE channel_pairing_codes \
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
    const MISSING_ID: &str = "0190f5fe-7c00-7a00-8000-000000000999";

    use super::*;
    use crate::init_database_memory;

    async fn setup() -> (SqliteChannelRepository, crate::Database) {
        let db = init_database_memory().await.unwrap();
        let repo = SqliteChannelRepository::new(db.pool().clone());
        (repo, db)
    }

    fn sample_plugin() -> NewChannelPluginRow {
        let now = nomifun_common::now_ms();
        NewChannelPluginRow {
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

    fn sample_user(channel_plugin_id: &str) -> NewChannelUserRow {
        let now = nomifun_common::now_ms();
        NewChannelUserRow {
            platform_user_id: "tg_12345".into(),
            platform_type: "telegram".into(),
            channel_plugin_id: Some(channel_plugin_id.to_owned()),
            display_name: Some("Alice".into()),
            authorized_at: now,
            last_active: None,
        }
    }

    fn sample_session(
        channel_user_id: &str,
        channel_plugin_id: &str,
        chat_id: &str,
    ) -> NewChannelSessionRow {
        let now = nomifun_common::now_ms();
        NewChannelSessionRow {
            channel_session_id: nomifun_common::ChannelSessionId::new().into_string(),
            channel_user_id: channel_user_id.to_owned(),
            agent_type: "acp".into(),
            conversation_id: None,
            workspace: None,
            chat_id: Some(chat_id.into()),
            channel_plugin_id: Some(channel_plugin_id.to_owned()),
            created_at: now,
            last_activity: now,
        }
    }

    fn sample_pairing() -> NewChannelPairingCodeRow {
        let now = nomifun_common::now_ms();
        NewChannelPairingCodeRow {
            code: "123456".into(),
            platform_user_id: "tg_99".into(),
            platform_type: "telegram".into(),
            channel_plugin_id: None,
            display_name: Some("Bob".into()),
            requested_at: now,
            expires_at: now + 600_000,
            status: "pending".into(),
        }
    }

    async fn seed_channel(repo: &SqliteChannelRepository, name: &str) -> ChannelPluginRow {
        let mut plugin = sample_plugin();
        plugin.name = name.into();
        repo.create_plugin(&plugin).await.unwrap()
    }

    async fn seed_user(
        repo: &SqliteChannelRepository,
        channel_plugin_id: &str,
    ) -> ChannelUserRow {
        repo.create_user(&sample_user(channel_plugin_id))
            .await
            .unwrap()
    }

    // -- Plugin tests -----------------------------------------------------

    #[tokio::test]
    async fn get_all_plugins_empty() {
        let (repo, _db) = setup().await;
        let plugins = repo.get_all_plugins().await.unwrap();
        assert!(plugins.is_empty());
    }

    #[tokio::test]
    async fn create_and_get_plugin() {
        let (repo, _db) = setup().await;
        let plugin = repo.create_plugin(&sample_plugin()).await.unwrap();

        let found = repo
            .get_plugin(&plugin.channel_plugin_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(found.channel_plugin_id, plugin.channel_plugin_id);
        assert_eq!(found.r#type, "telegram");
        assert_eq!(found.name, "My Telegram Bot");
        assert!(!found.enabled);
    }

    #[tokio::test]
    async fn update_plugin_updates_existing() {
        let (repo, _db) = setup().await;
        let plugin = repo.create_plugin(&sample_plugin()).await.unwrap();

        let updated = ChannelPluginRow {
            name: "Updated Bot".into(),
            enabled: true,
            updated_at: nomifun_common::now_ms(),
            ..plugin
        };
        repo.update_plugin(&updated).await.unwrap();

        let found = repo
            .get_plugin(&updated.channel_plugin_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(found.name, "Updated Bot");
        assert!(found.enabled);
    }

    #[tokio::test]
    async fn get_all_plugins_returns_multiple() {
        let (repo, _db) = setup().await;
        repo.create_plugin(&sample_plugin()).await.unwrap();

        let now = nomifun_common::now_ms();
        let lark = NewChannelPluginRow {
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
        repo.create_plugin(&lark).await.unwrap();

        let all = repo.get_all_plugins().await.unwrap();
        assert_eq!(all.len(), 2);
    }

    #[tokio::test]
    async fn update_plugin_status_sets_fields() {
        let (repo, _db) = setup().await;
        let plugin = repo.create_plugin(&sample_plugin()).await.unwrap();

        let now = nomifun_common::now_ms();
        repo.update_plugin_status(
            &plugin.channel_plugin_id,
            &UpdatePluginStatusParams {
                status: Some("running".into()),
                last_connected: Some(now),
                enabled: Some(true),
            },
        )
        .await
        .unwrap();

        let found = repo
            .get_plugin(&plugin.channel_plugin_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(found.status.as_deref(), Some("running"));
        assert_eq!(found.last_connected, Some(now));
        assert!(found.enabled);
    }

    #[tokio::test]
    async fn update_plugin_status_not_found() {
        let (repo, _db) = setup().await;
        let missing_id = ChannelPluginId::new();
        let err = repo
            .update_plugin_status(
                missing_id.as_str(),
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
        let plugin = repo.create_plugin(&sample_plugin()).await.unwrap();
        // No fields to update → no-op, no error.
        repo.update_plugin_status(
            &plugin.channel_plugin_id,
            &UpdatePluginStatusParams::default(),
        )
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn delete_plugin_removes_row() {
        let (repo, db) = setup().await;
        let plugin = repo.create_plugin(&sample_plugin()).await.unwrap();
        let other_plugin = seed_channel(&repo, "Other channel").await;
        let owned_user = seed_user(&repo, &plugin.channel_plugin_id).await;
        let mut unscoped_user = sample_user(&other_plugin.channel_plugin_id);
        unscoped_user.platform_user_id = "tg_unscoped".into();
        unscoped_user.channel_plugin_id = None;
        let other_user = repo.create_user(&unscoped_user).await.unwrap();
        let owned_session = sample_session(
            &owned_user.channel_user_id,
            &plugin.channel_plugin_id,
            "owned",
        );
        repo.get_or_create_session(
            &owned_user.channel_user_id,
            "owned",
            &plugin.channel_plugin_id,
            &owned_session,
        )
            .await
            .unwrap();
        let retained_session = sample_session(
            &other_user.channel_user_id,
            &plugin.channel_plugin_id,
            "retained",
        );
        let retained_session = repo
            .get_or_create_session(
                &other_user.channel_user_id,
                "retained",
                &plugin.channel_plugin_id,
                &retained_session,
            )
            .await
            .unwrap();
        let mut pairing = sample_pairing();
        pairing.code = "654321".into();
        pairing.channel_plugin_id = Some(plugin.channel_plugin_id.clone());
        repo.create_pairing(&pairing).await.unwrap();

        repo.delete_plugin(&plugin.channel_plugin_id).await.unwrap();

        assert!(
            repo.get_plugin(&plugin.channel_plugin_id)
                .await
                .unwrap()
                .is_none()
        );
        assert!(
            repo.get_user_by_platform(
                "tg_12345",
                "telegram",
                &plugin.channel_plugin_id,
            )
                .await
                .unwrap()
                .is_none()
        );
        assert!(
            repo.get_session(&owned_session.channel_session_id)
                .await
                .unwrap()
                .is_none()
        );
        assert!(
            repo.get_pairing_by_code("654321")
                .await
                .unwrap()
                .is_none()
        );
        let retained = repo
            .get_session(&retained_session.channel_session_id)
            .await
            .unwrap()
            .unwrap();
        assert!(retained.channel_plugin_id.is_none());

        let remaining_user_count: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM channel_users WHERE channel_user_id = ?")
                .bind(&other_user.channel_user_id)
                .fetch_one(db.pool())
                .await
                .unwrap();
        assert_eq!(remaining_user_count, 1);
    }

    #[tokio::test]
    async fn delete_plugin_not_found() {
        let (repo, _db) = setup().await;
        let missing_id = ChannelPluginId::new();
        let err = repo.delete_plugin(missing_id.as_str()).await.unwrap_err();
        assert!(matches!(err, DbError::NotFound(_)));
    }

    #[tokio::test]
    async fn same_bot_key_on_two_rows_conflicts() {
        let (repo, _db) = setup().await;
        let now = nomifun_common::now_ms();
        let companion_a = CompanionId::new().into_string();
        let companion_b = CompanionId::new().into_string();
        let bot = |name: &str, companion: &str| NewChannelPluginRow {
            r#type: "lark".into(),
            name: name.into(),
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
        let first = repo
            .create_plugin(&bot("Lark Bot A", &companion_a))
            .await
            .unwrap();

        // Same lark app on a second row (= bound to another companion) must fail.
        let err = repo
            .create_plugin(&bot("Lark Bot B", &companion_b))
            .await
            .unwrap_err();
        assert!(matches!(err, DbError::Conflict(_)));

        // Updating the same row keeps working.
        repo.update_plugin(&ChannelPluginRow {
            companion_id: Some(companion_b),
            updated_at: nomifun_common::now_ms(),
            ..first
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn different_bot_keys_same_platform_coexist() {
        let (repo, _db) = setup().await;
        let now = nomifun_common::now_ms();
        for (name, key) in [("Lark Bot A", "cli_app_a"), ("Lark Bot B", "cli_app_b")] {
            repo.create_plugin(&NewChannelPluginRow {
                r#type: "lark".into(),
                name: name.into(),
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
        let plugin = repo.create_plugin(&sample_plugin()).await.unwrap();
        let companion_id = CompanionId::new().into_string();

        repo.update_plugin_companion(&plugin.channel_plugin_id, Some(&companion_id))
            .await
            .unwrap();
        assert_eq!(
            repo.get_plugin(&plugin.channel_plugin_id)
                .await
                .unwrap()
                .unwrap()
                .companion_id
                .as_deref(),
            Some(companion_id.as_str())
        );

        repo.update_plugin_companion(&plugin.channel_plugin_id, None)
            .await
            .unwrap();
        assert!(
            repo.get_plugin(&plugin.channel_plugin_id)
                .await
                .unwrap()
                .unwrap()
                .companion_id
                .is_none()
        );

        let missing_id = ChannelPluginId::new();
        let err = repo
            .update_plugin_companion(missing_id.as_str(), Some(&companion_id))
            .await
            .unwrap_err();
        assert!(matches!(err, DbError::NotFound(_)));
    }

    #[tokio::test]
    async fn update_plugin_public_agent_roundtrip_and_clear() {
        let (repo, _db) = setup().await;
        let plugin = repo.create_plugin(&sample_plugin()).await.unwrap();
        let public_agent_id = PublicAgentId::new().into_string();

        repo.update_plugin_public_agent(&plugin.channel_plugin_id, Some(&public_agent_id))
            .await
            .unwrap();
        assert_eq!(
            repo.get_plugin(&plugin.channel_plugin_id)
                .await
                .unwrap()
                .unwrap()
                .public_agent_id
                .as_deref(),
            Some(public_agent_id.as_str())
        );

        repo.update_plugin_public_agent(&plugin.channel_plugin_id, None)
            .await
            .unwrap();
        assert!(
            repo.get_plugin(&plugin.channel_plugin_id)
                .await
                .unwrap()
                .unwrap()
                .public_agent_id
                .is_none()
        );

        let missing_id = ChannelPluginId::new();
        let err = repo
            .update_plugin_public_agent(missing_id.as_str(), Some(&public_agent_id))
            .await
            .unwrap_err();
        assert!(matches!(err, DbError::NotFound(_)));
    }

    /// A bot row serves EITHER a companion OR a public agent, never both:
    /// setting one binding clears the other, in both directions.
    #[tokio::test]
    async fn companion_and_public_agent_bindings_are_mutually_exclusive_on_a_row() {
        let (repo, _db) = setup().await;
        let plugin = repo.create_plugin(&sample_plugin()).await.unwrap();
        let companion_1 = CompanionId::new().into_string();
        let companion_2 = CompanionId::new().into_string();
        let public_agent_1 = PublicAgentId::new().into_string();
        let public_agent_2 = PublicAgentId::new().into_string();

        // Bind a companion, then bind a public agent → companion is cleared.
        repo.update_plugin_companion(&plugin.channel_plugin_id, Some(&companion_1))
            .await
            .unwrap();
        repo.update_plugin_public_agent(&plugin.channel_plugin_id, Some(&public_agent_1))
            .await
            .unwrap();
        let row = repo
            .get_plugin(&plugin.channel_plugin_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(row.public_agent_id.as_deref(), Some(public_agent_1.as_str()));
        assert!(
            row.companion_id.is_none(),
            "binding a public agent must clear the companion"
        );

        // Bind a companion again → public agent is cleared.
        repo.update_plugin_companion(&plugin.channel_plugin_id, Some(&companion_2))
            .await
            .unwrap();
        let row = repo
            .get_plugin(&plugin.channel_plugin_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(row.companion_id.as_deref(), Some(companion_2.as_str()));
        assert!(
            row.public_agent_id.is_none(),
            "binding a companion must clear the public agent"
        );

        // Clearing one binding does NOT touch the other.
        repo.update_plugin_public_agent(&plugin.channel_plugin_id, Some(&public_agent_2))
            .await
            .unwrap();
        repo.update_plugin_public_agent(&plugin.channel_plugin_id, None)
            .await
            .unwrap();
        let row = repo
            .get_plugin(&plugin.channel_plugin_id)
            .await
            .unwrap()
            .unwrap();
        assert!(row.public_agent_id.is_none());
        assert!(row.companion_id.is_none());
    }

    #[tokio::test]
    async fn update_plugin_bot_key_backfills() {
        let (repo, _db) = setup().await;
        let plugin = repo.create_plugin(&sample_plugin()).await.unwrap();

        repo.update_plugin_bot_key(&plugin.channel_plugin_id, "123456")
            .await
            .unwrap();
        assert_eq!(
            repo.get_plugin(&plugin.channel_plugin_id)
                .await
                .unwrap()
                .unwrap()
                .bot_key
                .as_deref(),
            Some("123456")
        );
    }

    // -- User tests -------------------------------------------------------

    #[tokio::test]
    async fn get_all_users_empty() {
        let (repo, _db) = setup().await;
        let users = repo.get_all_users().await.unwrap();
        assert!(users.is_empty());
    }

    #[tokio::test]
    async fn create_and_get_user_by_platform() {
        let (repo, _db) = setup().await;
        let plugin = seed_channel(&repo, "Telegram Stub").await;
        let user = repo
            .create_user(&sample_user(&plugin.channel_plugin_id))
            .await
            .unwrap();

        let found = repo
            .get_user_by_platform("tg_12345", "telegram", &plugin.channel_plugin_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(found.channel_user_id, user.channel_user_id);
        assert_eq!(found.display_name.as_deref(), Some("Alice"));
    }

    #[tokio::test]
    async fn create_duplicate_user_returns_conflict() {
        let (repo, _db) = setup().await;
        let plugin = seed_channel(&repo, "Telegram Stub").await;
        let user = sample_user(&plugin.channel_plugin_id);
        repo.create_user(&user).await.unwrap();

        let err = repo.create_user(&user).await.unwrap_err();
        assert!(matches!(err, DbError::Conflict(_)));
    }

    #[tokio::test]
    async fn get_user_by_platform_not_found() {
        let (repo, _db) = setup().await;
        let plugin = seed_channel(&repo, "Telegram Stub").await;
        assert!(
            repo.get_user_by_platform("nope", "telegram", &plugin.channel_plugin_id)
                .await
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    async fn same_platform_user_two_channels_are_independent() {
        let (repo, _db) = setup().await;
        let first_plugin = seed_channel(&repo, "Lark Stub A").await;
        let second_plugin = seed_channel(&repo, "Lark Stub B").await;

        let mk = |channel_plugin_id: &str| NewChannelUserRow {
            platform_user_id: "ou_same".into(),
            platform_type: "lark".into(),
            channel_plugin_id: Some(channel_plugin_id.to_owned()),
            display_name: None,
            authorized_at: nomifun_common::now_ms(),
            last_active: None,
        };
        let first_user = repo
            .create_user(&mk(&first_plugin.channel_plugin_id))
            .await
            .unwrap();
        let second_user = repo
            .create_user(&mk(&second_plugin.channel_plugin_id))
            .await
            .unwrap();

        assert_eq!(
            repo.get_user_by_platform("ou_same", "lark", &first_plugin.channel_plugin_id)
                .await
                .unwrap()
                .unwrap()
                .channel_user_id,
            first_user.channel_user_id
        );
        assert_eq!(
            repo.get_user_by_platform("ou_same", "lark", &second_plugin.channel_plugin_id)
                .await
                .unwrap()
                .unwrap()
                .channel_user_id,
            second_user.channel_user_id
        );
    }

    #[tokio::test]
    async fn deleting_channel_removes_scoped_user() {
        let (repo, _db) = setup().await;
        let plugin = seed_channel(&repo, "Lark Stub").await;
        repo.create_user(&NewChannelUserRow {
                platform_user_id: "ou_x".into(),
                platform_type: "lark".into(),
                channel_plugin_id: Some(plugin.channel_plugin_id.clone()),
                display_name: None,
                authorized_at: nomifun_common::now_ms(),
                last_active: None,
            })
            .await
            .unwrap();

        repo.delete_plugin(&plugin.channel_plugin_id).await.unwrap();
        assert!(
            repo.get_all_users()
                .await
                .unwrap()
                .iter()
                .all(|user| user.platform_user_id != "ou_x")
        );
    }

    #[tokio::test]
    async fn update_user_last_active_updates_timestamp() {
        let (repo, _db) = setup().await;
        let plugin = seed_channel(&repo, "Telegram Stub").await;
        let user = seed_user(&repo, &plugin.channel_plugin_id).await;

        let new_ts = nomifun_common::now_ms() + 5000;
        repo.update_user_last_active(&user.channel_user_id, new_ts)
            .await
            .unwrap();

        let found = repo
            .get_user_by_platform("tg_12345", "telegram", &plugin.channel_plugin_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(found.last_active, Some(new_ts));
    }

    #[tokio::test]
    async fn update_user_last_active_not_found() {
        let (repo, _db) = setup().await;
        let missing_id = ChannelUserId::new();
        let err = repo
            .update_user_last_active(missing_id.as_str(), 123)
            .await
            .unwrap_err();
        assert!(matches!(err, DbError::NotFound(_)));
    }

    #[tokio::test]
    async fn delete_user_removes_row() {
        let (repo, _db) = setup().await;
        let plugin = seed_channel(&repo, "Telegram Stub").await;
        let user = seed_user(&repo, &plugin.channel_plugin_id).await;
        repo.delete_user(&user.channel_user_id).await.unwrap();
        assert!(
            repo.get_user_by_platform("tg_12345", "telegram", &plugin.channel_plugin_id)
                .await
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    async fn delete_user_not_found() {
        let (repo, _db) = setup().await;
        let missing_id = ChannelUserId::new();
        let err = repo.delete_user(missing_id.as_str()).await.unwrap_err();
        assert!(matches!(err, DbError::NotFound(_)));
    }

    #[tokio::test]
    async fn delete_user_cascades_sessions() {
        let (repo, _db) = setup().await;
        let plugin = seed_channel(&repo, "Telegram Stub").await;
        let user = seed_user(&repo, &plugin.channel_plugin_id).await;

        let session = sample_session(
            &user.channel_user_id,
            &plugin.channel_plugin_id,
            "chat-abc",
        );
        repo.get_or_create_session(
            &user.channel_user_id,
            "chat-abc",
            &plugin.channel_plugin_id,
            &session,
        )
            .await
            .unwrap();

        // Sessions exist before delete.
        assert_eq!(repo.get_all_sessions().await.unwrap().len(), 1);

        repo.delete_user(&user.channel_user_id).await.unwrap();

        assert!(repo.get_all_sessions().await.unwrap().is_empty());
    }

    // -- Session tests ------------------------------------------------

    #[tokio::test]
    async fn get_all_sessions_empty() {
        let (repo, _db) = setup().await;
        assert!(repo.get_all_sessions().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn get_or_create_session_creates_new() {
        let (repo, _db) = setup().await;
        let plugin = seed_channel(&repo, "Telegram Stub").await;
        let user = seed_user(&repo, plugin.channel_plugin_id.as_str()).await;

        let new = sample_session(user.channel_user_id.as_str(), plugin.channel_plugin_id.as_str(), "chat-abc");
        let result = repo
            .get_or_create_session(user.channel_user_id.as_str(), "chat-abc", plugin.channel_plugin_id.as_str(), &new)
            .await
            .unwrap();
        assert_eq!(result.channel_session_id, new.channel_session_id);
        assert_eq!(result.channel_user_id, user.channel_user_id.as_str());
        assert_eq!(result.chat_id.as_deref(), Some("chat-abc"));
    }

    #[tokio::test]
    async fn get_or_create_session_reuses_existing() {
        let (repo, _db) = setup().await;
        let plugin = seed_channel(&repo, "Telegram Stub").await;
        let user = seed_user(&repo, plugin.channel_plugin_id.as_str()).await;

        let new = sample_session(user.channel_user_id.as_str(), plugin.channel_plugin_id.as_str(), "chat-abc");
        let first = repo
            .get_or_create_session(user.channel_user_id.as_str(), "chat-abc", plugin.channel_plugin_id.as_str(), &new)
            .await
            .unwrap();

        // A different proposed business id still reuses the persisted session.
        let another = sample_session(user.channel_user_id.as_str(), plugin.channel_plugin_id.as_str(), "chat-abc");
        let second = repo
            .get_or_create_session(user.channel_user_id.as_str(), "chat-abc", plugin.channel_plugin_id.as_str(), &another)
            .await
            .unwrap();
        assert_eq!(second.channel_session_id, first.channel_session_id);
        // last_activity should be updated.
        assert!(second.last_activity >= first.last_activity);
    }

    #[tokio::test]
    async fn per_chat_isolation_different_chats() {
        let (repo, _db) = setup().await;
        let plugin = seed_channel(&repo, "Telegram Stub").await;
        let user = seed_user(&repo, plugin.channel_plugin_id.as_str()).await;

        let s1 = sample_session(user.channel_user_id.as_str(), plugin.channel_plugin_id.as_str(), "chat-abc");
        repo.get_or_create_session(user.channel_user_id.as_str(), "chat-abc", plugin.channel_plugin_id.as_str(), &s1)
            .await
            .unwrap();

        let s2 = sample_session(user.channel_user_id.as_str(), plugin.channel_plugin_id.as_str(), "chat-xyz");
        repo.get_or_create_session(user.channel_user_id.as_str(), "chat-xyz", plugin.channel_plugin_id.as_str(), &s2)
            .await
            .unwrap();

        assert_eq!(repo.get_all_sessions().await.unwrap().len(), 2);
    }

    #[tokio::test]
    async fn get_session_by_business_id() {
        let (repo, _db) = setup().await;
        let plugin = seed_channel(&repo, "Telegram Stub").await;
        let user = seed_user(&repo, plugin.channel_plugin_id.as_str()).await;

        let new = sample_session(user.channel_user_id.as_str(), plugin.channel_plugin_id.as_str(), "chat-abc");
        let created = repo
            .get_or_create_session(user.channel_user_id.as_str(), "chat-abc", plugin.channel_plugin_id.as_str(), &new)
            .await
            .unwrap();

        let found = repo
            .get_session(&created.channel_session_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(found.channel_session_id, created.channel_session_id);
        assert_eq!(found.agent_type, "acp");
    }

    #[tokio::test]
    async fn get_session_not_found() {
        let (repo, _db) = setup().await;
        assert!(repo.get_session("nope").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn update_session_activity_updates_timestamp() {
        let (repo, _db) = setup().await;
        let plugin = seed_channel(&repo, "Telegram Stub").await;
        let user = seed_user(&repo, plugin.channel_plugin_id.as_str()).await;

        let new = sample_session(user.channel_user_id.as_str(), plugin.channel_plugin_id.as_str(), "chat-abc");
        let created = repo
            .get_or_create_session(user.channel_user_id.as_str(), "chat-abc", plugin.channel_plugin_id.as_str(), &new)
            .await
            .unwrap();

        let new_ts = nomifun_common::now_ms() + 5000;
        repo.update_session_activity(&created.channel_session_id, new_ts)
            .await
            .unwrap();

        let found = repo
            .get_session(&created.channel_session_id)
            .await
            .unwrap()
            .unwrap();
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
        let plugin = seed_channel(&repo, "Telegram Stub").await;
        let user = seed_user(&repo, plugin.channel_plugin_id.as_str()).await;

        let s1 = sample_session(user.channel_user_id.as_str(), plugin.channel_plugin_id.as_str(), "chat-abc");
        repo.get_or_create_session(user.channel_user_id.as_str(), "chat-abc", plugin.channel_plugin_id.as_str(), &s1)
            .await
            .unwrap();

        let s2 = sample_session(user.channel_user_id.as_str(), plugin.channel_plugin_id.as_str(), "chat-xyz");
        repo.get_or_create_session(user.channel_user_id.as_str(), "chat-xyz", plugin.channel_plugin_id.as_str(), &s2)
            .await
            .unwrap();

        repo.delete_sessions_by_user(user.channel_user_id.as_str()).await.unwrap();
        assert!(repo.get_all_sessions().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn delete_sessions_by_user_no_sessions_is_ok() {
        let (repo, _db) = setup().await;
        // No sessions exist for this user —should not error.
        repo.delete_sessions_by_user(MISSING_ID).await.unwrap();
    }

    /// Helper to create an installation-owned stub conversation for
    /// channel-session logical-reference tests. Channel sessions may point at a
    /// host-capable Conversation, so the fixture must use the one principal
    /// that is allowed to own host execution.
    async fn create_stub_conversation(pool: &SqlitePool, conv_id: &str) {
        let now = nomifun_common::now_ms();
        let installation_owner = crate::installation_owner_id(pool).await.unwrap();
        sqlx::query(
            "INSERT INTO conversations (conversation_id, user_id, name, type, created_at, updated_at) \
             VALUES (?1, ?2, 'Test Conv', 'nomi', ?3, ?3)",
        )
        .bind(conv_id)
        .bind(installation_owner)
        .bind(now)
        .execute(pool)
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn update_session_conversation_persists() {
        let conversation_id = nomifun_common::ConversationId::new().into_string();
        let (repo, db) = setup().await;
        let plugin = seed_channel(&repo, "Telegram Stub").await;
        let user = seed_user(&repo, plugin.channel_plugin_id.as_str()).await;

        let new = sample_session(user.channel_user_id.as_str(), plugin.channel_plugin_id.as_str(), "chat-abc");
        let created = repo
            .get_or_create_session(user.channel_user_id.as_str(), "chat-abc", plugin.channel_plugin_id.as_str(), &new)
            .await
            .unwrap();

        create_stub_conversation(db.pool(), &conversation_id).await;

        repo.update_session_conversation(&created.channel_session_id, &conversation_id)
            .await
            .unwrap();

        let found = repo
            .get_session(&created.channel_session_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(found.conversation_id, Some(conversation_id));
    }

    #[tokio::test]
    async fn update_session_conversation_not_found() {
        let (repo, db) = setup().await;
        let conversation_id = nomifun_common::ConversationId::new();
        create_stub_conversation(db.pool(), conversation_id.as_str()).await;
        let err = repo
            .update_session_conversation("nope", conversation_id.as_str())
            .await
            .unwrap_err();
        assert!(matches!(err, DbError::NotFound(_)));
    }

    #[tokio::test]
    async fn update_session_agent_type_persists() {
        let (repo, _db) = setup().await;
        let plugin = seed_channel(&repo, "Telegram Stub").await;
        let user = seed_user(&repo, plugin.channel_plugin_id.as_str()).await;

        let new = sample_session(user.channel_user_id.as_str(), plugin.channel_plugin_id.as_str(), "chat-abc");
        let created = repo
            .get_or_create_session(user.channel_user_id.as_str(), "chat-abc", plugin.channel_plugin_id.as_str(), &new)
            .await
            .unwrap();

        assert_eq!(
            repo.get_session(&created.channel_session_id)
                .await
                .unwrap()
                .unwrap()
                .agent_type,
            "acp"
        );

        repo.update_session_agent_type(&created.channel_session_id, "acp")
            .await
            .unwrap();

        let found = repo
            .get_session(&created.channel_session_id)
            .await
            .unwrap()
            .unwrap();
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
        let plugin = seed_channel(&repo, "Telegram Stub").await;
        let user = seed_user(&repo, plugin.channel_plugin_id.as_str()).await;

        let s1 = sample_session(user.channel_user_id.as_str(), plugin.channel_plugin_id.as_str(), "chat-abc");
        repo.get_or_create_session(user.channel_user_id.as_str(), "chat-abc", plugin.channel_plugin_id.as_str(), &s1)
            .await
            .unwrap();

        let s2 = sample_session(user.channel_user_id.as_str(), plugin.channel_plugin_id.as_str(), "chat-xyz");
        repo.get_or_create_session(user.channel_user_id.as_str(), "chat-xyz", plugin.channel_plugin_id.as_str(), &s2)
            .await
            .unwrap();

        repo.delete_session_by_user_chat(user.channel_user_id.as_str(), "chat-abc", plugin.channel_plugin_id.as_str())
            .await
            .unwrap();

        let remaining = repo.get_all_sessions().await.unwrap();
        assert_eq!(remaining.len(), 1);
        assert_eq!(remaining[0].chat_id.as_deref(), Some("chat-xyz"));
    }

    #[tokio::test]
    async fn delete_session_by_user_chat_no_match_is_ok() {
        let (repo, _db) = setup().await;
        // No sessions exist —should not error.
        repo.delete_session_by_user_chat(MISSING_ID, "chat-abc", MISSING_ID)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn same_chat_two_channels_get_isolated_sessions() {
        let (repo, _db) = setup().await;
        let first_plugin = seed_channel(&repo, "Telegram Stub A").await;
        let second_plugin = seed_channel(&repo, "Telegram Stub B").await;
        let mut unscoped = sample_user(first_plugin.channel_plugin_id.as_str());
        unscoped.channel_plugin_id = None;
        let user = repo.create_user(&unscoped).await.unwrap();

        let s1 = sample_session(user.channel_user_id.as_str(), first_plugin.channel_plugin_id.as_str(), "chat-abc");
        let first = repo
            .get_or_create_session(user.channel_user_id.as_str(), "chat-abc", first_plugin.channel_plugin_id.as_str(), &s1)
            .await
            .unwrap();

        // Same user + same chat through another bot → a second session.
        let s2 = sample_session(user.channel_user_id.as_str(), second_plugin.channel_plugin_id.as_str(), "chat-abc");
        let created = repo
            .get_or_create_session(user.channel_user_id.as_str(), "chat-abc", second_plugin.channel_plugin_id.as_str(), &s2)
            .await
            .unwrap();
        assert_eq!(created.channel_session_id, s2.channel_session_id);
        assert_eq!(repo.get_all_sessions().await.unwrap().len(), 2);

        // Reuse matches per channel.
        let reuse_candidate = sample_session(
            user.channel_user_id.as_str(),
            first_plugin.channel_plugin_id.as_str(),
            "chat-abc",
        );
        let reused = repo
            .get_or_create_session(
                user.channel_user_id.as_str(),
                "chat-abc",
                first_plugin.channel_plugin_id.as_str(),
                &reuse_candidate,
            )
            .await
            .unwrap();
        assert_eq!(reused.channel_session_id, first.channel_session_id);
    }

    #[tokio::test]
    async fn delete_sessions_by_channel_only_hits_that_channel() {
        let (repo, _db) = setup().await;
        let first_plugin = seed_channel(&repo, "Telegram Stub A").await;
        let second_plugin = seed_channel(&repo, "Telegram Stub B").await;
        let mut unscoped = sample_user(first_plugin.channel_plugin_id.as_str());
        unscoped.channel_plugin_id = None;
        let user = repo.create_user(&unscoped).await.unwrap();

        let s1 = sample_session(user.channel_user_id.as_str(), first_plugin.channel_plugin_id.as_str(), "chat-abc");
        repo.get_or_create_session(user.channel_user_id.as_str(), "chat-abc", first_plugin.channel_plugin_id.as_str(), &s1)
            .await
            .unwrap();
        let s2 = sample_session(user.channel_user_id.as_str(), second_plugin.channel_plugin_id.as_str(), "chat-abc");
        repo.get_or_create_session(user.channel_user_id.as_str(), "chat-abc", second_plugin.channel_plugin_id.as_str(), &s2)
            .await
            .unwrap();

        repo.delete_sessions_by_channel(first_plugin.channel_plugin_id.as_str())
            .await
            .unwrap();

        let remaining = repo.get_all_sessions().await.unwrap();
        assert_eq!(remaining.len(), 1);
        assert_eq!(
            remaining[0].channel_plugin_id,
            Some(second_plugin.channel_plugin_id.clone())
        );
    }

    // -- Pairing tests ------------------------------------------------

    #[tokio::test]
    async fn create_and_get_pairing() {
        let (repo, _db) = setup().await;
        let pairing = sample_pairing();
        let created = repo.create_pairing(&pairing).await.unwrap();

        let found = repo.get_pairing_by_code("123456").await.unwrap().unwrap();
        assert_eq!(found.code, created.code);
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

        let p2 = NewChannelPairingCodeRow {
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
        let expired = NewChannelPairingCodeRow {
            code: "111111".into(),
            expires_at: now - 1000,
            ..sample_pairing()
        };
        repo.create_pairing(&expired).await.unwrap();

        // Create a still-valid pairing.
        let valid = NewChannelPairingCodeRow {
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
        let approved = NewChannelPairingCodeRow {
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

use nomifun_common::now_ms;
use serde_json::Value;
use sqlx::SqlitePool;

use crate::error::DbError;
use crate::models::{CronJobRow, CronJobRunRow};
use crate::repository::bind::{BindValue, bind_value};
use crate::repository::cron::{CRON_RUN_HISTORY_LIMIT, ICronRepository, UpdateCronJobParams};

#[derive(Clone, Debug)]
pub struct SqliteCronRepository {
    pool: SqlitePool,
}

impl SqliteCronRepository {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }
}

fn validate_cron_agent_config_shape(
    agent_type: &str,
    agent_config: Option<&str>,
) -> Result<(), DbError> {
    let Some(agent_config) = agent_config else {
        return Ok(());
    };
    let config: Value = serde_json::from_str(agent_config)
        .map_err(|error| DbError::Conflict(format!("invalid cron agent_config JSON: {error}")))?;
    let object = config
        .as_object()
        .ok_or_else(|| DbError::Conflict("cron agent_config must be a JSON object".into()))?;
    const ALLOWED_FIELDS: &[&str] = &[
        "backend",
        "name",
        "cli_path",
        "custom_agent_id",
        "preset_id",
        "preset_revision",
        "preset_snapshot",
        "mode",
        "model",
        "provider_id",
        "config_options",
        "workspace",
        "clear_context_each_run",
    ];
    if let Some(field) = object
        .keys()
        .find(|field| !ALLOWED_FIELDS.contains(&field.as_str()))
    {
        return Err(DbError::Conflict(format!(
            "cron agent_config contains unknown field '{field}'"
        )));
    }
    if object.get("name").and_then(Value::as_str).is_none() {
        return Err(DbError::Conflict(
            "cron agent_config.name must be a string".into(),
        ));
    }
    for field in ["backend", "model", "provider_id"] {
        if object
            .get(field)
            .is_some_and(|value| !value.is_string() && !value.is_null())
        {
            return Err(DbError::Conflict(format!(
                "cron agent_config.{field} must be a string"
            )));
        }
    }
    if let Some(model) = object.get("model").and_then(Value::as_str)
        && (model.is_empty() || model.trim() != model)
    {
        return Err(DbError::Conflict(
            "cron agent_config.model must be a non-empty trimmed model key".into(),
        ));
    }

    if agent_type == "nomi" {
        if object.contains_key("backend") {
            return Err(DbError::Conflict(
                "cron Nomi agent_config.backend is forbidden; use provider_id".into(),
            ));
        }
        let provider_id = object
            .get("provider_id")
            .and_then(Value::as_str)
            .ok_or_else(|| {
                DbError::Conflict("cron Nomi agent_config.provider_id is required".into())
            })?;
        nomifun_common::ProviderId::parse(provider_id).map_err(|error| {
            DbError::Conflict(format!(
                "cron Nomi agent_config.provider_id is not a canonical UUIDv7: {error}"
            ))
        })?;
        if object.get("model").and_then(Value::as_str).is_none() {
            return Err(DbError::Conflict(
                "cron Nomi agent_config.model is required".into(),
            ));
        }
    } else if object.contains_key("provider_id") {
        return Err(DbError::Conflict(
            "cron agent_config.provider_id is only valid for Nomi jobs".into(),
        ));
    }
    Ok(())
}

/// Replaces the former database trigger guard for non-installation cron
/// callers. This is deliberately repository-owned logical validation: the
/// caller cannot smuggle a host-agent cron row through a lower-level write
/// path, while the schema remains trigger/FK-free.
async fn validate_cron_authority(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    user_id: &str,
    enabled: bool,
    execution_mode: &str,
    conversation_id: Option<&str>,
    agent_type: &str,
    agent_config: Option<&str>,
    preset_id: Option<&str>,
    preset_revision: Option<i64>,
    preset_snapshot: Option<&str>,
    skill_content: Option<&str>,
) -> Result<(), DbError> {
    if let Some(conversation_id) = conversation_id {
        let owned = sqlx::query(
            "UPDATE conversations SET updated_at = updated_at \
             WHERE conversation_id = ? AND user_id = ?",
        )
        .bind(conversation_id)
        .bind(user_id)
        .execute(&mut **tx)
        .await?;
        if owned.rows_affected() == 0 {
            return Err(DbError::Conflict(
                "cron job conversation owner mismatch".into(),
            ));
        }
    }

    let owner: String = sqlx::query_scalar(
        "SELECT owner_user_id FROM installation_identity \
         WHERE singleton_key = 'installation'",
    )
    .fetch_one(&mut **tx)
    .await?;
    if user_id == owner {
        return Ok(());
    }

    let model_only_error = || {
        DbError::Conflict("non-owner cron job must be model-only".into())
    };
    if agent_type != "nomi"
        || preset_id.is_some()
        || preset_revision.is_some()
        || preset_snapshot.is_some()
        || skill_content.is_some()
    {
        return Err(model_only_error());
    }

    let Some(agent_config) = agent_config else {
        if enabled && (execution_mode != "existing" || conversation_id.is_none()) {
            return Err(model_only_error());
        }
        if enabled && execution_mode == "existing" {
            let has_model: bool = sqlx::query_scalar(
                "SELECT EXISTS(\
                    SELECT 1 FROM conversations \
                    WHERE conversation_id = ? \
                      AND user_id = ? \
                      AND type = 'nomi' \
                      AND json_valid(model) \
                      AND json_type(model) = 'object' \
                      AND json_type(model, '$.provider_id') = 'text' \
                      AND trim(json_extract(model, '$.provider_id')) <> '' \
                      AND json_type(model, '$.model') = 'text' \
                      AND trim(json_extract(model, '$.model')) <> ''\
                )",
            )
            .bind(conversation_id.unwrap_or_default())
            .bind(user_id)
            .fetch_one(&mut **tx)
            .await?;
            if !has_model {
                return Err(model_only_error());
            }
        }
        return Ok(());
    };

    let config: Value = serde_json::from_str(agent_config)
        .map_err(|_| model_only_error())?;
    let Some(object) = config.as_object() else {
        return Err(model_only_error());
    };
    let allowed = [
        "provider_id",
        "name",
        "model",
        "clear_context_each_run",
    ];
    if object.keys().any(|key| !allowed.contains(&key.as_str()))
        || object
            .get("provider_id")
            .and_then(Value::as_str)
            .is_none_or(|value| value.trim().is_empty())
        || object.get("name").and_then(Value::as_str).is_none()
        || object
            .get("model")
            .and_then(Value::as_str)
            .is_none_or(|value| value.is_empty() || value.trim() != value)
        || object
            .get("clear_context_each_run")
            .is_some_and(|value| !value.is_boolean())
    {
        return Err(model_only_error());
    }
    Ok(())
}

fn nomi_provider_id(agent_type: &str, agent_config: Option<&str>) -> Result<Option<String>, DbError> {
    if agent_type != "nomi" {
        return Ok(None);
    }
    let Some(agent_config) = agent_config else {
        return Ok(None);
    };
    let config: Value = serde_json::from_str(agent_config)
        .map_err(|error| DbError::Conflict(format!("invalid cron agent_config JSON: {error}")))?;
    let Some(provider_id) = config.get("provider_id") else {
        return Ok(None);
    };
    if provider_id.is_null() {
        return Ok(None);
    }
    let provider_id = provider_id.as_str().ok_or_else(|| {
        DbError::Conflict("cron Nomi agent_config.provider_id must be a string".into())
    })?;
    let provider_id = nomifun_common::ProviderId::parse(provider_id).map_err(|error| {
        DbError::Conflict(format!(
            "cron Nomi agent_config.provider_id is not a canonical UUIDv7: {error}"
        ))
    })?;
    Ok(Some(provider_id.into_string()))
}

async fn lock_nomi_provider(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    agent_type: &str,
    agent_config: Option<&str>,
) -> Result<(), DbError> {
    let Some(provider_id) = nomi_provider_id(agent_type, agent_config)? else {
        return Ok(());
    };
    let locked = sqlx::query(
        "UPDATE providers SET updated_at = updated_at WHERE provider_id = ?",
    )
    .bind(&provider_id)
    .execute(&mut **tx)
    .await?;
    if locked.rows_affected() == 0 {
        return Err(DbError::Conflict(format!(
            "cron Nomi agent_config references missing provider '{provider_id}'"
        )));
    }
    Ok(())
}

async fn lock_preset(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    preset_id: Option<&str>,
) -> Result<(), DbError> {
    let Some(preset_id) = preset_id else {
        return Ok(());
    };
    let locked = sqlx::query(
        "UPDATE presets SET updated_at = updated_at WHERE preset_id = ?",
    )
    .bind(preset_id)
    .execute(&mut **tx)
    .await?;
    if locked.rows_affected() == 0 {
        return Err(DbError::Conflict(format!(
            "cron job preset '{preset_id}' does not exist"
        )));
    }
    Ok(())
}

#[async_trait::async_trait]
impl ICronRepository for SqliteCronRepository {
    async fn insert(&self, row: &CronJobRow) -> Result<(), DbError> {
        nomifun_common::CronJobId::parse(&row.cron_job_id).map_err(|error| {
            DbError::Conflict(format!("invalid cron_job_id: {error}"))
        })?;
        if row.max_retries < 0 {
            return Err(DbError::Conflict(
                "cron job max_retries must be non-negative".into(),
            ));
        }
        validate_cron_agent_config_shape(&row.agent_type, row.agent_config.as_deref())?;
        let mut tx = self.pool.begin().await?;
        lock_nomi_provider(&mut tx, &row.agent_type, row.agent_config.as_deref()).await?;
        lock_preset(&mut tx, row.preset_id.as_deref()).await?;
        validate_cron_authority(
            &mut tx,
            &row.user_id,
            row.enabled,
            &row.execution_mode,
            row.conversation_id.as_deref(),
            &row.agent_type,
            row.agent_config.as_deref(),
            row.preset_id.as_deref(),
            row.preset_revision,
            row.preset_snapshot.as_deref(),
            row.skill_content.as_deref(),
        )
        .await?;
        sqlx::query(
            "INSERT INTO cron_jobs (\
                cron_job_id, user_id, name, enabled, schedule_kind, schedule_value, schedule_tz, \
                schedule_description, payload_message, execution_mode, agent_config, \
                preset_id, preset_revision, preset_snapshot, \
                conversation_id, conversation_title, agent_type, created_by, \
                skill_content, description, created_at, updated_at, next_run_at, last_run_at, \
                last_status, last_error, run_count, retry_count, max_retries\
            ) VALUES (\
                ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?\
            )",
        )
        .bind(&row.cron_job_id)
        .bind(&row.user_id)
        .bind(&row.name)
        .bind(row.enabled)
        .bind(&row.schedule_kind)
        .bind(&row.schedule_value)
        .bind(&row.schedule_tz)
        .bind(&row.schedule_description)
        .bind(&row.payload_message)
        .bind(&row.execution_mode)
        .bind(&row.agent_config)
        .bind(&row.preset_id)
        .bind(row.preset_revision)
        .bind(&row.preset_snapshot)
        .bind(&row.conversation_id)
        .bind(&row.conversation_title)
        .bind(&row.agent_type)
        .bind(&row.created_by)
        .bind(&row.skill_content)
        .bind(&row.description)
        .bind(row.created_at)
        .bind(row.updated_at)
        .bind(row.next_run_at)
        .bind(row.last_run_at)
        .bind(&row.last_status)
        .bind(&row.last_error)
        .bind(row.run_count)
        .bind(row.retry_count)
        .bind(row.max_retries)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(())
    }

    async fn update(
        &self,
        user_id: &str,
        cron_job_id: &str,
        params: &UpdateCronJobParams,
    ) -> Result<(), DbError> {
        if params.max_retries.is_some_and(|value| value < 0) {
            return Err(DbError::Conflict(
                "cron job max_retries must be non-negative".into(),
            ));
        }
        let mut tx = self.pool.begin().await?;
        let existing = sqlx::query_as::<_, CronJobRow>(
            "SELECT * FROM cron_jobs WHERE cron_job_id = ? AND user_id = ?",
        )
        .bind(cron_job_id)
        .bind(user_id)
        .fetch_optional(&mut *tx)
        .await?
        .ok_or_else(|| DbError::NotFound(format!("cron job '{cron_job_id}'")))?;

        let mut set_parts: Vec<String> = Vec::new();
        let mut binds: Vec<BindValue> = Vec::new();

        macro_rules! push_str {
            ($field:ident) => {
                if let Some(ref v) = params.$field {
                    set_parts.push(concat!(stringify!($field), " = ?").to_string());
                    binds.push(BindValue::Str(v.clone()));
                }
            };
        }

        macro_rules! push_opt_str {
            ($field:ident) => {
                if let Some(ref v) = params.$field {
                    set_parts.push(concat!(stringify!($field), " = ?").to_string());
                    binds.push(BindValue::OptStr(v.clone()));
                }
            };
        }

        macro_rules! push_opt_i64 {
            ($field:ident) => {
                if let Some(ref v) = params.$field {
                    set_parts.push(concat!(stringify!($field), " = ?").to_string());
                    binds.push(BindValue::OptI64(*v));
                }
            };
        }

        macro_rules! push_i64 {
            ($field:ident) => {
                if let Some(v) = params.$field {
                    set_parts.push(concat!(stringify!($field), " = ?").to_string());
                    binds.push(BindValue::I64(v));
                }
            };
        }

        if let Some(v) = params.enabled {
            set_parts.push("enabled = ?".to_string());
            binds.push(BindValue::Bool(v));
        }

        push_str!(name);
        push_str!(schedule_kind);
        push_str!(schedule_value);
        push_opt_str!(schedule_tz);
        push_opt_str!(schedule_description);
        push_str!(payload_message);
        push_str!(execution_mode);
        push_opt_str!(agent_config);
        push_opt_str!(preset_id);
        push_opt_i64!(preset_revision);
        push_opt_str!(preset_snapshot);
        push_opt_str!(conversation_id);
        push_opt_str!(conversation_title);
        push_str!(agent_type);
        push_opt_str!(skill_content);
        push_opt_str!(description);
        push_opt_i64!(next_run_at);
        push_opt_i64!(last_run_at);
        push_opt_str!(last_status);
        push_opt_str!(last_error);
        push_i64!(run_count);
        push_i64!(retry_count);
        push_i64!(max_retries);

        if set_parts.is_empty() {
            tx.commit().await?;
            return Ok(());
        }

        let final_agent_config = match params.agent_config.as_ref() {
            Some(value) => value.as_deref(),
            None => existing.agent_config.as_deref(),
        };
        let final_preset_id = match params.preset_id.as_ref() {
            Some(value) => value.as_deref(),
            None => existing.preset_id.as_deref(),
        };
        let final_preset_revision = match params.preset_revision {
            Some(value) => value,
            None => existing.preset_revision,
        };
        let final_preset_snapshot = match params.preset_snapshot.as_ref() {
            Some(value) => value.as_deref(),
            None => existing.preset_snapshot.as_deref(),
        };
        let final_skill_content = match params.skill_content.as_ref() {
            Some(value) => value.as_deref(),
            None => existing.skill_content.as_deref(),
        };
        let final_conversation_id = match params.conversation_id.as_ref() {
            Some(value) => value.as_deref(),
            None => existing.conversation_id.as_deref(),
        };
        let final_execution_mode = params
            .execution_mode
            .as_deref()
            .unwrap_or(&existing.execution_mode);
        let final_agent_type = params.agent_type.as_deref().unwrap_or(&existing.agent_type);
        let final_enabled = params.enabled.unwrap_or(existing.enabled);
        validate_cron_agent_config_shape(final_agent_type, final_agent_config)?;
        validate_cron_authority(
            &mut tx,
            user_id,
            final_enabled,
            final_execution_mode,
            final_conversation_id,
            final_agent_type,
            final_agent_config,
            final_preset_id,
            final_preset_revision,
            final_preset_snapshot,
            final_skill_content,
        )
        .await?;
        lock_nomi_provider(&mut tx, final_agent_type, final_agent_config).await?;
        lock_preset(&mut tx, final_preset_id).await?;

        set_parts.push("updated_at = ?".to_string());
        binds.push(BindValue::I64(now_ms()));

        let sql = format!(
            "UPDATE cron_jobs SET {} WHERE cron_job_id = ? AND user_id = ?",
            set_parts.join(", ")
        );

        let mut query = sqlx::query(&sql);
        for bind in &binds {
            query = bind_value(query, bind);
        }
        query = query.bind(cron_job_id);
        query = query.bind(user_id);

        let result = query.execute(&mut *tx).await?;
        if result.rows_affected() == 0 {
            return Err(DbError::NotFound(format!("cron job '{cron_job_id}'")));
        }
        tx.commit().await?;
        Ok(())
    }

    async fn delete(&self, user_id: &str, cron_job_id: &str) -> Result<(), DbError> {
        let mut tx = self.pool.begin().await?;
        let locked = sqlx::query(
            "UPDATE cron_jobs \
             SET updated_at = updated_at \
             WHERE cron_job_id = ? AND user_id = ?",
        )
            .bind(cron_job_id)
            .bind(user_id)
            .execute(&mut *tx)
            .await?;
        if locked.rows_affected() == 0 {
            return Err(DbError::NotFound(format!("cron job '{cron_job_id}'")));
        }

        sqlx::query("UPDATE conversations SET cron_job_id = NULL WHERE cron_job_id = ?")
            .bind(cron_job_id)
            .execute(&mut *tx)
            .await?;
        sqlx::query(
            "UPDATE conversation_artifacts \
             SET cron_job_id = NULL \
             WHERE cron_job_id = ?",
        )
        .bind(cron_job_id)
        .execute(&mut *tx)
        .await?;
        sqlx::query("DELETE FROM cron_job_runs WHERE cron_job_id = ?")
            .bind(cron_job_id)
            .execute(&mut *tx)
            .await?;

        sqlx::query("DELETE FROM cron_jobs WHERE cron_job_id = ? AND user_id = ?")
            .bind(cron_job_id)
            .bind(user_id)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(())
    }

    async fn get_by_cron_job_id(
        &self,
        user_id: &str,
        cron_job_id: &str,
    ) -> Result<Option<CronJobRow>, DbError> {
        let row = sqlx::query_as::<_, CronJobRow>(
            "SELECT * FROM cron_jobs WHERE cron_job_id = ? AND user_id = ?",
        )
            .bind(cron_job_id)
            .bind(user_id)
            .fetch_optional(&self.pool)
            .await?;
        Ok(row)
    }

    async fn list_all(&self, user_id: &str) -> Result<Vec<CronJobRow>, DbError> {
        let rows = sqlx::query_as::<_, CronJobRow>(
            "SELECT * FROM cron_jobs WHERE user_id = ? ORDER BY created_at ASC",
        )
        .bind(user_id)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows)
    }

    async fn get_by_cron_job_id_for_scheduler(
        &self,
        cron_job_id: &str,
    ) -> Result<Option<CronJobRow>, DbError> {
        let row =
            sqlx::query_as::<_, CronJobRow>("SELECT * FROM cron_jobs WHERE cron_job_id = ?")
            .bind(cron_job_id)
            .fetch_optional(&self.pool)
            .await?;
        Ok(row)
    }

    async fn list_enabled_for_scheduler(&self) -> Result<Vec<CronJobRow>, DbError> {
        let rows = sqlx::query_as::<_, CronJobRow>(
            "SELECT * FROM cron_jobs WHERE enabled = 1 ORDER BY created_at ASC",
        )
        .fetch_all(&self.pool)
        .await?;
        Ok(rows)
    }

    async fn list_by_conversation(
        &self,
        user_id: &str,
        conversation_id: &str,
    ) -> Result<Vec<CronJobRow>, DbError> {
        let rows = sqlx::query_as::<_, CronJobRow>(
            "SELECT * FROM cron_jobs WHERE user_id = ? AND conversation_id = ? ORDER BY created_at ASC",
        )
        .bind(user_id)
        .bind(conversation_id)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows)
    }

    async fn delete_by_conversation(
        &self,
        user_id: &str,
        conversation_id: &str,
    ) -> Result<u64, DbError> {
        let mut tx = self.pool.begin().await?;
        let locked = sqlx::query(
            "UPDATE cron_jobs \
             SET updated_at = updated_at \
             WHERE user_id = ? AND conversation_id = ?",
        )
        .bind(user_id)
        .bind(conversation_id)
        .execute(&mut *tx)
        .await?;
        if locked.rows_affected() == 0 {
            tx.commit().await?;
            return Ok(0);
        }

        sqlx::query(
            "UPDATE conversations \
             SET cron_job_id = NULL \
             WHERE cron_job_id IN (\
                 SELECT cron_job_id FROM cron_jobs \
                 WHERE user_id = ? AND conversation_id = ?\
             )",
        )
        .bind(user_id)
        .bind(conversation_id)
        .execute(&mut *tx)
        .await?;
        sqlx::query(
            "UPDATE conversation_artifacts \
             SET cron_job_id = NULL \
             WHERE cron_job_id IN (\
                 SELECT cron_job_id FROM cron_jobs \
                 WHERE user_id = ? AND conversation_id = ?\
             )",
        )
        .bind(user_id)
        .bind(conversation_id)
        .execute(&mut *tx)
        .await?;
        sqlx::query(
            "DELETE FROM cron_job_runs \
             WHERE cron_job_id IN (\
                 SELECT cron_job_id FROM cron_jobs \
                 WHERE user_id = ? AND conversation_id = ?\
             )",
        )
        .bind(user_id)
        .bind(conversation_id)
        .execute(&mut *tx)
        .await?;

        let result = sqlx::query(
            "DELETE FROM cron_jobs WHERE user_id = ? AND conversation_id = ?",
        )
            .bind(user_id)
            .bind(conversation_id)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(result.rows_affected())
    }

    async fn insert_run_pruned(
        &self,
        user_id: &str,
        row: &CronJobRunRow,
    ) -> Result<(), DbError> {
        let mut tx = self.pool.begin().await?;

        let owned = sqlx::query(
            "UPDATE cron_jobs SET updated_at = updated_at WHERE cron_job_id = ? AND user_id = ?",
        )
        .bind(&row.cron_job_id)
        .bind(user_id)
        .execute(&mut *tx)
        .await?;
        if owned.rows_affected() == 0 {
            return Err(DbError::NotFound(format!("cron job '{}'", row.cron_job_id)));
        }

        sqlx::query(
            "INSERT INTO cron_job_runs \
                (cron_job_run_id, cron_job_id, executed_at_ms, status, created_at_ms) \
             VALUES (?, ?, ?, ?, ?)",
        )
        .bind(&row.cron_job_run_id)
        .bind(&row.cron_job_id)
        .bind(row.executed_at_ms)
        .bind(&row.status)
        .bind(row.created_at_ms)
        .execute(&mut *tx)
        .await?;

        sqlx::query(
            "DELETE FROM cron_job_runs \
             WHERE cron_job_id = ? \
             AND id NOT IN (\
                 SELECT id FROM cron_job_runs \
                 WHERE cron_job_id = ? \
                 ORDER BY executed_at_ms DESC, created_at_ms DESC, id DESC \
                 LIMIT ?\
             )",
        )
        .bind(&row.cron_job_id)
        .bind(&row.cron_job_id)
        .bind(CRON_RUN_HISTORY_LIMIT)
        .execute(&mut *tx)
        .await?;

        tx.commit().await?;
        Ok(())
    }

    async fn list_runs_by_job(
        &self,
        user_id: &str,
        cron_job_id: &str,
        limit: i64,
    ) -> Result<Vec<CronJobRunRow>, DbError> {
        let limit = limit.clamp(0, CRON_RUN_HISTORY_LIMIT);
        let rows = sqlx::query_as::<_, CronJobRunRow>(
            "SELECT run.* FROM cron_job_runs run \
             JOIN cron_jobs job ON job.cron_job_id = run.cron_job_id \
             WHERE run.cron_job_id = ? AND job.user_id = ? \
             ORDER BY run.executed_at_ms DESC, run.created_at_ms DESC, run.id DESC \
             LIMIT ?",
        )
        .bind(cron_job_id)
        .bind(user_id)
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::init_database_memory;
    use crate::models::CronJobRunRow;

    const CONVERSATION_ID: &str = "0190f5fe-7c00-7a00-8abc-012345678901";
    const OTHER_CONVERSATION_ID: &str = "0190f5fe-7c00-7a00-8abc-012345678902";
    const MISSING_CONVERSATION_ID: &str = "0190f5fe-7c00-7a00-8abc-012345678903";
    const PROVIDER_ID: &str = "0190f5fe-7c00-7a00-8abc-012345678904";
    const MISSING_PROVIDER_ID: &str = "0190f5fe-7c00-7a00-8abc-012345678905";
    const MISSING_CRON_JOB_ID: &str = "0190f5fe-7c00-7a00-8abc-012345678999";

    async fn setup() -> (SqliteCronRepository, crate::Database, String) {
        let db = init_database_memory().await.expect("init db");
        let installation_owner = crate::installation_owner_id(db.pool()).await.unwrap();
        let repo = SqliteCronRepository::new(db.pool().clone());

        // General Cron repository tests exercise the complete host-capable
        // shape, so their aggregate and target Conversation explicitly belong
        // to the installation owner seeded by the baseline migration.
        sqlx::query(
            "INSERT INTO conversations (conversation_id, user_id, name, type, created_at, updated_at) \
             VALUES (?1, ?2, 'Test Conv', 'acp', 0, 0)",
        )
        .bind(CONVERSATION_ID)
        .bind(&installation_owner)
        .execute(db.pool())
        .await
        .unwrap();

        (repo, db, installation_owner)
    }

    fn make_row(installation_owner: &str) -> CronJobRow {
        let now = now_ms();
        CronJobRow {
            id: 0,
            cron_job_id: nomifun_common::CronJobId::new().into_string(),
            user_id: installation_owner.to_owned(),
            name: "Test Job".into(),
            enabled: true,
            schedule_kind: "every".into(),
            schedule_value: "60000".into(),
            schedule_tz: None,
            schedule_description: Some("Every minute".into()),
            payload_message: "ping".into(),
            execution_mode: "existing".into(),
            agent_config: None,
            preset_id: None,
            preset_revision: None,
            preset_snapshot: None,
            conversation_id: Some(CONVERSATION_ID.into()),
            conversation_title: Some("Test Conv".into()),
            agent_type: "acp".into(),
            created_by: "user".into(),
            skill_content: None,
            description: None,
            created_at: now,
            updated_at: now,
            next_run_at: Some(now + 60_000),
            last_run_at: None,
            last_status: None,
            last_error: None,
            run_count: 0,
            retry_count: 0,
            max_retries: 3,
        }
    }

    fn make_run(cron_job_id: &str, index: i64) -> CronJobRunRow {
        CronJobRunRow {
            id: 0,
            cron_job_run_id: nomifun_common::CronJobRunId::new().into_string(),
            cron_job_id: cron_job_id.to_owned(),
            executed_at_ms: 1_000 + index,
            status: if index % 2 == 0 { "ok" } else { "error" }.to_owned(),
            created_at_ms: 2_000 + index,
        }
    }

    async fn insert_provider(db: &crate::Database, provider_id: &str) {
        sqlx::query(
            "INSERT INTO providers (\
                provider_id, platform, name, base_url, api_key_encrypted, models, enabled, \
                capabilities, created_at, updated_at\
             ) VALUES (?, 'openai', ?, 'https://example.invalid', 'encrypted', \
                       '[]', 1, '[]', 1, 1)",
        )
        .bind(provider_id)
        .bind(provider_id)
        .execute(db.pool())
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn nomi_insert_rejects_missing_or_noncanonical_provider() {
        let (repo, _db, owner) = setup().await;

        let mut missing = make_row(&owner);
        missing.agent_type = "nomi".into();
        missing.agent_config = Some(
            serde_json::json!({
                "provider_id": MISSING_PROVIDER_ID,
                "name": "Nomi",
                "model": "model"
            })
            .to_string(),
        );
        let error = repo.insert(&missing).await.unwrap_err();
        assert!(
            matches!(error, DbError::Conflict(ref message) if message.contains("missing provider")),
            "unexpected missing-provider error: {error:?}"
        );

        let mut noncanonical = make_row(&owner);
        noncanonical.agent_type = "nomi".into();
        noncanonical.agent_config = Some(
            serde_json::json!({
                "provider_id": format!("provider_{PROVIDER_ID}"),
                "name": "Nomi",
                "model": "model"
            })
            .to_string(),
        );
        let error = repo.insert(&noncanonical).await.unwrap_err();
        assert!(
            matches!(error, DbError::Conflict(ref message) if message.contains("not a canonical provider_id")),
            "unexpected noncanonical-provider error: {error:?}"
        );

        assert!(repo.list_all(&owner).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn nomi_update_rejects_missing_provider_without_partial_write() {
        let (repo, _db, owner) = setup().await;
        let row = make_row(&owner);
        let cron_job_id = row.cron_job_id.clone();
        repo.insert(&row).await.unwrap();

        let error = repo
            .update(
                &owner,
                &cron_job_id,
                &UpdateCronJobParams {
                    name: Some("must roll back".into()),
                    agent_type: Some("nomi".into()),
                    agent_config: Some(Some(
                        serde_json::json!({
                            "provider_id": MISSING_PROVIDER_ID,
                            "name": "Nomi",
                            "model": "model"
                        })
                        .to_string(),
                    )),
                    ..Default::default()
                },
            )
            .await
            .unwrap_err();
        assert!(
            matches!(error, DbError::Conflict(ref message) if message.contains("missing provider")),
            "unexpected update error: {error:?}"
        );

        let row = repo
            .get_by_cron_job_id(&owner, &cron_job_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(row.name, "Test Job");
        assert_eq!(row.agent_type, "acp");
        assert!(row.agent_config.is_none());
    }

    #[tokio::test]
    async fn provider_delete_is_restricted_by_nomi_cron_binding() {
        use crate::{IProviderRepository, SqliteProviderRepository};

        let (repo, db, owner) = setup().await;
        insert_provider(&db, PROVIDER_ID).await;
        let mut row = make_row(&owner);
        row.agent_type = "nomi".into();
        row.agent_config = Some(
            serde_json::json!({
                "provider_id": PROVIDER_ID,
                "name": "Nomi",
                "model": "model"
            })
            .to_string(),
        );
        let cron_job_id = row.cron_job_id.clone();
        repo.insert(&row).await.unwrap();

        let providers = SqliteProviderRepository::new(db.pool().clone());
        let error = providers.delete(PROVIDER_ID).await.unwrap_err();
        assert!(
            matches!(error, DbError::Conflict(ref message) if message.contains("executable Agent binding")),
            "unexpected provider delete error: {error:?}"
        );
        assert!(providers.find_by_id(PROVIDER_ID).await.unwrap().is_some());

        repo.delete(&owner, &cron_job_id).await.unwrap();
        providers.delete(PROVIDER_ID).await.unwrap();
    }

    #[tokio::test]
    async fn insert_run_pruned_keeps_latest_seven_per_job() {
        let (repo, _db, owner) = setup().await;
        let row_a = make_row(&owner);
        let job_a = row_a.cron_job_id.clone();
        repo.insert(&row_a).await.unwrap();
        let row_b = make_row(&owner);
        let job_b = row_b.cron_job_id.clone();
        repo.insert(&row_b).await.unwrap();

        for index in 0..10 {
            repo.insert_run_pruned(&owner, &make_run(&job_a, index))
                .await
                .unwrap();
        }
        for index in 0..3 {
            repo.insert_run_pruned(&owner, &make_run(&job_b, index))
                .await
                .unwrap();
        }

        let runs_a = repo.list_runs_by_job(&owner, &job_a, 20).await.unwrap();
        let runs_b = repo.list_runs_by_job(&owner, &job_b, 20).await.unwrap();

        assert_eq!(runs_a.len(), 7);
        assert_eq!(runs_a[0].executed_at_ms, 1_009);
        assert_eq!(runs_a[6].executed_at_ms, 1_003);
        assert!(runs_a.iter().all(|run| run.cron_job_id == job_a));

        assert_eq!(runs_b.len(), 3);
        assert_eq!(runs_b[0].executed_at_ms, 1_002);
        assert_eq!(runs_b[2].executed_at_ms, 1_000);
    }

    #[tokio::test]
    async fn insert_and_get_by_cron_job_id() {
        let (repo, _db, owner) = setup().await;
        let row = make_row(&owner);
        let cron_job_id = row.cron_job_id.clone();
        repo.insert(&row).await.unwrap();

        let found = repo
            .get_by_cron_job_id(&owner, &cron_job_id)
            .await
            .unwrap()
            .expect("found");
        assert!(found.id > 0);
        assert_eq!(found.cron_job_id, cron_job_id);
        assert_eq!(found.name, "Test Job");
        assert!(found.enabled);
        assert_eq!(found.schedule_kind, "every");
        assert_eq!(found.run_count, 0);
    }

    #[tokio::test]
    async fn get_by_cron_job_id_returns_none_for_missing() {
        let (repo, _db, owner) = setup().await;
        let result = repo
            .get_by_cron_job_id(&owner, MISSING_CRON_JOB_ID)
            .await
            .unwrap();
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn list_all_returns_all_rows() {
        let (repo, _db, owner) = setup().await;
        repo.insert(&make_row(&owner)).await.unwrap();
        repo.insert(&make_row(&owner)).await.unwrap();

        let all = repo.list_all(&owner).await.unwrap();
        assert_eq!(all.len(), 2);
    }

    #[tokio::test]
    async fn list_enabled_filters_disabled() {
        let (repo, _db, owner) = setup().await;
        let enabled_row = make_row(&owner);
        let enabled_id = enabled_row.cron_job_id.clone();
        repo.insert(&enabled_row).await.unwrap();

        let mut disabled = make_row(&owner);
        disabled.enabled = false;
        repo.insert(&disabled).await.unwrap();

        let enabled = repo.list_enabled_for_scheduler().await.unwrap();
        assert_eq!(enabled.len(), 1);
        assert!(enabled[0].id > 0);
        assert_eq!(enabled[0].cron_job_id, enabled_id);
    }

    #[tokio::test]
    async fn list_by_conversation_filters_correctly() {
        let (repo, db, owner) = setup().await;
        sqlx::query(
            "INSERT INTO conversations (conversation_id, user_id, name, type, created_at, updated_at) \
             VALUES (?1, ?2, 'Other', 'acp', 0, 0)",
        )
        .bind(OTHER_CONVERSATION_ID)
        .bind(&owner)
        .execute(db.pool())
        .await
        .unwrap();

        let conv1_job = make_row(&owner);
        let conv1_job_id = conv1_job.cron_job_id.clone();
        repo.insert(&conv1_job).await.unwrap();
        let mut other = make_row(&owner);
        other.conversation_id = Some(OTHER_CONVERSATION_ID.into());
        let conv2_job_id = other.cron_job_id.clone();
        repo.insert(&other).await.unwrap();

        let conv1_jobs = repo
            .list_by_conversation(&owner, CONVERSATION_ID)
            .await
            .unwrap();
        assert_eq!(conv1_jobs.len(), 1);
        assert_eq!(conv1_jobs[0].cron_job_id, conv1_job_id);

        let conv2_jobs = repo
            .list_by_conversation(&owner, OTHER_CONVERSATION_ID)
            .await
            .unwrap();
        assert_eq!(conv2_jobs.len(), 1);
        assert_eq!(conv2_jobs[0].cron_job_id, conv2_job_id);
    }

    #[tokio::test]
    async fn update_partial_fields() {
        let (repo, _db, owner) = setup().await;
        let row = make_row(&owner);
        let cron_job_id = row.cron_job_id.clone();
        repo.insert(&row).await.unwrap();

        let params = UpdateCronJobParams {
            name: Some("Renamed".into()),
            enabled: Some(false),
            run_count: Some(42),
            ..Default::default()
        };
        repo.update(&owner, &cron_job_id, &params).await.unwrap();

        let updated = repo
            .get_by_cron_job_id(&owner, &cron_job_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(updated.name, "Renamed");
        assert!(!updated.enabled);
        assert_eq!(updated.run_count, 42);
        assert!(updated.updated_at >= updated.created_at);
    }

    #[tokio::test]
    async fn update_optional_nullable_fields() {
        let (repo, _db, owner) = setup().await;
        let row = make_row(&owner);
        let cron_job_id = row.cron_job_id.clone();
        repo.insert(&row).await.unwrap();

        let params = UpdateCronJobParams {
            last_status: Some(Some("ok".into())),
            last_error: Some(Some("timeout".into())),
            skill_content: Some(Some("---\nname: skill\n---\nDo it".into())),
            ..Default::default()
        };
        repo.update(&owner, &cron_job_id, &params).await.unwrap();

        let updated = repo
            .get_by_cron_job_id(&owner, &cron_job_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(updated.last_status.as_deref(), Some("ok"));
        assert_eq!(updated.last_error.as_deref(), Some("timeout"));
        assert!(updated.skill_content.is_some());

        let clear_params = UpdateCronJobParams {
            last_status: Some(None),
            last_error: Some(None),
            skill_content: Some(None),
            ..Default::default()
        };
        repo.update(&owner, &cron_job_id, &clear_params)
            .await
            .unwrap();

        let cleared = repo
            .get_by_cron_job_id(&owner, &cron_job_id)
            .await
            .unwrap()
            .unwrap();
        assert!(cleared.last_status.is_none());
        assert!(cleared.last_error.is_none());
        assert!(cleared.skill_content.is_none());
    }

    #[tokio::test]
    async fn update_nonexistent_returns_not_found() {
        let (repo, _db, owner) = setup().await;
        let params = UpdateCronJobParams {
            name: Some("x".into()),
            ..Default::default()
        };
        let err = repo
            .update(&owner, MISSING_CRON_JOB_ID, &params)
            .await
            .unwrap_err();
        assert!(matches!(err, DbError::NotFound(_)));
    }

    #[tokio::test]
    async fn update_empty_params_is_noop() {
        let (repo, _db, owner) = setup().await;
        let row = make_row(&owner);
        let cron_job_id = row.cron_job_id.clone();
        repo.insert(&row).await.unwrap();

        let before = repo
            .get_by_cron_job_id(&owner, &cron_job_id)
            .await
            .unwrap()
            .unwrap();
        repo.update(&owner, &cron_job_id, &UpdateCronJobParams::default())
            .await
            .unwrap();
        let after = repo
            .get_by_cron_job_id(&owner, &cron_job_id)
            .await
            .unwrap()
            .unwrap();

        assert_eq!(before.updated_at, after.updated_at);
    }

    #[tokio::test]
    async fn delete_removes_row() {
        let (repo, db, owner) = setup().await;
        let row = make_row(&owner);
        let cron_job_id = row.cron_job_id.clone();
        repo.insert(&row).await.unwrap();
        sqlx::query(
            "UPDATE conversations SET cron_job_id = ? WHERE conversation_id = ?",
        )
        .bind(&cron_job_id)
        .bind(CONVERSATION_ID)
        .execute(db.pool())
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO conversation_artifacts \
                (conversation_artifact_id, conversation_id, cron_job_id, kind, payload, created_at, updated_at) \
             VALUES (?, ?, ?, 'cron_trigger', '{}', 0, 0)",
        )
        .bind(nomifun_common::generate_id())
        .bind(CONVERSATION_ID)
        .bind(&cron_job_id)
        .execute(db.pool())
        .await
        .unwrap();
        repo.insert_run_pruned(&owner, &make_run(&cron_job_id, 1))
            .await
            .unwrap();

        repo.delete(&owner, &cron_job_id).await.unwrap();
        let result = repo
            .get_by_cron_job_id(&owner, &cron_job_id)
            .await
            .unwrap();
        assert!(result.is_none());
        let conversation_job: Option<String> =
            sqlx::query_scalar("SELECT cron_job_id FROM conversations WHERE conversation_id = ?")
                .bind(CONVERSATION_ID)
                .fetch_one(db.pool())
                .await
                .unwrap();
        let artifact_job: Option<String> = sqlx::query_scalar(
            "SELECT cron_job_id FROM conversation_artifacts WHERE conversation_id = ?",
        )
        .bind(CONVERSATION_ID)
        .fetch_one(db.pool())
        .await
        .unwrap();
        let run_count: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM cron_job_runs WHERE cron_job_id = ?")
                .bind(&cron_job_id)
                .fetch_one(db.pool())
                .await
                .unwrap();
        assert!(conversation_job.is_none());
        assert!(artifact_job.is_none());
        assert_eq!(run_count, 0);
    }

    #[tokio::test]
    async fn delete_nonexistent_returns_not_found() {
        let (repo, _db, owner) = setup().await;
        let err = repo
            .delete(&owner, MISSING_CRON_JOB_ID)
            .await
            .unwrap_err();
        assert!(matches!(err, DbError::NotFound(_)));
    }

    #[tokio::test]
    async fn delete_by_conversation_removes_all_related() {
        let (repo, _db, owner) = setup().await;
        repo.insert(&make_row(&owner)).await.unwrap();
        repo.insert(&make_row(&owner)).await.unwrap();

        let deleted = repo
            .delete_by_conversation(&owner, CONVERSATION_ID)
            .await
            .unwrap();
        assert_eq!(deleted, 2);

        let remaining = repo.list_all(&owner).await.unwrap();
        assert!(remaining.is_empty());
    }

    #[tokio::test]
    async fn delete_by_conversation_returns_zero_for_no_match() {
        let (repo, _db, owner) = setup().await;
        let deleted = repo
            .delete_by_conversation(&owner, MISSING_CONVERSATION_ID)
            .await
            .unwrap();
        assert_eq!(deleted, 0);
    }

    #[tokio::test]
    async fn update_schedule_fields() {
        let (repo, _db, owner) = setup().await;
        let row = make_row(&owner);
        let cron_job_id = row.cron_job_id.clone();
        repo.insert(&row).await.unwrap();

        let params = UpdateCronJobParams {
            schedule_kind: Some("cron".into()),
            schedule_value: Some("0 0 9 * * *".into()),
            schedule_tz: Some(Some("Asia/Shanghai".into())),
            schedule_description: Some(Some("Daily at 9am".into())),
            next_run_at: Some(Some(9999999)),
            ..Default::default()
        };
        repo.update(&owner, &cron_job_id, &params).await.unwrap();

        let updated = repo
            .get_by_cron_job_id(&owner, &cron_job_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(updated.schedule_kind, "cron");
        assert_eq!(updated.schedule_value, "0 0 9 * * *");
        assert_eq!(updated.schedule_tz.as_deref(), Some("Asia/Shanghai"));
        assert_eq!(updated.next_run_at, Some(9999999));
    }

    #[tokio::test]
    async fn insert_all_schedule_kinds() {
        let (repo, _db, owner) = setup().await;

        let mut at_job = make_row(&owner);
        at_job.schedule_kind = "at".into();
        at_job.schedule_value = "1700000000000".into();
        repo.insert(&at_job).await.unwrap();

        let mut cron_job = make_row(&owner);
        cron_job.schedule_kind = "cron".into();
        cron_job.schedule_value = "0 */5 * * * *".into();
        cron_job.schedule_tz = Some("UTC".into());
        repo.insert(&cron_job).await.unwrap();

        let all = repo.list_all(&owner).await.unwrap();
        assert_eq!(all.len(), 2);
    }

    #[tokio::test]
    async fn insert_with_skill_content() {
        let (repo, _db, owner) = setup().await;
        let mut row = make_row(&owner);
        row.skill_content = Some("---\nname: My Skill\ndescription: A test\n---\nDo X".into());
        let cron_job_id = row.cron_job_id.clone();
        repo.insert(&row).await.unwrap();

        let found = repo
            .get_by_cron_job_id(&owner, &cron_job_id)
            .await
            .unwrap()
            .unwrap();
        assert!(found.skill_content.unwrap().contains("My Skill"));
    }

    #[tokio::test]
    async fn insert_with_agent_config_json() {
        let (repo, _db, owner) = setup().await;
        let mut row = make_row(&owner);
        row.agent_config = Some(r#"{"backend":"openai","name":"GPT","model":"gpt-4"}"#.into());
        let cron_job_id = row.cron_job_id.clone();
        repo.insert(&row).await.unwrap();

        let found = repo
            .get_by_cron_job_id(&owner, &cron_job_id)
            .await
            .unwrap()
            .unwrap();
        let config = found.agent_config.unwrap();
        assert!(config.contains("openai"));
        assert!(config.contains("gpt-4"));
    }

    #[tokio::test]
    async fn secondary_user_cron_accepts_model_only_shape_and_rejects_host_agent() {
        let (repo, db, owner) = setup().await;
        const SECONDARY_USER: &str = "0190f5fe-7c00-7a00-8000-000000000002";
        const PROVIDER_ID: &str = "0190f5fe-7c00-7a00-8000-000000000003";

        sqlx::query(
            "INSERT INTO users (user_id, username, password_hash, created_at, updated_at) \
             VALUES (?1, 'cron_secondary', 'hash', 0, 0)",
        )
        .bind(SECONDARY_USER)
        .execute(db.pool())
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO providers (\
                provider_id, platform, name, base_url, api_key_encrypted, models, enabled, \
                capabilities, created_at, updated_at\
             ) VALUES (\
                ?1, 'openai', 'Provider Test', 'https://example.invalid', \
                'encrypted', '[]', 1, '[]', 0, 0\
             )",
        )
        .bind(PROVIDER_ID)
        .execute(db.pool())
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO conversations \
                (conversation_id, user_id, name, type, model, delegation_policy, created_at, updated_at) \
             VALUES \
                (?1, ?2, 'Model-only target', 'nomi', ?3, \
                 'disabled', 0, 0)",
        )
        .bind(OTHER_CONVERSATION_ID)
        .bind(SECONDARY_USER)
        .bind(format!(
            "{{\"provider_id\":\"{PROVIDER_ID}\",\"model\":\"model-test\"}}"
        ))
        .execute(db.pool())
        .await
        .unwrap();

        let mut allowed = make_row(&owner);
        allowed.user_id = SECONDARY_USER.into();
        allowed.conversation_id = Some(OTHER_CONVERSATION_ID.into());
        allowed.conversation_title = Some("Model-only target".into());
        allowed.agent_type = "nomi".into();
        repo.insert(&allowed).await.unwrap();
        assert_eq!(repo.list_all(SECONDARY_USER).await.unwrap().len(), 1);

        let mut rejected = allowed;
        rejected.agent_type = "acp".into();
        let err = repo.insert(&rejected).await.unwrap_err();
        assert!(
            err.to_string().contains("non-owner cron job must be model-only"),
            "unexpected authority error: {err}"
        );
    }
}

use std::collections::HashSet;

use nomifun_common::{
    AgentId, MAX_AGENT_EXECUTION_MODELS, MAX_AGENT_EXECUTION_PARALLELISM,
    MAX_AGENT_EXECUTION_PARTICIPANTS, ProviderId, now_ms,
};
use serde_json::Value;
use sqlx::{Sqlite, SqlitePool, Transaction};

use crate::error::DbError;
use crate::models::{
    AgentExecutionTemplateDetailRows, AgentExecutionTemplateParticipantRow,
    AgentExecutionTemplateRow,
};
use crate::repository::agent_execution_template::{
    CreateAgentExecutionTemplateParams, IAgentExecutionTemplateRepository,
    NewAgentExecutionTemplateParticipant, UpdateAgentExecutionTemplateParams,
};
use crate::repository::agent_execution::validate_participant_constraints_json;

#[derive(Clone, Debug)]
pub struct SqliteAgentExecutionTemplateRepository {
    pool: SqlitePool,
}

impl SqliteAgentExecutionTemplateRepository {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }
}

fn invalid(message: impl Into<String>) -> DbError {
    DbError::Conflict(message.into())
}

fn validate_json(raw: &str, field: &str) -> Result<Value, DbError> {
    serde_json::from_str(raw).map_err(|_| invalid(format!("{field} must be valid JSON")))
}

fn validate_json_object(raw: &str, field: &str) -> Result<Value, DbError> {
    let value = validate_json(raw, field)?;
    if !value.is_object() {
        return Err(invalid(format!("{field} must be a JSON object")));
    }
    Ok(value)
}

fn validate_string_array(raw: &str, field: &str) -> Result<(), DbError> {
    let value = validate_json(raw, field)?;
    let values = value
        .as_array()
        .ok_or_else(|| invalid(format!("{field} must be a JSON array")))?;
    if values.iter().any(|value| value.as_str().is_none()) {
        return Err(invalid(format!("{field} must contain only strings")));
    }
    Ok(())
}

fn validate_participant(
    participant: &NewAgentExecutionTemplateParticipant,
) -> Result<(), DbError> {
    nomifun_common::validate_uuidv7(&participant.template_participant_id)
        .map_err(|error| invalid(format!("invalid template_participant_id: {error}")))?;
    AgentId::parse(&participant.source_agent_id)
        .map_err(|error| invalid(format!("invalid source_agent_id: {error}")))?;
    match (&participant.provider_id, &participant.model) {
        (None, None) => {}
        (Some(provider_id), Some(model))
            if ProviderId::parse(provider_id).is_ok()
                && !model.trim().is_empty()
                && model.trim() == model => {}
        _ => {
            return Err(invalid(
                "template participant provider_id and model must be a non-empty pair",
            ));
        }
    }
    match (
        &participant.preset_id,
        participant.preset_revision,
        &participant.preset_snapshot,
    ) {
        (None, None, None) => {}
        (Some(preset_id), Some(preset_revision), Some(snapshot))
            if !preset_id.trim().is_empty() && preset_revision > 0 =>
        {
            let snapshot = validate_json_object(snapshot, "template participant preset_snapshot")?;
            if snapshot.get("preset_id").and_then(Value::as_str) != Some(preset_id.as_str())
                || snapshot.get("preset_revision").and_then(Value::as_i64)
                    != Some(preset_revision)
                || snapshot.get("target").and_then(Value::as_str) != Some("execution_step")
            {
                return Err(invalid(
                    "template participant preset lineage and snapshot are inconsistent",
                ));
            }
        }
        _ => {
            return Err(invalid(
                "template participant preset lineage must be absent or complete",
            ));
        }
    }
    if let Some(capability) = &participant.capability {
        validate_json_object(capability, "template participant capability")?;
    }
    if let Some(constraints) = &participant.constraints {
        validate_participant_constraints_json(constraints)?;
    }
    validate_string_array(&participant.enabled_skills, "template participant enabled_skills")?;
    validate_string_array(
        &participant.disabled_builtin_skills,
        "template participant disabled_builtin_skills",
    )?;
    resolved_provider_model(participant)?;
    Ok(())
}

fn resolved_provider_model(
    participant: &NewAgentExecutionTemplateParticipant,
) -> Result<(String, String), DbError> {
    if let (Some(provider_id), Some(model)) = (&participant.provider_id, &participant.model) {
        return Ok((provider_id.trim().to_owned(), model.trim().to_owned()));
    }
    let resolved = participant
        .preset_snapshot
        .as_deref()
        .map(|snapshot| validate_json_object(snapshot, "template participant preset_snapshot"))
        .transpose()?
        .and_then(|snapshot| snapshot.get("resolved_model").cloned());
    let provider_id = resolved
        .as_ref()
        .and_then(|model| model.get("provider_id"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty());
    let model = resolved
        .as_ref()
        .and_then(|model| model.get("model"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty());
    match (provider_id, model) {
        (Some(provider_id), Some(model)) => Ok((provider_id.to_owned(), model.to_owned())),
        _ => Err(invalid(
            "template participant must resolve a concrete provider and model",
        )),
    }
}

fn validate_participants(
    participants: &[NewAgentExecutionTemplateParticipant],
) -> Result<(), DbError> {
    if participants.is_empty() || participants.len() > MAX_AGENT_EXECUTION_PARTICIPANTS {
        return Err(invalid(format!(
            "Agent Execution Template participants must contain 1..={MAX_AGENT_EXECUTION_PARTICIPANTS} entries"
        )));
    }
    let mut models = HashSet::new();
    for participant in participants {
        validate_participant(participant)?;
        models.insert(resolved_provider_model(participant)?);
    }
    if models.len() > MAX_AGENT_EXECUTION_MODELS {
        return Err(invalid(format!(
            "Agent Execution Template exceeds {MAX_AGENT_EXECUTION_MODELS} distinct models"
        )));
    }
    Ok(())
}

fn validate_create(params: &CreateAgentExecutionTemplateParams) -> Result<(), DbError> {
    if params.name.trim().is_empty() {
        return Err(invalid("Agent Execution Template name must not be empty"));
    }
    if params
        .max_parallel
        .is_some_and(|value| !(1..=MAX_AGENT_EXECUTION_PARALLELISM).contains(&value))
    {
        return Err(invalid(format!(
            "template max_parallel must be between 1 and {MAX_AGENT_EXECUTION_PARALLELISM}"
        )));
    }
    if let Some(context) = &params.context {
        validate_json(context, "Agent Execution Template context")?;
    }
    validate_participants(&params.participants)?;
    Ok(())
}

fn validate_update(params: &UpdateAgentExecutionTemplateParams) -> Result<(), DbError> {
    if params
        .name
        .as_ref()
        .is_some_and(|name| name.trim().is_empty())
    {
        return Err(invalid("Agent Execution Template name must not be empty"));
    }
    if params
        .max_parallel
        .is_some_and(|value| {
            value.is_some_and(|value| !(1..=MAX_AGENT_EXECUTION_PARALLELISM).contains(&value))
        })
    {
        return Err(invalid(format!(
            "template max_parallel must be between 1 and {MAX_AGENT_EXECUTION_PARALLELISM}"
        )));
    }
    if let Some(Some(context)) = &params.context {
        validate_json(context, "Agent Execution Template context")?;
    }
    if let Some(participants) = &params.participants {
        validate_participants(participants)?;
    }
    Ok(())
}

async fn insert_participants_tx(
    tx: &mut Transaction<'_, Sqlite>,
    execution_template_id: &str,
    participants: &[NewAgentExecutionTemplateParticipant],
    now: i64,
) -> Result<Vec<String>, DbError> {
    let mut participant_ids = Vec::with_capacity(participants.len());
    for participant in participants {
        let (provider_id, model) = resolved_provider_model(participant)?;
        let source_agent = sqlx::query(
            "UPDATE agent_metadata SET updated_at = updated_at WHERE agent_id = ?",
        )
        .bind(&participant.source_agent_id)
        .execute(&mut **tx)
        .await?;
        if source_agent.rows_affected() == 0 {
            return Err(invalid(format!(
                "template participant source agent '{}' does not exist",
                participant.source_agent_id
            )));
        }
        if let Some(preset_id) = participant.preset_id.as_deref() {
            let preset = sqlx::query(
                "UPDATE presets SET updated_at = updated_at WHERE preset_id = ?",
            )
            .bind(preset_id)
            .execute(&mut **tx)
            .await?;
            if preset.rows_affected() == 0 {
                return Err(invalid(format!(
                    "template participant preset '{preset_id}' does not exist"
                )));
            }
        }
        // Provider is a hard logical parent. Take SQLite's writer lock and
        // validate it in this transaction so provider deletion cannot race a
        // newly persisted Template participant. No FK/trigger is involved.
        let provider = sqlx::query(
            "UPDATE providers SET updated_at = updated_at WHERE provider_id = ?",
        )
        .bind(&provider_id)
        .execute(&mut **tx)
        .await?;
        if provider.rows_affected() == 0 {
            return Err(invalid(format!(
                "template participant provider '{provider_id}' does not exist"
            )));
        }
        sqlx::query(
            "INSERT INTO agent_execution_template_participants (\
                template_participant_id, template_id, source_agent_id, preset_id, preset_revision, preset_snapshot, \
                provider_id, model, role, capability, constraints, description, system_prompt, \
                enabled_skills, disabled_builtin_skills, sort_order, created_at, updated_at\
             ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(&participant.template_participant_id)
        .bind(execution_template_id)
        .bind(&participant.source_agent_id)
        .bind(&participant.preset_id)
        .bind(participant.preset_revision)
        .bind(&participant.preset_snapshot)
        .bind(&provider_id)
        .bind(&model)
        .bind(&participant.role)
        .bind(&participant.capability)
        .bind(&participant.constraints)
        .bind(&participant.description)
        .bind(&participant.system_prompt)
        .bind(&participant.enabled_skills)
        .bind(&participant.disabled_builtin_skills)
        .bind(participant.sort_order)
        .bind(now)
        .bind(now)
        .execute(&mut **tx)
        .await?;
        participant_ids.push(participant.template_participant_id.clone());
    }
    Ok(participant_ids)
}

async fn load_template_tx(
    tx: &mut Transaction<'_, Sqlite>,
    user_id: &str,
    execution_template_id: &str,
) -> Result<Option<AgentExecutionTemplateDetailRows>, DbError> {
    let template = sqlx::query_as::<_, AgentExecutionTemplateRow>(
        "SELECT * FROM agent_execution_templates \
         WHERE execution_template_id = ? AND user_id = ?",
    )
    .bind(execution_template_id)
    .bind(user_id)
    .fetch_optional(&mut **tx)
    .await?;
    let Some(template) = template else {
        return Ok(None);
    };
    let participants = sqlx::query_as::<_, AgentExecutionTemplateParticipantRow>(
        "SELECT * FROM agent_execution_template_participants \
         WHERE template_id = ? ORDER BY sort_order, id",
    )
    .bind(execution_template_id)
    .fetch_all(&mut **tx)
    .await?;
    Ok(Some(AgentExecutionTemplateDetailRows {
        template,
        participants,
    }))
}

#[async_trait::async_trait]
impl IAgentExecutionTemplateRepository for SqliteAgentExecutionTemplateRepository {
    async fn create_template(
        &self,
        user_id: &str,
        params: &CreateAgentExecutionTemplateParams,
    ) -> Result<AgentExecutionTemplateDetailRows, DbError> {
        validate_create(params)?;
        let execution_template_id =
            nomifun_common::AgentExecutionTemplateId::new().into_string();
        let now = now_ms();
        let mut tx = self.pool.begin().await?;
        let owner = sqlx::query("UPDATE users SET updated_at = updated_at WHERE user_id = ?")
            .bind(user_id)
            .execute(&mut *tx)
            .await?;
        if owner.rows_affected() == 0 {
            return Err(DbError::NotFound("template owner".to_owned()));
        }
        let participant_ids = insert_participants_tx(
            &mut tx,
            &execution_template_id,
            &params.participants,
            now,
        )
        .await?;
        let primary_participant_id = participant_ids
            .first()
            .expect("validated non-empty Template participants")
            .clone();
        sqlx::query(
            "INSERT INTO agent_execution_templates (\
                execution_template_id, user_id, name, description, max_parallel, work_dir, context, \
                primary_participant_id, version, created_at, updated_at\
             ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, 0, ?, ?)",
        )
        .bind(&execution_template_id)
        .bind(user_id)
        .bind(&params.name)
        .bind(&params.description)
        .bind(params.max_parallel)
        .bind(&params.work_dir)
        .bind(&params.context)
        .bind(primary_participant_id)
        .bind(now)
        .bind(now)
        .execute(&mut *tx)
        .await?;
        let result = load_template_tx(&mut tx, user_id, &execution_template_id)
            .await?
            .ok_or_else(|| invalid("created Agent Execution Template is not readable"))?;
        tx.commit().await?;
        Ok(result)
    }

    async fn get_template(
        &self,
        user_id: &str,
        template_id: &str,
    ) -> Result<Option<AgentExecutionTemplateDetailRows>, DbError> {
        let mut tx = self.pool.begin().await?;
        let result = load_template_tx(&mut tx, user_id, template_id).await?;
        tx.commit().await?;
        Ok(result)
    }

    async fn list_templates(
        &self,
        user_id: &str,
        limit: i64,
        offset: i64,
    ) -> Result<Vec<AgentExecutionTemplateRow>, DbError> {
        Ok(sqlx::query_as::<_, AgentExecutionTemplateRow>(
            "SELECT * FROM agent_execution_templates WHERE user_id = ? \
             ORDER BY updated_at DESC, execution_template_id LIMIT ? OFFSET ?",
        )
        .bind(user_id)
        .bind(limit.clamp(1, 500))
        .bind(offset.max(0))
        .fetch_all(&self.pool)
        .await?)
    }

    async fn update_template(
        &self,
        user_id: &str,
        template_id: &str,
        expected_version: i64,
        params: &UpdateAgentExecutionTemplateParams,
    ) -> Result<AgentExecutionTemplateDetailRows, DbError> {
        validate_update(params)?;
        let now = now_ms();
        let mut tx = self.pool.begin().await?;
        let locked = sqlx::query(
            "UPDATE agent_execution_templates SET updated_at = updated_at \
             WHERE execution_template_id = ? AND user_id = ?",
        )
        .bind(template_id)
        .bind(user_id)
        .execute(&mut *tx)
        .await?;
        if locked.rows_affected() == 0 {
            return Err(DbError::NotFound("Agent Execution Template".to_owned()));
        }
        let current = sqlx::query_as::<_, AgentExecutionTemplateRow>(
            "SELECT * FROM agent_execution_templates \
             WHERE execution_template_id = ? AND user_id = ?",
        )
        .bind(template_id)
        .bind(user_id)
        .fetch_optional(&mut *tx)
        .await?
        .ok_or_else(|| DbError::NotFound("Agent Execution Template".to_owned()))?;
        if current.version != expected_version {
            return Err(invalid("Agent Execution Template changed concurrently"));
        }
        let name = params.name.as_ref().unwrap_or(&current.name);
        let description = params
            .description
            .as_ref()
            .unwrap_or(&current.description);
        let max_parallel = params
            .max_parallel
            .as_ref()
            .unwrap_or(&current.max_parallel);
        let work_dir = params.work_dir.as_ref().unwrap_or(&current.work_dir);
        let context = params.context.as_ref().unwrap_or(&current.context);
        let replacement_ids = if let Some(participants) = &params.participants {
            sqlx::query(
                "DELETE FROM agent_execution_template_participants WHERE template_id = ?",
            )
            .bind(template_id)
            .execute(&mut *tx)
            .await?;
            Some(insert_participants_tx(&mut tx, template_id, participants, now).await?)
        } else {
            None
        };
        let primary_participant_id = replacement_ids
            .as_ref()
            .and_then(|ids| ids.first().cloned())
            .unwrap_or_else(|| current.primary_participant_id.clone());
        let update = sqlx::query(
            "UPDATE agent_execution_templates \
             SET name = ?, description = ?, max_parallel = ?, work_dir = ?, context = ?, \
                 primary_participant_id = ?, \
                 version = version + 1, updated_at = MAX(updated_at, ?) \
             WHERE execution_template_id = ? AND user_id = ? AND version = ?",
        )
        .bind(name)
        .bind(description)
        .bind(max_parallel)
        .bind(work_dir)
        .bind(context)
        .bind(primary_participant_id)
        .bind(now)
        .bind(template_id)
        .bind(user_id)
        .bind(expected_version)
        .execute(&mut *tx)
        .await?;
        if update.rows_affected() != 1 {
            return Err(invalid("Agent Execution Template changed concurrently"));
        }
        if replacement_ids.is_some() {
            // A selected Template is valid only while it contains the
            // Conversation's concrete lead. Replacing the participant set and
            // healing affected selections are one authoring transaction, so a
            // concurrent launcher never observes a stale selection.
            sqlx::query(
                "UPDATE conversations AS conversation \
                 SET execution_template_id = NULL, updated_at = MAX(updated_at, ?) \
                 WHERE execution_template_id = ? \
                   AND NOT EXISTS ( \
                       SELECT 1 \
                       FROM agent_execution_template_participants participant \
                       WHERE participant.template_id = ? \
                         AND participant.provider_id = \
                             json_extract(conversation.model, '$.provider_id') \
                         AND participant.model = COALESCE( \
                             json_extract(conversation.model, '$.use_model'), \
                             json_extract(conversation.model, '$.model') \
                         ) \
                   )",
            )
            .bind(now)
            .bind(template_id)
            .bind(template_id)
            .execute(&mut *tx)
            .await?;
        }
        let result = load_template_tx(&mut tx, user_id, template_id)
            .await?
            .ok_or_else(|| invalid("updated Agent Execution Template is not readable"))?;
        tx.commit().await?;
        Ok(result)
    }

    async fn delete_template(
        &self,
        user_id: &str,
        template_id: &str,
        expected_version: i64,
    ) -> Result<bool, DbError> {
        let mut tx = self.pool.begin().await?;
        let current: Option<i64> = sqlx::query_scalar(
            "UPDATE agent_execution_templates SET updated_at = updated_at \
             WHERE execution_template_id = ? AND user_id = ? \
             RETURNING version",
        )
        .bind(template_id)
        .bind(user_id)
        .fetch_optional(&mut *tx)
        .await?;
        let Some(current_version) = current else {
            return Ok(false);
        };
        if current_version != expected_version {
            return Err(invalid("Agent Execution Template changed concurrently"));
        }
        sqlx::query(
            "UPDATE conversations SET execution_template_id = NULL \
             WHERE execution_template_id = ?",
        )
        .bind(template_id)
        .execute(&mut *tx)
        .await?;
        sqlx::query(
            "DELETE FROM agent_execution_template_participants WHERE template_id = ?",
        )
        .bind(template_id)
        .execute(&mut *tx)
        .await?;
        let result = sqlx::query(
            "DELETE FROM agent_execution_templates \
             WHERE execution_template_id = ? AND user_id = ? AND version = ?",
        )
        .bind(template_id)
        .bind(user_id)
        .bind(expected_version)
        .execute(&mut *tx)
        .await?;
        if result.rows_affected() != 1 {
            return Err(invalid("Agent Execution Template changed concurrently"));
        }
        tx.commit().await?;
        Ok(true)
    }

    async fn list_templates_using_provider(
        &self,
        provider_id: &str,
    ) -> Result<Vec<(String, String)>, DbError> {
        Ok(sqlx::query_as(
            "SELECT DISTINCT template.execution_template_id, template.name \
             FROM agent_execution_template_participants participant \
             JOIN agent_execution_templates template \
               ON template.execution_template_id = participant.template_id \
             WHERE participant.provider_id = ? \
             ORDER BY template.updated_at DESC, template.execution_template_id",
        )
        .bind(provider_id)
        .fetch_all(&self.pool)
        .await?)
    }
}

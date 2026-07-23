use std::collections::HashSet;

use sqlx::{Sqlite, SqlitePool, Transaction};

use nomifun_common::{
    AgentId, CompanionId, ConversationId, CronJobId, MessageId, PaginatedResult, ProviderId,
    ProviderWithModel, PublicAgentId, RemoteAgentId, TimestampMs, validate_uuidv7,
};

use crate::error::DbError;
use crate::models::{
    ConversationArtifactRow, ConversationDeliveryReceiptRow, ConversationRow, MessageRow,
};
use crate::repository::bind::{BindValue, bind_value, bind_value_as};
use crate::repository::conversation::{
    ConversationFilters, ConversationMessageProjection, ConversationRowUpdate,
    IConversationRepository, MessageRowUpdate, MessageSearchRow, SortOrder,
    TurnArtifactMessageCommit,
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

fn artifact_commit_conflict(message: impl Into<String>) -> DbError {
    DbError::Conflict(format!("turn artifact message commit rejected: {}", message.into()))
}

fn validate_artifact_write(artifact: &ConversationArtifactRow) -> Result<(), DbError> {
    validate_uuidv7(&artifact.conversation_artifact_id).map_err(|error| {
        DbError::Conflict(format!(
            "invalid conversation_artifact_id '{}': {error}",
            artifact.conversation_artifact_id
        ))
    })?;
    ConversationId::parse(&artifact.conversation_id).map_err(|error| {
        DbError::Conflict(format!(
            "invalid artifact conversation_id '{}': {error}",
            artifact.conversation_id
        ))
    })?;
    if !matches!(artifact.kind.as_str(), "cron_trigger" | "skill_suggest") {
        return Err(DbError::Conflict(format!(
            "unsupported Conversation artifact kind '{}'",
            artifact.kind
        )));
    }
    if !matches!(
        artifact.status.as_str(),
        "active" | "pending" | "dismissed" | "saved"
    ) {
        return Err(DbError::Conflict(format!(
            "unsupported Conversation artifact status '{}'",
            artifact.status
        )));
    }
    let cron_job_id = artifact.cron_job_id.as_deref().ok_or_else(|| {
        DbError::Conflict(format!(
            "{} artifact requires a cron_job_id relation",
            artifact.kind
        ))
    })?;
    CronJobId::parse(cron_job_id).map_err(|error| {
        DbError::Conflict(format!("invalid artifact cron_job_id '{cron_job_id}': {error}"))
    })?;
    let payload: serde_json::Value = serde_json::from_str(&artifact.payload).map_err(|error| {
        DbError::Conflict(format!("artifact payload must be valid JSON: {error}"))
    })?;
    let payload = payload.as_object().ok_or_else(|| {
        DbError::Conflict("artifact payload must be a JSON object".into())
    })?;
    let payload_cron_job_id = payload
        .get("cron_job_id")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| {
            DbError::Conflict(format!(
                "{} artifact payload requires a string cron_job_id",
                artifact.kind
            ))
        })?;
    if payload_cron_job_id != cron_job_id {
        return Err(DbError::Conflict(format!(
            "{} artifact payload cron_job_id does not match its row relation",
            artifact.kind
        )));
    }
    Ok(())
}

async fn lock_required_parent(
    tx: &mut Transaction<'_, Sqlite>,
    table: &str,
    id_column: &str,
    touch_column: &str,
    value: &str,
    label: &str,
) -> Result<(), DbError> {
    let sql = format!(
        "UPDATE {table} SET {touch_column} = {touch_column} WHERE {id_column} = ?"
    );
    let locked = sqlx::query(&sql)
        .bind(value)
        .execute(&mut **tx)
        .await?;
    if locked.rows_affected() == 0 {
        return Err(DbError::Conflict(format!(
            "{label} '{value}' does not exist"
        )));
    }
    Ok(())
}

async fn lock_message_parent(
    tx: &mut Transaction<'_, Sqlite>,
    conversation_id: &str,
    message_id: &str,
    label: &str,
) -> Result<(), DbError> {
    let locked = sqlx::query(
        "UPDATE messages SET created_at = created_at \
         WHERE message_id = ? AND conversation_id = ?",
    )
    .bind(message_id)
    .bind(conversation_id)
    .execute(&mut **tx)
    .await?;
    if locked.rows_affected() == 0 {
        return Err(DbError::Conflict(format!(
            "{label} '{message_id}' does not exist in Conversation '{conversation_id}'"
        )));
    }
    Ok(())
}

async fn validate_conversation_parents(
    tx: &mut Transaction<'_, Sqlite>,
    row: &ConversationRow,
) -> Result<(), DbError> {
    lock_required_parent(tx, "users", "user_id", "updated_at", &row.user_id, "User").await?;
    if let Some(cron_job_id) = row.cron_job_id.as_deref() {
        let locked = sqlx::query(
            "UPDATE cron_jobs SET updated_at = updated_at WHERE cron_job_id = ? AND user_id = ?",
        )
        .bind(cron_job_id)
        .bind(&row.user_id)
        .execute(&mut **tx)
        .await?;
        if locked.rows_affected() == 0 {
            return Err(DbError::Conflict(format!(
                "Cron job '{cron_job_id}' does not exist or belongs to another user"
            )));
        }
    }
    if let Some(preset_id) = row.preset_id.as_deref() {
        lock_required_parent(
            tx,
            "presets",
            "preset_id",
            "updated_at",
            preset_id,
            "Preset",
        )
        .await?;
    }
    Ok(())
}

async fn ensure_messages_are_not_retained(
    tx: &mut Transaction<'_, Sqlite>,
    conversation_id: &str,
    from_created_at: Option<i64>,
    from_message_id: Option<&str>,
) -> Result<(), DbError> {
    let retained_reference_exists: bool = sqlx::query_scalar(
        "SELECT EXISTS(\
            SELECT 1 \
            FROM messages target \
            WHERE target.conversation_id = ?1 \
              AND (\
                    ?2 IS NULL \
                    OR target.created_at > ?2 \
                    OR (target.created_at = ?2 AND target.message_id >= ?3)\
              ) \
              AND (\
                    EXISTS(\
                        SELECT 1 FROM conversation_delivery_receipts receipt \
                        WHERE receipt.message_id = target.message_id\
                    ) \
                    OR EXISTS(\
                        SELECT 1 FROM message_correlations correlation \
                        WHERE correlation.message_id = target.message_id\
                    )\
              )\
         )",
    )
    .bind(conversation_id)
    .bind(from_created_at)
    .bind(from_message_id)
    .fetch_one(&mut **tx)
    .await?;
    if retained_reference_exists {
        return Err(DbError::Conflict(
            "Messages retained by delivery or projected correlation history cannot be deleted"
                .to_owned(),
        ));
    }
    Ok(())
}

fn artifact_tool_call_identity(message_type: &str, content: &serde_json::Value) -> Option<String> {
    match message_type {
        "tool_call" => content
            .get("call_id")
            .and_then(serde_json::Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_owned),
        "acp_tool_call" => content
            .get("update")
            .and_then(|update| update.get("tool_call_id"))
            .and_then(serde_json::Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_owned),
        _ => None,
    }
}

fn validate_turn_artifact_message_commit(
    turn_message_id: &str,
    message: &TurnArtifactMessageCommit,
) -> Result<String, DbError> {
    MessageId::parse(&message.message_id).map_err(|error| {
        artifact_commit_conflict(format!(
            "invalid message id '{}': {error}",
            message.message_id
        ))
    })?;
    if !matches!(message.message_type.as_str(), "tool_call" | "acp_tool_call") {
        return Err(artifact_commit_conflict(format!(
            "message '{}' has unsupported type '{}'",
            message.message_id, message.message_type
        )));
    }

    let content: serde_json::Value = serde_json::from_str(&message.content).map_err(|error| {
        artifact_commit_conflict(format!(
            "message '{}' has invalid JSON content: {error}",
            message.message_id
        ))
    })?;
    let object = content
        .as_object()
        .ok_or_else(|| {
            artifact_commit_conflict(format!(
                "message '{}' content is not an object",
                message.message_id
            ))
        })?;
    if object.get("turn_id").and_then(serde_json::Value::as_str) != Some(turn_message_id) {
        return Err(artifact_commit_conflict(format!(
            "message '{}' content belongs to another turn",
            message.message_id
        )));
    }
    if object
        .get("artifact_delivery_committed")
        .and_then(serde_json::Value::as_bool)
        != Some(true)
    {
        return Err(artifact_commit_conflict(format!(
            "message '{}' is not marked as a committed artifact delivery",
            message.message_id
        )));
    }

    let call_id = artifact_tool_call_identity(&message.message_type, &content).ok_or_else(|| {
        artifact_commit_conflict(format!(
            "message '{}' has no stable tool call identity",
            message.message_id
        ))
    })?;
    let has_delivery = match message.message_type.as_str() {
        "tool_call" => {
            object.get("status").and_then(serde_json::Value::as_str) == Some("completed")
                && object
                    .get("artifacts")
                    .and_then(serde_json::Value::as_array)
                    .is_some_and(|artifacts| !artifacts.is_empty() && artifacts.iter().all(serde_json::Value::is_object))
        }
        "acp_tool_call" => object
            .get("update")
            .and_then(serde_json::Value::as_object)
            .is_some_and(|update| {
                update.get("status").and_then(serde_json::Value::as_str) == Some("completed")
                    && update
                        .get("content")
                        .and_then(serde_json::Value::as_array)
                        .is_some_and(|items| {
                            items.iter().any(|item| {
                                matches!(
                                    item.get("type").and_then(serde_json::Value::as_str),
                                    Some("artifact" | "resource_link")
                                )
                            })
                        })
            }),
        _ => false,
    };
    if !has_delivery {
        return Err(artifact_commit_conflict(format!(
            "message '{}' is not a completed artifact-producing tool call",
            message.message_id
        )));
    }

    Ok(call_id)
}

fn validate_provisional_artifact_row(
    row: &MessageRow,
    conversation_id: &str,
    turn_message_id: &str,
    candidate: &TurnArtifactMessageCommit,
    candidate_call_id: &str,
) -> Result<(), DbError> {
    if row.conversation_id != conversation_id {
        return Err(artifact_commit_conflict(format!(
            "message '{}' belongs to another conversation",
            candidate.message_id
        )));
    }
    if row.msg_id.as_deref() != Some(turn_message_id) {
        return Err(artifact_commit_conflict(format!(
            "message '{}' belongs to another turn",
            candidate.message_id
        )));
    }
    if row.r#type != candidate.message_type {
        return Err(artifact_commit_conflict(format!(
            "message '{}' has a conflicting persisted type",
            candidate.message_id
        )));
    }
    if row.position.as_deref() != Some("left") || row.hidden {
        return Err(artifact_commit_conflict(format!(
            "message '{}' has a conflicting persisted projection",
            candidate.message_id
        )));
    }
    let existing_content: serde_json::Value = serde_json::from_str(&row.content).map_err(|error| {
        artifact_commit_conflict(format!(
            "message '{}' has invalid persisted JSON content: {error}",
            candidate.message_id
        ))
    })?;
    if existing_content.get("turn_id").and_then(serde_json::Value::as_str)
        != Some(turn_message_id)
        || artifact_tool_call_identity(&row.r#type, &existing_content).as_deref()
            != Some(candidate_call_id)
        || existing_content
            .get("artifact_delivery_committed")
            .and_then(serde_json::Value::as_bool)
            == Some(true)
    {
        return Err(artifact_commit_conflict(format!(
            "message '{}' has a conflicting persisted tool identity",
            candidate.message_id
        )));
    }
    Ok(())
}

fn finished_artifact_row_matches(
    row: &MessageRow,
    conversation_id: &str,
    turn_message_id: &str,
    candidate: &TurnArtifactMessageCommit,
) -> bool {
    row.conversation_id == conversation_id
        && row.msg_id.as_deref() == Some(turn_message_id)
        && row.r#type == candidate.message_type
        && row.content == candidate.content
        && row.position.as_deref() == Some("left")
        && row.status.as_deref() == Some("finish")
        && !row.hidden
}

async fn validate_execution_template_selection(
    tx: &mut Transaction<'_, Sqlite>,
    user_id: &str,
    template_id: &str,
    model: Option<&str>,
) -> Result<(), DbError> {
    let (provider_id, model) = model
        .and_then(effective_conversation_model_binding)
        .ok_or_else(|| {
            DbError::Conflict(
                "Conversation execution template requires a concrete lead model".to_owned(),
            )
        })?;
    let template = sqlx::query(
        "UPDATE agent_execution_templates SET updated_at = updated_at \
         WHERE execution_template_id = ? AND user_id = ?",
    )
    .bind(template_id)
    .bind(user_id)
    .execute(&mut **tx)
    .await?;
    if template.rows_affected() == 0 {
        return Err(DbError::Conflict(
            "Conversation execution template must exist and belong to the Conversation owner"
                .to_owned(),
        ));
    }
    let selectable: i64 = sqlx::query_scalar(
        "SELECT EXISTS( \
             SELECT 1 FROM agent_execution_template_participants \
             WHERE template_id = ? AND provider_id = ? AND model = ? \
         )",
    )
    .bind(template_id)
    .bind(provider_id)
    .bind(model)
    .fetch_one(&mut **tx)
    .await?;
    if selectable == 0 {
        return Err(DbError::Conflict(
            "Conversation execution template must be executable, owner-scoped, and contain the lead model"
                .to_owned(),
        ));
    }
    Ok(())
}

fn effective_conversation_model_binding(encoded: &str) -> Option<(String, String)> {
    let binding: ProviderWithModel = serde_json::from_str(encoded).ok()?;
    binding.validate().ok()?;
    let model = binding.use_model.unwrap_or_else(|| binding.model.clone());
    Some((binding.provider_id, model))
}

fn execution_model_provider_ids(encoded: &str) -> Result<Vec<String>, DbError> {
    let value: serde_json::Value = serde_json::from_str(encoded).map_err(|error| {
        DbError::Conflict(format!("Conversation execution model pool is invalid JSON: {error}"))
    })?;
    let object = value.as_object().ok_or_else(|| {
        DbError::Conflict("Conversation execution model pool must be an object".to_owned())
    })?;
    let mode = object
        .get("mode")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| {
            DbError::Conflict(
                "Conversation execution model pool requires a mode".to_owned(),
            )
        })?;
    let models: Vec<&serde_json::Value> = match mode {
        "automatic" if object.len() == 1 => Vec::new(),
        "single" if object.len() == 2 => vec![object.get("model").ok_or_else(|| {
            DbError::Conflict(
                "Conversation single execution model pool requires model".to_owned(),
            )
        })?],
        "range" if object.len() == 2 => {
            let values = object
                .get("models")
                .and_then(serde_json::Value::as_array)
                .ok_or_else(|| {
                    DbError::Conflict(
                        "Conversation execution model range requires models".to_owned(),
                    )
                })?;
            if values.is_empty() {
                return Err(DbError::Conflict(
                    "Conversation execution model range requires at least one model".to_owned(),
                ));
            }
            values.iter().collect()
        }
        "automatic" | "single" | "range" => {
            return Err(DbError::Conflict(
                "Conversation execution model pool contains unknown fields".to_owned(),
            ));
        }
        _ => {
            return Err(DbError::Conflict(format!(
                "Conversation execution model pool has unsupported mode '{mode}'"
            )));
        }
    };

    let mut provider_ids = Vec::with_capacity(models.len());
    let mut seen_models = HashSet::with_capacity(models.len());
    for model in models {
        let binding: ProviderWithModel =
            serde_json::from_value(model.clone()).map_err(|error| {
                DbError::Conflict(format!(
                    "Conversation execution model reference is invalid: {error}"
                ))
            })?;
        binding.validate().map_err(|error| {
            DbError::Conflict(format!(
                "Conversation execution model reference is invalid: {error}"
            ))
        })?;
        if !seen_models.insert((binding.provider_id.clone(), binding.model.clone())) {
            return Err(DbError::Conflict(
                "Conversation execution model pool contains a duplicate model".to_owned(),
            ));
        }
        provider_ids.push(binding.provider_id);
    }
    Ok(provider_ids)
}

async fn lock_provider_bindings(
    tx: &mut Transaction<'_, Sqlite>,
    model: Option<&str>,
    execution_model_pool: Option<&str>,
) -> Result<(), DbError> {
    let mut provider_ids = Vec::new();
    if let Some(encoded) = model {
        let (provider_id, _) = effective_conversation_model_binding(encoded).ok_or_else(|| {
            DbError::Conflict("Conversation lead model binding is invalid".to_owned())
        })?;
        provider_ids.push(provider_id);
    }
    if let Some(encoded) = execution_model_pool {
        provider_ids.extend(execution_model_provider_ids(encoded)?);
    }
    provider_ids.sort_unstable();
    provider_ids.dedup();

    for provider_id in provider_ids {
        let parent = sqlx::query(
            "UPDATE providers SET updated_at = updated_at WHERE provider_id = ?",
        )
        .bind(&provider_id)
        .execute(&mut **tx)
        .await?;
        if parent.rows_affected() == 0 {
            return Err(DbError::Conflict(format!(
                "Conversation references missing provider '{provider_id}'"
            )));
        }
    }
    Ok(())
}

fn idmm_bypass_provider_ids(encoded: &str) -> Result<Vec<String>, DbError> {
    let value: serde_json::Value = serde_json::from_str(encoded)
        .map_err(|error| DbError::Conflict(format!("IDMM config is invalid JSON: {error}")))?;
    idmm_bypass_provider_ids_from_value(&value)
}

fn idmm_bypass_provider_ids_from_value(
    value: &serde_json::Value,
) -> Result<Vec<String>, DbError> {
    let object = value
        .as_object()
        .ok_or_else(|| DbError::Conflict("IDMM config must be a JSON object".to_owned()))?;

    let mut provider_ids = Vec::new();
    for watch in ["fault_watch", "decision_watch"] {
        let Some(bypass_model) = object.get(watch).and_then(|watch| watch.get("bypass_model"))
        else {
            continue;
        };
        let bypass_model = bypass_model.as_object().ok_or_else(|| {
            DbError::Conflict(format!("IDMM {watch}.bypass_model must be an object"))
        })?;
        let Some(provider_id) = bypass_model.get("provider_id") else {
            continue;
        };
        if provider_id.is_null() {
            continue;
        }
        let provider_id = provider_id.as_str().ok_or_else(|| {
            DbError::Conflict(format!(
                "IDMM {watch}.bypass_model.provider_id must be a string"
            ))
        })?;
        ProviderId::parse(provider_id).map_err(|error| {
            DbError::Conflict(format!(
                "IDMM {watch}.bypass_model.provider_id is not canonical: {error}"
            ))
        })?;
        provider_ids.push(provider_id.to_owned());
    }
    provider_ids.sort_unstable();
    provider_ids.dedup();
    Ok(provider_ids)
}

async fn lock_provider_ids(
    tx: &mut Transaction<'_, Sqlite>,
    provider_ids: Vec<String>,
) -> Result<(), DbError> {
    for provider_id in provider_ids {
        let locked = sqlx::query(
            "UPDATE providers SET updated_at = updated_at WHERE provider_id = ?",
        )
        .bind(&provider_id)
        .execute(&mut **tx)
        .await?;
        if locked.rows_affected() == 0 {
            return Err(DbError::Conflict(format!(
                "IDMM references missing provider '{provider_id}'"
            )));
        }
    }
    Ok(())
}

async fn lock_idmm_bypass_providers(
    tx: &mut Transaction<'_, Sqlite>,
    idmm: Option<&str>,
) -> Result<(), DbError> {
    let Some(idmm) = idmm else {
        return Ok(());
    };
    lock_provider_ids(tx, idmm_bypass_provider_ids(idmm)?).await
}

fn optional_extra_id<'a>(
    object: &'a serde_json::Map<String, serde_json::Value>,
    field: &str,
) -> Result<Option<&'a str>, DbError> {
    match object.get(field) {
        None | Some(serde_json::Value::Null) => Ok(None),
        Some(serde_json::Value::String(value)) if !value.is_empty() && value.trim() == value => {
            Ok(Some(value))
        }
        Some(serde_json::Value::String(_)) => Err(DbError::Conflict(format!(
            "Conversation extra.{field} must be a non-empty trimmed string"
        ))),
        Some(_) => Err(DbError::Conflict(format!(
            "Conversation extra.{field} must be a string"
        ))),
    }
}

async fn lock_conversation_extra_references(
    tx: &mut Transaction<'_, Sqlite>,
    extra: &str,
) -> Result<(), DbError> {
    let extra: serde_json::Value = serde_json::from_str(extra)
        .map_err(|error| DbError::Conflict(format!("Conversation extra is invalid JSON: {error}")))?;
    let object = extra
        .as_object()
        .ok_or_else(|| DbError::Conflict("Conversation extra must be a JSON object".to_owned()))?;

    if let Some(remote_agent_id) = optional_extra_id(object, "remote_agent_id")? {
        let remote_agent_id = RemoteAgentId::parse(remote_agent_id).map_err(|error| {
            DbError::Conflict(format!(
                "Conversation extra.remote_agent_id is not a canonical UUIDv7: {error}"
            ))
        })?;
        lock_required_parent(
            tx,
            "remote_agents",
            "remote_agent_id",
            "updated_at",
            remote_agent_id.as_str(),
            "Remote agent",
        )
        .await?;
    }

    if let Some(agent_id) = optional_extra_id(object, "agent_id")? {
        lock_required_parent(
            tx,
            "agent_metadata",
            "agent_id",
            "updated_at",
            agent_id,
            "Agent",
        )
        .await?;
    }

    if let Some(custom_agent_id) = optional_extra_id(object, "custom_agent_id")? {
        let custom_agent_id = AgentId::parse(custom_agent_id).map_err(|error| {
            DbError::Conflict(format!(
                "Conversation extra.custom_agent_id is not a canonical UUIDv7: {error}"
            ))
        })?;
        let source: Option<String> = sqlx::query_scalar(
            "UPDATE agent_metadata SET updated_at = updated_at WHERE agent_id = ? \
             RETURNING agent_source",
        )
        .bind(custom_agent_id.as_str())
        .fetch_optional(&mut **tx)
        .await?;
        match source.as_deref() {
            None => {
                return Err(DbError::Conflict(format!(
                    "Custom agent '{}' does not exist",
                    custom_agent_id
                )));
            }
            Some("builtin" | "internal") => {
                return Err(DbError::Conflict(format!(
                    "Conversation extra.custom_agent_id '{}' is not a custom agent",
                    custom_agent_id
                )));
            }
            Some(_) => {}
        }
    }

    if let Some(companion_id) = optional_extra_id(object, "companion_id")? {
        CompanionId::parse(companion_id).map_err(|error| {
            DbError::Conflict(format!(
                "Conversation extra.companion_id is not a canonical UUIDv7: {error}"
            ))
        })?;
    }
    if let Some(public_agent_id) = optional_extra_id(object, "public_agent_id")? {
        PublicAgentId::parse(public_agent_id).map_err(|error| {
            DbError::Conflict(format!(
                "Conversation extra.public_agent_id is not a canonical UUIDv7: {error}"
            ))
        })?;
    }

    lock_provider_ids(tx, extra_idmm_bypass_provider_ids_from_value(object)?).await
}

fn extra_idmm_bypass_provider_ids_from_value(
    object: &serde_json::Map<String, serde_json::Value>,
) -> Result<Vec<String>, DbError> {
    match object.get("idmm") {
        None | Some(serde_json::Value::Null) => Ok(Vec::new()),
        Some(idmm) => idmm_bypass_provider_ids_from_value(idmm),
    }
}

#[async_trait::async_trait]
impl IConversationRepository for SqliteConversationRepository {
    // ── Conversation CRUD ───────────────────────────────────────────

    async fn get(&self, conversation_id: &str) -> Result<Option<ConversationRow>, DbError> {
        let row = sqlx::query_as::<_, ConversationRow>(
            "SELECT * FROM conversations WHERE conversation_id = ?",
        )
            .bind(conversation_id)
            .fetch_optional(&self.pool)
            .await?;

        Ok(row)
    }

    async fn create(&self, row: &ConversationRow) -> Result<String, DbError> {
        let mut tx = self.pool.begin().await?;
        validate_conversation_parents(&mut tx, row).await?;
        lock_provider_bindings(
            &mut tx,
            row.model.as_deref(),
            row.execution_model_pool.as_deref(),
        )
        .await?;
        lock_conversation_extra_references(&mut tx, &row.extra).await?;
        if let Some(template_id) = row.execution_template_id.as_deref() {
            validate_execution_template_selection(
                &mut tx,
                &row.user_id,
                template_id,
                row.model.as_deref(),
            )
            .await?;
        }
        sqlx::query(
            "INSERT INTO conversations \
                (conversation_id, user_id, name, type, extra, delegation_policy, execution_model_pool, \
                 decision_policy, execution_template_id, model, status, source, \
                 channel_chat_id, pinned, pinned_at, cron_job_id, preset_id, preset_revision, \
                 preset_snapshot, created_at, updated_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(&row.conversation_id)
        .bind(&row.user_id)
        .bind(&row.name)
        .bind(&row.r#type)
        .bind(&row.extra)
        .bind(&row.delegation_policy)
        .bind(&row.execution_model_pool)
        .bind(&row.decision_policy)
        .bind(&row.execution_template_id)
        .bind(&row.model)
        .bind(&row.status)
        .bind(&row.source)
        .bind(&row.channel_chat_id)
        .bind(row.pinned)
        .bind(row.pinned_at)
        .bind(&row.cron_job_id)
        .bind(&row.preset_id)
        .bind(row.preset_revision)
        .bind(&row.preset_snapshot)
        .bind(row.created_at)
        .bind(row.updated_at)
        .execute(&mut *tx)
        .await?;

        tx.commit().await?;
        Ok(row.conversation_id.clone())
    }

    async fn create_idempotent(
        &self,
        row: &ConversationRow,
        creation_key: &str,
    ) -> Result<(String, bool), DbError> {
        let creation_key = creation_key.trim();
        if creation_key.is_empty() {
            return Err(DbError::Conflict(
                "conversation creation key must not be empty".to_owned(),
            ));
        }
        let mut tx = self.pool.begin().await?;
        validate_conversation_parents(&mut tx, row).await?;
        lock_provider_bindings(
            &mut tx,
            row.model.as_deref(),
            row.execution_model_pool.as_deref(),
        )
        .await?;
        lock_conversation_extra_references(&mut tx, &row.extra).await?;
        if let Some(template_id) = row.execution_template_id.as_deref() {
            validate_execution_template_selection(
                &mut tx,
                &row.user_id,
                template_id,
                row.model.as_deref(),
            )
            .await?;
        }
        if let Some((conversation_id, existing_user_id)) = sqlx::query_as::<_, (String, String)>(
            "SELECT conversation_id, user_id FROM conversation_creation_keys \
             WHERE creation_key = ?",
        )
        .bind(creation_key)
        .fetch_optional(&mut *tx)
        .await?
        {
            if existing_user_id != row.user_id {
                return Err(DbError::Conflict(
                    "conversation creation key belongs to another owner".to_owned(),
                ));
            }
            tx.commit().await?;
            return Ok((conversation_id, false));
        }

        sqlx::query(
            "INSERT INTO conversations \
                (conversation_id, user_id, name, type, extra, delegation_policy, execution_model_pool, \
                 decision_policy, execution_template_id, model, status, source, \
                 channel_chat_id, pinned, pinned_at, cron_job_id, preset_id, preset_revision, \
                 preset_snapshot, created_at, updated_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(&row.conversation_id)
        .bind(&row.user_id)
        .bind(&row.name)
        .bind(&row.r#type)
        .bind(&row.extra)
        .bind(&row.delegation_policy)
        .bind(&row.execution_model_pool)
        .bind(&row.decision_policy)
        .bind(&row.execution_template_id)
        .bind(&row.model)
        .bind(&row.status)
        .bind(&row.source)
        .bind(&row.channel_chat_id)
        .bind(row.pinned)
        .bind(row.pinned_at)
        .bind(&row.cron_job_id)
        .bind(&row.preset_id)
        .bind(row.preset_revision)
        .bind(&row.preset_snapshot)
        .bind(row.created_at)
        .bind(row.updated_at)
        .execute(&mut *tx)
        .await?;
        let candidate_id = row.conversation_id.clone();
        let key_result = sqlx::query(
            "INSERT INTO conversation_creation_keys \
                (creation_key, user_id, conversation_id, created_at) \
             VALUES (?, ?, ?, ?) ON CONFLICT(creation_key) DO NOTHING",
        )
        .bind(creation_key)
        .bind(&row.user_id)
        .bind(&candidate_id)
        .bind(row.created_at)
        .execute(&mut *tx)
        .await?;
        if key_result.rows_affected() == 1 {
            tx.commit().await?;
            return Ok((candidate_id, true));
        }

        // A concurrent creator committed the same operation while this writer
        // was waiting for SQLite's write lock.  Remove the unobservable
        // candidate inside this transaction and return the committed identity.
        sqlx::query("DELETE FROM conversations WHERE conversation_id = ?")
            .bind(&candidate_id)
            .execute(&mut *tx)
            .await?;
        let (conversation_id, existing_user_id): (String, String) = sqlx::query_as(
            "SELECT conversation_id, user_id FROM conversation_creation_keys \
             WHERE creation_key = ?",
        )
        .bind(creation_key)
        .fetch_one(&mut *tx)
        .await?;
        if existing_user_id != row.user_id {
            return Err(DbError::Conflict(
                "conversation creation key belongs to another owner".to_owned(),
            ));
        }
        tx.commit().await?;
        Ok((conversation_id, false))
    }

    async fn find_by_creation_key(
        &self,
        user_id: &str,
        creation_key: &str,
    ) -> Result<Option<ConversationRow>, DbError> {
        Ok(sqlx::query_as::<_, ConversationRow>(
            "SELECT conversation.* FROM conversations conversation \
             JOIN conversation_creation_keys operation \
               ON operation.conversation_id = conversation.conversation_id \
             WHERE operation.creation_key = ? AND operation.user_id = ? \
               AND conversation.user_id = ?",
        )
        .bind(creation_key)
        .bind(user_id)
        .bind(user_id)
        .fetch_optional(&self.pool)
        .await?)
    }

    async fn claim_delivery_receipt(
        &self,
        user_id: &str,
        conversation_id: &str,
        operation_id: &str,
        kind: &str,
        request_payload: &str,
        now: i64,
    ) -> Result<ConversationDeliveryReceiptRow, DbError> {
        let mut tx = self.pool.begin().await?;
        let message_id = MessageId::new().into_string();
        sqlx::query(
            "INSERT INTO conversation_delivery_receipts (\
                operation_id, message_id, conversation_id, user_id, kind, request_payload, status, \
                created_at, updated_at\
             ) SELECT ?, ?, conversation.conversation_id, ?, ?, ?, 'accepted', ?, ? \
               FROM conversations conversation \
              WHERE conversation.conversation_id = ? AND conversation.user_id = ? \
             ON CONFLICT(operation_id) DO NOTHING",
        )
        .bind(operation_id)
        .bind(message_id)
        .bind(user_id)
        .bind(kind)
        .bind(request_payload)
        .bind(now)
        .bind(now)
        .bind(conversation_id)
        .bind(user_id)
        .execute(&mut *tx)
        .await?;
        let receipt = sqlx::query_as::<_, ConversationDeliveryReceiptRow>(
            "SELECT * FROM conversation_delivery_receipts WHERE operation_id = ?",
        )
        .bind(operation_id)
        .fetch_optional(&mut *tx)
        .await?
        .ok_or_else(|| DbError::NotFound("conversation delivery owner".to_owned()))?;
        if receipt.user_id != user_id
            || receipt.conversation_id != conversation_id
            || receipt.kind != kind
            || receipt.request_payload != request_payload
        {
            return Err(DbError::Conflict(
                "conversation delivery operation identity was reused".to_owned(),
            ));
        }
        tx.commit().await?;
        Ok(receipt)
    }

    async fn get_delivery_receipt(
        &self,
        user_id: &str,
        conversation_id: &str,
        operation_id: &str,
    ) -> Result<Option<ConversationDeliveryReceiptRow>, DbError> {
        Ok(sqlx::query_as::<_, ConversationDeliveryReceiptRow>(
            "SELECT * FROM conversation_delivery_receipts \
             WHERE operation_id = ? AND conversation_id = ? AND user_id = ?",
        )
        .bind(operation_id)
        .bind(conversation_id)
        .bind(user_id)
        .fetch_optional(&self.pool)
        .await?)
    }

    async fn complete_delivery_receipt(
        &self,
        user_id: &str,
        conversation_id: &str,
        operation_id: &str,
        result_ok: bool,
        result_text: Option<&str>,
        result_error: Option<&str>,
        completed_at: i64,
    ) -> Result<bool, DbError> {
        let result = sqlx::query(
            "UPDATE conversation_delivery_receipts \
             SET status = 'completed', result_ok = ?, result_text = ?, result_error = ?, \
                 completed_at = MAX(created_at, updated_at, ?), \
                 updated_at = MAX(created_at, updated_at, ?) \
             WHERE operation_id = ? AND conversation_id = ? AND user_id = ? \
               AND status = 'accepted'",
        )
        .bind(result_ok)
        .bind(result_text)
        .bind(result_error)
        .bind(completed_at)
        .bind(completed_at)
        .bind(operation_id)
        .bind(conversation_id)
        .bind(user_id)
        .execute(&self.pool)
        .await?;
        if result.rows_affected() == 1 {
            return Ok(true);
        }
        let existing = self
            .get_delivery_receipt(user_id, conversation_id, operation_id)
            .await?;
        Ok(existing.is_some_and(|receipt| {
            receipt.status == "completed"
                && receipt.result_ok == Some(result_ok)
                && receipt.result_text.as_deref() == result_text
                && receipt.result_error.as_deref() == result_error
        }))
    }

    async fn project_assistant_message_with_receipt(
        &self,
        user_id: &str,
        conversation_id: &str,
        operation_id: &str,
        kind: &str,
        request_payload: &str,
        message: &MessageRow,
        now: i64,
    ) -> Result<ConversationMessageProjection, DbError> {
        let valid_content = serde_json::from_str::<serde_json::Value>(&message.content)
            .is_ok_and(|value| value.is_object());
        if operation_id.trim().is_empty()
            || kind != "projection"
            || request_payload.trim().is_empty()
            || MessageId::parse(&message.message_id).is_err()
            || message.content.trim().is_empty()
            || !valid_content
            || message.r#type != "text"
            || message.position.as_deref() != Some("left")
            || message.status.as_deref() != Some("finish")
            || message.hidden
            || message.msg_id.as_deref() != Some(message.message_id.as_str())
        {
            return Err(DbError::Conflict(
                "assistant message projection requires one stable finished left-side text message"
                    .to_owned(),
            ));
        }
        if message.conversation_id != conversation_id {
            return Err(DbError::Conflict(
                "projected message does not belong to the target Conversation".to_owned(),
            ));
        }

        let mut tx = self.pool.begin().await?;
        let owned = sqlx::query(
            "UPDATE conversations SET updated_at = updated_at \
             WHERE conversation_id = ? AND user_id = ?",
        )
        .bind(conversation_id)
        .bind(user_id)
        .execute(&mut *tx)
        .await?;
        if owned.rows_affected() == 0 {
            return Err(DbError::NotFound("conversation".to_owned()));
        }

        // Claim the operation directly in its terminal state. The message is
        // inserted later in this same transaction, so either both facts commit
        // or neither one is externally visible.
        let claim = sqlx::query(
            "INSERT INTO conversation_delivery_receipts (\
                operation_id, message_id, conversation_id, user_id, kind, request_payload, status, \
                result_ok, result_text, result_error, created_at, updated_at, completed_at\
             ) VALUES (?, ?, ?, ?, ?, ?, 'completed', 1, ?, NULL, ?, ?, ?) \
             ON CONFLICT(operation_id) DO NOTHING",
        )
        .bind(operation_id)
        .bind(&message.message_id)
        .bind(conversation_id)
        .bind(user_id)
        .bind(kind)
        .bind(request_payload)
        .bind(&message.message_id)
        .bind(now)
        .bind(now)
        .bind(now)
        .execute(&mut *tx)
        .await?;

        if claim.rows_affected() == 1 {
            sqlx::query(
                "INSERT INTO messages \
                    (message_id, conversation_id, msg_id, type, content, position, \
                     status, hidden, created_at) \
                 VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
            )
            .bind(&message.message_id)
            .bind(&message.conversation_id)
            .bind(&message.msg_id)
            .bind(&message.r#type)
            .bind(&message.content)
            .bind(&message.position)
            .bind(&message.status)
            .bind(message.hidden)
            .bind(message.created_at)
            .execute(&mut *tx)
            .await?;
            let persisted = sqlx::query_as::<_, MessageRow>(
                "SELECT * FROM messages WHERE message_id = ? AND conversation_id = ?",
            )
            .bind(&message.message_id)
            .bind(conversation_id)
            .fetch_one(&mut *tx)
            .await?;
            tx.commit().await?;
            return Ok(ConversationMessageProjection {
                inserted: true,
                message: persisted,
            });
        }

        let receipt = sqlx::query_as::<_, ConversationDeliveryReceiptRow>(
            "SELECT * FROM conversation_delivery_receipts WHERE operation_id = ?",
        )
        .bind(operation_id)
        .fetch_one(&mut *tx)
        .await?;
        if receipt.user_id != user_id
            || receipt.conversation_id != conversation_id
            || receipt.kind != kind
            || receipt.request_payload != request_payload
        {
            return Err(DbError::Conflict(
                "conversation projection operation identity was reused".to_owned(),
            ));
        }
        let message_id = Some(receipt.message_id.as_str()).filter(|_| {
            receipt.status == "completed"
                && receipt.result_ok == Some(true)
                && receipt.result_error.is_none()
        });
        let Some(message_id) = message_id else {
            return Err(DbError::Conflict(
                "conversation projection operation is not a completed message projection"
                    .to_owned(),
            ));
        };
        let persisted = sqlx::query_as::<_, MessageRow>(
            "SELECT * FROM messages WHERE message_id = ? AND conversation_id = ?",
        )
        .bind(message_id)
        .bind(conversation_id)
        .fetch_optional(&mut *tx)
        .await?
        .ok_or_else(|| {
            DbError::Conflict(
                "conversation projection receipt has no durable message".to_owned(),
            )
        })?;
        tx.commit().await?;
        Ok(ConversationMessageProjection {
            inserted: false,
            message: persisted,
        })
    }

    async fn update(
        &self,
        conversation_id: &str,
        updates: &ConversationRowUpdate,
    ) -> Result<(), DbError> {
        let mut tx = self.pool.begin().await?;
        let conversation = sqlx::query(
            "UPDATE conversations SET updated_at = updated_at WHERE conversation_id = ?",
        )
        .bind(conversation_id)
        .execute(&mut *tx)
        .await?;
        if conversation.rows_affected() == 0 {
            return Err(DbError::NotFound(format!(
                "Conversation '{conversation_id}' not found"
            )));
        }
        let touches_logical_references = updates.execution_template_id.is_some()
            || updates.model.is_some()
            || updates.execution_model_pool.is_some()
            || updates.cron_job_id.is_some()
            || updates.preset_id.is_some();
        if touches_logical_references {
            let current: Option<(
                String,
                Option<String>,
                Option<String>,
                Option<String>,
                Option<String>,
                Option<String>,
            )> = sqlx::query_as(
                "SELECT user_id, execution_template_id, model, execution_model_pool, \
                        cron_job_id, preset_id \
                 FROM conversations WHERE conversation_id = ?",
            )
            .bind(conversation_id)
            .fetch_optional(&mut *tx)
            .await?;
            if let Some((
                user_id,
                current_template_id,
                current_model,
                current_execution_model_pool,
                current_cron_job_id,
                current_preset_id,
            )) = current
            {
                let template_id = updates
                    .execution_template_id
                    .as_ref()
                    .cloned()
                    .unwrap_or(current_template_id);
                let model = updates.model.as_ref().cloned().unwrap_or(current_model);
                let execution_model_pool = updates
                    .execution_model_pool
                    .as_ref()
                    .cloned()
                    .unwrap_or(current_execution_model_pool);
                let cron_job_id = updates
                    .cron_job_id
                    .as_ref()
                    .cloned()
                    .unwrap_or(current_cron_job_id);
                let preset_id = updates
                    .preset_id
                    .as_ref()
                    .cloned()
                    .unwrap_or(current_preset_id);

                lock_provider_bindings(
                    &mut tx,
                    model.as_deref(),
                    execution_model_pool.as_deref(),
                )
                .await?;
                if let Some(cron_job_id) = cron_job_id.as_deref() {
                    let cron = sqlx::query(
                        "UPDATE cron_jobs SET updated_at = updated_at \
                         WHERE cron_job_id = ? AND user_id = ?",
                    )
                    .bind(cron_job_id)
                    .bind(&user_id)
                    .execute(&mut *tx)
                    .await?;
                    if cron.rows_affected() == 0 {
                        return Err(DbError::Conflict(format!(
                            "Cron job '{cron_job_id}' does not exist or belongs to another user"
                        )));
                    }
                }
                if let Some(preset_id) = preset_id.as_deref() {
                    lock_required_parent(
                        &mut tx,
                        "presets",
                        "preset_id",
                        "updated_at",
                        preset_id,
                        "Preset",
                    )
                    .await?;
                }
                if let Some(template_id) = template_id.as_deref() {
                    validate_execution_template_selection(
                        &mut tx,
                        &user_id,
                        template_id,
                        model.as_deref(),
                    )
                    .await?;
                }
            }
        }
        if let Some(extra) = updates.extra.as_deref() {
            lock_conversation_extra_references(&mut tx, extra).await?;
        }
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
        if let Some(ref delegation_policy) = updates.delegation_policy {
            set_parts.push("delegation_policy = ?".to_string());
            binds.push(BindValue::Str(delegation_policy.clone()));
        }
        if let Some(ref execution_model_pool) = updates.execution_model_pool {
            set_parts.push("execution_model_pool = ?".to_string());
            binds.push(BindValue::OptStr(execution_model_pool.clone()));
        }
        if let Some(ref decision_policy) = updates.decision_policy {
            set_parts.push("decision_policy = ?".to_string());
            binds.push(BindValue::Str(decision_policy.clone()));
        }
        if let Some(ref execution_template_id) = updates.execution_template_id {
            set_parts.push("execution_template_id = ?".to_string());
            binds.push(BindValue::OptStr(execution_template_id.clone()));
        }
        if let Some(ref status) = updates.status {
            set_parts.push("status = ?".to_string());
            binds.push(BindValue::Str(status.clone()));
        }
        if let Some(cron_job_id) = updates.cron_job_id.as_ref() {
            set_parts.push("cron_job_id = ?".to_string());
            binds.push(BindValue::OptStr(cron_job_id.clone()));
        }
        if let Some(ref preset_id) = updates.preset_id {
            set_parts.push("preset_id = ?".to_string());
            binds.push(BindValue::OptStr(preset_id.clone()));
        }
        if let Some(ref preset_revision) = updates.preset_revision {
            set_parts.push("preset_revision = ?".to_string());
            binds.push(BindValue::OptI64(*preset_revision));
        }
        if let Some(ref preset_snapshot) = updates.preset_snapshot {
            set_parts.push("preset_snapshot = ?".to_string());
            binds.push(BindValue::OptStr(preset_snapshot.clone()));
        }
        if let Some(updated_at) = updates.updated_at {
            set_parts.push("updated_at = ?".to_string());
            binds.push(BindValue::I64(updated_at));
        }

        if set_parts.is_empty() {
            tx.commit().await?;
            return Ok(());
        }

        let sql = format!("UPDATE conversations SET {} WHERE conversation_id = ?", set_parts.join(", "));

        let mut query = sqlx::query(&sql);
        for bind in &binds {
            query = bind_value(query, bind);
        }
        query = query.bind(conversation_id);

        let result = query.execute(&mut *tx).await?;

        if result.rows_affected() == 0 {
            return Err(DbError::NotFound(format!(
                "Conversation '{conversation_id}' not found"
            )));
        }

        tx.commit().await?;
        Ok(())
    }

    async fn update_idmm(
        &self,
        conversation_id: &str,
        idmm: Option<&str>,
    ) -> Result<(), DbError> {
        let mut tx = self.pool.begin().await?;
        let locked = sqlx::query(
            "UPDATE conversations SET updated_at = updated_at WHERE conversation_id = ?",
        )
        .bind(conversation_id)
        .execute(&mut *tx)
        .await?;
        if locked.rows_affected() == 0 {
            return Err(DbError::NotFound(format!(
                "Conversation '{conversation_id}' not found"
            )));
        }

        let existing_extra: String = sqlx::query_scalar(
            "SELECT extra FROM conversations WHERE conversation_id = ?",
        )
        .bind(conversation_id)
        .fetch_one(&mut *tx)
        .await?;
        let mut extra = serde_json::from_str::<serde_json::Value>(&existing_extra)
            .map_err(|error| DbError::Conflict(format!("Conversation extra is invalid JSON: {error}")))?;
        let extra_object = extra.as_object_mut().ok_or_else(|| {
            DbError::Conflict("Conversation extra must be a JSON object".to_owned())
        })?;

        let idmm_value = match idmm {
            Some(encoded) => {
                let value: serde_json::Value = serde_json::from_str(encoded).map_err(|error| {
                    DbError::Conflict(format!("IDMM config is invalid JSON: {error}"))
                })?;
                lock_idmm_bypass_providers(&mut tx, Some(encoded)).await?;
                Some(value)
            }
            None => None,
        };
        match idmm_value {
            Some(value) => {
                extra_object.insert("idmm".to_owned(), value);
            }
            None => {
                extra_object.remove("idmm");
            }
        }
        let merged_extra = serde_json::to_string(&extra)
            .map_err(|error| DbError::Conflict(format!("Conversation extra is invalid: {error}")))?;
        lock_conversation_extra_references(&mut tx, &merged_extra).await?;
        sqlx::query(
            "UPDATE conversations SET extra = ?, updated_at = ? WHERE conversation_id = ?",
        )
        .bind(merged_extra)
        .bind(nomifun_common::now_ms())
        .bind(conversation_id)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(())
    }

    async fn delete(&self, conversation_id: &str) -> Result<(), DbError> {
        self.delete_with_cleanup(conversation_id).await?;
        Ok(())
    }

    async fn delete_with_cleanup(
        &self,
        conversation_id: &str,
    ) -> Result<Vec<String>, DbError> {
        let mut tx = self.pool.begin().await?;
        let locked = sqlx::query(
            "UPDATE conversations \
             SET updated_at = updated_at \
             WHERE conversation_id = ?",
        )
        .bind(conversation_id)
        .execute(&mut *tx)
        .await?;
        if locked.rows_affected() == 0 {
            return Err(DbError::NotFound(format!(
                "Conversation '{conversation_id}' not found"
            )));
        }

        // These rows are registered KEEP_HISTORY. Since Conversation has no
        // tombstone row, hard deletion must be restricted rather than leave a
        // dangling historical identity.
        let historical_reference_exists: bool = sqlx::query_scalar(
            "SELECT \
                EXISTS(\
                    SELECT 1 FROM agent_execution_events \
                    WHERE actor_conversation_id = ?1\
                ) \
                OR EXISTS(\
                    SELECT 1 FROM conversation_execution_links \
                    WHERE conversation_id = ?1\
                ) \
                OR EXISTS(\
                    SELECT 1 FROM conversation_delivery_receipts \
                    WHERE conversation_id = ?1\
                )",
        )
        .bind(conversation_id)
        .fetch_one(&mut *tx)
        .await?;
        if historical_reference_exists {
            return Err(DbError::Conflict(format!(
                "Conversation '{conversation_id}' is retained by execution or delivery history"
            )));
        }

        // Registry-owned SET_NULL references.
        sqlx::query(
            "UPDATE channel_sessions \
             SET conversation_id = NULL \
             WHERE conversation_id = ?",
        )
        .bind(conversation_id)
        .execute(&mut *tx)
        .await?;
        sqlx::query(
            "UPDATE requirements \
             SET status = CASE WHEN status = 'in_progress' THEN 'pending' ELSE status END, \
                 active_turn_started_at = CASE \
                     WHEN status = 'in_progress' THEN NULL \
                     ELSE active_turn_started_at \
                 END, \
                 lease_expires_at = CASE \
                     WHEN status = 'in_progress' THEN NULL \
                     ELSE lease_expires_at \
                 END, \
                 owner_conversation_id = NULL \
             WHERE owner_conversation_id = ?",
        )
        .bind(conversation_id)
        .execute(&mut *tx)
        .await?;

        // Registry-owned CASCADE references. Delete grandchildren first.
        let deleted_cron_job_ids = sqlx::query_scalar::<_, String>(
            "SELECT cron_job_id FROM cron_jobs \
             WHERE conversation_id = ? \
             ORDER BY cron_job_id",
        )
        .bind(conversation_id)
        .fetch_all(&mut *tx)
        .await?;
        sqlx::query(
            "UPDATE conversations \
             SET cron_job_id = NULL \
             WHERE cron_job_id IN (\
                 SELECT cron_job_id FROM cron_jobs \
                 WHERE conversation_id = ?\
             )",
        )
        .bind(conversation_id)
        .execute(&mut *tx)
        .await?;
        sqlx::query(
            "UPDATE conversation_artifacts \
             SET cron_job_id = NULL \
             WHERE cron_job_id IN (\
                 SELECT cron_job_id FROM cron_jobs \
                 WHERE conversation_id = ?\
             )",
        )
        .bind(conversation_id)
        .execute(&mut *tx)
        .await?;
        sqlx::query(
            "DELETE FROM cron_job_runs \
             WHERE cron_job_id IN (\
                 SELECT cron_job_id FROM cron_jobs \
                 WHERE conversation_id = ?\
             )",
        )
        .bind(conversation_id)
        .execute(&mut *tx)
        .await?;
        sqlx::query("DELETE FROM cron_jobs WHERE conversation_id = ?")
            .bind(conversation_id)
            .execute(&mut *tx)
            .await?;
        sqlx::query(
            "DELETE FROM knowledge_binding_bases \
             WHERE knowledge_binding_id IN (\
                SELECT knowledge_binding_id FROM knowledge_bindings \
                WHERE target_conversation_id = ?\
             )",
        )
        .bind(conversation_id)
        .execute(&mut *tx)
        .await?;
        sqlx::query("DELETE FROM knowledge_bindings WHERE target_conversation_id = ?")
            .bind(conversation_id)
            .execute(&mut *tx)
            .await?;
        sqlx::query("DELETE FROM message_correlations WHERE conversation_id = ?")
            .bind(conversation_id)
            .execute(&mut *tx)
            .await?;
        sqlx::query("DELETE FROM conversation_mcp_servers WHERE conversation_id = ?")
            .bind(conversation_id)
            .execute(&mut *tx)
            .await?;
        sqlx::query("DELETE FROM conversation_artifacts WHERE conversation_id = ?")
            .bind(conversation_id)
            .execute(&mut *tx)
            .await?;
        sqlx::query("DELETE FROM messages WHERE conversation_id = ?")
            .bind(conversation_id)
            .execute(&mut *tx)
            .await?;
        sqlx::query("DELETE FROM conversation_creation_keys WHERE conversation_id = ?")
            .bind(conversation_id)
            .execute(&mut *tx)
            .await?;
        sqlx::query("DELETE FROM acp_session WHERE conversation_id = ?")
            .bind(conversation_id)
            .execute(&mut *tx)
            .await?;
        sqlx::query(
            "DELETE FROM idmm_interventions \
             WHERE target_kind = 'conversation' AND target_id = ?",
        )
        .bind(conversation_id)
        .execute(&mut *tx)
        .await?;

        let result = sqlx::query("DELETE FROM conversations WHERE conversation_id = ?")
            .bind(conversation_id)
            .execute(&mut *tx)
            .await?;

        if result.rows_affected() == 0 {
            return Err(DbError::NotFound(format!(
                "Conversation '{conversation_id}' not found"
            )));
        }

        tx.commit().await?;
        Ok(deleted_cron_job_ids)
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
        if let Some(cursor_id) = &filters.cursor {
            where_parts.push(
                "(c.updated_at < (SELECT updated_at FROM conversations WHERE conversation_id = ?) \
                 OR (c.updated_at = (SELECT updated_at FROM conversations WHERE conversation_id = ?) \
                     AND c.id < (SELECT id FROM conversations WHERE conversation_id = ?)))"
                    .to_string(),
            );
            binds.push(BindValue::Str(cursor_id.clone()));
            binds.push(BindValue::Str(cursor_id.clone()));
            binds.push(BindValue::Str(cursor_id.clone()));
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

    async fn list_by_cron_job(
        &self,
        user_id: &str,
        cron_job_id: &str,
    ) -> Result<Vec<ConversationRow>, DbError> {
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

    async fn list_associated(
        &self,
        user_id: &str,
        conversation_id: &str,
    ) -> Result<Vec<ConversationRow>, DbError> {
        // First get the target conversation's workspace
        let target = sqlx::query_as::<_, ConversationRow>("SELECT * FROM conversations WHERE conversation_id = ? AND user_id = ?")
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
               AND conversation_id != ? \
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

    async fn list_conversations_using_model_provider(
        &self,
        provider_id: &str,
    ) -> Result<Vec<(String, String)>, DbError> {
        Ok(sqlx::query_as(
            "SELECT conversation_id, name FROM conversations \
             WHERE model IS NOT NULL AND json_valid(model) \
               AND json_extract(model, '$.provider_id') = ? \
             ORDER BY updated_at DESC, conversation_id",
        )
        .bind(provider_id)
        .fetch_all(&self.pool)
        .await?)
    }

    // ── Message operations ──────────────────────────────────────────

    async fn get_messages(
        &self,
        conversation_id: &str,
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
        .bind(conversation_id)
        .fetch_one(&self.pool)
        .await?;
        let total = count_row.0 as u64;

        let sql = format!(
            "SELECT * FROM messages \
             WHERE conversation_id = ? \
               AND type NOT IN ('cron_trigger', 'skill_suggest') \
             ORDER BY created_at {}, message_id {} \
             LIMIT ? OFFSET ?",
            order.as_sql(),
            order.as_sql()
        );

        let mut rows = sqlx::query_as::<_, MessageRow>(&sql)
            .bind(conversation_id)
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
        conversation_id: &str,
        before: Option<(i64, String)>,
        limit: u32,
    ) -> Result<PaginatedResult<MessageRow>, DbError> {
        let effective_limit = if limit == 0 { 40 } else { limit };
        let fetch_limit = effective_limit + 1;

        // Newest-first window; the UUIDv7 `message_id` is the stable keyset
        // tiebreaker for rows sharing a `created_at` millisecond.
        let mut rows = if let Some((before_created_at, before_id)) = before {
            sqlx::query_as::<_, MessageRow>(
                "SELECT * FROM messages \
                 WHERE conversation_id = ? \
                   AND type NOT IN ('cron_trigger', 'skill_suggest') \
                   AND (created_at < ? OR (created_at = ? AND message_id < ?)) \
                 ORDER BY created_at DESC, message_id DESC \
                 LIMIT ?",
            )
            .bind(conversation_id)
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
                 ORDER BY created_at DESC, message_id DESC \
                 LIMIT ?",
            )
            .bind(conversation_id)
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

    async fn get_message(
        &self,
        conversation_id: &str,
        message_id: &str,
    ) -> Result<Option<MessageRow>, DbError> {
        let row = sqlx::query_as::<_, MessageRow>(
            "SELECT * FROM messages \
             WHERE conversation_id = ? \
               AND message_id = ? \
               AND type NOT IN ('cron_trigger', 'skill_suggest')",
        )
        .bind(conversation_id)
        .bind(message_id)
        .fetch_optional(&self.pool)
        .await?;

        Ok(row)
    }

    async fn insert_message(&self, message: &MessageRow) -> Result<(), DbError> {
        let mut tx = self.pool.begin().await?;
        lock_required_parent(
            &mut tx,
            "conversations",
            "conversation_id",
            "updated_at",
            &message.conversation_id,
            "Conversation",
        )
        .await?;
        if let Some(msg_id) = message.msg_id.as_deref()
            && msg_id != message.message_id
        {
            lock_message_parent(
                &mut tx,
                &message.conversation_id,
                msg_id,
                "Message parent",
            )
            .await?;
        }
        sqlx::query(
            "INSERT INTO messages \
                (message_id, conversation_id, msg_id, type, content, position, \
                 status, hidden, created_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(&message.message_id)
        .bind(&message.conversation_id)
        .bind(&message.msg_id)
        .bind(&message.r#type)
        .bind(&message.content)
        .bind(&message.position)
        .bind(&message.status)
        .bind(message.hidden)
        .bind(message.created_at)
        .execute(&mut *tx)
        .await?;

        tx.commit().await?;
        Ok(())
    }

    async fn commit_turn_artifact_messages(
        &self,
        conversation_id: &str,
        turn_message_id: &str,
        messages: &[TurnArtifactMessageCommit],
        committed_at: TimestampMs,
    ) -> Result<Vec<MessageRow>, DbError> {
        ConversationId::parse(conversation_id).map_err(|error| {
            artifact_commit_conflict(format!("invalid conversation id '{conversation_id}': {error}"))
        })?;
        MessageId::parse(turn_message_id).map_err(|error| {
            artifact_commit_conflict(format!("invalid turn message id '{turn_message_id}': {error}"))
        })?;
        if messages.is_empty() {
            return Err(artifact_commit_conflict("artifact commit batch must not be empty"));
        }
        if committed_at < 0 {
            return Err(artifact_commit_conflict("artifact commit time must not be negative"));
        }

        let mut message_ids = HashSet::with_capacity(messages.len());
        let mut tool_calls = HashSet::with_capacity(messages.len());
        let mut call_ids = Vec::with_capacity(messages.len());
        for message in messages {
            if !message_ids.insert(message.message_id.clone()) {
                return Err(artifact_commit_conflict(format!(
                    "message '{}' appears more than once in the batch",
                    message.message_id
                )));
            }
            let call_id = validate_turn_artifact_message_commit(turn_message_id, message)?;
            if !tool_calls.insert((message.message_type.clone(), call_id.clone())) {
                return Err(artifact_commit_conflict(format!(
                    "tool call '{call_id}' appears more than once in the batch"
                )));
            }
            call_ids.push(call_id);
        }

        let mut tx = self.pool.begin().await?;
        let conversation = sqlx::query(
            "UPDATE conversations SET updated_at = updated_at WHERE conversation_id = ?",
        )
        .bind(conversation_id)
        .execute(&mut *tx)
        .await?;
        if conversation.rows_affected() == 0 {
            return Err(DbError::NotFound(format!("Conversation '{conversation_id}'")));
        }
        lock_message_parent(
            &mut tx,
            conversation_id,
            turn_message_id,
            "Turn message",
        )
        .await?;

        let mut committed = Vec::with_capacity(messages.len());
        for (message, call_id) in messages.iter().zip(call_ids.iter()) {
            let existing = sqlx::query_as::<_, MessageRow>("SELECT * FROM messages WHERE message_id = ?")
                .bind(&message.message_id)
                .fetch_optional(&mut *tx)
                .await?;

            match existing {
                None => {
                    sqlx::query(
                        "INSERT INTO messages \
                            (message_id, conversation_id, msg_id, type, content, position, \
                             status, hidden, created_at) \
                         VALUES (?, ?, ?, ?, ?, 'left', 'finish', 0, ?)",
                    )
                    .bind(&message.message_id)
                    .bind(conversation_id)
                    .bind(turn_message_id)
                    .bind(&message.message_type)
                    .bind(&message.content)
                    .bind(committed_at)
                    .execute(&mut *tx)
                    .await?;
                }
                Some(row) if row.status.as_deref() == Some("work") => {
                    validate_provisional_artifact_row(
                        &row,
                        conversation_id,
                        turn_message_id,
                        message,
                        call_id,
                    )?;
                    let result = sqlx::query(
                        "UPDATE messages SET content = ?, status = 'finish' \
                         WHERE message_id = ? AND conversation_id = ? AND msg_id = ? \
                           AND type = ? AND status = 'work' \
                           AND position = 'left' AND hidden = 0",
                    )
                    .bind(&message.content)
                    .bind(&message.message_id)
                    .bind(conversation_id)
                    .bind(turn_message_id)
                    .bind(&message.message_type)
                    .execute(&mut *tx)
                    .await?;
                    if result.rows_affected() != 1 {
                        return Err(artifact_commit_conflict(format!(
                            "message '{}' changed while its artifact delivery was being committed",
                            message.message_id
                        )));
                    }
                }
                Some(row) if row.status.as_deref() == Some("finish") => {
                    if !finished_artifact_row_matches(
                        &row,
                        conversation_id,
                        turn_message_id,
                        message,
                    ) {
                        return Err(artifact_commit_conflict(format!(
                            "message '{}' conflicts with an existing finished projection",
                            message.message_id
                        )));
                    }
                }
                Some(row) => {
                    return Err(artifact_commit_conflict(format!(
                        "message '{}' cannot transition from persisted status {:?} to an artifact success",
                        message.message_id, row.status
                    )));
                }
            }

            let row = sqlx::query_as::<_, MessageRow>("SELECT * FROM messages WHERE message_id = ?")
                .bind(&message.message_id)
                .fetch_one(&mut *tx)
                .await?;
            if !finished_artifact_row_matches(
                &row,
                conversation_id,
                turn_message_id,
                message,
            ) {
                return Err(artifact_commit_conflict(format!(
                    "message '{}' did not reach the exact committed projection",
                    message.message_id
                )));
            }
            committed.push(row);
        }

        tx.commit().await?;
        Ok(committed)
    }

    async fn claim_message_correlation(
        &self,
        conversation_id: &str,
        turn_message_id: &str,
        message_type: &str,
        correlation_key: &str,
    ) -> Result<String, DbError> {
        nomifun_common::ConversationId::parse(conversation_id)
            .map_err(|error| DbError::Conflict(error.to_string()))?;
        MessageId::parse(turn_message_id).map_err(|error| DbError::Conflict(error.to_string()))?;
        let message_type = message_type.trim();
        let correlation_key = correlation_key.trim();
        if message_type.is_empty() || correlation_key.is_empty() {
            return Err(DbError::Conflict(
                "message correlation type and key must be non-empty canonical strings".to_owned(),
            ));
        }

        let candidate = MessageId::new().into_string();
        let mut tx = self.pool.begin().await?;
        // Correlation reservations are created before the corresponding
        // streamed child message is projected.  `turn_message_id` is the
        // wire-scoped correlation owner, while the durable message row may be
        // inserted by the caller immediately afterward.  This is a protocol
        // token, not a logical or physical FK: requiring the message row here would
        // reject the normal reserve-then-project sequence and would also make
        // failover continuations depend on a synthetic parent row.
        sqlx::query(
            "INSERT INTO message_correlations \
             (conversation_id, turn_message_id, message_type, correlation_key, message_id) \
             VALUES (?, ?, ?, ?, ?) \
             ON CONFLICT(conversation_id, turn_message_id, message_type, correlation_key) DO NOTHING",
        )
        .bind(conversation_id)
        .bind(turn_message_id)
        .bind(message_type)
        .bind(correlation_key)
        .bind(&candidate)
        .execute(&mut *tx)
        .await?;

        let message_id: String = sqlx::query_scalar(
            "SELECT message_id FROM message_correlations \
             WHERE conversation_id = ? AND turn_message_id = ? \
               AND message_type = ? AND correlation_key = ?",
        )
        .bind(conversation_id)
        .bind(turn_message_id)
        .bind(message_type)
        .bind(correlation_key)
        .fetch_one(&mut *tx)
        .await
        .map_err(DbError::Query)?;
        tx.commit().await?;
        Ok(message_id)
    }

    async fn update_message(
        &self,
        message_id: &str,
        updates: &MessageRowUpdate,
    ) -> Result<(), DbError> {
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

        let sql = format!("UPDATE messages SET {} WHERE message_id = ?", set_parts.join(", "));

        let mut query = sqlx::query(&sql);
        for bind in &binds {
            query = bind_value(query, bind);
        }
        query = query.bind(message_id);

        let result = query.execute(&self.pool).await?;

        if result.rows_affected() == 0 {
            return Err(DbError::NotFound(format!(
                "Message '{message_id}' not found"
            )));
        }

        Ok(())
    }

    async fn delete_messages_by_conversation(
        &self,
        conversation_id: &str,
    ) -> Result<(), DbError> {
        let mut tx = self.pool.begin().await?;
        ensure_messages_are_not_retained(&mut tx, conversation_id, None, None).await?;
        sqlx::query("DELETE FROM messages WHERE conversation_id = ?")
            .bind(conversation_id)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(())
    }

    async fn delete_messages_from(
        &self,
        conversation_id: &str,
        from_created_at: i64,
        from_message_id: &str,
    ) -> Result<u64, DbError> {
        // Keyset 截断：删除 (created_at, message_id) >= 游标的所有消息。
        // 命中按 conversation_id/created_at/message_id 排序的复合索引。
        let mut tx = self.pool.begin().await?;
        ensure_messages_are_not_retained(
            &mut tx,
            conversation_id,
            Some(from_created_at),
            Some(from_message_id),
        )
        .await?;
        let result = sqlx::query(
            "DELETE FROM messages \
             WHERE conversation_id = ? \
               AND (created_at > ? OR (created_at = ? AND message_id >= ?))",
        )
        .bind(conversation_id)
        .bind(from_created_at)
        .bind(from_created_at)
        .bind(from_message_id)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(result.rows_affected())
    }

    async fn get_message_by_msg_id(
        &self,
        conversation_id: &str,
        msg_id: &str,
        msg_type: &str,
    ) -> Result<Option<MessageRow>, DbError> {
        let row = sqlx::query_as::<_, MessageRow>(
            "SELECT * FROM messages \
             WHERE conversation_id = ? AND msg_id = ? AND type = ?",
        )
        .bind(conversation_id)
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
             INNER JOIN conversations c ON m.conversation_id = c.conversation_id \
             WHERE c.user_id = ? AND m.content LIKE ?",
        )
        .bind(user_id)
        .bind(&like_pattern)
        .fetch_one(&self.pool)
        .await?;
        let total = count_row.0 as u64;

        let rows = sqlx::query_as::<_, MessageSearchRow>(
            "SELECT \
                m.message_id AS message_id, \
                m.type, \
                m.content, \
                m.created_at, \
                c.conversation_id AS conversation_id, \
                c.name AS conversation_name, \
                c.type AS conversation_type, \
                c.extra AS conversation_extra, \
                c.delegation_policy AS conversation_delegation_policy, \
                c.execution_model_pool AS conversation_execution_model_pool, \
                c.decision_policy AS conversation_decision_policy, \
                c.execution_template_id AS conversation_execution_template_id, \
                c.model AS conversation_model, \
                c.status AS conversation_status, \
                c.source AS conversation_source, \
                c.channel_chat_id AS conversation_channel_chat_id, \
                c.pinned AS conversation_pinned, \
                c.pinned_at AS conversation_pinned_at, \
                c.created_at AS conversation_created_at, \
                c.updated_at AS conversation_updated_at \
             FROM messages m \
             INNER JOIN conversations c ON m.conversation_id = c.conversation_id \
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

    async fn list_artifacts(
        &self,
        conversation_id: &str,
    ) -> Result<Vec<ConversationArtifactRow>, DbError> {
        let rows = sqlx::query_as::<_, ConversationArtifactRow>(
            "SELECT conversation_artifact_id, conversation_id, cron_job_id, \
                    kind, status, payload, created_at, updated_at \
             FROM conversation_artifacts \
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
        conversation_id: &str,
        conversation_artifact_id: &str,
    ) -> Result<Option<ConversationArtifactRow>, DbError> {
        validate_uuidv7(conversation_artifact_id).map_err(|error| {
            DbError::Conflict(format!(
                "invalid conversation_artifact_id '{conversation_artifact_id}': {error}"
            ))
        })?;
        let row = sqlx::query_as::<_, ConversationArtifactRow>(
            "SELECT conversation_artifact_id, conversation_id, cron_job_id, \
                    kind, status, payload, created_at, updated_at \
             FROM conversation_artifacts \
             WHERE conversation_id = ? AND conversation_artifact_id = ?",
        )
        .bind(conversation_id)
        .bind(conversation_artifact_id)
        .fetch_optional(&self.pool)
        .await?;

        Ok(row)
    }

    async fn upsert_artifact(
        &self,
        artifact: &ConversationArtifactRow,
    ) -> Result<ConversationArtifactRow, DbError> {
        validate_artifact_write(artifact)?;
        let mut tx = self.pool.begin().await?;
        let conversation_owner: Option<String> = sqlx::query_scalar(
            "SELECT user_id FROM conversations WHERE conversation_id = ?",
        )
        .bind(&artifact.conversation_id)
        .fetch_optional(&mut *tx)
        .await?;
        let conversation_owner = conversation_owner.ok_or_else(|| {
            DbError::Conflict(format!(
                "Conversation '{}' does not exist",
                artifact.conversation_id
            ))
        })?;
        if let Some(cron_job_id) = artifact.cron_job_id.as_deref() {
            let cron_job: Option<(String, Option<String>)> = sqlx::query_as(
                "SELECT user_id, conversation_id FROM cron_jobs WHERE cron_job_id = ?",
            )
            .bind(cron_job_id)
            .fetch_optional(&mut *tx)
            .await?;
            let (cron_owner, bound_conversation_id) = cron_job.ok_or_else(|| {
                DbError::Conflict(format!("Cron job '{cron_job_id}' does not exist"))
            })?;
            if cron_owner != conversation_owner {
                return Err(DbError::Conflict(format!(
                    "Conversation '{}' and cron job '{cron_job_id}' have different owners",
                    artifact.conversation_id
                )));
            }
            if bound_conversation_id
                .as_deref()
                .is_some_and(|conversation_id| conversation_id != artifact.conversation_id)
            {
                return Err(DbError::Conflict(format!(
                    "Cron job '{cron_job_id}' is bound to another Conversation"
                )));
            }
        }
        // Idempotency depends on `kind`:
        //   - skill_suggest: upsert against the partial UNIQUE index
        //     uq_conversation_artifacts_skill_suggest
        //     ON (conversation_id, cron_job_id) WHERE kind = 'skill_suggest'.
        //     The ON CONFLICT target must repeat the same WHERE predicate.
        //   - cron_trigger: plain INSERT, one row per trigger (no unique
        //     constraint, no ON CONFLICT clause).
        let conversation_artifact_id = if artifact.kind == "skill_suggest" {
            sqlx::query_scalar::<_, String>(
                "INSERT INTO conversation_artifacts \
                    (conversation_artifact_id, conversation_id, cron_job_id, kind, status, payload, created_at, updated_at) \
                 VALUES (?, ?, ?, ?, ?, ?, ?, ?) \
                 ON CONFLICT(conversation_id, cron_job_id) WHERE kind = 'skill_suggest' DO UPDATE SET \
                    status = excluded.status, \
                    payload = excluded.payload, \
                    updated_at = excluded.updated_at \
                 RETURNING conversation_artifact_id",
            )
            .bind(&artifact.conversation_artifact_id)
            .bind(&artifact.conversation_id)
            .bind(&artifact.cron_job_id)
            .bind(&artifact.kind)
            .bind(&artifact.status)
            .bind(&artifact.payload)
            .bind(artifact.created_at)
            .bind(artifact.updated_at)
            .fetch_one(&mut *tx)
            .await?
        } else {
            sqlx::query_scalar::<_, String>(
                "INSERT INTO conversation_artifacts \
                    (conversation_artifact_id, conversation_id, cron_job_id, kind, status, payload, created_at, updated_at) \
                 VALUES (?, ?, ?, ?, ?, ?, ?, ?) RETURNING conversation_artifact_id",
            )
            .bind(&artifact.conversation_artifact_id)
            .bind(&artifact.conversation_id)
            .bind(&artifact.cron_job_id)
            .bind(&artifact.kind)
            .bind(&artifact.status)
            .bind(&artifact.payload)
            .bind(artifact.created_at)
            .bind(artifact.updated_at)
            .fetch_one(&mut *tx)
            .await?
        };

        let persisted = sqlx::query_as::<_, ConversationArtifactRow>(
            "SELECT conversation_artifact_id, conversation_id, cron_job_id, \
                    kind, status, payload, created_at, updated_at \
             FROM conversation_artifacts \
             WHERE conversation_id = ? AND conversation_artifact_id = ?",
        )
        .bind(&artifact.conversation_id)
        .bind(conversation_artifact_id)
        .fetch_one(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(persisted)
    }

    async fn update_artifact_status(
        &self,
        conversation_id: &str,
        conversation_artifact_id: &str,
        status: &str,
        updated_at: i64,
    ) -> Result<Option<ConversationArtifactRow>, DbError> {
        validate_uuidv7(conversation_artifact_id).map_err(|error| {
            DbError::Conflict(format!(
                "invalid conversation_artifact_id '{conversation_artifact_id}': {error}"
            ))
        })?;
        if !matches!(status, "active" | "pending" | "dismissed" | "saved") {
            return Err(DbError::Conflict(format!(
                "unsupported Conversation artifact status '{status}'"
            )));
        }
        let result = sqlx::query(
            "UPDATE conversation_artifacts \
             SET status = ?, updated_at = ? \
             WHERE conversation_id = ? AND conversation_artifact_id = ?",
        )
        .bind(status)
        .bind(updated_at)
        .bind(conversation_id)
        .bind(conversation_artifact_id)
        .execute(&self.pool)
        .await?;

        if result.rows_affected() == 0 {
            return Ok(None);
        }

        self.get_artifact(conversation_id, conversation_artifact_id)
            .await
    }

    async fn mark_skill_suggest_artifacts_saved(
        &self,
        user_id: &str,
        cron_job_id: &str,
        updated_at: i64,
    ) -> Result<Vec<ConversationArtifactRow>, DbError> {
        sqlx::query(
            "UPDATE conversation_artifacts AS artifact \
             SET status = 'saved', updated_at = ? \
             WHERE artifact.kind = 'skill_suggest' \
               AND artifact.cron_job_id = ? \
               AND artifact.status != 'saved' \
               AND EXISTS (\
                   SELECT 1 \
                   FROM conversations AS conversation \
                   JOIN cron_jobs AS job ON job.cron_job_id = artifact.cron_job_id \
                   WHERE conversation.conversation_id = artifact.conversation_id \
                     AND conversation.user_id = ? \
                     AND job.user_id = ?\
               )",
        )
        .bind(updated_at)
        .bind(cron_job_id)
        .bind(user_id)
        .bind(user_id)
        .execute(&self.pool)
        .await?;

        let rows = sqlx::query_as::<_, ConversationArtifactRow>(
             "SELECT artifact.conversation_artifact_id, \
                     artifact.conversation_id, artifact.cron_job_id, artifact.kind, \
                     artifact.status, artifact.payload, artifact.created_at, artifact.updated_at \
             FROM conversation_artifacts AS artifact \
             JOIN conversations AS conversation ON conversation.conversation_id = artifact.conversation_id \
             JOIN cron_jobs AS job ON job.cron_job_id = artifact.cron_job_id \
             WHERE artifact.kind = 'skill_suggest' \
               AND artifact.cron_job_id = ? \
               AND conversation.user_id = ? \
               AND job.user_id = ? \
             ORDER BY artifact.created_at ASC, artifact.id ASC",
        )
        .bind(cron_job_id)
        .bind(user_id)
        .bind(user_id)
        .fetch_all(&self.pool)
        .await?;

        Ok(rows)
    }

    async fn delete_artifacts_by_conversation(
        &self,
        conversation_id: &str,
    ) -> Result<(), DbError> {
        sqlx::query("DELETE FROM conversation_artifacts WHERE conversation_id = ?")
            .bind(conversation_id)
            .execute(&self.pool)
            .await?;

        Ok(())
    }

    // ── conversation_mcp_servers junction ───────────────────────────

    async fn list_mcp_server_ids(&self, conversation_id: &str) -> Result<Vec<String>, DbError> {
        Ok(sqlx::query_scalar::<_, String>(
            "SELECT mcp_server_id FROM conversation_mcp_servers \
             WHERE conversation_id = ? \
             ORDER BY sort_order ASC, mcp_server_id ASC",
        )
        .bind(conversation_id)
        .fetch_all(&self.pool)
        .await?)
    }

    async fn set_mcp_server_ids(
        &self,
        conversation_id: &str,
        mcp_server_ids: &[String],
    ) -> Result<(), DbError> {
        let mut tx = self.pool.begin().await?;
        lock_required_parent(
            &mut tx,
            "conversations",
            "conversation_id",
            "updated_at",
            conversation_id,
            "Conversation",
        )
        .await?;

        let mut unique_ids = HashSet::with_capacity(mcp_server_ids.len());
        for mcp_server_id in mcp_server_ids {
            nomifun_common::validate_uuidv7(mcp_server_id).map_err(|error| {
                DbError::Conflict(format!(
                    "MCP server '{mcp_server_id}' is not a canonical UUIDv7: {error}"
                ))
            })?;
            if !unique_ids.insert(mcp_server_id.as_str()) {
                return Err(DbError::Conflict(format!(
                    "MCP server '{mcp_server_id}' appears more than once"
                )));
            }
            let locked = sqlx::query(
                "UPDATE mcp_servers SET updated_at = updated_at \
                 WHERE mcp_server_id = ? AND deleted_at IS NULL",
            )
            .bind(mcp_server_id)
            .execute(&mut *tx)
            .await?;
            if locked.rows_affected() == 0 {
                return Err(DbError::Conflict(format!(
                    "MCP server '{mcp_server_id}' does not exist or is deleted"
                )));
            }
        }

        sqlx::query("DELETE FROM conversation_mcp_servers WHERE conversation_id = ?")
            .bind(conversation_id)
            .execute(&mut *tx)
            .await?;

        for (sort_order, mcp_server_id) in mcp_server_ids.iter().enumerate() {
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
    if let Some(cron_job_id) = filters.cron_job_id.as_deref() {
        where_parts.push("c.cron_job_id = ?".to_string());
        binds.push(BindValue::Str(cron_job_id.to_owned()));
    }
    if let Some(pinned) = filters.pinned {
        where_parts.push("c.pinned = ?".to_string());
        binds.push(BindValue::Bool(pinned));
    }
    // Companion companion (work-partner) 单会话不计入普通会话列表/计数。
    // `extra.companion_session` 为 1 的行被排除;`IS NOT 1` 同时覆盖缺失/为 0
    // 的普通会话(json_extract 返回 NULL 时 `NULL IS NOT 1` 为真)。
    if filters.exclude_companion_companion {
        where_parts.push("json_extract(c.extra, '$.companion_session') IS NOT 1".to_string());
    }
    // Attempt conversations are aggregate-internal execution surfaces. The v3
    // execution link is the only source of truth; open-ended JSON-extra
    // identity markers are deliberately ignored.
    where_parts.push(
        "NOT EXISTS (SELECT 1 FROM conversation_execution_links execution_link \
         WHERE execution_link.conversation_id = c.conversation_id \
           AND execution_link.relation = 'attempt')"
            .to_string(),
    );
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

    const TEST_INSTALLATION_OWNER: &str = "0190f5fe-7c00-7a00-8000-000000000001";
    const FIXTURE_PROVIDER_ID: &str = "0190f5fe-7c00-7a00-8abc-012345678901";

    async fn init_database_memory() -> Result<crate::Database, crate::DbError> {
        crate::init_database_memory_with_owner(
            nomifun_common::UserId::parse(TEST_INSTALLATION_OWNER.to_owned())
                .expect("canonical fixture owner"),
        )
        .await
    }

    async fn insert_fixture_provider(pool: &SqlitePool, provider_id: &str) {
        sqlx::query(
            "INSERT INTO providers (\
                provider_id, platform, name, base_url, api_key_encrypted, models, enabled, \
                capabilities, created_at, updated_at\
             ) VALUES (?, 'openai', ?, 'https://example.invalid', \
                       'encrypted', '[]', 1, '[]', 0, 0)",
        )
        .bind(provider_id)
        .bind(provider_id)
        .execute(pool)
        .await
        .unwrap();
    }

    async fn setup() -> (SqliteConversationRepository, crate::Database) {
        let db = init_database_memory().await.unwrap();
        insert_fixture_provider(db.pool(), FIXTURE_PROVIDER_ID).await;
        let repo = SqliteConversationRepository::new(db.pool().clone());
        (repo, db)
    }

    fn sample_conversation(user_id: &str) -> ConversationRow {
        let now = nomifun_common::now_ms();
        ConversationRow {
            id: 0,
            conversation_id: nomifun_common::ConversationId::new().into_string(),
            user_id: user_id.to_string(),
            name: "Test Conversation".to_string(),
            r#type: "acp".to_string(),
            extra: r#"{"workspace":"/home/user/project"}"#.to_string(),
            delegation_policy: "automatic".to_string(),
            execution_model_pool: None,
            decision_policy: "automatic".to_string(),
            execution_template_id: None,
            model: Some(
                serde_json::json!({
                    "provider_id": FIXTURE_PROVIDER_ID,
                    "model": "claude-sonnet-4-20250514"
                })
                .to_string(),
            ),
            status: Some("pending".to_string()),
            source: Some("nomifun".to_string()),
            channel_chat_id: None,
            pinned: false,
            pinned_at: None,
            cron_job_id: None,
            preset_id: None,
            preset_revision: None,
            preset_snapshot: None,
            created_at: now,
            updated_at: now,
        }
    }

    fn sample_message(conv_id: impl Into<String>) -> MessageRow {
        let now = nomifun_common::now_ms();
        let message_id = MessageId::new().into_string();
        MessageRow {
            id: 0,
            message_id: message_id.clone(),
            conversation_id: conv_id.into(),
            msg_id: Some(message_id),
            r#type: "text".to_string(),
            content: r#"{"content":"Hello world"}"#.to_string(),
            position: Some("right".to_string()),
            status: Some("finish".to_string()),
            hidden: false,
            created_at: now,
        }
    }

    /// Inserts a minimal valid local cron job and returns its logical UUIDv7 ID.
    async fn insert_cron_job(pool: &SqlitePool, name: &str) -> String {
        let now = nomifun_common::now_ms();
        let cron_job_id = nomifun_common::CronJobId::new().into_string();
        sqlx::query_scalar(
            "INSERT INTO cron_jobs \
                (cron_job_id, user_id, name, schedule_kind, schedule_value, payload_message, agent_type, created_by, created_at, updated_at) \
             VALUES (?, ?, ?, 'every', '60', '', 'gemini', 'user', ?, ?) \
             RETURNING cron_job_id",
        )
        .bind(&cron_job_id)
        .bind(TEST_INSTALLATION_OWNER)
        .bind(format!("job {name}"))
        .bind(now)
        .bind(now)
        .fetch_one(pool)
        .await
        .unwrap()
    }

    /// Inserts a minimal valid MCP server and returns its business ID.
    async fn insert_mcp_server(pool: &SqlitePool, name: &str) -> String {
        let now = nomifun_common::now_ms();
        let mcp_server_id = nomifun_common::generate_id();
        sqlx::query_scalar(
            "INSERT INTO mcp_servers \
                (mcp_server_id, name, transport_type, transport_config, created_at, updated_at) \
             VALUES (?, ?, 'stdio', '{}', ?, ?) \
             RETURNING mcp_server_id",
        )
        .bind(&mcp_server_id)
        .bind(name)
        .bind(now)
        .bind(now)
        .fetch_one(pool)
        .await
        .unwrap()
    }

    fn sample_artifact(
        conversation_id: impl Into<String>,
        kind: &str,
        cron_job_id: Option<String>,
    ) -> ConversationArtifactRow {
        let now = nomifun_common::now_ms();
        let payload = cron_job_id
            .as_ref()
            .map(|cron_job_id| serde_json::json!({ "cron_job_id": cron_job_id }))
            .unwrap_or_else(|| serde_json::json!({}));
        ConversationArtifactRow {
            conversation_artifact_id: nomifun_common::generate_id(),
            conversation_id: conversation_id.into(),
            cron_job_id,
            kind: kind.to_string(),
            status: "active".to_string(),
            payload: payload.to_string(),
            created_at: now,
            updated_at: now,
        }
    }

    async fn link_lead_and_attempt_conversations(
        pool: &SqlitePool,
        lead_conversation_id: &str,
        attempt_conversation_id: &str,
    ) {
        let now = nomifun_common::now_ms();
        let execution_id = nomifun_common::AgentExecutionId::new().into_string();

        sqlx::query(
             "INSERT INTO agent_executions \
             (execution_id, user_id, goal, status, plan_gate, adaptation_policy, decision_policy, \
              delegation_policy, max_parallel, initial_plan_input, created_at, updated_at) \
             VALUES (?, ?, 'test execution', 'running', 'automatic', 'fixed', \
                     'automatic', 'automatic', 4, '{\"mode\":\"automatic\"}', ?, ?)",
        )
        .bind(&execution_id)
        .bind(TEST_INSTALLATION_OWNER)
        .bind(now)
        .bind(now)
        .execute(pool)
        .await
        .unwrap();

        let participant_id = nomifun_common::generate_id();
        sqlx::query(
            "INSERT INTO agent_execution_participants \
             (participant_id, execution_id, source_agent_id, provider_id, model, \
              introduced_in_revision, created_at) \
             VALUES (?, ?, 'test-agent', ?, 'fixture-model', 0, ?)",
        )
        .bind(&participant_id)
        .bind(&execution_id)
        .bind(FIXTURE_PROVIDER_ID)
        .bind(now)
        .execute(pool)
        .await
        .unwrap();

        let step_id = nomifun_common::generate_id();
        sqlx::query(
            "INSERT INTO agent_execution_steps \
             (step_id, execution_id, title, spec, kind, agent_mode, status, \
              assigned_participant_id, assignment_source, introduced_in_revision, \
              created_at, updated_at) \
             VALUES (?, ?, 'test step', 'test step', 'agent', 'normal', 'running', \
                     ?, 'manual', 0, ?, ?)",
        )
        .bind(&step_id)
        .bind(&execution_id)
        .bind(&participant_id)
        .bind(now)
        .bind(now)
        .execute(pool)
        .await
        .unwrap();

        let attempt_id = nomifun_common::generate_id();
        sqlx::query(
            "INSERT INTO agent_execution_attempts \
             (attempt_id, execution_id, step_id, attempt_no, participant_id, status, \
              trigger_reason, started_at, created_at, updated_at) \
             VALUES (?, ?, ?, 0, ?, 'running', 'initial', ?, ?, ?)",
        )
        .bind(&attempt_id)
        .bind(&execution_id)
        .bind(&step_id)
        .bind(&participant_id)
        .bind(now)
        .bind(now)
        .bind(now)
        .execute(pool)
        .await
        .unwrap();

        sqlx::query(
            "INSERT INTO conversation_execution_links \
             (conversation_id, execution_id, relation, active, created_at, updated_at) \
             VALUES (?, ?, 'lead', 1, ?, ?)",
        )
        .bind(lead_conversation_id)
        .bind(&execution_id)
        .bind(now)
        .bind(now)
        .execute(pool)
        .await
        .unwrap();

        sqlx::query(
            "INSERT INTO conversation_execution_links \
             (conversation_id, execution_id, relation, step_id, attempt_id, \
              active, created_at, updated_at) \
             VALUES (?, ?, 'attempt', ?, ?, 1, ?, ?)",
        )
        .bind(attempt_conversation_id)
        .bind(&execution_id)
        .bind(&step_id)
        .bind(&attempt_id)
        .bind(now)
        .bind(now)
        .execute(pool)
        .await
        .unwrap();
    }

    // ── Conversation CRUD tests ─────────────────────────────────────

    #[tokio::test]
    async fn create_and_get_conversation() {
        let (repo, _db) = setup().await;
        let mut conv = sample_conversation(TEST_INSTALLATION_OWNER);

        conv.conversation_id = repo.create(&conv).await.unwrap();
        assert!(nomifun_common::ConversationId::parse(conv.conversation_id.clone()).is_ok());
        let found = repo.get(&conv.conversation_id).await.unwrap().unwrap();

        assert_eq!(found.conversation_id, conv.conversation_id);
        assert_eq!(found.name, "Test Conversation");
        assert_eq!(found.r#type, "acp");
        assert_eq!(found.status.as_deref(), Some("pending"));
        assert!(!found.pinned);
    }

    #[tokio::test]
    async fn get_nonexistent_returns_none() {
        let (repo, _db) = setup().await;
        assert!(
            repo.get("019abcdef012-7abc-8abc-0123-456789abcdee")
                .await
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    async fn update_conversation_name() {
        let (repo, _db) = setup().await;
        let mut conv = sample_conversation(TEST_INSTALLATION_OWNER);
        conv.conversation_id = repo.create(&conv).await.unwrap();

        let now = nomifun_common::now_ms();
        repo.update(
            &conv.conversation_id,
            &ConversationRowUpdate {
                name: Some("Updated Name".to_string()),
                updated_at: Some(now),
                ..Default::default()
            },
        )
        .await
        .unwrap();

        let found = repo.get(&conv.conversation_id).await.unwrap().unwrap();
        assert_eq!(found.name, "Updated Name");
        assert!(found.updated_at >= conv.updated_at);
    }

    #[tokio::test]
    async fn update_conversation_pinned() {
        let (repo, _db) = setup().await;
        let mut conv = sample_conversation(TEST_INSTALLATION_OWNER);
        conv.conversation_id = repo.create(&conv).await.unwrap();

        let pin_time = nomifun_common::now_ms();
        repo.update(
            &conv.conversation_id,
            &ConversationRowUpdate {
                pinned: Some(true),
                pinned_at: Some(Some(pin_time)),
                updated_at: Some(pin_time),
                ..Default::default()
            },
        )
        .await
        .unwrap();

        let found = repo.get(&conv.conversation_id).await.unwrap().unwrap();
        assert!(found.pinned);
        assert_eq!(found.pinned_at, Some(pin_time));
    }

    #[tokio::test]
    async fn update_nonexistent_returns_not_found() {
        let (repo, _db) = setup().await;
        let err = repo
            .update(
                "019abcdef012-7abc-8abc-0123-456789abcdee",
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
        let mut conv = sample_conversation(TEST_INSTALLATION_OWNER);
        conv.conversation_id = repo.create(&conv).await.unwrap();

        // Empty update should succeed without error
        repo.update(&conv.conversation_id, &ConversationRowUpdate::default())
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn update_idmm_requires_existing_canonical_bypass_providers_atomically() {
        let (repo, db) = setup().await;
        let mut conv = sample_conversation(TEST_INSTALLATION_OWNER);
        conv.r#type = "acp".to_owned();
        conv.conversation_id = repo.create(&conv).await.unwrap();
        let original_extra = repo
            .get(&conv.conversation_id)
            .await
            .unwrap()
            .unwrap()
            .extra;

        let missing_provider = "0190f5fe-7c00-7a00-8000-000000000099";
        let missing = serde_json::json!({
            "fault_watch": {
                "enabled": true,
                "bypass_model": {
                    "provider_id": missing_provider,
                    "model": "missing-model"
                }
            }
        })
        .to_string();
        assert!(matches!(
            repo.update_idmm(&conv.conversation_id, Some(&missing))
                .await
                .unwrap_err(),
            DbError::Conflict(ref message) if message.contains("missing provider")
        ));
        assert_eq!(
            repo.get(&conv.conversation_id)
                .await
                .unwrap()
                .unwrap()
                .extra,
            original_extra,
            "a rejected IDMM reference must not partially mutate Conversation extra"
        );

        let malformed = serde_json::json!({
            "decision_watch": {
                "bypass_model": {
                    "provider_id": "provider-not-a-uuid",
                    "model": "bad-model"
                }
            }
        })
        .to_string();
        assert!(matches!(
            repo.update_idmm(&conv.conversation_id, Some(&malformed))
                .await
                .unwrap_err(),
            DbError::Conflict(ref message) if message.contains("not canonical")
        ));

        let second_provider = "0190f5fe-7c00-7a00-8000-000000000098";
        insert_fixture_provider(db.pool(), second_provider).await;
        let valid = serde_json::json!({
            "fault_watch": {
                "enabled": true,
                "scan_interval_secs": 31,
                "bypass_model": {
                    "provider_id": FIXTURE_PROVIDER_ID,
                    "model": "fault-model"
                }
            },
            "decision_watch": {
                "enabled": true,
                "scan_interval_secs": 47,
                "bypass_model": {
                    "provider_id": second_provider,
                    "model": "decision-model"
                }
            }
        })
        .to_string();
        repo.update_idmm(&conv.conversation_id, Some(&valid))
            .await
            .unwrap();
        let row = repo.get(&conv.conversation_id).await.unwrap().unwrap();
        let extra: serde_json::Value = serde_json::from_str(&row.extra).unwrap();
        assert_eq!(
            extra["idmm"]["fault_watch"]["bypass_model"]["provider_id"],
            FIXTURE_PROVIDER_ID
        );
        assert_eq!(
            extra["idmm"]["decision_watch"]["bypass_model"]["provider_id"],
            second_provider
        );
        assert_eq!(extra["workspace"], "/home/user/project");

        repo.update_idmm(&conv.conversation_id, None)
            .await
            .unwrap();
        let extra: serde_json::Value = serde_json::from_str(
            &repo
                .get(&conv.conversation_id)
                .await
                .unwrap()
                .unwrap()
                .extra,
        )
        .unwrap();
        assert!(extra.get("idmm").is_none());
        assert_eq!(extra["workspace"], "/home/user/project");
    }

    #[tokio::test]
    async fn generic_conversation_writes_cannot_bypass_idmm_provider_validation() {
        let (repo, _db) = setup().await;
        let missing_provider = "0190f5fe-7c00-7a00-8000-000000000096";
        let idmm_extra = serde_json::json!({
            "idmm": {
                "fault_watch": {
                    "bypass_model": {
                        "provider_id": missing_provider,
                        "model": "missing"
                    }
                }
            }
        })
        .to_string();

        let mut create_candidate = sample_conversation(TEST_INSTALLATION_OWNER);
        create_candidate.extra = idmm_extra.clone();
        assert!(matches!(
            repo.create(&create_candidate).await.unwrap_err(),
            DbError::Conflict(ref message) if message.contains("missing provider")
        ));

        let mut existing = sample_conversation(TEST_INSTALLATION_OWNER);
        existing.conversation_id = repo.create(&existing).await.unwrap();
        let before = repo
            .get(&existing.conversation_id)
            .await
            .unwrap()
            .unwrap()
            .extra;
        assert!(matches!(
            repo.update(
                &existing.conversation_id,
                &ConversationRowUpdate {
                    extra: Some(idmm_extra),
                    ..Default::default()
                },
            )
            .await
            .unwrap_err(),
            DbError::Conflict(ref message) if message.contains("missing provider")
        ));
        assert_eq!(
            repo.get(&existing.conversation_id)
                .await
                .unwrap()
                .unwrap()
                .extra,
            before
        );
    }

    #[tokio::test]
    async fn delete_conversation() {
        let (repo, _db) = setup().await;
        let mut conv = sample_conversation(TEST_INSTALLATION_OWNER);
        conv.conversation_id = repo.create(&conv).await.unwrap();

        repo.delete(&conv.conversation_id).await.unwrap();
        assert!(repo.get(&conv.conversation_id).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn delete_cleans_up_messages() {
        let (repo, _db) = setup().await;
        let mut conv = sample_conversation(TEST_INSTALLATION_OWNER);
        conv.conversation_id = repo.create(&conv).await.unwrap();

        let msg = sample_message(conv.conversation_id.clone());
        repo.insert_message(&msg).await.unwrap();

        repo.delete(&conv.conversation_id).await.unwrap();

        // Repository-owned logical cleanup removes dependent messages.
        let result = repo
            .get_messages(&conv.conversation_id, 1, 50, SortOrder::Desc)
            .await
            .unwrap();
        assert!(result.items.is_empty());
    }

    #[tokio::test]
    async fn delete_applies_registered_set_null_and_cascade_policies() {
        let (repo, db) = setup().await;
        let mut conv = sample_conversation(TEST_INSTALLATION_OWNER);
        conv.r#type = "acp".to_owned();
        conv.conversation_id = repo.create(&conv).await.unwrap();
        let now = nomifun_common::now_ms();

        let channel_plugin_id = nomifun_common::ChannelPluginId::new().into_string();
        sqlx::query(
            "INSERT INTO channel_plugins \
                (channel_plugin_id, type, name, enabled, config, created_at, updated_at) \
             VALUES (?, 'telegram', 'fixture', 0, '{}', ?, ?)",
        )
        .bind(&channel_plugin_id)
        .bind(now)
        .bind(now)
        .execute(db.pool())
        .await
        .unwrap();
        let channel_user_id = nomifun_common::ChannelUserId::new().into_string();
        sqlx::query(
            "INSERT INTO channel_users \
                (channel_user_id, platform_user_id, platform_type, channel_plugin_id, authorized_at) \
             VALUES (?, 'fixture-user', 'telegram', ?, ?)",
        )
        .bind(&channel_user_id)
        .bind(&channel_plugin_id)
        .bind(now)
        .execute(db.pool())
        .await
        .unwrap();
        let channel_session_id = nomifun_common::ChannelSessionId::new().into_string();
        sqlx::query(
            "INSERT INTO channel_sessions \
                (channel_session_id, channel_user_id, agent_type, conversation_id, \
                 channel_plugin_id, created_at, last_activity) \
             VALUES (?, ?, 'acp', ?, ?, ?, ?)",
        )
        .bind(&channel_session_id)
        .bind(&channel_user_id)
        .bind(&conv.conversation_id)
        .bind(&channel_plugin_id)
        .bind(now)
        .bind(now)
        .execute(db.pool())
        .await
        .unwrap();
        let cron_job_id: String = nomifun_common::CronJobId::new().into_string();
        let cron_job_run_id = nomifun_common::CronJobRunId::new().into_string();
        sqlx::query(
            "INSERT INTO cron_jobs \
                (cron_job_id, user_id, name, schedule_kind, schedule_value, payload_message, \
                 conversation_id, agent_type, created_by, created_at, updated_at) \
             VALUES (?, ?, 'fixture cron', 'every', '60', 'run', ?, 'acp', 'user', ?, ?)",
        )
        .bind(&cron_job_id)
        .bind(TEST_INSTALLATION_OWNER)
        .bind(&conv.conversation_id)
        .bind(now)
        .bind(now)
        .execute(db.pool())
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO cron_job_runs \
                (cron_job_run_id, cron_job_id, executed_at_ms, status, created_at_ms) \
             VALUES (?, ?, ?, 'ok', ?)",
        )
        .bind(&cron_job_run_id)
        .bind(&cron_job_id)
        .bind(now)
        .bind(now)
        .execute(db.pool())
        .await
        .unwrap();
        let requirement_id = nomifun_common::RequirementId::new().into_string();
        sqlx::query(
            "INSERT INTO requirements \
                (requirement_id, display_no, title, tag, owner_conversation_id, created_at, updated_at) \
             VALUES (?, 900001, 'fixture requirement', 'fixture', ?, ?, ?)",
        )
        .bind(&requirement_id)
        .bind(&conv.conversation_id)
        .bind(now)
        .bind(now)
        .execute(db.pool())
        .await
        .unwrap();
        let knowledge_binding_id = nomifun_common::KnowledgeBindingId::new();
        sqlx::query(
            "INSERT INTO knowledge_bindings \
                (knowledge_binding_id, target_kind, target_conversation_id, updated_at) \
             VALUES (?, 'conversation', ?, ?)",
        )
        .bind(knowledge_binding_id.as_str())
        .bind(&conv.conversation_id)
        .bind(now)
        .execute(db.pool())
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO knowledge_binding_bases \
                (knowledge_binding_id, knowledge_base_id, position) \
             VALUES (?, ?, 0)",
        )
        .bind(knowledge_binding_id.as_str())
        .bind(nomifun_common::KnowledgeBaseId::new().as_str())
        .execute(db.pool())
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO acp_session \
                (conversation_id, agent_backend, agent_source) \
             VALUES (?, 'claude', 'custom')",
        )
        .bind(&conv.conversation_id)
        .execute(db.pool())
        .await
        .unwrap();

        let deleted_cron_job_ids = repo
            .delete_with_cleanup(&conv.conversation_id)
            .await
            .unwrap();

        let session_conversation: Option<String> = sqlx::query_scalar(
            "SELECT conversation_id FROM channel_sessions WHERE channel_session_id = ?",
        )
        .bind(&channel_session_id)
        .fetch_one(db.pool())
        .await
        .unwrap();
        let cron_job_count: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM cron_jobs WHERE cron_job_id = ?")
                .bind(&cron_job_id)
                .fetch_one(db.pool())
                .await
                .unwrap();
        let cron_run_count: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM cron_job_runs WHERE cron_job_id = ?")
                .bind(&cron_job_id)
                .fetch_one(db.pool())
                .await
                .unwrap();
        let requirement_conversation: Option<String> =
            sqlx::query_scalar(
                "SELECT owner_conversation_id FROM requirements WHERE requirement_id = ?",
            )
                .bind(&requirement_id)
                .fetch_one(db.pool())
                .await
                .unwrap();
        let binding_count: i64 =
            sqlx::query_scalar(
                "SELECT COUNT(*) FROM knowledge_bindings WHERE knowledge_binding_id = ?",
            )
                .bind(knowledge_binding_id.as_str())
                .fetch_one(db.pool())
                .await
                .unwrap();
        let binding_base_count: i64 =
            sqlx::query_scalar(
                "SELECT COUNT(*) FROM knowledge_binding_bases WHERE knowledge_binding_id = ?",
            )
                .bind(knowledge_binding_id.as_str())
                .fetch_one(db.pool())
                .await
                .unwrap();
        let acp_count: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM acp_session WHERE conversation_id = ?")
                .bind(&conv.conversation_id)
                .fetch_one(db.pool())
                .await
                .unwrap();

        assert!(session_conversation.is_none());
        assert_eq!(deleted_cron_job_ids, vec![cron_job_id]);
        assert_eq!(cron_job_count, 0);
        assert_eq!(cron_run_count, 0);
        assert!(requirement_conversation.is_none());
        assert_eq!(binding_count, 0);
        assert_eq!(binding_base_count, 0);
        assert_eq!(acp_count, 0);
    }

    #[tokio::test]
    async fn delete_restricts_keep_history_execution_link() {
        let (repo, db) = setup().await;
        let mut conv = sample_conversation(TEST_INSTALLATION_OWNER);
        conv.conversation_id = repo.create(&conv).await.unwrap();
        let execution_id = nomifun_common::AgentExecutionId::new().into_string();
        let now = nomifun_common::now_ms();
        sqlx::query(
            "INSERT INTO conversation_execution_links \
                (conversation_id, execution_id, relation, active, created_at, updated_at) \
             VALUES (?, ?, 'lead', 1, ?, ?)",
        )
        .bind(&conv.conversation_id)
        .bind(&execution_id)
        .bind(now)
        .bind(now)
        .execute(db.pool())
        .await
        .unwrap();

        let error = repo.delete(&conv.conversation_id).await.unwrap_err();
        assert!(matches!(error, DbError::Conflict(_)));
        assert!(repo.get(&conv.conversation_id).await.unwrap().is_some());
    }

    #[tokio::test]
    async fn delete_nonexistent_returns_not_found() {
        let (repo, _db) = setup().await;
        let err = repo
            .delete("019abcdef012-7abc-8abc-0123-456789abcdee")
            .await
            .unwrap_err();
        assert!(matches!(err, DbError::NotFound(_)));
    }

    // ── Pagination tests ────────────────────────────────────────────

    #[tokio::test]
    async fn list_empty() {
        let (repo, _db) = setup().await;
        let result = repo
            .list_paginated(
                TEST_INSTALLATION_OWNER,
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

        let mut c1 = sample_conversation(TEST_INSTALLATION_OWNER);
        c1.name = "First".to_string();
        c1.updated_at = 1000;
        repo.create(&c1).await.unwrap();

        let mut c2 = sample_conversation(TEST_INSTALLATION_OWNER);
        c2.name = "Second".to_string();
        c2.updated_at = 2000;
        repo.create(&c2).await.unwrap();

        let mut c3 = sample_conversation(TEST_INSTALLATION_OWNER);
        c3.name = "Third".to_string();
        c3.updated_at = 3000;
        repo.create(&c3).await.unwrap();

        let result = repo
            .list_paginated(
                TEST_INSTALLATION_OWNER,
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
            let mut c = sample_conversation(TEST_INSTALLATION_OWNER);
            c.name = format!("Conv {i}");
            c.updated_at = (i + 1) as i64 * 1000;
            repo.create(&c).await.unwrap();
            convs.push(c);
        }

        // Page 1: limit 2 → items[4,3], hasMore=true
        let page1 = repo
            .list_paginated(
                TEST_INSTALLATION_OWNER,
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
        let cursor = page1.items.last().unwrap().conversation_id.clone();
        let page2 = repo
            .list_paginated(
                TEST_INSTALLATION_OWNER,
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
        let cursor = page2.items.last().unwrap().conversation_id.clone();
        let page3 = repo
            .list_paginated(
                TEST_INSTALLATION_OWNER,
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

        let mut c1 = sample_conversation(TEST_INSTALLATION_OWNER);
        c1.source = Some("nomifun".to_string());
        repo.create(&c1).await.unwrap();

        let mut c2 = sample_conversation(TEST_INSTALLATION_OWNER);
        c2.source = Some("telegram".to_string());
        repo.create(&c2).await.unwrap();

        let result = repo
            .list_paginated(
                TEST_INSTALLATION_OWNER,
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

        let cron_job_id = insert_cron_job(&repo.pool, "cron_abc").await;

        let mut c1 = sample_conversation(TEST_INSTALLATION_OWNER);
        c1.cron_job_id = Some(cron_job_id.clone());
        c1.conversation_id = repo.create(&c1).await.unwrap();

        let c2 = sample_conversation(TEST_INSTALLATION_OWNER);
        repo.create(&c2).await.unwrap();

        let result = repo
            .list_paginated(
                TEST_INSTALLATION_OWNER,
                &ConversationFilters {
                    cron_job_id: Some(cron_job_id),
                    limit: 20,
                    ..Default::default()
                },
            )
            .await
            .unwrap();

        assert_eq!(result.items.len(), 1);
        assert_eq!(result.total, 1);
        assert_eq!(result.items[0].conversation_id, c1.conversation_id);
    }

    #[tokio::test]
    async fn list_filter_by_pinned() {
        let (repo, _db) = setup().await;

        let mut c1 = sample_conversation(TEST_INSTALLATION_OWNER);
        c1.pinned = true;
        c1.pinned_at = Some(nomifun_common::now_ms());
        repo.create(&c1).await.unwrap();

        let mut c2 = sample_conversation(TEST_INSTALLATION_OWNER);
        c2.pinned = false;
        repo.create(&c2).await.unwrap();

        let result = repo
            .list_paginated(
                TEST_INSTALLATION_OWNER,
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

    #[tokio::test]
    async fn list_keeps_lead_and_excludes_attempt_conversation() {
        let (repo, db) = setup().await;

        let mut plain = sample_conversation(TEST_INSTALLATION_OWNER);
        plain.extra = r#"{"workspace":"/project"}"#.to_string();
        let plain_id = repo.create(&plain).await.unwrap();

        let lead = sample_conversation(TEST_INSTALLATION_OWNER);
        let lead_id = repo.create(&lead).await.unwrap();

        let attempt_id = repo
            .create(&sample_conversation(TEST_INSTALLATION_OWNER))
            .await
            .unwrap();
        link_lead_and_attempt_conversations(db.pool(), &lead_id, &attempt_id).await;

        let result = repo
            .list_paginated(
                TEST_INSTALLATION_OWNER,
                &ConversationFilters {
                    limit: 20,
                    ..Default::default()
                },
            )
            .await
            .unwrap();

        assert_eq!(result.total, 2);
        assert_eq!(result.items.len(), 2);
        assert!(
            result
                .items
                .iter()
                .any(|c| c.conversation_id == plain_id),
            "plain conversation must remain visible"
        );
        assert!(
            result
                .items
                .iter()
                .any(|c| c.conversation_id == lead_id),
            "lead conversation must remain visible"
        );
        assert!(
            result
                .items
                .iter()
                .all(|c| c.conversation_id != attempt_id),
            "attempt conversation must be excluded"
        );
    }

    // ── Extended query tests ────────────────────────────────────────
    #[tokio::test]
    async fn find_by_source_and_chat() {
        let (repo, _db) = setup().await;

        let mut c = sample_conversation(TEST_INSTALLATION_OWNER);
        c.source = Some("telegram".to_string());
        c.channel_chat_id = Some("user:123".to_string());
        c.r#type = "acp".to_string();
        c.conversation_id = repo.create(&c).await.unwrap();

        let found = repo
            .find_by_source_and_chat(TEST_INSTALLATION_OWNER, "telegram", "user:123", "acp")
            .await
            .unwrap();
        assert!(found.is_some());
        assert_eq!(found.unwrap().conversation_id, c.conversation_id);

        // Different chat ID → not found
        let not_found = repo
            .find_by_source_and_chat(TEST_INSTALLATION_OWNER, "telegram", "user:999", "acp")
            .await
            .unwrap();
        assert!(not_found.is_none());
    }

    #[tokio::test]
    async fn list_by_cron_job() {
        let (repo, _db) = setup().await;

        let cron_1 = insert_cron_job(&repo.pool, "cron_1").await;
        let cron_2 = insert_cron_job(&repo.pool, "cron_2").await;

        let mut c1 = sample_conversation(TEST_INSTALLATION_OWNER);
        c1.cron_job_id = Some(cron_1.clone());
        repo.create(&c1).await.unwrap();

        let mut c2 = sample_conversation(TEST_INSTALLATION_OWNER);
        c2.cron_job_id = Some(cron_1.clone());
        repo.create(&c2).await.unwrap();

        let mut c3 = sample_conversation(TEST_INSTALLATION_OWNER);
        c3.cron_job_id = Some(cron_2);
        repo.create(&c3).await.unwrap();

        let result = repo
            .list_by_cron_job(TEST_INSTALLATION_OWNER, &cron_1)
            .await
            .unwrap();
        assert_eq!(result.len(), 2);
    }

    #[tokio::test]
    async fn list_associated_by_workspace() {
        let (repo, _db) = setup().await;

        let mut c1 = sample_conversation(TEST_INSTALLATION_OWNER);
        c1.extra = r#"{"workspace":"/shared/project"}"#.to_string();
        c1.conversation_id = repo.create(&c1).await.unwrap();

        let mut c2 = sample_conversation(TEST_INSTALLATION_OWNER);
        c2.extra = r#"{"workspace":"/shared/project"}"#.to_string();
        c2.conversation_id = repo.create(&c2).await.unwrap();

        let mut c3 = sample_conversation(TEST_INSTALLATION_OWNER);
        c3.extra = r#"{"workspace":"/other/project"}"#.to_string();
        repo.create(&c3).await.unwrap();

        let associated = repo.list_associated(TEST_INSTALLATION_OWNER, &c1.conversation_id).await.unwrap();
        assert_eq!(associated.len(), 1);
        assert_eq!(associated[0].conversation_id, c2.conversation_id);
    }

    #[tokio::test]
    async fn list_associated_no_workspace() {
        let (repo, _db) = setup().await;

        let mut c = sample_conversation(TEST_INSTALLATION_OWNER);
        c.extra = r#"{}"#.to_string();
        c.conversation_id = repo.create(&c).await.unwrap();

        let associated = repo.list_associated(TEST_INSTALLATION_OWNER, &c.conversation_id).await.unwrap();
        assert!(associated.is_empty());
    }

    #[tokio::test]
    async fn list_associated_not_found() {
        let (repo, _db) = setup().await;
        let missing_conversation_id = ConversationId::new().into_string();
        let err = repo
            .list_associated(TEST_INSTALLATION_OWNER, &missing_conversation_id)
            .await
            .unwrap_err();
        assert!(matches!(err, DbError::NotFound(_)));
    }

    #[tokio::test]
    async fn cron_job_id_roundtrips_as_column() {
        let (repo, _db) = setup().await;

        let cron_job_id = insert_cron_job(&repo.pool, "cron_x").await;

        let mut conv = sample_conversation(TEST_INSTALLATION_OWNER);
        conv.cron_job_id = Some(cron_job_id.clone());
        conv.conversation_id = repo.create(&conv).await.unwrap();

        let found = repo.get(&conv.conversation_id).await.unwrap().unwrap();
        assert_eq!(found.cron_job_id, Some(cron_job_id));

        // Clearing via update sets the column to NULL.
        repo.update(
            &conv.conversation_id,
            &ConversationRowUpdate {
                cron_job_id: Some(None),
                updated_at: Some(nomifun_common::now_ms()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        let cleared = repo.get(&conv.conversation_id).await.unwrap().unwrap();
        assert_eq!(cleared.cron_job_id, None);
    }

    // ── Artifact tests ──────────────────────────────────────────────

    #[tokio::test]
    async fn cron_trigger_artifacts_insert_distinct_rows() {
        let (repo, _db) = setup().await;
        let mut conv = sample_conversation(TEST_INSTALLATION_OWNER);
        conv.conversation_id = repo.create(&conv).await.unwrap();
        let cron_job_id = insert_cron_job(&repo.pool, "cron_t").await;

        // cron_trigger has no unique constraint: each upsert is a fresh row with
        // a distinct auto-assigned i64 id.
        let a1 = repo
            .upsert_artifact(&sample_artifact(
                conv.conversation_id.clone(),
                "cron_trigger",
                Some(cron_job_id.clone()),
            ))
            .await
            .unwrap();
        let a2 = repo
            .upsert_artifact(&sample_artifact(
                conv.conversation_id.clone(),
                "cron_trigger",
                Some(cron_job_id.clone()),
            ))
            .await
            .unwrap();

        assert!(nomifun_common::validate_uuidv7(&a1.conversation_artifact_id).is_ok());
        assert!(nomifun_common::validate_uuidv7(&a2.conversation_artifact_id).is_ok());
        assert_ne!(
            a1.conversation_artifact_id,
            a2.conversation_artifact_id
        );

        let listed = repo.list_artifacts(&conv.conversation_id).await.unwrap();
        assert_eq!(listed.len(), 2);
    }

    #[tokio::test]
    async fn skill_suggest_artifacts_upsert_is_idempotent() {
        let (repo, _db) = setup().await;
        let mut conv = sample_conversation(TEST_INSTALLATION_OWNER);
        conv.conversation_id = repo.create(&conv).await.unwrap();
        let cron_job_id = insert_cron_job(&repo.pool, "cron_s").await;

        let first = repo
            .upsert_artifact(&sample_artifact(
                conv.conversation_id.clone(),
                "skill_suggest",
                Some(cron_job_id.clone()),
            ))
            .await
            .unwrap();

        // Second upsert for the same (conversation_id, cron_job_id) collides on the
        // partial UNIQUE index → updates in place, keeping the same id.
        let mut updated_input = sample_artifact(
            conv.conversation_id.clone(),
            "skill_suggest",
            Some(cron_job_id.clone()),
        );
        updated_input.payload =
            serde_json::json!({ "cron_job_id": cron_job_id, "v": 2 }).to_string();
        let second = repo.upsert_artifact(&updated_input).await.unwrap();

        assert_eq!(
            first.conversation_artifact_id,
            second.conversation_artifact_id
        );
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&second.payload).unwrap(),
            serde_json::json!({
                "cron_job_id": updated_input.cron_job_id.as_deref().unwrap(),
                "v": 2
            })
        );

        let listed = repo.list_artifacts(&conv.conversation_id).await.unwrap();
        assert_eq!(listed.len(), 1);
    }

    #[tokio::test]
    async fn artifact_upsert_rejects_invalid_relation_and_payload_shapes() {
        let (repo, _db) = setup().await;
        let mut conv = sample_conversation(TEST_INSTALLATION_OWNER);
        conv.conversation_id = repo.create(&conv).await.unwrap();
        let cron_job_id = insert_cron_job(&repo.pool, "cron_validation").await;
        let valid = sample_artifact(
            conv.conversation_id.clone(),
            "cron_trigger",
            Some(cron_job_id.clone()),
        );

        let cases = [
            {
                let mut row = valid.clone();
                row.conversation_id = "not-a-uuid".into();
                row
            },
            {
                let mut row = valid.clone();
                row.cron_job_id = None;
                row
            },
            {
                let mut row = valid.clone();
                row.cron_job_id = Some("not-a-uuid".into());
                row
            },
            {
                let mut row = valid.clone();
                row.payload = "[]".into();
                row
            },
            {
                let mut row = valid.clone();
                row.payload = r#"{"cron_job_id":7}"#.into();
                row
            },
            {
                let mut row = valid.clone();
                row.payload = serde_json::json!({
                    "cron_job_id": "0190f5fe-7c00-7a00-8abc-012345678999"
                })
                .to_string();
                row
            },
        ];

        for artifact in cases {
            assert!(
                matches!(
                    repo.upsert_artifact(&artifact).await,
                    Err(DbError::Conflict(_))
                ),
                "artifact write unexpectedly accepted: {artifact:?}"
            );
        }

        assert!(repo.list_artifacts(&conv.conversation_id).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn get_and_update_artifact_status_by_business_uuid() {
        let (repo, _db) = setup().await;
        let mut conv = sample_conversation(TEST_INSTALLATION_OWNER);
        conv.conversation_id = repo.create(&conv).await.unwrap();
        let cron_job_id = insert_cron_job(&repo.pool, "cron_u").await;

        let inserted = repo
            .upsert_artifact(&sample_artifact(
                conv.conversation_id.clone(),
                "cron_trigger",
                Some(cron_job_id),
            ))
            .await
            .unwrap();

        let fetched = repo
            .get_artifact(
                &conv.conversation_id,
                &inserted.conversation_artifact_id,
            )
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            fetched.conversation_artifact_id,
            inserted.conversation_artifact_id
        );

        let updated = repo
            .update_artifact_status(
                &conv.conversation_id,
                &inserted.conversation_artifact_id,
                "dismissed",
                nomifun_common::now_ms(),
            )
            .await
            .unwrap()
            .unwrap();
        assert_eq!(updated.status, "dismissed");

        // Missing canonical business UUID → None.
        let missing_id = nomifun_common::generate_id();
        let missing = repo
            .update_artifact_status(
                &conv.conversation_id,
                &missing_id,
                "dismissed",
                nomifun_common::now_ms(),
            )
            .await
            .unwrap();
        assert!(missing.is_none());

        for invalid_id in [
            "42",
            "artifact_0190f5fe-7c00-7a00-8abc-012345678951",
            "0190F5FE-7C00-7A00-8ABC-012345678951",
            "550e8400-e29b-41d4-a716-446655440000",
        ] {
            assert!(matches!(
                repo.get_artifact(&conv.conversation_id, invalid_id).await,
                Err(DbError::Conflict(_))
            ));
            assert!(matches!(
                repo.update_artifact_status(
                    &conv.conversation_id,
                    invalid_id,
                    "dismissed",
                    nomifun_common::now_ms(),
                )
                .await,
                Err(DbError::Conflict(_))
            ));
        }
    }

    // ── conversation_mcp_servers junction tests ─────────────────────

    #[tokio::test]
    async fn set_and_list_mcp_server_ids_preserves_order() {
        let (repo, _db) = setup().await;
        let mut conv = sample_conversation(TEST_INSTALLATION_OWNER);
        conv.conversation_id = repo.create(&conv).await.unwrap();

        let a = insert_mcp_server(&repo.pool, "srv_a").await;
        let b = insert_mcp_server(&repo.pool, "srv_b").await;
        let c = insert_mcp_server(&repo.pool, "srv_c").await;

        // Empty by default.
        assert!(repo.list_mcp_server_ids(&conv.conversation_id).await.unwrap().is_empty());

        // Order is preserved via sort_order, not numeric id order.
        repo.set_mcp_server_ids(&conv.conversation_id, &[c.clone(), a.clone(), b.clone()])
            .await
            .unwrap();
        assert_eq!(
            repo.list_mcp_server_ids(&conv.conversation_id).await.unwrap(),
            vec![c.clone(), a.clone(), b.clone()]
        );

        // set replaces the whole set (DELETE + ordered INSERT).
        repo.set_mcp_server_ids(&conv.conversation_id, std::slice::from_ref(&b))
            .await
            .unwrap();
        assert_eq!(
            repo.list_mcp_server_ids(&conv.conversation_id).await.unwrap(),
            vec![b.clone()]
        );

        // Empty slice clears the selection.
        repo.set_mcp_server_ids(&conv.conversation_id, &[]).await.unwrap();
        assert!(repo.list_mcp_server_ids(&conv.conversation_id).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn deleting_conversation_cleans_up_mcp_junction() {
        let (repo, _db) = setup().await;
        let mut conv = sample_conversation(TEST_INSTALLATION_OWNER);
        conv.conversation_id = repo.create(&conv).await.unwrap();
        let a = insert_mcp_server(&repo.pool, "srv_cascade").await;

        repo.set_mcp_server_ids(&conv.conversation_id, std::slice::from_ref(&a))
            .await
            .unwrap();
        repo.delete(&conv.conversation_id).await.unwrap();

        // Repository-owned logical cleanup removes junction rows.
        let remaining = repo.list_mcp_server_ids(&conv.conversation_id).await.unwrap();
        assert!(remaining.is_empty());
    }

    // ── Message tests ───────────────────────────────────────────────

    #[tokio::test]
    async fn insert_and_get_messages() {
        let (repo, _db) = setup().await;
        let mut conv = sample_conversation(TEST_INSTALLATION_OWNER);
        conv.conversation_id = repo.create(&conv).await.unwrap();

        let msg = sample_message(conv.conversation_id.clone());
        repo.insert_message(&msg).await.unwrap();

        let result = repo.get_messages(&conv.conversation_id, 1, 50, SortOrder::Desc).await.unwrap();
        assert_eq!(result.items.len(), 1);
        assert_eq!(result.total, 1);
        assert_eq!(result.items[0].message_id, msg.message_id);
    }

    #[tokio::test]
    async fn get_messages_pagination() {
        let (repo, _db) = setup().await;
        let mut conv = sample_conversation(TEST_INSTALLATION_OWNER);
        conv.conversation_id = repo.create(&conv).await.unwrap();

        for i in 0..10 {
            let mut msg = sample_message(conv.conversation_id.clone());
            msg.message_id = MessageId::new().into_string();
            msg.msg_id = Some(msg.message_id.clone());
            msg.created_at = (i + 1) * 1000;
            repo.insert_message(&msg).await.unwrap();
        }

        let page1 = repo.get_messages(&conv.conversation_id, 1, 3, SortOrder::Desc).await.unwrap();
        assert_eq!(page1.items.len(), 3);
        assert_eq!(page1.total, 10);
        assert!(page1.has_more);
        // DESC: most recent first
        assert!(page1.items[0].created_at > page1.items[1].created_at);
    }

    #[tokio::test]
    async fn get_messages_asc_order() {
        let (repo, _db) = setup().await;
        let mut conv = sample_conversation(TEST_INSTALLATION_OWNER);
        conv.conversation_id = repo.create(&conv).await.unwrap();

        for i in 0..3 {
            let mut msg = sample_message(conv.conversation_id.clone());
            msg.message_id = MessageId::new().into_string();
            msg.msg_id = Some(msg.message_id.clone());
            msg.created_at = (i + 1) * 1000;
            repo.insert_message(&msg).await.unwrap();
        }

        let result = repo.get_messages(&conv.conversation_id, 1, 50, SortOrder::Asc).await.unwrap();
        assert!(result.items[0].created_at < result.items[1].created_at);
    }

    #[tokio::test]
    async fn update_message_content() {
        let (repo, _db) = setup().await;
        let mut conv = sample_conversation(TEST_INSTALLATION_OWNER);
        conv.conversation_id = repo.create(&conv).await.unwrap();

        let msg = sample_message(conv.conversation_id.clone());
        repo.insert_message(&msg).await.unwrap();

        repo.update_message(
            &msg.message_id,
            &MessageRowUpdate {
                content: Some(r#"{"content":"Updated"}"#.to_string()),
                ..Default::default()
            },
        )
        .await
        .unwrap();

        let result = repo.get_messages(&conv.conversation_id, 1, 50, SortOrder::Desc).await.unwrap();
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
        let mut conv = sample_conversation(TEST_INSTALLATION_OWNER);
        conv.conversation_id = repo.create(&conv).await.unwrap();

        for _ in 0..3 {
            let mut msg = sample_message(conv.conversation_id.clone());
            msg.message_id = MessageId::new().into_string();
            msg.msg_id = Some(msg.message_id.clone());
            repo.insert_message(&msg).await.unwrap();
        }

        repo.delete_messages_by_conversation(&conv.conversation_id).await.unwrap();

        let result = repo.get_messages(&conv.conversation_id, 1, 50, SortOrder::Desc).await.unwrap();
        assert!(result.items.is_empty());
        assert_eq!(result.total, 0);
    }

    #[tokio::test]
    async fn delete_messages_restricts_delivery_history() {
        let (repo, db) = setup().await;
        let mut conv = sample_conversation(TEST_INSTALLATION_OWNER);
        conv.conversation_id = repo.create(&conv).await.unwrap();
        let msg = sample_message(conv.conversation_id.clone());
        repo.insert_message(&msg).await.unwrap();
        let now = nomifun_common::now_ms();
        sqlx::query(
            "INSERT INTO conversation_delivery_receipts \
                (operation_id, message_id, conversation_id, user_id, kind, \
                 request_payload, status, created_at, updated_at) \
             VALUES ('message-delete-fixture', ?, ?, ?, 'turn', '{}', \
                     'accepted', ?, ?)",
        )
        .bind(&msg.message_id)
        .bind(&conv.conversation_id)
        .bind(TEST_INSTALLATION_OWNER)
        .bind(now)
        .bind(now)
        .execute(db.pool())
        .await
        .unwrap();

        let error = repo
            .delete_messages_by_conversation(&conv.conversation_id)
            .await
            .unwrap_err();
        assert!(matches!(error, DbError::Conflict(_)));
        assert!(
            repo.get_message(&conv.conversation_id, &msg.message_id)
                .await
                .unwrap()
                .is_some()
        );
    }

    #[tokio::test]
    async fn delete_messages_ignores_unprojected_wire_owner_tokens() {
        let (repo, _db) = setup().await;
        let mut conv = sample_conversation(TEST_INSTALLATION_OWNER);
        conv.conversation_id = repo.create(&conv).await.unwrap();
        let msg = sample_message(conv.conversation_id.clone());
        repo.insert_message(&msg).await.unwrap();

        let reserved = repo
            .claim_message_correlation(
                &conv.conversation_id,
                &msg.message_id,
                "tool_call",
                "unprojected-wire-owner",
            )
            .await
            .unwrap();

        repo.delete_messages_by_conversation(&conv.conversation_id)
            .await
            .expect("wire owner tokens are not message parent references");
        assert!(
            repo.get_message(&conv.conversation_id, &msg.message_id)
                .await
                .unwrap()
                .is_none()
        );
        assert_eq!(
            repo.claim_message_correlation(
                &conv.conversation_id,
                &msg.message_id,
                "tool_call",
                "unprojected-wire-owner",
            )
            .await
            .unwrap(),
            reserved,
            "the durable correlation reservation survives independently of message projection"
        );
    }

    #[tokio::test]
    async fn get_message_by_msg_id() {
        let (repo, _db) = setup().await;
        let mut conv = sample_conversation(TEST_INSTALLATION_OWNER);
        conv.conversation_id = repo.create(&conv).await.unwrap();

        let msg = sample_message(conv.conversation_id.clone());
        let source_message_id = msg.msg_id.clone().unwrap();
        repo.insert_message(&msg).await.unwrap();

        let found = repo
            .get_message_by_msg_id(&conv.conversation_id, &source_message_id, "text")
            .await
            .unwrap();
        assert!(found.is_some());
        assert_eq!(found.unwrap().message_id, msg.message_id);

        // Wrong type → not found
        let not_found = repo
            .get_message_by_msg_id(&conv.conversation_id, &source_message_id, "tips")
            .await
            .unwrap();
        assert!(not_found.is_none());
    }

    #[tokio::test]
    async fn search_messages_by_keyword() {
        let (repo, _db) = setup().await;
        let mut conv = sample_conversation(TEST_INSTALLATION_OWNER);
        conv.conversation_id = repo.create(&conv).await.unwrap();

        let mut msg1 = sample_message(conv.conversation_id.clone());
        msg1.content = r#"{"content":"Rust 审查报告"}"#.to_string();
        repo.insert_message(&msg1).await.unwrap();

        let mut msg2 = sample_message(conv.conversation_id.clone());
        msg2.message_id = MessageId::new().into_string();
        msg2.msg_id = Some(msg2.message_id.clone());
        msg2.content = r#"{"content":"Python 测试"}"#.to_string();
        repo.insert_message(&msg2).await.unwrap();

        let result = repo.search_messages(TEST_INSTALLATION_OWNER, "审查", 1, 20).await.unwrap();
        assert_eq!(result.items.len(), 1);
        assert_eq!(result.total, 1);
        assert_eq!(result.items[0].conversation_name, "Test Conversation");
    }

    #[tokio::test]
    async fn search_messages_no_match() {
        let (repo, _db) = setup().await;
        let mut conv = sample_conversation(TEST_INSTALLATION_OWNER);
        conv.conversation_id = repo.create(&conv).await.unwrap();

        let msg = sample_message(conv.conversation_id.clone());
        repo.insert_message(&msg).await.unwrap();

        let result = repo
            .search_messages(TEST_INSTALLATION_OWNER, "xxxxnotexist", 1, 20)
            .await
            .unwrap();
        assert!(result.items.is_empty());
        assert_eq!(result.total, 0);
    }

    #[tokio::test]
    async fn search_messages_pagination() {
        let (repo, _db) = setup().await;
        let mut conv = sample_conversation(TEST_INSTALLATION_OWNER);
        conv.conversation_id = repo.create(&conv).await.unwrap();

        for i in 0..5 {
            let mut msg = sample_message(conv.conversation_id.clone());
            msg.message_id = MessageId::new().into_string();
            msg.msg_id = Some(msg.message_id.clone());
            msg.content = format!(r#"{{"content":"match keyword item {i}"}}"#);
            msg.created_at = (i + 1) * 1000;
            repo.insert_message(&msg).await.unwrap();
        }

        let result = repo.search_messages(TEST_INSTALLATION_OWNER, "keyword", 1, 2).await.unwrap();
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
        let mut conv = sample_conversation(TEST_INSTALLATION_OWNER);
        conv.conversation_id = repo.create(&conv).await.unwrap();

        let ids = [
            MessageId::new().into_string(),
            MessageId::new().into_string(),
            MessageId::new().into_string(),
        ];
        let mk = |message_id: &str, created_at: i64| MessageRow {
            id: 0,
            message_id: message_id.to_string(),
            conversation_id: conv.conversation_id.clone(),
            msg_id: Some(message_id.to_string()),
            r#type: "text".to_string(),
            content: r#"{"content":"x"}"#.to_string(),
            position: Some("right".to_string()),
            status: Some("finish".to_string()),
            hidden: false,
            created_at,
        };
        // 三条：t=100,200,300
        repo.insert_message(&mk(&ids[0], 100)).await.unwrap();
        repo.insert_message(&mk(&ids[1], 200)).await.unwrap();
        repo.insert_message(&mk(&ids[2], 300)).await.unwrap();

        // 从 m2 (t=200) 起（含）删除 → 删 m2、m3，留 m1
        let deleted = repo
            .delete_messages_from(&conv.conversation_id, 200, &ids[1])
            .await
            .unwrap();
        assert_eq!(deleted, 2);

        assert!(
            repo.get_message(&conv.conversation_id, &ids[0])
                .await
                .unwrap()
                .is_some()
        );
        assert!(
            repo.get_message(&conv.conversation_id, &ids[1])
                .await
                .unwrap()
                .is_none()
        );
        assert!(
            repo.get_message(&conv.conversation_id, &ids[2])
                .await
                .unwrap()
                .is_none()
        );
    }
}

//! SQLite implementation of the relational preset store.
//!
//! `preset_id` is always a bare UUIDv7 business ID. Builtin and extension
//! catalog lineage belongs in `source_key`.

use nomifun_common::{ProviderId, now_ms};
use sqlx::{Sqlite, SqlitePool, Transaction};

use crate::error::DbError;
use crate::models::*;
use crate::repository::preset::{IPresetRepository, IPresetStateRepository, IPresetTagRepository};

#[derive(Clone, Debug)]
pub struct SqlitePresetRepository { pool: SqlitePool }
impl SqlitePresetRepository { pub fn new(pool: SqlitePool) -> Self { Self { pool } } }

#[derive(Clone, Debug)]
pub struct SqlitePresetStateRepository { pool: SqlitePool }
impl SqlitePresetStateRepository { pub fn new(pool: SqlitePool) -> Self { Self { pool } } }

#[derive(Clone, Debug)]
pub struct SqlitePresetTagRepository { pool: SqlitePool }
impl SqlitePresetTagRepository { pub fn new(pool: SqlitePool) -> Self { Self { pool } } }

fn unique_violation(error: &sqlx::Error) -> bool {
    matches!(error, sqlx::Error::Database(e) if e.code().is_some_and(|c| c == "2067" || c == "1555"))
}

async fn load_record(pool: &SqlitePool, preset_id: &str) -> Result<Option<PresetRecord>, DbError> {
    let Some(preset) = sqlx::query_as::<_, PresetRow>("SELECT * FROM presets WHERE preset_id = ?")
        .bind(preset_id).fetch_optional(pool).await? else { return Ok(None); };
    let localizations = sqlx::query_as::<_, PresetLocalizationRow>(
        "SELECT * FROM preset_localizations WHERE preset_id = ? ORDER BY locale")
        .bind(preset_id).fetch_all(pool).await?;
    let targets = sqlx::query_scalar::<_, String>(
        "SELECT target_kind FROM preset_targets WHERE preset_id = ? ORDER BY target_kind")
        .bind(preset_id).fetch_all(pool).await?;
    let agent_preferences = sqlx::query_as::<_, PresetAgentPreferenceRow>(
        "SELECT * FROM preset_agent_preferences WHERE preset_id = ? ORDER BY rank")
        .bind(preset_id).fetch_all(pool).await?;
    let model_preferences = sqlx::query_as::<_, PresetModelPreferenceRow>(
        "SELECT * FROM preset_model_preferences WHERE preset_id = ? ORDER BY rank")
        .bind(preset_id).fetch_all(pool).await?;
    let skill_bindings = sqlx::query_as::<_, PresetSkillBindingRow>(
        "SELECT * FROM preset_skill_bindings WHERE preset_id = ? ORDER BY binding, sort_order")
        .bind(preset_id).fetch_all(pool).await?;
    let knowledge_policy = sqlx::query_as::<_, PresetKnowledgePolicyRow>(
        "SELECT * FROM preset_knowledge_policy WHERE preset_id = ?")
        .bind(preset_id).fetch_optional(pool).await?;
    let knowledge_bases = sqlx::query_as::<_, PresetKnowledgeBaseRow>(
        "SELECT * FROM preset_knowledge_bases WHERE preset_id = ? ORDER BY sort_order")
        .bind(preset_id).fetch_all(pool).await?;
    let examples = sqlx::query_as::<_, PresetExampleRow>(
        "SELECT * FROM preset_examples WHERE preset_id = ? ORDER BY locale, sort_order")
        .bind(preset_id).fetch_all(pool).await?;
    let tag_bindings = sqlx::query_as::<_, PresetTagBindingRow>(
        "SELECT b.id,b.preset_id,b.preset_tag_id,t.key,b.dimension \
         FROM preset_tag_bindings b \
         JOIN preset_tags t ON t.preset_tag_id = b.preset_tag_id \
         WHERE b.preset_id = ? ORDER BY b.dimension,t.key")
        .bind(preset_id).fetch_all(pool).await?;
    let user_state = sqlx::query_as::<_, PresetUserStateRow>(
        "SELECT * FROM preset_user_state WHERE preset_id = ?")
        .bind(preset_id).fetch_optional(pool).await?;
    Ok(Some(PresetRecord {
        preset: Some(preset), localizations, targets, agent_preferences, model_preferences,
        skill_bindings, knowledge_policy, knowledge_bases, examples, tag_bindings, user_state,
    }))
}

fn catalog_record_matches(record: &PresetRecord, p: &PresetWriteParams) -> bool {
    let Some(root) = record.preset.as_ref() else {
        return false;
    };
    if root.source_kind != p.source_kind
        || root.source_key != p.source_key
        || root.name != p.name
        || root.description != p.description
        || root.routing_description != p.routing_description
        || root.instructions != p.instructions
        || root.avatar != p.avatar
        || root.fallback_allowed != p.fallback_allowed
        || record.knowledge_policy.as_ref().is_none_or(|policy| {
            (
                policy.enabled,
                policy.mode.as_str(),
                policy.writeback,
                policy.eagerness.as_deref(),
                policy.grounded,
            ) != (
                p.knowledge_policy.0,
                p.knowledge_policy.1.as_str(),
                p.knowledge_policy.2,
                p.knowledge_policy.3.as_deref(),
                p.knowledge_policy.4,
            )
        })
    {
        return false;
    }

    let mut localizations = record
        .localizations
        .iter()
        .map(|row| {
            (
                row.locale.clone(),
                row.name.clone(),
                row.description.clone(),
                row.routing_description.clone(),
                row.instructions.clone(),
            )
        })
        .collect::<Vec<_>>();
    let mut expected_localizations = p.localizations.clone();
    localizations.sort();
    expected_localizations.sort();

    let mut targets = record.targets.clone();
    let mut expected_targets = p.targets.clone();
    targets.sort();
    expected_targets.sort();

    let agent_preferences = record
        .agent_preferences
        .iter()
        .map(|row| (row.rank, row.agent_id.clone(), row.required))
        .collect::<Vec<_>>();
    let expected_agent_preferences = p
        .agent_preferences
        .iter()
        .enumerate()
        .map(|(rank, (agent_id, required))| (rank as i64, agent_id.clone(), *required))
        .collect::<Vec<_>>();
    let model_preferences = record
        .model_preferences
        .iter()
        .map(|row| {
            (
                row.rank,
                row.provider_id.clone(),
                row.model.clone(),
                row.required,
            )
        })
        .collect::<Vec<_>>();
    let expected_model_preferences = p
        .model_preferences
        .iter()
        .enumerate()
        .map(|(rank, (provider_id, model, required))| {
            (
                rank as i64,
                provider_id.clone(),
                model.clone(),
                *required,
            )
        })
        .collect::<Vec<_>>();

    let mut skill_bindings = record
        .skill_bindings
        .iter()
        .map(|row| {
            (
                row.sort_order,
                row.skill_name.clone(),
                row.binding.clone(),
                row.required,
            )
        })
        .collect::<Vec<_>>();
    let mut expected_skill_bindings = p
        .skill_bindings
        .iter()
        .enumerate()
        .map(|(sort_order, (skill_name, binding, required))| {
            (
                sort_order as i64,
                skill_name.clone(),
                binding.clone(),
                *required,
            )
        })
        .collect::<Vec<_>>();
    skill_bindings.sort();
    expected_skill_bindings.sort();

    let knowledge_bases = record
        .knowledge_bases
        .iter()
        .map(|row| (row.sort_order, row.knowledge_base_id.clone(), row.required))
        .collect::<Vec<_>>();
    let expected_knowledge_bases = p
        .knowledge_bases
        .iter()
        .enumerate()
        .map(|(sort_order, (knowledge_base_id, required))| {
            (sort_order as i64, knowledge_base_id.clone(), *required)
        })
        .collect::<Vec<_>>();

    let mut examples = record
        .examples
        .iter()
        .map(|row| (row.locale.clone(), row.sort_order, row.prompt.clone()))
        .collect::<Vec<_>>();
    let mut locale_counts = std::collections::HashMap::<&str, i64>::new();
    let mut expected_examples = p
        .examples
        .iter()
        .map(|(locale, prompt)| {
            let sort_order = locale_counts.entry(locale.as_str()).or_default();
            let row = (locale.clone(), *sort_order, prompt.clone());
            *sort_order += 1;
            row
        })
        .collect::<Vec<_>>();
    examples.sort();
    expected_examples.sort();

    let mut tag_bindings = record
        .tag_bindings
        .iter()
        .map(|row| (row.preset_tag_id.clone(), row.dimension.clone()))
        .collect::<Vec<_>>();
    let mut expected_tag_bindings = p.tag_bindings.clone();
    tag_bindings.sort();
    expected_tag_bindings.sort();

    localizations == expected_localizations
        && targets == expected_targets
        && agent_preferences == expected_agent_preferences
        && model_preferences == expected_model_preferences
        && skill_bindings == expected_skill_bindings
        && knowledge_bases == expected_knowledge_bases
        && examples == expected_examples
        && tag_bindings == expected_tag_bindings
}

async fn replace_bindings(
    tx: &mut Transaction<'_, Sqlite>,
    p: &PresetWriteParams,
) -> Result<(), DbError> {
    // Model preferences are hard logical references to providers. Validate
    // every reference before removing any existing binding so an invalid or
    // stale provider can never leave a partially replaced preset aggregate.
    // The UPDATE takes SQLite's writer lock and validates the parent in this
    // same application-owned transaction; no physical FK, trigger, or
    // database-level cascade is involved.
    for (provider_id, _, _) in &p.model_preferences {
        let Some(provider_id) = provider_id else {
            continue;
        };
        ProviderId::parse(provider_id).map_err(|error| {
            DbError::Conflict(format!(
                "Preset model preference provider '{provider_id}' is invalid: {error}"
            ))
        })?;
        let parent = sqlx::query(
            "UPDATE providers SET updated_at = updated_at WHERE provider_id = ?",
        )
        .bind(provider_id)
        .execute(&mut **tx)
        .await?;
        if parent.rows_affected() == 0 {
            return Err(DbError::Conflict(format!(
                "Preset model preference provider '{provider_id}' does not exist"
            )));
        }
    }
    for (agent_id, _) in &p.agent_preferences {
        let parent = sqlx::query(
            "UPDATE agent_metadata SET updated_at = updated_at WHERE agent_id = ?",
        )
        .bind(agent_id)
        .execute(&mut **tx)
        .await?;
        if parent.rows_affected() == 0 {
            return Err(DbError::Conflict(format!(
                "Preset agent preference '{agent_id}' does not exist"
            )));
        }
    }
    for (knowledge_base_id, _) in &p.knowledge_bases {
        let parent = sqlx::query(
            "UPDATE knowledge_bases SET updated_at = updated_at WHERE knowledge_base_id = ?",
        )
        .bind(knowledge_base_id)
        .execute(&mut **tx)
        .await?;
        if parent.rows_affected() == 0 {
            return Err(DbError::Conflict(format!(
                "Preset knowledge base '{knowledge_base_id}' does not exist"
            )));
        }
    }
    for (preset_tag_id, dimension) in &p.tag_bindings {
        nomifun_common::validate_uuidv7(preset_tag_id).map_err(|error| {
            DbError::Conflict(format!(
                "Preset tag id '{preset_tag_id}' is invalid: {error}"
            ))
        })?;
        let dimension_matches = sqlx::query(
            "UPDATE preset_tags SET key = key WHERE preset_tag_id = ? AND dimension = ?",
        )
        .bind(preset_tag_id)
        .bind(dimension)
        .execute(&mut **tx)
        .await?;
        if dimension_matches.rows_affected() == 0 {
            return Err(DbError::Conflict(format!(
                "Preset tag '{preset_tag_id}' does not exist in dimension '{dimension}'"
            )));
        }
    }

    for table in [
        "preset_localizations", "preset_targets", "preset_agent_preferences",
        "preset_model_preferences", "preset_skill_bindings", "preset_knowledge_policy",
        "preset_knowledge_bases", "preset_examples", "preset_tag_bindings",
    ] {
        sqlx::query(&format!("DELETE FROM {table} WHERE preset_id = ?"))
            .bind(&p.preset_id).execute(&mut **tx).await?;
    }
    for (locale, name, description, routing, instructions) in &p.localizations {
        sqlx::query("INSERT INTO preset_localizations (preset_id,locale,name,description,routing_description,instructions) VALUES (?,?,?,?,?,?)")
            .bind(&p.preset_id).bind(locale).bind(name).bind(description).bind(routing).bind(instructions)
            .execute(&mut **tx).await?;
    }
    for target in &p.targets {
        sqlx::query("INSERT INTO preset_targets (preset_id,target_kind) VALUES (?,?)")
            .bind(&p.preset_id).bind(target).execute(&mut **tx).await?;
    }
    for (rank, (agent_id, required)) in p.agent_preferences.iter().enumerate() {
        sqlx::query("INSERT INTO preset_agent_preferences (preset_id,agent_id,rank,required) VALUES (?,?,?,?)")
            .bind(&p.preset_id).bind(agent_id).bind(rank as i64).bind(required).execute(&mut **tx).await?;
    }
    for (rank, (provider_id, model, required)) in p.model_preferences.iter().enumerate() {
        sqlx::query("INSERT INTO preset_model_preferences (preset_id,provider_id,model,rank,required) VALUES (?,?,?,?,?)")
            .bind(&p.preset_id).bind(provider_id).bind(model).bind(rank as i64).bind(required)
            .execute(&mut **tx).await?;
    }
    for (sort_order, (skill_name, binding, required)) in p.skill_bindings.iter().enumerate() {
        sqlx::query("INSERT INTO preset_skill_bindings (preset_id,skill_name,binding,required,sort_order) VALUES (?,?,?,?,?)")
            .bind(&p.preset_id).bind(skill_name).bind(binding).bind(required).bind(sort_order as i64)
            .execute(&mut **tx).await?;
    }
    let (enabled, mode, writeback, eagerness, grounded) = &p.knowledge_policy;
    sqlx::query("INSERT INTO preset_knowledge_policy (preset_id,enabled,mode,writeback,eagerness,grounded) VALUES (?,?,?,?,?,?)")
        .bind(&p.preset_id).bind(enabled).bind(mode).bind(writeback).bind(eagerness).bind(grounded)
        .execute(&mut **tx).await?;
    for (sort_order, (kb, required)) in p.knowledge_bases.iter().enumerate() {
        sqlx::query("INSERT INTO preset_knowledge_bases (preset_id,knowledge_base_id,sort_order,required) VALUES (?,?,?,?)")
            .bind(&p.preset_id).bind(kb).bind(sort_order as i64).bind(required).execute(&mut **tx).await?;
    }
    let mut locale_counts = std::collections::HashMap::<&str, i64>::new();
    for (locale, prompt) in &p.examples {
        let rank = locale_counts.entry(locale.as_str()).or_default();
        sqlx::query("INSERT INTO preset_examples (preset_id,locale,sort_order,prompt) VALUES (?,?,?,?)")
            .bind(&p.preset_id).bind(locale).bind(*rank).bind(prompt).execute(&mut **tx).await?;
        *rank += 1;
    }
    for (preset_tag_id, dimension) in &p.tag_bindings {
        sqlx::query("INSERT INTO preset_tag_bindings (preset_id,preset_tag_id,dimension) VALUES (?,?,?)")
            .bind(&p.preset_id).bind(preset_tag_id).bind(dimension).execute(&mut **tx).await?;
    }
    Ok(())
}

#[async_trait::async_trait]
impl IPresetRepository for SqlitePresetRepository {
    async fn list(&self) -> Result<Vec<PresetRecord>, DbError> {
        let ids = sqlx::query_scalar::<_, String>("SELECT preset_id FROM presets ORDER BY updated_at DESC")
            .fetch_all(&self.pool).await?;
        let mut records = Vec::with_capacity(ids.len());
        for id in ids { if let Some(record) = load_record(&self.pool, &id).await? { records.push(record); } }
        Ok(records)
    }

    async fn get(&self, id: &str) -> Result<Option<PresetRecord>, DbError> { load_record(&self.pool, id).await }

    async fn upsert_catalog(&self, p: &PresetWriteParams) -> Result<PresetRecord, DbError> {
        if !matches!(p.source_kind.as_str(), "builtin" | "extension") {
            return Err(DbError::Conflict(
                "only builtin and extension presets may be materialized as catalog entries".into(),
            ));
        }
        let existing = sqlx::query_scalar::<_, String>(
            "SELECT preset_id FROM presets \
             WHERE source_kind = ? AND source_key = ? \
             LIMIT 1",
        )
        .bind(&p.source_kind)
        .bind(&p.source_key)
        .fetch_optional(&self.pool)
        .await?;
        if let Some(preset_id) = existing {
            let current = load_record(&self.pool, &preset_id)
                .await?
                .ok_or_else(|| DbError::Init("catalog preset lookup lost existing row".into()))?;
            if catalog_record_matches(&current, p) {
                return Ok(current);
            }
            let mut replacement = p.clone();
            replacement.preset_id = preset_id.clone();
            return self
                .update(&preset_id, &replacement)
                .await?
                .ok_or_else(|| DbError::Init("catalog preset upsert lost existing row".into()));
        }
        self.create(p).await
    }

    async fn create(&self, p: &PresetWriteParams) -> Result<PresetRecord, DbError> {
        if p.source_kind == "user" && p.source_key.is_some() {
            return Err(DbError::Conflict(
                "user presets must not carry a catalog source_key".into(),
            ));
        }
        if matches!(p.source_kind.as_str(), "builtin" | "extension")
            && p.source_key.as_deref().is_none_or(str::is_empty)
        {
            return Err(DbError::Conflict(
                "catalog presets require a non-empty source_key".into(),
            ));
        }
        let now = now_ms();
        let mut tx = self.pool.begin().await?;
        let result = sqlx::query("INSERT INTO presets (preset_id,source_kind,source_key,revision,name,description,routing_description,instructions,avatar,fallback_allowed,created_at,updated_at) VALUES (?,?,?,1,?,?,?,?,?,?,?,?)")
            .bind(&p.preset_id).bind(&p.source_kind).bind(&p.source_key).bind(&p.name)
            .bind(&p.description).bind(&p.routing_description).bind(&p.instructions)
            .bind(&p.avatar).bind(p.fallback_allowed).bind(now).bind(now)
            .execute(&mut *tx).await;
        if let Err(error) = result {
            return Err(if unique_violation(&error) { DbError::Conflict(format!("Preset '{}' already exists", p.preset_id)) } else { DbError::Query(error) });
        }
        replace_bindings(&mut tx, p).await?;
        tx.commit().await?;
        load_record(&self.pool, &p.preset_id).await?.ok_or_else(|| DbError::Init("preset create lost row".into()))
    }

    async fn update(&self, id: &str, p: &PresetWriteParams) -> Result<Option<PresetRecord>, DbError> {
        let mut tx = self.pool.begin().await?;
        let result = sqlx::query("UPDATE presets SET source_kind=?,source_key=?,revision=revision+1,name=?,description=?,routing_description=?,instructions=?,avatar=?,fallback_allowed=?,updated_at=? WHERE preset_id=?")
            .bind(&p.source_kind).bind(&p.source_key).bind(&p.name).bind(&p.description)
            .bind(&p.routing_description).bind(&p.instructions).bind(&p.avatar)
            .bind(p.fallback_allowed).bind(now_ms()).bind(id).execute(&mut *tx).await?;
        if result.rows_affected() == 0 { tx.rollback().await?; return Ok(None); }
        let mut replacement = p.clone(); replacement.preset_id = id.to_string();
        replace_bindings(&mut tx, &replacement).await?;
        tx.commit().await?;
        load_record(&self.pool, id).await
    }

    async fn delete(&self, id: &str) -> Result<bool, DbError> {
        let mut tx = self.pool.begin().await?;

        // Take SQLite's writer lock before checking application-owned logical
        // references. No physical FK or trigger owns these lifecycle rules.
        let locked = sqlx::query(
            "UPDATE presets SET updated_at = updated_at WHERE preset_id = ?",
        )
        .bind(id)
        .execute(&mut *tx)
        .await?;
        if locked.rows_affected() == 0 {
            return Ok(false);
        }

        // Execution participants are immutable history snapshots. The
        // registry marks this edge KEEP_HISTORY, so the parent preset must
        // remain addressable while any participant still names it.
        let historical_reference_exists: bool = sqlx::query_scalar(
            "SELECT EXISTS(\
                SELECT 1 FROM agent_execution_participants WHERE preset_id = ?\
             )",
        )
        .bind(id)
        .fetch_one(&mut *tx)
        .await?;
        if historical_reference_exists {
            return Err(DbError::Conflict(format!(
                "Preset '{id}' is retained by execution history"
            )));
        }

        // SET_NULL references retain their frozen revision/snapshot columns;
        // only the live preset reference is detached.
        for table in [
            "conversations",
            "agent_execution_template_participants",
            "cron_jobs",
        ] {
            sqlx::query(&format!(
                "UPDATE {table} SET preset_id = NULL WHERE preset_id = ?"
            ))
            .bind(id)
            .execute(&mut *tx)
            .await?;
        }

        // Application-owned CASCADE for the preset aggregate.
        for table in [
            "preset_agent_preferences",
            "preset_examples",
            "preset_knowledge_bases",
            "preset_localizations",
            "preset_model_preferences",
            "preset_skill_bindings",
            "preset_tag_bindings",
            "preset_targets",
            "preset_knowledge_policy",
            "preset_user_state",
        ] {
            sqlx::query(&format!("DELETE FROM {table} WHERE preset_id = ?"))
                .bind(id)
                .execute(&mut *tx)
                .await?;
        }

        let result = sqlx::query("DELETE FROM presets WHERE preset_id = ?")
            .bind(id)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(result.rows_affected() > 0)
    }

    async fn list_rows(&self) -> Result<Vec<PresetRow>, DbError> {
        Ok(sqlx::query_as::<_, PresetRow>("SELECT * FROM presets ORDER BY updated_at DESC")
            .fetch_all(&self.pool).await?)
    }
}

#[async_trait::async_trait]
impl IPresetStateRepository for SqlitePresetStateRepository {
    async fn get(&self, id: &str) -> Result<Option<PresetUserStateRow>, DbError> {
        Ok(sqlx::query_as("SELECT * FROM preset_user_state WHERE preset_id=?").bind(id).fetch_optional(&self.pool).await?)
    }
    async fn get_all(&self) -> Result<Vec<PresetUserStateRow>, DbError> {
        Ok(sqlx::query_as("SELECT * FROM preset_user_state").fetch_all(&self.pool).await?)
    }
    async fn upsert(&self, p: &UpsertPresetStateParams) -> Result<PresetUserStateRow, DbError> {
        let now = now_ms();
        let mut tx = self.pool.begin().await?;
        let preset = sqlx::query(
            "UPDATE presets SET updated_at = updated_at WHERE preset_id = ?",
        )
        .bind(&p.preset_id)
        .execute(&mut *tx)
        .await?;
        if preset.rows_affected() == 0 {
            return Err(DbError::Conflict(format!(
                "Preset state parent '{}' does not exist",
                p.preset_id
            )));
        }
        if let Some(agent_id) = p.preferred_agent_id.as_deref() {
            let agent = sqlx::query(
                "UPDATE agent_metadata SET updated_at = updated_at WHERE agent_id = ?",
            )
            .bind(agent_id)
            .execute(&mut *tx)
            .await?;
            if agent.rows_affected() == 0 {
                return Err(DbError::Conflict(format!(
                    "Preferred preset agent '{agent_id}' does not exist"
                )));
            }
        }
        sqlx::query("INSERT INTO preset_user_state (preset_id,enabled,auto_selectable,preferred_agent_id,sort_order,last_used_at,updated_at) VALUES (?,?,?,?,?,?,?) ON CONFLICT(preset_id) DO UPDATE SET enabled=excluded.enabled,auto_selectable=excluded.auto_selectable,preferred_agent_id=excluded.preferred_agent_id,sort_order=excluded.sort_order,last_used_at=excluded.last_used_at,updated_at=excluded.updated_at")
            .bind(&p.preset_id).bind(p.enabled).bind(p.auto_selectable).bind(&p.preferred_agent_id).bind(p.sort_order)
            .bind(p.last_used_at).bind(now).execute(&mut *tx).await?;
        let row = sqlx::query_as("SELECT * FROM preset_user_state WHERE preset_id=?")
            .bind(&p.preset_id)
            .fetch_optional(&mut *tx)
            .await?
            .ok_or_else(|| DbError::Init("preset state upsert lost row".into()))?;
        tx.commit().await?;
        Ok(row)
    }
    async fn delete(&self, id: &str) -> Result<bool, DbError> {
        Ok(sqlx::query("DELETE FROM preset_user_state WHERE preset_id=?").bind(id).execute(&self.pool).await?.rows_affected() > 0)
    }
    async fn delete_orphans(&self, valid_ids: &[&str]) -> Result<u64, DbError> {
        if valid_ids.is_empty() { return Ok(sqlx::query("DELETE FROM preset_user_state").execute(&self.pool).await?.rows_affected()); }
        let placeholders = std::iter::repeat_n("?", valid_ids.len()).collect::<Vec<_>>().join(",");
        let sql = format!("DELETE FROM preset_user_state WHERE preset_id NOT IN ({placeholders})");
        let mut q = sqlx::query(&sql);
        for id in valid_ids { q = q.bind(id); }
        Ok(q.execute(&self.pool).await?.rows_affected())
    }
}

#[async_trait::async_trait]
impl IPresetTagRepository for SqlitePresetTagRepository {
    async fn list(&self) -> Result<Vec<PresetTagRow>, DbError> {
        Ok(sqlx::query_as(
            "SELECT id,preset_tag_id,key,dimension,label,sort_order,created_at \
             FROM preset_tags ORDER BY dimension,sort_order,created_at",
        )
        .fetch_all(&self.pool)
        .await?)
    }
    async fn get(&self, preset_tag_id: &str) -> Result<Option<PresetTagRow>, DbError> {
        Ok(sqlx::query_as(
            "SELECT id,preset_tag_id,key,dimension,label,sort_order,created_at \
             FROM preset_tags WHERE preset_tag_id=?",
        )
        .bind(preset_tag_id)
        .fetch_optional(&self.pool)
        .await?)
    }
    async fn get_by_key(&self, key: &str) -> Result<Option<PresetTagRow>, DbError> {
        Ok(sqlx::query_as(
            "SELECT id,preset_tag_id,key,dimension,label,sort_order,created_at \
             FROM preset_tags WHERE key=?",
        )
        .bind(key)
        .fetch_optional(&self.pool)
        .await?)
    }
    async fn create(&self, p: &CreatePresetTagParams<'_>) -> Result<PresetTagRow, DbError> {
        nomifun_common::validate_uuidv7(p.preset_tag_id).map_err(|error| {
            DbError::Conflict(format!("Preset tag id '{}' is invalid: {error}", p.preset_tag_id))
        })?;
        let now = now_ms();
        sqlx::query("INSERT INTO preset_tags (preset_tag_id,key,dimension,label,sort_order,created_at) VALUES (?,?,?,?,?,?)")
            .bind(p.preset_tag_id).bind(p.key).bind(p.dimension).bind(p.label).bind(p.sort_order).bind(now)
            .execute(&self.pool).await.map_err(|e| if unique_violation(&e) { DbError::Conflict(format!("Preset tag '{}' already exists", p.key)) } else { DbError::Query(e) })?;
        self.get(p.preset_tag_id).await?.ok_or_else(|| DbError::Init("preset tag create lost row".into()))
    }
    async fn update(&self, preset_tag_id: &str, p: &UpdatePresetTagParams<'_>) -> Result<Option<PresetTagRow>, DbError> {
        let Some(mut row) = self.get(preset_tag_id).await? else { return Ok(None); };
        if let Some(label) = p.label { row.label = label.to_string(); }
        if let Some(sort) = p.sort_order { row.sort_order = sort; }
        sqlx::query("UPDATE preset_tags SET label=?,sort_order=? WHERE preset_tag_id=?")
            .bind(&row.label).bind(row.sort_order).bind(preset_tag_id).execute(&self.pool).await?;
        Ok(Some(row))
    }
    async fn delete(&self, preset_tag_id: &str) -> Result<bool, DbError> {
        let mut tx = self.pool.begin().await?;
        sqlx::query("DELETE FROM preset_tag_bindings WHERE preset_tag_id=?").bind(preset_tag_id).execute(&mut *tx).await?;
        let changed = sqlx::query("DELETE FROM preset_tags WHERE preset_tag_id=?").bind(preset_tag_id).execute(&mut *tx).await?.rows_affected() > 0;
        tx.commit().await?; Ok(changed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::database::init_database_memory;

    const PRESET_ID: &str = "0190f5fe-7c00-7a00-8abc-012345678911";
    const RETAINED_PRESET_ID: &str = "0190f5fe-7c00-7a00-8abc-012345678912";
    const INVALID_PRESET_ID: &str = "0190f5fe-7c00-7a00-8abc-012345678913";
    const FIXTURE_PROVIDER_ID: &str = "0190f5fe-7c00-7a00-8000-000000000002";
    const MISSING_PROVIDER_ID: &str = "0190f5fe-7c00-7a00-8000-000000000003";
    const FIXTURE_KNOWLEDGE_BASE_ID: &str = "0190f5fe-7c00-7a00-8000-000000000004";
    const FIXTURE_PRESET_TAG_ID: &str = "0190f5fe-7c00-7a00-8000-000000000005";
    const NOMI_AGENT_ID: &str = "0190f5fe-7c00-7a00-8000-000000000114";

    async fn database_with_provider() -> crate::Database {
        let db = init_database_memory().await.unwrap();
        sqlx::query(
            "INSERT INTO providers (\
                provider_id, platform, name, base_url, api_key_encrypted, models, enabled, \
                capabilities, created_at, updated_at\
             ) VALUES (?, 'openai', 'preset fixture', 'https://example.invalid', \
                       'encrypted', '[]', 1, '[]', 1, 1)",
        )
        .bind(FIXTURE_PROVIDER_ID)
        .execute(db.pool())
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO knowledge_bases \
                (knowledge_base_id, name, description, root_path, managed, extra, \
                 created_at, updated_at) \
             VALUES (?, 'preset fixture', '', '/tmp/preset-fixture', 0, '{}', 1, 1)",
        )
        .bind(FIXTURE_KNOWLEDGE_BASE_ID)
        .execute(db.pool())
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO preset_tags \
                (preset_tag_id, key, dimension, label, sort_order, created_at) \
             VALUES (?, 'audience-engineer', 'audience', 'Engineer', 0, ?)",
        )
        .bind(FIXTURE_PRESET_TAG_ID)
        .bind(now_ms())
        .execute(db.pool())
        .await
        .unwrap();
        db
    }

    fn sample(id: &str) -> PresetWriteParams {
        PresetWriteParams {
            preset_id: id.into(), source_kind: "user".into(), source_key: None,
            name: "Research preset".into(), description: Some("Reusable research role".into()),
            routing_description: Some("Use for evidence gathering".into()),
            instructions: "Cite primary sources.".into(), avatar: None, fallback_allowed: true,
            localizations: vec![("zh-CN".into(), Some("研究设定".into()), None, None, Some("引用一手来源。".into()))],
            targets: vec!["conversation".into(), "execution_step".into()],
            agent_preferences: vec![(NOMI_AGENT_ID.into(), false)],
            model_preferences: vec![(Some(FIXTURE_PROVIDER_ID.into()), "model_x".into(), true)],
            skill_bindings: vec![("web-search".into(), "include".into(), true), ("unsafe-auto".into(), "exclude_auto".into(), false)],
            knowledge_policy: (true, "staged".into(), false, Some("conservative".into()), true),
            knowledge_bases: vec![(FIXTURE_KNOWLEDGE_BASE_ID.into(), true)],
            examples: vec![(String::new(), "Research this topic".into())],
            tag_bindings: vec![(FIXTURE_PRESET_TAG_ID.into(), "audience".into())],
        }
    }

    #[tokio::test]
    async fn preset_aggregate_round_trip_and_revision() {
        let db = database_with_provider().await;
        let repo = SqlitePresetRepository::new(db.pool().clone());
        let created = repo.create(&sample(PRESET_ID)).await.unwrap();
        assert!(nomifun_common::validate_uuidv7(PRESET_ID).is_ok());
        assert_eq!(created.preset.as_ref().unwrap().revision, 1);
        assert_eq!(
            created.model_preferences[0].provider_id.as_deref(),
            Some(FIXTURE_PROVIDER_ID)
        );
        assert_eq!(created.skill_bindings.len(), 2);
        assert_eq!(
            created.knowledge_bases[0].knowledge_base_id,
            FIXTURE_KNOWLEDGE_BASE_ID
        );
        let updated = repo.update(PRESET_ID, &sample(PRESET_ID)).await.unwrap().unwrap();
        assert_eq!(updated.preset.unwrap().revision, 2);
    }

    #[tokio::test]
    async fn catalog_upsert_preserves_business_id_and_only_revises_changed_content() {
        const INITIAL_CANDIDATE_ID: &str = "0190f5fe-7c00-7a00-8abc-012345678914";
        const LATER_CANDIDATE_ID: &str = "0190f5fe-7c00-7a00-8abc-012345678915";

        let db = database_with_provider().await;
        let repo = SqlitePresetRepository::new(db.pool().clone());

        let mut initial = sample(INITIAL_CANDIDATE_ID);
        initial.source_kind = "builtin".into();
        initial.source_key = Some("builtin-research".into());
        let created = repo.upsert_catalog(&initial).await.unwrap();
        let created_root = created.preset.unwrap();
        assert_eq!(created_root.preset_id, INITIAL_CANDIDATE_ID);
        assert_eq!(created_root.revision, 1);

        let mut identical = initial.clone();
        identical.preset_id = LATER_CANDIDATE_ID.into();
        let unchanged = repo.upsert_catalog(&identical).await.unwrap();
        let unchanged_root = unchanged.preset.unwrap();
        assert_eq!(unchanged_root.preset_id, INITIAL_CANDIDATE_ID);
        assert_eq!(
            unchanged_root.revision, 1,
            "catalog polling must not create a synthetic revision"
        );

        identical.name = "Updated catalog title".into();
        let updated = repo.upsert_catalog(&identical).await.unwrap();
        let updated_root = updated.preset.unwrap();
        assert_eq!(updated_root.preset_id, INITIAL_CANDIDATE_ID);
        assert_eq!(updated_root.revision, 2);
        assert_eq!(updated_root.name, "Updated catalog title");
        assert!(repo.get(LATER_CANDIDATE_ID).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn preset_provider_bindings_are_canonical_and_atomic() {
        let db = database_with_provider().await;
        let repo = SqlitePresetRepository::new(db.pool().clone());
        repo.create(&sample(PRESET_ID)).await.unwrap();

        let mut invalid_update = sample(PRESET_ID);
        invalid_update.model_preferences =
            vec![(Some(MISSING_PROVIDER_ID.into()), "missing-model".into(), true)];
        let error = repo.update(PRESET_ID, &invalid_update).await.unwrap_err();
        assert!(matches!(error, DbError::Conflict(message) if message.contains("does not exist")));

        let unchanged = repo.get(PRESET_ID).await.unwrap().unwrap();
        assert_eq!(unchanged.preset.as_ref().unwrap().revision, 1);
        assert_eq!(
            unchanged.model_preferences[0].provider_id.as_deref(),
            Some(FIXTURE_PROVIDER_ID)
        );

        let mut invalid_create = sample(INVALID_PRESET_ID);
        invalid_create.model_preferences =
            vec![(Some(MISSING_PROVIDER_ID.into()), "missing-model".into(), true)];
        let error = repo.create(&invalid_create).await.unwrap_err();
        assert!(matches!(error, DbError::Conflict(message) if message.contains("does not exist")));
        assert!(
            repo.get(INVALID_PRESET_ID).await.unwrap().is_none(),
            "failed create must roll back its preset row"
        );
        let binding_count: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM preset_model_preferences WHERE preset_id = ?",
        )
        .bind(INVALID_PRESET_ID)
        .fetch_one(db.pool())
        .await
        .unwrap();
        assert_eq!(binding_count, 0, "failed create must not leave bindings");

        let mut malformed = sample(INVALID_PRESET_ID);
        malformed.model_preferences =
            vec![(Some("not-a-canonical-provider".into()), "bad-model".into(), true)];
        let error = repo.create(&malformed).await.unwrap_err();
        assert!(matches!(error, DbError::Conflict(message) if message.contains("is invalid")));
        assert!(repo.get(INVALID_PRESET_ID).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn delete_applies_registered_preset_policies_atomically() {
        let db = database_with_provider().await;
        let repo = SqlitePresetRepository::new(db.pool().clone());
        repo.create(&sample(PRESET_ID)).await.unwrap();
        let owner = crate::installation_owner_id(db.pool()).await.unwrap();
        let now = now_ms();
        let conversation_id = nomifun_common::ConversationId::new().into_string();

        sqlx::query(
            "INSERT INTO conversations \
                (conversation_id, user_id, name, type, preset_id, preset_revision, \
                 preset_snapshot, created_at, updated_at) \
             VALUES (?, ?, 'preset consumer', 'nomi', ?, 1, '{}', ?, ?)",
        )
        .bind(&conversation_id)
        .bind(&owner)
        .bind(PRESET_ID)
        .bind(now)
        .bind(now)
        .execute(db.pool())
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO agent_execution_template_participants \
                (template_participant_id, template_id, source_agent_id, preset_id, preset_revision, preset_snapshot, \
                 created_at, updated_at) \
             VALUES (?, 'template-fixture', ?, ?, 1, '{}', ?, ?)",
        )
        .bind(nomifun_common::generate_id())
        .bind(NOMI_AGENT_ID)
        .bind(PRESET_ID)
        .bind(now)
        .bind(now)
        .execute(db.pool())
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO cron_jobs \
                (cron_job_id, user_id, name, schedule_kind, schedule_value, payload_message, \
                 preset_id, preset_revision, preset_snapshot, agent_type, created_by, \
                 created_at, updated_at) \
             VALUES (?, ?, 'preset cron', 'every', '60', 'run', ?, 1, '{}', \
                     'acp', 'user', ?, ?)",
        )
        .bind(nomifun_common::CronJobId::new().as_str())
        .bind(&owner)
        .bind(PRESET_ID)
        .bind(now)
        .bind(now)
        .execute(db.pool())
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO preset_user_state \
                (preset_id, enabled, auto_selectable, updated_at) \
             VALUES (?, 1, 0, ?)",
        )
        .bind(PRESET_ID)
        .bind(now)
        .execute(db.pool())
        .await
        .unwrap();

        assert!(repo.delete(PRESET_ID).await.unwrap());

        let preset_count: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM presets WHERE preset_id = ?")
                .bind(PRESET_ID)
                .fetch_one(db.pool())
                .await
                .unwrap();
        assert_eq!(preset_count, 0);
        for table in [
            "preset_agent_preferences",
            "preset_examples",
            "preset_knowledge_bases",
            "preset_localizations",
            "preset_model_preferences",
            "preset_skill_bindings",
            "preset_tag_bindings",
            "preset_targets",
            "preset_knowledge_policy",
            "preset_user_state",
        ] {
            let count: i64 =
                sqlx::query_scalar(&format!("SELECT COUNT(*) FROM {table} WHERE preset_id = ?"))
                    .bind(PRESET_ID)
                    .fetch_one(db.pool())
                    .await
                    .unwrap();
            assert_eq!(
                count, 0,
                "{table} must be explicitly deleted with its preset"
            );
        }

        let conversation_preset: Option<String> =
            sqlx::query_scalar("SELECT preset_id FROM conversations WHERE conversation_id = ?")
                .bind(&conversation_id)
                .fetch_one(db.pool())
                .await
                .unwrap();
        let template_preset: Option<String> = sqlx::query_scalar(
            "SELECT preset_id FROM agent_execution_template_participants \
             WHERE template_id = 'template-fixture'",
        )
        .fetch_one(db.pool())
        .await
        .unwrap();
        let cron_preset: Option<String> =
            sqlx::query_scalar("SELECT preset_id FROM cron_jobs WHERE name = 'preset cron'")
                .fetch_one(db.pool())
                .await
                .unwrap();
        assert!(conversation_preset.is_none());
        assert!(template_preset.is_none());
        assert!(cron_preset.is_none());
    }

    #[tokio::test]
    async fn delete_restricts_keep_history_execution_participant() {
        let db = database_with_provider().await;
        let repo = SqlitePresetRepository::new(db.pool().clone());
        repo.create(&sample(RETAINED_PRESET_ID)).await.unwrap();
        sqlx::query(
            "INSERT INTO agent_execution_participants \
                (participant_id, execution_id, source_agent_id, preset_id, introduced_in_revision, created_at) \
             VALUES (?, ?, ?, ?, 0, ?)",
        )
        .bind(nomifun_common::generate_id())
        .bind(nomifun_common::AgentExecutionId::new().as_str())
        .bind(NOMI_AGENT_ID)
        .bind(RETAINED_PRESET_ID)
        .bind(now_ms())
        .execute(db.pool())
        .await
        .unwrap();

        let error = repo.delete(RETAINED_PRESET_ID).await.unwrap_err();
        assert!(matches!(error, DbError::Conflict(_)));
        assert!(repo.get(RETAINED_PRESET_ID).await.unwrap().is_some());
    }

    #[tokio::test]
    async fn v3_baseline_contains_only_the_preset_catalog_tables() {
        let db = init_database_memory().await.unwrap();
        for table in ["assistants", "assistant_overrides", "assistant_tags"] {
            let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name=?")
                .bind(table).fetch_one(db.pool()).await.unwrap();
            assert_eq!(count, 0, "retired table {table} must not exist in v3");
        }
        for table in ["presets", "preset_agent_preferences", "preset_model_preferences", "preset_skill_bindings", "preset_knowledge_bases", "preset_user_state"] {
            let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name=?")
                .bind(table).fetch_one(db.pool()).await.unwrap();
            assert_eq!(count, 1, "preset table {table} must exist");
        }
    }

    #[tokio::test]
    async fn preset_tags_use_local_integer_ids_uuidv7_business_ids_and_catalog_keys() {
        let db = init_database_memory().await.unwrap();
        let repo = SqlitePresetTagRepository::new(db.pool().clone());
        let first_preset_tag_id = nomifun_common::generate_id();
        let first = repo
            .create(&CreatePresetTagParams {
                preset_tag_id: &first_preset_tag_id,
                key: "research",
                dimension: "scenario",
                label: "Research",
                sort_order: 0,
            })
            .await
            .unwrap();
        let second_preset_tag_id = nomifun_common::generate_id();
        let second = repo
            .create(&CreatePresetTagParams {
                preset_tag_id: &second_preset_tag_id,
                key: "research-2",
                dimension: "scenario",
                label: "Research",
                sort_order: 1,
            })
            .await
            .unwrap();

        assert!(first.id > 0);
        assert_eq!(second.id, first.id + 1);
        assert_eq!(first.preset_tag_id, first_preset_tag_id);
        assert_eq!(second.preset_tag_id, second_preset_tag_id);
        assert!(nomifun_common::validate_uuidv7(&first.preset_tag_id).is_ok());
        assert!(nomifun_common::validate_uuidv7(&second.preset_tag_id).is_ok());
        assert_eq!(first.key, "research");
        assert_eq!(second.key, "research-2");
    }
}

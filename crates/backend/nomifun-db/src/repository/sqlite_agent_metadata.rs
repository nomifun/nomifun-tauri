//! SQLite-backed agent metadata repository.

use nomifun_common::now_ms;
use sqlx::SqlitePool;

use crate::error::DbError;
use crate::models::{AgentMetadataRow, UpdateAgentHandshakeParams, UpsertAgentMetadataParams};
use crate::repository::agent_metadata::IAgentMetadataRepository;

#[derive(Clone, Debug)]
pub struct SqliteAgentMetadataRepository {
    pool: SqlitePool,
}

impl SqliteAgentMetadataRepository {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }
}

#[async_trait::async_trait]
impl IAgentMetadataRepository for SqliteAgentMetadataRepository {
    async fn list_all(&self) -> Result<Vec<AgentMetadataRow>, DbError> {
        let rows =
            sqlx::query_as::<_, AgentMetadataRow>("SELECT * FROM agent_metadata ORDER BY sort_order ASC, name ASC")
                .fetch_all(&self.pool)
                .await?;
        Ok(rows)
    }

    async fn get(&self, id: &str) -> Result<Option<AgentMetadataRow>, DbError> {
        let row = sqlx::query_as::<_, AgentMetadataRow>(
            "SELECT * FROM agent_metadata WHERE agent_id = ?",
        )
            .bind(id)
            .fetch_optional(&self.pool)
            .await?;
        Ok(row)
    }

    async fn find_by_source_and_name(
        &self,
        agent_source: &str,
        name: &str,
    ) -> Result<Option<AgentMetadataRow>, DbError> {
        let row =
            sqlx::query_as::<_, AgentMetadataRow>("SELECT * FROM agent_metadata WHERE agent_source = ? AND name = ?")
                .bind(agent_source)
                .bind(name)
                .fetch_optional(&self.pool)
                .await?;
        Ok(row)
    }

    async fn find_builtin_by_backend(&self, backend: &str) -> Result<Option<AgentMetadataRow>, DbError> {
        let row = sqlx::query_as::<_, AgentMetadataRow>(
            "SELECT * FROM agent_metadata \
             WHERE agent_source = 'builtin' AND backend = ? \
             ORDER BY sort_order ASC, name ASC LIMIT 1",
        )
        .bind(backend)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row)
    }

    async fn upsert(&self, params: &UpsertAgentMetadataParams<'_>) -> Result<AgentMetadataRow, DbError> {
        let now = now_ms();

        sqlx::query(
            "INSERT INTO agent_metadata \
                (agent_id, icon, name, name_i18n, description, description_i18n, \
                 backend, agent_type, agent_source, agent_source_info, \
                 source_key, \
                 enabled, command, args, env, native_skills_dirs, \
                 behavior_policy, yolo_id, \
                 agent_capabilities, auth_methods, config_options, \
                 available_modes, available_models, available_commands, \
                 sort_order, created_at, updated_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, NULL, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?) \
             ON CONFLICT(agent_id) DO UPDATE SET \
                icon = excluded.icon, \
                name = excluded.name, \
                name_i18n = excluded.name_i18n, \
                description = excluded.description, \
                description_i18n = excluded.description_i18n, \
                backend = excluded.backend, \
                agent_type = excluded.agent_type, \
                agent_source = excluded.agent_source, \
                agent_source_info = excluded.agent_source_info, \
                enabled = excluded.enabled, \
                command = excluded.command, \
                args = excluded.args, \
                env = excluded.env, \
                native_skills_dirs = excluded.native_skills_dirs, \
                behavior_policy = excluded.behavior_policy, \
                yolo_id = excluded.yolo_id, \
                agent_capabilities = excluded.agent_capabilities, \
                auth_methods = excluded.auth_methods, \
                config_options = excluded.config_options, \
                available_modes = excluded.available_modes, \
                available_models = excluded.available_models, \
                available_commands = excluded.available_commands, \
                sort_order = excluded.sort_order, \
                updated_at = excluded.updated_at",
        )
        .bind(params.agent_id)
        .bind(params.icon)
        .bind(params.name)
        .bind(params.name_i18n)
        .bind(params.description)
        .bind(params.description_i18n)
        .bind(params.backend)
        .bind(params.agent_type)
        .bind(params.agent_source)
        .bind(params.agent_source_info)
        .bind(params.enabled)
        .bind(params.command)
        .bind(params.args)
        .bind(params.env)
        .bind(params.native_skills_dirs)
        .bind(params.behavior_policy)
        .bind(params.yolo_id)
        .bind(params.agent_capabilities)
        .bind(params.auth_methods)
        .bind(params.config_options)
        .bind(params.available_modes)
        .bind(params.available_models)
        .bind(params.available_commands)
        .bind(params.sort_order)
        .bind(now)
        .bind(now)
        .execute(&self.pool)
        .await?;

        let row = self
            .get(params.agent_id)
            .await?
            .ok_or_else(|| DbError::Init(format!("upsert did not produce row for id '{}'", params.agent_id)))?;
        Ok(row)
    }

    async fn apply_handshake(
        &self,
        id: &str,
        params: &UpdateAgentHandshakeParams<'_>,
    ) -> Result<Option<AgentMetadataRow>, DbError> {
        let Some(existing) = self.get(id).await? else {
            return Ok(None);
        };

        let now = now_ms();
        let agent_capabilities = params
            .agent_capabilities
            .map_or(existing.agent_capabilities, |v| v.map(String::from));
        let auth_methods = params
            .auth_methods
            .map_or(existing.auth_methods, |v| v.map(String::from));
        let config_options = params
            .config_options
            .map_or(existing.config_options, |v| v.map(String::from));
        let available_modes = params
            .available_modes
            .map_or(existing.available_modes, |v| v.map(String::from));
        let available_models = params
            .available_models
            .map_or(existing.available_models, |v| v.map(String::from));
        let available_commands = params
            .available_commands
            .map_or(existing.available_commands, |v| v.map(String::from));

        sqlx::query(
            "UPDATE agent_metadata SET \
                agent_capabilities = ?, \
                auth_methods = ?, \
                config_options = ?, \
                available_modes = ?, \
                available_models = ?, \
                available_commands = ?, \
                updated_at = ? \
             WHERE agent_id = ?",
        )
        .bind(&agent_capabilities)
        .bind(&auth_methods)
        .bind(&config_options)
        .bind(&available_modes)
        .bind(&available_models)
        .bind(&available_commands)
        .bind(now)
        .bind(id)
        .execute(&self.pool)
        .await?;

        self.get(id).await
    }

    async fn set_enabled(&self, id: &str, enabled: bool) -> Result<bool, DbError> {
        let now = now_ms();
        let result = sqlx::query(
            "UPDATE agent_metadata SET enabled = ?, updated_at = ? WHERE agent_id = ?",
        )
            .bind(enabled)
            .bind(now)
            .bind(id)
            .execute(&self.pool)
            .await?;
        Ok(result.rows_affected() > 0)
    }

    async fn set_behavior_policy(
        &self,
        id: &str,
        behavior_policy: &str,
    ) -> Result<Option<AgentMetadataRow>, DbError> {
        if self.get(id).await?.is_none() {
            return Ok(None);
        }
        let now = now_ms();
        sqlx::query(
            "UPDATE agent_metadata SET behavior_policy = ?, updated_at = ? WHERE agent_id = ?",
        )
            .bind(behavior_policy)
            .bind(now)
            .bind(id)
            .execute(&self.pool)
            .await?;
        self.get(id).await
    }

    async fn delete(&self, id: &str) -> Result<bool, DbError> {
        let mut tx = self.pool.begin().await?;
        let locked = sqlx::query(
            "UPDATE agent_metadata SET updated_at = updated_at WHERE agent_id = ?",
        )
            .bind(id)
            .execute(&mut *tx)
            .await?;
        if locked.rows_affected() == 0 {
            return Ok(false);
        }

        // KEEP_HISTORY and RESTRICT references both require the agent row
        // to remain present. This is checked under the same writer
        // transaction as the eventual deletion.
        let retained_reference_exists: bool = sqlx::query_scalar(
            "SELECT \
                EXISTS(\
                    SELECT 1 FROM agent_execution_participants \
                    WHERE source_agent_id = ?1\
                ) \
                OR EXISTS(\
                    SELECT 1 FROM agent_execution_template_participants \
                    WHERE source_agent_id = ?1\
                ) \
                OR EXISTS(\
                    SELECT 1 FROM preset_agent_preferences \
                    WHERE agent_id = ?1\
                ) \
                OR EXISTS(\
                    SELECT 1 FROM acp_session \
                    WHERE agent_id = ?1\
                ) \
                OR EXISTS(\
                    SELECT 1 FROM conversations \
                    WHERE json_extract(extra, '$.agent_id') = ?1 \
                       OR json_extract(extra, '$.custom_agent_id') = ?1\
                )",
        )
        .bind(id)
        .fetch_one(&mut *tx)
        .await?;
        if retained_reference_exists {
            return Err(DbError::Conflict(format!(
                "Agent '{id}' is still referenced by execution, preset, or ACP state"
            )));
        }

        sqlx::query(
            "UPDATE preset_user_state \
             SET preferred_agent_id = NULL \
             WHERE preferred_agent_id = ?",
        )
        .bind(id)
        .execute(&mut *tx)
        .await?;

        let result = sqlx::query("DELETE FROM agent_metadata WHERE agent_id = ?")
            .bind(id)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(result.rows_affected() > 0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::init_database_memory;

    const CUSTOM_AGENT_ID: &str = "0190f5fe-7c00-7a00-8abc-012345678921";
    const OTHER_CUSTOM_AGENT_ID: &str = "0190f5fe-7c00-7a00-8abc-012345678922";
    const CLAUDE_AGENT_ID: &str = "0190f5fe-7c00-7a00-8000-000000000101";
    const OPENCODE_AGENT_ID: &str = "0190f5fe-7c00-7a00-8000-00000000010a";
    const CURSOR_AGENT_ID: &str = "0190f5fe-7c00-7a00-8000-00000000010e";
    const KIRO_AGENT_ID: &str = "0190f5fe-7c00-7a00-8000-00000000010f";
    const NOMI_AGENT_ID: &str = "0190f5fe-7c00-7a00-8000-000000000114";
    const DELETE_FIXTURE_PRESET_ID: &str = "0190f5fe-7c00-7a00-8abc-012345678923";

    async fn setup() -> (SqliteAgentMetadataRepository, crate::Database) {
        let db = init_database_memory().await.unwrap();
        let repo = SqliteAgentMetadataRepository::new(db.pool().clone());
        (repo, db)
    }

    fn custom_params<'a>(id: &'a str, name: &'a str) -> UpsertAgentMetadataParams<'a> {
        UpsertAgentMetadataParams {
            agent_id: id,
            icon: None,
            name,
            name_i18n: None,
            description: Some("a custom agent"),
            description_i18n: None,
            backend: Some("claude"),
            agent_type: "acp",
            agent_source: "custom",
            agent_source_info: Some(r#"{"binary_name":"claude"}"#),
            enabled: true,
            command: Some("claude"),
            args: Some("[]"),
            env: Some("[]"),
            native_skills_dirs: Some(r#"[".claude/skills"]"#),
            behavior_policy: Some(r#"{"supports_side_question":true}"#),
            yolo_id: Some("bypassPermissions"),
            agent_capabilities: None,
            auth_methods: None,
            config_options: None,
            available_modes: None,
            available_models: None,
            available_commands: None,
            sort_order: 1100,
        }
    }

    #[tokio::test]
    async fn seed_rows_populated_after_migrations() {
        let (repo, _db) = setup().await;
        let rows = repo.list_all().await.unwrap();
        // 17 ACP vendors + 2 non-ACP builtins + 1 internal = 20.
        assert_eq!(rows.len(), 20);
        assert!(
            rows.iter()
                .any(|r| r.name == "Claude Code" && r.agent_source == "builtin")
        );
        assert!(rows.iter().any(|r| r.name == "Nomi" && r.agent_source == "internal"));
        // Nanobot and OpenClaw are builtin (not internal).
        assert!(rows.iter().any(|r| r.name == "Nanobot" && r.agent_source == "builtin"));
        assert!(rows.iter().any(|r| r.name == "OpenClaw" && r.agent_source == "builtin"));
        assert!(rows
            .iter()
            .all(|r| nomifun_common::validate_uuidv7(&r.agent_id).is_ok()));
        assert!(rows.iter().all(|r| !r.agent_id.starts_with("agent_builtin_")));
        assert!(rows
            .iter()
            .filter(|r| matches!(r.agent_source.as_str(), "builtin" | "internal"))
            .all(|r| r
                .source_key
                .as_deref()
                .is_some_and(|key| key.starts_with("agent_builtin_"))));
    }

    #[tokio::test]
    async fn builtins_use_current_official_cli_names_after_migrations() {
        let (repo, _db) = setup().await;

        let cursor = repo.get(CURSOR_AGENT_ID).await.unwrap().expect("seeded cursor row");
        assert_eq!(cursor.agent_source_info.as_deref(), Some(r#"{"binary_name":"agent"}"#));
        assert_eq!(cursor.command.as_deref(), Some("agent"));
        assert_eq!(cursor.args.as_deref(), Some(r#"["acp"]"#));

        let kiro = repo.get(KIRO_AGENT_ID).await.unwrap().expect("seeded kiro row");
        assert_eq!(kiro.agent_source_info.as_deref(), Some(r#"{"binary_name":"kiro-cli"}"#));
        assert_eq!(kiro.command.as_deref(), Some("kiro-cli"));
        assert_eq!(kiro.args.as_deref(), Some(r#"["acp"]"#));
    }

    #[tokio::test]
    async fn find_by_source_and_name_hits_seed_row() {
        let (repo, _db) = setup().await;
        let row = repo
            .find_by_source_and_name("builtin", "Claude Code")
            .await
            .unwrap()
            .expect("seeded claude row");
        assert_eq!(row.backend.as_deref(), Some("claude"));
        assert_eq!(row.agent_type, "acp");
    }

    #[tokio::test]
    async fn seed_rows_include_icon_backfill() {
        let (repo, _db) = setup().await;

        let claude = repo.get(CLAUDE_AGENT_ID).await.unwrap().expect("seeded claude row");
        assert_eq!(claude.icon.as_deref(), Some("/api/assets/logos/ai-major/claude.svg"));
        assert_eq!(claude.source_key.as_deref(), Some("agent_builtin_claude"));

        let nomi = repo.get(NOMI_AGENT_ID).await.unwrap().expect("seeded nomi row");
        assert_eq!(nomi.icon.as_deref(), Some("/api/assets/logos/brand/nomi.svg"));

        let kiro = repo.get(KIRO_AGENT_ID).await.unwrap().expect("seeded kiro row");
        assert!(kiro.icon.is_none());
    }

    #[tokio::test]
    async fn upsert_inserts_then_updates() {
        let (repo, _db) = setup().await;
        let mut p = custom_params(CUSTOM_AGENT_ID, "my-claude");
        let first = repo.upsert(&p).await.unwrap();
        assert_eq!(first.name, "my-claude");
        assert!(first.enabled);
        assert!(first.source_key.is_none());

        p.description = Some("updated");
        p.enabled = false;
        let second = repo.upsert(&p).await.unwrap();
        assert_eq!(second.description.as_deref(), Some("updated"));
        assert!(!second.enabled);
        // No duplicate row introduced.
        let matches: Vec<_> = repo
            .list_all()
            .await
            .unwrap()
            .into_iter()
            .filter(|r| r.agent_id == CUSTOM_AGENT_ID)
            .collect();
        assert_eq!(matches.len(), 1);
    }

    #[tokio::test]
    async fn upsert_preserves_builtin_source_key_lineage() {
        let (repo, _db) = setup().await;
        let mut p = custom_params(CLAUDE_AGENT_ID, "updated Claude");
        p.agent_source = "builtin";

        let row = repo.upsert(&p).await.unwrap();

        assert_eq!(row.agent_id, CLAUDE_AGENT_ID);
        assert_eq!(row.source_key.as_deref(), Some("agent_builtin_claude"));
    }

    #[tokio::test]
    async fn apply_handshake_updates_only_specified_fields() {
        let (repo, _db) = setup().await;
        let updated = repo
            .apply_handshake(
                CLAUDE_AGENT_ID,
                &UpdateAgentHandshakeParams {
                    agent_capabilities: Some(Some(r#"{"loadSession":true}"#)),
                    auth_methods: Some(Some(r#"[{"id":"oauth"}]"#)),
                    ..Default::default()
                },
            )
            .await
            .unwrap()
            .expect("claude row exists");

        assert_eq!(updated.agent_capabilities.as_deref(), Some(r#"{"loadSession":true}"#));
        assert_eq!(updated.auth_methods.as_deref(), Some(r#"[{"id":"oauth"}]"#));
        assert!(updated.config_options.is_none());
    }

    #[tokio::test]
    async fn apply_handshake_can_clear_to_null() {
        let (repo, _db) = setup().await;
        repo.apply_handshake(
            CLAUDE_AGENT_ID,
            &UpdateAgentHandshakeParams {
                agent_capabilities: Some(Some(r#"{"x":1}"#)),
                ..Default::default()
            },
        )
        .await
        .unwrap();

        let cleared = repo
            .apply_handshake(
                CLAUDE_AGENT_ID,
                &UpdateAgentHandshakeParams {
                    agent_capabilities: Some(None),
                    ..Default::default()
                },
            )
            .await
            .unwrap()
            .unwrap();
        assert!(cleared.agent_capabilities.is_none());
    }

    #[tokio::test]
    async fn apply_handshake_missing_row_returns_none() {
        let (repo, _db) = setup().await;
        let res = repo
            .apply_handshake(
                "does-not-exist",
                &UpdateAgentHandshakeParams {
                    agent_capabilities: Some(Some("{}")),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        assert!(res.is_none());
    }

    #[tokio::test]
    async fn set_enabled_toggles_flag() {
        let (repo, _db) = setup().await;
        assert!(repo.set_enabled(CLAUDE_AGENT_ID, false).await.unwrap());
        let row = repo.get(CLAUDE_AGENT_ID).await.unwrap().unwrap();
        assert!(!row.enabled);
        assert!(!repo.set_enabled("missing", true).await.unwrap());
    }

    #[tokio::test]
    async fn set_behavior_policy_overwrites_column_and_misses_unknown_row() {
        let (repo, _db) = setup().await;
        let updated = repo
            .set_behavior_policy(OPENCODE_AGENT_ID, r#"{"supports_side_question":true}"#)
            .await
            .unwrap()
            .expect("opencode row exists");
        assert_eq!(
            updated.behavior_policy.as_deref(),
            Some(r#"{"supports_side_question":true}"#)
        );
        // Re-read confirms persistence.
        let row = repo.get(OPENCODE_AGENT_ID).await.unwrap().unwrap();
        assert_eq!(
            row.behavior_policy.as_deref(),
            Some(r#"{"supports_side_question":true}"#)
        );
        // Unknown id is a no-op returning None.
        assert!(repo.set_behavior_policy("missing", "{}").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn delete_removes_row() {
        let (repo, _db) = setup().await;
        let p = custom_params(CUSTOM_AGENT_ID, "throwaway");
        repo.upsert(&p).await.unwrap();
        assert!(repo.delete(CUSTOM_AGENT_ID).await.unwrap());
        assert!(repo.get(CUSTOM_AGENT_ID).await.unwrap().is_none());
        assert!(!repo.delete(CUSTOM_AGENT_ID).await.unwrap());
    }

    #[tokio::test]
    async fn delete_restricts_live_references_and_clears_preferred_agent() {
        let (repo, db) = setup().await;
        repo.upsert(&custom_params(CUSTOM_AGENT_ID, "referenced"))
            .await
            .unwrap();
        let now = now_ms();
        sqlx::query(
            "INSERT INTO presets \
                (preset_id, source_kind, name, instructions, created_at, updated_at) \
             VALUES (?, 'user', 'fixture', '', ?, ?)",
        )
        .bind(DELETE_FIXTURE_PRESET_ID)
        .bind(now)
        .bind(now)
        .execute(db.pool())
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO preset_agent_preferences \
                (preset_id, agent_id, rank, required) \
             VALUES (?, ?, 0, 1)",
        )
        .bind(DELETE_FIXTURE_PRESET_ID)
        .bind(CUSTOM_AGENT_ID)
        .execute(db.pool())
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO preset_user_state \
                (preset_id, enabled, auto_selectable, preferred_agent_id, updated_at) \
             VALUES (?, 1, 0, ?, ?)",
        )
        .bind(DELETE_FIXTURE_PRESET_ID)
        .bind(CUSTOM_AGENT_ID)
        .bind(now)
        .execute(db.pool())
        .await
        .unwrap();

        let error = repo.delete(CUSTOM_AGENT_ID).await.unwrap_err();
        assert!(matches!(error, DbError::Conflict(_)));

        sqlx::query(
            "DELETE FROM preset_agent_preferences \
             WHERE preset_id = ?",
        )
        .bind(DELETE_FIXTURE_PRESET_ID)
        .execute(db.pool())
        .await
        .unwrap();
        assert!(repo.delete(CUSTOM_AGENT_ID).await.unwrap());

        let preferred_agent_id: Option<String> = sqlx::query_scalar(
            "SELECT preferred_agent_id FROM preset_user_state \
             WHERE preset_id = ?",
        )
        .bind(DELETE_FIXTURE_PRESET_ID)
        .fetch_one(db.pool())
        .await
        .unwrap();
        assert!(preferred_agent_id.is_none());
    }

    #[tokio::test]
    async fn delete_restricts_keep_history_execution_participant() {
        let (repo, db) = setup().await;
        repo.upsert(&custom_params(CUSTOM_AGENT_ID, "historical"))
            .await
            .unwrap();
        sqlx::query(
            "INSERT INTO agent_execution_participants \
                (participant_id, execution_id, source_agent_id, introduced_in_revision, created_at) \
             VALUES (?, ?, ?, 0, ?)",
        )
        .bind(nomifun_common::generate_id())
        .bind(nomifun_common::AgentExecutionId::new().as_str())
        .bind(CUSTOM_AGENT_ID)
        .bind(now_ms())
        .execute(db.pool())
        .await
        .unwrap();

        let error = repo.delete(CUSTOM_AGENT_ID).await.unwrap_err();
        assert!(matches!(error, DbError::Conflict(_)));
        assert!(repo.get(CUSTOM_AGENT_ID).await.unwrap().is_some());
    }

    #[tokio::test]
    async fn same_source_same_name_allowed_with_different_ids() {
        let (repo, _db) = setup().await;
        let p1 = custom_params(CUSTOM_AGENT_ID, "dup");
        let p2 = custom_params(OTHER_CUSTOM_AGENT_ID, "dup");
        repo.upsert(&p1).await.unwrap();
        repo.upsert(&p2).await.unwrap();
        let all = repo.list_all().await.unwrap();
        let dup_count = all
            .iter()
            .filter(|r| r.name == "dup" && r.agent_source == "custom")
            .count();
        assert_eq!(
            dup_count, 2,
            "both rows should coexist after dropping UNIQUE(agent_source,name)"
        );
    }
}

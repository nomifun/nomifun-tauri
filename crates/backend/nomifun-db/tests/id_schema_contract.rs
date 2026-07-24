use nomifun_common::{ConversationId, TerminalId, validate_uuidv7};
use nomifun_db::{init_database_memory, validate_id_schema_contract};
use sqlx::Row;

const BASELINE: &str = include_str!("../migrations/001_v3_baseline.sql");

fn executable_baseline_sql() -> String {
    BASELINE
        .lines()
        .filter(|line| !line.trim_start().starts_with("--"))
        .collect::<Vec<_>>()
        .join("\n")
}

fn sql_tokens(sql: &str) -> Vec<String> {
    sql.split(|ch: char| !(ch.is_ascii_alphanumeric() || ch == '_'))
        .filter(|token| !token.is_empty())
        .map(str::to_ascii_uppercase)
        .collect()
}

const UNCONDITIONAL_UUIDV7_BUSINESS_IDS: &[(&str, &str)] = &[
    ("users", "user_id"),
    ("conversations", "conversation_id"),
    ("messages", "message_id"),
    ("terminal_sessions", "terminal_id"),
    ("terminal_turn_admissions", "turn_token"),
    ("providers", "provider_id"),
    ("agent_execution_templates", "execution_template_id"),
    ("agent_executions", "execution_id"),
    ("agent_metadata", "agent_id"),
    ("knowledge_bases", "knowledge_base_id"),
    ("knowledge_bindings", "knowledge_binding_id"),
    ("attachments", "attachment_id"),
    ("remote_agents", "remote_agent_id"),
    ("workshop_canvases", "canvas_id"),
    ("workshop_assets", "asset_id"),
    ("channel_plugins", "channel_plugin_id"),
    ("channel_sessions", "channel_session_id"),
    ("channel_users", "channel_user_id"),
    ("agent_execution_participants", "participant_id"),
    ("agent_execution_steps", "step_id"),
    ("agent_execution_attempts", "attempt_id"),
    (
        "agent_execution_template_participants",
        "template_participant_id",
    ),
    ("cron_jobs", "cron_job_id"),
    ("cron_job_runs", "cron_job_run_id"),
    ("requirements", "requirement_id"),
    ("mcp_servers", "mcp_server_id"),
    ("webhooks", "webhook_id"),
    ("connector_credentials", "credential_id"),
    ("creation_tasks", "creation_task_id"),
    ("conversation_artifacts", "conversation_artifact_id"),
    ("idmm_action_reservations", "reservation_id"),
    ("idmm_interventions", "intervention_id"),
    ("preset_tags", "preset_tag_id"),
    ("presets", "preset_id"),
];

#[test]
fn v3_baseline_is_a_single_hard_cut_without_physical_relationship_tokens() {
    assert_eq!(
        BASELINE.matches("CREATE TABLE ").count(),
        64,
        "v3 baseline must define exactly 64 product tables"
    );
    assert_eq!(
        BASELINE
            .lines()
            .filter(|line| {
                !line.trim_start().starts_with("--")
                    && line.contains("INTEGER PRIMARY KEY AUTOINCREMENT")
            })
            .count(),
        64,
        "every v3 product table must use the local integer technical primary key"
    );

    let tokens = sql_tokens(&executable_baseline_sql());
    for forbidden in [["FOREIGN", "KEY"], ["ON", "DELETE"], ["ON", "UPDATE"]] {
        assert!(
            !tokens
                .windows(forbidden.len())
                .any(|window| window.iter().map(String::as_str).eq(forbidden)),
            "v3 baseline must not contain physical relationship tokens {forbidden:?}"
        );
    }
    for forbidden in ["REFERENCES", "CASCADE", "TRIGGER"] {
        assert!(
            !tokens.iter().any(|token| token == forbidden),
            "v3 baseline must not contain physical relationship token {forbidden}"
        );
    }
    assert!(
        !BASELINE.contains("_row_id"),
        "v3 baseline must not reintroduce dual-key row-id columns"
    );
}

#[tokio::test]
async fn initialized_database_satisfies_the_v3_id_schema_contract() {
    let database = init_database_memory().await.expect("database");
    validate_id_schema_contract(database.pool())
        .await
        .expect("clean v3 baseline uses integer row keys and named business IDs");
}

#[tokio::test]
async fn every_product_table_has_one_integer_autoincrement_row_primary_key() {
    let database = init_database_memory().await.expect("database");
    let pool = database.pool();
    let tables: Vec<String> = sqlx::query_scalar(
        "SELECT name FROM sqlite_schema \
         WHERE type = 'table' AND name NOT LIKE 'sqlite_%' AND name <> '_sqlx_migrations' \
         ORDER BY name",
    )
    .fetch_all(pool)
    .await
    .expect("tables");

    assert_eq!(tables.len(), 70);
    for table in tables {
        let columns = sqlx::query(&format!("PRAGMA table_info(\"{table}\")"))
            .fetch_all(pool)
            .await
            .expect("table info");
        let primary_keys: Vec<_> = columns.iter().filter(|row| row.get::<i64, _>("pk") > 0).collect();
        assert_eq!(primary_keys.len(), 1, "{table} must have one primary-key column");
        assert_eq!(primary_keys[0].get::<String, _>("name"), "id", "{table}");
        assert_eq!(
            primary_keys[0].get::<String, _>("type").to_ascii_uppercase(),
            "INTEGER",
            "{table}.id"
        );
        let create_sql: String = sqlx::query_scalar(
            "SELECT sql FROM sqlite_schema WHERE type = 'table' AND name = ?",
        )
        .bind(&table)
        .fetch_one(pool)
        .await
        .expect("table SQL");
        assert!(
            create_sql
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(" ")
                .to_ascii_lowercase()
                .contains("id integer primary key autoincrement"),
            "{table}.id must be declared AUTOINCREMENT, not merely inferred as INTEGER PRIMARY KEY"
        );
    }
}

#[tokio::test]
async fn conversation_artifacts_use_explicit_business_id_and_internal_technical_id() {
    let database = init_database_memory().await.expect("database");
    let columns = sqlx::query("PRAGMA table_info(\"conversation_artifacts\")")
        .fetch_all(database.pool())
        .await
        .expect("conversation_artifacts table info");
    let names: Vec<String> = columns
        .iter()
        .map(|row| row.get::<String, _>("name"))
        .collect();

    assert!(names.iter().any(|name| name == "id"));
    assert!(names
        .iter()
        .any(|name| name == "conversation_artifact_id"));
    assert!(
        !names.iter().any(|name| name == "artifact_id"),
        "conversation_artifacts must not expose generic artifact_id"
    );

    let technical_id = columns
        .iter()
        .find(|row| row.get::<String, _>("name") == "id")
        .expect("conversation_artifacts.id");
    assert_eq!(
        technical_id.get::<String, _>("type").to_ascii_uppercase(),
        "INTEGER"
    );
    assert_eq!(technical_id.get::<i64, _>("pk"), 1);
}

#[tokio::test]
async fn named_business_ids_are_text_and_logical_links_have_no_sqlite_foreign_keys() {
    let database = init_database_memory().await.expect("database");
    let pool = database.pool();

    for (table, column) in UNCONDITIONAL_UUIDV7_BUSINESS_IDS {
        let rows = sqlx::query(&format!("PRAGMA table_info(\"{table}\")"))
            .fetch_all(pool)
            .await
            .expect("table info");
        let row = rows
            .iter()
            .find(|row| row.get::<String, _>("name") == *column)
            .unwrap_or_else(|| panic!("missing {table}.{column}"));
        assert_eq!(
            row.get::<String, _>("type").to_ascii_uppercase(),
            "TEXT",
            "{table}.{column}"
        );
    }
    for table in [
        "users",
        "conversations",
        "agent_execution_events",
        "channel_sessions",
        "requirements",
        "preset_agent_preferences",
    ] {
        let foreign_keys = sqlx::query(&format!("PRAGMA foreign_key_list(\"{table}\")"))
            .fetch_all(pool)
            .await
            .expect("foreign-key list");
        assert!(foreign_keys.is_empty(), "{table} must not declare SQLite foreign keys");
    }
}

#[tokio::test]
async fn all_nontechnical_id_columns_are_text_and_only_id_is_a_technical_key() {
    let database = init_database_memory().await.expect("database");
    let pool = database.pool();
    let tables: Vec<String> = sqlx::query_scalar(
        "SELECT name FROM sqlite_schema \
         WHERE type = 'table' AND name NOT LIKE 'sqlite_%' AND name <> '_sqlx_migrations' \
         ORDER BY name",
    )
    .fetch_all(pool)
    .await
    .expect("tables");

    for table in tables {
        let columns = sqlx::query(&format!("PRAGMA table_info(\"{table}\")"))
            .fetch_all(pool)
            .await
            .expect("table info");
        for column in columns {
            let name: String = column.get("name");
            assert!(
                !name.ends_with("_row_id"),
                "{table}.{name} must not reintroduce a dual-key technical ID"
            );
            if name != "id" && name.ends_with("_id") {
                assert_eq!(
                    column.get::<String, _>("type").to_ascii_uppercase(),
                    "TEXT",
                    "{table}.{name} must be a logical/business ID, not an inter-table integer"
                );
                assert_eq!(
                    column.get::<i64, _>("pk"),
                    0,
                    "{table}.{name} must not be a second technical primary key"
                );
            }
        }
    }
}

#[tokio::test]
async fn runtime_v3_schema_has_no_physical_foreign_keys_or_cascades_and_only_guard_triggers() {
    let database = init_database_memory().await.expect("database");
    let pool = database.pool();
    let tables: Vec<String> = sqlx::query_scalar(
        "SELECT name FROM sqlite_schema \
         WHERE type = 'table' AND name NOT LIKE 'sqlite_%' AND name <> '_sqlx_migrations' \
         ORDER BY name",
    )
    .fetch_all(pool)
    .await
    .expect("tables");

    for table in &tables {
        let foreign_keys = sqlx::query(&format!("PRAGMA foreign_key_list(\"{table}\")"))
            .fetch_all(pool)
            .await
            .expect("foreign-key list");
        assert!(
            foreign_keys.is_empty(),
            "{table} must not declare a physical SQLite foreign key"
        );
    }

    let triggers: Vec<String> =
        sqlx::query_scalar(
            "SELECT name FROM sqlite_schema WHERE type = 'trigger' ORDER BY name",
        )
            .fetch_all(pool)
            .await
            .expect("triggers");
    assert_eq!(
        triggers,
        vec![
            "channel_inbound_receipts_identity_immutable",
            "channel_inbound_receipts_no_delete",
            "channel_inbound_receipts_scope_set_once",
            "channel_session_bindings_identity_immutable",
            "trg_conversation_delivery_receipts_identity_immutable",
            "trg_conversation_delivery_receipts_lifecycle_insert_guard",
            "trg_conversation_delivery_receipts_lifecycle_update_guard",
            "trg_conversation_delivery_receipts_no_delete",
            "trg_conversations_running_admission_guard",
            "trg_conversations_running_delete_guard",
            "trg_conversations_running_exit_guard",
            "trg_conversations_running_insert_guard",
            "trg_conversations_running_owner_immutable",
            "trg_requirements_absorb_done_cancelled",
            "trg_requirements_active_identity_exit_guard",
            "trg_requirements_active_to_pending_pre_effect_guard",
            "trg_requirements_in_progress_insert_guard",
            "trg_requirements_in_progress_update_guard",
            "trg_requirements_pending_insert_guard",
            "trg_requirements_pending_update_guard",
            "trg_requirements_pre_effect_abandon_guard_apply",
            "trg_requirements_pre_effect_abandon_guard_consume",
            "trg_requirements_pre_effect_abandon_guard_delete_guard",
            "trg_requirements_pre_effect_abandon_guard_immutable",
            "trg_requirements_pre_effect_abandon_guard_insert",
            "trg_terminal_turn_admissions_open_insert_guard",
            "trg_terminal_turn_admissions_open_update_guard",
        ],
        "v3 schema permits only registered guard triggers"
    );

    let schema_sql: Vec<String> = sqlx::query_scalar(
        "SELECT sql FROM sqlite_schema \
         WHERE sql IS NOT NULL AND type IN ('table', 'index') \
         ORDER BY name",
    )
    .fetch_all(pool)
    .await
    .expect("schema SQL");
    let tokens = sql_tokens(&schema_sql.join("\n"));
    for forbidden in [["FOREIGN", "KEY"], ["ON", "DELETE"], ["ON", "UPDATE"]] {
        assert!(
            !tokens
                .windows(forbidden.len())
                .any(|window| window.iter().map(String::as_str).eq(forbidden)),
            "v3 runtime schema must not contain physical relationship tokens {forbidden:?}"
        );
    }
    for forbidden in ["REFERENCES", "CASCADE", "TRIGGER"] {
        assert!(
            !tokens.iter().any(|token| token == forbidden),
            "v3 runtime schema must not contain physical relationship token {forbidden}"
        );
    }
}

#[tokio::test]
async fn every_registered_business_id_is_named_text_and_bare_uuidv7() {
    let database = init_database_memory().await.expect("database");
    let pool = database.pool();

    for (table, column) in UNCONDITIONAL_UUIDV7_BUSINESS_IDS {
        let row = sqlx::query(&format!("PRAGMA table_info(\"{table}\")"))
            .fetch_all(pool)
            .await
            .expect("table info")
            .into_iter()
            .find(|row| row.get::<String, _>("name") == *column)
            .unwrap_or_else(|| panic!("missing {table}.{column}"));
        assert_eq!(
            row.get::<String, _>("type").to_ascii_uppercase(),
            "TEXT",
            "{table}.{column} must be an explicitly named TEXT business ID"
        );
        assert_eq!(
            row.get::<i64, _>("pk"),
            0,
            "{table}.{column} must not be the technical primary key"
        );

        let values: Vec<String> =
            sqlx::query_scalar(&format!("SELECT {column} FROM {table} WHERE {column} IS NOT NULL"))
                .fetch_all(pool)
                .await
                .expect("business ID values");
        for value in values {
            validate_uuidv7(&value).unwrap_or_else(|error| {
                panic!("{table}.{column} stored non-canonical ID {value}: {error}")
            });
        }
    }
}

#[tokio::test]
async fn complete_business_id_registry_enforces_unconditional_uuidv7() {
    let database = init_database_memory().await.expect("database");
    let pool = database.pool();

    for (table, column) in UNCONDITIONAL_UUIDV7_BUSINESS_IDS {
        let create_sql: String = sqlx::query_scalar(
            "SELECT sql FROM sqlite_schema WHERE type = 'table' AND name = ?",
        )
        .bind(table)
        .fetch_one(pool)
        .await
        .expect("table SQL");
        let normalized = create_sql
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ")
            .to_ascii_lowercase();
        for fragment in [
            format!("length({column}) = 36"),
            format!("lower({column}) = {column}"),
            format!("{column} glob '????????-????-7???-[89ab]???-????????????'"),
            format!("replace({column}, '-', '') not glob '*[^0-9a-f]*'"),
        ] {
            assert!(
                normalized.contains(&fragment),
                "{table}.{column} is missing UUIDv7 CHECK fragment {fragment}"
            );
        }
    }

    let custom_agent_id = nomifun_common::AgentId::new();
    sqlx::query(
        "INSERT INTO agent_metadata \
         (agent_id, name, agent_type, agent_source, enabled, sort_order, created_at, updated_at) \
         VALUES (?, 'contract custom agent', 'custom', 'custom', 1, 1000, 1, 1)",
    )
    .bind(custom_agent_id.as_str())
    .execute(pool)
    .await
    .expect("custom agent UUIDv7");
    let invalid_agent = sqlx::query(
        "INSERT INTO agent_metadata \
         (agent_id, name, agent_type, agent_source, enabled, sort_order, created_at, updated_at) \
         VALUES ('agent_custom_invalid', 'invalid custom agent', 'custom', 'custom', 1, 1000, 1, 1)",
    )
    .execute(pool)
    .await;
    assert!(invalid_agent.is_err(), "custom agent IDs must be UUIDv7");

    let invalid_source_agent = sqlx::query(
        "INSERT INTO agent_execution_participants \
         (participant_id, execution_id, source_agent_id, introduced_in_revision, created_at) \
         VALUES (?, ?, 'agent_builtin_invalid', 0, 1)",
    )
    .bind(nomifun_common::generate_id())
    .bind(nomifun_common::generate_id())
    .execute(pool)
    .await;
    assert!(
        invalid_source_agent.is_err(),
        "execution participant source_agent_id must be UUIDv7"
    );

    let preset_id = nomifun_common::PresetId::new();
    sqlx::query(
        "INSERT INTO presets \
         (preset_id, source_kind, name, instructions, created_at, updated_at) \
         VALUES (?, 'user', 'contract user preset', '', 1, 1)",
    )
    .bind(preset_id.as_str())
    .execute(pool)
    .await
    .expect("user preset UUIDv7");
    let invalid_preset = sqlx::query(
        "INSERT INTO presets \
         (preset_id, source_kind, name, instructions, created_at, updated_at) \
         VALUES ('preset_user_invalid', 'user', 'invalid user preset', '', 1, 1)",
    )
    .execute(pool)
    .await;
    assert!(invalid_preset.is_err(), "user preset IDs must be UUIDv7");

    let builtin_agent_id = nomifun_common::AgentId::new();
    sqlx::query(
        "INSERT INTO agent_metadata \
         (agent_id, name, agent_type, agent_source, source_key, enabled, sort_order, created_at, updated_at) \
         VALUES (?, 'builtin fixture', 'custom', 'builtin', 'agent_builtin_fixture', 1, 1000, 1, 1)",
    )
    .bind(builtin_agent_id.as_str())
    .execute(pool)
    .await
    .expect("builtin catalog row uses a UUID business ID and source_key");

    let builtin_preset_id = nomifun_common::PresetId::new();
    sqlx::query(
        "INSERT INTO presets \
         (preset_id, source_kind, source_key, name, instructions, created_at, updated_at) \
         VALUES (?, 'builtin', 'preset_builtin_fixture', 'builtin fixture', '', 1, 1)",
    )
    .bind(builtin_preset_id.as_str())
    .execute(pool)
    .await
    .expect("builtin preset uses a UUID business ID and source_key");
    let stored: String = sqlx::query_scalar(
        "SELECT source_key FROM presets WHERE source_kind = 'builtin' AND preset_id = ?",
    )
    .bind(builtin_preset_id.as_str())
    .fetch_one(pool)
    .await
    .expect("builtin preset source key");
    assert_eq!(
        stored, "preset_builtin_fixture",
        "presets.preset_id must remain a UUID while catalog identity uses source_key"
    );
}

#[tokio::test]
async fn remaining_product_business_ids_reject_duplicates_and_non_uuid_values() {
    let database = init_database_memory().await.expect("database");
    let pool = database.pool();
    let owner: String = sqlx::query_scalar("SELECT user_id FROM users ORDER BY id LIMIT 1")
        .fetch_one(pool)
        .await
        .expect("installation owner");
    let provider_id = nomifun_common::generate_id();
    sqlx::query(
        "INSERT INTO providers \
         (provider_id, platform, name, base_url, api_key_encrypted, created_at, updated_at) \
         VALUES (?, 'contract', 'contract provider', 'https://example.invalid', '', 1, 1)",
    )
    .bind(&provider_id)
    .execute(pool)
    .await
    .expect("provider fixture");
    let conversation_id = ConversationId::new();
    sqlx::query(
        "INSERT INTO conversations \
         (conversation_id, user_id, name, type, created_at, updated_at) \
         VALUES (?, ?, 'business-id fixture', 'nomi', 1, 1)",
    )
    .bind(conversation_id.as_str())
    .bind(&owner)
    .execute(pool)
    .await
    .expect("conversation fixture");

    let cases = [
        (
            "mcp_servers",
            "mcp_server_id",
            "INSERT INTO mcp_servers \
             (mcp_server_id, name, transport_type, transport_config, created_at, updated_at) \
             VALUES (?, ?, 'stdio', '{}', 1, 1)",
        ),
        (
            "webhooks",
            "webhook_id",
            "INSERT INTO webhooks \
             (webhook_id, name, url, created_at, updated_at) \
             VALUES (?, ?, 'https://example.invalid/hook', 1, 1)",
        ),
        (
            "connector_credentials",
            "credential_id",
            "INSERT INTO connector_credentials \
             (credential_id, kind, name, payload_encrypted, created_at, updated_at) \
             VALUES (?, 'contract', ?, 'ciphertext', 1, 1)",
        ),
    ];
    for (table, column, statement) in cases {
        let id = nomifun_common::generate_id();
        sqlx::query(statement)
            .bind(&id)
            .bind(format!("{table}-valid"))
            .execute(pool)
            .await
            .unwrap_or_else(|error| panic!("valid {table}.{column} rejected: {error}"));
        assert!(
            sqlx::query(statement)
                .bind(&id)
                .bind(format!("{table}-duplicate"))
                .execute(pool)
                .await
                .is_err(),
            "duplicate {table}.{column} must be rejected"
        );
        assert!(
            sqlx::query(statement)
                .bind("not-a-uuid")
                .bind(format!("{table}-invalid"))
                .execute(pool)
                .await
                .is_err(),
            "invalid {table}.{column} must be rejected"
        );
    }

    let creation_task_id = nomifun_common::generate_id();
    sqlx::query(
        "INSERT INTO creation_tasks \
         (creation_task_id, provider_id, model, capability, params, status, submitted_at) \
         VALUES (?, ?, 'model', 'text', '{}', 'queued', 1)",
    )
    .bind(&creation_task_id)
    .bind(&provider_id)
    .execute(pool)
    .await
    .expect("creation task UUID");
    assert!(
        sqlx::query(
            "INSERT INTO creation_tasks \
             (creation_task_id, provider_id, model, capability, params, status, submitted_at) \
             VALUES (?, ?, 'duplicate', 'text', '{}', 'queued', 1)",
        )
        .bind(&creation_task_id)
        .bind(&provider_id)
        .execute(pool)
        .await
        .is_err()
    );
    assert!(
        sqlx::query(
            "INSERT INTO creation_tasks \
             (creation_task_id, provider_id, model, capability, params, status, submitted_at) \
             VALUES ('not-a-uuid', ?, 'model', 'text', '{}', 'queued', 1)",
        )
        .bind(&provider_id)
        .execute(pool)
        .await
        .is_err()
    );

    let conversation_artifact_id = nomifun_common::generate_id();
    sqlx::query(
        "INSERT INTO conversation_artifacts \
         (conversation_artifact_id, conversation_id, kind, payload, created_at, updated_at) \
         VALUES (?, ?, 'cron_trigger', '{}', 1, 1)",
    )
    .bind(&conversation_artifact_id)
    .bind(conversation_id.as_str())
    .execute(pool)
    .await
    .expect("conversation artifact UUID");
    assert!(
        sqlx::query(
            "INSERT INTO conversation_artifacts \
             (conversation_artifact_id, conversation_id, kind, payload, created_at, updated_at) \
             VALUES (?, ?, 'cron_trigger', '{}', 1, 1)",
        )
        .bind(&conversation_artifact_id)
        .bind(conversation_id.as_str())
        .execute(pool)
        .await
        .is_err()
    );
    assert!(
        sqlx::query(
            "INSERT INTO conversation_artifacts \
             (conversation_artifact_id, conversation_id, kind, payload, created_at, updated_at) \
             VALUES ('not-a-uuid', ?, 'cron_trigger', '{}', 1, 1)",
        )
        .bind(conversation_id.as_str())
        .execute(pool)
        .await
        .is_err()
    );

    let intervention_id = nomifun_common::generate_id();
    sqlx::query(
        "INSERT INTO idmm_interventions \
         (intervention_id, user_id, target_kind, target_id, watch, at, signal, \
          tier_used, action, outcome) \
         VALUES (?, ?, 'conversation', ?, 'fault', 1, 'stall', 'rule_only', \
                 'observe', 'recorded')",
    )
    .bind(&intervention_id)
    .bind(&owner)
    .bind(conversation_id.as_str())
    .execute(pool)
    .await
    .expect("IDMM intervention UUID");
    assert!(
        sqlx::query(
            "INSERT INTO idmm_interventions \
             (intervention_id, user_id, target_kind, target_id, watch, at, signal, \
              tier_used, action, outcome) \
             VALUES (?, ?, 'conversation', ?, 'fault', 1, 'stall', 'rule_only', \
                     'observe', 'recorded')",
        )
        .bind(&intervention_id)
        .bind(&owner)
        .bind(conversation_id.as_str())
        .execute(pool)
        .await
        .is_err()
    );
    assert!(
        sqlx::query(
            "INSERT INTO idmm_interventions \
             (intervention_id, user_id, target_kind, target_id, watch, at, signal, \
              tier_used, action, outcome) \
             VALUES ('not-a-uuid', ?, 'conversation', ?, 'fault', 1, 'stall', \
                     'rule_only', 'observe', 'recorded')",
        )
        .bind(&owner)
        .bind(conversation_id.as_str())
        .execute(pool)
        .await
        .is_err()
    );
}

#[tokio::test]
async fn preset_catalog_source_identity_is_unique_only_for_non_user_rows() {
    let database = init_database_memory().await.expect("database");
    let pool = database.pool();

    async fn insert_preset(
        pool: &sqlx::SqlitePool,
        source_kind: &str,
        source_key: Option<&str>,
        name: &str,
    ) -> Result<(), sqlx::Error> {
        let preset_id = nomifun_common::PresetId::new();
        sqlx::query(
            "INSERT INTO presets \
             (preset_id, source_kind, source_key, name, instructions, created_at, updated_at) \
             VALUES (?, ?, ?, ?, '', 1, 1)",
        )
        .bind(preset_id.as_str())
        .bind(source_kind)
        .bind(source_key)
        .bind(name)
        .execute(pool)
        .await?;
        Ok(())
    }

    insert_preset(pool, "builtin", Some("same-key"), "builtin one")
        .await
        .expect("first builtin");
    assert!(
        insert_preset(pool, "builtin", Some("same-key"), "builtin duplicate")
            .await
            .is_err()
    );
    insert_preset(pool, "extension", Some("same-key"), "extension one")
        .await
        .expect("source_kind participates in catalog identity");
    assert!(
        insert_preset(pool, "extension", Some("same-key"), "extension duplicate")
            .await
            .is_err()
    );
    insert_preset(pool, "user", Some("same-key"), "user one")
        .await
        .expect("user rows are outside catalog uniqueness");
    insert_preset(pool, "user", Some("same-key"), "user two")
        .await
        .expect("duplicate user source keys are allowed");
    assert!(
        insert_preset(pool, "builtin", None, "missing catalog key")
            .await
            .is_err()
    );
    assert!(
        insert_preset(pool, "extension", Some(""), "blank catalog key")
            .await
            .is_err()
    );
}

#[tokio::test]
async fn preset_catalog_partial_unique_index_has_exact_shape() {
    let database = init_database_memory().await.expect("database");
    let pool = database.pool();
    let row = sqlx::query(
        "SELECT il.\"unique\", il.partial \
         FROM pragma_index_list('presets') il \
         WHERE il.name = 'uq_presets_catalog_source_key'",
    )
    .fetch_one(pool)
    .await
    .expect("catalog partial unique index");
    assert_eq!(row.get::<i64, _>("unique"), 1);
    assert_eq!(row.get::<i64, _>("partial"), 1);

    let columns: Vec<String> = sqlx::query_scalar(
        "SELECT name FROM pragma_index_info('uq_presets_catalog_source_key') ORDER BY seqno",
    )
    .fetch_all(pool)
    .await
    .expect("catalog index columns");
    assert_eq!(columns, ["source_kind", "source_key"]);

    let sql: String = sqlx::query_scalar(
        "SELECT sql FROM sqlite_schema \
         WHERE type = 'index' AND name = 'uq_presets_catalog_source_key'",
    )
    .fetch_one(pool)
    .await
    .expect("catalog index SQL");
    assert_eq!(
        sql.split_whitespace().collect::<Vec<_>>().join(" "),
        "CREATE UNIQUE INDEX uq_presets_catalog_source_key ON presets(source_kind, source_key) \
         WHERE source_kind IN ('builtin', 'extension')"
    );
}

#[tokio::test]
async fn remaining_uuid_logical_links_and_json_registry_enforce_text_values() {
    let database = init_database_memory().await.expect("database");
    let pool = database.pool();
    let owner: String = sqlx::query_scalar("SELECT user_id FROM users ORDER BY id LIMIT 1")
        .fetch_one(pool)
        .await
        .expect("installation owner");
    let conversation_id = ConversationId::new();
    sqlx::query(
        "INSERT INTO conversations \
         (conversation_id, user_id, name, type, created_at, updated_at) \
         VALUES (?, ?, 'logical-link fixture', 'nomi', 1, 1)",
    )
    .bind(conversation_id.as_str())
    .bind(&owner)
    .execute(pool)
    .await
    .expect("conversation");

    let mcp_server_id = nomifun_common::generate_id();
    sqlx::query(
        "INSERT INTO mcp_servers \
         (mcp_server_id, name, transport_type, transport_config, created_at, updated_at) \
         VALUES (?, 'logical-link-mcp', 'stdio', '{}', 1, 1)",
    )
    .bind(&mcp_server_id)
    .execute(pool)
    .await
    .expect("MCP server");
    sqlx::query(
        "INSERT INTO conversation_mcp_servers \
         (conversation_id, mcp_server_id) VALUES (?, ?)",
    )
    .bind(conversation_id.as_str())
    .bind(&mcp_server_id)
    .execute(pool)
    .await
    .expect("UUID MCP link");
    let mcp_type: String = sqlx::query_scalar(
        "SELECT typeof(mcp_server_id) FROM conversation_mcp_servers LIMIT 1",
    )
    .fetch_one(pool)
    .await
    .expect("MCP link type");
    assert_eq!(mcp_type, "text");
    assert!(
        sqlx::query(
            "INSERT INTO conversation_mcp_servers \
             (conversation_id, mcp_server_id) VALUES (?, '1')",
        )
        .bind(conversation_id.as_str())
        .execute(pool)
        .await
        .is_err()
    );

    let webhook_id = nomifun_common::generate_id();
    sqlx::query(
        "INSERT INTO webhooks \
         (webhook_id, name, url, created_at, updated_at) \
         VALUES (?, 'logical-link webhook', 'https://example.invalid/hook', 1, 1)",
    )
    .bind(&webhook_id)
    .execute(pool)
    .await
    .expect("webhook");
    sqlx::query(
        "INSERT INTO tag_settings (tag, webhook_id, updated_at) VALUES ('contract', ?, 1)",
    )
    .bind(&webhook_id)
    .execute(pool)
    .await
    .expect("UUID webhook link");
    assert!(
        sqlx::query(
            "INSERT INTO tag_settings (tag, webhook_id, updated_at) \
             VALUES ('invalid-contract', '1', 1)",
        )
        .execute(pool)
        .await
        .is_err()
    );

    let provider_id = nomifun_common::generate_id();
    sqlx::query(
        "INSERT INTO providers \
         (provider_id, platform, name, base_url, api_key_encrypted, created_at, updated_at) \
         VALUES (?, 'contract', 'link provider', 'https://example.invalid', '', 1, 1)",
    )
    .bind(&provider_id)
    .execute(pool)
    .await
    .expect("provider");
    let creation_task_id = nomifun_common::generate_id();
    sqlx::query(
        "INSERT INTO creation_tasks \
         (creation_task_id, provider_id, model, capability, params, status, submitted_at) \
         VALUES (?, ?, 'model', 'text', '{}', 'queued', 1)",
    )
    .bind(&creation_task_id)
    .bind(&provider_id)
    .execute(pool)
    .await
    .expect("creation task");
    sqlx::query(
        "INSERT INTO workshop_assets \
         (asset_id, kind, title, origin, created_at, updated_at) \
         VALUES (?, 'image', 'task origin', ?, 1, 1)",
    )
    .bind(nomifun_common::generate_id())
    .bind(serde_json::json!({ "creation_task_id": creation_task_id.clone() }).to_string())
    .execute(pool)
    .await
    .expect("UUID task origin");

    let credential_id = nomifun_common::generate_id();
    sqlx::query(
        "INSERT INTO connector_credentials \
         (credential_id, kind, name, payload_encrypted, created_at, updated_at) \
         VALUES (?, 'contract', 'knowledge credential', 'ciphertext', 1, 1)",
    )
    .bind(&credential_id)
    .execute(pool)
    .await
    .expect("credential");
    sqlx::query(
        "INSERT INTO knowledge_bases \
         (knowledge_base_id, name, root_path, extra, created_at, updated_at) \
         VALUES (?, 'credential knowledge', '/tmp/credential-knowledge', ?, 1, 1)",
    )
    .bind(nomifun_common::generate_id())
    .bind(
        serde_json::json!({
            "source": {
                "kind": "contract",
                "credentialRef": credential_id
            }
        })
        .to_string(),
    )
    .execute(pool)
    .await
    .expect("UUID credential reference");

    let task_value_type: String = sqlx::query_scalar(
        "SELECT typeof(json_extract(origin, '$.creation_task_id')) \
         FROM workshop_assets WHERE json_extract(origin, '$.creation_task_id') = ?",
    )
    .bind(&creation_task_id)
    .fetch_one(pool)
    .await
    .expect("workshop task JSON value");
    assert_eq!(task_value_type, "text");
    assert!(
        sqlx::query_scalar::<_, Option<String>>(
            "SELECT json_extract(origin, '$.task_id') \
             FROM workshop_assets WHERE json_extract(origin, '$.creation_task_id') = ?",
        )
        .bind(&creation_task_id)
        .fetch_one(pool)
        .await
        .expect("retired workshop task JSON path")
        .is_none()
    );
    for (label, invalid_origin) in [
        ("legacy integer", serde_json::json!({"task_id": 1})),
        ("legacy numeric string", serde_json::json!({"task_id": "1"})),
        (
            "legacy UUIDv7 field",
            serde_json::json!({"task_id": nomifun_common::generate_id()}),
        ),
        (
            "integer creation_task_id",
            serde_json::json!({"creation_task_id": 1}),
        ),
        (
            "numeric-string creation_task_id",
            serde_json::json!({"creation_task_id": "1"}),
        ),
        (
            "prefixed creation_task_id",
            serde_json::json!({
                "creation_task_id": format!("task_{}", nomifun_common::generate_id())
            }),
        ),
        (
            "UUIDv4 creation_task_id",
            serde_json::json!({
                "creation_task_id": "550e8400-e29b-41d4-a716-446655440000"
            }),
        ),
        (
            "uppercase creation_task_id",
            serde_json::json!({
                "creation_task_id": nomifun_common::generate_id().to_ascii_uppercase()
            }),
        ),
    ] {
        assert!(
            sqlx::query(
                "INSERT INTO workshop_assets \
                 (asset_id, kind, title, origin, created_at, updated_at) \
                 VALUES (?, 'image', ?, ?, 1, 1)",
            )
            .bind(nomifun_common::generate_id())
            .bind(label)
            .bind(invalid_origin.to_string())
            .execute(pool)
            .await
            .is_err(),
            "{label} must be rejected"
        );
    }
    let credential_value_type: String = sqlx::query_scalar(
        "SELECT typeof(json_extract(extra, '$.source.credentialRef')) \
         FROM knowledge_bases WHERE json_extract(extra, '$.source.credentialRef') = ?",
    )
    .bind(&credential_id)
    .fetch_one(pool)
    .await
    .expect("knowledge credential JSON value");
    assert_eq!(credential_value_type, "text");
}

#[tokio::test]
async fn contract_rejects_missing_unconditional_uuidv7_checks() {
    for (table, column) in [
        ("agent_metadata", "agent_id"),
        ("presets", "preset_id"),
    ] {
        let database = init_database_memory().await.expect("database");
        let pool = database.pool();
        let original: String = sqlx::query_scalar(
            "SELECT sql FROM sqlite_schema WHERE type = 'table' AND name = ?",
        )
        .bind(table)
        .fetch_one(pool)
        .await
        .expect("table SQL");
        let tampered = original.replace(
            &format!("length({column}) = 36"),
            &format!("length({column}) = 35"),
        );
        assert_ne!(tampered, original, "fixture must alter {table} CHECK predicate");

        sqlx::query("PRAGMA writable_schema = ON")
            .execute(pool)
            .await
            .expect("enable writable_schema");
        sqlx::query("UPDATE sqlite_schema SET sql = ? WHERE type = 'table' AND name = ?")
            .bind(tampered)
            .bind(table)
            .execute(pool)
            .await
            .expect("tamper table SQL");
        sqlx::query("PRAGMA writable_schema = OFF")
            .execute(pool)
            .await
            .expect("disable writable_schema");

        let error = validate_id_schema_contract(pool)
            .await
            .expect_err("missing unconditional CHECK predicate must fail");
        let message = error.to_string();
        assert!(
            message.contains("business ID") && message.contains(column),
            "unexpected contract error for {table}.{column}: {message}"
        );
    }
}

#[tokio::test]
async fn contract_rejects_legacy_workshop_asset_origin_task_path() {
    let database = init_database_memory().await.expect("database");
    let pool = database.pool();
    let original: String = sqlx::query_scalar(
        "SELECT sql FROM sqlite_schema WHERE type = 'table' AND name = 'workshop_assets'",
    )
    .fetch_one(pool)
    .await
    .expect("workshop_assets table SQL");
    let tampered = original.replace(
        "json_type(origin, '$.task_id') IS NULL",
        "json_type(origin, '$.task_id') <> 'forbidden'",
    );
    assert_ne!(
        tampered, original,
        "fixture must remove the retired task_id rejection"
    );

    sqlx::query("PRAGMA writable_schema = ON")
        .execute(pool)
        .await
        .expect("enable writable_schema");
    sqlx::query(
        "UPDATE sqlite_schema SET sql = ? WHERE type = 'table' AND name = 'workshop_assets'",
    )
    .bind(tampered)
    .execute(pool)
    .await
    .expect("tamper workshop_assets table SQL");
    sqlx::query("PRAGMA writable_schema = OFF")
        .execute(pool)
        .await
        .expect("disable writable_schema");

    let error = validate_id_schema_contract(pool)
        .await
        .expect_err("legacy origin.task_id compatibility must invalidate the v3 schema");
    assert!(
        error.to_string().contains("workshop_assets.origin")
            && error.to_string().contains("TASK_ID"),
        "unexpected contract error: {error}"
    );
}

#[tokio::test]
async fn external_owner_columns_enforce_uuidv7_without_requiring_local_parents() {
    let database = init_database_memory().await.expect("database");
    let pool = database.pool();
    let plugin_id = nomifun_common::generate_id();
    let companion_id = nomifun_common::generate_id();
    let public_agent_id = nomifun_common::generate_id();
    let binding_companion_id = nomifun_common::generate_id();
    let knowledge_binding_id = nomifun_common::KnowledgeBindingId::new();
    let token_companion_id = nomifun_common::generate_id();

    sqlx::query(
        "INSERT INTO channel_plugins \
         (channel_plugin_id, type, name, enabled, config, companion_id, created_at, updated_at) \
         VALUES (?, 'fixture', 'companion fixture', 0, '{}', ?, 1, 1)",
    )
    .bind(&plugin_id)
    .bind(&companion_id)
    .execute(pool)
    .await
    .expect("external companion ID does not require a local parent");
    sqlx::query(
        "UPDATE channel_plugins \
         SET companion_id = NULL, public_agent_id = ? WHERE channel_plugin_id = ?",
    )
    .bind(&public_agent_id)
    .bind(&plugin_id)
    .execute(pool)
    .await
    .expect("external public-agent ID does not require a local parent");
    sqlx::query(
        "INSERT INTO knowledge_bindings \
         (knowledge_binding_id, target_kind, target_companion_id, updated_at) \
         VALUES (?, 'companion', ?, 1)",
    )
    .bind(knowledge_binding_id.as_str())
    .bind(&binding_companion_id)
    .execute(pool)
    .await
    .expect("external knowledge companion ID does not require a local parent");
    sqlx::query(
        "INSERT INTO companion_access_token (companion_id, token_hash, created_at) \
         VALUES (?, 'hash', 1)",
    )
    .bind(&token_companion_id)
    .execute(pool)
    .await
    .expect("external companion token ID does not require a local parent");

    for statement in [
        "UPDATE channel_plugins SET public_agent_id = 'public_agent_bad' WHERE channel_plugin_id = ?",
        "UPDATE channel_plugins SET companion_id = 'companion_bad', public_agent_id = NULL WHERE channel_plugin_id = ?",
    ] {
        assert!(
            sqlx::query(statement)
                .bind(&plugin_id)
                .execute(pool)
                .await
                .is_err(),
            "channel external IDs must remain bare UUIDv7"
        );
    }
    assert!(
        sqlx::query(
            "INSERT INTO knowledge_bindings \
             (knowledge_binding_id, target_kind, target_companion_id, updated_at) \
             VALUES ('0190f5fe-7c00-7a00-8000-000000000209', \
                     'companion', 'companion_bad', 1)",
        )
        .execute(pool)
        .await
        .is_err(),
        "knowledge companion IDs must remain bare UUIDv7"
    );
    assert!(
        sqlx::query(
            "INSERT INTO companion_access_token (companion_id, token_hash, created_at) \
             VALUES ('companion_bad', 'hash', 1)",
        )
        .execute(pool)
        .await
        .is_err(),
        "companion token IDs must remain bare UUIDv7"
    );
}

#[tokio::test]
async fn scalar_external_business_ids_require_uuidv7_without_parent_existence() {
    let database = init_database_memory().await.expect("database");
    let pool = database.pool();
    let companion_id = nomifun_common::CompanionId::new();
    let public_agent_id = nomifun_common::PublicAgentId::new();
    let knowledge_binding_id = nomifun_common::KnowledgeBindingId::new();
    let plugin_a = nomifun_common::ChannelPluginId::new();
    let plugin_b = nomifun_common::ChannelPluginId::new();

    sqlx::query(
        "INSERT INTO channel_plugins \
         (channel_plugin_id, type, name, enabled, config, companion_id, created_at, updated_at) \
         VALUES (?, 'test-companion', 'external companion', 1, '{}', ?, 1, 1)",
    )
    .bind(plugin_a.as_str())
    .bind(companion_id.as_str())
    .execute(pool)
    .await
    .expect("external companion need not have a SQLite parent");
    sqlx::query(
        "INSERT INTO channel_plugins \
         (channel_plugin_id, type, name, enabled, config, public_agent_id, created_at, updated_at) \
         VALUES (?, 'test-agent', 'external public agent', 1, '{}', ?, 1, 1)",
    )
    .bind(plugin_b.as_str())
    .bind(public_agent_id.as_str())
    .execute(pool)
    .await
    .expect("external public agent need not have a SQLite parent");
    sqlx::query(
        "INSERT INTO companion_access_token (companion_id, token_hash, created_at) \
         VALUES (?, 'hash', 1)",
    )
    .bind(companion_id.as_str())
    .execute(pool)
    .await
    .expect("external token owner need not have a SQLite parent");
    sqlx::query(
        "INSERT INTO knowledge_bindings \
         (knowledge_binding_id, target_kind, target_companion_id, enabled, writeback, writeback_mode, \
          writeback_eagerness, updated_at, channel_write_enabled) \
         VALUES (?, 'companion', ?, 1, 0, 'staged', 'conservative', 1, 0)",
    )
    .bind(knowledge_binding_id.as_str())
    .bind(companion_id.as_str())
    .execute(pool)
    .await
    .expect("external knowledge target need not have a SQLite parent");

    for statement in [
        "INSERT INTO channel_plugins \
         (channel_plugin_id, type, name, enabled, config, companion_id, created_at, updated_at) \
         VALUES ('0190f5fe-7c00-7a00-8000-000000000201', 'invalid-companion', \
                 'invalid companion', 1, '{}', '1', 1, 1)",
        "INSERT INTO channel_plugins \
         (channel_plugin_id, type, name, enabled, config, public_agent_id, created_at, updated_at) \
         VALUES ('0190f5fe-7c00-7a00-8000-000000000202', 'invalid-agent', \
                 'invalid agent', 1, '{}', '1', 1, 1)",
        "INSERT INTO companion_access_token (companion_id, token_hash, created_at) \
         VALUES ('1', 'invalid', 1)",
        "INSERT INTO knowledge_bindings \
         (knowledge_binding_id, target_kind, target_companion_id, enabled, writeback, writeback_mode, \
          writeback_eagerness, updated_at, channel_write_enabled) \
         VALUES ('0190f5fe-7c00-7a00-8000-000000000210', \
                 'companion', '1', 1, 0, 'staged', 'conservative', 1, 0)",
    ] {
        assert!(
            sqlx::query(statement).execute(pool).await.is_err(),
            "database CHECK must reject non-UUIDv7 external business IDs"
        );
    }
}

#[tokio::test]
async fn runtime_contract_rejects_triggers_and_row_id_columns() {
    let trigger_database = init_database_memory().await.expect("database");
    sqlx::query(
        "CREATE TRIGGER forbidden_trigger AFTER INSERT ON users BEGIN SELECT 1; END",
    )
    .execute(trigger_database.pool())
    .await
    .expect("create trigger fixture");
    let trigger_error = validate_id_schema_contract(trigger_database.pool())
        .await
        .expect_err("runtime contract must reject triggers");
    assert!(trigger_error.to_string().contains("triggers"));

    let row_id_database = init_database_memory().await.expect("database");
    let pool = row_id_database.pool();
    sqlx::query("ALTER TABLE webhooks ADD COLUMN legacy_row_id INTEGER")
        .execute(pool)
        .await
        .expect("add row-id fixture column");
    let row_id_error = validate_id_schema_contract(pool)
        .await
        .expect_err("runtime contract must reject *_row_id columns");
    assert!(row_id_error.to_string().contains("legacy_row_id"));
}

#[tokio::test]
async fn external_agent_actor_uses_its_own_partial_index_branch() {
    let database = init_database_memory().await.expect("database");
    let pool = database.pool();

    let external_sql: String = sqlx::query_scalar(
        "SELECT sql FROM sqlite_schema \
         WHERE type = 'index' AND name = 'idx_execution_events_actor_external_agent_id'",
    )
    .fetch_one(pool)
    .await
    .expect("external-agent actor index");
    let external_sql = external_sql.split_whitespace().collect::<Vec<_>>().join(" ");
    assert!(external_sql.contains("ON agent_execution_events(actor_id)"));
    assert!(external_sql.contains(
        "WHERE actor_type = 'agent' AND actor_conversation_id IS NULL AND actor_id IS NOT NULL"
    ));

    let local_sql: String = sqlx::query_scalar(
        "SELECT sql FROM sqlite_schema \
         WHERE type = 'index' AND name = 'idx_execution_events_actor_local_agent_id'",
    )
    .fetch_one(pool)
    .await
    .expect("local-agent actor index");
    let local_sql = local_sql.split_whitespace().collect::<Vec<_>>().join(" ");
    assert!(local_sql.contains(
        "WHERE actor_type = 'agent' AND actor_conversation_id IS NOT NULL AND actor_id IS NOT NULL"
    ));
    assert_ne!(external_sql, local_sql);
}

#[tokio::test]
async fn installation_owner_uses_named_user_id_and_auto_allocated_id() {
    let database = init_database_memory().await.expect("database");
    let pool = database.pool();
    let owner: String = sqlx::query_scalar(
        "SELECT owner_user_id FROM installation_identity \
         WHERE singleton_key = 'installation'",
    )
    .fetch_one(pool)
    .await
    .expect("installation owner");
    validate_uuidv7(&owner).expect("canonical owner UUIDv7");

    let row = sqlx::query("SELECT id, user_id, username FROM users WHERE user_id = ?")
        .bind(&owner)
        .fetch_one(pool)
        .await
        .expect("owner row");
    assert!(row.get::<i64, _>("id") > 0);
    assert_eq!(row.get::<String, _>("user_id"), owner);
    assert_eq!(row.get::<String, _>("username"), "admin");
}

#[tokio::test]
async fn preset_tags_separate_local_ids_business_ids_and_catalog_keys() {
    let database = init_database_memory().await.expect("database");
    let pool = database.pool();
    let columns = sqlx::query("PRAGMA table_info(\"preset_tags\")")
        .fetch_all(pool)
        .await
        .expect("preset_tags columns");
    let id = columns
        .iter()
        .find(|row| row.get::<String, _>("name") == "id")
        .expect("preset_tags.id");
    assert_eq!(id.get::<String, _>("type").to_ascii_uppercase(), "INTEGER");
    assert_eq!(id.get::<i64, _>("pk"), 1);

    let preset_tag_id = columns
        .iter()
        .find(|row| row.get::<String, _>("name") == "preset_tag_id")
        .expect("preset_tags.preset_tag_id");
    assert_eq!(
        preset_tag_id.get::<String, _>("type").to_ascii_uppercase(),
        "TEXT"
    );
    assert_eq!(preset_tag_id.get::<i64, _>("notnull"), 1);

    let preset_tag_indexes = sqlx::query("PRAGMA index_list(\"preset_tags\")")
        .fetch_all(pool)
        .await
        .expect("preset_tags indexes");
    assert!(
        preset_tag_indexes
            .iter()
            .filter(|row| row.get::<i64, _>("unique") == 1)
            .count()
            >= 2,
        "preset_tags.preset_tag_id and preset_tags.key must both be unique"
    );

    let binding_columns = sqlx::query("PRAGMA table_info(\"preset_tag_bindings\")")
        .fetch_all(pool)
        .await
        .expect("preset_tag_bindings columns");
    assert!(binding_columns
        .iter()
        .any(|row| row.get::<String, _>("name") == "preset_tag_id"));
    assert!(!binding_columns
        .iter()
        .any(|row| row.get::<String, _>("name") == "tag_key"));
}

#[tokio::test]
async fn idmm_logical_targets_store_named_bare_uuidv7_ids() {
    let database = init_database_memory().await.expect("database");
    let pool = database.pool();
    let owner: String = sqlx::query_scalar("SELECT user_id FROM users ORDER BY id LIMIT 1")
        .fetch_one(pool)
        .await
        .expect("owner");
    let conversation_id = ConversationId::new();
    let terminal_id = TerminalId::new();

    sqlx::query(
        "INSERT INTO conversations \
         (conversation_id, user_id, name, type, created_at, updated_at) \
         VALUES (?, ?, 'contract conversation', 'nomi', 1, 1)",
    )
    .bind(conversation_id.as_str())
    .bind(&owner)
    .execute(pool)
    .await
    .expect("conversation");
    sqlx::query(
        "INSERT INTO terminal_sessions \
         (terminal_id, name, cwd, command, created_at, updated_at, user_id) \
         VALUES (?, 'contract terminal', '.', 'shell', 1, 1, ?)",
    )
    .bind(terminal_id.as_str())
    .bind(&owner)
    .execute(pool)
    .await
    .expect("terminal");

    sqlx::query(
        "INSERT INTO idmm_interventions \
         (intervention_id, user_id, target_kind, target_id, watch, at, signal, \
          tier_used, action, outcome) \
         VALUES (?, ?, 'conversation', ?, 'fault', 1, 'stall', 'rule_only', \
                 'observe', 'recorded')",
    )
    .bind(nomifun_common::generate_id())
    .bind(&owner)
    .bind(conversation_id.as_str())
    .execute(pool)
    .await
    .expect("idmm record");

    let stored: String =
        sqlx::query_scalar("SELECT target_id FROM idmm_interventions ORDER BY id DESC LIMIT 1")
            .fetch_one(pool)
            .await
            .expect("stored target");
    assert_eq!(stored, conversation_id.as_str());
    validate_uuidv7(&stored).expect("IDMM target must remain a canonical bare UUIDv7");
}

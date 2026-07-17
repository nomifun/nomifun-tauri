use nomifun_common::{ConversationId, McpServerId, RemoteAgentId, TerminalId, WebhookId};
use nomifun_db::{init_database_memory, installation_owner_id, validate_id_schema_contract};
use sqlx::Row;

const BASELINE: &str = include_str!("../migrations/001_id_contract_v2.sql");

#[test]
fn clean_baseline_contains_no_retired_session_id_coercions() {
    for retired_contract in [
        "actor_conversation_id > 0",
        "CAST(actor_conversation_id AS TEXT)",
        "CAST(conversation.id AS TEXT)",
        "CAST(terminal.id AS TEXT)",
        "conversation_id INTEGER",
        "terminal_id INTEGER",
        "target_conv_id INTEGER",
        "target_term_id INTEGER",
    ] {
        assert!(
            !BASELINE.contains(retired_contract),
            "retired numeric ID contract survived in clean baseline: {retired_contract}"
        );
    }
}

#[test]
fn clean_baseline_contains_no_entity_autoincrement_allocator() {
    assert!(
        !BASELINE.to_ascii_uppercase().contains("AUTOINCREMENT"),
        "business entities must never use SQLite AUTOINCREMENT"
    );
}

#[tokio::test]
async fn initialized_database_satisfies_the_id_schema_contract() {
    let database = init_database_memory().await.expect("database");
    validate_id_schema_contract(database.pool())
        .await
        .expect("clean baseline uses TEXT entity keys");

}

#[tokio::test]
async fn idmm_owner_guard_compares_canonical_session_ids_without_casting() {
    let database = init_database_memory().await.expect("database");
    let pool = database.pool();
    let owner = installation_owner_id(pool).await.expect("installation owner");
    let conversation_id = ConversationId::new();
    let terminal_id = TerminalId::new();

    sqlx::query(
        "INSERT INTO conversations \
         (id, user_id, name, type, extra, status, created_at, updated_at) \
         VALUES (?, ?, 'contract conversation', 'nomi', '{}', \
                 'pending', 1, 1)",
    )
    .bind(conversation_id.as_str())
    .bind(&owner)
    .execute(pool)
    .await
    .expect("conversation");
    sqlx::query(
        "INSERT INTO terminal_sessions \
         (id, name, cwd, command, args, cols, rows, created_at, updated_at, \
          last_status, user_id) \
         VALUES (?, 'contract terminal', '.', 'shell', '[]', 80, 24, 1, 1, \
                 'running', ?)",
    )
    .bind(terminal_id.as_str())
    .bind(&owner)
    .execute(pool)
    .await
    .expect("terminal");

    insert_idmm_record(pool, "idmmrec_0190f5fe-7c00-7a00-8000-000000000001", "conversation", conversation_id.as_str())
        .await
        .expect("canonical conversation target");
    insert_idmm_record(pool, "idmmrec_0190f5fe-7c00-7a00-8000-000000000002", "terminal", terminal_id.as_str())
        .await
        .expect("canonical terminal target");

    let error = insert_idmm_record(
        pool,
        "idmmrec_0190f5fe-7c00-7a00-8000-000000000003",
        "conversation",
        terminal_id.as_str(),
    )
    .await
    .expect_err("wrong entity domain must be rejected");
    assert!(
        error
            .to_string()
            .contains("IDMM conversation target owner mismatch")
    );

    let stored: Vec<String> =
        sqlx::query("SELECT target_id FROM idmm_interventions ORDER BY id")
            .fetch_all(pool)
            .await
            .expect("stored targets")
            .into_iter()
            .map(|row| row.get("target_id"))
            .collect();
    assert_eq!(
        stored,
        vec![
            conversation_id.into_string(),
            terminal_id.into_string(),
        ]
    );
}

#[tokio::test]
async fn mcp_remote_agent_and_webhook_keys_and_webhook_fk_are_text() {
    let database = init_database_memory().await.expect("database");
    let pool = database.pool();

    for (table, column) in [
        ("mcp_servers", "id"),
        ("remote_agents", "id"),
        ("webhooks", "id"),
        ("tag_settings", "webhook_id"),
        ("conversation_mcp_servers", "mcp_server_id"),
    ] {
        let rows = sqlx::query(&format!("PRAGMA table_info('{table}')"))
            .fetch_all(pool)
            .await
            .expect("table info");
        let declared_type = rows
            .iter()
            .find(|row| row.get::<String, _>("name") == column)
            .map(|row| row.get::<String, _>("type"))
            .expect("column");
        assert_eq!(declared_type.to_ascii_uppercase(), "TEXT", "{table}.{column}");
    }

    let webhook_fk = sqlx::query("PRAGMA foreign_key_list('tag_settings')")
        .fetch_all(pool)
        .await
        .expect("tag_settings foreign keys")
        .into_iter()
        .any(|row| {
            row.get::<String, _>("table") == "webhooks"
                && row.get::<String, _>("from") == "webhook_id"
                && row.get::<String, _>("to") == "id"
        });
    assert!(webhook_fk, "tag_settings.webhook_id must reference webhooks.id");

    let mcp_id = McpServerId::new();
    let remote_id = RemoteAgentId::new();
    let webhook_id = WebhookId::new();
    for id in [mcp_id.as_str(), remote_id.as_str(), webhook_id.as_str()] {
        assert!(
            !id.bytes().all(|byte| byte.is_ascii_digit()),
            "entity IDs must not regress to numeric keys"
        );
    }
}

#[tokio::test]
async fn durable_entity_keys_and_references_use_text_storage() {
    let database = init_database_memory().await.expect("database");
    let pool = database.pool();

    for (table, column) in [
        ("users", "id"),
        ("providers", "id"),
        ("agent_metadata", "id"),
        ("presets", "id"),
        ("preset_tags", "key"),
        ("messages", "id"),
        ("cron_jobs", "id"),
        ("cron_job_runs", "id"),
        ("agent_execution_templates", "id"),
        ("agent_execution_template_participants", "id"),
        ("agent_executions", "id"),
        ("agent_execution_participants", "id"),
        ("agent_execution_steps", "id"),
        ("agent_execution_attempts", "id"),
        ("agent_execution_events", "id"),
        ("channel_plugins", "id"),
        ("channel_users", "id"),
        ("channel_sessions", "id"),
        ("workshop_canvases", "id"),
        ("workshop_assets", "id"),
        ("creation_tasks", "id"),
        ("attachments", "id"),
        ("idmm_interventions", "id"),
        ("messages", "conversation_id"),
        ("cron_jobs", "conversation_id"),
        ("cron_job_runs", "job_id"),
        ("requirements", "owner_conversation_id"),
        ("requirements", "owner_terminal_id"),
        ("agent_execution_events", "actor_conversation_id"),
        ("conversation_execution_links", "execution_id"),
        ("creation_tasks", "provider_id"),
    ] {
        let rows = sqlx::query(&format!("PRAGMA table_info('{table}')"))
            .fetch_all(pool)
            .await
            .expect("table info");
        let declared_type = rows
            .iter()
            .find(|row| row.get::<String, _>("name") == column)
            .map(|row| row.get::<String, _>("type"))
            .unwrap_or_else(|| panic!("missing ID contract column {table}.{column}"));
        assert_eq!(declared_type.to_ascii_uppercase(), "TEXT", "{table}.{column}");
    }
}

#[tokio::test]
async fn only_the_system_settings_singleton_has_an_integer_primary_key() {
    let database = init_database_memory().await.expect("database");
    let pool = database.pool();
    let tables: Vec<String> = sqlx::query_scalar(
        "SELECT name FROM sqlite_schema \
         WHERE type = 'table' AND name NOT LIKE 'sqlite_%' ORDER BY name",
    )
    .fetch_all(pool)
    .await
    .expect("tables");
    let mut integer_primary_keys = Vec::new();
    for table in tables {
        let rows = sqlx::query(&format!("PRAGMA table_info('{table}')"))
            .fetch_all(pool)
            .await
            .expect("table info");
        let primary_key_rows: Vec<_> =
            rows.iter().filter(|row| row.get::<i64, _>("pk") > 0).collect();
        if primary_key_rows.len() == 1
            && primary_key_rows[0]
                .get::<String, _>("type")
                .eq_ignore_ascii_case("INTEGER")
        {
            integer_primary_keys.push((
                table.clone(),
                primary_key_rows[0].get::<String, _>("name"),
            ));
        }
    }
    assert_eq!(
        integer_primary_keys,
        vec![("system_settings".to_owned(), "id".to_owned())],
        "numeric primary keys are reserved for the explicit singleton discriminator"
    );
}

async fn insert_idmm_record(
    pool: &sqlx::SqlitePool,
    id: &str,
    target_kind: &str,
    target_id: &str,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO idmm_interventions \
         (id, user_id, target_kind, target_id, watch, at, signal, tier_used, \
          action, outcome) \
         VALUES (?, ?, ?, ?, 'fault', 1, 'stall', \
                 'rule_only', 'observe', 'recorded')",
    )
    .bind(id)
    .bind(installation_owner_id(pool).await.expect("installation owner"))
    .bind(target_kind)
    .bind(target_id)
    .execute(pool)
    .await?;
    Ok(())
}

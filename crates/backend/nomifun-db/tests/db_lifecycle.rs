use nomifun_db::{
    init_database, init_database_memory, init_database_memory_with_owner,
    installation_owner_id,
};
use sqlx::Row;

// -- T1.1 Initialization --

#[tokio::test]
async fn init_creates_users_table() {
    let db = init_database_memory().await.unwrap();

    let count: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM users")
        .fetch_one(db.pool())
        .await
        .unwrap();

    assert!(
        count.0 >= 1,
        "users table should exist and have at least the installation owner"
    );
}

// -- T1.2 Pragma configuration --

#[tokio::test]
async fn pragma_foreign_keys_enabled() {
    let db = init_database_memory().await.unwrap();

    let row: (i64,) = sqlx::query_as("PRAGMA foreign_keys")
        .fetch_one(db.pool())
        .await
        .unwrap();

    assert_eq!(row.0, 1, "foreign_keys should be ON");
}

#[tokio::test]
async fn pragma_busy_timeout() {
    let db = init_database_memory().await.unwrap();

    let row: (i64,) = sqlx::query_as("PRAGMA busy_timeout")
        .fetch_one(db.pool())
        .await
        .unwrap();

    assert_eq!(row.0, 5000, "busy_timeout should be 5000ms");
}

#[tokio::test]
async fn pragma_journal_mode_wal_on_file() {
    let dir = tempfile::tempdir().unwrap();
    let db = init_database(&dir.path().join("test.db")).await.unwrap();

    let row: (String,) = sqlx::query_as("PRAGMA journal_mode")
        .fetch_one(db.pool())
        .await
        .unwrap();

    assert_eq!(
        row.0.to_lowercase(),
        "wal",
        "journal_mode should be WAL for file-backed DB"
    );
    db.close().await;
}

// -- T1.3 Idempotent re-initialization --

#[tokio::test]
async fn idempotent_reinit_preserves_data() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("test.db");

    // First init + insert test data
    let db = init_database(&path).await.unwrap();
    sqlx::query(
        "INSERT INTO users (id, username, password_hash, created_at, updated_at) \
         VALUES ('u1', 'alice', 'hash123', 1000, 1000)",
    )
    .execute(db.pool())
    .await
    .unwrap();

    db.close().await;

    // Second init — data should persist
    let db = init_database(&path).await.unwrap();
    let row = sqlx::query("SELECT username FROM users WHERE id = 'u1'")
        .fetch_one(db.pool())
        .await
        .unwrap();

    assert_eq!(row.get::<String, _>("username"), "alice");
    db.close().await;
}

// -- T1.4 Migrations --

#[tokio::test]
async fn migrations_applied() {
    let db = init_database_memory().await.unwrap();

    let count: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM _sqlx_migrations WHERE success = 1")
        .fetch_one(db.pool())
        .await
        .unwrap();

    assert!(count.0 >= 1, "at least one migration should be applied");
}

#[tokio::test]
async fn local_model_decommission_removes_persisted_product_state() {
    const LOCAL_PROVIDER_ID: &str = "prov_0190f5fe-7c00-7a00-8000-000000000101";
    const CLOUD_PROVIDER_ID: &str = "prov_0190f5fe-7c00-7a00-8000-000000000102";

    let db = init_database_memory().await.unwrap();
    let owner = installation_owner_id(db.pool()).await.unwrap();

    for (id, platform) in [
        (LOCAL_PROVIDER_ID, "nomifun-local-model"),
        (CLOUD_PROVIDER_ID, "openai"),
    ] {
        sqlx::query(
            "INSERT INTO providers (\
                id, platform, name, base_url, api_key_encrypted, models, enabled, \
                capabilities, created_at, updated_at\
             ) VALUES (?, ?, ?, 'https://example.invalid', 'encrypted', '[]', 1, '[]', 1, 1)",
        )
        .bind(id)
        .bind(platform)
        .bind(platform)
        .execute(db.pool())
        .await
        .unwrap();
    }

    sqlx::query(
        "INSERT INTO conversations (\
            id, user_id, name, type, model, created_at, updated_at\
         ) VALUES (\
            'conv_0190f5fe-7c00-7a00-8000-000000000101', ?, 'local chat', 'nomi', ?, 1, 1\
         )",
    )
    .bind(&owner)
    .bind(format!(
        r#"{{"provider_id":"{LOCAL_PROVIDER_ID}","model":"local-model"}}"#
    ))
    .execute(db.pool())
    .await
    .unwrap();

    sqlx::query(
        "INSERT INTO creation_tasks (\
            id, provider_id, model, capability, params, status, submitted_at\
         ) VALUES ('creationtask_0190f5fe-7c00-7a00-8000-000000000101', ?, \
                   'local-image', 'image_generation', '{}', 'completed', 1)",
    )
    .bind(LOCAL_PROVIDER_ID)
    .execute(db.pool())
    .await
    .unwrap();

    for (provider_id, model) in [
        (LOCAL_PROVIDER_ID, "local-image"),
        (CLOUD_PROVIDER_ID, "cloud-image"),
    ] {
        sqlx::query(
            "INSERT INTO model_profiles (\
                provider_id, model, tasks, traits, params, source, updated_at\
             ) VALUES (?, ?, '[\"image_generation\"]', '[]', '{}', 'catalog', 1)",
        )
        .bind(provider_id)
        .bind(model)
        .execute(db.pool())
        .await
        .unwrap();
    }

    sqlx::query(
        "INSERT INTO client_preferences (key, value, updated_at) VALUES (\
            'tools.imageGenerationModel', ?, 1\
         )",
    )
    .bind(format!(
        r#"{{"provider_id":"{LOCAL_PROVIDER_ID}","model":"local-image"}}"#
    ))
    .execute(db.pool())
    .await
    .unwrap();

    sqlx::query(
        "INSERT INTO client_preferences (key, value, updated_at) VALUES (\
            'tools.speechToText', '{\"enabled\":true,\"provider\":\"local\",\"model\":\"retired-asr\"}', 1\
         )",
    )
    .execute(db.pool())
    .await
    .unwrap();

    // Production migrations run with foreign keys disabled so table rebuilds
    // cannot trigger cascades. Exercise the decommission and its repair under
    // that same connection-level setting.
    sqlx::query("PRAGMA foreign_keys = OFF")
        .execute(db.pool())
        .await
        .unwrap();
    sqlx::raw_sql(include_str!("../migrations/003_remove_local_model_support.sql"))
        .execute(db.pool())
        .await
        .unwrap();
    sqlx::raw_sql(include_str!("../migrations/004_repair_disabled_fk_cascades.sql"))
        .execute(db.pool())
        .await
        .unwrap();
    sqlx::query("PRAGMA foreign_keys = ON")
        .execute(db.pool())
        .await
        .unwrap();

    let local_provider_count: (i64,) =
        sqlx::query_as("SELECT COUNT(*) FROM providers WHERE platform = 'nomifun-local-model'")
            .fetch_one(db.pool())
            .await
            .unwrap();
    assert_eq!(local_provider_count.0, 0);

    let conversation_model: (Option<String>,) = sqlx::query_as(
        "SELECT model FROM conversations \
         WHERE id = 'conv_0190f5fe-7c00-7a00-8000-000000000101'",
    )
    .fetch_one(db.pool())
    .await
    .unwrap();
    assert_eq!(conversation_model.0, None);

    let creation_task_count: (i64,) =
        sqlx::query_as("SELECT COUNT(*) FROM creation_tasks WHERE provider_id = ?")
            .bind(LOCAL_PROVIDER_ID)
            .fetch_one(db.pool())
            .await
            .unwrap();
    assert_eq!(creation_task_count.0, 0);

    let stale_preference_count: (i64,) = sqlx::query_as(
        "SELECT COUNT(*) FROM client_preferences \
         WHERE key IN ('tools.imageGenerationModel', 'tools.speechToText')",
    )
    .fetch_one(db.pool())
    .await
    .unwrap();
    assert_eq!(stale_preference_count.0, 0);

    let cloud_profile_source: (String,) = sqlx::query_as(
        "SELECT source FROM model_profiles WHERE provider_id = ? AND model = 'cloud-image'",
    )
    .bind(CLOUD_PROVIDER_ID)
    .fetch_one(db.pool())
    .await
    .unwrap();
    assert_eq!(cloud_profile_source.0, "inferred");

    let local_profile_count: (i64,) =
        sqlx::query_as("SELECT COUNT(*) FROM model_profiles WHERE provider_id = ?")
            .bind(LOCAL_PROVIDER_ID)
            .fetch_one(db.pool())
            .await
            .unwrap();
    assert_eq!(local_profile_count.0, 0);

    let foreign_key_errors: (i64,) =
        sqlx::query_as("SELECT COUNT(*) FROM pragma_foreign_key_check")
            .fetch_one(db.pool())
            .await
            .unwrap();
    assert_eq!(foreign_key_errors.0, 0);
}

#[tokio::test]
async fn upgrade_repairs_model_profiles_orphaned_by_applied_migration_003() {
    const LOCAL_PROVIDER_ID: &str = "prov_0190f5fe-7c00-7a00-8000-000000000111";

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("nomifun-backend.db");
    let db = init_database(&path).await.unwrap();
    sqlx::query(
        "INSERT INTO providers (\
            id, platform, name, base_url, api_key_encrypted, models, enabled, \
            capabilities, created_at, updated_at\
         ) VALUES (?, 'nomifun-local-model', 'Local model', '', '', '[]', 1, '[]', 1, 1)",
    )
    .bind(LOCAL_PROVIDER_ID)
    .execute(db.pool())
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO model_profiles (\
            provider_id, model, tasks, traits, params, source, updated_at\
         ) VALUES (?, 'local-model', '[]', '[]', '{}', 'catalog', 1)",
    )
    .bind(LOCAL_PROVIDER_ID)
    .execute(db.pool())
    .await
    .unwrap();

    let mut conn = db.pool().acquire().await.unwrap();
    sqlx::query("PRAGMA foreign_keys = OFF")
        .execute(&mut *conn)
        .await
        .unwrap();
    sqlx::query("DELETE FROM providers WHERE id = ?")
        .bind(LOCAL_PROVIDER_ID)
        .execute(&mut *conn)
        .await
        .unwrap();
    sqlx::query("DELETE FROM _sqlx_migrations WHERE version >= 4")
        .execute(&mut *conn)
        .await
        .unwrap();
    sqlx::query("PRAGMA foreign_keys = ON")
        .execute(&mut *conn)
        .await
        .unwrap();
    let violation_before = sqlx::query("PRAGMA foreign_key_check")
        .fetch_one(&mut *conn)
        .await
        .unwrap();
    assert_eq!(violation_before.get::<String, _>("table"), "model_profiles");
    assert_eq!(violation_before.get::<String, _>("parent"), "providers");
    drop(conn);
    db.close().await;

    let repaired = init_database(&path).await.unwrap();
    let orphaned_profiles: (i64,) = sqlx::query_as(
        "SELECT COUNT(*) FROM model_profiles profile \
         WHERE NOT EXISTS (SELECT 1 FROM providers provider WHERE provider.id = profile.provider_id)",
    )
    .fetch_one(repaired.pool())
    .await
    .unwrap();
    assert_eq!(orphaned_profiles.0, 0);
    let migration_applied: (i64,) =
        sqlx::query_as("SELECT COUNT(*) FROM _sqlx_migrations WHERE version = 4 AND success = 1")
            .fetch_one(repaired.pool())
            .await
            .unwrap();
    assert_eq!(migration_applied.0, 1);
    repaired.close().await;
}

#[tokio::test]
async fn upgrade_retries_pending_migration_003_with_local_model_template_bindings() {
    const LOCAL_PROVIDER_ID: &str = "prov_0190f5fe-7c00-7a00-8000-000000000121";
    const TEMPLATE_ID: &str = "aetpl_0190f5fe-7c00-7a00-8000-000000000121";
    const PARTICIPANT_ID: &str = "participant-local";
    const CONVERSATION_ID: &str = "conv_0190f5fe-7c00-7a00-8000-000000000121";

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("nomifun-backend.db");
    let db = init_database(&path).await.unwrap();
    let owner = installation_owner_id(db.pool()).await.unwrap();
    sqlx::query(
        "INSERT INTO providers (\
            id, platform, name, base_url, api_key_encrypted, models, enabled, \
            capabilities, created_at, updated_at\
         ) VALUES (?, 'nomifun-local-model', 'Local model', '', '', '[]', 1, '[]', 1, 1)",
    )
    .bind(LOCAL_PROVIDER_ID)
    .execute(db.pool())
    .await
    .unwrap();

    let mut transaction = db.pool().begin().await.unwrap();
    sqlx::query(
        "INSERT INTO agent_execution_templates (\
            id, user_id, name, primary_participant_id, created_at, updated_at\
         ) VALUES (?, ?, 'Local template', ?, 1, 1)",
    )
    .bind(TEMPLATE_ID)
    .bind(&owner)
    .bind(PARTICIPANT_ID)
    .execute(&mut *transaction)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO agent_execution_template_participants (\
            id, template_id, source_agent_id, provider_id, model, created_at, updated_at\
         ) VALUES (?, ?, 'agent_builtin_nomi', ?, 'local-model', 1, 1)",
    )
    .bind(PARTICIPANT_ID)
    .bind(TEMPLATE_ID)
    .bind(LOCAL_PROVIDER_ID)
    .execute(&mut *transaction)
    .await
    .unwrap();
    transaction.commit().await.unwrap();
    sqlx::query(
        "INSERT INTO conversations (\
            id, user_id, name, type, model, execution_template_id, created_at, updated_at\
         ) VALUES (?, ?, 'Template conversation', 'nomi', ?, ?, 1, 1)",
    )
    .bind(CONVERSATION_ID)
    .bind(&owner)
    .bind(format!(
        r#"{{"provider_id":"{LOCAL_PROVIDER_ID}","model":"local-model"}}"#
    ))
    .bind(TEMPLATE_ID)
    .execute(db.pool())
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO model_profiles (\
            provider_id, model, tasks, traits, params, source, updated_at\
         ) VALUES (?, 'local-model', '[]', '[]', '{}', 'catalog', 1)",
    )
    .bind(LOCAL_PROVIDER_ID)
    .execute(db.pool())
    .await
    .unwrap();
    sqlx::query("DELETE FROM _sqlx_migrations WHERE version >= 3")
        .execute(db.pool())
        .await
        .unwrap();
    db.close().await;

    let upgraded = init_database(&path).await.unwrap();
    for (table, where_clause) in [
        ("providers", format!("id = '{LOCAL_PROVIDER_ID}'")),
        ("agent_execution_templates", format!("id = '{TEMPLATE_ID}'")),
        (
            "agent_execution_template_participants",
            format!("template_id = '{TEMPLATE_ID}'"),
        ),
        ("model_profiles", format!("provider_id = '{LOCAL_PROVIDER_ID}'")),
    ] {
        let count: (i64,) = sqlx::query_as(&format!("SELECT COUNT(*) FROM {table} WHERE {where_clause}"))
            .fetch_one(upgraded.pool())
            .await
            .unwrap();
        assert_eq!(count.0, 0, "retired rows should be removed from {table}");
    }
    let template_reference: (Option<String>,) =
        sqlx::query_as("SELECT execution_template_id FROM conversations WHERE id = ?")
            .bind(CONVERSATION_ID)
            .fetch_one(upgraded.pool())
            .await
            .unwrap();
    assert_eq!(template_reference.0, None);
    let foreign_key_errors: (i64,) =
        sqlx::query_as("SELECT COUNT(*) FROM pragma_foreign_key_check")
            .fetch_one(upgraded.pool())
            .await
            .unwrap();
    assert_eq!(foreign_key_errors.0, 0);
    upgraded.close().await;
}

#[test]
fn migration_file_versions_are_unique() {
    let migrations_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("migrations");
    let mut files_by_version = std::collections::BTreeMap::<String, Vec<String>>::new();

    for entry in std::fs::read_dir(&migrations_dir).unwrap() {
        let path = entry.unwrap().path();
        if path.extension().and_then(|ext| ext.to_str()) != Some("sql") {
            continue;
        }

        let file_name = path.file_name().and_then(|name| name.to_str()).unwrap();
        let (version, _) = file_name.split_once('_').unwrap_or_else(|| {
            panic!("migration file {file_name} must start with a numeric version")
        });
        assert!(
            version.chars().all(|ch| ch.is_ascii_digit()),
            "migration file {file_name} must start with a numeric version"
        );
        files_by_version
            .entry(version.to_string())
            .or_default()
            .push(file_name.to_string());
    }

    let duplicates = files_by_version
        .into_iter()
        .filter_map(|(version, files)| {
            (files.len() > 1).then(|| format!("{version}: {}", files.join(", ")))
        })
        .collect::<Vec<_>>();

    assert!(
        duplicates.is_empty(),
        "migration versions must be unique; duplicates: {}",
        duplicates.join("; ")
    );
}

// -- T1.5 Installation owner --

#[tokio::test]
async fn installation_owner_is_a_canonical_user() {
    let db = init_database_memory().await.unwrap();
    let owner = installation_owner_id(db.pool()).await.unwrap();

    let row = sqlx::query("SELECT id, username, password_hash FROM users WHERE id = ?")
        .bind(&owner)
        .fetch_one(db.pool())
        .await
        .unwrap();

    assert_eq!(row.get::<String, _>("id"), owner);
    nomifun_common::UserId::parse(row.get::<String, _>("id")).unwrap();
    assert_eq!(row.get::<String, _>("username"), "admin");
    assert_eq!(
        row.get::<String, _>("password_hash"),
        "",
        "installation owner should have empty password hash"
    );
}

#[tokio::test]
async fn deterministic_memory_fixture_records_the_requested_canonical_owner() {
    let requested = nomifun_common::UserId::new();
    let db = init_database_memory_with_owner(requested.clone()).await.unwrap();

    assert_eq!(
        installation_owner_id(db.pool()).await.unwrap(),
        requested.as_str()
    );
}

#[tokio::test]
async fn installation_owner_has_valid_timestamps() {
    let before = nomifun_common::now_ms();
    let db = init_database_memory().await.unwrap();
    let after = nomifun_common::now_ms();

    let owner = installation_owner_id(db.pool()).await.unwrap();
    let row = sqlx::query("SELECT created_at, updated_at FROM users WHERE id = ?")
        .bind(owner)
        .fetch_one(db.pool())
        .await
        .unwrap();

    let created = row.get::<i64, _>("created_at");
    let updated = row.get::<i64, _>("updated_at");
    assert!(
        created >= before && created <= after,
        "created_at should be within test window"
    );
    assert!(
        updated >= before && updated <= after,
        "updated_at should be within test window"
    );
}

#[tokio::test]
async fn installation_identity_is_one_immutable_singleton() {
    let db = init_database_memory().await.unwrap();
    let owner = installation_owner_id(db.pool()).await.unwrap();

    let rows: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM installation_identity")
        .fetch_one(db.pool())
        .await
        .unwrap();
    assert_eq!(rows, 1);

    let replacement = nomifun_common::UserId::new().into_string();
    sqlx::query(
        "INSERT INTO users (id, username, password_hash, created_at, updated_at) \
         VALUES (?, 'replacement-owner', '', 1, 1)",
    )
    .bind(&replacement)
    .execute(db.pool())
    .await
    .unwrap();

    assert!(
        sqlx::query(
            "UPDATE installation_identity SET owner_user_id = ? WHERE key = 'installation'",
        )
        .bind(&replacement)
        .execute(db.pool())
        .await
        .is_err(),
        "installation owner replacement must fail closed"
    );
    assert!(
        sqlx::query("DELETE FROM installation_identity WHERE key = 'installation'")
            .execute(db.pool())
            .await
            .is_err(),
        "installation identity deletion must fail closed"
    );
    assert_eq!(installation_owner_id(db.pool()).await.unwrap(), owner);
}

#[tokio::test]
async fn file_backed_reopen_preserves_installation_owner() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("nomifun-backend.db");
    let first = init_database(&path).await.unwrap();
    let owner_before = installation_owner_id(first.pool()).await.unwrap();
    first.close().await;

    let reopened = init_database(&path).await.unwrap();
    let owner_after = installation_owner_id(reopened.pool()).await.unwrap();
    assert_eq!(owner_after, owner_before);
}

// -- Schema validation --

#[tokio::test]
async fn users_table_accepts_all_columns() {
    let db = init_database_memory().await.unwrap();

    sqlx::query(
        "INSERT INTO users \
         (id, username, email, password_hash, avatar_path, jwt_secret, created_at, updated_at, last_login) \
         VALUES ('u1', 'testuser', 'test@example.com', 'hash', '/avatar.png', 'secret', 1000, 2000, 3000)",
    )
    .execute(db.pool())
    .await
    .unwrap();

    let row = sqlx::query("SELECT * FROM users WHERE id = 'u1'")
        .fetch_one(db.pool())
        .await
        .unwrap();

    assert_eq!(row.get::<String, _>("email"), "test@example.com");
    assert_eq!(
        row.get::<Option<String>, _>("avatar_path"),
        Some("/avatar.png".to_string())
    );
    assert_eq!(row.get::<Option<String>, _>("jwt_secret"), Some("secret".to_string()));
    assert_eq!(row.get::<Option<i64>, _>("last_login"), Some(3000));
}

#[tokio::test]
async fn username_unique_constraint() {
    let db = init_database_memory().await.unwrap();

    sqlx::query(
        "INSERT INTO users (id, username, password_hash, created_at, updated_at) \
         VALUES ('u1', 'duplicate', 'h', 1, 1)",
    )
    .execute(db.pool())
    .await
    .unwrap();

    let result = sqlx::query(
        "INSERT INTO users (id, username, password_hash, created_at, updated_at) \
         VALUES ('u2', 'duplicate', 'h', 1, 1)",
    )
    .execute(db.pool())
    .await;

    assert!(result.is_err(), "duplicate username should violate unique constraint");
}

#[tokio::test]
async fn email_unique_constraint() {
    let db = init_database_memory().await.unwrap();

    sqlx::query(
        "INSERT INTO users (id, username, email, password_hash, created_at, updated_at) \
         VALUES ('u1', 'user1', 'same@example.com', 'h', 1, 1)",
    )
    .execute(db.pool())
    .await
    .unwrap();

    let result = sqlx::query(
        "INSERT INTO users (id, username, email, password_hash, created_at, updated_at) \
         VALUES ('u2', 'user2', 'same@example.com', 'h', 1, 1)",
    )
    .execute(db.pool())
    .await;

    assert!(result.is_err(), "duplicate email should violate unique constraint");
}

// -- Corruption recovery --

#[tokio::test]
async fn corruption_recovery_creates_backup() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("test.db");

    // Write invalid content to simulate corruption
    std::fs::write(&path, b"not a valid sqlite database").unwrap();

    let db = init_database(&path).await.unwrap();

    // Recovered database should work
    let count: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM users")
        .fetch_one(db.pool())
        .await
        .unwrap();
    assert!(count.0 >= 1, "recovered DB should have an installation owner");

    // Backup file should exist
    let has_backup = std::fs::read_dir(dir.path())
        .unwrap()
        .filter_map(|e| e.ok())
        .any(|e| e.file_name().to_string_lossy().contains("backup"));
    assert!(has_backup, "backup of corrupted file should exist");

    db.close().await;
}

// -- Directory creation --

#[tokio::test]
async fn creates_parent_directories() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("sub").join("nested").join("test.db");

    let db = init_database(&path).await.unwrap();
    assert!(path.exists(), "database file should be created in nested directory");
    db.close().await;
}

// -- Retired numeric-ID lineage quarantine (pre-release hard cut) --
//
// ID contract v2 starts from a clean migration lineage. A database carrying
// the retired integer-ID migration history is moved aside as
// `*.pre-id-v2.bak` and a clean database is created. Nothing is imported
// row-by-row, but the complete old database remains available in quarantine.
//
// The forged "extra applied version" below is picked as ONE PAST the highest
// applied migration so it represents a version absent from the resolved set
// (a real version would collide with the already-applied row).

#[tokio::test]
async fn retired_id_lineage_is_quarantined_and_rebuilt() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("nomifun-backend.db");

    // Build a valid database, then forge a retired migration history by
    // tampering with the clean-baseline checksum and recording an extra
    // applied version.
    let db = init_database(&path).await.unwrap();
    sqlx::query("UPDATE _sqlx_migrations SET checksum = X'00'")
        .execute(db.pool())
        .await
        .unwrap();
    let forged_version: (i64,) =
        sqlx::query_as("SELECT COALESCE(MAX(version), 0) + 1 FROM _sqlx_migrations")
            .fetch_one(db.pool())
            .await
            .unwrap();
    sqlx::query(
        "INSERT INTO _sqlx_migrations (version, description, success, checksum, execution_time) \
         VALUES (?, 'forged future version', TRUE, X'00', 0)",
    )
    .bind(forged_version.0)
    .execute(db.pool())
    .await
    .unwrap();
    // Marker row that must not enter the clean ID-v2 database.
    sqlx::query(
        "INSERT INTO users (id, username, password_hash, created_at, updated_at) \
         VALUES ('u_old', 'old_dev_user', '', 1, 1)",
    )
    .execute(db.pool())
    .await
    .unwrap();
    db.close().await;

    // Re-init: the version mismatch must trigger the rename-and-rebuild path.
    let db = init_database(&path).await.unwrap();

    let old_user: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM users WHERE id = 'u_old'")
        .fetch_one(db.pool())
        .await
        .unwrap();
    assert_eq!(old_user.0, 0, "rebuilt DB must be empty (old data renamed aside)");

    let owner = installation_owner_id(db.pool()).await.unwrap();
    let installation_owner: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM users WHERE id = ?")
        .bind(owner)
        .fetch_one(db.pool())
        .await
        .unwrap();
    assert_eq!(
        installation_owner.0,
        1,
        "rebuilt DB should have the installation owner"
    );

    let backup = dir.path().join("nomifun-backend.db.pre-id-v2.bak");
    assert!(
        backup.exists(),
        "retired database should be preserved as .pre-id-v2.bak"
    );

    db.close().await;
}

#[tokio::test]
async fn retired_id_lineage_with_credentials_is_preserved_in_quarantine() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("nomifun-backend.db");

    // Seed a real credential before forging a retired-lineage mismatch. The
    // active database is still rebuilt because numeric entity IDs are not
    // compatible with ID contract v2, but the original bytes must remain in
    // quarantine rather than being destroyed.
    let db = init_database(&path).await.unwrap();
    sqlx::query(
        "INSERT INTO users (id, username, password_hash, created_at, updated_at) \
         VALUES ('u_real', 'real_user', 'bcrypt_hash', 1, 1)",
    )
    .execute(db.pool())
    .await
    .unwrap();
    sqlx::query("UPDATE _sqlx_migrations SET checksum = X'00'")
        .execute(db.pool())
        .await
        .unwrap();
    db.close().await;

    let rebuilt = init_database(&path).await.unwrap();
    let active_count: (i64,) =
        sqlx::query_as("SELECT COUNT(*) FROM users WHERE id = 'u_real'")
            .fetch_one(rebuilt.pool())
            .await
            .unwrap();
    assert_eq!(
        active_count.0, 0,
        "credential-bearing rows from the retired lineage must not leak into ID-v2"
    );
    rebuilt.close().await;

    let quarantine = dir.path().join("nomifun-backend.db.pre-id-v2.bak");
    assert!(
        quarantine.exists(),
        "the credential-bearing retired database must be preserved in quarantine"
    );
    let pool = sqlx::SqlitePool::connect(&format!("sqlite://{}", quarantine.display()))
        .await
        .unwrap();
    let survived: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM users WHERE id = 'u_real'")
        .fetch_one(&pool)
        .await
        .unwrap();
    pool.close().await;
    assert_eq!(
        survived.0, 1,
        "the real credential row must survive in the quarantined database"
    );
}

#[tokio::test]
async fn retired_id_lineage_numbers_subsequent_quarantines() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("nomifun-backend.db");

    // Occupy the primary quarantine name so the rebuild has to pick a suffix.
    std::fs::write(
        dir.path().join("nomifun-backend.db.pre-id-v2.bak"),
        b"earlier backup",
    )
    .unwrap();

    let db = init_database(&path).await.unwrap();
    sqlx::query("UPDATE _sqlx_migrations SET checksum = X'00'")
        .execute(db.pool())
        .await
        .unwrap();
    db.close().await;

    let db = init_database(&path).await.unwrap();
    let numbered = dir.path().join("nomifun-backend.db.pre-id-v2.bak.1");
    assert!(
        numbered.exists(),
        "second quarantine should get a numeric suffix"
    );
    db.close().await;
}

// -- Concurrent migrator regression (ELECTRON-1KK) --
//
// Repro for the Sentry secondary symptom: two processes opening the same
// SQLite DB on first start (e.g. Electron auto-update spawning the new
// version while the old one is still finalising shutdown, or
// `nomicore doctor` racing the server) both decide to apply the same
// migration version. sqlx-sqlite's lock()/unlock() are no-ops, so without
// the advisory file lock and retry-on-UNIQUE the slower process used to
// blow up with `UNIQUE constraint failed: _sqlx_migrations.version`.
//
// We use OS threads (not tokio::spawn) so each migrator runs on its own
// runtime — this matches the real "two processes" topology more closely
// than cooperative tasks would, and avoids the `&SqlitePool: Send` lifetime
// gymnastics that block tokio::spawn on this future.
#[test]
fn concurrent_init_database_does_not_panic_on_unique_conflict() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("nomifun-backend.db");

    let mut handles = Vec::new();
    for _ in 0..8 {
        let p = path.clone();
        handles.push(std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            rt.block_on(async move { init_database(&p).await })
        }));
    }

    // Every thread must succeed — none should bubble up the UNIQUE-constraint
    // error from `_sqlx_migrations`.
    let mut errors = Vec::new();
    for h in handles {
        match h.join().expect("thread panicked") {
            Ok(_db) => {}
            Err(e) => errors.push(e.to_string()),
        }
    }
    assert!(
        errors.is_empty(),
        "all parallel migrators should succeed, got errors: {errors:?}"
    );

    // All migrators converged on the same baseline schema with no duplicate
    // `_sqlx_migrations` rows.
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(async {
        let db = init_database(&path).await.unwrap();
        let count: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM _sqlx_migrations WHERE success = 1")
            .fetch_one(db.pool())
            .await
            .unwrap();
        assert!(count.0 >= 1, "at least one migration should be recorded");

        let dup: (i64,) = sqlx::query_as(
            "SELECT COUNT(*) FROM (SELECT version FROM _sqlx_migrations GROUP BY version HAVING COUNT(*) > 1)",
        )
        .fetch_one(db.pool())
        .await
        .unwrap();
        assert_eq!(dup.0, 0, "no duplicate versions should ever exist in _sqlx_migrations");
        db.close().await;
    });

    // Lock file is created next to the DB and is harmless to leave behind.
    let lock = path.with_file_name("nomifun-backend.db.migrate.lock");
    assert!(lock.exists(), "advisory lock file should be present after migrate");
}

use nomifun_db::{init_database, init_database_memory, init_database_memory_with_owner};
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

async fn owner_id(pool: &sqlx::SqlitePool) -> String {
    sqlx::query_scalar(
        "SELECT owner_user_id FROM installation_identity \
         WHERE singleton_key = 'installation'",
    )
    .fetch_one(pool)
    .await
    .expect("installation owner")
}

#[tokio::test]
async fn init_creates_v3_users_table_and_owner() {
    let db = init_database_memory().await.unwrap();
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM users")
        .fetch_one(db.pool())
        .await
        .unwrap();
    assert_eq!(count, 1);
    assert!(nomifun_common::validate_uuidv7(&owner_id(db.pool()).await).is_ok());
}

#[tokio::test]
async fn sqlite_busy_timeout_is_configured() {
    let db = init_database_memory().await.unwrap();
    let busy_timeout: i64 = sqlx::query_scalar("PRAGMA busy_timeout")
        .fetch_one(db.pool())
        .await
        .unwrap();
    assert_eq!(busy_timeout, 5000);
}

#[tokio::test]
async fn file_reopen_preserves_rows_and_named_business_ids() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("test.db");
    let db = init_database(&path).await.unwrap();
    let user_id = nomifun_common::UserId::new();
    sqlx::query(
        "INSERT INTO users (user_id, username, password_hash, created_at, updated_at) \
         VALUES (?, 'alice', 'hash123', 1000, 1000)",
    )
    .bind(user_id.as_str())
    .execute(db.pool())
    .await
    .unwrap();
    db.close().await;

    let db = init_database(&path).await.unwrap();
    let row = sqlx::query("SELECT id, user_id, username FROM users WHERE user_id = ?")
        .bind(user_id.as_str())
        .fetch_one(db.pool())
        .await
        .unwrap();
    assert!(row.get::<i64, _>("id") > 0);
    assert_eq!(row.get::<String, _>("user_id"), user_id.as_str());
    assert_eq!(row.get::<String, _>("username"), "alice");
    db.close().await;
}

#[tokio::test]
async fn migration_is_the_single_v3_baseline() {
    let db = init_database_memory().await.unwrap();
    let migrations: Vec<(i64, String)> =
        sqlx::query_as("SELECT version, description FROM _sqlx_migrations WHERE success = 1")
            .fetch_all(db.pool())
            .await
            .unwrap();
    assert_eq!(migrations.len(), 1);
    assert_eq!(migrations[0].0, 1);
    assert!(migrations[0].1.contains("v3 baseline"));
    assert_eq!(BASELINE.matches("CREATE TABLE ").count(), 64);
}

#[test]
fn migration_files_have_one_numeric_version_and_v3_has_no_relationship_tokens() {
    let migrations_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("migrations");
    let files = std::fs::read_dir(&migrations_dir)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .collect::<Vec<_>>();
    assert_eq!(files.len(), 1);
    assert_eq!(files[0], "001_v3_baseline.sql");

    let tokens = sql_tokens(&executable_baseline_sql());
    for forbidden in [["FOREIGN", "KEY"], ["ON", "DELETE"], ["ON", "UPDATE"]] {
        assert!(
            !tokens
                .windows(forbidden.len())
                .any(|window| window.iter().map(String::as_str).eq(forbidden)),
            "forbidden v3 tokens: {forbidden:?}"
        );
    }
    for forbidden in ["REFERENCES", "CASCADE", "TRIGGER"] {
        assert!(
            !tokens.iter().any(|token| token == forbidden),
            "forbidden v3 token: {forbidden}"
        );
    }
    assert!(
        !BASELINE.contains("_row_id"),
        "v3 baseline must not reintroduce dual-key row-id columns"
    );
}

#[tokio::test]
async fn deterministic_memory_fixture_records_requested_owner_as_business_id() {
    let requested = nomifun_common::UserId::new();
    let db = init_database_memory_with_owner(requested.clone()).await.unwrap();
    assert_eq!(owner_id(db.pool()).await, requested.as_str());
    // `users.id` is the SQLite technical primary key; the business identity
    // asserted above is `users.user_id`.
    let user_technical_id: i64 =
        sqlx::query_scalar("SELECT id FROM users WHERE user_id = ?")
        .bind(requested.as_str())
        .fetch_one(db.pool())
        .await
        .unwrap();
    assert!(user_technical_id > 0);
}

#[tokio::test]
async fn installation_identity_is_a_singleton_by_named_key() {
    let db = init_database_memory().await.unwrap();
    let result = sqlx::query(
        "INSERT INTO installation_identity (singleton_key, owner_user_id) \
         VALUES ('installation', ?)",
    )
    .bind(owner_id(db.pool()).await)
    .execute(db.pool())
    .await;
    assert!(result.is_err(), "singleton_key must remain unique");
}

#[tokio::test]
async fn users_table_accepts_nullable_columns_with_named_user_id() {
    let db = init_database_memory().await.unwrap();
    let user_id = nomifun_common::UserId::new();
    sqlx::query(
        "INSERT INTO users \
         (user_id, username, email, password_hash, avatar_path, jwt_secret, created_at, updated_at, last_login) \
         VALUES (?, 'testuser', 'test@example.com', 'hash', '/avatar.png', 'secret', 1000, 2000, 3000)",
    )
    .bind(user_id.as_str())
    .execute(db.pool())
    .await
    .unwrap();
    let row = sqlx::query("SELECT * FROM users WHERE user_id = ?")
        .bind(user_id.as_str())
        .fetch_one(db.pool())
        .await
        .unwrap();
    assert_eq!(row.get::<String, _>("email"), "test@example.com");
    assert_eq!(
        row.get::<Option<String>, _>("avatar_path"),
        Some("/avatar.png".to_owned())
    );
    assert_eq!(row.get::<Option<String>, _>("jwt_secret"), Some("secret".to_owned()));
    assert_eq!(row.get::<Option<i64>, _>("last_login"), Some(3000));
}

#[tokio::test]
async fn username_and_email_remain_unique() {
    let db = init_database_memory().await.unwrap();
    let first = nomifun_common::UserId::new();
    let second = nomifun_common::UserId::new();
    for (user_id, username, email) in [
        (first.as_str(), "duplicate", Some("same@example.com")),
        (second.as_str(), "duplicate", Some("same@example.com")),
    ] {
        let result = sqlx::query(
            "INSERT INTO users (user_id, username, email, password_hash, created_at, updated_at) \
             VALUES (?, ?, ?, 'h', 1, 1)",
        )
        .bind(user_id)
        .bind(username)
        .bind(email)
        .execute(db.pool())
        .await;
        if user_id == second.as_str() {
            assert!(result.is_err());
        } else {
            result.unwrap();
        }
    }
}

#[tokio::test]
async fn migration_lineage_mismatch_fails_fast_without_replacing_database() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("nomifun-backend.db");
    let db = init_database(&path).await.unwrap();
    let old_id = nomifun_common::UserId::new();
    sqlx::query(
        "INSERT INTO users (user_id, username, password_hash, created_at, updated_at) \
         VALUES (?, 'old_dev_user', '', 1, 1)",
    )
    .bind(old_id.as_str())
    .execute(db.pool())
    .await
    .unwrap();
    sqlx::query("UPDATE _sqlx_migrations SET checksum = X'00'")
        .execute(db.pool())
        .await
        .unwrap();
    db.close().await;

    let error = init_database(&path)
        .await
        .expect_err("migration lineage mismatch must fail fast");
    let message = error.to_string().to_ascii_lowercase();
    assert!(
        message.contains("migration")
            || message.contains("version")
            || message.contains("checksum"),
        "unexpected lineage error: {error}"
    );
    assert!(path.exists(), "DB initialization must not replace the source file");
    assert!(
        !std::fs::read_dir(dir.path())
            .unwrap()
            .filter_map(Result::ok)
            .any(|entry| entry.file_name().to_string_lossy().contains("pre-id")),
        "DB layer must not create lineage quarantine files"
    );
}

#[tokio::test]
async fn creates_parent_directories() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("sub").join("nested").join("test.db");
    let db = init_database(&path).await.unwrap();
    assert!(path.exists());
    db.close().await;
}

#[tokio::test]
async fn corruption_fails_closed_without_replacing_database() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("test.db");
    let corrupt_bytes = b"not a valid sqlite database";
    std::fs::write(&path, corrupt_bytes).unwrap();

    init_database(&path)
        .await
        .expect_err("the database layer must not replace corrupted input");

    assert_eq!(std::fs::read(&path).unwrap(), corrupt_bytes);
    assert!(
        !std::fs::read_dir(dir.path())
            .unwrap()
            .filter_map(Result::ok)
            .any(|entry| entry.file_name().to_string_lossy().contains("backup")),
        "the database layer must not create an ad-hoc recovery dataset"
    );
}

#[test]
fn concurrent_initializers_converge_on_one_v3_migration() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("nomifun-backend.db");
    let mut handles = Vec::new();
    for _ in 0..4 {
        let path = path.clone();
        handles.push(std::thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            runtime.block_on(async move { init_database(&path).await })
        }));
    }
    for handle in handles {
        let database = handle.join().expect("initializer thread");
        assert!(database.is_ok(), "concurrent v3 initialization failed: {database:?}");
    }
}

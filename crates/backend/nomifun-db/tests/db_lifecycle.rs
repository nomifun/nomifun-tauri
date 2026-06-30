use nomifun_db::{init_database, init_database_memory};
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
        "users table should exist and have at least the system user"
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

// -- T1.5 System default user --

#[tokio::test]
async fn system_default_user_exists() {
    let db = init_database_memory().await.unwrap();

    let row = sqlx::query("SELECT id, username, password_hash FROM users WHERE id = 'system_default_user'")
        .fetch_one(db.pool())
        .await
        .unwrap();

    assert_eq!(row.get::<String, _>("id"), "system_default_user");
    assert_eq!(row.get::<String, _>("username"), "admin");
    assert_eq!(
        row.get::<String, _>("password_hash"),
        "",
        "system user should have empty password hash"
    );
}

#[tokio::test]
async fn system_user_has_valid_timestamps() {
    let before = nomifun_common::now_ms();
    let db = init_database_memory().await.unwrap();
    let after = nomifun_common::now_ms();

    let row = sqlx::query("SELECT created_at, updated_at FROM users WHERE id = 'system_default_user'")
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
    assert!(count.0 >= 1, "recovered DB should have system user");

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

// -- Pre-baseline rebuild (pre-launch convenience; removed before release) --
//
// The 2026-06-12 clean-baseline refactor squashed migrations 001–021 into a
// single 001_baseline.sql, resetting the migration chain. A dev database
// carrying the old `_sqlx_migrations` history (mismatched checksum on
// version 1, applied versions 2–N missing from the resolved set) must be
// renamed to `*.pre-baseline.bak` and rebuilt empty instead of failing fast.
//
// The forged "extra applied version" below must stay ONE PAST the highest
// real migration on disk so it represents a version absent from the resolved
// set (a real version would collide with the already-applied row). Bump it
// whenever a new migration lands.

#[tokio::test]
async fn pre_baseline_database_is_renamed_and_rebuilt() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("nomifun-backend.db");

    // Build a valid database, then forge a pre-baseline migration history:
    // tamper the baseline checksum and record extra applied versions.
    let db = init_database(&path).await.unwrap();
    sqlx::query("UPDATE _sqlx_migrations SET checksum = X'00'")
        .execute(db.pool())
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO _sqlx_migrations (version, description, success, checksum, execution_time) \
         VALUES (24, 'entity seq', TRUE, X'00', 0)",
    )
    .execute(db.pool())
    .await
    .unwrap();
    // Marker row that must NOT survive the rebuild.
    sqlx::query(
        "INSERT INTO users (id, username, password_hash, created_at, updated_at) \
         VALUES ('u_old', 'old_dev_user', 'h', 1, 1)",
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

    let system_user: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM users WHERE id = 'system_default_user'")
        .fetch_one(db.pool())
        .await
        .unwrap();
    assert_eq!(system_user.0, 1, "rebuilt DB should have the system user");

    let backup = dir.path().join("nomifun-backend.db.pre-baseline.bak");
    assert!(backup.exists(), "old database should be preserved as .pre-baseline.bak");

    db.close().await;
}

#[tokio::test]
async fn pre_baseline_rebuild_numbers_subsequent_backups() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("nomifun-backend.db");

    // Occupy the primary backup name so the rebuild has to pick a suffix.
    std::fs::write(dir.path().join("nomifun-backend.db.pre-baseline.bak"), b"earlier backup").unwrap();

    let db = init_database(&path).await.unwrap();
    sqlx::query("UPDATE _sqlx_migrations SET checksum = X'00'")
        .execute(db.pool())
        .await
        .unwrap();
    db.close().await;

    let db = init_database(&path).await.unwrap();
    let numbered = dir.path().join("nomifun-backend.db.pre-baseline.bak.1");
    assert!(numbered.exists(), "second backup should get a numeric suffix");
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

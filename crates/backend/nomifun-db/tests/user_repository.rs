//! Black-box integration tests for IUserRepository (test-plan T2.1 – T2.13).
//!
//! Tests exercise the public trait interface against an in-memory SQLite database.
//! Internal details like SQL queries or column names are not referenced.

use std::sync::Arc;

use nomifun_db::{DbError, IUserRepository, SqliteUserRepository, init_database_memory};

async fn repo() -> Arc<dyn IUserRepository> {
    let db = init_database_memory().await.unwrap();
    Arc::new(SqliteUserRepository::new(db.pool().clone()))
}

// -- T2.1 Create user --

#[tokio::test]
async fn t2_1_create_user_returns_user_with_populated_fields() {
    let r = repo().await;
    let user = r.create_user("testuser", "$2b$12$fakehash").await.unwrap();

    assert!(!user.id.is_empty(), "id should be non-empty");
    assert_eq!(user.username, "testuser");
    assert_eq!(user.password_hash, "$2b$12$fakehash");
    assert!(user.created_at > 0);
    assert!(user.updated_at > 0);
}

#[tokio::test]
async fn t2_1_create_user_duplicate_username_returns_conflict() {
    let r = repo().await;
    r.create_user("dup", "h1").await.unwrap();

    let err = r.create_user("dup", "h2").await.unwrap_err();
    assert!(matches!(err, DbError::Conflict(_)), "expected Conflict, got: {err:?}");
}

// -- T2.2 Find by username --

#[tokio::test]
async fn t2_2_find_by_username_existing() {
    let r = repo().await;
    r.create_user("findme", "h").await.unwrap();

    let found = r.find_by_username("findme").await.unwrap();
    assert!(found.is_some());
    assert_eq!(found.unwrap().username, "findme");
}

#[tokio::test]
async fn t2_2_find_by_username_nonexistent_returns_none() {
    let r = repo().await;
    assert!(r.find_by_username("ghost").await.unwrap().is_none());
}

// -- T2.3 Find by ID --

#[tokio::test]
async fn t2_3_find_by_id_existing() {
    let r = repo().await;
    let user = r.create_user("byid", "h").await.unwrap();

    let found = r.find_by_id(&user.id).await.unwrap();
    assert!(found.is_some());
    assert_eq!(found.unwrap().id, user.id);
}

#[tokio::test]
async fn t2_3_find_by_id_nonexistent_returns_none() {
    let r = repo().await;
    assert!(r.find_by_id("no_such_id").await.unwrap().is_none());
}

// -- T2.4 List all users --

#[tokio::test]
async fn t2_4_list_users_returns_all() {
    let r = repo().await;
    r.create_user("u1", "h").await.unwrap();
    r.create_user("u2", "h").await.unwrap();

    let users = r.list_users().await.unwrap();
    // system_default_user + u1 + u2
    assert_eq!(users.len(), 3);
}

// -- T2.5 Count users --

#[tokio::test]
async fn t2_5_count_users() {
    let r = repo().await;
    r.create_user("counted", "h").await.unwrap();

    // system_default_user + counted
    assert_eq!(r.count_users().await.unwrap(), 2);
}

// -- T2.6 has_users --

#[tokio::test]
async fn t2_6_has_users_false_with_only_empty_password_system_user() {
    let r = repo().await;
    assert!(!r.has_users().await.unwrap());
}

#[tokio::test]
async fn t2_6_has_users_true_with_real_user() {
    let r = repo().await;
    r.create_user("real", "bcrypt_hash").await.unwrap();
    assert!(r.has_users().await.unwrap());
}

// -- T2.7 Get system user --

#[tokio::test]
async fn t2_7_get_system_user_returns_default() {
    let r = repo().await;
    let user = r.get_system_user().await.unwrap();
    assert!(user.is_some());

    let user = user.unwrap();
    assert_eq!(user.id, "system_default_user");
}

// -- T2.8 Get primary WebUI user --

#[tokio::test]
async fn t2_8_primary_webui_user_is_system_user_when_only_system() {
    let r = repo().await;
    let user = r.get_primary_webui_user().await.unwrap().unwrap();
    assert_eq!(user.id, "system_default_user");
}

#[tokio::test]
async fn t2_8_primary_webui_user_prefers_system_over_admin() {
    let r = repo().await;
    // Can't create another user called "admin" now that seed uses it.
    // The priority check still holds: any non-system user must not shadow system.
    r.create_user("other", "h").await.unwrap();

    let user = r.get_primary_webui_user().await.unwrap().unwrap();
    assert_eq!(
        user.id, "system_default_user",
        "system user should take priority over non-system users"
    );
}

// -- T2.9 Set system user credentials --

#[tokio::test]
async fn t2_9_set_system_user_credentials_updates_username_and_hash() {
    let r = repo().await;
    r.set_system_user_credentials("newadmin", "secure_hash").await.unwrap();

    let user = r.get_system_user().await.unwrap().unwrap();
    assert_eq!(user.username, "newadmin");
    assert_eq!(user.password_hash, "secure_hash");
}

#[tokio::test]
async fn t2_9_set_system_user_credentials_conflict_with_existing_username() {
    let r = repo().await;
    r.create_user("existing", "h").await.unwrap();

    let err = r.set_system_user_credentials("existing", "hash").await.unwrap_err();
    assert!(matches!(err, DbError::Conflict(_)), "expected Conflict, got: {err:?}");
}

#[tokio::test]
async fn t2_9_set_system_user_password_if_uninitialized_preserves_username() {
    let r = repo().await;
    // Rename the system admin while its password is still empty (WebUI-off edit).
    r.update_username("system_default_user", "renamed").await.unwrap();

    let wrote = r.set_system_user_password_if_uninitialized("hash1").await.unwrap();
    assert!(wrote, "should provision the password when empty");

    let user = r.get_system_user().await.unwrap().unwrap();
    assert_eq!(user.username, "renamed", "username must NOT be reset to admin");
    assert_eq!(user.password_hash, "hash1");

    // Idempotent: a second enable does not overwrite the stored password.
    let wrote_again = r.set_system_user_password_if_uninitialized("hash2").await.unwrap();
    assert!(!wrote_again);
    let user = r.get_system_user().await.unwrap().unwrap();
    assert_eq!(user.password_hash, "hash1");
    assert_eq!(user.username, "renamed");
}

// -- T2.10 Update password --

#[tokio::test]
async fn t2_10_update_password_changes_hash_and_updated_at() {
    let r = repo().await;
    let user = r.create_user("pwduser", "old").await.unwrap();

    r.update_password(&user.id, "new_hash").await.unwrap();

    let updated = r.find_by_id(&user.id).await.unwrap().unwrap();
    assert_eq!(updated.password_hash, "new_hash");
    assert!(updated.updated_at >= user.updated_at);
}

// -- T2.11 Update username --

#[tokio::test]
async fn t2_11_update_username_succeeds() {
    let r = repo().await;
    let user = r.create_user("oldname", "h").await.unwrap();

    r.update_username(&user.id, "newname").await.unwrap();

    let updated = r.find_by_id(&user.id).await.unwrap().unwrap();
    assert_eq!(updated.username, "newname");
}

#[tokio::test]
async fn t2_11_update_username_conflict_with_existing() {
    let r = repo().await;
    r.create_user("taken", "h").await.unwrap();
    let other = r.create_user("free", "h").await.unwrap();

    let err = r.update_username(&other.id, "taken").await.unwrap_err();
    assert!(matches!(err, DbError::Conflict(_)), "expected Conflict, got: {err:?}");
}

// -- T2.12 Update last login --

#[tokio::test]
async fn t2_12_update_last_login_sets_timestamp() {
    let r = repo().await;
    let user = r.create_user("loginuser", "h").await.unwrap();
    assert!(user.last_login.is_none());

    r.update_last_login(&user.id).await.unwrap();

    let updated = r.find_by_id(&user.id).await.unwrap().unwrap();
    assert!(updated.last_login.is_some());
    assert!(updated.last_login.unwrap() > 0);
}

// -- T2.13 Update JWT secret --

#[tokio::test]
async fn t2_13_update_jwt_secret_sets_value() {
    let r = repo().await;
    let user = r.create_user("jwtuser", "h").await.unwrap();
    assert!(user.jwt_secret.is_none());

    r.update_jwt_secret(&user.id, "my_secret").await.unwrap();

    let updated = r.find_by_id(&user.id).await.unwrap().unwrap();
    assert_eq!(updated.jwt_secret.as_deref(), Some("my_secret"));
}

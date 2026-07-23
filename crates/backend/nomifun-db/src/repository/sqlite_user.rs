use sqlx::SqlitePool;

use crate::error::DbError;
use crate::models::User;
use crate::repository::IUserRepository;

/// SQLite-backed implementation of [`IUserRepository`].
#[derive(Clone, Debug)]
pub struct SqliteUserRepository {
    pool: SqlitePool,
}

impl SqliteUserRepository {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }
}

#[async_trait::async_trait]
impl IUserRepository for SqliteUserRepository {
    async fn has_users(&self) -> Result<bool, DbError> {
        let row: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM users WHERE password_hash != ''")
            .fetch_one(&self.pool)
            .await?;

        Ok(row.0 > 0)
    }

    async fn get_system_user(&self) -> Result<Option<User>, DbError> {
        let user = sqlx::query_as::<_, User>(
            "SELECT users.* \
             FROM installation_identity identity \
             JOIN users ON users.user_id = identity.owner_user_id \
             WHERE identity.singleton_key = 'installation'",
        )
        .fetch_optional(&self.pool)
        .await?;

        Ok(user)
    }

    async fn get_primary_webui_user(&self) -> Result<Option<User>, DbError> {
        self.get_system_user().await
    }

    async fn set_system_user_credentials(&self, username: &str, password_hash: &str) -> Result<(), DbError> {
        let now = nomifun_common::now_ms();
        let result = sqlx::query(
            "UPDATE users SET username = ?, password_hash = ?, updated_at = ? \
             WHERE user_id = (SELECT owner_user_id FROM installation_identity \
                              WHERE singleton_key = 'installation')",
        )
        .bind(username)
        .bind(password_hash)
        .bind(now)
        .execute(&self.pool)
        .await
        .map_err(|e| match &e {
            sqlx::Error::Database(db_err) if is_unique_violation(db_err.as_ref()) => {
                DbError::Conflict(format!("Username '{username}' already exists"))
            }
            _ => DbError::Query(e),
        })?;

        if result.rows_affected() == 0 {
            return Err(DbError::NotFound(
                "installation owner user not found".to_string(),
            ));
        }

        Ok(())
    }

    async fn set_system_user_credentials_if_uninitialized(
        &self,
        username: &str,
        password_hash: &str,
    ) -> Result<bool, DbError> {
        let now = nomifun_common::now_ms();
        // The WHERE clause is the gate: it matches only the freshly-seeded
        // installation owner (empty/NULL password). SQLite serialises writers, so two
        // concurrent first-run callers cannot both match — the second sees the
        // already-populated hash and updates 0 rows.
        let result = sqlx::query(
            "UPDATE users SET username = ?, password_hash = ?, updated_at = ? \
             WHERE user_id = (SELECT owner_user_id FROM installation_identity \
                              WHERE singleton_key = 'installation') \
               AND (password_hash = '' OR password_hash IS NULL)",
        )
        .bind(username)
        .bind(password_hash)
        .bind(now)
        .execute(&self.pool)
        .await
        .map_err(|e| match &e {
            sqlx::Error::Database(db_err) if is_unique_violation(db_err.as_ref()) => {
                DbError::Conflict(format!("Username '{username}' already exists"))
            }
            _ => DbError::Query(e),
        })?;

        Ok(result.rows_affected() > 0)
    }

    async fn set_system_user_password_if_uninitialized(&self, password_hash: &str) -> Result<bool, DbError> {
        let now = nomifun_common::now_ms();
        // Only the password column is touched, and only while it is still empty
        // — the username the user may have set is preserved. The WHERE clause is
        // the gate, so a second concurrent enable writes 0 rows.
        let result = sqlx::query(
            "UPDATE users SET password_hash = ?, updated_at = ? \
             WHERE user_id = (SELECT owner_user_id FROM installation_identity \
                              WHERE singleton_key = 'installation') \
               AND (password_hash = '' OR password_hash IS NULL)",
        )
        .bind(password_hash)
        .bind(now)
        .execute(&self.pool)
        .await?;

        Ok(result.rows_affected() > 0)
    }

    async fn create_user(&self, username: &str, password_hash: &str) -> Result<User, DbError> {
        let id = nomifun_common::UserId::new();
        let now = nomifun_common::now_ms();

        let row_id: i64 = sqlx::query_scalar(
            "INSERT INTO users (user_id, username, password_hash, created_at, updated_at) \
             VALUES (?, ?, ?, ?, ?) RETURNING id",
        )
        .bind(id.as_str())
        .bind(username)
        .bind(password_hash)
        .bind(now)
        .bind(now)
        .fetch_one(&self.pool)
        .await
        .map_err(|e| match &e {
            sqlx::Error::Database(db_err) if is_unique_violation(db_err.as_ref()) => {
                DbError::Conflict(format!("Username '{username}' already exists"))
            }
            _ => DbError::Query(e),
        })?;

        Ok(User {
            id: row_id,
            user_id: id,
            username: username.to_string(),
            email: None,
            password_hash: password_hash.to_string(),
            avatar_path: None,
            jwt_secret: None,
            created_at: now,
            updated_at: now,
            last_login: None,
        })
    }

    async fn find_by_username(&self, username: &str) -> Result<Option<User>, DbError> {
        let user = sqlx::query_as::<_, User>("SELECT * FROM users WHERE username = ?")
            .bind(username)
            .fetch_optional(&self.pool)
            .await?;

        Ok(user)
    }

    async fn find_by_id(&self, id: &str) -> Result<Option<User>, DbError> {
        let user = sqlx::query_as::<_, User>("SELECT * FROM users WHERE user_id = ?")
            .bind(id)
            .fetch_optional(&self.pool)
            .await?;

        Ok(user)
    }

    async fn list_users(&self) -> Result<Vec<User>, DbError> {
        let users = sqlx::query_as::<_, User>("SELECT * FROM users")
            .fetch_all(&self.pool)
            .await?;

        Ok(users)
    }

    async fn count_users(&self) -> Result<i64, DbError> {
        let row: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM users")
            .fetch_one(&self.pool)
            .await?;

        Ok(row.0)
    }

    async fn update_password(&self, user_id: &str, password_hash: &str) -> Result<(), DbError> {
        let now = nomifun_common::now_ms();
        let result = sqlx::query("UPDATE users SET password_hash = ?, updated_at = ? WHERE user_id = ?")
            .bind(password_hash)
            .bind(now)
            .bind(user_id)
            .execute(&self.pool)
            .await?;

        if result.rows_affected() == 0 {
            return Err(DbError::NotFound(format!("User '{user_id}' not found")));
        }

        Ok(())
    }

    async fn update_username(&self, user_id: &str, username: &str) -> Result<(), DbError> {
        let now = nomifun_common::now_ms();
        let result = sqlx::query("UPDATE users SET username = ?, updated_at = ? WHERE user_id = ?")
            .bind(username)
            .bind(now)
            .bind(user_id)
            .execute(&self.pool)
            .await
            .map_err(|e| match &e {
                sqlx::Error::Database(db_err) if is_unique_violation(db_err.as_ref()) => {
                    DbError::Conflict(format!("Username '{username}' already exists"))
                }
                _ => DbError::Query(e),
            })?;

        if result.rows_affected() == 0 {
            return Err(DbError::NotFound(format!("User '{user_id}' not found")));
        }

        Ok(())
    }

    async fn update_last_login(&self, user_id: &str) -> Result<(), DbError> {
        let now = nomifun_common::now_ms();
        let result = sqlx::query("UPDATE users SET last_login = ?, updated_at = ? WHERE user_id = ?")
            .bind(now)
            .bind(now)
            .bind(user_id)
            .execute(&self.pool)
            .await?;

        if result.rows_affected() == 0 {
            return Err(DbError::NotFound(format!("User '{user_id}' not found")));
        }

        Ok(())
    }

    async fn update_jwt_secret(&self, user_id: &str, jwt_secret: &str) -> Result<(), DbError> {
        let now = nomifun_common::now_ms();
        let result = sqlx::query("UPDATE users SET jwt_secret = ?, updated_at = ? WHERE user_id = ?")
            .bind(jwt_secret)
            .bind(now)
            .bind(user_id)
            .execute(&self.pool)
            .await?;

        if result.rows_affected() == 0 {
            return Err(DbError::NotFound(format!("User '{user_id}' not found")));
        }

        Ok(())
    }
}

/// Checks if a SQLite database error is a UNIQUE constraint violation.
fn is_unique_violation(err: &dyn sqlx::error::DatabaseError) -> bool {
    // SQLite error code 2067 = SQLITE_CONSTRAINT_UNIQUE
    err.code().is_some_and(|c| c == "2067")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::init_database_memory;

    async fn setup() -> (SqliteUserRepository, crate::Database) {
        let db = init_database_memory().await.unwrap();
        let repo = SqliteUserRepository::new(db.pool().clone());
        (repo, db)
    }

    // -- Unit tests for is_unique_violation helper --

    #[test]
    fn unique_violation_code_detected() {
        // SQLite UNIQUE violation has code "2067"
        assert!(is_unique_violation(&FakeDbError("2067")));
    }

    #[test]
    fn non_unique_violation_code_rejected() {
        assert!(!is_unique_violation(&FakeDbError("1555")));
    }

    /// Minimal fake for testing is_unique_violation.
    struct FakeDbError(&'static str);

    impl std::fmt::Display for FakeDbError {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            write!(f, "fake error")
        }
    }

    impl std::fmt::Debug for FakeDbError {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            write!(f, "FakeDbError({})", self.0)
        }
    }

    impl std::error::Error for FakeDbError {}

    impl sqlx::error::DatabaseError for FakeDbError {
        fn message(&self) -> &str {
            "fake"
        }
        fn kind(&self) -> sqlx::error::ErrorKind {
            sqlx::error::ErrorKind::UniqueViolation
        }
        fn code(&self) -> Option<std::borrow::Cow<'_, str>> {
            Some(std::borrow::Cow::Borrowed(self.0))
        }
        fn as_error(&self) -> &(dyn std::error::Error + Send + Sync + 'static) {
            self
        }
        fn as_error_mut(&mut self) -> &mut (dyn std::error::Error + Send + Sync + 'static) {
            self
        }
        fn into_error(self: Box<Self>) -> Box<dyn std::error::Error + Send + Sync + 'static> {
            self
        }
    }

    // -- Integration tests that exercise the repository against in-memory SQLite --

    #[tokio::test]
    async fn create_user_returns_populated_fields() {
        let (repo, _db) = setup().await;
        let user = repo.create_user("alice", "hash123").await.unwrap();

        assert!(nomifun_common::UserId::parse(user.user_id.as_str()).is_ok());
        assert_eq!(user.username, "alice");
        assert_eq!(user.password_hash, "hash123");
        assert!(user.email.is_none());
        assert!(user.avatar_path.is_none());
        assert!(user.jwt_secret.is_none());
        assert!(user.last_login.is_none());
        assert!(user.created_at > 0);
        assert_eq!(user.created_at, user.updated_at);
    }

    #[tokio::test]
    async fn create_user_duplicate_username_returns_conflict() {
        let (repo, _db) = setup().await;
        repo.create_user("bob", "h1").await.unwrap();

        let err = repo.create_user("bob", "h2").await.unwrap_err();
        assert!(matches!(err, DbError::Conflict(_)));
    }

    #[tokio::test]
    async fn has_users_false_when_only_system_user() {
        let (repo, _db) = setup().await;
        assert!(!repo.has_users().await.unwrap());
    }

    #[tokio::test]
    async fn has_users_true_after_creating_real_user() {
        let (repo, _db) = setup().await;
        repo.create_user("real", "pass").await.unwrap();
        assert!(repo.has_users().await.unwrap());
    }

    #[tokio::test]
    async fn get_system_user_returns_default() {
        let (repo, db) = setup().await;
        let owner = crate::installation_owner_id(db.pool()).await.unwrap();
        let user = repo.get_system_user().await.unwrap().unwrap();
        assert_eq!(user.user_id.as_str(), owner);
        assert_eq!(user.username, "admin");
    }

    #[tokio::test]
    async fn get_primary_webui_user_returns_system_user_first() {
        let (repo, db) = setup().await;
        let owner = crate::installation_owner_id(db.pool()).await.unwrap();
        repo.create_user("other", "hash").await.unwrap();

        let user = repo.get_primary_webui_user().await.unwrap().unwrap();
        assert_eq!(user.user_id.as_str(), owner);
    }

    #[tokio::test]
    async fn find_by_username_existing() {
        let (repo, _db) = setup().await;
        repo.create_user("charlie", "h").await.unwrap();

        let found = repo.find_by_username("charlie").await.unwrap();
        assert!(found.is_some());
        assert_eq!(found.unwrap().username, "charlie");
    }

    #[tokio::test]
    async fn find_by_username_missing() {
        let (repo, _db) = setup().await;
        assert!(repo.find_by_username("ghost").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn find_by_id_existing() {
        let (repo, _db) = setup().await;
        let created = repo.create_user("dave", "h").await.unwrap();

        let found = repo.find_by_id(created.user_id.as_str()).await.unwrap();
        assert!(found.is_some());
        assert_eq!(found.unwrap().user_id, created.user_id);
    }

    #[tokio::test]
    async fn find_by_id_missing() {
        let (repo, _db) = setup().await;
        assert!(repo.find_by_id("nonexistent").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn list_users_includes_system_and_created() {
        let (repo, _db) = setup().await;
        repo.create_user("eve", "h").await.unwrap();
        repo.create_user("frank", "h").await.unwrap();

        let users = repo.list_users().await.unwrap();
        // installation owner + eve + frank
        assert_eq!(users.len(), 3);
    }

    #[tokio::test]
    async fn count_users_includes_all() {
        let (repo, _db) = setup().await;
        repo.create_user("grace", "h").await.unwrap();

        // installation owner + grace
        assert_eq!(repo.count_users().await.unwrap(), 2);
    }

    #[tokio::test]
    async fn update_password_succeeds() {
        let (repo, _db) = setup().await;
        let user = repo.create_user("hal", "old_hash").await.unwrap();

        repo.update_password(user.user_id.as_str(), "new_hash").await.unwrap();

        let updated = repo.find_by_id(user.user_id.as_str()).await.unwrap().unwrap();
        assert_eq!(updated.password_hash, "new_hash");
        assert!(updated.updated_at >= user.updated_at);
    }

    #[tokio::test]
    async fn update_password_nonexistent_user() {
        let (repo, _db) = setup().await;
        let err = repo.update_password("no_such_id", "h").await.unwrap_err();
        assert!(matches!(err, DbError::NotFound(_)));
    }

    #[tokio::test]
    async fn update_username_succeeds() {
        let (repo, _db) = setup().await;
        let user = repo.create_user("ivan", "h").await.unwrap();

        repo.update_username(user.user_id.as_str(), "ivan_new").await.unwrap();

        let updated = repo.find_by_id(user.user_id.as_str()).await.unwrap().unwrap();
        assert_eq!(updated.username, "ivan_new");
    }

    #[tokio::test]
    async fn update_username_conflict() {
        let (repo, _db) = setup().await;
        repo.create_user("jane", "h").await.unwrap();
        let other = repo.create_user("kate", "h").await.unwrap();

        let err = repo
            .update_username(other.user_id.as_str(), "jane")
            .await
            .unwrap_err();
        assert!(matches!(err, DbError::Conflict(_)));
    }

    #[tokio::test]
    async fn update_last_login_sets_timestamp() {
        let (repo, _db) = setup().await;
        let user = repo.create_user("leo", "h").await.unwrap();
        assert!(user.last_login.is_none());

        repo.update_last_login(user.user_id.as_str()).await.unwrap();

        let updated = repo.find_by_id(user.user_id.as_str()).await.unwrap().unwrap();
        assert!(updated.last_login.is_some());
        assert!(updated.last_login.unwrap() > 0);
    }

    #[tokio::test]
    async fn update_jwt_secret_succeeds() {
        let (repo, _db) = setup().await;
        let user = repo.create_user("mike", "h").await.unwrap();
        assert!(user.jwt_secret.is_none());

        repo.update_jwt_secret(user.user_id.as_str(), "secret123").await.unwrap();

        let updated = repo.find_by_id(user.user_id.as_str()).await.unwrap().unwrap();
        assert_eq!(updated.jwt_secret.as_deref(), Some("secret123"));
    }

    #[tokio::test]
    async fn set_system_user_credentials_conflict_with_existing_username() {
        let (repo, _db) = setup().await;
        repo.create_user("taken", "h").await.unwrap();

        let err = repo.set_system_user_credentials("taken", "hash").await.unwrap_err();
        assert!(matches!(err, DbError::Conflict(_)));
    }

    #[tokio::test]
    async fn set_system_user_credentials_updates_fields() {
        let (repo, _db) = setup().await;

        repo.set_system_user_credentials("admin", "secure_hash").await.unwrap();

        let user = repo.get_system_user().await.unwrap().unwrap();
        assert_eq!(user.username, "admin");
        assert_eq!(user.password_hash, "secure_hash");
    }

    #[tokio::test]
    async fn set_system_user_password_if_uninitialized_preserves_username() {
        let (repo, db) = setup().await;
        let owner = crate::installation_owner_id(db.pool()).await.unwrap();

        // User renamed the system account while its password was still empty.
        repo.update_username(&owner, "bob").await.unwrap();

        // Provisioning a password must fill the password WITHOUT touching the username.
        let wrote = repo
            .set_system_user_password_if_uninitialized("provisioned_hash")
            .await
            .unwrap();
        assert!(wrote, "should write when password was empty");

        let user = repo.get_system_user().await.unwrap().unwrap();
        assert_eq!(user.username, "bob", "username must be preserved, not reset to admin");
        assert_eq!(user.password_hash, "provisioned_hash");

        // A second enable is a no-op: password already set, nothing changes.
        let wrote_again = repo
            .set_system_user_password_if_uninitialized("another_hash")
            .await
            .unwrap();
        assert!(!wrote_again, "should not overwrite an existing password");
        let user = repo.get_system_user().await.unwrap().unwrap();
        assert_eq!(user.password_hash, "provisioned_hash", "existing password must be kept");
        assert_eq!(user.username, "bob");
    }
}

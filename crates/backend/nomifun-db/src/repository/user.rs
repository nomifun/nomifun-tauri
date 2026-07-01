use crate::error::DbError;
use crate::models::User;

/// User data access abstraction.
///
/// All methods return `Result<_, DbError>` so callers can handle
/// database failures uniformly via the `DbError → AppError` conversion.
///
/// Object-safe via `async_trait` to support `Arc<dyn IUserRepository>`.
#[async_trait::async_trait]
pub trait IUserRepository: Send + Sync {
    /// Returns `true` if at least one user with a non-empty password exists.
    ///
    /// The system default user (empty password_hash) does not count.
    async fn has_users(&self) -> Result<bool, DbError>;

    /// Returns the system default user (`id = "system_default_user"`).
    async fn get_system_user(&self) -> Result<Option<User>, DbError>;

    /// Returns the primary WebUI user.
    ///
    /// Priority: system default user first, then falls back to a user named "admin".
    async fn get_primary_webui_user(&self) -> Result<Option<User>, DbError>;

    /// Updates the system default user's username and password hash.
    ///
    /// Unconditional overwrite — used by local-mode credential management
    /// (desktop). For first-run provisioning prefer
    /// [`set_system_user_credentials_if_uninitialized`](Self::set_system_user_credentials_if_uninitialized).
    async fn set_system_user_credentials(&self, username: &str, password_hash: &str) -> Result<(), DbError>;

    /// Atomically sets the system default user's credentials ONLY if it has not
    /// been initialised yet (empty / NULL `password_hash`).
    ///
    /// Returns `Ok(true)` when the credentials were written, `Ok(false)` when an
    /// admin already exists (the caller should treat this as a conflict). The
    /// `WHERE` clause is the gate, so two concurrent first-run callers can never
    /// both win — this is the race-safe primitive for first-run setup.
    async fn set_system_user_credentials_if_uninitialized(
        &self,
        username: &str,
        password_hash: &str,
    ) -> Result<bool, DbError>;

    /// Sets the system default user's password hash ONLY if it is currently
    /// empty/NULL, and NEVER touches the username.
    ///
    /// This is the desktop LAN-provisioning primitive: it must fill in a
    /// password before exposing the WebUI to the network, but must not clobber
    /// a username the user already chose (unlike
    /// [`set_system_user_credentials`](Self::set_system_user_credentials), whose
    /// SQL rewrites both columns). The `WHERE` clause is the race-safe gate, so
    /// a second concurrent enable updates 0 rows and reuses the stored password.
    ///
    /// Returns `Ok(true)` when the password was written (it was uninitialised),
    /// `Ok(false)` when a password already existed (nothing changed).
    async fn set_system_user_password_if_uninitialized(&self, password_hash: &str) -> Result<bool, DbError>;

    /// Creates a new user and returns the inserted row.
    ///
    /// Returns `DbError::Conflict` if the username already exists.
    async fn create_user(&self, username: &str, password_hash: &str) -> Result<User, DbError>;

    /// Finds a user by username.
    async fn find_by_username(&self, username: &str) -> Result<Option<User>, DbError>;

    /// Finds a user by ID.
    async fn find_by_id(&self, id: &str) -> Result<Option<User>, DbError>;

    /// Lists all users.
    async fn list_users(&self) -> Result<Vec<User>, DbError>;

    /// Returns the total number of users.
    async fn count_users(&self) -> Result<i64, DbError>;

    /// Updates a user's password hash.
    async fn update_password(&self, user_id: &str, password_hash: &str) -> Result<(), DbError>;

    /// Updates a user's username.
    ///
    /// Returns `DbError::Conflict` if the new username already exists.
    async fn update_username(&self, user_id: &str, username: &str) -> Result<(), DbError>;

    /// Updates a user's last login timestamp to the current time.
    async fn update_last_login(&self, user_id: &str) -> Result<(), DbError>;

    /// Updates a user's JWT secret.
    async fn update_jwt_secret(&self, user_id: &str, jwt_secret: &str) -> Result<(), DbError>;
}

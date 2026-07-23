use nomifun_common::{TimestampMs, UserId};
use serde::{Deserialize, Serialize};

/// Row mapping for the `users` table.
///
/// All fields match the SQLite column names and types exactly.
/// Optional fields correspond to nullable columns.
#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct User {
    pub id: i64,
    #[sqlx(try_from = "String")]
    pub user_id: UserId,
    pub username: String,
    pub email: Option<String>,
    pub password_hash: String,
    pub avatar_path: Option<String>,
    pub jwt_secret: Option<String>,
    pub created_at: TimestampMs,
    pub updated_at: TimestampMs,
    pub last_login: Option<TimestampMs>,
}

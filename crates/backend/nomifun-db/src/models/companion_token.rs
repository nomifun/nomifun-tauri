use nomifun_common::{CompanionId, TimestampMs};

/// One row of `companion_access_token`: a per-companion Remote front-door token,
/// stored only as its SHA-256 hash. `companion_id` is the logical unique owner,
/// so each companion holds at most one live token (minting again rotates it).
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct CompanionApiTokenRow {
    pub id: i64,
    #[sqlx(try_from = "String")]
    pub companion_id: CompanionId,
    pub token_hash: String,
    pub created_at: TimestampMs,
}

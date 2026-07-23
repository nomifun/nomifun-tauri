use crate::error::DbError;
use crate::models::{IdmmInterventionRow, NewIdmmInterventionRow};

/// Data access for `idmm_interventions`. Aggressive eviction lives here:
/// `insert` prunes the target down to PER_TARGET_CAP after writing.
#[async_trait::async_trait]
pub trait IIdmmInterventionRepository: Send + Sync {
    /// Insert one record with an application-issued UUIDv7 business ID and a
    /// SQLite-assigned technical ID, then prune this target to the most-recent
    /// PER_TARGET_CAP.
    async fn insert(&self, row: &NewIdmmInterventionRow) -> Result<IdmmInterventionRow, DbError>;

    /// Most-recent-first, capped at `limit`.
    async fn list_for_target(
        &self,
        user_id: &str,
        target_kind: &str,
        target_id: &str,
        limit: i64,
    ) -> Result<Vec<IdmmInterventionRow>, DbError>;

    /// Delete all records for a target (manual clear + session-delete cascade). Returns count.
    async fn delete_for_target(
        &self,
        user_id: &str,
        target_kind: &str,
        target_id: &str,
    ) -> Result<u64, DbError>;

    /// Most-recent-first across one owner's targets, capped at `limit`.
    async fn list_recent(&self, user_id: &str, limit: i64) -> Result<Vec<IdmmInterventionRow>, DbError>;

    /// Delete every record owned by one user. Returns count.
    async fn clear_all(&self, user_id: &str) -> Result<u64, DbError>;

    /// Privileged janitor operation across all owners: TTL sweep plus an
    /// independently-applied per-user hard cap. Never exposed to REST/tools.
    async fn sweep_all_owners(&self, cutoff_ms: i64, per_user_cap: i64) -> Result<u64, DbError>;
}

/// Keep only the newest 30 records per target (data is disposable).
pub const PER_TARGET_CAP: i64 = 30;
/// TTL: 48 hours.
pub const TTL_MS: i64 = 48 * 60 * 60 * 1000;
/// Per-user activity-feed backstop.
pub const PER_USER_ACTIVITY_CAP: i64 = 2000;

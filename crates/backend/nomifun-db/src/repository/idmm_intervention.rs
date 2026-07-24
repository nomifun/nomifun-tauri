use crate::error::DbError;
use crate::models::{
    IdmmActionReservationRow, IdmmInterventionRow, NewIdmmInterventionRow,
};

/// Stable identity of one canonical IDMM action against one exact live
/// Conversation turn.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IdmmActionReservationKey {
    pub user_id: String,
    pub conversation_id: String,
    /// Stable public UUIDv7 (`AgentTurnHandle::wire_turn_id`).
    pub turn_id: String,
    /// Process-local exact active-turn generation.
    pub turn_generation: u64,
    /// Lowercase SHA-256 hex of the normalized signal + final action.
    pub action_identity: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReserveIdmmActionParams {
    pub key: IdmmActionReservationKey,
    pub reserved_at: i64,
}

/// Identity used to inspect or conservatively recover every unresolved action
/// belonging to one exact turn.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IdmmActionTurnIdentity {
    pub user_id: String,
    pub conversation_id: String,
    pub turn_id: String,
    pub turn_generation: u64,
}

/// Result of exact-action admission. Every variant except `Reserved` is
/// absorbing: the caller must not execute the action again.
#[derive(Debug, Clone, PartialEq)]
pub enum IdmmActionReserveResult {
    Reserved(IdmmActionReservationRow),
    AlreadyReserved(IdmmActionReservationRow),
    Completed(IdmmActionReservationRow),
}

impl IdmmActionReserveResult {
    pub fn reservation(&self) -> &IdmmActionReservationRow {
        match self {
            Self::Reserved(row) | Self::AlreadyReserved(row) | Self::Completed(row) => row,
        }
    }
}

/// Monotonic terminal settlement. Recovery is represented as `failed` in
/// storage, but its distinct source preserves crash ambiguity for audit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IdmmActionSettlement {
    Applied,
    Failed { reason: String },
    Recovered { reason: String },
}

#[derive(Debug, Clone, PartialEq)]
pub enum IdmmActionSettleResult {
    Settled(IdmmActionReservationRow),
    AlreadySettled(IdmmActionReservationRow),
}

impl IdmmActionSettleResult {
    pub fn reservation(&self) -> &IdmmActionReservationRow {
        match self {
            Self::Settled(row) | Self::AlreadySettled(row) => row,
        }
    }
}

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

    /// Atomically reserve one action. A durable duplicate is returned as an
    /// absorbing result rather than a uniqueness error. Implementations must
    /// fail closed on every persistence error.
    async fn reserve_action(
        &self,
        _params: &ReserveIdmmActionParams,
    ) -> Result<IdmmActionReserveResult, DbError> {
        Err(DbError::Init(
            "durable IDMM action reservations are unsupported by this repository".to_owned(),
        ))
    }

    /// Settle a reservation exactly once. A later or contradictory settlement
    /// is absorbed and returns the already-committed terminal row.
    async fn settle_action(
        &self,
        _key: &IdmmActionReservationKey,
        _settlement: &IdmmActionSettlement,
        _settled_at: i64,
    ) -> Result<IdmmActionSettleResult, DbError> {
        Err(DbError::Init(
            "durable IDMM action reservations are unsupported by this repository".to_owned(),
        ))
    }

    /// Read unresolved reservations for exact-turn crash recovery.
    async fn list_reserved_actions_for_turn(
        &self,
        _turn: &IdmmActionTurnIdentity,
    ) -> Result<Vec<IdmmActionReservationRow>, DbError> {
        Err(DbError::Init(
            "durable IDMM action reservations are unsupported by this repository".to_owned(),
        ))
    }

    /// Conservatively absorb every unresolved action for one exact turn after
    /// recovery. No action side effect is re-driven.
    async fn recover_reserved_actions_for_turn(
        &self,
        _turn: &IdmmActionTurnIdentity,
        _reason: &str,
        _settled_at: i64,
    ) -> Result<Vec<IdmmActionReservationRow>, DbError> {
        Err(DbError::Init(
            "durable IDMM action reservations are unsupported by this repository".to_owned(),
        ))
    }
}

/// Keep only the newest 30 records per target (data is disposable).
pub const PER_TARGET_CAP: i64 = 30;
/// TTL: 48 hours.
pub const TTL_MS: i64 = 48 * 60 * 60 * 1000;
/// Per-user activity-feed backstop.
pub const PER_USER_ACTIVITY_CAP: i64 = 2000;
/// Bounds recovery/failure diagnostics without truncating silently.
pub const MAX_IDMM_ACTION_FAILURE_REASON_CHARS: usize = 2000;

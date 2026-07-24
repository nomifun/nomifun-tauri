use serde::{Deserialize, Serialize};

/// Row in `idmm_interventions` — one persisted IDMM decision (the "思路"/audit
/// trail). Aggressively evicted: per-target cap + shared TTL. `target_id` is a
/// polymorphic locator interpreted together with `target_kind`; application
/// code owns its cleanup policy.
#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct IdmmInterventionRow {
    /// SQLite-local technical key used only for deterministic ordering and
    /// retention queries. It must never cross the repository boundary as an
    /// entity identifier.
    pub id: i64,
    /// Product-facing bare UUIDv7 business identity.
    pub intervention_id: String,
    /// Authenticated owner resolved from the supervised session before the
    /// intervention is persisted. Activity feeds are always partitioned by it.
    pub user_id: String,
    pub target_kind: String,
    pub target_id: String,
    pub watch: String,
    pub at: i64,
    pub signal: String,
    pub tier_used: String,
    pub category: Option<String>,
    pub action: String,
    pub detail: Option<String>,
    pub reason: Option<String>,
    pub confidence: Option<f64>,
    pub bypass_model: Option<String>,
    pub outcome: String,
}

/// New IDMM audit row. The application allocates `intervention_id`; SQLite
/// independently owns the technical autoincrement `id`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NewIdmmInterventionRow {
    pub intervention_id: String,
    /// Authenticated owner resolved from the supervised session before the
    /// intervention is persisted. Activity feeds are always partitioned by it.
    pub user_id: String,
    pub target_kind: String,
    pub target_id: String,
    pub watch: String,
    pub at: i64,
    pub signal: String,
    pub tier_used: String,
    pub category: Option<String>,
    pub action: String,
    pub detail: Option<String>,
    pub reason: Option<String>,
    pub confidence: Option<f64>,
    pub bypass_model: Option<String>,
    pub outcome: String,
}

/// Durable admission record for one canonical IDMM action against one exact
/// live Conversation turn. Unlike [`IdmmInterventionRow`], these rows are not
/// audit-feed entries and are never subject to IDMM log TTL/cap eviction.
#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow, PartialEq)]
pub struct IdmmActionReservationRow {
    /// SQLite-local ordering key. It must not be used as the action identity.
    pub id: i64,
    /// Product-facing bare UUIDv7 reservation identity.
    pub reservation_id: String,
    /// Authenticated Conversation owner at reservation time.
    pub user_id: String,
    pub conversation_id: String,
    /// Stable public UUIDv7 identity of the exact active turn.
    pub turn_id: String,
    /// Process-local active-turn generation, checked into SQLite's i64 range.
    pub turn_generation: i64,
    /// Canonical SHA-256 identity of the normalized signal + final action.
    pub action_identity: String,
    /// `reserved` | `applied` | `failed`.
    pub status: String,
    /// `execution` for normal settlement, `recovery` when crash ambiguity was
    /// conservatively absorbed. Null while reserved.
    pub settlement_source: Option<String>,
    /// Present only for `failed`; recovery ambiguity is recorded here too.
    pub failure_reason: Option<String>,
    pub reserved_at: i64,
    pub settled_at: Option<i64>,
}

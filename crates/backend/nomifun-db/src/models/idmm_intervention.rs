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

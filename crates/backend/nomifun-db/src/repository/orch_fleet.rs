use crate::models::{FleetMemberRow, FleetRow};

/// Parameters for creating a new fleet. The `id`/`created_at`/`updated_at`
/// columns are minted by the repository.
pub struct CreateFleetParams {
    pub user_id: String,
    pub name: String,
    pub description: Option<String>,
    pub max_parallel: Option<i64>,
}

/// Parameters for a partial fleet update. `None` = leave the column unchanged.
/// For the nullable columns, the nesting distinguishes "skip" from "set NULL":
/// `None` = skip, `Some(None)` = set NULL, `Some(Some(v))` = set `v`.
pub struct UpdateFleetParams {
    pub name: Option<String>,
    pub description: Option<Option<String>>,
    pub max_parallel: Option<Option<i64>>,
}

/// A fleet member to enroll via [`IFleetRepository::replace_members`]. The
/// `id`/`fleet_id`/timestamps are minted/filled by the repository.
pub struct NewFleetMember {
    pub agent_id: String,
    pub provider_id: Option<String>,
    pub model: Option<String>,
    pub role_hint: Option<String>,
    pub capability_profile: Option<String>,
    pub constraints: Option<String>,
    pub sort_order: i64,
}

/// Data access abstraction for the `fleets` + `fleet_members` tables.
///
/// A fleet is a per-user named group of agents available for orchestration.
/// Membership is edited wholesale: [`replace_members`](Self::replace_members)
/// swaps the entire member set in a single transaction.
#[async_trait::async_trait]
pub trait IFleetRepository: Send + Sync {
    /// Mint and insert a new fleet (`generate_prefixed_id("fleet")`), returning
    /// the created row.
    async fn create_fleet(&self, p: CreateFleetParams) -> Result<FleetRow, sqlx::Error>;

    /// Return all fleets owned by `user_id`, newest first.
    async fn list_fleets(&self, user_id: &str) -> Result<Vec<FleetRow>, sqlx::Error>;

    /// Return a single fleet by id, or `None`.
    async fn get_fleet(&self, id: &str) -> Result<Option<FleetRow>, sqlx::Error>;

    /// Apply a partial update (see [`UpdateFleetParams`]). No-op when every
    /// field is `None`. Bumps `updated_at` whenever any column changes.
    async fn update_fleet(&self, id: &str, p: UpdateFleetParams) -> Result<(), sqlx::Error>;

    /// Delete a fleet by id. Members are removed by `ON DELETE CASCADE`.
    async fn delete_fleet(&self, id: &str) -> Result<(), sqlx::Error>;

    /// Return a fleet's members ordered by `sort_order` ascending.
    async fn list_members(&self, fleet_id: &str) -> Result<Vec<FleetMemberRow>, sqlx::Error>;

    /// Atomically replace a fleet's entire member set: delete existing members,
    /// then batch-insert `members` (each minting `generate_prefixed_id("fmem")`).
    async fn replace_members(
        &self,
        fleet_id: &str,
        members: Vec<NewFleetMember>,
    ) -> Result<(), sqlx::Error>;
}

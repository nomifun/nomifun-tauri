use crate::models::OrchWorkspaceRow;

/// Parameters for creating a new orchestration workspace. The `id`/`created_at`/
/// `updated_at` columns are minted by the repository.
pub struct CreateOrchWorkspaceParams {
    pub user_id: String,
    pub name: String,
    pub default_fleet_id: Option<String>,
    pub workspace_dir: Option<String>,
    pub context: Option<String>,
}

/// Parameters for a partial workspace update. `None` = leave the column unchanged.
/// For the nullable columns, the nesting distinguishes "skip" from "set NULL":
/// `None` = skip, `Some(None)` = set NULL, `Some(Some(v))` = set `v`.
pub struct UpdateOrchWorkspaceParams {
    pub name: Option<String>,
    pub default_fleet_id: Option<Option<String>>,
    pub workspace_dir: Option<Option<String>>,
    pub context: Option<Option<String>>,
}

/// Data access abstraction for the `orch_workspaces` table.
///
/// A workspace is a per-user named scope for orchestration runs, optionally
/// anchored to a default fleet and a working directory.
#[async_trait::async_trait]
pub trait IOrchWorkspaceRepository: Send + Sync {
    /// Mint and insert a new workspace (`generate_prefixed_id("ows")`), returning
    /// the created row.
    async fn create(&self, p: CreateOrchWorkspaceParams) -> Result<OrchWorkspaceRow, sqlx::Error>;

    /// Return all workspaces owned by `user_id`, newest first.
    async fn list(&self, user_id: &str) -> Result<Vec<OrchWorkspaceRow>, sqlx::Error>;

    /// Return a single workspace by id, or `None`.
    async fn get(&self, id: &str) -> Result<Option<OrchWorkspaceRow>, sqlx::Error>;

    /// Apply a partial update (see [`UpdateOrchWorkspaceParams`]). No-op when
    /// every field is `None`. Bumps `updated_at` whenever any column changes.
    async fn update(&self, id: &str, p: UpdateOrchWorkspaceParams) -> Result<(), sqlx::Error>;

    /// Delete a workspace by id.
    async fn delete(&self, id: &str) -> Result<(), sqlx::Error>;
}

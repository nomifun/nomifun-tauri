use crate::error::DbError;
use crate::models::CreationTaskRow;

/// Data access for the `creation_tasks` table (生成引擎 任务队列 状态机).
///
/// The `nomifun-creation` service owns the state machine; this repo is the
/// persistence seam. `params` / `error` / `result_asset_ids` are pre-serialized
/// JSON strings the caller builds.
#[async_trait::async_trait]
pub trait ICreationTaskRepository: Send + Sync {
    /// Insert a task (typically `status = "queued"`).
    async fn create_task(&self, params: CreateCreationTaskParams<'_>) -> Result<CreationTaskRow, DbError>;

    /// One task by id, or `None`.
    async fn get_task(&self, id: &str) -> Result<Option<CreationTaskRow>, DbError>;

    /// Filtered listing (optional canvas / status), newest-submitted first,
    /// capped by `limit`.
    async fn list_tasks(&self, params: ListCreationTasksParams<'_>) -> Result<Vec<CreationTaskRow>, DbError>;

    /// Partial state-machine update. `DbError::NotFound` when the id is unknown.
    async fn update_task(&self, id: &str, params: UpdateCreationTaskParams<'_>) -> Result<CreationTaskRow, DbError>;

    /// Conditional terminal-state write: apply `params` ONLY if the task is
    /// still live (`status IN ('queued','running')`). Returns `Ok(true)` when
    /// the row was updated, `Ok(false)` when the task was no longer live (e.g.
    /// already `canceled`) or unknown. Unlike [`Self::update_task`] this never
    /// overwrites a terminal status — the worker's finalize routes through it so
    /// a `cancel` that lands mid-finalize is not silently flipped to
    /// `succeeded`/`failed` (compare-and-set on `status`, not the token).
    async fn update_task_if_live(&self, id: &str, params: UpdateCreationTaskParams<'_>) -> Result<bool, DbError>;

    /// Every task currently in a live (`queued`/`running`) state — the boot
    /// reconciliation input.
    async fn list_live_tasks(&self) -> Result<Vec<CreationTaskRow>, DbError>;
}

/// Params for [`ICreationTaskRepository::create_task`]. `id` / `submitted_at`
/// are caller-supplied so the service controls minting + clock.
#[derive(Debug)]
pub struct CreateCreationTaskParams<'a> {
    pub id: &'a str,
    pub canvas_id: Option<&'a str>,
    pub node_id: Option<&'a str>,
    pub provider_id: &'a str,
    pub model: &'a str,
    pub capability: &'a str,
    /// JSON parameter snapshot.
    pub params: &'a str,
    pub status: &'a str,
    pub submitted_at: i64,
}

/// Filters for [`ICreationTaskRepository::list_tasks`].
#[derive(Debug, Default)]
pub struct ListCreationTasksParams<'a> {
    pub canvas_id: Option<&'a str>,
    pub status: Option<&'a str>,
    /// Max rows (clamped by the caller).
    pub limit: i64,
}

/// Partial-update params for [`ICreationTaskRepository::update_task`]. Each
/// `Some` replaces the field; `None` keeps the current value. Inner `Option`
/// (for nullable columns) distinguishes "set to NULL" from "keep".
#[derive(Debug, Default)]
pub struct UpdateCreationTaskParams<'a> {
    pub status: Option<&'a str>,
    pub error: Option<Option<&'a str>>,
    /// Replacement JSON array string of result asset ids.
    pub result_asset_ids: Option<&'a str>,
    pub remote_task_id: Option<Option<&'a str>>,
    pub attempt: Option<i64>,
    pub started_at: Option<Option<i64>>,
    pub finished_at: Option<Option<i64>>,
}

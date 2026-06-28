use crate::models::{OrchAssignmentRow, OrchRunRow, OrchRunTaskDepRow, OrchRunTaskRow};

/// Parameters for creating a new orchestration run. `id`/`created_at`/
/// `updated_at` are minted by the repository (`generate_prefixed_id("run")`).
/// `status` is supplied by the service (e.g. `"planning"`).
pub struct CreateRunParams {
    /// Owning workspace, or `None` for an ad-hoc run created from a conversation
    /// (which carries its own `work_dir` instead).
    pub workspace_id: Option<String>,
    pub user_id: String,
    pub goal: String,
    pub fleet_snapshot: String, // JSON
    pub autonomy: String,
    pub max_parallel: Option<i64>,
    /// Lead/coordinator worker conversation — local `conversations.id` INTEGER.
    /// `create_adhoc` needs to write this at creation time.
    pub lead_conv_id: Option<i64>,
    /// Working directory for an ad-hoc (workspace-less) run.
    pub work_dir: Option<String>,
}

/// Parameters for a partial run update. `None` = leave the column unchanged.
/// For the nullable columns, the nesting distinguishes "skip" from "set NULL":
/// `None` = skip, `Some(None)` = set NULL, `Some(Some(v))` = set `v`.
/// `status` is a plain non-null column: `None` = skip, `Some(v)` = set `v`.
pub struct UpdateRunParams {
    pub status: Option<String>,
    pub summary: Option<Option<String>>,
    pub lead_conv_id: Option<Option<i64>>,
    pub total_tokens: Option<Option<i64>>,
    /// Run goal (rename). `goal` is a plain `NOT NULL` column, so it uses the
    /// single-`Option` skip/set encoding: `None` = skip, `Some(v)` = set `v`.
    pub goal: Option<String>,
    /// Autonomy (replan may switch the gate). `NOT NULL` column → single-`Option`
    /// skip/set encoding.
    pub autonomy: Option<String>,
    /// Fleet snapshot JSON (replan rebuilds it from a new model range). `NOT NULL`
    /// column → single-`Option` skip/set encoding.
    pub fleet_snapshot: Option<String>,
}

/// Parameters for creating a task within a run. `id` is minted
/// (`generate_prefixed_id("rtask")`); `attempt` starts at 0.
/// `status` is supplied by the service (e.g. `"pending"`).
pub struct CreateTaskParams {
    pub run_id: String,
    pub title: String,
    pub spec: String,
    pub task_profile: Option<String>, // JSON
    pub status: String,
    pub graph_x: Option<f64>,
    pub graph_y: Option<f64>,
    /// Short Chinese role the planner named for this task (P5 沉淀捕获). Nullable.
    pub role: Option<String>,
}

/// Parameters for a partial task update. `None` = leave the column unchanged.
/// Nullable columns use the double-`Option` skip/NULL/set encoding;
/// `status`/`attempt`/`graph_x`/`graph_y` are plain skip-on-`None`.
pub struct UpdateTaskParams {
    pub status: Option<String>,
    pub conversation_id: Option<Option<i64>>,
    pub output_summary: Option<Option<String>>,
    pub output_files: Option<Option<String>>,
    pub attempt: Option<i64>,
    pub tokens: Option<Option<i64>>,
    pub graph_x: Option<f64>,
    pub graph_y: Option<f64>,
}

/// Parameters for creating an assignment (member → task). `id` is minted
/// (`generate_prefixed_id("asg")`); `created_at` is filled by the repository.
pub struct CreateAssignmentParams {
    pub task_id: String,
    pub member_id: String,
    pub score: Option<f64>,
    pub rationale: Option<String>,
    pub source: String,
    pub locked: bool,
}

/// Data access abstraction for one orchestration-run aggregate: the
/// `orch_runs` + `orch_run_tasks` + `orch_run_task_deps` + `orch_assignments`
/// tables. A run owns a task DAG; deps are `blocker → blocked` edges; ready
/// tasks are derived (never mutated) from task status + dep completion.
#[async_trait::async_trait]
pub trait IRunRepository: Send + Sync {
    // --- runs ---

    /// Mint and insert a new run (`generate_prefixed_id("run")`), returning the
    /// created row.
    async fn create_run(&self, p: CreateRunParams) -> Result<OrchRunRow, sqlx::Error>;

    /// Return a single run by id, or `None`.
    async fn get_run(&self, id: &str) -> Result<Option<OrchRunRow>, sqlx::Error>;

    /// Return all runs in a workspace, newest first.
    async fn list_runs(&self, workspace_id: &str) -> Result<Vec<OrchRunRow>, sqlx::Error>;

    /// Return all runs owned by a user, newest first — across every workspace AND
    /// ad-hoc (workspace_id=NULL) runs. This is the read path for the read-only
    /// Run-history library (the repurposed orchestrator tab); ad-hoc runs created
    /// straight from a conversation carry no workspace, so they only surface here,
    /// never under the workspace-scoped [`list_runs`](Self::list_runs).
    async fn list_runs_by_user(&self, user_id: &str) -> Result<Vec<OrchRunRow>, sqlx::Error>;

    /// Return all runs in a given status across all workspaces (boot-resume).
    async fn list_runs_by_status(&self, status: &str) -> Result<Vec<OrchRunRow>, sqlx::Error>;

    /// Apply a partial run update (see [`UpdateRunParams`]). No-op when every
    /// field is `None`. Bumps `updated_at` whenever any column changes.
    async fn update_run(&self, id: &str, p: UpdateRunParams) -> Result<(), sqlx::Error>;

    /// Delete a run. The schema's `ON DELETE CASCADE` foreign keys (migration
    /// 018) sweep the whole aggregate out with it: the run's tasks
    /// (`orch_run_tasks.run_id`) and, via the task ids, that run's dependency
    /// edges (`orch_run_task_deps`) and assignments (`orch_assignments`). One
    /// `DELETE FROM orch_runs WHERE id = ?` is enough; no manual child cleanup.
    /// Requires `PRAGMA foreign_keys=ON` on the connection (the project default).
    async fn delete_run(&self, id: &str) -> Result<(), sqlx::Error>;

    // --- tasks ---

    /// Mint and insert a new task (`generate_prefixed_id("rtask")`), returning
    /// the created row (`attempt` = 0).
    async fn create_task(&self, p: CreateTaskParams) -> Result<OrchRunTaskRow, sqlx::Error>;

    /// Return all tasks in a run, oldest first.
    async fn list_tasks(&self, run_id: &str) -> Result<Vec<OrchRunTaskRow>, sqlx::Error>;

    /// Return a single task by id, or `None`.
    async fn get_task(&self, id: &str) -> Result<Option<OrchRunTaskRow>, sqlx::Error>;

    /// Apply a partial task update (see [`UpdateTaskParams`]). No-op when every
    /// field is `None`. Bumps `updated_at` whenever any column changes.
    async fn update_task(&self, id: &str, p: UpdateTaskParams) -> Result<(), sqlx::Error>;

    /// Delete ALL of a run's tasks (`DELETE FROM orch_run_tasks WHERE run_id = ?`),
    /// leaving the `orch_runs` row intact. The task-keyed `ON DELETE CASCADE` FKs
    /// (migration 018) sweep out that run's dependency edges (`orch_run_task_deps`)
    /// and assignments (`orch_assignments`) with the tasks. This is the replan
    /// "clear old plan" step: a clean re-decomposition wipes the prior task DAG so
    /// `plan` (which mints fresh tasks every call) re-plans rather than appends.
    /// Requires `PRAGMA foreign_keys=ON` on the connection (the project default).
    async fn clear_run_tasks(&self, run_id: &str) -> Result<(), sqlx::Error>;

    // --- deps ---

    /// Insert a `blocker → blocked` dependency edge into the task DAG.
    async fn add_dep(&self, blocker: &str, blocked: &str) -> Result<(), sqlx::Error>;

    /// Return all dependency edges for tasks belonging to a run.
    async fn list_deps(&self, run_id: &str) -> Result<Vec<OrchRunTaskDepRow>, sqlx::Error>;

    /// Return the run's currently-runnable tasks: `status == 'pending'` AND
    /// every blocker task is `'done'` (a task with zero deps is ready).
    /// Unblocking is modeled as a re-query — after a blocker → `done`, this
    /// returns the newly-unblocked tasks; dep edges are never deleted.
    async fn list_ready_tasks(&self, run_id: &str) -> Result<Vec<OrchRunTaskRow>, sqlx::Error>;

    // --- assignments ---

    /// Mint and insert a new assignment (`generate_prefixed_id("asg")`),
    /// returning the created row.
    async fn create_assignment(
        &self,
        p: CreateAssignmentParams,
    ) -> Result<OrchAssignmentRow, sqlx::Error>;

    /// Replace a task's assignment (override/lock path). Deletes any existing
    /// assignment rows for the task, then inserts a fresh one with the given
    /// member/source/locked. Used by `reassign` — a human override is a clean
    /// single-assignment replacement, not an additive row. Returns the new row.
    async fn set_assignment(
        &self,
        p: CreateAssignmentParams,
    ) -> Result<OrchAssignmentRow, sqlx::Error>;

    /// Return all assignments for tasks belonging to a run.
    async fn list_assignments(&self, run_id: &str) -> Result<Vec<OrchAssignmentRow>, sqlx::Error>;

    /// Return the latest assignment for a task, or `None`.
    async fn get_assignment_for_task(
        &self,
        task_id: &str,
    ) -> Result<Option<OrchAssignmentRow>, sqlx::Error>;
}

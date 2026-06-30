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
    /// Task mode (ultracode 模式增强, 迁移 023). `"agent"`(默认现状)|
    /// `"synthesis"`。The service passes the planner's `kind` here; an empty/legacy
    /// plan yields `"agent"`.
    pub kind: String,
    /// Optional per-kind config JSON (迁移 023, nullable), e.g. fan-out 分组
    /// `{"group":"<label>"}`。`None` when unused.
    pub pattern_config: Option<String>, // JSON
}

/// Parameters for a partial task update. `None` = leave the column unchanged.
/// Nullable columns use the double-`Option` skip/NULL/set encoding;
/// `status`/`attempt`/`graph_x`/`graph_y` are plain skip-on-`None`.
#[derive(Default)]
pub struct UpdateTaskParams {
    pub status: Option<String>,
    /// New task spec (the prompt/intent the worker brief is built from). Plain
    /// skip-on-`None` (the `spec` column is `NOT NULL`). Used by the per-node
    /// "意图/prompt 微调" path (`RunService::update_task_spec`) so a user can amend
    /// a node's intent before re-running it.
    pub spec: Option<String>,
    pub conversation_id: Option<Option<i64>>,
    pub output_summary: Option<Option<String>>,
    pub output_files: Option<Option<String>>,
    pub attempt: Option<i64>,
    pub tokens: Option<Option<i64>>,
    pub graph_x: Option<f64>,
    pub graph_y: Option<f64>,
    /// Nullable per-kind config JSON (skip/NULL/set). Used by the `loop` body
    /// re-run to carry the prior round's output forward (see the orchestrator
    /// engine's `settle_loop_task`).
    pub pattern_config: Option<Option<String>>,
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

/// A dependency reference inside a [`ReconcilePlan`], resolved INSIDE the
/// transaction once the NEW tasks have been minted. Mirrors the service-layer
/// `AdjustedDepRef` but lives here so the trait never sees orchestrator types:
/// a `Kept` id resolves directly; a `NewIndex(i)` resolves to the i-th minted
/// new task. The service validates ranges + acyclicity BEFORE handing the plan
/// to the repo, so the repo only resolves — it never re-validates the graph.
pub enum ReconcileDepRef {
    /// An existing task id the adjusted plan KEPT (survives the reconcile).
    Kept(String),
    /// A 0-based index into the [`ReconcilePlan::new_tasks`] vec (a freshly
    /// minted task in THIS reconcile).
    NewIndex(usize),
}

/// One NEW task to insert during a [`reconcile_run_plan`](IRunRepository::reconcile_run_plan):
/// the create params (sans the still-unminted task id), an OPTIONAL resolved
/// auto-assignment (the routing decision the service computed in memory — its
/// `task_id` is ignored/overwritten with the freshly minted id inside the tx),
/// and the dep refs naming this task's blockers (kept ids and/or earlier
/// new-task indices). The service resolves the routing pick BEFORE the tx so the
/// repo only persists — no orchestrator routing logic leaks into the db layer.
pub struct ReconcileNewTask {
    /// Create params for the task. `run_id` is filled by the repo from the
    /// reconcile's `run_id`; the minted id is returned implicitly (used to
    /// resolve `NewIndex` dep refs that point at this task).
    pub task: CreateTaskParams,
    /// The pre-computed auto-assignment for this task, or `None` to skip
    /// assigning it (e.g. an empty fleet snapshot — the engine will fail it).
    pub assignment: Option<CreateAssignmentParams>,
    /// The blockers this task depends on (resolved to ids inside the tx).
    pub depends_on: Vec<ReconcileDepRef>,
}

/// A fully-resolved conversational-reconcile plan, computed IN MEMORY by the
/// service ([`RunService::adjust`]) and applied ATOMICALLY by
/// [`reconcile_run_plan`](IRunRepository::reconcile_run_plan). The service does
/// ALL judgment first — validates kept ids exist, decides the routing pick per
/// new task, and proves the final (kept + new) graph is ACYCLIC — then hands this
/// over so the repo's only job is to persist it in one transaction (all-or-nothing).
pub struct ReconcilePlan {
    /// Existing task ids the adjusted plan did NOT keep → delete (cascading each
    /// task's deps + assignment). KEPT tasks are absent here and untouched.
    pub delete_task_ids: Vec<String>,
    /// The NEW tasks to insert (pending) + route. Order is significant: a
    /// `ReconcileDepRef::NewIndex(i)` resolves to `new_tasks[i]`'s minted id.
    pub new_tasks: Vec<ReconcileNewTask>,
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

    /// Delete ONE task (`DELETE FROM orch_run_tasks WHERE id = ?`). The task-keyed
    /// `ON DELETE CASCADE` FKs (migration 018) sweep out that task's dependency
    /// edges (`orch_run_task_deps`, where the task is blocker OR blocked) and its
    /// assignment (`orch_assignments`). This is the conversational-reconcile "drop
    /// an unkept task" step — unlike [`clear_run_tasks`](Self::clear_run_tasks) it
    /// removes a SINGLE task so the run's KEPT tasks (and their completed output)
    /// survive untouched. Requires `PRAGMA foreign_keys=ON` (project default).
    async fn delete_task(&self, id: &str) -> Result<(), sqlx::Error>;

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

    /// Delete ALL dependency edges of a run, leaving its tasks intact. Scoped to
    /// the run by joining each edge's blocked task back to its `run_id` (so other
    /// runs' edges are untouched). This is the conversational-reconcile "rebuild
    /// the DAG" step: clear the run's edges, then re-wire them from the adjusted
    /// plan (kept + new tasks) — the KEPT tasks' rows (status/output/assignment)
    /// survive, only their wiring is replaced.
    async fn clear_run_deps(&self, run_id: &str) -> Result<(), sqlx::Error>;

    /// Return all dependency edges for tasks belonging to a run.
    async fn list_deps(&self, run_id: &str) -> Result<Vec<OrchRunTaskDepRow>, sqlx::Error>;

    /// Apply a fully-resolved conversational-reconcile plan ATOMICALLY (UC-3a 评审
    /// Important-A): in ONE transaction, (1) clear the run's dep edges, (2) delete
    /// every un-kept task (cascading its deps + assignment), (3) insert each NEW
    /// task (pending) + its pre-computed assignment, and (4) rebuild the dep edges
    /// from the plan (resolving `NewIndex` refs to the freshly minted ids). The
    /// whole sequence is `pool.begin()…commit()`: a mid-way error rolls the WHOLE
    /// thing back, so a DB failure leaves the run EXACTLY as it was (no durable
    /// half-state). The service ([`RunService::adjust`]) does all judgment first —
    /// validates kept ids, decides routing, proves acyclicity — so the repo only
    /// persists. KEPT tasks (absent from `delete_task_ids`) and their completed
    /// output/conversation/assignment are untouched. Requires `PRAGMA
    /// foreign_keys=ON` (project default) for the delete cascades. A self-edge
    /// (blocker == blocked) is skipped (the table CHECKs blocker <> blocked).
    async fn reconcile_run_plan(
        &self,
        run_id: &str,
        plan: ReconcilePlan,
    ) -> Result<(), sqlx::Error>;


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

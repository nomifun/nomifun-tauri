//! [`RunService`]: create / plan / inspect / cancel orchestration runs.
//!
//! The service owns the *control-plane* of a run — everything that happens
//! synchronously around the [`RunEngine`](crate::engine::RunEngine) execution
//! loop:
//!
//! - [`RunService::create`] snapshots the chosen fleet into the run row (so the
//!   run is reproducible even if the fleet is later edited/deleted) and parks the
//!   run in `planning`.
//! - [`RunService::plan`] loads the run + its fleet snapshot, asks the
//!   [`PlanProducer`](crate::plan::PlanProducer) to decompose the goal into a
//!   [`PlannedDag`], then persists the tasks (status `pending`), the
//!   `depends_on` edges (planned index → minted task id), and the
//!   `member_index` → member-id assignments (`source = "auto"`). It emits
//!   `run.planUpdated` and flips the run to `running` — at which point the engine
//!   may pick it up.
//! - [`RunService::get_detail`] / [`RunService::list`] are the read paths
//!   (Row↔DTO mapping, JSON-as-TEXT decode of `task_profile` / `output_files`).
//! - [`RunService::cancel`] flips the run to `cancelled` and emits.
//!
//! Row↔DTO mapping note: `output_summary` is PROSE pass-through (the column is a
//! plain `Option<String>`, *not* JSON, despite a misleading Row comment), while
//! `output_files` is a JSON `Vec<String>` (decoded fail-soft to an empty vec).

use std::sync::Arc;

use nomifun_api_types::{
    Assignment, CreateRunRequest, FleetMember, PlannedDag, Run, RunDetail, RunTask, RunTaskDep,
    TaskProfile,
};
use nomifun_common::AppError;
use nomifun_db::models::{
    FleetMemberRow, OrchAssignmentRow, OrchRunRow, OrchRunTaskDepRow, OrchRunTaskRow,
};
use nomifun_db::{
    CreateAssignmentParams, CreateRunParams, CreateTaskParams, IFleetRepository,
    IOrchWorkspaceRepository, IRunRepository, UpdateRunParams,
};

use crate::error::OrchestratorError;
use crate::events::OrchestratorRunEventEmitter;
use crate::plan::PlanProducer;

/// Default autonomy when the create request omits it.
const DEFAULT_AUTONOMY: &str = "supervised";

#[derive(Clone)]
pub struct RunService {
    run_repo: Arc<dyn IRunRepository>,
    fleet_repo: Arc<dyn IFleetRepository>,
    ws_repo: Arc<dyn IOrchWorkspaceRepository>,
    planner: Arc<dyn PlanProducer>,
    emitter: OrchestratorRunEventEmitter,
}

impl RunService {
    pub fn new(
        run_repo: Arc<dyn IRunRepository>,
        fleet_repo: Arc<dyn IFleetRepository>,
        ws_repo: Arc<dyn IOrchWorkspaceRepository>,
        planner: Arc<dyn PlanProducer>,
        emitter: OrchestratorRunEventEmitter,
    ) -> Self {
        Self {
            run_repo,
            fleet_repo,
            ws_repo,
            planner,
            emitter,
        }
    }

    /// Create a run: snapshot the chosen fleet's members into the run row and
    /// park it in `planning`. The snapshot makes the run reproducible even if
    /// the fleet is later edited or deleted (we never re-read `fleets` after
    /// this point — the engine resolves members from `fleet_snapshot`).
    pub async fn create(&self, user_id: &str, req: CreateRunRequest) -> Result<Run, AppError> {
        if req.goal.trim().is_empty() {
            return Err(OrchestratorError::BadRequest("goal must not be empty".into()).into());
        }
        // Confirm the workspace exists (clean 404 vs a later FK failure).
        if self
            .ws_repo
            .get(&req.workspace_id)
            .await
            .map_err(OrchestratorError::from)?
            .is_none()
        {
            return Err(OrchestratorError::NotFound(format!("workspace {}", req.workspace_id)).into());
        }
        // Load + snapshot the fleet's members.
        let members = self.load_fleet_members(&req.fleet_id).await?;
        let fleet_snapshot =
            serde_json::to_string(&members).unwrap_or_else(|_| "[]".to_string());

        let autonomy = req
            .autonomy
            .filter(|a| !a.trim().is_empty())
            .unwrap_or_else(|| DEFAULT_AUTONOMY.to_string());

        let row = self
            .run_repo
            .create_run(CreateRunParams {
                workspace_id: req.workspace_id,
                user_id: user_id.to_string(),
                goal: req.goal,
                fleet_snapshot,
                autonomy,
                max_parallel: req.max_parallel,
            })
            .await
            .map_err(OrchestratorError::from)?;

        let run = run_row_to_dto(row);
        // Status starts at `planning` (the repo INSERTs it); surface it on the bus.
        self.emitter.emit_run_status(&run.id, &run.status);
        Ok(run)
    }

    /// Full run detail: the run + its task DAG (tasks, dep edges, assignments).
    pub async fn get_detail(&self, id: &str) -> Result<RunDetail, AppError> {
        let row = self
            .run_repo
            .get_run(id)
            .await
            .map_err(OrchestratorError::from)?
            .ok_or_else(|| OrchestratorError::NotFound(format!("run {id}")))?;
        let tasks = self.run_repo.list_tasks(id).await.map_err(OrchestratorError::from)?;
        let deps = self.run_repo.list_deps(id).await.map_err(OrchestratorError::from)?;
        let assignments = self
            .run_repo
            .list_assignments(id)
            .await
            .map_err(OrchestratorError::from)?;
        Ok(RunDetail {
            run: run_row_to_dto(row),
            tasks: tasks.into_iter().map(task_row_to_dto).collect(),
            deps: deps.into_iter().map(dep_row_to_dto).collect(),
            assignments: assignments.into_iter().map(assignment_row_to_dto).collect(),
        })
    }

    /// All runs in a workspace, newest first.
    pub async fn list(&self, workspace_id: &str) -> Result<Vec<Run>, AppError> {
        let rows = self
            .run_repo
            .list_runs(workspace_id)
            .await
            .map_err(OrchestratorError::from)?;
        Ok(rows.into_iter().map(run_row_to_dto).collect())
    }

    /// Plan a run: decompose the goal into a task DAG, persist tasks + deps +
    /// assignments, emit `planUpdated`, and flip the run to `running`.
    ///
    /// Edges are persisted AFTER all tasks are created so the planned `depends_on`
    /// indices can be resolved to the minted task ids. A planned task with no
    /// `member_index` (or an out-of-range one) defaults to member 0 — the engine
    /// requires an assignment to run a task, so defaulting is safer than skipping.
    pub async fn plan(&self, run_id: &str) -> Result<(), AppError> {
        let run = self
            .run_repo
            .get_run(run_id)
            .await
            .map_err(OrchestratorError::from)?
            .ok_or_else(|| OrchestratorError::NotFound(format!("run {run_id}")))?;

        let members: Vec<FleetMember> =
            serde_json::from_str(&run.fleet_snapshot).unwrap_or_default();

        let dag: PlannedDag = self.planner.produce(&run.goal, &members).await?;

        // 1. Create every task first (status `pending`); remember the minted ids
        //    in planned-index order so we can resolve dep edges + assignments.
        let mut task_ids: Vec<String> = Vec::with_capacity(dag.tasks.len());
        for planned in &dag.tasks {
            let task_profile = planned
                .task_profile
                .as_ref()
                .and_then(|p| serde_json::to_string(p).ok());
            let task = self
                .run_repo
                .create_task(CreateTaskParams {
                    run_id: run_id.to_string(),
                    title: planned.title.clone(),
                    spec: planned.spec.clone(),
                    task_profile,
                    status: "pending".to_string(),
                    graph_x: None,
                    graph_y: None,
                })
                .await
                .map_err(OrchestratorError::from)?;
            task_ids.push(task.id);
        }

        // 2. Dep edges: blocker (the depended-on, earlier task) → blocked (this).
        for (idx, planned) in dag.tasks.iter().enumerate() {
            let blocked_id = &task_ids[idx];
            for &dep_idx in &planned.depends_on {
                if let Some(blocker_id) = task_ids.get(dep_idx) {
                    self.run_repo
                        .add_dep(blocker_id, blocked_id)
                        .await
                        .map_err(OrchestratorError::from)?;
                } else {
                    tracing::warn!(
                        run_id,
                        task_idx = idx,
                        dep_idx,
                        "planner produced an out-of-range depends_on index; skipping edge"
                    );
                }
            }
        }

        // 3. Assignments: member_index → member id. Default to member 0 when the
        //    planner left it unset or out of range (engine needs an assignment).
        for (idx, planned) in dag.tasks.iter().enumerate() {
            let member = resolve_member(&members, planned.member_index);
            let Some(member) = member else {
                tracing::warn!(
                    run_id,
                    task_idx = idx,
                    "fleet snapshot has no members; cannot assign task (engine will fail it)"
                );
                continue;
            };
            let task_id = &task_ids[idx];
            self.run_repo
                .create_assignment(CreateAssignmentParams {
                    task_id: task_id.clone(),
                    member_id: member.id.clone(),
                    score: None,
                    rationale: planned.rationale.clone(),
                    source: "auto".to_string(),
                    locked: false,
                })
                .await
                .map_err(OrchestratorError::from)?;
            self.emitter.emit_task_assigned(run_id, task_id, &member.id);
        }

        self.emitter.emit_run_plan_updated(run_id);

        // Flip to running so the engine may pick it up.
        self.run_repo
            .update_run(
                run_id,
                UpdateRunParams {
                    status: Some("running".to_string()),
                    summary: None,
                    lead_conv_id: None,
                    total_tokens: None,
                },
            )
            .await
            .map_err(OrchestratorError::from)?;
        self.emitter.emit_run_status(run_id, "running");
        Ok(())
    }

    /// Cancel a run: flip it to `cancelled` and emit. The engine's cooperative
    /// cancel (set via [`RunEngine::stop`](crate::engine::RunEngine::stop)) is
    /// the runtime counterpart; this is the persisted state change.
    pub async fn cancel(&self, run_id: &str) -> Result<(), AppError> {
        // Confirm it exists for a clean 404.
        if self
            .run_repo
            .get_run(run_id)
            .await
            .map_err(OrchestratorError::from)?
            .is_none()
        {
            return Err(OrchestratorError::NotFound(format!("run {run_id}")).into());
        }
        self.run_repo
            .update_run(
                run_id,
                UpdateRunParams {
                    status: Some("cancelled".to_string()),
                    summary: None,
                    lead_conv_id: None,
                    total_tokens: None,
                },
            )
            .await
            .map_err(OrchestratorError::from)?;
        self.emitter.emit_run_status(run_id, "cancelled");
        self.emitter.emit_run_completed(run_id, "cancelled");
        Ok(())
    }

    /// Load the chosen fleet's members as DTOs (decoding JSON columns fail-soft).
    /// A missing fleet is a clean 404.
    async fn load_fleet_members(&self, fleet_id: &str) -> Result<Vec<FleetMember>, AppError> {
        if self
            .fleet_repo
            .get_fleet(fleet_id)
            .await
            .map_err(OrchestratorError::from)?
            .is_none()
        {
            return Err(OrchestratorError::NotFound(format!("fleet {fleet_id}")).into());
        }
        let rows = self
            .fleet_repo
            .list_members(fleet_id)
            .await
            .map_err(OrchestratorError::from)?;
        Ok(rows.into_iter().map(member_row_to_dto).collect())
    }
}

/// Resolve a planned `member_index` to a fleet-snapshot member, defaulting to
/// member 0 when the index is unset or out of range. Returns `None` only when
/// the snapshot is empty.
fn resolve_member(members: &[FleetMember], member_index: Option<usize>) -> Option<&FleetMember> {
    match member_index {
        Some(i) => members.get(i).or_else(|| members.first()),
        None => members.first(),
    }
}

// --- Row → DTO mapping ------------------------------------------------------

fn run_row_to_dto(row: OrchRunRow) -> Run {
    Run {
        id: row.id,
        workspace_id: row.workspace_id,
        goal: row.goal,
        autonomy: row.autonomy,
        max_parallel: row.max_parallel,
        status: row.status,
        summary: row.summary,
        lead_conv_id: row.lead_conv_id,
        total_tokens: row.total_tokens,
        created_at: row.created_at,
        updated_at: row.updated_at,
    }
}

fn task_row_to_dto(row: OrchRunTaskRow) -> RunTask {
    let task_profile = row
        .task_profile
        .as_deref()
        .and_then(|raw| serde_json::from_str::<TaskProfile>(raw).ok());
    // `output_files` is a JSON array of strings; decode fail-soft to empty.
    let output_files = row
        .output_files
        .as_deref()
        .and_then(|raw| serde_json::from_str::<Vec<String>>(raw).ok())
        .unwrap_or_default();
    RunTask {
        id: row.id,
        run_id: row.run_id,
        title: row.title,
        spec: row.spec,
        task_profile,
        status: row.status,
        conversation_id: row.conversation_id,
        // PROSE pass-through — NOT JSON (the Row comment is misleading).
        output_summary: row.output_summary,
        output_files,
        attempt: row.attempt,
        tokens: row.tokens,
        graph_x: row.graph_x,
        graph_y: row.graph_y,
    }
}

fn dep_row_to_dto(row: OrchRunTaskDepRow) -> RunTaskDep {
    RunTaskDep {
        blocker_task_id: row.blocker_task_id,
        blocked_task_id: row.blocked_task_id,
    }
}

fn assignment_row_to_dto(row: OrchAssignmentRow) -> Assignment {
    Assignment {
        id: row.id,
        task_id: row.task_id,
        member_id: row.member_id,
        score: row.score,
        rationale: row.rationale,
        source: row.source,
        locked: row.locked != 0,
    }
}

/// Map a fleet member DB row to its DTO, decoding the JSON columns fail-soft
/// (mirrors `service::member_row_to_dto`). Kept local so `RunService` stays
/// self-contained over the raw `fleet_repo`.
fn member_row_to_dto(row: FleetMemberRow) -> FleetMember {
    let capability_profile = row
        .capability_profile
        .as_deref()
        .and_then(|raw| serde_json::from_str(raw).ok());
    let constraints = row
        .constraints
        .as_deref()
        .and_then(|raw| serde_json::from_str(raw).ok());
    FleetMember {
        id: row.id,
        agent_id: row.agent_id,
        provider_id: row.provider_id,
        model: row.model,
        role_hint: row.role_hint,
        capability_profile,
        constraints,
        sort_order: row.sort_order,
    }
}

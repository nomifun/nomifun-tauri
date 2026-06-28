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
    Assignment, CreateAdhocRunRequest, CreateRunRequest, FleetMember, ModelRange, PlannedDag,
    ReassignRequest, ReplanRequest, Run, RunDetail, RunTask, RunTaskDep, TaskProfile,
    WorkspaceEntry,
};
use nomifun_common::AppError;
use nomifun_common::generate_prefixed_id;
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
use crate::router::rank_members;

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
                workspace_id: Some(req.workspace_id),
                user_id: user_id.to_string(),
                goal: req.goal,
                fleet_snapshot,
                autonomy,
                max_parallel: req.max_parallel,
                // Workspace-backed run: no ad-hoc work_dir / pre-bound conversation.
                work_dir: None,
                lead_conv_id: None,
            })
            .await
            .map_err(OrchestratorError::from)?;

        let run = run_row_to_dto(row);
        // Status starts at `planning` (the repo INSERTs it); surface it on the bus.
        self.emitter.emit_run_status(&run.id, &run.status);
        Ok(run)
    }

    /// Create an **ad-hoc** run straight from a conversation: no workspace, no
    /// pre-built fleet. The fleet is synthesized on the fly from the request's
    /// [`ModelRange`] (one synthetic [`FleetMember`] per `provider+model`), and the
    /// run carries its own `work_dir` (the engine prefers it over a workspace dir).
    ///
    /// The run is parked in `planning` exactly like [`create`](Self::create) —
    /// the snapshot is opaque to the engine, which resolves members from
    /// `fleet_snapshot` by id. Synthetic member ids are minted unique + stable
    /// (`generate_prefixed_id("rmbr")`) so the engine's task→member resolution is
    /// deterministic.
    ///
    /// `ModelRange::Auto` is rejected here: its expansion to a concrete `range`
    /// requires provider access and is the caps_orchestrator layer's job (Task 3),
    /// which calls this with an already-expanded `Single`/`Range`.
    pub async fn create_adhoc(
        &self,
        user_id: &str,
        req: CreateAdhocRunRequest,
    ) -> Result<Run, AppError> {
        if req.goal.trim().is_empty() {
            return Err(OrchestratorError::BadRequest("goal must not be empty".into()).into());
        }
        // Synthesize the fleet from the model range (Single/Range only; Auto and
        // empty ranges are rejected — pinned_roles is parsed but ignored in P1),
        // then merge in any pre-constructed role members (P4 Task 2: the
        // caps_orchestrator layer resolves ENABLED assistants into enriched
        // FleetMembers and passes them via `role_members`). Dedup by
        // `(provider_id, model, agent_id)` so an assistant pinned to the same
        // `(provider, model)` as a bare range member does not produce a duplicate
        // routing target — the enriched (assistant-backed) member wins, since it
        // carries the persona/skills/description the planner + worker need.
        let members = merge_members(build_members_from_range(&req.model_range)?, req.role_members);
        let fleet_snapshot =
            serde_json::to_string(&members).unwrap_or_else(|_| "[]".to_string());

        let autonomy = req
            .autonomy
            .filter(|a| !a.trim().is_empty())
            .unwrap_or_else(|| DEFAULT_AUTONOMY.to_string());
        // An empty work_dir string is treated as absent (no dir).
        let work_dir = req
            .work_dir
            .map(|w| w.trim().to_string())
            .filter(|w| !w.is_empty());

        let row = self
            .run_repo
            .create_run(CreateRunParams {
                workspace_id: None,
                user_id: user_id.to_string(),
                goal: req.goal,
                fleet_snapshot,
                autonomy,
                max_parallel: req.max_parallel,
                work_dir,
                lead_conv_id: req.lead_conv_id,
            })
            .await
            .map_err(OrchestratorError::from)?;

        let run = run_row_to_dto(row);
        self.emitter.emit_run_status(&run.id, &run.status);
        Ok(run)
    }

    /// Full run detail: the run + its task DAG (tasks, dep edges, assignments)
    /// plus the run's frozen fleet snapshot decoded into `fleet_members`. The
    /// snapshot decode is fail-soft (parse error → empty vec + warn) so a
    /// corrupt snapshot never blocks reading the rest of the run.
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
        let fleet_members = decode_fleet_snapshot(id, &row.fleet_snapshot);
        Ok(RunDetail {
            run: run_row_to_dto(row),
            tasks: tasks.into_iter().map(task_row_to_dto).collect(),
            deps: deps.into_iter().map(dep_row_to_dto).collect(),
            assignments: assignments.into_iter().map(assignment_row_to_dto).collect(),
            fleet_members,
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

    /// All runs owned by a user, newest first — across every workspace AND ad-hoc
    /// (workspace-less) runs. The read path for the read-only Run-history library
    /// (the repurposed orchestrator tab): ad-hoc runs created straight from a
    /// conversation carry `workspace_id = None`, so they never surface under the
    /// workspace-scoped [`list`](Self::list) and must be listed by owner here.
    pub async fn list_by_user(&self, user_id: &str) -> Result<Vec<Run>, AppError> {
        let rows = self
            .run_repo
            .list_runs_by_user(user_id)
            .await
            .map_err(OrchestratorError::from)?;
        Ok(rows.into_iter().map(run_row_to_dto).collect())
    }

    /// Plan a run: decompose the goal into a task DAG, persist tasks + deps +
    /// assignments, emit `planUpdated`, then apply the **autonomy gate** — an
    /// `interactive` run parks at `awaiting_plan_approval` (a human approves the
    /// plan before any worker dispatches); every other level flips to `running`.
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

        let members: Vec<FleetMember> = decode_fleet_snapshot(run_id, &run.fleet_snapshot);

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
                    role: planned.role.clone(),
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

        // 3. Assignments — LLM-primary + Router-veto. For each task we build a
        //    TaskProfile (the planner's, or a neutral default), rank the snapshot
        //    members, and pick:
        //    - the planner's `member_index` whenever it is VIABLE (present in
        //      `ranked` at all = it passed the Router's HARD filters);
        //    - else the Router's top pick (`ranked[0]`) — when the planner abstained
        //      (no `member_index`) OR its pick was hard-filtered out (vetoed);
        //    - a fallback to the planner's index / member 0 when EVERY member was
        //      hard-filtered out (`ranked` empty) — the engine needs an assignment to
        //      run the task, so leaving it unassigned would fail it.
        //    An existing *locked* assignment is never overwritten (re-plan must
        //    respect human overrides).
        //
        //    WHY honor the planner anywhere in `ranked` (not just a Router top-K)?
        //    Members are now typically BARE models (`capability_profile: None`), which
        //    the Router scores NEUTRALLY — every such member ties, so the Router has
        //    no discriminating signal and its ordering is arbitrary among them. The
        //    real signal is the LLM planner's description-informed `member_index`
        //    (Change A feeds each model's user-authored `desc` into the prompt). So
        //    the description-driven pick must be HONORED; the Router's job shrinks to
        //    (a) a HARD-FILTER veto (vision/tool requirements the member can't meet)
        //    and (b) supplying the fallback ordering when the planner abstains or its
        //    pick is vetoed. (The retired top-K rule predates per-model descriptions
        //    and would wrongly override a deliberate-but-not-top-scored pick.)
        for (idx, planned) in dag.tasks.iter().enumerate() {
            let task_id = &task_ids[idx];

            // Respect a locked assignment: re-plan must not touch it.
            if let Some(existing) = self
                .run_repo
                .get_assignment_for_task(task_id)
                .await
                .map_err(OrchestratorError::from)?
            {
                if existing.locked != 0 {
                    continue;
                }
            }

            let profile = planned.task_profile.clone().unwrap_or_else(default_profile);
            let ranked = rank_members(&members, &profile);

            // Decide the member index + the score/rationale to record.
            let pick = if ranked.is_empty() {
                // All hard-filtered: fall back so the task still gets assigned.
                resolve_member(&members, planned.member_index).map(|m| AssignmentPick {
                    member_id: m.id.clone(),
                    score: None,
                    rationale: planned.rationale.clone(),
                })
            } else {
                // Honor the planner's pre-assignment whenever it is VIABLE (present
                // anywhere in `ranked` = it survived the hard filters). Only when the
                // planner abstained, or its pick was hard-filtered (absent from
                // `ranked` = vetoed), do we fall back to the Router's top pick.
                let planner_choice = planned
                    .member_index
                    .and_then(|mi| ranked.iter().find(|c| c.member_index == mi));
                let chosen = planner_choice.unwrap_or(&ranked[0]);
                members.get(chosen.member_index).map(|m| AssignmentPick {
                    member_id: m.id.clone(),
                    score: Some(chosen.score),
                    rationale: Some(chosen.rationale.clone()),
                })
            };

            let Some(pick) = pick else {
                tracing::warn!(
                    run_id,
                    task_idx = idx,
                    "fleet snapshot has no members; cannot assign task (engine will fail it)"
                );
                continue;
            };

            // `plan` mints fresh tasks every call, so each task_id here is new and
            // has no prior assignment — `create_assignment` never stacks. The
            // locked-skip guard above is defensive (in case a future re-plan reuses
            // task ids): an existing locked assignment is honored, not overwritten.
            self.run_repo
                .create_assignment(CreateAssignmentParams {
                    task_id: task_id.clone(),
                    member_id: pick.member_id.clone(),
                    score: pick.score,
                    rationale: pick.rationale,
                    source: "auto".to_string(),
                    locked: false,
                })
                .await
                .map_err(OrchestratorError::from)?;
            self.emitter.emit_task_assigned(run_id, task_id, &pick.member_id);
        }

        self.emitter.emit_run_plan_updated(run_id);

        // Autonomy gate: `interactive` runs park at `awaiting_plan_approval` for a
        // human to confirm the plan before any worker dispatches (the human-in-the-
        // loop sits at the PLAN gate, not per-worker — workers always run yolo). All
        // other autonomy levels (`autonomous` / `supervised`) flip straight to
        // `running` so the engine may pick the run up. The route reads `run.autonomy`
        // to decide whether to `engine.start` (interactive: NOT until `approve_plan`).
        let next_status = if run.autonomy == "interactive" {
            "awaiting_plan_approval"
        } else {
            "running"
        };
        self.run_repo
            .update_run(
                run_id,
                UpdateRunParams {
                    status: Some(next_status.to_string()),
                    summary: None,
                    lead_conv_id: None,
                    total_tokens: None,
                    goal: None,
                    autonomy: None,
                    fleet_snapshot: None,
                },
            )
            .await
            .map_err(OrchestratorError::from)?;
        self.emitter.emit_run_status(run_id, next_status);
        Ok(())
    }

    /// Approve an `interactive` run's plan: `awaiting_plan_approval` → `running`
    /// + emit. The caller (route) then `engine.start`s the loop — `approve_plan`
    /// only mutates persisted state, mirroring how `create`/`plan` leave engine
    /// lifecycle to the route. A run not in `awaiting_plan_approval` is a 400
    /// (you cannot approve a plan that is already running / not awaiting approval).
    pub async fn approve_plan(&self, run_id: &str) -> Result<(), AppError> {
        self.transition(run_id, &["awaiting_plan_approval"], "running").await
    }

    /// Pause a `running` run: `running` → `paused` + emit. The engine's persistent
    /// loop keeps running but stops filling new workers (it re-reads the run status
    /// each iteration); any in-flight workers run to completion. Pause does NOT
    /// cancel in-flight work (that is `cancel`). A run not `running` is a 400.
    pub async fn pause(&self, run_id: &str) -> Result<(), AppError> {
        self.transition(run_id, &["running"], "paused").await
    }

    /// Resume a `paused` run: `paused` → `running` + emit. The caller (route) then
    /// `engine.start`s the loop (idempotent stop-then-start): if the loop was still
    /// alive idling on the paused gate it resumes filling on its next iteration; if
    /// it had exited, `start` respawns it. A run not `paused` is a 400.
    pub async fn resume(&self, run_id: &str) -> Result<(), AppError> {
        self.transition(run_id, &["paused"], "running").await
    }

    /// Shared status-transition helper for the run lifecycle controls
    /// (`approve_plan` / `pause` / `resume`): load the run (clean 404), require its
    /// current status to be one of `from` (else a 400 that names the actual state),
    /// then persist `to` + emit `run.statusChanged`.
    async fn transition(
        &self,
        run_id: &str,
        from: &[&str],
        to: &str,
    ) -> Result<(), AppError> {
        let run = self
            .run_repo
            .get_run(run_id)
            .await
            .map_err(OrchestratorError::from)?
            .ok_or_else(|| OrchestratorError::NotFound(format!("run {run_id}")))?;
        if !from.contains(&run.status.as_str()) {
            return Err(OrchestratorError::BadRequest(format!(
                "run {run_id} is `{}`, cannot transition to `{to}` (expected one of {from:?})",
                run.status
            ))
            .into());
        }
        self.run_repo
            .update_run(
                run_id,
                UpdateRunParams {
                    status: Some(to.to_string()),
                    summary: None,
                    lead_conv_id: None,
                    total_tokens: None,
                    goal: None,
                    autonomy: None,
                    fleet_snapshot: None,
                },
            )
            .await
            .map_err(OrchestratorError::from)?;
        self.emitter.emit_run_status(run_id, to);
        Ok(())
    }

    /// Override (or lock) the member assigned to a task. This is the human
    /// reassign path: it upserts the task's assignment to the requested member
    /// with `source = "override"`. `locked` defaults to `true` — a deliberate
    /// override should survive a later re-plan — unless the caller explicitly
    /// passes `false`.
    ///
    /// We verify the run + task exist (clean 404s) and that the member is part of
    /// the run's frozen fleet snapshot (a 400 otherwise — you cannot assign a
    /// member the run was never created with).
    pub async fn reassign(
        &self,
        run_id: &str,
        task_id: &str,
        req: ReassignRequest,
    ) -> Result<(), AppError> {
        let run = self
            .run_repo
            .get_run(run_id)
            .await
            .map_err(OrchestratorError::from)?
            .ok_or_else(|| OrchestratorError::NotFound(format!("run {run_id}")))?;

        // The task must exist and belong to this run.
        let task = self
            .run_repo
            .get_task(task_id)
            .await
            .map_err(OrchestratorError::from)?
            .ok_or_else(|| OrchestratorError::NotFound(format!("task {task_id}")))?;
        if task.run_id != run_id {
            return Err(OrchestratorError::NotFound(format!("task {task_id} in run {run_id}")).into());
        }

        // The member must be part of the run's frozen fleet snapshot.
        let members = decode_fleet_snapshot(run_id, &run.fleet_snapshot);
        if !members.iter().any(|m| m.id == req.member_id) {
            return Err(OrchestratorError::BadRequest(format!(
                "member {} is not in run {run_id}'s fleet snapshot",
                req.member_id
            ))
            .into());
        }

        let locked = req.locked.unwrap_or(true);
        self.run_repo
            .set_assignment(CreateAssignmentParams {
                task_id: task_id.to_string(),
                member_id: req.member_id.clone(),
                score: None,
                rationale: Some("人工指派".to_string()),
                source: "override".to_string(),
                locked,
            })
            .await
            .map_err(OrchestratorError::from)?;
        self.emitter.emit_task_assigned(run_id, task_id, &req.member_id);
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
                    goal: None,
                    autonomy: None,
                    fleet_snapshot: None,
                },
            )
            .await
            .map_err(OrchestratorError::from)?;
        self.emitter.emit_run_status(run_id, "cancelled");
        self.emitter.emit_run_completed(run_id, "cancelled");
        Ok(())
    }

    /// Delete a run (owner-scoped). Loads the run for a clean 404 (missing) /
    /// 403 (owned by another user — destructive, so ownership IS enforced here,
    /// unlike the read/lifecycle handlers), then deletes the row. The schema's
    /// `ON DELETE CASCADE` FKs sweep out the run's tasks → deps + assignments, so
    /// no manual child cleanup is needed. The route stops the engine loop FIRST
    /// (mirrors `cancel`'s `engine.stop` → service ordering) so a live loop is
    /// cooperatively cancelled before the rows vanish from under it.
    pub async fn delete(&self, user_id: &str, run_id: &str) -> Result<(), AppError> {
        self.owned_run(user_id, run_id).await?;
        self.run_repo
            .delete_run(run_id)
            .await
            .map_err(OrchestratorError::from)?;
        // The run is gone; surface a terminal "removed" signal on the bus so any
        // live subscriber drops it (reuse the completed channel with a removed
        // status — the run row no longer exists to query).
        self.emitter.emit_run_completed(run_id, "removed");
        Ok(())
    }

    /// Rename a run = change its goal (owner-scoped). Loads the run for a clean
    /// 404/403, rejects a blank goal (a run goal is `NOT NULL` and must not be
    /// empty — same rule as `create`), then updates only the `goal` column and
    /// emits a plan-updated signal so subscribers refresh the run header.
    pub async fn rename(&self, user_id: &str, run_id: &str, goal: &str) -> Result<(), AppError> {
        let goal = goal.trim();
        if goal.is_empty() {
            return Err(OrchestratorError::BadRequest("goal must not be empty".into()).into());
        }
        self.owned_run(user_id, run_id).await?;
        self.run_repo
            .update_run(
                run_id,
                UpdateRunParams {
                    status: None,
                    summary: None,
                    lead_conv_id: None,
                    total_tokens: None,
                    goal: Some(goal.to_string()),
                    autonomy: None,
                    fleet_snapshot: None,
                },
            )
            .await
            .map_err(OrchestratorError::from)?;
        self.emitter.emit_run_plan_updated(run_id);
        Ok(())
    }

    /// Re-plan a run in place (owner-scoped): clear the run's old plan and
    /// re-decompose against the (optionally) edited goal / model range / autonomy.
    /// This is the "全局重新规划" path — when the current decomposition is wrong,
    /// the user edits the run's inputs and the fleet re-plans from scratch (same
    /// run, no fork; the old plan is destroyed).
    ///
    /// Order matters: the edits (goal / autonomy / fleet_snapshot) are persisted
    /// BEFORE `clear` + `plan`, because [`plan`](Self::plan) re-reads the run row
    /// and uses `run.goal` (decomposition input), `run.fleet_snapshot` (assignment
    /// pool) and `run.autonomy` (the post-plan gate). [`clear_run_tasks`] drops the
    /// old tasks (cascading deps + assignments) so `plan`'s append semantics yield a
    /// clean re-decomposition rather than a merge. `plan()` itself is unchanged — the
    /// clear step is the only addition.
    ///
    /// `model_range` must already be `single`/`range` — an unexpanded `auto` is a
    /// `BadRequest` (same contract as `create_adhoc`; the caller expands it). An
    /// omitted field leaves that column unchanged. `pinned_roles` is accepted for
    /// forward-compatibility but not yet wired into planning (carry-forward).
    pub async fn replan(
        &self,
        user_id: &str,
        run_id: &str,
        req: ReplanRequest,
    ) -> Result<Run, AppError> {
        // 404 (missing) / 403 (not owner) BEFORE any mutation — a non-owner must
        // not clear the plan.
        self.owned_run(user_id, run_id).await?;

        // Resolve the optional edits into UpdateRunParams fields. `None` = leave the
        // column unchanged; a blank goal is rejected (NOT NULL, same rule as create
        // / rename).
        let goal = match req.goal.as_deref().map(str::trim) {
            Some("") => {
                return Err(OrchestratorError::BadRequest("goal must not be empty".into()).into())
            }
            Some(g) => Some(g.to_string()),
            None => None,
        };
        // A new model range rebuilds the fleet snapshot (same synthesis as
        // create_adhoc); `auto` is rejected here just like create.
        let fleet_snapshot = match &req.model_range {
            Some(range) => {
                let members = build_members_from_range(range)?;
                Some(serde_json::to_string(&members).unwrap_or_else(|_| "[]".to_string()))
            }
            None => None,
        };
        let autonomy = req.autonomy.clone();

        // Persist the edits FIRST so plan() reads the new goal / snapshot / autonomy.
        // (When every edit is None, update_run early-returns — a harmless no-op.)
        if goal.is_some() || autonomy.is_some() || fleet_snapshot.is_some() {
            self.run_repo
                .update_run(
                    run_id,
                    UpdateRunParams {
                        status: None,
                        summary: None,
                        lead_conv_id: None,
                        total_tokens: None,
                        goal,
                        autonomy,
                        fleet_snapshot,
                    },
                )
                .await
                .map_err(OrchestratorError::from)?;
        }

        // Clear the old plan (cascade drops its deps + assignments), then
        // re-decompose against the (edited) run inputs via the unchanged plan().
        self.run_repo
            .clear_run_tasks(run_id)
            .await
            .map_err(OrchestratorError::from)?;
        self.plan(run_id).await?;

        // Return the re-planned run so the route can read its (possibly switched)
        // autonomy to decide whether to (re)start the engine — mirrors create /
        // create_adhoc (which `engine.start` only for non-`interactive` runs).
        let row = self
            .run_repo
            .get_run(run_id)
            .await
            .map_err(OrchestratorError::from)?
            .ok_or_else(|| OrchestratorError::NotFound(format!("run {run_id}")))?;
        Ok(run_row_to_dto(row))
    }

    /// Load a run and enforce caller ownership: a missing run is a clean 404, a
    /// run owned by another user is a 403. Returns the row on success. Ownership
    /// reads `OrchRunRow.user_id` (deliberately NOT surfaced on the `Run` DTO),
    /// so the check must use the row, not `get_detail`. Used by the destructive /
    /// mutating run controls (`delete` / `rename`).
    async fn owned_run(&self, user_id: &str, run_id: &str) -> Result<OrchRunRow, AppError> {
        let row = self
            .run_repo
            .get_run(run_id)
            .await
            .map_err(OrchestratorError::from)?
            .ok_or_else(|| OrchestratorError::NotFound(format!("run {run_id}")))?;
        if row.user_id != user_id {
            return Err(AppError::Forbidden(format!("run {run_id} is not owned by you")));
        }
        Ok(row)
    }

    /// List one directory level under a run's working directory (owner-scoped).
    /// The root is **server-authoritative**: prefer the ad-hoc run's own
    /// `work_dir`, else fall back to its bound workspace's `workspace_dir`
    /// (workspace-backed runs). A run with neither is a `BadRequest` (nothing to
    /// browse). The client supplies only a workspace-relative `path` + optional
    /// `search`; the `..`-rejection + boundary/depth guards live in
    /// [`nomifun_file::list_workspace_level`]. The run-history counterpart of
    /// `ConversationService::browse_workspace` / `TerminalService::browse_workspace`.
    pub async fn browse_workspace(
        &self,
        user_id: &str,
        run_id: &str,
        path: &str,
        search: Option<&str>,
    ) -> Result<Vec<WorkspaceEntry>, AppError> {
        let row = self.owned_run(user_id, run_id).await?;
        let root = self.resolve_run_dir(&row).await?;
        nomifun_file::list_workspace_level(std::path::Path::new(&root), path, search)
    }

    /// Resolve a run's working-directory root: prefer `work_dir` (ad-hoc run),
    /// else the bound workspace's `workspace_dir`. A run with neither a non-blank
    /// `work_dir` nor a workspace dir is a `BadRequest` — there is nothing to
    /// browse (e.g. a legacy workspace-backed run whose workspace had no dir).
    async fn resolve_run_dir(&self, row: &OrchRunRow) -> Result<String, AppError> {
        if let Some(dir) = row.work_dir.as_ref().filter(|d| !d.trim().is_empty()) {
            return Ok(dir.clone());
        }
        if let Some(ws_id) = row.workspace_id.as_deref() {
            let ws = self
                .ws_repo
                .get(ws_id)
                .await
                .map_err(OrchestratorError::from)?;
            if let Some(dir) = ws.and_then(|w| w.workspace_dir).filter(|d| !d.trim().is_empty()) {
                return Ok(dir);
            }
        }
        Err(OrchestratorError::BadRequest(format!(
            "run {} has no working directory to browse",
            row.id
        ))
        .into())
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

/// Synthesize the ad-hoc fleet members for a [`ModelRange`]. Each `provider+model`
/// pair becomes one [`FleetMember`] with both `provider_id` and `model` set to
/// `Some` (the worker hard-requires both — `worker.rs:116-120`), a freshly minted
/// unique id (`generate_prefixed_id("rmbr")` — the engine resolves task→member by
/// id, so ids must be unique + stable), and `sort_order` = its snapshot index.
/// The member is otherwise bare (`agent_id` empty, no capability profile /
/// constraints / role hint) — capability routing in P1 falls back to the neutral
/// default profile.
///
/// **Contract (Task 3):** only `Single` and `Range` are handled here. `Auto` is a
/// `BadRequest` — its expansion to a concrete `Range` requires provider access and
/// belongs in the caps_orchestrator layer, which must pass an already-expanded
/// range. An empty `Range` is also a `BadRequest` (a run needs at least one model).
fn build_members_from_range(range: &ModelRange) -> Result<Vec<FleetMember>, AppError> {
    let model_refs: Vec<(&str, &str)> = match range {
        ModelRange::Single { model } => {
            vec![(model.provider_id.as_str(), model.model.as_str())]
        }
        ModelRange::Range { models } => models
            .iter()
            .map(|m| (m.provider_id.as_str(), m.model.as_str()))
            .collect(),
        ModelRange::Auto => {
            // Caller (caps_orchestrator, Task 3) must expand Auto to a concrete
            // Range before reaching the service — it has provider access; we don't.
            return Err(OrchestratorError::BadRequest(
                "auto range must be expanded by caller".into(),
            )
            .into());
        }
    };

    if model_refs.is_empty() {
        return Err(
            OrchestratorError::BadRequest("model_range 为空：无可用模型".into()).into(),
        );
    }

    let members = model_refs
        .into_iter()
        .enumerate()
        .map(|(i, (provider_id, model))| FleetMember {
            id: generate_prefixed_id("rmbr"),
            agent_id: String::new(),
            provider_id: Some(provider_id.to_string()),
            model: Some(model.to_string()),
            role_hint: None,
            capability_profile: None,
            constraints: None,
            sort_order: i as i64,
            // Bare model members carry no persona/skills. `description` is left
            // None here — the caps_orchestrator layer fills it from the model's
            // user-authored description before this point (a bare member built
            // straight from a range, e.g. the workspace path, simply has no
            // description, which the planner renders as `desc=-`).
            description: None,
            system_prompt: None,
            enabled_skills: Vec::new(),
            disabled_builtin_skills: Vec::new(),
        })
        .collect();
    Ok(members)
}

/// Merge the bare model-range members with the pre-constructed role
/// (assistant-backed) members into a single snapshot, deduping by
/// `(provider_id, model, agent_id)`.
///
/// **Order:** role members FIRST (keeping their relative order), then the bare
/// range members appended after; `sort_order` is rewritten to the final position
/// so the snapshot stays densely ordered and the `member_index` math (which
/// indexes into this vec during planning) stays correct.
///
/// **Dedup key** is the full `(provider_id, model, agent_id)` triple, FIRST
/// occurrence wins. Two consequences:
/// - An assistant-backed member is `(p, m, "<assistant id>")` while a bare range
///   member is `(p, m, "")`, so an assistant pinned to a model already in the
///   range is a DISTINCT routing target (it adds persona/skills) — both are kept.
/// - A role member with an EMPTY `agent_id` (a caps-built "description-decorated"
///   bare member) shares the bare range member's `(p, m, "")` key. Because role
///   members come first, the caps-built copy (which carries the model's
///   user-authored `description`) WINS over the plain range-built one — this is
///   how the bare members get their descriptions for the planner.
///
/// This keeps the keystone behavior (enabled assistants become candidate role
/// members alongside the bare models) while never minting two identical targets.
fn merge_members(range_members: Vec<FleetMember>, role_members: Vec<FleetMember>) -> Vec<FleetMember> {
    let mut seen: std::collections::HashSet<(Option<String>, Option<String>, String)> =
        std::collections::HashSet::new();
    let mut out: Vec<FleetMember> = Vec::with_capacity(range_members.len() + role_members.len());
    // Role members first so a caps-built copy wins on a true collision.
    for m in role_members.into_iter().chain(range_members.into_iter()) {
        let key = (m.provider_id.clone(), m.model.clone(), m.agent_id.clone());
        if seen.insert(key) {
            out.push(m);
        }
    }
    // Re-densify sort_order to the final snapshot index.
    for (i, m) in out.iter_mut().enumerate() {
        m.sort_order = i as i64;
    }
    out
}

/// The member + score/rationale chosen for a task during planning.
struct AssignmentPick {
    member_id: String,
    score: Option<f64>,
    rationale: Option<String>,
}

/// A neutral default profile for tasks the planner left unprofiled: a `general`
/// task with no special modality / reasoning / bulk requirements. The Router
/// scores every member against this without hard-filtering anyone out.
fn default_profile() -> TaskProfile {
    TaskProfile {
        kind: "general".to_string(),
        needs_vision: false,
        needs_long_context: false,
        needs_high_reasoning: false,
        bulk: false,
    }
}

/// Decode a run's `fleet_snapshot` JSON into its members, fail-soft: a parse
/// error logs a warning and yields an empty vec (never blocks the read path).
fn decode_fleet_snapshot(run_id: &str, snapshot: &str) -> Vec<FleetMember> {
    match serde_json::from_str::<Vec<FleetMember>>(snapshot) {
        Ok(members) => members,
        Err(err) => {
            tracing::warn!(run_id, error = %err, "failed to decode fleet_snapshot; using empty members");
            Vec::new()
        }
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
        work_dir: row.work_dir,
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
        role: row.role,
        created_at: row.created_at,
        updated_at: row.updated_at,
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
        // Pre-built fleet rows (the workspace path) carry no enriched persona;
        // the enrichment fields default. (Assistant-backed members are minted
        // at ad-hoc create time, never persisted in the `fleets` table.)
        description: None,
        system_prompt: None,
        enabled_skills: Vec::new(),
        disabled_builtin_skills: Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::service::{FleetService, WorkspaceService};
    use nomifun_api_types::{
        CapabilityProfile, CreateAdhocRunRequest, CreateFleetRequest, CreateWorkspaceRequest,
        FleetMemberInput, ModelRange, ModelRef, PlannedTask, ReplanRequest, WebSocketMessage,
    };
    use nomifun_db::{
        SqliteFleetRepository, SqliteOrchWorkspaceRepository, SqliteRunRepository,
        init_database_memory,
    };
    use nomifun_realtime::EventBroadcaster;
    use std::sync::Mutex;

    /// No-op broadcaster: these tests assert persisted state, not the event trail.
    struct NoopBroadcaster;
    impl EventBroadcaster for NoopBroadcaster {
        fn broadcast(&self, _event: WebSocketMessage<serde_json::Value>) {}
    }

    /// A planner that returns a fixed [`PlannedDag`] regardless of goal/members,
    /// so each test controls the exact tasks (and their member_index / profile).
    struct FixedPlanProducer(Mutex<PlannedDag>);
    impl FixedPlanProducer {
        fn new(dag: PlannedDag) -> Self {
            Self(Mutex::new(dag))
        }
    }
    #[async_trait::async_trait]
    impl PlanProducer for FixedPlanProducer {
        async fn produce(
            &self,
            _goal: &str,
            _members: &[FleetMember],
        ) -> Result<PlannedDag, AppError> {
            Ok(self.0.lock().unwrap().clone())
        }
    }

    /// Member input with a specific capability profile (strengths/reasoning/cost).
    fn member_input(
        agent_id: &str,
        strengths: &[&str],
        reasoning: &str,
        cost_tier: &str,
    ) -> FleetMemberInput {
        FleetMemberInput {
            agent_id: agent_id.to_string(),
            provider_id: None,
            model: None,
            role_hint: None,
            capability_profile: Some(CapabilityProfile {
                strengths: strengths.iter().map(|s| s.to_string()).collect(),
                modalities: vec!["text".to_string()],
                tools: true,
                reasoning: reasoning.to_string(),
                cost_tier: cost_tier.to_string(),
                speed_tier: "standard".to_string(),
            }),
            constraints: None,
            sort_order: None,
        }
    }

    fn coding_profile() -> TaskProfile {
        TaskProfile {
            kind: "coding".to_string(),
            needs_vision: false,
            needs_long_context: false,
            needs_high_reasoning: true,
            bulk: false,
        }
    }

    /// Build a fully-wired RunService + repos, seed a workspace + a fleet with the
    /// given members, create a run (parked in `planning`), and return everything a
    /// test needs. The planner is `FixedPlanProducer(dag)`.
    async fn harness(
        members: Vec<FleetMemberInput>,
        dag: PlannedDag,
    ) -> (RunService, Arc<SqliteRunRepository>, Vec<FleetMember>, String) {
        let db = init_database_memory().await.expect("db init");
        let pool = db.pool().clone();
        let run_repo = Arc::new(SqliteRunRepository::new(pool.clone()));
        let fleet_repo = Arc::new(SqliteFleetRepository::new(pool.clone()));
        let ws_repo = Arc::new(SqliteOrchWorkspaceRepository::new(pool.clone()));
        let emitter = OrchestratorRunEventEmitter::new(Arc::new(NoopBroadcaster));
        let planner: Arc<dyn PlanProducer> = Arc::new(FixedPlanProducer::new(dag));
        let run_service = RunService::new(
            run_repo.clone(),
            fleet_repo.clone(),
            ws_repo.clone(),
            planner,
            emitter,
        );

        let fleet = FleetService::new(fleet_repo)
            .create(
                "u1",
                CreateFleetRequest {
                    name: "router fleet".to_string(),
                    description: None,
                    max_parallel: None,
                    members,
                },
            )
            .await
            .expect("fleet create");
        let ws = WorkspaceService::new(ws_repo)
            .create(
                "u1",
                CreateWorkspaceRequest {
                    name: "router ws".to_string(),
                    default_fleet_id: Some(fleet.id.clone()),
                    workspace_dir: None,
                },
            )
            .await
            .expect("ws create");
        let run = run_service
            .create(
                "u1",
                CreateRunRequest {
                    workspace_id: ws.id,
                    goal: "do the thing".to_string(),
                    fleet_id: fleet.id,
                    autonomy: None,
                    max_parallel: None,
                },
            )
            .await
            .expect("run create");

        // The run snapshotted the fleet; read it back so the test knows member ids.
        let snapshot = run_service.get_detail(&run.id).await.expect("detail");
        (run_service, run_repo, snapshot.fleet_members, run.id)
    }

    /// A single-task DAG with the given member_index / profile.
    fn single_task_dag(member_index: Option<usize>, profile: Option<TaskProfile>) -> PlannedDag {
        PlannedDag {
            tasks: vec![PlannedTask {
                title: "task".to_string(),
                spec: "spec".to_string(),
                task_profile: profile,
                depends_on: vec![],
                member_index,
                rationale: None,
                role: None,
            }],
        }
    }

    // (a) plan picks the Router's top member when it is clearly more capable than
    // the others (coding + high reasoning task → the coding/high member wins),
    // even though the planner left member_index unset. The assignment records a
    // non-empty rationale + a score.
    #[tokio::test]
    async fn plan_picks_router_top_member() {
        // index 0: generalist (no strengths, medium reasoning); index 1: coding +
        // high reasoning — a far better fit for the coding profile.
        let members = vec![
            member_input("agent_gen", &[], "medium", "standard"),
            member_input("agent_coder", &["coding"], "high", "standard"),
        ];
        let dag = single_task_dag(None, Some(coding_profile()));
        let (svc, _repo, snapshot, run_id) = harness(members, dag).await;

        svc.plan(&run_id).await.expect("plan");

        let detail = svc.get_detail(&run_id).await.expect("detail");
        assert_eq!(detail.assignments.len(), 1);
        let asg = &detail.assignments[0];
        // The coder (member index 1) must win.
        assert_eq!(asg.member_id, snapshot[1].id, "router must pick the coder");
        assert_eq!(asg.source, "auto");
        assert!(!asg.locked);
        assert!(asg.score.is_some(), "router records a score");
        let rationale = asg.rationale.as_deref().unwrap_or("");
        assert!(!rationale.is_empty(), "rationale must be non-empty");
        assert!(
            rationale.contains("强项匹配[coding]"),
            "rationale should explain the coding match, got: {rationale}"
        );
    }

    // (b) plan honors the planner's member_index when it is a viable top
    // candidate. Both members are coders (tied scores → both in top-2), so a
    // planner pre-assignment of the (otherwise tie-broken-second) member 1 must
    // be honored rather than overridden to member 0.
    #[tokio::test]
    async fn plan_honors_planner_member_index_when_viable() {
        let members = vec![
            member_input("agent_a", &["coding"], "high", "standard"),
            member_input("agent_b", &["coding"], "high", "standard"),
        ];
        // Planner deliberately chose member 1.
        let dag = single_task_dag(Some(1), Some(coding_profile()));
        let (svc, _repo, snapshot, run_id) = harness(members, dag).await;

        svc.plan(&run_id).await.expect("plan");

        let detail = svc.get_detail(&run_id).await.expect("detail");
        assert_eq!(detail.assignments.len(), 1);
        assert_eq!(
            detail.assignments[0].member_id, snapshot[1].id,
            "planner's viable choice (member 1) must be honored"
        );
    }

    // (b') LLM-primary + Router-veto: the planner's pick is honored as long as it
    // is VIABLE (survives the Router's hard filters = present in `ranked`), even
    // when the Router would have scored OTHER members higher. Here member 0 is a
    // weak generalist and the task strongly favors the coders at 1/2, yet the
    // planner deliberately chose 0 — and a viable choice is honored, not overridden.
    // (Under the retired top-K rule member 0 fell outside the top-2 and the Router
    // overrode it; that is exactly the behavior this redesign reverses.)
    #[tokio::test]
    async fn plan_honors_viable_planner_pick_even_below_router_top() {
        // 3 members; the coding profile makes 1/2 clearly stronger than 0.
        let members = vec![
            member_input("agent_gen", &[], "low", "standard"), // weak, but still viable
            member_input("agent_c1", &["coding"], "high", "standard"),
            member_input("agent_c2", &["coding"], "high", "standard"),
        ];
        // Planner deliberately chose the weak generalist (index 0).
        let dag = single_task_dag(Some(0), Some(coding_profile()));
        let (svc, _repo, snapshot, run_id) = harness(members, dag).await;

        svc.plan(&run_id).await.expect("plan");

        let detail = svc.get_detail(&run_id).await.expect("detail");
        assert_eq!(
            detail.assignments[0].member_id, snapshot[0].id,
            "a viable planner pick is honored even when it is not the Router's top score"
        );
    }

    // (b) THE behavior change (keystone): 6 members all with capability_profile=None
    // (bare models → the Router scores them neutrally, no discriminating signal, all
    // tied). The planner — informed by the model DESCRIPTIONS — chose member_index=4.
    // The new rule must honor 4 because it is viable (present in `ranked`). The
    // retired top-K rule would have wrongly picked ranked[0] (index 0).
    #[tokio::test]
    async fn plan_honors_description_driven_pick_among_neutral_members() {
        // 6 bare members (no capability profile). The default `general` profile
        // hard-filters none, so all 6 are viable and tie at score 0.
        let members: Vec<FleetMemberInput> = (0..6)
            .map(|i| {
                let agent = format!("agent_{i}");
                FleetMemberInput {
                    agent_id: agent,
                    provider_id: None,
                    model: None,
                    role_hint: None,
                    capability_profile: None, // neutral: Router cannot discriminate
                    constraints: None,
                    sort_order: None,
                }
            })
            .collect();
        // Planner (reading descriptions) picked member 4 — NOT a top-2 candidate
        // under the retired rule, which would have fallen back to ranked[0].
        let dag = single_task_dag(Some(4), None);
        let (svc, _repo, snapshot, run_id) = harness(members, dag).await;

        svc.plan(&run_id).await.expect("plan");

        let detail = svc.get_detail(&run_id).await.expect("detail");
        assert_eq!(detail.assignments.len(), 1);
        assert_eq!(
            detail.assignments[0].member_id, snapshot[4].id,
            "the description-informed planner pick (member 4) must be honored, \
             not overridden to ranked[0]"
        );
    }

    // (d) when the planner abstains (no member_index), assignment falls to the
    // Router's top pick — here all members are neutral, so ranked[0] = member 0.
    #[tokio::test]
    async fn plan_falls_back_to_router_top_when_planner_abstains() {
        let members: Vec<FleetMemberInput> = (0..4)
            .map(|i| FleetMemberInput {
                agent_id: format!("agent_{i}"),
                provider_id: None,
                model: None,
                role_hint: None,
                capability_profile: None,
                constraints: None,
                sort_order: None,
            })
            .collect();
        // Planner left member_index unset.
        let dag = single_task_dag(None, None);
        let (svc, _repo, snapshot, run_id) = harness(members, dag).await;

        svc.plan(&run_id).await.expect("plan");

        let detail = svc.get_detail(&run_id).await.expect("detail");
        assert_eq!(
            detail.assignments[0].member_id, snapshot[0].id,
            "no planner pick → Router top (ranked[0] = member 0 for tied neutrals)"
        );
    }

    // plan falls back to the planner's member_index when every member is
    // hard-filtered out (e.g. a vision task with no vision-capable members) so the
    // task still gets an assignment.
    #[tokio::test]
    async fn plan_falls_back_when_all_hard_filtered() {
        let members = vec![
            member_input("agent_a", &["coding"], "high", "standard"),
            member_input("agent_b", &["writing"], "medium", "standard"),
        ];
        // Vision task; neither member declares the "vision" modality → all excluded.
        let vision = TaskProfile {
            kind: "analysis".to_string(),
            needs_vision: true,
            needs_long_context: false,
            needs_high_reasoning: false,
            bulk: false,
        };
        let dag = single_task_dag(Some(1), Some(vision));
        let (svc, _repo, snapshot, run_id) = harness(members, dag).await;

        svc.plan(&run_id).await.expect("plan");

        let detail = svc.get_detail(&run_id).await.expect("detail");
        assert_eq!(detail.assignments.len(), 1, "task still assigned on fallback");
        assert_eq!(
            detail.assignments[0].member_id, snapshot[1].id,
            "fallback honors the planner's member_index"
        );
    }

    // (c) Router-veto: when the planner picks a member the Router HARD-FILTERED
    // (it needs vision but that member has none) while OTHER members survive, the
    // filtered pick is NOT viable → assignment falls back to ranked[0], never the
    // excluded index. The hard filter is the Router's veto over the LLM pick.
    #[tokio::test]
    async fn plan_vetoes_planner_pick_that_was_hard_filtered() {
        // index 0: vision-capable (survives the vision filter, becomes ranked[0]).
        // index 1: text-only (hard-filtered out by the vision requirement).
        let vision_member = FleetMemberInput {
            agent_id: "agent_vision".to_string(),
            provider_id: None,
            model: None,
            role_hint: None,
            capability_profile: Some(CapabilityProfile {
                strengths: vec!["analysis".to_string()],
                modalities: vec!["text".to_string(), "vision".to_string()],
                tools: true,
                reasoning: "high".to_string(),
                cost_tier: "standard".to_string(),
                speed_tier: "standard".to_string(),
            }),
            constraints: None,
            sort_order: None,
        };
        let text_only = member_input("agent_text", &["analysis"], "high", "standard");
        let members = vec![vision_member, text_only];

        let vision = TaskProfile {
            kind: "analysis".to_string(),
            needs_vision: true,
            needs_long_context: false,
            needs_high_reasoning: false,
            bulk: false,
        };
        // Planner picked the text-only member (index 1) — but it is hard-filtered.
        let dag = single_task_dag(Some(1), Some(vision));
        let (svc, _repo, snapshot, run_id) = harness(members, dag).await;

        svc.plan(&run_id).await.expect("plan");

        let detail = svc.get_detail(&run_id).await.expect("detail");
        assert_eq!(detail.assignments.len(), 1);
        assert_eq!(
            detail.assignments[0].member_id, snapshot[0].id,
            "the hard-filtered planner pick is vetoed → fall back to the viable ranked[0]"
        );
        assert_ne!(
            detail.assignments[0].member_id, snapshot[1].id,
            "the excluded (no-vision) member must NOT be assigned"
        );
    }

    // (c) reassign sets source='override' and locked.
    #[tokio::test]
    async fn reassign_sets_override_and_locked() {
        let members = vec![
            member_input("agent_a", &["coding"], "high", "standard"),
            member_input("agent_b", &["writing"], "medium", "standard"),
        ];
        let dag = single_task_dag(None, Some(coding_profile()));
        let (svc, _repo, snapshot, run_id) = harness(members, dag).await;
        svc.plan(&run_id).await.expect("plan");

        let detail = svc.get_detail(&run_id).await.expect("detail");
        let task_id = detail.tasks[0].id.clone();
        // Auto-pick is the coder (member 0). Override to member 1.
        assert_eq!(detail.assignments[0].member_id, snapshot[0].id);

        svc.reassign(
            &run_id,
            &task_id,
            ReassignRequest {
                member_id: snapshot[1].id.clone(),
                locked: None, // default → true
            },
        )
        .await
        .expect("reassign");

        let after = svc.get_detail(&run_id).await.expect("detail");
        // Exactly one assignment (upsert replaced, not stacked).
        assert_eq!(after.assignments.len(), 1, "reassign upserts (no stacking)");
        let asg = &after.assignments[0];
        assert_eq!(asg.member_id, snapshot[1].id, "member overridden");
        assert_eq!(asg.source, "override");
        assert!(asg.locked, "locked defaults to true on override");
    }

    // reassign with locked=false overrides without locking.
    #[tokio::test]
    async fn reassign_unlocked_does_not_lock() {
        let members = vec![
            member_input("agent_a", &["coding"], "high", "standard"),
            member_input("agent_b", &["writing"], "medium", "standard"),
        ];
        let dag = single_task_dag(None, Some(coding_profile()));
        let (svc, _repo, snapshot, run_id) = harness(members, dag).await;
        svc.plan(&run_id).await.expect("plan");
        let task_id = svc.get_detail(&run_id).await.unwrap().tasks[0].id.clone();

        svc.reassign(
            &run_id,
            &task_id,
            ReassignRequest {
                member_id: snapshot[1].id.clone(),
                locked: Some(false),
            },
        )
        .await
        .expect("reassign");

        let asg = &svc.get_detail(&run_id).await.unwrap().assignments[0];
        assert_eq!(asg.source, "override");
        assert!(!asg.locked, "explicit locked=false must not lock");
    }

    // (d) re-plan does NOT change a locked assignment. (The locked-skip guard in
    // `plan` is exercised here for the original task; in the current flow `plan`
    // also mints fresh tasks, so the locked task is preserved either way — this
    // asserts the user's override survives a re-plan regardless.)
    #[tokio::test]
    async fn replan_does_not_touch_locked_assignment() {
        let members = vec![
            member_input("agent_a", &["coding"], "high", "standard"),
            member_input("agent_b", &["writing"], "medium", "standard"),
        ];
        let dag = single_task_dag(None, Some(coding_profile()));
        let (svc, _repo, snapshot, run_id) = harness(members, dag).await;
        svc.plan(&run_id).await.expect("first plan");
        let task_id = svc.get_detail(&run_id).await.unwrap().tasks[0].id.clone();

        // Lock the task to member 1 (the writer — NOT what the router would pick).
        svc.reassign(
            &run_id,
            &task_id,
            ReassignRequest {
                member_id: snapshot[1].id.clone(),
                locked: Some(true),
            },
        )
        .await
        .expect("reassign");

        // Re-plan: the locked task must keep its override.
        svc.plan(&run_id).await.expect("re-plan");

        // Find the assignment for the original (locked) task.
        let detail = svc.get_detail(&run_id).await.expect("detail");
        let locked_asg = detail
            .assignments
            .iter()
            .find(|a| a.task_id == task_id)
            .expect("locked task assignment");
        assert_eq!(
            locked_asg.member_id, snapshot[1].id,
            "locked assignment must survive re-plan"
        );
        assert_eq!(locked_asg.source, "override");
        assert!(locked_asg.locked);
    }

    // re-plan appends a fresh task DAG (plan mints new task ids each call); the
    // prior plan's tasks + assignments are left intact. This asserts a re-plan
    // does not retroactively rewrite the earlier auto assignment.
    #[tokio::test]
    async fn replan_appends_without_rewriting_prior_assignment() {
        let members = vec![member_input("agent_a", &["coding"], "high", "standard")];
        let dag = single_task_dag(None, Some(coding_profile()));
        let (svc, _repo, snapshot, run_id) = harness(members, dag).await;
        svc.plan(&run_id).await.expect("first plan");
        let first = svc.get_detail(&run_id).await.expect("detail");
        let first_task = first.tasks[0].id.clone();
        assert_eq!(first.assignments.len(), 1);

        svc.plan(&run_id).await.expect("re-plan");

        let detail = svc.get_detail(&run_id).await.expect("detail");
        // Each task carries exactly one auto assignment (no per-task stacking).
        let first_assigns: Vec<_> = detail
            .assignments
            .iter()
            .filter(|a| a.task_id == first_task)
            .collect();
        assert_eq!(first_assigns.len(), 1, "prior task keeps a single assignment");
        assert_eq!(first_assigns[0].member_id, snapshot[0].id);
        assert_eq!(first_assigns[0].source, "auto");
    }

    // (e) get_detail returns fleet_members decoded from the run's snapshot.
    #[tokio::test]
    async fn get_detail_returns_fleet_members() {
        let members = vec![
            member_input("agent_a", &["coding"], "high", "premium"),
            member_input("agent_b", &["writing"], "medium", "economy"),
        ];
        let dag = single_task_dag(None, None);
        let (svc, _repo, _snapshot, run_id) = harness(members, dag).await;

        let detail = svc.get_detail(&run_id).await.expect("detail");
        assert_eq!(detail.fleet_members.len(), 2, "snapshot members surfaced");
        assert_eq!(detail.fleet_members[0].agent_id, "agent_a");
        assert_eq!(detail.fleet_members[1].agent_id, "agent_b");
        // Capability profile survived the snapshot round-trip.
        let cap0 = detail.fleet_members[0]
            .capability_profile
            .as_ref()
            .expect("cap profile");
        assert_eq!(cap0.strengths, vec!["coding"]);
    }

    // reassign rejects a member that is not in the run's fleet snapshot (400).
    #[tokio::test]
    async fn reassign_unknown_member_is_bad_request() {
        let members = vec![member_input("agent_a", &["coding"], "high", "standard")];
        let dag = single_task_dag(None, Some(coding_profile()));
        let (svc, _repo, _snapshot, run_id) = harness(members, dag).await;
        svc.plan(&run_id).await.expect("plan");
        let task_id = svc.get_detail(&run_id).await.unwrap().tasks[0].id.clone();

        let err = svc
            .reassign(
                &run_id,
                &task_id,
                ReassignRequest {
                    member_id: "fmem_not_real".to_string(),
                    locked: None,
                },
            )
            .await
            .expect_err("unknown member must error");
        assert!(matches!(err, AppError::BadRequest(_)), "got: {err:?}");
    }

    // -------------------------------------------------------------------------
    // P3b: autonomy gate (plan's next-status decision) + pause/resume/approve.
    // -------------------------------------------------------------------------

    /// Build a harness with an explicit `autonomy` on the created run (the default
    /// harness leaves autonomy None → "supervised"). Returns (svc, run_id).
    async fn harness_with_autonomy(autonomy: &str) -> (RunService, String) {
        let db = init_database_memory().await.expect("db init");
        let pool = db.pool().clone();
        let run_repo = Arc::new(SqliteRunRepository::new(pool.clone()));
        let fleet_repo = Arc::new(SqliteFleetRepository::new(pool.clone()));
        let ws_repo = Arc::new(SqliteOrchWorkspaceRepository::new(pool.clone()));
        let emitter = OrchestratorRunEventEmitter::new(Arc::new(NoopBroadcaster));
        let planner: Arc<dyn PlanProducer> =
            Arc::new(FixedPlanProducer::new(single_task_dag(None, Some(coding_profile()))));
        let svc = RunService::new(
            run_repo,
            fleet_repo.clone(),
            ws_repo.clone(),
            planner,
            emitter,
        );
        let fleet = FleetService::new(fleet_repo)
            .create(
                "u1",
                CreateFleetRequest {
                    name: "auto fleet".to_string(),
                    description: None,
                    max_parallel: None,
                    members: vec![member_input("agent_a", &["coding"], "high", "standard")],
                },
            )
            .await
            .expect("fleet create");
        let ws = WorkspaceService::new(ws_repo)
            .create(
                "u1",
                CreateWorkspaceRequest {
                    name: "auto ws".to_string(),
                    default_fleet_id: Some(fleet.id.clone()),
                    workspace_dir: None,
                },
            )
            .await
            .expect("ws create");
        let run = svc
            .create(
                "u1",
                CreateRunRequest {
                    workspace_id: ws.id,
                    goal: "do the thing".to_string(),
                    fleet_id: fleet.id,
                    autonomy: Some(autonomy.to_string()),
                    max_parallel: None,
                },
            )
            .await
            .expect("run create");
        (svc, run.id)
    }

    // (a) An `interactive` run parks at `awaiting_plan_approval` after plan (NOT
    // running), and `approve_plan` flips it to `running`.
    #[tokio::test]
    async fn interactive_run_parks_then_approves_to_running() {
        let (svc, run_id) = harness_with_autonomy("interactive").await;
        svc.plan(&run_id).await.expect("plan");
        let detail = svc.get_detail(&run_id).await.expect("detail");
        assert_eq!(
            detail.run.status, "awaiting_plan_approval",
            "interactive run must park at awaiting_plan_approval after plan"
        );

        svc.approve_plan(&run_id).await.expect("approve");
        let detail = svc.get_detail(&run_id).await.expect("detail");
        assert_eq!(detail.run.status, "running", "approve flips to running");
    }

    // A `supervised` (default) / `autonomous` run flips straight to `running`
    // after plan (no approval gate).
    #[tokio::test]
    async fn non_interactive_run_is_running_after_plan() {
        for autonomy in ["supervised", "autonomous"] {
            let (svc, run_id) = harness_with_autonomy(autonomy).await;
            svc.plan(&run_id).await.expect("plan");
            let detail = svc.get_detail(&run_id).await.expect("detail");
            assert_eq!(detail.run.status, "running", "{autonomy} run runs after plan");
        }
    }

    // approve_plan rejects a run that is NOT awaiting approval (e.g. already
    // running) with a 400.
    #[tokio::test]
    async fn approve_plan_rejects_non_awaiting_run() {
        let (svc, run_id) = harness_with_autonomy("supervised").await;
        svc.plan(&run_id).await.expect("plan"); // → running
        let err = svc.approve_plan(&run_id).await.expect_err("must reject");
        assert!(matches!(err, AppError::BadRequest(_)), "got: {err:?}");
    }

    // (b) pause: running → paused; resume: paused → running. Each emits.
    #[tokio::test]
    async fn pause_then_resume_round_trips_status() {
        let (svc, run_id) = harness_with_autonomy("supervised").await;
        svc.plan(&run_id).await.expect("plan"); // → running

        svc.pause(&run_id).await.expect("pause");
        assert_eq!(
            svc.get_detail(&run_id).await.unwrap().run.status,
            "paused",
            "pause sets paused"
        );

        svc.resume(&run_id).await.expect("resume");
        assert_eq!(
            svc.get_detail(&run_id).await.unwrap().run.status,
            "running",
            "resume sets running"
        );
    }

    // pause rejects a run that is not running (e.g. still planning); resume
    // rejects a run that is not paused. Both 400.
    #[tokio::test]
    async fn pause_resume_reject_wrong_state() {
        let (svc, run_id) = harness_with_autonomy("supervised").await;
        // Before plan the run is `planning` — pause must reject.
        let err = svc.pause(&run_id).await.expect_err("pause on planning must reject");
        assert!(matches!(err, AppError::BadRequest(_)), "got: {err:?}");

        svc.plan(&run_id).await.expect("plan"); // → running
        // running is not paused — resume must reject.
        let err = svc.resume(&run_id).await.expect_err("resume on running must reject");
        assert!(matches!(err, AppError::BadRequest(_)), "got: {err:?}");
    }

    // The lifecycle controls 404 on a missing run.
    #[tokio::test]
    async fn lifecycle_controls_404_on_missing_run() {
        let (svc, _run_id) = harness_with_autonomy("supervised").await;
        let missing = "run_does_not_exist";
        assert!(
            matches!(svc.approve_plan(missing).await, Err(AppError::NotFound(_))),
            "approve_plan must 404 on missing run"
        );
        assert!(
            matches!(svc.pause(missing).await, Err(AppError::NotFound(_))),
            "pause must 404 on missing run"
        );
        assert!(
            matches!(svc.resume(missing).await, Err(AppError::NotFound(_))),
            "resume must 404 on missing run"
        );
    }

    // -------------------------------------------------------------------------
    // Task 2: ad-hoc (workspace-less) run creation from a model range.
    // -------------------------------------------------------------------------

    /// Build a bare RunService (no fleet/workspace seeded) for the ad-hoc path:
    /// `create_adhoc` synthesizes its members from a model range, so it needs
    /// neither a fleet nor a workspace. Returns (svc, run_repo).
    async fn adhoc_service() -> (RunService, Arc<SqliteRunRepository>) {
        let db = init_database_memory().await.expect("db init");
        let pool = db.pool().clone();
        let run_repo = Arc::new(SqliteRunRepository::new(pool.clone()));
        let fleet_repo = Arc::new(SqliteFleetRepository::new(pool.clone()));
        let ws_repo = Arc::new(SqliteOrchWorkspaceRepository::new(pool.clone()));
        let emitter = OrchestratorRunEventEmitter::new(Arc::new(NoopBroadcaster));
        let planner: Arc<dyn PlanProducer> =
            Arc::new(FixedPlanProducer::new(single_task_dag(None, None)));
        let svc = RunService::new(run_repo.clone(), fleet_repo, ws_repo, planner, emitter);
        (svc, run_repo)
    }

    fn model_ref(provider_id: &str, model: &str) -> ModelRef {
        ModelRef {
            provider_id: provider_id.to_string(),
            model: model.to_string(),
        }
    }

    // (a) create_adhoc with a `range` of two models persists the run, snapshots
    // exactly two synthetic members (each with provider_id + model both Some),
    // and persists work_dir + lead_conv_id.
    #[tokio::test]
    async fn create_adhoc_range_snapshots_two_members() {
        let (svc, _repo) = adhoc_service().await;
        let req = CreateAdhocRunRequest {
            goal: "ship the feature".to_string(),
            work_dir: Some("/tmp/proj".to_string()),
            model_range: ModelRange::Range {
                models: vec![model_ref("prov_a", "model-a"), model_ref("prov_b", "model-b")],
            },
            pinned_roles: vec![],
            role_members: vec![],
            autonomy: None,
            max_parallel: Some(2),
            lead_conv_id: Some(909),
        };

        let run = svc.create_adhoc("u1", req).await.expect("create_adhoc");

        // Ad-hoc run is workspace-less, carries its own work_dir + lead_conv_id.
        assert!(run.workspace_id.is_none(), "ad-hoc run has no workspace");
        assert_eq!(run.work_dir.as_deref(), Some("/tmp/proj"), "work_dir persisted");
        assert_eq!(run.lead_conv_id, Some(909), "lead_conv_id persisted");
        assert_eq!(run.status, "planning", "starts in planning");
        // Autonomy defaulted (request omitted it).
        assert_eq!(run.autonomy, DEFAULT_AUTONOMY);
        assert_eq!(run.max_parallel, Some(2));

        // The fleet snapshot must decode to two members, each Nomi-runnable
        // (provider_id + model both Some — worker.rs:116-120 requires it).
        let detail = svc.get_detail(&run.id).await.expect("detail");
        assert_eq!(detail.fleet_members.len(), 2, "two synthetic members snapshotted");
        let m0 = &detail.fleet_members[0];
        let m1 = &detail.fleet_members[1];
        assert_eq!(m0.provider_id.as_deref(), Some("prov_a"));
        assert_eq!(m0.model.as_deref(), Some("model-a"));
        assert_eq!(m1.provider_id.as_deref(), Some("prov_b"));
        assert_eq!(m1.model.as_deref(), Some("model-b"));
        // Member ids must be unique + stable (the engine resolves task→member by id).
        assert_ne!(m0.id, m1.id, "synthetic member ids are unique");
        assert!(m0.id.starts_with("rmbr_"), "member id uses the rmbr prefix: {}", m0.id);
        // sort_order is the snapshot index.
        assert_eq!(m0.sort_order, 0);
        assert_eq!(m1.sort_order, 1);
    }

    // create_adhoc with a `single` model snapshots exactly one member.
    #[tokio::test]
    async fn create_adhoc_single_snapshots_one_member() {
        let (svc, _repo) = adhoc_service().await;
        let req = CreateAdhocRunRequest {
            goal: "single-model run".to_string(),
            work_dir: None,
            model_range: ModelRange::Single { model: model_ref("prov_solo", "model-solo") },
            pinned_roles: vec![],
            role_members: vec![],
            autonomy: Some("autonomous".to_string()),
            max_parallel: None,
            lead_conv_id: None,
        };

        let run = svc.create_adhoc("u1", req).await.expect("create_adhoc");
        assert!(run.work_dir.is_none(), "no work_dir given");
        assert_eq!(run.autonomy, "autonomous", "explicit autonomy honored");

        let detail = svc.get_detail(&run.id).await.expect("detail");
        assert_eq!(detail.fleet_members.len(), 1);
        assert_eq!(detail.fleet_members[0].provider_id.as_deref(), Some("prov_solo"));
        assert_eq!(detail.fleet_members[0].model.as_deref(), Some("model-solo"));
    }

    // (b) create_adhoc rejects an empty goal (400).
    #[tokio::test]
    async fn create_adhoc_empty_goal_is_bad_request() {
        let (svc, _repo) = adhoc_service().await;
        let req = CreateAdhocRunRequest {
            goal: "   ".to_string(),
            work_dir: None,
            model_range: ModelRange::Single { model: model_ref("p", "m") },
            pinned_roles: vec![],
            role_members: vec![],
            autonomy: None,
            max_parallel: None,
            lead_conv_id: None,
        };
        let err = svc.create_adhoc("u1", req).await.expect_err("empty goal must reject");
        assert!(matches!(err, AppError::BadRequest(_)), "got: {err:?}");
    }

    // (b) create_adhoc rejects an empty `range` (no models → 400, no run persisted).
    #[tokio::test]
    async fn create_adhoc_empty_range_is_bad_request() {
        let (svc, _repo) = adhoc_service().await;
        let req = CreateAdhocRunRequest {
            goal: "needs a model".to_string(),
            work_dir: None,
            model_range: ModelRange::Range { models: vec![] },
            pinned_roles: vec![],
            role_members: vec![],
            autonomy: None,
            max_parallel: None,
            lead_conv_id: None,
        };
        let err = svc.create_adhoc("u1", req).await.expect_err("empty range must reject");
        assert!(matches!(err, AppError::BadRequest(_)), "got: {err:?}");
    }

    // (b) create_adhoc rejects an unexpanded `auto` range — Auto must be expanded
    // to a concrete `range` by the caps_orchestrator layer (Task 3) before
    // reaching the service. This is the contract Task 3 relies on.
    #[tokio::test]
    async fn create_adhoc_auto_range_is_bad_request() {
        let (svc, _repo) = adhoc_service().await;
        let req = CreateAdhocRunRequest {
            goal: "auto picks".to_string(),
            work_dir: None,
            model_range: ModelRange::Auto,
            pinned_roles: vec![],
            role_members: vec![],
            autonomy: None,
            max_parallel: None,
            lead_conv_id: None,
        };
        let err = svc.create_adhoc("u1", req).await.expect_err("auto must reject at service");
        assert!(matches!(err, AppError::BadRequest(_)), "got: {err:?}");
    }

    // (c) build_members_from_range unit coverage: Range → one member per ModelRef
    // (provider+model both Some, unique ids, sort_order = index); Single → one
    // member; Auto / empty Range → BadRequest (must be expanded/non-empty).
    #[test]
    fn build_members_from_range_unit() {
        // Range → two members.
        let members = build_members_from_range(&ModelRange::Range {
            models: vec![model_ref("p1", "m1"), model_ref("p2", "m2")],
        })
        .expect("range builds members");
        assert_eq!(members.len(), 2);
        assert_eq!(members[0].provider_id.as_deref(), Some("p1"));
        assert_eq!(members[0].model.as_deref(), Some("m1"));
        assert_eq!(members[0].sort_order, 0);
        assert_eq!(members[1].sort_order, 1);
        assert_ne!(members[0].id, members[1].id);
        assert!(members[0].agent_id.is_empty(), "synthetic member has no agent");

        // Single → one member.
        let single = build_members_from_range(&ModelRange::Single {
            model: model_ref("ps", "ms"),
        })
        .expect("single builds a member");
        assert_eq!(single.len(), 1);
        assert_eq!(single[0].provider_id.as_deref(), Some("ps"));

        // Empty Range → BadRequest.
        let empty = build_members_from_range(&ModelRange::Range { models: vec![] });
        assert!(matches!(empty, Err(AppError::BadRequest(_))), "got: {empty:?}");

        // Auto → BadRequest (caller must expand it).
        let auto = build_members_from_range(&ModelRange::Auto);
        assert!(matches!(auto, Err(AppError::BadRequest(_))), "got: {auto:?}");
    }

    // -------------------------------------------------------------------------
    // P4 Task 2: role_members merge (assistant-backed members in the snapshot).
    // -------------------------------------------------------------------------

    /// An enriched (assistant-backed) member, as the caps_orchestrator layer
    /// would construct it.
    fn enriched_member(agent_id: &str, provider_id: &str, model: &str, name: &str) -> FleetMember {
        FleetMember {
            id: generate_prefixed_id("rmbr"),
            agent_id: agent_id.to_string(),
            provider_id: Some(provider_id.to_string()),
            model: Some(model.to_string()),
            role_hint: Some(name.to_string()),
            capability_profile: None,
            constraints: None,
            sort_order: 0,
            description: Some(format!("{name} 的描述")),
            system_prompt: Some(format!("你是 {name}")),
            enabled_skills: vec!["web_search".to_string()],
            disabled_builtin_skills: vec!["browser".to_string()],
        }
    }

    // merge_members unit: role members first, bare range members appended; a true
    // duplicate `(provider, model, agent_id)` collapses (first wins); an
    // assistant on the same model but a non-empty agent_id is a DISTINCT member;
    // a role member with an EMPTY agent_id (a description-decorated bare member)
    // WINS over the plain range-built one; sort_order re-densified to the index.
    #[test]
    fn merge_members_dedups_and_densifies() {
        let range = build_members_from_range(&ModelRange::Range {
            models: vec![model_ref("p1", "m1"), model_ref("p2", "m2")],
        })
        .expect("range");
        let roles = vec![
            // Same (p1, m1) as a range member but with an agent_id → distinct.
            enriched_member("asst_a", "p1", "m1", "研究员"),
            // A second assistant on a fresh model.
            enriched_member("asst_b", "p3", "m3", "写手"),
        ];

        let merged = merge_members(range, roles);
        // 2 bare + 2 assistant = 4 distinct routing targets (no collapse, since
        // the assistant members carry agent_ids the bare ones lack).
        assert_eq!(merged.len(), 4, "bare + assistant members all kept: {merged:?}");
        // sort_order densified 0..4 in append order (roles first, then range).
        for (i, m) in merged.iter().enumerate() {
            assert_eq!(m.sort_order, i as i64, "sort_order re-densified");
        }
        // The assistant-backed members preserved their enrichment.
        let asst = merged.iter().find(|m| m.agent_id == "asst_a").expect("asst_a present");
        assert_eq!(asst.role_hint.as_deref(), Some("研究员"));
        assert_eq!(asst.system_prompt.as_deref(), Some("你是 研究员"));
        assert_eq!(asst.enabled_skills, vec!["web_search"]);

        // A true duplicate (identical triple) collapses, first occurrence wins.
        let dup_a = enriched_member("asst_a", "p1", "m1", "研究员");
        let dup_b = enriched_member("asst_a", "p1", "m1", "研究员-changed");
        let collapsed = merge_members(vec![], vec![dup_a.clone(), dup_b]);
        assert_eq!(collapsed.len(), 1, "identical (provider,model,agent) collapses");
        assert_eq!(collapsed[0].role_hint.as_deref(), Some("研究员"), "first wins");

        // A description-decorated bare role member (empty agent_id) WINS over the
        // plain range-built one with the same (provider, model).
        let mut decorated = build_members_from_range(&ModelRange::Single { model: model_ref("p9", "m9") })
            .expect("single")
            .remove(0);
        decorated.description = Some("用户描述".to_string());
        let plain_range = build_members_from_range(&ModelRange::Single { model: model_ref("p9", "m9") })
            .expect("single");
        let merged2 = merge_members(plain_range, vec![decorated]);
        assert_eq!(merged2.len(), 1, "same (provider,model,empty agent) collapses to one");
        assert_eq!(
            merged2[0].description.as_deref(),
            Some("用户描述"),
            "the description-decorated role copy wins over the plain range member"
        );
    }

    // (KEYSTONE) create_adhoc with role_members merges the assistant-backed
    // member INTO the snapshot alongside the bare range member, preserving the
    // assistant's agent_id / role_hint / system_prompt / enabled_skills /
    // description. This is what lets the worker (Task 3) read a self-contained
    // persona from the snapshot with no assistant-crate dependency.
    #[tokio::test]
    async fn create_adhoc_merges_role_members_into_snapshot() {
        let (svc, _repo) = adhoc_service().await;
        let req = CreateAdhocRunRequest {
            goal: "build it".to_string(),
            work_dir: None,
            model_range: ModelRange::Range { models: vec![model_ref("p1", "m1")] },
            pinned_roles: vec![],
            role_members: vec![enriched_member("asst_research", "p2", "m2", "研究员")],
            autonomy: None,
            max_parallel: None,
            lead_conv_id: None,
        };
        let run = svc.create_adhoc("u1", req).await.expect("create_adhoc");

        let detail = svc.get_detail(&run.id).await.expect("detail");
        // 1 bare range member + 1 assistant member.
        assert_eq!(detail.fleet_members.len(), 2, "range + role member snapshotted");
        let asst = detail
            .fleet_members
            .iter()
            .find(|m| m.agent_id == "asst_research")
            .expect("assistant member present in snapshot");
        assert_eq!(asst.provider_id.as_deref(), Some("p2"));
        assert_eq!(asst.model.as_deref(), Some("m2"));
        assert_eq!(asst.role_hint.as_deref(), Some("研究员"));
        assert_eq!(asst.system_prompt.as_deref(), Some("你是 研究员"), "persona folded in");
        assert_eq!(asst.enabled_skills, vec!["web_search"], "skills folded in");
        assert_eq!(asst.disabled_builtin_skills, vec!["browser"]);
        assert_eq!(asst.description.as_deref(), Some("研究员 的描述"), "description folded in");

        // The bare range member is still there (no agent_id), unchanged.
        let bare = detail
            .fleet_members
            .iter()
            .find(|m| m.agent_id.is_empty())
            .expect("bare range member present");
        assert_eq!(bare.provider_id.as_deref(), Some("p1"));
        assert!(bare.system_prompt.is_none(), "bare member has no persona");
    }

    // (P5 Task 1, c) plan() persists each planned task's `role` onto the task row,
    // so a later precipitation UI can read the roles a run used. A planned task
    // without a role stays NULL (back-compat).
    #[tokio::test]
    async fn plan_persists_task_role() {
        let members = vec![member_input("agent_a", &["coding"], "high", "standard")];
        // Two tasks: one carries a role, one leaves it absent.
        let dag = PlannedDag {
            tasks: vec![
                PlannedTask {
                    title: "前端任务".to_string(),
                    spec: "实现页面".to_string(),
                    task_profile: None,
                    depends_on: vec![],
                    member_index: Some(0),
                    rationale: None,
                    role: Some("前端".to_string()),
                },
                PlannedTask {
                    title: "无角色任务".to_string(),
                    spec: "做点别的".to_string(),
                    task_profile: None,
                    depends_on: vec![],
                    member_index: Some(0),
                    rationale: None,
                    role: None,
                },
            ],
        };
        let (svc, _repo, _snapshot, run_id) = harness(members, dag).await;

        svc.plan(&run_id).await.expect("plan");

        let detail = svc.get_detail(&run_id).await.expect("detail");
        assert_eq!(detail.tasks.len(), 2);
        let fe = detail
            .tasks
            .iter()
            .find(|t| t.title == "前端任务")
            .expect("frontend task present");
        assert_eq!(fe.role.as_deref(), Some("前端"), "planned role must be persisted");
        let none_task = detail
            .tasks
            .iter()
            .find(|t| t.title == "无角色任务")
            .expect("roleless task present");
        assert_eq!(none_task.role, None, "absent role stays NULL");
    }

    // -------------------------------------------------------------------------
    // P1 Task 1: Run delete (owner-scoped, cascade) + rename (owner-scoped goal).
    // -------------------------------------------------------------------------

    // delete: the owner can delete a run; the whole aggregate (tasks + deps +
    // assignments) cascades out and `get_detail` then 404s.
    #[tokio::test]
    async fn delete_removes_run_and_cascades_for_owner() {
        let members = vec![member_input("agent_a", &["coding"], "high", "standard")];
        // Two tasks so there is a dep edge + two assignments to cascade.
        let dag = PlannedDag {
            tasks: vec![
                PlannedTask {
                    title: "A".to_string(),
                    spec: "a".to_string(),
                    task_profile: None,
                    depends_on: vec![],
                    member_index: Some(0),
                    rationale: None,
                    role: None,
                },
                PlannedTask {
                    title: "B".to_string(),
                    spec: "b".to_string(),
                    task_profile: None,
                    depends_on: vec![0],
                    member_index: Some(0),
                    rationale: None,
                    role: None,
                },
            ],
        };
        let (svc, _repo, _snapshot, run_id) = harness(members, dag).await;
        svc.plan(&run_id).await.expect("plan");

        // Pre-condition: tasks + deps + assignments exist.
        let before = svc.get_detail(&run_id).await.expect("detail");
        assert_eq!(before.tasks.len(), 2);
        assert_eq!(before.deps.len(), 1);
        assert_eq!(before.assignments.len(), 2);

        // Owner deletes → ok.
        svc.delete("u1", &run_id).await.expect("owner delete");

        // The run is gone (get_detail 404s) — the cascade is asserted at the repo
        // layer; here we prove the service path removed the row.
        assert!(
            matches!(svc.get_detail(&run_id).await, Err(AppError::NotFound(_))),
            "deleted run must 404 on get_detail"
        );
    }

    // delete: a non-owner is rejected with 403 (Forbidden) and the run survives.
    #[tokio::test]
    async fn delete_cross_user_is_forbidden() {
        let members = vec![member_input("agent_a", &["coding"], "high", "standard")];
        let dag = single_task_dag(Some(0), None);
        let (svc, _repo, _snapshot, run_id) = harness(members, dag).await; // owned by u1
        svc.plan(&run_id).await.expect("plan");

        let err = svc.delete("intruder", &run_id).await.expect_err("cross-user delete must reject");
        assert!(matches!(err, AppError::Forbidden(_)), "cross-user delete is 403, got: {err:?}");

        // The run is untouched.
        assert!(svc.get_detail(&run_id).await.is_ok(), "non-owner delete must not remove the run");
    }

    // delete: a missing run 404s.
    #[tokio::test]
    async fn delete_missing_run_is_not_found() {
        let members = vec![member_input("agent_a", &["coding"], "high", "standard")];
        let (svc, _repo, _snapshot, _run_id) = harness(members, single_task_dag(None, None)).await;
        let err = svc.delete("u1", "run_missing").await.expect_err("missing run must 404");
        assert!(matches!(err, AppError::NotFound(_)), "got: {err:?}");
    }

    // rename: the owner can change a run's goal; get_detail reflects the new goal.
    #[tokio::test]
    async fn rename_updates_goal_for_owner() {
        let members = vec![member_input("agent_a", &["coding"], "high", "standard")];
        let (svc, _repo, _snapshot, run_id) = harness(members, single_task_dag(None, None)).await;
        // Sanity: the harness seeds goal "do the thing".
        assert_eq!(svc.get_detail(&run_id).await.unwrap().run.goal, "do the thing");

        svc.rename("u1", &run_id, "全新目标").await.expect("owner rename");

        let detail = svc.get_detail(&run_id).await.expect("detail");
        assert_eq!(detail.run.goal, "全新目标", "goal rewritten by rename");
    }

    // rename: a non-owner is rejected with 403 and the goal is unchanged.
    #[tokio::test]
    async fn rename_cross_user_is_forbidden() {
        let members = vec![member_input("agent_a", &["coding"], "high", "standard")];
        let (svc, _repo, _snapshot, run_id) = harness(members, single_task_dag(None, None)).await;

        let err = svc
            .rename("intruder", &run_id, "盗改目标")
            .await
            .expect_err("cross-user rename must reject");
        assert!(matches!(err, AppError::Forbidden(_)), "cross-user rename is 403, got: {err:?}");

        assert_eq!(
            svc.get_detail(&run_id).await.unwrap().run.goal,
            "do the thing",
            "non-owner rename must not change the goal"
        );
    }

    // rename: an empty goal is a 400 (a run goal must not be blank); a missing run 404s.
    #[tokio::test]
    async fn rename_rejects_empty_goal_and_missing_run() {
        let members = vec![member_input("agent_a", &["coding"], "high", "standard")];
        let (svc, _repo, _snapshot, run_id) = harness(members, single_task_dag(None, None)).await;

        let err = svc.rename("u1", &run_id, "   ").await.expect_err("empty goal must reject");
        assert!(matches!(err, AppError::BadRequest(_)), "empty goal is 400, got: {err:?}");

        let err = svc.rename("u1", "run_missing", "x").await.expect_err("missing run must 404");
        assert!(matches!(err, AppError::NotFound(_)), "got: {err:?}");
    }

    // -------------------------------------------------------------------------
    // P1 Task 2: Run replan (clear old plan + re-decompose against a new goal).
    // -------------------------------------------------------------------------

    /// A planner that, on each `produce` call, returns the dag at the current head
    /// of a queue (consuming it), so a test can stage DIFFERENT dags for the
    /// initial plan vs the replan and prove the second plan was re-decomposed
    /// rather than appended. Once the queue is exhausted it keeps returning the
    /// last dag (so it never panics if called more than expected).
    struct QueuedPlanProducer(Mutex<std::collections::VecDeque<PlannedDag>>);
    impl QueuedPlanProducer {
        fn new(dags: Vec<PlannedDag>) -> Self {
            Self(Mutex::new(dags.into_iter().collect()))
        }
    }
    #[async_trait::async_trait]
    impl PlanProducer for QueuedPlanProducer {
        async fn produce(
            &self,
            _goal: &str,
            _members: &[FleetMember],
        ) -> Result<PlannedDag, AppError> {
            let mut q = self.0.lock().unwrap();
            if q.len() > 1 {
                Ok(q.pop_front().unwrap())
            } else {
                // Keep the last staged dag available for any extra call.
                Ok(q.front().cloned().unwrap_or(PlannedDag { tasks: vec![] }))
            }
        }
    }

    /// A multi-task dag with the given titles (each a single independent task,
    /// pre-assigned to member 0), so a test can count + name the re-decomposed
    /// tasks after a replan.
    fn dag_with_titles(titles: &[&str]) -> PlannedDag {
        PlannedDag {
            tasks: titles
                .iter()
                .map(|t| PlannedTask {
                    title: (*t).to_string(),
                    spec: format!("spec-{t}"),
                    task_profile: None,
                    depends_on: vec![],
                    member_index: Some(0),
                    rationale: None,
                    role: None,
                })
                .collect(),
        }
    }

    /// Build a RunService whose planner stages `dags` in order (initial plan +
    /// later replans), seed a workspace + single-member fleet, create a run with
    /// the given autonomy, and return (svc, run_id). The run is left in `planning`.
    async fn replan_harness(
        autonomy: &str,
        dags: Vec<PlannedDag>,
    ) -> (RunService, String) {
        let db = init_database_memory().await.expect("db init");
        let pool = db.pool().clone();
        let run_repo = Arc::new(SqliteRunRepository::new(pool.clone()));
        let fleet_repo = Arc::new(SqliteFleetRepository::new(pool.clone()));
        let ws_repo = Arc::new(SqliteOrchWorkspaceRepository::new(pool.clone()));
        let emitter = OrchestratorRunEventEmitter::new(Arc::new(NoopBroadcaster));
        let planner: Arc<dyn PlanProducer> = Arc::new(QueuedPlanProducer::new(dags));
        let svc = RunService::new(run_repo, fleet_repo.clone(), ws_repo.clone(), planner, emitter);
        let fleet = FleetService::new(fleet_repo)
            .create(
                "u1",
                CreateFleetRequest {
                    name: "replan fleet".to_string(),
                    description: None,
                    max_parallel: None,
                    members: vec![member_input("agent_a", &["coding"], "high", "standard")],
                },
            )
            .await
            .expect("fleet create");
        let ws = WorkspaceService::new(ws_repo)
            .create(
                "u1",
                CreateWorkspaceRequest {
                    name: "replan ws".to_string(),
                    default_fleet_id: Some(fleet.id.clone()),
                    workspace_dir: None,
                },
            )
            .await
            .expect("ws create");
        let run = svc
            .create(
                "u1",
                CreateRunRequest {
                    workspace_id: ws.id,
                    goal: "do the thing".to_string(),
                    fleet_id: fleet.id,
                    autonomy: Some(autonomy.to_string()),
                    max_parallel: None,
                },
            )
            .await
            .expect("run create");
        (svc, run.id)
    }

    // replan with a NEW goal clears the old plan's tasks and re-decomposes against
    // the new goal: after replan the run carries ONLY the new dag's tasks (the old
    // task ids are gone, not appended), the goal is rewritten, and a supervised run
    // is back to `running`.
    #[tokio::test]
    async fn replan_clears_old_tasks_and_redecomposes_against_new_goal() {
        // Initial plan = 1 task ("旧"); replan = 2 tasks ("新A","新B").
        let (svc, run_id) = replan_harness(
            "supervised",
            vec![dag_with_titles(&["旧"]), dag_with_titles(&["新A", "新B"])],
        )
        .await;
        svc.plan(&run_id).await.expect("first plan");
        let before = svc.get_detail(&run_id).await.expect("detail");
        assert_eq!(before.tasks.len(), 1, "initial plan has one task");
        let old_task_id = before.tasks[0].id.clone();

        svc.replan(
            "u1",
            &run_id,
            ReplanRequest {
                goal: Some("全新目标".to_string()),
                model_range: None,
                autonomy: None,
                pinned_roles: vec![],
            },
        )
        .await
        .expect("replan");

        let after = svc.get_detail(&run_id).await.expect("detail");
        // Only the new dag's tasks remain (old plan cleared, not appended).
        assert_eq!(after.tasks.len(), 2, "replan re-decomposed to two tasks (no append)");
        assert!(
            !after.tasks.iter().any(|t| t.id == old_task_id),
            "the old plan's task must be cleared, not retained"
        );
        let titles: Vec<&str> = after.tasks.iter().map(|t| t.title.as_str()).collect();
        assert!(titles.contains(&"新A") && titles.contains(&"新B"), "new tasks present: {titles:?}");
        // The goal was rewritten and a supervised run re-armed to `running`.
        assert_eq!(after.run.goal, "全新目标", "goal updated by replan");
        assert_eq!(after.run.status, "running", "supervised replan re-arms to running");
    }

    // replan of an INTERACTIVE run parks it back at `awaiting_plan_approval` after
    // re-decomposition (the autonomy gate in plan() applies); old tasks cleared.
    #[tokio::test]
    async fn replan_interactive_parks_at_awaiting_plan_approval() {
        let (svc, run_id) = replan_harness(
            "interactive",
            vec![dag_with_titles(&["旧"]), dag_with_titles(&["新"])],
        )
        .await;
        // First plan parks interactive at awaiting_plan_approval.
        svc.plan(&run_id).await.expect("first plan");
        assert_eq!(
            svc.get_detail(&run_id).await.unwrap().run.status,
            "awaiting_plan_approval"
        );

        svc.replan(
            "u1",
            &run_id,
            ReplanRequest { goal: None, model_range: None, autonomy: None, pinned_roles: vec![] },
        )
        .await
        .expect("replan");

        let after = svc.get_detail(&run_id).await.expect("detail");
        assert_eq!(after.tasks.len(), 1, "re-decomposed to the new single task");
        assert_eq!(after.tasks[0].title, "新", "new dag's task present");
        assert_eq!(
            after.run.status, "awaiting_plan_approval",
            "interactive replan re-parks at the approval gate"
        );
    }

    // replan with a NEW autonomy switches the run's gate: a supervised run replanned
    // with autonomy=interactive must park at awaiting_plan_approval (the persisted
    // autonomy is updated BEFORE plan() reads it for the gate).
    #[tokio::test]
    async fn replan_with_new_autonomy_switches_gate() {
        let (svc, run_id) = replan_harness(
            "supervised",
            vec![dag_with_titles(&["旧"]), dag_with_titles(&["新"])],
        )
        .await;
        svc.plan(&run_id).await.expect("first plan");
        assert_eq!(svc.get_detail(&run_id).await.unwrap().run.status, "running");

        svc.replan(
            "u1",
            &run_id,
            ReplanRequest {
                goal: None,
                model_range: None,
                autonomy: Some("interactive".to_string()),
                pinned_roles: vec![],
            },
        )
        .await
        .expect("replan");

        let after = svc.get_detail(&run_id).await.expect("detail");
        assert_eq!(after.run.autonomy, "interactive", "autonomy switched by replan");
        assert_eq!(
            after.run.status, "awaiting_plan_approval",
            "the new interactive gate applies after replan"
        );
    }

    // replan with a NEW model_range rebuilds the run's fleet snapshot (reusing
    // build_members_from_range): after replan the snapshot carries exactly the new
    // range's members. Old tasks are cleared and re-decomposed against the new fleet.
    #[tokio::test]
    async fn replan_with_model_range_rebuilds_fleet_snapshot() {
        let (svc, run_id) = replan_harness(
            "supervised",
            vec![dag_with_titles(&["旧"]), dag_with_titles(&["新"])],
        )
        .await;
        svc.plan(&run_id).await.expect("first plan");
        // The initial snapshot is the seeded single-member fleet.
        let before = svc.get_detail(&run_id).await.expect("detail");
        assert_eq!(before.fleet_members.len(), 1, "initial snapshot: one fleet member");

        svc.replan(
            "u1",
            &run_id,
            ReplanRequest {
                goal: None,
                model_range: Some(ModelRange::Range {
                    models: vec![model_ref("p_new1", "m1"), model_ref("p_new2", "m2")],
                }),
                autonomy: None,
                pinned_roles: vec![],
            },
        )
        .await
        .expect("replan");

        let after = svc.get_detail(&run_id).await.expect("detail");
        // The snapshot was rebuilt from the new range: two synthetic members.
        assert_eq!(after.fleet_members.len(), 2, "snapshot rebuilt from new model_range");
        let providers: Vec<&str> = after
            .fleet_members
            .iter()
            .filter_map(|m| m.provider_id.as_deref())
            .collect();
        assert!(providers.contains(&"p_new1") && providers.contains(&"p_new2"), "new providers: {providers:?}");
    }

    // replan is owner-scoped: a non-owner is rejected (403) and the run's plan is
    // left intact (old tasks NOT cleared).
    #[tokio::test]
    async fn replan_cross_user_is_forbidden() {
        let (svc, run_id) = replan_harness(
            "supervised",
            vec![dag_with_titles(&["旧"]), dag_with_titles(&["新A", "新B"])],
        )
        .await;
        svc.plan(&run_id).await.expect("first plan");
        let before = svc.get_detail(&run_id).await.expect("detail");
        assert_eq!(before.tasks.len(), 1);

        let err = svc
            .replan(
                "intruder",
                &run_id,
                ReplanRequest { goal: Some("盗改".to_string()), model_range: None, autonomy: None, pinned_roles: vec![] },
            )
            .await
            .expect_err("cross-user replan must reject");
        assert!(matches!(err, AppError::Forbidden(_)), "cross-user replan is 403, got: {err:?}");

        // The run's plan + goal are untouched.
        let after = svc.get_detail(&run_id).await.expect("detail");
        assert_eq!(after.tasks.len(), 1, "non-owner replan must not clear the plan");
        assert_eq!(after.run.goal, "do the thing", "goal unchanged");
    }

    // replan 404s on a missing run.
    #[tokio::test]
    async fn replan_missing_run_is_not_found() {
        let (svc, _run_id) = replan_harness("supervised", vec![dag_with_titles(&["x"])]).await;
        let err = svc
            .replan(
                "u1",
                "run_missing",
                ReplanRequest { goal: None, model_range: None, autonomy: None, pinned_roles: vec![] },
            )
            .await
            .expect_err("missing run must 404");
        assert!(matches!(err, AppError::NotFound(_)), "got: {err:?}");
    }

    // ── T3: per-task timestamps + run workspace browse ──────────────────────

    // get_detail's RunTask DTOs carry created_at/updated_at (epoch ms) — the
    // pacing data the roster/inspector surface. A freshly planned task has both
    // populated (> 0).
    #[tokio::test]
    async fn get_detail_tasks_carry_timestamps() {
        let (svc, run_id) = replan_harness("supervised", vec![dag_with_titles(&["t"])]).await;
        svc.plan(&run_id).await.expect("plan");
        let detail = svc.get_detail(&run_id).await.expect("detail");
        assert_eq!(detail.tasks.len(), 1);
        assert!(detail.tasks[0].created_at > 0, "created_at populated");
        assert!(detail.tasks[0].updated_at > 0, "updated_at populated");
    }

    /// Build an ad-hoc run rooted at `work_dir` (or none) and return (svc, run).
    async fn adhoc_run_with_dir(work_dir: Option<String>) -> (RunService, Run) {
        let (svc, _repo) = adhoc_service().await;
        let req = CreateAdhocRunRequest {
            goal: "browse run".to_string(),
            work_dir,
            model_range: ModelRange::Single { model: model_ref("p", "m") },
            pinned_roles: vec![],
            role_members: vec![],
            autonomy: Some("supervised".to_string()),
            max_parallel: None,
            lead_conv_id: None,
        };
        let run = svc.create_adhoc("u1", req).await.expect("create_adhoc");
        (svc, run)
    }

    // browse_workspace lists one directory level under an ad-hoc run's work_dir
    // (owner-scoped). Seeds a temp dir with a file and asserts it is listed.
    #[tokio::test]
    async fn browse_workspace_lists_files_for_owner() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join("hello.txt"), b"hi").expect("write file");
        let (svc, run) = adhoc_run_with_dir(Some(dir.path().to_string_lossy().into_owned())).await;

        let entries = svc.browse_workspace("u1", &run.id, "", None).await.expect("browse");
        assert!(
            entries.iter().any(|e| e.name == "hello.txt" && e.entry_type == "file"),
            "expected hello.txt in {entries:?}"
        );
    }

    // browse_workspace is owner-scoped: a non-owner is rejected (403).
    #[tokio::test]
    async fn browse_workspace_cross_user_is_forbidden() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (svc, run) = adhoc_run_with_dir(Some(dir.path().to_string_lossy().into_owned())).await;
        let err = svc
            .browse_workspace("intruder", &run.id, "", None)
            .await
            .expect_err("cross-user browse must reject");
        assert!(matches!(err, AppError::Forbidden(_)), "got: {err:?}");
    }

    // browse_workspace 404s on a missing run.
    #[tokio::test]
    async fn browse_workspace_missing_run_is_not_found() {
        let (svc, _repo) = adhoc_service().await;
        let err = svc
            .browse_workspace("u1", "run_missing", "", None)
            .await
            .expect_err("missing run must 404");
        assert!(matches!(err, AppError::NotFound(_)), "got: {err:?}");
    }

    // A run with neither a work_dir nor a workspace dir has nothing to browse →
    // BadRequest (400). An ad-hoc run with work_dir=None (no bound workspace) is
    // exactly such a run.
    #[tokio::test]
    async fn browse_workspace_without_dir_is_bad_request() {
        let (svc, run) = adhoc_run_with_dir(None).await;
        let err = svc
            .browse_workspace("u1", &run.id, "", None)
            .await
            .expect_err("no working dir must reject");
        assert!(matches!(err, AppError::BadRequest(_)), "got: {err:?}");
    }
}

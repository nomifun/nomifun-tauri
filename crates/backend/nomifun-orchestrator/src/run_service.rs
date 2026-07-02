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
    Assignment, CapabilityProfile, CreateAdhocRunRequest, CreateRunRequest, FleetMember, ModelRange,
    ModelRef, PlannedDag, PlannedTask, ReassignRequest, ReplanRequest, Run, RunDetail, RunTask,
    RunTaskDep, TaskProfile, WorkspaceEntry,
};
use nomifun_common::AppError;
use nomifun_common::generate_prefixed_id;
use nomifun_db::models::{
    FleetMemberRow, OrchAssignmentRow, OrchRunRow, OrchRunTaskDepRow, OrchRunTaskRow,
};
use nomifun_db::{
    CreateAssignmentParams, CreateRunParams, CreateTaskParams, IFleetRepository,
    IOrchWorkspaceRepository, IRunRepository, ReconcileDepRef, ReconcileNewTask, ReconcilePlan,
    UpdateRunParams, UpdateTaskParams,
};

use crate::error::OrchestratorError;
use crate::events::OrchestratorRunEventEmitter;
use crate::plan::PlanProducer;
use crate::router::rank_members;

/// Default autonomy when the create request omits it.
const DEFAULT_AUTONOMY: &str = "supervised";

/// Hard ceiling on the planner's single LLM completion (`plan()`). The planner
/// itself has no retry/timeout, so an unresponsive provider would otherwise leave
/// the run stuck in `planning` forever (a black hole). On elapse `plan()` returns a
/// `Timeout` error → `spawn_plan_and_start` fail-soft leaves the run re-plannable,
/// and the run-level stall watchdog reaps it if it stays stuck.
const PLAN_TIMEOUT_SECS: u64 = 300;

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
        // 主模型 as planner: when the request names a `lead_model` (the homepage
        // 主模型), float its fleet member to the front so `pick_lead` (first member
        // with provider+model) selects it. No-op when `lead_model` is None or
        // matches nothing — zero behavior change for uncurated/Auto runs.
        let members = float_lead_member(members, req.lead_model.as_ref());
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
    ///
    /// The produced DAG is cycle-checked ([`planned_dag_has_cycle`], symmetric with
    /// `adjust`'s guard) BEFORE any persist: a cyclic planner output would
    /// soft-strand the run, so it degrades to the degenerate single-task plan.
    pub async fn plan(&self, run_id: &str) -> Result<(), AppError> {
        let run = self
            .run_repo
            .get_run(run_id)
            .await
            .map_err(OrchestratorError::from)?
            .ok_or_else(|| OrchestratorError::NotFound(format!("run {run_id}")))?;

        let members: Vec<FleetMember> = decode_fleet_snapshot(run_id, &run.fleet_snapshot);

        // B3: phase narration — a deterministic, provider-independent progress
        // narrative so the frontend can show "正在规划…" even when no reasoning
        // stream is available. The `content` is a SEMANTIC KEY (the i18n copy lives
        // in the frontend); the backend NEVER ships prose. `planning_started` fires
        // BEFORE the lead call (the long silent gap the user complained about).
        self.emitter
            .emit_lead_thinking(run_id, "plan", "phase", None, Some("planning_started"), false);

        // B2: stream the lead's planning thought (reasoning + draft) over WS while
        // the one-shot completion runs. This path holds NO per-run lock (planning
        // happens before `engine.start`), so streaming is safe here. The throttle
        // coalesces deltas to avoid WS flooding; `flush()` after the call emits the
        // residue (nothing dropped). `sink=None` would be byte-identical to the
        // pre-B2 one-shot path — passing Some only adds the fan-out.
        let throttle = crate::plan::LeadThinkingThrottle::new(self.emitter.clone(), run_id, "plan");
        let sink = throttle.sink();
        // 安全网：给 planner 的 LLM 单次 completion 加硬超时——planner 本身无重试、无
        // 超时（plan.rs），底层若 hang 会让 run 永停 `planning`（黑洞）。到点即 Err，
        // 经 `spawn_plan_and_start` fail-soft 留 `planning` 可重规划；run 级看门狗随后兜底。
        let produced = tokio::time::timeout(
            std::time::Duration::from_secs(PLAN_TIMEOUT_SECS),
            self.planner.produce(&run.goal, &members, Some(&sink)),
        )
        .await;
        throttle.flush();
        let produced = match produced {
            Ok(res) => res,
            Err(_elapsed) => {
                return Err(AppError::Timeout(format!(
                    "planning timed out after {PLAN_TIMEOUT_SECS}s (planner unresponsive); run left re-plannable"
                )));
            }
        };
        let dag: PlannedDag = produced?;

        // B3: the lead produced a DAG — narrate the decomposition phase before we
        // persist the tasks/edges (still a semantic key, no prose).
        self.emitter
            .emit_lead_thinking(run_id, "plan", "phase", None, Some("decomposing"), false);

        self.persist_dag_and_activate(run_id, &run, &members, dag).await
    }

    /// plan() 的「落库半段」：cycle guard → 建任务 → 连边 → 分派 assignment →
    /// planUpdated → autonomy 门 → emit_run_status。与 planner 完全解耦，
    /// plan()（LLM 产 DAG）与 plan_flat()（调用方显式任务列表）共用；行为与
    /// 拆分前逐字节一致。`assigning` 阶段叙事保留在此（两条路径都要刷画布）。
    async fn persist_dag_and_activate(
        &self,
        run_id: &str,
        run: &nomifun_db::models::OrchRunRow,
        members: &[FleetMember],
        dag: PlannedDag,
    ) -> Result<(), AppError> {
        #[allow(unused_mut)]
        let mut dag = dag;
        // CYCLE GUARD (symmetric with `adjust`'s `reconcile_plan_has_cycle`): the
        // planner's `depends_on` indices are range-validated when wiring edges below,
        // but a back-referencing planner output (e.g. task 0 depends_on [2], task 2
        // depends_on [0]) would persist a back-edge → a soft-strand the engine can
        // never make ready (the same class `adjust` rejects). The initial-plan path
        // had NO acyclicity check. We don't have a prior plan to fall back to here, so
        // rejecting would strand the WHOLE run; instead we degrade to the degenerate
        // single-task plan (the whole goal as one agent task) so the run still
        // proceeds. The normal acyclic plan is untouched (the check returns false).
        if planned_dag_has_cycle(&dag) {
            tracing::warn!(
                run_id,
                task_count = dag.tasks.len(),
                "planner produced a cyclic task DAG; falling back to a single-task plan"
            );
            dag = degenerate_plan(&run.goal);
        }

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
                    // 迁移 023: persist the planner-chosen mode + optional config.
                    // `kind` is serde-defaulted to "agent" for legacy/fallback plans,
                    // so a normal single-agent task is unchanged (zero regression).
                    kind: planned.kind.clone(),
                    pattern_config: planned.pattern_config.clone(),
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
        // B3: narrate the assignment phase before routing each task to a member.
        self.emitter
            .emit_lead_thinking(run_id, "plan", "phase", None, Some("assigning"), false);
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

            self.assign_task(
                run_id,
                task_id,
                &members,
                planned.task_profile.as_ref(),
                planned.member_index,
                planned.rationale.as_deref(),
            )
            .await?;
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

    /// 扁平 fan-out 规划（nomi_spawn）：跳过 planner LLM，直接把调用方给的任务
    /// 列表落库并激活。任务为空 → BadRequest（否则 run 会立即被判 stuck）。
    /// depends_on（如 synthesize 汇总节点）照常落边；autonomy 门与 plan() 一致
    /// （扁平 run 由前门以 supervised 创建 → 直接 running）。
    pub async fn plan_flat(&self, run_id: &str, tasks: Vec<PlannedTask>) -> Result<(), AppError> {
        if tasks.is_empty() {
            return Err(AppError::BadRequest("plan_flat requires at least one task".into()));
        }
        let run = self
            .run_repo
            .get_run(run_id)
            .await
            .map_err(OrchestratorError::from)?
            .ok_or_else(|| OrchestratorError::NotFound(format!("run {run_id}")))?;
        let members: Vec<FleetMember> = decode_fleet_snapshot(run_id, &run.fleet_snapshot);
        self.persist_dag_and_activate(run_id, &run, &members, PlannedDag { tasks }).await
    }

    /// Route + persist ONE task's auto-assignment (the LLM-primary + Router-veto
    /// pick) and emit `task.assigned`. Extracted from [`plan`](Self::plan) so the
    /// conversational reconcile path ([`adjust`](Self::adjust)) routes its NEW
    /// tasks IDENTICALLY (same Router behavior, same `source = "auto"`). `profile`
    /// is the task's [`TaskProfile`] (or a neutral default), `member_index` the
    /// planner's pre-assignment (honored when viable), `rationale` the planner's
    /// note (used only on the all-hard-filtered fallback). A snapshot with no
    /// members logs a warn + skips (the engine will fail the task).
    ///
    /// The routing DECISION (which member, score, rationale) is the pure
    /// [`resolve_assignment_pick`] free fn — `adjust` reuses it to compute every
    /// new task's pick IN MEMORY before the transactional reconcile, so this
    /// method is only the persist-and-emit half.
    async fn assign_task(
        &self,
        run_id: &str,
        task_id: &str,
        members: &[FleetMember],
        task_profile: Option<&TaskProfile>,
        member_index: Option<usize>,
        rationale: Option<&str>,
    ) -> Result<(), AppError> {
        let Some(pick) = resolve_assignment_pick(members, task_profile, member_index, rationale)
        else {
            tracing::warn!(
                run_id,
                task_id,
                "fleet snapshot has no members; cannot assign task (engine will fail it)"
            );
            return Ok(());
        };

        self.run_repo
            .create_assignment(CreateAssignmentParams {
                task_id: task_id.to_string(),
                member_id: pick.member_id.clone(),
                score: pick.score,
                rationale: pick.rationale,
                source: "auto".to_string(),
                locked: false,
            })
            .await
            .map_err(OrchestratorError::from)?;
        self.emitter.emit_task_assigned(run_id, task_id, &pick.member_id);
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

    /// Re-execute a single node (UC-2a): RESET the task to `pending` (clearing its
    /// prior output + worker conversation, bumping `attempt`), CASCADE the reset
    /// down to its settled dependents (so they re-run against the new upstream
    /// output), and RE-ACTIVATE the run (flip a terminal run back to `running`) so
    /// the engine loop re-drives the now-pending tasks. Owner-scoped (404 missing /
    /// 403 not-owner, like `delete`/`rename`/`replan`).
    ///
    /// **Reject-running (no live-worker clobber):** if the target task is currently
    /// `running`, this is a `BadRequest` — resetting a live task's row would let its
    /// in-flight worker settle `done` OVER the reset (the worker future is still
    /// holding the row and will `update_task(done)` when it returns). The user must
    /// pause/stop the run first. (This is the validated reverted-workflow lesson.)
    ///
    /// **Cascade safety:** the BFS over the dep edges resets *settled* dependents
    /// (`done`/`failed`/`skipped`, kind-aware on `pattern_config`) and descends into
    /// their dependents. A `running` dependent is a HARD BOUNDARY (UC-2a 评审
    /// Important-2): it is SKIPPED (never clobbered — its live worker would settle
    /// over the reset) AND the walk does NOT descend past it, so its downstream is
    /// NOT re-run against the running node's stale (pre-rerun) output; that subtree
    /// is left to the live loop / a later rerun. A `pending` dependent needs no
    /// reset (it is already going to run) but is not a stale-output boundary, so the
    /// walk keeps descending through it. The walk is bounded by a `seen` guard over
    /// the (acyclic) graph.
    ///
    /// **Re-activation:** if the run is terminal (`completed`/`failed`/`cancelled`)
    /// it is flipped back to `running` + emitted, so the boot-resume-style loop the
    /// route then `engine.start`s has a `running` run to drive. An already-`running`
    /// run is left as-is (the live loop re-picks the reset tasks on its next sweep);
    /// the route still pokes the engine so an exited loop respawns. Returns the run
    /// DTO so the route can mirror `approve`/`replan`'s engine-lifecycle handling.
    /// **Concurrency (UC-2a 评审 Critical):** this method MUST run under the
    /// engine's per-run lock — call it via
    /// [`RunEngine::rerun_task`](crate::engine::RunEngine::rerun_task), NOT directly
    /// in production, so the reset + re-activation is serialized with the run-loop's
    /// terminal-check-and-finish. Calling it lock-free races the loop: the loop
    /// could write `completed`/`failed` between our `owned_run` read and the
    /// re-activation, stranding the run terminal-with-a-pending-task (boot-resume
    /// only re-lists `running`, so it is never recovered). The re-activation block
    /// below therefore re-reads the run status FRESH (never the stale top-of-method
    /// snapshot).
    pub async fn rerun_task(&self, user_id: &str, run_id: &str, task_id: &str) -> Result<Run, AppError> {
        let run = self.owned_run(user_id, run_id).await?;

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

        // Reject re-running a LIVE task: its in-flight worker would settle `done`
        // over the reset. Pause/stop the run first.
        if task.status == "running" {
            return Err(OrchestratorError::BadRequest(
                "任务运行中，请先暂停/停止再重跑".into(),
            )
            .into());
        }

        // RESET the target task (status→pending, clear output/conv, attempt+1).
        // Pass the kind so a pattern node (verify/judge/loop) keeps its policy in
        // `pattern_config` (Important-1).
        self.reset_task(task_id, task.attempt, &task.kind).await?;
        self.emitter.emit_task_status(run_id, task_id, "pending");

        // CASCADE: transitively reset the target's SETTLED dependents so they
        // re-run with the new upstream output. A `running` dependent is a HARD
        // BOUNDARY — we skip it AND do NOT descend past it (UC-2a 评审 Important-2):
        // its in-flight worker will settle against the STALE pre-rerun upstream, so
        // resetting ITS downstream would re-run them against that stale lineage.
        // That subtree is left to the live loop / a later rerun. We only descend
        // through nodes we actually reset (settled→pending) or that were already
        // `pending` (harmless — they have not produced output yet); a `pending`
        // node is enqueued so a genuinely-mixed frontier still reaches deeper
        // settled nodes.
        let dep_edges = self
            .run_repo
            .list_deps(run_id)
            .await
            .map_err(OrchestratorError::from)?;
        let mut frontier: Vec<String> = dep_edges
            .iter()
            .filter(|d| d.blocker_task_id == task_id)
            .map(|d| d.blocked_task_id.clone())
            .collect();
        let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
        while let Some(tid) = frontier.pop() {
            if !seen.insert(tid.clone()) {
                continue;
            }
            // Default: if we cannot read the node, do not descend past it (treat an
            // unreadable node as an opaque boundary rather than blindly cascading).
            let mut descend = false;
            if let Some(dep_task) = self
                .run_repo
                .get_task(&tid)
                .await
                .map_err(OrchestratorError::from)?
            {
                match dep_task.status.as_str() {
                    // Settled dependent → reset it (kind-aware pattern_config) and
                    // descend into ITS dependents (the reset propagates downstream).
                    "done" | "failed" | "skipped" => {
                        self.reset_task(&tid, dep_task.attempt, &dep_task.kind).await?;
                        self.emitter.emit_task_status(run_id, &tid, "pending");
                        descend = true;
                    }
                    // Running dependent → BOUNDARY: skip (never clobber the live
                    // worker) AND do NOT enqueue its dependents (no stale-lineage
                    // re-run downstream). Leave the subtree to the live loop.
                    "running" => {
                        descend = false;
                    }
                    // Pending dependent → needs no reset (already going to run), but
                    // it is NOT a stale-output boundary (it has produced nothing), so
                    // keep descending to reach any settled nodes beyond it.
                    _ => {
                        descend = true;
                    }
                }
            }
            // Enqueue this task's own dependents only when we descended through it.
            if descend {
                for d in dep_edges.iter().filter(|d| d.blocker_task_id == tid) {
                    frontier.push(d.blocked_task_id.clone());
                }
            }
        }

        // RE-ACTIVATE: a terminal run must flip back to `running` so the engine
        // loop (which the route then starts) has a live run to drive — `run_loop`
        // fills the now-pending ready tasks and re-settles. An already-running run
        // needs no flip (its loop re-picks the reset tasks).
        //
        // **Re-read the run status FRESH here (do NOT use the top-of-method `run`
        // snapshot).** Under the engine's per-run lock (this method runs through
        // `RunEngine::rerun_task`), the loop's terminal-check-and-finish is mutually
        // exclusive with this block — but the loop may have written
        // `completed`/`failed` AFTER our `owned_run` read and BEFORE we acquired the
        // lock. The stale snapshot would then read `running`, skip the flip, and
        // leave the run TERMINAL with a freshly-reset pending task and no loop (the
        // 评审 Critical variant B — never recovered, as boot-resume only lists
        // `running`). The fresh read closes that window: whatever the loop committed
        // before us is visible, so a now-terminal run is correctly re-activated.
        let current_status = self
            .run_repo
            .get_run(run_id)
            .await
            .map_err(OrchestratorError::from)?
            .map(|r| r.status)
            .unwrap_or_else(|| run.status.clone());
        if matches!(current_status.as_str(), "completed" | "failed" | "cancelled") {
            self.run_repo
                .update_run(
                    run_id,
                    UpdateRunParams {
                        status: Some("running".to_string()),
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
            self.emitter.emit_run_status(run_id, "running");
        }

        // Return the (possibly re-activated) run so the route can read its status
        // and start the engine loop.
        let row = self
            .run_repo
            .get_run(run_id)
            .await
            .map_err(OrchestratorError::from)?
            .ok_or_else(|| OrchestratorError::NotFound(format!("run {run_id}")))?;
        Ok(run_row_to_dto(row))
    }

    /// **UC-2c — "采用为该节点产出" (adopt task result).** Pull the CURRENT final
    /// output of a node's worker conversation back into the orchestration node, then
    /// re-activate the run so the engine drives any now-unblocked downstream.
    ///
    /// This closes the gap created by the conversation-native worker projection: a
    /// user can keep chatting with a *failed* / *stuck* worker (a normal turn in the
    /// worker conversation) to push it toward a good answer, but those turns are NOT
    /// observed by the engine, so the node never updates on its own. Adopt is the
    /// explicit "I'm happy with what the worker produced — make it the node's output"
    /// action: it reads the worker's latest assistant text (via the engine's
    /// `WorkerRunner`), writes the node `done` + `output_summary`, and re-activates a
    /// terminal run so its loop unblocks the downstream and re-settles.
    ///
    /// Runs **only through** [`RunEngine::adopt_task_result`](crate::engine::RunEngine::adopt_task_result),
    /// NOT directly — so the write + re-activation are serialized under the per-run
    /// lock with the run-loop's terminal-check-and-finish (same invariant as
    /// [`rerun_task`](Self::rerun_task); the re-activation re-reads the run status
    /// FRESH rather than trusting the top-of-method snapshot).
    ///
    /// Owner-scoped (404/403). REJECTS when the RUN is `running` (the engine loop is
    /// live and WILL settle the node itself — a manual adopt would race it); allows
    /// any non-running run (cancelled / failed / completed / paused / awaiting),
    /// which is exactly the state a stuck/continued node sits in. Note this guards on
    /// the RUN, not the TASK: a task stuck `running` inside a CANCELLED run (the loop
    /// stopped mid-flight) is the canonical adopt target and must be allowed.
    pub async fn adopt_task_result(
        &self,
        worker: &Arc<dyn crate::worker::WorkerRunner>,
        user_id: &str,
        run_id: &str,
        task_id: &str,
    ) -> Result<Run, AppError> {
        let run = self.owned_run(user_id, run_id).await?;

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

        // A LIVE run's loop owns settlement — manual adopt would race the loop's
        // own `done` write. The user pauses/stops (→ non-running) first. (Guards on
        // the RUN status, not the task: a task left `running` by a cancelled loop is
        // a valid adopt target.)
        if run.status == "running" {
            return Err(OrchestratorError::BadRequest(
                "run 运行中，引擎会自动结算该节点，无需手动采用".into(),
            )
            .into());
        }

        // The node must have a worker conversation to read from.
        let Some(conv_id) = task.conversation_id else {
            return Err(OrchestratorError::BadRequest("该节点尚无 worker 会话，无法采用产出".into()).into());
        };

        // Read the worker conversation's CURRENT final assistant text. `None` →
        // nothing to adopt yet (the worker has produced no assistant message); ask
        // the user to wait for a reply rather than marking the node done with empty
        // output.
        let Some(output) = worker.read_final_output(&conv_id.to_string()).await else {
            return Err(OrchestratorError::BadRequest(
                "该节点 worker 暂无最终回复(可能仍在自动重试,或本次仅产生了报错)。请等其回复完成,或在该节点对话里继续/重跑后再采用产出".into(),
            )
            .into());
        };

        // ADOPT: mark the node `done` and write the worker's text as its output (the
        // same shape the engine's `settle_task_outcome` writes on a normal finish).
        self.run_repo
            .update_task(
                task_id,
                UpdateTaskParams {
                    status: Some("done".to_string()),
                    output_summary: Some(Some(output)),
                    ..Default::default()
                },
            )
            .await
            .map_err(OrchestratorError::from)?;
        self.emitter.emit_task_status(run_id, task_id, "done");

        // RE-ACTIVATE a terminal run so the engine loop (the route then starts) has a
        // live run to drive the now-unblocked downstream. Re-read the status FRESH
        // (same reasoning as `rerun_task`): under the per-run lock the loop's
        // terminal-finish is mutually exclusive with this block, but may have
        // committed `completed`/`failed` just before we acquired the lock — the fresh
        // read sees it so a now-terminal run is correctly re-activated. A `paused`
        // run is deliberately left paused (the user resumes it explicitly).
        let current_status = self
            .run_repo
            .get_run(run_id)
            .await
            .map_err(OrchestratorError::from)?
            .map(|r| r.status)
            .unwrap_or_else(|| run.status.clone());
        if matches!(current_status.as_str(), "completed" | "failed" | "cancelled") {
            self.run_repo
                .update_run(
                    run_id,
                    UpdateRunParams {
                        status: Some("running".to_string()),
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
            self.emitter.emit_run_status(run_id, "running");
        }

        // Return the (possibly re-activated) run so the route can decide whether to
        // (re)start the engine loop.
        let row = self
            .run_repo
            .get_run(run_id)
            .await
            .map_err(OrchestratorError::from)?
            .ok_or_else(|| OrchestratorError::NotFound(format!("run {run_id}")))?;
        Ok(run_row_to_dto(row))
    }

    /// Fine-tune a node's intent/prompt (UC-2a, "意图/prompt 微调"): replace the
    /// task's `spec` (the field the worker brief is built from). Owner-scoped
    /// (404/403). A subsequent [`rerun_task`](Self::rerun_task) re-executes the node
    /// with the amended spec. REJECTS a task that is currently `running` — mutating
    /// a live task's spec would race the in-flight worker (which already composed
    /// its brief from the OLD spec); the user must pause/stop first. A blank spec is
    /// a `BadRequest` (the spec drives the worker; an empty intent is meaningless).
    pub async fn update_task_spec(
        &self,
        user_id: &str,
        run_id: &str,
        task_id: &str,
        new_spec: &str,
    ) -> Result<(), AppError> {
        let new_spec = new_spec.trim();
        if new_spec.is_empty() {
            return Err(OrchestratorError::BadRequest("spec must not be empty".into()).into());
        }
        self.owned_run(user_id, run_id).await?;

        let task = self
            .run_repo
            .get_task(task_id)
            .await
            .map_err(OrchestratorError::from)?
            .ok_or_else(|| OrchestratorError::NotFound(format!("task {task_id}")))?;
        if task.run_id != run_id {
            return Err(OrchestratorError::NotFound(format!("task {task_id} in run {run_id}")).into());
        }
        if task.status == "running" {
            return Err(OrchestratorError::BadRequest(
                "任务运行中，请先暂停/停止再微调".into(),
            )
            .into());
        }

        self.run_repo
            .update_task(
                task_id,
                UpdateTaskParams {
                    spec: Some(new_spec.to_string()),
                    ..Default::default()
                },
            )
            .await
            .map_err(OrchestratorError::from)?;
        // Surface a plan-updated signal so subscribers refresh the node's spec.
        self.emitter.emit_run_plan_updated(run_id);
        Ok(())
    }

    /// 启动前配置台 (迁移 026): set/clear a node's per-task **model override** and
    /// **预置要求**. Owner-scoped; rejects a `running` task (400) — pending / settled
    /// (done/failed) nodes are fine (a settled node's change takes effect on the next
    /// `rerun`; a pending node's at dispatch). This is a FULL replace of the three
    /// override fields (the config panel always sends the desired state; `None`/blank
    /// clears). The model override requires BOTH provider + model — a half-set is
    /// normalized to "cleared" so a partial value never reaches dispatch. No engine
    /// call: `resolve_task_member` / `compose_brief` read these at dispatch time.
    pub async fn set_task_config(
        &self,
        user_id: &str,
        run_id: &str,
        task_id: &str,
        override_provider_id: Option<String>,
        override_model: Option<String>,
        preset_prompt: Option<String>,
    ) -> Result<(), AppError> {
        self.owned_run(user_id, run_id).await?;

        let task = self
            .run_repo
            .get_task(task_id)
            .await
            .map_err(OrchestratorError::from)?
            .ok_or_else(|| OrchestratorError::NotFound(format!("task {task_id}")))?;
        if task.run_id != run_id {
            return Err(OrchestratorError::NotFound(format!("task {task_id} in run {run_id}")).into());
        }
        if task.status == "running" {
            return Err(OrchestratorError::BadRequest(
                "任务运行中，请先暂停/停止再配置".into(),
            )
            .into());
        }

        // Trim to non-empty, else clear. Model override is all-or-nothing (both
        // provider + model), so a half-set clears both.
        let norm = |s: Option<String>| s.map(|v| v.trim().to_string()).filter(|v| !v.is_empty());
        let (mut prov, mut model) = (norm(override_provider_id), norm(override_model));
        if prov.is_none() || model.is_none() {
            prov = None;
            model = None;
        }
        let preset = norm(preset_prompt);

        self.run_repo
            .set_task_overrides(task_id, prov, model, preset)
            .await
            .map_err(OrchestratorError::from)?;
        self.emitter.emit_run_plan_updated(run_id);
        Ok(())
    }

    /// RESET a settled task for re-execution: status→`pending`, clear
    /// `output_summary` + `conversation_id` (and `output_files`), and bump
    /// `attempt`. Shared by the target reset + the cascade walk in
    /// [`rerun_task`](Self::rerun_task). Mirrors the engine's `settle_loop_task`
    /// CONTINUE reset (the validated reset shape: pending + clear output/conv +
    /// attempt+1) so a re-run task is indistinguishable from a fresh dispatch.
    ///
    /// **`pattern_config` is KIND-AWARE (UC-2a 评审 Important-1).** The
    /// `pattern_config` column means two completely different things by node kind:
    /// - for an `agent` task it carries ONLY the transient loop-body carry
    ///   (`loop_prior_output` / `loop_iteration`, written by the engine's loop
    ///   controller on CONTINUE) — that stale carry MUST be cleared on reset so a
    ///   re-run starts from the (possibly amended) spec, not a prior round's brief;
    /// - for a `verify` / `judge` / `loop` PATTERN node it carries the node's
    ///   POLICY (`VotePolicy` / `JudgePolicy` / `LoopConfig` — unanimous/threshold,
    ///   custom aggregate, `max_iter`/stop) which the engine RE-PARSES on every
    ///   settle. Wiping it would silently revert the policy to its default on a
    ///   rerun (unanimous→majority, custom judge→mean, custom max_iter→cap-only).
    ///
    /// So we clear `pattern_config` ONLY for the `agent` kind (dropping the loop
    /// carry); for `verify`/`judge`/`loop` we leave it intact (omit the field from
    /// the update) to PRESERVE the policy.
    async fn reset_task(
        &self,
        task_id: &str,
        prior_attempt: i64,
        kind: &str,
    ) -> Result<(), AppError> {
        // Pattern nodes (verify/judge/loop) store POLICY in pattern_config — keep
        // it. An agent task's pattern_config is only the stale loop-body carry —
        // clear it. `None` here means "leave the column unchanged".
        let pattern_config = if kind == "agent" {
            Some(None)
        } else {
            None
        };
        self.run_repo
            .update_task(
                task_id,
                UpdateTaskParams {
                    status: Some("pending".to_string()),
                    conversation_id: Some(None),
                    output_summary: Some(None),
                    output_files: Some(None),
                    attempt: Some(prior_attempt + 1),
                    pattern_config,
                    // Clear any transient-error retry gate so a manual rerun is not
                    // held back by a stale backoff timestamp (迁移 024).
                    next_retry_at: Some(None),
                    ..Default::default()
                },
            )
            .await
            .map_err(OrchestratorError::from)?;
        Ok(())
    }

    /// Reset a run's ORPHANED `running` tasks back to `pending` (thin service
    /// wrapper over [`IRunRepository::reset_orphaned_running_tasks`] scoped to one
    /// run). Called by [`RunEngine::rerun_task`](crate::engine::RunEngine::rerun_task)
    /// ONLY when the run has no live loop (`!is_running`), so a `running` row is
    /// provably an orphan whose worker died — normalizing it lets the subsequent
    /// rerun proceed (the rerun guard rejects a `running` target). Kind-aware; no
    /// emit (the caller's rerun emits + the route's refetch reload all task states).
    pub async fn reset_orphaned_running(&self, run_id: &str) -> Result<u64, AppError> {
        let n = self
            .run_repo
            .reset_orphaned_running_tasks(Some(run_id))
            .await
            .map_err(OrchestratorError::from)?;
        Ok(n)
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
        // Settle the interrupted node(s): the cancel ROUTE called `engine.stop`
        // FIRST, which aborted the run loop and cancelled its in-flight worker
        // conversation(s) — so any `running` task is now a guaranteed ORPHAN (its
        // worker is gone and the aborted loop will never write its terminal status).
        // Mark them `cancelled` so the invariant `running ⟺ live worker` holds and
        // no phantom「执行中」node survives the cancel (which would otherwise block a
        // later rerun/adjust). A subsequent rerun resets it (non-running → pending).
        self.run_repo
            .mark_run_running_tasks_cancelled(run_id)
            .await
            .map_err(OrchestratorError::from)?;
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

    /// UC-3a — **conversation-driven intelligent re-adjust**. The user expresses a
    /// free-form `intent` against an EXISTING run; the lead model (one-shot, no
    /// persistent session) sees the intent + the CURRENT run state and JUDGES, per
    /// task, whether to KEEP the completed work or re-decompose. The service then
    /// RECONCILEs the current plan to the lead's adjusted plan:
    ///
    /// - **KEEP** = the existing task ids the adjusted plan kept → their rows
    ///   (status / output_summary / conversation_id / tokens / assignment) are left
    ///   UNTOUCHED — completed work is preserved, never re-run.
    /// - **DROP** = existing tasks NOT kept → deleted (cascading their deps +
    ///   assignment). The lead chose to discard/replace them.
    /// - **NEW** = the adjusted plan's new tasks → inserted `pending` and routed via
    ///   the SAME [`assign_task`](Self::assign_task) path the planner uses.
    /// - **DEPS** = the run's edges are cleared and REBUILT from the adjusted plan:
    ///   every node's `depends_on` (kept-id strings + new-index ints) is resolved to
    ///   task ids and wired. A kept DONE task with new deps is fine — its deps are
    ///   already satisfied (it is done), so it is not re-run.
    ///
    /// **Running-task safety (the chosen SAFE option — REJECT):** if ANY task is
    /// currently `running`, `adjust` is a `BadRequest` ("运行中,请先暂停再重调") and
    /// NOTHING is mutated. A running task has a live worker holding its row (it will
    /// `update_task` when it returns); deleting/rewiring it under the worker is the
    /// Phase-2 live-clobber hazard. Rejecting (vs. force-keeping) keeps the contract
    /// simple and the run state coherent — the user pauses, then re-adjusts.
    ///
    /// **No-strand (lock + re-activation):** the WHOLE reconcile + the terminal-run
    /// re-activation runs under the engine's per-run lock — call it via
    /// [`RunEngine::adjust`](crate::engine::RunEngine::adjust), NOT directly in
    /// production. That serializes it with the run loop's terminal-check-and-finish
    /// (mirroring [`rerun_task`](Self::rerun_task)): the re-activation re-reads the
    /// run status FRESH, so a run the loop concluded terminal AFTER our `owned_run`
    /// read is still flipped back to `running` (never stranded terminal-with-pending).
    ///
    /// Owner-scoped (404 missing / 403 not-owner). A bad adjusted plan (unparseable
    /// / empty / referencing an unknown kept id / forming a DEPENDENCY CYCLE) is
    /// surfaced as an error with the run UNCHANGED (no partial mutation — every
    /// validation precedes the writes).
    ///
    /// **Atomic reconcile (UC-3a 评审 Important-A):** the service resolves the WHOLE
    /// reconcile IN MEMORY (the tasks to delete, the new tasks + their routed
    /// assignments, the full dep set) and then applies it via ONE transactional repo
    /// call ([`IRunRepository::reconcile_run_plan`]) — clear-deps + delete-unkept +
    /// insert-new + rebuild-deps in a single `pool.begin()…commit()`. A mid-way DB
    /// error rolls the whole thing back, so a failure leaves the run exactly as it
    /// was (no durable half-reconciled state). The terminal-run re-activation stays
    /// its OWN write AFTER the successful reconcile tx (it only flips terminal→running
    /// once the reconcile committed).
    ///
    /// **Acyclicity pre-check (UC-3a 评审 Important-B):** after resolving the intended
    /// final (kept + new) graph, a pure bounded cycle check ([`reconcile_plan_has_cycle`],
    /// Kahn topological sort) runs BEFORE any write. A lead output forming mutual
    /// new↔new deps or a new→kept→…→new cycle would otherwise persist a cycle the
    /// engine can never make ready (a soft-strand); rejecting it here keeps the run
    /// unchanged.
    /// **B4 — the production adjust entry is now SPLIT into two halves** so the lead
    /// LLM call can run OUTSIDE the per-run lock (Global Constraint: the per-run lock
    /// MUST NOT span an LLM await). This wrapper preserves the single-call semantics
    /// for the tests + any non-engine caller (compute → apply, no lock); the engine
    /// (`RunEngine::adjust`) calls the two halves directly, wrapping ONLY
    /// [`apply_adjusted_plan`](Self::apply_adjusted_plan) in the lock while
    /// [`compute_adjusted_plan`](Self::compute_adjusted_plan) (the LLM await) runs
    /// lock-free. Behavior of this wrapper is byte-identical to the pre-B4 monolith
    /// (`sink=None`): same validation order, same reconcile, same re-activation.
    pub async fn adjust(&self, user_id: &str, run_id: &str, intent: &str) -> Result<Run, AppError> {
        let adjusted = self.compute_adjusted_plan(user_id, run_id, intent, None).await?;
        self.apply_adjusted_plan(user_id, run_id, adjusted).await
    }

    /// **B4 half 1 — LOCK-FREE.** Snapshot the run's current state, ask the lead to
    /// judge keep-vs-redo (the LLM await — streamed over `sink` when `Some`), and
    /// return the parsed [`AdjustedPlan`] IN MEMORY. NOTHING is written here. This is
    /// the half [`RunEngine::adjust`] runs OUTSIDE the per-run lock so a multi-second
    /// (or hanging) lead call never stalls a concurrent rerun/loop on the same run.
    ///
    /// Fast-fail guards (BEFORE the LLM, mutating nothing): empty intent → BadRequest;
    /// missing run → 404; not-owner → 403; any `running` task → BadRequest ("运行中").
    /// A garbled adjusted plan surfaces as the lead's error (still nothing mutated).
    ///
    /// **TOCTOU:** the snapshot taken here is for the LEAD's input only — it is NOT
    /// trusted by [`apply_adjusted_plan`](Self::apply_adjusted_plan), which re-reads
    /// the run state FRESH under the lock and re-validates (kept-exists / no-running /
    /// acyclic) before any write. A concurrent rerun/loop that lands between this
    /// snapshot and the apply is caught there (mirrors the B2 summarize fresh-guard
    /// re-verify), so the run is never corrupted by a stale-snapshot adjust.
    pub async fn compute_adjusted_plan(
        &self,
        user_id: &str,
        run_id: &str,
        intent: &str,
        sink: Option<&crate::plan::LeadThinkingSink>,
    ) -> Result<crate::plan::AdjustedPlan, AppError> {
        let intent = intent.trim();
        if intent.is_empty() {
            return Err(OrchestratorError::BadRequest("意图不能为空".into()).into());
        }
        // 404 (missing) / 403 (not owner) BEFORE any read of run internals.
        let run = self.owned_run(user_id, run_id).await?;

        // Snapshot the CURRENT run state for the lead (input only — apply re-reads).
        let members: Vec<FleetMember> = decode_fleet_snapshot(run_id, &run.fleet_snapshot);
        let current_tasks = self.run_repo.list_tasks(run_id).await.map_err(OrchestratorError::from)?;
        let current_deps = self.run_repo.list_deps(run_id).await.map_err(OrchestratorError::from)?;

        // SAFETY: refuse to re-adjust while any worker is in-flight. Checked BEFORE
        // calling the lead so we fail fast and mutate nothing. (Re-checked under the
        // lock in `apply_adjusted_plan` against a FRESH read — this is the fast path.)
        if current_tasks.iter().any(|t| t.status == "running") {
            return Err(OrchestratorError::BadRequest(
                "运行中，请先暂停再重调".into(),
            )
            .into());
        }

        let task_dtos: Vec<RunTask> =
            current_tasks.iter().cloned().map(task_row_to_dto).collect();
        let dep_dtos: Vec<RunTaskDep> =
            current_deps.iter().cloned().map(dep_row_to_dto).collect();

        // The lead JUDGES keep-vs-redo. Fail-soft TO AN ERROR: a garbled adjusted
        // plan returns BadRequest (the run is still untouched — nothing is written in
        // this method). B4: this LLM await runs LOCK-FREE (the engine holds NO per-run
        // lock here); `sink` streams the lead's adjust-phase thought over WS when set.
        let plan = self
            .planner
            .adjust(intent, &task_dtos, &dep_dtos, &members, sink)
            .await?;
        Ok(plan)
    }

    /// **B4 half 2 — LOCK-INTERNAL, pure DB.** Apply a precomputed [`AdjustedPlan`]
    /// (from [`compute_adjusted_plan`](Self::compute_adjusted_plan)) to the run:
    /// RE-VALIDATE against a FRESH read, reconcile in ONE transaction, and re-activate
    /// a terminal run. CONTAINS NO LLM await — the engine runs this UNDER the per-run
    /// lock, so it serializes with the run loop's terminal-check-and-finish.
    ///
    /// **Re-validation (TOCTOU close):** the run state is re-read FRESH here (NOT the
    /// compute-phase snapshot) and re-checked before any write — owner (404/403), no
    /// `running` task (a rerun/loop may have started one during the LLM await →
    /// BadRequest, run untouched), every kept id still EXISTS (a concurrent rerun/loop
    /// may have changed the task set → a vanished kept id is a BadRequest, run
    /// untouched), and the resolved (kept+new) graph is ACYCLIC (Kahn). This mirrors
    /// the B2 summarize fresh-guard re-verify: the apply trusts the lock-held fresh
    /// read, not the stale compute snapshot.
    pub async fn apply_adjusted_plan(
        &self,
        user_id: &str,
        run_id: &str,
        plan: crate::plan::AdjustedPlan,
    ) -> Result<Run, AppError> {
        // 404 (missing) / 403 (not owner) on a FRESH read under the lock.
        let run = self.owned_run(user_id, run_id).await?;

        // RE-SNAPSHOT the run state FRESH (do NOT trust the compute-phase snapshot):
        // a concurrent rerun/loop may have mutated tasks during the lock-free LLM
        // await. Every validation below runs against THIS fresh read.
        let current_tasks = self.run_repo.list_tasks(run_id).await.map_err(OrchestratorError::from)?;

        // SAFETY (re-check under the lock): refuse to reconcile while any worker is
        // in-flight. A rerun/resume may have started a task AFTER `compute`'s fast
        // check — reject here so we never delete/rewire a task with a live worker
        // (the Phase-2 live-clobber hazard). Run is left UNCHANGED.
        if current_tasks.iter().any(|t| t.status == "running") {
            return Err(OrchestratorError::BadRequest(
                "运行中，请先暂停再重调".into(),
            )
            .into());
        }

        // VALIDATE every kept id exists in the (FRESH) current run BEFORE mutating —
        // an adjusted plan that references a stale/foreign id (or one a concurrent
        // rerun/loop dropped during the LLM await) is a bad plan; reject it so we
        // never delete real tasks chasing an unresolvable keep. Run left UNCHANGED.
        let current_ids: std::collections::HashSet<&str> =
            current_tasks.iter().map(|t| t.id.as_str()).collect();
        let mut kept_ids: std::collections::HashSet<String> = std::collections::HashSet::new();
        for node in &plan.tasks {
            if let crate::plan::AdjustedNode::Keep { keep } = node {
                if !current_ids.contains(keep.as_str()) {
                    return Err(OrchestratorError::BadRequest(format!(
                        "调整计划保留了不存在的任务 {keep};运行未改动"
                    ))
                    .into());
                }
                kept_ids.insert(keep.clone());
            }
        }

        // The fleet snapshot (for routing the NEW tasks) comes off the fresh run row.
        let members: Vec<FleetMember> = decode_fleet_snapshot(run_id, &run.fleet_snapshot);

        // ---- RESOLVE the adjusted plan IN MEMORY (no writes yet) ----
        //
        // Compute the whole reconcile up front so it can be (a) cycle-checked over
        // the FULL final graph and (b) applied ATOMICALLY in one repo transaction
        // (UC-3a 评审 Important-A + B). Nothing below this point writes until the
        // single transactional `reconcile_run_plan` call.

        // (a) DELETE list: every existing task the adjusted plan did NOT keep.
        let delete_task_ids: Vec<String> = current_tasks
            .iter()
            .filter(|t| !kept_ids.contains(&t.id))
            .map(|t| t.id.clone())
            .collect();

        // (b) Per-node identity map for resolving dep refs. A kept node maps to its
        //     existing id; a NEW node maps to a synthetic key (its index among the
        //     NEW tasks) — the repo mints the real id inside the tx and resolves the
        //     index. `node_keys` is indexed by the adjusted plan's node order so a
        //     `NewIndex(i)` ref (which indexes the PLAN's `tasks`) maps to node i.
        enum NodeKey {
            Kept(String),
            New(usize),
        }
        let mut node_keys: Vec<NodeKey> = Vec::with_capacity(plan.tasks.len());
        let mut new_idx: usize = 0;
        for node in &plan.tasks {
            match node {
                crate::plan::AdjustedNode::Keep { keep } => {
                    node_keys.push(NodeKey::Kept(keep.clone()));
                }
                crate::plan::AdjustedNode::New(_) => {
                    node_keys.push(NodeKey::New(new_idx));
                    new_idx += 1;
                }
            }
        }

        // (c) Build the NEW tasks (create params + precomputed routing pick + dep
        //     refs) for the transactional reconcile. The routing DECISION is the
        //     same pure pick `plan()` uses (`source = "auto"`); the repo persists it
        //     with the freshly minted task id. A dep ref resolves to a Kept id
        //     (validated above) or a NewIndex among the NEW tasks (the plan-node
        //     index `i` maps through `node_keys[i]`). A non-kept id ref or an
        //     out-of-range plan index is logged + skipped (fail-soft on a single bad
        //     edge — the plan as a whole already parsed + validated).
        let mut new_tasks: Vec<ReconcileNewTask> = Vec::with_capacity(new_idx);
        for (idx, node) in plan.tasks.iter().enumerate() {
            let crate::plan::AdjustedNode::New(new_task) = node else {
                continue;
            };
            // Resolve this new task's dep refs to ReconcileDepRef (kept id / new
            // index), the form the repo resolves inside the tx.
            let mut depends_on: Vec<ReconcileDepRef> = Vec::new();
            for dep_ref in &new_task.depends_on {
                match dep_ref {
                    crate::plan::AdjustedDepRef::Kept(id) => {
                        if kept_ids.contains(id) {
                            depends_on.push(ReconcileDepRef::Kept(id.clone()));
                        } else {
                            tracing::warn!(
                                run_id,
                                node_idx = idx,
                                dep_id = %id,
                                "adjusted plan dep references a non-kept id; skipping edge"
                            );
                        }
                    }
                    crate::plan::AdjustedDepRef::NewIndex(i) => match node_keys.get(*i) {
                        Some(NodeKey::New(ni)) => depends_on.push(ReconcileDepRef::NewIndex(*ni)),
                        Some(NodeKey::Kept(id)) => {
                            // A new-index ref that happens to point at a KEPT node:
                            // resolve it to that kept id (still a valid edge).
                            depends_on.push(ReconcileDepRef::Kept(id.clone()));
                        }
                        None => {
                            tracing::warn!(
                                run_id,
                                node_idx = idx,
                                dep_index = *i,
                                "adjusted plan dep references an out-of-range new index; skipping edge"
                            );
                        }
                    },
                }
            }

            let assignment = resolve_assignment_pick(&members, None, None, None).map(|pick| {
                CreateAssignmentParams {
                    // Placeholder; the repo overwrites task_id with the minted id.
                    task_id: String::new(),
                    member_id: pick.member_id,
                    score: pick.score,
                    rationale: pick.rationale,
                    source: "auto".to_string(),
                    locked: false,
                }
            });

            new_tasks.push(ReconcileNewTask {
                task: CreateTaskParams {
                    run_id: run_id.to_string(),
                    title: new_task.title.clone(),
                    spec: new_task.spec.clone(),
                    task_profile: None,
                    status: "pending".to_string(),
                    graph_x: None,
                    graph_y: None,
                    role: new_task.role.clone(),
                    kind: new_task.kind.clone(),
                    pattern_config: new_task.pattern_config.clone(),
                },
                assignment,
                depends_on,
            });
        }

        // (d) ACYCLICITY PRE-CHECK over the FULL resolved graph (kept + new), BEFORE
        //     any write (UC-3a 评审 Important-B). A bad lead output (mutual new↔new
        //     deps, or a new→kept→…→new cycle) would otherwise persist a cycle that
        //     the engine can never make ready → the run soft-strands `running` with
        //     un-runnable pending tasks. Reject it here so the run stays UNCHANGED
        //     (same "run untouched on a bad adjusted plan" contract validate-kept
        //     already honors). The check is pure + bounded (Kahn topological sort
        //     over the resolved edge set; nodes are kept ids + new-task indices).
        if reconcile_plan_has_cycle(&kept_ids, &new_tasks) {
            return Err(OrchestratorError::BadRequest(
                "调整计划存在循环依赖，已拒绝(run 未改动)".into(),
            )
            .into());
        }

        // ---- APPLY the reconcile ATOMICALLY (one transaction, all-or-nothing) ----
        //
        // The repo wraps clear-deps + delete-unkept + insert-new(+assignment) +
        // rebuild-deps in a single `pool.begin()…commit()`: a mid-way DB error rolls
        // the WHOLE thing back, so the run is unchanged on failure (Important-A).
        self.run_repo
            .reconcile_run_plan(
                run_id,
                ReconcilePlan {
                    delete_task_ids,
                    new_tasks,
                },
            )
            .await
            .map_err(OrchestratorError::from)?;

        // A dropped task simply disappears from the next RunDetail. We DON'T emit a
        // per-task `task.statusChanged` for the deletion: `"removed"` is not a real
        // task status (the statuses are pending/running/done/failed/skipped/cancelled),
        // and the FE's `useRunLive` already refetches the WHOLE RunDetail on
        // `planUpdated` below — so a deleted task vanishes from the refetched plan with
        // no fake status involved. (Emitting `planUpdated` AFTER the successful commit
        // means a rolled-back reconcile never signals a phantom plan change.)
        self.emitter.emit_run_plan_updated(run_id);

        // (5) RE-ACTIVATE a terminal run so the engine loop (the route then starts)
        //     has a live run to drive. Re-read the status FRESH (NOT the
        //     top-of-method `run` snapshot): under the per-run lock the loop's
        //     terminal-check-and-finish is mutually exclusive with this reconcile,
        //     but the loop may have written `completed`/`failed` AFTER our
        //     `owned_run` read and BEFORE the lock was acquired (mirrors
        //     `rerun_task`'s 评审 Critical fix). The fresh read flips a now-terminal
        //     run back to `running`; an already-`running` run is left as-is.
        let current_status = self
            .run_repo
            .get_run(run_id)
            .await
            .map_err(OrchestratorError::from)?
            .map(|r| r.status)
            .unwrap_or_else(|| run.status.clone());
        if matches!(current_status.as_str(), "completed" | "failed" | "cancelled") {
            self.run_repo
                .update_run(
                    run_id,
                    UpdateRunParams {
                        status: Some("running".to_string()),
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
            self.emitter.emit_run_status(run_id, "running");
        }

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

/// **Optimistic create (B3): background plan + engine start.** Spawn the run's
/// planning (and, for non-`interactive` autonomy, the engine loop) onto a detached
/// `tokio` task so the HTTP handler can return the freshly-created `planning`-state
/// run IMMEDIATELY — closing the long "submit → blank wait" gap (the lead's
/// planning thought then streams in over `leadThinking` while the FE shows the run).
///
/// Used by BOTH front doors so they stay consistent:
/// - the Tab route [`create_adhoc_run`](crate::routes) (the空挡 complaint's source), and
/// - the MCP/caps front door (`caps_orchestrator::create`).
///
/// **Fail-soft (no panic):** [`RunService::plan`] is itself fail-soft for a bad
/// plan (it degrades a cyclic/garbled DAG to the degenerate single-task plan via
/// `degenerate_plan`/`fallback_dag`), so the only `Err` it returns is a genuine
/// infrastructure failure (DB / provider-config). We LOG that and leave the run in
/// `planning` (re-plannable) — we never `unwrap`/panic in the detached task. On a
/// successful plan the autonomy gate inside `plan` already set the run to
/// `running` (non-interactive) or `awaiting_plan_approval` (interactive); we then
/// start the engine for non-interactive runs (an interactive run waits for
/// `approve`). `start` is synchronous (it spawns its own loop) — safe in the task.
pub fn spawn_plan_and_start(
    run_service: Arc<RunService>,
    engine: crate::engine::RunEngine,
    run_id: String,
    autonomy: String,
) {
    tokio::spawn(async move {
        if let Err(err) = run_service.plan(&run_id).await {
            // Fail-soft: a bad plan already degraded inside `plan`; an Err here is a
            // real infra failure. The run stays `planning` (re-plannable) — never
            // panic the detached task.
            tracing::warn!(
                run_id = %run_id,
                error = %err,
                "background planning failed; run left in `planning` (re-plannable)"
            );
            return;
        }
        // `interactive` parks at `awaiting_plan_approval` — do NOT start the engine
        // until the plan is approved. All other autonomy levels start immediately.
        if autonomy != "interactive" {
            engine.start(run_id);
        }
    });
}

/// nomi_spawn 的后台编排：plan_flat（无 planner）→ engine.start。扁平 run 恒为
/// 非 interactive（前门以 supervised 创建），故 plan_flat 成功即直接启动引擎。
/// 与 [`spawn_plan_and_start`] 同样 fail-soft：失败只 warn，run 留在 `planning`
/// 可重试，绝不在 detached task 里 panic。
pub fn spawn_plan_flat_and_start(
    run_service: Arc<RunService>,
    engine: crate::engine::RunEngine,
    run_id: String,
    tasks: Vec<PlannedTask>,
) {
    tokio::spawn(async move {
        if let Err(err) = run_service.plan_flat(&run_id, tasks).await {
            tracing::warn!(
                run_id = %run_id,
                error = %err,
                "flat planning failed; run left in `planning` (re-plannable)"
            );
            return;
        }
        engine.start(run_id);
    });
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

/// Best-effort cost / reasoning / speed tier for a bare model, inferred from
/// common naming conventions, so the capability [`router`](crate::router) can do
/// real cost-vs-effect routing for range members (which otherwise carry no
/// profile). DELIBERATELY conservative and **fail-soft**: only a clear "strong"
/// or clear "light" signal sets tiers; anything unknown or ambiguous returns
/// `None`, which the Router treats as its neutral baseline — i.e. exactly the
/// pre-existing behavior (zero regression). Strengths/modalities/tools are left
/// at the baseline (empty / false), so this only nudges the reasoning + cost-tier
/// scoring arms, never the hard filters (a bare member stays tool-neutral exactly
/// as before).
fn infer_model_capability(model: &str) -> Option<CapabilityProfile> {
    let m = model.to_lowercase();
    // Strong / premium signals (high reasoning, premium cost).
    const STRONG: &[&str] = &[
        "opus", "ultra", "-pro", "gpt-4", "gpt4", "o1", "o3", "-large", "reasoner", "deepseek-r", "-r1",
    ];
    // Light / economy signals (low reasoning, economy cost, fast).
    const LIGHT: &[&str] = &["haiku", "mini", "flash", "lite", "nano", "-small", "-8b", "phi-"];
    let strong = STRONG.iter().any(|k| m.contains(k));
    let light = LIGHT.iter().any(|k| m.contains(k));
    // Only commit when the signal is unambiguous; a name hitting both (or neither)
    // stays None → baseline.
    let (reasoning, cost_tier, speed_tier) = if strong && !light {
        ("high", "premium", "medium")
    } else if light && !strong {
        ("low", "economy", "fast")
    } else {
        return None;
    };
    Some(CapabilityProfile {
        strengths: Vec::new(),
        modalities: Vec::new(),
        tools: false,
        reasoning: reasoning.to_string(),
        cost_tier: cost_tier.to_string(),
        speed_tier: speed_tier.to_string(),
    })
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
            // Best-effort cost/reasoning tier inferred from the model NAME so the
            // capability Router can weigh cost-vs-effect (router.rs's reasoning /
            // cost-tier arms). Unknown / ambiguous names → None → the Router's
            // neutral baseline = current behavior (zero regression). When the
            // caps layer prepends a description-decorated copy of this same
            // (provider, model), that copy wins dedup and the planner reads the
            // richer user description instead — so this only fires where there is
            // no description, exactly where the Router needs a signal.
            capability_profile: infer_model_capability(model),
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

/// Float the fleet member matching `lead_model` to the front of `members` so the
/// planner's [`pick_lead`](crate::plan) (first member with provider+model) selects
/// it as the run's lead/planner, then re-densify `sort_order` to the new index.
///
/// Prefers the BARE model member (empty `agent_id`) over an assistant-backed
/// member that merely shares the same `(provider, model)`, so the planner runs as
/// the pure 主模型 the user picked rather than an assistant. A `None` `lead_model`
/// or no match is a no-op — the snapshot (and thus the engine's positional default)
/// is returned unchanged, so uncurated / Auto runs keep their current behavior.
fn float_lead_member(mut members: Vec<FleetMember>, lead_model: Option<&ModelRef>) -> Vec<FleetMember> {
    let Some(lead) = lead_model else {
        return members;
    };
    let is_match = |m: &FleetMember| {
        m.provider_id.as_deref() == Some(lead.provider_id.as_str())
            && m.model.as_deref() == Some(lead.model.as_str())
    };
    let pos = members
        .iter()
        .position(|m| is_match(m) && m.agent_id.is_empty())
        .or_else(|| members.iter().position(is_match));
    if let Some(pos) = pos {
        if pos != 0 {
            let lead_member = members.remove(pos);
            members.insert(0, lead_member);
        }
        for (i, m) in members.iter_mut().enumerate() {
            m.sort_order = i as i64;
        }
    }
    members
}

/// The member + score/rationale chosen for a task during planning.
struct AssignmentPick {    member_id: String,
    score: Option<f64>,
    rationale: Option<String>,
}

/// Decide which member a task should be assigned to (the PURE routing decision,
/// no DB / no emit): the LLM-primary + Router-veto pick used by both
/// [`RunService::assign_task`] (which then persists + emits) and
/// [`RunService::adjust`] (which precomputes every new task's pick in memory so
/// the transactional reconcile only persists). Returns `None` ONLY when the
/// snapshot has no members (nothing to assign to). The logic is unchanged from
/// the original inline `assign_task` body:
/// - all members hard-filtered (`ranked` empty) → fall back to the planner's
///   `member_index` (or member 0), recording no score + the planner's rationale;
/// - else honor a VIABLE planner pick (present in `ranked` = survived the hard
///   filters), recording the Router's score/rationale; otherwise the Router top.
fn resolve_assignment_pick(
    members: &[FleetMember],
    task_profile: Option<&TaskProfile>,
    member_index: Option<usize>,
    rationale: Option<&str>,
) -> Option<AssignmentPick> {
    let profile = task_profile.cloned().unwrap_or_else(default_profile);
    let ranked = rank_members(members, &profile);

    if ranked.is_empty() {
        // All hard-filtered: fall back so the task still gets assigned.
        resolve_member(members, member_index).map(|m| AssignmentPick {
            member_id: m.id.clone(),
            score: None,
            rationale: rationale.map(str::to_string),
        })
    } else {
        // Honor a VIABLE pre-assignment (present in `ranked` = survived the hard
        // filters); else fall back to the Router's top pick.
        let planner_choice =
            member_index.and_then(|mi| ranked.iter().find(|c| c.member_index == mi));
        let chosen = planner_choice.unwrap_or(&ranked[0]);
        members.get(chosen.member_index).map(|m| AssignmentPick {
            member_id: m.id.clone(),
            score: Some(chosen.score),
            rationale: Some(chosen.rationale.clone()),
        })
    }
}

/// Detect a dependency CYCLE in a resolved conversational-reconcile plan, over
/// the FULL final task graph (kept tasks + new tasks), BEFORE any write
/// (UC-3a 评审 Important-B). A cycle would persist a DAG the engine can never make
/// ready (a soft-strand: the run stays `running` with un-runnable pending tasks),
/// so `adjust` rejects a cyclic plan and leaves the run unchanged.
///
/// **Nodes** are the kept task ids (each surviving kept task is a node) PLUS one
/// node per NEW task (identified by its index in `new_tasks`). **Edges** are
/// `blocker → blocked`: each new task `i`'s `depends_on` lists its blockers, so a
/// `Kept(id)` ref is an edge `id → new#i` and a `NewIndex(j)` ref is `new#j →
/// new#i`. Kept tasks declare no `depends_on` here (their old wiring was cleared;
/// any upstream they regain is expressed by OTHER nodes referencing them), so they
/// contribute no OUTGOING edges — a cycle therefore must run through at least one
/// new task, but we still model kept nodes so a new→kept→new path is detected.
///
/// Pure + bounded: a Kahn topological sort (repeatedly remove a zero-in-degree
/// node). If any node remains when no zero-in-degree node is left, the remaining
/// nodes form a cycle. Self-edges (a `NewIndex(i)` on task `i`) are an immediate
/// cycle. Out-of-range new indices cannot occur (the caller validated ranges).
fn reconcile_plan_has_cycle(
    kept_ids: &std::collections::HashSet<String>,
    new_tasks: &[ReconcileNewTask],
) -> bool {
    use std::collections::HashMap;

    // Assign every node a dense index: kept ids first, then one per new task.
    // A new task `i` is keyed `new#i` so its node id is `kept_ids.len() + i`.
    let mut node_index: HashMap<String, usize> = HashMap::new();
    for id in kept_ids {
        let n = node_index.len();
        node_index.insert(id.clone(), n);
    }
    let kept_count = node_index.len();
    let new_node = |i: usize| kept_count + i;
    let total = kept_count + new_tasks.len();

    // Build adjacency (blocker → [blocked]) + in-degrees over the resolved edges.
    let mut adj: Vec<Vec<usize>> = vec![Vec::new(); total];
    let mut in_degree: Vec<usize> = vec![0; total];
    for (i, nt) in new_tasks.iter().enumerate() {
        let blocked = new_node(i);
        for dep in &nt.depends_on {
            let blocker = match dep {
                ReconcileDepRef::Kept(id) => match node_index.get(id) {
                    Some(&n) => n,
                    // A kept ref the caller didn't include as a kept node can't form
                    // a cycle (it has no outgoing edges); skip it defensively.
                    None => continue,
                },
                ReconcileDepRef::NewIndex(j) => {
                    if *j >= new_tasks.len() {
                        continue; // out-of-range guarded upstream; skip defensively
                    }
                    new_node(*j)
                }
            };
            // A self-edge (blocker == blocked) is itself a cycle.
            if blocker == blocked {
                return true;
            }
            adj[blocker].push(blocked);
            in_degree[blocked] += 1;
        }
    }

    // Kahn: pop zero-in-degree nodes; count how many we can remove. If fewer than
    // `total` are removable, the rest are in a cycle.
    let mut queue: Vec<usize> = (0..total).filter(|&n| in_degree[n] == 0).collect();
    let mut removed = 0usize;
    while let Some(n) = queue.pop() {
        removed += 1;
        for &m in &adj[n] {
            in_degree[m] -= 1;
            if in_degree[m] == 0 {
                queue.push(m);
            }
        }
    }
    removed != total
}

/// Acyclicity check for the INITIAL planned DAG (symmetric with the `adjust`
/// path's [`reconcile_plan_has_cycle`], but over the planner's index-keyed graph
/// instead of the kept-id/new-index reconcile graph). Nodes are the planned tasks
/// (one per index); edges are `blocker → blocked`: each task `i`'s `depends_on`
/// lists its blocker indices, so a `dep` on task `i` is an edge `dep → i`.
///
/// Pure + bounded: a Kahn topological sort (repeatedly drop a zero-in-degree
/// node). If any node remains when none has zero in-degree, the remainder forms a
/// cycle. A self-edge (`i` in its own `depends_on`) is an immediate cycle.
/// Out-of-range `depends_on` indices contribute no edge (they're dropped — the
/// caller already range-validates them when wiring edges, warning + skipping).
fn planned_dag_has_cycle(dag: &PlannedDag) -> bool {
    let total = dag.tasks.len();
    if total == 0 {
        return false;
    }
    let mut adj: Vec<Vec<usize>> = vec![Vec::new(); total];
    let mut in_degree: Vec<usize> = vec![0; total];
    for (blocked, task) in dag.tasks.iter().enumerate() {
        for &dep in &task.depends_on {
            // Out-of-range dep: no edge (range-validated + skipped at wire time).
            if dep >= total {
                continue;
            }
            // A self-edge (a task depending on itself) is itself a cycle.
            if dep == blocked {
                return true;
            }
            adj[dep].push(blocked);
            in_degree[blocked] += 1;
        }
    }

    let mut queue: Vec<usize> = (0..total).filter(|&n| in_degree[n] == 0).collect();
    let mut removed = 0usize;
    while let Some(n) = queue.pop() {
        removed += 1;
        for &m in &adj[n] {
            in_degree[m] -= 1;
            if in_degree[m] == 0 {
                queue.push(m);
            }
        }
    }
    removed != total
}

/// The degenerate single-task plan: the whole goal as one plain `agent` task
/// assigned to member 0 (mirrors the planner's own unparseable-output fallback in
/// `plan::fallback_dag`, kept here so the cyclic-plan guard can degrade without
/// reaching into `plan`'s privates). Always acyclic (no `depends_on`), so the run
/// still proceeds with something runnable.
fn degenerate_plan(goal: &str) -> PlannedDag {
    let trimmed = goal.trim();
    // CJK-safe ~60-char title from the goal (a long goal becomes a short title).
    let title: String = if trimmed.chars().count() <= 60 {
        trimmed.to_string()
    } else {
        let head: String = trimmed.chars().take(60).collect();
        format!("{head}…")
    };
    PlannedDag {
        tasks: vec![PlannedTask {
            title,
            spec: goal.to_string(),
            task_profile: None,
            depends_on: vec![],
            member_index: Some(0),
            rationale: Some("fallback: planner produced a cyclic DAG".to_string()),
            role: None,
            kind: "agent".to_string(),
            pattern_config: None,
        }],
    }
}
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
        // 迁移 023: pass through the task mode + optional config (raw JSON text,
        // like task_profile). Legacy rows read back `kind = "agent"` (column
        // default) so the DTO is unchanged for them.
        kind: row.kind,
        pattern_config: row.pattern_config,
        // 迁移 026: 启动前配置台的模型覆盖 + 预置要求。旧行读回 None。
        override_provider_id: row.override_provider_id,
        override_model: row.override_model,
        preset_prompt: row.preset_prompt,
        last_error: row.last_error,
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

    /// Broadcaster that records every event's `(name, status)` so a test can assert
    /// on the emitted event trail (e.g. that a deleted task emits `planUpdated`, not
    /// a fake `task.statusChanged="removed"`). `status` is the payload's `status`
    /// field (when present).
    #[derive(Clone)]
    struct RecordingBroadcaster {
        events: Arc<Mutex<Vec<(String, Option<String>)>>>,
    }
    impl RecordingBroadcaster {
        fn new() -> Self {
            Self { events: Arc::new(Mutex::new(Vec::new())) }
        }
        fn names(&self) -> Vec<String> {
            self.events.lock().unwrap().iter().map(|(n, _)| n.clone()).collect()
        }
        /// All recorded `(name, status)` pairs whose payload carried a `status`.
        fn statuses(&self) -> Vec<(String, String)> {
            self.events
                .lock()
                .unwrap()
                .iter()
                .filter_map(|(n, s)| s.clone().map(|s| (n.clone(), s)))
                .collect()
        }
    }
    impl EventBroadcaster for RecordingBroadcaster {
        fn broadcast(&self, event: WebSocketMessage<serde_json::Value>) {
            let status = event.data.get("status").and_then(|v| v.as_str()).map(str::to_string);
            self.events.lock().unwrap().push((event.name.clone(), status));
        }
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
            _sink: Option<&crate::plan::LeadThinkingSink>,
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
                kind: "agent".to_string(),
                pattern_config: None,
            }],
        }
    }

    // ── B3: plan() phase-narration leadThinking events ───────────────────────
    //
    // A broadcaster that records the ORDERED `content` key of every leadThinking
    // `kind:"phase"` frame so a test can assert the deterministic phase narration
    // `plan()` emits (planning_started → decomposing → assigning). The content is a
    // SEMANTIC KEY (the frontend owns the i18n copy); the backend never sends prose.
    #[derive(Clone)]
    struct PhaseRecordingBroadcaster {
        phases: Arc<Mutex<Vec<String>>>,
    }
    impl PhaseRecordingBroadcaster {
        fn new() -> Self {
            Self { phases: Arc::new(Mutex::new(Vec::new())) }
        }
        fn phase_keys(&self) -> Vec<String> {
            self.phases.lock().unwrap().clone()
        }
    }
    impl EventBroadcaster for PhaseRecordingBroadcaster {
        fn broadcast(&self, event: WebSocketMessage<serde_json::Value>) {
            if event.name == "orchestrator.run.leadThinking"
                && event.data.get("kind").and_then(|v| v.as_str()) == Some("phase")
            {
                if let Some(key) = event.data.get("content").and_then(|v| v.as_str()) {
                    self.phases.lock().unwrap().push(key.to_string());
                }
            }
        }
    }

    /// `plan()` emits an ORDERED sequence of `kind:"phase"` leadThinking events at
    /// its key milestones — planning_started (before the lead call), decomposing
    /// (after a DAG is in hand, before persisting tasks), assigning (before routing
    /// assignments) — so the frontend has a deterministic, provider-independent
    /// progress narrative even when no reasoning stream is available. The content
    /// is a SEMANTIC KEY, never prose (i18n lives in the frontend).
    #[tokio::test]
    async fn plan_emits_ordered_phase_narration_events() {
        let db = init_database_memory().await.expect("db init");
        let pool = db.pool().clone();
        let run_repo = Arc::new(SqliteRunRepository::new(pool.clone()));
        let fleet_repo = Arc::new(SqliteFleetRepository::new(pool.clone()));
        let ws_repo = Arc::new(SqliteOrchWorkspaceRepository::new(pool.clone()));
        let bc = Arc::new(PhaseRecordingBroadcaster::new());
        let emitter = OrchestratorRunEventEmitter::new(bc.clone());
        let planner: Arc<dyn PlanProducer> =
            Arc::new(FixedPlanProducer::new(single_task_dag(Some(0), None)));
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
                    name: "phase fleet".to_string(),
                    description: None,
                    max_parallel: None,
                    members: vec![member_input("agent_a", &[], "medium", "standard")],
                },
            )
            .await
            .expect("fleet create");
        let ws = WorkspaceService::new(ws_repo)
            .create(
                "u1",
                CreateWorkspaceRequest {
                    name: "phase ws".to_string(),
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

        run_service.plan(&run.id).await.expect("plan");

        // The three deterministic phase keys, IN ORDER.
        assert_eq!(
            bc.phase_keys(),
            vec![
                "planning_started".to_string(),
                "decomposing".to_string(),
                "assigning".to_string(),
            ],
            "plan() must emit planning_started → decomposing → assigning phase narration"
        );
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

    // 迁移 026 set_task_config: FULL-replace of model override + preset with
    // trim/all-or-nothing normalization, and the owner / running guards.
    #[tokio::test]
    async fn set_task_config_full_replace_normalize_and_guards() {
        let members = vec![
            member_input("agent_a", &["coding"], "high", "standard"),
            member_input("agent_b", &["writing"], "medium", "standard"),
        ];
        let dag = single_task_dag(None, Some(coding_profile()));
        let (svc, repo, _snapshot, run_id) = harness(members, dag).await;
        svc.plan(&run_id).await.expect("plan");
        let task_id = svc.get_detail(&run_id).await.unwrap().tasks[0].id.clone();

        // Full config: model override (both) + preset (trimmed).
        svc.set_task_config(
            "u1",
            &run_id,
            &task_id,
            Some("prov_x".into()),
            Some("model_y".into()),
            Some("  必须用中文  ".into()),
        )
        .await
        .expect("set full");
        let t = repo.get_task(&task_id).await.unwrap().unwrap();
        assert_eq!(t.override_provider_id.as_deref(), Some("prov_x"));
        assert_eq!(t.override_model.as_deref(), Some("model_y"));
        assert_eq!(t.preset_prompt.as_deref(), Some("必须用中文"), "preset trimmed");

        // Half-set (model only, no provider) → BOTH model fields cleared.
        svc.set_task_config("u1", &run_id, &task_id, None, Some("model_z".into()), Some("keep".into()))
            .await
            .expect("half-set");
        let t = repo.get_task(&task_id).await.unwrap().unwrap();
        assert_eq!(t.override_provider_id, None, "half-set clears provider");
        assert_eq!(t.override_model, None, "half-set clears model");
        assert_eq!(t.preset_prompt.as_deref(), Some("keep"));

        // FULL replace with empties → all three cleared.
        svc.set_task_config("u1", &run_id, &task_id, None, None, Some("   ".into()))
            .await
            .expect("clear");
        let t = repo.get_task(&task_id).await.unwrap().unwrap();
        assert_eq!(t.override_provider_id, None);
        assert_eq!(t.override_model, None);
        assert_eq!(t.preset_prompt, None, "blank preset cleared");

        // Non-owner is rejected (owned_run guard).
        assert!(
            svc.set_task_config("intruder", &run_id, &task_id, None, None, None).await.is_err(),
            "non-owner rejected"
        );

        // A running task is rejected (must pause/stop first).
        repo.update_task(&task_id, UpdateTaskParams { status: Some("running".into()), ..Default::default() })
            .await
            .unwrap();
        assert!(
            svc.set_task_config("u1", &run_id, &task_id, None, None, Some("y".into())).await.is_err(),
            "running task rejected"
        );
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

    // ----- plan_flat（nomi_spawn 扁平 fan-out，跳过 planner）-----

    fn flat_task(title: &str, spec: &str) -> PlannedTask {
        PlannedTask {
            title: title.to_string(),
            spec: spec.to_string(),
            task_profile: None,
            depends_on: vec![],
            member_index: None,
            rationale: None,
            role: None,
            kind: "agent".to_string(),
            pattern_config: None,
        }
    }

    /// supervised 的 adhoc run（nomi_spawn 前门的形状：单模型、无审批门）。
    async fn flat_run(svc: &RunService) -> String {
        let req = CreateAdhocRunRequest {
            goal: "并行执行子任务".to_string(),
            work_dir: None,
            model_range: ModelRange::Single {
                model: model_ref("prov_a", "model-a"),
            },
            pinned_roles: vec![],
            role_members: vec![],
            autonomy: Some("supervised".to_string()),
            max_parallel: None,
            lead_conv_id: None,
            lead_model: None,
        };
        svc.create_adhoc("u1", req).await.expect("create_adhoc").id
    }

    #[tokio::test]
    async fn plan_flat_persists_tasks_assignments_and_activates() {
        let (svc, _repo) = adhoc_service().await;
        let run_id = flat_run(&svc).await;

        svc.plan_flat(&run_id, vec![flat_task("查 A", "搜索模块 A 的用法"), flat_task("查 B", "搜索模块 B 的用法")])
            .await
            .expect("plan_flat");

        let detail = svc.get_detail(&run_id).await.expect("detail");
        assert_eq!(detail.tasks.len(), 2);
        assert!(detail.deps.is_empty(), "扁平 run 无依赖边");
        assert_eq!(detail.run.status, "running", "supervised 直接 running（autonomy 门复用）");
        for t in &detail.tasks {
            assert_eq!(t.status, "pending");
        }
        // 每个任务必须有 assignment（引擎 dispatch 需要）。
        assert_eq!(detail.assignments.len(), 2);
    }

    #[tokio::test]
    async fn plan_flat_rejects_empty_tasks() {
        let (svc, _repo) = adhoc_service().await;
        let run_id = flat_run(&svc).await;
        let err = svc.plan_flat(&run_id, vec![]).await.expect_err("empty must reject");
        assert!(
            matches!(err, AppError::BadRequest(_)),
            "空任务列表必须拒绝，否则 run 立即 stuck: {err:?}"
        );
    }

    #[tokio::test]
    async fn plan_flat_persists_synthesis_dep_edges() {
        // 携带 depends_on 的 synthesis 任务（nomi_spawn synthesize=true 的形状）也能落边。
        let (svc, _repo) = adhoc_service().await;
        let run_id = flat_run(&svc).await;
        let mut synth = flat_task("综合", "汇总各子任务产出并标注冲突");
        synth.kind = "synthesis".to_string();
        synth.depends_on = vec![0, 1];
        synth.role = Some("reviewer".to_string());
        svc.plan_flat(&run_id, vec![flat_task("A", "a"), flat_task("B", "b"), synth])
            .await
            .expect("plan_flat");
        let detail = svc.get_detail(&run_id).await.expect("detail");
        assert_eq!(detail.tasks.len(), 3);
        assert_eq!(detail.deps.len(), 2, "synthesis 依赖两个上游");
        // role 持久化到任务行（worker 端据此收缩工具）。
        let synth_task = detail.tasks.iter().find(|t| t.kind == "synthesis").expect("synthesis task");
        assert_eq!(synth_task.role.as_deref(), Some("reviewer"));
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
            lead_model: None,
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
            lead_model: None,
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
            lead_model: None,
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
            lead_model: None,
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
            lead_model: None,
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

    // 主模型 as planner: float_lead_member moves the member matching `lead_model`
    // to the front (so `pick_lead` selects it), preferring the BARE model member
    // over an assistant that merely shares the same (provider, model).
    #[test]
    fn float_lead_member_floats_bare_main_to_front() {
        // Range: collaborator first, 主模型 (p1/m1) second. Plus an assistant pinned
        // to the SAME (p1, m1) — merge_members puts the assistant (role) first, so
        // without floating, pick_lead would pick the collaborator (index 0).
        let range = build_members_from_range(&ModelRange::Range {
            models: vec![model_ref("p0", "collab"), model_ref("p1", "m1")],
        })
        .expect("range");
        let roles = vec![enriched_member("asst_x", "p1", "m1", "助手X")];
        let merged = merge_members(range, roles);

        let floated = float_lead_member(merged, Some(&model_ref("p1", "m1")));
        assert_eq!(floated[0].provider_id.as_deref(), Some("p1"));
        assert_eq!(floated[0].model.as_deref(), Some("m1"));
        assert!(
            floated[0].agent_id.is_empty(),
            "the floated lead must be the pure 主模型 (empty agent_id), not the assistant"
        );
        // sort_order re-densified to the new positions.
        for (i, m) in floated.iter().enumerate() {
            assert_eq!(m.sort_order, i as i64, "sort_order re-densified after float");
        }
    }

    // float_lead_member is a no-op when `lead_model` is None or matches no member —
    // the snapshot (and thus the engine's positional default) is unchanged.
    #[test]
    fn float_lead_member_none_or_unmatched_is_noop() {
        let members = build_members_from_range(&ModelRange::Range {
            models: vec![model_ref("p0", "a"), model_ref("p1", "b")],
        })
        .expect("range");
        let first = members[0].provider_id.clone();

        let unchanged = float_lead_member(members.clone(), None);
        assert_eq!(unchanged[0].provider_id, first, "None lead_model is a no-op");

        let unchanged2 = float_lead_member(members, Some(&model_ref("nope", "nope")));
        assert_eq!(unchanged2[0].provider_id, first, "unmatched lead_model is a no-op");
    }

    // 裸模型能力档启发式: clear strong/light names get tiered; unknown / ambiguous
    // names stay None (= Router baseline, zero regression).
    #[test]
    fn infer_model_capability_tiers_by_name() {
        let opus = infer_model_capability("claude-opus-4-8").expect("opus → strong");
        assert_eq!(opus.reasoning, "high");
        assert_eq!(opus.cost_tier, "premium");
        // Bare members never claim tools/strengths — hard filters unchanged.
        assert!(opus.strengths.is_empty() && !opus.tools);

        let haiku = infer_model_capability("claude-haiku-4-5").expect("haiku → light");
        assert_eq!(haiku.reasoning, "low");
        assert_eq!(haiku.cost_tier, "economy");
        assert_eq!(haiku.speed_tier, "fast");

        let flash = infer_model_capability("gemini-2.5-flash").expect("flash → light");
        assert_eq!(flash.cost_tier, "economy");

        // Unknown → None (baseline). Ambiguous (matches BOTH strong+light) → None.
        assert!(infer_model_capability("some-unknown-model").is_none());
        assert!(
            infer_model_capability("gpt-4o-mini").is_none(),
            "a name hitting both strong (gpt-4) and light (mini) stays neutral"
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
            lead_model: None,
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
                    kind: "agent".to_string(),
                    pattern_config: None,
                },
                PlannedTask {
                    title: "无角色任务".to_string(),
                    spec: "做点别的".to_string(),
                    task_profile: None,
                    depends_on: vec![],
                    member_index: Some(0),
                    rationale: None,
                    role: None,
                    kind: "agent".to_string(),
                    pattern_config: None,
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
                    kind: "agent".to_string(),
                    pattern_config: None,
                },
                PlannedTask {
                    title: "B".to_string(),
                    spec: "b".to_string(),
                    task_profile: None,
                    depends_on: vec![0],
                    member_index: Some(0),
                    rationale: None,
                    role: None,
                    kind: "agent".to_string(),
                    pattern_config: None,
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
            _sink: Option<&crate::plan::LeadThinkingSink>,
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
                    kind: "agent".to_string(),
                    pattern_config: None,
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
            lead_model: None,
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

    // -------------------------------------------------------------------------
    // UC-3a: conversation-driven intelligent re-adjust (adjust + reconcile).
    // -------------------------------------------------------------------------

    use crate::plan::{AdjustedDepRef, AdjustedNewTask, AdjustedNode, AdjustedPlan};

    /// A planner whose `produce` returns a fixed initial dag and whose `adjust`
    /// returns a pre-staged [`AdjustedPlan`] (or a configurable error), so a test
    /// drives reconcile with the EXACT adjusted plan it wants — no live LLM.
    struct AdjustTestProducer {
        initial: PlannedDag,
        adjusted: Mutex<Result<AdjustedPlan, AppError>>,
    }
    impl AdjustTestProducer {
        fn new(initial: PlannedDag, adjusted: AdjustedPlan) -> Self {
            Self { initial, adjusted: Mutex::new(Ok(adjusted)) }
        }
        fn with_error(initial: PlannedDag, err: AppError) -> Self {
            Self { initial, adjusted: Mutex::new(Err(err)) }
        }
    }
    #[async_trait::async_trait]
    impl PlanProducer for AdjustTestProducer {
        async fn produce(&self, _goal: &str, _members: &[FleetMember], _sink: Option<&crate::plan::LeadThinkingSink>) -> Result<PlannedDag, AppError> {
            Ok(self.initial.clone())
        }
        async fn adjust(
            &self,
            _intent: &str,
            _tasks: &[RunTask],
            _deps: &[RunTaskDep],
            _members: &[FleetMember],
            _sink: Option<&crate::plan::LeadThinkingSink>,
        ) -> Result<AdjustedPlan, AppError> {
            match &*self.adjusted.lock().unwrap() {
                Ok(p) => Ok(p.clone()),
                Err(AppError::BadRequest(m)) => Err(AppError::BadRequest(m.clone())),
                Err(e) => Err(AppError::Internal(format!("{e}"))),
            }
        }
    }

    /// Build a RunService whose planner stages `initial` (for the first `plan`) +
    /// `adjusted` (for `adjust`), seed a workspace + single-member fleet, create a
    /// run, plan it, and return (svc, run_id). The run is `running` after plan.
    async fn adjust_harness(initial: PlannedDag, adjusted: AdjustedPlan) -> (RunService, String) {
        adjust_harness_with(AdjustTestProducer::new(initial, adjusted)).await
    }

    async fn adjust_harness_with(producer: AdjustTestProducer) -> (RunService, String) {
        let db = init_database_memory().await.expect("db init");
        let pool = db.pool().clone();
        let run_repo = Arc::new(SqliteRunRepository::new(pool.clone()));
        let fleet_repo = Arc::new(SqliteFleetRepository::new(pool.clone()));
        let ws_repo = Arc::new(SqliteOrchWorkspaceRepository::new(pool.clone()));
        let emitter = OrchestratorRunEventEmitter::new(Arc::new(NoopBroadcaster));
        let planner: Arc<dyn PlanProducer> = Arc::new(producer);
        let svc = RunService::new(run_repo, fleet_repo.clone(), ws_repo.clone(), planner, emitter);
        let fleet = FleetService::new(fleet_repo)
            .create(
                "u1",
                CreateFleetRequest {
                    name: "adjust fleet".to_string(),
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
                    name: "adjust ws".to_string(),
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
                    autonomy: Some("supervised".to_string()),
                    max_parallel: None,
                },
            )
            .await
            .expect("run create");
        svc.plan(&run.id).await.expect("plan");
        (svc, run.id)
    }

    /// Like [`adjust_harness`] but wires a [`RecordingBroadcaster`] so a test can
    /// assert on the emitted event trail. Returns the recorder too. Events from the
    /// initial `plan` are CLEARED before returning so a test sees only its `adjust`.
    async fn adjust_harness_recording(
        initial: PlannedDag,
        adjusted: AdjustedPlan,
    ) -> (RunService, String, RecordingBroadcaster) {
        let db = init_database_memory().await.expect("db init");
        let pool = db.pool().clone();
        let run_repo = Arc::new(SqliteRunRepository::new(pool.clone()));
        let fleet_repo = Arc::new(SqliteFleetRepository::new(pool.clone()));
        let ws_repo = Arc::new(SqliteOrchWorkspaceRepository::new(pool.clone()));
        let recorder = RecordingBroadcaster::new();
        let emitter = OrchestratorRunEventEmitter::new(Arc::new(recorder.clone()));
        let planner: Arc<dyn PlanProducer> = Arc::new(AdjustTestProducer::new(initial, adjusted));
        let svc = RunService::new(run_repo, fleet_repo.clone(), ws_repo.clone(), planner, emitter);
        let fleet = FleetService::new(fleet_repo)
            .create(
                "u1",
                CreateFleetRequest {
                    name: "adjust fleet".to_string(),
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
                    name: "adjust ws".to_string(),
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
                    autonomy: Some("supervised".to_string()),
                    max_parallel: None,
                },
            )
            .await
            .expect("run create");
        svc.plan(&run.id).await.expect("plan");
        recorder.events.lock().unwrap().clear();
        (svc, run.id, recorder)
    }

    /// Mark a task `done` with an output summary (simulating a completed worker)
    /// so a KEEP test can assert the output survived reconcile.
    async fn mark_done(svc: &RunService, task_id: &str, output: &str) {
        svc.run_repo
            .update_task(
                task_id,
                UpdateTaskParams {
                    status: Some("done".to_string()),
                    output_summary: Some(Some(output.to_string())),
                    conversation_id: Some(Some(4242)),
                    ..Default::default()
                },
            )
            .await
            .expect("mark done");
    }

    /// An adjusted node keeping an existing task by id.
    fn keep(id: &str) -> AdjustedNode {
        AdjustedNode::Keep { keep: id.to_string() }
    }
    /// An adjusted NEW task with the given title + deps.
    fn new_node(title: &str, deps: Vec<AdjustedDepRef>) -> AdjustedNode {
        AdjustedNode::New(AdjustedNewTask {
            title: title.to_string(),
            spec: format!("spec-{title}"),
            role: None,
            kind: "agent".to_string(),
            pattern_config: None,
            depends_on: deps,
        })
    }

    // KEEP + ADD: the adjusted plan keeps a DONE task (its output preserved, NOT
    // re-run) and adds a NEW task depending on it (kept-id string ref). Reconcile
    // preserves the done task + its assignment, inserts+routes the new pending
    // task, wires the dep, and leaves the done task done.
    #[tokio::test]
    async fn adjust_keep_done_and_add_new_preserves_output_and_wires_dep() {
        let (svc, run_id) = adjust_harness(
            single_task_dag(Some(0), Some(coding_profile())),
            AdjustedPlan { tasks: vec![] }, // placeholder; reset below
        )
        .await;
        // The initial plan has one task; complete it.
        let before = svc.get_detail(&run_id).await.expect("detail");
        assert_eq!(before.tasks.len(), 1);
        let done_id = before.tasks[0].id.clone();
        mark_done(&svc, &done_id, "已完成的产出").await;

        // Re-stage the adjusted plan to keep that task + add one depending on it.
        // (We rebuild the harness producer's staged plan via a fresh service so the
        // ids line up — simplest is to drive adjust through a producer that returns
        // a plan referencing the real done_id.)
        let svc = restage_adjust(svc, AdjustedPlan {
            tasks: vec![keep(&done_id), new_node("扩展", vec![AdjustedDepRef::Kept(done_id.clone())])],
        });

        let run = svc.adjust("u1", &run_id, "在已完成工作基础上扩展").await.expect("adjust");
        assert_eq!(run.status, "running", "terminal-or-running run is running after adjust");

        let after = svc.get_detail(&run_id).await.expect("detail");
        // Two tasks now: the kept done one + the new pending one.
        assert_eq!(after.tasks.len(), 2, "kept + new");
        let kept = after.tasks.iter().find(|t| t.id == done_id).expect("kept task survives");
        assert_eq!(kept.status, "done", "kept task NOT re-run");
        assert_eq!(kept.output_summary.as_deref(), Some("已完成的产出"), "output preserved");
        assert_eq!(kept.conversation_id, Some(4242), "conversation preserved");
        let new_task = after.tasks.iter().find(|t| t.title == "扩展").expect("new task added");
        assert_eq!(new_task.status, "pending", "new task is pending");
        // The new task is routed (has an auto assignment).
        assert!(
            after.assignments.iter().any(|a| a.task_id == new_task.id && a.source == "auto"),
            "new task routed: {:?}",
            after.assignments
        );
        // The kept done task still has its assignment.
        assert!(
            after.assignments.iter().any(|a| a.task_id == done_id),
            "kept task assignment preserved"
        );
        // The dep done→new is wired.
        assert!(
            after.deps.iter().any(|d| d.blocker_task_id == done_id && d.blocked_task_id == new_task.id),
            "dep kept→new wired: {:?}",
            after.deps
        );
    }

    // RE-DECOMPOSE: the adjusted plan keeps NOTHING — all old tasks are deleted and
    // the new tasks created (like replan, but via the intelligent reconcile path).
    #[tokio::test]
    async fn adjust_keep_nothing_redecomposes() {
        let (svc, run_id) = adjust_harness(
            dag_with_titles(&["旧A", "旧B"]),
            AdjustedPlan { tasks: vec![] },
        )
        .await;
        let before = svc.get_detail(&run_id).await.expect("detail");
        assert_eq!(before.tasks.len(), 2);
        let old_ids: Vec<String> = before.tasks.iter().map(|t| t.id.clone()).collect();

        let svc = restage_adjust(svc, AdjustedPlan {
            tasks: vec![new_node("新X", vec![]), new_node("新Y", vec![AdjustedDepRef::NewIndex(0)])],
        });
        svc.adjust("u1", &run_id, "全部重做").await.expect("adjust");

        let after = svc.get_detail(&run_id).await.expect("detail");
        assert_eq!(after.tasks.len(), 2, "two new tasks");
        for old in &old_ids {
            assert!(!after.tasks.iter().any(|t| &t.id == old), "old task {old} deleted");
        }
        let titles: Vec<&str> = after.tasks.iter().map(|t| t.title.as_str()).collect();
        assert!(titles.contains(&"新X") && titles.contains(&"新Y"), "new tasks present: {titles:?}");
    }

    // DEPS REBUILD: a NEW task whose depends_on MIXES a kept-id string ref and a
    // new-index int ref resolves both to concrete edges.
    #[tokio::test]
    async fn adjust_rebuilds_mixed_kept_and_new_index_deps() {
        let (svc, run_id) = adjust_harness(
            single_task_dag(Some(0), None),
            AdjustedPlan { tasks: vec![] },
        )
        .await;
        let before = svc.get_detail(&run_id).await.expect("detail");
        let kept_id = before.tasks[0].id.clone();
        mark_done(&svc, &kept_id, "out").await;

        // Plan: keep[0] = existing; new[1] = "中间" deps [kept]; new[2] = "末" deps
        // [kept (string), node 1 (int)].
        let svc = restage_adjust(svc, AdjustedPlan {
            tasks: vec![
                keep(&kept_id),
                new_node("中间", vec![AdjustedDepRef::Kept(kept_id.clone())]),
                new_node("末", vec![AdjustedDepRef::Kept(kept_id.clone()), AdjustedDepRef::NewIndex(1)]),
            ],
        });
        svc.adjust("u1", &run_id, "扩展两层").await.expect("adjust");

        let after = svc.get_detail(&run_id).await.expect("detail");
        let mid = after.tasks.iter().find(|t| t.title == "中间").expect("中间").id.clone();
        let last = after.tasks.iter().find(|t| t.title == "末").expect("末").id.clone();
        // kept→中间, kept→末, 中间→末 all wired.
        let has = |b: &str, k: &str| after.deps.iter().any(|d| d.blocker_task_id == b && d.blocked_task_id == k);
        assert!(has(&kept_id, &mid), "kept→中间");
        assert!(has(&kept_id, &last), "kept→末 (string ref)");
        assert!(has(&mid, &last), "中间→末 (new-index ref)");
        assert_eq!(after.deps.len(), 3, "exactly the three rebuilt edges: {:?}", after.deps);
    }

    // DROP: an un-kept old task is deleted along with its assignment + deps. Here a
    // two-task run is adjusted to keep only the first; the second must vanish.
    #[tokio::test]
    async fn adjust_drops_unkept_task_and_its_assignment() {
        let (svc, run_id) = adjust_harness(
            dag_with_titles(&["留", "弃"]),
            AdjustedPlan { tasks: vec![] },
        )
        .await;
        let before = svc.get_detail(&run_id).await.expect("detail");
        let keep_id = before.tasks.iter().find(|t| t.title == "留").unwrap().id.clone();
        let drop_id = before.tasks.iter().find(|t| t.title == "弃").unwrap().id.clone();
        assert_eq!(before.assignments.len(), 2);

        let svc = restage_adjust(svc, AdjustedPlan { tasks: vec![keep(&keep_id)] });
        svc.adjust("u1", &run_id, "删掉第二个").await.expect("adjust");

        let after = svc.get_detail(&run_id).await.expect("detail");
        assert_eq!(after.tasks.len(), 1, "only the kept task remains");
        assert!(after.tasks.iter().any(|t| t.id == keep_id), "kept task present");
        assert!(!after.tasks.iter().any(|t| t.id == drop_id), "dropped task gone");
        // The dropped task's assignment cascaded.
        assert!(
            !after.assignments.iter().any(|a| a.task_id == drop_id),
            "dropped task's assignment gone"
        );
    }

    // C item 2: deleting a task during `adjust` emits `run.planUpdated` (the FE
    // refetches the whole RunDetail on it, so the deleted task disappears) and does
    // NOT emit a fake `task.statusChanged="removed"` (which is not a real task
    // status). Asserts on the recorded event trail.
    #[tokio::test]
    async fn adjust_deleting_task_emits_plan_updated_not_fake_removed_status() {
        let (svc, run_id, recorder) = adjust_harness_recording(
            dag_with_titles(&["留", "弃"]),
            AdjustedPlan { tasks: vec![] },
        )
        .await;
        let before = svc.get_detail(&run_id).await.expect("detail");
        let keep_id = before.tasks.iter().find(|t| t.title == "留").unwrap().id.clone();

        let svc = restage_adjust(svc, AdjustedPlan { tasks: vec![keep(&keep_id)] });
        svc.adjust("u1", &run_id, "删掉第二个").await.expect("adjust");

        // The deletion was reflected via a planUpdated event.
        let names = recorder.names();
        assert!(
            names.iter().any(|n| n == "orchestrator.run.planUpdated"),
            "adjust must emit planUpdated so the FE refetches: {names:?}"
        );
        // NO event carried a fake "removed" task status.
        let statuses = recorder.statuses();
        assert!(
            !statuses.iter().any(|(_, s)| s == "removed"),
            "no fake \"removed\" status must be emitted: {statuses:?}"
        );
        // (Defensive) no `task.statusChanged` event at all references the deleted task
        // with a removed status — the deletion is plan-level only.
        assert!(
            !statuses
                .iter()
                .any(|(n, s)| n == "orchestrator.task.statusChanged" && s == "removed"),
            "task.statusChanged must never carry \"removed\": {statuses:?}"
        );
    }

    // SAFETY: adjust with any `running` task is REJECTED (400) and NOTHING is
    // mutated (the chosen safe option — pause first, then re-adjust).
    #[tokio::test]
    async fn adjust_rejects_when_a_task_is_running() {
        let (svc, run_id) = adjust_harness(
            single_task_dag(Some(0), None),
            AdjustedPlan { tasks: vec![] },
        )
        .await;
        let before = svc.get_detail(&run_id).await.expect("detail");
        let tid = before.tasks[0].id.clone();
        // Flip the task to running (a live worker would hold it).
        svc.run_repo
            .update_task(&tid, UpdateTaskParams { status: Some("running".to_string()), ..Default::default() })
            .await
            .expect("set running");

        let svc = restage_adjust(svc, AdjustedPlan { tasks: vec![new_node("新", vec![])] });
        let err = svc.adjust("u1", &run_id, "改改").await.expect_err("must reject");
        assert!(matches!(err, AppError::BadRequest(_)), "got: {err:?}");
        // Nothing changed: still exactly the one running task, no new task added.
        let after = svc.get_detail(&run_id).await.expect("detail");
        assert_eq!(after.tasks.len(), 1, "run untouched");
        assert_eq!(after.tasks[0].id, tid);
        assert!(!after.tasks.iter().any(|t| t.title == "新"), "no new task added");
    }

    // Fix B: cancelling a run SETTLES its interrupted `running` node to `cancelled`
    // (not leaving a phantom「执行中」that would block a later rerun/adjust). The run
    // route calls `engine.stop` first (aborting the loop + cancelling the worker) so
    // by the time the service runs, the `running` row is a guaranteed orphan — here
    // we simulate that static orphan (no live loop) and assert cancel normalizes it.
    #[tokio::test]
    async fn cancel_settles_running_task_to_cancelled() {
        let (svc, run_id) = adjust_harness(
            single_task_dag(Some(0), None),
            AdjustedPlan { tasks: vec![] },
        )
        .await;
        let before = svc.get_detail(&run_id).await.expect("detail");
        let tid = before.tasks[0].id.clone();
        svc.run_repo
            .update_task(&tid, UpdateTaskParams { status: Some("running".to_string()), ..Default::default() })
            .await
            .expect("set running");

        svc.cancel(&run_id).await.expect("cancel");

        let after = svc.get_detail(&run_id).await.expect("detail");
        assert_eq!(after.run.status, "cancelled", "run cancelled");
        assert_eq!(
            after.tasks[0].status, "cancelled",
            "interrupted running node settled to cancelled (no phantom 执行中)"
        );
    }

    // OWNER-SCOPE: a non-owner is rejected (403) and the run is untouched.
    #[tokio::test]
    async fn adjust_cross_user_is_forbidden() {
        let (svc, run_id) = adjust_harness(
            dag_with_titles(&["A"]),
            AdjustedPlan { tasks: vec![] },
        )
        .await;
        let svc = restage_adjust(svc, AdjustedPlan { tasks: vec![new_node("X", vec![])] });
        let err = svc.adjust("intruder", &run_id, "盗改").await.expect_err("cross-user must reject");
        assert!(matches!(err, AppError::Forbidden(_)), "got: {err:?}");
        let after = svc.get_detail(&run_id).await.expect("detail");
        assert_eq!(after.tasks.len(), 1, "non-owner adjust must not mutate");
        assert_eq!(after.tasks[0].title, "A");
    }

    // FAIL-SOFT PARSE: a bad adjusted plan (the producer errors, as parse would on
    // garbage) surfaces an error and the run is UNCHANGED (no partial mutation).
    #[tokio::test]
    async fn adjust_bad_plan_errors_and_leaves_run_unchanged() {
        let (svc, run_id) = adjust_harness_with(AdjustTestProducer::with_error(
            dag_with_titles(&["A", "B"]),
            AppError::BadRequest("主 agent 调整计划无法解析;运行未改动".to_string()),
        ))
        .await;
        let before = svc.get_detail(&run_id).await.expect("detail");
        assert_eq!(before.tasks.len(), 2);
        let ids: Vec<String> = before.tasks.iter().map(|t| t.id.clone()).collect();

        let err = svc.adjust("u1", &run_id, "乱来").await.expect_err("bad plan must error");
        assert!(matches!(err, AppError::BadRequest(_)), "got: {err:?}");

        let after = svc.get_detail(&run_id).await.expect("detail");
        assert_eq!(after.tasks.len(), 2, "run unchanged on parse failure");
        for id in &ids {
            assert!(after.tasks.iter().any(|t| &t.id == id), "task {id} survives");
        }
        assert_eq!(after.deps.len(), before.deps.len(), "deps unchanged");
    }

    // KEEP-UNKNOWN-ID: an adjusted plan keeping a non-existent task id is rejected
    // BEFORE any mutation (we never delete real tasks chasing an unresolvable keep).
    #[tokio::test]
    async fn adjust_keep_unknown_id_is_rejected_run_unchanged() {
        let (svc, run_id) = adjust_harness(
            dag_with_titles(&["A", "B"]),
            AdjustedPlan { tasks: vec![] },
        )
        .await;
        let before = svc.get_detail(&run_id).await.expect("detail");
        assert_eq!(before.tasks.len(), 2);

        let svc = restage_adjust(svc, AdjustedPlan { tasks: vec![keep("rtask_does_not_exist")] });
        let err = svc.adjust("u1", &run_id, "保留幽灵").await.expect_err("unknown keep must reject");
        assert!(matches!(err, AppError::BadRequest(_)), "got: {err:?}");
        let after = svc.get_detail(&run_id).await.expect("detail");
        assert_eq!(after.tasks.len(), 2, "run unchanged when a kept id is unknown");
    }

    // EMPTY-INTENT: a blank intent is a 400 (mutates nothing, never calls the lead).
    #[tokio::test]
    async fn adjust_empty_intent_is_bad_request() {
        let (svc, run_id) = adjust_harness(dag_with_titles(&["A"]), AdjustedPlan { tasks: vec![] }).await;
        let err = svc.adjust("u1", &run_id, "   ").await.expect_err("empty intent must reject");
        assert!(matches!(err, AppError::BadRequest(_)), "got: {err:?}");
    }

    // UC-3a 评审 Important-B: an adjusted plan whose NEW tasks form a mutual cycle
    // (new0 ↔ new1) is REJECTED before any write — the cycle check runs over the
    // resolved graph and the run is left UNCHANGED. (Engine-wise such a cycle would
    // soft-strand the run: neither task could ever become ready.)
    #[tokio::test]
    async fn adjust_rejects_new_new_cycle_run_unchanged() {
        let (svc, run_id) = adjust_harness(
            dag_with_titles(&["A", "B"]),
            AdjustedPlan { tasks: vec![] },
        )
        .await;
        let before = svc.get_detail(&run_id).await.expect("detail");
        assert_eq!(before.tasks.len(), 2);
        let before_ids: Vec<String> = before.tasks.iter().map(|t| t.id.clone()).collect();

        // new[0] depends on new[1]; new[1] depends on new[0] → a 2-cycle.
        let svc = restage_adjust(svc, AdjustedPlan {
            tasks: vec![
                new_node("环A", vec![AdjustedDepRef::NewIndex(1)]),
                new_node("环B", vec![AdjustedDepRef::NewIndex(0)]),
            ],
        });
        let err = svc.adjust("u1", &run_id, "造个环").await.expect_err("cycle must reject");
        assert!(matches!(err, AppError::BadRequest(_)), "got: {err:?}");

        // The run is UNCHANGED: same two original tasks, no new tasks, deps intact.
        let after = svc.get_detail(&run_id).await.expect("detail");
        assert_eq!(after.tasks.len(), 2, "run unchanged on a cyclic plan");
        for id in &before_ids {
            assert!(after.tasks.iter().any(|t| &t.id == id), "task {id} survives");
        }
        assert!(!after.tasks.iter().any(|t| t.title == "环A" || t.title == "环B"), "no new task");
        assert_eq!(after.deps.len(), before.deps.len(), "deps unchanged");
    }

    // UC-3a 评审 Important-B: a new↔kept cycle (kept → new → kept, the authoritative
    // case the full-graph check catches that a "point earlier" constraint alone
    // would miss) is REJECTED before any write; the run is UNCHANGED. Here we keep a
    // done task K and add a new task N that BOTH depends on K (kept ref) AND that K
    // is wired to depend on — but K depending on N can only be expressed via a new
    // node referencing K while another new node K' creates the back-edge. We model
    // the cycle as: new0 depends_on kept K AND new1; new1 depends_on new0 — a cycle
    // among the new tasks that also threads through the kept reference, proving the
    // check spans kept + new nodes.
    #[tokio::test]
    async fn adjust_rejects_new_kept_cycle_run_unchanged() {
        let (svc, run_id) = adjust_harness(
            single_task_dag(Some(0), None),
            AdjustedPlan { tasks: vec![] },
        )
        .await;
        let before = svc.get_detail(&run_id).await.expect("detail");
        let kept_id = before.tasks[0].id.clone();
        mark_done(&svc, &kept_id, "out").await;

        // keep[0] = K; new[1] depends on [K, new#2]; new[2] depends on [new#1].
        // new#1 ↔ new#2 form a cycle (each blocks the other), threaded with a kept
        // ref so the full-graph (kept + new) check is what catches it.
        let svc = restage_adjust(svc, AdjustedPlan {
            tasks: vec![
                keep(&kept_id),
                new_node("环1", vec![AdjustedDepRef::Kept(kept_id.clone()), AdjustedDepRef::NewIndex(2)]),
                new_node("环2", vec![AdjustedDepRef::NewIndex(1)]),
            ],
        });
        let err = svc.adjust("u1", &run_id, "环上加环").await.expect_err("cycle must reject");
        assert!(matches!(err, AppError::BadRequest(_)), "got: {err:?}");

        // The run is UNCHANGED: only the kept task, still done, no new tasks.
        let after = svc.get_detail(&run_id).await.expect("detail");
        assert_eq!(after.tasks.len(), 1, "run unchanged on a cyclic plan");
        assert_eq!(after.tasks[0].id, kept_id, "kept task survives");
        assert_eq!(after.tasks[0].status, "done", "kept task untouched");
        assert!(after.deps.is_empty(), "no deps wired");
    }

    // A VALID acyclic adjusted plan still reconciles fine after the cycle check
    // (regression guard: the check must not reject legitimate DAGs). kept → new[1]
    // → new[2] is a clean chain.
    #[tokio::test]
    async fn adjust_accepts_acyclic_plan_after_cycle_check() {
        let (svc, run_id) = adjust_harness(
            single_task_dag(Some(0), None),
            AdjustedPlan { tasks: vec![] },
        )
        .await;
        let before = svc.get_detail(&run_id).await.expect("detail");
        let kept_id = before.tasks[0].id.clone();
        mark_done(&svc, &kept_id, "out").await;

        let svc = restage_adjust(svc, AdjustedPlan {
            tasks: vec![
                keep(&kept_id),
                new_node("中", vec![AdjustedDepRef::Kept(kept_id.clone())]),
                new_node("末", vec![AdjustedDepRef::NewIndex(1)]),
            ],
        });
        svc.adjust("u1", &run_id, "正常扩展").await.expect("acyclic plan reconciles");

        let after = svc.get_detail(&run_id).await.expect("detail");
        assert_eq!(after.tasks.len(), 3, "kept + 2 new");
        let mid = after.tasks.iter().find(|t| t.title == "中").expect("中").id.clone();
        let last = after.tasks.iter().find(|t| t.title == "末").expect("末").id.clone();
        let has = |b: &str, k: &str| after.deps.iter().any(|d| d.blocker_task_id == b && d.blocked_task_id == k);
        assert!(has(&kept_id, &mid), "kept→中");
        assert!(has(&mid, &last), "中→末");
        assert_eq!(after.deps.len(), 2, "exactly the two acyclic edges: {:?}", after.deps);
    }

    // UC-3a 评审 Important-A (atomic commit): a successful adjust commits the WHOLE
    // reconcile through the single transactional repo path — the dropped task is
    // gone, the kept task + its output survive, the new task is inserted + routed,
    // and the rebuilt edge is present, all in one consistent post-state. (The
    // rollback half — a mid-tx error leaves the run unchanged — is asserted at the
    // repo layer in `reconcile_run_plan_rolls_back_on_mid_transaction_error`.)
    #[tokio::test]
    async fn adjust_commits_whole_reconcile_atomically() {
        let (svc, run_id) = adjust_harness(
            dag_with_titles(&["留", "弃"]),
            AdjustedPlan { tasks: vec![] },
        )
        .await;
        let before = svc.get_detail(&run_id).await.expect("detail");
        let keep_id = before.tasks.iter().find(|t| t.title == "留").unwrap().id.clone();
        let drop_id = before.tasks.iter().find(|t| t.title == "弃").unwrap().id.clone();
        mark_done(&svc, &keep_id, "保留产出").await;

        // Keep 留 (done), drop 弃, add a new task depending on the kept one.
        let svc = restage_adjust(svc, AdjustedPlan {
            tasks: vec![keep(&keep_id), new_node("新", vec![AdjustedDepRef::Kept(keep_id.clone())])],
        });
        svc.adjust("u1", &run_id, "保留留删掉弃加新").await.expect("adjust commits");

        let after = svc.get_detail(&run_id).await.expect("detail");
        // Whole reconcile committed consistently: kept survives done, drop gone,
        // new inserted+routed, edge wired.
        assert_eq!(after.tasks.len(), 2, "kept + new only");
        let kept = after.tasks.iter().find(|t| t.id == keep_id).expect("kept survives");
        assert_eq!(kept.status, "done", "kept not re-run");
        assert_eq!(kept.output_summary.as_deref(), Some("保留产出"), "output preserved");
        assert!(!after.tasks.iter().any(|t| t.id == drop_id), "dropped gone");
        let new_task = after.tasks.iter().find(|t| t.title == "新").expect("new added");
        assert_eq!(new_task.status, "pending");
        assert!(
            after.assignments.iter().any(|a| a.task_id == new_task.id && a.source == "auto"),
            "new task routed atomically: {:?}",
            after.assignments
        );
        assert!(
            after.assignments.iter().any(|a| a.task_id == keep_id),
            "kept assignment preserved"
        );
        assert!(
            after.deps.iter().any(|d| d.blocker_task_id == keep_id && d.blocked_task_id == new_task.id),
            "edge kept→new wired: {:?}",
            after.deps
        );
        assert_eq!(after.deps.len(), 1, "exactly the rebuilt edge");
    }

    // Pure unit coverage of reconcile_plan_has_cycle: a self-edge (new#0 →
    // new#0), a 2-cycle, and a new→kept→new path are cyclic; a clean chain and a
    // lone new task with no deps are acyclic.
    #[test]
    fn reconcile_plan_has_cycle_unit() {
        use nomifun_db::ReconcileDepRef as DR;
        let kept: std::collections::HashSet<String> =
            ["K".to_string()].into_iter().collect();
        let mk = |deps: Vec<DR>| ReconcileNewTask {
            task: CreateTaskParams {
                run_id: "r".into(),
                title: "t".into(),
                spec: "s".into(),
                task_profile: None,
                status: "pending".into(),
                graph_x: None,
                graph_y: None,
                role: None,
                kind: "agent".into(),
                pattern_config: None,
            },
            assignment: None,
            depends_on: deps,
        };
        let empty: std::collections::HashSet<String> = std::collections::HashSet::new();

        // Self-edge → cycle.
        assert!(reconcile_plan_has_cycle(&empty, &[mk(vec![DR::NewIndex(0)])]));
        // 2-cycle new0 ↔ new1 → cycle.
        assert!(reconcile_plan_has_cycle(
            &empty,
            &[mk(vec![DR::NewIndex(1)]), mk(vec![DR::NewIndex(0)])]
        ));
        // Clean chain new0 → new1 (new1 depends on new0) → acyclic.
        assert!(!reconcile_plan_has_cycle(&empty, &[mk(vec![]), mk(vec![DR::NewIndex(0)])]));
        // Lone new task, no deps → acyclic.
        assert!(!reconcile_plan_has_cycle(&empty, &[mk(vec![])]));
        // A new task depending on a kept node → acyclic (kept has no out-edges).
        assert!(!reconcile_plan_has_cycle(&kept, &[mk(vec![DR::Kept("K".into())])]));
    }

    // C item 6: pure unit coverage of planned_dag_has_cycle (the initial-plan guard,
    // symmetric with reconcile_plan_has_cycle but over the planner's index graph).
    #[test]
    fn planned_dag_has_cycle_unit() {
        let mk = |deps: Vec<usize>| PlannedTask {
            title: "t".into(),
            spec: "s".into(),
            task_profile: None,
            depends_on: deps,
            member_index: Some(0),
            rationale: None,
            role: None,
            kind: "agent".into(),
            pattern_config: None,
        };
        let dag = |tasks: Vec<PlannedTask>| PlannedDag { tasks };

        // Empty / lone / clean chain → acyclic.
        assert!(!planned_dag_has_cycle(&dag(vec![])));
        assert!(!planned_dag_has_cycle(&dag(vec![mk(vec![])])));
        // 0 → 1 → 2 clean chain (each depends on the prior) → acyclic.
        assert!(!planned_dag_has_cycle(&dag(vec![mk(vec![]), mk(vec![0]), mk(vec![1])])));
        // Self-edge (task 0 depends on itself) → cycle.
        assert!(planned_dag_has_cycle(&dag(vec![mk(vec![0])])));
        // 2-cycle: task 0 depends on [1], task 1 depends on [0] → cycle.
        assert!(planned_dag_has_cycle(&dag(vec![mk(vec![1]), mk(vec![0])])));
        // Back-edge in a 3-task chain: 0→[2] creates 2→0, plus 1→0, 2→1 → cycle.
        assert!(planned_dag_has_cycle(&dag(vec![mk(vec![2]), mk(vec![0]), mk(vec![1])])));
        // Out-of-range dep contributes no edge → acyclic (range-validated upstream).
        assert!(!planned_dag_has_cycle(&dag(vec![mk(vec![9])])));
    }

    // C item 6: a CYCLIC initial planner output degrades to the degenerate
    // single-task plan (the whole goal as one agent task) — so the run still
    // proceeds and is NOT persisted as a back-edged cycle. (We don't have a prior
    // plan to fall back to on the initial path, unlike `adjust` which rejects.)
    #[tokio::test]
    async fn plan_cyclic_dag_falls_back_to_single_task() {
        let members = vec![member_input("agent_a", &["coding"], "high", "standard")];
        // Two tasks forming a 2-cycle: task 0 depends on [1], task 1 depends on [0].
        let mk = |title: &str, deps: Vec<usize>| PlannedTask {
            title: title.into(),
            spec: format!("spec-{title}"),
            task_profile: None,
            depends_on: deps,
            member_index: Some(0),
            rationale: None,
            role: None,
            kind: "agent".into(),
            pattern_config: None,
        };
        let dag = PlannedDag { tasks: vec![mk("环A", vec![1]), mk("环B", vec![0])] };
        let (svc, _repo, _snapshot, run_id) = harness(members, dag).await;

        svc.plan(&run_id).await.expect("plan must succeed via fallback");

        let detail = svc.get_detail(&run_id).await.expect("detail");
        // Degraded to exactly one task (the degenerate plan), with NO deps (so no
        // cycle was persisted), and the original cyclic titles are gone.
        assert_eq!(detail.tasks.len(), 1, "cyclic plan degrades to one task");
        assert!(detail.deps.is_empty(), "no dep edges persisted (no cycle)");
        assert!(
            !detail.tasks.iter().any(|t| t.title == "环A" || t.title == "环B"),
            "cyclic plan's tasks must not be persisted: {:?}",
            detail.tasks.iter().map(|t| &t.title).collect::<Vec<_>>()
        );
        // The single fallback task is routed + the run flips to running (proceeds).
        assert_eq!(detail.assignments.len(), 1, "fallback task is assigned");
        assert_eq!(detail.run.status, "running", "run proceeds after fallback");
    }

    // C item 6 (regression guard): a normal ACYCLIC plan is UNCHANGED by the new
    // cycle guard — both tasks persist and the dep edge is wired exactly as before.
    #[tokio::test]
    async fn plan_acyclic_dag_is_unchanged_by_cycle_guard() {
        let members = vec![member_input("agent_a", &["coding"], "high", "standard")];
        let mk = |title: &str, deps: Vec<usize>| PlannedTask {
            title: title.into(),
            spec: format!("spec-{title}"),
            task_profile: None,
            depends_on: deps,
            member_index: Some(0),
            rationale: None,
            role: None,
            kind: "agent".into(),
            pattern_config: None,
        };
        // 上游 → 下游 (task 1 depends on task 0): a clean two-node DAG.
        let dag = PlannedDag { tasks: vec![mk("上游", vec![]), mk("下游", vec![0])] };
        let (svc, _repo, _snapshot, run_id) = harness(members, dag).await;

        svc.plan(&run_id).await.expect("plan");

        let detail = svc.get_detail(&run_id).await.expect("detail");
        assert_eq!(detail.tasks.len(), 2, "acyclic plan persists both tasks");
        let up = detail.tasks.iter().find(|t| t.title == "上游").expect("上游");
        let down = detail.tasks.iter().find(|t| t.title == "下游").expect("下游");
        assert_eq!(
            detail.deps.len(),
            1,
            "exactly the one planned edge is wired: {:?}",
            detail.deps
        );
        assert!(
            detail.deps.iter().any(|d| d.blocker_task_id == up.id && d.blocked_task_id == down.id),
            "上游→下游 edge wired"
        );
    }


    /// planner re-staged to return `adjusted` from `adjust` (so a test can stage a
    /// plan that references the REAL task ids minted by the first `plan`). The
    /// initial dag is irrelevant here (we never re-`plan`).
    fn restage_adjust(svc: RunService, adjusted: AdjustedPlan) -> RunService {
        let planner: Arc<dyn PlanProducer> = Arc::new(AdjustTestProducer::new(
            PlannedDag { tasks: vec![] },
            adjusted,
        ));
        RunService {
            run_repo: svc.run_repo.clone(),
            fleet_repo: svc.fleet_repo.clone(),
            ws_repo: svc.ws_repo.clone(),
            planner,
            emitter: svc.emitter.clone(),
        }
    }
}

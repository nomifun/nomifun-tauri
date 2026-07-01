//! Orchestration (智能编排) HTTP routes. Handlers do request/response
//! transformation only; all logic lives in [`FleetService`] / [`WorkspaceService`].
//! Auth is layered externally in nomifun-app (mirrors the webhook / requirement
//! / idmm routes), so it is safe to extract [`CurrentUser`] here — these routes
//! mount UNDER the auth middleware, not as public routes.
//!
//! IDs are application strings (`fleet_…` / `ows_…`), so the `{id}` path segment
//! is passed straight to the service without parsing.

use axum::Router;
use axum::extract::rejection::JsonRejection;
use axum::extract::{Extension, Json, Path, Query, State};
use axum::http::StatusCode;
use axum::routing::{get, patch, post, put};

use nomifun_api_types::{
    ApiResponse, AdjustRunRequest, CreateAdhocRunRequest, CreateFleetRequest, CreateRunRequest,
    CreateWorkspaceRequest, Fleet, OrchWorkspace, ReassignRequest, ReplanRequest, Run, RunDetail,
    RunRenameRequest, SteerRequest, TaskConfigUpdateRequest, TaskSpecUpdateRequest, UpdateFleetRequest, UpdateWorkspaceRequest,
    WorkspaceEntry,
};
use nomifun_auth::CurrentUser;
use nomifun_common::AppError;
use serde::Deserialize;

use crate::state::OrchestratorRouterState;

/// Query for `GET /api/orchestrator/runs/{id}/workspace`. `path` is
/// workspace-relative (default = the run's working-directory root) + optional
/// case-insensitive `search`. The root itself is resolved server-side from the
/// run (`work_dir`, else the bound workspace's dir) and is never accepted here.
#[derive(Debug, Deserialize)]
pub struct RunWorkspaceQuery {
    #[serde(default)]
    pub path: String,
    pub search: Option<String>,
}

pub fn orchestrator_routes(state: OrchestratorRouterState) -> Router {
    Router::new()
        .route(
            "/api/orchestrator/fleets",
            get(list_fleets).post(create_fleet),
        )
        .route(
            "/api/orchestrator/fleets/{id}",
            get(get_fleet).put(update_fleet).delete(delete_fleet),
        )
        .route(
            "/api/orchestrator/workspaces",
            get(list_workspaces).post(create_workspace),
        )
        .route(
            "/api/orchestrator/workspaces/{id}",
            get(get_workspace).put(update_workspace).delete(delete_workspace),
        )
        .route(
            "/api/orchestrator/runs",
            get(list_my_runs).post(create_run),
        )
        .route(
            "/api/orchestrator/runs/adhoc",
            post(create_adhoc_run),
        )
        .route(
            "/api/orchestrator/workspaces/{ws}/runs",
            get(list_workspace_runs),
        )
        .route(
            "/api/orchestrator/runs/{id}",
            get(get_run).delete(delete_run).patch(rename_run),
        )
        .route("/api/orchestrator/runs/{id}/cancel", post(cancel_run))
        .route("/api/orchestrator/runs/{id}/replan", post(replan_run))
        .route("/api/orchestrator/runs/{id}/adjust", post(adjust_run))
        .route("/api/orchestrator/runs/{id}/approve", post(approve_run))
        .route("/api/orchestrator/runs/{id}/pause", post(pause_run))
        .route("/api/orchestrator/runs/{id}/resume", post(resume_run))
        .route(
            "/api/orchestrator/runs/{id}/workspace",
            get(browse_run_workspace),
        )
        .route(
            "/api/orchestrator/runs/{run_id}/tasks/{task_id}/steer",
            post(steer_task),
        )
        .route(
            "/api/orchestrator/runs/{run_id}/tasks/{task_id}/assignment",
            put(reassign_task),
        )
        .route(
            "/api/orchestrator/runs/{run_id}/tasks/{task_id}/rerun",
            post(rerun_task),
        )
        .route(
            "/api/orchestrator/runs/{run_id}/tasks/{task_id}/adopt",
            post(adopt_task_result),
        )
        .route(
            "/api/orchestrator/runs/{run_id}/tasks/{task_id}/spec",
            patch(update_task_spec),
        )
        .route(
            "/api/orchestrator/runs/{run_id}/tasks/{task_id}/config",
            patch(set_task_config),
        )
        .with_state(state)
}

// ── Fleets ──────────────────────────────────────────────────────────────────

async fn list_fleets(
    State(state): State<OrchestratorRouterState>,
    Extension(user): Extension<CurrentUser>,
) -> Result<Json<ApiResponse<Vec<Fleet>>>, AppError> {
    Ok(Json(ApiResponse::ok(state.fleet.list(&user.id).await?)))
}

async fn get_fleet(
    State(state): State<OrchestratorRouterState>,
    Extension(_user): Extension<CurrentUser>,
    Path(id): Path<String>,
) -> Result<Json<ApiResponse<Fleet>>, AppError> {
    Ok(Json(ApiResponse::ok(state.fleet.get(&id).await?)))
}

async fn create_fleet(
    State(state): State<OrchestratorRouterState>,
    Extension(user): Extension<CurrentUser>,
    body: Result<Json<CreateFleetRequest>, JsonRejection>,
) -> Result<(StatusCode, Json<ApiResponse<Fleet>>), AppError> {
    let Json(req) = body.map_err(|e| AppError::BadRequest(e.to_string()))?;
    let created = state.fleet.create(&user.id, req).await?;
    Ok((StatusCode::CREATED, Json(ApiResponse::ok(created))))
}

async fn update_fleet(
    State(state): State<OrchestratorRouterState>,
    Extension(_user): Extension<CurrentUser>,
    Path(id): Path<String>,
    body: Result<Json<UpdateFleetRequest>, JsonRejection>,
) -> Result<Json<ApiResponse<Fleet>>, AppError> {
    let Json(req) = body.map_err(|e| AppError::BadRequest(e.to_string()))?;
    Ok(Json(ApiResponse::ok(state.fleet.update(&id, req).await?)))
}

async fn delete_fleet(
    State(state): State<OrchestratorRouterState>,
    Extension(_user): Extension<CurrentUser>,
    Path(id): Path<String>,
) -> Result<Json<ApiResponse<()>>, AppError> {
    state.fleet.delete(&id).await?;
    Ok(Json(ApiResponse::success()))
}

// ── Workspaces ───────────────────────────────────────────────────────────────

async fn list_workspaces(
    State(state): State<OrchestratorRouterState>,
    Extension(user): Extension<CurrentUser>,
) -> Result<Json<ApiResponse<Vec<OrchWorkspace>>>, AppError> {
    Ok(Json(ApiResponse::ok(state.workspace.list(&user.id).await?)))
}

async fn get_workspace(
    State(state): State<OrchestratorRouterState>,
    Extension(_user): Extension<CurrentUser>,
    Path(id): Path<String>,
) -> Result<Json<ApiResponse<OrchWorkspace>>, AppError> {
    Ok(Json(ApiResponse::ok(state.workspace.get(&id).await?)))
}

async fn create_workspace(
    State(state): State<OrchestratorRouterState>,
    Extension(user): Extension<CurrentUser>,
    body: Result<Json<CreateWorkspaceRequest>, JsonRejection>,
) -> Result<(StatusCode, Json<ApiResponse<OrchWorkspace>>), AppError> {
    let Json(req) = body.map_err(|e| AppError::BadRequest(e.to_string()))?;
    let created = state.workspace.create(&user.id, req).await?;
    Ok((StatusCode::CREATED, Json(ApiResponse::ok(created))))
}

async fn update_workspace(
    State(state): State<OrchestratorRouterState>,
    Extension(_user): Extension<CurrentUser>,
    Path(id): Path<String>,
    body: Result<Json<UpdateWorkspaceRequest>, JsonRejection>,
) -> Result<Json<ApiResponse<OrchWorkspace>>, AppError> {
    let Json(req) = body.map_err(|e| AppError::BadRequest(e.to_string()))?;
    Ok(Json(ApiResponse::ok(state.workspace.update(&id, req).await?)))
}

async fn delete_workspace(
    State(state): State<OrchestratorRouterState>,
    Extension(_user): Extension<CurrentUser>,
    Path(id): Path<String>,
) -> Result<Json<ApiResponse<()>>, AppError> {
    state.workspace.delete(&id).await?;
    Ok(Json(ApiResponse::success()))
}

// ── Runs ─────────────────────────────────────────────────────────────────────

/// Create a run, then plan it, then (unless interactive) hand it to the engine.
/// The steps are deliberately separate (Task 6 contract): `create` parks the run
/// in `planning`, `plan` decomposes the goal + applies the **autonomy gate**
/// (`interactive` → `awaiting_plan_approval`; else → `running`), and
/// `engine.start` spawns the (synchronous, fire-and-forget) execution loop.
///
/// **Autonomy gate (P3b):** an `interactive` run must NOT start until a human
/// approves the plan (`POST .../approve`), so we skip `engine.start` here for it.
/// All other levels start immediately. If planning fails, the error is surfaced —
/// the run already exists in `planning` and can be re-planned later.
async fn create_run(
    State(state): State<OrchestratorRouterState>,
    Extension(user): Extension<CurrentUser>,
    body: Result<Json<CreateRunRequest>, JsonRejection>,
) -> Result<(StatusCode, Json<ApiResponse<Run>>), AppError> {
    let Json(req) = body.map_err(|e| AppError::BadRequest(e.to_string()))?;
    let run = state.run_service.create(&user.id, req).await?;
    state.run_service.plan(&run.id).await?;
    // `interactive` parks at `awaiting_plan_approval` — do NOT start the engine
    // until the plan is approved. All other autonomy levels start immediately.
    // `start` is synchronous (spawns the loop internally) — do not await.
    if run.autonomy != "interactive" {
        state.engine.start(run.id.clone());
    }
    Ok((StatusCode::CREATED, Json(ApiResponse::ok(run))))
}

/// Create an **ad-hoc** run straight from a structured Tab form (no workspace, no
/// pre-built fleet — the fleet is synthesized from the request's `model_range`),
/// then plan it and apply the same autonomy gate as [`create_run`].
///
/// **Optimistic create (B3):** `create_adhoc` persists the run in `planning` and we
/// return it IMMEDIATELY; planning (`plan`) + the conditional `engine.start` run in
/// a detached background task ([`spawn_plan_and_start`]). This closes the long
/// "submit → blank wait" gap — the FE jumps straight to the planning-state run and
/// watches the lead's planning thought stream in over `leadThinking`, instead of
/// blocking on the ~4096-token structured completion before any response. The
/// returned run is `planning` with 0 tasks (the FE has a planning empty-state); for
/// an `interactive` run the background plan parks it at `awaiting_plan_approval`.
///
/// **Default autonomy is `interactive`** (the Tab's approval门): unlike the
/// workspace path's `create`, an ad-hoc run launched from the Tab should park at
/// `awaiting_plan_approval` for a human to confirm the plan before any worker
/// dispatches. The default is injected onto the REQUEST here — BEFORE
/// `create_adhoc` persists it — so the background `plan` (which re-reads the
/// persisted autonomy) applies the gate, and the background `engine.start` decision
/// reads the same value. (The MCP/caps front door, by contrast, defaults to
/// `supervised` — it has no Tab to approve through. See `caps_orchestrator`.)
async fn create_adhoc_run(
    State(state): State<OrchestratorRouterState>,
    Extension(user): Extension<CurrentUser>,
    body: Result<Json<CreateAdhocRunRequest>, JsonRejection>,
) -> Result<(StatusCode, Json<ApiResponse<Run>>), AppError> {
    let Json(mut req) = body.map_err(|e| AppError::BadRequest(e.to_string()))?;
    // Tab default: park at the plan-approval gate unless the form picks another
    // level. Applied to the request so the value is what `create_adhoc` PERSISTS
    // and the background `plan` re-reads for the autonomy gate (an empty string is
    // treated as absent — same rule as RunService).
    if req.autonomy.as_deref().map(str::trim).unwrap_or("").is_empty() {
        req.autonomy = Some("interactive".to_string());
    }
    let run = state.run_service.create_adhoc(&user.id, req).await?;
    // Path B (B3): associate the originating conversation as this run's lead.
    // `create_adhoc` already PERSISTED `lead_conv_id` onto the run row; here we also
    // write the inverse pointer onto the conversation (`extra.orchestrator_run_id`)
    // + broadcast `conversation.listChanged` so the FE lights up that conversation's
    // orchestration canvas entry. A lightweight await BEFORE the optimistic return —
    // it must NOT move into `spawn_plan_and_start` (that would change the immediate
    // return timing). Best-effort: the run is already created, so a link failure only
    // `warn!`s — it never fails creation and never panics. `lead_conv_id: None`
    // (legacy callers / no originating conversation) skips the call entirely.
    if let Some(lead_conv_id) = run.lead_conv_id {
        if let Err(e) = state
            .conversation_service
            .link_orchestrator_run(&lead_conv_id.to_string(), &run.id)
            .await
        {
            tracing::warn!(
                error = %e,
                conversation_id = lead_conv_id,
                run_id = %run.id,
                "failed to link orchestration run to originating conversation; run still created"
            );
        }
    }
    // Return the `planning`-state run NOW; plan + (conditional) start run in the
    // background so the FE jumps to the run and sees the planning thought stream
    // immediately rather than waiting out the lead completion (the空挡 fix).
    crate::run_service::spawn_plan_and_start(
        state.run_service.clone(),
        state.engine.clone(),
        run.id.clone(),
        run.autonomy.clone(),
    );
    Ok((StatusCode::CREATED, Json(ApiResponse::ok(run))))
}
/// workspaces AND ad-hoc (workspace-less) runs. This is the read path for the
/// read-only Run-history library (the repurposed orchestrator tab); ad-hoc runs
/// created from a conversation carry no workspace, so they only surface here,
/// never under the workspace-scoped `list_workspace_runs`. PROTECTED route — it
/// extracts `CurrentUser`, so it mounts under the same auth middleware as the
/// other run routes (NOT a public route).
async fn list_my_runs(
    State(state): State<OrchestratorRouterState>,
    Extension(user): Extension<CurrentUser>,
) -> Result<Json<ApiResponse<Vec<Run>>>, AppError> {
    Ok(Json(ApiResponse::ok(
        state.run_service.list_by_user(&user.id).await?,
    )))
}

async fn list_workspace_runs(
    State(state): State<OrchestratorRouterState>,
    Extension(_user): Extension<CurrentUser>,
    Path(ws): Path<String>,
) -> Result<Json<ApiResponse<Vec<Run>>>, AppError> {
    Ok(Json(ApiResponse::ok(state.run_service.list(&ws).await?)))
}

async fn get_run(
    State(state): State<OrchestratorRouterState>,
    Extension(_user): Extension<CurrentUser>,
    Path(id): Path<String>,
) -> Result<Json<ApiResponse<RunDetail>>, AppError> {
    Ok(Json(ApiResponse::ok(state.run_service.get_detail(&id).await?)))
}

/// Cancel a run: stop the engine loop (cooperative cancel + abort) then persist
/// the `cancelled` status. `stop` is synchronous; `cancel` persists + emits.
async fn cancel_run(
    State(state): State<OrchestratorRouterState>,
    Extension(_user): Extension<CurrentUser>,
    Path(id): Path<String>,
) -> Result<Json<ApiResponse<()>>, AppError> {
    state.engine.stop(&id);
    state.run_service.cancel(&id).await?;
    Ok(Json(ApiResponse::success()))
}

/// Delete a run (owner-scoped). Stop the engine loop FIRST (mirrors `cancel_run`'s
/// `engine.stop` → service ordering) so a live loop is cooperatively cancelled
/// before the row + its tasks/deps/assignments cascade out from under it, then
/// delete. The service enforces ownership (404 missing / 403 not-owned) and the
/// schema's `ON DELETE CASCADE` FKs sweep out the whole aggregate.
async fn delete_run(
    State(state): State<OrchestratorRouterState>,
    Extension(user): Extension<CurrentUser>,
    Path(id): Path<String>,
) -> Result<Json<ApiResponse<()>>, AppError> {
    state.engine.stop(&id);
    state.run_service.delete(&user.id, &id).await?;
    Ok(Json(ApiResponse::success()))
}

/// Rename a run = change its goal (owner-scoped). Body is a [`RunRenameRequest`];
/// the service enforces ownership (404/403) and rejects a blank goal (400).
async fn rename_run(
    State(state): State<OrchestratorRouterState>,
    Extension(user): Extension<CurrentUser>,
    Path(id): Path<String>,
    body: Result<Json<RunRenameRequest>, JsonRejection>,
) -> Result<Json<ApiResponse<()>>, AppError> {
    let Json(req) = body.map_err(|e| AppError::BadRequest(e.to_string()))?;
    state.run_service.rename(&user.id, &id, &req.goal).await?;
    Ok(Json(ApiResponse::success()))
}

/// List one directory level under a run's working directory (owner-scoped). The
/// root is server-authoritative (the run's `work_dir`, else its workspace's dir);
/// the client supplies only a workspace-relative `path` + optional `search`.
/// Missing/not-owned run → 404/403, a run with no working dir → 400, `..`
/// traversal → 400 (from the service / `list_workspace_level`). Read-only — the
/// run-history counterpart of the conversation / terminal workspace-browse routes.
async fn browse_run_workspace(
    State(state): State<OrchestratorRouterState>,
    Extension(user): Extension<CurrentUser>,
    Path(id): Path<String>,
    Query(query): Query<RunWorkspaceQuery>,
) -> Result<Json<ApiResponse<Vec<WorkspaceEntry>>>, AppError> {
    let entries = state
        .run_service
        .browse_workspace(&user.id, &id, &query.path, query.search.as_deref())
        .await?;
    Ok(Json(ApiResponse::ok(entries)))
}

/// Re-plan a run in place (owner-scoped). Stop the engine loop FIRST (the old
/// plan is about to be cleared out from under any live worker — mirrors cancel /
/// delete's `engine.stop` → service ordering), then re-decompose via the service.
/// Body is a [`ReplanRequest`] (all fields optional). The service enforces
/// ownership (404/403) and rejects a blank goal / unexpanded `auto` range (400).
/// On success the route reads the re-planned run's autonomy and (re)starts the
/// engine for non-`interactive` runs — exactly like `create_run` / `create_adhoc`
/// (an `interactive` run parks at `awaiting_plan_approval` and waits for approve).
async fn replan_run(
    State(state): State<OrchestratorRouterState>,
    Extension(user): Extension<CurrentUser>,
    Path(id): Path<String>,
    body: Result<Json<ReplanRequest>, JsonRejection>,
) -> Result<Json<ApiResponse<Run>>, AppError> {
    let Json(req) = body.map_err(|e| AppError::BadRequest(e.to_string()))?;
    state.engine.stop(&id);
    let run = state.run_service.replan(&user.id, &id, req).await?;
    if run.autonomy != "interactive" {
        state.engine.start(run.id.clone());
    }
    Ok(Json(ApiResponse::ok(run)))
}

/// Conversation-driven intelligent re-adjust (UC-3a, owner-scoped). Goes through
/// [`RunEngine::adjust`](crate::engine::RunEngine::adjust) so the service's WHOLE
/// reconcile (clear deps + drop unkept + insert/route new + rebuild deps) and the
/// terminal-run re-activation run UNDER the engine's per-run lock (the SAME lock
/// the run-loop's terminal-check-and-finish holds) — closing the re-activation
/// race (a lock-free reconcile could be overwritten by a loop concurrently writing
/// `completed`/`failed`, stranding the run with freshly-added pending tasks). The
/// lock is released before the engine-lifecycle decision below: only `engine.start`
/// when the loop is NOT already running (mirrors `rerun_run`/`resume_run`) — an
/// unconditional start would `stop()` first, cancelling in-flight work of an
/// already-running run. A re-activated terminal run had its loop deregister under
/// the lock as it finished, so `!is_running` is authoritative and the fresh loop is
/// (re)spawned; an alive loop self-re-picks the new pending tasks. The service
/// rejects a blank intent / a run with any `running` task (400, mutating nothing).
async fn adjust_run(
    State(state): State<OrchestratorRouterState>,
    Extension(user): Extension<CurrentUser>,
    Path(id): Path<String>,
    body: Result<Json<AdjustRunRequest>, JsonRejection>,
) -> Result<Json<ApiResponse<Run>>, AppError> {
    let Json(req) = body.map_err(|e| AppError::BadRequest(e.to_string()))?;
    let run = state
        .engine
        .adjust(&state.run_service, &user.id, &id, &req.intent)
        .await?;
    if run.status == "running" && !state.engine.is_running(&id) {
        state.engine.start(id);
    }
    Ok(Json(ApiResponse::ok(run)))
}

/// Approve an `interactive` run's plan: `awaiting_plan_approval` → `running`,
/// then start the engine. Mirrors `create_run`'s start step — the service mutates
/// status + emits, the route owns the engine lifecycle.
async fn approve_run(
    State(state): State<OrchestratorRouterState>,
    Extension(_user): Extension<CurrentUser>,
    Path(id): Path<String>,
) -> Result<Json<ApiResponse<()>>, AppError> {
    state.run_service.approve_plan(&id).await?;
    state.engine.start(id);
    Ok(Json(ApiResponse::success()))
}

/// Pause a `running` run: `running` → `paused`. The engine's persistent loop
/// observes the paused status and stops dispatching new workers (in-flight
/// workers run to completion). No engine call needed — the loop self-gates.
async fn pause_run(
    State(state): State<OrchestratorRouterState>,
    Extension(_user): Extension<CurrentUser>,
    Path(id): Path<String>,
) -> Result<Json<ApiResponse<()>>, AppError> {
    state.run_service.pause(&id).await?;
    Ok(Json(ApiResponse::success()))
}

/// Resume a `paused` run: `paused` → `running`. A paused run loop stays ALIVE
/// (it idles on the paused gate, re-reading the run status each tick), so once
/// the service flips the status back to `running` the loop self-resumes filling
/// on its next iteration — no engine restart is needed. We therefore only
/// `engine.start` when the loop is NOT already running (i.e. it actually exited,
/// e.g. after a process restart / boot before `resume_persisted_runs` re-armed
/// it). **Critically, an unconditional `engine.start` would `stop()` first,
/// cancelling every in-flight worker conversation — destroying the live work
/// that pause was meant to let finish (at cap=1 that is the standard pause
/// state). The `!is_running` gate avoids that destructive restart.**
async fn resume_run(
    State(state): State<OrchestratorRouterState>,
    Extension(_user): Extension<CurrentUser>,
    Path(id): Path<String>,
) -> Result<Json<ApiResponse<()>>, AppError> {
    state.run_service.resume(&id).await?;
    // Alive paused loop self-resumes; only (re)start if it actually exited.
    if !state.engine.is_running(&id) {
        state.engine.start(id);
    }
    Ok(Json(ApiResponse::success()))
}

/// Steer (mid-turn inject) a message into a running task's worker conversation.
/// The engine validates the run/task + a stamped `conversation_id` and delegates
/// to `ConversationService::steer_message`. Does NOT change run status.
async fn steer_task(
    State(state): State<OrchestratorRouterState>,
    Extension(_user): Extension<CurrentUser>,
    Path((run_id, task_id)): Path<(String, String)>,
    body: Result<Json<SteerRequest>, JsonRejection>,
) -> Result<Json<ApiResponse<()>>, AppError> {
    let Json(req) = body.map_err(|e| AppError::BadRequest(e.to_string()))?;
    state.engine.steer_task(&run_id, &task_id, &req.text).await?;
    Ok(Json(ApiResponse::success()))
}

/// Override (or lock) the member assigned to a task. The reassign path: upserts
/// the task's assignment to the requested member with `source = "override"`,
/// `locked = req.locked.unwrap_or(true)`. The service validates the run/task
/// exist and the member is in the run's fleet snapshot.
async fn reassign_task(
    State(state): State<OrchestratorRouterState>,
    Extension(_user): Extension<CurrentUser>,
    Path((run_id, task_id)): Path<(String, String)>,
    body: Result<Json<ReassignRequest>, JsonRejection>,
) -> Result<Json<ApiResponse<()>>, AppError> {
    let Json(req) = body.map_err(|e| AppError::BadRequest(e.to_string()))?;
    state.run_service.reassign(&run_id, &task_id, req).await?;
    Ok(Json(ApiResponse::success()))
}

/// Re-execute a single node (UC-2a, owner-scoped). Goes through
/// [`RunEngine::rerun_task`](crate::engine::RunEngine::rerun_task) so the service's
/// RESET + cascade + RE-ACTIVATION runs UNDER the engine's per-run lock (the SAME
/// lock the run-loop's terminal-check-and-finish holds) — this closes the 评审
/// Critical re-activation race (a lock-free reset could be overwritten by a loop
/// that concurrently writes `completed`/`failed`, stranding the run). The lock is
/// released before the engine-lifecycle decision below: only `engine.start` when
/// the loop is NOT already running (mirrors `resume_run`) — an unconditional start
/// would `stop()` first, cancelling any in-flight worker conversations of an
/// already-running run. A re-activated terminal run had its loop deregister its
/// handle under the lock as it finished, so `!is_running` here is authoritative and
/// the fresh loop is (re)spawned; an alive loop self-re-picks the reset pending
/// tasks on its next sweep. The service rejects re-running a `running` task (400).
async fn rerun_task(
    State(state): State<OrchestratorRouterState>,
    Extension(user): Extension<CurrentUser>,
    Path((run_id, task_id)): Path<(String, String)>,
) -> Result<Json<ApiResponse<()>>, AppError> {
    let run = state
        .engine
        .rerun_task(&state.run_service, &user.id, &run_id, &task_id)
        .await?;
    // Re-activated terminal run's loop has exited (it deregistered under the lock)
    // → (re)spawn it; an alive loop re-picks the reset tasks itself, so do not
    // stop+restart it (would clobber any in-flight worker of an already-running
    // run). The `run.status == "running"` guard avoids starting a loop for a run
    // the reset left non-`running` (e.g. a no-op rerun edge).
    if run.status == "running" && !state.engine.is_running(&run_id) {
        state.engine.start(run_id);
    }
    Ok(Json(ApiResponse::success()))
}

/// "采用为该节点产出" (adopt task result, UC-2c, owner-scoped). Pulls the node's
/// worker conversation's CURRENT final output into the orchestration node, marks it
/// `done`, and re-activates a terminal run. Goes through
/// [`RunEngine::adopt_task_result`](crate::engine::RunEngine::adopt_task_result) so
/// the write + re-activation run under the per-run lock; the route then (re)starts
/// the loop only when a re-activated run is `running` and no loop is already alive
/// (mirrors `rerun_task` — an unconditional start would `stop()` an already-running
/// run's in-flight work). The service rejects a `running` RUN (the loop settles the
/// node itself) and a node with no worker conversation / no output yet (400).
async fn adopt_task_result(
    State(state): State<OrchestratorRouterState>,
    Extension(user): Extension<CurrentUser>,
    Path((run_id, task_id)): Path<(String, String)>,
) -> Result<Json<ApiResponse<()>>, AppError> {
    let run = state
        .engine
        .adopt_task_result(&state.run_service, &user.id, &run_id, &task_id)
        .await?;
    if run.status == "running" && !state.engine.is_running(&run_id) {
        state.engine.start(run_id);
    }
    Ok(Json(ApiResponse::success()))
}
/// [`TaskSpecUpdateRequest`]; the service rejects a blank spec / a `running` task
/// (400) and updates the task's `spec`. No engine call — this only amends the
/// node; a subsequent `rerun` re-executes it with the new spec.
async fn update_task_spec(
    State(state): State<OrchestratorRouterState>,
    Extension(user): Extension<CurrentUser>,
    Path((run_id, task_id)): Path<(String, String)>,
    body: Result<Json<TaskSpecUpdateRequest>, JsonRejection>,
) -> Result<Json<ApiResponse<()>>, AppError> {
    let Json(req) = body.map_err(|e| AppError::BadRequest(e.to_string()))?;
    state
        .run_service
        .update_task_spec(&user.id, &run_id, &task_id, &req.spec)
        .await?;
    Ok(Json(ApiResponse::success()))
}

/// 启动前配置台 (迁移 025, owner-scoped): set/clear a node's per-task model override
/// + 预置要求 via [`TaskConfigUpdateRequest`]. FULL replace of the three fields; the
/// service rejects a `running` task (400). No engine call — `resolve_task_member` /
/// `compose_brief` pick these up at dispatch (pending) or on the next `rerun` (settled).
async fn set_task_config(
    State(state): State<OrchestratorRouterState>,
    Extension(user): Extension<CurrentUser>,
    Path((run_id, task_id)): Path<(String, String)>,
    body: Result<Json<TaskConfigUpdateRequest>, JsonRejection>,
) -> Result<Json<ApiResponse<()>>, AppError> {
    let Json(req) = body.map_err(|e| AppError::BadRequest(e.to_string()))?;
    state
        .run_service
        .set_task_config(
            &user.id,
            &run_id,
            &task_id,
            req.override_provider_id,
            req.override_model,
            req.preset_prompt,
        )
        .await?;
    Ok(Json(ApiResponse::success()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::{ConversationCanceller, RunEngine, RunEngineDeps};
    use crate::events::OrchestratorRunEventEmitter;
    use crate::plan::PlanProducer;
    use crate::run_service::RunService;
    use crate::service::{FleetService, WorkspaceService};
    use crate::worker::{MockWorkerRunner, WorkerOutcome, WorkerRunner};
    use async_trait::async_trait;
    use axum::body::Body;
    use axum::http::Request;
    use nomifun_api_types::{
        CreateFleetRequest, CreateRunRequest, CreateWorkspaceRequest, FleetMember, FleetMemberInput,
        PlannedDag, PlannedTask, WebSocketMessage,
    };
    use nomifun_common::AppError;
    use nomifun_db::{
        SqliteFleetRepository, SqliteOrchWorkspaceRepository, SqliteRunRepository,
        init_database_memory,
    };
    use nomifun_conversation::ConversationService;
    use nomifun_conversation::skill_resolver::{ResolvedAgentSkill, SkillResolver};
    use nomifun_ai_agent::IWorkerTaskManager;
    use nomifun_realtime::EventBroadcaster;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;
    use std::sync::Arc;
    use tower::ServiceExt; // for `oneshot`


    /// No-op broadcaster: the router-builds test never asserts the event trail.
    struct NoopBroadcaster;
    impl EventBroadcaster for NoopBroadcaster {
        fn broadcast(&self, _event: WebSocketMessage<serde_json::Value>) {}
    }

    /// No-op skill resolver for the route state's `ConversationService` (B3): in
    /// these route tests the orchestrator conversation handle only ever serves
    /// `link_orchestrator_run`, so skill resolution is irrelevant.
    struct NoopSkillResolver;
    #[async_trait]
    impl SkillResolver for NoopSkillResolver {
        async fn auto_inject_names(&self) -> Vec<String> {
            Vec::new()
        }
        async fn resolve_skills(&self, _names: &[String]) -> Vec<ResolvedAgentSkill> {
            Vec::new()
        }
        async fn link_workspace_skills(
            &self,
            _workspace: &std::path::Path,
            _rel_dirs: &[&str],
            _skills: &[ResolvedAgentSkill],
        ) -> usize {
            0
        }
    }

    /// No-op worker task manager — the route conversation handle never spawns agents.
    struct NoopTaskManager;
    #[async_trait]
    impl IWorkerTaskManager for NoopTaskManager {
        fn get_task(&self, _: &str) -> Option<nomifun_ai_agent::AgentInstance> {
            None
        }
        async fn get_or_build_task(
            &self,
            _: &str,
            _: nomifun_ai_agent::types::BuildTaskOptions,
        ) -> Result<nomifun_ai_agent::AgentInstance, AppError> {
            Err(AppError::Internal("noop".into()))
        }
        fn kill(&self, _: &str, _: Option<nomifun_common::AgentKillReason>) -> Result<(), AppError> {
            Ok(())
        }
        fn kill_and_wait(
            &self,
            _: &str,
            _: Option<nomifun_common::AgentKillReason>,
        ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>> {
            Box::pin(std::future::ready(()))
        }
        fn clear(&self) {}
        fn active_count(&self) -> usize {
            0
        }
        fn collect_idle(&self, _: nomifun_common::TimestampMs) -> Vec<String> {
            vec![]
        }
    }

    /// Build a `ConversationService` over `pool` for the route state (B3). The
    /// production `build_orchestrator_state` threads in the worker's
    /// `ConversationService`; in tests a no-op-backed instance over the SAME pool
    /// lets the link write `extra.orchestrator_run_id` and the test re-read it.
    fn build_conv_service(
        pool: nomifun_db::SqlitePool,
        broadcaster: Arc<dyn EventBroadcaster>,
    ) -> ConversationService {
        let conv_repo = Arc::new(nomifun_db::SqliteConversationRepository::new(pool.clone()));
        let agent_metadata_repo: Arc<dyn nomifun_db::IAgentMetadataRepository> =
            Arc::new(nomifun_db::SqliteAgentMetadataRepository::new(pool.clone()));
        let acp_session_repo: Arc<dyn nomifun_db::IAcpSessionRepository> =
            Arc::new(nomifun_db::SqliteAcpSessionRepository::new(pool));
        ConversationService::new(
            std::env::temp_dir(),
            broadcaster,
            Arc::new(NoopSkillResolver),
            Arc::new(NoopTaskManager),
            conv_repo,
            agent_metadata_repo,
            acp_session_repo,
        )
    }

    /// Records the NAME of every broadcast event so a test can assert a particular
    /// event (e.g. `orchestrator.run.planUpdated`) eventually fired — used by the
    /// optimistic-create test to confirm the background plan task completed.
    #[derive(Clone)]
    struct NameRecordingBroadcaster {
        names: Arc<Mutex<Vec<String>>>,
    }
    impl NameRecordingBroadcaster {
        fn new() -> Self {
            Self { names: Arc::new(Mutex::new(Vec::new())) }
        }
        fn names(&self) -> Vec<String> {
            self.names.lock().unwrap().clone()
        }
    }
    impl EventBroadcaster for NameRecordingBroadcaster {
        fn broadcast(&self, event: WebSocketMessage<serde_json::Value>) {
            self.names.lock().unwrap().push(event.name.clone());
        }
    }

    /// A planner that BLOCKS in `produce` until a shared gate is released, after
    /// signalling that it was entered. This lets the optimistic-create test prove
    /// the route returns the planning-state run BEFORE planning finishes (the
    /// background spawn is still parked in `produce`).
    struct GatedPlanProducer {
        entered: Arc<tokio::sync::Notify>,
        release: Arc<tokio::sync::Notify>,
    }
    #[async_trait::async_trait]
    impl PlanProducer for GatedPlanProducer {
        async fn produce(
            &self,
            _goal: &str,
            _members: &[FleetMember],
            _sink: Option<&crate::plan::LeadThinkingSink>,
        ) -> Result<PlannedDag, AppError> {
            // Tell the test we are inside produce, then block until released.
            self.entered.notify_one();
            self.release.notified().await;
            Ok(PlannedDag {
                tasks: vec![PlannedTask {
                    title: "planned after release".to_string(),
                    spec: "do it".to_string(),
                    task_profile: None,
                    depends_on: vec![],
                    member_index: Some(0),
                    rationale: None,
                    role: None,
                    kind: "agent".to_string(),
                    pattern_config: None,
                }],
            })
        }
    }

    /// Minimal planner so a RunService can be constructed for the state.
    struct EmptyPlanProducer;
    #[async_trait::async_trait]
    impl PlanProducer for EmptyPlanProducer {
        async fn produce(
            &self,
            _goal: &str,
            _members: &[FleetMember],
            _sink: Option<&crate::plan::LeadThinkingSink>,
        ) -> Result<PlannedDag, AppError> {
            Ok(PlannedDag { tasks: vec![] })
        }
    }

    async fn build_state() -> OrchestratorRouterState {
        build_state_with_run().await.0
    }

    /// Build a fully-wired state AND seed one run (workspace + single-member fleet
    /// + a run parked in `planning`). Returns the state and the seeded run id so a
    /// oneshot can hit `GET /api/orchestrator/runs/{id}` against a real row. We
    /// build the services over the same repos the state holds so the seeded data
    /// is visible to the handlers.
    async fn build_state_with_run() -> (OrchestratorRouterState, String) {
        let db = init_database_memory().await.expect("db init");
        let pool = db.pool().clone();
        let fleet_repo = Arc::new(SqliteFleetRepository::new(pool.clone()));
        let ws_repo = Arc::new(SqliteOrchWorkspaceRepository::new(pool.clone()));
        let run_repo = Arc::new(SqliteRunRepository::new(pool.clone()));
        let fleet = FleetService::new(fleet_repo.clone());
        let workspace = WorkspaceService::new(ws_repo.clone());
        let emitter = OrchestratorRunEventEmitter::new(Arc::new(NoopBroadcaster));
        let planner: Arc<dyn PlanProducer> = Arc::new(EmptyPlanProducer);
        let run_service = Arc::new(RunService::new(
            run_repo.clone(),
            fleet_repo,
            ws_repo.clone(),
            planner,
            emitter.clone(),
        ));
        let worker: Arc<dyn WorkerRunner> = Arc::new(MockWorkerRunner::with_text(1, "x"));
        let engine = RunEngine::new(Arc::new(RunEngineDeps::new(run_repo, worker, emitter, ws_repo)));

        // Seed: fleet (one member) → workspace → run (parked in `planning`).
        let seeded_fleet = fleet
            .create(
                "u1",
                CreateFleetRequest {
                    name: "smoke fleet".to_string(),
                    description: None,
                    max_parallel: None,
                    members: vec![FleetMemberInput {
                        agent_id: "agent_a".to_string(),
                        provider_id: None,
                        model: None,
                        role_hint: None,
                        capability_profile: None,
                        constraints: None,
                        sort_order: None,
                    }],
                },
            )
            .await
            .expect("seed fleet");
        let seeded_ws = workspace
            .create(
                "u1",
                CreateWorkspaceRequest {
                    name: "smoke ws".to_string(),
                    default_fleet_id: Some(seeded_fleet.id.clone()),
                    workspace_dir: None,
                },
            )
            .await
            .expect("seed workspace");
        let run = run_service
            .create(
                "u1",
                CreateRunRequest {
                    workspace_id: seeded_ws.id,
                    goal: "smoke goal".to_string(),
                    fleet_id: seeded_fleet.id,
                    autonomy: None,
                    max_parallel: None,
                },
            )
            .await
            .expect("seed run");

        let state = OrchestratorRouterState::new(
            fleet,
            workspace,
            run_service,
            engine,
            build_conv_service(pool, Arc::new(NoopBroadcaster)),
        );
        (state, run.id)
    }

    /// The router builds without panicking.
    #[tokio::test]
    async fn router_builds() {
        let state = build_state().await;
        let _router = orchestrator_routes(state);
    }

    /// `GET /api/orchestrator/fleets` returns 200 once a `CurrentUser` extension
    /// is present. We inject it via a layer here, exactly as the auth middleware
    /// does in nomifun-app — so the handler's `Extension<CurrentUser>` requirement
    /// is exercised, not bypassed. (The full auth-wired path is covered by Task 8's
    /// app-level integration test.)
    #[tokio::test]
    async fn list_fleets_returns_ok_with_user() {
        let state = build_state().await;
        let app = orchestrator_routes(state).layer(axum::Extension(CurrentUser {
            id: "u1".to_string(),
            username: "tester".to_string(),
        }));

        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/api/orchestrator/fleets")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .expect("request");
        assert_eq!(resp.status(), StatusCode::OK);
    }

    /// Without the `CurrentUser` extension the handler cannot run — axum returns
    /// 500 (missing required extension). This guards that we did NOT weaken the
    /// handler by dropping the `Extension<CurrentUser>` requirement.
    #[tokio::test]
    async fn list_fleets_without_user_is_not_ok() {
        let state = build_state().await;
        let app = orchestrator_routes(state);

        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/api/orchestrator/fleets")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .expect("request");
        assert_ne!(resp.status(), StatusCode::OK);
    }

    /// `GET /api/orchestrator/runs/{id}` returns 200 for a seeded run once a
    /// `CurrentUser` extension is present. This exercises the new run route end to
    /// end through the router (path extraction → RunService::get_detail → 200) and
    /// confirms the route is actually mounted — before the route existed axum would
    /// have routed this to a 404. (Full HTTP behavior is covered by Task 9.)
    #[tokio::test]
    async fn get_run_returns_ok_with_user() {
        let (state, run_id) = build_state_with_run().await;
        let app = orchestrator_routes(state).layer(axum::Extension(CurrentUser {
            id: "u1".to_string(),
            username: "tester".to_string(),
        }));

        let resp = app
            .oneshot(
                Request::builder()
                    .uri(format!("/api/orchestrator/runs/{run_id}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .expect("request");
        assert_eq!(resp.status(), StatusCode::OK);
    }

    /// The `GET /api/orchestrator/runs/{id}` route still requires the
    /// `CurrentUser` extension — without it the handler cannot run (axum returns a
    /// non-200). Guards that the run route was not wired without auth.
    #[tokio::test]
    async fn get_run_without_user_is_not_ok() {
        let (state, run_id) = build_state_with_run().await;
        let app = orchestrator_routes(state);

        let resp = app
            .oneshot(
                Request::builder()
                    .uri(format!("/api/orchestrator/runs/{run_id}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .expect("request");
        assert_ne!(resp.status(), StatusCode::OK);
    }

    /// `GET /api/orchestrator/runs` (list-my-runs) returns 200 with a `CurrentUser`
    /// extension present — exercising the new read path for the Run-history library
    /// end to end (auth extract → RunService::list_by_user → 200) and confirming
    /// the GET method was actually mounted on the `/runs` path (which previously
    /// only carried POST). The seeded run belongs to "u1", the same user injected.
    #[tokio::test]
    async fn list_my_runs_returns_ok_with_user() {
        let (state, _run_id) = build_state_with_run().await;
        let app = orchestrator_routes(state).layer(axum::Extension(CurrentUser {
            id: "u1".to_string(),
            username: "tester".to_string(),
        }));

        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/api/orchestrator/runs")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .expect("request");
        assert_eq!(resp.status(), StatusCode::OK);
    }

    /// `GET /api/orchestrator/runs` still requires the `CurrentUser` extension —
    /// without it the handler cannot run (axum returns a non-200). Guards that the
    /// list-my-runs route was not wired as a public route (a public handler that
    /// extracted `Extension<CurrentUser>` would 500 with axum 0.8 MissingExtension).
    #[tokio::test]
    async fn list_my_runs_without_user_is_not_ok() {
        let state = build_state().await;
        let app = orchestrator_routes(state);

        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/api/orchestrator/runs")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .expect("request");
        assert_ne!(resp.status(), StatusCode::OK);
    }

    /// `POST /api/orchestrator/runs/adhoc` with a `CurrentUser` extension creates
    /// an ad-hoc run straight from a structured form body (no workspace, no
    /// pre-built fleet): the fleet is synthesized from the `model_range` (a 2-model
    /// `range` here), the run is planned, and — because the ad-hoc front door
    /// DEFAULTS autonomy to `interactive` (the Tab审批门) when the body omits it —
    /// the run parks at `awaiting_plan_approval` (NOT `running`) and the engine is
    /// NOT started. Exercises the new route end to end through the router
    /// (auth extract → create_adhoc → plan → autonomy gate → 201). RED before the
    /// route exists (404 ≠ CREATED), GREEN after.
    #[tokio::test]
    async fn create_adhoc_run_parks_at_awaiting_plan_approval_with_user() {
        let state = build_state().await;
        // Keep a handle to read back the created run's persisted status.
        let run_service = state.run_service.clone();
        let app = orchestrator_routes(state).layer(axum::Extension(CurrentUser {
            id: "u1".to_string(),
            username: "tester".to_string(),
        }));

        let body = serde_json::json!({
            "goal": "ad-hoc smoke goal",
            "model_range": {
                "mode": "range",
                "models": [
                    { "provider_id": "openai", "model": "gpt-x" },
                    { "provider_id": "anthropic", "model": "claude-y" }
                ]
            }
        })
        .to_string();

        let resp = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/orchestrator/runs/adhoc")
                    .header("content-type", "application/json")
                    .body(Body::from(body))
                    .unwrap(),
            )
            .await
            .expect("request");
        assert_eq!(
            resp.status(),
            StatusCode::CREATED,
            "ad-hoc create must 201 CREATED"
        );

        // Parse the returned Run and confirm the interactive-default autonomy was
        // PERSISTED on the request. With optimistic create the route returns in the
        // `planning` state and the autonomy gate is applied by the BACKGROUND plan,
        // so we poll for the eventual `awaiting_plan_approval` (the engine is NOT
        // started for an interactive run).
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .expect("body bytes");
        let api: ApiResponse<Run> = serde_json::from_slice(&bytes).expect("decode ApiResponse<Run>");
        let run = api.data.expect("run in response");
        assert_eq!(run.autonomy, "interactive", "ad-hoc default autonomy");

        let mut status = String::new();
        for _ in 0..200 {
            status = run_service.get_detail(&run.id).await.expect("detail").run.status;
            if status == "awaiting_plan_approval" {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert_eq!(
            status, "awaiting_plan_approval",
            "interactive ad-hoc run must park at awaiting_plan_approval after background plan"
        );
    }

    /// **Optimistic create (B3):** `POST /api/orchestrator/runs/adhoc` returns the
    /// run IMMEDIATELY in the `planning` state with ZERO tasks — planning runs in a
    /// background `tokio::spawn`, NOT inline. We prove it with a planner that BLOCKS
    /// in `produce` until the test releases it: with the optimistic change the route
    /// responds (201, planning, 0 tasks) while the planner is still parked; the
    /// pre-fix inline route would be stuck inside `produce` and the bounded `timeout`
    /// below would elapse (RED). After releasing the gate, the background plan
    /// completes and emits `planUpdated`.
    #[tokio::test]
    async fn create_adhoc_run_returns_planning_before_plan_completes() {
        let db = init_database_memory().await.expect("db init");
        let pool = db.pool().clone();
        let fleet_repo = Arc::new(SqliteFleetRepository::new(pool.clone()));
        let ws_repo = Arc::new(SqliteOrchWorkspaceRepository::new(pool.clone()));
        let run_repo = Arc::new(SqliteRunRepository::new(pool.clone()));
        let fleet = FleetService::new(fleet_repo.clone());
        let workspace = WorkspaceService::new(ws_repo.clone());
        let bc = Arc::new(NameRecordingBroadcaster::new());
        let emitter = OrchestratorRunEventEmitter::new(bc.clone());

        // A planner gated on two Notifys: signal-on-entry + block-until-released.
        let entered = Arc::new(tokio::sync::Notify::new());
        let release = Arc::new(tokio::sync::Notify::new());
        let planner: Arc<dyn PlanProducer> = Arc::new(GatedPlanProducer {
            entered: entered.clone(),
            release: release.clone(),
        });
        let run_service = Arc::new(RunService::new(
            run_repo.clone(),
            fleet_repo,
            ws_repo.clone(),
            planner,
            emitter.clone(),
        ));
        let worker: Arc<dyn WorkerRunner> = Arc::new(MockWorkerRunner::with_text(1, "x"));
        let engine =
            RunEngine::new(Arc::new(RunEngineDeps::new(run_repo, worker, emitter, ws_repo)));

        let run_service_for_assert = run_service.clone();
        let state = OrchestratorRouterState::new(
            fleet,
            workspace,
            run_service,
            engine,
            build_conv_service(pool, Arc::new(NoopBroadcaster)),
        );
        let app = orchestrator_routes(state).layer(axum::Extension(CurrentUser {
            id: "u1".to_string(),
            username: "tester".to_string(),
        }));

        let body = serde_json::json!({
            "goal": "optimistic goal",
            "model_range": { "mode": "single", "model": { "provider_id": "p", "model": "m" } }
        })
        .to_string();

        // The route MUST return without waiting on the (still-gated) planner. Bound
        // it with a timeout so the pre-fix inline route fails loudly (RED) rather
        // than hanging the test.
        let resp = tokio::time::timeout(
            Duration::from_secs(5),
            app.oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/orchestrator/runs/adhoc")
                    .header("content-type", "application/json")
                    .body(Body::from(body))
                    .unwrap(),
            ),
        )
        .await
        .expect("route must return before planning completes (optimistic create)")
        .expect("request");
        assert_eq!(resp.status(), StatusCode::CREATED, "optimistic create must 201");

        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .expect("body bytes");
        let api: ApiResponse<Run> = serde_json::from_slice(&bytes).expect("decode ApiResponse<Run>");
        let run = api.data.expect("run in response");

        // At return time the run is still `planning` with NO tasks (the gated
        // planner has not produced a DAG yet) — confirm the background plan really
        // is still parked by waiting for it to ENTER `produce`.
        tokio::time::timeout(Duration::from_secs(5), entered.notified())
            .await
            .expect("background plan task must reach produce()");
        let detail = run_service_for_assert.get_detail(&run.id).await.expect("detail");
        assert_eq!(detail.run.status, "planning", "run returned in planning state");
        assert_eq!(detail.tasks.len(), 0, "no tasks persisted before plan completes");

        // Release the gate; the background plan finishes and emits planUpdated.
        release.notify_one();
        let mut saw_plan_updated = false;
        for _ in 0..200 {
            if bc.names().iter().any(|n| n == "orchestrator.run.planUpdated") {
                saw_plan_updated = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(
            saw_plan_updated,
            "background planning must complete and emit planUpdated after release; saw {:?}",
            bc.names()
        );
    }

    /// `POST /api/orchestrator/runs/adhoc` still requires the `CurrentUser`
    /// extension — without it the handler cannot run (axum returns a non-200).
    /// Guards that the ad-hoc create route was not wired as a public route (a
    /// public handler that extracted `Extension<CurrentUser>` would 500 with
    /// axum 0.8 MissingExtension).
    #[tokio::test]
    async fn create_adhoc_run_without_user_is_not_ok() {
        let state = build_state().await;
        let app = orchestrator_routes(state);

        let body = serde_json::json!({
            "goal": "ad-hoc smoke goal",
            "model_range": {
                "mode": "range",
                "models": [
                    { "provider_id": "openai", "model": "gpt-x" },
                    { "provider_id": "anthropic", "model": "claude-y" }
                ]
            }
        })
        .to_string();

        let resp = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/orchestrator/runs/adhoc")
                    .header("content-type", "application/json")
                    .body(Body::from(body))
                    .unwrap(),
            )
            .await
            .expect("request");
        assert_ne!(resp.status(), StatusCode::OK);
    }

    // -------------------------------------------------------------------------
    // B3 (Path B): `create_adhoc_run` associates the ORIGINATING conversation as
    // the run's lead when the request carries `lead_conv_id`. We build a state
    // whose route `ConversationService` runs over the SAME in-memory pool (so the
    // link write is re-readable), seed a conversation, POST adhoc with
    // `lead_conv_id: Some(conv)`, then assert (a) the returned run carries that
    // `lead_conv_id` and (b) the conversation got `extra.orchestrator_run_id`
    // written (the inverse pointer the FE reads to light up the canvas). A second
    // test proves `lead_conv_id: None` does NOT link (regression — legacy callers).
    // The link is a synchronous best-effort await BEFORE the optimistic return, so
    // by the time the response is in hand the link has already run — no planner gate
    // is needed for these assertions.
    // -------------------------------------------------------------------------

    /// Build a full state over a fresh in-memory pool, returning the state, the
    /// route `ConversationService` (same pool, so a test can re-read conversation
    /// extra), and the `RunService` (so a test can read back the created run).
    async fn build_state_with_conv() -> (OrchestratorRouterState, ConversationService, Arc<RunService>)
    {
        let db = init_database_memory().await.expect("db init");
        let pool = db.pool().clone();
        let fleet_repo = Arc::new(SqliteFleetRepository::new(pool.clone()));
        let ws_repo = Arc::new(SqliteOrchWorkspaceRepository::new(pool.clone()));
        let run_repo = Arc::new(SqliteRunRepository::new(pool.clone()));
        let fleet = FleetService::new(fleet_repo.clone());
        let workspace = WorkspaceService::new(ws_repo.clone());
        let emitter = OrchestratorRunEventEmitter::new(Arc::new(NoopBroadcaster));
        let planner: Arc<dyn PlanProducer> = Arc::new(EmptyPlanProducer);
        let run_service = Arc::new(RunService::new(
            run_repo.clone(),
            fleet_repo,
            ws_repo.clone(),
            planner,
            emitter.clone(),
        ));
        let worker: Arc<dyn WorkerRunner> = Arc::new(MockWorkerRunner::with_text(1, "x"));
        let engine = RunEngine::new(Arc::new(RunEngineDeps::new(run_repo, worker, emitter, ws_repo)));
        // The route's ConversationService over the SAME pool — the test re-reads the
        // conversation it seeds to confirm the link write.
        let conv = build_conv_service(pool, Arc::new(NoopBroadcaster));
        let state = OrchestratorRouterState::new(
            fleet,
            workspace,
            run_service.clone(),
            engine,
            conv.clone(),
        );
        (state, conv, run_service)
    }

    /// Seed a bare conversation and return its integer id (the FE would pass this
    /// as `lead_conv_id`). Created as the seeded `system_default_user` (the only
    /// user `init_database_memory` provisions — `conversations.user_id` FKs to it)
    /// so the insert + a later re-read both succeed. `link_orchestrator_run` itself
    /// is owner-agnostic (it writes by conversation id), so the run's own owner
    /// ("u1" from the route's CurrentUser) is irrelevant to the link.
    async fn seed_conversation(conv: &ConversationService) -> i64 {
        let req: nomifun_api_types::CreateConversationRequest =
            serde_json::from_value(serde_json::json!({ "type": "acp", "extra": {} })).expect("conv req");
        conv.create(nomifun_auth::SYSTEM_USER_ID, req)
            .await
            .expect("seed conversation")
            .id
    }

    /// `POST /runs/adhoc` with `lead_conv_id: Some(conv)` → the returned run carries
    /// that lead AND the originating conversation is linked
    /// (`extra.orchestrator_run_id == run.id`). RED before the route calls
    /// `link_orchestrator_run`; GREEN after.
    #[tokio::test]
    async fn create_adhoc_run_links_originating_conversation() {
        let (state, conv, _run_service) = build_state_with_conv().await;
        let conv_id = seed_conversation(&conv).await;
        let app = orchestrator_routes(state).layer(axum::Extension(CurrentUser {
            id: "u1".to_string(),
            username: "tester".to_string(),
        }));

        let body = serde_json::json!({
            "goal": "linked goal",
            "model_range": { "mode": "single", "model": { "provider_id": "p", "model": "m" } },
            "lead_conv_id": conv_id
        })
        .to_string();

        let resp = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/orchestrator/runs/adhoc")
                    .header("content-type", "application/json")
                    .body(Body::from(body))
                    .unwrap(),
            )
            .await
            .expect("request");
        assert_eq!(resp.status(), StatusCode::CREATED, "adhoc create must 201");

        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .expect("body bytes");
        let api: ApiResponse<Run> = serde_json::from_slice(&bytes).expect("decode ApiResponse<Run>");
        let run = api.data.expect("run in response");
        assert_eq!(
            run.lead_conv_id,
            Some(conv_id),
            "returned run must carry the originating lead_conv_id"
        );

        // The link write-back is a synchronous await before the optimistic return,
        // so the conversation's extra is already updated by now.
        let fetched = conv.get(nomifun_auth::SYSTEM_USER_ID, &conv_id.to_string()).await.expect("re-read conversation");
        assert_eq!(
            fetched.extra["orchestrator_run_id"], run.id,
            "originating conversation must be linked to the run (extra.orchestrator_run_id)"
        );
    }

    /// Regression: `POST /runs/adhoc` WITHOUT `lead_conv_id` (legacy callers / no
    /// originating conversation) must NOT link any conversation. We seed a
    /// conversation (which the route never sees) and confirm it gains no
    /// `orchestrator_run_id` after an unlinked adhoc create.
    #[tokio::test]
    async fn create_adhoc_run_without_lead_conv_id_does_not_link() {
        let (state, conv, _run_service) = build_state_with_conv().await;
        let conv_id = seed_conversation(&conv).await;
        let app = orchestrator_routes(state).layer(axum::Extension(CurrentUser {
            id: "u1".to_string(),
            username: "tester".to_string(),
        }));

        // No lead_conv_id in the body — the run is workspace-less + conversation-less.
        let body = serde_json::json!({
            "goal": "unlinked goal",
            "model_range": { "mode": "single", "model": { "provider_id": "p", "model": "m" } }
        })
        .to_string();

        let resp = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/orchestrator/runs/adhoc")
                    .header("content-type", "application/json")
                    .body(Body::from(body))
                    .unwrap(),
            )
            .await
            .expect("request");
        assert_eq!(resp.status(), StatusCode::CREATED, "adhoc create must 201");

        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .expect("body bytes");
        let api: ApiResponse<Run> = serde_json::from_slice(&bytes).expect("decode ApiResponse<Run>");
        let run = api.data.expect("run in response");
        assert_eq!(run.lead_conv_id, None, "no lead when lead_conv_id omitted");

        // The seeded conversation was never the lead → it must carry no run link.
        let fetched = conv.get(nomifun_auth::SYSTEM_USER_ID, &conv_id.to_string()).await.expect("re-read conversation");
        assert!(
            fetched.extra.get("orchestrator_run_id").is_none(),
            "unlinked adhoc create must NOT write orchestrator_run_id; got {:?}",
            fetched.extra
        );
    }

    // -------------------------------------------------------------------------
    // Mirror the seeded-run smoke pattern: with the CurrentUser layer the owner
    // hits the route end to end (200/OK), and without the layer the handler
    // cannot run (axum 0.8 MissingExtension → non-200) — guarding the new routes
    // were mounted UNDER auth, never as public routes.
    // -------------------------------------------------------------------------

    /// `DELETE /api/orchestrator/runs/{id}` returns 200 for the owner of a seeded
    /// run (auth extract → engine.stop → RunService::delete → 200) and confirms
    /// the route is mounted (before it existed axum would 404/405 this).
    #[tokio::test]
    async fn delete_run_returns_ok_with_user() {
        let (state, run_id) = build_state_with_run().await; // run owned by "u1"
        let app = orchestrator_routes(state).layer(axum::Extension(CurrentUser {
            id: "u1".to_string(),
            username: "tester".to_string(),
        }));

        let resp = app
            .oneshot(
                Request::builder()
                    .method("DELETE")
                    .uri(format!("/api/orchestrator/runs/{run_id}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .expect("request");
        assert_eq!(resp.status(), StatusCode::OK);
    }

    /// `DELETE /api/orchestrator/runs/{id}` still requires the `CurrentUser`
    /// extension — without it the handler cannot run (non-200). Guards the delete
    /// route was not wired as a public route.
    #[tokio::test]
    async fn delete_run_without_user_is_not_ok() {
        let (state, run_id) = build_state_with_run().await;
        let app = orchestrator_routes(state);

        let resp = app
            .oneshot(
                Request::builder()
                    .method("DELETE")
                    .uri(format!("/api/orchestrator/runs/{run_id}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .expect("request");
        assert_ne!(resp.status(), StatusCode::OK);
    }

    /// `PATCH /api/orchestrator/runs/{id}` with a `RenameRequest` body returns 200
    /// for the owner (auth extract → RunService::rename → 200) and confirms the
    /// PATCH method is mounted on the `/runs/{id}` path.
    #[tokio::test]
    async fn rename_run_returns_ok_with_user() {
        let (state, run_id) = build_state_with_run().await; // run owned by "u1"
        let app = orchestrator_routes(state).layer(axum::Extension(CurrentUser {
            id: "u1".to_string(),
            username: "tester".to_string(),
        }));

        let body = serde_json::json!({ "goal": "重命名后的目标" }).to_string();
        let resp = app
            .oneshot(
                Request::builder()
                    .method("PATCH")
                    .uri(format!("/api/orchestrator/runs/{run_id}"))
                    .header("content-type", "application/json")
                    .body(Body::from(body))
                    .unwrap(),
            )
            .await
            .expect("request");
        assert_eq!(resp.status(), StatusCode::OK);
    }

    /// `PATCH /api/orchestrator/runs/{id}` still requires the `CurrentUser`
    /// extension — without it the handler cannot run (non-200). Guards the rename
    /// route was not wired as a public route.
    #[tokio::test]
    async fn rename_run_without_user_is_not_ok() {
        let (state, run_id) = build_state_with_run().await;
        let app = orchestrator_routes(state);

        let body = serde_json::json!({ "goal": "x" }).to_string();
        let resp = app
            .oneshot(
                Request::builder()
                    .method("PATCH")
                    .uri(format!("/api/orchestrator/runs/{run_id}"))
                    .header("content-type", "application/json")
                    .body(Body::from(body))
                    .unwrap(),
            )
            .await
            .expect("request");
        assert_ne!(resp.status(), StatusCode::OK);
    }

    // -------------------------------------------------------------------------
    // P1 Task 2: POST /runs/{id}/replan route. Mirrors the seeded-run smoke
    // pattern: with the CurrentUser layer the owner hits the route end to end
    // (200/OK), and without the layer the handler cannot run (non-200) — guarding
    // the replan route was mounted UNDER auth, never as a public route.
    // -------------------------------------------------------------------------

    /// `POST /api/orchestrator/runs/{id}/replan` with a `ReplanRequest` body
    /// returns 200 for the owner (auth extract → engine.stop → RunService::replan
    /// → 200) and confirms the route is mounted (before it existed axum 404s).
    #[tokio::test]
    async fn replan_run_returns_ok_with_user() {
        let (state, run_id) = build_state_with_run().await; // run owned by "u1"
        let app = orchestrator_routes(state).layer(axum::Extension(CurrentUser {
            id: "u1".to_string(),
            username: "tester".to_string(),
        }));

        let body = serde_json::json!({ "goal": "重新规划的目标" }).to_string();
        let resp = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(format!("/api/orchestrator/runs/{run_id}/replan"))
                    .header("content-type", "application/json")
                    .body(Body::from(body))
                    .unwrap(),
            )
            .await
            .expect("request");
        assert_eq!(resp.status(), StatusCode::OK);
    }

    /// `POST /api/orchestrator/runs/{id}/replan` still requires the `CurrentUser`
    /// extension — without it the handler cannot run (non-200). Guards the replan
    /// route was not wired as a public route.
    #[tokio::test]
    async fn replan_run_without_user_is_not_ok() {
        let (state, run_id) = build_state_with_run().await;
        let app = orchestrator_routes(state);

        let body = serde_json::json!({}).to_string();
        let resp = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(format!("/api/orchestrator/runs/{run_id}/replan"))
                    .header("content-type", "application/json")
                    .body(Body::from(body))
                    .unwrap(),
            )
            .await
            .expect("request");
        assert_ne!(resp.status(), StatusCode::OK);
    }

    // -------------------------------------------------------------------------
    // UC-3a: POST /runs/{id}/adjust route. Mirrors the seeded-run smoke pattern.
    // The seeded run is planned by EmptyPlanProducer (no `adjust` support → the
    // default trait `adjust` errors), so an owner reaching the handler gets a
    // BadRequest (NOT 404/405): that still confirms the route is MOUNTED UNDER
    // auth + reaches the service. Without the CurrentUser layer the handler cannot
    // run (axum 0.8 MissingExtension → 500) — guarding it is not a public route.
    // -------------------------------------------------------------------------

    /// `POST /api/orchestrator/runs/{id}/adjust` with an `AdjustRunRequest` body is
    /// MOUNTED under auth: with the CurrentUser layer the owner reaches the handler
    /// (the route is not 404/405). The seeded run's planner does not support adjust
    /// so the body errors with 400 — which still proves routing + auth extraction.
    #[tokio::test]
    async fn adjust_run_is_mounted_under_auth() {
        let (state, run_id) = build_state_with_run().await; // run owned by "u1"
        let app = orchestrator_routes(state).layer(axum::Extension(CurrentUser {
            id: "u1".to_string(),
            username: "tester".to_string(),
        }));

        let body = serde_json::json!({ "intent": "把报告改成中文" }).to_string();
        let resp = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(format!("/api/orchestrator/runs/{run_id}/adjust"))
                    .header("content-type", "application/json")
                    .body(Body::from(body))
                    .unwrap(),
            )
            .await
            .expect("request");
        // Mounted + reached the handler: NOT a routing miss (404) / method miss (405).
        assert_ne!(resp.status(), StatusCode::NOT_FOUND, "adjust route must be mounted");
        assert_ne!(resp.status(), StatusCode::METHOD_NOT_ALLOWED, "POST must be allowed");
    }

    /// `POST /api/orchestrator/runs/{id}/adjust` still requires the `CurrentUser`
    /// extension — without it the handler cannot run (non-200). Guards the adjust
    /// route was not wired as a public route.
    #[tokio::test]
    async fn adjust_run_without_user_is_not_ok() {
        let (state, run_id) = build_state_with_run().await;
        let app = orchestrator_routes(state);
        let body = serde_json::json!({ "intent": "x" }).to_string();
        let resp = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(format!("/api/orchestrator/runs/{run_id}/adjust"))
                    .header("content-type", "application/json")
                    .body(Body::from(body))
                    .unwrap(),
            )
            .await
            .expect("request");
        assert_ne!(resp.status(), StatusCode::OK);
    }
    // worker. At cap=1, pausing a run with one live worker is the standard pause
    // state; the buggy resume_run called `engine.start` UNCONDITIONALLY, whose
    // `stop()` cancels every in-flight worker conversation — destroying the live
    // work that pause was meant to let finish. This test hits the REAL resume
    // route handler, so it is RED before the `!is_running` gate and GREEN after.
    // -------------------------------------------------------------------------

    /// Single independent task DAG (one task, no deps), pre-assigned to member 0 —
    /// so at cap=1 there is exactly one in-flight worker when the run is paused.
    struct SingleTaskPlanProducer;
    #[async_trait]
    impl PlanProducer for SingleTaskPlanProducer {
        async fn produce(
            &self,
            _goal: &str,
            _members: &[FleetMember],
            _sink: Option<&crate::plan::LeadThinkingSink>,
        ) -> Result<PlannedDag, AppError> {
            Ok(PlannedDag {
                tasks: vec![PlannedTask {
                    title: "solo".to_string(),
                    spec: "do the work".to_string(),
                    task_profile: None,
                    depends_on: vec![],
                    member_index: Some(0),
                    rationale: None,
                    role: None,
                    kind: "agent".to_string(),
                    pattern_config: None,
                }],
            })
        }
    }

    /// Records every conversation id it was asked to cancel — the test asserts the
    /// in-flight conv was NEVER passed here across pause→resume.
    struct RecordingCanceller {
        cancelled: Arc<Mutex<Vec<i64>>>,
    }
    impl RecordingCanceller {
        fn new() -> Self {
            Self {
                cancelled: Arc::new(Mutex::new(vec![])),
            }
        }
        fn handle(&self) -> Arc<Mutex<Vec<i64>>> {
            self.cancelled.clone()
        }
    }
    #[async_trait]
    impl ConversationCanceller for RecordingCanceller {
        async fn cancel(&self, conversation_id: i64) {
            self.cancelled.lock().unwrap().push(conversation_id);
        }
    }

    /// A worker that stamps a distinct conv id via `on_started`, then blocks on a
    /// shared gate until the test releases it — keeping the worker provably
    /// in-flight across the pause→resume window.
    struct GatedWorkerRunner {
        gate: Arc<tokio::sync::Notify>,
        next_conv_id: AtomicUsize,
    }
    impl GatedWorkerRunner {
        fn new(gate: Arc<tokio::sync::Notify>) -> Self {
            Self {
                gate,
                next_conv_id: AtomicUsize::new(8000),
            }
        }
    }
    #[async_trait]
    impl WorkerRunner for GatedWorkerRunner {
        async fn run(
            &self,
            _member: &FleetMember,
            _workspace_dir: Option<&str>,
            _run_id: &str,
            task_id: &str,
            _brief: &str,
            _task_spec: &str,
            _timeout: Duration,
            on_started: Box<dyn FnOnce(i64) + Send>,
        ) -> Result<WorkerOutcome, AppError> {
            let conv_id = self.next_conv_id.fetch_add(1, Ordering::SeqCst) as i64;
            on_started(conv_id);
            self.gate.notified().await;
            Ok(WorkerOutcome {
                conversation_id: conv_id,
                text: Some(format!("output of {task_id}")),
                ok: true,
                tokens: None,
            })
        }
    }

    #[tokio::test]
    async fn resume_route_does_not_cancel_in_flight_worker() {
        let db = init_database_memory().await.expect("db init");
        let pool = db.pool().clone();
        let fleet_repo = Arc::new(SqliteFleetRepository::new(pool.clone()));
        let ws_repo = Arc::new(SqliteOrchWorkspaceRepository::new(pool.clone()));
        let run_repo = Arc::new(SqliteRunRepository::new(pool.clone()));
        let fleet = FleetService::new(fleet_repo.clone());
        let workspace = WorkspaceService::new(ws_repo.clone());
        let emitter = OrchestratorRunEventEmitter::new(Arc::new(NoopBroadcaster));
        let planner: Arc<dyn PlanProducer> = Arc::new(SingleTaskPlanProducer);
        let run_service = Arc::new(RunService::new(
            run_repo.clone(),
            fleet_repo.clone(),
            ws_repo.clone(),
            planner,
            emitter.clone(),
        ));

        // cap=1, gated worker → the single task is in-flight (blocked on the gate)
        // when we pause. A RecordingCanceller proves resume does not tear it down.
        let gate = Arc::new(tokio::sync::Notify::new());
        let worker: Arc<dyn WorkerRunner> = Arc::new(GatedWorkerRunner::new(gate.clone()));
        let canceller = Arc::new(RecordingCanceller::new());
        let recorded_cancels = canceller.handle();
        let mut engine_deps =
            RunEngineDeps::new(run_repo.clone(), worker, emitter, ws_repo.clone());
        engine_deps.worker_timeout = Duration::from_secs(60);
        engine_deps.default_max_parallel = 1;
        engine_deps.cancel_conversation = canceller;
        let engine = RunEngine::new(Arc::new(engine_deps));

        // Seed fleet (one member) → workspace → run (cap=1) → plan.
        let seeded_fleet = fleet
            .create(
                "u1",
                CreateFleetRequest {
                    name: "resume fleet".to_string(),
                    description: None,
                    max_parallel: None,
                    members: vec![FleetMemberInput {
                        agent_id: "agent_a".to_string(),
                        provider_id: None,
                        model: None,
                        role_hint: None,
                        capability_profile: None,
                        constraints: None,
                        sort_order: None,
                    }],
                },
            )
            .await
            .expect("fleet");
        let seeded_ws = workspace
            .create(
                "u1",
                CreateWorkspaceRequest {
                    name: "resume ws".to_string(),
                    default_fleet_id: Some(seeded_fleet.id.clone()),
                    workspace_dir: None,
                },
            )
            .await
            .expect("ws");
        let run = run_service
            .create(
                "u1",
                CreateRunRequest {
                    workspace_id: seeded_ws.id,
                    goal: "resume must preserve in-flight".to_string(),
                    fleet_id: seeded_fleet.id,
                    autonomy: None,
                    max_parallel: Some(1),
                },
            )
            .await
            .expect("run");
        run_service.plan(&run.id).await.expect("plan");
        let run_id = run.id.clone();

        // Keep an engine clone (RunEngine is Clone — cheap Arc internals) so the
        // test can start the loop directly; the router consumes the original into
        // state. Pause/resume are then driven through the REAL route handlers.
        let engine_for_test = engine.clone();
        let state = OrchestratorRouterState::new(
            fleet,
            workspace,
            run_service.clone(),
            engine,
            build_conv_service(pool, Arc::new(NoopBroadcaster)),
        );
        let app = orchestrator_routes(state).layer(axum::Extension(CurrentUser {
            id: "u1".to_string(),
            username: "tester".to_string(),
        }));

        // The orchestrator has no "start" route (runs start at create/approve/
        // resume); the run is already planned+`running`, so start its loop here.
        engine_for_test.start(run_id.clone());

        // Wait until the single task is `running` with its conversation_id stamped.
        let mut in_flight_conv: Option<i64> = None;
        for _ in 0..200 {
            let detail = run_service.get_detail(&run_id).await.expect("detail");
            in_flight_conv = detail
                .tasks
                .iter()
                .find(|t| t.status == "running")
                .and_then(|t| t.conversation_id);
            if in_flight_conv.is_some() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let in_flight_conv = in_flight_conv.expect("task running with stamped conv id");

        // Pause via the real route, then resume via the real route — the path the
        // bug lives in. The pre-fix resume_run calls engine.start unconditionally,
        // cancelling the in-flight worker.
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(format!("/api/orchestrator/runs/{run_id}/pause"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .expect("pause request");
        assert_eq!(resp.status(), StatusCode::OK, "pause must 200");

        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(format!("/api/orchestrator/runs/{run_id}/resume"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .expect("resume request");
        assert_eq!(resp.status(), StatusCode::OK, "resume must 200");

        // Release the gated worker so it finishes, then poll to completion.
        tokio::spawn({
            let gate = gate.clone();
            async move {
                for _ in 0..10 {
                    gate.notify_one();
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
            }
        });
        let mut final_status = String::new();
        for _ in 0..200 {
            let d = run_service.get_detail(&run_id).await.expect("detail");
            final_status = d.run.status.clone();
            if final_status == "completed" || final_status == "failed" {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert_eq!(final_status, "completed", "resumed run must complete, not fail");

        let detail = run_service.get_detail(&run_id).await.expect("detail");
        assert_eq!(
            detail.tasks[0].status, "done",
            "the in-flight worker preserved across pause→resume must settle `done`, got {}",
            detail.tasks[0].status
        );
        let cancels = recorded_cancels.lock().unwrap().clone();
        assert!(
            !cancels.contains(&in_flight_conv),
            "resume must NOT cancel the in-flight worker conversation {in_flight_conv}; cancelled={cancels:?}"
        );
    }

    // -------------------------------------------------------------------------
    // T3: run workspace browse route
    // -------------------------------------------------------------------------

    /// `GET /api/orchestrator/runs/{id}/workspace` lists the run's working
    /// directory for the owner: seeds an ad-hoc run rooted at a temp dir with a
    /// file (via the real `/runs/adhoc` route), then browses it and confirms the
    /// file is in the response. Exercises the route end to end (auth extract →
    /// owned_run → dir resolve → list_workspace_level → 200).
    #[tokio::test]
    async fn browse_run_workspace_lists_files_for_owner() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join("readme.md"), b"x").expect("write file");
        let work_dir = dir.path().to_string_lossy().into_owned();

        let state = build_state().await;
        let app = orchestrator_routes(state).layer(axum::Extension(CurrentUser {
            id: "u1".to_string(),
            username: "tester".to_string(),
        }));

        // Seed an ad-hoc run rooted at the temp dir through the real create route.
        let create_body = serde_json::json!({
            "goal": "browse via route",
            "work_dir": work_dir,
            "model_range": { "mode": "single", "model": { "provider_id": "p", "model": "m" } },
            "autonomy": "supervised"
        })
        .to_string();
        let created = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/orchestrator/runs/adhoc")
                    .header("content-type", "application/json")
                    .body(Body::from(create_body))
                    .unwrap(),
            )
            .await
            .expect("create request");
        assert_eq!(created.status(), StatusCode::CREATED);
        let bytes = axum::body::to_bytes(created.into_body(), usize::MAX)
            .await
            .expect("create body");
        let api: ApiResponse<Run> = serde_json::from_slice(&bytes).expect("decode Run");
        let run = api.data.expect("run in response");

        // Browse the run's workspace — the seeded file is listed.
        let resp = app
            .oneshot(
                Request::builder()
                    .uri(format!("/api/orchestrator/runs/{}/workspace", run.id))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .expect("browse request");
        assert_eq!(resp.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .expect("browse body");
        let api: ApiResponse<Vec<WorkspaceEntry>> =
            serde_json::from_slice(&bytes).expect("decode entries");
        let entries = api.data.expect("entries in response");
        assert!(
            entries.iter().any(|e| e.name == "readme.md"),
            "expected readme.md in {entries:?}"
        );
    }

    /// `GET /api/orchestrator/runs/{id}/workspace` still requires the
    /// `CurrentUser` extension — without it the handler cannot run (non-200).
    /// Guards the browse route was not wired as a public route.
    #[tokio::test]
    async fn browse_run_workspace_without_user_is_not_ok() {
        let (state, run_id) = build_state_with_run().await;
        let app = orchestrator_routes(state);
        let resp = app
            .oneshot(
                Request::builder()
                    .uri(format!("/api/orchestrator/runs/{run_id}/workspace"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .expect("request");
        assert_ne!(resp.status(), StatusCode::OK);
    }
}

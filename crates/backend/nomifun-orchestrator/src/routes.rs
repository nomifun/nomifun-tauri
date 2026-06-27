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
use axum::extract::{Extension, Json, Path, State};
use axum::http::StatusCode;
use axum::routing::{get, post, put};

use nomifun_api_types::{
    ApiResponse, CreateAdhocRunRequest, CreateFleetRequest, CreateRunRequest, CreateWorkspaceRequest,
    Fleet, OrchWorkspace, ReassignRequest, Run, RunDetail, SteerRequest, UpdateFleetRequest,
    UpdateWorkspaceRequest,
};
use nomifun_auth::CurrentUser;
use nomifun_common::AppError;

use crate::state::OrchestratorRouterState;

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
        .route("/api/orchestrator/runs/{id}", get(get_run))
        .route("/api/orchestrator/runs/{id}/cancel", post(cancel_run))
        .route("/api/orchestrator/runs/{id}/approve", post(approve_run))
        .route("/api/orchestrator/runs/{id}/pause", post(pause_run))
        .route("/api/orchestrator/runs/{id}/resume", post(resume_run))
        .route(
            "/api/orchestrator/runs/{run_id}/tasks/{task_id}/steer",
            post(steer_task),
        )
        .route(
            "/api/orchestrator/runs/{run_id}/tasks/{task_id}/assignment",
            put(reassign_task),
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
/// **Default autonomy is `interactive`** (the Tab's approval门): unlike the
/// workspace path's `create`, an ad-hoc run launched from the Tab should park at
/// `awaiting_plan_approval` for a human to confirm the plan before any worker
/// dispatches. The default is injected onto the REQUEST here — BEFORE
/// `create_adhoc` persists it — so `plan` (which re-reads the persisted autonomy)
/// applies the gate, and the `engine.start` decision below reads the same value.
/// (The MCP/caps front door, by contrast, defaults to `supervised` — it has no
/// Tab to approve through. See `caps_orchestrator`.)
async fn create_adhoc_run(
    State(state): State<OrchestratorRouterState>,
    Extension(user): Extension<CurrentUser>,
    body: Result<Json<CreateAdhocRunRequest>, JsonRejection>,
) -> Result<(StatusCode, Json<ApiResponse<Run>>), AppError> {
    let Json(mut req) = body.map_err(|e| AppError::BadRequest(e.to_string()))?;
    // Tab default: park at the plan-approval gate unless the form picks another
    // level. Applied to the request so the value is what `create_adhoc` PERSISTS
    // and `plan` re-reads for the autonomy gate (an empty string is treated as
    // absent — same rule as RunService).
    if req.autonomy.as_deref().map(str::trim).unwrap_or("").is_empty() {
        req.autonomy = Some("interactive".to_string());
    }
    let run = state.run_service.create_adhoc(&user.id, req).await?;
    state.run_service.plan(&run.id).await?;
    // `interactive` parks at `awaiting_plan_approval` — do NOT start the engine
    // until the plan is approved. All other autonomy levels start immediately.
    if run.autonomy != "interactive" {
        state.engine.start(run.id.clone());
    }
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

    /// Minimal planner so a RunService can be constructed for the state.
    struct EmptyPlanProducer;
    #[async_trait::async_trait]
    impl PlanProducer for EmptyPlanProducer {
        async fn produce(
            &self,
            _goal: &str,
            _members: &[FleetMember],
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
        let run_repo = Arc::new(SqliteRunRepository::new(pool));
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

        let state = OrchestratorRouterState::new(fleet, workspace, run_service, engine);
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

        // Parse the returned Run and confirm the interactive-default autonomy
        // parked it at `awaiting_plan_approval` after plan (engine NOT started).
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .expect("body bytes");
        let api: ApiResponse<Run> = serde_json::from_slice(&bytes).expect("decode ApiResponse<Run>");
        let run = api.data.expect("run in response");
        assert_eq!(run.autonomy, "interactive", "ad-hoc default autonomy");

        let detail = run_service.get_detail(&run.id).await.expect("detail");
        assert_eq!(
            detail.run.status, "awaiting_plan_approval",
            "interactive ad-hoc run must park at awaiting_plan_approval after plan"
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
    // P3b Task 1 (bug guard): `POST .../resume` must NOT cancel an in-flight
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
            })
        }
    }

    #[tokio::test]
    async fn resume_route_does_not_cancel_in_flight_worker() {
        let db = init_database_memory().await.expect("db init");
        let pool = db.pool().clone();
        let fleet_repo = Arc::new(SqliteFleetRepository::new(pool.clone()));
        let ws_repo = Arc::new(SqliteOrchWorkspaceRepository::new(pool.clone()));
        let run_repo = Arc::new(SqliteRunRepository::new(pool));
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
        let state = OrchestratorRouterState::new(fleet, workspace, run_service.clone(), engine);
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
}

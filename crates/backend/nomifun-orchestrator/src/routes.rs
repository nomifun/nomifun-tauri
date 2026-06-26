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
use axum::routing::get;

use nomifun_api_types::{
    ApiResponse, CreateFleetRequest, CreateWorkspaceRequest, Fleet, OrchWorkspace,
    UpdateFleetRequest, UpdateWorkspaceRequest,
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::{RunEngine, RunEngineDeps};
    use crate::events::OrchestratorRunEventEmitter;
    use crate::plan::PlanProducer;
    use crate::run_service::RunService;
    use crate::service::{FleetService, WorkspaceService};
    use crate::worker::{MockWorkerRunner, WorkerRunner};
    use axum::body::Body;
    use axum::http::Request;
    use nomifun_api_types::{FleetMember, PlannedDag, WebSocketMessage};
    use nomifun_common::AppError;
    use nomifun_db::{
        SqliteFleetRepository, SqliteOrchWorkspaceRepository, SqliteRunRepository,
        init_database_memory,
    };
    use nomifun_realtime::EventBroadcaster;
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
            ws_repo,
            planner,
            emitter.clone(),
        ));
        let worker: Arc<dyn WorkerRunner> = Arc::new(MockWorkerRunner::with_text(1, "x"));
        let engine = RunEngine::new(Arc::new(RunEngineDeps::new(run_repo, worker, emitter)));
        OrchestratorRouterState::new(fleet, workspace, run_service, engine)
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
}

use axum::Router;
use axum::extract::rejection::JsonRejection;
use axum::extract::{Extension, Json, Path, Query, State};
use axum::http::StatusCode;
use axum::routing::{get, post};

use nomifun_api_types::{
    ApiResponse, CreateTerminalRequest, TerminalInputRequest, TerminalResizeRequest, TerminalSessionResponse,
    UpdateTerminalRequest, WorkspaceEntry,
};
use nomifun_auth::CurrentUser;
use nomifun_common::{AppError, TerminalId};
use serde::Deserialize;

use crate::state::TerminalRouterState;

/// Query for `GET /api/terminals/{terminal_id}/workspace`. `path` (workspace-relative,
/// default the cwd root) + optional case-insensitive `search`. The root itself
/// is derived server-side from the session's cwd and is never accepted here.
#[derive(Debug, Deserialize)]
pub struct TerminalWorkspaceQuery {
    #[serde(default)]
    pub path: String,
    pub search: Option<String>,
}

pub fn terminal_routes(state: TerminalRouterState) -> Router {
    Router::new()
        .route("/api/terminals", get(list_terminals).post(create_terminal))
        .route(
            "/api/terminals/{terminal_id}",
            get(get_terminal).patch(update_terminal).delete(delete_terminal),
        )
        .route("/api/terminals/{terminal_id}/input", post(write_input))
        .route("/api/terminals/{terminal_id}/resize", post(resize_terminal))
        .route("/api/terminals/{terminal_id}/kill", post(kill_terminal))
        .route("/api/terminals/{terminal_id}/relaunch", post(relaunch_terminal))
        .route(
            "/api/terminals/{terminal_id}/relaunch-shell",
            post(relaunch_shell_terminal),
        )
        .route(
            "/api/terminals/{terminal_id}/workspace",
            get(browse_workspace),
        )
        .with_state(state)
}

async fn create_terminal(
    State(state): State<TerminalRouterState>,
    Extension(user): Extension<CurrentUser>,
    body: Result<Json<CreateTerminalRequest>, JsonRejection>,
) -> Result<(StatusCode, Json<ApiResponse<TerminalSessionResponse>>), AppError> {
    let Json(req) = body.map_err(|e| AppError::BadRequest(e.to_string()))?;
    let resp = state.terminal_service.create(&user.id, req).await?;
    Ok((StatusCode::CREATED, Json(ApiResponse::ok(resp))))
}

async fn list_terminals(
    State(state): State<TerminalRouterState>,
    Extension(user): Extension<CurrentUser>,
) -> Result<Json<ApiResponse<Vec<TerminalSessionResponse>>>, AppError> {
    let items = state.terminal_service.list(&user.id).await?;
    Ok(Json(ApiResponse::ok(items)))
}

async fn get_terminal(
    State(state): State<TerminalRouterState>,
    Extension(_user): Extension<CurrentUser>,
    Path(terminal_id): Path<TerminalId>,
) -> Result<Json<ApiResponse<TerminalSessionResponse>>, AppError> {
    let resp = state.terminal_service.get(terminal_id.as_str()).await?;
    Ok(Json(ApiResponse::ok(resp)))
}

async fn write_input(
    State(state): State<TerminalRouterState>,
    Extension(_user): Extension<CurrentUser>,
    Path(terminal_id): Path<TerminalId>,
    body: Result<Json<TerminalInputRequest>, JsonRejection>,
) -> Result<Json<ApiResponse<()>>, AppError> {
    let Json(req) = body.map_err(|e| AppError::BadRequest(e.to_string()))?;
    state
        .terminal_service
        .input(terminal_id.as_str(), &req.data_b64)
        .await?;
    Ok(Json(ApiResponse::success()))
}

async fn resize_terminal(
    State(state): State<TerminalRouterState>,
    Extension(_user): Extension<CurrentUser>,
    Path(terminal_id): Path<TerminalId>,
    body: Result<Json<TerminalResizeRequest>, JsonRejection>,
) -> Result<Json<ApiResponse<()>>, AppError> {
    let Json(req) = body.map_err(|e| AppError::BadRequest(e.to_string()))?;
    state
        .terminal_service
        .resize(terminal_id.as_str(), req.cols, req.rows)
        .await?;
    Ok(Json(ApiResponse::success()))
}

async fn kill_terminal(
    State(state): State<TerminalRouterState>,
    Extension(_user): Extension<CurrentUser>,
    Path(terminal_id): Path<TerminalId>,
) -> Result<Json<ApiResponse<()>>, AppError> {
    state.terminal_service.kill(terminal_id.as_str()).await?;
    Ok(Json(ApiResponse::success()))
}

async fn delete_terminal(
    State(state): State<TerminalRouterState>,
    Extension(_user): Extension<CurrentUser>,
    Path(terminal_id): Path<TerminalId>,
) -> Result<Json<ApiResponse<()>>, AppError> {
    state.terminal_service.delete(terminal_id.as_str()).await?;
    Ok(Json(ApiResponse::success()))
}

async fn relaunch_terminal(
    State(state): State<TerminalRouterState>,
    Extension(_user): Extension<CurrentUser>,
    Path(terminal_id): Path<TerminalId>,
) -> Result<Json<ApiResponse<TerminalSessionResponse>>, AppError> {
    let resp = state.terminal_service.relaunch(terminal_id.as_str()).await?;
    Ok(Json(ApiResponse::ok(resp)))
}

/// Fall back to a clean login shell in place: kill the (possibly wedged) agent
/// CLI and spawn the platform shell under the SAME session id. The escape hatch
/// for a garbled/unresponsive claude/codex TUI — see `relaunch_as_shell`.
async fn relaunch_shell_terminal(
    State(state): State<TerminalRouterState>,
    Extension(_user): Extension<CurrentUser>,
    Path(terminal_id): Path<TerminalId>,
) -> Result<Json<ApiResponse<TerminalSessionResponse>>, AppError> {
    let resp = state
        .terminal_service
        .relaunch_as_shell(terminal_id.as_str())
        .await?;
    Ok(Json(ApiResponse::ok(resp)))
}

async fn update_terminal(
    State(state): State<TerminalRouterState>,
    Extension(_user): Extension<CurrentUser>,
    Path(terminal_id): Path<TerminalId>,
    body: Result<Json<UpdateTerminalRequest>, JsonRejection>,
) -> Result<Json<ApiResponse<TerminalSessionResponse>>, AppError> {
    let Json(req) = body.map_err(|e| AppError::BadRequest(e.to_string()))?;
    let resp = state
        .terminal_service
        .update_meta(terminal_id.as_str(), req.name, req.pinned)
        .await?;
    Ok(Json(ApiResponse::ok(resp)))
}

/// List one directory level under the terminal session's working directory.
/// The root is the session's `cwd` (server-authoritative); the client supplies
/// only a workspace-relative `path` + optional `search`. Missing session → 404,
/// `..` traversal → 400 (both from the service / `list_workspace_level`).
async fn browse_workspace(
    State(state): State<TerminalRouterState>,
    Extension(_user): Extension<CurrentUser>,
    Path(terminal_id): Path<TerminalId>,
    Query(query): Query<TerminalWorkspaceQuery>,
) -> Result<Json<ApiResponse<Vec<WorkspaceEntry>>>, AppError> {
    let entries = state
        .terminal_service
        .browse_workspace(
            terminal_id.as_str(),
            &query.path,
            query.search.as_deref(),
        )
        .await?;
    Ok(Json(ApiResponse::ok(entries)))
}

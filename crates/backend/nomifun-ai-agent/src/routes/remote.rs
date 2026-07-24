//! Remote agent management API routes.
//!
//! Endpoints:
//!
//! - `GET  /api/remote-agents`                    — list remote agents
//! - `POST /api/remote-agents`                    — create new remote agent
//! - `GET  /api/remote-agents/{remote_agent_id}`       — get remote agent details
//! - `PUT  /api/remote-agents/{remote_agent_id}`       — update remote agent
//! - `DELETE /api/remote-agents/{remote_agent_id}`     — delete remote agent
//! - `POST /api/remote-agents/test-connection`          — test connection to remote agent (without saving it)
//! - `POST /api/remote-agents/{remote_agent_id}/handshake` — perform handshake with the remote agent to verify connectivity and retrieve agent info

use axum::Router;
use axum::extract::rejection::JsonRejection;
use axum::extract::{Extension, Json, Path, State};
use axum::http::StatusCode;
use axum::routing::{get, post};

use nomifun_api_types::{
    ApiResponse, CreateRemoteAgentRequest, HandshakeResponse, RemoteAgentListItem, RemoteAgentResponse,
    TestRemoteAgentConnectionRequest, UpdateRemoteAgentRequest,
};
use nomifun_auth::CurrentUser;
use nomifun_common::{AppError, RemoteAgentId};

use super::state::RemoteAgentRouterState;

/// Build the remote agent router.
///
/// All routes require authentication (applied by the caller).
pub fn remote_agent_routes(state: RemoteAgentRouterState) -> Router {
    Router::new()
        .route("/api/remote-agents", get(list).post(create))
        .route("/api/remote-agents/test-connection", post(test_connection))
        .route(
            "/api/remote-agents/{remote_agent_id}",
            get(get_one).put(update).delete(delete_one),
        )
        .route(
            "/api/remote-agents/{remote_agent_id}/handshake",
            post(handshake),
        )
        .with_state(state)
}

async fn list(
    State(state): State<RemoteAgentRouterState>,
    Extension(_user): Extension<CurrentUser>,
) -> Result<Json<ApiResponse<Vec<RemoteAgentListItem>>>, AppError> {
    let items = state.service.list().await?;
    Ok(Json(ApiResponse::ok(items)))
}

async fn get_one(
    State(state): State<RemoteAgentRouterState>,
    Extension(_user): Extension<CurrentUser>,
    Path(remote_agent_id): Path<RemoteAgentId>,
) -> Result<Json<ApiResponse<RemoteAgentResponse>>, AppError> {
    let agent = state.service.get(&remote_agent_id).await?;
    Ok(Json(ApiResponse::ok(agent)))
}

async fn create(
    State(state): State<RemoteAgentRouterState>,
    Extension(_user): Extension<CurrentUser>,
    body: Result<Json<CreateRemoteAgentRequest>, JsonRejection>,
) -> Result<(StatusCode, Json<ApiResponse<RemoteAgentResponse>>), AppError> {
    let Json(req) = body.map_err(|e| AppError::BadRequest(e.to_string()))?;
    let agent = state.service.create(req).await?;
    Ok((StatusCode::CREATED, Json(ApiResponse::ok(agent))))
}

async fn update(
    State(state): State<RemoteAgentRouterState>,
    Extension(_user): Extension<CurrentUser>,
    Path(remote_agent_id): Path<RemoteAgentId>,
    body: Result<Json<UpdateRemoteAgentRequest>, JsonRejection>,
) -> Result<Json<ApiResponse<RemoteAgentResponse>>, AppError> {
    let Json(req) = body.map_err(|e| AppError::BadRequest(e.to_string()))?;
    let agent = state.service.update(&remote_agent_id, req).await?;
    Ok(Json(ApiResponse::ok(agent)))
}

async fn delete_one(
    State(state): State<RemoteAgentRouterState>,
    Extension(_user): Extension<CurrentUser>,
    Path(remote_agent_id): Path<RemoteAgentId>,
) -> Result<Json<ApiResponse<()>>, AppError> {
    state.service.delete(&remote_agent_id).await?;
    Ok(Json(ApiResponse::success()))
}

async fn test_connection(
    State(state): State<RemoteAgentRouterState>,
    Extension(_user): Extension<CurrentUser>,
    body: Result<Json<TestRemoteAgentConnectionRequest>, JsonRejection>,
) -> Result<Json<ApiResponse<()>>, AppError> {
    let Json(req) = body.map_err(|e| AppError::BadRequest(e.to_string()))?;
    state.service.test_connection(req).await?;
    Ok(Json(ApiResponse::success()))
}

async fn handshake(
    State(state): State<RemoteAgentRouterState>,
    Extension(_user): Extension<CurrentUser>,
    Path(remote_agent_id): Path<RemoteAgentId>,
) -> Result<Json<ApiResponse<HandshakeResponse>>, AppError> {
    let resp = state.service.handshake(&remote_agent_id).await?;
    Ok(Json(ApiResponse::ok(resp)))
}

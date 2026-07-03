//! `/api/public-agents/*` route handlers for the 对外伙伴 (public companion)
//! domain. Owner-only (behind the app's authenticated router) — this domain has
//! NO gateway / external write path, so every field is safely owner-editable.

use axum::Router;
use axum::extract::rejection::JsonRejection;
use axum::extract::{Extension, Json, Path, Query, State};
use axum::http::StatusCode;
use axum::routing::get;

use nomifun_api_types::ApiResponse;
use nomifun_auth::CurrentUser;
use nomifun_common::AppError;
use serde::Deserialize;

use crate::audit::{AuditPage, AuditQuery};
use crate::config::PublicAgentConfig;
use crate::state::PublicAgentRouterState;

pub fn public_agent_routes(state: PublicAgentRouterState) -> Router {
    Router::new()
        .route("/api/public-agents", get(list_agents).post(create_agent))
        .route(
            "/api/public-agents/{id}",
            get(get_agent).patch(patch_agent).delete(delete_agent),
        )
        .route(
            "/api/public-agents/{id}/audit",
            get(get_audit).delete(delete_audit),
        )
        .with_state(state)
}

async fn list_agents(
    State(state): State<PublicAgentRouterState>,
    Extension(_user): Extension<CurrentUser>,
) -> Result<Json<ApiResponse<Vec<PublicAgentConfig>>>, AppError> {
    Ok(Json(ApiResponse::ok(state.service.list().await)))
}

#[derive(Deserialize)]
struct CreateAgentRequest {
    name: String,
}

async fn create_agent(
    State(state): State<PublicAgentRouterState>,
    Extension(_user): Extension<CurrentUser>,
    body: Result<Json<CreateAgentRequest>, JsonRejection>,
) -> Result<impl axum::response::IntoResponse, AppError> {
    let Json(req) = body.map_err(|e| AppError::BadRequest(e.to_string()))?;
    let agent = state.service.create(&req.name).await?;
    Ok((StatusCode::CREATED, Json(ApiResponse::ok(agent))))
}

async fn get_agent(
    State(state): State<PublicAgentRouterState>,
    Extension(_user): Extension<CurrentUser>,
    Path(id): Path<String>,
) -> Result<Json<ApiResponse<PublicAgentConfig>>, AppError> {
    Ok(Json(ApiResponse::ok(state.service.get(&id).await?)))
}

/// RFC 7396 merge-patch over any editable field (name/greeting/tone/model/
/// knowledge_base_ids/grounded_mode/service_policy/audit_retention_days/enabled).
async fn patch_agent(
    State(state): State<PublicAgentRouterState>,
    Extension(_user): Extension<CurrentUser>,
    Path(id): Path<String>,
    body: Result<Json<serde_json::Value>, JsonRejection>,
) -> Result<Json<ApiResponse<PublicAgentConfig>>, AppError> {
    let Json(patch) = body.map_err(|e| AppError::BadRequest(e.to_string()))?;
    Ok(Json(ApiResponse::ok(state.service.patch(&id, patch).await?)))
}

async fn delete_agent(
    State(state): State<PublicAgentRouterState>,
    Extension(_user): Extension<CurrentUser>,
    Path(id): Path<String>,
) -> Result<StatusCode, AppError> {
    state.service.delete(&id).await?;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize)]
struct AuditQueryParams {
    limit: Option<usize>,
    cursor: Option<i64>,
    q: Option<String>,
    kind: Option<String>,
    days: Option<u32>,
}

async fn get_audit(
    State(state): State<PublicAgentRouterState>,
    Extension(_user): Extension<CurrentUser>,
    Path(id): Path<String>,
    Query(params): Query<AuditQueryParams>,
) -> Result<Json<ApiResponse<AuditPage>>, AppError> {
    let query = AuditQuery {
        limit: params.limit.unwrap_or(50).clamp(1, 200),
        cursor: params.cursor,
        q: params.q.filter(|s| !s.trim().is_empty()),
        kind: params.kind.filter(|s| !s.trim().is_empty()),
        days: params.days,
    };
    Ok(Json(ApiResponse::ok(state.service.search_audit(&id, query).await?)))
}

#[derive(Deserialize)]
struct DeleteAuditParams {
    older_than_days: Option<u32>,
}

#[derive(serde::Serialize)]
struct DeleteAuditResult {
    deleted_days: usize,
}

async fn delete_audit(
    State(state): State<PublicAgentRouterState>,
    Extension(_user): Extension<CurrentUser>,
    Path(id): Path<String>,
    Query(params): Query<DeleteAuditParams>,
) -> Result<Json<ApiResponse<DeleteAuditResult>>, AppError> {
    let deleted = state
        .service
        .delete_audit(&id, params.older_than_days.unwrap_or(0))
        .await?;
    Ok(Json(ApiResponse::ok(DeleteAuditResult { deleted_days: deleted })))
}

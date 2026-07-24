//! Owner-scoped HTTP management for reusable collaboration inputs.

use std::sync::Arc;

use axum::extract::rejection::JsonRejection;
use axum::extract::{Extension, Json, Path, Query, State};
use axum::http::StatusCode;
use axum::routing::{get, post};
use axum::Router;
use nomifun_api_types::{
    AgentExecution, AgentExecutionTemplate, AgentExecutionTemplateDetail, ApiResponse,
    CreateAgentExecutionTemplateRequest, CreateExecutionFromTemplateRequest,
    UpdateAgentExecutionTemplateRequest,
};
use nomifun_auth::CurrentUser;
use nomifun_common::{AgentExecutionActor, AgentExecutionTemplateId, AppError};
use serde::Deserialize;

use crate::AgentExecutionEngine;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ListQuery {
    limit: Option<i64>,
    offset: Option<i64>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct DeleteQuery {
    expected_version: i64,
}

pub fn agent_execution_template_routes(engine: Arc<AgentExecutionEngine>) -> Router {
    Router::new()
        .route(
            "/api/agent-execution-templates",
            get(list_templates).post(create_template),
        )
        .route(
            "/api/agent-execution-templates/{execution_template_id}",
            get(get_template).put(update_template).delete(delete_template),
        )
        .route(
            "/api/agent-execution-templates/{execution_template_id}/create-execution",
            post(create_execution_from_template),
        )
        .with_state(engine)
}

async fn list_templates(
    State(engine): State<Arc<AgentExecutionEngine>>,
    Extension(user): Extension<CurrentUser>,
    Query(query): Query<ListQuery>,
) -> Result<Json<ApiResponse<Vec<AgentExecutionTemplate>>>, AppError> {
    Ok(Json(ApiResponse::ok(
        engine
            .list_templates(&user.id, query.limit, query.offset)
            .await?,
    )))
}

async fn get_template(
    State(engine): State<Arc<AgentExecutionEngine>>,
    Extension(user): Extension<CurrentUser>,
    Path(execution_template_id): Path<AgentExecutionTemplateId>,
) -> Result<Json<ApiResponse<AgentExecutionTemplateDetail>>, AppError> {
    Ok(Json(ApiResponse::ok(
        engine.get_template(&user.id, execution_template_id.as_str()).await?,
    )))
}

async fn create_template(
    State(engine): State<Arc<AgentExecutionEngine>>,
    Extension(user): Extension<CurrentUser>,
    body: Result<Json<CreateAgentExecutionTemplateRequest>, JsonRejection>,
) -> Result<(StatusCode, Json<ApiResponse<AgentExecutionTemplateDetail>>), AppError> {
    let Json(request) = json_body(body)?;
    let template = engine.create_template(&user.id, request).await?;
    Ok((StatusCode::CREATED, Json(ApiResponse::ok(template))))
}

async fn update_template(
    State(engine): State<Arc<AgentExecutionEngine>>,
    Extension(user): Extension<CurrentUser>,
    Path(execution_template_id): Path<AgentExecutionTemplateId>,
    body: Result<Json<UpdateAgentExecutionTemplateRequest>, JsonRejection>,
) -> Result<Json<ApiResponse<AgentExecutionTemplateDetail>>, AppError> {
    let Json(request) = json_body(body)?;
    Ok(Json(ApiResponse::ok(
        engine
            .update_template(&user.id, execution_template_id.as_str(), request)
            .await?,
    )))
}

async fn delete_template(
    State(engine): State<Arc<AgentExecutionEngine>>,
    Extension(user): Extension<CurrentUser>,
    Path(execution_template_id): Path<AgentExecutionTemplateId>,
    Query(query): Query<DeleteQuery>,
) -> Result<Json<ApiResponse<()>>, AppError> {
    engine
        .delete_template(&user.id, execution_template_id.as_str(), query.expected_version)
        .await?;
    Ok(Json(ApiResponse::success()))
}

async fn create_execution_from_template(
    State(engine): State<Arc<AgentExecutionEngine>>,
    Extension(user): Extension<CurrentUser>,
    Path(execution_template_id): Path<AgentExecutionTemplateId>,
    body: Result<Json<CreateExecutionFromTemplateRequest>, JsonRejection>,
) -> Result<(StatusCode, Json<ApiResponse<AgentExecution>>), AppError> {
    let Json(request) = json_body(body)?;
    let actor = AgentExecutionActor::user(user.id.clone());
    let execution = engine
        .create_from_template(&user.id, &actor, execution_template_id.as_str(), request)
        .await?;
    Ok((StatusCode::CREATED, Json(ApiResponse::ok(execution))))
}

fn json_body<T>(body: Result<Json<T>, JsonRejection>) -> Result<Json<T>, AppError> {
    body.map_err(|error| AppError::BadRequest(error.body_text()))
}

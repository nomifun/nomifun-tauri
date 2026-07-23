//! Authenticated HTTP projection of [`AgentExecutionEngine`](crate::AgentExecutionEngine).

use std::sync::Arc;

use axum::extract::rejection::JsonRejection;
use axum::extract::{Extension, Json, Path, Query, State};
use axum::http::StatusCode;
use axum::routing::{get, patch, post, put};
use axum::Router;
use nomifun_api_types::{
    AddExecutionStepsRequest, AdoptExecutionStepOutputRequest, AdjustAgentExecutionRequest,
    AgentExecution, AgentExecutionDetail, AgentExecutionEvent, AgentExecutionEventsQuery,
    AnswerExecutionDecisionRequest, ApiResponse, ConfigureExecutionStepRequest,
    CreateAgentExecutionRequest, ExecutionStep, ReassignExecutionStepRequest,
    RenameAgentExecutionRequest, ReplanAgentExecutionRequest, RetryExecutionStepRequest,
    SteerExecutionStepRequest, UpdateExecutionStepRequest, VersionedAgentExecutionCommand,
    WorkspaceEntry,
};
use nomifun_auth::CurrentUser;
use nomifun_common::{
    AgentExecutionActor, AgentExecutionAttemptId, AgentExecutionId, AgentExecutionStepId,
    AppError,
};
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
struct WorkspaceQuery {
    #[serde(default)]
    path: String,
    search: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct DeleteQuery {
    expected_version: i64,
}

pub fn agent_execution_routes(engine: Arc<AgentExecutionEngine>) -> Router {
    Router::new()
        .route(
            "/api/agent-executions",
            get(list_executions).post(create_execution),
        )
        .route(
            "/api/agent-executions/{execution_id}",
            get(get_execution).delete(delete_execution),
        )
        .route("/api/agent-executions/{execution_id}/rename", patch(rename_execution))
        .route("/api/agent-executions/{execution_id}/replan", post(replan_execution))
        .route("/api/agent-executions/{execution_id}/adjust", post(adjust_execution))
        .route("/api/agent-executions/{execution_id}/approve", post(approve_execution))
        .route("/api/agent-executions/{execution_id}/pause", post(pause_execution))
        .route("/api/agent-executions/{execution_id}/resume", post(resume_execution))
        .route("/api/agent-executions/{execution_id}/cancel", post(cancel_execution))
        .route(
            "/api/agent-executions/{execution_id}/steps",
            post(add_execution_steps),
        )
        .route(
            "/api/agent-executions/{execution_id}/steps/{step_id}",
            patch(update_execution_step),
        )
        .route(
            "/api/agent-executions/{execution_id}/steps/{step_id}/reassign",
            put(reassign_execution_step),
        )
        .route(
            "/api/agent-executions/{execution_id}/steps/{step_id}/configure",
            patch(configure_execution_step),
        )
        .route(
            "/api/agent-executions/{execution_id}/steps/{step_id}/retry",
            post(retry_execution_step),
        )
        .route(
            "/api/agent-executions/{execution_id}/steps/{step_id}/adopt",
            post(adopt_execution_step_output),
        )
        .route(
            "/api/agent-executions/{execution_id}/steps/{step_id}/steer",
            post(steer_execution_step),
        )
        .route(
            "/api/agent-executions/{execution_id}/steps/{step_id}/attempts/{attempt_id}/answer",
            post(answer_execution_decision),
        )
        .route("/api/agent-executions/{execution_id}/events", get(list_execution_events))
        .route(
            "/api/agent-executions/{execution_id}/workspace",
            get(browse_execution_workspace),
        )
        .with_state(engine)
}

async fn list_executions(
    State(engine): State<Arc<AgentExecutionEngine>>,
    Extension(user): Extension<CurrentUser>,
    Query(query): Query<ListQuery>,
) -> Result<Json<ApiResponse<Vec<AgentExecution>>>, AppError> {
    Ok(Json(ApiResponse::ok(
        engine.list(&user.id, query.limit, query.offset).await?,
    )))
}

async fn create_execution(
    State(engine): State<Arc<AgentExecutionEngine>>,
    Extension(user): Extension<CurrentUser>,
    body: Result<Json<CreateAgentExecutionRequest>, JsonRejection>,
) -> Result<(StatusCode, Json<ApiResponse<AgentExecution>>), AppError> {
    let Json(request) = json_body(body)?;
    let actor = user_actor(&user);
    let execution = engine.create(&user.id, &actor, request).await?;
    Ok((StatusCode::CREATED, Json(ApiResponse::ok(execution))))
}

async fn get_execution(
    State(engine): State<Arc<AgentExecutionEngine>>,
    Extension(user): Extension<CurrentUser>,
    Path(execution_id): Path<AgentExecutionId>,
) -> Result<Json<ApiResponse<AgentExecutionDetail>>, AppError> {
    Ok(Json(ApiResponse::ok(
        engine.get(&user.id, execution_id.as_str()).await?,
    )))
}

async fn delete_execution(
    State(engine): State<Arc<AgentExecutionEngine>>,
    Extension(user): Extension<CurrentUser>,
    Path(execution_id): Path<AgentExecutionId>,
    Query(query): Query<DeleteQuery>,
) -> Result<Json<ApiResponse<()>>, AppError> {
    engine
        .delete(
            &user.id,
            &user_actor(&user),
            execution_id.as_str(),
            VersionedAgentExecutionCommand {
                expected_version: query.expected_version,
            },
        )
        .await?;
    Ok(Json(ApiResponse::success()))
}

async fn rename_execution(
    State(engine): State<Arc<AgentExecutionEngine>>,
    Extension(user): Extension<CurrentUser>,
    Path(execution_id): Path<AgentExecutionId>,
    body: Result<Json<RenameAgentExecutionRequest>, JsonRejection>,
) -> Result<Json<ApiResponse<AgentExecution>>, AppError> {
    let Json(request) = json_body(body)?;
    Ok(Json(ApiResponse::ok(
        engine
            .rename(&user.id, &user_actor(&user), execution_id.as_str(), request)
            .await?,
    )))
}

async fn replan_execution(
    State(engine): State<Arc<AgentExecutionEngine>>,
    Extension(user): Extension<CurrentUser>,
    Path(execution_id): Path<AgentExecutionId>,
    body: Result<Json<ReplanAgentExecutionRequest>, JsonRejection>,
) -> Result<Json<ApiResponse<AgentExecutionDetail>>, AppError> {
    let Json(request) = json_body(body)?;
    Ok(Json(ApiResponse::ok(
        engine
            .replan(&user.id, &user_actor(&user), execution_id.as_str(), request)
            .await?,
    )))
}

async fn adjust_execution(
    State(engine): State<Arc<AgentExecutionEngine>>,
    Extension(user): Extension<CurrentUser>,
    Path(execution_id): Path<AgentExecutionId>,
    body: Result<Json<AdjustAgentExecutionRequest>, JsonRejection>,
) -> Result<Json<ApiResponse<AgentExecutionDetail>>, AppError> {
    let Json(request) = json_body(body)?;
    Ok(Json(ApiResponse::ok(
        engine
            .adjust(&user.id, &user_actor(&user), execution_id.as_str(), request)
            .await?,
    )))
}

async fn approve_execution(
    State(engine): State<Arc<AgentExecutionEngine>>,
    Extension(user): Extension<CurrentUser>,
    Path(execution_id): Path<AgentExecutionId>,
    body: Result<Json<VersionedAgentExecutionCommand>, JsonRejection>,
) -> Result<Json<ApiResponse<AgentExecution>>, AppError> {
    let Json(command) = json_body(body)?;
    Ok(Json(ApiResponse::ok(
        engine
            .approve(&user.id, &user_actor(&user), execution_id.as_str(), command)
            .await?,
    )))
}

async fn pause_execution(
    State(engine): State<Arc<AgentExecutionEngine>>,
    Extension(user): Extension<CurrentUser>,
    Path(execution_id): Path<AgentExecutionId>,
    body: Result<Json<VersionedAgentExecutionCommand>, JsonRejection>,
) -> Result<Json<ApiResponse<AgentExecution>>, AppError> {
    let Json(command) = json_body(body)?;
    Ok(Json(ApiResponse::ok(
        engine
            .pause(&user.id, &user_actor(&user), execution_id.as_str(), command)
            .await?,
    )))
}

async fn resume_execution(
    State(engine): State<Arc<AgentExecutionEngine>>,
    Extension(user): Extension<CurrentUser>,
    Path(execution_id): Path<AgentExecutionId>,
    body: Result<Json<VersionedAgentExecutionCommand>, JsonRejection>,
) -> Result<Json<ApiResponse<AgentExecution>>, AppError> {
    let Json(command) = json_body(body)?;
    Ok(Json(ApiResponse::ok(
        engine
            .resume(&user.id, &user_actor(&user), execution_id.as_str(), command)
            .await?,
    )))
}

async fn cancel_execution(
    State(engine): State<Arc<AgentExecutionEngine>>,
    Extension(user): Extension<CurrentUser>,
    Path(execution_id): Path<AgentExecutionId>,
    body: Result<Json<VersionedAgentExecutionCommand>, JsonRejection>,
) -> Result<Json<ApiResponse<AgentExecutionDetail>>, AppError> {
    let Json(command) = json_body(body)?;
    Ok(Json(ApiResponse::ok(
        engine
            .cancel(&user.id, &user_actor(&user), execution_id.as_str(), command)
            .await?,
    )))
}

async fn add_execution_steps(
    State(engine): State<Arc<AgentExecutionEngine>>,
    Extension(user): Extension<CurrentUser>,
    Path(execution_id): Path<AgentExecutionId>,
    body: Result<Json<AddExecutionStepsRequest>, JsonRejection>,
) -> Result<Json<ApiResponse<AgentExecutionDetail>>, AppError> {
    let Json(request) = json_body(body)?;
    Ok(Json(ApiResponse::ok(
        engine
            .add_steps(&user.id, &user_actor(&user), execution_id.as_str(), request)
            .await?,
    )))
}

async fn update_execution_step(
    State(engine): State<Arc<AgentExecutionEngine>>,
    Extension(user): Extension<CurrentUser>,
    Path((execution_id, step_id)): Path<(AgentExecutionId, AgentExecutionStepId)>,
    body: Result<Json<UpdateExecutionStepRequest>, JsonRejection>,
) -> Result<Json<ApiResponse<ExecutionStep>>, AppError> {
    let Json(request) = json_body(body)?;
    Ok(Json(ApiResponse::ok(
        engine
            .update_step(
                &user.id,
                &user_actor(&user),
                execution_id.as_str(),
                step_id.as_str(),
                request,
            )
            .await?,
    )))
}

async fn reassign_execution_step(
    State(engine): State<Arc<AgentExecutionEngine>>,
    Extension(user): Extension<CurrentUser>,
    Path((execution_id, step_id)): Path<(AgentExecutionId, AgentExecutionStepId)>,
    body: Result<Json<ReassignExecutionStepRequest>, JsonRejection>,
) -> Result<Json<ApiResponse<ExecutionStep>>, AppError> {
    let Json(request) = json_body(body)?;
    Ok(Json(ApiResponse::ok(
        engine
            .reassign_step(
                &user.id,
                &user_actor(&user),
                execution_id.as_str(),
                step_id.as_str(),
                request,
            )
            .await?,
    )))
}

async fn configure_execution_step(
    State(engine): State<Arc<AgentExecutionEngine>>,
    Extension(user): Extension<CurrentUser>,
    Path((execution_id, step_id)): Path<(AgentExecutionId, AgentExecutionStepId)>,
    body: Result<Json<ConfigureExecutionStepRequest>, JsonRejection>,
) -> Result<Json<ApiResponse<ExecutionStep>>, AppError> {
    let Json(request) = json_body(body)?;
    Ok(Json(ApiResponse::ok(
        engine
            .configure_step(
                &user.id,
                &user_actor(&user),
                execution_id.as_str(),
                step_id.as_str(),
                request,
            )
            .await?,
    )))
}

async fn retry_execution_step(
    State(engine): State<Arc<AgentExecutionEngine>>,
    Extension(user): Extension<CurrentUser>,
    Path((execution_id, step_id)): Path<(AgentExecutionId, AgentExecutionStepId)>,
    body: Result<Json<RetryExecutionStepRequest>, JsonRejection>,
) -> Result<Json<ApiResponse<AgentExecutionDetail>>, AppError> {
    let Json(request) = json_body(body)?;
    Ok(Json(ApiResponse::ok(
        engine
            .retry_step(
                &user.id,
                &user_actor(&user),
                execution_id.as_str(),
                step_id.as_str(),
                request,
            )
            .await?,
    )))
}

async fn adopt_execution_step_output(
    State(engine): State<Arc<AgentExecutionEngine>>,
    Extension(user): Extension<CurrentUser>,
    Path((execution_id, step_id)): Path<(AgentExecutionId, AgentExecutionStepId)>,
    body: Result<Json<AdoptExecutionStepOutputRequest>, JsonRejection>,
) -> Result<Json<ApiResponse<AgentExecutionDetail>>, AppError> {
    let Json(request) = json_body(body)?;
    Ok(Json(ApiResponse::ok(
        engine
            .adopt_step_output(
                &user.id,
                &user_actor(&user),
                execution_id.as_str(),
                step_id.as_str(),
                request,
            )
            .await?,
    )))
}

async fn steer_execution_step(
    State(engine): State<Arc<AgentExecutionEngine>>,
    Extension(user): Extension<CurrentUser>,
    Path((execution_id, step_id)): Path<(AgentExecutionId, AgentExecutionStepId)>,
    body: Result<Json<SteerExecutionStepRequest>, JsonRejection>,
) -> Result<Json<ApiResponse<()>>, AppError> {
    let Json(request) = json_body(body)?;
    engine
        .steer_step(
            &user.id,
            &user_actor(&user),
            execution_id.as_str(),
            step_id.as_str(),
            request,
        )
        .await?;
    Ok(Json(ApiResponse::success()))
}

async fn answer_execution_decision(
    State(engine): State<Arc<AgentExecutionEngine>>,
    Extension(user): Extension<CurrentUser>,
    Path((execution_id, step_id, attempt_id)): Path<(
        AgentExecutionId,
        AgentExecutionStepId,
        AgentExecutionAttemptId,
    )>,
    body: Result<Json<AnswerExecutionDecisionRequest>, JsonRejection>,
) -> Result<Json<ApiResponse<AgentExecutionDetail>>, AppError> {
    let Json(request) = json_body(body)?;
    Ok(Json(ApiResponse::ok(
        engine
            .answer_decision(
                &user.id,
                &user_actor(&user),
                execution_id.as_str(),
                step_id.as_str(),
                attempt_id.as_str(),
                request,
            )
            .await?,
    )))
}

async fn list_execution_events(
    State(engine): State<Arc<AgentExecutionEngine>>,
    Extension(user): Extension<CurrentUser>,
    Path(execution_id): Path<AgentExecutionId>,
    Query(query): Query<AgentExecutionEventsQuery>,
) -> Result<Json<ApiResponse<Vec<AgentExecutionEvent>>>, AppError> {
    Ok(Json(ApiResponse::ok(
        engine
            .events(&user.id, execution_id.as_str(), query.after_sequence, query.limit)
            .await?,
    )))
}

async fn browse_execution_workspace(
    State(engine): State<Arc<AgentExecutionEngine>>,
    Extension(user): Extension<CurrentUser>,
    Path(execution_id): Path<AgentExecutionId>,
    Query(query): Query<WorkspaceQuery>,
) -> Result<Json<ApiResponse<Vec<WorkspaceEntry>>>, AppError> {
    Ok(Json(ApiResponse::ok(
        engine
            .browse_workspace(
                &user.id,
                execution_id.as_str(),
                &query.path,
                query.search.as_deref(),
            )
            .await?,
    )))
}

fn json_body<T>(body: Result<Json<T>, JsonRejection>) -> Result<Json<T>, AppError> {
    body.map_err(|error| AppError::BadRequest(error.to_string()))
}

fn user_actor(user: &CurrentUser) -> AgentExecutionActor {
    AgentExecutionActor::user(user.id.clone())
}

use axum::Router;
use axum::extract::rejection::JsonRejection;
use axum::extract::{Extension, Json, Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::routing::{get, post};

use nomifun_api_types::{
    ApiResponse, ConversationResponse, CreateCronJobRequest, CronJobResponse, CronJobRunResponse,
    HasSkillResponse, ListCronJobsQuery, RunNowResponse, SaveCronSkillRequest,
    UpdateCronJobRequest,
};
use nomifun_auth::CurrentUser;
use nomifun_common::AppError;

use crate::service::CronService;
use crate::state::CronRouterState;

pub fn cron_routes(state: CronRouterState) -> Router {
    Router::new()
        .route("/api/cron/jobs", get(list_jobs).post(create_job))
        .route(
            "/api/cron/jobs/{cron_job_id}",
            get(get_job).put(update_job).delete(delete_job),
        )
        .route("/api/cron/jobs/{cron_job_id}/run", post(run_now))
        .route("/api/cron/jobs/{cron_job_id}/runs", get(list_runs_by_cron_job))
        .route("/api/cron/internal/system-resume", post(system_resume))
        .route(
            "/api/cron/jobs/{cron_job_id}/conversations",
            get(list_conversations_by_cron_job),
        )
        .route(
            "/api/cron/jobs/{cron_job_id}/skill",
            get(has_skill).post(save_skill).delete(delete_skill),
        )
        .with_state(state)
}

async fn create_job(
    State(state): State<CronRouterState>,
    Extension(user): Extension<CurrentUser>,
    body: Result<Json<CreateCronJobRequest>, JsonRejection>,
) -> Result<(StatusCode, Json<ApiResponse<CronJobResponse>>), AppError> {
    let Json(req) = body.map_err(|e| AppError::BadRequest(e.to_string()))?;
    let job = state.cron_service.add_job(&user.id, req).await?;
    let resp = CronService::to_response(&job);
    Ok((StatusCode::CREATED, Json(ApiResponse::ok(resp))))
}

async fn list_jobs(
    State(state): State<CronRouterState>,
    Extension(user): Extension<CurrentUser>,
    Query(query): Query<ListCronJobsQuery>,
) -> Result<Json<ApiResponse<Vec<CronJobResponse>>>, AppError> {
    let jobs = state.cron_service.list_jobs(&user.id, &query).await?;
    let items: Vec<CronJobResponse> = jobs.iter().map(CronService::to_response).collect();
    Ok(Json(ApiResponse::ok(items)))
}

async fn get_job(
    State(state): State<CronRouterState>,
    Extension(user): Extension<CurrentUser>,
    Path(cron_job_id): Path<String>,
) -> Result<Json<ApiResponse<CronJobResponse>>, AppError> {
    let job = state.cron_service.get_job(&user.id, &cron_job_id).await?;
    Ok(Json(ApiResponse::ok(CronService::to_response(&job))))
}

async fn update_job(
    State(state): State<CronRouterState>,
    Extension(user): Extension<CurrentUser>,
    Path(cron_job_id): Path<String>,
    body: Result<Json<UpdateCronJobRequest>, JsonRejection>,
) -> Result<Json<ApiResponse<CronJobResponse>>, AppError> {
    let Json(req) = body.map_err(|e| AppError::BadRequest(e.to_string()))?;
    let job = state
        .cron_service
        .update_job(&user.id, &cron_job_id, req)
        .await?;
    Ok(Json(ApiResponse::ok(CronService::to_response(&job))))
}

async fn delete_job(
    State(state): State<CronRouterState>,
    Extension(user): Extension<CurrentUser>,
    Path(cron_job_id): Path<String>,
) -> Result<Json<ApiResponse<()>>, AppError> {
    state
        .cron_service
        .remove_job(&user.id, &cron_job_id)
        .await?;
    Ok(Json(ApiResponse::success()))
}

async fn run_now(
    State(state): State<CronRouterState>,
    Extension(user): Extension<CurrentUser>,
    Path(cron_job_id): Path<String>,
) -> Result<Json<ApiResponse<RunNowResponse>>, AppError> {
    let resp = state.cron_service.run_now(&user.id, &cron_job_id).await?;
    Ok(Json(ApiResponse::ok(resp)))
}

async fn system_resume(
    State(state): State<CronRouterState>,
    headers: HeaderMap,
) -> Result<Json<ApiResponse<()>>, AppError> {
    let is_internal = headers
        .get("x-nomifun-internal")
        .and_then(|value| value.to_str().ok())
        == Some("1");
    if !is_internal {
        return Err(AppError::Forbidden("internal route".into()));
    }

    state.cron_service.handle_system_resume().await;
    Ok(Json(ApiResponse::success()))
}

async fn save_skill(
    State(state): State<CronRouterState>,
    Extension(user): Extension<CurrentUser>,
    Path(cron_job_id): Path<String>,
    body: Result<Json<SaveCronSkillRequest>, JsonRejection>,
) -> Result<Json<ApiResponse<()>>, AppError> {
    let Json(req) = body.map_err(|e| AppError::BadRequest(e.to_string()))?;
    state
        .cron_service
        .save_skill(&user.id, &cron_job_id, req)
        .await?;
    Ok(Json(ApiResponse::success()))
}

async fn list_conversations_by_cron_job(
    State(state): State<CronRouterState>,
    Extension(user): Extension<CurrentUser>,
    Path(cron_job_id): Path<String>,
) -> Result<Json<ApiResponse<Vec<ConversationResponse>>>, AppError> {
    state
        .cron_service
        .get_job(&user.id, &cron_job_id)
        .await?;
    let items = state
        .conversation_service
        .list_by_cron_job(&user.id, &cron_job_id)
        .await?;
    Ok(Json(ApiResponse::ok(items)))
}

async fn list_runs_by_cron_job(
    State(state): State<CronRouterState>,
    Extension(user): Extension<CurrentUser>,
    Path(cron_job_id): Path<String>,
) -> Result<Json<ApiResponse<Vec<CronJobRunResponse>>>, AppError> {
    let items = state
        .cron_service
        .list_runs(&user.id, &cron_job_id)
        .await?;
    Ok(Json(ApiResponse::ok(items)))
}

async fn has_skill(
    State(state): State<CronRouterState>,
    Extension(user): Extension<CurrentUser>,
    Path(cron_job_id): Path<String>,
) -> Result<Json<ApiResponse<HasSkillResponse>>, AppError> {
    let resp = state
        .cron_service
        .has_skill(&user.id, &cron_job_id)
        .await?;
    Ok(Json(ApiResponse::ok(resp)))
}

async fn delete_skill(
    State(state): State<CronRouterState>,
    Extension(user): Extension<CurrentUser>,
    Path(cron_job_id): Path<String>,
) -> Result<Json<ApiResponse<()>>, AppError> {
    state
        .cron_service
        .delete_skill(&user.id, &cron_job_id)
        .await?;
    Ok(Json(ApiResponse::success()))
}

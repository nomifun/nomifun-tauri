use axum::Router;
use axum::extract::rejection::JsonRejection;
use axum::extract::{Extension, Json, Path, Query, State};
use axum::http::StatusCode;
use axum::routing::{get, post};

use nomifun_api_types::{
    ApiResponse, AutoWorkConfigRequest, AutoWorkRunState, AutoWorkState, AutoWorkTargetKind, BatchDeleteRequest,
    BatchDeleteResponse, BoardResponse, ClaimRequest, CompleteRequest, CreateRequirementRequest, ListRequirementsQuery,
    Requirement, ResumeTagRequest, TagBindings, TagSummary, UpdateRequirementRequest, UpdateStatusRequest,
};
use nomifun_auth::CurrentUser;
use nomifun_common::{AppError, PaginatedResult};
use serde::Deserialize;

use crate::state::RequirementRouterState;

pub fn requirement_routes(state: RequirementRouterState) -> Router {
    Router::new()
        .route("/api/requirements", get(list_requirements).post(create_requirement))
        .route("/api/requirements/tags", get(list_tags))
        .route("/api/requirements/tags/{tag}/resume", post(resume_tag))
        .route("/api/requirements/tag-bindings", get(list_tag_bindings))
        .route("/api/requirements/board", get(get_board))
        .route("/api/requirements/batch-delete", post(batch_delete_requirements))
        .route("/api/requirements/claim", post(claim_requirement))
        .route("/api/requirements/autowork", post(set_autowork))
        .route("/api/requirements/autowork/{kind}/{target_id}", get(get_autowork))
        .route("/api/requirements/{id}/status", post(update_requirement_status))
        .route("/api/requirements/{id}/complete", post(complete_requirement))
        .route(
            "/api/requirements/{id}",
            get(get_requirement).put(update_requirement).delete(delete_requirement),
        )
        .with_state(state)
}

async fn create_requirement(
    State(state): State<RequirementRouterState>,
    Extension(_user): Extension<CurrentUser>,
    body: Result<Json<CreateRequirementRequest>, JsonRejection>,
) -> Result<(StatusCode, Json<ApiResponse<Requirement>>), AppError> {
    let Json(req) = body.map_err(|e| AppError::BadRequest(e.to_string()))?;
    let created = state.requirement_service.create(req).await?;
    Ok((StatusCode::CREATED, Json(ApiResponse::ok(created))))
}

async fn list_requirements(
    State(state): State<RequirementRouterState>,
    Extension(_user): Extension<CurrentUser>,
    Query(query): Query<ListRequirementsQuery>,
) -> Result<Json<ApiResponse<PaginatedResult<Requirement>>>, AppError> {
    let page = state.requirement_service.list(&query).await?;
    Ok(Json(ApiResponse::ok(page)))
}

async fn get_requirement(
    State(state): State<RequirementRouterState>,
    Extension(_user): Extension<CurrentUser>,
    Path(id): Path<i64>,
) -> Result<Json<ApiResponse<Requirement>>, AppError> {
    let req = state.requirement_service.get(id).await?;
    Ok(Json(ApiResponse::ok(req)))
}

async fn update_requirement(
    State(state): State<RequirementRouterState>,
    Extension(_user): Extension<CurrentUser>,
    Path(id): Path<i64>,
    body: Result<Json<UpdateRequirementRequest>, JsonRejection>,
) -> Result<Json<ApiResponse<Requirement>>, AppError> {
    let Json(req) = body.map_err(|e| AppError::BadRequest(e.to_string()))?;
    let updated = state.requirement_service.update(id, req).await?;
    Ok(Json(ApiResponse::ok(updated)))
}

async fn delete_requirement(
    State(state): State<RequirementRouterState>,
    Extension(_user): Extension<CurrentUser>,
    Path(id): Path<i64>,
) -> Result<Json<ApiResponse<()>>, AppError> {
    state.requirement_service.delete(id).await?;
    Ok(Json(ApiResponse::success()))
}

async fn batch_delete_requirements(
    State(state): State<RequirementRouterState>,
    Extension(_user): Extension<CurrentUser>,
    body: Result<Json<BatchDeleteRequest>, JsonRejection>,
) -> Result<Json<ApiResponse<BatchDeleteResponse>>, AppError> {
    let Json(req) = body.map_err(|e| AppError::BadRequest(e.to_string()))?;
    if req.ids.is_empty() {
        return Err(AppError::BadRequest("ids must not be empty".into()));
    }
    let deleted = state.requirement_service.delete_many(&req.ids).await?;
    Ok(Json(ApiResponse::ok(BatchDeleteResponse { deleted })))
}

async fn list_tags(
    State(state): State<RequirementRouterState>,
    Extension(_user): Extension<CurrentUser>,
) -> Result<Json<ApiResponse<Vec<TagSummary>>>, AppError> {
    let tags = state.requirement_service.tags().await?;
    Ok(Json(ApiResponse::ok(tags)))
}

/// Resume a paused tag so AutoWork claims its requirements again. Optionally
/// re-queue failed requirements (all, or specific ids) back to pending. Body is
/// optional. Returns the refreshed tag summary.
async fn resume_tag(
    State(state): State<RequirementRouterState>,
    Extension(_user): Extension<CurrentUser>,
    Path(tag): Path<String>,
    body: Result<Json<ResumeTagRequest>, JsonRejection>,
) -> Result<Json<ApiResponse<TagSummary>>, AppError> {
    let req = body.map(|Json(r)| r).unwrap_or_default();
    let mut requeue_ids = req.requeue_ids;
    if req.requeue_failed {
        // Re-queue every currently-failed requirement in the tag.
        let board = state.requirement_service.board(&tag).await?;
        requeue_ids.extend(board.failed.into_iter().map(|r| r.id));
    }
    state.requirement_service.resume_tag(&tag, &requeue_ids).await?;
    let summary = state
        .requirement_service
        .tags()
        .await?
        .into_iter()
        .find(|t| t.tag == tag)
        .unwrap_or_else(|| TagSummary {
            tag: tag.clone(),
            ..Default::default()
        });
    Ok(Json(ApiResponse::ok(summary)))
}

/// AutoWork tag→session bindings for the calling user, grouped by tag. The
/// service returns persisted bindings (every enabled one as `Idle`); here we
/// upgrade `run_state` to `Active` for targets the orchestrator is currently
/// driving (it owns the live progress map). Used by the AutoWork admin.
async fn list_tag_bindings(
    State(state): State<RequirementRouterState>,
    Extension(user): Extension<CurrentUser>,
) -> Result<Json<ApiResponse<Vec<TagBindings>>>, AppError> {
    let mut groups = state.requirement_service.tag_bindings(&user.id).await?;
    for group in &mut groups {
        for binding in &mut group.bindings {
            if matches!(state.orchestrator.live_progress(binding.kind, &binding.target_id), Some((Some(_), _))) {
                binding.run_state = AutoWorkRunState::Active;
            }
        }
    }
    Ok(Json(ApiResponse::ok(groups)))
}

#[derive(Debug, Deserialize)]
struct BoardQuery {
    tag: String,
}

async fn get_board(
    State(state): State<RequirementRouterState>,
    Extension(_user): Extension<CurrentUser>,
    Query(query): Query<BoardQuery>,
) -> Result<Json<ApiResponse<BoardResponse>>, AppError> {
    let board = state.requirement_service.board(&query.tag).await?;
    Ok(Json(ApiResponse::ok(board)))
}

async fn claim_requirement(
    State(state): State<RequirementRouterState>,
    Extension(user): Extension<CurrentUser>,
    body: Result<Json<ClaimRequest>, JsonRejection>,
) -> Result<Json<ApiResponse<Option<Requirement>>>, AppError> {
    let Json(req) = body.map_err(|e| AppError::BadRequest(e.to_string()))?;
    state
        .requirement_service
        .verify_conversation_owner(req.conversation_id, &user.id)
        .await?;
    let lease = req.lease_ms.unwrap_or(crate::service::DEFAULT_LEASE_MS);
    let claimed = state
        .requirement_service
        .claim_next(&req.tag, req.conversation_id, AutoWorkTargetKind::Conversation, lease)
        .await?;
    Ok(Json(ApiResponse::ok(claimed)))
}

async fn update_requirement_status(
    State(state): State<RequirementRouterState>,
    Extension(_user): Extension<CurrentUser>,
    Path(id): Path<i64>,
    body: Result<Json<UpdateStatusRequest>, JsonRejection>,
) -> Result<Json<ApiResponse<Requirement>>, AppError> {
    let Json(req) = body.map_err(|e| AppError::BadRequest(e.to_string()))?;
    let updated = state.requirement_service.set_status(id, req.status, req.note).await?;
    Ok(Json(ApiResponse::ok(updated)))
}

async fn complete_requirement(
    State(state): State<RequirementRouterState>,
    Extension(_user): Extension<CurrentUser>,
    Path(id): Path<i64>,
    body: Result<Json<CompleteRequest>, JsonRejection>,
) -> Result<Json<ApiResponse<Requirement>>, AppError> {
    let Json(req) = body.map_err(|e| AppError::BadRequest(e.to_string()))?;
    let done = state.requirement_service.complete(id, req.completion_note).await?;
    Ok(Json(ApiResponse::ok(done)))
}

async fn set_autowork(
    State(state): State<RequirementRouterState>,
    Extension(user): Extension<CurrentUser>,
    body: Result<Json<AutoWorkConfigRequest>, JsonRejection>,
) -> Result<Json<ApiResponse<AutoWorkState>>, AppError> {
    let Json(req) = body.map_err(|e| AppError::BadRequest(e.to_string()))?;
    if req.target_id.trim().is_empty() {
        return Err(AppError::BadRequest("target_id is required".into()));
    }
    if req.enabled && req.tag.as_deref().unwrap_or("").trim().is_empty() {
        return Err(AppError::BadRequest("tag is required when enabling autowork".into()));
    }
    // Admin guard (标签会话管理): refuse to disable a session that is actively
    // executing a requirement when the request comes from the admin backend. The
    // user must stop it from the session page so a live turn is not interrupted.
    // Session-page toggles leave `from_admin` false and may always disable.
    if !req.enabled
        && req.from_admin
        && matches!(state.orchestrator.live_progress(req.kind, &req.target_id), Some((Some(_), _)))
    {
        return Err(AppError::BadRequest(
            "session is actively executing a requirement; stop it from the session page first".into(),
        ));
    }
    // Ownership + (terminal) eligibility, per target kind.
    match req.kind {
        AutoWorkTargetKind::Conversation => {
            // `target_id` is the AutoWork (string) target handle; the conversation
            // owner check is keyed by the integer conversation id.
            let conv_id = req
                .target_id
                .parse::<i64>()
                .map_err(|_| AppError::NotFound(format!("conversation {}", req.target_id)))?;
            state
                .requirement_service
                .verify_conversation_owner(conv_id, &user.id)
                .await?;
        }
        AutoWorkTargetKind::Terminal => {
            state
                .requirement_service
                .verify_terminal_owner(&req.target_id, &user.id)
                .await?;
            if req.enabled {
                state
                    .requirement_service
                    .ensure_terminal_autowork_eligible(&req.target_id)
                    .await?;
            }
        }
    }
    // Persist config.
    state
        .requirement_service
        .save_autowork_config(
            req.kind,
            &req.target_id,
            req.enabled,
            req.tag.as_deref(),
            req.max_requirements,
        )
        .await?;
    // Start/stop the live loop.
    if req.enabled {
        if let Some(tag) = req.tag.clone() {
            // An explicit enable resumes a tag a prior failure left paused, so
            // toggling 自动工作 on actually RUNS instead of silently inheriting the
            // paused state (which blocks every conversation bound to the tag —
            // the recurring "彻底不工作" trap). Best-effort: a resume failure must
            // not block enabling.
            if let Err(e) = state.requirement_service.resume_tag_for_enable(&tag).await {
                tracing::warn!(tag, error = %e, "auto-resume on autowork enable failed (non-fatal)");
            }
            state
                .orchestrator
                .start(req.kind, req.target_id.clone(), tag, req.max_requirements);
        }
    } else {
        state.orchestrator.stop(req.kind, &req.target_id);
    }
    let st = build_autowork_state(&state, req.kind, &req.target_id).await?;
    state.requirement_service.emit_autowork_state(&st);
    Ok(Json(ApiResponse::ok(st)))
}

async fn get_autowork(
    State(state): State<RequirementRouterState>,
    Extension(user): Extension<CurrentUser>,
    Path((kind, target_id)): Path<(String, String)>,
) -> Result<Json<ApiResponse<AutoWorkState>>, AppError> {
    let kind = AutoWorkTargetKind::parse(&kind)
        .ok_or_else(|| AppError::BadRequest(format!("unknown autowork target kind: {kind}")))?;
    match kind {
        AutoWorkTargetKind::Conversation => {
            let conv_id = target_id
                .parse::<i64>()
                .map_err(|_| AppError::NotFound(format!("conversation {target_id}")))?;
            state
                .requirement_service
                .verify_conversation_owner(conv_id, &user.id)
                .await?;
        }
        AutoWorkTargetKind::Terminal => {
            state
                .requirement_service
                .verify_terminal_owner(&target_id, &user.id)
                .await?;
        }
    }
    let st = build_autowork_state(&state, kind, &target_id).await?;
    Ok(Json(ApiResponse::ok(st)))
}

async fn build_autowork_state(
    state: &RequirementRouterState,
    kind: AutoWorkTargetKind,
    target_id: &str,
) -> Result<AutoWorkState, AppError> {
    let (enabled, tag, _max) = state.requirement_service.read_autowork_config(kind, target_id).await?;
    let running = state.orchestrator.is_running(kind, target_id);
    let live_tag = state.orchestrator.running_tag(kind, target_id).or(tag);
    let (current_requirement_id, completed_count) =
        state.orchestrator.live_progress(kind, target_id).unwrap_or((None, 0));
    let run_state = AutoWorkState::run_state(enabled, current_requirement_id.as_deref());
    Ok(AutoWorkState {
        kind,
        target_id: target_id.to_string(),
        enabled,
        tag: live_tag,
        running,
        run_state,
        current_requirement_id,
        completed_count,
    })
}

use crate::state::ConversationRouterState;
use axum::Router;
use axum::extract::rejection::JsonRejection;
use axum::extract::{Extension, Json, Path, Query, State};
use axum::routing::{get, post};
use nomifun_api_types::{
    AgentModeResponse, ApiResponse, GetModelInfoResponse, SetModeRequest, SetModelRequest, SideQuestionRequest,
    SideQuestionResponse, SlashCommandItem, WorkspaceBrowseQuery, WorkspaceEntry,
};
use nomifun_auth::CurrentUser;
use nomifun_common::{AppError, ConversationId};

/// Build the conversation-ops router (no auth layer applied — the caller is
/// responsible for wrapping this with the auth middleware).
pub fn conversation_ops_routes(state: ConversationRouterState) -> Router {
    Router::new()
        .route(
            "/api/conversations/{conversation_id}/side-question",
            post(side_question),
        )
        .route(
            "/api/conversations/{conversation_id}/slash-commands",
            get(get_slash_commands),
        )
        .route("/api/conversations/{conversation_id}/usage", get(get_usage))
        .route(
            "/api/conversations/{conversation_id}/mode",
            get(get_mode).put(set_mode),
        )
        .route(
            "/api/conversations/{conversation_id}/model",
            get(get_model).put(set_model),
        )
        .route(
            "/api/conversations/{conversation_id}/openclaw/runtime",
            get(get_openclaw_runtime),
        )
        .route(
            "/api/conversations/{conversation_id}/workspace",
            get(browse_workspace),
        )
        .route(
            "/api/conversations/{conversation_id}/clear-context",
            post(clear_context),
        )
        .route(
            "/api/conversations/{conversation_id}/clear-messages",
            post(clear_messages),
        )
        .with_state(state)
}

// ── Route handlers ─────────────────────────────────────────────────

async fn get_mode(
    State(state): State<ConversationRouterState>,
    Extension(user): Extension<CurrentUser>,
    Path(conversation_id): Path<ConversationId>,
) -> Result<Json<ApiResponse<AgentModeResponse>>, AppError> {
    Ok(Json(ApiResponse::ok(
        state.service.get_mode(&user.id, conversation_id.as_str()).await?,
    )))
}

async fn set_mode(
    State(state): State<ConversationRouterState>,
    Extension(user): Extension<CurrentUser>,
    Path(conversation_id): Path<ConversationId>,
    body: Result<Json<SetModeRequest>, JsonRejection>,
) -> Result<Json<ApiResponse<()>>, AppError> {
    let Json(req) = body.map_err(|e| AppError::BadRequest(e.to_string()))?;
    state
        .service
        .set_mode(&user.id, conversation_id.as_str(), req)
        .await?;
    Ok(Json(ApiResponse::success()))
}

/// Clear a conversation's agent context (release model context) while keeping
/// the visible message history. See [`ConversationService::clear_context`].
async fn clear_context(
    State(state): State<ConversationRouterState>,
    Extension(user): Extension<CurrentUser>,
    Path(conversation_id): Path<ConversationId>,
) -> Result<Json<ApiResponse<()>>, AppError> {
    state
        .service
        .clear_context(
            &user.id,
            conversation_id.as_str(),
            &state.runtime_registry,
        )
        .await?;
    Ok(Json(ApiResponse::success()))
}

/// Clear a conversation's **messages** (and artifacts) while keeping the
/// conversation row — the work-partner「清空上下文」按钮。Does not reset
/// status and never touches the companion memory store. See
/// [`ConversationService::clear_messages`].
async fn clear_messages(
    State(state): State<ConversationRouterState>,
    Extension(user): Extension<CurrentUser>,
    Path(conversation_id): Path<ConversationId>,
) -> Result<Json<ApiResponse<()>>, AppError> {
    state
        .service
        .clear_messages(
            &user.id,
            conversation_id.as_str(),
            &state.runtime_registry,
        )
        .await?;
    Ok(Json(ApiResponse::success()))
}

async fn get_model(
    State(state): State<ConversationRouterState>,
    Extension(user): Extension<CurrentUser>,
    Path(conversation_id): Path<ConversationId>,
) -> Result<Json<ApiResponse<GetModelInfoResponse>>, AppError> {
    Ok(Json(ApiResponse::ok(
        state.service.get_model(&user.id, conversation_id.as_str()).await?,
    )))
}

async fn set_model(
    State(state): State<ConversationRouterState>,
    Extension(user): Extension<CurrentUser>,
    Path(conversation_id): Path<ConversationId>,
    body: Result<Json<SetModelRequest>, JsonRejection>,
) -> Result<Json<ApiResponse<()>>, AppError> {
    let Json(req) = body.map_err(|e| AppError::BadRequest(e.to_string()))?;
    state
        .service
        .set_model(&user.id, conversation_id.as_str(), req)
        .await?;
    Ok(Json(ApiResponse::success()))
}

async fn get_usage(
    State(state): State<ConversationRouterState>,
    Extension(user): Extension<CurrentUser>,
    Path(conversation_id): Path<ConversationId>,
) -> Result<Json<ApiResponse<Option<serde_json::Value>>>, AppError> {
    Ok(Json(ApiResponse::ok(
        state.service.get_usage(&user.id, conversation_id.as_str()).await?,
    )))
}

async fn side_question(
    State(state): State<ConversationRouterState>,
    Extension(user): Extension<CurrentUser>,
    Path(conversation_id): Path<ConversationId>,
    Json(req): Json<SideQuestionRequest>,
) -> Result<Json<ApiResponse<SideQuestionResponse>>, AppError> {
    Ok(Json(ApiResponse::ok(
        state
            .service
            .handle_side_question(&user.id, conversation_id.as_str(), req)
            .await?,
    )))
}

async fn get_slash_commands(
    State(state): State<ConversationRouterState>,
    Extension(user): Extension<CurrentUser>,
    Path(conversation_id): Path<ConversationId>,
) -> Result<Json<ApiResponse<Vec<SlashCommandItem>>>, AppError> {
    Ok(Json(ApiResponse::ok(
        state
            .service
            .get_slash_commands(&user.id, conversation_id.as_str())
            .await?,
    )))
}

async fn get_openclaw_runtime(
    State(state): State<ConversationRouterState>,
    Extension(user): Extension<CurrentUser>,
    Path(conversation_id): Path<ConversationId>,
) -> Result<Json<ApiResponse<serde_json::Value>>, AppError> {
    Ok(Json(ApiResponse::ok(
        state
            .service
            .get_openclaw_runtime(&user.id, conversation_id.as_str())
            .await?,
    )))
}

async fn browse_workspace(
    State(state): State<ConversationRouterState>,
    Extension(user): Extension<CurrentUser>,
    Path(conversation_id): Path<ConversationId>,
    Query(query): Query<WorkspaceBrowseQuery>,
) -> Result<Json<ApiResponse<Vec<WorkspaceEntry>>>, AppError> {
    Ok(Json(ApiResponse::ok(
        state
            .service
            .browse_workspace(&user.id, conversation_id.as_str(), query)
            .await?,
    )))
}

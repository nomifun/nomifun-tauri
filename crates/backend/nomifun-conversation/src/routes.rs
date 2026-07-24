use axum::Router;
use axum::extract::rejection::JsonRejection;
use axum::extract::{Extension, Json, Path, Query, State};
use axum::http::StatusCode;
use axum::routing::{get, patch, post};

use nomifun_api_types::{
    ActiveCountResponse, ApiResponse, ApprovalCheckQuery, ApprovalCheckResponse, CloneConversationRequest,
    ConfirmRequest, ConfirmationListResponse, ConversationArtifactListResponse, ConversationArtifactResponse,
    ConversationListResponse, ConversationResponse, CreateConversationRequest, ListConversationsQuery,
    ListMessagesQuery, MessageListResponse, MessageResponse, MessageSearchResponse, SearchMessagesQuery,
    SendMessageRequest, SendMessageResponse, UpdateConversationArtifactRequest, UpdateConversationRequest,
};
use nomifun_auth::CurrentUser;
use nomifun_common::{AppError, ConversationId, MessageId};

use crate::service::strip_clone_instance_state;
use crate::state::ConversationRouterState;

/// Build the conversation router (CRUD + message flow + confirmation + extended operations).
///
/// All routes require authentication (applied by the caller).
pub fn conversation_routes(state: ConversationRouterState) -> Router {
    Router::new()
        .route("/api/conversations", post(create).get(list))
        .route(
            "/api/conversations/{conversation_id}",
            get(get_one).patch(update).delete(delete_one),
        )
        .route("/api/conversations/{conversation_id}/reset", post(reset))
        .route("/api/conversations/{conversation_id}/associated", get(associated))
        .route(
            "/api/conversations/{conversation_id}/messages",
            get(list_msg).post(send_msg),
        )
        .route(
            "/api/conversations/{conversation_id}/messages/{message_id}",
            get(get_msg),
        )
        .route(
            "/api/conversations/{conversation_id}/messages/{message_id}/edit-resubmit",
            post(edit_resubmit),
        )
        .route(
            "/api/conversations/{conversation_id}/messages/{message_id}/knowledge-writeback/retry",
            post(retry_knowledge_writeback),
        )
        .route(
            "/api/conversations/{conversation_id}/artifacts",
            get(list_artifacts),
        )
        .route(
            "/api/conversations/{conversation_id}/artifacts/{conversation_artifact_id}",
            patch(update_artifact),
        )
        .route("/api/conversations/{conversation_id}/cancel", post(cancel))
        .route("/api/conversations/{conversation_id}/steer", post(steer))
        .route("/api/conversations/{conversation_id}/warmup", post(warmup))
        // Confirmation system
        .route(
            "/api/conversations/{conversation_id}/confirmations",
            get(list_confirmations),
        )
        .route(
            "/api/conversations/{conversation_id}/confirmations/{call_id}/confirm",
            post(confirm),
        )
        .route(
            "/api/conversations/{conversation_id}/approvals/check",
            get(check_approval),
        )
        .route("/api/conversations/active-count", get(active_runtime_count))
        .route("/api/conversations/clone", post(clone))
        .route("/api/messages/search", get(search_messages))
        .with_state(state)
}

// ── Handlers ───────────────────────────────────────────────────────

/// Remove every runtime-authority field from open JSON at the one untrusted
/// HTTP boundary.  The service/factory derive owner authority and inject
/// scoped configs from backend state; create/update/clone cannot persist a
/// second authorization source.
fn strip_server_owned_runtime_fields(extra: &mut serde_json::Value) {
    if let Some(map) = extra.as_object_mut() {
        for key in [
            "desktopGateway",
            "desktop_gateway",
            "gateway_mcp_config",
            "gateway_excluded_tools",
            "requirement_mcp_config",
            "knowledge_mcp_config",
            "open_mcp_config",
            "computer_mcp_config",
            "browser_mcp_config",
            "user_id",
            "allowed_tools",
            "knowledge_mounts",
            "knowledge_writeback",
            "knowledge_channel_write_enabled",
            "companion_session",
            "companion",
            "companion_id",
            "channel_platform",
            "public_agent_id",
            "exposure",
            "cron_job_id",
            "mcp_server_ids",
            "mcp_servers",
            "mcp_statuses",
            "session_mcp_servers",
            "skills",
            "temp_workspace_id",
        ] {
            map.remove(key);
        }
    }
}

fn strip_server_owned_preset_fields(extra: &mut serde_json::Value) {
    if let Some(map) = extra.as_object_mut() {
        for key in [
            "preset_id",
            "preset_revision",
            "preset_snapshot",
            "preset_knowledge_binding",
            "preset_instructions_embedded",
        ] {
            map.remove(key);
        }
    }
}

async fn create(
    State(state): State<ConversationRouterState>,
    Extension(user): Extension<CurrentUser>,
    body: Result<Json<CreateConversationRequest>, JsonRejection>,
) -> Result<(StatusCode, Json<ApiResponse<ConversationResponse>>), AppError> {
    let Json(mut req) = body.map_err(|e| AppError::BadRequest(e.to_string()))?;
    strip_server_owned_runtime_fields(&mut req.extra);
    strip_server_owned_preset_fields(&mut req.extra);
    let conversation = state.service.create(&user.id, req).await?;
    Ok((StatusCode::CREATED, Json(ApiResponse::ok(conversation))))
}

async fn list(
    State(state): State<ConversationRouterState>,
    Extension(user): Extension<CurrentUser>,
    Query(query): Query<ListConversationsQuery>,
) -> Result<Json<ApiResponse<ConversationListResponse>>, AppError> {
    // 普通会话列表保留 companion 行(前端侧边栏自行过滤),不在此处排除。
    let result = state.service.list(&user.id, query, false).await?;
    Ok(Json(ApiResponse::ok(result)))
}

async fn clone(
    State(state): State<ConversationRouterState>,
    Extension(user): Extension<CurrentUser>,
    body: Result<Json<CloneConversationRequest>, JsonRejection>,
) -> Result<(StatusCode, Json<ApiResponse<ConversationResponse>>), AppError> {
    let Json(mut req) = body.map_err(|e| AppError::BadRequest(e.to_string()))?;
    strip_server_owned_runtime_fields(&mut req.conversation.extra);
    strip_server_owned_preset_fields(&mut req.conversation.extra);
    strip_clone_instance_state(&mut req.conversation.extra);
    let conversation = state.service.clone_create(&user.id, req).await?;
    Ok((StatusCode::CREATED, Json(ApiResponse::ok(conversation))))
}

async fn get_one(
    State(state): State<ConversationRouterState>,
    Extension(user): Extension<CurrentUser>,
    Path(conversation_id): Path<ConversationId>,
) -> Result<Json<ApiResponse<ConversationResponse>>, AppError> {
    let conversation = state.service.get(&user.id, conversation_id.as_str()).await?;
    Ok(Json(ApiResponse::ok(conversation)))
}

async fn update(
    State(state): State<ConversationRouterState>,
    Extension(user): Extension<CurrentUser>,
    Path(conversation_id): Path<ConversationId>,
    body: Result<Json<UpdateConversationRequest>, JsonRejection>,
) -> Result<Json<ApiResponse<ConversationResponse>>, AppError> {
    let Json(mut req) = body.map_err(|e| AppError::BadRequest(e.to_string()))?;
    if let Some(extra) = req.extra.as_mut() {
        // `update` merges extra keys, so a client could otherwise smuggle the
        // gateway flag into an existing conversation.
        strip_server_owned_runtime_fields(extra);
        strip_server_owned_preset_fields(extra);
    }
    let conversation = state
        .service
        .update(
            &user.id,
            conversation_id.as_str(),
            req,
            &state.runtime_registry,
        )
        .await?;
    Ok(Json(ApiResponse::ok(conversation)))
}

async fn delete_one(
    State(state): State<ConversationRouterState>,
    Extension(user): Extension<CurrentUser>,
    Path(conversation_id): Path<ConversationId>,
) -> Result<Json<ApiResponse<()>>, AppError> {
    state.service.delete(&user.id, conversation_id.as_str()).await?;
    Ok(Json(ApiResponse::success()))
}

async fn reset(
    State(state): State<ConversationRouterState>,
    Extension(user): Extension<CurrentUser>,
    Path(conversation_id): Path<ConversationId>,
) -> Result<Json<ApiResponse<()>>, AppError> {
    state.service.reset(&user.id, conversation_id.as_str()).await?;
    Ok(Json(ApiResponse::success()))
}

async fn associated(
    State(state): State<ConversationRouterState>,
    Extension(user): Extension<CurrentUser>,
    Path(conversation_id): Path<ConversationId>,
) -> Result<Json<ApiResponse<Vec<ConversationResponse>>>, AppError> {
    let items = state
        .service
        .list_associated(&user.id, conversation_id.as_str())
        .await?;
    Ok(Json(ApiResponse::ok(items)))
}

async fn list_msg(
    State(state): State<ConversationRouterState>,
    Extension(user): Extension<CurrentUser>,
    Path(conversation_id): Path<ConversationId>,
    Query(query): Query<ListMessagesQuery>,
) -> Result<Json<ApiResponse<MessageListResponse>>, AppError> {
    let result = state
        .service
        .list_messages(&user.id, conversation_id.as_str(), query)
        .await?;
    Ok(Json(ApiResponse::ok(result)))
}

#[derive(serde::Deserialize)]
struct MessagePathParams {
    conversation_id: ConversationId,
    message_id: MessageId,
}

#[derive(serde::Deserialize)]
struct RetryKnowledgeWritebackRequest {
    attempt_id: String,
}

async fn get_msg(
    State(state): State<ConversationRouterState>,
    Extension(user): Extension<CurrentUser>,
    Path(params): Path<MessagePathParams>,
) -> Result<Json<ApiResponse<MessageResponse>>, AppError> {
    let result = state
        .service
        .get_message(
            &user.id,
            params.conversation_id.as_str(),
            params.message_id.as_str(),
        )
        .await?;
    Ok(Json(ApiResponse::ok(result)))
}

async fn edit_resubmit(
    State(state): State<ConversationRouterState>,
    Extension(user): Extension<CurrentUser>,
    Path(params): Path<MessagePathParams>,
    body: Result<Json<SendMessageRequest>, JsonRejection>,
) -> Result<(StatusCode, Json<ApiResponse<SendMessageResponse>>), AppError> {
    let Json(req) = body.map_err(|e| AppError::BadRequest(e.to_string()))?;
    let msg_id = state
        .service
        .edit_and_resubmit(
            &user.id,
            params.conversation_id.as_str(),
            params.message_id.as_str(),
            req,
            &state.runtime_registry,
        )
        .await?;
    Ok((
        StatusCode::ACCEPTED,
        Json(ApiResponse::ok(SendMessageResponse { msg_id })),
    ))
}

async fn retry_knowledge_writeback(
    State(state): State<ConversationRouterState>,
    Extension(user): Extension<CurrentUser>,
    Path(params): Path<MessagePathParams>,
    body: Result<Json<RetryKnowledgeWritebackRequest>, JsonRejection>,
) -> Result<(StatusCode, Json<ApiResponse<()>>), AppError> {
    let Json(req) = body.map_err(|error| AppError::BadRequest(error.to_string()))?;
    state
        .service
        .retry_knowledge_writeback(
            &user.id,
            params.conversation_id.as_str(),
            params.message_id.as_str(),
            &req.attempt_id,
        )
        .await?;
    Ok((StatusCode::ACCEPTED, Json(ApiResponse::ok(()))))
}

async fn send_msg(
    State(state): State<ConversationRouterState>,
    Extension(user): Extension<CurrentUser>,
    Path(conversation_id): Path<ConversationId>,
    body: Result<Json<SendMessageRequest>, JsonRejection>,
) -> Result<(StatusCode, Json<ApiResponse<SendMessageResponse>>), AppError> {
    let Json(req) = body.map_err(|e| AppError::BadRequest(e.to_string()))?;
    let msg_id = state
        .service
        .send_message(
            &user.id,
            conversation_id.as_str(),
            req,
            &state.runtime_registry,
        )
        .await?;
    Ok((
        StatusCode::ACCEPTED,
        Json(ApiResponse::ok(SendMessageResponse { msg_id })),
    ))
}

async fn steer(
    State(state): State<ConversationRouterState>,
    Extension(user): Extension<CurrentUser>,
    Path(conversation_id): Path<ConversationId>,
    body: Result<Json<SendMessageRequest>, JsonRejection>,
) -> Result<(StatusCode, Json<ApiResponse<SendMessageResponse>>), AppError> {
    let Json(req) = body.map_err(|e| AppError::BadRequest(e.to_string()))?;
    let msg_id = state
        .service
        .steer_message(
            &user.id,
            conversation_id.as_str(),
            req,
            &state.runtime_registry,
        )
        .await?;
    Ok((
        StatusCode::ACCEPTED,
        Json(ApiResponse::ok(SendMessageResponse { msg_id })),
    ))
}

async fn list_artifacts(
    State(state): State<ConversationRouterState>,
    Extension(user): Extension<CurrentUser>,
    Path(conversation_id): Path<ConversationId>,
) -> Result<Json<ApiResponse<ConversationArtifactListResponse>>, AppError> {
    let result = state
        .service
        .list_artifacts(&user.id, conversation_id.as_str())
        .await?;
    Ok(Json(ApiResponse::ok(result)))
}

#[derive(serde::Deserialize)]
struct ArtifactPathParams {
    conversation_id: ConversationId,
    conversation_artifact_id: String,
}

async fn update_artifact(
    State(state): State<ConversationRouterState>,
    Extension(user): Extension<CurrentUser>,
    Path(params): Path<ArtifactPathParams>,
    body: Result<Json<UpdateConversationArtifactRequest>, JsonRejection>,
) -> Result<Json<ApiResponse<ConversationArtifactResponse>>, AppError> {
    let Json(req) = body.map_err(|e| AppError::BadRequest(e.to_string()))?;
    let artifact = state
        .service
        .update_artifact(
            &user.id,
            params.conversation_id.as_str(),
            &params.conversation_artifact_id,
            req,
        )
        .await?;
    Ok(Json(ApiResponse::ok(artifact)))
}

async fn cancel(
    State(state): State<ConversationRouterState>,
    Extension(user): Extension<CurrentUser>,
    Path(conversation_id): Path<ConversationId>,
) -> Result<Json<ApiResponse<()>>, AppError> {
    state
        .service
        .cancel(
            &user.id,
            conversation_id.as_str(),
            &state.runtime_registry,
        )
        .await?;
    Ok(Json(ApiResponse::success()))
}

async fn warmup(
    State(state): State<ConversationRouterState>,
    Extension(user): Extension<CurrentUser>,
    Path(conversation_id): Path<ConversationId>,
) -> Result<Json<ApiResponse<()>>, AppError> {
    state
        .service
        .warmup(
            &user.id,
            conversation_id.as_str(),
            &state.runtime_registry,
        )
        .await?;
    Ok(Json(ApiResponse::success()))
}

async fn search_messages(
    State(state): State<ConversationRouterState>,
    Extension(user): Extension<CurrentUser>,
    Query(query): Query<SearchMessagesQuery>,
) -> Result<Json<ApiResponse<MessageSearchResponse>>, AppError> {
    let result = state.service.search_messages(&user.id, query).await?;
    Ok(Json(ApiResponse::ok(result)))
}

// ── Confirmation handlers ─────────────────────────────────────────

async fn list_confirmations(
    State(state): State<ConversationRouterState>,
    Extension(user): Extension<CurrentUser>,
    Path(conversation_id): Path<ConversationId>,
) -> Result<Json<ApiResponse<ConfirmationListResponse>>, AppError> {
    let items = state
        .service
        .list_confirmations(
            &user.id,
            conversation_id.as_str(),
            &state.runtime_registry,
        )
        .await?;
    Ok(Json(ApiResponse::ok(items)))
}

#[derive(serde::Deserialize)]
struct ConfirmPathParams {
    conversation_id: ConversationId,
    call_id: String,
}

async fn confirm(
    State(state): State<ConversationRouterState>,
    Extension(user): Extension<CurrentUser>,
    Path(params): Path<ConfirmPathParams>,
    body: Result<Json<ConfirmRequest>, JsonRejection>,
) -> Result<Json<ApiResponse<()>>, AppError> {
    let Json(req) = body.map_err(|e| AppError::BadRequest(e.to_string()))?;
    state
        .service
        .confirm(
            &user.id,
            params.conversation_id.as_str(),
            &params.call_id,
            req,
            &state.runtime_registry,
        )
        .await?;
    Ok(Json(ApiResponse::success()))
}

async fn check_approval(
    State(state): State<ConversationRouterState>,
    Extension(user): Extension<CurrentUser>,
    Path(conversation_id): Path<ConversationId>,
    Query(query): Query<ApprovalCheckQuery>,
) -> Result<Json<ApiResponse<ApprovalCheckResponse>>, AppError> {
    if query.action.trim().is_empty() {
        return Err(AppError::BadRequest("action must not be empty".into()));
    }

    let result = state
        .service
        .check_approval(
            &user.id,
            conversation_id.as_str(),
            &query.action,
            query.command_type.as_deref(),
            &state.runtime_registry,
        )
        .await?;
    Ok(Json(ApiResponse::ok(result)))
}

async fn active_runtime_count(
    State(state): State<ConversationRouterState>,
    Extension(_user): Extension<CurrentUser>,
) -> Result<Json<ApiResponse<ActiveCountResponse>>, AppError> {
    let count = state.runtime_registry.active_runtime_count();
    Ok(Json(ApiResponse::ok(ActiveCountResponse { count })))
}

#[cfg(test)]
mod tests {
    use super::strip_server_owned_runtime_fields;
    use nomifun_api_types::SendMessageRequest;
    use serde_json::json;

    #[test]
    fn public_send_body_cannot_forge_engine_delivery_authority() {
        let result = serde_json::from_value::<SendMessageRequest>(json!({
            "content": "ordinary user turn",
            "durable_operation_id": "forged-operation",
            "execution_id": "forged-execution"
        }));

        // Durable operation identity is deliberately absent from the public
        // DTO. The boundary rejects forged unknown keys instead of silently
        // normalizing a legacy or ambiguous payload.
        assert!(result.is_err());
    }

    #[test]
    fn strips_runtime_authority_fields_but_keeps_agent_configuration() {
        let mut extra = json!({
            "desktopGateway": true,
            "desktop_gateway": true,
            "companion_session": true,
            "backend": "claude",
        });
        strip_server_owned_runtime_fields(&mut extra);
        assert!(extra.get("desktopGateway").is_none());
        assert!(extra.get("desktop_gateway").is_none());
        assert!(extra.get("companion_session").is_none());
        // Non-authority agent configuration survives.
        assert_eq!(extra["backend"], json!("claude"));
    }

    #[test]
    fn strip_is_a_noop_on_non_objects() {
        let mut extra = json!("not an object");
        strip_server_owned_runtime_fields(&mut extra);
        assert_eq!(extra, json!("not an object"));
    }
}

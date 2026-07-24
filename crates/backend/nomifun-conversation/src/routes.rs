use axum::Router;
use axum::extract::rejection::JsonRejection;
use axum::extract::{Extension, Json, Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
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

use crate::service::{
    IdempotentMessageDelivery, strip_clone_instance_state, validate_public_idempotency_key,
};
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
            "preset_rules",
            "preset_context",
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
    headers: HeaderMap,
    body: Result<Json<SendMessageRequest>, JsonRejection>,
) -> Result<(StatusCode, Json<ApiResponse<SendMessageResponse>>), AppError> {
    let Json(req) = body.map_err(|e| AppError::BadRequest(e.to_string()))?;
    let idempotency_key = public_idempotency_key_from_headers(&headers)?;
    let delivery = state
        .service
        .edit_and_resubmit_with_idempotency_key(
            &user.id,
            params.conversation_id.as_str(),
            params.message_id.as_str(),
            &idempotency_key,
            req,
            &state.runtime_registry,
        )
        .await?;
    Ok((
        StatusCode::ACCEPTED,
        Json(ApiResponse::ok(send_message_response(delivery))),
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

fn public_idempotency_key_from_headers(
    headers: &HeaderMap,
) -> Result<String, AppError> {
    let mut values = headers.get_all("idempotency-key").iter();
    let Some(value) = values.next() else {
        return Err(AppError::BadRequest(
            "Idempotency-Key header is required".to_owned(),
        ));
    };
    if values.next().is_some() {
        return Err(AppError::BadRequest(
            "Idempotency-Key header must be supplied exactly once".to_owned(),
        ));
    }
    let value = value
        .to_str()
        .map_err(|_| AppError::BadRequest("Idempotency-Key must be valid ASCII".to_owned()))?;
    validate_public_idempotency_key(value)?;
    Ok(value.to_owned())
}

fn initial_delivery_requested_from_headers(
    headers: &HeaderMap,
) -> Result<bool, AppError> {
    let mut values = headers
        .get_all("x-nomifun-initial-delivery")
        .iter();
    let Some(value) = values.next() else {
        return Ok(false);
    };
    if values.next().is_some() {
        return Err(AppError::BadRequest(
            "X-Nomifun-Initial-Delivery header must be supplied at most once"
                .to_owned(),
        ));
    }
    let value = value.to_str().map_err(|_| {
        AppError::BadRequest(
            "X-Nomifun-Initial-Delivery must be valid ASCII".to_owned(),
        )
    })?;
    if value != "1" {
        return Err(AppError::BadRequest(
            "X-Nomifun-Initial-Delivery must be exactly '1' when present"
                .to_owned(),
        ));
    }
    Ok(true)
}

fn send_message_response(delivery: IdempotentMessageDelivery) -> SendMessageResponse {
    SendMessageResponse {
        msg_id: delivery.message_id,
        replayed: delivery.replayed,
        completed: delivery.completed,
        result_ok: delivery.result_ok,
        result_text: delivery.result_text,
        result_error: delivery.result_error,
    }
}

async fn send_msg(
    State(state): State<ConversationRouterState>,
    Extension(user): Extension<CurrentUser>,
    Path(conversation_id): Path<ConversationId>,
    headers: HeaderMap,
    body: Result<Json<SendMessageRequest>, JsonRejection>,
) -> Result<(StatusCode, Json<ApiResponse<SendMessageResponse>>), AppError> {
    let Json(req) = body.map_err(|e| AppError::BadRequest(e.to_string()))?;
    let idempotency_key = public_idempotency_key_from_headers(&headers)?;
    let initial_delivery = initial_delivery_requested_from_headers(&headers)?;
    let delivery = if initial_delivery {
        state
            .service
            .send_initial_message_with_idempotency_key(
                &user.id,
                conversation_id.as_str(),
                &idempotency_key,
                req,
                &state.runtime_registry,
            )
            .await?
    } else {
        state
            .service
            .send_message_with_idempotency_key(
                &user.id,
                conversation_id.as_str(),
                &idempotency_key,
                req,
                &state.runtime_registry,
            )
            .await?
    };
    Ok((
        StatusCode::ACCEPTED,
        Json(ApiResponse::ok(send_message_response(delivery))),
    ))
}

async fn steer(
    State(state): State<ConversationRouterState>,
    Extension(user): Extension<CurrentUser>,
    Path(conversation_id): Path<ConversationId>,
    headers: HeaderMap,
    body: Result<Json<SendMessageRequest>, JsonRejection>,
) -> Result<(StatusCode, Json<ApiResponse<SendMessageResponse>>), AppError> {
    let Json(req) = body.map_err(|e| AppError::BadRequest(e.to_string()))?;
    let idempotency_key = public_idempotency_key_from_headers(&headers)?;
    let delivery = state
        .service
        .steer_message_with_idempotency_key(
            &user.id,
            conversation_id.as_str(),
            &idempotency_key,
            req,
            &state.runtime_registry,
        )
        .await?;
    Ok((
        StatusCode::ACCEPTED,
        Json(ApiResponse::ok(send_message_response(delivery))),
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
        .warmup_for_view(
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
    use super::{
        initial_delivery_requested_from_headers,
        public_idempotency_key_from_headers, send_message_response,
        strip_server_owned_preset_fields, strip_server_owned_runtime_fields,
    };
    use crate::service::{
        IdempotentMessageDelivery, PUBLIC_IDEMPOTENCY_KEY_MAX_BYTES,
    };
    use axum::http::{HeaderMap, HeaderValue};
    use nomifun_api_types::SendMessageRequest;
    use nomifun_common::AppError;
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
    fn public_send_accepts_one_bounded_visible_ascii_idempotency_header() {
        let mut headers = HeaderMap::new();
        headers.insert(
            "idempotency-key",
            HeaderValue::from_static("0190f5fe-7c00-7a00-8000-000000000777"),
        );

        assert_eq!(
            public_idempotency_key_from_headers(&headers).unwrap(),
            "0190f5fe-7c00-7a00-8000-000000000777"
        );
    }

    #[test]
    fn initial_delivery_header_is_explicit_strict_and_non_forgiving() {
        assert!(
            !initial_delivery_requested_from_headers(&HeaderMap::new())
                .unwrap()
        );

        let mut enabled = HeaderMap::new();
        enabled.insert(
            "x-nomifun-initial-delivery",
            HeaderValue::from_static("1"),
        );
        assert!(initial_delivery_requested_from_headers(&enabled).unwrap());

        for invalid in ["0", "true", " 1", "1 "] {
            let mut headers = HeaderMap::new();
            headers.insert(
                "x-nomifun-initial-delivery",
                HeaderValue::from_str(invalid).unwrap(),
            );
            assert!(
                initial_delivery_requested_from_headers(&headers).is_err(),
                "invalid initial delivery header must fail closed: {invalid:?}"
            );
        }

        let mut duplicate = HeaderMap::new();
        duplicate.append(
            "x-nomifun-initial-delivery",
            HeaderValue::from_static("1"),
        );
        duplicate.append(
            "x-nomifun-initial-delivery",
            HeaderValue::from_static("1"),
        );
        assert!(initial_delivery_requested_from_headers(&duplicate).is_err());
    }

    #[test]
    fn public_delivery_response_preserves_replay_and_terminal_metadata() {
        let response = send_message_response(IdempotentMessageDelivery {
            message_id: "0190f5fe-7c00-7a00-8000-000000000501".to_owned(),
            replayed: true,
            completed: true,
            result_ok: Some(false),
            result_text: Some("terminal output".to_owned()),
            result_error: Some("provider failed".to_owned()),
        });

        assert_eq!(
            serde_json::to_value(response).unwrap(),
            json!({
                "msg_id": "0190f5fe-7c00-7a00-8000-000000000501",
                "replayed": true,
                "completed": true,
                "result_ok": false,
                "result_text": "terminal output",
                "result_error": "provider failed",
            })
        );
    }

    #[test]
    fn public_send_rejects_ambiguous_or_unsafe_idempotency_headers() {
        let missing = public_idempotency_key_from_headers(&HeaderMap::new())
            .expect_err("missing header must be rejected");
        assert!(matches!(missing, AppError::BadRequest(_)));

        let mut duplicate = HeaderMap::new();
        duplicate.append("idempotency-key", HeaderValue::from_static("first"));
        duplicate.append("idempotency-key", HeaderValue::from_static("second"));
        assert!(public_idempotency_key_from_headers(&duplicate).is_err());

        for invalid in [
            String::new(),
            "contains space".to_owned(),
            "x".repeat(PUBLIC_IDEMPOTENCY_KEY_MAX_BYTES + 1),
        ] {
            let mut headers = HeaderMap::new();
            headers.insert(
                "idempotency-key",
                HeaderValue::from_str(&invalid).expect("HTTP-safe test header"),
            );
            assert!(
                public_idempotency_key_from_headers(&headers).is_err(),
                "header must be rejected: {invalid:?}"
            );
        }

        let mut non_ascii = HeaderMap::new();
        non_ascii.insert(
            "idempotency-key",
            HeaderValue::from_bytes(&[0xff]).expect("opaque header bytes"),
        );
        assert!(public_idempotency_key_from_headers(&non_ascii).is_err());
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
    fn strips_all_server_owned_preset_projection_fields() {
        let mut extra = json!({
            "preset_id": "forged",
            "preset_revision": 99,
            "preset_snapshot": {"instructions": "forged"},
            "preset_rules": "forged",
            "preset_context": "forged",
            "preset_knowledge_binding": true,
            "preset_instructions_embedded": true,
            "backend": "claude",
        });

        strip_server_owned_preset_fields(&mut extra);

        for key in [
            "preset_id",
            "preset_revision",
            "preset_snapshot",
            "preset_rules",
            "preset_context",
            "preset_knowledge_binding",
            "preset_instructions_embedded",
        ] {
            assert!(extra.get(key).is_none(), "{key} must be server-owned");
        }
        assert_eq!(extra["backend"], json!("claude"));
    }

    #[test]
    fn strip_is_a_noop_on_non_objects() {
        let mut extra = json!("not an object");
        strip_server_owned_runtime_fields(&mut extra);
        assert_eq!(extra, json!("not an object"));
    }
}

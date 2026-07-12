use axum::Router;
use axum::extract::rejection::JsonRejection;
use axum::extract::{Json, Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};

use nomifun_api_types::{
    ApiResponse, BatchImportMcpServersRequest, CreateMcpServerRequest, DetectedMcpServerResponse, ErrorResponse,
    McpConnectionTestErrorCode, McpServerResponse, OAuthCheckStatusRequest, OAuthLoginRequest, OAuthLoginResponse,
    OAuthLogoutRequest, OAuthStatusResponse, TestMcpConnectionRequest, UpdateMcpServerRequest,
};
use nomifun_common::AppError;

use crate::connection_test::McpConnectionTestService;
use crate::oauth_service::McpOAuthService;
use crate::service::McpConfigService;
use crate::sync_service::McpSyncService;
use crate::types::McpServerTransport;

// ---------------------------------------------------------------------------
// Router state
// ---------------------------------------------------------------------------

/// Shared state for MCP route handlers.
#[derive(Clone)]
pub struct McpRouterState {
    pub config_service: McpConfigService,
    pub sync_service: McpSyncService,
    pub connection_test_service: McpConnectionTestService,
    pub oauth_service: McpOAuthService,
}

// ---------------------------------------------------------------------------
// Router builder
// ---------------------------------------------------------------------------

/// Build the MCP router with all `/api/mcp/*` routes.
///
/// Includes CRUD routes, agent config detection, connection tests, and OAuth.
/// All routes require authentication (applied by the caller).
pub fn mcp_routes(state: McpRouterState) -> Router {
    Router::new()
        .route("/api/mcp/servers", get(list_servers).post(add_server))
        .route("/api/mcp/servers/import", post(batch_import))
        .route(
            "/api/mcp/servers/{id}",
            get(get_server).put(edit_server).delete(delete_server),
        )
        .route("/api/mcp/servers/{id}/toggle", post(toggle_server))
        // Connection test route
        .route("/api/mcp/test-connection", post(test_connection))
        // Agent config discovery route
        .route("/api/mcp/agent-configs", get(get_agent_configs))
        // OAuth routes
        .route("/api/mcp/oauth/check-status", post(oauth_check_status))
        .route("/api/mcp/oauth/login", post(oauth_login))
        .route("/api/mcp/oauth/logout", post(oauth_logout))
        .route("/api/mcp/oauth/authenticated", get(oauth_authenticated))
        .with_state(state)
}

// ---------------------------------------------------------------------------
// CRUD Handlers
// ---------------------------------------------------------------------------

/// `GET /api/mcp/servers` — list all MCP servers.
async fn list_servers(
    State(state): State<McpRouterState>,
) -> Result<Json<ApiResponse<Vec<McpServerResponse>>>, AppError> {
    let servers = state.config_service.list_servers().await?;
    Ok(Json(ApiResponse::ok(servers)))
}

/// `GET /api/mcp/servers/:id` — get a single MCP server.
async fn get_server(
    State(state): State<McpRouterState>,
    Path(id): Path<String>,
) -> Result<Json<ApiResponse<McpServerResponse>>, AppError> {
    let server = state.config_service.get_server(&id).await?;
    Ok(Json(ApiResponse::ok(server)))
}

/// `POST /api/mcp/servers` — create (or upsert by name) an MCP server.
async fn add_server(
    State(state): State<McpRouterState>,
    body: Result<Json<CreateMcpServerRequest>, JsonRejection>,
) -> Result<(StatusCode, Json<ApiResponse<McpServerResponse>>), AppError> {
    let Json(req) = body.map_err(|e| AppError::BadRequest(e.to_string()))?;
    let server = state.config_service.add_server(req).await?;
    Ok((StatusCode::CREATED, Json(ApiResponse::ok(server))))
}

/// `PUT /api/mcp/servers/:id` — partial update an MCP server.
async fn edit_server(
    State(state): State<McpRouterState>,
    Path(id): Path<String>,
    body: Result<Json<UpdateMcpServerRequest>, JsonRejection>,
) -> Result<Json<ApiResponse<McpServerResponse>>, AppError> {
    let Json(req) = body.map_err(|e| AppError::BadRequest(e.to_string()))?;
    let server = state.config_service.edit_server(&id, req).await?;
    Ok(Json(ApiResponse::ok(server)))
}

/// `DELETE /api/mcp/servers/:id` — delete an MCP server.
async fn delete_server(
    State(state): State<McpRouterState>,
    Path(id): Path<String>,
) -> Result<Json<ApiResponse<()>>, AppError> {
    state.config_service.delete_server(&id).await?;
    Ok(Json(ApiResponse::success()))
}

/// `POST /api/mcp/servers/:id/toggle` — toggle enabled state.
async fn toggle_server(
    State(state): State<McpRouterState>,
    Path(id): Path<String>,
) -> Result<Json<ApiResponse<McpServerResponse>>, AppError> {
    let server = state.config_service.toggle_server(&id).await?;
    Ok(Json(ApiResponse::ok(server)))
}

/// `POST /api/mcp/servers/import` — batch import MCP servers.
async fn batch_import(
    State(state): State<McpRouterState>,
    body: Result<Json<BatchImportMcpServersRequest>, JsonRejection>,
) -> Result<Json<ApiResponse<Vec<McpServerResponse>>>, AppError> {
    let Json(req) = body.map_err(|e| AppError::BadRequest(e.to_string()))?;
    let servers = state.config_service.batch_import(req).await?;
    Ok(Json(ApiResponse::ok(servers)))
}

// ---------------------------------------------------------------------------
// Connection Test Handler
// ---------------------------------------------------------------------------

/// `POST /api/mcp/test-connection` — test MCP server connectivity.
///
/// Creates a temporary MCP client, connects, lists tools, and closes.
async fn test_connection(
    State(state): State<McpRouterState>,
    body: Result<Json<TestMcpConnectionRequest>, JsonRejection>,
) -> Result<Response, AppError> {
    let Json(req) = body.map_err(|e| AppError::BadRequest(e.to_string()))?;
    let transport = McpServerTransport::from(req.transport);
    let result = state
        .connection_test_service
        .test_connection(&req.name, &transport)
        .await;
    if let Some(server_id) = req.id {
        state.config_service.persist_test_result(server_id, &result).await?;
    }
    if result.success || result.needs_auth == Some(true) {
        return Ok(Json(ApiResponse::ok(result)).into_response());
    }

    let status = result
        .code
        .map(connection_test_failure_status)
        .unwrap_or(StatusCode::BAD_GATEWAY);
    let error = result
        .error
        .clone()
        .unwrap_or_else(|| "MCP connection test failed".to_string());
    let code = result
        .code
        .map(McpConnectionTestErrorCode::as_str)
        .unwrap_or("MCP_CONNECTION_FAILED");

    Ok((
        status,
        Json(ErrorResponse::new_with_details(error, code, result.details.clone())),
    )
        .into_response())
}

fn connection_test_failure_status(code: McpConnectionTestErrorCode) -> StatusCode {
    match code {
        McpConnectionTestErrorCode::CommandNotFound
        | McpConnectionTestErrorCode::CommandPermissionDenied
        | McpConnectionTestErrorCode::CommandStartFailed => StatusCode::UNPROCESSABLE_ENTITY,
        McpConnectionTestErrorCode::Timeout => StatusCode::GATEWAY_TIMEOUT,
        McpConnectionTestErrorCode::ConnectionFailed
        | McpConnectionTestErrorCode::HttpError
        | McpConnectionTestErrorCode::RpcError
        | McpConnectionTestErrorCode::ProtocolError => StatusCode::BAD_GATEWAY,
    }
}

// ---------------------------------------------------------------------------
// Agent Sync Handlers
// ---------------------------------------------------------------------------

/// `GET /api/mcp/agent-configs` — scan all installed Agent CLIs
/// and return their current MCP server configurations.
async fn get_agent_configs(
    State(state): State<McpRouterState>,
) -> Result<Json<ApiResponse<Vec<DetectedMcpServerResponse>>>, AppError> {
    let configs = state.sync_service.get_agent_configs().await?;
    Ok(Json(ApiResponse::ok(configs)))
}

// ---------------------------------------------------------------------------
// OAuth Handlers
// ---------------------------------------------------------------------------

/// `POST /api/mcp/oauth/check-status` — check OAuth authentication status.
async fn oauth_check_status(
    State(state): State<McpRouterState>,
    body: Result<Json<OAuthCheckStatusRequest>, JsonRejection>,
) -> Result<Json<ApiResponse<OAuthStatusResponse>>, AppError> {
    let Json(req) = body.map_err(|e| AppError::BadRequest(e.to_string()))?;
    let status = state.oauth_service.check_oauth_status(&req.server_url).await?;
    Ok(Json(ApiResponse::ok(status)))
}

/// `POST /api/mcp/oauth/login` — start OAuth PKCE login flow.
///
/// Discovers endpoints, opens the browser for authorization, waits for
/// the callback, and exchanges the code for tokens.
async fn oauth_login(
    State(state): State<McpRouterState>,
    body: Result<Json<OAuthLoginRequest>, JsonRejection>,
) -> Result<Json<ApiResponse<OAuthLoginResponse>>, AppError> {
    let Json(req) = body.map_err(|e| AppError::BadRequest(e.to_string()))?;
    let result = state.oauth_service.login(&req.server_url).await?;
    Ok(Json(ApiResponse::ok(result)))
}

/// `POST /api/mcp/oauth/logout` — delete stored OAuth token.
async fn oauth_logout(
    State(state): State<McpRouterState>,
    body: Result<Json<OAuthLogoutRequest>, JsonRejection>,
) -> Result<Json<ApiResponse<()>>, AppError> {
    let Json(req) = body.map_err(|e| AppError::BadRequest(e.to_string()))?;
    state.oauth_service.logout(&req.server_url).await?;
    Ok(Json(ApiResponse::success()))
}

/// `GET /api/mcp/oauth/authenticated` — list server URLs with stored tokens.
async fn oauth_authenticated(State(state): State<McpRouterState>) -> Result<Json<ApiResponse<Vec<String>>>, AppError> {
    let urls = state.oauth_service.get_authenticated_servers().await?;
    Ok(Json(ApiResponse::ok(urls)))
}

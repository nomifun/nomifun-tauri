use axum::Router;
use axum::extract::rejection::JsonRejection;
use axum::extract::{Json, Path, Query, State};
use axum::http::StatusCode;
use axum::routing::{delete, get, post};
use std::path::PathBuf;

use nomifun_api_types::{
    ApiResponse, ClientPreferencesResponse, CreateProviderRequest, DetectProtocolRequest, FetchModelsAnonymousRequest,
    FetchModelsRequest, FetchModelsResponse, ProtocolDetectionResponse, ProviderResponse, SystemInfoResponse,
    SystemSettingsResponse, UpdateCheckRequest, UpdateCheckResult, UpdateClientPreferencesRequest,
    UpdateProviderRequest, UpdateSettingsRequest, UpdateWorkDirRequest,
};
use nomifun_common::AppError;

use crate::client_pref::ClientPrefService;
use crate::model_fetcher::ModelFetchService;
use crate::protocol::ProtocolDetectionService;
use crate::provider::ProviderService;
use crate::settings::SettingsService;
use crate::version::VersionCheckService;

/// Shared state for system route handlers.
#[derive(Clone)]
pub struct SystemRouterState {
    pub settings_service: SettingsService,
    pub client_pref_service: ClientPrefService,
    pub provider_service: ProviderService,
    pub model_fetch_service: ModelFetchService,
    pub protocol_detection_service: ProtocolDetectionService,
    pub version_check_service: VersionCheckService,
    /// Data directory root — used to arm a factory reset (write the marker that
    /// the next boot consumes). See `nomifun_common::factory_reset`.
    pub data_dir: PathBuf,
}

/// Build the system router (settings + client prefs + providers + system).
///
/// All routes require authentication (applied by the caller).
///
/// Endpoints:
/// - `GET  /api/settings`                    — get all backend settings
/// - `PATCH /api/settings`                   — partial update backend settings
/// - `GET  /api/settings/client`             — get client preferences
/// - `PUT  /api/settings/client`             — batch update client preferences
/// - `GET  /api/providers`                   — list all providers
/// - `POST /api/providers`                   — create a provider
/// - `PUT  /api/providers/:id`               — update a provider
/// - `DELETE /api/providers/:id`             — delete a provider
/// - `POST /api/providers/:id/models`        — fetch models from remote API
/// - `POST /api/providers/fetch-models`      — fetch models anonymously (pre-create preview)
/// - `POST /api/providers/detect-protocol`   — detect API protocol
/// - `GET  /api/system/info`                 — system directory & platform info
/// - `POST /api/system/check-update`         — check GitHub for new versions
/// - `POST /api/system/factory-reset`        — arm a factory reset (wipes on next boot)
/// - `POST /api/system/work-dir`             — persist the work dir (applies on next restart)
pub fn system_routes(state: SystemRouterState) -> Router {
    Router::new()
        .route("/api/settings", get(get_settings).patch(update_settings))
        .route(
            "/api/settings/client",
            get(get_client_preferences).put(update_client_preferences),
        )
        .route("/api/providers", get(list_providers).post(create_provider))
        // Literal-segment routes must register BEFORE the `/{id}` routes so
        // axum matches the literals instead of treating "detect-protocol" /
        // "fetch-models" as a provider id.
        .route("/api/providers/detect-protocol", post(detect_protocol))
        .route("/api/providers/fetch-models", post(fetch_models_anonymous))
        .route("/api/providers/{id}", delete(delete_provider).put(update_provider))
        .route("/api/providers/{id}/models", post(fetch_models))
        .route("/api/system/info", get(get_system_info))
        .route("/api/system/check-update", post(check_update))
        .route("/api/system/factory-reset", post(factory_reset))
        .route("/api/system/work-dir", post(set_work_dir))
        .with_state(state)
}

/// Backwards-compatible alias — delegates to `system_routes`.
pub fn settings_routes(state: SystemRouterState) -> Router {
    system_routes(state)
}

// ===========================================================================
// Settings handlers
// ===========================================================================

async fn get_settings(
    State(state): State<SystemRouterState>,
) -> Result<Json<ApiResponse<SystemSettingsResponse>>, AppError> {
    let settings = state.settings_service.get_settings().await?;
    Ok(Json(ApiResponse::ok(settings)))
}

async fn update_settings(
    State(state): State<SystemRouterState>,
    body: Result<Json<UpdateSettingsRequest>, JsonRejection>,
) -> Result<Json<ApiResponse<SystemSettingsResponse>>, AppError> {
    let Json(req) = body.map_err(|e| AppError::BadRequest(e.to_string()))?;
    let settings = state.settings_service.update_settings(req).await?;
    Ok(Json(ApiResponse::ok(settings)))
}

// ===========================================================================
// Client preferences handlers
// ===========================================================================

#[derive(Debug, serde::Deserialize, Default)]
struct ClientPrefQuery {
    keys: Option<String>,
}

async fn get_client_preferences(
    State(state): State<SystemRouterState>,
    Query(query): Query<ClientPrefQuery>,
) -> Result<Json<ApiResponse<ClientPreferencesResponse>>, AppError> {
    let keys_filter: Option<Vec<String>> = query.keys.map(|k| {
        k.split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect()
    });

    let key_refs: Option<Vec<&str>> = keys_filter.as_ref().map(|v| v.iter().map(|s| s.as_str()).collect());

    let prefs = state.client_pref_service.get_preferences(key_refs.as_deref()).await?;
    Ok(Json(ApiResponse::ok(prefs)))
}

async fn update_client_preferences(
    State(state): State<SystemRouterState>,
    body: Result<Json<UpdateClientPreferencesRequest>, JsonRejection>,
) -> Result<Json<ApiResponse<()>>, AppError> {
    let Json(req) = body.map_err(|e| AppError::BadRequest(e.to_string()))?;
    state.client_pref_service.update_preferences(req).await?;
    Ok(Json(ApiResponse::success()))
}

// ===========================================================================
// Provider handlers
// ===========================================================================

async fn list_providers(
    State(state): State<SystemRouterState>,
) -> Result<Json<ApiResponse<Vec<ProviderResponse>>>, AppError> {
    let providers = state.provider_service.list().await?;
    Ok(Json(ApiResponse::ok(providers)))
}

async fn create_provider(
    State(state): State<SystemRouterState>,
    body: Result<Json<CreateProviderRequest>, JsonRejection>,
) -> Result<(StatusCode, Json<ApiResponse<ProviderResponse>>), AppError> {
    let Json(req) = body.map_err(|e| AppError::BadRequest(e.to_string()))?;
    let provider = state.provider_service.create(req).await?;
    Ok((StatusCode::CREATED, Json(ApiResponse::ok(provider))))
}

async fn update_provider(
    State(state): State<SystemRouterState>,
    Path(id): Path<String>,
    body: Result<Json<UpdateProviderRequest>, JsonRejection>,
) -> Result<Json<ApiResponse<ProviderResponse>>, AppError> {
    let Json(req) = body.map_err(|e| AppError::BadRequest(e.to_string()))?;
    let provider = state.provider_service.update(&id, req).await?;
    Ok(Json(ApiResponse::ok(provider)))
}

async fn delete_provider(
    State(state): State<SystemRouterState>,
    Path(id): Path<String>,
) -> Result<Json<ApiResponse<()>>, AppError> {
    state.provider_service.delete(&id).await?;
    Ok(Json(ApiResponse::success()))
}

async fn fetch_models(
    State(state): State<SystemRouterState>,
    Path(id): Path<String>,
    body: Result<Json<FetchModelsRequest>, JsonRejection>,
) -> Result<Json<ApiResponse<FetchModelsResponse>>, AppError> {
    let Json(req) = body.map_err(|e| AppError::BadRequest(e.to_string()))?;
    let result = state.model_fetch_service.fetch_models(&id, &req).await?;
    Ok(Json(ApiResponse::ok(result)))
}

async fn fetch_models_anonymous(
    State(state): State<SystemRouterState>,
    body: Result<Json<FetchModelsAnonymousRequest>, JsonRejection>,
) -> Result<Json<ApiResponse<FetchModelsResponse>>, AppError> {
    let Json(req) = body.map_err(|e| AppError::BadRequest(e.to_string()))?;
    let result = state.model_fetch_service.fetch_models_anonymous(&req).await?;
    Ok(Json(ApiResponse::ok(result)))
}

async fn detect_protocol(
    State(state): State<SystemRouterState>,
    body: Result<Json<DetectProtocolRequest>, JsonRejection>,
) -> Result<Json<ApiResponse<ProtocolDetectionResponse>>, AppError> {
    let Json(req) = body.map_err(|e| AppError::BadRequest(e.to_string()))?;
    let result = state.protocol_detection_service.detect_protocol(&req).await?;
    Ok(Json(ApiResponse::ok(result)))
}

// ===========================================================================
// System info & version check handlers
// ===========================================================================

async fn get_system_info() -> Json<ApiResponse<SystemInfoResponse>> {
    let info = crate::sysinfo::get_system_info();
    Json(ApiResponse::ok(info))
}

async fn check_update(
    State(state): State<SystemRouterState>,
    body: Result<Json<UpdateCheckRequest>, JsonRejection>,
) -> Result<Json<ApiResponse<UpdateCheckResult>>, AppError> {
    let Json(req) = body.map_err(|e| AppError::BadRequest(e.to_string()))?;
    let result = state.version_check_service.check_update(&req).await?;
    Ok(Json(ApiResponse::ok(result)))
}

// ===========================================================================
// Factory reset handler
// ===========================================================================

/// Arm a factory reset: write the marker that the next boot consumes. The
/// actual database/derived-data wipe happens early on the next startup (see
/// `nomifun_common::factory_reset`); the client should restart the app right
/// after this returns. Nothing is deleted synchronously here — that would race
/// with the live connection pool and the background write loops.
async fn factory_reset(State(state): State<SystemRouterState>) -> Result<Json<ApiResponse<()>>, AppError> {
    let marker = nomifun_common::factory_reset::ResetMarker::new(nomifun_common::factory_reset::ResetScope::Full);
    nomifun_common::factory_reset::write_marker(&state.data_dir, &marker)?;
    tracing::warn!(target: "factory_reset", "factory reset armed — will wipe database and derived data on next restart");
    Ok(Json(ApiResponse::success()))
}

// ===========================================================================
// Work directory handler
// ===========================================================================

/// Persist the user-chosen working directory. Like factory reset, this only
/// takes effect on the *next* boot: the backend resolves `work_dir` (and injects
/// it into every service) before the HTTP server even exists, so the value
/// cannot change in the running process. The stored path is read early next boot
/// by `bootstrap::work_dir::resolve_work_dir` (see `nomifun_common::dir_config`).
/// The client should restart the app right after this returns.
///
/// The new path is validated to be a non-empty, absolute, creatable directory so
/// the next boot does not fail on an unusable value.
async fn set_work_dir(
    State(state): State<SystemRouterState>,
    body: Result<Json<UpdateWorkDirRequest>, JsonRejection>,
) -> Result<Json<ApiResponse<()>>, AppError> {
    let Json(req) = body.map_err(|e| AppError::BadRequest(e.to_string()))?;

    let trimmed = req.work_dir.trim();
    if trimmed.is_empty() {
        return Err(AppError::BadRequest("work_dir must not be empty".into()));
    }
    let path = PathBuf::from(trimmed);
    if !path.is_absolute() {
        return Err(AppError::BadRequest(format!("work_dir must be an absolute path: {trimmed}")));
    }
    // Reject paths with a leading/trailing-whitespace segment up front, with the
    // same dedicated error the conversation layer raises (service.rs) — otherwise
    // such a work_dir is accepted here only to make every later workspace
    // creation fail, and create_dir_all's behavior on these names is OS-specific.
    if nomifun_common::workspace_path_has_edge_whitespace_segment(&path) {
        return Err(AppError::WorkspacePathEdgeWhitespace(path.display().to_string()));
    }
    // Create it now so we (a) confirm the location is writable and (b) reject a
    // path that collides with an existing file — both would otherwise surface as
    // a confusing failure on the next boot.
    std::fs::create_dir_all(&path)
        .map_err(|e| AppError::BadRequest(format!("cannot use work_dir {}: {e}", path.display())))?;
    if !path.is_dir() {
        return Err(AppError::BadRequest(format!(
            "work_dir is not a directory: {}",
            path.display()
        )));
    }

    nomifun_common::dir_config::set_work_dir(&state.data_dir, &path)?;
    tracing::info!(
        target: "system",
        work_dir = %path.display(),
        "work dir override persisted — applies on next restart"
    );
    Ok(Json(ApiResponse::success()))
}

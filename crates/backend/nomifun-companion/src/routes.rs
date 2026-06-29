//! `/api/companion/*` route handlers.

use axum::Router;
use axum::body::Body;
use axum::extract::rejection::JsonRejection;
use axum::extract::{Extension, Json, Path, Query, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post};

use nomifun_api_types::ApiResponse;
use nomifun_auth::CurrentUser;
use nomifun_common::AppError;
use serde::Deserialize;

use crate::profile::{HeadBox, CompanionProfileConfig, SharedCompanionConfig};
use crate::service::{CompanionSkillContent, CompanionSkillView, CompanionStatus, CompanionWeeklyDigest, SourceStats};
use crate::state::CompanionRouterState;
use crate::store::{MemoryFilter, MemoryScope, CompanionLearnRun, CompanionMemory, CompanionSkill, CompanionSuggestion};

pub fn companion_routes(state: CompanionRouterState) -> Router {
    Router::new()
        .route("/api/companion/config", get(get_config).put(update_config).patch(patch_config))
        .route("/api/companion/status", get(status))
        .route("/api/companion/companions", get(list_companions).post(create_companion))
        .route(
            "/api/companion/companions/{companion_id}",
            get(get_companion).patch(patch_companion).delete(delete_companion),
        )
        .route("/api/companion/companions/{companion_id}/status", get(companion_status))
        .route("/api/companion/companions/{companion_id}/figure", post(upload_figure).get(get_figure))
        .route("/api/companion/matting-model", get(get_matting_model))
        .route("/api/companion/figures", get(list_figures).post(create_figure))
        .route(
            "/api/companion/figures/{figure_id}",
            axum::routing::patch(update_figure).delete(delete_figure),
        )
        .route(
            "/api/companion/companions/{companion_id}/companion/threads",
            post(create_thread),
        )
        .route("/api/companion/companions/{companion_id}/companion/active", get(get_active_thread))
        .route("/api/companion/memories", get(list_memories).post(add_memory))
        .route("/api/companion/memories/{id}", axum::routing::put(update_memory).delete(delete_memory))
        .route("/api/companion/suggestions", get(list_suggestions))
        .route("/api/companion/suggestions/{id}/decide", post(decide_suggestion))
        .route("/api/companion/companions/{companion_id}/skills", get(list_companion_skills))
        .route("/api/companion/companions/{companion_id}/weekly-digest", get(weekly_digest))
        .route(
            "/api/companion/companions/{companion_id}/skills/{name}",
            get(get_companion_skill).put(update_companion_skill),
        )
        .route(
            "/api/companion/companions/{companion_id}/skills/{name}/decide",
            post(decide_companion_skill),
        )
        .route(
            "/api/companion/companions/{companion_id}/skills/from-session",
            post(draft_skill_from_session),
        )
        .route(
            "/api/companion/companions/{companion_id}/skills/{name}/gift",
            post(gift_companion_skill),
        )
        .route("/api/companion/learn/run", post(run_learn))
        .route("/api/companion/learn/runs", get(list_learn_runs))
        .route("/api/companion/events/stats", get(event_stats))
        .route("/api/companion/events/recent", get(recent_events))
        .route("/api/companion/events", delete(clear_events))
        .route("/api/companion/consent", post(apply_consent))
        .route("/api/companion/disable-all", post(disable_all))
        .route("/api/companion/export/memory", post(export_memory))
        .route("/api/companion/export/companions/{companion_id}", post(export_companion))
        .route("/api/companion/import", post(import_package))
        .with_state(state)
}

/// Public (auth-exempt) figure-image serving.
///
/// `<img>` / `new Image()` are browser-native subresource loads with no
/// custom-header API, so under the desktop's `TrustLocalToken` policy they
/// cannot present the `x-nomi-local-trust` header — the authenticated router
/// would 403 every figure thumbnail (broken library image + blank desktop
/// companion mesh). This GET-only route therefore lives outside auth, exactly
/// like `asset_routes` (logos) and the office proxy. Figure ids are unguessable
/// (`figure_<uuidv7>`) and listing/creation/rename/delete stay authenticated,
/// so this only serves opaque-id image bytes — a capability URL, not an
/// enumeration surface.
pub fn companion_public_routes(state: CompanionRouterState) -> Router {
    Router::new()
        .route("/api/companion/figures/{figure_id}/image", get(get_figure_image))
        .with_state(state)
}

async fn get_config(
    State(state): State<CompanionRouterState>,
    Extension(_user): Extension<CurrentUser>,
) -> Result<Json<ApiResponse<SharedCompanionConfig>>, AppError> {
    Ok(Json(ApiResponse::ok(state.service.get_config().await)))
}

async fn update_config(
    State(state): State<CompanionRouterState>,
    Extension(_user): Extension<CurrentUser>,
    body: Result<Json<SharedCompanionConfig>, JsonRejection>,
) -> Result<Json<ApiResponse<SharedCompanionConfig>>, AppError> {
    let Json(config) = body.map_err(|e| AppError::BadRequest(e.to_string()))?;
    Ok(Json(ApiResponse::ok(state.service.update_config(config).await?)))
}

async fn patch_config(
    State(state): State<CompanionRouterState>,
    Extension(_user): Extension<CurrentUser>,
    body: Result<Json<serde_json::Value>, JsonRejection>,
) -> Result<Json<ApiResponse<SharedCompanionConfig>>, AppError> {
    let Json(patch) = body.map_err(|e| AppError::BadRequest(e.to_string()))?;
    Ok(Json(ApiResponse::ok(state.service.patch_config(patch).await?)))
}

async fn status(
    State(state): State<CompanionRouterState>,
    Extension(_user): Extension<CurrentUser>,
) -> Result<Json<ApiResponse<CompanionStatus>>, AppError> {
    Ok(Json(ApiResponse::ok(state.service.status().await?)))
}

/// Build an optional [`MemoryScope`] from wire parts.
/// - `scope_kind = Some("companion")` with a non-empty id → private to it.
/// - `scope_kind = Some(_other)` → Shared.
/// - `scope_kind = None` → `None` (leave unchanged on update / default on add).
fn scope_from_parts(scope_kind: Option<&str>, scope_companion_id: Option<&str>) -> Option<MemoryScope> {
    let kind = scope_kind?;
    let cid = scope_companion_id.unwrap_or("").trim();
    if kind == "companion" && !cid.is_empty() {
        Some(MemoryScope::Companion(cid.to_owned()))
    } else {
        Some(MemoryScope::Shared)
    }
}

#[derive(Deserialize)]
struct ListMemoriesQuery {
    kind: Option<String>,
    q: Option<String>,
    status: Option<String>,
    /// When set, scope the list to memories visible to this companion (shared +
    /// its own private). Empty/absent = cross-companion "all" view.
    scope_companion_id: Option<String>,
    limit: Option<i64>,
    offset: Option<i64>,
}

async fn list_memories(
    State(state): State<CompanionRouterState>,
    Extension(_user): Extension<CurrentUser>,
    Query(query): Query<ListMemoriesQuery>,
) -> Result<Json<ApiResponse<Vec<CompanionMemory>>>, AppError> {
    let filter = MemoryFilter {
        kind: query.kind.filter(|k| !k.is_empty()),
        q: query.q.filter(|q| !q.is_empty()),
        status: Some(query.status.filter(|s| !s.is_empty()).unwrap_or_else(|| "active".into())),
        scope_companion_id: query.scope_companion_id.filter(|s| !s.is_empty()),
        limit: query.limit.unwrap_or(100),
        offset: query.offset.unwrap_or(0),
    };
    Ok(Json(ApiResponse::ok(state.service.list_memories(&filter).await?)))
}

#[derive(Deserialize)]
struct AddMemoryRequest {
    kind: String,
    content: String,
    #[serde(default)]
    tags: Vec<String>,
    /// Owning companion for a private memory; empty/absent = shared.
    #[serde(default)]
    scope_companion_id: Option<String>,
}

async fn add_memory(
    State(state): State<CompanionRouterState>,
    Extension(_user): Extension<CurrentUser>,
    body: Result<Json<AddMemoryRequest>, JsonRejection>,
) -> Result<Json<ApiResponse<CompanionMemory>>, AppError> {
    let Json(req) = body.map_err(|e| AppError::BadRequest(e.to_string()))?;
    let scope = scope_from_parts(Some("companion"), req.scope_companion_id.as_deref()).unwrap_or(MemoryScope::Shared);
    Ok(Json(ApiResponse::ok(
        state.service.add_memory(&req.kind, &req.content, &req.tags, scope).await?,
    )))
}

#[derive(Deserialize)]
struct UpdateMemoryRequest {
    content: Option<String>,
    pinned: Option<bool>,
    status: Option<String>,
    /// `'user'` (shared) or `'companion'` (private). Present together with
    /// `scope_companion_id` to re-home a memory; both absent = scope unchanged.
    scope_kind: Option<String>,
    scope_companion_id: Option<String>,
}

async fn update_memory(
    State(state): State<CompanionRouterState>,
    Extension(_user): Extension<CurrentUser>,
    Path(id): Path<String>,
    body: Result<Json<UpdateMemoryRequest>, JsonRejection>,
) -> Result<Json<ApiResponse<()>>, AppError> {
    let Json(req) = body.map_err(|e| AppError::BadRequest(e.to_string()))?;
    let scope = scope_from_parts(req.scope_kind.as_deref(), req.scope_companion_id.as_deref());
    state
        .service
        .update_memory(&id, req.content.as_deref(), req.pinned, req.status.as_deref(), scope)
        .await?;
    Ok(Json(ApiResponse::ok(())))
}

async fn delete_memory(
    State(state): State<CompanionRouterState>,
    Extension(_user): Extension<CurrentUser>,
    Path(id): Path<String>,
) -> Result<Json<ApiResponse<()>>, AppError> {
    state.service.delete_memory(&id).await?;
    Ok(Json(ApiResponse::ok(())))
}

#[derive(Deserialize)]
struct ListSuggestionsQuery {
    status: Option<String>,
    limit: Option<i64>,
}

async fn list_suggestions(
    State(state): State<CompanionRouterState>,
    Extension(_user): Extension<CurrentUser>,
    Query(query): Query<ListSuggestionsQuery>,
) -> Result<Json<ApiResponse<Vec<CompanionSuggestion>>>, AppError> {
    let status = query.status.filter(|s| !s.is_empty());
    Ok(Json(ApiResponse::ok(
        state
            .service
            .list_suggestions(status.as_deref(), query.limit.unwrap_or(100))
            .await?,
    )))
}

#[derive(Deserialize)]
struct DecideSuggestionRequest {
    accept: bool,
}

async fn decide_suggestion(
    State(state): State<CompanionRouterState>,
    Extension(_user): Extension<CurrentUser>,
    Path(id): Path<String>,
    body: Result<Json<DecideSuggestionRequest>, JsonRejection>,
) -> Result<Json<ApiResponse<CompanionSuggestion>>, AppError> {
    let Json(req) = body.map_err(|e| AppError::BadRequest(e.to_string()))?;
    Ok(Json(ApiResponse::ok(state.service.decide_suggestion(&id, req.accept).await?)))
}

#[derive(Deserialize)]
struct ListSkillsQuery {
    include_shared: Option<bool>,
}

async fn list_companion_skills(
    State(state): State<CompanionRouterState>,
    Extension(_user): Extension<CurrentUser>,
    Path(companion_id): Path<String>,
    Query(q): Query<ListSkillsQuery>,
) -> Result<Json<ApiResponse<Vec<CompanionSkillView>>>, AppError> {
    let views = state
        .service
        .list_companion_skills(&companion_id, q.include_shared.unwrap_or(true))
        .await?;
    Ok(Json(ApiResponse::ok(views)))
}

#[derive(Deserialize)]
struct DigestQuery {
    days: Option<i64>,
}

async fn weekly_digest(
    State(state): State<CompanionRouterState>,
    Extension(_user): Extension<CurrentUser>,
    Path(companion_id): Path<String>,
    Query(q): Query<DigestQuery>,
) -> Result<Json<ApiResponse<CompanionWeeklyDigest>>, AppError> {
    let days = q.days.unwrap_or(7).clamp(1, 90);
    let since_ms = nomifun_common::now_ms() - days * 86_400_000;
    Ok(Json(ApiResponse::ok(state.service.weekly_digest(&companion_id, since_ms).await?)))
}

async fn get_companion_skill(
    State(state): State<CompanionRouterState>,
    Extension(_user): Extension<CurrentUser>,
    Path((companion_id, name)): Path<(String, String)>,
) -> Result<Json<ApiResponse<CompanionSkillContent>>, AppError> {
    Ok(Json(ApiResponse::ok(state.service.get_companion_skill_content(&companion_id, &name).await?)))
}

#[derive(Deserialize)]
struct UpdateSkillRequest {
    content: String,
}

async fn update_companion_skill(
    State(state): State<CompanionRouterState>,
    Extension(_user): Extension<CurrentUser>,
    Path((companion_id, name)): Path<(String, String)>,
    body: Result<Json<UpdateSkillRequest>, JsonRejection>,
) -> Result<Json<ApiResponse<()>>, AppError> {
    let Json(req) = body.map_err(|e| AppError::BadRequest(e.to_string()))?;
    state.service.write_companion_skill_content(&companion_id, &name, &req.content).await?;
    Ok(Json(ApiResponse::ok(())))
}

#[derive(Deserialize)]
struct DecideSkillRequest {
    accept: bool,
    reason: Option<String>,
}

async fn decide_companion_skill(
    State(state): State<CompanionRouterState>,
    Extension(_user): Extension<CurrentUser>,
    Path((companion_id, name)): Path<(String, String)>,
    body: Result<Json<DecideSkillRequest>, JsonRejection>,
) -> Result<Json<ApiResponse<CompanionSkill>>, AppError> {
    let Json(req) = body.map_err(|e| AppError::BadRequest(e.to_string()))?;
    Ok(Json(ApiResponse::ok(
        state
            .service
            .decide_companion_skill(&companion_id, &name, req.accept, req.reason.as_deref())
            .await?,
    )))
}

#[derive(Deserialize)]
struct FromSessionRequest {
    conversation_id: String,
}

async fn draft_skill_from_session(
    State(state): State<CompanionRouterState>,
    Extension(_user): Extension<CurrentUser>,
    Path(companion_id): Path<String>,
    body: Result<Json<FromSessionRequest>, JsonRejection>,
) -> Result<Json<ApiResponse<Option<String>>>, AppError> {
    let Json(req) = body.map_err(|e| AppError::BadRequest(e.to_string()))?;
    Ok(Json(ApiResponse::ok(
        state.service.draft_skill_from_session(&companion_id, &req.conversation_id).await?,
    )))
}

#[derive(Deserialize)]
struct GiftSkillRequest {
    to_companion_id: String,
}

async fn gift_companion_skill(
    State(state): State<CompanionRouterState>,
    Extension(_user): Extension<CurrentUser>,
    Path((companion_id, name)): Path<(String, String)>,
    body: Result<Json<GiftSkillRequest>, JsonRejection>,
) -> Result<Json<ApiResponse<CompanionSkill>>, AppError> {
    let Json(req) = body.map_err(|e| AppError::BadRequest(e.to_string()))?;
    Ok(Json(ApiResponse::ok(
        state.service.gift_companion_skill(&companion_id, &name, &req.to_companion_id).await?,
    )))
}

async fn run_learn(
    State(state): State<CompanionRouterState>,
    Extension(_user): Extension<CurrentUser>,
) -> Result<Json<ApiResponse<CompanionLearnRun>>, AppError> {
    Ok(Json(ApiResponse::ok(state.service.run_learn_now().await?)))
}

#[derive(Deserialize)]
struct LimitQuery {
    limit: Option<i64>,
}

async fn list_learn_runs(
    State(state): State<CompanionRouterState>,
    Extension(_user): Extension<CurrentUser>,
    Query(query): Query<LimitQuery>,
) -> Result<Json<ApiResponse<Vec<CompanionLearnRun>>>, AppError> {
    Ok(Json(ApiResponse::ok(
        state.service.list_learn_runs(query.limit.unwrap_or(30)).await?,
    )))
}

async fn event_stats(
    State(state): State<CompanionRouterState>,
    Extension(_user): Extension<CurrentUser>,
) -> Result<Json<ApiResponse<Vec<SourceStats>>>, AppError> {
    Ok(Json(ApiResponse::ok(state.service.event_stats())))
}

async fn recent_events(
    State(state): State<CompanionRouterState>,
    Extension(_user): Extension<CurrentUser>,
    Query(query): Query<LimitQuery>,
) -> Result<Json<ApiResponse<Vec<crate::collector::CollectedEvent>>>, AppError> {
    let limit = query.limit.unwrap_or(100).clamp(1, 500) as usize;
    Ok(Json(ApiResponse::ok(state.service.recent_events(limit))))
}

async fn apply_consent(
    State(state): State<CompanionRouterState>,
    Extension(_user): Extension<CurrentUser>,
) -> Result<Json<ApiResponse<SharedCompanionConfig>>, AppError> {
    Ok(Json(ApiResponse::ok(state.service.apply_default_on_consent().await?)))
}

async fn disable_all(
    State(state): State<CompanionRouterState>,
    Extension(_user): Extension<CurrentUser>,
) -> Result<Json<ApiResponse<SharedCompanionConfig>>, AppError> {
    Ok(Json(ApiResponse::ok(state.service.disable_all().await?)))
}

// ----- companions -----

/// One companion card: profile fields flattened at the top level plus that companion's
/// live status — list/detail fetch everything for a card in one round trip.
#[derive(serde::Serialize)]
struct CompanionWithStatus {
    #[serde(flatten)]
    profile: CompanionProfileConfig,
    status: CompanionStatus,
}

async fn list_companions(
    State(state): State<CompanionRouterState>,
    Extension(_user): Extension<CurrentUser>,
) -> Result<Json<ApiResponse<Vec<CompanionWithStatus>>>, AppError> {
    let mut companions = Vec::new();
    for profile in state.service.list_companions().await {
        match state.service.companion_status(&profile.id).await {
            Ok(status) => companions.push(CompanionWithStatus { profile, status }),
            // The companion vanished between list and status (concurrent delete):
            // drop the card rather than failing the whole list.
            Err(AppError::NotFound(_)) => {}
            Err(e) => return Err(e),
        }
    }
    Ok(Json(ApiResponse::ok(companions)))
}

#[derive(Deserialize)]
struct CreateCompanionRequest {
    name: String,
    /// Empty/missing falls back to the default roster character.
    #[serde(default)]
    character: String,
}

async fn create_companion(
    State(state): State<CompanionRouterState>,
    Extension(_user): Extension<CurrentUser>,
    body: Result<Json<CreateCompanionRequest>, JsonRejection>,
) -> Result<impl IntoResponse, AppError> {
    let Json(req) = body.map_err(|e| AppError::BadRequest(e.to_string()))?;
    let profile = state.service.create_companion(&req.name, &req.character).await?;
    Ok((StatusCode::CREATED, Json(ApiResponse::ok(profile))))
}

async fn get_companion(
    State(state): State<CompanionRouterState>,
    Extension(_user): Extension<CurrentUser>,
    Path(companion_id): Path<String>,
) -> Result<Json<ApiResponse<CompanionWithStatus>>, AppError> {
    let profile = state.service.get_companion(&companion_id).await?;
    let status = state.service.companion_status(&companion_id).await?;
    Ok(Json(ApiResponse::ok(CompanionWithStatus { profile, status })))
}

/// RFC 7396 merge patch over one companion's profile.
async fn patch_companion(
    State(state): State<CompanionRouterState>,
    Extension(_user): Extension<CurrentUser>,
    Path(companion_id): Path<String>,
    body: Result<Json<serde_json::Value>, JsonRejection>,
) -> Result<Json<ApiResponse<CompanionProfileConfig>>, AppError> {
    let Json(patch) = body.map_err(|e| AppError::BadRequest(e.to_string()))?;
    Ok(Json(ApiResponse::ok(state.service.patch_companion(&companion_id, patch).await?)))
}

async fn delete_companion(
    State(state): State<CompanionRouterState>,
    Extension(_user): Extension<CurrentUser>,
    Path(companion_id): Path<String>,
) -> Result<StatusCode, AppError> {
    state.service.delete_companion(&companion_id).await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn companion_status(
    State(state): State<CompanionRouterState>,
    Extension(_user): Extension<CurrentUser>,
    Path(companion_id): Path<String>,
) -> Result<Json<ApiResponse<CompanionStatus>>, AppError> {
    Ok(Json(ApiResponse::ok(state.service.companion_status(&companion_id).await?)))
}

// ----- DIY custom figure (spec §3 存储与回显) -----

#[derive(Deserialize)]
struct UploadFigureRequest {
    /// Temp path returned by `POST /api/fs/upload` (two-phase upload).
    source_path: String,
}

async fn upload_figure(
    State(state): State<CompanionRouterState>,
    Extension(_user): Extension<CurrentUser>,
    Path(companion_id): Path<String>,
    body: Result<Json<UploadFigureRequest>, JsonRejection>,
) -> Result<Json<ApiResponse<()>>, AppError> {
    let Json(req) = body.map_err(|e| AppError::BadRequest(e.to_string()))?;
    state.service.ingest_figure(&companion_id, &req.source_path).await?;
    Ok(Json(ApiResponse::ok(())))
}

/// Binary serve of one companion's figure (the nomifun-assets Response template,
/// disk-backed). `Cache-Control: no-cache` + a `"{mtime}-{len}"` ETag: the
/// browser revalidates every time and gets a cheap 304 until re-upload.
async fn get_figure(
    State(state): State<CompanionRouterState>,
    Extension(_user): Extension<CurrentUser>,
    Path(companion_id): Path<String>,
    headers: HeaderMap,
) -> Result<Response, AppError> {
    let (bytes, mtime) = state.service.read_figure(&companion_id).await?;
    let etag = format!("\"{}-{}\"", mtime, bytes.len());

    let if_none_match_hits = headers
        .get(header::IF_NONE_MATCH)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.split(',').map(str::trim).any(|c| c == etag || c == "*"));
    if if_none_match_hits {
        return Response::builder()
            .status(StatusCode::NOT_MODIFIED)
            .header(header::CACHE_CONTROL, "no-cache")
            .header(header::ETAG, etag)
            .body(Body::empty())
            .map_err(|e| AppError::Internal(e.to_string()));
    }

    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, crate::figure::content_type_of(&bytes))
        .header(header::CACHE_CONTROL, "no-cache")
        .header(header::ETAG, etag)
        .body(Body::from(bytes))
        .map_err(|e| AppError::Internal(e.to_string()))
}

/// Binary serve of the cached MODNet matting model, downloading it from a
/// mirror on first use (see [`crate::matting_model`]). The renderer fetches
/// this from `127.0.0.1` and mirrors it into Cache Storage, so the matting
/// Web Worker reads a local copy instead of hitting huggingface behind a 30 s
/// timeout. Immutable + long-lived: the filename is versioned, so the browser
/// may cache it forever.
async fn get_matting_model(
    State(state): State<CompanionRouterState>,
    Extension(_user): Extension<CurrentUser>,
) -> Result<Response, AppError> {
    let bytes = state.service.matting_model_bytes().await?;
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "application/octet-stream")
        .header(header::CACHE_CONTROL, "public, max-age=31536000, immutable")
        .body(Body::from(bytes))
        .map_err(|e| AppError::Internal(e.to_string()))
}

// ----- custom-figure library (decoupled from companions) -----

async fn list_figures(
    State(state): State<CompanionRouterState>,
    Extension(_user): Extension<CurrentUser>,
) -> Result<Json<ApiResponse<Vec<crate::figures::FigureMeta>>>, AppError> {
    Ok(Json(ApiResponse::ok(state.service.list_figures().await)))
}

#[derive(Deserialize)]
struct CreateFigureRequest {
    /// Temp path returned by `POST /api/fs/upload` (two-phase upload).
    source_path: String,
    #[serde(default)]
    name: String,
    aspect: f32,
    head_box: HeadBox,
    #[serde(default)]
    size_tier: String,
}

async fn create_figure(
    State(state): State<CompanionRouterState>,
    Extension(_user): Extension<CurrentUser>,
    body: Result<Json<CreateFigureRequest>, JsonRejection>,
) -> Result<impl IntoResponse, AppError> {
    let Json(req) = body.map_err(|e| AppError::BadRequest(e.to_string()))?;
    let figure = state
        .service
        .create_figure(&req.source_path, &req.name, req.aspect, req.head_box, &req.size_tier)
        .await?;
    Ok((StatusCode::CREATED, Json(ApiResponse::ok(figure))))
}

#[derive(Deserialize)]
struct UpdateFigureRequest {
    name: Option<String>,
    head_box: Option<HeadBox>,
    size_tier: Option<String>,
}

async fn update_figure(
    State(state): State<CompanionRouterState>,
    Extension(_user): Extension<CurrentUser>,
    Path(figure_id): Path<String>,
    body: Result<Json<UpdateFigureRequest>, JsonRejection>,
) -> Result<Json<ApiResponse<crate::figures::FigureMeta>>, AppError> {
    let Json(req) = body.map_err(|e| AppError::BadRequest(e.to_string()))?;
    Ok(Json(ApiResponse::ok(state.service.update_figure(
        &figure_id,
        crate::figures::FigureUpdate { name: req.name, head_box: req.head_box, size_tier: req.size_tier },
    ).await?)))
}

async fn delete_figure(
    State(state): State<CompanionRouterState>,
    Extension(_user): Extension<CurrentUser>,
    Path(figure_id): Path<String>,
) -> Result<StatusCode, AppError> {
    state.service.delete_figure(&figure_id).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// Binary serve of one library figure's image (same ETag/no-cache template as
/// the per-companion `get_figure`).
///
/// AUTH-EXEMPT route (see `companion_public_routes`): native `<img>` loads carry
/// no trust header, so `trust_resolve_middleware` injects NO `CurrentUser` for
/// them. This handler therefore MUST NOT extract `Extension<CurrentUser>` — that
/// extractor would 500 on the very (untrusted-header) requests this route exists
/// to serve. The figure id is the opaque capability; no user identity is needed.
async fn get_figure_image(
    State(state): State<CompanionRouterState>,
    Path(figure_id): Path<String>,
    headers: HeaderMap,
) -> Result<Response, AppError> {
    let (bytes, mtime) = state.service.read_figure_image(&figure_id).await?;
    let etag = format!("\"{}-{}\"", mtime, bytes.len());

    let if_none_match_hits = headers
        .get(header::IF_NONE_MATCH)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.split(',').map(str::trim).any(|c| c == etag || c == "*"));
    if if_none_match_hits {
        return Response::builder()
            .status(StatusCode::NOT_MODIFIED)
            .header(header::CACHE_CONTROL, "no-cache")
            .header(header::ETAG, etag)
            .body(Body::empty())
            .map_err(|e| AppError::Internal(e.to_string()));
    }

    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, crate::figure::content_type_of(&bytes))
        .header(header::CACHE_CONTROL, "no-cache")
        .header(header::ETAG, etag)
        .body(Body::from(bytes))
        .map_err(|e| AppError::Internal(e.to_string()))
}

// ----- companion thread (per companion, single session) -----

#[derive(Deserialize)]
struct CreateThreadRequest {
    #[serde(default)]
    title: Option<String>,
}

/// Idempotent ensure of the companion's single companion session: returns the
/// existing one, or creates it (requires the companion's model to be configured).
async fn create_thread(
    State(state): State<CompanionRouterState>,
    Extension(_user): Extension<CurrentUser>,
    Path(companion_id): Path<String>,
    body: Result<Json<CreateThreadRequest>, JsonRejection>,
) -> Result<Json<ApiResponse<crate::store::CompanionThread>>, AppError> {
    let title = body.map(|Json(b)| b.title).unwrap_or_default();
    Ok(Json(ApiResponse::ok(
        state.service.create_companion_thread(&companion_id, title).await?,
    )))
}

#[derive(serde::Serialize)]
struct ActiveThreadResponse {
    conversation_id: Option<String>,
}

/// The companion's single companion session id (or null when none exists yet).
async fn get_active_thread(
    State(state): State<CompanionRouterState>,
    Extension(_user): Extension<CurrentUser>,
    Path(companion_id): Path<String>,
) -> Result<Json<ApiResponse<ActiveThreadResponse>>, AppError> {
    // Existence gate: an unknown companion must 404, not read as "no active thread".
    state.service.get_companion(&companion_id).await?;
    Ok(Json(ApiResponse::ok(ActiveThreadResponse {
        conversation_id: state.service.companion_active_thread(&companion_id).await?,
    })))
}

async fn clear_events(
    State(state): State<CompanionRouterState>,
    Extension(_user): Extension<CurrentUser>,
) -> Result<Json<ApiResponse<()>>, AppError> {
    state.service.clear_events()?;
    Ok(Json(ApiResponse::ok(())))
}

// ----- export / import (§4.8 migration) -----

/// The live shared store + shared dir for export/import. `CompanionService` keeps
/// its store private, so the boot-time registration in `crate::store` is the
/// only crate-visible handle. `None` means boot fell back to the in-memory
/// store (corrupt/locked memory.db) — exporting that throwaway snapshot would
/// silently lose the on-disk data, so the endpoints refuse instead.
fn live_store() -> Result<(&'static std::path::Path, &'static crate::store::CompanionStore), AppError> {
    crate::store::live_store()
        .ok_or_else(|| AppError::Internal("伙伴存储当前处于内存降级模式，无法导入导出".into()))
}

#[derive(Deserialize)]
struct ExportMemoryRequest {
    dest_path: String,
    #[serde(default)]
    include_events: bool,
}

async fn export_memory(
    State(_state): State<CompanionRouterState>,
    Extension(_user): Extension<CurrentUser>,
    body: Result<Json<ExportMemoryRequest>, JsonRejection>,
) -> Result<Json<ApiResponse<crate::export::ExportSummary>>, AppError> {
    let Json(req) = body.map_err(|e| AppError::BadRequest(e.to_string()))?;
    let (shared_dir, store) = live_store()?;
    let summary = crate::export::export_memory_bundle(
        store,
        shared_dir,
        std::path::Path::new(&req.dest_path),
        req.include_events,
    )
    .await?;
    Ok(Json(ApiResponse::ok(summary)))
}

#[derive(Deserialize)]
struct ExportCompanionRequest {
    dest_path: String,
    /// Names of the knowledge bases bound to this companion, collected by the
    /// frontend (the companion crate never reaches into the knowledge domain).
    #[serde(default)]
    knowledge_names: Vec<String>,
}

async fn export_companion(
    State(state): State<CompanionRouterState>,
    Extension(_user): Extension<CurrentUser>,
    Path(companion_id): Path<String>,
    body: Result<Json<ExportCompanionRequest>, JsonRejection>,
) -> Result<Json<ApiResponse<crate::export::ExportSummary>>, AppError> {
    let Json(req) = body.map_err(|e| AppError::BadRequest(e.to_string()))?;
    // Existence gate: an unknown companion must 404 before any file is written.
    let profile = state.service.get_companion(&companion_id).await?;
    let (_, store) = live_store()?;
    let summary = crate::export::export_companion_bundle(
        store,
        &profile,
        std::path::Path::new(&req.dest_path),
        &req.knowledge_names,
    )
    .await?;
    Ok(Json(ApiResponse::ok(summary)))
}

#[derive(Deserialize)]
struct ImportPackageRequest {
    src_path: String,
}

async fn import_package(
    State(state): State<CompanionRouterState>,
    Extension(_user): Extension<CurrentUser>,
    body: Result<Json<ImportPackageRequest>, JsonRejection>,
) -> Result<Json<ApiResponse<crate::export::ImportOutcome>>, AppError> {
    let Json(req) = body.map_err(|e| AppError::BadRequest(e.to_string()))?;
    let (shared_dir, store) = live_store()?;
    let outcome = crate::export::import_bundle(
        store,
        state.service.as_ref(),
        shared_dir,
        std::path::Path::new(&req.src_path),
    )
    .await?;
    Ok(Json(ApiResponse::ok(outcome)))
}

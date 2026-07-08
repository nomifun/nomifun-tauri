//! `/api/workshop/*` route handlers (contract §3.1/§3.2). The management surface
//! (list/create/patch/delete, doc read/write, upload, export/import, gc,
//! agent-ops) is owner-only — mounted behind the app's authenticated router
//! (same auth extractor as the knowledge routes). The multipart upload route
//! raises the body limit to [`MAX_ASSET_BYTES`]; every other route rides the
//! app's default limit.
//!
//! The two **read-only binary serve** routes ([`serve_file`] +
//! [`serve_canvas_thumb`]) instead live on the auth-EXEMPT public router
//! ([`workshop_public_routes`]): `<img>` / `<video>` / `new Image()` loads carry
//! no custom-header API, so under the desktop's `TrustLocalToken` policy they
//! cannot present the `x-nomi-local-trust` header — the authenticated router
//! would 403 every asset thumbnail and canvas gallery image. They are GET-only,
//! serve opaque unguessable ids (`wsa_`/`wsc_` + uuidv7 — a capability URL, not
//! an enumeration surface), keep the service's traversal sandbox, and never
//! extract `CurrentUser` (see the note on the public router).

use axum::Router;
use axum::body::Body;
use axum::extract::rejection::JsonRejection;
use axum::extract::{DefaultBodyLimit, Extension, Json, Multipart, Path, Query, State};
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use serde::Deserialize;
use serde_json::Value;
use tower_http::limit::RequestBodyLimitLayer;

use nomifun_api_types::ApiResponse;
use nomifun_auth::CurrentUser;
use nomifun_common::AppError;

use crate::MAX_ASSET_BYTES;
use crate::agent_ops::PendingOp;
use crate::dto::{WorkshopAsset, WorkshopCanvasMeta};
use crate::service::{AssetPatch, AssetQuery, NewAssetUpload, NewTextAsset};
use crate::state::WorkshopRouterState;

pub fn workshop_routes(state: WorkshopRouterState) -> Router {
    // The asset upload + canvas import routes carry their own (larger) body
    // limit. Disable the app's global `DefaultBodyLimit` on them, then cap at
    // MAX_ASSET_BYTES.
    let upload_router = Router::new()
        .route("/api/workshop/assets/upload", post(upload_asset))
        .route("/api/workshop/canvases/import", post(import_canvas))
        .layer(DefaultBodyLimit::disable())
        .layer(RequestBodyLimitLayer::new(MAX_ASSET_BYTES))
        .with_state(state.clone());

    Router::new()
        .route("/api/workshop/canvases", get(list_canvases).post(create_canvas))
        .route(
            "/api/workshop/canvases/{id}",
            get(get_canvas).patch(patch_canvas).delete(delete_canvas),
        )
        .route("/api/workshop/canvases/{id}/doc", axum::routing::put(put_doc))
        .route("/api/workshop/canvases/{id}/export", get(export_canvas))
        .route(
            "/api/workshop/canvases/{id}/pending-ops",
            get(get_pending_ops),
        )
        .route(
            "/api/workshop/canvases/{id}/pending-ops/ack",
            post(ack_pending_ops),
        )
        .route("/api/workshop/gc", post(run_gc))
        .route("/api/workshop/assets", get(list_assets).post(create_text_asset))
        .route("/api/workshop/assets/{id}", axum::routing::patch(patch_asset).delete(delete_asset))
        .route("/api/workshop/collections/rename", post(rename_collection))
        .with_state(state)
        .merge(upload_router)
}

/// Auth-EXEMPT read-only binary serve routes (see the module doc). GET-only, two
/// prefixes only, opaque unguessable ids. Merged into the app's public router
/// next to the other auth-exempt serve routes (logos / office proxy / companion
/// figure images). Every write / list / delete route stays under auth in
/// [`workshop_routes`].
///
/// These handlers MUST NOT extract `Extension<CurrentUser>`: `<img>`/`<video>`
/// loads carry no trust header, so `trust_resolve_middleware` injects no
/// `CurrentUser` and that extractor would 500 the very requests this router
/// exists to serve.
pub fn workshop_public_routes(state: WorkshopRouterState) -> Router {
    Router::new()
        .route("/api/workshop/files/{asset_id}", get(serve_file))
        .route("/api/workshop/canvas-thumbs/{id}", get(serve_canvas_thumb))
        .with_state(state)
}

/// `Cache-Control` for served binaries: privately cacheable for an hour. Ids are
/// content-immutable capability URLs, but `private` keeps shared proxies from
/// caching a user's media.
const SERVE_CACHE_CONTROL: &str = "private, max-age=3600";

// ── canvases ────────────────────────────────────────────────────────────────

#[derive(serde::Serialize)]
struct CanvasListResponse {
    canvases: Vec<WorkshopCanvasMeta>,
}

async fn list_canvases(
    State(state): State<WorkshopRouterState>,
    Extension(_user): Extension<CurrentUser>,
) -> Result<Json<ApiResponse<CanvasListResponse>>, AppError> {
    let canvases = state.service.list_canvases().await?;
    Ok(Json(ApiResponse::ok(CanvasListResponse { canvases })))
}

#[derive(Deserialize)]
struct CreateCanvasRequest {
    #[serde(default)]
    title: Option<String>,
}

async fn create_canvas(
    State(state): State<WorkshopRouterState>,
    Extension(_user): Extension<CurrentUser>,
    body: Result<Json<CreateCanvasRequest>, JsonRejection>,
) -> Result<impl IntoResponse, AppError> {
    // Body is optional — an empty POST creates a default-titled canvas.
    let title = body.ok().and_then(|Json(req)| req.title);
    let meta = state.service.create_canvas(title).await?;
    Ok((StatusCode::CREATED, Json(ApiResponse::ok(meta))))
}

#[derive(serde::Serialize)]
struct CanvasDetailResponse {
    meta: WorkshopCanvasMeta,
    doc: Value,
}

async fn get_canvas(
    State(state): State<WorkshopRouterState>,
    Extension(_user): Extension<CurrentUser>,
    Path(id): Path<String>,
) -> Result<Json<ApiResponse<CanvasDetailResponse>>, AppError> {
    let c = state.service.get_canvas(&id).await?;
    // This REST route is the editor's canvas-doc load path (CanvasPage). Mark
    // the canvas "open" now so an agent's concurrent apply_ops in the gap before
    // the first pending-ops poll is queued for this editor rather than written
    // straight to canvas.json and then clobbered by the editor's first autosave.
    // The gateway agent reads via `service.get_canvas` directly and never hits
    // this handler, so it is not falsely marked open.
    state.service.mark_canvas_open(&id);
    Ok(Json(ApiResponse::ok(CanvasDetailResponse { meta: c.meta, doc: c.doc })))
}

#[derive(Deserialize)]
struct PutDocRequest {
    doc: Value,
}

#[derive(serde::Serialize)]
struct PutDocResponse {
    updated_at: i64,
}

async fn put_doc(
    State(state): State<WorkshopRouterState>,
    Extension(_user): Extension<CurrentUser>,
    Path(id): Path<String>,
    body: Result<Json<PutDocRequest>, JsonRejection>,
) -> Result<Json<ApiResponse<PutDocResponse>>, AppError> {
    let Json(req) = body.map_err(|e| AppError::BadRequest(e.to_string()))?;
    let updated_at = state.service.save_doc(&id, &req.doc).await?;
    Ok(Json(ApiResponse::ok(PutDocResponse { updated_at })))
}

// ── 画布助手 (agent-op) pending queue ─────────────────────────────────────────

#[derive(serde::Serialize)]
struct PendingOpsResponse {
    ops: Vec<PendingOp>,
}

/// Drain the pending agent ops for an open canvas (idempotent — ops stay until
/// acked). Polling this also registers the canvas as "open" so the agent's writes
/// route to this frontend rather than the backend direct applier.
async fn get_pending_ops(
    State(state): State<WorkshopRouterState>,
    Extension(_user): Extension<CurrentUser>,
    Path(id): Path<String>,
) -> Result<Json<ApiResponse<PendingOpsResponse>>, AppError> {
    let ops = state.service.take_pending_ops(&id).await?;
    Ok(Json(ApiResponse::ok(PendingOpsResponse { ops })))
}

#[derive(Deserialize)]
struct AckOpsRequest {
    #[serde(default)]
    op_ids: Vec<String>,
}

#[derive(serde::Serialize)]
struct AckOpsResponse {
    acked: usize,
}

/// Acknowledge (remove) agent ops the frontend has applied.
async fn ack_pending_ops(
    State(state): State<WorkshopRouterState>,
    Extension(_user): Extension<CurrentUser>,
    Path(id): Path<String>,
    body: Result<Json<AckOpsRequest>, JsonRejection>,
) -> Result<Json<ApiResponse<AckOpsResponse>>, AppError> {
    let Json(req) = body.map_err(|e| AppError::BadRequest(e.to_string()))?;
    state.service.ack_agent_ops(&id, &req.op_ids);
    Ok(Json(ApiResponse::ok(AckOpsResponse { acked: req.op_ids.len() })))
}

#[derive(Deserialize)]
struct PatchCanvasRequest {
    #[serde(default)]
    title: Option<String>,
    /// Set the canvas gallery thumbnail from this asset (append-only over the
    /// original `{ title }` contract).
    #[serde(default)]
    thumbnail_asset_id: Option<String>,
}

async fn patch_canvas(
    State(state): State<WorkshopRouterState>,
    Extension(_user): Extension<CurrentUser>,
    Path(id): Path<String>,
    body: Result<Json<PatchCanvasRequest>, JsonRejection>,
) -> Result<Json<ApiResponse<WorkshopCanvasMeta>>, AppError> {
    let Json(req) = body.map_err(|e| AppError::BadRequest(e.to_string()))?;
    let meta = state.service.patch_canvas(&id, req.title, req.thumbnail_asset_id).await?;
    Ok(Json(ApiResponse::ok(meta)))
}

async fn delete_canvas(
    State(state): State<WorkshopRouterState>,
    Extension(_user): Extension<CurrentUser>,
    Path(id): Path<String>,
) -> Result<StatusCode, AppError> {
    state.service.delete_canvas(&id).await?;
    Ok(StatusCode::NO_CONTENT)
}

// ── export / import / gc ─────────────────────────────────────────────────────

async fn export_canvas(
    State(state): State<WorkshopRouterState>,
    Extension(_user): Extension<CurrentUser>,
    Path(id): Path<String>,
) -> Result<Response, AppError> {
    let zip = state.service.export_canvas(&id).await?;
    let filename = format!("workshop-canvas-{id}.zip");
    Ok((
        [
            (header::CONTENT_TYPE, "application/zip".to_string()),
            (header::CONTENT_DISPOSITION, format!("attachment; filename=\"{filename}\"")),
        ],
        Body::from(zip),
    )
        .into_response())
}

async fn import_canvas(
    State(state): State<WorkshopRouterState>,
    Extension(_user): Extension<CurrentUser>,
    multipart: Multipart,
) -> Result<impl IntoResponse, AppError> {
    let bytes = extract_single_file(multipart).await?;
    let meta = state.service.import_canvas(bytes).await?;
    Ok((StatusCode::CREATED, Json(ApiResponse::ok(meta))))
}

/// Pull the `file` field bytes out of a multipart body (the import zip).
async fn extract_single_file(mut multipart: Multipart) -> Result<Vec<u8>, AppError> {
    while let Some(field) = multipart
        .next_field()
        .await
        .map_err(|e| AppError::BadRequest(format!("multipart error: {e}")))?
    {
        if field.name() == Some("file") {
            return Ok(field
                .bytes()
                .await
                .map_err(|e| AppError::BadRequest(format!("failed to read file: {e}")))?
                .to_vec());
        }
    }
    Err(AppError::BadRequest("missing 'file' field".into()))
}

#[derive(serde::Serialize)]
struct GcResponse {
    orphan_rows_deleted: usize,
    orphan_files_deleted: usize,
}

async fn run_gc(
    State(state): State<WorkshopRouterState>,
    Extension(_user): Extension<CurrentUser>,
) -> Result<Json<ApiResponse<GcResponse>>, AppError> {
    let stats = state.service.gc().await?;
    Ok(Json(ApiResponse::ok(GcResponse {
        orphan_rows_deleted: stats.orphan_rows_deleted,
        orphan_files_deleted: stats.orphan_files_deleted,
    })))
}

/// AUTH-EXEMPT (mounted on [`workshop_public_routes`]): no `CurrentUser`
/// extractor. Serves a canvas gallery thumbnail (JPEG).
async fn serve_canvas_thumb(
    State(state): State<WorkshopRouterState>,
    Path(id): Path<String>,
) -> Result<Response, AppError> {
    let served = state.service.serve_canvas_thumbnail(&id).await?;
    Ok((
        [
            (header::CONTENT_TYPE, served.mime),
            (header::CACHE_CONTROL, SERVE_CACHE_CONTROL.to_string()),
        ],
        Body::from(served.bytes),
    )
        .into_response())
}

// ── assets ────────────────────────────────────────────────────────────────

#[derive(Deserialize)]
struct ListAssetsQuery {
    kind: Option<String>,
    collection: Option<String>,
    q: Option<String>,
    in_library: Option<String>,
    /// Append-only (M10a): `ungrouped=1` returns only assets with no collection
    /// (`collection IS NULL OR ''`). Mutually exclusive with `collection` — when
    /// set, `collection` is ignored so the two never fight.
    #[serde(default)]
    ungrouped: Option<String>,
    /// Append-only (asset-library page): exact-match filter on one tag.
    #[serde(default)]
    tag: Option<String>,
    /// Append-only (asset-library page): result ordering token
    /// (`created_desc`|`created_asc`|`updated_desc`|`name_asc`|`size_desc`).
    /// Unknown/absent → newest-created first.
    #[serde(default)]
    sort: Option<String>,
    page: Option<i64>,
    page_size: Option<i64>,
}

#[derive(serde::Serialize)]
struct AssetListResponse {
    items: Vec<WorkshopAsset>,
    total: i64,
}

fn parse_bool_flag(v: &str) -> bool {
    matches!(v.trim(), "1" | "true" | "True" | "TRUE" | "yes")
}

/// Map a `sort` query token to an [`AssetSort`]. Unknown/empty → the default
/// (newest-created first).
fn parse_asset_sort(v: &str) -> nomifun_db::AssetSort {
    use nomifun_db::AssetSort;
    match v.trim() {
        "created_asc" => AssetSort::CreatedAsc,
        "updated_desc" => AssetSort::UpdatedDesc,
        "name_asc" => AssetSort::TitleAsc,
        "size_desc" => AssetSort::SizeDesc,
        _ => AssetSort::CreatedDesc,
    }
}

async fn list_assets(
    State(state): State<WorkshopRouterState>,
    Extension(_user): Extension<CurrentUser>,
    Query(query): Query<ListAssetsQuery>,
) -> Result<Json<ApiResponse<AssetListResponse>>, AppError> {
    let ungrouped = query.ungrouped.as_deref().map(parse_bool_flag).unwrap_or(false);
    let page = state
        .service
        .list_assets(AssetQuery {
            kind: query.kind.filter(|s| !s.trim().is_empty()),
            // `ungrouped` wins over `collection` (contract: mutually exclusive).
            collection: if ungrouped {
                None
            } else {
                query.collection.filter(|s| !s.trim().is_empty())
            },
            q: query.q,
            in_library: query.in_library.as_deref().map(parse_bool_flag),
            ungrouped,
            tag: query.tag.filter(|s| !s.trim().is_empty()),
            sort: query.sort.as_deref().map(parse_asset_sort).unwrap_or_default(),
            page: query.page.unwrap_or(1),
            page_size: query.page_size.unwrap_or(30),
        })
        .await?;
    Ok(Json(ApiResponse::ok(AssetListResponse { items: page.items, total: page.total })))
}

/// Fields extracted from a `/api/workshop/assets/upload` multipart request.
struct UploadFields {
    bytes: Vec<u8>,
    file_name: Option<String>,
    content_type: Option<String>,
    title: Option<String>,
    collection: Option<String>,
    tags: Option<Vec<String>>,
    in_library: Option<bool>,
}

/// Parse a `tags` form value: a JSON array string, else comma-separated.
fn parse_tags_field(raw: &str) -> Vec<String> {
    if let Ok(v) = serde_json::from_str::<Vec<String>>(raw) {
        return v.into_iter().map(|t| t.trim().to_string()).filter(|t| !t.is_empty()).collect();
    }
    raw.split(',').map(|t| t.trim().to_string()).filter(|t| !t.is_empty()).collect()
}

async fn extract_upload(mut multipart: Multipart) -> Result<UploadFields, AppError> {
    let mut bytes: Option<Vec<u8>> = None;
    let mut file_name: Option<String> = None;
    let mut content_type: Option<String> = None;
    let mut title: Option<String> = None;
    let mut collection: Option<String> = None;
    let mut tags: Option<Vec<String>> = None;
    let mut in_library: Option<bool> = None;

    while let Some(field) = multipart
        .next_field()
        .await
        .map_err(|e| AppError::BadRequest(format!("multipart error: {e}")))?
    {
        match field.name().unwrap_or("") {
            "file" => {
                file_name = field.file_name().map(str::to_string).filter(|s| !s.trim().is_empty());
                content_type = field.content_type().map(str::to_string);
                bytes = Some(
                    field
                        .bytes()
                        .await
                        .map_err(|e| AppError::BadRequest(format!("failed to read file: {e}")))?
                        .to_vec(),
                );
            }
            "title" => title = read_text(field).await?.filter(|s| !s.trim().is_empty()),
            "collection" => collection = read_text(field).await?.filter(|s| !s.trim().is_empty()),
            "tags" => tags = read_text(field).await?.map(|t| parse_tags_field(&t)),
            "in_library" => in_library = read_text(field).await?.map(|t| parse_bool_flag(&t)),
            _ => {}
        }
    }

    let bytes = bytes.ok_or_else(|| AppError::BadRequest("missing 'file' field".into()))?;
    Ok(UploadFields { bytes, file_name, content_type, title, collection, tags, in_library })
}

async fn read_text(field: axum::extract::multipart::Field<'_>) -> Result<Option<String>, AppError> {
    field
        .text()
        .await
        .map(Some)
        .map_err(|e| AppError::BadRequest(format!("failed to read field: {e}")))
}

async fn upload_asset(
    State(state): State<WorkshopRouterState>,
    Extension(_user): Extension<CurrentUser>,
    multipart: Multipart,
) -> Result<impl IntoResponse, AppError> {
    let fields = extract_upload(multipart).await?;
    let file_name = fields
        .file_name
        .unwrap_or_else(|| "upload".to_string());
    let asset = state
        .service
        .upload_asset(NewAssetUpload {
            file_name,
            content_type: fields.content_type,
            bytes: fields.bytes,
            title: fields.title,
            collection: fields.collection,
            tags: fields.tags,
            in_library: fields.in_library,
        })
        .await?;
    Ok((StatusCode::CREATED, Json(ApiResponse::ok(asset))))
}

#[derive(Deserialize)]
struct CreateTextAssetRequest {
    kind: String,
    title: String,
    #[serde(default)]
    text_content: String,
    #[serde(default)]
    collection: Option<String>,
    #[serde(default)]
    tags: Option<Vec<String>>,
    #[serde(default)]
    in_library: Option<bool>,
}

async fn create_text_asset(
    State(state): State<WorkshopRouterState>,
    Extension(_user): Extension<CurrentUser>,
    body: Result<Json<CreateTextAssetRequest>, JsonRejection>,
) -> Result<impl IntoResponse, AppError> {
    let Json(req) = body.map_err(|e| AppError::BadRequest(e.to_string()))?;
    if req.kind != "text" {
        return Err(AppError::BadRequest(
            "this endpoint only registers text assets; upload binaries via /api/workshop/assets/upload".into(),
        ));
    }
    let asset = state
        .service
        .create_text_asset(NewTextAsset {
            title: req.title,
            text_content: req.text_content,
            collection: req.collection,
            tags: req.tags,
            in_library: req.in_library,
        })
        .await?;
    Ok((StatusCode::CREATED, Json(ApiResponse::ok(asset))))
}

#[derive(Deserialize)]
struct PatchAssetRequest {
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    collection: Option<String>,
    #[serde(default)]
    tags: Option<Vec<String>>,
    #[serde(default)]
    in_library: Option<bool>,
}

async fn patch_asset(
    State(state): State<WorkshopRouterState>,
    Extension(_user): Extension<CurrentUser>,
    Path(id): Path<String>,
    body: Result<Json<PatchAssetRequest>, JsonRejection>,
) -> Result<Json<ApiResponse<WorkshopAsset>>, AppError> {
    let Json(req) = body.map_err(|e| AppError::BadRequest(e.to_string()))?;
    let patched = state
        .service
        .patch_asset(
            &id,
            AssetPatch {
                title: req.title,
                collection: req.collection,
                tags: req.tags,
                in_library: req.in_library,
            },
        )
        .await?;
    Ok(Json(ApiResponse::ok(patched)))
}

async fn delete_asset(
    State(state): State<WorkshopRouterState>,
    Extension(_user): Extension<CurrentUser>,
    Path(id): Path<String>,
) -> Result<StatusCode, AppError> {
    state.service.delete_asset(&id).await?;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize)]
struct RenameCollectionRequest {
    from: String,
    /// The new collection name; a blank value ungroups the affected assets.
    #[serde(default)]
    to: String,
}

#[derive(serde::Serialize)]
struct RenameCollectionResponse {
    updated: u64,
}

/// Bulk-rename a collection across every asset that used it (management
/// surface). Returns the number of rows updated.
async fn rename_collection(
    State(state): State<WorkshopRouterState>,
    Extension(_user): Extension<CurrentUser>,
    body: Result<Json<RenameCollectionRequest>, JsonRejection>,
) -> Result<Json<ApiResponse<RenameCollectionResponse>>, AppError> {
    let Json(req) = body.map_err(|e| AppError::BadRequest(e.to_string()))?;
    let updated = state.service.rename_collection(&req.from, &req.to).await?;
    Ok(Json(ApiResponse::ok(RenameCollectionResponse { updated })))
}

#[derive(Deserialize)]
struct FileQuery {
    #[serde(default)]
    thumb: Option<String>,
}

/// AUTH-EXEMPT (mounted on [`workshop_public_routes`]): no `CurrentUser`
/// extractor. Serves an asset's original binary (or, with `?thumb=1`, its
/// thumbnail). Traversal-safe via the service; missing → 404.
async fn serve_file(
    State(state): State<WorkshopRouterState>,
    Path(asset_id): Path<String>,
    Query(query): Query<FileQuery>,
) -> Result<Response, AppError> {
    let thumb = query.thumb.as_deref().map(parse_bool_flag).unwrap_or(false);
    let served = state.service.serve_file(&asset_id, thumb).await?;
    Ok((
        [
            (header::CONTENT_TYPE, served.mime),
            (header::CACHE_CONTROL, SERVE_CACHE_CONTROL.to_string()),
        ],
        Body::from(served.bytes),
    )
        .into_response())
}

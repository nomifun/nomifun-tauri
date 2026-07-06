//! `/api/knowledge/*` route handlers.

use axum::Router;
use axum::extract::rejection::JsonRejection;
use axum::extract::{Extension, Json, Path, Query, State};
use axum::routing::{get, post};

use nomifun_api_types::{ApiResponse, ConnectorCredentialSummary, CreateKnowledgeTagRequest, KnowledgeSource, KnowledgeTag, UpdateKnowledgeTagRequest};
use nomifun_auth::CurrentUser;
use nomifun_common::AppError;
use serde::{Deserialize, Serialize};

use crate::connector::ConnectorIdentity;
use crate::export::{self, ExportSummary, ImportSummary};
use crate::service::{
    AutogenOutcome, ConsumerInfo, InboxDiff, InboxEntry, InboxMergeResult, KbFileContent, KbFileEntry,
    KbTreeEntry, KnowledgeBaseInfo, KnowledgeBinding, KnowledgeSearchHit, RefreshSourceSummary,
};
use crate::state::KnowledgeRouterState;

pub fn knowledge_routes(state: KnowledgeRouterState) -> Router {
    Router::new()
        .route("/api/knowledge/bases", get(list_bases).post(create_base))
        .route("/api/knowledge/bases/import", post(import_base))
        .route(
            "/api/knowledge/bases/{id}",
            get(get_base).put(update_base).delete(delete_base),
        )
        .route("/api/knowledge/bases/{id}/export", post(export_base))
        .route("/api/knowledge/bases/{id}/autogen", post(autogen_base))
        .route("/api/knowledge/description/generate", post(generate_description))
        .route("/api/knowledge/description/polish", post(polish_description))
        .route("/api/knowledge/bases/{id}/refresh-source", post(refresh_source))
        .route("/api/knowledge/bases/{id}/source", axum::routing::put(set_source))
        .route("/api/knowledge/bases/{id}/sync", post(sync_source))
        .route(
            "/api/knowledge/connectors/credentials",
            get(list_credentials).post(create_credential),
        )
        .route(
            "/api/knowledge/connectors/credentials/{id}",
            axum::routing::delete(delete_credential),
        )
        .route(
            "/api/knowledge/connectors/credentials/{id}/test",
            post(test_credential),
        )
        .route(
            "/api/knowledge/tags",
            get(list_tags).post(create_tag),
        )
        .route(
            "/api/knowledge/tags/{key}",
            axum::routing::put(update_tag).delete(delete_tag),
        )
        .route("/api/knowledge/bases/{id}/files", get(list_files))
        .route("/api/knowledge/bases/{id}/tree", get(list_tree))
        .route("/api/knowledge/bases/{id}/folder", post(create_folder).delete(delete_folder))
        .route("/api/knowledge/bases/{id}/tree/rename", post(rename_tree_entry))
        .route("/api/knowledge/bases/{id}/inbox", get(list_inbox))
        .route("/api/knowledge/inbox/pending-count", get(pending_inbox_count))
        .route("/api/knowledge/bases/{id}/inbox/diff", get(inbox_diff))
        .route("/api/knowledge/bases/{id}/inbox/merge", post(merge_inbox))
        .route("/api/knowledge/bases/{id}/inbox/discard", post(discard_inbox))
        .route("/api/knowledge/inbox/merge-all", post(merge_all_inbox))
        .route("/api/knowledge/inbox/discard-all", post(discard_all_inbox))
        .route("/api/knowledge/bases/{id}/consumers", get(list_consumers))
        .route(
            "/api/knowledge/bases/{id}/file",
            get(read_file).put(write_file).delete(delete_file),
        )
        .route(
            // `target_id` is ONE path segment. Workpath targets (normalized
            // absolute paths) therefore arrive percent-encoded — the
            // frontend calls `encodeURIComponent(workpathKey)` so `/`
            // travels as `%2F`. axum matches routes on the still-encoded
            // path and the `Path` extractor decodes afterwards, so an
            // encoded path never splits into extra segments (pinned by
            // `binding_route_extracts_percent_encoded_workpath` below).
            "/api/knowledge/binding/{kind}/{target_id}",
            get(get_binding).post(set_binding),
        )
        .route("/api/knowledge/search", post(search_bases))
        .with_state(state)
}

async fn list_bases(
    State(state): State<KnowledgeRouterState>,
    Extension(_user): Extension<CurrentUser>,
) -> Result<Json<ApiResponse<Vec<KnowledgeBaseInfo>>>, AppError> {
    Ok(Json(ApiResponse::ok(state.service.list_bases().await?)))
}

#[derive(Deserialize)]
struct CreateBaseRequest {
    name: String,
    #[serde(default)]
    description: String,
    /// Absolute path of an existing external directory; omit to provision a
    /// managed directory under the backend data dir.
    root_path: Option<String>,
    /// Optional URL source, stored in `extra.source`. `mode=live` stores it
    /// without fetching; `mode=snapshot` fetches every entry into
    /// `snapshots/` before the response returns (and chains a best-effort
    /// AI overview run) — the per-entry fetch outcome is reported in the
    /// response's `source_fetch` field.
    #[serde(default)]
    source: Option<KnowledgeSource>,
    /// Optional tag keys to assign at creation time (same semantics as
    /// `UpdateBaseRequest.tags`).
    #[serde(default)]
    tags: Option<Vec<String>>,
}

async fn create_base(
    State(state): State<KnowledgeRouterState>,
    Extension(_user): Extension<CurrentUser>,
    body: Result<Json<CreateBaseRequest>, JsonRejection>,
) -> Result<Json<ApiResponse<KnowledgeBaseInfo>>, AppError> {
    let Json(req) = body.map_err(|e| AppError::BadRequest(e.to_string()))?;
    // Detect whether the source is connector-backed (e.g. feishu) so we can
    // fire-and-forget a first sync after creation without blocking the response.
    let is_connector_source = req
        .source
        .as_ref()
        .is_some_and(|s| s.kind != "url" && !s.kind.is_empty());
    let mut info = state
        .service
        .create_base(&req.name, &req.description, req.root_path.as_deref(), req.source)
        .await?;
    // Persist tags (if provided) as a post-creation step — avoids changing the
    // 4-param `create_base` signature used by 50+ callers.
    if let Some(ref tag_keys) = req.tags {
        if !tag_keys.is_empty() {
            info = state.service.update_base(&info.id, None, None, Some(tag_keys.clone())).await?;
        }
    }
    // Connector-backed sources (feishu, etc.): trigger background sync so the
    // user does not have to manually invoke /sync after creation.
    if is_connector_source {
        let service = state.service.clone();
        let kb_id = info.id.clone();
        tokio::spawn(async move {
            if let Err(e) = service.sync_connector_source(&kb_id).await {
                tracing::warn!(kb_id, error = %e, "background connector sync after create failed");
            }
        });
    }
    Ok(Json(ApiResponse::ok(info)))
}

async fn get_base(
    State(state): State<KnowledgeRouterState>,
    Extension(_user): Extension<CurrentUser>,
    Path(id): Path<String>,
) -> Result<Json<ApiResponse<KnowledgeBaseInfo>>, AppError> {
    Ok(Json(ApiResponse::ok(state.service.get_base_info(&id).await?)))
}

#[derive(Deserialize)]
struct UpdateBaseRequest {
    name: Option<String>,
    description: Option<String>,
    #[serde(default)]
    tags: Option<Vec<String>>,
}

async fn update_base(
    State(state): State<KnowledgeRouterState>,
    Extension(_user): Extension<CurrentUser>,
    Path(id): Path<String>,
    body: Result<Json<UpdateBaseRequest>, JsonRejection>,
) -> Result<Json<ApiResponse<KnowledgeBaseInfo>>, AppError> {
    let Json(req) = body.map_err(|e| AppError::BadRequest(e.to_string()))?;
    Ok(Json(ApiResponse::ok(
        state
            .service
            .update_base(&id, req.name.as_deref(), req.description.as_deref(), req.tags)
            .await?,
    )))
}

#[derive(Deserialize)]
struct DeleteBaseQuery {
    #[serde(default)]
    purge: bool,
}

async fn delete_base(
    State(state): State<KnowledgeRouterState>,
    Extension(_user): Extension<CurrentUser>,
    Path(id): Path<String>,
    Query(query): Query<DeleteBaseQuery>,
) -> Result<Json<ApiResponse<()>>, AppError> {
    state.service.delete_base(&id, query.purge).await?;
    Ok(Json(ApiResponse::ok(())))
}

async fn list_files(
    State(state): State<KnowledgeRouterState>,
    Extension(_user): Extension<CurrentUser>,
    Path(id): Path<String>,
) -> Result<Json<ApiResponse<Vec<KbFileEntry>>>, AppError> {
    Ok(Json(ApiResponse::ok(state.service.list_files(&id).await?)))
}

#[derive(Deserialize, Default)]
struct TreeQuery {
    #[serde(default)]
    path: String,
}

async fn list_tree(
    State(state): State<KnowledgeRouterState>,
    Extension(_user): Extension<CurrentUser>,
    Path(id): Path<String>,
    Query(query): Query<TreeQuery>,
) -> Result<Json<ApiResponse<Vec<KbTreeEntry>>>, AppError> {
    Ok(Json(ApiResponse::ok(
        state.service.list_tree(&id, &query.path).await?,
    )))
}

#[derive(Deserialize)]
struct CreateFolderRequest {
    path: String,
}

async fn create_folder(
    State(state): State<KnowledgeRouterState>,
    Extension(_user): Extension<CurrentUser>,
    Path(id): Path<String>,
    body: Result<Json<CreateFolderRequest>, JsonRejection>,
) -> Result<Json<ApiResponse<KbTreeEntry>>, AppError> {
    let Json(req) = body.map_err(|e| AppError::BadRequest(e.to_string()))?;
    Ok(Json(ApiResponse::ok(state.service.create_folder(&id, &req.path).await?)))
}

async fn delete_folder(
    State(state): State<KnowledgeRouterState>,
    Extension(_user): Extension<CurrentUser>,
    Path(id): Path<String>,
    Query(query): Query<TreeQuery>,
) -> Result<Json<ApiResponse<()>>, AppError> {
    state.service.delete_folder(&id, &query.path).await?;
    Ok(Json(ApiResponse::ok(())))
}

#[derive(Deserialize)]
struct RenameTreeEntryRequest {
    path: String,
    new_name: String,
}

async fn rename_tree_entry(
    State(state): State<KnowledgeRouterState>,
    Extension(_user): Extension<CurrentUser>,
    Path(id): Path<String>,
    body: Result<Json<RenameTreeEntryRequest>, JsonRejection>,
) -> Result<Json<ApiResponse<KbTreeEntry>>, AppError> {
    let Json(req) = body.map_err(|e| AppError::BadRequest(e.to_string()))?;
    Ok(Json(ApiResponse::ok(
        state.service.rename_tree_entry(&id, &req.path, &req.new_name).await?,
    )))
}

#[derive(Deserialize)]
struct ExportBaseRequest {
    /// Absolute destination path of the zip package.
    dest_path: String,
}

async fn export_base(
    State(state): State<KnowledgeRouterState>,
    Extension(_user): Extension<CurrentUser>,
    Path(id): Path<String>,
    body: Result<Json<ExportBaseRequest>, JsonRejection>,
) -> Result<Json<ApiResponse<ExportSummary>>, AppError> {
    let Json(req) = body.map_err(|e| AppError::BadRequest(e.to_string()))?;
    Ok(Json(ApiResponse::ok(
        export::export_base(&state.service, &id, std::path::Path::new(&req.dest_path)).await?,
    )))
}

#[derive(Deserialize)]
struct ImportBaseRequest {
    /// Absolute path of a zip package created by the export endpoint.
    src_path: String,
}

/// On success the service's managed-create path has already emitted
/// `knowledge.base-created` (followed by `knowledge.base-updated` with the
/// final file stats), so connected frontends refresh automatically. A
/// best-effort AI overview run is then spawned in the background: it never
/// overwrites a README carried by the package and only backfills the
/// description when the package had none.
async fn import_base(
    State(state): State<KnowledgeRouterState>,
    Extension(_user): Extension<CurrentUser>,
    body: Result<Json<ImportBaseRequest>, JsonRejection>,
) -> Result<Json<ApiResponse<ImportSummary>>, AppError> {
    let Json(req) = body.map_err(|e| AppError::BadRequest(e.to_string()))?;
    let summary = export::import_base(&state.service, std::path::Path::new(&req.src_path)).await?;

    let service = state.service.clone();
    let kb_id = summary.kb_id.clone();
    tokio::spawn(async move {
        // Best-effort: a missing completer (409) or an empty base (400) is
        // expected and must not surface anywhere. `None`: post-import
        // backfill is a background curation task → always the default model.
        if let Err(e) = service.generate_overview_opts(&kb_id, false, true, None).await {
            tracing::debug!(kb_id, error = %e, "post-import knowledge autogen skipped");
        }
    });

    Ok(Json(ApiResponse::ok(summary)))
}

#[derive(Deserialize, Default)]
struct AutogenRequest {
    /// Replace an existing `README.md`; default keeps it (the description is
    /// refreshed either way).
    #[serde(default)]
    overwrite_readme: bool,
    /// Explicit provider for the LLM call (the model picker). Must be sent
    /// together with `model` or not at all.
    #[serde(default)]
    provider_id: Option<String>,
    /// Explicit model for the LLM call. Must be sent together with
    /// `provider_id` or not at all.
    #[serde(default)]
    model: Option<String>,
}

/// Validate and assemble an optional explicit `(provider_id, model)` pick
/// from a request: both fields must be present (non-blank) or both absent —
/// a half-specified pick is a 400. Returns `Ok(None)` when neither is given
/// (use the completer's default model).
fn model_override(
    provider_id: Option<String>,
    model: Option<String>,
) -> Result<Option<(String, String)>, AppError> {
    let provider_id = provider_id.map(|s| s.trim().to_owned()).filter(|s| !s.is_empty());
    let model = model.map(|s| s.trim().to_owned()).filter(|s| !s.is_empty());
    match (provider_id, model) {
        (Some(p), Some(m)) => Ok(Some((p, m))),
        (None, None) => Ok(None),
        _ => Err(AppError::BadRequest(
            "provider_id and model must be supplied together (or both omitted)".into(),
        )),
    }
}

/// AI overview generation. Without a wired completer this returns 409 with
/// an actionable message. Completion is broadcast as `knowledge.base-updated`.
async fn autogen_base(
    State(state): State<KnowledgeRouterState>,
    Extension(_user): Extension<CurrentUser>,
    Path(id): Path<String>,
    body: Option<Json<AutogenRequest>>,
) -> Result<Json<ApiResponse<AutogenOutcome>>, AppError> {
    let req = body.map(|Json(r)| r).unwrap_or_default();
    let override_model = model_override(req.provider_id, req.model)?;
    Ok(Json(ApiResponse::ok(
        state.service.generate_overview(&id, req.overwrite_readme, override_model).await?,
    )))
}

#[derive(Deserialize)]
struct GenerateDescriptionRequest {
    /// Tentative base name from the create form; may be omitted/blank.
    #[serde(default)]
    name: String,
    /// Absolute path of an existing directory to sample.
    root_path: String,
    /// Explicit provider for the LLM call; pair with `model` or omit both.
    #[serde(default)]
    provider_id: Option<String>,
    /// Explicit model for the LLM call; pair with `provider_id` or omit both.
    #[serde(default)]
    model: Option<String>,
}

#[derive(Deserialize)]
struct PolishDescriptionRequest {
    /// Tentative base name from the create form; may be omitted/blank.
    #[serde(default)]
    name: String,
    /// User-written draft description to rewrite.
    draft: String,
    /// Explicit provider for the LLM call; pair with `model` or omit both.
    #[serde(default)]
    provider_id: Option<String>,
    /// Explicit model for the LLM call; pair with `provider_id` or omit both.
    #[serde(default)]
    model: Option<String>,
}

#[derive(Serialize)]
struct DescriptionResponse {
    description: String,
}

/// Stateless AI description generation for the create-base form: samples the
/// given directory and returns a description only — no base row required,
/// nothing persisted. 409 without a wired completer, 400 on an invalid path.
async fn generate_description(
    State(state): State<KnowledgeRouterState>,
    Extension(_user): Extension<CurrentUser>,
    body: Result<Json<GenerateDescriptionRequest>, JsonRejection>,
) -> Result<Json<ApiResponse<DescriptionResponse>>, AppError> {
    let Json(req) = body.map_err(|e| AppError::BadRequest(e.to_string()))?;
    let override_model = model_override(req.provider_id, req.model)?;
    let description = state
        .service
        .generate_description_for_path(&req.name, &req.root_path, override_model)
        .await?;
    Ok(Json(ApiResponse::ok(DescriptionResponse { description })))
}

/// Stateless AI polish of a user-written draft description. 409 without a
/// wired completer, 400 on an empty draft. Nothing persisted.
async fn polish_description(
    State(state): State<KnowledgeRouterState>,
    Extension(_user): Extension<CurrentUser>,
    body: Result<Json<PolishDescriptionRequest>, JsonRejection>,
) -> Result<Json<ApiResponse<DescriptionResponse>>, AppError> {
    let Json(req) = body.map_err(|e| AppError::BadRequest(e.to_string()))?;
    let override_model = model_override(req.provider_id, req.model)?;
    let description = state.service.polish_description(&req.name, &req.draft, override_model).await?;
    Ok(Json(ApiResponse::ok(DescriptionResponse { description })))
}

/// Re-fetch every URL-source entry (overwriting old snapshots) and stamp
/// `extra.source.last_fetched_at`.
async fn refresh_source(
    State(state): State<KnowledgeRouterState>,
    Extension(_user): Extension<CurrentUser>,
    Path(id): Path<String>,
) -> Result<Json<ApiResponse<RefreshSourceSummary>>, AppError> {
    Ok(Json(ApiResponse::ok(state.service.refresh_source(&id).await?)))
}

/// Pull a connector-backed base's remote documents into `snapshots/` (Feishu
/// wiki, …). Distinct from `refresh-source`, which is for URL sources.
async fn sync_source(
    State(state): State<KnowledgeRouterState>,
    Extension(_user): Extension<CurrentUser>,
    Path(id): Path<String>,
) -> Result<Json<ApiResponse<RefreshSourceSummary>>, AppError> {
    Ok(Json(ApiResponse::ok(state.service.sync_connector_source(&id).await?)))
}

#[derive(Deserialize)]
struct SetSourceRequest {
    /// New source config, or `null` to detach the base's source.
    #[serde(default)]
    source: Option<KnowledgeSource>,
}

/// Attach / replace / clear a base's source config (`extra.source`). Used to
/// wire a connector (Feishu, …) onto an existing base. Does not fetch — the
/// caller triggers sync afterward.
async fn set_source(
    State(state): State<KnowledgeRouterState>,
    Extension(_user): Extension<CurrentUser>,
    Path(id): Path<String>,
    body: Result<Json<SetSourceRequest>, JsonRejection>,
) -> Result<Json<ApiResponse<KnowledgeBaseInfo>>, AppError> {
    let Json(req) = body.map_err(|e| AppError::BadRequest(e.to_string()))?;
    Ok(Json(ApiResponse::ok(state.service.set_source(&id, req.source).await?)))
}

async fn list_credentials(
    State(state): State<KnowledgeRouterState>,
    Extension(_user): Extension<CurrentUser>,
) -> Result<Json<ApiResponse<Vec<ConnectorCredentialSummary>>>, AppError> {
    Ok(Json(ApiResponse::ok(state.service.list_credentials().await?)))
}

#[derive(Deserialize)]
struct CreateCredentialRequest {
    /// Connector discriminator: "feishu", …
    kind: String,
    name: String,
    /// Connector-specific secret payload (e.g. Feishu `{ app_id, app_secret }`).
    /// Probed against the remote before being encrypted at rest.
    payload: serde_json::Value,
}

async fn create_credential(
    State(state): State<KnowledgeRouterState>,
    Extension(_user): Extension<CurrentUser>,
    body: Result<Json<CreateCredentialRequest>, JsonRejection>,
) -> Result<Json<ApiResponse<ConnectorCredentialSummary>>, AppError> {
    let Json(req) = body.map_err(|e| AppError::BadRequest(e.to_string()))?;
    let summary = state.service.create_credential(&req.kind, &req.name, req.payload).await?;
    Ok(Json(ApiResponse::ok(summary)))
}

async fn delete_credential(
    State(state): State<KnowledgeRouterState>,
    Extension(_user): Extension<CurrentUser>,
    Path(id): Path<String>,
) -> Result<Json<ApiResponse<()>>, AppError> {
    state.service.delete_credential(&id).await?;
    Ok(Json(ApiResponse::ok(())))
}

/// Re-probe a stored credential against its remote (the UI "test connection"
/// action). Returns the connector identity on success.
async fn test_credential(
    State(state): State<KnowledgeRouterState>,
    Extension(_user): Extension<CurrentUser>,
    Path(id): Path<String>,
) -> Result<Json<ApiResponse<ConnectorIdentity>>, AppError> {
    Ok(Json(ApiResponse::ok(state.service.test_credential(&id).await?)))
}

// ── Tag CRUD routes ──────────────────────────────────────────────────────

async fn list_tags(
    State(state): State<KnowledgeRouterState>,
    Extension(_user): Extension<CurrentUser>,
) -> Result<Json<ApiResponse<Vec<KnowledgeTag>>>, AppError> {
    Ok(Json(ApiResponse::ok(state.service.list_tags().await?)))
}

async fn create_tag(
    State(state): State<KnowledgeRouterState>,
    Extension(_user): Extension<CurrentUser>,
    body: Result<Json<CreateKnowledgeTagRequest>, JsonRejection>,
) -> Result<Json<ApiResponse<KnowledgeTag>>, AppError> {
    let Json(req) = body.map_err(|e| AppError::BadRequest(e.to_string()))?;
    Ok(Json(ApiResponse::ok(
        state.service.create_tag(&req.label, req.color).await?,
    )))
}

async fn update_tag(
    State(state): State<KnowledgeRouterState>,
    Extension(_user): Extension<CurrentUser>,
    Path(key): Path<String>,
    body: Result<Json<UpdateKnowledgeTagRequest>, JsonRejection>,
) -> Result<Json<ApiResponse<KnowledgeTag>>, AppError> {
    let Json(req) = body.map_err(|e| AppError::BadRequest(e.to_string()))?;
    Ok(Json(ApiResponse::ok(
        state.service.update_tag(&key, req).await?,
    )))
}

async fn delete_tag(
    State(state): State<KnowledgeRouterState>,
    Extension(_user): Extension<CurrentUser>,
    Path(key): Path<String>,
) -> Result<Json<ApiResponse<()>>, AppError> {
    state.service.delete_tag(&key).await?;
    Ok(Json(ApiResponse::ok(())))
}

// ── P4 inbox review + consumers ───────────────────────────────────────

async fn list_inbox(
    State(state): State<KnowledgeRouterState>,
    Extension(_user): Extension<CurrentUser>,
    Path(id): Path<String>,
) -> Result<Json<ApiResponse<Vec<InboxEntry>>>, AppError> {
    Ok(Json(ApiResponse::ok(state.service.list_inbox(&id).await?)))
}

/// Total unreviewed staged proposals across all bases (sidebar red-dot signal).
async fn pending_inbox_count(
    State(state): State<KnowledgeRouterState>,
    Extension(_user): Extension<CurrentUser>,
) -> Result<Json<ApiResponse<usize>>, AppError> {
    Ok(Json(ApiResponse::ok(state.service.count_pending_inbox().await?)))
}

#[derive(Deserialize)]
struct InboxItemQuery {
    scope: String,
    path: String,
}

async fn inbox_diff(
    State(state): State<KnowledgeRouterState>,
    Extension(_user): Extension<CurrentUser>,
    Path(id): Path<String>,
    Query(q): Query<InboxItemQuery>,
) -> Result<Json<ApiResponse<InboxDiff>>, AppError> {
    Ok(Json(ApiResponse::ok(state.service.inbox_diff(&id, &q.scope, &q.path).await?)))
}

#[derive(Deserialize)]
struct InboxActionRequest {
    scope: String,
    path: String,
}

async fn merge_inbox(
    State(state): State<KnowledgeRouterState>,
    Extension(_user): Extension<CurrentUser>,
    Path(id): Path<String>,
    body: Result<Json<InboxActionRequest>, JsonRejection>,
) -> Result<Json<ApiResponse<InboxMergeResult>>, AppError> {
    let Json(req) = body.map_err(|e| AppError::BadRequest(e.to_string()))?;
    Ok(Json(ApiResponse::ok(state.service.merge_inbox(&id, &req.scope, &req.path).await?)))
}

async fn discard_inbox(
    State(state): State<KnowledgeRouterState>,
    Extension(_user): Extension<CurrentUser>,
    Path(id): Path<String>,
    body: Result<Json<InboxActionRequest>, JsonRejection>,
) -> Result<Json<ApiResponse<()>>, AppError> {
    let Json(req) = body.map_err(|e| AppError::BadRequest(e.to_string()))?;
    state.service.discard_inbox(&id, &req.scope, &req.path).await?;
    Ok(Json(ApiResponse::ok(())))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct InboxBatchRequest {
    kb_id: String,
    scope: Option<String>,
}

#[derive(Serialize)]
struct InboxBatchResult {
    count: usize,
}

async fn merge_all_inbox(
    State(state): State<KnowledgeRouterState>,
    Extension(_user): Extension<CurrentUser>,
    body: Result<Json<InboxBatchRequest>, JsonRejection>,
) -> Result<Json<ApiResponse<InboxBatchResult>>, AppError> {
    let Json(req) = body.map_err(|e| AppError::BadRequest(e.to_string()))?;
    let count = state.service.merge_all_inbox(&req.kb_id, req.scope.as_deref()).await?;
    Ok(Json(ApiResponse::ok(InboxBatchResult { count })))
}

async fn discard_all_inbox(
    State(state): State<KnowledgeRouterState>,
    Extension(_user): Extension<CurrentUser>,
    body: Result<Json<InboxBatchRequest>, JsonRejection>,
) -> Result<Json<ApiResponse<InboxBatchResult>>, AppError> {
    let Json(req) = body.map_err(|e| AppError::BadRequest(e.to_string()))?;
    let count = state.service.discard_all_inbox(&req.kb_id, req.scope.as_deref()).await?;
    Ok(Json(ApiResponse::ok(InboxBatchResult { count })))
}

async fn list_consumers(
    State(state): State<KnowledgeRouterState>,
    Extension(_user): Extension<CurrentUser>,
    Path(id): Path<String>,
) -> Result<Json<ApiResponse<Vec<ConsumerInfo>>>, AppError> {
    Ok(Json(ApiResponse::ok(state.service.list_consumers(&id).await?)))
}

#[derive(Deserialize)]
struct FilePathQuery {
    path: String,
}

async fn read_file(
    State(state): State<KnowledgeRouterState>,
    Extension(_user): Extension<CurrentUser>,
    Path(id): Path<String>,
    Query(query): Query<FilePathQuery>,
) -> Result<Json<ApiResponse<KbFileContent>>, AppError> {
    Ok(Json(ApiResponse::ok(state.service.read_file(&id, &query.path).await?)))
}

#[derive(Deserialize)]
struct WriteFileRequest {
    path: String,
    content: String,
}

async fn write_file(
    State(state): State<KnowledgeRouterState>,
    Extension(_user): Extension<CurrentUser>,
    Path(id): Path<String>,
    body: Result<Json<WriteFileRequest>, JsonRejection>,
) -> Result<Json<ApiResponse<()>>, AppError> {
    let Json(req) = body.map_err(|e| AppError::BadRequest(e.to_string()))?;
    state.service.write_file(&id, &req.path, &req.content).await?;
    Ok(Json(ApiResponse::ok(())))
}

async fn delete_file(
    State(state): State<KnowledgeRouterState>,
    Extension(_user): Extension<CurrentUser>,
    Path(id): Path<String>,
    Query(query): Query<FilePathQuery>,
) -> Result<Json<ApiResponse<()>>, AppError> {
    state.service.delete_file(&id, &query.path).await?;
    Ok(Json(ApiResponse::ok(())))
}

async fn get_binding(
    State(state): State<KnowledgeRouterState>,
    Extension(_user): Extension<CurrentUser>,
    Path((kind, target_id)): Path<(String, String)>,
) -> Result<Json<ApiResponse<KnowledgeBinding>>, AppError> {
    Ok(Json(ApiResponse::ok(state.service.get_binding(&kind, &target_id).await?)))
}

async fn set_binding(
    State(state): State<KnowledgeRouterState>,
    Extension(_user): Extension<CurrentUser>,
    Path((kind, target_id)): Path<(String, String)>,
    body: Result<Json<KnowledgeBinding>, JsonRejection>,
) -> Result<Json<ApiResponse<KnowledgeBinding>>, AppError> {
    let Json(binding) = body.map_err(|e| AppError::BadRequest(e.to_string()))?;
    Ok(Json(ApiResponse::ok(
        state.service.set_binding(&kind, &target_id, binding).await?,
    )))
}

// ─── Manual search (read-only, scoped) ───────────────────────────────────────

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct SearchBasesRequest {
    kb_ids: Vec<String>,
    query: String,
    limit: Option<usize>,
}

async fn search_bases(
    State(state): State<KnowledgeRouterState>,
    Extension(_user): Extension<CurrentUser>,
    body: Result<Json<SearchBasesRequest>, JsonRejection>,
) -> Result<Json<ApiResponse<Vec<KnowledgeSearchHit>>>, AppError> {
    let Json(req) = body.map_err(|e| AppError::BadRequest(e.to_string()))?;
    // Scope: only search the caller-supplied kb_ids. Empty list → empty result
    // (search_bases already handles this, but we make it explicit).
    if req.kb_ids.is_empty() {
        return Ok(Json(ApiResponse::ok(Vec::new())));
    }
    let limit = req.limit.unwrap_or(20);
    let hits = state.service.search_bases(&req.kb_ids, &req.query, limit).await?;
    Ok(Json(ApiResponse::ok(hits)))
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use http_body_util::BodyExt;
    use tower::ServiceExt;

    use super::*;
    use crate::testutil::make_service;

    fn test_app(data_dir: &std::path::Path) -> Router {
        let service = Arc::new(make_service(data_dir));
        // The auth middleware normally injects `CurrentUser`; tests attach
        // it directly as a request extension.
        knowledge_routes(KnowledgeRouterState::new(service)).layer(Extension(CurrentUser {
            id: "u1".into(),
            username: "u1".into(),
        }))
    }

    async fn json_body(resp: axum::response::Response) -> serde_json::Value {
        let bytes = resp.into_body().collect().await.unwrap().to_bytes();
        serde_json::from_slice(&bytes).unwrap()
    }

    /// Wire-contract pin for workpath bindings (frontend Task 11): the
    /// target_id is sent as ONE percent-encoded segment
    /// (`encodeURIComponent(workpathKey)`, `/` → `%2F`). axum must match
    /// the single-segment route on the encoded path and hand the DECODED
    /// path to the handler; the service then canonicalizes spellings
    /// (trailing slash et al) onto one row.
    #[tokio::test]
    async fn binding_route_extracts_percent_encoded_workpath() {
        let dir = tempfile::TempDir::new().unwrap();
        let app = test_app(dir.path());

        // Write under a trailing-slash spelling…
        let set = Request::post("/api/knowledge/binding/workpath/%2FUsers%2Fme%2Fproj%2F")
            .header("content-type", "application/json")
            .body(Body::from(
                r#"{"enabled":true,"writeback":false,"kb_ids":["kb_x"]}"#,
            ))
            .unwrap();
        let resp = app.clone().oneshot(set).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        // …and read it back under the canonical spelling: same row.
        let get = Request::get("/api/knowledge/binding/workpath/%2FUsers%2Fme%2Fproj")
            .body(Body::empty())
            .unwrap();
        let resp = app.clone().oneshot(get).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let v = json_body(resp).await;
        assert_eq!(v["success"], true, "{v}");
        assert_eq!(v["data"]["enabled"], true, "{v}");
        assert_eq!(v["data"]["kb_ids"][0], "kb_x", "{v}");

        // A never-bound workpath reads as the default (disabled) binding.
        let get = Request::get("/api/knowledge/binding/workpath/%2Felsewhere")
            .body(Body::empty())
            .unwrap();
        let resp = app.clone().oneshot(get).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let v = json_body(resp).await;
        assert_eq!(v["data"]["enabled"], false, "{v}");

        // The default-workpath sentinel needs no encoding at all.
        let get = Request::get("/api/knowledge/binding/workpath/__default__")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(get).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    /// An unknown binding kind stays a 400 — `workpath` is now accepted,
    /// arbitrary kinds are not.
    #[tokio::test]
    async fn binding_route_rejects_unknown_kind() {
        let dir = tempfile::TempDir::new().unwrap();
        let app = test_app(dir.path());
        let get = Request::get("/api/knowledge/binding/nonsense/x")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(get).await.unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    /// A half-specified model pick (only `provider_id`, or only `model`) is a
    /// 400 BadRequest — the validation runs before any completer/path work,
    /// so it fires even with no completer wired and an arbitrary root_path.
    #[tokio::test]
    async fn description_generate_rejects_half_specified_model() {
        let dir = tempfile::TempDir::new().unwrap();
        let app = test_app(dir.path());

        for body in [
            r#"{"name":"x","root_path":"/tmp/x","provider_id":"p1"}"#,
            r#"{"name":"x","root_path":"/tmp/x","model":"m1"}"#,
        ] {
            let req = Request::post("/api/knowledge/description/generate")
                .header("content-type", "application/json")
                .body(Body::from(body))
                .unwrap();
            let resp = app.clone().oneshot(req).await.unwrap();
            assert_eq!(resp.status(), StatusCode::BAD_REQUEST, "body={body}");
            let v = json_body(resp).await;
            assert!(
                v["error"].as_str().unwrap_or_default().contains("supplied together"),
                "{v}"
            );
        }
    }

    /// The matching `model_override` unit contract: both or neither.
    #[test]
    fn model_override_requires_both_or_neither() {
        assert_eq!(model_override(None, None).unwrap(), None);
        assert_eq!(
            model_override(Some("p".into()), Some("m".into())).unwrap(),
            Some(("p".into(), "m".into()))
        );
        // Blank strings collapse to "absent" — so blank+blank is None, not an error.
        assert_eq!(model_override(Some("  ".into()), Some("  ".into())).unwrap(), None);
        assert!(model_override(Some("p".into()), None).is_err());
        assert!(model_override(None, Some("m".into())).is_err());
        assert!(model_override(Some("p".into()), Some("  ".into())).is_err());
    }

    #[tokio::test]
    async fn manual_search_returns_scoped_hits() {
        let dir = tempfile::TempDir::new().unwrap();
        let app = test_app(dir.path());

        // 1. Create a knowledge base.
        let resp = app
            .clone()
            .oneshot(
                Request::post("/api/knowledge/bases")
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"name":"规范","description":""}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let v = json_body(resp).await;
        let kb_id = v["data"]["id"].as_str().unwrap().to_owned();

        // 2. Write a file with a keyword.
        let write_body = serde_json::json!({
            "path": "a.md",
            "content": "# 评审\n选中态用 primary-1"
        });
        let resp = app
            .clone()
            .oneshot(
                Request::put(format!("/api/knowledge/bases/{kb_id}/file"))
                    .header("content-type", "application/json")
                    .body(Body::from(write_body.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK, "write_file failed");

        // 3. Search via the new route.
        let search_body = serde_json::json!({
            "kbIds": [kb_id],
            "query": "评审",
            "limit": 10
        });
        let resp = app
            .clone()
            .oneshot(
                Request::post("/api/knowledge/search")
                    .header("content-type", "application/json")
                    .body(Body::from(search_body.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let v = json_body(resp).await;
        let hits = v["data"].as_array().expect("data should be an array");
        assert!(
            hits.iter().any(|h| h["kb_id"].as_str() == Some(&kb_id)),
            "expected hit for kb_id={kb_id}, got {v}"
        );
    }

    #[tokio::test]
    async fn manual_search_empty_kb_ids_returns_empty() {
        let dir = tempfile::TempDir::new().unwrap();
        let app = test_app(dir.path());

        let search_body = serde_json::json!({
            "kbIds": [],
            "query": "anything"
        });
        let resp = app
            .oneshot(
                Request::post("/api/knowledge/search")
                    .header("content-type", "application/json")
                    .body(Body::from(search_body.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let v = json_body(resp).await;
        let hits = v["data"].as_array().expect("data should be an array");
        assert!(hits.is_empty(), "empty kb_ids should return empty hits");
    }
}

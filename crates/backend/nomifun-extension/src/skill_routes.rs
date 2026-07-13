use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use axum::Router;
use axum::extract::rejection::JsonRejection;
use axum::extract::{Json, Path as AxumPath, State};
use axum::routing::{delete, get, post, put};

use nomifun_api_types::{
    AddExternalPathRequest, ApiResponse, BuiltinAutoSkillResponse, ExportSkillRequest, ExternalSkillSourceResponse,
    ImportSkillRequest, ImportSkillResponse, MaterializeSkillsRequest, MaterializeSkillsResponse, MaterializedSkillRef,
    NamedPathResponse, ReadAssistantRuleRequest, ReadBuiltinResourceRequest, ReadSkillInfoRequest,
    ReadSkillInfoResponse, RemoveExternalPathRequest, ScanForSkillsRequest, ScanForSkillsResponse,
    ScannedSkillResponse, SetSkillTagsRequest, SkillListItemResponse, SkillPathsResponse, SkillSourceResponse,
    SkillMarketItemResponse, SkillMarketSyncRequest, SkillMarketSyncResponse, WriteAssistantRuleRequest,
};
use nomifun_common::AppError;
use nomifun_db::ISkillTagRepository;

use crate::classifier::AssistantRuleDispatcher;
use crate::external_paths::ExternalPathsManager;
use crate::skill_service::{self, SkillPaths, SkillSource};

fn to_source_response(source: SkillSource) -> SkillSourceResponse {
    match source {
        SkillSource::Builtin => SkillSourceResponse::Builtin,
        SkillSource::Custom => SkillSourceResponse::Custom,
        SkillSource::Extension => SkillSourceResponse::Extension,
    }
}

// ---------------------------------------------------------------------------
// Router state
// ---------------------------------------------------------------------------

/// Shared state for skill/rule route handlers.
#[derive(Clone)]
pub struct SkillRouterState {
    pub skill_paths: SkillPaths,
    pub external_paths_manager: Arc<ExternalPathsManager>,
    /// Optional dispatcher that routes assistant-rule / assistant-skill
    /// read/write/delete by source (builtin / extension / user). When
    /// `None`, the legacy user-directory-only behavior is preserved.
    #[allow(clippy::type_complexity)]
    pub assistant_dispatcher: Option<Arc<dyn AssistantRuleDispatcher>>,
    /// Per-skill tag assignment repo (user assignments/overrides).
    pub skill_tag_repo: Arc<dyn ISkillTagRepository>,
    /// Built-in skill tag seed: skill name → (audience_tags, scenario_tags).
    pub builtin_skill_tags: Arc<HashMap<String, (Vec<String>, Vec<String>)>>,
}

// ---------------------------------------------------------------------------
// Router builder
// ---------------------------------------------------------------------------

/// Build the skill router with all `/api/skills/*` routes.
///
/// All routes require authentication (applied by the caller).
pub fn skill_routes(state: SkillRouterState) -> Router {
    Router::new()
        // Skill listing & info
        .route("/api/skills", get(list_skills))
        .route("/api/skills/builtin-auto", get(list_builtin_auto_skills))
        .route("/api/skills/{name}/tags", put(set_skill_tags))
        .route("/api/skills/info", post(read_skill_info))
        .route("/api/skills/paths", get(get_skill_paths))
        // Import / export / delete
        .route("/api/skills/import", post(import_skill))
        .route("/api/skills/import-symlink", post(import_skill_symlink))
        .route("/api/skills/export-symlink", post(export_skill_symlink))
        .route("/api/skills/{name}", delete(delete_skill))
        // Scanning & discovery
        .route("/api/skills/scan", post(scan_for_skills))
        .route("/api/skills/detect-paths", get(detect_paths))
        .route("/api/skills/detect-external", get(detect_external))
        // Built-in resources
        .route("/api/skills/builtin-rule", post(read_builtin_rule))
        .route("/api/skills/builtin-skill", post(read_builtin_skill))
        // Per-agent skill resolution (for agent CLI symlink layout).
        .route("/api/skills/materialize-for-agent", post(materialize_for_agent))
        // Assistant rules CRUD
        .route("/api/skills/assistant-rule/read", post(read_assistant_rule))
        .route("/api/skills/assistant-rule/write", post(write_assistant_rule))
        .route("/api/skills/assistant-rule/{id}", delete(delete_assistant_rule))
        // Assistant skills CRUD
        .route("/api/skills/assistant-skill/read", post(read_assistant_skill))
        .route("/api/skills/assistant-skill/write", post(write_assistant_skill))
        .route("/api/skills/assistant-skill/{id}", delete(delete_assistant_skill))
        // External path management
        .route(
            "/api/skills/external-paths",
            get(get_external_paths)
                .post(add_external_path)
                .delete(remove_external_path),
        )
        // Skills market
        .route("/api/skills/market/enable", post(enable_skills_market))
        .route("/api/skills/market/disable", post(disable_skills_market))
        .route(
            "/api/skills/market/rankings/sync",
            post(sync_skill_market_rankings),
        )
        .with_state(state)
}

// ---------------------------------------------------------------------------
// Skill listing & info
// ---------------------------------------------------------------------------

/// `GET /api/skills` — list all available skills.
async fn list_skills(
    State(state): State<SkillRouterState>,
) -> Result<Json<ApiResponse<Vec<SkillListItemResponse>>>, AppError> {
    let items = skill_service::list_available_skills(&state.skill_paths).await?;
    let builtin_display = skill_service::load_builtin_skill_display_metadata();
    // user sidecar assignments (decode JSON arrays), keyed by skill name
    let user_rows = state.skill_tag_repo.get_all().await.map_err(AppError::from)?;
    let mut user_map: HashMap<String, (Vec<String>, Vec<String>)> = HashMap::new();
    for r in user_rows {
        let aud = decode_tags(r.audience_tags.as_deref());
        let scn = decode_tags(r.scenario_tags.as_deref());
        user_map.insert(r.skill_name, (aud, scn));
    }
    let resp: Vec<SkillListItemResponse> = items
        .into_iter()
        .map(|s| {
            let display = if s.source == skill_service::SkillSource::Builtin {
                builtin_display.get(&s.name).cloned().unwrap_or_default()
            } else {
                Default::default()
            };
            let (audience_tags, scenario_tags) = user_map
                .get(&s.name)
                .cloned()
                .or_else(|| state.builtin_skill_tags.get(&s.name).cloned())
                .unwrap_or_default();
            SkillListItemResponse {
                name: s.name,
                description: s.description,
                name_i18n: display.name_i18n,
                description_i18n: display.description_i18n,
                location: s.location,
                relative_location: s.relative_location,
                is_custom: s.is_custom,
                source: to_source_response(s.source),
                audience_tags,
                scenario_tags,
            }
        })
        .collect();
    Ok(Json(ApiResponse::ok(resp)))
}

/// Decode a JSON-array TEXT column into a `Vec<String>`. Fail-soft on purpose
/// (intentionally unlike `nomifun-assistant`'s `decode_str_list`, which 500s on
/// bad JSON): this is the read path for the skill list, so one corrupted sidecar
/// row must not break the whole listing — it degrades to no tags for that skill.
fn decode_tags(raw: Option<&str>) -> Vec<String> {
    match raw {
        Some(s) if !s.is_empty() => serde_json::from_str(s).unwrap_or_default(),
        _ => Vec::new(),
    }
}

/// `PUT /api/skills/{name}/tags` — set a skill's tag assignment (user sidecar).
async fn set_skill_tags(
    State(state): State<SkillRouterState>,
    AxumPath(name): AxumPath<String>,
    body: Result<Json<SetSkillTagsRequest>, JsonRejection>,
) -> Result<Json<ApiResponse<()>>, AppError> {
    let Json(req) = body.map_err(|e| AppError::BadRequest(e.to_string()))?;
    let aud = serde_json::to_string(&req.audience_tags).map_err(|e| AppError::Internal(e.to_string()))?;
    let scn = serde_json::to_string(&req.scenario_tags).map_err(|e| AppError::Internal(e.to_string()))?;
    state
        .skill_tag_repo
        .upsert(&nomifun_db::UpsertSkillTagParams {
            skill_name: &name,
            audience_tags: Some(&aud),
            scenario_tags: Some(&scn),
        })
        .await
        .map_err(AppError::from)?;
    Ok(Json(ApiResponse::success()))
}

/// `GET /api/skills/builtin-auto` — list auto-injected built-in skills.
async fn list_builtin_auto_skills(
    State(state): State<SkillRouterState>,
) -> Result<Json<ApiResponse<Vec<BuiltinAutoSkillResponse>>>, AppError> {
    let items = skill_service::list_builtin_auto_skills(&state.skill_paths).await?;
    let builtin_display = skill_service::load_builtin_skill_display_metadata();
    let resp: Vec<BuiltinAutoSkillResponse> = items
        .into_iter()
        .map(|s| {
            let display = builtin_display.get(&s.name).cloned().unwrap_or_default();
            BuiltinAutoSkillResponse {
                name: s.name,
                description: s.description,
                name_i18n: display.name_i18n,
                description_i18n: display.description_i18n,
                location: s.location,
            }
        })
        .collect();
    Ok(Json(ApiResponse::ok(resp)))
}

/// `POST /api/skills/info` — read skill info without importing.
async fn read_skill_info(
    body: Result<Json<ReadSkillInfoRequest>, JsonRejection>,
) -> Result<Json<ApiResponse<ReadSkillInfoResponse>>, AppError> {
    let Json(req) = body.map_err(|e| AppError::BadRequest(e.to_string()))?;
    let (name, description) = skill_service::read_skill_info(Path::new(&req.skill_path)).await?;
    Ok(Json(ApiResponse::ok(ReadSkillInfoResponse { name, description })))
}

/// `GET /api/skills/paths` — get user and built-in skill directories.
async fn get_skill_paths(
    State(state): State<SkillRouterState>,
) -> Result<Json<ApiResponse<SkillPathsResponse>>, AppError> {
    let (user_dir, builtin_dir) = skill_service::get_skill_paths(&state.skill_paths);
    Ok(Json(ApiResponse::ok(SkillPathsResponse {
        user_skills_dir: user_dir,
        builtin_skills_dir: builtin_dir,
    })))
}

// ---------------------------------------------------------------------------
// Import / export / delete
// ---------------------------------------------------------------------------

/// `POST /api/skills/import` — import a skill by copying.
async fn import_skill(
    State(state): State<SkillRouterState>,
    body: Result<Json<ImportSkillRequest>, JsonRejection>,
) -> Result<Json<ApiResponse<ImportSkillResponse>>, AppError> {
    let Json(req) = body.map_err(|e| AppError::BadRequest(e.to_string()))?;
    let name = skill_service::import_skill(&state.skill_paths, Path::new(&req.skill_path)).await?;
    Ok(Json(ApiResponse::ok(ImportSkillResponse {
        skill_name: name.clone(),
        skill_names: vec![name],
    })))
}

/// `POST /api/skills/import-symlink` — import a skill by symlink.
async fn import_skill_symlink(
    State(state): State<SkillRouterState>,
    body: Result<Json<ImportSkillRequest>, JsonRejection>,
) -> Result<Json<ApiResponse<ImportSkillResponse>>, AppError> {
    let Json(req) = body.map_err(|e| AppError::BadRequest(e.to_string()))?;
    let names = skill_service::import_skills_with_symlink(&state.skill_paths, Path::new(&req.skill_path)).await?;
    let first_name = names.first().cloned().unwrap_or_default();
    Ok(Json(ApiResponse::ok(ImportSkillResponse {
        skill_name: first_name,
        skill_names: names,
    })))
}

/// `POST /api/skills/export-symlink` — export a skill symlink.
async fn export_skill_symlink(
    body: Result<Json<ExportSkillRequest>, JsonRejection>,
) -> Result<Json<ApiResponse<()>>, AppError> {
    let Json(req) = body.map_err(|e| AppError::BadRequest(e.to_string()))?;
    skill_service::export_skill_with_symlink(Path::new(&req.skill_path), Path::new(&req.target_dir)).await?;
    Ok(Json(ApiResponse::success()))
}

/// `DELETE /api/skills/:name` — delete a user-custom skill.
async fn delete_skill(
    State(state): State<SkillRouterState>,
    AxumPath(name): AxumPath<String>,
) -> Result<Json<ApiResponse<()>>, AppError> {
    skill_service::delete_skill(&state.skill_paths, &name).await?;
    Ok(Json(ApiResponse::success()))
}

// ---------------------------------------------------------------------------
// Scanning & discovery
// ---------------------------------------------------------------------------

/// `POST /api/skills/scan` — scan a directory for skills.
async fn scan_for_skills(
    body: Result<Json<ScanForSkillsRequest>, JsonRejection>,
) -> Result<Json<ApiResponse<ScanForSkillsResponse>>, AppError> {
    let Json(req) = body.map_err(|e| AppError::BadRequest(e.to_string()))?;
    let skills = skill_service::scan_for_skills(Path::new(&req.folder_path)).await?;
    let resp = ScanForSkillsResponse {
        skills: skills
            .into_iter()
            .map(|s| ScannedSkillResponse {
                name: s.name,
                description: s.description,
                path: s.path,
            })
            .collect(),
    };
    Ok(Json(ApiResponse::ok(resp)))
}

/// `GET /api/skills/detect-paths` — detect common skill paths.
async fn detect_paths() -> Result<Json<ApiResponse<Vec<NamedPathResponse>>>, AppError> {
    let paths = skill_service::detect_common_skill_paths().await;
    let resp: Vec<NamedPathResponse> = paths
        .into_iter()
        .map(|p| NamedPathResponse {
            name: p.name,
            path: p.path,
        })
        .collect();
    Ok(Json(ApiResponse::ok(resp)))
}

/// `GET /api/skills/detect-external` — discover external skills from all sources.
async fn detect_external(
    State(state): State<SkillRouterState>,
) -> Result<Json<ApiResponse<Vec<ExternalSkillSourceResponse>>>, AppError> {
    let custom = state.external_paths_manager.get_custom_external_paths().await;
    let sources = skill_service::detect_and_count_external_skills(&custom).await;
    let resp: Vec<ExternalSkillSourceResponse> = sources
        .into_iter()
        .map(|s| ExternalSkillSourceResponse {
            name: s.name,
            path: s.path,
            source: s.source,
            skill_count: s.skill_count,
            skills: s
                .skills
                .into_iter()
                .map(|sk| ScannedSkillResponse {
                    name: sk.name,
                    description: sk.description,
                    path: sk.path,
                })
                .collect(),
        })
        .collect();
    Ok(Json(ApiResponse::ok(resp)))
}

// ---------------------------------------------------------------------------
// Built-in resources
// ---------------------------------------------------------------------------

/// `POST /api/skills/builtin-rule` — read a built-in rule file.
async fn read_builtin_rule(
    State(state): State<SkillRouterState>,
    body: Result<Json<ReadBuiltinResourceRequest>, JsonRejection>,
) -> Result<Json<ApiResponse<String>>, AppError> {
    let Json(req) = body.map_err(|e| AppError::BadRequest(e.to_string()))?;
    let content = skill_service::read_builtin_rule(&state.skill_paths, &req.file_name).await?;
    Ok(Json(ApiResponse::ok(content)))
}

/// `POST /api/skills/builtin-skill` — read a built-in skill file.
async fn read_builtin_skill(
    State(state): State<SkillRouterState>,
    body: Result<Json<ReadBuiltinResourceRequest>, JsonRejection>,
) -> Result<Json<ApiResponse<String>>, AppError> {
    let Json(req) = body.map_err(|e| AppError::BadRequest(e.to_string()))?;
    let content = skill_service::read_builtin_skill(&state.skill_paths, &req.file_name).await?;
    Ok(Json(ApiResponse::ok(content)))
}

/// `POST /api/skills/materialize-for-agent` — resolve each requested skill
/// name to its on-disk source directory. The frontend symlinks each
/// returned `source_path` into the agent CLI's native skills dir. The
/// backend no longer copies any files per-conversation.
async fn materialize_for_agent(
    State(state): State<SkillRouterState>,
    body: Result<Json<MaterializeSkillsRequest>, JsonRejection>,
) -> Result<Json<ApiResponse<MaterializeSkillsResponse>>, AppError> {
    let Json(req) = body.map_err(|e| AppError::BadRequest(e.to_string()))?;
    if req.conversation_id <= 0 {
        return Err(AppError::BadRequest("conversationId must not be empty".into()));
    }
    let conversation_id = req.conversation_id.to_string();
    let resolved =
        skill_service::materialize_skills_for_agent(&state.skill_paths, &conversation_id, &req.skills).await?;
    let skills: Vec<MaterializedSkillRef> = resolved
        .into_iter()
        .map(|s| MaterializedSkillRef {
            name: s.name,
            source_path: s.source_path.to_string_lossy().into_owned(),
        })
        .collect();
    Ok(Json(ApiResponse::ok(MaterializeSkillsResponse { skills })))
}

// ---------------------------------------------------------------------------
// Assistant rules CRUD
// ---------------------------------------------------------------------------

/// `POST /api/skills/assistant-rule/read` — read an assistant rule.
///
/// Dispatches by source via [`AssistantRuleDispatcher`] when wired; falls
/// back to user-directory-only legacy behavior otherwise.
async fn read_assistant_rule(
    State(state): State<SkillRouterState>,
    body: Result<Json<ReadAssistantRuleRequest>, JsonRejection>,
) -> Result<Json<ApiResponse<String>>, AppError> {
    let Json(req) = body.map_err(|e| AppError::BadRequest(e.to_string()))?;
    if let Some(dispatcher) = &state.assistant_dispatcher {
        let content = dispatcher.read_rule(&req.assistant_id, req.locale.as_deref()).await?;
        return Ok(Json(ApiResponse::ok(content)));
    }
    let content =
        skill_service::read_assistant_rule(&state.skill_paths, &req.assistant_id, req.locale.as_deref()).await?;
    Ok(Json(ApiResponse::ok(content)))
}

/// `POST /api/skills/assistant-rule/write` — write an assistant rule.
///
/// Dispatches by source: builtin / extension ids reject with 400.
async fn write_assistant_rule(
    State(state): State<SkillRouterState>,
    body: Result<Json<WriteAssistantRuleRequest>, JsonRejection>,
) -> Result<Json<ApiResponse<bool>>, AppError> {
    let Json(req) = body.map_err(|e| AppError::BadRequest(e.to_string()))?;
    if let Some(dispatcher) = &state.assistant_dispatcher {
        dispatcher
            .write_rule(&req.assistant_id, req.locale.as_deref(), &req.content)
            .await?;
        return Ok(Json(ApiResponse::ok(true)));
    }
    let ok = skill_service::write_assistant_rule(
        &state.skill_paths,
        &req.assistant_id,
        &req.content,
        req.locale.as_deref(),
    )
    .await?;
    Ok(Json(ApiResponse::ok(ok)))
}

/// `DELETE /api/skills/assistant-rule/:id` — delete all locale versions.
async fn delete_assistant_rule(
    State(state): State<SkillRouterState>,
    AxumPath(id): AxumPath<String>,
) -> Result<Json<ApiResponse<bool>>, AppError> {
    if let Some(dispatcher) = &state.assistant_dispatcher {
        let ok = dispatcher.delete_rule(&id).await?;
        return Ok(Json(ApiResponse::ok(ok)));
    }
    let ok = skill_service::delete_assistant_rule(&state.skill_paths, &id).await?;
    Ok(Json(ApiResponse::ok(ok)))
}

// ---------------------------------------------------------------------------
// Assistant skills CRUD
// ---------------------------------------------------------------------------

/// `POST /api/skills/assistant-skill/read` — read an assistant skill.
///
/// Dispatches by source via [`AssistantRuleDispatcher`] when wired.
async fn read_assistant_skill(
    State(state): State<SkillRouterState>,
    body: Result<Json<ReadAssistantRuleRequest>, JsonRejection>,
) -> Result<Json<ApiResponse<String>>, AppError> {
    let Json(req) = body.map_err(|e| AppError::BadRequest(e.to_string()))?;
    if let Some(dispatcher) = &state.assistant_dispatcher {
        let content = dispatcher.read_skill(&req.assistant_id, req.locale.as_deref()).await?;
        return Ok(Json(ApiResponse::ok(content)));
    }
    let content =
        skill_service::read_assistant_skill(&state.skill_paths, &req.assistant_id, req.locale.as_deref()).await?;
    Ok(Json(ApiResponse::ok(content)))
}

/// `POST /api/skills/assistant-skill/write` — write an assistant skill.
///
/// Dispatches by source: builtin / extension ids reject with 400.
async fn write_assistant_skill(
    State(state): State<SkillRouterState>,
    body: Result<Json<WriteAssistantRuleRequest>, JsonRejection>,
) -> Result<Json<ApiResponse<bool>>, AppError> {
    let Json(req) = body.map_err(|e| AppError::BadRequest(e.to_string()))?;
    if let Some(dispatcher) = &state.assistant_dispatcher {
        dispatcher
            .write_skill(&req.assistant_id, req.locale.as_deref(), &req.content)
            .await?;
        return Ok(Json(ApiResponse::ok(true)));
    }
    let ok = skill_service::write_assistant_skill(
        &state.skill_paths,
        &req.assistant_id,
        &req.content,
        req.locale.as_deref(),
    )
    .await?;
    Ok(Json(ApiResponse::ok(ok)))
}

/// `DELETE /api/skills/assistant-skill/:id` — delete all locale versions.
async fn delete_assistant_skill(
    State(state): State<SkillRouterState>,
    AxumPath(id): AxumPath<String>,
) -> Result<Json<ApiResponse<bool>>, AppError> {
    if let Some(dispatcher) = &state.assistant_dispatcher {
        let ok = dispatcher.delete_skill(&id).await?;
        return Ok(Json(ApiResponse::ok(ok)));
    }
    let ok = skill_service::delete_assistant_skill(&state.skill_paths, &id).await?;
    Ok(Json(ApiResponse::ok(ok)))
}

// ---------------------------------------------------------------------------
// External path management
// ---------------------------------------------------------------------------

/// `GET /api/skills/external-paths` — list custom external paths.
async fn get_external_paths(
    State(state): State<SkillRouterState>,
) -> Result<Json<ApiResponse<Vec<NamedPathResponse>>>, AppError> {
    let paths = state.external_paths_manager.get_custom_external_paths().await;
    let resp: Vec<NamedPathResponse> = paths
        .into_iter()
        .map(|p| NamedPathResponse {
            name: p.name,
            path: p.path,
        })
        .collect();
    Ok(Json(ApiResponse::ok(resp)))
}

/// `POST /api/skills/external-paths` — add a custom external path.
async fn add_external_path(
    State(state): State<SkillRouterState>,
    body: Result<Json<AddExternalPathRequest>, JsonRejection>,
) -> Result<Json<ApiResponse<()>>, AppError> {
    let Json(req) = body.map_err(|e| AppError::BadRequest(e.to_string()))?;
    state
        .external_paths_manager
        .add_custom_external_path(&req.name, &req.path)
        .await?;
    Ok(Json(ApiResponse::success()))
}

/// `DELETE /api/skills/external-paths` — remove a custom external path.
async fn remove_external_path(
    State(state): State<SkillRouterState>,
    body: Result<Json<RemoveExternalPathRequest>, JsonRejection>,
) -> Result<Json<ApiResponse<()>>, AppError> {
    let Json(req) = body.map_err(|e| AppError::BadRequest(e.to_string()))?;
    state
        .external_paths_manager
        .remove_custom_external_path(&req.path)
        .await?;
    Ok(Json(ApiResponse::success()))
}

// ---------------------------------------------------------------------------
// Skills market
// ---------------------------------------------------------------------------

/// `POST /api/skills/market/enable` — enable the nomifun skills market.
async fn enable_skills_market(State(state): State<SkillRouterState>) -> Result<Json<ApiResponse<()>>, AppError> {
    state.external_paths_manager.enable_skills_market().await?;
    Ok(Json(ApiResponse::success()))
}

/// `POST /api/skills/market/disable` — disable the nomifun skills market.
async fn disable_skills_market(State(state): State<SkillRouterState>) -> Result<Json<ApiResponse<()>>, AppError> {
    state.external_paths_manager.disable_skills_market().await?;
    Ok(Json(ApiResponse::success()))
}

async fn sync_skill_market_rankings(
    body: Result<Json<SkillMarketSyncRequest>, JsonRejection>,
) -> Result<Json<ApiResponse<SkillMarketSyncResponse>>, AppError> {
    let Json(req) = body.map_err(|e| AppError::BadRequest(e.to_string()))?;
    let resp = fetch_skill_market_rankings(req.sources).await?;
    Ok(Json(ApiResponse::ok(resp)))
}

const CLAWHUB_SOURCE: &str = "clawhub";
const SHILLHUB_SOURCE: &str = "shillhub";
const CLAWHUB_RANKING_URL: &str = "https://clawhub.ai/";
const SHILLHUB_RANKING_URL: &str = "https://www.skills.sh/";
const MAX_MARKET_BODY_BYTES: u64 = 1_000_000;
const MAX_MARKET_ITEMS_PER_SOURCE: usize = 40;

async fn fetch_skill_market_rankings(sources: Vec<String>) -> Result<SkillMarketSyncResponse, AppError> {
    let selected = normalize_market_sources(sources)?;
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::limited(3))
        .timeout(Duration::from_secs(12))
        .user_agent("NomiFun-SkillMarket/1.0")
        .build()
        .map_err(|e| AppError::Internal(e.to_string()))?;

    let mut items = Vec::new();
    let mut errors = Vec::new();

    for source in selected {
        match fetch_market_source(&client, source).await {
            Ok(mut source_items) => items.append(&mut source_items),
            Err(error) => errors.push(format!("{source}: {error}")),
        }
    }

    Ok(SkillMarketSyncResponse {
        fetched_at: now_epoch_ms(),
        items,
        errors,
    })
}

fn normalize_market_sources(sources: Vec<String>) -> Result<Vec<&'static str>, AppError> {
    if sources.is_empty() {
        return Ok(vec![CLAWHUB_SOURCE, SHILLHUB_SOURCE]);
    }

    let mut selected = Vec::new();
    let mut seen = HashSet::new();
    for source in sources {
        let normalized = source.trim().to_ascii_lowercase();
        let source = match normalized.as_str() {
            CLAWHUB_SOURCE => CLAWHUB_SOURCE,
            SHILLHUB_SOURCE => SHILLHUB_SOURCE,
            other => return Err(AppError::BadRequest(format!("unsupported skill market source: {other}"))),
        };
        if seen.insert(source) {
            selected.push(source);
        }
    }
    Ok(selected)
}

async fn fetch_market_source(
    client: &reqwest::Client,
    source: &'static str,
) -> Result<Vec<SkillMarketItemResponse>, AppError> {
    let url = match source {
        CLAWHUB_SOURCE => CLAWHUB_RANKING_URL,
        SHILLHUB_SOURCE => SHILLHUB_RANKING_URL,
        _ => return Err(AppError::BadRequest("unsupported skill market source".into())),
    };

    let response = client.get(url).send().await.map_err(map_market_fetch_error)?;
    if !response.status().is_success() {
        return Err(AppError::BadGateway(format!("ranking page returned {}", response.status())));
    }
    if response.content_length().unwrap_or(0) > MAX_MARKET_BODY_BYTES {
        return Err(AppError::BadGateway("ranking page is too large".into()));
    }

    let bytes = response.bytes().await.map_err(map_market_fetch_error)?;
    if bytes.len() as u64 > MAX_MARKET_BODY_BYTES {
        return Err(AppError::BadGateway("ranking page is too large".into()));
    }

    let html = String::from_utf8_lossy(&bytes);
    Ok(match source {
        CLAWHUB_SOURCE => parse_clawhub_rankings(&html),
        SHILLHUB_SOURCE => parse_shillhub_rankings(&html),
        _ => Vec::new(),
    })
}

fn map_market_fetch_error(error: reqwest::Error) -> AppError {
    if error.is_timeout() {
        AppError::Timeout(format!("skill market fetch timed out: {error}"))
    } else {
        AppError::BadGateway(format!("skill market fetch failed: {error}"))
    }
}

fn parse_clawhub_rankings(html: &str) -> Vec<SkillMarketItemResponse> {
    let mut seen = HashSet::new();
    let mut parsed = Vec::new();

    for (href, text) in market_anchors(html) {
        let Some(url) = market_url(CLAWHUB_SOURCE, &href) else {
            continue;
        };
        let Some((owner, slug)) = clawhub_owner_slug(&url) else {
            continue;
        };
        let id = format!("{CLAWHUB_SOURCE}:{owner}/{slug}");
        if !seen.insert(id.clone()) {
            continue;
        }

        let name = extract_clawhub_name(&text, &owner, &slug);
        let description = extract_clawhub_description(&text, &owner, &name);
        let stats = extract_stats(&text);
        let (tags, audience_tags, scenario_tags) = infer_market_tags(&format!("{name} {description}"));
        let rank = parsed.len() + 1;
        parsed.push(SkillMarketItemResponse {
            id,
            source: CLAWHUB_SOURCE.into(),
            rank,
            name,
            description,
            url: format!("https://clawhub.ai/{owner}/skills/{slug}"),
            install_command: format!("openclaw skills install @{owner}/{slug}"),
            tags,
            audience_tags,
            scenario_tags,
            stats,
        });
        if parsed.len() >= MAX_MARKET_ITEMS_PER_SOURCE {
            break;
        }
    }

    parsed
}

fn parse_shillhub_rankings(html: &str) -> Vec<SkillMarketItemResponse> {
    let mut seen = HashSet::new();
    let mut parsed = Vec::new();

    for (href, text) in market_anchors(html) {
        let Some(url) = market_url(SHILLHUB_SOURCE, &href) else {
            continue;
        };
        let Some((owner, slug)) = skills_sh_owner_slug(&url) else {
            continue;
        };
        let id = format!("{SHILLHUB_SOURCE}:{owner}/skills/{slug}");
        if !seen.insert(id.clone()) {
            continue;
        }

        let name = extract_skills_sh_name(&text, &owner, &slug);
        let stats = extract_stats(&text);
        let description = extract_skills_sh_description(&text, &owner, &name, stats.as_deref());
        let (tags, audience_tags, scenario_tags) = infer_market_tags(&format!("{name} {description}"));
        let install_command = if owner.contains('.') {
            format!("npx skills add https://www.skills.sh/{owner}/skills/{slug}")
        } else {
            format!("npx skills add https://github.com/{owner}/skills --skill {slug}")
        };
        let rank = parsed.len() + 1;
        parsed.push(SkillMarketItemResponse {
            id,
            source: SHILLHUB_SOURCE.into(),
            rank,
            name,
            description,
            url: format!("https://www.skills.sh/{owner}/skills/{slug}"),
            install_command,
            tags,
            audience_tags,
            scenario_tags,
            stats,
        });
        if parsed.len() >= MAX_MARKET_ITEMS_PER_SOURCE {
            break;
        }
    }

    parsed
}

fn market_anchors(html: &str) -> Vec<(String, String)> {
    let anchor_re = regex::Regex::new(r#"(?is)<a\b[^>]*\bhref=["']([^"']+)["'][^>]*>(.*?)</a>"#).unwrap();
    anchor_re
        .captures_iter(html)
        .filter_map(|cap| {
            let href = cap.get(1)?.as_str().trim();
            let inner = cap.get(2)?.as_str();
            let text = clean_market_text(&strip_html_tags(inner), 360);
            if href.is_empty() || text.is_empty() {
                return None;
            }
            Some((href.to_string(), text))
        })
        .collect()
}

fn market_url(source: &str, href: &str) -> Option<String> {
    let href = href.trim();
    if href.starts_with('#') || href.starts_with("mailto:") || href.starts_with("javascript:") {
        return None;
    }
    let url = if href.starts_with("http://") || href.starts_with("https://") {
        href.to_string()
    } else if href.starts_with('/') {
        match source {
            CLAWHUB_SOURCE => format!("https://clawhub.ai{href}"),
            SHILLHUB_SOURCE => format!("https://www.skills.sh{href}"),
            _ => return None,
        }
    } else {
        return None;
    };

    match source {
        CLAWHUB_SOURCE if url.starts_with("https://clawhub.ai/") => Some(url),
        SHILLHUB_SOURCE if url.starts_with("https://www.skills.sh/") || url.starts_with("https://skills.sh/") => {
            Some(url.replacen("https://skills.sh/", "https://www.skills.sh/", 1))
        }
        _ => None,
    }
}

fn clawhub_owner_slug(url: &str) -> Option<(String, String)> {
    let segments = market_path_segments(url, "https://clawhub.ai")?;
    let reserved = ["skills", "plugins", "docs", "about", "login", "sign-in", "search"];
    if segments.len() >= 3 && segments.get(1).is_some_and(|s| s == "skills") {
        return valid_owner_slug(&segments[0], &segments[2]);
    }
    if segments.len() == 2 && !reserved.contains(&segments[0].as_str()) && !reserved.contains(&segments[1].as_str()) {
        return valid_owner_slug(&segments[0], &segments[1]);
    }
    None
}

fn skills_sh_owner_slug(url: &str) -> Option<(String, String)> {
    let segments = market_path_segments(url, "https://www.skills.sh")?;
    if segments.len() >= 3 && segments.get(1).is_some_and(|s| s == "skills") {
        return valid_owner_slug(&segments[0], &segments[2]);
    }
    None
}

fn market_path_segments(url: &str, origin: &str) -> Option<Vec<String>> {
    let rest = url.strip_prefix(origin)?;
    let path = rest
        .split(['?', '#'])
        .next()
        .unwrap_or_default()
        .trim_matches('/');
    if path.is_empty() {
        return None;
    }
    Some(path.split('/').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect())
}

fn valid_owner_slug(owner: &str, slug: &str) -> Option<(String, String)> {
    if is_market_slug(owner) && is_market_slug(slug) {
        Some((owner.to_string(), slug.to_string()))
    } else {
        None
    }
}

fn is_market_slug(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 96
        && value
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
}

fn extract_clawhub_name(text: &str, owner: &str, slug: &str) -> String {
    let owner_marker = format!("@ {owner}");
    let before_owner = text
        .split(&owner_marker)
        .next()
        .unwrap_or(text)
        .split('@')
        .next()
        .unwrap_or(text);
    let candidate = clean_market_text(before_owner.trim_matches(|c: char| c == '#' || c.is_ascii_digit()), 80);
    if candidate.len() >= 2 {
        candidate
    } else {
        title_from_slug(slug)
    }
}

fn extract_clawhub_description(text: &str, owner: &str, name: &str) -> String {
    let owner_marker = format!("@ {owner}");
    let tail = text.split(&owner_marker).nth(1).unwrap_or(text);
    let cleaned = strip_known_stats(tail);
    let cleaned = clean_market_text(&cleaned.replace(name, ""), 180);
    if cleaned.len() >= 12 {
        cleaned
    } else {
        "Trending ClawHub skill package.".into()
    }
}

fn extract_skills_sh_name(text: &str, owner: &str, slug: &str) -> String {
    let repo_marker = format!("{owner}/skills");
    let before_repo = text.split(&repo_marker).next().unwrap_or(text);
    let candidate = clean_market_text(
        before_repo.trim_matches(|c: char| c == '#' || c.is_ascii_digit() || c == '.'),
        80,
    );
    if candidate.len() >= 2 && !candidate.eq_ignore_ascii_case("skill") {
        candidate
    } else {
        title_from_slug(slug)
    }
}

fn extract_skills_sh_description(text: &str, owner: &str, name: &str, stats: Option<&str>) -> String {
    let without_stats = stats.map_or_else(|| text.to_string(), |s| text.replace(s, ""));
    let without_repo = without_stats.replace(&format!("{owner}/skills"), "");
    let cleaned = clean_market_text(&without_repo.replace(name, ""), 180);
    if cleaned.len() >= 18 {
        cleaned
    } else {
        format!("Ranked Skills.sh skill from {owner}/skills.")
    }
}

fn extract_stats(text: &str) -> Option<String> {
    let stats_re =
        regex::Regex::new(r"(?i)(\d+(?:\.\d+)?\s*[km]?\+?\s*(?:installs?|downloads?|uses?|stars?)?)").unwrap();
    let mut matches = stats_re
        .captures_iter(text)
        .filter_map(|cap| cap.get(1).map(|m| clean_market_text(m.as_str(), 40)))
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>();
    matches.dedup();
    matches.last().cloned()
}

fn strip_known_stats(text: &str) -> String {
    let stats_re = regex::Regex::new(r"(?i)\d+(?:\.\d+)?\s*[km]?\+?\s*(?:installs?|downloads?|uses?|stars?)?").unwrap();
    stats_re.replace_all(text, " ").to_string()
}

fn strip_html_tags(html: &str) -> String {
    let tag_re = regex::Regex::new(r"(?is)<[^>]+>").unwrap();
    tag_re.replace_all(html, " ").to_string()
}

fn clean_market_text(text: &str, max_chars: usize) -> String {
    let decoded = text
        .replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&nbsp;", " ");
    let mut out = String::new();
    let mut last_was_space = false;
    for ch in decoded.chars() {
        let is_space = ch.is_whitespace() || ch.is_control();
        if is_space {
            if !last_was_space {
                out.push(' ');
                last_was_space = true;
            }
        } else {
            out.push(ch);
            last_was_space = false;
        }
        if out.chars().count() >= max_chars {
            break;
        }
    }
    out.trim().to_string()
}

fn title_from_slug(slug: &str) -> String {
    slug.split(['-', '_'])
        .filter(|part| !part.is_empty())
        .map(|part| {
            let mut chars = part.chars();
            match chars.next() {
                Some(first) => format!("{}{}", first.to_ascii_uppercase(), chars.as_str()),
                None => String::new(),
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn infer_market_tags(text: &str) -> (Vec<String>, Vec<String>, Vec<String>) {
    let lower = text.to_ascii_lowercase();
    let mut audience = Vec::new();
    let mut scenario = Vec::new();

    if contains_any(&lower, &["code", "github", "git", "api", "cli", "npm", "python", "typescript", "developer"]) {
        audience.push("developer".to_string());
        scenario.push("coding".to_string());
    }
    if contains_any(&lower, &["doc", "pdf", "word", "office", "excel", "sheet", "ppt", "slide"]) {
        audience.push("office".to_string());
        if contains_any(&lower, &["excel", "sheet", "spreadsheet"]) {
            scenario.push("spreadsheet".to_string());
        }
        if contains_any(&lower, &["ppt", "slide", "presentation"]) {
            scenario.push("presentation".to_string());
        }
        if contains_any(&lower, &["doc", "pdf", "word"]) {
            scenario.push("document".to_string());
        }
    }
    if contains_any(&lower, &["design", "image", "figma", "ui", "ux"]) {
        audience.push("designer".to_string());
        scenario.push("design".to_string());
    }
    if contains_any(&lower, &["research", "paper", "academic", "web search"]) {
        audience.push("student".to_string());
        scenario.push("research".to_string());
    }
    if contains_any(&lower, &["write", "blog", "copy", "content"]) {
        scenario.push("writing".to_string());
    }
    if contains_any(&lower, &["plan", "project", "task", "calendar"]) {
        scenario.push("planning".to_string());
    }
    if contains_any(&lower, &["social", "tweet", "x.com", "marketing"]) {
        audience.push("marketing".to_string());
        scenario.push("social".to_string());
    }
    if contains_any(&lower, &["setup", "install", "configure", "config"]) {
        scenario.push("setup".to_string());
    }

    dedup_strings(&mut audience);
    dedup_strings(&mut scenario);
    let mut tags = audience.clone();
    tags.extend(scenario.clone());
    dedup_strings(&mut tags);
    (tags, audience, scenario)
}

fn contains_any(text: &str, needles: &[&str]) -> bool {
    needles.iter().any(|needle| text.contains(needle))
}

fn dedup_strings(values: &mut Vec<String>) {
    let mut seen = HashSet::new();
    values.retain(|value| seen.insert(value.clone()));
}

fn now_epoch_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as i64)
        .unwrap_or(0)
}

// ---------------------------------------------------------------------------
// Unit tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Default)]
    struct InMemorySkillTagRepo {
        rows: std::sync::Mutex<Vec<nomifun_db::SkillTagRow>>,
    }
    #[async_trait::async_trait]
    impl nomifun_db::ISkillTagRepository for InMemorySkillTagRepo {
        async fn get_all(&self) -> Result<Vec<nomifun_db::SkillTagRow>, nomifun_db::DbError> {
            Ok(self.rows.lock().unwrap().clone())
        }
        async fn upsert(
            &self,
            p: &nomifun_db::UpsertSkillTagParams<'_>,
        ) -> Result<nomifun_db::SkillTagRow, nomifun_db::DbError> {
            let row = nomifun_db::SkillTagRow {
                skill_name: p.skill_name.into(),
                audience_tags: p.audience_tags.map(String::from),
                scenario_tags: p.scenario_tags.map(String::from),
                updated_at: 0,
            };
            let mut g = self.rows.lock().unwrap();
            g.retain(|r| r.skill_name != row.skill_name);
            g.push(row.clone());
            Ok(row)
        }
        async fn delete(&self, name: &str) -> Result<bool, nomifun_db::DbError> {
            let mut g = self.rows.lock().unwrap();
            let before = g.len();
            g.retain(|r| r.skill_name != name);
            Ok(g.len() != before)
        }
    }

    async fn make_state() -> SkillRouterState {
        let tmp = tempfile::TempDir::new().unwrap();
        let paths = SkillPaths {
            data_dir: tmp.path().to_path_buf(),
            user_skills_dir: tmp.path().join("skills"),
            cron_skills_dir: tmp.path().join("cron").join("skills"),
            builtin_skills_dir: tmp.path().join("builtin-skills"),
            builtin_rules_dir: tmp.path().join("builtin-rules"),
            assistant_rules_dir: tmp.path().join("assistant-rules"),
            assistant_skills_dir: tmp.path().join("assistant-skills"),
        };
        let ext_mgr = Arc::new(ExternalPathsManager::with_file(tmp.path().join("paths.json")).await);
        std::mem::forget(tmp);
        SkillRouterState {
            skill_paths: paths,
            external_paths_manager: ext_mgr,
            assistant_dispatcher: None,
            skill_tag_repo: std::sync::Arc::new(InMemorySkillTagRepo::default()),
            builtin_skill_tags: std::sync::Arc::new(std::collections::HashMap::new()),
        }
    }

    #[tokio::test]
    async fn skill_routes_builds_router() {
        let state = make_state().await;
        let _router = skill_routes(state);
    }

    #[test]
    fn normalize_market_sources_rejects_unknown_source() {
        let err = normalize_market_sources(vec!["unknown".into()]).unwrap_err();
        assert!(err.to_string().contains("unsupported skill market source"));
    }

    #[test]
    fn parse_clawhub_rankings_extracts_safe_install_command() {
        let html = r#"
          <a href="/pskoett/skills/self-improving-agent">
            <span>self-improving agent</span>
            <span>@ pskoett</span>
            <p>Captures discoveries from agent sessions into reusable skills.</p>
            <span>468k installs</span>
          </a>
        "#;
        let items = parse_clawhub_rankings(html);
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].source, CLAWHUB_SOURCE);
        assert_eq!(items[0].install_command, "openclaw skills install @pskoett/self-improving-agent");
        assert!(items[0].url.starts_with("https://clawhub.ai/"));
    }

    #[test]
    fn parse_shillhub_rankings_extracts_skills_command() {
        let html = r#"
          <a href="/vercel-labs/skills/find-skills">
            <span>find-skills</span>
            <span>vercel-labs/skills</span>
            <span>2.5M installs</span>
          </a>
        "#;
        let items = parse_shillhub_rankings(html);
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].source, SHILLHUB_SOURCE);
        assert_eq!(
            items[0].install_command,
            "npx skills add https://github.com/vercel-labs/skills --skill find-skills"
        );
    }
}

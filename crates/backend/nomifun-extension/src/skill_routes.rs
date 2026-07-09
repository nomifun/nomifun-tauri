use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

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
    WriteAssistantRuleRequest,
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
}

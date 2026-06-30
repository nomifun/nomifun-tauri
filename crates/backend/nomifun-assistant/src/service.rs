//! Assistant service — three-source merge, CRUD, state overrides, import,
//! and source-dispatched rule/skill read/write helpers.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use nomifun_api_types::{
    AssistantResponse, AssistantSource, CreateAssistantRequest, ImportAssistantsRequest, ImportAssistantsResult,
    ImportError, SetAssistantStateRequest, UpdateAssistantRequest,
};
use nomifun_common::{AppError, now_ms};
use nomifun_db::{
    AssistantOverrideRow, AssistantRow, CreateAssistantParams, CreateAssistantTagParams,
    IAssistantOverrideRepository, IAssistantRepository, IAssistantTagRepository, IProviderRepository,
    UpdateAssistantParams, UpdateAssistantTagParams, UpsertOverrideParams,
};
use nomifun_extension::{AssistantClassifier, AssistantRuleDispatcher, ExtensionRegistry, ResolvedAssistant};
use serde_json;
use tracing::{debug, warn};

use crate::builtin::{AvatarAsset, BuiltinAssistant, BuiltinAssistantRegistry};

/// Aggregated business logic for `/api/assistants/*` and rule/skill dispatch.
pub struct AssistantService {
    repo: Arc<dyn IAssistantRepository>,
    override_repo: Arc<dyn IAssistantOverrideRepository>,
    tag_repo: Arc<dyn IAssistantTagRepository>,
    /// Used to infer a sane `preset_agent_type` default when the caller did
    /// not supply one. The historical default of `"gemini"` 400'd within
    /// 1 ms on machines without the Gemini CLI (ELECTRON-1J1 / 1KV); we now
    /// pick an agent that actually matches the configured provider list.
    provider_repo: Arc<dyn IProviderRepository>,
    builtin: Arc<BuiltinAssistantRegistry>,
    extension_registry: ExtensionRegistry,
    /// Root directory holding user-authored rule/skill md files and avatars.
    /// Defaults to `~/.nomifun/` but can be overridden for tests.
    user_data_dir: PathBuf,
}

impl AssistantService {
    /// Construct an `AssistantService` pinned to the runtime data directory.
    ///
    /// `user_data_dir` is the on-disk root for user-authored rule and skill
    /// `.md` files plus avatar uploads (`<user_data_dir>/assistant-rules/`,
    /// `<user_data_dir>/assistant-skills/`, `<user_data_dir>/assistant-avatars/`).
    /// Production code passes the same `services.data_dir` that the SQLite
    /// database lives under, so dev / packaged / multi-instance launches
    /// keep their rule files alongside the matching db. Tests pin a temp
    /// directory.
    ///
    /// There is no implicit `~/.nomifun` fallback on purpose: an earlier
    /// version had one, and dev builds silently wrote rule files to the
    /// release directory while the db lived under `~/.nomifun-dev/`,
    /// resulting in `read_rule` returning empty in dev mode. Forcing the
    /// caller to pass a path makes the wiring explicit.
    pub fn new(
        repo: Arc<dyn IAssistantRepository>,
        override_repo: Arc<dyn IAssistantOverrideRepository>,
        tag_repo: Arc<dyn IAssistantTagRepository>,
        provider_repo: Arc<dyn IProviderRepository>,
        builtin: Arc<BuiltinAssistantRegistry>,
        extension_registry: ExtensionRegistry,
        user_data_dir: PathBuf,
    ) -> Self {
        Self {
            repo,
            override_repo,
            tag_repo,
            provider_repo,
            builtin,
            extension_registry,
            user_data_dir,
        }
    }

    // -----------------------------------------------------------------------
    // Classification
    // -----------------------------------------------------------------------

    /// Classify an assistant id into its source.
    pub async fn classify_source(&self, id: &str) -> AssistantSource {
        if self.builtin.has(id) {
            return AssistantSource::Builtin;
        }
        if self.extension_registry.has_assistant(id).await {
            return AssistantSource::Extension;
        }
        AssistantSource::User
    }

    // -----------------------------------------------------------------------
    // List / Get
    // -----------------------------------------------------------------------

    /// Three-source merge (built-in + user + extension) with per-assistant
    /// override application. Also performs opportunistic orphan cleanup on
    /// the overrides table.
    pub async fn list(&self) -> Result<Vec<AssistantResponse>, AppError> {
        let user_rows = self.repo.list().await?;
        let extensions = self.extension_registry.get_assistants().await;
        let overrides = self.override_repo.get_all().await?;

        let overrides_map: HashMap<String, AssistantOverrideRow> =
            overrides.into_iter().map(|o| (o.assistant_id.clone(), o)).collect();

        let mut result = Vec::new();

        for b in self.builtin.all() {
            result.push(builtin_to_response(b, overrides_map.get(&b.id)));
        }
        for u in &user_rows {
            result.push(user_row_to_response(u, overrides_map.get(&u.id))?);
        }
        for e in &extensions {
            result.push(extension_to_response(e));
        }

        // Sort by sort_order asc, then last_used_at desc (newer first).
        result.sort_by(|a, b| {
            a.sort_order
                .cmp(&b.sort_order)
                .then_with(|| b.last_used_at.cmp(&a.last_used_at))
        });

        // Opportunistic orphan cleanup: any override row whose assistant_id no
        // longer appears in the merged list is stale.
        let valid_ids: Vec<&str> = result.iter().map(|a| a.id.as_str()).collect();
        if let Err(e) = self.override_repo.delete_orphans(&valid_ids).await {
            warn!("override orphan cleanup failed: {e}");
        }

        Ok(result)
    }

    pub async fn get(&self, id: &str) -> Result<AssistantResponse, AppError> {
        match self.classify_source(id).await {
            AssistantSource::Builtin => {
                let b = self
                    .builtin
                    .get(id)
                    .ok_or_else(|| AppError::NotFound(format!("assistant '{id}' not found")))?;
                let ov = self.override_repo.get(id).await?;
                Ok(builtin_to_response(b, ov.as_ref()))
            }
            AssistantSource::Extension => {
                let e = self
                    .extension_registry
                    .get_assistant_by_id(id)
                    .await
                    .ok_or_else(|| AppError::NotFound(format!("assistant '{id}' not found")))?;
                Ok(extension_to_response(&e))
            }
            AssistantSource::User => {
                let row = self
                    .repo
                    .get(id)
                    .await?
                    .ok_or_else(|| AppError::NotFound(format!("assistant '{id}' not found")))?;
                let ov = self.override_repo.get(id).await?;
                user_row_to_response(&row, ov.as_ref())
            }
        }
    }

    // -----------------------------------------------------------------------
    // Default-agent inference
    // -----------------------------------------------------------------------

    /// Pick a sane `preset_agent_type` default for newly created /
    /// imported assistants when the caller did not supply one.
    ///
    /// Inference rule (ELECTRON-1J1 / 1KV):
    /// 1. If any enabled provider exists (Anthropic, OpenAI, custom,
    ///    Bedrock, Vertex, …), return `"nomi"`. Nomi speaks both
    ///    OpenAI-compatible and Anthropic-protocol APIs over the
    ///    user-configured base URL and does not require any third-party
    ///    CLI to be installed. CLI-based agents (`claude`, `gemini`)
    ///    must be opted into explicitly via `preset_agent_type` because
    ///    the presence of an Anthropic API key does not imply that the
    ///    Claude Code CLI is on `PATH`.
    /// 2. Otherwise (no providers configured), return a `BadRequest`
    ///    error. The previous code silently fell back to `"gemini"`,
    ///    which on machines without the Gemini CLI 400'd within 1 ms
    ///    with `Agent 'Gemini CLI' CLI not found in PATH`.
    pub async fn resolve_default_agent_type(&self) -> Result<String, AppError> {
        let providers = self
            .provider_repo
            .list()
            .await
            .map_err(|e| AppError::Internal(format!("failed to list providers: {e}")))?;

        if providers.iter().any(|p| p.enabled) {
            Ok("nomi".to_string())
        } else {
            Err(AppError::BadRequest(
                "Cannot create assistant: no providers configured. Add a provider before creating an assistant, \
                 or pass an explicit `preset_agent_type` in the request body."
                    .into(),
            ))
        }
    }

    // -----------------------------------------------------------------------
    // Create / Update / Delete
    // -----------------------------------------------------------------------

    pub async fn create(&self, req: CreateAssistantRequest) -> Result<AssistantResponse, AppError> {
        let name = req.name.trim().to_string();
        if name.is_empty() {
            return Err(AppError::BadRequest("name is required".into()));
        }

        let id = match req.id.as_deref() {
            Some(s) if !s.is_empty() => s.to_string(),
            _ => generate_user_id(),
        };

        // Reject id collisions with built-in / extension-contributed.
        if self.builtin.has(&id) {
            return Err(AppError::BadRequest("Id conflicts with built-in assistant".into()));
        }
        if self.extension_registry.has_assistant(&id).await {
            return Err(AppError::BadRequest(
                "Id conflicts with extension-contributed assistant".into(),
            ));
        }

        let serialized = SerializedFields::from_create(&req)?;
        // Resolve the default agent type from the configured provider list
        // when the caller did not supply one. Avoids the historical
        // `"gemini"` fallback that 400'd within 1 ms on machines without
        // the Gemini CLI (ELECTRON-1J1, ELECTRON-1KV).
        let resolved_agent_type = match req.preset_agent_type.as_deref() {
            Some(s) if !s.is_empty() => s.to_string(),
            _ => self.resolve_default_agent_type().await?,
        };
        let params = CreateAssistantParams {
            id: &id,
            name: &name,
            description: req.description.as_deref(),
            avatar: req.avatar.as_deref(),
            preset_agent_type: &resolved_agent_type,
            enabled_skills: serialized.enabled_skills.as_deref(),
            custom_skill_names: serialized.custom_skill_names.as_deref(),
            disabled_builtin_skills: serialized.disabled_builtin_skills.as_deref(),
            prompts: serialized.prompts.as_deref(),
            models: serialized.models.as_deref(),
            name_i18n: serialized.name_i18n.as_deref(),
            description_i18n: serialized.description_i18n.as_deref(),
            prompts_i18n: serialized.prompts_i18n.as_deref(),
            audience_tags: serialized.audience_tags.as_deref(),
            scenario_tags: serialized.scenario_tags.as_deref(),
        };

        let row = self.repo.create(&params).await?;
        let ov = self.override_repo.get(&id).await?;
        user_row_to_response(&row, ov.as_ref())
    }

    pub async fn update(&self, id: &str, req: UpdateAssistantRequest) -> Result<AssistantResponse, AppError> {
        match self.classify_source(id).await {
            AssistantSource::Builtin => {
                // Built-in rows are sourced from the embedded bundle and can't
                // be mutated. Users may still override `preset_agent_type` —
                // that lives in the overrides table. Any other field on the
                // request is rejected so callers don't silently lose data.
                if req.name.is_some()
                    || req.description.is_some()
                    || req.avatar.is_some()
                    || req.enabled_skills.is_some()
                    || req.custom_skill_names.is_some()
                    || req.disabled_builtin_skills.is_some()
                    || req.prompts.is_some()
                    || req.models.is_some()
                    || req.name_i18n.is_some()
                    || req.description_i18n.is_some()
                    || req.prompts_i18n.is_some()
                    || req.audience_tags.is_some()
                    || req.scenario_tags.is_some()
                {
                    return Err(AppError::Forbidden(
                        "Only 'preset_agent_type' can be overridden on built-in assistants".into(),
                    ));
                }

                let preset_agent_type = req.preset_agent_type.as_deref().ok_or_else(|| {
                    AppError::BadRequest("'preset_agent_type' is required when updating a built-in assistant".into())
                })?;

                let existing = self.override_repo.get(id).await?;
                let enabled = existing.as_ref().is_none_or(|o| o.enabled);
                let sort_order = existing.as_ref().map(|o| o.sort_order).unwrap_or(0);
                let last_used_at = existing.as_ref().and_then(|o| o.last_used_at);

                let params = UpsertOverrideParams {
                    assistant_id: id,
                    enabled,
                    sort_order,
                    last_used_at,
                    preset_agent_type: Some(Some(preset_agent_type)),
                };
                self.override_repo.upsert(&params).await?;
                return self.get(id).await;
            }
            AssistantSource::Extension => {
                return Err(AppError::Forbidden(
                    "Cannot modify extension-contributed assistant".into(),
                ));
            }
            AssistantSource::User => {}
        }

        let serialized = SerializedFields::from_update(&req)?;
        let params = UpdateAssistantParams {
            name: req.name.as_deref(),
            description: req.description.as_ref().map(|s| Some(s.as_str())),
            avatar: req.avatar.as_ref().map(|s| Some(s.as_str())),
            preset_agent_type: req.preset_agent_type.as_deref(),
            enabled_skills: serialized.enabled_skills.as_ref().map(|s| Some(s.as_str())),
            custom_skill_names: serialized.custom_skill_names.as_ref().map(|s| Some(s.as_str())),
            disabled_builtin_skills: serialized.disabled_builtin_skills.as_ref().map(|s| Some(s.as_str())),
            prompts: serialized.prompts.as_ref().map(|s| Some(s.as_str())),
            models: serialized.models.as_ref().map(|s| Some(s.as_str())),
            name_i18n: serialized.name_i18n.as_ref().map(|s| Some(s.as_str())),
            description_i18n: serialized.description_i18n.as_ref().map(|s| Some(s.as_str())),
            prompts_i18n: serialized.prompts_i18n.as_ref().map(|s| Some(s.as_str())),
            audience_tags: serialized.audience_tags.as_ref().map(|s| Some(s.as_str())),
            scenario_tags: serialized.scenario_tags.as_ref().map(|s| Some(s.as_str())),
        };

        let row = self
            .repo
            .update(id, &params)
            .await?
            .ok_or_else(|| AppError::NotFound(format!("assistant '{id}' not found")))?;
        let ov = self.override_repo.get(id).await?;
        user_row_to_response(&row, ov.as_ref())
    }

    pub async fn delete(&self, id: &str) -> Result<(), AppError> {
        match self.classify_source(id).await {
            AssistantSource::Builtin => {
                return Err(AppError::Forbidden("Cannot delete built-in assistant".into()));
            }
            AssistantSource::Extension => {
                return Err(AppError::Forbidden(
                    "Cannot delete extension-contributed assistant".into(),
                ));
            }
            AssistantSource::User => {}
        }

        let removed = self.repo.delete(id).await?;
        if !removed {
            return Err(AppError::NotFound(format!("assistant '{id}' not found")));
        }

        // Drop the override row (best-effort).
        if let Err(e) = self.override_repo.delete(id).await {
            warn!("failed to remove override for deleted assistant '{id}': {e}");
        }

        // Best-effort filesystem cleanup.
        self.cleanup_user_assets(id);

        Ok(())
    }

    pub async fn set_state(&self, id: &str, req: SetAssistantStateRequest) -> Result<AssistantResponse, AppError> {
        match self.classify_source(id).await {
            AssistantSource::Extension => {
                return Err(AppError::BadRequest("Extension assistants are read-only".into()));
            }
            AssistantSource::Builtin => {}
            AssistantSource::User => {
                // Confirm the user row exists (otherwise 404).
                if self.repo.get(id).await?.is_none() {
                    return Err(AppError::NotFound(format!("assistant '{id}' not found")));
                }
            }
        }

        // Merge with existing override to preserve fields not in this request.
        let existing = self.override_repo.get(id).await?;
        let enabled = req
            .enabled
            .unwrap_or_else(|| existing.as_ref().is_none_or(|o| o.enabled));
        let sort_order = req
            .sort_order
            .or_else(|| existing.as_ref().map(|o| o.sort_order))
            .unwrap_or(0);
        let last_used_at = req
            .last_used_at
            .or_else(|| existing.as_ref().and_then(|o| o.last_used_at));

        let params = UpsertOverrideParams {
            assistant_id: id,
            enabled,
            sort_order,
            last_used_at,
            preset_agent_type: None,
        };
        self.override_repo.upsert(&params).await?;

        self.get(id).await
    }

    // -----------------------------------------------------------------------
    // Assistant tags
    // -----------------------------------------------------------------------

    /// List the merged tag vocabulary: built-in seed tags (from the embedded
    /// manifest) first, then user-created tags. The built-in portion is
    /// sorted by `(dimension, sort_order)` so the manifest's `sort_order`
    /// field is authoritative (Audience dimension before Scenario); user
    /// tags follow in repository order.
    pub async fn list_tags(&self) -> Result<Vec<nomifun_api_types::AssistantTagResponse>, AppError> {
        use nomifun_api_types::{AssistantTagDimension, AssistantTagResponse};
        let parse_dim = |d: &str| match d {
            "scenario" => AssistantTagDimension::Scenario,
            _ => AssistantTagDimension::Audience,
        };
        // Sort key for dimension ordering: Audience (0) before Scenario (1).
        let dim_rank = |d: AssistantTagDimension| -> u8 {
            match d {
                AssistantTagDimension::Audience => 0,
                AssistantTagDimension::Scenario => 1,
            }
        };
        let mut out: Vec<AssistantTagResponse> = self
            .builtin
            .tags()
            .iter()
            .map(|t| AssistantTagResponse {
                key: t.key.clone(),
                dimension: parse_dim(&t.dimension),
                label: t.label.clone(),
                label_i18n: t.label_i18n.clone(),
                sort_order: t.sort_order,
                builtin: true,
            })
            .collect();
        // Make the built-in `sort_order` authoritative: sort ONLY the
        // built-in portion by (dimension, sort_order) before user tags are
        // appended, preserving built-in-before-user ordering.
        out.sort_by(|a, b| {
            dim_rank(a.dimension)
                .cmp(&dim_rank(b.dimension))
                .then_with(|| a.sort_order.cmp(&b.sort_order))
        });
        for r in self.tag_repo.list().await.map_err(AppError::from)? {
            out.push(AssistantTagResponse {
                key: r.key,
                dimension: parse_dim(&r.dimension),
                label: r.label,
                label_i18n: std::collections::HashMap::new(),
                sort_order: r.sort_order,
                builtin: false,
            });
        }
        Ok(out)
    }

    pub async fn create_tag(
        &self,
        req: nomifun_api_types::CreateAssistantTagRequest,
    ) -> Result<nomifun_api_types::AssistantTagResponse, AppError> {
        use nomifun_api_types::{AssistantTagDimension, AssistantTagResponse};
        let label = req.label.trim().to_string();
        if label.is_empty() {
            return Err(AppError::BadRequest("tag label is required".into()));
        }
        let dimension = match req.dimension {
            AssistantTagDimension::Audience => "audience",
            AssistantTagDimension::Scenario => "scenario",
        };
        let key = generate_tag_key();
        let row = self
            .tag_repo
            .create(&CreateAssistantTagParams { key: &key, dimension, label: &label, sort_order: 0 })
            .await
            .map_err(AppError::from)?;
        Ok(AssistantTagResponse {
            key: row.key,
            dimension: req.dimension,
            label: row.label,
            label_i18n: std::collections::HashMap::new(),
            sort_order: row.sort_order,
            builtin: false,
        })
    }

    pub async fn update_tag(
        &self,
        key: &str,
        req: nomifun_api_types::UpdateAssistantTagRequest,
    ) -> Result<nomifun_api_types::AssistantTagResponse, AppError> {
        use nomifun_api_types::{AssistantTagDimension, AssistantTagResponse};
        // Built-in seed tags are not stored in the DB → not editable.
        if self.builtin.tags().iter().any(|t| t.key == key) {
            return Err(AppError::Forbidden("Built-in tags cannot be edited".into()));
        }
        let label = req.label.as_ref().map(|s| s.trim().to_string());
        let row = self
            .tag_repo
            .update(
                key,
                &UpdateAssistantTagParams { label: label.as_deref(), sort_order: req.sort_order },
            )
            .await
            .map_err(AppError::from)?
            .ok_or_else(|| AppError::NotFound(format!("tag '{key}' not found")))?;
        let dimension = if row.dimension == "scenario" {
            AssistantTagDimension::Scenario
        } else {
            AssistantTagDimension::Audience
        };
        Ok(AssistantTagResponse {
            key: row.key,
            dimension,
            label: row.label,
            label_i18n: std::collections::HashMap::new(),
            sort_order: row.sort_order,
            builtin: false,
        })
    }

    /// Delete a user tag and strip its key from every user assistant's tag
    /// arrays. Built-in seed tags cannot be deleted.
    pub async fn delete_tag(&self, key: &str) -> Result<(), AppError> {
        if self.builtin.tags().iter().any(|t| t.key == key) {
            return Err(AppError::Forbidden("Built-in tags cannot be deleted".into()));
        }
        let removed = self.tag_repo.delete(key).await.map_err(AppError::from)?;
        if !removed {
            return Err(AppError::NotFound(format!("tag '{key}' not found")));
        }
        // Strip from all user assistant rows (best-effort, per-row).
        for row in self.repo.list().await? {
            let mut aud = decode_str_list(row.audience_tags.as_deref())?;
            let mut scn = decode_str_list(row.scenario_tags.as_deref())?;
            let before = (aud.len(), scn.len());
            aud.retain(|k| k != key);
            scn.retain(|k| k != key);
            if (aud.len(), scn.len()) == before {
                continue;
            }
            let aud_s = encode_str_list(Some(&aud))?.unwrap_or_default();
            let scn_s = encode_str_list(Some(&scn))?.unwrap_or_default();
            let params = UpdateAssistantParams {
                audience_tags: Some(Some(&aud_s)),
                scenario_tags: Some(Some(&scn_s)),
                ..Default::default()
            };
            let _ = self.repo.update(&row.id, &params).await?;
        }
        Ok(())
    }

    // -----------------------------------------------------------------------
    // Import (insert-only, idempotent)
    // -----------------------------------------------------------------------

    /// Bulk insert-only import of legacy Electron config rows. Skip on
    /// built-in / extension id collision or already-imported user-id collision.
    /// Never overwrites an existing user row.
    pub async fn import(&self, req: ImportAssistantsRequest) -> Result<ImportAssistantsResult, AppError> {
        let mut result = ImportAssistantsResult::default();

        // Resolved-once cache for the inferred default agent type. We only
        // hit the provider repo when at least one row in the batch omits
        // `preset_agent_type` AND has cleared all the other skip conditions.
        let mut cached_default_agent_type: Option<String> = None;

        for entry in req.assistants {
            let id = entry
                .id
                .clone()
                .filter(|s| !s.is_empty())
                .unwrap_or_else(generate_user_id);

            if self.builtin.has(&id) {
                result.skipped += 1;
                continue;
            }
            if self.extension_registry.has_assistant(&id).await {
                result.skipped += 1;
                continue;
            }
            match self.repo.get(&id).await {
                Ok(Some(_)) => {
                    result.skipped += 1;
                    continue;
                }
                Ok(None) => {}
                Err(e) => {
                    result.failed += 1;
                    result.errors.push(ImportError {
                        id: id.clone(),
                        error: e.to_string(),
                    });
                    continue;
                }
            }

            let name = entry.name.trim().to_string();
            if name.is_empty() {
                result.failed += 1;
                result.errors.push(ImportError {
                    id,
                    error: "name is required".into(),
                });
                continue;
            }

            let serialized = match SerializedFields::from_create(&entry) {
                Ok(s) => s,
                Err(e) => {
                    result.failed += 1;
                    result.errors.push(ImportError {
                        id,
                        error: e.to_string(),
                    });
                    continue;
                }
            };

            // Mirror the create() path: prefer the caller-supplied value;
            // otherwise infer from the configured provider list.
            let resolved_agent_type = match entry.preset_agent_type.as_deref() {
                Some(s) if !s.is_empty() => s.to_string(),
                _ => match cached_default_agent_type.as_deref() {
                    Some(v) => v.to_string(),
                    None => match self.resolve_default_agent_type().await {
                        Ok(v) => {
                            cached_default_agent_type = Some(v.clone());
                            v
                        }
                        Err(e) => {
                            result.failed += 1;
                            result.errors.push(ImportError {
                                id,
                                error: e.to_string(),
                            });
                            continue;
                        }
                    },
                },
            };

            let params = CreateAssistantParams {
                id: &id,
                name: &name,
                description: entry.description.as_deref(),
                avatar: entry.avatar.as_deref(),
                preset_agent_type: &resolved_agent_type,
                enabled_skills: serialized.enabled_skills.as_deref(),
                custom_skill_names: serialized.custom_skill_names.as_deref(),
                disabled_builtin_skills: serialized.disabled_builtin_skills.as_deref(),
                prompts: serialized.prompts.as_deref(),
                models: serialized.models.as_deref(),
                name_i18n: serialized.name_i18n.as_deref(),
                description_i18n: serialized.description_i18n.as_deref(),
                prompts_i18n: serialized.prompts_i18n.as_deref(),
                audience_tags: serialized.audience_tags.as_deref(),
                scenario_tags: serialized.scenario_tags.as_deref(),
            };

            match self.repo.create(&params).await {
                Ok(_) => result.imported += 1,
                Err(nomifun_db::DbError::Conflict(_)) => {
                    // Someone raced us into the table — treat as skip to
                    // keep import idempotent across retries.
                    result.skipped += 1;
                }
                Err(e) => {
                    result.failed += 1;
                    result.errors.push(ImportError {
                        id,
                        error: e.to_string(),
                    });
                }
            }
        }

        Ok(result)
    }

    // -----------------------------------------------------------------------
    // Rule / skill dispatch helpers
    // -----------------------------------------------------------------------

    /// Read an assistant rule file, dispatching by source.
    pub async fn read_rule(&self, id: &str, locale: Option<&str>) -> Result<String, AppError> {
        match self.classify_source(id).await {
            AssistantSource::Builtin => {
                let locale = locale.unwrap_or("");
                Ok(self
                    .builtin
                    .rule_bytes(id, locale)
                    .and_then(|b| String::from_utf8(b).ok())
                    .unwrap_or_default())
            }
            AssistantSource::Extension => {
                // ResolvedAssistant doesn't expose rule content directly in
                // the current backend; return empty until extension schema
                // gains this field. Callers see empty == "no rule".
                Ok(String::new())
            }
            AssistantSource::User => {
                let path = self.user_rule_path(id, locale);
                Ok(read_file_or_empty(&path))
            }
        }
    }

    /// Write an assistant rule file. User only; built-in / extension reject.
    pub async fn write_rule(&self, id: &str, locale: Option<&str>, content: &str) -> Result<(), AppError> {
        match self.classify_source(id).await {
            AssistantSource::Builtin => Err(AppError::BadRequest("Cannot write rule for built-in assistant".into())),
            AssistantSource::Extension => Err(AppError::BadRequest(
                "Cannot write rule for extension-contributed assistant".into(),
            )),
            AssistantSource::User => {
                let path = self.user_rule_path(id, locale);
                if let Some(parent) = path.parent() {
                    std::fs::create_dir_all(parent)
                        .map_err(|e| AppError::Internal(format!("create dir failed: {e}")))?;
                }
                std::fs::write(&path, content).map_err(|e| AppError::Internal(format!("write failed: {e}")))?;
                Ok(())
            }
        }
    }

    /// Delete all locale versions of an assistant rule. User only.
    pub async fn delete_rule(&self, id: &str) -> Result<bool, AppError> {
        match self.classify_source(id).await {
            AssistantSource::Builtin => Err(AppError::BadRequest("Cannot delete rule for built-in assistant".into())),
            AssistantSource::Extension => Err(AppError::BadRequest(
                "Cannot delete rule for extension-contributed assistant".into(),
            )),
            AssistantSource::User => Ok(remove_assistant_md_files(&self.user_rules_dir(), id)),
        }
    }

    pub async fn read_skill(&self, id: &str, locale: Option<&str>) -> Result<String, AppError> {
        match self.classify_source(id).await {
            AssistantSource::Builtin => {
                let locale = locale.unwrap_or("");
                Ok(self
                    .builtin
                    .skill_bytes(id, locale)
                    .and_then(|b| String::from_utf8(b).ok())
                    .unwrap_or_default())
            }
            AssistantSource::Extension => Ok(String::new()),
            AssistantSource::User => {
                let path = self.user_skill_path(id, locale);
                Ok(read_file_or_empty(&path))
            }
        }
    }

    pub async fn write_skill(&self, id: &str, locale: Option<&str>, content: &str) -> Result<(), AppError> {
        match self.classify_source(id).await {
            AssistantSource::Builtin => Err(AppError::BadRequest("Cannot write skill for built-in assistant".into())),
            AssistantSource::Extension => Err(AppError::BadRequest(
                "Cannot write skill for extension-contributed assistant".into(),
            )),
            AssistantSource::User => {
                let path = self.user_skill_path(id, locale);
                if let Some(parent) = path.parent() {
                    std::fs::create_dir_all(parent)
                        .map_err(|e| AppError::Internal(format!("create dir failed: {e}")))?;
                }
                std::fs::write(&path, content).map_err(|e| AppError::Internal(format!("write failed: {e}")))?;
                Ok(())
            }
        }
    }

    pub async fn delete_skill(&self, id: &str) -> Result<bool, AppError> {
        match self.classify_source(id).await {
            AssistantSource::Builtin => Err(AppError::BadRequest(
                "Cannot delete skill for built-in assistant".into(),
            )),
            AssistantSource::Extension => Err(AppError::BadRequest(
                "Cannot delete skill for extension-contributed assistant".into(),
            )),
            AssistantSource::User => Ok(remove_assistant_md_files(&self.user_skills_dir(), id)),
        }
    }

    // -----------------------------------------------------------------------
    // Avatar helpers
    // -----------------------------------------------------------------------

    /// Resolve the avatar bytes for an assistant together with its file
    /// extension (for `Content-Type` inference).
    ///
    /// - Built-in source → read from the embedded bundle (or the disk
    ///   override when `NOMIFUN_BUILTIN_ASSISTANTS_PATH` is set).
    /// - User source → scan the user-writable avatars directory for a file
    ///   whose stem equals `id`.
    /// - Extension source → `None`; the frontend serves those via
    ///   `nomi-asset://`.
    ///
    /// Built-ins whose manifest `avatar` field is an inline emoji (and thus
    /// has no on-disk file) also return `None`; clients fall back to the
    /// text avatar for those.
    pub async fn avatar_asset(&self, id: &str) -> Option<AvatarAsset> {
        match self.classify_source(id).await {
            AssistantSource::Builtin => self.builtin.avatar_asset(id),
            AssistantSource::Extension => None,
            AssistantSource::User => {
                let dir = self.user_avatars_dir();
                let entries = std::fs::read_dir(&dir).ok()?;
                for entry in entries.flatten() {
                    let name = entry.file_name();
                    let name = name.to_string_lossy();
                    if let Some(stem) = name.split('.').next()
                        && stem == id
                    {
                        let bytes = std::fs::read(entry.path()).ok()?;
                        let extension = std::path::Path::new(name.as_ref())
                            .extension()
                            .and_then(|e| e.to_str())
                            .map(|s| s.to_ascii_lowercase());
                        return Some(AvatarAsset { bytes, extension });
                    }
                }
                None
            }
        }
    }

    // -----------------------------------------------------------------------
    // Internal helpers
    // -----------------------------------------------------------------------

    fn user_rules_dir(&self) -> PathBuf {
        self.user_data_dir.join("assistant-rules")
    }

    fn user_skills_dir(&self) -> PathBuf {
        self.user_data_dir.join("assistant-skills")
    }

    fn user_avatars_dir(&self) -> PathBuf {
        self.user_data_dir.join("assistant-avatars")
    }

    fn user_rule_path(&self, id: &str, locale: Option<&str>) -> PathBuf {
        assistant_md_path(&self.user_rules_dir(), id, locale)
    }

    fn user_skill_path(&self, id: &str, locale: Option<&str>) -> PathBuf {
        assistant_md_path(&self.user_skills_dir(), id, locale)
    }

    fn cleanup_user_assets(&self, id: &str) {
        remove_assistant_md_files(&self.user_rules_dir(), id);
        remove_assistant_md_files(&self.user_skills_dir(), id);
        remove_assistant_avatar_files(&self.user_avatars_dir(), id);
    }
}

#[async_trait::async_trait]
impl AssistantClassifier for AssistantService {
    async fn classify(&self, id: &str) -> AssistantSource {
        self.classify_source(id).await
    }
}

#[async_trait::async_trait]
impl AssistantRuleDispatcher for AssistantService {
    async fn read_rule(&self, id: &str, locale: Option<&str>) -> Result<String, AppError> {
        AssistantService::read_rule(self, id, locale).await
    }

    async fn write_rule(&self, id: &str, locale: Option<&str>, content: &str) -> Result<(), AppError> {
        AssistantService::write_rule(self, id, locale, content).await
    }

    async fn delete_rule(&self, id: &str) -> Result<bool, AppError> {
        AssistantService::delete_rule(self, id).await
    }

    async fn read_skill(&self, id: &str, locale: Option<&str>) -> Result<String, AppError> {
        AssistantService::read_skill(self, id, locale).await
    }

    async fn write_skill(&self, id: &str, locale: Option<&str>, content: &str) -> Result<(), AppError> {
        AssistantService::write_skill(self, id, locale, content).await
    }

    async fn delete_skill(&self, id: &str) -> Result<bool, AppError> {
        AssistantService::delete_skill(self, id).await
    }
}

// ---------------------------------------------------------------------------
// Response conversion
// ---------------------------------------------------------------------------

/// Last-resort fallback for the assistant `preset_agent_type` when no
/// provider list is reachable (extension-contributed rows, sync display
/// conversions). `"nomi"` is the only Nomi agent that does not require
/// a third-party CLI to be installed, so it never fails the
/// `Agent '<name>' CLI not found in PATH` guard at agent build time.
///
/// User- and import-created assistants take a different path: see
/// [`AssistantService::resolve_default_agent_type`], which inspects the
/// configured providers and returns a more specific default when possible.
const DEFAULT_AGENT_TYPE: &str = "nomi";

fn builtin_to_response(b: &BuiltinAssistant, ov: Option<&AssistantOverrideRow>) -> AssistantResponse {
    AssistantResponse {
        id: b.id.clone(),
        source: AssistantSource::Builtin,
        name: b.name.clone(),
        name_i18n: b.name_i18n.clone(),
        description: b.description.clone(),
        description_i18n: b.description_i18n.clone(),
        avatar: b.avatar.clone(),
        enabled: ov.map(|o| o.enabled).unwrap_or(true),
        sort_order: ov.map(|o| o.sort_order).unwrap_or(0),
        preset_agent_type: ov
            .and_then(|o| o.preset_agent_type.clone())
            .unwrap_or_else(|| b.preset_agent_type.clone()),
        enabled_skills: b.enabled_skills.clone(),
        custom_skill_names: b.custom_skill_names.clone(),
        disabled_builtin_skills: b.disabled_builtin_skills.clone(),
        context: None,
        context_i18n: HashMap::new(),
        prompts: b.prompts.clone(),
        prompts_i18n: b.prompts_i18n.clone(),
        models: b.models.clone(),
        audience_tags: b.audience_tags.clone(),
        scenario_tags: b.scenario_tags.clone(),
        last_used_at: ov.and_then(|o| o.last_used_at),
    }
}

fn user_row_to_response(row: &AssistantRow, ov: Option<&AssistantOverrideRow>) -> Result<AssistantResponse, AppError> {
    Ok(AssistantResponse {
        id: row.id.clone(),
        source: AssistantSource::User,
        name: row.name.clone(),
        name_i18n: decode_str_map(row.name_i18n.as_deref())?,
        description: row.description.clone(),
        description_i18n: decode_str_map(row.description_i18n.as_deref())?,
        avatar: row.avatar.clone(),
        enabled: ov.map(|o| o.enabled).unwrap_or(true),
        sort_order: ov.map(|o| o.sort_order).unwrap_or(0),
        preset_agent_type: row.preset_agent_type.clone(),
        enabled_skills: decode_str_list(row.enabled_skills.as_deref())?,
        custom_skill_names: decode_str_list(row.custom_skill_names.as_deref())?,
        disabled_builtin_skills: decode_str_list(row.disabled_builtin_skills.as_deref())?,
        context: None,
        context_i18n: HashMap::new(),
        prompts: decode_str_list(row.prompts.as_deref())?,
        prompts_i18n: decode_list_map(row.prompts_i18n.as_deref())?,
        models: decode_str_list(row.models.as_deref())?,
        audience_tags: decode_str_list(row.audience_tags.as_deref())?,
        scenario_tags: decode_str_list(row.scenario_tags.as_deref())?,
        last_used_at: ov.and_then(|o| o.last_used_at),
    })
}

fn extension_to_response(e: &ResolvedAssistant) -> AssistantResponse {
    AssistantResponse {
        id: e.id.clone(),
        source: AssistantSource::Extension,
        name: e.name.clone(),
        name_i18n: HashMap::new(),
        description: e.description.clone(),
        description_i18n: HashMap::new(),
        avatar: e.icon.clone(),
        enabled: true,
        sort_order: 0,
        preset_agent_type: DEFAULT_AGENT_TYPE.to_string(),
        enabled_skills: Vec::new(),
        custom_skill_names: Vec::new(),
        disabled_builtin_skills: Vec::new(),
        context: e.context.clone(),
        context_i18n: HashMap::new(),
        prompts: Vec::new(),
        prompts_i18n: HashMap::new(),
        models: Vec::new(),
        audience_tags: Vec::new(),
        scenario_tags: Vec::new(),
        last_used_at: None,
    }
}

// ---------------------------------------------------------------------------
// Serialization helpers
// ---------------------------------------------------------------------------

/// Serialized-JSON fragments for a single user-authored assistant row,
/// produced from either a create or update request.
struct SerializedFields {
    enabled_skills: Option<String>,
    custom_skill_names: Option<String>,
    disabled_builtin_skills: Option<String>,
    prompts: Option<String>,
    models: Option<String>,
    name_i18n: Option<String>,
    description_i18n: Option<String>,
    prompts_i18n: Option<String>,
    audience_tags: Option<String>,
    scenario_tags: Option<String>,
}

impl SerializedFields {
    fn from_create(req: &CreateAssistantRequest) -> Result<Self, AppError> {
        Ok(Self {
            enabled_skills: encode_str_list(req.enabled_skills.as_deref())?,
            custom_skill_names: encode_str_list(req.custom_skill_names.as_deref())?,
            disabled_builtin_skills: encode_str_list(req.disabled_builtin_skills.as_deref())?,
            prompts: encode_str_list(req.prompts.as_deref())?,
            models: encode_str_list(req.models.as_deref())?,
            name_i18n: encode_str_map(req.name_i18n.as_ref())?,
            description_i18n: encode_str_map(req.description_i18n.as_ref())?,
            prompts_i18n: encode_list_map(req.prompts_i18n.as_ref())?,
            audience_tags: encode_str_list(req.audience_tags.as_deref())?,
            scenario_tags: encode_str_list(req.scenario_tags.as_deref())?,
        })
    }

    fn from_update(req: &UpdateAssistantRequest) -> Result<Self, AppError> {
        Ok(Self {
            enabled_skills: encode_str_list(req.enabled_skills.as_deref())?,
            custom_skill_names: encode_str_list(req.custom_skill_names.as_deref())?,
            disabled_builtin_skills: encode_str_list(req.disabled_builtin_skills.as_deref())?,
            prompts: encode_str_list(req.prompts.as_deref())?,
            models: encode_str_list(req.models.as_deref())?,
            name_i18n: encode_str_map(req.name_i18n.as_ref())?,
            description_i18n: encode_str_map(req.description_i18n.as_ref())?,
            prompts_i18n: encode_list_map(req.prompts_i18n.as_ref())?,
            audience_tags: encode_str_list(req.audience_tags.as_deref())?,
            scenario_tags: encode_str_list(req.scenario_tags.as_deref())?,
        })
    }
}

fn encode_str_list(value: Option<&[String]>) -> Result<Option<String>, AppError> {
    match value {
        Some(v) => Ok(Some(
            serde_json::to_string(v).map_err(|e| AppError::Internal(format!("encode list: {e}")))?,
        )),
        None => Ok(None),
    }
}

fn encode_str_map(value: Option<&HashMap<String, String>>) -> Result<Option<String>, AppError> {
    match value {
        Some(v) => Ok(Some(
            serde_json::to_string(v).map_err(|e| AppError::Internal(format!("encode map: {e}")))?,
        )),
        None => Ok(None),
    }
}

fn encode_list_map(value: Option<&HashMap<String, Vec<String>>>) -> Result<Option<String>, AppError> {
    match value {
        Some(v) => Ok(Some(
            serde_json::to_string(v).map_err(|e| AppError::Internal(format!("encode map: {e}")))?,
        )),
        None => Ok(None),
    }
}

fn decode_str_list(raw: Option<&str>) -> Result<Vec<String>, AppError> {
    match raw {
        Some(s) if !s.is_empty() => {
            serde_json::from_str(s).map_err(|e| AppError::Internal(format!("decode list: {e}")))
        }
        _ => Ok(Vec::new()),
    }
}

fn decode_str_map(raw: Option<&str>) -> Result<HashMap<String, String>, AppError> {
    match raw {
        Some(s) if !s.is_empty() => serde_json::from_str(s).map_err(|e| AppError::Internal(format!("decode map: {e}"))),
        _ => Ok(HashMap::new()),
    }
}

fn decode_list_map(raw: Option<&str>) -> Result<HashMap<String, Vec<String>>, AppError> {
    match raw {
        Some(s) if !s.is_empty() => serde_json::from_str(s).map_err(|e| AppError::Internal(format!("decode map: {e}"))),
        _ => Ok(HashMap::new()),
    }
}

// ---------------------------------------------------------------------------
// Filesystem helpers
// ---------------------------------------------------------------------------

fn assistant_md_path(dir: &Path, id: &str, locale: Option<&str>) -> PathBuf {
    let filename = match locale {
        Some(loc) if !loc.is_empty() => format!("{id}.{loc}.md"),
        _ => format!("{id}.md"),
    };
    dir.join(filename)
}

fn read_file_or_empty(path: &Path) -> String {
    std::fs::read_to_string(path).unwrap_or_default()
}

/// Remove every `{id}*.md` file in `dir`. Returns `true` if any file was
/// deleted.
fn remove_assistant_md_files(dir: &Path, id: &str) -> bool {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return false;
    };
    let mut deleted = false;
    let prefix = format!("{id}.");
    let exact = format!("{id}.md");
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name == exact || (name.starts_with(&prefix) && name.ends_with(".md")) {
            if let Err(e) = std::fs::remove_file(entry.path()) {
                warn!("failed to remove {}: {e}", entry.path().display());
                continue;
            }
            deleted = true;
        }
    }
    deleted
}

fn remove_assistant_avatar_files(dir: &Path, id: &str) -> bool {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return false;
    };
    let mut deleted = false;
    let prefix = format!("{id}.");
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name.starts_with(&prefix) {
            if let Err(e) = std::fs::remove_file(entry.path()) {
                warn!("failed to remove {}: {e}", entry.path().display());
                continue;
            }
            deleted = true;
        }
    }
    deleted
}

/// Generate a new user-authored assistant id with millisecond-resolution
/// timestamp + 4 hex chars of randomness.
pub fn generate_user_id() -> String {
    // Use time + a pseudo-random 16-bit value (sufficient for collision-free
    // ids within the same millisecond for any realistic UI workflow).
    let ms = now_ms();
    // Best-effort 16-bit random: hash the current nanos.
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    let hex = format!("{:04x}", (nanos as u16) ^ 0xA5A5);
    debug!("generated user assistant id: custom-{ms}-{hex}");
    format!("custom-{ms}-{hex}")
}

/// Generate a user-created tag key (`utag-<ms>-<hex>`).
pub fn generate_tag_key() -> String {
    let ms = now_ms();
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    let hex = format!("{:04x}", (nanos as u16) ^ 0x5A5A);
    format!("utag-{ms}-{hex}")
}

// ---------------------------------------------------------------------------
// Unit tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use nomifun_db::{
        CreateProviderParams, SqliteAssistantOverrideRepository, SqliteAssistantRepository,
        SqliteAssistantTagRepository, SqliteProviderRepository, init_database_memory,
    };
    use nomifun_extension::ExtensionStateStore;
    use nomifun_realtime::BroadcastEventBus;
    use tempfile::TempDir;

    struct Fixture {
        service: AssistantService,
        provider_repo: Arc<dyn IProviderRepository>,
        _tmp: TempDir,
        _db: nomifun_db::Database,
    }

    /// Default fixture: seeded with a single OpenAI-compatible provider so
    /// `resolve_default_agent_type` returns `"nomi"`. Tests that need to
    /// exercise the no-provider or anthropic-only branches construct their
    /// own fixture via [`fixture_with_options`].
    async fn fixture() -> Fixture {
        fixture_with_options(FixtureOpts::default()).await
    }

    async fn fixture_with_builtins(builtins: Vec<BuiltinAssistant>) -> Fixture {
        fixture_with_options(FixtureOpts {
            builtins,
            ..Default::default()
        })
        .await
    }

    #[derive(Default)]
    struct FixtureOpts {
        builtins: Vec<BuiltinAssistant>,
        /// When `true`, no provider is seeded — used by the test that
        /// asserts the no-provider error path.
        no_default_provider: bool,
        /// When set, the seeded provider's `platform` is overridden.
        /// Defaults to `"openai"` so existing tests get an `"nomi"`
        /// default agent type.
        seed_platform: Option<&'static str>,
    }

    async fn fixture_with_options(opts: FixtureOpts) -> Fixture {
        let tmp = TempDir::new().unwrap();
        let db = init_database_memory().await.unwrap();
        let repo: Arc<dyn IAssistantRepository> = Arc::new(SqliteAssistantRepository::new(db.pool().clone()));
        let orepo: Arc<dyn IAssistantOverrideRepository> =
            Arc::new(SqliteAssistantOverrideRepository::new(db.pool().clone()));
        let trepo: Arc<dyn IAssistantTagRepository> =
            Arc::new(SqliteAssistantTagRepository::new(db.pool().clone()));
        let provider_repo: Arc<dyn IProviderRepository> = Arc::new(SqliteProviderRepository::new(db.pool().clone()));

        if !opts.no_default_provider {
            seed_provider(&*provider_repo, opts.seed_platform.unwrap_or("openai")).await;
        }

        // Write a manifest into a temp dir and load from it.
        let assets_dir = tmp.path().join("assets");
        std::fs::create_dir_all(&assets_dir).unwrap();
        let manifest_json = serde_json::json!({
            "version": "1.0.0",
            "assistants": opts
                .builtins
                .iter()
                .map(|b| {
                    serde_json::json!({
                        "id": b.id,
                        "name": b.name,
                        "preset_agent_type": b.preset_agent_type,
                        "rule_file": b.rule_file,
                        "skill_file": b.skill_file,
                    })
                })
                .collect::<Vec<_>>()
        });
        std::fs::write(
            assets_dir.join("assistants.json"),
            serde_json::to_string(&manifest_json).unwrap(),
        )
        .unwrap();
        let builtin_reg = Arc::new(BuiltinAssistantRegistry::load_from_dir(assets_dir));

        let event_bus = Arc::new(BroadcastEventBus::new(8));
        let ext_state_store = ExtensionStateStore::new(tmp.path().join("ext-states.json"));
        let extension_registry = ExtensionRegistry::new(ext_state_store, event_bus, "1.0.0".to_string());

        let service = AssistantService::new(
            repo,
            orepo,
            trepo,
            provider_repo.clone(),
            builtin_reg,
            extension_registry,
            tmp.path().to_path_buf(),
        );

        Fixture {
            service,
            provider_repo,
            _tmp: tmp,
            _db: db,
        }
    }

    async fn seed_provider(repo: &dyn IProviderRepository, platform: &str) {
        repo.create(CreateProviderParams {
            id: None,
            platform,
            name: "Test Provider",
            base_url: "https://example.invalid",
            api_key_encrypted: "stub",
            models: "[]",
            enabled: true,
            capabilities: "[]",
            context_limit: None,
            model_protocols: None,
            model_descriptions: None,
            model_enabled: None,
            model_health: None,
            bedrock_config: None,
            is_full_url: false,
        })
        .await
        .expect("seed provider");
    }

    fn mk_builtin(id: &str, name: &str) -> BuiltinAssistant {
        BuiltinAssistant {
            id: id.into(),
            name: name.into(),
            name_i18n: HashMap::new(),
            description: None,
            description_i18n: HashMap::new(),
            avatar: None,
            preset_agent_type: "gemini".into(),
            enabled_skills: Vec::new(),
            custom_skill_names: Vec::new(),
            disabled_builtin_skills: Vec::new(),
            rule_file: None,
            skill_file: None,
            prompts: Vec::new(),
            prompts_i18n: HashMap::new(),
            models: Vec::new(),
            audience_tags: Vec::new(),
            scenario_tags: Vec::new(),
        }
    }

    #[tokio::test]
    async fn list_empty_is_empty() {
        let fx = fixture().await;
        let list = fx.service.list().await.unwrap();
        assert!(list.is_empty());
    }

    #[tokio::test]
    async fn list_includes_builtin_and_user() {
        let fx = fixture_with_builtins(vec![mk_builtin("builtin-office", "Office")]).await;

        let created = fx
            .service
            .create(CreateAssistantRequest {
                id: Some("u1".into()),
                name: "Mine".into(),
                ..req_default()
            })
            .await
            .unwrap();
        assert_eq!(created.source, AssistantSource::User);

        let list = fx.service.list().await.unwrap();
        assert_eq!(list.len(), 2);
        assert!(list.iter().any(|a| a.id == "builtin-office"));
        assert!(list.iter().any(|a| a.id == "u1"));
    }

    #[tokio::test]
    async fn create_rejects_empty_name() {
        let fx = fixture().await;
        let err = fx
            .service
            .create(CreateAssistantRequest {
                id: Some("u1".into()),
                name: "   ".into(),
                ..req_default()
            })
            .await
            .unwrap_err();
        assert!(matches!(err, AppError::BadRequest(_)));
    }

    #[tokio::test]
    async fn create_rejects_builtin_id_collision() {
        let fx = fixture_with_builtins(vec![mk_builtin("builtin-office", "Office")]).await;
        let err = fx
            .service
            .create(CreateAssistantRequest {
                id: Some("builtin-office".into()),
                name: "Mine".into(),
                ..req_default()
            })
            .await
            .unwrap_err();
        assert!(matches!(err, AppError::BadRequest(_)));
    }

    #[tokio::test]
    async fn create_rejects_duplicate_user_id() {
        let fx = fixture().await;
        fx.service
            .create(CreateAssistantRequest {
                id: Some("u1".into()),
                name: "A".into(),
                ..req_default()
            })
            .await
            .unwrap();
        let err = fx
            .service
            .create(CreateAssistantRequest {
                id: Some("u1".into()),
                name: "B".into(),
                ..req_default()
            })
            .await
            .unwrap_err();
        assert!(matches!(err, AppError::Conflict(_)));
    }

    #[tokio::test]
    async fn update_rejects_builtin_non_preset_fields() {
        let fx = fixture_with_builtins(vec![mk_builtin("builtin-office", "Office")]).await;
        let err = fx
            .service
            .update(
                "builtin-office",
                UpdateAssistantRequest {
                    name: Some("New".into()),
                    ..Default::default()
                },
            )
            .await
            .unwrap_err();
        assert!(matches!(err, AppError::Forbidden(_)));
    }

    #[tokio::test]
    async fn update_builtin_preset_agent_type_writes_override() {
        let fx = fixture_with_builtins(vec![mk_builtin("builtin-office", "Office")]).await;
        let updated = fx
            .service
            .update(
                "builtin-office",
                UpdateAssistantRequest {
                    preset_agent_type: Some("claude".into()),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        assert_eq!(updated.source, AssistantSource::Builtin);
        assert_eq!(updated.preset_agent_type, "claude");
        // List view must reflect the override too.
        let listed = fx
            .service
            .list()
            .await
            .unwrap()
            .into_iter()
            .find(|a| a.id == "builtin-office")
            .unwrap();
        assert_eq!(listed.preset_agent_type, "claude");
    }

    #[tokio::test]
    async fn update_user_partial_preserves_other_fields() {
        let fx = fixture().await;
        fx.service
            .create(CreateAssistantRequest {
                id: Some("u1".into()),
                name: "original".into(),
                description: Some("desc".into()),
                ..req_default()
            })
            .await
            .unwrap();
        let updated = fx
            .service
            .update(
                "u1",
                UpdateAssistantRequest {
                    name: Some("renamed".into()),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        assert_eq!(updated.name, "renamed");
        assert_eq!(updated.description.as_deref(), Some("desc"));
    }

    #[tokio::test]
    async fn delete_user_removes_row_and_override() {
        let fx = fixture().await;
        fx.service
            .create(CreateAssistantRequest {
                id: Some("u1".into()),
                name: "A".into(),
                ..req_default()
            })
            .await
            .unwrap();
        fx.service
            .set_state(
                "u1",
                SetAssistantStateRequest {
                    enabled: Some(false),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        fx.service.delete("u1").await.unwrap();
        // list now empty
        let list = fx.service.list().await.unwrap();
        assert!(list.is_empty());
    }

    #[tokio::test]
    async fn delete_builtin_rejects() {
        let fx = fixture_with_builtins(vec![mk_builtin("builtin-office", "Office")]).await;
        let err = fx.service.delete("builtin-office").await.unwrap_err();
        assert!(matches!(err, AppError::Forbidden(_)));
    }

    #[tokio::test]
    async fn set_state_builtin_writes_override() {
        let fx = fixture_with_builtins(vec![mk_builtin("builtin-office", "Office")]).await;
        let resp = fx
            .service
            .set_state(
                "builtin-office",
                SetAssistantStateRequest {
                    enabled: Some(false),
                    sort_order: Some(7),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        assert!(!resp.enabled);
        assert_eq!(resp.sort_order, 7);
        assert_eq!(resp.source, AssistantSource::Builtin);
    }

    #[tokio::test]
    async fn set_state_user_404_when_missing() {
        let fx = fixture().await;
        let err = fx
            .service
            .set_state(
                "unknown",
                SetAssistantStateRequest {
                    enabled: Some(true),
                    ..Default::default()
                },
            )
            .await
            .unwrap_err();
        assert!(matches!(err, AppError::NotFound(_)));
    }

    #[tokio::test]
    async fn import_happy_path() {
        let fx = fixture().await;
        let res = fx
            .service
            .import(ImportAssistantsRequest {
                assistants: vec![
                    CreateAssistantRequest {
                        id: Some("u1".into()),
                        name: "A".into(),
                        ..req_default()
                    },
                    CreateAssistantRequest {
                        id: Some("u2".into()),
                        name: "B".into(),
                        ..req_default()
                    },
                ],
            })
            .await
            .unwrap();
        assert_eq!(res.imported, 2);
        assert_eq!(res.skipped, 0);
        assert_eq!(res.failed, 0);
    }

    #[tokio::test]
    async fn import_skips_builtin_collision() {
        let fx = fixture_with_builtins(vec![mk_builtin("builtin-office", "Office")]).await;
        let res = fx
            .service
            .import(ImportAssistantsRequest {
                assistants: vec![CreateAssistantRequest {
                    id: Some("builtin-office".into()),
                    name: "spoof".into(),
                    ..req_default()
                }],
            })
            .await
            .unwrap();
        assert_eq!(res.imported, 0);
        assert_eq!(res.skipped, 1);
    }

    #[tokio::test]
    async fn import_retry_is_idempotent() {
        let fx = fixture().await;
        let first = fx
            .service
            .import(ImportAssistantsRequest {
                assistants: vec![CreateAssistantRequest {
                    id: Some("u1".into()),
                    name: "A".into(),
                    ..req_default()
                }],
            })
            .await
            .unwrap();
        assert_eq!(first.imported, 1);

        let second = fx
            .service
            .import(ImportAssistantsRequest {
                assistants: vec![CreateAssistantRequest {
                    id: Some("u1".into()),
                    name: "A".into(),
                    ..req_default()
                }],
            })
            .await
            .unwrap();
        assert_eq!(second.imported, 0);
        assert_eq!(second.skipped, 1);
    }

    #[tokio::test]
    async fn import_fails_on_empty_name() {
        let fx = fixture().await;
        let res = fx
            .service
            .import(ImportAssistantsRequest {
                assistants: vec![CreateAssistantRequest {
                    id: Some("u1".into()),
                    name: "  ".into(),
                    ..req_default()
                }],
            })
            .await
            .unwrap();
        assert_eq!(res.imported, 0);
        assert_eq!(res.failed, 1);
        assert_eq!(res.errors.len(), 1);
        assert_eq!(res.errors[0].id, "u1");
    }

    #[tokio::test]
    async fn read_rule_user_returns_empty_when_missing() {
        let fx = fixture().await;
        fx.service
            .create(CreateAssistantRequest {
                id: Some("u1".into()),
                name: "A".into(),
                ..req_default()
            })
            .await
            .unwrap();
        let content = fx.service.read_rule("u1", Some("en-US")).await.unwrap();
        assert!(content.is_empty());
    }

    #[tokio::test]
    async fn write_rule_user_then_read_returns_same() {
        let fx = fixture().await;
        fx.service
            .create(CreateAssistantRequest {
                id: Some("u1".into()),
                name: "A".into(),
                ..req_default()
            })
            .await
            .unwrap();
        fx.service.write_rule("u1", Some("en-US"), "rule body").await.unwrap();
        let content = fx.service.read_rule("u1", Some("en-US")).await.unwrap();
        assert_eq!(content, "rule body");
    }

    #[tokio::test]
    async fn write_rule_builtin_rejects() {
        let fx = fixture_with_builtins(vec![mk_builtin("builtin-office", "Office")]).await;
        let err = fx
            .service
            .write_rule("builtin-office", Some("en-US"), "x")
            .await
            .unwrap_err();
        assert!(matches!(err, AppError::BadRequest(_)));
    }

    #[tokio::test]
    async fn read_rule_builtin_dispatches_to_manifest() {
        let tmp = TempDir::new().unwrap();
        let db = init_database_memory().await.unwrap();

        let assets_dir = tmp.path().join("assets");
        let rules_dir = assets_dir.join("rules");
        std::fs::create_dir_all(&rules_dir).unwrap();
        std::fs::write(rules_dir.join("office.en-US.md"), "office rules").unwrap();
        let manifest = serde_json::json!({
            "assistants": [{
                "id": "builtin-office",
                "name": "Office",
                "preset_agent_type": "gemini",
                "rule_file": "rules/office.{locale}.md",
            }]
        });
        std::fs::write(
            assets_dir.join("assistants.json"),
            serde_json::to_string(&manifest).unwrap(),
        )
        .unwrap();
        let builtin_reg = Arc::new(BuiltinAssistantRegistry::load_from_dir(assets_dir));

        let repo: Arc<dyn IAssistantRepository> = Arc::new(SqliteAssistantRepository::new(db.pool().clone()));
        let orepo: Arc<dyn IAssistantOverrideRepository> =
            Arc::new(SqliteAssistantOverrideRepository::new(db.pool().clone()));
        let trepo: Arc<dyn IAssistantTagRepository> =
            Arc::new(SqliteAssistantTagRepository::new(db.pool().clone()));
        let provider_repo: Arc<dyn IProviderRepository> = Arc::new(SqliteProviderRepository::new(db.pool().clone()));
        let event_bus = Arc::new(BroadcastEventBus::new(8));
        let ext_state_store = ExtensionStateStore::new(tmp.path().join("ext-states.json"));
        let extension_registry = ExtensionRegistry::new(ext_state_store, event_bus, "1.0.0".to_string());

        let service = AssistantService::new(
            repo,
            orepo,
            trepo,
            provider_repo,
            builtin_reg,
            extension_registry,
            tmp.path().to_path_buf(),
        );
        let content = service.read_rule("builtin-office", Some("en-US")).await.unwrap();
        assert_eq!(content, "office rules");
    }

    #[tokio::test]
    async fn classify_falls_back_to_user() {
        let fx = fixture().await;
        assert_eq!(fx.service.classify_source("ghost").await, AssistantSource::User);
    }

    #[tokio::test]
    async fn classify_builtin_wins() {
        let fx = fixture_with_builtins(vec![mk_builtin("builtin-office", "Office")]).await;
        assert_eq!(
            fx.service.classify_source("builtin-office").await,
            AssistantSource::Builtin
        );
    }

    // -----------------------------------------------------------------------
    // Default agent-type inference (ELECTRON-1J1 / 1KV regression coverage)
    // -----------------------------------------------------------------------

    /// Anthropic provider routes to Nomi, not the Claude Code CLI:
    /// having an Anthropic API key does not imply the user has
    /// `claude` on `PATH`. CLI-based agents must be opted into
    /// explicitly.
    #[tokio::test]
    async fn resolve_default_agent_type_routes_anthropic_provider_to_nomi() {
        let fx = fixture_with_options(FixtureOpts {
            seed_platform: Some("anthropic"),
            ..Default::default()
        })
        .await;
        let resolved = fx.service.resolve_default_agent_type().await.unwrap();
        assert_eq!(resolved, "nomi");
    }

    /// OpenAI / custom provider falls back to Nomi, the only Nomi
    /// agent that doesn't require a third-party CLI.
    #[tokio::test]
    async fn resolve_default_agent_type_falls_back_to_nomi_for_openai_provider() {
        let fx = fixture_with_options(FixtureOpts {
            seed_platform: Some("openai"),
            ..Default::default()
        })
        .await;
        let resolved = fx.service.resolve_default_agent_type().await.unwrap();
        assert_eq!(resolved, "nomi");
    }

    /// Custom (non-anthropic, non-openai) platform also routes to Nomi,
    /// which handles OpenAI-compatible custom URLs.
    #[tokio::test]
    async fn resolve_default_agent_type_handles_custom_platform_as_nomi() {
        let fx = fixture_with_options(FixtureOpts {
            seed_platform: Some("custom"),
            ..Default::default()
        })
        .await;
        let resolved = fx.service.resolve_default_agent_type().await.unwrap();
        assert_eq!(resolved, "nomi");
    }

    /// No providers → loud BadRequest with actionable text. Crucially,
    /// this no longer silently falls through to `"gemini"`.
    #[tokio::test]
    async fn resolve_default_agent_type_errors_when_no_providers() {
        let fx = fixture_with_options(FixtureOpts {
            no_default_provider: true,
            ..Default::default()
        })
        .await;
        let err = fx.service.resolve_default_agent_type().await.unwrap_err();
        match err {
            AppError::BadRequest(msg) => {
                assert!(
                    msg.to_lowercase().contains("no providers"),
                    "unexpected error message: {msg}"
                );
                assert!(
                    !msg.to_lowercase().contains("gemini"),
                    "error message must not mention gemini: {msg}"
                );
            }
            other => panic!("expected BadRequest, got {other:?}"),
        }
    }

    /// Disabled providers do not satisfy the inference; the resolver
    /// must treat them as if they were absent.
    #[tokio::test]
    async fn resolve_default_agent_type_ignores_disabled_providers() {
        let fx = fixture_with_options(FixtureOpts {
            no_default_provider: true,
            ..Default::default()
        })
        .await;

        // Seed a *disabled* provider directly via the repo; resolution
        // must still error out because no enabled provider exists.
        fx.provider_repo
            .create(CreateProviderParams {
                id: None,
                platform: "anthropic",
                name: "Disabled",
                base_url: "https://example.invalid",
                api_key_encrypted: "stub",
                models: "[]",
                enabled: false,
                capabilities: "[]",
                context_limit: None,
                model_protocols: None,
                model_descriptions: None,
                model_enabled: None,
                model_health: None,
                bedrock_config: None,
                is_full_url: false,
            })
            .await
            .unwrap();

        let err = fx.service.resolve_default_agent_type().await.unwrap_err();
        assert!(matches!(err, AppError::BadRequest(_)));
    }

    /// End-to-end regression for ELECTRON-1J1 / 1KV: creating an
    /// assistant with no `preset_agent_type` and no Gemini CLI installed
    /// must NOT default to `"gemini"`. Any enabled provider — Anthropic
    /// or otherwise — should resolve to `"nomi"`, the only built-in
    /// agent that doesn't depend on a third-party CLI being on `PATH`.
    #[tokio::test]
    async fn create_without_preset_does_not_default_to_gemini_when_provider_exists() {
        for platform in ["anthropic", "openai"] {
            let fx = fixture_with_options(FixtureOpts {
                seed_platform: Some(platform),
                ..Default::default()
            })
            .await;
            let created = fx
                .service
                .create(CreateAssistantRequest {
                    id: Some(format!("u-{platform}")),
                    name: "Mine".into(),
                    ..req_default()
                })
                .await
                .unwrap();
            assert_ne!(
                created.preset_agent_type, "gemini",
                "Gemini default would 400 within 1ms on machines without the CLI"
            );
            assert_eq!(
                created.preset_agent_type, "nomi",
                "{platform} provider should resolve to nomi"
            );
        }
    }

    /// Explicit `preset_agent_type` in the request body wins over the
    /// inferred default — callers that know what they want stay in
    /// control.
    #[tokio::test]
    async fn create_respects_explicit_preset_agent_type() {
        let fx = fixture_with_options(FixtureOpts {
            seed_platform: Some("anthropic"),
            ..Default::default()
        })
        .await;
        let created = fx
            .service
            .create(CreateAssistantRequest {
                id: Some("u1".into()),
                name: "Mine".into(),
                preset_agent_type: Some("codex".into()),
                ..req_default()
            })
            .await
            .unwrap();
        assert_eq!(created.preset_agent_type, "codex");
    }

    #[tokio::test]
    async fn tag_lifecycle_and_strip_on_delete() {
        let fx = fixture().await;
        let tag = fx
            .service
            .create_tag(nomifun_api_types::CreateAssistantTagRequest {
                dimension: nomifun_api_types::AssistantTagDimension::Audience,
                label: "营销".into(),
            })
            .await
            .unwrap();
        // attach to a user assistant
        fx.service
            .create(CreateAssistantRequest {
                id: Some("u1".into()),
                name: "A".into(),
                audience_tags: Some(vec![tag.key.clone()]),
                ..req_default()
            })
            .await
            .unwrap();
        assert!(fx.service.list_tags().await.unwrap().iter().any(|t| t.key == tag.key));

        fx.service.delete_tag(&tag.key).await.unwrap();
        // tag gone from vocabulary
        assert!(!fx.service.list_tags().await.unwrap().iter().any(|t| t.key == tag.key));
        // and stripped from the assistant
        let a = fx.service.get("u1").await.unwrap();
        assert!(a.audience_tags.is_empty());
    }

    #[tokio::test]
    async fn delete_tag_missing_returns_not_found() {
        let fx = fixture().await;
        let err = fx.service.delete_tag("utag-nope").await.unwrap_err();
        assert!(matches!(err, AppError::NotFound(_)));
    }

    fn req_default() -> CreateAssistantRequest {
        CreateAssistantRequest {
            id: None,
            name: String::new(),
            description: None,
            avatar: None,
            preset_agent_type: None,
            enabled_skills: None,
            custom_skill_names: None,
            disabled_builtin_skills: None,
            prompts: None,
            models: None,
            audience_tags: None,
            scenario_tags: None,
            name_i18n: None,
            description_i18n: None,
            prompts_i18n: None,
        }
    }
}

//! Preset catalog CRUD and the single execution-time resolver.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::Mutex;

use nomifun_api_types::*;
use nomifun_common::{AgentId, AppError, KnowledgeBaseId, PresetId, PresetTagId, ProviderId};
use nomifun_db::{
    CreatePresetTagParams, IAgentMetadataRepository, IPresetRepository, IPresetStateRepository,
    IPresetTagRepository, IProviderRepository, PresetRecord, PresetWriteParams,
    UpdatePresetTagParams, UpsertPresetStateParams,
};
use nomifun_extension::{ExtensionRegistry, ResolvedPreset};

use crate::builtin::{AvatarAsset, BuiltinPreset, BuiltinPresetRegistry};
use nomifun_extension::{PresetClassifier, PresetRuleDispatcher};

pub struct PresetService {
    repo: Arc<dyn IPresetRepository>,
    state_repo: Arc<dyn IPresetStateRepository>,
    tag_repo: Arc<dyn IPresetTagRepository>,
    agent_repo: Arc<dyn IAgentMetadataRepository>,
    provider_repo: Arc<dyn IProviderRepository>,
    builtin: Arc<BuiltinPresetRegistry>,
    extension_registry: ExtensionRegistry,
    user_data_dir: PathBuf,
    catalog_sync: Arc<Mutex<()>>,
}

#[async_trait::async_trait]
impl PresetClassifier for PresetService {
    async fn classify(&self, id: &str) -> PresetSource { self.classify_source(id).await }
}

#[async_trait::async_trait]
impl PresetRuleDispatcher for PresetService {
    async fn read_rule(&self, id: &str, locale: Option<&str>) -> Result<String, AppError> {
        let preset = self.get(id).await?;
        Ok(locale.and_then(|l| localized_value(&preset.instructions_i18n, l)).unwrap_or(preset.instructions))
    }
    async fn write_rule(&self, id: &str, _locale: Option<&str>, content: &str) -> Result<(), AppError> {
        self.update(id, UpdatePresetRequest { instructions: Some(content.to_string()), ..Default::default() }).await?;
        Ok(())
    }
    async fn delete_rule(&self, id: &str) -> Result<bool, AppError> {
        self.update(id, UpdatePresetRequest { instructions: Some(String::new()), ..Default::default() }).await?;
        Ok(true)
    }
    async fn read_skill(&self, _id: &str, _locale: Option<&str>) -> Result<String, AppError> { Ok(String::new()) }
    async fn write_skill(&self, _id: &str, _locale: Option<&str>, _content: &str) -> Result<(), AppError> {
        Err(AppError::BadRequest("Preset skills are managed through included_skills".into()))
    }
    async fn delete_skill(&self, _id: &str) -> Result<bool, AppError> { Ok(false) }
}

impl PresetService {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        repo: Arc<dyn IPresetRepository>,
        state_repo: Arc<dyn IPresetStateRepository>,
        tag_repo: Arc<dyn IPresetTagRepository>,
        agent_repo: Arc<dyn IAgentMetadataRepository>,
        provider_repo: Arc<dyn IProviderRepository>,
        builtin: Arc<BuiltinPresetRegistry>,
        extension_registry: ExtensionRegistry,
        user_data_dir: PathBuf,
    ) -> Self {
        Self {
            repo,
            state_repo,
            tag_repo,
            agent_repo,
            provider_repo,
            builtin,
            extension_registry,
            user_data_dir,
            catalog_sync: Arc::new(Mutex::new(())),
        }
    }

    pub async fn classify_source(&self, id: &str) -> PresetSource {
        if PresetId::parse(id).is_err() {
            return PresetSource::User;
        }
        match self.repo.get(id).await {
            Ok(Some(record)) => record
                .preset
                .as_ref()
                .map(|preset| match preset.source_kind.as_str() {
                    "builtin" => PresetSource::Builtin,
                    "extension" => PresetSource::Extension,
                    _ => PresetSource::User,
                })
                .unwrap_or(PresetSource::User),
            _ => PresetSource::User,
        }
    }

    async fn sync_catalog(&self) -> Result<(), AppError> {
        let _guard = self.catalog_sync.lock().await;

        self.ensure_builtin_tags().await?;
        let tag_ids_by_key = self
            .tag_repo
            .list()
            .await?
            .into_iter()
            .map(|tag| (tag.key, tag.preset_tag_id))
            .collect::<HashMap<_, _>>();
        for item in self.builtin.all() {
            let params = builtin_write_params(&self.builtin, item, &tag_ids_by_key)?;
            self.repo.upsert_catalog(&params).await?;
        }
        for item in self.extension_registry.get_presets().await {
            let params = extension_write_params(&item)?;
            self.repo.upsert_catalog(&params).await?;
        }
        Ok(())
    }

    async fn ensure_builtin_tags(&self) -> Result<(), AppError> {
        for tag in self.builtin.tags() {
            validate_preset_tag_key(&tag.key)?;
            if self.tag_repo.get_by_key(&tag.key).await?.is_none() {
                let preset_tag_id = PresetTagId::new().into_string();
                self.tag_repo
                    .create(&CreatePresetTagParams {
                        preset_tag_id: &preset_tag_id,
                        key: &tag.key,
                        dimension: &tag.dimension,
                        label: &tag.label,
                        sort_order: tag.sort_order,
                    })
                    .await?;
            }
        }
        Ok(())
    }

    pub async fn list(&self) -> Result<Vec<PresetResponse>, AppError> {
        self.sync_catalog().await?;
        let states = self.state_repo.get_all().await?;
        let state_map: HashMap<_, _> = states.into_iter().map(|s| (s.preset_id.clone(), s)).collect();
        let mut output = Vec::new();
        for record in self.repo.list().await? {
            let Some(preset) = record.preset.as_ref() else { continue };
            let mut response = record_to_response(&record)?;
            apply_state(&mut response, state_map.get(&preset.preset_id));
            output.push(response);
        }
        output.sort_by(|a, b| a.sort_order.cmp(&b.sort_order).then_with(|| b.last_used_at.cmp(&a.last_used_at)));
        let ids: Vec<_> = output.iter().map(|p| p.preset_id.as_str()).collect();
        if let Err(error) = self.state_repo.delete_orphans(&ids).await {
            tracing::warn!(%error, "failed to remove state for presets no longer in the catalog");
        }
        Ok(output)
    }

    pub async fn get(&self, id: &str) -> Result<PresetResponse, AppError> {
        PresetId::parse(id)
            .map_err(|error| AppError::BadRequest(format!("invalid preset_id: {error}")))?;
        self.sync_catalog().await?;
        let record = self.repo.get(id).await?
            .ok_or_else(|| AppError::NotFound(format!("preset '{id}' not found")))?;
        let mut response = record_to_response(&record)?;
        let state = self.state_repo.get(id).await?;
        apply_state(&mut response, state.as_ref());
        Ok(response)
    }

    pub async fn create(&self, request: CreatePresetRequest) -> Result<PresetResponse, AppError> {
        validate_request(
            &request.name,
            &request.agent_preferences,
            &request.model_preferences,
            &request.knowledge_policy,
            &request.knowledge_bases,
            &request.audience_tag_ids,
            &request.scenario_tag_ids,
        )?;
        let preset_id = match request.preset_id.clone() {
            Some(value) => PresetId::parse(value)
                .map_err(|error| AppError::BadRequest(format!("invalid user preset id: {error}")))?
                .into_string(),
            None => PresetId::new().into_string(),
        };
        let params = write_from_create(preset_id.clone(), request);
        let record = self.repo.create(&params).await?;
        let state = self.state_repo.upsert(&UpsertPresetStateParams {
            preset_id, enabled: true, auto_selectable: false,
            preferred_agent_id: None, sort_order: 0, last_used_at: None,
        }).await?;
        let mut response = record_to_response(&record)?;
        apply_state(&mut response, Some(&state));
        Ok(response)
    }

    pub async fn update(&self, id: &str, request: UpdatePresetRequest) -> Result<PresetResponse, AppError> {
        let existing = self.get(id).await?;
        if existing.source != PresetSource::User {
            return Err(AppError::Forbidden("Copy bundled presets before editing them".into()));
        }
        PresetId::parse(id)
            .map_err(|error| AppError::BadRequest(format!("invalid user preset id: {error}")))?;
        let merged = merge_update(existing, request);
        validate_request(
            &merged.name,
            &merged.agent_preferences,
            &merged.model_preferences,
            &merged.knowledge_policy,
            &merged.knowledge_bases,
            &merged.audience_tag_ids,
            &merged.scenario_tag_ids,
        )?;
        let params = write_from_response(merged);
        let record = self.repo.update(id, &params).await?
            .ok_or_else(|| AppError::NotFound(format!("preset '{id}' not found")))?;
        record_to_response(&record)
    }

    pub async fn delete(&self, id: &str) -> Result<(), AppError> {
        let existing = self.get(id).await?;
        if existing.source != PresetSource::User {
            return Err(AppError::Forbidden("Bundled presets cannot be deleted".into()));
        }
        PresetId::parse(id)
            .map_err(|error| AppError::BadRequest(format!("invalid user preset id: {error}")))?;
        if !self.repo.delete(id).await? { return Err(AppError::NotFound(format!("preset '{id}' not found"))); }
        let _ = self.state_repo.delete(id).await;
        for dir in ["preset-instructions", "preset-avatars"] {
            remove_files_with_stem(&self.user_data_dir.join(dir), id);
        }
        Ok(())
    }

    pub async fn set_state(&self, id: &str, request: SetPresetStateRequest) -> Result<PresetResponse, AppError> {
        let current = self.get(id).await?;
        if current.source == PresetSource::Extension {
            return Err(AppError::Forbidden("Extension presets are managed by their extension".into()));
        }
        let existing = self.state_repo.get(id).await?;
        let preferred_agent_id = match request.preferred_agent_id {
            Some(value) => {
                validate_agent_reference(&value)?;
                Some(value)
            }
            None => existing.as_ref().and_then(|state| state.preferred_agent_id.clone()),
        };
        self.state_repo.upsert(&UpsertPresetStateParams {
            preset_id: id.to_string(),
            enabled: request.enabled.or_else(|| existing.as_ref().map(|s| s.enabled)).unwrap_or(true),
            auto_selectable: request.auto_selectable.or_else(|| existing.as_ref().map(|s| s.auto_selectable)).unwrap_or(false),
            preferred_agent_id,
            sort_order: request.sort_order.or_else(|| existing.as_ref().map(|s| s.sort_order)).unwrap_or(0),
            last_used_at: request.last_used_at.or_else(|| existing.and_then(|s| s.last_used_at)),
        }).await?;
        self.get(id).await
    }

    /// Resolve one preset into an immutable snapshot. Explicit overrides win,
    /// then ordered preset preferences, then an enabled catalog fallback when
    /// the preset permits fallback.
    pub async fn resolve(
        &self,
        id: &str,
        target: PresetTarget,
        locale: Option<&str>,
        overrides: PresetOverrides,
    ) -> Result<ResolvedPresetSnapshot, AppError> {
        if let Some(provider_id) = overrides.provider_id.as_deref() {
            ProviderId::parse(provider_id).map_err(|error| {
                AppError::BadRequest(format!("invalid provider_id override: {error}"))
            })?;
        }
        if let Some(agent_id) = overrides.agent_id.as_deref() {
            validate_agent_reference(agent_id)?;
        }
        let preset = self.get(id).await?;
        if !preset.enabled { return Err(AppError::BadRequest(format!("preset '{id}' is disabled"))); }
        if !preset.targets.is_empty() && !preset.targets.contains(&target) {
            return Err(AppError::BadRequest(format!("preset '{id}' cannot target {target:?}")));
        }
        let mut warnings = Vec::new();
        let instructions = overrides.instructions.clone().unwrap_or_else(|| {
            locale.and_then(|l| localized_value(&preset.instructions_i18n, l)).unwrap_or_else(|| preset.instructions.clone())
        });

        let resolved_agent_id = if let Some(agent_id) = overrides.agent_id {
            Some(self.resolve_agent(&agent_id, true, &mut warnings).await?)
        } else {
            let mut selected = None;
            if let Some(agent_id) = preset.preferred_agent_id.as_deref() {
                match self.resolve_agent(agent_id, false, &mut warnings).await {
                    Ok(id) => selected = Some(id),
                    Err(error) if preset.fallback_allowed => warnings.push(error.to_string()),
                    Err(error) => return Err(error),
                }
            }
            for pref in &preset.agent_preferences {
                if selected.is_some() { break; }
                match self.resolve_agent(&pref.agent_id, pref.required, &mut warnings).await {
                    Ok(id) => { selected = Some(id); break; }
                    Err(error) if !pref.required && preset.fallback_allowed => warnings.push(error.to_string()),
                    Err(error) => return Err(error),
                }
            }
            if selected.is_none() && preset.fallback_allowed {
                selected = self
                    .agent_repo
                    .list_all()
                    .await?
                    .into_iter()
                    .find(|a| a.enabled)
                    .map(|a| a.agent_id);
                if selected.is_some() { warnings.push("Agent preference unavailable; used the first enabled agent".into()); }
            }
            selected
        };

        let resolved_agent = if let Some(agent_id) = resolved_agent_id.as_ref() {
            self.agent_repo.get(agent_id).await?
        } else {
            None
        };
        let resolved_agent_type = resolved_agent.as_ref().map(|agent| agent.agent_type.clone());
        let resolved_agent_backend = resolved_agent.as_ref().and_then(|agent| agent.backend.clone());

        let resolved_model = if let Some(model) = overrides.model {
            let preference = ModelPreference { provider_id: overrides.provider_id, model, required: true };
            Some(self.resolve_model_preference(&preference, &mut warnings).await?)
        } else {
            let mut selected = None;
            for preference in &preset.model_preferences {
                match self.resolve_model_preference(preference, &mut warnings).await {
                    Ok(resolved) => { selected = Some(resolved); break; }
                    Err(error) if !preference.required && preset.fallback_allowed => {
                        warnings.push(error.to_string());
                    }
                    Err(error) => return Err(error),
                }
            }
            selected
        };

        let mut skills: Vec<String> = preset.included_skills.iter().map(|s| s.skill_name.clone()).collect();
        skills.extend(overrides.include_skills);
        let excluded: HashSet<_> = overrides.exclude_skills.into_iter().collect();
        skills.retain(|s| !excluded.contains(s));
        dedupe(&mut skills);

        let knowledge_policy = overrides.knowledge_policy.unwrap_or(preset.knowledge_policy);
        let knowledge_base_ids = match overrides.knowledge_base_ids {
            Some(ids) => ids,
            None => preset
                .knowledge_bases
                .into_iter()
                .map(|binding| binding.knowledge_base_id)
                .collect(),
        };
        Ok(ResolvedPresetSnapshot {
            preset_id: preset.preset_id, preset_revision: preset.revision, preset_name: preset.name,
            target, routing_description: preset.routing_description, instructions,
            resolved_agent_id, resolved_agent_type, resolved_agent_backend,
            resolved_model, included_skills: skills,
            excluded_auto_skills: preset.excluded_auto_skills, knowledge_policy,
            knowledge_base_ids, warnings,
        })
    }

    async fn resolve_agent(&self, value: &str, required: bool, warnings: &mut Vec<String>) -> Result<String, AppError> {
        validate_agent_reference(value)?;
        if let Some(row) = self.agent_repo.get(value).await? {
            if row.enabled {
                validate_agent_reference(&row.agent_id).map_err(|error| {
                    AppError::Internal(format!("stored agent identity is invalid: {error}"))
                })?;
                return Ok(row.agent_id);
            }
        }
        if !required { warnings.push(format!("Agent preference '{value}' is unavailable")); }
        Err(AppError::BadRequest(format!("agent preference '{value}' is unavailable")))
    }

    async fn resolve_model_preference(
        &self,
        preference: &ModelPreference,
        warnings: &mut Vec<String>,
    ) -> Result<ModelPreference, AppError> {
        if let Some(provider_id) = preference.provider_id.as_deref() {
            ProviderId::parse(provider_id).map_err(|error| {
                AppError::BadRequest(format!("invalid model preference provider_id: {error}"))
            })?;
        }
        let providers = self.provider_repo.list().await?;
        let candidates: Vec<_> = providers
            .into_iter()
            .filter(|provider| {
                provider.enabled
                    && preference
                        .provider_id
                        .as_ref()
                        .is_none_or(|provider_id| provider_id == &provider.provider_id)
            })
            .collect();
        for provider in candidates {
            let models: Vec<String> = serde_json::from_str(&provider.models).unwrap_or_default();
            if models.iter().any(|m| m == &preference.model) {
                if preference.provider_id.is_none() {
                    warnings.push(format!(
                        "Unqualified model '{}' resolved to provider '{}'",
                        preference.model, provider.provider_id
                    ));
                }
                return Ok(ModelPreference {
                    provider_id: Some(provider.provider_id),
                    model: preference.model.clone(),
                    required: preference.required,
                });
            }
        }
        Err(AppError::BadRequest(format!("model '{}' is unavailable", preference.model)))
    }

    pub async fn import(&self, request: ImportPresetsRequest) -> Result<ImportPresetsResult, AppError> {
        let mut result = ImportPresetsResult::default();
        for item in request.presets {
            let preset_id = item.preset_id.clone().unwrap_or_default();
            match self.create(item).await {
                Ok(_) => result.imported += 1,
                Err(AppError::Conflict(_)) => result.skipped += 1,
                Err(error) => {
                    result.failed += 1;
                    result.errors.push(PresetImportError {
                        preset_id,
                        error: error.to_string(),
                    });
                }
            }
        }
        Ok(result)
    }

    pub async fn list_tags(&self) -> Result<Vec<PresetTagResponse>, AppError> {
        self.ensure_builtin_tags().await?;
        Ok(merge_preset_tags(
            self.builtin.tags(),
            self.tag_repo.list().await?,
        ))
    }

    pub async fn create_tag(&self, request: CreatePresetTagRequest) -> Result<PresetTagResponse, AppError> {
        let label = request.label.trim();
        if label.is_empty() { return Err(AppError::BadRequest("tag label is required".into())); }
        let base_key = slugify_tag_label(label);
        let existing_keys = self.builtin.tags().iter().map(|tag| tag.key.clone())
            .chain(self.tag_repo.list().await?.into_iter().map(|tag| tag.key))
            .collect::<HashSet<_>>();
        let key = deduplicate_tag_key(&base_key, &existing_keys);
        let dimension = dimension_str(request.dimension);
        let preset_tag_id = PresetTagId::new().into_string();
        let row = self.tag_repo.create(&CreatePresetTagParams {
            preset_tag_id: &preset_tag_id,
            key: &key,
            dimension,
            label,
            sort_order: 0,
        }).await?;
        Ok(PresetTagResponse {
            preset_tag_id: row.preset_tag_id,
            key: row.key,
            dimension: request.dimension,
            label: row.label,
            label_i18n: HashMap::new(),
            sort_order: row.sort_order,
            builtin: false,
        })
    }

    pub async fn update_tag(&self, preset_tag_id: &str, request: UpdatePresetTagRequest) -> Result<PresetTagResponse, AppError> {
        validate_preset_tag_id(preset_tag_id)?;
        let existing = self.tag_repo.get(preset_tag_id).await?
            .ok_or_else(|| AppError::NotFound(format!("preset tag '{preset_tag_id}' not found")))?;
        if self.builtin.tags().iter().any(|tag| tag.key == existing.key) {
            return Err(AppError::Forbidden("Built-in tags cannot be edited".into()));
        }
        let label = request.label.as_deref().map(str::trim);
        if label.is_some_and(str::is_empty) { return Err(AppError::BadRequest("tag label is required".into())); }
        let row = self.tag_repo.update(preset_tag_id, &UpdatePresetTagParams { label: request.label.as_deref(), sort_order: request.sort_order }).await?
            .ok_or_else(|| AppError::NotFound(format!("preset tag '{preset_tag_id}' not found")))?;
        Ok(PresetTagResponse {
            preset_tag_id: row.preset_tag_id,
            key: row.key,
            dimension: parse_dimension(&row.dimension),
            label: row.label,
            label_i18n: HashMap::new(),
            sort_order: row.sort_order,
            builtin: false,
        })
    }

    pub async fn delete_tag(&self, preset_tag_id: &str) -> Result<(), AppError> {
        validate_preset_tag_id(preset_tag_id)?;
        let existing = self.tag_repo.get(preset_tag_id).await?
            .ok_or_else(|| AppError::NotFound(format!("preset tag '{preset_tag_id}' not found")))?;
        if self.builtin.tags().iter().any(|tag| tag.key == existing.key) {
            return Err(AppError::Forbidden("Built-in tags cannot be deleted".into()));
        }
        if !self.tag_repo.delete(preset_tag_id).await? {
            return Err(AppError::NotFound(format!("preset tag '{preset_tag_id}' not found")));
        }
        Ok(())
    }

    pub async fn avatar_asset(&self, id: &str) -> Option<AvatarAsset> {
        let preset_id = PresetId::parse(id).ok()?;
        self.sync_catalog().await.ok()?;
        let record = self.repo.get(preset_id.as_str()).await.ok()??;
        let root = record.preset.as_ref()?;
        match root.source_kind.as_str() {
            "builtin" => self.builtin.avatar_asset(root.source_key.as_deref()?).or_else(|| {
                find_asset(&self.user_data_dir.join("preset-avatars"), id)
            }),
            "user" => find_asset(&self.user_data_dir.join("preset-avatars"), id),
            _ => None,
        }
    }
}

fn validate_request(
    name: &str,
    agents: &[AgentPreference],
    models: &[ModelPreference],
    policy: &PresetKnowledgePolicy,
    _knowledge_bases: &[KnowledgeBaseBinding],
    audience_tags: &[String],
    scenario_tags: &[String],
) -> Result<(), AppError> {
    if name.trim().is_empty() { return Err(AppError::BadRequest("name is required".into())); }
    for agent in agents {
        validate_agent_reference(&agent.agent_id)?;
    }
    if models.iter().any(|m| m.model.trim().is_empty()) { return Err(AppError::BadRequest("model preference requires model".into())); }
    for provider_id in models.iter().filter_map(|model| model.provider_id.as_deref()) {
        ProviderId::parse(provider_id).map_err(|error| {
            AppError::BadRequest(format!("invalid model preference provider_id: {error}"))
        })?;
    }
    if !matches!(policy.mode.as_str(), "inherit" | "staged" | "direct") {
        return Err(AppError::BadRequest("knowledge policy mode must be inherit, staged, or direct".into()));
    }
    if policy.eagerness.as_deref().is_some_and(|value| !matches!(value, "conservative" | "aggressive")) {
        return Err(AppError::BadRequest("knowledge eagerness must be conservative or aggressive".into()));
    }
    for preset_tag_id in audience_tags.iter().chain(scenario_tags) {
        validate_preset_tag_id(preset_tag_id)?;
    }
    Ok(())
}

fn validate_agent_reference(value: &str) -> Result<(), AppError> {
    AgentId::parse(value)
        .map(|_| ())
        .map_err(|error| AppError::BadRequest(format!("invalid agent_id: {error}")))
}

fn record_to_response(record: &PresetRecord) -> Result<PresetResponse, AppError> {
    let p = record.preset.as_ref().ok_or_else(|| AppError::Internal("preset aggregate missing root".into()))?;
    PresetId::parse(&p.preset_id).map_err(|error| {
        AppError::Internal(format!(
            "stored preset_id '{}' is not canonical: {error}",
            p.preset_id
        ))
    })?;
    for preference in &record.model_preferences {
        if let Some(provider_id) = preference.provider_id.as_deref() {
            ProviderId::parse(provider_id).map_err(|error| {
                AppError::Internal(format!(
                    "stored preset model provider_id '{provider_id}' is not canonical: {error}"
                ))
            })?;
        }
    }
    let knowledge_bases = record
        .knowledge_bases
        .iter()
        .map(|binding| {
            KnowledgeBaseId::parse(&binding.knowledge_base_id)
                .map(|knowledge_base_id| KnowledgeBaseBinding {
                    knowledge_base_id,
                    required: binding.required,
                })
                .map_err(|error| {
                    AppError::Internal(format!(
                        "stored preset knowledge_base_id '{}' is not canonical: {error}",
                        binding.knowledge_base_id
                    ))
                })
        })
        .collect::<Result<Vec<_>, _>>()?;
    for preference in &record.agent_preferences {
        validate_agent_reference(&preference.agent_id).map_err(|error| {
            AppError::Internal(format!("stored preset agent identity is invalid: {error}"))
        })?;
    }
    if let Some(state) = record.user_state.as_ref()
        && let Some(agent_id) = state.preferred_agent_id.as_deref()
    {
        validate_agent_reference(agent_id).map_err(|error| {
            AppError::Internal(format!(
                "stored preset preferred_agent_id is invalid: {error}"
            ))
        })?;
    }
    let mut name_i18n = HashMap::new(); let mut description_i18n = HashMap::new(); let mut instructions_i18n = HashMap::new();
    for l in &record.localizations {
        if let Some(v) = &l.name { name_i18n.insert(l.locale.clone(), v.clone()); }
        if let Some(v) = &l.description { description_i18n.insert(l.locale.clone(), v.clone()); }
        if let Some(v) = &l.instructions { instructions_i18n.insert(l.locale.clone(), v.clone()); }
    }
    let policy = record.knowledge_policy.as_ref().map(|k| PresetKnowledgePolicy { enabled: k.enabled, mode: k.mode.clone(), writeback: k.writeback, eagerness: k.eagerness.clone(), grounded: k.grounded }).unwrap_or_default();
    let mut response = PresetResponse {
        preset_id: p.preset_id.clone(), revision: p.revision, source: match p.source_kind.as_str() { "builtin" => PresetSource::Builtin, "extension" => PresetSource::Extension, _ => PresetSource::User },
        source_key: p.source_key.clone(), name: p.name.clone(), name_i18n, description: p.description.clone(), description_i18n,
        routing_description: p.routing_description.clone(), instructions: p.instructions.clone(), instructions_i18n, avatar: p.avatar.clone(), fallback_allowed: p.fallback_allowed,
        targets: record.targets.iter().filter_map(|v| parse_target(v)).collect(),
        agent_preferences: record.agent_preferences.iter().map(|v| AgentPreference { agent_id: v.agent_id.clone(), required: v.required }).collect(),
        model_preferences: record.model_preferences.iter().map(|v| ModelPreference { provider_id: v.provider_id.clone(), model: v.model.clone(), required: v.required }).collect(),
        included_skills: record.skill_bindings.iter().filter(|v| v.binding == "include").map(|v| SkillBinding { skill_name: v.skill_name.clone(), required: v.required }).collect(),
        excluded_auto_skills: record.skill_bindings.iter().filter(|v| v.binding == "exclude_auto").map(|v| v.skill_name.clone()).collect(),
        knowledge_policy: policy, knowledge_bases,
        examples: record.examples.iter().filter(|v| v.locale.is_empty()).map(|v| v.prompt.clone()).collect(),
        examples_i18n: collect_examples_i18n(&record.examples),
        audience_tag_ids: record.tag_bindings.iter().filter(|v| v.dimension == "audience").map(|v| v.preset_tag_id.clone()).collect(),
        scenario_tag_ids: record.tag_bindings.iter().filter(|v| v.dimension == "scenario").map(|v| v.preset_tag_id.clone()).collect(),
        audience_tags: record.tag_bindings.iter().filter(|v| v.dimension == "audience").map(|v| v.key.clone()).collect(),
        scenario_tags: record.tag_bindings.iter().filter(|v| v.dimension == "scenario").map(|v| v.key.clone()).collect(),
        enabled: true, auto_selectable: false, preferred_agent_id: None,
        sort_order: 0, last_used_at: None,
    };
    apply_state(&mut response, record.user_state.as_ref()); Ok(response)
}

fn builtin_write_params(
    registry: &BuiltinPresetRegistry,
    item: &BuiltinPreset,
    tag_ids_by_key: &HashMap<String, String>,
) -> Result<PresetWriteParams, AppError> {
    validate_agent_reference(&item.preferred_agent_id)?;
    for key in item.audience_tags.iter().chain(&item.scenario_tags) {
        validate_preset_tag_key(key)?;
    }
    let instructions = registry
        .rule_bytes(&item.source_key, "en-US")
        .and_then(|value| String::from_utf8(value).ok())
        .unwrap_or_default();
    let instructions_i18n = ["zh-CN", "en-US"]
        .into_iter()
        .filter_map(|locale| {
            registry
                .rule_bytes(&item.source_key, locale)
                .and_then(|value| String::from_utf8(value).ok())
                .map(|value| (locale.to_owned(), value))
        })
        .collect::<HashMap<_, _>>();
    Ok(PresetWriteParams {
        preset_id: PresetId::new().into_string(),
        source_kind: "builtin".into(),
        source_key: Some(item.source_key.clone()),
        name: item.name.clone(),
        description: item.description.clone(),
        routing_description: item.description.clone(),
        instructions,
        avatar: item.avatar.clone(),
        fallback_allowed: true,
        localizations: collect_localizations(
            &item.name_i18n,
            &item.description_i18n,
            &instructions_i18n,
        ),
        targets: target_strings(&default_targets()),
        agent_preferences: vec![(item.preferred_agent_id.clone(), false)],
        model_preferences: item
            .models
            .iter()
            .cloned()
            .map(|model| (None, model, false))
            .collect(),
        skill_bindings: item
            .enabled_skills
            .iter()
            .chain(&item.custom_skill_names)
            .cloned()
            .map(|skill| (skill, "include".into(), false))
            .chain(
                item.disabled_builtin_skills
                    .iter()
                    .cloned()
                    .map(|skill| (skill, "exclude_auto".into(), false)),
            )
            .collect(),
        knowledge_policy: (false, "inherit".into(), false, None, false),
        knowledge_bases: vec![],
        examples: flatten_examples(item.prompts.clone(), item.prompts_i18n.clone()),
        tag_bindings: item
            .audience_tags
            .iter()
            .map(|key| {
                tag_ids_by_key
                    .get(key)
                    .cloned()
                    .map(|preset_tag_id| (preset_tag_id, "audience".into()))
                    .ok_or_else(|| {
                        AppError::Internal(format!(
                            "builtin preset tag key '{key}' was not materialized"
                        ))
                    })
            })
            .chain(
                item.scenario_tags
                    .iter()
                    .map(|key| {
                        tag_ids_by_key
                            .get(key)
                            .cloned()
                            .map(|preset_tag_id| (preset_tag_id, "scenario".into()))
                            .ok_or_else(|| {
                                AppError::Internal(format!(
                                    "builtin preset tag key '{key}' was not materialized"
                                ))
                            })
                    }),
            )
            .collect::<Result<Vec<_>, _>>()?,
    })
}

fn extension_write_params(item: &ResolvedPreset) -> Result<PresetWriteParams, AppError> {
    if let Some(agent_id) = item.preferred_agent_id.as_deref() {
        validate_agent_reference(agent_id)?;
    }
    Ok(PresetWriteParams {
        preset_id: PresetId::new().into_string(),
        source_kind: "extension".into(),
        source_key: Some(item.source_key.clone()),
        name: item.name.clone(),
        description: item.description.clone(),
        routing_description: item.description.clone(),
        instructions: item
            .system_prompt
            .clone()
            .or_else(|| item.context.clone())
            .unwrap_or_default(),
        avatar: item.icon.clone(),
        fallback_allowed: true,
        localizations: vec![],
        targets: target_strings(&default_targets()),
        agent_preferences: item
            .preferred_agent_id
            .iter()
            .cloned()
            .map(|agent_id| (agent_id, false))
            .collect(),
        model_preferences: item
            .models
            .iter()
            .cloned()
            .map(|model| (None, model, false))
            .collect(),
        skill_bindings: item
            .enabled_skills
            .iter()
            .cloned()
            .map(|skill| (skill, "include".into(), false))
            .collect(),
        knowledge_policy: (false, "inherit".into(), false, None, false),
        knowledge_bases: vec![],
        examples: item
            .prompts
            .iter()
            .cloned()
            .map(|prompt| (String::new(), prompt))
            .collect(),
        tag_bindings: vec![],
    })
}

fn write_from_create(preset_id: String, r: CreatePresetRequest) -> PresetWriteParams {
    let localizations = collect_localizations(&r.name_i18n, &r.description_i18n, &r.instructions_i18n);
    let examples = flatten_examples(r.examples, r.examples_i18n);
    PresetWriteParams { preset_id: preset_id.clone(), source_kind: "user".into(), source_key: None, name: r.name.trim().into(), description: r.description, routing_description: r.routing_description, instructions: r.instructions, avatar: r.avatar, fallback_allowed: r.fallback_allowed,
        localizations, targets: target_strings(&r.targets), agent_preferences: r.agent_preferences.into_iter().map(|v| (v.agent_id, v.required)).collect(), model_preferences: r.model_preferences.into_iter().map(|v| (v.provider_id, v.model, v.required)).collect(),
        skill_bindings: r.included_skills.into_iter().map(|v| (v.skill_name,"include".into(),v.required)).chain(r.excluded_auto_skills.into_iter().map(|v| (v,"exclude_auto".into(),false))).collect(),
        knowledge_policy: (r.knowledge_policy.enabled,r.knowledge_policy.mode,r.knowledge_policy.writeback,r.knowledge_policy.eagerness,r.knowledge_policy.grounded),
        knowledge_bases: r.knowledge_bases.into_iter().map(|v| (v.knowledge_base_id.to_string(),v.required)).collect(), examples,
        tag_bindings: r.audience_tag_ids.into_iter().map(|v| (v,"audience".into())).chain(r.scenario_tag_ids.into_iter().map(|v| (v,"scenario".into()))).collect() }
}

fn write_from_response(r: PresetResponse) -> PresetWriteParams {
    let localizations = collect_localizations(&r.name_i18n, &r.description_i18n, &r.instructions_i18n);
    let examples = flatten_examples(r.examples, r.examples_i18n);
    PresetWriteParams { preset_id:r.preset_id.clone(),source_kind:"user".into(),source_key:None,name:r.name,description:r.description,routing_description:r.routing_description,instructions:r.instructions,avatar:r.avatar,fallback_allowed:r.fallback_allowed,
        localizations,targets:target_strings(&r.targets),agent_preferences:r.agent_preferences.into_iter().map(|v|(v.agent_id,v.required)).collect(),model_preferences:r.model_preferences.into_iter().map(|v|(v.provider_id,v.model,v.required)).collect(),
        skill_bindings:r.included_skills.into_iter().map(|v|(v.skill_name,"include".into(),v.required)).chain(r.excluded_auto_skills.into_iter().map(|v|(v,"exclude_auto".into(),false))).collect(),
        knowledge_policy:(r.knowledge_policy.enabled,r.knowledge_policy.mode,r.knowledge_policy.writeback,r.knowledge_policy.eagerness,r.knowledge_policy.grounded),knowledge_bases:r.knowledge_bases.into_iter().map(|v|(v.knowledge_base_id.to_string(),v.required)).collect(),examples,
        tag_bindings:r.audience_tag_ids.into_iter().map(|v|(v,"audience".into())).chain(r.scenario_tag_ids.into_iter().map(|v|(v,"scenario".into()))).collect() }
}

fn merge_update(mut p: PresetResponse, r: UpdatePresetRequest) -> PresetResponse {
    if let Some(v)=r.name {p.name=v} if r.description.is_some(){p.description=r.description} if r.routing_description.is_some(){p.routing_description=r.routing_description}
    if let Some(v)=r.instructions{p.instructions=v} if r.avatar.is_some(){p.avatar=r.avatar} if let Some(v)=r.fallback_allowed{p.fallback_allowed=v}
    if let Some(v)=r.targets{p.targets=v} if let Some(v)=r.agent_preferences{p.agent_preferences=v} if let Some(v)=r.model_preferences{p.model_preferences=v}
    if let Some(v)=r.included_skills{p.included_skills=v} if let Some(v)=r.excluded_auto_skills{p.excluded_auto_skills=v} if let Some(v)=r.knowledge_policy{p.knowledge_policy=v}
    if let Some(v)=r.knowledge_bases{p.knowledge_bases=v} if let Some(v)=r.examples{p.examples=v} if let Some(v)=r.examples_i18n{p.examples_i18n=v} if let Some(v)=r.audience_tag_ids{p.audience_tag_ids=v} if let Some(v)=r.scenario_tag_ids{p.scenario_tag_ids=v}
    if let Some(v)=r.name_i18n{p.name_i18n=v} if let Some(v)=r.description_i18n{p.description_i18n=v} if let Some(v)=r.instructions_i18n{p.instructions_i18n=v} p
}

fn apply_state(response:&mut PresetResponse,state:Option<&nomifun_db::PresetUserStateRow>){if let Some(s)=state{response.enabled=s.enabled;response.auto_selectable=s.auto_selectable;response.preferred_agent_id=s.preferred_agent_id.clone();response.sort_order=s.sort_order;response.last_used_at=s.last_used_at}}
fn default_targets()->Vec<PresetTarget>{vec![PresetTarget::Conversation,PresetTarget::ExecutionStep,PresetTarget::Companion,PresetTarget::Cron]}
fn target_strings(v:&[PresetTarget])->Vec<String>{v.iter().map(|v|match v{PresetTarget::Conversation=>"conversation",PresetTarget::ExecutionStep=>"execution_step",PresetTarget::Companion=>"companion",PresetTarget::PublicCompanion=>"public_companion",PresetTarget::Cron=>"cron"}.into()).collect()}
fn parse_target(v:&str)->Option<PresetTarget>{match v{"conversation"=>Some(PresetTarget::Conversation),"execution_step"=>Some(PresetTarget::ExecutionStep),"companion"=>Some(PresetTarget::Companion),"public_companion"=>Some(PresetTarget::PublicCompanion),"cron"=>Some(PresetTarget::Cron),_=>None}}
fn dimension_str(v:PresetTagDimension)->&'static str{match v{PresetTagDimension::Audience=>"audience",PresetTagDimension::Scenario=>"scenario"}}
fn parse_dimension(v:&str)->PresetTagDimension{if v=="scenario"{PresetTagDimension::Scenario}else{PresetTagDimension::Audience}}
fn merge_preset_tags(builtin:&[crate::builtin::BuiltinTag],stored:Vec<nomifun_db::PresetTagRow>)->Vec<PresetTagResponse>{let builtin_by_key=builtin.iter().map(|tag|(tag.key.as_str(),tag)).collect::<HashMap<_,_>>();stored.into_iter().map(|tag|{let builtin=builtin_by_key.get(tag.key.as_str()).copied();PresetTagResponse{preset_tag_id:tag.preset_tag_id,key:tag.key,dimension:parse_dimension(&tag.dimension),label:builtin.map(|value|value.label.clone()).unwrap_or(tag.label),label_i18n:builtin.map(|value|value.label_i18n.clone()).unwrap_or_default(),sort_order:builtin.map(|value|value.sort_order).unwrap_or(tag.sort_order),builtin:builtin.is_some()}}).collect()}
fn validate_preset_tag_key(key:&str)->Result<(),AppError>{if !key.is_empty()&&key.len()<=255&&key.bytes().all(|byte|byte.is_ascii_lowercase()||byte.is_ascii_digit()||matches!(byte,b'_'|b'-'|b'.'|b':')){Ok(())}else{Err(AppError::BadRequest("invalid preset tag natural key".into()))}}
fn validate_preset_tag_id(preset_tag_id:&str)->Result<(),AppError>{nomifun_common::validate_uuidv7(preset_tag_id).map(|_|()).map_err(|error|AppError::BadRequest(format!("invalid preset_tag_id: {error}")))}
fn slugify_tag_label(label:&str)->String{let lower=label.to_lowercase();let mut slug=String::with_capacity(lower.len());for ch in lower.chars(){if ch.is_ascii_alphanumeric(){slug.push(ch)}else if !slug.ends_with('-'){slug.push('-')}}let slug=slug.trim_matches('-').to_owned();if slug.is_empty(){use std::hash::{Hash,Hasher};let mut hasher=std::collections::hash_map::DefaultHasher::new();label.hash(&mut hasher);format!("tag-{:08x}",hasher.finish()as u32)}else{slug}}
fn deduplicate_tag_key(base:&str,existing:&HashSet<String>)->String{if !existing.contains(base){return base.to_owned()}for n in 2..=999{let candidate=format!("{base}-{n}");if !existing.contains(&candidate){return candidate}}format!("{base}-{}",nomifun_common::now_ms())}
fn localized_value(map:&HashMap<String,String>,locale:&str)->Option<String>{map.get(locale).cloned().or_else(||map.get(locale.split('-').next().unwrap_or(locale)).cloned())}
fn dedupe(values:&mut Vec<String>){let mut seen=HashSet::new();values.retain(|v|seen.insert(v.clone()))}
fn collect_localizations(names:&HashMap<String,String>,descriptions:&HashMap<String,String>,instructions:&HashMap<String,String>)->Vec<(String,Option<String>,Option<String>,Option<String>,Option<String>)>{let keys:HashSet<_>=names.keys().chain(descriptions.keys()).chain(instructions.keys()).cloned().collect();keys.into_iter().map(|k|(k.clone(),names.get(&k).cloned(),descriptions.get(&k).cloned(),None,instructions.get(&k).cloned())).collect()}
fn collect_examples_i18n(rows:&[nomifun_db::PresetExampleRow])->HashMap<String,Vec<String>>{let mut output=HashMap::new();for row in rows.iter().filter(|row|!row.locale.is_empty()){output.entry(row.locale.clone()).or_insert_with(Vec::new).push(row.prompt.clone());}output}
fn flatten_examples(defaults:Vec<String>,localized:HashMap<String,Vec<String>>)->Vec<(String,String)>{defaults.into_iter().map(|value|(String::new(),value)).chain(localized.into_iter().flat_map(|(locale,values)|values.into_iter().map(move|value|(locale.clone(),value)))).collect()}
fn find_asset(dir:&std::path::Path,id:&str)->Option<AvatarAsset>{for e in std::fs::read_dir(dir).ok()?.flatten(){if e.path().file_stem()?.to_str()?==id{return Some(AvatarAsset{bytes:std::fs::read(e.path()).ok()?,extension:e.path().extension().and_then(|v|v.to_str()).map(str::to_lowercase)})}}None}
fn remove_files_with_stem(dir:&std::path::Path,id:&str){if let Ok(entries)=std::fs::read_dir(dir){for e in entries.flatten(){if e.path().file_stem().and_then(|v|v.to_str())==Some(id){let _=std::fs::remove_file(e.path());}}}}
#[cfg(test)]
mod tag_key_tests {
    use super::*;

    #[test]
    fn tag_key_is_slugified_from_label() {
        assert_eq!(slugify_tag_label("Research & Writing"), "research-writing");
        assert_eq!(slugify_tag_label("  Data/Viz  "), "data-viz");
    }

    #[test]
    fn tag_key_collision_uses_one_based_numeric_suffixes() {
        let existing = HashSet::from(["research".to_owned(), "research-2".to_owned()]);
        assert_eq!(deduplicate_tag_key("research", &existing), "research-3");
        assert_eq!(deduplicate_tag_key("coding", &existing), "coding");
    }

    #[test]
    fn tag_key_validation_is_shared_by_routes_and_bindings() {
        assert!(validate_preset_tag_key("research-2").is_ok());
        assert!(validate_preset_tag_key("Research").is_err());
        assert!(validate_preset_tag_key("research/tag").is_err());
    }

    #[test]
    fn agent_references_accept_only_bare_uuidv7_business_ids() {
        assert!(
            validate_agent_reference("0190f5fe-7c00-7a00-8000-000000000103").is_ok()
        );
        for legacy_alias in ["gemini", "agent_builtin_gemini", "agent-1"] {
            assert!(
                validate_agent_reference(legacy_alias).is_err(),
                "{legacy_alias} must not be accepted as agent_id"
            );
        }
    }

    #[test]
    fn builtin_tags_are_not_duplicated_by_their_materialized_rows() {
        let builtin = vec![crate::builtin::BuiltinTag {
            key: "office".into(),
            dimension: "scenario".into(),
            label: "Office".into(),
            label_i18n: HashMap::from([("zh-CN".into(), "办公".into())]),
            sort_order: 1,
        }];
        let stored = vec![
            nomifun_db::PresetTagRow {
                id: 1,
                preset_tag_id: "0190f5fe-7c00-7a00-8000-000000000121".into(),
                key: "office".into(),
                dimension: "scenario".into(),
                label: "stale materialized label".into(),
                sort_order: 1,
                created_at: 1,
            },
            nomifun_db::PresetTagRow {
                id: 2,
                preset_tag_id: "0190f5fe-7c00-7a00-8000-000000000122".into(),
                key: "custom".into(),
                dimension: "audience".into(),
                label: "Custom".into(),
                sort_order: 2,
                created_at: 2,
            },
        ];

        let tags = merge_preset_tags(&builtin, stored);
        assert_eq!(tags.iter().filter(|tag| tag.key == "office").count(), 1);
        assert_eq!(tags.len(), 2);
        assert!(tags.iter().any(|tag| tag.key == "office" && tag.builtin));
        assert!(tags.iter().any(|tag| tag.key == "custom" && !tag.builtin));
        assert!(tags.iter().all(|tag| validate_preset_tag_id(&tag.preset_tag_id).is_ok()));
    }
}

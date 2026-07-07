//! "Get model by capability" — the pure resolution the backend authority and
//! consumers (creative workshop, TTS/ASR, vision routing) use to pick models by
//! [`ModelTask`] + required [`ModelTrait`]s, instead of each caller re-running a
//! name heuristic client-side.
//!
//! Pure over its inputs (providers + profiles) so it is trivially unit-tested
//! and free of repository/IO deps; the HTTP route composes it with the repos.

use serde::{Deserialize, Serialize};

use crate::model_task::{derive_tasks_and_traits, ModelProfile, ModelTask, ModelTrait};
use crate::provider::ProviderResponse;

/// A concrete (provider, model) selection. Named to avoid colliding with the
/// orchestrator's `ModelRef`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CatalogModelRef {
    pub provider_id: String,
    pub model: String,
}

/// Request body for `POST /api/model-profiles/resolve`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResolveModelsRequest {
    pub task: ModelTask,
    #[serde(default)]
    pub required_traits: Vec<ModelTrait>,
}

/// Response body for `POST /api/model-profiles/resolve`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResolveModelsResponse {
    pub models: Vec<CatalogModelRef>,
}

/// Whether a model (given its resolved tasks/traits) satisfies the query.
fn matches(tasks: &[ModelTask], traits: &[ModelTrait], task: ModelTask, required: &[ModelTrait]) -> bool {
    tasks.contains(&task) && required.iter().all(|rt| traits.contains(rt))
}

/// Resolve all enabled models across enabled providers that support `task` and
/// carry every trait in `required_traits`. Authoritative profiles win; a model
/// with no stored profile falls back to the name/platform heuristic so results
/// are never silently empty before backfill completes.
pub fn resolve_models(
    providers: &[ProviderResponse],
    profiles: &[ModelProfile],
    task: ModelTask,
    required_traits: &[ModelTrait],
) -> Vec<CatalogModelRef> {
    let mut out = Vec::new();
    for provider in providers {
        if !provider.enabled {
            continue;
        }
        for model in &provider.models {
            // Per-model enable flag: absent = enabled; explicit false = disabled.
            let model_enabled = provider
                .model_enabled
                .as_ref()
                .and_then(|m| m.get(model))
                .copied()
                .unwrap_or(true);
            if !model_enabled {
                continue;
            }

            let profile = profiles
                .iter()
                .find(|p| p.provider_id == provider.id && &p.model == model);

            let included = match profile {
                Some(p) => matches(&p.tasks, &p.traits, task, required_traits),
                None => {
                    let (tasks, traits) = derive_tasks_and_traits(&provider.platform, model);
                    matches(&tasks, &traits, task, required_traits)
                }
            };
            if included {
                out.push(CatalogModelRef { provider_id: provider.id.clone(), model: model.clone() });
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::ProviderResponse;
    use std::collections::HashMap;

    fn provider(id: &str, platform: &str, models: &[&str]) -> ProviderResponse {
        ProviderResponse {
            id: id.into(),
            platform: platform.into(),
            name: id.into(),
            base_url: "https://x.test/v1".into(),
            api_key: "k".into(),
            models: models.iter().map(|s| s.to_string()).collect(),
            enabled: true,
            capabilities: vec![],
            context_limit: None,
            model_context_limits: None,
            model_protocols: None,
            model_descriptions: None,
            model_enabled: None,
            model_health: None,
            bedrock_config: None,
            is_full_url: false,
            sort_order: 0,
            created_at: 0,
            updated_at: 0,
        }
    }

    fn profile(pid: &str, model: &str, tasks: Vec<ModelTask>, traits: Vec<ModelTrait>) -> ModelProfile {
        ModelProfile {
            provider_id: pid.into(),
            model: model.into(),
            tasks,
            traits,
            params: serde_json::Value::Null,
            source: crate::model_task::ProfileSource::User,
            updated_at: 0,
        }
    }

    #[test]
    fn resolves_by_task_from_profiles() {
        let providers = vec![provider("p1", "stepfun-plan", &["step-image-edit-2", "gpt-4o"])];
        let profiles = vec![
            profile("p1", "step-image-edit-2", vec![ModelTask::ImageGeneration, ModelTask::ImageEdit], vec![]),
            profile("p1", "gpt-4o", vec![ModelTask::Chat], vec![ModelTrait::VisionInput]),
        ];
        let img = resolve_models(&providers, &profiles, ModelTask::ImageGeneration, &[]);
        assert_eq!(img, vec![CatalogModelRef { provider_id: "p1".into(), model: "step-image-edit-2".into() }]);
        let chat = resolve_models(&providers, &profiles, ModelTask::Chat, &[]);
        assert_eq!(chat.len(), 1);
        assert_eq!(chat[0].model, "gpt-4o");
    }

    #[test]
    fn required_trait_filters() {
        let providers = vec![provider("p1", "openai", &["gpt-4o", "o1-mini"])];
        let profiles = vec![
            profile("p1", "gpt-4o", vec![ModelTask::Chat], vec![ModelTrait::VisionInput]),
            profile("p1", "o1-mini", vec![ModelTask::Chat], vec![]),
        ];
        let vision = resolve_models(&providers, &profiles, ModelTask::Chat, &[ModelTrait::VisionInput]);
        assert_eq!(vision.len(), 1);
        assert_eq!(vision[0].model, "gpt-4o");
    }

    #[test]
    fn falls_back_to_heuristic_when_no_profile() {
        let providers = vec![provider("p1", "openai", &["dall-e-3"])];
        // No profiles at all — heuristic should still surface the image model.
        let img = resolve_models(&providers, &[], ModelTask::ImageGeneration, &[]);
        assert_eq!(img.len(), 1);
        assert_eq!(img[0].model, "dall-e-3");
    }

    #[test]
    fn disabled_model_and_provider_excluded() {
        let mut providers = vec![provider("p1", "openai", &["gpt-4o", "gpt-4o-mini"])];
        let mut me = HashMap::new();
        me.insert("gpt-4o-mini".to_string(), false);
        providers[0].model_enabled = Some(me);
        let profiles = vec![
            profile("p1", "gpt-4o", vec![ModelTask::Chat], vec![]),
            profile("p1", "gpt-4o-mini", vec![ModelTask::Chat], vec![]),
        ];
        let chat = resolve_models(&providers, &profiles, ModelTask::Chat, &[]);
        assert_eq!(chat.len(), 1);
        assert_eq!(chat[0].model, "gpt-4o");

        providers[0].enabled = false;
        assert!(resolve_models(&providers, &profiles, ModelTask::Chat, &[]).is_empty());
    }
}

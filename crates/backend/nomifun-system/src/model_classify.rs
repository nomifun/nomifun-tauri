//! Model classification suggestions for the Creative Workshop.
//!
//! When a provider's model list is fetched (see [`ModelFetchService`]), the
//! Creative Workshop wants to know which of the newly-seen models can generate
//! images or videos. That signal is a **name heuristic** — there is no
//! per-model capability field persisted on the provider (provider `capabilities`
//! is provider-level). Rather than add a `model_capabilities` column (a schema
//! change), the suggestion is computed on demand in this wire/read layer,
//! delegating to the shared engine [`nomifun_api_types::infer_generation_capabilities`].
//!
//! The result is a **suggested default**: the user may override it. This module
//! is the backend hookpoint reused by the future `nomifun-creation` engine and
//! any endpoint that wants to annotate a fetched model list; it never mutates
//! provider rows.
//!
//! [`ModelFetchService`]: crate::ModelFetchService

use nomifun_api_types::{ModelInfo, ModelType, infer_generation_capabilities};

/// A single model's suggested Creative-Workshop generation capabilities.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelGenerationSuggestion {
    /// The model id (the string a caller would store in `provider.models`).
    pub model: String,
    /// Suggested generation capabilities — a non-empty subset of
    /// {[`ModelType::ImageGeneration`], [`ModelType::VideoGeneration`]}.
    pub capabilities: Vec<ModelType>,
}

/// Extract the model id from a [`ModelInfo`] (bare string or `{id,name}`).
fn model_id(info: &ModelInfo) -> &str {
    match info {
        ModelInfo::Id(id) => id,
        ModelInfo::Named { id, .. } => id,
    }
}

/// Classify a freshly-fetched model list into generation-capability suggestions.
///
/// Only models the name heuristic recognizes as image/video generators are
/// returned (chat/embedding/rerank models are omitted), so the caller can hand
/// the result straight to a Creative-Workshop picker.
pub fn suggest_generation_capabilities(models: &[ModelInfo]) -> Vec<ModelGenerationSuggestion> {
    models
        .iter()
        .filter_map(|info| {
            let model = model_id(info);
            let capabilities = infer_generation_capabilities(model);
            if capabilities.is_empty() {
                None
            } else {
                Some(ModelGenerationSuggestion {
                    model: model.to_string(),
                    capabilities,
                })
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_mixed_model_list() {
        let models = vec![
            ModelInfo::Id("gpt-4o".into()),
            ModelInfo::Id("dall-e-3".into()),
            ModelInfo::Named {
                id: "sora-2".into(),
                name: "Sora 2".into(),
            },
            ModelInfo::Id("text-embedding-3-large".into()),
        ];
        let out = suggest_generation_capabilities(&models);
        // Only the two generators survive.
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].model, "dall-e-3");
        assert_eq!(out[0].capabilities, vec![ModelType::ImageGeneration]);
        assert_eq!(out[1].model, "sora-2");
        assert_eq!(out[1].capabilities, vec![ModelType::VideoGeneration]);
    }

    #[test]
    fn empty_when_no_generators() {
        let models = vec![ModelInfo::Id("claude-opus-4".into()), ModelInfo::Id("gpt-4o-mini".into())];
        assert!(suggest_generation_capabilities(&models).is_empty());
    }
}

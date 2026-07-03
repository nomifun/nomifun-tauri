//! Production [`KnowledgeCompleter`]: resolves a default provider/model and
//! runs a one-shot completion. Same layering as the companion learner's
//! `LiveCompanionCompleter` and the IDMM sidecar's `LiveCompleter` — the knowledge
//! crate holds only the trait, this crate provides the provider-backed
//! implementation, and the app layer wires it via
//! `KnowledgeService::set_completer`.
//!
//! Unlike companion/IDMM there is no per-feature model setting (yet): knowledge
//! autogen is a background curation task, so the default is the first
//! enabled provider (registry creation order) and its first enabled model.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use nomifun_common::AppError;
use nomifun_db::IProviderRepository;
use nomifun_knowledge::KnowledgeCompleter;

use crate::factory::provider_config::{one_shot_completion, resolve_provider_config, user_message};

/// READMEs can be sizeable; keep enough room that the strict-JSON overview
/// reply (description + full readme_markdown) never gets cut mid-object —
/// a truncated reply is guaranteed-unparseable. The prompt side also bounds
/// the README length (see `autogen::OVERVIEW_SYSTEM`).
const KNOWLEDGE_MAX_TOKENS: u32 = 8192;

/// Provider-backed completer for knowledge autogen / snapshot compression.
pub struct LiveKnowledgeCompleter {
    pub provider_repo: Arc<dyn IProviderRepository>,
    pub encryption_key: [u8; 32],
    pub workspace: PathBuf,
}

impl LiveKnowledgeCompleter {
    /// First enabled provider (creation order) + its first enabled model.
    async fn resolve_default_model(&self) -> Result<(String, String), AppError> {
        let providers = self
            .provider_repo
            .list()
            .await
            .map_err(|e| AppError::Internal(format!("failed to list providers: {e}")))?;
        for provider in providers.iter().filter(|p| p.enabled) {
            if let Some(model) = first_enabled_model(&provider.models, provider.model_enabled.as_deref()) {
                return Ok((provider.id.clone(), model));
            }
        }
        Err(AppError::Conflict(
            "knowledge autogen unavailable: no enabled provider/model is configured".into(),
        ))
    }

    /// Resolve the given `(provider_id, model)` into a provider config and run
    /// the one-shot completion. Shared by [`KnowledgeCompleter::complete`]
    /// (which feeds it the default pick) and
    /// [`KnowledgeCompleter::complete_with`] (which feeds it the caller's
    /// explicit pick), so the resolve→complete tail is identical regardless
    /// of how the model was chosen.
    async fn complete_for_model(
        &self,
        system: &str,
        user: &str,
        provider_id: &str,
        model: &str,
    ) -> Result<String, AppError> {
        let cfg = resolve_provider_config(
            &self.provider_repo,
            &self.encryption_key,
            provider_id,
            model,
            &self.workspace,
        )
        .await?;
        one_shot_completion(&cfg, system, vec![user_message(user)], KNOWLEDGE_MAX_TOKENS).await
    }
}

#[async_trait::async_trait]
impl KnowledgeCompleter for LiveKnowledgeCompleter {
    async fn complete(&self, system: &str, user: &str) -> Result<String, AppError> {
        let (provider_id, model) = self.resolve_default_model().await?;
        self.complete_for_model(system, user, &provider_id, &model).await
    }

    /// Honor the caller's explicit `(provider_id, model)`, skipping the
    /// default-model resolution entirely — the knowledge UI uses this to let
    /// the user pick which model generates/regenerates a base.
    async fn complete_with(
        &self,
        system: &str,
        user: &str,
        provider_id: &str,
        model: &str,
    ) -> Result<String, AppError> {
        self.complete_for_model(system, user, provider_id, model).await
    }
}

/// First entry of the `models` JSON array that the `model_enabled` JSON map
/// does not disable (absent ⇒ enabled, matching the provider API semantics).
pub(crate) fn first_enabled_model(models_json: &str, model_enabled_json: Option<&str>) -> Option<String> {
    let models: Vec<String> = serde_json::from_str(models_json).unwrap_or_default();
    let enabled: HashMap<String, bool> = model_enabled_json
        .and_then(|raw| serde_json::from_str(raw).ok())
        .unwrap_or_default();
    models
        .into_iter()
        .map(|m| m.trim().to_owned())
        .find(|m| !m.is_empty() && enabled.get(m).copied().unwrap_or(true))
}

/// Resolve the app's DEFAULT `(provider_id, model)`: the first enabled provider
/// (creation order) and its first enabled model. `None` when no enabled
/// provider/model is configured. The shared "what model would the app use by
/// default" resolution — reused wherever a caller has no explicit model (e.g. a
/// public agent whose own model field is unset, so it answers as soon as ANY
/// provider is configured, no per-agent setup required).
pub async fn resolve_default_model(
    provider_repo: &std::sync::Arc<dyn IProviderRepository>,
) -> Option<(String, String)> {
    let providers = provider_repo.list().await.ok()?;
    providers
        .iter()
        .filter(|p| p.enabled)
        .find_map(|p| first_enabled_model(&p.models, p.model_enabled.as_deref()).map(|m| (p.id.clone(), m)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use nomifun_db::models::Provider;
    use nomifun_db::{CreateProviderParams, DbError, UpdateProviderParams};

    fn provider(id: &str, enabled: bool, models: &str, model_enabled: Option<&str>) -> Provider {
        Provider {
            id: id.into(),
            platform: "openai".into(),
            name: id.into(),
            base_url: String::new(),
            api_key_encrypted: String::new(),
            models: models.into(),
            enabled,
            capabilities: "[]".into(),
            context_limit: None,
            model_context_limits: None,
            model_protocols: None,
            model_descriptions: None,
            model_enabled: model_enabled.map(str::to_owned),
            model_health: None,
            bedrock_config: None,
            is_full_url: false,
            created_at: 0,
            updated_at: 0,
        }
    }

    struct ListOnlyRepo(Vec<Provider>);

    #[async_trait::async_trait]
    impl IProviderRepository for ListOnlyRepo {
        async fn list(&self) -> Result<Vec<Provider>, DbError> {
            Ok(self.0.clone())
        }
        async fn find_by_id(&self, id: &str) -> Result<Option<Provider>, DbError> {
            Ok(self.0.iter().find(|p| p.id == id).cloned())
        }
        async fn create(&self, _params: CreateProviderParams<'_>) -> Result<Provider, DbError> {
            unimplemented!("not used by these tests")
        }
        async fn update(&self, _id: &str, _params: UpdateProviderParams<'_>) -> Result<Provider, DbError> {
            unimplemented!("not used by these tests")
        }
        async fn delete(&self, _id: &str) -> Result<(), DbError> {
            unimplemented!("not used by these tests")
        }
    }

    fn completer(providers: Vec<Provider>) -> LiveKnowledgeCompleter {
        LiveKnowledgeCompleter {
            provider_repo: Arc::new(ListOnlyRepo(providers)),
            encryption_key: [0u8; 32],
            workspace: std::env::temp_dir(),
        }
    }

    #[test]
    fn first_enabled_model_honors_enable_map() {
        assert_eq!(
            first_enabled_model(r#"["m1","m2"]"#, None).as_deref(),
            Some("m1"),
            "absent map means everything enabled"
        );
        assert_eq!(
            first_enabled_model(r#"["m1","m2"]"#, Some(r#"{"m1":false}"#)).as_deref(),
            Some("m2")
        );
        assert_eq!(first_enabled_model(r#"["m1"]"#, Some(r#"{"m1":false}"#)), None);
        assert_eq!(first_enabled_model("not json", None), None);
        assert_eq!(first_enabled_model(r#"["  "]"#, None), None);
    }

    #[tokio::test]
    async fn default_model_skips_disabled_providers_and_models() {
        // Disabled provider first, then an enabled one whose first model is
        // disabled — the pick must be (p2, m2).
        let c = completer(vec![
            provider("p1", false, r#"["m0"]"#, None),
            provider("p2", true, r#"["m1","m2"]"#, Some(r#"{"m1":false}"#)),
        ]);
        let (provider_id, model) = c.resolve_default_model().await.unwrap();
        assert_eq!((provider_id.as_str(), model.as_str()), ("p2", "m2"));
    }

    #[tokio::test]
    async fn resolve_default_model_free_fn_picks_first_enabled_else_none() {
        let repo: Arc<dyn IProviderRepository> = Arc::new(ListOnlyRepo(vec![
            provider("p1", false, r#"["m0"]"#, None),
            provider("p2", true, r#"["m1","m2"]"#, Some(r#"{"m1":false}"#)),
        ]));
        assert_eq!(resolve_default_model(&repo).await, Some(("p2".to_owned(), "m2".to_owned())));
        // No enabled provider/model → None (a public agent then truthfully reports
        // no model rather than pretending one exists).
        let none: Arc<dyn IProviderRepository> =
            Arc::new(ListOnlyRepo(vec![provider("p", false, r#"["m"]"#, None)]));
        assert_eq!(resolve_default_model(&none).await, None);
    }

    #[tokio::test]
    async fn default_model_errors_with_clear_message_when_unconfigured() {
        let c = completer(vec![provider("p1", false, r#"["m0"]"#, None)]);
        let err = c.resolve_default_model().await.unwrap_err();
        assert!(matches!(err, AppError::Conflict(_)), "{err:?}");
        assert!(err.to_string().contains("no enabled provider"), "{err}");
    }

    /// `complete_with` must resolve the EXPLICIT `(provider_id, model)` the
    /// caller passes, never the default. Pinned at the network-free
    /// resolution boundary: a provider id that the repo cannot find yields
    /// `BadRequest("Provider '…' not found")`. Passing the explicit `px`
    /// surfaces *that* id in the error — proving the override (not the
    /// enabled default `p1`) drove resolution.
    #[tokio::test]
    async fn complete_with_resolves_the_explicit_provider_not_the_default() {
        // `p1` is enabled, so the default path would pick it; `px` is absent.
        let c = completer(vec![provider("p1", true, r#"["m1"]"#, None)]);
        let err = c
            .complete_with("sys", "usr", "px", "model-z")
            .await
            .unwrap_err();
        assert!(matches!(err, AppError::BadRequest(_)), "{err:?}");
        assert!(err.to_string().contains("Provider 'px' not found"), "{err}");
    }

    /// `complete` keeps using `resolve_default_model`: with the enabled
    /// default `p1` present in the repo it gets PAST the (network-free)
    /// provider-resolution stage — so the failure is NOT "Provider not
    /// found". This is the contrast to the override test above and confirms
    /// the default path is unchanged.
    #[tokio::test]
    async fn complete_uses_the_default_provider() {
        let c = completer(vec![provider("p1", true, r#"["m1"]"#, None)]);
        // No network in tests: this will fail building/calling the provider,
        // but it must have resolved the existing default `p1` first — so the
        // error must not be a missing-provider BadRequest.
        let err = c.complete("sys", "usr").await.unwrap_err();
        assert!(
            !err.to_string().contains("not found"),
            "default path must resolve the existing provider p1, got: {err}"
        );
    }
}

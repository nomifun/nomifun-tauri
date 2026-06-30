//! Provider/model gateway tools + the shared nomi model resolution chain.
//!
//! The chain exists to kill the "cron job silently bound to a model-less
//! conversation, blows up at execution time with Provider '' not found"
//! class of bug: nomi sessions get a model AT CREATION, resolved as
//! explicit args → calling companion's own profile model → first configured
//! provider's first model → hard error with guidance.

use nomifun_common::ProviderWithModel;
use serde_json::{Value, json};

use crate::deps::{CallerCtx, GatewayDeps};

/// A provider row reduced to what the listing tool + resolution chain need.
#[derive(Debug, Clone)]
pub(crate) struct ProviderSummary {
    pub id: String,
    pub name: String,
    pub platform: String,
    pub enabled: bool,
    /// Effective model ids: the `models` JSON array filtered by the
    /// per-model `model_enabled` map (absent entry = enabled).
    pub models: Vec<String>,
    /// User-authored per-model descriptions (`model_id → description`), decoded
    /// from the provider's `model_descriptions` JSON. Used by the
    /// caps_orchestrator layer to (a) map an assistant's preferred model NAME to
    /// a `(provider_id, model)` in range and (b) fill `FleetMember::description`
    /// on the bare model-range members so the planner sees a description for
    /// both kinds of member. Empty when none are configured.
    pub model_descriptions: std::collections::HashMap<String, String>,
}

pub(crate) fn summarize_provider(row: &nomifun_db::models::Provider) -> ProviderSummary {
    let all_models: Vec<String> = serde_json::from_str(&row.models).unwrap_or_default();
    let enabled_map: serde_json::Map<String, Value> = row
        .model_enabled
        .as_deref()
        .and_then(|s| serde_json::from_str(s).ok())
        .unwrap_or_default();
    let models = all_models
        .into_iter()
        .filter(|m| enabled_map.get(m).and_then(Value::as_bool).unwrap_or(true))
        .collect();
    // Decode the user-authored per-model descriptions fail-soft (the column is
    // NOT NULL DEFAULT '{}'; a malformed value degrades to no descriptions).
    let model_descriptions: std::collections::HashMap<String, String> = row
        .model_descriptions
        .as_deref()
        .and_then(|s| serde_json::from_str(s).ok())
        .unwrap_or_default();
    ProviderSummary {
        id: row.id.clone(),
        name: row.name.clone(),
        platform: row.platform.clone(),
        enabled: row.enabled,
        models,
        model_descriptions,
    }
}

pub(crate) async fn load_provider_summaries(deps: &GatewayDeps) -> Result<Vec<ProviderSummary>, Value> {
    let rows = deps
        .provider_repo
        .list()
        .await
        .map_err(|e| json!({"error": format!("failed to list providers: {e}")}))?;
    Ok(rows.iter().map(summarize_provider).collect())
}

/// `nomi_list_providers` lives in `caps_provider`; this module retains only the
/// shared provider summaries + the nomi model-resolution chain.

/// Outcome of the model resolution chain, with the step that produced it
/// (surfaced to the calling agent so it can tell the owner what was picked).
#[derive(Debug, PartialEq)]
pub(crate) struct ResolvedModel {
    pub provider_id: String,
    pub model: String,
    pub source: &'static str,
}

/// The pure nomi model resolution chain:
/// 1. explicit provider+model → as given (provider must exist and be enabled)
/// 2. explicit provider only → that provider's first available model
/// 3. explicit model only → first enabled provider offering it
/// 4. calling companion's profile model (only when its provider still exists+enabled)
/// 5. first enabled provider's first model
/// 6. error with configuration guidance
pub(crate) fn resolve_model_chain(
    explicit_provider: Option<&str>,
    explicit_model: Option<&str>,
    companion_model: Option<(&str, &str)>,
    providers: &[ProviderSummary],
) -> Result<ResolvedModel, String> {
    let find = |id: &str| providers.iter().find(|p| p.id == id);
    let require_enabled = |pid: &str| -> Result<&ProviderSummary, String> {
        let p = find(pid)
            .ok_or_else(|| format!("provider '{pid}' not found; call nomi_list_providers for valid ids"))?;
        if !p.enabled {
            return Err(format!(
                "provider '{}' ({}) is disabled; pick another via nomi_list_providers",
                p.name, p.id
            ));
        }
        Ok(p)
    };

    match (explicit_provider, explicit_model) {
        (Some(pid), Some(model)) => {
            require_enabled(pid)?;
            return Ok(ResolvedModel {
                provider_id: pid.to_owned(),
                model: model.to_owned(),
                source: "explicit",
            });
        }
        (Some(pid), None) => {
            let p = require_enabled(pid)?;
            let model = p
                .models
                .first()
                .cloned()
                .ok_or_else(|| format!("provider '{}' ({}) has no available models", p.name, p.id))?;
            return Ok(ResolvedModel {
                provider_id: pid.to_owned(),
                model,
                source: "explicit_provider_first_model",
            });
        }
        (None, Some(model)) => {
            let p = providers
                .iter()
                .find(|p| p.enabled && p.models.iter().any(|m| m == model))
                .ok_or_else(|| {
                    format!("no enabled provider offers model '{model}'; call nomi_list_providers for valid combinations")
                })?;
            return Ok(ResolvedModel {
                provider_id: p.id.clone(),
                model: model.to_owned(),
                source: "explicit_model",
            });
        }
        (None, None) => {}
    }

    if let Some((pid, model)) = companion_model
        && !pid.is_empty()
        && !model.is_empty()
        && find(pid).map(|p| p.enabled).unwrap_or(false)
    {
        return Ok(ResolvedModel {
            provider_id: pid.to_owned(),
            model: model.to_owned(),
            source: "companion_profile",
        });
    }

    if let Some(p) = providers.iter().find(|p| p.enabled && !p.models.is_empty()) {
        return Ok(ResolvedModel {
            provider_id: p.id.clone(),
            model: p.models[0].clone(),
            source: "first_available_provider",
        });
    }

    Err("no model available: no provider is configured/enabled on this desktop. Call nomi_list_providers to confirm, then ask the owner to configure one in Settings → Providers — do NOT create nomi sessions or cron jobs without a model.".to_owned())
}

/// Async wrapper around [`resolve_model_chain`]: loads the provider rows and
/// the calling companion's profile model, returns a ready-to-persist
/// `ProviderWithModel` plus the resolution source.
pub(crate) async fn resolve_nomi_model(
    deps: &GatewayDeps,
    ctx: &CallerCtx,
    explicit_provider: Option<&str>,
    explicit_model: Option<&str>,
) -> Result<(ProviderWithModel, &'static str), Value> {
    let providers = load_provider_summaries(deps).await?;
    let companion_model = companion_profile_model(deps, ctx).await;
    match resolve_model_chain(
        explicit_provider,
        explicit_model,
        companion_model.as_ref().map(|(p, m)| (p.as_str(), m.as_str())),
        &providers,
    ) {
        Ok(r) => {
            let model = r.model;
            Ok((
                ProviderWithModel {
                    provider_id: r.provider_id,
                    model: model.clone(),
                    use_model: Some(model),
                },
                r.source,
            ))
        }
        Err(msg) => Err(json!({"error": msg})),
    }
}

/// Explicit-args-only resolution (no companion / first-provider fallback): used by
/// `nomi_update_conversation`, where a model change is an explicit owner
/// instruction that must not be silently substituted.
pub(crate) async fn resolve_explicit_model(
    deps: &GatewayDeps,
    explicit_provider: Option<&str>,
    explicit_model: Option<&str>,
) -> Result<ProviderWithModel, Value> {
    let providers = load_provider_summaries(deps).await?;
    match resolve_model_chain(explicit_provider, explicit_model, None, &providers) {
        Ok(r) => {
            let model = r.model;
            Ok(ProviderWithModel {
                provider_id: r.provider_id,
                model: model.clone(),
                use_model: Some(model),
            })
        }
        Err(msg) => Err(json!({"error": msg})),
    }
}

/// The calling companion's configured profile model `(provider_id, model)`.
/// `ctx.companion_id` first; a missing/unconfigured bound companion degrades to the
/// default companion (mirrors `CompanionMasterAgentProfile`).
async fn companion_profile_model(deps: &GatewayDeps, ctx: &CallerCtx) -> Option<(String, String)> {
    if let Some(id) = &ctx.companion_id
        && let Ok(p) = deps.companion_service.get_companion(id).await
        && p.model.is_configured()
    {
        return Some((p.model.provider_id, p.model.model));
    }
    let default_id = deps.companion_service.default_companion_id().await?;
    let p = deps.companion_service.get_companion(&default_id).await.ok()?;
    p.model
        .is_configured()
        .then(|| (p.model.provider_id, p.model.model))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn provider(id: &str, enabled: bool, models: &[&str]) -> ProviderSummary {
        ProviderSummary {
            id: id.to_owned(),
            name: format!("name-{id}"),
            platform: "openai".to_owned(),
            enabled,
            models: models.iter().map(|m| m.to_string()).collect(),
            model_descriptions: std::collections::HashMap::new(),
        }
    }

    #[test]
    fn explicit_provider_and_model_win() {
        let providers = vec![provider("p1", true, &["m1"]), provider("p2", true, &["m2"])];
        let r = resolve_model_chain(Some("p2"), Some("custom-model"), Some(("p1", "m1")), &providers).unwrap();
        assert_eq!(r.provider_id, "p2");
        assert_eq!(r.model, "custom-model");
        assert_eq!(r.source, "explicit");
    }

    #[test]
    fn explicit_unknown_provider_errors_instead_of_falling_back() {
        let providers = vec![provider("p1", true, &["m1"])];
        let err = resolve_model_chain(Some("ghost"), Some("m"), Some(("p1", "m1")), &providers).unwrap_err();
        assert!(err.contains("ghost"), "{err}");
    }

    #[test]
    fn explicit_disabled_provider_errors() {
        let providers = vec![provider("p1", false, &["m1"])];
        let err = resolve_model_chain(Some("p1"), None, None, &providers).unwrap_err();
        assert!(err.contains("disabled"), "{err}");
    }

    #[test]
    fn explicit_provider_only_takes_its_first_model() {
        let providers = vec![provider("p1", true, &["a", "b"])];
        let r = resolve_model_chain(Some("p1"), None, None, &providers).unwrap();
        assert_eq!((r.provider_id.as_str(), r.model.as_str()), ("p1", "a"));
        assert_eq!(r.source, "explicit_provider_first_model");
    }

    #[test]
    fn explicit_model_only_scans_enabled_providers() {
        let providers = vec![
            provider("p0", false, &["target"]),
            provider("p1", true, &["other"]),
            provider("p2", true, &["target"]),
        ];
        let r = resolve_model_chain(None, Some("target"), None, &providers).unwrap();
        assert_eq!(r.provider_id, "p2");
        assert_eq!(r.source, "explicit_model");
    }

    #[test]
    fn companion_profile_used_when_no_explicit_args() {
        let providers = vec![provider("p1", true, &["m1"]), provider("p2", true, &["m2"])];
        let r = resolve_model_chain(None, None, Some(("p2", "m2")), &providers).unwrap();
        assert_eq!((r.provider_id.as_str(), r.model.as_str()), ("p2", "m2"));
        assert_eq!(r.source, "companion_profile");
    }

    #[test]
    fn companion_profile_with_deleted_provider_falls_through_to_first_available() {
        let providers = vec![provider("p1", true, &["m1"])];
        let r = resolve_model_chain(None, None, Some(("gone", "mx")), &providers).unwrap();
        assert_eq!((r.provider_id.as_str(), r.model.as_str()), ("p1", "m1"));
        assert_eq!(r.source, "first_available_provider");
    }

    #[test]
    fn first_available_skips_disabled_and_empty_providers() {
        let providers = vec![
            provider("off", false, &["m"]),
            provider("empty", true, &[]),
            provider("good", true, &["pick-me"]),
        ];
        let r = resolve_model_chain(None, None, None, &providers).unwrap();
        assert_eq!((r.provider_id.as_str(), r.model.as_str()), ("good", "pick-me"));
    }

    #[test]
    fn nothing_resolvable_returns_guidance_error() {
        let err = resolve_model_chain(None, None, None, &[]).unwrap_err();
        assert!(err.contains("nomi_list_providers"), "{err}");
    }

    #[test]
    fn summarize_filters_per_model_enabled_map() {
        let row = nomifun_db::models::Provider {
            id: "p1".into(),
            platform: "openai".into(),
            name: "P1".into(),
            base_url: String::new(),
            api_key_encrypted: String::new(),
            models: r#"["a","b","c"]"#.into(),
            enabled: true,
            capabilities: "[]".into(),
            context_limit: None,
            model_protocols: None,
            model_descriptions: None,
            model_enabled: Some(r#"{"b": false}"#.into()),
            model_health: None,
            bedrock_config: None,
            is_full_url: false,
            created_at: 0,
            updated_at: 0,
        };
        let s = summarize_provider(&row);
        assert_eq!(s.models, vec!["a".to_owned(), "c".to_owned()]);
    }
}

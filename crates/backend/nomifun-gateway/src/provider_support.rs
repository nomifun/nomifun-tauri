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
    pub provider_id: String,
    pub name: String,
    pub platform: String,
    pub enabled: bool,
    /// Effective model ids: the `models` JSON array filtered by the
    /// per-model `model_enabled` map (absent entry = enabled).
    pub models: Vec<String>,
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
    ProviderSummary {
        provider_id: row.provider_id.clone(),
        name: row.name.clone(),
        platform: row.platform.clone(),
        enabled: row.enabled,
        models,
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
/// 1. explicit provider/model object → exact pair (provider must exist and be enabled)
/// 2. calling companion's profile model (only when its provider still exists+enabled)
/// 3. first enabled provider's first model
/// 4. error with configuration guidance
pub(crate) fn resolve_model_chain(
    explicit_model: Option<&ProviderWithModel>,
    companion_model: Option<(&str, &str)>,
    providers: &[ProviderSummary],
) -> Result<ResolvedModel, String> {
    let find = |provider_id: &str| providers.iter().find(|p| p.provider_id == provider_id);
    let require_enabled = |pid: &str| -> Result<&ProviderSummary, String> {
        let p = find(pid)
            .ok_or_else(|| format!("provider '{pid}' not found; call nomi_list_providers for valid ids"))?;
        if !p.enabled {
            return Err(format!(
                "provider '{}' ({}) is disabled; pick another via nomi_list_providers",
                p.name, p.provider_id
            ));
        }
        Ok(p)
    };

    if let Some(explicit) = explicit_model {
        explicit.validate()?;
        let pid = explicit.provider_id.as_str();
        let model = explicit.use_model.as_deref().unwrap_or(&explicit.model);
        require_enabled(pid)?;
        return Ok(ResolvedModel {
            provider_id: pid.to_owned(),
            model: model.to_owned(),
            source: "explicit",
        });
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
            provider_id: p.provider_id.clone(),
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
    explicit_model: Option<&ProviderWithModel>,
) -> Result<(ProviderWithModel, &'static str), Value> {
    let providers = load_provider_summaries(deps).await?;
    let companion_model = companion_profile_model(deps, ctx).await;
    match resolve_model_chain(
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
    explicit_model: ProviderWithModel,
) -> Result<ProviderWithModel, Value> {
    let providers = load_provider_summaries(deps).await?;
    match resolve_model_chain(Some(&explicit_model), None, &providers) {
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
/// default companion (mirrors `CompanionChannelAgentProfile`).
async fn companion_profile_model(deps: &GatewayDeps, ctx: &CallerCtx) -> Option<(String, String)> {
    if let Some(id) = &ctx.companion_id
        && let Ok(p) = deps.companion_service.get_companion(id.as_str()).await
        && let Some(model) = p.model
    {
        return Some((model.provider_id, model.model));
    }
    let default_id = deps.companion_service.default_companion_id().await?;
    let p = deps.companion_service.get_companion(&default_id).await.ok()?;
    p.model.map(|model| (model.provider_id, model.model))
}

#[cfg(test)]
mod tests {
    use super::*;

    const PROVIDER_ID_1: &str = "0190f5fe-7c00-7a00-8000-000000000001";
    const PROVIDER_ID_2: &str = "0190f5fe-7c00-7a00-8000-000000000002";
    const PROVIDER_ID_3: &str = "0190f5fe-7c00-7a00-8000-000000000003";

    fn provider(id: &str, enabled: bool, models: &[&str]) -> ProviderSummary {
        ProviderSummary {
            provider_id: id.to_owned(),
            name: format!("name-{id}"),
            platform: "openai".to_owned(),
            enabled,
            models: models.iter().map(|m| m.to_string()).collect(),
        }
    }

    fn model(provider_id: &str, model: &str) -> ProviderWithModel {
        ProviderWithModel {
            provider_id: provider_id.to_owned(),
            model: model.to_owned(),
            use_model: None,
        }
    }

    #[test]
    fn explicit_provider_and_model_win() {
        let providers = vec![
            provider(PROVIDER_ID_1, true, &["m1"]),
            provider(PROVIDER_ID_2, true, &["m2"]),
        ];
        let explicit = model(PROVIDER_ID_2, "custom-model");
        let r =
            resolve_model_chain(Some(&explicit), Some((PROVIDER_ID_1, "m1")), &providers).unwrap();
        assert_eq!(r.provider_id, PROVIDER_ID_2);
        assert_eq!(r.model, "custom-model");
        assert_eq!(r.source, "explicit");
    }

    #[test]
    fn explicit_unknown_provider_errors_instead_of_falling_back() {
        let providers = vec![provider(PROVIDER_ID_1, true, &["m1"])];
        let explicit = model(PROVIDER_ID_2, "m");
        let err = resolve_model_chain(
            Some(&explicit),
            Some((PROVIDER_ID_1, "m1")),
            &providers,
        )
        .unwrap_err();
        assert!(err.contains(PROVIDER_ID_2), "{err}");
    }

    #[test]
    fn explicit_disabled_provider_errors() {
        let providers = vec![provider(PROVIDER_ID_1, false, &["m1"])];
        let explicit = model(PROVIDER_ID_1, "m1");
        let err = resolve_model_chain(Some(&explicit), None, &providers).unwrap_err();
        assert!(err.contains("disabled"), "{err}");
    }

    #[test]
    fn partial_explicit_model_shapes_are_rejected_by_deserialization() {
        assert!(
            serde_json::from_value::<ProviderWithModel>(json!({
                "provider_id": PROVIDER_ID_1
            }))
            .is_err()
        );
        assert!(
            serde_json::from_value::<ProviderWithModel>(json!({
                "model": "m1"
            }))
            .is_err()
        );
    }

    #[test]
    fn companion_profile_used_when_no_explicit_args() {
        let providers = vec![
            provider(PROVIDER_ID_1, true, &["m1"]),
            provider(PROVIDER_ID_2, true, &["m2"]),
        ];
        let r = resolve_model_chain(None, Some((PROVIDER_ID_2, "m2")), &providers).unwrap();
        assert_eq!(
            (r.provider_id.as_str(), r.model.as_str()),
            (PROVIDER_ID_2, "m2")
        );
        assert_eq!(r.source, "companion_profile");
    }

    #[test]
    fn companion_profile_with_deleted_provider_falls_through_to_first_available() {
        let providers = vec![provider(PROVIDER_ID_1, true, &["m1"])];
        let r = resolve_model_chain(None, Some((PROVIDER_ID_2, "mx")), &providers).unwrap();
        assert_eq!(
            (r.provider_id.as_str(), r.model.as_str()),
            (PROVIDER_ID_1, "m1")
        );
        assert_eq!(r.source, "first_available_provider");
    }

    #[test]
    fn first_available_skips_disabled_and_empty_providers() {
        let providers = vec![
            provider(PROVIDER_ID_1, false, &["m"]),
            provider(PROVIDER_ID_2, true, &[]),
            provider(PROVIDER_ID_3, true, &["pick-me"]),
        ];
        let r = resolve_model_chain(None, None, &providers).unwrap();
        assert_eq!(
            (r.provider_id.as_str(), r.model.as_str()),
            (PROVIDER_ID_3, "pick-me")
        );
    }

    #[test]
    fn nothing_resolvable_returns_guidance_error() {
        let err = resolve_model_chain(None, None, &[]).unwrap_err();
        assert!(err.contains("nomi_list_providers"), "{err}");
    }

    #[test]
    fn summarize_filters_per_model_enabled_map() {
        let row = nomifun_db::models::Provider {
            id: 1,
            provider_id: nomifun_common::ProviderId::new().into_string(),
            platform: "openai".into(),
            name: "P1".into(),
            base_url: String::new(),
            api_key_encrypted: String::new(),
            models: r#"["a","b","c"]"#.into(),
            enabled: true,
            capabilities: "[]".into(),
            model_context_limits: None,
            model_protocols: None,
            model_descriptions: None,
            model_enabled: Some(r#"{"b": false}"#.into()),
            model_health: None,
            bedrock_config: None,
            is_full_url: false,
            sort_order: 0,
            created_at: 0,
            updated_at: 0,
        };
        let s = summarize_provider(&row);
        assert_eq!(s.models, vec!["a".to_owned(), "c".to_owned()]);
    }
}

//! System-domain capabilities (registry form): desktop settings, client
//! preferences (theme / zoom / keep-awake / feature toggles), model-provider
//! CRUD, model fetching, and read-only system info.
//!
//! These tools let the LLM agent configure the desktop environment on behalf
//! of the user — the headline use case is "set my theme to dark" / "add a
//! new provider" / "change my zoom level" spoken to the companion.
//!
//! SKIPPED tools (listed at the bottom of this file) need extra GatewayDeps
//! fields the parent has not yet wired:
//! - `nomi_system_check_update` — needs `VersionCheckService`
//! - `nomi_system_factory_reset` — needs `data_dir: PathBuf`

use std::collections::HashMap;
use std::sync::Arc;

use nomifun_api_types::{
    CreateProviderRequest, FetchModelsRequest, UpdateProviderRequest, UpdateSettingsRequest,
};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::deps::GatewayDeps;
use crate::registry::{Capability, CapabilityMeta, DangerTier};
use crate::server::ok;

// ── param structs (single source: schema + runtime) ──────────────────────

#[derive(Deserialize, JsonSchema)]
struct GetSettingsParams {}

#[derive(Deserialize, JsonSchema)]
struct UpdateSettingsParams {
    /// System language code. Allowed: "en-US" or "zh-CN".
    #[serde(default)]
    language: Option<String>,
    /// Enable/disable desktop notifications globally.
    #[serde(default)]
    notification_enabled: Option<bool>,
    /// Enable/disable notifications specifically for cron-job results.
    #[serde(default)]
    cron_notification_enabled: Option<bool>,
    /// Enable/disable the command queue (batch-queued execution of LLM requests).
    #[serde(default)]
    command_queue_enabled: Option<bool>,
    /// Whether uploaded files should be saved to the current workspace.
    #[serde(default)]
    save_upload_to_workspace: Option<bool>,
}

#[derive(Deserialize, JsonSchema)]
struct GetPreferencesParams {
    /// Optional list of preference keys to fetch (omit to return all).
    /// Common keys: "theme", "ui.zoomFactor", "system.closeToTray",
    /// "companion.size", "system.keepAwake", "feature.*".
    #[serde(default)]
    keys: Option<Vec<String>>,
}

#[derive(Deserialize, JsonSchema)]
struct UpdatePreferencesParams {
    /// Map of key → JSON value to set. A `null` value deletes the key.
    /// Keys must be non-empty and at most 255 characters.
    ///
    /// Common keys (non-exhaustive):
    ///   "theme" (string: "light" | "dark" | "rhythm-dark" | …),
    ///   "ui.zoomFactor" (number: 0.5–2.0),
    ///   "system.closeToTray" (bool),
    ///   "system.keepAwake" (bool),
    ///   "companion.size" (number: px),
    ///   "feature.<name>" (bool).
    preferences: HashMap<String, Value>,
}

#[derive(Deserialize, JsonSchema)]
struct CreateProviderParams {
    /// Provider platform identifier (e.g. "openai", "anthropic", "gemini",
    /// "new-api", "bedrock", "vertex-ai", "minimax", "dashscope-coding", etc.).
    platform: String,
    /// Human-readable display name for this provider.
    name: String,
    /// API base URL (must start with http:// or https://). Empty string allowed
    /// only for bedrock platform.
    base_url: String,
    /// Plain-text API key (supports comma/newline-separated multi-keys for
    /// load balancing). Required for non-bedrock platforms.
    api_key: String,
    /// Initial model list. If omitted, use nomi_system_fetch_models after
    /// creation to populate.
    #[serde(default)]
    models: Option<Vec<String>>,
    /// Whether the provider is enabled (default true).
    #[serde(default)]
    enabled: Option<bool>,
    /// Optional context-window limit override (token count).
    #[serde(default)]
    context_limit: Option<i64>,
    /// Optional AWS Bedrock configuration (required when platform = "bedrock").
    /// Pass the full BedrockConfig object as JSON.
    #[serde(default)]
    bedrock_config: Option<Value>,
}

#[derive(Deserialize, JsonSchema)]
struct UpdateProviderParams {
    /// Provider id (from nomi_list_providers).
    id: String,
    /// New platform identifier (omit to keep).
    #[serde(default)]
    platform: Option<String>,
    /// New display name (omit to keep).
    #[serde(default)]
    name: Option<String>,
    /// New API base URL (omit to keep).
    #[serde(default)]
    base_url: Option<String>,
    /// New API key in plain text (omit to keep).
    #[serde(default)]
    api_key: Option<String>,
    /// Replace model list (omit to keep).
    #[serde(default)]
    models: Option<Vec<String>>,
    /// Enable or disable (omit to keep).
    #[serde(default)]
    enabled: Option<bool>,
    /// Override context-window limit (omit to keep).
    #[serde(default)]
    context_limit: Option<i64>,
    /// AWS Bedrock configuration update (omit to keep).
    #[serde(default)]
    bedrock_config: Option<Value>,
}

#[derive(Deserialize, JsonSchema)]
struct DeleteProviderParams {
    /// Provider id to permanently delete.
    id: String,
}

#[derive(Deserialize, JsonSchema)]
struct FetchModelsParams {
    /// Provider id whose models to fetch from the remote API.
    id: String,
    /// If true, attempt automatic URL correction on failure for
    /// OpenAI-compatible providers (probes common URL suffixes).
    #[serde(default)]
    try_fix: Option<bool>,
}

#[derive(Deserialize, JsonSchema)]
struct GetInfoParams {}

// ── handlers ──────────────────────────────────────────────────────────────

async fn get_settings(deps: Arc<GatewayDeps>, _p: GetSettingsParams) -> Value {
    match deps.settings_service.get_settings().await {
        Ok(settings) => ok(settings),
        Err(e) => json!({ "error": e.to_string() }),
    }
}

async fn update_settings(deps: Arc<GatewayDeps>, p: UpdateSettingsParams) -> Value {
    let req = UpdateSettingsRequest {
        language: p.language,
        notification_enabled: p.notification_enabled,
        cron_notification_enabled: p.cron_notification_enabled,
        command_queue_enabled: p.command_queue_enabled,
        save_upload_to_workspace: p.save_upload_to_workspace,
    };
    if req.is_empty() {
        return json!({ "error": "nothing to update: provide at least one field" });
    }
    match deps.settings_service.update_settings(req).await {
        Ok(settings) => ok(settings),
        Err(e) => json!({ "error": e.to_string() }),
    }
}

async fn get_preferences(deps: Arc<GatewayDeps>, p: GetPreferencesParams) -> Value {
    let keys_owned = p.keys.unwrap_or_default();
    let keys_ref: Vec<&str> = keys_owned.iter().map(String::as_str).collect();
    let filter = if keys_ref.is_empty() { None } else { Some(keys_ref.as_slice()) };
    match deps.client_pref_service.get_preferences(filter).await {
        Ok(prefs) => ok(prefs),
        Err(e) => json!({ "error": e.to_string() }),
    }
}

async fn update_preferences(deps: Arc<GatewayDeps>, p: UpdatePreferencesParams) -> Value {
    if p.preferences.is_empty() {
        return json!({ "error": "preferences map must not be empty" });
    }
    match deps.client_pref_service.update_preferences(p.preferences).await {
        Ok(()) => ok(json!({ "updated": true })),
        Err(e) => json!({ "error": e.to_string() }),
    }
}

async fn create_provider(deps: Arc<GatewayDeps>, p: CreateProviderParams) -> Value {
    // Map the bedrock_config Value passthrough into the typed struct.
    let bedrock_config = match p.bedrock_config {
        Some(val) => match serde_json::from_value(val) {
            Ok(cfg) => Some(cfg),
            Err(e) => return json!({ "error": format!("invalid bedrock_config: {e}") }),
        },
        None => None,
    };
    let req = CreateProviderRequest {
        id: None,
        platform: p.platform,
        name: p.name,
        base_url: p.base_url,
        api_key: p.api_key,
        models: p.models.unwrap_or_default(),
        enabled: p.enabled.unwrap_or(true),
        capabilities: vec![],
        context_limit: p.context_limit,
        model_protocols: None,
        model_descriptions: None,
        model_enabled: None,
        model_health: None,
        bedrock_config,
        is_full_url: false,
    };
    match deps.provider_service.create(req).await {
        Ok(resp) => ok(json!({
            "id": resp.id,
            "platform": resp.platform,
            "name": resp.name,
            "base_url": resp.base_url,
            "models": resp.models,
            "enabled": resp.enabled,
            "note": "provider created; use nomi_system_fetch_models to populate the model list from the remote API if models were not specified",
        })),
        Err(e) => json!({ "error": e.to_string() }),
    }
}

async fn update_provider(deps: Arc<GatewayDeps>, p: UpdateProviderParams) -> Value {
    let bedrock_config = match p.bedrock_config {
        Some(val) => match serde_json::from_value(val) {
            Ok(cfg) => Some(cfg),
            Err(e) => return json!({ "error": format!("invalid bedrock_config: {e}") }),
        },
        None => None,
    };
    let req = UpdateProviderRequest {
        platform: p.platform,
        name: p.name,
        base_url: p.base_url,
        api_key: p.api_key,
        models: p.models,
        enabled: p.enabled,
        capabilities: None,
        context_limit: p.context_limit,
        model_protocols: None,
        model_descriptions: None,
        model_enabled: None,
        model_health: None,
        bedrock_config,
        is_full_url: None,
    };
    match deps.provider_service.update(&p.id, req).await {
        Ok(resp) => ok(json!({
            "id": resp.id,
            "platform": resp.platform,
            "name": resp.name,
            "base_url": resp.base_url,
            "models": resp.models,
            "enabled": resp.enabled,
        })),
        Err(e) => json!({ "error": e.to_string() }),
    }
}

async fn delete_provider(deps: Arc<GatewayDeps>, p: DeleteProviderParams) -> Value {
    match deps.provider_service.delete(&p.id).await {
        Ok(()) => json!({ "result": format!("provider {} deleted", p.id) }),
        Err(e) => json!({ "error": e.to_string() }),
    }
}

async fn fetch_models(deps: Arc<GatewayDeps>, p: FetchModelsParams) -> Value {
    let req = FetchModelsRequest {
        try_fix: p.try_fix.unwrap_or(false),
    };
    match deps.model_fetch_service.fetch_models(&p.id, &req).await {
        Ok(resp) => {
            let mut result = json!({
                "models": resp.models,
                "count": resp.models.len(),
            });
            if let Some(fixed_url) = resp.fixed_base_url {
                result["fixed_base_url"] = json!(fixed_url);
                result["note"] = json!(
                    "the provider's base URL was auto-corrected; the new URL has been applied"
                );
            }
            ok(result)
        }
        Err(e) => json!({ "error": e.to_string() }),
    }
}

async fn get_info(_deps: Arc<GatewayDeps>, _p: GetInfoParams) -> Value {
    let info = nomifun_system::sysinfo::get_system_info();
    ok(info)
}

// ── registration ─────────────────────────────────────────────────────────

/// Register the system-domain capabilities.
pub(crate) fn register(out: &mut Vec<Capability>) {
    // 1. Settings (read)
    out.push(Capability::new::<GetSettingsParams, _, _>(
        CapabilityMeta::new(
            "nomi_system_get_settings",
            "system",
            "Read the desktop's system settings (language, notification toggles, etc.).",
            DangerTier::Read,
        ),
        |deps, _ctx, p| get_settings(deps, p),
    ));

    // 2. Settings (write)
    out.push(Capability::new::<UpdateSettingsParams, _, _>(
        CapabilityMeta::new(
            "nomi_system_update_settings",
            "system",
            "Partially update system settings (language, notification toggles, command queue, workspace upload). Only provided fields are changed.",
            DangerTier::Write,
        ),
        |deps, _ctx, p| update_settings(deps, p),
    ));

    // 3. Preferences (read)
    out.push(Capability::new::<GetPreferencesParams, _, _>(
        CapabilityMeta::new(
            "nomi_system_get_preferences",
            "system",
            "Read client preferences (theme, zoom, keep-awake, companion size, feature toggles, etc.). Omit keys to get all.",
            DangerTier::Read,
        ),
        |deps, _ctx, p| get_preferences(deps, p),
    ));

    // 4. Preferences (write) — the headline "set theme / zoom / keep-awake" tool
    out.push(Capability::new::<UpdatePreferencesParams, _, _>(
        CapabilityMeta::new(
            "nomi_system_update_preferences",
            "system",
            "Batch set/delete client preferences (theme, ui.zoomFactor, system.closeToTray, system.keepAwake, companion.size, feature toggles). Pass null value to delete a key.",
            DangerTier::Write,
        ),
        |deps, _ctx, p| update_preferences(deps, p),
    ));

    // 5. Create provider (sensitive — handles API keys)
    out.push(Capability::new::<CreateProviderParams, _, _>(
        CapabilityMeta::new(
            "nomi_system_create_provider",
            "system",
            "Register a new model provider (platform + base URL + API key). The service validates credentials format and encrypts the key at rest.",
            DangerTier::Sensitive,
        ),
        |deps, _ctx, p| create_provider(deps, p),
    ));

    // 6. Update provider (sensitive — may update API key)
    out.push(Capability::new::<UpdateProviderParams, _, _>(
        CapabilityMeta::new(
            "nomi_system_update_provider",
            "system",
            "Partially update an existing model provider (name, URL, API key, models, enabled). Only provided fields are changed.",
            DangerTier::Sensitive,
        ),
        |deps, _ctx, p| update_provider(deps, p),
    ));

    // 7. Delete provider (destructive)
    out.push(Capability::new::<DeleteProviderParams, _, _>(
        CapabilityMeta::new(
            "nomi_system_delete_provider",
            "system",
            "Permanently delete a model provider and all its stored credentials.",
            DangerTier::Destructive,
        ),
        |deps, _ctx, p| delete_provider(deps, p),
    ));

    // 8. Fetch models (write — triggers a network call and may auto-fix the URL)
    out.push(Capability::new::<FetchModelsParams, _, _>(
        CapabilityMeta::new(
            "nomi_system_fetch_models",
            "system",
            "Fetch the model list from a provider's remote API (by provider id). Use after creating a provider without specifying models.",
            DangerTier::Write,
        ),
        |deps, _ctx, p| fetch_models(deps, p),
    ));

    // 9. System info (read — pure, no service dependency beyond sysinfo)
    out.push(Capability::new::<GetInfoParams, _, _>(
        CapabilityMeta::new(
            "nomi_system_get_info",
            "system",
            "Read system info: data/cache/log directories, OS platform, and CPU architecture.",
            DangerTier::Read,
        ),
        |deps, _ctx, p| get_info(deps, p),
    ));
}

// ── SKIPPED tools ────────────────────────────────────────────────────────
//
// 10. `nomi_system_check_update` (Read)
//     Needs: `deps.version_check_service: nomifun_system::VersionCheckService`
//     Method: `version_check_service.check_update(&UpdateCheckRequest { .. })`
//     Not wired because VersionCheckService is not in the assumed GatewayDeps.
//
// 11. `nomi_system_factory_reset` (Destructive, deny_on Channel+Remote)
//     Needs: `deps.data_dir: PathBuf`
//     Method: `nomifun_common::factory_reset::write_marker(&data_dir, &ResetMarker::new(ResetScope::Full))`
//     Not wired because data_dir is not in the assumed GatewayDeps.

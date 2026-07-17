use std::collections::{HashMap, HashSet};
use std::fmt::Write;
use std::future::Future;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::body::Body;
use axum::extract::State;
use axum::http::{HeaderMap, HeaderName, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use nomifun_api_types::{
    ManagedModel, ManagedModelHealthBatchResult, ManagedModelHealthErrorKind,
    ManagedModelHealthResult, ManagedModelHealthStatus, ManagedModelServiceAvailability,
    ManagedModelServiceStatus,
};
use nomifun_common::{AppError, ProviderId, encrypt_string, now_ms};
use nomifun_db::{
    CreateProviderParams, IClientPreferenceRepository, IProviderRepository,
    UpdateProviderParams, models::Provider,
};
use reqwest::Url;
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::net::TcpListener;
use tokio::sync::{Mutex, RwLock, Semaphore};
use tokio::task::JoinSet;
use tracing::{debug, info, warn};

pub const FREE_MODEL_PLATFORM: &str = "nomifun-free-model";
pub const MANAGED_MODEL_PROTOCOL_VERSION: &str = "1";

const FREE_MODEL_PROVIDER_NAME: &str = "NomiFun Free Model";
const OPENCODE_MODELS_URL: &str = "https://opencode.ai/zen/v1/models";
const OPENCODE_CHAT_URL: &str = "https://opencode.ai/zen/v1/chat/completions";
const PUBLIC_SOURCE_ALIAS: &str = "oc";
const FREE_PRIVACY_NOTICE: &str = concat!(
    "Prompts and model responses are processed by an external free-model service. ",
    "Do not send sensitive information."
);
const FREE_LAST_REFRESH_PREF: &str = "managedModel.free.lastRefresh";
const FREE_LAST_ERROR_PREF: &str = "managedModel.free.lastError";
pub const DEFAULT_FREE_REFRESH_INTERVAL: Duration = Duration::from_secs(6 * 60 * 60);
const DEFAULT_FREE_REFRESH_JITTER: Duration = Duration::from_secs(15 * 60);
const DEFAULT_FREE_REFRESH_RETRY_BASE: Duration = Duration::from_secs(5 * 60);
const DEFAULT_FREE_REFRESH_RETRY_MAX: Duration = Duration::from_secs(60 * 60);
const FREE_HEALTH_TIMEOUT: Duration = Duration::from_secs(15);
const FREE_HEALTH_MAX_CONCURRENCY: usize = 3;
const FREE_HEALTH_PROMPT: &str = "Reply with exactly OK.";

// A small startup catalog means a fresh install can resolve a model before the
// first network refresh completes. Live refresh replaces this list when the
// fixed upstream catalog is available.
const FREE_SEED_MODELS: &[&str] = &[
    "big-pickle",
    "deepseek-v4-flash-free",
    "mimo-v2.5-free",
    "hy3-free",
    "nemotron-3-ultra-free",
    "north-mini-code-free",
];

fn is_managed_provider(value: &str) -> bool {
    let value = value.trim();
    value.eq_ignore_ascii_case(FREE_MODEL_PLATFORM)
}

/// Return true when a provider platform belongs to a protected managed supply.
pub fn is_managed_provider_platform(platform: &str) -> bool {
    is_managed_provider(platform)
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct CatalogModel {
    id: String,
    name: String,
}

impl CatalogModel {
    fn opencode(id: impl Into<String>, name: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            name: name.into(),
        }
    }
}

#[derive(Debug, Clone)]
struct FreeState {
    enabled: bool,
    catalog: Vec<CatalogModel>,
    model_enabled: HashMap<String, bool>,
    last_refresh: Option<i64>,
    last_error: Option<String>,
    automatic_refresh: bool,
    refresh_interval: Duration,
    next_refresh: Option<i64>,
}

#[derive(Clone)]
struct LoopbackState {
    service: Arc<ManagedModelService>,
    auth_token: String,
}

/// In-memory control plane for the stable `nomifun-free-model` supply.
///
/// The service owns no arbitrary upstream URL. Both discovery and inference
/// are fixed to the OpenCode Zen HTTPS endpoints above.
pub struct ManagedModelService {
    provider_id: String,
    provider_repo: Arc<dyn IProviderRepository>,
    preference_repo: Option<Arc<dyn IClientPreferenceRepository>>,
    http_client: reqwest::Client,
    health_limiter: Arc<Semaphore>,
    health_cache: RwLock<HashMap<String, ManagedModelHealthResult>>,
    refresh_lock: Mutex<()>,
    mutation_lock: Mutex<()>,
    free: RwLock<FreeState>,
}

impl ManagedModelService {
    pub fn new(provider_repo: Arc<dyn IProviderRepository>) -> Arc<Self> {
        Self::new_with_client_and_preferences(
            ProviderId::new().into_string(),
            provider_repo,
            None,
            managed_http_client(),
        )
    }

    pub fn new_with_preferences(
        provider_repo: Arc<dyn IProviderRepository>,
        preference_repo: Arc<dyn IClientPreferenceRepository>,
    ) -> Arc<Self> {
        Self::new_with_client_and_preferences(
            ProviderId::new().into_string(),
            provider_repo,
            Some(preference_repo),
            managed_http_client(),
        )
    }

    pub fn new_with_client(
        provider_repo: Arc<dyn IProviderRepository>,
        http_client: reqwest::Client,
    ) -> Arc<Self> {
        Self::new_with_client_and_preferences(
            ProviderId::new().into_string(),
            provider_repo,
            None,
            http_client,
        )
    }

    fn new_with_client_and_preferences(
        provider_id: String,
        provider_repo: Arc<dyn IProviderRepository>,
        preference_repo: Option<Arc<dyn IClientPreferenceRepository>>,
        http_client: reqwest::Client,
    ) -> Arc<Self> {
        Arc::new(Self {
            provider_id,
            provider_repo,
            preference_repo,
            http_client,
            health_limiter: Arc::new(Semaphore::new(FREE_HEALTH_MAX_CONCURRENCY)),
            health_cache: RwLock::new(HashMap::new()),
            refresh_lock: Mutex::new(()),
            mutation_lock: Mutex::new(()),
            free: RwLock::new(FreeState {
                enabled: true,
                catalog: seed_catalog(),
                model_enabled: HashMap::new(),
                last_refresh: None,
                last_error: None,
                automatic_refresh: false,
                refresh_interval: DEFAULT_FREE_REFRESH_INTERVAL,
                next_refresh: None,
            }),
        })
    }

    /// Restore the last successful refresh time and most recent refresh error.
    ///
    /// These are diagnostics rather than provider configuration, so they live
    /// in the generic preference KV store and survive process restarts without
    /// expanding the provider schema.
    pub async fn hydrate_refresh_metadata(&self) -> Result<(), AppError> {
        let Some(repo) = &self.preference_repo else {
            return Ok(());
        };
        let rows = repo
            .get_by_keys(&[FREE_LAST_REFRESH_PREF, FREE_LAST_ERROR_PREF])
            .await
            .map_err(|error| {
                AppError::Internal(format!(
                    "Failed to load managed-model refresh metadata: {error}"
                ))
            })?;
        let mut last_refresh = None;
        let mut last_error = None;
        for row in rows {
            match row.key.as_str() {
                FREE_LAST_REFRESH_PREF => {
                    last_refresh = serde_json::from_str::<i64>(&row.value).ok();
                }
                FREE_LAST_ERROR_PREF => {
                    last_error = serde_json::from_str::<Option<String>>(&row.value)
                        .ok()
                        .flatten()
                        .filter(|message| !message.trim().is_empty());
                }
                _ => {}
            }
        }
        let mut state = self.free.write().await;
        state.last_refresh = last_refresh;
        state.last_error = last_error;
        Ok(())
    }

    /// Hydrate user-controlled flags from an existing managed provider row.
    pub async fn hydrate_from_provider(&self, row: Option<&Provider>) {
        let Some(row) = row else {
            return;
        };
        let persisted_catalog = parse_persisted_catalog(row);
        let persisted_enabled = parse_model_enabled(row.model_enabled.as_deref());

        let mut state = self.free.write().await;
        state.enabled = row.enabled;
        if !persisted_catalog.is_empty() {
            state.catalog = persisted_catalog;
        }
        state.model_enabled = persisted_enabled;
    }

    pub async fn free_status(&self) -> ManagedModelServiceStatus {
        let state = self.free.read().await;
        status_from_free_state(&state, &self.provider_id)
    }

    pub async fn free_models(&self) -> Vec<ManagedModel> {
        self.free_status().await.models
    }

    /// Return the latest in-process health results keyed by model id.
    ///
    /// Health is intentionally diagnostic and ephemeral: a stale result must
    /// never become provider configuration or block inference after restart.
    pub async fn free_health_snapshot(&self) -> Vec<ManagedModelHealthResult> {
        let catalog = self
            .free
            .read()
            .await
            .catalog
            .iter()
            .map(|model| model.id.clone())
            .collect::<Vec<_>>();
        let cache = self.health_cache.read().await;
        catalog
            .into_iter()
            .filter_map(|model_id| cache.get(&model_id).cloned())
            .collect()
    }

    /// Check one model using the same internal adapter as ordinary
    /// `nomifun-free-model` chat traffic.
    ///
    /// A four-token, non-streaming prompt keeps the probe cheap. The result is
    /// normalized before it crosses the public API so upstream URLs, response
    /// bodies and provider names cannot leak through diagnostics.
    pub async fn check_free_model_health(
        self: &Arc<Self>,
        model_id: &str,
    ) -> Result<ManagedModelHealthResult, AppError> {
        self.check_free_model_health_with_slot(model_id, false).await
    }

    async fn check_free_model_health_with_slot(
        self: &Arc<Self>,
        model_id: &str,
        wait_for_slot: bool,
    ) -> Result<ManagedModelHealthResult, AppError> {
        let model_id = model_id.trim();
        if model_id.is_empty() {
            return Err(AppError::BadRequest("model id must not be empty".into()));
        }

        let (service_enabled, model_enabled) = {
            let state = self.free.read().await;
            let model = state
                .catalog
                .iter()
                .find(|model| model.id == model_id)
                .ok_or_else(|| {
                    AppError::NotFound(format!("Managed free model '{model_id}' was not found"))
                })?;
            (
                state.enabled,
                state
                    .model_enabled
                    .get(&model.id)
                    .copied()
                    .unwrap_or(true),
            )
        };

        let immediate = if !service_enabled {
            Some(unknown_health_result(
                model_id,
                ManagedModelHealthErrorKind::ServiceDisabled,
                "Enable the free model service before checking it.",
            ))
        } else if !model_enabled {
            Some(unknown_health_result(
                model_id,
                ManagedModelHealthErrorKind::ModelDisabled,
                "Enable this model before checking it.",
            ))
        } else {
            None
        };
        if let Some(result) = immediate {
            self.cache_health_result(result.clone()).await;
            return Ok(result);
        }

        // Do not queue an unbounded number of real inference calls. A second
        // UI action while all three slots are occupied receives an immediate,
        // retryable "busy" diagnostic rather than extending upstream load.
        let permit = if wait_for_slot {
            // The semaphore is process-owned and never closed before the
            // service is dropped, so this only waits for an active probe.
            self.health_limiter
                .clone()
                .acquire_owned()
                .await
                .expect("managed-model health semaphore remains open")
        } else {
            match self.health_limiter.clone().try_acquire_owned() {
                Ok(permit) => permit,
                Err(_) => {
                    let result = unknown_health_result(
                        model_id,
                        ManagedModelHealthErrorKind::Busy,
                        "Health checks are already running. Try again shortly.",
                    );
                    self.cache_health_result(result.clone()).await;
                    return Ok(result);
                }
            }
        };

        let started = Instant::now();
        let body = json!({
            "model": model_id,
            "messages": [
                {
                    "role": "user",
                    "content": FREE_HEALTH_PROMPT
                }
            ],
            "max_tokens": 4,
            "temperature": 0,
            "stream": false
        });
        let response = tokio::time::timeout(FREE_HEALTH_TIMEOUT, async {
            let response = self.proxy_chat(body).await;
            normalize_health_response(model_id.to_owned(), &started, response).await
        })
        .await;
        drop(permit);

        let result = match response {
            Ok(result) => result,
            Err(_) => unhealthy_health_result(
                model_id,
                Some(duration_millis_u64(started.elapsed())),
                ManagedModelHealthErrorKind::Timeout,
                "The model check timed out. Try again later.",
            ),
        };
        self.cache_health_result(result.clone()).await;
        Ok(result)
    }

    /// Check every enabled model with a hard concurrency ceiling.
    pub async fn check_all_free_model_health(
        self: &Arc<Self>,
    ) -> ManagedModelHealthBatchResult {
        let model_ids = {
            let state = self.free.read().await;
            state
                .catalog
                .iter()
                .filter(|model| {
                    state
                        .model_enabled
                        .get(&model.id)
                        .copied()
                        .unwrap_or(true)
                })
                .map(|model| model.id.clone())
                .collect::<Vec<_>>()
        };
        let mut tasks = JoinSet::new();
        for model_id in model_ids {
            let service = self.clone();
            tasks.spawn(async move {
                service
                    .check_free_model_health_with_slot(&model_id, true)
                    .await
                    .unwrap_or_else(|_| {
                        unhealthy_health_result(
                            &model_id,
                            None,
                            ManagedModelHealthErrorKind::Unknown,
                            "The model could not be checked.",
                        )
                    })
            });
        }

        let mut results = Vec::new();
        while let Some(joined) = tasks.join_next().await {
            if let Ok(result) = joined {
                results.push(result);
            }
        }
        let model_order = self
            .free
            .read()
            .await
            .catalog
            .iter()
            .enumerate()
            .map(|(index, model)| (model.id.clone(), index))
            .collect::<HashMap<_, _>>();
        results.sort_by_key(|result| {
            model_order
                .get(&result.model_id)
                .copied()
                .unwrap_or(usize::MAX)
        });
        health_batch_result(results)
    }

    async fn cache_health_result(&self, result: ManagedModelHealthResult) {
        self.health_cache
            .write()
            .await
            .insert(result.model_id.clone(), result);
    }

    async fn set_refresh_schedule(
        &self,
        automatic_refresh: bool,
        refresh_interval: Duration,
        next_refresh: Option<i64>,
    ) {
        let mut state = self.free.write().await;
        state.automatic_refresh = automatic_refresh;
        state.refresh_interval = refresh_interval;
        state.next_refresh = next_refresh;
    }

    /// Fetch the fixed OpenCode catalog, keep only known-free ids, and project
    /// the result into the provider row. A failed network/catalog refresh
    /// preserves the last usable catalog and returns a degraded status carrying
    /// a non-secret diagnostic, so the UI can display the failure immediately.
    pub async fn refresh_free_models(&self) -> Result<ManagedModelServiceStatus, AppError> {
        // Coalesce catalog refreshes while keeping the long-lived upstream
        // request outside the mutation lock. Service/model toggles therefore
        // stay responsive even if OpenCode discovery is slow.
        let _refresh = self.refresh_lock.lock().await;
        let result = self.fetch_free_catalog().await;
        match result {
            Ok(catalog) => {
                let _mutation = self.mutation_lock.lock().await;
                // Hold the write lock across the DB projection so every mutation
                // is serialized and the in-memory state is committed only after
                // persistence succeeds. The DB call is short and never calls
                // back into this service.
                let mut state = self.free.write().await;
                let mut next = state.clone();
                next.catalog = catalog;
                next.last_refresh = Some(now_ms());
                next.last_error = None;
                // Preserve every existing user choice, including ids that have
                // temporarily disappeared from the live catalog. If an id
                // returns in a later refresh, its prior toggle is restored.
                if let Err(error) = self
                    .sync_provider_projection(next.enabled, &next.catalog, &next.model_enabled)
                    .await
                {
                    let message = refresh_error_message(&error);
                    state.last_error = Some(message);
                    if let Err(persist_error) = self
                        .persist_refresh_metadata(
                            state.last_refresh,
                            state.last_error.as_deref(),
                        )
                        .await
                    {
                        warn!(
                            error = %persist_error,
                            "Failed to persist managed-model projection diagnostic"
                        );
                    }
                    return Ok(status_from_free_state(&state, &self.provider_id));
                }
                *state = next;
                if let Err(error) = self
                    .persist_refresh_metadata(state.last_refresh, None)
                    .await
                {
                    warn!(
                        error = %error,
                        "Failed to persist managed-model refresh diagnostics"
                    );
                }
                Ok(status_from_free_state(&state, &self.provider_id))
            }
            Err(error) => {
                let message = refresh_error_message(&error);
                let mut state = self.free.write().await;
                state.last_error = Some(message);
                if let Err(error) = self
                    .persist_refresh_metadata(state.last_refresh, state.last_error.as_deref())
                    .await
                {
                    warn!(
                        error = %error,
                        "Failed to persist managed-model refresh diagnostics"
                    );
                }
                Ok(status_from_free_state(&state, &self.provider_id))
            }
        }
    }

    async fn persist_refresh_metadata(
        &self,
        last_refresh: Option<i64>,
        last_error: Option<&str>,
    ) -> Result<(), AppError> {
        let Some(repo) = &self.preference_repo else {
            return Ok(());
        };
        let refresh_json = last_refresh
            .map(|value| serde_json::to_string(&value))
            .transpose()
            .map_err(|error| {
                AppError::Internal(format!(
                    "Failed to serialize managed-model refresh time: {error}"
                ))
            })?;
        // Persist JSON null on success instead of deleting the old error in a
        // second operation. `upsert_batch` is transactional, so the successful
        // timestamp and cleared diagnostic cannot be observed independently
        // after a crash or partial storage failure.
        let error_json = serde_json::to_string(&last_error).map_err(|error| {
            AppError::Internal(format!(
                "Failed to serialize managed-model refresh error: {error}"
            ))
        })?;
        let mut upserts = Vec::new();
        if let Some(value) = refresh_json.as_deref() {
            upserts.push((FREE_LAST_REFRESH_PREF, value));
        }
        upserts.push((FREE_LAST_ERROR_PREF, error_json.as_str()));
        if !upserts.is_empty() {
            repo.upsert_batch(&upserts).await.map_err(|error| {
                AppError::Internal(format!(
                    "Failed to persist managed-model refresh metadata: {error}"
                ))
            })?;
        }
        Ok(())
    }

    pub async fn set_free_enabled(
        &self,
        enabled: bool,
    ) -> Result<ManagedModelServiceStatus, AppError> {
        let _mutation = self.mutation_lock.lock().await;
        let mut state = self.free.write().await;
        let mut next = state.clone();
        next.enabled = enabled;
        self.sync_provider_projection(enabled, &next.catalog, &next.model_enabled)
            .await?;
        *state = next;
        Ok(status_from_free_state(&state, &self.provider_id))
    }

    pub async fn set_free_model_enabled(
        &self,
        model_id: &str,
        enabled: bool,
    ) -> Result<ManagedModelServiceStatus, AppError> {
        let _mutation = self.mutation_lock.lock().await;
        let model_id = model_id.trim();
        if model_id.is_empty() {
            return Err(AppError::BadRequest("model id must not be empty".into()));
        }

        let mut state = self.free.write().await;
        if !state.catalog.iter().any(|model| model.id == model_id) {
            return Err(AppError::NotFound(format!(
                "Managed free model '{model_id}' was not found"
            )));
        }
        let mut next = state.clone();
        next.model_enabled.insert(model_id.to_owned(), enabled);
        self.sync_provider_projection(next.enabled, &next.catalog, &next.model_enabled)
            .await?;
        *state = next;
        Ok(status_from_free_state(&state, &self.provider_id))
    }

    async fn fetch_free_catalog(&self) -> Result<Vec<CatalogModel>, AppError> {
        // URL is a compile-time constant and validated defensively so a future
        // refactor cannot accidentally turn discovery into an SSRF primitive.
        validate_fixed_upstream(OPENCODE_MODELS_URL)?;
        let response = self
            .http_client
            .get(OPENCODE_MODELS_URL)
            .bearer_auth("public")
            .header(header::ACCEPT, "application/json")
            .send()
            .await
            .map_err(|_| {
                AppError::BadGateway(
                    "The free model list could not be updated. Try again later.".into(),
                )
            })?;

        let status = response.status();
        if !status.is_success() {
            return Err(AppError::BadGateway(
                "The free model list is temporarily unavailable.".into(),
            ));
        }

        let payload: OpenAiModelList = response.json().await.map_err(|_| {
            AppError::BadGateway("The free model list returned an unreadable response.".into())
        })?;
        let mut seen = HashSet::new();
        let mut models = payload
            .data
            .into_iter()
            .filter(|model| is_free_opencode_model(&model.id))
            .filter_map(|model| {
                let id = model.id.trim().to_owned();
                if id.is_empty() || !seen.insert(id.clone()) {
                    return None;
                }
                let name = model.name.unwrap_or_else(|| id.clone());
                Some(CatalogModel::opencode(id, name))
            })
            .collect::<Vec<_>>();

        if models.is_empty() {
            return Err(AppError::BadGateway(
                "The free model list did not contain any supported models.".into(),
            ));
        }

        // Keep `big-pickle` first as a stable seed/default, then retain upstream
        // order for the remaining free entries.
        models.sort_by_key(|model| if model.id == "big-pickle" { 0 } else { 1 });
        Ok(models)
    }

    async fn sync_provider_projection(
        &self,
        enabled: bool,
        catalog: &[CatalogModel],
        model_enabled: &HashMap<String, bool>,
    ) -> Result<(), AppError> {
        let models_json = serde_json::to_string(
            &catalog
                .iter()
                .map(|model| model.id.as_str())
                .collect::<Vec<_>>(),
        )
        .map_err(|e| AppError::Internal(format!("Failed to serialize managed models: {e}")))?;
        let enabled_json = serde_json::to_string(model_enabled).map_err(|e| {
            AppError::Internal(format!("Failed to serialize managed model flags: {e}"))
        })?;
        let descriptions_json = serde_json::to_string(
            &catalog
                .iter()
                .map(|model| {
                    (
                        model.id.as_str(),
                        "Free model supplied through NomiFun's managed model adapter",
                    )
                })
                .collect::<HashMap<_, _>>(),
        )
        .map_err(|e| {
            AppError::Internal(format!("Failed to serialize managed model descriptions: {e}"))
        })?;

        self.provider_repo
            .update(
                &self.provider_id,
                UpdateProviderParams {
                    platform: Some(FREE_MODEL_PLATFORM),
                    name: Some(FREE_MODEL_PROVIDER_NAME),
                    models: Some(&models_json),
                    enabled: Some(enabled),
                    capabilities: Some("[]"),
                    model_descriptions: Some(Some(&descriptions_json)),
                    model_enabled: Some(Some(&enabled_json)),
                    bedrock_config: Some(None),
                    is_full_url: Some(false),
                    ..Default::default()
                },
            )
            .await?;
        Ok(())
    }

    async fn proxy_chat(&self, mut body: Value) -> Response {
        let Some(model) = body.get("model").and_then(Value::as_str).map(str::to_owned)
        else {
            return openai_error(
                StatusCode::BAD_REQUEST,
                "The 'model' field is required",
                "invalid_request_error",
            );
        };

        let state = self.free.read().await;
        if !state.enabled {
            return openai_error(
                StatusCode::SERVICE_UNAVAILABLE,
                "The NomiFun free model service is disabled",
                "service_unavailable",
            );
        }
        let selected = state
            .catalog
            .iter()
            .find(|candidate| candidate.id == model)
            .filter(|candidate| {
                state
                    .model_enabled
                    .get(&candidate.id)
                    .copied()
                    .unwrap_or(true)
            })
            .map(|candidate| candidate.id.clone());
        drop(state);

        let Some(selected) = selected else {
            return openai_error(
                StatusCode::BAD_REQUEST,
                &format!("Model '{model}' is not an enabled free model"),
                "invalid_request_error",
            );
        };
        body["model"] = Value::String(selected);

        if let Err(error) = validate_fixed_upstream(OPENCODE_CHAT_URL) {
            warn!(error = %error, "Managed free-model upstream validation failed");
            return openai_error(
                StatusCode::BAD_GATEWAY,
                "The free model service is temporarily unavailable",
                "upstream_error",
            );
        }
        let response = match self
            .http_client
            .post(OPENCODE_CHAT_URL)
            .bearer_auth("public")
            .header("x-opencode-client", "desktop")
            .header(header::CONTENT_TYPE, "application/json")
            .header(header::ACCEPT, "text/event-stream, application/json")
            .json(&body)
            .send()
            .await
        {
            Ok(response) => response,
            Err(error) => {
                warn!(error = %error, "Managed free-model chat request failed");
                return openai_error(
                    StatusCode::BAD_GATEWAY,
                    "The free model service is temporarily unavailable",
                    "upstream_error",
                );
            }
        };

        proxy_upstream_response(response).await
    }
}

/// Loopback OpenAI-compatible server kept alive by `AppServices`.
pub struct ManagedModelServer {
    http_addr: SocketAddr,
    auth_token: String,
    shutdown_handle: Option<tokio::task::JoinHandle<()>>,
}

impl ManagedModelServer {
    pub async fn start(service: Arc<ManagedModelService>) -> Result<Self, String> {
        let auth_token = generate_auth_token()?;
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .map_err(|e| format!("Failed to bind managed model listener: {e}"))?;
        let http_addr = listener
            .local_addr()
            .map_err(|e| format!("Failed to read managed model listener address: {e}"))?;
        let state = LoopbackState {
            service,
            auth_token: auth_token.clone(),
        };

        let app = Router::new()
            .route("/v1/models", get(loopback_models))
            .route("/v1/chat/completions", post(loopback_chat))
            .with_state(state);
        let shutdown_handle = tokio::spawn(async move {
            if let Err(error) = axum::serve(listener, app).await {
                warn!(error = %error, "Managed model loopback server exited");
            }
        });

        debug!(
            http_port = http_addr.port(),
            "Managed model loopback server started"
        );
        Ok(Self {
            http_addr,
            auth_token,
            shutdown_handle: Some(shutdown_handle),
        })
    }

    pub fn base_url(&self) -> String {
        format!("http://127.0.0.1:{}/v1", self.http_addr.port())
    }

    pub fn auth_token(&self) -> &str {
        &self.auth_token
    }

    pub fn stop(&mut self) {
        if let Some(handle) = self.shutdown_handle.take() {
            handle.abort();
        }
    }
}

impl Drop for ManagedModelServer {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Scheduling policy for the managed free-model catalog refresher.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ManagedModelRefreshPolicy {
    pub success_interval: Duration,
    pub success_jitter: Duration,
    pub retry_base: Duration,
    pub retry_max: Duration,
}

impl Default for ManagedModelRefreshPolicy {
    fn default() -> Self {
        Self {
            success_interval: DEFAULT_FREE_REFRESH_INTERVAL,
            success_jitter: DEFAULT_FREE_REFRESH_JITTER,
            retry_base: DEFAULT_FREE_REFRESH_RETRY_BASE,
            retry_max: DEFAULT_FREE_REFRESH_RETRY_MAX,
        }
    }
}

impl ManagedModelRefreshPolicy {
    fn normalized(self) -> Self {
        Self {
            success_interval: self.success_interval.max(Duration::from_millis(1)),
            success_jitter: self.success_jitter,
            retry_base: self.retry_base.max(Duration::from_millis(1)),
            retry_max: self.retry_max.max(self.retry_base.max(Duration::from_millis(1))),
        }
    }

    /// Pure delay calculation. `jitter_unit` is clamped to `[0, 1]` and maps
    /// symmetrically onto `[-success_jitter, +success_jitter]`.
    pub fn delay_after_success(self, jitter_unit: f64) -> Duration {
        let policy = self.normalized();
        symmetric_jitter(
            policy.success_interval,
            policy.success_jitter,
            jitter_unit,
        )
    }

    /// Pure capped exponential backoff with full positive jitter.
    ///
    /// Attempt 1 starts at `retry_base`; every additional failure doubles the
    /// base up to `retry_max`. `jitter_unit` adds up to 25% to avoid many
    /// clients retrying in lock-step.
    pub fn delay_after_failure(self, consecutive_failures: u32, jitter_unit: f64) -> Duration {
        let policy = self.normalized();
        let exponent = consecutive_failures.saturating_sub(1).min(31);
        let multiplier = 1u32.checked_shl(exponent).unwrap_or(u32::MAX);
        let base = policy
            .retry_base
            .checked_mul(multiplier)
            .unwrap_or(policy.retry_max)
            .min(policy.retry_max);
        positive_jitter(base, policy.retry_max, jitter_unit)
    }
}

/// Owns the automatic refresh loop. Dropping the handle aborts sleeping or
/// in-flight work immediately, matching the process-owned server lifecycle.
pub struct ManagedModelRefreshTask {
    handle: Option<tokio::task::JoinHandle<()>>,
    service: Arc<ManagedModelService>,
}

type RefreshSuccessHook = Arc<
    dyn Fn(ManagedModelServiceStatus) -> Pin<Box<dyn Future<Output = ()> + Send>> + Send + Sync,
>;

impl ManagedModelRefreshTask {
    /// Start an immediate refresh followed by the default six-hour schedule.
    pub fn start(service: Arc<ManagedModelService>) -> Self {
        Self::start_with_policy(service, ManagedModelRefreshPolicy::default())
    }

    pub fn start_with_policy(
        service: Arc<ManagedModelService>,
        policy: ManagedModelRefreshPolicy,
    ) -> Self {
        Self::start_with_policy_and_hook(service, policy, None)
    }

    /// Start the refresh loop and invoke a lifecycle-bound hook after every
    /// successful catalog refresh. Hooks are serialized with the loop so profile
    /// reconciliation cannot pile up, and dropping this task cancels an in-flight
    /// hook together with the refresh work.
    pub fn start_with_success_hook<F, Fut>(
        service: Arc<ManagedModelService>,
        hook: F,
    ) -> Self
    where
        F: Fn(ManagedModelServiceStatus) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = ()> + Send + 'static,
    {
        Self::start_with_policy_and_hook(
            service,
            ManagedModelRefreshPolicy::default(),
            Some(Arc::new(move |status| Box::pin(hook(status)))),
        )
    }

    fn start_with_policy_and_hook(
        service: Arc<ManagedModelService>,
        policy: ManagedModelRefreshPolicy,
        success_hook: Option<RefreshSuccessHook>,
    ) -> Self {
        Self::start_with_policy_jitter_and_hook(
            service,
            policy,
            random_jitter_unit,
            success_hook,
        )
    }

    #[cfg(test)]
    fn start_with_policy_and_jitter<J>(
        service: Arc<ManagedModelService>,
        policy: ManagedModelRefreshPolicy,
        jitter: J,
    ) -> Self
    where
        J: Fn() -> f64 + Send + Sync + 'static,
    {
        Self::start_with_policy_jitter_and_hook(service, policy, jitter, None)
    }

    fn start_with_policy_jitter_and_hook<J>(
        service: Arc<ManagedModelService>,
        policy: ManagedModelRefreshPolicy,
        jitter: J,
        success_hook: Option<RefreshSuccessHook>,
    ) -> Self
    where
        J: Fn() -> f64 + Send + Sync + 'static,
    {
        let policy = policy.normalized();
        if let Ok(mut state) = service.free.try_write() {
            // Publish the task-owned scheduler contract synchronously so a
            // status request immediately after startup never reports that
            // automatic refresh is disabled merely because the spawned future
            // has not yet been polled.
            state.automatic_refresh = true;
            state.refresh_interval = policy.success_interval;
            state.next_refresh = None;
        }
        let handle = tokio::spawn(run_refresh_loop(
            service.clone(),
            policy,
            jitter,
            success_hook,
        ));
        Self {
            handle: Some(handle),
            service,
        }
    }

    pub fn stop(&mut self) {
        if let Some(handle) = self.handle.take() {
            handle.abort();
        }
        if let Ok(mut state) = self.service.free.try_write() {
            state.automatic_refresh = false;
            state.next_refresh = None;
        }
    }

    #[cfg(test)]
    fn is_finished(&self) -> bool {
        self.handle.as_ref().is_none_or(tokio::task::JoinHandle::is_finished)
    }
}

impl Drop for ManagedModelRefreshTask {
    fn drop(&mut self) {
        self.stop();
    }
}

async fn run_refresh_loop<J>(
    service: Arc<ManagedModelService>,
    policy: ManagedModelRefreshPolicy,
    jitter: J,
    success_hook: Option<RefreshSuccessHook>,
) where
    J: Fn() -> f64 + Send + Sync + 'static,
{
    let mut consecutive_failures = 0u32;
    loop {
        // The public refresh method owns the singleflight mutation lock. Manual
        // and automatic refreshes therefore never commit concurrently.
        let succeeded = match service.refresh_free_models().await {
            Ok(status) if status.last_error.is_none() => {
                consecutive_failures = 0;
                info!(
                    model_count = status.models.len(),
                    "Managed free-model catalog refreshed automatically"
                );
                if let Some(hook) = &success_hook {
                    hook(status).await;
                }
                true
            }
            Ok(status) => {
                consecutive_failures = consecutive_failures.saturating_add(1);
                warn!(
                    error = status.last_error.as_deref().unwrap_or("unknown"),
                    consecutive_failures,
                    "Automatic managed free-model catalog refresh failed"
                );
                false
            }
            Err(error) => {
                consecutive_failures = consecutive_failures.saturating_add(1);
                warn!(
                    error = %error,
                    consecutive_failures,
                    "Automatic managed free-model catalog refresh could not commit"
                );
                false
            }
        };

        let delay = if succeeded {
            policy.delay_after_success(jitter())
        } else {
            policy.delay_after_failure(consecutive_failures, jitter())
        };
        let next_refresh = epoch_ms_after(delay);
        service
            .set_refresh_schedule(true, policy.success_interval, Some(next_refresh))
            .await;
        debug!(
            delay_ms = duration_millis_u64(delay),
            next_refresh,
            consecutive_failures,
            "Scheduled next managed free-model catalog refresh"
        );
        tokio::time::sleep(delay).await;
    }
}

/// Start the loopback supply and create/update its stable provider projection.
///
/// Existing `enabled` and `model_enabled` values are hydrated first and never
/// overwritten by startup defaults.
pub async fn start_and_provision_free_model(
    provider_repo: Arc<dyn IProviderRepository>,
    encryption_key: [u8; 32],
) -> Result<(Arc<ManagedModelService>, ManagedModelServer), AppError> {
    start_and_provision_free_model_with_preferences(provider_repo, None, encryption_key).await
}

/// Provision the managed free-model supply and optionally persist refresh
/// diagnostics in the application's generic preference store.
pub async fn start_and_provision_free_model_with_preferences(
    provider_repo: Arc<dyn IProviderRepository>,
    preference_repo: Option<Arc<dyn IClientPreferenceRepository>>,
    encryption_key: [u8; 32],
) -> Result<(Arc<ManagedModelService>, ManagedModelServer), AppError> {
    let mut managed_rows = provider_repo
        .list()
        .await?
        .into_iter()
        .filter(|row| row.platform.trim().eq_ignore_ascii_case(FREE_MODEL_PLATFORM));
    let existing = managed_rows.next();
    if let Some(alias) = managed_rows.next() {
        return Err(AppError::Conflict(format!(
            "Reserved managed platform '{FREE_MODEL_PLATFORM}' has multiple provider rows (including '{}')",
            alias.id
        )));
    }
    let provider_id = match existing.as_ref() {
        Some(row) => ProviderId::parse(&row.id)
            .map_err(|error| {
                AppError::Conflict(format!(
                    "Managed free-model provider has a non-canonical id '{}': {error}",
                    row.id
                ))
            })?
            .into_string(),
        None => ProviderId::new().into_string(),
    };

    let service = match preference_repo {
        Some(repo) => ManagedModelService::new_with_client_and_preferences(
            provider_id.clone(),
            provider_repo.clone(),
            Some(repo),
            managed_http_client(),
        ),
        None => ManagedModelService::new_with_client_and_preferences(
            provider_id.clone(),
            provider_repo.clone(),
            None,
            managed_http_client(),
        ),
    };
    service.hydrate_from_provider(existing.as_ref()).await;
    if let Err(error) = service.hydrate_refresh_metadata().await {
        // Refresh timestamps/errors are diagnostics only. A corrupt or
        // temporarily unavailable preference store must not prevent the
        // built-in seed catalog and loopback model supply from starting.
        warn!(
            error = %error,
            "Failed to restore managed-model refresh diagnostics; continuing with defaults"
        );
    }
    let server = ManagedModelServer::start(service.clone())
        .await
        .map_err(AppError::Internal)?;

    let token_encrypted = encrypt_string(server.auth_token(), &encryption_key)?;
    let base_url = server.base_url();
    let (status, model_enabled) = {
        let state = service.free.read().await;
        (
            status_from_free_state(&state, &service.provider_id),
            state.model_enabled.clone(),
        )
    };
    let models_json = serde_json::to_string(
        &status
            .models
            .iter()
            .map(|model| model.id.as_str())
            .collect::<Vec<_>>(),
    )
    .map_err(|e| AppError::Internal(format!("Failed to serialize startup model catalog: {e}")))?;
    let enabled_json = serde_json::to_string(&model_enabled).map_err(|e| {
        AppError::Internal(format!("Failed to serialize startup managed model flags: {e}"))
    })?;
    let descriptions_json = serde_json::to_string(
        &status
            .models
            .iter()
            .map(|model| {
                (
                    model.id.as_str(),
                    "Free model supplied through NomiFun's managed model adapter",
                )
            })
            .collect::<HashMap<_, _>>(),
    )
    .map_err(|e| {
        AppError::Internal(format!("Failed to serialize startup model descriptions: {e}"))
    })?;

    match existing {
        Some(_) => {
            provider_repo
                .update(
                    &provider_id,
                    UpdateProviderParams {
                        platform: Some(FREE_MODEL_PLATFORM),
                        name: Some(FREE_MODEL_PROVIDER_NAME),
                        base_url: Some(&base_url),
                        api_key_encrypted: Some(&token_encrypted),
                        models: Some(&models_json),
                        enabled: Some(status.enabled),
                        capabilities: Some("[]"),
                        model_descriptions: Some(Some(&descriptions_json)),
                        model_enabled: Some(Some(&enabled_json)),
                        bedrock_config: Some(None),
                        is_full_url: Some(false),
                        ..Default::default()
                    },
                )
                .await?;
        }
        None => {
            provider_repo
                .create(CreateProviderParams {
                    id: Some(&provider_id),
                    platform: FREE_MODEL_PLATFORM,
                    name: FREE_MODEL_PROVIDER_NAME,
                    base_url: &base_url,
                    api_key_encrypted: &token_encrypted,
                    models: &models_json,
                    enabled: status.enabled,
                    capabilities: "[]",
                    context_limit: None,
                    model_context_limits: None,
                    model_protocols: None,
                    model_descriptions: Some(&descriptions_json),
                    model_enabled: Some(&enabled_json),
                    model_health: None,
                    bedrock_config: None,
                    is_full_url: false,
                    sort_order: None,
                })
                .await?;
        }
    }

    Ok((service, server))
}

async fn loopback_models(
    State(state): State<LoopbackState>,
    headers: HeaderMap,
) -> Response {
    if !authorized(&headers, &state.auth_token) {
        return openai_error(StatusCode::UNAUTHORIZED, "Unauthorized", "authentication_error");
    }
    let status = state.service.free_status().await;
    if !status.enabled {
        return openai_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "The NomiFun free model service is disabled",
            "service_unavailable",
        );
    }
    Json(json!({
        "object": "list",
        "data": status.models.into_iter()
            .filter(|model| model.enabled)
            .map(|model| json!({
                "id": model.id,
                "object": "model",
                "created": 0,
                "owned_by": FREE_MODEL_PLATFORM
            }))
            .collect::<Vec<_>>()
    }))
    .into_response()
}

async fn loopback_chat(
    State(state): State<LoopbackState>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    if !authorized(&headers, &state.auth_token) {
        return openai_error(StatusCode::UNAUTHORIZED, "Unauthorized", "authentication_error");
    }
    state.service.proxy_chat(body).await
}

async fn proxy_upstream_response(response: reqwest::Response) -> Response {
    let upstream_status = response.status();
    let status = StatusCode::from_u16(upstream_status.as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
    if !status.is_success() {
        warn!(
            http_status = status.as_u16(),
            "Managed free-model upstream returned an error"
        );
        let (message, kind) = match status {
            StatusCode::TOO_MANY_REQUESTS => (
                "The free model is busy. Try again later.",
                "rate_limit_error",
            ),
            StatusCode::BAD_REQUEST | StatusCode::NOT_FOUND => (
                "The selected free model is currently unavailable.",
                "model_unavailable",
            ),
            StatusCode::REQUEST_TIMEOUT | StatusCode::GATEWAY_TIMEOUT => (
                "The free model request timed out. Try again later.",
                "timeout_error",
            ),
            _ => (
                "The free model service is temporarily unavailable.",
                "upstream_error",
            ),
        };
        return openai_error(status, message, kind);
    }

    let upstream_headers = response.headers().clone();
    let mut builder = Response::builder().status(status);

    for name in [
        header::CONTENT_TYPE,
        header::CACHE_CONTROL,
        header::RETRY_AFTER,
        header::CONTENT_ENCODING,
    ] {
        if let Some(value) = upstream_headers.get(&name) {
            builder = builder.header(name, value);
        }
    }
    // Common reverse-proxy headers that help browsers/stream readers avoid
    // buffering, without reflecting arbitrary upstream headers.
    builder = builder.header(
        HeaderName::from_static("x-accel-buffering"),
        HeaderValue::from_static("no"),
    );

    builder
        .body(Body::from_stream(response.bytes_stream()))
        .unwrap_or_else(|_| {
            openai_error(
                StatusCode::BAD_GATEWAY,
                "Failed to build upstream response",
                "upstream_error",
            )
        })
}

fn authorized(headers: &HeaderMap, token: &str) -> bool {
    headers
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
        .is_some_and(|provided| provided == token)
}

fn openai_error(status: StatusCode, message: &str, kind: &str) -> Response {
    (
        status,
        Json(json!({
            "error": {
                "message": message,
                "type": kind,
                "param": Value::Null,
                "code": Value::Null
            }
        })),
    )
        .into_response()
}

fn unknown_health_result(
    model_id: &str,
    error_kind: ManagedModelHealthErrorKind,
    message: &str,
) -> ManagedModelHealthResult {
    ManagedModelHealthResult {
        model_id: model_id.to_owned(),
        status: ManagedModelHealthStatus::Unknown,
        latency_ms: None,
        checked_at: now_ms(),
        error_kind: Some(error_kind),
        message: Some(message.to_owned()),
    }
}

fn unhealthy_health_result(
    model_id: &str,
    latency_ms: Option<u64>,
    error_kind: ManagedModelHealthErrorKind,
    message: &str,
) -> ManagedModelHealthResult {
    ManagedModelHealthResult {
        model_id: model_id.to_owned(),
        status: ManagedModelHealthStatus::Unhealthy,
        latency_ms,
        checked_at: now_ms(),
        error_kind: Some(error_kind),
        message: Some(message.to_owned()),
    }
}

async fn normalize_health_response(
    model_id: String,
    started: &Instant,
    response: Response,
) -> ManagedModelHealthResult {
    let status = response.status();
    if !status.is_success() {
        let (error_kind, message) = match status {
            StatusCode::TOO_MANY_REQUESTS => (
                ManagedModelHealthErrorKind::Unavailable,
                "The free model is busy. Try again later.",
            ),
            StatusCode::BAD_REQUEST
            | StatusCode::NOT_FOUND
            | StatusCode::UNAUTHORIZED
            | StatusCode::FORBIDDEN => (
                ManagedModelHealthErrorKind::Unavailable,
                "This model is currently unavailable.",
            ),
            StatusCode::REQUEST_TIMEOUT | StatusCode::GATEWAY_TIMEOUT => (
                ManagedModelHealthErrorKind::Timeout,
                "The model check timed out. Try again later.",
            ),
            _ => (
                ManagedModelHealthErrorKind::Unavailable,
                "The free model service is temporarily unavailable.",
            ),
        };
        return unhealthy_health_result(
            &model_id,
            Some(duration_millis_u64(started.elapsed())),
            error_kind,
            message,
        );
    }

    let content_type_is_json = response
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.to_ascii_lowercase().contains("application/json"));
    if !content_type_is_json {
        return unhealthy_health_result(
            &model_id,
            Some(duration_millis_u64(started.elapsed())),
            ManagedModelHealthErrorKind::InvalidResponse,
            "The model returned an unreadable response.",
        );
    }

    let bytes = match axum::body::to_bytes(response.into_body(), 256 * 1024).await {
        Ok(bytes) => bytes,
        Err(_) => {
            return unhealthy_health_result(
                &model_id,
                Some(duration_millis_u64(started.elapsed())),
                ManagedModelHealthErrorKind::InvalidResponse,
                "The model returned an unreadable response.",
            );
        }
    };
    let valid = serde_json::from_slice::<Value>(&bytes)
        .ok()
        .and_then(|payload| payload.get("choices").and_then(Value::as_array).cloned())
        .is_some_and(|choices| !choices.is_empty());
    if !valid {
        return unhealthy_health_result(
            &model_id,
            Some(duration_millis_u64(started.elapsed())),
            ManagedModelHealthErrorKind::InvalidResponse,
            "The model returned an unexpected response.",
        );
    }

    ManagedModelHealthResult {
        model_id,
        status: ManagedModelHealthStatus::Healthy,
        latency_ms: Some(duration_millis_u64(started.elapsed())),
        checked_at: now_ms(),
        error_kind: None,
        message: None,
    }
}

fn health_batch_result(results: Vec<ManagedModelHealthResult>) -> ManagedModelHealthBatchResult {
    let total = results.len();
    let healthy = results
        .iter()
        .filter(|result| result.status == ManagedModelHealthStatus::Healthy)
        .count();
    let unhealthy = results
        .iter()
        .filter(|result| result.status == ManagedModelHealthStatus::Unhealthy)
        .count();
    let unknown = total.saturating_sub(healthy).saturating_sub(unhealthy);
    ManagedModelHealthBatchResult {
        results,
        total,
        healthy,
        unhealthy,
        unknown,
    }
}

fn seed_catalog() -> Vec<CatalogModel> {
    FREE_SEED_MODELS
        .iter()
        .map(|id| CatalogModel::opencode(*id, *id))
        .collect()
}

fn parse_persisted_catalog(row: &Provider) -> Vec<CatalogModel> {
    serde_json::from_str::<Vec<String>>(&row.models)
        .unwrap_or_default()
        .into_iter()
        .map(|id| CatalogModel::opencode(id.clone(), id))
        .collect()
}

fn parse_model_enabled(raw: Option<&str>) -> HashMap<String, bool> {
    raw.and_then(|value| serde_json::from_str(value).ok())
        .unwrap_or_default()
}

fn status_from_free_state(state: &FreeState, provider_id: &str) -> ManagedModelServiceStatus {
    let models = state
        .catalog
        .iter()
        .map(|model| ManagedModel {
            id: model.id.clone(),
            name: model.name.clone(),
            enabled: state
                .model_enabled
                .get(&model.id)
                .copied()
                .unwrap_or(true),
            // Never trust persisted/source-adapter labels at the public
            // boundary. The stable alias avoids revealing implementation
            // details even when hydrating rows written by an older build.
            source: PUBLIC_SOURCE_ALIAS.into(),
        })
        .collect::<Vec<_>>();
    // `ready` means the in-process loopback supply has an enabled catalog entry. The
    // separate availability enum stays `Unverified` until a live discovery has
    // succeeded; even then it deliberately does not promise a third-party SLA.
    let ready = state.enabled && models.iter().any(|model| model.enabled);
    ManagedModelServiceStatus {
        protocol_version: MANAGED_MODEL_PROTOCOL_VERSION.into(),
        provider_id: Some(provider_id.to_owned()),
        enabled: state.enabled,
        ready,
        upstream: PUBLIC_SOURCE_ALIAS.into(),
        models,
        last_refresh: state.last_refresh,
        automatic_refresh: state.automatic_refresh,
        refresh_interval_ms: duration_millis_u64(state.refresh_interval),
            next_refresh: state.next_refresh,
            last_error: state
                .last_error
                .as_deref()
                .map(sanitize_public_diagnostic),
        privacy_notice: FREE_PRIVACY_NOTICE.into(),
        availability: if !ready || state.last_error.is_some() {
            ManagedModelServiceAvailability::Degraded
        } else if state.last_refresh.is_none() {
            ManagedModelServiceAvailability::Unverified
        } else {
            ManagedModelServiceAvailability::Ready
        },
    }
}

fn duration_millis_u64(duration: Duration) -> u64 {
    duration.as_millis().min(u128::from(u64::MAX)) as u64
}

fn duration_from_millis_saturating(milliseconds: f64) -> Duration {
    Duration::from_millis(milliseconds.max(1.0).min(u64::MAX as f64).round() as u64)
}

fn normalized_jitter_unit(value: f64) -> f64 {
    if value.is_finite() {
        value.clamp(0.0, 1.0)
    } else {
        0.5
    }
}

fn symmetric_jitter(base: Duration, max_jitter: Duration, jitter_unit: f64) -> Duration {
    let base_ms = duration_millis_u64(base) as f64;
    let jitter_ms = duration_millis_u64(max_jitter) as f64;
    let centered = normalized_jitter_unit(jitter_unit).mul_add(2.0, -1.0);
    duration_from_millis_saturating(base_ms + centered * jitter_ms)
}

fn positive_jitter(base: Duration, cap: Duration, jitter_unit: f64) -> Duration {
    let base_ms = duration_millis_u64(base) as f64;
    let cap_ms = duration_millis_u64(cap) as f64;
    let extra = base_ms * 0.25 * normalized_jitter_unit(jitter_unit);
    duration_from_millis_saturating((base_ms + extra).min(cap_ms))
}

fn epoch_ms_after(delay: Duration) -> i64 {
    let delay_ms = delay.as_millis().min(i64::MAX as u128) as i64;
    now_ms().saturating_add(delay_ms)
}

fn random_jitter_unit() -> f64 {
    let mut bytes = [0u8; 8];
    if getrandom::getrandom(&mut bytes).is_err() {
        return 0.5;
    }
    (u64::from_le_bytes(bytes) as f64) / (u64::MAX as f64)
}

fn managed_http_client() -> reqwest::Client {
    let build = || {
        reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(15))
            .read_timeout(Duration::from_secs(120))
            // The allowlist applies to every request destination. Do not let an
            // upstream redirect forward a catalog request or user prompt to a
            // different host without an explicit code change and review.
            .redirect(reqwest::redirect::Policy::none())
    };
    nomifun_net::proxy::apply_detected_proxy(build())
        .build()
        .unwrap_or_else(|_| {
            build()
                .build()
                .expect("managed model HTTP client configuration is valid")
        })
}

fn generate_auth_token() -> Result<String, String> {
    let mut bytes = [0u8; 32];
    getrandom::getrandom(&mut bytes)
        .map_err(|error| format!("Failed to generate managed-model auth token: {error}"))?;
    let mut token = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        write!(token, "{byte:02x}")
            .map_err(|error| format!("Failed to encode managed-model auth token: {error}"))?;
    }
    Ok(token)
}

fn is_free_opencode_model(id: &str) -> bool {
    id.ends_with("-free") || id == "big-pickle"
}

fn validate_fixed_upstream(raw: &str) -> Result<(), AppError> {
    let url = Url::parse(raw)
        .map_err(|e| AppError::Internal(format!("Invalid built-in OpenCode URL: {e}")))?;
    if url.scheme() != "https" || url.host_str() != Some("opencode.ai") {
        return Err(AppError::Internal(
            "Built-in managed model upstream is outside the allowlist".into(),
        ));
    }
    Ok(())
}

fn refresh_error_message(error: &AppError) -> String {
    match error {
        AppError::BadGateway(message)
        | AppError::Timeout(message)
        | AppError::Internal(message) => sanitize_public_diagnostic(message),
        _ => error.to_string(),
    }
}

fn sanitize_public_diagnostic(message: &str) -> String {
    let lower = message.to_ascii_lowercase();
    if lower.contains("opencode")
        || lower.contains("zen")
        || lower.contains("http://")
        || lower.contains("https://")
    {
        "The free model service is temporarily unavailable. Try again later.".into()
    } else {
        message.to_owned()
    }
}

#[derive(Debug, Deserialize)]
struct OpenAiModelList {
    #[serde(default)]
    data: Vec<OpenAiModel>,
}

#[derive(Debug, Deserialize)]
struct OpenAiModel {
    id: String,
    #[serde(default)]
    name: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::to_bytes;
    use nomifun_common::decrypt_string;
    use nomifun_db::{
        IClientPreferenceRepository, SqliteClientPreferenceRepository,
        SqliteProviderRepository, init_database_memory,
    };

    const TEST_KEY: [u8; 32] = [0x42; 32];

    #[test]
    fn free_filter_matches_opencode_contract() {
        assert!(is_free_opencode_model("big-pickle"));
        assert!(is_free_opencode_model("deepseek-v4-flash-free"));
        assert!(!is_free_opencode_model("gpt-5.4"));
        assert!(!is_free_opencode_model("not-free-tier"));
    }

    #[test]
    fn fixed_upstream_allowlist_is_https_opencode_only() {
        assert!(validate_fixed_upstream(OPENCODE_MODELS_URL).is_ok());
        assert!(validate_fixed_upstream("http://opencode.ai/zen/v1/models").is_err());
        assert!(validate_fixed_upstream("https://example.com/zen/v1/models").is_err());
    }

    #[test]
    fn auth_token_uses_256_bits_of_os_randomness() {
        let first = generate_auth_token().unwrap();
        let second = generate_auth_token().unwrap();
        assert_eq!(first.len(), 64);
        assert!(first.chars().all(|ch| ch.is_ascii_hexdigit()));
        assert_ne!(first, second);
    }

    #[test]
    fn refresh_policy_success_jitter_is_symmetric_and_bounded() {
        let policy = ManagedModelRefreshPolicy {
            success_interval: Duration::from_secs(100),
            success_jitter: Duration::from_secs(10),
            retry_base: Duration::from_secs(5),
            retry_max: Duration::from_secs(60),
        };
        assert_eq!(policy.delay_after_success(0.0), Duration::from_secs(90));
        assert_eq!(policy.delay_after_success(0.5), Duration::from_secs(100));
        assert_eq!(policy.delay_after_success(1.0), Duration::from_secs(110));
        assert_eq!(policy.delay_after_success(-3.0), Duration::from_secs(90));
        assert_eq!(policy.delay_after_success(3.0), Duration::from_secs(110));
    }

    #[test]
    fn refresh_policy_failure_backoff_doubles_and_caps() {
        let policy = ManagedModelRefreshPolicy {
            success_interval: Duration::from_secs(100),
            success_jitter: Duration::ZERO,
            retry_base: Duration::from_secs(5),
            retry_max: Duration::from_secs(40),
        };
        assert_eq!(
            policy.delay_after_failure(1, 0.0),
            Duration::from_secs(5)
        );
        assert_eq!(
            policy.delay_after_failure(2, 0.0),
            Duration::from_secs(10)
        );
        assert_eq!(
            policy.delay_after_failure(3, 0.0),
            Duration::from_secs(20)
        );
        assert_eq!(
            policy.delay_after_failure(4, 0.0),
            Duration::from_secs(40)
        );
        assert_eq!(
            policy.delay_after_failure(20, 1.0),
            Duration::from_secs(40)
        );
        assert_eq!(
            policy.delay_after_failure(1, 1.0),
            Duration::from_millis(6_250)
        );
    }

    #[tokio::test]
    async fn provision_creates_stable_provider_and_hides_runtime_in_service_status() {
        let db = init_database_memory().await.unwrap();
        let repo: Arc<dyn IProviderRepository> =
            Arc::new(SqliteProviderRepository::new(db.pool().clone()));
        let (service, mut server) =
            start_and_provision_free_model(repo.clone(), TEST_KEY).await.unwrap();

        let provider_id = service.free_status().await.provider_id.unwrap();
        let row = repo.find_by_id(&provider_id).await.unwrap().unwrap();
        assert_eq!(row.platform, FREE_MODEL_PLATFORM);
        assert!(row.base_url.starts_with("http://127.0.0.1:"));
        assert!(row.base_url.ends_with("/v1"));
        assert_eq!(
            decrypt_string(&row.api_key_encrypted, &TEST_KEY).unwrap(),
            server.auth_token()
        );
        assert!(row.enabled);
        assert!(service.free_status().await.ready);
        server.stop();
    }

    #[tokio::test]
    async fn reprovision_preserves_user_enable_flags() {
        let db = init_database_memory().await.unwrap();
        let repo: Arc<dyn IProviderRepository> =
            Arc::new(SqliteProviderRepository::new(db.pool().clone()));
        let (first_service, mut first_server) =
            start_and_provision_free_model(repo.clone(), TEST_KEY).await.unwrap();
        let provider_id = first_service.free_status().await.provider_id.unwrap();
        first_server.stop();
        repo.update(
            &provider_id,
            UpdateProviderParams {
                enabled: Some(false),
                model_enabled: Some(Some(r#"{"big-pickle":false}"#)),
                ..Default::default()
            },
        )
        .await
        .unwrap();

        let (service, mut second_server) =
            start_and_provision_free_model(repo.clone(), TEST_KEY).await.unwrap();
        let status = service.free_status().await;
        assert!(!status.enabled);
        assert_eq!(
            status
                .models
                .iter()
                .find(|model| model.id == "big-pickle")
                .map(|model| model.enabled),
            Some(false)
        );
        let row = repo.find_by_id(&provider_id).await.unwrap().unwrap();
        assert!(!row.enabled);
        second_server.stop();
    }

    #[tokio::test]
    async fn reprovision_preserves_toggle_for_temporarily_missing_model() {
        let db = init_database_memory().await.unwrap();
        let repo: Arc<dyn IProviderRepository> =
            Arc::new(SqliteProviderRepository::new(db.pool().clone()));
        let (first_service, mut first_server) =
            start_and_provision_free_model(repo.clone(), TEST_KEY).await.unwrap();
        let provider_id = first_service.free_status().await.provider_id.unwrap();
        first_server.stop();
        repo.update(
            &provider_id,
            UpdateProviderParams {
                models: Some(r#"["big-pickle"]"#),
                model_enabled: Some(Some(r#"{"temporarily-missing-free":false}"#)),
                ..Default::default()
            },
        )
        .await
        .unwrap();

        let (_service, mut second_server) =
            start_and_provision_free_model(repo.clone(), TEST_KEY).await.unwrap();
        let row = repo.find_by_id(&provider_id).await.unwrap().unwrap();
        let flags: HashMap<String, bool> =
            serde_json::from_str(row.model_enabled.as_deref().unwrap()).unwrap();
        assert_eq!(flags.get("temporarily-missing-free"), Some(&false));
        second_server.stop();
    }

    #[tokio::test]
    async fn provision_rejects_noncanonical_reserved_platform_alias() {
        let db = init_database_memory().await.unwrap();
        let repo: Arc<dyn IProviderRepository> =
            Arc::new(SqliteProviderRepository::new(db.pool().clone()));
        let encrypted = encrypt_string("legacy-token", &TEST_KEY).unwrap();
        repo.create(CreateProviderParams {
            id: Some("legacy-managed-alias"),
            platform: " NOMIFUN-FREE-MODEL ",
            name: "Legacy managed alias",
            base_url: "http://127.0.0.1:12345/v1",
            api_key_encrypted: &encrypted,
            models: r#"["big-pickle"]"#,
            enabled: true,
            capabilities: "[]",
            context_limit: None,
            model_context_limits: None,
            model_protocols: None,
            model_descriptions: None,
            model_enabled: None,
            model_health: None,
            bedrock_config: None,
            is_full_url: false,
            sort_order: None,
        })
        .await
        .unwrap();

        let error = match start_and_provision_free_model(repo.clone(), TEST_KEY).await {
            Ok(_) => panic!("noncanonical managed platform alias must be rejected"),
            Err(error) => error,
        };
        assert!(matches!(error, AppError::Conflict(_)));
        assert!(
            repo.list().await.unwrap().iter().all(|row| row.platform != FREE_MODEL_PLATFORM)
        );
    }

    #[tokio::test]
    async fn persisted_description_does_not_replace_model_display_name() {
        let db = init_database_memory().await.unwrap();
        let repo: Arc<dyn IProviderRepository> =
            Arc::new(SqliteProviderRepository::new(db.pool().clone()));
        let encrypted = encrypt_string("old-token", &TEST_KEY).unwrap();
        let provider_id = ProviderId::new().into_string();
        repo.create(CreateProviderParams {
            id: Some(&provider_id),
            platform: FREE_MODEL_PLATFORM,
            name: FREE_MODEL_PROVIDER_NAME,
            base_url: "http://127.0.0.1:1/v1",
            api_key_encrypted: &encrypted,
            models: r#"["big-pickle"]"#,
            enabled: true,
            capabilities: "[]",
            context_limit: None,
            model_context_limits: None,
            model_protocols: None,
            model_descriptions: Some(r#"{"big-pickle":"A long capability description"}"#),
            model_enabled: None,
            model_health: None,
            bedrock_config: None,
            is_full_url: false,
            sort_order: None,
        })
        .await
        .unwrap();

        let service = ManagedModelService::new(repo.clone());
        let row = repo.find_by_id(&provider_id).await.unwrap();
        service.hydrate_from_provider(row.as_ref()).await;
        let status = service.free_status().await;
        assert_eq!(status.models[0].name, "big-pickle");
    }

    #[tokio::test]
    async fn refresh_failure_returns_degraded_status_with_diagnostic() {
        let db = init_database_memory().await.unwrap();
        let repo: Arc<dyn IProviderRepository> =
            Arc::new(SqliteProviderRepository::new(db.pool().clone()));
        let client = reqwest::Client::builder()
            .proxy(reqwest::Proxy::all("http://127.0.0.1:9").unwrap())
            .connect_timeout(Duration::from_millis(100))
            .build()
            .unwrap();
        let service = ManagedModelService::new_with_client(repo, client);

        let status = service.refresh_free_models().await.unwrap();
        assert!(status.ready, "seed catalog remains locally ready");
        assert_eq!(
            status.availability,
            ManagedModelServiceAvailability::Degraded
        );
        assert!(status.last_error.is_some());
        assert_eq!(status.last_refresh, None);
    }

    #[tokio::test]
    async fn disabled_model_health_check_is_unknown_without_network() {
        let db = init_database_memory().await.unwrap();
        let repo: Arc<dyn IProviderRepository> =
            Arc::new(SqliteProviderRepository::new(db.pool().clone()));
        let service = ManagedModelService::new(repo);
        {
            let mut state = service.free.write().await;
            state.model_enabled.insert("big-pickle".into(), false);
        }

        let result = service
            .check_free_model_health("big-pickle")
            .await
            .unwrap();
        assert_eq!(result.status, ManagedModelHealthStatus::Unknown);
        assert_eq!(
            result.error_kind,
            Some(ManagedModelHealthErrorKind::ModelDisabled)
        );
        assert_eq!(service.free_health_snapshot().await, vec![result]);
    }

    #[tokio::test]
    async fn health_response_requires_valid_openai_json_and_never_exposes_body() {
        let valid = (
            StatusCode::OK,
            [(header::CONTENT_TYPE, "application/json")],
            Json(json!({"choices": [{"message": {"content": "OK"}}]})),
        )
            .into_response();
        let started = Instant::now();
        let healthy = normalize_health_response("model-a".into(), &started, valid).await;
        assert_eq!(healthy.status, ManagedModelHealthStatus::Healthy);
        assert_eq!(healthy.error_kind, None);

        let invalid = (
            StatusCode::OK,
            [(header::CONTENT_TYPE, "application/json")],
            Json(json!({
                "vendor": "OpenCode Zen",
                "url": "https://opencode.ai/private"
            })),
        )
            .into_response();
        let unhealthy = normalize_health_response("model-a".into(), &started, invalid).await;
        assert_eq!(unhealthy.status, ManagedModelHealthStatus::Unhealthy);
        assert_eq!(
            unhealthy.error_kind,
            Some(ManagedModelHealthErrorKind::InvalidResponse)
        );
        let message = unhealthy.message.unwrap();
        assert!(!message.to_ascii_lowercase().contains("opencode"));
        assert!(!message.contains("https://"));
    }

    #[test]
    fn public_status_and_diagnostics_hide_real_upstream_name() {
        let mut state = FreeState {
            enabled: true,
            catalog: vec![CatalogModel {
                id: "model-a".into(),
                name: "Model A".into(),
            }],
            model_enabled: HashMap::new(),
            last_refresh: None,
            last_error: Some(
                "OpenCode request to https://opencode.ai/zen failed".into(),
            ),
            automatic_refresh: false,
            refresh_interval: DEFAULT_FREE_REFRESH_INTERVAL,
            next_refresh: None,
        };
        let provider_id = ProviderId::new().into_string();
        let status = status_from_free_state(&state, &provider_id);
        assert_eq!(status.upstream, "oc");
        assert_eq!(status.models[0].source, "oc");
        assert!(!status.privacy_notice.to_ascii_lowercase().contains("opencode"));
        assert!(
            !status
                .last_error
                .as_deref()
                .unwrap()
                .to_ascii_lowercase()
                .contains("opencode")
        );
        state.last_error = None;
        let provider_id = ProviderId::new().into_string();
        assert_eq!(status_from_free_state(&state, &provider_id).models[0].source, "oc");
    }

    #[test]
    fn health_batch_counts_each_state() {
        let result = health_batch_result(vec![
            ManagedModelHealthResult {
                model_id: "healthy".into(),
                status: ManagedModelHealthStatus::Healthy,
                latency_ms: Some(1),
                checked_at: 1,
                error_kind: None,
                message: None,
            },
            unhealthy_health_result(
                "unhealthy",
                Some(2),
                ManagedModelHealthErrorKind::Unavailable,
                "Unavailable",
            ),
            unknown_health_result(
                "unknown",
                ManagedModelHealthErrorKind::ModelDisabled,
                "Disabled",
            ),
        ]);
        assert_eq!(result.total, 3);
        assert_eq!(result.healthy, 1);
        assert_eq!(result.unhealthy, 1);
        assert_eq!(result.unknown, 1);
    }

    #[tokio::test]
    async fn seed_catalog_is_ready_but_not_claimed_live_before_refresh() {
        let db = init_database_memory().await.unwrap();
        let repo: Arc<dyn IProviderRepository> =
            Arc::new(SqliteProviderRepository::new(db.pool().clone()));
        let status = ManagedModelService::new(repo).free_status().await;
        assert!(status.ready);
        assert_eq!(
            status.availability,
            ManagedModelServiceAvailability::Unverified
        );
    }

    #[tokio::test]
    async fn refresh_task_runs_immediately_schedules_retry_and_drop_cancels() {
        let db = init_database_memory().await.unwrap();
        let repo: Arc<dyn IProviderRepository> =
            Arc::new(SqliteProviderRepository::new(db.pool().clone()));
        let client = reqwest::Client::builder()
            .proxy(reqwest::Proxy::all("http://127.0.0.1:9").unwrap())
            .connect_timeout(Duration::from_millis(20))
            .build()
            .unwrap();
        let service = ManagedModelService::new_with_client(repo, client);
        let policy = ManagedModelRefreshPolicy {
            success_interval: Duration::from_millis(200),
            success_jitter: Duration::ZERO,
            retry_base: Duration::from_millis(100),
            retry_max: Duration::from_millis(100),
        };
        let mut task =
            ManagedModelRefreshTask::start_with_policy_and_jitter(service.clone(), policy, || 0.0);
        let starting = service.free_status().await;
        assert!(
            starting.automatic_refresh,
            "scheduler state must be visible immediately after task start"
        );
        assert_eq!(starting.refresh_interval_ms, 200);

        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                let status = service.free_status().await;
                if status.automatic_refresh
                    && status.last_error.is_some()
                    && status.next_refresh.is_some()
                {
                    assert_eq!(status.refresh_interval_ms, 200);
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();

        assert!(!task.is_finished());
        task.stop();
        tokio::task::yield_now().await;
        assert!(task.is_finished());
    }

    #[tokio::test]
    async fn refresh_metadata_survives_service_recreation() {
        let db = init_database_memory().await.unwrap();
        let provider_repo: Arc<dyn IProviderRepository> =
            Arc::new(SqliteProviderRepository::new(db.pool().clone()));
        let preference_repo: Arc<dyn IClientPreferenceRepository> =
            Arc::new(SqliteClientPreferenceRepository::new(db.pool().clone()));
        preference_repo
            .upsert_batch(&[
                (FREE_LAST_REFRESH_PREF, "1700000000000"),
                (FREE_LAST_ERROR_PREF, r#""temporary outage""#),
            ])
            .await
            .unwrap();

        let service =
            ManagedModelService::new_with_preferences(provider_repo, preference_repo);
        service.hydrate_refresh_metadata().await.unwrap();
        let status = service.free_status().await;
        assert_eq!(status.last_refresh, Some(1_700_000_000_000));
        assert_eq!(status.last_error.as_deref(), Some("temporary outage"));
        assert_eq!(
            status.availability,
            ManagedModelServiceAvailability::Degraded
        );
    }

    #[tokio::test]
    async fn successful_metadata_persist_clears_prior_error() {
        let db = init_database_memory().await.unwrap();
        let provider_repo: Arc<dyn IProviderRepository> =
            Arc::new(SqliteProviderRepository::new(db.pool().clone()));
        let preference_repo: Arc<dyn IClientPreferenceRepository> =
            Arc::new(SqliteClientPreferenceRepository::new(db.pool().clone()));
        preference_repo
            .upsert_batch(&[(FREE_LAST_ERROR_PREF, r#""old error""#)])
            .await
            .unwrap();
        let service = ManagedModelService::new_with_preferences(
            provider_repo,
            preference_repo.clone(),
        );

        service
            .persist_refresh_metadata(Some(1_700_000_000_000), None)
            .await
            .unwrap();
        let rows = preference_repo
            .get_by_keys(&[FREE_LAST_REFRESH_PREF, FREE_LAST_ERROR_PREF])
            .await
            .unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(
            rows.iter()
                .find(|row| row.key == FREE_LAST_REFRESH_PREF)
                .map(|row| row.value.as_str()),
            Some("1700000000000")
        );
        assert_eq!(
            rows.iter()
                .find(|row| row.key == FREE_LAST_ERROR_PREF)
                .map(|row| row.value.as_str()),
            Some("null")
        );
    }

    #[tokio::test]
    async fn loopback_rejects_missing_bearer() {
        let db = init_database_memory().await.unwrap();
        let repo: Arc<dyn IProviderRepository> =
            Arc::new(SqliteProviderRepository::new(db.pool().clone()));
        let service = ManagedModelService::new(repo);
        let response = loopback_models(
            State(LoopbackState {
                service,
                auth_token: "secret".into(),
            }),
            HeaderMap::new(),
        )
        .await;
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let json: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["error"]["type"], "authentication_error");
    }

    #[tokio::test]
    async fn loopback_models_requires_token_and_returns_only_enabled_catalog() {
        let db = init_database_memory().await.unwrap();
        let repo: Arc<dyn IProviderRepository> =
            Arc::new(SqliteProviderRepository::new(db.pool().clone()));
        let service = ManagedModelService::new(repo);
        {
            let mut state = service.free.write().await;
            state.model_enabled.insert("big-pickle".into(), false);
        }
        let mut headers = HeaderMap::new();
        headers.insert(
            header::AUTHORIZATION,
            HeaderValue::from_static("Bearer secret"),
        );
        let response = loopback_models(
            State(LoopbackState {
                service,
                auth_token: "secret".into(),
            }),
            headers,
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let json: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["object"], "list");
        assert!(
            json["data"]
                .as_array()
                .unwrap()
                .iter()
                .all(|model| model["id"] != "big-pickle")
        );
    }

    #[tokio::test]
    async fn loopback_chat_rejects_unknown_model_without_upstream_request() {
        let db = init_database_memory().await.unwrap();
        let repo: Arc<dyn IProviderRepository> =
            Arc::new(SqliteProviderRepository::new(db.pool().clone()));
        let service = ManagedModelService::new(repo);
        let mut headers = HeaderMap::new();
        headers.insert(
            header::AUTHORIZATION,
            HeaderValue::from_static("Bearer secret"),
        );
        let response = loopback_chat(
            State(LoopbackState {
                service,
                auth_token: "secret".into(),
            }),
            headers,
            Json(json!({"model": "not-in-catalog", "messages": []})),
        )
        .await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let json: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["error"]["type"], "invalid_request_error");
    }

    #[tokio::test]
    async fn loopback_server_is_openai_compatible_over_http() {
        let db = init_database_memory().await.unwrap();
        let repo: Arc<dyn IProviderRepository> =
            Arc::new(SqliteProviderRepository::new(db.pool().clone()));
        let service = ManagedModelService::new(repo);
        let mut server = ManagedModelServer::start(service).await.unwrap();
        let client = reqwest::Client::builder().no_proxy().build().unwrap();

        let unauthorized = client
            .get(format!("{}/models", server.base_url()))
            .send()
            .await
            .unwrap();
        assert_eq!(unauthorized.status(), reqwest::StatusCode::UNAUTHORIZED);

        let authorized = client
            .get(format!("{}/models", server.base_url()))
            .bearer_auth(server.auth_token())
            .send()
            .await
            .unwrap();
        assert_eq!(authorized.status(), reqwest::StatusCode::OK);
        let payload: Value = authorized.json().await.unwrap();
        assert_eq!(payload["object"], "list");
        assert!(!payload["data"].as_array().unwrap().is_empty());
        server.stop();
    }
}

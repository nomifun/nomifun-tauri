//! Lightweight, application-managed local text models.
//!
//! NomiFun deliberately owns only the control plane here: a curated catalog,
//! resumable and verified downloads, one active model, and a stable loopback
//! OpenAI facade. Inference remains isolated in a pinned `llama-server`
//! sidecar, so a malformed GGUF or native runtime failure cannot take down the
//! main application process.

use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::io::Read;
use std::net::SocketAddr;
use std::path::{Component, Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::body::Body;
use axum::extract::{DefaultBodyLimit, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use futures_util::StreamExt;
use nomifun_api_types::{
    LocalModelCatalogEntry, LocalModelErrorKind, LocalModelInstallPhase,
    LocalModelProgressComponent, LocalModelRuntimeBackend, LocalModelRuntimePhase,
    LocalModelServiceStatus, LocalModelState, LocalModelTransferProgress,
    LocalRuntimeStatus, ManagedModelServiceKind, ModelTask,
    ModelTrait,
};
use nomifun_common::{encrypt_string, AppError};
use nomifun_db::{
    CreateProviderParams, IProviderRepository, UpdateProviderParams,
};
use reqwest::header::{CONTENT_LENGTH, CONTENT_RANGE, RANGE};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use tokio::io::AsyncWriteExt;
use tokio::net::TcpListener;
use tokio::process::Child;
use tokio::sync::{Mutex, OwnedSemaphorePermit, RwLock, Semaphore};
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, warn};

use crate::managed_model::{LOCAL_MODEL_PROVIDER_ID, MANAGED_MODEL_PROTOCOL_VERSION};

const LOCAL_MODEL_PROVIDER_NAME: &str = "NomiFun Local Model";
const LOCAL_ROOT_DIR: &str = "local-ai";
const RUNTIME_VERSION: &str = "b9957";
const STATE_VERSION: u32 = 1;
const DOWNLOAD_PROGRESS_INTERVAL: Duration = Duration::from_millis(250);
const START_TIMEOUT: Duration = Duration::from_secs(120);
const DISK_SAFETY_BYTES: u64 = 64 * 1024 * 1024;
const RESTART_BACKOFF_BASE_SECS: u64 = 2;
const RESTART_BACKOFF_MAX_SECS: u64 = 60;
const DOWNLOAD_ATTEMPTS_PER_SOURCE: usize = 3;
/// Four sanitized 1.5 MiB images expand to roughly 8 MiB as base64. Keep the
/// loopback facade explicit and bounded while leaving room for JSON and text.
const LOCAL_CHAT_BODY_LIMIT: usize = 10 * 1024 * 1024;
const RETIRED_MODEL_ARTIFACTS: [(&str, &str); 3] = [
    ("qwen3-0.6b-q4-k-m", "qwen3-0.6b-q4-k-m.gguf"),
    ("qwen3-1.7b-q4-k-m", "qwen3-1.7b-q4-k-m.gguf"),
    ("qwen3-4b-q4-k-m", "qwen3-4b-q4-k-m.gguf"),
];

#[derive(Clone)]
struct ModelArtifact {
    entry: LocalModelCatalogEntry,
    /// Size of the language-model GGUF. `entry.download_size_bytes` is the
    /// aggregate user-visible download size including optional components.
    model_size_bytes: u64,
    file_name: &'static str,
    url: &'static str,
    sha256: &'static str,
    vision_projector: Option<ComponentArtifact>,
}

#[derive(Clone, Copy)]
struct ComponentArtifact {
    file_name: &'static str,
    url: &'static str,
    sha256: &'static str,
    size: u64,
    progress_component: LocalModelProgressComponent,
}

#[derive(Clone, Copy)]
enum ArchiveKind {
    Zip,
    TarGz,
}

#[derive(Clone, Copy)]
struct RuntimeArtifact {
    url: &'static str,
    sha256: &'static str,
    size: u64,
    archive_name: &'static str,
    archive_kind: ArchiveKind,
    backend: LocalModelRuntimeBackend,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
struct PersistedState {
    #[serde(default = "state_version")]
    version: u32,
    #[serde(default)]
    installed_model_ids: Vec<String>,
    #[serde(default)]
    active_model_id: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RuntimeStamp {
    version: String,
    archive_sha256: String,
    files: Vec<RuntimeStampedFile>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RuntimeStampedFile {
    path: String,
    size: u64,
}

fn state_version() -> u32 {
    STATE_VERSION
}

struct ActiveDownload {
    model_id: String,
    cancel: CancellationToken,
}

struct Sidecar {
    model_id: String,
    base_url: String,
    api_key: String,
    api_key_file: PathBuf,
    child: Child,
}

struct KeyFileGuard {
    path: PathBuf,
    armed: bool,
}

impl KeyFileGuard {
    fn new(path: PathBuf) -> Self {
        Self { path, armed: true }
    }

    fn path(&self) -> &Path {
        &self.path
    }

    fn disarm(mut self) -> PathBuf {
        self.armed = false;
        self.path.clone()
    }
}

impl Drop for KeyFileGuard {
    fn drop(&mut self) {
        if self.armed {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

struct MutableState {
    persisted: PersistedState,
    models: HashMap<String, LocalModelState>,
    /// Full-file hashes verified during this process. A persisted size match is
    /// never sufficient before handing a GGUF to native code.
    verified_models: HashSet<String>,
    runtime: LocalRuntimeStatus,
    download: Option<ActiveDownload>,
    sidecar: Option<Sidecar>,
    restart_failures: u32,
    restart_not_before: Option<Instant>,
    last_error: Option<String>,
}

#[derive(Clone)]
struct ProviderProjection {
    base_url: String,
    encrypted_token: String,
}

#[derive(Debug, Clone)]
struct AuxiliaryProjectionModel {
    id: String,
    description: String,
}

#[derive(Debug)]
struct LocalFailure {
    kind: LocalModelErrorKind,
    safe_message: &'static str,
    detail: String,
}

impl LocalFailure {
    fn new(kind: LocalModelErrorKind, safe_message: &'static str, detail: impl Into<String>) -> Self {
        Self {
            kind,
            safe_message,
            detail: detail.into(),
        }
    }

    fn cancelled() -> Self {
        Self::new(
            LocalModelErrorKind::Unknown,
            "下载已暂停，可以稍后继续。",
            "download cancelled by user",
        )
    }
}

fn restart_backoff(failures: u32) -> Duration {
    let exponent = failures.saturating_sub(1).min(5);
    let seconds = RESTART_BACKOFF_BASE_SECS
        .saturating_mul(1_u64 << exponent)
        .min(RESTART_BACKOFF_MAX_SECS);
    Duration::from_secs(seconds)
}

/// Local-model control plane shared by REST routes and the loopback facade.
pub struct LocalModelService {
    root: PathBuf,
    provider_repo: Arc<dyn IProviderRepository>,
    http_client: reqwest::Client,
    sidecar_client: reqwest::Client,
    catalog: Vec<ModelArtifact>,
    runtime_artifact: Option<RuntimeArtifact>,
    /// Test-only escape hatch for an injected loopback HTTP origin. Production
    /// constructors always keep this false.
    allow_insecure_loopback_downloads: bool,
    state: Mutex<MutableState>,
    /// Serializes user-visible control mutations and the completion edge of a
    /// background install, preventing activate/delete/install TOCTOU races.
    mutation_lock: Mutex<()>,
    /// Serializes state-file commits even if a future caller persists outside
    /// the control mutation critical section.
    persist_lock: Mutex<()>,
    /// Avoids hashing the same multi-GB GGUF concurrently on first use.
    verification_lock: Mutex<()>,
    start_lock: Mutex<()>,
    inference_gate: Arc<Semaphore>,
    projection: RwLock<Option<ProviderProjection>>,
    auxiliary_projection_models: RwLock<HashMap<String, AuxiliaryProjectionModel>>,
    /// Serializes projections owned by the chat and image control planes so a
    /// slower stale write cannot erase a model installed by the other plane.
    projection_sync_lock: Mutex<()>,
}

impl LocalModelService {
    async fn new(
        root: PathBuf,
        provider_repo: Arc<dyn IProviderRepository>,
    ) -> Result<Arc<Self>, AppError> {
        prepare_managed_directory(&root, &root)
            .and_then(|_| prepare_managed_directory(&root, &root.join("models")))
            .and_then(|_| prepare_managed_directory(&root, &root.join("runtime")))
            .and_then(|_| prepare_managed_directory(&root, &root.join("downloads")))
            .map_err(|e| AppError::Internal(format!("prepare local AI directory: {e}")))?;

        cleanup_retired_model_artifacts(&root).await;
        let catalog = built_in_catalog();
        let mut persisted = load_persisted_state(&root).await;
        let runtime_artifact = runtime_artifact();
        let runtime = LocalRuntimeStatus {
            version: runtime_artifact.map(|_| RUNTIME_VERSION.to_owned()),
            backend: runtime_artifact.map(|asset| asset.backend),
            phase: LocalModelRuntimePhase::Stopped,
            error_kind: runtime_artifact
                .is_none()
                .then_some(LocalModelErrorKind::UnsupportedPlatform),
            message: runtime_artifact
                .is_none()
                .then_some("当前系统暂不支持一键本地模型运行。".to_owned()),
        };

        let known_ids = catalog
            .iter()
            .map(|model| model.entry.id.as_str())
            .collect::<HashSet<_>>();
        persisted
            .installed_model_ids
            .retain(|id| known_ids.contains(id.as_str()));

        let mut models = HashMap::new();
        for model in &catalog {
            let final_path = model_path_at(&root, model);
            let model_partial_path = partial_path(&final_path);
            prepare_managed_file(&root, &final_path)
                .and_then(|_| prepare_managed_file(&root, &model_partial_path))
                .map_err(|e| AppError::Internal(format!("validate local model path: {e}")))?;
            if let Some(component) = model.vision_projector {
                let component_path = component_path_at(&root, model, &component);
                prepare_managed_file(&root, &component_path)
                    .and_then(|_| prepare_managed_file(&root, &partial_path(&component_path)))
                    .map_err(|e| AppError::Internal(format!("validate local model component path: {e}")))?;
            }
            let installed = persisted.installed_model_ids.iter().any(|id| id == &model.entry.id)
                && artifact_files_installed(&root, model).await;
            let downloaded_bytes = downloaded_artifact_bytes(&root, model).await;
            let install_phase = if installed {
                LocalModelInstallPhase::Installed
            } else if downloaded_bytes > 0 {
                LocalModelInstallPhase::Paused
            } else {
                LocalModelInstallPhase::NotInstalled
            };
            if !installed {
                persisted
                    .installed_model_ids
                    .retain(|id| id != &model.entry.id);
            }
            models.insert(
                model.entry.id.clone(),
                LocalModelState {
                    model_id: model.entry.id.clone(),
                    install_phase,
                    progress: None,
                    installed_bytes: downloaded_bytes,
                    runtime_phase: LocalModelRuntimePhase::Stopped,
                    error_kind: None,
                    message: None,
                },
            );
        }

        if persisted
            .active_model_id
            .as_ref()
            .is_some_and(|id| !persisted.installed_model_ids.contains(id))
        {
            persisted.active_model_id = None;
        }

        Ok(Arc::new(Self {
            root,
            provider_repo,
            http_client: local_download_client(),
            sidecar_client: reqwest::Client::builder()
                .no_proxy()
                .connect_timeout(Duration::from_secs(3))
                .read_timeout(Duration::from_secs(300))
                .build()
                .expect("loopback HTTP client configuration is valid"),
            catalog,
            runtime_artifact,
            allow_insecure_loopback_downloads: false,
            state: Mutex::new(MutableState {
                persisted,
                models,
                verified_models: HashSet::new(),
                runtime,
                download: None,
                sidecar: None,
                restart_failures: 0,
                restart_not_before: None,
                last_error: None,
            }),
            mutation_lock: Mutex::new(()),
            persist_lock: Mutex::new(()),
            verification_lock: Mutex::new(()),
            start_lock: Mutex::new(()),
            inference_gate: Arc::new(Semaphore::new(1)),
            projection: RwLock::new(None),
            auxiliary_projection_models: RwLock::new(HashMap::new()),
            projection_sync_lock: Mutex::new(()),
        }))
    }

    /// Return immutable curated metadata. URLs and local paths intentionally
    /// stay out of the public contract.
    pub async fn catalog(&self) -> Vec<LocalModelCatalogEntry> {
        self.catalog.iter().map(|m| m.entry.clone()).collect()
    }

    /// Return the complete mutable snapshot consumed by the Model Hub.
    pub async fn status(&self) -> LocalModelServiceStatus {
        let state = self.state.lock().await;
        let active = state.persisted.active_model_id.clone();
        let ready = active.as_ref().is_some_and(|id| {
            state.models.get(id).is_some_and(|model| {
                model.install_phase == LocalModelInstallPhase::Installed
                    && model.runtime_phase == LocalModelRuntimePhase::Ready
            })
        });
        LocalModelServiceStatus {
            kind: ManagedModelServiceKind::Local,
            protocol_version: MANAGED_MODEL_PROTOCOL_VERSION.to_owned(),
            provider_id: LOCAL_MODEL_PROVIDER_ID.to_owned(),
            enabled: active.is_some(),
            ready,
            active_model_id: active,
            runtime: state.runtime.clone(),
            models: self
                .catalog
                .iter()
                .filter_map(|entry| state.models.get(&entry.entry.id).cloned())
                .collect(),
            last_error: state.last_error.clone(),
        }
    }

    fn artifact(&self, model_id: &str) -> Result<ModelArtifact, AppError> {
        self.catalog
            .iter()
            .find(|model| model.entry.id == model_id)
            .cloned()
            .ok_or_else(|| AppError::NotFound("未知的本地模型。".into()))
    }

    fn try_inference_permit(&self) -> Result<OwnedSemaphorePermit, AppError> {
        self.inference_gate
            .clone()
            .try_acquire_owned()
            .map_err(|_| AppError::Conflict("本地模型正在生成内容，请稍后再切换。".into()))
    }

    /// Shared one-at-a-time gate for heavyweight local GPU/RAM workloads.
    /// The image-generation adapter uses this same permit as chat inference.
    pub fn workload_gate(&self) -> Arc<Semaphore> {
        self.inference_gate.clone()
    }

    /// Add or remove an installed non-chat model from the managed provider.
    /// Installation services call this only after all pinned artifacts have
    /// passed verification; partial downloads never become selectable.
    pub async fn set_auxiliary_model_projection(
        &self,
        model_id: &str,
        description: &str,
        installed: bool,
    ) -> Result<(), AppError> {
        let model_id = model_id.trim();
        if model_id.is_empty()
            || self.catalog.iter().any(|artifact| artifact.entry.id == model_id)
        {
            return Err(AppError::BadRequest(
                "辅助本地模型 ID 为空或与语言模型冲突。".into(),
            ));
        }
        {
            let mut models = self.auxiliary_projection_models.write().await;
            if installed {
                models.insert(
                    model_id.to_owned(),
                    AuxiliaryProjectionModel {
                        id: model_id.to_owned(),
                        description: description.trim().to_owned(),
                    },
                );
            } else {
                models.remove(model_id);
            }
        }
        self.sync_provider_projection().await
    }

    /// Start or resume a model install. The transfer runs in the background;
    /// callers poll [`Self::status`] for progress.
    pub async fn install(self: &Arc<Self>, model_id: &str) -> Result<LocalModelServiceStatus, AppError> {
        let _mutation_guard = self.mutation_lock.lock().await;
        let artifact = self.artifact(model_id)?;
        if self.runtime_artifact.is_none() && configured_runtime_path().is_none() {
            let mut state = self.state.lock().await;
            if let Some(model) = state.models.get_mut(model_id) {
                model.install_phase = LocalModelInstallPhase::Failed;
                model.error_kind = Some(LocalModelErrorKind::UnsupportedPlatform);
                model.message = Some("当前系统暂不支持一键本地模型运行。".into());
            }
            return Err(AppError::BadRequest("当前系统暂不支持本地模型运行。".into()));
        }

        let cancel = CancellationToken::new();
        {
            let mut state = self.state.lock().await;
            if state
                .models
                .get(model_id)
                .is_some_and(|model| model.install_phase == LocalModelInstallPhase::Installed)
            {
                drop(state);
                let _inference_guard = self.try_inference_permit()?;
                self.ensure_runtime_for_activation(model_id).await?;
                self.activate_model(model_id).await?;
                return Ok(self.status().await);
            }
            if let Some(active) = &state.download {
                if active.model_id == model_id {
                    drop(state);
                    return Ok(self.status().await);
                }
                return Err(AppError::Conflict(
                    "另一个本地模型正在下载，请等待或先取消。".into(),
                ));
            }
            state.download = Some(ActiveDownload {
                model_id: model_id.to_owned(),
                cancel: cancel.clone(),
            });
            let model = state
                .models
                .get_mut(model_id)
                .expect("catalog and mutable state stay aligned");
            model.install_phase = LocalModelInstallPhase::Downloading;
            model.progress = None;
            model.error_kind = None;
            model.message = None;
            state.last_error = None;
        }

        let service = self.clone();
        tokio::spawn(async move {
            let result = service.run_install(&artifact, cancel).await;
            service.finish_install(&artifact, result).await;
        });
        Ok(self.status().await)
    }

    pub async fn cancel(&self, model_id: &str) -> Result<LocalModelServiceStatus, AppError> {
        let _mutation_guard = self.mutation_lock.lock().await;
        self.artifact(model_id)?;
        let state = self.state.lock().await;
        match &state.download {
            Some(download) if download.model_id == model_id => {
                download.cancel.cancel();
            }
            Some(_) => {
                return Err(AppError::Conflict("另一个本地模型正在下载。".into()));
            }
            None => {
                if !state.models.get(model_id).is_some_and(|model| {
                    model.install_phase == LocalModelInstallPhase::Paused
                }) {
                    return Err(AppError::Conflict("该模型当前没有可取消的下载。".into()));
                }
            }
        }
        drop(state);
        Ok(self.status().await)
    }

    pub async fn delete(&self, model_id: &str) -> Result<LocalModelServiceStatus, AppError> {
        let _mutation_guard = self.mutation_lock.lock().await;
        let artifact = self.artifact(model_id)?;
        let deleting_active = {
            let state = self.state.lock().await;
            if state.download.is_some() {
                return Err(AppError::Conflict("请先取消当前下载。".into()));
            }
            state.persisted.active_model_id.as_deref() == Some(model_id)
        };
        let _inference_guard = deleting_active
            .then(|| self.try_inference_permit())
            .transpose()?;
        if deleting_active {
            self.deactivate_model().await?;
        }

        let _verification_guard = self.verification_lock.lock().await;
        let final_path = model_path_at(&self.root, &artifact);
        let part_path = partial_path(&final_path);
        prepare_managed_file(&self.root, &final_path)
            .and_then(|_| prepare_managed_file(&self.root, &part_path))
            .map_err(|error| {
                AppError::Internal(format!("validate local model deletion path: {error}"))
            })?;
        remove_file_if_exists(&final_path).await?;
        remove_file_if_exists(&part_path).await?;
        if let Some(component) = artifact.vision_projector {
            let component_path = component_path_at(&self.root, &artifact, &component);
            let component_part_path = partial_path(&component_path);
            prepare_managed_file(&self.root, &component_path)
                .and_then(|_| prepare_managed_file(&self.root, &component_part_path))
                .map_err(|error| {
                    AppError::Internal(format!("validate local model component deletion path: {error}"))
                })?;
            remove_file_if_exists(&component_path).await?;
            remove_file_if_exists(&component_part_path).await?;
        }
        if let Some(parent) = final_path.parent() {
            let _ = tokio::fs::remove_dir(parent).await;
        }
        {
            let mut state = self.state.lock().await;
            state
                .persisted
                .installed_model_ids
                .retain(|id| id != model_id);
            state.verified_models.remove(model_id);
            if let Some(model) = state.models.get_mut(model_id) {
                *model = LocalModelState {
                    model_id: model_id.to_owned(),
                    install_phase: LocalModelInstallPhase::NotInstalled,
                    progress: None,
                    installed_bytes: 0,
                    runtime_phase: LocalModelRuntimePhase::Stopped,
                    error_kind: None,
                    message: None,
                };
            }
        }
        self.save_state().await?;
        self.sync_provider_projection().await?;
        Ok(self.status().await)
    }

    pub async fn set_active(
        self: &Arc<Self>,
        model_id: &str,
        enabled: bool,
    ) -> Result<LocalModelServiceStatus, AppError> {
        let _mutation_guard = self.mutation_lock.lock().await;
        self.artifact(model_id)?;
        if enabled {
            {
                let state = self.state.lock().await;
                if state.download.is_some() {
                    return Err(AppError::Conflict("模型仍在下载中。".into()));
                }
                if !state.models.get(model_id).is_some_and(|model| {
                    model.install_phase == LocalModelInstallPhase::Installed
                }) {
                    return Err(AppError::Conflict("请先下载该模型。".into()));
                }
            }
            let _inference_guard = self.try_inference_permit()?;
            self.ensure_runtime_for_activation(model_id).await?;
            self.activate_model(model_id).await?;
        } else {
            let active = self
                .state
                .lock()
                .await
                .persisted
                .active_model_id
                .clone();
            if active.as_deref() == Some(model_id) {
                let _inference_guard = self.try_inference_permit()?;
                self.deactivate_model().await?;
            }
        }
        Ok(self.status().await)
    }

    async fn run_install(
        &self,
        artifact: &ModelArtifact,
        cancel: CancellationToken,
    ) -> Result<(), LocalFailure> {
        self.ensure_runtime_installed(&artifact.entry.id, &cancel).await?;
        if cancel.is_cancelled() {
            return Err(LocalFailure::cancelled());
        }
        let destination = model_path_at(&self.root, artifact);
        self.download_verified(
            artifact.url,
            artifact.sha256,
            artifact.model_size_bytes,
            &destination,
            &artifact.entry.id,
            LocalModelProgressComponent::Model,
            &cancel,
        )
        .await?;
        if cancel.is_cancelled() {
            return Err(LocalFailure::cancelled());
        }
        if let Some(component) = artifact.vision_projector {
            let destination = component_path_at(&self.root, artifact, &component);
            self.download_verified(
                component.url,
                component.sha256,
                component.size,
                &destination,
                &artifact.entry.id,
                component.progress_component,
                &cancel,
            )
            .await?;
        }
        Ok(())
    }

    async fn finish_install(
        self: &Arc<Self>,
        artifact: &ModelArtifact,
        result: Result<(), LocalFailure>,
    ) {
        let _mutation_guard = self.mutation_lock.lock().await;
        match result {
            Ok(()) => {
                {
                    let mut state = self.state.lock().await;
                    if !state
                        .persisted
                        .installed_model_ids
                        .contains(&artifact.entry.id)
                    {
                        state
                            .persisted
                            .installed_model_ids
                            .push(artifact.entry.id.clone());
                    }
                    if let Some(model) = state.models.get_mut(&artifact.entry.id) {
                        model.install_phase = LocalModelInstallPhase::Installed;
                        model.progress = None;
                        model.installed_bytes = artifact.entry.download_size_bytes;
                        model.error_kind = None;
                        model.message = None;
                    }
                    state.verified_models.insert(artifact.entry.id.clone());
                    state.download = None;
                    state.last_error = None;
                }
                if let Err(error) = self.save_state().await {
                    error!(error = %error, "Failed to persist completed local model install");
                }
                match self.try_inference_permit() {
                    Ok(_inference_guard) => {
                        if let Err(error) = self.activate_model(&artifact.entry.id).await {
                            warn!(error = %error, model = %artifact.entry.id, "Installed local model could not start");
                        }
                    }
                    Err(error) => {
                        info!(error = %error, model = %artifact.entry.id, "Installed local model will remain inactive while inference is busy");
                    }
                }
            }
            Err(failure) if failure.detail == "download cancelled by user" => {
                let downloaded_bytes = downloaded_artifact_bytes(&self.root, artifact).await;
                let mut state = self.state.lock().await;
                if let Some(model) = state.models.get_mut(&artifact.entry.id) {
                    model.install_phase = LocalModelInstallPhase::Paused;
                    model.progress = None;
                    model.installed_bytes = downloaded_bytes;
                    model.error_kind = None;
                    model.message = Some(failure.safe_message.into());
                }
                state.download = None;
            }
            Err(failure) => {
                warn!(
                    model = %artifact.entry.id,
                    error = %failure.detail,
                    "Local model install failed"
                );
                let mut state = self.state.lock().await;
                if let Some(model) = state.models.get_mut(&artifact.entry.id) {
                    model.install_phase = LocalModelInstallPhase::Failed;
                    model.progress = None;
                    model.error_kind = Some(failure.kind);
                    model.message = Some(failure.safe_message.into());
                }
                state.download = None;
                state.last_error = Some(failure.safe_message.into());
            }
        }
    }

    async fn set_transfer_progress(
        &self,
        model_id: &str,
        component: LocalModelProgressComponent,
        downloaded_bytes: u64,
        total_bytes: u64,
        bytes_per_second: u64,
    ) {
        let completed_model_bytes = if component == LocalModelProgressComponent::VisionProjector {
            self.catalog
                .iter()
                .find(|artifact| artifact.entry.id == model_id)
                .map(|artifact| artifact.model_size_bytes)
                .unwrap_or_default()
        } else {
            0
        };
        let mut state = self.state.lock().await;
        if let Some(model) = state.models.get_mut(model_id) {
            model.install_phase = LocalModelInstallPhase::Downloading;
            model.progress = Some(LocalModelTransferProgress {
                component,
                downloaded_bytes,
                total_bytes,
                bytes_per_second,
            });
            model.installed_bytes = completed_model_bytes.saturating_add(downloaded_bytes);
        }
    }

    async fn set_verifying(&self, model_id: &str) {
        let mut state = self.state.lock().await;
        if let Some(model) = state.models.get_mut(model_id) {
            model.install_phase = LocalModelInstallPhase::Verifying;
            model.progress = None;
        }
    }

    #[allow(clippy::too_many_arguments)]
    async fn download_verified(
        &self,
        url: &str,
        expected_sha256: &str,
        expected_size: u64,
        destination: &Path,
        model_id: &str,
        component: LocalModelProgressComponent,
        cancel: &CancellationToken,
    ) -> Result<(), LocalFailure> {
        let sources = download_sources(url);
        let source_count = sources.len();
        let mut last_failure = None;
        for (source_index, source) in sources.into_iter().enumerate() {
            for source_attempt in 0..DOWNLOAD_ATTEMPTS_PER_SOURCE {
                if cancel.is_cancelled() {
                    return Err(LocalFailure::cancelled());
                }
                match self
                    .download_verified_once(
                        &source,
                        expected_sha256,
                        expected_size,
                        destination,
                        model_id,
                        component,
                        cancel,
                    )
                    .await
                {
                    Ok(()) => return Ok(()),
                    Err(failure)
                        if matches!(
                            failure.kind,
                            LocalModelErrorKind::Network
                                | LocalModelErrorKind::ChecksumMismatch
                        ) =>
                    {
                        let checksum_mismatch =
                            failure.kind == LocalModelErrorKind::ChecksumMismatch;
                        let source_host = reqwest::Url::parse(&source)
                            .ok()
                            .and_then(|url| url.host_str().map(str::to_owned))
                            .unwrap_or_else(|| "unknown".into());
                        warn!(
                            host = source_host,
                            attempt = source_attempt + 1,
                            error = %failure.detail,
                            "Local artifact download attempt failed"
                        );
                        last_failure = Some(failure);
                        let another_source = source_index + 1 < source_count;
                        let another_attempt = source_attempt + 1 < DOWNLOAD_ATTEMPTS_PER_SOURCE;
                        if another_attempt || another_source {
                            let delay = Duration::from_secs(1_u64 << source_attempt.min(2));
                            tokio::select! {
                                _ = cancel.cancelled() => return Err(LocalFailure::cancelled()),
                                _ = tokio::time::sleep(delay) => {}
                            }
                        }
                        // A complete checksum mismatch from the primary host
                        // should move directly to the independent mirror. The
                        // failed verification already removed the mixed .part,
                        // so the mirror restarts safely from byte zero. On the
                        // final source, retry once more from zero instead.
                        if checksum_mismatch && another_source {
                            break;
                        }
                    }
                    Err(failure) => return Err(failure),
                }
            }
        }
        Err(last_failure.unwrap_or_else(|| {
            LocalFailure::new(
                LocalModelErrorKind::Network,
                "下载失败，请检查网络后重试。",
                "no download source succeeded",
            )
        }))
    }

    #[allow(clippy::too_many_arguments)]
    async fn download_verified_once(
        &self,
        url: &str,
        expected_sha256: &str,
        expected_size: u64,
        destination: &Path,
        model_id: &str,
        component: LocalModelProgressComponent,
        cancel: &CancellationToken,
    ) -> Result<(), LocalFailure> {
        prepare_managed_file(&self.root, destination).map_err(|error| {
            LocalFailure::new(
                LocalModelErrorKind::Unknown,
                "本地模型目录未通过安全校验。",
                error.to_string(),
            )
        })?;
        let part = partial_path(destination);
        prepare_managed_file(&self.root, &part).map_err(|error| {
            LocalFailure::new(
                LocalModelErrorKind::Unknown,
                "本地模型下载路径未通过安全校验。",
                error.to_string(),
            )
        })?;

        if file_len(destination).await == expected_size {
            self.set_verifying(model_id).await;
            if hash_file_cancellable(destination, cancel).await? == expected_sha256 {
                if cancel.is_cancelled() {
                    return Err(LocalFailure::cancelled());
                }
                prepare_managed_file(&self.root, destination).map_err(|error| {
                    LocalFailure::new(
                        LocalModelErrorKind::Unknown,
                        "本地模型文件未通过安全校验。",
                        error.to_string(),
                    )
                })?;
                return Ok(());
            }
            remove_file_if_exists_failure(destination).await?;
        }

        let mut offset = file_len(&part).await;
        if offset > expected_size {
            remove_file_if_exists_failure(&part).await?;
            offset = 0;
        }
        if offset == expected_size {
            self.set_verifying(model_id).await;
            prepare_managed_file(&self.root, &part).map_err(|error| {
                LocalFailure::new(
                    LocalModelErrorKind::Unknown,
                    "本地模型下载路径未通过安全校验。",
                    error.to_string(),
                )
            })?;
            if hash_file_cancellable(&part, cancel).await? == expected_sha256 {
                if cancel.is_cancelled() {
                    return Err(LocalFailure::cancelled());
                }
                prepare_managed_file(&self.root, destination)
                    .and_then(|_| prepare_managed_file(&self.root, &part))
                    .map_err(|error| {
                        LocalFailure::new(
                            LocalModelErrorKind::Unknown,
                            "本地模型提交路径未通过安全校验。",
                            error.to_string(),
                        )
                    })?;
                remove_file_if_exists_failure(destination).await?;
                tokio::fs::rename(&part, destination).await.map_err(|error| {
                    LocalFailure::new(
                        LocalModelErrorKind::Unknown,
                        "无法完成模型安装。",
                        error.to_string(),
                    )
                })?;
                return Ok(());
            }
            remove_file_if_exists_failure(&part).await?;
            offset = 0;
        }

        let parent = destination.parent().unwrap_or(&self.root).to_path_buf();
        ensure_disk_space(parent, expected_size.saturating_sub(offset)).await?;
        if cancel.is_cancelled() {
            return Err(LocalFailure::cancelled());
        }

        let mut request = self.http_client.get(url);
        if offset > 0 {
            request = request.header(RANGE, format!("bytes={offset}-"));
        }
        let response = tokio::select! {
            _ = cancel.cancelled() => return Err(LocalFailure::cancelled()),
            response = request.send() => response.map_err(|error| {
                LocalFailure::new(
                    LocalModelErrorKind::Network,
                    "下载失败，请检查网络后重试。",
                    error.to_string(),
                )
            })?,
        };
        if !allowed_download_url(response.url())
            && !(self.allow_insecure_loopback_downloads
                && loopback_download_url(response.url()))
        {
            return Err(LocalFailure::new(
                LocalModelErrorKind::Network,
                "下载来源未通过安全校验。",
                "redirected to a disallowed host",
            ));
        }

        let status = response.status();
        let mut append = false;
        if offset > 0 && status == reqwest::StatusCode::PARTIAL_CONTENT {
            let value = response
                .headers()
                .get(CONTENT_RANGE)
                .and_then(|value| value.to_str().ok())
                .and_then(parse_content_range)
                .ok_or_else(|| {
                    LocalFailure::new(
                        LocalModelErrorKind::Network,
                        "服务器返回了无效的续传响应。",
                        "missing or invalid Content-Range",
                    )
                })?;
            if value.0 != offset
                || value.1 != expected_size.saturating_sub(1)
                || value.2 != expected_size
            {
                return Err(LocalFailure::new(
                    LocalModelErrorKind::Network,
                    "服务器返回了不匹配的续传范围。",
                    format!(
                        "Content-Range start/total mismatch: start={}, total={}",
                        value.0, value.2
                    ),
                ));
            }
            append = true;
        } else if offset > 0 && status.is_success() {
            // The origin ignored Range. Restart from byte zero rather than
            // appending a complete response to stale partial data.
            offset = 0;
        } else if offset == 0 && status == reqwest::StatusCode::PARTIAL_CONTENT {
            let value = response
                .headers()
                .get(CONTENT_RANGE)
                .and_then(|value| value.to_str().ok())
                .and_then(parse_content_range)
                .ok_or_else(|| {
                    LocalFailure::new(
                        LocalModelErrorKind::Network,
                        "服务器返回了无效的下载范围。",
                        "invalid initial Content-Range",
                    )
                })?;
            if value.0 != 0
                || value.1 != expected_size.saturating_sub(1)
                || value.2 != expected_size
            {
                return Err(LocalFailure::new(
                    LocalModelErrorKind::Network,
                    "服务器返回了不匹配的下载范围。",
                    "initial Content-Range mismatch",
                ));
            }
        } else if !status.is_success() {
            return Err(LocalFailure::new(
                LocalModelErrorKind::Network,
                "下载服务暂时不可用，请稍后重试。",
                format!("HTTP status {status}"),
            ));
        }

        if let Some(length) = response
            .headers()
            .get(CONTENT_LENGTH)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.parse::<u64>().ok())
        {
            let expected_response = expected_size.saturating_sub(offset);
            if length != expected_response {
                return Err(LocalFailure::new(
                    LocalModelErrorKind::Network,
                    "下载文件大小与目录记录不一致。",
                    format!("Content-Length {length}, expected {expected_response}"),
                ));
            }
        }

        // Re-check immediately before opening: the request may have spent a
        // long time waiting on the network after the initial preflight.
        prepare_managed_file(&self.root, &part).map_err(|error| {
            LocalFailure::new(
                LocalModelErrorKind::Unknown,
                "本地模型下载路径未通过安全校验。",
                error.to_string(),
            )
        })?;
        let mut options = tokio::fs::OpenOptions::new();
        options.create(true).write(true);
        if append {
            options.append(true);
        }
        let mut file = options.open(&part).await.map_err(|error| {
            LocalFailure::new(
                LocalModelErrorKind::Unknown,
                "无法写入模型文件。",
                error.to_string(),
            )
        })?;
        // Do not truncate until both the pathname and opened handle have been
        // checked. This prevents a pre-positioned link from truncating a file
        // outside the managed root even if it appears during the request.
        prepare_managed_file(&self.root, &part).map_err(|error| {
            LocalFailure::new(
                LocalModelErrorKind::Unknown,
                "本地模型下载路径未通过安全校验。",
                error.to_string(),
            )
        })?;
        if !file.metadata().await.map(|metadata| metadata.is_file()).unwrap_or(false) {
            return Err(LocalFailure::new(
                LocalModelErrorKind::Unknown,
                "本地模型下载路径不是普通文件。",
                "opened partial target is not a regular file",
            ));
        }
        if !append {
            file.set_len(0).await.map_err(|error| {
                LocalFailure::new(
                    LocalModelErrorKind::Unknown,
                    "无法重置模型下载文件。",
                    error.to_string(),
                )
            })?;
        }
        let mut stream = response.bytes_stream();
        let started = Instant::now();
        let mut last_report = Instant::now() - DOWNLOAD_PROGRESS_INTERVAL;
        let initial_offset = offset;
        let mut downloaded = offset;
        loop {
            let next = tokio::select! {
                _ = cancel.cancelled() => {
                    file.sync_all().await.ok();
                    return Err(LocalFailure::cancelled());
                }
                next = stream.next() => next,
            };
            let Some(chunk) = next else {
                break;
            };
            let chunk = match chunk {
                Ok(chunk) => chunk,
                Err(error) => {
                    file.sync_all().await.ok();
                    return Err(LocalFailure::new(
                        LocalModelErrorKind::Network,
                        "下载中断，可以稍后继续。",
                        error.to_string(),
                    ));
                }
            };
            downloaded = downloaded.saturating_add(chunk.len() as u64);
            if downloaded > expected_size {
                return Err(LocalFailure::new(
                    LocalModelErrorKind::Network,
                    "下载文件大于目录记录的大小。",
                    "response exceeded expected size",
                ));
            }
            file.write_all(&chunk).await.map_err(|error| {
                LocalFailure::new(
                    LocalModelErrorKind::Unknown,
                    "写入模型文件失败。",
                    error.to_string(),
                )
            })?;
            if last_report.elapsed() >= DOWNLOAD_PROGRESS_INTERVAL {
                let seconds = started.elapsed().as_secs_f64().max(0.001);
                let rate = ((downloaded - initial_offset) as f64 / seconds) as u64;
                self.set_transfer_progress(
                    model_id,
                    component,
                    downloaded,
                    expected_size,
                    rate,
                )
                .await;
                last_report = Instant::now();
            }
        }
        file.sync_all().await.map_err(|error| {
            LocalFailure::new(
                LocalModelErrorKind::Unknown,
                "无法提交模型文件。",
                error.to_string(),
            )
        })?;
        drop(file);
        if downloaded != expected_size {
            return Err(LocalFailure::new(
                LocalModelErrorKind::Network,
                "下载未完成，可以稍后继续。",
                format!("downloaded {downloaded} of {expected_size}"),
            ));
        }

        self.set_verifying(model_id).await;
        prepare_managed_file(&self.root, destination)
            .and_then(|_| prepare_managed_file(&self.root, &part))
            .map_err(|error| {
                LocalFailure::new(
                    LocalModelErrorKind::Unknown,
                    "本地模型校验路径未通过安全校验。",
                    error.to_string(),
                )
            })?;
        let actual = hash_file_cancellable(&part, cancel).await?;
        if cancel.is_cancelled() {
            return Err(LocalFailure::cancelled());
        }
        if actual != expected_sha256 {
            remove_file_if_exists_failure(&part).await?;
            return Err(LocalFailure::new(
                LocalModelErrorKind::ChecksumMismatch,
                "模型完整性校验失败，请重新下载。",
                format!("SHA-256 mismatch: expected {expected_sha256}, got {actual}"),
            ));
        }
        prepare_managed_file(&self.root, destination)
            .and_then(|_| prepare_managed_file(&self.root, &part))
            .map_err(|error| {
                LocalFailure::new(
                    LocalModelErrorKind::Unknown,
                    "本地模型提交路径未通过安全校验。",
                    error.to_string(),
                )
            })?;
        remove_file_if_exists_failure(destination).await?;
        tokio::fs::rename(&part, destination).await.map_err(|error| {
            LocalFailure::new(
                LocalModelErrorKind::Unknown,
                "无法完成模型安装。",
                error.to_string(),
            )
        })?;
        Ok(())
    }

    async fn ensure_runtime_installed(
        &self,
        model_id: &str,
        cancel: &CancellationToken,
    ) -> Result<PathBuf, LocalFailure> {
        if let Some(path) = configured_runtime_path() {
            return Ok(path);
        }
        let artifact = self.runtime_artifact.ok_or_else(|| {
            LocalFailure::new(
                LocalModelErrorKind::UnsupportedPlatform,
                "当前系统暂不支持一键本地模型运行。",
                "no runtime artifact for target",
            )
        })?;
        if let Some(path) = managed_runtime_executable(&self.root, &artifact) {
            return Ok(path);
        }
        // Runtime archives contain many shared libraries and may expand to
        // roughly three times their compressed size. Reserve for archive +
        // staging extraction, not merely the network payload.
        ensure_disk_space(
            self.root.join("runtime"),
            artifact.size.saturating_mul(3),
        )
        .await?;
        let archive = self.root.join("downloads").join(artifact.archive_name);
        self.download_verified(
            artifact.url,
            artifact.sha256,
            artifact.size,
            &archive,
            model_id,
            LocalModelProgressComponent::Runtime,
            cancel,
        )
        .await?;
        if cancel.is_cancelled() {
            return Err(LocalFailure::cancelled());
        }
        self.set_verifying(model_id).await;

        let runtime_dir = runtime_dir_at(&self.root);
        let staging = self
            .root
            .join("runtime")
            .join(format!(".{RUNTIME_VERSION}.staging"));
        prepare_managed_directory(&self.root, &runtime_dir)
            .and_then(|_| prepare_managed_directory(&self.root, &staging))
            .map_err(|error| {
                LocalFailure::new(
                    LocalModelErrorKind::RuntimeUnavailable,
                    "本地运行时目录未通过安全校验。",
                    error.to_string(),
                )
            })?;
        let archive_for_extract = archive.clone();
        let staging_for_extract = staging.clone();
        tokio::task::spawn_blocking(move || {
            if staging_for_extract.exists() {
                std::fs::remove_dir_all(&staging_for_extract)?;
            }
            std::fs::create_dir_all(&staging_for_extract)?;
            extract_archive(
                &archive_for_extract,
                &staging_for_extract,
                artifact.archive_kind,
            )
        })
        .await
        .map_err(|error| {
            LocalFailure::new(
                LocalModelErrorKind::RuntimeUnavailable,
                "本地运行时解压失败。",
                error.to_string(),
            )
        })?
        .map_err(|error| {
            LocalFailure::new(
                LocalModelErrorKind::RuntimeUnavailable,
                "本地运行时解压失败。",
                error.to_string(),
            )
        })?;
        if find_runtime_executable(&staging).is_none() {
            return Err(LocalFailure::new(
                LocalModelErrorKind::RuntimeUnavailable,
                "下载的运行时缺少必要组件。",
                "llama-server executable not found in archive",
            ));
        }
        if runtime_dir.exists() {
            tokio::fs::remove_dir_all(&runtime_dir).await.map_err(|error| {
                LocalFailure::new(
                    LocalModelErrorKind::RuntimeUnavailable,
                    "无法更新本地运行时。",
                    error.to_string(),
                )
            })?;
        }
        tokio::fs::rename(&staging, &runtime_dir)
            .await
            .map_err(|error| {
                LocalFailure::new(
                    LocalModelErrorKind::RuntimeUnavailable,
                    "无法启用本地运行时。",
                    error.to_string(),
                )
            })?;
        let root_for_stamp = self.root.clone();
        tokio::task::spawn_blocking(move || write_runtime_stamp(&root_for_stamp, &artifact))
            .await
            .map_err(|error| {
                LocalFailure::new(
                    LocalModelErrorKind::RuntimeUnavailable,
                    "无法记录本地运行时完整性信息。",
                    error.to_string(),
                )
            })?
            .map_err(|error| {
                LocalFailure::new(
                    LocalModelErrorKind::RuntimeUnavailable,
                    "无法记录本地运行时完整性信息。",
                    error.to_string(),
                )
            })?;
        remove_file_if_exists_failure(&archive).await?;
        let executable = managed_runtime_executable(&self.root, &artifact).ok_or_else(|| {
            LocalFailure::new(
                LocalModelErrorKind::RuntimeUnavailable,
                "本地运行时安装不完整。",
                "runtime executable disappeared after activation",
            )
        })?;
        info!(version = RUNTIME_VERSION, "Local llama-server runtime installed");
        Ok(executable)
    }

    async fn ensure_runtime_for_activation(&self, model_id: &str) -> Result<(), AppError> {
        if configured_runtime_path().is_some() {
            return Ok(());
        }
        if let Some(artifact) = self.runtime_artifact
            && managed_runtime_executable(&self.root, &artifact).is_some()
        {
            return Ok(());
        }
        let artifact = self.artifact(model_id)?;
        let cancel = CancellationToken::new();
        if let Err(failure) = self.ensure_runtime_installed(model_id, &cancel).await {
            let mut state = self.state.lock().await;
            if let Some(model) = state.models.get_mut(model_id) {
                model.install_phase = LocalModelInstallPhase::Installed;
                model.progress = None;
                model.installed_bytes = artifact.entry.download_size_bytes;
                model.error_kind = Some(failure.kind);
                model.message = Some(failure.safe_message.into());
            }
            state.runtime.phase = LocalModelRuntimePhase::Failed;
            state.runtime.error_kind = Some(failure.kind);
            state.runtime.message = Some(failure.safe_message.into());
            state.last_error = Some(failure.safe_message.into());
            warn!(error = %failure.detail, "Local runtime repair failed");
            return Err(AppError::ProviderUnavailable(failure.safe_message.into()));
        }
        let mut state = self.state.lock().await;
        if let Some(model) = state.models.get_mut(model_id) {
            model.install_phase = LocalModelInstallPhase::Installed;
            model.progress = None;
            model.installed_bytes = artifact.entry.download_size_bytes;
            model.error_kind = None;
            model.message = None;
        }
        state.runtime.phase = LocalModelRuntimePhase::Stopped;
        state.runtime.error_kind = None;
        state.runtime.message = None;
        Ok(())
    }

    async fn activate_model(self: &Arc<Self>, model_id: &str) -> Result<(), AppError> {
        let current = self
            .state
            .lock()
            .await
            .persisted
            .active_model_id
            .clone();
        {
            let mut state = self.state.lock().await;
            state.persisted.active_model_id = Some(model_id.to_owned());
            state.restart_failures = 0;
            state.restart_not_before = None;
            state.runtime.phase = LocalModelRuntimePhase::Starting;
            state.runtime.error_kind = None;
            state.runtime.message = None;
            if let Some(model) = state.models.get_mut(model_id) {
                model.runtime_phase = LocalModelRuntimePhase::Starting;
                model.error_kind = None;
                model.message = None;
            }
        }
        if let Err(error) = self.save_state().await {
            self.rollback_activation(
                model_id,
                LocalModelErrorKind::Unknown,
                "无法保存本地模型启用状态。",
                error.to_string(),
            )
            .await;
            return Err(error);
        }
        if let Err(error) = self.sync_provider_projection().await {
            self.rollback_activation(
                model_id,
                LocalModelErrorKind::Unknown,
                "无法启用本地模型服务。",
                error.to_string(),
            )
            .await;
            return Err(error);
        }
        if current.as_deref() != Some(model_id) {
            // The facade selection is committed while the inference permit is
            // exclusively held, then the obsolete process can be stopped
            // without any request resurrecting it.
            self.stop_sidecar().await;
            if let Some(previous_model_id) = &current {
                let mut state = self.state.lock().await;
                if let Some(previous_model) = state.models.get_mut(previous_model_id) {
                    previous_model.runtime_phase = LocalModelRuntimePhase::Stopped;
                }
            }
        }
        if let Err(failure) = self.ensure_sidecar().await {
            warn!(model = model_id, error = %failure.detail, "Local model runtime failed to start");
            self.rollback_activation(
                model_id,
                failure.kind,
                failure.safe_message,
                failure.detail,
            )
            .await;
            return Err(AppError::ProviderUnavailable(failure.safe_message.into()));
        }
        Ok(())
    }

    async fn deactivate_model(&self) -> Result<(), AppError> {
        let active = {
            let mut state = self.state.lock().await;
            let active = state.persisted.active_model_id.take();
            state.runtime.phase = LocalModelRuntimePhase::Stopping;
            if let Some(model_id) = &active
                && let Some(model) = state.models.get_mut(model_id)
            {
                model.runtime_phase = LocalModelRuntimePhase::Stopping;
            }
            active
        };
        if let Err(error) = self.save_state().await {
            warn!(error = %error, "Could not persist local model deactivation before shutdown");
        }
        // Remove the provider projection before terminating the listener it
        // points through, so new selections cannot race with shutdown.
        let projection_error = match self.sync_provider_projection().await {
            Ok(()) => None,
            Err(error) => {
                warn!(error = %error, "Could not update local provider during deactivation");
                match disable_local_model_provider(self.provider_repo.clone()).await {
                    Ok(()) => None,
                    Err(disable_error) => {
                        warn!(error = %disable_error, "Could not disable local provider during deactivation");
                        Some(error)
                    }
                }
            }
        };
        self.stop_sidecar().await;
        {
            let mut state = self.state.lock().await;
            state.restart_failures = 0;
            state.restart_not_before = None;
            state.runtime.phase = LocalModelRuntimePhase::Stopped;
            state.runtime.error_kind = None;
            state.runtime.message = None;
            if let Some(model_id) = active
                && let Some(model) = state.models.get_mut(&model_id)
            {
                model.runtime_phase = LocalModelRuntimePhase::Stopped;
                model.error_kind = None;
                model.message = None;
            }
        }
        self.save_state().await?;
        if let Some(error) = projection_error {
            return Err(error);
        }
        Ok(())
    }

    async fn rollback_activation(
        &self,
        model_id: &str,
        kind: LocalModelErrorKind,
        safe_message: &'static str,
        detail: String,
    ) {
        let delay = {
            let mut state = self.state.lock().await;
            if state.persisted.active_model_id.as_deref() == Some(model_id) {
                state.persisted.active_model_id = None;
            }
            state.restart_failures = state.restart_failures.saturating_add(1);
            let delay = restart_backoff(state.restart_failures);
            state.restart_not_before = Some(Instant::now() + delay);
            state.runtime.phase = LocalModelRuntimePhase::Failed;
            state.runtime.error_kind = Some(kind);
            state.runtime.message = Some(safe_message.into());
            if let Some(model) = state.models.get_mut(model_id) {
                model.runtime_phase = LocalModelRuntimePhase::Failed;
                model.error_kind = Some(kind);
                model.message = Some(safe_message.into());
            }
            for (id, model) in &mut state.models {
                if id != model_id {
                    model.runtime_phase = LocalModelRuntimePhase::Stopped;
                }
            }
            state.last_error = Some(safe_message.into());
            delay
        };
        self.stop_sidecar().await;
        if let Err(error) = self.save_state().await {
            warn!(error = %error, "Could not persist local model activation rollback");
        }
        if let Err(error) = self.sync_provider_projection().await {
            warn!(error = %error, "Could not roll back local provider projection");
            if let Err(disable_error) =
                disable_local_model_provider(self.provider_repo.clone()).await
            {
                warn!(error = %disable_error, "Could not disable local provider after activation failure");
            }
        }
        warn!(model = model_id, error = %detail, retry_after_seconds = delay.as_secs(), "Local model activation rolled back");
    }

    async fn record_sidecar_failure(&self, model_id: &str, failure: &LocalFailure) {
        let delay = {
            let mut state = self.state.lock().await;
            state.restart_failures = state.restart_failures.saturating_add(1);
            let delay = restart_backoff(state.restart_failures);
            state.restart_not_before = Some(Instant::now() + delay);
            state.runtime.phase = LocalModelRuntimePhase::Failed;
            state.runtime.error_kind = Some(failure.kind);
            state.runtime.message = Some(failure.safe_message.into());
            if let Some(model) = state.models.get_mut(model_id) {
                model.runtime_phase = LocalModelRuntimePhase::Failed;
                model.error_kind = Some(failure.kind);
                model.message = Some(failure.safe_message.into());
            }
            state.last_error = Some(failure.safe_message.into());
            delay
        };
        warn!(model = model_id, error = %failure.detail, retry_after_seconds = delay.as_secs(), "Local model sidecar entered restart backoff");
    }

    async fn ensure_sidecar(&self) -> Result<(String, String), LocalFailure> {
        let model_id = self
            .state
            .lock()
            .await
            .persisted
            .active_model_id
            .clone()
            .ok_or_else(|| {
                LocalFailure::new(
                    LocalModelErrorKind::RuntimeUnavailable,
                    "尚未启用本地模型。",
                    "no active model",
                )
            })?;
        let artifact = self
            .catalog
            .iter()
            .find(|model| model.entry.id == model_id)
            .cloned()
            .ok_or_else(|| {
                LocalFailure::new(
                    LocalModelErrorKind::NotFound,
                    "当前本地模型不在目录中。",
                    "active model missing from catalog",
                )
            })?;
        {
            let mut state = self.state.lock().await;
            if let Some(not_before) = state.restart_not_before {
                let now = Instant::now();
                if not_before > now {
                    return Err(LocalFailure::new(
                        LocalModelErrorKind::Busy,
                        "本地模型正在恢复，请稍后重试。",
                        format!(
                            "restart backoff active for another {} ms",
                            not_before.saturating_duration_since(now).as_millis()
                        ),
                    ));
                }
                state.restart_not_before = None;
            }
        }
        let executable = configured_runtime_path()
            .or_else(|| {
                self.runtime_artifact
                    .and_then(|runtime| managed_runtime_executable(&self.root, &runtime))
            })
            .ok_or_else(|| {
                LocalFailure::new(
                    LocalModelErrorKind::RuntimeUnavailable,
                    "本地运行时尚未安装。",
                    "llama-server executable missing",
                )
        })?;
        let model_path = model_path_at(&self.root, &artifact);
        prepare_managed_file(&self.root, &model_path).map_err(|error| {
            LocalFailure::new(
                LocalModelErrorKind::NotFound,
                "本地模型文件未通过安全校验，请重新安装。",
                error.to_string(),
            )
        })?;
        if file_len(&model_path).await != artifact.model_size_bytes {
            return Err(LocalFailure::new(
                LocalModelErrorKind::NotFound,
                "本地模型文件缺失，请重新安装。",
                "installed model file missing or wrong size",
            ));
        }
        let projector_path = if let Some(projector) = artifact.vision_projector {
            let path = component_path_at(&self.root, &artifact, &projector);
            prepare_managed_file(&self.root, &path).map_err(|error| {
                LocalFailure::new(
                    LocalModelErrorKind::NotFound,
                    "本地视觉组件未通过安全校验，请重新安装。",
                    error.to_string(),
                )
            })?;
            if file_len(&path).await != projector.size {
                return Err(LocalFailure::new(
                    LocalModelErrorKind::NotFound,
                    "本地视觉组件缺失，请重新安装。",
                    "installed vision projector missing or wrong size",
                ));
            }
            Some((projector, path))
        } else {
            None
        };
        {
            let _verification_guard = self.verification_lock.lock().await;
            let verified = self
                .state
                .lock()
                .await
                .verified_models
                .contains(&model_id);
            if !verified {
                let actual = hash_file(&model_path).await?;
                if actual != artifact.sha256 {
                    return Err(LocalFailure::new(
                        LocalModelErrorKind::ChecksumMismatch,
                        "本地模型完整性校验失败，请重新安装。",
                        format!(
                            "installed SHA-256 mismatch: expected {}, got {actual}",
                            artifact.sha256
                        ),
                    ));
                }
                if let Some((projector, path)) = &projector_path {
                    let actual = hash_file(path).await?;
                    if actual != projector.sha256 {
                        return Err(LocalFailure::new(
                            LocalModelErrorKind::ChecksumMismatch,
                            "本地视觉组件完整性校验失败，请重新安装。",
                            format!(
                                "installed vision projector SHA-256 mismatch: expected {}, got {actual}",
                                projector.sha256
                            ),
                        ));
                    }
                }
                self.state
                    .lock()
                    .await
                    .verified_models
                    .insert(model_id.clone());
            }
        }

        let _start_guard = self.start_lock.lock().await;
        if self
            .state
            .lock()
            .await
            .persisted
            .active_model_id
            .as_deref()
            != Some(model_id.as_str())
        {
            return Err(LocalFailure::new(
                LocalModelErrorKind::Busy,
                "本地模型选择已改变，请重试。",
                "active model changed while preparing sidecar",
            ));
        }

        let mut previous = None;
        {
            let mut state = self.state.lock().await;
            let mut ready_target = None;
            if let Some(sidecar) = state.sidecar.as_mut() {
                match sidecar.child.try_wait() {
                    Ok(None) if sidecar.model_id == model_id => {
                        ready_target =
                            Some((sidecar.base_url.clone(), sidecar.api_key.clone()));
                    }
                    Ok(None) => previous = state.sidecar.take(),
                    Ok(Some(status)) => {
                        warn!(?status, "Local model sidecar exited");
                        previous = state.sidecar.take();
                    }
                    Err(error) => {
                        warn!(error = %error, "Could not inspect local model sidecar");
                        previous = state.sidecar.take();
                    }
                }
            }
            if let Some(target) = ready_target {
                state.restart_failures = 0;
                state.restart_not_before = None;
                return Ok(target);
            }
            state.runtime.phase = LocalModelRuntimePhase::Starting;
            if let Some(model) = state.models.get_mut(&model_id) {
                model.runtime_phase = LocalModelRuntimePhase::Starting;
            }
        }
        if let Some(mut sidecar) = previous {
            let _ = nomifun_runtime::kill_process_tree(&mut sidecar.child).await;
            let _ = tokio::fs::remove_file(sidecar.api_key_file).await;
        }

        let listener = TcpListener::bind("127.0.0.1:0").await.map_err(|error| {
            LocalFailure::new(
                LocalModelErrorKind::RuntimeUnavailable,
                "无法为本地模型分配端口。",
                error.to_string(),
            )
        })?;
        let port = listener.local_addr().map_err(|error| {
            LocalFailure::new(
                LocalModelErrorKind::RuntimeUnavailable,
                "无法读取本地模型端口。",
                error.to_string(),
            )
        })?.port();
        drop(listener);

        let api_key = generate_token().map_err(|error| {
            LocalFailure::new(
                LocalModelErrorKind::RuntimeUnavailable,
                "无法初始化本地模型认证。",
                error,
            )
        })?;
        let api_key_file = self.root.join("runtime").join("session.key");
        prepare_managed_file(&self.root, &api_key_file).map_err(|error| {
            LocalFailure::new(
                LocalModelErrorKind::RuntimeUnavailable,
                "本地模型认证路径未通过安全校验。",
                error.to_string(),
            )
        })?;
        write_private_key_file(api_key_file.clone(), api_key.clone()).await?;
        let api_key_guard = KeyFileGuard::new(api_key_file);
        let mut command = nomifun_runtime::Builder::new(&executable);
        let threads = std::thread::available_parallelism()
            .map(|count| count.get().saturating_sub(1).max(1))
            .unwrap_or(1);
        command
            .arg("--model")
            .arg(&model_path)
            .arg("--alias")
            .arg(&model_id)
            .arg("--host")
            .arg("127.0.0.1")
            .arg("--port")
            .arg(port.to_string())
            .arg("--api-key-file")
            .arg(api_key_guard.path())
            .arg("--ctx-size")
            .arg(artifact.entry.context_window.to_string())
            .arg("--parallel")
            .arg("1")
            .arg("--threads")
            .arg(threads.to_string())
            .arg("--threads-batch")
            .arg(threads.to_string())
            .arg("--n-gpu-layers")
            .arg("auto")
            .arg("--sleep-idle-seconds")
            .arg("300")
            .arg("--jinja")
            .arg("--offline")
            .arg("--no-webui")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        if let Some((_, projector_path)) = &projector_path {
            command.arg("--mmproj").arg(projector_path);
        }
        if let Some(dir) = executable.parent() {
            command.current_dir(dir);
        }
        let mut child = command.spawn().map_err(|error| {
            LocalFailure::new(
                LocalModelErrorKind::RuntimeUnavailable,
                "本地模型进程启动失败。",
                error.to_string(),
            )
        })?;
        let base_url = format!("http://127.0.0.1:{port}");
        let health_url = format!("{base_url}/health");
        let started = Instant::now();
        loop {
            match child.try_wait() {
                Ok(Some(status)) => {
                    return Err(LocalFailure::new(
                        LocalModelErrorKind::RuntimeUnavailable,
                        "本地模型加载失败，请检查设备资源。",
                        format!("llama-server exited with {status}"),
                    ));
                }
                Err(error) => {
                    return Err(LocalFailure::new(
                        LocalModelErrorKind::RuntimeUnavailable,
                        "无法检查本地模型进程。",
                        error.to_string(),
                    ));
                }
                Ok(None) => {}
            }
            if started.elapsed() >= START_TIMEOUT {
                let _ = nomifun_runtime::kill_process_tree(&mut child).await;
                return Err(LocalFailure::new(
                    LocalModelErrorKind::RuntimeUnavailable,
                    "本地模型加载超时，请尝试较小的模型。",
                    "llama-server health timeout",
                ));
            }
            let health = tokio::time::timeout(
                Duration::from_secs(2),
                self.sidecar_client
                    .get(&health_url)
                    .bearer_auth(&api_key)
                    .send(),
            )
            .await;
            match health {
                Ok(Ok(response)) if response.status().is_success() => break,
                _ => tokio::time::sleep(Duration::from_millis(300)).await,
            }
        }

        let api_key_file = api_key_guard.disarm();
        {
            let mut state = self.state.lock().await;
            state.sidecar = Some(Sidecar {
                model_id: model_id.clone(),
                base_url: base_url.clone(),
                api_key: api_key.clone(),
                api_key_file,
                child,
            });
            state.restart_failures = 0;
            state.restart_not_before = None;
            state.runtime.phase = LocalModelRuntimePhase::Ready;
            state.runtime.error_kind = None;
            state.runtime.message = None;
            state.last_error = None;
            if let Some(model) = state.models.get_mut(&model_id) {
                model.runtime_phase = LocalModelRuntimePhase::Ready;
                model.error_kind = None;
                model.message = None;
            }
        }
        info!(model = %model_id, port, "Local model sidecar is ready");
        Ok((base_url, api_key))
    }

    async fn stop_sidecar(&self) {
        let _start_guard = self.start_lock.lock().await;
        let mut sidecar = { self.state.lock().await.sidecar.take() };
        if let Some(sidecar) = sidecar.as_mut()
            && let Err(error) = nomifun_runtime::kill_process_tree(&mut sidecar.child).await
        {
            warn!(error = %error, "Failed to stop local model sidecar cleanly");
        }
        if let Some(sidecar) = sidecar {
            let _ = tokio::fs::remove_file(sidecar.api_key_file).await;
        }
    }

    async fn proxy_chat(self: &Arc<Self>, body: Value) -> Response {
        let body = apply_local_chat_defaults(body);
        let requested_model = body
            .get("model")
            .and_then(Value::as_str)
            .map(str::to_owned);
        let permit = match self.inference_gate.clone().try_acquire_owned() {
            Ok(permit) => permit,
            Err(_) => {
                return openai_error(
                    StatusCode::TOO_MANY_REQUESTS,
                    "The local model is busy",
                    "rate_limit_error",
                );
            }
        };
        let active = self
            .state
            .lock()
            .await
            .persisted
            .active_model_id
            .clone();
        let Some(active) = active else {
            return openai_error(
                StatusCode::SERVICE_UNAVAILABLE,
                "No local model is active",
                "service_unavailable",
            );
        };
        if requested_model.as_deref() != Some(active.as_str()) {
            return openai_error(
                StatusCode::BAD_REQUEST,
                "The requested local model is not active",
                "invalid_request_error",
            );
        }
        let (base_url, api_key) = match self.ensure_sidecar().await {
            Ok(target) => target,
            Err(failure) => {
                warn!(error = %failure.detail, "Could not serve local model request");
                if failure.kind != LocalModelErrorKind::Busy {
                    self.record_sidecar_failure(&active, &failure).await;
                }
                return openai_error(
                    StatusCode::SERVICE_UNAVAILABLE,
                    failure.safe_message,
                    "service_unavailable",
                );
            }
        };
        match self
            .sidecar_client
            .post(format!("{base_url}/v1/chat/completions"))
            .bearer_auth(api_key)
            .json(&body)
            .send()
            .await
        {
            Ok(response) => {
                proxy_sidecar_response(response, permit, self.clone(), active)
            }
            Err(error) => {
                warn!(error = %error, "Local model request failed");
                let failure = LocalFailure::new(
                    LocalModelErrorKind::RuntimeUnavailable,
                    "本地模型已停止响应，正在等待恢复。",
                    error.to_string(),
                );
                self.record_sidecar_failure(&active, &failure).await;
                self.stop_sidecar().await;
                openai_error(
                    StatusCode::BAD_GATEWAY,
                    "The local model stopped responding",
                    "service_unavailable",
                )
            }
        }
    }
}

/// Curated metadata without constructing the local control plane.
pub fn local_model_catalog() -> Vec<LocalModelCatalogEntry> {
    built_in_catalog()
        .into_iter()
        .map(|model| model.entry)
        .collect()
}

/// Fresh-install status used before the lazy local runtime is initialized.
pub fn inactive_local_model_status() -> LocalModelServiceStatus {
    let runtime_artifact = runtime_artifact();
    LocalModelServiceStatus {
        kind: ManagedModelServiceKind::Local,
        protocol_version: MANAGED_MODEL_PROTOCOL_VERSION.to_owned(),
        provider_id: LOCAL_MODEL_PROVIDER_ID.to_owned(),
        enabled: false,
        ready: false,
        active_model_id: None,
        runtime: LocalRuntimeStatus {
            version: runtime_artifact.map(|_| RUNTIME_VERSION.to_owned()),
            backend: runtime_artifact.map(|asset| asset.backend),
            phase: LocalModelRuntimePhase::Stopped,
            error_kind: runtime_artifact
                .is_none()
                .then_some(LocalModelErrorKind::UnsupportedPlatform),
            message: runtime_artifact
                .is_none()
                .then_some("当前系统暂不支持一键本地模型运行。".to_owned()),
        },
        models: local_model_catalog()
            .into_iter()
            .map(|model| LocalModelState {
                model_id: model.id,
                install_phase: LocalModelInstallPhase::NotInstalled,
                progress: None,
                installed_bytes: 0,
                runtime_phase: LocalModelRuntimePhase::Stopped,
                error_kind: None,
                message: None,
            })
            .collect(),
        last_error: None,
    }
}

async fn local_models(State(state): State<LocalFacadeState>, headers: HeaderMap) -> Response {
    if !authorized(&headers, &state.auth_token) {
        return openai_error(
            StatusCode::UNAUTHORIZED,
            "Unauthorized",
            "authentication_error",
        );
    }
    let status = state.service.status().await;
    let data = status
        .active_model_id
        .into_iter()
        .map(|id| {
            json!({
                "id": id,
                "object": "model",
                "created": 0,
                "owned_by": LOCAL_MODEL_PROVIDER_ID,
            })
        })
        .collect::<Vec<_>>();
    Json(json!({"object": "list", "data": data})).into_response()
}

async fn local_chat(
    State(state): State<LocalFacadeState>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    if !authorized(&headers, &state.auth_token) {
        return openai_error(
            StatusCode::UNAUTHORIZED,
            "Unauthorized",
            "authentication_error",
        );
    }
    state.service.proxy_chat(body).await
}

fn proxy_sidecar_response(
    response: reqwest::Response,
    permit: OwnedSemaphorePermit,
    service: Arc<LocalModelService>,
    model_id: String,
) -> Response {
    let status = StatusCode::from_u16(response.status().as_u16())
        .unwrap_or(StatusCode::BAD_GATEWAY);
    let content_type = response.headers().get(header::CONTENT_TYPE).cloned();
    let mut builder = Response::builder().status(status);
    if let Some(content_type) = content_type {
        builder = builder.header(header::CONTENT_TYPE, content_type);
    }
    builder = builder
        .header(header::CACHE_CONTROL, "no-cache")
        .header("x-accel-buffering", "no");
    let permit = Arc::new(permit);
    let failure_recorded = Arc::new(AtomicBool::new(false));
    let stream = response.bytes_stream().then(move |chunk| {
        let permit = permit.clone();
        let service = service.clone();
        let model_id = model_id.clone();
        let failure_recorded = failure_recorded.clone();
        async move {
            let _keep_permit_alive = permit;
            if let Err(error) = &chunk
                && failure_recorded
                    .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
                    .is_ok()
            {
                let failure = LocalFailure::new(
                    LocalModelErrorKind::RuntimeUnavailable,
                    "本地模型流式响应中断，正在等待恢复。",
                    error.to_string(),
                );
                service.record_sidecar_failure(&model_id, &failure).await;
                service.stop_sidecar().await;
            }
            chunk
        }
    });
    builder
        .body(Body::from_stream(stream))
        .unwrap_or_else(|_| {
            openai_error(
                StatusCode::BAD_GATEWAY,
                "Invalid local model response",
                "service_unavailable",
            )
        })
}

fn openai_error(status: StatusCode, message: &str, error_type: &str) -> Response {
    (
        status,
        Json(json!({
            "error": {
                "message": message,
                "type": error_type,
                "code": Value::Null,
                "param": Value::Null,
            }
        })),
    )
        .into_response()
}

fn authorized(headers: &HeaderMap, expected: &str) -> bool {
    headers
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
        == Some(expected)
}

fn local_download_client() -> reqwest::Client {
    let build = || {
        reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(20))
            .read_timeout(Duration::from_secs(120))
            .redirect(reqwest::redirect::Policy::custom(|attempt| {
                if attempt.previous().len() >= 10 || !allowed_download_url(attempt.url()) {
                    attempt.stop()
                } else {
                    attempt.follow()
                }
            }))
    };
    nomifun_net::proxy::apply_detected_proxy(build())
        .build()
        .unwrap_or_else(|error| {
            warn!(error = %error, "Could not apply system proxy to local model downloader");
            build()
                .build()
                .expect("local model HTTP client configuration is valid")
        })
}

fn allowed_download_url(url: &reqwest::Url) -> bool {
    if url.scheme() != "https" {
        return false;
    }
    let Some(host) = url.host_str().map(|host| host.to_ascii_lowercase()) else {
        return false;
    };
    host == "huggingface.co"
        || host.ends_with(".huggingface.co")
        || host == "hf-mirror.com"
        || host.ends_with(".hf-mirror.com")
        || host == "hf.co"
        || host.ends_with(".hf.co")
        || host == "xethub.hf.co"
        || host.ends_with(".xethub.hf.co")
        || host == "github.com"
        || host.ends_with(".github.com")
        || host == "githubusercontent.com"
        || host.ends_with(".githubusercontent.com")
}

fn download_sources(url: &str) -> Vec<String> {
    let mut sources = vec![url.to_owned()];
    let Ok(mut mirror) = reqwest::Url::parse(url) else {
        return sources;
    };
    if mirror.host_str() == Some("huggingface.co")
        && mirror.set_host(Some("hf-mirror.com")).is_ok()
    {
        sources.push(mirror.into());
    }
    sources
}

fn loopback_download_url(url: &reqwest::Url) -> bool {
    matches!(url.scheme(), "http" | "https")
        && url
            .host_str()
            .is_some_and(|host| matches!(host, "localhost" | "127.0.0.1" | "::1"))
}

/// Parse `bytes START-END/TOTAL`, rejecting wildcard or internally
/// inconsistent responses.
fn parse_content_range(value: &str) -> Option<(u64, u64, u64)> {
    let value = value.strip_prefix("bytes ")?;
    let (range, total) = value.split_once('/')?;
    let (start, end) = range.split_once('-')?;
    let start = start.parse::<u64>().ok()?;
    let end = end.parse::<u64>().ok()?;
    let total = total.parse::<u64>().ok()?;
    (start <= end && end < total).then_some((start, end, total))
}

async fn ensure_disk_space(path: PathBuf, remaining: u64) -> Result<(), LocalFailure> {
    let available = tokio::task::spawn_blocking(move || fs2::available_space(path))
        .await
        .map_err(|error| {
            LocalFailure::new(
                LocalModelErrorKind::Unknown,
                "无法检查磁盘剩余空间。",
                error.to_string(),
            )
        })?
        .map_err(|error| {
            LocalFailure::new(
                LocalModelErrorKind::Unknown,
                "无法检查磁盘剩余空间。",
                error.to_string(),
            )
        })?;
    let required = remaining
        .saturating_add(remaining / 5)
        .saturating_add(DISK_SAFETY_BYTES);
    if available < required {
        return Err(LocalFailure::new(
            LocalModelErrorKind::InsufficientSpace,
            "磁盘空间不足，请释放空间后重试。",
            format!("available {available}, required {required}"),
        ));
    }
    Ok(())
}

async fn hash_file(path: &Path) -> Result<String, LocalFailure> {
    hash_file_inner(path, None).await
}

async fn hash_file_cancellable(
    path: &Path,
    cancel: &CancellationToken,
) -> Result<String, LocalFailure> {
    hash_file_inner(path, Some(cancel.clone())).await
}

async fn hash_file_inner(
    path: &Path,
    cancel: Option<CancellationToken>,
) -> Result<String, LocalFailure> {
    let path = path.to_path_buf();
    let result = tokio::task::spawn_blocking(move || {
        let mut file = File::open(&path)?;
        let mut hasher = Sha256::new();
        let mut buffer = vec![0_u8; 1024 * 1024];
        loop {
            if cancel.as_ref().is_some_and(CancellationToken::is_cancelled) {
                return Ok(None);
            }
            let read = file.read(&mut buffer)?;
            if read == 0 {
                break;
            }
            hasher.update(&buffer[..read]);
        }
        Ok::<_, std::io::Error>(Some(hex::encode(hasher.finalize())))
    })
    .await
    .map_err(|error| {
        LocalFailure::new(
            LocalModelErrorKind::Unknown,
            "无法校验下载文件。",
            error.to_string(),
        )
    })?
    .map_err(|error| {
        LocalFailure::new(
            LocalModelErrorKind::Unknown,
            "无法校验下载文件。",
            error.to_string(),
        )
    })?;
    result.ok_or_else(LocalFailure::cancelled)
}

async fn remove_file_if_exists(path: &Path) -> Result<(), AppError> {
    match tokio::fs::remove_file(path).await {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(AppError::Internal(format!("remove local model file: {error}"))),
    }
}

async fn remove_file_if_exists_failure(path: &Path) -> Result<(), LocalFailure> {
    match tokio::fs::remove_file(path).await {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(LocalFailure::new(
            LocalModelErrorKind::Unknown,
            "无法清理旧的下载文件。",
            error.to_string(),
        )),
    }
}

fn extract_archive(
    archive_path: &Path,
    destination: &Path,
    kind: ArchiveKind,
) -> Result<(), std::io::Error> {
    match kind {
        ArchiveKind::Zip => {
            let file = File::open(archive_path)?;
            let mut archive = zip::ZipArchive::new(file)
                .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))?;
            for index in 0..archive.len() {
                let mut entry = archive
                    .by_index(index)
                    .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))?;
                let relative = entry.enclosed_name().ok_or_else(|| {
                    std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        "unsafe path in runtime archive",
                    )
                })?;
                let output = destination.join(relative);
                if entry.is_dir() {
                    std::fs::create_dir_all(&output)?;
                    continue;
                }
                if let Some(parent) = output.parent() {
                    std::fs::create_dir_all(parent)?;
                }
                let mut output_file = File::create(&output)?;
                std::io::copy(&mut entry, &mut output_file)?;
                output_file.sync_all()?;
                #[cfg(unix)]
                if let Some(mode) = entry.unix_mode() {
                    use std::os::unix::fs::PermissionsExt;
                    std::fs::set_permissions(&output, std::fs::Permissions::from_mode(mode))?;
                }
            }
        }
        ArchiveKind::TarGz => {
            let file = File::open(archive_path)?;
            let decoder = flate2::read::GzDecoder::new(file);
            let mut archive = tar::Archive::new(decoder);
            let mut pending_symlinks = Vec::<(PathBuf, PathBuf)>::new();
            for entry in archive.entries()? {
                let mut entry = entry?;
                let path = entry.path()?.into_owned();
                if !safe_relative_path(&path) {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        "unsafe path in runtime archive",
                    ));
                }
                let kind = entry.header().entry_type();
                if kind.is_symlink() {
                    let target = entry.link_name()?.ok_or_else(|| {
                        std::io::Error::new(
                            std::io::ErrorKind::InvalidData,
                            "runtime symlink has no target",
                        )
                    })?;
                    let resolved = resolve_archive_link(&path, &target).ok_or_else(|| {
                        std::io::Error::new(
                            std::io::ErrorKind::InvalidData,
                            "runtime symlink escapes destination",
                        )
                    })?;
                    // Materialize safe links as regular copies after all
                    // ordinary entries are extracted. This supports official
                    // llama.cpp dylib/so link chains without ever creating a
                    // filesystem link an attacker could redirect later.
                    pending_symlinks.push((path, resolved));
                    continue;
                }
                if kind.is_hard_link() {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        "hard links are not allowed in runtime archive",
                    ));
                }
                if !entry.unpack_in(destination)? {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        "runtime archive entry escaped destination",
                    ));
                }
            }
            let mut remaining = pending_symlinks;
            while !remaining.is_empty() {
                let before = remaining.len();
                let mut unresolved = Vec::new();
                for (link, target) in remaining {
                    let source = destination.join(&target);
                    let output = destination.join(&link);
                    if source.is_file() {
                        if let Some(parent) = output.parent() {
                            std::fs::create_dir_all(parent)?;
                        }
                        std::fs::copy(&source, &output)?;
                        std::fs::OpenOptions::new()
                            .write(true)
                            .open(&output)?
                            .sync_all()?;
                    } else {
                        unresolved.push((link, target));
                    }
                }
                if unresolved.len() == before {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        "runtime symlink target is missing or not a file",
                    ));
                }
                remaining = unresolved;
            }
        }
    }
    Ok(())
}

fn safe_relative_path(path: &Path) -> bool {
    !path.is_absolute()
        && path.components().all(|component| {
            matches!(component, Component::Normal(_) | Component::CurDir)
        })
}

fn unsafe_link_or_reparse(metadata: &std::fs::Metadata) -> bool {
    if metadata.file_type().is_symlink() {
        return true;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if metadata.is_file() && metadata.nlink() > 1 {
            return true;
        }
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x0000_0400;
        return metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0;
    }
    #[cfg(not(windows))]
    false
}

fn prepare_managed_directory(root: &Path, directory: &Path) -> std::io::Result<()> {
    let relative = directory.strip_prefix(root).map_err(|_| {
        std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "managed directory escaped local AI root",
        )
    })?;
    if !safe_relative_path(relative) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "managed directory has an unsafe relative path",
        ));
    }

    std::fs::create_dir_all(root)?;
    let root_metadata = std::fs::symlink_metadata(root)?;
    if unsafe_link_or_reparse(&root_metadata) || !root_metadata.is_dir() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "local AI root is a link or not a directory",
        ));
    }
    let canonical_root = std::fs::canonicalize(root)?;
    let mut current = root.to_path_buf();
    for component in relative.components() {
        let Component::Normal(part) = component else {
            continue;
        };
        current.push(part);
        match std::fs::symlink_metadata(&current) {
            Ok(metadata) => {
                if unsafe_link_or_reparse(&metadata) || !metadata.is_dir() {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::PermissionDenied,
                        "managed path ancestor is a link or not a directory",
                    ));
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                std::fs::create_dir(&current)?;
                let metadata = std::fs::symlink_metadata(&current)?;
                if unsafe_link_or_reparse(&metadata) || !metadata.is_dir() {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::PermissionDenied,
                        "managed directory creation was redirected",
                    ));
                }
            }
            Err(error) => return Err(error),
        }
    }
    let canonical_directory = std::fs::canonicalize(&current)?;
    if !canonical_directory.starts_with(&canonical_root) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "managed directory resolved outside local AI root",
        ));
    }
    Ok(())
}

fn prepare_managed_file(root: &Path, path: &Path) -> std::io::Result<()> {
    let relative = path.strip_prefix(root).map_err(|_| {
        std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "managed file escaped local AI root",
        )
    })?;
    if relative.as_os_str().is_empty() || !safe_relative_path(relative) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "managed file has an unsafe relative path",
        ));
    }
    let parent = path.parent().ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "managed file has no parent",
        )
    })?;
    prepare_managed_directory(root, parent)?;
    match std::fs::symlink_metadata(path) {
        Ok(metadata) => {
            if unsafe_link_or_reparse(&metadata) || !metadata.is_file() {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::PermissionDenied,
                    "managed file target is a link or not a regular file",
                ));
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    Ok(())
}

fn resolve_archive_link(link_path: &Path, target: &Path) -> Option<PathBuf> {
    if target.is_absolute() || !safe_relative_path(link_path) {
        return None;
    }
    let combined = link_path.parent().unwrap_or_else(|| Path::new("")).join(target);
    let mut normalized = PathBuf::new();
    for component in combined.components() {
        match component {
            Component::Normal(part) => normalized.push(part),
            Component::CurDir => {}
            Component::ParentDir => {
                if !normalized.pop() {
                    return None;
                }
            }
            Component::RootDir | Component::Prefix(_) => return None,
        }
    }
    (!normalized.as_os_str().is_empty()).then_some(normalized)
}

fn configured_runtime_path() -> Option<PathBuf> {
    let path = std::env::var_os("NOMIFUN_LLAMA_SERVER_PATH")?;
    let path = PathBuf::from(path);
    path.is_file().then_some(path)
}

fn find_runtime_executable(root: &Path) -> Option<PathBuf> {
    let target = if cfg!(windows) {
        "llama-server.exe"
    } else {
        "llama-server"
    };
    fn visit(path: &Path, target: &str, depth: usize) -> Option<PathBuf> {
        if depth > 5 {
            return None;
        }
        for entry in std::fs::read_dir(path).ok()? {
            let entry = entry.ok()?;
            let file_type = entry.file_type().ok()?;
            if file_type.is_symlink() {
                continue;
            }
            let child = entry.path();
            if file_type.is_file() && entry.file_name() == target {
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    let mut permissions = entry.metadata().ok()?.permissions();
                    permissions.set_mode(permissions.mode() | 0o700);
                    std::fs::set_permissions(&child, permissions).ok()?;
                }
                return Some(child);
            }
            if file_type.is_dir()
                && let Some(found) = visit(&child, target, depth + 1)
            {
                return Some(found);
            }
        }
        None
    }
    visit(root, target, 0)
}

fn generate_token() -> Result<String, String> {
    let mut bytes = [0_u8; 32];
    getrandom::getrandom(&mut bytes)
        .map_err(|error| format!("secure random generation failed: {error}"))?;
    Ok(hex::encode(bytes))
}

async fn write_private_key_file(path: PathBuf, token: String) -> Result<(), LocalFailure> {
    tokio::task::spawn_blocking(move || {
        match std::fs::remove_file(&path) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
        let mut options = std::fs::OpenOptions::new();
        options.create_new(true).write(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(path)?;
        use std::io::Write as _;
        writeln!(file, "{token}")?;
        file.sync_all()
    })
    .await
    .map_err(|error| {
        LocalFailure::new(
            LocalModelErrorKind::RuntimeUnavailable,
            "无法创建本地模型认证文件。",
            error.to_string(),
        )
    })?
    .map_err(|error| {
        LocalFailure::new(
            LocalModelErrorKind::RuntimeUnavailable,
            "无法创建本地模型认证文件。",
            error.to_string(),
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use nomifun_db::{SqliteProviderRepository, init_database_memory};
    use tempfile::TempDir;
    use wiremock::matchers::{header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn sha256(bytes: &[u8]) -> String {
        hex::encode(Sha256::digest(bytes))
    }

    async fn test_service(temp: &TempDir) -> Arc<LocalModelService> {
        let root = temp.path().join("local-ai");
        tokio::fs::create_dir_all(root.join("models/test-model"))
            .await
            .unwrap();
        tokio::fs::create_dir_all(root.join("runtime")).await.unwrap();
        tokio::fs::create_dir_all(root.join("downloads")).await.unwrap();
        let db = init_database_memory().await.unwrap();
        let provider_repo: Arc<dyn IProviderRepository> =
            Arc::new(SqliteProviderRepository::new(db.pool().clone()));
        let catalog = vec![ModelArtifact {
            entry: LocalModelCatalogEntry {
                id: "test-model".into(),
                name: "Test".into(),
                description: "Test".into(),
                parameter_size: "tiny".into(),
                quantization: "Q4_K_M".into(),
                download_size_bytes: 0,
                required_memory_bytes: 0,
                context_window: 4096,
                license: "MIT".into(),
                source: "test".into(),
                recommended: true,
                tasks: vec![ModelTask::Chat],
                traits: vec![],
            },
            model_size_bytes: 0,
            file_name: "model.gguf",
            url: "https://example.invalid/model.gguf",
            sha256: "00",
            vision_projector: None,
        }];
        let models = HashMap::from([(
            "test-model".into(),
            LocalModelState {
                model_id: "test-model".into(),
                install_phase: LocalModelInstallPhase::NotInstalled,
                progress: None,
                installed_bytes: 0,
                runtime_phase: LocalModelRuntimePhase::Stopped,
                error_kind: None,
                message: None,
            },
        )]);
        Arc::new(LocalModelService {
            root,
            provider_repo,
            http_client: reqwest::Client::new(),
            sidecar_client: reqwest::Client::new(),
            catalog,
            runtime_artifact: None,
            allow_insecure_loopback_downloads: true,
            state: Mutex::new(MutableState {
                persisted: PersistedState {
                    version: STATE_VERSION,
                    ..Default::default()
                },
                models,
                verified_models: HashSet::new(),
                runtime: LocalRuntimeStatus {
                    version: None,
                    backend: None,
                    phase: LocalModelRuntimePhase::Stopped,
                    error_kind: None,
                    message: None,
                },
                download: None,
                sidecar: None,
                restart_failures: 0,
                restart_not_before: None,
                last_error: None,
            }),
            mutation_lock: Mutex::new(()),
            persist_lock: Mutex::new(()),
            verification_lock: Mutex::new(()),
            start_lock: Mutex::new(()),
            inference_gate: Arc::new(Semaphore::new(1)),
            projection: RwLock::new(None),
            auxiliary_projection_models: RwLock::new(HashMap::new()),
            projection_sync_lock: Mutex::new(()),
        })
    }

    #[test]
    fn catalog_is_small_curated_and_pinned() {
        let catalog = built_in_catalog();
        assert_eq!(catalog.len(), 2);
        assert_eq!(catalog.iter().filter(|model| model.entry.recommended).count(), 1);
        for (retired_id, _) in RETIRED_MODEL_ARTIFACTS {
            assert!(catalog.iter().all(|model| model.entry.id != retired_id));
        }
        for model in catalog {
            let parameter_billions = model
                .entry
                .parameter_size
                .trim_end_matches('B')
                .parse::<f32>()
                .unwrap();
            assert!(parameter_billions >= 4.0);
            assert!(model.entry.download_size_bytes <= 7_000_000_000);
            assert_eq!(model.entry.context_window, 65_536);
            assert_eq!(model.entry.quantization, "Q4_K_M");
            assert_eq!(model.entry.traits, vec![ModelTrait::VisionInput]);
            assert_eq!(model.sha256.len(), 64);
            let projector = model.vision_projector.expect("vision projector");
            assert_eq!(projector.sha256.len(), 64);
            assert_eq!(
                model.entry.download_size_bytes,
                model.model_size_bytes + projector.size
            );
            assert!(model.url.contains("/resolve/"));
        }
    }

    #[tokio::test]
    async fn auxiliary_projection_is_selectable_only_while_installed() {
        let temp = TempDir::new().unwrap();
        let service = test_service(&temp).await;
        service
            .set_projection("http://127.0.0.1:1/v1".into(), "encrypted".into())
            .await;

        service
            .set_auxiliary_model_projection(
                "z-image-turbo-q3-k",
                "Local text-to-image generation",
                true,
            )
            .await
            .unwrap();
        let installed = service
            .provider_repo
            .find_by_id(LOCAL_MODEL_PROVIDER_ID)
            .await
            .unwrap()
            .unwrap();
        assert!(installed.enabled);
        assert_eq!(
            serde_json::from_str::<Vec<String>>(&installed.models).unwrap(),
            vec!["z-image-turbo-q3-k"]
        );

        service
            .set_auxiliary_model_projection(
                "z-image-turbo-q3-k",
                "Local text-to-image generation",
                false,
            )
            .await
            .unwrap();
        let removed = service
            .provider_repo
            .find_by_id(LOCAL_MODEL_PROVIDER_ID)
            .await
            .unwrap()
            .unwrap();
        assert!(!removed.enabled);
        assert_eq!(removed.models, "[]");
    }

    #[tokio::test]
    async fn retired_model_cleanup_removes_only_known_artifacts() {
        let temp = TempDir::new().unwrap();
        let root = temp.path().join("local-ai");

        for (model_id, file_name) in RETIRED_MODEL_ARTIFACTS {
            let directory = root.join("models").join(model_id);
            tokio::fs::create_dir_all(&directory).await.unwrap();
            let model = directory.join(file_name);
            tokio::fs::write(&model, b"retired model").await.unwrap();
            tokio::fs::write(partial_path(&model), b"partial retired model")
                .await
                .unwrap();
        }
        let preserved = root
            .join("models/qwen3-0.6b-q4-k-m")
            .join("user-note.txt");
        tokio::fs::write(&preserved, b"preserve me").await.unwrap();

        cleanup_retired_model_artifacts(&root).await;
        cleanup_retired_model_artifacts(&root).await;

        for (model_id, file_name) in RETIRED_MODEL_ARTIFACTS {
            let model = root.join("models").join(model_id).join(file_name);
            assert!(!model.exists());
            assert!(!partial_path(&model).exists());
        }
        assert_eq!(tokio::fs::read(preserved).await.unwrap(), b"preserve me");
        assert!(!root.join("models/qwen3-1.7b-q4-k-m").exists());
        assert!(!root.join("models/qwen3-4b-q4-k-m").exists());
    }

    #[test]
    fn content_range_parser_is_strict() {
        assert_eq!(parse_content_range("bytes 100-199/1000"), Some((100, 199, 1000)));
        assert_eq!(parse_content_range("bytes 200-100/1000"), None);
        assert_eq!(parse_content_range("bytes 0-100/*"), None);
        assert_eq!(parse_content_range("items 0-100/1000"), None);
        assert_eq!(parse_content_range("bytes 0-100/100"), None);
    }

    #[test]
    fn local_chat_defaults_disable_thinking_without_overriding_callers() {
        let defaulted = apply_local_chat_defaults(json!({
            "model": "qwen3-5-4b-q4-k-m"
        }));
        assert_eq!(
            defaulted["chat_template_kwargs"]["enable_thinking"],
            false
        );

        let augmented = apply_local_chat_defaults(json!({
            "model": "qwen3-5-4b-q4-k-m",
            "chat_template_kwargs": {"reasoning_format": "none"}
        }));
        assert_eq!(augmented["chat_template_kwargs"]["enable_thinking"], false);
        assert_eq!(
            augmented["chat_template_kwargs"]["reasoning_format"],
            "none"
        );

        let explicit = apply_local_chat_defaults(json!({
            "model": "qwen3-5-4b-q4-k-m",
            "chat_template_kwargs": {"enable_thinking": true}
        }));
        assert_eq!(explicit["chat_template_kwargs"]["enable_thinking"], true);
    }

    #[test]
    fn download_allowlist_requires_https_and_known_hosts() {
        assert!(allowed_download_url(
            &reqwest::Url::parse("https://huggingface.co/a/b").unwrap()
        ));
        assert!(allowed_download_url(
            &reqwest::Url::parse("https://hf-mirror.com/a/b").unwrap()
        ));
        assert!(allowed_download_url(
            &reqwest::Url::parse("https://release-assets.githubusercontent.com/a").unwrap()
        ));
        assert!(!allowed_download_url(
            &reqwest::Url::parse("http://huggingface.co/a").unwrap()
        ));
        assert!(!allowed_download_url(
            &reqwest::Url::parse("https://huggingface.co.evil.example/a").unwrap()
        ));
        assert!(!allowed_download_url(
            &reqwest::Url::parse("https://hf-mirror.com.evil.example/a").unwrap()
        ));
    }

    #[test]
    fn hugging_face_downloads_have_a_pinned_mirror_fallback() {
        assert_eq!(
            download_sources("https://huggingface.co/org/repo/resolve/revision/model.gguf"),
            vec![
                "https://huggingface.co/org/repo/resolve/revision/model.gguf",
                "https://hf-mirror.com/org/repo/resolve/revision/model.gguf",
            ]
        );
        assert_eq!(
            download_sources("https://github.com/org/repo/archive/runtime.zip"),
            vec!["https://github.com/org/repo/archive/runtime.zip"]
        );
    }

    #[test]
    fn partial_file_stays_next_to_final_file() {
        let final_path = Path::new("models/example/model.gguf");
        assert_eq!(
            partial_path(final_path),
            PathBuf::from("models/example/model.gguf.part")
        );
    }

    #[test]
    fn managed_paths_reject_files_outside_local_ai_root() {
        let temp = TempDir::new().unwrap();
        let root = temp.path().join("local-ai");
        prepare_managed_directory(&root, &root).unwrap();
        assert!(prepare_managed_file(&root, &root.join("models/model.gguf")).is_ok());
        assert!(prepare_managed_file(&root, &temp.path().join("outside.gguf")).is_err());
    }

    #[test]
    #[cfg(unix)]
    fn managed_paths_reject_hard_linked_files() {
        let temp = TempDir::new().unwrap();
        let root = temp.path().join("local-ai");
        let outside = temp.path().join("outside.bin");
        prepare_managed_directory(&root, &root).unwrap();
        std::fs::write(&outside, b"outside").unwrap();
        let linked = root.join("linked.bin");
        std::fs::hard_link(&outside, &linked).unwrap();
        assert!(prepare_managed_file(&root, &linked).is_err());
    }

    #[test]
    #[cfg(any(unix, windows))]
    fn managed_paths_reject_linked_ancestors() {
        let temp = TempDir::new().unwrap();
        let root = temp.path().join("local-ai");
        let outside = temp.path().join("outside");
        prepare_managed_directory(&root, &root).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        let linked_models = root.join("models");
        #[cfg(unix)]
        std::os::unix::fs::symlink(&outside, &linked_models).unwrap();
        #[cfg(windows)]
        if std::os::windows::fs::symlink_dir(&outside, &linked_models).is_err() {
            // Creating symlinks requires Developer Mode or elevated rights on
            // some Windows CI hosts. The reparse-point branch is still built.
            return;
        }

        assert!(prepare_managed_file(&root, &linked_models.join("model.gguf")).is_err());
    }

    #[test]
    fn runtime_stamp_rejects_truncated_and_deleted_files() {
        let temp = TempDir::new().unwrap();
        let runtime_dir = runtime_dir_at(temp.path());
        std::fs::create_dir_all(&runtime_dir).unwrap();
        let executable = runtime_dir.join(if cfg!(windows) {
            "llama-server.exe"
        } else {
            "llama-server"
        });
        let support = runtime_dir.join("runtime-support.bin");
        let support_bytes = b"runtime support payload";
        std::fs::write(&executable, b"test executable").unwrap();
        std::fs::write(&support, support_bytes).unwrap();
        let artifact = RuntimeArtifact {
            url: "https://example.invalid/runtime.zip",
            sha256: "test-archive-sha",
            size: 1,
            archive_name: "runtime.zip",
            archive_kind: ArchiveKind::Zip,
            backend: LocalModelRuntimeBackend::Cpu,
        };

        write_runtime_stamp(temp.path(), &artifact).unwrap();
        assert!(managed_runtime_executable(temp.path(), &artifact).is_some());

        std::fs::write(&support, b"x").unwrap();
        assert!(managed_runtime_executable(temp.path(), &artifact).is_none());

        std::fs::write(&support, support_bytes).unwrap();
        assert!(managed_runtime_executable(temp.path(), &artifact).is_some());

        std::fs::remove_file(&support).unwrap();
        assert!(managed_runtime_executable(temp.path(), &artifact).is_none());
    }

    #[test]
    fn restart_backoff_is_exponential_and_bounded() {
        assert_eq!(restart_backoff(1), Duration::from_secs(2));
        assert_eq!(restart_backoff(2), Duration::from_secs(4));
        assert_eq!(restart_backoff(3), Duration::from_secs(8));
        assert_eq!(restart_backoff(6), Duration::from_secs(60));
        assert_eq!(restart_backoff(100), Duration::from_secs(60));
    }

    #[tokio::test]
    async fn sidecar_failure_enters_cooldown_and_failed_state() {
        let temp = TempDir::new().unwrap();
        let service = test_service(&temp).await;
        {
            let mut state = service.state.lock().await;
            state.persisted.active_model_id = Some("test-model".into());
            state.models.get_mut("test-model").unwrap().install_phase =
                LocalModelInstallPhase::Installed;
        }
        let failure = LocalFailure::new(
            LocalModelErrorKind::RuntimeUnavailable,
            "本地模型启动失败。",
            "test sidecar failure",
        );
        service
            .record_sidecar_failure("test-model", &failure)
            .await;
        {
            let state = service.state.lock().await;
            assert_eq!(state.restart_failures, 1);
            assert!(state.restart_not_before.is_some_and(|time| time > Instant::now()));
            assert_eq!(state.runtime.phase, LocalModelRuntimePhase::Failed);
            assert_eq!(
                state.models["test-model"].runtime_phase,
                LocalModelRuntimePhase::Failed
            );
        }
        assert_eq!(
            service.ensure_sidecar().await.unwrap_err().kind,
            LocalModelErrorKind::Busy
        );
    }

    #[tokio::test]
    async fn activation_projection_failure_rolls_back_selection() {
        let temp = TempDir::new().unwrap();
        let service = test_service(&temp).await;
        service
            .state
            .lock()
            .await
            .models
            .get_mut("test-model")
            .unwrap()
            .install_phase = LocalModelInstallPhase::Installed;

        assert!(service.activate_model("test-model").await.is_err());
        let state = service.state.lock().await;
        assert_eq!(state.persisted.active_model_id, None);
        assert_eq!(state.runtime.phase, LocalModelRuntimePhase::Failed);
        assert_eq!(
            state.models["test-model"].runtime_phase,
            LocalModelRuntimePhase::Failed
        );
        drop(state);
        let persisted: PersistedState = serde_json::from_slice(
            &tokio::fs::read(service.root.join("state.json"))
                .await
                .unwrap(),
        )
        .unwrap();
        assert_eq!(persisted.active_model_id, None);
    }

    #[test]
    fn archive_links_must_resolve_inside_staging_root() {
        assert_eq!(
            resolve_archive_link(Path::new("lib/libalias.so"), Path::new("libreal.so")),
            Some(PathBuf::from("lib/libreal.so"))
        );
        assert_eq!(
            resolve_archive_link(Path::new("build/bin/tool"), Path::new("../../lib/tool")),
            Some(PathBuf::from("lib/tool"))
        );
        assert_eq!(
            resolve_archive_link(Path::new("lib/libalias.so"), Path::new("../../outside")),
            None
        );
        assert_eq!(
            resolve_archive_link(Path::new("lib/libalias.so"), Path::new("/outside")),
            None
        );
    }

    #[test]
    fn tar_runtime_symlinks_are_materialized_as_safe_files() {
        let temp = TempDir::new().unwrap();
        let archive_path = temp.path().join("runtime.tar.gz");
        let file = File::create(&archive_path).unwrap();
        let encoder = flate2::write::GzEncoder::new(file, flate2::Compression::fast());
        let mut builder = tar::Builder::new(encoder);

        let payload = b"signed-library-bytes";
        let mut file_header = tar::Header::new_gnu();
        file_header.set_size(payload.len() as u64);
        file_header.set_mode(0o755);
        file_header.set_uid(0);
        file_header.set_gid(0);
        file_header.set_mtime(0);
        file_header.set_cksum();
        builder
            .append_data(&mut file_header, "lib/libreal.so", &payload[..])
            .unwrap();

        for (name, target) in [
            ("lib/libalias.so.0", "libreal.so"),
            ("lib/libalias.so", "libalias.so.0"),
        ] {
            let mut link_header = tar::Header::new_gnu();
            link_header.set_entry_type(tar::EntryType::Symlink);
            link_header.set_size(0);
            link_header.set_mode(0o777);
            link_header.set_uid(0);
            link_header.set_gid(0);
            link_header.set_mtime(0);
            link_header.set_link_name(target).unwrap();
            link_header.set_cksum();
            builder
                .append_data(&mut link_header, name, std::io::empty())
                .unwrap();
        }
        let encoder = builder.into_inner().unwrap();
        encoder.finish().unwrap();

        let destination = temp.path().join("out");
        std::fs::create_dir_all(&destination).unwrap();
        extract_archive(&archive_path, &destination, ArchiveKind::TarGz).unwrap();
        for name in ["lib/libreal.so", "lib/libalias.so.0", "lib/libalias.so"] {
            let path = destination.join(name);
            assert!(path.is_file());
            assert!(!path.symlink_metadata().unwrap().file_type().is_symlink());
            assert_eq!(std::fs::read(path).unwrap(), payload);
        }
    }

    #[tokio::test]
    async fn downloader_streams_verifies_and_commits_200_response() {
        let server = MockServer::start().await;
        let bytes = b"tiny verified gguf".to_vec();
        Mock::given(method("GET"))
            .and(path("/model"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(bytes.clone()))
            .mount(&server)
            .await;
        let temp = TempDir::new().unwrap();
        let service = test_service(&temp).await;
        let destination = temp.path().join("local-ai/models/test-model/model.gguf");
        service
            .download_verified(
                &format!("{}/model", server.uri()),
                &sha256(&bytes),
                bytes.len() as u64,
                &destination,
                "test-model",
                LocalModelProgressComponent::Model,
                &CancellationToken::new(),
            )
            .await
            .unwrap();
        assert_eq!(tokio::fs::read(&destination).await.unwrap(), bytes);
        assert!(!partial_path(&destination).exists());
    }

    #[tokio::test]
    async fn downloader_resumes_only_with_matching_content_range() {
        let bytes = b"0123456789abcdef".to_vec();
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/model"))
            .and(header("range", "bytes=5-"))
            .respond_with(
                ResponseTemplate::new(206)
                    .insert_header("content-range", "bytes 5-15/16")
                    .set_body_bytes(bytes[5..].to_vec()),
            )
            .mount(&server)
            .await;
        let temp = TempDir::new().unwrap();
        let service = test_service(&temp).await;
        let destination = temp.path().join("local-ai/models/test-model/model.gguf");
        tokio::fs::write(partial_path(&destination), &bytes[..5])
            .await
            .unwrap();
        service
            .download_verified(
                &format!("{}/model", server.uri()),
                &sha256(&bytes),
                bytes.len() as u64,
                &destination,
                "test-model",
                LocalModelProgressComponent::Model,
                &CancellationToken::new(),
            )
            .await
            .unwrap();
        assert_eq!(tokio::fs::read(destination).await.unwrap(), bytes);
    }

    #[tokio::test]
    async fn downloader_restarts_safely_when_origin_ignores_range() {
        let bytes = b"complete response".to_vec();
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/model"))
            .and(header("range", "bytes=4-"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(bytes.clone()))
            .mount(&server)
            .await;
        let temp = TempDir::new().unwrap();
        let service = test_service(&temp).await;
        let destination = temp.path().join("local-ai/models/test-model/model.gguf");
        tokio::fs::write(partial_path(&destination), b"stale")
            .await
            .unwrap();
        // Match the actual stale length used in Range.
        Mock::given(method("GET"))
            .and(path("/model-ignored"))
            .and(header("range", "bytes=5-"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(bytes.clone()))
            .mount(&server)
            .await;
        service
            .download_verified(
                &format!("{}/model-ignored", server.uri()),
                &sha256(&bytes),
                bytes.len() as u64,
                &destination,
                "test-model",
                LocalModelProgressComponent::Model,
                &CancellationToken::new(),
            )
            .await
            .unwrap();
        assert_eq!(tokio::fs::read(destination).await.unwrap(), bytes);
    }

    #[tokio::test]
    async fn downloader_rejects_wrong_content_range_without_appending() {
        let bytes = b"0123456789".to_vec();
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/model"))
            .and(header("range", "bytes=3-"))
            .respond_with(
                ResponseTemplate::new(206)
                    .insert_header("content-range", "bytes 4-9/10")
                    .set_body_bytes(bytes[4..].to_vec()),
            )
            .mount(&server)
            .await;
        let temp = TempDir::new().unwrap();
        let service = test_service(&temp).await;
        let destination = temp.path().join("local-ai/models/test-model/model.gguf");
        tokio::fs::write(partial_path(&destination), &bytes[..3])
            .await
            .unwrap();
        let error = service
            .download_verified(
                &format!("{}/model", server.uri()),
                &sha256(&bytes),
                bytes.len() as u64,
                &destination,
                "test-model",
                LocalModelProgressComponent::Model,
                &CancellationToken::new(),
            )
            .await
            .unwrap_err();
        assert_eq!(error.kind, LocalModelErrorKind::Network);
        assert_eq!(tokio::fs::read(partial_path(&destination)).await.unwrap(), &bytes[..3]);
    }

    #[tokio::test]
    async fn downloader_removes_corrupt_completed_partial() {
        let bytes = b"corrupt bytes".to_vec();
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/model"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(bytes.clone()))
            .mount(&server)
            .await;
        let temp = TempDir::new().unwrap();
        let service = test_service(&temp).await;
        let destination = temp.path().join("local-ai/models/test-model/model.gguf");
        let error = service
            .download_verified(
                &format!("{}/model", server.uri()),
                &sha256(b"different bytes"),
                bytes.len() as u64,
                &destination,
                "test-model",
                LocalModelProgressComponent::Model,
                &CancellationToken::new(),
            )
            .await
            .unwrap_err();
        assert_eq!(error.kind, LocalModelErrorKind::ChecksumMismatch);
        assert!(!partial_path(&destination).exists());
        assert!(!destination.exists());
    }

    #[tokio::test]
    async fn pre_cancelled_download_preserves_existing_partial() {
        let temp = TempDir::new().unwrap();
        let service = test_service(&temp).await;
        let destination = temp.path().join("local-ai/models/test-model/model.gguf");
        tokio::fs::write(partial_path(&destination), b"resume-me")
            .await
            .unwrap();
        let cancel = CancellationToken::new();
        cancel.cancel();
        let error = service
            .download_verified(
                "http://127.0.0.1:1/never-requested",
                &sha256(b"resume-me-and-more"),
                18,
                &destination,
                "test-model",
                LocalModelProgressComponent::Model,
                &cancel,
            )
            .await
            .unwrap_err();
        assert_eq!(error.detail, "download cancelled by user");
        assert_eq!(
            tokio::fs::read(partial_path(&destination)).await.unwrap(),
            b"resume-me"
        );
    }

    #[tokio::test]
    async fn cancellation_interrupts_a_stalled_request() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/slow-model"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_delay(Duration::from_secs(5))
                    .set_body_bytes(b"eventual bytes"),
            )
            .mount(&server)
            .await;
        let temp = TempDir::new().unwrap();
        let service = test_service(&temp).await;
        let destination = temp.path().join("local-ai/models/test-model/model.gguf");
        let cancel = CancellationToken::new();
        let trigger = cancel.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(50)).await;
            trigger.cancel();
        });

        let error = tokio::time::timeout(
            Duration::from_secs(1),
            service.download_verified(
                &format!("{}/slow-model", server.uri()),
                &sha256(b"eventual bytes"),
                14,
                &destination,
                "test-model",
                LocalModelProgressComponent::Model,
                &cancel,
            ),
        )
        .await
        .expect("cancellation should not wait for the HTTP read timeout")
        .unwrap_err();
        assert_eq!(error.detail, "download cancelled by user");
    }

    #[tokio::test]
    async fn cancellation_interrupts_file_verification() {
        let temp = TempDir::new().unwrap();
        let artifact = temp.path().join("artifact.bin");
        tokio::fs::write(&artifact, b"verified bytes").await.unwrap();
        let cancel = CancellationToken::new();
        cancel.cancel();

        let error = hash_file_cancellable(&artifact, &cancel)
            .await
            .unwrap_err();
        assert_eq!(error.detail, "download cancelled by user");
    }

    #[tokio::test]
    #[ignore = "downloads about 3.42 GB and starts the real multimodal llama-server runtime"]
    async fn real_qwen_3_5_4b_install_and_streaming_smoke_test() {
        let temp = TempDir::new().unwrap();
        let db = init_database_memory().await.unwrap();
        let provider_repo: Arc<dyn IProviderRepository> =
            Arc::new(SqliteProviderRepository::new(db.pool().clone()));
        let (service, server) =
            start_and_provision_local_model(temp.path(), provider_repo, [7_u8; 32])
                .await
                .unwrap();
        let model_id = "qwen3-5-4b-q4-k-m";
        if let Some(seed) = std::env::var_os("NOMIFUN_LOCAL_MODEL_SMOKE_MODEL") {
            let artifact = service.artifact(model_id).unwrap();
            let destination = model_path_at(&service.root, &artifact);
            prepare_managed_file(&service.root, &destination).unwrap();
            tokio::fs::copy(seed, destination).await.unwrap();
        }

        service.install(model_id).await.unwrap();
        let install_result: Result<(), String> = async {
            let deadline = Instant::now() + Duration::from_secs(120 * 60);
            loop {
                let status = service.status().await;
                let model = status
                    .models
                    .iter()
                    .find(|model| model.model_id == model_id)
                    .ok_or_else(|| "smoke-test model disappeared from status".to_owned())?;
                if model.install_phase == LocalModelInstallPhase::Failed
                    || model.runtime_phase == LocalModelRuntimePhase::Failed
                {
                    return Err(format!(
                        "local model failed: {:?} / {:?}: {:?}",
                        model.install_phase, model.runtime_phase, model.message
                    ));
                }
                if model.install_phase == LocalModelInstallPhase::Installed
                    && model.runtime_phase == LocalModelRuntimePhase::Ready
                {
                    break;
                }
                if Instant::now() >= deadline {
                    return Err("real local-model smoke test timed out".into());
                }
                tokio::time::sleep(Duration::from_millis(500)).await;
            }

            let client = reqwest::Client::builder()
                .no_proxy()
                .timeout(Duration::from_secs(300))
                .build()
                .map_err(|error| error.to_string())?;
            let models = client
                .get(format!("{}/models", server.base_url()))
                .bearer_auth(server.auth_token())
                .send()
                .await
                .map_err(|error| error.to_string())?;
            if !models.status().is_success() {
                return Err(format!("/v1/models returned {}", models.status()));
            }
            let models_json: Value = models.json().await.map_err(|error| error.to_string())?;
            if models_json["data"][0]["id"] != model_id {
                return Err(format!("unexpected /v1/models response: {models_json}"));
            }

            let chat = client
                .post(format!("{}/chat/completions", server.base_url()))
                .bearer_auth(server.auth_token())
                .json(&json!({
                    "model": model_id,
                    "messages": [{"role": "user", "content": "只回答：你好"}],
                    "stream": true,
                    "max_tokens": 128,
                    "chat_template_kwargs": {"enable_thinking": false}
                }))
                .send()
                .await
                .map_err(|error| error.to_string())?;
            let status = chat.status();
            let body = chat.text().await.map_err(|error| error.to_string())?;
            if !status.is_success() || !body.contains("data:") || !body.contains("[DONE]") {
                return Err(format!("unexpected streaming response ({status}): {body}"));
            }
            Ok(())
        }
        .await;

        let _ = service.set_active(model_id, false).await;
        service.stop_sidecar().await;
        drop(server);
        install_result.unwrap();
    }
}

fn built_in_catalog() -> Vec<ModelArtifact> {
    vec![
        ModelArtifact {
            entry: LocalModelCatalogEntry {
                id: "qwen3-5-4b-q4-k-m".into(),
                name: "Qwen3.5 4B 日常版".into(),
                description: "本地问答的最低推荐档，适合中文对话、总结和轻量任务。".into(),
                parameter_size: "4B".into(),
                quantization: "Q4_K_M".into(),
                download_size_bytes: 3_413_361_504,
                required_memory_bytes: 8 * 1024 * 1024 * 1024,
                context_window: 65_536,
                license: "Apache-2.0".into(),
                source: "Qwen3.5 / Unsloth GGUF".into(),
                recommended: true,
                tasks: vec![ModelTask::Chat],
                traits: vec![ModelTrait::VisionInput],
            },
            model_size_bytes: 2_740_937_888,
            file_name: "qwen3-5-4b-q4-k-m.gguf",
            url: "https://huggingface.co/unsloth/Qwen3.5-4B-GGUF/resolve/e87f176479d0855a907a41277aca2f8ee7a09523/Qwen3.5-4B-Q4_K_M.gguf",
            sha256: "00fe7986ff5f6b463e62455821146049db6f9313603938a70800d1fb69ef11a4",
            vision_projector: Some(ComponentArtifact {
                file_name: "qwen3-5-4b-mmproj-f16.gguf",
                url: "https://huggingface.co/unsloth/Qwen3.5-4B-GGUF/resolve/e87f176479d0855a907a41277aca2f8ee7a09523/mmproj-F16.gguf",
                sha256: "cd88edcf8d031894960bb0c9c5b9b7e1fea6ebee02b9f7ce925a00d12891f864",
                size: 672_423_616,
                progress_component: LocalModelProgressComponent::VisionProjector,
            }),
        },
        ModelArtifact {
            entry: LocalModelCatalogEntry {
                id: "qwen3-5-9b-q4-k-m".into(),
                name: "Qwen3.5 9B 增强版".into(),
                description: "复杂指令和长对话更稳定，适合内存较充足的设备。".into(),
                parameter_size: "9B".into(),
                quantization: "Q4_K_M".into(),
                download_size_bytes: 6_598_688_544,
                required_memory_bytes: 12 * 1024 * 1024 * 1024,
                context_window: 65_536,
                license: "Apache-2.0".into(),
                source: "Qwen3.5 / Unsloth GGUF".into(),
                recommended: false,
                tasks: vec![ModelTask::Chat],
                traits: vec![ModelTrait::VisionInput],
            },
            model_size_bytes: 5_680_522_464,
            file_name: "qwen3-5-9b-q4-k-m.gguf",
            url: "https://huggingface.co/unsloth/Qwen3.5-9B-GGUF/resolve/3885219b6810b007914f3a7950a8d1b469d598a5/Qwen3.5-9B-Q4_K_M.gguf",
            sha256: "03b74727a860a56338e042c4420bb3f04b2fec5734175f4cb9fa853daf52b7e8",
            vision_projector: Some(ComponentArtifact {
                file_name: "qwen3-5-9b-mmproj-f16.gguf",
                url: "https://huggingface.co/unsloth/Qwen3.5-9B-GGUF/resolve/3885219b6810b007914f3a7950a8d1b469d598a5/mmproj-F16.gguf",
                sha256: "f70dc3509053962b0d0d3ee8a7eacebf5d60aa560cad78254ae8698516ae029f",
                size: 918_166_080,
                progress_component: LocalModelProgressComponent::VisionProjector,
            }),
        },
    ]
}

fn apply_local_chat_defaults(mut body: Value) -> Value {
    let Some(object) = body.as_object_mut() else {
        return body;
    };
    if !object.contains_key("chat_template_kwargs") {
        object.insert(
            "chat_template_kwargs".into(),
            json!({ "enable_thinking": false }),
        );
    } else if let Some(kwargs) = object
        .get_mut("chat_template_kwargs")
        .and_then(Value::as_object_mut)
    {
        kwargs
            .entry("enable_thinking".to_owned())
            .or_insert(Value::Bool(false));
    }
    body
}

fn runtime_artifact() -> Option<RuntimeArtifact> {
    let base = "https://github.com/ggml-org/llama.cpp/releases/download/b9957/";
    let artifact = match (std::env::consts::OS, std::env::consts::ARCH) {
        ("windows", "x86_64") => RuntimeArtifact {
            url: concat!("https://github.com/ggml-org/llama.cpp/releases/download/b9957/", "llama-b9957-bin-win-vulkan-x64.zip"),
            sha256: "fcc0a8c0f0f3140122452ed2728cebb520c5fbc4fc921836ee3a45dd77e18c68",
            size: 32_897_089,
            archive_name: "llama-b9957-bin-win-vulkan-x64.zip",
            archive_kind: ArchiveKind::Zip,
            backend: LocalModelRuntimeBackend::Vulkan,
        },
        ("windows", "aarch64") => RuntimeArtifact {
            url: concat!("https://github.com/ggml-org/llama.cpp/releases/download/b9957/", "llama-b9957-bin-win-cpu-arm64.zip"),
            sha256: "3eeecdc9d1d33932e84bb7cecec9b6dcbc95072f3f7e52a1d7252f17afac6542",
            size: 12_134_012,
            archive_name: "llama-b9957-bin-win-cpu-arm64.zip",
            archive_kind: ArchiveKind::Zip,
            backend: LocalModelRuntimeBackend::Cpu,
        },
        ("macos", "aarch64") => RuntimeArtifact {
            url: concat!("https://github.com/ggml-org/llama.cpp/releases/download/b9957/", "llama-b9957-bin-macos-arm64.tar.gz"),
            sha256: "7a43fd3c4ddd30f3c408da7c80975503f18b829da023a7d0e34bdb6f1b1a056f",
            size: 10_737_291,
            archive_name: "llama-b9957-bin-macos-arm64.tar.gz",
            archive_kind: ArchiveKind::TarGz,
            backend: LocalModelRuntimeBackend::Metal,
        },
        ("macos", "x86_64") => RuntimeArtifact {
            url: concat!("https://github.com/ggml-org/llama.cpp/releases/download/b9957/", "llama-b9957-bin-macos-x64.tar.gz"),
            sha256: "f03f6669c7e34c2768ca4a318dd13e105dec46e1f87a2165d2be7fd6a0ee4716",
            size: 11_006_704,
            archive_name: "llama-b9957-bin-macos-x64.tar.gz",
            archive_kind: ArchiveKind::TarGz,
            backend: LocalModelRuntimeBackend::Metal,
        },
        ("linux", "x86_64") => RuntimeArtifact {
            url: concat!("https://github.com/ggml-org/llama.cpp/releases/download/b9957/", "llama-b9957-bin-ubuntu-vulkan-x64.tar.gz"),
            sha256: "0a65257a72010e93c39136a50b8904202f3c4c40ff3ecd8a33a47c903035c724",
            size: 31_171_524,
            archive_name: "llama-b9957-bin-ubuntu-vulkan-x64.tar.gz",
            archive_kind: ArchiveKind::TarGz,
            backend: LocalModelRuntimeBackend::Vulkan,
        },
        ("linux", "aarch64") => RuntimeArtifact {
            url: concat!("https://github.com/ggml-org/llama.cpp/releases/download/b9957/", "llama-b9957-bin-ubuntu-vulkan-arm64.tar.gz"),
            sha256: "87554e8d13a1980d9a3829361b430249fd74a8b924a02f74e29dc996b58384b3",
            size: 25_413_005,
            archive_name: "llama-b9957-bin-ubuntu-vulkan-arm64.tar.gz",
            archive_kind: ArchiveKind::TarGz,
            backend: LocalModelRuntimeBackend::Vulkan,
        },
        _ => return None,
    };
    debug_assert!(artifact.url.starts_with(base));
    Some(artifact)
}

fn model_path_at(root: &Path, model: &ModelArtifact) -> PathBuf {
    root.join("models")
        .join(&model.entry.id)
        .join(model.file_name)
}

fn component_path_at(root: &Path, model: &ModelArtifact, component: &ComponentArtifact) -> PathBuf {
    root.join("models")
        .join(&model.entry.id)
        .join(component.file_name)
}

async fn downloaded_artifact_bytes(root: &Path, model: &ModelArtifact) -> u64 {
    async fn downloaded(path: &Path, expected: u64) -> u64 {
        let complete = file_len(path).await;
        if complete == expected {
            expected
        } else {
            file_len(&partial_path(path)).await.min(expected)
        }
    }

    let mut total = downloaded(&model_path_at(root, model), model.model_size_bytes).await;
    if let Some(component) = model.vision_projector {
        total = total.saturating_add(
            downloaded(&component_path_at(root, model, &component), component.size).await,
        );
    }
    total
}

async fn artifact_files_installed(root: &Path, model: &ModelArtifact) -> bool {
    if file_len(&model_path_at(root, model)).await != model.model_size_bytes {
        return false;
    }
    if let Some(component) = model.vision_projector {
        return file_len(&component_path_at(root, model, &component)).await == component.size;
    }
    true
}

/// Best-effort, idempotent cleanup for artifacts downloaded by catalogs that
/// are no longer offered. Only exact NomiFun-owned file names are removed;
/// unknown files keep the directory in place.
async fn cleanup_retired_model_artifacts(root: &Path) {
    for (model_id, file_name) in RETIRED_MODEL_ARTIFACTS {
        let directory = root.join("models").join(model_id);
        match tokio::fs::symlink_metadata(&directory).await {
            Ok(metadata) if !metadata.is_dir() || unsafe_link_or_reparse(&metadata) => {
                warn!(model_id, "Skipping unsafe retired local-model directory");
                continue;
            }
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => {
                warn!(model_id, error = %error, "Could not inspect retired local-model directory");
                continue;
            }
        }
        if let Err(error) = prepare_managed_directory(root, &directory) {
            warn!(model_id, error = %error, "Skipping invalid retired local-model directory");
            continue;
        }

        let model_path = directory.join(file_name);
        for path in [model_path.clone(), partial_path(&model_path)] {
            if let Err(error) = prepare_managed_file(root, &path) {
                warn!(model_id, error = %error, "Skipping unsafe retired local-model artifact");
                continue;
            }
            match tokio::fs::remove_file(&path).await {
                Ok(()) => info!(model_id, "Removed retired local-model artifact"),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => {
                    warn!(model_id, error = %error, "Could not remove retired local-model artifact");
                }
            }
        }

        // This deliberately fails harmlessly if an unknown file is present.
        let _ = tokio::fs::remove_dir(&directory).await;
    }
}

fn partial_path(final_path: &Path) -> PathBuf {
    let mut name = final_path
        .file_name()
        .unwrap_or_default()
        .to_os_string();
    name.push(".part");
    final_path.with_file_name(name)
}

fn runtime_dir_at(root: &Path) -> PathBuf {
    root.join("runtime").join(RUNTIME_VERSION)
}

fn runtime_stamp_path(root: &Path) -> PathBuf {
    runtime_dir_at(root).join("runtime.json")
}

fn managed_runtime_executable(root: &Path, artifact: &RuntimeArtifact) -> Option<PathBuf> {
    let runtime_dir = runtime_dir_at(root);
    prepare_managed_directory(root, &runtime_dir).ok()?;
    let stamp_path = runtime_stamp_path(root);
    prepare_managed_file(root, &stamp_path).ok()?;
    let bytes = std::fs::read(stamp_path).ok()?;
    let stamp: RuntimeStamp = serde_json::from_slice(&bytes).ok()?;
    if stamp.version != RUNTIME_VERSION
        || stamp.archive_sha256 != artifact.sha256
        || stamp.files.is_empty()
    {
        return None;
    }
    for file in stamp.files {
        let relative = Path::new(&file.path);
        if !safe_relative_path(relative) {
            return None;
        }
        let path = runtime_dir.join(relative);
        let metadata = std::fs::symlink_metadata(path).ok()?;
        if !metadata.is_file() || metadata.file_type().is_symlink() || metadata.len() != file.size {
            return None;
        }
    }
    find_runtime_executable(&runtime_dir)
}

fn write_runtime_stamp(root: &Path, artifact: &RuntimeArtifact) -> std::io::Result<()> {
    let runtime_dir = runtime_dir_at(root);
    let mut files = Vec::new();
    collect_runtime_files(&runtime_dir, &runtime_dir, &mut files, 0)?;
    if files.is_empty() || find_runtime_executable(&runtime_dir).is_none() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "runtime contains no executable files",
        ));
    }
    files.sort_by(|a, b| a.path.cmp(&b.path));
    let stamp = RuntimeStamp {
        version: RUNTIME_VERSION.to_owned(),
        archive_sha256: artifact.sha256.to_owned(),
        files,
    };
    let path = runtime_stamp_path(root);
    let temp = path.with_extension("json.tmp");
    prepare_managed_file(root, &path)?;
    prepare_managed_file(root, &temp)?;
    match std::fs::remove_file(&temp) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    let bytes = serde_json::to_vec_pretty(&stamp)
        .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))?;
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temp)?;
    use std::io::Write as _;
    file.write_all(&bytes)?;
    file.sync_all()?;
    drop(file);
    atomic_replace(&temp, &path)
}

fn collect_runtime_files(
    root: &Path,
    directory: &Path,
    output: &mut Vec<RuntimeStampedFile>,
    depth: usize,
) -> std::io::Result<()> {
    if depth > 8 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "runtime archive is nested too deeply",
        ));
    }
    for entry in std::fs::read_dir(directory)? {
        let entry = entry?;
        let file_type = entry.file_type()?;
        if file_type.is_symlink() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "runtime directory contains a link",
            ));
        }
        let path = entry.path();
        if file_type.is_dir() {
            collect_runtime_files(root, &path, output, depth + 1)?;
        } else if file_type.is_file() {
            if entry.file_name() == "runtime.json" || entry.file_name() == "runtime.json.tmp" {
                continue;
            }
            let relative = path.strip_prefix(root).map_err(|_| {
                std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "runtime file escaped root",
                )
            })?;
            if !safe_relative_path(relative) {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "invalid runtime file path",
                ));
            }
            output.push(RuntimeStampedFile {
                path: relative.to_string_lossy().replace('\\', "/"),
                size: entry.metadata()?.len(),
            });
        }
    }
    Ok(())
}

async fn file_len(path: &Path) -> u64 {
    tokio::fs::metadata(path).await.map(|m| m.len()).unwrap_or(0)
}

async fn load_persisted_state(root: &Path) -> PersistedState {
    let path = root.join("state.json");
    if let Err(error) = prepare_managed_file(root, &path) {
        warn!(error = %error, "Ignoring unsafe local-model state path");
        return PersistedState {
            version: STATE_VERSION,
            ..Default::default()
        };
    }
    match tokio::fs::read(&path).await {
        Ok(bytes) => match serde_json::from_slice::<PersistedState>(&bytes) {
            Ok(state) if state.version == STATE_VERSION => state,
            Ok(_) => {
                warn!("Ignoring unsupported local-model state version");
                PersistedState { version: STATE_VERSION, ..Default::default() }
            }
            Err(error) => {
                warn!(error = %error, "Ignoring invalid local-model state file");
                PersistedState { version: STATE_VERSION, ..Default::default() }
            }
        },
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            PersistedState { version: STATE_VERSION, ..Default::default() }
        }
        Err(error) => {
            warn!(error = %error, "Could not read local-model state file");
            PersistedState { version: STATE_VERSION, ..Default::default() }
        }
    }
}

#[cfg(windows)]
fn atomic_replace(from: &Path, to: &Path) -> std::io::Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::{
        MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH, MoveFileExW,
    };

    let from_wide = from.as_os_str().encode_wide().chain(Some(0)).collect::<Vec<_>>();
    let to_wide = to.as_os_str().encode_wide().chain(Some(0)).collect::<Vec<_>>();
    // SAFETY: both buffers are owned, NUL-terminated UTF-16 paths and remain
    // alive for the duration of the Win32 call.
    let result = unsafe {
        MoveFileExW(
            from_wide.as_ptr(),
            to_wide.as_ptr(),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    };
    if result == 0 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(())
    }
}

#[cfg(unix)]
fn atomic_replace(from: &Path, to: &Path) -> std::io::Result<()> {
    std::fs::rename(from, to)?;
    if let Some(parent) = to.parent() {
        File::open(parent)?.sync_all()?;
    }
    Ok(())
}

#[cfg(not(any(unix, windows)))]
fn atomic_replace(from: &Path, to: &Path) -> std::io::Result<()> {
    if to.exists() {
        std::fs::remove_file(to)?;
    }
    std::fs::rename(from, to)
}

impl LocalModelService {
    async fn save_state(&self) -> Result<(), AppError> {
        let _persist_guard = self.persist_lock.lock().await;
        let persisted = { self.state.lock().await.persisted.clone() };
        let bytes = serde_json::to_vec_pretty(&persisted)
            .map_err(|e| AppError::Internal(format!("serialize local model state: {e}")))?;
        let path = self.root.join("state.json");
        let temp = self.root.join("state.json.tmp");
        prepare_managed_file(&self.root, &path)
            .and_then(|_| prepare_managed_file(&self.root, &temp))
            .map_err(|e| AppError::Internal(format!("validate local model state path: {e}")))?;
        remove_file_if_exists(&temp).await?;
        let mut file = tokio::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temp)
            .await
            .map_err(|e| AppError::Internal(format!("create local model state: {e}")))?;
        file.write_all(&bytes)
            .await
            .map_err(|e| AppError::Internal(format!("write local model state: {e}")))?;
        file.sync_all()
            .await
            .map_err(|e| AppError::Internal(format!("sync local model state: {e}")))?;
        drop(file);
        prepare_managed_file(&self.root, &path)
            .and_then(|_| prepare_managed_file(&self.root, &temp))
            .map_err(|e| AppError::Internal(format!("revalidate local model state path: {e}")))?;
        tokio::task::spawn_blocking(move || atomic_replace(&temp, &path))
            .await
            .map_err(|e| AppError::Internal(format!("join local state commit: {e}")))?
            .map_err(|e| AppError::Internal(format!("commit local model state: {e}")))?;
        Ok(())
    }

    async fn set_projection(&self, base_url: String, encrypted_token: String) {
        *self.projection.write().await = Some(ProviderProjection {
            base_url,
            encrypted_token,
        });
    }

    async fn provision_provider(&self) -> Result<(), AppError> {
        let _projection_sync_guard = self.projection_sync_lock.lock().await;
        let rows = self.provider_repo.list().await?;
        if let Some(alias) = rows.iter().find(|row| {
            row.id != LOCAL_MODEL_PROVIDER_ID && row.platform == LOCAL_MODEL_PROVIDER_ID
        }) {
            return Err(AppError::Conflict(format!(
                "Reserved local-model platform is already used by provider '{}'",
                alias.id
            )));
        }
        let existing = self.provider_repo.find_by_id(LOCAL_MODEL_PROVIDER_ID).await?;
        if let Some(row) = &existing
            && row.platform != LOCAL_MODEL_PROVIDER_ID
        {
            return Err(AppError::Conflict(format!(
                "Reserved provider id '{LOCAL_MODEL_PROVIDER_ID}' is already used by another platform"
            )));
        }

        let projection = self
            .projection
            .read()
            .await
            .clone()
            .ok_or_else(|| AppError::Internal("local model facade is not initialized".into()))?;
        let active = {
            let state = self.state.lock().await;
            state.persisted.active_model_id.clone().filter(|id| {
                state.models.get(id).is_some_and(|model| {
                    model.install_phase == LocalModelInstallPhase::Installed
                })
            })
        };
        let mut auxiliary = self
            .auxiliary_projection_models
            .read()
            .await
            .values()
            .cloned()
            .collect::<Vec<_>>();
        auxiliary.sort_by(|left, right| left.id.cmp(&right.id));
        let active_context_limit = active
            .as_deref()
            .and_then(|id| self.catalog.iter().find(|model| model.entry.id == id))
            .map(|model| i64::from(model.entry.context_window));

        let mut projected_ids = Vec::with_capacity(usize::from(active.is_some()) + auxiliary.len());
        if let Some(id) = &active {
            projected_ids.push(id.clone());
        }
        projected_ids.extend(auxiliary.iter().map(|model| model.id.clone()));
        let models_json = serde_json::to_string(&projected_ids)
            .map_err(|e| AppError::Internal(format!("serialize local provider models: {e}")))?;
        let enabled_json = serde_json::to_string(
            &projected_ids
                .iter()
                .map(|id| (id.clone(), true))
                .collect::<HashMap<_, _>>(),
        )
        .map_err(|e| AppError::Internal(format!("serialize local model flags: {e}")))?;
        let mut descriptions = auxiliary
            .iter()
            .map(|model| (model.id.clone(), model.description.clone()))
            .collect::<HashMap<_, _>>();
        if let Some(model) = active
            .as_deref()
            .and_then(|id| self.catalog.iter().find(|model| model.entry.id == id))
        {
            descriptions.insert(model.entry.id.clone(), model.entry.description.clone());
        }
        let descriptions_json = serde_json::to_string(&descriptions)
            .map_err(|e| AppError::Internal(format!("serialize local model description: {e}")))?;
        let context_limits = active
            .as_deref()
            .and_then(|id| self.catalog.iter().find(|model| model.entry.id == id))
            .map(|model| HashMap::from([(model.entry.id.clone(), model.entry.context_window)]))
            .unwrap_or_default();
        let context_limits_json = serde_json::to_string(&context_limits)
            .map_err(|e| AppError::Internal(format!("serialize local context limit: {e}")))?;
        let provider_enabled = !projected_ids.is_empty();

        match existing {
            Some(_) => {
                self.provider_repo
                    .update(
                        LOCAL_MODEL_PROVIDER_ID,
                        UpdateProviderParams {
                            platform: Some(LOCAL_MODEL_PROVIDER_ID),
                            name: Some(LOCAL_MODEL_PROVIDER_NAME),
                            base_url: Some(&projection.base_url),
                            api_key_encrypted: Some(&projection.encrypted_token),
                            models: Some(&models_json),
                            enabled: Some(provider_enabled),
                            capabilities: Some("[]"),
                            context_limit: Some(active_context_limit),
                            model_context_limits: Some(Some(&context_limits_json)),
                            model_descriptions: Some(Some(&descriptions_json)),
                            model_enabled: Some(Some(&enabled_json)),
                            model_health: Some(None),
                            bedrock_config: Some(None),
                            is_full_url: Some(false),
                            ..Default::default()
                        },
                    )
                    .await?;
            }
            None => {
                self.provider_repo
                    .create(CreateProviderParams {
                        id: Some(LOCAL_MODEL_PROVIDER_ID),
                        platform: LOCAL_MODEL_PROVIDER_ID,
                        name: LOCAL_MODEL_PROVIDER_NAME,
                        base_url: &projection.base_url,
                        api_key_encrypted: &projection.encrypted_token,
                        models: &models_json,
                        enabled: provider_enabled,
                        capabilities: "[]",
                        context_limit: active_context_limit,
                        model_context_limits: Some(&context_limits_json),
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
        Ok(())
    }

    async fn sync_provider_projection(&self) -> Result<(), AppError> {
        self.provision_provider().await
    }
}

/// Start the stable authenticated OpenAI facade and create/update the reserved
/// `nomifun-local-model` provider projection. No runtime or model bytes are
/// downloaded at application startup.
pub async fn start_and_provision_local_model(
    data_dir: impl AsRef<Path>,
    provider_repo: Arc<dyn IProviderRepository>,
    encryption_key: [u8; 32],
) -> Result<(Arc<LocalModelService>, LocalModelServer), AppError> {
    let service = LocalModelService::new(
        data_dir.as_ref().join(LOCAL_ROOT_DIR),
        provider_repo,
    )
    .await?;
    let server = LocalModelServer::start(service.clone())
        .await
        .map_err(AppError::Internal)?;
    let encrypted_token = encrypt_string(server.auth_token(), &encryption_key)?;
    service
        .set_projection(server.base_url(), encrypted_token)
        .await;
    if let Err(error) = service.provision_provider().await {
        let _ = disable_local_model_provider(service.provider_repo.clone()).await;
        return Err(error);
    }
    service.save_state().await?;
    Ok((service, server))
}

/// Disable the canonical local provider when the optional service cannot
/// start, so a previous boot's ephemeral port/token never remains selectable.
pub async fn disable_local_model_provider(
    provider_repo: Arc<dyn IProviderRepository>,
) -> Result<(), AppError> {
    let Some(existing) = provider_repo.find_by_id(LOCAL_MODEL_PROVIDER_ID).await? else {
        return Ok(());
    };
    if existing.platform != LOCAL_MODEL_PROVIDER_ID {
        return Ok(());
    }
    provider_repo
        .update(
            LOCAL_MODEL_PROVIDER_ID,
            UpdateProviderParams {
                models: Some("[]"),
                enabled: Some(false),
                model_enabled: Some(Some("{}")),
                model_health: Some(None),
                ..Default::default()
            },
        )
        .await?;
    Ok(())
}

#[derive(Clone)]
struct LocalFacadeState {
    service: Arc<LocalModelService>,
    auth_token: String,
}

/// Stable loopback facade kept alive by `AppServices` while the underlying
/// inference sidecar may be stopped, restarted, or sleeping.
pub struct LocalModelServer {
    http_addr: SocketAddr,
    auth_token: String,
    task: Option<tokio::task::JoinHandle<()>>,
}

impl LocalModelServer {
    async fn start(service: Arc<LocalModelService>) -> Result<Self, String> {
        let auth_token = generate_token()?;
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .map_err(|e| format!("Failed to bind local model facade: {e}"))?;
        let http_addr = listener
            .local_addr()
            .map_err(|e| format!("Failed to inspect local model facade: {e}"))?;
        let app = Router::new()
            .route("/v1/models", get(local_models))
            .route("/v1/chat/completions", post(local_chat))
            .layer(DefaultBodyLimit::max(LOCAL_CHAT_BODY_LIMIT))
            .with_state(LocalFacadeState {
                service,
                auth_token: auth_token.clone(),
            });
        let task = tokio::spawn(async move {
            if let Err(error) = axum::serve(listener, app).await {
                warn!(error = %error, "Local model facade exited");
            }
        });
        debug!(port = http_addr.port(), "Local model facade started");
        Ok(Self {
            http_addr,
            auth_token,
            task: Some(task),
        })
    }

    pub fn base_url(&self) -> String {
        format!("http://127.0.0.1:{}/v1", self.http_addr.port())
    }

    pub fn auth_token(&self) -> &str {
        &self.auth_token
    }

    pub fn stop(&mut self) {
        if let Some(task) = self.task.take() {
            task.abort();
        }
    }
}

impl Drop for LocalModelServer {
    fn drop(&mut self) {
        self.stop();
    }
}

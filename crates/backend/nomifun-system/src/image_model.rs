//! Managed, opt-in Z-Image artifact control plane.
//!
//! Construction is deliberately network-free. Downloads begin only after an
//! explicit [`ImageModelService::install`] or [`ImageModelService::resume`]
//! call. The public API never accepts a URL or a local path: every artifact is
//! selected from the immutable recipe exported by `nomifun-creation`.

use std::collections::HashSet;
use std::ffi::OsStr;
use std::fs::File;
use std::path::{Component, Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Weak};
use std::time::{Duration, Instant};

use futures_util::StreamExt;
use nomifun_api_types::{
    ImageModelCatalogEntry, ImageModelComponent, ImageModelComponentProgress,
    ImageModelInstallPhase, ImageModelRuntimePhase, ImageModelServiceStatus, ImageModelState,
    LocalModelErrorKind, ModelTask,
};
use nomifun_common::AppError;
use nomifun_db::{IModelProfileRepository, UpsertModelProfileParams};
use nomifun_creation::{
    LOCAL_Z_IMAGE_TURBO_MODEL_ID, LocalImageBackend, LocalImageRequest,
    SD_CPP_RUNTIME_VERSION, SdCliZImageBackend, SdCliZImageConfig,
    Z_IMAGE_TURBO_ARTIFACTS, Z_IMAGE_TURBO_DOWNLOAD_SIZE, ZImageArtifactRole,
    current_sd_cpp_runtime_artifact,
};
use reqwest::header::{CONTENT_LENGTH, CONTENT_RANGE, RANGE};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::{Mutex, Notify, RwLock, Semaphore};
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

use crate::local_model::LocalModelService;

const IMAGE_PROTOCOL_VERSION: &str = "1";
const LOCAL_AI_DIR: &str = "local-ai";
const IMAGE_DIR: &str = "image";
const RUNTIME_DIR: &str = "runtime";
const MODELS_DIR: &str = "models";
const DOWNLOADS_DIR: &str = "downloads";
const JOBS_DIR: &str = "jobs";
const STATE_FILE: &str = "state.json";
const STATE_VERSION: u32 = 1;
const DOWNLOAD_PROGRESS_INTERVAL: Duration = Duration::from_millis(250);
const DISK_SAFETY_BYTES: u64 = 256 * 1024 * 1024;
const RUNTIME_EXTRACT_RESERVE_BYTES: u64 = 768 * 1024 * 1024;
const MAX_ARCHIVE_ENTRIES: usize = 4_096;
const MAX_ARCHIVE_EXPANDED_BYTES: u64 = 2 * 1024 * 1024 * 1024;

pub const Z_IMAGE_TURBO_MODEL_ID: &str = LOCAL_Z_IMAGE_TURBO_MODEL_ID;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ArtifactKind {
    RuntimeZip,
    Model,
}

#[derive(Debug, Clone)]
struct ImageArtifact {
    component: ImageModelComponent,
    kind: ArtifactKind,
    file_name: String,
    url: String,
    size: u64,
    sha256: String,
}

#[derive(Debug)]
struct ActiveInstall {
    generation: u64,
    cancel: CancellationToken,
    done: Arc<Notify>,
}

#[derive(Debug)]
struct MutableState {
    model: ImageModelState,
    active: Option<ActiveInstall>,
    next_generation: u64,
    last_error: Option<String>,
}

#[derive(Debug)]
struct ImageFailure {
    kind: LocalModelErrorKind,
    safe_message: &'static str,
    detail: String,
    cancelled: bool,
}

impl ImageFailure {
    fn new(
        kind: LocalModelErrorKind,
        safe_message: &'static str,
        detail: impl Into<String>,
    ) -> Self {
        Self {
            kind,
            safe_message,
            detail: detail.into(),
            cancelled: false,
        }
    }

    fn cancelled() -> Self {
        Self {
            kind: LocalModelErrorKind::Unknown,
            safe_message: "Image model download is paused.",
            detail: "image model install cancelled by user".into(),
            cancelled: true,
        }
    }
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct InstalledManifest {
    version: u32,
    model_id: String,
    runtime_version: String,
    artifacts: Vec<ManifestArtifact>,
}

#[derive(Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ManifestArtifact {
    component: ImageModelComponent,
    file_name: String,
    size: u64,
    sha256: String,
}

/// One-click manager for the fixed, consumer-sized Z-Image-Turbo recipe.
///
/// The service is cheap to construct and never downloads or starts native code
/// on application startup.
pub struct ImageModelService {
    /// Safety anchor shared with other local-AI features.
    root: PathBuf,
    http_client: reqwest::Client,
    artifacts: Vec<ImageArtifact>,
    supported_platform: bool,
    allow_insecure_loopback_downloads: bool,
    state: Mutex<MutableState>,
    mutation_lock: Mutex<()>,
    projection_service: RwLock<Option<Weak<LocalModelService>>>,
    verification_lock: Mutex<()>,
    verified_bundle: AtomicBool,
    workload_gate: std::sync::RwLock<Arc<Semaphore>>,
}

struct ManagedImageBackend {
    service: Arc<ImageModelService>,
    workload_gate: Arc<Semaphore>,
}

#[async_trait::async_trait]
impl LocalImageBackend for ManagedImageBackend {
    async fn generate(
        &self,
        request: LocalImageRequest,
    ) -> Result<Vec<nomifun_creation::ProducedAsset>, nomifun_creation::CreationError> {
        let permit = self
            .workload_gate
            .clone()
            .acquire_owned()
            .await
            .map_err(|_| {
                nomifun_creation::CreationError::config(
                    "the local image workload gate is shutting down",
                )
            })?;
        let config = self.service.sd_cli_config().await.map_err(|_| {
            nomifun_creation::CreationError::config(
                "local Z-Image is not fully installed; install it from Local Models first",
            )
        })?;
        SdCliZImageBackend::new(config)
            .generate_with_permit(request, permit)
            .await
    }
}

impl ImageModelService {
    pub async fn new(data_dir: impl AsRef<Path>) -> Result<Arc<Self>, AppError> {
        let root = data_dir.as_ref().join(LOCAL_AI_DIR);
        let (artifacts, supported_platform) = production_artifacts();
        Self::new_inner(
            root,
            image_download_client(),
            artifacts,
            supported_platform,
            false,
        )
        .await
    }

    async fn new_inner(
        root: PathBuf,
        http_client: reqwest::Client,
        artifacts: Vec<ImageArtifact>,
        supported_platform: bool,
        allow_insecure_loopback_downloads: bool,
    ) -> Result<Arc<Self>, AppError> {
        prepare_layout(&root, &artifacts).map_err(|error| {
            AppError::Internal(format!("prepare image model directory: {error}"))
        })?;
        let root = std::fs::canonicalize(&root).map_err(|error| {
            AppError::Internal(format!("resolve image model directory: {error}"))
        })?;
        let model = inspect_model_state(&root, &artifacts, supported_platform).await;
        let last_error = model.error_kind.map(|kind| match kind {
            LocalModelErrorKind::UnsupportedPlatform => {
                "Local image generation is not available on this platform.".into()
            }
            _ => "Image model files need repair before they can be used.".into(),
        });
        Ok(Arc::new(Self {
            root,
            http_client,
            artifacts,
            supported_platform,
            allow_insecure_loopback_downloads,
            state: Mutex::new(MutableState {
                model,
                active: None,
                next_generation: 0,
                last_error,
            }),
            mutation_lock: Mutex::new(()),
            projection_service: RwLock::new(None),
            verification_lock: Mutex::new(()),
            verified_bundle: AtomicBool::new(false),
            workload_gate: std::sync::RwLock::new(Arc::new(Semaphore::new(1))),
        }))
    }

    pub async fn catalog(&self) -> Vec<ImageModelCatalogEntry> {
        vec![catalog_entry(&self.artifacts)]
    }

    pub async fn status(&self) -> ImageModelServiceStatus {
        let state = self.state.lock().await;
        let workload_busy = self
            .workload_gate
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .available_permits()
            == 0;
        snapshot(
            &state,
            self.verified_bundle.load(Ordering::Acquire),
            workload_busy,
        )
    }

    /// Seed the authoritative image-generation capability without overriding
    /// an explicit user edit. The provider row itself remains empty until the
    /// bundle is fully installed.
    pub async fn reconcile_profile(
        &self,
        repository: &dyn IModelProfileRepository,
        provider_id: &str,
    ) -> Result<bool, AppError> {
        let tasks = serde_json::to_string(&[ModelTask::ImageGeneration])
            .map_err(|error| AppError::Internal(format!("serialize image model tasks: {error}")))?;
        let params = serde_json::to_string(&serde_json::json!({
            "steps": nomifun_creation::Z_IMAGE_TURBO_STEPS,
            "cfgScale": nomifun_creation::Z_IMAGE_TURBO_CFG_SCALE,
            "count": 1,
        }))
        .map_err(|error| AppError::Internal(format!("serialize image model params: {error}")))?;
        Ok(repository
            .upsert_unless_user(&UpsertModelProfileParams {
                provider_id,
                model: Z_IMAGE_TURBO_MODEL_ID,
                tasks: &tasks,
                traits: "[]",
                params: &params,
                source: "catalog",
            })
            .await?)
    }

    /// Bind the shared managed-provider projection after both optional local
    /// control planes have started. This performs no network I/O or download.
    pub async fn bind_projection_service(
        &self,
        service: &Arc<LocalModelService>,
    ) -> Result<(), AppError> {
        *self.projection_service.write().await = Some(Arc::downgrade(service));
        let installed = self.status().await.artifacts_ready;
        self.sync_projection(installed).await
    }

    /// Build a lazy creation backend. It resolves verified paths at generation
    /// time, so installing Z-Image does not require restarting NomiFun.
    pub fn creation_backend(
        self: &Arc<Self>,
        workload_gate: Arc<Semaphore>,
    ) -> Arc<dyn LocalImageBackend> {
        *self
            .workload_gate
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = workload_gate.clone();
        Arc::new(ManagedImageBackend {
            service: self.clone(),
            workload_gate,
        })
    }

    async fn sync_projection(&self, installed: bool) -> Result<(), AppError> {
        let Some(service) = self
            .projection_service
            .read()
            .await
            .as_ref()
            .and_then(Weak::upgrade)
        else {
            return Ok(());
        };
        service
            .set_auxiliary_model_projection(
                Z_IMAGE_TURBO_MODEL_ID,
                "本地 Z-Image Turbo 文生图模型",
                installed,
            )
            .await
    }

    pub async fn install(
        self: &Arc<Self>,
        model_id: &str,
    ) -> Result<ImageModelServiceStatus, AppError> {
        self.start_install(model_id, false).await
    }

    pub async fn resume(
        self: &Arc<Self>,
        model_id: &str,
    ) -> Result<ImageModelServiceStatus, AppError> {
        self.start_install(model_id, true).await
    }

    async fn start_install(
        self: &Arc<Self>,
        model_id: &str,
        resume_only: bool,
    ) -> Result<ImageModelServiceStatus, AppError> {
        validate_model_id(model_id)?;
        if !self.supported_platform {
            return Err(AppError::BadRequest(
                "Local image generation is not supported on this platform".into(),
            ));
        }
        let _mutation = self.mutation_lock.lock().await;
        let (generation, cancel, done) = {
            let mut state = self.state.lock().await;
            if state.active.is_some() {
                return Err(AppError::Conflict(
                    "An image model installation is already running".into(),
                ));
            }
            if state.model.install_phase == ImageModelInstallPhase::Installed {
                return Ok(snapshot(
                    &state,
                    self.verified_bundle.load(Ordering::Acquire),
                    false,
                ));
            }
            if resume_only && state.model.install_phase != ImageModelInstallPhase::Paused {
                return Err(AppError::Conflict(
                    "The image model does not have a paused installation to resume".into(),
                ));
            }
            if !resume_only && state.model.install_phase == ImageModelInstallPhase::Paused {
                return Err(AppError::Conflict(
                    "Resume the paused image model installation instead".into(),
                ));
            }

            self.verified_bundle.store(false, Ordering::Release);

            state.next_generation = state.next_generation.wrapping_add(1).max(1);
            let generation = state.next_generation;
            let cancel = CancellationToken::new();
            let done = Arc::new(Notify::new());
            state.active = Some(ActiveInstall {
                generation,
                cancel: cancel.clone(),
                done: Arc::clone(&done),
            });
            state.model.install_phase = ImageModelInstallPhase::Downloading;
            state.model.error_kind = None;
            state.model.message = None;
            state.last_error = None;
            (generation, cancel, done)
        };

        let service = Arc::clone(self);
        tokio::spawn(async move {
            service.run_install(generation, cancel).await;
            done.notify_one();
        });
        Ok(self.status().await)
    }

    /// Pause an active install and keep every verified file and `.part` file.
    /// This waits for the background task to observe cancellation, so a client
    /// can call `resume` immediately after this method returns.
    pub async fn pause(&self, model_id: &str) -> Result<ImageModelServiceStatus, AppError> {
        validate_model_id(model_id)?;
        let _mutation = self.mutation_lock.lock().await;
        let done = {
            let mut state = self.state.lock().await;
            let Some(active) = state.active.as_ref() else {
                return Err(AppError::Conflict(
                    "The image model is not currently installing".into(),
                ));
            };
            let done = Arc::clone(&active.done);
            active.cancel.cancel();
            state.model.install_phase = ImageModelInstallPhase::Paused;
            state.model.message = Some("Image model download is paused.".into());
            done
        };
        done.notified().await;
        Ok(self.status().await)
    }

    /// REST calls this operation `cancel`; semantically it is a resumable pause.
    pub async fn cancel(&self, model_id: &str) -> Result<ImageModelServiceStatus, AppError> {
        self.pause(model_id).await
    }

    pub async fn delete(&self, model_id: &str) -> Result<ImageModelServiceStatus, AppError> {
        validate_model_id(model_id)?;
        let _mutation = self.mutation_lock.lock().await;
        if self.state.lock().await.active.is_some() {
            return Err(AppError::Conflict(
                "Cancel the image model installation before deleting it".into(),
            ));
        }
        let workload_gate = self
            .workload_gate
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        let _workload_permit = workload_gate.try_acquire_owned().map_err(|_| {
            AppError::Conflict("本地 AI 正忙，请完成或取消当前任务后再删除模型。".into())
        })?;

        let image = image_root(&self.root);
        remove_managed_tree(&self.root, &image).map_err(|error| {
            AppError::Internal(format!("remove managed image model files: {error}"))
        })?;
        prepare_layout(&self.root, &self.artifacts).map_err(|error| {
            AppError::Internal(format!("recreate image model directory: {error}"))
        })?;

        let mut state = self.state.lock().await;
        self.verified_bundle.store(false, Ordering::Release);
        state.model = empty_model_state(&self.artifacts, self.supported_platform);
        state.last_error = None;
        let snapshot = snapshot(&state, false, false);
        drop(state);
        self.sync_projection(false).await?;
        Ok(snapshot)
    }

    /// Resolve the verified install into the one-shot creation backend config.
    pub async fn sd_cli_config(&self) -> Result<SdCliZImageConfig, AppError> {
        let installed = self.state.lock().await.model.install_phase
            == ImageModelInstallPhase::Installed;
        if !installed || !installed_manifest_is_current(&self.root, &self.artifacts).await {
            if installed {
                self.invalidate_ready_bundle(
                    LocalModelErrorKind::ChecksumMismatch,
                    "本地图片模型文件发生变化，请重新安装。",
                )
                .await;
            }
            return Err(AppError::Conflict(
                "The local image model is not fully installed".into(),
            ));
        }
        self.verify_bundle_before_use().await?;

        let executable = find_runtime_executable(&runtime_install_dir(&self.root)).map_err(
            |error| AppError::Internal(format!("validate local image runtime: {error}")),
        )?;
        let diffusion_model = model_artifact_path(
            &self.root,
            artifact_for(&self.artifacts, ImageModelComponent::DiffusionModel)?,
        );
        let text_encoder = model_artifact_path(
            &self.root,
            artifact_for(&self.artifacts, ImageModelComponent::TextEncoder)?,
        );
        let vae = model_artifact_path(
            &self.root,
            artifact_for(&self.artifacts, ImageModelComponent::Vae)?,
        );
        for (path, component) in [
            (&diffusion_model, ImageModelComponent::DiffusionModel),
            (&text_encoder, ImageModelComponent::TextEncoder),
            (&vae, ImageModelComponent::Vae),
        ] {
            let artifact = artifact_for(&self.artifacts, component)?;
            prepare_managed_file(&self.root, path).map_err(|error| {
                AppError::Internal(format!("validate local image artifact path: {error}"))
            })?;
            if std::fs::metadata(path)
                .map(|metadata| metadata.len() != artifact.size || !metadata.is_file())
                .unwrap_or(true)
            {
                return Err(AppError::Conflict(
                    "The local image model files changed and must be repaired".into(),
                ));
            }
        }
        let job_root = image_root(&self.root).join(JOBS_DIR);
        prepare_managed_directory(&self.root, &job_root).map_err(|error| {
            AppError::Internal(format!("prepare local image job directory: {error}"))
        })?;
        Ok(SdCliZImageConfig::new(
            executable,
            diffusion_model,
            text_encoder,
            vae,
            job_root,
        ))
    }

    async fn verify_bundle_before_use(&self) -> Result<(), AppError> {
        if self.verified_bundle.load(Ordering::Acquire) {
            return Ok(());
        }
        let _verification = self.verification_lock.lock().await;
        if self.verified_bundle.load(Ordering::Acquire) {
            return Ok(());
        }
        let cancel = CancellationToken::new();
        for artifact in &self.artifacts {
            let path = artifact_path(&self.root, artifact);
            match verified_file(&self.root, &path, artifact, &cancel).await {
                Ok(true) => {}
                Ok(false) | Err(_) => {
                    self.invalidate_ready_bundle(
                        LocalModelErrorKind::ChecksumMismatch,
                        "本地图片模型完整性校验失败，请重新安装。",
                    )
                    .await;
                    return Err(AppError::Conflict(
                        "The local image model failed integrity verification".into(),
                    ));
                }
            }
        }

        // The archive itself was just verified. Rebuild native runtime files
        // on the first use of each process so an equal-sized modified binary
        // can never be launched based on manifest sizes alone.
        let runtime = artifact_for(&self.artifacts, ImageModelComponent::Runtime)?;
        if let Err(error) = self.extract_runtime(runtime, 0, &cancel, true).await {
            warn!(error = %error.detail, "could not restore verified local image runtime");
            self.invalidate_ready_bundle(
                LocalModelErrorKind::RuntimeUnavailable,
                "本地图片运行组件校验失败，请重新安装。",
            )
            .await;
            return Err(AppError::Conflict(
                "The local image runtime failed integrity verification".into(),
            ));
        }
        if !self.allow_insecure_loopback_downloads
            && let Err(error) = smoke_test_runtime(&self.root).await
        {
            warn!(error = %error.detail, "local image runtime smoke test failed");
            self.invalidate_ready_bundle(
                LocalModelErrorKind::RuntimeUnavailable,
                "本地图片运行组件与当前系统不兼容，请删除后等待版本更新。",
            )
            .await;
            return Err(AppError::Conflict(
                "The local image runtime is incompatible with this system".into(),
            ));
        }
        self.verified_bundle.store(true, Ordering::Release);
        Ok(())
    }

    async fn invalidate_ready_bundle(&self, kind: LocalModelErrorKind, message: &str) {
        self.verified_bundle.store(false, Ordering::Release);
        let _ = remove_state_file_if_exists(&self.root).await;
        let mut state = self.state.lock().await;
        state.model.install_phase = ImageModelInstallPhase::Failed;
        state.model.error_kind = Some(kind);
        state.model.message = Some(message.to_owned());
        state.last_error = Some(message.to_owned());
        drop(state);
        if let Err(error) = self.sync_projection(false).await {
            warn!(error = %error, "could not remove invalid local image model projection");
        }
    }

    async fn run_install(self: Arc<Self>, generation: u64, cancel: CancellationToken) {
        let result = self.install_artifacts(generation, &cancel).await;
        let mut state = self.state.lock().await;
        if !state
            .active
            .as_ref()
            .is_some_and(|active| active.generation == generation)
        {
            return;
        }
        state.active = None;
        refresh_totals(&mut state.model);

        match result {
            Ok(()) => {
                self.verified_bundle.store(true, Ordering::Release);
                state.model.install_phase = ImageModelInstallPhase::Installed;
                state.model.installed_bytes = total_download_size(&self.artifacts);
                state.model.error_kind = None;
                state.model.message = Some("Local Z-Image is installed and ready to use.".into());
                state.last_error = None;
                info!(model = Z_IMAGE_TURBO_MODEL_ID, "local image model installed");
            }
            Err(error) if error.cancelled => {
                self.verified_bundle.store(false, Ordering::Release);
                state.model.install_phase = ImageModelInstallPhase::Paused;
                state.model.error_kind = None;
                state.model.message = Some(error.safe_message.into());
                state.last_error = None;
                for progress in &mut state.model.component_progress {
                    if matches!(
                        progress.install_phase,
                        ImageModelInstallPhase::Downloading
                            | ImageModelInstallPhase::Verifying
                            | ImageModelInstallPhase::Extracting
                    ) {
                        progress.install_phase = ImageModelInstallPhase::Paused;
                        progress.bytes_per_second = 0;
                        progress.message = Some(error.safe_message.into());
                    }
                }
            }
            Err(error) => {
                self.verified_bundle.store(false, Ordering::Release);
                warn!(error = %error.detail, model = Z_IMAGE_TURBO_MODEL_ID, "image model install failed");
                state.model.install_phase = ImageModelInstallPhase::Failed;
                state.model.error_kind = Some(error.kind);
                state.model.message = Some(error.safe_message.into());
                state.last_error = Some(error.safe_message.into());
                for progress in &mut state.model.component_progress {
                    if matches!(
                        progress.install_phase,
                        ImageModelInstallPhase::Downloading
                            | ImageModelInstallPhase::Verifying
                            | ImageModelInstallPhase::Extracting
                    ) {
                        progress.install_phase = ImageModelInstallPhase::Failed;
                        progress.bytes_per_second = 0;
                        progress.error_kind = Some(error.kind);
                        progress.message = Some(error.safe_message.into());
                    }
                }
            }
        }
        let installed = state.model.install_phase == ImageModelInstallPhase::Installed;
        drop(state);
        if let Err(error) = self.sync_projection(installed).await {
            warn!(error = %error, installed, "could not synchronize local image model projection");
        }
    }

    async fn install_artifacts(
        &self,
        generation: u64,
        cancel: &CancellationToken,
    ) -> Result<(), ImageFailure> {
        self.ensure_bundle_space().await?;
        remove_state_file_if_exists(&self.root).await?;
        for artifact in &self.artifacts {
            if cancel.is_cancelled() {
                return Err(ImageFailure::cancelled());
            }
            let destination = artifact_path(&self.root, artifact);
            prepare_managed_file(&self.root, &destination).map_err(storage_failure)?;
            if file_len(&destination).await == artifact.size {
                self.set_component_phase(
                    generation,
                    artifact.component,
                    ImageModelInstallPhase::Verifying,
                )
                .await;
            }
            let verified = verified_file(&self.root, &destination, artifact, cancel).await?;
            if !verified {
                remove_file_if_exists(&self.root, &destination).await?;
                let mut last_error = None;
                for source in download_sources(&artifact.url) {
                    match self
                        .download_once(&source, artifact, &destination, generation, cancel)
                        .await
                    {
                        Ok(()) => {
                            last_error = None;
                            break;
                        }
                        Err(error) if error.cancelled => return Err(error),
                        Err(error) => {
                            warn!(component = ?artifact.component, error = %error.detail, "image artifact source failed");
                            last_error = Some(error);
                        }
                    }
                }
                if let Some(error) = last_error {
                    return Err(error);
                }
            }

            if artifact.kind == ArtifactKind::RuntimeZip {
                self.extract_runtime(artifact, generation, cancel, false).await?;
            }
            self.set_component_installed(generation, artifact).await;
        }
        if cancel.is_cancelled() {
            return Err(ImageFailure::cancelled());
        }
        if !self.allow_insecure_loopback_downloads {
            smoke_test_runtime(&self.root).await?;
        }
        write_installed_manifest(&self.root, &self.artifacts).await?;
        Ok(())
    }

    async fn ensure_bundle_space(&self) -> Result<(), ImageFailure> {
        let mut remaining = 0_u64;
        for artifact in &self.artifacts {
            let destination = artifact_path(&self.root, artifact);
            if file_len(&destination).await == artifact.size {
                continue;
            }
            remaining = remaining.saturating_add(
                artifact
                    .size
                    .saturating_sub(file_len(&partial_path(&self.root, artifact)).await),
            );
        }
        let required = remaining
            .saturating_add(RUNTIME_EXTRACT_RESERVE_BYTES)
            .saturating_add(DISK_SAFETY_BYTES);
        let path = image_root(&self.root);
        let available = tokio::task::spawn_blocking(move || fs2::available_space(path))
            .await
            .map_err(|error| {
                ImageFailure::new(
                    LocalModelErrorKind::Unknown,
                    "Could not inspect available storage for the image model.",
                    error.to_string(),
                )
            })?
            .map_err(|error| {
                ImageFailure::new(
                    LocalModelErrorKind::Unknown,
                    "Could not inspect available storage for the image model.",
                    error.to_string(),
                )
            })?;
        if available < required {
            return Err(ImageFailure::new(
                LocalModelErrorKind::InsufficientSpace,
                "There is not enough free space to install the image model.",
                format!("required {required}, available {available}"),
            ));
        }
        Ok(())
    }

    async fn download_once(
        &self,
        url: &str,
        artifact: &ImageArtifact,
        destination: &Path,
        generation: u64,
        cancel: &CancellationToken,
    ) -> Result<(), ImageFailure> {
        let part = partial_path(&self.root, artifact);
        prepare_managed_file(&self.root, &part).map_err(storage_failure)?;
        let mut offset = file_len(&part).await;
        if offset > artifact.size {
            remove_file_if_exists(&self.root, &part).await?;
            offset = 0;
        }
        if offset == artifact.size {
            self.set_component_phase(
                generation,
                artifact.component,
                ImageModelInstallPhase::Verifying,
            )
            .await;
            if hash_file(&part, cancel).await? == artifact.sha256 {
                commit_partial(&self.root, &part, destination).await?;
                return Ok(());
            }
            remove_file_if_exists(&self.root, &part).await?;
            offset = 0;
        }
        if cancel.is_cancelled() {
            return Err(ImageFailure::cancelled());
        }
        self.set_progress(generation, artifact, offset, 0).await;

        let mut request = self.http_client.get(url);
        if offset > 0 {
            request = request.header(RANGE, format!("bytes={offset}-"));
        }
        let response = tokio::select! {
            _ = cancel.cancelled() => return Err(ImageFailure::cancelled()),
            response = request.send() => response.map_err(|error| ImageFailure::new(
                LocalModelErrorKind::Network,
                "Image model download failed. Check the network and try again.",
                error.to_string(),
            ))?,
        };
        if !allowed_download_url(response.url())
            && !(self.allow_insecure_loopback_downloads && loopback_download_url(response.url()))
        {
            return Err(ImageFailure::new(
                LocalModelErrorKind::Network,
                "The image model download source did not pass safety checks.",
                "redirected to a disallowed host",
            ));
        }

        let status = response.status();
        let mut append = false;
        if offset > 0 && status == reqwest::StatusCode::PARTIAL_CONTENT {
            let range = response
                .headers()
                .get(CONTENT_RANGE)
                .and_then(|value| value.to_str().ok())
                .and_then(parse_content_range)
                .ok_or_else(|| {
                    ImageFailure::new(
                        LocalModelErrorKind::Network,
                        "The image download server returned an invalid resume response.",
                        "missing or invalid Content-Range",
                    )
                })?;
            if range != (offset, artifact.size.saturating_sub(1), artifact.size) {
                return Err(ImageFailure::new(
                    LocalModelErrorKind::Network,
                    "The image download server returned a mismatched resume range.",
                    format!("unexpected Content-Range {range:?}"),
                ));
            }
            append = true;
        } else if offset > 0 && status.is_success() {
            // A 200 response ignored Range. Truncate and safely consume it as a
            // fresh full response; appending would corrupt the artifact.
            offset = 0;
        } else if !status.is_success() {
            return Err(ImageFailure::new(
                LocalModelErrorKind::Network,
                "The image model download service is temporarily unavailable.",
                format!("HTTP status {status}"),
            ));
        }

        if let Some(length) = response
            .headers()
            .get(CONTENT_LENGTH)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.parse::<u64>().ok())
        {
            let expected = artifact.size.saturating_sub(offset);
            if length != expected {
                return Err(ImageFailure::new(
                    LocalModelErrorKind::Network,
                    "The image model download has an unexpected size.",
                    format!("Content-Length {length}, expected {expected}"),
                ));
            }
        }

        let mut options = tokio::fs::OpenOptions::new();
        options.create(true).write(true);
        if append {
            options.append(true);
        }
        let mut file = options.open(&part).await.map_err(|error| {
            ImageFailure::new(
                LocalModelErrorKind::Unknown,
                "Could not write the image model download.",
                error.to_string(),
            )
        })?;
        prepare_managed_file(&self.root, &part).map_err(storage_failure)?;
        if !file
            .metadata()
            .await
            .map(|metadata| metadata.is_file())
            .unwrap_or(false)
        {
            return Err(storage_failure(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "opened image partial is not a regular file",
            )));
        }
        if !append {
            file.set_len(0).await.map_err(|error| {
                ImageFailure::new(
                    LocalModelErrorKind::Unknown,
                    "Could not reset the image model download.",
                    error.to_string(),
                )
            })?;
        }

        let mut downloaded = offset;
        let started = Instant::now();
        let mut last_report = Instant::now();
        let mut stream = response.bytes_stream();
        loop {
            let next = tokio::select! {
                _ = cancel.cancelled() => {
                    file.sync_data().await.map_err(|error| ImageFailure::new(
                        LocalModelErrorKind::Unknown,
                        "Could not preserve the paused image model download.",
                        error.to_string(),
                    ))?;
                    return Err(ImageFailure::cancelled());
                }
                next = stream.next() => next,
            };
            let Some(chunk) = next else { break };
            let chunk = chunk.map_err(|error| {
                ImageFailure::new(
                    LocalModelErrorKind::Network,
                    "Image model download was interrupted and can be resumed.",
                    error.to_string(),
                )
            })?;
            let new_total = downloaded.saturating_add(chunk.len() as u64);
            if new_total > artifact.size {
                drop(file);
                remove_file_if_exists(&self.root, &part).await?;
                return Err(ImageFailure::new(
                    LocalModelErrorKind::Network,
                    "The image model download exceeded its expected size.",
                    format!("received more than {} bytes", artifact.size),
                ));
            }
            file.write_all(&chunk).await.map_err(|error| {
                ImageFailure::new(
                    LocalModelErrorKind::Unknown,
                    "Could not write the image model download.",
                    error.to_string(),
                )
            })?;
            downloaded = new_total;
            if last_report.elapsed() >= DOWNLOAD_PROGRESS_INTERVAL {
                let rate = ((downloaded.saturating_sub(offset)) as f64
                    / started.elapsed().as_secs_f64().max(0.001)) as u64;
                self.set_progress(generation, artifact, downloaded, rate).await;
                last_report = Instant::now();
            }
        }
        file.sync_all().await.map_err(|error| {
            ImageFailure::new(
                LocalModelErrorKind::Unknown,
                "Could not commit the image model download.",
                error.to_string(),
            )
        })?;
        drop(file);

        if downloaded != artifact.size {
            return Err(ImageFailure::new(
                LocalModelErrorKind::Network,
                "Image model download was interrupted and can be resumed.",
                format!("downloaded {downloaded} of {}", artifact.size),
            ));
        }
        self.set_component_phase(
            generation,
            artifact.component,
            ImageModelInstallPhase::Verifying,
        )
        .await;
        let actual = hash_file(&part, cancel).await?;
        if actual != artifact.sha256 {
            remove_file_if_exists(&self.root, &part).await?;
            return Err(ImageFailure::new(
                LocalModelErrorKind::ChecksumMismatch,
                "Image model integrity verification failed. Download it again.",
                format!("SHA-256 mismatch for {:?}", artifact.component),
            ));
        }
        commit_partial(&self.root, &part, destination).await
    }

    async fn extract_runtime(
        &self,
        artifact: &ImageArtifact,
        generation: u64,
        cancel: &CancellationToken,
        force: bool,
    ) -> Result<(), ImageFailure> {
        debug_assert_eq!(artifact.kind, ArtifactKind::RuntimeZip);
        if !force && runtime_install_ready(&self.root).is_ok() {
            return Ok(());
        }
        self.set_component_phase(
            generation,
            ImageModelComponent::Runtime,
            ImageModelInstallPhase::Extracting,
        )
        .await;

        let archive = artifact_path(&self.root, artifact);
        let staging = runtime_staging_dir(&self.root);
        let destination = runtime_install_dir(&self.root);
        remove_managed_tree(&self.root, &staging).map_err(storage_failure)?;
        prepare_managed_directory(&self.root, &staging).map_err(storage_failure)?;
        if cancel.is_cancelled() {
            return Err(ImageFailure::cancelled());
        }

        let archive_for_task = archive.clone();
        let staging_for_task = staging.clone();
        let extracted = tokio::task::spawn_blocking(move || {
            extract_runtime_zip(&archive_for_task, &staging_for_task)
        })
        .await
        .map_err(|error| {
            ImageFailure::new(
                LocalModelErrorKind::RuntimeUnavailable,
                "Could not unpack the local image runtime.",
                error.to_string(),
            )
        })?
        .map_err(|error| {
            ImageFailure::new(
                LocalModelErrorKind::RuntimeUnavailable,
                "Could not unpack the local image runtime.",
                error.to_string(),
            )
        });
        if let Err(error) = extracted {
            let _ = remove_managed_tree(&self.root, &staging);
            return Err(error);
        }
        if cancel.is_cancelled() {
            remove_managed_tree(&self.root, &staging).map_err(storage_failure)?;
            return Err(ImageFailure::cancelled());
        }
        find_runtime_executable(&staging).map_err(|error| {
            let _ = remove_managed_tree(&self.root, &staging);
            ImageFailure::new(
                LocalModelErrorKind::RuntimeUnavailable,
                "The local image runtime archive is incomplete.",
                error.to_string(),
            )
        })?;
        remove_managed_tree(&self.root, &destination).map_err(storage_failure)?;
        prepare_managed_directory(
            &self.root,
            destination.parent().ok_or_else(|| {
                storage_failure(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    "runtime destination has no parent",
                ))
            })?,
        )
        .map_err(storage_failure)?;
        std::fs::rename(&staging, &destination).map_err(|error| {
            ImageFailure::new(
                LocalModelErrorKind::RuntimeUnavailable,
                "Could not activate the local image runtime.",
                error.to_string(),
            )
        })?;
        runtime_install_ready(&self.root).map_err(|error| {
            ImageFailure::new(
                LocalModelErrorKind::RuntimeUnavailable,
                "The local image runtime did not pass safety checks.",
                error.to_string(),
            )
        })
    }

    async fn set_progress(
        &self,
        generation: u64,
        artifact: &ImageArtifact,
        downloaded: u64,
        bytes_per_second: u64,
    ) {
        let mut state = self.state.lock().await;
        if !state
            .active
            .as_ref()
            .is_some_and(|active| active.generation == generation)
        {
            return;
        }
        state.model.install_phase = ImageModelInstallPhase::Downloading;
        if let Some(progress) = component_progress_mut(&mut state.model, artifact.component) {
            progress.install_phase = ImageModelInstallPhase::Downloading;
            progress.downloaded_bytes = downloaded;
            progress.total_bytes = artifact.size;
            progress.bytes_per_second = bytes_per_second;
            progress.error_kind = None;
            progress.message = None;
        }
        refresh_totals(&mut state.model);
    }

    async fn set_component_phase(
        &self,
        generation: u64,
        component: ImageModelComponent,
        phase: ImageModelInstallPhase,
    ) {
        let mut state = self.state.lock().await;
        if !state
            .active
            .as_ref()
            .is_some_and(|active| active.generation == generation)
        {
            return;
        }
        state.model.install_phase = phase;
        if let Some(progress) = component_progress_mut(&mut state.model, component) {
            progress.install_phase = phase;
            progress.bytes_per_second = 0;
            progress.error_kind = None;
            progress.message = None;
        }
    }

    async fn set_component_installed(&self, generation: u64, artifact: &ImageArtifact) {
        let mut state = self.state.lock().await;
        if !state
            .active
            .as_ref()
            .is_some_and(|active| active.generation == generation)
        {
            return;
        }
        if let Some(progress) = component_progress_mut(&mut state.model, artifact.component) {
            progress.install_phase = ImageModelInstallPhase::Installed;
            progress.downloaded_bytes = artifact.size;
            progress.total_bytes = artifact.size;
            progress.installed_bytes = artifact.size;
            progress.bytes_per_second = 0;
            progress.error_kind = None;
            progress.message = None;
        }
        refresh_totals(&mut state.model);
    }
}

/// Curated image metadata without creating directories or downloader state.
pub fn image_model_catalog() -> Vec<ImageModelCatalogEntry> {
    let (artifacts, _) = production_artifacts();
    vec![catalog_entry(&artifacts)]
}

/// Fresh-install status used before the lazy local runtime is initialized.
pub fn inactive_image_model_status() -> ImageModelServiceStatus {
    let (_, supported) = production_artifacts();
    let model = ImageModelState {
        model_id: Z_IMAGE_TURBO_MODEL_ID.into(),
        install_phase: ImageModelInstallPhase::NotInstalled,
        component_progress: component_order()
            .iter()
            .copied()
            .map(|component| ImageModelComponentProgress {
                component,
                install_phase: ImageModelInstallPhase::NotInstalled,
                downloaded_bytes: 0,
                total_bytes: 0,
                installed_bytes: 0,
                bytes_per_second: 0,
                error_kind: None,
                message: None,
            })
            .collect(),
        installed_bytes: 0,
        error_kind: (!supported).then_some(LocalModelErrorKind::UnsupportedPlatform),
        message: None,
    };
    ImageModelServiceStatus {
        protocol_version: IMAGE_PROTOCOL_VERSION.into(),
        artifacts_ready: false,
        inference_ready: false,
        runtime_phase: ImageModelRuntimePhase::Unavailable,
        models: vec![model],
        last_error: None,
    }
}

fn production_artifacts() -> (Vec<ImageArtifact>, bool) {
    let mut artifacts = Vec::with_capacity(4);
    let supported = if let Some(runtime) = current_sd_cpp_runtime_artifact() {
        artifacts.push(ImageArtifact {
            component: ImageModelComponent::Runtime,
            kind: ArtifactKind::RuntimeZip,
            file_name: runtime.archive_name.into(),
            url: runtime.url.into(),
            size: runtime.size,
            sha256: runtime.sha256.into(),
        });
        true
    } else {
        false
    };
    artifacts.extend(Z_IMAGE_TURBO_ARTIFACTS.iter().map(|artifact| ImageArtifact {
        component: match artifact.role {
            ZImageArtifactRole::DiffusionModel => ImageModelComponent::DiffusionModel,
            ZImageArtifactRole::TextEncoder => ImageModelComponent::TextEncoder,
            ZImageArtifactRole::Vae => ImageModelComponent::Vae,
        },
        kind: ArtifactKind::Model,
        file_name: artifact.file_name.into(),
        url: artifact.url.into(),
        size: artifact.size,
        sha256: artifact.sha256.into(),
    }));
    (artifacts, supported)
}

fn catalog_entry(artifacts: &[ImageArtifact]) -> ImageModelCatalogEntry {
    let vae_notice = Z_IMAGE_TURBO_ARTIFACTS
        .iter()
        .find(|artifact| artifact.role == ZImageArtifactRole::Vae)
        .and_then(|artifact| artifact.notice)
        .map(str::to_owned);
    ImageModelCatalogEntry {
        id: Z_IMAGE_TURBO_MODEL_ID.into(),
        name: "Z-Image Turbo (Q3_K)".into(),
        description: "Fast local text-to-image generation for consumer computers.".into(),
        format: "GGUF + SafeTensors".into(),
        download_size_bytes: total_download_size(artifacts),
        required_memory_bytes: 12 * 1024 * 1024 * 1024,
        license: "Apache-2.0 model; MIT runtime; see VAE notice".into(),
        source: "Z-Image Turbo, Qwen3 text encoder, Comfy-Org VAE and stable-diffusion.cpp"
            .into(),
        components: component_order().to_vec(),
        recommended: true,
        notice: vae_notice,
    }
}

fn validate_model_id(model_id: &str) -> Result<(), AppError> {
    if model_id == Z_IMAGE_TURBO_MODEL_ID {
        Ok(())
    } else {
        Err(AppError::NotFound(
            "Image model is not in the curated catalog".into(),
        ))
    }
}

fn artifact_for(
    artifacts: &[ImageArtifact],
    component: ImageModelComponent,
) -> Result<&ImageArtifact, AppError> {
    artifacts
        .iter()
        .find(|artifact| artifact.component == component)
        .ok_or_else(|| AppError::Internal(format!("missing image component {component:?}")))
}

fn component_order() -> [ImageModelComponent; 4] {
    [
        ImageModelComponent::Runtime,
        ImageModelComponent::DiffusionModel,
        ImageModelComponent::TextEncoder,
        ImageModelComponent::Vae,
    ]
}

fn empty_model_state(artifacts: &[ImageArtifact], supported: bool) -> ImageModelState {
    let mut progress = Vec::with_capacity(4);
    for component in component_order() {
        let artifact = artifacts
            .iter()
            .find(|artifact| artifact.component == component);
        let unsupported_runtime = component == ImageModelComponent::Runtime && !supported;
        progress.push(ImageModelComponentProgress {
            component,
            install_phase: if unsupported_runtime {
                ImageModelInstallPhase::Failed
            } else {
                ImageModelInstallPhase::NotInstalled
            },
            downloaded_bytes: 0,
            total_bytes: artifact.map_or(0, |artifact| artifact.size),
            installed_bytes: 0,
            bytes_per_second: 0,
            error_kind: unsupported_runtime.then_some(LocalModelErrorKind::UnsupportedPlatform),
            message: unsupported_runtime
                .then(|| "Local image generation is unavailable on this platform.".into()),
        });
    }
    ImageModelState {
        model_id: Z_IMAGE_TURBO_MODEL_ID.into(),
        install_phase: if supported {
            ImageModelInstallPhase::NotInstalled
        } else {
            ImageModelInstallPhase::Failed
        },
        component_progress: progress,
        installed_bytes: 0,
        error_kind: (!supported).then_some(LocalModelErrorKind::UnsupportedPlatform),
        message: (!supported)
            .then(|| "Local image generation is unavailable on this platform.".into()),
    }
}

fn component_progress_mut(
    model: &mut ImageModelState,
    component: ImageModelComponent,
) -> Option<&mut ImageModelComponentProgress> {
    model
        .component_progress
        .iter_mut()
        .find(|progress| progress.component == component)
}

fn refresh_totals(model: &mut ImageModelState) {
    model.installed_bytes = model
        .component_progress
        .iter()
        .map(|progress| progress.installed_bytes)
        .sum();
}

fn total_download_size(artifacts: &[ImageArtifact]) -> u64 {
    // Keep the creation constant as the source of truth for the three model
    // files while allowing a platform-specific runtime size.
    Z_IMAGE_TURBO_DOWNLOAD_SIZE.saturating_add(
        artifacts
            .iter()
            .find(|artifact| artifact.component == ImageModelComponent::Runtime)
            .map_or(0, |artifact| artifact.size),
    )
}

fn snapshot(
    state: &MutableState,
    verified: bool,
    workload_busy: bool,
) -> ImageModelServiceStatus {
    let ready = state.model.install_phase == ImageModelInstallPhase::Installed;
    let inference_ready = ready && verified;
    let runtime_phase = if inference_ready && workload_busy {
        ImageModelRuntimePhase::Busy
    } else if inference_ready {
        ImageModelRuntimePhase::Ready
    } else if state.model.error_kind == Some(LocalModelErrorKind::RuntimeUnavailable) {
        ImageModelRuntimePhase::Failed
    } else {
        ImageModelRuntimePhase::Unavailable
    };
    ImageModelServiceStatus {
        protocol_version: IMAGE_PROTOCOL_VERSION.into(),
        artifacts_ready: ready,
        inference_ready,
        runtime_phase,
        models: vec![state.model.clone()],
        last_error: state.last_error.clone(),
    }
}

async fn inspect_model_state(
    root: &Path,
    artifacts: &[ImageArtifact],
    supported: bool,
) -> ImageModelState {
    let mut model = empty_model_state(artifacts, supported);
    if !supported {
        return model;
    }
    let manifest_ready = installed_manifest_is_current(root, artifacts).await;
    let mut has_partial_or_final = false;
    let mut corrupt = false;

    for artifact in artifacts {
        let final_path = artifact_path(root, artifact);
        let part_path = partial_path(root, artifact);
        let final_len = file_len(&final_path).await;
        let part_len = file_len(&part_path).await;
        let runtime_ready = artifact.kind != ArtifactKind::RuntimeZip
            || runtime_install_ready(root).is_ok();
        let installed = manifest_ready && final_len == artifact.size && runtime_ready;
        let artifact_corrupt =
            (final_len > 0 && final_len != artifact.size) || part_len > artifact.size;
        if final_len > 0 || part_len > 0 {
            has_partial_or_final = true;
        }
        corrupt |= artifact_corrupt;
        if let Some(progress) = component_progress_mut(&mut model, artifact.component) {
            progress.downloaded_bytes = if final_len == artifact.size {
                artifact.size
            } else {
                part_len.min(artifact.size)
            };
            progress.installed_bytes = if installed { artifact.size } else { 0 };
            progress.install_phase = if installed {
                ImageModelInstallPhase::Installed
            } else if artifact_corrupt {
                ImageModelInstallPhase::Failed
            } else if final_len > 0 || part_len > 0 {
                ImageModelInstallPhase::Paused
            } else {
                ImageModelInstallPhase::NotInstalled
            };
            progress.error_kind =
                artifact_corrupt.then_some(LocalModelErrorKind::ChecksumMismatch);
            progress.message = match progress.install_phase {
                ImageModelInstallPhase::Paused => {
                    Some("This image model component can be resumed.".into())
                }
                ImageModelInstallPhase::Failed => {
                    Some("This image model component needs repair.".into())
                }
                _ => None,
            };
        }
    }
    refresh_totals(&mut model);
    if manifest_ready
        && model
            .component_progress
            .iter()
            .all(|progress| progress.install_phase == ImageModelInstallPhase::Installed)
    {
        model.install_phase = ImageModelInstallPhase::Installed;
        model.message = Some("Local Z-Image is installed and ready to use.".into());
    } else if corrupt {
        model.install_phase = ImageModelInstallPhase::Failed;
        model.error_kind = Some(LocalModelErrorKind::ChecksumMismatch);
        model.message = Some("Image model files need repair before they can be used.".into());
    } else if has_partial_or_final {
        model.install_phase = ImageModelInstallPhase::Paused;
        model.message = Some("The image model installation can be resumed.".into());
    }
    model
}

fn expected_manifest(artifacts: &[ImageArtifact]) -> InstalledManifest {
    InstalledManifest {
        version: STATE_VERSION,
        model_id: Z_IMAGE_TURBO_MODEL_ID.into(),
        runtime_version: SD_CPP_RUNTIME_VERSION.into(),
        artifacts: artifacts
            .iter()
            .map(|artifact| ManifestArtifact {
                component: artifact.component,
                file_name: artifact.file_name.clone(),
                size: artifact.size,
                sha256: artifact.sha256.clone(),
            })
            .collect(),
    }
}

async fn installed_manifest_is_current(root: &Path, artifacts: &[ImageArtifact]) -> bool {
    let path = state_path(root);
    if prepare_managed_file(root, &path).is_err() {
        return false;
    }
    let Ok(bytes) = tokio::fs::read(&path).await else {
        return false;
    };
    let Ok(manifest) = serde_json::from_slice::<InstalledManifest>(&bytes) else {
        return false;
    };
    let expected = expected_manifest(artifacts);
    if manifest.version != expected.version
        || manifest.model_id != expected.model_id
        || manifest.runtime_version != expected.runtime_version
        || manifest.artifacts != expected.artifacts
    {
        return false;
    }
    for artifact in artifacts {
        let path = artifact_path(root, artifact);
        if prepare_managed_file(root, &path).is_err()
            || std::fs::metadata(&path)
                .map(|metadata| !metadata.is_file() || metadata.len() != artifact.size)
                .unwrap_or(true)
        {
            return false;
        }
    }
    runtime_install_ready(root).is_ok()
}

async fn write_installed_manifest(
    root: &Path,
    artifacts: &[ImageArtifact],
) -> Result<(), ImageFailure> {
    let state = state_path(root);
    let temporary = state_tmp_path(root);
    prepare_managed_file(root, &state).map_err(storage_failure)?;
    prepare_managed_file(root, &temporary).map_err(storage_failure)?;
    let bytes = serde_json::to_vec_pretty(&expected_manifest(artifacts)).map_err(|error| {
        ImageFailure::new(
            LocalModelErrorKind::Unknown,
            "Could not save the image model installation state.",
            error.to_string(),
        )
    })?;
    let mut file = tokio::fs::File::create(&temporary).await.map_err(|error| {
        ImageFailure::new(
            LocalModelErrorKind::Unknown,
            "Could not save the image model installation state.",
            error.to_string(),
        )
    })?;
    file.write_all(&bytes).await.map_err(|error| {
        ImageFailure::new(
            LocalModelErrorKind::Unknown,
            "Could not save the image model installation state.",
            error.to_string(),
        )
    })?;
    file.sync_all().await.map_err(|error| {
        ImageFailure::new(
            LocalModelErrorKind::Unknown,
            "Could not save the image model installation state.",
            error.to_string(),
        )
    })?;
    drop(file);
    remove_file_if_exists(root, &state).await?;
    tokio::fs::rename(&temporary, &state)
        .await
        .map_err(|error| {
            ImageFailure::new(
                LocalModelErrorKind::Unknown,
                "Could not commit the image model installation state.",
                error.to_string(),
            )
        })
}

async fn remove_state_file_if_exists(root: &Path) -> Result<(), ImageFailure> {
    remove_file_if_exists(root, &state_path(root)).await?;
    remove_file_if_exists(root, &state_tmp_path(root)).await
}

async fn verified_file(
    root: &Path,
    path: &Path,
    artifact: &ImageArtifact,
    cancel: &CancellationToken,
) -> Result<bool, ImageFailure> {
    prepare_managed_file(root, path).map_err(storage_failure)?;
    if file_len(path).await != artifact.size {
        return Ok(false);
    }
    Ok(hash_file(path, cancel).await? == artifact.sha256)
}

async fn hash_file(path: &Path, cancel: &CancellationToken) -> Result<String, ImageFailure> {
    let mut file = tokio::fs::File::open(path).await.map_err(|error| {
        ImageFailure::new(
            LocalModelErrorKind::Unknown,
            "Could not verify the image model file.",
            error.to_string(),
        )
    })?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0_u8; 1024 * 1024];
    loop {
        let count = tokio::select! {
            _ = cancel.cancelled() => return Err(ImageFailure::cancelled()),
            result = file.read(&mut buffer) => result.map_err(|error| ImageFailure::new(
                LocalModelErrorKind::Unknown,
                "Could not verify the image model file.",
                error.to_string(),
            ))?,
        };
        if count == 0 {
            break;
        }
        hasher.update(&buffer[..count]);
    }
    Ok(hex::encode(hasher.finalize()))
}

async fn file_len(path: &Path) -> u64 {
    tokio::fs::metadata(path)
        .await
        .map(|metadata| metadata.len())
        .unwrap_or(0)
}

fn image_root(root: &Path) -> PathBuf {
    root.join(IMAGE_DIR)
}

fn model_dir(root: &Path) -> PathBuf {
    image_root(root)
        .join(MODELS_DIR)
        .join(Z_IMAGE_TURBO_MODEL_ID)
}

fn downloads_dir(root: &Path) -> PathBuf {
    image_root(root).join(DOWNLOADS_DIR)
}

fn runtime_root(root: &Path) -> PathBuf {
    image_root(root).join(RUNTIME_DIR)
}

fn runtime_install_dir(root: &Path) -> PathBuf {
    runtime_root(root).join(format!(
        "{}-{}-{SD_CPP_RUNTIME_VERSION}",
        std::env::consts::OS,
        std::env::consts::ARCH
    ))
}

fn runtime_staging_dir(root: &Path) -> PathBuf {
    runtime_root(root).join(format!(
        ".extracting-{}-{}-{SD_CPP_RUNTIME_VERSION}",
        std::env::consts::OS,
        std::env::consts::ARCH
    ))
}

fn artifact_path(root: &Path, artifact: &ImageArtifact) -> PathBuf {
    match artifact.kind {
        ArtifactKind::RuntimeZip => downloads_dir(root).join(&artifact.file_name),
        ArtifactKind::Model => model_artifact_path(root, artifact),
    }
}

fn model_artifact_path(root: &Path, artifact: &ImageArtifact) -> PathBuf {
    model_dir(root).join(&artifact.file_name)
}

fn partial_path(root: &Path, artifact: &ImageArtifact) -> PathBuf {
    let key = match artifact.component {
        ImageModelComponent::Runtime => "runtime",
        ImageModelComponent::DiffusionModel => "diffusion-model",
        ImageModelComponent::TextEncoder => "text-encoder",
        ImageModelComponent::Vae => "vae",
    };
    downloads_dir(root).join(format!("{key}.part"))
}

fn state_path(root: &Path) -> PathBuf {
    image_root(root).join(STATE_FILE)
}

fn state_tmp_path(root: &Path) -> PathBuf {
    image_root(root).join(format!("{STATE_FILE}.tmp"))
}

fn prepare_layout(root: &Path, artifacts: &[ImageArtifact]) -> std::io::Result<()> {
    prepare_managed_directory(root, root)?;
    for directory in [
        image_root(root),
        runtime_root(root),
        model_dir(root),
        downloads_dir(root),
    ] {
        prepare_managed_directory(root, &directory)?;
    }
    for artifact in artifacts {
        prepare_managed_file(root, &artifact_path(root, artifact))?;
        prepare_managed_file(root, &partial_path(root, artifact))?;
    }
    prepare_managed_file(root, &state_path(root))?;
    prepare_managed_file(root, &state_tmp_path(root))?;
    Ok(())
}

async fn commit_partial(
    root: &Path,
    part: &Path,
    destination: &Path,
) -> Result<(), ImageFailure> {
    prepare_managed_file(root, part).map_err(storage_failure)?;
    prepare_managed_file(root, destination).map_err(storage_failure)?;
    remove_file_if_exists(root, destination).await?;
    tokio::fs::rename(part, destination).await.map_err(|error| {
        ImageFailure::new(
            LocalModelErrorKind::Unknown,
            "Could not complete the image model installation.",
            error.to_string(),
        )
    })
}

async fn remove_file_if_exists(root: &Path, path: &Path) -> Result<(), ImageFailure> {
    prepare_managed_file(root, path).map_err(storage_failure)?;
    match tokio::fs::remove_file(path).await {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(ImageFailure::new(
            LocalModelErrorKind::Unknown,
            "Could not update the image model files.",
            error.to_string(),
        )),
    }
}

fn storage_failure(error: std::io::Error) -> ImageFailure {
    ImageFailure::new(
        LocalModelErrorKind::Unknown,
        "Image model storage did not pass safety checks.",
        error.to_string(),
    )
}

fn runtime_install_ready(root: &Path) -> std::io::Result<()> {
    find_runtime_executable(&runtime_install_dir(root)).map(|_| ())
}

async fn smoke_test_runtime(root: &Path) -> Result<(), ImageFailure> {
    let executable = find_runtime_executable(&runtime_install_dir(root)).map_err(|error| {
        ImageFailure::new(
            LocalModelErrorKind::RuntimeUnavailable,
            "The local image runtime is incomplete.",
            error.to_string(),
        )
    })?;
    let mut command = nomifun_runtime::Builder::clean_cli(&executable);
    command
        .arg("--help")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    if let Some(directory) = executable.parent() {
        command.current_dir(directory);
        #[cfg(target_os = "linux")]
        command.env("LD_LIBRARY_PATH", directory);
        #[cfg(target_os = "macos")]
        command.env("DYLD_LIBRARY_PATH", directory);
    }
    let mut child = command.spawn().map_err(|error| {
        ImageFailure::new(
            LocalModelErrorKind::RuntimeUnavailable,
            "The local image runtime is not compatible with this system.",
            error.to_string(),
        )
    })?;
    match tokio::time::timeout(Duration::from_secs(10), child.wait()).await {
        Ok(Ok(status)) if status.success() => Ok(()),
        Ok(Ok(status)) => Err(ImageFailure::new(
            LocalModelErrorKind::RuntimeUnavailable,
            "The local image runtime is not compatible with this system.",
            format!("sd-cli --help exited with {status}"),
        )),
        Ok(Err(error)) => Err(ImageFailure::new(
            LocalModelErrorKind::RuntimeUnavailable,
            "The local image runtime could not be checked.",
            error.to_string(),
        )),
        Err(_) => {
            let _ = nomifun_runtime::kill_process_tree(&mut child).await;
            Err(ImageFailure::new(
                LocalModelErrorKind::RuntimeUnavailable,
                "The local image runtime compatibility check timed out.",
                "sd-cli --help timed out",
            ))
        }
    }
}

fn runtime_executable_name(name: &OsStr) -> bool {
    name == OsStr::new("sd-cli") || name == OsStr::new("sd-cli.exe")
}

#[cfg(any(test, unix))]
fn runtime_entry_mode(recorded: Option<u32>, name: &OsStr) -> u32 {
    if runtime_executable_name(name) {
        0o755
    } else {
        recorded.unwrap_or(0o644) & 0o777
    }
}

fn find_runtime_executable(root: &Path) -> std::io::Result<PathBuf> {
    let metadata = std::fs::symlink_metadata(root)?;
    if unsafe_link_or_reparse(&metadata) || !metadata.is_dir() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "runtime root is a link or not a directory",
        ));
    }
    let canonical_root = std::fs::canonicalize(root)?;
    let mut stack = vec![root.to_path_buf()];
    let mut executable = None;
    let mut visited = 0_usize;
    while let Some(directory) = stack.pop() {
        for entry in std::fs::read_dir(&directory)? {
            let entry = entry?;
            visited += 1;
            if visited > MAX_ARCHIVE_ENTRIES {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "runtime contains too many entries",
                ));
            }
            let path = entry.path();
            let metadata = std::fs::symlink_metadata(&path)?;
            if unsafe_link_or_reparse(&metadata) {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::PermissionDenied,
                    "runtime contains a link or reparse point",
                ));
            }
            let canonical = std::fs::canonicalize(&path)?;
            if !canonical.starts_with(&canonical_root) {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::PermissionDenied,
                    "runtime entry escaped its root",
                ));
            }
            if metadata.is_dir() {
                stack.push(path);
            } else if metadata.is_file() && runtime_executable_name(&entry.file_name()) {
                if executable.replace(path).is_some() {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        "runtime contains multiple sd-cli executables",
                    ));
                }
            } else if !metadata.is_file() {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "runtime contains an unsupported file type",
                ));
            }
        }
    }
    let executable = executable.ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "runtime does not contain sd-cli",
        )
    })?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if std::fs::metadata(&executable)?.permissions().mode() & 0o111 == 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "sd-cli is not executable",
            ));
        }
    }
    Ok(executable)
}

fn extract_runtime_zip(archive_path: &Path, destination: &Path) -> std::io::Result<()> {
    let file = File::open(archive_path)?;
    let mut archive = zip::ZipArchive::new(file)
        .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))?;
    if archive.len() > MAX_ARCHIVE_ENTRIES {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "runtime archive contains too many entries",
        ));
    }
    let mut seen = HashSet::new();
    let mut expanded = 0_u64;
    for index in 0..archive.len() {
        let mut entry = archive
            .by_index(index)
            .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))?;
        let relative = entry.enclosed_name().ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "unsafe path in image runtime archive",
            )
        })?;
        if relative.as_os_str().is_empty() || !safe_relative_path(&relative) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "unsafe path in image runtime archive",
            ));
        }
        let relative = relative.to_path_buf();
        if !seen.insert(relative.clone()) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "duplicate path in image runtime archive",
            ));
        }
        if let Some(mode) = entry.unix_mode() {
            let file_type = mode & 0o170000;
            if file_type != 0 && file_type != 0o100000 && file_type != 0o040000 {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "links and special files are not allowed in the runtime archive",
                ));
            }
        }
        expanded = expanded.checked_add(entry.size()).ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "runtime archive expanded size overflow",
            )
        })?;
        if expanded > MAX_ARCHIVE_EXPANDED_BYTES {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "runtime archive expands beyond the allowed limit",
            ));
        }

        let output = destination.join(&relative);
        if entry.is_dir() {
            std::fs::create_dir_all(&output)?;
            continue;
        }
        if let Some(parent) = output.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mut output_file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&output)?;
        std::io::copy(&mut entry, &mut output_file)?;
        output_file.sync_all()?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            // Upstream's Linux/macOS release ZIPs currently record `sd-cli`
            // as 0664. The archive itself is pinned and SHA-verified, so make
            // that one expected entry executable while keeping every other
            // file at its recorded (or conservative default) permission.
            let mode = runtime_entry_mode(
                entry.unix_mode(),
                output.file_name().unwrap_or_else(|| OsStr::new("")),
            );
            std::fs::set_permissions(&output, std::fs::Permissions::from_mode(mode))?;
        }
    }
    Ok(())
}

fn safe_relative_path(path: &Path) -> bool {
    !path.is_absolute()
        && path
            .components()
            .all(|component| matches!(component, Component::Normal(_) | Component::CurDir))
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
            "managed image directory has an unsafe relative path",
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
                        "managed image ancestor is a link or not a directory",
                    ));
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                std::fs::create_dir(&current)?;
                let metadata = std::fs::symlink_metadata(&current)?;
                if unsafe_link_or_reparse(&metadata) || !metadata.is_dir() {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::PermissionDenied,
                        "managed image directory creation was redirected",
                    ));
                }
            }
            Err(error) => return Err(error),
        }
    }
    let canonical_directory = std::fs::canonicalize(&current)?;
    if !canonical_directory.starts_with(canonical_root) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "managed image directory resolved outside its root",
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
            "managed image file has an unsafe relative path",
        ));
    }
    let parent = path.parent().ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "managed image file has no parent",
        )
    })?;
    prepare_managed_directory(root, parent)?;
    match std::fs::symlink_metadata(path) {
        Ok(metadata) => {
            if unsafe_link_or_reparse(&metadata) || !metadata.is_file() {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::PermissionDenied,
                    "managed image target is a link or not a regular file",
                ));
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    Ok(())
}

fn remove_managed_tree(root: &Path, path: &Path) -> std::io::Result<()> {
    let relative = path.strip_prefix(root).map_err(|_| {
        std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "managed tree escaped local AI root",
        )
    })?;
    if relative.as_os_str().is_empty() || !safe_relative_path(relative) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "refusing to remove unsafe managed image tree",
        ));
    }
    let metadata = match std::fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
    };
    if unsafe_link_or_reparse(&metadata) || !metadata.is_dir() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "managed image tree is a link or not a directory",
        ));
    }
    let canonical_root = std::fs::canonicalize(root)?;
    let canonical_path = std::fs::canonicalize(path)?;
    if canonical_path == canonical_root || !canonical_path.starts_with(&canonical_root) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "managed image tree resolved outside its root",
        ));
    }
    validate_tree_has_no_links(path, &canonical_path)?;
    std::fs::remove_dir_all(path)
}

fn validate_tree_has_no_links(directory: &Path, canonical_root: &Path) -> std::io::Result<()> {
    let mut stack = vec![directory.to_path_buf()];
    let mut visited = 0_usize;
    while let Some(current) = stack.pop() {
        for entry in std::fs::read_dir(current)? {
            let entry = entry?;
            visited += 1;
            if visited > MAX_ARCHIVE_ENTRIES * 4 {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "managed image tree contains too many entries",
                ));
            }
            let path = entry.path();
            let metadata = std::fs::symlink_metadata(&path)?;
            if unsafe_link_or_reparse(&metadata) {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::PermissionDenied,
                    "managed image tree contains a link or reparse point",
                ));
            }
            if !std::fs::canonicalize(&path)?.starts_with(canonical_root) {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::PermissionDenied,
                    "managed image tree entry escaped its root",
                ));
            }
            if metadata.is_dir() {
                stack.push(path);
            } else if !metadata.is_file() {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "managed image tree contains an unsupported file type",
                ));
            }
        }
    }
    Ok(())
}

fn image_download_client() -> reqwest::Client {
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
            warn!(error = %error, "Could not apply system proxy to image model downloader");
            build()
                .build()
                .expect("image model HTTP client configuration is valid")
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

fn loopback_download_url(url: &reqwest::Url) -> bool {
    matches!(url.scheme(), "http" | "https")
        && url
            .host_str()
            .is_some_and(|host| matches!(host, "localhost" | "127.0.0.1" | "::1"))
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

fn parse_content_range(value: &str) -> Option<(u64, u64, u64)> {
    let value = value.strip_prefix("bytes ")?;
    let (range, total) = value.split_once('/')?;
    let (start, end) = range.split_once('-')?;
    let start = start.parse::<u64>().ok()?;
    let end = end.parse::<u64>().ok()?;
    let total = total.parse::<u64>().ok()?;
    (start <= end && end < total).then_some((start, end, total))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write as _;
    use tempfile::TempDir;
    use wiremock::matchers::{header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};
    use zip::write::SimpleFileOptions;

    fn digest(bytes: &[u8]) -> String {
        hex::encode(Sha256::digest(bytes))
    }

    fn model_artifact(url: String, bytes: &[u8]) -> ImageArtifact {
        ImageArtifact {
            component: ImageModelComponent::DiffusionModel,
            kind: ArtifactKind::Model,
            file_name: "tiny.gguf".into(),
            url,
            size: bytes.len() as u64,
            sha256: digest(bytes),
        }
    }

    async fn test_service(temp: &TempDir, artifacts: Vec<ImageArtifact>) -> Arc<ImageModelService> {
        ImageModelService::new_inner(
            temp.path().join(LOCAL_AI_DIR),
            reqwest::Client::builder().build().unwrap(),
            artifacts,
            true,
            true,
        )
        .await
        .unwrap()
    }

    fn test_runtime_zip() -> Vec<u8> {
        let mut bytes = Vec::new();
        {
            let mut writer = zip::ZipWriter::new(std::io::Cursor::new(&mut bytes));
            writer
                .start_file(
                    if cfg!(windows) { "sd-cli.exe" } else { "sd-cli" },
                    // Mirrors the pinned Unix release archives: provisioning
                    // must add the executable bit after integrity verification.
                    SimpleFileOptions::default().unix_permissions(0o664),
                )
                .unwrap();
            writer.write_all(b"test runtime").unwrap();
            writer.finish().unwrap();
        }
        bytes
    }

    fn complete_test_artifacts() -> Vec<(ImageArtifact, Vec<u8>)> {
        let runtime = test_runtime_zip();
        [
            (
                ImageModelComponent::Runtime,
                ArtifactKind::RuntimeZip,
                "runtime.zip",
                runtime,
            ),
            (
                ImageModelComponent::DiffusionModel,
                ArtifactKind::Model,
                "diffusion.gguf",
                vec![2_u8],
            ),
            (
                ImageModelComponent::TextEncoder,
                ArtifactKind::Model,
                "encoder.gguf",
                vec![3_u8],
            ),
            (
                ImageModelComponent::Vae,
                ArtifactKind::Model,
                "vae.safetensors",
                vec![4_u8],
            ),
        ]
        .into_iter()
        .map(|(component, kind, file_name, bytes)| {
            (
                ImageArtifact {
                    component,
                    kind,
                    file_name: file_name.into(),
                    url: "http://127.0.0.1/unused".into(),
                    size: bytes.len() as u64,
                    sha256: digest(&bytes),
                },
                bytes,
            )
        })
        .collect()
    }

    async fn installed_test_service(temp: &TempDir) -> Arc<ImageModelService> {
        let root = temp.path().join(LOCAL_AI_DIR);
        let artifact_bytes = complete_test_artifacts();
        let artifacts = artifact_bytes
            .iter()
            .map(|(artifact, _)| artifact.clone())
            .collect::<Vec<_>>();
        prepare_layout(&root, &artifacts).unwrap();
        for (artifact, bytes) in &artifact_bytes {
            tokio::fs::write(artifact_path(&root, artifact), bytes)
                .await
                .unwrap();
        }
        let runtime = runtime_install_dir(&root);
        prepare_managed_directory(&root, &runtime).unwrap();
        let executable = runtime.join(if cfg!(windows) {
            "sd-cli.exe"
        } else {
            "sd-cli"
        });
        std::fs::write(&executable, b"runtime").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        write_installed_manifest(&root, &artifacts).await.unwrap();
        ImageModelService::new_inner(
            root,
            reqwest::Client::new(),
            artifacts,
            true,
            true,
        )
        .await
        .unwrap()
    }

    async fn seed_part(service: &ImageModelService, artifact: &ImageArtifact, bytes: &[u8]) {
        let part = partial_path(&service.root, artifact);
        prepare_managed_file(&service.root, &part).unwrap();
        tokio::fs::write(part, bytes).await.unwrap();
    }

    #[tokio::test]
    async fn construction_never_starts_a_download() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/artifact"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(b"tiny"))
            .mount(&server)
            .await;
        let temp = TempDir::new().unwrap();
        let artifact = model_artifact(format!("{}/artifact", server.uri()), b"tiny");
        let service = test_service(&temp, vec![artifact]).await;
        assert_eq!(
            service.status().await.models[0].install_phase,
            ImageModelInstallPhase::NotInstalled
        );
        assert!(server.received_requests().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn full_200_download_is_verified_and_committed() {
        let server = MockServer::start().await;
        let bytes = b"abcdef";
        Mock::given(method("GET"))
            .and(path("/artifact"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(bytes))
            .expect(1)
            .mount(&server)
            .await;
        let temp = TempDir::new().unwrap();
        let artifact = model_artifact(format!("{}/artifact", server.uri()), bytes);
        let service = test_service(&temp, vec![artifact.clone()]).await;
        let destination = artifact_path(&service.root, &artifact);
        service
            .download_once(
                &artifact.url,
                &artifact,
                &destination,
                1,
                &CancellationToken::new(),
            )
            .await
            .unwrap();
        assert_eq!(tokio::fs::read(destination).await.unwrap(), bytes);
        assert!(!partial_path(&service.root, &artifact).exists());
    }

    #[tokio::test]
    async fn range_206_appends_only_the_matching_suffix() {
        let server = MockServer::start().await;
        let bytes = b"abcdef";
        Mock::given(method("GET"))
            .and(path("/artifact"))
            .and(header("range", "bytes=3-"))
            .respond_with(
                ResponseTemplate::new(206)
                    .insert_header("Content-Range", "bytes 3-5/6")
                    .set_body_bytes(&bytes[3..]),
            )
            .expect(1)
            .mount(&server)
            .await;
        let temp = TempDir::new().unwrap();
        let artifact = model_artifact(format!("{}/artifact", server.uri()), bytes);
        let service = test_service(&temp, vec![artifact.clone()]).await;
        seed_part(&service, &artifact, &bytes[..3]).await;
        let destination = artifact_path(&service.root, &artifact);
        service
            .download_once(
                &artifact.url,
                &artifact,
                &destination,
                1,
                &CancellationToken::new(),
            )
            .await
            .unwrap();
        assert_eq!(tokio::fs::read(destination).await.unwrap(), bytes);
    }

    #[tokio::test]
    async fn origin_ignoring_range_restarts_instead_of_appending() {
        let server = MockServer::start().await;
        let bytes = b"uvwxyz";
        Mock::given(method("GET"))
            .and(path("/artifact"))
            .and(header("range", "bytes=3-"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(bytes))
            .expect(1)
            .mount(&server)
            .await;
        let temp = TempDir::new().unwrap();
        let artifact = model_artifact(format!("{}/artifact", server.uri()), bytes);
        let service = test_service(&temp, vec![artifact.clone()]).await;
        seed_part(&service, &artifact, b"old").await;
        let destination = artifact_path(&service.root, &artifact);
        service
            .download_once(
                &artifact.url,
                &artifact,
                &destination,
                1,
                &CancellationToken::new(),
            )
            .await
            .unwrap();
        assert_eq!(tokio::fs::read(destination).await.unwrap(), bytes);
    }

    #[tokio::test]
    async fn mismatched_content_range_is_rejected_without_destroying_partial() {
        let server = MockServer::start().await;
        let bytes = b"abcdef";
        Mock::given(method("GET"))
            .and(path("/artifact"))
            .and(header("range", "bytes=3-"))
            .respond_with(
                ResponseTemplate::new(206)
                    .insert_header("Content-Range", "bytes 2-5/6")
                    .set_body_bytes(&bytes[3..]),
            )
            .mount(&server)
            .await;
        let temp = TempDir::new().unwrap();
        let artifact = model_artifact(format!("{}/artifact", server.uri()), bytes);
        let service = test_service(&temp, vec![artifact.clone()]).await;
        seed_part(&service, &artifact, &bytes[..3]).await;
        let destination = artifact_path(&service.root, &artifact);
        let failure = service
            .download_once(
                &artifact.url,
                &artifact,
                &destination,
                1,
                &CancellationToken::new(),
            )
            .await
            .unwrap_err();
        assert_eq!(failure.kind, LocalModelErrorKind::Network);
        assert_eq!(
            tokio::fs::read(partial_path(&service.root, &artifact))
                .await
                .unwrap(),
            &bytes[..3]
        );
    }

    #[tokio::test]
    async fn pre_cancelled_transfer_preserves_the_resume_file() {
        let server = MockServer::start().await;
        let bytes = b"abcdef";
        let temp = TempDir::new().unwrap();
        let artifact = model_artifact(format!("{}/artifact", server.uri()), bytes);
        let service = test_service(&temp, vec![artifact.clone()]).await;
        seed_part(&service, &artifact, &bytes[..3]).await;
        let cancel = CancellationToken::new();
        cancel.cancel();
        let failure = service
            .download_once(
                &artifact.url,
                &artifact,
                &artifact_path(&service.root, &artifact),
                1,
                &cancel,
            )
            .await
            .unwrap_err();
        assert!(failure.cancelled);
        assert_eq!(
            tokio::fs::read(partial_path(&service.root, &artifact))
                .await
                .unwrap(),
            &bytes[..3]
        );
        assert!(server.received_requests().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn checksum_failure_removes_poisoned_partial() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/artifact"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(b"badbad"))
            .mount(&server)
            .await;
        let temp = TempDir::new().unwrap();
        let artifact = model_artifact(format!("{}/artifact", server.uri()), b"good!!");
        let service = test_service(&temp, vec![artifact.clone()]).await;
        let failure = service
            .download_once(
                &artifact.url,
                &artifact,
                &artifact_path(&service.root, &artifact),
                1,
                &CancellationToken::new(),
            )
            .await
            .unwrap_err();
        assert_eq!(failure.kind, LocalModelErrorKind::ChecksumMismatch);
        assert!(!partial_path(&service.root, &artifact).exists());
        assert!(!artifact_path(&service.root, &artifact).exists());
    }

    fn write_zip(path: &Path, entries: &[(&str, &[u8])]) {
        let file = File::create(path).unwrap();
        let mut writer = zip::ZipWriter::new(file);
        let options = SimpleFileOptions::default().unix_permissions(0o755);
        for (name, bytes) in entries {
            writer.start_file(*name, options).unwrap();
            writer.write_all(bytes).unwrap();
        }
        writer.finish().unwrap();
    }

    #[test]
    fn runtime_zip_extracts_regular_files_and_rejects_path_escape() {
        let temp = TempDir::new().unwrap();
        let safe_zip = temp.path().join("safe.zip");
        let executable = if cfg!(windows) {
            "bundle/sd-cli.exe"
        } else {
            "bundle/sd-cli"
        };
        write_zip(&safe_zip, &[(executable, b"runtime")]);
        let safe_output = temp.path().join("safe-output");
        std::fs::create_dir(&safe_output).unwrap();
        extract_runtime_zip(&safe_zip, &safe_output).unwrap();
        assert!(find_runtime_executable(&safe_output).is_ok());

        let unsafe_zip = temp.path().join("unsafe.zip");
        write_zip(&unsafe_zip, &[("../escaped", b"no")]);
        let unsafe_output = temp.path().join("unsafe-output");
        std::fs::create_dir(&unsafe_output).unwrap();
        assert!(extract_runtime_zip(&unsafe_zip, &unsafe_output).is_err());
        assert!(!temp.path().join("escaped").exists());
    }

    #[test]
    fn pinned_unix_archive_mode_is_normalized_for_sd_cli_only() {
        assert_eq!(runtime_entry_mode(Some(0o664), OsStr::new("sd-cli")), 0o755);
        assert_eq!(runtime_entry_mode(Some(0o664), OsStr::new("libggml.so")), 0o664);
    }

    #[tokio::test]
    async fn config_is_refused_before_all_components_are_installed() {
        let temp = TempDir::new().unwrap();
        let artifact = model_artifact("http://127.0.0.1/unused".into(), b"tiny");
        let service = test_service(&temp, vec![artifact]).await;
        assert!(matches!(
            service.sd_cli_config().await,
            Err(AppError::Conflict(_))
        ));
    }

    #[tokio::test]
    async fn installed_bundle_resolves_config_and_delete_removes_every_artifact() {
        let temp = TempDir::new().unwrap();
        let service = installed_test_service(&temp).await;
        let status = service.status().await;
        assert!(status.artifacts_ready);
        assert!(!status.inference_ready);
        let config = service.sd_cli_config().await.unwrap();
        assert!(service.status().await.inference_ready);
        assert!(runtime_executable_name(
            config.executable.file_name().unwrap()
        ));
        assert_eq!(config.diffusion_model.file_name().unwrap(), "diffusion.gguf");
        assert_eq!(config.text_encoder.file_name().unwrap(), "encoder.gguf");
        assert_eq!(config.vae.file_name().unwrap(), "vae.safetensors");

        let old_runtime = runtime_install_dir(&service.root);
        let old_model = model_dir(&service.root);
        let deleted = service.delete(Z_IMAGE_TURBO_MODEL_ID).await.unwrap();
        assert!(!deleted.artifacts_ready);
        assert_eq!(
            deleted.models[0].install_phase,
            ImageModelInstallPhase::NotInstalled
        );
        assert!(!old_runtime.exists());
        assert!(old_model.exists());
        assert!(!state_path(&service.root).exists());
    }

    #[tokio::test]
    async fn equal_sized_model_tampering_is_rejected_before_runtime_use() {
        let temp = TempDir::new().unwrap();
        let service = installed_test_service(&temp).await;
        let artifact = artifact_for(
            &service.artifacts,
            ImageModelComponent::DiffusionModel,
        )
        .unwrap();
        tokio::fs::write(model_artifact_path(&service.root, artifact), [99_u8])
            .await
            .unwrap();

        assert!(matches!(
            service.sd_cli_config().await,
            Err(AppError::Conflict(_))
        ));
        let status = service.status().await;
        assert!(!status.artifacts_ready);
        assert!(!status.inference_ready);
        assert_eq!(
            status.models[0].error_kind,
            Some(LocalModelErrorKind::ChecksumMismatch)
        );
    }

    #[tokio::test]
    async fn deletion_is_blocked_while_shared_workload_permit_is_held() {
        let temp = TempDir::new().unwrap();
        let service = installed_test_service(&temp).await;
        let gate = Arc::new(Semaphore::new(1));
        let _backend = service.creation_backend(gate.clone());
        let permit = gate.clone().acquire_owned().await.unwrap();

        assert!(matches!(
            service.delete(Z_IMAGE_TURBO_MODEL_ID).await,
            Err(AppError::Conflict(_))
        ));
        drop(permit);
        assert!(service.delete(Z_IMAGE_TURBO_MODEL_ID).await.is_ok());
    }

    #[test]
    fn fixed_recipe_has_four_unique_components() {
        let (artifacts, supported) = production_artifacts();
        if supported {
            assert_eq!(artifacts.len(), 4);
            for component in component_order() {
                assert_eq!(
                    artifacts
                        .iter()
                        .filter(|artifact| artifact.component == component)
                        .count(),
                    1
                );
            }
        }
        assert_eq!(Z_IMAGE_TURBO_ARTIFACTS.len(), 3);
    }

    #[test]
    fn download_url_allowlist_does_not_accept_host_suffix_tricks() {
        assert!(allowed_download_url(
            &reqwest::Url::parse("https://huggingface.co/a/b").unwrap()
        ));
        assert!(allowed_download_url(
            &reqwest::Url::parse("https://github.com/a/b").unwrap()
        ));
        assert!(!allowed_download_url(
            &reqwest::Url::parse("https://github.com.evil.test/a").unwrap()
        ));
        assert!(!allowed_download_url(
            &reqwest::Url::parse("http://huggingface.co/a").unwrap()
        ));
    }
}

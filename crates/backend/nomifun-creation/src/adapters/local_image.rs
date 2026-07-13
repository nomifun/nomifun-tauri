//! Application-managed local image generation.
//!
//! This module intentionally owns no downloader or model installation state.
//! The app/local-model control plane verifies and provisions the pinned runtime
//! and three Z-Image artifacts, then injects a [`LocalImageBackend`] into the
//! creation engine. The adapter keeps the existing creation queue, asset input,
//! cancellation, and result-persistence paths unchanged while avoiding an
//! unnecessary loopback HTTP hop.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use serde_json::Value;
use tokio::io::AsyncReadExt;
use tokio::process::Child;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

use crate::provider::{
    InputAsset, MediaProvider, PollResult, ProducedAsset, ProducedData, SubmitAck, SubmitRequest,
};
use crate::types::{CreationError, MediaCapability};

/// Stable model id that the managed provider projection and catalog must use.
pub const LOCAL_Z_IMAGE_TURBO_MODEL_ID: &str = "z-image-turbo-q3-k";

/// Adapter id selected by [`super::route_adapter_id`].
pub const LOCAL_IMAGE_ADAPTER_ID: &str = "local_image";

/// Fixed Turbo settings recommended by stable-diffusion.cpp's Z-Image guide.
pub const Z_IMAGE_TURBO_STEPS: u32 = 8;
pub const Z_IMAGE_TURBO_CFG_SCALE: f32 = 1.0;

const DEFAULT_WIDTH: u32 = 1024;
const DEFAULT_HEIGHT: u32 = 1024;
const MIN_DIMENSION: u32 = 256;
const MAX_DIMENSION: u32 = 2048;
// Existing workshop presets include 1280x720; the VAE requires latent-safe
// dimensions divisible by 8, not an unnecessarily strict multiple of 64.
const DIMENSION_MULTIPLE: u32 = 8;
const MAX_PROMPT_CHARS: usize = 8_000;
const DEFAULT_RUN_TIMEOUT: Duration = Duration::from_secs(9 * 60);
const MAX_OUTPUT_BYTES: u64 = 256 * 1024 * 1024;
const MAX_STDERR_CHARS: usize = 1_000;

/// Role of one file in the fixed Z-Image recipe.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ZImageArtifactRole {
    DiffusionModel,
    TextEncoder,
    Vae,
}

/// Immutable artifact metadata consumed by the future provisioning layer.
///
/// `license = None` is deliberate for the VAE: the pinned repository card does
/// not declare a license. Its provenance note must be surfaced in NOTICE/UI
/// rather than silently claiming the upstream FLUX.1-schnell license applies.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ZImageArtifactSpec {
    pub role: ZImageArtifactRole,
    pub file_name: &'static str,
    pub url: &'static str,
    pub size: u64,
    pub sha256: &'static str,
    pub license: Option<&'static str>,
    pub notice: Option<&'static str>,
}

pub const Z_IMAGE_TURBO_ARTIFACTS: [ZImageArtifactSpec; 3] = [
    ZImageArtifactSpec {
        role: ZImageArtifactRole::DiffusionModel,
        file_name: "z_image_turbo-Q3_K.gguf",
        url: "https://huggingface.co/leejet/Z-Image-Turbo-GGUF/resolve/c61c0e422dc8b541b7548cf33a4ef8302b0f8085/z_image_turbo-Q3_K.gguf",
        size: 3_143_559_104,
        sha256: "4b44bdaa7814f20d7cf144e3939bd93aa32f50660204dd0c2aea5c5376232980",
        license: Some("Apache-2.0"),
        notice: None,
    },
    ZImageArtifactSpec {
        role: ZImageArtifactRole::TextEncoder,
        file_name: "Qwen3-4B-Instruct-2507-Q4_K_M.gguf",
        url: "https://huggingface.co/unsloth/Qwen3-4B-Instruct-2507-GGUF/resolve/a06e946bb6b655725eafa393f4a9745d460374c9/Qwen3-4B-Instruct-2507-Q4_K_M.gguf",
        size: 2_497_281_120,
        sha256: "3605803b982cb64aead44f6c1b2ae36e3acdb41d8e46c8a94c6533bc4c67e597",
        license: Some("Apache-2.0"),
        notice: None,
    },
    ZImageArtifactSpec {
        role: ZImageArtifactRole::Vae,
        file_name: "ae.safetensors",
        url: "https://huggingface.co/Comfy-Org/z_image_turbo/resolve/d24c4cf2a0cd98a42f23467e27e3d76ee9438b8e/split_files/vae/ae.safetensors",
        size: 335_304_388,
        sha256: "afc8e28272cd15db3919bacdb6918ce9c1ed22e96cb12c4d5ed0fba823529e38",
        license: None,
        notice: Some(
            "The pinned Comfy-Org/z_image_turbo repository card has no license field; the VAE is documented as originating from FLUX.1-schnell. Review and preserve upstream notices before distribution.",
        ),
    },
];

pub const Z_IMAGE_TURBO_DOWNLOAD_SIZE: u64 = 5_976_144_612;

/// A pinned stable-diffusion.cpp runtime archive.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SdCppRuntimeArtifactSpec {
    pub os: &'static str,
    pub arch: &'static str,
    pub backend: &'static str,
    pub version: &'static str,
    pub archive_name: &'static str,
    pub url: &'static str,
    pub size: u64,
    pub sha256: &'static str,
    pub license: &'static str,
}

pub const SD_CPP_RUNTIME_VERSION: &str = "master-775-b5d8120";

pub const SD_CPP_RUNTIME_ARTIFACTS: [SdCppRuntimeArtifactSpec; 3] = [
    SdCppRuntimeArtifactSpec {
        os: "windows",
        arch: "x86_64",
        backend: "vulkan",
        version: SD_CPP_RUNTIME_VERSION,
        archive_name: "sd-master-b5d8120-bin-win-vulkan-x64.zip",
        url: "https://github.com/leejet/stable-diffusion.cpp/releases/download/master-775-b5d8120/sd-master-b5d8120-bin-win-vulkan-x64.zip",
        size: 37_680_378,
        sha256: "679e23655dc27700c016f0f256810902d3c0edae3f25e477b42bbb650d13497a",
        license: "MIT",
    },
    SdCppRuntimeArtifactSpec {
        os: "macos",
        arch: "aarch64",
        backend: "metal",
        version: SD_CPP_RUNTIME_VERSION,
        archive_name: "sd-master-b5d8120-bin-Darwin-macOS-26.4-arm64.zip",
        url: "https://github.com/leejet/stable-diffusion.cpp/releases/download/master-775-b5d8120/sd-master-b5d8120-bin-Darwin-macOS-26.4-arm64.zip",
        size: 49_227_335,
        sha256: "1b9a851bdaf787b63dca2f442e43685b42a5b76b6849ffeabeeb3b819541fb8f",
        license: "MIT",
    },
    SdCppRuntimeArtifactSpec {
        os: "linux",
        arch: "x86_64",
        backend: "vulkan",
        version: SD_CPP_RUNTIME_VERSION,
        archive_name: "sd-master-b5d8120-bin-Linux-Ubuntu-24.04-x86_64-vulkan.zip",
        url: "https://github.com/leejet/stable-diffusion.cpp/releases/download/master-775-b5d8120/sd-master-b5d8120-bin-Linux-Ubuntu-24.04-x86_64-vulkan.zip",
        size: 44_791_302,
        sha256: "968db72d1aa92cb4388c7e0ca5d2ec740bfd8d0ca7d21965a6ad70cf1c472501",
        license: "MIT",
    },
];

/// Pick the pinned runtime artifact for the current target.
pub fn current_sd_cpp_runtime_artifact() -> Option<&'static SdCppRuntimeArtifactSpec> {
    SD_CPP_RUNTIME_ARTIFACTS.iter().find(|artifact| {
        artifact.os == std::env::consts::OS && artifact.arch == std::env::consts::ARCH
    })
}

/// Sanitized request passed across the app-injected local-image seam.
///
/// Provider credentials and loopback URLs are intentionally absent: a local
/// backend should not need either.
pub struct LocalImageRequest {
    pub model: String,
    pub capability: MediaCapability,
    pub params: Value,
    pub inputs: Vec<InputAsset>,
}

/// In-process seam implemented by a local runtime supervisor or the included
/// one-shot [`SdCliZImageBackend`].
#[async_trait]
pub trait LocalImageBackend: Send + Sync {
    async fn generate(
        &self,
        request: LocalImageRequest,
    ) -> Result<Vec<ProducedAsset>, CreationError>;
}

/// Creation-engine adapter that delegates to an injected local backend.
pub struct LocalImageAdapter {
    backend: Arc<dyn LocalImageBackend>,
}

impl LocalImageAdapter {
    pub fn new(backend: Arc<dyn LocalImageBackend>) -> Self {
        Self { backend }
    }
}

#[async_trait]
impl MediaProvider for LocalImageAdapter {
    fn id(&self) -> &'static str {
        LOCAL_IMAGE_ADAPTER_ID
    }

    fn supports(&self, cap: MediaCapability) -> bool {
        // The pinned Z-Image-Turbo recipe is text-to-image. Keep i2i/inpaint
        // out of capability discovery until the runtime path is implemented and
        // verified rather than silently ignoring reference images.
        cap == MediaCapability::T2i
    }

    async fn submit(&self, request: &SubmitRequest) -> Result<SubmitAck, CreationError> {
        if request.model != LOCAL_Z_IMAGE_TURBO_MODEL_ID {
            return Err(CreationError::new(
                "unsupported_model",
                "the local image backend does not recognize this model",
            ));
        }
        if !self.supports(request.capability) {
            return Err(CreationError::new(
                "unsupported_capability",
                format!(
                    "the local Z-Image backend cannot serve {}",
                    request.capability.as_str()
                ),
            ));
        }

        let produced = self
            .backend
            .generate(LocalImageRequest {
                model: request.model.clone(),
                capability: request.capability,
                params: request.params.clone(),
                inputs: request.inputs.clone(),
            })
            .await?;
        if produced.is_empty() {
            return Err(CreationError::provider_error(
                "the local image backend produced no image",
            ));
        }
        Ok(SubmitAck::Done(produced))
    }

    async fn poll(
        &self,
        _remote_task_id: &str,
        _request: &SubmitRequest,
    ) -> Result<PollResult, CreationError> {
        Err(CreationError::config(
            "local_image is synchronous and has no poll step",
        ))
    }
}

/// Fully resolved paths for the one-shot stable-diffusion.cpp runner.
///
/// The provisioning layer is responsible for verifying every file against the
/// pinned metadata above before constructing this value.
#[derive(Debug, Clone)]
pub struct SdCliZImageConfig {
    pub executable: PathBuf,
    pub diffusion_model: PathBuf,
    pub text_encoder: PathBuf,
    pub vae: PathBuf,
    pub job_root: PathBuf,
    pub timeout: Duration,
}

impl SdCliZImageConfig {
    pub fn new(
        executable: PathBuf,
        diffusion_model: PathBuf,
        text_encoder: PathBuf,
        vae: PathBuf,
        job_root: PathBuf,
    ) -> Self {
        Self {
            executable,
            diffusion_model,
            text_encoder,
            vae,
            job_root,
            timeout: DEFAULT_RUN_TIMEOUT,
        }
    }

    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }
}

/// Lightweight concrete backend that starts `sd-cli` for one generation and
/// exits. It is deliberately serialized: multiple Z-Image processes would
/// contend for the same consumer GPU/RAM budget.
pub struct SdCliZImageBackend {
    config: SdCliZImageConfig,
    gate: Arc<Semaphore>,
}

impl SdCliZImageBackend {
    pub fn new(config: SdCliZImageConfig) -> Self {
        Self {
            config,
            gate: Arc::new(Semaphore::new(1)),
        }
    }

    /// Use the app-wide heavyweight local-workload gate. This prevents a chat
    /// model and Z-Image from competing for consumer GPU/RAM at the same time.
    pub fn with_gate(config: SdCliZImageConfig, gate: Arc<Semaphore>) -> Self {
        Self { config, gate }
    }

    /// Run with an app-wide permit already acquired. The managed installer
    /// uses this to keep path verification, generation, and model deletion in
    /// one race-free critical section.
    pub async fn generate_with_permit(
        &self,
        request: LocalImageRequest,
        permit: OwnedSemaphorePermit,
    ) -> Result<Vec<ProducedAsset>, CreationError> {
        if request.model != LOCAL_Z_IMAGE_TURBO_MODEL_ID
            || request.capability != MediaCapability::T2i
        {
            return Err(CreationError::new(
                "unsupported_model",
                "this runner only serves the pinned Z-Image-Turbo recipe",
            ));
        }
        if !request.inputs.is_empty() {
            return Err(CreationError::new(
                "bad_input",
                "the pinned Z-Image-Turbo runner does not accept image inputs",
            ));
        }
        let params = Self::resolved_params(&request.params)?;
        let bytes = self.run_once(&params, permit).await?;
        Ok(vec![ProducedAsset {
            data: ProducedData::Bytes(bytes),
            mime: Some("image/png".into()),
        }])
    }

    fn resolved_params(params: &Value) -> Result<ResolvedZImageParams, CreationError> {
        let prompt = params
            .get("prompt")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|prompt| !prompt.is_empty())
            .ok_or_else(|| CreationError::new("bad_input", "a non-empty prompt is required"))?
            .to_owned();
        if prompt.contains('\0') || prompt.chars().count() > MAX_PROMPT_CHARS {
            return Err(CreationError::new(
                "bad_input",
                format!("prompt must contain at most {MAX_PROMPT_CHARS} characters"),
            ));
        }
        let width = dimension(params, "width", DEFAULT_WIDTH)?;
        let height = dimension(params, "height", DEFAULT_HEIGHT)?;
        let count = optional_u64(params, "count")?.unwrap_or(1);
        if count != 1 {
            return Err(CreationError::new(
                "bad_input",
                "local Z-Image currently generates one image per task; use a workshop loop for batches",
            ));
        }
        let seed = match params.get("seed") {
            None | Some(Value::Null) => None,
            Some(value) => Some(
                value
                    .as_i64()
                    .ok_or_else(|| CreationError::new("bad_input", "seed must be an integer"))?,
            ),
        };
        Ok(ResolvedZImageParams {
            prompt,
            width,
            height,
            seed,
        })
    }

    async fn run_once(
        &self,
        params: &ResolvedZImageParams,
        permit: OwnedSemaphorePermit,
    ) -> Result<Vec<u8>, CreationError> {
        validate_runtime_file(&self.config.executable, "sd-cli executable").await?;
        validate_runtime_file(&self.config.diffusion_model, "diffusion model").await?;
        validate_runtime_file(&self.config.text_encoder, "text encoder").await?;
        validate_runtime_file(&self.config.vae, "VAE").await?;

        if !self.config.job_root.is_absolute() {
            return Err(CreationError::config(
                "the local image job directory must be an absolute path",
            ));
        }
        tokio::fs::create_dir_all(&self.config.job_root)
            .await
            .map_err(|_| CreationError::config("could not create the local image job directory"))?;
        let job_root = tokio::fs::canonicalize(&self.config.job_root)
            .await
            .map_err(|_| CreationError::config("could not resolve the local image job directory"))?;
        let job_dir = create_job_dir(&job_root).await?;
        let output_path = job_dir.join("output.png");
        let args = build_sd_cli_args(&self.config, params, &output_path);

        let mut command = nomifun_runtime::Builder::clean_cli(&self.config.executable);
        command
            .args(args)
            .current_dir(&job_dir)
            .stdout(Stdio::null())
            .stderr(Stdio::piped());
        configure_runtime_library_path(&mut command, &self.config.executable);
        let child = match command.spawn() {
            Ok(child) => child,
            Err(error) => {
                let _ = tokio::fs::remove_dir_all(&job_dir).await;
                tracing::warn!(error = %error, "Could not start local Z-Image runtime");
                return Err(CreationError::config(
                    "the local image runtime could not be started",
                ));
            }
        };
        let mut child = ChildTreeGuard::new(child, job_dir.clone(), permit);
        let mut stderr_task = child
            .child_mut()
            .stderr
            .take()
            .map(|stderr| tokio::spawn(read_stderr_excerpt(stderr)));

        let status = match tokio::time::timeout(self.config.timeout, child.child_mut().wait()).await {
            Ok(Ok(status)) => {
                // The process has exited; disarming avoids scheduling a redundant
                // tree kill when the guard leaves scope.
                child.disarm();
                status
            }
            Ok(Err(error)) => {
                child.terminate_and_cleanup().await;
                if let Some(task) = stderr_task.take() {
                    let _ = task.await;
                }
                tracing::warn!(error = %error, "Could not wait for local Z-Image runtime");
                return Err(CreationError::provider_error(
                    "the local image runtime stopped unexpectedly",
                ));
            }
            Err(_) => {
                // Explicit process-tree cleanup is required here: Child's
                // kill_on_drop only promises to kill the direct child.
                child.terminate_and_cleanup().await;
                if let Some(task) = stderr_task.take() {
                    let _ = task.await;
                }
                return Err(CreationError::timeout(
                    "local Z-Image generation exceeded its time limit",
                ));
            }
        };

        let stderr = match stderr_task.take() {
            Some(task) => task.await.unwrap_or_default(),
            None => String::new(),
        };
        if !status.success() {
            tracing::warn!(status = %status, stderr = %stderr, "Local Z-Image runtime failed");
            child.cleanup_job().await;
            return Err(CreationError::provider_error(
                "the local image runtime failed to generate an image",
            ));
        }

        let bytes = read_generated_png(&output_path).await;
        child.cleanup_job().await;
        bytes
    }
}

fn configure_runtime_library_path(command: &mut nomifun_runtime::Builder, executable: &Path) {
    let Some(directory) = executable.parent() else {
        return;
    };
    // The pinned Unix archives ship sd-cli beside private shared libraries.
    // Generation runs in an isolated job directory, so make that verified
    // runtime directory explicit instead of relying on the caller's shell.
    #[cfg(target_os = "linux")]
    command.env("LD_LIBRARY_PATH", directory);
    #[cfg(target_os = "macos")]
    command.env("DYLD_LIBRARY_PATH", directory);
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    let _ = (command, directory);
}

/// Cancellation-safe process owner. CreationService cancels work by dropping
/// the adapter future, so Drop must arrange a process-tree kill as well as the
/// explicit timeout path above.
struct ChildTreeGuard {
    child: Option<Child>,
    job_dir: Option<PathBuf>,
    permit: Option<OwnedSemaphorePermit>,
}

impl ChildTreeGuard {
    fn new(child: Child, job_dir: PathBuf, permit: OwnedSemaphorePermit) -> Self {
        Self {
            child: Some(child),
            job_dir: Some(job_dir),
            permit: Some(permit),
        }
    }

    fn child_mut(&mut self) -> &mut Child {
        self.child.as_mut().expect("child guard must be armed")
    }

    fn disarm(&mut self) {
        self.child.take();
    }

    async fn kill_tree(&mut self) {
        if let Some(mut child) = self.child.take()
            && let Err(error) = nomifun_runtime::kill_process_tree(&mut child).await
        {
            tracing::warn!(error = %error, "Could not terminate local Z-Image process tree");
        }
    }

    async fn cleanup_job(&mut self) {
        if let Some(job_dir) = self.job_dir.take()
            && let Err(error) = tokio::fs::remove_dir_all(&job_dir).await
        {
            tracing::warn!(error = %error, "Could not remove local Z-Image job directory");
        }
        self.permit.take();
    }

    async fn terminate_and_cleanup(&mut self) {
        self.kill_tree().await;
        self.cleanup_job().await;
    }
}

impl Drop for ChildTreeGuard {
    fn drop(&mut self) {
        let child = self.child.take();
        let job_dir = self.job_dir.take();
        let permit = self.permit.take();
        if child.is_none() && job_dir.is_none() && permit.is_none() {
            return;
        }
        // A dropped adapter future is the normal cancellation path. Schedule
        // full tree cleanup followed by job-directory removal while the Tokio runtime is alive; the runtime
        // Builder's kill_on_drop and parent-death protections remain fallback
        // safety nets during process shutdown.
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            handle.spawn(async move {
                let _permit = permit;
                if let Some(mut child) = child
                    && let Err(error) = nomifun_runtime::kill_process_tree(&mut child).await
                {
                    tracing::warn!(error = %error, "Could not terminate cancelled Z-Image process tree");
                }
                if let Some(job_dir) = job_dir
                    && let Err(error) = tokio::fs::remove_dir_all(&job_dir).await
                {
                    tracing::warn!(error = %error, "Could not remove cancelled Z-Image job directory");
                }
            });
        } else {
            if let Some(mut child) = child {
                let _ = child.start_kill();
            }
            if let Some(job_dir) = job_dir {
                let _ = std::fs::remove_dir_all(job_dir);
            }
            drop(permit);
        }
    }
}

async fn read_stderr_excerpt(mut stderr: tokio::process::ChildStderr) -> String {
    let mut excerpt = Vec::with_capacity(MAX_STDERR_CHARS);
    let mut buffer = [0_u8; 8 * 1024];
    loop {
        match stderr.read(&mut buffer).await {
            Ok(0) | Err(_) => break,
            Ok(read) if excerpt.len() < MAX_STDERR_CHARS => {
                let remaining = MAX_STDERR_CHARS - excerpt.len();
                excerpt.extend_from_slice(&buffer[..read.min(remaining)]);
            }
            Ok(_) => {}
        }
    }
    String::from_utf8_lossy(&excerpt).into_owned()
}

#[async_trait]
impl LocalImageBackend for SdCliZImageBackend {
    async fn generate(
        &self,
        request: LocalImageRequest,
    ) -> Result<Vec<ProducedAsset>, CreationError> {
        let permit = self
            .gate
            .clone()
            .acquire_owned()
            .await
            .map_err(|_| CreationError::config("the local image runtime is shutting down"))?;
        self.generate_with_permit(request, permit).await
    }
}

#[derive(Debug, Clone, PartialEq)]
struct ResolvedZImageParams {
    prompt: String,
    width: u32,
    height: u32,
    seed: Option<i64>,
}

fn dimension(params: &Value, key: &str, default: u32) -> Result<u32, CreationError> {
    let raw = optional_u64(params, key)?.unwrap_or(u64::from(default));
    let value = u32::try_from(raw)
        .map_err(|_| CreationError::new("bad_input", format!("{key} is out of range")))?;
    if !(MIN_DIMENSION..=MAX_DIMENSION).contains(&value)
        || value % DIMENSION_MULTIPLE != 0
    {
        return Err(CreationError::new(
            "bad_input",
            format!(
                "{key} must be between {MIN_DIMENSION} and {MAX_DIMENSION} and divisible by {DIMENSION_MULTIPLE}"
            ),
        ));
    }
    Ok(value)
}

fn optional_u64(params: &Value, key: &str) -> Result<Option<u64>, CreationError> {
    match params.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(value) => value
            .as_u64()
            .map(Some)
            .ok_or_else(|| CreationError::new("bad_input", format!("{key} must be an integer"))),
    }
}

fn build_sd_cli_args(
    config: &SdCliZImageConfig,
    params: &ResolvedZImageParams,
    output: &Path,
) -> Vec<std::ffi::OsString> {
    let mut args = vec![
        "--diffusion-model".into(),
        config.diffusion_model.as_os_str().to_owned(),
        "--vae".into(),
        config.vae.as_os_str().to_owned(),
        "--llm".into(),
        config.text_encoder.as_os_str().to_owned(),
        "-p".into(),
        params.prompt.clone().into(),
        "--cfg-scale".into(),
        format!("{Z_IMAGE_TURBO_CFG_SCALE:.1}").into(),
        "--steps".into(),
        Z_IMAGE_TURBO_STEPS.to_string().into(),
        "--offload-to-cpu".into(),
        "--diffusion-fa".into(),
        "-W".into(),
        params.width.to_string().into(),
        "-H".into(),
        params.height.to_string().into(),
        "-o".into(),
        output.as_os_str().to_owned(),
    ];
    if let Some(seed) = params.seed {
        args.push("--seed".into());
        args.push(seed.to_string().into());
    }
    args
}

async fn validate_runtime_file(path: &Path, label: &str) -> Result<(), CreationError> {
    if !path.is_absolute() {
        return Err(CreationError::config(format!(
            "the provisioned {label} path must be absolute"
        )));
    }
    match tokio::fs::metadata(path).await {
        Ok(metadata) if metadata.is_file() => Ok(()),
        _ => Err(CreationError::config(format!(
            "the provisioned {label} is missing"
        ))),
    }
}

async fn create_job_dir(root: &Path) -> Result<PathBuf, CreationError> {
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};

    static NEXT_JOB: AtomicU64 = AtomicU64::new(0);
    let epoch = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    for _ in 0..10 {
        let sequence = NEXT_JOB.fetch_add(1, Ordering::Relaxed);
        let path = root.join(format!("zimage-{epoch}-{sequence}"));
        match tokio::fs::create_dir(&path).await {
            Ok(()) => return Ok(path),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(_) => {
                return Err(CreationError::config(
                    "could not create an isolated local image job directory",
                ));
            }
        }
    }
    Err(CreationError::config(
        "could not allocate an isolated local image job directory",
    ))
}

async fn read_generated_png(path: &Path) -> Result<Vec<u8>, CreationError> {
    let metadata = tokio::fs::metadata(path)
        .await
        .map_err(|_| CreationError::provider_error("the local image runtime produced no output file"))?;
    if !metadata.is_file() || metadata.len() == 0 || metadata.len() > MAX_OUTPUT_BYTES {
        return Err(CreationError::provider_error(
            "the local image runtime produced an invalid output file",
        ));
    }
    let bytes = tokio::fs::read(path)
        .await
        .map_err(|_| CreationError::provider_error("the generated image could not be read"))?;
    if !bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        return Err(CreationError::provider_error(
            "the local image runtime output is not a PNG image",
        ));
    }
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn artifact_recipe_is_pinned_and_complete() {
        assert_eq!(
            Z_IMAGE_TURBO_ARTIFACTS.iter().map(|artifact| artifact.size).sum::<u64>(),
            Z_IMAGE_TURBO_DOWNLOAD_SIZE
        );
        assert_eq!(Z_IMAGE_TURBO_ARTIFACTS.len(), 3);
        for artifact in Z_IMAGE_TURBO_ARTIFACTS {
            assert!(artifact.url.starts_with("https://huggingface.co/"));
            assert_eq!(artifact.sha256.len(), 64);
        }
        assert!(Z_IMAGE_TURBO_ARTIFACTS[2].license.is_none());
        assert!(Z_IMAGE_TURBO_ARTIFACTS[2].notice.is_some());
    }

    #[test]
    fn runtime_recipe_is_pinned_to_one_release() {
        assert_eq!(SD_CPP_RUNTIME_ARTIFACTS.len(), 3);
        for artifact in SD_CPP_RUNTIME_ARTIFACTS {
            assert_eq!(artifact.version, SD_CPP_RUNTIME_VERSION);
            assert!(artifact.url.contains("/master-775-b5d8120/"));
            assert_eq!(artifact.sha256.len(), 64);
            assert_eq!(artifact.license, "MIT");
        }
    }

    #[test]
    fn turbo_params_are_fixed_and_dimensions_validated() {
        let params = SdCliZImageBackend::resolved_params(&json!({
            "prompt": "a red panda",
            "width": 512,
            "height": 1024,
            "seed": 42
        }))
        .unwrap();
        assert_eq!(params.width, 512);
        assert_eq!(params.height, 1024);
        assert_eq!(params.seed, Some(42));
        assert!(SdCliZImageBackend::resolved_params(&json!({
            "prompt": "x",
            "width": "1024"
        }))
        .is_err());
        assert!(SdCliZImageBackend::resolved_params(&json!({
            "prompt": "x",
            "width": 515
        }))
        .is_err());
        // Matches an existing workshop wide-screen preset.
        assert!(SdCliZImageBackend::resolved_params(&json!({
            "prompt": "x",
            "width": 1280,
            "height": 720
        }))
        .is_ok());
        assert!(SdCliZImageBackend::resolved_params(&json!({
            "prompt": "x",
            "count": 2
        }))
        .is_err());
        assert!(SdCliZImageBackend::resolved_params(&json!({
            "prompt": "x",
            "seed": "random"
        }))
        .is_err());
        assert!(SdCliZImageBackend::resolved_params(&json!({
            "prompt": "x".repeat(MAX_PROMPT_CHARS + 1)
        }))
        .is_err());
    }

    #[test]
    fn local_adapter_advertises_only_text_to_image() {
        let backend = Arc::new(SdCliZImageBackend::new(SdCliZImageConfig::new(
            "sd-cli".into(),
            "diffusion.gguf".into(),
            "qwen.gguf".into(),
            "ae.safetensors".into(),
            "jobs".into(),
        )));
        let adapter = LocalImageAdapter::new(backend);
        assert!(adapter.supports(MediaCapability::T2i));
        assert!(!adapter.supports(MediaCapability::I2i));
        assert!(!adapter.supports(MediaCapability::Inpaint));
    }

    #[tokio::test]
    async fn job_outputs_are_isolated_in_unique_directories() {
        let root = tempfile::tempdir().unwrap();
        let first = create_job_dir(root.path()).await.unwrap();
        let second = create_job_dir(root.path()).await.unwrap();
        assert_ne!(first, second);
        assert_eq!(first.parent(), Some(root.path()));
        assert_eq!(second.parent(), Some(root.path()));
        assert_eq!(first.join("output.png").parent(), Some(first.as_path()));
        tokio::fs::remove_dir_all(first).await.unwrap();
        tokio::fs::remove_dir_all(second).await.unwrap();
    }

    #[cfg(any(unix, windows))]
    #[tokio::test]
    async fn process_guard_terminates_child_tree_before_job_cleanup() {
        let root = tempfile::tempdir().unwrap();
        let job_dir = create_job_dir(root.path()).await.unwrap();

        #[cfg(windows)]
        let mut command = {
            let mut command = nomifun_runtime::Builder::clean_cli("cmd.exe");
            command.args(["/C", "ping -n 30 127.0.0.1 >NUL"]);
            command
        };
        #[cfg(unix)]
        let mut command = {
            let mut command = nomifun_runtime::Builder::clean_cli("sh");
            command.args(["-c", "sleep 30"]);
            command
        };
        command.stdout(Stdio::null()).stderr(Stdio::null());
        let child = command.spawn().unwrap();
        let gate = Arc::new(Semaphore::new(1));
        let permit = gate.clone().acquire_owned().await.unwrap();
        let mut guard = ChildTreeGuard::new(child, job_dir.clone(), permit);
        guard.terminate_and_cleanup().await;
        assert!(!job_dir.exists());
        assert_eq!(gate.available_permits(), 1);
    }

    #[cfg(any(unix, windows))]
    #[tokio::test]
    async fn cancelled_guard_releases_permit_only_after_async_tree_cleanup() {
        let root = tempfile::tempdir().unwrap();
        let job_dir = create_job_dir(root.path()).await.unwrap();
        #[cfg(windows)]
        let mut command = {
            let mut command = nomifun_runtime::Builder::clean_cli("cmd.exe");
            command.args(["/C", "ping -n 30 127.0.0.1 >NUL"]);
            command
        };
        #[cfg(unix)]
        let mut command = {
            let mut command = nomifun_runtime::Builder::clean_cli("sh");
            command.args(["-c", "sleep 30"]);
            command
        };
        command.stdout(Stdio::null()).stderr(Stdio::null());
        let gate = Arc::new(Semaphore::new(1));
        let permit = gate.clone().acquire_owned().await.unwrap();
        let guard = ChildTreeGuard::new(command.spawn().unwrap(), job_dir.clone(), permit);

        drop(guard);
        assert_eq!(gate.available_permits(), 0);
        tokio::time::timeout(Duration::from_secs(5), async {
            while gate.available_permits() == 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("cleanup should terminate the child and release the permit");
        assert!(!job_dir.exists());
    }

    #[test]
    fn command_contains_the_verified_z_image_recipe() {
        let config = SdCliZImageConfig::new(
            "sd-cli".into(),
            "diffusion.gguf".into(),
            "qwen.gguf".into(),
            "ae.safetensors".into(),
            "jobs".into(),
        );
        let params = ResolvedZImageParams {
            prompt: "a cat".into(),
            width: 512,
            height: 1024,
            seed: Some(7),
        };
        let args = build_sd_cli_args(&config, &params, Path::new("out.png"));
        let args = args.iter().map(|arg| arg.to_string_lossy()).collect::<Vec<_>>();
        assert!(args.windows(2).any(|v| v == ["--cfg-scale", "1.0"]));
        assert!(args.windows(2).any(|v| v == ["--steps", "8"]));
        assert!(args.windows(2).any(|v| v == ["--llm", "qwen.gguf"]));
        assert!(args.contains(&std::borrow::Cow::Borrowed("--offload-to-cpu")));
        assert!(args.contains(&std::borrow::Cow::Borrowed("--diffusion-fa")));
        assert!(args.windows(2).any(|v| v == ["--seed", "7"]));
    }
}

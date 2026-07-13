//! Opt-in local speech recognition powered by a pinned whisper.cpp runtime.
//!
//! The service owns only downloads, integrity state and one active model. The
//! native runtime is launched for a single transcription request and exits
//! afterwards, so enabling local ASR has no resident CPU or model-memory cost.

use std::collections::{HashMap, HashSet};
use std::ffi::OsStr;
use std::path::{Component, Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::{Duration, Instant};

use futures_util::StreamExt;
use nomifun_api_types::{
    AsrCapability, AsrEngine, AsrModelCatalogEntry, AsrModelServiceStatus, LocalModelErrorKind,
    LocalModelInstallPhase, LocalModelProgressComponent, LocalModelRuntimeBackend,
    LocalModelRuntimePhase, LocalModelState, LocalModelTransferProgress,
    LocalRuntimeStatus,
};
use nomifun_common::AppError;
use reqwest::header::{CONTENT_LENGTH, CONTENT_RANGE, RANGE};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::process::Command;
use tokio::sync::{Mutex, Notify, OwnedRwLockReadGuard, RwLock, Semaphore};
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

const PROTOCOL_VERSION: &str = "2";
const LOCAL_AI_DIR: &str = "local-ai";
const ASR_DIR: &str = "asr";
const RUNTIME_DIR: &str = "runtime";
const MODELS_DIR: &str = "models";
const DOWNLOADS_DIR: &str = "downloads";
const JOBS_DIR: &str = "jobs";
const STATE_FILE: &str = "state.json";
const STATE_VERSION: u32 = 2;
const WHISPER_CPP_VERSION: &str = "1.9.1";
const FUNASR_LLAMACPP_VERSION: &str = "0.1.4";
const DOWNLOAD_PROGRESS_INTERVAL: Duration = Duration::from_millis(250);
const TRANSCRIBE_TIMEOUT: Duration = Duration::from_secs(15 * 60);
const DISK_SAFETY_BYTES: u64 = 64 * 1024 * 1024;
const RUNTIME_EXTRACT_RESERVE_BYTES: u64 = 32 * 1024 * 1024;
const MAX_ARCHIVE_ENTRIES: usize = 512;
const MAX_ARCHIVE_EXPANDED_BYTES: u64 = 128 * 1024 * 1024;
const MAX_AUDIO_BYTES: usize = 30 * 1024 * 1024;

#[derive(Clone)]
struct AsrModelArtifact {
    entry: AsrModelCatalogEntry,
    engine: AsrEngineKind,
    file_name: &'static str,
    url: &'static str,
    sha256: &'static str,
    size: u64,
    auxiliary: Option<AsrAuxiliaryArtifact>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
enum AsrEngineKind {
    WhisperCpp,
    FunAsrParaformer,
}

impl AsrEngineKind {
    fn runtime_family(self) -> RuntimeFamily {
        match self {
            Self::WhisperCpp => RuntimeFamily::WhisperCpp,
            Self::FunAsrParaformer => RuntimeFamily::FunAsrLlamaCpp,
        }
    }
}

#[derive(Clone, Copy)]
struct AsrAuxiliaryArtifact {
    id: &'static str,
    file_name: &'static str,
    url: &'static str,
    sha256: &'static str,
    size: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
enum RuntimeFamily {
    WhisperCpp,
    FunAsrLlamaCpp,
}

#[derive(Clone, Copy)]
struct RuntimeArtifact {
    family: RuntimeFamily,
    version: &'static str,
    executable: &'static str,
    file_name: &'static str,
    url: &'static str,
    sha256: &'static str,
    size: u64,
    backend: LocalModelRuntimeBackend,
}

#[derive(Debug)]
struct ActiveInstall {
    model_id: String,
    generation: u64,
    cancel: CancellationToken,
    done: Arc<Notify>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct PersistedState {
    #[serde(default = "state_version")]
    version: u32,
    #[serde(default)]
    installed_model_ids: Vec<String>,
    #[serde(default)]
    active_model_id: Option<String>,
}

impl Default for PersistedState {
    fn default() -> Self {
        Self {
            version: STATE_VERSION,
            installed_model_ids: Vec::new(),
            active_model_id: None,
        }
    }
}

fn state_version() -> u32 {
    STATE_VERSION
}

struct MutableState {
    persisted: PersistedState,
    models: HashMap<String, LocalModelState>,
    verified_models: HashSet<String>,
    present_runtimes: HashSet<RuntimeFamily>,
    verified_runtimes: HashSet<RuntimeFamily>,
    active_install: Option<ActiveInstall>,
    next_generation: u64,
    last_error: Option<String>,
}

#[derive(Debug)]
struct AsrFailure {
    kind: LocalModelErrorKind,
    safe_message: &'static str,
    detail: String,
    cancelled: bool,
}

/// Removes one safely-created ASR job directory even when the async request is
/// cancelled while whisper-cli is running.
struct AsrJobGuard {
    root: PathBuf,
    job_root: PathBuf,
}

impl Drop for AsrJobGuard {
    fn drop(&mut self) {
        if let Err(error) = remove_managed_tree(&self.root, &self.job_root) {
            warn!(
                error = %error,
                "could not remove local ASR temporary job directory"
            );
        }
    }
}

impl AsrFailure {
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
            safe_message: "ASR model download is paused.",
            detail: "ASR model install cancelled by user".into(),
            cancelled: true,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocalAsrTranscription {
    pub text: String,
    pub model_id: String,
    pub language: Option<String>,
}

/// One-click manager for the curated multilingual whisper.cpp models.
pub struct AsrModelService {
    root: PathBuf,
    http_client: reqwest::Client,
    catalog: Vec<AsrModelArtifact>,
    runtime_artifacts: Vec<RuntimeArtifact>,
    allow_insecure_loopback_downloads: bool,
    state: Mutex<MutableState>,
    mutation_lock: Mutex<()>,
    persist_lock: Mutex<()>,
    verification_lock: Mutex<()>,
    runtime_lifecycle: Arc<RwLock<()>>,
    transcription_gate: Arc<Semaphore>,
}

impl AsrModelService {
    pub fn opted_in(data_dir: impl AsRef<Path>) -> bool {
        let path = asr_root(&data_dir.as_ref().join(LOCAL_AI_DIR)).join(STATE_FILE);
        let Ok(bytes) = std::fs::read(path) else {
            return false;
        };
        let Ok(state) = serde_json::from_slice::<PersistedState>(&bytes) else {
            return false;
        };
        matches!(state.version, 1 | STATE_VERSION)
            && (state.active_model_id.is_some() || !state.installed_model_ids.is_empty())
    }

    pub async fn new(data_dir: impl AsRef<Path>) -> Result<Arc<Self>, AppError> {
        let root = data_dir.as_ref().join(LOCAL_AI_DIR);
        let runtime_artifacts = production_runtime_artifacts();
        Self::new_inner(
            root,
            asr_download_client(),
            built_in_catalog(),
            runtime_artifacts,
            false,
        )
        .await
    }

    async fn new_inner(
        root: PathBuf,
        http_client: reqwest::Client,
        catalog: Vec<AsrModelArtifact>,
        runtime_artifacts: Vec<RuntimeArtifact>,
        allow_insecure_loopback_downloads: bool,
    ) -> Result<Arc<Self>, AppError> {
        prepare_layout(&root, &catalog, &runtime_artifacts)
            .map_err(|error| AppError::Internal(format!("prepare ASR model directory: {error}")))?;
        let root = std::fs::canonicalize(&root)
            .map_err(|error| AppError::Internal(format!("resolve ASR model directory: {error}")))?;
        let mut persisted = load_state(&root).await;
        let present_runtimes = catalog
            .iter()
            .filter_map(|artifact| {
                let family = artifact.engine.runtime_family();
                runtime_is_present(&root, family, &runtime_artifacts).then_some(family)
            })
            .collect::<HashSet<_>>();
        let known_ids = catalog
            .iter()
            .map(|artifact| artifact.entry.id.as_str())
            .collect::<HashSet<_>>();
        persisted
            .installed_model_ids
            .retain(|id| known_ids.contains(id.as_str()));

        let mut models = HashMap::new();
        for artifact in &catalog {
            let final_path = model_path(&root, artifact);
            let installed = persisted
                .installed_model_ids
                .iter()
                .any(|id| id == &artifact.entry.id)
                && file_len(&final_path).await == artifact.size
                && auxiliary_is_present(&root, artifact).await;
            if !installed {
                persisted
                    .installed_model_ids
                    .retain(|id| id != &artifact.entry.id);
            }
            let partial = partial_model_path(&root, artifact);
            let downloaded = if installed {
                artifact_total_size(artifact)
            } else {
                file_len(&partial).await.min(artifact.size)
            };
            models.insert(
                artifact.entry.id.clone(),
                LocalModelState {
                    model_id: artifact.entry.id.clone(),
                    install_phase: if installed
                        && present_runtimes.contains(&artifact.engine.runtime_family())
                    {
                        LocalModelInstallPhase::Installed
                    } else if installed {
                        LocalModelInstallPhase::Failed
                    } else if downloaded > 0 {
                        LocalModelInstallPhase::Paused
                    } else {
                        LocalModelInstallPhase::NotInstalled
                    },
                    progress: None,
                    installed_bytes: downloaded,
                    runtime_phase: LocalModelRuntimePhase::Stopped,
                    error_kind: (installed
                        && !present_runtimes.contains(&artifact.engine.runtime_family()))
                        .then_some(LocalModelErrorKind::RuntimeUnavailable),
                    message: (installed
                        && !present_runtimes.contains(&artifact.engine.runtime_family()))
                    .then(|| {
                        "The local ASR runtime is missing. Retry installation to repair it."
                            .into()
                    }),
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
        let missing_runtime_for_install = persisted.installed_model_ids.iter().any(|id| {
            catalog
                .iter()
                .find(|artifact| &artifact.entry.id == id)
                .is_some_and(|artifact| {
                    !present_runtimes.contains(&artifact.engine.runtime_family())
                })
        });

        let verified_runtimes = catalog
            .iter()
            .filter_map(|artifact| {
                configured_runtime_path(artifact.engine.runtime_family())
                    .map(|_| artifact.engine.runtime_family())
            })
            .collect();

        Ok(Arc::new(Self {
            root,
            http_client,
            catalog,
            runtime_artifacts,
            allow_insecure_loopback_downloads,
            state: Mutex::new(MutableState {
                persisted,
                models,
                verified_models: HashSet::new(),
                verified_runtimes,
                present_runtimes,
                active_install: None,
                next_generation: 0,
                last_error: missing_runtime_for_install.then(|| {
                    "The local ASR runtime is missing. Retry installation to repair it."
                        .into()
                }),
            }),
            mutation_lock: Mutex::new(()),
            persist_lock: Mutex::new(()),
            verification_lock: Mutex::new(()),
            runtime_lifecycle: Arc::new(RwLock::new(())),
            transcription_gate: Arc::new(Semaphore::new(1)),
        }))
    }

    pub async fn catalog(&self) -> Vec<AsrModelCatalogEntry> {
        self.catalog
            .iter()
            .map(|artifact| artifact.entry.clone())
            .collect()
    }

    pub async fn status(&self) -> AsrModelServiceStatus {
        let state = self.state.lock().await;
        snapshot(
            &state,
            &self.catalog,
            &self.runtime_artifacts,
        )
    }

    fn artifact(&self, model_id: &str) -> Result<AsrModelArtifact, AppError> {
        self.catalog
            .iter()
            .find(|artifact| artifact.entry.id == model_id)
            .cloned()
            .ok_or_else(|| AppError::NotFound("ASR model is not in the curated catalog".into()))
    }

    fn runtime_artifact(&self, family: RuntimeFamily) -> Option<&RuntimeArtifact> {
        self.runtime_artifacts
            .iter()
            .find(|runtime| runtime.family == family)
    }

    pub async fn install(
        self: &Arc<Self>,
        model_id: &str,
    ) -> Result<AsrModelServiceStatus, AppError> {
        let artifact = self.artifact(model_id)?;
        if !runtime_supported(
            artifact.engine.runtime_family(),
            &self.runtime_artifacts,
        ) {
            return Err(AppError::BadRequest(
                "Local speech recognition is not supported on this platform".into(),
            ));
        }

        let _mutation = self.mutation_lock.lock().await;
        self.start_install_locked(artifact).await
    }

    async fn start_install_locked(
        self: &Arc<Self>,
        artifact: AsrModelArtifact,
    ) -> Result<AsrModelServiceStatus, AppError> {
        let model_id = artifact.entry.id.clone();
        let (generation, cancel, done) = {
            let mut state = self.state.lock().await;
            if let Some(active) = &state.active_install {
                if active.model_id == model_id {
                    return Ok(snapshot(
                        &state,
                        &self.catalog,
                        &self.runtime_artifacts,
                    ));
                }
                return Err(AppError::Conflict(
                    "Another ASR model installation is already running".into(),
                ));
            }
            if state
                .models
                .get(&model_id)
                .is_some_and(|model| model.install_phase == LocalModelInstallPhase::Installed)
                && state
                    .present_runtimes
                    .contains(&artifact.engine.runtime_family())
            {
                drop(state);
                return self.set_active_locked(&model_id, true).await;
            }
            state.next_generation = state.next_generation.wrapping_add(1).max(1);
            let generation = state.next_generation;
            let cancel = CancellationToken::new();
            let done = Arc::new(Notify::new());
            state.active_install = Some(ActiveInstall {
                model_id: model_id.clone(),
                generation,
                cancel: cancel.clone(),
                done: done.clone(),
            });
            let model = state
                .models
                .get_mut(&model_id)
                .expect("ASR catalog and state stay aligned");
            model.install_phase = LocalModelInstallPhase::Downloading;
            model.progress = None;
            model.error_kind = None;
            model.message = None;
            state.last_error = None;
            (generation, cancel, done)
        };

        let service = self.clone();
        tokio::spawn(async move {
            service.run_install(artifact, generation, cancel).await;
            // `notify_one` retains a permit if cancellation has not started
            // waiting yet, avoiding a lost-wakeup race in `cancel`.
            done.notify_one();
        });
        Ok(self.status().await)
    }

    /// Pause the current resumable transfer.
    pub async fn cancel(&self, model_id: &str) -> Result<AsrModelServiceStatus, AppError> {
        self.artifact(model_id)?;
        let _mutation = self.mutation_lock.lock().await;
        let done = {
            let state = self.state.lock().await;
            let active = state.active_install.as_ref().ok_or_else(|| {
                AppError::Conflict("The ASR model is not currently downloading".into())
            })?;
            if active.model_id != model_id {
                return Err(AppError::Conflict(
                    "A different ASR model is currently downloading".into(),
                ));
            }
            active.cancel.cancel();
            active.done.clone()
        };
        done.notified().await;
        Ok(self.status().await)
    }

    pub async fn set_active(
        &self,
        model_id: &str,
        enabled: bool,
    ) -> Result<AsrModelServiceStatus, AppError> {
        self.artifact(model_id)?;
        let _mutation = self.mutation_lock.lock().await;
        self.set_active_locked(model_id, enabled).await
    }

    async fn set_active_locked(
        &self,
        model_id: &str,
        enabled: bool,
    ) -> Result<AsrModelServiceStatus, AppError> {
        {
            let mut state = self.state.lock().await;
            if state.active_install.is_some() {
                return Err(AppError::Conflict("An ASR model is still downloading".into()));
            }
            if enabled
                && !state.models.get(model_id).is_some_and(|model| {
                    model.install_phase == LocalModelInstallPhase::Installed
                })
            {
                return Err(AppError::Conflict("Install the ASR model first".into()));
            }
            if enabled {
                let artifact = self.artifact(model_id)?;
                if !state
                    .present_runtimes
                    .contains(&artifact.engine.runtime_family())
                {
                return Err(AppError::Conflict(
                    "Repair the local ASR runtime before enabling the model".into(),
                ));
                }
            }
            if enabled {
                state.persisted.active_model_id = Some(model_id.to_owned());
            } else if state.persisted.active_model_id.as_deref() == Some(model_id) {
                state.persisted.active_model_id = None;
            }
            state.last_error = None;
        }
        self.save_state().await?;
        Ok(self.status().await)
    }

    pub async fn delete(&self, model_id: &str) -> Result<AsrModelServiceStatus, AppError> {
        let artifact = self.artifact(model_id)?;
        let _mutation = self.mutation_lock.lock().await;
        if self.state.lock().await.active_install.is_some() {
            return Err(AppError::Conflict(
                "Pause the ASR model download before deleting it".into(),
            ));
        }
        let _transcription = self
            .transcription_gate
            .clone()
            .try_acquire_owned()
            .map_err(|_| AppError::Conflict("Local speech recognition is busy".into()))?;

        for path in [
            model_path(&self.root, &artifact),
            partial_model_path(&self.root, &artifact),
        ] {
            remove_file_if_exists(&self.root, &path).await?;
        }
        if let Some(auxiliary) = artifact.auxiliary {
            let used_by_other_installed_model = {
                let state = self.state.lock().await;
                self.catalog.iter().any(|candidate| {
                    candidate.entry.id != model_id
                        && state
                            .persisted
                            .installed_model_ids
                            .contains(&candidate.entry.id)
                        && candidate
                            .auxiliary
                            .is_some_and(|candidate_auxiliary| {
                                candidate_auxiliary.id == auxiliary.id
                            })
                })
            };
            if !used_by_other_installed_model {
                for path in [
                    auxiliary_path(&self.root, &auxiliary),
                    partial_auxiliary_path(&self.root, &auxiliary),
                ] {
                    remove_file_if_exists(&self.root, &path).await?;
                }
            }
        }
        let mut state = self.state.lock().await;
        state
            .persisted
            .installed_model_ids
            .retain(|id| id != model_id);
        state.verified_models.remove(model_id);
        if state.persisted.active_model_id.as_deref() == Some(model_id) {
            state.persisted.active_model_id = None;
        }
        state.models.insert(
            model_id.to_owned(),
            LocalModelState {
                model_id: model_id.to_owned(),
                install_phase: LocalModelInstallPhase::NotInstalled,
                progress: None,
                installed_bytes: 0,
                runtime_phase: LocalModelRuntimePhase::Stopped,
                error_kind: None,
                message: None,
            },
        );
        state.last_error = None;
        drop(state);
        self.save_state().await?;
        Ok(self.status().await)
    }

    pub async fn has_active_model(&self) -> bool {
        self.state.lock().await.persisted.active_model_id.is_some()
    }

    pub async fn transcribe(
        &self,
        audio_data: Vec<u8>,
        file_name: &str,
        mime_type: &str,
        language_hint: Option<&str>,
    ) -> Result<LocalAsrTranscription, AppError> {
        if audio_data.is_empty() {
            return Err(AppError::BadRequest("The audio file is empty".into()));
        }
        if audio_data.len() > MAX_AUDIO_BYTES {
            return Err(AppError::BadRequest("The audio file is too large".into()));
        }
        let _permit = self
            .transcription_gate
            .clone()
            .try_acquire_owned()
            .map_err(|_| AppError::Conflict("Local speech recognition is busy".into()))?;

        let active_id = self
            .state
            .lock()
            .await
            .persisted
            .active_model_id
            .clone()
            .ok_or_else(|| AppError::ProviderUnavailable("No local ASR model is active".into()))?;
        let artifact = self.artifact(&active_id)?;
        let _runtime = self.prepare_runtime_for_use(&artifact).await?;
        let executable = self.runtime_executable(&artifact)?;

        let extension = safe_audio_extension(file_name, mime_type).ok_or_else(|| {
            AppError::BadRequest(
                "Local speech recognition supports WAV, MP3, OGG and FLAC audio".into(),
            )
        })?;
        let job_id = random_job_id()?;
        let job_root = jobs_dir(&self.root).join(job_id);
        prepare_managed_directory(&self.root, &job_root)
            .map_err(|error| AppError::Internal(format!("prepare ASR job directory: {error}")))?;
        let _job_guard = AsrJobGuard {
            root: self.root.clone(),
            job_root: job_root.clone(),
        };
        let input = job_root.join(format!("input.{extension}"));
        prepare_managed_file(&self.root, &input)
            .map_err(|error| AppError::Internal(format!("prepare ASR audio input: {error}")))?;
        tokio::fs::write(&input, audio_data)
            .await
            .map_err(|error| AppError::Internal(format!("write ASR audio input: {error}")))?;

        let model = model_path(&self.root, &artifact);
        let threads = std::thread::available_parallelism()
            .map(|value| value.get().clamp(1, 8))
            .unwrap_or(4);
        let mut command = Command::new(executable);
        let language = match artifact.engine {
            AsrEngineKind::WhisperCpp => {
                let language = normalize_language_hint(language_hint);
                let output_prefix = job_root.join("transcript");
                command
                    .arg("-m")
                    .arg(&model)
                    .arg("-f")
                    .arg(&input)
                    .arg("-l")
                    .arg(language.as_deref().unwrap_or("auto"))
                    .arg("-t")
                    .arg(threads.to_string())
                    .arg("-oj")
                    .arg("-of")
                    .arg(&output_prefix)
                    .arg("-np")
                    .arg("-nt");
                language
            }
            AsrEngineKind::FunAsrParaformer => {
                command.arg("-m").arg(&model).arg("-a").arg(&input);
                if let Some(auxiliary) = artifact.auxiliary {
                    command
                        .arg("--vad")
                        .arg(auxiliary_path(&self.root, &auxiliary));
                }
                Some("zh".into())
            }
        };
        command
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            // If the request future is cancelled or its timeout fires, dropping
            // the child future must not leave the native ASR process behind.
            .kill_on_drop(true);
        #[cfg(windows)]
        command.creation_flags(0x0800_0000);

        let output = match command.spawn() {
            Ok(child) => {
                let result = tokio::time::timeout(TRANSCRIBE_TIMEOUT, child.wait_with_output())
                    .await;
                match result {
                    Ok(Ok(output)) => Ok(output),
                    Ok(Err(error)) => Err(AppError::ProviderUnavailable(format!(
                        "Local ASR runtime failed: {error}"
                    ))),
                    Err(_) => Err(AppError::ProviderUnavailable(
                        "Local transcription timed out".into(),
                    )),
                }
            }
            Err(error) => Err(AppError::ProviderUnavailable(format!(
                "Could not start local ASR runtime: {error}"
            ))),
        };
        let output = match output {
            Ok(output) => output,
            Err(error) => return Err(error),
        };
        let result = if output.status.success() {
            let text = match artifact.engine {
                AsrEngineKind::WhisperCpp => {
                    let output_path = job_root.join("transcript.json");
                    let raw = tokio::fs::read(&output_path).await.map_err(|error| {
                        AppError::ProviderUnavailable(format!(
                            "Local ASR runtime did not produce a result: {error}"
                        ))
                    })?;
                    parse_whisper_json(&raw)?
                }
                AsrEngineKind::FunAsrParaformer => parse_funasr_stdout(&output.stdout),
            };
            if text.is_empty() {
                Err(AppError::ProviderUnavailable(
                    "Local ASR returned an empty transcription".into(),
                ))
            } else {
                Ok(LocalAsrTranscription {
                    text,
                    model_id: active_id,
                    language,
                })
            }
        } else {
            warn!(
                engine = ?artifact.engine,
                status = ?output.status.code(),
                stderr = %sanitize_process_output(&output.stderr),
                "local ASR runtime failed"
            );
            Err(AppError::ProviderUnavailable(
                "Local speech recognition failed to process this audio format".into(),
            ))
        };
        result
    }

    async fn run_install(
        self: Arc<Self>,
        artifact: AsrModelArtifact,
        generation: u64,
        cancel: CancellationToken,
    ) {
        let result = self.install_artifacts(&artifact, generation, &cancel).await;
        self.finish_install(artifact, generation, result).await;
    }

    async fn finish_install(
        &self,
        artifact: AsrModelArtifact,
        generation: u64,
        result: Result<(), AsrFailure>,
    ) {
        let mut state = self.state.lock().await;
        if !state
            .active_install
            .as_ref()
            .is_some_and(|active| active.generation == generation)
        {
            return;
        }
        state.active_install = None;
        match result {
            Ok(()) => {
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
                // One-click install is immediately useful. Activating a second
                // model atomically replaces the previous active model.
                state.persisted.active_model_id = Some(artifact.entry.id.clone());
                if let Some(model) = state.models.get_mut(&artifact.entry.id) {
                    model.install_phase = LocalModelInstallPhase::Installed;
                    model.progress = None;
                    model.installed_bytes = artifact_total_size(&artifact);
                    model.error_kind = None;
                    model.message = Some("ASR model is installed and ready to use.".into());
                }
                state.verified_models.insert(artifact.entry.id.clone());
                state
                    .present_runtimes
                    .insert(artifact.engine.runtime_family());
                state
                    .verified_runtimes
                    .insert(artifact.engine.runtime_family());
                state.last_error = None;
                info!(model = %artifact.entry.id, "local ASR model installed");
            }
            Err(error) if error.cancelled => {
                if let Some(model) = state.models.get_mut(&artifact.entry.id) {
                    model.install_phase = LocalModelInstallPhase::Paused;
                    model.progress = model.progress.take().map(|mut progress| {
                        progress.bytes_per_second = 0;
                        progress
                    });
                    model.message = Some(error.safe_message.into());
                    model.error_kind = None;
                }
                state.last_error = None;
            }
            Err(error) => {
                warn!(error = %error.detail, model = %artifact.entry.id, "ASR model install failed");
                if let Some(model) = state.models.get_mut(&artifact.entry.id) {
                    model.install_phase = LocalModelInstallPhase::Failed;
                    model.progress = None;
                    model.error_kind = Some(error.kind);
                    model.message = Some(error.safe_message.into());
                }
                state.last_error = Some(error.safe_message.into());
            }
        }
        drop(state);
        if let Err(error) = self.save_state().await {
            warn!(error = %error, "could not persist local ASR state");
        }
    }

    #[cfg(test)]
    async fn run_install_finish_for_test(
        &self,
        artifact: AsrModelArtifact,
        generation: u64,
        result: Result<(), AsrFailure>,
    ) {
        self.finish_install(artifact, generation, result).await;
    }

    async fn install_artifacts(
        &self,
        artifact: &AsrModelArtifact,
        generation: u64,
        cancel: &CancellationToken,
    ) -> Result<(), AsrFailure> {
        self.ensure_disk_space(artifact).await?;
        let family = artifact.engine.runtime_family();
        if configured_runtime_path(family).is_none() {
            let runtime = self.runtime_artifact(family).ok_or_else(|| {
                AsrFailure::new(
                    LocalModelErrorKind::UnsupportedPlatform,
                    "Local speech recognition is unavailable on this platform.",
                    "no compatible ASR runtime artifact",
                )
            })?;
            if find_installed_runtime_executable(&self.root, runtime).is_err() {
                let destination = runtime_archive_path(&self.root, runtime);
                self.download_verified(
                    runtime.url,
                    runtime.sha256,
                    runtime.size,
                    &destination,
                    &partial_runtime_path(&self.root, runtime),
                    &artifact.entry.id,
                    generation,
                    LocalModelProgressComponent::Runtime,
                    cancel,
                )
                .await?;
                let _runtime_write = self.runtime_lifecycle.write().await;
                if find_installed_runtime_executable(&self.root, runtime).is_err() {
                    self.extract_runtime(runtime, cancel).await?;
                }
            }
            if cancel.is_cancelled() {
                return Err(AsrFailure::cancelled());
            }
        }
        self.download_verified(
            artifact.url,
            artifact.sha256,
            artifact.size,
            &model_path(&self.root, artifact),
            &partial_model_path(&self.root, artifact),
            &artifact.entry.id,
            generation,
            LocalModelProgressComponent::Model,
            cancel,
        )
        .await?;
        if let Some(auxiliary) = artifact.auxiliary {
            self.download_verified(
                auxiliary.url,
                auxiliary.sha256,
                auxiliary.size,
                &auxiliary_path(&self.root, &auxiliary),
                &partial_auxiliary_path(&self.root, &auxiliary),
                &artifact.entry.id,
                generation,
                LocalModelProgressComponent::AsrAuxiliary,
                cancel,
            )
            .await?;
        }
        Ok(())
    }

    async fn download_verified(
        &self,
        url: &str,
        sha256: &str,
        size: u64,
        destination: &Path,
        partial: &Path,
        model_id: &str,
        generation: u64,
        component: LocalModelProgressComponent,
        cancel: &CancellationToken,
    ) -> Result<(), AsrFailure> {
        if file_len(destination).await == size {
            self.set_phase(model_id, generation, LocalModelInstallPhase::Verifying)
                .await;
            if hash_file(destination, cancel).await? == sha256 {
                return Ok(());
            }
            remove_file_if_exists_failure(&self.root, destination).await?;
        }

        let mut last_error = None;
        for source in download_sources(url) {
            match self
                .download_once(
                    &source,
                    sha256,
                    size,
                    destination,
                    partial,
                    model_id,
                    generation,
                    component,
                    cancel,
                )
                .await
            {
                Ok(()) => return Ok(()),
                Err(error) if error.cancelled => return Err(error),
                Err(error) => {
                    warn!(error = %error.detail, model = model_id, "ASR artifact source failed");
                    last_error = Some(error);
                }
            }
        }
        Err(last_error.unwrap_or_else(|| {
            AsrFailure::new(
                LocalModelErrorKind::Network,
                "ASR model download failed. Check the network and try again.",
                "all ASR artifact sources failed",
            )
        }))
    }

    #[allow(clippy::too_many_arguments)]
    async fn download_once(
        &self,
        url: &str,
        sha256: &str,
        size: u64,
        destination: &Path,
        partial: &Path,
        model_id: &str,
        generation: u64,
        component: LocalModelProgressComponent,
        cancel: &CancellationToken,
    ) -> Result<(), AsrFailure> {
        prepare_managed_file(&self.root, partial).map_err(storage_failure)?;
        let mut offset = file_len(partial).await;
        if offset > size {
            remove_file_if_exists_failure(&self.root, partial).await?;
            offset = 0;
        }
        if offset == size {
            self.set_phase(model_id, generation, LocalModelInstallPhase::Verifying)
                .await;
            if hash_file(partial, cancel).await? == sha256 {
                commit_partial(&self.root, partial, destination).await?;
                return Ok(());
            }
            remove_file_if_exists_failure(&self.root, partial).await?;
            offset = 0;
        }
        self.set_progress(model_id, generation, component, offset, size, 0)
            .await;

        let mut request = self.http_client.get(url);
        if offset > 0 {
            request = request.header(RANGE, format!("bytes={offset}-"));
        }
        let response = tokio::select! {
            _ = cancel.cancelled() => return Err(AsrFailure::cancelled()),
            response = request.send() => response.map_err(|error| AsrFailure::new(
                LocalModelErrorKind::Network,
                "ASR model download failed. Check the network and try again.",
                error.to_string(),
            ))?,
        };
        if !allowed_download_url(response.url())
            && !(self.allow_insecure_loopback_downloads
                && loopback_download_url(response.url()))
        {
            return Err(AsrFailure::new(
                LocalModelErrorKind::Network,
                "The ASR download source did not pass safety checks.",
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
                    AsrFailure::new(
                        LocalModelErrorKind::Network,
                        "The ASR download server returned an invalid resume response.",
                        "missing or invalid Content-Range",
                    )
                })?;
            if range != (offset, size.saturating_sub(1), size) {
                return Err(AsrFailure::new(
                    LocalModelErrorKind::Network,
                    "The ASR download server returned a mismatched resume range.",
                    format!("unexpected Content-Range {range:?}"),
                ));
            }
            append = true;
        } else if offset > 0 && status.is_success() {
            offset = 0;
        } else if !status.is_success() {
            return Err(AsrFailure::new(
                LocalModelErrorKind::Network,
                "The ASR download service is temporarily unavailable.",
                format!("HTTP status {status}"),
            ));
        }
        if let Some(length) = response
            .headers()
            .get(CONTENT_LENGTH)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.parse::<u64>().ok())
        {
            let expected = size.saturating_sub(offset);
            if length != expected {
                return Err(AsrFailure::new(
                    LocalModelErrorKind::Network,
                    "The ASR model download has an unexpected size.",
                    format!("Content-Length {length}, expected {expected}"),
                ));
            }
        }

        let mut options = tokio::fs::OpenOptions::new();
        options.create(true).write(true);
        if append {
            options.append(true);
        }
        let mut file = options.open(partial).await.map_err(|error| {
            AsrFailure::new(
                LocalModelErrorKind::Unknown,
                "Could not write the ASR model download.",
                error.to_string(),
            )
        })?;
        if !append {
            file.set_len(0).await.map_err(|error| {
                AsrFailure::new(
                    LocalModelErrorKind::Unknown,
                    "Could not reset the ASR model download.",
                    error.to_string(),
                )
            })?;
        }

        let started = Instant::now();
        let mut last_report = Instant::now();
        let mut downloaded = offset;
        let mut stream = response.bytes_stream();
        loop {
            let next = tokio::select! {
                _ = cancel.cancelled() => {
                    file.sync_data().await.map_err(|error| AsrFailure::new(
                        LocalModelErrorKind::Unknown,
                        "Could not preserve the paused ASR download.",
                        error.to_string(),
                    ))?;
                    return Err(AsrFailure::cancelled());
                }
                next = stream.next() => next,
            };
            let Some(chunk) = next else { break };
            let chunk = chunk.map_err(|error| {
                AsrFailure::new(
                    LocalModelErrorKind::Network,
                    "ASR model download was interrupted and can be resumed.",
                    error.to_string(),
                )
            })?;
            let next_total = downloaded.saturating_add(chunk.len() as u64);
            if next_total > size {
                drop(file);
                remove_file_if_exists_failure(&self.root, partial).await?;
                return Err(AsrFailure::new(
                    LocalModelErrorKind::Network,
                    "The ASR model download exceeded its expected size.",
                    format!("received more than {size} bytes"),
                ));
            }
            file.write_all(&chunk).await.map_err(|error| {
                AsrFailure::new(
                    LocalModelErrorKind::Unknown,
                    "Could not write the ASR model download.",
                    error.to_string(),
                )
            })?;
            downloaded = next_total;
            if last_report.elapsed() >= DOWNLOAD_PROGRESS_INTERVAL {
                let rate = ((downloaded.saturating_sub(offset)) as f64
                    / started.elapsed().as_secs_f64().max(0.001)) as u64;
                self.set_progress(model_id, generation, component, downloaded, size, rate)
                    .await;
                last_report = Instant::now();
            }
        }
        file.sync_all().await.map_err(|error| {
            AsrFailure::new(
                LocalModelErrorKind::Unknown,
                "Could not commit the ASR model download.",
                error.to_string(),
            )
        })?;
        drop(file);
        if downloaded != size {
            return Err(AsrFailure::new(
                LocalModelErrorKind::Network,
                "ASR model download was interrupted and can be resumed.",
                format!("downloaded {downloaded} of {size}"),
            ));
        }
        self.set_phase(model_id, generation, LocalModelInstallPhase::Verifying)
            .await;
        if hash_file(partial, cancel).await? != sha256 {
            remove_file_if_exists_failure(&self.root, partial).await?;
            return Err(AsrFailure::new(
                LocalModelErrorKind::ChecksumMismatch,
                "ASR model integrity verification failed. Download it again.",
                "ASR artifact SHA-256 mismatch",
            ));
        }
        commit_partial(&self.root, partial, destination).await
    }

    async fn set_phase(
        &self,
        model_id: &str,
        generation: u64,
        phase: LocalModelInstallPhase,
    ) {
        let mut state = self.state.lock().await;
        if !state
            .active_install
            .as_ref()
            .is_some_and(|active| active.generation == generation)
        {
            return;
        }
        if let Some(model) = state.models.get_mut(model_id) {
            model.install_phase = phase;
            if let Some(progress) = model.progress.as_mut() {
                progress.bytes_per_second = 0;
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    async fn set_progress(
        &self,
        model_id: &str,
        generation: u64,
        component: LocalModelProgressComponent,
        downloaded_bytes: u64,
        total_bytes: u64,
        bytes_per_second: u64,
    ) {
        let mut state = self.state.lock().await;
        if !state
            .active_install
            .as_ref()
            .is_some_and(|active| active.generation == generation)
        {
            return;
        }
        if let Some(model) = state.models.get_mut(model_id) {
            model.install_phase = LocalModelInstallPhase::Downloading;
            model.progress = Some(LocalModelTransferProgress {
                component,
                downloaded_bytes,
                total_bytes,
                bytes_per_second,
            });
            let completed_before_component = match component {
                LocalModelProgressComponent::AsrAuxiliary => self
                    .catalog
                    .iter()
                    .find(|artifact| artifact.entry.id == model_id)
                    .map_or(0, |artifact| artifact.size),
                _ => 0,
            };
            model.installed_bytes =
                completed_before_component.saturating_add(downloaded_bytes);
        }
    }

    async fn extract_runtime(
        &self,
        runtime: &RuntimeArtifact,
        cancel: &CancellationToken,
    ) -> Result<(), AsrFailure> {
        if cancel.is_cancelled() {
            return Err(AsrFailure::cancelled());
        }
        let archive = runtime_archive_path(&self.root, runtime);
        let staging = runtime_staging_dir(&self.root, runtime);
        let destination = runtime_install_dir(&self.root, runtime);
        let executable = runtime.executable;
        let root = self.root.clone();
        tokio::task::spawn_blocking(move || {
            if staging.exists() {
                remove_managed_tree(&root, &staging)?;
            }
            prepare_managed_directory(&root, &staging)?;
            extract_runtime_zip(&archive, &staging)?;
            find_runtime_executable(&staging, executable)?;
            if let Some(parent) = destination.parent() {
                prepare_managed_directory(&root, parent)?;
            }
            if destination.exists() {
                remove_managed_tree(&root, &destination)?;
            }
            std::fs::rename(&staging, &destination)?;
            Ok::<_, std::io::Error>(())
        })
        .await
        .map_err(|error| {
            AsrFailure::new(
                LocalModelErrorKind::RuntimeUnavailable,
                "Could not install the local ASR runtime.",
                error.to_string(),
            )
        })?
        .map_err(|error| {
            AsrFailure::new(
                LocalModelErrorKind::RuntimeUnavailable,
                "Could not install the local ASR runtime.",
                error.to_string(),
            )
        })
    }

    async fn prepare_runtime_for_use(
        &self,
        artifact: &AsrModelArtifact,
    ) -> Result<OwnedRwLockReadGuard<()>, AppError> {
        let runtime = self.runtime_lifecycle.clone().read_owned().await;
        self.verify_before_use(artifact).await?;
        Ok(runtime)
    }

    async fn verify_before_use(&self, artifact: &AsrModelArtifact) -> Result<(), AppError> {
        let family = artifact.engine.runtime_family();
        {
            let state = self.state.lock().await;
            if state.verified_models.contains(&artifact.entry.id)
                && state.verified_runtimes.contains(&family)
            {
                return Ok(());
            }
        }
        let _verification = self.verification_lock.lock().await;
        {
            let state = self.state.lock().await;
            if state.verified_models.contains(&artifact.entry.id)
                && state.verified_runtimes.contains(&family)
            {
                return Ok(());
            }
        }
        let cancel = CancellationToken::new();
        let model = model_path(&self.root, artifact);
        if file_len(&model).await != artifact.size
            || hash_file(&model, &cancel)
                .await
                .map_err(|error| {
                    AppError::ProviderUnavailable(format!(
                        "Could not verify local ASR model: {}",
                        error.safe_message
                    ))
                })?
                != artifact.sha256
        {
            self.invalidate_model(
                &artifact.entry.id,
                LocalModelErrorKind::ChecksumMismatch,
                "Local ASR model integrity verification failed. Reinstall it.",
            )
            .await;
            return Err(AppError::ProviderUnavailable(
                "The local ASR model failed integrity verification".into(),
            ));
        }
        if let Some(auxiliary) = artifact.auxiliary {
            let path = auxiliary_path(&self.root, &auxiliary);
            if file_len(&path).await != auxiliary.size
                || hash_file(&path, &cancel)
                    .await
                    .map_err(|error| {
                        AppError::ProviderUnavailable(format!(
                            "Could not verify local ASR auxiliary model: {}",
                            error.safe_message
                        ))
                    })?
                    != auxiliary.sha256
            {
                self.invalidate_model(
                    &artifact.entry.id,
                    LocalModelErrorKind::ChecksumMismatch,
                    "Local ASR auxiliary model integrity verification failed. Reinstall it.",
                )
                .await;
                return Err(AppError::ProviderUnavailable(
                    "The local ASR auxiliary model failed integrity verification".into(),
                ));
            }
        }
        if configured_runtime_path(family).is_none() {
            let runtime = self.runtime_artifact(family).ok_or_else(|| {
                AppError::ProviderUnavailable("Local ASR runtime is unavailable".into())
            })?;
            if find_installed_runtime_executable(&self.root, runtime).is_err() {
                self.invalidate_runtime(
                    family,
                    LocalModelErrorKind::RuntimeUnavailable,
                    "Local ASR runtime is missing. Retry installation to repair it.",
                )
                .await;
                return Err(AppError::ProviderUnavailable(
                    "The local ASR runtime is missing".into(),
                ));
            }
        }
        let mut state = self.state.lock().await;
        state.verified_models.insert(artifact.entry.id.clone());
        state.verified_runtimes.insert(family);
        Ok(())
    }

    async fn invalidate_model(
        &self,
        model_id: &str,
        kind: LocalModelErrorKind,
        message: &str,
    ) {
        let mut state = self.state.lock().await;
        state
            .persisted
            .installed_model_ids
            .retain(|id| id != model_id);
        if state.persisted.active_model_id.as_deref() == Some(model_id) {
            state.persisted.active_model_id = None;
        }
        if let Some(model) = state.models.get_mut(model_id) {
            model.install_phase = LocalModelInstallPhase::Failed;
            model.error_kind = Some(kind);
            model.message = Some(message.to_owned());
        }
        state.verified_models.remove(model_id);
        state.last_error = Some(message.to_owned());
        drop(state);
        let _ = self.save_state().await;
    }

    async fn invalidate_runtime(
        &self,
        family: RuntimeFamily,
        kind: LocalModelErrorKind,
        message: &str,
    ) {
        let mut state = self.state.lock().await;
        state.present_runtimes.remove(&family);
        state.verified_runtimes.remove(&family);
        let active = state.persisted.active_model_id.clone();
        if let Some(model_id) = active
            && self
                .catalog
                .iter()
                .find(|artifact| artifact.entry.id == model_id)
                .is_some_and(|artifact| artifact.engine.runtime_family() == family)
            && let Some(model) = state.models.get_mut(&model_id)
        {
            model.install_phase = LocalModelInstallPhase::Failed;
            model.error_kind = Some(kind);
            model.message = Some(message.to_owned());
        }
        state.last_error = Some(message.to_owned());
    }

    fn runtime_executable(&self, artifact: &AsrModelArtifact) -> Result<PathBuf, AppError> {
        let family = artifact.engine.runtime_family();
        if let Some(path) = configured_runtime_path(family) {
            return Ok(path);
        }
        let runtime = self.runtime_artifact(family).ok_or_else(|| {
            AppError::ProviderUnavailable("Local ASR runtime is unavailable".into())
        })?;
        find_installed_runtime_executable(&self.root, runtime).map_err(|error| {
            AppError::ProviderUnavailable(format!("Local ASR runtime is unavailable: {error}"))
        })
    }

    async fn ensure_disk_space(&self, artifact: &AsrModelArtifact) -> Result<(), AsrFailure> {
        let model_remaining = artifact
            .size
            .saturating_sub(file_len(&partial_model_path(&self.root, artifact)).await);
        let auxiliary_remaining = if let Some(auxiliary) = artifact.auxiliary {
            auxiliary.size.saturating_sub(
                file_len(&partial_auxiliary_path(&self.root, &auxiliary)).await,
            )
        } else {
            0
        };
        let family = artifact.engine.runtime_family();
        let runtime_remaining = if configured_runtime_path(family).is_some() {
            0
        } else {
            self.runtime_artifact(family).map_or(0, |runtime| {
                runtime
                    .size
                    .saturating_sub(
                        std::fs::metadata(partial_runtime_path(&self.root, runtime))
                            .map_or(0, |metadata| metadata.len()),
                    )
            })
        };
        let required = model_remaining
            .saturating_add(auxiliary_remaining)
            .saturating_add(runtime_remaining)
            .saturating_add(RUNTIME_EXTRACT_RESERVE_BYTES)
            .saturating_add(DISK_SAFETY_BYTES);
        let root = asr_root(&self.root);
        let available = tokio::task::spawn_blocking(move || fs2::available_space(root))
            .await
            .map_err(|error| {
                AsrFailure::new(
                    LocalModelErrorKind::Unknown,
                    "Could not inspect storage for the ASR model.",
                    error.to_string(),
                )
            })?
            .map_err(|error| {
                AsrFailure::new(
                    LocalModelErrorKind::Unknown,
                    "Could not inspect storage for the ASR model.",
                    error.to_string(),
                )
            })?;
        if available < required {
            return Err(AsrFailure::new(
                LocalModelErrorKind::InsufficientSpace,
                "There is not enough free space to install the ASR model.",
                format!("required {required}, available {available}"),
            ));
        }
        Ok(())
    }

    async fn save_state(&self) -> Result<(), AppError> {
        let _persist = self.persist_lock.lock().await;
        let bytes = {
            let state = self.state.lock().await;
            serde_json::to_vec_pretty(&state.persisted)
                .map_err(|error| AppError::Internal(format!("serialize ASR state: {error}")))?
        };
        let path = state_path(&self.root);
        let temp = state_temp_path(&self.root);
        prepare_managed_file(&self.root, &path)
            .and_then(|_| prepare_managed_file(&self.root, &temp))
            .map_err(|error| AppError::Internal(format!("validate ASR state path: {error}")))?;
        tokio::fs::write(&temp, bytes)
            .await
            .map_err(|error| AppError::Internal(format!("write ASR state: {error}")))?;
        match tokio::fs::rename(&temp, &path).await {
            Ok(()) => Ok(()),
            Err(_error) if cfg!(windows) && path.exists() => {
                tokio::fs::remove_file(&path)
                    .await
                    .map_err(|remove| AppError::Internal(format!("replace ASR state: {remove}")))?;
                tokio::fs::rename(&temp, &path)
                    .await
                    .map_err(|rename| AppError::Internal(format!("commit ASR state: {rename}")))
            }
            Err(error) => Err(AppError::Internal(format!("commit ASR state: {error}"))),
        }
    }
}

/// Curated metadata without constructing the ASR service or creating files.
pub fn asr_model_catalog() -> Vec<AsrModelCatalogEntry> {
    built_in_catalog()
        .into_iter()
        .map(|artifact| artifact.entry)
        .collect()
}

/// Side-effect-free status for a fresh installation.
pub fn inactive_asr_model_status() -> AsrModelServiceStatus {
    let runtimes = production_runtime_artifacts();
    let catalog = built_in_catalog();
    let supported = catalog.iter().any(|artifact| {
        runtime_supported(artifact.engine.runtime_family(), &runtimes)
    });
    AsrModelServiceStatus {
        protocol_version: PROTOCOL_VERSION.into(),
        enabled: false,
        ready: false,
        active_model_id: None,
        runtime: LocalRuntimeStatus {
            version: supported.then(|| "multi-engine".into()),
            backend: runtimes.first().map(|runtime| runtime.backend),
            phase: LocalModelRuntimePhase::Stopped,
            error_kind: (!supported).then_some(LocalModelErrorKind::UnsupportedPlatform),
            message: (!supported).then(|| {
                "Local speech recognition is not available on this platform.".into()
            }),
        },
        models: asr_model_catalog()
            .into_iter()
            .map(|entry| LocalModelState {
                model_id: entry.id,
                install_phase: if supported {
                    LocalModelInstallPhase::NotInstalled
                } else {
                    LocalModelInstallPhase::Failed
                },
                progress: None,
                installed_bytes: 0,
                runtime_phase: LocalModelRuntimePhase::Stopped,
                error_kind: (!supported).then_some(LocalModelErrorKind::UnsupportedPlatform),
                message: None,
            })
            .collect(),
        last_error: None,
    }
}

fn snapshot(
    state: &MutableState,
    catalog: &[AsrModelArtifact],
    runtimes: &[RuntimeArtifact],
) -> AsrModelServiceStatus {
    let active = state.persisted.active_model_id.clone();
    let active_artifact = active
        .as_ref()
        .and_then(|id| catalog.iter().find(|artifact| &artifact.entry.id == id));
    let supported = active_artifact.map_or_else(
        || {
            catalog.iter().any(|artifact| {
                runtime_supported(artifact.engine.runtime_family(), runtimes)
            })
        },
        |artifact| runtime_supported(artifact.engine.runtime_family(), runtimes),
    );
    let ready = active_artifact.is_some_and(|artifact| {
        state
            .models
            .get(&artifact.entry.id)
            .is_some_and(|model| model.install_phase == LocalModelInstallPhase::Installed)
            && state
                .present_runtimes
                .contains(&artifact.engine.runtime_family())
    });
    let runtime_version = active_artifact
        .and_then(|artifact| {
            runtimes
                .iter()
                .find(|runtime| runtime.family == artifact.engine.runtime_family())
                .map(|runtime| runtime.version.to_owned())
        })
        .or_else(|| supported.then(|| "multi-engine".into()));
    AsrModelServiceStatus {
        protocol_version: PROTOCOL_VERSION.into(),
        enabled: active.is_some(),
        ready,
        active_model_id: active,
        runtime: LocalRuntimeStatus {
            version: runtime_version,
            backend: active_artifact
                .and_then(|artifact| {
                    runtimes
                        .iter()
                        .find(|runtime| {
                            runtime.family == artifact.engine.runtime_family()
                        })
                        .map(|runtime| runtime.backend)
                })
                .or(supported.then_some(LocalModelRuntimeBackend::Cpu)),
            // The CLI is deliberately one-shot, so it is "ready" when its
            // verified artifacts can be launched and otherwise stopped.
            phase: if ready {
                LocalModelRuntimePhase::Ready
            } else {
                LocalModelRuntimePhase::Stopped
            },
            error_kind: (!supported).then_some(LocalModelErrorKind::UnsupportedPlatform),
            message: (!supported).then(|| {
                "Local speech recognition is not available on this platform.".into()
            }),
        },
        models: catalog
            .iter()
            .filter_map(|artifact| state.models.get(&artifact.entry.id).cloned())
            .collect(),
        last_error: state.last_error.clone(),
    }
}

fn built_in_catalog() -> Vec<AsrModelArtifact> {
    let runtimes = production_runtime_artifacts();
    let whisper_runtime_size = runtimes
        .iter()
        .find(|runtime| runtime.family == RuntimeFamily::WhisperCpp)
        .map_or(0, |runtime| runtime.size);
    let funasr_runtime_size = runtimes
        .iter()
        .find(|runtime| runtime.family == RuntimeFamily::FunAsrLlamaCpp)
        .map_or(0, |runtime| runtime.size);
    let vad = AsrAuxiliaryArtifact {
        id: "funasr-fsmn-vad",
        file_name: "fsmn-vad.gguf",
        url: "https://huggingface.co/FunAudioLLM/fsmn-vad-GGUF/resolve/6840bae4c5c92ee8c04faaf4db23dd0105098d7f/fsmn-vad.gguf",
        sha256: "1270f2559c495f4e7b6e739541151027d360761a3fda43fc147034f5719f5479",
        size: 1_720_512,
    };
    vec![
        AsrModelArtifact {
            entry: AsrModelCatalogEntry {
                id: "funasr-paraformer-zh-q8".into(),
                name: "Paraformer 中文 Q8".into(),
                description:
                    "Chinese-first offline speech recognition with fast CPU inference and built-in long-audio VAD."
                        .into(),
                model_size: "Paraformer-zh".into(),
                quantization: "Q8".into(),
                download_size_bytes: 236_929_024 + vad.size + funasr_runtime_size,
                required_memory_bytes: 900_000_000,
                languages: vec!["zh".into(), "en".into(), "中英混说".into()],
                license: "Apache-2.0".into(),
                source: "FunAudioLLM Paraformer / FunASR llama.cpp".into(),
                recommended: true,
                engine: AsrEngine::FunAsrLlamaCpp,
                capabilities: vec![
                    AsrCapability::Transcription,
                    AsrCapability::LongAudioVad,
                ],
            },
            engine: AsrEngineKind::FunAsrParaformer,
            file_name: "paraformer-q8.gguf",
            url: "https://huggingface.co/FunAudioLLM/Paraformer-GGUF/resolve/de2cbaaa0f30b34f398d7a066fdfefb8e50d902c/paraformer-q8.gguf",
            sha256: "42bf76ea1575a336aaca4c1b7c01a82b79113e6d04d0d6b799561bfcf07ee011",
            size: 236_929_024,
            auxiliary: Some(vad),
        },
        AsrModelArtifact {
            entry: AsrModelCatalogEntry {
                id: "whisper-small-q5-1".into(),
                name: "Whisper Small (Q5_1)".into(),
                description:
                    "Recommended multilingual speech recognition with a strong speed/accuracy balance for Chinese and English."
                        .into(),
                model_size: "244M".into(),
                quantization: "Q5_1".into(),
                download_size_bytes: 190_085_487 + whisper_runtime_size,
                required_memory_bytes: 1_200_000_000,
                languages: vec!["zh".into(), "en".into(), "multilingual".into()],
                license: "MIT".into(),
                source: "OpenAI Whisper / whisper.cpp".into(),
                recommended: false,
                engine: AsrEngine::WhisperCpp,
                capabilities: vec![
                    AsrCapability::Transcription,
                    AsrCapability::LanguageDetection,
                ],
            },
            engine: AsrEngineKind::WhisperCpp,
            file_name: "ggml-small-q5_1.bin",
            url: "https://huggingface.co/ggerganov/whisper.cpp/resolve/5359861c739e955e79d9a303bcbc70fb988958b1/ggml-small-q5_1.bin",
            sha256: "ae85e4a935d7a567bd102fe55afc16bb595bdb618e11b2fc7591bc08120411bb",
            size: 190_085_487,
            auxiliary: None,
        },
        AsrModelArtifact {
            entry: AsrModelCatalogEntry {
                id: "whisper-large-v3-turbo-q5-0".into(),
                name: "Whisper Large v3 Turbo (Q5_0)".into(),
                description:
                    "Higher-accuracy multilingual transcription for Chinese, English and mixed-language speech."
                        .into(),
                model_size: "809M".into(),
                quantization: "Q5_0".into(),
                download_size_bytes: 574_041_195 + whisper_runtime_size,
                required_memory_bytes: 2_400_000_000,
                languages: vec!["zh".into(), "en".into(), "multilingual".into()],
                license: "MIT".into(),
                source: "OpenAI Whisper large-v3-turbo / whisper.cpp".into(),
                recommended: false,
                engine: AsrEngine::WhisperCpp,
                capabilities: vec![
                    AsrCapability::Transcription,
                    AsrCapability::LanguageDetection,
                ],
            },
            engine: AsrEngineKind::WhisperCpp,
            file_name: "ggml-large-v3-turbo-q5_0.bin",
            url: "https://huggingface.co/ggerganov/whisper.cpp/resolve/5359861c739e955e79d9a303bcbc70fb988958b1/ggml-large-v3-turbo-q5_0.bin",
            sha256: "394221709cd5ad1f40c46e6031ca61bce88931e6e088c188294c6d5a55ffa7e2",
            size: 574_041_195,
            auxiliary: None,
        },
    ]
}

fn production_runtime_artifacts() -> Vec<RuntimeArtifact> {
    match (std::env::consts::OS, std::env::consts::ARCH) {
        ("windows", "x86_64") => vec![
            RuntimeArtifact {
                family: RuntimeFamily::WhisperCpp,
                version: WHISPER_CPP_VERSION,
                executable: "whisper-cli.exe",
                file_name: "whisper-bin-x64-v1.9.1.zip",
                url: "https://github.com/ggml-org/whisper.cpp/releases/download/v1.9.1/whisper-bin-x64.zip",
                sha256: "7d8be46ecd31828e1eb7a2ecdd0d6b314feafd82163038ab6092594b0a063539",
                size: 7_982_101,
                backend: LocalModelRuntimeBackend::Cpu,
            },
            RuntimeArtifact {
                family: RuntimeFamily::FunAsrLlamaCpp,
                version: FUNASR_LLAMACPP_VERSION,
                executable: "llama-funasr-paraformer.exe",
                file_name: "funasr-llamacpp-windows-x64-v0.1.4.zip",
                url: "https://github.com/modelscope/FunASR/releases/download/runtime-llamacpp-v0.1.4/funasr-llamacpp-windows-x64.zip",
                sha256: "ae0bca37e046dcd0e59ac3399f2ed246abf0696a84dc1f4322adc894bb5339e7",
                size: 4_663_344,
                backend: LocalModelRuntimeBackend::Cpu,
            },
        ],
        _ => Vec::new(),
    }
}

fn configured_runtime_path(family: RuntimeFamily) -> Option<PathBuf> {
    let variable = match family {
        RuntimeFamily::WhisperCpp => "NOMIFUN_WHISPER_CLI_PATH",
        RuntimeFamily::FunAsrLlamaCpp => "NOMIFUN_FUNASR_PARAFORMER_CLI_PATH",
    };
    let path = PathBuf::from(std::env::var_os(variable)?);
    path.is_file().then_some(path)
}

fn runtime_supported(family: RuntimeFamily, runtimes: &[RuntimeArtifact]) -> bool {
    configured_runtime_path(family).is_some()
        || runtimes.iter().any(|runtime| runtime.family == family)
}

fn runtime_is_present(
    root: &Path,
    family: RuntimeFamily,
    runtimes: &[RuntimeArtifact],
) -> bool {
    if configured_runtime_path(family).is_some() {
        return true;
    }
    runtimes
        .iter()
        .find(|runtime| runtime.family == family)
        .is_some_and(|runtime| {
            find_installed_runtime_executable(root, runtime).is_ok()
        })
}

async fn auxiliary_is_present(root: &Path, artifact: &AsrModelArtifact) -> bool {
    match artifact.auxiliary {
        Some(auxiliary) => file_len(&auxiliary_path(root, &auxiliary)).await == auxiliary.size,
        None => true,
    }
}

fn artifact_total_size(artifact: &AsrModelArtifact) -> u64 {
    artifact
        .size
        .saturating_add(artifact.auxiliary.map_or(0, |auxiliary| auxiliary.size))
}

async fn load_state(root: &Path) -> PersistedState {
    let path = state_path(root);
    let Ok(bytes) = tokio::fs::read(path).await else {
        return PersistedState::default();
    };
    let Some(mut state) = serde_json::from_slice::<PersistedState>(&bytes).ok() else {
        return PersistedState::default();
    };
    if state.version == 1 {
        state.version = STATE_VERSION;
        return state;
    }
    (state.version == STATE_VERSION)
        .then_some(state)
        .unwrap_or_default()
}

fn asr_root(root: &Path) -> PathBuf {
    root.join(ASR_DIR)
}

fn models_dir(root: &Path) -> PathBuf {
    asr_root(root).join(MODELS_DIR)
}

fn downloads_dir(root: &Path) -> PathBuf {
    asr_root(root).join(DOWNLOADS_DIR)
}

fn runtime_root(root: &Path) -> PathBuf {
    asr_root(root).join(RUNTIME_DIR)
}

fn runtime_family_dir_name(family: RuntimeFamily) -> &'static str {
    match family {
        RuntimeFamily::WhisperCpp => "whisper-cpp",
        RuntimeFamily::FunAsrLlamaCpp => "funasr-llamacpp",
    }
}

fn runtime_install_dir(root: &Path, runtime: &RuntimeArtifact) -> PathBuf {
    runtime_root(root)
        .join(runtime_family_dir_name(runtime.family))
        .join(runtime.version)
        .join(format!("{}-{}", std::env::consts::OS, std::env::consts::ARCH))
}

fn legacy_whisper_runtime_install_dir(root: &Path) -> PathBuf {
    runtime_root(root).join(format!(
        "{}-{}-{WHISPER_CPP_VERSION}",
        std::env::consts::OS,
        std::env::consts::ARCH
    ))
}

fn find_installed_runtime_executable(
    root: &Path,
    runtime: &RuntimeArtifact,
) -> std::io::Result<PathBuf> {
    find_runtime_executable(
        &runtime_install_dir(root, runtime),
        runtime.executable,
    )
    .or_else(|primary_error| {
        if runtime.family == RuntimeFamily::WhisperCpp {
            find_runtime_executable(
                &legacy_whisper_runtime_install_dir(root),
                runtime.executable,
            )
            .map_err(|_| primary_error)
        } else {
            Err(primary_error)
        }
    })
}

fn runtime_staging_dir(root: &Path, runtime: &RuntimeArtifact) -> PathBuf {
    runtime_root(root).join(format!(
        ".extracting-{}-{}-{}-{}",
        runtime_family_dir_name(runtime.family),
        std::env::consts::OS,
        std::env::consts::ARCH,
        runtime.version
    ))
}

fn jobs_dir(root: &Path) -> PathBuf {
    asr_root(root).join(JOBS_DIR)
}

fn model_path(root: &Path, artifact: &AsrModelArtifact) -> PathBuf {
    models_dir(root)
        .join(&artifact.entry.id)
        .join(artifact.file_name)
}

fn partial_model_path(root: &Path, artifact: &AsrModelArtifact) -> PathBuf {
    downloads_dir(root).join(format!("{}.part", artifact.entry.id))
}

fn auxiliary_path(root: &Path, auxiliary: &AsrAuxiliaryArtifact) -> PathBuf {
    models_dir(root)
        .join("shared")
        .join(auxiliary.id)
        .join(auxiliary.file_name)
}

fn partial_auxiliary_path(root: &Path, auxiliary: &AsrAuxiliaryArtifact) -> PathBuf {
    downloads_dir(root).join(format!("{}.part", auxiliary.id))
}

fn runtime_archive_path(root: &Path, runtime: &RuntimeArtifact) -> PathBuf {
    downloads_dir(root).join(runtime.file_name)
}

fn partial_runtime_path(root: &Path, runtime: &RuntimeArtifact) -> PathBuf {
    downloads_dir(root).join(format!(
        "runtime-{}-{}.part",
        runtime_family_dir_name(runtime.family),
        runtime.version
    ))
}

fn state_path(root: &Path) -> PathBuf {
    asr_root(root).join(STATE_FILE)
}

fn state_temp_path(root: &Path) -> PathBuf {
    asr_root(root).join(format!("{STATE_FILE}.tmp"))
}

fn prepare_layout(
    root: &Path,
    catalog: &[AsrModelArtifact],
    runtimes: &[RuntimeArtifact],
) -> std::io::Result<()> {
    for directory in [
        root.to_path_buf(),
        asr_root(root),
        models_dir(root),
        downloads_dir(root),
        runtime_root(root),
        jobs_dir(root),
    ] {
        prepare_managed_directory(root, &directory)?;
    }
    for artifact in catalog {
        prepare_managed_file(root, &model_path(root, artifact))?;
        prepare_managed_file(root, &partial_model_path(root, artifact))?;
        if let Some(auxiliary) = artifact.auxiliary {
            prepare_managed_file(root, &auxiliary_path(root, &auxiliary))?;
            prepare_managed_file(root, &partial_auxiliary_path(root, &auxiliary))?;
        }
    }
    for runtime in runtimes {
        prepare_managed_file(root, &runtime_archive_path(root, runtime))?;
        prepare_managed_file(root, &partial_runtime_path(root, runtime))?;
    }
    prepare_managed_file(root, &state_path(root))?;
    prepare_managed_file(root, &state_temp_path(root))?;
    Ok(())
}

fn random_job_id() -> Result<String, AppError> {
    let mut bytes = [0_u8; 16];
    getrandom::getrandom(&mut bytes)
        .map_err(|error| AppError::Internal(format!("create ASR job identifier: {error}")))?;
    Ok(hex::encode(bytes))
}

fn normalize_language_hint(language: Option<&str>) -> Option<String> {
    let value = language?.trim().replace('_', "-").to_ascii_lowercase();
    let primary = value.split('-').next().unwrap_or_default();
    if primary.len() == 2 && primary.bytes().all(|byte| byte.is_ascii_lowercase()) {
        Some(primary.to_owned())
    } else {
        None
    }
}

fn safe_audio_extension(file_name: &str, mime_type: &str) -> Option<&'static str> {
    let extension = Path::new(file_name)
        .extension()
        .and_then(OsStr::to_str)
        .map(str::to_ascii_lowercase);
    let mime_type = mime_type
        .split(';')
        .next()
        .unwrap_or_default()
        .trim()
        .to_ascii_lowercase();
    match (extension.as_deref(), mime_type.as_str()) {
        (Some("wav"), _) | (_, "audio/wav" | "audio/wave" | "audio/x-wav") => Some("wav"),
        (Some("mp3"), _) | (_, "audio/mpeg" | "audio/mp3") => Some("mp3"),
        (Some("ogg" | "oga" | "opus"), _) | (_, "audio/ogg" | "audio/opus") => Some("ogg"),
        (Some("flac"), _) | (_, "audio/flac" | "audio/x-flac") => Some("flac"),
        _ => None,
    }
}

fn sanitize_process_output(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes)
        .chars()
        .filter(|character| !character.is_control() || matches!(character, '\n' | '\t'))
        .take(1_000)
        .collect()
}

fn parse_whisper_json(raw: &[u8]) -> Result<String, AppError> {
    let value: serde_json::Value = serde_json::from_slice(raw).map_err(|error| {
        AppError::ProviderUnavailable(format!(
            "Local ASR runtime returned an invalid result: {error}"
        ))
    })?;
    Ok(value
        .get("transcription")
        .and_then(serde_json::Value::as_array)
        .map(|segments| {
            segments
                .iter()
                .filter_map(|segment| segment.get("text").and_then(serde_json::Value::as_str))
                .collect::<Vec<_>>()
                .join("")
        })
        .or_else(|| {
            value
                .get("text")
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned)
        })
        .unwrap_or_default()
        .trim()
        .to_owned())
}

fn parse_funasr_stdout(stdout: &[u8]) -> String {
    let text = decode_process_text(stdout);
    text
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .filter(|line| {
            !line.starts_with("usage:")
                && !line.starts_with("system_info:")
                && !line.starts_with("main:")
                && !line.starts_with("load:")
                && !line.starts_with("vad:")
                && !line.starts_with("ggml_")
                && !line.starts_with("llama_")
        })
        .collect::<Vec<_>>()
        .join("\n")
        .trim()
        .to_owned()
}

fn decode_process_text(bytes: &[u8]) -> String {
    if let Ok(text) = std::str::from_utf8(bytes) {
        return text.to_owned();
    }
    #[cfg(windows)]
    {
        let (text, _, _) = encoding_rs::GBK.decode(bytes);
        return text.into_owned();
    }
    #[cfg(not(windows))]
    {
        String::from_utf8_lossy(bytes).into_owned()
    }
}

fn asr_download_client() -> reqwest::Client {
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
            warn!(error = %error, "could not apply system proxy to ASR downloader");
            build()
                .build()
                .expect("ASR HTTP client configuration is valid")
        })
}

fn allowed_download_url(url: &reqwest::Url) -> bool {
    if url.scheme() != "https" {
        return false;
    }
    let Some(host) = url.host_str().map(str::to_ascii_lowercase) else {
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

fn parse_content_range(value: &str) -> Option<(u64, u64, u64)> {
    let value = value.strip_prefix("bytes ")?;
    let (range, total) = value.split_once('/')?;
    let (start, end) = range.split_once('-')?;
    let start = start.parse::<u64>().ok()?;
    let end = end.parse::<u64>().ok()?;
    let total = total.parse::<u64>().ok()?;
    (start <= end && end < total).then_some((start, end, total))
}

async fn hash_file(path: &Path, cancel: &CancellationToken) -> Result<String, AsrFailure> {
    let mut file = tokio::fs::File::open(path).await.map_err(|error| {
        AsrFailure::new(
            LocalModelErrorKind::Unknown,
            "Could not verify the ASR model file.",
            error.to_string(),
        )
    })?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0_u8; 1024 * 1024];
    loop {
        let count = tokio::select! {
            _ = cancel.cancelled() => return Err(AsrFailure::cancelled()),
            result = file.read(&mut buffer) => result.map_err(|error| AsrFailure::new(
                LocalModelErrorKind::Unknown,
                "Could not verify the ASR model file.",
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

async fn commit_partial(
    root: &Path,
    partial: &Path,
    destination: &Path,
) -> Result<(), AsrFailure> {
    prepare_managed_file(root, partial).map_err(storage_failure)?;
    prepare_managed_file(root, destination).map_err(storage_failure)?;
    remove_file_if_exists_failure(root, destination).await?;
    tokio::fs::rename(partial, destination).await.map_err(|error| {
        AsrFailure::new(
            LocalModelErrorKind::Unknown,
            "Could not complete the ASR model installation.",
            error.to_string(),
        )
    })
}

async fn remove_file_if_exists(root: &Path, path: &Path) -> Result<(), AppError> {
    prepare_managed_file(root, path)
        .map_err(|error| AppError::Internal(format!("validate ASR file path: {error}")))?;
    match tokio::fs::remove_file(path).await {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(AppError::Internal(format!("remove ASR model file: {error}"))),
    }
}

async fn remove_file_if_exists_failure(
    root: &Path,
    path: &Path,
) -> Result<(), AsrFailure> {
    prepare_managed_file(root, path).map_err(storage_failure)?;
    match tokio::fs::remove_file(path).await {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(storage_failure(error)),
    }
}

fn storage_failure(error: std::io::Error) -> AsrFailure {
    AsrFailure::new(
        LocalModelErrorKind::Unknown,
        "ASR model storage did not pass safety checks.",
        error.to_string(),
    )
}

fn find_runtime_executable(root: &Path, target: &str) -> std::io::Result<PathBuf> {
    let mut found = None;
    let mut stack = vec![(root.to_path_buf(), 0_usize)];
    while let Some((directory, depth)) = stack.pop() {
        if depth > 5 {
            continue;
        }
        for entry in std::fs::read_dir(directory)? {
            let entry = entry?;
            let file_type = entry.file_type()?;
            if file_type.is_symlink() {
                continue;
            }
            let path = entry.path();
            if file_type.is_file() && entry.file_name() == target {
                if found.replace(path).is_some() {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        "runtime contains multiple matching ASR executables",
                    ));
                }
            } else if file_type.is_dir() {
                stack.push((path, depth + 1));
            }
        }
    }
    found.ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "runtime does not contain the expected ASR executable",
        )
    })
}

fn extract_runtime_zip(archive_path: &Path, destination: &Path) -> std::io::Result<()> {
    let file = std::fs::File::open(archive_path)?;
    let mut archive = zip::ZipArchive::new(file)
        .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))?;
    if archive.len() > MAX_ARCHIVE_ENTRIES {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "ASR runtime archive contains too many entries",
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
                "unsafe path in ASR runtime archive",
            )
        })?;
        if relative.as_os_str().is_empty() || !safe_relative_path(&relative) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "unsafe path in ASR runtime archive",
            ));
        }
        let relative = relative.to_path_buf();
        if !seen.insert(relative.clone()) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "duplicate path in ASR runtime archive",
            ));
        }
        if let Some(mode) = entry.unix_mode() {
            let kind = mode & 0o170000;
            if kind != 0 && kind != 0o100000 && kind != 0o040000 {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "links and special files are not allowed in ASR runtime archive",
                ));
            }
        }
        expanded = expanded.checked_add(entry.size()).ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "ASR runtime archive size overflow",
            )
        })?;
        if expanded > MAX_ARCHIVE_EXPANDED_BYTES {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "ASR runtime archive expands beyond the allowed limit",
            ));
        }
        let output = destination.join(relative);
        if entry.is_dir() {
            std::fs::create_dir_all(output)?;
            continue;
        }
        if let Some(parent) = output.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mut output_file = std::fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(output)?;
        std::io::copy(&mut entry, &mut output_file)?;
        output_file.sync_all()?;
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
            "managed ASR directory escaped local AI root",
        )
    })?;
    if !safe_relative_path(relative) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "managed ASR directory has an unsafe relative path",
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
                        "managed ASR ancestor is a link or not a directory",
                    ));
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                std::fs::create_dir(&current)?;
            }
            Err(error) => return Err(error),
        }
    }
    if !std::fs::canonicalize(&current)?.starts_with(canonical_root) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "managed ASR directory resolved outside its root",
        ));
    }
    Ok(())
}

fn prepare_managed_file(root: &Path, path: &Path) -> std::io::Result<()> {
    let relative = path.strip_prefix(root).map_err(|_| {
        std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "managed ASR file escaped local AI root",
        )
    })?;
    if relative.as_os_str().is_empty() || !safe_relative_path(relative) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "managed ASR file has an unsafe relative path",
        ));
    }
    let parent = path.parent().ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "managed ASR file has no parent",
        )
    })?;
    prepare_managed_directory(root, parent)?;
    match std::fs::symlink_metadata(path) {
        Ok(metadata) => {
            if unsafe_link_or_reparse(&metadata) || !metadata.is_file() {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::PermissionDenied,
                    "managed ASR target is a link or not a regular file",
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
            "managed ASR tree escaped local AI root",
        )
    })?;
    if relative.as_os_str().is_empty() || !safe_relative_path(relative) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "refusing to remove unsafe ASR tree",
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
            "managed ASR tree is a link or not a directory",
        ));
    }
    let canonical_root = std::fs::canonicalize(root)?;
    let canonical_path = std::fs::canonicalize(path)?;
    if canonical_path == canonical_root || !canonical_path.starts_with(&canonical_root) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "managed ASR tree resolved outside its root",
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
                    "managed ASR tree contains too many entries",
                ));
            }
            let path = entry.path();
            let metadata = std::fs::symlink_metadata(&path)?;
            if unsafe_link_or_reparse(&metadata) {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::PermissionDenied,
                    "managed ASR tree contains a link or reparse point",
                ));
            }
            if !std::fs::canonicalize(&path)?.starts_with(canonical_root) {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::PermissionDenied,
                    "managed ASR tree entry escaped its root",
                ));
            }
            if metadata.is_dir() {
                stack.push(path);
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::TempDir;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};
    use zip::write::SimpleFileOptions;

    #[test]
    fn catalog_contains_recommended_multilingual_model() {
        let catalog = asr_model_catalog();
        let recommended = catalog.iter().find(|model| model.recommended).unwrap();
        assert!(recommended.languages.contains(&"zh".to_owned()));
        assert!(recommended.languages.contains(&"en".to_owned()));
        assert_eq!(recommended.id, "funasr-paraformer-zh-q8");
        assert_eq!(recommended.engine, AsrEngine::FunAsrLlamaCpp);
        assert!(
            recommended
                .capabilities
                .contains(&AsrCapability::LongAudioVad)
        );
        assert!(recommended.download_size_bytes >= 236_929_024);
    }

    #[test]
    fn funasr_stdout_parser_keeps_transcript_and_drops_known_runtime_logs() {
        let output = r#"
system_info: n_threads = 8
ggml_backend_load_all: loaded CPU backend
欢迎大家来体验达摩院推出的语音识别模型。
"#;
        assert_eq!(
            parse_funasr_stdout(output.as_bytes()),
            "欢迎大家来体验达摩院推出的语音识别模型。"
        );
    }

    #[cfg(windows)]
    #[test]
    fn funasr_stdout_parser_accepts_windows_gbk_console_output() {
        let (encoded, _, _) = encoding_rs::GBK.encode("中文语音识别");
        assert_eq!(parse_funasr_stdout(&encoded), "中文语音识别");
    }

    #[test]
    fn language_hint_is_reduced_to_safe_iso_primary_tag() {
        assert_eq!(normalize_language_hint(Some("zh-CN")), Some("zh".into()));
        assert_eq!(normalize_language_hint(Some("en_US")), Some("en".into()));
        assert_eq!(normalize_language_hint(Some("--model")), None);
    }

    #[test]
    fn audio_extension_is_allowlisted() {
        assert_eq!(
            safe_audio_extension("voice.MP3", "application/octet-stream"),
            Some("mp3")
        );
        assert_eq!(
            safe_audio_extension("voice.anything", "audio/ogg; codecs=opus"),
            Some("ogg")
        );
        assert_eq!(
            safe_audio_extension("voice.anything", "Audio/WAV; charset=binary"),
            Some("wav")
        );
        assert_eq!(
            safe_audio_extension("../../evil.exe", "application/octet-stream"),
            None
        );
    }

    async fn tiny_test_service(
        temp: &TempDir,
        server: &MockServer,
        model: Vec<u8>,
    ) -> Arc<AsrModelService> {
        let runtime = b"runtime".to_vec();
        Mock::given(method("GET"))
            .and(path("/runtime.zip"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(runtime.clone()))
            .mount(server)
            .await;
        Mock::given(method("GET"))
            .and(path("/model.bin"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(model.clone()))
            .mount(server)
            .await;
        let entry = AsrModelCatalogEntry {
            id: "test-asr".into(),
            name: "Test ASR".into(),
            description: "test".into(),
            model_size: "tiny".into(),
            quantization: "test".into(),
            download_size_bytes: (runtime.len() + model.len()) as u64,
            required_memory_bytes: 1,
            languages: vec!["zh".into(), "en".into()],
            license: "MIT".into(),
            source: "test".into(),
            recommended: true,
            engine: AsrEngine::WhisperCpp,
            capabilities: vec![AsrCapability::Transcription],
        };
        AsrModelService::new_inner(
            temp.path().join("local-ai"),
            reqwest::Client::new(),
            vec![AsrModelArtifact {
                entry,
                engine: AsrEngineKind::WhisperCpp,
                file_name: "model.bin",
                url: Box::leak(format!("{}/model.bin", server.uri()).into_boxed_str()),
                sha256: Box::leak(hex::encode(Sha256::digest(&model)).into_boxed_str()),
                size: model.len() as u64,
                auxiliary: None,
            }],
            vec![RuntimeArtifact {
                family: RuntimeFamily::WhisperCpp,
                version: "test",
                executable: if cfg!(windows) {
                    "whisper-cli.exe"
                } else {
                    "whisper-cli"
                },
                file_name: "runtime.zip",
                url: Box::leak(format!("{}/runtime.zip", server.uri()).into_boxed_str()),
                sha256: Box::leak(hex::encode(Sha256::digest(&runtime)).into_boxed_str()),
                size: runtime.len() as u64,
                backend: LocalModelRuntimeBackend::Cpu,
            }],
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
                    if cfg!(windows) {
                        "whisper-cli.exe"
                    } else {
                        "whisper-cli"
                    },
                    SimpleFileOptions::default().unix_permissions(0o755),
                )
                .unwrap();
            writer.write_all(b"test runtime").unwrap();
            writer.finish().unwrap();
        }
        bytes
    }

    async fn runtime_test_service(temp: &TempDir, runtime: Vec<u8>) -> Arc<AsrModelService> {
        let model = b"runtime-test-model";
        let entry = AsrModelCatalogEntry {
            id: "runtime-test-asr".into(),
            name: "Runtime Test ASR".into(),
            description: "test".into(),
            model_size: "tiny".into(),
            quantization: "test".into(),
            download_size_bytes: runtime.len() as u64,
            required_memory_bytes: 1,
            languages: vec!["zh".into(), "en".into()],
            license: "MIT".into(),
            source: "test".into(),
            recommended: true,
            engine: AsrEngine::WhisperCpp,
            capabilities: vec![AsrCapability::Transcription],
        };
        AsrModelService::new_inner(
            temp.path().join("local-ai"),
            reqwest::Client::new(),
            vec![AsrModelArtifact {
                entry,
                engine: AsrEngineKind::WhisperCpp,
                file_name: "model.bin",
                url: "http://127.0.0.1/model.bin",
                sha256: Box::leak(hex::encode(Sha256::digest(model)).into_boxed_str()),
                size: model.len() as u64,
                auxiliary: None,
            }],
            vec![RuntimeArtifact {
                family: RuntimeFamily::WhisperCpp,
                version: "test",
                executable: if cfg!(windows) {
                    "whisper-cli.exe"
                } else {
                    "whisper-cli"
                },
                file_name: "runtime.zip",
                url: "http://127.0.0.1/runtime.zip",
                sha256: Box::leak(hex::encode(Sha256::digest(&runtime)).into_boxed_str()),
                size: runtime.len() as u64,
                backend: LocalModelRuntimeBackend::Cpu,
            }],
            true,
        )
        .await
        .unwrap()
    }

    #[tokio::test]
    async fn install_completion_auto_activates_and_delete_clears_contract() {
        let temp = TempDir::new().unwrap();
        let server = MockServer::start().await;
        let model = b"tiny-model".to_vec();
        let service = tiny_test_service(&temp, &server, model.clone()).await;
        let artifact = service.artifact("test-asr").unwrap();

        // Bypass runtime extraction in this state-machine test; verified
        // artifact download/commit and lifecycle behavior are exercised.
        service
            .download_verified(
                artifact.url,
                artifact.sha256,
                artifact.size,
                &model_path(&service.root, &artifact),
                &partial_model_path(&service.root, &artifact),
                &artifact.entry.id,
                1,
                LocalModelProgressComponent::Model,
                &CancellationToken::new(),
            )
            .await
            .unwrap();
        {
            let mut state = service.state.lock().await;
            state.active_install = Some(ActiveInstall {
                model_id: artifact.entry.id.clone(),
                generation: 7,
                cancel: CancellationToken::new(),
                done: Arc::new(Notify::new()),
            });
        }
        service
            .run_install_finish_for_test(artifact.clone(), 7, Ok(()))
            .await;
        let status = service.status().await;
        assert!(status.enabled);
        assert_eq!(status.active_model_id.as_deref(), Some("test-asr"));
        assert_eq!(
            status.models[0].install_phase,
            LocalModelInstallPhase::Installed
        );

        let status = service.delete("test-asr").await.unwrap();
        assert!(!status.enabled);
        assert_eq!(status.active_model_id, None);
        assert_eq!(
            status.models[0].install_phase,
            LocalModelInstallPhase::NotInstalled
        );
    }

    #[tokio::test]
    async fn install_completion_notification_survives_late_waiter() {
        let done = Arc::new(Notify::new());
        done.notify_one();
        tokio::time::timeout(Duration::from_millis(50), done.notified())
            .await
            .expect("notify_one keeps a permit for a waiter registered later");
    }

    #[tokio::test]
    async fn saved_state_uses_current_version_and_is_restored() {
        let temp = TempDir::new().unwrap();
        let server = MockServer::start().await;
        let service = tiny_test_service(&temp, &server, b"tiny-model".to_vec()).await;
        {
            let mut state = service.state.lock().await;
            state.persisted.installed_model_ids = vec!["test-asr".into()];
            state.persisted.active_model_id = Some("test-asr".into());
        }
        service.save_state().await.unwrap();

        let path = asr_root(&temp.path().join("local-ai")).join(STATE_FILE);
        let saved: PersistedState =
            serde_json::from_slice(&tokio::fs::read(path).await.unwrap()).unwrap();
        assert_eq!(saved.version, STATE_VERSION);
        assert!(AsrModelService::opted_in(temp.path()));

        // Reloading the same catalog keeps the active identity when the
        // installed artifact is still present.
        let artifact = service.artifact("test-asr").unwrap();
        tokio::fs::write(model_path(&service.root, &artifact), b"tiny-model")
            .await
            .unwrap();
        let runtime = *service
            .runtime_artifact(RuntimeFamily::WhisperCpp)
            .unwrap();
        tokio::fs::write(runtime_archive_path(&service.root, &runtime), b"runtime")
            .await
            .unwrap();
        let reloaded = AsrModelService::new_inner(
            temp.path().join("local-ai"),
            reqwest::Client::new(),
            service.catalog.clone(),
            service.runtime_artifacts.clone(),
            true,
        )
        .await
        .unwrap();
        assert_eq!(
            reloaded.status().await.active_model_id.as_deref(),
            Some("test-asr")
        );
    }

    #[tokio::test]
    async fn version_one_state_is_migrated_without_losing_whisper_selection() {
        let temp = TempDir::new().unwrap();
        let root = temp.path().join("local-ai");
        let state_dir = asr_root(&root);
        std::fs::create_dir_all(&state_dir).unwrap();
        std::fs::write(
            state_dir.join(STATE_FILE),
            br#"{"version":1,"installedModelIds":["whisper-small-q5-1"],"activeModelId":"whisper-small-q5-1"}"#,
        )
        .unwrap();

        assert!(AsrModelService::opted_in(temp.path()));
        let state = load_state(&root).await;
        assert_eq!(state.version, STATE_VERSION);
        assert_eq!(
            state.active_model_id.as_deref(),
            Some("whisper-small-q5-1")
        );
    }

    #[tokio::test]
    async fn missing_runtime_on_restart_is_not_ready_and_install_repairs_it() {
        let temp = TempDir::new().unwrap();
        let server = MockServer::start().await;
        let service = tiny_test_service(&temp, &server, b"tiny-model".to_vec()).await;
        let artifact = service.artifact("test-asr").unwrap();
        tokio::fs::write(model_path(&service.root, &artifact), b"tiny-model")
            .await
            .unwrap();
        {
            let mut state = service.state.lock().await;
            state.persisted.installed_model_ids = vec!["test-asr".into()];
            state.persisted.active_model_id = Some("test-asr".into());
        }
        service.save_state().await.unwrap();

        let runtime = *service
            .runtime_artifact(RuntimeFamily::WhisperCpp)
            .unwrap();
        let reloaded = AsrModelService::new_inner(
            temp.path().join("local-ai"),
            reqwest::Client::new(),
            service.catalog.clone(),
            vec![runtime],
            true,
        )
        .await
        .unwrap();
        let status = reloaded.status().await;
        assert!(!status.ready);
        assert_eq!(
            status.models[0].error_kind,
            Some(LocalModelErrorKind::RuntimeUnavailable)
        );

        let install = reloaded.install("test-asr").await.unwrap();
        assert_eq!(
            install.models[0].install_phase,
            LocalModelInstallPhase::Downloading
        );
        assert!(reloaded.state.lock().await.active_install.is_some());
        reloaded.cancel("test-asr").await.unwrap();
    }

    #[tokio::test]
    async fn runtime_verification_failure_downgrades_ready_status() {
        let temp = TempDir::new().unwrap();
        let server = MockServer::start().await;
        let service = tiny_test_service(&temp, &server, b"tiny-model".to_vec()).await;
        let artifact = service.artifact("test-asr").unwrap();
        let runtime = *service
            .runtime_artifact(RuntimeFamily::WhisperCpp)
            .unwrap();
        tokio::fs::write(model_path(&service.root, &artifact), b"tiny-model")
            .await
            .unwrap();
        tokio::fs::write(runtime_archive_path(&service.root, &runtime), b"corrupt")
            .await
            .unwrap();
        {
            let mut state = service.state.lock().await;
            state.persisted.installed_model_ids = vec!["test-asr".into()];
            state.persisted.active_model_id = Some("test-asr".into());
            state.models.get_mut("test-asr").unwrap().install_phase =
                LocalModelInstallPhase::Installed;
            state.present_runtimes.insert(RuntimeFamily::WhisperCpp);
            state.verified_runtimes.remove(&RuntimeFamily::WhisperCpp);
        }

        assert!(service.verify_before_use(&artifact).await.is_err());
        let status = service.status().await;
        assert!(!status.ready);
        assert_eq!(
            status.models[0].error_kind,
            Some(LocalModelErrorKind::RuntimeUnavailable)
        );
    }

    #[tokio::test]
    async fn installed_runtime_is_not_reextracted_during_verification() {
        let temp = TempDir::new().unwrap();
        let runtime = test_runtime_zip();
        let service = runtime_test_service(&temp, runtime.clone()).await;
        let artifact = service.artifact("runtime-test-asr").unwrap();
        let runtime_artifact = *service
            .runtime_artifact(RuntimeFamily::WhisperCpp)
            .unwrap();
        tokio::fs::write(
            model_path(&service.root, &artifact),
            b"runtime-test-model",
        )
        .await
        .unwrap();
        tokio::fs::write(
            runtime_archive_path(&service.root, &runtime_artifact),
            runtime,
        )
        .await
        .unwrap();
        service
            .extract_runtime(&runtime_artifact, &CancellationToken::new())
            .await
            .unwrap();
        let executable = find_runtime_executable(
            &runtime_install_dir(&service.root, &runtime_artifact),
            runtime_artifact.executable,
        )
        .unwrap();
        tokio::fs::write(&executable, b"keep installed runtime")
            .await
            .unwrap();

        service.verify_before_use(&artifact).await.unwrap();

        assert_eq!(
            tokio::fs::read(executable).await.unwrap(),
            b"keep installed runtime"
        );
    }

    #[tokio::test]
    async fn runtime_writer_waits_for_active_transcription_reader() {
        let temp = TempDir::new().unwrap();
        let runtime = test_runtime_zip();
        let service = runtime_test_service(&temp, runtime.clone()).await;
        let runtime_artifact = *service
            .runtime_artifact(RuntimeFamily::WhisperCpp)
            .unwrap();
        tokio::fs::write(
            runtime_archive_path(&service.root, &runtime_artifact),
            runtime,
        )
        .await
        .unwrap();

        let reader = service.runtime_lifecycle.clone().read_owned().await;
        let lifecycle = service.runtime_lifecycle.clone();
        let waiter = tokio::spawn(async move { lifecycle.write_owned().await });
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert!(!waiter.is_finished());
        drop(reader);
        tokio::time::timeout(Duration::from_secs(1), waiter)
            .await
            .expect("runtime writer should proceed after transcription reader exits")
            .unwrap();
    }

    #[test]
    fn job_guard_cleans_directory_when_request_future_is_dropped() {
        let temp = TempDir::new().unwrap();
        let root = temp.path().join("local-ai");
        let job = jobs_dir(&root).join("cancelled-request");
        prepare_managed_directory(&root, &job).unwrap();
        std::fs::write(job.join("input.wav"), b"temporary audio").unwrap();

        {
            let _guard = AsrJobGuard {
                root: root.clone(),
                job_root: job.clone(),
            };
            assert!(job.is_dir());
        }

        assert!(!job.exists());
        assert!(jobs_dir(&root).is_dir());
    }
}

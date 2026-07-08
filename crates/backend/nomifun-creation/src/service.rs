//! [`CreationService`] — the generation task queue + state machine (contract §6
//! `service.rs`).
//!
//! The service owns the full lifecycle: `queued → running →
//! succeeded/failed/canceled`, a per-provider concurrency gate + a global cap,
//! synchronous and async (submit→poll) adapters, cancellation propagation, boot
//! reconciliation, and handing produced bytes to an [`AssetSink`]. Provider
//! rows are resolved (row lookup + API-key decrypt) here so the crypto/DB
//! surface stays in one place; adapters receive a [`ResolvedProvider`].

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use nomifun_common::{AppError, decrypt_string, generate_prefixed_id, now_ms};
use nomifun_db::{
    CreateCreationTaskParams, ICreationTaskRepository, IProviderRepository, ListCreationTasksParams,
    UpdateCreationTaskParams,
};
use serde_json::{Value, json};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tokio_util::sync::CancellationToken;

use crate::adapters::{MAX_ARTIFACT_BYTES, error_from_response, net_err, read_body_capped, route_adapter_id};
use crate::dto::CreationTask;
use crate::provider::{
    InputAsset, MediaProvider, PollResult, ProducedData, ResolvedProvider, SubmitAck, SubmitRequest,
};
use crate::types::{CreationError, CreationInput, MediaCapability, TaskStatus};

/// Default per-provider in-flight cap (信号量).
const DEFAULT_PER_PROVIDER_LIMIT: usize = 3;
/// Default global in-flight cap across all providers.
const DEFAULT_GLOBAL_LIMIT: usize = 10;
/// Default poll interval for async submit→poll protocols.
const DEFAULT_POLL_INTERVAL: Duration = Duration::from_millis(2500);
/// Default total budget for an async task before it is failed as `timeout`.
const DEFAULT_TASK_TIMEOUT: Duration = Duration::from_secs(600);
/// Timeout for fetching a URL-form artifact the adapter returned.
const DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(180);

/// A generation request accepted by [`CreationService::create_task`].
pub struct NewCreationTask {
    pub canvas_id: Option<String>,
    pub node_id: Option<String>,
    pub provider_id: String,
    pub model: String,
    /// Wire capability code (`t2i|i2i|…`).
    pub capability: String,
    /// Opaque parameter map (prompt/size/quality/…).
    pub params: Value,
    pub inputs: Vec<CreationInput>,
}

/// A produced artifact ready for persistence: resolved bytes (URL artifacts are
/// fetched by the engine first) + MIME + provenance.
pub struct PersistAsset {
    pub canvas_id: Option<String>,
    pub node_id: Option<String>,
    pub bytes: Vec<u8>,
    pub mime: String,
    /// Whether the produced asset appears in the asset library. Generated
    /// products default to `true` (see [`CreationService::persist_assets`]).
    pub in_library: bool,
    /// `{prompt,model,provider_id,params,canvas_id,node_id,task_id}`.
    pub origin: Value,
}

/// An input asset loaded to bytes (returned by [`AssetSource`]).
pub struct LoadedAsset {
    pub bytes: Vec<u8>,
    pub mime: String,
}

/// Where produced artifacts are persisted — implemented by the app over
/// `nomifun-workshop` (registers each result as a `wsa_` asset), so this crate
/// never depends on `nomifun-workshop` (no dependency cycle).
#[async_trait]
pub trait AssetSink: Send + Sync {
    /// Persist one produced artifact and return its new `wsa_` asset id.
    async fn persist(&self, asset: PersistAsset) -> Result<String, CreationError>;
}

/// Where task input assets are read from — the mirror of [`AssetSink`], also
/// implemented by the app over `nomifun-workshop`.
#[async_trait]
pub trait AssetSource: Send + Sync {
    /// Load an asset's bytes + MIME by its `wsa_` id.
    async fn load(&self, asset_id: &str) -> Result<LoadedAsset, CreationError>;
}

/// The persisted fields a worker needs to run (or resume) one task.
struct WorkerJob {
    id: String,
    canvas_id: Option<String>,
    node_id: Option<String>,
    provider_id: String,
    model: String,
    capability: MediaCapability,
    params: Value,
    inputs: Vec<CreationInput>,
    submitted_at: i64,
    /// Present only on a boot resume (skip submit, poll this remote job).
    remote_task_id: Option<String>,
}

/// The result of running one task through an adapter.
enum ExecOutcome {
    Succeeded(Vec<String>),
    Failed(CreationError),
    /// Cancelled mid-flight — the terminal `canceled` status was already written
    /// by [`CreationService::cancel_task`], so the worker must not overwrite it.
    Canceled,
}

pub struct CreationService {
    repo: Arc<dyn ICreationTaskRepository>,
    /// Registered media adapters (see [`crate::default_adapters`]).
    providers: Vec<Arc<dyn MediaProvider>>,
    /// Provider-row lookup for endpoint/key resolution (`None` in the bare
    /// skeleton — tasks then fail `config`).
    provider_repo: Option<Arc<dyn IProviderRepository>>,
    encryption_key: [u8; 32],
    http: reqwest::Client,
    asset_source: Option<Arc<dyn AssetSource>>,
    asset_sink: Option<Arc<dyn AssetSink>>,
    global_sem: Arc<Semaphore>,
    per_provider_limit: usize,
    provider_sems: Mutex<HashMap<String, Arc<Semaphore>>>,
    /// Live task id → its cancellation token (present while queued/running).
    inflight: Mutex<HashMap<String, CancellationToken>>,
    poll_interval: Duration,
    task_timeout: Duration,
}

/// Builder for [`CreationService`] (the app wires adapters + resolver + sink).
pub struct CreationServiceBuilder {
    repo: Arc<dyn ICreationTaskRepository>,
    providers: Vec<Arc<dyn MediaProvider>>,
    provider_repo: Option<Arc<dyn IProviderRepository>>,
    encryption_key: [u8; 32],
    http: Option<reqwest::Client>,
    asset_source: Option<Arc<dyn AssetSource>>,
    asset_sink: Option<Arc<dyn AssetSink>>,
    per_provider_limit: usize,
    global_limit: usize,
    poll_interval: Duration,
    task_timeout: Duration,
}

impl CreationServiceBuilder {
    pub fn with_providers(mut self, providers: Vec<Arc<dyn MediaProvider>>) -> Self {
        self.providers = providers;
        self
    }

    /// Provider-row repo + machine-bound AES key (mirrors the `ProviderService`
    /// / `ModelFetchService` key-passing convention).
    pub fn with_provider_repo(mut self, repo: Arc<dyn IProviderRepository>, encryption_key: [u8; 32]) -> Self {
        self.provider_repo = Some(repo);
        self.encryption_key = encryption_key;
        self
    }

    pub fn with_http(mut self, http: reqwest::Client) -> Self {
        self.http = Some(http);
        self
    }

    pub fn with_asset_source(mut self, source: Arc<dyn AssetSource>) -> Self {
        self.asset_source = Some(source);
        self
    }

    pub fn with_asset_sink(mut self, sink: Arc<dyn AssetSink>) -> Self {
        self.asset_sink = Some(sink);
        self
    }

    /// Override the poll interval (async protocols) — primarily for tests.
    pub fn with_poll_interval(mut self, interval: Duration) -> Self {
        self.poll_interval = interval;
        self
    }

    /// Override the async task timeout — primarily for tests.
    pub fn with_task_timeout(mut self, timeout: Duration) -> Self {
        self.task_timeout = timeout;
        self
    }

    pub fn build(self) -> Arc<CreationService> {
        Arc::new(CreationService {
            repo: self.repo,
            providers: self.providers,
            provider_repo: self.provider_repo,
            encryption_key: self.encryption_key,
            http: self.http.unwrap_or_default(),
            asset_source: self.asset_source,
            asset_sink: self.asset_sink,
            global_sem: Arc::new(Semaphore::new(self.global_limit)),
            per_provider_limit: self.per_provider_limit,
            provider_sems: Mutex::new(HashMap::new()),
            inflight: Mutex::new(HashMap::new()),
            poll_interval: self.poll_interval,
            task_timeout: self.task_timeout,
        })
    }
}

impl CreationService {
    /// Start a builder over the task repo (adapters/resolver/sink layered on).
    pub fn builder(repo: Arc<dyn ICreationTaskRepository>) -> CreationServiceBuilder {
        CreationServiceBuilder {
            repo,
            providers: Vec::new(),
            provider_repo: None,
            encryption_key: [0u8; 32],
            http: None,
            asset_source: None,
            asset_sink: None,
            per_provider_limit: DEFAULT_PER_PROVIDER_LIMIT,
            global_limit: DEFAULT_GLOBAL_LIMIT,
            poll_interval: DEFAULT_POLL_INTERVAL,
            task_timeout: DEFAULT_TASK_TIMEOUT,
        }
    }

    /// Build a bare service over just the task repo (no adapters/resolver — tasks
    /// created against it fail `config`/`adapter_unavailable`). Full wiring uses
    /// [`CreationService::builder`].
    pub fn new(repo: Arc<dyn ICreationTaskRepository>) -> Arc<Self> {
        Self::builder(repo).build()
    }

    /// The first registered adapter that serves `cap`, if any.
    pub fn provider_for(&self, cap: MediaCapability) -> Option<Arc<dyn MediaProvider>> {
        self.providers.iter().find(|p| p.supports(cap)).cloned()
    }

    // -----------------------------------------------------------------------
    // Public surface (routes)
    // -----------------------------------------------------------------------

    /// Enqueue a task (`queued`), spawn its worker, and return the queued task.
    /// The worker resolves the provider, loads inputs, runs the adapter, and
    /// drives the state machine to a terminal state asynchronously.
    pub async fn create_task(self: &Arc<Self>, req: NewCreationTask) -> Result<CreationTask, AppError> {
        let capability = MediaCapability::parse(&req.capability).ok_or_else(|| {
            AppError::BadRequest(format!(
                "unknown capability '{}' (expected t2i|i2i|inpaint|t2v|i2v|v2v|tts|text)",
                req.capability
            ))
        })?;
        if req.provider_id.trim().is_empty() {
            return Err(AppError::BadRequest("provider_id must not be empty".into()));
        }
        if req.model.trim().is_empty() {
            return Err(AppError::BadRequest("model must not be empty".into()));
        }
        if let Some(bad) = req.inputs.iter().find(|i| i.asset_id.trim().is_empty()) {
            return Err(AppError::BadRequest(format!("input has an empty asset_id (role '{}')", bad.role)));
        }

        let params_json = serde_json::to_string(&req.params)
            .map_err(|e| AppError::BadRequest(format!("invalid params json: {e}")))?;
        let id = generate_prefixed_id("wst");
        let now = now_ms();
        let row = self
            .repo
            .create_task(CreateCreationTaskParams {
                id: &id,
                canvas_id: req.canvas_id.as_deref(),
                node_id: req.node_id.as_deref(),
                provider_id: &req.provider_id,
                model: &req.model,
                capability: capability.as_str(),
                params: &params_json,
                status: TaskStatus::Queued.as_str(),
                submitted_at: now,
            })
            .await?;

        self.spawn(WorkerJob {
            id,
            canvas_id: req.canvas_id,
            node_id: req.node_id,
            provider_id: req.provider_id,
            model: req.model,
            capability,
            params: req.params,
            inputs: req.inputs,
            submitted_at: now,
            remote_task_id: None,
        });

        Ok(row.into())
    }

    pub async fn get_task(&self, id: &str) -> Result<CreationTask, AppError> {
        let row = self
            .repo
            .get_task(id)
            .await?
            .ok_or_else(|| AppError::NotFound(format!("creation task {id} not found")))?;
        Ok(row.into())
    }

    pub async fn list_tasks(
        &self,
        canvas_id: Option<&str>,
        status: Option<&str>,
        limit: i64,
    ) -> Result<Vec<CreationTask>, AppError> {
        let rows = self
            .repo
            .list_tasks(ListCreationTasksParams {
                canvas_id: canvas_id.filter(|s| !s.trim().is_empty()),
                status: status.filter(|s| !s.trim().is_empty()),
                limit,
            })
            .await?;
        Ok(rows.into_iter().map(CreationTask::from).collect())
    }

    /// Cancel a task. Terminal tasks are returned unchanged (idempotent); a live
    /// task moves to `canceled` and its worker is signalled to abort in-flight.
    pub async fn cancel_task(&self, id: &str) -> Result<CreationTask, AppError> {
        let row = self
            .repo
            .get_task(id)
            .await?
            .ok_or_else(|| AppError::NotFound(format!("creation task {id} not found")))?;
        if TaskStatus::parse_str(&row.status).is_some_and(TaskStatus::is_terminal) {
            return Ok(row.into());
        }
        // Write the terminal status FIRST, then cancel the token so the worker's
        // finalize sees `Canceled` and won't overwrite it.
        let updated = self
            .repo
            .update_task(
                id,
                UpdateCreationTaskParams {
                    status: Some(TaskStatus::Canceled.as_str()),
                    finished_at: Some(Some(now_ms())),
                    ..Default::default()
                },
            )
            .await?;
        if let Some(token) = self.inflight.lock().unwrap().get(id) {
            token.cancel();
        }
        Ok(updated.into())
    }

    /// Boot reconciliation ("running ⟺ live worker" invariant). Async tasks that
    /// have a remote job id are RESUMED (their poll loop restarts); every other
    /// live task (queued, or running with no remote handle) is converged to
    /// `failed(interrupted)`. Returns the count settled as failed.
    pub async fn reconcile_on_boot(self: &Arc<Self>) -> usize {
        let live = match self.repo.list_live_tasks().await {
            Ok(rows) => rows,
            Err(e) => {
                tracing::warn!(error = %e, "creation boot reconcile: list live tasks failed");
                return 0;
            }
        };
        let mut settled = 0;
        let mut resumed = 0;
        for row in live {
            let remote = row.remote_task_id.as_deref().map(str::trim).filter(|s| !s.is_empty());
            let capability = MediaCapability::parse(&row.capability);
            let resumable = row.status == TaskStatus::Running.as_str() && remote.is_some() && capability.is_some();

            if resumable {
                let params = serde_json::from_str::<Value>(&row.params).unwrap_or_else(|_| json!({}));
                self.spawn(WorkerJob {
                    id: row.id,
                    canvas_id: row.canvas_id,
                    node_id: row.node_id,
                    provider_id: row.provider_id,
                    model: row.model,
                    capability: capability.expect("checked above"),
                    params,
                    inputs: Vec::new(), // inputs already consumed at submit; poll needs none
                    submitted_at: row.submitted_at,
                    remote_task_id: remote.map(str::to_string),
                });
                resumed += 1;
                continue;
            }

            let err = CreationError::new(
                "interrupted",
                "task did not survive a restart (no live worker); settled at boot",
            );
            match self.write_failed(&row.id, &err).await {
                Ok(()) => settled += 1,
                Err(e) => tracing::warn!(id = %row.id, error = %e, "creation boot reconcile: settle failed"),
            }
        }
        if settled > 0 || resumed > 0 {
            tracing::info!(settled, resumed, "creation boot reconcile complete");
        }
        settled
    }

    // -----------------------------------------------------------------------
    // Worker lifecycle
    // -----------------------------------------------------------------------

    /// Register the task's cancellation token and spawn its worker (fresh or
    /// resume, distinguished by `job.remote_task_id`).
    fn spawn(self: &Arc<Self>, job: WorkerJob) {
        let token = CancellationToken::new();
        self.inflight.lock().unwrap().insert(job.id.clone(), token.clone());
        let this = Arc::clone(self);
        let id = job.id.clone();
        tokio::spawn(async move {
            this.run_worker(job, token).await;
            this.inflight.lock().unwrap().remove(&id);
        });
    }

    async fn run_worker(&self, job: WorkerJob, token: CancellationToken) {
        // Wait for a global + per-provider permit (cancellable while queued).
        let _permits = match self.acquire_permits(&job.provider_id, &token).await {
            Some(p) => p,
            None => return, // cancelled while queued (status already `canceled`)
        };
        if token.is_cancelled() {
            return;
        }
        // A fresh task transitions queued→running; a resume is already running.
        // The transition is conditional on the task still being live, so a
        // cancel that lands after acquire_permits cannot be resurrected to
        // `running` (and then finalized as succeeded).
        if job.remote_task_id.is_none() {
            match self.mark_running(&job.id).await {
                Ok(true) => {}
                Ok(false) => return, // canceled (or gone) before we claimed running
                Err(e) => {
                    tracing::warn!(id = %job.id, error = %e, "creation: mark running failed; abandoning task");
                    return;
                }
            }
        }

        let outcome = self.execute(&job, &token).await;
        self.finalize(&job.id, &token, outcome).await;
    }

    async fn execute(&self, job: &WorkerJob, token: &CancellationToken) -> ExecOutcome {
        let provider = match self.resolve_provider(&job.provider_id).await {
            Ok(p) => p,
            Err(e) => return ExecOutcome::Failed(e),
        };
        let adapter = match self.select_adapter(job.capability, &provider.platform, &job.model) {
            Ok(a) => a,
            Err(e) => return ExecOutcome::Failed(e),
        };
        // Fresh tasks load their input bytes; a resume polls with no inputs.
        let inputs = if job.remote_task_id.is_none() {
            match self.load_inputs(&job.inputs).await {
                Ok(i) => i,
                Err(e) => return ExecOutcome::Failed(e),
            }
        } else {
            Vec::new()
        };
        let req = SubmitRequest {
            provider,
            model: job.model.clone(),
            capability: job.capability,
            params: job.params.clone(),
            inputs,
        };

        if let Some(remote) = job.remote_task_id.as_deref() {
            return self.poll_loop(job, adapter.as_ref(), &req, remote, token).await;
        }

        let ack = tokio::select! {
            _ = token.cancelled() => return ExecOutcome::Canceled,
            r = adapter.submit(&req) => r,
        };
        match ack {
            Err(e) => ExecOutcome::Failed(e),
            Ok(SubmitAck::Done(assets)) => self.persist_or_fail(job, assets).await,
            Ok(SubmitAck::Pending { remote_task_id }) => {
                if let Err(e) = self.set_remote(&job.id, &remote_task_id).await {
                    return ExecOutcome::Failed(CreationError::config(format!("persist remote task id failed: {e}")));
                }
                self.poll_loop(job, adapter.as_ref(), &req, &remote_task_id, token).await
            }
        }
    }

    async fn poll_loop(
        &self,
        job: &WorkerJob,
        adapter: &dyn MediaProvider,
        req: &SubmitRequest,
        remote_task_id: &str,
        token: &CancellationToken,
    ) -> ExecOutcome {
        // A boot-resumed job (its `remote_task_id` was set at spawn from the
        // persisted row) budgets from resume time, NOT the original submit: the
        // app may have been down far longer than `task_timeout`, and an absolute
        // `submitted_at + timeout` deadline would already be elapsed, failing the
        // still-healthy remote job on the first iteration without a single poll.
        let deadline = if job.remote_task_id.is_some() {
            now_ms() + self.task_timeout.as_millis() as i64
        } else {
            job.submitted_at + self.task_timeout.as_millis() as i64
        };
        loop {
            if token.is_cancelled() {
                return ExecOutcome::Canceled;
            }
            if now_ms() >= deadline {
                return ExecOutcome::Failed(CreationError::timeout(
                    "async task exceeded its poll deadline",
                ));
            }
            tokio::select! {
                _ = token.cancelled() => return ExecOutcome::Canceled,
                _ = tokio::time::sleep(self.poll_interval) => {}
            }
            let poll = tokio::select! {
                _ = token.cancelled() => return ExecOutcome::Canceled,
                r = adapter.poll(remote_task_id, req) => r,
            };
            match poll {
                Ok(PollResult::Pending) => continue,
                Ok(PollResult::Done(assets)) => return self.persist_or_fail(job, assets).await,
                Ok(PollResult::Failed(e)) => return ExecOutcome::Failed(e),
                Err(e) => {
                    // 4xx is terminal (bad job id / auth); 5xx / network is
                    // transient — keep polling until the deadline.
                    if e.http_status.is_some_and(|s| (400..500).contains(&s)) {
                        return ExecOutcome::Failed(e);
                    }
                    tracing::warn!(id = %job.id, error = %e.message, "creation poll transient error; retrying");
                }
            }
        }
    }

    async fn persist_or_fail(&self, job: &WorkerJob, assets: Vec<crate::provider::ProducedAsset>) -> ExecOutcome {
        match self.persist_assets(job, assets).await {
            Ok(ids) => ExecOutcome::Succeeded(ids),
            Err(e) => ExecOutcome::Failed(e),
        }
    }

    async fn finalize(&self, id: &str, token: &CancellationToken, outcome: ExecOutcome) {
        match outcome {
            ExecOutcome::Canceled => {} // status already `canceled`
            ExecOutcome::Succeeded(ids) => {
                if token.is_cancelled() {
                    return; // a cancel won the race; leave the `canceled` status
                }
                if let Err(e) = self.write_succeeded(id, &ids).await {
                    tracing::warn!(id, error = %e, "creation: write succeeded failed");
                }
            }
            ExecOutcome::Failed(err) => {
                if token.is_cancelled() {
                    return;
                }
                if let Err(e) = self.write_failed(id, &err).await {
                    tracing::warn!(id, error = %e, "creation: write failed failed");
                }
            }
        }
    }

    // -----------------------------------------------------------------------
    // Resolution + IO helpers
    // -----------------------------------------------------------------------

    async fn resolve_provider(&self, provider_id: &str) -> Result<ResolvedProvider, CreationError> {
        let repo = self
            .provider_repo
            .as_ref()
            .ok_or_else(|| CreationError::config("no provider repository wired into the creation engine"))?;
        let row = repo
            .find_by_id(provider_id)
            .await
            .map_err(|e| CreationError::config(format!("provider lookup failed: {e}")))?
            .ok_or_else(|| CreationError::new("provider_not_found", format!("provider '{provider_id}' not found")))?;
        let key_raw = decrypt_string(&row.api_key_encrypted, &self.encryption_key)
            .map_err(|e| CreationError::config(format!("decrypt provider api key failed: {e}")))?;
        let api_key = primary_api_key(&key_raw)
            .ok_or_else(|| CreationError::config("provider has no usable api key"))?;
        if row.base_url.trim().is_empty() {
            return Err(CreationError::config("provider base_url is empty"));
        }
        Ok(ResolvedProvider {
            provider_id: row.id,
            platform: row.platform,
            base_url: row.base_url,
            api_key,
            is_full_url: row.is_full_url,
        })
    }

    fn select_adapter(
        &self,
        cap: MediaCapability,
        platform: &str,
        model: &str,
    ) -> Result<Arc<dyn MediaProvider>, CreationError> {
        let id = route_adapter_id(cap, platform, model).ok_or_else(|| {
            CreationError::new("unsupported_capability", format!("no adapter routes capability {}", cap.as_str()))
        })?;
        let adapter = self
            .providers
            .iter()
            .find(|p| p.id() == id)
            .cloned()
            .ok_or_else(CreationError::adapter_unavailable)?;
        if !adapter.supports(cap) {
            return Err(CreationError::new(
                "adapter_unavailable",
                format!("adapter '{id}' does not support {}", cap.as_str()),
            ));
        }
        Ok(adapter)
    }

    async fn load_inputs(&self, inputs: &[CreationInput]) -> Result<Vec<InputAsset>, CreationError> {
        if inputs.is_empty() {
            return Ok(Vec::new());
        }
        let source = self
            .asset_source
            .as_ref()
            .ok_or_else(|| CreationError::config("no asset source wired into the creation engine"))?;
        let mut out = Vec::with_capacity(inputs.len());
        for i in inputs {
            let loaded = source.load(&i.asset_id).await?;
            out.push(InputAsset {
                asset_id: i.asset_id.clone(),
                role: i.role.clone(),
                bytes: loaded.bytes,
                mime: loaded.mime,
            });
        }
        Ok(out)
    }

    async fn persist_assets(
        &self,
        job: &WorkerJob,
        assets: Vec<crate::provider::ProducedAsset>,
    ) -> Result<Vec<String>, CreationError> {
        let sink = self
            .asset_sink
            .as_ref()
            .ok_or_else(|| CreationError::config("no asset sink wired into the creation engine"))?;
        let origin = build_origin(job);
        let mut ids = Vec::with_capacity(assets.len());
        for a in assets {
            let (bytes, mime) = match a.data {
                ProducedData::Bytes(b) => (b, a.mime.unwrap_or_else(|| "image/png".to_string())),
                ProducedData::Url(u) => self.download(&u, a.mime).await?,
            };
            let id = sink
                .persist(PersistAsset {
                    canvas_id: job.canvas_id.clone(),
                    node_id: job.node_id.clone(),
                    bytes,
                    mime,
                    in_library: true, // generated products land in the library by default
                    origin: origin.clone(),
                })
                .await?;
            ids.push(id);
        }
        if ids.is_empty() {
            return Err(CreationError::provider_error("adapter produced no artifacts"));
        }
        Ok(ids)
    }

    async fn download(&self, url: &str, mime_hint: Option<String>) -> Result<(Vec<u8>, String), CreationError> {
        let resp = self
            .http
            .get(url)
            .timeout(DOWNLOAD_TIMEOUT)
            .send()
            .await
            .map_err(net_err)?;
        if !resp.status().is_success() {
            return Err(error_from_response(resp).await);
        }
        let mime = mime_hint
            .or_else(|| {
                resp.headers()
                    .get(reqwest::header::CONTENT_TYPE)
                    .and_then(|v| v.to_str().ok())
                    .map(|s| s.split(';').next().unwrap_or(s).trim().to_string())
                    .filter(|s| !s.is_empty())
            })
            .unwrap_or_else(|| "application/octet-stream".to_string());
        let bytes = read_body_capped(resp, MAX_ARTIFACT_BYTES).await?;
        Ok((bytes, mime))
    }

    fn provider_sem(&self, provider_id: &str) -> Arc<Semaphore> {
        self.provider_sems
            .lock()
            .unwrap()
            .entry(provider_id.to_string())
            .or_insert_with(|| Arc::new(Semaphore::new(self.per_provider_limit)))
            .clone()
    }

    /// Acquire a global + per-provider permit, cancellable while waiting. Returns
    /// `None` if the token fires before both are held.
    async fn acquire_permits(
        &self,
        provider_id: &str,
        token: &CancellationToken,
    ) -> Option<(OwnedSemaphorePermit, OwnedSemaphorePermit)> {
        let global = tokio::select! {
            _ = token.cancelled() => return None,
            p = self.global_sem.clone().acquire_owned() => p.ok()?,
        };
        let sem = self.provider_sem(provider_id);
        let per = tokio::select! {
            _ = token.cancelled() => return None,
            p = sem.acquire_owned() => p.ok()?,
        };
        Some((global, per))
    }

    // -----------------------------------------------------------------------
    // DB state transitions (best-effort; log on failure)
    // -----------------------------------------------------------------------

    /// Transition queued→running, conditional on the task still being live.
    /// Returns `false` when a concurrent cancel already wrote a terminal status
    /// (so the worker must not proceed and resurrect it).
    async fn mark_running(&self, id: &str) -> Result<bool, AppError> {
        let applied = self
            .repo
            .update_task_if_live(
                id,
                UpdateCreationTaskParams {
                    status: Some(TaskStatus::Running.as_str()),
                    started_at: Some(Some(now_ms())),
                    ..Default::default()
                },
            )
            .await?;
        Ok(applied)
    }

    async fn set_remote(&self, id: &str, remote_task_id: &str) -> Result<(), AppError> {
        self.repo
            .update_task(
                id,
                UpdateCreationTaskParams {
                    remote_task_id: Some(Some(remote_task_id)),
                    ..Default::default()
                },
            )
            .await?;
        Ok(())
    }

    async fn write_succeeded(&self, id: &str, asset_ids: &[String]) -> Result<(), AppError> {
        let ids_json = serde_json::to_string(asset_ids).unwrap_or_else(|_| "[]".to_string());
        // Conditional: never overwrite a terminal status (e.g. a `canceled` that
        // won the race with this finalize). The token check in `finalize` is a
        // cheap early-out; THIS is the correctness gate.
        let applied = self
            .repo
            .update_task_if_live(
                id,
                UpdateCreationTaskParams {
                    status: Some(TaskStatus::Succeeded.as_str()),
                    result_asset_ids: Some(&ids_json),
                    finished_at: Some(Some(now_ms())),
                    ..Default::default()
                },
            )
            .await?;
        if !applied {
            tracing::info!(id, "creation: succeeded write skipped; task no longer live (cancel won the race)");
        }
        Ok(())
    }

    async fn write_failed(&self, id: &str, err: &CreationError) -> Result<(), AppError> {
        let error_json = serde_json::to_string(err)
            .unwrap_or_else(|_| r#"{"kind":"internal","message":"error serialization failed"}"#.to_string());
        let applied = self
            .repo
            .update_task_if_live(
                id,
                UpdateCreationTaskParams {
                    status: Some(TaskStatus::Failed.as_str()),
                    error: Some(Some(&error_json)),
                    finished_at: Some(Some(now_ms())),
                    ..Default::default()
                },
            )
            .await?;
        if !applied {
            tracing::info!(id, "creation: failed write skipped; task no longer live");
        }
        Ok(())
    }
}

/// The first non-empty API key from a comma/newline-separated list (P0 takes the
/// first usable key; rotation is a later hook).
fn primary_api_key(raw: &str) -> Option<String> {
    raw.split([',', '\n']).map(str::trim).find(|k| !k.is_empty()).map(str::to_owned)
}

/// Build the provenance object stamped onto every produced asset's `origin`.
fn build_origin(job: &WorkerJob) -> Value {
    json!({
        "prompt": job.params.get("prompt").and_then(|v| v.as_str()).unwrap_or_default(),
        "model": job.model,
        "provider_id": job.provider_id,
        "capability": job.capability.as_str(),
        "params": job.params,
        "canvas_id": job.canvas_id,
        "node_id": job.node_id,
        "task_id": job.id,
    })
}

impl TaskStatus {
    fn parse_str(s: &str) -> Option<Self> {
        Some(match s {
            "queued" => Self::Queued,
            "running" => Self::Running,
            "succeeded" => Self::Succeeded,
            "failed" => Self::Failed,
            "canceled" => Self::Canceled,
            _ => return None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::{PollResult, ProducedAsset, ProducedData};
    use nomifun_db::{SqliteCreationTaskRepository, SqliteProviderRepository};
    use std::sync::atomic::{AtomicUsize, Ordering};

    const TEST_KEY: [u8; 32] = [0x42; 32];

    // ---- test doubles ----

    /// A configurable adapter: synchronous `Done`, or async `Pending` then a
    /// scripted number of `Pending` polls before a terminal outcome.
    struct MockAdapter {
        id: &'static str,
        supports: Vec<MediaCapability>,
        behavior: MockBehavior,
        submit_calls: AtomicUsize,
        poll_calls: AtomicUsize,
    }
    #[derive(Clone)]
    enum MockBehavior {
        DoneSync,
        SubmitError(String),
        /// Pending on submit; return Pending for `pending_polls` polls, then Done.
        AsyncDone { pending_polls: usize },
        /// Pending on submit; never completes (each poll returns Pending).
        AsyncNever,
    }
    impl MockAdapter {
        fn sync(id: &'static str) -> Arc<Self> {
            Arc::new(Self {
                id,
                supports: vec![MediaCapability::T2i, MediaCapability::I2i, MediaCapability::Inpaint],
                behavior: MockBehavior::DoneSync,
                submit_calls: AtomicUsize::new(0),
                poll_calls: AtomicUsize::new(0),
            })
        }
        fn with(id: &'static str, supports: Vec<MediaCapability>, behavior: MockBehavior) -> Arc<Self> {
            Arc::new(Self {
                id,
                supports,
                behavior,
                submit_calls: AtomicUsize::new(0),
                poll_calls: AtomicUsize::new(0),
            })
        }
    }
    #[async_trait]
    impl MediaProvider for MockAdapter {
        fn id(&self) -> &'static str {
            self.id
        }
        fn supports(&self, cap: MediaCapability) -> bool {
            self.supports.contains(&cap)
        }
        async fn submit(&self, _req: &SubmitRequest) -> Result<SubmitAck, CreationError> {
            self.submit_calls.fetch_add(1, Ordering::SeqCst);
            match &self.behavior {
                MockBehavior::DoneSync => Ok(SubmitAck::Done(vec![ProducedAsset {
                    data: ProducedData::Bytes(b"img".to_vec()),
                    mime: Some("image/png".into()),
                }])),
                MockBehavior::SubmitError(m) => Err(CreationError::provider_error(m.clone())),
                MockBehavior::AsyncDone { .. } | MockBehavior::AsyncNever => {
                    Ok(SubmitAck::Pending { remote_task_id: "remote-123".into() })
                }
            }
        }
        async fn poll(&self, _remote: &str, _req: &SubmitRequest) -> Result<PollResult, CreationError> {
            let n = self.poll_calls.fetch_add(1, Ordering::SeqCst);
            match &self.behavior {
                MockBehavior::AsyncDone { pending_polls } => {
                    if n < *pending_polls {
                        Ok(PollResult::Pending)
                    } else {
                        Ok(PollResult::Done(vec![ProducedAsset {
                            data: ProducedData::Bytes(b"vid".to_vec()),
                            mime: Some("video/mp4".into()),
                        }]))
                    }
                }
                MockBehavior::AsyncNever => Ok(PollResult::Pending),
                _ => Ok(PollResult::Pending),
            }
        }
    }

    struct RecordingSink {
        count: AtomicUsize,
    }
    #[async_trait]
    impl AssetSink for RecordingSink {
        async fn persist(&self, _asset: PersistAsset) -> Result<String, CreationError> {
            let n = self.count.fetch_add(1, Ordering::SeqCst);
            Ok(format!("wsa_test_{n}"))
        }
    }

    struct StaticSource;
    #[async_trait]
    impl AssetSource for StaticSource {
        async fn load(&self, _asset_id: &str) -> Result<LoadedAsset, CreationError> {
            Ok(LoadedAsset { bytes: b"input".to_vec(), mime: "image/png".into() })
        }
    }

    // ---- harness ----

    async fn seed_provider(pool: &nomifun_db::SqlitePool, platform: &str) -> String {
        let repo = SqliteProviderRepository::new(pool.clone());
        let encrypted = nomifun_common::encrypt_string("sk-test-key", &TEST_KEY).unwrap();
        let row = repo
            .create(nomifun_db::CreateProviderParams {
                id: None,
                platform,
                name: "Test",
                base_url: "https://api.test.com/v1",
                api_key_encrypted: &encrypted,
                models: "[]",
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
        row.id
    }

    struct Harness {
        svc: Arc<CreationService>,
        provider_id: String,
        sink: Arc<RecordingSink>,
        _db: nomifun_db::Database,
    }

    async fn harness(adapter: Arc<dyn MediaProvider>, platform: &str) -> Harness {
        let db = nomifun_db::init_database_memory().await.unwrap();
        let pool = db.pool().clone();
        let provider_id = seed_provider(&pool, platform).await;
        let repo: Arc<dyn ICreationTaskRepository> = Arc::new(SqliteCreationTaskRepository::new(pool.clone()));
        let provider_repo: Arc<dyn IProviderRepository> = Arc::new(SqliteProviderRepository::new(pool));
        let sink = Arc::new(RecordingSink { count: AtomicUsize::new(0) });
        let svc = CreationService::builder(repo)
            .with_providers(vec![adapter])
            .with_provider_repo(provider_repo, TEST_KEY)
            .with_asset_source(Arc::new(StaticSource))
            .with_asset_sink(sink.clone())
            .with_poll_interval(Duration::from_millis(10))
            .with_task_timeout(Duration::from_secs(30))
            .build();
        Harness { svc, provider_id, sink, _db: db }
    }

    async fn wait_terminal(svc: &Arc<CreationService>, id: &str) -> CreationTask {
        for _ in 0..400 {
            let t = svc.get_task(id).await.unwrap();
            if TaskStatus::parse_str(&t.status).is_some_and(TaskStatus::is_terminal) {
                return t;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("task {id} did not reach a terminal state");
    }

    fn new_task(provider_id: &str, capability: &str) -> NewCreationTask {
        NewCreationTask {
            canvas_id: Some("wsc_1".into()),
            node_id: Some("node_1".into()),
            provider_id: provider_id.into(),
            model: "test-model".into(),
            capability: capability.into(),
            params: json!({"prompt": "a cat", "count": 1}),
            inputs: vec![],
        }
    }

    #[tokio::test]
    async fn sync_task_succeeds_and_persists_asset() {
        let h = harness(MockAdapter::sync("openai_images"), "openai").await;
        let created = h.svc.create_task(new_task(&h.provider_id, "t2i")).await.unwrap();
        assert_eq!(created.status, "queued");
        assert!(created.id.starts_with("wst_"));

        let done = wait_terminal(&h.svc, &created.id).await;
        assert_eq!(done.status, "succeeded");
        assert_eq!(done.result_asset_ids, vec!["wsa_test_0".to_string()]);
        assert!(done.finished_at.is_some());
        assert_eq!(h.sink.count.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn async_task_polls_then_succeeds() {
        let adapter = MockAdapter::with(
            "openai_video",
            vec![MediaCapability::T2v, MediaCapability::I2v],
            MockBehavior::AsyncDone { pending_polls: 2 },
        );
        let h = harness(adapter, "openai").await;
        let created = h.svc.create_task(new_task(&h.provider_id, "t2v")).await.unwrap();
        let done = wait_terminal(&h.svc, &created.id).await;
        assert_eq!(done.status, "succeeded");
        assert_eq!(done.result_asset_ids.len(), 1);
        // remote task id was persisted on the way through
        let row = h.svc.get_task(&created.id).await.unwrap();
        assert_eq!(row.status, "succeeded");
    }

    #[tokio::test]
    async fn submit_error_fails_task() {
        let adapter = MockAdapter::with(
            "openai_images",
            vec![MediaCapability::T2i],
            MockBehavior::SubmitError("boom".into()),
        );
        let h = harness(adapter, "openai").await;
        let created = h.svc.create_task(new_task(&h.provider_id, "t2i")).await.unwrap();
        let done = wait_terminal(&h.svc, &created.id).await;
        assert_eq!(done.status, "failed");
        assert_eq!(done.error.as_ref().unwrap()["kind"], "provider_error");
        assert!(done.error.as_ref().unwrap()["message"].as_str().unwrap().contains("boom"));
    }

    #[tokio::test]
    async fn cancel_interrupts_running_async_task() {
        let adapter = MockAdapter::with(
            "openai_video",
            vec![MediaCapability::T2v],
            MockBehavior::AsyncNever,
        );
        let h = harness(adapter, "openai").await;
        let created = h.svc.create_task(new_task(&h.provider_id, "t2v")).await.unwrap();

        // Wait until it is running (submitted → pending → polling).
        let mut running = false;
        for _ in 0..200 {
            if h.svc.get_task(&created.id).await.unwrap().status == "running" {
                running = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(running, "task never reached running");

        let canceled = h.svc.cancel_task(&created.id).await.unwrap();
        assert_eq!(canceled.status, "canceled");
        // Stays canceled (worker must not overwrite with succeeded/failed).
        tokio::time::sleep(Duration::from_millis(80)).await;
        assert_eq!(h.svc.get_task(&created.id).await.unwrap().status, "canceled");
    }

    #[tokio::test]
    async fn cancel_is_idempotent_on_terminal() {
        let h = harness(MockAdapter::sync("openai_images"), "openai").await;
        let created = h.svc.create_task(new_task(&h.provider_id, "t2i")).await.unwrap();
        let done = wait_terminal(&h.svc, &created.id).await;
        assert_eq!(done.status, "succeeded");
        // cancel of a terminal task returns it unchanged
        let after = h.svc.cancel_task(&created.id).await.unwrap();
        assert_eq!(after.status, "succeeded");
        assert!(matches!(h.svc.cancel_task("wst_missing").await.unwrap_err(), AppError::NotFound(_)));
    }

    #[tokio::test]
    async fn bad_capability_and_empty_provider_rejected() {
        let h = harness(MockAdapter::sync("openai_images"), "openai").await;
        let mut bad = new_task(&h.provider_id, "nope");
        assert!(matches!(h.svc.create_task(bad).await.unwrap_err(), AppError::BadRequest(_)));
        bad = new_task("  ", "t2i");
        assert!(matches!(h.svc.create_task(bad).await.unwrap_err(), AppError::BadRequest(_)));
    }

    #[tokio::test]
    async fn missing_provider_fails_task() {
        let h = harness(MockAdapter::sync("openai_images"), "openai").await;
        let created = h.svc.create_task(new_task("prov_missing", "t2i")).await.unwrap();
        let done = wait_terminal(&h.svc, &created.id).await;
        assert_eq!(done.status, "failed");
        assert_eq!(done.error.as_ref().unwrap()["kind"], "provider_not_found");
    }

    #[tokio::test]
    async fn reconcile_settles_queued_and_resumes_running_with_remote() {
        // Build a service whose adapter completes on the first poll, so a resumed
        // running-with-remote task reaches succeeded.
        let adapter = MockAdapter::with(
            "openai_video",
            vec![MediaCapability::T2v],
            MockBehavior::AsyncDone { pending_polls: 0 },
        );
        let h = harness(adapter, "openai").await;
        let repo = &h.svc.repo;

        // (a) a queued leftover → should become failed(interrupted)
        repo.create_task(CreateCreationTaskParams {
            id: "wst_queued",
            canvas_id: None,
            node_id: None,
            provider_id: &h.provider_id,
            model: "test-model",
            capability: "t2i",
            params: "{}",
            status: "queued",
            submitted_at: now_ms(),
        })
        .await
        .unwrap();

        // (b) a running task WITHOUT remote → failed(interrupted)
        repo.create_task(CreateCreationTaskParams {
            id: "wst_running_noremote",
            canvas_id: None,
            node_id: None,
            provider_id: &h.provider_id,
            model: "test-model",
            capability: "t2v",
            params: "{}",
            status: "queued",
            submitted_at: now_ms(),
        })
        .await
        .unwrap();
        repo.update_task("wst_running_noremote", UpdateCreationTaskParams { status: Some("running"), ..Default::default() })
            .await
            .unwrap();

        // (c) a running task WITH remote → resumed → succeeded
        repo.create_task(CreateCreationTaskParams {
            id: "wst_resume",
            canvas_id: None,
            node_id: None,
            provider_id: &h.provider_id,
            model: "test-model",
            capability: "t2v",
            params: "{}",
            status: "queued",
            submitted_at: now_ms(),
        })
        .await
        .unwrap();
        repo.update_task(
            "wst_resume",
            UpdateCreationTaskParams {
                status: Some("running"),
                remote_task_id: Some(Some("remote-xyz")),
                ..Default::default()
            },
        )
        .await
        .unwrap();

        let settled = h.svc.reconcile_on_boot().await;
        assert_eq!(settled, 2, "queued + running-without-remote settle as failed");

        assert_eq!(h.svc.get_task("wst_queued").await.unwrap().status, "failed");
        assert_eq!(
            h.svc.get_task("wst_queued").await.unwrap().error.unwrap()["kind"],
            "interrupted"
        );
        assert_eq!(h.svc.get_task("wst_running_noremote").await.unwrap().status, "failed");

        // resumed one completes via its poll loop
        let resumed = wait_terminal(&h.svc, "wst_resume").await;
        assert_eq!(resumed.status, "succeeded");
    }

    #[tokio::test]
    async fn reconcile_resumed_task_uses_fresh_deadline_not_stale_submitted_at() {
        // A resumable async task whose remote completes on the first poll.
        let adapter = MockAdapter::with(
            "openai_video",
            vec![MediaCapability::T2v],
            MockBehavior::AsyncDone { pending_polls: 0 },
        );
        let h = harness(adapter, "openai").await; // task_timeout = 30s
        let repo = &h.svc.repo;

        // submitted far in the past: an absolute (submitted_at + timeout)
        // deadline would already be elapsed, so the old code would fail this on
        // the first loop iteration WITHOUT ever polling the healthy remote job.
        let old = now_ms() - 3_600_000; // 1h ago
        repo.create_task(CreateCreationTaskParams {
            id: "wst_old_resume",
            canvas_id: None,
            node_id: None,
            provider_id: &h.provider_id,
            model: "test-model",
            capability: "t2v",
            params: "{}",
            status: "queued",
            submitted_at: old,
        })
        .await
        .unwrap();
        repo.update_task(
            "wst_old_resume",
            UpdateCreationTaskParams {
                status: Some("running"),
                remote_task_id: Some(Some("remote-old")),
                ..Default::default()
            },
        )
        .await
        .unwrap();

        let settled = h.svc.reconcile_on_boot().await;
        assert_eq!(settled, 0, "the resumable task is resumed, not settled as failed");
        // With a resume-relative deadline it polls to completion instead of an
        // instant timeout.
        let done = wait_terminal(&h.svc, "wst_old_resume").await;
        assert_eq!(done.status, "succeeded", "resumed old job polls to completion; error={:?}", done.error);
    }

    #[tokio::test]
    async fn bare_service_without_adapter_fails_config() {
        let db = nomifun_db::init_database_memory().await.unwrap();
        let repo: Arc<dyn ICreationTaskRepository> = Arc::new(SqliteCreationTaskRepository::new(db.pool().clone()));
        Box::leak(Box::new(db));
        let svc = CreationService::new(repo);
        let created = svc.create_task(new_task("prov_x", "t2i")).await.unwrap();
        assert_eq!(created.status, "queued");
        let done = wait_terminal(&svc, &created.id).await;
        // No provider repo wired → resolution fails with a config error.
        assert_eq!(done.status, "failed");
        assert_eq!(done.error.as_ref().unwrap()["kind"], "config");
    }

    #[test]
    fn primary_api_key_takes_first_nonempty() {
        assert_eq!(primary_api_key("k1,k2").as_deref(), Some("k1"));
        assert_eq!(primary_api_key("\n  ,  k2 \n k3").as_deref(), Some("k2"));
        assert_eq!(primary_api_key("   ").as_deref(), None);
        assert_eq!(primary_api_key("solo").as_deref(), Some("solo"));
    }

    #[test]
    fn build_origin_carries_provenance() {
        let job = WorkerJob {
            id: "wst_1".into(),
            canvas_id: Some("wsc_9".into()),
            node_id: Some("n_2".into()),
            provider_id: "prov_a".into(),
            model: "gpt-image-1".into(),
            capability: MediaCapability::T2i,
            params: json!({"prompt": "sunset", "count": 2}),
            inputs: vec![],
            submitted_at: 1,
            remote_task_id: None,
        };
        let o = build_origin(&job);
        assert_eq!(o["prompt"], "sunset");
        assert_eq!(o["model"], "gpt-image-1");
        assert_eq!(o["provider_id"], "prov_a");
        assert_eq!(o["canvas_id"], "wsc_9");
        assert_eq!(o["node_id"], "n_2");
        assert_eq!(o["task_id"], "wst_1");
        assert_eq!(o["capability"], "t2i");
        assert_eq!(o["params"]["count"], 2);
    }
}

/// End-to-end tests driving the **real adapters** through the engine against a
/// wiremock HTTP server — verifies request construction + response parsing +
/// artifact persistence over the wire (no live network).
#[cfg(test)]
mod http_e2e_tests {
    use super::*;
    use nomifun_db::{SqliteCreationTaskRepository, SqliteProviderRepository};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    const TEST_KEY: [u8; 32] = [0x37; 32];

    struct CountingSink {
        count: AtomicUsize,
        /// Captured `(mime, bytes)` of each persisted artifact — lets the text
        /// e2e assert the produced MIME + body without the real bridge.
        persisted: std::sync::Mutex<Vec<(String, Vec<u8>)>>,
    }
    #[async_trait]
    impl AssetSink for CountingSink {
        async fn persist(&self, asset: PersistAsset) -> Result<String, CreationError> {
            assert!(!asset.bytes.is_empty(), "persisted asset must carry bytes");
            self.persisted.lock().unwrap().push((asset.mime.clone(), asset.bytes.clone()));
            let n = self.count.fetch_add(1, Ordering::SeqCst);
            Ok(format!("wsa_e2e_{n}"))
        }
    }
    struct NoInputs;
    #[async_trait]
    impl AssetSource for NoInputs {
        async fn load(&self, _id: &str) -> Result<LoadedAsset, CreationError> {
            Err(CreationError::new("no_input", "no inputs in these tests"))
        }
    }

    async fn build(base_url: &str) -> (Arc<CreationService>, String, Arc<CountingSink>, nomifun_db::Database) {
        let db = nomifun_db::init_database_memory().await.unwrap();
        let pool = db.pool().clone();
        // seed a provider row pointed at the mock server
        let prov_repo = SqliteProviderRepository::new(pool.clone());
        let encrypted = nomifun_common::encrypt_string("sk-e2e", &TEST_KEY).unwrap();
        let provider_id = prov_repo
            .create(nomifun_db::CreateProviderParams {
                id: None,
                platform: "openai",
                name: "Mock",
                base_url,
                api_key_encrypted: &encrypted,
                models: "[]",
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
            .unwrap()
            .id;
        let repo: Arc<dyn ICreationTaskRepository> = Arc::new(SqliteCreationTaskRepository::new(pool.clone()));
        let provider_repo: Arc<dyn IProviderRepository> = Arc::new(SqliteProviderRepository::new(pool));
        let sink = Arc::new(CountingSink { count: AtomicUsize::new(0), persisted: std::sync::Mutex::new(Vec::new()) });
        let svc = CreationService::builder(repo)
            .with_providers(crate::default_adapters(reqwest::Client::new()))
            .with_provider_repo(provider_repo, TEST_KEY)
            .with_asset_source(Arc::new(NoInputs))
            .with_asset_sink(sink.clone())
            .with_poll_interval(Duration::from_millis(10))
            .with_task_timeout(Duration::from_secs(30))
            .build();
        (svc, provider_id, sink, db)
    }

    async fn wait_terminal(svc: &Arc<CreationService>, id: &str) -> CreationTask {
        for _ in 0..400 {
            let t = svc.get_task(id).await.unwrap();
            if TaskStatus::parse_str(&t.status).is_some_and(TaskStatus::is_terminal) {
                return t;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("task {id} never terminated");
    }

    fn t2i(provider_id: &str) -> NewCreationTask {
        NewCreationTask {
            canvas_id: Some("wsc_e".into()),
            node_id: None,
            provider_id: provider_id.into(),
            model: "gpt-image-1".into(),
            capability: "t2i".into(),
            params: json!({"prompt": "a fox", "width": 512, "height": 512, "count": 1}),
            inputs: vec![],
        }
    }

    #[tokio::test]
    async fn openai_images_end_to_end() {
        let server = MockServer::start().await;
        // "aGk=" == base64("hi")
        Mock::given(method("POST"))
            .and(path("/v1/images/generations"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"data": [{"b64_json": "aGk="}]})))
            .mount(&server)
            .await;

        let (svc, provider_id, sink, _db) = build(&server.uri()).await;
        let created = svc.create_task(t2i(&provider_id)).await.unwrap();
        let done = wait_terminal(&svc, &created.id).await;
        assert_eq!(done.status, "succeeded", "error={:?}", done.error);
        assert_eq!(done.result_asset_ids, vec!["wsa_e2e_0".to_string()]);
        assert_eq!(sink.count.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn openai_images_propagates_provider_error() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/images/generations"))
            .respond_with(ResponseTemplate::new(401).set_body_string("bad key"))
            .mount(&server)
            .await;

        let (svc, provider_id, _sink, _db) = build(&server.uri()).await;
        let created = svc.create_task(t2i(&provider_id)).await.unwrap();
        let done = wait_terminal(&svc, &created.id).await;
        assert_eq!(done.status, "failed");
        let err = done.error.unwrap();
        assert_eq!(err["kind"], "provider_error");
        assert_eq!(err["http_status"], 401);
    }

    #[tokio::test]
    async fn openai_video_submit_poll_content_end_to_end() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/videos"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"id": "vid_1", "status": "queued"})))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/v1/videos/vid_1"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"id": "vid_1", "status": "completed"})))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/v1/videos/vid_1/content"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "video/mp4")
                    .set_body_bytes(b"MP4DATA".to_vec()),
            )
            .mount(&server)
            .await;

        let (svc, provider_id, sink, _db) = build(&server.uri()).await;
        let task = NewCreationTask {
            canvas_id: None,
            node_id: None,
            provider_id: provider_id.clone(),
            model: "sora-2".into(),
            capability: "t2v".into(),
            params: json!({"prompt": "a wave", "seconds": 4}),
            inputs: vec![],
        };
        let created = svc.create_task(task).await.unwrap();
        let done = wait_terminal(&svc, &created.id).await;
        assert_eq!(done.status, "succeeded", "error={:?}", done.error);
        assert_eq!(sink.count.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn openai_chat_text_end_to_end() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "choices": [{"message": {"role": "assistant", "content": "hello from the model"}}]
            })))
            .mount(&server)
            .await;

        let (svc, provider_id, sink, _db) = build(&server.uri()).await;
        let task = NewCreationTask {
            canvas_id: Some("wsc_t".into()),
            node_id: None,
            provider_id: provider_id.clone(),
            model: "gpt-4o-mini".into(),
            capability: "text".into(),
            params: json!({"prompt": "say hi"}),
            inputs: vec![],
        };
        let created = svc.create_task(task).await.unwrap();
        let done = wait_terminal(&svc, &created.id).await;
        assert_eq!(done.status, "succeeded", "error={:?}", done.error);
        assert_eq!(sink.count.load(Ordering::SeqCst), 1);
        let persisted = sink.persisted.lock().unwrap();
        assert_eq!(persisted.len(), 1);
        assert!(persisted[0].0.starts_with("text/plain"), "mime={}", persisted[0].0);
        assert_eq!(String::from_utf8_lossy(&persisted[0].1), "hello from the model");
    }

    #[tokio::test]
    async fn gemini_text_end_to_end() {
        let server = MockServer::start().await;
        // A `gemini`-named model routes to the gemini_text adapter regardless of
        // the seeded platform (routing keys off the model-name substring too).
        Mock::given(method("POST"))
            .and(path("/v1beta/models/gemini-2.5-flash:generateContent"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "candidates": [{"content": {"parts": [{"text": "gemini says "}, {"text": "hi"}]}}]
            })))
            .mount(&server)
            .await;

        let (svc, provider_id, sink, _db) = build(&server.uri()).await;
        let task = NewCreationTask {
            canvas_id: None,
            node_id: None,
            provider_id: provider_id.clone(),
            model: "gemini-2.5-flash".into(),
            capability: "text".into(),
            params: json!({"prompt": "greet me"}),
            inputs: vec![],
        };
        let created = svc.create_task(task).await.unwrap();
        let done = wait_terminal(&svc, &created.id).await;
        assert_eq!(done.status, "succeeded", "error={:?}", done.error);
        let persisted = sink.persisted.lock().unwrap();
        assert_eq!(persisted.len(), 1);
        assert!(persisted[0].0.starts_with("text/plain"), "mime={}", persisted[0].0);
        assert_eq!(String::from_utf8_lossy(&persisted[0].1), "gemini says hi");
    }
}


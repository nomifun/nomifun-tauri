//! The [`MediaProvider`] adapter trait + its submit/poll value types (contract
//! §6 `provider.rs`). Adapters live under `adapters/`.

use async_trait::async_trait;
use serde_json::Value;

use crate::types::{CreationError, MediaCapability};

/// A provider row resolved for one task: the decrypted primary API key + the
/// endpoint shape the adapter needs. Built by the [`crate::CreationService`]
/// from the `providers` row (never by the adapter) so the crypto/DB surface
/// stays in one place.
///
/// Deliberately does **not** derive `Debug` — it holds a decrypted API key that
/// must never land in a log line (mirrors `nomifun-ai-agent`'s
/// `ResolvedProviderFields`).
#[derive(Clone)]
pub struct ResolvedProvider {
    pub provider_id: String,
    /// Provider platform tag (`openai` / `gemini` / `ark` / …) — drives routing.
    pub platform: String,
    /// Raw base URL from the provider row (trailing slash tolerated).
    pub base_url: String,
    /// Decrypted primary API key (first of comma/newline-separated keys).
    pub api_key: String,
    /// When true, `base_url` already carries the version path — adapters append
    /// their operation path without inserting an extra `/v1`.
    pub is_full_url: bool,
}

/// One input asset already loaded to bytes for an adapter (the service resolves
/// `inputs[{asset_id,role}]` → bytes via its [`crate::AssetSource`] before
/// calling `submit`). `role` is a free string
/// (`reference|mask|first_frame|last_frame|video|audio`).
#[derive(Clone)]
pub struct InputAsset {
    pub asset_id: String,
    pub role: String,
    pub bytes: Vec<u8>,
    pub mime: String,
}

/// Everything an adapter needs to run one task: the resolved provider/model,
/// the capability, the opaque parameter map, and the (byte-loaded) inputs.
///
/// No `Debug` — it carries the resolved provider (with its API key) and large
/// input byte buffers.
pub struct SubmitRequest {
    pub provider: ResolvedProvider,
    pub model: String,
    pub capability: MediaCapability,
    /// Opaque parameter snapshot (prompt/size/quality/count/seconds/…).
    pub params: Value,
    pub inputs: Vec<InputAsset>,
}

/// A generated artifact handed back by an adapter: either inline bytes or a
/// URL the engine will fetch.
#[derive(Debug, Clone)]
pub struct ProducedAsset {
    pub data: ProducedData,
    /// MIME of the artifact when the adapter knows it.
    pub mime: Option<String>,
}

#[derive(Debug, Clone)]
pub enum ProducedData {
    Bytes(Vec<u8>),
    Url(String),
}

/// The outcome of [`MediaProvider::submit`]: a synchronous protocol returns the
/// artifacts directly (`Done`); an async submit→poll protocol returns a remote
/// task id (`Pending`) the engine polls.
#[derive(Debug, Clone)]
pub enum SubmitAck {
    Done(Vec<ProducedAsset>),
    Pending { remote_task_id: String },
}

/// The outcome of one [`MediaProvider::poll`] tick.
#[derive(Debug, Clone)]
pub enum PollResult {
    Pending,
    Done(Vec<ProducedAsset>),
    Failed(CreationError),
}

/// A media generation backend. Adapter ids: `openai_images | gemini_image |
/// openai_video | local_image | ark | modelscope | comfyui`.
#[async_trait]
pub trait MediaProvider: Send + Sync {
    /// Stable adapter id (matches the routing tag chosen by the engine).
    fn id(&self) -> &'static str;

    /// Whether this adapter can serve `cap`.
    fn supports(&self, cap: MediaCapability) -> bool;

    /// Kick off the job. `Done` for synchronous protocols; `Pending` for async
    /// submit→poll.
    async fn submit(&self, req: &SubmitRequest) -> Result<SubmitAck, CreationError>;

    /// Poll an async job by its remote id. `req` is the original request (for
    /// re-auth / endpoint reconstruction — inputs may be empty on a boot resume).
    async fn poll(&self, remote_task_id: &str, req: &SubmitRequest) -> Result<PollResult, CreationError>;
}

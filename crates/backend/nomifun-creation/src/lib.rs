//! `nomifun-creation` — the 生成引擎 (media generation engine): the async task
//! queue behind the 创意工坊 canvas's generation nodes.
//!
//! The engine is **provider-agnostic**: a [`MediaProvider`] adapter declares its
//! capabilities and does submit/poll; the [`CreationService`] owns the state
//! machine (`queued → running → succeeded/failed/canceled`), per-provider
//! concurrency + a global cap, cancellation, boot reconciliation, provider-row
//! resolution (endpoint + decrypted key), and hands produced bytes to an
//! [`AssetSink`] (implemented by the app over `nomifun-workshop`, so neither
//! domain crate depends on the other — no cycle). Task inputs are read back
//! through the symmetric [`AssetSource`].
//!
//! Live adapters ([`default_adapters`]): `openai_images` (sync images),
//! `gemini_image` (`:generateContent`), `openai_video` (async submit→poll).
//! `ark` / `modelscope` are P1 stubs. Capability → adapter routing lives in
//! [`route_adapter_id`]. [`default_adapters_with_local_image`] adds an explicitly
//! provisioned Z-Image `sd-cli` backend; this crate never downloads it on its own.

mod adapters;
mod dto;
mod types;

pub mod provider;
pub mod routes;
pub mod service;
pub mod state;

pub use adapters::{default_adapters, default_adapters_with_local_image, route_adapter_id};
pub use adapters::local_image::{
    LOCAL_IMAGE_ADAPTER_ID, LOCAL_Z_IMAGE_TURBO_MODEL_ID, LocalImageAdapter,
    LocalImageBackend, LocalImageRequest, SD_CPP_RUNTIME_ARTIFACTS, SD_CPP_RUNTIME_VERSION,
    SdCliZImageBackend, SdCliZImageConfig, SdCppRuntimeArtifactSpec,
    Z_IMAGE_TURBO_ARTIFACTS, Z_IMAGE_TURBO_CFG_SCALE, Z_IMAGE_TURBO_DOWNLOAD_SIZE,
    Z_IMAGE_TURBO_STEPS, ZImageArtifactRole, ZImageArtifactSpec,
    current_sd_cpp_runtime_artifact,
};
pub use dto::CreationTask;
pub use provider::{
    InputAsset, MediaProvider, PollResult, ProducedAsset, ProducedData, ResolvedProvider, SubmitAck,
    SubmitRequest,
};
pub use routes::creation_routes;
pub use service::{
    AssetSink, AssetSource, CreationService, CreationServiceBuilder, LoadedAsset, NewCreationTask,
    PersistAsset,
};
pub use state::CreationRouterState;
pub use types::{CreationError, CreationInput, MediaCapability, TaskStatus};

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
//! [`route_adapter_id`].

mod adapters;
mod dto;
mod types;

pub mod provider;
pub mod routes;
pub mod service;
pub mod state;

pub use adapters::{default_adapters, route_adapter_id};
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

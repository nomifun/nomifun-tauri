//! `nomifun-workshop` — the 创意工坊 (Creative Workshop) domain: an
//! infinite-canvas AI visual-creation workspace.
//!
//! Two backend crates back the domain: this one owns **canvases + assets**
//! (index rows in `nomifun-db`, canvas bodies + asset binaries on disk under
//! the data dir), while `nomifun-creation` owns the generation task queue.
//!
//! Deliberately mirrors the `nomifun-public-agent` crate shape: `fsio` (atomic
//! temp+rename writes), `service` (the single handle the routes talk to),
//! `state`/`routes` (the `/api/workshop/*` surface). The canvas *doc*
//! (nodes/edges/viewport/settings) is **opaque JSON** to the backend — the
//! service only stores it, caps its size, and derives `node_count` from it; the
//! doc's internal shape is a frontend contract.

mod archive;
mod docscan;
mod dto;
mod fsio;
mod imagemeta;
mod thumbnail;

pub mod agent_ops;
pub mod routes;
pub mod service;
pub mod state;

pub use agent_ops::{AddNodeSpec, AgentOp, AppliedOp, OpDisposition, PendingOp};
pub use dto::{WorkshopAsset, WorkshopCanvasMeta};
pub use routes::{workshop_public_routes, workshop_routes};
pub use service::WorkshopService;
pub use state::WorkshopRouterState;

/// Domain root under the backend data dir. Layout:
/// - `{data_dir}/workshop/canvases/{id}/canvas.json` — canvas body (opaque).
/// - `{data_dir}/workshop/canvases/{id}/thumb.jpg` — canvas gallery thumbnail.
/// - `{data_dir}/workshop/assets/{id}.{ext}` — asset originals.
/// - `{data_dir}/workshop/assets/thumbs/{id}.jpg` — asset thumbnails (JPEG).
pub const WORKSHOP_REL_DIR: &str = "workshop";

/// Max serialized canvas doc size (contract §1: ≤ 8 MB).
pub const MAX_DOC_BYTES: usize = 8 * 1024 * 1024;

/// Max uploaded asset size (contract §3.2: ≤ 64 MB).
pub const MAX_ASSET_BYTES: usize = 64 * 1024 * 1024;

/// The default (empty) canvas doc written on create — a valid, minimal
/// [`WorkshopCanvasDoc`](../../docs) the frontend can load directly. The
/// backend treats it as opaque thereafter.
pub(crate) const DEFAULT_DOC: &str =
    r#"{"schema":1,"viewport":{"x":0,"y":0,"zoom":1},"background":"dots","nodes":[],"edges":[]}"#;

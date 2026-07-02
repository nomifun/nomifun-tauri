//! `nomifun-companion` — the desktop-companion domain: a roster of companions sharing one
//! memory hub (opt-in event collection + scheduled LLM learning that distills
//! events into memories + suggestions), per-companion persona companion chats over
//! the real agent engine, and the companion config/status API surface.
//!
//! Layering: `profile` is the per-companion/shared config split (`config` keeps the
//! legacy single-companion shape for migration only); `registry` is the companion roster;
//! `store` owns the shared sqlite db under `{data_dir}/companion/shared/`;
//! `collector` taps the global event bus and appends JSONL event files;
//! `learner` is the periodic LLM distillation loop; `companion` is the
//! per-companion companion chat; `service` bundles them; `routes`/`state` are the
//! API surface; `migrate` lifts a legacy `companion/nomi/` install into the split.

pub mod collector;
pub mod companion;
pub mod archiver;
pub mod archive_port;
pub mod config;
pub mod events;
pub mod evolution;
pub mod export;
pub mod figure;
pub mod figures;
mod fsio;
pub mod gamify;
pub mod learner;
pub mod matting_model;
pub mod migrate;
pub mod profile;
pub mod prompt;
pub mod registry;
pub mod routes;
pub mod service;
pub mod skill_sink;
pub mod state;
pub mod store;

pub use config::CompanionConfig;
pub use events::CompanionEventEmitter;
pub use figures::FigureMeta;
pub use profile::{CustomFigureMeta, HeadBox, CompanionProfileConfig, CompanionWindowConfig, SharedArchiveConfig, SharedLearnConfig, SharedCompanionConfig};
pub use registry::CompanionRegistry;
pub use routes::{companion_public_routes, companion_routes};
pub use service::CompanionService;
pub use state::CompanionRouterState;
pub use store::CompanionStore;

/// Legacy single-companion directory (under the backend data dir). Kept only so
/// boot can detect and migrate a pre-multi-companion install; new code must use
/// [`COMPANION_SHARED_REL_DIR`] / [`COMPANION_COMPANIONS_REL_DIR`].
pub const COMPANION_REL_DIR: &str = "companion/nomi";

/// Shared multi-companion artifacts (under the backend data dir): shared
/// `config.json`, `events/*.jsonl`, `memory.db`.
pub const COMPANION_SHARED_REL_DIR: &str = "companion/shared";

/// Per-companion profile roots (under the backend data dir): one
/// `{COMPANION_COMPANIONS_REL_DIR}/{companion_id}/config.json` per companion.
pub const COMPANION_COMPANIONS_REL_DIR: &str = "companion/companions";

/// 伙伴工作区树根（`{data_dir}/companion/workspaces`）：与 home 目录解耦的、
/// 见名知意的每伙伴工作目录所在（`{seq}_{净化名}`）。home 目录因注册表扫描约束
/// （目录名==id）不可改名，故工作区另放此树，由 `extra.workspace` 指向。
pub const COMPANION_WORKSPACES_REL_DIR: &str = "companion/workspaces";

/// Cached ML assets shared across companions (under the backend data dir): the
/// MODNet matting model is proxied here once and served from `127.0.0.1`
/// (see [`matting_model`]) so the webview never hits a remote origin or the
/// 30 s in-worker download timeout that made DIY figures unusable.
pub const COMPANION_MODELS_REL_DIR: &str = "companion/models";

/// Shared custom-figure library (under the backend data dir): reusable figures
/// decoupled from any single companion — `{id}.webp` + `index.json` (see
/// [`figures`]).
pub const COMPANION_FIGURES_REL_DIR: &str = "companion/figures";

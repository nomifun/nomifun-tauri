//! `nomifun-public-agent` — the **对外伙伴 / Public Companion** domain: an
//! enterprise-grade agent that safely serves strangers (customer service).
//!
//! Deliberately a SEPARATE first-class domain from `nomifun-companion` (the
//! desktop companion): its own data (`public-agents/{id}/`), its own config
//! model, its own management surface. It shares NO management code with the
//! desktop companion (no growth / skill / character roster / personal memory).
//! Capabilities are narrow-but-deep: Q&A + grounded knowledge-base retrieval,
//! every dangerous tool off. The runtime reuses the platform's safe-execution
//! kernel (the `PublicService` exposure clamp) via `public_agent_id`; only the
//! config source is new.
//!
//! Layering: `config` is the per-agent profile; `registry` is the roster
//! (atomic JSON files + a private seq watermark); `audit` is the append-only,
//! day-partitioned conversation log with day-level retention + search;
//! `service` bundles them; `routes`/`state` are the API surface.

mod fsio;

pub mod audit;
pub mod config;
pub mod provider;
pub mod registry;
pub mod routes;
pub mod service;
pub mod state;

pub use config::{PublicAgentConfig, PublicAgentModel};
pub use registry::PublicAgentRegistry;
pub use routes::public_agent_routes;
pub use service::PublicAgentService;
pub use state::PublicAgentRouterState;

/// Per-agent profile roots (under the backend data dir): one
/// `{PUBLIC_AGENTS_REL_DIR}/{id}/config.json` per public companion, plus its
/// `audit/` sub-tree. Completely separate from `companion/`.
pub const PUBLIC_AGENTS_REL_DIR: &str = "public-agents";

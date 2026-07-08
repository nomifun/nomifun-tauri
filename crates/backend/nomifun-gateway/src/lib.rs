//! `nomifun-gateway` — the Desktop Gateway MCP: an in-process HTTP tool server
//! that exposes the whole Nomi Desktop capability surface (conversations,
//! terminals, cron jobs, global companion memory, requirements, AutoWork, IDMM,
//! knowledge bases, model providers) to agent sessions that carry the
//! backend-set `desktopGateway` extra flag.
//!
//! Governance principle: the companion IS the desktop's universal semantic control
//! surface — every new desktop feature domain ships a companion-operable gateway
//! tool by default (see the gateway design spec appendix).
//!
//! ## Why this exists
//!
//! Remote IM (channel) sessions and companion companion threads act as the user's
//! "master agent": one conversation through which the user can see and drive
//! everything running on the desktop. Agents reach this server through the
//! `nomicore mcp-gateway-stdio` bridge (claude / codex / gemini advertise
//! stdio-only MCP capabilities; the nomi engine consumes the same bridge), and
//! every tool call is forwarded back here as an authenticated `POST /tool`.
//!
//! ## Shape (third instance of the house pattern)
//!
//! Mirrors the requirement MCP server lifecycle: bind `127.0.0.1:0`, mint a
//! per-process random bearer token, late-wire the service dependencies.

pub mod deps;
pub mod registry;
pub mod server;

#[cfg(feature = "browser-use")]
pub mod browser_registry;

#[cfg(feature = "computer-use")]
pub mod computer_registry;

// ── legacy helper modules retained for shared pure logic ─────────────────
// `tools_provider` keeps the nomi model-resolution chain (used by the cron +
// conversation capabilities); `tools_terminal` keeps `preset_launch` (used by
// the terminal capabilities). `tools_browser` is the not-yet-migrated browser
// domain, still dispatched by the legacy match in `server.rs` under coexistence.
mod tools_provider;
mod tools_terminal;

// ── capability domains (registry form) ───────────────────────────────────
// NEW DOMAIN? Adding `mod caps_<x>;` here is step 2 of 3 — also add the
// `crate::caps_<x>::register(&mut caps)` call in `registry/mod.rs::build()`.
// The `all_caps_modules_are_mod_declared_and_registered` test fails CI if a
// file here is missing its register() call (and vice-versa).
mod caps_agent;
mod caps_autowork;
#[cfg(feature = "browser-use")]
mod caps_browser;
mod caps_channel;
mod caps_companion;
#[cfg(feature = "computer-use")]
mod caps_computer;
mod caps_confirmation;
mod caps_conversation;
mod caps_cron;
mod caps_files;
mod caps_idmm;
mod caps_knowledge;
mod caps_knowledge_ext;
mod caps_mcp;
mod caps_memory;
mod caps_orchestrator;
mod caps_provider;
mod caps_requirement;
mod caps_scheduling_ext;
mod caps_system;
mod caps_terminal;
mod caps_terminal_ext;
mod caps_workshop;

pub use deps::{CallerCtx, GatewayDeps};
pub use registry::{Registry, Surface, ToolSpec};
pub use server::GatewayMcpServer;

//! `nomifun-public` — the **Remote 前门** (external companion surface).
//!
//! Projects the platform's single capability source of truth
//! (`nomifun_gateway::Registry`) onto a network-reachable, companion-token-
//! authenticated **MCP Streamable-HTTP** endpoint, so an external AI agent
//! (Claude Code / Cursor / a custom LLM agent) — i.e. an "外部伙伴" — can drive
//! the platform exactly as the desktop companion does, over `Surface::Remote`.
//!
//! This crate is deliberately thin: it owns transport + auth + identity only.
//! Every capability, its schema, its danger tier and its surface gate already
//! live in `nomifun-gateway`; adding a capability there makes it appear here
//! automatically (the inheritance guarantee — see the design spec §2.1). It MUST
//! be mounted in-process by `nomifun-app` (the `server.lock` data-dir is
//! single-writer; a sidecar is impossible).

mod handler;
mod idempotency;
mod rest;
mod result;
mod router;

pub use handler::RemoteMcpHandler;
pub use rest::public_rest_router;
pub use result::build_tool_result;
pub use router::{PublicMcpState, public_mcp_router};

/// Curated "agent" profile for the Remote surface: the do-work capability
/// domains an external task-delegation agent typically needs, excluding
/// platform-management domains (channel/companion/cron/system/team/…). Keeps a
/// remote MCP client's tool list tight (better tool-selection) without changing
/// permissions — dispatch is still gated by the Remote surface, not the profile.
/// (`computer` lights up when the computer-use caps land.)
pub const AGENT_PROFILE_DOMAINS: &[&str] =
    &[
        "agent_execution",
        "agent",
        "remote",
        "conversation",
        "browser",
        "computer",
        "knowledge",
        "files",
        "memory",
    ];

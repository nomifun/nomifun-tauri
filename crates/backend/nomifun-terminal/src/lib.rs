//! Terminal sessions: PTY-backed interactive sessions managed alongside
//! conversations. Owns a `portable-pty` per session, streams output over the
//! realtime WebSocket broadcaster, and persists session metadata in SQLite.

pub mod ansi;
pub mod driver;
pub mod enhance;
pub mod error;
pub mod events;
pub mod lifecycle;
pub mod pty;
pub mod routes;
pub mod service;
pub mod state;
pub mod types;

pub use ansi::{AnsiLineScanner, strip_ansi};
pub use driver::{TerminalDescription, TerminalDriver};
pub use enhance::{apply_enhancement, resolve_agent_family, terminal_autowork_capable, AgentCli, LifecycleHookWiring, McpServerSpec, TerminalLaunchEnhancement};
pub use events::TerminalEventEmitter;
pub use lifecycle::{LifecycleKind, TerminalLifecycleEvent, TerminalLifecycleServer};
pub use routes::terminal_routes;
pub use service::{TerminalService, TerminalSupervisionHook};
pub use state::TerminalRouterState;

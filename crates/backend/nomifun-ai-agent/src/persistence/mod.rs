//! Persistence consumers — subscribers that mirror in-memory state to
//! durable storage (SQLite) without carrying business semantics.
//!
//! Today this layer only holds [`acp_session_sync`], which drains
//! `AcpSessionEvent`s from a running ACP agent into the
//! `acp_session.session_config.runtime` row.

pub mod acp_session_sync;

pub use acp_session_sync::{AcpSessionPersistenceBarrier, AcpSessionSyncService};

//! External channel integration: plugin system, pairing handshake, and per-session messaging.
pub mod action;
pub mod channel_settings;
pub mod constants;
pub mod error;
pub mod formatter;
pub mod manager;
pub mod media_refs;
pub mod message_service;
pub mod orchestrator;
pub mod pairing;
pub mod pending_decision;
pub mod plugin;
pub mod plugins;
pub mod routes;
pub mod session;
pub mod stream_relay;
pub mod think_filter;
pub mod types;

pub use routes::{ChannelRouterState, channel_routes};

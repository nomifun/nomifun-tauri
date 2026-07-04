//! Application crate: assembles all domain crates into an Axum server with DI and middleware.
//!
//! This file is a public façade — it only re-exports symbols defined in
//! submodules. All logic lives in the modules below.

mod config;
mod provider_deletion;
mod router;
mod services;

// Promoted from the `nomicore` bin so in-process hosts (Tauri desktop, web)
// can boot the backend as a library — no spawned binary.
pub mod bootstrap;
pub mod cli;
pub mod commands;
pub mod desktop;
pub mod mcp_endpoints;

pub use config::{AppConfig, derive_encryption_key};
pub use desktop::{DesktopKeepAlive, DesktopServer, WebUiStatus};
pub use nomifun_auth::AuthPolicy;
// Re-export the build channel so in-process hosts (the Tauri desktop shell)
// reach it through their existing `nomifun-app` dependency without adding a
// direct `nomifun-common` dep.
pub use nomifun_common::channel;
pub use router::{
    ChannelOrchestratorComponents, ModuleStates, build_assistant_state, build_conversation_state,
    build_extension_states, build_module_states, build_ws_state, create_router, create_router_with_all_state,
    create_router_with_states,
};
pub use services::AppServices;

/// In-process server entry used by embedded hosts (Tauri desktop, `nomifun-web`)
/// and by the `nomicore` bin's default path. Builds environment → data layer →
/// services, then serves until shutdown. For a host that also serves static
/// assets (the web SPA), compose `create_router` + your fallback instead.
pub async fn run_embedded_server(cli: &cli::Cli, merged_path: &str) -> anyhow::Result<std::process::ExitCode> {
    let env = bootstrap::init_environment(cli, merged_path)?;
    let database = bootstrap::init_data_layer(&env.config).await?;
    let services = AppServices::from_config(database, &env.config).await?;
    commands::run_server(env, services).await
}

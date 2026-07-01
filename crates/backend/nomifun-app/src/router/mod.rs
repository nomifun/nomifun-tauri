//! HTTP router assembly for the application.

pub mod companion_token_routes;
#[cfg(feature = "browser-use")]
pub(crate) mod browser_login;
mod computer_permissions;
mod health;
mod model_failover;
mod routes;
mod state;
mod trace;

pub use routes::{create_router, create_router_with_all_state, create_router_with_states};
pub use state::{
    ChannelOrchestratorComponents, ModuleStates, build_assistant_state, build_conversation_state,
    build_extension_states, build_module_states, build_ws_state,
};

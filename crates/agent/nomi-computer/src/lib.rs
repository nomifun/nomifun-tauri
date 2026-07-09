pub mod fallback_backend;
pub mod input;
pub mod keys;
pub mod launch;
mod macos_main;
pub mod permissions;
pub mod scale;
pub mod screen;
pub mod tool;

pub use tool::ComputerTool;

// Re-exported so a host that pulls computer-use (but not nomi-a11y directly)
// can brand the permission-error guidance with its own app name — see
// `nomi_a11y::set_host_app_label`.
pub use nomi_a11y::{host_app_label, set_host_app_label};

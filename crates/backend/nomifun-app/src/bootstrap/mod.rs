//! Process-level bootstrap helpers for the binary.
//!
//! These are *not* subcommands — they are layered initialization steps
//! (logging, work_dir resolution, builtin-skill materialization, database
//! init) that subcommands compose to start the application.

mod admin;
mod bind;
mod boot_log;
mod builtin_skills;
mod environment;
mod relocation;
mod server_lock;
mod tracing_init;
mod webui_dist;
mod work_dir;

pub use admin::{AdminBootstrap, ensure_admin_credentials};
pub use bind::{PORT_FILE, PortAnnouncement, SCAN_SPAN, announce_bound_port, bind_with_fallback, write_port_file};
pub use boot_log::{BootNoteLevel, record_boot_note};
pub use environment::{ServerEnvironment, init_data_layer, init_environment};
pub use relocation::{RELOCATED_DONE_MARKER, RELOCATED_FROM_MARKER, RelocationMarker, rewrite_relocated_paths};
pub use server_lock::{SERVER_LOCK_FILE, ServerLock};
pub use webui_dist::{
    UI_BUILD_MANIFEST_FILE, UI_BUILD_MANIFEST_SCHEMA, UiBuildManifest, ui_api_contract_version,
    validate_webui_dist,
};
pub(crate) use work_dir::resolve_work_dir;

/// Acquire the canonical data-directory lock for an offline maintenance
/// command. Kept separate from `init_environment` so backup can avoid logging,
/// factory-reset processing, and all other server boot side effects.
pub fn acquire_offline_server_lock(data_dir: &std::path::Path) -> anyhow::Result<ServerLock> {
    server_lock::acquire_server_lock(data_dir)
}

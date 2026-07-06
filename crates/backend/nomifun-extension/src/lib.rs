//! Extension registry: manifest parsing, hub installer, skill scanning, and lifecycle hooks.

mod asset_paths;
pub mod classifier;
pub mod constants;
pub mod dependency;
pub mod error;
pub mod external_paths;
pub mod hub;
pub mod hub_routes;
pub mod lifecycle;
pub mod loader;
pub mod manifest;
pub mod permission;
pub mod registry;
mod registry_helpers;
pub mod resolvers;
pub mod routes;
pub mod skill_routes;
pub mod skill_service;
pub mod startup_materialize;
pub mod state;
pub mod template;
pub mod types;
pub mod watcher;

pub use classifier::{AssistantClassifier, AssistantRuleDispatcher};
pub use constants::*;
pub use dependency::{DependencyIssue, DependencyValidationResult, validate_dependencies};
pub use error::ExtensionError;
pub use lifecycle::{HookKind, execute_hook, needs_install_hook, resolve_hook_path};
pub use loader::{
    ScanPath, filter_by_engine_compatibility, load_all, resolve_install_target_dir_for_data_dir, resolve_scan_paths,
    resolve_scan_paths_for_data_dir,
};
pub use manifest::{parse_manifest, validate_manifest};
pub use permission::{build_permission_summary, calculate_risk_level};
pub use registry::{ExtensionRegistry, ExtensionSummary};
pub use resolvers::{resolve_all_contributions, resolve_extension_contributions, resolve_i18n_for_all};
pub use startup_materialize::materialize_if_needed;
pub use state::{ExtensionStateStore, load_states_from_file, resolve_state_file_path, save_states_to_file};
pub use template::{resolve_env_map, resolve_env_templates, resolve_file_reference};
pub use types::*;
pub use watcher::ExtensionWatcher;

pub use external_paths::ExternalPathsManager;
pub use hub::{HubIndexManager, HubInstaller};
pub use hub_routes::{HubRouterState, hub_routes};
pub use routes::{ExtensionRouterState, extension_routes};
pub use skill_routes::{SkillRouterState, skill_routes};
pub use skill_service::{
    BUILTIN_SKILLS_ENV_VAR, BuiltinAutoSkillItem, ExternalSkillSource, NamedPath, ResolvedAgentSkill, ScannedSkill,
    SkillListItem, SkillPaths, SkillSource, builtin_skills_corpus, delete_skill, detect_and_count_external_skills,
    detect_common_skill_paths, export_skill_with_symlink, get_skill_paths, import_skill, import_skill_with_symlink,
    link_workspace_skills, list_available_skills, list_builtin_auto_skills, materialize_skills_for_agent,
    read_builtin_rule, read_builtin_skill, read_skill_info, resolve_skill_paths, scan_for_skills,
};
pub use skill_service::{builtin_skills_corpus_fingerprint, builtin_skills_materialize_version};
pub use skill_service::{
    delete_assistant_rule, delete_assistant_skill, read_assistant_rule, read_assistant_skill, write_assistant_rule,
    write_assistant_skill,
};

//! System services: provider management, model fetching, settings, and version checks.
pub mod bedrock_probe;
pub mod asr_model;
pub mod client_pref;
pub mod image_model;
pub mod managed_model;
pub mod local_model;
pub mod local_model_runtime;
pub mod model_classify;
pub mod model_fetcher;
pub mod model_profile;
pub mod protocol;
pub mod provider;
pub mod provider_deletion;
pub mod routes;
pub mod settings;
pub mod sysinfo;
pub mod version;

pub use bedrock_probe::{ConnectionTestRouterState, ConnectionTestService, connection_test_routes};
pub use asr_model::{
    AsrModelService, LocalAsrTranscription, asr_model_catalog, inactive_asr_model_status,
};
pub use client_pref::ClientPrefService;
pub use image_model::{
    ImageModelService, image_model_catalog, inactive_image_model_status,
};
pub use managed_model::{
    DEFAULT_FREE_REFRESH_INTERVAL, FREE_MODEL_PROVIDER_ID, LOCAL_MODEL_PROVIDER_ID,
    ManagedModelRefreshPolicy, ManagedModelRefreshTask, ManagedModelServer,
    ManagedModelService, is_managed_provider_identity, start_and_provision_free_model,
    start_and_provision_free_model_with_preferences,
};
pub use local_model::{
    LocalModelServer, LocalModelService, disable_local_model_provider,
    inactive_local_model_status, local_model_catalog, start_and_provision_local_model,
};
pub use local_model_runtime::LazyLocalModelRuntime;
pub use model_classify::{ModelGenerationSuggestion, suggest_generation_capabilities};
pub use model_fetcher::ModelFetchService;
pub use model_profile::{
    ModelProfileService, reconcile_local_catalog_profiles, seed_missing_inferred_profiles,
};
pub use protocol::ProtocolDetectionService;
pub use provider::ProviderService;
pub use provider_deletion::{ProviderDeletionCoordinator, SharedProviderDeletionCoordinator};
pub use routes::{SystemRouterState, settings_routes, system_routes};
pub use settings::SettingsService;
pub use version::VersionCheckService;

//! System services: provider management, model fetching, settings, and version checks.
pub mod bedrock_probe;
pub mod client_pref;
pub mod model_fetcher;
pub mod protocol;
pub mod provider;
pub mod provider_deletion;
pub mod routes;
pub mod settings;
pub mod sysinfo;
pub mod version;

pub use bedrock_probe::{ConnectionTestRouterState, ConnectionTestService, connection_test_routes};
pub use client_pref::ClientPrefService;
pub use model_fetcher::ModelFetchService;
pub use protocol::ProtocolDetectionService;
pub use provider::ProviderService;
pub use provider_deletion::{ProviderDeletionCoordinator, SharedProviderDeletionCoordinator};
pub use routes::{SystemRouterState, settings_routes, system_routes};
pub use settings::SettingsService;
pub use version::VersionCheckService;

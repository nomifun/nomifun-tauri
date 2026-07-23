use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

use nomifun_api_types::WebSocketMessage;
use nomifun_common::{TimestampMs, now_ms};
use nomifun_realtime::EventBroadcaster;
use serde_json::json;
use tokio::sync::RwLock;
use tracing::{debug, info, warn};

use crate::error::ExtensionError;
use crate::lifecycle::{HookKind, execute_hook, needs_install_hook, resolve_hook_path};
use crate::loader::{ScanPath, resolve_scan_paths};
use crate::registry_helpers::{
    build_state_map, load_and_validate, merge_persisted_states, run_deactivation_hooks, to_summary,
};
use crate::resolvers::{resolve_all_contributions, resolve_i18n_for_all};
use crate::state::ExtensionStateStore;
use crate::types::{
    ExtensionLifecyclePayload, ExtensionState, ExtensionSystemEvent, LoadedExtension, ResolvedAcpAdapter,
    ResolvedAgent, ResolvedPreset, ResolvedChannelPlugin, ResolvedContributions, ResolvedModelProvider,
    ResolvedSettingsTab, ResolvedSkill, ResolvedTheme, WebuiContribution,
};

// Re-export ExtensionSummary from registry_helpers so that
// `registry::{ExtensionRegistry, ExtensionSummary}` continues to work.
pub use crate::registry_helpers::ExtensionSummary;

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

/// Central registry coordinating extension loading, activation, contribution
/// resolution, and event broadcasting.
///
/// Thread-safe: can be shared across HTTP handlers, the file watcher, and
/// other async tasks via `Arc`.
#[derive(Clone)]
pub struct ExtensionRegistry {
    inner: Arc<RwLock<RegistryInner>>,
    state_store: ExtensionStateStore,
    broadcaster: Arc<dyn EventBroadcaster>,
    app_version: String,
}

struct RegistryInner {
    extensions: Vec<LoadedExtension>,
    contributions: ResolvedContributions,
    scan_paths: Vec<ScanPath>,
    initialized: bool,
}

// ---------------------------------------------------------------------------
// Construction
// ---------------------------------------------------------------------------

impl ExtensionRegistry {
    /// Create a new registry.
    ///
    /// - `state_store`: persists enabled/disabled states across restarts.
    /// - `broadcaster`: pushes WebSocket events to connected clients.
    /// - `app_version`: current application version for engine compatibility.
    pub fn new(state_store: ExtensionStateStore, broadcaster: Arc<dyn EventBroadcaster>, app_version: String) -> Self {
        Self {
            inner: Arc::new(RwLock::new(RegistryInner {
                extensions: Vec::new(),
                contributions: ResolvedContributions::default(),
                scan_paths: Vec::new(),
                initialized: false,
            })),
            state_store,
            broadcaster,
            app_version,
        }
    }
}

// ---------------------------------------------------------------------------
// Initialization
// ---------------------------------------------------------------------------

impl ExtensionRegistry {
    /// Run the full initialization pipeline using auto-detected scan paths.
    ///
    /// Resolves scan paths from environment variables and platform defaults,
    /// then delegates to [`Self::initialize_with_scan_paths`].
    pub async fn initialize(&self) -> Result<(), ExtensionError> {
        let scan_paths = resolve_scan_paths();
        self.initialize_with_scan_paths(scan_paths).await
    }

    /// Run the full initialization pipeline with explicit scan paths.
    ///
    /// Prefer this over [`Self::initialize`] when the caller already knows
    /// the extension directories (e.g., in tests or embedded deployments).
    ///
    /// Pipeline:
    /// 1. Load manifests from all directories
    /// 2. Filter by engine compatibility
    /// 3. Validate dependencies + topological sort
    /// 4. Merge persisted states (enabled/disabled)
    /// 5. Run lifecycle hooks (onInstall if needed, then onActivate)
    /// 6. Resolve all contributions
    /// 7. Persist updated states
    pub async fn initialize_with_scan_paths(&self, scan_paths: Vec<ScanPath>) -> Result<(), ExtensionError> {
        info!("initializing extension registry");
        debug!(count = scan_paths.len(), "resolved scan paths");

        // 1-3. Load, filter, validate (all sync/blocking).
        let (extensions, dep_result) = load_and_validate(&scan_paths, &self.app_version);

        // 4. Merge persisted states.
        let persisted = self.state_store.load().await?;
        let extensions = merge_persisted_states(extensions, &persisted);

        // 5. Run lifecycle hooks.
        let extensions = self.run_activation_hooks(extensions, &persisted).await;

        // 6. Resolve contributions.
        let contributions = resolve_all_contributions(&extensions);

        // 7. Persist updated states.
        let states = build_state_map(&extensions);
        self.state_store.set_all(states).await;

        // Commit to inner state.
        {
            let mut guard = self.inner.write().await;
            guard.extensions = extensions;
            guard.contributions = contributions;
            guard.scan_paths = scan_paths;
            guard.initialized = true;
        }

        if !dep_result.issues.is_empty() {
            warn!(issues = dep_result.issues.len(), "dependency validation found issues");
        }

        info!("extension registry initialized");
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Hot reload
// ---------------------------------------------------------------------------

impl ExtensionRegistry {
    /// Hot-reload the registry: deactivate all -> clear -> reload -> re-resolve.
    ///
    /// Emits `REGISTRY_RELOADED` event when complete.
    pub async fn hot_reload(&self) {
        info!("hot-reloading extension registry");

        // 1. Snapshot current extensions and scan paths for deactivation.
        let (current_exts, scan_paths) = {
            let guard = self.inner.read().await;
            (guard.extensions.clone(), guard.scan_paths.clone())
        };

        // 2. Run onDeactivate hooks for each currently active extension.
        run_deactivation_hooks(&current_exts).await;

        // 3. Reload pipeline (same as initialize but reuses existing scan paths).
        let (extensions, _dep_result) = load_and_validate(&scan_paths, &self.app_version);

        // Use in-memory state (not file) to preserve pending writes that
        // haven't been flushed yet by the debounce timer.
        let persisted = self.state_store.get_all().await;

        let extensions = merge_persisted_states(extensions, &persisted);
        let extensions = self.run_activation_hooks(extensions, &persisted).await;
        let contributions = resolve_all_contributions(&extensions);

        let states = build_state_map(&extensions);
        self.state_store.set_all(states).await;

        // 4. Commit new state.
        {
            let mut guard = self.inner.write().await;
            guard.extensions = extensions;
            guard.contributions = contributions;
            // scan_paths stay the same
        }

        // 5. Broadcast REGISTRY_RELOADED event.
        self.broadcast_lifecycle_event("registry", ExtensionSystemEvent::RegistryReloaded, None);

        info!("extension registry hot-reloaded");
    }
}

// ---------------------------------------------------------------------------
// Enable / Disable
// ---------------------------------------------------------------------------

impl ExtensionRegistry {
    /// Enable an extension by name.
    ///
    /// Updates the in-memory state, re-resolves contributions, persists the
    /// change, and broadcasts `extensions.state-changed`.
    pub async fn enable_extension(&self, name: &str) -> Result<(), ExtensionError> {
        let state = {
            let mut guard = self.inner.write().await;

            let idx = guard
                .extensions
                .iter()
                .position(|e| e.manifest.name == name)
                .ok_or_else(|| ExtensionError::NotFound(name.to_owned()))?;

            if guard.extensions[idx].state.enabled {
                debug!(name, "extension already enabled");
                return Ok(());
            }

            guard.extensions[idx].state.enabled = true;
            guard.extensions[idx].state.last_activated_at = Some(now_ms());

            // Re-resolve contributions with updated enabled set.
            guard.contributions = resolve_all_contributions(&guard.extensions);

            guard.extensions[idx].state.clone()
        };

        // Persist + broadcast outside the write lock.
        self.state_store.set(state).await;
        self.broadcast_state_changed(name, true);

        info!(name, "extension enabled");
        Ok(())
    }

    /// Disable an extension by name.
    ///
    /// Optionally records a reason (logged for auditing). Updates state,
    /// re-resolves contributions, persists, and broadcasts
    /// `extensions.state-changed`.
    pub async fn disable_extension(&self, name: &str, reason: Option<&str>) -> Result<(), ExtensionError> {
        let state = {
            let mut guard = self.inner.write().await;

            let idx = guard
                .extensions
                .iter()
                .position(|e| e.manifest.name == name)
                .ok_or_else(|| ExtensionError::NotFound(name.to_owned()))?;

            if !guard.extensions[idx].state.enabled {
                debug!(name, "extension already disabled");
                return Ok(());
            }

            guard.extensions[idx].state.enabled = false;

            // Re-resolve contributions with updated enabled set.
            guard.contributions = resolve_all_contributions(&guard.extensions);

            guard.extensions[idx].state.clone()
        };

        // Persist + broadcast outside the write lock.
        self.state_store.set(state).await;
        self.broadcast_state_changed(name, false);

        if let Some(r) = reason {
            info!(name, reason = r, "extension disabled");
        } else {
            info!(name, "extension disabled");
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Query methods
// ---------------------------------------------------------------------------

impl ExtensionRegistry {
    /// Return summaries of all loaded extensions.
    pub async fn get_loaded_extensions(&self) -> Vec<ExtensionSummary> {
        let guard = self.inner.read().await;
        guard.extensions.iter().map(to_summary).collect()
    }

    pub(crate) fn event_broadcaster(&self) -> Arc<dyn EventBroadcaster> {
        self.broadcaster.clone()
    }

    /// Look up a single loaded extension by name.
    pub async fn get_extension_by_name(&self, name: &str) -> Option<LoadedExtension> {
        let guard = self.inner.read().await;
        guard.extensions.iter().find(|e| e.manifest.name == name).cloned()
    }

    /// Snapshot of all resolved contributions.
    pub async fn get_contributions(&self) -> ResolvedContributions {
        let guard = self.inner.read().await;
        guard.contributions.clone()
    }

    pub async fn get_themes(&self) -> Vec<ResolvedTheme> {
        let guard = self.inner.read().await;
        guard.contributions.themes.clone()
    }

    pub async fn get_presets(&self) -> Vec<ResolvedPreset> {
        let guard = self.inner.read().await;
        guard.contributions.presets.clone()
    }

    /// Return `true` if any extension contributes a preset with this source key.
    pub async fn has_preset(&self, source_key: &str) -> bool {
        let guard = self.inner.read().await;
        guard
            .contributions
            .presets
            .iter()
            .any(|preset| preset.source_key == source_key)
    }

    /// Lookup a single extension-contributed preset by global source key.
    pub async fn get_preset_by_source_key(&self, source_key: &str) -> Option<ResolvedPreset> {
        let guard = self.inner.read().await;
        guard
            .contributions
            .presets
            .iter()
            .find(|preset| preset.source_key == source_key)
            .cloned()
    }

    pub async fn get_acp_adapters(&self) -> Vec<ResolvedAcpAdapter> {
        let guard = self.inner.read().await;
        guard.contributions.acp_adapters.clone()
    }

    pub async fn get_agents(&self) -> Vec<ResolvedAgent> {
        let guard = self.inner.read().await;
        guard.contributions.agents.clone()
    }

    pub async fn get_mcp_servers(&self) -> Vec<crate::types::ResolvedMcpServer> {
        let guard = self.inner.read().await;
        guard.contributions.mcp_servers.clone()
    }

    pub async fn get_skills(&self) -> Vec<ResolvedSkill> {
        let guard = self.inner.read().await;
        guard.contributions.skills.clone()
    }

    pub async fn get_settings_tabs(&self) -> Vec<ResolvedSettingsTab> {
        let guard = self.inner.read().await;
        guard.contributions.settings_tabs.clone()
    }

    pub async fn get_webui_contributions(&self) -> Vec<WebuiContribution> {
        let guard = self.inner.read().await;
        guard.contributions.webui.clone()
    }

    pub async fn get_channel_plugins(&self) -> Vec<ResolvedChannelPlugin> {
        let guard = self.inner.read().await;
        guard.contributions.channel_plugins.clone()
    }

    pub async fn get_model_providers(&self) -> Vec<ResolvedModelProvider> {
        let guard = self.inner.read().await;
        guard.contributions.model_providers.clone()
    }

    /// Resolve i18n data for a given locale across all enabled extensions.
    pub async fn get_i18n_for_locale(&self, locale: &str) -> HashMap<String, HashMap<String, String>> {
        let guard = self.inner.read().await;
        resolve_i18n_for_all(&guard.extensions, locale)
    }

    /// Whether the registry has been initialized.
    pub async fn is_initialized(&self) -> bool {
        let guard = self.inner.read().await;
        guard.initialized
    }
}

// ---------------------------------------------------------------------------
// Event broadcasting helpers
// ---------------------------------------------------------------------------

impl ExtensionRegistry {
    fn broadcast_state_changed(&self, name: &str, enabled: bool) {
        let event = WebSocketMessage::new("extensions.state-changed", json!({ "name": name, "enabled": enabled }));
        self.broadcaster.broadcast(event);
    }

    fn broadcast_lifecycle_event(
        &self,
        extension_name: &str,
        event: ExtensionSystemEvent,
        data: Option<serde_json::Value>,
    ) {
        let payload = ExtensionLifecyclePayload {
            extension_name: extension_name.to_owned(),
            event,
            timestamp: now_ms(),
            data,
        };
        let msg = WebSocketMessage::new(
            "extensions.lifecycle",
            serde_json::to_value(&payload).unwrap_or_default(),
        );
        self.broadcaster.broadcast(msg);
    }
}

// ---------------------------------------------------------------------------
// Activation hooks
// ---------------------------------------------------------------------------

impl ExtensionRegistry {
    /// Run lifecycle hooks for each extension in order:
    /// - `onInstall` if first time or version changed
    /// - `onActivate` for each enabled extension
    ///
    /// Hook failures are logged but do not prevent other extensions from
    /// activating.
    async fn run_activation_hooks(
        &self,
        mut extensions: Vec<LoadedExtension>,
        persisted: &HashMap<String, ExtensionState>,
    ) -> Vec<LoadedExtension> {
        let now: TimestampMs = now_ms();

        for ext in &mut extensions {
            if !ext.state.enabled {
                continue;
            }

            let ext_name = ext.manifest.name.clone();
            let ext_dir = Path::new(&ext.directory);

            // Check onInstall + onActivate hooks.
            if let Some(hooks) = &ext.manifest.lifecycle {
                let persisted_version = persisted.get(&ext_name).map(|s| s.version.as_str());

                if needs_install_hook(&ext.manifest.version, persisted_version)
                    && let Some(hook_path) = resolve_hook_path(hooks, HookKind::OnInstall)
                    && let Err(e) = execute_hook(ext_dir, hook_path, HookKind::OnInstall, &ext_name).await
                {
                    warn!(
                        extension = %ext_name,
                        error = %e,
                        "onInstall hook failed, continuing"
                    );
                }

                // Run onActivate
                if let Some(hook_path) = resolve_hook_path(hooks, HookKind::OnActivate)
                    && let Err(e) = execute_hook(ext_dir, hook_path, HookKind::OnActivate, &ext_name).await
                {
                    warn!(
                        extension = %ext_name,
                        error = %e,
                        "onActivate hook failed, continuing"
                    );
                }
            }

            // Update activation timestamp and install time.
            ext.state.last_activated_at = Some(now);
            if ext.state.installed_at.is_none() {
                ext.state.installed_at = Some(now);
            }

            self.broadcast_lifecycle_event(&ext_name, ExtensionSystemEvent::ExtensionActivated, None);
        }

        extensions
    }
}

// ---------------------------------------------------------------------------
// Unit tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{ExtensionManifest, ExtensionSource, ExtensionState};
    use nomifun_realtime::BroadcastEventBus;

    fn make_test_ext(name: &str, enabled: bool) -> LoadedExtension {
        LoadedExtension {
            manifest: ExtensionManifest {
                name: name.to_owned(),
                version: "1.0.0".to_owned(),
                display_name: Some(format!("{name} Display")),
                description: Some(format!("{name} description")),
                author: None,
                license: None,
                homepage: None,
                icon: None,
                engine: None,
                api_version: None,
                dependencies: HashMap::new(),
                entry_point: None,
                permissions: None,
                contributes: None,
                lifecycle: None,
                i18n: None,
            },
            directory: format!("/tmp/ext/{name}"),
            source: ExtensionSource::Local,
            state: ExtensionState {
                name: name.to_owned(),
                version: "1.0.0".to_owned(),
                enabled,
                installed_at: Some(1000),
                last_activated_at: None,
            },
        }
    }

    fn make_registry() -> (ExtensionRegistry, ExtensionStateStore, Arc<BroadcastEventBus>) {
        let tmp = tempfile::TempDir::new().unwrap();
        let store = ExtensionStateStore::new(tmp.path().join("states.json"));
        let bus = Arc::new(BroadcastEventBus::new(64));
        let registry = ExtensionRegistry::new(store.clone(), bus.clone(), "1.0.0".to_owned());
        (registry, store, bus)
    }

    // -- enable_extension / disable_extension -----------------------------------

    #[tokio::test]
    async fn enable_nonexistent_returns_not_found() {
        let (registry, _, _) = make_registry();
        let result = registry.enable_extension("no-such-ext").await;
        assert!(matches!(result, Err(ExtensionError::NotFound(_))));
    }

    #[tokio::test]
    async fn disable_nonexistent_returns_not_found() {
        let (registry, _, _) = make_registry();
        let result = registry.disable_extension("no-such-ext", None).await;
        assert!(matches!(result, Err(ExtensionError::NotFound(_))));
    }

    #[tokio::test]
    async fn enable_disable_roundtrip() {
        let (registry, _, bus) = make_registry();

        // Seed the registry with a disabled extension.
        {
            let mut guard = registry.inner.write().await;
            guard.extensions = vec![make_test_ext("test-ext", false)];
            guard.initialized = true;
        }

        let mut rx = bus.subscribe();

        // Enable
        registry.enable_extension("test-ext").await.unwrap();
        {
            let guard = registry.inner.read().await;
            assert!(guard.extensions[0].state.enabled);
        }

        let msg = rx.recv().await.unwrap();
        assert_eq!(msg.name, "extensions.state-changed");
        assert_eq!(msg.data["enabled"], true);

        // Disable
        registry
            .disable_extension("test-ext", Some("test reason"))
            .await
            .unwrap();
        {
            let guard = registry.inner.read().await;
            assert!(!guard.extensions[0].state.enabled);
        }

        let msg = rx.recv().await.unwrap();
        assert_eq!(msg.name, "extensions.state-changed");
        assert_eq!(msg.data["enabled"], false);
    }

    #[tokio::test]
    async fn enable_already_enabled_is_noop() {
        let (registry, _, _) = make_registry();
        {
            let mut guard = registry.inner.write().await;
            guard.extensions = vec![make_test_ext("ext", true)];
        }
        // Should succeed without error.
        registry.enable_extension("ext").await.unwrap();
    }

    #[tokio::test]
    async fn disable_already_disabled_is_noop() {
        let (registry, _, _) = make_registry();
        {
            let mut guard = registry.inner.write().await;
            guard.extensions = vec![make_test_ext("ext", false)];
        }
        registry.disable_extension("ext", None).await.unwrap();
    }

    // -- query methods --------------------------------------------------------

    #[tokio::test]
    async fn get_loaded_extensions_returns_summaries() {
        let (registry, _, _) = make_registry();
        {
            let mut guard = registry.inner.write().await;
            guard.extensions = vec![make_test_ext("ext-a", true), make_test_ext("ext-b", false)];
        }

        let summaries = registry.get_loaded_extensions().await;
        assert_eq!(summaries.len(), 2);
        assert_eq!(summaries[0].name, "ext-a");
        assert!(summaries[0].enabled);
        assert_eq!(summaries[1].name, "ext-b");
        assert!(!summaries[1].enabled);
    }

    #[tokio::test]
    async fn get_extension_by_name_found_and_not_found() {
        let (registry, _, _) = make_registry();
        {
            let mut guard = registry.inner.write().await;
            guard.extensions = vec![make_test_ext("my-ext", true)];
        }

        assert!(registry.get_extension_by_name("my-ext").await.is_some());
        assert!(registry.get_extension_by_name("nope").await.is_none());
    }

    #[tokio::test]
    async fn is_initialized_before_and_after() {
        let (registry, _, _) = make_registry();
        assert!(!registry.is_initialized().await);

        {
            let mut guard = registry.inner.write().await;
            guard.initialized = true;
        }
        assert!(registry.is_initialized().await);
    }
}

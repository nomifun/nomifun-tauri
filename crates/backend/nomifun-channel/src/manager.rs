use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, Instant};

use dashmap::DashMap;
use nomifun_api_types::{PluginStatusChangedPayload, PluginStatusResponse, WebSocketMessage};
use nomifun_common::{decrypt_string, encrypt_string, generate_prefixed_id, now_ms};
use nomifun_db::models::ChannelPluginRow;
use nomifun_db::{IChannelRepository, UpdatePluginStatusParams};
use nomifun_realtime::EventBroadcaster;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tracing::{debug, error, info, warn};

use crate::constants::{
    WATCHDOG_BACKOFF_BASE, WATCHDOG_MAX_RESTARTS_PER_WINDOW, WATCHDOG_RESTART_WINDOW, WATCHDOG_SWEEP_INTERVAL,
};
use crate::error::{ChannelError, ChannelOwner};
use crate::plugin::{ChannelPlugin, PluginCallbacks};
use crate::types::{ChannelIncoming, PluginConfig, PluginStatus, PluginType, UnifiedIncomingMessage, bot_key_for};

/// Manages the lifecycle of channel plugins.
///
/// One plugin instance per `assistant_plugins` row — multiple bots may run
/// on the same platform simultaneously (each bound to its own companion).
///
/// Responsibilities:
/// - Loading enabled plugins from DB on startup (`restore_plugins`)
/// - Enabling/disabling plugins (with credential encryption)
/// - Enforcing bot identity uniqueness (one bot ↔ at most one companion)
/// - Testing plugin credentials without persisting
/// - Broadcasting status change events via WebSocket
/// - Holding active plugin instances in a concurrent map
///
/// Plugin instances are stored as `Box<dyn ChannelPlugin>` behind a
/// `DashMap` for lock-free concurrent access.
pub struct ChannelManager {
    repo: Arc<dyn IChannelRepository>,
    broadcaster: Arc<dyn EventBroadcaster>,
    encryption_key: [u8; 32],
    /// Active plugin instances keyed by `assistant_plugins.id` (row id, not
    /// platform — legacy rows keep `id == platform`).
    plugins: DashMap<String, Box<dyn ChannelPlugin>>,
    /// Sender for incoming messages from all plugins, stamped with their
    /// channel row id. The `ChannelOrchestrator` holds the receiving end.
    message_tx: mpsc::Sender<ChannelIncoming>,
    /// Sender for tool confirmation callbacks from all plugins.
    confirm_tx: mpsc::Sender<(String, String)>,
}

/// Factory function type for creating plugin instances.
///
/// Platform-specific implementations register their factory via
/// `ChannelManager::enable_plugin`. The factory is called with a
/// `PluginType` and returns a boxed trait object.
///
/// This keeps the manager decoupled from concrete plugin types —
/// platform implementations are behind feature flags.
pub type PluginFactory = Box<dyn Fn(PluginType) -> Option<Box<dyn ChannelPlugin>> + Send + Sync>;

/// How a channel row is addressed by `enable_plugin`.
///
/// - `plugin_id` set: update that row (legacy callers pass the platform
///   name, which doubles as the legacy row id).
/// - `plugin_id` unset + `plugin_type` set: create a new row with a
///   generated `achn_` id — this is the per-companion multi-bot path.
/// - `companion_id`: bind the bot to a companion; `None` keeps the row's binding.
/// - `public_agent_id`: bind the bot to a 对外伙伴 (public agent); `None` keeps
///   the row's binding. Row-level MUTUALLY EXCLUSIVE with `companion_id` — an
///   explicit non-empty value for one clears the other on the persisted row.
#[derive(Debug, Default, Clone)]
pub struct EnableChannelSpec {
    pub plugin_id: Option<String>,
    pub plugin_type: Option<String>,
    pub companion_id: Option<String>,
    pub public_agent_id: Option<String>,
}

impl EnableChannelSpec {
    /// Legacy addressing: plugin id == platform name.
    pub fn legacy(plugin_id: &str) -> Self {
        Self {
            plugin_id: Some(plugin_id.to_owned()),
            ..Default::default()
        }
    }
}

/// Tunables for the plugin health watchdog.
///
/// Production values come from `constants.rs`; tests override them to avoid
/// real waits.
#[derive(Debug, Clone)]
pub struct WatchdogConfig {
    /// Interval between health sweeps.
    pub sweep_interval: Duration,
    /// Sliding window for restart-rate limiting.
    pub restart_window: Duration,
    /// Maximum restart attempts per plugin within `restart_window`.
    pub max_restarts_per_window: u32,
    /// Base delay for exponential backoff between restart attempts
    /// (delay after the n-th attempt = base * 2^(n-1)).
    pub backoff_base: Duration,
}

impl Default for WatchdogConfig {
    fn default() -> Self {
        Self {
            sweep_interval: WATCHDOG_SWEEP_INTERVAL,
            restart_window: WATCHDOG_RESTART_WINDOW,
            max_restarts_per_window: WATCHDOG_MAX_RESTARTS_PER_WINDOW,
            backoff_base: WATCHDOG_BACKOFF_BASE,
        }
    }
}

/// Restart bookkeeping carried across watchdog sweeps.
#[derive(Default)]
pub struct WatchdogState {
    /// Restart attempt timestamps per plugin (pruned to the sliding window).
    attempts: HashMap<String, Vec<Instant>>,
    /// Plugins whose last restart failed. They are no longer in the live
    /// plugin map, so subsequent sweeps must re-check them from here.
    retry_queue: HashSet<String>,
}

impl WatchdogState {
    /// Whether a restart attempt is allowed now under the rate limit and
    /// exponential backoff. Records the attempt when allowed.
    fn allow_attempt(&mut self, plugin_id: &str, config: &WatchdogConfig, now: Instant) -> bool {
        let attempts = self.attempts.entry(plugin_id.to_owned()).or_default();
        attempts.retain(|t| now.duration_since(*t) < config.restart_window);

        if attempts.len() as u32 >= config.max_restarts_per_window {
            return false;
        }
        if let Some(last) = attempts.last() {
            // Exponential backoff: base * 2^(n-1) after the n-th attempt.
            // Cap the shift so a pathological window/base combination can't
            // overflow the multiplier.
            let exponent = (attempts.len() as u32 - 1).min(16);
            let backoff = config.backoff_base.saturating_mul(1u32 << exponent);
            if now.duration_since(*last) < backoff {
                return false;
            }
        }
        attempts.push(now);
        true
    }
}

impl ChannelManager {
    /// Creates a new `ChannelManager`.
    ///
    /// # Arguments
    ///
    /// - `repo`: Data access for plugin configuration persistence
    /// - `broadcaster`: WebSocket event broadcaster for status updates
    /// - `encryption_key`: 32-byte AES-256-GCM key for credential encryption
    /// - `message_tx`: Channel sender for routing incoming messages
    /// - `confirm_tx`: Channel sender for tool confirmation callbacks
    pub fn new(
        repo: Arc<dyn IChannelRepository>,
        broadcaster: Arc<dyn EventBroadcaster>,
        encryption_key: [u8; 32],
        message_tx: mpsc::Sender<ChannelIncoming>,
        confirm_tx: mpsc::Sender<(String, String)>,
    ) -> Self {
        Self {
            repo,
            broadcaster,
            encryption_key,
            plugins: DashMap::new(),
            message_tx,
            confirm_tx,
        }
    }

    /// Returns the status of all registered plugins from the database.
    ///
    /// Merges DB state with live runtime status for active plugins.
    pub async fn get_plugin_status(&self) -> Result<Vec<PluginStatusResponse>, ChannelError> {
        let rows = self.repo.get_all_plugins().await?;
        let statuses: Vec<PluginStatusResponse> = rows
            .into_iter()
            .map(|row| {
                let live_status = self.plugins.get(&row.id).map(|p| p.status().to_string());
                self.row_to_status_response(&row, live_status)
            })
            .collect();
        Ok(statuses)
    }

    /// Enables a bot channel: validates config, enforces bot identity
    /// uniqueness, encrypts credentials, persists to DB, and starts the
    /// plugin connection. Returns the channel row id.
    ///
    /// If the channel is already running, it will be stopped first and
    /// restarted with the new configuration.
    pub async fn enable_plugin(
        &self,
        spec: &EnableChannelSpec,
        config_value: &serde_json::Value,
        factory: &PluginFactory,
    ) -> Result<String, ChannelError> {
        // Resolve the target row: existing row id > legacy platform-named
        // create > explicit-type create with a generated id.
        let existing = match spec.plugin_id.as_deref() {
            Some(id) => self.repo.get_plugin(id).await?,
            None => None,
        };
        let (mut row_id, plugin_type, mut created_at, mut prior_companion, mut prior_public_agent) =
            match (&existing, spec.plugin_id.as_deref()) {
                (Some(row), _) => {
                    let pt = PluginType::from_str_opt(&row.r#type)
                        .ok_or_else(|| ChannelError::InvalidPluginType(row.r#type.clone()))?;
                    (
                        row.id.clone(),
                        pt,
                        row.created_at,
                        row.companion_id.clone(),
                        row.public_agent_id.clone(),
                    )
                }
                (None, Some(id)) => {
                    let pt =
                        PluginType::from_str_opt(id).ok_or_else(|| ChannelError::InvalidPluginType(id.to_owned()))?;
                    (id.to_owned(), pt, now_ms(), None, None)
                }
                (None, None) => {
                    let type_str = spec
                        .plugin_type
                        .as_deref()
                        .ok_or_else(|| ChannelError::InvalidConfig("plugin_type is required to create a channel".into()))?;
                    let pt = PluginType::from_str_opt(type_str)
                        .ok_or_else(|| ChannelError::InvalidPluginType(type_str.to_owned()))?;
                    (generate_prefixed_id("achn"), pt, now_ms(), None, None)
                }
            };

        // Parse and validate config structure
        let config = self.plugin_config_from_request(config_value, existing.as_ref())?;

        // One bot, one live binding: the same bot identity on another row would
        // let a second companion/public agent answer for it. A row that failed
        // before its first successful connection is only a stale draft; reuse it
        // instead of reporting a false "already connected" conflict.
        let bot_key = bot_key_for(plugin_type, &config.credentials);
        if let Some(key) = bot_key.as_deref() {
            let clash = self
                .repo
                .get_all_plugins()
                .await?
                .into_iter()
                .find(|r| r.id != row_id && r.r#type == plugin_type.to_string() && r.bot_key.as_deref() == Some(key));
            if let Some(other) = clash {
                if existing.is_none()
                    && spec.plugin_id.is_none()
                    && self.can_reuse_identity_row_for_spec(&other, spec)
                {
                    info!(
                        plugin_id = %other.id,
                        plugin_type = %plugin_type,
                        bot_key = %key,
                        "reusing existing channel row for bot identity"
                    );
                    row_id = other.id.clone();
                    created_at = other.created_at;
                    prior_companion = other.companion_id;
                    prior_public_agent = other.public_agent_id;
                } else {
                    return Err(ChannelError::BotAlreadyBound(Self::bound_owner(&other)));
                }
            }
        }

        // Stop existing plugin if running
        if self.plugins.contains_key(&row_id) {
            self.stop_plugin(&row_id).await;
        }

        // Encrypt config for storage
        let config_json = serde_json::to_string(&config)?;
        let encrypted_config = encrypt_string(&config_json, &self.encryption_key)
            .map_err(|e| ChannelError::EncryptionFailed(e.to_string()))?;

        // Persist to DB. Row-level mutual exclusivity: a bot serves EITHER a
        // companion OR a 对外伙伴 (public agent) OR nothing. An explicit non-empty
        // binding for one side clears the other; when the spec sets neither, the
        // row's prior (already-exclusive) bindings are preserved.
        let spec_companion = spec
            .companion_id
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty());
        let spec_public_agent = spec
            .public_agent_id
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty());
        let (companion_id, public_agent_id) = if let Some(pa) = spec_public_agent {
            (None, Some(pa.to_owned()))
        } else if let Some(comp) = spec_companion {
            (Some(comp.to_owned()), None)
        } else {
            (prior_companion, prior_public_agent)
        };
        let row = ChannelPluginRow {
            id: row_id.clone(),
            r#type: plugin_type.to_string(),
            name: self.default_plugin_name(plugin_type),
            enabled: true,
            config: encrypted_config,
            status: Some(PluginStatus::Created.to_string()),
            last_connected: None,
            companion_id,
            public_agent_id,
            bot_key,
            created_at,
            updated_at: now_ms(),
        };
        self.repo.upsert_plugin(&row).await?;

        // Create and start plugin instance
        let mut plugin = factory(plugin_type)
            .ok_or_else(|| ChannelError::InvalidPluginType(format!("No implementation for {plugin_type}")))?;

        let callbacks = self.plugin_callbacks(&row_id);

        if let Err(e) = plugin.initialize(config, callbacks).await {
            self.update_plugin_error(&row_id, &e.to_string()).await;
            self.broadcast_status_change(&row_id).await;
            return Err(e);
        }

        if let Err(e) = plugin.start().await {
            self.update_plugin_error(&row_id, &e.to_string()).await;
            self.broadcast_status_change(&row_id).await;
            return Err(e);
        }

        // Update DB with running status
        let params = UpdatePluginStatusParams {
            status: Some(PluginStatus::Running.to_string()),
            last_connected: Some(now_ms()),
            enabled: None,
        };
        self.repo.update_plugin_status(&row_id, &params).await?;

        // Store active instance
        self.plugins.insert(row_id.clone(), plugin);

        info!(plugin_id = %row_id, plugin_type = %plugin_type, "plugin enabled and started");
        self.broadcast_status_change(&row_id).await;
        Ok(row_id)
    }

    /// Enables an extension-contributed plugin in metadata-only mode.
    ///
    /// The backend does not yet execute extension channel runtime JS, but we
    /// still persist the plugin configuration and enabled flag so Settings UI
    /// can behave consistently and survive restarts.
    pub async fn enable_extension_plugin(
        &self,
        plugin_id: &str,
        plugin_name: &str,
        config: &PluginConfig,
    ) -> Result<(), ChannelError> {
        if self.plugins.contains_key(plugin_id) {
            self.stop_plugin(plugin_id).await;
        }

        let config_json = serde_json::to_string(config)?;
        let encrypted_config = encrypt_string(&config_json, &self.encryption_key)
            .map_err(|e| ChannelError::EncryptionFailed(e.to_string()))?;

        let now = now_ms();
        let existing = self.repo.get_plugin(plugin_id).await?;
        let row = ChannelPluginRow {
            id: plugin_id.to_owned(),
            r#type: plugin_id.to_owned(),
            name: plugin_name.to_owned(),
            enabled: true,
            config: encrypted_config,
            status: Some(PluginStatus::Stopped.to_string()),
            last_connected: existing.as_ref().and_then(|row| row.last_connected),
            companion_id: existing.as_ref().and_then(|row| row.companion_id.clone()),
            public_agent_id: existing.as_ref().and_then(|row| row.public_agent_id.clone()),
            bot_key: existing.as_ref().and_then(|row| row.bot_key.clone()),
            created_at: existing.as_ref().map(|row| row.created_at).unwrap_or(now),
            updated_at: now,
        };
        self.repo.upsert_plugin(&row).await?;

        info!(plugin_id = %plugin_id, "extension plugin enabled (metadata-only mode)");
        self.broadcast_status_change(plugin_id).await;
        Ok(())
    }

    /// Disables a plugin: stops the connection, updates DB, and removes
    /// the active instance.
    ///
    /// Idempotent — disabling an already-disabled plugin is a no-op.
    pub async fn disable_plugin(&self, plugin_id: &str) -> Result<(), ChannelError> {
        // Stop running instance if any
        self.stop_plugin(plugin_id).await;

        // Update DB
        let params = UpdatePluginStatusParams {
            status: Some(PluginStatus::Stopped.to_string()),
            last_connected: None,
            enabled: Some(false),
        };
        self.repo.update_plugin_status(plugin_id, &params).await?;

        info!(plugin_id = %plugin_id, "plugin disabled");
        self.broadcast_status_change(plugin_id).await;
        Ok(())
    }

    /// Deletes a channel row entirely: stops the running instance, clears
    /// its sessions, removes the row, and broadcasts the change. The bot's
    /// conversations survive (they belong to the conversation domain).
    pub async fn delete_channel(&self, plugin_id: &str) -> Result<(), ChannelError> {
        let row = self
            .repo
            .get_plugin(plugin_id)
            .await?
            .ok_or_else(|| ChannelError::PluginNotFound(plugin_id.to_owned()))?;

        self.stop_plugin(plugin_id).await;
        self.repo.delete_sessions_by_channel(plugin_id).await?;
        self.repo.delete_plugin(plugin_id).await?;

        // The row is gone, so synthesize the final "stopped" view for the
        // broadcast instead of reading it back.
        let mut response = self.row_to_status_response(&row, None);
        response.enabled = false;
        response.connected = false;
        response.status = Some(PluginStatus::Stopped.to_string());
        self.broadcast_status_response(plugin_id, response);

        info!(plugin_id = %plugin_id, "channel deleted");
        Ok(())
    }

    /// Rebinds (or clears) the companion of a channel row and clears only that
    /// channel's sessions so the next inbound message recreates its
    /// conversation under the new binding.
    pub async fn rebind_channel_companion(&self, plugin_id: &str, companion_id: Option<&str>) -> Result<(), ChannelError> {
        self.repo.update_plugin_companion(plugin_id, companion_id).await?;
        self.repo.delete_sessions_by_channel(plugin_id).await?;
        self.broadcast_status_change(plugin_id).await;
        info!(plugin_id = %plugin_id, companion_id = ?companion_id, "channel companion rebound; channel sessions cleared");
        Ok(())
    }

    /// Rebinds (or clears) the 对外伙伴 (public agent) of a channel row and clears
    /// only that channel's sessions so the next inbound message recreates its
    /// conversation under the new binding.
    ///
    /// Row-level mutual exclusivity: setting a non-null `public_agent_id` also
    /// clears that bot's `companion_id` (enforced in the repository layer), so a
    /// bot serves EITHER a companion OR a public agent, never both.
    pub async fn rebind_channel_public_agent(
        &self,
        plugin_id: &str,
        public_agent_id: Option<&str>,
    ) -> Result<(), ChannelError> {
        self.repo.update_plugin_public_agent(plugin_id, public_agent_id).await?;
        self.repo.delete_sessions_by_channel(plugin_id).await?;
        self.broadcast_status_change(plugin_id).await;
        info!(plugin_id = %plugin_id, public_agent_id = ?public_agent_id, "channel public agent rebound; channel sessions cleared");
        Ok(())
    }

    /// Clears the channel sessions of every bot bound to `companion_id`, so each
    /// such channel recreates its backing conversation — and thus re-resolves
    /// the (now changed) 伙伴模型 — on the next inbound message.
    ///
    /// Called when a 伙伴 (companion) 的 profile.model 变更后，main 在 services.rs
    /// 接线触发，让已绑定 bot 的 channel 会话下轮重建拾取新模型。
    ///
    /// Best-effort: enumerates `assistant_plugins` rows whose `companion_id` matches
    /// (no per-companion repo query exists, so list-all + filter), then clears each
    /// channel via the same `delete_sessions_by_channel` primitive that
    /// `rebind_channel_companion` / `delete_channel` use. Failures are logged and
    /// skipped rather than aborting the whole sweep.
    pub async fn clear_sessions_for_companion(&self, companion_id: &str) {
        let companion_id = companion_id.trim();
        if companion_id.is_empty() {
            return;
        }

        let rows = match self.repo.get_all_plugins().await {
            Ok(rows) => rows,
            Err(e) => {
                warn!(companion_id = %companion_id, error = %e, "clear_sessions_for_companion: failed to list plugins");
                return;
            }
        };

        let mut cleared = 0usize;
        for row in rows
            .into_iter()
            .filter(|r| r.companion_id.as_deref().map(str::trim) == Some(companion_id))
        {
            match self.repo.delete_sessions_by_channel(&row.id).await {
                Ok(()) => cleared += 1,
                Err(e) => {
                    warn!(
                        plugin_id = %row.id,
                        companion_id = %companion_id,
                        error = %e,
                        "clear_sessions_for_companion: failed to clear channel sessions"
                    );
                }
            }
        }

        info!(companion_id = %companion_id, channels = cleared, "cleared channel sessions for companion model change");
    }

    /// Clears any channel-row companion bindings that point at a deleted
    /// companion, then clears those channels' sessions. This prevents ghost
    /// `companion_id`s from reserving bot identities after the owner is gone.
    pub async fn unbind_channels_for_deleted_companion(&self, companion_id: &str) {
        let companion_id = companion_id.trim();
        if companion_id.is_empty() {
            return;
        }

        let rows = match self.repo.get_all_plugins().await {
            Ok(rows) => rows,
            Err(e) => {
                warn!(companion_id = %companion_id, error = %e, "unbind_channels_for_deleted_companion: failed to list plugins");
                return;
            }
        };

        let mut unbound = 0usize;
        for row in rows
            .into_iter()
            .filter(|r| r.companion_id.as_deref().map(str::trim) == Some(companion_id))
        {
            if let Err(e) = self.repo.update_plugin_companion(&row.id, None).await {
                warn!(
                    plugin_id = %row.id,
                    companion_id = %companion_id,
                    error = %e,
                    "unbind_channels_for_deleted_companion: failed to clear binding"
                );
                continue;
            }
            if let Err(e) = self.repo.delete_sessions_by_channel(&row.id).await {
                warn!(
                    plugin_id = %row.id,
                    companion_id = %companion_id,
                    error = %e,
                    "unbind_channels_for_deleted_companion: failed to clear channel sessions"
                );
            }
            self.broadcast_status_change(&row.id).await;
            unbound += 1;
        }

        info!(companion_id = %companion_id, channels = unbound, "unbound channels for deleted companion");
    }

    /// Startup self-heal: unbinds any channel row whose owner (companion or
    /// public agent) is no longer in the live roster, then clears that row's
    /// sessions. A companion/public-agent deleted BEFORE the delete-hook existed
    /// (or missed by it) leaves a ghost `companion_id`/`public_agent_id` that
    /// keeps reserving the bot identity (UNIQUE(type,bot_key)), so re-enabling
    /// the same bot under a live owner fails with `BotAlreadyBound` forever. An
    /// unbound row is adoptable again on the next enable (the `can_reuse`
    /// both-owners-empty branch), so this rescues rows orphaned in the past.
    /// Best-effort: per-row failures are logged and skipped.
    pub async fn reconcile_orphaned_owners(
        &self,
        live_companions: &HashSet<String>,
        live_public_agents: &HashSet<String>,
    ) {
        let rows = match self.repo.get_all_plugins().await {
            Ok(rows) => rows,
            Err(e) => {
                warn!(error = %e, "reconcile_orphaned_owners: failed to list plugins");
                return;
            }
        };

        let mut healed = 0usize;
        for row in rows {
            let orphan_companion = matches!(
                row.companion_id.as_deref().map(str::trim).filter(|s| !s.is_empty()),
                Some(c) if !live_companions.contains(c)
            );
            let orphan_public_agent = matches!(
                row.public_agent_id.as_deref().map(str::trim).filter(|s| !s.is_empty()),
                Some(p) if !live_public_agents.contains(p)
            );
            if !orphan_companion && !orphan_public_agent {
                continue;
            }

            if orphan_companion {
                if let Err(e) = self.repo.update_plugin_companion(&row.id, None).await {
                    warn!(plugin_id = %row.id, error = %e, "reconcile_orphaned_owners: failed to clear companion binding");
                    continue;
                }
            }
            if orphan_public_agent {
                if let Err(e) = self.repo.update_plugin_public_agent(&row.id, None).await {
                    warn!(plugin_id = %row.id, error = %e, "reconcile_orphaned_owners: failed to clear public-agent binding");
                    continue;
                }
            }
            if let Err(e) = self.repo.delete_sessions_by_channel(&row.id).await {
                warn!(plugin_id = %row.id, error = %e, "reconcile_orphaned_owners: failed to clear channel sessions");
            }
            self.broadcast_status_change(&row.id).await;
            healed += 1;
        }

        if healed > 0 {
            info!(channels = healed, "reconcile_orphaned_owners: unbound channels whose owner no longer exists");
        }
    }

    /// Tests plugin credentials without persisting.
    ///
    /// Creates a temporary plugin instance, initializes it with the
    /// provided credentials, and checks if the connection succeeds.
    /// Returns the bot username on success.
    pub async fn test_plugin(
        &self,
        plugin_id: &str,
        config: PluginConfig,
        factory: &PluginFactory,
    ) -> Result<Option<String>, ChannelError> {
        let plugin_type =
            PluginType::from_str_opt(plugin_id).ok_or_else(|| ChannelError::InvalidPluginType(plugin_id.to_owned()))?;

        let mut plugin = factory(plugin_type)
            .ok_or_else(|| ChannelError::InvalidPluginType(format!("No implementation for {plugin_type}")))?;

        // Create throwaway channels for the test
        let (msg_tx, _msg_rx) = mpsc::channel(1);
        let (confirm_tx, _confirm_rx) = mpsc::channel(1);
        let callbacks = PluginCallbacks {
            message_tx: msg_tx,
            confirm_tx,
        };

        plugin.initialize(config, callbacks).await?;

        let bot_username = plugin.bot_info().and_then(|b| b.username.clone());

        // Clean up — don't leave a started connection
        debug!(plugin_id = %plugin_id, "plugin credential test successful");
        Ok(bot_username)
    }

    /// Restores previously enabled plugins on startup.
    ///
    /// Reads all plugins from DB, backfills missing `bot_key`s (the
    /// migration cannot compute them — they live inside the encrypted
    /// config), then decrypts and starts the enabled ones. Errors on
    /// individual plugins are logged but don't prevent other plugins
    /// from starting.
    pub async fn restore_plugins(&self, factory: &PluginFactory) -> Result<(), ChannelError> {
        let rows = self.repo.get_all_plugins().await?;

        for row in &rows {
            if row.bot_key.is_none() {
                self.backfill_bot_key(row).await;
            }
        }

        let enabled: Vec<ChannelPluginRow> = rows.into_iter().filter(|r| r.enabled).collect();

        if enabled.is_empty() {
            debug!("no enabled plugins to restore");
            return Ok(());
        }

        info!(count = enabled.len(), "restoring enabled plugins");

        for row in enabled {
            if PluginType::from_str_opt(&row.r#type).is_none() {
                info!(
                    plugin_id = %row.id,
                    plugin_type = %row.r#type,
                    "skipping extension plugin runtime restore; metadata-only mode"
                );
                self.broadcast_status_change(&row.id).await;
                continue;
            }
            if let Err(e) = self.restore_single_plugin(&row, factory).await {
                warn!(
                    plugin_id = %row.id,
                    error = %e,
                    "failed to restore plugin, marking as error"
                );
                self.update_plugin_error(&row.id, &e.to_string()).await;
                self.broadcast_status_change(&row.id).await;
            }
        }

        Ok(())
    }

    /// Gracefully stops all active plugin connections.
    ///
    /// Called during application shutdown.
    pub async fn shutdown(&self) {
        let keys: Vec<String> = self.plugins.iter().map(|entry| entry.key().clone()).collect();

        for key in keys {
            self.stop_plugin(&key).await;
        }
        info!("all plugins shut down");
    }

    /// Returns the number of currently active (in-memory) plugins.
    pub fn active_plugin_count(&self) -> usize {
        self.plugins.len()
    }

    /// Checks whether a specific plugin is currently running.
    pub fn is_plugin_running(&self, plugin_id: &str) -> bool {
        self.plugins
            .get(plugin_id)
            .map(|p| p.status() == PluginStatus::Running)
            .unwrap_or(false)
    }

    /// Sends a message through a specific plugin.
    ///
    /// Used by the `ChannelMessageService` to route outgoing messages
    /// to the correct platform plugin.
    pub async fn send_message(
        &self,
        plugin_id: &str,
        chat_id: &str,
        message: crate::types::UnifiedOutgoingMessage,
    ) -> Result<String, ChannelError> {
        let plugin = self
            .plugins
            .get(plugin_id)
            .ok_or_else(|| ChannelError::PluginNotFound(plugin_id.to_owned()))?;
        plugin.send_message(chat_id, message).await
    }

    /// Edits an existing message through a specific plugin.
    pub async fn edit_message(
        &self,
        plugin_id: &str,
        chat_id: &str,
        message_id: &str,
        message: crate::types::UnifiedOutgoingMessage,
    ) -> Result<(), ChannelError> {
        let plugin = self
            .plugins
            .get(plugin_id)
            .ok_or_else(|| ChannelError::PluginNotFound(plugin_id.to_owned()))?;
        plugin.edit_message(chat_id, message_id, message).await
    }

    // ── Watchdog ─────────────────────────────────────────────────────

    /// Spawns the periodic plugin health watchdog.
    ///
    /// Background receive loops (Telegram long-polling, Lark/DingTalk
    /// WebSocket) give up after exhausting their reconnect budget and mark
    /// the plugin `Error` via its shared status cell — but nothing else in
    /// the system would notice: the DB row and the frontend would keep
    /// claiming the plugin is running. The watchdog closes that gap: every
    /// sweep it persists the real status, broadcasts
    /// `channel.plugin-status-changed`, and attempts a rate-limited restart.
    pub fn spawn_watchdog(self: &Arc<Self>, factory: Arc<PluginFactory>, config: WatchdogConfig) -> JoinHandle<()> {
        let manager = Arc::clone(self);
        tokio::spawn(async move {
            let mut state = WatchdogState::default();
            let mut ticker = tokio::time::interval(config.sweep_interval);
            // The first tick of a tokio interval completes immediately —
            // consume it so the first sweep happens one full interval after
            // startup (restore_plugins may still be in flight).
            ticker.tick().await;
            loop {
                ticker.tick().await;
                manager.check_and_heal_plugins(&factory, &config, &mut state).await;
            }
        })
    }

    /// One watchdog sweep: detect dead plugins, persist/broadcast their
    /// real status, and attempt rate-limited restarts.
    ///
    /// Public so tests can drive sweeps deterministically without the timer.
    pub async fn check_and_heal_plugins(
        &self,
        factory: &PluginFactory,
        config: &WatchdogConfig,
        state: &mut WatchdogState,
    ) {
        // Live instances whose background loop flagged itself dead. Collect
        // the keys first — DashMap shard guards must not be held across the
        // awaits below.
        let mut dead: Vec<(String, bool)> = self
            .plugins
            .iter()
            .filter(|entry| entry.value().status() == PluginStatus::Error)
            .map(|entry| (entry.key().clone(), true))
            .collect();

        // Plugins whose previous restart failed: they are no longer in the
        // live map (stop_plugin removed them), so they can only be retried
        // from the carried-over state.
        for plugin_id in std::mem::take(&mut state.retry_queue) {
            if !dead.iter().any(|(id, _)| id == &plugin_id) {
                dead.push((plugin_id, false));
            }
        }

        for (plugin_id, freshly_dead) in dead {
            // Skip plugins that were disabled or deleted in the meantime —
            // the user explicitly turned them off, nothing to heal.
            let enabled = match self.repo.get_plugin(&plugin_id).await {
                Ok(Some(row)) => row.enabled,
                Ok(None) => false,
                Err(e) => {
                    warn!(plugin_id = %plugin_id, error = %e, "watchdog: failed to read plugin row, will retry");
                    state.retry_queue.insert(plugin_id);
                    continue;
                }
            };
            if !enabled {
                state.attempts.remove(&plugin_id);
                continue;
            }

            if freshly_dead {
                warn!(plugin_id = %plugin_id, "watchdog: plugin background loop reported Error");
                // Keep DB + frontend truthful even if the restart below is
                // rate-limited or fails.
                self.update_plugin_error(&plugin_id, "background loop exited unexpectedly")
                    .await;
                self.broadcast_status_change(&plugin_id).await;
            }

            if !state.allow_attempt(&plugin_id, config, Instant::now()) {
                debug!(plugin_id = %plugin_id, "watchdog: restart suppressed by backoff / hourly budget");
                state.retry_queue.insert(plugin_id);
                continue;
            }

            match self.restart_plugin(&plugin_id, factory).await {
                Ok(()) => {
                    info!(plugin_id = %plugin_id, "watchdog: plugin restarted");
                }
                Err(e) => {
                    warn!(plugin_id = %plugin_id, error = %e, "watchdog: plugin restart failed");
                    self.update_plugin_error(&plugin_id, &e.to_string()).await;
                    self.broadcast_status_change(&plugin_id).await;
                    state.retry_queue.insert(plugin_id);
                }
            }
        }
    }

    /// Full restart cycle for one plugin: stop/remove the dead instance,
    /// then re-run the restore path (decrypt config → initialize → start).
    async fn restart_plugin(&self, plugin_id: &str, factory: &PluginFactory) -> Result<(), ChannelError> {
        let row = self
            .repo
            .get_plugin(plugin_id)
            .await?
            .ok_or_else(|| ChannelError::PluginNotFound(plugin_id.to_owned()))?;

        self.stop_plugin(plugin_id).await;
        self.restore_single_plugin(&row, factory).await
    }

    // ── Private helpers ──────────────────────────────────────────────

    /// Parses request config, or reuses the row's saved encrypted config when
    /// callers re-enable an existing channel with an empty `{}` payload.
    fn plugin_config_from_request(
        &self,
        config_value: &serde_json::Value,
        existing: Option<&ChannelPluginRow>,
    ) -> Result<PluginConfig, ChannelError> {
        let empty_object = config_value.as_object().map(|m| m.is_empty()).unwrap_or(false);
        if !empty_object {
            return serde_json::from_value(config_value.clone())
                .map_err(|e| ChannelError::InvalidConfig(format!("Invalid config: {e}")));
        }

        let row = existing.ok_or_else(|| ChannelError::InvalidConfig("credentials are required".into()))?;
        let config_json = decrypt_string(&row.config, &self.encryption_key)
            .map_err(|e| ChannelError::DecryptionFailed(e.to_string()))?;
        serde_json::from_str(&config_json)
            .map_err(|e| ChannelError::InvalidConfig(format!("Stored config is invalid: {e}")))
    }

    /// Decide whether a create-mode enable should adopt an existing row with
    /// the same bot identity instead of trying to insert a duplicate.
    ///
    /// Reuse is correct when the row already belongs to the requested owner
    /// (the UI simply lost the row id), when the row is unbound, or when it is
    /// only a failed draft that never reached a successful connection.
    fn can_reuse_identity_row_for_spec(&self, row: &ChannelPluginRow, spec: &EnableChannelSpec) -> bool {
        let requested_companion = spec
            .companion_id
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty());
        let requested_public_agent = spec
            .public_agent_id
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty());
        let row_companion = row.companion_id.as_deref().map(str::trim).filter(|s| !s.is_empty());
        let row_public_agent = row.public_agent_id.as_deref().map(str::trim).filter(|s| !s.is_empty());

        if let Some(companion_id) = requested_companion
            && row_companion == Some(companion_id)
            && row_public_agent.is_none()
        {
            return true;
        }
        if let Some(public_agent_id) = requested_public_agent
            && row_public_agent == Some(public_agent_id)
            && row_companion.is_none()
        {
            return true;
        }
        if row_companion.is_none() && row_public_agent.is_none() {
            return true;
        }

        !self.plugins.contains_key(&row.id)
            && row.last_connected.is_none()
            && row.status.as_deref() != Some("running")
    }

    fn bound_owner(row: &ChannelPluginRow) -> ChannelOwner {
        if let Some(companion) = row.companion_id.as_deref().filter(|p| !p.is_empty()) {
            ChannelOwner::Companion(companion.to_owned())
        } else if let Some(public_agent) = row.public_agent_id.as_deref().filter(|p| !p.is_empty()) {
            ChannelOwner::PublicAgent(public_agent.to_owned())
        } else {
            ChannelOwner::Channel(row.id.clone())
        }
    }

    /// Per-instance callbacks: a forwarder task stamps every message this
    /// plugin emits with its channel row id (plugins stay channel-agnostic).
    /// The forwarder exits when the plugin instance (and all its sender
    /// clones) is dropped after `stop_plugin`.
    fn plugin_callbacks(&self, channel_id: &str) -> PluginCallbacks {
        let (tx, mut rx) = mpsc::channel::<UnifiedIncomingMessage>(64);
        let main_tx = self.message_tx.clone();
        let channel_id = channel_id.to_owned();
        tokio::spawn(async move {
            while let Some(message) = rx.recv().await {
                let incoming = ChannelIncoming {
                    channel_id: channel_id.clone(),
                    message,
                };
                if main_tx.send(incoming).await.is_err() {
                    break;
                }
            }
        });
        PluginCallbacks {
            message_tx: tx,
            confirm_tx: self.confirm_tx.clone(),
        }
    }

    /// Backfills `bot_key` for a pre-migration row by decrypting its config.
    /// Best-effort: failures only log (the watchdog/UI keep working, the
    /// uniqueness check just can't protect this row until it succeeds).
    async fn backfill_bot_key(&self, row: &ChannelPluginRow) {
        let Some(plugin_type) = PluginType::from_str_opt(&row.r#type) else {
            return; // extension plugins have no builtin bot identity
        };
        let Ok(config_json) = decrypt_string(&row.config, &self.encryption_key) else {
            warn!(plugin_id = %row.id, "bot_key backfill: cannot decrypt config");
            return;
        };
        let Ok(config) = serde_json::from_str::<PluginConfig>(&config_json) else {
            warn!(plugin_id = %row.id, "bot_key backfill: cannot parse config");
            return;
        };
        if let Some(key) = bot_key_for(plugin_type, &config.credentials)
            && let Err(e) = self.repo.update_plugin_bot_key(&row.id, &key).await
        {
            warn!(plugin_id = %row.id, error = %e, "bot_key backfill failed");
        }
    }

    /// Stops and removes an active plugin instance.
    async fn stop_plugin(&self, plugin_id: &str) {
        if let Some((_, mut plugin)) = self.plugins.remove(plugin_id) {
            if let Err(e) = plugin.stop().await {
                warn!(
                    plugin_id = %plugin_id,
                    error = %e,
                    "error stopping plugin"
                );
            }
            debug!(plugin_id = %plugin_id, "plugin stopped");
        }
    }

    /// Restores a single plugin from its DB row.
    async fn restore_single_plugin(&self, row: &ChannelPluginRow, factory: &PluginFactory) -> Result<(), ChannelError> {
        let plugin_type =
            PluginType::from_str_opt(&row.r#type).ok_or_else(|| ChannelError::InvalidPluginType(row.r#type.clone()))?;

        // Decrypt config
        let config_json = decrypt_string(&row.config, &self.encryption_key)
            .map_err(|e| ChannelError::DecryptionFailed(e.to_string()))?;
        let config: PluginConfig = serde_json::from_str(&config_json)?;

        let mut plugin = factory(plugin_type)
            .ok_or_else(|| ChannelError::InvalidPluginType(format!("No implementation for {plugin_type}")))?;

        let callbacks = self.plugin_callbacks(&row.id);

        plugin.initialize(config, callbacks).await?;
        plugin.start().await?;

        // Update DB with running status
        let params = UpdatePluginStatusParams {
            status: Some(PluginStatus::Running.to_string()),
            last_connected: Some(now_ms()),
            enabled: None,
        };
        self.repo.update_plugin_status(&row.id, &params).await?;

        self.plugins.insert(row.id.clone(), plugin);
        info!(plugin_id = %row.id, "plugin restored");
        self.broadcast_status_change(&row.id).await;
        Ok(())
    }

    /// Start the WeChat QR-code login flow, streaming its lifecycle events to
    /// all connected WebSocket clients as `channel.weixin-login` messages
    /// (`phase` = `qr` / `scanned` / `done` / `error`).
    ///
    /// Returns immediately; the flow runs in a spawned task and always emits a
    /// terminal `done`/`error` phase. The frontend uses the WebSocket (not SSE)
    /// because `EventSource` cannot carry the desktop's `x-nomi-local-trust`
    /// header, so an SSE stream is rejected 403 by the auth middleware.
    #[cfg(feature = "weixin")]
    pub fn start_weixin_login(&self) {
        use crate::plugins::weixin::weixin_login_stream;

        let broadcaster = self.broadcaster.clone();
        let mut rx = weixin_login_stream();
        tokio::spawn(async move {
            while let Some(event) = rx.recv().await {
                broadcaster.broadcast(WebSocketMessage::new("channel.weixin-login", event.to_ws_payload()));
            }
        });
    }

    /// Updates a plugin to error status in the DB.
    async fn update_plugin_error(&self, plugin_id: &str, error_msg: &str) {        let params = UpdatePluginStatusParams {
            status: Some(PluginStatus::Error.to_string()),
            last_connected: None,
            enabled: None,
        };
        if let Err(e) = self.repo.update_plugin_status(plugin_id, &params).await {
            error!(
                plugin_id = %plugin_id,
                db_error = %e,
                original_error = %error_msg,
                "failed to update plugin error status in DB"
            );
        }
    }

    /// Broadcasts a `channel.plugin-status-changed` event.
    async fn broadcast_status_change(&self, plugin_id: &str) {
        let row = match self.repo.get_plugin(plugin_id).await {
            Ok(Some(row)) => row,
            Ok(None) => {
                warn!(plugin_id = %plugin_id, "plugin not found for status broadcast");
                return;
            }
            Err(e) => {
                warn!(
                    plugin_id = %plugin_id,
                    error = %e,
                    "failed to read plugin for status broadcast"
                );
                return;
            }
        };

        let live_status = self.plugins.get(plugin_id).map(|p| p.status().to_string());
        let status_response = self.row_to_status_response(&row, live_status);
        self.broadcast_status_response(plugin_id, status_response);
    }

    /// Broadcasts a `channel.plugin-status-changed` event with a prebuilt
    /// status view (used when the row no longer exists, e.g. after delete).
    fn broadcast_status_response(&self, plugin_id: &str, status_response: PluginStatusResponse) {
        let payload = PluginStatusChangedPayload {
            plugin_id: plugin_id.to_owned(),
            status: status_response,
        };
        let value = match serde_json::to_value(payload) {
            Ok(v) => v,
            Err(e) => {
                error!(error = %e, "failed to serialize status change payload");
                return;
            }
        };
        self.broadcaster
            .broadcast(WebSocketMessage::new("channel.plugin-status-changed", value));
    }

    /// Converts a DB row + optional live status to a `PluginStatusResponse`.
    fn row_to_status_response(&self, row: &ChannelPluginRow, live_status: Option<String>) -> PluginStatusResponse {
        let is_running = self.plugins.contains_key(&row.id);
        let has_token = !row.config.is_empty();
        PluginStatusResponse {
            plugin_id: row.id.clone(),
            plugin_type: row.r#type.clone(),
            name: row.name.clone(),
            enabled: row.enabled,
            status: live_status.or_else(|| row.status.clone()),
            last_connected: row.last_connected,
            companion_id: row.companion_id.clone(),
            public_agent_id: row.public_agent_id.clone(),
            bot_key: row.bot_key.clone(),
            created_at: row.created_at,
            updated_at: row.updated_at,
            connected: is_running,
            has_token,
            bot_username: None,
            active_users: 0,
        }
    }

    /// Returns a default display name for a plugin type.
    fn default_plugin_name(&self, plugin_type: PluginType) -> String {
        match plugin_type {
            PluginType::Telegram => "Telegram Bot".into(),
            PluginType::Lark => "Lark Bot".into(),
            PluginType::Dingtalk => "DingTalk Bot".into(),
            PluginType::Weixin => "WeChat Bot".into(),
            PluginType::Wecom => "WeCom Bot".into(),
            PluginType::Slack => "Slack Bot".into(),
            PluginType::Discord => "Discord Bot".into(),
            PluginType::Matrix => "Matrix Bot".into(),
            PluginType::Mattermost => "Mattermost Bot".into(),
            PluginType::Twitch => "Twitch Bot".into(),
            PluginType::Nostr => "Nostr Bot".into(),
            PluginType::Qqbot => "QQ Bot".into(),
        }
    }
}

#[async_trait::async_trait]
impl crate::stream_relay::ChannelSender for ChannelManager {
    async fn send_message(
        &self,
        plugin_id: &str,
        chat_id: &str,
        message: crate::types::UnifiedOutgoingMessage,
    ) -> Result<String, crate::error::ChannelError> {
        self.send_message(plugin_id, chat_id, message).await
    }

    async fn edit_message(
        &self,
        plugin_id: &str,
        chat_id: &str,
        message_id: &str,
        message: crate::types::UnifiedOutgoingMessage,
    ) -> Result<(), crate::error::ChannelError> {
        self.edit_message(plugin_id, chat_id, message_id, message).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{
        BotInfo, OutgoingMessageType, PluginCredentials, PluginStatus, PluginType, UnifiedOutgoingMessage,
    };
    use nomifun_common::TimestampMs;
    use nomifun_db::models::{AssistantSessionRow, AssistantUserRow, ChannelPluginRow, PairingCodeRow};
    use nomifun_db::{DbError, IChannelRepository, UpdatePluginStatusParams};
    use std::sync::Mutex;

    // ── Mock EventBroadcaster ──────────────────────────────────────────

    struct MockBroadcaster {
        events: Mutex<Vec<WebSocketMessage<serde_json::Value>>>,
    }

    impl MockBroadcaster {
        fn new() -> Self {
            Self {
                events: Mutex::new(Vec::new()),
            }
        }

        fn take_events(&self) -> Vec<WebSocketMessage<serde_json::Value>> {
            let mut guard = self.events.lock().unwrap();
            std::mem::take(&mut *guard)
        }
    }

    impl EventBroadcaster for MockBroadcaster {
        fn broadcast(&self, event: WebSocketMessage<serde_json::Value>) {
            self.events.lock().unwrap().push(event);
        }
    }

    // ── Mock IChannelRepository ────────────────────────────────────────

    struct MockRepo {
        plugins: Mutex<Vec<ChannelPluginRow>>,
        /// Channel ids passed to `delete_sessions_by_channel`, so tests can
        /// assert that delete/rebind clear exactly that channel's sessions.
        cleared_session_channels: Mutex<Vec<String>>,
    }

    impl MockRepo {
        fn new() -> Self {
            Self {
                plugins: Mutex::new(Vec::new()),
                cleared_session_channels: Mutex::new(Vec::new()),
            }
        }

        fn get_plugins(&self) -> Vec<ChannelPluginRow> {
            self.plugins.lock().unwrap().clone()
        }

        fn cleared_channels(&self) -> Vec<String> {
            self.cleared_session_channels.lock().unwrap().clone()
        }
    }

    #[async_trait::async_trait]
    impl IChannelRepository for MockRepo {
        async fn get_all_plugins(&self) -> Result<Vec<ChannelPluginRow>, DbError> {
            Ok(self.plugins.lock().unwrap().clone())
        }

        async fn get_plugin(&self, id: &str) -> Result<Option<ChannelPluginRow>, DbError> {
            let plugins = self.plugins.lock().unwrap();
            Ok(plugins.iter().find(|p| p.id == id).cloned())
        }

        async fn upsert_plugin(&self, row: &ChannelPluginRow) -> Result<(), DbError> {
            let mut plugins = self.plugins.lock().unwrap();
            if let Some(existing) = plugins.iter_mut().find(|p| p.id == row.id) {
                *existing = row.clone();
            } else {
                plugins.push(row.clone());
            }
            Ok(())
        }

        async fn update_plugin_status(&self, id: &str, params: &UpdatePluginStatusParams) -> Result<(), DbError> {
            let mut plugins = self.plugins.lock().unwrap();
            if let Some(p) = plugins.iter_mut().find(|p| p.id == id) {
                if let Some(ref s) = params.status {
                    p.status = Some(s.clone());
                }
                if let Some(lc) = params.last_connected {
                    p.last_connected = Some(lc);
                }
                if let Some(e) = params.enabled {
                    p.enabled = e;
                }
                p.updated_at = now_ms();
                Ok(())
            } else {
                Err(DbError::NotFound(id.into()))
            }
        }

        async fn update_plugin_companion(&self, id: &str, companion_id: Option<&str>) -> Result<(), DbError> {
            let mut plugins = self.plugins.lock().unwrap();
            if let Some(p) = plugins.iter_mut().find(|p| p.id == id) {
                p.companion_id = companion_id.map(str::to_owned);
                if companion_id.is_some() {
                    p.public_agent_id = None;
                }
                p.updated_at = now_ms();
                Ok(())
            } else {
                Err(DbError::NotFound(id.into()))
            }
        }

        async fn update_plugin_public_agent(&self, id: &str, public_agent_id: Option<&str>) -> Result<(), DbError> {
            let mut plugins = self.plugins.lock().unwrap();
            if let Some(p) = plugins.iter_mut().find(|p| p.id == id) {
                p.public_agent_id = public_agent_id.map(str::to_owned);
                if public_agent_id.is_some() {
                    p.companion_id = None;
                }
                p.updated_at = now_ms();
                Ok(())
            } else {
                Err(DbError::NotFound(id.into()))
            }
        }

        async fn update_plugin_bot_key(&self, id: &str, bot_key: &str) -> Result<(), DbError> {
            let mut plugins = self.plugins.lock().unwrap();
            if let Some(p) = plugins.iter_mut().find(|p| p.id == id) {
                p.bot_key = Some(bot_key.to_owned());
                p.updated_at = now_ms();
                Ok(())
            } else {
                Err(DbError::NotFound(id.into()))
            }
        }

        async fn delete_plugin(&self, id: &str) -> Result<(), DbError> {
            let mut plugins = self.plugins.lock().unwrap();
            let len_before = plugins.len();
            plugins.retain(|p| p.id != id);
            if plugins.len() == len_before {
                Err(DbError::NotFound(id.into()))
            } else {
                Ok(())
            }
        }

        // -- User CRUD (unused stubs) --
        async fn get_all_users(&self) -> Result<Vec<AssistantUserRow>, DbError> {
            Ok(vec![])
        }
        async fn get_user_by_platform(
            &self,
            _pid: &str,
            _pt: &str,
            _channel_id: &str,
        ) -> Result<Option<AssistantUserRow>, DbError> {
            Ok(None)
        }
        async fn create_user(&self, _row: &AssistantUserRow) -> Result<(), DbError> {
            Ok(())
        }
        async fn update_user_last_active(&self, _id: &str, _la: TimestampMs) -> Result<(), DbError> {
            Ok(())
        }
        async fn delete_user(&self, _id: &str) -> Result<(), DbError> {
            Ok(())
        }

        // -- Session CRUD (unused stubs) --
        async fn get_all_sessions(&self) -> Result<Vec<AssistantSessionRow>, DbError> {
            Ok(vec![])
        }
        async fn get_session(&self, _id: &str) -> Result<Option<AssistantSessionRow>, DbError> {
            Ok(None)
        }
        async fn get_or_create_session(
            &self,
            _uid: &str,
            _cid: &str,
            _channel_id: &str,
            new_row: &AssistantSessionRow,
        ) -> Result<AssistantSessionRow, DbError> {
            Ok(new_row.clone())
        }
        async fn update_session_activity(&self, _id: &str, _la: TimestampMs) -> Result<(), DbError> {
            Ok(())
        }
        async fn update_session_conversation(&self, _id: &str, _cid: i64) -> Result<(), DbError> {
            Ok(())
        }
        async fn update_session_agent_type(&self, _id: &str, _at: &str) -> Result<(), DbError> {
            Ok(())
        }
        async fn delete_sessions_by_user(&self, _uid: &str) -> Result<(), DbError> {
            Ok(())
        }
        async fn delete_sessions_by_channel(&self, channel_id: &str) -> Result<(), DbError> {
            self.cleared_session_channels.lock().unwrap().push(channel_id.to_owned());
            Ok(())
        }
        async fn delete_session_by_user_chat(&self, _uid: &str, _cid: &str, _channel_id: &str) -> Result<(), DbError> {
            Ok(())
        }

        // -- Pairing codes (unused stubs) --
        async fn create_pairing(&self, _row: &PairingCodeRow) -> Result<(), DbError> {
            Ok(())
        }
        async fn get_pending_pairings(&self) -> Result<Vec<PairingCodeRow>, DbError> {
            Ok(vec![])
        }
        async fn get_pairing_by_code(&self, _code: &str) -> Result<Option<PairingCodeRow>, DbError> {
            Ok(None)
        }
        async fn update_pairing_status(&self, _code: &str, _status: &str) -> Result<(), DbError> {
            Ok(())
        }
        async fn cleanup_expired_pairings(&self, _now: TimestampMs) -> Result<u64, DbError> {
            Ok(0)
        }
    }

    // ── Mock ChannelPlugin ─────────────────────────────────────────────

    struct MockPlugin {
        status: PluginStatus,
        plugin_type: PluginType,
        bot_info: Option<BotInfo>,
        last_error: Option<String>,
        should_fail_init: bool,
        should_fail_start: bool,
        /// When set, `initialize` stores the per-instance `message_tx` here
        /// so tests can emit messages as if the platform delivered them.
        capture_tx: Option<Arc<Mutex<Option<mpsc::Sender<UnifiedIncomingMessage>>>>>,
    }

    impl MockPlugin {
        fn new(plugin_type: PluginType) -> Self {
            Self {
                status: PluginStatus::Created,
                plugin_type,
                bot_info: None,
                last_error: None,
                should_fail_init: false,
                should_fail_start: false,
                capture_tx: None,
            }
        }

        fn failing_init(plugin_type: PluginType) -> Self {
            Self {
                should_fail_init: true,
                ..Self::new(plugin_type)
            }
        }

        fn failing_start(plugin_type: PluginType) -> Self {
            Self {
                should_fail_start: true,
                ..Self::new(plugin_type)
            }
        }
    }

    #[async_trait::async_trait]
    impl ChannelPlugin for MockPlugin {
        async fn initialize(&mut self, _config: PluginConfig, callbacks: PluginCallbacks) -> Result<(), ChannelError> {
            if let Some(slot) = &self.capture_tx {
                *slot.lock().unwrap() = Some(callbacks.message_tx.clone());
            }
            if self.should_fail_init {
                self.status = PluginStatus::Error;
                self.last_error = Some("Init failed".into());
                return Err(ChannelError::ConnectionFailed("Init failed".into()));
            }
            self.status = PluginStatus::Initializing;
            self.bot_info = Some(BotInfo {
                id: "mock_bot".into(),
                username: Some("mock_bot_user".into()),
                display_name: "Mock Bot".into(),
            });
            self.status = PluginStatus::Ready;
            Ok(())
        }

        async fn start(&mut self) -> Result<(), ChannelError> {
            if self.should_fail_start {
                self.status = PluginStatus::Error;
                self.last_error = Some("Start failed".into());
                return Err(ChannelError::ConnectionFailed("Start failed".into()));
            }
            self.status = PluginStatus::Starting;
            self.status = PluginStatus::Running;
            Ok(())
        }

        async fn stop(&mut self) -> Result<(), ChannelError> {
            self.status = PluginStatus::Stopping;
            self.status = PluginStatus::Stopped;
            Ok(())
        }

        async fn send_message(&self, _chat_id: &str, _message: UnifiedOutgoingMessage) -> Result<String, ChannelError> {
            Ok("mock_msg_id".into())
        }

        async fn edit_message(
            &self,
            _chat_id: &str,
            _message_id: &str,
            _message: UnifiedOutgoingMessage,
        ) -> Result<(), ChannelError> {
            Ok(())
        }

        fn active_user_count(&self) -> usize {
            0
        }

        fn bot_info(&self) -> Option<&BotInfo> {
            self.bot_info.as_ref()
        }

        fn plugin_type(&self) -> PluginType {
            self.plugin_type
        }

        fn status(&self) -> PluginStatus {
            self.status
        }

        fn last_error(&self) -> Option<&str> {
            self.last_error.as_deref()
        }
    }

    // ── Test helpers ───────────────────────────────────────────────────

    fn test_key() -> [u8; 32] {
        [0x42; 32]
    }

    fn make_manager() -> (ChannelManager, Arc<MockRepo>, Arc<MockBroadcaster>) {
        let (mgr, repo, bc, _rx) = make_manager_with_rx();
        (mgr, repo, bc)
    }

    /// Like `make_manager`, but keeps the message receiver alive so tests
    /// can observe the channel-stamped forwarding path.
    fn make_manager_with_rx() -> (
        ChannelManager,
        Arc<MockRepo>,
        Arc<MockBroadcaster>,
        mpsc::Receiver<ChannelIncoming>,
    ) {
        let repo = Arc::new(MockRepo::new());
        let broadcaster = Arc::new(MockBroadcaster::new());
        let (msg_tx, msg_rx) = mpsc::channel(16);
        let (confirm_tx, _confirm_rx) = mpsc::channel(16);
        let mgr = ChannelManager::new(repo.clone(), broadcaster.clone(), test_key(), msg_tx, confirm_tx);
        (mgr, repo, broadcaster, msg_rx)
    }

    fn make_test_config() -> serde_json::Value {
        serde_json::json!({
            "credentials": { "token": "bot:test123" },
            "config": { "mode": "polling" }
        })
    }

    fn make_plugin_config() -> PluginConfig {
        PluginConfig {
            credentials: PluginCredentials {
                token: Some("bot:test123".into()),
                ..Default::default()
            },
            config: None,
        }
    }

    fn make_factory() -> PluginFactory {
        Box::new(|pt| Some(Box::new(MockPlugin::new(pt))))
    }

    /// Factory whose plugins store their `message_tx` into `slot` on init.
    fn make_capturing_factory(slot: &Arc<Mutex<Option<mpsc::Sender<UnifiedIncomingMessage>>>>) -> PluginFactory {
        let slot = Arc::clone(slot);
        Box::new(move |pt| {
            let mut plugin = MockPlugin::new(pt);
            plugin.capture_tx = Some(Arc::clone(&slot));
            Some(Box::new(plugin))
        })
    }

    fn make_failing_init_factory() -> PluginFactory {
        Box::new(|pt| Some(Box::new(MockPlugin::failing_init(pt))))
    }

    fn make_failing_start_factory() -> PluginFactory {
        Box::new(|pt| Some(Box::new(MockPlugin::failing_start(pt))))
    }

    fn make_no_impl_factory() -> PluginFactory {
        Box::new(|_pt| None)
    }

    fn make_test_outgoing() -> UnifiedOutgoingMessage {
        UnifiedOutgoingMessage {
            message_type: OutgoingMessageType::Text,
            text: Some("test".into()),
            parse_mode: None,
            buttons: None,
            keyboard: None,
            image_url: None,
            file_url: None,
            file_name: None,
            media_actions: None,
            reply_to_message_id: None,
            silent: None,
        }
    }

    // ── get_plugin_status ──────────────────────────────────────────────

    #[tokio::test]
    async fn get_status_empty() {
        let (mgr, _repo, _bc) = make_manager();
        let statuses = mgr.get_plugin_status().await.unwrap();
        assert!(statuses.is_empty());
    }

    #[tokio::test]
    async fn get_status_returns_db_plugins() {
        let (mgr, repo, _bc) = make_manager();
        let now = now_ms();
        repo.plugins.lock().unwrap().push(ChannelPluginRow {
            id: "telegram".into(),
            r#type: "telegram".into(),
            name: "Telegram Bot".into(),
            enabled: true,
            config: "encrypted".into(),
            status: Some("running".into()),
            last_connected: Some(now),
            companion_id: None,
            public_agent_id: None,
            bot_key: None,
            created_at: now,
            updated_at: now,
        });

        let statuses = mgr.get_plugin_status().await.unwrap();
        assert_eq!(statuses.len(), 1);
        assert_eq!(statuses[0].plugin_id, "telegram");
        assert_eq!(statuses[0].plugin_type, "telegram");
        assert_eq!(statuses[0].name, "Telegram Bot");
        assert!(statuses[0].enabled);
    }

    #[tokio::test]
    async fn get_status_uses_live_status_over_db() {
        let (mgr, repo, _bc) = make_manager();
        let factory = make_factory();

        // Enable the plugin (will set live status to Running)
        mgr.enable_plugin(&EnableChannelSpec::legacy("telegram"), &make_test_config(), &factory)
            .await
            .unwrap();

        // Manually set DB status to something different
        {
            let mut plugins = repo.plugins.lock().unwrap();
            if let Some(p) = plugins.iter_mut().find(|p| p.id == "telegram") {
                p.status = Some("stopped".into());
            }
        }

        let statuses = mgr.get_plugin_status().await.unwrap();
        assert_eq!(statuses.len(), 1);
        // Live status (running) should override DB status (stopped)
        assert_eq!(statuses[0].status.as_deref(), Some("running"));
    }

    // ── enable_plugin ──────────────────────────────────────────────────

    #[tokio::test]
    async fn enable_plugin_persists_encrypted_config() {
        let (mgr, repo, _bc) = make_manager();
        let factory = make_factory();

        mgr.enable_plugin(&EnableChannelSpec::legacy("telegram"), &make_test_config(), &factory)
            .await
            .unwrap();

        let plugins = repo.get_plugins();
        assert_eq!(plugins.len(), 1);
        assert_eq!(plugins[0].id, "telegram");
        assert!(plugins[0].enabled);
        // Config should be encrypted (base64), not plaintext
        assert_ne!(plugins[0].config, serde_json::to_string(&make_test_config()).unwrap());
        // Verify it can be decrypted back
        let decrypted = decrypt_string(&plugins[0].config, &test_key()).unwrap();
        let parsed: PluginConfig = serde_json::from_str(&decrypted).unwrap();
        assert_eq!(parsed.credentials.token.as_deref(), Some("bot:test123"));
    }

    #[tokio::test]
    async fn enable_plugin_stores_running_instance() {
        let (mgr, _repo, _bc) = make_manager();
        let factory = make_factory();

        mgr.enable_plugin(&EnableChannelSpec::legacy("telegram"), &make_test_config(), &factory)
            .await
            .unwrap();

        assert_eq!(mgr.active_plugin_count(), 1);
        assert!(mgr.is_plugin_running("telegram"));
    }

    #[tokio::test]
    async fn enable_plugin_broadcasts_status_change() {
        let (mgr, _repo, bc) = make_manager();
        let factory = make_factory();

        mgr.enable_plugin(&EnableChannelSpec::legacy("telegram"), &make_test_config(), &factory)
            .await
            .unwrap();

        let events = bc.take_events();
        assert!(!events.is_empty());
        let last = events.last().unwrap();
        assert_eq!(last.name, "channel.plugin-status-changed");
        assert_eq!(last.data["plugin_id"], "telegram");
    }

    #[tokio::test]
    async fn enable_replaces_existing_plugin() {
        let (mgr, _repo, _bc) = make_manager();
        let factory = make_factory();

        mgr.enable_plugin(&EnableChannelSpec::legacy("telegram"), &make_test_config(), &factory)
            .await
            .unwrap();
        assert_eq!(mgr.active_plugin_count(), 1);

        // Re-enable should replace (stop old, start new)
        mgr.enable_plugin(&EnableChannelSpec::legacy("telegram"), &make_test_config(), &factory)
            .await
            .unwrap();
        assert_eq!(mgr.active_plugin_count(), 1);
    }

    #[tokio::test]
    async fn enable_existing_plugin_with_empty_config_reuses_saved_config() {
        let (mgr, repo, _bc) = make_manager();
        let factory = make_factory();

        mgr.enable_plugin(&EnableChannelSpec::legacy("telegram"), &make_test_config(), &factory)
            .await
            .unwrap();

        mgr.enable_plugin(&EnableChannelSpec::legacy("telegram"), &serde_json::json!({}), &factory)
            .await
            .unwrap();

        assert!(mgr.is_plugin_running("telegram"));
        assert_eq!(mgr.active_plugin_count(), 1);
        assert_eq!(repo.get_plugins()[0].bot_key.as_deref(), Some("bot"));
    }

    #[tokio::test]
    async fn enable_invalid_plugin_type_fails() {
        let (mgr, _repo, _bc) = make_manager();
        let factory = make_factory();

        let err = mgr
            .enable_plugin(&EnableChannelSpec::legacy("whatsapp"), &make_test_config(), &factory)
            .await
            .unwrap_err();
        assert!(matches!(err, ChannelError::InvalidPluginType(_)));
    }

    #[tokio::test]
    async fn enable_invalid_config_json_fails() {
        let (mgr, _repo, _bc) = make_manager();
        let factory = make_factory();

        let bad_config = serde_json::json!({ "wrong": "shape" });
        let err = mgr.enable_plugin(&EnableChannelSpec::legacy("telegram"), &bad_config, &factory).await.unwrap_err();
        assert!(matches!(err, ChannelError::InvalidConfig(_)));
    }

    #[tokio::test]
    async fn enable_no_implementation_fails() {
        let (mgr, _repo, _bc) = make_manager();
        let factory = make_no_impl_factory();

        let err = mgr
            .enable_plugin(&EnableChannelSpec::legacy("telegram"), &make_test_config(), &factory)
            .await
            .unwrap_err();
        assert!(matches!(err, ChannelError::InvalidPluginType(_)));
    }

    #[tokio::test]
    async fn enable_init_failure_sets_error_status() {
        let (mgr, repo, _bc) = make_manager();
        let factory = make_failing_init_factory();

        let err = mgr.enable_plugin(&EnableChannelSpec::legacy("telegram"), &make_test_config(), &factory).await;
        assert!(err.is_err());

        // Plugin should not be in active map
        assert_eq!(mgr.active_plugin_count(), 0);

        // DB should have error status
        let plugins = repo.get_plugins();
        assert_eq!(plugins[0].status.as_deref(), Some("error"));
    }

    #[tokio::test]
    async fn enable_start_failure_sets_error_status() {
        let (mgr, repo, _bc) = make_manager();
        let factory = make_failing_start_factory();

        let err = mgr.enable_plugin(&EnableChannelSpec::legacy("telegram"), &make_test_config(), &factory).await;
        assert!(err.is_err());

        assert_eq!(mgr.active_plugin_count(), 0);
        let plugins = repo.get_plugins();
        assert_eq!(plugins[0].status.as_deref(), Some("error"));
    }

    // ── disable_plugin ─────────────────────────────────────────────────

    #[tokio::test]
    async fn disable_stops_and_updates_db() {
        let (mgr, repo, _bc) = make_manager();
        let factory = make_factory();

        mgr.enable_plugin(&EnableChannelSpec::legacy("telegram"), &make_test_config(), &factory)
            .await
            .unwrap();
        assert!(mgr.is_plugin_running("telegram"));

        mgr.disable_plugin("telegram").await.unwrap();

        assert_eq!(mgr.active_plugin_count(), 0);
        let plugins = repo.get_plugins();
        assert!(!plugins[0].enabled);
        assert_eq!(plugins[0].status.as_deref(), Some("stopped"));
    }

    #[tokio::test]
    async fn disable_broadcasts_status_change() {
        let (mgr, _repo, bc) = make_manager();
        let factory = make_factory();

        mgr.enable_plugin(&EnableChannelSpec::legacy("telegram"), &make_test_config(), &factory)
            .await
            .unwrap();
        bc.take_events(); // clear enable events

        mgr.disable_plugin("telegram").await.unwrap();

        let events = bc.take_events();
        assert!(!events.is_empty());
        assert_eq!(events.last().unwrap().name, "channel.plugin-status-changed");
    }

    #[tokio::test]
    async fn disable_idempotent_for_not_running() {
        let (mgr, repo, _bc) = make_manager();
        // Manually insert a disabled plugin in DB
        repo.plugins.lock().unwrap().push(ChannelPluginRow {
            id: "telegram".into(),
            r#type: "telegram".into(),
            name: "Telegram Bot".into(),
            enabled: false,
            config: "encrypted".into(),
            status: Some("stopped".into()),
            last_connected: None,
            companion_id: None,
            public_agent_id: None,
            bot_key: None,
            created_at: now_ms(),
            updated_at: now_ms(),
        });

        // Should not error
        mgr.disable_plugin("telegram").await.unwrap();
        assert_eq!(mgr.active_plugin_count(), 0);
    }

    // ── test_plugin ────────────────────────────────────────────────────

    #[tokio::test]
    async fn test_plugin_returns_bot_username() {
        let (mgr, _repo, _bc) = make_manager();
        let factory = make_factory();

        let result = mgr
            .test_plugin("telegram", make_plugin_config(), &factory)
            .await
            .unwrap();
        assert_eq!(result.as_deref(), Some("mock_bot_user"));
    }

    #[tokio::test]
    async fn test_plugin_does_not_persist() {
        let (mgr, repo, _bc) = make_manager();
        let factory = make_factory();

        mgr.test_plugin("telegram", make_plugin_config(), &factory)
            .await
            .unwrap();

        // Nothing should be stored in DB
        assert!(repo.get_plugins().is_empty());
        assert_eq!(mgr.active_plugin_count(), 0);
    }

    #[tokio::test]
    async fn test_plugin_invalid_type_fails() {
        let (mgr, _repo, _bc) = make_manager();
        let factory = make_factory();

        let err = mgr
            .test_plugin("whatsapp", make_plugin_config(), &factory)
            .await
            .unwrap_err();
        assert!(matches!(err, ChannelError::InvalidPluginType(_)));
    }

    #[tokio::test]
    async fn test_plugin_init_failure_propagates() {
        let (mgr, _repo, _bc) = make_manager();
        let factory = make_failing_init_factory();

        let err = mgr.test_plugin("telegram", make_plugin_config(), &factory).await;
        assert!(err.is_err());
    }

    // ── restore_plugins ────────────────────────────────────────────────

    #[tokio::test]
    async fn restore_skips_disabled_plugins() {
        let (mgr, repo, _bc) = make_manager();
        let factory = make_factory();

        let config_json = serde_json::to_string(&make_plugin_config()).unwrap();
        let encrypted = encrypt_string(&config_json, &test_key()).unwrap();

        repo.plugins.lock().unwrap().push(ChannelPluginRow {
            id: "telegram".into(),
            r#type: "telegram".into(),
            name: "Telegram Bot".into(),
            enabled: false,
            config: encrypted,
            status: Some("stopped".into()),
            last_connected: None,
            companion_id: None,
            public_agent_id: None,
            bot_key: None,
            created_at: now_ms(),
            updated_at: now_ms(),
        });

        mgr.restore_plugins(&factory).await.unwrap();
        assert_eq!(mgr.active_plugin_count(), 0);
    }

    #[tokio::test]
    async fn restore_starts_enabled_plugins() {
        let (mgr, repo, _bc) = make_manager();
        let factory = make_factory();

        let config_json = serde_json::to_string(&make_plugin_config()).unwrap();
        let encrypted = encrypt_string(&config_json, &test_key()).unwrap();

        repo.plugins.lock().unwrap().push(ChannelPluginRow {
            id: "telegram".into(),
            r#type: "telegram".into(),
            name: "Telegram Bot".into(),
            enabled: true,
            config: encrypted,
            status: Some("stopped".into()),
            last_connected: None,
            companion_id: None,
            public_agent_id: None,
            bot_key: None,
            created_at: now_ms(),
            updated_at: now_ms(),
        });

        mgr.restore_plugins(&factory).await.unwrap();
        assert_eq!(mgr.active_plugin_count(), 1);
        assert!(mgr.is_plugin_running("telegram"));
    }

    #[tokio::test]
    async fn restore_continues_on_individual_failure() {
        let (mgr, repo, _bc) = make_manager();

        let config_json = serde_json::to_string(&make_plugin_config()).unwrap();
        let encrypted = encrypt_string(&config_json, &test_key()).unwrap();

        // One valid plugin and one with bad encrypted config
        {
            let mut plugins = repo.plugins.lock().unwrap();
            plugins.push(ChannelPluginRow {
                id: "telegram".into(),
                r#type: "telegram".into(),
                name: "Telegram Bot".into(),
                enabled: true,
                config: encrypted,
                status: None,
                last_connected: None,
                companion_id: None,
                public_agent_id: None,
                bot_key: None,
                created_at: now_ms(),
                updated_at: now_ms(),
            });
            plugins.push(ChannelPluginRow {
                id: "lark".into(),
                r#type: "lark".into(),
                name: "Lark Bot".into(),
                enabled: true,
                config: "invalid-encrypted-data".into(),
                status: None,
                last_connected: None,
                companion_id: None,
                public_agent_id: None,
                bot_key: None,
                created_at: now_ms(),
                updated_at: now_ms(),
            });
        }

        let factory = make_factory();
        mgr.restore_plugins(&factory).await.unwrap();

        // Telegram should have started, Lark should have failed
        assert_eq!(mgr.active_plugin_count(), 1);
        assert!(mgr.is_plugin_running("telegram"));
        assert!(!mgr.is_plugin_running("lark"));

        // Lark should have error status in DB
        let plugins = repo.get_plugins();
        let lark = plugins.iter().find(|p| p.id == "lark").unwrap();
        assert_eq!(lark.status.as_deref(), Some("error"));
    }

    #[tokio::test]
    async fn restore_empty_is_noop() {
        let (mgr, _repo, _bc) = make_manager();
        let factory = make_factory();
        mgr.restore_plugins(&factory).await.unwrap();
        assert_eq!(mgr.active_plugin_count(), 0);
    }

    // ── shutdown ───────────────────────────────────────────────────────

    #[tokio::test]
    async fn shutdown_stops_all_plugins() {
        let (mgr, _repo, _bc) = make_manager();
        let factory = make_factory();

        mgr.enable_plugin(&EnableChannelSpec::legacy("telegram"), &make_test_config(), &factory)
            .await
            .unwrap();

        let lark_config = serde_json::json!({
            "credentials": {
                "appId": "cli_abc",
                "appSecret": "secret"
            }
        });
        mgr.enable_plugin(&EnableChannelSpec::legacy("lark"), &lark_config, &factory).await.unwrap();

        assert_eq!(mgr.active_plugin_count(), 2);

        mgr.shutdown().await;
        assert_eq!(mgr.active_plugin_count(), 0);
    }

    // ── send_message / edit_message ────────────────────────────────────

    #[tokio::test]
    async fn send_message_through_plugin() {
        let (mgr, _repo, _bc) = make_manager();
        let factory = make_factory();

        mgr.enable_plugin(&EnableChannelSpec::legacy("telegram"), &make_test_config(), &factory)
            .await
            .unwrap();

        let msg_id = mgr
            .send_message("telegram", "chat_1", make_test_outgoing())
            .await
            .unwrap();
        assert_eq!(msg_id, "mock_msg_id");
    }

    #[tokio::test]
    async fn send_message_plugin_not_found() {
        let (mgr, _repo, _bc) = make_manager();
        let err = mgr
            .send_message("telegram", "chat_1", make_test_outgoing())
            .await
            .unwrap_err();
        assert!(matches!(err, ChannelError::PluginNotFound(_)));
    }

    #[tokio::test]
    async fn edit_message_through_plugin() {
        let (mgr, _repo, _bc) = make_manager();
        let factory = make_factory();

        mgr.enable_plugin(&EnableChannelSpec::legacy("telegram"), &make_test_config(), &factory)
            .await
            .unwrap();

        mgr.edit_message("telegram", "chat_1", "msg_1", make_test_outgoing())
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn edit_message_plugin_not_found() {
        let (mgr, _repo, _bc) = make_manager();
        let err = mgr
            .edit_message("telegram", "chat_1", "msg_1", make_test_outgoing())
            .await
            .unwrap_err();
        assert!(matches!(err, ChannelError::PluginNotFound(_)));
    }

    // ── helper methods ─────────────────────────────────────────────────

    #[tokio::test]
    async fn active_plugin_count_tracks_correctly() {
        let (mgr, _repo, _bc) = make_manager();
        let factory = make_factory();

        assert_eq!(mgr.active_plugin_count(), 0);

        mgr.enable_plugin(&EnableChannelSpec::legacy("telegram"), &make_test_config(), &factory)
            .await
            .unwrap();
        assert_eq!(mgr.active_plugin_count(), 1);

        mgr.disable_plugin("telegram").await.unwrap();
        assert_eq!(mgr.active_plugin_count(), 0);
    }

    #[tokio::test]
    async fn is_plugin_running_false_for_missing() {
        let (mgr, _repo, _bc) = make_manager();
        assert!(!mgr.is_plugin_running("nonexistent"));
    }

    #[test]
    fn default_plugin_names() {
        let (mgr, _repo, _bc) = make_manager();
        assert_eq!(mgr.default_plugin_name(PluginType::Telegram), "Telegram Bot");
        assert_eq!(mgr.default_plugin_name(PluginType::Lark), "Lark Bot");
        assert_eq!(mgr.default_plugin_name(PluginType::Dingtalk), "DingTalk Bot");
        assert_eq!(mgr.default_plugin_name(PluginType::Weixin), "WeChat Bot");
        assert_eq!(mgr.default_plugin_name(PluginType::Slack), "Slack Bot");
        assert_eq!(mgr.default_plugin_name(PluginType::Discord), "Discord Bot");
    }

    // ── Per-companion bot channels ───────────────────────────────────────────

    fn lark_spec(companion: &str) -> EnableChannelSpec {
        EnableChannelSpec {
            plugin_id: None,
            plugin_type: Some("lark".into()),
            companion_id: Some(companion.into()),
            public_agent_id: None,
        }
    }

    fn lark_public_agent_spec(public_agent_id: &str) -> EnableChannelSpec {
        EnableChannelSpec {
            plugin_id: None,
            plugin_type: Some("lark".into()),
            companion_id: None,
            public_agent_id: Some(public_agent_id.into()),
        }
    }

    fn lark_config_with_app(app_id: &str) -> serde_json::Value {
        serde_json::json!({
            "credentials": { "app_id": app_id, "app_secret": "s3cret" }
        })
    }

    fn qqbot_spec(companion: &str) -> EnableChannelSpec {
        EnableChannelSpec {
            plugin_id: None,
            plugin_type: Some("qqbot".into()),
            companion_id: Some(companion.into()),
            public_agent_id: None,
        }
    }

    fn qqbot_config_with_app(app_id: &str) -> serde_json::Value {
        serde_json::json!({
            "credentials": { "client_id": app_id, "client_secret": "s3cret" }
        })
    }

    fn make_incoming(id: &str) -> UnifiedIncomingMessage {
        use crate::types::{MessageContentType, UnifiedMessageContent, UnifiedUser};
        UnifiedIncomingMessage {
            id: id.into(),
            platform: PluginType::Telegram,
            chat_id: "chat_1".into(),
            user: UnifiedUser {
                id: "u1".into(),
                username: None,
                display_name: "U".into(),
                avatar_url: None,
            },
            content: UnifiedMessageContent {
                content_type: MessageContentType::Text,
                text: "hi".into(),
                attachments: None,
            },
            timestamp: 0,
            reply_to_message_id: None,
            action: None,
            raw: None,
        }
    }

    #[tokio::test]
    async fn enable_with_plugin_type_creates_companion_bound_row() {
        let (mgr, repo, _bc) = make_manager();
        let factory = make_factory();

        let id = mgr
            .enable_plugin(&lark_spec("companion_a"), &lark_config_with_app("cli_app_1"), &factory)
            .await
            .unwrap();

        assert!(id.starts_with("achn"), "generated id should be achn-prefixed: {id}");
        let plugins = repo.get_plugins();
        assert_eq!(plugins.len(), 1);
        assert_eq!(plugins[0].id, id);
        assert_eq!(plugins[0].companion_id.as_deref(), Some("companion_a"));
        assert_eq!(plugins[0].bot_key.as_deref(), Some("cli_app_1"));
        assert!(mgr.is_plugin_running(&id));
    }

    #[tokio::test]
    async fn same_bot_for_second_companion_is_rejected() {
        let (mgr, repo, _bc) = make_manager();
        let factory = make_factory();

        mgr.enable_plugin(&lark_spec("companion_a"), &lark_config_with_app("cli_app_1"), &factory)
            .await
            .unwrap();

        let err = mgr
            .enable_plugin(&lark_spec("companion_b"), &lark_config_with_app("cli_app_1"), &factory)
            .await
            .unwrap_err();
        match err {
            ChannelError::BotAlreadyBound(ref bound_to) => {
                assert!(
                    bound_to.to_string().contains("companion_a"),
                    "error should name the bound companion: {bound_to}"
                );
            }
            other => panic!("expected BotAlreadyBound, got {other:?}"),
        }
        // The rejected attempt must not have touched persistence or runtime.
        assert_eq!(repo.get_plugins().len(), 1);
        assert_eq!(mgr.active_plugin_count(), 1);
    }

    #[tokio::test]
    async fn same_bot_for_same_companion_reuses_existing_row() {
        let (mgr, repo, _bc) = make_manager();
        let factory = make_factory();

        let first_id = mgr
            .enable_plugin(&lark_spec("companion_a"), &lark_config_with_app("cli_app_1"), &factory)
            .await
            .unwrap();
        mgr.disable_plugin(&first_id).await.unwrap();
        assert!(repo.get_plugins()[0].last_connected.is_some());

        let reused_id = mgr
            .enable_plugin(&lark_spec("companion_a"), &lark_config_with_app("cli_app_1"), &factory)
            .await
            .unwrap();

        assert_eq!(reused_id, first_id);
        assert_eq!(repo.get_plugins().len(), 1);
        assert_eq!(repo.get_plugins()[0].companion_id.as_deref(), Some("companion_a"));
        assert!(mgr.is_plugin_running(&first_id));
    }

    #[tokio::test]
    async fn qqbot_same_appid_for_same_companion_reuses_error_row() {
        let (mgr, repo, _bc) = make_manager();
        let factory = make_factory();

        let companion_id = "companion_1kwgygw5ryvedy89";
        let app_id = "1904991044";
        let first_id = mgr
            .enable_plugin(&qqbot_spec(companion_id), &qqbot_config_with_app(app_id), &factory)
            .await
            .unwrap();
        mgr.disable_plugin(&first_id).await.unwrap();
        {
            let mut rows = repo.plugins.lock().unwrap();
            rows[0].enabled = true;
            rows[0].status = Some("error".into());
        }

        let reused_id = mgr
            .enable_plugin(&qqbot_spec(companion_id), &qqbot_config_with_app(app_id), &factory)
            .await
            .unwrap();

        assert_eq!(reused_id, first_id);
        let rows = repo.get_plugins();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].companion_id.as_deref(), Some(companion_id));
        assert_eq!(rows[0].bot_key.as_deref(), Some(app_id));
        assert!(mgr.is_plugin_running(&first_id));
    }

    #[tokio::test]
    async fn same_bot_for_same_public_agent_reuses_existing_row() {
        let (mgr, repo, _bc) = make_manager();
        let factory = make_factory();

        let first_id = mgr
            .enable_plugin(&lark_public_agent_spec("pubagent_a"), &lark_config_with_app("cli_app_1"), &factory)
            .await
            .unwrap();
        mgr.disable_plugin(&first_id).await.unwrap();

        let reused_id = mgr
            .enable_plugin(&lark_public_agent_spec("pubagent_a"), &lark_config_with_app("cli_app_1"), &factory)
            .await
            .unwrap();

        assert_eq!(reused_id, first_id);
        assert_eq!(repo.get_plugins().len(), 1);
        assert_eq!(repo.get_plugins()[0].public_agent_id.as_deref(), Some("pubagent_a"));
        assert!(mgr.is_plugin_running(&first_id));
    }

    #[tokio::test]
    async fn same_bot_unbound_row_is_adopted_by_requested_companion() {
        let (mgr, repo, _bc) = make_manager();
        let factory = make_factory();

        let first_id = mgr
            .enable_plugin(
                &EnableChannelSpec {
                    plugin_id: None,
                    plugin_type: Some("lark".into()),
                    companion_id: None,
                    public_agent_id: None,
                },
                &lark_config_with_app("cli_app_1"),
                &factory,
            )
            .await
            .unwrap();

        let reused_id = mgr
            .enable_plugin(&lark_spec("companion_a"), &lark_config_with_app("cli_app_1"), &factory)
            .await
            .unwrap();

        assert_eq!(reused_id, first_id);
        assert_eq!(repo.get_plugins().len(), 1);
        assert_eq!(repo.get_plugins()[0].companion_id.as_deref(), Some("companion_a"));
    }

    #[tokio::test]
    async fn never_connected_failed_bot_row_is_reused_instead_of_blocking() {
        let (mgr, repo, _bc) = make_manager();
        let failing_factory = make_failing_start_factory();
        let factory = make_factory();

        let err = mgr
            .enable_plugin(&lark_spec("companion_a"), &lark_config_with_app("cli_app_1"), &failing_factory)
            .await;
        assert!(err.is_err());
        let failed_id = repo.get_plugins()[0].id.clone();
        assert_eq!(repo.get_plugins()[0].status.as_deref(), Some("error"));
        assert!(repo.get_plugins()[0].last_connected.is_none());
        assert_eq!(mgr.active_plugin_count(), 0);

        let reused_id = mgr
            .enable_plugin(&lark_spec("companion_b"), &lark_config_with_app("cli_app_1"), &factory)
            .await
            .unwrap();

        assert_eq!(reused_id, failed_id);
        let plugins = repo.get_plugins();
        assert_eq!(plugins.len(), 1);
        assert_eq!(plugins[0].companion_id.as_deref(), Some("companion_b"));
        assert_eq!(plugins[0].bot_key.as_deref(), Some("cli_app_1"));
        assert!(mgr.is_plugin_running(&reused_id));
    }

    #[tokio::test]
    async fn previously_connected_disabled_bot_still_blocks_second_companion() {
        let (mgr, repo, _bc) = make_manager();
        let factory = make_factory();

        let id = mgr
            .enable_plugin(&lark_spec("companion_a"), &lark_config_with_app("cli_app_1"), &factory)
            .await
            .unwrap();
        mgr.disable_plugin(&id).await.unwrap();
        assert_eq!(mgr.active_plugin_count(), 0);
        assert!(repo.get_plugins()[0].last_connected.is_some());

        let err = mgr
            .enable_plugin(&lark_spec("companion_b"), &lark_config_with_app("cli_app_1"), &factory)
            .await
            .unwrap_err();
        assert!(matches!(err, ChannelError::BotAlreadyBound(_)));
        assert_eq!(repo.get_plugins()[0].companion_id.as_deref(), Some("companion_a"));
    }

    #[tokio::test]
    async fn two_bots_same_platform_coexist() {
        let (mgr, repo, _bc) = make_manager();
        let factory = make_factory();

        let id_a = mgr
            .enable_plugin(&lark_spec("companion_a"), &lark_config_with_app("cli_app_a"), &factory)
            .await
            .unwrap();
        let id_b = mgr
            .enable_plugin(&lark_spec("companion_b"), &lark_config_with_app("cli_app_b"), &factory)
            .await
            .unwrap();

        assert_ne!(id_a, id_b);
        assert_eq!(mgr.active_plugin_count(), 2);
        assert!(mgr.is_plugin_running(&id_a));
        assert!(mgr.is_plugin_running(&id_b));
        assert_eq!(repo.get_plugins().len(), 2);
    }

    #[tokio::test]
    async fn legacy_spec_keeps_platform_row_id() {
        let (mgr, repo, _bc) = make_manager();
        let factory = make_factory();

        let id = mgr
            .enable_plugin(&EnableChannelSpec::legacy("telegram"), &make_test_config(), &factory)
            .await
            .unwrap();

        assert_eq!(id, "telegram");
        let plugins = repo.get_plugins();
        assert_eq!(plugins.len(), 1);
        assert_eq!(plugins[0].id, "telegram");
        assert!(plugins[0].companion_id.is_none());
        assert!(mgr.is_plugin_running("telegram"));
    }

    #[tokio::test]
    async fn forwarded_messages_are_stamped_with_channel_id() {
        let (mgr, _repo, _bc, mut rx) = make_manager_with_rx();
        let slot = Arc::new(Mutex::new(None));
        let factory = make_capturing_factory(&slot);

        let id = mgr
            .enable_plugin(&EnableChannelSpec::legacy("telegram"), &make_test_config(), &factory)
            .await
            .unwrap();

        let plugin_tx = slot
            .lock()
            .unwrap()
            .clone()
            .expect("initialize must hand the plugin its message_tx");
        plugin_tx.send(make_incoming("m-1")).await.unwrap();

        let incoming = tokio::time::timeout(Duration::from_secs(2), rx.recv())
            .await
            .expect("forwarder should deliver within the timeout")
            .expect("manager message channel must stay open");
        assert_eq!(incoming.channel_id, id);
        assert_eq!(incoming.message.id, "m-1");
    }

    #[tokio::test]
    async fn delete_channel_removes_row_instance_and_sessions() {
        let (mgr, repo, bc) = make_manager();
        let factory = make_factory();

        mgr.enable_plugin(&EnableChannelSpec::legacy("telegram"), &make_test_config(), &factory)
            .await
            .unwrap();
        assert!(mgr.is_plugin_running("telegram"));
        bc.take_events();

        mgr.delete_channel("telegram").await.unwrap();

        assert!(repo.get_plugins().is_empty());
        assert_eq!(mgr.active_plugin_count(), 0);
        assert_eq!(repo.cleared_channels(), vec!["telegram".to_string()]);
        // The final broadcast reports the deleted channel as stopped/disabled.
        let events = bc.take_events();
        let last = events.last().unwrap();
        assert_eq!(last.name, "channel.plugin-status-changed");
        assert_eq!(last.data["plugin_id"], "telegram");
        assert_eq!(last.data["status"]["enabled"], false);
    }

    #[tokio::test]
    async fn delete_channel_unknown_id_is_not_found() {
        let (mgr, _repo, _bc) = make_manager();
        let err = mgr.delete_channel("missing").await.unwrap_err();
        assert!(matches!(err, ChannelError::PluginNotFound(_)));
    }

    #[tokio::test]
    async fn rebind_channel_companion_updates_binding_and_clears_sessions() {
        let (mgr, repo, _bc) = make_manager();
        let factory = make_factory();

        let id = mgr
            .enable_plugin(&lark_spec("companion_a"), &lark_config_with_app("cli_app_1"), &factory)
            .await
            .unwrap();

        mgr.rebind_channel_companion(&id, Some("companion_z")).await.unwrap();
        assert_eq!(repo.get_plugins()[0].companion_id.as_deref(), Some("companion_z"));
        assert_eq!(repo.cleared_channels(), vec![id.clone()]);

        // Clearing the binding works too (and clears sessions again).
        mgr.rebind_channel_companion(&id, None).await.unwrap();
        assert!(repo.get_plugins()[0].companion_id.is_none());
        assert_eq!(repo.cleared_channels().len(), 2);
    }

    #[tokio::test]
    async fn enable_with_public_agent_id_binds_row_and_clears_companion() {
        let (mgr, repo, _bc) = make_manager();
        let factory = make_factory();

        // A brand-new bot bound to a public agent: the row carries public_agent_id
        // and NO companion.
        let id = mgr
            .enable_plugin(&lark_public_agent_spec("pubagent_1"), &lark_config_with_app("cli_app_1"), &factory)
            .await
            .unwrap();
        let row = repo.get_plugins().into_iter().find(|r| r.id == id).unwrap();
        assert_eq!(row.public_agent_id.as_deref(), Some("pubagent_1"));
        assert!(row.companion_id.is_none(), "a public-agent bot must not carry a companion");

        // Re-enabling the SAME row with a companion spec flips it to companion and
        // clears the public-agent binding (row-level mutual exclusivity).
        let spec = EnableChannelSpec {
            plugin_id: Some(id.clone()),
            plugin_type: Some("lark".into()),
            companion_id: Some("companion_a".into()),
            public_agent_id: None,
        };
        mgr.enable_plugin(&spec, &lark_config_with_app("cli_app_1"), &factory).await.unwrap();
        let row = repo.get_plugins().into_iter().find(|r| r.id == id).unwrap();
        assert_eq!(row.companion_id.as_deref(), Some("companion_a"));
        assert!(row.public_agent_id.is_none(), "binding a companion must clear the public agent");
    }

    #[tokio::test]
    async fn rebind_channel_public_agent_updates_binding_and_clears_sessions_and_companion() {
        let (mgr, repo, _bc) = make_manager();
        let factory = make_factory();

        // Start companion-bound, then rebind to a public agent.
        let id = mgr
            .enable_plugin(&lark_spec("companion_a"), &lark_config_with_app("cli_app_1"), &factory)
            .await
            .unwrap();
        assert_eq!(repo.get_plugins()[0].companion_id.as_deref(), Some("companion_a"));

        mgr.rebind_channel_public_agent(&id, Some("pubagent_z")).await.unwrap();
        let row = repo.get_plugins().into_iter().find(|r| r.id == id).unwrap();
        assert_eq!(row.public_agent_id.as_deref(), Some("pubagent_z"));
        assert!(row.companion_id.is_none(), "rebinding to a public agent clears the companion");
        assert_eq!(repo.cleared_channels(), vec![id.clone()]);

        // Clearing the binding works too (and clears sessions again).
        mgr.rebind_channel_public_agent(&id, None).await.unwrap();
        assert!(repo.get_plugins()[0].public_agent_id.is_none());
        assert_eq!(repo.cleared_channels().len(), 2);
    }

    #[tokio::test]
    async fn clear_sessions_for_companion_clears_only_bound_channels() {
        let (mgr, repo, _bc) = make_manager();
        let factory = make_factory();

        // Two bots on companion_a, one on companion_b.
        let id_a1 = mgr
            .enable_plugin(&lark_spec("companion_a"), &lark_config_with_app("cli_app_a1"), &factory)
            .await
            .unwrap();
        let id_a2 = mgr
            .enable_plugin(&lark_spec("companion_a"), &lark_config_with_app("cli_app_a2"), &factory)
            .await
            .unwrap();
        let _id_b = mgr
            .enable_plugin(&lark_spec("companion_b"), &lark_config_with_app("cli_app_b"), &factory)
            .await
            .unwrap();

        mgr.clear_sessions_for_companion("companion_a").await;

        let cleared = repo.cleared_channels();
        assert_eq!(cleared.len(), 2, "only companion_a's two channels cleared: {cleared:?}");
        assert!(cleared.contains(&id_a1));
        assert!(cleared.contains(&id_a2));
    }

    #[tokio::test]
    async fn unbind_channels_for_deleted_companion_clears_bindings_and_sessions() {
        let (mgr, repo, _bc) = make_manager();
        let factory = make_factory();

        let id_a = mgr
            .enable_plugin(&lark_spec("companion_a"), &lark_config_with_app("cli_app_a"), &factory)
            .await
            .unwrap();
        let id_b = mgr
            .enable_plugin(&lark_spec("companion_b"), &lark_config_with_app("cli_app_b"), &factory)
            .await
            .unwrap();

        mgr.unbind_channels_for_deleted_companion("companion_a").await;

        let rows = repo.get_plugins();
        let row_a = rows.iter().find(|r| r.id == id_a).unwrap();
        let row_b = rows.iter().find(|r| r.id == id_b).unwrap();
        assert!(row_a.companion_id.is_none());
        assert_eq!(row_b.companion_id.as_deref(), Some("companion_b"));
        assert_eq!(repo.cleared_channels(), vec![id_a]);
    }

    #[tokio::test]
    async fn reconcile_orphaned_owners_unbinds_only_dead_owners() {
        let (mgr, repo, _bc) = make_manager();
        let factory = make_factory();

        let live_id = mgr
            .enable_plugin(&lark_spec("companion_live"), &lark_config_with_app("cli_live"), &factory)
            .await
            .unwrap();
        let ghost_companion_id = mgr
            .enable_plugin(&lark_spec("companion_ghost"), &lark_config_with_app("cli_ghost"), &factory)
            .await
            .unwrap();
        let ghost_pa_id = mgr
            .enable_plugin(
                &lark_public_agent_spec("pubagent_ghost"),
                &lark_config_with_app("cli_pa_ghost"),
                &factory,
            )
            .await
            .unwrap();

        // Only `companion_live` still exists; both ghosts must be unbound.
        let live_companions = std::collections::HashSet::from(["companion_live".to_string()]);
        let live_public_agents: std::collections::HashSet<String> = std::collections::HashSet::new();
        mgr.reconcile_orphaned_owners(&live_companions, &live_public_agents).await;

        let rows = repo.get_plugins();
        let live = rows.iter().find(|r| r.id == live_id).unwrap();
        let ghost_c = rows.iter().find(|r| r.id == ghost_companion_id).unwrap();
        let ghost_p = rows.iter().find(|r| r.id == ghost_pa_id).unwrap();

        assert_eq!(live.companion_id.as_deref(), Some("companion_live"), "live owner preserved");
        assert!(ghost_c.companion_id.is_none(), "ghost companion unbound");
        assert!(ghost_p.public_agent_id.is_none(), "ghost public agent unbound");

        let cleared = repo.cleared_channels();
        assert!(cleared.contains(&ghost_companion_id), "ghost companion sessions cleared");
        assert!(cleared.contains(&ghost_pa_id), "ghost public-agent sessions cleared");
        assert!(!cleared.contains(&live_id), "live row sessions untouched");
    }

    #[tokio::test]
    async fn clear_sessions_for_companion_blank_or_unbound_is_noop() {
        let (mgr, repo, _bc) = make_manager();
        let factory = make_factory();

        mgr.enable_plugin(&lark_spec("companion_a"), &lark_config_with_app("cli_app_a"), &factory)
            .await
            .unwrap();

        // Blank companion id: nothing cleared.
        mgr.clear_sessions_for_companion("   ").await;
        assert!(repo.cleared_channels().is_empty());

        // Unknown companion id: nothing cleared.
        mgr.clear_sessions_for_companion("companion_ghost").await;
        assert!(repo.cleared_channels().is_empty());
    }

    // ── Watchdog ───────────────────────────────────────────────────────

    use crate::plugin::SharedPluginStatus;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// Mock plugin whose status lives in a shared cell, mimicking the real
    /// plugins where the background loop flips the status to `Error` after
    /// reconnect exhaustion.
    struct SharedStatusPlugin {
        status: SharedPluginStatus,
        plugin_type: PluginType,
        fail_init: bool,
    }

    impl SharedStatusPlugin {
        fn new(plugin_type: PluginType, fail_init: bool) -> Self {
            Self {
                status: SharedPluginStatus::default(),
                plugin_type,
                fail_init,
            }
        }
    }

    #[async_trait::async_trait]
    impl ChannelPlugin for SharedStatusPlugin {
        async fn initialize(&mut self, _config: PluginConfig, _callbacks: PluginCallbacks) -> Result<(), ChannelError> {
            if self.fail_init {
                self.status.set(PluginStatus::Error);
                return Err(ChannelError::ConnectionFailed("init failed".into()));
            }
            self.status.set(PluginStatus::Ready);
            Ok(())
        }

        async fn start(&mut self) -> Result<(), ChannelError> {
            self.status.set(PluginStatus::Running);
            Ok(())
        }

        async fn stop(&mut self) -> Result<(), ChannelError> {
            self.status.set(PluginStatus::Stopped);
            Ok(())
        }

        async fn send_message(&self, _chat_id: &str, _message: UnifiedOutgoingMessage) -> Result<String, ChannelError> {
            Ok("mock_msg_id".into())
        }

        async fn edit_message(
            &self,
            _chat_id: &str,
            _message_id: &str,
            _message: UnifiedOutgoingMessage,
        ) -> Result<(), ChannelError> {
            Ok(())
        }

        fn active_user_count(&self) -> usize {
            0
        }

        fn bot_info(&self) -> Option<&BotInfo> {
            None
        }

        fn plugin_type(&self) -> PluginType {
            self.plugin_type
        }

        fn status(&self) -> PluginStatus {
            self.status.get()
        }

        fn last_error(&self) -> Option<&str> {
            None
        }
    }

    /// Records every factory invocation and exposes the shared status handle
    /// of each created plugin so tests can simulate background-loop death.
    struct FactoryProbe {
        handles: Mutex<Vec<SharedPluginStatus>>,
        calls: AtomicUsize,
        /// Creations with index >= this fail `initialize()`.
        fail_from_call: usize,
    }

    impl FactoryProbe {
        fn new(fail_from_call: usize) -> Arc<Self> {
            Arc::new(Self {
                handles: Mutex::new(Vec::new()),
                calls: AtomicUsize::new(0),
                fail_from_call,
            })
        }

        fn calls(&self) -> usize {
            self.calls.load(Ordering::SeqCst)
        }

        fn handle(&self, index: usize) -> SharedPluginStatus {
            self.handles.lock().unwrap()[index].clone()
        }
    }

    fn make_probe_factory(probe: &Arc<FactoryProbe>) -> PluginFactory {
        let probe = Arc::clone(probe);
        Box::new(move |pt| {
            let index = probe.calls.fetch_add(1, Ordering::SeqCst);
            let plugin = SharedStatusPlugin::new(pt, index >= probe.fail_from_call);
            probe.handles.lock().unwrap().push(plugin.status.clone());
            Some(Box::new(plugin))
        })
    }

    fn zero_backoff_config() -> WatchdogConfig {
        WatchdogConfig {
            sweep_interval: Duration::from_millis(10),
            restart_window: Duration::from_secs(3600),
            max_restarts_per_window: 3,
            backoff_base: Duration::ZERO,
        }
    }

    #[test]
    fn watchdog_allow_attempt_enforces_budget_and_backoff() {
        let config = WatchdogConfig {
            sweep_interval: Duration::from_secs(1),
            restart_window: Duration::from_secs(100),
            max_restarts_per_window: 2,
            backoff_base: Duration::from_secs(10),
        };
        let mut state = WatchdogState::default();
        let t0 = Instant::now();

        // First attempt is always allowed.
        assert!(state.allow_attempt("p", &config, t0));
        // Within the backoff window (base * 2^0 = 10s) → blocked.
        assert!(!state.allow_attempt("p", &config, t0 + Duration::from_secs(5)));
        // After the backoff → second attempt allowed.
        assert!(state.allow_attempt("p", &config, t0 + Duration::from_secs(11)));
        // Budget (2) exhausted within the window → blocked even much later.
        assert!(!state.allow_attempt("p", &config, t0 + Duration::from_secs(50)));
        // Window expired → attempts pruned, allowed again.
        assert!(state.allow_attempt("p", &config, t0 + Duration::from_secs(200)));
    }

    #[tokio::test]
    async fn watchdog_restarts_dead_plugin_and_syncs_db() {
        let (mgr, repo, bc) = make_manager();
        let probe = FactoryProbe::new(usize::MAX);
        let factory = make_probe_factory(&probe);

        mgr.enable_plugin(&EnableChannelSpec::legacy("telegram"), &make_test_config(), &factory)
            .await
            .unwrap();
        assert!(mgr.is_plugin_running("telegram"));
        bc.take_events();

        // Simulate the background loop dying after reconnect exhaustion.
        probe.handle(0).set(PluginStatus::Error);
        assert!(!mgr.is_plugin_running("telegram"));

        let mut state = WatchdogState::default();
        mgr.check_and_heal_plugins(&factory, &zero_backoff_config(), &mut state)
            .await;

        // A fresh instance was created and started.
        assert_eq!(probe.calls(), 2);
        assert!(mgr.is_plugin_running("telegram"));
        // DB reflects the restart.
        let plugins = repo.get_plugins();
        assert_eq!(plugins[0].status.as_deref(), Some("running"));
        // Both the error and the recovery were broadcast.
        let events = bc.take_events();
        assert!(events.len() >= 2);
        assert!(events.iter().all(|e| e.name == "channel.plugin-status-changed"));
    }

    #[tokio::test]
    async fn watchdog_failed_restart_persists_error_and_respects_budget() {
        let (mgr, repo, _bc) = make_manager();
        // First creation (enable) is healthy; every restart attempt fails.
        let probe = FactoryProbe::new(1);
        let factory = make_probe_factory(&probe);

        mgr.enable_plugin(&EnableChannelSpec::legacy("telegram"), &make_test_config(), &factory)
            .await
            .unwrap();
        probe.handle(0).set(PluginStatus::Error);

        let config = zero_backoff_config();
        let mut state = WatchdogState::default();
        // Sweep 4 times: only 3 restart attempts may happen per window.
        for _ in 0..4 {
            mgr.check_and_heal_plugins(&factory, &config, &mut state).await;
        }

        // enable (1) + capped restart attempts (3)
        assert_eq!(probe.calls(), 4);
        assert!(!mgr.is_plugin_running("telegram"));
        let plugins = repo.get_plugins();
        assert_eq!(plugins[0].status.as_deref(), Some("error"));
    }

    #[tokio::test]
    async fn watchdog_backoff_blocks_immediate_second_attempt() {
        let (mgr, _repo, _bc) = make_manager();
        let probe = FactoryProbe::new(1);
        let factory = make_probe_factory(&probe);

        mgr.enable_plugin(&EnableChannelSpec::legacy("telegram"), &make_test_config(), &factory)
            .await
            .unwrap();
        probe.handle(0).set(PluginStatus::Error);

        let config = WatchdogConfig {
            backoff_base: Duration::from_secs(3600),
            ..zero_backoff_config()
        };
        let mut state = WatchdogState::default();
        mgr.check_and_heal_plugins(&factory, &config, &mut state).await;
        mgr.check_and_heal_plugins(&factory, &config, &mut state).await;

        // enable (1) + first restart attempt (1); the second sweep is
        // suppressed by the exponential backoff.
        assert_eq!(probe.calls(), 2);
    }

    #[tokio::test]
    async fn watchdog_skips_disabled_plugins() {
        let (mgr, repo, _bc) = make_manager();
        let probe = FactoryProbe::new(usize::MAX);
        let factory = make_probe_factory(&probe);

        mgr.enable_plugin(&EnableChannelSpec::legacy("telegram"), &make_test_config(), &factory)
            .await
            .unwrap();
        probe.handle(0).set(PluginStatus::Error);

        // User disabled the plugin between the death and the sweep.
        {
            let mut plugins = repo.plugins.lock().unwrap();
            plugins[0].enabled = false;
        }

        let mut state = WatchdogState::default();
        mgr.check_and_heal_plugins(&factory, &zero_backoff_config(), &mut state)
            .await;

        // No restart was attempted for the disabled plugin.
        assert_eq!(probe.calls(), 1);
    }

    #[tokio::test]
    async fn watchdog_leaves_healthy_plugins_alone() {
        let (mgr, repo, bc) = make_manager();
        let probe = FactoryProbe::new(usize::MAX);
        let factory = make_probe_factory(&probe);

        mgr.enable_plugin(&EnableChannelSpec::legacy("telegram"), &make_test_config(), &factory)
            .await
            .unwrap();
        bc.take_events();

        let mut state = WatchdogState::default();
        mgr.check_and_heal_plugins(&factory, &zero_backoff_config(), &mut state)
            .await;

        assert_eq!(probe.calls(), 1);
        assert!(mgr.is_plugin_running("telegram"));
        assert!(bc.take_events().is_empty());
        let plugins = repo.get_plugins();
        assert_eq!(plugins[0].status.as_deref(), Some("running"));
    }
}

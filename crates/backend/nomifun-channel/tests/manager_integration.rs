//! Black-box integration tests for `ChannelManager`.
//!
//! Uses real SQLite (in-memory), an owner-scoped event sink, and MockPlugin.
//! Covers test-plan items: PS-1..PS-3, EP-1..EP-5, DP-1..DP-4,
//! TP-1..TP-5, CS-1..CS-2, WS-2.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use nomifun_api_types::WebSocketMessage;
use nomifun_channel::error::ChannelError;
use nomifun_channel::manager::{ChannelManager, EnableChannelSpec, PluginFactory};
use nomifun_channel::plugin::{ChannelPlugin, PluginCallbacks};
use nomifun_channel::types::{
    BotInfo, OutgoingMessageType, PluginConfig, PluginCredentials, PluginStatus, PluginType, UnifiedOutgoingMessage,
};
use nomifun_common::decrypt_string;
use nomifun_db::{IChannelRepository, SqliteChannelRepository, init_database_memory};
use nomifun_realtime::UserEventSink;
use tokio::sync::mpsc;

const OWNER_ID: &str = "018f1234-5678-7abc-8def-012345678960";

fn platform_spec(plugin_type: &str) -> EnableChannelSpec {
    EnableChannelSpec {
        plugin_id: None,
        plugin_type: Some(plugin_type.to_owned()),
        companion_id: None,
        public_agent_id: None,
    }
}

fn existing_spec(channel_plugin_id: &str, plugin_type: &str) -> EnableChannelSpec {
    EnableChannelSpec {
        plugin_id: Some(channel_plugin_id.to_owned()),
        plugin_type: Some(plugin_type.to_owned()),
        companion_id: None,
        public_agent_id: None,
    }
}

// ── Test infrastructure ─────────────────────────────────────────────

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

impl UserEventSink for MockBroadcaster {
    fn send_to_user(&self, _user_id: &str, event: WebSocketMessage<serde_json::Value>) {
        self.events.lock().unwrap().push(event);
    }
}

/// Mock plugin that tracks lifecycle calls.
struct MockPlugin {
    status: PluginStatus,
    plugin_type: PluginType,
    bot_info: Option<BotInfo>,
    last_error: Option<String>,
    should_fail_init: bool,
    start_calls: Arc<AtomicUsize>,
}

impl MockPlugin {
    fn new(plugin_type: PluginType) -> Self {
        Self {
            status: PluginStatus::Created,
            plugin_type,
            bot_info: None,
            last_error: None,
            should_fail_init: false,
            start_calls: Arc::new(AtomicUsize::new(0)),
        }
    }

    fn failing(plugin_type: PluginType) -> Self {
        Self {
            should_fail_init: true,
            ..Self::new(plugin_type)
        }
    }
}

#[async_trait::async_trait]
impl ChannelPlugin for MockPlugin {
    async fn initialize(&mut self, _config: PluginConfig, _callbacks: PluginCallbacks) -> Result<(), ChannelError> {
        if self.should_fail_init {
            self.status = PluginStatus::Error;
            self.last_error = Some("Mock init failure".into());
            return Err(ChannelError::ConnectionFailed("Mock init failure".into()));
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
        self.start_calls.fetch_add(1, Ordering::SeqCst);
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

fn test_key() -> [u8; 32] {
    [0x42; 32]
}

async fn setup() -> (ChannelManager, Arc<dyn IChannelRepository>, Arc<MockBroadcaster>) {
    let db = init_database_memory().await.unwrap();
    let repo: Arc<dyn IChannelRepository> = Arc::new(SqliteChannelRepository::new(db.pool().clone()));
    let bc = Arc::new(MockBroadcaster::new());
    let (msg_tx, _msg_rx) = mpsc::channel(16);
    let (confirm_tx, _confirm_rx) = mpsc::channel(16);
    let mgr = ChannelManager::new(
        repo.clone(),
        bc.clone(),
        OWNER_ID,
        test_key(),
        msg_tx,
        confirm_tx,
    );
    // Keep db alive by leaking — test process exits anyway
    std::mem::forget(db);
    (mgr, repo, bc)
}

fn make_factory() -> PluginFactory {
    Box::new(|pt| Some(Box::new(MockPlugin::new(pt))))
}

fn make_failing_factory() -> PluginFactory {
    Box::new(|pt| Some(Box::new(MockPlugin::failing(pt))))
}

fn make_no_impl_factory() -> PluginFactory {
    Box::new(|_pt| None)
}

fn make_counting_factory() -> (PluginFactory, Arc<AtomicUsize>) {
    let start_calls = Arc::new(AtomicUsize::new(0));
    let captured = Arc::clone(&start_calls);
    let factory = Box::new(move |pt| {
        let mut plugin = MockPlugin::new(pt);
        plugin.start_calls = Arc::clone(&captured);
        Some(Box::new(plugin) as Box<dyn ChannelPlugin>)
    });
    (factory, start_calls)
}

fn make_telegram_config() -> serde_json::Value {
    serde_json::json!({
        "credentials": { "token": "bot:valid123" },
        "config": { "mode": "polling" }
    })
}

fn make_lark_config() -> serde_json::Value {
    serde_json::json!({
        "credentials": {
            "app_id": "cli_abc",
            "app_secret": "secret123"
        }
    })
}

fn make_plugin_config() -> PluginConfig {
    PluginConfig {
        credentials: PluginCredentials {
            token: Some("bot:valid123".into()),
            ..Default::default()
        },
        config: None,
    }
}

fn make_test_outgoing() -> UnifiedOutgoingMessage {
    UnifiedOutgoingMessage {
        message_type: OutgoingMessageType::Text,
        text: Some("hello".into()),
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

// ── PS-1: Get plugin status (no plugins) ──────────────────────────

#[tokio::test]
async fn ps1_get_status_empty() {
    let (mgr, _repo, _bc) = setup().await;
    let statuses = mgr.get_plugin_status().await.unwrap();
    assert!(statuses.is_empty());
}

// ── PS-2: Get plugin status (with plugins) ────────────────────────

#[tokio::test]
async fn ps2_get_status_with_plugins() {
    let (mgr, _repo, _bc) = setup().await;
    let factory = make_factory();

    let channel_id = mgr
        .enable_plugin(&platform_spec("telegram"), &make_telegram_config(), &factory)
        .await
        .unwrap();

    let statuses = mgr.get_plugin_status().await.unwrap();
    assert_eq!(statuses.len(), 1);
    assert_eq!(statuses[0].plugin_id, channel_id);
    assert_eq!(statuses[0].plugin_type, "telegram");
    assert_eq!(statuses[0].name, "Telegram Bot");
    assert!(statuses[0].enabled);
    assert_eq!(statuses[0].status.as_deref(), Some("running"));
}

// ── EP-1: Enable Telegram plugin ──────────────────────────────────

#[tokio::test]
async fn ep1_enable_telegram_plugin() {
    let (mgr, repo, _bc) = setup().await;
    let factory = make_factory();

    let channel_id = mgr
        .enable_plugin(&platform_spec("telegram"), &make_telegram_config(), &factory)
        .await
        .unwrap();

    // Plugin persisted in DB
    let row = repo.get_plugin(&channel_id).await.unwrap().unwrap();
    assert!(row.enabled);
    assert_eq!(row.r#type, "telegram");
    assert_eq!(row.name, "Telegram Bot");
    assert!(row.last_connected.is_some());

    // Plugin is running in memory
    assert!(mgr.is_plugin_running(&channel_id));
    assert_eq!(mgr.active_plugin_count(), 1);
}

// ── EP-2: Re-enable updates config ────────────────────────────────

#[tokio::test]
async fn ep2_re_enable_updates_config() {
    let (mgr, repo, _bc) = setup().await;
    let factory = make_factory();

    let channel_id = mgr
        .enable_plugin(&platform_spec("telegram"), &make_telegram_config(), &factory)
        .await
        .unwrap();

    // Re-enable with different config
    let new_config = serde_json::json!({
        "credentials": { "token": "bot:new_token_456" },
        "config": { "mode": "webhook", "webhook_url": "https://example.com" }
    });
    mgr.enable_plugin(&existing_spec(&channel_id, "telegram"), &new_config, &factory)
        .await
        .unwrap();

    // Still only one plugin
    assert_eq!(mgr.active_plugin_count(), 1);

    // Config should be updated
    let row = repo.get_plugin(&channel_id).await.unwrap().unwrap();
    let decrypted = decrypt_string(&row.config, &test_key()).unwrap();
    let config: PluginConfig = serde_json::from_str(&decrypted).unwrap();
    assert_eq!(config.credentials.token.as_deref(), Some("bot:new_token_456"));
}

// ── EP-5: Invalid plugin ID ──────────────────────────────────────

#[tokio::test]
async fn ep5_invalid_plugin_id() {
    let (mgr, _repo, _bc) = setup().await;
    let factory = make_factory();

    let err = mgr
        .enable_plugin(&platform_spec("nonexistent"), &make_telegram_config(), &factory)
        .await
        .unwrap_err();
    assert!(matches!(err, ChannelError::InvalidPluginType(_)));
}

// ── EP-3/EP-4: Missing required fields ────────────────────────────

#[tokio::test]
async fn ep3_ep4_invalid_config_structure() {
    let (mgr, _repo, _bc) = setup().await;
    let factory = make_factory();

    // Missing credentials entirely
    let bad = serde_json::json!({ "wrong_key": "value" });
    let err = mgr
        .enable_plugin(&platform_spec("telegram"), &bad, &factory)
        .await
        .unwrap_err();
    assert!(matches!(err, ChannelError::InvalidConfig(_)));
}

// ── DP-1: Disable enabled plugin ──────────────────────────────────

#[tokio::test]
async fn dp1_disable_enabled_plugin() {
    let (mgr, repo, _bc) = setup().await;
    let factory = make_factory();

    let channel_id = mgr
        .enable_plugin(&platform_spec("telegram"), &make_telegram_config(), &factory)
        .await
        .unwrap();
    mgr.disable_plugin(&channel_id).await.unwrap();

    assert_eq!(mgr.active_plugin_count(), 0);
    assert!(!mgr.is_plugin_running(&channel_id));

    let row = repo.get_plugin(&channel_id).await.unwrap().unwrap();
    assert!(!row.enabled);
    assert_eq!(row.status.as_deref(), Some("stopped"));
}

// ── DP-2: Disable already disabled (idempotent) ──────────────────

#[tokio::test]
async fn dp2_disable_already_disabled() {
    let (mgr, _repo, _bc) = setup().await;
    let factory = make_factory();

    let channel_id = mgr
        .enable_plugin(&platform_spec("telegram"), &make_telegram_config(), &factory)
        .await
        .unwrap();
    mgr.disable_plugin(&channel_id).await.unwrap();

    // Second disable should not error
    mgr.disable_plugin(&channel_id).await.unwrap();
    assert_eq!(mgr.active_plugin_count(), 0);
}

// ── TP-1: Test valid credentials returns bot username ─────────────

#[tokio::test]
async fn tp1_test_valid_credentials() {
    let (mgr, _repo, _bc) = setup().await;
    let factory = make_factory();

    let result = mgr
        .test_plugin("telegram", make_plugin_config(), &factory)
        .await
        .unwrap();
    assert_eq!(result.as_deref(), Some("mock_bot_user"));
}

#[tokio::test]
async fn test_plugin_initializes_without_starting_runtime() {
    let (mgr, _repo, _bc) = setup().await;
    let (factory, start_calls) = make_counting_factory();

    let username = mgr
        .test_plugin("telegram", make_plugin_config(), &factory)
        .await
        .unwrap();

    assert_eq!(username.as_deref(), Some("mock_bot_user"));
    assert_eq!(
        start_calls.load(Ordering::SeqCst),
        0,
        "credential tests must not start long-running plugin runtime"
    );
}

// ── TP-2: Test invalid credentials propagates error ───────────────

#[tokio::test]
async fn tp2_test_invalid_credentials() {
    let (mgr, _repo, _bc) = setup().await;
    let factory = make_failing_factory();

    let err = mgr.test_plugin("telegram", make_plugin_config(), &factory).await;
    assert!(err.is_err());
}

// ── TP-4: Missing plugin ID ──────────────────────────────────────

#[tokio::test]
async fn tp4_test_invalid_plugin_type() {
    let (mgr, _repo, _bc) = setup().await;
    let factory = make_factory();

    let err = mgr
        .test_plugin("nonexistent", make_plugin_config(), &factory)
        .await
        .unwrap_err();
    assert!(matches!(err, ChannelError::InvalidPluginType(_)));
}

// ── TP: Test does not persist ─────────────────────────────────────

#[tokio::test]
async fn tp_test_does_not_persist() {
    let (mgr, repo, _bc) = setup().await;
    let factory = make_factory();

    mgr.test_plugin("telegram", make_plugin_config(), &factory)
        .await
        .unwrap();

    let plugins = repo.get_all_plugins().await.unwrap();
    assert!(plugins.is_empty());
    assert_eq!(mgr.active_plugin_count(), 0);
}

// ── CS-1: Credentials stored encrypted ────────────────────────────

#[tokio::test]
async fn cs1_credentials_stored_encrypted() {
    let (mgr, repo, _bc) = setup().await;
    let factory = make_factory();

    let channel_id = mgr
        .enable_plugin(&platform_spec("telegram"), &make_telegram_config(), &factory)
        .await
        .unwrap();

    let row = repo.get_plugin(&channel_id).await.unwrap().unwrap();

    // Config should not contain plaintext token
    assert!(!row.config.contains("bot:valid123"));
    assert!(!row.config.contains("token"));

    // Should be valid base64 (encrypted output)
    assert!(base64_looks_valid(&row.config));

    // Decryption should yield the original config
    let decrypted = decrypt_string(&row.config, &test_key()).unwrap();
    let config: PluginConfig = serde_json::from_str(&decrypted).unwrap();
    assert_eq!(config.credentials.token.as_deref(), Some("bot:valid123"));
}

// ── CS-2: Status response does not leak credentials ───────────────

#[tokio::test]
async fn cs2_status_does_not_leak_credentials() {
    let (mgr, _repo, _bc) = setup().await;
    let factory = make_factory();

    mgr.enable_plugin(&platform_spec("telegram"), &make_telegram_config(), &factory)
        .await
        .unwrap();

    let statuses = mgr.get_plugin_status().await.unwrap();
    let json = serde_json::to_string(&statuses).unwrap();

    // No sensitive fields should appear
    assert!(!json.contains("bot:valid123"));
    assert!(!json.contains("credentials"));
    assert!(!json.contains("config"));
    // But the plugin metadata should be there
    assert!(json.contains("telegram"));
    assert!(json.contains("Telegram Bot"));
}

// ── WS-2: Plugin status change event broadcast ───────────────────

#[tokio::test]
async fn ws2_enable_broadcasts_status_change() {
    let (mgr, _repo, bc) = setup().await;
    let factory = make_factory();

    let channel_id = mgr
        .enable_plugin(&platform_spec("telegram"), &make_telegram_config(), &factory)
        .await
        .unwrap();

    let events = bc.take_events();
    let status_events: Vec<_> = events
        .iter()
        .filter(|e| e.name == "channel.plugin-status-changed")
        .collect();
    assert!(!status_events.is_empty());
    assert_eq!(status_events.last().unwrap().data["plugin_id"], channel_id);
}

#[tokio::test]
async fn ws2_disable_broadcasts_status_change() {
    let (mgr, _repo, bc) = setup().await;
    let factory = make_factory();

    let channel_id = mgr
        .enable_plugin(&platform_spec("telegram"), &make_telegram_config(), &factory)
        .await
        .unwrap();
    bc.take_events(); // clear enable events

    mgr.disable_plugin(&channel_id).await.unwrap();

    let events = bc.take_events();
    let status_events: Vec<_> = events
        .iter()
        .filter(|e| e.name == "channel.plugin-status-changed")
        .collect();
    assert!(!status_events.is_empty());
}

// ── Restore: enabled plugins start on restore ────────────────────

#[tokio::test]
async fn restore_starts_enabled_plugins() {
    let (mgr, _repo, _bc) = setup().await;
    let factory = make_factory();

    // First enable and persist a plugin
    let channel_id = mgr
        .enable_plugin(&platform_spec("telegram"), &make_telegram_config(), &factory)
        .await
        .unwrap();

    // Simulate shutdown
    mgr.shutdown().await;
    assert_eq!(mgr.active_plugin_count(), 0);

    // Restore should bring it back
    mgr.restore_plugins(&factory).await.unwrap();
    assert_eq!(mgr.active_plugin_count(), 1);
    assert!(mgr.is_plugin_running(&channel_id));
}

// ── Restore: disabled plugins are skipped ─────────────────────────

#[tokio::test]
async fn restore_skips_disabled_plugins() {
    let (mgr, _repo, _bc) = setup().await;
    let factory = make_factory();

    let channel_id = mgr
        .enable_plugin(&platform_spec("telegram"), &make_telegram_config(), &factory)
        .await
        .unwrap();
    mgr.disable_plugin(&channel_id).await.unwrap();

    mgr.restore_plugins(&factory).await.unwrap();
    assert_eq!(mgr.active_plugin_count(), 0);
}

// ── Multiple plugins ──────────────────────────────────────────────

#[tokio::test]
async fn enable_multiple_plugins() {
    let (mgr, _repo, _bc) = setup().await;
    let factory = make_factory();

    let telegram_id = mgr
        .enable_plugin(&platform_spec("telegram"), &make_telegram_config(), &factory)
        .await
        .unwrap();
    let lark_id = mgr
        .enable_plugin(&platform_spec("lark"), &make_lark_config(), &factory)
        .await
        .unwrap();

    assert_eq!(mgr.active_plugin_count(), 2);
    assert!(mgr.is_plugin_running(&telegram_id));
    assert!(mgr.is_plugin_running(&lark_id));

    let statuses = mgr.get_plugin_status().await.unwrap();
    assert_eq!(statuses.len(), 2);
}

// ── Shutdown stops all ────────────────────────────────────────────

#[tokio::test]
async fn shutdown_stops_all() {
    let (mgr, _repo, _bc) = setup().await;
    let factory = make_factory();

    mgr.enable_plugin(&platform_spec("telegram"), &make_telegram_config(), &factory)
        .await
        .unwrap();
    mgr.enable_plugin(&platform_spec("lark"), &make_lark_config(), &factory)
        .await
        .unwrap();

    mgr.shutdown().await;
    assert_eq!(mgr.active_plugin_count(), 0);
}

// ── Send/Edit message routing ─────────────────────────────────────

#[tokio::test]
async fn send_message_routes_to_plugin() {
    let (mgr, _repo, _bc) = setup().await;
    let factory = make_factory();

    let channel_id = mgr
        .enable_plugin(&platform_spec("telegram"), &make_telegram_config(), &factory)
        .await
        .unwrap();

    let msg_id = mgr
        .send_message(&channel_id, "chat_1", make_test_outgoing())
        .await
        .unwrap();
    assert_eq!(msg_id, "mock_msg_id");
}

#[tokio::test]
async fn send_message_not_running_fails() {
    let (mgr, _repo, _bc) = setup().await;
    let channel_id = "0190f5fe-7c00-7a00-8000-000000000999";

    let err = mgr
        .send_message(channel_id, "chat_1", make_test_outgoing())
        .await
        .unwrap_err();
    assert!(matches!(err, ChannelError::PluginNotFound(_)));
}

#[tokio::test]
async fn edit_message_routes_to_plugin() {
    let (mgr, _repo, _bc) = setup().await;
    let factory = make_factory();

    let channel_id = mgr
        .enable_plugin(&platform_spec("telegram"), &make_telegram_config(), &factory)
        .await
        .unwrap();

    mgr.edit_message(&channel_id, "chat_1", "msg_1", make_test_outgoing())
        .await
        .unwrap();
}

// ── Init failure sets error in DB ─────────────────────────────────

#[tokio::test]
async fn enable_failure_sets_error_in_db() {
    let (mgr, repo, _bc) = setup().await;
    let factory = make_failing_factory();

    let err = mgr
        .enable_plugin(&platform_spec("telegram"), &make_telegram_config(), &factory)
        .await;
    assert!(err.is_err());

    // Plugin should exist in DB with error status
    let row = repo.get_all_plugins().await.unwrap().into_iter().next().unwrap();
    assert_eq!(row.status.as_deref(), Some("error"));
    assert_eq!(mgr.active_plugin_count(), 0);
}

// ── No implementation factory ─────────────────────────────────────

#[tokio::test]
async fn enable_no_implementation_fails() {
    let (mgr, _repo, _bc) = setup().await;
    let factory = make_no_impl_factory();

    let err = mgr
        .enable_plugin(&platform_spec("telegram"), &make_telegram_config(), &factory)
        .await
        .unwrap_err();
    assert!(matches!(err, ChannelError::InvalidPluginType(_)));
}

// ── Helper ────────────────────────────────────────────────────────

fn base64_looks_valid(s: &str) -> bool {
    s.chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '+' || c == '/' || c == '=')
        && s.len() > 20
}

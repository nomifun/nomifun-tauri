//! Black-box integration tests for the Lark (Feishu) plugin.
//!
//! Tests the LarkPlugin through the public ChannelPlugin trait interface
//! and ChannelManager integration.
//!
//! Covers test-plan items: TP-3 (partial — invalid creds), TP-4, EP-5.
//!
//! NOTE: Tests requiring a live Lark API (TP-1, EP-1) are not included.
//! The unit tests within the crate cover pure function logic (event parsing,
//! card building, deduplication, callback encoding/decoding, etc.).

#[cfg(feature = "lark")]
mod lark_tests {
    use std::sync::Mutex;

    use nomifun_api_types::WebSocketMessage;
    use nomifun_channel::manager::{ChannelManager, EnableChannelSpec, PluginFactory};
    use nomifun_channel::plugin::ChannelPlugin;
    use nomifun_channel::plugins::lark::LarkPlugin;
    use nomifun_channel::types::{PluginConfig, PluginCredentials, PluginStatus, PluginType};
    use nomifun_db::{IChannelRepository, SqliteChannelRepository, init_database_memory};
    use nomifun_realtime::UserEventSink;
    use std::sync::Arc;
    use tokio::sync::mpsc;

    // -- Test infrastructure ------------------------------------------------

    struct MockBroadcaster {
        events: Mutex<Vec<WebSocketMessage<serde_json::Value>>>,
    }

    impl MockBroadcaster {
        fn new() -> Self {
            Self {
                events: Mutex::new(Vec::new()),
            }
        }
    }

    impl UserEventSink for MockBroadcaster {
        fn send_to_user(&self, _user_id: &str, event: WebSocketMessage<serde_json::Value>) {
            self.events.lock().unwrap().push(event);
        }
    }

    fn make_encryption_key() -> [u8; 32] {
        [0x42u8; 32]
    }

    async fn setup() -> (ChannelManager, Arc<dyn IChannelRepository>, Arc<MockBroadcaster>) {
        let db = init_database_memory().await.unwrap();
        let repo: Arc<dyn IChannelRepository> = Arc::new(SqliteChannelRepository::new(db.pool().clone()));
        let broadcaster = Arc::new(MockBroadcaster::new());
        let (message_tx, _message_rx) = mpsc::channel(16);
        let (confirm_tx, _confirm_rx) = mpsc::channel(16);

        let manager = ChannelManager::new(
            repo.clone(),
            broadcaster.clone(),
            "018f1234-5678-7abc-8def-012345678971",
            make_encryption_key(),
            message_tx,
            confirm_tx,
        );

        std::mem::forget(db);

        (manager, repo, broadcaster)
    }

    fn lark_factory() -> PluginFactory {
        Box::new(|pt| {
            if pt == PluginType::Lark {
                Some(Box::new(LarkPlugin::new()))
            } else {
                None
            }
        })
    }

    fn make_lark_config(app_id: Option<&str>, app_secret: Option<&str>) -> PluginConfig {
        PluginConfig {
            credentials: PluginCredentials {
                app_id: app_id.map(String::from),
                app_secret: app_secret.map(String::from),
                ..Default::default()
            },
            config: None,
        }
    }

    fn make_lark_config_value(app_id: Option<&str>, app_secret: Option<&str>) -> serde_json::Value {
        let mut creds = serde_json::Map::new();
        if let Some(id) = app_id {
            creds.insert("appId".into(), serde_json::Value::String(id.into()));
        }
        if let Some(secret) = app_secret {
            creds.insert("appSecret".into(), serde_json::Value::String(secret.into()));
        }
        serde_json::json!({
            "credentials": creds,
            "config": { "mode": "websocket" }
        })
    }

    // -- Plugin construction ------------------------------------------------

    #[test]
    fn lark_plugin_initial_state() {
        let plugin = LarkPlugin::new();
        assert_eq!(plugin.status(), PluginStatus::Created);
        assert!(plugin.bot_info().is_none());
        assert!(plugin.last_error().is_none());
        assert_eq!(plugin.plugin_type(), PluginType::Lark);
        assert_eq!(plugin.active_user_count(), 0);
    }

    #[test]
    fn lark_plugin_is_send_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<LarkPlugin>();
    }

    #[test]
    fn lark_plugin_as_trait_object() {
        let plugin = LarkPlugin::new();
        let boxed: Box<dyn ChannelPlugin> = Box::new(plugin);
        assert_eq!(boxed.plugin_type(), PluginType::Lark);
        assert_eq!(boxed.status(), PluginStatus::Created);
    }

    // -- Factory registration -----------------------------------------------

    #[test]
    fn factory_creates_lark_plugin() {
        let factory = lark_factory();
        let plugin = factory(PluginType::Lark);
        assert!(plugin.is_some());
        let plugin = plugin.unwrap();
        assert_eq!(plugin.plugin_type(), PluginType::Lark);
        assert_eq!(plugin.status(), PluginStatus::Created);
    }

    #[test]
    fn factory_returns_none_for_other_types() {
        let factory = lark_factory();
        assert!(factory(PluginType::Telegram).is_none());
        assert!(factory(PluginType::Dingtalk).is_none());
        assert!(factory(PluginType::Weixin).is_none());
    }

    // -- TP-3: Invalid credentials (app_id + app_secret) --------------------

    #[tokio::test]
    async fn test_plugin_invalid_credentials_fails() {
        let (manager, _repo, _bc) = setup().await;
        let factory = lark_factory();

        let config = make_lark_config(Some("invalid_app_id"), Some("invalid_secret"));
        let result = manager.test_plugin("lark", config, &factory).await;
        assert!(result.is_err());
    }

    // -- Missing app_id -----------------------------------------------------

    #[tokio::test]
    async fn test_plugin_missing_app_id_fails() {
        let (manager, _repo, _bc) = setup().await;
        let factory = lark_factory();

        let config = make_lark_config(None, Some("secret123"));
        let result = manager.test_plugin("lark", config, &factory).await;
        assert!(result.is_err());
        let err_msg = result.unwrap_err().to_string();
        assert!(
            err_msg.to_lowercase().contains("app_id"),
            "Error should mention app_id: {err_msg}"
        );
    }

    // -- Missing app_secret -------------------------------------------------

    #[tokio::test]
    async fn test_plugin_missing_app_secret_fails() {
        let (manager, _repo, _bc) = setup().await;
        let factory = lark_factory();

        let config = make_lark_config(Some("cli_123"), None);
        let result = manager.test_plugin("lark", config, &factory).await;
        assert!(result.is_err());
        let err_msg = result.unwrap_err().to_string();
        assert!(
            err_msg.to_lowercase().contains("app_secret"),
            "Error should mention app_secret: {err_msg}"
        );
    }

    // -- Empty credentials --------------------------------------------------

    #[tokio::test]
    async fn test_plugin_empty_credentials_fails() {
        let (manager, _repo, _bc) = setup().await;
        let factory = lark_factory();

        let config = make_lark_config(Some(""), Some(""));
        let result = manager.test_plugin("lark", config, &factory).await;
        assert!(result.is_err());
    }

    // -- EP-5: Invalid plugin type ------------------------------------------

    #[tokio::test]
    async fn enable_invalid_plugin_type_fails() {
        let (manager, _repo, _bc) = setup().await;
        let factory = lark_factory();

        let config = make_lark_config_value(Some("cli_123"), Some("secret"));
        let result = manager
            .enable_plugin(
                &EnableChannelSpec {
                    plugin_id: None,
                    plugin_type: Some("nonexistent".into()),
                    companion_id: None,
                    public_agent_id: None,
                },
                &config,
                &factory,
            )
            .await;
        assert!(result.is_err());
    }

    // -- Enable with invalid credentials ------------------------------------

    #[tokio::test]
    async fn enable_plugin_invalid_credentials_fails() {
        let (manager, _repo, _bc) = setup().await;
        let factory = lark_factory();

        let config = make_lark_config_value(Some("bad_id"), Some("bad_secret"));
        let result = manager
            .enable_plugin(
                &EnableChannelSpec {
                    plugin_id: None,
                    plugin_type: Some("lark".into()),
                    companion_id: None,
                    public_agent_id: None,
                },
                &config,
                &factory,
            )
            .await;
        assert!(result.is_err());
    }

    // -- Disable without DB row ---------------------------------------------

    #[tokio::test]
    async fn disable_without_db_row_returns_error() {
        let (manager, _repo, _bc) = setup().await;
        let result = manager
            .disable_plugin("0190f5fe-7c00-7a00-8000-00000000ffff")
            .await;
        assert!(result.is_err());
    }

    // -- PS-1: Empty plugin status ------------------------------------------

    #[tokio::test]
    async fn get_plugin_status_empty() {
        let (manager, _repo, _bc) = setup().await;
        let statuses = manager.get_plugin_status().await.unwrap();
        assert!(statuses.is_empty());
    }

    // -- Restore with nothing stored ----------------------------------------

    #[tokio::test]
    async fn restore_plugins_none_stored() {
        let (manager, _repo, _bc) = setup().await;
        let factory = lark_factory();
        let result = manager.restore_plugins(&factory).await;
        assert!(result.is_ok());
        assert_eq!(manager.active_plugin_count(), 0);
    }

    // -- Plugin running check -----------------------------------------------

    #[tokio::test]
    async fn is_plugin_running_false_when_not_enabled() {
        let (manager, _repo, _bc) = setup().await;
        assert!(!manager.is_plugin_running(
            "0190f5fe-7c00-7a00-8000-00000000ffff"
        ));
    }
}

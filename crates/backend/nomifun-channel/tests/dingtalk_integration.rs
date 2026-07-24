//! Black-box integration tests for the DingTalk plugin.
//!
//! Tests the DingtalkPlugin through the public ChannelPlugin trait interface
//! and ChannelManager integration.
//!
//! Covers test-plan items: TP-3 (partial — invalid creds), TP-4, EP-5.
//!
//! NOTE: Tests requiring a live DingTalk API (TP-1, EP-1) are not included.
//! The unit tests within the crate cover pure function logic (callback encoding/
//! decoding, chatId encoding/decoding, message extraction, AI Card param
//! building, stream frame parsing, etc.).

#[cfg(feature = "dingtalk")]
mod dingtalk_tests {
    use std::sync::Mutex;

    use nomifun_api_types::WebSocketMessage;
    use nomifun_channel::manager::{ChannelManager, EnableChannelSpec, PluginFactory};
    use nomifun_channel::plugin::ChannelPlugin;
    use nomifun_channel::plugins::dingtalk::DingtalkPlugin;
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
            "018f1234-5678-7abc-8def-012345678972",
            make_encryption_key(),
            message_tx,
            confirm_tx,
        );

        std::mem::forget(db);

        (manager, repo, broadcaster)
    }

    fn dingtalk_factory() -> PluginFactory {
        Box::new(|pt| {
            if pt == PluginType::Dingtalk {
                Some(Box::new(DingtalkPlugin::new()))
            } else {
                None
            }
        })
    }

    fn make_dingtalk_config(client_id: Option<&str>, client_secret: Option<&str>) -> PluginConfig {
        PluginConfig {
            credentials: PluginCredentials {
                client_id: client_id.map(String::from),
                client_secret: client_secret.map(String::from),
                ..Default::default()
            },
            config: None,
        }
    }

    fn make_dingtalk_config_value(client_id: Option<&str>, client_secret: Option<&str>) -> serde_json::Value {
        let mut creds = serde_json::Map::new();
        if let Some(id) = client_id {
            creds.insert("clientId".into(), serde_json::Value::String(id.into()));
        }
        if let Some(secret) = client_secret {
            creds.insert("clientSecret".into(), serde_json::Value::String(secret.into()));
        }
        serde_json::json!({
            "credentials": creds,
            "config": { "mode": "websocket" }
        })
    }

    // -- Plugin construction ------------------------------------------------

    #[test]
    fn dingtalk_plugin_initial_state() {
        let plugin = DingtalkPlugin::new();
        assert_eq!(plugin.status(), PluginStatus::Created);
        assert!(plugin.bot_info().is_none());
        assert!(plugin.last_error().is_none());
        assert_eq!(plugin.plugin_type(), PluginType::Dingtalk);
        assert_eq!(plugin.active_user_count(), 0);
    }

    #[test]
    fn dingtalk_plugin_is_send_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<DingtalkPlugin>();
    }

    #[test]
    fn dingtalk_plugin_as_trait_object() {
        let plugin = DingtalkPlugin::new();
        let boxed: Box<dyn ChannelPlugin> = Box::new(plugin);
        assert_eq!(boxed.plugin_type(), PluginType::Dingtalk);
        assert_eq!(boxed.status(), PluginStatus::Created);
    }

    // -- Factory registration -----------------------------------------------

    #[test]
    fn factory_creates_dingtalk_plugin() {
        let factory = dingtalk_factory();
        let plugin = factory(PluginType::Dingtalk);
        assert!(plugin.is_some());
        let plugin = plugin.unwrap();
        assert_eq!(plugin.plugin_type(), PluginType::Dingtalk);
        assert_eq!(plugin.status(), PluginStatus::Created);
    }

    #[test]
    fn factory_returns_none_for_other_types() {
        let factory = dingtalk_factory();
        assert!(factory(PluginType::Telegram).is_none());
        assert!(factory(PluginType::Lark).is_none());
        assert!(factory(PluginType::Weixin).is_none());
    }

    // -- TP-3: Invalid credentials (client_id + client_secret) ---------------

    #[tokio::test]
    async fn test_plugin_invalid_credentials_fails() {
        let (manager, _repo, _bc) = setup().await;
        let factory = dingtalk_factory();

        let config = make_dingtalk_config(Some("invalid_id"), Some("invalid_secret"));
        let result = manager.test_plugin("dingtalk", config, &factory).await;
        assert!(result.is_err());
    }

    // -- Missing client_id ---------------------------------------------------

    #[tokio::test]
    async fn test_plugin_missing_client_id_fails() {
        let (manager, _repo, _bc) = setup().await;
        let factory = dingtalk_factory();

        let config = make_dingtalk_config(None, Some("secret123"));
        let result = manager.test_plugin("dingtalk", config, &factory).await;
        assert!(result.is_err());
        let err_msg = result.unwrap_err().to_string();
        assert!(
            err_msg.to_lowercase().contains("client_id"),
            "Error should mention client_id: {err_msg}"
        );
    }

    // -- Missing client_secret -----------------------------------------------

    #[tokio::test]
    async fn test_plugin_missing_client_secret_fails() {
        let (manager, _repo, _bc) = setup().await;
        let factory = dingtalk_factory();

        let config = make_dingtalk_config(Some("key_123"), None);
        let result = manager.test_plugin("dingtalk", config, &factory).await;
        assert!(result.is_err());
        let err_msg = result.unwrap_err().to_string();
        assert!(
            err_msg.to_lowercase().contains("client_secret"),
            "Error should mention client_secret: {err_msg}"
        );
    }

    // -- Empty credentials ---------------------------------------------------

    #[tokio::test]
    async fn test_plugin_empty_credentials_fails() {
        let (manager, _repo, _bc) = setup().await;
        let factory = dingtalk_factory();

        let config = make_dingtalk_config(Some(""), Some(""));
        let result = manager.test_plugin("dingtalk", config, &factory).await;
        assert!(result.is_err());
    }

    // -- EP-5: Invalid plugin type -------------------------------------------

    #[tokio::test]
    async fn enable_invalid_plugin_type_fails() {
        let (manager, _repo, _bc) = setup().await;
        let factory = dingtalk_factory();

        let config = make_dingtalk_config_value(Some("key_123"), Some("secret"));
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

    // -- Enable with invalid credentials -------------------------------------

    #[tokio::test]
    async fn enable_plugin_invalid_credentials_fails() {
        let (manager, _repo, _bc) = setup().await;
        let factory = dingtalk_factory();

        let config = make_dingtalk_config_value(Some("bad_id"), Some("bad_secret"));
        let result = manager
            .enable_plugin(
                &EnableChannelSpec {
                    plugin_id: None,
                    plugin_type: Some("dingtalk".into()),
                    companion_id: None,
                    public_agent_id: None,
                },
                &config,
                &factory,
            )
            .await;
        assert!(result.is_err());
    }

    // -- Disable without DB row ----------------------------------------------

    #[tokio::test]
    async fn disable_without_db_row_returns_error() {
        let (manager, _repo, _bc) = setup().await;
        let result = manager
            .disable_plugin("0190f5fe-7c00-7a00-8000-00000000ffff")
            .await;
        assert!(result.is_err());
    }

    // -- PS-1: Empty plugin status -------------------------------------------

    #[tokio::test]
    async fn get_plugin_status_empty() {
        let (manager, _repo, _bc) = setup().await;
        let statuses = manager.get_plugin_status().await.unwrap();
        assert!(statuses.is_empty());
    }

    // -- Restore with nothing stored -----------------------------------------

    #[tokio::test]
    async fn restore_plugins_none_stored() {
        let (manager, _repo, _bc) = setup().await;
        let factory = dingtalk_factory();
        let result = manager.restore_plugins(&factory).await;
        assert!(result.is_ok());
        assert_eq!(manager.active_plugin_count(), 0);
    }

    // -- Plugin running check ------------------------------------------------

    #[tokio::test]
    async fn is_plugin_running_false_when_not_enabled() {
        let (manager, _repo, _bc) = setup().await;
        assert!(!manager.is_plugin_running(
            "0190f5fe-7c00-7a00-8000-00000000ffff"
        ));
    }
}

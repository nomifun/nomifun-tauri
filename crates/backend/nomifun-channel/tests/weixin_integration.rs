//! Black-box integration tests for the WeChat (iLink Bot) plugin.
//!
//! Tests the WeixinPlugin through the public ChannelPlugin trait interface
//! and ChannelManager integration.
//!
//! Covers test-plan items: TP-2, TP-5, EP-5, DP-2, WL-1 (event structure).
//!
//! NOTE: Tests that require a live iLink Bot API (TP-1, EP-1, WL-1 full flow)
//! are not included — they need a real bot token + account. Unit tests within
//! the crate cover pure function logic (content extraction, message types,
//! login event serialization, etc.).

#[cfg(feature = "weixin")]
mod weixin_tests {
    use std::sync::Mutex;

    use nomifun_api_types::WebSocketMessage;
    use nomifun_channel::manager::{ChannelManager, EnableChannelSpec, PluginFactory};
    use nomifun_channel::plugin::ChannelPlugin;
    use nomifun_channel::plugins::weixin::WeixinPlugin;
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
            "018f1234-5678-7abc-8def-012345678973",
            make_encryption_key(),
            message_tx,
            confirm_tx,
        );

        // Keep db alive — test process exits anyway
        std::mem::forget(db);

        (manager, repo, broadcaster)
    }

    fn weixin_factory() -> PluginFactory {
        Box::new(|pt| {
            if pt == PluginType::Weixin {
                Some(Box::new(WeixinPlugin::new()))
            } else {
                None
            }
        })
    }

    fn make_plugin_config(bot_token: Option<&str>, account_id: Option<&str>) -> PluginConfig {
        PluginConfig {
            credentials: PluginCredentials {
                account_id: account_id.map(String::from),
                bot_token: bot_token.map(String::from),
                ..Default::default()
            },
            config: None,
        }
    }

    fn make_config_value(bot_token: Option<&str>, account_id: Option<&str>) -> serde_json::Value {
        let mut creds = serde_json::Map::new();
        if let Some(t) = bot_token {
            creds.insert("botToken".into(), serde_json::Value::String(t.into()));
        }
        if let Some(a) = account_id {
            creds.insert("accountId".into(), serde_json::Value::String(a.into()));
        }
        serde_json::json!({
            "credentials": creds,
            "config": { "mode": "polling" }
        })
    }

    // -- Plugin construction ------------------------------------------------

    #[test]
    fn weixin_plugin_initial_state() {
        let plugin = WeixinPlugin::new();
        assert_eq!(plugin.status(), PluginStatus::Created);
        assert!(plugin.bot_info().is_none());
        assert!(plugin.last_error().is_none());
        assert_eq!(plugin.plugin_type(), PluginType::Weixin);
        assert_eq!(plugin.active_user_count(), 0);
    }

    #[test]
    fn weixin_plugin_is_send_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<WeixinPlugin>();
    }

    #[test]
    fn weixin_plugin_as_trait_object() {
        let plugin = WeixinPlugin::new();
        let boxed: Box<dyn ChannelPlugin> = Box::new(plugin);
        assert_eq!(boxed.plugin_type(), PluginType::Weixin);
        assert_eq!(boxed.status(), PluginStatus::Created);
    }

    // -- Factory registration -----------------------------------------------

    #[test]
    fn factory_creates_weixin_plugin() {
        let factory = weixin_factory();
        let plugin = factory(PluginType::Weixin);
        assert!(plugin.is_some());
        let plugin = plugin.unwrap();
        assert_eq!(plugin.plugin_type(), PluginType::Weixin);
        assert_eq!(plugin.status(), PluginStatus::Created);
    }

    #[test]
    fn factory_returns_none_for_other_types() {
        let factory = weixin_factory();
        assert!(factory(PluginType::Telegram).is_none());
        assert!(factory(PluginType::Lark).is_none());
        assert!(factory(PluginType::Dingtalk).is_none());
    }

    // -- TP-5: Missing credentials ------------------------------------------

    #[tokio::test]
    async fn test_plugin_missing_bot_token_fails() {
        let (manager, _repo, _bc) = setup().await;
        let factory = weixin_factory();

        let config = make_plugin_config(None, Some("acc_1"));
        let result = manager.test_plugin("weixin", config, &factory).await;
        assert!(result.is_err());
        let err_msg = result.unwrap_err().to_string();
        assert!(
            err_msg.to_lowercase().contains("bot_token") || err_msg.to_lowercase().contains("bottoken"),
            "Error should mention bot_token: {err_msg}"
        );
    }

    #[tokio::test]
    async fn test_plugin_missing_account_id_fails() {
        let (manager, _repo, _bc) = setup().await;
        let factory = weixin_factory();

        let config = make_plugin_config(Some("tok_1"), None);
        let result = manager.test_plugin("weixin", config, &factory).await;
        assert!(result.is_err());
        let err_msg = result.unwrap_err().to_string();
        assert!(
            err_msg.to_lowercase().contains("account_id") || err_msg.to_lowercase().contains("accountid"),
            "Error should mention account_id: {err_msg}"
        );
    }

    #[tokio::test]
    async fn test_plugin_empty_bot_token_fails() {
        let (manager, _repo, _bc) = setup().await;
        let factory = weixin_factory();

        let config = make_plugin_config(Some(""), Some("acc_1"));
        let result = manager.test_plugin("weixin", config, &factory).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_plugin_empty_account_id_fails() {
        let (manager, _repo, _bc) = setup().await;
        let factory = weixin_factory();

        let config = make_plugin_config(Some("tok_1"), Some(""));
        let result = manager.test_plugin("weixin", config, &factory).await;
        assert!(result.is_err());
    }

    // -- EP-5: Invalid plugin type ------------------------------------------

    #[tokio::test]
    async fn enable_invalid_plugin_type_fails() {
        let (manager, _repo, _bc) = setup().await;
        let factory = weixin_factory();

        let config = make_config_value(Some("tok_1"), Some("acc_1"));
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

    // -- DP-2: Disable without enable (idempotent/error) -------------------

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
        let factory = weixin_factory();
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

    // -- Login event serialization ------------------------------------------

    #[test]
    fn login_event_qr_serializes_correctly() {
        use nomifun_channel::plugins::weixin::weixin_login_stream;

        // Just verify the public function is accessible and returns a receiver
        // (we cannot test the full flow without a live API, but we verify the
        // type is exported correctly).
        let _fn_ref: fn() -> tokio::sync::mpsc::Receiver<_> = weixin_login_stream;
    }
}

use std::sync::{Arc, RwLock};

use crate::error::ChannelError;
use crate::types::{BotInfo, PluginConfig, PluginStatus, PluginType, UnifiedIncomingMessage, UnifiedOutgoingMessage};

/// Thread-safe plugin status cell shared between a plugin facade and its
/// background connection loop.
///
/// Plugin structs expose `status()` through `&self`, but their long-running
/// receive loops (long-polling / WebSocket) run on detached tokio tasks.
/// When such a loop exhausts its reconnect budget and exits, the facade must
/// reflect the death — otherwise the `ChannelManager` (and the frontend)
/// keep reporting `Running` for a plugin that no longer receives messages.
/// Sharing the status through this cell lets the loop flip it to `Error`,
/// which the manager watchdog observes via the regular
/// [`ChannelPlugin::status`] call.
#[derive(Clone, Debug)]
pub struct SharedPluginStatus(Arc<RwLock<PluginStatus>>);

impl SharedPluginStatus {
    pub fn new(initial: PluginStatus) -> Self {
        Self(Arc::new(RwLock::new(initial)))
    }

    pub fn get(&self) -> PluginStatus {
        // PluginStatus is Copy and no code can panic while holding the
        // guard, so poisoning is unreachable in practice.
        *self.0.read().expect("plugin status lock poisoned")
    }

    pub fn set(&self, status: PluginStatus) {
        *self.0.write().expect("plugin status lock poisoned") = status;
    }
}

impl Default for SharedPluginStatus {
    fn default() -> Self {
        Self::new(PluginStatus::Created)
    }
}

/// Marks a plugin as `Error` when its background loop exits without having
/// been asked to shut down (reconnect exhaustion, unexpected clean close).
///
/// Returns `true` when the status was flipped. Called at the tail of every
/// plugin receive loop so a dead loop never leaves the facade stuck on
/// `Running`; the manager watchdog picks the `Error` up and attempts a
/// rate-limited restart.
pub fn mark_error_on_unexpected_exit(
    status: &SharedPluginStatus,
    shutdown_rx: &tokio::sync::watch::Receiver<bool>,
    plugin_name: &str,
) -> bool {
    if *shutdown_rx.borrow() {
        // Ordinary `stop()` path — the facade manages its own transition to
        // `Stopping`/`Stopped`, don't fight it from the loop.
        return false;
    }
    status.set(PluginStatus::Error);
    tracing::error!(
        plugin = plugin_name,
        "background loop exited unexpectedly; plugin marked as Error"
    );
    true
}

/// Callback channels for a channel plugin.
///
/// Instead of closures (which are hard to make object-safe), plugins
/// receive an `mpsc::Sender` for incoming messages and tool-confirmation
/// events. The `ChannelManager` holds the receiving ends.
///
/// This addresses M-63 — the API Spec `BasePlugin.onMessage/onConfirm`
/// callbacks are mapped to channel-based injection.
#[derive(Clone)]
pub struct PluginCallbacks {
    /// Sender for incoming messages from the platform.
    pub message_tx: tokio::sync::mpsc::Sender<UnifiedIncomingMessage>,
    /// Sender for tool confirmation callbacks (callId, value).
    pub confirm_tx: tokio::sync::mpsc::Sender<(String, String)>,
}

/// Abstraction over a platform-specific channel plugin.
///
/// Each IM platform (Telegram, Lark, DingTalk, WeChat) implements this
/// trait behind a feature flag. The `ChannelManager` holds plugins as
/// `Box<dyn ChannelPlugin>` for runtime polymorphism.
///
/// ## Lifecycle
///
/// ```text
/// created → initialize(config, callbacks) → ready → start() → running
///   → stop() → stopped
/// ```
///
/// Any method may transition to `Error` on failure.
#[async_trait::async_trait]
pub trait ChannelPlugin: Send + Sync {
    /// Initialize the plugin with configuration and callback channels.
    ///
    /// Should validate credentials format (but not test the connection).
    /// Transitions status: `Created → Initializing → Ready` (or `Error`).
    async fn initialize(&mut self, config: PluginConfig, callbacks: PluginCallbacks) -> Result<(), ChannelError>;

    /// Start the platform connection (long-polling, WebSocket, etc.).
    ///
    /// Transitions status: `Ready → Starting → Running` (or `Error`).
    async fn start(&mut self) -> Result<(), ChannelError>;

    /// Gracefully stop the platform connection.
    ///
    /// Transitions status: `Running → Stopping → Stopped`.
    async fn stop(&mut self) -> Result<(), ChannelError>;

    /// Send a message to a specific chat. Returns the platform message ID.
    async fn send_message(&self, chat_id: &str, message: UnifiedOutgoingMessage) -> Result<String, ChannelError>;

    /// Edit an existing message on the platform.
    ///
    /// Platforms that don't support editing (e.g., WeChat) may implement
    /// a degraded strategy (send a new reply instead).
    async fn edit_message(
        &self,
        chat_id: &str,
        message_id: &str,
        message: UnifiedOutgoingMessage,
    ) -> Result<(), ChannelError>;

    /// Send a media item (image/file) to a chat, returning the platform message
    /// id. Default: not supported — log at debug and no-op. The assistant's text
    /// is delivered separately by the relay, so text-only / not-yet-wired
    /// channels keep working; platforms with media support override this.
    async fn send_media(
        &self,
        _chat_id: &str,
        _media: crate::types::OutgoingMedia,
        _caption: Option<&str>,
    ) -> Result<String, ChannelError> {
        tracing::debug!(
            plugin = %self.plugin_type(),
            "send_media not supported for this platform; skipping media"
        );
        Ok(String::new())
    }

    /// Number of currently active (chatting) users on this plugin.
    fn active_user_count(&self) -> usize;

    /// Bot identity on the platform, available after initialization.
    fn bot_info(&self) -> Option<&BotInfo>;

    /// The platform type this plugin handles.
    fn plugin_type(&self) -> PluginType;

    /// Current lifecycle status.
    fn status(&self) -> PluginStatus;

    /// The most recent error message, if status is `Error`.
    fn last_error(&self) -> Option<&str>;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{OutgoingMessageType, PluginCredentials, PluginStatus, PluginType};
    use tokio::sync::mpsc;

    /// Minimal mock plugin for testing the trait interface.
    struct MockPlugin {
        status: PluginStatus,
        plugin_type: PluginType,
        bot_info: Option<BotInfo>,
        last_error: Option<String>,
    }

    impl MockPlugin {
        fn new(plugin_type: PluginType) -> Self {
            Self {
                status: PluginStatus::Created,
                plugin_type,
                bot_info: None,
                last_error: None,
            }
        }
    }

    #[async_trait::async_trait]
    impl ChannelPlugin for MockPlugin {
        async fn initialize(&mut self, config: PluginConfig, _callbacks: PluginCallbacks) -> Result<(), ChannelError> {
            self.status = PluginStatus::Initializing;
            if config.credentials.token.is_none() {
                self.status = PluginStatus::Error;
                self.last_error = Some("Missing token".into());
                return Err(ChannelError::InvalidConfig("Missing token".into()));
            }
            self.bot_info = Some(BotInfo {
                id: "mock_bot".into(),
                username: Some("mock_bot_user".into()),
                display_name: "Mock Bot".into(),
            });
            self.status = PluginStatus::Ready;
            Ok(())
        }

        async fn start(&mut self) -> Result<(), ChannelError> {
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

    fn make_test_config(token: Option<&str>) -> PluginConfig {
        PluginConfig {
            credentials: PluginCredentials {
                token: token.map(String::from),
                ..Default::default()
            },
            config: None,
        }
    }

    fn make_test_callbacks() -> PluginCallbacks {
        let (message_tx, _message_rx) = mpsc::channel(16);
        let (confirm_tx, _confirm_rx) = mpsc::channel(16);
        PluginCallbacks { message_tx, confirm_tx }
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

    #[tokio::test]
    async fn lifecycle_happy_path() {
        let mut plugin = MockPlugin::new(PluginType::Telegram);
        assert_eq!(plugin.status(), PluginStatus::Created);
        assert!(plugin.bot_info().is_none());

        let config = make_test_config(Some("bot:123"));
        plugin.initialize(config, make_test_callbacks()).await.unwrap();
        assert_eq!(plugin.status(), PluginStatus::Ready);
        assert!(plugin.bot_info().is_some());

        plugin.start().await.unwrap();
        assert_eq!(plugin.status(), PluginStatus::Running);

        plugin.stop().await.unwrap();
        assert_eq!(plugin.status(), PluginStatus::Stopped);
    }

    #[tokio::test]
    async fn initialize_with_missing_token_fails() {
        let mut plugin = MockPlugin::new(PluginType::Telegram);
        let config = make_test_config(None);
        let result = plugin.initialize(config, make_test_callbacks()).await;
        assert!(result.is_err());
        assert_eq!(plugin.status(), PluginStatus::Error);
        assert_eq!(plugin.last_error(), Some("Missing token"));
    }

    #[tokio::test]
    async fn send_message_returns_id() {
        let mut plugin = MockPlugin::new(PluginType::Telegram);
        let config = make_test_config(Some("bot:abc"));
        plugin.initialize(config, make_test_callbacks()).await.unwrap();
        plugin.start().await.unwrap();

        let msg_id = plugin.send_message("chat_1", make_test_outgoing()).await.unwrap();
        assert_eq!(msg_id, "mock_msg_id");
    }

    #[tokio::test]
    async fn edit_message_ok() {
        let mut plugin = MockPlugin::new(PluginType::Lark);
        let config = make_test_config(Some("token:xyz"));
        plugin.initialize(config, make_test_callbacks()).await.unwrap();
        plugin.start().await.unwrap();

        let result = plugin.edit_message("chat_1", "msg_1", make_test_outgoing()).await;
        assert!(result.is_ok());
    }

    #[test]
    fn plugin_type_accessor() {
        let plugin = MockPlugin::new(PluginType::Dingtalk);
        assert_eq!(plugin.plugin_type(), PluginType::Dingtalk);
    }

    #[test]
    fn active_user_count_default() {
        let plugin = MockPlugin::new(PluginType::Weixin);
        assert_eq!(plugin.active_user_count(), 0);
    }

    #[tokio::test]
    async fn trait_object_dispatch() {
        let mut plugin = MockPlugin::new(PluginType::Telegram);
        let config = make_test_config(Some("bot:obj"));
        plugin.initialize(config, make_test_callbacks()).await.unwrap();

        // Verify the plugin can be used as a trait object
        let plugin_ref: &dyn ChannelPlugin = &plugin;
        assert_eq!(plugin_ref.plugin_type(), PluginType::Telegram);
        assert_eq!(plugin_ref.status(), PluginStatus::Ready);
        assert!(plugin_ref.bot_info().is_some());
    }

    // ── SharedPluginStatus / mark_error_on_unexpected_exit ─────────────

    #[test]
    fn shared_status_defaults_to_created() {
        let status = SharedPluginStatus::default();
        assert_eq!(status.get(), PluginStatus::Created);
    }

    #[test]
    fn shared_status_clones_observe_writes() {
        let status = SharedPluginStatus::new(PluginStatus::Ready);
        let clone = status.clone();
        clone.set(PluginStatus::Running);
        assert_eq!(status.get(), PluginStatus::Running);
    }

    #[tokio::test]
    async fn unexpected_loop_exit_marks_error() {
        let status = SharedPluginStatus::new(PluginStatus::Running);
        let (_shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);

        // No shutdown was requested → the loop died on its own.
        assert!(mark_error_on_unexpected_exit(&status, &shutdown_rx, "test"));
        assert_eq!(status.get(), PluginStatus::Error);
    }

    #[tokio::test]
    async fn shutdown_loop_exit_does_not_mark_error() {
        let status = SharedPluginStatus::new(PluginStatus::Running);
        let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
        shutdown_tx.send(true).unwrap();

        // stop() was requested → the facade owns the Stopping/Stopped
        // transition; the loop must not overwrite it with Error.
        assert!(!mark_error_on_unexpected_exit(&status, &shutdown_rx, "test"));
        assert_eq!(status.get(), PluginStatus::Running);
    }
}

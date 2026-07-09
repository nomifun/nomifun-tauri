//! Mattermost channel plugin — WebSocket receive + REST send/edit.
//!
//! Mirrors the Telegram plugin lifecycle (initialize/start/stop/send/edit)
//! but uses a persistent WebSocket for inbound events and REST v4 for outbound.
//!
//! ## Interactive buttons
//!
//! Mattermost *does* support interactive message buttons (attachments with
//! `integration.url` callbacks), but they require a **publicly reachable
//! integration callback URL** that the Mattermost server POSTs to when a
//! user clicks a button.  A desktop-local Tauri app cannot host such an
//! endpoint, so **buttons are intentionally not implemented**.  The
//! `message.buttons` field on outgoing messages is silently ignored.
//! Text + reply + streaming edit are fully supported.

use std::sync::Arc;
use std::time::Duration;

use reqwest::Client;
use tokio::sync::{mpsc, watch};
use tokio::task::JoinHandle;
use tracing::{debug, error, info, warn};

use crate::constants::{MATTERMOST_MAX_RECONNECT_ATTEMPTS, MATTERMOST_MAX_RECONNECT_DELAY, MATTERMOST_MESSAGE_LIMIT};
use crate::error::ChannelError;
use crate::plugin::{ChannelPlugin, PluginCallbacks, SharedPluginStatus, mark_error_on_unexpected_exit};
use crate::types::{
    BotInfo, MessageContentType, PluginConfig, PluginStatus, PluginType,
    UnifiedIncomingMessage, UnifiedMessageContent, UnifiedOutgoingMessage, UnifiedUser,
};

use super::api::MattermostApi;
use super::types::{CreatePostRequest, MmPost, UpdatePostRequest, WsAuthChallenge, WsEvent};

/// Mattermost channel plugin.
///
/// Receives messages via WebSocket (`/api/v4/websocket`), sends/edits via
/// REST (`/api/v4/posts`).
pub struct MattermostPlugin {
    status: SharedPluginStatus,
    bot_info: Option<BotInfo>,
    last_error: Option<String>,
    api: Option<Arc<MattermostApi>>,
    callbacks: Option<PluginCallbacks>,
    ws_handle: Option<JoinHandle<()>>,
    shutdown_tx: Option<watch::Sender<bool>>,
    /// The bot's own user id, used for the self-loop guard.
    bot_user_id: Option<String>,
    /// Server URL (stored for WS URL derivation).
    server_url: Option<String>,
    /// Bot access token (stored for WS auth challenge).
    token: Option<String>,
}

impl Default for MattermostPlugin {
    fn default() -> Self {
        Self {
            status: SharedPluginStatus::default(),
            bot_info: None,
            last_error: None,
            api: None,
            callbacks: None,
            ws_handle: None,
            shutdown_tx: None,
            bot_user_id: None,
            server_url: None,
            token: None,
        }
    }
}

impl MattermostPlugin {
    pub fn new() -> Self {
        Self::default()
    }
}

#[async_trait::async_trait]
impl ChannelPlugin for MattermostPlugin {
    async fn initialize(&mut self, config: PluginConfig, callbacks: PluginCallbacks) -> Result<(), ChannelError> {
        self.status.set(PluginStatus::Initializing);

        let server_url = config
            .credentials
            .server_url
            .as_deref()
            .filter(|s| !s.is_empty())
            .ok_or_else(|| {
                self.status.set(PluginStatus::Error);
                self.last_error = Some("Missing Mattermost server_url".into());
                ChannelError::InvalidConfig("Missing Mattermost server_url".into())
            })?
            .to_owned();

        let token = config
            .credentials
            .token
            .as_deref()
            .filter(|t| !t.is_empty())
            .ok_or_else(|| {
                self.status.set(PluginStatus::Error);
                self.last_error = Some("Missing Mattermost bot token".into());
                ChannelError::InvalidConfig("Missing Mattermost bot token".into())
            })?
            .to_owned();

        let client = Client::builder()
            .timeout(Duration::from_secs(30))
            .build()
            .map_err(|e| {
                self.status.set(PluginStatus::Error);
                self.last_error = Some(format!("HTTP client init failed: {e}"));
                ChannelError::ConnectionFailed(format!("HTTP client init failed: {e}"))
            })?;

        let api = Arc::new(MattermostApi::new(client, &server_url, &token));

        // Validate credentials by calling GET /api/v4/users/me
        let me = api.get_me().await.map_err(|e| {
            self.status.set(PluginStatus::Error);
            self.last_error = Some(format!("Token validation failed: {e}"));
            e
        })?;

        self.bot_info = Some(BotInfo {
            id: me.id.clone(),
            username: Some(me.username.clone()),
            display_name: me.username.clone(),
        });
        self.bot_user_id = Some(me.id.clone());

        info!(
            bot_id = %me.id,
            bot_username = %me.username,
            "Mattermost bot initialized"
        );

        self.api = Some(api);
        self.callbacks = Some(callbacks);
        self.server_url = Some(server_url);
        self.token = Some(token);
        self.status.set(PluginStatus::Ready);
        Ok(())
    }

    async fn start(&mut self) -> Result<(), ChannelError> {
        self.status.set(PluginStatus::Starting);

        if self.ws_handle.is_some() {
            self.status.set(PluginStatus::Running);
            return Ok(());
        }

        let callbacks = self
            .callbacks
            .clone()
            .ok_or_else(|| ChannelError::PlatformApi("Mattermost callbacks not initialized".into()))?;

        let server_url = self
            .server_url
            .clone()
            .ok_or_else(|| ChannelError::PlatformApi("Mattermost server_url not set".into()))?;

        let token = self
            .token
            .clone()
            .ok_or_else(|| ChannelError::PlatformApi("Mattermost token not set".into()))?;

        let bot_user_id = self
            .bot_user_id
            .clone()
            .ok_or_else(|| ChannelError::PlatformApi("Mattermost bot_user_id not set".into()))?;

        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        self.shutdown_tx = Some(shutdown_tx);

        self.ws_handle = Some(tokio::spawn(ws_loop(
            server_url,
            token,
            bot_user_id,
            callbacks.message_tx,
            self.status.clone(),
            shutdown_rx,
        )));

        self.status.set(PluginStatus::Running);
        info!("Mattermost plugin started");
        Ok(())
    }

    async fn stop(&mut self) -> Result<(), ChannelError> {
        self.status.set(PluginStatus::Stopping);

        if let Some(tx) = self.shutdown_tx.take() {
            let _ = tx.send(true);
        }

        if let Some(handle) = self.ws_handle.take() {
            let _ = tokio::time::timeout(Duration::from_secs(5), handle).await;
        }

        self.api = None;
        self.callbacks = None;
        self.status.set(PluginStatus::Stopped);
        info!("Mattermost plugin stopped");
        Ok(())
    }

    async fn send_message(&self, chat_id: &str, message: UnifiedOutgoingMessage) -> Result<String, ChannelError> {
        let api = self
            .api
            .as_ref()
            .ok_or_else(|| ChannelError::PlatformApi("Plugin not initialized".into()))?;

        let text = truncate_message(message.text.as_deref().unwrap_or(""), MATTERMOST_MESSAGE_LIMIT);

        // NOTE: message.buttons is intentionally ignored — see module-level doc
        // comment. Mattermost interactive buttons require a public callback URL
        // that a desktop app cannot host.

        let root_id = message
            .reply_to_message_id
            .as_deref()
            .filter(|s| !s.is_empty())
            .map(String::from);

        let req = CreatePostRequest {
            channel_id: chat_id.to_owned(),
            message: text,
            root_id,
            file_ids: Vec::new(),
        };

        let resp = api.create_post(&req).await?;
        Ok(resp.id)
    }

    async fn send_media(
        &self,
        chat_id: &str,
        media: crate::types::OutgoingMedia,
        caption: Option<&str>,
    ) -> Result<String, ChannelError> {
        // Image and File both take the same two-step path: upload the bytes to
        // /api/v4/files for a file id, then create a post attaching that id.
        let api = self
            .api
            .as_ref()
            .ok_or_else(|| ChannelError::PlatformApi("Plugin not initialized".into()))?;

        let file_id = api
            .upload_file(chat_id, media.bytes, &media.filename, &media.mime)
            .await?;

        let req = CreatePostRequest {
            channel_id: chat_id.to_owned(),
            message: caption.unwrap_or("").to_owned(),
            root_id: None,
            file_ids: vec![file_id],
        };

        let resp = api.create_post(&req).await?;
        Ok(resp.id)
    }

    async fn edit_message(
        &self,
        _chat_id: &str,
        message_id: &str,
        message: UnifiedOutgoingMessage,
    ) -> Result<(), ChannelError> {
        let api = self
            .api
            .as_ref()
            .ok_or_else(|| ChannelError::PlatformApi("Plugin not initialized".into()))?;

        let text = truncate_message(message.text.as_deref().unwrap_or(""), MATTERMOST_MESSAGE_LIMIT);

        let req = UpdatePostRequest {
            id: message_id.to_owned(),
            message: text,
        };

        api.update_post(&req).await
    }

    fn active_user_count(&self) -> usize {
        0
    }

    fn bot_info(&self) -> Option<&BotInfo> {
        self.bot_info.as_ref()
    }

    fn plugin_type(&self) -> PluginType {
        PluginType::Mattermost
    }

    fn status(&self) -> PluginStatus {
        self.status.get()
    }

    fn last_error(&self) -> Option<&str> {
        self.last_error.as_deref()
    }
}

// ---------------------------------------------------------------------------
// WebSocket loop
// ---------------------------------------------------------------------------

/// Background task: connect to Mattermost WS, authenticate, listen for events.
///
/// Reconnects with exponential backoff up to `MATTERMOST_MAX_RECONNECT_ATTEMPTS`
/// consecutive failures. On exhaustion, calls `mark_error_on_unexpected_exit`.
async fn ws_loop(
    server_url: String,
    token: String,
    bot_user_id: String,
    message_tx: mpsc::Sender<UnifiedIncomingMessage>,
    status: SharedPluginStatus,
    mut shutdown_rx: watch::Receiver<bool>,
) {
    let mut consecutive_errors: u32 = 0;

    loop {
        if *shutdown_rx.borrow() {
            debug!("Mattermost WS loop received shutdown signal");
            break;
        }

        let ws_url = derive_ws_url(&server_url);

        match connect_and_listen(
            &ws_url,
            &token,
            &bot_user_id,
            &message_tx,
            &mut shutdown_rx,
        )
        .await
        {
            Ok(()) => {
                // Clean disconnect (e.g. shutdown requested inside listen)
                if *shutdown_rx.borrow() {
                    break;
                }
                // Server closed; reconnect
                consecutive_errors += 1;
                warn!("Mattermost WS closed, reconnecting ({consecutive_errors})");
            }
            Err(e) => {
                consecutive_errors += 1;
                warn!(
                    error = %e,
                    consecutive_errors,
                    "Mattermost WS error"
                );
            }
        }

        if consecutive_errors >= MATTERMOST_MAX_RECONNECT_ATTEMPTS {
            error!("Mattermost max reconnect attempts reached, stopping WS loop");
            break;
        }

        let backoff = backoff_delay(consecutive_errors);
        tokio::select! {
            _ = tokio::time::sleep(backoff) => {}
            _ = shutdown_rx.changed() => {
                debug!("Mattermost WS loop shutdown during backoff");
                break;
            }
        }
    }

    mark_error_on_unexpected_exit(&status, &shutdown_rx, "mattermost");
    debug!("Mattermost WS loop exited");
}

/// Connect to the Mattermost WebSocket, send auth challenge, and listen.
async fn connect_and_listen(
    ws_url: &str,
    token: &str,
    bot_user_id: &str,
    message_tx: &mpsc::Sender<UnifiedIncomingMessage>,
    shutdown_rx: &mut watch::Receiver<bool>,
) -> Result<(), ChannelError> {
    use futures_util::{SinkExt, StreamExt};
    use tokio_tungstenite::tungstenite::Message as WsMessage;

    let connector = build_ws_tls_connector()?;
    let (ws_stream, _) =
        tokio_tungstenite::connect_async_tls_with_config(ws_url, None, false, Some(connector))
            .await
            .map_err(|e| ChannelError::ConnectionFailed(format!("Mattermost WS connect failed: {e}")))?;

    info!("Mattermost WebSocket connected");

    let (mut write, mut read) = ws_stream.split();

    // Send authentication challenge
    let auth = WsAuthChallenge::new(token);
    let auth_json = serde_json::to_string(&auth)
        .map_err(|e| ChannelError::PlatformApi(format!("Auth challenge serialize failed: {e}")))?;
    write
        .send(WsMessage::Text(auth_json.into()))
        .await
        .map_err(|e| ChannelError::ConnectionFailed(format!("Mattermost auth send failed: {e}")))?;

    debug!("Mattermost auth challenge sent");

    loop {
        tokio::select! {
            msg = read.next() => {
                match msg {
                    Some(Ok(WsMessage::Text(text))) => {
                        handle_ws_text(&text, bot_user_id, message_tx).await;
                    }
                    Some(Ok(WsMessage::Close(_))) => {
                        debug!("Mattermost WS received close frame");
                        return Ok(());
                    }
                    Some(Err(e)) => {
                        return Err(ChannelError::ConnectionFailed(
                            format!("Mattermost WS read error: {e}")
                        ));
                    }
                    None => {
                        return Err(ChannelError::ConnectionFailed(
                            "Mattermost WS stream ended unexpectedly".into()
                        ));
                    }
                    // Binary, Ping, Pong, Frame — tokio-tungstenite handles
                    // ping/pong automatically at the protocol level.
                    _ => {}
                }
            }
            _ = shutdown_rx.changed() => {
                debug!("Mattermost WS shutdown during listen");
                return Ok(());
            }
        }
    }
}

/// Process a single WS text frame.
async fn handle_ws_text(
    text: &str,
    bot_user_id: &str,
    message_tx: &mpsc::Sender<UnifiedIncomingMessage>,
) {
    let event: WsEvent = match serde_json::from_str(text) {
        Ok(e) => e,
        Err(_) => return, // auth responses, status frames, etc.
    };

    if event.event.as_deref() != Some("posted") {
        return;
    }

    let data = match event.data {
        Some(d) => d,
        None => return,
    };

    let post_str = match &data.post {
        Some(s) => s,
        None => return,
    };

    let post: MmPost = match serde_json::from_str(post_str) {
        Ok(p) => p,
        Err(e) => {
            warn!(error = %e, "Failed to parse Mattermost post JSON");
            return;
        }
    };

    // Bot-loop guard: skip own messages
    if post.user_id == bot_user_id {
        return;
    }

    let channel_type = data.channel_type.as_deref().unwrap_or("");

    // Mention gating: for non-DM channels, only process if the bot is mentioned
    if channel_type != "D" {
        if !is_bot_mentioned(&data.mentions, &post.message, bot_user_id) {
            return;
        }
    }

    let sender_name = data
        .sender_name
        .as_deref()
        .unwrap_or("")
        .trim_start_matches('@')
        .to_owned();

    let reply_to = if post.root_id.is_empty() {
        None
    } else {
        Some(post.root_id.clone())
    };

    let unified = UnifiedIncomingMessage {
        id: post.id,
        platform: PluginType::Mattermost,
        chat_id: post.channel_id,
        user: UnifiedUser {
            id: post.user_id,
            username: Some(sender_name.clone()),
            display_name: sender_name,
            avatar_url: None,
        },
        content: UnifiedMessageContent {
            content_type: MessageContentType::Text,
            text: post.message,
            attachments: None,
        },
        timestamp: chrono_now(),
        reply_to_message_id: reply_to,
        action: None,
        raw: None,
    };

    let _ = message_tx.send(unified).await;
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Derive the WebSocket URL from a server URL.
///
/// `https://` → `wss://`, `http://` → `ws://`; appends `/api/v4/websocket`.
pub(crate) fn derive_ws_url(server_url: &str) -> String {
    let base = server_url.trim_end_matches('/');
    let ws_base = if base.starts_with("https://") {
        base.replacen("https://", "wss://", 1)
    } else if base.starts_with("http://") {
        base.replacen("http://", "ws://", 1)
    } else {
        // Fallback: assume wss
        format!("wss://{base}")
    };
    format!("{ws_base}/api/v4/websocket")
}

/// Check if the bot is mentioned in a `"posted"` event.
///
/// Returns `true` if:
/// - The `data.mentions` JSON array contains the bot's user id, OR
/// - The post message text contains `@<bot_username>` (not checked here — the
///   caller should use `bot_info.username`; we rely on the server-side mentions
///   array which is the canonical source).
///
/// For robustness we also accept `@<bot_user_id>` in the text, though
/// Mattermost normally uses username-based mentions.
pub(crate) fn is_bot_mentioned(mentions_json: &Option<String>, message: &str, bot_user_id: &str) -> bool {
    // Primary: check the mentions array from the WS event data
    if let Some(mentions_str) = mentions_json {
        if let Ok(ids) = serde_json::from_str::<Vec<String>>(mentions_str) {
            if ids.iter().any(|id| id == bot_user_id) {
                return true;
            }
        }
    }

    // Fallback: check if message contains @bot_user_id (edge case)
    if message.contains(&format!("@{bot_user_id}")) {
        return true;
    }

    false
}

/// Truncate a message to the platform limit, appending "..." if truncated.
pub(crate) fn truncate_message(text: &str, limit: usize) -> String {
    if text.len() <= limit {
        return text.to_string();
    }
    let truncated: String = text.chars().take(limit - 3).collect();
    format!("{truncated}...")
}

/// Exponential backoff delay, capped.
fn backoff_delay(attempt: u32) -> Duration {
    let delay_secs = 2u64.saturating_pow(attempt).min(MATTERMOST_MAX_RECONNECT_DELAY.as_secs());
    Duration::from_secs(delay_secs)
}

/// Build a TLS connector for WebSocket connections (mirrors Lark).
///
/// Explicitly sets ALPN to `http/1.1` — WebSocket requires an HTTP/1.1
/// upgrade handshake and is incompatible with h2.
fn build_ws_tls_connector() -> Result<tokio_tungstenite::Connector, ChannelError> {
    use std::sync::Arc;
    use tokio_tungstenite::Connector;

    let certs = rustls_native_certs::load_native_certs();
    let mut root_store = rustls::RootCertStore::empty();
    root_store.add_parsable_certificates(certs.certs);

    let provider = rustls::crypto::CryptoProvider::get_default()
        .cloned()
        .unwrap_or_else(|| Arc::new(rustls::crypto::ring::default_provider()));

    let mut config = rustls::ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .map_err(|e| ChannelError::ConnectionFailed(format!("TLS config error: {e}")))?
        .with_root_certificates(root_store)
        .with_no_client_auth();
    config.alpn_protocols = vec![b"http/1.1".to_vec()];

    Ok(Connector::Rustls(Arc::new(config)))
}

/// Current unix timestamp in seconds.
fn chrono_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // -- derive_ws_url -------------------------------------------------------

    #[test]
    fn ws_url_from_https() {
        assert_eq!(
            derive_ws_url("https://mm.example.com"),
            "wss://mm.example.com/api/v4/websocket"
        );
    }

    #[test]
    fn ws_url_from_http() {
        assert_eq!(
            derive_ws_url("http://localhost:8065"),
            "ws://localhost:8065/api/v4/websocket"
        );
    }

    #[test]
    fn ws_url_strips_trailing_slash() {
        assert_eq!(
            derive_ws_url("https://mm.example.com/"),
            "wss://mm.example.com/api/v4/websocket"
        );
    }

    #[test]
    fn ws_url_bare_host_gets_wss() {
        assert_eq!(
            derive_ws_url("mm.example.com"),
            "wss://mm.example.com/api/v4/websocket"
        );
    }

    // -- is_bot_mentioned ----------------------------------------------------

    #[test]
    fn mentioned_in_mentions_array() {
        let mentions = Some(r#"["user1","bot123","user2"]"#.to_string());
        assert!(is_bot_mentioned(&mentions, "hello", "bot123"));
    }

    #[test]
    fn not_mentioned_in_mentions_array() {
        let mentions = Some(r#"["user1","user2"]"#.to_string());
        assert!(!is_bot_mentioned(&mentions, "hello", "bot123"));
    }

    #[test]
    fn mentioned_no_mentions_field() {
        assert!(!is_bot_mentioned(&None, "hello", "bot123"));
    }

    #[test]
    fn mentioned_via_message_text_fallback() {
        assert!(is_bot_mentioned(&None, "hey @bot123 help", "bot123"));
    }

    #[test]
    fn not_mentioned_anywhere() {
        let mentions = Some(r#"["user1"]"#.to_string());
        assert!(!is_bot_mentioned(&mentions, "hello world", "bot123"));
    }

    #[test]
    fn mentioned_empty_mentions_but_in_text() {
        let mentions = Some(r#"[]"#.to_string());
        assert!(is_bot_mentioned(&mentions, "cc @bot123", "bot123"));
    }

    #[test]
    fn mentioned_malformed_mentions_json() {
        let mentions = Some("not-json".to_string());
        // Falls through to text check
        assert!(is_bot_mentioned(&mentions, "@bot123", "bot123"));
        assert!(!is_bot_mentioned(&mentions, "hello", "bot123"));
    }

    // -- truncate_message ----------------------------------------------------

    #[test]
    fn truncate_within_limit() {
        assert_eq!(truncate_message("Hello", 100), "Hello");
    }

    #[test]
    fn truncate_at_limit() {
        assert_eq!(truncate_message("abc", 3), "abc");
    }

    #[test]
    fn truncate_exceeds_limit() {
        let result = truncate_message("Hello, world!", 10);
        assert_eq!(result, "Hello, ...");
        assert!(result.len() <= 10);
    }

    #[test]
    fn truncate_unicode() {
        let text = "abcdefghij"; // 10 chars
        let result = truncate_message(text, 8);
        assert_eq!(result, "abcde...");
    }

    // -- backoff_delay -------------------------------------------------------

    #[test]
    fn backoff_exponential() {
        assert_eq!(backoff_delay(1), Duration::from_secs(2));
        assert_eq!(backoff_delay(2), Duration::from_secs(4));
        assert_eq!(backoff_delay(3), Duration::from_secs(8));
    }

    #[test]
    fn backoff_capped() {
        assert_eq!(backoff_delay(5), Duration::from_secs(30));
        assert_eq!(backoff_delay(10), Duration::from_secs(30));
    }

    // -- MattermostPlugin constructor ----------------------------------------

    #[test]
    fn new_plugin_initial_state() {
        let plugin = MattermostPlugin::new();
        assert_eq!(plugin.status(), PluginStatus::Created);
        assert!(plugin.bot_info().is_none());
        assert!(plugin.last_error().is_none());
        assert_eq!(plugin.plugin_type(), PluginType::Mattermost);
        assert_eq!(plugin.active_user_count(), 0);
    }

    // -- handle_ws_text (unit tests via faked JSON) --------------------------

    #[tokio::test]
    async fn handle_posted_event() {
        let (tx, mut rx) = mpsc::channel(16);
        let post = serde_json::json!({
            "id": "post1",
            "channel_id": "chan1",
            "user_id": "user1",
            "message": "Hello bot",
            "root_id": "",
            "file_ids": []
        });
        let event = serde_json::json!({
            "event": "posted",
            "data": {
                "post": serde_json::to_string(&post).unwrap(),
                "channel_type": "D",
                "sender_name": "@alice"
            }
        });

        handle_ws_text(
            &serde_json::to_string(&event).unwrap(),
            "bot_id",
            &tx,
        )
        .await;

        let msg = rx.try_recv().unwrap();
        assert_eq!(msg.id, "post1");
        assert_eq!(msg.chat_id, "chan1");
        assert_eq!(msg.user.id, "user1");
        assert_eq!(msg.user.username.as_deref(), Some("alice"));
        assert_eq!(msg.content.text, "Hello bot");
        assert_eq!(msg.platform, PluginType::Mattermost);
    }

    #[tokio::test]
    async fn handle_posted_skips_bot_own_message() {
        let (tx, mut rx) = mpsc::channel(16);
        let post = serde_json::json!({
            "id": "post2",
            "channel_id": "chan1",
            "user_id": "bot_id",  // same as bot
            "message": "I said this"
        });
        let event = serde_json::json!({
            "event": "posted",
            "data": {
                "post": serde_json::to_string(&post).unwrap(),
                "channel_type": "D"
            }
        });

        handle_ws_text(
            &serde_json::to_string(&event).unwrap(),
            "bot_id",
            &tx,
        )
        .await;

        assert!(rx.try_recv().is_err(), "Bot's own message should be skipped");
    }

    #[tokio::test]
    async fn handle_posted_channel_requires_mention() {
        let (tx, mut rx) = mpsc::channel(16);
        let post = serde_json::json!({
            "id": "post3",
            "channel_id": "chan1",
            "user_id": "user1",
            "message": "hello"
        });
        // Open channel, no mentions
        let event = serde_json::json!({
            "event": "posted",
            "data": {
                "post": serde_json::to_string(&post).unwrap(),
                "channel_type": "O",
                "sender_name": "@alice"
            }
        });

        handle_ws_text(
            &serde_json::to_string(&event).unwrap(),
            "bot_id",
            &tx,
        )
        .await;

        assert!(rx.try_recv().is_err(), "Non-DM without mention should be skipped");
    }

    #[tokio::test]
    async fn handle_posted_channel_with_mention_passes() {
        let (tx, mut rx) = mpsc::channel(16);
        let post = serde_json::json!({
            "id": "post4",
            "channel_id": "chan1",
            "user_id": "user1",
            "message": "@bot_id help me"
        });
        let event = serde_json::json!({
            "event": "posted",
            "data": {
                "post": serde_json::to_string(&post).unwrap(),
                "channel_type": "O",
                "sender_name": "@alice",
                "mentions": "[\"bot_id\"]"
            }
        });

        handle_ws_text(
            &serde_json::to_string(&event).unwrap(),
            "bot_id",
            &tx,
        )
        .await;

        let msg = rx.try_recv().unwrap();
        assert_eq!(msg.id, "post4");
    }

    #[tokio::test]
    async fn handle_non_posted_event_ignored() {
        let (tx, mut rx) = mpsc::channel(16);
        let event = serde_json::json!({
            "event": "typing",
            "data": {}
        });

        handle_ws_text(
            &serde_json::to_string(&event).unwrap(),
            "bot_id",
            &tx,
        )
        .await;

        assert!(rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn handle_auth_response_ignored() {
        let (tx, mut rx) = mpsc::channel(16);
        let frame = r#"{"status":"OK","seq_reply":1}"#;

        handle_ws_text(frame, "bot_id", &tx).await;

        assert!(rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn handle_posted_with_thread_reply() {
        let (tx, mut rx) = mpsc::channel(16);
        let post = serde_json::json!({
            "id": "post5",
            "channel_id": "chan1",
            "user_id": "user1",
            "message": "threaded reply",
            "root_id": "root_post_1"
        });
        let event = serde_json::json!({
            "event": "posted",
            "data": {
                "post": serde_json::to_string(&post).unwrap(),
                "channel_type": "D"
            }
        });

        handle_ws_text(
            &serde_json::to_string(&event).unwrap(),
            "bot_id",
            &tx,
        )
        .await;

        let msg = rx.try_recv().unwrap();
        assert_eq!(msg.reply_to_message_id.as_deref(), Some("root_post_1"));
    }
}

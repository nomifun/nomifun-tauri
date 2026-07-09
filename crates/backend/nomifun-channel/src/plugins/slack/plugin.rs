//! Slack channel plugin — Socket Mode WebSocket connection.
//!
//! Mirrors the Telegram/Lark plugin structure:
//! - `initialize`: validate via `auth.test`
//! - `start`: open Socket Mode WS, listen for events
//! - `stop`: signal shutdown, wait for loop exit
//! - `send_message` / `edit_message`: REST via `chat.postMessage` / `chat.update`

use std::sync::Arc;
use std::time::Duration;

use reqwest::Client;
use tokio::sync::{mpsc, watch};
use tokio::task::JoinHandle;
use tracing::{debug, error, info, warn};

use crate::constants::{SLACK_MAX_RECONNECT_ATTEMPTS, SLACK_MAX_RECONNECT_DELAY, SLACK_MESSAGE_LIMIT};
use crate::error::ChannelError;
use crate::plugin::{ChannelPlugin, PluginCallbacks, SharedPluginStatus, mark_error_on_unexpected_exit};
use crate::plugins::callback::{format_callback_data, parse_callback_data};
use crate::types::{
    ActionContext, BotInfo, MessageContentType, PluginConfig, PluginStatus,
    PluginType, UnifiedAction, UnifiedIncomingMessage, UnifiedMessageContent,
    UnifiedOutgoingMessage, UnifiedUser,
};

use super::api::SlackApi;
use super::types::{
    ActionsBlock, ButtonElement, EventsApiPayload, InteractivePayload, PlainTextObject,
    PostMessageRequest, SocketAck, SocketEnvelope, UpdateMessageRequest,
};

/// Slack channel plugin implementing Socket Mode (fully outbound WebSocket).
pub struct SlackPlugin {
    /// Shared with the WS loop so a dead loop can flip it to `Error`.
    status: SharedPluginStatus,
    bot_info: Option<BotInfo>,
    last_error: Option<String>,
    api: Option<Arc<SlackApi>>,
    callbacks: Option<PluginCallbacks>,
    ws_handle: Option<JoinHandle<()>>,
    shutdown_tx: Option<watch::Sender<bool>>,
    /// The bot's own user ID (for self-loop guard + mention gating).
    bot_user_id: Option<String>,
}

impl Default for SlackPlugin {
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
        }
    }
}

impl SlackPlugin {
    pub fn new() -> Self {
        Self::default()
    }
}

#[async_trait::async_trait]
impl ChannelPlugin for SlackPlugin {
    async fn initialize(&mut self, config: PluginConfig, callbacks: PluginCallbacks) -> Result<(), ChannelError> {
        self.status.set(PluginStatus::Initializing);

        let bot_token = config
            .credentials
            .token
            .as_deref()
            .filter(|t| !t.is_empty())
            .ok_or_else(|| {
                self.status.set(PluginStatus::Error);
                self.last_error = Some("Missing Slack bot token (xoxb-)".into());
                ChannelError::InvalidConfig("Missing Slack bot token (xoxb-)".into())
            })?;

        let app_token = config
            .credentials
            .app_token
            .as_deref()
            .filter(|t| !t.is_empty())
            .ok_or_else(|| {
                self.status.set(PluginStatus::Error);
                self.last_error = Some("Missing Slack app-level token (xapp-)".into());
                ChannelError::InvalidConfig("Missing Slack app-level token (xapp-)".into())
            })?;

        let client = Client::builder()
            .timeout(Duration::from_secs(30))
            .build()
            .map_err(|e| {
                self.status.set(PluginStatus::Error);
                self.last_error = Some(format!("HTTP client init failed: {e}"));
                ChannelError::ConnectionFailed(format!("HTTP client init failed: {e}"))
            })?;

        let api = Arc::new(SlackApi::new(client, bot_token, app_token));

        // Validate bot token via auth.test
        let auth = api.auth_test().await.map_err(|e| {
            self.status.set(PluginStatus::Error);
            self.last_error = Some(format!("Token validation failed: {e}"));
            e
        })?;

        let user_id = auth.user_id.unwrap_or_default();
        let username = auth.user.clone();
        let display_name = auth.user.unwrap_or_else(|| user_id.clone());

        self.bot_info = Some(BotInfo {
            id: user_id.clone(),
            username: username.clone(),
            display_name,
        });
        self.bot_user_id = Some(user_id.clone());

        info!(
            bot_id = %user_id,
            bot_username = ?username,
            "Slack bot initialized"
        );

        self.api = Some(api);
        self.callbacks = Some(callbacks);
        self.status.set(PluginStatus::Ready);
        Ok(())
    }

    async fn start(&mut self) -> Result<(), ChannelError> {
        self.status.set(PluginStatus::Starting);

        if self.ws_handle.is_some() {
            self.status.set(PluginStatus::Running);
            return Ok(());
        }

        let api = self
            .api
            .as_ref()
            .cloned()
            .ok_or_else(|| ChannelError::PlatformApi("Slack plugin not initialized".into()))?;
        let callbacks = self
            .callbacks
            .clone()
            .ok_or_else(|| ChannelError::PlatformApi("Slack callbacks not initialized".into()))?;
        let bot_user_id = self.bot_user_id.clone().unwrap_or_default();

        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        self.shutdown_tx = Some(shutdown_tx);

        self.ws_handle = Some(tokio::spawn(socket_mode_loop(
            api,
            callbacks.message_tx,
            callbacks.confirm_tx,
            self.status.clone(),
            shutdown_rx,
            bot_user_id,
        )));

        self.status.set(PluginStatus::Running);
        info!("Slack plugin started");
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
        info!("Slack plugin stopped");
        Ok(())
    }

    async fn send_message(&self, chat_id: &str, message: UnifiedOutgoingMessage) -> Result<String, ChannelError> {
        let api = self
            .api
            .as_ref()
            .ok_or_else(|| ChannelError::PlatformApi("Plugin not initialized".into()))?;

        let text = truncate_message(message.text.as_deref().unwrap_or(""), SLACK_MESSAGE_LIMIT);
        let blocks = build_blocks(&message);

        let req = PostMessageRequest {
            channel: chat_id.to_string(),
            text,
            thread_ts: message.reply_to_message_id.clone(),
            blocks,
        };

        api.post_message(&req).await
    }

    async fn send_media(
        &self,
        chat_id: &str,
        media: crate::types::OutgoingMedia,
        caption: Option<&str>,
    ) -> Result<String, ChannelError> {
        // Slack has no separate photo endpoint — images and documents alike go
        // through the 3-step external file upload flow.
        let api = self
            .api
            .as_ref()
            .ok_or_else(|| ChannelError::PlatformApi("Plugin not initialized".into()))?;
        api.upload_file(chat_id, media.bytes, &media.filename, &media.mime, caption)
            .await
    }

    async fn edit_message(
        &self,
        chat_id: &str,
        message_id: &str,
        message: UnifiedOutgoingMessage,
    ) -> Result<(), ChannelError> {
        let api = self
            .api
            .as_ref()
            .ok_or_else(|| ChannelError::PlatformApi("Plugin not initialized".into()))?;

        let text = truncate_message(message.text.as_deref().unwrap_or(""), SLACK_MESSAGE_LIMIT);
        let blocks = build_blocks(&message);

        let req = UpdateMessageRequest {
            channel: chat_id.to_string(),
            ts: message_id.to_string(),
            text,
            blocks,
        };

        api.update_message(&req).await
    }

    fn active_user_count(&self) -> usize {
        0
    }

    fn bot_info(&self) -> Option<&BotInfo> {
        self.bot_info.as_ref()
    }

    fn plugin_type(&self) -> PluginType {
        PluginType::Slack
    }

    fn status(&self) -> PluginStatus {
        self.status.get()
    }

    fn last_error(&self) -> Option<&str> {
        self.last_error.as_deref()
    }
}

// ---------------------------------------------------------------------------
// Socket Mode WebSocket loop
// ---------------------------------------------------------------------------

/// Background task that maintains a Socket Mode WebSocket connection.
///
/// On disconnect or error, reconnects via `apps.connections.open` with
/// exponential backoff (cap ~30s, max ~10 attempts).
async fn socket_mode_loop(
    api: Arc<SlackApi>,
    message_tx: mpsc::Sender<UnifiedIncomingMessage>,
    confirm_tx: mpsc::Sender<(String, String)>,
    status: SharedPluginStatus,
    mut shutdown_rx: watch::Receiver<bool>,
    bot_user_id: String,
) {
    let mut consecutive_errors: u32 = 0;

    loop {
        if *shutdown_rx.borrow() {
            debug!("Slack socket mode loop received shutdown signal");
            break;
        }

        // Obtain a fresh WS URL
        let ws_url = match api.open_connection().await {
            Ok(url) => {
                consecutive_errors = 0;
                url
            }
            Err(e) => {
                consecutive_errors += 1;
                warn!(
                    error = %e,
                    consecutive_errors,
                    "Slack apps.connections.open failed"
                );
                if consecutive_errors >= SLACK_MAX_RECONNECT_ATTEMPTS {
                    error!("Slack max reconnect attempts reached, stopping socket mode loop");
                    break;
                }
                let backoff = backoff_delay(consecutive_errors);
                tokio::select! {
                    _ = tokio::time::sleep(backoff) => {}
                    _ = shutdown_rx.changed() => {
                        debug!("Slack socket mode loop shutdown during backoff");
                        break;
                    }
                }
                continue;
            }
        };

        // Connect and listen
        match connect_and_listen(
            &ws_url,
            &message_tx,
            &confirm_tx,
            &mut shutdown_rx,
            &bot_user_id,
        )
        .await
        {
            Ok(()) => {
                // Clean close (disconnect frame or shutdown) — reconnect unless shutting down
                if *shutdown_rx.borrow() {
                    break;
                }
                debug!("Slack WS closed, reconnecting");
                consecutive_errors = 0;
            }
            Err(e) => {
                consecutive_errors += 1;
                warn!(
                    error = %e,
                    consecutive_errors,
                    "Slack WS error"
                );
                if consecutive_errors >= SLACK_MAX_RECONNECT_ATTEMPTS {
                    error!("Slack max reconnect attempts reached, stopping socket mode loop");
                    break;
                }
                let backoff = backoff_delay(consecutive_errors);
                tokio::select! {
                    _ = tokio::time::sleep(backoff) => {}
                    _ = shutdown_rx.changed() => {
                        debug!("Slack socket mode loop shutdown during backoff");
                        break;
                    }
                }
            }
        }
    }

    mark_error_on_unexpected_exit(&status, &shutdown_rx, "slack");
    debug!("Slack socket mode loop exited");
}

/// Connect to a Slack Socket Mode WebSocket and process frames.
async fn connect_and_listen(
    ws_url: &str,
    message_tx: &mpsc::Sender<UnifiedIncomingMessage>,
    confirm_tx: &mpsc::Sender<(String, String)>,
    shutdown_rx: &mut watch::Receiver<bool>,
    bot_user_id: &str,
) -> Result<(), ChannelError> {
    use futures_util::{SinkExt, StreamExt};
    use tokio_tungstenite::connect_async_tls_with_config;
    use tokio_tungstenite::tungstenite::Message as WsMessage;

    let connector = build_ws_tls_connector()?;
    let (ws_stream, _) = connect_async_tls_with_config(ws_url, None, false, Some(connector))
        .await
        .map_err(|e| ChannelError::ConnectionFailed(format!("Slack WS connect failed: {e}")))?;

    info!("Slack Socket Mode WebSocket connected");

    let (mut write, mut read) = ws_stream.split();

    loop {
        tokio::select! {
            msg = read.next() => {
                match msg {
                    Some(Ok(WsMessage::Text(text))) => {
                        let envelope: SocketEnvelope = match serde_json::from_str(&text) {
                            Ok(e) => e,
                            Err(e) => {
                                warn!(error = %e, "Failed to parse Slack Socket Mode JSON");
                                continue;
                            }
                        };

                        // ACK immediately if envelope_id is present
                        if let Some(ref eid) = envelope.envelope_id {
                            let ack = SocketAck { envelope_id: eid.clone() };
                            if let Ok(ack_json) = serde_json::to_string(&ack) {
                                if let Err(e) = write.send(WsMessage::Text(ack_json.into())).await {
                                    warn!(error = %e, "Failed to send Slack ACK");
                                }
                            }
                        }

                        match envelope.envelope_type.as_str() {
                            "hello" => {
                                debug!("Slack Socket Mode hello received");
                            }
                            "disconnect" => {
                                debug!("Slack Socket Mode disconnect received, reconnecting");
                                return Ok(());
                            }
                            "events_api" => {
                                if let Some(payload_val) = envelope.payload {
                                    if let Ok(payload) = serde_json::from_value::<EventsApiPayload>(payload_val) {
                                        if let Some(event) = payload.event {
                                            handle_event(&event, message_tx, bot_user_id).await;
                                        }
                                    }
                                }
                            }
                            "interactive" => {
                                if let Some(payload_val) = envelope.payload {
                                    if let Ok(payload) = serde_json::from_value::<InteractivePayload>(payload_val) {
                                        handle_interactive(&payload, message_tx, confirm_tx).await;
                                    }
                                }
                            }
                            other => {
                                debug!(envelope_type = other, "Slack unknown envelope type");
                            }
                        }
                    }
                    Some(Ok(WsMessage::Close(_))) => {
                        debug!("Slack WS received close frame");
                        return Ok(());
                    }
                    Some(Err(e)) => {
                        return Err(ChannelError::ConnectionFailed(
                            format!("Slack WS read error: {e}")
                        ));
                    }
                    None => {
                        return Err(ChannelError::ConnectionFailed(
                            "Slack WS stream ended unexpectedly".into()
                        ));
                    }
                    _ => {}
                }
            }
            _ = shutdown_rx.changed() => {
                debug!("Slack WS shutdown during listen");
                return Ok(());
            }
        }
    }
}

/// Build a TLS connector for WebSocket connections.
///
/// Mirrors the Lark plugin's pattern: explicitly sets ALPN to `http/1.1`
/// to prevent h2 negotiation which breaks WebSocket upgrade.
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

// ---------------------------------------------------------------------------
// Event handlers
// ---------------------------------------------------------------------------

/// Handle a Slack `events_api` event (type=message).
async fn handle_event(
    event: &super::types::SlackEvent,
    message_tx: &mpsc::Sender<UnifiedIncomingMessage>,
    bot_user_id: &str,
) {
    let event_type = event.event_type.as_deref().unwrap_or("");
    if event_type != "message" {
        debug!(event_type, "Slack ignoring non-message event");
        return;
    }

    // Bot-loop guard: skip messages from bots or self
    if is_bot_message(event, bot_user_id) {
        return;
    }

    let channel = event.channel.as_deref().unwrap_or("");
    let user_id = event.user.as_deref().unwrap_or("");
    let text = event.text.as_deref().unwrap_or("");
    let ts = event.ts.as_deref().unwrap_or("");
    let channel_type = event.channel_type.as_deref().unwrap_or("");

    // Mention gating: in channels/groups, only process if bot is mentioned
    if !should_process_message(channel_type, text, bot_user_id) {
        debug!(
            channel,
            channel_type,
            "Slack message in channel/group without bot mention, skipping"
        );
        return;
    }

    let timestamp = parse_slack_ts(ts);

    let unified = UnifiedIncomingMessage {
        id: ts.to_string(),
        platform: PluginType::Slack,
        chat_id: channel.to_string(),
        user: UnifiedUser {
            id: user_id.to_string(),
            username: None,
            display_name: user_id.to_string(),
            avatar_url: None,
        },
        content: UnifiedMessageContent {
            content_type: MessageContentType::Text,
            text: text.to_string(),
            attachments: None,
        },
        timestamp,
        reply_to_message_id: event.thread_ts.clone(),
        action: None,
        raw: None,
    };

    let _ = message_tx.send(unified).await;
}

/// Handle a Slack `interactive` event (block_actions).
async fn handle_interactive(
    payload: &InteractivePayload,
    message_tx: &mpsc::Sender<UnifiedIncomingMessage>,
    confirm_tx: &mpsc::Sender<(String, String)>,
) {
    let interaction_type = payload.interaction_type.as_deref().unwrap_or("");
    if interaction_type != "block_actions" {
        debug!(interaction_type, "Slack ignoring non-block_actions interaction");
        return;
    }

    let actions = match &payload.actions {
        Some(a) if !a.is_empty() => a,
        _ => return,
    };

    let chat_id = payload
        .channel
        .as_ref()
        .and_then(|c| c.id.as_deref())
        .unwrap_or("");
    let user_id = payload
        .user
        .as_ref()
        .and_then(|u| u.id.as_deref())
        .unwrap_or("");
    let message_ts = payload
        .message
        .as_ref()
        .and_then(|m| m.ts.as_deref());

    for action in actions {
        // Use action_id as the primary callback data source, fall back to value
        let callback_data = action
            .action_id
            .as_deref()
            .or(action.value.as_deref())
            .unwrap_or("");

        let parsed = parse_callback_data(callback_data);

        // Check for tool confirmation callback
        if let Some(ref parsed_action) = parsed {
            if parsed_action.action == "system.confirm" {
                if let Some(ref params) = parsed_action.params {
                    let call_id = params.get("callId").cloned().unwrap_or_default();
                    let value = params.get("value").cloned().unwrap_or_default();
                    if !call_id.is_empty() {
                        let _ = confirm_tx.send((call_id, value)).await;
                    }
                }
            }
        }

        let unified_action = parsed.map(|a| UnifiedAction {
            action: a.action,
            category: a.category,
            params: a.params,
            context: ActionContext {
                platform: PluginType::Slack,
                user_id: user_id.to_string(),
                chat_id: chat_id.to_string(),
                message_id: message_ts.map(|s| s.to_string()),
                session_id: None,
            },
        });

        let msg = UnifiedIncomingMessage {
            id: format!(
                "{}:{}",
                message_ts.unwrap_or("0"),
                callback_data
            ),
            platform: PluginType::Slack,
            chat_id: chat_id.to_string(),
            user: UnifiedUser {
                id: user_id.to_string(),
                username: None,
                display_name: user_id.to_string(),
                avatar_url: None,
            },
            content: UnifiedMessageContent {
                content_type: MessageContentType::Action,
                text: callback_data.to_string(),
                attachments: None,
            },
            timestamp: chrono_now(),
            reply_to_message_id: None,
            action: unified_action,
            raw: None,
        };

        let _ = message_tx.send(msg).await;
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Bot-loop guard: returns `true` if the message should be skipped
/// (sent by a bot or by ourselves).
fn is_bot_message(event: &super::types::SlackEvent, bot_user_id: &str) -> bool {
    // Has a bot_id field
    if event.bot_id.is_some() {
        return true;
    }
    // subtype is bot_message
    if event.subtype.as_deref() == Some("bot_message") {
        return true;
    }
    // Sent by our own user
    if let Some(user) = event.user.as_deref() {
        if user == bot_user_id && !bot_user_id.is_empty() {
            return true;
        }
    }
    false
}

/// Mention gating: for channel_type != "im", only process if text
/// contains `<@BOT_USER_ID>`. DMs always process.
fn should_process_message(channel_type: &str, text: &str, bot_user_id: &str) -> bool {
    if channel_type == "im" {
        return true;
    }
    // In channels/groups, require @mention
    if bot_user_id.is_empty() {
        return false;
    }
    let mention_pattern = format!("<@{bot_user_id}>");
    text.contains(&mention_pattern)
}

/// Parse Slack timestamp (e.g., "1700000000.000100") to seconds.
fn parse_slack_ts(ts: &str) -> i64 {
    ts.split('.')
        .next()
        .and_then(|s| s.parse::<i64>().ok())
        .unwrap_or(0)
}

/// Truncate a message to the platform limit, appending "..." if truncated.
fn truncate_message(text: &str, limit: usize) -> String {
    if text.len() <= limit {
        return text.to_string();
    }
    let truncated: String = text.chars().take(limit - 3).collect();
    format!("{truncated}...")
}

/// Build Block Kit blocks for buttons, if present.
fn build_blocks(msg: &UnifiedOutgoingMessage) -> Option<serde_json::Value> {
    let buttons = msg.buttons.as_ref()?;
    let elements: Vec<ButtonElement> = buttons
        .iter()
        .flatten()
        .map(|btn| {
            let callback_data = format_callback_data(btn);
            ButtonElement {
                element_type: "button".into(),
                text: PlainTextObject {
                    text_type: "plain_text".into(),
                    text: btn.label.clone(),
                },
                action_id: callback_data.clone(),
                value: callback_data,
            }
        })
        .collect();

    if elements.is_empty() {
        return None;
    }

    let block = ActionsBlock {
        block_type: "actions".into(),
        elements,
    };

    serde_json::to_value(vec![block]).ok()
}

/// Calculate exponential backoff delay, capped at the configured maximum.
fn backoff_delay(attempt: u32) -> Duration {
    let delay_secs = 2u64.saturating_pow(attempt).min(SLACK_MAX_RECONNECT_DELAY.as_secs());
    Duration::from_secs(delay_secs)
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
    use crate::types::{ActionButton, OutgoingMessageType};
    use std::collections::HashMap;

    // -- SlackPlugin constructor -----------------------------------------------

    #[test]
    fn new_plugin_initial_state() {
        let plugin = SlackPlugin::new();
        assert_eq!(plugin.status(), PluginStatus::Created);
        assert!(plugin.bot_info().is_none());
        assert!(plugin.last_error().is_none());
        assert_eq!(plugin.plugin_type(), PluginType::Slack);
        assert_eq!(plugin.active_user_count(), 0);
    }

    // -- is_bot_message --------------------------------------------------------

    #[test]
    fn bot_message_with_bot_id() {
        let event = make_event(Some("U999"), None, Some("B123"), None);
        assert!(is_bot_message(&event, "U000"));
    }

    #[test]
    fn bot_message_with_subtype() {
        let event = make_event(Some("U999"), None, None, Some("bot_message"));
        assert!(is_bot_message(&event, "U000"));
    }

    #[test]
    fn bot_message_from_self() {
        let event = make_event(Some("U000"), None, None, None);
        assert!(is_bot_message(&event, "U000"));
    }

    #[test]
    fn not_bot_message_from_human() {
        let event = make_event(Some("U999"), None, None, None);
        assert!(!is_bot_message(&event, "U000"));
    }

    #[test]
    fn not_bot_message_empty_bot_user_id() {
        let event = make_event(Some("U999"), None, None, None);
        assert!(!is_bot_message(&event, ""));
    }

    // -- should_process_message -----------------------------------------------

    #[test]
    fn dm_always_processed() {
        assert!(should_process_message("im", "hello", "U000"));
    }

    #[test]
    fn channel_with_mention() {
        assert!(should_process_message("channel", "hey <@U000> help", "U000"));
    }

    #[test]
    fn channel_without_mention() {
        assert!(!should_process_message("channel", "hey there", "U000"));
    }

    #[test]
    fn group_with_mention() {
        assert!(should_process_message("group", "<@U000>", "U000"));
    }

    #[test]
    fn group_without_mention() {
        assert!(!should_process_message("group", "nothing here", "U000"));
    }

    #[test]
    fn channel_empty_bot_user_id() {
        assert!(!should_process_message("channel", "<@U000>", ""));
    }

    // -- parse_slack_ts -------------------------------------------------------

    #[test]
    fn parse_ts_normal() {
        assert_eq!(parse_slack_ts("1700000000.000100"), 1700000000);
    }

    #[test]
    fn parse_ts_no_dot() {
        assert_eq!(parse_slack_ts("1700000000"), 1700000000);
    }

    #[test]
    fn parse_ts_empty() {
        assert_eq!(parse_slack_ts(""), 0);
    }

    #[test]
    fn parse_ts_invalid() {
        assert_eq!(parse_slack_ts("abc.def"), 0);
    }

    // -- truncate_message -----------------------------------------------------

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
        let text = "你好世界测试文本";
        let result = truncate_message(text, 5);
        assert_eq!(result, "你好...");
    }

    // -- build_blocks ---------------------------------------------------------

    #[test]
    fn build_blocks_no_buttons() {
        let msg = make_outgoing(None, None);
        assert!(build_blocks(&msg).is_none());
    }

    #[test]
    fn build_blocks_empty_buttons() {
        let msg = make_outgoing(None, Some(vec![]));
        assert!(build_blocks(&msg).is_none());
    }

    #[test]
    fn build_blocks_with_buttons() {
        let buttons = vec![vec![
            ActionButton {
                label: "Yes".into(),
                action: "system.confirm".into(),
                params: Some(HashMap::from([
                    ("callId".into(), "abc".into()),
                    ("value".into(), "yes".into()),
                ])),
            },
            ActionButton {
                label: "No".into(),
                action: "system.confirm".into(),
                params: Some(HashMap::from([
                    ("callId".into(), "abc".into()),
                    ("value".into(), "no".into()),
                ])),
            },
        ]];
        let msg = make_outgoing(None, Some(buttons));
        let blocks = build_blocks(&msg).unwrap();
        let blocks_arr = blocks.as_array().unwrap();
        assert_eq!(blocks_arr.len(), 1);
        assert_eq!(blocks_arr[0]["type"], "actions");
        let elements = blocks_arr[0]["elements"].as_array().unwrap();
        assert_eq!(elements.len(), 2);
        assert_eq!(elements[0]["type"], "button");
        assert_eq!(elements[0]["text"]["type"], "plain_text");
        assert_eq!(elements[0]["text"]["text"], "Yes");
        // action_id should be encoded callback data
        let action_id = elements[0]["action_id"].as_str().unwrap();
        assert!(action_id.starts_with("chat:system.confirm:"));
    }

    #[test]
    fn build_blocks_action_id_roundtrip() {
        let buttons = vec![vec![ActionButton {
            label: "Help".into(),
            action: "help.show".into(),
            params: None,
        }]];
        let msg = make_outgoing(None, Some(buttons));
        let blocks = build_blocks(&msg).unwrap();
        let action_id = blocks[0]["elements"][0]["action_id"].as_str().unwrap();
        let parsed = parse_callback_data(action_id).unwrap();
        assert_eq!(parsed.action, "help.show");
    }

    // -- backoff_delay --------------------------------------------------------

    #[test]
    fn backoff_exponential() {
        assert_eq!(backoff_delay(1), Duration::from_secs(2));
        assert_eq!(backoff_delay(2), Duration::from_secs(4));
        assert_eq!(backoff_delay(3), Duration::from_secs(8));
        assert_eq!(backoff_delay(4), Duration::from_secs(16));
    }

    #[test]
    fn backoff_capped() {
        assert_eq!(backoff_delay(5), Duration::from_secs(30));
        assert_eq!(backoff_delay(10), Duration::from_secs(30));
    }

    // -- handle_event normalization -------------------------------------------

    #[tokio::test]
    async fn handle_event_dm_normalizes() {
        let (tx, mut rx) = mpsc::channel(16);
        let event = super::super::types::SlackEvent {
            event_type: Some("message".into()),
            channel: Some("D12345".into()),
            channel_type: Some("im".into()),
            user: Some("U99999".into()),
            text: Some("hi there".into()),
            ts: Some("1700000000.000100".into()),
            thread_ts: None,
            subtype: None,
            bot_id: None,
        };
        handle_event(&event, &tx, "U00000").await;
        let msg = rx.try_recv().unwrap();
        assert_eq!(msg.platform, PluginType::Slack);
        assert_eq!(msg.chat_id, "D12345");
        assert_eq!(msg.user.id, "U99999");
        assert_eq!(msg.content.text, "hi there");
        assert_eq!(msg.content.content_type, MessageContentType::Text);
        assert_eq!(msg.timestamp, 1700000000);
        assert_eq!(msg.id, "1700000000.000100");
    }

    #[tokio::test]
    async fn handle_event_channel_with_mention() {
        let (tx, mut rx) = mpsc::channel(16);
        let event = super::super::types::SlackEvent {
            event_type: Some("message".into()),
            channel: Some("C12345".into()),
            channel_type: Some("channel".into()),
            user: Some("U99999".into()),
            text: Some("hey <@U00000> help me".into()),
            ts: Some("1700000001.000200".into()),
            thread_ts: Some("1700000000.000100".into()),
            subtype: None,
            bot_id: None,
        };
        handle_event(&event, &tx, "U00000").await;
        let msg = rx.try_recv().unwrap();
        assert_eq!(msg.chat_id, "C12345");
        assert_eq!(msg.content.text, "hey <@U00000> help me");
        assert_eq!(
            msg.reply_to_message_id.as_deref(),
            Some("1700000000.000100")
        );
    }

    #[tokio::test]
    async fn handle_event_channel_without_mention_skipped() {
        let (tx, mut rx) = mpsc::channel(16);
        let event = super::super::types::SlackEvent {
            event_type: Some("message".into()),
            channel: Some("C12345".into()),
            channel_type: Some("channel".into()),
            user: Some("U99999".into()),
            text: Some("just chatting".into()),
            ts: Some("1700000001.000200".into()),
            thread_ts: None,
            subtype: None,
            bot_id: None,
        };
        handle_event(&event, &tx, "U00000").await;
        assert!(rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn handle_event_bot_message_skipped() {
        let (tx, mut rx) = mpsc::channel(16);
        let event = super::super::types::SlackEvent {
            event_type: Some("message".into()),
            channel: Some("D12345".into()),
            channel_type: Some("im".into()),
            user: Some("U99999".into()),
            text: Some("I am a bot".into()),
            ts: Some("1700000001.000200".into()),
            thread_ts: None,
            subtype: None,
            bot_id: Some("B12345".into()),
        };
        handle_event(&event, &tx, "U00000").await;
        assert!(rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn handle_event_self_message_skipped() {
        let (tx, mut rx) = mpsc::channel(16);
        let event = super::super::types::SlackEvent {
            event_type: Some("message".into()),
            channel: Some("D12345".into()),
            channel_type: Some("im".into()),
            user: Some("U00000".into()),
            text: Some("echo".into()),
            ts: Some("1700000001.000200".into()),
            thread_ts: None,
            subtype: None,
            bot_id: None,
        };
        handle_event(&event, &tx, "U00000").await;
        assert!(rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn handle_event_non_message_skipped() {
        let (tx, mut rx) = mpsc::channel(16);
        let event = super::super::types::SlackEvent {
            event_type: Some("reaction_added".into()),
            channel: Some("D12345".into()),
            channel_type: Some("im".into()),
            user: Some("U99999".into()),
            text: None,
            ts: Some("1700000001.000200".into()),
            thread_ts: None,
            subtype: None,
            bot_id: None,
        };
        handle_event(&event, &tx, "U00000").await;
        assert!(rx.try_recv().is_err());
    }

    // -- handle_interactive normalization -------------------------------------------

    #[tokio::test]
    async fn handle_interactive_block_actions() {
        let (msg_tx, mut msg_rx) = mpsc::channel(16);
        let (confirm_tx, _confirm_rx) = mpsc::channel(16);

        let payload = InteractivePayload {
            interaction_type: Some("block_actions".into()),
            actions: Some(vec![super::super::types::BlockAction {
                action_id: Some("system:session.new".into()),
                value: Some("system:session.new".into()),
            }]),
            channel: Some(super::super::types::InteractiveChannel {
                id: Some("C12345".into()),
            }),
            user: Some(super::super::types::InteractiveUser {
                id: Some("U99999".into()),
            }),
            message: Some(super::super::types::InteractiveMessage {
                ts: Some("1700000000.000100".into()),
            }),
        };

        handle_interactive(&payload, &msg_tx, &confirm_tx).await;
        let msg = msg_rx.try_recv().unwrap();
        assert_eq!(msg.platform, PluginType::Slack);
        assert_eq!(msg.chat_id, "C12345");
        assert_eq!(msg.user.id, "U99999");
        assert_eq!(msg.content.content_type, MessageContentType::Action);
        assert_eq!(msg.content.text, "system:session.new");
        let action = msg.action.unwrap();
        assert_eq!(action.action, "session.new");
    }

    #[tokio::test]
    async fn handle_interactive_confirm_sends_to_confirm_tx() {
        let (msg_tx, _msg_rx) = mpsc::channel(16);
        let (confirm_tx, mut confirm_rx) = mpsc::channel(16);

        let payload = InteractivePayload {
            interaction_type: Some("block_actions".into()),
            actions: Some(vec![super::super::types::BlockAction {
                action_id: Some("chat:system.confirm:callId=abc,value=yes".into()),
                value: Some("chat:system.confirm:callId=abc,value=yes".into()),
            }]),
            channel: Some(super::super::types::InteractiveChannel {
                id: Some("C12345".into()),
            }),
            user: Some(super::super::types::InteractiveUser {
                id: Some("U99999".into()),
            }),
            message: Some(super::super::types::InteractiveMessage {
                ts: Some("1700000000.000100".into()),
            }),
        };

        handle_interactive(&payload, &msg_tx, &confirm_tx).await;
        let (call_id, value) = confirm_rx.try_recv().unwrap();
        assert_eq!(call_id, "abc");
        assert_eq!(value, "yes");
    }

    #[tokio::test]
    async fn handle_interactive_non_block_actions_skipped() {
        let (msg_tx, mut msg_rx) = mpsc::channel(16);
        let (confirm_tx, _confirm_rx) = mpsc::channel(16);

        let payload = InteractivePayload {
            interaction_type: Some("view_submission".into()),
            actions: None,
            channel: None,
            user: None,
            message: None,
        };

        handle_interactive(&payload, &msg_tx, &confirm_tx).await;
        assert!(msg_rx.try_recv().is_err());
    }

    // -- test helpers ----------------------------------------------------------

    fn make_event(
        user: Option<&str>,
        text: Option<&str>,
        bot_id: Option<&str>,
        subtype: Option<&str>,
    ) -> super::super::types::SlackEvent {
        super::super::types::SlackEvent {
            event_type: Some("message".into()),
            channel: Some("C12345".into()),
            channel_type: Some("im".into()),
            user: user.map(String::from),
            text: text.map(String::from),
            ts: Some("1700000000.000100".into()),
            thread_ts: None,
            subtype: subtype.map(String::from),
            bot_id: bot_id.map(String::from),
        }
    }

    fn make_outgoing(
        text: Option<&str>,
        buttons: Option<Vec<Vec<ActionButton>>>,
    ) -> UnifiedOutgoingMessage {
        UnifiedOutgoingMessage {
            message_type: OutgoingMessageType::Text,
            text: text.map(String::from),
            parse_mode: None,
            buttons,
            keyboard: None,
            image_url: None,
            file_url: None,
            file_name: None,
            media_actions: None,
            reply_to_message_id: None,
            silent: None,
        }
    }
}

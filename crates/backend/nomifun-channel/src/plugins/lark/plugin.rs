use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use reqwest::Client;
use tokio::sync::{Mutex, mpsc, watch};
use tokio::task::JoinHandle;
use tracing::{debug, error, info, warn};

use crate::constants::{LARK_EVENT_DEDUP_TTL, LARK_MESSAGE_LIMIT};
use crate::error::ChannelError;
use crate::plugin::{ChannelPlugin, PluginCallbacks, SharedPluginStatus, mark_error_on_unexpected_exit};
use crate::types::{
    ActionCategory, ActionContext, BotInfo, MessageContentType, PluginConfig, PluginStatus, PluginType, UnifiedAction,
    UnifiedAttachment, UnifiedIncomingMessage, UnifiedMessageContent, UnifiedOutgoingMessage, UnifiedUser,
};

use super::api::LarkApi;
use super::frame::{
    METHOD_CONTROL, METHOD_DATA, build_ack_frame, build_ping_frame, decode_frame, encode_frame, get_header,
};
use super::types::{BotMenuEvent, CardActionEvent, MessageEvent, build_interactive_card, parse_lark_callback};
use super::ws_session::{FragmentCache, parse_pong_config};

/// Maximum reconnect attempts before giving up.
const MAX_RECONNECT_ATTEMPTS: u32 = 10;

/// Maximum backoff delay between reconnection attempts.
const MAX_RECONNECT_DELAY: Duration = Duration::from_secs(30);

/// Interval between event dedup cache cleanup sweeps.
const DEDUP_CLEANUP_INTERVAL: Duration = Duration::from_secs(60);

/// Lark (Feishu) platform plugin.
///
/// Connects via WebSocket long connection, handles message events,
/// card action triggers, and bot menu events. All responses use
/// interactive cards (Lark only supports editing card messages).
pub struct LarkPlugin {
    /// Shared with the WebSocket loop so a dead loop can flip it to `Error`.
    status: SharedPluginStatus,
    bot_info: Option<BotInfo>,
    last_error: Option<String>,
    api: Option<Arc<LarkApi>>,
    callbacks: Option<PluginCallbacks>,
    ws_handle: Option<JoinHandle<()>>,
    cleanup_handle: Option<JoinHandle<()>>,
    shutdown_tx: Option<watch::Sender<bool>>,
    /// Shared event deduplication cache: event_id → received_at.
    dedup_cache: Arc<Mutex<HashMap<String, Instant>>>,
}

impl Default for LarkPlugin {
    fn default() -> Self {
        Self {
            status: SharedPluginStatus::default(),
            bot_info: None,
            last_error: None,
            api: None,
            callbacks: None,
            ws_handle: None,
            cleanup_handle: None,
            shutdown_tx: None,
            dedup_cache: Arc::new(Mutex::new(HashMap::new())),
        }
    }
}

impl LarkPlugin {
    pub fn new() -> Self {
        Self::default()
    }
}

#[async_trait::async_trait]
impl ChannelPlugin for LarkPlugin {
    async fn initialize(&mut self, config: PluginConfig, callbacks: PluginCallbacks) -> Result<(), ChannelError> {
        self.status.set(PluginStatus::Initializing);

        let app_id = config
            .credentials
            .app_id
            .as_deref()
            .filter(|s| !s.is_empty())
            .ok_or_else(|| {
                self.status.set(PluginStatus::Error);
                self.last_error = Some("Missing Lark app_id".into());
                ChannelError::InvalidConfig("Missing Lark app_id".into())
            })?;

        let app_secret = config
            .credentials
            .app_secret
            .as_deref()
            .filter(|s| !s.is_empty())
            .ok_or_else(|| {
                self.status.set(PluginStatus::Error);
                self.last_error = Some("Missing Lark app_secret".into());
                ChannelError::InvalidConfig("Missing Lark app_secret".into())
            })?;

        let http_client = Client::builder()
            .timeout(Duration::from_secs(30))
            .build()
            .map_err(|e| {
                self.status.set(PluginStatus::Error);
                self.last_error = Some(format!("HTTP client init failed: {e}"));
                ChannelError::ConnectionFailed(format!("HTTP client init failed: {e}"))
            })?;

        let api = Arc::new(LarkApi::new(http_client, app_id, app_secret));

        // Validate credentials by getting bot info
        let bot_data = api.get_bot_info().await.map_err(|e| {
            self.status.set(PluginStatus::Error);
            self.last_error = Some(format!("Credential validation failed: {e}"));
            e
        })?;

        self.bot_info = Some(BotInfo {
            id: bot_data.open_id.clone(),
            username: None,
            display_name: bot_data.app_name.clone(),
        });

        info!(
            bot_name = bot_data.app_name,
            bot_id = bot_data.open_id,
            "Lark bot initialized"
        );

        self.api = Some(api);
        self.callbacks = Some(callbacks);
        self.status.set(PluginStatus::Ready);
        Ok(())
    }

    async fn start(&mut self) -> Result<(), ChannelError> {
        self.status.set(PluginStatus::Starting);

        let callbacks = self.callbacks.take().ok_or_else(|| {
            self.status.set(PluginStatus::Error);
            ChannelError::ConnectionFailed("Plugin not initialized".into())
        })?;

        // Set up shutdown channel
        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        self.shutdown_tx = Some(shutdown_tx);

        // Spawn the WebSocket connection loop
        let api_clone = Arc::clone(self.api.as_ref().expect("api set in initialize"));
        let dedup_cache = Arc::clone(&self.dedup_cache);
        self.ws_handle = Some(tokio::spawn(ws_loop(
            api_clone,
            callbacks.message_tx,
            callbacks.confirm_tx,
            dedup_cache.clone(),
            self.status.clone(),
            shutdown_rx,
        )));

        // Spawn the dedup cache cleanup task
        let dedup_for_cleanup = dedup_cache;
        let mut cleanup_shutdown = self.shutdown_tx.as_ref().expect("shutdown_tx just set").subscribe();
        self.cleanup_handle = Some(tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = tokio::time::sleep(DEDUP_CLEANUP_INTERVAL) => {
                        cleanup_expired_events(&dedup_for_cleanup).await;
                    }
                    _ = cleanup_shutdown.changed() => {
                        break;
                    }
                }
            }
        }));

        self.status.set(PluginStatus::Running);
        info!("Lark plugin started");
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

        if let Some(handle) = self.cleanup_handle.take() {
            let _ = tokio::time::timeout(Duration::from_secs(2), handle).await;
        }

        self.api = None;
        self.status.set(PluginStatus::Stopped);
        info!("Lark plugin stopped");
        Ok(())
    }

    async fn send_message(&self, chat_id: &str, message: UnifiedOutgoingMessage) -> Result<String, ChannelError> {
        let api = self
            .api
            .as_ref()
            .ok_or_else(|| ChannelError::PlatformApi("Plugin not initialized".into()))?;

        let text = truncate_message(message.text.as_deref().unwrap_or(""), LARK_MESSAGE_LIMIT);

        let card_content = build_interactive_card(&text, message.buttons.as_deref());
        let data = api.send_card(chat_id, &card_content).await?;
        Ok(data.message_id)
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

        let text = truncate_message(message.text.as_deref().unwrap_or(""), LARK_MESSAGE_LIMIT);

        let card_content = build_interactive_card(&text, message.buttons.as_deref());
        api.update_card(message_id, &card_content).await
    }

    async fn send_media(
        &self,
        chat_id: &str,
        media: crate::types::OutgoingMedia,
        _caption: Option<&str>,
    ) -> Result<String, ChannelError> {
        use crate::types::MediaKind;
        let api = self
            .api
            .as_ref()
            .ok_or_else(|| ChannelError::PlatformApi("Plugin not initialized".into()))?;
        // Lark image/file messages carry no caption field; the relay delivers
        // the assistant's text separately, so `caption` is intentionally unused.
        let data = match media.kind {
            MediaKind::Image => api.send_image(chat_id, media.bytes, &media.filename, &media.mime).await?,
            MediaKind::File => api.send_file(chat_id, media.bytes, &media.filename, &media.mime).await?,
        };
        Ok(data.message_id)
    }

    fn active_user_count(&self) -> usize {
        0
    }

    fn bot_info(&self) -> Option<&BotInfo> {
        self.bot_info.as_ref()
    }

    fn plugin_type(&self) -> PluginType {
        PluginType::Lark
    }

    fn status(&self) -> PluginStatus {
        self.status.get()
    }

    fn last_error(&self) -> Option<&str> {
        self.last_error.as_deref()
    }
}

// ---------------------------------------------------------------------------
// WebSocket connection loop
// ---------------------------------------------------------------------------

/// Background task that maintains a WebSocket connection to Lark.
///
/// On disconnect, implements exponential backoff reconnection up to
/// `MAX_RECONNECT_ATTEMPTS`.
async fn ws_loop(
    api: Arc<LarkApi>,
    message_tx: mpsc::Sender<UnifiedIncomingMessage>,
    confirm_tx: mpsc::Sender<(String, String)>,
    dedup_cache: Arc<Mutex<HashMap<String, Instant>>>,
    status: SharedPluginStatus,
    mut shutdown_rx: watch::Receiver<bool>,
) {
    let mut consecutive_errors: u32 = 0;

    loop {
        if *shutdown_rx.borrow() {
            debug!("Lark WS loop received shutdown signal");
            break;
        }

        let endpoint_data = match api.get_ws_endpoint().await {
            Ok(data) => data,
            Err(e) => {
                consecutive_errors += 1;
                warn!(error = %e, consecutive_errors, "Lark WS endpoint fetch failed");
                if consecutive_errors >= MAX_RECONNECT_ATTEMPTS {
                    error!("Lark max reconnect attempts reached");
                    break;
                }
                let delay = backoff_delay(consecutive_errors);
                tokio::select! {
                    _ = tokio::time::sleep(delay) => continue,
                    _ = shutdown_rx.changed() => break,
                }
            }
        };

        let ws_url = &endpoint_data.url;
        let initial_ping_secs = endpoint_data
            .client_config
            .as_ref()
            .and_then(|c| c.ping_interval)
            .unwrap_or(120);

        let service_id = extract_service_id(ws_url);
        debug!(url = %ws_url, service_id, initial_ping_secs, "Connecting to Lark WebSocket");

        match connect_and_listen(
            ws_url,
            service_id,
            initial_ping_secs,
            &message_tx,
            &confirm_tx,
            &dedup_cache,
            &mut shutdown_rx,
        )
        .await
        {
            Ok(()) => {
                debug!("Lark WS connection closed cleanly");
                break;
            }
            Err(e) => {
                consecutive_errors += 1;
                warn!(error = %e, consecutive_errors, "Lark WS connection error");
                if consecutive_errors >= MAX_RECONNECT_ATTEMPTS {
                    error!("Lark max reconnect attempts reached");
                    break;
                }
                let delay = backoff_delay(consecutive_errors);
                tokio::select! {
                    _ = tokio::time::sleep(delay) => {}
                    _ = shutdown_rx.changed() => break,
                }
            }
        }
    }

    // The loop exits on shutdown, reconnect exhaustion, or a server-side
    // clean close (which the loop does not currently re-dial). For any
    // non-shutdown exit the connection is gone while the facade still says
    // Running — flip the shared status to Error so the manager watchdog can
    // persist/broadcast the real state and attempt a restart.
    mark_error_on_unexpected_exit(&status, &shutdown_rx, "lark");

    debug!("Lark WS loop exited");
}

/// Connect to the WebSocket and listen for binary protobuf frames.
async fn connect_and_listen(
    ws_url: &str,
    service_id: i32,
    initial_ping_secs: u64,
    message_tx: &mpsc::Sender<UnifiedIncomingMessage>,
    confirm_tx: &mpsc::Sender<(String, String)>,
    dedup_cache: &Arc<Mutex<HashMap<String, Instant>>>,
    shutdown_rx: &mut watch::Receiver<bool>,
) -> Result<(), ChannelError> {
    use futures_util::{SinkExt, StreamExt};
    use tokio_tungstenite::connect_async_tls_with_config;
    use tokio_tungstenite::tungstenite::Message as WsMessage;

    let connector = build_ws_tls_connector()?;
    let (ws_stream, _) = connect_async_tls_with_config(ws_url, None, false, Some(connector))
        .await
        .map_err(|e| ChannelError::ConnectionFailed(format!("Lark WS connect failed: {e}")))?;

    info!("Lark WebSocket connected");

    let (mut write, mut read) = ws_stream.split();
    let mut fragment_cache = FragmentCache::new();
    let mut ping_duration = Duration::from_secs(initial_ping_secs);
    let mut ping_deadline = tokio::time::Instant::now() + ping_duration;

    loop {
        tokio::select! {
            msg = read.next() => {
                match msg {
                    Some(Ok(WsMessage::Binary(data))) => {
                        let frame = match decode_frame(&data) {
                            Ok(f) => f,
                            Err(e) => {
                                warn!(error = %e, "Failed to decode Lark protobuf frame");
                                continue;
                            }
                        };

                        match frame.method {
                            METHOD_CONTROL => {
                                if let Some(new_duration) = handle_control_frame(&frame) {
                                    ping_duration = new_duration;
                                    ping_deadline = tokio::time::Instant::now() + ping_duration;
                                }
                            }
                            METHOD_DATA => {
                                let ack = build_ack_frame(&frame);
                                let ack_bytes = encode_frame(&ack);
                                if let Err(e) = write.send(WsMessage::Binary(ack_bytes.into())).await {
                                    warn!(error = %e, "Failed to send Lark ack frame");
                                }

                                let message_id = get_header(&frame.headers, "message_id")
                                    .unwrap_or("")
                                    .to_owned();
                                let sum: usize = get_header(&frame.headers, "sum")
                                    .and_then(|s| s.parse().ok())
                                    .unwrap_or(1);
                                let seq: usize = get_header(&frame.headers, "seq")
                                    .and_then(|s| s.parse().ok())
                                    .unwrap_or(0);

                                if let Some(merged) = fragment_cache.push(&message_id, sum, seq, &frame.payload) {
                                    let msg_type = get_header(&frame.headers, "type").unwrap_or("");
                                    if msg_type == "event" || msg_type == "card" {
                                        if let Ok(text) = String::from_utf8(merged) {
                                            let dedup_id = if message_id.is_empty() {
                                                None
                                            } else {
                                                Some(message_id.as_str())
                                            };
                                            handle_ws_text(&text, msg_type, dedup_id, message_tx, confirm_tx, dedup_cache)
                                                .await;
                                        } else {
                                            warn!("Lark frame payload is not valid UTF-8");
                                        }
                                    }
                                }
                            }
                            _ => {
                                debug!(method = frame.method, "Lark unknown frame method");
                            }
                        }
                    }
                    Some(Ok(WsMessage::Close(_))) => {
                        debug!("Lark WS received close frame");
                        return Ok(());
                    }
                    Some(Err(e)) => {
                        return Err(ChannelError::ConnectionFailed(
                            format!("Lark WS read error: {e}")
                        ));
                    }
                    None => {
                        return Err(ChannelError::ConnectionFailed(
                            "Lark WS stream ended unexpectedly".into()
                        ));
                    }
                    _ => {}
                }
            }
            _ = tokio::time::sleep_until(ping_deadline) => {
                let ping = build_ping_frame(service_id);
                let ping_bytes = encode_frame(&ping);
                if let Err(e) = write.send(WsMessage::Binary(ping_bytes.into())).await {
                    warn!(error = %e, "Failed to send Lark ping frame");
                    return Err(ChannelError::ConnectionFailed("Lark ping send failed".into()));
                }
                debug!("Lark ping sent");
                ping_deadline = tokio::time::Instant::now() + ping_duration;
                fragment_cache.cleanup(Duration::from_secs(300));
            }
            _ = shutdown_rx.changed() => {
                debug!("Lark WS shutdown during listen");
                return Ok(());
            }
        }
    }
}

/// Handle a control frame (ping/pong). Returns updated ping duration if pong contains config.
fn handle_control_frame(frame: &super::frame::PbFrame) -> Option<Duration> {
    let frame_type = get_header(&frame.headers, "type").unwrap_or("");
    match frame_type {
        "pong" => {
            if !frame.payload.is_empty()
                && let Some(config) = parse_pong_config(&frame.payload)
            {
                debug!(
                    interval_secs = config.ping_interval_secs,
                    "Lark ping interval updated from pong"
                );
                return Some(Duration::from_secs(config.ping_interval_secs));
            }
            None
        }
        "ping" => None,
        _ => {
            debug!(frame_type, "Lark unknown control frame type");
            None
        }
    }
}

/// Handle a decoded and reassembled Lark event payload.
///
/// `dedup_id` is the transport frame's `message_id` header. The `event`
/// branch dedups on the payload's own `event_id`; the `card` branch has no
/// stable id in its payload (its `token`/`open_message_id` are not reliably
/// unique-per-click), so it dedups on this frame id instead — a WS
/// re-delivery of the same click carries the same frame `message_id`,
/// preventing a duplicate `chat.regenerate`/`chat.continue` from re-firing.
async fn handle_ws_text(
    text: &str,
    frame_type: &str,
    dedup_id: Option<&str>,
    message_tx: &mpsc::Sender<UnifiedIncomingMessage>,
    confirm_tx: &mpsc::Sender<(String, String)>,
    dedup_cache: &Arc<Mutex<HashMap<String, Instant>>>,
) {
    match frame_type {
        "event" => {
            let envelope: serde_json::Value = match serde_json::from_str(text) {
                Ok(v) => v,
                Err(e) => {
                    warn!(error = %e, "Failed to parse Lark event JSON");
                    return;
                }
            };

            let event_id = envelope
                .get("header")
                .and_then(|h| h.get("event_id"))
                .and_then(|v| v.as_str());
            if let Some(eid) = event_id
                && is_duplicate(dedup_cache, eid).await
            {
                debug!(event_id = eid, "Lark duplicate event, skipping");
                return;
            }

            let event_type = envelope
                .get("header")
                .and_then(|h| h.get("event_type"))
                .and_then(|v| v.as_str())
                .unwrap_or("");

            match event_type {
                "im.message.receive_v1" => {
                    if let Some(event_data) = envelope.get("event").cloned() {
                        handle_message_event(event_data, message_tx).await;
                    }
                }
                "application.bot.menu_v6" => {
                    if let Some(event_data) = envelope.get("event").cloned() {
                        handle_bot_menu_event(event_data, message_tx).await;
                    }
                }
                _ => {
                    debug!(event_type, "Lark unhandled event type");
                }
            }
        }
        "card" => {
            // Card frames have no payload-level dedup id, so guard on the
            // transport frame `message_id`. A WS re-delivery of the same
            // click would otherwise re-fire its action (e.g. chat.regenerate).
            if let Some(id) = dedup_id
                && is_duplicate(dedup_cache, id).await
            {
                debug!(message_id = id, "Lark duplicate card frame, skipping");
                return;
            }
            let data: serde_json::Value = match serde_json::from_str(text) {
                Ok(v) => v,
                Err(e) => {
                    warn!(error = %e, "Failed to parse Lark card action JSON");
                    return;
                }
            };
            handle_card_action(data, message_tx, confirm_tx).await;
        }
        _ => {
            debug!(frame_type, "Lark unhandled payload type");
        }
    }
}

// ---------------------------------------------------------------------------
// Event handlers
// ---------------------------------------------------------------------------

/// Handle an `im.message.receive_v1` event.
async fn handle_message_event(event_data: serde_json::Value, message_tx: &mpsc::Sender<UnifiedIncomingMessage>) {
    let evt: MessageEvent = match serde_json::from_value(event_data.clone()) {
        Ok(e) => e,
        Err(e) => {
            warn!(error = %e, raw = %event_data, "Failed to parse Lark message event");
            return;
        }
    };

    let user = UnifiedUser {
        id: evt.sender.sender_id.open_id.clone(),
        username: None,
        display_name: evt.sender.sender_id.open_id.clone(),
        avatar_url: None,
    };

    let (content_type, text, attachments) = extract_message_content(
        &evt.message.message_type,
        &evt.message.content,
        evt.message.mentions.as_deref(),
    );

    let timestamp = evt
        .message
        .create_time
        .as_deref()
        .and_then(|t| t.parse::<i64>().ok())
        .map(|ms| ms / 1000)
        .unwrap_or_else(chrono_now);

    let unified = UnifiedIncomingMessage {
        id: evt.message.message_id.clone(),
        platform: PluginType::Lark,
        chat_id: evt.message.chat_id.clone(),
        user,
        content: UnifiedMessageContent {
            content_type,
            text,
            attachments,
        },
        timestamp,
        reply_to_message_id: evt.message.parent_id.clone(),
        action: None,
        raw: None,
    };

    let _ = message_tx.send(unified).await;
}

/// Handle a `card.action.trigger` event.
async fn handle_card_action(
    data: serde_json::Value,
    message_tx: &mpsc::Sender<UnifiedIncomingMessage>,
    confirm_tx: &mpsc::Sender<(String, String)>,
) {
    let evt: CardActionEvent = match serde_json::from_value(data) {
        Ok(e) => e,
        Err(e) => {
            warn!(error = %e, "Failed to parse Lark card action");
            return;
        }
    };

    // Extract action string from the card button value
    let action_str = evt
        .action
        .value
        .as_ref()
        .and_then(|v| v.get("action"))
        .and_then(|a| a.as_str())
        .unwrap_or("");

    let parsed = parse_lark_callback(action_str);

    // Check if this is a tool confirmation
    if let Some((_, ref action, ref params)) = parsed
        && action == "system.confirm"
        && let Some(p) = params
    {
        let call_id = p.get("callId").cloned().unwrap_or_default();
        let value = p.get("value").cloned().unwrap_or_default();
        if !call_id.is_empty() {
            let _ = confirm_tx.send((call_id, value)).await;
        }
    }

    let chat_id = evt.open_chat_id.as_deref().unwrap_or("").to_string();

    let message_id = evt.open_message_id.clone();

    let user = UnifiedUser {
        id: evt.operator.open_id.clone(),
        username: None,
        display_name: evt.operator.open_id.clone(),
        avatar_url: None,
    };

    let unified_action = parsed.map(|(cat_str, action, params)| {
        let category = match cat_str.as_str() {
            "platform" => ActionCategory::Platform,
            "chat" => ActionCategory::Chat,
            _ => ActionCategory::System,
        };
        UnifiedAction {
            action,
            category,
            params,
            context: ActionContext {
                platform: PluginType::Lark,
                user_id: evt.operator.open_id.clone(),
                chat_id: chat_id.clone(),
                message_id: message_id.clone(),
                session_id: None,
            },
        }
    });

    let msg = UnifiedIncomingMessage {
        id: format!("card_{}", chrono_now()),
        platform: PluginType::Lark,
        chat_id,
        user,
        content: UnifiedMessageContent {
            content_type: MessageContentType::Action,
            text: action_str.to_string(),
            attachments: None,
        },
        timestamp: chrono_now(),
        reply_to_message_id: None,
        action: unified_action,
        raw: None,
    };

    let _ = message_tx.send(msg).await;
}

/// Handle an `application.bot.menu_v6` event.
async fn handle_bot_menu_event(event_data: serde_json::Value, message_tx: &mpsc::Sender<UnifiedIncomingMessage>) {
    let evt: BotMenuEvent = match serde_json::from_value(event_data) {
        Ok(e) => e,
        Err(e) => {
            warn!(error = %e, "Failed to parse Lark bot menu event");
            return;
        }
    };

    let user = UnifiedUser {
        id: evt.operator.operator_id.open_id.clone(),
        username: None,
        display_name: evt.operator.operator_id.open_id.clone(),
        avatar_url: None,
    };

    let msg = UnifiedIncomingMessage {
        id: format!("menu_{}", chrono_now()),
        platform: PluginType::Lark,
        chat_id: evt.operator.operator_id.open_id.clone(),
        user,
        content: UnifiedMessageContent {
            content_type: MessageContentType::Command,
            text: format!("/{}", evt.event_key),
            attachments: None,
        },
        timestamp: chrono_now(),
        reply_to_message_id: None,
        action: None,
        raw: None,
    };

    let _ = message_tx.send(msg).await;
}

// ---------------------------------------------------------------------------
// Message content extraction
// ---------------------------------------------------------------------------

/// Extract content type, text, and attachments from a Lark message.
///
/// The `content` field is a JSON string; the structure depends on
/// `message_type` (text, image, file, audio, etc.).
fn extract_message_content(
    message_type: &str,
    content_json: &str,
    mentions: Option<&[super::types::Mention]>,
) -> (MessageContentType, String, Option<Vec<UnifiedAttachment>>) {
    match message_type {
        "text" => {
            let mut text = serde_json::from_str::<super::types::TextContent>(content_json)
                .map(|tc| tc.text)
                .unwrap_or_default();

            // Strip mention placeholders like @_user_1
            if let Some(ms) = mentions {
                for m in ms {
                    text = text.replace(&m.key, "").trim().to_string();
                }
            }

            if text.starts_with('/') {
                (MessageContentType::Command, text, None)
            } else {
                (MessageContentType::Text, text, None)
            }
        }
        "image" => {
            let image_key = serde_json::from_str::<super::types::ImageContent>(content_json)
                .map(|ic| ic.image_key)
                .unwrap_or_default();
            let attachments = vec![UnifiedAttachment {
                file_id: Some(image_key),
                file_name: None,
                mime_type: Some("image/jpeg".into()),
                file_size: None,
                url: None,
            }];
            (MessageContentType::Photo, String::new(), Some(attachments))
        }
        "file" => {
            let fc = serde_json::from_str::<super::types::FileContent>(content_json);
            let (file_key, file_name) = fc.map(|f| (f.file_key, f.file_name)).unwrap_or_default();
            let attachments = vec![UnifiedAttachment {
                file_id: Some(file_key),
                file_name,
                mime_type: None,
                file_size: None,
                url: None,
            }];
            (MessageContentType::Document, String::new(), Some(attachments))
        }
        "audio" => {
            let file_key = serde_json::from_str::<super::types::AudioContent>(content_json)
                .map(|ac| ac.file_key)
                .unwrap_or_default();
            let attachments = vec![UnifiedAttachment {
                file_id: Some(file_key),
                file_name: None,
                mime_type: Some("audio/opus".into()),
                file_size: None,
                url: None,
            }];
            (MessageContentType::Audio, String::new(), Some(attachments))
        }
        _ => {
            // Unsupported type — treat as text with the raw JSON
            (
                MessageContentType::Text,
                format!("[Unsupported message type: {message_type}]"),
                None,
            )
        }
    }
}

// ---------------------------------------------------------------------------
// Event deduplication
// ---------------------------------------------------------------------------

/// Check if an event ID has been seen recently. If not, mark it as seen.
async fn is_duplicate(cache: &Arc<Mutex<HashMap<String, Instant>>>, event_id: &str) -> bool {
    let mut map = cache.lock().await;
    if map.contains_key(event_id) {
        return true;
    }
    map.insert(event_id.to_string(), Instant::now());
    false
}

/// Remove expired entries from the dedup cache.
async fn cleanup_expired_events(cache: &Arc<Mutex<HashMap<String, Instant>>>) {
    let mut map = cache.lock().await;
    let before = map.len();
    map.retain(|_, instant| instant.elapsed() < LARK_EVENT_DEDUP_TTL);
    let removed = before - map.len();
    if removed > 0 {
        debug!(removed, remaining = map.len(), "Lark dedup cache cleanup");
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Truncate a message to the platform limit, appending "..." if truncated.
fn truncate_message(text: &str, limit: usize) -> String {
    if text.len() <= limit {
        return text.to_string();
    }
    let truncated: String = text.chars().take(limit - 3).collect();
    format!("{truncated}...")
}

/// Calculate exponential backoff delay, capped at the maximum.
fn backoff_delay(attempt: u32) -> Duration {
    let delay_secs = 2u64.saturating_pow(attempt).min(MAX_RECONNECT_DELAY.as_secs());
    Duration::from_secs(delay_secs)
}

/// Current unix timestamp in seconds.
fn chrono_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Build a TLS connector for WebSocket connections.
///
/// Explicitly sets ALPN to `http/1.1` only — WebSocket requires an HTTP/1.1
/// upgrade handshake and is incompatible with h2. Without this, some servers
/// negotiate h2 via ALPN and the WebSocket upgrade never completes.
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

/// Extract service_id from a WebSocket URL query string.
fn extract_service_id(url: &str) -> i32 {
    url.split('?')
        .nth(1)
        .unwrap_or("")
        .split('&')
        .find_map(|param| {
            let (k, v) = param.split_once('=')?;
            if k == "service_id" { v.parse().ok() } else { None }
        })
        .unwrap_or(0)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // -- truncate_message ---------------------------------------------------

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
    }

    #[test]
    fn truncate_unicode() {
        let result = truncate_message("你好世界测试文本", 5);
        assert_eq!(result, "你好...");
    }

    // -- backoff_delay ------------------------------------------------------

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

    // -- extract_message_content --------------------------------------------

    #[test]
    fn extract_text_content() {
        let (ct, text, att) = extract_message_content("text", r#"{"text":"Hello"}"#, None);
        assert_eq!(ct, MessageContentType::Text);
        assert_eq!(text, "Hello");
        assert!(att.is_none());
    }

    #[test]
    fn extract_text_command() {
        let (ct, text, _) = extract_message_content("text", r#"{"text":"/start"}"#, None);
        assert_eq!(ct, MessageContentType::Command);
        assert_eq!(text, "/start");
    }

    #[test]
    fn extract_text_strips_mentions() {
        use super::super::types::{Mention, MentionId};
        let mentions = vec![Mention {
            key: "@_user_1".into(),
            id: MentionId {
                open_id: "ou_bot".into(),
                user_id: String::new(),
                union_id: String::new(),
            },
            name: "Bot".into(),
        }];
        let (ct, text, _) = extract_message_content("text", r#"{"text":"@_user_1 Hello bot"}"#, Some(&mentions));
        assert_eq!(ct, MessageContentType::Text);
        assert_eq!(text, "Hello bot");
    }

    #[test]
    fn extract_image_content() {
        let (ct, _, att) = extract_message_content("image", r#"{"image_key":"img_123"}"#, None);
        assert_eq!(ct, MessageContentType::Photo);
        let atts = att.unwrap();
        assert_eq!(atts[0].file_id.as_deref(), Some("img_123"));
    }

    #[test]
    fn extract_file_content() {
        let (ct, _, att) = extract_message_content("file", r#"{"file_key":"file_123","file_name":"doc.pdf"}"#, None);
        assert_eq!(ct, MessageContentType::Document);
        let atts = att.unwrap();
        assert_eq!(atts[0].file_id.as_deref(), Some("file_123"));
        assert_eq!(atts[0].file_name.as_deref(), Some("doc.pdf"));
    }

    #[test]
    fn extract_audio_content() {
        let (ct, _, att) = extract_message_content("audio", r#"{"file_key":"audio_123"}"#, None);
        assert_eq!(ct, MessageContentType::Audio);
        let atts = att.unwrap();
        assert_eq!(atts[0].file_id.as_deref(), Some("audio_123"));
    }

    #[test]
    fn extract_unsupported_type() {
        let (ct, text, _) = extract_message_content("sticker", "{}", None);
        assert_eq!(ct, MessageContentType::Text);
        assert!(text.contains("Unsupported"));
    }

    // -- is_duplicate -------------------------------------------------------

    #[tokio::test]
    async fn dedup_first_seen_not_duplicate() {
        let cache = Arc::new(Mutex::new(HashMap::new()));
        assert!(!is_duplicate(&cache, "ev_1").await);
    }

    #[tokio::test]
    async fn dedup_second_seen_is_duplicate() {
        let cache = Arc::new(Mutex::new(HashMap::new()));
        is_duplicate(&cache, "ev_1").await;
        assert!(is_duplicate(&cache, "ev_1").await);
    }

    #[tokio::test]
    async fn dedup_different_ids_not_duplicate() {
        let cache = Arc::new(Mutex::new(HashMap::new()));
        is_duplicate(&cache, "ev_1").await;
        assert!(!is_duplicate(&cache, "ev_2").await);
    }

    // -- cleanup_expired_events ---------------------------------------------

    #[tokio::test]
    async fn cleanup_removes_expired_entries() {
        let cache = Arc::new(Mutex::new(HashMap::new()));
        {
            let mut map = cache.lock().await;
            // Insert an entry that is already expired
            map.insert(
                "old".into(),
                Instant::now() - LARK_EVENT_DEDUP_TTL - Duration::from_secs(1),
            );
            map.insert("recent".into(), Instant::now());
        }
        cleanup_expired_events(&cache).await;
        let map = cache.lock().await;
        assert!(!map.contains_key("old"));
        assert!(map.contains_key("recent"));
    }

    // -- extract_service_id ---------------------------------------------------

    #[test]
    fn extract_service_id_from_url() {
        let url = "wss://open.feishu.cn/ws/abc?service_id=7&device_id=xxx";
        assert_eq!(extract_service_id(url), 7);
    }

    #[test]
    fn extract_service_id_missing_returns_zero() {
        let url = "wss://open.feishu.cn/ws/abc";
        assert_eq!(extract_service_id(url), 0);
    }

    // -- handle_control_frame -------------------------------------------------

    #[test]
    fn control_frame_pong_with_config_returns_duration() {
        use super::super::frame::PbFrame;
        let frame = PbFrame {
            seq_id: 0,
            log_id: 0,
            service: 1,
            method: 0,
            headers: vec![super::super::frame::PbHeader {
                key: "type".into(),
                value: "pong".into(),
            }],
            payload_encoding: String::new(),
            payload_type: String::new(),
            payload: br#"{"PingInterval":60,"ReconnectCount":5,"ReconnectInterval":30,"ReconnectNonce":10}"#.to_vec(),
            log_id_new: String::new(),
        };
        let result = handle_control_frame(&frame);
        assert_eq!(result, Some(Duration::from_secs(60)));
    }

    #[test]
    fn control_frame_pong_empty_payload_returns_none() {
        use super::super::frame::PbFrame;
        let frame = PbFrame {
            seq_id: 0,
            log_id: 0,
            service: 1,
            method: 0,
            headers: vec![super::super::frame::PbHeader {
                key: "type".into(),
                value: "pong".into(),
            }],
            payload_encoding: String::new(),
            payload_type: String::new(),
            payload: Vec::new(),
            log_id_new: String::new(),
        };
        let result = handle_control_frame(&frame);
        assert_eq!(result, None);
    }

    #[test]
    fn control_frame_ping_returns_none() {
        use super::super::frame::PbFrame;
        let frame = PbFrame {
            seq_id: 0,
            log_id: 0,
            service: 1,
            method: 0,
            headers: vec![super::super::frame::PbHeader {
                key: "type".into(),
                value: "ping".into(),
            }],
            payload_encoding: String::new(),
            payload_type: String::new(),
            payload: Vec::new(),
            log_id_new: String::new(),
        };
        let result = handle_control_frame(&frame);
        assert_eq!(result, None);
    }

    // -- handle_ws_text integration -------------------------------------------

    #[tokio::test]
    async fn ws_text_event_dispatches_message() {
        let (message_tx, mut message_rx) = mpsc::channel(16);
        let (confirm_tx, _confirm_rx) = mpsc::channel(16);
        let dedup_cache = Arc::new(Mutex::new(HashMap::new()));

        let payload = r#"{
            "header": {
                "event_id": "ev_test_1",
                "event_type": "im.message.receive_v1"
            },
            "event": {
                "sender": {
                    "sender_id": { "open_id": "ou_user1", "user_id": "", "union_id": "" },
                    "sender_type": "user",
                    "tenant_key": "t1"
                },
                "message": {
                    "message_id": "msg_test_1",
                    "chat_id": "oc_chat1",
                    "chat_type": "p2p",
                    "message_type": "text",
                    "content": "{\"text\":\"Hello bot\"}"
                }
            }
        }"#;

        handle_ws_text(payload, "event", None, &message_tx, &confirm_tx, &dedup_cache).await;

        let msg = message_rx.try_recv().unwrap();
        assert_eq!(msg.id, "msg_test_1");
        assert_eq!(msg.chat_id, "oc_chat1");
        assert_eq!(msg.content.text, "Hello bot");
        assert_eq!(msg.user.id, "ou_user1");
        assert_eq!(msg.platform, PluginType::Lark);
    }

    #[tokio::test]
    async fn ws_text_event_deduplicates() {
        let (message_tx, mut message_rx) = mpsc::channel(16);
        let (confirm_tx, _confirm_rx) = mpsc::channel(16);
        let dedup_cache = Arc::new(Mutex::new(HashMap::new()));

        let payload = r#"{
            "header": {
                "event_id": "ev_dup_1",
                "event_type": "im.message.receive_v1"
            },
            "event": {
                "sender": {
                    "sender_id": { "open_id": "ou_x", "user_id": "", "union_id": "" },
                    "sender_type": "user",
                    "tenant_key": "t1"
                },
                "message": {
                    "message_id": "msg_dup",
                    "chat_id": "oc_1",
                    "chat_type": "p2p",
                    "message_type": "text",
                    "content": "{\"text\":\"hi\"}"
                }
            }
        }"#;

        handle_ws_text(payload, "event", None, &message_tx, &confirm_tx, &dedup_cache).await;
        handle_ws_text(payload, "event", None, &message_tx, &confirm_tx, &dedup_cache).await;

        assert!(message_rx.try_recv().is_ok());
        assert!(message_rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn ws_text_card_dispatches_action() {
        let (message_tx, mut message_rx) = mpsc::channel(16);
        let (confirm_tx, _confirm_rx) = mpsc::channel(16);
        let dedup_cache = Arc::new(Mutex::new(HashMap::new()));

        let payload = r#"{
            "operator": { "open_id": "ou_actor", "user_id": null },
            "action": { "tag": "button", "value": {"action": "chat:help.show"} },
            "open_message_id": "om_card1",
            "open_chat_id": "oc_chat2"
        }"#;

        handle_ws_text(payload, "card", Some("card_frame_1"), &message_tx, &confirm_tx, &dedup_cache).await;

        let msg = message_rx.try_recv().unwrap();
        assert_eq!(msg.chat_id, "oc_chat2");
        assert_eq!(msg.user.id, "ou_actor");
        assert!(msg.action.is_some());
        let action = msg.action.unwrap();
        assert_eq!(action.action, "help.show");
    }

    #[tokio::test]
    async fn ws_text_card_deduplicates() {
        // A WS re-delivery of the same card-action frame (identical frame
        // `message_id`) must dispatch the action exactly once, so a duplicate
        // chat.regenerate/chat.continue cannot re-fire.
        let (message_tx, mut message_rx) = mpsc::channel(16);
        let (confirm_tx, _confirm_rx) = mpsc::channel(16);
        let dedup_cache = Arc::new(Mutex::new(HashMap::new()));

        let payload = r#"{
            "operator": { "open_id": "ou_actor", "user_id": null },
            "action": { "tag": "button", "value": {"action": "chat:chat.regenerate"} },
            "open_message_id": "om_card_dup",
            "open_chat_id": "oc_chat_dup"
        }"#;

        handle_ws_text(payload, "card", Some("card_frame_dup"), &message_tx, &confirm_tx, &dedup_cache).await;
        handle_ws_text(payload, "card", Some("card_frame_dup"), &message_tx, &confirm_tx, &dedup_cache).await;

        assert!(message_rx.try_recv().is_ok(), "first card frame dispatches");
        assert!(message_rx.try_recv().is_err(), "duplicate card frame is dropped");
    }

    #[tokio::test]
    async fn ws_text_card_confirm_sends_to_confirm_channel() {
        let (message_tx, _message_rx) = mpsc::channel(16);
        let (confirm_tx, mut confirm_rx) = mpsc::channel(16);
        let dedup_cache = Arc::new(Mutex::new(HashMap::new()));

        // NOTE: handle_card_action checks `action == "system.confirm"` but
        // parse_lark_callback splits "system:confirm:..." into category="system",
        // action="confirm". This means the confirm path requires the action field
        // to literally contain "system.confirm" as a dotted string within the
        // "system" category. The format "system:system.confirm:k=v" satisfies this.
        let payload = r#"{
            "operator": { "open_id": "ou_actor" },
            "action": { "tag": "button", "value": {"action": "system:system.confirm:callId=call_123,value=approve"} },
            "open_message_id": "om_1",
            "open_chat_id": "oc_1"
        }"#;

        handle_ws_text(payload, "card", Some("card_frame_1"), &message_tx, &confirm_tx, &dedup_cache).await;

        let (call_id, value) = confirm_rx.try_recv().unwrap();
        assert_eq!(call_id, "call_123");
        assert_eq!(value, "approve");
    }

    #[tokio::test]
    async fn ws_text_unknown_type_does_not_dispatch() {
        let (message_tx, mut message_rx) = mpsc::channel(16);
        let (confirm_tx, _confirm_rx) = mpsc::channel(16);
        let dedup_cache = Arc::new(Mutex::new(HashMap::new()));

        handle_ws_text("{}", "unknown_type", None, &message_tx, &confirm_tx, &dedup_cache).await;

        assert!(message_rx.try_recv().is_err());
    }

    // -- LarkPlugin constructor ---------------------------------------------

    #[test]
    fn new_plugin_initial_state() {
        let plugin = LarkPlugin::new();
        assert_eq!(plugin.status(), PluginStatus::Created);
        assert!(plugin.bot_info().is_none());
        assert!(plugin.last_error().is_none());
        assert_eq!(plugin.plugin_type(), PluginType::Lark);
        assert_eq!(plugin.active_user_count(), 0);
    }
}

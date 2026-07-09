use std::sync::Arc;
use std::time::Duration;

use reqwest::Client;
use tokio::sync::{mpsc, watch};
use tokio::task::JoinHandle;
use tracing::{debug, error, info, warn};

use crate::constants::{TELEGRAM_MAX_RECONNECT_ATTEMPTS, TELEGRAM_MAX_RECONNECT_DELAY, TELEGRAM_MESSAGE_LIMIT};
use crate::error::ChannelError;
use crate::plugin::{ChannelPlugin, PluginCallbacks, SharedPluginStatus, mark_error_on_unexpected_exit};
use crate::types::{
    ActionButton, ActionCategory, ActionContext, BotInfo, MessageContentType, ParseMode, PluginConfig, PluginStatus,
    PluginType, UnifiedAction, UnifiedAttachment, UnifiedIncomingMessage, UnifiedMessageContent,
    UnifiedOutgoingMessage, UnifiedUser,
};

use super::api::TelegramApi;
use super::types::{
    AnswerCallbackQueryRequest, EditMessageTextRequest, InlineKeyboardButton, InlineKeyboardMarkup, KeyboardButton,
    ReplyKeyboardMarkup, ReplyMarkup, SendMessageRequest, TgCallbackQuery, TgMessage,
};
use super::watermark::{UpdateArrival, UpdateWatermark, WatermarkStore, default_watermark_store};

/// Long-polling timeout in seconds (Telegram recommends 20-30s).
const POLL_TIMEOUT: u32 = 25;

/// Telegram Bot plugin implementing long-polling message reception,
/// exponential backoff reconnection, and message send/edit via the
/// Telegram Bot API.
pub struct TelegramPlugin {
    /// Shared with the polling loop so a dead loop can flip it to `Error`.
    status: SharedPluginStatus,
    bot_info: Option<BotInfo>,
    last_error: Option<String>,
    api: Option<Arc<TelegramApi>>,
    callbacks: Option<PluginCallbacks>,
    poll_handle: Option<JoinHandle<()>>,
    shutdown_tx: Option<watch::Sender<bool>>,
    /// Persistence for the last processed update_id, so a restart never
    /// re-executes updates Telegram redelivers (see `watermark` module docs).
    watermark_store: Arc<dyn WatermarkStore>,
}

impl Default for TelegramPlugin {
    fn default() -> Self {
        Self {
            status: SharedPluginStatus::default(),
            bot_info: None,
            last_error: None,
            api: None,
            callbacks: None,
            poll_handle: None,
            shutdown_tx: None,
            watermark_store: default_watermark_store(),
        }
    }
}

impl TelegramPlugin {
    pub fn new() -> Self {
        Self::default()
    }

    /// Construct with a custom watermark store (tests / future DI).
    #[allow(dead_code)]
    pub fn with_watermark_store(store: Arc<dyn WatermarkStore>) -> Self {
        Self {
            watermark_store: store,
            ..Self::default()
        }
    }
}

#[async_trait::async_trait]
impl ChannelPlugin for TelegramPlugin {
    async fn initialize(&mut self, config: PluginConfig, callbacks: PluginCallbacks) -> Result<(), ChannelError> {
        self.status.set(PluginStatus::Initializing);

        let token = config
            .credentials
            .token
            .as_deref()
            .filter(|t| !t.is_empty())
            .ok_or_else(|| {
                self.status.set(PluginStatus::Error);
                self.last_error = Some("Missing Telegram bot token".into());
                ChannelError::InvalidConfig("Missing Telegram bot token".into())
            })?;

        let client = Client::builder()
            .timeout(Duration::from_secs(POLL_TIMEOUT as u64 + 10))
            .build()
            .map_err(|e| {
                self.status.set(PluginStatus::Error);
                self.last_error = Some(format!("HTTP client init failed: {e}"));
                ChannelError::ConnectionFailed(format!("HTTP client init failed: {e}"))
            })?;

        let api = Arc::new(TelegramApi::new(client, token));

        // Validate token by calling getMe
        let me = api.get_me().await.map_err(|e| {
            self.status.set(PluginStatus::Error);
            self.last_error = Some(format!("Token validation failed: {e}"));
            e
        })?;

        self.bot_info = Some(BotInfo {
            id: me.id.to_string(),
            username: me.username.clone(),
            display_name: me.first_name.clone(),
        });

        info!(
            bot_id = me.id,
            bot_username = ?me.username,
            "Telegram bot initialized"
        );

        self.api = Some(api);
        self.callbacks = Some(callbacks);
        self.status.set(PluginStatus::Ready);
        Ok(())
    }

    async fn start(&mut self) -> Result<(), ChannelError> {
        self.status.set(PluginStatus::Starting);

        if self.poll_handle.is_some() {
            self.status.set(PluginStatus::Running);
            return Ok(());
        }

        let api = self
            .api
            .as_ref()
            .cloned()
            .ok_or_else(|| ChannelError::PlatformApi("Telegram plugin not initialized".into()))?;
        let callbacks = self
            .callbacks
            .clone()
            .ok_or_else(|| ChannelError::PlatformApi("Telegram callbacks not initialized".into()))?;

        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        self.shutdown_tx = Some(shutdown_tx);
        // The watermark file is keyed by bot id so switching tokens (a
        // different bot) never inherits a foreign watermark.
        let bot_id = self.bot_info.as_ref().map(|b| b.id.clone()).unwrap_or_default();
        self.poll_handle = Some(tokio::spawn(poll_loop(
            api,
            callbacks.message_tx,
            callbacks.confirm_tx,
            self.status.clone(),
            shutdown_rx,
            bot_id,
            Arc::clone(&self.watermark_store),
        )));

        self.status.set(PluginStatus::Running);
        info!("Telegram plugin started");
        Ok(())
    }

    async fn stop(&mut self) -> Result<(), ChannelError> {
        self.status.set(PluginStatus::Stopping);

        // Signal shutdown to the polling loop
        if let Some(tx) = self.shutdown_tx.take() {
            let _ = tx.send(true);
        }

        // Wait for the polling task to finish
        if let Some(handle) = self.poll_handle.take() {
            // Give it a few seconds to wind down
            let _ = tokio::time::timeout(Duration::from_secs(5), handle).await;
        }

        self.api = None;
        self.callbacks = None;
        self.status.set(PluginStatus::Stopped);
        info!("Telegram plugin stopped");
        Ok(())
    }

    async fn send_message(&self, chat_id: &str, message: UnifiedOutgoingMessage) -> Result<String, ChannelError> {
        let api = self
            .api
            .as_ref()
            .ok_or_else(|| ChannelError::PlatformApi("Plugin not initialized".into()))?;

        let chat_id_num = parse_chat_id(chat_id)?;
        let text = truncate_message(message.text.as_deref().unwrap_or(""), TELEGRAM_MESSAGE_LIMIT);

        let parse_mode = message.parse_mode.map(format_parse_mode);
        let reply_markup = build_reply_markup(&message);
        let reply_to = message
            .reply_to_message_id
            .as_deref()
            .and_then(|id| id.parse::<i64>().ok());

        let req = SendMessageRequest {
            chat_id: chat_id_num,
            text,
            parse_mode,
            reply_to_message_id: reply_to,
            reply_markup,
            disable_notification: message.silent,
        };

        let sent = api.send_message(&req).await?;
        Ok(sent.message_id.to_string())
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

        let chat_id_num = parse_chat_id(chat_id)?;
        let message_id_num = message_id
            .parse::<i64>()
            .map_err(|_| ChannelError::InvalidConfig(format!("Invalid message_id: {message_id}")))?;

        let text = truncate_message(message.text.as_deref().unwrap_or(""), TELEGRAM_MESSAGE_LIMIT);
        let parse_mode = message.parse_mode.map(format_parse_mode);
        let reply_markup = build_inline_markup(&message);

        let req = EditMessageTextRequest {
            chat_id: chat_id_num,
            message_id: message_id_num,
            text,
            parse_mode,
            reply_markup,
        };

        api.edit_message_text(&req).await
    }

    async fn send_media(
        &self,
        chat_id: &str,
        media: crate::types::OutgoingMedia,
        caption: Option<&str>,
    ) -> Result<String, ChannelError> {
        use crate::types::MediaKind;
        let api = self
            .api
            .as_ref()
            .ok_or_else(|| ChannelError::PlatformApi("Plugin not initialized".into()))?;
        let chat_id_num = parse_chat_id(chat_id)?;

        let sent = match media.kind {
            MediaKind::Image => {
                api.send_photo(chat_id_num, media.bytes, &media.filename, &media.mime, caption)
                    .await?
            }
            MediaKind::File => {
                api.send_document(chat_id_num, media.bytes, &media.filename, &media.mime, caption)
                    .await?
            }
        };
        Ok(sent.message_id.to_string())
    }

    fn active_user_count(&self) -> usize {
        // Tracked externally by ChannelManager via SessionManager
        0
    }

    fn bot_info(&self) -> Option<&BotInfo> {
        self.bot_info.as_ref()
    }

    fn plugin_type(&self) -> PluginType {
        PluginType::Telegram
    }

    fn status(&self) -> PluginStatus {
        self.status.get()
    }

    fn last_error(&self) -> Option<&str> {
        self.last_error.as_deref()
    }
}

// ---------------------------------------------------------------------------
// Long-polling loop
// ---------------------------------------------------------------------------

/// Background task that continuously polls Telegram for updates.
///
/// Implements exponential backoff on errors, up to
/// `TELEGRAM_MAX_RECONNECT_ATTEMPTS` consecutive failures.
///
/// Deduplication: Telegram only confirms updates when the next `getUpdates`
/// carries an advanced offset, so a restart between "batch dispatched" and
/// "next poll issued" makes Telegram redeliver the whole batch. The persisted
/// watermark (highest processed update_id per bot) closes that window by
/// seeding the initial offset, letting the SERVER filter the redelivered
/// batch out (at-most-once for agent actions). An update that still arrives
/// at or below the watermark is therefore not a redelivery but Telegram's
/// random update_id sequence reset (bot idle >= 1 week) — it is processed and
/// the watermark rebased, never skipped (see the `watermark` module docs).
async fn poll_loop(
    api: Arc<TelegramApi>,
    message_tx: mpsc::Sender<UnifiedIncomingMessage>,
    confirm_tx: mpsc::Sender<(String, String)>,
    status: SharedPluginStatus,
    mut shutdown_rx: watch::Receiver<bool>,
    bot_id: String,
    watermark_store: Arc<dyn WatermarkStore>,
) {
    let mut watermark = UpdateWatermark::new(watermark_store.load(&bot_id));
    // Seed the offset from the watermark: the very first getUpdates then
    // confirms the pre-restart batch server-side AND filters it out of the
    // response. Dedup thus relies entirely on this server-side offset filter;
    // anything that still arrives at or below the watermark is a sequence
    // reset (handled in the loop below), never a redelivery.
    let mut offset: Option<i64> = watermark.next_offset();
    if let Some(last) = watermark.last_processed() {
        info!(bot_id = %bot_id, last_processed_update_id = last, "Telegram poll loop resuming from persisted watermark");
    }
    let mut consecutive_errors: u32 = 0;

    loop {
        // Check shutdown signal
        if *shutdown_rx.borrow() {
            debug!("Telegram poll loop received shutdown signal");
            break;
        }

        match api.get_updates(offset, POLL_TIMEOUT).await {
            Ok(updates) => {
                consecutive_errors = 0;

                for update in updates {
                    // Advance offset past this update
                    offset = Some(update.update_id + 1);

                    // The offset is always seeded past the watermark, so
                    // genuine redeliveries are filtered server-side and never
                    // reach this loop. An id at or below the watermark can
                    // only mean Telegram randomly reset the update_id
                    // sequence (bot idle >= 1 week). Skipping it would
                    // silently drop every message of the new (lower)
                    // sequence forever — process it and rebase instead.
                    if watermark.classify(update.update_id) == UpdateArrival::SequenceReset {
                        warn!(
                            update_id = update.update_id,
                            watermark = watermark.last_processed().unwrap_or_default(),
                            "Telegram update_id sequence reset detected (update at/below watermark despite seeded offset); processing and rebasing watermark"
                        );
                    }

                    if let Some(cb) = update.callback_query {
                        handle_callback_query(&api, &cb, &message_tx, &confirm_tx).await;
                    } else if let Some(msg) = update.message {
                        handle_message(&msg, &message_tx).await;
                    }

                    // Record + persist the watermark immediately after the
                    // update was dispatched onto the orchestrator queue — not
                    // after the agent finished handling it. Deliberate
                    // trade-off: dying between dispatch and persist re-runs
                    // this one update (tiny window), while dying after
                    // persist but before the queued message is fully handled
                    // — a window of seconds to minutes of agent work — loses
                    // it. We prefer losing one IM message over re-executing a
                    // creation-style agent action (see `watermark` docs).
                    watermark.record(update.update_id);
                    watermark_store.save(&bot_id, update.update_id);
                }
            }
            Err(e) => {
                consecutive_errors += 1;
                warn!(
                    error = %e,
                    consecutive_errors,
                    "Telegram poll error"
                );

                if consecutive_errors >= TELEGRAM_MAX_RECONNECT_ATTEMPTS {
                    error!("Telegram max reconnect attempts reached, stopping poll loop");
                    break;
                }

                let backoff = backoff_delay(consecutive_errors);
                tokio::select! {
                    _ = tokio::time::sleep(backoff) => {}
                    _ = shutdown_rx.changed() => {
                        debug!("Telegram poll loop shutdown during backoff");
                        break;
                    }
                }
            }
        }
    }

    // The loop only exits via shutdown or reconnect exhaustion. For any
    // non-shutdown exit the plugin is deaf to new messages while the facade
    // still says Running — flip the shared status to Error so the manager
    // watchdog can persist/broadcast the real state and attempt a restart.
    mark_error_on_unexpected_exit(&status, &shutdown_rx, "telegram");

    debug!("Telegram poll loop exited");
}

/// Calculate exponential backoff delay, capped at the configured maximum.
fn backoff_delay(attempt: u32) -> Duration {
    let delay_secs = 2u64.saturating_pow(attempt).min(TELEGRAM_MAX_RECONNECT_DELAY.as_secs());
    Duration::from_secs(delay_secs)
}

// ---------------------------------------------------------------------------
// Update handlers
// ---------------------------------------------------------------------------

/// Handle a callback query (inline keyboard button press).
///
/// Parses `callback_data` as `"category:action"` or `"category:action:k=v,k=v"`,
/// builds a `UnifiedIncomingMessage` with the parsed action, then acknowledges
/// the callback to dismiss the loading indicator on the client.
async fn handle_callback_query(
    api: &TelegramApi,
    cb: &TgCallbackQuery,
    message_tx: &mpsc::Sender<UnifiedIncomingMessage>,
    confirm_tx: &mpsc::Sender<(String, String)>,
) {
    let data = cb.data.as_deref().unwrap_or("");
    let parsed = parse_callback_data(data);

    // Check if this is a tool confirmation callback (system.confirm:callId=X,value=Y)
    if let Some(action) = &parsed
        && action.action == "system.confirm"
        && let Some(params) = &action.params
    {
        let call_id = params.get("callId").cloned().unwrap_or_default();
        let value = params.get("value").cloned().unwrap_or_default();
        if !call_id.is_empty() {
            let _ = confirm_tx.send((call_id, value)).await;
        }
    }

    let chat_id = cb.message.as_ref().map(|m| m.chat.id).unwrap_or(cb.from.id);

    let message_id = cb.message.as_ref().map(|m| m.message_id.to_string());

    let user = UnifiedUser {
        id: cb.from.id.to_string(),
        username: cb.from.username.clone(),
        display_name: build_display_name(&cb.from.first_name, cb.from.last_name.as_deref()),
        avatar_url: None,
    };

    let unified_action = parsed.map(|a| UnifiedAction {
        action: a.action,
        category: a.category,
        params: a.params,
        context: ActionContext {
            platform: PluginType::Telegram,
            user_id: cb.from.id.to_string(),
            chat_id: chat_id.to_string(),
            message_id: message_id.clone(),
            session_id: None,
        },
    });

    let msg = UnifiedIncomingMessage {
        id: cb.id.clone(),
        platform: PluginType::Telegram,
        chat_id: chat_id.to_string(),
        user,
        content: UnifiedMessageContent {
            content_type: MessageContentType::Action,
            text: data.to_string(),
            attachments: None,
        },
        timestamp: chrono_now(),
        reply_to_message_id: None,
        action: unified_action,
        raw: None,
    };

    let _ = message_tx.send(msg).await;

    // Acknowledge the callback query
    let ack = AnswerCallbackQueryRequest {
        callback_query_id: cb.id.clone(),
        text: None,
        show_alert: None,
    };
    let _ = api.answer_callback_query(&ack).await;
}

/// Handle a regular text/media message from Telegram.
async fn handle_message(msg: &TgMessage, message_tx: &mpsc::Sender<UnifiedIncomingMessage>) {
    let from = match &msg.from {
        Some(u) => u,
        None => return, // system messages without a sender
    };

    let user = UnifiedUser {
        id: from.id.to_string(),
        username: from.username.clone(),
        display_name: build_display_name(&from.first_name, from.last_name.as_deref()),
        avatar_url: None,
    };

    let (content_type, text, attachments) = extract_content(msg);

    let reply_to = msg.reply_to_message.as_ref().map(|r| r.message_id.to_string());

    let unified = UnifiedIncomingMessage {
        id: msg.message_id.to_string(),
        platform: PluginType::Telegram,
        chat_id: msg.chat.id.to_string(),
        user,
        content: UnifiedMessageContent {
            content_type,
            text,
            attachments,
        },
        timestamp: msg.date,
        reply_to_message_id: reply_to,
        action: None,
        raw: None,
    };

    let _ = message_tx.send(unified).await;
}

// ---------------------------------------------------------------------------
// Content extraction
// ---------------------------------------------------------------------------

/// Extract content type, text, and attachments from a Telegram message.
fn extract_content(msg: &TgMessage) -> (MessageContentType, String, Option<Vec<UnifiedAttachment>>) {
    // For media messages, Telegram puts text in `caption` (not `text`).
    let caption = msg.caption.clone().unwrap_or_default();

    // Photo — pick the largest resolution
    if let Some(photos) = &msg.photo {
        let best = photos.iter().max_by_key(|p| p.width * p.height);
        let attachments = best.map(|p| {
            vec![UnifiedAttachment {
                file_id: Some(p.file_id.clone()),
                file_name: None,
                mime_type: Some("image/jpeg".into()),
                file_size: p.file_size,
                url: None,
            }]
        });
        return (MessageContentType::Photo, caption, attachments);
    }

    // Document
    if let Some(doc) = &msg.document {
        let attachments = vec![UnifiedAttachment {
            file_id: Some(doc.file_id.clone()),
            file_name: doc.file_name.clone(),
            mime_type: doc.mime_type.clone(),
            file_size: doc.file_size,
            url: None,
        }];
        return (MessageContentType::Document, caption, Some(attachments));
    }

    // Voice
    if let Some(voice) = &msg.voice {
        let attachments = vec![UnifiedAttachment {
            file_id: Some(voice.file_id.clone()),
            file_name: None,
            mime_type: voice.mime_type.clone(),
            file_size: voice.file_size,
            url: None,
        }];
        return (MessageContentType::Voice, caption, Some(attachments));
    }

    // Audio
    if let Some(audio) = &msg.audio {
        let attachments = vec![UnifiedAttachment {
            file_id: Some(audio.file_id.clone()),
            file_name: audio.file_name.clone(),
            mime_type: audio.mime_type.clone(),
            file_size: audio.file_size,
            url: None,
        }];
        return (MessageContentType::Audio, caption, Some(attachments));
    }

    // Video
    if let Some(video) = &msg.video {
        let attachments = vec![UnifiedAttachment {
            file_id: Some(video.file_id.clone()),
            file_name: video.file_name.clone(),
            mime_type: video.mime_type.clone(),
            file_size: video.file_size,
            url: None,
        }];
        return (MessageContentType::Video, caption, Some(attachments));
    }

    // Sticker
    if let Some(sticker) = &msg.sticker {
        let text = sticker.emoji.clone().unwrap_or_default();
        let attachments = vec![UnifiedAttachment {
            file_id: Some(sticker.file_id.clone()),
            file_name: None,
            mime_type: None,
            file_size: None,
            url: None,
        }];
        return (MessageContentType::Sticker, text, Some(attachments));
    }

    // Text (default)
    let text = msg.text.clone().unwrap_or_default();

    // Detect commands (messages starting with '/')
    if text.starts_with('/') {
        return (MessageContentType::Command, text, None);
    }

    (MessageContentType::Text, text, None)
}

// ---------------------------------------------------------------------------
// Callback data parsing
// ---------------------------------------------------------------------------

/// Parsed callback data from an inline keyboard button.
struct ParsedCallback {
    category: ActionCategory,
    action: String,
    params: Option<std::collections::HashMap<String, String>>,
}

/// Parse callback_data string `"category:action"` or `"category:action:k=v,k=v"`.
fn parse_callback_data(data: &str) -> Option<ParsedCallback> {
    let parts: Vec<&str> = data.splitn(3, ':').collect();
    if parts.len() < 2 {
        return None;
    }

    let category = match parts[0] {
        "platform" => ActionCategory::Platform,
        "system" => ActionCategory::System,
        "chat" => ActionCategory::Chat,
        _ => return None,
    };

    let action = parts[1].to_string();

    let params = if parts.len() == 3 && !parts[2].is_empty() {
        let mut map = std::collections::HashMap::new();
        for pair in parts[2].split(',') {
            if let Some((k, v)) = pair.split_once('=') {
                map.insert(k.to_string(), v.to_string());
            }
        }
        if map.is_empty() { None } else { Some(map) }
    } else {
        None
    };

    Some(ParsedCallback {
        category,
        action,
        params,
    })
}

// ---------------------------------------------------------------------------
// Reply markup builders
// ---------------------------------------------------------------------------

/// Build combined reply markup from an outgoing message.
/// Inline buttons take priority over keyboard buttons.
fn build_reply_markup(msg: &UnifiedOutgoingMessage) -> Option<ReplyMarkup> {
    if let Some(markup) = build_inline_markup(msg) {
        return Some(markup);
    }
    build_keyboard_markup(msg)
}

/// Build inline keyboard markup from `buttons` field.
fn build_inline_markup(msg: &UnifiedOutgoingMessage) -> Option<ReplyMarkup> {
    let buttons = msg.buttons.as_ref()?;
    let rows: Vec<Vec<InlineKeyboardButton>> = buttons
        .iter()
        .map(|row| {
            row.iter()
                .map(|btn| InlineKeyboardButton {
                    text: btn.label.clone(),
                    callback_data: Some(format_callback_data(btn)),
                    url: None,
                })
                .collect()
        })
        .collect();

    if rows.is_empty() {
        return None;
    }

    Some(ReplyMarkup::InlineKeyboard(InlineKeyboardMarkup {
        inline_keyboard: rows,
    }))
}

/// Build reply keyboard markup from `keyboard` field.
fn build_keyboard_markup(msg: &UnifiedOutgoingMessage) -> Option<ReplyMarkup> {
    let keyboard = msg.keyboard.as_ref()?;
    let rows: Vec<Vec<KeyboardButton>> = keyboard
        .iter()
        .map(|row| {
            row.iter()
                .map(|btn| KeyboardButton {
                    text: btn.label.clone(),
                })
                .collect()
        })
        .collect();

    if rows.is_empty() {
        return None;
    }

    Some(ReplyMarkup::ReplyKeyboard(ReplyKeyboardMarkup {
        keyboard: rows,
        resize_keyboard: Some(true),
        one_time_keyboard: None,
    }))
}

/// Derive the category prefix from an action name.
///
/// The mapping follows the `ActionCategory` routing in `ActionExecutor`:
///   - `system.confirm` → `"chat"` (routed to `handle_chat_action`)
///   - `pairing.*` → `"platform"`
///   - `chat.*` / `action.*` → `"chat"`
///   - everything else (`session.*`, `help.*`, `settings.*`, `agent.*`, `system.*`) → `"system"`
fn action_category_prefix(action: &str) -> &'static str {
    // Full-name overrides first: `system.confirm` is handled by
    // `handle_chat_action` in ActionExecutor despite the "system." prefix.
    if action == "system.confirm" {
        return "chat";
    }
    let prefix = action.split('.').next().unwrap_or("");
    match prefix {
        "pairing" => "platform",
        "chat" | "action" => "chat",
        _ => "system",
    }
}

/// Encode an ActionButton into callback_data format:
/// `"category:action"` or `"category:action:k=v,k=v"`.
///
/// This is the inverse of [`parse_callback_data`].
fn format_callback_data(btn: &ActionButton) -> String {
    let category = action_category_prefix(&btn.action);
    match &btn.params {
        Some(params) if !params.is_empty() => {
            let encoded: Vec<String> = params.iter().map(|(k, v)| format!("{k}={v}")).collect();
            format!("{category}:{}:{}", btn.action, encoded.join(","))
        }
        _ => format!("{category}:{}", btn.action),
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Parse a chat_id string to i64.
fn parse_chat_id(chat_id: &str) -> Result<i64, ChannelError> {
    chat_id
        .parse::<i64>()
        .map_err(|_| ChannelError::InvalidConfig(format!("Invalid chat_id: {chat_id}")))
}

/// Truncate a message to the platform limit, appending "..." if truncated.
fn truncate_message(text: &str, limit: usize) -> String {
    if text.len() <= limit {
        return text.to_string();
    }
    // Truncate at char boundary, leave room for "..."
    let truncated: String = text.chars().take(limit - 3).collect();
    format!("{truncated}...")
}

/// Build display name from first + last name.
fn build_display_name(first: &str, last: Option<&str>) -> String {
    match last {
        Some(l) if !l.is_empty() => format!("{first} {l}"),
        _ => first.to_string(),
    }
}

/// Convert ParseMode enum to Telegram API string.
fn format_parse_mode(mode: ParseMode) -> String {
    match mode {
        ParseMode::HTML => "HTML".into(),
        ParseMode::MarkdownV2 => "MarkdownV2".into(),
        ParseMode::Markdown => "Markdown".into(),
    }
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
    use std::collections::HashMap;

    // -- truncate_message ---------------------------------------------------

    #[test]
    fn truncate_within_limit() {
        let text = "Hello, world!";
        assert_eq!(truncate_message(text, 100), "Hello, world!");
    }

    #[test]
    fn truncate_at_limit() {
        let text = "abc";
        assert_eq!(truncate_message(text, 3), "abc");
    }

    #[test]
    fn truncate_exceeds_limit() {
        let text = "Hello, world!";
        let result = truncate_message(text, 10);
        assert_eq!(result, "Hello, ...");
        assert!(result.len() <= 10);
    }

    #[test]
    fn truncate_unicode() {
        let text = "你好世界测试文本";
        let result = truncate_message(text, 5);
        // chars().take(2) = "你好", then "..."
        assert_eq!(result, "你好...");
    }

    // -- parse_chat_id ------------------------------------------------------

    #[test]
    fn parse_valid_chat_id() {
        assert_eq!(parse_chat_id("12345").unwrap(), 12345);
        assert_eq!(parse_chat_id("-100123456").unwrap(), -100123456);
    }

    #[test]
    fn parse_invalid_chat_id() {
        assert!(parse_chat_id("abc").is_err());
        assert!(parse_chat_id("").is_err());
    }

    // -- build_display_name -------------------------------------------------

    #[test]
    fn display_name_first_only() {
        assert_eq!(build_display_name("Alice", None), "Alice");
        assert_eq!(build_display_name("Alice", Some("")), "Alice");
    }

    #[test]
    fn display_name_full() {
        assert_eq!(build_display_name("Alice", Some("Smith")), "Alice Smith");
    }

    // -- parse_callback_data ------------------------------------------------

    #[test]
    fn parse_callback_category_action() {
        let result = parse_callback_data("system:session.new").unwrap();
        assert_eq!(result.category, ActionCategory::System);
        assert_eq!(result.action, "session.new");
        assert!(result.params.is_none());
    }

    #[test]
    fn parse_callback_with_params() {
        let result = parse_callback_data("system:system.confirm:callId=abc,value=yes").unwrap();
        assert_eq!(result.category, ActionCategory::System);
        assert_eq!(result.action, "system.confirm");
        let params = result.params.unwrap();
        assert_eq!(params.get("callId").unwrap(), "abc");
        assert_eq!(params.get("value").unwrap(), "yes");
    }

    #[test]
    fn parse_callback_invalid() {
        assert!(parse_callback_data("invalid").is_none());
        assert!(parse_callback_data("unknown:action").is_none());
    }

    #[test]
    fn parse_callback_platform_category() {
        let result = parse_callback_data("platform:pairing.show").unwrap();
        assert_eq!(result.category, ActionCategory::Platform);
        assert_eq!(result.action, "pairing.show");
    }

    #[test]
    fn parse_callback_chat_category() {
        let result = parse_callback_data("chat:chat.send").unwrap();
        assert_eq!(result.category, ActionCategory::Chat);
        assert_eq!(result.action, "chat.send");
    }

    // -- format_callback_data -----------------------------------------------

    #[test]
    fn format_callback_no_params() {
        let btn = ActionButton {
            label: "Test".into(),
            action: "help.show".into(),
            params: None,
        };
        assert_eq!(format_callback_data(&btn), "system:help.show");
    }

    #[test]
    fn format_callback_with_params() {
        let mut params = HashMap::new();
        params.insert("agentType".into(), "gemini".into());
        let btn = ActionButton {
            label: "Test".into(),
            action: "agent.select".into(),
            params: Some(params),
        };
        let result = format_callback_data(&btn);
        assert!(result.starts_with("system:agent.select:"));
        assert!(result.contains("agentType=gemini"));
    }

    #[test]
    fn format_callback_chat_category() {
        let btn = ActionButton {
            label: "Regen".into(),
            action: "chat.regenerate".into(),
            params: None,
        };
        assert_eq!(format_callback_data(&btn), "chat:chat.regenerate");
    }

    #[test]
    fn format_callback_platform_category() {
        let btn = ActionButton {
            label: "Pair".into(),
            action: "pairing.show".into(),
            params: None,
        };
        assert_eq!(format_callback_data(&btn), "platform:pairing.show");
    }

    // -- action_category_prefix ------------------------------------------------

    #[test]
    fn category_prefix_mapping() {
        assert_eq!(action_category_prefix("pairing.show"), "platform");
        assert_eq!(action_category_prefix("pairing.refresh"), "platform");
        assert_eq!(action_category_prefix("chat.send"), "chat");
        assert_eq!(action_category_prefix("chat.regenerate"), "chat");
        assert_eq!(action_category_prefix("action.copy"), "chat");
        assert_eq!(action_category_prefix("session.new"), "system");
        assert_eq!(action_category_prefix("help.show"), "system");
        assert_eq!(action_category_prefix("agent.select"), "system");
        assert_eq!(action_category_prefix("system.confirm"), "chat");
        assert_eq!(action_category_prefix("settings.show"), "system");
    }

    // -- roundtrip format ↔ parse ----------------------------------------------

    #[test]
    fn roundtrip_no_params() {
        let btn = ActionButton {
            label: "Help".into(),
            action: "help.show".into(),
            params: None,
        };
        let encoded = format_callback_data(&btn);
        let parsed = parse_callback_data(&encoded).expect("should parse");
        assert_eq!(parsed.category, ActionCategory::System);
        assert_eq!(parsed.action, "help.show");
        assert!(parsed.params.is_none());
    }

    #[test]
    fn roundtrip_with_params() {
        let btn = ActionButton {
            label: "Confirm".into(),
            action: "system.confirm".into(),
            params: Some(HashMap::from([
                ("callId".into(), "abc123".into()),
                ("value".into(), "yes".into()),
            ])),
        };
        let encoded = format_callback_data(&btn);
        let parsed = parse_callback_data(&encoded).expect("should parse");
        // system.confirm is routed to handle_chat_action, so category is Chat
        assert_eq!(parsed.category, ActionCategory::Chat);
        assert_eq!(parsed.action, "system.confirm");
        let params = parsed.params.expect("should have params");
        assert_eq!(params.get("callId").unwrap(), "abc123");
        assert_eq!(params.get("value").unwrap(), "yes");
    }

    #[test]
    fn roundtrip_chat_action() {
        let btn = ActionButton {
            label: "Regen".into(),
            action: "chat.regenerate".into(),
            params: None,
        };
        let encoded = format_callback_data(&btn);
        let parsed = parse_callback_data(&encoded).expect("should parse");
        assert_eq!(parsed.category, ActionCategory::Chat);
        assert_eq!(parsed.action, "chat.regenerate");
    }

    #[test]
    fn roundtrip_platform_action() {
        let btn = ActionButton {
            label: "Refresh".into(),
            action: "pairing.refresh".into(),
            params: None,
        };
        let encoded = format_callback_data(&btn);
        let parsed = parse_callback_data(&encoded).expect("should parse");
        assert_eq!(parsed.category, ActionCategory::Platform);
        assert_eq!(parsed.action, "pairing.refresh");
    }

    // -- format_parse_mode --------------------------------------------------

    #[test]
    fn parse_mode_formats() {
        assert_eq!(format_parse_mode(ParseMode::HTML), "HTML");
        assert_eq!(format_parse_mode(ParseMode::MarkdownV2), "MarkdownV2");
        assert_eq!(format_parse_mode(ParseMode::Markdown), "Markdown");
    }

    // -- build_reply_markup -------------------------------------------------

    #[test]
    fn build_inline_markup_from_buttons() {
        let msg = UnifiedOutgoingMessage {
            message_type: crate::types::OutgoingMessageType::Buttons,
            text: Some("Choose".into()),
            parse_mode: None,
            buttons: Some(vec![vec![ActionButton {
                label: "Yes".into(),
                action: "confirm.yes".into(),
                params: None,
            }]]),
            keyboard: None,
            image_url: None,
            file_url: None,
            file_name: None,
            media_actions: None,
            reply_to_message_id: None,
            silent: None,
        };
        let markup = build_reply_markup(&msg);
        assert!(matches!(markup, Some(ReplyMarkup::InlineKeyboard(_))));
    }

    #[test]
    fn build_keyboard_markup_from_keyboard() {
        let msg = UnifiedOutgoingMessage {
            message_type: crate::types::OutgoingMessageType::Text,
            text: Some("Choose".into()),
            parse_mode: None,
            buttons: None,
            keyboard: Some(vec![vec![ActionButton {
                label: "/start".into(),
                action: "start".into(),
                params: None,
            }]]),
            image_url: None,
            file_url: None,
            file_name: None,
            media_actions: None,
            reply_to_message_id: None,
            silent: None,
        };
        let markup = build_reply_markup(&msg);
        assert!(matches!(markup, Some(ReplyMarkup::ReplyKeyboard(_))));
    }

    #[test]
    fn build_no_markup() {
        let msg = UnifiedOutgoingMessage {
            message_type: crate::types::OutgoingMessageType::Text,
            text: Some("Plain".into()),
            parse_mode: None,
            buttons: None,
            keyboard: None,
            image_url: None,
            file_url: None,
            file_name: None,
            media_actions: None,
            reply_to_message_id: None,
            silent: None,
        };
        assert!(build_reply_markup(&msg).is_none());
    }

    // -- extract_content ----------------------------------------------------

    #[test]
    fn extract_text_content() {
        let msg = make_tg_message(Some("Hello"), None, None, None, None, None, None);
        let (content_type, text, attachments) = extract_content(&msg);
        assert_eq!(content_type, MessageContentType::Text);
        assert_eq!(text, "Hello");
        assert!(attachments.is_none());
    }

    #[test]
    fn extract_command_content() {
        let msg = make_tg_message(Some("/start"), None, None, None, None, None, None);
        let (content_type, text, _) = extract_content(&msg);
        assert_eq!(content_type, MessageContentType::Command);
        assert_eq!(text, "/start");
    }

    #[test]
    fn extract_photo_content() {
        use super::super::types::TgPhotoSize;
        let msg = make_tg_message(
            None,
            Some(vec![
                TgPhotoSize {
                    file_id: "small".into(),
                    file_unique_id: "u1".into(),
                    width: 90,
                    height: 90,
                    file_size: None,
                },
                TgPhotoSize {
                    file_id: "large".into(),
                    file_unique_id: "u2".into(),
                    width: 800,
                    height: 600,
                    file_size: Some(50000),
                },
            ]),
            None,
            None,
            None,
            None,
            None,
        );
        let (content_type, _, attachments) = extract_content(&msg);
        assert_eq!(content_type, MessageContentType::Photo);
        let atts = attachments.unwrap();
        assert_eq!(atts.len(), 1);
        assert_eq!(atts[0].file_id.as_deref(), Some("large"));
    }

    #[test]
    fn extract_document_content() {
        use super::super::types::TgDocument;
        let msg = make_tg_message(
            None,
            None,
            Some(TgDocument {
                file_id: "doc_1".into(),
                file_name: Some("test.pdf".into()),
                mime_type: Some("application/pdf".into()),
                file_size: Some(1024),
            }),
            None,
            None,
            None,
            None,
        );
        let (content_type, _, attachments) = extract_content(&msg);
        assert_eq!(content_type, MessageContentType::Document);
        let atts = attachments.unwrap();
        assert_eq!(atts[0].file_name.as_deref(), Some("test.pdf"));
    }

    #[test]
    fn extract_sticker_content() {
        use super::super::types::TgSticker;
        let msg = make_tg_message(
            None,
            None,
            None,
            None,
            None,
            None,
            Some(TgSticker {
                file_id: "sticker_1".into(),
                emoji: Some("😀".into()),
            }),
        );
        let (content_type, text, attachments) = extract_content(&msg);
        assert_eq!(content_type, MessageContentType::Sticker);
        assert_eq!(text, "😀");
        assert!(attachments.is_some());
    }

    #[test]
    fn extract_photo_caption() {
        use super::super::types::TgPhotoSize;
        let msg = make_tg_message_with_caption(
            None,
            Some("Check this out"),
            Some(vec![TgPhotoSize {
                file_id: "p1".into(),
                file_unique_id: "u1".into(),
                width: 100,
                height: 100,
                file_size: None,
            }]),
            None,
            None,
            None,
            None,
            None,
        );
        let (content_type, text, _) = extract_content(&msg);
        assert_eq!(content_type, MessageContentType::Photo);
        assert_eq!(text, "Check this out");
    }

    #[test]
    fn extract_document_caption() {
        use super::super::types::TgDocument;
        let msg = make_tg_message_with_caption(
            None,
            Some("My report"),
            None,
            Some(TgDocument {
                file_id: "d1".into(),
                file_name: Some("report.pdf".into()),
                mime_type: Some("application/pdf".into()),
                file_size: Some(2048),
            }),
            None,
            None,
            None,
            None,
        );
        let (content_type, text, _) = extract_content(&msg);
        assert_eq!(content_type, MessageContentType::Document);
        assert_eq!(text, "My report");
    }

    // -- backoff_delay ------------------------------------------------------

    #[test]
    fn backoff_exponential() {
        assert_eq!(backoff_delay(1), Duration::from_secs(2));
        assert_eq!(backoff_delay(2), Duration::from_secs(4));
        assert_eq!(backoff_delay(3), Duration::from_secs(8));
        assert_eq!(backoff_delay(4), Duration::from_secs(16));
    }

    #[test]
    fn backoff_capped() {
        // 2^5 = 32, capped to 30
        assert_eq!(backoff_delay(5), Duration::from_secs(30));
        assert_eq!(backoff_delay(10), Duration::from_secs(30));
    }

    // -- TelegramPlugin constructor -----------------------------------------

    #[test]
    fn new_plugin_initial_state() {
        let plugin = TelegramPlugin::new();
        assert_eq!(plugin.status(), PluginStatus::Created);
        assert!(plugin.bot_info().is_none());
        assert!(plugin.last_error().is_none());
        assert_eq!(plugin.plugin_type(), PluginType::Telegram);
        assert_eq!(plugin.active_user_count(), 0);
    }

    // -- Test helpers -------------------------------------------------------

    fn make_tg_message(
        text: Option<&str>,
        photo: Option<Vec<super::super::types::TgPhotoSize>>,
        document: Option<super::super::types::TgDocument>,
        voice: Option<super::super::types::TgVoice>,
        audio: Option<super::super::types::TgAudio>,
        video: Option<super::super::types::TgVideo>,
        sticker: Option<super::super::types::TgSticker>,
    ) -> TgMessage {
        make_tg_message_with_caption(text, None, photo, document, voice, audio, video, sticker)
    }

    #[allow(clippy::too_many_arguments)] // test helper requires all media variants
    fn make_tg_message_with_caption(
        text: Option<&str>,
        caption: Option<&str>,
        photo: Option<Vec<super::super::types::TgPhotoSize>>,
        document: Option<super::super::types::TgDocument>,
        voice: Option<super::super::types::TgVoice>,
        audio: Option<super::super::types::TgAudio>,
        video: Option<super::super::types::TgVideo>,
        sticker: Option<super::super::types::TgSticker>,
    ) -> TgMessage {
        use super::super::types::TgChat;
        TgMessage {
            message_id: 1,
            from: None,
            chat: TgChat {
                id: 1,
                chat_type: "private".into(),
                title: None,
            },
            date: 1700000000,
            text: text.map(String::from),
            caption: caption.map(String::from),
            photo,
            document,
            voice,
            audio,
            video,
            sticker,
            reply_to_message: None,
        }
    }
}

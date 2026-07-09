use std::sync::Arc;
use std::time::Duration;

use dashmap::DashMap;
use reqwest::Client;
use tokio::sync::watch;
use tokio::task::JoinHandle;
use tracing::{debug, info, warn};

use crate::constants::{WEIXIN_BACKOFF_DELAY, WEIXIN_MAX_RETRIES, WEIXIN_POLL_TIMEOUT, WEIXIN_RETRY_DELAY};
use crate::error::ChannelError;
use crate::plugin::{ChannelPlugin, PluginCallbacks};
use crate::types::{
    BotInfo, MessageContentType, PluginConfig, PluginStatus, PluginType, UnifiedIncomingMessage, UnifiedMessageContent,
    UnifiedOutgoingMessage, UnifiedUser,
};

use super::api::WeixinApi;
use super::types::{ITEM_TYPE_TEXT, ITEM_TYPE_VOICE, WeixinRawItem, WeixinRawMessage};

/// Default base URL for the iLink Bot API.
const DEFAULT_BASE_URL: &str = "https://ilinkai.weixin.qq.com";

/// WeChat (iLink Bot) platform plugin.
///
/// Connects via long-polling (buffer-based `getupdates`), handles text/voice
/// messages. Does not support editing messages (WeChat limitation);
/// `edit_message` sends a new reply instead.
pub struct WeixinPlugin {
    status: PluginStatus,
    bot_info: Option<BotInfo>,
    last_error: Option<String>,
    api: Option<Arc<WeixinApi>>,
    poll_handle: Option<JoinHandle<()>>,
    shutdown_tx: Option<watch::Sender<bool>>,
    context_tokens: Arc<DashMap<String, String>>,
}

impl Default for WeixinPlugin {
    fn default() -> Self {
        Self {
            status: PluginStatus::Created,
            bot_info: None,
            last_error: None,
            api: None,
            poll_handle: None,
            shutdown_tx: None,
            context_tokens: Arc::new(DashMap::new()),
        }
    }
}

impl WeixinPlugin {
    pub fn new() -> Self {
        Self::default()
    }
}

#[async_trait::async_trait]
impl ChannelPlugin for WeixinPlugin {
    async fn initialize(&mut self, config: PluginConfig, callbacks: PluginCallbacks) -> Result<(), ChannelError> {
        self.status = PluginStatus::Initializing;

        let bot_token = config
            .credentials
            .bot_token
            .as_deref()
            .filter(|t| !t.is_empty())
            .ok_or_else(|| {
                self.status = PluginStatus::Error;
                self.last_error = Some("Missing WeChat bot_token".into());
                ChannelError::InvalidConfig("Missing WeChat bot_token".into())
            })?;

        let account_id = config
            .credentials
            .account_id
            .as_deref()
            .filter(|a| !a.is_empty())
            .ok_or_else(|| {
                self.status = PluginStatus::Error;
                self.last_error = Some("Missing WeChat account_id".into());
                ChannelError::InvalidConfig("Missing WeChat account_id".into())
            })?;

        let base_url = config
            .credentials
            .extra
            .get("baseUrl")
            .and_then(|v| v.as_str())
            .unwrap_or(DEFAULT_BASE_URL);

        let http_client = Client::builder()
            .timeout(Duration::from_secs(WEIXIN_POLL_TIMEOUT.as_secs() + 10))
            .build()
            .map_err(|e| {
                self.status = PluginStatus::Error;
                self.last_error = Some(format!("HTTP client init failed: {e}"));
                ChannelError::ConnectionFailed(format!("HTTP client init failed: {e}"))
            })?;

        let api = Arc::new(WeixinApi::new(http_client, base_url, bot_token));

        self.bot_info = Some(BotInfo {
            id: account_id.to_string(),
            username: None,
            display_name: format!("WeChat Bot ({account_id})"),
        });

        info!(account_id, "WeChat bot initialized");

        self.api = Some(api);

        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        self.shutdown_tx = Some(shutdown_tx);

        let api_clone = Arc::clone(self.api.as_ref().expect("api just set"));
        let context_tokens = Arc::clone(&self.context_tokens);
        self.poll_handle = Some(tokio::spawn(poll_loop(
            api_clone,
            callbacks.message_tx,
            shutdown_rx,
            context_tokens,
        )));

        self.status = PluginStatus::Ready;
        Ok(())
    }

    async fn start(&mut self) -> Result<(), ChannelError> {
        self.status = PluginStatus::Starting;
        self.status = PluginStatus::Running;
        info!("WeChat plugin started");
        Ok(())
    }

    async fn stop(&mut self) -> Result<(), ChannelError> {
        self.status = PluginStatus::Stopping;

        if let Some(tx) = self.shutdown_tx.take() {
            let _ = tx.send(true);
        }

        if let Some(handle) = self.poll_handle.take() {
            let _ = tokio::time::timeout(Duration::from_secs(5), handle).await;
        }

        self.api = None;
        self.context_tokens.clear();
        self.status = PluginStatus::Stopped;
        info!("WeChat plugin stopped");
        Ok(())
    }

    async fn send_message(&self, chat_id: &str, message: UnifiedOutgoingMessage) -> Result<String, ChannelError> {
        let api = self
            .api
            .as_ref()
            .ok_or_else(|| ChannelError::PlatformApi("Plugin not initialized".into()))?;

        let text = message.text.as_deref().unwrap_or("").to_string();
        let context_token = self.context_tokens.get(chat_id).map(|v| v.clone());

        api.send_message(chat_id, &text, context_token.as_deref()).await?;
        Ok(String::new())
    }

    /// WeChat does not support editing messages.
    async fn edit_message(
        &self,
        chat_id: &str,
        _message_id: &str,
        message: UnifiedOutgoingMessage,
    ) -> Result<(), ChannelError> {
        let _ = self.send_message(chat_id, message).await?;
        Ok(())
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
        // context_token (from the inbound turn) routes the reply to the right
        // conversation — same requirement as text sends.
        let context_token = self.context_tokens.get(chat_id).map(|v| v.clone());
        let is_image = matches!(media.kind, MediaKind::Image);
        api.send_media(chat_id, media.bytes, &media.filename, is_image, context_token.as_deref())
            .await?;
        Ok(String::new())
    }

    fn active_user_count(&self) -> usize {
        0
    }

    fn bot_info(&self) -> Option<&BotInfo> {
        self.bot_info.as_ref()
    }

    fn plugin_type(&self) -> PluginType {
        PluginType::Weixin
    }

    fn status(&self) -> PluginStatus {
        self.status
    }

    fn last_error(&self) -> Option<&str> {
        self.last_error.as_deref()
    }
}

// ---------------------------------------------------------------------------
// Long-polling loop (buffer-based protocol)
// ---------------------------------------------------------------------------

async fn poll_loop(
    api: Arc<WeixinApi>,
    message_tx: tokio::sync::mpsc::Sender<UnifiedIncomingMessage>,
    mut shutdown_rx: watch::Receiver<bool>,
    context_tokens: Arc<DashMap<String, String>>,
) {
    let mut buf = String::new();
    let mut consecutive_failures: u32 = 0;

    loop {
        if *shutdown_rx.borrow() {
            debug!("WeChat poll loop received shutdown signal");
            break;
        }

        match api.get_updates(&buf).await {
            Ok(resp) => {
                let is_api_error = resp.ret.unwrap_or(0) != 0 || resp.errcode.unwrap_or(0) != 0;

                if is_api_error {
                    consecutive_failures += 1;
                    warn!(
                        ret = resp.ret,
                        errcode = resp.errcode,
                        consecutive_failures,
                        "WeChat getupdates API error"
                    );

                    if consecutive_failures >= WEIXIN_MAX_RETRIES {
                        consecutive_failures = 0;
                        tokio::select! {
                            _ = tokio::time::sleep(WEIXIN_BACKOFF_DELAY) => {}
                            _ = shutdown_rx.changed() => {
                                debug!("WeChat poll loop shutdown during backoff");
                                break;
                            }
                        }
                    } else {
                        tokio::select! {
                            _ = tokio::time::sleep(WEIXIN_RETRY_DELAY) => {}
                            _ = shutdown_rx.changed() => {
                                debug!("WeChat poll loop shutdown during retry");
                                break;
                            }
                        }
                    }
                    continue;
                }

                consecutive_failures = 0;

                if let Some(new_buf) = resp.get_updates_buf {
                    buf = new_buf;
                }

                for msg in resp.msgs.unwrap_or_default() {
                    handle_message(&msg, &message_tx, &context_tokens).await;
                }
            }
            Err(e) => {
                consecutive_failures += 1;
                warn!(error = %e, consecutive_failures, "WeChat poll error");

                if consecutive_failures >= WEIXIN_MAX_RETRIES {
                    consecutive_failures = 0;
                    tokio::select! {
                        _ = tokio::time::sleep(WEIXIN_BACKOFF_DELAY) => {}
                        _ = shutdown_rx.changed() => {
                            debug!("WeChat poll loop shutdown during backoff");
                            break;
                        }
                    }
                } else {
                    tokio::select! {
                        _ = tokio::time::sleep(WEIXIN_RETRY_DELAY) => {}
                        _ = shutdown_rx.changed() => {
                            debug!("WeChat poll loop shutdown during retry");
                            break;
                        }
                    }
                }
            }
        }
    }

    debug!("WeChat poll loop exited");
}

// ---------------------------------------------------------------------------
// Message handling
// ---------------------------------------------------------------------------

async fn handle_message(
    msg: &WeixinRawMessage,
    message_tx: &tokio::sync::mpsc::Sender<UnifiedIncomingMessage>,
    context_tokens: &DashMap<String, String>,
) {
    let from_user_id = match &msg.from_user_id {
        Some(id) if !id.is_empty() => id.clone(),
        _ => return,
    };

    // Store context_token for reply use
    if let Some(ctx) = &msg.context_token
        && !ctx.is_empty()
    {
        context_tokens.insert(from_user_id.clone(), ctx.clone());
    }

    let items = msg.item_list.as_deref().unwrap_or_default();
    let (content_type, text, _has_media) = extract_content(items);

    if text.is_empty() {
        return;
    }

    let display_name = if from_user_id.len() > 6 {
        from_user_id[from_user_id.len() - 6..].to_string()
    } else {
        from_user_id.clone()
    };

    let unified = UnifiedIncomingMessage {
        id: msg.msg_id.clone().unwrap_or_default(),
        platform: PluginType::Weixin,
        chat_id: from_user_id.clone(),
        user: UnifiedUser {
            id: from_user_id,
            username: None,
            display_name,
            avatar_url: None,
        },
        content: UnifiedMessageContent {
            content_type,
            text,
            attachments: None,
        },
        timestamp: chrono_now(),
        reply_to_message_id: None,
        action: None,
        raw: None,
    };

    let _ = message_tx.send(unified).await;
}

/// Extract text content from item_list.
///
/// Returns (content_type, combined_text, has_media_items).
fn extract_content(items: &[WeixinRawItem]) -> (MessageContentType, String, bool) {
    let mut text_parts: Vec<&str> = Vec::new();
    let mut has_media = false;

    for item in items {
        match item.item_type {
            Some(ITEM_TYPE_TEXT) => {
                if let Some(ref ti) = item.text_item
                    && let Some(ref t) = ti.text
                {
                    let trimmed = t.trim();
                    if !trimmed.is_empty() {
                        text_parts.push(trimmed);
                    }
                }
            }
            Some(ITEM_TYPE_VOICE) => {
                if let Some(ref vi) = item.voice_item
                    && let Some(ref t) = vi.text
                {
                    let trimmed = t.trim();
                    if !trimmed.is_empty() {
                        text_parts.push(trimmed);
                    }
                }
            }
            Some(2) | Some(4) => {
                has_media = true;
            }
            _ => {}
        }
    }

    let text = text_parts.join("\n\n");

    let content_type = if text.starts_with('/') {
        MessageContentType::Command
    } else {
        MessageContentType::Text
    };

    (content_type, text, has_media)
}

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
    use crate::types::PluginCredentials;
    use std::collections::HashMap;

    // -- extract_content -------------------------------------------------------

    #[test]
    fn extract_text_only() {
        let items = vec![make_text_item("Hello world")];
        let (ct, text, has_media) = extract_content(&items);
        assert_eq!(ct, MessageContentType::Text);
        assert_eq!(text, "Hello world");
        assert!(!has_media);
    }

    #[test]
    fn extract_command() {
        let items = vec![make_text_item("/start")];
        let (ct, text, _) = extract_content(&items);
        assert_eq!(ct, MessageContentType::Command);
        assert_eq!(text, "/start");
    }

    #[test]
    fn extract_voice_text() {
        let items = vec![WeixinRawItem {
            item_type: Some(ITEM_TYPE_VOICE),
            voice_item: Some(super::super::types::VoiceItem {
                text: Some("transcribed text".into()),
            }),
            ..Default::default()
        }];
        let (ct, text, _) = extract_content(&items);
        assert_eq!(ct, MessageContentType::Text);
        assert_eq!(text, "transcribed text");
    }

    #[test]
    fn extract_mixed_text_and_voice() {
        let items = vec![
            make_text_item("Hello"),
            WeixinRawItem {
                item_type: Some(ITEM_TYPE_VOICE),
                voice_item: Some(super::super::types::VoiceItem {
                    text: Some("voice part".into()),
                }),
                ..Default::default()
            },
        ];
        let (_, text, _) = extract_content(&items);
        assert_eq!(text, "Hello\n\nvoice part");
    }

    #[test]
    fn extract_media_items_detected() {
        let items = vec![WeixinRawItem {
            item_type: Some(2),
            image_item: Some(super::super::types::MediaItemData {
                media: Some(super::super::types::MediaEncryptInfo {
                    encrypt_query_param: Some("enc".into()),
                    aes_key: Some("key".into()),
                }),
                ..Default::default()
            }),
            ..Default::default()
        }];
        let (_, text, has_media) = extract_content(&items);
        assert!(text.is_empty());
        assert!(has_media);
    }

    #[test]
    fn extract_empty_items() {
        let items: Vec<WeixinRawItem> = vec![];
        let (_, text, has_media) = extract_content(&items);
        assert!(text.is_empty());
        assert!(!has_media);
    }

    // -- WeixinPlugin constructor -----------------------------------------------

    #[test]
    fn new_plugin_initial_state() {
        let plugin = WeixinPlugin::new();
        assert_eq!(plugin.status(), PluginStatus::Created);
        assert!(plugin.bot_info().is_none());
        assert!(plugin.last_error().is_none());
        assert_eq!(plugin.plugin_type(), PluginType::Weixin);
        assert_eq!(plugin.active_user_count(), 0);
    }

    // -- initialize validation --------------------------------------------------

    #[tokio::test]
    async fn initialize_missing_bot_token_fails() {
        let mut plugin = WeixinPlugin::new();
        let config = make_config(None, Some("acc_1"));
        let callbacks = make_callbacks();
        let result = plugin.initialize(config, callbacks).await;
        assert!(result.is_err());
        assert_eq!(plugin.status(), PluginStatus::Error);
        assert_eq!(plugin.last_error(), Some("Missing WeChat bot_token"));
    }

    #[tokio::test]
    async fn initialize_missing_account_id_fails() {
        let mut plugin = WeixinPlugin::new();
        let config = make_config(Some("tok_1"), None);
        let callbacks = make_callbacks();
        let result = plugin.initialize(config, callbacks).await;
        assert!(result.is_err());
        assert_eq!(plugin.status(), PluginStatus::Error);
        assert_eq!(plugin.last_error(), Some("Missing WeChat account_id"));
    }

    #[tokio::test]
    async fn initialize_empty_bot_token_fails() {
        let mut plugin = WeixinPlugin::new();
        let config = make_config(Some(""), Some("acc_1"));
        let callbacks = make_callbacks();
        let result = plugin.initialize(config, callbacks).await;
        assert!(result.is_err());
        assert_eq!(plugin.status(), PluginStatus::Error);
    }

    // -- Test helpers -----------------------------------------------------------

    fn make_text_item(text: &str) -> WeixinRawItem {
        WeixinRawItem {
            item_type: Some(ITEM_TYPE_TEXT),
            text_item: Some(super::super::types::TextItem {
                text: Some(text.into()),
            }),
            ..Default::default()
        }
    }

    fn make_config(bot_token: Option<&str>, account_id: Option<&str>) -> PluginConfig {
        PluginConfig {
            credentials: PluginCredentials {
                account_id: account_id.map(String::from),
                bot_token: bot_token.map(String::from),
                ..Default::default()
            },
            config: None,
        }
    }

    fn make_callbacks() -> PluginCallbacks {
        let (message_tx, _) = tokio::sync::mpsc::channel(16);
        let (confirm_tx, _) = tokio::sync::mpsc::channel(16);
        PluginCallbacks { message_tx, confirm_tx }
    }
}

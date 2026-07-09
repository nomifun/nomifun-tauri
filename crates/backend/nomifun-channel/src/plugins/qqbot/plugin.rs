//! QQ Bot channel plugin: `ChannelPlugin` impl + outbound message building.
//!
//! Lifecycle mirrors Discord: initialize validates credentials, start spawns
//! both a gateway WS loop and a background token-refresh task, stop cancels
//! both. Outbound messages are routed by the prefixed chat_id (c2c/group/
//! channel/dm).

use std::sync::Arc;
use std::time::Duration;

use dashmap::DashMap;
use reqwest::Client;
use tokio::sync::{watch, RwLock};
use tokio::task::JoinHandle;
use tracing::info;

use crate::constants::QQBOT_MESSAGE_LIMIT;
use crate::error::ChannelError;
use crate::plugin::{ChannelPlugin, PluginCallbacks, SharedPluginStatus};
use crate::types::{BotInfo, PluginConfig, PluginStatus, PluginType, UnifiedOutgoingMessage};

use super::api::{QqbotApi, SharedToken};
use super::gateway::{
    consume_passive_reply, next_msg_seq, parse_chat_id, run_gateway, run_token_refresh,
    ChatTarget, PassiveReplyMap,
};
use super::types::{MediaRef, SendMediaMessageRequest, SendMessageRequest};

/// QQ Bot plugin: Gateway WS for inbound, REST for outbound, OAuth2 token.
pub struct QqbotPlugin {
    status: SharedPluginStatus,
    bot_info: Option<BotInfo>,
    last_error: Option<String>,
    api: Option<Arc<QqbotApi>>,
    app_id: Option<String>,
    callbacks: Option<PluginCallbacks>,
    gateway_handle: Option<JoinHandle<()>>,
    token_refresh_handle: Option<JoinHandle<()>>,
    shutdown_tx: Option<watch::Sender<bool>>,
    reply_map: PassiveReplyMap,
}

impl Default for QqbotPlugin {
    fn default() -> Self {
        Self {
            status: SharedPluginStatus::default(),
            bot_info: None,
            last_error: None,
            api: None,
            app_id: None,
            callbacks: None,
            gateway_handle: None,
            token_refresh_handle: None,
            shutdown_tx: None,
            reply_map: Arc::new(DashMap::new()),
        }
    }
}

impl QqbotPlugin {
    pub fn new() -> Self {
        Self::default()
    }
}

#[async_trait::async_trait]
impl ChannelPlugin for QqbotPlugin {
    async fn initialize(
        &mut self,
        config: PluginConfig,
        callbacks: PluginCallbacks,
    ) -> Result<(), ChannelError> {
        self.status.set(PluginStatus::Initializing);

        let app_id = config
            .credentials
            .client_id
            .as_deref()
            .filter(|s| !s.is_empty())
            .ok_or_else(|| {
                self.status.set(PluginStatus::Error);
                self.last_error = Some("Missing QQ Bot appId (client_id)".into());
                ChannelError::InvalidConfig("Missing QQ Bot appId (client_id)".into())
            })?
            .to_string();

        let client_secret = config
            .credentials
            .client_secret
            .as_deref()
            .filter(|s| !s.is_empty())
            .ok_or_else(|| {
                self.status.set(PluginStatus::Error);
                self.last_error = Some("Missing QQ Bot clientSecret (client_secret)".into());
                ChannelError::InvalidConfig("Missing QQ Bot clientSecret (client_secret)".into())
            })?
            .to_string();

        let client = Client::builder()
            .timeout(Duration::from_secs(30))
            .build()
            .map_err(|e| {
                self.status.set(PluginStatus::Error);
                self.last_error = Some(format!("HTTP client init failed: {e}"));
                ChannelError::ConnectionFailed(format!("HTTP client init failed: {e}"))
            })?;

        let token_store: SharedToken = Arc::new(RwLock::new(None));
        let api = Arc::new(QqbotApi::new(client, &app_id, &client_secret, token_store));

        // Validate credentials by fetching an initial token.
        let _token = api.refresh_token().await.map_err(|e| {
            self.status.set(PluginStatus::Error);
            self.last_error = Some(format!("Token validation failed: {e}"));
            e
        })?;

        self.bot_info = Some(BotInfo {
            id: app_id.clone(),
            username: None,
            display_name: format!("QQBot:{app_id}"),
        });
        info!(app_id = %app_id, "QQBot plugin initialized");

        self.api = Some(api);
        self.app_id = Some(app_id);
        self.callbacks = Some(callbacks);
        self.status.set(PluginStatus::Ready);
        Ok(())
    }

    async fn start(&mut self) -> Result<(), ChannelError> {
        self.status.set(PluginStatus::Starting);

        if self.gateway_handle.is_some() {
            self.status.set(PluginStatus::Running);
            return Ok(());
        }

        let api = self
            .api
            .as_ref()
            .cloned()
            .ok_or_else(|| ChannelError::PlatformApi("QQBot plugin not initialized".into()))?;
        let callbacks = self
            .callbacks
            .clone()
            .ok_or_else(|| ChannelError::PlatformApi("QQBot callbacks not initialized".into()))?;

        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        self.shutdown_tx = Some(shutdown_tx);

        // Spawn the gateway connection loop.
        let gw_api = api.clone();
        let gw_shutdown = shutdown_rx.clone();
        let reply_map = self.reply_map.clone();
        self.gateway_handle = Some(tokio::spawn(run_gateway(
            gw_api,
            callbacks.message_tx,
            callbacks.confirm_tx,
            self.status.clone(),
            reply_map,
            gw_shutdown,
        )));

        // Spawn the background token refresh task.
        let tr_api = api;
        let tr_shutdown = shutdown_rx;
        self.token_refresh_handle = Some(tokio::spawn(run_token_refresh(tr_api, tr_shutdown)));

        self.status.set(PluginStatus::Running);
        info!("QQBot plugin started");
        Ok(())
    }

    async fn stop(&mut self) -> Result<(), ChannelError> {
        self.status.set(PluginStatus::Stopping);

        if let Some(tx) = self.shutdown_tx.take() {
            let _ = tx.send(true);
        }
        if let Some(handle) = self.gateway_handle.take() {
            let _ = tokio::time::timeout(Duration::from_secs(5), handle).await;
        }
        if let Some(handle) = self.token_refresh_handle.take() {
            let _ = tokio::time::timeout(Duration::from_secs(5), handle).await;
        }

        self.api = None;
        self.callbacks = None;
        self.reply_map.clear();
        self.status.set(PluginStatus::Stopped);
        info!("QQBot plugin stopped");
        Ok(())
    }

    async fn send_message(
        &self,
        chat_id: &str,
        message: UnifiedOutgoingMessage,
    ) -> Result<String, ChannelError> {
        let api = self
            .api
            .as_ref()
            .ok_or_else(|| ChannelError::PlatformApi("Plugin not initialized".into()))?;

        let (target, bare_id) = parse_chat_id(chat_id)
            .ok_or_else(|| ChannelError::MessageSendFailed(format!("Unknown chat_id format: {chat_id}")))?;

        let text = message.text.as_deref().unwrap_or("");
        let content = truncate_message(text, QQBOT_MESSAGE_LIMIT);

        // Try passive reply (with msg_id from most recent inbound).
        let now = tokio::time::Instant::now();
        let passive_msg_id = consume_passive_reply(&self.reply_map, chat_id, now);
        let msg_seq = next_msg_seq();

        let req = SendMessageRequest {
            content,
            msg_type: 0, // text
            msg_seq: Some(msg_seq),
            msg_id: passive_msg_id,
        };

        let resp = match target {
            ChatTarget::C2c => api.send_c2c_message(bare_id, &req).await?,
            ChatTarget::Group => api.send_group_message(bare_id, &req).await?,
            ChatTarget::Channel => api.send_channel_message(bare_id, &req).await?,
            ChatTarget::Dm => api.send_dm_message(bare_id, &req).await?,
        };

        Ok(resp.id.unwrap_or_default())
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

        let (target, bare_id) = parse_chat_id(chat_id)
            .ok_or_else(|| ChannelError::MessageSendFailed(format!("Unknown chat_id format: {chat_id}")))?;

        // QQ media type: 1 = image, 4 = file.
        let file_type = match media.kind {
            MediaKind::Image => 1,
            MediaKind::File => 4,
        };

        // Passive reply + sequence, mirroring send_message.
        let now = tokio::time::Instant::now();
        let passive_msg_id = consume_passive_reply(&self.reply_map, chat_id, now);
        let content = caption.unwrap_or("").to_string();

        let resp = match target {
            // C2C / group: upload the bytes first, then send a msg_type 7
            // (rich media) message referencing the returned file_info handle.
            ChatTarget::C2c => {
                let file_info = api.upload_c2c_file(bare_id, file_type, &media.bytes).await?;
                let req = SendMediaMessageRequest {
                    content,
                    msg_type: 7,
                    media: MediaRef { file_info },
                    msg_seq: Some(next_msg_seq()),
                    msg_id: passive_msg_id,
                };
                api.send_c2c_media(bare_id, &req).await?
            }
            ChatTarget::Group => {
                let file_info = api.upload_group_file(bare_id, file_type, &media.bytes).await?;
                let req = SendMediaMessageRequest {
                    content,
                    msg_type: 7,
                    media: MediaRef { file_info },
                    msg_seq: Some(next_msg_seq()),
                    msg_id: passive_msg_id,
                };
                api.send_group_media(bare_id, &req).await?
            }
            // Guild channel / DM: single multipart request (file_image).
            ChatTarget::Channel => {
                api.send_channel_media(
                    bare_id,
                    media.bytes,
                    &media.filename,
                    &media.mime,
                    caption,
                    passive_msg_id.as_deref(),
                )
                .await?
            }
            ChatTarget::Dm => {
                api.send_dm_media(
                    bare_id,
                    media.bytes,
                    &media.filename,
                    &media.mime,
                    caption,
                    passive_msg_id.as_deref(),
                )
                .await?
            }
        };

        Ok(resp.id.unwrap_or_default())
    }

    async fn edit_message(
        &self,
        chat_id: &str,
        _message_id: &str,
        message: UnifiedOutgoingMessage,
    ) -> Result<(), ChannelError> {
        // QQ has no message edit API; degrade to sending a new message.
        // This is already classified as send-once in stream_relay.
        let _ = self.send_message(chat_id, message).await?;
        Ok(())
    }

    fn active_user_count(&self) -> usize {
        0
    }

    fn bot_info(&self) -> Option<&BotInfo> {
        self.bot_info.as_ref()
    }

    fn plugin_type(&self) -> PluginType {
        PluginType::Qqbot
    }

    fn status(&self) -> PluginStatus {
        self.status.get()
    }

    fn last_error(&self) -> Option<&str> {
        self.last_error.as_deref()
    }
}

// ---------------------------------------------------------------------------
// Outbound helpers (pure, unit-tested)
// ---------------------------------------------------------------------------

/// Truncate text at a char boundary to `limit`, appending "..." if cut.
fn truncate_message(text: &str, limit: usize) -> String {
    if text.chars().count() <= limit {
        return text.to_string();
    }
    let truncated: String = text.chars().take(limit.saturating_sub(3)).collect();
    format!("{truncated}...")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::OutgoingMessageType;

    #[test]
    fn new_plugin_initial_state() {
        let p = QqbotPlugin::new();
        assert_eq!(p.status(), PluginStatus::Created);
        assert!(p.bot_info().is_none());
        assert_eq!(p.plugin_type(), PluginType::Qqbot);
        assert_eq!(p.active_user_count(), 0);
    }

    #[test]
    fn truncate_respects_limit() {
        assert_eq!(truncate_message("short", 4000), "short");
        let long = "a".repeat(4100);
        let out = truncate_message(&long, 4000);
        assert_eq!(out.chars().count(), 4000);
        assert!(out.ends_with("..."));
    }

    #[test]
    fn truncate_unicode_boundary() {
        let text = "你好世界测试";
        assert_eq!(truncate_message(text, 4), "你...");
    }

    #[test]
    fn truncate_empty() {
        assert_eq!(truncate_message("", 4000), "");
    }

    fn _make_msg(text: &str) -> UnifiedOutgoingMessage {
        UnifiedOutgoingMessage {
            message_type: OutgoingMessageType::Text,
            text: Some(text.into()),
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
}

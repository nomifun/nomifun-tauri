//! Discord channel plugin: `ChannelPlugin` impl + outbound message building.
//!
//! Inbound runs through the gateway loop in `gateway.rs`; this file owns the
//! plugin lifecycle (mirrors `telegram`) and the REST outbound path.

use std::sync::Arc;
use std::time::Duration;

use reqwest::Client;
use tokio::sync::watch;
use tokio::task::JoinHandle;
use tracing::info;

use crate::constants::DISCORD_MESSAGE_LIMIT;
use crate::error::ChannelError;
use crate::plugin::{ChannelPlugin, PluginCallbacks, SharedPluginStatus};
use crate::plugins::callback::format_callback_data;
use crate::types::{BotInfo, PluginConfig, PluginStatus, PluginType, UnifiedOutgoingMessage};

use super::api::DiscordApi;
use super::gateway::run_gateway;
use super::types::{
    ActionRow, ButtonComponent, CreateMessageRequest, RestMessageReference, BUTTON_STYLE_SECONDARY,
    COMPONENT_ACTION_ROW, COMPONENT_BUTTON,
};

/// Discord component custom_id hard limit.
const CUSTOM_ID_LIMIT: usize = 100;

/// Discord bot plugin: Gateway WS for inbound, REST for outbound.
pub struct DiscordPlugin {
    status: SharedPluginStatus,
    bot_info: Option<BotInfo>,
    last_error: Option<String>,
    api: Option<Arc<DiscordApi>>,
    token: Option<String>,
    callbacks: Option<PluginCallbacks>,
    gateway_handle: Option<JoinHandle<()>>,
    shutdown_tx: Option<watch::Sender<bool>>,
}

impl Default for DiscordPlugin {
    fn default() -> Self {
        Self {
            status: SharedPluginStatus::default(),
            bot_info: None,
            last_error: None,
            api: None,
            token: None,
            callbacks: None,
            gateway_handle: None,
            shutdown_tx: None,
        }
    }
}

impl DiscordPlugin {
    pub fn new() -> Self {
        Self::default()
    }
}

#[async_trait::async_trait]
impl ChannelPlugin for DiscordPlugin {
    async fn initialize(&mut self, config: PluginConfig, callbacks: PluginCallbacks) -> Result<(), ChannelError> {
        self.status.set(PluginStatus::Initializing);

        let token = config
            .credentials
            .token
            .as_deref()
            .filter(|t| !t.is_empty())
            .ok_or_else(|| {
                self.status.set(PluginStatus::Error);
                self.last_error = Some("Missing Discord bot token".into());
                ChannelError::InvalidConfig("Missing Discord bot token".into())
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

        let api = Arc::new(DiscordApi::new(client, &token));

        // Validate the token and learn our own bot id (needed for the inbound
        // self-loop guard and guild mention gating).
        let me = api.get_me().await.map_err(|e| {
            self.status.set(PluginStatus::Error);
            self.last_error = Some(format!("Token validation failed: {e}"));
            e
        })?;

        let display_name = me.global_name.clone().filter(|s| !s.is_empty()).unwrap_or_else(|| me.username.clone());
        self.bot_info = Some(BotInfo {
            id: me.id.clone(),
            username: Some(me.username.clone()),
            display_name,
        });
        info!(bot_id = %me.id, bot_username = %me.username, "Discord bot initialized");

        self.api = Some(api);
        self.token = Some(token);
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
            .ok_or_else(|| ChannelError::PlatformApi("Discord plugin not initialized".into()))?;
        let token = self
            .token
            .clone()
            .ok_or_else(|| ChannelError::PlatformApi("Discord token not initialized".into()))?;
        let callbacks = self
            .callbacks
            .clone()
            .ok_or_else(|| ChannelError::PlatformApi("Discord callbacks not initialized".into()))?;
        let self_bot_id = self.bot_info.as_ref().map(|b| b.id.clone()).unwrap_or_default();

        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        self.shutdown_tx = Some(shutdown_tx);
        self.gateway_handle = Some(tokio::spawn(run_gateway(
            api,
            token,
            self_bot_id,
            callbacks.message_tx,
            callbacks.confirm_tx,
            self.status.clone(),
            shutdown_rx,
        )));

        self.status.set(PluginStatus::Running);
        info!("Discord plugin started");
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

        self.api = None;
        self.callbacks = None;
        self.status.set(PluginStatus::Stopped);
        info!("Discord plugin stopped");
        Ok(())
    }

    async fn send_message(&self, chat_id: &str, message: UnifiedOutgoingMessage) -> Result<String, ChannelError> {
        let api = self
            .api
            .as_ref()
            .ok_or_else(|| ChannelError::PlatformApi("Plugin not initialized".into()))?;
        let req = build_create_message_request(&message);
        let sent = api.create_message(chat_id, &req).await?;
        Ok(sent.id)
    }

    async fn send_media(&self, chat_id: &str, media: crate::types::OutgoingMedia, caption: Option<&str>) -> Result<String, ChannelError> {
        // Discord: images and files both go through the message-attachment path.
        let api = self
            .api
            .as_ref()
            .ok_or_else(|| ChannelError::PlatformApi("Plugin not initialized".into()))?;
        let sent = api
            .create_message_with_attachment(chat_id, media.bytes, &media.filename, &media.mime, caption)
            .await?;
        Ok(sent.id)
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
        // Edits never carry a reply reference.
        let mut req = build_create_message_request(&message);
        req.message_reference = None;
        api.edit_message(chat_id, message_id, &req).await
    }

    fn active_user_count(&self) -> usize {
        0
    }

    fn bot_info(&self) -> Option<&BotInfo> {
        self.bot_info.as_ref()
    }

    fn plugin_type(&self) -> PluginType {
        PluginType::Discord
    }

    fn status(&self) -> PluginStatus {
        self.status.get()
    }

    fn last_error(&self) -> Option<&str> {
        self.last_error.as_deref()
    }
}

// ---------------------------------------------------------------------------
// Outbound building (pure, unit-tested)
// ---------------------------------------------------------------------------

/// Build the Discord REST create/edit body from a unified outgoing message:
/// truncated content, optional reply reference, and button components.
fn build_create_message_request(message: &UnifiedOutgoingMessage) -> CreateMessageRequest {
    let text = message.text.as_deref().unwrap_or("");
    let content = if text.is_empty() {
        None
    } else {
        Some(truncate_message(text, DISCORD_MESSAGE_LIMIT))
    };
    let message_reference = message.reply_to_message_id.as_ref().map(|id| RestMessageReference {
        message_id: id.clone(),
        fail_if_not_exists: false,
    });
    let components = build_components(message);
    CreateMessageRequest {
        content,
        message_reference,
        components,
    }
}

/// Map unified buttons (rows × cols) to Discord action-row components.
/// Discord allows at most 5 action rows and 5 buttons per row.
fn build_components(message: &UnifiedOutgoingMessage) -> Option<Vec<ActionRow>> {
    let buttons = message.buttons.as_ref()?;
    let rows: Vec<ActionRow> = buttons
        .iter()
        .take(5)
        .filter_map(|row| {
            let comps: Vec<ButtonComponent> = row
                .iter()
                .take(5)
                .map(|btn| ButtonComponent {
                    component_type: COMPONENT_BUTTON,
                    style: BUTTON_STYLE_SECONDARY,
                    label: btn.label.clone(),
                    custom_id: truncate_custom_id(&format_callback_data(btn)),
                })
                .collect();
            if comps.is_empty() {
                None
            } else {
                Some(ActionRow {
                    component_type: COMPONENT_ACTION_ROW,
                    components: comps,
                })
            }
        })
        .collect();
    if rows.is_empty() { None } else { Some(rows) }
}

/// Truncate text at a char boundary to `limit`, appending "..." if cut.
fn truncate_message(text: &str, limit: usize) -> String {
    if text.chars().count() <= limit {
        return text.to_string();
    }
    let truncated: String = text.chars().take(limit.saturating_sub(3)).collect();
    format!("{truncated}...")
}

/// Discord custom_id is capped at 100 chars; truncate defensively.
fn truncate_custom_id(id: &str) -> String {
    if id.len() <= CUSTOM_ID_LIMIT {
        id.to_string()
    } else {
        id.chars().take(CUSTOM_ID_LIMIT).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{ActionButton, OutgoingMessageType};

    fn msg(text: &str) -> UnifiedOutgoingMessage {
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

    #[test]
    fn new_plugin_initial_state() {
        let p = DiscordPlugin::new();
        assert_eq!(p.status(), PluginStatus::Created);
        assert!(p.bot_info().is_none());
        assert_eq!(p.plugin_type(), PluginType::Discord);
        assert_eq!(p.active_user_count(), 0);
    }

    #[test]
    fn build_request_plain_text() {
        let req = build_create_message_request(&msg("hello"));
        assert_eq!(req.content.as_deref(), Some("hello"));
        assert!(req.message_reference.is_none());
        assert!(req.components.is_none());
    }

    #[test]
    fn build_request_empty_text_is_none() {
        let req = build_create_message_request(&msg(""));
        assert!(req.content.is_none());
    }

    #[test]
    fn build_request_with_reply() {
        let mut m = msg("re");
        m.reply_to_message_id = Some("orig123".into());
        let req = build_create_message_request(&m);
        let r = req.message_reference.unwrap();
        assert_eq!(r.message_id, "orig123");
        assert!(!r.fail_if_not_exists);
    }

    #[test]
    fn build_request_with_buttons() {
        let mut m = msg("choose");
        m.buttons = Some(vec![vec![
            ActionButton {
                label: "Continue".into(),
                action: "chat.continue".into(),
                params: None,
            },
            ActionButton {
                label: "Regenerate".into(),
                action: "chat.regenerate".into(),
                params: None,
            },
        ]]);
        let req = build_create_message_request(&m);
        let comps = req.components.unwrap();
        assert_eq!(comps.len(), 1);
        assert_eq!(comps[0].component_type, COMPONENT_ACTION_ROW);
        assert_eq!(comps[0].components.len(), 2);
        assert_eq!(comps[0].components[0].component_type, COMPONENT_BUTTON);
        assert_eq!(comps[0].components[0].label, "Continue");
        assert_eq!(comps[0].components[0].custom_id, "chat:chat.continue");
    }

    #[test]
    fn components_capped_at_five_rows_and_buttons() {
        let mut m = msg("many");
        let big_row: Vec<ActionButton> = (0..8)
            .map(|i| ActionButton {
                label: format!("b{i}"),
                action: format!("chat.a{i}"),
                params: None,
            })
            .collect();
        m.buttons = Some(vec![big_row; 8]);
        let comps = build_create_message_request(&m).components.unwrap();
        assert_eq!(comps.len(), 5); // max 5 rows
        assert_eq!(comps[0].components.len(), 5); // max 5 buttons/row
    }

    #[test]
    fn truncate_respects_limit() {
        assert_eq!(truncate_message("short", 2000), "short");
        let long = "a".repeat(2100);
        let out = truncate_message(&long, 2000);
        assert_eq!(out.chars().count(), 2000);
        assert!(out.ends_with("..."));
    }

    #[test]
    fn truncate_unicode_boundary() {
        let text = "你好世界测试";
        assert_eq!(truncate_message(text, 4), "你...");
    }

    #[test]
    fn custom_id_is_capped() {
        let long = "x".repeat(200);
        assert_eq!(truncate_custom_id(&long).len(), CUSTOM_ID_LIMIT);
        assert_eq!(truncate_custom_id("chat:chat.send"), "chat:chat.send");
    }
}

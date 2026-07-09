//! REST client for the Discord HTTP API (v10).

use reqwest::Client;
use tracing::debug;

use crate::error::ChannelError;

use super::types::{
    CreateMessageRequest, CreateMessageResponse, GetMeResponse, InteractionCallbackBody,
};

const DISCORD_API_BASE: &str = "https://discord.com/api/v10";

/// HTTP client for the Discord REST API. Authenticates with a bot token via the
/// `Authorization: Bot <token>` header.
pub(crate) struct DiscordApi {
    client: Client,
    token: String,
}

impl DiscordApi {
    pub fn new(client: Client, token: &str) -> Self {
        Self {
            client,
            token: token.to_string(),
        }
    }

    fn auth_header(&self) -> String {
        format!("Bot {}", self.token)
    }

    /// `GET /users/@me` — validates the token and returns the bot identity.
    pub async fn get_me(&self) -> Result<GetMeResponse, ChannelError> {
        let url = format!("{DISCORD_API_BASE}/users/@me");
        let resp = self
            .client
            .get(&url)
            .header("Authorization", self.auth_header())
            .send()
            .await
            .map_err(|e| ChannelError::PlatformApi(format!("Discord get_me request failed: {e}")))?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(ChannelError::ConnectionFailed(format!(
                "Discord get_me failed: HTTP {status}: {body}"
            )));
        }

        resp.json()
            .await
            .map_err(|e| ChannelError::PlatformApi(format!("Discord get_me parse failed: {e}")))
    }

    /// `POST /channels/{channel_id}/messages` — send a message; returns its id.
    pub async fn create_message(
        &self,
        channel_id: &str,
        req: &CreateMessageRequest,
    ) -> Result<CreateMessageResponse, ChannelError> {
        let url = format!("{DISCORD_API_BASE}/channels/{channel_id}/messages");
        debug!(channel_id, "Sending Discord message");

        let resp = self
            .client
            .post(&url)
            .header("Authorization", self.auth_header())
            .json(req)
            .send()
            .await
            .map_err(|e| ChannelError::MessageSendFailed(format!("Discord create_message request failed: {e}")))?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(ChannelError::MessageSendFailed(format!(
                "Discord create_message failed: HTTP {status}: {body}"
            )));
        }

        resp.json()
            .await
            .map_err(|e| ChannelError::MessageSendFailed(format!("Discord create_message parse failed: {e}")))
    }

    /// `POST /channels/{channel_id}/messages` (multipart) — upload raw bytes as
    /// a message attachment; returns the created message's id. Images and files
    /// share this mechanism, so `caption` becomes the message content.
    pub async fn create_message_with_attachment(
        &self,
        channel_id: &str,
        bytes: Vec<u8>,
        filename: &str,
        mime: &str,
        caption: Option<&str>,
    ) -> Result<CreateMessageResponse, ChannelError> {
        let url = format!("{DISCORD_API_BASE}/channels/{channel_id}/messages");
        debug!(channel_id, bytes = bytes.len(), "Uploading Discord attachment");

        // `payload_json` carries the message content plus attachment metadata
        // that references the uploaded file part by index (files[0] → id 0).
        let payload = serde_json::json!({
            "content": caption.unwrap_or(""),
            "attachments": [{ "id": 0, "filename": filename }],
        });

        let part = reqwest::multipart::Part::bytes(bytes)
            .file_name(filename.to_owned())
            .mime_str(mime)
            .map_err(|e| ChannelError::MessageSendFailed(format!("invalid attachment mime {mime}: {e}")))?;

        let form = reqwest::multipart::Form::new()
            .text("payload_json", payload.to_string())
            .part("files[0]", part);

        let resp = self
            .client
            .post(&url)
            .header("Authorization", self.auth_header())
            .multipart(form)
            .send()
            .await
            .map_err(|e| ChannelError::MessageSendFailed(format!("Discord attachment request failed: {e}")))?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(ChannelError::MessageSendFailed(format!(
                "Discord attachment upload failed: HTTP {status}: {body}"
            )));
        }

        resp.json()
            .await
            .map_err(|e| ChannelError::MessageSendFailed(format!("Discord attachment parse failed: {e}")))
    }

    /// `PATCH /channels/{channel_id}/messages/{message_id}` — edit a message.
    pub async fn edit_message(
        &self,
        channel_id: &str,
        message_id: &str,
        req: &CreateMessageRequest,
    ) -> Result<(), ChannelError> {
        let url = format!("{DISCORD_API_BASE}/channels/{channel_id}/messages/{message_id}");
        debug!(channel_id, message_id, "Editing Discord message");

        let resp = self
            .client
            .patch(&url)
            .header("Authorization", self.auth_header())
            .json(req)
            .send()
            .await
            .map_err(|e| ChannelError::MessageSendFailed(format!("Discord edit_message request failed: {e}")))?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(ChannelError::MessageSendFailed(format!(
                "Discord edit_message failed: HTTP {status}: {body}"
            )));
        }
        Ok(())
    }

    /// `POST /interactions/{id}/{token}/callback` — acknowledge a component
    /// (button) interaction. Uses the per-interaction token, not the bot token.
    pub async fn ack_interaction(
        &self,
        interaction_id: &str,
        interaction_token: &str,
        callback_type: u8,
    ) -> Result<(), ChannelError> {
        let url = format!("{DISCORD_API_BASE}/interactions/{interaction_id}/{interaction_token}/callback");
        let body = InteractionCallbackBody { callback_type };
        let resp = self
            .client
            .post(&url)
            .json(&body)
            .send()
            .await
            .map_err(|e| ChannelError::PlatformApi(format!("Discord ack_interaction request failed: {e}")))?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            // Non-fatal: log via the error string; the caller ignores ack errors.
            return Err(ChannelError::PlatformApi(format!(
                "Discord ack_interaction failed: HTTP {status}: {body}"
            )));
        }
        Ok(())
    }
}

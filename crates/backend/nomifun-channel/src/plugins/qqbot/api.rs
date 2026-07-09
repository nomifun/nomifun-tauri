//! REST client for the QQ Bot HTTP API + OAuth2 token management.
//!
//! Token lifecycle: `POST https://bots.qq.com/app/getAppAccessToken` returns a
//! time-limited `access_token`.  A background task refreshes it ~5 min before
//! expiry.  All REST calls use `Authorization: QQBot <access_token>`.

use std::sync::Arc;
use std::time::Duration;

use reqwest::Client;
use tokio::sync::RwLock;
use tracing::{debug, warn};

use crate::error::ChannelError;

use super::types::{
    AppAccessTokenRequest, AppAccessTokenResponse, CachedToken, FileUploadRequest,
    FileUploadResponse, GatewayUrlResponse, InteractionCallbackBody, SendMediaMessageRequest,
    SendMessageRequest, SendMessageResponse,
};

const TOKEN_URL: &str = "https://bots.qq.com/app/getAppAccessToken";
const API_BASE: &str = "https://api.sgroup.qq.com";

/// Shared access-token store. The background refresh task writes; API calls read.
pub(crate) type SharedToken = Arc<RwLock<Option<CachedToken>>>;

/// REST client for the QQ Bot API.
pub(crate) struct QqbotApi {
    client: Client,
    app_id: String,
    client_secret: String,
    token_store: SharedToken,
}

impl QqbotApi {
    pub fn new(client: Client, app_id: &str, client_secret: &str, token_store: SharedToken) -> Self {
        Self {
            client,
            app_id: app_id.to_string(),
            client_secret: client_secret.to_string(),
            token_store,
        }
    }

    // -- Token management ---------------------------------------------------

    /// Fetch a fresh access token from the QQ Bot OAuth2 endpoint and store it.
    pub async fn refresh_token(&self) -> Result<String, ChannelError> {
        let body = AppAccessTokenRequest {
            app_id: self.app_id.clone(),
            client_secret: self.client_secret.clone(),
        };

        let resp = self
            .client
            .post(TOKEN_URL)
            .json(&body)
            .send()
            .await
            .map_err(|e| ChannelError::ConnectionFailed(format!("QQBot token request failed: {e}")))?;

        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            return Err(ChannelError::ConnectionFailed(format!(
                "QQBot token request failed: HTTP {status}: {text}"
            )));
        }

        let token_resp: AppAccessTokenResponse = resp
            .json()
            .await
            .map_err(|e| ChannelError::ConnectionFailed(format!("QQBot token parse failed: {e}")))?;

        let cached = CachedToken {
            access_token: token_resp.access_token.clone(),
            expires_at: tokio::time::Instant::now() + Duration::from_secs(token_resp.expires_in),
        };

        *self.token_store.write().await = Some(cached);
        debug!(expires_in = token_resp.expires_in, "QQBot access token refreshed");

        Ok(token_resp.access_token)
    }

    /// Get the current access token, refreshing if expired or absent.
    pub async fn get_token(&self) -> Result<String, ChannelError> {
        {
            let guard = self.token_store.read().await;
            if let Some(cached) = guard.as_ref() {
                if tokio::time::Instant::now() < cached.expires_at {
                    return Ok(cached.access_token.clone());
                }
            }
        }
        // Token expired or missing — refresh.
        self.refresh_token().await
    }

    /// Clear the cached token (e.g. on 401).
    pub async fn clear_token(&self) {
        *self.token_store.write().await = None;
    }

    fn auth_header(token: &str) -> String {
        format!("QQBot {token}")
    }

    // -- Gateway URL --------------------------------------------------------

    /// `GET /gateway` — obtain the WebSocket gateway URL.
    pub async fn get_gateway_url(&self) -> Result<String, ChannelError> {
        let token = self.get_token().await?;
        let url = format!("{API_BASE}/gateway");
        let resp = self
            .client
            .get(&url)
            .header("Authorization", Self::auth_header(&token))
            .send()
            .await
            .map_err(|e| ChannelError::ConnectionFailed(format!("QQBot gateway URL request failed: {e}")))?;

        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            return Err(ChannelError::ConnectionFailed(format!(
                "QQBot gateway URL failed: HTTP {status}: {text}"
            )));
        }

        let gw: GatewayUrlResponse = resp
            .json()
            .await
            .map_err(|e| ChannelError::ConnectionFailed(format!("QQBot gateway URL parse failed: {e}")))?;

        Ok(gw.url)
    }

    // -- Message sending ----------------------------------------------------

    /// Send a message to a C2C user.
    pub async fn send_c2c_message(
        &self,
        user_openid: &str,
        req: &SendMessageRequest,
    ) -> Result<SendMessageResponse, ChannelError> {
        let url = format!("{API_BASE}/v2/users/{user_openid}/messages");
        self.post_message(&url, req).await
    }

    /// Send a message to a group.
    pub async fn send_group_message(
        &self,
        group_openid: &str,
        req: &SendMessageRequest,
    ) -> Result<SendMessageResponse, ChannelError> {
        let url = format!("{API_BASE}/v2/groups/{group_openid}/messages");
        self.post_message(&url, req).await
    }

    /// Send a message to a guild channel.
    pub async fn send_channel_message(
        &self,
        channel_id: &str,
        req: &SendMessageRequest,
    ) -> Result<SendMessageResponse, ChannelError> {
        let url = format!("{API_BASE}/channels/{channel_id}/messages");
        self.post_message(&url, req).await
    }

    /// Send a direct message in a guild.
    pub async fn send_dm_message(
        &self,
        guild_id: &str,
        req: &SendMessageRequest,
    ) -> Result<SendMessageResponse, ChannelError> {
        let url = format!("{API_BASE}/dms/{guild_id}/messages");
        self.post_message(&url, req).await
    }

    // -- Rich media ---------------------------------------------------------

    /// Upload raw media bytes to a C2C user's file store; returns the
    /// `file_info` handle to reference from a msg_type 7 message.
    pub async fn upload_c2c_file(
        &self,
        user_openid: &str,
        file_type: u32,
        bytes: &[u8],
    ) -> Result<String, ChannelError> {
        let url = format!("{API_BASE}/v2/users/{user_openid}/files");
        self.upload_file(&url, file_type, bytes).await
    }

    /// Upload raw media bytes to a group's file store; returns the
    /// `file_info` handle to reference from a msg_type 7 message.
    pub async fn upload_group_file(
        &self,
        group_openid: &str,
        file_type: u32,
        bytes: &[u8],
    ) -> Result<String, ChannelError> {
        let url = format!("{API_BASE}/v2/groups/{group_openid}/files");
        self.upload_file(&url, file_type, bytes).await
    }

    /// Send a rich-media (msg_type 7) message to a C2C user.
    pub async fn send_c2c_media(
        &self,
        user_openid: &str,
        req: &SendMediaMessageRequest,
    ) -> Result<SendMessageResponse, ChannelError> {
        let url = format!("{API_BASE}/v2/users/{user_openid}/messages");
        self.post_message(&url, req).await
    }

    /// Send a rich-media (msg_type 7) message to a group.
    pub async fn send_group_media(
        &self,
        group_openid: &str,
        req: &SendMediaMessageRequest,
    ) -> Result<SendMessageResponse, ChannelError> {
        let url = format!("{API_BASE}/v2/groups/{group_openid}/messages");
        self.post_message(&url, req).await
    }

    /// Send an image to a guild channel via multipart (`file_image`).
    /// The guild API uploads and sends in a single request.
    pub async fn send_channel_media(
        &self,
        channel_id: &str,
        bytes: Vec<u8>,
        filename: &str,
        mime: &str,
        content: Option<&str>,
        msg_id: Option<&str>,
    ) -> Result<SendMessageResponse, ChannelError> {
        let url = format!("{API_BASE}/channels/{channel_id}/messages");
        self.post_multipart_image(&url, bytes, filename, mime, content, msg_id).await
    }

    /// Send an image in a guild direct message via multipart (`file_image`).
    pub async fn send_dm_media(
        &self,
        guild_id: &str,
        bytes: Vec<u8>,
        filename: &str,
        mime: &str,
        content: Option<&str>,
        msg_id: Option<&str>,
    ) -> Result<SendMessageResponse, ChannelError> {
        let url = format!("{API_BASE}/dms/{guild_id}/messages");
        self.post_multipart_image(&url, bytes, filename, mime, content, msg_id).await
    }

    /// ACK an interaction callback.
    pub async fn ack_interaction(&self, interaction_id: &str) -> Result<(), ChannelError> {
        let token = self.get_token().await?;
        let url = format!("{API_BASE}/interactions/{interaction_id}");
        let body = InteractionCallbackBody { code: 0 };
        let resp = self
            .client
            .put(&url)
            .header("Authorization", Self::auth_header(&token))
            .json(&body)
            .send()
            .await
            .map_err(|e| ChannelError::PlatformApi(format!("QQBot ack_interaction request failed: {e}")))?;

        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            warn!(status = %status, "QQBot ack_interaction failed: {text}");
        }
        Ok(())
    }

    // -- Internal -----------------------------------------------------------

    /// Upload rich-media bytes as base64 JSON, returning the `file_info` handle.
    /// Shared by the C2C and group upload endpoints.
    async fn upload_file(
        &self,
        url: &str,
        file_type: u32,
        bytes: &[u8],
    ) -> Result<String, ChannelError> {
        use base64::Engine;

        let token = self.get_token().await?;
        let file_data = base64::engine::general_purpose::STANDARD.encode(bytes);
        let body = FileUploadRequest {
            file_type,
            file_data,
            srv_send_msg: false,
        };
        debug!(url, bytes = bytes.len(), file_type, "QQBot uploading rich media");

        let resp = self
            .client
            .post(url)
            .header("Authorization", Self::auth_header(&token))
            .json(&body)
            .send()
            .await
            .map_err(|e| ChannelError::MessageSendFailed(format!("QQBot file upload request failed: {e}")))?;

        let status = resp.status();
        if status.as_u16() == 401 {
            self.clear_token().await;
            let text = resp.text().await.unwrap_or_default();
            return Err(ChannelError::MessageSendFailed(format!(
                "QQBot file upload auth failed (token cleared): HTTP 401: {text}"
            )));
        }
        if !status.is_success() {
            let text = resp.text().await.unwrap_or_default();
            return Err(ChannelError::MessageSendFailed(format!(
                "QQBot file upload failed: HTTP {status}: {text}"
            )));
        }

        let parsed: FileUploadResponse = resp
            .json()
            .await
            .map_err(|e| ChannelError::MessageSendFailed(format!("QQBot file upload parse failed: {e}")))?;
        Ok(parsed.file_info)
    }

    /// POST a multipart `file_image` message to a guild channel / DM endpoint.
    async fn post_multipart_image(
        &self,
        url: &str,
        bytes: Vec<u8>,
        filename: &str,
        mime: &str,
        content: Option<&str>,
        msg_id: Option<&str>,
    ) -> Result<SendMessageResponse, ChannelError> {
        let token = self.get_token().await?;
        debug!(url, bytes = bytes.len(), "QQBot sending guild media");

        let part = reqwest::multipart::Part::bytes(bytes)
            .file_name(filename.to_owned())
            .mime_str(mime)
            .map_err(|e| ChannelError::MessageSendFailed(format!("QQBot invalid media mime {mime}: {e}")))?;

        let mut form = reqwest::multipart::Form::new().part("file_image", part);
        if let Some(c) = content.filter(|c| !c.is_empty()) {
            form = form.text("content", c.to_owned());
        }
        if let Some(m) = msg_id {
            form = form.text("msg_id", m.to_owned());
        }

        let resp = self
            .client
            .post(url)
            .header("Authorization", Self::auth_header(&token))
            .multipart(form)
            .send()
            .await
            .map_err(|e| ChannelError::MessageSendFailed(format!("QQBot media send request failed: {e}")))?;

        let status = resp.status();
        if status.as_u16() == 401 {
            self.clear_token().await;
            let text = resp.text().await.unwrap_or_default();
            return Err(ChannelError::MessageSendFailed(format!(
                "QQBot media send auth failed (token cleared): HTTP 401: {text}"
            )));
        }
        if !status.is_success() {
            let text = resp.text().await.unwrap_or_default();
            return Err(ChannelError::MessageSendFailed(format!(
                "QQBot media send failed: HTTP {status}: {text}"
            )));
        }

        resp.json()
            .await
            .map_err(|e| ChannelError::MessageSendFailed(format!("QQBot media send parse failed: {e}")))
    }

    async fn post_message<T: serde::Serialize>(
        &self,
        url: &str,
        req: &T,
    ) -> Result<SendMessageResponse, ChannelError> {
        let token = self.get_token().await?;
        debug!(url, "QQBot sending message");

        let resp = self
            .client
            .post(url)
            .header("Authorization", Self::auth_header(&token))
            .json(req)
            .send()
            .await
            .map_err(|e| ChannelError::MessageSendFailed(format!("QQBot send_message request failed: {e}")))?;

        let status = resp.status();

        // On 401, clear token so it gets refreshed on next call.
        if status.as_u16() == 401 {
            self.clear_token().await;
            let text = resp.text().await.unwrap_or_default();
            return Err(ChannelError::MessageSendFailed(format!(
                "QQBot send_message auth failed (token cleared): HTTP 401: {text}"
            )));
        }

        if !status.is_success() {
            let text = resp.text().await.unwrap_or_default();
            return Err(ChannelError::MessageSendFailed(format!(
                "QQBot send_message failed: HTTP {status}: {text}"
            )));
        }

        resp.json()
            .await
            .map_err(|e| ChannelError::MessageSendFailed(format!("QQBot send_message parse failed: {e}")))
    }
}

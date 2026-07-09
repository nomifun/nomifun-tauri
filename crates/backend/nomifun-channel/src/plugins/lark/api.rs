use std::sync::Arc;
use std::time::{Duration, Instant};

use reqwest::Client;
use tokio::sync::RwLock;
use tracing::{debug, warn};

use crate::error::ChannelError;

use super::types::{
    BotInfoData, BotInfoResponse, FileUploadResponse, GenericResponse, ImageUploadResponse, SendCardRequest,
    SendMessageData, SendMessageResponse, TenantAccessTokenRequest, TenantAccessTokenResponse, UpdateCardRequest,
    WsEndpointData, WsEndpointRequest, WsEndpointResponse,
};

const LARK_OPEN_API_BASE: &str = "https://open.feishu.cn/open-apis";
const LARK_BASE: &str = "https://open.feishu.cn";

/// Token refresh margin — refresh 5 minutes before expiry.
const TOKEN_REFRESH_MARGIN: Duration = Duration::from_secs(5 * 60);

/// Cached tenant access token with expiry tracking.
struct TokenCache {
    token: String,
    acquired_at: Instant,
    expires_in: Duration,
}

impl TokenCache {
    fn is_expired(&self) -> bool {
        self.acquired_at.elapsed() + TOKEN_REFRESH_MARGIN >= self.expires_in
    }
}

/// HTTP client for the Lark Open Platform API.
///
/// Manages tenant access token lifecycle (auto-refresh) and provides
/// typed methods for bot info, WebSocket endpoint, send/edit messages.
pub(crate) struct LarkApi {
    client: Client,
    app_id: String,
    app_secret: String,
    token_cache: Arc<RwLock<Option<TokenCache>>>,
}

impl LarkApi {
    /// Create a new Lark API client.
    pub fn new(client: Client, app_id: &str, app_secret: &str) -> Self {
        Self {
            client,
            app_id: app_id.to_string(),
            app_secret: app_secret.to_string(),
            token_cache: Arc::new(RwLock::new(None)),
        }
    }

    /// Get a valid tenant access token, refreshing if needed.
    async fn get_token(&self) -> Result<String, ChannelError> {
        // Fast path: check if cached token is still valid
        {
            let cache = self.token_cache.read().await;
            if let Some(ref tc) = *cache
                && !tc.is_expired()
            {
                return Ok(tc.token.clone());
            }
        }

        // Slow path: refresh the token
        self.refresh_token().await
    }

    /// Request a new tenant access token from Lark.
    async fn refresh_token(&self) -> Result<String, ChannelError> {
        let url = format!("{LARK_OPEN_API_BASE}/auth/v3/tenant_access_token/internal");
        let body = TenantAccessTokenRequest {
            app_id: self.app_id.clone(),
            app_secret: self.app_secret.clone(),
        };

        let resp: TenantAccessTokenResponse = self
            .client
            .post(&url)
            .json(&body)
            .send()
            .await
            .map_err(|e| ChannelError::ConnectionFailed(format!("Lark token request failed: {e}")))?
            .json()
            .await
            .map_err(|e| ChannelError::ConnectionFailed(format!("Lark token parse failed: {e}")))?;

        if resp.code != 0 {
            return Err(ChannelError::ConnectionFailed(format!(
                "Lark token error (code={}): {}",
                resp.code, resp.msg
            )));
        }

        let token = resp
            .tenant_access_token
            .ok_or_else(|| ChannelError::ConnectionFailed("Lark token response missing token".into()))?;

        let expires_in = Duration::from_secs(resp.expire.unwrap_or(7200) as u64);

        debug!(expires_in_secs = expires_in.as_secs(), "Lark token refreshed");

        let mut cache = self.token_cache.write().await;
        *cache = Some(TokenCache {
            token: token.clone(),
            acquired_at: Instant::now(),
            expires_in,
        });

        Ok(token)
    }

    /// Get bot identity information.
    pub async fn get_bot_info(&self) -> Result<BotInfoData, ChannelError> {
        let token = self.get_token().await?;
        let url = format!("{LARK_OPEN_API_BASE}/bot/v3/info");

        let resp: BotInfoResponse = self
            .client
            .get(&url)
            .header("Authorization", format!("Bearer {token}"))
            .send()
            .await
            .map_err(|e| ChannelError::PlatformApi(format!("Lark bot info request failed: {e}")))?
            .json()
            .await
            .map_err(|e| ChannelError::PlatformApi(format!("Lark bot info parse failed: {e}")))?;

        if resp.code != 0 {
            return Err(ChannelError::ConnectionFailed(format!(
                "Lark bot info error (code={}): {}",
                resp.code, resp.msg
            )));
        }

        resp.bot
            .ok_or_else(|| ChannelError::PlatformApi("Lark bot info returned no data".into()))
    }

    /// Get the WebSocket endpoint URL for long connection.
    ///
    /// Note: This endpoint uses AppID/AppSecret in the request body for auth,
    /// NOT the Bearer token used by other Lark APIs.
    pub async fn get_ws_endpoint(&self) -> Result<WsEndpointData, ChannelError> {
        let url = format!("{LARK_BASE}/callback/ws/endpoint");

        let body = WsEndpointRequest {
            app_id: self.app_id.clone(),
            app_secret: self.app_secret.clone(),
        };

        let raw_resp = self
            .client
            .post(&url)
            .header("locale", "zh")
            .json(&body)
            .send()
            .await
            .map_err(|e| ChannelError::ConnectionFailed(format!("Lark WS endpoint request failed: {e}")))?;

        let status = raw_resp.status();
        let body_text = raw_resp
            .text()
            .await
            .map_err(|e| ChannelError::ConnectionFailed(format!("Lark WS endpoint read body failed: {e}")))?;

        debug!(status = %status, body_len = body_text.len(), "Lark WS endpoint response received");

        let resp: WsEndpointResponse = serde_json::from_str(&body_text)
            .map_err(|e| ChannelError::ConnectionFailed(format!("Lark WS endpoint parse failed: {e}")))?;

        if resp.code != 0 {
            return Err(ChannelError::ConnectionFailed(format!(
                "Lark WS endpoint error (code={}): {}",
                resp.code, resp.msg
            )));
        }

        resp.data
            .ok_or_else(|| ChannelError::ConnectionFailed("Lark WS endpoint returned no URL".into()))
    }

    /// Send an interactive card message to a chat.
    ///
    /// Uses `receive_id_type=chat_id` to address by chat ID.
    /// Returns the message ID of the sent card.
    pub async fn send_card(&self, chat_id: &str, card_content: &str) -> Result<SendMessageData, ChannelError> {
        let token = self.get_token().await?;
        let url = format!("{LARK_OPEN_API_BASE}/im/v1/messages?receive_id_type=chat_id");

        debug!(chat_id, "Sending Lark card message");

        let body = SendCardRequest {
            receive_id: chat_id.to_string(),
            msg_type: "interactive".into(),
            content: card_content.to_string(),
        };

        let resp: SendMessageResponse = self
            .client
            .post(&url)
            .header("Authorization", format!("Bearer {token}"))
            .json(&body)
            .send()
            .await
            .map_err(|e| ChannelError::MessageSendFailed(format!("Lark send card request failed: {e}")))?
            .json()
            .await
            .map_err(|e| ChannelError::MessageSendFailed(format!("Lark send card parse failed: {e}")))?;

        if resp.code != 0 {
            return Err(ChannelError::MessageSendFailed(format!(
                "Lark send card error (code={}): {}",
                resp.code, resp.msg
            )));
        }

        resp.data
            .ok_or_else(|| ChannelError::MessageSendFailed("Lark send card returned no data".into()))
    }

    /// Upload raw image bytes and send them as an `image` message to a chat.
    ///
    /// Two-step flow: POST the bytes to `/im/v1/images` (`image_type=message`)
    /// to obtain an `image_key`, then send an `image` message referencing it.
    /// Returns the sent message data.
    pub async fn send_image(
        &self,
        chat_id: &str,
        bytes: Vec<u8>,
        filename: &str,
        mime: &str,
    ) -> Result<SendMessageData, ChannelError> {
        let image_key = self.upload_image(bytes, filename, mime).await?;
        let content = serde_json::json!({ "image_key": image_key }).to_string();
        self.send_content(chat_id, "image", &content).await
    }

    /// Upload raw file bytes and send them as a `file` message to a chat.
    ///
    /// Two-step flow: POST the bytes to `/im/v1/files` to obtain a `file_key`,
    /// then send a `file` message referencing it. Returns the sent message data.
    pub async fn send_file(
        &self,
        chat_id: &str,
        bytes: Vec<u8>,
        filename: &str,
        mime: &str,
    ) -> Result<SendMessageData, ChannelError> {
        let file_key = self.upload_file(bytes, filename, mime).await?;
        let content = serde_json::json!({ "file_key": file_key }).to_string();
        self.send_content(chat_id, "file", &content).await
    }

    /// Upload raw image bytes, returning the reusable `image_key`.
    ///
    /// `image_type=message` marks the image for use in chat messages.
    async fn upload_image(&self, bytes: Vec<u8>, filename: &str, mime: &str) -> Result<String, ChannelError> {
        let token = self.get_token().await?;
        let url = format!("{LARK_OPEN_API_BASE}/im/v1/images");

        debug!(bytes = bytes.len(), "Uploading Lark image");

        let part = reqwest::multipart::Part::bytes(bytes)
            .file_name(filename.to_owned())
            .mime_str(mime)
            .map_err(|e| ChannelError::MessageSendFailed(format!("invalid image mime {mime}: {e}")))?;
        let form = reqwest::multipart::Form::new()
            .text("image_type", "message")
            .part("image", part);

        let resp: ImageUploadResponse = self
            .client
            .post(&url)
            .header("Authorization", format!("Bearer {token}"))
            .multipart(form)
            .send()
            .await
            .map_err(|e| ChannelError::MessageSendFailed(format!("Lark image upload request failed: {e}")))?
            .json()
            .await
            .map_err(|e| ChannelError::MessageSendFailed(format!("Lark image upload parse failed: {e}")))?;

        if resp.code != 0 {
            return Err(ChannelError::MessageSendFailed(format!(
                "Lark image upload error (code={}): {}",
                resp.code, resp.msg
            )));
        }

        resp.data
            .map(|d| d.image_key)
            .ok_or_else(|| ChannelError::MessageSendFailed("Lark image upload returned no image_key".into()))
    }

    /// Upload raw file bytes, returning the reusable `file_key`.
    ///
    /// `file_type=stream` is Lark's generic binary type (non opus/mp4/pdf/doc/xls/ppt).
    async fn upload_file(&self, bytes: Vec<u8>, filename: &str, mime: &str) -> Result<String, ChannelError> {
        let token = self.get_token().await?;
        let url = format!("{LARK_OPEN_API_BASE}/im/v1/files");

        debug!(bytes = bytes.len(), file_name = filename, "Uploading Lark file");

        let part = reqwest::multipart::Part::bytes(bytes)
            .file_name(filename.to_owned())
            .mime_str(mime)
            .map_err(|e| ChannelError::MessageSendFailed(format!("invalid file mime {mime}: {e}")))?;
        let form = reqwest::multipart::Form::new()
            .text("file_type", "stream")
            .text("file_name", filename.to_owned())
            .part("file", part);

        let resp: FileUploadResponse = self
            .client
            .post(&url)
            .header("Authorization", format!("Bearer {token}"))
            .multipart(form)
            .send()
            .await
            .map_err(|e| ChannelError::MessageSendFailed(format!("Lark file upload request failed: {e}")))?
            .json()
            .await
            .map_err(|e| ChannelError::MessageSendFailed(format!("Lark file upload parse failed: {e}")))?;

        if resp.code != 0 {
            return Err(ChannelError::MessageSendFailed(format!(
                "Lark file upload error (code={}): {}",
                resp.code, resp.msg
            )));
        }

        resp.data
            .map(|d| d.file_key)
            .ok_or_else(|| ChannelError::MessageSendFailed("Lark file upload returned no file_key".into()))
    }

    /// Send a message with the given `msg_type` and JSON `content` string.
    ///
    /// Mirrors `send_card`'s POST but with a caller-provided message type;
    /// shared by the image/file media sends.
    async fn send_content(
        &self,
        chat_id: &str,
        msg_type: &str,
        content: &str,
    ) -> Result<SendMessageData, ChannelError> {
        let token = self.get_token().await?;
        let url = format!("{LARK_OPEN_API_BASE}/im/v1/messages?receive_id_type=chat_id");

        debug!(chat_id, msg_type, "Sending Lark media message");

        let body = SendCardRequest {
            receive_id: chat_id.to_string(),
            msg_type: msg_type.to_string(),
            content: content.to_string(),
        };

        let resp: SendMessageResponse = self
            .client
            .post(&url)
            .header("Authorization", format!("Bearer {token}"))
            .json(&body)
            .send()
            .await
            .map_err(|e| ChannelError::MessageSendFailed(format!("Lark send {msg_type} request failed: {e}")))?
            .json()
            .await
            .map_err(|e| ChannelError::MessageSendFailed(format!("Lark send {msg_type} parse failed: {e}")))?;

        if resp.code != 0 {
            return Err(ChannelError::MessageSendFailed(format!(
                "Lark send {msg_type} error (code={}): {}",
                resp.code, resp.msg
            )));
        }

        resp.data
            .ok_or_else(|| ChannelError::MessageSendFailed(format!("Lark send {msg_type} returned no data")))
    }

    /// Update (patch) an existing interactive card message.
    pub async fn update_card(&self, message_id: &str, card_content: &str) -> Result<(), ChannelError> {
        let token = self.get_token().await?;
        let url = format!("{LARK_OPEN_API_BASE}/im/v1/messages/{message_id}");

        debug!(message_id, "Updating Lark card message");

        let body = UpdateCardRequest {
            content: card_content.to_string(),
        };

        let resp: GenericResponse = self
            .client
            .patch(&url)
            .header("Authorization", format!("Bearer {token}"))
            .json(&body)
            .send()
            .await
            .map_err(|e| ChannelError::MessageSendFailed(format!("Lark update card request failed: {e}")))?
            .json()
            .await
            .map_err(|e| ChannelError::MessageSendFailed(format!("Lark update card parse failed: {e}")))?;

        if resp.code != 0 {
            warn!(code = resp.code, msg = resp.msg, "Lark update card error");
            return Err(ChannelError::MessageSendFailed(format!(
                "Lark update card error (code={}): {}",
                resp.code, resp.msg
            )));
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn api_stores_credentials() {
        let client = Client::new();
        let api = LarkApi::new(client, "cli_123", "secret_456");
        assert_eq!(api.app_id, "cli_123");
        assert_eq!(api.app_secret, "secret_456");
    }

    #[test]
    fn token_cache_not_expired() {
        let cache = TokenCache {
            token: "test".into(),
            acquired_at: Instant::now(),
            expires_in: Duration::from_secs(7200),
        };
        assert!(!cache.is_expired());
    }

    #[test]
    fn token_cache_expired() {
        let cache = TokenCache {
            token: "test".into(),
            acquired_at: Instant::now() - Duration::from_secs(7200),
            expires_in: Duration::from_secs(7200),
        };
        assert!(cache.is_expired());
    }

    #[test]
    fn token_cache_near_expiry() {
        // Token acquired 6900s ago with 7200s TTL — within 5min margin
        let cache = TokenCache {
            token: "test".into(),
            acquired_at: Instant::now() - Duration::from_secs(6900),
            expires_in: Duration::from_secs(7200),
        };
        assert!(cache.is_expired());
    }
}

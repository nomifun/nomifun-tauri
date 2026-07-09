//! HTTP client for the Slack Web API.
//!
//! Provides typed methods for `auth.test`, `apps.connections.open`,
//! `chat.postMessage`, and `chat.update`.

use reqwest::Client;
use tracing::debug;

use crate::error::ChannelError;

use super::types::{
    AuthTestResult, CompleteUploadFile, CompleteUploadRequest, CompleteUploadResult,
    ConnectionOpenResult, GetUploadUrlResult, PostMessageRequest, PostMessageResult,
    SlackResponse, UpdateMessageRequest, UpdateMessageResult,
};

const SLACK_API_BASE: &str = "https://slack.com/api";

/// HTTP client for the Slack Web API.
///
/// Wraps `reqwest::Client` with two tokens:
/// - `bot_token` (xoxb-): used for all regular API calls
/// - `app_token` (xapp-): used only for `apps.connections.open`
pub(crate) struct SlackApi {
    client: Client,
    bot_token: String,
    app_token: String,
}

impl SlackApi {
    /// Create a new API client.
    pub fn new(client: Client, bot_token: &str, app_token: &str) -> Self {
        Self {
            client,
            bot_token: bot_token.to_string(),
            app_token: app_token.to_string(),
        }
    }

    /// `auth.test` -- validate the bot token and return bot identity.
    pub async fn auth_test(&self) -> Result<AuthTestResult, ChannelError> {
        let url = format!("{SLACK_API_BASE}/auth.test");
        let resp: SlackResponse<AuthTestResult> = self
            .client
            .post(&url)
            .bearer_auth(&self.bot_token)
            .send()
            .await
            .map_err(|e| ChannelError::PlatformApi(format!("auth.test request failed: {e}")))?
            .json()
            .await
            .map_err(|e| ChannelError::PlatformApi(format!("auth.test parse failed: {e}")))?;

        if !resp.ok {
            let desc = resp.error.unwrap_or_default();
            return Err(ChannelError::ConnectionFailed(format!(
                "Slack auth.test failed: {desc}"
            )));
        }

        resp.data
            .ok_or_else(|| ChannelError::PlatformApi("auth.test returned no result".into()))
    }

    /// `apps.connections.open` -- obtain a WebSocket URL for Socket Mode.
    pub async fn open_connection(&self) -> Result<String, ChannelError> {
        let url = format!("{SLACK_API_BASE}/apps.connections.open");
        let resp: SlackResponse<ConnectionOpenResult> = self
            .client
            .post(&url)
            .bearer_auth(&self.app_token)
            .send()
            .await
            .map_err(|e| {
                ChannelError::ConnectionFailed(format!(
                    "apps.connections.open request failed: {e}"
                ))
            })?
            .json()
            .await
            .map_err(|e| {
                ChannelError::PlatformApi(format!(
                    "apps.connections.open parse failed: {e}"
                ))
            })?;

        if !resp.ok {
            let desc = resp.error.unwrap_or_default();
            return Err(ChannelError::ConnectionFailed(format!(
                "Slack apps.connections.open failed: {desc}"
            )));
        }

        resp.data
            .and_then(|d| d.url)
            .filter(|u| !u.is_empty())
            .ok_or_else(|| {
                ChannelError::ConnectionFailed(
                    "apps.connections.open returned no URL".into(),
                )
            })
    }

    /// `chat.postMessage` -- send a message to a channel.
    pub async fn post_message(
        &self,
        req: &PostMessageRequest,
    ) -> Result<String, ChannelError> {
        let url = format!("{SLACK_API_BASE}/chat.postMessage");
        debug!(channel = %req.channel, "Sending Slack message");

        let resp: SlackResponse<PostMessageResult> = self
            .client
            .post(&url)
            .bearer_auth(&self.bot_token)
            .json(req)
            .send()
            .await
            .map_err(|e| {
                ChannelError::MessageSendFailed(format!(
                    "chat.postMessage request failed: {e}"
                ))
            })?
            .json()
            .await
            .map_err(|e| {
                ChannelError::MessageSendFailed(format!(
                    "chat.postMessage parse failed: {e}"
                ))
            })?;

        if !resp.ok {
            let desc = resp.error.unwrap_or_default();
            return Err(ChannelError::MessageSendFailed(format!(
                "chat.postMessage failed: {desc}"
            )));
        }

        resp.data
            .and_then(|d| d.ts)
            .ok_or_else(|| {
                ChannelError::MessageSendFailed(
                    "chat.postMessage returned no ts".into(),
                )
            })
    }

    /// Upload raw bytes as a file via Slack's modern 3-step external upload
    /// flow and share it into `channel_id`. Used for both images and documents
    /// (Slack has no separate photo endpoint). Returns the finalized file id.
    ///
    /// 1. `files.getUploadURLExternal` — reserve an `upload_url` + `file_id`.
    /// 2. POST the raw bytes to `upload_url` (pre-signed; no bot auth).
    /// 3. `files.completeUploadExternal` — finalize and post into the channel.
    pub async fn upload_file(
        &self,
        channel_id: &str,
        bytes: Vec<u8>,
        filename: &str,
        mime: &str,
        caption: Option<&str>,
    ) -> Result<String, ChannelError> {
        // Step 1: reserve an upload URL + file id.
        let length = bytes.len();
        debug!(channel = %channel_id, bytes = length, "Reserving Slack upload URL");
        let url = format!("{SLACK_API_BASE}/files.getUploadURLExternal");
        let resp: SlackResponse<GetUploadUrlResult> = self
            .client
            .post(&url)
            .bearer_auth(&self.bot_token)
            .form(&[("filename", filename.to_string()), ("length", length.to_string())])
            .send()
            .await
            .map_err(|e| {
                ChannelError::MessageSendFailed(format!(
                    "files.getUploadURLExternal request failed: {e}"
                ))
            })?
            .json()
            .await
            .map_err(|e| {
                ChannelError::MessageSendFailed(format!(
                    "files.getUploadURLExternal parse failed: {e}"
                ))
            })?;

        if !resp.ok {
            let desc = resp.error.unwrap_or_default();
            return Err(ChannelError::MessageSendFailed(format!(
                "files.getUploadURLExternal failed: {desc}"
            )));
        }
        let reserved = resp.data.ok_or_else(|| {
            ChannelError::MessageSendFailed("files.getUploadURLExternal returned no result".into())
        })?;
        let upload_url = reserved.upload_url.filter(|u| !u.is_empty()).ok_or_else(|| {
            ChannelError::MessageSendFailed("files.getUploadURLExternal returned no upload_url".into())
        })?;
        let file_id = reserved.file_id.filter(|f| !f.is_empty()).ok_or_else(|| {
            ChannelError::MessageSendFailed("files.getUploadURLExternal returned no file_id".into())
        })?;

        // Step 2: POST the raw bytes to the pre-signed URL. This host is not the
        // Slack Web API, so no bot token — a non-200 status signals failure.
        let upload_resp = self
            .client
            .post(&upload_url)
            .header(reqwest::header::CONTENT_TYPE, mime.to_string())
            .body(bytes)
            .send()
            .await
            .map_err(|e| {
                ChannelError::MessageSendFailed(format!("Slack file upload request failed: {e}"))
            })?;
        if !upload_resp.status().is_success() {
            let status = upload_resp.status();
            return Err(ChannelError::MessageSendFailed(format!(
                "Slack file upload failed with HTTP {status}"
            )));
        }

        // Step 3: finalize the upload and share it into the channel.
        let complete_url = format!("{SLACK_API_BASE}/files.completeUploadExternal");
        let req = CompleteUploadRequest {
            files: vec![CompleteUploadFile {
                id: file_id.clone(),
                title: Some(filename.to_string()),
            }],
            channel_id: Some(channel_id.to_string()),
            initial_comment: caption.map(|c| c.to_string()),
        };
        let complete: SlackResponse<CompleteUploadResult> = self
            .client
            .post(&complete_url)
            .bearer_auth(&self.bot_token)
            .json(&req)
            .send()
            .await
            .map_err(|e| {
                ChannelError::MessageSendFailed(format!(
                    "files.completeUploadExternal request failed: {e}"
                ))
            })?
            .json()
            .await
            .map_err(|e| {
                ChannelError::MessageSendFailed(format!(
                    "files.completeUploadExternal parse failed: {e}"
                ))
            })?;

        if !complete.ok {
            let desc = complete.error.unwrap_or_default();
            return Err(ChannelError::MessageSendFailed(format!(
                "files.completeUploadExternal failed: {desc}"
            )));
        }

        // Prefer the finalized file id; fall back to the reserved id.
        let handle = complete
            .data
            .and_then(|d| d.files)
            .and_then(|mut f| f.pop())
            .and_then(|f| f.id)
            .unwrap_or(file_id);
        Ok(handle)
    }

    /// `chat.update` -- edit an existing message.
    pub async fn update_message(
        &self,
        req: &UpdateMessageRequest,
    ) -> Result<(), ChannelError> {
        let url = format!("{SLACK_API_BASE}/chat.update");
        debug!(channel = %req.channel, ts = %req.ts, "Editing Slack message");

        let resp: SlackResponse<UpdateMessageResult> = self
            .client
            .post(&url)
            .bearer_auth(&self.bot_token)
            .json(req)
            .send()
            .await
            .map_err(|e| {
                ChannelError::MessageSendFailed(format!(
                    "chat.update request failed: {e}"
                ))
            })?
            .json()
            .await
            .map_err(|e| {
                ChannelError::MessageSendFailed(format!(
                    "chat.update parse failed: {e}"
                ))
            })?;

        if !resp.ok {
            let desc = resp.error.unwrap_or_default();
            return Err(ChannelError::MessageSendFailed(format!(
                "chat.update failed: {desc}"
            )));
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn api_stores_tokens() {
        let client = Client::new();
        let api = SlackApi::new(client, "xoxb-test", "xapp-test");
        assert_eq!(api.bot_token, "xoxb-test");
        assert_eq!(api.app_token, "xapp-test");
    }
}

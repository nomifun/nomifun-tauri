//! HTTP client for the Mattermost API v4.
//!
//! Wraps `reqwest::Client`, a base server URL, and a bearer token.  Provides
//! typed methods for the three endpoints the plugin requires:
//!
//! - `GET  /api/v4/users/me`        — validate token, fetch bot identity
//! - `POST /api/v4/files`           — upload raw bytes, returns a file id
//! - `POST /api/v4/posts`           — create a post (send message / attach files)
//! - `PUT  /api/v4/posts/{post_id}` — update a post (edit/stream message)

use reqwest::Client;
use tracing::debug;

use crate::error::ChannelError;

use super::types::{CreatePostRequest, CreatePostResponse, FileUploadResponse, MmUser, UpdatePostRequest};

/// REST client for the Mattermost API v4.
pub(crate) struct MattermostApi {
    client: Client,
    /// Base URL without trailing slash, e.g. `https://mm.example.com`.
    base_url: String,
    /// Bot access token (sent as `Authorization: Bearer <token>`).
    token: String,
}

impl MattermostApi {
    pub fn new(client: Client, base_url: &str, token: &str) -> Self {
        Self {
            client,
            base_url: base_url.trim_end_matches('/').to_owned(),
            token: token.to_owned(),
        }
    }

    /// `GET /api/v4/users/me` — fetch bot identity.
    pub async fn get_me(&self) -> Result<MmUser, ChannelError> {
        let url = format!("{}/api/v4/users/me", self.base_url);
        let resp = self
            .client
            .get(&url)
            .bearer_auth(&self.token)
            .send()
            .await
            .map_err(|e| ChannelError::PlatformApi(format!("Mattermost get_me request failed: {e}")))?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(ChannelError::ConnectionFailed(format!(
                "Mattermost get_me failed ({status}): {body}"
            )));
        }

        resp.json::<MmUser>()
            .await
            .map_err(|e| ChannelError::PlatformApi(format!("Mattermost get_me parse failed: {e}")))
    }

    /// `POST /api/v4/posts` — create a post. Returns the post id.
    pub async fn create_post(&self, req: &CreatePostRequest) -> Result<CreatePostResponse, ChannelError> {
        let url = format!("{}/api/v4/posts", self.base_url);
        debug!(channel_id = %req.channel_id, "Mattermost creating post");

        let resp = self
            .client
            .post(&url)
            .bearer_auth(&self.token)
            .json(req)
            .send()
            .await
            .map_err(|e| ChannelError::MessageSendFailed(format!("Mattermost create_post request failed: {e}")))?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(ChannelError::MessageSendFailed(format!(
                "Mattermost create_post failed ({status}): {body}"
            )));
        }

        resp.json::<CreatePostResponse>()
            .await
            .map_err(|e| ChannelError::MessageSendFailed(format!("Mattermost create_post parse failed: {e}")))
    }

    /// `POST /api/v4/files` — upload raw bytes for `channel_id` and return the
    /// new file id (used as a `file_ids` entry when creating a post).
    pub async fn upload_file(
        &self,
        channel_id: &str,
        bytes: Vec<u8>,
        filename: &str,
        mime: &str,
    ) -> Result<String, ChannelError> {
        let url = format!("{}/api/v4/files", self.base_url);
        debug!(channel_id = %channel_id, bytes = bytes.len(), "Mattermost uploading file");

        let part = reqwest::multipart::Part::bytes(bytes)
            .file_name(filename.to_owned())
            .mime_str(mime)
            .map_err(|e| ChannelError::MessageSendFailed(format!("invalid media mime {mime}: {e}")))?;
        let form = reqwest::multipart::Form::new().part("files", part);

        let resp = self
            .client
            .post(&url)
            .bearer_auth(&self.token)
            .query(&[("channel_id", channel_id), ("filename", filename)])
            .multipart(form)
            .send()
            .await
            .map_err(|e| ChannelError::MessageSendFailed(format!("Mattermost upload_file request failed: {e}")))?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(ChannelError::MessageSendFailed(format!(
                "Mattermost upload_file failed ({status}): {body}"
            )));
        }

        let parsed = resp
            .json::<FileUploadResponse>()
            .await
            .map_err(|e| ChannelError::MessageSendFailed(format!("Mattermost upload_file parse failed: {e}")))?;

        parsed
            .file_infos
            .into_iter()
            .next()
            .map(|f| f.id)
            .ok_or_else(|| ChannelError::MessageSendFailed("Mattermost upload_file returned no file_infos".into()))
    }

    /// `PUT /api/v4/posts/{post_id}` — update an existing post.
    pub async fn update_post(&self, req: &UpdatePostRequest) -> Result<(), ChannelError> {
        let url = format!("{}/api/v4/posts/{}", self.base_url, req.id);
        debug!(post_id = %req.id, "Mattermost updating post");

        let resp = self
            .client
            .put(&url)
            .bearer_auth(&self.token)
            .json(req)
            .send()
            .await
            .map_err(|e| ChannelError::MessageSendFailed(format!("Mattermost update_post request failed: {e}")))?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(ChannelError::MessageSendFailed(format!(
                "Mattermost update_post failed ({status}): {body}"
            )));
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn api_constructs_correct_base_url() {
        let client = Client::new();
        let api = MattermostApi::new(client, "https://mm.example.com/", "my-token");
        assert_eq!(api.base_url, "https://mm.example.com");
        assert_eq!(api.token, "my-token");
    }

    #[test]
    fn api_strips_trailing_slash() {
        let client = Client::new();
        let api = MattermostApi::new(client, "https://mm.example.com///", "tok");
        assert_eq!(api.base_url, "https://mm.example.com");
    }
}

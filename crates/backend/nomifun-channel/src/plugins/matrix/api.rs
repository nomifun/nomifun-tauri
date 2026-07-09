//! Low-level HTTP helpers for the Matrix Client-Server API v3 (Route B).
//!
//! All calls use Bearer-token auth via `reqwest`.  No E2EE — encrypted
//! events are skipped with a warning in the sync loop.

use std::sync::atomic::{AtomicU64, Ordering};

use reqwest::Client;
use tracing::debug;

use crate::constants::{MATRIX_API_TIMEOUT, MATRIX_SYNC_TIMEOUT_MS};
use crate::error::ChannelError;

use super::types::{
    MediaUploadResponse, ProfileResponse, SendEventResponse, SyncResponse, WhoAmIResponse,
};

/// Monotonic transaction-ID counter for idempotent PUT requests.
static TXN_COUNTER: AtomicU64 = AtomicU64::new(1);

/// Generate a unique transaction ID for Matrix PUT requests.
///
/// Uses a process-wide monotonic counter to guarantee uniqueness within a
/// single process lifetime (sufficient for `txnId` semantics — the server
/// deduplicates within a short window per device).
pub fn next_txn_id() -> String {
    let n = TXN_COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("nomi_{n}")
}

/// Thin wrapper around `reqwest::Client` holding the homeserver base URL
/// and access token for Bearer auth.
#[derive(Clone)]
pub struct MatrixApi {
    client: Client,
    homeserver: String, // no trailing '/'
    access_token: String,
}

impl MatrixApi {
    pub fn new(client: Client, homeserver: &str, access_token: &str) -> Self {
        Self {
            client,
            homeserver: homeserver.trim_end_matches('/').to_owned(),
            access_token: access_token.to_owned(),
        }
    }

    // -- whoami ----------------------------------------------------------------

    /// `GET /_matrix/client/v3/account/whoami`
    pub async fn whoami(&self) -> Result<WhoAmIResponse, ChannelError> {
        let url = format!("{}/_matrix/client/v3/account/whoami", self.homeserver);
        let resp = self
            .client
            .get(&url)
            .bearer_auth(&self.access_token)
            .timeout(MATRIX_API_TIMEOUT)
            .send()
            .await
            .map_err(|e| ChannelError::ConnectionFailed(format!("whoami request failed: {e}")))?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(ChannelError::PlatformApi(format!(
                "whoami returned {status}: {body}"
            )));
        }

        resp.json::<WhoAmIResponse>()
            .await
            .map_err(|e| ChannelError::PlatformApi(format!("whoami parse error: {e}")))
    }

    // -- profile ---------------------------------------------------------------

    /// `GET /_matrix/client/v3/profile/{userId}`
    pub async fn get_profile(&self, user_id: &str) -> Result<ProfileResponse, ChannelError> {
        let url = format!(
            "{}/_matrix/client/v3/profile/{}",
            self.homeserver,
            urlencoded(user_id)
        );
        let resp = self
            .client
            .get(&url)
            .bearer_auth(&self.access_token)
            .timeout(MATRIX_API_TIMEOUT)
            .send()
            .await
            .map_err(|e| ChannelError::ConnectionFailed(format!("profile request failed: {e}")))?;

        if !resp.status().is_success() {
            // Profile fetch is best-effort; return empty on failure.
            debug!(user_id, status = %resp.status(), "profile fetch failed, using defaults");
            return Ok(ProfileResponse {
                displayname: None,
                avatar_url: None,
            });
        }

        resp.json::<ProfileResponse>()
            .await
            .map_err(|e| ChannelError::PlatformApi(format!("profile parse error: {e}")))
    }

    // -- sync ------------------------------------------------------------------

    /// `GET /_matrix/client/v3/sync?timeout={MATRIX_SYNC_TIMEOUT_MS}&since={since}`
    pub async fn sync(&self, since: Option<&str>) -> Result<SyncResponse, ChannelError> {
        let mut url = format!(
            "{}/_matrix/client/v3/sync?timeout={}",
            self.homeserver, MATRIX_SYNC_TIMEOUT_MS,
        );
        if let Some(since) = since {
            url.push_str("&since=");
            url.push_str(&urlencoded(since));
        }
        // The HTTP timeout must exceed the server-side long-poll timeout.
        let http_timeout = std::time::Duration::from_millis(MATRIX_SYNC_TIMEOUT_MS as u64 + 10_000);
        let resp = self
            .client
            .get(&url)
            .bearer_auth(&self.access_token)
            .timeout(http_timeout)
            .send()
            .await
            .map_err(|e| ChannelError::ConnectionFailed(format!("sync request failed: {e}")))?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(ChannelError::PlatformApi(format!(
                "sync returned {status}: {body}"
            )));
        }

        resp.json::<SyncResponse>()
            .await
            .map_err(|e| ChannelError::PlatformApi(format!("sync parse error: {e}")))
    }

    // -- send message ----------------------------------------------------------

    /// `PUT /_matrix/client/v3/rooms/{roomId}/send/m.room.message/{txnId}`
    ///
    /// Returns the `event_id` of the sent message.
    pub async fn send_text(
        &self,
        room_id: &str,
        text: &str,
        html: Option<&str>,
    ) -> Result<String, ChannelError> {
        let txn_id = next_txn_id();
        let url = format!(
            "{}/_matrix/client/v3/rooms/{}/send/m.room.message/{}",
            self.homeserver,
            urlencoded(room_id),
            urlencoded(&txn_id),
        );

        let mut body = serde_json::json!({
            "msgtype": "m.text",
            "body": text,
        });
        if let Some(html) = html {
            body["format"] = serde_json::Value::String("org.matrix.custom.html".into());
            body["formatted_body"] = serde_json::Value::String(html.into());
        }

        let resp = self
            .client
            .put(&url)
            .bearer_auth(&self.access_token)
            .json(&body)
            .timeout(MATRIX_API_TIMEOUT)
            .send()
            .await
            .map_err(|e| ChannelError::ConnectionFailed(format!("send_text request failed: {e}")))?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(ChannelError::PlatformApi(format!(
                "send_text returned {status}: {body}"
            )));
        }

        let result: SendEventResponse = resp
            .json()
            .await
            .map_err(|e| ChannelError::PlatformApi(format!("send_text parse error: {e}")))?;
        Ok(result.event_id)
    }

    // -- edit message ----------------------------------------------------------

    /// Send an edit (`m.replace`) for an existing event.
    ///
    /// Per the Matrix spec, edits are sent as a new `m.room.message` with
    /// `m.relates_to.rel_type = "m.replace"` pointing at the original event
    /// and `m.new_content` carrying the replacement.
    pub async fn edit_text(
        &self,
        room_id: &str,
        original_event_id: &str,
        new_text: &str,
        new_html: Option<&str>,
    ) -> Result<String, ChannelError> {
        let txn_id = next_txn_id();
        let url = format!(
            "{}/_matrix/client/v3/rooms/{}/send/m.room.message/{}",
            self.homeserver,
            urlencoded(room_id),
            urlencoded(&txn_id),
        );

        let mut new_content = serde_json::json!({
            "msgtype": "m.text",
            "body": new_text,
        });
        if let Some(html) = new_html {
            new_content["format"] = serde_json::Value::String("org.matrix.custom.html".into());
            new_content["formatted_body"] = serde_json::Value::String(html.into());
        }

        let body = serde_json::json!({
            "msgtype": "m.text",
            "body": format!("* {new_text}"),
            "m.relates_to": {
                "rel_type": "m.replace",
                "event_id": original_event_id,
            },
            "m.new_content": new_content,
        });

        let resp = self
            .client
            .put(&url)
            .bearer_auth(&self.access_token)
            .json(&body)
            .timeout(MATRIX_API_TIMEOUT)
            .send()
            .await
            .map_err(|e| ChannelError::ConnectionFailed(format!("edit_text request failed: {e}")))?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(ChannelError::PlatformApi(format!(
                "edit_text returned {status}: {body}"
            )));
        }

        let result: SendEventResponse = resp
            .json()
            .await
            .map_err(|e| ChannelError::PlatformApi(format!("edit_text parse error: {e}")))?;
        Ok(result.event_id)
    }

    // -- media upload ----------------------------------------------------------

    /// `POST /_matrix/media/v3/upload?filename={filename}`
    ///
    /// Uploads raw bytes to the homeserver content repository, returning the
    /// resulting `mxc://` content URI referenced by media events.
    pub async fn upload_media(
        &self,
        bytes: Vec<u8>,
        filename: &str,
        mime: &str,
    ) -> Result<String, ChannelError> {
        let url = format!(
            "{}/_matrix/media/v3/upload?filename={}",
            self.homeserver,
            urlencoded(filename),
        );

        let resp = self
            .client
            .post(&url)
            .bearer_auth(&self.access_token)
            .header(reqwest::header::CONTENT_TYPE, mime)
            .body(bytes)
            .timeout(MATRIX_API_TIMEOUT)
            .send()
            .await
            .map_err(|e| {
                ChannelError::MessageSendFailed(format!("upload_media request failed: {e}"))
            })?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(ChannelError::MessageSendFailed(format!(
                "upload_media returned {status}: {body}"
            )));
        }

        let result: MediaUploadResponse = resp
            .json()
            .await
            .map_err(|e| ChannelError::MessageSendFailed(format!("upload_media parse error: {e}")))?;
        Ok(result.content_uri)
    }

    // -- send media event ------------------------------------------------------

    /// `PUT /_matrix/client/v3/rooms/{roomId}/send/m.room.message/{txnId}`
    ///
    /// Sends an `m.image` or `m.file` message referencing a previously-uploaded
    /// `mxc://` URI. Returns the `event_id` of the sent message.
    pub async fn send_media_event(
        &self,
        room_id: &str,
        msgtype: &str,
        body: &str,
        mxc_url: &str,
        mime: &str,
    ) -> Result<String, ChannelError> {
        let txn_id = next_txn_id();
        let url = format!(
            "{}/_matrix/client/v3/rooms/{}/send/m.room.message/{}",
            self.homeserver,
            urlencoded(room_id),
            urlencoded(&txn_id),
        );

        let event_body = serde_json::json!({
            "msgtype": msgtype,
            "body": body,
            "url": mxc_url,
            "info": { "mimetype": mime },
        });

        let resp = self
            .client
            .put(&url)
            .bearer_auth(&self.access_token)
            .json(&event_body)
            .timeout(MATRIX_API_TIMEOUT)
            .send()
            .await
            .map_err(|e| {
                ChannelError::MessageSendFailed(format!("send_media_event request failed: {e}"))
            })?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(ChannelError::MessageSendFailed(format!(
                "send_media_event returned {status}: {body}"
            )));
        }

        let result: SendEventResponse = resp.json().await.map_err(|e| {
            ChannelError::MessageSendFailed(format!("send_media_event parse error: {e}"))
        })?;
        Ok(result.event_id)
    }
}

/// Percent-encode a path segment for use in Matrix API URLs.
fn urlencoded(s: &str) -> String {
    // Matrix room IDs and event IDs contain `!`, `$`, `:` which must be
    // percent-encoded when used in URL path segments.
    let mut out = String::with_capacity(s.len() * 2);
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char);
            }
            _ => {
                out.push_str(&format!("%{b:02X}"));
            }
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn txn_id_is_monotonic() {
        let a = next_txn_id();
        let b = next_txn_id();
        let c = next_txn_id();
        // Each must start with "nomi_" and parse to increasing numbers.
        let na: u64 = a.strip_prefix("nomi_").unwrap().parse().unwrap();
        let nb: u64 = b.strip_prefix("nomi_").unwrap().parse().unwrap();
        let nc: u64 = c.strip_prefix("nomi_").unwrap().parse().unwrap();
        assert!(na < nb);
        assert!(nb < nc);
    }

    #[test]
    fn txn_id_uniqueness() {
        let mut ids: Vec<String> = (0..100).map(|_| next_txn_id()).collect();
        let len_before = ids.len();
        ids.sort();
        ids.dedup();
        assert_eq!(ids.len(), len_before, "txn IDs must be unique");
    }

    #[test]
    fn urlencoded_basic() {
        assert_eq!(urlencoded("hello"), "hello");
        assert_eq!(urlencoded("a-b_c.d~e"), "a-b_c.d~e");
    }

    #[test]
    fn urlencoded_special_chars() {
        // Room IDs like !abc:example.com
        let encoded = urlencoded("!abc:example.com");
        assert_eq!(encoded, "%21abc%3Aexample.com");
    }

    #[test]
    fn urlencoded_event_id() {
        // Event IDs like $abc123
        let encoded = urlencoded("$abc123");
        assert_eq!(encoded, "%24abc123");
    }

    #[test]
    fn urlencoded_user_id() {
        // User IDs like @bot:matrix.org
        let encoded = urlencoded("@bot:matrix.org");
        assert_eq!(encoded, "%40bot%3Amatrix.org");
    }

    #[test]
    fn urlencoded_empty() {
        assert_eq!(urlencoded(""), "");
    }
}

use std::time::Duration;

use aes::Aes128;
use aes::cipher::generic_array::GenericArray;
use aes::cipher::{BlockEncrypt, KeyInit};
use base64::Engine;
use md5::{Digest, Md5};
use reqwest::Client;
use serde::Serialize;
use serde::de::DeserializeOwned;
use tracing::{debug, warn};
use uuid::Uuid;

use crate::constants::{WEIXIN_API_TIMEOUT, WEIXIN_POLL_TIMEOUT};
use crate::error::ChannelError;

use super::types::{
    GetUpdatesRequest, GetUpdatesResponse, GetUploadUrlRequest, GetUploadUrlResponse, ILinkResponse, ITEM_TYPE_FILE,
    ITEM_TYPE_IMAGE, ITEM_TYPE_TEXT, QrCodeData, QrCodeStatusData, SendCdnMedia, SendFileItem, SendImageItem,
    SendMessageItem, SendMessageMsg, SendMessageRequest, SendTextItem, UPLOAD_MEDIA_TYPE_FILE, UPLOAD_MEDIA_TYPE_IMAGE,
};

/// WeChat media CDN base (AES-128-ECB encrypted blobs). Distinct host from the
/// iLink gateway `base_url`; the `upload_param` is the pre-signed credential, so
/// CDN requests carry NO gateway auth headers.
const WEIXIN_CDN_BASE_URL: &str = "https://novac2c.cdn.weixin.qq.com/c2c";

/// AES-128-ECB ciphertext size for `n` plaintext bytes (PKCS7 always pads, so a
/// full-block plaintext still grows by one block).
fn aes_ecb_padded_size(n: usize) -> usize {
    n + (16 - n % 16)
}

/// Encrypt with AES-128-ECB + PKCS7 padding — the scheme all WeChat CDN media
/// uses. ECB = each 16-byte block encrypted independently (no IV/chaining).
fn aes128_ecb_pkcs7_encrypt(plaintext: &[u8], key: &[u8; 16]) -> Vec<u8> {
    let cipher = Aes128::new(GenericArray::from_slice(key));
    let pad = 16 - (plaintext.len() % 16); // PKCS7: 1..=16 bytes, value == count
    let mut buf = Vec::with_capacity(plaintext.len() + pad);
    buf.extend_from_slice(plaintext);
    buf.extend(std::iter::repeat(pad as u8).take(pad));
    for chunk in buf.chunks_mut(16) {
        cipher.encrypt_block(GenericArray::from_mut_slice(chunk));
    }
    buf
}

/// HTTP client for the WeChat iLink Bot API.
pub(crate) struct WeixinApi {
    client: Client,
    base_url: String,
    bot_token: String,
    wechat_uin: String,
}

impl WeixinApi {
    pub fn new(client: Client, base_url: &str, bot_token: &str) -> Self {
        let base = base_url.trim_end_matches('/');

        let mut uin_bytes = [0u8; 4];
        getrandom::getrandom(&mut uin_bytes).expect("RNG failure");
        let wechat_uin = base64::engine::general_purpose::STANDARD.encode(uin_bytes);

        Self {
            client,
            base_url: base.to_string(),
            bot_token: bot_token.to_string(),
            wechat_uin,
        }
    }

    #[cfg(test)]
    pub fn bot_token(&self) -> &str {
        &self.bot_token
    }

    #[cfg(test)]
    pub fn wechat_uin(&self) -> &str {
        &self.wechat_uin
    }

    // -----------------------------------------------------------------------
    // Internal helpers
    // -----------------------------------------------------------------------

    async fn authenticated_post<T: DeserializeOwned>(
        &self,
        endpoint: &str,
        body: &impl Serialize,
        timeout: Duration,
    ) -> Result<T, ChannelError> {
        let url = format!("{}/{}", self.base_url, endpoint);

        let resp = self
            .client
            .post(&url)
            .header("Content-Type", "application/json")
            .header("AuthorizationType", "ilink_bot_token")
            .header("Authorization", format!("Bearer {}", self.bot_token))
            .header("X-WECHAT-UIN", &self.wechat_uin)
            .timeout(timeout)
            .json(body)
            .send()
            .await
            .map_err(|e| ChannelError::PlatformApi(format!("{endpoint} request failed: {e}")))?;

        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            return Err(ChannelError::PlatformApi(format!("{endpoint} HTTP {status}: {text}")));
        }

        resp.json()
            .await
            .map_err(|e| ChannelError::PlatformApi(format!("{endpoint} parse failed: {e}")))
    }

    async fn ilink_get<T: DeserializeOwned>(&self, endpoint: &str, query: &[(&str, &str)]) -> Result<T, ChannelError> {
        let url = format!("{}/{}", self.base_url, endpoint);

        let resp = self
            .client
            .get(&url)
            .header("iLink-App-ClientVersion", "1")
            .query(query)
            .send()
            .await
            .map_err(|e| ChannelError::PlatformApi(format!("{endpoint} request failed: {e}")))?;

        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            return Err(ChannelError::PlatformApi(format!("{endpoint} HTTP {status}: {text}")));
        }

        resp.json()
            .await
            .map_err(|e| ChannelError::PlatformApi(format!("{endpoint} parse failed: {e}")))
    }

    // -----------------------------------------------------------------------
    // QR code login
    // -----------------------------------------------------------------------

    /// Fetch a QR code for bot login.
    ///
    /// `GET /ilink/bot/get_bot_qrcode?bot_type=3`
    pub async fn get_bot_qrcode(&self) -> Result<QrCodeData, ChannelError> {
        debug!("Fetching WeChat QR code");

        // Try direct response first, then wrapped
        let result: Result<QrCodeData, _> = self.ilink_get("ilink/bot/get_bot_qrcode", &[("bot_type", "3")]).await;

        match result {
            Ok(data) if data.qrcode.is_some() => Ok(data),
            _ => {
                let wrapped: ILinkResponse<QrCodeData> =
                    self.ilink_get("ilink/bot/get_bot_qrcode", &[("bot_type", "3")]).await?;
                wrapped
                    .data
                    .ok_or_else(|| ChannelError::PlatformApi("get_bot_qrcode returned no data".into()))
            }
        }
    }

    /// Check the status of a QR code scan.
    ///
    /// `GET /ilink/bot/get_qrcode_status?qrcode=<ticket>`
    pub async fn get_qrcode_status(&self, qrcode: &str) -> Result<QrCodeStatusData, ChannelError> {
        // Try direct response first, then wrapped
        let result: Result<QrCodeStatusData, _> = self
            .ilink_get("ilink/bot/get_qrcode_status", &[("qrcode", qrcode)])
            .await;

        match result {
            Ok(data) if data.status.is_some() => Ok(data),
            _ => {
                let wrapped: ILinkResponse<QrCodeStatusData> = self
                    .ilink_get("ilink/bot/get_qrcode_status", &[("qrcode", qrcode)])
                    .await?;
                wrapped
                    .data
                    .ok_or_else(|| ChannelError::PlatformApi("get_qrcode_status returned no data".into()))
            }
        }
    }

    // -----------------------------------------------------------------------
    // Long-polling
    // -----------------------------------------------------------------------

    /// Long-poll for new updates using buffer-based protocol.
    ///
    /// `POST /ilink/bot/getupdates`
    pub async fn get_updates(&self, buf: &str) -> Result<GetUpdatesResponse, ChannelError> {
        let body = GetUpdatesRequest {
            get_updates_buf: buf.to_string(),
            base_info: serde_json::json!({}),
        };

        let timeout = WEIXIN_POLL_TIMEOUT + Duration::from_secs(10);

        self.authenticated_post("ilink/bot/getupdates", &body, timeout).await
    }

    // -----------------------------------------------------------------------
    // Send message
    // -----------------------------------------------------------------------

    /// Send a text message.
    ///
    /// `POST /ilink/bot/sendmessage`
    pub async fn send_message(
        &self,
        to_user_id: &str,
        text: &str,
        context_token: Option<&str>,
    ) -> Result<(), ChannelError> {
        debug!(to_user_id, "Sending WeChat message");

        let body = SendMessageRequest {
            msg: SendMessageMsg {
                to_user_id: to_user_id.to_string(),
                client_id: Uuid::new_v4().to_string(),
                message_type: 2,
                message_state: 2,
                item_list: vec![SendMessageItem {
                    item_type: ITEM_TYPE_TEXT,
                    text_item: Some(SendTextItem { text: text.to_string() }),
                    image_item: None,
                    file_item: None,
                }],
                context_token: context_token.map(String::from),
            },
            base_info: serde_json::json!({}),
        };

        let _resp: serde_json::Value = self
            .authenticated_post("ilink/bot/sendmessage", &body, WEIXIN_API_TIMEOUT)
            .await
            .map_err(|e| {
                warn!(to_user_id, error = %e, "sendmessage failed");
                ChannelError::MessageSendFailed(format!("sendmessage failed: {e}"))
            })?;

        Ok(())
    }

    // -----------------------------------------------------------------------
    // Send media (image / file) — AES-128-ECB CDN upload + sendmessage
    // -----------------------------------------------------------------------

    /// Upload `bytes` to the WeChat CDN (AES-128-ECB encrypted) and send it as an
    /// image or file message. Mirrors the iLink reference SDK flow exactly:
    /// md5(plaintext) → reserve upload URL → encrypt → PUT to CDN → sendmessage
    /// with a media item referencing the returned encrypted param.
    ///
    /// `context_token` (from the inbound message) is required for the reply to
    /// route to the right conversation — same contract as text sends.
    pub async fn send_media(
        &self,
        to_user_id: &str,
        bytes: Vec<u8>,
        file_name: &str,
        is_image: bool,
        context_token: Option<&str>,
    ) -> Result<(), ChannelError> {
        // 1. Plaintext hash + sizes.
        let rawsize = bytes.len() as u64;
        let rawfilemd5 = {
            let mut hasher = Md5::new();
            hasher.update(&bytes);
            hex::encode(hasher.finalize())
        };
        let filesize = aes_ecb_padded_size(bytes.len()) as u64;

        // 2. Random 16-byte filekey + AES-128 key (hex-encoded on the wire).
        let mut filekey_bytes = [0u8; 16];
        let mut aeskey_bytes = [0u8; 16];
        getrandom::getrandom(&mut filekey_bytes).expect("RNG failure");
        getrandom::getrandom(&mut aeskey_bytes).expect("RNG failure");
        let filekey = hex::encode(filekey_bytes);
        let aeskey_hex = hex::encode(aeskey_bytes);

        let media_type = if is_image {
            UPLOAD_MEDIA_TYPE_IMAGE
        } else {
            UPLOAD_MEDIA_TYPE_FILE
        };

        // 3. Reserve a pre-signed CDN upload URL.
        let upload_req = GetUploadUrlRequest {
            filekey: filekey.clone(),
            media_type,
            to_user_id: to_user_id.to_string(),
            rawsize,
            rawfilemd5,
            filesize,
            no_need_thumb: true,
            aeskey: aeskey_hex.clone(),
        };
        let upload_resp: GetUploadUrlResponse = self
            .authenticated_post("ilink/bot/getuploadurl", &upload_req, WEIXIN_API_TIMEOUT)
            .await
            .map_err(|e| ChannelError::MessageSendFailed(format!("getuploadurl failed: {e}")))?;
        let upload_param = upload_resp
            .upload_param
            .filter(|s| !s.is_empty())
            .ok_or_else(|| ChannelError::MessageSendFailed("getuploadurl returned no upload_param".into()))?;

        // 4. AES-128-ECB encrypt and PUT the ciphertext to the CDN.
        let ciphertext = aes128_ecb_pkcs7_encrypt(&bytes, &aeskey_bytes);
        let download_param = self.upload_to_cdn(&upload_param, &filekey, ciphertext).await?;

        // 5. Build the media item and send. NOTE: `media.aes_key` is the base64 of
        //    the AES key's HEX STRING bytes (not the raw 16 bytes) — replicated
        //    exactly from the reference SDK; the raw key still rides `aeskey`-derived
        //    fields the server expects.
        let aes_key_field = base64::engine::general_purpose::STANDARD.encode(aeskey_hex.as_bytes());
        let media = SendCdnMedia {
            encrypt_query_param: download_param,
            aes_key: aes_key_field,
            encrypt_type: 1,
        };
        let item = if is_image {
            SendMessageItem {
                item_type: ITEM_TYPE_IMAGE,
                text_item: None,
                image_item: Some(SendImageItem { media, mid_size: filesize }),
                file_item: None,
            }
        } else {
            SendMessageItem {
                item_type: ITEM_TYPE_FILE,
                text_item: None,
                image_item: None,
                file_item: Some(SendFileItem {
                    media,
                    file_name: file_name.to_string(),
                    len: rawsize.to_string(),
                }),
            }
        };

        let body = SendMessageRequest {
            msg: SendMessageMsg {
                to_user_id: to_user_id.to_string(),
                client_id: Uuid::new_v4().to_string(),
                message_type: 2,
                message_state: 2,
                item_list: vec![item],
                context_token: context_token.map(String::from),
            },
            base_info: serde_json::json!({}),
        };
        let _resp: serde_json::Value = self
            .authenticated_post("ilink/bot/sendmessage", &body, WEIXIN_API_TIMEOUT)
            .await
            .map_err(|e| {
                warn!(to_user_id, error = %e, "send media message failed");
                ChannelError::MessageSendFailed(format!("send media message failed: {e}"))
            })?;

        Ok(())
    }

    /// POST AES-encrypted `ciphertext` to the WeChat CDN and return the download
    /// `x-encrypted-param` used to reference the file in a message. No gateway
    /// auth — `upload_param` is itself the pre-signed credential.
    async fn upload_to_cdn(
        &self,
        upload_param: &str,
        filekey: &str,
        ciphertext: Vec<u8>,
    ) -> Result<String, ChannelError> {
        let url = format!("{WEIXIN_CDN_BASE_URL}/upload");
        let resp = self
            .client
            .post(&url)
            .query(&[("encrypted_query_param", upload_param), ("filekey", filekey)])
            .header(reqwest::header::CONTENT_TYPE, "application/octet-stream")
            .timeout(WEIXIN_API_TIMEOUT)
            .body(ciphertext)
            .send()
            .await
            .map_err(|e| ChannelError::MessageSendFailed(format!("CDN upload request failed: {e}")))?;

        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            return Err(ChannelError::MessageSendFailed(format!("CDN upload HTTP {status}: {text}")));
        }

        resp.headers()
            .get("x-encrypted-param")
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned)
            .ok_or_else(|| ChannelError::MessageSendFailed("CDN upload response missing x-encrypted-param".into()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn aes_ecb_padded_size_matches_reference() {
        // PKCS7 always pads; ceil((n+1)/16)*16.
        assert_eq!(aes_ecb_padded_size(0), 16);
        assert_eq!(aes_ecb_padded_size(15), 16);
        assert_eq!(aes_ecb_padded_size(16), 32);
        assert_eq!(aes_ecb_padded_size(17), 32);
        assert_eq!(aes_ecb_padded_size(31), 32);
        assert_eq!(aes_ecb_padded_size(32), 48);
    }

    #[test]
    fn aes128_ecb_pkcs7_roundtrips() {
        use aes::cipher::BlockDecrypt;
        let key = [7u8; 16];
        let plaintext = b"hello wechat image bytes \x00\x01\x02\xffend".to_vec();
        let ct = aes128_ecb_pkcs7_encrypt(&plaintext, &key);
        assert_eq!(ct.len(), aes_ecb_padded_size(plaintext.len()));
        assert_eq!(ct.len() % 16, 0);

        // Decrypt (ECB block-by-block) and strip PKCS7 to confirm correctness.
        let cipher = Aes128::new(GenericArray::from_slice(&key));
        let mut buf = ct.clone();
        for chunk in buf.chunks_mut(16) {
            cipher.decrypt_block(GenericArray::from_mut_slice(chunk));
        }
        let pad = *buf.last().unwrap() as usize;
        assert!((1..=16).contains(&pad));
        buf.truncate(buf.len() - pad);
        assert_eq!(buf, plaintext);
    }

    #[test]
    fn full_block_plaintext_gets_extra_padding_block() {
        let key = [1u8; 16];
        let plaintext = vec![0xABu8; 16]; // exactly one block
        let ct = aes128_ecb_pkcs7_encrypt(&plaintext, &key);
        assert_eq!(ct.len(), 32, "PKCS7 adds a full padding block on block-aligned input");
    }

    #[test]
    fn api_stores_credentials() {
        let client = Client::new();
        let api = WeixinApi::new(client, "https://ilinkai.weixin.qq.com/", "tok_abc");
        assert_eq!(api.base_url, "https://ilinkai.weixin.qq.com");
        assert_eq!(api.bot_token(), "tok_abc");
    }

    #[test]
    fn api_normalizes_trailing_slash() {
        let client = Client::new();
        let api = WeixinApi::new(client, "https://ilinkai.weixin.qq.com///", "tok");
        assert!(api.base_url.ends_with("com"));
    }

    #[test]
    fn api_generates_wechat_uin() {
        let client = Client::new();
        let api = WeixinApi::new(client, "https://example.com", "tok");
        // base64 of 4 bytes should be 8 chars (with padding)
        assert_eq!(api.wechat_uin().len(), 8);
        // Should be valid base64
        let decoded = base64::engine::general_purpose::STANDARD.decode(api.wechat_uin());
        assert!(decoded.is_ok());
        assert_eq!(decoded.unwrap().len(), 4);
    }

    #[test]
    fn api_generates_different_uin_each_time() {
        let client1 = Client::new();
        let api1 = WeixinApi::new(client1, "https://example.com", "tok");
        let client2 = Client::new();
        let api2 = WeixinApi::new(client2, "https://example.com", "tok");
        // Extremely unlikely to collide (2^32 space)
        assert_ne!(api1.wechat_uin(), api2.wechat_uin());
    }
}

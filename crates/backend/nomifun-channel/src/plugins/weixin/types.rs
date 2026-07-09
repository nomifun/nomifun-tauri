use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// Item type constants
// ---------------------------------------------------------------------------

pub(crate) const ITEM_TYPE_TEXT: i32 = 1;
#[allow(dead_code)]
pub(crate) const ITEM_TYPE_IMAGE: i32 = 2;
pub(crate) const ITEM_TYPE_VOICE: i32 = 3;
#[allow(dead_code)]
pub(crate) const ITEM_TYPE_FILE: i32 = 4;

// `getuploadurl` media_type (proto UploadMediaType) — DISTINCT from the
// `item_list[].type` constants above: FILE is 3 here but 4 as an item type.
pub(crate) const UPLOAD_MEDIA_TYPE_IMAGE: i32 = 1;
pub(crate) const UPLOAD_MEDIA_TYPE_FILE: i32 = 3;

// ---------------------------------------------------------------------------
// ILinkResponse wrapper (kept for QR code endpoints that may use it)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Deserialize)]
#[allow(dead_code)]
pub(crate) struct ILinkResponse<T> {
    #[serde(default)]
    pub code: Option<i32>,
    #[serde(default)]
    pub msg: Option<String>,
    #[serde(default)]
    pub data: Option<T>,
}

// ---------------------------------------------------------------------------
// QR code login
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default, Deserialize)]
#[allow(dead_code)]
pub(crate) struct QrCodeData {
    #[serde(default)]
    pub qrcode: Option<String>,
    #[serde(default)]
    pub qrcode_img_content: Option<String>,
}

/// Response from `get_qrcode_status`.
///
/// The actual API returns snake_case with non-standard field names:
/// `{ status, bot_token, ilink_bot_id, baseurl }`
#[derive(Debug, Clone, Default, Deserialize)]
pub(crate) struct QrCodeStatusData {
    #[serde(default)]
    pub status: Option<String>,
    #[serde(default)]
    pub bot_token: Option<String>,
    #[serde(default)]
    pub ilink_bot_id: Option<String>,
    /// All lowercase — NOT `base_url`.
    #[serde(default)]
    pub baseurl: Option<String>,
}

// ---------------------------------------------------------------------------
// getupdates (long-polling, buffer-based protocol)
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize)]
pub(crate) struct GetUpdatesRequest {
    pub get_updates_buf: String,
    pub base_info: serde_json::Value,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[allow(dead_code)]
pub(crate) struct GetUpdatesResponse {
    #[serde(default)]
    pub ret: Option<i32>,
    #[serde(default)]
    pub errcode: Option<i32>,
    #[serde(default)]
    pub errmsg: Option<String>,
    #[serde(default)]
    pub msgs: Option<Vec<WeixinRawMessage>>,
    #[serde(default)]
    pub get_updates_buf: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub(crate) struct WeixinRawMessage {
    #[serde(default)]
    pub from_user_id: Option<String>,
    #[serde(default)]
    pub context_token: Option<String>,
    #[serde(default)]
    pub msg_id: Option<String>,
    #[serde(default)]
    pub item_list: Option<Vec<WeixinRawItem>>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[allow(dead_code)]
pub(crate) struct WeixinRawItem {
    #[serde(default, rename = "type")]
    pub item_type: Option<i32>,
    #[serde(default)]
    pub text_item: Option<TextItem>,
    #[serde(default)]
    pub voice_item: Option<VoiceItem>,
    #[serde(default)]
    pub image_item: Option<MediaItemData>,
    #[serde(default)]
    pub file_item: Option<MediaItemData>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub(crate) struct TextItem {
    #[serde(default)]
    pub text: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub(crate) struct VoiceItem {
    #[serde(default)]
    pub text: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[allow(dead_code)]
pub(crate) struct MediaItemData {
    #[serde(default)]
    pub media: Option<MediaEncryptInfo>,
    #[serde(default)]
    pub aeskey: Option<String>,
    #[serde(default)]
    pub file_name: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[allow(dead_code)]
pub(crate) struct MediaEncryptInfo {
    #[serde(default)]
    pub encrypt_query_param: Option<String>,
    #[serde(default)]
    pub aes_key: Option<String>,
}

// ---------------------------------------------------------------------------
// sendmessage
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize)]
pub(crate) struct SendMessageRequest {
    pub msg: SendMessageMsg,
    pub base_info: serde_json::Value,
}

#[derive(Debug, Serialize)]
pub(crate) struct SendMessageMsg {
    pub to_user_id: String,
    pub client_id: String,
    pub message_type: i32,
    pub message_state: i32,
    pub item_list: Vec<SendMessageItem>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub context_token: Option<String>,
}

#[derive(Debug, Serialize)]
pub(crate) struct SendMessageItem {
    #[serde(rename = "type")]
    pub item_type: i32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text_item: Option<SendTextItem>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub image_item: Option<SendImageItem>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub file_item: Option<SendFileItem>,
}

#[derive(Debug, Serialize)]
pub(crate) struct SendTextItem {
    pub text: String,
}

// ---------------------------------------------------------------------------
// Outbound media (image/file) — CDN-encrypted references. Mirrors the iLink
// reference SDK (openclaw-weixin); see the media-upload findings spec doc.
// ---------------------------------------------------------------------------

/// `getuploadurl` request: reserve a pre-signed CDN upload URL for one media file.
#[derive(Debug, Serialize)]
pub(crate) struct GetUploadUrlRequest {
    /// Random 16-byte key, hex-encoded (32 chars).
    pub filekey: String,
    /// [`UPLOAD_MEDIA_TYPE_IMAGE`] / [`UPLOAD_MEDIA_TYPE_FILE`].
    pub media_type: i32,
    pub to_user_id: String,
    /// Plaintext size in bytes.
    pub rawsize: u64,
    /// Plaintext MD5, hex-encoded.
    pub rawfilemd5: String,
    /// Ciphertext size (AES-128-ECB + PKCS7 padded).
    pub filesize: u64,
    /// No thumbnail upload URL needed.
    pub no_need_thumb: bool,
    /// AES-128 key, hex-encoded (32 chars).
    pub aeskey: String,
}

/// `getuploadurl` response.
#[derive(Debug, Clone, Default, Deserialize)]
#[allow(dead_code)]
pub(crate) struct GetUploadUrlResponse {
    /// Encrypted param for uploading the original file.
    #[serde(default)]
    pub upload_param: Option<String>,
    /// Thumbnail upload param (absent when `no_need_thumb`).
    #[serde(default)]
    pub thumb_upload_param: Option<String>,
}

/// CDN media reference carried in an outbound image/file item.
#[derive(Debug, Serialize)]
pub(crate) struct SendCdnMedia {
    /// Download param returned by the CDN upload (`x-encrypted-param`).
    pub encrypt_query_param: String,
    /// AES key — base64 of the hex-string's bytes, matching the reference SDK.
    pub aes_key: String,
    /// 1 = packed thumbnail/mid-size info.
    pub encrypt_type: i32,
}

#[derive(Debug, Serialize)]
pub(crate) struct SendImageItem {
    pub media: SendCdnMedia,
    /// Ciphertext size of the original image.
    pub mid_size: u64,
}

#[derive(Debug, Serialize)]
pub(crate) struct SendFileItem {
    pub media: SendCdnMedia,
    pub file_name: String,
    /// Plaintext size in bytes, as a string.
    pub len: String,
}

// ---------------------------------------------------------------------------
// SSE event payloads (frontend-facing — DO NOT CHANGE field names)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct SseQrEvent {
    pub qrcode_data: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct SseDoneEvent {
    pub account_id: String,
    pub bot_token: String,
    pub base_url: String,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct SseErrorEvent {
    pub message: String,
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deserialize_qr_code_data_direct() {
        let json = r#"{"qrcode": "ticket_123", "qrcode_img_content": "https://example.com/qr.png"}"#;
        let data: QrCodeData = serde_json::from_str(json).unwrap();
        assert_eq!(data.qrcode.as_deref(), Some("ticket_123"));
        assert_eq!(data.qrcode_img_content.as_deref(), Some("https://example.com/qr.png"));
    }

    #[test]
    fn deserialize_qr_code_data_wrapped() {
        let json = r#"{"code": 0, "data": {"qrcode": "ticket_456"}}"#;
        let resp: ILinkResponse<QrCodeData> = serde_json::from_str(json).unwrap();
        assert_eq!(resp.code, Some(0));
        assert_eq!(resp.data.unwrap().qrcode.unwrap(), "ticket_456");
    }

    #[test]
    fn deserialize_qr_status_confirmed() {
        let json = r#"{
            "status": "confirmed",
            "bot_token": "tok_1",
            "ilink_bot_id": "acc_1",
            "baseurl": "https://ilinkai.weixin.qq.com"
        }"#;
        let data: QrCodeStatusData = serde_json::from_str(json).unwrap();
        assert_eq!(data.status.as_deref(), Some("confirmed"));
        assert_eq!(data.bot_token.as_deref(), Some("tok_1"));
        assert_eq!(data.ilink_bot_id.as_deref(), Some("acc_1"));
        assert_eq!(data.baseurl.as_deref(), Some("https://ilinkai.weixin.qq.com"));
    }

    #[test]
    fn deserialize_qr_status_scaned() {
        let json = r#"{"status": "scaned"}"#;
        let data: QrCodeStatusData = serde_json::from_str(json).unwrap();
        assert_eq!(data.status.as_deref(), Some("scaned"));
    }

    #[test]
    fn deserialize_get_updates_response() {
        let json = r#"{
            "ret": 0,
            "errcode": 0,
            "msgs": [{
                "from_user_id": "user_1",
                "context_token": "ctx_abc",
                "msg_id": "msg_1",
                "item_list": [
                    {"type": 1, "text_item": {"text": "Hello"}}
                ]
            }],
            "get_updates_buf": "base64buf=="
        }"#;
        let resp: GetUpdatesResponse = serde_json::from_str(json).unwrap();
        assert_eq!(resp.ret, Some(0));
        assert_eq!(resp.errcode, Some(0));
        let msgs = resp.msgs.unwrap();
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0].from_user_id.as_deref(), Some("user_1"));
        assert_eq!(msgs[0].context_token.as_deref(), Some("ctx_abc"));
        let items = msgs[0].item_list.as_ref().unwrap();
        assert_eq!(items[0].item_type, Some(1));
        assert_eq!(items[0].text_item.as_ref().unwrap().text.as_deref(), Some("Hello"));
        assert_eq!(resp.get_updates_buf.as_deref(), Some("base64buf=="));
    }

    #[test]
    fn deserialize_get_updates_with_media() {
        let json = r#"{
            "ret": 0,
            "msgs": [{
                "from_user_id": "user_2",
                "msg_id": "msg_2",
                "item_list": [
                    {"type": 2, "image_item": {"media": {"encrypt_query_param": "enc_param", "aes_key": "key123"}, "aeskey": "hex_key"}},
                    {"type": 4, "file_item": {"media": {"encrypt_query_param": "enc_f"}, "file_name": "doc.pdf"}}
                ]
            }]
        }"#;
        let resp: GetUpdatesResponse = serde_json::from_str(json).unwrap();
        let msgs = resp.msgs.unwrap();
        let items = msgs[0].item_list.as_ref().unwrap();
        assert_eq!(items[0].item_type, Some(2));
        let img = items[0].image_item.as_ref().unwrap();
        assert_eq!(
            img.media.as_ref().unwrap().encrypt_query_param.as_deref(),
            Some("enc_param")
        );
        assert_eq!(img.aeskey.as_deref(), Some("hex_key"));
        assert_eq!(items[1].item_type, Some(4));
        assert_eq!(
            items[1].file_item.as_ref().unwrap().file_name.as_deref(),
            Some("doc.pdf")
        );
    }

    #[test]
    fn deserialize_get_updates_api_error() {
        let json = r#"{"ret": 1001, "errcode": 401, "errmsg": "invalid token"}"#;
        let resp: GetUpdatesResponse = serde_json::from_str(json).unwrap();
        assert_eq!(resp.ret, Some(1001));
        assert_eq!(resp.errcode, Some(401));
        assert!(resp.msgs.is_none());
    }

    #[test]
    fn serialize_send_message_request() {
        let req = SendMessageRequest {
            msg: SendMessageMsg {
                to_user_id: "user_1".into(),
                client_id: "uuid-1234".into(),
                message_type: 2,
                message_state: 2,
                item_list: vec![SendMessageItem {
                    item_type: ITEM_TYPE_TEXT,
                    text_item: Some(SendTextItem { text: "Hello".into() }),
                    image_item: None,
                    file_item: None,
                }],
                context_token: Some("ctx_abc".into()),
            },
            base_info: serde_json::json!({}),
        };
        let json = serde_json::to_string(&req).unwrap();
        assert!(json.contains(r#""to_user_id":"user_1"#));
        assert!(json.contains(r#""client_id":"uuid-1234"#));
        assert!(json.contains(r#""message_type":2"#));
        assert!(json.contains(r#""message_state":2"#));
        assert!(json.contains(r#""type":1"#));
        assert!(json.contains(r#""text":"Hello"#));
        assert!(json.contains(r#""context_token":"ctx_abc"#));
        assert!(json.contains(r#""base_info":{}"#));
    }

    #[test]
    fn serialize_send_message_no_context_token() {
        let req = SendMessageRequest {
            msg: SendMessageMsg {
                to_user_id: "user_1".into(),
                client_id: "uuid-1234".into(),
                message_type: 2,
                message_state: 2,
                item_list: vec![SendMessageItem {
                    item_type: ITEM_TYPE_TEXT,
                    text_item: Some(SendTextItem { text: "Hi".into() }),
                    image_item: None,
                    file_item: None,
                }],
                context_token: None,
            },
            base_info: serde_json::json!({}),
        };
        let json = serde_json::to_string(&req).unwrap();
        assert!(!json.contains("context_token"));
    }

    #[test]
    fn serialize_sse_qr_event() {
        let evt = SseQrEvent {
            qrcode_data: "ticket_abc".into(),
        };
        let json = serde_json::to_string(&evt).unwrap();
        assert!(json.contains(r#""qrcodeData":"ticket_abc"#));
    }

    #[test]
    fn serialize_sse_done_event() {
        let evt = SseDoneEvent {
            account_id: "acc_1".into(),
            bot_token: "tok_1".into(),
            base_url: "https://example.com".into(),
        };
        let json = serde_json::to_string(&evt).unwrap();
        assert!(json.contains(r#""accountId":"acc_1"#));
        assert!(json.contains(r#""botToken":"tok_1"#));
        assert!(json.contains(r#""baseUrl":"https://example.com"#));
    }

    #[test]
    fn serialize_sse_error_event() {
        let evt = SseErrorEvent {
            message: "timeout".into(),
        };
        let json = serde_json::to_string(&evt).unwrap();
        assert!(json.contains(r#""message":"timeout"#));
    }

    #[test]
    fn item_type_constants() {
        assert_eq!(ITEM_TYPE_TEXT, 1);
        assert_eq!(ITEM_TYPE_IMAGE, 2);
        assert_eq!(ITEM_TYPE_VOICE, 3);
        assert_eq!(ITEM_TYPE_FILE, 4);
    }
}

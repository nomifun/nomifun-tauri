mod common;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::json;
use tower::ServiceExt;
use wiremock::matchers::{body_string_contains, header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use common::{body_json, build_app_with_noop_opener, json_with_token, setup_and_login};

// ---------------------------------------------------------------------------
// Helper: build multipart/form-data body
// ---------------------------------------------------------------------------

struct MultipartBuilder {
    boundary: String,
    parts: Vec<u8>,
}

impl MultipartBuilder {
    fn new() -> Self {
        Self {
            boundary: "----TestBoundary7MA4YWxkTrZu0gW".to_owned(),
            parts: Vec::new(),
        }
    }

    fn add_text(mut self, name: &str, value: &str) -> Self {
        self.parts
            .extend_from_slice(format!("--{}\r\n", self.boundary).as_bytes());
        self.parts
            .extend_from_slice(format!("Content-Disposition: form-data; name=\"{name}\"\r\n\r\n").as_bytes());
        self.parts.extend_from_slice(value.as_bytes());
        self.parts.extend_from_slice(b"\r\n");
        self
    }

    fn add_file(mut self, name: &str, filename: &str, mime: &str, data: &[u8]) -> Self {
        self.parts
            .extend_from_slice(format!("--{}\r\n", self.boundary).as_bytes());
        self.parts.extend_from_slice(
            format!("Content-Disposition: form-data; name=\"{name}\"; filename=\"{filename}\"\r\n").as_bytes(),
        );
        self.parts
            .extend_from_slice(format!("Content-Type: {mime}\r\n\r\n").as_bytes());
        self.parts.extend_from_slice(data);
        self.parts.extend_from_slice(b"\r\n");
        self
    }

    fn build(mut self) -> (String, Vec<u8>) {
        self.parts
            .extend_from_slice(format!("--{}--\r\n", self.boundary).as_bytes());
        let content_type = format!("multipart/form-data; boundary={}", self.boundary);
        (content_type, self.parts)
    }
}

fn multipart_request(uri: &str, content_type: &str, body: Vec<u8>, token: &str, csrf: &str) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri(uri)
        .header("content-type", content_type)
        .header("authorization", format!("Bearer {token}"))
        .header("x-csrf-token", csrf)
        .header("cookie", format!("nomifun-csrf-token={csrf}"))
        .body(Body::from(body))
        .unwrap()
}

async fn set_stt_config(app: &mut axum::Router, token: &str, csrf: &str, config: serde_json::Value) {
    let req = json_with_token(
        "PUT",
        "/api/settings/client",
        json!({ "tools.speechToText": config }),
        token,
        csrf,
    );
    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
}

async fn create_provider(
    app: &mut axum::Router,
    token: &str,
    csrf: &str,
    name: &str,
    base_url: &str,
    api_key: &str,
    models: &[&str],
) -> String {
    let req = json_with_token(
        "POST",
        "/api/providers",
        json!({
            "platform": "custom",
            "name": name,
            "base_url": base_url,
            "api_key": api_key,
            "models": models,
            "enabled": true,
            "capabilities": []
        }),
        token,
        csrf,
    );
    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);
    let body = body_json(resp).await;
    body["data"]["id"]
        .as_str()
        .expect("created provider has a canonical id")
        .to_owned()
}

// ===========================================================================
// A. Shell Operations
// ===========================================================================

// SH-2: open-file — file not found
#[tokio::test]
async fn sh2_open_file_not_found() {
    let (mut app, services) = build_app_with_noop_opener().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;

    let req = json_with_token(
        "POST",
        "/api/shell/open-file",
        json!({ "file_path": "/nonexistent/file.txt" }),
        &token,
        &csrf,
    );
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let json = body_json(resp).await;
    assert_eq!(json["success"], false);
}

// SH-4: show-item-in-folder — path not found
#[tokio::test]
async fn sh4_show_item_in_folder_not_found() {
    let (mut app, services) = build_app_with_noop_opener().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;

    let req = json_with_token(
        "POST",
        "/api/shell/show-item-in-folder",
        json!({ "file_path": "/nonexistent/path" }),
        &token,
        &csrf,
    );
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

// SH-6: open-external — command injection attempt
#[tokio::test]
async fn sh6_open_external_command_injection() {
    let (mut app, services) = build_app_with_noop_opener().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;

    let req = json_with_token(
        "POST",
        "/api/shell/open-external",
        json!({ "url": "; rm -rf /" }),
        &token,
        &csrf,
    );
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let json = body_json(resp).await;
    assert_eq!(json["success"], false);
}

// SH-7: open-external — disallowed scheme
#[tokio::test]
async fn sh7_open_external_file_scheme() {
    let (mut app, services) = build_app_with_noop_opener().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;

    let req = json_with_token(
        "POST",
        "/api/shell/open-external",
        json!({ "url": "file:///etc/passwd" }),
        &token,
        &csrf,
    );
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

// SH-8: check-tool-installed — terminal always true
#[tokio::test]
async fn sh8_check_tool_terminal() {
    let (mut app, services) = build_app_with_noop_opener().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;

    let req = json_with_token(
        "POST",
        "/api/shell/check-tool-installed",
        json!({ "tool": "terminal" }),
        &token,
        &csrf,
    );
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let json = body_json(resp).await;
    assert_eq!(json["success"], true);
    assert_eq!(json["data"]["installed"], true);
}

// SH-9: check-tool-installed — explorer always true
#[tokio::test]
async fn sh9_check_tool_explorer() {
    let (mut app, services) = build_app_with_noop_opener().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;

    let req = json_with_token(
        "POST",
        "/api/shell/check-tool-installed",
        json!({ "tool": "explorer" }),
        &token,
        &csrf,
    );
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let json = body_json(resp).await;
    assert_eq!(json["success"], true);
    assert_eq!(json["data"]["installed"], true);
}

// SH-10: check-tool-installed — vscode (result depends on environment)
#[tokio::test]
async fn sh10_check_tool_vscode() {
    let (mut app, services) = build_app_with_noop_opener().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;

    let req = json_with_token(
        "POST",
        "/api/shell/check-tool-installed",
        json!({ "tool": "vscode" }),
        &token,
        &csrf,
    );
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let json = body_json(resp).await;
    assert_eq!(json["success"], true);
    assert!(json["data"]["installed"].is_boolean());
}

// SH-12: open-folder-with — directory not found
#[tokio::test]
async fn sh12_open_folder_with_nonexistent() {
    let (mut app, services) = build_app_with_noop_opener().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;

    let req = json_with_token(
        "POST",
        "/api/shell/open-folder-with",
        json!({ "folder_path": "/nonexistent/dir", "tool": "explorer" }),
        &token,
        &csrf,
    );
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

// SH-13: open-file — missing filePath
#[tokio::test]
async fn sh13_open_file_missing_field() {
    let (mut app, services) = build_app_with_noop_opener().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;

    let req = json_with_token("POST", "/api/shell/open-file", json!({}), &token, &csrf);
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

// SH-14: open-external — empty URL
#[tokio::test]
async fn sh14_open_external_empty_url() {
    let (mut app, services) = build_app_with_noop_opener().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;

    let req = json_with_token("POST", "/api/shell/open-external", json!({ "url": "" }), &token, &csrf);
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

// ===========================================================================
// B. Speech-to-Text (STT)
// ===========================================================================

// ST-3: STT not enabled
#[tokio::test]
async fn st3_stt_disabled() {
    let (mut app, services) = build_app_with_noop_opener().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;

    set_stt_config(
        &mut app,
        &token,
        &csrf,
        json!({ "enabled": false, "provider": "openai" }),
    )
    .await;

    let (content_type, body) = MultipartBuilder::new()
        .add_file("file", "test.wav", "audio/wav", b"fake audio data")
        .add_text("fileName", "test.wav")
        .add_text("mimeType", "audio/wav")
        .build();

    let req = multipart_request("/api/stt", &content_type, body, &token, &csrf);
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let json = body_json(resp).await;
    assert_eq!(json["code"], "STT_DISABLED");
}

// ST-4: STT config not set (treated as disabled)
#[tokio::test]
async fn st4_stt_config_not_set() {
    let (mut app, services) = build_app_with_noop_opener().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;

    let (content_type, body) = MultipartBuilder::new()
        .add_file("file", "test.wav", "audio/wav", b"fake audio data")
        .add_text("fileName", "test.wav")
        .add_text("mimeType", "audio/wav")
        .build();

    let req = multipart_request("/api/stt", &content_type, body, &token, &csrf);
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let json = body_json(resp).await;
    assert_eq!(json["code"], "STT_DISABLED");
}

// ST-5: OpenAI not configured (missing API key)
#[tokio::test]
async fn st5_openai_not_configured() {
    let (mut app, services) = build_app_with_noop_opener().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;

    set_stt_config(
        &mut app,
        &token,
        &csrf,
        json!({
            "enabled": true,
            "provider": "openai",
            "openai": { "api_key": "", "model": "whisper-1" }
        }),
    )
    .await;

    let (content_type, body) = MultipartBuilder::new()
        .add_file("file", "test.wav", "audio/wav", b"fake audio data")
        .add_text("fileName", "test.wav")
        .add_text("mimeType", "audio/wav")
        .build();

    let req = multipart_request("/api/stt", &content_type, body, &token, &csrf);
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let json = body_json(resp).await;
    assert_eq!(json["code"], "STT_OPENAI_NOT_CONFIGURED");
}

// ST-6: Deepgram not configured (missing API key)
#[tokio::test]
async fn st6_deepgram_not_configured() {
    let (mut app, services) = build_app_with_noop_opener().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;

    set_stt_config(
        &mut app,
        &token,
        &csrf,
        json!({
            "enabled": true,
            "provider": "deepgram",
            "deepgram": { "api_key": "", "model": "nova-2" }
        }),
    )
    .await;

    let (content_type, body) = MultipartBuilder::new()
        .add_file("file", "test.wav", "audio/wav", b"fake audio data")
        .add_text("fileName", "test.wav")
        .add_text("mimeType", "audio/wav")
        .build();

    let req = multipart_request("/api/stt", &content_type, body, &token, &csrf);
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let json = body_json(resp).await;
    assert_eq!(json["code"], "STT_DEEPGRAM_NOT_CONFIGURED");
}

// ST-7: STT third-party API failure (fake API key → 401)
#[tokio::test]
async fn st7_stt_api_failure() {
    let mock_server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/audio/transcriptions"))
        .respond_with(ResponseTemplate::new(401).set_body_string("Unauthorized"))
        .mount(&mock_server)
        .await;

    let (mut app, services) = build_app_with_noop_opener().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;

    set_stt_config(
        &mut app,
        &token,
        &csrf,
        json!({
            "enabled": true,
            "provider": "openai",
            "openai": {
                "api_key": "sk-fake-key",
                "base_url": mock_server.uri(),
                "model": "whisper-1"
            }
        }),
    )
    .await;

    let (content_type, body) = MultipartBuilder::new()
        .add_file("file", "test.wav", "audio/wav", b"fake audio data")
        .add_text("fileName", "test.wav")
        .add_text("mimeType", "audio/wav")
        .build();

    let req = multipart_request("/api/stt", &content_type, body, &token, &csrf);
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_GATEWAY);
    let json = body_json(resp).await;
    assert_eq!(json["code"], "STT_REQUEST_FAILED");
}

// ST-8: multipart missing fileName
#[tokio::test]
async fn st8_multipart_missing_filename() {
    let (mut app, services) = build_app_with_noop_opener().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;

    set_stt_config(
        &mut app,
        &token,
        &csrf,
        json!({ "enabled": true, "provider": "openai", "openai": { "api_key": "sk-test", "model": "whisper-1" } }),
    )
    .await;

    let (content_type, body) = MultipartBuilder::new()
        .add_file("file", "test.wav", "audio/wav", b"fake audio data")
        .add_text("mimeType", "audio/wav")
        .build();

    let req = multipart_request("/api/stt", &content_type, body, &token, &csrf);
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let json = body_json(resp).await;
    assert_eq!(json["success"], false);
}

// ST-9: multipart missing file
#[tokio::test]
async fn st9_multipart_missing_file() {
    let (mut app, services) = build_app_with_noop_opener().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;

    set_stt_config(
        &mut app,
        &token,
        &csrf,
        json!({ "enabled": true, "provider": "openai", "openai": { "api_key": "sk-test", "model": "whisper-1" } }),
    )
    .await;

    let (content_type, body) = MultipartBuilder::new()
        .add_text("fileName", "test.wav")
        .add_text("mimeType", "audio/wav")
        .build();

    let req = multipart_request("/api/stt", &content_type, body, &token, &csrf);
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let json = body_json(resp).await;
    assert_eq!(json["success"], false);
}

// ST-1: OpenAI transcription success (mocked)
#[tokio::test]
async fn st1_openai_transcription_success() {
    let mock_server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/audio/transcriptions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "text": "hello world" })))
        .mount(&mock_server)
        .await;

    let (mut app, services) = build_app_with_noop_opener().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;

    set_stt_config(
        &mut app,
        &token,
        &csrf,
        json!({
            "enabled": true,
            "provider": "openai",
            "openai": {
                "api_key": "sk-test-key",
                "base_url": mock_server.uri(),
                "model": "whisper-1"
            }
        }),
    )
    .await;

    let (content_type, body) = MultipartBuilder::new()
        .add_file("file", "test.wav", "audio/wav", b"fake audio data")
        .add_text("fileName", "test.wav")
        .add_text("mimeType", "audio/wav")
        .build();

    let req = multipart_request("/api/stt", &content_type, body, &token, &csrf);
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let json = body_json(resp).await;
    assert_eq!(json["success"], true);
    assert_eq!(json["data"]["text"], "hello world");
    assert_eq!(json["data"]["model"], "whisper-1");
    assert_eq!(json["data"]["provider"], "openai");
}

// ST-2: Deepgram transcription success (mocked)
#[tokio::test]
async fn st2_deepgram_transcription_success() {
    let mock_server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "metadata": {
                "model_info": {
                    "key": { "name": "nova-2-general" }
                }
            },
            "results": {
                "channels": [{
                    "alternatives": [{
                        "transcript": "hello from deepgram"
                    }]
                }]
            }
        })))
        .mount(&mock_server)
        .await;

    let (mut app, services) = build_app_with_noop_opener().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;

    set_stt_config(
        &mut app,
        &token,
        &csrf,
        json!({
            "enabled": true,
            "provider": "deepgram",
            "deepgram": {
                "api_key": "dg-test-key",
                "base_url": mock_server.uri(),
                "model": "nova-2"
            }
        }),
    )
    .await;

    let (content_type, body) = MultipartBuilder::new()
        .add_file("file", "test.wav", "audio/wav", b"fake audio data")
        .add_text("fileName", "test.wav")
        .add_text("mimeType", "audio/wav")
        .build();

    let req = multipart_request("/api/stt", &content_type, body, &token, &csrf);
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let json = body_json(resp).await;
    assert_eq!(json["success"], true);
    assert_eq!(json["data"]["text"], "hello from deepgram");
    assert_eq!(json["data"]["provider"], "deepgram");
}

#[tokio::test]
async fn st11_configured_provider_is_resolved_by_id() {
    let mock_server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/audio/transcriptions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "text": "provider reference works" })))
        .mount(&mock_server)
        .await;

    let (mut app, services) = build_app_with_noop_opener().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;
    let provider_id = create_provider(
        &mut app,
        &token,
        &csrf,
        "Speech Provider",
        &mock_server.uri(),
        "sk-from-provider",
        &["whisper-1"],
    )
    .await;
    set_stt_config(
        &mut app,
        &token,
        &csrf,
        json!({
            "enabled": true,
            "provider": "openai",
            "provider_id": provider_id,
            "model": "whisper-1",
            "language": "en"
        }),
    )
    .await;

    let (content_type, body) = MultipartBuilder::new()
        .add_file("file", "test.wav", "audio/wav", b"fake audio data")
        .add_text("fileName", "test.wav")
        .add_text("mimeType", "audio/wav")
        .build();

    let req = multipart_request("/api/stt", &content_type, body, &token, &csrf);
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    assert_eq!(body["data"]["text"], "provider reference works");
    assert_eq!(body["data"]["model"], "whisper-1");
}

#[tokio::test]
async fn st12_stepfun_provider_uses_single_version_prefix_and_required_form_fields() {
    let mock_server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/audio/transcriptions"))
        .and(header("Authorization", "Bearer sk-stepfun-provider"))
        .and(body_string_contains("name=\"model\""))
        .and(body_string_contains("step-asr"))
        .and(body_string_contains("name=\"response_format\""))
        .and(body_string_contains("json"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "text": "阶跃端到端转写成功" })))
        .expect(1)
        .mount(&mock_server)
        .await;

    let (mut app, services) = build_app_with_noop_opener().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;
    let provider_id = create_provider(
        &mut app,
        &token,
        &csrf,
        "StepFun",
        &format!("{}/v1", mock_server.uri()),
        "sk-stepfun-provider",
        &["step-asr"],
    )
    .await;
    set_stt_config(
        &mut app,
        &token,
        &csrf,
        json!({
            "enabled": true,
            "provider": "openai",
            "provider_id": provider_id,
            "model": "step-asr"
        }),
    )
    .await;

    let (content_type, body) = MultipartBuilder::new()
        .add_file("file", "speech.wav", "audio/wav", b"fake stepfun audio")
        .add_text("fileName", "speech.wav")
        .add_text("mimeType", "audio/wav")
        .build();

    let req = multipart_request("/api/stt", &content_type, body, &token, &csrf);
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    assert_eq!(body["data"]["text"], "阶跃端到端转写成功");
    assert_eq!(body["data"]["model"], "step-asr");
}

// ST-10: languageHint passed through
#[tokio::test]
async fn st10_language_hint_passed() {
    let mock_server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/audio/transcriptions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "text": "你好世界" })))
        .mount(&mock_server)
        .await;

    let (mut app, services) = build_app_with_noop_opener().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;

    set_stt_config(
        &mut app,
        &token,
        &csrf,
        json!({
            "enabled": true,
            "provider": "openai",
            "openai": {
                "api_key": "sk-test-key",
                "base_url": mock_server.uri(),
                "model": "whisper-1"
            }
        }),
    )
    .await;

    let (content_type, body) = MultipartBuilder::new()
        .add_file("file", "test.wav", "audio/wav", b"fake audio data")
        .add_text("fileName", "test.wav")
        .add_text("mimeType", "audio/wav")
        .add_text("languageHint", "zh")
        .build();

    let req = multipart_request("/api/stt", &content_type, body, &token, &csrf);
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let json = body_json(resp).await;
    assert_eq!(json["success"], true);
    assert_eq!(json["data"]["text"], "你好世界");
}

// ===========================================================================
// C. Authentication
// ===========================================================================

// AU-1: unauthenticated shell request rejected
#[tokio::test]
async fn au1_shell_unauthenticated() {
    let (app, _services) = build_app_with_noop_opener().await;

    let req = Request::builder()
        .method("POST")
        .uri("/api/shell/open-file")
        .header("content-type", "application/json")
        .body(Body::from(r#"{"file_path":"/tmp/test.txt"}"#))
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
}

// AU-2: unauthenticated STT request rejected
#[tokio::test]
async fn au2_stt_unauthenticated() {
    let (app, _services) = build_app_with_noop_opener().await;

    let (content_type, body) = MultipartBuilder::new()
        .add_file("file", "test.wav", "audio/wav", b"fake audio")
        .add_text("fileName", "test.wav")
        .add_text("mimeType", "audio/wav")
        .build();

    let req = Request::builder()
        .method("POST")
        .uri("/api/stt")
        .header("content-type", content_type)
        .body(Body::from(body))
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
}

// M-145: mailto scheme URL positive test
#[tokio::test]
async fn sh_open_external_mailto_scheme() {
    let (mut app, services) = build_app_with_noop_opener().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;

    let req = json_with_token(
        "POST",
        "/api/shell/open-external",
        json!({ "url": "mailto:user@example.com" }),
        &token,
        &csrf,
    );
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
}

// M-147: multipart missing mimeType field
#[tokio::test]
async fn st_multipart_missing_mimetype() {
    let (mut app, services) = build_app_with_noop_opener().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;

    set_stt_config(
        &mut app,
        &token,
        &csrf,
        json!({ "enabled": true, "provider": "openai", "openai": { "api_key": "sk-test", "model": "whisper-1" } }),
    )
    .await;

    let (content_type, body) = MultipartBuilder::new()
        .add_file("file", "test.wav", "audio/wav", b"fake audio data")
        .add_text("fileName", "test.wav")
        .build();

    let req = multipart_request("/api/stt", &content_type, body, &token, &csrf);
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let json = body_json(resp).await;
    assert_eq!(json["success"], false);
}

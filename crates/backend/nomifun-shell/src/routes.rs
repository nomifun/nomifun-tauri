use axum::extract::{DefaultBodyLimit, Multipart, State};
use axum::http::StatusCode;
use axum::routing::post;
use axum::{Json, Router};
use tower_http::limit::RequestBodyLimitLayer;

use nomifun_api_types::{
    ApiResponse, CheckToolInstalledRequest, CheckToolInstalledResponse, ClientPreferencesResponse,
    DeepgramSpeechToTextConfig, OpenAISpeechToTextConfig, OpenExternalRequest, OpenFileRequest,
    OpenFolderWithRequest, ShowItemInFolderRequest, SpeechToTextConfig, SpeechToTextProvider,
};
use nomifun_common::AppError;

use crate::error::SttError;
use crate::state::ShellRouterState;

pub fn shell_routes(state: ShellRouterState) -> Router {
    let shell = Router::new()
        .route("/api/shell/open-file", post(open_file))
        .route("/api/shell/show-item-in-folder", post(show_item_in_folder))
        .route("/api/shell/open-external", post(open_external))
        .route("/api/shell/check-tool-installed", post(check_tool_installed))
        .route("/api/shell/open-folder-with", post(open_folder_with));
    let stt = Router::new()
        .route("/api/stt", post(speech_to_text))
        // Disable the application's 10 MiB extractor default, then make the
        // transport layer the sole cap: 30 MiB audio plus multipart overhead.
        .layer(DefaultBodyLimit::disable())
        .layer(RequestBodyLimitLayer::new(31 * 1024 * 1024));
    shell.merge(stt).with_state(state)
}

async fn open_file(
    State(state): State<ShellRouterState>,
    body: Result<Json<OpenFileRequest>, axum::extract::rejection::JsonRejection>,
) -> Result<Json<ApiResponse<()>>, AppError> {
    let Json(req) = body.map_err(|e| AppError::BadRequest(e.to_string()))?;
    state.shell_service.open_file(&req.file_path).await?;
    Ok(Json(ApiResponse::success()))
}

async fn show_item_in_folder(
    State(state): State<ShellRouterState>,
    body: Result<Json<ShowItemInFolderRequest>, axum::extract::rejection::JsonRejection>,
) -> Result<Json<ApiResponse<()>>, AppError> {
    let Json(req) = body.map_err(|e| AppError::BadRequest(e.to_string()))?;
    state.shell_service.show_item_in_folder(&req.file_path).await?;
    Ok(Json(ApiResponse::success()))
}

async fn open_external(
    State(state): State<ShellRouterState>,
    body: Result<Json<OpenExternalRequest>, axum::extract::rejection::JsonRejection>,
) -> Result<Json<ApiResponse<()>>, AppError> {
    let Json(req) = body.map_err(|e| AppError::BadRequest(e.to_string()))?;
    state.shell_service.open_external(&req.url).await?;
    Ok(Json(ApiResponse::success()))
}

async fn check_tool_installed(
    State(state): State<ShellRouterState>,
    body: Result<Json<CheckToolInstalledRequest>, axum::extract::rejection::JsonRejection>,
) -> Result<Json<ApiResponse<CheckToolInstalledResponse>>, AppError> {
    let Json(req) = body.map_err(|e| AppError::BadRequest(e.to_string()))?;
    let installed = state.shell_service.check_tool_installed(req.tool).await;
    Ok(Json(ApiResponse::ok(CheckToolInstalledResponse { installed })))
}

async fn open_folder_with(
    State(state): State<ShellRouterState>,
    body: Result<Json<OpenFolderWithRequest>, axum::extract::rejection::JsonRejection>,
) -> Result<Json<ApiResponse<()>>, AppError> {
    let Json(req) = body.map_err(|e| AppError::BadRequest(e.to_string()))?;
    state.shell_service.open_folder_with(&req.folder_path, req.tool).await?;
    Ok(Json(ApiResponse::success()))
}

struct SttMultipartFields {
    file_data: Vec<u8>,
    file_name: String,
    mime_type: String,
    language_hint: Option<String>,
}

async fn extract_stt_multipart(mut multipart: Multipart) -> Result<SttMultipartFields, AppError> {
    let mut file_data: Option<Vec<u8>> = None;
    let mut file_name: Option<String> = None;
    let mut mime_type: Option<String> = None;
    let mut language_hint: Option<String> = None;

    while let Some(field) = multipart
        .next_field()
        .await
        .map_err(|e| AppError::BadRequest(format!("multipart error: {e}")))?
    {
        let name = field.name().unwrap_or("").to_owned();
        match name.as_str() {
            "file" => {
                file_data = Some(
                    field
                        .bytes()
                        .await
                        .map_err(|e| AppError::BadRequest(format!("failed to read file: {e}")))?
                        .to_vec(),
                );
            }
            "fileName" => {
                file_name = Some(
                    field
                        .text()
                        .await
                        .map_err(|e| AppError::BadRequest(format!("failed to read fileName: {e}")))?,
                );
            }
            "mimeType" => {
                mime_type = Some(
                    field
                        .text()
                        .await
                        .map_err(|e| AppError::BadRequest(format!("failed to read mimeType: {e}")))?,
                );
            }
            "languageHint" => {
                let text = field
                    .text()
                    .await
                    .map_err(|e| AppError::BadRequest(format!("failed to read languageHint: {e}")))?;
                if !text.is_empty() {
                    language_hint = Some(text);
                }
            }
            _ => {}
        }
    }

    let file_data = file_data.ok_or_else(|| AppError::BadRequest("missing 'file' field".to_owned()))?;
    let file_name = file_name.ok_or_else(|| AppError::BadRequest("missing 'fileName' field".to_owned()))?;
    let mime_type = mime_type.ok_or_else(|| AppError::BadRequest("missing 'mimeType' field".to_owned()))?;

    Ok(SttMultipartFields {
        file_data,
        file_name,
        mime_type,
        language_hint,
    })
}

async fn speech_to_text(
    State(state): State<ShellRouterState>,
    multipart: Multipart,
) -> Result<(StatusCode, Json<serde_json::Value>), (StatusCode, Json<serde_json::Value>)> {
    let fields = extract_stt_multipart(multipart).await.map_err(|e| {
        let status = e.status_code();
        let body = serde_json::json!({
            "success": false,
            "error": e.to_string(),
            "code": e.error_code(),
        });
        (status, Json(body))
    })?;

    let prefs = state
        .client_pref_service
        .get_preferences(Some(&["tools.speechToText", "speechToText"]))
        .await
        .map_err(|e| {
            let status = e.status_code();
            let body = serde_json::json!({
                "success": false,
                "error": e.to_string(),
                "code": e.error_code(),
            });
            (status, Json(body))
        })?;

    let config = speech_to_text_config_from_preferences(&prefs);

    let config = resolve_cloud_speech_to_text_config(&state, config)
        .await
        .map_err(|error| stt_error_response(&error))?;

    let result = state
        .stt_service
        .transcribe(
            fields.file_data,
            &fields.file_name,
            &fields.mime_type,
            config
                .language
                .as_deref()
                .or(fields.language_hint.as_deref()),
            &config,
        )
        .await
        .map_err(|e| stt_error_response(&e))?;

    let body = serde_json::json!({
        "success": true,
        "data": result,
    });
    Ok((StatusCode::OK, Json(body)))
}

fn speech_to_text_config_from_preferences(prefs: &ClientPreferencesResponse) -> SpeechToTextConfig {
    ["tools.speechToText", "speechToText"]
        .into_iter()
        .filter_map(|key| prefs.get(key))
        .find_map(|value| serde_json::from_value(value.clone()).ok())
        .unwrap_or(SpeechToTextConfig {
            enabled: false,
            provider: SpeechToTextProvider::Openai,
            provider_id: None,
            model: None,
            language: None,
            auto_send: None,
            openai: None,
            deepgram: None,
        })
}

async fn resolve_cloud_speech_to_text_config(
    state: &ShellRouterState,
    config: SpeechToTextConfig,
) -> Result<SpeechToTextConfig, SttError> {
    if !config.enabled {
        return Ok(config);
    }
    let Some(provider_id) = config.provider_id.as_deref() else {
        return Ok(config);
    };
    let Some(provider_service) = state.provider_service.as_ref() else {
        return Err(SttError::Unknown(
            "provider service is unavailable for speech recognition".into(),
        ));
    };
    let provider = provider_service
        .list()
        .await
        .map_err(|error| SttError::Unknown(error.to_string()))?
        .into_iter()
        .find(|provider| provider.id == provider_id && provider.enabled)
        .ok_or_else(|| SttError::Unknown("selected speech provider was not found or is disabled".into()))?;
    if provider.api_key.trim().is_empty() {
        return Err(match config.provider {
            SpeechToTextProvider::Openai => SttError::OpenaiNotConfigured,
            SpeechToTextProvider::Deepgram => SttError::DeepgramNotConfigured,
        });
    }

    let model = config
        .model
        .clone()
        .ok_or_else(|| SttError::Unknown("selected speech provider has no selected speech model".into()))?;
    let model_is_enabled = provider
        .model_enabled
        .as_ref()
        .and_then(|models| models.get(&model))
        .copied()
        .unwrap_or(true);
    if !provider.models.contains(&model) || !model_is_enabled {
        return Err(SttError::Unknown(
            "selected speech model was not found or is disabled".into(),
        ));
    }
    let language = config.language.clone().filter(|value| !value.trim().is_empty());

    Ok(match config.provider {
        SpeechToTextProvider::Openai => SpeechToTextConfig {
            openai: Some(OpenAISpeechToTextConfig {
                api_key: provider.api_key,
                base_url: Some(provider.base_url),
                is_full_url: provider.is_full_url,
                model,
                language: language.clone(),
                prompt: None,
                temperature: None,
            }),
            language,
            ..config
        },
        SpeechToTextProvider::Deepgram => SpeechToTextConfig {
            deepgram: Some(DeepgramSpeechToTextConfig {
                api_key: provider.api_key,
                base_url: Some(provider.base_url),
                model,
                language: language.clone(),
                detect_language: Some(language.is_none()),
                punctuate: Some(true),
                smart_format: Some(true),
            }),
            language,
            ..config
        },
    })
}

fn stt_error_response(err: &SttError) -> (StatusCode, Json<serde_json::Value>) {
    let status = StatusCode::from_u16(err.status_code()).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    let body = serde_json::json!({
        "success": false,
        "error": err.to_string(),
        "code": err.error_code(),
    });
    (status, Json(body))
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::Request;
    use bytes::Bytes;
    use http_body_util::BodyExt;
    use serde_json::json;
    use std::sync::Arc;
    use tower::ServiceExt;

    fn make_state() -> ShellRouterState {
        use crate::opener::NoopSystemOpener;
        use crate::shell::ShellService;
        use crate::stt::SttService;

        let pool = sqlx::SqlitePool::connect_lazy("sqlite::memory:").unwrap();
        let repo = Arc::new(nomifun_db::SqliteClientPreferenceRepository::new(pool));
        let client_pref_service = nomifun_system::ClientPrefService::new(repo);

        ShellRouterState {
            shell_service: Arc::new(ShellService::new(Arc::new(NoopSystemOpener))),
            stt_service: Arc::new(SttService::new(reqwest::Client::new())),
            client_pref_service,
            provider_service: None,
        }
    }

    fn make_router() -> Router {
        shell_routes(make_state())
    }

    async fn body_json(resp: axum::response::Response) -> serde_json::Value {
        let bytes = resp.into_body().collect().await.unwrap().to_bytes();
        serde_json::from_slice(&bytes).unwrap()
    }

    fn multipart_request(audio_len: usize) -> Request<Body> {
        multipart_request_with_format(audio_len, "audio.wav", "audio/wav")
    }

    fn multipart_request_with_format(
        audio_len: usize,
        file_name: &str,
        mime_type: &str,
    ) -> Request<Body> {
        const BOUNDARY: &str = "nomifun-stt-limit-test";
        let prefix = format!(
            "--{BOUNDARY}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"{file_name}\"\r\nContent-Type: {mime_type}\r\n\r\n"
        );
        let suffix = format!(
            "\r\n--{BOUNDARY}\r\nContent-Disposition: form-data; name=\"fileName\"\r\n\r\n{file_name}\r\n--{BOUNDARY}\r\nContent-Disposition: form-data; name=\"mimeType\"\r\n\r\n{mime_type}\r\n--{BOUNDARY}--\r\n"
        );
        let stream = futures_util::stream::iter([
            Ok::<_, std::io::Error>(Bytes::from(prefix)),
            Ok(Bytes::from(vec![0_u8; audio_len])),
            Ok(Bytes::from(suffix)),
        ]);
        Request::builder()
            .method("POST")
            .uri("/api/stt")
            .header(
                "content-type",
                format!("multipart/form-data; boundary={BOUNDARY}"),
            )
            .body(Body::from_stream(stream))
            .unwrap()
    }

    #[test]
    fn speech_to_text_config_prefers_tools_key_and_supports_legacy_key() {
        let legacy = json!({
            "enabled": true,
            "provider": "openai",
            "openai": {
                "api_key": "legacy-key",
                "model": "legacy-model"
            }
        });
        let current = json!({
            "enabled": true,
            "provider": "deepgram",
            "deepgram": {
                "api_key": "current-key",
                "model": "nova-2"
            }
        });

        let legacy_only = ClientPreferencesResponse::from([("speechToText".into(), legacy.clone())]);
        let config = speech_to_text_config_from_preferences(&legacy_only);
        assert!(matches!(
            config.provider,
            nomifun_api_types::SpeechToTextProvider::Openai
        ));
        assert_eq!(
            config.openai.as_ref().map(|value| value.api_key.as_str()),
            Some("legacy-key")
        );

        let both = ClientPreferencesResponse::from([
            ("speechToText".into(), legacy),
            ("tools.speechToText".into(), current),
        ]);
        let config = speech_to_text_config_from_preferences(&both);
        assert!(matches!(
            config.provider,
            nomifun_api_types::SpeechToTextProvider::Deepgram
        ));
        assert_eq!(
            config.deepgram.as_ref().map(|value| value.api_key.as_str()),
            Some("current-key")
        );
    }

    #[test]
    fn invalid_current_speech_to_text_config_falls_back_to_legacy_key() {
        let prefs = ClientPreferencesResponse::from([
            ("tools.speechToText".into(), json!({"enabled": true})),
            (
                "speechToText".into(),
                json!({
                    "enabled": true,
                    "provider": "openai",
                    "openai": {
                        "api_key": "legacy-key",
                        "model": "whisper-1"
                    }
                }),
            ),
        ]);

        let config = speech_to_text_config_from_preferences(&prefs);
        assert!(matches!(
            config.provider,
            nomifun_api_types::SpeechToTextProvider::Openai
        ));
        assert_eq!(
            config.openai.as_ref().map(|value| value.api_key.as_str()),
            Some("legacy-key")
        );
    }

    #[tokio::test]
    async fn stt_route_accepts_body_larger_than_global_ten_mib_limit() {
        let response = make_router()
            .oneshot(multipart_request(10 * 1024 * 1024 + 1))
            .await
            .unwrap();
        assert_ne!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
        // The lazy in-memory preference repository used by this unit test can
        // return 500 before configuration lookup; reaching that handler is the
        // contract under test (the transport did not reject at 10 MiB).
        assert!(matches!(
            response.status(),
            StatusCode::BAD_REQUEST | StatusCode::INTERNAL_SERVER_ERROR
        ));
    }

    #[tokio::test]
    async fn open_file_missing_body_returns_400() {
        let app = make_router();
        let req = Request::builder()
            .method("POST")
            .uri("/api/shell/open-file")
            .header("content-type", "application/json")
            .body(Body::from("{}"))
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn open_file_nonexistent_returns_400() {
        let app = make_router();
        let req = Request::builder()
            .method("POST")
            .uri("/api/shell/open-file")
            .header("content-type", "application/json")
            .body(Body::from(r#"{"filePath":"/nonexistent/file.txt"}"#))
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        let json = body_json(resp).await;
        assert_eq!(json["success"], false);
    }

    #[tokio::test]
    async fn open_external_invalid_url_returns_400() {
        let app = make_router();
        let req = Request::builder()
            .method("POST")
            .uri("/api/shell/open-external")
            .header("content-type", "application/json")
            .body(Body::from(r#"{"url":"; rm -rf /"}"#))
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn open_external_file_scheme_returns_400() {
        let app = make_router();
        let req = Request::builder()
            .method("POST")
            .uri("/api/shell/open-external")
            .header("content-type", "application/json")
            .body(Body::from(r#"{"url":"file:///etc/passwd"}"#))
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn check_tool_terminal_returns_installed_true() {
        let app = make_router();
        let req = Request::builder()
            .method("POST")
            .uri("/api/shell/check-tool-installed")
            .header("content-type", "application/json")
            .body(Body::from(r#"{"tool":"terminal"}"#))
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let json = body_json(resp).await;
        assert_eq!(json["success"], true);
        assert_eq!(json["data"]["installed"], true);
    }

    #[tokio::test]
    async fn check_tool_explorer_returns_installed_true() {
        let app = make_router();
        let req = Request::builder()
            .method("POST")
            .uri("/api/shell/check-tool-installed")
            .header("content-type", "application/json")
            .body(Body::from(r#"{"tool":"explorer"}"#))
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let json = body_json(resp).await;
        assert_eq!(json["success"], true);
        assert_eq!(json["data"]["installed"], true);
    }

    #[tokio::test]
    async fn open_folder_with_nonexistent_dir_returns_400() {
        let app = make_router();
        let req = Request::builder()
            .method("POST")
            .uri("/api/shell/open-folder-with")
            .header("content-type", "application/json")
            .body(Body::from(r#"{"folderPath":"/nonexistent/dir","tool":"explorer"}"#))
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn show_item_in_folder_nonexistent_returns_400() {
        let app = make_router();
        let req = Request::builder()
            .method("POST")
            .uri("/api/shell/show-item-in-folder")
            .header("content-type", "application/json")
            .body(Body::from(r#"{"filePath":"/nonexistent/path"}"#))
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }
}

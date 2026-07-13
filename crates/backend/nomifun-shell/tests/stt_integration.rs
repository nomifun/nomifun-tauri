use nomifun_api_types::{
    DeepgramSpeechToTextConfig, OpenAISpeechToTextConfig, SpeechToTextConfig, SpeechToTextProvider,
};
use nomifun_shell::{SttError, SttService};
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn dummy_audio() -> Vec<u8> {
    vec![0u8; 64]
}

fn stt_service() -> SttService {
    let client = reqwest::Client::builder()
        .no_proxy()
        .build()
        .expect("test HTTP client should build");
    SttService::new(client)
}

// ---------------------------------------------------------------------------
// ST-1: OpenAI transcription — success
// ---------------------------------------------------------------------------
#[tokio::test]
async fn st1_openai_transcribe_success() {
    let mock_server = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path("/v1/audio/transcriptions"))
        .and(header("Authorization", "Bearer sk-test-key"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({ "text": "hello world" })))
        .mount(&mock_server)
        .await;

    let config = SpeechToTextConfig {
        enabled: true,
        provider: SpeechToTextProvider::Openai,
        provider_id: None,
        model: None,
        language: None,
        auto_send: None,
        openai: Some(OpenAISpeechToTextConfig {
            api_key: "sk-test-key".into(),
            base_url: Some(mock_server.uri()),
            model: "whisper-1".into(),
            language: None,
            prompt: None,
            temperature: None,
        }),
        deepgram: None,
    };

    let result = stt_service()
        .transcribe(dummy_audio(), "test.wav", "audio/wav", None, &config)
        .await
        .unwrap();

    assert_eq!(result.text, "hello world");
    assert_eq!(result.model, "whisper-1");
    assert_eq!(result.provider, SpeechToTextProvider::Openai);
}

// ---------------------------------------------------------------------------
// ST-2: Deepgram transcription — success
// ---------------------------------------------------------------------------
#[tokio::test]
async fn st2_deepgram_transcribe_success() {
    let mock_server = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path("/v1/listen"))
        .and(header("Authorization", "Token dg-test-key"))
        .and(header("Content-Type", "audio/wav"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "metadata": {
                "model_info": {
                    "uuid-1": {
                        "name": "2-general-nova",
                        "version": "2024-01"
                    }
                }
            },
            "results": {
                "channels": [{
                    "detected_language": "en",
                    "alternatives": [{
                        "transcript": "hello deepgram"
                    }]
                }]
            }
        })))
        .mount(&mock_server)
        .await;

    let config = SpeechToTextConfig {
        enabled: true,
        provider: SpeechToTextProvider::Deepgram,
        provider_id: None,
        model: None,
        language: None,
        auto_send: None,
        openai: None,
        deepgram: Some(DeepgramSpeechToTextConfig {
            api_key: "dg-test-key".into(),
            base_url: Some(mock_server.uri()),
            model: "nova-2".into(),
            language: None,
            detect_language: Some(true),
            punctuate: Some(true),
            smart_format: Some(true),
        }),
    };

    let result = stt_service()
        .transcribe(dummy_audio(), "test.wav", "audio/wav", None, &config)
        .await
        .unwrap();

    assert_eq!(result.text, "hello deepgram");
    assert_eq!(result.model, "2-general-nova");
    assert_eq!(result.provider, SpeechToTextProvider::Deepgram);
    assert_eq!(result.language.as_deref(), Some("en"));
}

// ---------------------------------------------------------------------------
// ST-3: STT disabled
// ---------------------------------------------------------------------------
#[tokio::test]
async fn st3_stt_disabled() {
    let config = SpeechToTextConfig {
        enabled: false,
        provider: SpeechToTextProvider::Openai,
        provider_id: None,
        model: None,
        language: None,
        auto_send: None,
        openai: None,
        deepgram: None,
    };

    let result = stt_service()
        .transcribe(dummy_audio(), "test.wav", "audio/wav", None, &config)
        .await;

    assert!(matches!(result, Err(SttError::Disabled)));
}

// ---------------------------------------------------------------------------
// ST-4: STT config missing — treated as disabled at service layer
//       (the handler reads from ClientPrefService; if key is absent, config
//        will have enabled=false or we surface STT_DISABLED upstream)
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// ST-5: OpenAI missing API key
// ---------------------------------------------------------------------------
#[tokio::test]
async fn st5_openai_empty_api_key() {
    let config = SpeechToTextConfig {
        enabled: true,
        provider: SpeechToTextProvider::Openai,
        provider_id: None,
        model: None,
        language: None,
        auto_send: None,
        openai: Some(OpenAISpeechToTextConfig {
            api_key: String::new(),
            base_url: None,
            model: "whisper-1".into(),
            language: None,
            prompt: None,
            temperature: None,
        }),
        deepgram: None,
    };

    let result = stt_service()
        .transcribe(dummy_audio(), "test.wav", "audio/wav", None, &config)
        .await;

    assert!(matches!(result, Err(SttError::OpenaiNotConfigured)));
}

#[tokio::test]
async fn st5b_openai_config_section_missing() {
    let config = SpeechToTextConfig {
        enabled: true,
        provider: SpeechToTextProvider::Openai,
        provider_id: None,
        model: None,
        language: None,
        auto_send: None,
        openai: None,
        deepgram: None,
    };

    let result = stt_service()
        .transcribe(dummy_audio(), "test.wav", "audio/wav", None, &config)
        .await;

    assert!(matches!(result, Err(SttError::OpenaiNotConfigured)));
}

// ---------------------------------------------------------------------------
// ST-6: Deepgram missing API key
// ---------------------------------------------------------------------------
#[tokio::test]
async fn st6_deepgram_empty_api_key() {
    let config = SpeechToTextConfig {
        enabled: true,
        provider: SpeechToTextProvider::Deepgram,
        provider_id: None,
        model: None,
        language: None,
        auto_send: None,
        openai: None,
        deepgram: Some(DeepgramSpeechToTextConfig {
            api_key: String::new(),
            base_url: None,
            model: "nova-2".into(),
            language: None,
            detect_language: None,
            punctuate: None,
            smart_format: None,
        }),
    };

    let result = stt_service()
        .transcribe(dummy_audio(), "test.wav", "audio/wav", None, &config)
        .await;

    assert!(matches!(result, Err(SttError::DeepgramNotConfigured)));
}

#[tokio::test]
async fn st6b_deepgram_config_section_missing() {
    let config = SpeechToTextConfig {
        enabled: true,
        provider: SpeechToTextProvider::Deepgram,
        provider_id: None,
        model: None,
        language: None,
        auto_send: None,
        openai: None,
        deepgram: None,
    };

    let result = stt_service()
        .transcribe(dummy_audio(), "test.wav", "audio/wav", None, &config)
        .await;

    assert!(matches!(result, Err(SttError::DeepgramNotConfigured)));
}

// ---------------------------------------------------------------------------
// ST-7: OpenAI upstream API failure (401)
// ---------------------------------------------------------------------------
#[tokio::test]
async fn st7_openai_upstream_failure() {
    let mock_server = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path("/v1/audio/transcriptions"))
        .respond_with(ResponseTemplate::new(401).set_body_json(serde_json::json!({
            "error": {
                "message": "Incorrect API key provided",
                "type": "invalid_request_error"
            }
        })))
        .mount(&mock_server)
        .await;

    let config = SpeechToTextConfig {
        enabled: true,
        provider: SpeechToTextProvider::Openai,
        provider_id: None,
        model: None,
        language: None,
        auto_send: None,
        openai: Some(OpenAISpeechToTextConfig {
            api_key: "sk-invalid".into(),
            base_url: Some(mock_server.uri()),
            model: "whisper-1".into(),
            language: None,
            prompt: None,
            temperature: None,
        }),
        deepgram: None,
    };

    let result = stt_service()
        .transcribe(dummy_audio(), "test.wav", "audio/wav", None, &config)
        .await;

    match result {
        Err(SttError::RequestFailed(msg)) => {
            assert!(msg.contains("401"), "expected 401 in error: {msg}");
        }
        other => panic!("expected RequestFailed, got: {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// ST-7b: Deepgram upstream API failure (403)
// ---------------------------------------------------------------------------
#[tokio::test]
async fn st7b_deepgram_upstream_failure() {
    let mock_server = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path("/v1/listen"))
        .respond_with(ResponseTemplate::new(403).set_body_json(serde_json::json!({ "err_msg": "Invalid credentials" })))
        .mount(&mock_server)
        .await;

    let config = SpeechToTextConfig {
        enabled: true,
        provider: SpeechToTextProvider::Deepgram,
        provider_id: None,
        model: None,
        language: None,
        auto_send: None,
        openai: None,
        deepgram: Some(DeepgramSpeechToTextConfig {
            api_key: "dg-invalid".into(),
            base_url: Some(mock_server.uri()),
            model: "nova-2".into(),
            language: None,
            detect_language: None,
            punctuate: None,
            smart_format: None,
        }),
    };

    let result = stt_service()
        .transcribe(dummy_audio(), "test.wav", "audio/wav", None, &config)
        .await;

    match result {
        Err(SttError::RequestFailed(msg)) => {
            assert!(msg.contains("403"), "expected 403 in error: {msg}");
        }
        other => panic!("expected RequestFailed, got: {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// ST-10: languageHint passed to OpenAI
// ---------------------------------------------------------------------------
#[tokio::test]
async fn st10_openai_language_hint_passed() {
    let mock_server = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path("/v1/audio/transcriptions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({ "text": "你好世界" })))
        .mount(&mock_server)
        .await;

    let config = SpeechToTextConfig {
        enabled: true,
        provider: SpeechToTextProvider::Openai,
        provider_id: None,
        model: None,
        language: None,
        auto_send: None,
        openai: Some(OpenAISpeechToTextConfig {
            api_key: "sk-test".into(),
            base_url: Some(mock_server.uri()),
            model: "whisper-1".into(),
            language: Some("en".into()),
            prompt: None,
            temperature: None,
        }),
        deepgram: None,
    };

    let result = stt_service()
        .transcribe(dummy_audio(), "test.wav", "audio/wav", Some("zh"), &config)
        .await
        .unwrap();

    assert_eq!(result.text, "你好世界");
    assert_eq!(result.language.as_deref(), Some("zh"));
}

// ---------------------------------------------------------------------------
// ST-10b: languageHint passed to Deepgram
// ---------------------------------------------------------------------------
#[tokio::test]
async fn st10b_deepgram_language_hint_passed() {
    let mock_server = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path("/v1/listen"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "metadata": { "model_info": {} },
            "results": {
                "channels": [{
                    "detected_language": "zh",
                    "alternatives": [{ "transcript": "你好" }]
                }]
            }
        })))
        .mount(&mock_server)
        .await;

    let config = SpeechToTextConfig {
        enabled: true,
        provider: SpeechToTextProvider::Deepgram,
        provider_id: None,
        model: None,
        language: None,
        auto_send: None,
        openai: None,
        deepgram: Some(DeepgramSpeechToTextConfig {
            api_key: "dg-test".into(),
            base_url: Some(mock_server.uri()),
            model: "nova-2".into(),
            language: None,
            detect_language: None,
            punctuate: None,
            smart_format: None,
        }),
    };

    let result = stt_service()
        .transcribe(dummy_audio(), "test.wav", "audio/wav", Some("zh"), &config)
        .await
        .unwrap();

    assert_eq!(result.text, "你好");
    assert_eq!(result.language.as_deref(), Some("zh"));
}

// ---------------------------------------------------------------------------
// Additional: OpenAI with all optional params (prompt, temperature)
// ---------------------------------------------------------------------------
#[tokio::test]
async fn openai_with_all_optional_params() {
    let mock_server = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path("/v1/audio/transcriptions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({ "text": "technical terms test" })))
        .mount(&mock_server)
        .await;

    let config = SpeechToTextConfig {
        enabled: true,
        provider: SpeechToTextProvider::Openai,
        provider_id: None,
        model: None,
        language: None,
        auto_send: Some(true),
        openai: Some(OpenAISpeechToTextConfig {
            api_key: "sk-full".into(),
            base_url: Some(mock_server.uri()),
            model: "whisper-1".into(),
            language: Some("en".into()),
            prompt: Some("technical terms".into()),
            temperature: Some(0.2),
        }),
        deepgram: None,
    };

    let result = stt_service()
        .transcribe(dummy_audio(), "audio.m4a", "audio/mp4", None, &config)
        .await
        .unwrap();

    assert_eq!(result.text, "technical terms test");
    assert_eq!(result.model, "whisper-1");
    assert_eq!(result.provider, SpeechToTextProvider::Openai);
    assert_eq!(result.language.as_deref(), Some("en"));
}

// ---------------------------------------------------------------------------
// Additional: Deepgram with all optional flags
// ---------------------------------------------------------------------------
#[tokio::test]
async fn deepgram_with_all_optional_flags() {
    let mock_server = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path("/v1/listen"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "metadata": {
                "model_info": {
                    "id-1": { "name": "nova-2-general" }
                }
            },
            "results": {
                "channels": [{
                    "detected_language": "fr",
                    "alternatives": [{ "transcript": "bonjour" }]
                }]
            }
        })))
        .mount(&mock_server)
        .await;

    let config = SpeechToTextConfig {
        enabled: true,
        provider: SpeechToTextProvider::Deepgram,
        provider_id: None,
        model: None,
        language: None,
        auto_send: None,
        openai: None,
        deepgram: Some(DeepgramSpeechToTextConfig {
            api_key: "dg-full".into(),
            base_url: Some(mock_server.uri()),
            model: "nova-2".into(),
            language: Some("fr".into()),
            detect_language: Some(false),
            punctuate: Some(true),
            smart_format: Some(true),
        }),
    };

    let result = stt_service()
        .transcribe(dummy_audio(), "test.ogg", "audio/ogg", None, &config)
        .await
        .unwrap();

    assert_eq!(result.text, "bonjour");
    assert_eq!(result.model, "nova-2-general");
    assert_eq!(result.language.as_deref(), Some("fr"));
}

// ---------------------------------------------------------------------------
// SttError → AppError conversion (black-box integration test)
// ---------------------------------------------------------------------------
#[test]
fn stt_error_to_app_error_mapping() {
    use nomifun_common::AppError;

    let err: AppError = SttError::Disabled.into();
    assert!(matches!(err, AppError::BadRequest(_)));

    let err: AppError = SttError::OpenaiNotConfigured.into();
    assert!(matches!(err, AppError::BadRequest(_)));

    let err: AppError = SttError::DeepgramNotConfigured.into();
    assert!(matches!(err, AppError::BadRequest(_)));

    let err: AppError = SttError::RequestFailed("upstream".into()).into();
    assert!(matches!(err, AppError::BadGateway(_)));

    let err: AppError = SttError::Unknown("bug".into()).into();
    assert!(matches!(err, AppError::Internal(_)));
}

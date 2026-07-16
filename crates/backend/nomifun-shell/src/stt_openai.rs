use nomifun_api_types::{
    ModelTask, OpenAISpeechToTextConfig, SpeechToTextProvider, SpeechToTextResult,
    resolve_dispatch_target,
};
use reqwest::Client;

use crate::error::SttError;

const DEFAULT_BASE_URL: &str = "https://api.openai.com";

pub async fn transcribe(
    client: &Client,
    config: &OpenAISpeechToTextConfig,
    audio_data: Vec<u8>,
    file_name: &str,
    mime_type: &str,
    language_hint: Option<&str>,
) -> Result<SpeechToTextResult, SttError> {
    if config.api_key.is_empty() {
        return Err(SttError::OpenaiNotConfigured);
    }

    let base_url = config
        .base_url
        .as_deref()
        .unwrap_or(DEFAULT_BASE_URL)
        .trim_end_matches('/');
    let url = resolve_dispatch_target(
        "openai-compatible",
        base_url,
        config.is_full_url,
        ModelTask::SpeechRecognition,
        &serde_json::Value::Null,
    )
    .url;

    let file_part = reqwest::multipart::Part::bytes(audio_data)
        .file_name(file_name.to_owned())
        .mime_str(mime_type)
        .map_err(|e| SttError::Unknown(format!("invalid MIME type: {e}")))?;

    let mut form = reqwest::multipart::Form::new()
        .part("file", file_part)
        .text("model", config.model.clone())
        // OpenAI defaults this to JSON, but StepFun's `step-asr` contract
        // requires the field explicitly.
        .text("response_format", "json");

    let language = language_hint.or(config.language.as_deref()).filter(|s| !s.is_empty());
    if let Some(lang) = language {
        form = form.text("language", lang.to_owned());
    }

    if let Some(prompt) = config.prompt.as_deref().filter(|s| !s.is_empty()) {
        form = form.text("prompt", prompt.to_owned());
    }

    if let Some(temp) = config.temperature {
        form = form.text("temperature", temp.to_string());
    }

    let response = client
        .post(&url)
        .header("Authorization", format!("Bearer {}", config.api_key))
        .multipart(form)
        .send()
        .await
        .map_err(|e| SttError::RequestFailed(format!("speech recognition request error: {e}")))?;

    let status = response.status();
    if !status.is_success() {
        let body = response.text().await.unwrap_or_else(|_| "<unreadable>".to_owned());
        return Err(SttError::RequestFailed(format!("speech recognition API returned {status}: {body}")));
    }

    let body: serde_json::Value = response
        .json()
        .await
        .map_err(|e| SttError::RequestFailed(format!("failed to parse speech recognition response: {e}")))?;

    let text = body["text"].as_str().unwrap_or("").to_owned();

    Ok(SpeechToTextResult {
        text,
        model: config.model.clone(),
        provider: SpeechToTextProvider::Openai,
        language: language.map(|s| s.to_owned()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_base_url_value() {
        assert_eq!(DEFAULT_BASE_URL, "https://api.openai.com");
    }

    #[tokio::test]
    async fn empty_api_key_returns_not_configured() {
        let config = OpenAISpeechToTextConfig {
            api_key: String::new(),
            base_url: None,
            is_full_url: false,
            model: "whisper-1".into(),
            language: None,
            prompt: None,
            temperature: None,
        };
        let result = transcribe(&Client::new(), &config, vec![0u8; 10], "test.wav", "audio/wav", None).await;
        assert!(matches!(result, Err(SttError::OpenaiNotConfigured)));
    }

    #[test]
    fn versioned_base_url_is_normalized_once() {
        let target = resolve_dispatch_target(
            "stepfun",
            "https://api.stepfun.com/v1",
            false,
            ModelTask::SpeechRecognition,
            &serde_json::Value::Null,
        );
        assert_eq!(target.url, "https://api.stepfun.com/v1/audio/transcriptions");
    }

    #[test]
    fn full_transcription_url_is_preserved() {
        let target = resolve_dispatch_target(
            "custom",
            "https://gateway.example/custom/transcribe",
            true,
            ModelTask::SpeechRecognition,
            &serde_json::Value::Null,
        );
        assert_eq!(target.url, "https://gateway.example/custom/transcribe");
    }
}

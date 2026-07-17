use std::sync::Arc;

use nomifun_api_types::{SpeechToTextConfig, SpeechToTextProvider, SpeechToTextResult};
use reqwest::Client;

use crate::error::SttError;
use crate::{stt_deepgram, stt_openai};

pub struct SttService {
    client: HttpClientFactory,
}

type HttpClientFactory = Arc<dyn Fn() -> Client + Send + Sync>;

impl SttService {
    pub fn new(client: Client) -> Self {
        Self {
            client: Arc::new(move || client.clone()),
        }
    }

    pub fn new_dynamic() -> Self {
        Self {
            client: Arc::new(nomifun_net::http_client),
        }
    }

    fn client(&self) -> Client {
        (self.client)()
    }

    pub async fn transcribe(
        &self,
        audio_data: Vec<u8>,
        file_name: &str,
        mime_type: &str,
        language_hint: Option<&str>,
        config: &SpeechToTextConfig,
    ) -> Result<SpeechToTextResult, SttError> {
        if !config.enabled {
            return Err(SttError::Disabled);
        }

        let client = self.client();
        match config.provider {
            SpeechToTextProvider::Openai => {
                let openai_config = config.openai.as_ref().ok_or(SttError::OpenaiNotConfigured)?;
                stt_openai::transcribe(
                    &client,
                    openai_config,
                    audio_data,
                    file_name,
                    mime_type,
                    language_hint,
                )
                .await
            }
            SpeechToTextProvider::Deepgram => {
                let deepgram_config = config.deepgram.as_ref().ok_or(SttError::DeepgramNotConfigured)?;
                stt_deepgram::transcribe(&client, deepgram_config, audio_data, mime_type, language_hint).await
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nomifun_api_types::{DeepgramSpeechToTextConfig, OpenAISpeechToTextConfig};

    fn make_disabled_config() -> SpeechToTextConfig {
        SpeechToTextConfig {
            enabled: false,
            provider: SpeechToTextProvider::Openai,
            provider_id: None,
            model: None,
            language: None,
            auto_send: None,
            openai: None,
            deepgram: None,
        }
    }

    fn make_openai_config(api_key: &str) -> SpeechToTextConfig {
        SpeechToTextConfig {
            enabled: true,
            provider: SpeechToTextProvider::Openai,
            provider_id: None,
            model: None,
            language: None,
            auto_send: None,
            openai: Some(OpenAISpeechToTextConfig {
                api_key: api_key.to_owned(),
                base_url: None,
                is_full_url: false,
                model: "whisper-1".into(),
                language: None,
                prompt: None,
                temperature: None,
            }),
            deepgram: None,
        }
    }

    fn make_deepgram_config(api_key: &str) -> SpeechToTextConfig {
        SpeechToTextConfig {
            enabled: true,
            provider: SpeechToTextProvider::Deepgram,
            provider_id: None,
            model: None,
            language: None,
            auto_send: None,
            openai: None,
            deepgram: Some(DeepgramSpeechToTextConfig {
                api_key: api_key.to_owned(),
                base_url: None,
                model: "nova-2".into(),
                language: None,
                detect_language: None,
                punctuate: None,
                smart_format: None,
            }),
        }
    }

    #[tokio::test]
    async fn disabled_config_returns_disabled_error() {
        let svc = SttService::new(Client::new());
        let result = svc
            .transcribe(vec![0u8; 10], "test.wav", "audio/wav", None, &make_disabled_config())
            .await;
        assert!(matches!(result, Err(SttError::Disabled)));
    }

    #[tokio::test]
    async fn openai_provider_missing_config_returns_not_configured() {
        let svc = SttService::new(Client::new());
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
        let result = svc
            .transcribe(vec![0u8; 10], "test.wav", "audio/wav", None, &config)
            .await;
        assert!(matches!(result, Err(SttError::OpenaiNotConfigured)));
    }

    #[tokio::test]
    async fn deepgram_provider_missing_config_returns_not_configured() {
        let svc = SttService::new(Client::new());
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
        let result = svc
            .transcribe(vec![0u8; 10], "test.wav", "audio/wav", None, &config)
            .await;
        assert!(matches!(result, Err(SttError::DeepgramNotConfigured)));
    }

    #[tokio::test]
    async fn openai_empty_api_key_returns_not_configured() {
        let svc = SttService::new(Client::new());
        let config = make_openai_config("");
        let result = svc
            .transcribe(vec![0u8; 10], "test.wav", "audio/wav", None, &config)
            .await;
        assert!(matches!(result, Err(SttError::OpenaiNotConfigured)));
    }

    #[tokio::test]
    async fn deepgram_empty_api_key_returns_not_configured() {
        let svc = SttService::new(Client::new());
        let config = make_deepgram_config("");
        let result = svc
            .transcribe(vec![0u8; 10], "test.wav", "audio/wav", None, &config)
            .await;
        assert!(matches!(result, Err(SttError::DeepgramNotConfigured)));
    }
}

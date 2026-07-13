use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// Shell operation types
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ToolType {
    Vscode,
    Terminal,
    Explorer,
}

#[derive(Debug, Clone, Deserialize)]
pub struct OpenFileRequest {
    pub file_path: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ShowItemInFolderRequest {
    pub file_path: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct OpenExternalRequest {
    pub url: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct CheckToolInstalledRequest {
    pub tool: ToolType,
}

#[derive(Debug, Clone, Serialize)]
pub struct CheckToolInstalledResponse {
    pub installed: bool,
}

#[derive(Debug, Clone, Deserialize)]
pub struct OpenFolderWithRequest {
    pub folder_path: String,
    pub tool: ToolType,
}

// ---------------------------------------------------------------------------
// Speech-to-text types
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SpeechToTextProvider {
    Local,
    Openai,
    Deepgram,
}

#[derive(Debug, Clone, Serialize)]
pub struct SpeechToTextResult {
    pub text: String,
    pub model: String,
    pub provider: SpeechToTextProvider,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub language: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct OpenAISpeechToTextConfig {
    pub api_key: String,
    #[serde(default)]
    pub base_url: Option<String>,
    pub model: String,
    #[serde(default)]
    pub language: Option<String>,
    #[serde(default)]
    pub prompt: Option<String>,
    #[serde(default)]
    pub temperature: Option<f64>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct DeepgramSpeechToTextConfig {
    pub api_key: String,
    #[serde(default)]
    pub base_url: Option<String>,
    pub model: String,
    #[serde(default)]
    pub language: Option<String>,
    #[serde(default)]
    pub detect_language: Option<bool>,
    #[serde(default)]
    pub punctuate: Option<bool>,
    #[serde(default)]
    pub smart_format: Option<bool>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct SpeechToTextConfig {
    pub enabled: bool,
    pub provider: SpeechToTextProvider,
    #[serde(default)]
    pub provider_id: Option<String>,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub language: Option<String>,
    #[serde(default)]
    pub auto_send: Option<bool>,
    #[serde(default)]
    pub openai: Option<OpenAISpeechToTextConfig>,
    #[serde(default)]
    pub deepgram: Option<DeepgramSpeechToTextConfig>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // -- ToolType --

    #[test]
    fn tool_type_serializes_lowercase() {
        assert_eq!(serde_json::to_value(ToolType::Vscode).unwrap(), "vscode");
        assert_eq!(serde_json::to_value(ToolType::Terminal).unwrap(), "terminal");
        assert_eq!(serde_json::to_value(ToolType::Explorer).unwrap(), "explorer");
    }

    #[test]
    fn tool_type_deserializes_lowercase() {
        let v: ToolType = serde_json::from_str(r#""vscode""#).unwrap();
        assert_eq!(v, ToolType::Vscode);
        let t: ToolType = serde_json::from_str(r#""terminal""#).unwrap();
        assert_eq!(t, ToolType::Terminal);
        let e: ToolType = serde_json::from_str(r#""explorer""#).unwrap();
        assert_eq!(e, ToolType::Explorer);
    }

    #[test]
    fn tool_type_rejects_unknown() {
        let result = serde_json::from_str::<ToolType>(r#""unknown""#);
        assert!(result.is_err());
    }

    // -- Shell request types --

    #[test]
    fn open_file_request_snake_case() {
        let raw = json!({ "file_path": "/tmp/test.txt" });
        let req: OpenFileRequest = serde_json::from_value(raw).unwrap();
        assert_eq!(req.file_path, "/tmp/test.txt");
    }

    #[test]
    fn open_file_request_missing_field() {
        let result = serde_json::from_value::<OpenFileRequest>(json!({}));
        assert!(result.is_err());
    }

    #[test]
    fn show_item_in_folder_request_snake_case() {
        let raw = json!({ "file_path": "/home/user/doc.pdf" });
        let req: ShowItemInFolderRequest = serde_json::from_value(raw).unwrap();
        assert_eq!(req.file_path, "/home/user/doc.pdf");
    }

    #[test]
    fn open_external_request_parses() {
        let raw = json!({ "url": "https://example.com" });
        let req: OpenExternalRequest = serde_json::from_value(raw).unwrap();
        assert_eq!(req.url, "https://example.com");
    }

    #[test]
    fn check_tool_installed_request_parses() {
        let raw = json!({ "tool": "vscode" });
        let req: CheckToolInstalledRequest = serde_json::from_value(raw).unwrap();
        assert_eq!(req.tool, ToolType::Vscode);
    }

    #[test]
    fn check_tool_installed_response_serializes() {
        let resp = CheckToolInstalledResponse { installed: true };
        let json = serde_json::to_value(&resp).unwrap();
        assert_eq!(json["installed"], true);
    }

    #[test]
    fn open_folder_with_request_snake_case() {
        let raw = json!({ "folder_path": "/tmp", "tool": "terminal" });
        let req: OpenFolderWithRequest = serde_json::from_value(raw).unwrap();
        assert_eq!(req.folder_path, "/tmp");
        assert_eq!(req.tool, ToolType::Terminal);
    }

    // -- SpeechToTextProvider --

    #[test]
    fn stt_provider_serializes_lowercase() {
        assert_eq!(serde_json::to_value(SpeechToTextProvider::Local).unwrap(), "local");
        assert_eq!(serde_json::to_value(SpeechToTextProvider::Openai).unwrap(), "openai");
        assert_eq!(
            serde_json::to_value(SpeechToTextProvider::Deepgram).unwrap(),
            "deepgram"
        );
    }

    #[test]
    fn stt_provider_deserializes_lowercase() {
        let local: SpeechToTextProvider = serde_json::from_str(r#""local""#).unwrap();
        assert_eq!(local, SpeechToTextProvider::Local);
        let o: SpeechToTextProvider = serde_json::from_str(r#""openai""#).unwrap();
        assert_eq!(o, SpeechToTextProvider::Openai);
        let d: SpeechToTextProvider = serde_json::from_str(r#""deepgram""#).unwrap();
        assert_eq!(d, SpeechToTextProvider::Deepgram);
    }

    #[test]
    fn stt_provider_rejects_unknown() {
        let result = serde_json::from_str::<SpeechToTextProvider>(r#""azure""#);
        assert!(result.is_err());
    }

    // -- SpeechToTextResult --

    #[test]
    fn stt_result_serializes_with_language() {
        let result = SpeechToTextResult {
            text: "hello world".to_owned(),
            model: "whisper-1".to_owned(),
            provider: SpeechToTextProvider::Openai,
            language: Some("en".to_owned()),
        };
        let json = serde_json::to_value(&result).unwrap();
        assert_eq!(json["text"], "hello world");
        assert_eq!(json["model"], "whisper-1");
        assert_eq!(json["provider"], "openai");
        assert_eq!(json["language"], "en");
    }

    #[test]
    fn stt_result_omits_null_language() {
        let result = SpeechToTextResult {
            text: "test".to_owned(),
            model: "nova-2".to_owned(),
            provider: SpeechToTextProvider::Deepgram,
            language: None,
        };
        let json = serde_json::to_value(&result).unwrap();
        assert!(json.get("language").is_none());
    }

    // -- SpeechToTextConfig --

    #[test]
    fn stt_config_full_openai() {
        let raw = json!({
            "enabled": true,
            "provider": "openai",
            "auto_send": true,
            "openai": {
                "api_key": "sk-test",
                "base_url": "https://api.openai.com",
                "model": "whisper-1",
                "language": "en",
                "prompt": "technical terms",
                "temperature": 0.2
            }
        });
        let config: SpeechToTextConfig = serde_json::from_value(raw).unwrap();
        assert!(config.enabled);
        assert_eq!(config.provider, SpeechToTextProvider::Openai);
        assert_eq!(config.auto_send, Some(true));
        let openai = config.openai.unwrap();
        assert_eq!(openai.api_key, "sk-test");
        assert_eq!(openai.base_url.as_deref(), Some("https://api.openai.com"));
        assert_eq!(openai.model, "whisper-1");
        assert_eq!(openai.language.as_deref(), Some("en"));
        assert_eq!(openai.prompt.as_deref(), Some("technical terms"));
        assert_eq!(openai.temperature, Some(0.2));
        assert!(config.deepgram.is_none());
    }

    #[test]
    fn stt_config_full_deepgram() {
        let raw = json!({
            "enabled": true,
            "provider": "deepgram",
            "deepgram": {
                "api_key": "dg-test",
                "model": "nova-2",
                "language": "zh",
                "detect_language": true,
                "punctuate": true,
                "smart_format": false
            }
        });
        let config: SpeechToTextConfig = serde_json::from_value(raw).unwrap();
        assert!(config.enabled);
        assert_eq!(config.provider, SpeechToTextProvider::Deepgram);
        assert!(config.auto_send.is_none());
        assert!(config.openai.is_none());
        let dg = config.deepgram.unwrap();
        assert_eq!(dg.api_key, "dg-test");
        assert!(dg.base_url.is_none());
        assert_eq!(dg.model, "nova-2");
        assert_eq!(dg.language.as_deref(), Some("zh"));
        assert_eq!(dg.detect_language, Some(true));
        assert_eq!(dg.punctuate, Some(true));
        assert_eq!(dg.smart_format, Some(false));
    }

    #[test]
    fn stt_config_minimal() {
        let raw = json!({
            "enabled": false,
            "provider": "openai"
        });
        let config: SpeechToTextConfig = serde_json::from_value(raw).unwrap();
        assert!(!config.enabled);
        assert_eq!(config.provider, SpeechToTextProvider::Openai);
        assert!(config.auto_send.is_none());
        assert!(config.openai.is_none());
        assert!(config.deepgram.is_none());
    }

    #[test]
    fn stt_config_missing_required_field() {
        let raw = json!({ "enabled": true });
        let result = serde_json::from_value::<SpeechToTextConfig>(raw);
        assert!(result.is_err());
    }

    // -- OpenAISpeechToTextConfig --

    #[test]
    fn openai_config_minimal() {
        let raw = json!({
            "api_key": "sk-key",
            "model": "whisper-1"
        });
        let config: OpenAISpeechToTextConfig = serde_json::from_value(raw).unwrap();
        assert_eq!(config.api_key, "sk-key");
        assert_eq!(config.model, "whisper-1");
        assert!(config.base_url.is_none());
        assert!(config.language.is_none());
        assert!(config.prompt.is_none());
        assert!(config.temperature.is_none());
    }

    // -- DeepgramSpeechToTextConfig --

    #[test]
    fn deepgram_config_minimal() {
        let raw = json!({
            "api_key": "dg-key",
            "model": "nova-2"
        });
        let config: DeepgramSpeechToTextConfig = serde_json::from_value(raw).unwrap();
        assert_eq!(config.api_key, "dg-key");
        assert_eq!(config.model, "nova-2");
        assert!(config.base_url.is_none());
        assert!(config.language.is_none());
        assert!(config.detect_language.is_none());
        assert!(config.punctuate.is_none());
        assert!(config.smart_format.is_none());
    }
}

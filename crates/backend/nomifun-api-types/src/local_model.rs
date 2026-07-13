use serde::{Deserialize, Serialize};

use crate::{ManagedModelServiceKind, ModelTask, ModelTrait};

/// Immutable metadata for one model in NomiFun's curated local catalog.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LocalModelCatalogEntry {
    pub id: String,
    pub name: String,
    pub description: String,
    pub parameter_size: String,
    pub quantization: String,
    pub download_size_bytes: u64,
    pub required_memory_bytes: u64,
    pub context_window: u32,
    pub license: String,
    /// Human-readable model origin. Download URLs remain internal to the catalog.
    pub source: String,
    pub recommended: bool,
    pub tasks: Vec<ModelTask>,
    pub traits: Vec<ModelTrait>,
}

/// Persistent installation state for a curated local model.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LocalModelInstallPhase {
    NotInstalled,
    Downloading,
    Verifying,
    Installed,
    /// A cancelled transfer whose partial file can be resumed.
    Paused,
    Failed,
}

/// Lifecycle state of the local inference process.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LocalModelRuntimePhase {
    Stopped,
    Starting,
    Ready,
    Stopping,
    Failed,
}

/// Artifact currently contributing to an aggregate install transfer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LocalModelProgressComponent {
    Runtime,
    Model,
    /// Auxiliary speech-recognition artifact such as a VAD model.
    AsrAuxiliary,
    /// Vision encoder/projector paired with a multimodal GGUF.
    VisionProjector,
}

/// Stable, non-sensitive category suitable for localized UI errors.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LocalModelErrorKind {
    Network,
    InsufficientSpace,
    ChecksumMismatch,
    UnsupportedPlatform,
    RuntimeUnavailable,
    Busy,
    NotFound,
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LocalModelTransferProgress {
    pub component: LocalModelProgressComponent,
    pub downloaded_bytes: u64,
    pub total_bytes: u64,
    pub bytes_per_second: u64,
}

/// Mutable installation and runtime state for one catalog model.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LocalModelState {
    pub model_id: String,
    pub install_phase: LocalModelInstallPhase,
    pub progress: Option<LocalModelTransferProgress>,
    pub installed_bytes: u64,
    pub runtime_phase: LocalModelRuntimePhase,
    pub error_kind: Option<LocalModelErrorKind>,
    /// Sanitized user-safe detail. It must not contain paths, URLs, or command output.
    pub message: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LocalModelRuntimeBackend {
    Cpu,
    Vulkan,
    Metal,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LocalRuntimeStatus {
    pub version: Option<String>,
    pub backend: Option<LocalModelRuntimeBackend>,
    pub phase: LocalModelRuntimePhase,
    pub error_kind: Option<LocalModelErrorKind>,
    /// Sanitized user-safe detail. It must not contain paths, URLs, or command output.
    pub message: Option<String>,
}

/// Complete status returned by local-model status and mutation endpoints.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LocalModelServiceStatus {
    pub kind: ManagedModelServiceKind,
    pub protocol_version: String,
    pub provider_id: String,
    pub enabled: bool,
    pub ready: bool,
    pub active_model_id: Option<String>,
    pub runtime: LocalRuntimeStatus,
    pub models: Vec<LocalModelState>,
    /// Sanitized service-level diagnostic, if any.
    pub last_error: Option<String>,
}

/// Request body for `POST /api/model-services/local/models/{id}/activate`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct SetLocalModelActiveRequest {
    pub enabled: bool,
}

/// Immutable metadata for a curated local speech-recognition model.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AsrEngine {
    WhisperCpp,
    FunAsrLlamaCpp,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AsrCapability {
    Transcription,
    LanguageDetection,
    EmotionDetection,
    AudioEventDetection,
    LongAudioVad,
}

/// Immutable metadata for a curated local speech-recognition model.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AsrModelCatalogEntry {
    pub id: String,
    pub name: String,
    pub description: String,
    pub model_size: String,
    pub quantization: String,
    /// Runtime archive plus the model artifact for the current platform.
    pub download_size_bytes: u64,
    pub required_memory_bytes: u64,
    pub languages: Vec<String>,
    pub license: String,
    pub source: String,
    pub recommended: bool,
    pub engine: AsrEngine,
    pub capabilities: Vec<AsrCapability>,
}

/// Complete status for the opt-in local speech-recognition service.
///
/// The runtime phase describes availability of the installed speech-recognition
/// command-line runtime. It is launched for one transcription request and is
/// never kept resident between requests.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AsrModelServiceStatus {
    pub protocol_version: String,
    pub enabled: bool,
    pub ready: bool,
    pub active_model_id: Option<String>,
    pub runtime: LocalRuntimeStatus,
    pub models: Vec<LocalModelState>,
    /// Sanitized service-level diagnostic, if any.
    pub last_error: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn local_status_uses_stable_camel_case_wire_contract() {
        let status = LocalModelServiceStatus {
            kind: ManagedModelServiceKind::Local,
            protocol_version: "1".into(),
            provider_id: "nomifun-local-model".into(),
            enabled: true,
            ready: false,
            active_model_id: Some("example-local".into()),
            runtime: LocalRuntimeStatus {
                version: Some("1.0.0".into()),
                backend: Some(LocalModelRuntimeBackend::Cpu),
                phase: LocalModelRuntimePhase::Starting,
                error_kind: None,
                message: None,
            },
            models: vec![LocalModelState {
                model_id: "example-local".into(),
                install_phase: LocalModelInstallPhase::Downloading,
                progress: Some(LocalModelTransferProgress {
                    component: LocalModelProgressComponent::Model,
                    downloaded_bytes: 12,
                    total_bytes: 24,
                    bytes_per_second: 6,
                }),
                installed_bytes: 0,
                runtime_phase: LocalModelRuntimePhase::Stopped,
                error_kind: None,
                message: None,
            }],
            last_error: None,
        };

        let json = serde_json::to_value(status).unwrap();
        assert_eq!(json["kind"], "local");
        assert_eq!(json["protocolVersion"], "1");
        assert_eq!(json["providerId"], "nomifun-local-model");
        assert_eq!(json["activeModelId"], "example-local");
        assert_eq!(json["runtime"]["backend"], "cpu");
        assert_eq!(json["models"][0]["installPhase"], "downloading");
        assert_eq!(json["models"][0]["progress"]["downloadedBytes"], 12);
        assert!(json.get("protocol_version").is_none());
    }

    #[test]
    fn local_error_and_phase_values_are_stable_snake_case() {
        assert_eq!(
            serde_json::to_value(LocalModelErrorKind::ChecksumMismatch).unwrap(),
            serde_json::json!("checksum_mismatch")
        );
        assert_eq!(
            serde_json::to_value(LocalModelInstallPhase::NotInstalled).unwrap(),
            serde_json::json!("not_installed")
        );
        assert_eq!(
            serde_json::to_value(LocalModelRuntimeBackend::Vulkan).unwrap(),
            serde_json::json!("vulkan")
        );
    }
}

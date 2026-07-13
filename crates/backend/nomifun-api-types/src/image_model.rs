use serde::{Deserialize, Serialize};

use crate::LocalModelErrorKind;

/// Immutable metadata for one curated local image-generation bundle.
///
/// Download URLs, checksums, revisions and local paths intentionally remain
/// server-side so clients cannot turn the managed installer into an arbitrary
/// network or file-system primitive.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ImageModelCatalogEntry {
    pub id: String,
    pub name: String,
    pub description: String,
    pub format: String,
    /// Total bytes for the selected platform's runtime and all model files.
    pub download_size_bytes: u64,
    pub required_memory_bytes: u64,
    pub license: String,
    pub source: String,
    pub components: Vec<ImageModelComponent>,
    pub recommended: bool,
    /// User-visible provenance or licensing qualification, when required.
    pub notice: Option<String>,
}

/// One independently downloaded and verified part of an image model bundle.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ImageModelComponent {
    Runtime,
    DiffusionModel,
    TextEncoder,
    Vae,
}

/// Persistent installation phase for either a component or its whole bundle.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ImageModelInstallPhase {
    NotInstalled,
    Downloading,
    Verifying,
    /// Used by archive-based runtimes after verification succeeds.
    Extracting,
    Installed,
    /// A stopped transfer whose partial file remains available for resuming.
    Paused,
    Failed,
}

/// Coarse readiness of the one-shot local image inference runtime.
///
/// Unlike a resident language-model server, the image runtime starts once per
/// generation. `Ready` therefore means that a verified executable can be
/// launched; it does not claim a process is currently running.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ImageModelRuntimePhase {
    Unavailable,
    Ready,
    Busy,
    Failed,
}

/// Live and persisted transfer state for one managed component.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ImageModelComponentProgress {
    pub component: ImageModelComponent,
    pub install_phase: ImageModelInstallPhase,
    /// Bytes currently present in the final file or resumable partial file.
    pub downloaded_bytes: u64,
    pub total_bytes: u64,
    /// Bytes that have passed both size and checksum verification.
    pub installed_bytes: u64,
    pub bytes_per_second: u64,
    pub error_kind: Option<LocalModelErrorKind>,
    /// Sanitized user-safe detail. It must not contain paths, URLs, response
    /// bodies, checksums or process output.
    pub message: Option<String>,
}

/// Mutable installation state for one curated image-generation bundle.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ImageModelState {
    pub model_id: String,
    pub install_phase: ImageModelInstallPhase,
    /// Contains one entry for every component, including inactive components,
    /// so clients can render four stable progress rows throughout installation.
    pub component_progress: Vec<ImageModelComponentProgress>,
    pub installed_bytes: u64,
    pub error_kind: Option<LocalModelErrorKind>,
    /// Sanitized user-safe detail. It must not contain paths, URLs, response
    /// bodies, checksums or process output.
    pub message: Option<String>,
}

/// Complete image-model status returned by status and mutation endpoints.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ImageModelServiceStatus {
    pub protocol_version: String,
    /// True only after every managed artifact has passed verification.
    pub artifacts_ready: bool,
    /// True only when verified artifacts can be resolved into a runnable local
    /// image adapter configuration.
    pub inference_ready: bool,
    pub runtime_phase: ImageModelRuntimePhase,
    pub models: Vec<ImageModelState>,
    /// Sanitized service-level diagnostic, if any.
    pub last_error: Option<String>,
}

/// Request body accepted by image-model install commands.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct InstallImageModelRequest {
    pub model_id: String,
}

/// Request body accepted by image-model resume commands.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ResumeImageModelInstallRequest {
    pub model_id: String,
}

/// Request body accepted by image-model cancellation commands.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CancelImageModelInstallRequest {
    pub model_id: String,
}

/// Request body accepted by image-model deletion commands.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct DeleteImageModelRequest {
    pub model_id: String,
}

/// Mutation endpoints return the same authoritative snapshot as the status
/// endpoint, without adding another response wrapper inside `ApiResponse`.
pub type InstallImageModelResponse = ImageModelServiceStatus;
pub type ResumeImageModelInstallResponse = ImageModelServiceStatus;
pub type CancelImageModelInstallResponse = ImageModelServiceStatus;
pub type DeleteImageModelResponse = ImageModelServiceStatus;

#[cfg(test)]
mod tests {
    use super::*;

    fn status() -> ImageModelServiceStatus {
        ImageModelServiceStatus {
            protocol_version: "1".into(),
            artifacts_ready: false,
            inference_ready: false,
            runtime_phase: ImageModelRuntimePhase::Unavailable,
            models: vec![ImageModelState {
                model_id: "z-image-turbo-q3-k".into(),
                install_phase: ImageModelInstallPhase::Downloading,
                component_progress: vec![ImageModelComponentProgress {
                    component: ImageModelComponent::DiffusionModel,
                    install_phase: ImageModelInstallPhase::Downloading,
                    downloaded_bytes: 5,
                    total_bytes: 10,
                    installed_bytes: 0,
                    bytes_per_second: 2,
                    error_kind: None,
                    message: None,
                }],
                installed_bytes: 0,
                error_kind: None,
                message: None,
            }],
            last_error: None,
        }
    }

    #[test]
    fn status_has_stable_per_component_wire_contract() {
        let json = serde_json::to_value(status()).unwrap();
        assert_eq!(json["protocolVersion"], "1");
        assert_eq!(json["artifactsReady"], false);
        assert_eq!(json["inferenceReady"], false);
        assert_eq!(json["runtimePhase"], "unavailable");
        assert_eq!(
            json["models"][0]["componentProgress"][0]["component"],
            "diffusion_model"
        );
        assert_eq!(
            json["models"][0]["componentProgress"][0]["installPhase"],
            "downloading"
        );
        assert!(json.get("downloadUrl").is_none());
        assert!(json.get("localPath").is_none());
    }

    #[test]
    fn all_component_and_phase_values_are_stable_snake_case() {
        assert_eq!(
            serde_json::to_value([
                ImageModelComponent::Runtime,
                ImageModelComponent::DiffusionModel,
                ImageModelComponent::TextEncoder,
                ImageModelComponent::Vae,
            ])
            .unwrap(),
            serde_json::json!(["runtime", "diffusion_model", "text_encoder", "vae"])
        );
        assert_eq!(
            serde_json::to_value(ImageModelInstallPhase::NotInstalled).unwrap(),
            serde_json::json!("not_installed")
        );
    }

    #[test]
    fn mutation_requests_use_only_a_camel_case_catalog_id() {
        let request: InstallImageModelRequest =
            serde_json::from_value(serde_json::json!({"modelId": "z-image-turbo-q3-k"}))
                .unwrap();
        assert_eq!(request.model_id, "z-image-turbo-q3-k");
        assert!(serde_json::from_value::<InstallImageModelRequest>(serde_json::json!({
            "modelId": "z-image-turbo-q3-k",
            "downloadUrl": "https://example.invalid/model"
        }))
        .is_err());
    }

    #[test]
    fn mutation_response_alias_preserves_the_status_wire_shape() {
        let response: InstallImageModelResponse = status();
        let json = serde_json::to_value(response).unwrap();
        assert_eq!(json["models"][0]["modelId"], "z-image-turbo-q3-k");
        assert!(json.get("status").is_none());
    }
}

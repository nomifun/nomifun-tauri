//! Unified multimodal capability taxonomy: the authoritative per-model
//! `ModelTask` / `ModelTrait` vocabulary that replaces the two legacy
//! vocabularies (`ModelType` here, `MediaCapability` in nomifun-creation).
//!
//! - [`ModelTask`] is the endpoint-determining "task" a model performs. It is
//!   what the dispatch/probe layer branches on to pick the right HTTP endpoint
//!   and request shape.
//! - [`ModelTrait`] is a within-task refinement (mostly for Chat models):
//!   whether a chat model accepts image input, calls functions, reasons, etc.
//! - [`ModelProfile`] is the authoritative per-model record persisted in the
//!   `model_profiles` table (keyed by `(provider_id, model)`), superseding the
//!   name-only heuristic as the runtime source of truth.
//!
//! [`derive_tasks_and_traits`] seeds a profile from the model name + platform
//! (used for backfill and as the default suggestion for newly-entered models);
//! it is a SEED, not the runtime authority — once a row exists (especially
//! `source = User`) the stored profile wins.

use serde::{Deserialize, Serialize};

use crate::model_capability::{base_model_name, infer_generation_capabilities, infer_model_modalities};
use crate::ModelType;

/// The endpoint-determining task a model performs. Wire values are snake_case.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelTask {
    /// Text / multimodal chat completions (`/chat/completions`).
    Chat,
    /// Text → image (`/images/generations`).
    ImageGeneration,
    /// Image(+mask)+text → image (`/images/edits`).
    ImageEdit,
    /// Text/image → video (`/videos`).
    VideoGeneration,
    /// Text → speech / TTS (`/audio/speech`).
    SpeechSynthesis,
    /// Speech → text / ASR (`/audio/transcriptions`).
    SpeechRecognition,
    /// Text → vector (`/embeddings`).
    Embedding,
    /// Query+documents → scores (`/rerank`).
    Rerank,
}

/// Within-task refinement of a model's abilities. Mostly modifies [`ModelTask::Chat`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelTrait {
    /// Chat model accepts image input (vision understanding).
    VisionInput,
    /// Chat model supports tool/function calling.
    FunctionCalling,
    /// Chat model is a reasoning model.
    Reasoning,
    /// Chat model has built-in web search.
    WebSearch,
}

/// Provenance of a [`ModelProfile`]. Higher authority wins: `User` > `Catalog` > `Inferred`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum ProfileSource {
    /// Auto-derived from the model name/platform heuristic.
    #[default]
    Inferred,
    /// Explicitly set by the user in the UI (authoritative).
    User,
    /// Populated from a managed catalog (e.g. local AI).
    Catalog,
}

/// The authoritative per-model capability record. Identity is `(provider_id, model)`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ModelProfile {
    pub provider_id: String,
    pub model: String,
    pub tasks: Vec<ModelTask>,
    pub traits: Vec<ModelTrait>,
    /// Free-form service config (image size/steps, tts voice, asr language,
    /// endpoint/request-shape overrides, timeout, …). See [`crate::dispatch_target`].
    #[serde(default)]
    pub params: serde_json::Value,
    #[serde(default)]
    pub source: ProfileSource,
    pub updated_at: i64,
}

impl ModelProfile {
    /// The primary task used when a caller (e.g. the health probe) needs a
    /// single task and none was specified. Prefers the first declared task;
    /// falls back to [`ModelTask::Chat`].
    pub fn primary_task(&self) -> ModelTask {
        self.tasks.first().copied().unwrap_or(ModelTask::Chat)
    }
}

/// Request body for `POST /api/model-profiles` (upsert one profile).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelProfileUpsertRequest {
    pub provider_id: String,
    pub model: String,
    #[serde(default)]
    pub tasks: Vec<ModelTask>,
    #[serde(default)]
    pub traits: Vec<ModelTrait>,
    #[serde(default)]
    pub params: Option<serde_json::Value>,
    /// Defaults to `User` when omitted (this endpoint is the user-edit path).
    #[serde(default)]
    pub source: Option<ProfileSource>,
}

/// Body identifying a single profile (`POST /api/model-profiles/delete`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelProfileKeyRequest {
    pub provider_id: String,
    pub model: String,
}

// --- Name/platform substring seeds (extend the model_capability.rs heuristic) ---

/// Substrings implying text-to-speech. `whisper` is excluded (that's ASR).
const TTS_INCLUDE: &[&str] = &["tts", "text-to-speech", "cosyvoice", "-voice", "speech-0", "sovits"];
/// Substrings implying speech recognition / transcription.
const ASR_INCLUDE: &[&str] =
    &["whisper", "asr", "transcrib", "speech-to-text", "sensevoice", "paraformer", "nova-2", "nova-3"];
/// Substrings implying embedding models.
const EMBEDDING_INCLUDE: &[&str] = &["embed", "text-embedding", "bge-", "gte-", "-e5-"];
/// Substrings implying rerank models.
const RERANK_INCLUDE: &[&str] = &["rerank"];
/// Substrings implying image editing (in addition to image generation).
const IMAGE_EDIT_INCLUDE: &[&str] = &["edit", "inpaint"];

fn push_unique(tasks: &mut Vec<ModelTask>, task: ModelTask) {
    if !tasks.contains(&task) {
        tasks.push(task);
    }
}

/// Seed a model's `(tasks, traits)` from its platform + name.
///
/// Platform acts as a first-class authority where it is unambiguous (e.g.
/// `stepfun-plan` is StepFun's image-only Step Plan product, so every model on
/// it is an image model — this is why `step-image-edit-2` is correctly typed
/// even though its name matches no generic image substring). Otherwise the
/// model name drives the classification. A model that matches no specialized
/// (image/video/audio/embedding/rerank) signal is treated as a Chat model.
pub fn derive_tasks_and_traits(platform: &str, model: &str) -> (Vec<ModelTask>, Vec<ModelTrait>) {
    let base = base_model_name(model);
    let mut tasks: Vec<ModelTask> = Vec::new();
    let mut traits: Vec<ModelTrait> = Vec::new();

    // 1. Platform-level authority.
    if platform.eq_ignore_ascii_case("stepfun-plan") {
        push_unique(&mut tasks, ModelTask::ImageGeneration);
        push_unique(&mut tasks, ModelTask::ImageEdit);
    }

    // 2. Generation capabilities from the existing name heuristic.
    for cap in infer_generation_capabilities(model) {
        match cap {
            ModelType::ImageGeneration => push_unique(&mut tasks, ModelTask::ImageGeneration),
            ModelType::VideoGeneration => push_unique(&mut tasks, ModelTask::VideoGeneration),
            _ => {}
        }
    }

    // 3. Broader image signal: an "image" model id that the family list missed.
    if base.contains("image") {
        push_unique(&mut tasks, ModelTask::ImageGeneration);
    }
    // 4. Image editing signal (only meaningful for image models).
    if !tasks.is_empty()
        && (tasks.contains(&ModelTask::ImageGeneration))
        && IMAGE_EDIT_INCLUDE.iter().any(|k| base.contains(k))
    {
        push_unique(&mut tasks, ModelTask::ImageEdit);
    }

    // 5. Audio / embedding / rerank (mutually exclusive families, checked in priority order).
    if RERANK_INCLUDE.iter().any(|k| base.contains(k)) {
        push_unique(&mut tasks, ModelTask::Rerank);
    } else if EMBEDDING_INCLUDE.iter().any(|k| base.contains(k)) {
        push_unique(&mut tasks, ModelTask::Embedding);
    } else if ASR_INCLUDE.iter().any(|k| base.contains(k)) {
        push_unique(&mut tasks, ModelTask::SpeechRecognition);
    } else if TTS_INCLUDE.iter().any(|k| base.contains(k)) {
        push_unique(&mut tasks, ModelTask::SpeechSynthesis);
    }

    // 6. Vision-input trait (a vision model is a Chat model that accepts images).
    if infer_model_modalities(model).iter().any(|m| m == "vision") {
        traits.push(ModelTrait::VisionInput);
    }

    // 7. Default: no specialized task means it is a Chat model.
    if tasks.is_empty() {
        tasks.push(ModelTask::Chat);
    }

    (tasks, traits)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tasks_of(platform: &str, model: &str) -> Vec<ModelTask> {
        derive_tasks_and_traits(platform, model).0
    }

    #[test]
    fn chat_model_is_chat() {
        assert_eq!(tasks_of("openai", "gpt-4o-mini"), vec![ModelTask::Chat]);
        assert_eq!(tasks_of("deepseek", "deepseek-chat"), vec![ModelTask::Chat]);
    }

    #[test]
    fn vision_chat_model_has_chat_task_and_vision_trait() {
        let (tasks, traits) = derive_tasks_and_traits("openai", "gpt-4o");
        assert_eq!(tasks, vec![ModelTask::Chat]);
        assert!(traits.contains(&ModelTrait::VisionInput));
    }

    #[test]
    fn stepfun_plan_model_is_image_even_without_name_match() {
        // The reported failing case: name matches no generic image substring,
        // but the platform is StepFun's image-only Step Plan product.
        let tasks = tasks_of("stepfun-plan", "step-image-edit-2");
        assert!(tasks.contains(&ModelTask::ImageGeneration));
        assert!(tasks.contains(&ModelTask::ImageEdit));
        assert!(!tasks.contains(&ModelTask::Chat));
    }

    #[test]
    fn dall_e_is_image_generation() {
        assert!(tasks_of("openai", "dall-e-3").contains(&ModelTask::ImageGeneration));
    }

    #[test]
    fn whisper_is_speech_recognition_not_tts() {
        let tasks = tasks_of("openai", "whisper-1");
        assert!(tasks.contains(&ModelTask::SpeechRecognition));
        assert!(!tasks.contains(&ModelTask::SpeechSynthesis));
        assert!(!tasks.contains(&ModelTask::Chat));
    }

    #[test]
    fn tts_is_speech_synthesis() {
        assert!(tasks_of("openai", "gpt-4o-mini-tts").contains(&ModelTask::SpeechSynthesis));
        assert!(tasks_of("stepfun", "step-tts-mini").contains(&ModelTask::SpeechSynthesis));
    }

    #[test]
    fn embedding_and_rerank() {
        assert!(tasks_of("openai", "text-embedding-3-large").contains(&ModelTask::Embedding));
        assert!(tasks_of("jina", "bge-reranker-v2").contains(&ModelTask::Rerank));
    }

    #[test]
    fn video_generation() {
        assert!(tasks_of("openai", "sora-2").contains(&ModelTask::VideoGeneration));
    }

    #[test]
    fn primary_task_prefers_first() {
        let p = ModelProfile {
            provider_id: "p".into(),
            model: "m".into(),
            tasks: vec![ModelTask::ImageGeneration, ModelTask::ImageEdit],
            traits: vec![],
            params: serde_json::Value::Null,
            source: ProfileSource::User,
            updated_at: 0,
        };
        assert_eq!(p.primary_task(), ModelTask::ImageGeneration);
        let empty = ModelProfile { tasks: vec![], ..p };
        assert_eq!(empty.primary_task(), ModelTask::Chat);
    }

    #[test]
    fn wire_format_is_snake_case() {
        assert_eq!(serde_json::to_string(&ModelTask::ImageGeneration).unwrap(), "\"image_generation\"");
        assert_eq!(serde_json::to_string(&ModelTask::SpeechRecognition).unwrap(), "\"speech_recognition\"");
        assert_eq!(serde_json::to_string(&ModelTrait::VisionInput).unwrap(), "\"vision_input\"");
        assert_eq!(serde_json::to_string(&ProfileSource::Inferred).unwrap(), "\"inferred\"");
    }
}

//! Per-model capability inference from the model NAME — the only per-model
//! signal available (provider `capabilities` is provider-level; there is no
//! user-authored per-model capability field). Ported from the frontend
//! `ui/src/common/utils/modelCapabilities.ts`. Dep-free substring matching.
//!
//! Two heuristics live here and MUST stay in sync with the frontend twin:
//! - [`infer_model_modalities`] — chat-modality signal (currently `"vision"`),
//!   consumed by the orchestrator router's `needs_vision` hard filter.
//! - [`infer_generation_capabilities`] — Creative Workshop signal: does the
//!   model NAME look like an image/video generator? Returns suggested
//!   [`ModelType`] tags used as **defaults** the user may override. This is a
//!   separate function so it has ZERO impact on orchestrator chat routing (a
//!   generator model keeps returning no chat modality and stays a router
//!   baseline member exactly as before).

use crate::ModelType;

/// Normalize a model id for name matching (mirrors FE `getBaseModelName`):
/// lowercase, non-[a-z0-9./-] → '-', collapse runs, trim leading/trailing '-'.
pub fn base_model_name(model: &str) -> String {
    let lowered = model.to_lowercase();
    let mut s = String::with_capacity(lowered.len());
    for ch in lowered.chars() {
        if ch.is_ascii_alphanumeric() || matches!(ch, '.' | '/' | '-') {
            s.push(ch);
        } else {
            s.push('-');
        }
    }
    // collapse runs of '-' and trim.
    let mut out = String::with_capacity(s.len());
    let mut prev_dash = false;
    for ch in s.chars() {
        if ch == '-' {
            if !prev_dash {
                out.push('-');
            }
            prev_dash = true;
        } else {
            out.push(ch);
            prev_dash = false;
        }
    }
    out.trim_matches('-').to_string()
}

/// Model families that DISQUALIFY vision (checked first).
const VISION_EXCLUDE: &[&str] =
    &["embed", "rerank", "dall-e", "flux", "stable-diffusion", "whisper", "tts"];
/// Model families that IMPLY vision. Note `"-vl"` catches the current-gen
/// vision-language IDs (`qwen2-vl`, `qwen2.5-vl`, future `qwenN-vl`, …) that the
/// bare `"qwen-vl"` substring misses once a version digit is inserted.
const VISION_INCLUDE: &[&str] = &[
    "4o", "claude-3", "gpt-4", "gemini", "-vl", "qwen-vl", "llava", "vision", "pixtral",
    "grok-vision", "internvl", "minicpm-v", "mimo-v2.5",
];

/// Infer per-model modalities from the model name. Currently only `"vision"`.
pub fn infer_model_modalities(model: &str) -> Vec<String> {
    let base = base_model_name(model);
    let mut out = Vec::new();
    let excluded = VISION_EXCLUDE.iter().any(|k| base.contains(k));
    if !excluded && VISION_INCLUDE.iter().any(|k| base.contains(k)) {
        out.push("vision".to_string());
    }
    out
}

/// Model-name substrings that imply IMAGE generation. Kept in sync with the FE
/// `CAPABILITY_PATTERNS.image_generation`. None of these overlap `VISION_INCLUDE`,
/// so a generator name is never mis-tagged as a vision-understanding model.
const IMAGE_GENERATION_INCLUDE: &[&str] = &[
    "gpt-image",
    "dall-e",
    "dall",
    "seedream",
    "flux",
    "stable-diffusion",
    "sd-",
    "sdxl",
    "imagen",
    "midjourney",
    "mj-",
    "nano-banana",
    "kolors",
    "hidream",
    "janus",
    "cogview",
    "diffusion",
    "stabilityai",
];
/// Model-name substrings that imply VIDEO generation. Kept in sync with the FE
/// `CAPABILITY_PATTERNS.video_generation`. Deliberately specific (e.g. `wan2`/
/// `wanx` rather than a bare `wan`) to avoid false positives on chat models.
const VIDEO_GENERATION_INCLUDE: &[&str] = &[
    "sora",
    "veo",
    "kling",
    "seedance",
    "wanx",
    "wan2",
    "hailuo",
    "vidu",
    "cogvideo",
    "pixverse",
    "runway",
    "luma",
    "dream-machine",
];

/// Infer the Creative Workshop generation capabilities suggested by a model
/// NAME. Returns [`ModelType::ImageGeneration`] and/or
/// [`ModelType::VideoGeneration`] when the name matches a known generator
/// family. The result is a **suggested default** — the user may override it
/// (mirrors the `is_user_selected` semantics on provider capabilities).
///
/// This is intentionally decoupled from [`infer_model_modalities`]: generation
/// models advertise NO chat modality here, so the orchestrator's chat router is
/// unaffected.
pub fn infer_generation_capabilities(model: &str) -> Vec<ModelType> {
    let base = base_model_name(model);
    let mut out = Vec::new();
    if IMAGE_GENERATION_INCLUDE.iter().any(|k| base.contains(k)) {
        out.push(ModelType::ImageGeneration);
    }
    if VIDEO_GENERATION_INCLUDE.iter().any(|k| base.contains(k)) {
        out.push(ModelType::VideoGeneration);
    }
    out
}

#[cfg(test)]
mod tests {
    #[test]
    fn vision_models_infer_vision_modality() {
        for m in [
            "gpt-4o",
            "gpt-4o-mini",
            "claude-3-5-sonnet",
            "gemini-1.5-pro",
            "qwen-vl-max",
            "qwen2-vl-7b-instruct",
            "qwen2.5-vl-72b-instruct",
            "mimo-v2.5",
            "llava-1.6",
            "pixtral-12b",
            "some-vision-model",
        ] {
            assert!(
                super::infer_model_modalities(m).contains(&"vision".to_string()),
                "{m} should infer vision"
            );
        }
    }

    #[test]
    fn non_vision_and_excluded_models_infer_no_vision() {
        for m in [
            "text-embedding-3-large",
            "bge-reranker",
            "dall-e-3",
            "flux-schnell",
            "whisper-1",
            "deepseek-chat", /* 纯文本无视觉族 */
        ] {
            assert!(
                !super::infer_model_modalities(m).contains(&"vision".to_string()),
                "{m} should NOT infer vision"
            );
        }
    }

    #[test]
    fn base_model_name_normalizes() {
        assert_eq!(super::base_model_name("GPT-4o (Preview)!"), "gpt-4o-preview");
    }

    #[test]
    fn image_generation_models_infer_image_capability() {
        use crate::ModelType;
        for m in [
            "gpt-image-1",
            "dall-e-3",
            "seedream-3.0",
            "flux.1-schnell",
            "stable-diffusion-3.5-large",
            "sd-3.5",
            "sdxl-turbo",
            "imagen-3.0-generate",
            "midjourney-v6",
            "nano-banana",
            "kolors-2.0",
            "hidream-i1",
            "janus-pro-7b",
            "cogview-4",
        ] {
            assert!(
                super::infer_generation_capabilities(m).contains(&ModelType::ImageGeneration),
                "{m} should infer image_generation"
            );
        }
    }

    #[test]
    fn video_generation_models_infer_video_capability() {
        use crate::ModelType;
        for m in [
            "sora-2",
            "veo-3",
            "kling-v2",
            "seedance-1.0-pro",
            "wan2.2-t2v",
            "wanx-v1",
            "hailuo-02",
            "vidu-q1",
            "cogvideox-5b",
            "pixverse-v4",
            "runway-gen3",
            "luma-ray2",
            "dream-machine",
        ] {
            assert!(
                super::infer_generation_capabilities(m).contains(&ModelType::VideoGeneration),
                "{m} should infer video_generation"
            );
        }
    }

    #[test]
    fn chat_models_infer_no_generation_capability() {
        // Representative chat / embedding / rerank models must NOT be classified
        // as generators (would pollute the Creative Workshop model pickers).
        for m in [
            "gpt-4o",
            "gpt-4o-mini",
            "claude-3-5-sonnet",
            "claude-opus-4",
            "gemini-1.5-pro",
            "qwen2.5-vl-72b-instruct",
            "deepseek-chat",
            "text-embedding-3-large",
            "bge-reranker",
        ] {
            assert!(
                super::infer_generation_capabilities(m).is_empty(),
                "{m} should NOT infer any generation capability"
            );
        }
    }

    #[test]
    fn generation_models_never_infer_vision_modality() {
        // "生成模型不误判为视觉理解": image/video generators stay out of the
        // vision-understanding modality that drives the orchestrator router.
        for m in [
            "gpt-image-1",
            "dall-e-3",
            "seedream-3.0",
            "flux.1-schnell",
            "stable-diffusion-3.5",
            "midjourney-v6",
            "imagen-3.0",
            "sora-2",
            "veo-3",
            "kling-v2",
            "wan2.2-t2v",
        ] {
            assert!(
                !super::infer_model_modalities(m).contains(&"vision".to_string()),
                "{m} must not be inferred as a vision-understanding model"
            );
        }
    }
}

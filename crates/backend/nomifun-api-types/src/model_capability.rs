//! Per-model capability inference from the model NAME — the only per-model
//! signal available (provider `capabilities` is provider-level; there is no
//! user-authored per-model capability field). Ported from the frontend
//! `ui/src/common/utils/modelCapabilities.ts`. Dep-free substring matching.

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
}

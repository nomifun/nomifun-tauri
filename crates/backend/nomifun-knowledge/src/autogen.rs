//! Creation-time AI autogen: sample a base's markdown corpus, ask the LLM
//! for a registry description + root README, and parse the strict-JSON
//! reply.
//!
//! This module only owns the LLM **seam** ([`KnowledgeCompleter`]), the
//! sampling/prompt/parse pure logic, and the constants. Coordination
//! (loading the base row, writing files, emitting events) lives in
//! `service::KnowledgeService::generate_overview`. The production completer
//! implementation lives in `nomifun-ai-agent` (same layering as the companion
//! learner's `LiveCompanionCompleter`) and is late-wired via
//! `KnowledgeService::set_completer`.

use std::path::Path;

use nomifun_common::AppError;
use serde::Deserialize;

use crate::service::{KB_INBOX_REL_DIR, is_md};

/// LLM seam for knowledge autogen (same pattern as `CompanionCompleter` in
/// `nomifun-companion`). The knowledge crate holds only the trait; provider/model
/// selection is the implementor's concern.
#[async_trait::async_trait]
pub trait KnowledgeCompleter: Send + Sync {
    /// Run a one-shot completion using the implementor's default
    /// provider/model selection (the first enabled provider/model).
    async fn complete(&self, system: &str, user: &str) -> Result<String, AppError>;

    /// Run a one-shot completion against an explicitly chosen
    /// `(provider_id, model)` instead of the default. The base trait falls
    /// back to [`Self::complete`] so existing implementations (and test
    /// fakes) keep compiling and behaving unchanged; the production
    /// completer overrides this to honor the caller's pick. Used by the
    /// user-facing autogen/description endpoints where the UI lets the user
    /// pick a model, and by turn-final write-back after the conversation layer
    /// has resolved the user's knowledge-model preference or the model that
    /// actually answered the turn. Callers without an explicit provider-backed
    /// model keep using [`Self::complete`].
    async fn complete_with(
        &self,
        system: &str,
        user: &str,
        _provider_id: &str,
        _model: &str,
    ) -> Result<String, AppError> {
        self.complete(system, user).await
    }
}

/// Sampling budget: at most this many files feed the overview prompt.
pub const SAMPLE_MAX_FILES: usize = 20;
/// Sampling budget: at most this many bytes are read from each file.
pub const SAMPLE_MAX_PER_FILE: usize = 4 * 1024;
/// Sampling budget: total cap across all sampled excerpts.
pub const SAMPLE_MAX_TOTAL: usize = 60 * 1024;

/// Generated descriptions are clamped to this many chars before persisting.
pub const DESCRIPTION_MAX_CHARS: usize = 120;

/// Fetched snapshots above this size are condensed via the completer
/// (when available) before persisting.
pub const SNAPSHOT_COMPRESS_THRESHOLD: usize = 32 * 1024;
/// Cap on the markdown fed into a snapshot-compression call.
pub const SNAPSHOT_LLM_INPUT_MAX: usize = 64 * 1024;

/// System prompt for condensing an oversized fetched page. Output is plain
/// markdown (not JSON).
pub const SNAPSHOT_COMPRESS_SYSTEM: &str = "You are a knowledge-base curator. The user message is a markdown \
document fetched from a web page; it is too long to store verbatim. Rewrite it as a condensed digest:\n\
- Keep the original heading structure (#/##/###) where it carries meaning.\n\
- Keep key facts, definitions, API signatures, tables and short code snippets; drop navigation, boilerplate \
and repetition.\n\
- Write in the document's own language.\n\
Output ONLY the condensed markdown — no commentary, no fences.";

/// Strict-JSON contract for the overview generation call. Agent-facing
/// wording is English by project convention.
pub const OVERVIEW_SYSTEM: &str = "You are a knowledge-base curator. You will receive samples from a \
markdown knowledge base. Reply with ONLY a JSON object of this exact shape:\n\
{\"description\": \"...\", \"readme_markdown\": \"...\"}\n\
Rules:\n\
- description: one or two sentences (max 120 characters) stating what the base covers and when to \
consult it. Write it in the dominant language of the sampled content.\n\
- readme_markdown: a complete README.md for the base root — an H1 title, a short overview paragraph, \
and a section describing the main topics/structure so a reader can navigate the documents. Keep the \
README under ~300 lines; prefer a concise overview to exhaustive listings.\n\
- Ground everything in the samples; never invent documents or facts that are not present.\n\
- Output the JSON object only: no prose, no markdown fences.";

/// Strict-JSON contract for the stateless description-generation call
/// (create-base form, before any row exists). Description only — no README.
/// The output lands in conversation/terminal prompt contexts as
/// `- Description: ...` under a `### {name}` heading (see `context.rs`), so
/// it must double as a retrieval hint.
pub const DESCRIPTION_SYSTEM: &str = "You are a knowledge-base curator. You will receive samples from a \
markdown knowledge base. Reply with ONLY a JSON object of this exact shape:\n\
{\"description\": \"...\"}\n\
Rules:\n\
- description: one or two sentences (max 120 characters) stating what topics/content the base covers \
AND when an assistant should consult it, so a model scanning a list of bases can decide at a glance \
whether to search this one.\n\
- Write it in the dominant language of the sampled content.\n\
- Ground it in the samples; never invent topics or facts that are not present.\n\
- Output the JSON object only: no prose, no markdown fences.";

/// Strict-JSON contract for the stateless description-polish call: rewrite a
/// user-typed draft into a high-quality registry description. Same prompt
/// surface as [`DESCRIPTION_SYSTEM`] (see `context.rs` rendering).
pub const POLISH_SYSTEM: &str = "You are a knowledge-base curator. You will receive a user-written draft \
description of a knowledge base. Rewrite and polish the draft into a high-quality description. Reply \
with ONLY a JSON object of this exact shape:\n\
{\"description\": \"...\"}\n\
Rules:\n\
- description: one or two sentences (max 120 characters) stating what topics/content the base covers \
AND when an assistant should consult it, so a model scanning a list of bases can decide at a glance \
whether to search this one.\n\
- Preserve every fact and the intent of the draft; never invent capabilities, topics or facts the \
draft does not mention. This is a rewrite/polish, not free creation.\n\
- Write it in the dominant language of the draft.\n\
- Output the JSON object only: no prose, no markdown fences.";

/// Parsed model reply for the overview call.
#[derive(Debug, Default, Deserialize)]
pub struct OverviewOutput {
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub readme_markdown: String,
}

/// Extract the outermost `{...}` block from a raw model reply — the shared
/// tolerance step of both parsers below (strips ```json fences and
/// surrounding prose; same approach as the companion learner's parser).
fn extract_json_block(raw: &str) -> Result<&str, String> {
    let start = raw.find('{').ok_or_else(|| "no JSON object found in model output".to_owned())?;
    let end = raw.rfind('}').filter(|e| *e > start).ok_or_else(|| "no JSON object found in model output".to_owned())?;
    Ok(&raw[start..=end])
}

/// Parse the model output into [`OverviewOutput`], tolerating ```json fences
/// and surrounding prose (extracts the outermost `{...}` block — same
/// tolerance as the companion learner's parser).
pub fn parse_overview_output(raw: &str) -> Result<OverviewOutput, String> {
    let output: OverviewOutput =
        serde_json::from_str(extract_json_block(raw)?).map_err(|e| format!("invalid overview JSON: {e}"))?;
    if output.description.trim().is_empty() && output.readme_markdown.trim().is_empty() {
        return Err("overview JSON carries neither description nor readme_markdown".into());
    }
    Ok(output)
}

/// Parse a description-only reply (`{"description": "..."}`) with the same
/// tolerance as [`parse_overview_output`]: ```json fences and surrounding
/// prose are stripped by extracting the outermost `{...}` block. An empty
/// description is a parse failure (callers retry once).
pub fn parse_description_output(raw: &str) -> Result<String, String> {
    #[derive(Deserialize)]
    struct DescriptionOutput {
        #[serde(default)]
        description: String,
    }
    let output: DescriptionOutput =
        serde_json::from_str(extract_json_block(raw)?).map_err(|e| format!("invalid description JSON: {e}"))?;
    let description = output.description.trim();
    if description.is_empty() {
        return Err("description JSON carries an empty description".into());
    }
    Ok(description.to_owned())
}

/// Clamp a model-produced description to [`DESCRIPTION_MAX_CHARS`] — the same
/// bound the overview path enforces before persisting (char-based, so a
/// multi-byte boundary can never split).
pub fn clamp_description(raw: &str) -> String {
    raw.trim().chars().take(DESCRIPTION_MAX_CHARS).collect()
}

/// Build the user prompt from the base registry info and sampled excerpts.
pub fn build_overview_prompt(name: &str, description: &str, samples: &[(String, String)]) -> String {
    let mut prompt = format!("Knowledge base name: {name}\n");
    let description = description.trim();
    if !description.is_empty() {
        prompt.push_str(&format!("Current description: {description}\n"));
    }
    prompt.push_str(&format!("Sampled documents ({}):\n", samples.len()));
    for (rel, excerpt) in samples {
        prompt.push_str(&format!("\n--- FILE: {rel} ---\n{excerpt}\n"));
    }
    prompt.push_str("\nReply with the JSON object now.");
    prompt
}

/// Build the user prompt for the stateless description-generation call.
/// `name` may be blank (the create form lets users ask before naming).
pub fn build_description_prompt(name: &str, samples: &[(String, String)]) -> String {
    let mut prompt = String::new();
    let name = name.trim();
    if !name.is_empty() {
        prompt.push_str(&format!("Knowledge base name: {name}\n"));
    }
    prompt.push_str(&format!("Sampled documents ({}):\n", samples.len()));
    for (rel, excerpt) in samples {
        prompt.push_str(&format!("\n--- FILE: {rel} ---\n{excerpt}\n"));
    }
    prompt.push_str("\nReply with the JSON object now.");
    prompt
}

/// Build the user prompt for the stateless description-polish call.
/// `name` may be blank; `draft` is the user's raw description text.
pub fn build_polish_prompt(name: &str, draft: &str) -> String {
    let mut prompt = String::new();
    let name = name.trim();
    if !name.is_empty() {
        prompt.push_str(&format!("Knowledge base name: {name}\n"));
    }
    prompt.push_str(&format!("Draft description:\n{}\n", draft.trim()));
    prompt.push_str("\nReply with the JSON object now.");
    prompt
}

/// Sample the markdown corpus under `root` for the overview prompt:
/// `_inbox/` (unreviewed staged write-backs) and the root `README.md` (the
/// artifact being regenerated) are excluded; files are taken in sorted-path
/// order up to [`SAMPLE_MAX_FILES`], each excerpt capped at
/// [`SAMPLE_MAX_PER_FILE`] bytes, total capped at [`SAMPLE_MAX_TOTAL`].
pub async fn sample_base_files(root: &Path) -> Vec<(String, String)> {
    let root = root.to_path_buf();
    tokio::task::spawn_blocking(move || sample_base_files_blocking(&root))
        .await
        .unwrap_or_default()
}

fn sample_base_files_blocking(root: &Path) -> Vec<(String, String)> {
    if !root.is_dir() {
        return Vec::new();
    }
    let mut rels: Vec<String> = walkdir::WalkDir::new(root)
        .into_iter()
        .flatten()
        .filter(|e| e.file_type().is_file() && is_md(e.path()))
        .filter_map(|e| {
            let rel = e.path().strip_prefix(root).ok()?.to_string_lossy().replace('\\', "/");
            let keep = !rel.starts_with(&format!("{KB_INBOX_REL_DIR}/")) && rel != "README.md";
            keep.then_some(rel)
        })
        .collect();
    rels.sort();

    let mut samples = Vec::new();
    let mut total = 0usize;
    for rel in rels.into_iter().take(SAMPLE_MAX_FILES) {
        if total >= SAMPLE_MAX_TOTAL {
            break;
        }
        let budget = SAMPLE_MAX_PER_FILE.min(SAMPLE_MAX_TOTAL - total);
        let Some(excerpt) = read_prefix_lossy(&root.join(&rel), budget) else {
            continue;
        };
        if excerpt.trim().is_empty() {
            continue;
        }
        total += excerpt.len();
        samples.push((rel, excerpt));
    }
    samples
}

/// Read at most `limit` bytes from the start of `path`, lossily decoded
/// (a multi-byte char cut at the boundary degrades to U+FFFD, never a panic).
fn read_prefix_lossy(path: &Path, limit: usize) -> Option<String> {
    use std::io::Read;
    let mut file = std::fs::File::open(path).ok()?;
    let mut buf = vec![0u8; limit];
    let mut read = 0usize;
    loop {
        match file.read(&mut buf[read..]) {
            Ok(0) => break,
            Ok(n) => read += n,
            Err(_) => return None,
        }
        if read == buf.len() {
            break;
        }
    }
    Some(String::from_utf8_lossy(&buf[..read]).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_tolerates_fences_and_prose() {
        let plain = r##"{"description":"覆盖部署与运维。","readme_markdown":"# 运维库\n\n概览。"}"##;
        let out = parse_overview_output(plain).unwrap();
        assert_eq!(out.description, "覆盖部署与运维。");
        assert!(out.readme_markdown.starts_with("# 运维库"));

        let fenced = format!("Sure, here you go:\n```json\n{plain}\n```\ndone");
        let out = parse_overview_output(&fenced).unwrap();
        assert_eq!(out.description, "覆盖部署与运维。");

        assert!(parse_overview_output("I cannot do that").is_err());
        assert!(parse_overview_output(r#"{"description":"","readme_markdown":""}"#).is_err());
        // Missing fields default to empty (partial output still usable).
        let only_desc = parse_overview_output(r#"{"description":"d"}"#).unwrap();
        assert_eq!(only_desc.readme_markdown, "");
    }

    #[tokio::test]
    async fn sampling_skips_inbox_and_readme_and_caps_budgets() {
        let dir = tempfile::TempDir::new().unwrap();
        let root = dir.path();
        std::fs::write(root.join("README.md"), "# old readme").unwrap();
        let inbox = root.join("_inbox/0190f5fe-7c00-7a00-8000-000000000001");
        std::fs::create_dir_all(&inbox).unwrap();
        std::fs::write(inbox.join("draft.md"), "# draft").unwrap();
        // 25 real files, one larger than the per-file cap.
        for i in 0..25 {
            std::fs::write(root.join(format!("f{i:02}.md")), format!("# 文件 {i}\n正文")).unwrap();
        }
        std::fs::write(root.join("big.md"), "x".repeat(SAMPLE_MAX_PER_FILE * 2)).unwrap();

        let samples = sample_base_files(root).await;
        assert_eq!(samples.len(), SAMPLE_MAX_FILES, "{:?}", samples.iter().map(|s| &s.0).collect::<Vec<_>>());
        assert!(samples.iter().all(|(rel, _)| rel != "README.md" && !rel.starts_with("_inbox/")));
        let big = samples.iter().find(|(rel, _)| rel == "big.md").expect("big.md sampled (sorted first)");
        assert!(big.1.len() <= SAMPLE_MAX_PER_FILE);
        let total: usize = samples.iter().map(|(_, s)| s.len()).sum();
        assert!(total <= SAMPLE_MAX_TOTAL);
    }

    #[tokio::test]
    async fn sampling_empty_or_missing_root() {
        let dir = tempfile::TempDir::new().unwrap();
        assert!(sample_base_files(dir.path()).await.is_empty());
        assert!(sample_base_files(&dir.path().join("nope")).await.is_empty());
    }

    #[test]
    fn prompt_carries_name_and_samples() {
        let samples = vec![("a.md".to_string(), "# A\nbody".to_string())];
        let prompt = build_overview_prompt("领域知识", "旧描述", &samples);
        assert!(prompt.contains("领域知识"));
        assert!(prompt.contains("旧描述"));
        assert!(prompt.contains("--- FILE: a.md ---"));
        assert!(prompt.contains("# A\nbody"));
        // Empty current description line is omitted.
        let prompt = build_overview_prompt("x", "  ", &samples);
        assert!(!prompt.contains("Current description"));
    }

    #[test]
    fn parse_description_tolerates_fences_and_prose() {
        let plain = r#"{"description":"覆盖部署与运维，排障时查阅。"}"#;
        assert_eq!(parse_description_output(plain).unwrap(), "覆盖部署与运维，排障时查阅。");

        let fenced = format!("Sure!\n```json\n{plain}\n```\nthat's it");
        assert_eq!(parse_description_output(&fenced).unwrap(), "覆盖部署与运维，排障时查阅。");

        // Whitespace-padded description is trimmed.
        assert_eq!(parse_description_output(r#"{"description":"  d  "}"#).unwrap(), "d");

        // No JSON / invalid JSON / empty or missing description all fail.
        assert!(parse_description_output("I cannot do that").is_err());
        assert!(parse_description_output("{not json}").is_err());
        assert!(parse_description_output(r#"{"description":""}"#).is_err());
        assert!(parse_description_output(r#"{"other":"x"}"#).is_err());
    }

    #[test]
    fn clamp_description_caps_chars_and_trims() {
        assert_eq!(clamp_description("  short  "), "short");
        let long = "知".repeat(DESCRIPTION_MAX_CHARS + 80);
        let clamped = clamp_description(&long);
        assert_eq!(clamped.chars().count(), DESCRIPTION_MAX_CHARS);
        // Exactly at the cap → kept whole.
        let exact = "k".repeat(DESCRIPTION_MAX_CHARS);
        assert_eq!(clamp_description(&exact), exact);
    }

    #[test]
    fn description_prompt_carries_optional_name_and_samples() {
        let samples = vec![("guide.md".to_string(), "# 指南\n正文".to_string())];
        let prompt = build_description_prompt("运维库", &samples);
        assert!(prompt.contains("Knowledge base name: 运维库"));
        assert!(prompt.contains("--- FILE: guide.md ---"));
        assert!(prompt.contains("# 指南\n正文"));
        // Blank name → the name line is omitted entirely.
        let prompt = build_description_prompt("  ", &samples);
        assert!(!prompt.contains("Knowledge base name"));
        assert!(prompt.contains("Sampled documents (1):"));
    }

    #[test]
    fn polish_prompt_carries_optional_name_and_draft() {
        let prompt = build_polish_prompt("运维库", "  记录一些部署的东西  ");
        assert!(prompt.contains("Knowledge base name: 运维库"));
        assert!(prompt.contains("Draft description:\n记录一些部署的东西\n"));
        let prompt = build_polish_prompt("", "draft text");
        assert!(!prompt.contains("Knowledge base name"));
        assert!(prompt.contains("draft text"));
    }
}

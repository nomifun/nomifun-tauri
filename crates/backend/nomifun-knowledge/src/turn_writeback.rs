//! Turn-final knowledge write-back extraction.
//!
//! This module is pure prompt/parse logic. The service layer owns provider calls,
//! write policy, and disk writes.

use nomifun_api_types::KnowledgeMountInfo;
use nomifun_common::KnowledgeBaseId;
use serde::Deserialize;

use crate::context::WritebackEagerness;

const USER_TEXT_MAX_CHARS: usize = 12_000;
const ASSISTANT_TEXT_MAX_CHARS: usize = 24_000;
const TOC_LINES_PER_BASE_MAX: usize = 24;

/// Strict-JSON contract for extracting durable knowledge from one completed turn.
pub const TURN_WRITEBACK_SYSTEM: &str = "You are a knowledge-base curator for NomiFun. \
Read one completed conversation turn and decide whether anything should be proposed \
for knowledge-base write-back. Output ONLY a JSON object of this exact shape:\n\
{\"candidates\":[{\"kb_id\":\"...\",\"rel_path\":\"path/inside/base.md\",\"content\":\"markdown body\"}]}\n\
Rules:\n\
- Only write reusable knowledge that will help future sessions using one of the mounted bases.\n\
- Do not write one-off task status, transient debugging notes, chat narration, or facts already obvious from the current code/git state.\n\
- Do not write secrets, tokens, credentials, private identifiers, or sensitive personal data.\n\
- Ground every candidate in the provided turn. Never invent facts.\n\
- Pick exactly one mounted kb_id for each candidate.\n\
- Return at most 8 candidates.\n\
- rel_path must be a relative markdown path inside that base, never absolute, never under _inbox/.\n\
- content must be concise markdown, organized as a durable note.\n\
- If nothing qualifies, return {\"candidates\":[]}.";

#[derive(Debug, Clone, Deserialize)]
pub struct TurnWritebackCandidate {
    pub kb_id: KnowledgeBaseId,
    #[serde(default)]
    pub rel_path: String,
    #[serde(default)]
    pub content: String,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct TurnWritebackOutput {
    #[serde(default)]
    pub candidates: Vec<TurnWritebackCandidate>,
}

pub fn build_turn_writeback_prompt(
    mounts: &[KnowledgeMountInfo],
    eagerness: WritebackEagerness,
    user_text: &str,
    assistant_text: &str,
) -> String {
    let eagerness_label = match eagerness {
        WritebackEagerness::Conservative => "conservative",
        WritebackEagerness::Aggressive => "aggressive",
    };
    let eagerness_rule = match eagerness {
        WritebackEagerness::Conservative => {
            "Conservative: require clear durable value and high confidence. Prefer no candidate over a noisy one."
        }
        WritebackEagerness::Aggressive => {
            "Aggressive: include anything plausibly reusable for a mounted base, while still obeying all no-noise and no-secret gates."
        }
    };

    let mut prompt = String::new();
    prompt.push_str("Write-back extraction settings:\n");
    prompt.push_str(&format!("- eagerness: {eagerness_label}\n"));
    prompt.push_str(&format!("- rule: {eagerness_rule}\n\n"));
    prompt.push_str("Mounted knowledge bases:\n");
    for m in mounts {
        prompt.push_str(&format!("- kb_id: {}\n", m.knowledge_base_id));
        prompt.push_str(&format!("  name: {}\n", m.name));
        if !m.description.trim().is_empty() {
            prompt.push_str(&format!("  description: {}\n", one_line(&m.description)));
        }
        if let Some(summary) = m.summary.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
            prompt.push_str(&format!("  summary: {}\n", one_line(summary)));
        }
        prompt.push_str(&format!("  mount_path: {}\n", m.rel_path));
        if !m.toc.is_empty() {
            prompt.push_str("  known_paths:\n");
            for line in m.toc.iter().take(TOC_LINES_PER_BASE_MAX) {
                prompt.push_str(&format!("    - {}\n", one_line(line)));
            }
        }
    }
    prompt.push_str("\nCompleted turn:\n<user>\n");
    prompt.push_str(&truncate_chars(user_text, USER_TEXT_MAX_CHARS));
    prompt.push_str("\n</user>\n<assistant>\n");
    prompt.push_str(&bounded_assistant_text(assistant_text));
    prompt.push_str("\n</assistant>\n");
    prompt
}

pub fn parse_turn_writeback_output(raw: &str) -> Result<TurnWritebackOutput, String> {
    let slice = extract_json_object(raw).ok_or_else(|| "no JSON object found".to_owned())?;
    serde_json::from_str::<TurnWritebackOutput>(slice).map_err(|e| format!("invalid turn writeback JSON: {e}"))
}

/// Exact assistant slice used by the turn-final prompt. Conversation state
/// persists only this bounded value so a manual retry reproduces a multi-segment
/// answer without duplicating an arbitrarily large transcript in message JSON.
pub fn bounded_assistant_text(text: &str) -> String {
    truncate_chars(text, ASSISTANT_TEXT_MAX_CHARS)
}

fn one_line(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn truncate_chars(s: &str, max_chars: usize) -> String {
    let trimmed = s.trim();
    // A durable retry may feed back the exact already-bounded prompt slice.
    // Keep the truncation marker intact instead of clipping it a second time.
    if trimmed.ends_with("\n[truncated]")
        && trimmed.chars().count() <= max_chars + "\n[truncated]".chars().count()
    {
        return trimmed.to_owned();
    }
    if trimmed.chars().count() <= max_chars {
        return trimmed.to_owned();
    }
    let mut out: String = trimmed.chars().take(max_chars).collect();
    out.push_str("\n[truncated]");
    out
}

fn extract_json_object(raw: &str) -> Option<&str> {
    let bytes = raw.as_bytes();
    let start = raw.find('{')?;
    let mut depth = 0i32;
    let mut in_string = false;
    let mut escaped = false;

    for i in start..bytes.len() {
        let c = bytes[i];
        if in_string {
            if escaped {
                escaped = false;
            } else if c == b'\\' {
                escaped = true;
            } else if c == b'"' {
                in_string = false;
            }
            continue;
        }

        match c {
            b'"' => in_string = true,
            b'{' => depth += 1,
            b'}' => {
                depth -= 1;
                if depth == 0 {
                    return Some(&raw[start..=i]);
                }
            }
            _ => {}
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_tolerates_fences_and_surrounding_text() {
        let kb_id = KnowledgeBaseId::new();
        let raw = format!(
            "```json\n{{\"candidates\":[{{\"kb_id\":\"{kb_id}\",\"rel_path\":\"a.md\",\"content\":\"# A\"}}]}}\n```"
        );
        let out = parse_turn_writeback_output(&raw).unwrap();
        assert_eq!(out.candidates.len(), 1);
        assert_eq!(out.candidates[0].kb_id, kb_id);
        assert_eq!(out.candidates[0].rel_path, "a.md");
    }

    #[test]
    fn parse_rejects_noncanonical_knowledge_base_id() {
        let raw = r##"{"candidates":[{"kb_id":"kb_1","rel_path":"a.md","content":"# A"}]}"##;
        assert!(parse_turn_writeback_output(raw).is_err());
    }

    #[test]
    fn prompt_labels_eagerness_without_changing_placement() {
        let prompt = build_turn_writeback_prompt(
            &[KnowledgeMountInfo {
                knowledge_base_id: nomifun_common::KnowledgeBaseId::new(),
                name: "Ops".into(),
                description: String::new(),
                rel_path: ".nomi/knowledge/Ops".into(),
                toc: vec!["runbook.md — Runbook".into()],
                summary: None,
                live_sources: Vec::new(),
            }],
            WritebackEagerness::Aggressive,
            "u",
            "a",
        );
        assert!(prompt.contains("eagerness: aggressive"));
        assert!(prompt.contains("runbook.md"));
        assert!(!prompt.contains("_inbox/{"));
    }

}

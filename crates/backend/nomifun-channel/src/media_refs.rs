//! Pure helpers that extract workshop asset ids (`wsa_…`) from the two
//! machine-readable signals a channel turn carries: a completed
//! `nomi_workshop_*` tool call's `output` JSON (`result_asset_ids`), and any
//! `/api/workshop/files/{id}` URL the assistant wrote into its visible text
//! (the same link the desktop renders). Both are deduped by the caller.

use regex::Regex;
use std::sync::OnceLock;

/// Matches a workshop capability URL and captures the asset id, e.g.
/// `/api/workshop/files/wsa_01H…` (host optional, `?thumb=1` tolerated).
fn files_url_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"/api/workshop/files/(wsa_[A-Za-z0-9]+)").unwrap())
}

/// Extract asset ids from a completed tool call's `output` string by parsing it
/// as JSON and collecting every `result_asset_ids` string array found anywhere
/// in the tree. Non-JSON or absent key → empty.
pub fn asset_ids_from_tool_output(output: &str) -> Vec<String> {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(output) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    collect_result_asset_ids(&value, &mut out);
    out
}

fn collect_result_asset_ids(value: &serde_json::Value, out: &mut Vec<String>) {
    match value {
        serde_json::Value::Object(map) => {
            for (k, v) in map {
                if k == "result_asset_ids"
                    && let Some(arr) = v.as_array()
                {
                    for item in arr {
                        if let Some(s) = item.as_str() {
                            out.push(s.to_owned());
                        }
                    }
                }
                collect_result_asset_ids(v, out);
            }
        }
        serde_json::Value::Array(arr) => {
            for v in arr {
                collect_result_asset_ids(v, out);
            }
        }
        _ => {}
    }
}

/// Extract asset ids from assistant text by matching workshop capability URLs.
pub fn asset_ids_from_text(text: &str) -> Vec<String> {
    files_url_re()
        .captures_iter(text)
        .map(|c| c[1].to_owned())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_output_top_level_ids() {
        let out = r#"{"status":"succeeded","result_asset_ids":["wsa_1","wsa_2"]}"#;
        assert_eq!(asset_ids_from_tool_output(out), vec!["wsa_1", "wsa_2"]);
    }

    #[test]
    fn tool_output_nested_ids() {
        let out = r#"{"data":{"task":{"result_asset_ids":["wsa_9"]}}}"#;
        assert_eq!(asset_ids_from_tool_output(out), vec!["wsa_9"]);
    }

    #[test]
    fn tool_output_non_json_or_missing_is_empty() {
        assert!(asset_ids_from_tool_output("not json").is_empty());
        assert!(asset_ids_from_tool_output(r#"{"status":"running"}"#).is_empty());
    }

    #[test]
    fn text_extracts_capability_urls() {
        let text = "图来咯～ ![cat](/api/workshop/files/wsa_abc123) and http://127.0.0.1:8080/api/workshop/files/wsa_def456?thumb=1";
        assert_eq!(asset_ids_from_text(text), vec!["wsa_abc123", "wsa_def456"]);
    }

    #[test]
    fn text_without_urls_is_empty() {
        assert!(asset_ids_from_text("just text, no image").is_empty());
    }
}

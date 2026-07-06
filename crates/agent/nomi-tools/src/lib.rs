pub mod apply_patch;
pub mod bash;
pub mod edit;
pub mod exec_command;
pub mod file_cache;
pub mod glob;
pub mod grep;
pub mod lsp;
pub mod output_truncation;
pub mod path_guard;
pub mod persistent_shell;
pub mod process_store;
pub mod pty;
pub mod read;
pub mod registry;
pub mod sandbox;
pub mod tool_search;
pub mod update_plan;
pub mod worktree;
pub mod write;
pub mod write_stdin;

/// Shared test-only helpers (path to the cross-platform `pty_test_helper` bin).
#[cfg(test)]
pub(crate) mod test_support;

pub use output_truncation::{TruncationBudget, approx_token_count, truncate_middle};

use async_trait::async_trait;
use serde_json::Value;

use nomi_config::hooks::HooksConfig;
use nomi_protocol::events::ToolCategory;
use nomi_types::skill_types::ContextModifier;
use nomi_types::tool::{JsonSchema, ToolResult};

/// Truncate a string to at most `max_bytes`, snapping to a char boundary.
pub fn truncate_utf8(s: &str, max_bytes: usize) -> &str {
    if s.len() <= max_bytes {
        return s;
    }
    let mut end = max_bytes;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

/// Normalize a model-supplied tool `input` against the tool's JSON `schema`.
///
/// Many LLM providers — especially OpenAI-compatible / non-Anthropic models —
/// serialize *nested* tool arguments as JSON **strings** instead of real JSON,
/// e.g. `{"tasks": "[{...}]"}` rather than `{"tasks": [{...}]}`. That string then
/// fails every `.as_array()` / `.as_object()` a tool does on the field, so a
/// perfectly valid call is rejected ("Missing or invalid 'tasks' array") through
/// no fault of the user's. This applies to *any* tool with a nested array/object
/// argument, so it is fixed once, centrally, for all tools.
///
/// For each top-level property the schema declares as `array` or `object`, if
/// the model passed a string that parses back to that exact shape, replace it
/// with the parsed value. As a last resort, unwrap a fully-stringified argument
/// object (`input` itself being a JSON-object string).
///
/// Conservative and non-lossy: only rewrites `string -> (array|object)` when the
/// schema asks for that shape AND the string parses to it. A property the schema
/// types as `string` is never touched, even if its value looks like JSON; an
/// unparseable or wrong-shape string is left exactly as-is so the tool still
/// produces its own precise validation error.
pub fn coerce_input_to_schema(schema: &JsonSchema, input: Value) -> Value {
    // Last resort: the whole argument object arrived as a JSON string.
    let mut input = match input {
        Value::String(ref s) => match serde_json::from_str::<Value>(s) {
            Ok(parsed @ Value::Object(_)) => parsed,
            _ => return input,
        },
        other => other,
    };

    let Some(props) = schema.get("properties").and_then(Value::as_object) else {
        return input;
    };
    let Some(obj) = input.as_object_mut() else {
        return input;
    };

    for (key, prop_schema) in props {
        let expected = schema_type_names(prop_schema);
        if expected.iter().any(|t| *t == "string") {
            continue;
        }
        // Copy the string out (releasing the borrow) so the insert below can mutate `obj`.
        let Some(s) = obj.get(key).and_then(Value::as_str).map(str::to_owned) else {
            continue;
        };
        if let Some(coerced) = coerce_string_to_types(&s, &expected) {
            obj.insert(key.clone(), coerced);
        }
    }
    input
}

fn schema_type_names(schema: &Value) -> Vec<&str> {
    let mut types = Vec::new();
    collect_schema_type_names(schema, &mut types);
    types
}

fn collect_schema_type_names<'a>(schema: &'a Value, out: &mut Vec<&'a str>) {
    match schema.get("type") {
        Some(Value::String(s)) => out.push(s.as_str()),
        Some(Value::Array(items)) => {
            for item in items {
                if let Some(s) = item.as_str() {
                    out.push(s);
                }
            }
        }
        _ => {}
    }
    for key in ["oneOf", "anyOf"] {
        if let Some(items) = schema.get(key).and_then(Value::as_array) {
            for item in items {
                collect_schema_type_names(item, out);
            }
        }
    }
}

fn coerce_string_to_types(s: &str, expected: &[&str]) -> Option<Value> {
    if expected.iter().any(|t| *t == "array" || *t == "object") {
        if let Ok(parsed) = serde_json::from_str::<Value>(s) {
            if (expected.contains(&"array") && parsed.is_array())
                || (expected.contains(&"object") && parsed.is_object())
            {
                return Some(parsed);
            }
        }
    }

    let trimmed = s.trim();
    if trimmed.is_empty() {
        return None;
    }
    if expected.contains(&"integer") {
        if let Ok(n) = trimmed.parse::<i64>() {
            return Some(Value::Number(n.into()));
        }
    }
    if expected.contains(&"number") {
        if let Ok(n) = trimmed.parse::<f64>() {
            return serde_json::Number::from_f64(n).map(Value::Number);
        }
    }
    if expected.contains(&"boolean") {
        if trimmed.eq_ignore_ascii_case("true") {
            return Some(Value::Bool(true));
        }
        if trimmed.eq_ignore_ascii_case("false") {
            return Some(Value::Bool(false));
        }
    }
    None
}

/// Write `content` to `file_path` atomically: write to a uniquely-named temp
/// file in the same directory, then rename it over the target. Rename is atomic
/// on the same filesystem, so a crash or a concurrent reader never observes a
/// half-written file. Falls back to a direct write only if the rename fails
/// (e.g. cross-device). Shared by the Edit and Write tools so both get the same
/// crash-safety guarantee.
pub(crate) fn atomic_write(file_path: &str, content: &str) -> std::io::Result<()> {
    use std::sync::atomic::{AtomicU64, Ordering};
    static TMP_SEQ: AtomicU64 = AtomicU64::new(0);

    let seq = TMP_SEQ.fetch_add(1, Ordering::Relaxed);
    let tmp_path = format!("{}.tmp.{}.{}", file_path, std::process::id(), seq);

    if let Err(e) = std::fs::write(&tmp_path, content) {
        let _ = std::fs::remove_file(&tmp_path);
        return Err(e);
    }
    if std::fs::rename(&tmp_path, file_path).is_err() {
        // Cross-device rename (temp and target on different filesystems) cannot
        // be atomic; clean up the temp and fall back to a direct write.
        let _ = std::fs::remove_file(&tmp_path);
        std::fs::write(file_path, content)?;
    }
    Ok(())
}

/// A tool that the agent can invoke
#[async_trait]
pub trait Tool: Send + Sync {
    /// Tool name (must match API schema)
    fn name(&self) -> &str;

    /// Human-readable description for the LLM
    fn description(&self) -> &str;

    /// JSON Schema for input parameters
    fn input_schema(&self) -> JsonSchema;

    /// Whether this tool is safe to run concurrently
    fn is_concurrency_safe(&self, input: &Value) -> bool;

    /// Execute the tool
    async fn execute(&self, input: Value) -> ToolResult;

    /// Return an optional context modifier based on the tool input.
    /// Called after execute() to collect any engine-level overrides.
    /// Only SkillTool overrides this; all other tools return None.
    fn context_modifier_for(&self, _input: &Value) -> Option<ContextModifier> {
        None
    }

    /// Return any hooks declared in the skill's frontmatter for dynamic registration.
    /// Called after a successful execute() so the orchestration layer can merge
    /// the returned hooks into the active HookEngine.
    /// Only SkillTool overrides this; all other tools return None.
    fn skill_hooks_for(&self, _input: &Value) -> Option<HooksConfig> {
        None
    }

    /// Max result size in chars before truncation
    fn max_result_size(&self) -> usize {
        50_000
    }

    /// Tool category for protocol classification
    fn category(&self) -> ToolCategory;

    /// Category for a specific invocation. Lets multi-action tools (e.g.
    /// Computer/Browser) report read-only actions as Info so approval
    /// gating can distinguish them from mutating actions.
    fn category_for(&self, _input: &Value) -> ToolCategory {
        self.category()
    }

    /// Whether this tool's schema should be deferred (sent as name-only stub).
    /// Override to `true` for tools with large schemas or infrequent use.
    fn is_deferred(&self) -> bool {
        false
    }

    /// Human-readable description of what the tool will do with the given input
    fn describe(&self, input: &Value) -> String {
        format!(
            "{}: {}",
            self.name(),
            serde_json::to_string(input).unwrap_or_default()
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncate_utf8_ascii_within_limit() {
        assert_eq!(truncate_utf8("hello", 80), "hello");
    }

    #[test]
    fn truncate_utf8_ascii_at_boundary() {
        assert_eq!(truncate_utf8("abcde", 3), "abc");
    }

    #[test]
    fn truncate_utf8_multibyte_snaps_back() {
        // '些' is 3 bytes (E4 BA 9B) starting at index 79 would span 79..82
        let s = "# 用 script 模拟 TTY 交互来添加 DeepSeek 提供商\n# 首先看看有哪些";
        let result = truncate_utf8(s, 80);
        assert!(result.len() <= 80);
        assert!(result.is_char_boundary(result.len()));
    }

    #[test]
    fn truncate_utf8_empty() {
        assert_eq!(truncate_utf8("", 80), "");
    }

    #[test]
    fn truncate_utf8_zero_limit() {
        assert_eq!(truncate_utf8("hello", 0), "");
    }

    #[test]
    fn truncate_utf8_emoji() {
        // 🦀 is 4 bytes
        let s = "aaa🦀bbb";
        assert_eq!(truncate_utf8(s, 4), "aaa");
        assert_eq!(truncate_utf8(s, 7), "aaa🦀");
    }

    #[test]
    fn coerce_parses_stringified_array_property() {
        // The reported bug: a provider sent `tasks` as a JSON string.
        let schema = serde_json::json!({
            "type": "object",
            "properties": { "tasks": { "type": "array" } },
            "required": ["tasks"]
        });
        let input = serde_json::json!({ "tasks": "[{\"name\":\"a\",\"prompt\":\"p\"}]" });
        let out = coerce_input_to_schema(&schema, input);
        assert!(
            out["tasks"].is_array(),
            "stringified array must be parsed back"
        );
        assert_eq!(out["tasks"][0]["name"], "a");
        assert_eq!(out["tasks"][0]["prompt"], "p");
    }

    #[test]
    fn coerce_parses_stringified_object_property() {
        let schema = serde_json::json!({ "properties": { "config": { "type": "object" } } });
        let out = coerce_input_to_schema(&schema, serde_json::json!({ "config": "{\"a\":1}" }));
        assert!(out["config"].is_object());
        assert_eq!(out["config"]["a"], 1);
    }

    #[test]
    fn coerce_parses_stringified_scalar_properties() {
        let schema = serde_json::json!({
            "properties": {
                "conversation_id": { "type": "integer" },
                "limit": { "type": "integer" },
                "timeout_secs": { "type": "number" },
                "confirm": { "type": "boolean" }
            }
        });
        let input = serde_json::json!({
            "conversation_id": "8",
            "limit": "50",
            "timeout_secs": "1.5",
            "confirm": "true"
        });
        let out = coerce_input_to_schema(&schema, input);
        assert_eq!(out["conversation_id"], 8);
        assert_eq!(out["limit"], 50);
        assert_eq!(out["timeout_secs"], 1.5);
        assert_eq!(out["confirm"], true);
    }

    #[test]
    fn coerce_leaves_proper_values_untouched() {
        let schema = serde_json::json!({ "properties": { "tasks": { "type": "array" } } });
        let input = serde_json::json!({ "tasks": [{ "name": "a" }] });
        assert_eq!(coerce_input_to_schema(&schema, input.clone()), input);
    }

    #[test]
    fn coerce_never_mangles_string_typed_props_or_bad_json() {
        // A `string`-typed prop that merely looks like JSON must NOT be parsed;
        // an unparseable / wrong-shape string for an array prop is left as-is so
        // the tool still emits its own precise error.
        let schema = serde_json::json!({
            "properties": {
                "note": { "type": "string" },
                "tasks": { "type": "array" }
            }
        });
        let input = serde_json::json!({ "note": "[1,2,3]", "tasks": "not json" });
        assert_eq!(coerce_input_to_schema(&schema, input.clone()), input);

        // A string that parses to the WRONG shape (object where array expected) is left as-is.
        let obj_for_array = serde_json::json!({ "tasks": "{\"x\":1}" });
        assert_eq!(
            coerce_input_to_schema(&schema, obj_for_array.clone()),
            obj_for_array
        );
    }

    #[test]
    fn coerce_unwraps_fully_stringified_argument_object() {
        let schema = serde_json::json!({ "properties": { "tasks": { "type": "array" } } });
        let input = serde_json::json!("{\"tasks\":[{\"name\":\"a\",\"prompt\":\"p\"}]}");
        let out = coerce_input_to_schema(&schema, input);
        assert!(out["tasks"].is_array());
        assert_eq!(out["tasks"][0]["name"], "a");
    }

    #[test]
    fn coerce_is_noop_without_schema_properties_or_on_non_object_input() {
        let schema = serde_json::json!({ "type": "object" }); // no properties
        let input = serde_json::json!({ "tasks": "[1]" });
        assert_eq!(coerce_input_to_schema(&schema, input.clone()), input);
        // Non-object, non-string input passes through.
        let arr = serde_json::json!([1, 2, 3]);
        assert_eq!(coerce_input_to_schema(&schema, arr.clone()), arr);
    }
}

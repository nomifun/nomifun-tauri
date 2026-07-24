// Configuration-driven provider compatibility layer.
// Each provider type has default presets; users can override any field via config.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

/// Provider-level compatibility settings.
/// Each field is Option — None means "use provider-type default".
#[derive(Debug, Clone, Deserialize, Serialize, Default)]
pub struct ProviderCompat {
    /// Field name for max tokens in request body.
    /// Default: "max_tokens" for all providers.
    pub max_tokens_field: Option<String>,

    /// Merge consecutive assistant messages (text concat + tool_calls merge).
    /// Default: true for openai.
    pub merge_assistant_messages: Option<bool>,

    /// Remove tool_use blocks that have no corresponding tool_result.
    /// Default: true for openai.
    pub clean_orphan_tool_calls: Option<bool>,

    /// Deduplicate tool results with same tool_call_id (keep last).
    /// Default: true for openai.
    pub dedup_tool_results: Option<bool>,

    /// Ensure messages alternate user/assistant (insert filler if needed).
    /// Default: true for anthropic/bedrock/vertex.
    pub ensure_alternation: Option<bool>,

    /// Merge consecutive same-role messages into one.
    /// Default: true for anthropic/bedrock/vertex.
    pub merge_same_role: Option<bool>,

    /// Sanitize JSON schemas for strict providers (remove additionalProperties, etc.).
    /// Default: true for bedrock.
    pub sanitize_schema: Option<bool>,

    /// Text patterns to strip from message history before sending.
    /// Default: empty.
    pub strip_patterns: Option<Vec<String>>,

    /// Auto-generate tool IDs when missing.
    /// Default: true for anthropic/bedrock/vertex.
    pub auto_tool_id: Option<bool>,

    /// Custom API path appended to base_url for chat completions.
    /// Default: "/v1/chat/completions" for OpenAI provider.
    /// Override to "/chat/completions" for providers like Gemini that include
    /// version prefix in the base URL itself.
    pub api_path: Option<String>,

    /// Whether this provider supports extended thinking (Anthropic-style).
    /// Default: true for anthropic/bedrock/vertex, false for openai.
    pub supports_thinking: Option<bool>,

    /// Whether this provider supports reasoning_effort (OpenAI-style).
    /// Default: false for anthropic/bedrock/vertex, true for openai.
    pub supports_effort: Option<bool>,

    /// Available effort levels for this provider (e.g., ["low", "medium", "high"]).
    /// Only meaningful when supports_effort is true.
    pub effort_levels: Option<Vec<String>>,

    /// 该模型是否支持图片输入(多模态)。None = 默认支持(true)。
    /// 为 Some(false) 时 OpenAI provider 的 build_messages 会剔除图片、改文字占位。
    /// 由 VisionUnsupportedRegistry 在工厂构建时按 provider+model 注入,不持久化。
    pub supports_image: Option<bool>,

    /// Require a non-empty `reasoning_content` field on assistant history
    /// messages. Used only by gateways that explicitly enforce this extension.
    pub require_reasoning_content: Option<bool>,
}

impl ProviderCompat {
    /// Defaults for Anthropic-family providers (Anthropic, Vertex)
    pub fn anthropic_defaults() -> Self {
        Self {
            ensure_alternation: Some(true),
            merge_same_role: Some(true),
            auto_tool_id: Some(true),
            supports_thinking: Some(true),
            supports_effort: Some(false),
            ..Default::default()
        }
    }

    /// Defaults for Bedrock (Anthropic + schema sanitization)
    pub fn bedrock_defaults() -> Self {
        Self {
            ensure_alternation: Some(true),
            merge_same_role: Some(true),
            auto_tool_id: Some(true),
            sanitize_schema: Some(true),
            supports_thinking: Some(true),
            supports_effort: Some(false),
            ..Default::default()
        }
    }

    /// Defaults for OpenAI-compatible providers
    pub fn openai_defaults() -> Self {
        Self {
            max_tokens_field: Some("max_tokens".into()),
            merge_assistant_messages: Some(true),
            clean_orphan_tool_calls: Some(true),
            dedup_tool_results: Some(true),
            auto_tool_id: Some(true),
            supports_thinking: Some(false),
            supports_effort: Some(true),
            effort_levels: Some(vec!["low".into(), "medium".into(), "high".into()]),
            ..Default::default()
        }
    }

    /// Merge user config over defaults (user wins on non-None fields)
    pub fn merge(defaults: Self, user: Self) -> Self {
        Self {
            max_tokens_field: user.max_tokens_field.or(defaults.max_tokens_field),
            merge_assistant_messages: user
                .merge_assistant_messages
                .or(defaults.merge_assistant_messages),
            clean_orphan_tool_calls: user
                .clean_orphan_tool_calls
                .or(defaults.clean_orphan_tool_calls),
            dedup_tool_results: user.dedup_tool_results.or(defaults.dedup_tool_results),
            ensure_alternation: user.ensure_alternation.or(defaults.ensure_alternation),
            merge_same_role: user.merge_same_role.or(defaults.merge_same_role),
            sanitize_schema: user.sanitize_schema.or(defaults.sanitize_schema),
            strip_patterns: user.strip_patterns.or(defaults.strip_patterns),
            auto_tool_id: user.auto_tool_id.or(defaults.auto_tool_id),
            api_path: user.api_path.or(defaults.api_path),
            supports_thinking: user.supports_thinking.or(defaults.supports_thinking),
            supports_effort: user.supports_effort.or(defaults.supports_effort),
            effort_levels: user.effort_levels.or(defaults.effort_levels),
            supports_image: user.supports_image.or(defaults.supports_image),
            require_reasoning_content: user
                .require_reasoning_content
                .or(defaults.require_reasoning_content),
        }
    }

    // --- Resolved accessors (Option<bool> → bool; false default, except
    //     supports_image() which defaults true — see its doc comment) ---

    pub fn merge_assistant_messages(&self) -> bool {
        self.merge_assistant_messages.unwrap_or(false)
    }

    pub fn clean_orphan_tool_calls(&self) -> bool {
        self.clean_orphan_tool_calls.unwrap_or(false)
    }

    pub fn dedup_tool_results(&self) -> bool {
        self.dedup_tool_results.unwrap_or(false)
    }

    pub fn ensure_alternation(&self) -> bool {
        self.ensure_alternation.unwrap_or(false)
    }

    pub fn merge_same_role(&self) -> bool {
        self.merge_same_role.unwrap_or(false)
    }

    pub fn sanitize_schema(&self) -> bool {
        self.sanitize_schema.unwrap_or(false)
    }

    pub fn auto_tool_id(&self) -> bool {
        self.auto_tool_id.unwrap_or(false)
    }

    pub fn api_path(&self) -> &str {
        self.api_path.as_deref().unwrap_or("/v1/chat/completions")
    }

    pub fn supports_thinking(&self) -> bool {
        self.supports_thinking.unwrap_or(false)
    }

    pub fn supports_effort(&self) -> bool {
        self.supports_effort.unwrap_or(false)
    }

    pub fn effort_levels(&self) -> &[String] {
        self.effort_levels.as_deref().unwrap_or(&[])
    }

    /// 是否支持图片输入。**默认 true**——只有被显式标记不支持时才 false。
    pub fn supports_image(&self) -> bool {
        self.supports_image.unwrap_or(true)
    }

    pub fn require_reasoning_content(&self) -> bool {
        self.require_reasoning_content.unwrap_or(false)
    }
}

/// Sanitize a JSON Schema for strict providers (e.g., Bedrock).
/// - Root type must be "object" (wrap if not)
/// - Remove unsupported root "oneOf", "allOf", and "anyOf" keywords
/// - Recursively remove "additionalProperties"
/// - Normalize array types: ["string", "null"] → "string"
pub fn sanitize_json_schema(schema: &Value) -> Value {
    let mut schema = schema.clone();

    // Schemars commonly emits object enums as a root oneOf/anyOf/$ref without
    // repeating `type: object` at the root. Project those first; otherwise the
    // generic scalar wrapper would hide every real tool argument under `value`.
    let projected_object = flatten_root_composition(&mut schema);

    // Ensure genuinely non-object roots still satisfy provider requirements.
    if !projected_object && schema.get("type").and_then(|t| t.as_str()) != Some("object") {
        schema = serde_json::json!({
            "type": "object",
            "properties": {
                "value": schema
            },
            "required": ["value"]
        });
    }

    strip_additional_properties(&mut schema);
    normalize_array_types(&mut schema);
    schema
}

const MAX_SCHEMA_PROJECTION_WORK: usize = 4_096;

#[derive(Clone, Default)]
struct ObjectProjection {
    properties: Map<String, Value>,
    required: Vec<String>,
}

struct ProjectionWork {
    remaining: usize,
    exhausted: bool,
    active_refs: Vec<String>,
    ref_cache: BTreeMap<String, Option<ObjectProjection>>,
}

impl ProjectionWork {
    fn new() -> Self {
        Self {
            remaining: MAX_SCHEMA_PROJECTION_WORK,
            exhausted: false,
            active_refs: Vec::new(),
            ref_cache: BTreeMap::new(),
        }
    }

    fn visit(&mut self) -> bool {
        if self.remaining == 0 {
            self.exhausted = true;
            return false;
        }
        self.remaining -= 1;
        true
    }

    fn enter_ref(&mut self, reference: &str) -> bool {
        if self
            .active_refs
            .iter()
            .any(|active| active == reference)
        {
            return false;
        }
        self.active_refs.push(reference.to_owned());
        true
    }

    fn leave_ref(&mut self) {
        self.active_refs.pop();
    }
}

/// Project root composition branches into a flat provider-facing object.
///
/// Some providers reject `oneOf`/`anyOf`/`allOf` at the tool-schema root.
/// Deleting those keywords without first projecting their fields turns common
/// enum-shaped tool inputs into an empty object and leaves the model guessing.
/// The original, composed schema remains the execution-time authority.
fn flatten_root_composition(schema: &mut Value) -> bool {
    let source = schema.clone();
    let mut work = ProjectionWork::new();
    let Some(mut projection) = collect_object_projection(&source, &source, &mut work, 0) else {
        return false;
    };
    let Some(root) = schema.as_object_mut() else {
        return false;
    };

    root.insert("type".to_owned(), Value::String("object".to_owned()));
    if !projection.properties.is_empty() {
        root.insert(
            "properties".to_owned(),
            Value::Object(std::mem::take(&mut projection.properties)),
        );
    }
    if !projection.required.is_empty() {
        root.insert(
            "required".to_owned(),
            Value::Array(
                projection
                    .required
                    .into_iter()
                    .map(Value::String)
                    .collect(),
            ),
        );
    }
    for keyword in ["oneOf", "allOf", "anyOf"] {
        root.remove(keyword);
    }
    root.remove("$ref");
    true
}

fn collect_object_projection(
    root: &Value,
    schema: &Value,
    work: &mut ProjectionWork,
    depth: usize,
) -> Option<ObjectProjection> {
    if depth > 32 || !work.visit() {
        return None;
    }
    let mut projection = ObjectProjection::default();
    let mut object_like = false;
    if let Some(reference) = schema.get("$ref").and_then(Value::as_str) {
        let referenced = if let Some(cached) = work.ref_cache.get(reference) {
            cached.clone()
        } else if let Some(resolved) = resolve_local_schema_ref(root, schema) {
            if work.enter_ref(reference) {
                let resolved_projection =
                    collect_object_projection(root, resolved, work, depth + 1);
                work.leave_ref();
                work.ref_cache
                    .insert(reference.to_owned(), resolved_projection.clone());
                resolved_projection
            } else {
                None
            }
        } else {
            None
        };
        if let Some(referenced) = referenced {
            object_like = true;
            merge_projection_properties(&mut projection.properties, referenced.properties);
            extend_unique(
                &mut projection.required,
                referenced.required.iter().map(String::as_str),
            );
        }
    }

    object_like |= schema.get("type").and_then(Value::as_str) == Some("object")
        || schema.get("properties").is_some()
        || schema.get("required").is_some();
    if let Some(properties) = schema.get("properties").and_then(Value::as_object) {
        for (name, property) in properties {
            merge_projected_property(&mut projection.properties, name, property.clone());
        }
    }
    if let Some(required) = schema.get("required").and_then(Value::as_array) {
        extend_unique(
            &mut projection.required,
            required.iter().filter_map(Value::as_str),
        );
    }

    if let Some(branches) = schema.get("allOf").and_then(Value::as_array) {
        for branch in branches {
            if work.exhausted {
                break;
            }
            if let Some(branch_projection) = collect_object_projection(root, branch, work, depth + 1)
            {
                object_like = true;
                merge_projection_properties(
                    &mut projection.properties,
                    branch_projection.properties,
                );
                extend_unique(
                    &mut projection.required,
                    branch_projection.required.iter().map(String::as_str),
                );
            }
        }
    }

    for keyword in ["oneOf", "anyOf"] {
        let Some(branches) = schema.get(keyword).and_then(Value::as_array) else {
            continue;
        };
        let mut branch_projections = Vec::with_capacity(branches.len());
        let mut all_object_branches = !branches.is_empty();
        for branch in branches {
            if work.exhausted {
                all_object_branches = false;
                break;
            }
            let Some(branch_projection) =
                collect_object_projection(root, branch, work, depth + 1)
            else {
                all_object_branches = false;
                continue;
            };
            branch_projections.push(branch_projection);
        }
        if !all_object_branches {
            continue;
        }
        object_like = true;

        for branch in &branch_projections {
            for (name, property) in &branch.properties {
                merge_projected_property(
                    &mut projection.properties,
                    name,
                    property.clone(),
                );
            }
        }

        // Requiring a field that exists in only one union arm would make the
        // flattened schema impossible for the other arms. Lift only the
        // intersection; the original validator still enforces each branch.
        let mut common_required = branch_projections[0].required.clone();
        common_required.retain(|name| {
            branch_projections[1..]
                .iter()
                .all(|branch| branch.required.contains(name))
        });
        extend_unique(
            &mut projection.required,
            common_required.iter().map(String::as_str),
        );
    }

    object_like.then_some(projection)
}

fn merge_projection_properties(
    target: &mut Map<String, Value>,
    incoming: Map<String, Value>,
) {
    for (name, property) in incoming {
        merge_projected_property(target, &name, property);
    }
}

fn merge_projected_property(
    properties: &mut Map<String, Value>,
    name: &str,
    incoming: Value,
) {
    let Some(existing) = properties.get(name) else {
        properties.insert(name.to_owned(), incoming);
        return;
    };
    if existing == &incoming {
        return;
    }

    if let (Some(mut literals), Some(incoming_literals)) = (
        string_literal_values(existing),
        string_literal_values(&incoming),
    ) {
        extend_unique(&mut literals, incoming_literals.iter().map(String::as_str));
        let mut merged = Map::new();
        merged.insert("type".to_owned(), Value::String("string".to_owned()));
        merged.insert(
            "enum".to_owned(),
            Value::Array(literals.into_iter().map(Value::String).collect()),
        );
        if let Some(description) = existing
            .get("description")
            .or_else(|| incoming.get("description"))
            .cloned()
        {
            merged.insert("description".to_owned(), description);
        }
        properties.insert(name.to_owned(), Value::Object(merged));
        return;
    }

    // Incompatible definitions of a shared union property remain a nested
    // union. Strict providers reject composition only at the tool-schema root,
    // and retaining both alternatives is more accurate than guessing one.
    let mut alternatives = Vec::new();
    append_unique_alternatives(&mut alternatives, existing);
    append_unique_alternatives(&mut alternatives, &incoming);
    properties.insert(
        name.to_owned(),
        serde_json::json!({ "anyOf": alternatives }),
    );
}

fn append_unique_alternatives(alternatives: &mut Vec<Value>, schema: &Value) {
    let candidates = schema
        .get("anyOf")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or_else(|| std::slice::from_ref(schema));
    for candidate in candidates {
        if !alternatives.contains(candidate) {
            alternatives.push(candidate.clone());
        }
    }
}

fn string_literal_values(schema: &Value) -> Option<Vec<String>> {
    if let Some(literal) = schema.get("const").and_then(Value::as_str) {
        return Some(vec![literal.to_owned()]);
    }
    let values = schema.get("enum")?.as_array()?;
    if values.is_empty() || values.iter().any(|value| !value.is_string()) {
        return None;
    }
    Some(
        values
            .iter()
            .filter_map(Value::as_str)
            .map(str::to_owned)
            .collect(),
    )
}

fn extend_unique<'a>(target: &mut Vec<String>, values: impl IntoIterator<Item = &'a str>) {
    for value in values {
        if !target.iter().any(|existing| existing == value) {
            target.push(value.to_owned());
        }
    }
}

fn resolve_local_schema_ref<'a>(root: &'a Value, schema: &Value) -> Option<&'a Value> {
    let reference = schema.get("$ref")?.as_str()?;
    root.pointer(reference.strip_prefix('#')?)
}

fn strip_additional_properties(val: &mut Value) {
    if let Some(obj) = val.as_object_mut() {
        obj.remove("additionalProperties");
        for v in obj.values_mut() {
            strip_additional_properties(v);
        }
    } else if let Some(arr) = val.as_array_mut() {
        for v in arr.iter_mut() {
            strip_additional_properties(v);
        }
    }
}

fn normalize_array_types(val: &mut Value) {
    if let Some(obj) = val.as_object_mut() {
        // Normalize ["string", "null"] → "string"
        if let Some(arr) = obj.get("type").and_then(Value::as_array) {
            let non_null: Vec<&Value> = arr.iter().filter(|v| v.as_str() != Some("null")).collect();
            if non_null.len() == 1 {
                obj.insert("type".to_string(), non_null[0].clone());
            }
        }
        for v in obj.values_mut() {
            normalize_array_types(v);
        }
    } else if let Some(arr) = val.as_array_mut() {
        for v in arr.iter_mut() {
            normalize_array_types(v);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_anthropic_defaults() {
        let compat = ProviderCompat::anthropic_defaults();
        assert!(compat.ensure_alternation());
        assert!(compat.merge_same_role());
        assert!(compat.auto_tool_id());
        assert!(!compat.sanitize_schema());
        assert!(!compat.merge_assistant_messages());
        assert!(!compat.clean_orphan_tool_calls());
    }

    #[test]
    fn test_bedrock_defaults() {
        let compat = ProviderCompat::bedrock_defaults();
        assert!(compat.ensure_alternation());
        assert!(compat.merge_same_role());
        assert!(compat.auto_tool_id());
        assert!(compat.sanitize_schema());
    }

    #[test]
    fn test_openai_defaults() {
        let compat = ProviderCompat::openai_defaults();
        assert!(compat.merge_assistant_messages());
        assert!(compat.clean_orphan_tool_calls());
        assert!(compat.dedup_tool_results());
        assert_eq!(compat.max_tokens_field.as_deref(), Some("max_tokens"));
        assert!(!compat.ensure_alternation());
    }

    #[test]
    fn test_merge_user_overrides_defaults() {
        let defaults = ProviderCompat::openai_defaults();
        let user = ProviderCompat {
            max_tokens_field: Some("max_completion_tokens".into()),
            merge_assistant_messages: Some(false),
            ..Default::default()
        };

        let merged = ProviderCompat::merge(defaults, user);
        assert_eq!(
            merged.max_tokens_field.as_deref(),
            Some("max_completion_tokens")
        );
        assert!(!merged.merge_assistant_messages());
        // Non-overridden fields keep defaults
        assert!(merged.clean_orphan_tool_calls());
        assert!(merged.dedup_tool_results());
    }

    #[test]
    fn test_merge_empty_user_keeps_defaults() {
        let defaults = ProviderCompat::anthropic_defaults();
        let user = ProviderCompat::default();

        let merged = ProviderCompat::merge(defaults, user);
        assert!(merged.ensure_alternation());
        assert!(merged.merge_same_role());
        assert!(merged.auto_tool_id());
    }

    #[test]
    fn test_sanitize_schema_wraps_non_object_root() {
        let schema = json!({"type": "string"});
        let sanitized = sanitize_json_schema(&schema);

        assert_eq!(sanitized["type"], "object");
        assert_eq!(sanitized["properties"]["value"]["type"], "string");
    }

    #[test]
    fn test_sanitize_schema_removes_additional_properties() {
        let schema = json!({
            "type": "object",
            "properties": {
                "name": {"type": "string", "additionalProperties": false}
            },
            "additionalProperties": false
        });
        let sanitized = sanitize_json_schema(&schema);

        assert!(sanitized.get("additionalProperties").is_none());
        assert!(
            sanitized["properties"]["name"]
                .get("additionalProperties")
                .is_none()
        );
    }

    #[test]
    fn test_sanitize_schema_normalizes_array_types() {
        let schema = json!({
            "type": "object",
            "properties": {
                "name": {"type": ["string", "null"]}
            }
        });
        let sanitized = sanitize_json_schema(&schema);

        assert_eq!(sanitized["properties"]["name"]["type"], "string");
    }

    #[test]
    fn test_sanitize_schema_no_change_for_valid_object() {
        let schema = json!({
            "type": "object",
            "properties": {
                "cmd": {"type": "string"}
            },
            "required": ["cmd"]
        });
        let sanitized = sanitize_json_schema(&schema);

        assert_eq!(sanitized["type"], "object");
        assert_eq!(sanitized["properties"]["cmd"]["type"], "string");
    }

    #[test]
    fn test_sanitize_schema_removes_only_root_composition_keywords() {
        let schema = json!({
            "type": "object",
            "properties": {
                "mode": { "anyOf": [{ "type": "string" }, { "type": "integer" }] }
            },
            "required": ["mode"],
            "oneOf": [{ "required": ["mode"] }],
            "allOf": [{ "type": "object" }],
            "anyOf": [{ "type": "object" }]
        });
        let sanitized = sanitize_json_schema(&schema);
        assert!(sanitized.get("oneOf").is_none());
        assert!(sanitized.get("allOf").is_none());
        assert!(sanitized.get("anyOf").is_none());
        assert_eq!(sanitized["required"], json!(["mode"]));
        assert!(sanitized["properties"]["mode"].get("anyOf").is_some());
    }

    #[test]
    fn test_sanitize_schema_projects_delegate_union_fields_and_types() {
        let schema = json!({
            "$defs": {
                "Planned": {
                    "type": "object",
                    "properties": {
                        "strategy": {"type": "string", "const": "planned"},
                        "goal": {"type": "string"},
                        "model_pool": {
                            "anyOf": [
                                {"$ref": "#/$defs/ModelPool"},
                                {"type": "null"}
                            ]
                        }
                    },
                    "required": ["strategy", "goal"],
                    "additionalProperties": false
                },
                "Parallel": {
                    "type": "object",
                    "properties": {
                        "strategy": {"type": "string", "const": "parallel"},
                        "tasks": {
                            "type": "array",
                            "items": {"type": "object"}
                        },
                        "synthesize": {"type": "boolean"}
                    },
                    "required": ["strategy", "tasks"],
                    "additionalProperties": false
                },
                "ModelPool": {
                    "type": "object",
                    "properties": {"mode": {"type": "string"}}
                }
            },
            "type": "object",
            "properties": {},
            "oneOf": [
                {"$ref": "#/$defs/Planned"},
                {"$ref": "#/$defs/Parallel"}
            ]
        });

        let sanitized = sanitize_json_schema(&schema);

        assert!(sanitized.get("oneOf").is_none());
        assert_eq!(
            sanitized["properties"]["strategy"]["enum"],
            json!(["planned", "parallel"])
        );
        assert_eq!(sanitized["properties"]["strategy"]["type"], "string");
        assert_eq!(sanitized["properties"]["goal"]["type"], "string");
        assert_eq!(sanitized["properties"]["tasks"]["type"], "array");
        assert_eq!(sanitized["properties"]["synthesize"]["type"], "boolean");
        assert!(sanitized["properties"]["model_pool"]["anyOf"].is_array());
        assert_eq!(sanitized["required"], json!(["strategy"]));
    }

    #[test]
    fn test_sanitize_schema_projects_root_union_without_explicit_object_type() {
        let schema = json!({
            "$defs": {
                "ById": {
                    "type": "object",
                    "properties": {
                        "mode": {"const": "id"},
                        "id": {"type": "string"}
                    },
                    "required": ["mode", "id"]
                },
                "ByQuery": {
                    "type": "object",
                    "properties": {
                        "mode": {"const": "query"},
                        "query": {"type": "string"}
                    },
                    "required": ["mode", "query"]
                }
            },
            "oneOf": [
                {"$ref": "#/$defs/ById"},
                {"$ref": "#/$defs/ByQuery"}
            ]
        });

        let sanitized = sanitize_json_schema(&schema);

        assert_eq!(sanitized["type"], "object");
        assert!(sanitized.get("oneOf").is_none());
        assert!(sanitized["properties"].get("value").is_none());
        assert_eq!(sanitized["properties"]["id"]["type"], "string");
        assert_eq!(sanitized["properties"]["query"]["type"], "string");
        assert_eq!(sanitized["required"], json!(["mode"]));
    }

    #[test]
    fn test_sanitize_schema_projects_root_ref_without_explicit_object_type() {
        let schema = json!({
            "$defs": {
                "Request": {
                    "type": "object",
                    "properties": {"query": {"type": "string"}},
                    "required": ["query"]
                }
            },
            "$ref": "#/$defs/Request",
            "properties": {"request_id": {"type": "string"}},
            "required": ["request_id"]
        });

        let sanitized = sanitize_json_schema(&schema);

        assert_eq!(sanitized["type"], "object");
        assert!(sanitized.get("$ref").is_none());
        assert!(sanitized["properties"].get("value").is_none());
        assert_eq!(sanitized["properties"]["query"]["type"], "string");
        assert_eq!(sanitized["properties"]["request_id"]["type"], "string");
        assert_eq!(sanitized["required"], json!(["query", "request_id"]));
    }

    #[test]
    fn test_sanitize_schema_memoizes_repeated_ref_projection() {
        let repeated = (0..128)
            .map(|_| json!({"$ref": "#/$defs/Request"}))
            .collect::<Vec<_>>();
        let schema = json!({
            "$defs": {
                "Request": {
                    "type": "object",
                    "properties": {"query": {"type": "string"}},
                    "required": ["query"]
                }
            },
            "anyOf": repeated
        });

        let sanitized = sanitize_json_schema(&schema);

        assert_eq!(sanitized["type"], "object");
        assert_eq!(sanitized["properties"]["query"]["type"], "string");
        assert_eq!(sanitized["required"], json!(["query"]));
        assert!(sanitized.get("anyOf").is_none());
    }

    #[test]
    fn test_sanitize_schema_unions_all_of_requirements() {
        let schema = json!({
            "type": "object",
            "properties": {"base": {"type": "string"}},
            "required": ["base"],
            "allOf": [
                {
                    "type": "object",
                    "properties": {"count": {"type": "integer"}},
                    "required": ["count"]
                }
            ]
        });

        let sanitized = sanitize_json_schema(&schema);
        assert_eq!(sanitized["properties"]["count"]["type"], "integer");
        assert_eq!(sanitized["required"], json!(["base", "count"]));
    }

    #[test]
    fn test_anthropic_defaults_capability_fields() {
        let compat = ProviderCompat::anthropic_defaults();
        assert_eq!(compat.supports_thinking, Some(true));
        assert_eq!(compat.supports_effort, Some(false));
        assert!(compat.effort_levels.is_none());
    }

    #[test]
    fn test_openai_defaults_capability_fields() {
        let compat = ProviderCompat::openai_defaults();
        assert_eq!(compat.supports_thinking, Some(false));
        assert_eq!(compat.supports_effort, Some(true));
        assert_eq!(
            compat.effort_levels,
            Some(vec![
                "low".to_string(),
                "medium".to_string(),
                "high".to_string()
            ])
        );
    }

    #[test]
    fn test_bedrock_defaults_capability_fields() {
        let compat = ProviderCompat::bedrock_defaults();
        assert_eq!(compat.supports_thinking, Some(true));
        assert_eq!(compat.supports_effort, Some(false));
    }

    #[test]
    fn test_merge_capability_fields_user_overrides() {
        let defaults = ProviderCompat::openai_defaults();
        let user = ProviderCompat {
            supports_thinking: Some(true),
            ..Default::default()
        };
        let merged = ProviderCompat::merge(defaults, user);
        assert_eq!(merged.supports_thinking, Some(true));
        assert_eq!(merged.supports_effort, Some(true));
    }

    #[test]
    fn test_capability_accessors() {
        let compat = ProviderCompat::anthropic_defaults();
        assert!(compat.supports_thinking());
        assert!(!compat.supports_effort());
        assert!(compat.effort_levels().is_empty());

        let compat2 = ProviderCompat::openai_defaults();
        assert!(!compat2.supports_thinking());
        assert!(compat2.supports_effort());
        assert_eq!(compat2.effort_levels(), &["low", "medium", "high"]);
    }

    #[test]
    fn supports_image_defaults_true_when_unset() {
        let compat = ProviderCompat::default();
        assert!(compat.supports_image());
    }

    #[test]
    fn supports_image_false_when_set_false() {
        let compat = ProviderCompat {
            supports_image: Some(false),
            ..Default::default()
        };
        assert!(!compat.supports_image());
    }

    #[test]
    fn merge_user_supports_image_wins() {
        let defaults = ProviderCompat::default();
        let user = ProviderCompat {
            supports_image: Some(false),
            ..Default::default()
        };
        let merged = ProviderCompat::merge(defaults, user);
        assert!(!merged.supports_image());
    }

    #[test]
    fn test_deserialize_from_toml() {
        let toml_str = r#"
max_tokens_field = "max_completion_tokens"
merge_assistant_messages = true
strip_patterns = ["__REASONING__"]
"#;
        let compat: ProviderCompat = toml::from_str(toml_str).unwrap();
        assert_eq!(
            compat.max_tokens_field.as_deref(),
            Some("max_completion_tokens")
        );
        assert_eq!(compat.merge_assistant_messages, Some(true));
        assert_eq!(
            compat.strip_patterns,
            Some(vec!["__REASONING__".to_string()])
        );
        assert!(compat.clean_orphan_tool_calls.is_none());
    }
}

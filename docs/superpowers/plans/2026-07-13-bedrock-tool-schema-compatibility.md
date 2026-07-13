# Bedrock Tool Schema Compatibility Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Recover automatically when a user-entered OpenAI- or Anthropic-compatible gateway routes tool calls to Bedrock, while making provider health probes tool-free.

**Architecture:** Extend the strict-provider schema sanitizer and add a precise `ProviderError` classifier. OpenAI and Anthropic providers retain full schemas until an explicit tool-schema rejection teaches the provider instance to rebuild and resend with sanitized schemas; direct Bedrock sanitizes proactively. Health probes clear their registry after bootstrap.

**Tech Stack:** Rust 2024, Tokio, Reqwest, Serde JSON, Wiremock, Cargo workspace tests.

## Global Constraints

- Do not infer Bedrock from provider names, URLs, or model aliases.
- Preserve full schemas unless direct Bedrock is known or the endpoint explicitly reports `TOOL_SCHEMA_INVALID` or the equivalent top-level `input_schema` restriction.
- Remove only root-level `oneOf`, `allOf`, and `anyOf`; preserve nested composition and argument shape.
- Retry a schema-incompatible initial request once; unrelated HTTP 500 responses are never schema-retried.
- Health probes send zero tool definitions.
- Add no dependencies and log no credentials or full request bodies.

## File Structure

- `crates/agent/nomi-config/src/compat.rs`: strict-provider schema normalization.
- `crates/agent/nomi-providers/src/lib.rs`: exact schema-error classification.
- `crates/agent/nomi-providers/src/bedrock.rs`: direct Bedrock request regression.
- `crates/agent/nomi-providers/src/openai.rs`: OpenAI-compatible adaptive fallback.
- `crates/agent/nomi-providers/src/anthropic.rs`: Anthropic-compatible adaptive fallback.
- `crates/agent/nomi-providers/tests/provider_openai_test.rs`: OpenAI wire tests.
- `crates/agent/nomi-providers/tests/provider_anthropic_test.rs`: Anthropic wire tests.
- `crates/agent/nomi-tools/src/registry.rs`: explicit deny-all registry operation.
- `crates/backend/nomifun-ai-agent/src/services/provider_health.rs`: tool-free probe engine.

---

### Task 1: Complete strict schema sanitization

**Files:**
- Modify: `crates/agent/nomi-config/src/compat.rs:200-380`
- Modify: `crates/agent/nomi-providers/src/bedrock.rs:57-100`

**Interfaces:**
- Consumes and preserves `sanitize_json_schema(&Value) -> Value`.
- Produces an object root without root `oneOf`, `allOf`, or `anyOf`.

- [ ] **Step 1: Add the failing sanitizer test**

```rust
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
```

- [ ] **Step 2: Verify RED**

Run `cargo test -p nomi-config test_sanitize_schema_removes_only_root_composition_keywords -- --nocapture`.
Expected: FAIL because all three root composition keys remain.

- [ ] **Step 3: Implement root-only removal**

Insert after object-root normalization and update the function documentation to name the three unsupported root keys:

```rust
if let Some(root) = schema.as_object_mut() {
    for keyword in ["oneOf", "allOf", "anyOf"] {
        root.remove(keyword);
    }
}
```

- [ ] **Step 4: Add a direct Bedrock body test**

Append this test module to `bedrock.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use nomi_types::message::{ContentBlock, Message, Role};
    use nomi_types::tool::ToolDef;

    #[test]
    fn bedrock_request_removes_top_level_tool_schema_composition() {
        let provider = BedrockProvider::new(
            "us-east-1",
            AwsCredentials::Environment,
            false,
            ProviderCompat::bedrock_defaults(),
        );
        let request = LlmRequest {
            model: "anthropic.claude-test".into(),
            system: "test".into(),
            messages: vec![Message::new(
                Role::User,
                vec![ContentBlock::Text { text: "hi".into() }],
            )],
            tools: vec![ToolDef {
                name: "Read".into(),
                description: "Read one or more files".into(),
                input_schema: json!({
                    "type": "object",
                    "properties": {
                        "file_path": { "type": "string" },
                        "file_paths": {
                            "type": "array",
                            "items": { "type": "string" }
                        }
                    },
                    "oneOf": [
                        { "required": ["file_path"] },
                        { "required": ["file_paths"] }
                    ]
                }),
                deferred: false,
            }],
            max_tokens: 16,
            thinking: None,
            reasoning_effort: None,
        };

        let body = provider.build_request_body(&request);
        let schema = &body["tools"][0]["input_schema"];
        assert_eq!(schema["type"], "object");
        assert!(schema["properties"].get("file_path").is_some());
        assert!(schema["properties"].get("file_paths").is_some());
        assert!(schema.get("oneOf").is_none());
    }
}
```

- [ ] **Step 5: Verify GREEN**

Run:

```powershell
cargo test -p nomi-config test_sanitize_schema -- --nocapture
cargo test -p nomi-providers bedrock_request_removes_top_level_tool_schema_composition -- --nocapture
```

Expected: both commands PASS.

- [ ] **Step 6: Commit**

```powershell
git add crates/agent/nomi-config/src/compat.rs crates/agent/nomi-providers/src/bedrock.rs
git commit -m "fix(provider): sanitize Bedrock tool schema roots"
```

---

### Task 2: Classify exact schema incompatibility errors

**Files:**
- Modify: `crates/agent/nomi-providers/src/lib.rs:20-80`

**Interfaces:**
- Produces `pub(crate) fn is_tool_schema_incompatible(&self) -> bool` on `ProviderError`.

- [ ] **Step 1: Add failing positive and negative tests**

```rust
#[test]
fn tool_schema_classifier_accepts_bedrock_gateway_signals() {
    let reason = ProviderError::Api {
        status: 500,
        message: r#"{"reason":"TOOL_SCHEMA_INVALID"}"#.into(),
    };
    let wording = ProviderError::Api {
        status: 400,
        message: "input_schema does not support oneOf, allOf, or anyOf at the top level".into(),
    };
    assert!(reason.is_tool_schema_incompatible());
    assert!(wording.is_tool_schema_incompatible());
}

#[test]
fn tool_schema_classifier_rejects_unrelated_failures() {
    let errors = [
        ProviderError::Api { status: 500, message: "upstream unavailable".into() },
        ProviderError::Api { status: 400, message: "input_schema is malformed".into() },
        ProviderError::Connection("input_schema connection reset".into()),
    ];
    assert!(errors.iter().all(|error| !error.is_tool_schema_incompatible()));
}
```

- [ ] **Step 2: Verify RED**

Run `cargo test -p nomi-providers tool_schema_classifier -- --nocapture`.
Expected: compilation fails because the method is absent.

- [ ] **Step 3: Implement the classifier**

```rust
pub(crate) fn is_tool_schema_incompatible(&self) -> bool {
    let ProviderError::Api { message, .. } = self else {
        return false;
    };
    let lower = message.to_ascii_lowercase();
    lower.contains("tool_schema_invalid")
        || (lower.contains("input_schema")
            && lower.contains("top level")
            && ["oneof", "allof", "anyof"]
                .iter()
                .any(|keyword| lower.contains(keyword)))
}
```

- [ ] **Step 4: Verify GREEN and commit**

Run `cargo test -p nomi-providers tool_schema_classifier -- --nocapture`; expect 2 PASS.

```powershell
git add crates/agent/nomi-providers/src/lib.rs
git commit -m "fix(provider): classify Bedrock tool schema errors"
```

---

### Task 3: Adapt OpenAI-compatible gateways

**Files:**
- Modify: `crates/agent/nomi-providers/src/openai.rs:1-380,659-752`
- Modify: `crates/agent/nomi-providers/tests/provider_openai_test.rs`

**Interfaces:**
- Consumes the Task 1 sanitizer and Task 2 classifier.
- Adds private `AtomicBool` state; `OpenAIProvider::new` remains unchanged.

- [ ] **Step 1: Add a failing conditional-gateway test**

Extend the imports and helpers, then add both tests:

```rust
use nomi_providers::{LlmProvider, ProviderError};
use nomi_types::tool::ToolDef;
use wiremock::{Request, Respond};

#[derive(Clone)]
struct OpenAiBedrockSchemaResponder;

impl Respond for OpenAiBedrockSchemaResponder {
    fn respond(&self, request: &Request) -> ResponseTemplate {
        let body: serde_json::Value = serde_json::from_slice(&request.body).unwrap();
        let schema = &body["tools"][0]["function"]["parameters"];
        if schema.get("oneOf").is_some() {
            return ResponseTemplate::new(500).set_body_json(json!({
                "error": {
                    "message": "input_schema does not support oneOf, allOf, or anyOf at the top level",
                    "reason": "TOOL_SCHEMA_INVALID"
                }
            }));
        }
        let chunk = json!({
            "choices": [{ "delta": { "content": "Recovered" }, "finish_reason": null }]
        })
        .to_string();
        let finish = json!({
            "choices": [{ "delta": {}, "finish_reason": "stop" }],
            "usage": { "prompt_tokens": 1, "completion_tokens": 1 }
        })
        .to_string();
        ResponseTemplate::new(200)
            .set_body_raw(build_sse_body(&[&chunk, &finish]), "text/event-stream")
    }
}

fn request_with_composed_tool_schema() -> LlmRequest {
    let mut request = make_request();
    request.tools.push(ToolDef {
        name: "Read".into(),
        description: "Read one or more files".into(),
        input_schema: json!({
            "type": "object",
            "properties": {
                "file_path": { "type": "string" },
                "file_paths": { "type": "array", "items": { "type": "string" } }
            },
            "oneOf": [
                { "required": ["file_path"] },
                { "required": ["file_paths"] }
            ]
        }),
        deferred: false,
    });
    request
}

#[tokio::test]
async fn openai_gateway_recovers_and_remembers_bedrock_schema_requirement() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(OpenAiBedrockSchemaResponder)
        .expect(3)
        .mount(&server)
        .await;
    let provider = OpenAIProvider::new(
        "test-key",
        &server.uri(),
        ProviderCompat::openai_defaults(),
    );
    let request = request_with_composed_tool_schema();
    for _ in 0..2 {
        let events = collect_events(provider.stream(&request).await.unwrap()).await;
        assert!(events.iter().any(
            |event| matches!(event, LlmEvent::TextDelta(text) if text == "Recovered")
        ));
    }
    let received = server.received_requests().await.unwrap();
    let has_root_one_of: Vec<bool> = received
        .iter()
        .map(|request| {
            let body: serde_json::Value = serde_json::from_slice(&request.body).unwrap();
            body["tools"][0]["function"]["parameters"]
                .get("oneOf")
                .is_some()
        })
        .collect();
    assert_eq!(has_root_one_of, vec![true, false, false]);
    server.verify().await;
}

#[tokio::test]
async fn openai_gateway_does_not_schema_retry_an_unrelated_500() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(500).set_body_string("upstream unavailable"))
        .expect(1)
        .mount(&server)
        .await;
    let provider = OpenAIProvider::new(
        "test-key",
        &server.uri(),
        ProviderCompat::openai_defaults(),
    );
    let error = provider
        .stream(&request_with_composed_tool_schema())
        .await
        .unwrap_err();
    assert!(matches!(error, ProviderError::Api { status: 500, .. }));
    server.verify().await;
}
```

- [ ] **Step 2: Verify RED**

Run `cargo test -p nomi-providers --test provider_openai_test openai_gateway_ -- --nocapture`.
Expected: recovery fails on the first 500; unrelated 500 is called once.

- [ ] **Step 3: Add learned state and schema-aware tool building**

```rust
use std::sync::atomic::{AtomicBool, Ordering};
use nomi_config::compat::{self, ProviderCompat};

pub struct OpenAIProvider {
    api_key: String,
    base_url: String,
    compat: ProviderCompat,
    sanitize_tool_schemas: AtomicBool,
}

pub fn new(api_key: &str, base_url: &str, compat: ProviderCompat) -> Self {
    Self {
        api_key: api_key.to_string(),
        base_url: base_url.to_string(),
        compat,
        sanitize_tool_schemas: AtomicBool::new(false),
    }
}

fn should_sanitize_tool_schemas(&self) -> bool {
    self.compat.sanitize_schema()
        || self.sanitize_tool_schemas.load(Ordering::Acquire)
}
```

Replace `build_tools` with this schema-aware version:

```rust
fn build_tools(tools: &[ToolDef], sanitize: bool) -> Vec<Value> {
    tools
        .iter()
        .map(|tool| {
            if tool.deferred {
                let short_desc = truncate_deferred_description(&tool.description);
                return json!({
                    "type": "function",
                    "function": {
                        "name": tool.name,
                        "description": format!(
                            "(Deferred) {short_desc} — Use ToolSearch to load full schema before calling."
                        ),
                        "parameters": { "type": "object", "properties": {} }
                    }
                });
            }
            let parameters = if sanitize {
                compat::sanitize_json_schema(&tool.input_schema)
            } else {
                tool.input_schema.clone()
            };
            json!({
                "type": "function",
                "function": {
                    "name": tool.name,
                    "description": tool.description,
                    "parameters": parameters
                }
            })
        })
        .collect()
}
```

Pass `self.should_sanitize_tool_schemas()` from `build_request_body`:

```rust
if !request.tools.is_empty() {
    body["tools"] = json!(Self::build_tools(
        &request.tools,
        self.should_sanitize_tool_schemas(),
    ));
}
```

- [ ] **Step 4: Implement one classified resend**

Add this private method to preserve the current connection retry and rate-limit mapping:

```rust
async fn send_initial(
    client: &reqwest::Client,
    url: &str,
    headers: &HeaderMap,
    body: &Value,
) -> Result<reqwest::Response, ProviderError> {
    crate::retry::with_initial_connect_retry(|| async {
        let response = client
            .post(url)
            .headers(headers.clone())
            .json(body)
            .send()
            .await?;
        let status = response.status();
        if status.is_success() {
            return Ok(response);
        }
        let retry_after_ms =
            crate::parse_retry_after_ms(response.headers()).unwrap_or(5000);
        let body_text = response.text().await.unwrap_or_default();
        if status.as_u16() == 429 {
            return Err(ProviderError::RateLimited {
                retry_after_ms,
                message: crate::non_empty_rate_limit_message(body_text),
            });
        }
        Err(ProviderError::Api {
            status: status.as_u16(),
            message: body_text,
        })
    })
    .await
}
```

Replace the current initial response block with:

```rust
let used_sanitized_schema = self.should_sanitize_tool_schemas();
let mut body = self.build_request_body(request);
let response = match Self::send_initial(&client, &url, &headers, &body).await {
    Ok(response) => response,
    Err(error)
        if !request.tools.is_empty()
            && !used_sanitized_schema
            && error.is_tool_schema_incompatible() =>
    {
        let ProviderError::Api { status, .. } = &error else {
            unreachable!("schema classifier only accepts API errors");
        };
        tracing::warn!(
            target: "nomi_providers",
            provider = "openai",
            status,
            "provider rejected tool schemas; retrying with Bedrock-compatible schema roots"
        );
        self.sanitize_tool_schemas.store(true, Ordering::Release);
        body = self.build_request_body(request);
        Self::send_initial(&client, &url, &headers, &body).await?
    }
    Err(error) => return Err(error),
};
```

Return other errors unchanged. Let the existing spawned empty-stream retry capture the final `body`.

- [ ] **Step 5: Verify GREEN and commit**

Run `cargo test -p nomi-providers --test provider_openai_test -- --nocapture`; expect zero failures.

```powershell
git add crates/agent/nomi-providers/src/openai.rs crates/agent/nomi-providers/tests/provider_openai_test.rs
git commit -m "fix(provider): adapt OpenAI gateways to Bedrock schemas"
```

---

### Task 4: Adapt Anthropic-compatible gateways

**Files:**
- Modify: `crates/agent/nomi-providers/src/anthropic.rs:1-170`
- Modify: `crates/agent/nomi-providers/tests/provider_anthropic_test.rs`

**Interfaces:**
- Consumes `ProviderError::is_tool_schema_incompatible` and `sanitize_json_schema`.
- Adds private `AtomicBool` state; public constructor and `with_cache` stay unchanged.

- [ ] **Step 1: Add a failing Anthropic conditional-gateway test**

Extend the imports and helpers, then add both tests:

```rust
use nomi_types::tool::ToolDef;
use wiremock::{Request, Respond};

#[derive(Clone)]
struct AnthropicBedrockSchemaResponder;

impl Respond for AnthropicBedrockSchemaResponder {
    fn respond(&self, request: &Request) -> ResponseTemplate {
        let body: serde_json::Value = serde_json::from_slice(&request.body).unwrap();
        let schema = &body["tools"][0]["input_schema"];
        if schema.get("oneOf").is_some() {
            ResponseTemplate::new(500).set_body_json(serde_json::json!({
                "error": {
                    "message": "input_schema does not support oneOf, allOf, or anyOf at the top level",
                    "reason": "TOOL_SCHEMA_INVALID"
                }
            }))
        } else {
            ResponseTemplate::new(200)
                .set_body_raw(text_sse_body("Recovered"), "text/event-stream")
        }
    }
}

fn request_with_composed_tool_schema() -> LlmRequest {
    let mut request = minimal_request();
    request.tools.push(ToolDef {
        name: "Read".into(),
        description: "Read one or more files".into(),
        input_schema: serde_json::json!({
            "type": "object",
            "properties": {
                "file_path": { "type": "string" },
                "file_paths": { "type": "array", "items": { "type": "string" } }
            },
            "oneOf": [
                { "required": ["file_path"] },
                { "required": ["file_paths"] }
            ]
        }),
        deferred: false,
    });
    request
}

#[tokio::test]
async fn anthropic_gateway_recovers_and_remembers_bedrock_schema_requirement() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(AnthropicBedrockSchemaResponder)
        .expect(3)
        .mount(&server)
        .await;
    let provider = AnthropicProvider::new(
        "test-api-key",
        &server.uri(),
        ProviderCompat::anthropic_defaults(),
    )
    .with_cache(false);
    let request = request_with_composed_tool_schema();
    for _ in 0..2 {
        let events = collect_events(provider.stream(&request).await.unwrap()).await;
        assert!(events.iter().any(
            |event| matches!(event, LlmEvent::TextDelta(text) if text == "Recovered")
        ));
    }
    let received = server.received_requests().await.unwrap();
    let has_root_one_of: Vec<bool> = received
        .iter()
        .map(|request| {
            let body: serde_json::Value = serde_json::from_slice(&request.body).unwrap();
            body["tools"][0]["input_schema"].get("oneOf").is_some()
        })
        .collect();
    assert_eq!(has_root_one_of, vec![true, false, false]);
    server.verify().await;
}

#[tokio::test]
async fn anthropic_gateway_does_not_schema_retry_an_unrelated_500() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(500).set_body_string("upstream unavailable"))
        .expect(1)
        .mount(&server)
        .await;
    let provider = AnthropicProvider::new(
        "test-api-key",
        &server.uri(),
        ProviderCompat::anthropic_defaults(),
    )
    .with_cache(false);
    let error = provider
        .stream(&request_with_composed_tool_schema())
        .await
        .unwrap_err();
    assert!(matches!(error, ProviderError::Api { status: 500, .. }));
    server.verify().await;
}
```

- [ ] **Step 2: Verify RED**

Run `cargo test -p nomi-providers --test provider_anthropic_test anthropic_gateway_ -- --nocapture`.
Expected: recovery fails before fallback exists.

- [ ] **Step 3: Add learned state and sanitize the Anthropic wire schema**

Add these imports, field, initialization, and accessor. In `build_request_body`, retain `anthropic_shared::build_tools`, then sanitize conditionally:

```rust
use std::sync::atomic::{AtomicBool, Ordering};
use nomi_config::compat::{self, ProviderCompat};

pub struct AnthropicProvider {
    api_key: String,
    base_url: String,
    cache_enabled: bool,
    compat: ProviderCompat,
    sanitize_tool_schemas: AtomicBool,
}

pub fn new(api_key: &str, base_url: &str, compat: ProviderCompat) -> Self {
    Self {
        api_key: api_key.to_string(),
        base_url: base_url.to_string(),
        cache_enabled: true,
        compat,
        sanitize_tool_schemas: AtomicBool::new(false),
    }
}

fn should_sanitize_tool_schemas(&self) -> bool {
    self.compat.sanitize_schema()
        || self.sanitize_tool_schemas.load(Ordering::Acquire)
}

if self.should_sanitize_tool_schemas() {
    for tool in &mut tools {
        if let Some(schema) = tool.get("input_schema").cloned() {
            tool["input_schema"] = compat::sanitize_json_schema(&schema);
        }
    }
}
```

Preserve the existing cache marker on the last tool.

- [ ] **Step 4: Implement one classified resend**

Add this Anthropic-local method:

```rust
async fn send_initial(
    client: &reqwest::Client,
    url: &str,
    headers: &HeaderMap,
    body: &Value,
) -> Result<reqwest::Response, ProviderError> {
    crate::retry::with_initial_connect_retry(|| async {
        let response = client
            .post(url)
            .headers(headers.clone())
            .json(body)
            .send()
            .await?;
        let status = response.status();
        if status.is_success() {
            return Ok(response);
        }
        let retry_after_ms =
            crate::parse_retry_after_ms(response.headers()).unwrap_or(5000);
        let body_text = response.text().await.unwrap_or_default();
        if status.as_u16() == 429 {
            return Err(ProviderError::RateLimited {
                retry_after_ms,
                message: crate::non_empty_rate_limit_message(body_text),
            });
        }
        Err(ProviderError::Api {
            status: status.as_u16(),
            message: body_text,
        })
    })
    .await
}
```

Build headers once with `let headers = self.build_headers()?`, then replace the current initial response block with:

```rust
let used_sanitized_schema = self.should_sanitize_tool_schemas();
let mut body = self.build_request_body(request);
let response = match Self::send_initial(&client, &url, &headers, &body).await {
    Ok(response) => response,
    Err(error)
        if !request.tools.is_empty()
            && !used_sanitized_schema
            && error.is_tool_schema_incompatible() =>
    {
        let ProviderError::Api { status, .. } = &error else {
            unreachable!("schema classifier only accepts API errors");
        };
        tracing::warn!(
            target: "nomi_providers",
            provider = "anthropic",
            status,
            "provider rejected tool schemas; retrying with Bedrock-compatible schema roots"
        );
        self.sanitize_tool_schemas.store(true, Ordering::Release);
        body = self.build_request_body(request);
        Self::send_initial(&client, &url, &headers, &body).await?
    }
    Err(error) => return Err(error),
};
```

The spawned empty-stream retry captures the final `body` and the already-built headers.

- [ ] **Step 5: Verify GREEN and commit**

Run `cargo test -p nomi-providers --test provider_anthropic_test -- --nocapture`; expect zero failures.

```powershell
git add crates/agent/nomi-providers/src/anthropic.rs crates/agent/nomi-providers/tests/provider_anthropic_test.rs
git commit -m "fix(provider): adapt Anthropic gateways to Bedrock schemas"
```

---

### Task 5: Make health probes explicitly tool-free

**Files:**
- Modify: `crates/agent/nomi-tools/src/registry.rs:10-85,280-320`
- Modify: `crates/backend/nomifun-ai-agent/src/services/provider_health.rs:578-620,730-830`

**Interfaces:**
- Produces `ToolRegistry::clear(&mut self)`.
- `build_probe_engine` returns an engine whose `tool_names()` is empty.

- [ ] **Step 1: Add the failing registry test**

```rust
#[test]
fn clear_removes_every_registered_tool() {
    let mut registry = ToolRegistry::new();
    registry.register(make_tool("Read", "read files"));
    registry.register(make_tool("exec_command", "run commands"));
    registry.clear();
    assert!(registry.tool_names().is_empty());
}
```

- [ ] **Step 2: Verify RED, implement, and verify GREEN**

Run `cargo test -p nomi-tools clear_removes_every_registered_tool -- --nocapture`; expect compilation failure.

```rust
/// Remove every registered tool.
/// Unlike an empty allowlist, this is an explicit deny-all operation.
pub fn clear(&mut self) {
    self.tools.clear();
}
```

Run the same test; expect PASS.

- [ ] **Step 3: Add the failing probe-engine test**

Add this helper and test to the existing `provider_health.rs` test module:

```rust
fn test_chat_probe_config(session_directory: PathBuf) -> NomiResolvedConfig {
    NomiResolvedConfig {
        provider: "openai".to_owned(),
        api_key: "sk-test".to_owned(),
        model: "gpt-test".to_owned(),
        base_url: Some("https://api.openai.com".to_owned()),
        system_prompt: Some("Reply with exactly OK.".to_owned()),
        max_tokens: 16,
        max_turns: Some(1),
        context_limit: None,
        compat_overrides: crate::types::NomiCompatOverrides::default(),
        session_directory,
        session_mode: None,
        extra_mcp_servers: HashMap::new(),
        bedrock_config: None,
        computer_use: false,
        browser_use: false,
        browser_silent: true,
        browser_source: "managed".to_owned(),
        browser_full_power: false,
        browser_persistent_login: false,
        browser_site_memory: false,
        browser_takeover: false,
        browser_unrestricted_approval: false,
        browser_visual_fallback: false,
        goal: None,
        browser_secret_vault: None,
        owner_token: None,
        in_process_spawn: false,
        allowed_tools: Vec::new(),
        write_root: None,
    }
}

#[tokio::test]
async fn chat_health_probe_engine_has_no_tools() {
    let temp = tempfile::tempdir().unwrap();
    let engine = build_probe_engine(test_chat_probe_config(temp.path().join("sessions")))
        .await
        .unwrap();
    assert!(engine.tool_names().is_empty());
}
```

- [ ] **Step 4: Verify RED**

Run `cargo test -p nomifun-ai-agent chat_health_probe_engine_has_no_tools -- --nocapture`.
Expected: FAIL because bootstrap registers native tools and `ToolSearch`.

- [ ] **Step 5: Clear after every bootstrap registration has run**

Replace the tail of `build_probe_engine` with:

```rust
let mut result = AgentBootstrap::new(config, workspace, sink)
    .build()
    .await
    .map_err(|error| AppError::Internal(error.to_string()))?;
result.engine.registry_mut().clear();
Ok(result.engine)
```

- [ ] **Step 6: Verify GREEN and commit**

Run:

```powershell
cargo test -p nomi-tools clear_removes_every_registered_tool -- --nocapture
cargo test -p nomifun-ai-agent chat_health_probe_engine_has_no_tools -- --nocapture
cargo test -p nomifun-ai-agent services::provider_health::tests -- --nocapture
```

Expected: all commands PASS.

```powershell
git add crates/agent/nomi-tools/src/registry.rs crates/backend/nomifun-ai-agent/src/services/provider_health.rs
git commit -m "fix(provider): keep health probes tool-free"
```

---

### Task 6: Verify the complete affected scope

**Files:**
- Verify all files changed by Tasks 1-5.

**Interfaces:**
- Produces a formatting-clean, test-clean, compile-clean affected workspace.

- [ ] **Step 1: Format and inspect**

```powershell
cargo fmt --all
git diff --check
git status --short
```

Expected: formatting exits 0, diff check is silent, status contains only intentional changes.

- [ ] **Step 2: Run complete affected tests**

```powershell
cargo test -p nomi-config
cargo test -p nomi-tools
cargo test -p nomi-providers
cargo test -p nomifun-ai-agent services::provider_health::tests -- --nocapture
```

Expected: zero failed tests.

- [ ] **Step 3: Compile default and desktop feature scopes**

```powershell
cargo check -p nomi-config -p nomi-tools -p nomi-providers -p nomifun-ai-agent
cargo check -p nomifun-ai-agent --features computer-use,browser-use
```

Expected: both commands exit 0 without new warnings.

- [ ] **Step 4: Review requirements against the final diff**

Confirm from `git show --stat HEAD~5..HEAD` and `git diff HEAD~5..HEAD` that the five implementation commits contain only root schema sanitation, exact fallback classification, learned OpenAI/Anthropic modes, direct Bedrock coverage, and explicit probe tool clearing.

- [ ] **Step 5: Commit formatting-only drift if present**

If `cargo fmt` changed intentional task files, stage only those files and run `git commit -m "style: format Bedrock schema compatibility fix"`. If status is clean, create no empty commit.

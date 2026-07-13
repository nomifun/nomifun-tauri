// Integration tests for AnthropicProvider using wiremock to mock the Anthropic API.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};

use nomi_config::compat::ProviderCompat;
use nomi_providers::anthropic::AnthropicProvider;
use nomi_providers::{LlmProvider, ProviderError};
use nomi_types::llm::{LlmEvent, LlmRequest, ThinkingConfig};
use nomi_types::message::{ContentBlock, Message, Role, StopReason};
use nomi_types::tool::ToolDef;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn minimal_request() -> LlmRequest {
    LlmRequest {
        model: "claude-3-5-sonnet-20241022".to_string(),
        system: "You are helpful.".to_string(),
        messages: vec![Message::new(
            Role::User,
            vec![ContentBlock::Text {
                text: "Hello".to_string(),
            }],
        )],
        tools: vec![],
        max_tokens: 1024,
        thinking: None,
        reasoning_effort: None,
    }
}

/// Build a complete SSE body for a simple text response.
fn text_sse_body(text: &str) -> String {
    format!(
        "event: message_start\n\
         data: {{\"type\":\"message_start\",\"message\":{{\"id\":\"msg_test\",\"type\":\"message\",\"role\":\"assistant\",\"content\":[],\"model\":\"claude-3-5-sonnet-20241022\",\"stop_reason\":null,\"stop_sequence\":null,\"usage\":{{\"input_tokens\":100,\"output_tokens\":1}}}}}}\n\n\
         event: content_block_start\n\
         data: {{\"type\":\"content_block_start\",\"index\":0,\"content_block\":{{\"type\":\"text\",\"text\":\"\"}}}}\n\n\
         event: content_block_delta\n\
         data: {{\"type\":\"content_block_delta\",\"index\":0,\"delta\":{{\"type\":\"text_delta\",\"text\":\"{text}\"}}}}\n\n\
         event: content_block_stop\n\
         data: {{\"type\":\"content_block_stop\",\"index\":0}}\n\n\
         event: message_delta\n\
         data: {{\"type\":\"message_delta\",\"delta\":{{\"stop_reason\":\"end_turn\",\"stop_sequence\":null}},\"usage\":{{\"output_tokens\":50}}}}\n\n\
         event: message_stop\n\
         data: {{\"type\":\"message_stop\"}}\n\n"
    )
}

/// Collect all events from a receiver into a Vec, draining until closed.
async fn collect_events(mut rx: tokio::sync::mpsc::Receiver<LlmEvent>) -> Vec<LlmEvent> {
    let mut events = Vec::new();
    while let Some(ev) = rx.recv().await {
        events.push(ev);
    }
    events
}

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

#[derive(Clone)]
struct AnthropicFailedSanitizedResendResponder {
    attempt: Arc<AtomicUsize>,
}

impl Respond for AnthropicFailedSanitizedResendResponder {
    fn respond(&self, _request: &Request) -> ResponseTemplate {
        match self.attempt.fetch_add(1, Ordering::SeqCst) {
            0 => ResponseTemplate::new(500).set_body_json(serde_json::json!({
                "error": {
                    "message": "input_schema does not support oneOf at the top level",
                    "reason": "TOOL_SCHEMA_INVALID"
                }
            })),
            1..=3 => ResponseTemplate::new(503).set_body_string("sanitized resend unavailable"),
            _ => ResponseTemplate::new(200)
                .set_body_raw(text_sse_body("Recovered"), "text/event-stream"),
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
        assert!(
            events
                .iter()
                .any(|event| matches!(event, LlmEvent::TextDelta(text) if text == "Recovered"))
        );
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
        .expect(3)
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

#[tokio::test]
async fn anthropic_gateway_does_not_remember_a_failed_sanitized_resend() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(AnthropicFailedSanitizedResendResponder {
            attempt: Arc::new(AtomicUsize::new(0)),
        })
        .expect(5)
        .mount(&server)
        .await;
    let provider = AnthropicProvider::new(
        "test-api-key",
        &server.uri(),
        ProviderCompat::anthropic_defaults(),
    )
    .with_cache(false);
    let request = request_with_composed_tool_schema();

    let error = provider.stream(&request).await.unwrap_err();
    assert!(matches!(error, ProviderError::Api { status: 503, .. }));

    let events = collect_events(provider.stream(&request).await.unwrap()).await;
    assert!(
        events
            .iter()
            .any(|event| matches!(event, LlmEvent::TextDelta(text) if text == "Recovered"))
    );

    let received = server.received_requests().await.unwrap();
    let has_root_one_of: Vec<bool> = received
        .iter()
        .map(|request| {
            let body: serde_json::Value = serde_json::from_slice(&request.body).unwrap();
            body["tools"][0]["input_schema"].get("oneOf").is_some()
        })
        .collect();
    assert_eq!(
        has_root_one_of,
        vec![true, false, false, false, true]
    );
    server.verify().await;
}

// ---------------------------------------------------------------------------
// test_anthropic_stream_text_response
// ---------------------------------------------------------------------------

/// A normal text SSE stream produces TextDelta events followed by a Done event.
#[tokio::test]
async fn test_anthropic_stream_text_response() {
    // Arrange: start a mock server
    let server = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_raw(text_sse_body("Hello, world!"), "text/event-stream"),
        )
        .mount(&server)
        .await;

    let provider = AnthropicProvider::new(
        "test-api-key",
        &server.uri(),
        ProviderCompat::anthropic_defaults(),
    )
    .with_cache(false);
    let request = minimal_request();

    // Act
    let rx = provider
        .stream(&request)
        .await
        .expect("stream should succeed");
    let events = collect_events(rx).await;

    // Assert: at least one TextDelta and exactly one Done
    let text_deltas: Vec<&LlmEvent> = events
        .iter()
        .filter(|e| matches!(e, LlmEvent::TextDelta(_)))
        .collect();
    assert!(!text_deltas.is_empty(), "expected at least one TextDelta");

    match &text_deltas[0] {
        LlmEvent::TextDelta(text) => assert_eq!(text, "Hello, world!"),
        _ => panic!("expected TextDelta"),
    }

    let done_events: Vec<&LlmEvent> = events
        .iter()
        .filter(|e| matches!(e, LlmEvent::Done { .. }))
        .collect();
    assert_eq!(done_events.len(), 1, "expected exactly one Done event");

    match done_events[0] {
        LlmEvent::Done { stop_reason, usage } => {
            assert_eq!(*stop_reason, StopReason::EndTurn);
            assert_eq!(usage.input_tokens, 100);
            assert_eq!(usage.output_tokens, 50);
        }
        _ => panic!("expected Done"),
    }
}

// ---------------------------------------------------------------------------
// test_anthropic_stream_text_response_crlf_framed
// ---------------------------------------------------------------------------

/// Some Anthropic-compatible gateways (e.g. new-api / one-api proxies) frame SSE
/// events with CRLF ("\r\n\r\n") instead of the Anthropic API's "\n\n". The
/// stream must still parse to TextDelta + Done rather than silently yielding an
/// empty response. Regression test for the stepfun-proxy / new-api breakage.
#[tokio::test]
async fn test_anthropic_stream_text_response_crlf_framed() {
    let server = MockServer::start().await;

    // Convert the canonical LF-framed body into CRLF framing.
    let crlf_body = text_sse_body("Hello, world!").replace('\n', "\r\n");

    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(crlf_body, "text/event-stream"))
        .mount(&server)
        .await;

    let provider = AnthropicProvider::new(
        "test-api-key",
        &server.uri(),
        ProviderCompat::anthropic_defaults(),
    )
    .with_cache(false);

    let rx = provider
        .stream(&minimal_request())
        .await
        .expect("stream should succeed");
    let events = collect_events(rx).await;

    let text: String = events
        .iter()
        .filter_map(|e| match e {
            LlmEvent::TextDelta(t) => Some(t.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(text, "Hello, world!", "CRLF-framed SSE must yield text");

    let done = events
        .iter()
        .filter(|e| matches!(e, LlmEvent::Done { .. }))
        .count();
    assert_eq!(done, 1, "expected exactly one Done event from CRLF stream");
}

// ---------------------------------------------------------------------------
// test_anthropic_stream_tool_use
// ---------------------------------------------------------------------------

/// An SSE stream containing a tool_use block produces a ToolUse event with
/// accumulated JSON input.
#[tokio::test]
async fn test_anthropic_stream_tool_use() {
    let server = MockServer::start().await;

    let sse_body = "\
event: message_start\n\
data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_tool\",\"type\":\"message\",\"role\":\"assistant\",\"content\":[],\"model\":\"claude-3-5-sonnet-20241022\",\"stop_reason\":null,\"stop_sequence\":null,\"usage\":{\"input_tokens\":80,\"output_tokens\":1}}}\n\n\
event: content_block_start\n\
data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"tool_use\",\"id\":\"toolu_abc\",\"name\":\"Read\",\"input\":{}}}\n\n\
event: content_block_delta\n\
data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{\\\"file\"}}\n\n\
event: content_block_delta\n\
data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"_path\\\":\\\"/tmp/test\\\"}\"}}\n\n\
event: content_block_stop\n\
data: {\"type\":\"content_block_stop\",\"index\":0}\n\n\
event: message_delta\n\
data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"tool_use\",\"stop_sequence\":null},\"usage\":{\"output_tokens\":30}}\n\n\
event: message_stop\n\
data: {\"type\":\"message_stop\"}\n\n";

    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(sse_body, "text/event-stream"))
        .mount(&server)
        .await;

    let provider = AnthropicProvider::new(
        "test-api-key",
        &server.uri(),
        ProviderCompat::anthropic_defaults(),
    )
    .with_cache(false);
    let request = minimal_request();

    // Act
    let rx = provider
        .stream(&request)
        .await
        .expect("stream should succeed");
    let events = collect_events(rx).await;

    // Assert: one ToolUse event with correct fields
    let tool_events: Vec<&LlmEvent> = events
        .iter()
        .filter(|e| matches!(e, LlmEvent::ToolUse { .. }))
        .collect();
    assert_eq!(tool_events.len(), 1, "expected exactly one ToolUse event");

    match tool_events[0] {
        LlmEvent::ToolUse {
            id, name, input, ..
        } => {
            assert_eq!(id, "toolu_abc");
            assert_eq!(name, "Read");
            assert_eq!(input["file_path"], "/tmp/test");
        }
        _ => panic!("expected ToolUse"),
    }

    // Done event should reflect tool_use stop reason
    let done_events: Vec<&LlmEvent> = events
        .iter()
        .filter(|e| matches!(e, LlmEvent::Done { .. }))
        .collect();
    assert_eq!(done_events.len(), 1);
    match done_events[0] {
        LlmEvent::Done { stop_reason, .. } => {
            assert_eq!(*stop_reason, StopReason::ToolUse);
        }
        _ => panic!("expected Done"),
    }
}

// ---------------------------------------------------------------------------
// test_anthropic_stream_with_thinking
// ---------------------------------------------------------------------------

/// An SSE stream containing a thinking block produces ThinkingDelta events.
#[tokio::test]
async fn test_anthropic_stream_with_thinking() {
    let server = MockServer::start().await;

    let sse_body = "\
event: message_start\n\
data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_think\",\"type\":\"message\",\"role\":\"assistant\",\"content\":[],\"model\":\"claude-3-5-sonnet-20241022\",\"stop_reason\":null,\"stop_sequence\":null,\"usage\":{\"input_tokens\":90,\"output_tokens\":1}}}\n\n\
event: content_block_start\n\
data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"thinking\",\"thinking\":\"\"}}\n\n\
event: content_block_delta\n\
data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"thinking_delta\",\"thinking\":\"Let me think...\"}}\n\n\
event: content_block_stop\n\
data: {\"type\":\"content_block_stop\",\"index\":0}\n\n\
event: content_block_start\n\
data: {\"type\":\"content_block_start\",\"index\":1,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n\
event: content_block_delta\n\
data: {\"type\":\"content_block_delta\",\"index\":1,\"delta\":{\"type\":\"text_delta\",\"text\":\"Answer.\"}}\n\n\
event: content_block_stop\n\
data: {\"type\":\"content_block_stop\",\"index\":1}\n\n\
event: message_delta\n\
data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\",\"stop_sequence\":null},\"usage\":{\"output_tokens\":20}}\n\n\
event: message_stop\n\
data: {\"type\":\"message_stop\"}\n\n";

    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(sse_body, "text/event-stream"))
        .mount(&server)
        .await;

    // Enable thinking in the request
    let mut request = minimal_request();
    request.thinking = Some(ThinkingConfig::Enabled {
        budget_tokens: 5000,
    });

    let provider = AnthropicProvider::new(
        "test-api-key",
        &server.uri(),
        ProviderCompat::anthropic_defaults(),
    )
    .with_cache(false);

    // Act
    let rx = provider
        .stream(&request)
        .await
        .expect("stream should succeed");
    let events = collect_events(rx).await;

    // Assert: ThinkingDelta event present with expected content
    let thinking_events: Vec<&LlmEvent> = events
        .iter()
        .filter(|e| matches!(e, LlmEvent::ThinkingDelta(_)))
        .collect();
    assert!(
        !thinking_events.is_empty(),
        "expected at least one ThinkingDelta"
    );

    match thinking_events[0] {
        LlmEvent::ThinkingDelta(text) => assert_eq!(text, "Let me think..."),
        _ => panic!("expected ThinkingDelta"),
    }

    // TextDelta should also be present
    let text_events: Vec<&LlmEvent> = events
        .iter()
        .filter(|e| matches!(e, LlmEvent::TextDelta(_)))
        .collect();
    assert!(
        !text_events.is_empty(),
        "expected at least one TextDelta after thinking"
    );
}

// ---------------------------------------------------------------------------
// test_anthropic_auth_error
// ---------------------------------------------------------------------------

/// A 401 response from the API should produce a ProviderError::Api with status 401.
#[tokio::test]
async fn test_anthropic_auth_error() {
    let server = MockServer::start().await;

    let error_body =
        r#"{"type":"error","error":{"type":"authentication_error","message":"invalid x-api-key"}}"#;

    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(401).set_body_string(error_body))
        .mount(&server)
        .await;

    let provider = AnthropicProvider::new(
        "bad-api-key",
        &server.uri(),
        ProviderCompat::anthropic_defaults(),
    )
    .with_cache(false);
    let request = minimal_request();

    // Act
    let result = provider.stream(&request).await;

    // Assert: returns an Api error with status 401
    match result {
        Err(ProviderError::Api { status, message }) => {
            assert_eq!(status, 401);
            assert!(
                message.contains("authentication_error") || message.contains("invalid x-api-key"),
                "unexpected error message: {message}"
            );
        }
        Err(other) => panic!("expected Api error, got: {other:?}"),
        Ok(_) => panic!("expected an error but stream succeeded"),
    }
}

#[tokio::test]
async fn test_anthropic_multi_key_rotates_after_auth_failure() {
    let server = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .and(header("x-api-key", "rejected-key"))
        .respond_with(ResponseTemplate::new(401).set_body_string(
            r#"{"type":"error","error":{"type":"authentication_error","message":"invalid x-api-key"}}"#,
        ))
        .expect(1)
        .mount(&server)
        .await;

    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .and(header("x-api-key", "working-key"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_raw(text_sse_body("rotated"), "text/event-stream"),
        )
        .expect(2)
        .mount(&server)
        .await;

    let provider = AnthropicProvider::new(
        " rejected-key,\n working-key ",
        &server.uri(),
        ProviderCompat::anthropic_defaults(),
    )
    .with_cache(false);
    for _ in 0..2 {
        let events = collect_events(provider.stream(&minimal_request()).await.unwrap()).await;
        assert!(
            events
                .iter()
                .any(|event| matches!(event, LlmEvent::TextDelta(text) if text == "rotated"))
        );
    }
    server.verify().await;
}

// ---------------------------------------------------------------------------
// test_anthropic_rate_limit_retryable
// ---------------------------------------------------------------------------

/// A 429 response from the API should produce a ProviderError::RateLimited.
#[tokio::test]
async fn test_anthropic_rate_limit_retryable() {
    let server = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(429).set_body_string(
            r#"{"type":"error","error":{"type":"rate_limit_error","message":"rate limit exceeded"}}"#,
        ))
        .mount(&server)
        .await;

    let provider = AnthropicProvider::new(
        "test-api-key",
        &server.uri(),
        ProviderCompat::anthropic_defaults(),
    )
    .with_cache(false);
    let request = minimal_request();

    // Act
    let result = provider.stream(&request).await;

    // Assert: RateLimited error, which is retryable
    match result {
        Err(ProviderError::RateLimited { retry_after_ms, .. }) => {
            assert!(retry_after_ms > 0, "retry_after_ms should be positive");
        }
        Err(other) => panic!("expected RateLimited error, got: {other:?}"),
        Ok(_) => panic!("expected an error but stream succeeded"),
    }
}

// ---------------------------------------------------------------------------
// test_anthropic_request_headers
// ---------------------------------------------------------------------------

/// The provider must send the correct HTTP headers: x-api-key, anthropic-version,
/// and content-type. This test uses wiremock header matchers to verify them.
#[tokio::test]
async fn test_anthropic_request_headers() {
    let server = MockServer::start().await;

    // Register the mock with header matchers; only requests carrying the
    // correct headers will match and receive a 200 response.
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .and(header("x-api-key", "my-secret-key"))
        .and(header("anthropic-version", "2023-06-01"))
        .and(header("content-type", "application/json"))
        .respond_with(
            ResponseTemplate::new(200).set_body_raw(text_sse_body("ok"), "text/event-stream"),
        )
        .expect(1) // exactly one matching request must arrive
        .mount(&server)
        .await;

    let provider = AnthropicProvider::new(
        "my-secret-key",
        &server.uri(),
        ProviderCompat::anthropic_defaults(),
    )
    .with_cache(false);
    let request = minimal_request();

    // Act — should succeed because the headers are correct
    let result = provider.stream(&request).await;
    assert!(result.is_ok(), "stream failed: {:?}", result.err());

    // Drain the channel so the spawned task finishes
    if let Ok(rx) = result {
        collect_events(rx).await;
    }

    // wiremock verifies the `expect(1)` assertion when MockServer is dropped;
    // if the header matcher was not satisfied the test will panic here.
    server.verify().await;
}

// ---------------------------------------------------------------------------
// test_anthropic_prompt_caching_header
// ---------------------------------------------------------------------------

/// When cache is enabled the provider must include the anthropic-beta header
/// for prompt caching.
#[tokio::test]
async fn test_anthropic_prompt_caching_header() {
    let server = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .and(header("anthropic-beta", "prompt-caching-2024-07-31"))
        .respond_with(
            ResponseTemplate::new(200).set_body_raw(text_sse_body("cached"), "text/event-stream"),
        )
        .expect(1)
        .mount(&server)
        .await;

    // with_cache(true) — default, but explicit here for clarity
    let provider = AnthropicProvider::new(
        "test-api-key",
        &server.uri(),
        ProviderCompat::anthropic_defaults(),
    )
    .with_cache(true);
    let request = minimal_request();

    let result = provider.stream(&request).await;
    assert!(result.is_ok(), "stream failed: {:?}", result.err());

    if let Ok(rx) = result {
        collect_events(rx).await;
    }

    server.verify().await;
}

// ---------------------------------------------------------------------------
// test_anthropic_no_prompt_caching_header_when_disabled
// ---------------------------------------------------------------------------

/// When cache is disabled the anthropic-beta header must NOT be present.
/// We verify this by mounting a mock that matches only without that header and
/// checking it receives exactly one request.
#[tokio::test]
async fn test_anthropic_no_prompt_caching_header_when_disabled() {
    let server = MockServer::start().await;

    // This mock matches any POST to /v1/messages (no anthropic-beta requirement).
    // We then confirm via received_requests that the header is absent.
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(
            ResponseTemplate::new(200).set_body_raw(text_sse_body("no cache"), "text/event-stream"),
        )
        .mount(&server)
        .await;

    let provider = AnthropicProvider::new(
        "test-api-key",
        &server.uri(),
        ProviderCompat::anthropic_defaults(),
    )
    .with_cache(false);
    let request = minimal_request();

    let result = provider.stream(&request).await;
    assert!(result.is_ok(), "stream failed: {:?}", result.err());

    if let Ok(rx) = result {
        collect_events(rx).await;
    }

    // Inspect the captured request to assert that anthropic-beta is absent
    let received = server.received_requests().await.unwrap();
    assert_eq!(received.len(), 1, "expected exactly one request");
    let has_beta = received[0].headers.contains_key("anthropic-beta");
    assert!(
        !has_beta,
        "anthropic-beta header should not be present when cache is disabled"
    );
}

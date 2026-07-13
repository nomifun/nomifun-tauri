use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use nomi_config::compat::ProviderCompat;
use nomi_providers::{LlmProvider, ProviderError};
use nomi_providers::openai::OpenAIProvider;
use nomi_types::llm::{LlmEvent, LlmRequest};
use nomi_types::message::{ContentBlock, Message, Role, StopReason};
use nomi_types::tool::ToolDef;
use serde_json::json;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Build a minimal LlmRequest suitable for all tests.
fn make_request() -> LlmRequest {
    LlmRequest {
        model: "gpt-4o".to_string(),
        system: "You are a test assistant.".to_string(),
        messages: vec![Message::new(
            Role::User,
            vec![ContentBlock::Text {
                text: "Hello".to_string(),
            }],
        )],
        tools: vec![],
        max_tokens: 512,
        thinking: None,
        reasoning_effort: None,
    }
}

/// Collect all events from the receiver until the channel closes.
async fn collect_events(mut rx: tokio::sync::mpsc::Receiver<LlmEvent>) -> Vec<LlmEvent> {
    let mut events = Vec::new();
    while let Some(event) = rx.recv().await {
        events.push(event);
    }
    events
}

/// Build a raw SSE body string from a slice of JSON lines.
/// Each line is wrapped in `data: ...\n\n` and a final `data: [DONE]\n\n` is appended.
fn build_sse_body(data_lines: &[&str]) -> String {
    let mut body = String::new();
    for line in data_lines {
        body.push_str("data: ");
        body.push_str(line);
        body.push_str("\n\n");
    }
    body.push_str("data: [DONE]\n\n");
    body
}

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

#[derive(Clone)]
struct OpenAiFailedSanitizedResendResponder {
    attempt: Arc<AtomicUsize>,
}

impl Respond for OpenAiFailedSanitizedResendResponder {
    fn respond(&self, _request: &Request) -> ResponseTemplate {
        match self.attempt.fetch_add(1, Ordering::SeqCst) {
            0 => ResponseTemplate::new(500).set_body_json(json!({
                "error": {
                    "message": "input_schema does not support oneOf at the top level",
                    "reason": "TOOL_SCHEMA_INVALID"
                }
            })),
            1..=3 => ResponseTemplate::new(503).set_body_string("sanitized resend unavailable"),
            _ => {
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
        .expect(3)
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

#[tokio::test]
async fn openai_gateway_does_not_remember_a_failed_sanitized_resend() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(OpenAiFailedSanitizedResendResponder {
            attempt: Arc::new(AtomicUsize::new(0)),
        })
        .expect(5)
        .mount(&server)
        .await;
    let provider = OpenAIProvider::new(
        "test-key",
        &server.uri(),
        ProviderCompat::openai_defaults(),
    );
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
            body["tools"][0]["function"]["parameters"]
                .get("oneOf")
                .is_some()
        })
        .collect();
    assert_eq!(
        has_root_one_of,
        vec![true, false, false, false, true]
    );
    server.verify().await;
}

async fn start_server_after_initial_connect_refusal(sse_body: String) -> String {
    let probe = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = probe.local_addr().unwrap();
    drop(probe);

    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(500)).await;

        let listener = TcpListener::bind(addr).await.unwrap();
        let (mut second, _) = listener.accept().await.unwrap();
        let mut buf = [0_u8; 4096];
        let _ = second.read(&mut buf).await.unwrap();

        let response = format!(
            "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
            sse_body.len(),
            sse_body
        );
        second.write_all(response.as_bytes()).await.unwrap();
    });

    format!("http://{addr}")
}

// ---------------------------------------------------------------------------
// test_openai_stream_text_response
// ---------------------------------------------------------------------------

/// Verify that a normal text response (multiple content deltas followed by a
/// stop chunk with usage) is parsed into the correct sequence of TextDelta
/// and Done events.
#[tokio::test]
async fn test_openai_stream_text_response() {
    let server = MockServer::start().await;

    // Chunk 1: first text delta
    let chunk1 = json!({
        "id": "chatcmpl-001",
        "object": "chat.completion.chunk",
        "choices": [{
            "index": 0,
            "delta": { "role": "assistant", "content": "Hello" },
            "finish_reason": null
        }]
    })
    .to_string();

    // Chunk 2: second text delta
    let chunk2 = json!({
        "id": "chatcmpl-001",
        "object": "chat.completion.chunk",
        "choices": [{
            "index": 0,
            "delta": { "content": ", world!" },
            "finish_reason": null
        }]
    })
    .to_string();

    // Chunk 3: finish_reason = "stop" with usage
    let chunk3 = json!({
        "id": "chatcmpl-001",
        "object": "chat.completion.chunk",
        "choices": [{
            "index": 0,
            "delta": {},
            "finish_reason": "stop"
        }],
        "usage": {
            "prompt_tokens": 25,
            "completion_tokens": 10
        }
    })
    .to_string();

    let sse_body = build_sse_body(&[&chunk1, &chunk2, &chunk3]);

    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .and(header("authorization", "Bearer test-key"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(sse_body, "text/event-stream"))
        .mount(&server)
        .await;

    let provider =
        OpenAIProvider::new("test-key", &server.uri(), ProviderCompat::openai_defaults());
    let rx = provider.stream(&make_request()).await.unwrap();
    let events = collect_events(rx).await;

    // Expect: TextDelta("Hello"), TextDelta(", world!"), Done{EndTurn}
    assert_eq!(events.len(), 3, "expected 3 events, got: {:?}", events);

    match &events[0] {
        LlmEvent::TextDelta(text) => assert_eq!(text, "Hello"),
        e => panic!("expected TextDelta, got: {:?}", e),
    }

    match &events[1] {
        LlmEvent::TextDelta(text) => assert_eq!(text, ", world!"),
        e => panic!("expected TextDelta, got: {:?}", e),
    }

    match &events[2] {
        LlmEvent::Done { stop_reason, usage } => {
            assert_eq!(*stop_reason, StopReason::EndTurn);
            assert_eq!(usage.input_tokens, 25);
            assert_eq!(usage.output_tokens, 10);
        }
        e => panic!("expected Done, got: {:?}", e),
    }
}

// ---------------------------------------------------------------------------
// test_openai_initial_connect_error_is_retried
// ---------------------------------------------------------------------------

/// Verify that the provider retries when the initial HTTP request fails before
/// receiving any response. This covers transient connect/TLS failures where no
/// model output has been emitted yet.
#[tokio::test]
async fn test_openai_initial_connect_error_is_retried() {
    let chunk = json!({
        "id": "chatcmpl-retry",
        "object": "chat.completion.chunk",
        "choices": [{
            "index": 0,
            "delta": { "role": "assistant", "content": "Recovered" },
            "finish_reason": null
        }]
    })
    .to_string();
    let finish = json!({
        "id": "chatcmpl-retry",
        "object": "chat.completion.chunk",
        "choices": [{
            "index": 0,
            "delta": {},
            "finish_reason": "stop"
        }]
    })
    .to_string();
    let sse_body = build_sse_body(&[&chunk, &finish]);
    let base_url = start_server_after_initial_connect_refusal(sse_body).await;

    let provider = OpenAIProvider::new("test-key", &base_url, ProviderCompat::openai_defaults());
    let rx = provider.stream(&make_request()).await.unwrap();
    let events = collect_events(rx).await;

    assert_eq!(
        events.len(),
        2,
        "expected retry success events, got: {:?}",
        events
    );
    match &events[0] {
        LlmEvent::TextDelta(text) => assert_eq!(text, "Recovered"),
        e => panic!("expected TextDelta, got: {:?}", e),
    }
    match &events[1] {
        LlmEvent::Done { stop_reason, .. } => assert_eq!(*stop_reason, StopReason::EndTurn),
        e => panic!("expected Done, got: {:?}", e),
    }
}

// ---------------------------------------------------------------------------
// test_openai_stream_tool_call_aggregation
// ---------------------------------------------------------------------------

/// Verify that a tool call streamed in multiple delta chunks (id in first chunk,
/// name in first chunk, arguments split across chunks) is correctly aggregated
/// into a single ToolUse event.
#[tokio::test]
async fn test_openai_stream_tool_call_aggregation() {
    let server = MockServer::start().await;

    // Chunk 1: tool call header — id and function name arrive first
    let chunk1 = json!({
        "id": "chatcmpl-002",
        "object": "chat.completion.chunk",
        "choices": [{
            "index": 0,
            "delta": {
                "tool_calls": [{
                    "index": 0,
                    "id": "call_abc123",
                    "type": "function",
                    "function": {
                        "name": "read_file",
                        "arguments": "{\"path\":"
                    }
                }]
            },
            "finish_reason": null
        }]
    })
    .to_string();

    // Chunk 2: arguments continuation
    let chunk2 = json!({
        "id": "chatcmpl-002",
        "object": "chat.completion.chunk",
        "choices": [{
            "index": 0,
            "delta": {
                "tool_calls": [{
                    "index": 0,
                    "function": {
                        "arguments": "\"/tmp/test.txt\"}"
                    }
                }]
            },
            "finish_reason": null
        }]
    })
    .to_string();

    // Chunk 3: finish_reason = "tool_calls" with usage
    let chunk3 = json!({
        "id": "chatcmpl-002",
        "object": "chat.completion.chunk",
        "choices": [{
            "index": 0,
            "delta": {},
            "finish_reason": "tool_calls"
        }],
        "usage": {
            "prompt_tokens": 40,
            "completion_tokens": 15
        }
    })
    .to_string();

    let sse_body = build_sse_body(&[&chunk1, &chunk2, &chunk3]);

    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(sse_body, "text/event-stream"))
        .mount(&server)
        .await;

    let provider =
        OpenAIProvider::new("test-key", &server.uri(), ProviderCompat::openai_defaults());
    let rx = provider.stream(&make_request()).await.unwrap();
    let events = collect_events(rx).await;

    // Expect: early tool progress, target-bearing progress, final ToolUse,
    // Done{ToolUse}. The progress events are what keep the UI responsive while
    // long tool arguments are still streaming.
    assert_eq!(events.len(), 4, "expected 4 events, got: {:?}", events);

    match &events[0] {
        LlmEvent::ToolUseDelta { id, name, input } => {
            assert_eq!(id, "call_abc123");
            assert_eq!(name, "read_file");
            assert!(input.is_none());
        }
        e => panic!("expected first ToolUseDelta, got: {:?}", e),
    }

    match &events[1] {
        LlmEvent::ToolUseDelta { id, name, input } => {
            assert_eq!(id, "call_abc123");
            assert_eq!(name, "read_file");
            assert_eq!(input.as_ref().unwrap()["path"], "/tmp/test.txt");
        }
        e => panic!("expected second ToolUseDelta, got: {:?}", e),
    }

    match &events[2] {
        LlmEvent::ToolUse {
            id, name, input, ..
        } => {
            assert_eq!(id, "call_abc123");
            assert_eq!(name, "read_file");
            assert_eq!(input["path"], "/tmp/test.txt");
        }
        e => panic!("expected ToolUse, got: {:?}", e),
    }

    match &events[3] {
        LlmEvent::Done { stop_reason, usage } => {
            assert_eq!(*stop_reason, StopReason::ToolUse);
            assert_eq!(usage.input_tokens, 40);
            assert_eq!(usage.output_tokens, 15);
        }
        e => panic!("expected Done, got: {:?}", e),
    }
}

// ---------------------------------------------------------------------------
// test_openai_multiple_tool_calls
// ---------------------------------------------------------------------------

/// Verify that when the API streams multiple parallel tool calls (different
/// indices) they are all emitted as separate ToolUse events.
#[tokio::test]
async fn test_openai_multiple_tool_calls() {
    let server = MockServer::start().await;

    // Chunk 1: first tool call (index 0)
    let chunk1 = json!({
        "id": "chatcmpl-003",
        "object": "chat.completion.chunk",
        "choices": [{
            "index": 0,
            "delta": {
                "tool_calls": [{
                    "index": 0,
                    "id": "call_tool0",
                    "type": "function",
                    "function": {
                        "name": "list_files",
                        "arguments": "{\"dir\": \"/tmp\"}"
                    }
                }]
            },
            "finish_reason": null
        }]
    })
    .to_string();

    // Chunk 2: second tool call (index 1)
    let chunk2 = json!({
        "id": "chatcmpl-003",
        "object": "chat.completion.chunk",
        "choices": [{
            "index": 0,
            "delta": {
                "tool_calls": [{
                    "index": 1,
                    "id": "call_tool1",
                    "type": "function",
                    "function": {
                        "name": "read_file",
                        "arguments": "{\"path\": \"/etc/hosts\"}"
                    }
                }]
            },
            "finish_reason": null
        }]
    })
    .to_string();

    // Chunk 3: finish_reason = "tool_calls"
    let chunk3 = json!({
        "id": "chatcmpl-003",
        "object": "chat.completion.chunk",
        "choices": [{
            "index": 0,
            "delta": {},
            "finish_reason": "tool_calls"
        }],
        "usage": {
            "prompt_tokens": 60,
            "completion_tokens": 20
        }
    })
    .to_string();

    let sse_body = build_sse_body(&[&chunk1, &chunk2, &chunk3]);

    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(sse_body, "text/event-stream"))
        .mount(&server)
        .await;

    let provider =
        OpenAIProvider::new("test-key", &server.uri(), ProviderCompat::openai_defaults());
    let rx = provider.stream(&make_request()).await.unwrap();
    let events = collect_events(rx).await;

    // Expect: progress for both tools, final ToolUse for both, Done{ToolUse}.
    assert_eq!(events.len(), 5, "expected 5 events, got: {:?}", events);

    match &events[0] {
        LlmEvent::ToolUseDelta { id, name, input } => {
            assert_eq!(id, "call_tool0");
            assert_eq!(name, "list_files");
            assert_eq!(input.as_ref().unwrap()["dir"], "/tmp");
        }
        e => panic!("expected first ToolUseDelta, got: {:?}", e),
    }

    match &events[1] {
        LlmEvent::ToolUseDelta { id, name, input } => {
            assert_eq!(id, "call_tool1");
            assert_eq!(name, "read_file");
            assert_eq!(input.as_ref().unwrap()["path"], "/etc/hosts");
        }
        e => panic!("expected second ToolUseDelta, got: {:?}", e),
    }

    match &events[2] {
        LlmEvent::ToolUse {
            id, name, input, ..
        } => {
            assert_eq!(id, "call_tool0");
            assert_eq!(name, "list_files");
            assert_eq!(input["dir"], "/tmp");
        }
        e => panic!("expected first ToolUse, got: {:?}", e),
    }

    match &events[3] {
        LlmEvent::ToolUse {
            id, name, input, ..
        } => {
            assert_eq!(id, "call_tool1");
            assert_eq!(name, "read_file");
            assert_eq!(input["path"], "/etc/hosts");
        }
        e => panic!("expected second ToolUse, got: {:?}", e),
    }

    match &events[4] {
        LlmEvent::Done { stop_reason, .. } => {
            assert_eq!(*stop_reason, StopReason::ToolUse);
        }
        e => panic!("expected Done, got: {:?}", e),
    }
}

// ---------------------------------------------------------------------------
// test_openai_stream_state_transitions
// ---------------------------------------------------------------------------

/// Verify that the stream correctly stops processing events once it encounters
/// the `[DONE]` sentinel — any data after [DONE] is ignored and the receiver
/// channel closes cleanly.
#[tokio::test]
async fn test_openai_stream_state_transitions() {
    let server = MockServer::start().await;

    // A single text delta followed by a stop chunk, then the [DONE] sentinel.
    let chunk1 = json!({
        "id": "chatcmpl-004",
        "object": "chat.completion.chunk",
        "choices": [{
            "index": 0,
            "delta": { "content": "Transition test." },
            "finish_reason": null
        }]
    })
    .to_string();

    let chunk2 = json!({
        "id": "chatcmpl-004",
        "object": "chat.completion.chunk",
        "choices": [{
            "index": 0,
            "delta": {},
            "finish_reason": "stop"
        }],
        "usage": {
            "prompt_tokens": 10,
            "completion_tokens": 5
        }
    })
    .to_string();

    // Build SSE body manually: two data lines, then [DONE], then a stray line
    // that must NOT produce any events.
    let mut sse_body = String::new();
    sse_body.push_str("data: ");
    sse_body.push_str(&chunk1);
    sse_body.push_str("\n\n");
    sse_body.push_str("data: ");
    sse_body.push_str(&chunk2);
    sse_body.push_str("\n\n");
    sse_body.push_str("data: [DONE]\n\n");
    // Stray chunk after [DONE] — must be ignored
    sse_body.push_str("data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"ignored\"},\"finish_reason\":null}]}\n\n");

    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(sse_body, "text/event-stream"))
        .mount(&server)
        .await;

    let provider =
        OpenAIProvider::new("test-key", &server.uri(), ProviderCompat::openai_defaults());
    let rx = provider.stream(&make_request()).await.unwrap();
    let events = collect_events(rx).await;

    // Expect exactly: TextDelta, Done — the trailing chunk after [DONE] is discarded.
    assert_eq!(events.len(), 2, "expected 2 events, got: {:?}", events);

    match &events[0] {
        LlmEvent::TextDelta(text) => assert_eq!(text, "Transition test."),
        e => panic!("expected TextDelta, got: {:?}", e),
    }

    match &events[1] {
        LlmEvent::Done { stop_reason, usage } => {
            assert_eq!(*stop_reason, StopReason::EndTurn);
            assert_eq!(usage.input_tokens, 10);
            assert_eq!(usage.output_tokens, 5);
            assert_eq!(usage.cache_creation_tokens, 0);
            assert_eq!(usage.cache_read_tokens, 0);
        }
        e => panic!("expected Done, got: {:?}", e),
    }
}

// ---------------------------------------------------------------------------
// test_openai_api_error_non_success_status
// ---------------------------------------------------------------------------

/// Verify that a non-2xx HTTP response is surfaced as a ProviderError::Api.
#[tokio::test]
async fn test_openai_api_error_non_success_status() {
    let server = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(401).set_body_string(
            r#"{"error":{"message":"Invalid API key","type":"invalid_request_error"}}"#,
        ))
        .mount(&server)
        .await;

    let provider = OpenAIProvider::new("bad-key", &server.uri(), ProviderCompat::openai_defaults());
    let result = provider.stream(&make_request()).await;

    assert!(result.is_err());
    match result.unwrap_err() {
        nomi_providers::ProviderError::Api { status, .. } => {
            assert_eq!(status, 401);
        }
        e => panic!("expected Api error, got: {:?}", e),
    }
}

#[tokio::test]
async fn test_openai_multi_key_rotates_after_auth_failure() {
    let server = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .and(header("authorization", "Bearer rejected-key"))
        .respond_with(ResponseTemplate::new(401).set_body_string(
            r#"{"error":{"message":"Invalid token","type":"invalid_request_error"}}"#,
        ))
        .expect(1)
        .mount(&server)
        .await;

    let success_chunk = json!({
        "choices": [{ "delta": { "content": "rotated" }, "finish_reason": null }]
    })
    .to_string();
    let success_finish = json!({
        "choices": [{ "delta": {}, "finish_reason": "stop" }],
        "usage": { "prompt_tokens": 1, "completion_tokens": 1 }
    })
    .to_string();
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .and(header("authorization", "Bearer working-key"))
        .respond_with(
            ResponseTemplate::new(200).set_body_raw(
                build_sse_body(&[&success_chunk, &success_finish]),
                "text/event-stream",
            ),
        )
        .expect(2)
        .mount(&server)
        .await;

    let provider = OpenAIProvider::new(
        " rejected-key,\n working-key ",
        &server.uri(),
        ProviderCompat::openai_defaults(),
    );
    for _ in 0..2 {
        let events = collect_events(provider.stream(&make_request()).await.unwrap()).await;
        assert!(
            events
                .iter()
                .any(|event| matches!(event, LlmEvent::TextDelta(text) if text == "rotated"))
        );
    }
    server.verify().await;
}

// ---------------------------------------------------------------------------
// test_openai_rate_limited
// ---------------------------------------------------------------------------

/// Verify that a 429 response is surfaced as ProviderError::RateLimited.
#[tokio::test]
async fn test_openai_rate_limited() {
    let server = MockServer::start().await;

    let body = r#"{"error":{"message":"You exceeded your current quota","type":"insufficient_quota","code":"insufficient_quota"}}"#;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(429).set_body_string(body))
        .mount(&server)
        .await;

    let provider =
        OpenAIProvider::new("test-key", &server.uri(), ProviderCompat::openai_defaults());
    let result = provider.stream(&make_request()).await;

    assert!(result.is_err());
    let err = result.unwrap_err();
    let display = err.to_string();
    match err {
        nomi_providers::ProviderError::RateLimited { retry_after_ms, .. } => {
            assert_eq!(retry_after_ms, 5000);
        }
        e => panic!("expected RateLimited error, got: {:?}", e),
    }

    assert!(
        display.contains("insufficient_quota"),
        "rate limit error should preserve provider body, got: {display}"
    );
}

// ---------------------------------------------------------------------------
// test_openai_stream_max_tokens_stop_reason
// ---------------------------------------------------------------------------

/// Verify that finish_reason "length" maps to StopReason::MaxTokens.
#[tokio::test]
async fn test_openai_stream_max_tokens_stop_reason() {
    let server = MockServer::start().await;

    let chunk1 = json!({
        "id": "chatcmpl-005",
        "object": "chat.completion.chunk",
        "choices": [{
            "index": 0,
            "delta": { "content": "Truncated" },
            "finish_reason": null
        }]
    })
    .to_string();

    let chunk2 = json!({
        "id": "chatcmpl-005",
        "object": "chat.completion.chunk",
        "choices": [{
            "index": 0,
            "delta": {},
            "finish_reason": "length"
        }],
        "usage": {
            "prompt_tokens": 100,
            "completion_tokens": 512
        }
    })
    .to_string();

    let sse_body = build_sse_body(&[&chunk1, &chunk2]);

    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(sse_body, "text/event-stream"))
        .mount(&server)
        .await;

    let provider =
        OpenAIProvider::new("test-key", &server.uri(), ProviderCompat::openai_defaults());
    let rx = provider.stream(&make_request()).await.unwrap();
    let events = collect_events(rx).await;

    assert_eq!(events.len(), 2);

    match &events[1] {
        LlmEvent::Done { stop_reason, usage } => {
            assert_eq!(*stop_reason, StopReason::MaxTokens);
            assert_eq!(usage.input_tokens, 100);
            assert_eq!(usage.output_tokens, 512);
        }
        e => panic!("expected Done with MaxTokens, got: {:?}", e),
    }
}

// ---------------------------------------------------------------------------
// test_openai_stream_empty_content_delta_skipped
// ---------------------------------------------------------------------------

/// Verify that empty content strings in deltas do NOT produce TextDelta events
/// (the provider filters them out).
#[tokio::test]
async fn test_openai_stream_empty_content_delta_skipped() {
    let server = MockServer::start().await;

    // Chunk with empty content — should be silently skipped
    let chunk_empty = json!({
        "id": "chatcmpl-006",
        "object": "chat.completion.chunk",
        "choices": [{
            "index": 0,
            "delta": { "content": "" },
            "finish_reason": null
        }]
    })
    .to_string();

    let chunk_text = json!({
        "id": "chatcmpl-006",
        "object": "chat.completion.chunk",
        "choices": [{
            "index": 0,
            "delta": { "content": "actual content" },
            "finish_reason": null
        }]
    })
    .to_string();

    let chunk_done = json!({
        "id": "chatcmpl-006",
        "object": "chat.completion.chunk",
        "choices": [{
            "index": 0,
            "delta": {},
            "finish_reason": "stop"
        }],
        "usage": { "prompt_tokens": 5, "completion_tokens": 3 }
    })
    .to_string();

    let sse_body = build_sse_body(&[&chunk_empty, &chunk_text, &chunk_done]);

    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(sse_body, "text/event-stream"))
        .mount(&server)
        .await;

    let provider =
        OpenAIProvider::new("test-key", &server.uri(), ProviderCompat::openai_defaults());
    let rx = provider.stream(&make_request()).await.unwrap();
    let events = collect_events(rx).await;

    // Expect only TextDelta("actual content") and Done — no empty TextDelta
    assert_eq!(events.len(), 2, "expected 2 events, got: {:?}", events);

    match &events[0] {
        LlmEvent::TextDelta(text) => assert_eq!(text, "actual content"),
        e => panic!("expected TextDelta with actual content, got: {:?}", e),
    }

    match &events[1] {
        LlmEvent::Done { stop_reason, .. } => assert_eq!(*stop_reason, StopReason::EndTurn),
        e => panic!("expected Done, got: {:?}", e),
    }
}

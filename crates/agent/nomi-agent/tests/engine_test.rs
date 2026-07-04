mod common;

use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use nomi_agent::engine::{AgentEngine, AgentError};
use nomi_agent::output::OutputSink;
use nomi_agent::output::terminal::TerminalSink;
use nomi_agent::session::SessionManager;
use nomi_providers::{LlmProvider, ProviderError};
use nomi_tools::registry::ToolRegistry;
use nomi_types::llm::{LlmEvent, LlmRequest};
use nomi_types::message::{ContentBlock, Message, Role, StopReason, TokenUsage};
use serde_json::json;
use tempfile::tempdir;
use tokio::sync::mpsc;

use common::{MockLlmProvider, MockTool, test_config};

// ---------------------------------------------------------------------------
// Helper: build a no-color OutputFormatter for silent test output
// ---------------------------------------------------------------------------
fn silent_output() -> Arc<dyn OutputSink> {
    Arc::new(TerminalSink::new(true))
}

#[derive(Default)]
struct RecordingOutputSink {
    tool_call_deltas: Mutex<Vec<(String, String, Option<String>)>>,
    tool_calls: Mutex<Vec<(String, String)>>,
    tool_results: Mutex<Vec<(String, String, bool)>>,
    model_activity: Mutex<Vec<(String, String)>>,
}

impl OutputSink for RecordingOutputSink {
    fn emit_text_delta(&self, _text: &str, _msg_id: &str) {}
    fn emit_thinking(&self, _text: &str, _msg_id: &str) {}

    fn emit_tool_call(&self, tool_use_id: &str, name: &str, _input: &str) {
        self.tool_calls
            .lock()
            .unwrap()
            .push((tool_use_id.to_owned(), name.to_owned()));
    }

    fn emit_tool_call_delta(&self, tool_use_id: &str, name: &str, input: Option<&str>) {
        self.tool_call_deltas.lock().unwrap().push((
            tool_use_id.to_owned(),
            name.to_owned(),
            input.map(ToOwned::to_owned),
        ));
    }

    fn emit_model_activity(&self, msg_id: &str, status: &str) {
        self.model_activity
            .lock()
            .unwrap()
            .push((msg_id.to_owned(), status.to_owned()));
    }

    fn emit_tool_result(&self, tool_use_id: &str, name: &str, is_error: bool, _content: &str) {
        self.tool_results
            .lock()
            .unwrap()
            .push((tool_use_id.to_owned(), name.to_owned(), is_error));
    }

    fn emit_stream_start(&self, _msg_id: &str) {}
    fn emit_stream_end(
        &self,
        _msg_id: &str,
        _turns: usize,
        _input_tokens: u64,
        _output_tokens: u64,
        _cache_creation_tokens: u64,
        _cache_read_tokens: u64,
    ) {
    }
    fn emit_error(&self, _msg: &str) {}
    fn emit_info(&self, _msg: &str) {}
}

struct RecordingRequestProvider {
    requests: Arc<Mutex<Vec<Vec<Message>>>>,
    responses: Mutex<Vec<Vec<LlmEvent>>>,
}

struct DelayedEventsProvider {
    responses: Mutex<Vec<Vec<(Duration, LlmEvent)>>>,
}

impl DelayedEventsProvider {
    fn with_turns(turns: Vec<Vec<(Duration, LlmEvent)>>) -> Self {
        Self {
            responses: Mutex::new(turns),
        }
    }
}

#[async_trait]
impl LlmProvider for DelayedEventsProvider {
    async fn stream(
        &self,
        _request: &LlmRequest,
    ) -> Result<mpsc::Receiver<LlmEvent>, ProviderError> {
        let events = self.responses.lock().unwrap().remove(0);
        let (tx, rx) = mpsc::channel(64);
        tokio::spawn(async move {
            for (delay, event) in events {
                if !delay.is_zero() {
                    tokio::time::sleep(delay).await;
                }
                let _ = tx.send(event).await;
            }
        });
        Ok(rx)
    }
}

impl RecordingRequestProvider {
    fn new(responses: Vec<Vec<LlmEvent>>) -> Self {
        Self {
            requests: Arc::new(Mutex::new(Vec::new())),
            responses: Mutex::new(responses),
        }
    }

    fn requests(&self) -> Arc<Mutex<Vec<Vec<Message>>>> {
        Arc::clone(&self.requests)
    }
}

#[async_trait]
impl LlmProvider for RecordingRequestProvider {
    async fn stream(
        &self,
        request: &LlmRequest,
    ) -> Result<mpsc::Receiver<LlmEvent>, ProviderError> {
        self.requests.lock().unwrap().push(request.messages.clone());
        let events = self.responses.lock().unwrap().remove(0);
        let (tx, rx) = mpsc::channel(64);
        tokio::spawn(async move {
            for event in events {
                let _ = tx.send(event).await;
            }
        });
        Ok(rx)
    }
}

// ---------------------------------------------------------------------------
// test_engine_text_response_ends_turn
//
// Verifies that when the LLM returns a pure text response the engine:
//   - captures the full text
//   - reports StopReason::EndTurn
//   - completes in a single turn
// ---------------------------------------------------------------------------
#[tokio::test]
async fn test_engine_text_response_ends_turn() {
    let provider = Arc::new(MockLlmProvider::with_text_response("Hello, world!"));
    let config = test_config();
    let registry = ToolRegistry::new();
    let output = silent_output();

    let mut engine =
        AgentEngine::new_with_provider(provider, config, registry, output, std::env::temp_dir());
    let result = engine.run("Hi", "").await.expect("engine should succeed");

    assert_eq!(result.text, "Hello, world!");
    assert_eq!(result.stop_reason, StopReason::EndTurn);
    assert_eq!(result.turns, 1);
}

// ---------------------------------------------------------------------------
// test_engine_tool_use_executes_and_continues
//
// Verifies the agentic loop when the LLM first requests a tool then, after
// receiving the tool result, produces a final text answer.
//   - Turn 1: LLM emits ToolUse for "mock_tool"
//   - Turn 2: LLM emits TextDelta("Done") + EndTurn
//   - result.turns == 2 and result.text == "Done"
// ---------------------------------------------------------------------------
#[tokio::test]
async fn test_engine_tool_use_executes_and_continues() {
    let turn1 = vec![
        LlmEvent::ToolUse {
            id: "tool-1".to_string(),
            name: "mock_tool".to_string(),
            input: json!({}),
            extra: None,
        },
        LlmEvent::Done {
            stop_reason: StopReason::ToolUse,
            usage: TokenUsage {
                input_tokens: 80,
                output_tokens: 30,
                cache_creation_tokens: 0,
                cache_read_tokens: 0,
            },
        },
    ];
    let turn2 = vec![
        LlmEvent::TextDelta("Done".to_string()),
        LlmEvent::Done {
            stop_reason: StopReason::EndTurn,
            usage: TokenUsage {
                input_tokens: 100,
                output_tokens: 50,
                cache_creation_tokens: 0,
                cache_read_tokens: 0,
            },
        },
    ];

    let provider = Arc::new(MockLlmProvider::with_turns(vec![turn1, turn2]));
    let config = test_config();
    let mut registry = ToolRegistry::new();
    registry.register(Box::new(MockTool::new("mock_tool", "tool output", false)));
    let output = silent_output();

    let mut engine =
        AgentEngine::new_with_provider(provider, config, registry, output, std::env::temp_dir());
    let result = engine
        .run("Use the tool", "")
        .await
        .expect("engine should succeed");

    assert_eq!(result.turns, 2);
    assert_eq!(result.text, "Done");
}

#[tokio::test]
async fn test_engine_forwards_tool_use_delta_before_final_tool_call() {
    let turn1 = vec![
        LlmEvent::ToolUseDelta {
            id: "tool-1".to_string(),
            name: "mock_tool".to_string(),
            input: Some(json!({"file_path": "snake.html"})),
        },
        LlmEvent::ToolUse {
            id: "tool-1".to_string(),
            name: "mock_tool".to_string(),
            input: json!({"file_path": "snake.html", "content": "long payload"}),
            extra: None,
        },
        LlmEvent::Done {
            stop_reason: StopReason::ToolUse,
            usage: TokenUsage::default(),
        },
    ];
    let turn2 = vec![
        LlmEvent::TextDelta("Done".to_string()),
        LlmEvent::Done {
            stop_reason: StopReason::EndTurn,
            usage: TokenUsage::default(),
        },
    ];

    let provider = Arc::new(MockLlmProvider::with_turns(vec![turn1, turn2]));
    let mut registry = ToolRegistry::new();
    registry.register(Box::new(MockTool::new("mock_tool", "tool output", false)));
    let output = Arc::new(RecordingOutputSink::default());

    let mut engine = AgentEngine::new_with_provider(
        provider,
        test_config(),
        registry,
        output.clone(),
        std::env::temp_dir(),
    );
    let result = engine
        .run("Use the tool", "")
        .await
        .expect("engine should succeed");

    assert_eq!(result.text, "Done");
    assert_eq!(
        *output.tool_call_deltas.lock().unwrap(),
        vec![(
            "tool-1".to_string(),
            "mock_tool".to_string(),
            Some(r#"{"file_path":"snake.html"}"#.to_string())
        )]
    );
    assert_eq!(
        *output.tool_calls.lock().unwrap(),
        vec![("tool-1".to_string(), "mock_tool".to_string())]
    );
}

#[tokio::test]
async fn test_engine_emits_model_activity_during_idle_stream_gap_before_tool_use() {
    let turn1 = vec![
        (
            Duration::ZERO,
            LlmEvent::ThinkingDelta("I will create a complete Snake game.".to_string()),
        ),
        (
            Duration::from_millis(1_500),
            LlmEvent::ToolUse {
                id: "tool-1".to_string(),
                name: "mock_tool".to_string(),
                input: json!({"file_path": "snake.html", "content": "long payload"}),
                extra: None,
            },
        ),
        (
            Duration::ZERO,
            LlmEvent::Done {
                stop_reason: StopReason::ToolUse,
                usage: TokenUsage::default(),
            },
        ),
    ];
    let turn2 = vec![
        (
            Duration::ZERO,
            LlmEvent::TextDelta("Done".to_string()),
        ),
        (
            Duration::ZERO,
            LlmEvent::Done {
                stop_reason: StopReason::EndTurn,
                usage: TokenUsage::default(),
            },
        ),
    ];

    let provider = Arc::new(DelayedEventsProvider::with_turns(vec![turn1, turn2]));
    let mut registry = ToolRegistry::new();
    registry.register(Box::new(MockTool::new("mock_tool", "tool output", false)));
    let output = Arc::new(RecordingOutputSink::default());

    let mut engine = AgentEngine::new_with_provider(
        provider,
        test_config(),
        registry,
        output.clone(),
        std::env::temp_dir(),
    );
    let result = engine
        .run("Use the tool", "")
        .await
        .expect("engine should succeed");

    assert_eq!(result.text, "Done");
    let activity = output.model_activity.lock().unwrap().clone();
    assert!(
        activity.iter().any(|(_, status)| status == "preparing"),
        "engine should emit a preparing activity event while the provider stream is idle"
    );
    assert!(
        activity.iter().any(|(_, status)| status == "prepared"),
        "engine should complete the preparing activity when the next provider event arrives"
    );
}

#[tokio::test]
async fn test_engine_round_trips_thinking_signature_into_tool_followup_request() {
    let provider = Arc::new(RecordingRequestProvider::new(vec![
        vec![
            LlmEvent::ThinkingDelta("need a tool".to_string()),
            LlmEvent::ThinkingSignature("sig-123".to_string()),
            LlmEvent::ToolUse {
                id: "call_1".to_string(),
                name: "mock_tool".to_string(),
                input: json!({}),
                extra: None,
            },
            LlmEvent::Done {
                stop_reason: StopReason::ToolUse,
                usage: TokenUsage::default(),
            },
        ],
        vec![
            LlmEvent::TextDelta("done".to_string()),
            LlmEvent::Done {
                stop_reason: StopReason::EndTurn,
                usage: TokenUsage::default(),
            },
        ],
    ]));
    let requests = provider.requests();

    let mut registry = ToolRegistry::new();
    registry.register(Box::new(MockTool::new("mock_tool", "tool result", false)));

    let mut engine = AgentEngine::new_with_provider(
        provider,
        test_config(),
        registry,
        silent_output(),
        std::env::temp_dir(),
    );

    let result = engine
        .run("use tool", "")
        .await
        .expect("engine should succeed");

    assert_eq!(result.text, "done");
    let requests = requests.lock().unwrap();
    assert_eq!(requests.len(), 2);

    let followup_messages = &requests[1];
    let assistant_message = followup_messages
        .iter()
        .find(|message| message.role == Role::Assistant)
        .expect("assistant message should be present");

    match &assistant_message.content[0] {
        ContentBlock::Thinking {
            thinking,
            signature,
        } => {
            assert_eq!(thinking, "need a tool");
            assert_eq!(signature.as_deref(), Some("sig-123"));
        }
        other => panic!("expected thinking block, got {other:?}"),
    }
}

#[tokio::test]
async fn duplicate_tool_names_emit_distinct_tool_use_ids() {
    let turn1 = vec![
        LlmEvent::ToolUse {
            id: "call_a".to_string(),
            name: "Glob".to_string(),
            input: json!({"pattern": "*.rs"}),
            extra: None,
        },
        LlmEvent::ToolUse {
            id: "call_b".to_string(),
            name: "Glob".to_string(),
            input: json!({"pattern": "*.toml"}),
            extra: None,
        },
        LlmEvent::Done {
            stop_reason: StopReason::ToolUse,
            usage: TokenUsage {
                input_tokens: 80,
                output_tokens: 30,
                cache_creation_tokens: 0,
                cache_read_tokens: 0,
            },
        },
    ];
    let turn2 = vec![
        LlmEvent::TextDelta("Done".to_string()),
        LlmEvent::Done {
            stop_reason: StopReason::EndTurn,
            usage: TokenUsage {
                input_tokens: 100,
                output_tokens: 50,
                cache_creation_tokens: 0,
                cache_read_tokens: 0,
            },
        },
    ];

    let provider = Arc::new(MockLlmProvider::with_turns(vec![turn1, turn2]));
    let config = test_config();
    let mut registry = ToolRegistry::new();
    registry.register(Box::new(MockTool::new("Glob", "tool output", false)));
    let output = Arc::new(RecordingOutputSink::default());

    let mut engine = AgentEngine::new_with_provider(
        provider,
        config,
        registry,
        output.clone(),
        std::env::temp_dir(),
    );
    let result = engine
        .run("Use Glob twice", "")
        .await
        .expect("engine should succeed");

    assert_eq!(result.text, "Done");
    assert_eq!(
        *output.tool_calls.lock().unwrap(),
        vec![
            ("call_a".to_string(), "Glob".to_string()),
            ("call_b".to_string(), "Glob".to_string()),
        ]
    );
    assert_eq!(
        *output.tool_results.lock().unwrap(),
        vec![
            ("call_a".to_string(), "Glob".to_string(), false),
            ("call_b".to_string(), "Glob".to_string(), false),
        ]
    );
}

// ---------------------------------------------------------------------------
// test_engine_max_tokens_handling
//
// Verifies that a MaxTokens stop reason is surfaced correctly when the LLM
// hits its token limit mid-response.
// ---------------------------------------------------------------------------
#[tokio::test]
async fn test_engine_max_tokens_handling() {
    let events = vec![
        LlmEvent::TextDelta("partial".to_string()),
        LlmEvent::Done {
            stop_reason: StopReason::MaxTokens,
            usage: TokenUsage {
                input_tokens: 200,
                output_tokens: 100,
                cache_creation_tokens: 0,
                cache_read_tokens: 0,
            },
        },
    ];

    let provider = Arc::new(MockLlmProvider::with_events(events));
    let config = test_config();
    let registry = ToolRegistry::new();
    let output = silent_output();

    let mut engine =
        AgentEngine::new_with_provider(provider, config, registry, output, std::env::temp_dir());
    let result = engine
        .run("Give me a long answer", "")
        .await
        .expect("engine should succeed");

    assert_eq!(result.stop_reason, StopReason::MaxTokens);
    assert_eq!(result.text, "partial");
}

// ---------------------------------------------------------------------------
// test_engine_message_accumulation
//
// Verifies that consecutive calls to `run` accumulate messages across turns.
// Session persistence is used to observe the messages externally since
// engine.messages is private.
//
// After two independent `run` calls the persisted session must contain
// exactly 4 messages: [user, assistant, user, assistant].
// ---------------------------------------------------------------------------
#[tokio::test]
async fn test_engine_message_accumulation() {
    let dir = tempdir().expect("tempdir should be created");

    // Provider needs two responses (one per run() call)
    let provider = Arc::new(MockLlmProvider::with_turns(vec![
        vec![
            LlmEvent::TextDelta("Response 1".to_string()),
            LlmEvent::Done {
                stop_reason: StopReason::EndTurn,
                usage: TokenUsage {
                    input_tokens: 10,
                    output_tokens: 5,
                    cache_creation_tokens: 0,
                    cache_read_tokens: 0,
                },
            },
        ],
        vec![
            LlmEvent::TextDelta("Response 2".to_string()),
            LlmEvent::Done {
                stop_reason: StopReason::EndTurn,
                usage: TokenUsage {
                    input_tokens: 10,
                    output_tokens: 5,
                    cache_creation_tokens: 0,
                    cache_read_tokens: 0,
                },
            },
        ],
    ]));

    let mut config = test_config();
    config.session.enabled = true;
    config.session.directory = dir.path().to_string_lossy().into_owned();

    let registry = ToolRegistry::new();
    let output = silent_output();

    let mut engine = AgentEngine::new_with_provider(
        provider,
        config.clone(),
        registry,
        output,
        std::env::temp_dir(),
    );

    // Initialize session so save_session() has a session to persist
    engine
        .init_session("test-provider", "/tmp", None)
        .expect("init_session should succeed");

    engine
        .run("First message", "")
        .await
        .expect("first run should succeed");
    engine
        .run("Second message", "")
        .await
        .expect("second run should succeed");

    // Load the persisted session and count accumulated messages
    let session_manager = SessionManager::new(dir.path().to_path_buf(), 10);
    let session = session_manager
        .load("latest")
        .expect("session should be loadable");

    // Expected layout: user, assistant, user, assistant
    assert_eq!(
        session.messages.len(),
        4,
        "expected 4 messages (user+assistant for each run), got {}",
        session.messages.len()
    );
}

// ---------------------------------------------------------------------------
// test_engine_token_usage_tracking
//
// Verifies that token usage is accumulated correctly across multiple turns.
//   - Turn 1: ToolUse with usage(80 in, 30 out)
//   - Turn 2: EndTurn  with usage(100 in, 50 out)
//   - Expected total: input=180, output=80
// ---------------------------------------------------------------------------
#[tokio::test]
async fn test_engine_token_usage_tracking() {
    let turn1 = vec![
        LlmEvent::ToolUse {
            id: "tool-1".to_string(),
            name: "mock_tool".to_string(),
            input: json!({}),
            extra: None,
        },
        LlmEvent::Done {
            stop_reason: StopReason::ToolUse,
            usage: TokenUsage {
                input_tokens: 80,
                output_tokens: 30,
                cache_creation_tokens: 0,
                cache_read_tokens: 0,
            },
        },
    ];
    let turn2 = vec![
        LlmEvent::TextDelta("Final answer".to_string()),
        LlmEvent::Done {
            stop_reason: StopReason::EndTurn,
            usage: TokenUsage {
                input_tokens: 100,
                output_tokens: 50,
                cache_creation_tokens: 0,
                cache_read_tokens: 0,
            },
        },
    ];

    let provider = Arc::new(MockLlmProvider::with_turns(vec![turn1, turn2]));
    let config = test_config();
    let mut registry = ToolRegistry::new();
    registry.register(Box::new(MockTool::new("mock_tool", "result", false)));
    let output = silent_output();

    let mut engine =
        AgentEngine::new_with_provider(provider, config, registry, output, std::env::temp_dir());
    let result = engine
        .run("Do work", "")
        .await
        .expect("engine should succeed");

    assert_eq!(
        result.usage.input_tokens, 180,
        "input tokens should accumulate across turns"
    );
    assert_eq!(
        result.usage.output_tokens, 80,
        "output tokens should accumulate across turns"
    );
}

// ---------------------------------------------------------------------------
// test_engine_max_turns_returns_ok
//
// Verifies that the engine returns Ok with StopReason::MaxTurns when the
// LLM keeps requesting tools beyond the configured max_turns limit.
//
// With max_turns=1 the engine executes one turn.  If that turn has tool
// calls it processes them, then loops back and hits the limit.
// ---------------------------------------------------------------------------
#[tokio::test]
async fn test_engine_max_turns_returns_ok() {
    let tool_use_turn = || {
        vec![
            LlmEvent::ToolUse {
                id: "tool-1".to_string(),
                name: "mock_tool".to_string(),
                input: json!({}),
                extra: None,
            },
            LlmEvent::Done {
                stop_reason: StopReason::ToolUse,
                usage: TokenUsage {
                    input_tokens: 50,
                    output_tokens: 20,
                    cache_creation_tokens: 0,
                    cache_read_tokens: 0,
                },
            },
        ]
    };

    let provider = Arc::new(MockLlmProvider::with_turns(vec![
        tool_use_turn(),
        tool_use_turn(),
    ]));

    let mut config = test_config();
    config.max_turns = Some(1);

    let mut registry = ToolRegistry::new();
    registry.register(Box::new(MockTool::new("mock_tool", "result", false)));
    let output = silent_output();

    let mut engine =
        AgentEngine::new_with_provider(provider, config, registry, output, std::env::temp_dir());
    let result = engine
        .run("Keep calling tools", "")
        .await
        .expect("should return Ok, not Err");

    assert_eq!(result.stop_reason, StopReason::MaxTurns);
    assert_eq!(result.turns, 1);
}

// ---------------------------------------------------------------------------
// test_engine_api_error_handling
//
// Verifies that an LlmEvent::Error propagates as AgentError::ApiError with
// the original error message intact.
// ---------------------------------------------------------------------------
#[tokio::test]
async fn test_engine_api_error_handling() {
    let events = vec![LlmEvent::Error("test error".to_string())];

    let provider = Arc::new(MockLlmProvider::with_events(events));
    let config = test_config();
    let registry = ToolRegistry::new();
    let output = silent_output();

    let mut engine =
        AgentEngine::new_with_provider(provider, config, registry, output, std::env::temp_dir());
    let err = engine
        .run("Hello", "")
        .await
        .map(|_| panic!("expected error, got Ok"))
        .unwrap_err();

    match err {
        AgentError::ApiError(msg) => assert_eq!(msg, "test error"),
        other => panic!("expected ApiError(\"test error\"), got: {:?}", other),
    }
}

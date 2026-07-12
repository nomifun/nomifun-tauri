use std::sync::{Arc, Mutex};

use nomi_agent::confirm::ToolConfirmer;
use nomi_agent::engine::AgentEngine;
use nomi_agent::orchestration::execute_tool_calls;
use nomi_agent::output::OutputSink;
use nomi_agent::output::null_sink::NullSink;
use nomi_compact::CompactionLevel;
use nomi_config::compat::ProviderCompat;
use nomi_config::config::{Config, ProviderType, SessionConfig, ToolsConfig};
use nomi_config::hooks::HooksConfig;
use nomi_mcp::config::McpConfig;
use nomi_providers::create_provider;
use nomi_tools::registry::ToolRegistry;
use nomi_types::message::ContentBlock;
use serde_json::json;

const TEST_OUTPUT: &str = "\x1b[32mSTATUS: OK\x1b[0m\n\n\n\n50%\r100%\nCompiling dep-0 v1.0.0\nCompiling dep-1 v1.0.0\nCompiling dep-2 v1.0.0\nCompiling dep-3 v1.0.0\nCompiling dep-4 v1.0.0\n{\n    \"id\": 1,\n    \"name\": \"Alice Wonderland\",\n    \"email\": \"alice@example.com\",\n    \"age\": 30,\n    \"address\": \"123 Main Street, Anytown, USA 12345\",\n    \"phone\": \"+1-555-0123\"\n}";

const TOON_INPUT: &str =
    r#"[{"id":1,"name":"Alice","role":"admin"},{"id":2,"name":"Bob","role":"user"}]"#;

fn openai_api_key() -> Option<String> {
    std::env::var("OPENAI_API_KEY")
        .ok()
        .filter(|k| !k.is_empty())
}

fn openai_config(api_key: &str) -> Config {
    Config {
        provider: ProviderType::OpenAI,
        provider_label: "openai".to_string(),
        api_key: api_key.to_string(),
        base_url: "https://api.openai.com".to_string(),
        model: "gpt-4o-mini".to_string(),
        max_tokens: 256,
        max_turns: Some(3),
        system_prompt: Some(
            "You are a helpful assistant. Be concise. Answer exactly what is asked.".to_string(),
        ),
        project_instructions: Default::default(),
        thinking: None,
        prompt_caching: false,
        compat: ProviderCompat::openai_defaults(),
        tools: ToolsConfig {
            auto_approve: true,
            allow_list: vec![],
            ..ToolsConfig::default()
        },
        session: SessionConfig {
            enabled: false,
            directory: "/tmp".to_string(),
            max_sessions: 1,
        },
        compact: nomi_config::compact::CompactConfig::default(),
        plan: nomi_config::plan::PlanConfig::default(),
        file_cache: nomi_config::file_cache::FileCacheConfig::default(),
        hooks: HooksConfig::default(),
        bedrock: None,
        vertex: None,
        mcp: McpConfig::default(),
        logging: nomi_config::logging::LoggingConfig::default(),
    }
}

struct FixedOutputTool {
    name: String,
    output: String,
}

impl FixedOutputTool {
    fn new(name: &str, output: &str) -> Self {
        Self {
            name: name.to_string(),
            output: output.to_string(),
        }
    }
}

#[async_trait::async_trait]
impl nomi_tools::Tool for FixedOutputTool {
    fn name(&self) -> &str {
        &self.name
    }

    fn description(&self) -> &str {
        "Returns fixed output for testing"
    }

    fn input_schema(&self) -> serde_json::Value {
        json!({"type": "object", "properties": {}, "required": []})
    }

    fn category(&self) -> nomi_protocol::events::ToolCategory {
        nomi_protocol::events::ToolCategory::Info
    }

    fn is_concurrency_safe(&self, _input: &serde_json::Value) -> bool {
        true
    }

    async fn execute(&self, _input: serde_json::Value) -> nomi_types::tool::ToolResult {
        nomi_types::tool::ToolResult {
            content: self.output.clone(),
            is_error: false,
            images: Vec::new(),
        }
    }
}

fn extract_tool_result_content(blocks: &[ContentBlock]) -> Option<String> {
    for block in blocks {
        if let ContentBlock::ToolResult { content, .. } = block {
            return Some(content.clone());
        }
    }
    None
}

// ---------------------------------------------------------------------------
// C Layer: Case 9 (Off vs Safe content comparison)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn case_9_off_vs_safe_content() {
    let Some(api_key) = openai_api_key() else {
        eprintln!("[e2e:compaction] OPENAI_API_KEY not set — skipping");
        return;
    };

    eprintln!("[e2e:compaction] === Case 9: Off vs Safe content comparison ===");

    let confirmer = Arc::new(Mutex::new(ToolConfirmer::new(true, vec![])));

    let mut registry = ToolRegistry::new();
    registry.register(Box::new(FixedOutputTool::new("check_tool", TEST_OUTPUT)));
    let tool_calls = vec![ContentBlock::ToolUse {
        id: "t1".to_string(),
        name: "check_tool".to_string(),
        input: json!({}),
        extra: None,
    }];

    // Off
    let outcome_off = execute_tool_calls(
        &registry,
        &tool_calls,
        &confirmer,
        None,
        CompactionLevel::Off,
        false,
    )
    .await
    .expect("should succeed");
    let content_off = extract_tool_result_content(&outcome_off).unwrap();

    // Safe
    let outcome_safe = execute_tool_calls(
        &registry,
        &tool_calls,
        &confirmer,
        None,
        CompactionLevel::Safe,
        false,
    )
    .await
    .expect("should succeed");
    let content_safe = extract_tool_result_content(&outcome_safe).unwrap();

    eprintln!("[e2e:compaction] Off content ({} chars)", content_off.len());
    eprintln!(
        "[e2e:compaction] Safe content ({} chars)",
        content_safe.len()
    );

    assert!(
        content_off.contains("\x1b"),
        "Off should preserve ANSI escapes"
    );
    assert!(
        !content_safe.contains("\x1b"),
        "Safe should strip ANSI escapes"
    );

    // LLM question (secondary evidence)
    let mut config = openai_config(&api_key);
    config.compact.compaction = CompactionLevel::Safe;

    let provider = create_provider(&config);
    let mut registry2 = ToolRegistry::new();
    registry2.register(Box::new(FixedOutputTool::new("check_tool", TEST_OUTPUT)));
    let output: Arc<dyn OutputSink> = Arc::new(NullSink);
    let mut engine =
        AgentEngine::new_with_provider(provider, config, registry2, output, std::env::temp_dir());

    let prompt = "Call check_tool, then answer: does the tool output contain ANSI color escape codes (sequences starting with \\x1b)? Answer only 'yes' or 'no'.";
    let result = engine
        .run(prompt, "")
        .await
        .expect("engine.run should succeed");

    eprintln!("[e2e:compaction] LLM question: does Safe output contain ANSI?");
    eprintln!("[e2e:compaction] LLM answer: {}", result.text);
    eprintln!(
        "[e2e:compaction] Token usage: {} input / {} output",
        result.usage.input_tokens, result.usage.output_tokens
    );

    let answer = result.text.to_lowercase();
    if answer.contains("no") {
        eprintln!("[e2e:compaction] ✓ LLM confirms no ANSI in Safe output");
    } else {
        eprintln!(
            "[e2e:compaction] ⚠ LLM answer unexpected (non-deterministic, logged for review)"
        );
    }

    eprintln!("[e2e:compaction] ✓ PASS (primary: content assertions passed)");
}

// ---------------------------------------------------------------------------
// C Layer: Case 10 (Off vs Full token savings)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn case_10_off_vs_full_token_savings() {
    let Some(api_key) = openai_api_key() else {
        eprintln!("[e2e:compaction] OPENAI_API_KEY not set — skipping");
        return;
    };

    eprintln!("[e2e:compaction] === Case 10: Off vs Full token savings ===");

    let mut large_output = String::new();
    for i in 0..20 {
        large_output.push_str(&format!(
            "Compiling dependency-{i} v0.1.0 (registry+https://github.com/rust-lang/crates.io-index)\n"
        ));
    }
    large_output.push_str("{\n    \"users\": [\n");
    for i in 0..10 {
        large_output.push_str(&format!(
            "        {{\n            \"id\": {i},\n            \"name\": \"User {i}\",\n            \"email\": \"user{i}@example.com\"\n        }}{}\n",
            if i < 9 { "," } else { "" }
        ));
    }
    large_output.push_str("    ]\n}");

    // Off
    let mut config_off = openai_config(&api_key);
    config_off.compact.compaction = CompactionLevel::Off;
    let provider_off = create_provider(&config_off);
    let mut registry_off = ToolRegistry::new();
    registry_off.register(Box::new(FixedOutputTool::new("big_tool", &large_output)));
    let output_off: Arc<dyn OutputSink> = Arc::new(NullSink);
    let mut engine_off = AgentEngine::new_with_provider(
        provider_off,
        config_off,
        registry_off,
        output_off,
        std::env::temp_dir(),
    );

    let prompt = "Call big_tool, then say 'done'.";
    let result_off = engine_off
        .run(prompt, "")
        .await
        .expect("engine.run should succeed");

    // Full
    let mut config_full = openai_config(&api_key);
    config_full.compact.compaction = CompactionLevel::Full;
    let provider_full = create_provider(&config_full);
    let mut registry_full = ToolRegistry::new();
    registry_full.register(Box::new(FixedOutputTool::new("big_tool", &large_output)));
    let output_full: Arc<dyn OutputSink> = Arc::new(NullSink);
    let mut engine_full = AgentEngine::new_with_provider(
        provider_full,
        config_full,
        registry_full,
        output_full,
        std::env::temp_dir(),
    );

    let result_full = engine_full
        .run(prompt, "")
        .await
        .expect("engine.run should succeed");

    eprintln!(
        "[e2e:compaction] Off  input_tokens: {}",
        result_off.usage.input_tokens
    );
    eprintln!(
        "[e2e:compaction] Full input_tokens: {}",
        result_full.usage.input_tokens
    );
    eprintln!(
        "[e2e:compaction] Savings: {} tokens ({:.1}%)",
        result_off
            .usage
            .input_tokens
            .saturating_sub(result_full.usage.input_tokens),
        if result_off.usage.input_tokens > 0 {
            (1.0 - result_full.usage.input_tokens as f64 / result_off.usage.input_tokens as f64)
                * 100.0
        } else {
            0.0
        }
    );

    assert!(
        result_full.usage.input_tokens < result_off.usage.input_tokens,
        "Full compaction should use fewer input tokens: full={} vs off={}",
        result_full.usage.input_tokens,
        result_off.usage.input_tokens
    );

    eprintln!("[e2e:compaction] ✓ PASS");
}

// ---------------------------------------------------------------------------
// C Layer: Case 11 (TOON comprehension + system prompt)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn case_11_toon_comprehension_and_system_prompt() {
    let Some(api_key) = openai_api_key() else {
        eprintln!("[e2e:compaction] OPENAI_API_KEY not set — skipping");
        return;
    };

    eprintln!("[e2e:compaction] === Case 11: TOON comprehension + system prompt ===");

    // Direct content check (deterministic)
    let confirmer = Arc::new(Mutex::new(ToolConfirmer::new(true, vec![])));
    let mut registry_check = ToolRegistry::new();
    registry_check.register(Box::new(FixedOutputTool::new("data_tool", TOON_INPUT)));
    let tool_calls = vec![ContentBlock::ToolUse {
        id: "t1".to_string(),
        name: "data_tool".to_string(),
        input: json!({}),
        extra: None,
    }];

    let outcome = execute_tool_calls(
        &registry_check,
        &tool_calls,
        &confirmer,
        None,
        CompactionLevel::Full,
        true,
    )
    .await
    .expect("should succeed");
    let content = extract_tool_result_content(&outcome).unwrap();

    eprintln!("[e2e:compaction] TOON-encoded content: {content}");
    assert!(
        content.contains("[2]{id,name,role}:"),
        "should contain TOON header: {content}"
    );

    // LLM comprehension test
    let mut config = openai_config(&api_key);
    config.compact.compaction = CompactionLevel::Full;
    config.compact.toon = true;

    let provider = create_provider(&config);
    let mut registry = ToolRegistry::new();
    registry.register(Box::new(FixedOutputTool::new("data_tool", TOON_INPUT)));
    let output: Arc<dyn OutputSink> = Arc::new(NullSink);
    let mut engine =
        AgentEngine::new_with_provider(provider, config, registry, output, std::env::temp_dir());

    let prompt = "Call data_tool, then answer: what is the name of the second record? Answer with just the name, nothing else.";
    let result = engine
        .run(prompt, "")
        .await
        .expect("engine.run should succeed");

    eprintln!("[e2e:compaction] LLM question: name of second record?");
    eprintln!("[e2e:compaction] LLM answer: {}", result.text);
    eprintln!(
        "[e2e:compaction] Token usage: {} input / {} output",
        result.usage.input_tokens, result.usage.output_tokens
    );

    let answer = result.text.to_lowercase();
    if answer.contains("bob") {
        eprintln!("[e2e:compaction] ✓ LLM correctly understood TOON format");
    } else {
        eprintln!(
            "[e2e:compaction] ⚠ LLM answer: '{}' (expected 'Bob', logged for review)",
            result.text
        );
    }

    eprintln!("[e2e:compaction] ✓ PASS (primary: TOON content assertion passed)");
}

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use nomi_agent::bootstrap::AgentBootstrap;
use nomi_agent::output::null_sink::NullSink;
use nomi_config::compat::ProviderCompat;
use nomi_config::config::{Config, ProviderType};
use nomi_providers::{LlmProvider, ProviderError};
use nomi_types::llm::{LlmEvent, LlmRequest};
use nomi_types::message::{StopReason, TokenUsage};
use tokio::sync::mpsc;

fn minimal_config() -> Config {
    Config {
        provider_label: "openai".into(),
        provider: ProviderType::OpenAI,
        api_key: "sk-test".into(),
        base_url: "http://localhost:0".into(),
        model: "gpt-test-model".into(),
        max_tokens: 1024,
        max_turns: Some(5),
        system_prompt: None,
        project_instructions: Default::default(),
        thinking: None,
        prompt_caching: false,
        compat: ProviderCompat::openai_defaults(),
        tools: Default::default(),
        session: Default::default(),
        compact: Default::default(),
        plan: Default::default(),
        file_cache: Default::default(),
        hooks: Default::default(),
        bedrock: None,
        vertex: None,
        mcp: Default::default(),
        logging: Default::default(),
    }
}

fn null_output() -> Arc<dyn nomi_agent::output::OutputSink> {
    Arc::new(NullSink)
}

struct CapturingProvider {
    systems: Arc<Mutex<Vec<String>>>,
}

#[async_trait]
impl LlmProvider for CapturingProvider {
    async fn stream(
        &self,
        request: &LlmRequest,
    ) -> Result<mpsc::Receiver<LlmEvent>, ProviderError> {
        self.systems.lock().unwrap().push(request.system.clone());
        let (tx, rx) = mpsc::channel(1);
        tx.send(LlmEvent::Done {
            stop_reason: StopReason::EndTurn,
            usage: TokenUsage::default(),
        })
        .await
        .unwrap();
        Ok(rx)
    }
}

#[tokio::test]
async fn bootstrap_builds_engine_with_model_in_prompt() {
    let config = minimal_config();
    let result = AgentBootstrap::new(config, "/tmp/test-workspace", null_output())
        .build()
        .await
        .expect("bootstrap should succeed");

    assert!(!result.engine.tool_names().is_empty());
    assert!(!result.has_mcp);
    assert!(result.mcp_managers.is_empty());
}

#[tokio::test]
async fn bootstrap_registers_all_expected_tools() {
    let config = minimal_config();
    let result = AgentBootstrap::new(config, "/tmp/test-workspace", null_output())
        .build()
        .await
        .unwrap();

    let names = result.engine.tool_names();

    for expected in &[
        "Read",
        "Write",
        "Edit",
        "Bash",
        "Grep",
        "Glob",
        "exec_command",
        "write_stdin",
        "update_plan",
    ] {
        assert!(
            names.iter().any(|n| n == expected),
            "missing built-in tool: {expected}"
        );
    }

    assert!(
        names.iter().any(|n| n == "Skill"),
        "SkillTool should be registered"
    );
    assert!(
        names.iter().any(|n| n == "Spawn"),
        "SpawnTool should be registered"
    );
    assert!(
        names.iter().any(|n| n == "ToolSearch"),
        "ToolSearchTool should be registered"
    );
}

#[tokio::test]
async fn bootstrap_spawn_gated_off_when_in_process_spawn_false() {
    let mut config = minimal_config();
    config.tools.in_process_spawn = false;

    let result = AgentBootstrap::new(config, "/tmp/test-workspace", null_output())
        .build()
        .await
        .unwrap();

    let names = result.engine.tool_names();
    assert!(
        !names.iter().any(|n| n == "Spawn"),
        "门控关闭时不得注册进程内 Spawn（桌面会话改走 nomi_spawn 编排扇出）"
    );
    // 其余内建工具不受影响。
    assert!(names.iter().any(|n| n == "Read"));
    assert!(names.iter().any(|n| n == "Bash"));
}

#[tokio::test]
async fn bootstrap_builtin_allowlist_restricts_tools() {
    let mut config = minimal_config();
    config.tools.builtin_allowlist = vec!["Read".into(), "Grep".into(), "Glob".into()];

    let result = AgentBootstrap::new(config, "/tmp/test-workspace", null_output())
        .build()
        .await
        .unwrap();

    let names = result.engine.tool_names();
    assert!(names.iter().any(|n| n == "Read"));
    assert!(names.iter().any(|n| n == "Grep"));
    assert!(names.iter().any(|n| n == "Glob"));
    for denied in &[
        "Bash",
        "Write",
        "Edit",
        "Spawn",
        "Skill",
        "exec_command",
        "write_stdin",
        "update_plan",
    ] {
        assert!(
            !names.iter().any(|n| n == denied),
            "白名单外的工具必须被过滤: {denied}"
        );
    }
}

#[tokio::test]
async fn bootstrap_builtin_allowlist_can_keep_native_exec_pair() {
    let mut config = minimal_config();
    config.tools.builtin_allowlist = vec!["exec_command".into(), "write_stdin".into()];

    let result = AgentBootstrap::new(config, "/tmp/test-workspace", null_output())
        .build()
        .await
        .unwrap();
    let names = result.engine.tool_names();

    assert!(names.iter().any(|name| name == "exec_command"));
    assert!(names.iter().any(|name| name == "write_stdin"));
    assert!(!names.iter().any(|name| name == "Bash"));
}

#[tokio::test]
async fn bootstrap_plan_tools_when_enabled() {
    let mut config = minimal_config();
    config.plan.enabled = true;

    let result = AgentBootstrap::new(config, "/tmp/test-workspace", null_output())
        .build()
        .await
        .unwrap();

    let names = result.engine.tool_names();
    assert!(
        names.iter().any(|n| n == "EnterPlanMode"),
        "EnterPlanMode should be registered when plan.enabled"
    );
    assert!(
        names.iter().any(|n| n == "ExitPlanMode"),
        "ExitPlanMode should be registered when plan.enabled"
    );
}

#[tokio::test]
async fn bootstrap_no_plan_tools_when_disabled() {
    let mut config = minimal_config();
    config.plan.enabled = false;

    let result = AgentBootstrap::new(config, "/tmp/test-workspace", null_output())
        .build()
        .await
        .unwrap();

    let names = result.engine.tool_names();
    assert!(
        !names.iter().any(|n| n == "EnterPlanMode"),
        "EnterPlanMode should NOT be registered when plan.disabled"
    );
}

#[tokio::test]
async fn bootstrap_no_mcp_when_no_servers() {
    let config = minimal_config();
    let result = AgentBootstrap::new(config, "/tmp/test-workspace", null_output())
        .build()
        .await
        .unwrap();

    assert!(!result.has_mcp);
    assert!(result.mcp_managers.is_empty());
}

#[tokio::test]
async fn bootstrap_with_custom_system_prompt() {
    let mut config = minimal_config();
    config.system_prompt = Some("You are a pirate assistant.".into());

    let _result = AgentBootstrap::new(config, "/tmp/test-workspace", null_output())
        .build()
        .await
        .unwrap();
}

#[tokio::test]
async fn bootstrap_with_agents_md_in_workspace() {
    let tmp = tempfile::TempDir::new().unwrap();
    let root = tmp.path();
    std::fs::create_dir(root.join(".git")).unwrap();
    std::fs::write(root.join("AGENTS.md"), "ROOT_RULE_AT_STARTUP").unwrap();
    let workspace = root.join("crates/agent");
    std::fs::create_dir_all(&workspace).unwrap();
    std::fs::write(workspace.join("TEAM_GUIDE.md"), "LEAF_RULE_AT_STARTUP").unwrap();

    let mut config = minimal_config();
    config.project_instructions.project_doc_fallback_filenames = vec!["TEAM_GUIDE.md".into()];
    let systems = Arc::new(Mutex::new(Vec::new()));
    let provider = Arc::new(CapturingProvider {
        systems: Arc::clone(&systems),
    });
    let result = AgentBootstrap::new(config, workspace.to_string_lossy().as_ref(), null_output())
        .provider(provider)
        .build()
        .await
        .unwrap();

    std::fs::write(root.join("AGENTS.md"), "ROOT_RULE_AFTER_BOOTSTRAP").unwrap();
    std::fs::write(workspace.join("TEAM_GUIDE.md"), "LEAF_RULE_AFTER_BOOTSTRAP").unwrap();

    let mut engine = result.engine;
    engine
        .run("show active instructions", &workspace.to_string_lossy())
        .await
        .unwrap();

    let captured = systems.lock().unwrap();
    assert_eq!(captured.len(), 1);
    let system = &captured[0];
    let root_pos = system.find("ROOT_RULE_AT_STARTUP").unwrap();
    let leaf_pos = system.find("LEAF_RULE_AT_STARTUP").unwrap();
    assert!(root_pos < leaf_pos);
    assert!(!system.contains("ROOT_RULE_AFTER_BOOTSTRAP"));
    assert!(!system.contains("LEAF_RULE_AFTER_BOOTSTRAP"));
}

#[tokio::test]
async fn bootstrap_config_accessor_returns_config() {
    let config = minimal_config();
    let bootstrap = AgentBootstrap::new(config, "/tmp/ws", null_output());
    assert_eq!(bootstrap.config().model, "gpt-test-model");
    assert_eq!(bootstrap.config().max_tokens, 1024);
}

#[tokio::test]
async fn bootstrap_with_external_provider() {
    let config = minimal_config();
    let provider = nomi_providers::create_provider(&config);

    let result = AgentBootstrap::new(config, "/tmp/test-workspace", null_output())
        .provider(provider)
        .build()
        .await
        .unwrap();

    assert!(!result.engine.tool_names().is_empty());
}

use std::sync::Arc;

use nomi_agent::bootstrap::AgentBootstrap;
use nomi_agent::output::null_sink::NullSink;
use nomi_config::compat::ProviderCompat;
use nomi_config::config::{Config, ProviderType};

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

    for expected in &["Read", "Write", "Edit", "Bash", "Grep", "Glob"] {
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
    for denied in &["Bash", "Write", "Edit", "Spawn", "Skill"] {
        assert!(
            !names.iter().any(|n| n == denied),
            "白名单外的工具必须被过滤: {denied}"
        );
    }
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
    let workspace = tmp.path();
    std::fs::write(workspace.join("AGENTS.md"), "PROJECT_RULES_MARKER").unwrap();

    let config = minimal_config();
    let _result = AgentBootstrap::new(config, workspace.to_string_lossy().as_ref(), null_output())
        .build()
        .await
        .unwrap();
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

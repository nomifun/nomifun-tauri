//! Integration tests for AcpAgentManager.
//!
//! **Status: TEMPORARILY IGNORED** — These tests use mock shell scripts that
//! produce line-delimited JSON on stdout. After the ACP SDK integration
//! (replacing raw JSON-over-stdio with `agent-client-protocol` JSON-RPC),
//! `AcpAgentManager::new()` now performs an SDK `initialize` handshake that
//! mock shell scripts cannot respond to.
//!
//! To re-enable these tests, the mock scripts need to be replaced with a
//! minimal JSON-RPC responder that handles `initialize`, `session/new`,
//! `session/prompt`, and `session/update` notifications.
//!
//! Tests are serialized via `SERIAL_LOCK` to avoid OS-level resource
//! contention from parallel subprocess spawning (pipes, I/O scheduling).

// Pre-existing: serial() MutexGuard held across await points is intentional —
// it serializes test execution. Useless .into() is a pre-existing nit.
#![allow(clippy::await_holding_lock, clippy::useless_conversion)]

use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use nomifun_ai_agent::factory::acp_assembler::{WorkspaceInfo, assemble_acp_params};
use nomifun_ai_agent::manager::acp::AcpAgentManager;
use nomifun_ai_agent::registry::AgentRegistry;
use nomifun_ai_agent::{AgentInstance, AgentStreamEvent, IAgentTask};
use nomifun_common::ConversationStatus;
use nomifun_db::{SqliteAgentMetadataRepository, init_database_memory};
use tokio::sync::broadcast;

/// Timeout for receiving events from the relay.
const EVENT_TIMEOUT: Duration = Duration::from_secs(5);

/// Serialize integration tests to avoid OS-level resource contention
/// from parallel subprocess spawning (pipes, I/O scheduling).
static SERIAL_LOCK: Mutex<()> = Mutex::new(());

/// Acquire the serial lock (panics on poison).
fn serial() -> MutexGuard<'static, ()> {
    SERIAL_LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

/// Create an AcpAgentManager wrapping a mock shell script.
///
/// Returns the Arc-wrapped manager and a pre-subscribed event receiver
/// (subscribed BEFORE the relay starts, so no events are missed).
async fn make_mock_agent(script: &str, backend: &str) -> (Arc<AcpAgentManager>, broadcast::Receiver<AgentStreamEvent>) {
    let temp_dir = std::env::temp_dir();
    let script_path = temp_dir.join(format!(
        "mock_acp_{}_{}.sh",
        std::process::id(),
        nomifun_common::now_ms()
    ));
    std::fs::write(&script_path, format!("#!/bin/sh\n{script}")).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&script_path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    let config = nomifun_ai_agent::AcpBuildExtra {
        desktop_gateway: false,
        gateway_mcp_config: None,
        open_mcp_config: None,
        computer_mcp_config: None,
        browser_mcp_config: None,
        agent_id: None,
        backend: Some(backend.to_owned()),
        cli_path: Some(script_path.to_string_lossy().into_owned()),
        agent_name: None,
        custom_agent_id: None,
        preset_context: None,
        skills: vec![],
        preset_assistant_id: None,
        session_mode: None,
        current_model_id: None,
        cron_job_id: None,
        guide_mcp_config: None,
        requirement_mcp_config: None,
        knowledge_mcp_config: None,
        mcp_server_ids: None,
        session_mcp_servers: vec![],
        user_id: None,
        companion_id: None,
        channel_platform: None,
        knowledge_mounts: vec![],
        knowledge_writeback: false,
        knowledge_writeback_mode: None,
        knowledge_writeback_eagerness: None,
    };

    let tmp_skills = tempfile::TempDir::new().unwrap();
    let skill_paths = std::sync::Arc::new(nomifun_extension::resolve_skill_paths(
        tmp_skills.path(),
        tmp_skills.path(),
    ));
    let skill_manager = nomifun_ai_agent::AcpSkillManager::new(skill_paths);

    let db = init_database_memory().await.unwrap();
    let repo = Arc::new(SqliteAgentMetadataRepository::new(db.pool().clone()));
    let registry = AgentRegistry::new(repo);
    registry.hydrate().await.unwrap();

    let metadata = registry
        .find_builtin_by_backend(backend)
        .await
        .expect("seeded backend row must exist");
    let catalog_tx = registry.catalog_sender();

    let params = Arc::new(
        assemble_acp_params(
            "test-conv-1".into(),
            WorkspaceInfo {
                path: "/tmp".into(),
                is_custom: true,
            },
            metadata,
            nomifun_common::CommandSpec {
                command: script_path.into(),
                args: vec![],
                env: vec![],
                cwd: None,
            },
            config,
            Vec::new(),
            None,
            std::env::temp_dir(),
        )
        .await,
    );

    let (manager, _, _) = AcpAgentManager::build(params, skill_manager, &catalog_tx)
        .await
        .expect("Failed to spawn mock ACP agent");

    let arc = Arc::new(manager);

    // Subscribe to typed events BEFORE starting handler to capture all events
    let rx = arc.subscribe();
    arc.start_permission_handler();

    (arc, rx)
}

/// Wait until a specific event type is received, returning all collected events.
async fn wait_for_event(
    rx: &mut broadcast::Receiver<AgentStreamEvent>,
    predicate: impl Fn(&AgentStreamEvent) -> bool,
) -> Vec<AgentStreamEvent> {
    let mut events = Vec::new();
    loop {
        match tokio::time::timeout(EVENT_TIMEOUT, rx.recv()).await {
            Ok(Ok(event)) => {
                let matched = predicate(&event);
                events.push(event);
                if matched {
                    return events;
                }
            }
            Ok(Err(broadcast::error::RecvError::Closed)) => {
                panic!(
                    "Event channel closed before target event. Received: {:?}",
                    events.iter().map(event_type_name).collect::<Vec<_>>()
                )
            }
            Ok(Err(broadcast::error::RecvError::Lagged(n))) => {
                eprintln!("Warning: receiver lagged by {n} events");
                continue;
            }
            Err(_) => panic!(
                "Timed out waiting for target event. Received: {:?}",
                events.iter().map(event_type_name).collect::<Vec<_>>()
            ),
        }
    }
}

/// Get a short name for the event type (for debug output).
fn event_type_name(event: &AgentStreamEvent) -> &'static str {
    match event {
        AgentStreamEvent::Start(_) => "Start",
        AgentStreamEvent::Text(_) => "Text",
        AgentStreamEvent::Tips(_) => "Tips",
        AgentStreamEvent::ToolCall(_) => "ToolCall",
        AgentStreamEvent::ToolGroup(_) => "ToolGroup",
        AgentStreamEvent::AgentStatus(_) => "AgentStatus",
        AgentStreamEvent::Thinking(_) => "Thinking",
        AgentStreamEvent::Plan(_) => "Plan",
        AgentStreamEvent::Permission(_) => "Permission",
        AgentStreamEvent::AcpPermission(_) => "AcpPermission",
        AgentStreamEvent::AcpToolCall(_) => "AcpToolCall",
        AgentStreamEvent::AvailableCommands(_) => "AvailableCommands",
        AgentStreamEvent::SkillSuggest(_) => "SkillSuggest",
        AgentStreamEvent::CronTrigger(_) => "CronTrigger",
        AgentStreamEvent::AcpModelInfo(_) => "AcpModelInfo",
        AgentStreamEvent::AcpModeInfo(_) => "AcpModeInfo",
        AgentStreamEvent::AcpConfigOption(_) => "AcpConfigOption",
        AgentStreamEvent::AcpSessionInfo(_) => "AcpSessionInfo",
        AgentStreamEvent::AcpContextUsage(_) => "AcpContextUsage",
        AgentStreamEvent::AcpPromptHookWarning(_) => "AcpPromptHookWarning",
        AgentStreamEvent::TurnCompleted(_) => "TurnCompleted",
        AgentStreamEvent::Finish(_) => "Finish",
        AgentStreamEvent::Error(_) => "Error",
        AgentStreamEvent::System(_) => "System",
        AgentStreamEvent::RequestTrace(_) => "RequestTrace",
        AgentStreamEvent::SlashCommandsUpdated(_) => "SlashCommandsUpdated",
        AgentStreamEvent::SessionAssigned(_) => "SessionAssigned",
    }
}

#[test]
fn acp_build_extra_populates_skills_from_extra_json() {
    let json = serde_json::json!({
        "backend": "claude",
        "skills": ["cron", "pdf"],
    });
    let extra: nomifun_ai_agent::AcpBuildExtra = serde_json::from_value(json).unwrap();
    assert_eq!(extra.skills, vec!["cron".to_owned(), "pdf".to_owned()]);
}

// -- Tests --
// All tests below are #[ignore] because make_mock_agent() spawns shell scripts
// that cannot respond to the SDK's JSON-RPC `initialize` handshake.

#[tokio::test]
#[ignore = "requires JSON-RPC mock agent"]
async fn acp_agent_type_is_acp() {
    let _guard = serial();
    let (agent, _rx) = make_mock_agent(r#"echo '{"type":"finish","data":{}}'"#, "claude").await;

    assert_eq!(agent.agent_type(), nomifun_common::AgentType::Acp);
    assert_eq!(agent.conversation_id(), "test-conv-1");
    assert_eq!(agent.workspace(), "/tmp");
    assert_eq!(agent.backend(), Some("claude"));
}

#[tokio::test]
#[ignore = "requires JSON-RPC mock agent"]
async fn acp_agent_receives_stream_events() {
    let _guard = serial();
    let (_agent, mut rx) = make_mock_agent(
        r#"echo '{"type":"start","data":{"session_id":"sess-1"}}' && echo '{"type":"text","data":{"content":"Hello"}}' && echo '{"type":"finish","data":{"session_id":"sess-1"}}'"#,
        "claude",
    )
    .await;

    // Wait for finish event, collecting all events along the way
    let events = wait_for_event(&mut rx, |e| matches!(e, AgentStreamEvent::Finish(_))).await;

    assert!(events.len() >= 2, "Expected at least 2 events, got {}", events.len());

    let has_start = events.iter().any(|e| matches!(e, AgentStreamEvent::Start(_)));
    let has_text = events.iter().any(|e| matches!(e, AgentStreamEvent::Text(_)));

    assert!(has_start, "Should have received Start event");
    assert!(has_text, "Should have received Text event");
}

#[tokio::test]
#[ignore = "requires JSON-RPC mock agent"]
async fn acp_agent_session_id_captured_from_start() {
    let _guard = serial();
    let (agent, mut rx) = make_mock_agent(
        r#"echo '{"type":"start","data":{"session_id":"sess-abc"}}' && sleep 1"#,
        "claude",
    )
    .await;

    wait_for_event(&mut rx, |e| matches!(e, AgentStreamEvent::Start(_))).await;

    let session_id = agent.session_id().await;
    assert_eq!(session_id, Some("sess-abc".into()));

    agent.kill(None).unwrap();
}

#[tokio::test]
#[ignore = "requires JSON-RPC mock agent"]
async fn acp_agent_status_transitions() {
    let _guard = serial();
    let (agent, mut rx) = make_mock_agent(
        r#"sleep 0.1 && echo '{"type":"start","data":{}}' && sleep 0.3 && echo '{"type":"finish","data":{}}'"#,
        "claude",
    )
    .await;

    // Initial status: None
    assert_eq!(agent.status(), None);

    // Wait for Start event
    wait_for_event(&mut rx, |e| matches!(e, AgentStreamEvent::Start(_))).await;
    assert_eq!(agent.status(), Some(ConversationStatus::Running));

    // Wait for Finish event
    wait_for_event(&mut rx, |e| matches!(e, AgentStreamEvent::Finish(_))).await;
    assert_eq!(agent.status(), Some(ConversationStatus::Finished));
}

#[tokio::test]
#[ignore = "requires JSON-RPC mock agent"]
async fn acp_agent_error_event_sets_finished() {
    let _guard = serial();
    let (agent, mut rx) = make_mock_agent(
        r#"echo '{"type":"start","data":{}}' && sleep 0.1 && echo '{"type":"error","data":{"message":"timeout"}}'"#,
        "claude",
    )
    .await;

    wait_for_event(&mut rx, |e| matches!(e, AgentStreamEvent::Error(_))).await;
    assert_eq!(agent.status(), Some(ConversationStatus::Finished));
}

#[tokio::test]
#[ignore = "requires JSON-RPC mock agent"]
async fn acp_agent_model_info_captured() {
    let _guard = serial();
    let (agent, mut rx) = make_mock_agent(
        r#"echo '{"type":"acp_model_info","data":{"current_model_id":"claude-sonnet-4","current_model_label":"Claude Sonnet 4","available_models":[{"id":"claude-sonnet-4","label":"Claude Sonnet 4"},{"id":"claude-opus-4","label":"Claude Opus 4"}],"can_switch":true,"source":"models","source_detail":"acp-models"}}' && sleep 0.5"#,
        "claude",
    )
    .await;

    wait_for_event(&mut rx, |e| matches!(e, AgentStreamEvent::AcpModelInfo(_))).await;

    // Route through the public `AgentInstance` API rather than reaching
    // into the private `AcpAgentManager::model()`: the ai-agent crate only
    // exposes `AgentInstance` to downstream callers, so tests should
    // exercise the same surface.
    let instance = AgentInstance::Acp(agent.clone());
    let resp = instance.get_model().await.expect("get_model should succeed");
    let info = resp.model_info.expect("Model info should be captured");
    assert_eq!(info.current_model_id.as_deref(), Some("claude-sonnet-4"));
    assert_eq!(info.available_models.len(), 2);
    assert_eq!(info.available_models[0].label, "Claude Sonnet 4");

    agent.kill(None).unwrap();
}

#[tokio::test]
#[ignore = "requires JSON-RPC mock agent"]
async fn acp_agent_kill_terminates_process() {
    let _guard = serial();
    let (agent, _rx) = make_mock_agent(r#"trap '' TERM; while true; do sleep 1; done"#, "claude").await;

    assert!(agent.last_activity_at() > 0);

    agent.kill(Some(nomifun_common::AgentKillReason::IdleTimeout)).unwrap();

    tokio::time::sleep(Duration::from_millis(1000)).await;
}

#[tokio::test]
#[ignore = "requires JSON-RPC mock agent"]
async fn acp_agent_last_activity_updates() {
    let _guard = serial();
    let (agent, _rx) = make_mock_agent(r#"sleep 10"#, "claude").await;

    let initial = agent.last_activity_at();
    assert!(initial > 0);

    let now = nomifun_common::now_ms();
    assert!(now - initial < 5000, "Last activity should be recent");

    agent.kill(None).unwrap();
}

#[tokio::test]
#[ignore = "requires JSON-RPC mock agent"]
async fn acp_agent_text_content_received() {
    let _guard = serial();
    let (_agent, mut rx) = make_mock_agent(
        r#"echo '{"type":"text","data":{"content":"Hello from ACP"}}'"#,
        "claude",
    )
    .await;

    match tokio::time::timeout(EVENT_TIMEOUT, rx.recv()).await {
        Ok(Ok(AgentStreamEvent::Text(data))) => {
            assert_eq!(data.content, "Hello from ACP");
        }
        other => panic!("Expected Text event, got {:?}", other),
    }
}

#[tokio::test]
#[ignore = "requires JSON-RPC mock agent"]
async fn acp_agent_agent_status_event_captures_session() {
    let _guard = serial();
    let (agent, mut rx) = make_mock_agent(
        r#"echo '{"type":"agent_status","data":{"backend":"claude","status":"running","session_id":"sess-xyz"}}' && sleep 1"#,
        "claude",
    )
    .await;

    wait_for_event(&mut rx, |e| matches!(e, AgentStreamEvent::AgentStatus(_))).await;

    let session = agent.session_id().await;
    assert_eq!(session, Some("sess-xyz".into()));

    agent.kill(None).unwrap();
}

#[tokio::test]
#[ignore = "requires JSON-RPC mock agent"]
async fn acp_agent_multiple_event_types() {
    let _guard = serial();
    let (_agent, mut rx) = make_mock_agent(
        r#"echo '{"type":"start","data":{"session_id":"sess-multi"}}' && echo '{"type":"thinking","data":{"content":"Analyzing...","subject":"code","duration":100,"status":"in_progress"}}' && echo '{"type":"text","data":{"content":"Result"}}' && echo '{"type":"finish","data":{"session_id":"sess-multi"}}'"#,
        "claude",
    )
    .await;

    let events = wait_for_event(&mut rx, |e| matches!(e, AgentStreamEvent::Finish(_))).await;

    assert!(events.len() >= 4, "Expected 4+ events, got {}", events.len());

    assert!(matches!(&events[0], AgentStreamEvent::Start(d) if d.session_id == Some("sess-multi".into())));
    assert!(matches!(&events[1], AgentStreamEvent::Thinking(d) if d.content == "Analyzing..."));
    assert!(matches!(&events[2], AgentStreamEvent::Text(d) if d.content == "Result"));
    assert!(matches!(&events[3], AgentStreamEvent::Finish(d) if d.session_id == Some("sess-multi".into())));
}

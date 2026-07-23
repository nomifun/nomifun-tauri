//! Integration tests for the ACP prompt pipeline.
//!
//! Unlike acp_agent_integration.rs, these tests do not exercise
//! AcpAgentManager or the JSON-RPC protocol. They construct a
//! PromptPipeline with the two built-in hooks and invoke
//! pre_send against a real PromptCtx, asserting the observable
//! prompt transformation.

use std::collections::HashMap;
use std::sync::Arc;

use nomifun_ai_agent::capability::prompt_pipeline::{PromptCtx, PromptPipeline};
use nomifun_ai_agent::factory::acp_assembler::{AcpSessionParams, WorkspaceInfo, assemble_acp_params};
use nomifun_ai_agent::manager::acp::{
    AcpSession, KnowledgeContextHook, ModelIdentityReminderHook, SessionNewPreludeHook,
};
use nomifun_ai_agent::registry::AgentRegistry;
use nomifun_ai_agent::session::ModelId;
use nomifun_ai_agent::{AcpBuildExtra, AcpSkillManager};
use nomifun_db::{SqliteAgentMetadataRepository, init_database_memory};

// ── Fixtures ──────────────────────────────────────────────────────────────────

const CLAUDE_AGENT_ID: &str = "0190f5fe-7c00-7a00-8000-000000000101";
const CODEX_AGENT_ID: &str = "0190f5fe-7c00-7a00-8000-000000000102";

fn baseline_agent_id(backend: &str) -> &'static str {
    match backend {
        "claude" => CLAUDE_AGENT_ID,
        "codex" => CODEX_AGENT_ID,
        other => panic!("unsupported ACP fixture backend: {other}"),
    }
}

async fn fixture_params(
    backend: &str,
    preset_context: Option<&str>,
    is_custom_workspace: bool,
) -> Arc<AcpSessionParams> {
    let db = init_database_memory().await.unwrap();
    let repo = Arc::new(SqliteAgentMetadataRepository::new(db.pool().clone()));
    let registry = AgentRegistry::new(repo);
    registry.hydrate().await.unwrap();

    let metadata = registry
        .get(baseline_agent_id(backend))
        .await
        .expect("seeded agent row must exist");

    let config = AcpBuildExtra {
        gateway_mcp_config: None,
        gateway_excluded_tools: Vec::new(),
        open_mcp_config: None,
        computer_mcp_config: None,
        browser_mcp_config: None,
        agent_id: Some(baseline_agent_id(backend).to_owned()),
        backend: Some(backend.to_owned()),
        cli_path: None,
        agent_name: None,
        custom_agent_id: None,
        preset_context: preset_context.map(str::to_owned),
        skills: vec![],
        preset_id: None,
        session_mode: None,
        current_model_id: None,
        cron_job_id: None,
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

    Arc::new(
        assemble_acp_params(
            "conv-pp-test".into(),
            WorkspaceInfo {
                path: "/tmp".into(),
                is_custom: is_custom_workspace,
            },
            metadata,
            nomifun_common::CommandSpec {
                command: "/usr/bin/true".into(),
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
    )
}

/// Like [`fixture_params`] but with a single mounted knowledge base, so the
/// assembled params carry a non-empty `knowledge_context`. preset_context is
/// `None` and workspace custom — the knowledge section is independent of both.
async fn fixture_params_with_knowledge(backend: &str) -> Arc<AcpSessionParams> {
    let db = init_database_memory().await.unwrap();
    let repo = Arc::new(SqliteAgentMetadataRepository::new(db.pool().clone()));
    let registry = AgentRegistry::new(repo);
    registry.hydrate().await.unwrap();
    let metadata = registry
        .get(baseline_agent_id(backend))
        .await
        .expect("seeded agent row must exist");

    let config = AcpBuildExtra {
        agent_id: Some(baseline_agent_id(backend).to_owned()),
        backend: Some(backend.to_owned()),
        knowledge_mounts: vec![nomifun_api_types::KnowledgeMountInfo {
            knowledge_base_id: nomifun_common::KnowledgeBaseId::new(),
            name: "领域知识".into(),
            description: "团队约定".into(),
            rel_path: ".nomi/knowledge/领域知识".into(),
            toc: vec!["concepts/术语.md — 术语表".into()],
            summary: Some("Covers domain terms.".into()),
            live_sources: vec![],
        }],
        ..Default::default()
    };

    Arc::new(
        assemble_acp_params(
            "conv-pp-test".into(),
            WorkspaceInfo {
                path: "/tmp".into(),
                is_custom: true,
            },
            metadata,
            nomifun_common::CommandSpec {
                command: "/usr/bin/true".into(),
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
    )
}

fn fixture_skill_manager() -> Arc<AcpSkillManager> {
    let tmp = tempfile::TempDir::new().unwrap();
    let paths = Arc::new(nomifun_extension::resolve_skill_paths(tmp.path(), tmp.path()));
    // tmp dir needs to live until the test finishes.
    // mem::forget is acceptable in test code — we just don't need the Drop cleanup.
    std::mem::forget(tmp);
    AcpSkillManager::new(paths)
}

fn make_pipeline() -> PromptPipeline {
    // Mirror the real registration order in AcpAgentManager::new.
    PromptPipeline::new(vec![
        Arc::new(KnowledgeContextHook),
        Arc::new(SessionNewPreludeHook),
        Arc::new(ModelIdentityReminderHook),
    ])
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[tokio::test(flavor = "current_thread")]
async fn empty_pipeline_is_identity() {
    let params = fixture_params("claude", None, true).await;
    let skill_manager = fixture_skill_manager();
    let mut session = AcpSession::new(None, None, HashMap::new());
    let pipeline = PromptPipeline::new(vec![]);
    let mut ctx = PromptCtx {
        session: &mut session,
        params: &params,
        skill_manager: &skill_manager,
    };

    let out = pipeline.pre_send(&mut ctx, "hello".into()).await;
    assert_eq!(out, "hello");
}

/// First prompt after session/new: prelude block injected, flag consumed.
#[tokio::test(flavor = "current_thread")]
async fn brand_new_first_prompt_injects_preset_context() {
    let params = fixture_params("claude", Some("Rule A"), true).await;
    let skill_manager = fixture_skill_manager();
    let mut session = AcpSession::new(None, None, HashMap::new());

    // Simulate: open_session_new just succeeded.
    session.mark_pending_session_new_prelude();

    let pipeline = make_pipeline();

    let mut ctx = PromptCtx {
        session: &mut session,
        params: &params,
        skill_manager: &skill_manager,
    };

    let out = pipeline.pre_send(&mut ctx, "hello".into()).await;
    assert!(out.contains("[Assistant Rules]"), "prelude block missing: {out}");
    assert!(out.contains("Rule A"), "preset_context missing: {out}");
    assert!(out.ends_with("hello"), "user content should be at the end: {out}");

    // Flag must have been consumed.
    assert!(
        !session.take_pending_session_new_prelude(),
        "pending_session_new_prelude must be false after pre_send consumed it"
    );
}

/// Second prompt: no prelude, no reminder — pure passthrough.
#[tokio::test(flavor = "current_thread")]
async fn second_prompt_is_passthrough() {
    let params = fixture_params("claude", Some("Rule A"), true).await;
    let skill_manager = fixture_skill_manager();
    let mut session = AcpSession::new(None, None, HashMap::new());
    session.mark_pending_session_new_prelude();

    let pipeline = make_pipeline();

    // First prompt consumes the flag.
    {
        let mut ctx = PromptCtx {
            session: &mut session,
            params: &params,
            skill_manager: &skill_manager,
            };
        let _ = pipeline.pre_send(&mut ctx, "first".into()).await;
    }

    // Second prompt: flag already consumed.
    let mut ctx = PromptCtx {
        session: &mut session,
        params: &params,
        skill_manager: &skill_manager,
    };
    let out = pipeline.pre_send(&mut ctx, "second".into()).await;
    assert_eq!(out, "second", "no prelude / no reminder expected on second turn");
}

/// Resume path: no mark_pending_session_new_prelude — prompt must be unchanged.
#[tokio::test(flavor = "current_thread")]
async fn resume_path_does_not_inject() {
    let params = fixture_params("claude", Some("Rule A"), true).await;
    let skill_manager = fixture_skill_manager();

    // Resume: session opened by open_session_resume which does NOT call
    // mark_pending_session_new_prelude. The flag stays false.
    let mut session = AcpSession::new(None, None, HashMap::new());

    let pipeline = make_pipeline();

    let mut ctx = PromptCtx {
        session: &mut session,
        params: &params,
        skill_manager: &skill_manager,
    };

    let out = pipeline.pre_send(&mut ctx, "continue the story".into()).await;
    assert_eq!(out, "continue the story");
}

/// L2 fix: the knowledge retrieval-protocol section is delivered on a RESUMED
/// session (open_session_resume marks pending_knowledge_prelude) even though the
/// new-session [Assistant Rules] prelude is NOT. This is what makes a
/// resumed/restarted ACP session — or one rebuilt after a 挂载知识库 change —
/// actually trigger retrieval.
#[tokio::test(flavor = "current_thread")]
async fn knowledge_section_delivered_on_resume() {
    let params = fixture_params_with_knowledge("claude").await;
    let skill_manager = fixture_skill_manager();
    let mut session = AcpSession::new(None, None, HashMap::new());

    // Resume: open_session_resume marks the knowledge prelude but NOT the
    // new-session prelude.
    session.mark_pending_knowledge_prelude();

    let pipeline = make_pipeline();
    let mut ctx = PromptCtx {
        session: &mut session,
        params: &params,
        skill_manager: &skill_manager,
    };

    let out = pipeline.pre_send(&mut ctx, "用领域知识回答".into()).await;
    assert!(out.contains("[Knowledge Bases]"), "knowledge block missing: {out}");
    assert!(
        out.contains("## Knowledge bases (extended knowledge source)"),
        "retrieval protocol section missing: {out}"
    );
    assert!(out.contains("Retrieval protocol"), "retrieval protocol missing: {out}");
    assert!(out.contains("领域知识"), "mounted base name missing: {out}");
    // The new-session rules prelude must NOT appear on a resumed session.
    assert!(!out.contains("[Assistant Rules]"), "rules prelude must not inject on resume: {out}");
    assert!(out.ends_with("用领域知识回答"), "user content must survive at the end: {out}");

    // Flag consumed — a later turn in the same session is a passthrough.
    let mut ctx2 = PromptCtx {
        session: &mut session,
        params: &params,
        skill_manager: &skill_manager,
    };
    let out2 = pipeline.pre_send(&mut ctx2, "再问一句".into()).await;
    assert_eq!(out2, "再问一句", "knowledge section must be one-shot per session open");
}

/// Without the pending_knowledge_prelude flag the section is not injected, even
/// when bases are mounted (e.g. an ordinary mid-session turn).
#[tokio::test(flavor = "current_thread")]
async fn knowledge_section_skipped_without_flag() {
    let params = fixture_params_with_knowledge("claude").await;
    let skill_manager = fixture_skill_manager();
    let mut session = AcpSession::new(None, None, HashMap::new());

    let pipeline = make_pipeline();
    let mut ctx = PromptCtx {
        session: &mut session,
        params: &params,
        skill_manager: &skill_manager,
    };

    let out = pipeline.pre_send(&mut ctx, "hello".into()).await;
    assert_eq!(out, "hello", "no knowledge flag → passthrough");
}

/// Pending model notice: reminder prepended, then drained so second call is clean.
#[tokio::test(flavor = "current_thread")]
async fn pending_model_notice_triggers_reminder_prepend() {
    let params = fixture_params("claude", None, true).await;
    let skill_manager = fixture_skill_manager();
    let mut session = AcpSession::new(None, None, HashMap::new());

    // Simulate set_model reconciled successfully and stuck the notice.
    session.set_pending_model_notice(ModelId::new("claude-opus-4"));

    let pipeline = make_pipeline();

    let mut ctx = PromptCtx {
        session: &mut session,
        params: &params,
        skill_manager: &skill_manager,
    };

    let out = pipeline.pre_send(&mut ctx, "go".into()).await;
    assert!(out.contains("<system-reminder>"), "reminder missing: {out}");
    assert!(out.ends_with("go"), "user content must survive at the end: {out}");

    // Second call: notice already drained — no reminder.
    let mut ctx2 = PromptCtx {
        session: &mut session,
        params: &params,
        skill_manager: &skill_manager,
    };
    let out2 = pipeline.pre_send(&mut ctx2, "next".into()).await;
    assert_eq!(out2, "next");
}

/// Both flags set: reminder (outermost) wraps the prelude block.
#[tokio::test(flavor = "current_thread")]
async fn both_flags_prepend_reminder_outermost() {
    let params = fixture_params("claude", Some("Rule A"), true).await;
    let skill_manager = fixture_skill_manager();
    let mut session = AcpSession::new(None, None, HashMap::new());
    session.mark_pending_session_new_prelude();
    session.set_pending_model_notice(ModelId::new("claude-opus-4"));

    let pipeline = make_pipeline();

    let mut ctx = PromptCtx {
        session: &mut session,
        params: &params,
        skill_manager: &skill_manager,
    };

    let out = pipeline.pre_send(&mut ctx, "hi".into()).await;
    let reminder_idx = out.find("<system-reminder>").expect("reminder must be present");
    let rules_idx = out.find("[Assistant Rules]").expect("rules block must be present");
    assert!(
        reminder_idx < rules_idx,
        "reminder must sit outside (before) the assistant rules block:\n{out}"
    );
    assert!(out.ends_with("hi"));
}

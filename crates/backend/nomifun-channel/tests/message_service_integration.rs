use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use nomifun_ai_agent::agent_task::{AgentInstance, IAgentTask};
use nomifun_ai_agent::protocol::events::FinishEventData;
use nomifun_ai_agent::types::{BuildTaskOptions, SendMessageData};
use nomifun_ai_agent::{AgentSendError, AgentStreamEvent, IMockAgent, IWorkerTaskManager};
use nomifun_api_types::WebSocketMessage;
use nomifun_channel::channel_settings::ChannelSettingsService;
use nomifun_channel::message_service::ChannelMessageService;
use nomifun_channel::types::PluginType;
use nomifun_common::{AgentKillReason, AgentType, AppError, ConversationStatus, TimestampMs};
use nomifun_conversation::ConversationService;
use nomifun_conversation::skill_resolver::{ResolvedAgentSkill, SkillResolver};
use nomifun_db::models::AssistantSessionRow;
use nomifun_db::{
    SqliteAcpSessionRepository, SqliteAgentMetadataRepository, SqliteChannelRepository,
    SqliteClientPreferenceRepository, SqliteConversationRepository, init_database_memory,
};
use nomifun_realtime::EventBroadcaster;
use tokio::sync::broadcast;

struct TestBroadcaster {
    events: Mutex<Vec<WebSocketMessage<serde_json::Value>>>,
}

impl TestBroadcaster {
    fn new() -> Self {
        Self {
            events: Mutex::new(Vec::new()),
        }
    }
}

impl EventBroadcaster for TestBroadcaster {
    fn broadcast(&self, event: WebSocketMessage<serde_json::Value>) {
        self.events.lock().unwrap().push(event);
    }
}

struct NoopSkillResolver;

#[async_trait]
impl SkillResolver for NoopSkillResolver {
    async fn auto_inject_names(&self) -> Vec<String> {
        Vec::new()
    }

    async fn resolve_skills(&self, _names: &[String]) -> Vec<ResolvedAgentSkill> {
        Vec::new()
    }

    async fn link_workspace_skills(
        &self,
        _workspace: &std::path::Path,
        _rel_dirs: &[&str],
        _skills: &[ResolvedAgentSkill],
    ) -> usize {
        0
    }
}

struct ScriptedAgent {
    conversation_id: String,
    event_tx: broadcast::Sender<AgentStreamEvent>,
}

impl ScriptedAgent {
    fn new(conversation_id: &str) -> Self {
        let (event_tx, _) = broadcast::channel(16);
        Self {
            conversation_id: conversation_id.to_owned(),
            event_tx,
        }
    }
}

#[async_trait]
impl IAgentTask for ScriptedAgent {
    fn agent_type(&self) -> AgentType {
        AgentType::Nomi
    }

    fn conversation_id(&self) -> &str {
        &self.conversation_id
    }

    fn workspace(&self) -> &str {
        "/tmp/nomifun-channel-test"
    }

    fn status(&self) -> Option<ConversationStatus> {
        Some(ConversationStatus::Finished)
    }

    fn last_activity_at(&self) -> TimestampMs {
        0
    }

    fn subscribe(&self) -> broadcast::Receiver<AgentStreamEvent> {
        self.event_tx.subscribe()
    }

    async fn send_message(&self, _data: SendMessageData) -> Result<(), AgentSendError> {
        let _ = self.event_tx.send(AgentStreamEvent::Finish(FinishEventData::default()));
        Ok(())
    }

    async fn cancel(&self) -> Result<(), AppError> {
        Ok(())
    }

    fn kill(&self, _reason: Option<AgentKillReason>) -> Result<(), AppError> {
        Ok(())
    }
}

impl IMockAgent for ScriptedAgent {}

struct RecordingTaskManager {
    agents: Mutex<std::collections::HashMap<String, AgentInstance>>,
}

impl RecordingTaskManager {
    fn new() -> Self {
        Self {
            agents: Mutex::new(std::collections::HashMap::new()),
        }
    }
}

#[async_trait]
impl IWorkerTaskManager for RecordingTaskManager {
    fn get_task(&self, conversation_id: &str) -> Option<AgentInstance> {
        self.agents.lock().unwrap().get(conversation_id).cloned()
    }

    async fn get_or_build_task(
        &self,
        conversation_id: &str,
        _options: BuildTaskOptions,
    ) -> Result<AgentInstance, AppError> {
        let mut agents = self.agents.lock().unwrap();
        if let Some(agent) = agents.get(conversation_id) {
            return Ok(agent.clone());
        }

        let agent = AgentInstance::Mock(Arc::new(ScriptedAgent::new(conversation_id)));
        agents.insert(conversation_id.to_owned(), agent.clone());
        Ok(agent)
    }

    fn kill(&self, conversation_id: &str, _reason: Option<AgentKillReason>) -> Result<(), AppError> {
        self.agents.lock().unwrap().remove(conversation_id);
        Ok(())
    }

    fn kill_and_wait(
        &self,
        conversation_id: &str,
        reason: Option<AgentKillReason>,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>> {
        let _ = self.kill(conversation_id, reason);
        Box::pin(std::future::ready(()))
    }

    fn clear(&self) {
        self.agents.lock().unwrap().clear();
    }

    fn active_count(&self) -> usize {
        self.agents.lock().unwrap().len()
    }

    fn collect_idle(&self, _idle_threshold_ms: TimestampMs) -> Vec<String> {
        Vec::new()
    }
}

#[tokio::test]
async fn send_to_agent_warms_cold_task_before_returning_stream_subscription() {
    let db = init_database_memory().await.unwrap();
    let pool = db.pool().clone();

    let task_manager: Arc<dyn IWorkerTaskManager> = Arc::new(RecordingTaskManager::new());
    let conversation_svc = Arc::new(ConversationService::new(
        std::env::temp_dir(),
        Arc::new(TestBroadcaster::new()),
        Arc::new(NoopSkillResolver),
        Arc::clone(&task_manager),
        Arc::new(SqliteConversationRepository::new(pool.clone())),
        Arc::new(SqliteAgentMetadataRepository::new(pool.clone())),
        Arc::new(SqliteAcpSessionRepository::new(pool.clone())),
    ));

    let settings = Arc::new(ChannelSettingsService::new(Arc::new(
        SqliteClientPreferenceRepository::new(pool.clone()),
    )));
    let message_svc = ChannelMessageService::new(
        conversation_svc,
        Arc::clone(&task_manager),
        settings,
        Arc::new(SqliteChannelRepository::new(pool)),
        "system_default_user".to_owned(),
    );

    let session = AssistantSessionRow {
        id: "session-1".to_owned(),
        user_id: "channel-user-1".to_owned(),
        agent_type: "nomi".to_owned(),
        conversation_id: None,
        workspace: None,
        chat_id: Some("7088048016".to_owned()),
        channel_id: None,
        created_at: 1,
        last_activity: 1,
    };

    for platform in [
        PluginType::Telegram,
        PluginType::Lark,
        PluginType::Dingtalk,
        PluginType::Weixin,
    ] {
        let result = message_svc.send_to_agent(&session, "hello", platform).await.unwrap();

        assert!(
            result.stream_rx.is_some(),
            "channel relay must have an agent stream receiver after cold start for {platform:?}"
        );
        assert!(task_manager.get_task(&result.conversation_id).is_some());
    }
}

// ── Fix 3/4 support: last_user_text + is_conversation_busy ──────────────

struct TestStack {
    conversation_svc: Arc<ConversationService>,
    message_svc: ChannelMessageService,
    runtime: Arc<nomifun_conversation::runtime_state::ConversationRuntimeStateService>,
    channel_repo: Arc<SqliteChannelRepository>,
}

fn build_stack(pool: nomifun_db::SqlitePool) -> TestStack {
    let task_manager: Arc<dyn IWorkerTaskManager> = Arc::new(RecordingTaskManager::new());
    let runtime = Arc::new(nomifun_conversation::runtime_state::ConversationRuntimeStateService::default());
    let conversation_svc = Arc::new(
        ConversationService::new(
            std::env::temp_dir(),
            Arc::new(TestBroadcaster::new()),
            Arc::new(NoopSkillResolver),
            Arc::clone(&task_manager),
            Arc::new(SqliteConversationRepository::new(pool.clone())),
            Arc::new(SqliteAgentMetadataRepository::new(pool.clone())),
            Arc::new(SqliteAcpSessionRepository::new(pool.clone())),
        )
        .with_runtime_state(Arc::clone(&runtime)),
    );

    let settings = Arc::new(ChannelSettingsService::new(Arc::new(
        SqliteClientPreferenceRepository::new(pool.clone()),
    )));
    let channel_repo = Arc::new(SqliteChannelRepository::new(pool));
    let message_svc = ChannelMessageService::new(
        Arc::clone(&conversation_svc),
        Arc::clone(&task_manager),
        settings,
        channel_repo.clone(),
        "system_default_user".to_owned(),
    );

    TestStack {
        conversation_svc,
        message_svc,
        runtime,
        channel_repo,
    }
}

fn make_session(conversation_id: Option<i64>) -> AssistantSessionRow {
    AssistantSessionRow {
        id: "session-1".to_owned(),
        user_id: "channel-user-1".to_owned(),
        agent_type: "nomi".to_owned(),
        conversation_id,
        workspace: None,
        chat_id: Some("7088048016".to_owned()),
        channel_id: None,
        created_at: 1,
        last_activity: 1,
    }
}

/// Waits for the background turn spawned by `send_message` to release its
/// runtime claim so the next send doesn't hit the turn-conflict guard.
async fn wait_until_idle(svc: &Arc<ConversationService>, conversation_id: &str) {
    use nomifun_api_types::ConversationRuntimeStateKind;
    for _ in 0..500 {
        let summary = svc.runtime_summary_for(conversation_id).await;
        if summary.state == ConversationRuntimeStateKind::Idle {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    panic!("conversation {conversation_id} never became idle");
}

#[tokio::test]
async fn last_user_text_returns_latest_user_prompt() {
    let db = init_database_memory().await.unwrap();
    let stack = build_stack(db.pool().clone());

    // First prompt creates the conversation; second one is the newest.
    let session = make_session(None);
    let first = stack
        .message_svc
        .send_to_agent(&session, "first prompt", PluginType::Telegram)
        .await
        .unwrap();
    wait_until_idle(&stack.conversation_svc, &first.conversation_id).await;

    // SendResult.conversation_id is a String (Option A); the session FK is i64.
    let bound_session = make_session(Some(first.conversation_id.parse::<i64>().unwrap()));
    stack
        .message_svc
        .send_to_agent(&bound_session, "second prompt", PluginType::Telegram)
        .await
        .unwrap();
    wait_until_idle(&stack.conversation_svc, &first.conversation_id).await;

    let text = stack.message_svc.last_user_text(&first.conversation_id).await.unwrap();
    assert_eq!(text.as_deref(), Some("second prompt"));
}

#[tokio::test]
async fn last_user_text_none_for_unknown_conversation() {
    let db = init_database_memory().await.unwrap();
    let stack = build_stack(db.pool().clone());

    // Unknown conversation maps to a lookup error, not a silent None.
    let result = stack.message_svc.last_user_text("missing-conv").await;
    assert!(result.is_err());
}

#[tokio::test]
async fn is_conversation_busy_reflects_turn_claim() {
    let db = init_database_memory().await.unwrap();
    let stack = build_stack(db.pool().clone());

    let session = make_session(None);
    let sent = stack
        .message_svc
        .send_to_agent(&session, "hello", PluginType::Telegram)
        .await
        .unwrap();
    wait_until_idle(&stack.conversation_svc, &sent.conversation_id).await;

    assert!(!stack.message_svc.is_conversation_busy(&sent.conversation_id).await);

    // Claiming the turn is exactly what send_message does while a prompt is
    // in flight → the channel guard must report busy.
    let _claim = stack.runtime.try_claim_turn(&sent.conversation_id).unwrap();
    assert!(stack.message_svc.is_conversation_busy(&sent.conversation_id).await);

    drop(_claim);
    assert!(!stack.message_svc.is_conversation_busy(&sent.conversation_id).await);
}

// ── Channel companion binding resolution + single-session routing ──────────────

/// Profile stub: maps each companion id to a pre-seeded single-session
/// conversation id (what `CompanionManager.create` would return in production),
/// records every `ensure_companion_session` call, and uses `companion_y` as the
/// legacy/default per-platform fallback. An empty `sessions` map models a
/// companion with no chat model configured (ensure returns `None`).
struct StubProfile {
    sessions: std::collections::HashMap<String, i64>,
    calls: Mutex<Vec<String>>,
}

impl StubProfile {
    fn new(sessions: std::collections::HashMap<String, i64>) -> Self {
        Self {
            sessions,
            calls: Mutex::new(Vec::new()),
        }
    }
}

#[async_trait]
impl nomifun_channel::message_service::MasterAgentProfile for StubProfile {
    async fn companion_model(&self, _companion_id: &str) -> Option<nomifun_common::ProviderWithModel> {
        None
    }
    async fn master_companion_id(&self, _platform: &str) -> Option<String> {
        Some("companion_y".to_owned())
    }
    async fn companion_exists(&self, _companion_id: &str) -> bool {
        true
    }
    async fn ensure_companion_session(&self, companion_id: &str) -> Option<i64> {
        self.calls.lock().unwrap().push(companion_id.to_owned());
        self.sessions.get(companion_id).copied()
    }
}

/// Seed a companion's single-session conversation (the row `CompanionManager`
/// would own), returning its i64 id.
async fn seed_companion_session(svc: &Arc<ConversationService>, companion_id: &str) -> i64 {
    let req = nomifun_api_types::CreateConversationRequest {
        r#type: AgentType::Nomi,
        name: Some(format!("和 {companion_id} 聊天")),
        model: Some(nomifun_common::ProviderWithModel {
            provider_id: "p".to_owned(),
            model: "m".to_owned(),
            use_model: Some("m".to_owned()),
        }),
        source: None,
        channel_chat_id: None,
        extra: serde_json::json!({ "companionSession": true, "companionId": companion_id }),
    };
    svc.create("system_default_user", req).await.unwrap().id
}

async fn bind_channel_to_companion(repo: &Arc<SqliteChannelRepository>, channel_id: &str, companion_id: &str) {
    use nomifun_db::IChannelRepository;
    let now = nomifun_common::now_ms();
    repo.upsert_plugin(&nomifun_db::models::ChannelPluginRow {
        id: channel_id.to_owned(),
        r#type: "telegram".to_owned(),
        name: "Telegram Bot".to_owned(),
        enabled: true,
        config: "enc".to_owned(),
        status: None,
        last_connected: None,
        companion_id: Some(companion_id.to_owned()),
        public_agent_id: None,
        bot_key: Some("42".to_owned()),
        created_at: now,
        updated_at: now,
    })
    .await
    .unwrap();
}

/// The channel row's own companion binding wins over the profile fallback, and
/// either way the turn is routed INTO that companion's single session (not a
/// freshly-minted channel-master conversation).
#[tokio::test]
async fn channel_companion_turn_routes_into_companion_single_session() {
    let db = init_database_memory().await.unwrap();
    let stack = build_stack(db.pool().clone());

    let conv_x = seed_companion_session(&stack.conversation_svc, "companion_x").await;
    let conv_y = seed_companion_session(&stack.conversation_svc, "companion_y").await;
    let sessions = std::collections::HashMap::from([
        ("companion_x".to_owned(), conv_x),
        ("companion_y".to_owned(), conv_y),
    ]);
    let message_svc = stack.message_svc.with_master_profile(Arc::new(StubProfile::new(sessions)));

    bind_channel_to_companion(&stack.channel_repo, "achn_test", "companion_x").await;

    // Bound channel → channel companion (companion_x) wins; the turn runs on
    // companion_x's single session conversation, NOT a new channel conversation.
    let mut bound = make_session(None);
    bound.channel_id = Some("achn_test".to_owned());
    let sent = message_svc.send_to_agent(&bound, "hi", PluginType::Telegram).await.unwrap();
    assert_eq!(sent.conversation_id, conv_x.to_string());
    wait_until_idle(&stack.conversation_svc, &sent.conversation_id).await;

    // No channel binding → profile fallback companion (companion_y) → its session.
    let mut unbound = make_session(None);
    unbound.id = "session-2".to_owned();
    unbound.chat_id = Some("other-chat".to_owned());
    let sent = message_svc.send_to_agent(&unbound, "hi", PluginType::Telegram).await.unwrap();
    assert_eq!(sent.conversation_id, conv_y.to_string());
}

/// Two different IM chats bound to the SAME companion both land in that
/// companion's ONE session — the unification guarantee. No standalone
/// channel-master conversation is created for either.
#[tokio::test]
async fn companion_im_turns_share_one_session() {
    let db = init_database_memory().await.unwrap();
    let stack = build_stack(db.pool().clone());

    let conv_x = seed_companion_session(&stack.conversation_svc, "companion_x").await;
    let sessions = std::collections::HashMap::from([("companion_x".to_owned(), conv_x)]);
    let message_svc = stack.message_svc.with_master_profile(Arc::new(StubProfile::new(sessions)));
    bind_channel_to_companion(&stack.channel_repo, "achn_test", "companion_x").await;

    let mut chat_a = make_session(None);
    chat_a.channel_id = Some("achn_test".to_owned());
    chat_a.chat_id = Some("chat-A".to_owned());
    let a = message_svc.send_to_agent(&chat_a, "hi from A", PluginType::Telegram).await.unwrap();
    wait_until_idle(&stack.conversation_svc, &a.conversation_id).await;

    let mut chat_b = make_session(None);
    chat_b.id = "session-b".to_owned();
    chat_b.channel_id = Some("achn_test".to_owned());
    chat_b.chat_id = Some("chat-B".to_owned());
    let b = message_svc.send_to_agent(&chat_b, "hi from B", PluginType::Telegram).await.unwrap();

    assert_eq!(a.conversation_id, conv_x.to_string());
    assert_eq!(b.conversation_id, conv_x.to_string(), "both IM chats must share the companion's single session");
}

/// A companion with no chat model (ensure returns None) refuses the turn with a
/// distinct error instead of silently minting a leaking standalone conversation.
#[tokio::test]
async fn companion_without_model_refuses_turn() {
    use nomifun_channel::error::ChannelError;

    let db = init_database_memory().await.unwrap();
    let stack = build_stack(db.pool().clone());
    // Empty sessions map → ensure_companion_session returns None for every companion.
    let message_svc = stack
        .message_svc
        .with_master_profile(Arc::new(StubProfile::new(std::collections::HashMap::new())));
    bind_channel_to_companion(&stack.channel_repo, "achn_test", "companion_x").await;

    let mut bound = make_session(None);
    bound.channel_id = Some("achn_test".to_owned());
    let err = message_svc
        .send_to_agent(&bound, "hi", PluginType::Telegram)
        .await
        .expect_err("a model-less companion must refuse the turn");
    assert!(matches!(err, ChannelError::CompanionNotReady(_)));
}

/// Binds a bot channel row to a 对外伙伴 (public agent) — the per-bot binding the
/// dispatch reads via `session.channel_id` → `get_plugin` → `row.public_agent_id`.
async fn bind_channel_to_public_agent(repo: &Arc<SqliteChannelRepository>, channel_id: &str, public_agent_id: &str) {
    use nomifun_db::IChannelRepository;
    let now = nomifun_common::now_ms();
    repo.upsert_plugin(&nomifun_db::models::ChannelPluginRow {
        id: channel_id.to_owned(),
        r#type: "telegram".to_owned(),
        name: "Telegram Bot".to_owned(),
        enabled: true,
        config: "enc".to_owned(),
        status: None,
        last_connected: None,
        companion_id: None,
        public_agent_id: Some(public_agent_id.to_owned()),
        bot_key: Some("43".to_owned()),
        created_at: now,
        updated_at: now,
    })
    .await
    .unwrap();
}

// ── 对外伙伴 / public-agent channel routing ─────────────────────────────────

/// Profile stub for public-agent routing: `servable` is what
/// `public_agent_servable` returns (true ⇒ alive + enabled), and `model` is the
/// agent's answering model. Companion methods are inert (a public-agent bot must
/// never touch the companion path).
struct PublicStubProfile {
    servable: bool,
    model: Option<nomifun_common::ProviderWithModel>,
}

#[async_trait]
impl nomifun_channel::message_service::MasterAgentProfile for PublicStubProfile {
    async fn companion_model(&self, _companion_id: &str) -> Option<nomifun_common::ProviderWithModel> {
        None
    }
    async fn master_companion_id(&self, _platform: &str) -> Option<String> {
        // If the public-agent path ever fell through to the companion path, this
        // would host the turn — the tests assert that never happens.
        Some("companion_should_not_be_used".to_owned())
    }
    async fn companion_exists(&self, _companion_id: &str) -> bool {
        true
    }
    async fn ensure_companion_session(&self, _companion_id: &str) -> Option<i64> {
        panic!("public-agent bot must NOT route into a companion session");
    }
    async fn public_agent_servable(&self, _id: &str) -> bool {
        self.servable
    }
    async fn public_agent_model(&self, _id: &str) -> Option<nomifun_common::ProviderWithModel> {
        self.model.clone()
    }
}

/// (a) A public-agent-bound bot's turn builds an ISOLATED per-chat nomi
/// conversation carrying `public_agent_id` + `channelPlatform` + the public
/// agent's model, and NO `companionId` / `desktopGateway` (no gateway for public
/// agents). The companion path is never taken.
#[tokio::test]
async fn public_agent_bound_platform_builds_clamped_session() {
    let db = init_database_memory().await.unwrap();
    let stack = build_stack(db.pool().clone());

    // The routing reads the BOT ROW's public_agent_id (per-bot), via channel_id.
    bind_channel_to_public_agent(&stack.channel_repo, "achn_pa", "pubagent_1").await;

    let model = nomifun_common::ProviderWithModel {
        provider_id: "prov_pa".to_owned(),
        model: "pa-model".to_owned(),
        use_model: Some("pa-model-v1".to_owned()),
    };
    let message_svc = stack.message_svc.with_master_profile(Arc::new(PublicStubProfile {
        servable: true,
        model: Some(model),
    }));

    let mut session = make_session(None);
    session.channel_id = Some("achn_pa".to_owned());
    let sent = message_svc
        .send_to_agent(&session, "你好", PluginType::Telegram)
        .await
        .unwrap();
    wait_until_idle(&stack.conversation_svc, &sent.conversation_id).await;

    let conv = stack
        .conversation_svc
        .get("system_default_user", &sent.conversation_id)
        .await
        .unwrap();

    assert_eq!(conv.r#type, AgentType::Nomi);
    assert_eq!(conv.extra["public_agent_id"], serde_json::json!("pubagent_1"));
    assert_eq!(conv.extra["channelPlatform"], serde_json::json!("telegram"));
    assert!(
        conv.extra.get("companionId").is_none(),
        "public agent must not carry a companion"
    );
    assert!(
        conv.extra.get("desktopGateway").is_none(),
        "public agent gets NO gateway"
    );
    let m = conv.model.expect("public-agent conversation must carry a model");
    assert_eq!(m.provider_id, "prov_pa");
    assert_eq!(m.use_model.as_deref(), Some("pa-model-v1"));
}

/// A public-agent-bound bot whose agent is disabled/missing refuses with a
/// friendly notice — it MUST NOT fall through to the companion path (the stub's
/// `ensure_companion_session` panics if it does).
#[tokio::test]
async fn public_agent_bound_but_disabled_refuses_without_companion_fallthrough() {
    use nomifun_channel::error::ChannelError;

    let db = init_database_memory().await.unwrap();
    let stack = build_stack(db.pool().clone());

    bind_channel_to_public_agent(&stack.channel_repo, "achn_pa", "pubagent_1").await;

    // servable = false models a disabled/deleted agent.
    let message_svc = stack.message_svc.with_master_profile(Arc::new(PublicStubProfile {
        servable: false,
        model: None,
    }));

    let mut session = make_session(None);
    session.channel_id = Some("achn_pa".to_owned());
    let err = message_svc
        .send_to_agent(&session, "你好", PluginType::Telegram)
        .await
        .expect_err("a disabled public agent must refuse the turn");
    assert!(matches!(err, ChannelError::CompanionNotReady(_)));
}

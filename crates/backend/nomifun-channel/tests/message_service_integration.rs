use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use nomifun_ai_agent::runtime_handle::{AgentRuntimeHandle, AgentRuntimeControl};
use nomifun_ai_agent::protocol::events::FinishEventData;
use nomifun_ai_agent::types::{AgentRuntimeBuildOptions, SendMessageData};
use nomifun_ai_agent::{AgentSendError, AgentStreamEvent, MockAgentRuntime, AgentRuntimeRegistry};
use nomifun_api_types::WebSocketMessage;
use nomifun_channel::channel_settings::ChannelSettingsService;
use nomifun_channel::message_service::ChannelMessageService;
use nomifun_channel::types::PluginType;
use nomifun_common::{AgentKillReason, AgentType, AppError, ConversationStatus, TimestampMs};
use nomifun_conversation::ConversationService;
use nomifun_conversation::skill_resolver::{ResolvedAgentSkill, SkillResolver};
use nomifun_db::models::{ChannelSessionRow, NewChannelPluginRow};
use nomifun_db::{
    CreateProviderParams, IClientPreferenceRepository, IProviderRepository, SqliteAcpSessionRepository,
    SqliteAgentMetadataRepository, SqliteChannelRepository, SqliteClientPreferenceRepository,
    SqliteConversationRepository, SqliteProviderRepository, init_database_memory,
};
use nomifun_realtime::UserEventSink;
use tokio::sync::broadcast;

const DEFAULT_PROVIDER: &str = "018f1234-5678-7abc-8def-012345678940";
const COMPANION_PROVIDER: &str = "018f1234-5678-7abc-8def-012345678941";
const PUBLIC_AGENT_PROVIDER: &str = "018f1234-5678-7abc-8def-012345678942";
const SESSION_A: &str = "018f1234-5678-7abc-8def-012345678943";
const SESSION_B: &str = "018f1234-5678-7abc-8def-012345678944";
const CHANNEL_USER_ID: &str = "018f1234-5678-7abc-8def-012345678945";
const COMPANION_X: &str = "018f1234-5678-7abc-8def-012345678948";
const COMPANION_Y: &str = "018f1234-5678-7abc-8def-012345678949";
const UNUSED_COMPANION: &str = "018f1234-5678-7abc-8def-01234567894a";
const PUBLIC_AGENT: &str = "018f1234-5678-7abc-8def-01234567894b";

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

impl UserEventSink for TestBroadcaster {
    fn send_to_user(&self, _user_id: &str, event: WebSocketMessage<serde_json::Value>) {
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
impl AgentRuntimeControl for ScriptedAgent {
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

    fn is_transport_healthy(&self) -> bool {
        true
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

impl MockAgentRuntime for ScriptedAgent {}

struct RecordingAgentRuntimeRegistry {
    agents: Mutex<std::collections::HashMap<String, AgentRuntimeHandle>>,
}

impl RecordingAgentRuntimeRegistry {
    fn new() -> Self {
        Self {
            agents: Mutex::new(std::collections::HashMap::new()),
        }
    }
}

#[async_trait]
impl AgentRuntimeRegistry for RecordingAgentRuntimeRegistry {
    fn get_runtime(&self, conversation_id: &str) -> Option<AgentRuntimeHandle> {
        self.agents.lock().unwrap().get(conversation_id).cloned()
    }

    async fn get_or_create_runtime(
        &self,
        conversation_id: &str,
        _options: AgentRuntimeBuildOptions,
    ) -> Result<AgentRuntimeHandle, AppError> {
        let mut agents = self.agents.lock().unwrap();
        if let Some(agent) = agents.get(conversation_id) {
            return Ok(agent.clone());
        }

        let agent = AgentRuntimeHandle::Mock(Arc::new(ScriptedAgent::new(conversation_id)));
        agents.insert(conversation_id.to_owned(), agent.clone());
        Ok(agent)
    }

    fn terminate(&self, conversation_id: &str, _reason: Option<AgentKillReason>) -> Result<(), AppError> {
        self.agents.lock().unwrap().remove(conversation_id);
        Ok(())
    }

    fn terminate_and_wait(
        &self,
        conversation_id: &str,
        reason: Option<AgentKillReason>,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>> {
        let _ = self.terminate(conversation_id, reason);
        Box::pin(std::future::ready(()))
    }

    fn terminate_all(&self) {
        self.agents.lock().unwrap().clear();
    }

    fn active_runtime_count(&self) -> usize {
        self.agents.lock().unwrap().len()
    }

    fn collect_idle_runtimes(&self, _idle_threshold_ms: TimestampMs) -> Vec<String> {
        Vec::new()
    }
}

/// Seed every provider id used by this integration fixture and give each
/// platform a valid default model. The database deliberately rejects dangling
/// Conversation model authorities, so tests must model a real provider catalog.
async fn seed_channel_models(pool: &nomifun_db::SqlitePool) {
    let providers = SqliteProviderRepository::new(pool.clone());
    for id in [DEFAULT_PROVIDER, COMPANION_PROVIDER, PUBLIC_AGENT_PROVIDER] {
        providers
            .create(CreateProviderParams {
                provider_id: Some(id),
                platform: "openai",
                name: "Channel test provider",
                base_url: "https://example.invalid/v1",
                api_key_encrypted: "test-only",
                models: r#"["channel-test-model","m","pa-model-v1"]"#,
                enabled: true,
                capabilities: "[]",
                model_context_limits: None,
                model_protocols: None,
                model_descriptions: None,
                model_enabled: None,
                model_health: None,
                bedrock_config: None,
                is_full_url: false,
                sort_order: None,
            })
            .await
            .unwrap();
    }

    let preferences = SqliteClientPreferenceRepository::new(pool.clone());
    let model =
        format!(r#"{{"provider_id":"{DEFAULT_PROVIDER}","model":"channel-test-model"}}"#);
    preferences
        .upsert_batch(&[
            ("channels.telegram.defaultModel", model.as_str()),
            ("channels.lark.defaultModel", model.as_str()),
            ("channels.dingtalk.defaultModel", model.as_str()),
            ("channels.weixin.defaultModel", model.as_str()),
        ])
        .await
        .unwrap();
}

#[tokio::test]
async fn send_to_agent_warms_cold_task_before_returning_stream_subscription() {
    let db = init_database_memory().await.unwrap();
    let installation_owner = nomifun_db::installation_owner_id(db.pool()).await.unwrap();
    let pool = db.pool().clone();
    seed_channel_models(&pool).await;

    let runtime_registry: Arc<dyn AgentRuntimeRegistry> = Arc::new(RecordingAgentRuntimeRegistry::new());
    let conversation_svc = Arc::new(ConversationService::new(
        Arc::<str>::from(installation_owner.as_str()),
        std::env::temp_dir(),
        Arc::new(TestBroadcaster::new()),
        Arc::new(NoopSkillResolver),
        Arc::clone(&runtime_registry),
        Arc::new(SqliteConversationRepository::new(pool.clone())),
        Arc::new(SqliteAgentMetadataRepository::new(pool.clone())),
        Arc::new(SqliteAcpSessionRepository::new(pool.clone())),
        Arc::new(nomifun_conversation::NoExecutionConversationBoundary),
    ));

    let settings = Arc::new(ChannelSettingsService::new(Arc::new(
        SqliteClientPreferenceRepository::new(pool.clone()),
    )));
    let message_svc = ChannelMessageService::new(
        conversation_svc,
        Arc::clone(&runtime_registry),
        settings,
        Arc::new(SqliteChannelRepository::new(pool)),
        installation_owner,
    );

    let session = ChannelSessionRow {
        channel_session_id: SESSION_A.to_owned(),
        channel_user_id: CHANNEL_USER_ID.to_owned(),
        agent_type: "nomi".to_owned(),
        conversation_id: None,
        workspace: None,
        chat_id: Some("7088048016".to_owned()),
        channel_plugin_id: None,
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
        assert!(runtime_registry.get_runtime(&result.conversation_id).is_some());
    }
}

// ── Fix 3/4 support: last_user_text + is_conversation_busy ──────────────

struct TestStack {
    conversation_svc: Arc<ConversationService>,
    message_svc: ChannelMessageService,
    runtime: Arc<nomifun_conversation::runtime_state::ConversationRuntimeStateService>,
    channel_repo: Arc<SqliteChannelRepository>,
    installation_owner: String,
}

async fn build_stack(pool: nomifun_db::SqlitePool) -> TestStack {
    let installation_owner = nomifun_db::installation_owner_id(&pool).await.unwrap();
    seed_channel_models(&pool).await;
    let runtime_registry: Arc<dyn AgentRuntimeRegistry> = Arc::new(RecordingAgentRuntimeRegistry::new());
    let runtime = Arc::new(nomifun_conversation::runtime_state::ConversationRuntimeStateService::default());
    let conversation_svc = Arc::new(
        ConversationService::new(
            Arc::<str>::from(installation_owner.as_str()),
            std::env::temp_dir(),
            Arc::new(TestBroadcaster::new()),
            Arc::new(NoopSkillResolver),
            Arc::clone(&runtime_registry),
            Arc::new(SqliteConversationRepository::new(pool.clone())),
            Arc::new(SqliteAgentMetadataRepository::new(pool.clone())),
            Arc::new(SqliteAcpSessionRepository::new(pool.clone())),
            Arc::new(nomifun_conversation::NoExecutionConversationBoundary),
        )
        .with_runtime_state(Arc::clone(&runtime)),
    );

    let settings = Arc::new(ChannelSettingsService::new(Arc::new(
        SqliteClientPreferenceRepository::new(pool.clone()),
    )));
    let channel_repo = Arc::new(SqliteChannelRepository::new(pool));
    let message_svc = ChannelMessageService::new(
        Arc::clone(&conversation_svc),
        Arc::clone(&runtime_registry),
        settings,
        channel_repo.clone(),
        installation_owner.clone(),
    );

    TestStack {
        conversation_svc,
        message_svc,
        runtime,
        channel_repo,
        installation_owner,
    }
}

fn make_session(conversation_id: Option<String>) -> ChannelSessionRow {
    ChannelSessionRow {
        channel_session_id: SESSION_A.to_owned(),
        channel_user_id: CHANNEL_USER_ID.to_owned(),
        agent_type: "nomi".to_owned(),
        conversation_id,
        workspace: None,
        chat_id: Some("7088048016".to_owned()),
        channel_plugin_id: None,
        created_at: 1,
        last_activity: 1,
    }
}

/// Waits for the background turn spawned by `send_message` to release its
/// Agent turn handle so the next send doesn't hit the turn-conflict guard.
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
    let stack = build_stack(db.pool().clone()).await;

    // First prompt creates the conversation; second one is the newest.
    let session = make_session(None);
    let first = stack
        .message_svc
        .send_to_agent(&session, "first prompt", PluginType::Telegram)
        .await
        .unwrap();
    wait_until_idle(&stack.conversation_svc, &first.conversation_id).await;

    let bound_session = make_session(Some(first.conversation_id.clone()));
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
    let stack = build_stack(db.pool().clone()).await;

    // Unknown conversation maps to a lookup error, not a silent None.
    let missing = nomifun_common::ConversationId::new();
    let result = stack.message_svc.last_user_text(missing.as_str()).await;
    assert!(result.is_err());
}

#[tokio::test]
async fn is_conversation_busy_reflects_active_turn_handle() {
    let db = init_database_memory().await.unwrap();
    let stack = build_stack(db.pool().clone()).await;

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
    let _turn_handle = stack.runtime.try_acquire_turn(&sent.conversation_id).unwrap();
    assert!(stack.message_svc.is_conversation_busy(&sent.conversation_id).await);

    drop(_turn_handle);
    assert!(!stack.message_svc.is_conversation_busy(&sent.conversation_id).await);
}

// ── Channel companion binding resolution + single-session routing ──────────────

/// Profile stub: maps each companion id to a pre-seeded single-session
/// conversation id (what `CompanionManager.create` would return in production),
/// records every `ensure_companion_session` call, and uses `companion_y` as the
/// per-platform binding. An empty `sessions` map models a
/// companion with no chat model configured (ensure returns `None`).
struct StubProfile {
    sessions: std::collections::HashMap<String, String>,
    calls: Mutex<Vec<String>>,
}

impl StubProfile {
    fn new(sessions: std::collections::HashMap<String, String>) -> Self {
        Self {
            sessions,
            calls: Mutex::new(Vec::new()),
        }
    }
}

#[async_trait]
impl nomifun_channel::message_service::ChannelAgentProfile for StubProfile {
    async fn companion_model(&self, _companion_id: &str) -> Option<nomifun_common::ProviderWithModel> {
        None
    }
    async fn channel_companion_id(&self, _platform: &str) -> Option<String> {
        Some(COMPANION_Y.to_owned())
    }
    async fn companion_exists(&self, _companion_id: &str) -> bool {
        true
    }
    async fn ensure_companion_session(&self, companion_id: &str) -> Option<String> {
        self.calls.lock().unwrap().push(companion_id.to_owned());
        self.sessions.get(companion_id).cloned()
    }
}

/// Seed a companion's single-session conversation (the row `CompanionManager`
/// would own), returning its canonical conversation ID.
async fn seed_companion_session(
    svc: &Arc<ConversationService>,
    installation_owner: &str,
    companion_id: &str,
) -> String {
    let req = nomifun_api_types::CreateConversationRequest {
        r#type: AgentType::Nomi,
        name: Some(format!("和 {companion_id} 聊天")),
        model: Some(nomifun_common::ProviderWithModel {
            provider_id: COMPANION_PROVIDER.to_owned(),
            model: "m".to_owned(),
            use_model: Some("m".to_owned()),
        }),
        source: None,
        channel_chat_id: None,
        preset_id: None,
        preset_overrides: None,
        delegation_policy: Default::default(),
        execution_model_pool: None,
        decision_policy: Default::default(),
        execution_template_id: None,
        extra: serde_json::json!({ "companion_session": true, "companion_id": companion_id }),
    };
    svc.create(installation_owner, req).await.unwrap().conversation_id
}

async fn bind_channel_to_companion(
    repo: &Arc<SqliteChannelRepository>,
    companion_id: &str,
) -> String {
    use nomifun_db::IChannelRepository;
    let now = nomifun_common::now_ms();
    repo.create_plugin(&NewChannelPluginRow {
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
    .unwrap()
    .channel_plugin_id
}

/// The channel row's own companion binding wins over the profile fallback, and
/// either way the turn is routed INTO that companion's single session (not a
/// freshly-minted channel conversation).
#[tokio::test]
async fn channel_companion_turn_routes_into_companion_single_session() {
    let db = init_database_memory().await.unwrap();
    let stack = build_stack(db.pool().clone()).await;

    let conv_x = seed_companion_session(
        &stack.conversation_svc,
        &stack.installation_owner,
        COMPANION_X,
    )
    .await;
    let conv_y = seed_companion_session(
        &stack.conversation_svc,
        &stack.installation_owner,
        COMPANION_Y,
    )
    .await;
    let sessions = std::collections::HashMap::from([
        (COMPANION_X.to_owned(), conv_x.clone()),
        (COMPANION_Y.to_owned(), conv_y.clone()),
    ]);
    let message_svc = stack
        .message_svc
        .with_channel_agent_profile(Arc::new(StubProfile::new(sessions)));

    let channel_plugin_id = bind_channel_to_companion(&stack.channel_repo, COMPANION_X).await;

    // Bound channel → channel companion (companion_x) wins; the turn runs on
    // companion_x's single session conversation, NOT a new channel conversation.
    let mut bound = make_session(None);
    bound.channel_plugin_id = Some(channel_plugin_id);
    let sent = message_svc.send_to_agent(&bound, "hi", PluginType::Telegram).await.unwrap();
    assert_eq!(sent.conversation_id, conv_x);
    wait_until_idle(&stack.conversation_svc, &sent.conversation_id).await;

    // No channel binding → profile fallback companion (companion_y) → its session.
    let mut unbound = make_session(None);
    unbound.channel_session_id = SESSION_B.to_owned();
    unbound.chat_id = Some("other-chat".to_owned());
    let sent = message_svc.send_to_agent(&unbound, "hi", PluginType::Telegram).await.unwrap();
    assert_eq!(sent.conversation_id, conv_y);
}

/// Two different IM chats bound to the SAME companion both land in that
/// companion's ONE session — the unification guarantee. No separate channel
/// conversation is created for either.
#[tokio::test]
async fn companion_im_turns_share_one_session() {
    let db = init_database_memory().await.unwrap();
    let stack = build_stack(db.pool().clone()).await;

    let conv_x = seed_companion_session(
        &stack.conversation_svc,
        &stack.installation_owner,
        COMPANION_X,
    )
    .await;
    let sessions =
        std::collections::HashMap::from([(COMPANION_X.to_owned(), conv_x.clone())]);
    let message_svc = stack
        .message_svc
        .with_channel_agent_profile(Arc::new(StubProfile::new(sessions)));
    let channel_plugin_id = bind_channel_to_companion(&stack.channel_repo, COMPANION_X).await;

    let mut chat_a = make_session(None);
    chat_a.channel_plugin_id = Some(channel_plugin_id.clone());
    chat_a.chat_id = Some("chat-A".to_owned());
    let a = message_svc.send_to_agent(&chat_a, "hi from A", PluginType::Telegram).await.unwrap();
    wait_until_idle(&stack.conversation_svc, &a.conversation_id).await;

    let mut chat_b = make_session(None);
    chat_b.channel_session_id = SESSION_B.to_owned();
    chat_b.channel_plugin_id = Some(channel_plugin_id);
    chat_b.chat_id = Some("chat-B".to_owned());
    let b = message_svc.send_to_agent(&chat_b, "hi from B", PluginType::Telegram).await.unwrap();

    assert_eq!(a.conversation_id, conv_x);
    assert_eq!(b.conversation_id, conv_x, "both IM chats must share the companion's single session");
}

/// A companion with no chat model (ensure returns None) refuses the turn with a
/// distinct error instead of silently minting a separate channel conversation.
#[tokio::test]
async fn companion_without_model_refuses_turn() {
    use nomifun_channel::error::ChannelError;

    let db = init_database_memory().await.unwrap();
    let stack = build_stack(db.pool().clone()).await;
    // Empty sessions map → ensure_companion_session returns None for every companion.
    let message_svc = stack
        .message_svc
        .with_channel_agent_profile(Arc::new(StubProfile::new(std::collections::HashMap::new())));
    let channel_plugin_id = bind_channel_to_companion(&stack.channel_repo, COMPANION_X).await;

    let mut bound = make_session(None);
    bound.channel_plugin_id = Some(channel_plugin_id);
    let err = message_svc
        .send_to_agent(&bound, "hi", PluginType::Telegram)
        .await
        .expect_err("a model-less companion must refuse the turn");
    assert!(matches!(err, ChannelError::CompanionNotReady(_)));
}

/// Binds a bot channel row to a 对外伙伴 (public agent) — the per-bot binding the
/// dispatch reads via `session.channel_id` → `get_plugin` → `row.public_agent_id`.
async fn bind_channel_to_public_agent(
    repo: &Arc<SqliteChannelRepository>,
    public_agent_id: &str,
) -> String {
    use nomifun_db::IChannelRepository;
    let now = nomifun_common::now_ms();
    repo.create_plugin(&NewChannelPluginRow {
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
    .unwrap()
    .channel_plugin_id
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
impl nomifun_channel::message_service::ChannelAgentProfile for PublicStubProfile {
    async fn companion_model(&self, _companion_id: &str) -> Option<nomifun_common::ProviderWithModel> {
        None
    }
    async fn channel_companion_id(&self, _platform: &str) -> Option<String> {
        // If the public-agent path ever fell through to the companion path, this
        // would host the turn — the tests assert that never happens.
        Some(UNUSED_COMPANION.to_owned())
    }
    async fn companion_exists(&self, _companion_id: &str) -> bool {
        true
    }
    async fn ensure_companion_session(&self, _companion_id: &str) -> Option<String> {
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
/// conversation carrying `public_agent_id` + `channel_platform` + the public
/// agent's model, and no `companion_id`. The factory applies the public capability
/// clamp; the companion path is never taken.
#[tokio::test]
async fn public_agent_bound_platform_builds_clamped_session() {
    let db = init_database_memory().await.unwrap();
    let stack = build_stack(db.pool().clone()).await;

    // The routing reads the BOT ROW's public_agent_id (per-bot), via channel_id.
    let channel_plugin_id = bind_channel_to_public_agent(&stack.channel_repo, PUBLIC_AGENT).await;

    let model = nomifun_common::ProviderWithModel {
        provider_id: PUBLIC_AGENT_PROVIDER.to_owned(),
        model: "pa-model".to_owned(),
        use_model: Some("pa-model-v1".to_owned()),
    };
    let message_svc = stack
        .message_svc
        .with_channel_agent_profile(Arc::new(PublicStubProfile {
            servable: true,
            model: Some(model),
        }));

    let mut session = make_session(None);
    session.channel_plugin_id = Some(channel_plugin_id);
    let sent = message_svc
        .send_to_agent(&session, "你好", PluginType::Telegram)
        .await
        .unwrap();
    wait_until_idle(&stack.conversation_svc, &sent.conversation_id).await;

    let conv = stack
        .conversation_svc
        .get(&stack.installation_owner, &sent.conversation_id)
        .await
        .unwrap();

    assert_eq!(conv.r#type, AgentType::Nomi);
    assert_eq!(conv.extra["public_agent_id"], serde_json::json!(PUBLIC_AGENT));
    assert_eq!(conv.extra["channel_platform"], serde_json::json!("telegram"));
    assert!(
        conv.extra.get("companion_id").is_none(),
        "public agent must not carry a companion"
    );
    let m = conv.model.expect("public-agent conversation must carry a model");
    assert_eq!(m.provider_id, PUBLIC_AGENT_PROVIDER);
    assert_eq!(m.use_model.as_deref(), Some("pa-model-v1"));
}

/// A public-agent-bound bot whose agent is disabled/missing refuses with a
/// friendly notice — it MUST NOT fall through to the companion path (the stub's
/// `ensure_companion_session` panics if it does).
#[tokio::test]
async fn public_agent_bound_but_disabled_refuses_without_companion_fallthrough() {
    use nomifun_channel::error::ChannelError;

    let db = init_database_memory().await.unwrap();
    let stack = build_stack(db.pool().clone()).await;

    let channel_plugin_id = bind_channel_to_public_agent(&stack.channel_repo, PUBLIC_AGENT).await;

    // servable = false models a disabled/deleted agent.
    let message_svc = stack
        .message_svc
        .with_channel_agent_profile(Arc::new(PublicStubProfile {
            servable: false,
            model: None,
        }));

    let mut session = make_session(None);
    session.channel_plugin_id = Some(channel_plugin_id);
    let err = message_svc
        .send_to_agent(&session, "你好", PluginType::Telegram)
        .await
        .expect_err("a disabled public agent must refuse the turn");
    assert!(matches!(err, ChannelError::CompanionNotReady(_)));
}

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use nomifun_ai_agent::agent_task::{AgentInstance, IAgentTask};
use nomifun_ai_agent::protocol::events::FinishEventData;
use nomifun_ai_agent::types::{BuildTaskOptions, SendMessageData};
use nomifun_ai_agent::{AgentSendError, AgentStreamEvent, IMockAgent, IWorkerTaskManager};
use nomifun_api_types::{ConversationRuntimeStateKind, ListMessagesQuery, WebSocketMessage};
use nomifun_channel::action::{ActionExecutor, MessageResult};
use nomifun_channel::channel_settings::ChannelSettingsService;
use nomifun_channel::message_service::ChannelMessageService;
use nomifun_channel::orchestrator::ChannelOrchestrator;
use nomifun_channel::pairing::PairingService;
use nomifun_channel::session::SessionManager;
use nomifun_channel::stream_relay::{ChannelSender, MessageRecorder};
use nomifun_channel::types::{
    ActionCategory, ActionContext, ChannelIncoming, MessageContentType, PluginType, UnifiedAction,
    UnifiedIncomingMessage, UnifiedMessageContent, UnifiedOutgoingMessage, UnifiedUser,
};
use nomifun_common::{
    AgentKillReason, AgentType, AppError, ConversationStatus, MessagePosition, TimestampMs, now_ms,
};
use nomifun_conversation::ConversationService;
use nomifun_conversation::runtime_state::ConversationRuntimeStateService;
use nomifun_conversation::skill_resolver::{ResolvedAgentSkill, SkillResolver};
use nomifun_db::models::{AssistantUserRow, ChannelPluginRow};
use nomifun_db::{
    IChannelRepository, SqliteAcpSessionRepository, SqliteAgentMetadataRepository, SqliteChannelRepository,
    SqliteClientPreferenceRepository, SqliteConversationRepository,
};
use nomifun_realtime::EventBroadcaster;
use tokio::sync::{broadcast, mpsc};

/// The channel row id every test message arrives through.
const TEST_CHANNEL: &str = "tg-1";

/// Stamps a platform message with the test channel id, the way the
/// manager's per-instance forwarder does in production.
fn incoming(message: UnifiedIncomingMessage) -> ChannelIncoming {
    ChannelIncoming {
        channel_id: TEST_CHANNEL.into(),
        message,
    }
}

fn make_text_message(user_id: &str, chat_id: &str, text: &str) -> UnifiedIncomingMessage {
    UnifiedIncomingMessage {
        id: "msg-1".into(),
        platform: PluginType::Telegram,
        chat_id: chat_id.into(),
        user: UnifiedUser {
            id: user_id.into(),
            username: None,
            display_name: "Test".into(),
            avatar_url: None,
        },
        content: UnifiedMessageContent {
            content_type: MessageContentType::Text,
            text: text.into(),
            attachments: None,
        },
        timestamp: 0,
        reply_to_message_id: None,
        action: None,
        raw: None,
    }
}

fn make_chat_action_message(user_id: &str, chat_id: &str, action_name: &str) -> UnifiedIncomingMessage {
    UnifiedIncomingMessage {
        id: "msg-action".into(),
        platform: PluginType::Telegram,
        chat_id: chat_id.into(),
        user: UnifiedUser {
            id: user_id.into(),
            username: None,
            display_name: "Test".into(),
            avatar_url: None,
        },
        content: UnifiedMessageContent {
            content_type: MessageContentType::Action,
            text: String::new(),
            attachments: None,
        },
        timestamp: 0,
        reply_to_message_id: None,
        action: Some(UnifiedAction {
            action: action_name.into(),
            category: ActionCategory::Chat,
            params: None,
            context: ActionContext {
                platform: PluginType::Telegram,
                user_id: user_id.into(),
                chat_id: chat_id.into(),
                message_id: None,
                session_id: None,
            },
        }),
        raw: None,
    }
}

/// Unauthorized user should receive a pairing code response.
#[tokio::test]
async fn unauthorized_user_gets_pairing_response() {
    let db = nomifun_db::init_database_memory().await.unwrap();
    let pool = db.pool().clone();
    let repo: Arc<dyn nomifun_db::IChannelRepository> =
        Arc::new(nomifun_db::SqliteChannelRepository::new(pool.clone()));
    let bus = Arc::new(nomifun_realtime::BroadcastEventBus::new(64));

    let pref_repo: Arc<dyn nomifun_db::IClientPreferenceRepository> =
        Arc::new(nomifun_db::SqliteClientPreferenceRepository::new(pool));
    let settings = Arc::new(ChannelSettingsService::new(pref_repo));

    let pairing = Arc::new(PairingService::new(repo.clone(), bus));
    let session_mgr = Arc::new(SessionManager::new(repo.clone()));
    let executor = Arc::new(ActionExecutor::new(pairing, Arc::clone(&session_mgr), settings, "acp"));

    // The pairing code created for the unauthorized user carries an FK
    // channel_id → assistant_plugins(id), so the bot channel must exist first.
    repo.upsert_plugin(&ChannelPluginRow {
        id: TEST_CHANNEL.into(),
        r#type: "telegram".into(),
        name: "Test Bot".into(),
        enabled: true,
        config: "{}".into(),
        status: None,
        last_connected: None,
        companion_id: None,
        public_agent_id: None,
        bot_key: None,
        created_at: now_ms(),
        updated_at: now_ms(),
    })
    .await
    .unwrap();

    let msg = make_text_message("unknown_user", "chat_1", "hello");
    let result = executor.handle_incoming_message(&msg, TEST_CHANNEL).await.unwrap();

    match result {
        MessageResult::Action(response) => {
            let text = response.text.unwrap();
            assert!(text.len() > 5, "expected pairing response, got: {text}");
        }
        other => panic!("expected Action, got: {other:?}"),
    }
}

// ═════════════════════════════════════════════════════════════════════════
// Full-pipeline tests: busy guard, chat.continue, chat.regenerate
// ═════════════════════════════════════════════════════════════════════════

struct TestBroadcaster;

impl EventBroadcaster for TestBroadcaster {
    fn broadcast(&self, _event: WebSocketMessage<serde_json::Value>) {}
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

/// Everything needed to drive the orchestrator end-to-end with an in-memory
/// DB, a scripted agent, and a recording channel sender.
struct Harness {
    message_tx: mpsc::Sender<ChannelIncoming>,
    /// Held so the orchestrator's confirm branch stays open.
    _confirm_tx: mpsc::Sender<(String, String)>,
    recorder: Arc<MessageRecorder>,
    channel_repo: Arc<dyn IChannelRepository>,
    conversation_svc: Arc<ConversationService>,
    runtime: Arc<ConversationRuntimeStateService>,
    /// The shared pending-decision store the orchestrator's relay/interception
    /// uses, so tests can seed and inspect pending decisions.
    pending_decisions: Arc<nomifun_channel::pending_decision::PendingDecisionStore>,
}

async fn build_harness() -> Harness {
    let db = nomifun_db::init_database_memory().await.unwrap();
    let pool = db.pool().clone();

    let channel_repo: Arc<dyn IChannelRepository> = Arc::new(SqliteChannelRepository::new(pool.clone()));
    let bus = Arc::new(nomifun_realtime::BroadcastEventBus::new(64));
    let settings = Arc::new(ChannelSettingsService::new(Arc::new(SqliteClientPreferenceRepository::new(
        pool.clone(),
    ))));
    let pairing = Arc::new(PairingService::new(channel_repo.clone(), bus));
    let session_mgr = Arc::new(SessionManager::new(channel_repo.clone()));
    let executor = Arc::new(ActionExecutor::new(
        pairing,
        Arc::clone(&session_mgr),
        Arc::clone(&settings),
        "nomi",
    ));

    // Every test message arrives through TEST_CHANNEL ("tg-1"). assistant_sessions
    // now has an FK channel_id → assistant_plugins(id), so the plugin row must
    // exist before any session is created. bot_key=None avoids the
    // UNIQUE(type, bot_key) index.
    channel_repo
        .upsert_plugin(&ChannelPluginRow {
            id: TEST_CHANNEL.into(),
            r#type: "telegram".into(),
            name: "Test Bot".into(),
            enabled: true,
            config: "{}".into(),
            status: None,
            last_connected: None,
            companion_id: None,
            public_agent_id: None,
            bot_key: None,
            created_at: now_ms(),
            updated_at: now_ms(),
        })
        .await
        .unwrap();

    // Authorize the test user so messages reach the dispatch path.
    channel_repo
        .create_user(&AssistantUserRow {
            id: "user_tg_42".into(),
            platform_user_id: "tg_42".into(),
            platform_type: "telegram".into(),
            channel_id: Some(TEST_CHANNEL.into()),
            display_name: Some("Test".into()),
            authorized_at: now_ms(),
            last_active: None,
            session_id: None,
        })
        .await
        .unwrap();

    let task_manager: Arc<dyn IWorkerTaskManager> = Arc::new(RecordingTaskManager::new());
    let runtime = Arc::new(ConversationRuntimeStateService::default());
    let conversation_svc = Arc::new(
        ConversationService::new(
            std::env::temp_dir(),
            Arc::new(TestBroadcaster),
            Arc::new(NoopSkillResolver),
            Arc::clone(&task_manager),
            Arc::new(SqliteConversationRepository::new(pool.clone())),
            Arc::new(SqliteAgentMetadataRepository::new(pool.clone())),
            Arc::new(SqliteAcpSessionRepository::new(pool.clone())),
        )
        .with_runtime_state(Arc::clone(&runtime)),
    );
    let message_svc = Arc::new(ChannelMessageService::new(
        Arc::clone(&conversation_svc),
        Arc::clone(&task_manager),
        settings,
        channel_repo.clone(),
        "system_default_user".to_owned(),
    ));
    let pending_decisions = message_svc.pending_decisions();

    let recorder = Arc::new(MessageRecorder::new());
    let orchestrator = ChannelOrchestrator::new(
        executor,
        message_svc,
        session_mgr,
        Arc::clone(&recorder) as Arc<dyn ChannelSender>,
    );

    let (message_tx, message_rx) = mpsc::channel(16);
    let (confirm_tx, confirm_rx) = mpsc::channel(16);
    tokio::spawn(orchestrator.run(message_rx, confirm_rx));

    Harness {
        message_tx,
        _confirm_tx: confirm_tx,
        recorder,
        channel_repo,
        conversation_svc,
        runtime,
        pending_decisions,
    }
}

/// Polls the channel sessions until one has a bound conversation.
async fn wait_for_bound_conversation(repo: &Arc<dyn IChannelRepository>) -> String {
    for _ in 0..500 {
        let sessions = repo.get_all_sessions().await.unwrap();
        // Session FK is now i64; this helper returns a String for the
        // string-keyed downstream calls (Option A).
        if let Some(cid) = sessions.iter().find_map(|s| s.conversation_id.map(|id| id.to_string())) {
            return cid;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    panic!("no session was bound to a conversation");
}

/// Waits for the runtime turn claim of `conversation_id` to be released.
async fn wait_until_idle(svc: &Arc<ConversationService>, conversation_id: &str) {
    for _ in 0..500 {
        let summary = svc.runtime_summary_for(conversation_id).await;
        if summary.state == ConversationRuntimeStateKind::Idle {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    panic!("conversation {conversation_id} never became idle");
}

/// Drains the recorder until a send containing `needle` shows up.
async fn wait_for_send_containing(recorder: &Arc<MessageRecorder>, needle: &str) -> UnifiedOutgoingMessage {
    let mut seen: Vec<UnifiedOutgoingMessage> = Vec::new();
    for _ in 0..500 {
        seen.extend(recorder.take_sends());
        if let Some(found) = seen
            .iter()
            .find(|m| m.text.as_deref().is_some_and(|t| t.contains(needle)))
        {
            return found.clone();
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    panic!("no send containing {needle:?}; saw: {seen:?}");
}

/// Returns the visible user (`right`) message texts of a conversation.
async fn user_messages(svc: &Arc<ConversationService>, conversation_id: &str) -> Vec<String> {
    let query = ListMessagesQuery {
        page: Some(1),
        page_size: Some(50),
        order: Some("ASC".into()),
        content_mode: None,
        cursor: None,
    };
    let result = svc
        .list_messages("system_default_user", conversation_id, query)
        .await
        .unwrap();
    result
        .items
        .iter()
        .filter(|m| m.position == Some(MessagePosition::Right))
        .filter_map(|m| m.content.get("content").and_then(|v| v.as_str()).map(str::to_owned))
        .collect()
}

/// Polls until the conversation has `expected` visible user messages.
async fn wait_for_user_message_count(
    svc: &Arc<ConversationService>,
    conversation_id: &str,
    expected: usize,
) -> Vec<String> {
    let mut last = Vec::new();
    for _ in 0..500 {
        last = user_messages(svc, conversation_id).await;
        if last.len() >= expected {
            return last;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    panic!("conversation never reached {expected} user messages; got {last:?}");
}

/// Fix 4: a second message for a busy conversation must be answered with the
/// "still processing" notice instead of racing a second prompt.
#[tokio::test]
async fn busy_conversation_replies_with_processing_notice() {
    let harness = build_harness().await;

    harness
        .message_tx
        .send(incoming(make_text_message("tg_42", "chat_1", "hello world")))
        .await
        .unwrap();
    let cid = wait_for_bound_conversation(&harness.channel_repo).await;
    wait_until_idle(&harness.conversation_svc, &cid).await;

    // Simulate an in-flight turn exactly the way send_message does.
    let _claim = harness.runtime.try_claim_turn(&cid).unwrap();

    harness
        .message_tx
        .send(incoming(make_text_message("tg_42", "chat_1", "second message")))
        .await
        .unwrap();

    wait_for_send_containing(&harness.recorder, "still being processed").await;

    // The guard fired before send_to_agent: no second user message was
    // persisted into the conversation.
    let messages = user_messages(&harness.conversation_svc, &cid).await;
    assert_eq!(messages, vec!["hello world".to_string()]);
}

/// Fix 3: chat.continue dispatches the fixed continue prompt as a user turn
/// through the regular streaming pipeline.
#[tokio::test]
async fn chat_continue_sends_continue_prompt_to_agent() {
    let harness = build_harness().await;

    harness
        .message_tx
        .send(incoming(make_text_message("tg_42", "chat_1", "hello world")))
        .await
        .unwrap();
    let cid = wait_for_bound_conversation(&harness.channel_repo).await;
    wait_until_idle(&harness.conversation_svc, &cid).await;

    harness
        .message_tx
        .send(incoming(make_chat_action_message("tg_42", "chat_1", "chat.continue")))
        .await
        .unwrap();

    let messages = wait_for_user_message_count(&harness.conversation_svc, &cid, 2).await;
    assert_eq!(messages, vec![
        "hello world".to_string(),
        nomifun_channel::action::CONTINUE_PROMPT.to_string()
    ]);
}

/// Fix 3: chat.regenerate resends the conversation's last user message.
#[tokio::test]
async fn chat_regenerate_resends_last_user_message() {
    let harness = build_harness().await;

    harness
        .message_tx
        .send(incoming(make_text_message("tg_42", "chat_1", "hello world")))
        .await
        .unwrap();
    let cid = wait_for_bound_conversation(&harness.channel_repo).await;
    wait_until_idle(&harness.conversation_svc, &cid).await;

    harness
        .message_tx
        .send(incoming(make_chat_action_message("tg_42", "chat_1", "chat.regenerate")))
        .await
        .unwrap();

    let messages = wait_for_user_message_count(&harness.conversation_svc, &cid, 2).await;
    assert_eq!(messages, vec!["hello world".to_string(), "hello world".to_string()]);
}

/// Fix 3: chat.regenerate before any message exists must reply with a
/// helpful notice instead of silently doing nothing.
#[tokio::test]
async fn chat_regenerate_without_history_replies_with_notice() {
    let harness = build_harness().await;

    harness
        .message_tx
        .send(incoming(make_chat_action_message("tg_42", "chat_1", "chat.regenerate")))
        .await
        .unwrap();

    wait_for_send_containing(&harness.recorder, "no previous message to regenerate").await;
}

// ═════════════════════════════════════════════════════════════════════════
// Bug 1, Case A: relayed decision → numbered reply interception
// ═════════════════════════════════════════════════════════════════════════

use nomifun_channel::pending_decision::PendingDecision;
use nomifun_channel::types::DecisionOption;

/// Seeds a two-option pending decision for `conversation_id`.
fn seed_decision(harness: &Harness, conversation_id: &str) {
    harness.pending_decisions.put(PendingDecision {
        conversation_id: conversation_id.to_owned(),
        call_id: "call-dec".into(),
        prompt: "Proceed?".into(),
        options: vec![
            DecisionOption {
                option_id: "allow".into(),
                label: "Allow".into(),
            },
            DecisionOption {
                option_id: "reject".into(),
                label: "Reject".into(),
            },
        ],
    });
}

/// A numeric reply to a pending decision resolves it (ack + cleared store)
/// and is NOT dispatched as a new user prompt.
#[tokio::test]
async fn decision_numeric_reply_resolves_and_does_not_dispatch() {
    let harness = build_harness().await;

    // Establish a bound conversation with exactly one user message.
    harness
        .message_tx
        .send(incoming(make_text_message("tg_42", "chat_1", "hello world")))
        .await
        .unwrap();
    let cid = wait_for_bound_conversation(&harness.channel_repo).await;
    wait_until_idle(&harness.conversation_svc, &cid).await;

    // The conversation is now blocked on a decision.
    seed_decision(&harness, &cid);

    harness
        .message_tx
        .send(incoming(make_text_message("tg_42", "chat_1", "2")))
        .await
        .unwrap();

    // Ack confirms the chosen label.
    wait_for_send_containing(&harness.recorder, "已选择：Reject").await;

    // Pending entry cleared.
    for _ in 0..500 {
        if harness.pending_decisions.peek(&cid).is_none() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    assert!(harness.pending_decisions.peek(&cid).is_none(), "pending decision must be cleared");

    // No second user message was dispatched — the reply was consumed.
    let messages = user_messages(&harness.conversation_svc, &cid).await;
    assert_eq!(messages, vec!["hello world".to_string()]);
}

/// A non-numeric reply while a decision is pending re-shows the numbered list
/// and is NOT dispatched.
#[tokio::test]
async fn decision_non_numeric_reply_reshows_list_and_does_not_dispatch() {
    let harness = build_harness().await;

    harness
        .message_tx
        .send(incoming(make_text_message("tg_42", "chat_1", "hello world")))
        .await
        .unwrap();
    let cid = wait_for_bound_conversation(&harness.channel_repo).await;
    wait_until_idle(&harness.conversation_svc, &cid).await;

    seed_decision(&harness, &cid);

    harness
        .message_tx
        .send(incoming(make_text_message("tg_42", "chat_1", "what?")))
        .await
        .unwrap();

    // The numbered list is re-shown.
    let reshow = wait_for_send_containing(&harness.recorder, "需要你的决策").await;
    let text = reshow.text.unwrap();
    assert!(text.contains("1. Allow"), "re-shown list numbered: {text}");
    assert!(text.contains("2. Reject"), "re-shown list numbered: {text}");

    // Pending entry survives (the user still has to answer).
    assert!(harness.pending_decisions.peek(&cid).is_some(), "pending decision must survive a bad reply");

    // No new user message dispatched.
    let messages = user_messages(&harness.conversation_svc, &cid).await;
    assert_eq!(messages, vec!["hello world".to_string()]);
}

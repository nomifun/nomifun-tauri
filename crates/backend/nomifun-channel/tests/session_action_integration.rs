//! Black-box integration tests for SessionManager and ActionExecutor.
//!
//! Uses real SQLite (in-memory) and mock EventBroadcaster.
//! Covers test-plan items: GS-1, GS-2, PC-1..PC-3, RU-3.

use std::sync::{Arc, Mutex};

use nomifun_api_types::WebSocketMessage;
use nomifun_common::{generate_id, now_ms};
use nomifun_db::models::{AssistantUserRow, ChannelPluginRow};
use nomifun_db::{IChannelRepository, SqliteChannelRepository, init_database_memory};
use nomifun_realtime::EventBroadcaster;

use nomifun_channel::action::{ActionExecutor, MessageResult};
use nomifun_channel::channel_settings::ChannelSettingsService;
use nomifun_channel::pairing::PairingService;
use nomifun_channel::session::SessionManager;
use nomifun_channel::types::{
    ActionBehavior, ActionCategory, ActionContext, MessageContentType, PluginType, UnifiedAction,
    UnifiedIncomingMessage, UnifiedMessageContent, UnifiedUser,
};

// ── Test infrastructure ─────────────────────────────────────────────

struct MockBroadcaster {
    events: Mutex<Vec<WebSocketMessage<serde_json::Value>>>,
}

impl MockBroadcaster {
    fn new() -> Self {
        Self {
            events: Mutex::new(Vec::new()),
        }
    }
}

impl EventBroadcaster for MockBroadcaster {
    fn broadcast(&self, event: WebSocketMessage<serde_json::Value>) {
        self.events.lock().unwrap().push(event);
    }
}

async fn setup() -> (
    SessionManager,
    ActionExecutor,
    PairingService,
    Arc<dyn IChannelRepository>,
) {
    let db = init_database_memory().await.unwrap();
    let repo: Arc<dyn IChannelRepository> = Arc::new(SqliteChannelRepository::new(db.pool().clone()));
    let bc: Arc<dyn EventBroadcaster> = Arc::new(MockBroadcaster::new());

    let session_mgr = SessionManager::new(repo.clone());
    let pairing = PairingService::new(repo.clone(), bc);
    let pairing_arc = Arc::new(PairingService::new(repo.clone(), Arc::new(MockBroadcaster::new())));
    let session_mgr_arc = Arc::new(SessionManager::new(repo.clone()));
    let pref_repo: Arc<dyn nomifun_db::IClientPreferenceRepository> =
        Arc::new(nomifun_db::SqliteClientPreferenceRepository::new(db.pool().clone()));
    let settings = Arc::new(ChannelSettingsService::new(pref_repo));
    let executor = ActionExecutor::new(pairing_arc, session_mgr_arc, settings, "gemini");

    // Every test message arrives through the "tg-1" channel. assistant_sessions
    // now has an FK channel_id → assistant_plugins(id), so the plugin row must
    // exist before any session is created. bot_key=None avoids the
    // UNIQUE(type, bot_key) index.
    repo.upsert_plugin(&ChannelPluginRow {
        id: "tg-1".into(),
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

    // Keep db alive
    std::mem::forget(db);
    (session_mgr, executor, pairing, repo)
}

/// Create an assistant_users record (required for FK on sessions).
async fn create_user(repo: &Arc<dyn IChannelRepository>, platform_user_id: &str, platform_type: &str) -> String {
    let user_id = generate_id();
    let row = AssistantUserRow {
        id: user_id.clone(),
        platform_user_id: platform_user_id.to_owned(),
        platform_type: platform_type.to_owned(),
        channel_id: Some("tg-1".into()),
        display_name: Some("Test User".into()),
        authorized_at: now_ms(),
        last_active: None,
        session_id: None,
    };
    repo.create_user(&row).await.unwrap();
    user_id
}

fn make_text_message(user_id: &str, chat_id: &str, text: &str) -> UnifiedIncomingMessage {
    UnifiedIncomingMessage {
        id: format!("msg_{}", now_ms()),
        platform: PluginType::Telegram,
        chat_id: chat_id.into(),
        user: UnifiedUser {
            id: user_id.into(),
            username: None,
            display_name: "Test User".into(),
            avatar_url: None,
        },
        content: UnifiedMessageContent {
            content_type: MessageContentType::Text,
            text: text.into(),
            attachments: None,
        },
        timestamp: now_ms(),
        reply_to_message_id: None,
        action: None,
        raw: None,
    }
}

fn make_action_message(
    user_id: &str,
    chat_id: &str,
    action_name: &str,
    category: ActionCategory,
) -> UnifiedIncomingMessage {
    UnifiedIncomingMessage {
        id: format!("msg_{}", now_ms()),
        platform: PluginType::Telegram,
        chat_id: chat_id.into(),
        user: UnifiedUser {
            id: user_id.into(),
            username: None,
            display_name: "Test User".into(),
            avatar_url: None,
        },
        content: UnifiedMessageContent {
            content_type: MessageContentType::Action,
            text: String::new(),
            attachments: None,
        },
        timestamp: now_ms(),
        reply_to_message_id: None,
        action: Some(UnifiedAction {
            action: action_name.into(),
            category,
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

/// Helper: authorize a user via the pairing flow.
async fn authorize_user(pairing: &PairingService, platform_user_id: &str, platform_type: &str) {
    let code = pairing
        .request_pairing(platform_user_id, platform_type, "tg-1", Some("Test"))
        .await
        .unwrap();
    pairing.approve_pairing(&code).await.unwrap();
}

// ── GS-1: No active sessions returns empty ─────────────────────────

#[tokio::test]
async fn gs1_no_sessions_returns_empty() {
    let (session_mgr, _, _, _) = setup().await;
    let sessions = session_mgr.get_active_sessions().await.unwrap();
    assert!(sessions.is_empty());
}

// ── GS-2: Multiple active sessions returned ────────────────────────

#[tokio::test]
async fn gs2_multiple_sessions_returned() {
    let (session_mgr, _, _, repo) = setup().await;

    // Create users first (FK constraint)
    let uid1 = create_user(&repo, "p1", "telegram").await;
    let uid2 = create_user(&repo, "p2", "telegram").await;

    session_mgr
        .get_or_create_session(&uid1, "c1", "tg-1", "gemini", None)
        .await
        .unwrap();
    session_mgr
        .get_or_create_session(&uid2, "c2", "tg-1", "acp", None)
        .await
        .unwrap();

    let sessions = session_mgr.get_active_sessions().await.unwrap();
    assert_eq!(sessions.len(), 2);

    for s in &sessions {
        assert!(!s.id.is_empty());
        assert!(!s.user_id.is_empty());
        assert!(!s.agent_type.is_empty());
        assert!(s.chat_id.is_some());
        assert!(s.created_at > 0);
        assert!(s.last_activity > 0);
    }
}

// ── PC-1: Same user, different chatId → different sessions ─────────

#[tokio::test]
async fn pc1_same_user_different_chat() {
    let (session_mgr, _, _, repo) = setup().await;

    let uid = create_user(&repo, "p1", "telegram").await;

    let s1 = session_mgr
        .get_or_create_session(&uid, "chatA", "tg-1", "gemini", None)
        .await
        .unwrap();
    let s2 = session_mgr
        .get_or_create_session(&uid, "chatB", "tg-1", "gemini", None)
        .await
        .unwrap();

    assert_ne!(s1.id, s2.id);
    assert_eq!(s1.user_id, uid);
    assert_eq!(s2.user_id, uid);
    assert_eq!(s1.chat_id.as_deref(), Some("chatA"));
    assert_eq!(s2.chat_id.as_deref(), Some("chatB"));
}

// ── PC-2: Different users, same chatId → different sessions ────────

#[tokio::test]
async fn pc2_different_users_same_chat() {
    let (session_mgr, _, _, repo) = setup().await;

    let uid1 = create_user(&repo, "p1", "telegram").await;
    let uid2 = create_user(&repo, "p2", "telegram").await;

    let s1 = session_mgr
        .get_or_create_session(&uid1, "chatA", "tg-1", "gemini", None)
        .await
        .unwrap();
    let s2 = session_mgr
        .get_or_create_session(&uid2, "chatA", "tg-1", "gemini", None)
        .await
        .unwrap();

    assert_ne!(s1.id, s2.id);
}

// ── PC-3: Same user, same chatId → reuse session ──────────────────

#[tokio::test]
async fn pc3_same_user_same_chat_reuses() {
    let (session_mgr, _, _, repo) = setup().await;

    let uid = create_user(&repo, "p1", "telegram").await;

    let s1 = session_mgr
        .get_or_create_session(&uid, "chatA", "tg-1", "gemini", None)
        .await
        .unwrap();
    let s2 = session_mgr
        .get_or_create_session(&uid, "chatA", "tg-1", "gemini", None)
        .await
        .unwrap();

    assert_eq!(s1.id, s2.id);
}

// ── RU-3: Revoke user clears sessions ──────────────────────────────

#[tokio::test]
async fn ru3_revoke_clears_sessions() {
    let (session_mgr, _, _, repo) = setup().await;

    let uid1 = create_user(&repo, "p1", "telegram").await;
    let uid2 = create_user(&repo, "p2", "telegram").await;

    session_mgr
        .get_or_create_session(&uid1, "c1", "tg-1", "gemini", None)
        .await
        .unwrap();
    session_mgr
        .get_or_create_session(&uid1, "c2", "tg-1", "acp", None)
        .await
        .unwrap();
    session_mgr
        .get_or_create_session(&uid2, "c1", "tg-1", "gemini", None)
        .await
        .unwrap();

    // Cleanup user1 sessions
    session_mgr.cleanup_user_sessions(&uid1).await.unwrap();

    let sessions = repo.get_all_sessions().await.unwrap();
    assert_eq!(sessions.len(), 1);
    assert_eq!(sessions[0].user_id, uid2);
}

// ── ActionExecutor: unauthorized user gets pairing ─────────────────

#[tokio::test]
async fn action_unauthorized_triggers_pairing() {
    let (_, executor, _, _) = setup().await;

    let msg = make_text_message("new_user", "chat1", "Hello");
    let result = executor.handle_incoming_message(&msg, "tg-1").await.unwrap();

    match result {
        MessageResult::Action(resp) => {
            assert_eq!(resp.behavior, ActionBehavior::Send);
            let text = resp.text.unwrap();
            assert!(text.contains("pairing code"));
            assert!(resp.buttons.is_some());
        }
        _ => panic!("Expected Action (pairing) for unauthorized user"),
    }
}

// ── ActionExecutor: authorized user dispatches to agent ────────────

#[tokio::test]
async fn action_authorized_dispatches() {
    let (_, executor, pairing, _) = setup().await;

    authorize_user(&pairing, "tg_42", "telegram").await;

    let msg = make_text_message("tg_42", "chat1", "Hello AI");
    let result = executor.handle_incoming_message(&msg, "tg-1").await.unwrap();

    match result {
        MessageResult::Dispatched { session_id, .. } => {
            assert!(!session_id.is_empty());
        }
        _ => panic!("Expected Dispatched for authorized user"),
    }
}

// ── ActionExecutor: help.show action ───────────────────────────────

#[tokio::test]
async fn action_help_show() {
    let (_, executor, pairing, _) = setup().await;

    authorize_user(&pairing, "tg_42", "telegram").await;

    let msg = make_action_message("tg_42", "chat1", "help.show", ActionCategory::System);
    let result = executor.handle_incoming_message(&msg, "tg-1").await.unwrap();

    match result {
        MessageResult::Action(resp) => {
            assert!(resp.text.is_some());
            assert!(resp.buttons.is_some());
            let buttons = resp.buttons.unwrap();
            assert!(buttons.len() >= 2);
        }
        _ => panic!("Expected Action result"),
    }
}

// ── ActionExecutor: session.new action ─────────────────────────────

#[tokio::test]
async fn action_session_new() {
    let (_, executor, pairing, _) = setup().await;

    authorize_user(&pairing, "tg_42", "telegram").await;

    let msg = make_action_message("tg_42", "chat1", "session.new", ActionCategory::System);
    let result = executor.handle_incoming_message(&msg, "tg-1").await.unwrap();

    match result {
        MessageResult::Action(resp) => {
            let text = resp.text.unwrap();
            assert!(text.contains("New session"));
            // With no client_preferences, defaults to "nomi"
            assert!(text.contains("nomi"));
        }
        _ => panic!("Expected Action result"),
    }
}

// ── ActionExecutor: session.new resets the session (H-2 fix) ─────

#[tokio::test]
async fn action_session_new_resets_existing() {
    let (_, executor, pairing, repo) = setup().await;

    authorize_user(&pairing, "tg_42", "telegram").await;

    // Create a session by sending a text message
    let msg1 = make_text_message("tg_42", "chat1", "Hello");
    let r1 = executor.handle_incoming_message(&msg1, "tg-1").await.unwrap();
    let sid1 = match r1 {
        MessageResult::Dispatched { session_id, .. } => session_id,
        _ => panic!("Expected Dispatched"),
    };

    // session.new should delete old and create fresh
    let new_msg = make_action_message("tg_42", "chat1", "session.new", ActionCategory::System);
    let r2 = executor.handle_incoming_message(&new_msg, "tg-1").await.unwrap();
    match r2 {
        MessageResult::Action(resp) => {
            let text = resp.text.unwrap();
            assert!(text.contains("New session"));
        }
        _ => panic!("Expected Action result"),
    }

    // Send another text message — should get a different session ID
    let msg3 = make_text_message("tg_42", "chat1", "Hello again");
    let r3 = executor.handle_incoming_message(&msg3, "tg-1").await.unwrap();
    let sid3 = match r3 {
        MessageResult::Dispatched { session_id, .. } => session_id,
        _ => panic!("Expected Dispatched"),
    };

    // The new session should have a different ID from the original
    assert_ne!(sid1, sid3);

    // Only 1 session should exist for this user+chat in the DB
    let all = repo.get_all_sessions().await.unwrap();
    let user_sessions: Vec<_> = all.iter().filter(|s| s.chat_id.as_deref() == Some("chat1")).collect();
    assert_eq!(user_sessions.len(), 1);
}

// ── ActionExecutor: agent.select persists agent_type (H-3 fix) ───

#[tokio::test]
async fn action_agent_select_persists() {
    let (_, executor, pairing, repo) = setup().await;

    authorize_user(&pairing, "tg_42", "telegram").await;

    // Create a session (default agent is "gemini")
    let msg1 = make_text_message("tg_42", "chat1", "Hello");
    executor.handle_incoming_message(&msg1, "tg-1").await.unwrap();

    // Switch agent to "acp"
    let select_msg = UnifiedIncomingMessage {
        id: format!("msg_{}", now_ms()),
        platform: PluginType::Telegram,
        chat_id: "chat1".into(),
        user: UnifiedUser {
            id: "tg_42".into(),
            username: None,
            display_name: "Test User".into(),
            avatar_url: None,
        },
        content: UnifiedMessageContent {
            content_type: MessageContentType::Action,
            text: String::new(),
            attachments: None,
        },
        timestamp: now_ms(),
        reply_to_message_id: None,
        action: Some(UnifiedAction {
            action: "agent.select".into(),
            category: ActionCategory::System,
            params: Some(std::collections::HashMap::from([("agentType".into(), "acp".into())])),
            context: ActionContext {
                platform: PluginType::Telegram,
                user_id: "tg_42".into(),
                chat_id: "chat1".into(),
                message_id: None,
                session_id: None,
            },
        }),
        raw: None,
    };
    let r = executor.handle_incoming_message(&select_msg, "tg-1").await.unwrap();
    match r {
        MessageResult::Action(resp) => {
            let text = resp.text.unwrap();
            assert!(text.contains("acp"));
        }
        _ => panic!("Expected Action result"),
    }

    // Verify the session's agent_type in DB
    let all = repo.get_all_sessions().await.unwrap();
    let session = all
        .iter()
        .find(|s| s.chat_id.as_deref() == Some("chat1"))
        .expect("session should exist");
    assert_eq!(session.agent_type, "acp");
}

// ── ActionExecutor: session isolation across messages ───────────────

#[tokio::test]
async fn action_session_isolation() {
    let (_, executor, pairing, _) = setup().await;

    authorize_user(&pairing, "tg_42", "telegram").await;

    // Send messages in two different chats
    let msg1 = make_text_message("tg_42", "chatA", "Hello 1");
    let msg2 = make_text_message("tg_42", "chatB", "Hello 2");

    let r1 = executor.handle_incoming_message(&msg1, "tg-1").await.unwrap();
    let r2 = executor.handle_incoming_message(&msg2, "tg-1").await.unwrap();

    let sid1 = match r1 {
        MessageResult::Dispatched { session_id, .. } => session_id,
        _ => panic!("Expected Dispatched"),
    };
    let sid2 = match r2 {
        MessageResult::Dispatched { session_id, .. } => session_id,
        _ => panic!("Expected Dispatched"),
    };

    // Different chats → different sessions
    assert_ne!(sid1, sid2);

    // Same chat again → reuse
    let msg3 = make_text_message("tg_42", "chatA", "Hello 3");
    let r3 = executor.handle_incoming_message(&msg3, "tg-1").await.unwrap();
    let sid3 = match r3 {
        MessageResult::Dispatched { session_id, .. } => session_id,
        _ => panic!("Expected Dispatched"),
    };
    assert_eq!(sid1, sid3);
}

// Note: bind_conversation FK-constrained persistence is tested in
// nomifun-db sqlite_channel.rs::update_session_conversation_persists.
// Unit tests for the SessionManager layer are in session.rs.

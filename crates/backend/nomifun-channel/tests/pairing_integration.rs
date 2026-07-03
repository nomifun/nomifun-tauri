//! Black-box integration tests for `PairingService`.
//!
//! Uses real SQLite (in-memory) and mock EventBroadcaster.
//! Covers test-plan items: PG-1..PG-3, AP-1..AP-6, RP-1..RP-4,
//! PP-1..PP-3, EC-1..EC-2, DC-2..DC-3, WS-1, WS-3.

use std::sync::{Arc, Mutex};

use nomifun_api_types::WebSocketMessage;
use nomifun_common::{TimestampMs, now_ms};
use nomifun_db::models::{ChannelPluginRow, PairingCodeRow};
use nomifun_db::{IChannelRepository, SqliteChannelRepository, init_database_memory};
use nomifun_realtime::EventBroadcaster;

use nomifun_channel::constants::{PAIRING_CODE_LENGTH, PAIRING_CODE_TTL};
use nomifun_channel::error::ChannelError;
use nomifun_channel::pairing::PairingService;

/// Telegram bot channel id used by the integration tests. `assistant_pairing_codes`
/// and `assistant_users` carry an FK channel_id → assistant_plugins(id), so the
/// plugin rows must exist before any pairing is created.
const CH_TG: &str = "tg-1";
/// Lark bot channel id (second platform exercised by these tests).
const CH_LARK: &str = "lark-1";
/// A second lark bot channel id. Same platform as `CH_LARK`, different bot —
/// used to prove pairing/auth are scoped per bot (channel), not per platform.
/// `setup()` does not seed this; the multi-bot test seeds it itself so the
/// channel_id FK is satisfied.
const CH_LARK2: &str = "lark-2";

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

    fn take_events(&self) -> Vec<WebSocketMessage<serde_json::Value>> {
        let mut guard = self.events.lock().unwrap();
        std::mem::take(&mut *guard)
    }
}

impl EventBroadcaster for MockBroadcaster {
    fn broadcast(&self, event: WebSocketMessage<serde_json::Value>) {
        self.events.lock().unwrap().push(event);
    }
}

async fn setup() -> (PairingService, Arc<dyn IChannelRepository>, Arc<MockBroadcaster>) {
    let db = init_database_memory().await.unwrap();
    let repo: Arc<dyn IChannelRepository> = Arc::new(SqliteChannelRepository::new(db.pool().clone()));
    let bc = Arc::new(MockBroadcaster::new());
    let svc = PairingService::new(repo.clone(), bc.clone());

    // Seed the bot channels the tests pair against. assistant_pairing_codes /
    // assistant_users have an FK channel_id → assistant_plugins(id), so these
    // rows must exist before request_pairing inserts a code.
    for (id, ty, name) in [(CH_TG, "telegram", "Telegram Bot"), (CH_LARK, "lark", "Lark Bot")] {
        repo.upsert_plugin(&ChannelPluginRow {
            id: id.into(),
            r#type: ty.into(),
            name: name.into(),
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
    }

    // Keep db alive by leaking — test process exits anyway
    std::mem::forget(db);
    (svc, repo, bc)
}

// ── PG-1: Generated code is 6 digits ───────────────────────────────

#[tokio::test]
async fn pg1_code_is_six_digits() {
    let (svc, _repo, _bc) = setup().await;
    let code = svc.request_pairing("u1", "telegram", CH_TG, Some("Alice")).await.unwrap();
    assert_eq!(code.len(), PAIRING_CODE_LENGTH);
    assert!(code.chars().all(|c| c.is_ascii_digit()));
}

// ── PG-2: Code expires after 10 minutes ────────────────────────────

#[tokio::test]
async fn pg2_code_expires_after_ten_minutes() {
    let (svc, repo, _bc) = setup().await;
    let before = now_ms();
    let code = svc.request_pairing("u1", "telegram", CH_TG, None).await.unwrap();
    let after = now_ms();

    let row = repo.get_pairing_by_code(&code).await.unwrap().unwrap();
    let ttl = PAIRING_CODE_TTL.as_millis() as TimestampMs;
    assert!(row.expires_at >= before + ttl);
    assert!(row.expires_at <= after + ttl);
}

// ── PG-3: Same user re-request expires old code ────────────────────

#[tokio::test]
async fn pg3_same_user_re_request_expires_old_code() {
    let (svc, repo, _bc) = setup().await;
    let code1 = svc.request_pairing("u1", "telegram", CH_TG, Some("Alice")).await.unwrap();
    let code2 = svc.request_pairing("u1", "telegram", CH_TG, Some("Alice")).await.unwrap();

    assert_ne!(code1, code2);

    let old = repo.get_pairing_by_code(&code1).await.unwrap().unwrap();
    let new = repo.get_pairing_by_code(&code2).await.unwrap().unwrap();
    assert_eq!(old.status, "expired");
    assert_eq!(new.status, "pending");
}

// ── PP-1: No pending pairings returns empty ────────────────────────

#[tokio::test]
async fn pp1_no_pending_returns_empty() {
    let (svc, _repo, _bc) = setup().await;
    let pending = svc.get_pending_pairings().await.unwrap();
    assert!(pending.is_empty());
}

// ── PP-2: Multiple pending pairings returned ───────────────────────

#[tokio::test]
async fn pp2_multiple_pending_returned() {
    let (svc, _repo, _bc) = setup().await;
    svc.request_pairing("u1", "telegram", CH_TG, Some("Alice")).await.unwrap();
    svc.request_pairing("u2", "lark", CH_LARK, Some("Bob")).await.unwrap();

    let pending = svc.get_pending_pairings().await.unwrap();
    assert_eq!(pending.len(), 2);
}

// ── PP-3: Expired pairings not in pending list ─────────────────────

#[tokio::test]
async fn pp3_expired_not_in_pending() {
    let (svc, repo, _bc) = setup().await;
    svc.request_pairing("u1", "telegram", CH_TG, None).await.unwrap();

    // Insert already-expired code directly
    let expired_row = PairingCodeRow {
        code: "000001".into(),
        platform_user_id: "u2".into(),
        platform_type: "lark".into(),
        channel_id: None,
        display_name: None,
        requested_at: 1000,
        expires_at: 1001,
        status: "pending".into(),
    };
    repo.create_pairing(&expired_row).await.unwrap();

    let pending = svc.get_pending_pairings().await.unwrap();
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].platform_user_id, "u1");
}

// ── AP-1: Approve valid pairing ────────────────────────────────────

#[tokio::test]
async fn ap1_approve_valid_pairing() {
    let (svc, repo, _bc) = setup().await;
    let code = svc.request_pairing("tg_42", "telegram", CH_TG, Some("Alice")).await.unwrap();

    svc.approve_pairing(&code).await.unwrap();

    // Status updated
    let row = repo.get_pairing_by_code(&code).await.unwrap().unwrap();
    assert_eq!(row.status, "approved");
}

// ── AP-2: Approved user appears in authorized list (DC-2) ──────────

#[tokio::test]
async fn ap2_dc2_approved_user_in_authorized_list() {
    let (svc, repo, _bc) = setup().await;
    let code = svc.request_pairing("tg_42", "telegram", CH_TG, Some("Alice")).await.unwrap();
    svc.approve_pairing(&code).await.unwrap();

    let users = repo.get_all_users().await.unwrap();
    assert_eq!(users.len(), 1);
    assert_eq!(users[0].platform_user_id, "tg_42");
    assert_eq!(users[0].platform_type, "telegram");
    assert_eq!(users[0].display_name.as_deref(), Some("Alice"));
}

// ── AP-3: Approve nonexistent code ─────────────────────────────────

#[tokio::test]
async fn ap3_approve_nonexistent_code() {
    let (svc, _repo, _bc) = setup().await;
    let err = svc.approve_pairing("000000").await.unwrap_err();
    assert!(matches!(err, ChannelError::PairingNotFound(_)));
}

// ── AP-4: Approve expired code ─────────────────────────────────────

#[tokio::test]
async fn ap4_approve_expired_code() {
    let (_svc, repo, bc) = setup().await;
    let svc = PairingService::new(repo.clone(), bc.clone());

    let expired_row = PairingCodeRow {
        code: "999999".into(),
        platform_user_id: "u1".into(),
        platform_type: "telegram".into(),
        channel_id: None,
        display_name: None,
        requested_at: 1000,
        expires_at: 1001,
        status: "pending".into(),
    };
    repo.create_pairing(&expired_row).await.unwrap();

    let err = svc.approve_pairing("999999").await.unwrap_err();
    assert!(matches!(err, ChannelError::PairingExpired(_)));
}

// ── AP-5: Double approve returns already processed ─────────────────

#[tokio::test]
async fn ap5_double_approve_returns_already_processed() {
    let (svc, _repo, _bc) = setup().await;
    let code = svc.request_pairing("u1", "telegram", CH_TG, None).await.unwrap();
    svc.approve_pairing(&code).await.unwrap();

    let err = svc.approve_pairing(&code).await.unwrap_err();
    assert!(matches!(err, ChannelError::PairingAlreadyProcessed(_)));
}

// ── AP-6: Missing code field (validated by DTO layer, but test via service)

#[tokio::test]
async fn ap6_empty_code_returns_not_found() {
    let (svc, _repo, _bc) = setup().await;
    let err = svc.approve_pairing("").await.unwrap_err();
    assert!(matches!(err, ChannelError::PairingNotFound(_)));
}

// ── RP-1: Reject valid pairing ─────────────────────────────────────

#[tokio::test]
async fn rp1_reject_valid_pairing() {
    let (svc, repo, _bc) = setup().await;
    let code = svc.request_pairing("u1", "telegram", CH_TG, None).await.unwrap();

    svc.reject_pairing(&code).await.unwrap();

    let row = repo.get_pairing_by_code(&code).await.unwrap().unwrap();
    assert_eq!(row.status, "rejected");
}

// ── RP-2: Rejected code not in pending list ────────────────────────

#[tokio::test]
async fn rp2_rejected_not_in_pending() {
    let (svc, _repo, _bc) = setup().await;
    let code = svc.request_pairing("u1", "telegram", CH_TG, None).await.unwrap();
    svc.reject_pairing(&code).await.unwrap();

    let pending = svc.get_pending_pairings().await.unwrap();
    assert!(pending.is_empty());
}

// ── RP-3: Reject nonexistent code ──────────────────────────────────

#[tokio::test]
async fn rp3_reject_nonexistent_code() {
    let (svc, _repo, _bc) = setup().await;
    let err = svc.reject_pairing("000000").await.unwrap_err();
    assert!(matches!(err, ChannelError::PairingNotFound(_)));
}

// ── RP-4: Reject already approved code ─────────────────────────────

#[tokio::test]
async fn rp4_reject_already_approved() {
    let (svc, _repo, _bc) = setup().await;
    let code = svc.request_pairing("u1", "telegram", CH_TG, None).await.unwrap();
    svc.approve_pairing(&code).await.unwrap();

    let err = svc.reject_pairing(&code).await.unwrap_err();
    assert!(matches!(err, ChannelError::PairingAlreadyProcessed(_)));
}

// ── EC-1: Expired codes cleaned up ─────────────────────────────────

#[tokio::test]
async fn ec1_expired_codes_cleaned_up() {
    let (_svc, repo, bc) = setup().await;
    let _svc = PairingService::new(repo.clone(), bc.clone());

    let expired_row = PairingCodeRow {
        code: "111111".into(),
        platform_user_id: "u1".into(),
        platform_type: "telegram".into(),
        channel_id: None,
        display_name: None,
        requested_at: 1000,
        expires_at: 2000,
        status: "pending".into(),
    };
    repo.create_pairing(&expired_row).await.unwrap();

    let count = repo.cleanup_expired_pairings(now_ms()).await.unwrap();
    assert_eq!(count, 1);

    let row = repo.get_pairing_by_code("111111").await.unwrap().unwrap();
    assert_eq!(row.status, "expired");
}

// ── EC-2: Non-expired codes unaffected by cleanup ──────────────────

#[tokio::test]
async fn ec2_non_expired_unaffected() {
    let (svc, repo, _bc) = setup().await;
    let code = svc.request_pairing("u1", "telegram", CH_TG, None).await.unwrap();

    let count = repo.cleanup_expired_pairings(now_ms()).await.unwrap();
    assert_eq!(count, 0);

    let row = repo.get_pairing_by_code(&code).await.unwrap().unwrap();
    assert_eq!(row.status, "pending");
}

// ── DC-3: Same platform user unique constraint ─────────────────────

#[tokio::test]
async fn dc3_same_platform_user_unique() {
    let (svc, _repo, _bc) = setup().await;

    // Approve first pairing
    let code1 = svc.request_pairing("tg_42", "telegram", CH_TG, Some("Alice")).await.unwrap();
    svc.approve_pairing(&code1).await.unwrap();

    // Second pairing for same user should fail on user creation (unique constraint)
    let code2 = svc.request_pairing("tg_42", "telegram", CH_TG, Some("Alice")).await.unwrap();
    let result = svc.approve_pairing(&code2).await;
    // DB should reject duplicate (platform_user_id, platform_type)
    assert!(result.is_err());
}

// ── WS-1: Pairing request broadcasts event ─────────────────────────

#[tokio::test]
async fn ws1_pairing_request_broadcasts_event() {
    let (svc, _repo, bc) = setup().await;
    svc.request_pairing("tg_42", "telegram", CH_TG, Some("Alice")).await.unwrap();

    let events = bc.take_events();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].name, "channel.pairing-requested");
    assert_eq!(events[0].data["platform_user_id"], "tg_42");
    assert_eq!(events[0].data["platform_type"], "telegram");
    assert_eq!(events[0].data["display_name"], "Alice");
    assert!(events[0].data["code"].is_string());
    assert!(events[0].data["expires_at"].is_number());
}

// ── WS-3: Approve broadcasts user-authorized event ─────────────────

#[tokio::test]
async fn ws3_approve_broadcasts_user_authorized() {
    let (svc, _repo, bc) = setup().await;
    let code = svc.request_pairing("tg_42", "telegram", CH_TG, Some("Alice")).await.unwrap();
    bc.take_events(); // clear request event

    svc.approve_pairing(&code).await.unwrap();

    let events = bc.take_events();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].name, "channel.user-authorized");
    assert_eq!(events[0].data["platform_user_id"], "tg_42");
    assert_eq!(events[0].data["platform_type"], "telegram");
    assert_eq!(events[0].data["display_name"], "Alice");
    assert!(events[0].data["id"].is_string());
}

// ── is_user_authorized ─────────────────────────────────────────────

#[tokio::test]
async fn is_user_authorized_false_before_approval() {
    let (svc, _repo, _bc) = setup().await;
    assert!(!svc.is_user_authorized("tg_42", "telegram", CH_TG).await.unwrap());
}

#[tokio::test]
async fn is_user_authorized_true_after_approval() {
    let (svc, _repo, _bc) = setup().await;
    let code = svc.request_pairing("tg_42", "telegram", CH_TG, None).await.unwrap();
    svc.approve_pairing(&code).await.unwrap();

    assert!(svc.is_user_authorized("tg_42", "telegram", CH_TG).await.unwrap());
}

#[tokio::test]
async fn is_user_authorized_different_platform_false() {
    let (svc, _repo, _bc) = setup().await;
    let code = svc.request_pairing("tg_42", "telegram", CH_TG, None).await.unwrap();
    svc.approve_pairing(&code).await.unwrap();

    // Same user ID but different platform
    assert!(!svc.is_user_authorized("tg_42", "lark", CH_LARK).await.unwrap());
}

// ── Two lark bots pair independently (per-bot channel isolation) ────
//
// Regression for the per-bot pairing scoping on this branch: pairing and
// authorization are keyed by channel_id (the specific bot a message arrived
// through), not just by platform_type. Two lark bots must pair entirely
// independently — approving one must not authorize the other, and the other's
// pending code must be untouched.
#[tokio::test]
async fn two_lark_bots_pair_independently() {
    let (svc, repo, _bc) = setup().await;

    // setup() seeds CH_LARK (lark-1). Seed a second lark bot so the
    // channel_id FK (assistant_pairing_codes/assistant_users → assistant_plugins)
    // is satisfied for bot 2. Same upsert_plugin pattern setup() uses.
    repo.upsert_plugin(&ChannelPluginRow {
        id: CH_LARK2.into(),
        r#type: "lark".into(),
        name: "Lark Bot 2".into(),
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

    // Two distinct lark users each initiate pairing, one per bot. Distinct
    // open_ids mirror reality (Lark open_id is per-app) and keep the test
    // focused on channel isolation rather than same-user expiry behavior.
    let code1 = svc.request_pairing("ou_a", "lark", CH_LARK, Some("A")).await.unwrap();
    let code2 = svc.request_pairing("ou_b", "lark", CH_LARK2, Some("B")).await.unwrap();
    assert_ne!(code1, code2);

    // Both codes are pending simultaneously, each carrying its own channel_id.
    let pending = svc.get_pending_pairings().await.unwrap();
    assert_eq!(pending.len(), 2, "both bots' pairings should be pending");
    let p1 = pending
        .iter()
        .find(|p| p.code == code1)
        .expect("bot 1 code pending");
    let p2 = pending
        .iter()
        .find(|p| p.code == code2)
        .expect("bot 2 code pending");
    assert_eq!(p1.channel_id.as_deref(), Some(CH_LARK));
    assert_eq!(p1.platform_user_id, "ou_a");
    assert_eq!(p2.channel_id.as_deref(), Some(CH_LARK2));
    assert_eq!(p2.platform_user_id, "ou_b");

    // Approve only bot 1's pairing.
    svc.approve_pairing(&code1).await.unwrap();

    // Bot 1's user is now authorized — but only on bot 1's channel.
    assert!(
        svc.is_user_authorized("ou_a", "lark", CH_LARK).await.unwrap(),
        "approved user must be authorized on bot 1"
    );

    // Bot 2 is entirely unaffected: its user is not authorized and its code
    // is still pending.
    assert!(
        !svc.is_user_authorized("ou_b", "lark", CH_LARK2).await.unwrap(),
        "bot 2's user must NOT be authorized by bot 1's approval"
    );
    let pending_after = svc.get_pending_pairings().await.unwrap();
    assert_eq!(pending_after.len(), 1, "bot 2's pairing should remain pending");
    assert_eq!(pending_after[0].code, code2);
    assert_eq!(pending_after[0].channel_id.as_deref(), Some(CH_LARK2));
}

use std::sync::Arc;

use nomifun_api_types::{PairingRequestedPayload, UserAuthorizedPayload, WebSocketMessage};
use nomifun_common::{TimestampMs, generate_prefixed_id, now_ms};
use nomifun_db::IChannelRepository;
use nomifun_db::models::{AssistantUserRow, PairingCodeRow};
use nomifun_realtime::EventBroadcaster;
use tokio::task::JoinHandle;
use tracing::{debug, info, warn};

use crate::constants::{PAIRING_CLEANUP_INTERVAL, PAIRING_CODE_LENGTH, PAIRING_CODE_TTL};
use crate::error::ChannelError;
use crate::types::PairingStatus;

/// Generates a random numeric pairing code of the configured length.
///
/// Uses `getrandom` for cryptographically secure randomness.
/// Returns a zero-padded string (e.g., "003421").
pub fn generate_pairing_code() -> Result<String, ChannelError> {
    let mut bytes = [0u8; 4];
    getrandom::getrandom(&mut bytes).map_err(|e| ChannelError::InvalidConfig(format!("RNG failure: {e}")))?;
    let num = u32::from_le_bytes(bytes) % 10u32.pow(PAIRING_CODE_LENGTH as u32);
    Ok(format!("{num:0>width$}", width = PAIRING_CODE_LENGTH))
}

/// Service for managing pairing authorization flow.
///
/// Handles:
/// - Pairing code generation and creation
/// - Approval / rejection of pairing requests
/// - Periodic cleanup of expired codes
/// - Event broadcasting to WebSocket clients
pub struct PairingService {
    repo: Arc<dyn IChannelRepository>,
    broadcaster: Arc<dyn EventBroadcaster>,
}

impl PairingService {
    pub fn new(repo: Arc<dyn IChannelRepository>, broadcaster: Arc<dyn EventBroadcaster>) -> Self {
        Self { repo, broadcaster }
    }

    /// Creates a pairing request for an IM user.
    ///
    /// Generates a 6-digit code, stores it with a 10-minute TTL, and
    /// broadcasts a `channel.pairing-requested` event to all WebSocket
    /// clients.
    ///
    /// If the same platform user already has a pending code, that code is
    /// marked as expired before creating the new one.
    pub async fn request_pairing(
        &self,
        platform_user_id: &str,
        platform_type: &str,
        channel_id: &str,
        display_name: Option<&str>,
    ) -> Result<String, ChannelError> {
        // Expire any existing pending codes for this user on this bot channel
        self.expire_user_pending_codes(platform_user_id, platform_type, channel_id)
            .await?;

        let code = generate_pairing_code()?;
        let now = now_ms();
        let expires_at = now + PAIRING_CODE_TTL.as_millis() as TimestampMs;

        let row = PairingCodeRow {
            code: code.clone(),
            platform_user_id: platform_user_id.to_owned(),
            platform_type: platform_type.to_owned(),
            channel_id: Some(channel_id.to_owned()),
            display_name: display_name.map(String::from),
            requested_at: now,
            expires_at,
            status: PairingStatus::Pending.to_string(),
        };

        self.repo.create_pairing(&row).await?;

        info!(
            code = %code,
            platform_user_id = %platform_user_id,
            platform_type = %platform_type,
            channel_id = %channel_id,
            "pairing code created"
        );

        // Broadcast event
        let payload = PairingRequestedPayload {
            code: code.clone(),
            platform_user_id: platform_user_id.to_owned(),
            platform_type: platform_type.to_owned(),
            channel_id: Some(channel_id.to_owned()),
            display_name: display_name.map(String::from),
            expires_at,
        };
        let value = serde_json::to_value(payload)?;
        self.broadcaster
            .broadcast(WebSocketMessage::new("channel.pairing-requested", value));

        Ok(code)
    }

    /// Approves a pending pairing code.
    ///
    /// - Validates the code exists and is still pending + not expired
    /// - Creates an `assistant_users` record
    /// - Updates the pairing status to `approved`
    /// - Broadcasts a `channel.user-authorized` event
    pub async fn approve_pairing(&self, code: &str) -> Result<(), ChannelError> {
        let row = self.get_valid_pending_pairing(code).await?;
        let now = now_ms();

        // Create user record. `achu_` (assistant-channel user) keeps these IM
        // identities in their own id namespace, distinct from the `users`
        // table — see the primary-key redesign spec.
        let user_id = generate_prefixed_id("achu");
        let user_row = AssistantUserRow {
            id: user_id.clone(),
            platform_user_id: row.platform_user_id.clone(),
            platform_type: row.platform_type.clone(),
            channel_id: row.channel_id.clone(),
            display_name: row.display_name.clone(),
            authorized_at: now,
            last_active: None,
            session_id: None,
        };
        self.repo.create_user(&user_row).await?;

        // Update pairing status
        self.repo
            .update_pairing_status(code, &PairingStatus::Approved.to_string())
            .await?;

        info!(
            code = %code,
            user_id = %user_id,
            platform_user_id = %row.platform_user_id,
            "pairing approved, user created"
        );

        // Broadcast event
        let payload = UserAuthorizedPayload {
            id: user_id,
            platform_user_id: row.platform_user_id,
            platform_type: row.platform_type,
            channel_id: row.channel_id,
            display_name: row.display_name,
        };
        let value = serde_json::to_value(payload)?;
        self.broadcaster
            .broadcast(WebSocketMessage::new("channel.user-authorized", value));

        Ok(())
    }

    /// Rejects a pending pairing code.
    ///
    /// Validates the code exists and is still pending (not expired or
    /// already processed), then marks it as rejected.
    pub async fn reject_pairing(&self, code: &str) -> Result<(), ChannelError> {
        let _row = self.get_valid_pending_pairing(code).await?;

        self.repo
            .update_pairing_status(code, &PairingStatus::Rejected.to_string())
            .await?;

        info!(code = %code, "pairing rejected");
        Ok(())
    }

    /// Returns all pending (not expired) pairing requests.
    pub async fn get_pending_pairings(&self) -> Result<Vec<PairingCodeRow>, ChannelError> {
        let rows = self.repo.get_pending_pairings().await?;
        let now = now_ms();
        // Filter out expired ones that haven't been cleaned up yet
        let active: Vec<PairingCodeRow> = rows.into_iter().filter(|r| r.expires_at > now).collect();
        Ok(active)
    }

    /// Checks whether a platform user is already authorized on this bot channel.
    pub async fn is_user_authorized(
        &self,
        platform_user_id: &str,
        platform_type: &str,
        channel_id: &str,
    ) -> Result<bool, ChannelError> {
        let user = self
            .repo
            .get_user_by_platform(platform_user_id, platform_type, channel_id)
            .await?;
        Ok(user.is_some())
    }

    /// Looks up the internal user ID for a platform user on this bot channel.
    ///
    /// Returns `None` if the user is not authorized.
    pub async fn get_internal_user_id(
        &self,
        platform_user_id: &str,
        platform_type: &str,
        channel_id: &str,
    ) -> Result<Option<String>, ChannelError> {
        let user = self
            .repo
            .get_user_by_platform(platform_user_id, platform_type, channel_id)
            .await?;
        Ok(user.map(|u| u.id))
    }

    /// Get-or-create the internal `assistant_users` id for a platform sender on
    /// this bot channel, WITHOUT a pairing code — used by the public-agent
    /// auto-serve path (a bot bound to a public agent serves strangers directly
    /// because the session is hard-clamped). Returns the `achu_` id, which
    /// `assistant_sessions.user_id` foreign-keys to. Idempotent: a returning
    /// stranger reuses their row. NOT used for companion/unbound bots — those
    /// still require explicit pairing approval.
    pub async fn ensure_channel_user(
        &self,
        platform_user_id: &str,
        platform_type: &str,
        channel_id: &str,
        display_name: &str,
    ) -> Result<String, ChannelError> {
        if let Some(user) = self
            .repo
            .get_user_by_platform(platform_user_id, platform_type, channel_id)
            .await?
        {
            return Ok(user.id);
        }
        let user_id = generate_prefixed_id("achu");
        let user_row = AssistantUserRow {
            id: user_id.clone(),
            platform_user_id: platform_user_id.to_owned(),
            platform_type: platform_type.to_owned(),
            channel_id: Some(channel_id.to_owned()),
            display_name: Some(display_name.to_owned()),
            authorized_at: now_ms(),
            last_active: None,
            session_id: None,
        };
        self.repo.create_user(&user_row).await?;
        info!(
            user_id = %user_id,
            platform_user_id = %platform_user_id,
            channel_id = %channel_id,
            "public-agent channel auto-registered a stranger (no pairing)"
        );
        Ok(user_id)
    }

    /// Starts a background task that periodically cleans up expired
    /// pairing codes. Returns a `JoinHandle` that can be used to cancel
    /// the task on shutdown.
    pub fn start_cleanup_timer(repo: Arc<dyn IChannelRepository>) -> JoinHandle<()> {
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(PAIRING_CLEANUP_INTERVAL);
            loop {
                interval.tick().await;
                let now = now_ms();
                match repo.cleanup_expired_pairings(now).await {
                    Ok(count) if count > 0 => {
                        debug!(count, "cleaned up expired pairing codes");
                    }
                    Ok(_) => {}
                    Err(e) => {
                        warn!(error = %e, "failed to clean up expired pairings");
                    }
                }
            }
        })
    }

    /// Validates that a pairing code exists, is pending, and not expired.
    async fn get_valid_pending_pairing(&self, code: &str) -> Result<PairingCodeRow, ChannelError> {
        let row = self
            .repo
            .get_pairing_by_code(code)
            .await?
            .ok_or_else(|| ChannelError::PairingNotFound(code.to_owned()))?;

        if row.status != PairingStatus::Pending.to_string() {
            return Err(ChannelError::PairingAlreadyProcessed(code.to_owned()));
        }

        let now = now_ms();
        if row.expires_at <= now {
            // Mark as expired for consistency
            let _ = self
                .repo
                .update_pairing_status(code, &PairingStatus::Expired.to_string())
                .await;
            return Err(ChannelError::PairingExpired(code.to_owned()));
        }

        Ok(row)
    }

    /// Expires any pending codes for the given platform user.
    ///
    /// Called before creating a new code to ensure only one active code
    /// per user at a time.
    async fn expire_user_pending_codes(
        &self,
        platform_user_id: &str,
        platform_type: &str,
        channel_id: &str,
    ) -> Result<(), ChannelError> {
        let pending = self.repo.get_pending_pairings().await?;
        for row in pending {
            if row.platform_user_id == platform_user_id
                && row.platform_type == platform_type
                && row.channel_id.as_deref() == Some(channel_id)
            {
                self.repo
                    .update_pairing_status(&row.code, &PairingStatus::Expired.to_string())
                    .await?;
                debug!(
                    code = %row.code,
                    "expired old pending code for user"
                );
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nomifun_db::models::{AssistantSessionRow, AssistantUserRow, ChannelPluginRow, PairingCodeRow};
    use nomifun_db::{DbError, IChannelRepository, UpdatePluginStatusParams};
    use std::sync::Mutex;

    // ── Mock EventBroadcaster ──────────────────────────────────────────

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

    // ── Mock IChannelRepository ────────────────────────────────────────

    struct MockRepo {
        pairings: Mutex<Vec<PairingCodeRow>>,
        users: Mutex<Vec<AssistantUserRow>>,
    }

    impl MockRepo {
        fn new() -> Self {
            Self {
                pairings: Mutex::new(Vec::new()),
                users: Mutex::new(Vec::new()),
            }
        }

        fn get_pairings(&self) -> Vec<PairingCodeRow> {
            self.pairings.lock().unwrap().clone()
        }

        fn get_users(&self) -> Vec<AssistantUserRow> {
            self.users.lock().unwrap().clone()
        }
    }

    #[async_trait::async_trait]
    impl IChannelRepository for MockRepo {
        // -- Plugin CRUD (unused stubs) --

        async fn get_all_plugins(&self) -> Result<Vec<ChannelPluginRow>, DbError> {
            Ok(vec![])
        }
        async fn get_plugin(&self, _id: &str) -> Result<Option<ChannelPluginRow>, DbError> {
            Ok(None)
        }
        async fn upsert_plugin(&self, _row: &ChannelPluginRow) -> Result<(), DbError> {
            Ok(())
        }
        async fn update_plugin_status(&self, _id: &str, _params: &UpdatePluginStatusParams) -> Result<(), DbError> {
            Ok(())
        }
        async fn update_plugin_companion(&self, _id: &str, _companion_id: Option<&str>) -> Result<(), DbError> {
            Ok(())
        }
        async fn update_plugin_public_agent(&self, _id: &str, _public_agent_id: Option<&str>) -> Result<(), DbError> {
            Ok(())
        }
        async fn update_plugin_bot_key(&self, _id: &str, _bot_key: &str) -> Result<(), DbError> {
            Ok(())
        }
        async fn delete_plugin(&self, _id: &str) -> Result<(), DbError> {
            Ok(())
        }

        // -- User CRUD --

        async fn get_all_users(&self) -> Result<Vec<AssistantUserRow>, DbError> {
            Ok(self.users.lock().unwrap().clone())
        }

        async fn get_user_by_platform(
            &self,
            platform_user_id: &str,
            platform_type: &str,
            channel_id: &str,
        ) -> Result<Option<AssistantUserRow>, DbError> {
            let users = self.users.lock().unwrap();
            Ok(users
                .iter()
                .find(|u| {
                    u.platform_user_id == platform_user_id
                        && u.platform_type == platform_type
                        && u.channel_id.as_deref() == Some(channel_id)
                })
                .cloned())
        }

        async fn create_user(&self, row: &AssistantUserRow) -> Result<(), DbError> {
            let mut users = self.users.lock().unwrap();
            if users.iter().any(|u| {
                u.platform_user_id == row.platform_user_id
                    && u.platform_type == row.platform_type
                    && u.channel_id == row.channel_id
            }) {
                return Err(DbError::Conflict("user already exists".into()));
            }
            users.push(row.clone());
            Ok(())
        }

        async fn update_user_last_active(&self, id: &str, last_active: TimestampMs) -> Result<(), DbError> {
            let mut users = self.users.lock().unwrap();
            if let Some(u) = users.iter_mut().find(|u| u.id == id) {
                u.last_active = Some(last_active);
                Ok(())
            } else {
                Err(DbError::NotFound(id.into()))
            }
        }

        async fn delete_user(&self, id: &str) -> Result<(), DbError> {
            let mut users = self.users.lock().unwrap();
            let len_before = users.len();
            users.retain(|u| u.id != id);
            if users.len() == len_before {
                Err(DbError::NotFound(id.into()))
            } else {
                Ok(())
            }
        }

        // -- Session CRUD (unused stubs) --

        async fn get_all_sessions(&self) -> Result<Vec<AssistantSessionRow>, DbError> {
            Ok(vec![])
        }
        async fn get_session(&self, _id: &str) -> Result<Option<AssistantSessionRow>, DbError> {
            Ok(None)
        }
        async fn get_or_create_session(
            &self,
            _user_id: &str,
            _chat_id: &str,
            _channel_id: &str,
            new_row: &AssistantSessionRow,
        ) -> Result<AssistantSessionRow, DbError> {
            Ok(new_row.clone())
        }
        async fn update_session_activity(&self, _id: &str, _last_activity: TimestampMs) -> Result<(), DbError> {
            Ok(())
        }
        async fn update_session_conversation(&self, _id: &str, _conversation_id: i64) -> Result<(), DbError> {
            Ok(())
        }
        async fn update_session_agent_type(&self, _id: &str, _agent_type: &str) -> Result<(), DbError> {
            Ok(())
        }
        async fn delete_sessions_by_user(&self, _user_id: &str) -> Result<(), DbError> {
            Ok(())
        }
        async fn delete_sessions_by_channel(&self, _channel_id: &str) -> Result<(), DbError> {
            Ok(())
        }
        async fn delete_session_by_user_chat(
            &self,
            _user_id: &str,
            _chat_id: &str,
            _channel_id: &str,
        ) -> Result<(), DbError> {
            Ok(())
        }

        // -- Pairing codes --

        async fn create_pairing(&self, row: &PairingCodeRow) -> Result<(), DbError> {
            let mut pairings = self.pairings.lock().unwrap();
            if pairings.iter().any(|p| p.code == row.code) {
                return Err(DbError::Conflict("duplicate code".into()));
            }
            pairings.push(row.clone());
            Ok(())
        }

        async fn get_pending_pairings(&self) -> Result<Vec<PairingCodeRow>, DbError> {
            let pairings = self.pairings.lock().unwrap();
            Ok(pairings.iter().filter(|p| p.status == "pending").cloned().collect())
        }

        async fn get_pairing_by_code(&self, code: &str) -> Result<Option<PairingCodeRow>, DbError> {
            let pairings = self.pairings.lock().unwrap();
            Ok(pairings.iter().find(|p| p.code == code).cloned())
        }

        async fn update_pairing_status(&self, code: &str, status: &str) -> Result<(), DbError> {
            let mut pairings = self.pairings.lock().unwrap();
            if let Some(p) = pairings.iter_mut().find(|p| p.code == code) {
                p.status = status.to_owned();
                Ok(())
            } else {
                Err(DbError::NotFound(code.into()))
            }
        }

        async fn cleanup_expired_pairings(&self, now: TimestampMs) -> Result<u64, DbError> {
            let mut pairings = self.pairings.lock().unwrap();
            let mut count = 0u64;
            for p in pairings.iter_mut() {
                if p.status == "pending" && p.expires_at <= now {
                    p.status = "expired".into();
                    count += 1;
                }
            }
            Ok(count)
        }
    }

    // ── Helpers ────────────────────────────────────────────────────────

    fn make_service() -> (PairingService, Arc<MockRepo>, Arc<MockBroadcaster>) {
        let repo = Arc::new(MockRepo::new());
        let broadcaster = Arc::new(MockBroadcaster::new());
        let svc = PairingService::new(repo.clone(), broadcaster.clone());
        (svc, repo, broadcaster)
    }

    // ── generate_pairing_code ──────────────────────────────────────────

    #[test]
    fn code_has_correct_length() {
        let code = generate_pairing_code().unwrap();
        assert_eq!(code.len(), PAIRING_CODE_LENGTH);
    }

    #[test]
    fn code_is_all_digits() {
        let code = generate_pairing_code().unwrap();
        assert!(code.chars().all(|c| c.is_ascii_digit()));
    }

    #[test]
    fn code_is_zero_padded() {
        // Generate many codes; at least some should start with '0' statistically,
        // but more importantly verify format consistency.
        for _ in 0..100 {
            let code = generate_pairing_code().unwrap();
            assert_eq!(code.len(), PAIRING_CODE_LENGTH);
            assert!(code.chars().all(|c| c.is_ascii_digit()));
        }
    }

    #[test]
    fn codes_are_not_all_identical() {
        let codes: std::collections::HashSet<String> = (0..50).map(|_| generate_pairing_code().unwrap()).collect();
        // With 6-digit codes, 50 random samples should produce > 1 unique
        assert!(codes.len() > 1);
    }

    // ── request_pairing ────────────────────────────────────────────────

    #[tokio::test]
    async fn request_pairing_creates_code() {
        let (svc, repo, _bc) = make_service();
        let code = svc.request_pairing("tg_42", "telegram", "achn_1", Some("Alice")).await.unwrap();
        assert_eq!(code.len(), PAIRING_CODE_LENGTH);

        let pairings = repo.get_pairings();
        assert_eq!(pairings.len(), 1);
        assert_eq!(pairings[0].code, code);
        assert_eq!(pairings[0].platform_user_id, "tg_42");
        assert_eq!(pairings[0].platform_type, "telegram");
        assert_eq!(pairings[0].display_name.as_deref(), Some("Alice"));
        assert_eq!(pairings[0].status, "pending");
    }

    #[tokio::test]
    async fn request_pairing_broadcasts_event() {
        let (svc, _repo, bc) = make_service();
        svc.request_pairing("tg_42", "telegram", "achn_1", Some("Alice")).await.unwrap();

        let events = bc.take_events();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].name, "channel.pairing-requested");
        assert_eq!(events[0].data["platform_user_id"], "tg_42");
        assert_eq!(events[0].data["platform_type"], "telegram");
        assert_eq!(events[0].data["display_name"], "Alice");
    }

    #[tokio::test]
    async fn request_pairing_sets_correct_expiry() {
        let (svc, repo, _bc) = make_service();
        let before = now_ms();
        svc.request_pairing("u1", "lark", "achn_1", None).await.unwrap();
        let after = now_ms();

        let p = &repo.get_pairings()[0];
        let expected_ttl = PAIRING_CODE_TTL.as_millis() as TimestampMs;
        assert!(p.expires_at >= before + expected_ttl);
        assert!(p.expires_at <= after + expected_ttl);
    }

    #[tokio::test]
    async fn request_pairing_expires_old_code() {
        let (svc, repo, _bc) = make_service();

        let code1 = svc.request_pairing("tg_42", "telegram", "achn_1", Some("Alice")).await.unwrap();
        let code2 = svc.request_pairing("tg_42", "telegram", "achn_1", Some("Alice")).await.unwrap();

        assert_ne!(code1, code2);

        let pairings = repo.get_pairings();
        let old = pairings.iter().find(|p| p.code == code1).unwrap();
        let new = pairings.iter().find(|p| p.code == code2).unwrap();
        assert_eq!(old.status, "expired");
        assert_eq!(new.status, "pending");
    }

    #[tokio::test]
    async fn request_pairing_no_display_name() {
        let (svc, repo, _bc) = make_service();
        svc.request_pairing("u1", "dingtalk", "achn_1", None).await.unwrap();

        let pairings = repo.get_pairings();
        assert!(pairings[0].display_name.is_none());
    }

    // ── approve_pairing ────────────────────────────────────────────────

    #[tokio::test]
    async fn approve_creates_user_and_updates_status() {
        let (svc, repo, _bc) = make_service();
        let code = svc.request_pairing("tg_42", "telegram", "achn_1", Some("Alice")).await.unwrap();

        svc.approve_pairing(&code).await.unwrap();

        // Check pairing status
        let pairings = repo.get_pairings();
        let p = pairings.iter().find(|p| p.code == code).unwrap();
        assert_eq!(p.status, "approved");

        // Check user created
        let users = repo.get_users();
        assert_eq!(users.len(), 1);
        assert_eq!(users[0].platform_user_id, "tg_42");
        assert_eq!(users[0].platform_type, "telegram");
        assert_eq!(users[0].display_name.as_deref(), Some("Alice"));
    }

    #[tokio::test]
    async fn approve_broadcasts_user_authorized() {
        let (svc, _repo, bc) = make_service();
        let code = svc.request_pairing("tg_42", "telegram", "achn_1", Some("Alice")).await.unwrap();
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

    #[tokio::test]
    async fn approve_nonexistent_code_returns_not_found() {
        let (svc, _repo, _bc) = make_service();
        let err = svc.approve_pairing("000000").await.unwrap_err();
        assert!(matches!(err, ChannelError::PairingNotFound(_)));
    }

    #[tokio::test]
    async fn approve_already_approved_returns_already_processed() {
        let (svc, _repo, _bc) = make_service();
        let code = svc.request_pairing("tg_42", "telegram", "achn_1", None).await.unwrap();
        svc.approve_pairing(&code).await.unwrap();

        let err = svc.approve_pairing(&code).await.unwrap_err();
        assert!(matches!(err, ChannelError::PairingAlreadyProcessed(_)));
    }

    #[tokio::test]
    async fn approve_expired_code_returns_expired() {
        let (svc, repo, _bc) = make_service();
        // Manually insert an already-expired code
        let row = PairingCodeRow {
            code: "999999".into(),
            platform_user_id: "u1".into(),
            platform_type: "telegram".into(),
            channel_id: None,
            display_name: None,
            requested_at: 1000,
            expires_at: 1001, // long expired
            status: "pending".into(),
        };
        repo.pairings.lock().unwrap().push(row);

        let err = svc.approve_pairing("999999").await.unwrap_err();
        assert!(matches!(err, ChannelError::PairingExpired(_)));
    }

    // ── reject_pairing ─────────────────────────────────────────────────

    #[tokio::test]
    async fn reject_updates_status() {
        let (svc, repo, _bc) = make_service();
        let code = svc.request_pairing("tg_42", "telegram", "achn_1", None).await.unwrap();

        svc.reject_pairing(&code).await.unwrap();

        let pairings = repo.get_pairings();
        let p = pairings.iter().find(|p| p.code == code).unwrap();
        assert_eq!(p.status, "rejected");
    }

    #[tokio::test]
    async fn reject_nonexistent_code_returns_not_found() {
        let (svc, _repo, _bc) = make_service();
        let err = svc.reject_pairing("000000").await.unwrap_err();
        assert!(matches!(err, ChannelError::PairingNotFound(_)));
    }

    #[tokio::test]
    async fn reject_already_approved_returns_already_processed() {
        let (svc, _repo, _bc) = make_service();
        let code = svc.request_pairing("tg_42", "telegram", "achn_1", None).await.unwrap();
        svc.approve_pairing(&code).await.unwrap();

        let err = svc.reject_pairing(&code).await.unwrap_err();
        assert!(matches!(err, ChannelError::PairingAlreadyProcessed(_)));
    }

    // ── get_pending_pairings ───────────────────────────────────────────

    #[tokio::test]
    async fn get_pending_filters_expired() {
        let (svc, repo, _bc) = make_service();

        // Insert valid pending code
        svc.request_pairing("u1", "telegram", "achn_1", None).await.unwrap();

        // Insert manually expired code
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
        repo.pairings.lock().unwrap().push(expired_row);

        let pending = svc.get_pending_pairings().await.unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].platform_user_id, "u1");
    }

    #[tokio::test]
    async fn get_pending_empty_when_none() {
        let (svc, _repo, _bc) = make_service();
        let pending = svc.get_pending_pairings().await.unwrap();
        assert!(pending.is_empty());
    }

    // ── is_user_authorized ─────────────────────────────────────────────

    #[tokio::test]
    async fn unauthorized_user_returns_false() {
        let (svc, _repo, _bc) = make_service();
        let authorized = svc.is_user_authorized("tg_42", "telegram", "achn_1").await.unwrap();
        assert!(!authorized);
    }

    #[tokio::test]
    async fn authorized_user_returns_true_after_approval() {
        let (svc, _repo, _bc) = make_service();
        let code = svc.request_pairing("tg_42", "telegram", "achn_1", None).await.unwrap();
        svc.approve_pairing(&code).await.unwrap();

        let authorized = svc.is_user_authorized("tg_42", "telegram", "achn_1").await.unwrap();
        assert!(authorized);
    }

    #[tokio::test]
    async fn two_channels_same_user_pair_independently() {
        let (svc, repo, _bc) = make_service();
        let c1 = svc.request_pairing("ou_same", "lark", "achn_1", Some("U")).await.unwrap();
        let c2 = svc.request_pairing("ou_same", "lark", "achn_2", Some("U")).await.unwrap();
        let pend = repo.get_pairings();
        assert_eq!(pend.iter().find(|p| p.code == c1).unwrap().status, "pending");
        assert_eq!(pend.iter().find(|p| p.code == c2).unwrap().status, "pending");
        svc.approve_pairing(&c1).await.unwrap();
        assert!(svc.is_user_authorized("ou_same", "lark", "achn_1").await.unwrap());
        assert!(!svc.is_user_authorized("ou_same", "lark", "achn_2").await.unwrap());
    }

    // ── cleanup_expired_pairings (via repo directly) ───────────────────

    #[tokio::test]
    async fn cleanup_marks_expired_as_expired() {
        let (svc, repo, _bc) = make_service();

        // Insert manually expired pending code
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
        repo.pairings.lock().unwrap().push(expired_row);

        // Insert valid pending code
        svc.request_pairing("u2", "lark", "achn_1", None).await.unwrap();

        let count = repo.cleanup_expired_pairings(now_ms()).await.unwrap();
        assert_eq!(count, 1);

        let pairings = repo.get_pairings();
        let expired = pairings.iter().find(|p| p.code == "111111").unwrap();
        assert_eq!(expired.status, "expired");
    }

    // ── start_cleanup_timer ────────────────────────────────────────────

    fn make_expired_row(code: &str) -> PairingCodeRow {
        PairingCodeRow {
            code: code.into(),
            platform_user_id: "u1".into(),
            platform_type: "telegram".into(),
            channel_id: None,
            display_name: None,
            requested_at: 1000,
            expires_at: 2000, // long past
            status: "pending".into(),
        }
    }

    /// Regression: `start_cleanup_timer` existed but had no caller, so the
    /// sweep never ran. This pins the timer behaviour itself — it must keep
    /// invoking `cleanup_expired_pairings` once per `PAIRING_CLEANUP_INTERVAL`
    /// (the assembly in nomifun-app now starts it at boot).
    #[tokio::test(start_paused = true)]
    async fn cleanup_timer_periodically_purges_expired_codes() {
        let repo = Arc::new(MockRepo::new());
        repo.pairings.lock().unwrap().push(make_expired_row("222222"));

        let handle = PairingService::start_cleanup_timer(repo.clone());

        // The paused clock auto-advances while the test sleeps, driving the
        // spawned interval deterministically. The first tick fires
        // immediately and purges the seeded code.
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        assert_eq!(repo.get_pairings()[0].status, "expired");

        // Seed another expired code and cross one full interval to prove
        // the sweep is periodic, not one-shot.
        repo.pairings.lock().unwrap().push(make_expired_row("333333"));
        tokio::time::sleep(PAIRING_CLEANUP_INTERVAL).await;
        let pairings = repo.get_pairings();
        let second = pairings.iter().find(|p| p.code == "333333").unwrap();
        assert_eq!(second.status, "expired");

        handle.abort();
    }
}

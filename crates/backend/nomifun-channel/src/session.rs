use std::sync::Arc;

use nomifun_common::{ChannelSessionId, ConversationId, now_ms};
use nomifun_db::{
    ChannelInboundClaim, IChannelRepository, SettleChannelInboundReceiptParams,
};
use nomifun_db::models::{
    ChannelInboundReceiptRow, ChannelSessionRow, NewChannelInboundReceiptRow,
    NewChannelSessionRow,
};
use tracing::{debug, info};

use crate::error::ChannelError;

/// Manages per-chat session isolation for channel users.
///
/// Each (channel_plugin_id, channel_user_id, chat_id) triple maps to exactly
/// one session.
/// This ensures that the same user chatting in different groups/DMs — or
/// with different bots in the same group — gets independent conversation
/// contexts, while repeated messages in the same chat reuse the existing
/// session.
pub struct SessionManager {
    repo: Arc<dyn IChannelRepository>,
}

impl SessionManager {
    pub fn new(repo: Arc<dyn IChannelRepository>) -> Self {
        Self { repo }
    }

    /// Finds an existing session for the channel+user+chat triple, or
    /// creates one.
    ///
    /// - If found: updates `last_activity` and returns the existing session.
    /// - If not found: creates a new session with the given `agent_type`.
    ///
    /// The `workspace` parameter is optional and may be set later by
    /// the `ChannelManager` when it knows the active workspace path.
    pub async fn get_or_create_session(
        &self,
        channel_user_id: &str,
        chat_id: &str,
        channel_plugin_id: &str,
        agent_type: &str,
        workspace: Option<&str>,
    ) -> Result<ChannelSessionRow, ChannelError> {
        let now = now_ms();
        let new_row = NewChannelSessionRow {
            channel_session_id: ChannelSessionId::new().into_string(),
            channel_user_id: channel_user_id.to_owned(),
            agent_type: agent_type.to_owned(),
            conversation_id: None,
            workspace: workspace.map(String::from),
            chat_id: Some(chat_id.to_owned()),
            channel_plugin_id: Some(channel_plugin_id.to_owned()),
            created_at: now,
            last_activity: now,
        };

        let session = self
            .repo
            .get_or_create_session(channel_user_id, chat_id, channel_plugin_id, &new_row)
            .await?;

        debug!(
            channel_session_id = %session.channel_session_id,
            channel_user_id,
            chat_id = %chat_id,
            channel_plugin_id,
            "session resolved"
        );

        Ok(session)
    }

    /// Returns all active sessions.
    pub async fn get_active_sessions(&self) -> Result<Vec<ChannelSessionRow>, ChannelError> {
        let sessions = self.repo.get_all_sessions().await?;
        Ok(sessions)
    }

    /// Deletes the existing session for a channel+user+chat triple and
    /// creates a fresh one. Returns the newly created session.
    ///
    /// Used by `session.new` to give the user a clean slate in a chat.
    pub async fn reset_session(
        &self,
        channel_user_id: &str,
        chat_id: &str,
        channel_plugin_id: &str,
        agent_type: &str,
        workspace: Option<&str>,
    ) -> Result<ChannelSessionRow, ChannelError> {
        // Delete old session if it exists
        self.repo
            .delete_session_by_user_chat(channel_user_id, chat_id, channel_plugin_id)
            .await?;

        // Create a fresh session
        let now = now_ms();
        let new_row = NewChannelSessionRow {
            channel_session_id: ChannelSessionId::new().into_string(),
            channel_user_id: channel_user_id.to_owned(),
            agent_type: agent_type.to_owned(),
            conversation_id: None,
            workspace: workspace.map(String::from),
            chat_id: Some(chat_id.to_owned()),
            channel_plugin_id: Some(channel_plugin_id.to_owned()),
            created_at: now,
            last_activity: now,
        };

        let session = self
            .repo
            .get_or_create_session(channel_user_id, chat_id, channel_plugin_id, &new_row)
            .await?;

        info!(
            channel_session_id = %session.channel_session_id,
            channel_user_id,
            chat_id = %chat_id,
            channel_plugin_id,
            "session reset"
        );

        Ok(session)
    }

    /// Updates the agent_type for an existing session.
    pub async fn update_agent_type(&self, session_id: &str, agent_type: &str) -> Result<(), ChannelError> {
        self.repo.update_session_agent_type(session_id, agent_type).await?;

        debug!(
            session_id = %session_id,
            agent_type = %agent_type,
            "session agent_type updated"
        );
        Ok(())
    }

    /// Removes all sessions belonging to a user.
    ///
    /// Called when a user is revoked to clean up their session state.
    pub async fn cleanup_user_sessions(&self, channel_user_id: &str) -> Result<(), ChannelError> {
        self.repo.delete_sessions_by_user(channel_user_id).await?;
        info!(channel_user_id = %channel_user_id, "cleaned up user sessions");
        Ok(())
    }

    /// Removes all sessions across all users.
    ///
    /// Called after settings sync to force sessions to be recreated
    /// with updated agent/model configuration.
    pub async fn clear_all_sessions(&self) -> Result<(), ChannelError> {
        let sessions = self.repo.get_all_sessions().await?;
        let mut cleared_users = std::collections::HashSet::new();
        for session in &sessions {
            if cleared_users.insert(session.channel_user_id.clone()) {
                self.repo
                    .delete_sessions_by_user(&session.channel_user_id)
                    .await?;
            }
        }
        info!(count = sessions.len(), "cleared all channel sessions");
        Ok(())
    }

    /// Looks up a session by its unique ID.
    pub async fn get_session_by_id(&self, session_id: &str) -> Result<Option<ChannelSessionRow>, ChannelError> {
        Ok(self.repo.get_session(session_id).await?)
    }

    /// The 对外伙伴 (public agent) bound to a bot channel row (`None` when the
    /// row is unbound or absent). Per-bot: reads `channel_plugins.public_agent_id`
    /// for `channel_plugin_id`. Used by the inbound path to decide whether a bot
    /// auto-serves unknown senders (public-agent-bound bots do; companion-bound
    /// and unbound bots keep the pairing gate).
    pub async fn channel_public_agent_id(
        &self,
        channel_plugin_id: &str,
    ) -> Result<Option<String>, ChannelError> {
        Ok(self
            .repo
            .get_plugin(channel_plugin_id)
            .await?
            .and_then(|row| row.public_agent_id)
            .filter(|s| !s.trim().is_empty()))
    }

    /// Persists the conversation binding for a session.
    ///
    /// Called after a new conversation is created for this session,
    /// linking the session to its backing conversation in the database.
    pub async fn bind_conversation(&self, session_id: &str, conversation_id: &str) -> Result<(), ChannelError> {
        ConversationId::try_from(conversation_id).map_err(|_| {
            ChannelError::MessageSendFailed(format!("invalid conversation id: {conversation_id}"))
        })?;
        self.repo
            .update_session_conversation(session_id, conversation_id)
            .await?;

        debug!(
            session_id = %session_id,
            conversation_id = %conversation_id,
            "session bound to conversation"
        );
        Ok(())
    }

    /// Claim a provider-owned inbound event before ActionExecutor or any other
    /// channel side effect runs.
    pub async fn claim_inbound(
        &self,
        row: NewChannelInboundReceiptRow,
    ) -> Result<ChannelInboundClaim, ChannelError> {
        Ok(self.repo.claim_inbound_receipt(&row).await?)
    }

    /// Durably cross the point after which a crashed owner can never be
    /// re-executed automatically.
    pub async fn begin_inbound_effects(
        &self,
        operation_key: &str,
        payload_hash: &str,
        owner_generation: i64,
    ) -> Result<bool, ChannelError> {
        Ok(self
            .repo
            .begin_inbound_effects(
                operation_key,
                payload_hash,
                owner_generation,
                now_ms(),
            )
            .await?)
    }

    pub async fn settle_inbound(
        &self,
        operation_key: &str,
        payload_hash: &str,
        owner_generation: i64,
        status: &str,
        params: SettleChannelInboundReceiptParams,
    ) -> Result<ChannelInboundReceiptRow, ChannelError> {
        Ok(self
            .repo
            .settle_inbound_receipt(
                operation_key,
                payload_hash,
                owner_generation,
                status,
                &params,
                now_ms(),
            )
            .await?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nomifun_common::TimestampMs;
    use nomifun_db::models::{
        ChannelPairingCodeRow, ChannelPluginRow, ChannelSessionRow, ChannelUserRow,
        NewChannelPairingCodeRow, NewChannelPluginRow, NewChannelSessionRow, NewChannelUserRow,
    };
    use nomifun_db::{DbError, IChannelRepository, UpdatePluginStatusParams};
    use std::sync::Mutex;

    const USER_1: &str = "0190f5fe-7c00-7a00-8000-000000000011";
    const USER_2: &str = "0190f5fe-7c00-7a00-8000-000000000012";
    const UNKNOWN_USER: &str = "0190f5fe-7c00-7a00-8000-000000000999";
    const PLUGIN_1: &str = "0190f5fe-7c00-7a00-8000-000000000021";
    const PLUGIN_2: &str = "0190f5fe-7c00-7a00-8000-000000000022";

    // ── Mock IChannelRepository ────────────────────────────────────────

    struct MockRepo {
        sessions: Mutex<Vec<ChannelSessionRow>>,
    }

    impl MockRepo {
        fn new() -> Self {
            Self {
                sessions: Mutex::new(Vec::new()),
            }
        }

        fn get_sessions(&self) -> Vec<ChannelSessionRow> {
            self.sessions.lock().unwrap().clone()
        }
    }

    #[async_trait::async_trait]
    impl IChannelRepository for MockRepo {
        // -- Plugin CRUD (unused stubs) --
        async fn get_all_plugins(&self) -> Result<Vec<ChannelPluginRow>, DbError> {
            Ok(vec![])
        }
        async fn get_plugin(&self, _channel_plugin_id: &str) -> Result<Option<ChannelPluginRow>, DbError> {
            Ok(None)
        }
        async fn create_plugin(&self, row: &NewChannelPluginRow) -> Result<ChannelPluginRow, DbError> {
            Ok(ChannelPluginRow {
                channel_plugin_id: nomifun_common::generate_id(),
                r#type: row.r#type.clone(),
                name: row.name.clone(),
                enabled: row.enabled,
                config: row.config.clone(),
                status: row.status.clone(),
                last_connected: row.last_connected,
                companion_id: row.companion_id.clone(),
                public_agent_id: row.public_agent_id.clone(),
                bot_key: row.bot_key.clone(),
                created_at: row.created_at,
                updated_at: row.updated_at,
            })
        }
        async fn update_plugin(&self, row: &ChannelPluginRow) -> Result<ChannelPluginRow, DbError> {
            Ok(row.clone())
        }
        async fn update_plugin_status(
            &self,
            _channel_plugin_id: &str,
            _params: &UpdatePluginStatusParams,
        ) -> Result<(), DbError> {
            Ok(())
        }
        async fn update_plugin_companion(
            &self,
            _channel_plugin_id: &str,
            _companion_id: Option<&str>,
        ) -> Result<(), DbError> {
            Ok(())
        }
        async fn update_plugin_public_agent(
            &self,
            _channel_plugin_id: &str,
            _public_agent_id: Option<&str>,
        ) -> Result<(), DbError> {
            Ok(())
        }
        async fn update_plugin_bot_key(
            &self,
            _channel_plugin_id: &str,
            _bot_key: &str,
        ) -> Result<(), DbError> {
            Ok(())
        }
        async fn delete_plugin(&self, _channel_plugin_id: &str) -> Result<(), DbError> {
            Ok(())
        }

        // -- User CRUD (unused stubs) --
        async fn get_all_users(&self) -> Result<Vec<ChannelUserRow>, DbError> {
            Ok(vec![])
        }
        async fn get_user_by_platform(
            &self,
            _platform_user_id: &str,
            _platform_type: &str,
            _channel_plugin_id: &str,
        ) -> Result<Option<ChannelUserRow>, DbError> {
            Ok(None)
        }
        async fn create_user(&self, row: &NewChannelUserRow) -> Result<ChannelUserRow, DbError> {
            Ok(ChannelUserRow {
                channel_user_id: nomifun_common::generate_id(),
                platform_user_id: row.platform_user_id.clone(),
                platform_type: row.platform_type.clone(),
                channel_plugin_id: row.channel_plugin_id.clone(),
                display_name: row.display_name.clone(),
                authorized_at: row.authorized_at,
                last_active: row.last_active,
            })
        }
        async fn update_user_last_active(
            &self,
            _channel_user_id: &str,
            _last_active: TimestampMs,
        ) -> Result<(), DbError> {
            Ok(())
        }
        async fn delete_user(&self, _channel_user_id: &str) -> Result<(), DbError> {
            Ok(())
        }

        // -- Session CRUD --
        async fn get_all_sessions(&self) -> Result<Vec<ChannelSessionRow>, DbError> {
            Ok(self.sessions.lock().unwrap().clone())
        }

        async fn get_session(&self, channel_session_id: &str) -> Result<Option<ChannelSessionRow>, DbError> {
            let sessions = self.sessions.lock().unwrap();
            Ok(sessions
                .iter()
                .find(|s| s.channel_session_id == channel_session_id)
                .cloned())
        }

        async fn get_or_create_session(
            &self,
            channel_user_id: &str,
            chat_id: &str,
            channel_plugin_id: &str,
            new_row: &NewChannelSessionRow,
        ) -> Result<ChannelSessionRow, DbError> {
            let mut sessions = self.sessions.lock().unwrap();
            // Look for an existing session by channel_plugin_id +
            // channel_user_id + chat_id
            // (mirrors the sqlite implementation's lookup key).
            if let Some(existing) = sessions.iter_mut().find(|s| {
                s.channel_user_id == channel_user_id
                    && s.chat_id.as_deref() == Some(chat_id)
                    && s.channel_plugin_id.as_deref() == Some(channel_plugin_id)
            }) {
                existing.last_activity = new_row.last_activity;
                return Ok(existing.clone());
            }

            let created = ChannelSessionRow {
                channel_session_id: new_row.channel_session_id.clone(),
                channel_user_id: new_row.channel_user_id.clone(),
                agent_type: new_row.agent_type.clone(),
                conversation_id: new_row.conversation_id.clone(),
                workspace: new_row.workspace.clone(),
                chat_id: new_row.chat_id.clone(),
                channel_plugin_id: new_row.channel_plugin_id.clone(),
                created_at: new_row.created_at,
                last_activity: new_row.last_activity,
            };
            sessions.push(created.clone());
            Ok(created)
        }

        async fn update_session_activity(
            &self,
            channel_session_id: &str,
            last_activity: TimestampMs,
        ) -> Result<(), DbError> {
            let mut sessions = self.sessions.lock().unwrap();
            if let Some(s) = sessions
                .iter_mut()
                .find(|s| s.channel_session_id == channel_session_id)
            {
                s.last_activity = last_activity;
                Ok(())
            } else {
                Err(DbError::NotFound(channel_session_id.into()))
            }
        }

        async fn update_session_conversation(
            &self,
            channel_session_id: &str,
            conversation_id: &str,
        ) -> Result<(), DbError> {
            let mut sessions = self.sessions.lock().unwrap();
            if let Some(s) = sessions
                .iter_mut()
                .find(|s| s.channel_session_id == channel_session_id)
            {
                s.conversation_id = Some(conversation_id.to_owned());
                s.last_activity = nomifun_common::now_ms();
                Ok(())
            } else {
                Err(DbError::NotFound(channel_session_id.into()))
            }
        }

        async fn update_session_agent_type(
            &self,
            channel_session_id: &str,
            agent_type: &str,
        ) -> Result<(), DbError> {
            let mut sessions = self.sessions.lock().unwrap();
            if let Some(s) = sessions
                .iter_mut()
                .find(|s| s.channel_session_id == channel_session_id)
            {
                s.agent_type = agent_type.to_owned();
                s.last_activity = nomifun_common::now_ms();
                Ok(())
            } else {
                Err(DbError::NotFound(channel_session_id.into()))
            }
        }

        async fn delete_sessions_by_user(&self, channel_user_id: &str) -> Result<(), DbError> {
            let mut sessions = self.sessions.lock().unwrap();
            sessions.retain(|s| s.channel_user_id != channel_user_id);
            Ok(())
        }

        async fn delete_sessions_by_channel(&self, channel_plugin_id: &str) -> Result<(), DbError> {
            let mut sessions = self.sessions.lock().unwrap();
            sessions.retain(|s| s.channel_plugin_id.as_deref() != Some(channel_plugin_id));
            Ok(())
        }

        async fn delete_session_by_user_chat(
            &self,
            channel_user_id: &str,
            chat_id: &str,
            channel_plugin_id: &str,
        ) -> Result<(), DbError> {
            let mut sessions = self.sessions.lock().unwrap();
            sessions.retain(|s| {
                !(s.channel_user_id == channel_user_id
                    && s.chat_id.as_deref() == Some(chat_id)
                    && s.channel_plugin_id.as_deref() == Some(channel_plugin_id))
            });
            Ok(())
        }

        // -- Pairing codes (unused stubs) --
        async fn create_pairing(&self, row: &NewChannelPairingCodeRow) -> Result<ChannelPairingCodeRow, DbError> {
            Ok(ChannelPairingCodeRow {
                code: row.code.clone(),
                platform_user_id: row.platform_user_id.clone(),
                platform_type: row.platform_type.clone(),
                channel_plugin_id: row.channel_plugin_id.clone(),
                display_name: row.display_name.clone(),
                requested_at: row.requested_at,
                expires_at: row.expires_at,
                status: row.status.clone(),
            })
        }
        async fn get_pending_pairings(&self) -> Result<Vec<ChannelPairingCodeRow>, DbError> {
            Ok(vec![])
        }
        async fn get_pairing_by_code(&self, _code: &str) -> Result<Option<ChannelPairingCodeRow>, DbError> {
            Ok(None)
        }
        async fn update_pairing_status(&self, _code: &str, _status: &str) -> Result<(), DbError> {
            Ok(())
        }
        async fn cleanup_expired_pairings(&self, _now: TimestampMs) -> Result<u64, DbError> {
            Ok(0)
        }
    }

    fn make_manager() -> (SessionManager, Arc<MockRepo>) {
        let repo = Arc::new(MockRepo::new());
        let mgr = SessionManager::new(repo.clone());
        (mgr, repo)
    }

    // ── get_or_create_session ──────────────────────────────────────────

    #[tokio::test]
    async fn creates_new_session() {
        let (mgr, repo) = make_manager();
        let session = mgr
            .get_or_create_session(USER_1, "chat1", PLUGIN_1, "acp", None)
            .await
            .unwrap();

        assert_eq!(session.channel_user_id, USER_1);
        assert_eq!(session.chat_id.as_deref(), Some("chat1"));
        assert_eq!(session.channel_plugin_id.as_deref(), Some(PLUGIN_1));
        assert_eq!(session.agent_type, "acp");
        assert!(session.conversation_id.is_none());

        let all = repo.get_sessions();
        assert_eq!(all.len(), 1);
    }

    #[tokio::test]
    async fn reuses_existing_session_for_same_user_chat() {
        let (mgr, repo) = make_manager();

        let s1 = mgr
            .get_or_create_session(USER_1, "chat1", PLUGIN_1, "acp", None)
            .await
            .unwrap();
        let s2 = mgr
            .get_or_create_session(USER_1, "chat1", PLUGIN_1, "acp", None)
            .await
            .unwrap();

        assert_eq!(s1.channel_session_id, s2.channel_session_id);
        assert_eq!(repo.get_sessions().len(), 1);
    }

    #[tokio::test]
    async fn different_chats_get_different_sessions() {
        let (mgr, repo) = make_manager();

        let s1 = mgr
            .get_or_create_session(USER_1, "chatA", PLUGIN_1, "acp", None)
            .await
            .unwrap();
        let s2 = mgr
            .get_or_create_session(USER_1, "chatB", PLUGIN_1, "acp", None)
            .await
            .unwrap();

        assert_ne!(s1.channel_session_id, s2.channel_session_id);
        assert_eq!(repo.get_sessions().len(), 2);
    }

    #[tokio::test]
    async fn different_users_same_chat_get_different_sessions() {
        let (mgr, repo) = make_manager();

        let s1 = mgr
            .get_or_create_session(USER_1, "chat1", PLUGIN_1, "acp", None)
            .await
            .unwrap();
        let s2 = mgr
            .get_or_create_session(USER_2, "chat1", PLUGIN_1, "acp", None)
            .await
            .unwrap();

        assert_ne!(s1.channel_session_id, s2.channel_session_id);
        assert_eq!(repo.get_sessions().len(), 2);
    }

    #[tokio::test]
    async fn different_channels_same_chat_get_different_sessions() {
        let (mgr, repo) = make_manager();

        let s1 = mgr
            .get_or_create_session(USER_1, "chat1", PLUGIN_1, "acp", None)
            .await
            .unwrap();
        let s2 = mgr
            .get_or_create_session(USER_1, "chat1", PLUGIN_2, "acp", None)
            .await
            .unwrap();

        assert_ne!(s1.channel_session_id, s2.channel_session_id);
        assert_eq!(s1.channel_plugin_id.as_deref(), Some(PLUGIN_1));
        assert_eq!(s2.channel_plugin_id.as_deref(), Some(PLUGIN_2));
        assert_eq!(repo.get_sessions().len(), 2);

        // Same channel again → reuse, no third session.
        let s3 = mgr
            .get_or_create_session(USER_1, "chat1", PLUGIN_1, "acp", None)
            .await
            .unwrap();
        assert_eq!(s3.channel_session_id, s1.channel_session_id);
        assert_eq!(repo.get_sessions().len(), 2);
    }

    #[tokio::test]
    async fn session_with_workspace() {
        let (mgr, _repo) = make_manager();
        let session = mgr
            .get_or_create_session(USER_1, "c1", PLUGIN_1, "acp", Some("/workspace"))
            .await
            .unwrap();

        assert_eq!(session.workspace.as_deref(), Some("/workspace"));
    }

    // ── get_active_sessions ────────────────────────────────────────────

    #[tokio::test]
    async fn get_active_sessions_empty() {
        let (mgr, _repo) = make_manager();
        let sessions = mgr.get_active_sessions().await.unwrap();
        assert!(sessions.is_empty());
    }

    #[tokio::test]
    async fn get_active_sessions_returns_all() {
        let (mgr, _repo) = make_manager();
        mgr.get_or_create_session(USER_1, "c1", PLUGIN_1, "acp", None)
            .await
            .unwrap();
        mgr.get_or_create_session(USER_2, "c2", PLUGIN_1, "acp", None)
            .await
            .unwrap();

        let sessions = mgr.get_active_sessions().await.unwrap();
        assert_eq!(sessions.len(), 2);
    }

    // ── cleanup_user_sessions ──────────────────────────────────────────

    #[tokio::test]
    async fn cleanup_removes_user_sessions() {
        let (mgr, repo) = make_manager();
        mgr.get_or_create_session(USER_1, "c1", PLUGIN_1, "acp", None)
            .await
            .unwrap();
        mgr.get_or_create_session(USER_1, "c2", PLUGIN_1, "acp", None)
            .await
            .unwrap();
        mgr.get_or_create_session(USER_2, "c1", PLUGIN_1, "acp", None)
            .await
            .unwrap();

        mgr.cleanup_user_sessions(USER_1).await.unwrap();

        let sessions = repo.get_sessions();
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].channel_user_id, USER_2);
    }

    #[tokio::test]
    async fn cleanup_noop_for_unknown_user() {
        let (mgr, repo) = make_manager();
        mgr.get_or_create_session(USER_1, "c1", PLUGIN_1, "acp", None)
            .await
            .unwrap();

        mgr.cleanup_user_sessions(UNKNOWN_USER).await.unwrap();

        assert_eq!(repo.get_sessions().len(), 1);
    }

    // ── bind_conversation ──────────────────────────────────────────────

    #[tokio::test]
    async fn bind_conversation_persists_conversation_id() {
        let (mgr, repo) = make_manager();
        let session = mgr
            .get_or_create_session(USER_1, "c1", PLUGIN_1, "acp", None)
            .await
            .unwrap();
        assert!(session.conversation_id.is_none());
        let conversation_id = ConversationId::new();

        mgr.bind_conversation(&session.channel_session_id, conversation_id.as_ref())
            .await
            .unwrap();

        let updated = repo
            .get_sessions()
            .into_iter()
            .find(|s| s.channel_session_id == session.channel_session_id)
            .unwrap();
        assert_eq!(updated.conversation_id.as_deref(), Some(conversation_id.as_ref()));
    }

    #[tokio::test]
    async fn bind_conversation_not_found() {
        let (mgr, _repo) = make_manager();
        let conversation_id = ConversationId::new();
        let err = mgr
            .bind_conversation("nonexistent", conversation_id.as_ref())
            .await;
        assert!(err.is_err());
    }

    #[tokio::test]
    async fn bind_conversation_rejects_noncanonical_id() {
        let (mgr, repo) = make_manager();
        let session = mgr
            .get_or_create_session(USER_1, "c1", PLUGIN_1, "acp", None)
            .await
            .unwrap();

        let err = mgr.bind_conversation(&session.channel_session_id, "123").await;

        assert!(err.is_err());
        let updated = repo
            .get_sessions()
            .into_iter()
            .find(|s| s.channel_session_id == session.channel_session_id)
            .unwrap();
        assert!(updated.conversation_id.is_none());
    }

    // ── reset_session ─────────────────────────────────────────────────

    #[tokio::test]
    async fn reset_session_creates_fresh_session() {
        let (mgr, repo) = make_manager();
        let s1 = mgr
            .get_or_create_session(USER_1, "c1", PLUGIN_1, "acp", None)
            .await
            .unwrap();

        let s2 = mgr
            .reset_session(USER_1, "c1", PLUGIN_1, "acp", None)
            .await
            .unwrap();

        // The stable session UUID changes; technical row ids stay internal.
        assert_ne!(s1.channel_session_id, s2.channel_session_id);
        assert_eq!(s2.channel_user_id, USER_1);
        assert_eq!(s2.chat_id.as_deref(), Some("c1"));
        assert_eq!(s2.channel_plugin_id.as_deref(), Some(PLUGIN_1));
        assert!(s2.conversation_id.is_none());

        // Only 1 session should exist (old one deleted)
        assert_eq!(repo.get_sessions().len(), 1);
    }

    #[tokio::test]
    async fn reset_session_noop_when_no_existing() {
        let (mgr, repo) = make_manager();
        let session = mgr
            .reset_session(USER_1, "c1", PLUGIN_1, "acp", None)
            .await
            .unwrap();

        assert_eq!(session.channel_user_id, USER_1);
        assert_eq!(repo.get_sessions().len(), 1);
    }

    // ── update_agent_type ─────────────────────────────────────────────

    #[tokio::test]
    async fn update_agent_type_persists() {
        let (mgr, repo) = make_manager();
        let session = mgr
            .get_or_create_session(USER_1, "c1", PLUGIN_1, "acp", None)
            .await
            .unwrap();
        assert_eq!(session.agent_type, "acp");

        mgr.update_agent_type(&session.channel_session_id, "acp").await.unwrap();

        let updated = repo
            .get_sessions()
            .into_iter()
            .find(|s| s.channel_session_id == session.channel_session_id)
            .unwrap();
        assert_eq!(updated.agent_type, "acp");
    }

    #[tokio::test]
    async fn update_agent_type_not_found() {
        let (mgr, _repo) = make_manager();
        let err = mgr.update_agent_type("nonexistent", "acp").await;
        assert!(err.is_err());
    }
}

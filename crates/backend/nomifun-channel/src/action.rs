use std::collections::HashMap;
use std::sync::Arc;

use tracing::{debug, info, warn};

use crate::channel_settings::ChannelSettingsService;
use crate::error::ChannelError;
use crate::pairing::PairingService;
use crate::session::SessionManager;
use crate::types::{
    ActionBehavior, ActionButton, ActionCategory, ActionResponse, UnifiedAction, UnifiedIncomingMessage,
};

/// User text synthesized for the `chat.continue` button. Sent to the agent
/// as a regular user turn; "继续" is the canonical continue prompt that all
/// supported agents understand regardless of conversation language.
pub const CONTINUE_PROMPT: &str = "继续";

/// Result of processing an incoming message.
///
/// The caller (ChannelManager / plugin) uses this to decide what to send
/// back to the IM platform.
#[derive(Debug, Clone)]
pub enum MessageResult {
    /// An action response to send/edit on the platform.
    Action(ActionResponse),
    /// Message was dispatched to the AI Agent. The caller should send
    /// a "thinking" placeholder and then relay stream events.
    Dispatched {
        session_id: String,
        conversation_id: Option<String>,
    },
    /// A chat action (`chat.continue`) re-enters the normal AI dispatch
    /// path with a synthesized user text. The message loop relays it
    /// exactly like `Dispatched`, preserving streaming replies.
    DispatchedText {
        session_id: String,
        conversation_id: Option<String>,
        text: String,
    },
    /// `chat.regenerate`: resend the conversation's last user message.
    /// Resolving that text needs conversation history access
    /// (`ChannelMessageService`), which the executor deliberately does not
    /// hold — the message loop performs the lookup and dispatch.
    RegenerateRequested {
        session_id: String,
        conversation_id: Option<String>,
    },
    /// Message was a text but user already has an active agent stream
    /// (no duplicate dispatch needed).
    AlreadyProcessing,
}

/// Processes incoming IM messages: authorization → action routing → AI dispatch.
///
/// This is the core message entry point for the channel system. Each
/// incoming `UnifiedIncomingMessage` is either:
/// 1. Rejected (unauthorized → pairing flow)
/// 2. Routed to an action handler (button callback)
/// 3. Dispatched to the AI Agent (text message)
pub struct ActionExecutor {
    pairing: Arc<PairingService>,
    session_mgr: Arc<SessionManager>,
    settings: Arc<ChannelSettingsService>,
    default_agent_type: String,
    /// Opt-in IM → requirement creator. `None` (default) keeps the normal AI
    /// dispatch path unchanged.
    requirement_creator: Option<Arc<dyn nomifun_common::RequirementCreator>>,
}

/// Derive a requirement `(title, content)` from an inbound message's text:
/// title = first non-empty line (trimmed to 120 chars), content = the full text.
/// Pure so the mapping is unit-testable without the channel stack.
pub(crate) fn message_to_requirement(text: &str) -> (String, String) {
    let first = text.lines().map(str::trim).find(|l| !l.is_empty()).unwrap_or("");
    let title = if first.is_empty() {
        "(message)".to_string()
    } else if first.chars().count() > 120 {
        let truncated: String = first.chars().take(117).collect();
        format!("{truncated}...")
    } else {
        first.to_string()
    };
    (title, text.to_string())
}

impl ActionExecutor {
    pub fn new(
        pairing: Arc<PairingService>,
        session_mgr: Arc<SessionManager>,
        settings: Arc<ChannelSettingsService>,
        default_agent_type: &str,
    ) -> Self {
        Self {
            pairing,
            session_mgr,
            settings,
            default_agent_type: default_agent_type.to_owned(),
            requirement_creator: None,
        }
    }

    /// Enable the opt-in IM → requirement pipeline (channel inbound → tracked
    /// requirement). When unset, behaviour is unchanged.
    pub fn with_requirement_creator(
        mut self,
        creator: Option<Arc<dyn nomifun_common::RequirementCreator>>,
    ) -> Self {
        self.requirement_creator = creator;
        self
    }

    /// Main entry point: handle an incoming message from any platform.
    ///
    /// `channel_plugin_id` is the `channel_plugins.channel_plugin_id` business
    /// identity the message arrived
    /// through — sessions are scoped to it so two bots in the same chat
    /// stay isolated.
    ///
    /// Flow:
    /// 1. Authorization check → if unauthorized, trigger pairing
    /// 2. Button callback → route to action handler
    /// 3. Text message → get/create session → return Dispatched for AI
    pub async fn handle_incoming_message(
        &self,
        msg: &UnifiedIncomingMessage,
        channel_plugin_id: &str,
    ) -> Result<MessageResult, ChannelError> {
        let platform_type = msg.platform.to_string();
        let user_id = &msg.user.id;
        let chat_id = &msg.chat_id;

        // 1. Authorization check — resolve platform user → internal user ID
        let internal_user_id = self
            .pairing
            .get_internal_user_id(user_id, &platform_type, channel_plugin_id)
            .await?;

        let internal_user_id = match internal_user_id {
            Some(id) => id,
            None => {
                // 对外伙伴自动接待 (SECURITY-CRITICAL): a bot bound to a PUBLIC
                // AGENT auto-serves unknown senders with NO pairing code — the
                // public-agent session is hard-clamped to `PublicService` (safe
                // tools only), which is the whole point of the feature. Auto-served
                // strangers run under the owner/system identity (the clamp is the
                // boundary, not the user id); their per-chat conversation gives
                // per-stranger isolation. This bypass is gated STRICTLY on the BOT
                // (per-bot `channel_plugins.public_agent_id`, keyed by the arriving
                // `channel_id`) being bound to a public agent — companion-bound and
                // unbound bots keep the pairing approval gate UNCHANGED.
                if self
                    .session_mgr
                    .channel_public_agent_id(channel_plugin_id)
                    .await?
                    .is_some()
                {
                    // Auto-register the stranger as a channel user (no pairing code) so
                    // channel_sessions.channel_user_id resolves to its logical parent. The
                    // agent itself runs under the owner/system identity (set in
                    // ChannelMessageService); the PublicService clamp is the real boundary,
                    // and the per-chat conversation gives per-stranger isolation.
                    self.pairing
                        .ensure_channel_user(
                            user_id,
                            &platform_type,
                            channel_plugin_id,
                            &msg.user.display_name,
                        )
                        .await?
                } else {
                    let response = self
                        .handle_unauthorized(
                            user_id,
                            &platform_type,
                            channel_plugin_id,
                            &msg.user.display_name,
                        )
                        .await?;
                    return Ok(MessageResult::Action(response));
                }
            }
        };

        // 2. Button callback → action routing
        if let Some(action) = &msg.action {
            return self
                .route_action(action, &internal_user_id, channel_plugin_id)
                .await;
        }

        // 2.5 Opt-in IM → requirement: file the text as a tracked requirement
        // (which AutoWork executes) instead of dispatching an immediate AI reply.
        // Gated by a per-platform setting; default off → the normal path runs.
        if let Some(creator) = &self.requirement_creator
            && self.settings.get_route_to_requirement(msg.platform).await?
        {
            let (title, content) = message_to_requirement(&msg.content.text);
            let tag = self.settings.get_requirement_tag(msg.platform).await?;
            let created_by = format!("channel:{platform_type}");
            let response = match creator
                .create_from_message(&title, &content, &tag, &created_by)
                .await
            {
                Ok(id) => ActionResponse {
                    // Keep decimal conversion at this human-message boundary;
                    // the cross-crate hook itself carries the local i64.
                    text: Some(format!("✅ Created requirement #{id}: {title}")),
                    parse_mode: None,
                    buttons: None,
                    keyboard: None,
                    behavior: ActionBehavior::Send,
                    toast: None,
                    edit_message_id: None,
                },
                Err(e) => {
                    warn!(channel_plugin_id, error = %e, "IM → requirement creation failed");
                    ActionResponse {
                        text: Some(format!("⚠️ Could not create a requirement: {e}")),
                        parse_mode: None,
                        buttons: None,
                        keyboard: None,
                        behavior: ActionBehavior::Send,
                        toast: None,
                        edit_message_id: None,
                    }
                }
            };
            return Ok(MessageResult::Action(response));
        }

        // 3. Text message → session resolution → AI dispatch
        let agent_config = self.settings.get_agent_config(msg.platform).await?;
        let session = self
            .session_mgr
            .get_or_create_session(
                &internal_user_id,
                chat_id,
                channel_plugin_id,
                &agent_config.agent_type,
                None,
            )
            .await?;

        info!(
            channel_session_id = %session.channel_session_id,
            user_id = %user_id,
            chat_id = %chat_id,
            channel_plugin_id,
            text_len = msg.content.text.len(),
            "message dispatched to agent"
        );

        Ok(MessageResult::Dispatched {
            session_id: session.channel_session_id,
            conversation_id: session.conversation_id,
        })
    }

    /// Handles an unauthorized user: generate pairing code and return
    /// a response with instructions and action buttons.
    async fn handle_unauthorized(
        &self,
        platform_user_id: &str,
        platform_type: &str,
        channel_plugin_id: &str,
        display_name: &str,
    ) -> Result<ActionResponse, ChannelError> {
        let code = self
            .pairing
            .request_pairing(
                platform_user_id,
                platform_type,
                channel_plugin_id,
                Some(display_name),
            )
            .await?;

        debug!(
            platform_user_id = %platform_user_id,
            code = %code,
            "pairing code generated for unauthorized user"
        );

        Ok(build_pairing_response(&code))
    }

    /// Routes an action to the appropriate handler by category.
    ///
    /// Platform/system actions always produce an `ActionResponse`; chat
    /// actions may re-enter the AI dispatch path (continue / regenerate).
    async fn route_action(
        &self,
        action: &UnifiedAction,
        internal_user_id: &str,
        channel_plugin_id: &str,
    ) -> Result<MessageResult, ChannelError> {
        match action.category {
            ActionCategory::Platform => Ok(MessageResult::Action(
                self.handle_platform_action(action, channel_plugin_id).await?,
            )),
            ActionCategory::System => Ok(MessageResult::Action(
                self.handle_system_action(action, internal_user_id, channel_plugin_id)
                    .await?,
            )),
            ActionCategory::Chat => {
                self.handle_chat_action(action, internal_user_id, channel_plugin_id)
                    .await
            }
        }
    }

    // ── Platform actions ────────────────────────────────────────────

    async fn handle_platform_action(
        &self,
        action: &UnifiedAction,
        channel_plugin_id: &str,
    ) -> Result<ActionResponse, ChannelError> {
        match action.action.as_str() {
            "pairing.show" | "pairing.refresh" => {
                let code = self
                    .pairing
                    .request_pairing(
                        &action.context.user_id,
                        &action.context.platform.to_string(),
                        channel_plugin_id,
                        None,
                    )
                    .await?;
                Ok(build_pairing_response(&code))
            }
            "pairing.check" => {
                let authorized = self
                    .pairing
                    .is_user_authorized(
                        &action.context.user_id,
                        &action.context.platform.to_string(),
                        channel_plugin_id,
                    )
                    .await?;
                if authorized {
                    Ok(ActionResponse {
                        text: Some("You are authorized! Send a message to start chatting.".into()),
                        parse_mode: None,
                        buttons: None,
                        keyboard: None,
                        behavior: ActionBehavior::Send,
                        toast: None,
                        edit_message_id: None,
                    })
                } else {
                    Ok(ActionResponse {
                        text: Some("Still waiting for approval. Ask the admin to check Settings → Channel.".into()),
                        parse_mode: None,
                        buttons: Some(vec![vec![
                            ActionButton {
                                label: "Refresh".into(),
                                action: "pairing.refresh".into(),
                                params: None,
                            },
                            ActionButton {
                                label: "Check Again".into(),
                                action: "pairing.check".into(),
                                params: None,
                            },
                        ]]),
                        keyboard: None,
                        behavior: ActionBehavior::Send,
                        toast: None,
                        edit_message_id: None,
                    })
                }
            }
            "pairing.help" => Ok(ActionResponse {
                text: Some(
                    "To use this bot, you need authorization:\n\
                         1. Send any message to get a 6-digit pairing code\n\
                         2. Share this code with the admin\n\
                         3. Admin approves in Settings → Channel\n\
                         4. You're ready to chat!"
                        .into(),
                ),
                parse_mode: None,
                buttons: None,
                keyboard: None,
                behavior: ActionBehavior::Send,
                toast: None,
                edit_message_id: None,
            }),
            other => {
                warn!(action = %other, "unknown platform action");
                Ok(build_unknown_action_response(other))
            }
        }
    }

    // ── System actions ──────────────────────────────────────────────

    async fn handle_system_action(
        &self,
        action: &UnifiedAction,
        internal_user_id: &str,
        channel_plugin_id: &str,
    ) -> Result<ActionResponse, ChannelError> {
        match action.action.as_str() {
            "session.new" => {
                let user_id = internal_user_id;
                let chat_id = &action.context.chat_id;
                let agent_config = self.settings.get_agent_config(action.context.platform).await?;
                let session = self
                    .session_mgr
                    .reset_session(
                        user_id,
                        chat_id,
                        channel_plugin_id,
                        &agent_config.agent_type,
                        None,
                    )
                    .await?;

                Ok(ActionResponse {
                    text: Some(format!(
                        "New session created.\nAgent: {}\nSession: {}",
                        session.agent_type,
                        short_session_id(&session.channel_session_id)
                    )),
                    parse_mode: None,
                    buttons: Some(vec![vec![ActionButton {
                        label: "Help".into(),
                        action: "help.show".into(),
                        params: None,
                    }]]),
                    keyboard: None,
                    behavior: ActionBehavior::Send,
                    toast: None,
                    edit_message_id: None,
                })
            }
            "session.status" => {
                let user_id = internal_user_id;
                let chat_id = &action.context.chat_id;
                let agent_config = self.settings.get_agent_config(action.context.platform).await?;
                let session = self
                    .session_mgr
                    .get_or_create_session(
                        user_id,
                        chat_id,
                        channel_plugin_id,
                        &agent_config.agent_type,
                        None,
                    )
                    .await?;

                Ok(ActionResponse {
                    text: Some(format!(
                        "Session: {}\nAgent: {}\nCreated: {}\nLast active: {}",
                        short_session_id(&session.channel_session_id),
                        session.agent_type,
                        session.created_at,
                        session.last_activity,
                    )),
                    parse_mode: None,
                    buttons: Some(vec![vec![ActionButton {
                        label: "New Session".into(),
                        action: "session.new".into(),
                        params: None,
                    }]]),
                    keyboard: None,
                    behavior: ActionBehavior::Send,
                    toast: None,
                    edit_message_id: None,
                })
            }
            "help.show" => Ok(build_help_response()),
            "help.features" => Ok(ActionResponse {
                text: Some(
                    "Features:\n\
                         • AI chat with multiple backends\n\
                         • Tool execution with auto-approval\n\
                         • Session isolation per chat\n\
                         • Agent switching"
                        .into(),
                ),
                parse_mode: None,
                buttons: None,
                keyboard: None,
                behavior: ActionBehavior::Send,
                toast: None,
                edit_message_id: None,
            }),
            "help.pairing" => Ok(ActionResponse {
                text: Some(
                    "Pairing:\n\
                         Send any message → get a 6-digit code → admin approves → you're in!"
                        .into(),
                ),
                parse_mode: None,
                buttons: None,
                keyboard: None,
                behavior: ActionBehavior::Send,
                toast: None,
                edit_message_id: None,
            }),
            "help.tips" => Ok(ActionResponse {
                text: Some(
                    "Tips:\n\
                         • Start a new session to clear context\n\
                         • Use /help to see available commands\n\
                         • In group chats, @mention the bot"
                        .into(),
                ),
                parse_mode: None,
                buttons: None,
                keyboard: None,
                behavior: ActionBehavior::Send,
                toast: None,
                edit_message_id: None,
            }),
            "settings.show" => Ok(ActionResponse {
                text: Some(
                    "Settings are managed in the desktop app.\n\
                         Go to Settings → Channel to configure plugins and manage users."
                        .into(),
                ),
                parse_mode: None,
                buttons: None,
                keyboard: None,
                behavior: ActionBehavior::Send,
                toast: None,
                edit_message_id: None,
            }),
            "agent.show" => Ok(ActionResponse {
                text: Some("Available agents:".into()),
                parse_mode: None,
                buttons: Some(vec![vec![ActionButton {
                    label: "ACP".into(),
                    action: "agent.select".into(),
                    params: Some(HashMap::from([("agentType".into(), "acp".into())])),
                }]]),
                keyboard: None,
                behavior: ActionBehavior::Send,
                toast: None,
                edit_message_id: None,
            }),
            "agent.select" => {
                let agent_type = action
                    .params
                    .as_ref()
                    .and_then(|p| p.get("agentType"))
                    .map(|s| s.as_str())
                    .unwrap_or(&self.default_agent_type);

                // Persist the agent_type change to the session
                let chat_id = &action.context.chat_id;
                let session = self
                    .session_mgr
                    .get_or_create_session(
                        internal_user_id,
                        chat_id,
                        channel_plugin_id,
                        agent_type,
                        None,
                    )
                    .await?;
                self.session_mgr
                    .update_agent_type(&session.channel_session_id, agent_type)
                    .await?;

                Ok(ActionResponse {
                    text: Some(format!("Agent switched to: {agent_type}")),
                    parse_mode: None,
                    buttons: None,
                    keyboard: None,
                    behavior: ActionBehavior::Send,
                    toast: Some(format!("Switched to {agent_type}")),
                    edit_message_id: None,
                })
            }
            other => {
                warn!(action = %other, "unknown system action");
                Ok(build_unknown_action_response(other))
            }
        }
    }

    // ── Chat actions ────────────────────────────────────────────────

    async fn handle_chat_action(
        &self,
        action: &UnifiedAction,
        internal_user_id: &str,
        channel_plugin_id: &str,
    ) -> Result<MessageResult, ChannelError> {
        match action.action.as_str() {
            "chat.continue" => {
                // Re-enter the normal dispatch path with a fixed "continue"
                // prompt so the agent resumes its previous answer. The
                // message loop handles streaming exactly like a typed message.
                let session = self
                    .resolve_action_session(action, internal_user_id, channel_plugin_id)
                    .await?;
                Ok(MessageResult::DispatchedText {
                    session_id: session.channel_session_id,
                    conversation_id: session.conversation_id,
                    text: CONTINUE_PROMPT.into(),
                })
            }
            "chat.regenerate" => {
                // The last user message lives in conversation history, which
                // only ChannelMessageService can read — hand the lookup to
                // the message loop instead of growing this executor's deps.
                let session = self
                    .resolve_action_session(action, internal_user_id, channel_plugin_id)
                    .await?;
                Ok(MessageResult::RegenerateRequested {
                    session_id: session.channel_session_id,
                    conversation_id: session.conversation_id,
                })
            }
            "chat.send" => {
                // Reserved: platforms deliver plain text through the message
                // flow, not through this action. Acknowledge the tap only.
                Ok(MessageResult::Action(ActionResponse {
                    text: None,
                    parse_mode: None,
                    buttons: None,
                    keyboard: None,
                    behavior: ActionBehavior::Send,
                    toast: Some("Processing...".into()),
                    edit_message_id: None,
                }))
            }
            "action.copy" => Ok(MessageResult::Action(ActionResponse {
                text: None,
                parse_mode: None,
                buttons: None,
                keyboard: None,
                behavior: ActionBehavior::Answer,
                toast: Some("Copied to clipboard".into()),
                edit_message_id: None,
            })),
            "system.confirm" => {
                let call_id = action
                    .params
                    .as_ref()
                    .and_then(|p| p.get("callId"))
                    .cloned()
                    .unwrap_or_default();
                let value = action
                    .params
                    .as_ref()
                    .and_then(|p| p.get("value"))
                    .cloned()
                    .unwrap_or_else(|| "true".into());

                debug!(call_id = %call_id, value = %value, "tool confirmation received");

                Ok(MessageResult::Action(ActionResponse {
                    text: None,
                    parse_mode: None,
                    buttons: None,
                    keyboard: None,
                    behavior: ActionBehavior::Answer,
                    toast: Some("Confirmed".into()),
                    edit_message_id: None,
                }))
            }
            other => {
                warn!(action = %other, "unknown chat action");
                Ok(MessageResult::Action(build_unknown_action_response(other)))
            }
        }
    }

    /// Resolves the session for an action callback, mirroring the text
    /// message flow (same channel + user + chat + configured agent type).
    async fn resolve_action_session(
        &self,
        action: &UnifiedAction,
        internal_user_id: &str,
        channel_plugin_id: &str,
    ) -> Result<nomifun_db::models::ChannelSessionRow, ChannelError> {
        let agent_config = self.settings.get_agent_config(action.context.platform).await?;
        self.session_mgr
            .get_or_create_session(
                internal_user_id,
                &action.context.chat_id,
                channel_plugin_id,
                &agent_config.agent_type,
                None,
            )
            .await
    }
}

// ── Helper builders ─────────────────────────────────────────────────

/// Short display form of a channel-session UUIDv7 for IM replies.
/// The timestamp-heavy head is common across sessions; the random tail is the
/// distinctive part, so truncate from the front.
fn short_session_id(id: &str) -> String {
    if id.len() > 8 {
        format!("…{}", &id[id.len() - 8..])
    } else {
        id.to_owned()
    }
}

fn build_pairing_response(code: &str) -> ActionResponse {
    ActionResponse {
        text: Some(format!(
            "Welcome! To use this bot, you need authorization.\n\n\
             Your pairing code: *{code}*\n\n\
             Share this code with the admin, who can approve it in \
             Settings → Channel → Pairing Requests.\n\
             The code expires in 10 minutes."
        )),
        parse_mode: None,
        buttons: Some(vec![vec![
            ActionButton {
                label: "Refresh Code".into(),
                action: "pairing.refresh".into(),
                params: None,
            },
            ActionButton {
                label: "Check Status".into(),
                action: "pairing.check".into(),
                params: None,
            },
            ActionButton {
                label: "Help".into(),
                action: "pairing.help".into(),
                params: None,
            },
        ]]),
        keyboard: None,
        behavior: ActionBehavior::Send,
        toast: None,
        edit_message_id: None,
    }
}

fn build_help_response() -> ActionResponse {
    ActionResponse {
        text: Some(
            "How can I help?\n\
             Choose an option below or just send me a message."
                .into(),
        ),
        parse_mode: None,
        buttons: Some(vec![
            vec![
                ActionButton {
                    label: "New Session".into(),
                    action: "session.new".into(),
                    params: None,
                },
                ActionButton {
                    label: "Session Status".into(),
                    action: "session.status".into(),
                    params: None,
                },
            ],
            vec![
                ActionButton {
                    label: "Features".into(),
                    action: "help.features".into(),
                    params: None,
                },
                ActionButton {
                    label: "Tips".into(),
                    action: "help.tips".into(),
                    params: None,
                },
            ],
            vec![ActionButton {
                label: "Switch Agent".into(),
                action: "agent.show".into(),
                params: None,
            }],
        ]),
        keyboard: None,
        behavior: ActionBehavior::Send,
        toast: None,
        edit_message_id: None,
    }
}

fn build_unknown_action_response(action: &str) -> ActionResponse {
    ActionResponse {
        text: Some(format!("Unknown action: {action}")),
        parse_mode: None,
        buttons: None,
        keyboard: None,
        behavior: ActionBehavior::Send,
        toast: None,
        edit_message_id: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn message_to_requirement_derives_title_and_content() {
        // Multi-line: title is the first non-empty line; content is the whole text.
        let (title, content) = message_to_requirement("  \n  Fix the login bug\nmore details\n");
        assert_eq!(title, "Fix the login bug");
        assert_eq!(content, "  \n  Fix the login bug\nmore details\n");

        // Empty / whitespace-only → a placeholder title (never empty: tag/title
        // are required by CreateRequirementRequest).
        assert_eq!(message_to_requirement("   ").0, "(message)");
        assert_eq!(message_to_requirement("").0, "(message)");

        // A very long first line is truncated with an ellipsis.
        let long = "x".repeat(200);
        let (title, _) = message_to_requirement(&long);
        assert!(title.ends_with("..."));
        assert_eq!(title.chars().count(), 120);
    }
}

#[cfg(test)]
mod action_tests {
    use super::*;
    use crate::types::{ActionContext, MessageContentType, PluginType, UnifiedMessageContent, UnifiedUser};
    use nomifun_api_types::WebSocketMessage;
    use nomifun_common::{TimestampMs, now_ms};
    use nomifun_db::models::{
        ChannelPairingCodeRow, ChannelPluginRow, ChannelSessionRow, ChannelUserRow,
        ClientPreference, NewChannelPairingCodeRow, NewChannelPluginRow,
        NewChannelSessionRow, NewChannelUserRow,
    };
    use nomifun_db::{DbError, IChannelRepository, IClientPreferenceRepository, UpdatePluginStatusParams};
    use nomifun_realtime::UserEventSink;
    use std::sync::Mutex;

    const TEST_CHANNEL_PLUGIN_ID: &str = "0190f5fe-7c00-7a00-8000-000000000001";
    const TEST_CHANNEL_USER_ID: &str = "0190f5fe-7c00-7a00-8000-000000000101";
    const ALT_CHANNEL_USER_ID: &str = "0190f5fe-7c00-7a00-8000-000000000199";

    // ── Mock owner-scoped event sink ───────────────────────────────────

    struct MockBroadcaster;

    impl UserEventSink for MockBroadcaster {
        fn send_to_user(&self, _user_id: &str, _event: WebSocketMessage<serde_json::Value>) {}
    }

    // ── Mock IChannelRepository ────────────────────────────────────────

    struct MockRepo {
        users: Mutex<Vec<ChannelUserRow>>,
        sessions: Mutex<Vec<ChannelSessionRow>>,
        pairings: Mutex<Vec<ChannelPairingCodeRow>>,
        plugins: Mutex<Vec<ChannelPluginRow>>,
    }

    impl MockRepo {
        fn new() -> Self {
            Self {
                users: Mutex::new(Vec::new()),
                sessions: Mutex::new(Vec::new()),
                pairings: Mutex::new(Vec::new()),
                plugins: Mutex::new(Vec::new()),
            }
        }

        fn add_authorized_user(&self, platform_user_id: &str, platform_type: &str) {
            let channel_user_id = match platform_user_id {
                "tg_42" => TEST_CHANNEL_USER_ID,
                "tg_99" => ALT_CHANNEL_USER_ID,
                _ => panic!("missing canonical channel user fixture for {platform_user_id}"),
            };
            let user = ChannelUserRow {
                channel_user_id: channel_user_id.to_owned(),
                platform_user_id: platform_user_id.to_owned(),
                platform_type: platform_type.to_owned(),
                channel_plugin_id: Some(TEST_CHANNEL_PLUGIN_ID.to_owned()),
                display_name: Some("Test User".into()),
                authorized_at: now_ms(),
                last_active: None,
            };
            self.users.lock().unwrap().push(user);
        }

        /// Seeds a bot channel row bound to a public agent, so the per-bot
        /// auto-serve gate (`SessionManager::channel_public_agent_id`) resolves it.
        fn add_public_agent_channel(&self, channel_plugin_id: &str, public_agent_id: &str) {
            self.plugins.lock().unwrap().push(ChannelPluginRow {
                channel_plugin_id: channel_plugin_id.to_owned(),
                r#type: "telegram".to_owned(),
                name: "Telegram Bot".to_owned(),
                enabled: true,
                config: "{}".to_owned(),
                status: None,
                last_connected: None,
                companion_id: None,
                public_agent_id: Some(public_agent_id.to_owned()),
                bot_key: None,
                created_at: now_ms(),
                updated_at: now_ms(),
            });
        }
    }

    #[async_trait::async_trait]
    impl IChannelRepository for MockRepo {
        async fn get_all_plugins(&self) -> Result<Vec<ChannelPluginRow>, DbError> {
            Ok(self.plugins.lock().unwrap().clone())
        }
        async fn get_plugin(&self, channel_plugin_id: &str) -> Result<Option<ChannelPluginRow>, DbError> {
            Ok(self
                .plugins
                .lock()
                .unwrap()
                .iter()
                .find(|p| p.channel_plugin_id == channel_plugin_id)
                .cloned())
        }
        async fn create_plugin(&self, row: &NewChannelPluginRow) -> Result<ChannelPluginRow, DbError> {
            let mut plugins = self.plugins.lock().unwrap();
            let persisted = ChannelPluginRow {
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
            };
            plugins.push(persisted.clone());
            Ok(persisted)
        }
        async fn update_plugin(&self, row: &ChannelPluginRow) -> Result<ChannelPluginRow, DbError> {
            let mut plugins = self.plugins.lock().unwrap();
            let plugin = plugins
                .iter_mut()
                .find(|plugin| plugin.channel_plugin_id == row.channel_plugin_id)
                .ok_or_else(|| DbError::NotFound(row.channel_plugin_id.clone()))?;
            *plugin = row.clone();
            Ok(plugin.clone())
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

        async fn get_all_users(&self) -> Result<Vec<ChannelUserRow>, DbError> {
            Ok(self.users.lock().unwrap().clone())
        }
        async fn get_user_by_platform(
            &self,
            platform_user_id: &str,
            platform_type: &str,
            channel_plugin_id: &str,
        ) -> Result<Option<ChannelUserRow>, DbError> {
            let users = self.users.lock().unwrap();
            Ok(users
                .iter()
                .find(|u| {
                    u.platform_user_id == platform_user_id
                        && u.platform_type == platform_type
                        && u.channel_plugin_id.as_deref() == Some(channel_plugin_id)
                })
                .cloned())
        }
        async fn create_user(&self, row: &NewChannelUserRow) -> Result<ChannelUserRow, DbError> {
            let mut users = self.users.lock().unwrap();
            let persisted = ChannelUserRow {
                channel_user_id: nomifun_common::generate_id(),
                platform_user_id: row.platform_user_id.clone(),
                platform_type: row.platform_type.clone(),
                channel_plugin_id: row.channel_plugin_id.clone(),
                display_name: row.display_name.clone(),
                authorized_at: row.authorized_at,
                last_active: row.last_active,
            };
            users.push(persisted.clone());
            Ok(persisted)
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
            if let Some(existing) = sessions.iter_mut().find(|s| {
                s.channel_user_id == channel_user_id
                    && s.chat_id.as_deref() == Some(chat_id)
                    && s.channel_plugin_id.as_deref() == Some(channel_plugin_id)
            }) {
                existing.last_activity = new_row.last_activity;
                return Ok(existing.clone());
            }
            let persisted = ChannelSessionRow {
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
            sessions.push(persisted.clone());
            Ok(persisted)
        }
        async fn update_session_activity(&self, _id: &str, _last_activity: TimestampMs) -> Result<(), DbError> {
            Ok(())
        }
        async fn update_session_conversation(&self, id: &str, conversation_id: &str) -> Result<(), DbError> {
            let mut sessions = self.sessions.lock().unwrap();
            if let Some(s) = sessions.iter_mut().find(|s| s.channel_session_id == id) {
                s.conversation_id = Some(conversation_id.to_owned());
                Ok(())
            } else {
                Err(DbError::NotFound(id.into()))
            }
        }
        async fn update_session_agent_type(&self, id: &str, agent_type: &str) -> Result<(), DbError> {
            let mut sessions = self.sessions.lock().unwrap();
            if let Some(s) = sessions.iter_mut().find(|s| s.channel_session_id == id) {
                s.agent_type = agent_type.to_owned();
                Ok(())
            } else {
                Err(DbError::NotFound(id.into()))
            }
        }
        async fn delete_sessions_by_user(&self, channel_user_id: &str) -> Result<(), DbError> {
            self.sessions
                .lock()
                .unwrap()
                .retain(|s| s.channel_user_id != channel_user_id);
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

        async fn create_pairing(&self, row: &NewChannelPairingCodeRow) -> Result<ChannelPairingCodeRow, DbError> {
            let mut pairings = self.pairings.lock().unwrap();
            let persisted = ChannelPairingCodeRow {
                code: row.code.clone(),
                platform_user_id: row.platform_user_id.clone(),
                platform_type: row.platform_type.clone(),
                channel_plugin_id: row.channel_plugin_id.clone(),
                display_name: row.display_name.clone(),
                requested_at: row.requested_at,
                expires_at: row.expires_at,
                status: row.status.clone(),
            };
            pairings.push(persisted.clone());
            Ok(persisted)
        }
        async fn get_pending_pairings(&self) -> Result<Vec<ChannelPairingCodeRow>, DbError> {
            let pairings = self.pairings.lock().unwrap();
            Ok(pairings.iter().filter(|p| p.status == "pending").cloned().collect())
        }
        async fn get_pairing_by_code(&self, code: &str) -> Result<Option<ChannelPairingCodeRow>, DbError> {
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
        async fn cleanup_expired_pairings(&self, _now: TimestampMs) -> Result<u64, DbError> {
            Ok(0)
        }
    }

    // ── Mock IClientPreferenceRepository ──────────────────────────────

    struct MockPrefRepo;

    #[async_trait::async_trait]
    impl IClientPreferenceRepository for MockPrefRepo {
        async fn get_all(&self) -> Result<Vec<ClientPreference>, DbError> {
            Ok(vec![])
        }
        async fn get_by_keys(&self, _keys: &[&str]) -> Result<Vec<ClientPreference>, DbError> {
            Ok(vec![])
        }
        async fn upsert_batch(&self, _entries: &[(&str, &str)]) -> Result<(), DbError> {
            Ok(())
        }
        async fn delete_keys(&self, _keys: &[&str]) -> Result<(), DbError> {
            Ok(())
        }
    }

    /// Read-only pref repo seeded with fixed `(key, value)` rows — lets the
    /// pairing-bypass tests stand up a platform bound to a public agent (or a
    /// companion) without a real DB.
    struct SeededPrefRepo {
        data: Vec<(String, String)>,
    }

    impl SeededPrefRepo {
        fn new(entries: &[(&str, &str)]) -> Self {
            Self {
                data: entries.iter().map(|(k, v)| ((*k).to_owned(), (*v).to_owned())).collect(),
            }
        }
    }

    #[async_trait::async_trait]
    impl IClientPreferenceRepository for SeededPrefRepo {
        async fn get_all(&self) -> Result<Vec<ClientPreference>, DbError> {
            Ok(self
                .data
                .iter()
                .map(|(k, v)| ClientPreference {
                    id: 0,
                    key: k.clone(),
                    value: v.clone(),
                    updated_at: 0,
                })
                .collect())
        }
        async fn get_by_keys(&self, keys: &[&str]) -> Result<Vec<ClientPreference>, DbError> {
            Ok(self
                .data
                .iter()
                .filter(|(k, _)| keys.contains(&k.as_str()))
                .map(|(k, v)| ClientPreference {
                    id: 0,
                    key: k.clone(),
                    value: v.clone(),
                    updated_at: 0,
                })
                .collect())
        }
        async fn upsert_batch(&self, _entries: &[(&str, &str)]) -> Result<(), DbError> {
            Ok(())
        }
        async fn delete_keys(&self, _keys: &[&str]) -> Result<(), DbError> {
            Ok(())
        }
    }

    // ── Test helpers ───────────────────────────────────────────────────

    fn setup() -> (ActionExecutor, Arc<MockRepo>) {
        let repo = Arc::new(MockRepo::new());
        let broadcaster = Arc::new(MockBroadcaster);
        let pairing = Arc::new(PairingService::new(repo.clone(), broadcaster, "owner-a"));
        let session_mgr = Arc::new(SessionManager::new(repo.clone()));
        let pref_repo: Arc<dyn IClientPreferenceRepository> = Arc::new(MockPrefRepo);
        let settings = Arc::new(ChannelSettingsService::new(pref_repo));
        let executor = ActionExecutor::new(pairing, session_mgr, settings, "acp");
        (executor, repo)
    }

    /// Like `setup()` but with the settings service backed by seeded preference
    /// rows (used to bind a platform to a public agent / companion).
    fn setup_with_prefs(entries: &[(&str, &str)]) -> (ActionExecutor, Arc<MockRepo>) {
        let repo = Arc::new(MockRepo::new());
        let broadcaster = Arc::new(MockBroadcaster);
        let pairing = Arc::new(PairingService::new(repo.clone(), broadcaster, "owner-a"));
        let session_mgr = Arc::new(SessionManager::new(repo.clone()));
        let pref_repo: Arc<dyn IClientPreferenceRepository> = Arc::new(SeededPrefRepo::new(entries));
        let settings = Arc::new(ChannelSettingsService::new(pref_repo));
        let executor = ActionExecutor::new(pairing, session_mgr, settings, "acp");
        (executor, repo)
    }

    fn make_text_message(user_id: &str, chat_id: &str, text: &str, platform: PluginType) -> UnifiedIncomingMessage {
        UnifiedIncomingMessage {
            id: "msg_1".into(),
            platform,
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
        platform: PluginType,
        params: Option<HashMap<String, String>>,
    ) -> UnifiedIncomingMessage {
        UnifiedIncomingMessage {
            id: "msg_1".into(),
            platform,
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
            timestamp: now_ms(),
            reply_to_message_id: None,
            action: Some(UnifiedAction {
                action: action_name.into(),
                category,
                params,
                context: ActionContext {
                    platform,
                    user_id: user_id.into(),
                    chat_id: chat_id.into(),
                    message_id: None,
                    session_id: None,
                },
            }),
            raw: None,
        }
    }

    // ── Authorization tests ────────────────────────────────────────────

    #[tokio::test]
    async fn unauthorized_user_gets_pairing_response() {
        let (executor, _repo) = setup();
        let msg = make_text_message("tg_42", "chat_1", "Hello", PluginType::Telegram);

        let result = executor
            .handle_incoming_message(&msg, TEST_CHANNEL_PLUGIN_ID)
            .await
            .unwrap();
        match result {
            MessageResult::Action(resp) => {
                assert_eq!(resp.behavior, ActionBehavior::Send);
                let text = resp.text.unwrap();
                assert!(text.contains("pairing code"));
                assert!(resp.buttons.is_some());
            }
            _ => panic!("Expected Action result for unauthorized user"),
        }
    }

    #[tokio::test]
    async fn authorized_user_text_dispatches_to_agent() {
        let (executor, repo) = setup();
        repo.add_authorized_user("tg_42", "telegram");

        let msg = make_text_message("tg_42", "chat_1", "Hello AI", PluginType::Telegram);
        let result = executor
            .handle_incoming_message(&msg, TEST_CHANNEL_PLUGIN_ID)
            .await
            .unwrap();

        match result {
            MessageResult::Dispatched { session_id, .. } => {
                assert!(!session_id.is_empty());
            }
            _ => panic!("Expected Dispatched result for authorized user"),
        }
    }

    // ── 对外伙伴 pairing bypass (public-agent-bound platforms only) ─────────

    /// A bot BOUND to a public agent auto-serves an unknown sender with NO
    /// pairing code — the public-agent session is hard-clamped, so this is safe.
    /// Per-bot: the binding lives on the arriving channel row.
    #[tokio::test]
    async fn public_agent_bound_platform_auto_serves_unknown_sender() {
        // No authorized user; the bot (channel `tg-1`) is bound to a public agent.
        let (executor, repo) = setup();
        repo.add_public_agent_channel(TEST_CHANNEL_PLUGIN_ID, "pubagent_1");

        let msg = make_text_message("tg_stranger", "chat_1", "hi", PluginType::Telegram);
        let result = executor
            .handle_incoming_message(&msg, TEST_CHANNEL_PLUGIN_ID)
            .await
            .unwrap();

        match result {
            MessageResult::Dispatched { session_id, .. } => assert!(!session_id.is_empty()),
            other => panic!("stranger on a public-agent bot must be auto-served, got {other:?}"),
        }

        // Regression guard: auto-serve must REGISTER the stranger in
        // channel_users because channel_sessions.channel_user_id is the
        // authoritative logical relation.
        // The earlier bug returned the owner's `users` id (not a channel_users
        // id), which violated the FK on session creation.
        assert!(
            repo.get_user_by_platform(
                "tg_stranger",
                "telegram",
                TEST_CHANNEL_PLUGIN_ID,
            )
                .await
                .unwrap()
                .is_some(),
            "auto-served stranger must be auto-registered as the channel session's logical parent"
        );
    }

    /// The bypass is STRICTLY gated on a public-agent binding: a COMPANION-bound
    /// bot still gates unknown senders behind pairing (never loosened).
    #[tokio::test]
    async fn companion_bound_platform_still_gates_unknown_sender() {
        // Companion bound, but NO public-agent binding on the row → pairing gate
        // intact. (channel_public_agent_id returns None for a row with no
        // public_agent_id, and here there's no row at all.)
        let (executor, _repo) =
            setup_with_prefs(&[("channels.telegram.companion_id", "\"companion_1\"")]);

        let msg = make_text_message("tg_stranger", "chat_1", "hi", PluginType::Telegram);
        let result = executor
            .handle_incoming_message(&msg, TEST_CHANNEL_PLUGIN_ID)
            .await
            .unwrap();

        match result {
            MessageResult::Action(resp) => {
                let text = resp.text.unwrap();
                assert!(text.contains("pairing code"), "companion platform must still require pairing");
            }
            other => panic!("companion-bound platform must gate strangers, got {other:?}"),
        }
    }

    /// An UNBOUND platform keeps the pairing gate for unknown senders (control).
    #[tokio::test]
    async fn unbound_platform_still_gates_unknown_sender() {
        let (executor, _repo) = setup();

        let msg = make_text_message("tg_stranger", "chat_1", "hi", PluginType::Telegram);
        let result = executor
            .handle_incoming_message(&msg, TEST_CHANNEL_PLUGIN_ID)
            .await
            .unwrap();

        assert!(
            matches!(result, MessageResult::Action(_)),
            "unbound platform must gate an unknown sender behind pairing"
        );
    }

    // ── Platform action tests ──────────────────────────────────────────

    #[tokio::test]
    async fn pairing_show_generates_code() {
        let (executor, repo) = setup();
        repo.add_authorized_user("tg_42", "telegram");

        let msg = make_action_message(
            "tg_42",
            "chat_1",
            "pairing.show",
            ActionCategory::Platform,
            PluginType::Telegram,
            None,
        );
        let result = executor
            .handle_incoming_message(&msg, TEST_CHANNEL_PLUGIN_ID)
            .await
            .unwrap();

        match result {
            MessageResult::Action(resp) => {
                let text = resp.text.unwrap();
                assert!(text.contains("pairing code"));
            }
            _ => panic!("Expected Action result"),
        }
    }

    #[tokio::test]
    async fn pairing_check_authorized() {
        let (executor, repo) = setup();
        repo.add_authorized_user("tg_42", "telegram");

        let msg = make_action_message(
            "tg_42",
            "chat_1",
            "pairing.check",
            ActionCategory::Platform,
            PluginType::Telegram,
            None,
        );
        let result = executor
            .handle_incoming_message(&msg, TEST_CHANNEL_PLUGIN_ID)
            .await
            .unwrap();

        match result {
            MessageResult::Action(resp) => {
                let text = resp.text.unwrap();
                assert!(text.contains("authorized"));
            }
            _ => panic!("Expected Action result"),
        }
    }

    #[tokio::test]
    async fn pairing_check_not_authorized() {
        let (executor, repo) = setup();
        repo.add_authorized_user("tg_42", "telegram");

        let msg = make_action_message(
            "tg_99", // different user
            "chat_1",
            "pairing.check",
            ActionCategory::Platform,
            PluginType::Telegram,
            None,
        );
        // tg_99 is not authorized, but the action itself needs the user to be authorized
        // first (it's routed via handle_incoming_message which checks auth first)
        // So for this test, authorize tg_99 too
        repo.add_authorized_user("tg_99", "telegram");

        let result = executor
            .handle_incoming_message(&msg, TEST_CHANNEL_PLUGIN_ID)
            .await
            .unwrap();
        match result {
            MessageResult::Action(resp) => {
                let text = resp.text.unwrap();
                // tg_99 is authorized
                assert!(text.contains("authorized"));
            }
            _ => panic!("Expected Action result"),
        }
    }

    #[tokio::test]
    async fn pairing_help_returns_instructions() {
        let (executor, repo) = setup();
        repo.add_authorized_user("tg_42", "telegram");

        let msg = make_action_message(
            "tg_42",
            "chat_1",
            "pairing.help",
            ActionCategory::Platform,
            PluginType::Telegram,
            None,
        );
        let result = executor
            .handle_incoming_message(&msg, TEST_CHANNEL_PLUGIN_ID)
            .await
            .unwrap();
        match result {
            MessageResult::Action(resp) => {
                let text = resp.text.unwrap();
                assert!(text.contains("authorization"));
            }
            _ => panic!("Expected Action result"),
        }
    }

    // ── System action tests ────────────────────────────────────────────

    #[tokio::test]
    async fn session_new_creates_session() {
        let (executor, repo) = setup();
        repo.add_authorized_user("tg_42", "telegram");

        let msg = make_action_message(
            "tg_42",
            "chat_1",
            "session.new",
            ActionCategory::System,
            PluginType::Telegram,
            None,
        );
        let result = executor
            .handle_incoming_message(&msg, TEST_CHANNEL_PLUGIN_ID)
            .await
            .unwrap();
        match result {
            MessageResult::Action(resp) => {
                let text = resp.text.unwrap();
                assert!(text.contains("New session"));
                // With no client_preferences configured, defaults to "nomi"
                assert!(text.contains("nomi"));
            }
            _ => panic!("Expected Action result"),
        }
    }

    #[tokio::test]
    async fn session_new_resets_existing_session() {
        let (executor, repo) = setup();
        repo.add_authorized_user("tg_42", "telegram");

        // First: send a text message to create a session
        let text_msg = make_text_message("tg_42", "chat_1", "Hello", PluginType::Telegram);
        let r1 = executor
            .handle_incoming_message(&text_msg, TEST_CHANNEL_PLUGIN_ID)
            .await
            .unwrap();
        let sid1 = match r1 {
            MessageResult::Dispatched { session_id, .. } => session_id,
            _ => panic!("Expected Dispatched"),
        };

        // Then: session.new should delete old + create fresh
        let new_msg = make_action_message(
            "tg_42",
            "chat_1",
            "session.new",
            ActionCategory::System,
            PluginType::Telegram,
            None,
        );
        let r2 = executor
            .handle_incoming_message(&new_msg, TEST_CHANNEL_PLUGIN_ID)
            .await
            .unwrap();
        match r2 {
            MessageResult::Action(resp) => {
                let text = resp.text.unwrap();
                assert!(text.contains("New session"));
            }
            _ => panic!("Expected Action result"),
        }

        // Send another text message — the session ID should differ
        let text_msg2 = make_text_message("tg_42", "chat_1", "Again", PluginType::Telegram);
        let r3 = executor
            .handle_incoming_message(&text_msg2, TEST_CHANNEL_PLUGIN_ID)
            .await
            .unwrap();
        let sid3 = match r3 {
            MessageResult::Dispatched { session_id, .. } => session_id,
            _ => panic!("Expected Dispatched"),
        };
        // New session has different full ID (reset deleted the old one)
        assert_ne!(sid1, sid3);

        // Only 1 session should exist for this user+chat
        let sessions = repo.sessions.lock().unwrap();
        let user_chat_sessions: Vec<_> = sessions
            .iter()
            .filter(|s| {
                s.channel_user_id == TEST_CHANNEL_USER_ID
                    && s.chat_id.as_deref() == Some("chat_1")
            })
            .collect();
        assert_eq!(user_chat_sessions.len(), 1);
    }

    #[tokio::test]
    async fn session_status_shows_info() {
        let (executor, repo) = setup();
        repo.add_authorized_user("tg_42", "telegram");

        let msg = make_action_message(
            "tg_42",
            "chat_1",
            "session.status",
            ActionCategory::System,
            PluginType::Telegram,
            None,
        );
        let result = executor
            .handle_incoming_message(&msg, TEST_CHANNEL_PLUGIN_ID)
            .await
            .unwrap();
        match result {
            MessageResult::Action(resp) => {
                let text = resp.text.unwrap();
                assert!(text.contains("Session:"));
                assert!(text.contains("Agent:"));
            }
            _ => panic!("Expected Action result"),
        }
    }

    #[tokio::test]
    async fn help_show_returns_menu() {
        let (executor, repo) = setup();
        repo.add_authorized_user("tg_42", "telegram");

        let msg = make_action_message(
            "tg_42",
            "chat_1",
            "help.show",
            ActionCategory::System,
            PluginType::Telegram,
            None,
        );
        let result = executor
            .handle_incoming_message(&msg, TEST_CHANNEL_PLUGIN_ID)
            .await
            .unwrap();
        match result {
            MessageResult::Action(resp) => {
                assert!(resp.text.is_some());
                assert!(resp.buttons.is_some());
                let buttons = resp.buttons.unwrap();
                assert!(buttons.len() >= 2); // at least 2 rows
            }
            _ => panic!("Expected Action result"),
        }
    }

    #[tokio::test]
    async fn agent_select_with_params() {
        let (executor, repo) = setup();
        repo.add_authorized_user("tg_42", "telegram");

        let params = HashMap::from([("agentType".into(), "acp".into())]);
        let msg = make_action_message(
            "tg_42",
            "chat_1",
            "agent.select",
            ActionCategory::System,
            PluginType::Telegram,
            Some(params),
        );
        let result = executor
            .handle_incoming_message(&msg, TEST_CHANNEL_PLUGIN_ID)
            .await
            .unwrap();
        match result {
            MessageResult::Action(resp) => {
                let text = resp.text.unwrap();
                assert!(text.contains("acp"));
                assert!(resp.toast.is_some());
            }
            _ => panic!("Expected Action result"),
        }
    }

    #[tokio::test]
    async fn agent_select_persists_agent_type() {
        let (executor, repo) = setup();
        repo.add_authorized_user("tg_42", "telegram");

        // First: send a text to create a session (defaults to "nomi")
        let text_msg = make_text_message("tg_42", "chat_1", "Hello", PluginType::Telegram);
        executor
            .handle_incoming_message(&text_msg, TEST_CHANNEL_PLUGIN_ID)
            .await
            .unwrap();

        // Then: switch agent to "acp"
        let params = HashMap::from([("agentType".into(), "acp".into())]);
        let select_msg = make_action_message(
            "tg_42",
            "chat_1",
            "agent.select",
            ActionCategory::System,
            PluginType::Telegram,
            Some(params),
        );
        executor
            .handle_incoming_message(&select_msg, TEST_CHANNEL_PLUGIN_ID)
            .await
            .unwrap();

        // Verify session's agent_type was updated in the repo
        let sessions = repo.sessions.lock().unwrap();
        let session = sessions
            .iter()
            .find(|s| {
                s.channel_user_id == TEST_CHANNEL_USER_ID
                    && s.chat_id.as_deref() == Some("chat_1")
            })
            .expect("session should exist");
        assert_eq!(session.agent_type, "acp");
    }

    // ── Chat action tests ──────────────────────────────────────────────

    #[tokio::test]
    async fn chat_continue_dispatches_fixed_text() {
        let (executor, repo) = setup();
        repo.add_authorized_user("tg_42", "telegram");

        let msg = make_action_message(
            "tg_42",
            "chat_1",
            "chat.continue",
            ActionCategory::Chat,
            PluginType::Telegram,
            None,
        );
        let result = executor
            .handle_incoming_message(&msg, TEST_CHANNEL_PLUGIN_ID)
            .await
            .unwrap();
        match result {
            MessageResult::DispatchedText {
                session_id,
                conversation_id,
                text,
            } => {
                assert!(!session_id.is_empty());
                // Fresh session — no conversation bound yet.
                assert_eq!(conversation_id, None);
                assert_eq!(text, CONTINUE_PROMPT);
            }
            other => panic!("Expected DispatchedText, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn chat_continue_carries_existing_conversation_binding() {
        let (executor, repo) = setup();
        repo.add_authorized_user("tg_42", "telegram");
        let bound_conversation_id = nomifun_common::ConversationId::new();

        // Create the session via a normal text message, then bind a
        // conversation to it like the message loop would.
        let text_msg = make_text_message("tg_42", "chat_1", "Hello", PluginType::Telegram);
        let sid = match executor
            .handle_incoming_message(&text_msg, TEST_CHANNEL_PLUGIN_ID)
            .await
            .unwrap()
        {
            MessageResult::Dispatched { session_id, .. } => session_id,
            other => panic!("Expected Dispatched, got {other:?}"),
        };
        repo.update_session_conversation(&sid, bound_conversation_id.as_ref())
            .await
            .unwrap();

        let msg = make_action_message(
            "tg_42",
            "chat_1",
            "chat.continue",
            ActionCategory::Chat,
            PluginType::Telegram,
            None,
        );
        match executor
            .handle_incoming_message(&msg, TEST_CHANNEL_PLUGIN_ID)
            .await
            .unwrap()
        {
            MessageResult::DispatchedText {
                session_id,
                conversation_id,
                ..
            } => {
                assert_eq!(session_id, sid);
                assert_eq!(
                    conversation_id.as_deref(),
                    Some(bound_conversation_id.as_ref())
                );
            }
            other => panic!("Expected DispatchedText, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn chat_regenerate_requests_message_loop_lookup() {
        let (executor, repo) = setup();
        repo.add_authorized_user("tg_42", "telegram");
        let bound_conversation_id = nomifun_common::ConversationId::new();

        let text_msg = make_text_message("tg_42", "chat_1", "Hello", PluginType::Telegram);
        let sid = match executor
            .handle_incoming_message(&text_msg, TEST_CHANNEL_PLUGIN_ID)
            .await
            .unwrap()
        {
            MessageResult::Dispatched { session_id, .. } => session_id,
            other => panic!("Expected Dispatched, got {other:?}"),
        };
        repo.update_session_conversation(&sid, bound_conversation_id.as_ref())
            .await
            .unwrap();

        let msg = make_action_message(
            "tg_42",
            "chat_1",
            "chat.regenerate",
            ActionCategory::Chat,
            PluginType::Telegram,
            None,
        );
        match executor
            .handle_incoming_message(&msg, TEST_CHANNEL_PLUGIN_ID)
            .await
            .unwrap()
        {
            MessageResult::RegenerateRequested {
                session_id,
                conversation_id,
            } => {
                assert_eq!(session_id, sid);
                assert_eq!(
                    conversation_id.as_deref(),
                    Some(bound_conversation_id.as_ref())
                );
            }
            other => panic!("Expected RegenerateRequested, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn chat_regenerate_without_conversation_still_resolves_session() {
        let (executor, repo) = setup();
        repo.add_authorized_user("tg_42", "telegram");

        let msg = make_action_message(
            "tg_42",
            "chat_1",
            "chat.regenerate",
            ActionCategory::Chat,
            PluginType::Telegram,
            None,
        );
        match executor
            .handle_incoming_message(&msg, TEST_CHANNEL_PLUGIN_ID)
            .await
            .unwrap()
        {
            MessageResult::RegenerateRequested {
                session_id,
                conversation_id,
            } => {
                assert!(!session_id.is_empty());
                // The message loop turns this into a "nothing to regenerate"
                // notice — the executor only reports the missing binding.
                assert_eq!(conversation_id, None);
            }
            other => panic!("Expected RegenerateRequested, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn system_confirm_returns_answer() {
        let (executor, repo) = setup();
        repo.add_authorized_user("tg_42", "telegram");

        let params = HashMap::from([("callId".into(), "call_123".into()), ("value".into(), "true".into())]);
        let msg = make_action_message(
            "tg_42",
            "chat_1",
            "system.confirm",
            ActionCategory::Chat,
            PluginType::Telegram,
            Some(params),
        );
        let result = executor
            .handle_incoming_message(&msg, TEST_CHANNEL_PLUGIN_ID)
            .await
            .unwrap();
        match result {
            MessageResult::Action(resp) => {
                assert_eq!(resp.behavior, ActionBehavior::Answer);
                assert_eq!(resp.toast.as_deref(), Some("Confirmed"));
            }
            _ => panic!("Expected Action result"),
        }
    }

    #[tokio::test]
    async fn action_copy_returns_answer() {
        let (executor, repo) = setup();
        repo.add_authorized_user("tg_42", "telegram");

        let msg = make_action_message(
            "tg_42",
            "chat_1",
            "action.copy",
            ActionCategory::Chat,
            PluginType::Telegram,
            None,
        );
        let result = executor
            .handle_incoming_message(&msg, TEST_CHANNEL_PLUGIN_ID)
            .await
            .unwrap();
        match result {
            MessageResult::Action(resp) => {
                assert_eq!(resp.behavior, ActionBehavior::Answer);
                assert!(resp.toast.as_deref().unwrap().contains("Copied"));
            }
            _ => panic!("Expected Action result"),
        }
    }

    // ── Unknown action tests ───────────────────────────────────────────

    #[tokio::test]
    async fn unknown_platform_action() {
        let (executor, repo) = setup();
        repo.add_authorized_user("tg_42", "telegram");

        let msg = make_action_message(
            "tg_42",
            "chat_1",
            "unknown.action",
            ActionCategory::Platform,
            PluginType::Telegram,
            None,
        );
        let result = executor
            .handle_incoming_message(&msg, TEST_CHANNEL_PLUGIN_ID)
            .await
            .unwrap();
        match result {
            MessageResult::Action(resp) => {
                let text = resp.text.unwrap();
                assert!(text.contains("Unknown action"));
            }
            _ => panic!("Expected Action result"),
        }
    }

    // ── build_pairing_response tests ───────────────────────────────────

    #[test]
    fn pairing_response_contains_code() {
        let resp = build_pairing_response("123456");
        let text = resp.text.unwrap();
        assert!(text.contains("123456"));
        assert!(text.contains("pairing code"));
        assert_eq!(resp.behavior, ActionBehavior::Send);
        assert!(resp.buttons.is_some());
    }

    #[test]
    fn help_response_has_buttons() {
        let resp = build_help_response();
        assert!(resp.text.is_some());
        let buttons = resp.buttons.unwrap();
        assert!(!buttons.is_empty());
    }

    #[test]
    fn unknown_action_response_includes_name() {
        let resp = build_unknown_action_response("foo.bar");
        let text = resp.text.unwrap();
        assert!(text.contains("foo.bar"));
    }
}

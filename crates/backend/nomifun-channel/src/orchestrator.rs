use std::sync::Arc;

use tokio::sync::mpsc;
use tracing::{error, info, warn};

use crate::action::{ActionExecutor, MessageResult};
use crate::error::ChannelError;
use crate::message_service::ChannelMessageService;
use crate::session::SessionManager;
use crate::stream_relay::{ChannelSender, ChannelStreamRelay, RelayConfig};
use crate::types::{ActionBehavior, ChannelIncoming, OutgoingMessageType, UnifiedOutgoingMessage};

/// Reply sent when a new message arrives while the previous turn of the same
/// chat is still being processed (per-chat concurrency guard).
const BUSY_NOTICE: &str =
    "\u{23f3} Your previous message is still being processed \u{2014} please wait for it to finish.";

/// Reply for `chat.regenerate` when there is no user message to resend.
const NOTHING_TO_REGENERATE: &str =
    "\u{2139}\u{fe0f} There is no previous message to regenerate \u{2014} send a new message first.";

/// Orchestrates the full channel message lifecycle.
///
/// Consumes incoming IM messages from `message_rx` and tool confirmation
/// callbacks from `confirm_rx`, driving the pipeline:
/// 1. ActionExecutor routing (auth → action/AI dispatch)
/// 2. For Dispatched: send_to_agent + spawn ChannelStreamRelay
/// 3. For Action: reply via plugin
/// 4. Forward tool confirmations to the agent
pub struct ChannelOrchestrator {
    action_executor: Arc<ActionExecutor>,
    message_service: Arc<ChannelMessageService>,
    session_manager: Arc<SessionManager>,
    sender: Arc<dyn ChannelSender>,
}

impl ChannelOrchestrator {
    pub fn new(
        action_executor: Arc<ActionExecutor>,
        message_service: Arc<ChannelMessageService>,
        session_manager: Arc<SessionManager>,
        sender: Arc<dyn ChannelSender>,
    ) -> Self {
        Self {
            action_executor,
            message_service,
            session_manager,
            sender,
        }
    }

    /// Start the message loop. Runs until both channels close.
    pub async fn run(
        self,
        mut message_rx: mpsc::Receiver<ChannelIncoming>,
        mut confirm_rx: mpsc::Receiver<(String, String)>,
    ) {
        info!("ChannelOrchestrator started");

        loop {
            tokio::select! {
                Some(incoming) = message_rx.recv() => {
                    self.handle_message(incoming).await;
                }
                Some((call_id, value)) = confirm_rx.recv() => {
                    handle_confirm(&call_id, &value);
                }
                else => break,
            }
        }

        info!("ChannelOrchestrator stopped (channels closed)");
    }

    async fn handle_message(&self, incoming: ChannelIncoming) {
        let ChannelIncoming { channel_id, message: msg } = incoming;
        let platform = msg.platform;
        let chat_id = msg.chat_id.clone();
        // Outgoing routing is per channel row — the manager keys running
        // bot instances by their `assistant_plugins` row id.
        let plugin_id = channel_id.clone();
        let text = msg.content.text.clone();

        let executor = Arc::clone(&self.action_executor);
        let msg_svc = Arc::clone(&self.message_service);
        let session_mgr = Arc::clone(&self.session_manager);
        let sender = Arc::clone(&self.sender);

        tokio::spawn(async move {
            match executor.handle_incoming_message(&msg, &channel_id).await {
                Ok(MessageResult::Action(response)) => {
                    send_action_response(&sender, &plugin_id, &chat_id, &response).await;
                }
                Ok(MessageResult::Dispatched {
                    session_id,
                    conversation_id,
                }) => {
                    handle_dispatched(
                        &msg_svc,
                        &session_mgr,
                        &sender,
                        &session_id,
                        conversation_id.as_deref(),
                        &text,
                        platform,
                        &plugin_id,
                        &chat_id,
                    )
                    .await;
                }
                Ok(MessageResult::DispatchedText {
                    session_id,
                    conversation_id,
                    text: synthesized,
                }) => {
                    // chat.continue: same pipeline as a typed message, with a
                    // synthesized prompt instead of the callback payload text.
                    handle_dispatched(
                        &msg_svc,
                        &session_mgr,
                        &sender,
                        &session_id,
                        conversation_id.as_deref(),
                        &synthesized,
                        platform,
                        &plugin_id,
                        &chat_id,
                    )
                    .await;
                }
                Ok(MessageResult::RegenerateRequested {
                    session_id,
                    conversation_id,
                }) => {
                    handle_regenerate(
                        &msg_svc,
                        &session_mgr,
                        &sender,
                        &session_id,
                        conversation_id.as_deref(),
                        platform,
                        &plugin_id,
                        &chat_id,
                    )
                    .await;
                }
                Ok(MessageResult::AlreadyProcessing) => {
                    info!(chat_id = %chat_id, "message ignored: already processing");
                    let _ = sender
                        .send_message(&plugin_id, &chat_id, plain_text_message(BUSY_NOTICE.into()))
                        .await;
                }
                Err(e) => {
                    error!(error = %e, "failed to handle incoming message");
                }
            }
        });
    }
}

async fn send_action_response(
    sender: &Arc<dyn ChannelSender>,
    plugin_id: &str,
    chat_id: &str,
    response: &crate::types::ActionResponse,
) {
    if let Some(text) = &response.text {
        let outgoing = UnifiedOutgoingMessage {
            message_type: OutgoingMessageType::Text,
            text: Some(text.clone()),
            parse_mode: response.parse_mode,
            buttons: response.buttons.clone(),
            keyboard: response.keyboard.clone(),
            image_url: None,
            file_url: None,
            file_name: None,
            media_actions: None,
            reply_to_message_id: None,
            silent: None,
        };

        match response.behavior {
            ActionBehavior::Edit => {
                if let Some(ref edit_id) = response.edit_message_id {
                    let _ = sender.edit_message(plugin_id, chat_id, edit_id, outgoing).await;
                }
            }
            _ => {
                let _ = sender.send_message(plugin_id, chat_id, outgoing).await;
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn handle_dispatched(
    msg_svc: &Arc<ChannelMessageService>,
    session_mgr: &Arc<SessionManager>,
    sender: &Arc<dyn ChannelSender>,
    session_id: &str,
    conversation_id: Option<&str>,
    text: &str,
    platform: crate::types::PluginType,
    plugin_id: &str,
    chat_id: &str,
) {
    // Decision interception (Bug 1, Case A): when the bound conversation is
    // waiting on a relayed numbered decision, a reply is the user's *answer*,
    // not a new prompt. Map a valid number onto an option and resolve it via
    // `confirm`; re-show the list on any other reply. Runs before the busy
    // guard because the conversation is intentionally blocked on the decision.
    if let Some(cid) = conversation_id
        && let Some(pending) = msg_svc.pending_decisions().peek(cid)
    {
        match parse_choice(text, pending.options.len()) {
            Some(idx) => {
                let option = &pending.options[idx];
                match msg_svc.submit_decision(cid, &pending.call_id, &option.option_id).await {
                    Ok(()) => {
                        msg_svc.pending_decisions().take(cid);
                        info!(conversation_id = %cid, option_id = %option.option_id, "channel decision resolved");
                        let _ = sender
                            .send_message(
                                plugin_id,
                                chat_id,
                                plain_text_message(format!("\u{2705} 已选择：{}", option.label)),
                            )
                            .await;
                    }
                    Err(e) => {
                        // The decision can no longer be submitted — most often it
                        // was already answered from the desktop UI, or the turn
                        // ended. Clear the stale entry so the user's next message
                        // dispatches normally instead of being trapped on it.
                        msg_svc.pending_decisions().take(cid);
                        error!(error = %e, conversation_id = %cid, "channel decision submit failed; cleared stale pending");
                        let _ = sender
                            .send_message(
                                plugin_id,
                                chat_id,
                                plain_text_message(format!(
                                    "\u{274c} 该决策已无法提交（可能已在桌面处理）：{e}。已清除等待，请重新发送你的指令。"
                                )),
                            )
                            .await;
                    }
                }
            }
            None => {
                // Non-numeric / out-of-range reply: re-show the numbered list
                // (do not dispatch it as a new prompt).
                let msg = ChannelMessageService::build_decision_message(&pending.prompt, &pending.options);
                let _ = sender.send_message(plugin_id, chat_id, msg).await;
            }
        }
        return;
    }

    // Per-chat concurrency guard: when the bound conversation is already
    // working on a turn, don't race a second prompt into it (the turn claim
    // would reject it with an opaque error anyway) — tell the user instead.
    if let Some(cid) = conversation_id
        && msg_svc.is_conversation_busy(cid).await
    {
        info!(conversation_id = %cid, chat_id = %chat_id, "message rejected: conversation busy");
        let _ = sender
            .send_message(plugin_id, chat_id, plain_text_message(BUSY_NOTICE.into()))
            .await;
        return;
    }

    let session = match session_mgr.get_session_by_id(session_id).await {
        Ok(Some(s)) => s,
        Ok(None) => {
            warn!(session_id = %session_id, "session not found after dispatch");
            return;
        }
        Err(e) => {
            error!(error = %e, "failed to get session");
            return;
        }
    };

    let send_result = match msg_svc.send_to_agent(&session, text, platform).await {
        Ok(r) => r,
        // The (now shared) companion session is already running a turn — answer
        // with the same friendly notice the per-chat busy guard uses. Covers the
        // first-turn race the guard above can't see (it checks the pre-bind id).
        Err(ChannelError::ConversationBusy) => {
            info!(chat_id = %chat_id, "message rejected: companion session busy");
            let _ = sender
                .send_message(plugin_id, chat_id, plain_text_message(BUSY_NOTICE.into()))
                .await;
            return;
        }
        // Companion bound but not yet usable (no model) — relay the plain notice,
        // not the generic ❌ failure line.
        Err(e @ ChannelError::CompanionNotReady(_)) => {
            info!(chat_id = %chat_id, "message rejected: companion not ready");
            let _ = sender.send_message(plugin_id, chat_id, plain_text_message(e.to_string())).await;
            return;
        }
        Err(e) => {
            error!(error = %e, "failed to send to agent");
            let err_msg = plain_text_message(format!("\u{274c} Failed to process: {e}"));
            let _ = sender.send_message(plugin_id, chat_id, err_msg).await;
            return;
        }
    };

    // Bind the conversation to this per-chat session whenever the conversation
    // the turn actually ran on differs from the session's current binding: a
    // first turn (was None), or a companion turn rerouted into the companion's
    // shared single session (the per-chat session may still point at None or a
    // stale standalone id). Keeps the per-chat pointer in sync so the busy guard
    // and decision interception operate on the shared id on subsequent turns.
    if conversation_id != Some(send_result.conversation_id.as_str())
        && let Err(e) = session_mgr
            .bind_conversation(session_id, &send_result.conversation_id)
            .await
    {
        warn!(error = %e, "failed to bind conversation to session");
    }

    // Spawn stream relay if we got a subscription
    if let Some(rx) = send_result.stream_rx {
        let relay_config = RelayConfig {
            platform,
            plugin_id: plugin_id.to_owned(),
            chat_id: chat_id.to_owned(),
            throttle_ms: 500,
            conversation_id: send_result.conversation_id.clone(),
        };
        let relay = ChannelStreamRelay::new(
            relay_config,
            Arc::clone(sender),
            msg_svc.pending_decisions(),
            msg_svc.asset_resolver(),
        );
        tokio::spawn(relay.run(rx));
    } else {
        warn!(
            conversation_id = %send_result.conversation_id,
            "no agent task for stream subscription"
        );
    }
}

/// Handles `chat.regenerate`: look up the conversation's last user message
/// and resend it through the regular dispatch path (streaming reply
/// included). Falls back to a notice when there is nothing to resend.
#[allow(clippy::too_many_arguments)]
async fn handle_regenerate(
    msg_svc: &Arc<ChannelMessageService>,
    session_mgr: &Arc<SessionManager>,
    sender: &Arc<dyn ChannelSender>,
    session_id: &str,
    conversation_id: Option<&str>,
    platform: crate::types::PluginType,
    plugin_id: &str,
    chat_id: &str,
) {
    let Some(conversation_id) = conversation_id else {
        // Session has no backing conversation yet — nothing was ever asked.
        let _ = sender
            .send_message(plugin_id, chat_id, plain_text_message(NOTHING_TO_REGENERATE.into()))
            .await;
        return;
    };

    match msg_svc.last_user_text(conversation_id).await {
        Ok(Some(text)) => {
            handle_dispatched(
                msg_svc,
                session_mgr,
                sender,
                session_id,
                Some(conversation_id),
                &text,
                platform,
                plugin_id,
                chat_id,
            )
            .await;
        }
        Ok(None) => {
            let _ = sender
                .send_message(plugin_id, chat_id, plain_text_message(NOTHING_TO_REGENERATE.into()))
                .await;
        }
        Err(e) => {
            error!(error = %e, conversation_id = %conversation_id, "failed to load last user message for regenerate");
            let _ = sender
                .send_message(
                    plugin_id,
                    chat_id,
                    plain_text_message(format!("\u{274c} Failed to process: {e}")),
                )
                .await;
        }
    }
}

/// Builds a plain text outgoing message (no parse mode, no buttons).
fn plain_text_message(text: String) -> UnifiedOutgoingMessage {
    UnifiedOutgoingMessage {
        message_type: OutgoingMessageType::Text,
        text: Some(text),
        parse_mode: None,
        buttons: None,
        keyboard: None,
        image_url: None,
        file_url: None,
        file_name: None,
        media_actions: None,
        reply_to_message_id: None,
        silent: None,
    }
}

/// Forward a tool confirmation callback to the active agent.
fn handle_confirm(call_id: &str, value: &str) {
    // Channel conversations use yoloMode which auto-approves everything,
    // so this path is rarely hit. When needed, we can add a
    // call_id→conversation_id lookup via IWorkerTaskManager.
    info!(call_id = %call_id, value = %value, "forwarding tool confirmation");
}

/// Parses a channel user's numbered-decision reply into a 0-based option
/// index, valid only for `1..=n` (where `n` is the option count).
///
/// Returns `None` for non-numeric, out-of-range, or empty replies so the
/// caller can re-show the numbered list instead of dispatching the text.
fn parse_choice(text: &str, n: usize) -> Option<usize> {
    let choice: usize = text.trim().parse().ok()?;
    if choice >= 1 && choice <= n {
        Some(choice - 1)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_choice_valid_indices() {
        assert_eq!(parse_choice("1", 2), Some(0));
        assert_eq!(parse_choice("2", 2), Some(1));
        // Surrounding whitespace is tolerated.
        assert_eq!(parse_choice("  2  ", 3), Some(1));
        assert_eq!(parse_choice("\n1\t", 3), Some(0));
    }

    #[test]
    fn parse_choice_out_of_range() {
        assert_eq!(parse_choice("0", 2), None, "1-based: 0 is invalid");
        assert_eq!(parse_choice("3", 2), None, "beyond option count");
        assert_eq!(parse_choice("1", 0), None, "no options at all");
    }

    #[test]
    fn parse_choice_non_numeric() {
        assert_eq!(parse_choice("hello", 2), None);
        assert_eq!(parse_choice("", 2), None);
        assert_eq!(parse_choice("1.5", 2), None);
        assert_eq!(parse_choice("-1", 2), None);
        assert_eq!(parse_choice("1 2", 2), None, "two numbers is not a single choice");
    }
}

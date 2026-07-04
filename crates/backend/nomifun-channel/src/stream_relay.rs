use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use nomifun_ai_agent::AgentStreamEvent;
use tokio::sync::broadcast;
use tracing::{debug, error, info, warn};

use crate::error::ChannelError;
use crate::formatter::format_text_for_platform;
use crate::message_service::{ChannelMessageService, StreamAction};
use crate::pending_decision::{PendingDecision, PendingDecisionStore};
use crate::types::{OutgoingMessageType, ParseMode, PluginType, UnifiedOutgoingMessage};

/// Configuration for a stream relay session.
#[derive(Debug, Clone)]
pub struct RelayConfig {
    pub platform: PluginType,
    pub plugin_id: String,
    pub chat_id: String,
    pub throttle_ms: u64,
    /// The backing conversation, so a relayed decision can be recorded
    /// against it in the shared pending-decision store.
    pub conversation_id: String,
}

/// Abstraction for sending/editing messages through a channel plugin.
///
/// Decouples ChannelStreamRelay from ChannelManager for testability.
#[async_trait]
pub trait ChannelSender: Send + Sync {
    async fn send_message(
        &self,
        plugin_id: &str,
        chat_id: &str,
        message: UnifiedOutgoingMessage,
    ) -> Result<String, ChannelError>;

    async fn edit_message(
        &self,
        plugin_id: &str,
        chat_id: &str,
        message_id: &str,
        message: UnifiedOutgoingMessage,
    ) -> Result<(), ChannelError>;
}

/// Relays agent stream events to an IM platform.
///
/// Responsibilities:
/// - Send "Thinking..." placeholder on start
/// - Accumulate text, throttled editMessage every N ms
/// - Send final message with action buttons on Finish
/// - Send error message on Error
pub struct ChannelStreamRelay {
    config: RelayConfig,
    sender: Arc<dyn ChannelSender>,
    /// Shared store: a relayed decision is recorded here so the inbound
    /// numeric reply can be mapped back to the right `call_id`/option.
    pending: Arc<PendingDecisionStore>,
}

impl ChannelStreamRelay {
    pub fn new(config: RelayConfig, sender: Arc<dyn ChannelSender>, pending: Arc<PendingDecisionStore>) -> Self {
        Self {
            config,
            sender,
            pending,
        }
    }

    /// Run the relay loop until the agent stream ends.
    pub async fn run(self, rx: broadcast::Receiver<AgentStreamEvent>) {
        if is_send_once_platform(self.config.platform) {
            self.run_send_once(rx).await;
        } else {
            self.run_editable(rx).await;
        }
    }

    /// Send-once relay (WeChat/Twitch/Nostr): no edit support, accumulate text
    /// then send once.
    async fn run_send_once(self, mut rx: broadcast::Receiver<AgentStreamEvent>) {
        let mut text_buffer = String::new();
        let mut has_content = false;

        loop {
            match rx.recv().await {
                Ok(event) => match ChannelMessageService::process_stream_event(&event) {
                    Some(StreamAction::AppendText(chunk)) => {
                        text_buffer.push_str(&chunk);
                        has_content = true;
                    }
                    Some(StreamAction::Thinking(_)) => {}
                    Some(StreamAction::ToolCall { .. }) if has_content && !text_buffer.trim().is_empty() => {
                        let formatted = format_text_for_platform(&text_buffer, self.config.platform);
                        let flush_msg = ChannelMessageService::build_streaming_message(&formatted);
                        let _ = self
                            .sender
                            .send_message(&self.config.plugin_id, &self.config.chat_id, flush_msg)
                            .await;
                        text_buffer.clear();
                        has_content = false;
                    }
                    Some(StreamAction::ToolCall { .. }) => {}
                    // A blocking decision: record it and forward a numbered
                    // list as a new message. WeChat cannot edit, so this is a
                    // fresh send_message either way.
                    Some(StreamAction::Decision { call_id, prompt, options }) => {
                        self.record_and_send_decision(call_id, prompt, options).await;
                    }
                    Some(StreamAction::Finish) => {
                        if has_content && !text_buffer.trim().is_empty() {
                            let formatted = format_text_for_platform(&text_buffer, self.config.platform);
                            let final_msg = ChannelMessageService::build_final_message(&formatted);
                            let _ = self
                                .sender
                                .send_message(&self.config.plugin_id, &self.config.chat_id, final_msg)
                                .await;
                        }
                        info!(
                            plugin_id = %self.config.plugin_id,
                            chat_id = %self.config.chat_id,
                            text_len = text_buffer.len(),
                            "channel stream relay finished (weixin)"
                        );
                        break;
                    }
                    Some(StreamAction::Error(msg)) => {
                        let error_msg = UnifiedOutgoingMessage {
                            message_type: OutgoingMessageType::Text,
                            text: Some(format!("\u{274c} {msg}")),
                            parse_mode: None,
                            buttons: None,
                            keyboard: None,
                            image_url: None,
                            file_url: None,
                            file_name: None,
                            media_actions: None,
                            reply_to_message_id: None,
                            silent: None,
                        };
                        let _ = self
                            .sender
                            .send_message(&self.config.plugin_id, &self.config.chat_id, error_msg)
                            .await;
                        break;
                    }
                    None => {}
                },
                Err(broadcast::error::RecvError::Closed) => {
                    if has_content && !text_buffer.trim().is_empty() {
                        let formatted = format_text_for_platform(&text_buffer, self.config.platform);
                        let final_msg = ChannelMessageService::build_final_message(&formatted);
                        let _ = self
                            .sender
                            .send_message(&self.config.plugin_id, &self.config.chat_id, final_msg)
                            .await;
                    }
                    break;
                }
                Err(broadcast::error::RecvError::Lagged(n)) => {
                    warn!(lagged = n, "channel stream relay lagged (weixin)");
                }
            }
        }

        debug!(
            plugin_id = %self.config.plugin_id,
            chat_id = %self.config.chat_id,
            "channel stream relay exited (weixin)"
        );
    }

    /// Standard relay for platforms that support edit (Telegram, Lark, DingTalk).
    async fn run_editable(self, mut rx: broadcast::Receiver<AgentStreamEvent>) {
        let throttle = Duration::from_millis(self.config.throttle_ms);

        let thinking_msg = ChannelMessageService::build_thinking_message();
        let thinking_msg_id = match self
            .sender
            .send_message(&self.config.plugin_id, &self.config.chat_id, thinking_msg)
            .await
        {
            Ok(id) => id,
            Err(e) => {
                error!(error = %e, "failed to send thinking message");
                return;
            }
        };

        let mut text_buffer = String::new();
        let mut last_edit = Instant::now() - throttle;
        let mut has_content = false;
        // Whether a blocking decision was forwarded during this turn. When a
        // decision is pending, the thinking/streaming card is deliberately left
        // intact so the turn stays live (see `record_and_send_decision`); we
        // must not replace it with a terminal "(no text output)" card on Finish.
        let mut decision_forwarded = false;

        loop {
            match rx.recv().await {
                Ok(event) => match ChannelMessageService::process_stream_event(&event) {
                    Some(StreamAction::AppendText(chunk)) => {
                        text_buffer.push_str(&chunk);
                        has_content = true;
                        if last_edit.elapsed() >= throttle {
                            let formatted = format_text_for_platform(&text_buffer, self.config.platform);
                            let mut msg = ChannelMessageService::build_streaming_message(&formatted);
                            msg.parse_mode = formatted_parse_mode(self.config.platform);
                            let _ = self
                                .sender
                                .edit_message(&self.config.plugin_id, &self.config.chat_id, &thinking_msg_id, msg)
                                .await;
                            last_edit = Instant::now();
                        }
                    }
                    Some(StreamAction::Thinking(_)) => {}
                    Some(StreamAction::ToolCall { name, .. }) => {
                        // Deliberately no parse mode: the tool name is raw
                        // agent output and is not HTML-escaped here.
                        let msg = ChannelMessageService::build_streaming_message(&format!("\u{23f3} {name}..."));
                        let _ = self
                            .sender
                            .edit_message(&self.config.plugin_id, &self.config.chat_id, &thinking_msg_id, msg)
                            .await;
                    }
                    // A blocking decision: record it and forward a numbered
                    // list as a new message; the thinking/streaming card is
                    // left intact and the turn stays live.
                    Some(StreamAction::Decision { call_id, prompt, options }) => {
                        decision_forwarded = true;
                        self.record_and_send_decision(call_id, prompt, options).await;
                    }
                    Some(StreamAction::Finish) => {
                        self.send_final_edit(&text_buffer, has_content, decision_forwarded, &thinking_msg_id)
                            .await;
                        info!(
                            plugin_id = %self.config.plugin_id,
                            chat_id = %self.config.chat_id,
                            text_len = text_buffer.len(),
                            "channel stream relay finished"
                        );
                        break;
                    }
                    Some(StreamAction::Error(msg)) => {
                        // Raw agent error text is not formatter-escaped, so
                        // it must stay plain text (no parse mode).
                        let error_msg = UnifiedOutgoingMessage {
                            message_type: OutgoingMessageType::Text,
                            text: Some(format!("\u{274c} {msg}")),
                            parse_mode: None,
                            buttons: None,
                            keyboard: None,
                            image_url: None,
                            file_url: None,
                            file_name: None,
                            media_actions: None,
                            reply_to_message_id: None,
                            silent: None,
                        };
                        let _ = self
                            .sender
                            .edit_message(
                                &self.config.plugin_id,
                                &self.config.chat_id,
                                &thinking_msg_id,
                                error_msg,
                            )
                            .await;
                        break;
                    }
                    None => {}
                },
                Err(broadcast::error::RecvError::Closed) => {
                    warn!("channel stream relay: broadcast closed without terminal event");
                    self.send_final_edit(&text_buffer, has_content, decision_forwarded, &thinking_msg_id)
                        .await;
                    break;
                }
                Err(broadcast::error::RecvError::Lagged(n)) => {
                    warn!(lagged = n, "channel stream relay lagged");
                }
            }
        }

        debug!(
            plugin_id = %self.config.plugin_id,
            chat_id = %self.config.chat_id,
            "channel stream relay exited"
        );
    }

    /// Finalize the turn by replacing the "Thinking..." placeholder.
    ///
    /// - With assistant text: render the formatted final card.
    /// - Without text but a decision was forwarded: leave the card intact — the
    ///   decision flow owns the live UX and the turn stays interactive.
    /// - Without text and no decision (tool-only / empty completion): the agent
    ///   reported a finished turn that produced no `Text` event. The placeholder
    ///   must still be replaced with a terminal card, otherwise the user is left
    ///   staring at "Thinking..." forever on an already-completed turn (the
    ///   silent-empty-reply failure class). Emit a neutral "(no text output)"
    ///   final card so the action buttons are delivered and the card is终态.
    async fn send_final_edit(&self, text_buffer: &str, has_content: bool, decision_forwarded: bool, msg_id: &str) {
        if has_content {
            let formatted = format_text_for_platform(text_buffer, self.config.platform);
            let mut final_msg = ChannelMessageService::build_final_message(&formatted);
            final_msg.parse_mode = formatted_parse_mode(self.config.platform);
            let _ = self
                .sender
                .edit_message(&self.config.plugin_id, &self.config.chat_id, msg_id, final_msg)
                .await;
        } else if !decision_forwarded {
            // Plain text — no formatter output here, so no parse mode.
            let final_msg = ChannelMessageService::build_final_message("（无文本输出）");
            let _ = self
                .sender
                .edit_message(&self.config.plugin_id, &self.config.chat_id, msg_id, final_msg)
                .await;
        }
    }

    /// Records a blocking decision against its conversation and forwards the
    /// numbered choice list as a new message. The streaming/thinking card is
    /// untouched and the turn stays live until the user answers (the inbound
    /// numeric reply resolves it via `ConversationService::confirm`).
    async fn record_and_send_decision(
        &self,
        call_id: String,
        prompt: String,
        options: Vec<crate::types::DecisionOption>,
    ) {
        self.pending.put(PendingDecision {
            conversation_id: self.config.conversation_id.clone(),
            call_id,
            prompt: prompt.clone(),
            options: options.clone(),
        });
        let msg = ChannelMessageService::build_decision_message(&prompt, &options);
        let _ = self
            .sender
            .send_message(&self.config.plugin_id, &self.config.chat_id, msg)
            .await;
    }
}

/// Parse mode for text that has been through `format_text_for_platform`.
///
/// The formatter emits HTML for Telegram (escaping `&`, `<`, `>` in the
/// source before inserting tags), so requesting `HTML` parse mode is both
/// safe and required — without it Telegram renders the tags literally
/// (`<b>hi</b>` shows up verbatim in the chat). Lark/DingTalk receive
/// markdown and WeChat plain text; they keep `None`.
///
/// Trade-off note: Telegram rejects malformed HTML with a 400. The
/// formatter's up-front entity escaping makes the body well-formed; the one
/// residual edge is a markdown link URL containing a double quote, which
/// would break the `href` attribute. That is pathological agent output and
/// the failed edit is logged rather than guarded against here.
fn formatted_parse_mode(platform: PluginType) -> Option<ParseMode> {
    match platform {
        PluginType::Telegram => Some(ParseMode::HTML),
        _ => None,
    }
}

/// Channels that cannot edit messages in place — each reply must be a new send,
/// so the relay buffers assistant text and sends it once (no streaming edits).
/// WeChat/WeCom (no edit API), Twitch (IRC chat), Nostr (immutable events), and
/// QQ Bot (no edit API + tight passive-reply window).
fn is_send_once_platform(platform: PluginType) -> bool {
    matches!(
        platform,
        PluginType::Weixin | PluginType::Wecom | PluginType::Twitch | PluginType::Nostr | PluginType::Qqbot
    )
}

// ── Test helpers (pub so integration tests can use them) ─────────

/// Records send/edit calls for test assertions.
pub struct MessageRecorder {
    sends: std::sync::Mutex<Vec<UnifiedOutgoingMessage>>,
    edits: std::sync::Mutex<Vec<UnifiedOutgoingMessage>>,
}

impl MessageRecorder {
    pub fn new() -> Self {
        Self {
            sends: std::sync::Mutex::new(Vec::new()),
            edits: std::sync::Mutex::new(Vec::new()),
        }
    }

    pub fn take_sends(&self) -> Vec<UnifiedOutgoingMessage> {
        std::mem::take(&mut self.sends.lock().unwrap())
    }

    pub fn take_edits(&self) -> Vec<UnifiedOutgoingMessage> {
        std::mem::take(&mut self.edits.lock().unwrap())
    }
}

impl Default for MessageRecorder {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl ChannelSender for MessageRecorder {
    async fn send_message(
        &self,
        _plugin_id: &str,
        _chat_id: &str,
        message: UnifiedOutgoingMessage,
    ) -> Result<String, ChannelError> {
        self.sends.lock().unwrap().push(message);
        Ok("msg-1".into())
    }

    async fn edit_message(
        &self,
        _plugin_id: &str,
        _chat_id: &str,
        _message_id: &str,
        message: UnifiedOutgoingMessage,
    ) -> Result<(), ChannelError> {
        self.edits.lock().unwrap().push(message);
        Ok(())
    }
}

use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use nomifun_ai_agent::AgentStreamEvent;
use sha2::{Digest, Sha256};
use tokio::sync::broadcast;
use tracing::{debug, error, info, warn};

use crate::error::ChannelError;
use crate::formatter::format_text_for_platform;
use crate::message_service::{ChannelMessageService, StreamAction};
use crate::pending_decision::{PendingDecision, PendingDecisionStore};
use crate::think_filter::{Stage, strip_reasoning};
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

    async fn send_media(
        &self,
        plugin_id: &str,
        chat_id: &str,
        media: crate::types::OutgoingMedia,
        caption: Option<&str>,
    ) -> Result<String, ChannelError>;
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
    /// Resolves workshop asset UUIDv7 ids to bytes for outbound media. `None`
    /// disables sending.
    asset_resolver: Option<Arc<dyn crate::message_service::AssetResolver>>,
}

/// A tool call has exactly one terminal projection per turn. Provider retries
/// and transport reordering must not let a late `Completed` frame reverse a
/// prior `Error` (or re-open a completed call with a progress frame).
#[derive(Default)]
struct TerminalToolCallGate {
    tool_calls: std::collections::HashSet<String>,
    acp_tool_calls: std::collections::HashSet<String>,
}

impl TerminalToolCallGate {
    fn accepts(&mut self, event: &AgentStreamEvent) -> bool {
        use nomifun_ai_agent::protocol::events::{AcpToolCallStatus, ToolCallStatus};

        match event {
            AgentStreamEvent::ToolCall(data) => match data.status {
                ToolCallStatus::Running => !self.tool_calls.contains(&data.call_id),
                ToolCallStatus::Completed | ToolCallStatus::Error => {
                    self.tool_calls.insert(data.call_id.clone())
                }
            },
            AgentStreamEvent::AcpToolCall(data) => match data.update.status {
                Some(AcpToolCallStatus::Completed | AcpToolCallStatus::Failed) => self
                    .acp_tool_calls
                    .insert(data.update.tool_call_id.clone()),
                Some(AcpToolCallStatus::Pending | AcpToolCallStatus::InProgress) | None => {
                    !self.acp_tool_calls.contains(&data.update.tool_call_id)
                }
            },
            _ => true,
        }
    }
}

impl ChannelStreamRelay {
    pub fn new(
        config: RelayConfig,
        sender: Arc<dyn ChannelSender>,
        pending: Arc<PendingDecisionStore>,
        asset_resolver: Option<Arc<dyn crate::message_service::AssetResolver>>,
    ) -> Self {
        Self {
            config,
            sender,
            pending,
            asset_resolver,
        }
    }

    /// Send a durable Workshop asset id when the channel cannot upload bytes.
    ///
    /// `true` means the channel acknowledged the locator message. Callers must
    /// not treat a best-effort send as delivery: if both media upload and this
    /// fallback fail, the turn has no user-visible artifact and must fail
    /// closed.
    async fn send_workshop_asset_fallback(&self, asset_id: &str, reason: &str) -> bool {
        let message = UnifiedOutgoingMessage {
            message_type: OutgoingMessageType::Text,
            text: Some(format!(
                "Generated Workshop asset could not be uploaded ({reason}). Asset ID: {asset_id}"
            )),
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
        match self
            .sender
            .send_message(&self.config.plugin_id, &self.config.chat_id, message)
            .await
        {
            Ok(_) => true,
            Err(error) => {
                warn!(
                    %asset_id,
                    %error,
                    "failed to send channel Workshop asset locator fallback"
                );
                false
            }
        }
    }

    /// Resolve each not-yet-sent asset id to bytes and send it via the plugin's
    /// media path. `seen` dedupes across a turn. When this relay has no resolver
    /// the durable Workshop id is surfaced as a locator. A wired resolver that
    /// cannot find the id is an integrity failure and prevents a green final.
    async fn flush_media(
        &self,
        ids: &[String],
        seen: &mut std::collections::HashSet<String>,
    ) -> bool {
        let Some(resolver) = self.asset_resolver.as_ref() else {
            let mut all_delivered = true;
            for id in ids {
                if seen.insert(id.clone())
                    && !self
                        .send_workshop_asset_fallback(
                            id,
                            "channel media resolver is unavailable",
                        )
                        .await
                {
                    all_delivered = false;
                }
            }
            return all_delivered;
        };
        let mut all_delivered = true;
        for id in ids {
            if !seen.insert(id.clone()) {
                continue;
            }
            match resolver.resolve(id).await {
                Some(media) => {
                    if let Err(e) = self
                        .sender
                        .send_media(&self.config.plugin_id, &self.config.chat_id, media, None)
                        .await
                    {
                        warn!(asset_id = %id, error = %e, "failed to send channel media");
                        if !self
                            .send_workshop_asset_fallback(id, "channel upload failed")
                            .await
                        {
                            all_delivered = false;
                        }
                    }
                }
                None => {
                    warn!(asset_id = %id, "asset could not be resolved for channel media");
                    self.send_workshop_asset_fallback(id, "asset is no longer resolvable")
                        .await;
                    // A resolver returning `None` proves that this id is not a
                    // usable locator at the delivery boundary. The diagnostic
                    // message may be acknowledged, but it cannot substitute
                    // for the missing output.
                    all_delivered = false;
                }
            }
        }
        all_delivered
    }

    async fn send_artifact_path_fallback(
        &self,
        artifact: &nomifun_ai_agent::artifact_store::PersistedArtifact,
        reason: &str,
    ) -> bool {
        let message = UnifiedOutgoingMessage {
            message_type: OutgoingMessageType::Text,
            text: Some(format!(
                "Artifact could not be uploaded ({reason}). Recorded artifact path: {}",
                artifact.path
            )),
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
        match self
            .sender
            .send_message(&self.config.plugin_id, &self.config.chat_id, message)
            .await
        {
            Ok(_) => true,
            Err(error) => {
                warn!(
                    artifact_id = %artifact.id,
                    path = %artifact.path,
                    %error,
                    "failed to send channel artifact path fallback"
                );
                false
            }
        }
    }

    async fn send_artifact_integrity_failure(
        &self,
        artifact: &nomifun_ai_agent::artifact_store::PersistedArtifact,
        reason: &str,
    ) {
        let message = UnifiedOutgoingMessage {
            message_type: OutgoingMessageType::Text,
            text: Some(format!(
                "❌ Generated artifact delivery failed ({reason}). The recorded locator is not a usable verified output: {}",
                artifact.path
            )),
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
            .send_message(&self.config.plugin_id, &self.config.chat_id, message)
            .await;
    }

    /// Upload verified tool artifacts directly from their durable receipts.
    /// Integrity is rechecked at the consumption boundary to catch deletion or
    /// mutation between persistence and channel delivery. Any read, integrity
    /// or upload failure is surfaced as an explicit path message.
    async fn flush_artifacts(
        &self,
        artifacts: &[nomifun_ai_agent::artifact_store::PersistedArtifact],
        seen: &mut std::collections::HashSet<String>,
    ) -> bool {
        let mut all_verified = true;
        for artifact in artifacts {
            if !seen.insert(artifact.id.clone()) {
                continue;
            }
            let bytes = match tokio::fs::read(&artifact.path).await {
                Ok(bytes) => bytes,
                Err(error) => {
                    warn!(artifact_id = %artifact.id, path = %artifact.path, %error, "failed to read channel artifact");
                    self.send_artifact_integrity_failure(artifact, "file is no longer readable")
                        .await;
                    all_verified = false;
                    continue;
                }
            };
            let sha256 = format!("{:x}", Sha256::digest(&bytes));
            if bytes.len() as u64 != artifact.size_bytes || sha256 != artifact.sha256 {
                warn!(
                    artifact_id = %artifact.id,
                    path = %artifact.path,
                    expected_size = artifact.size_bytes,
                    actual_size = bytes.len(),
                    "channel artifact failed receipt integrity verification"
                );
                self.send_artifact_integrity_failure(artifact, "file changed after generation")
                    .await;
                all_verified = false;
                continue;
            }
            let filename = std::path::Path::new(&artifact.path)
                .file_name()
                .and_then(|name| name.to_str())
                .filter(|name| !name.is_empty())
                .map(ToOwned::to_owned)
                .unwrap_or_else(|| format!("{}.bin", artifact.id));
            let kind = if artifact.kind
                == nomifun_ai_agent::artifact_store::ArtifactKind::Image
            {
                crate::types::MediaKind::Image
            } else {
                crate::types::MediaKind::File
            };
            let media = crate::types::OutgoingMedia {
                bytes,
                mime: artifact.mime_type.clone(),
                filename,
                kind,
            };
            if let Err(error) = self
                .sender
                .send_media(&self.config.plugin_id, &self.config.chat_id, media, None)
                .await
            {
                warn!(artifact_id = %artifact.id, path = %artifact.path, %error, "failed to upload channel artifact");
                if !self
                    .send_artifact_path_fallback(artifact, "channel upload failed")
                    .await
                {
                    all_verified = false;
                }
            }
        }
        all_verified
    }

    /// Build an editable terminal failure without erasing assistant text that
    /// was already streamed into the channel card. Reasoning is stripped at
    /// the final boundary and the combined body is formatted as one unit so
    /// Telegram HTML remains escaped and well-formed.
    fn terminal_failure_message(
        &self,
        text_buffer: &str,
        reason: &str,
    ) -> UnifiedOutgoingMessage {
        let visible = strip_reasoning(text_buffer, Stage::Final);
        let (text, parse_mode) = if visible.trim().is_empty() {
            (format!("❌ {reason}"), None)
        } else {
            let combined = format!("{}\n\n❌ {reason}", visible.trim_end());
            (
                format_text_for_platform(&combined, self.config.platform),
                formatted_parse_mode(self.config.platform),
            )
        };
        UnifiedOutgoingMessage {
            message_type: OutgoingMessageType::Text,
            text: Some(text),
            parse_mode,
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
        let mut media_ids: Vec<String> = Vec::new();
        let mut media_seen: std::collections::HashSet<String> = std::collections::HashSet::new();
        let mut artifacts = Vec::new();
        let mut artifact_seen: std::collections::HashSet<String> = std::collections::HashSet::new();
        let mut terminal_tool_calls = TerminalToolCallGate::default();

        loop {
            match rx.recv().await {
                Ok(event) => {
                    if !terminal_tool_calls.accepts(&event) {
                        warn!("ignoring late tool event after its terminal channel projection");
                        continue;
                    }
                    match ChannelMessageService::process_stream_event(&event) {
                    Some(StreamAction::AppendText(chunk)) => {
                        text_buffer.push_str(&chunk);
                        has_content = true;
                    }
                    Some(StreamAction::Thinking(_)) => {}
                    // Produced Workshop asset ids: accumulate and verify at
                    // the authoritative Finish boundary (deduped per turn).
                    Some(StreamAction::MediaProduced(ids)) => {
                        media_ids.extend(ids);
                    }
                    Some(StreamAction::ArtifactsProduced(produced)) => {
                        artifacts.extend(produced);
                    }
                    // A send-once platform cannot retract text if a later
                    // required tool/artifact fails. Keep every assistant chunk
                    // buffered until the authoritative successful Finish.
                    Some(StreamAction::ToolCall { .. }) => {}
                    // A blocking decision: record it and forward a numbered
                    // list as a new message. WeChat cannot edit, so this is a
                    // fresh send_message either way.
                    Some(StreamAction::Decision { call_id, prompt, options }) => {
                        self.record_and_send_decision(call_id, prompt, options).await;
                    }
                    Some(StreamAction::Finish) => {
                        let visible = strip_reasoning(&text_buffer, Stage::Final);
                        if !self.flush_artifacts(&artifacts, &mut artifact_seen).await {
                            let error_msg = self.terminal_failure_message(
                                &text_buffer,
                                "One or more generated artifacts failed final channel verification; task output was not delivered.",
                            );
                            let _ = self
                                .sender
                                .send_message(
                                    &self.config.plugin_id,
                                    &self.config.chat_id,
                                    error_msg,
                                )
                                .await;
                            break;
                        }
                        // Also catch asset URLs written into the visible text
                        // (the same link the desktop renders). Resolve every
                        // claimed id before publishing a green final.
                        media_ids.extend(crate::media_refs::asset_ids_from_text(&visible));
                        if !self.flush_media(&media_ids, &mut media_seen).await {
                            let error_msg = self.terminal_failure_message(
                                &text_buffer,
                                "One or more generated Workshop assets could not be resolved; task output was not delivered.",
                            );
                            let _ = self
                                .sender
                                .send_message(
                                    &self.config.plugin_id,
                                    &self.config.chat_id,
                                    error_msg,
                                )
                                .await;
                            break;
                        }
                        if has_content && !visible.trim().is_empty() {
                            let formatted = format_text_for_platform(&visible, self.config.platform);
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
                    }
                }
                Err(broadcast::error::RecvError::Closed) => {
                    // A closed sender is not a successful turn boundary. A
                    // send-once channel cannot retract partial model text, so
                    // surface only an explicit terminal failure.
                    let error_msg = UnifiedOutgoingMessage {
                        message_type: OutgoingMessageType::Text,
                        text: Some(
                            "❌ Response stream ended before completion; generated output was not released."
                                .into(),
                        ),
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
                    warn!(
                        workshop_assets = media_ids.len(),
                        tool_artifacts = artifacts.len(),
                        "discarding queued channel artifacts after stream closed without terminal event"
                    );
                    break;
                }
                Err(broadcast::error::RecvError::Lagged(n)) => {
                    let reason = format!(
                        "Response stream skipped {n} event(s); completion and generated output cannot be verified."
                    );
                    let error_msg = self.terminal_failure_message(&text_buffer, &reason);
                    let _ = self
                        .sender
                        .send_message(&self.config.plugin_id, &self.config.chat_id, error_msg)
                        .await;
                    warn!(
                        lagged = n,
                        workshop_assets = media_ids.len(),
                        tool_artifacts = artifacts.len(),
                        "channel stream relay lagged; failing closed and discarding queued artifacts (weixin)"
                    );
                    break;
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
        // Whether a blocking decision was forwarded during this turn. When a
        // decision is pending, the thinking/streaming card is deliberately left
        // intact so the turn stays live (see `record_and_send_decision`); we
        // must not replace it with a terminal "(no text output)" card on Finish.
        let mut decision_forwarded = false;
        let mut media_ids: Vec<String> = Vec::new();
        let mut media_seen: std::collections::HashSet<String> = std::collections::HashSet::new();
        let mut artifacts = Vec::new();
        let mut artifact_seen: std::collections::HashSet<String> = std::collections::HashSet::new();
        let mut terminal_tool_calls = TerminalToolCallGate::default();

        loop {
            match rx.recv().await {
                Ok(event) => {
                    if !terminal_tool_calls.accepts(&event) {
                        warn!("ignoring late tool event after its terminal channel projection");
                        continue;
                    }
                    match ChannelMessageService::process_stream_event(&event) {
                    Some(StreamAction::AppendText(chunk)) => {
                        text_buffer.push_str(&chunk);
                        if last_edit.elapsed() >= throttle {
                            let visible = strip_reasoning(&text_buffer, Stage::Streaming);
                            if !visible.trim().is_empty() {
                                let formatted = format_text_for_platform(&visible, self.config.platform);
                                let mut msg = ChannelMessageService::build_streaming_message(&formatted);
                                msg.parse_mode = formatted_parse_mode(self.config.platform);
                                let _ = self
                                    .sender
                                    .edit_message(&self.config.plugin_id, &self.config.chat_id, &thinking_msg_id, msg)
                                    .await;
                            }
                            // Advance the throttle clock whether or not we edited:
                            // while only reasoning has streamed in, the "⏳
                            // Thinking..." placeholder stays put, and re-scanning a
                            // long buffer on every delta would waste CPU. The first
                            // visible text appears within one throttle window, same
                            // as ordinary streaming.
                            last_edit = Instant::now();
                        }
                    }
                    Some(StreamAction::Thinking(_)) => {}
                    // Produced Workshop asset ids: accumulate and verify at
                    // the authoritative Finish boundary (deduped per turn).
                    Some(StreamAction::MediaProduced(ids)) => {
                        media_ids.extend(ids);
                    }
                    Some(StreamAction::ArtifactsProduced(produced)) => {
                        artifacts.extend(produced);
                    }
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
                        if !self.flush_artifacts(&artifacts, &mut artifact_seen).await {
                            let error_msg = self.terminal_failure_message(
                                &text_buffer,
                                "One or more generated artifacts failed final channel verification; task output was not delivered.",
                            );
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
                        let visible = strip_reasoning(&text_buffer, Stage::Final);
                        media_ids.extend(crate::media_refs::asset_ids_from_text(&visible));
                        if !self.flush_media(&media_ids, &mut media_seen).await {
                            let error_msg = self.terminal_failure_message(
                                &text_buffer,
                                "One or more generated Workshop assets could not be resolved; task output was not delivered.",
                            );
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
                        self.send_final_edit(&text_buffer, decision_forwarded, &thinking_msg_id)
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
                        let error_msg = self.terminal_failure_message(&text_buffer, &msg);
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
                    }
                }
                Err(broadcast::error::RecvError::Closed) => {
                    warn!("channel stream relay: broadcast closed without terminal event");
                    let error_msg = self.terminal_failure_message(
                        &text_buffer,
                        "Response stream ended before completion; queued artifacts were not published.",
                    );
                    let _ = self
                        .sender
                        .edit_message(
                            &self.config.plugin_id,
                            &self.config.chat_id,
                            &thinking_msg_id,
                            error_msg,
                        )
                        .await;
                    warn!(
                        workshop_assets = media_ids.len(),
                        tool_artifacts = artifacts.len(),
                        "discarding queued channel artifacts after stream closed without terminal event"
                    );
                    break;
                }
                Err(broadcast::error::RecvError::Lagged(n)) => {
                    let reason = format!(
                        "Response stream skipped {n} event(s); completion and generated output cannot be verified."
                    );
                    let error_msg = self.terminal_failure_message(&text_buffer, &reason);
                    let _ = self
                        .sender
                        .edit_message(
                            &self.config.plugin_id,
                            &self.config.chat_id,
                            &thinking_msg_id,
                            error_msg,
                        )
                        .await;
                    warn!(
                        lagged = n,
                        workshop_assets = media_ids.len(),
                        tool_artifacts = artifacts.len(),
                        "channel stream relay lagged; failing closed and discarding queued artifacts"
                    );
                    break;
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
    /// "Has assistant text" is decided on the reasoning-stripped buffer, so a
    /// turn that produced only inline `<think>` reasoning counts as no-text.
    ///
    /// - With visible text: render the formatted final card.
    /// - Without visible text but a decision was forwarded: leave the card intact
    ///   — the decision flow owns the live UX and the turn stays interactive.
    /// - Without visible text and no decision (tool-only / pure-thinking / empty
    ///   completion): the agent reported a finished turn that produced no visible
    ///   `Text`. The placeholder must still be replaced with a terminal card,
    ///   otherwise the user is left staring at "Thinking..." forever on an
    ///   already-completed turn (the silent-empty-reply failure class). Emit a
    ///   neutral "(no text output)" final card so the action buttons are
    ///   delivered and the card is终态.
    async fn send_final_edit(&self, text_buffer: &str, decision_forwarded: bool, msg_id: &str) {
        let visible = strip_reasoning(text_buffer, Stage::Final);
        if !visible.trim().is_empty() {
            let formatted = format_text_for_platform(&visible, self.config.platform);
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
    media: std::sync::Mutex<Vec<crate::types::OutgoingMedia>>,
}

impl MessageRecorder {
    pub fn new() -> Self {
        Self {
            sends: std::sync::Mutex::new(Vec::new()),
            edits: std::sync::Mutex::new(Vec::new()),
            media: std::sync::Mutex::new(Vec::new()),
        }
    }

    pub fn take_sends(&self) -> Vec<UnifiedOutgoingMessage> {
        std::mem::take(&mut self.sends.lock().unwrap())
    }

    pub fn take_edits(&self) -> Vec<UnifiedOutgoingMessage> {
        std::mem::take(&mut self.edits.lock().unwrap())
    }

    pub fn take_media(&self) -> Vec<crate::types::OutgoingMedia> {
        std::mem::take(&mut self.media.lock().unwrap())
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

    async fn send_media(
        &self,
        _plugin_id: &str,
        _chat_id: &str,
        media: crate::types::OutgoingMedia,
        _caption: Option<&str>,
    ) -> Result<String, ChannelError> {
        self.media.lock().unwrap().push(media);
        Ok("media-1".into())
    }
}

#[cfg(test)]
mod media_tests {
    use super::*;
    use crate::types::{MediaKind, OutgoingMedia};
    use async_trait::async_trait;
    use nomifun_common::PersistedArtifactId;
    use std::sync::Arc;

    struct StubResolver;
    #[async_trait]
    impl crate::message_service::AssetResolver for StubResolver {
        async fn resolve(&self, asset_id: &str) -> Option<OutgoingMedia> {
            Some(OutgoingMedia {
                bytes: vec![1, 2, 3],
                mime: "image/png".into(),
                filename: format!("{asset_id}.png"),
                kind: MediaKind::Image,
            })
        }
    }

    struct MissingResolver;
    #[async_trait]
    impl crate::message_service::AssetResolver for MissingResolver {
        async fn resolve(&self, _asset_id: &str) -> Option<OutgoingMedia> {
            None
        }
    }

    /// A channel that can acknowledge ordinary messages/edits while allowing
    /// media upload and locator fallback failures to be controlled separately.
    /// Failed locator messages are retained as attempts for assertions, but
    /// return `Err` and therefore are not delivery acknowledgements.
    struct DeliverySender {
        fail_media: bool,
        fail_locator: bool,
        sends: std::sync::Mutex<Vec<UnifiedOutgoingMessage>>,
        edits: std::sync::Mutex<Vec<UnifiedOutgoingMessage>>,
        media_attempts: std::sync::atomic::AtomicUsize,
    }

    impl DeliverySender {
        fn new(fail_media: bool, fail_locator: bool) -> Self {
            Self {
                fail_media,
                fail_locator,
                sends: std::sync::Mutex::new(Vec::new()),
                edits: std::sync::Mutex::new(Vec::new()),
                media_attempts: std::sync::atomic::AtomicUsize::new(0),
            }
        }

        fn take_sends(&self) -> Vec<UnifiedOutgoingMessage> {
            std::mem::take(&mut self.sends.lock().unwrap())
        }

        fn take_edits(&self) -> Vec<UnifiedOutgoingMessage> {
            std::mem::take(&mut self.edits.lock().unwrap())
        }

        fn media_attempts(&self) -> usize {
            self.media_attempts
                .load(std::sync::atomic::Ordering::Relaxed)
        }
    }

    #[async_trait]
    impl ChannelSender for DeliverySender {
        async fn send_message(
            &self,
            _plugin_id: &str,
            _chat_id: &str,
            message: UnifiedOutgoingMessage,
        ) -> Result<String, ChannelError> {
            let is_locator = message.text.as_deref().is_some_and(|text| {
                text.contains("Asset ID:") || text.contains("Recorded artifact path:")
            });
            self.sends.lock().unwrap().push(message);
            if self.fail_locator && is_locator {
                Err(ChannelError::MessageSendFailed(
                    "locator fallback rejected".into(),
                ))
            } else {
                Ok("msg-1".into())
            }
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

        async fn send_media(
            &self,
            _plugin_id: &str,
            _chat_id: &str,
            _media: OutgoingMedia,
            _caption: Option<&str>,
        ) -> Result<String, ChannelError> {
            self.media_attempts
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            if self.fail_media {
                Err(ChannelError::MessageSendFailed(
                    "media upload rejected".into(),
                ))
            } else {
                Ok("media-1".into())
            }
        }
    }

    fn cfg(platform: PluginType) -> RelayConfig {
        RelayConfig {
            platform,
            plugin_id: "018f1234-5678-7abc-8def-012345678901".into(),
            chat_id: "c1".into(),
            throttle_ms: 0,
            conversation_id: "0190f5fe-7c00-7a00-8000-000000000080".into(),
        }
    }

    // Drives the relay with two identical MediaProduced events then Finish, and
    // asserts the resolved image is sent exactly once via send_media (deduped),
    // and the final text is still delivered.
    #[tokio::test]
    async fn relay_flushes_media_on_finish() {
        use nomifun_ai_agent::protocol::events::{FinishEventData, TextEventData, ToolCallEventData, ToolCallStatus};

        let recorder = Arc::new(MessageRecorder::new());
        let pending = crate::pending_decision::PendingDecisionStore::new();
        let relay = ChannelStreamRelay::new(
            cfg(PluginType::Telegram),
            recorder.clone(),
            pending,
            Some(Arc::new(StubResolver)),
        );

        let (tx, rx) = tokio::sync::broadcast::channel(16);
        tx.send(AgentStreamEvent::Text(TextEventData { content: "图来咯～".into() })).unwrap();
        // Two completed tool calls returning the SAME asset id → must dedupe to one send.
        for _ in 0..2 {
            tx.send(AgentStreamEvent::ToolCall(ToolCallEventData {
                call_id: "t".into(),
                name: "nomi_workshop_get_task".into(),
                args: serde_json::Value::Null,
                status: ToolCallStatus::Completed,
                description: None,
                input: None,
                output: Some(
                    r#"{"result_asset_ids":["0190f5fe-7c00-7a00-8000-000000000081"]}"#
                        .into(),
                ),
                artifacts: Vec::new(),
            })).unwrap();
        }
        tx.send(AgentStreamEvent::Finish(FinishEventData { session_id: None, stop_reason: None })).unwrap();
        drop(tx);

        relay.run(rx).await;

        let media = recorder.take_media();
        assert_eq!(media.len(), 1, "one deduped image sent");
        assert_eq!(
            media[0].filename,
            "0190f5fe-7c00-7a00-8000-000000000081.png"
        );
        assert!(!recorder.take_edits().is_empty(), "final text edit delivered too");
    }

    #[tokio::test]
    async fn unresolved_workshop_asset_prevents_green_channel_finish() {
        use nomifun_ai_agent::protocol::events::{FinishEventData, ToolCallEventData, ToolCallStatus};

        let recorder = Arc::new(MessageRecorder::new());
        let relay = ChannelStreamRelay::new(
            cfg(PluginType::Telegram),
            recorder.clone(),
            crate::pending_decision::PendingDecisionStore::new(),
            Some(Arc::new(MissingResolver)),
        );
        let (tx, rx) = tokio::sync::broadcast::channel(8);
        tx.send(AgentStreamEvent::ToolCall(ToolCallEventData {
            call_id: "missing-workshop-asset".into(),
            name: "nomi_workshop_get_task".into(),
            args: serde_json::Value::Null,
            status: ToolCallStatus::Completed,
            description: None,
            input: None,
            output: Some(
                r#"{"result_asset_ids":["0190f5fe-7c00-7a00-8000-000000000082"]}"#
                    .into(),
            ),
            artifacts: Vec::new(),
        }))
        .unwrap();
        tx.send(AgentStreamEvent::Finish(FinishEventData {
            session_id: None,
            stop_reason: None,
        }))
        .unwrap();
        drop(tx);

        relay.run(rx).await;

        assert!(recorder.take_media().is_empty());
        assert!(recorder.take_sends().iter().any(|message| {
            message
                .text
                .as_deref()
                .is_some_and(|text| {
                    text.contains("0190f5fe-7c00-7a00-8000-000000000082")
                        && text.contains("no longer resolvable")
                })
        }));
        assert!(recorder.take_edits().iter().any(|message| {
            message
                .text
                .as_deref()
                .is_some_and(|text| text.contains("Workshop assets could not be resolved"))
        }));
    }

    #[tokio::test]
    async fn workshop_upload_failure_with_acknowledged_locator_can_finish() {
        use nomifun_ai_agent::protocol::events::{
            FinishEventData, ToolCallEventData, ToolCallStatus,
        };

        let sender = Arc::new(DeliverySender::new(true, false));
        let relay = ChannelStreamRelay::new(
            cfg(PluginType::Telegram),
            sender.clone(),
            crate::pending_decision::PendingDecisionStore::new(),
            Some(Arc::new(StubResolver)),
        );
        let (tx, rx) = tokio::sync::broadcast::channel(8);
        tx.send(AgentStreamEvent::ToolCall(ToolCallEventData {
            call_id: "workshop-upload-fallback-ok".into(),
            name: "nomi_workshop_get_task".into(),
            args: serde_json::Value::Null,
            status: ToolCallStatus::Completed,
            description: None,
            input: None,
            output: Some(
                r#"{"result_asset_ids":["0190f5fe-7c00-7a00-8000-000000000083"]}"#
                    .into(),
            ),
            artifacts: Vec::new(),
        }))
        .unwrap();
        tx.send(AgentStreamEvent::Finish(FinishEventData::default()))
            .unwrap();
        drop(tx);

        relay.run(rx).await;

        assert_eq!(sender.media_attempts(), 1);
        assert!(sender.take_sends().iter().any(|message| {
            message
                .text
                .as_deref()
                .is_some_and(|text| {
                    text.contains("Asset ID: 0190f5fe-7c00-7a00-8000-000000000083")
                })
        }));
        let edits = sender.take_edits();
        assert!(edits.last().is_some_and(|message| {
            message
                .text
                .as_deref()
                .is_some_and(|text| text.contains("无文本输出"))
        }));
        assert!(edits.iter().all(|message| {
            !message
                .text
                .as_deref()
                .is_some_and(|text| text.contains("task output was not delivered"))
        }));
    }

    #[tokio::test]
    async fn workshop_upload_and_locator_failure_prevents_green_finish() {
        use nomifun_ai_agent::protocol::events::{
            FinishEventData, ToolCallEventData, ToolCallStatus,
        };

        let sender = Arc::new(DeliverySender::new(true, true));
        let relay = ChannelStreamRelay::new(
            cfg(PluginType::Telegram),
            sender.clone(),
            crate::pending_decision::PendingDecisionStore::new(),
            Some(Arc::new(StubResolver)),
        );
        let (tx, rx) = tokio::sync::broadcast::channel(8);
        tx.send(AgentStreamEvent::ToolCall(ToolCallEventData {
            call_id: "workshop-delivery-failed".into(),
            name: "nomi_workshop_get_task".into(),
            args: serde_json::Value::Null,
            status: ToolCallStatus::Completed,
            description: None,
            input: None,
            output: Some(
                r#"{"result_asset_ids":["0190f5fe-7c00-7a00-8000-000000000084"]}"#
                    .into(),
            ),
            artifacts: Vec::new(),
        }))
        .unwrap();
        tx.send(AgentStreamEvent::Finish(FinishEventData::default()))
            .unwrap();
        drop(tx);

        relay.run(rx).await;

        assert_eq!(sender.media_attempts(), 1);
        let edits = sender.take_edits();
        assert!(edits.last().is_some_and(|message| {
            message.text.as_deref().is_some_and(|text| {
                text.contains("Workshop assets could not be resolved")
                    && text.contains("task output was not delivered")
            })
        }));
        assert!(edits.iter().all(|message| {
            !message
                .text
                .as_deref()
                .is_some_and(|text| text.contains("无文本输出"))
        }));
    }

    #[tokio::test]
    async fn missing_resolver_and_locator_failure_prevents_green_finish() {
        use nomifun_ai_agent::protocol::events::{
            FinishEventData, ToolCallEventData, ToolCallStatus,
        };

        let sender = Arc::new(DeliverySender::new(false, true));
        let relay = ChannelStreamRelay::new(
            cfg(PluginType::Telegram),
            sender.clone(),
            crate::pending_decision::PendingDecisionStore::new(),
            None,
        );
        let (tx, rx) = tokio::sync::broadcast::channel(8);
        tx.send(AgentStreamEvent::ToolCall(ToolCallEventData {
            call_id: "workshop-no-resolver".into(),
            name: "nomi_workshop_get_task".into(),
            args: serde_json::Value::Null,
            status: ToolCallStatus::Completed,
            description: None,
            input: None,
            output: Some(
                r#"{"result_asset_ids":["0190f5fe-7c00-7a00-8000-000000000085"]}"#
                    .into(),
            ),
            artifacts: Vec::new(),
        }))
        .unwrap();
        tx.send(AgentStreamEvent::Finish(FinishEventData::default()))
            .unwrap();
        drop(tx);

        relay.run(rx).await;

        assert_eq!(sender.media_attempts(), 0);
        let edits = sender.take_edits();
        assert!(edits.last().is_some_and(|message| {
            message
                .text
                .as_deref()
                .is_some_and(|text| text.contains("Workshop assets could not be resolved"))
        }));
        assert!(edits.iter().all(|message| {
            !message
                .text
                .as_deref()
                .is_some_and(|text| text.contains("无文本输出"))
        }));
    }

    #[tokio::test]
    async fn relay_uploads_verified_image_and_file_artifacts_without_asset_resolver() {
        use nomifun_ai_agent::artifact_store::{ArtifactKind, PersistedArtifact};
        use nomifun_ai_agent::protocol::events::{FinishEventData, ToolCallEventData, ToolCallStatus};

        let temp = tempfile::tempdir().unwrap();
        let image_path = temp.path().join("image.png");
        let file_path = temp.path().join("report.pdf");
        let image_bytes = b"image-bytes".to_vec();
        let file_bytes = b"%PDF-report".to_vec();
        std::fs::write(&image_path, &image_bytes).unwrap();
        std::fs::write(&file_path, &file_bytes).unwrap();
        let receipt = |id: &str, kind, mime: &str, path: &std::path::Path, bytes: &[u8]| {
            PersistedArtifact {
                id: PersistedArtifactId::new().into_string(),
                kind,
                mime_type: mime.into(),
                path: path.to_string_lossy().into_owned(),
                relative_path: format!("nomifun-artifacts/{id}"),
                size_bytes: bytes.len() as u64,
                sha256: format!("{:x}", Sha256::digest(bytes)),
            }
        };
        let artifacts = vec![
            receipt("image", ArtifactKind::Image, "image/png", &image_path, &image_bytes),
            receipt("file", ArtifactKind::File, "application/pdf", &file_path, &file_bytes),
        ];
        let recorder = Arc::new(MessageRecorder::new());
        let relay = ChannelStreamRelay::new(
            cfg(PluginType::Telegram),
            recorder.clone(),
            crate::pending_decision::PendingDecisionStore::new(),
            None,
        );
        let (tx, rx) = tokio::sync::broadcast::channel(8);
        tx.send(AgentStreamEvent::ToolCall(ToolCallEventData {
            call_id: "tool-1".into(),
            name: "mcp__reports__export".into(),
            args: serde_json::Value::Null,
            status: ToolCallStatus::Completed,
            input: None,
            output: Some("done".into()),
            description: None,
            artifacts,
        }))
        .unwrap();
        tx.send(AgentStreamEvent::Finish(FinishEventData {
            session_id: None,
            stop_reason: None,
        }))
        .unwrap();
        drop(tx);

        relay.run(rx).await;

        let media = recorder.take_media();
        assert_eq!(media.len(), 2);
        assert_eq!(media[0].kind, MediaKind::Image);
        assert_eq!(media[0].bytes, image_bytes);
        assert_eq!(media[1].kind, MediaKind::File);
        assert_eq!(media[1].bytes, file_bytes);
    }

    #[tokio::test]
    async fn artifact_upload_failure_with_acknowledged_path_can_finish() {
        use nomifun_ai_agent::artifact_store::{ArtifactKind, PersistedArtifact};
        use nomifun_ai_agent::protocol::events::{
            FinishEventData, ToolCallEventData, ToolCallStatus,
        };

        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("report.pdf");
        let bytes = b"%PDF-deliverable";
        std::fs::write(&path, bytes).unwrap();
        let artifact = PersistedArtifact {
            id: PersistedArtifactId::new().into_string(),
            kind: ArtifactKind::File,
            mime_type: "application/pdf".into(),
            path: path.to_string_lossy().into_owned(),
            relative_path: "nomifun-artifacts/report.pdf".into(),
            size_bytes: bytes.len() as u64,
            sha256: format!("{:x}", Sha256::digest(bytes)),
        };
        let sender = Arc::new(DeliverySender::new(true, false));
        let relay = ChannelStreamRelay::new(
            cfg(PluginType::Telegram),
            sender.clone(),
            crate::pending_decision::PendingDecisionStore::new(),
            None,
        );
        let (tx, rx) = tokio::sync::broadcast::channel(8);
        tx.send(AgentStreamEvent::ToolCall(ToolCallEventData {
            call_id: "artifact-fallback-ok".into(),
            name: "mcp__reports__export".into(),
            args: serde_json::Value::Null,
            status: ToolCallStatus::Completed,
            input: None,
            output: Some("done".into()),
            description: None,
            artifacts: vec![artifact.clone()],
        }))
        .unwrap();
        tx.send(AgentStreamEvent::Finish(FinishEventData::default()))
            .unwrap();
        drop(tx);

        relay.run(rx).await;

        assert_eq!(sender.media_attempts(), 1);
        assert!(sender.take_sends().iter().any(|message| {
            message.text.as_deref().is_some_and(|text| {
                text.contains("Recorded artifact path:") && text.contains(&artifact.path)
            })
        }));
        let edits = sender.take_edits();
        assert!(edits.last().is_some_and(|message| {
            message
                .text
                .as_deref()
                .is_some_and(|text| text.contains("无文本输出"))
        }));
    }

    #[tokio::test]
    async fn artifact_upload_and_path_fallback_failure_prevents_green_finish() {
        use nomifun_ai_agent::artifact_store::{ArtifactKind, PersistedArtifact};
        use nomifun_ai_agent::protocol::events::{
            FinishEventData, ToolCallEventData, ToolCallStatus,
        };

        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("report.pdf");
        let bytes = b"%PDF-undelivered";
        std::fs::write(&path, bytes).unwrap();
        let artifact = PersistedArtifact {
            id: PersistedArtifactId::new().into_string(),
            kind: ArtifactKind::File,
            mime_type: "application/pdf".into(),
            path: path.to_string_lossy().into_owned(),
            relative_path: "nomifun-artifacts/report.pdf".into(),
            size_bytes: bytes.len() as u64,
            sha256: format!("{:x}", Sha256::digest(bytes)),
        };
        let sender = Arc::new(DeliverySender::new(true, true));
        let relay = ChannelStreamRelay::new(
            cfg(PluginType::Telegram),
            sender.clone(),
            crate::pending_decision::PendingDecisionStore::new(),
            None,
        );
        let (tx, rx) = tokio::sync::broadcast::channel(8);
        tx.send(AgentStreamEvent::ToolCall(ToolCallEventData {
            call_id: "artifact-delivery-failed".into(),
            name: "mcp__reports__export".into(),
            args: serde_json::Value::Null,
            status: ToolCallStatus::Completed,
            input: None,
            output: Some("done".into()),
            description: None,
            artifacts: vec![artifact],
        }))
        .unwrap();
        tx.send(AgentStreamEvent::Finish(FinishEventData::default()))
            .unwrap();
        drop(tx);

        relay.run(rx).await;

        assert_eq!(sender.media_attempts(), 1);
        let edits = sender.take_edits();
        assert!(edits.last().is_some_and(|message| {
            message.text.as_deref().is_some_and(|text| {
                text.contains("failed final channel verification")
                    && text.contains("task output was not delivered")
            })
        }));
        assert!(edits.iter().all(|message| {
            !message
                .text
                .as_deref()
                .is_some_and(|text| text.contains("无文本输出"))
        }));
    }

    #[tokio::test]
    async fn relay_uploads_acp_artifact_from_terminal_completed_frame() {
        use nomifun_ai_agent::artifact_store::{ArtifactKind, PersistedArtifact};
        use nomifun_ai_agent::protocol::events::{
            AcpToolCallContentItem, AcpToolCallEventData, AcpToolCallSessionUpdateKind,
            AcpToolCallStatus, AcpToolCallUpdateData, FinishEventData,
        };

        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("acp-image.png");
        let bytes = b"verified-acp-image".to_vec();
        std::fs::write(&path, &bytes).unwrap();
        let artifact = PersistedArtifact {
            id: PersistedArtifactId::new().into_string(),
            kind: ArtifactKind::Image,
            mime_type: "image/png".into(),
            path: path.to_string_lossy().into_owned(),
            relative_path: "nomifun-artifacts/acp-image.png".into(),
            size_bytes: bytes.len() as u64,
            sha256: format!("{:x}", Sha256::digest(&bytes)),
        };
        let event = |status, session_update| {
            AgentStreamEvent::AcpToolCall(AcpToolCallEventData {
                session_id: "sess-1".into(),
                update: AcpToolCallUpdateData {
                    session_update,
                    tool_call_id: "tool-image".into(),
                    status: Some(status),
                    title: None,
                    kind: None,
                    raw_input: None,
                    raw_output: None,
                    content: Some(vec![AcpToolCallContentItem::Artifact {
                        artifact: artifact.clone(),
                        source_uri: None,
                    }]),
                    locations: None,
                },
                meta: None,
            })
        };

        let recorder = Arc::new(MessageRecorder::new());
        let relay = ChannelStreamRelay::new(
            cfg(PluginType::Telegram),
            recorder.clone(),
            crate::pending_decision::PendingDecisionStore::new(),
            None,
        );
        let (tx, rx) = tokio::sync::broadcast::channel(8);
        tx.send(event(
            AcpToolCallStatus::InProgress,
            AcpToolCallSessionUpdateKind::ToolCall,
        ))
        .unwrap();
        tx.send(event(
            AcpToolCallStatus::Completed,
            AcpToolCallSessionUpdateKind::ToolCallUpdate,
        ))
        .unwrap();
        tx.send(AgentStreamEvent::Finish(FinishEventData {
            session_id: None,
            stop_reason: None,
        }))
        .unwrap();
        drop(tx);

        relay.run(rx).await;

        let media = recorder.take_media();
        assert_eq!(media.len(), 1, "only the terminal ACP receipt is uploadable");
        assert_eq!(media[0].bytes, bytes);
        assert_eq!(media[0].kind, MediaKind::Image);
    }

    #[tokio::test]
    async fn relay_does_not_upload_failed_acp_artifact_receipt() {
        use nomifun_ai_agent::artifact_store::{ArtifactKind, PersistedArtifact};
        use nomifun_ai_agent::protocol::events::{
            AcpToolCallContentItem, AcpToolCallEventData, AcpToolCallSessionUpdateKind,
            AcpToolCallStatus, AcpToolCallUpdateData, ErrorEventData,
        };

        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("failed-acp-image.png");
        let bytes = b"failed-acp-image";
        std::fs::write(&path, bytes).unwrap();
        let artifact = PersistedArtifact {
            id: PersistedArtifactId::new().into_string(),
            kind: ArtifactKind::Image,
            mime_type: "image/png".into(),
            path: path.to_string_lossy().into_owned(),
            relative_path: "nomifun-artifacts/failed-acp-image.png".into(),
            size_bytes: bytes.len() as u64,
            sha256: format!("{:x}", Sha256::digest(bytes)),
        };
        let recorder = Arc::new(MessageRecorder::new());
        let relay = ChannelStreamRelay::new(
            cfg(PluginType::Telegram),
            recorder.clone(),
            crate::pending_decision::PendingDecisionStore::new(),
            None,
        );
        let (tx, rx) = tokio::sync::broadcast::channel(8);
        tx.send(AgentStreamEvent::AcpToolCall(AcpToolCallEventData {
            session_id: "sess-1".into(),
            update: AcpToolCallUpdateData {
                session_update: AcpToolCallSessionUpdateKind::ToolCallUpdate,
                tool_call_id: "tool-image".into(),
                status: Some(AcpToolCallStatus::Failed),
                title: None,
                kind: None,
                raw_input: None,
                raw_output: None,
                content: Some(vec![AcpToolCallContentItem::Artifact {
                    artifact,
                    source_uri: None,
                }]),
                locations: None,
            },
            meta: None,
        }))
        .unwrap();
        tx.send(AgentStreamEvent::Error(ErrorEventData::legacy(
            "artifact generation failed",
            None,
        )))
        .unwrap();
        drop(tx);

        relay.run(rx).await;

        assert!(
            recorder.take_media().is_empty(),
            "a Failed ACP frame must never enter the upload queue"
        );
    }

    #[tokio::test]
    async fn relay_marks_turn_failed_when_recorded_artifact_is_not_usable() {
        use nomifun_ai_agent::artifact_store::{ArtifactKind, PersistedArtifact};
        use nomifun_ai_agent::protocol::events::{FinishEventData, ToolCallEventData, ToolCallStatus};

        let missing = std::env::temp_dir().join("nomifun-missing-artifact-test.pdf");
        let artifact = PersistedArtifact {
            id: PersistedArtifactId::new().into_string(),
            kind: ArtifactKind::File,
            mime_type: "application/pdf".into(),
            path: missing.to_string_lossy().into_owned(),
            relative_path: "nomifun-artifacts/missing.pdf".into(),
            size_bytes: 1,
            sha256: "00".into(),
        };
        let recorder = Arc::new(MessageRecorder::new());
        let relay = ChannelStreamRelay::new(
            cfg(PluginType::Telegram),
            recorder.clone(),
            crate::pending_decision::PendingDecisionStore::new(),
            None,
        );
        let (tx, rx) = tokio::sync::broadcast::channel(8);
        tx.send(AgentStreamEvent::ToolCall(ToolCallEventData {
            call_id: "tool-1".into(),
            name: "mcp__reports__export".into(),
            args: serde_json::Value::Null,
            status: ToolCallStatus::Completed,
            input: None,
            output: Some("done".into()),
            description: None,
            artifacts: vec![artifact.clone()],
        }))
        .unwrap();
        tx.send(AgentStreamEvent::Finish(FinishEventData {
            session_id: None,
            stop_reason: None,
        }))
        .unwrap();
        drop(tx);

        relay.run(rx).await;

        assert!(recorder.take_media().is_empty());
        let sends = recorder.take_sends();
        assert!(sends.iter().any(|message| {
            message
                .text
                .as_deref()
                .is_some_and(|text| text.contains(&artifact.path) && text.contains("not a usable verified output"))
        }));
        assert!(sends.iter().all(|message| {
            !message
                .text
                .as_deref()
                .is_some_and(|text| text.contains("Recorded artifact path"))
        }));
        assert!(recorder.take_edits().iter().any(|message| {
            message
                .text
                .as_deref()
                .is_some_and(|text| text.contains("failed final channel verification"))
        }));
    }

    #[tokio::test]
    async fn failed_tool_artifact_is_never_uploaded_on_error_termination() {
        use nomifun_ai_agent::artifact_store::{ArtifactKind, PersistedArtifact};
        use nomifun_ai_agent::protocol::events::{ErrorEventData, ToolCallEventData, ToolCallStatus};

        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("partial.pdf");
        let bytes = b"%PDF-partial";
        std::fs::write(&path, bytes).unwrap();
        let artifact = PersistedArtifact {
            id: PersistedArtifactId::new().into_string(),
            kind: ArtifactKind::File,
            mime_type: "application/pdf".into(),
            path: path.to_string_lossy().into_owned(),
            relative_path: "nomifun-artifacts/partial.pdf".into(),
            size_bytes: bytes.len() as u64,
            sha256: format!("{:x}", Sha256::digest(bytes)),
        };
        let recorder = Arc::new(MessageRecorder::new());
        let relay = ChannelStreamRelay::new(
            cfg(PluginType::Telegram),
            recorder.clone(),
            crate::pending_decision::PendingDecisionStore::new(),
            None,
        );
        let (tx, rx) = tokio::sync::broadcast::channel(8);
        tx.send(AgentStreamEvent::ToolCall(ToolCallEventData {
            call_id: "tool-failed".into(),
            name: "mcp__reports__export".into(),
            args: serde_json::Value::Null,
            status: ToolCallStatus::Error,
            input: None,
            output: Some("artifact batch failed".into()),
            description: None,
            artifacts: vec![artifact],
        }))
        .unwrap();
        tx.send(AgentStreamEvent::Error(ErrorEventData::legacy(
            "tool failed",
            None,
        )))
        .unwrap();
        drop(tx);

        relay.run(rx).await;

        assert!(
            recorder.take_media().is_empty(),
            "receipts from a failed tool call must never enter the upload queue"
        );
    }

    #[tokio::test]
    async fn completed_artifact_is_discarded_when_turn_later_errors() {
        use nomifun_ai_agent::artifact_store::{ArtifactKind, PersistedArtifact};
        use nomifun_ai_agent::protocol::events::{ErrorEventData, ToolCallEventData, ToolCallStatus};

        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("partial.pdf");
        let bytes = b"%PDF-partial";
        std::fs::write(&path, bytes).unwrap();
        let artifact = PersistedArtifact {
            id: PersistedArtifactId::new().into_string(),
            kind: ArtifactKind::File,
            mime_type: "application/pdf".into(),
            path: path.to_string_lossy().into_owned(),
            relative_path: "nomifun-artifacts/partial.pdf".into(),
            size_bytes: bytes.len() as u64,
            sha256: format!("{:x}", Sha256::digest(bytes)),
        };
        let recorder = Arc::new(MessageRecorder::new());
        let relay = ChannelStreamRelay::new(
            cfg(PluginType::Telegram),
            recorder.clone(),
            crate::pending_decision::PendingDecisionStore::new(),
            None,
        );
        let (tx, rx) = tokio::sync::broadcast::channel(8);
        tx.send(AgentStreamEvent::ToolCall(ToolCallEventData {
            call_id: "tool-partial".into(),
            name: "mcp__reports__export".into(),
            args: serde_json::Value::Null,
            status: ToolCallStatus::Completed,
            input: None,
            output: Some("done".into()),
            description: None,
            artifacts: vec![artifact],
        }))
        .unwrap();
        tx.send(AgentStreamEvent::Error(ErrorEventData::legacy(
            "a later required tool failed",
            None,
        )))
        .unwrap();
        drop(tx);

        relay.run(rx).await;

        assert!(
            recorder.take_media().is_empty(),
            "an overall turn error must discard artifacts queued by earlier calls"
        );
    }

    #[tokio::test]
    async fn late_completed_frame_cannot_reverse_failed_tool_and_upload_artifact() {
        use nomifun_ai_agent::artifact_store::{ArtifactKind, PersistedArtifact};
        use nomifun_ai_agent::protocol::events::{FinishEventData, ToolCallEventData, ToolCallStatus};

        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("late.pdf");
        let bytes = b"%PDF-late";
        std::fs::write(&path, bytes).unwrap();
        let artifact = PersistedArtifact {
            id: PersistedArtifactId::new().into_string(),
            kind: ArtifactKind::File,
            mime_type: "application/pdf".into(),
            path: path.to_string_lossy().into_owned(),
            relative_path: "nomifun-artifacts/late.pdf".into(),
            size_bytes: bytes.len() as u64,
            sha256: format!("{:x}", Sha256::digest(bytes)),
        };
        let event = |status, artifacts| {
            AgentStreamEvent::ToolCall(ToolCallEventData {
                call_id: "same-call".into(),
                name: "mcp__reports__export".into(),
                args: serde_json::Value::Null,
                status,
                input: None,
                output: None,
                description: None,
                artifacts,
            })
        };
        let recorder = Arc::new(MessageRecorder::new());
        let relay = ChannelStreamRelay::new(
            cfg(PluginType::Telegram),
            recorder.clone(),
            crate::pending_decision::PendingDecisionStore::new(),
            None,
        );
        let (tx, rx) = tokio::sync::broadcast::channel(8);
        tx.send(event(ToolCallStatus::Error, Vec::new())).unwrap();
        tx.send(event(ToolCallStatus::Completed, vec![artifact])).unwrap();
        tx.send(AgentStreamEvent::Finish(FinishEventData::default()))
            .unwrap();
        drop(tx);

        relay.run(rx).await;

        assert!(
            recorder.take_media().is_empty(),
            "the first terminal state is absorbing for channel artifact delivery"
        );
    }

    #[tokio::test]
    async fn stream_close_without_finish_discards_queued_artifact() {
        use nomifun_ai_agent::artifact_store::{ArtifactKind, PersistedArtifact};
        use nomifun_ai_agent::protocol::events::{ToolCallEventData, ToolCallStatus};

        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("orphan.pdf");
        let bytes = b"%PDF-orphan";
        std::fs::write(&path, bytes).unwrap();
        let recorder = Arc::new(MessageRecorder::new());
        let relay = ChannelStreamRelay::new(
            cfg(PluginType::Telegram),
            recorder.clone(),
            crate::pending_decision::PendingDecisionStore::new(),
            None,
        );
        let (tx, rx) = tokio::sync::broadcast::channel(8);
        tx.send(AgentStreamEvent::ToolCall(ToolCallEventData {
            call_id: "orphan-call".into(),
            name: "mcp__reports__export".into(),
            args: serde_json::Value::Null,
            status: ToolCallStatus::Completed,
            input: None,
            output: None,
            description: None,
            artifacts: vec![PersistedArtifact {
                id: PersistedArtifactId::new().into_string(),
                kind: ArtifactKind::File,
                mime_type: "application/pdf".into(),
                path: path.to_string_lossy().into_owned(),
                relative_path: "nomifun-artifacts/orphan.pdf".into(),
                size_bytes: bytes.len() as u64,
                sha256: format!("{:x}", Sha256::digest(bytes)),
            }],
        }))
        .unwrap();
        drop(tx);

        relay.run(rx).await;

        assert!(recorder.take_media().is_empty());
        assert!(recorder.take_edits().iter().any(|message| {
            message
                .text
                .as_deref()
                .is_some_and(|text| text.contains("ended before completion"))
        }));
    }

    #[tokio::test]
    async fn send_once_channel_does_not_publish_partial_success_text_before_finish() {
        use nomifun_ai_agent::protocol::events::{TextEventData, ToolCallEventData, ToolCallStatus};

        let recorder = Arc::new(MessageRecorder::new());
        let relay = ChannelStreamRelay::new(
            cfg(PluginType::Weixin),
            recorder.clone(),
            crate::pending_decision::PendingDecisionStore::new(),
            None,
        );
        let (tx, rx) = tokio::sync::broadcast::channel(8);
        tx.send(AgentStreamEvent::Text(TextEventData {
            content: "Image generated successfully".into(),
        }))
        .unwrap();
        tx.send(AgentStreamEvent::ToolCall(ToolCallEventData {
            call_id: "failed-after-text".into(),
            name: "ImageGeneration".into(),
            args: serde_json::Value::Null,
            status: ToolCallStatus::Running,
            input: None,
            output: None,
            description: None,
            artifacts: Vec::new(),
        }))
        .unwrap();
        drop(tx);

        relay.run(rx).await;

        let sends = recorder.take_sends();
        assert!(sends.iter().any(|message| {
            message
                .text
                .as_deref()
                .is_some_and(|text| text.contains("ended before completion"))
        }));
        assert!(sends.iter().all(|message| {
            !message
                .text
                .as_deref()
                .is_some_and(|text| text.contains("generated successfully"))
        }));
    }

    #[tokio::test]
    async fn editable_channel_fails_closed_when_stream_events_are_lost() {
        use nomifun_ai_agent::protocol::events::{FinishEventData, TextEventData};

        let recorder = Arc::new(MessageRecorder::new());
        let relay = ChannelStreamRelay::new(
            cfg(PluginType::Telegram),
            recorder.clone(),
            crate::pending_decision::PendingDecisionStore::new(),
            None,
        );
        let (tx, rx) = tokio::sync::broadcast::channel(1);
        tx.send(AgentStreamEvent::Text(TextEventData {
            content: "unverified partial success".into(),
        }))
        .unwrap();
        tx.send(AgentStreamEvent::Finish(FinishEventData {
            session_id: None,
            stop_reason: None,
        }))
        .unwrap();
        drop(tx);

        relay.run(rx).await;

        assert!(recorder.take_media().is_empty());
        assert!(recorder.take_edits().iter().any(|message| {
            message
                .text
                .as_deref()
                .is_some_and(|text| text.contains("stream skipped 1 event"))
        }));
    }

    #[tokio::test]
    async fn send_once_channel_fails_closed_when_stream_events_are_lost() {
        use nomifun_ai_agent::protocol::events::{FinishEventData, TextEventData};

        let recorder = Arc::new(MessageRecorder::new());
        let relay = ChannelStreamRelay::new(
            cfg(PluginType::Weixin),
            recorder.clone(),
            crate::pending_decision::PendingDecisionStore::new(),
            None,
        );
        let (tx, rx) = tokio::sync::broadcast::channel(1);
        tx.send(AgentStreamEvent::Text(TextEventData {
            content: "unverified partial success".into(),
        }))
        .unwrap();
        tx.send(AgentStreamEvent::Finish(FinishEventData {
            session_id: None,
            stop_reason: None,
        }))
        .unwrap();
        drop(tx);

        relay.run(rx).await;

        assert!(recorder.take_media().is_empty());
        let sends = recorder.take_sends();
        assert!(sends.iter().any(|message| {
            message
                .text
                .as_deref()
                .is_some_and(|text| text.contains("stream skipped 1 event"))
        }));
        assert!(sends.iter().all(|message| {
            !message
                .text
                .as_deref()
                .is_some_and(|text| text.contains("unverified partial success"))
        }));
    }

    // Without a resolver wired, no media is sent (graceful text-only behaviour).
    #[tokio::test]
    async fn relay_without_resolver_sends_no_media() {
        use nomifun_ai_agent::protocol::events::{FinishEventData, ToolCallEventData, ToolCallStatus};

        let recorder = Arc::new(MessageRecorder::new());
        let pending = crate::pending_decision::PendingDecisionStore::new();
        let relay = ChannelStreamRelay::new(cfg(PluginType::Telegram), recorder.clone(), pending, None);

        let (tx, rx) = tokio::sync::broadcast::channel(16);
        tx.send(AgentStreamEvent::ToolCall(ToolCallEventData {
            call_id: "t".into(),
            name: "nomi_workshop_get_task".into(),
            args: serde_json::Value::Null,
            status: ToolCallStatus::Completed,
            description: None,
            input: None,
            output: Some(
                r#"{"result_asset_ids":["0190f5fe-7c00-7a00-8000-000000000081"]}"#
                    .into(),
            ),
            artifacts: Vec::new(),
        })).unwrap();
        tx.send(AgentStreamEvent::Finish(FinishEventData { session_id: None, stop_reason: None })).unwrap();
        drop(tx);

        relay.run(rx).await;
        assert!(recorder.take_media().is_empty(), "no resolver → no media");
    }
}

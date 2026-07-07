use std::collections::HashMap;
use std::sync::Arc;

use nomifun_ai_agent::{
    AgentSendError, AgentStreamEvent,
    protocol::events::{
        PlanEventData, ThinkingEventData,
        tool_call::{AcpToolCallSessionUpdateKind, AcpToolCallStatus, ToolCallEventData, ToolCallStatus},
    },
};

use crate::response_middleware::{ICronService, MessageMiddleware, MiddlewareResult};
use crate::runtime_state::ConversationRuntimeStateService;
use nomifun_api_types::{AgentErrorCode, ConversationRuntimeSummary, WebSocketMessage};
use nomifun_common::{ErrorChain, normalize_keys_to_snake_case, now_ms};

use crate::service::ConversationService;
use nomifun_db::IConversationRepository;
use nomifun_db::models::MessageRow;
use nomifun_realtime::EventBroadcaster;
use serde_json::json;
use tokio::sync::{broadcast, oneshot};
use tracing::{debug, error, info, warn};

/// Number of text chunks to accumulate before flushing to the database.
const FLUSH_INTERVAL: u32 = 20;

#[derive(Debug, Clone)]
struct TextSegmentState {
    id: String,
    buffer: String,
    created_at: i64,
    record_created: bool,
    flush_counter: u32,
}

#[derive(Debug, Clone)]
struct PersistedTextSegment {
    id: String,
}

#[derive(Debug, Clone)]
struct ThinkingSegmentState {
    id: String,
    buffer: String,
    started_at: i64,
}

/// Result returned after a relay turn has fully drained and finalized.
#[derive(Debug, Clone, Default)]
pub struct RelayOutcome {
    pub system_responses: Vec<String>,
    pub terminal: RelayTerminal,
    /// Phase 3 (plan D4): whether this turn emitted **any** externally-visible
    /// response before terminating — assistant `Text` OR a forwarded/persisted
    /// tool action (ToolCall / AcpToolCall / ToolGroup / persisted Thinking).
    /// The failover seam only switches models pre-response (`!emitted_response`)
    /// so a fault AFTER any visible output is never failed over — that would
    /// duplicate already-streamed text OR re-run a tool side effect (and re-bill).
    pub emitted_response: bool,
    /// Phase 3 (review #1/#5): when the relay SUPPRESSED a pre-response provider
    /// fault (no WS error event, no error `tips` row — because the send loop was
    /// expected to fail over), this carries the swallowed `Error` event. The send
    /// loop re-surfaces it (broadcast + persist) if the failover did NOT actually
    /// fire (e.g. the picker found no usable candidate at runtime) — preserving
    /// the "queue-exhausted → ORIGINAL error" invariant. `None` = nothing suppressed.
    pub suppressed_error: Option<AgentStreamEvent>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum RelayTerminal {
    #[default]
    Finish,
    Error {
        code: Option<AgentErrorCode>,
        retryable: Option<bool>,
    },
    ChannelClosed,
}

impl RelayTerminal {
    pub fn is_error(&self) -> bool {
        matches!(self, Self::Error { .. })
    }

    pub fn code(&self) -> Option<AgentErrorCode> {
        match self {
            Self::Error { code, .. } => *code,
            Self::Finish | Self::ChannelClosed => None,
        }
    }

    pub fn retryable(&self) -> Option<bool> {
        match self {
            Self::Error { retryable, .. } => *retryable,
            Self::Finish | Self::ChannelClosed => None,
        }
    }
}

/// Relays agent stream events to WebSocket and persists messages.
///
/// This struct is created for each `send_message` call and runs as a
/// background tokio task until the agent finishes or errors out.
pub struct StreamRelay {
    conversation_id: String,
    msg_id: String,
    user_id: String,
    repo: Arc<dyn IConversationRepository>,
    broadcaster: Arc<dyn EventBroadcaster>,
    cron_service: Option<Arc<dyn ICronService>>,
    complete_turn: bool,
    /// Companion-companion wire markers (from `conversation.extra.companionSession` /
    /// `.companionId`), stamped onto every `message.stream` / `turn.completed`
    /// payload so the companion collector can classify the turn off the wire.
    companion: bool,
    companion_id: Option<String>,
    /// Originator of the user message that started this turn when it was NOT
    /// typed by the human owner (`"companion"` / `"cron"` / `"autowork"` /
    /// `"idmm"`; `None` = a real person). Stamped onto every `message.stream`
    /// / `turn.completed` payload of the turn so downstream consumers (the
    /// companion collector) can tell agent-driven replies from owner-driven work.
    origin: Option<String>,
    /// IM platform of a channel master-agent conversation (from
    /// `conversation.extra.channelPlatform`, e.g. `"telegram"`; `None` = not
    /// a channel conversation). Stamped onto every `message.stream` /
    /// `turn.completed` payload so the companion window can tell remote IM turns
    /// from local companion turns off the wire.
    channel_platform: Option<String>,
    /// Phase 3 (review #1/#5): predicate telling the relay whether a PRE-RESPONSE
    /// terminal provider-fault with this error code WILL be failed over by the
    /// send loop. When it returns `true` the relay suppresses the user-visible
    /// error AT SOURCE — it does NOT forward the WS error event NOR persist the
    /// error `tips` row — so a recovered fault shows only the backup model's turn,
    /// never the swallowed error. `None` (the default) = never suppress. The
    /// send loop is the only caller that wires this (it knows nomi + enabled +
    /// within-bound up front; pre-response + provider-fault are evaluated here).
    #[allow(clippy::type_complexity)]
    failover_suppressor: Option<Arc<dyn Fn(AgentErrorCode) -> bool + Send + Sync>>,
    /// Process-wide runtime state, used here ONLY to accumulate this turn's
    /// `TurnCompleted` token usage (`input + output`) into the conversation's
    /// running total so the orchestrator worker can read it after the turn
    /// settles (DAG/inspector per-node token display). `None` (the default) =
    /// no token accumulation (the common chat/companion path is unaffected).
    /// The GATE is NOT in this relay (nor per-`ConversationService`): a single
    /// shared `ConversationService` serves both chat and orchestrator turns. It is
    /// `ConversationService::send_message` that decides, PER TURN, whether to wire
    /// this — it sets it ONLY when the conversation row's `extra` carries an
    /// `orchestrator_run_id` (see its `token_runtime_state`). Once wired here, this
    /// relay always accumulates (no further conditional below).
    runtime_state: Option<Arc<ConversationRuntimeStateService>>,
}

impl StreamRelay {
    pub fn new(
        conversation_id: String,
        msg_id: String,
        user_id: String,
        repo: Arc<dyn IConversationRepository>,
        broadcaster: Arc<dyn EventBroadcaster>,
        cron_service: Option<Arc<dyn ICronService>>,
    ) -> Self {
        Self {
            conversation_id,
            msg_id,
            user_id,
            repo,
            broadcaster,
            cron_service,
            complete_turn: true,
            companion: false,
            companion_id: None,
            origin: None,
            channel_platform: None,
            failover_suppressor: None,
            runtime_state: None,
        }
    }

    pub fn with_turn_completion(mut self, enabled: bool) -> Self {
        self.complete_turn = enabled;
        self
    }

    /// Wire the process-wide runtime state so this relay accumulates each turn's
    /// `TurnCompleted` token usage into the conversation's running total (read
    /// back by the orchestrator worker after the turn settles). Wired PER TURN by
    /// `ConversationService::send_message` — and only for a conversation whose row
    /// carries an `orchestrator_run_id` (the gate lives there, not in this relay or
    /// in a dedicated service). The default chat/companion turn leaves it `None`
    /// (no accumulation, no behaviour change).
    pub fn with_runtime_state(mut self, runtime_state: Arc<ConversationRuntimeStateService>) -> Self {
        self.runtime_state = Some(runtime_state);
        self
    }

    /// Wire the pre-response failover error-suppressor (review #1/#5). When the
    /// predicate returns `true` for a pre-response provider-fault's error code,
    /// the relay swallows the user-visible error (no WS error event, no error
    /// `tips` row) because the send loop will fail over and re-run the turn.
    pub fn with_failover_suppressor(
        mut self,
        suppressor: Arc<dyn Fn(AgentErrorCode) -> bool + Send + Sync>,
    ) -> Self {
        self.failover_suppressor = Some(suppressor);
        self
    }

    /// Tag this relay's broadcasts with the conversation's companion-companion
    /// markers (no-op markers by default; see field docs).
    pub fn with_companion_context(mut self, companion: bool, companion_id: Option<String>) -> Self {
        self.companion = companion;
        self.companion_id = companion_id;
        self
    }

    /// Tag this relay's broadcasts with the originating user message's
    /// `origin` marker (see field docs). Blank values normalize to `None`.
    pub fn with_origin(mut self, origin: Option<String>) -> Self {
        self.origin = origin
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_owned);
        self
    }

    /// Tag this relay's broadcasts with the conversation's IM platform
    /// marker (see field docs). Blank values normalize to `None`.
    pub fn with_channel_platform(mut self, channel_platform: Option<String>) -> Self {
        self.channel_platform = channel_platform
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_owned);
        self
    }

    /// Run the relay loop. Consumes `self` and runs until the agent stream ends.
    #[tracing::instrument(
        skip_all,
        fields(
            conversation_id = %self.conversation_id,
            msg_id = %self.msg_id,
        )
    )]
    pub async fn consume(self, rx: broadcast::Receiver<AgentStreamEvent>) -> RelayOutcome {
        self.consume_inner(rx, None).await
    }

    /// Re-surface a terminal `Error` event the relay previously SUPPRESSED for a
    /// pending failover that then did NOT fire (review #1/#5). Mirrors the relay's
    /// own terminal-error side effects: broadcast the WS `message.stream` error
    /// event and persist the error `tips` row — so a queue-exhausted failover
    /// still shows the ORIGINAL error. No-op for non-`Error` events.
    pub async fn surface_terminal_error(&self, event: &AgentStreamEvent) {
        let AgentStreamEvent::Error(data) = event else {
            return;
        };
        self.forward_to_websocket(event);
        self.persist_error_tips(data).await;
    }

    /// Run the relay loop while also accepting a typed send failure from the
    /// task that called `IAgentTask::send_message`.
    #[tracing::instrument(
        skip_all,
        fields(
            conversation_id = %self.conversation_id,
            msg_id = %self.msg_id,
        )
    )]
    pub async fn consume_with_send_error(
        self,
        rx: broadcast::Receiver<AgentStreamEvent>,
        send_error_rx: oneshot::Receiver<AgentSendError>,
    ) -> RelayOutcome {
        self.consume_inner(rx, Some(send_error_rx)).await
    }

    async fn consume_inner(
        self,
        mut rx: broadcast::Receiver<AgentStreamEvent>,
        mut send_error_rx: Option<oneshot::Receiver<AgentSendError>>,
    ) -> RelayOutcome {
        let started_at = now_ms();
        info!("StreamRelay started");

        let mut full_text_buffer = String::new();
        let mut text_segments: Vec<PersistedTextSegment> = Vec::new();
        let mut active_text: Option<TextSegmentState> = None;
        let mut active_thinking: Option<ThinkingSegmentState> = None;
        let mut active_tool_calls: HashMap<String, ToolCallEventData> = HashMap::new();
        let mut used_primary_segment_msg_id = false;
        let mut first_agent_event_logged = false;
        let mut first_visible_output_logged = false;
        // Phase 3 (plan D4): tracks whether any externally-visible response has
        // been emitted this turn — assistant Text OR a forwarded/persisted tool
        // action. Surfaced on the RelayOutcome so the failover seam can restrict
        // switching to faults that produced NO visible output (no duplicate
        // text, no duplicate tool side effect / billing).
        let mut emitted_response = false;
        let mut send_error_done = send_error_rx.is_none();

        loop {
            let recv_result = if send_error_done {
                rx.recv().await
            } else {
                tokio::select! {
                    recv = rx.recv() => recv,
                    send_error = send_error_rx.as_mut().expect("send_error_rx exists while pending") => {
                        send_error_done = true;
                        match send_error {
                            Ok(send_error) => {
                                warn!(
                                    code = ?send_error.code(),
                                    ownership = ?send_error.ownership(),
                                    "Injecting stream error for failed agent send"
                                );
                                Ok(AgentStreamEvent::Error(send_error.into_stream_error()))
                            }
                            Err(_) => continue,
                        }
                    }
                }
            };

            match recv_result {
                Ok(event) => {
                    if !first_agent_event_logged {
                        first_agent_event_logged = true;
                        info!(
                            event_type = Self::event_kind(&event),
                            elapsed_ms = now_ms().saturating_sub(started_at),
                            "StreamRelay received first agent event"
                        );
                    }

                    match &event {
                        AgentStreamEvent::Thinking(data) => {
                            if data.status.as_deref() == Some("done") {
                                self.complete_active_thinking(&mut active_thinking).await;
                                continue;
                            }

                            // Plan D4: a broadcast/persisted thinking segment is
                            // externally visible — once it streams we are no
                            // longer pre-response, so the failover seam stands down.
                            emitted_response = true;
                            self.close_active_text_segment(&mut active_text, &mut text_segments, "finish")
                                .await;
                            if !first_visible_output_logged && !data.content.is_empty() {
                                first_visible_output_logged = true;
                                info!(
                                    event_type = "Thinking",
                                    elapsed_ms = now_ms().saturating_sub(started_at),
                                    "StreamRelay received first visible output"
                                );
                            }

                            let segment = active_thinking.get_or_insert_with(|| ThinkingSegmentState {
                                id: Self::mint_segment_msg_id(&mut used_primary_segment_msg_id, &self.msg_id),
                                buffer: String::new(),
                                started_at: now_ms(),
                            });
                            segment.buffer.push_str(&data.content);
                            self.forward_to_websocket_with_msg_id(&segment.id, &event);
                        }
                        AgentStreamEvent::Text(data) => {
                            self.complete_active_thinking(&mut active_thinking).await;
                            // Plan D4: any assistant Text means we are no longer
                            // pre-response. The failover seam keys off this.
                            emitted_response = true;
                            if !first_visible_output_logged && !data.content.is_empty() {
                                first_visible_output_logged = true;
                                info!(
                                    event_type = "Text",
                                    elapsed_ms = now_ms().saturating_sub(started_at),
                                    "StreamRelay received first visible output"
                                );
                            }

                            let segment = active_text.get_or_insert_with(|| TextSegmentState {
                                id: Self::mint_segment_msg_id(&mut used_primary_segment_msg_id, &self.msg_id),
                                buffer: String::new(),
                                created_at: now_ms(),
                                record_created: false,
                                flush_counter: 0,
                            });
                            self.forward_to_websocket_with_msg_id(&segment.id, &event);
                            segment.buffer.push_str(&data.content);
                            full_text_buffer.push_str(&data.content);
                            segment.flush_counter += 1;
                            if segment.flush_counter >= FLUSH_INTERVAL {
                                self.flush_text_segment(segment).await;
                                segment.flush_counter = 0;
                            }
                        }
                        AgentStreamEvent::Finish(_) | AgentStreamEvent::Error(_) => {
                            let elapsed_ms = now_ms() - started_at;
                            let event_type = if matches!(event, AgentStreamEvent::Finish(_)) {
                                "Finish"
                            } else {
                                "Error"
                            };
                            let terminal = Self::terminal_from_event(&event);
                            match &terminal {
                                RelayTerminal::Error { code, retryable } => {
                                    info!(
                                        event_type,
                                        elapsed_ms,
                                        text_len = full_text_buffer.len(),
                                        error_code = ?code,
                                        retryable = ?retryable,
                                        "StreamRelay received terminal event"
                                    );
                                }
                                RelayTerminal::Finish | RelayTerminal::ChannelClosed => {
                                    info!(
                                        event_type,
                                        elapsed_ms,
                                        text_len = full_text_buffer.len(),
                                        "StreamRelay received terminal event"
                                    );
                                }
                            }

                            self.complete_active_thinking(&mut active_thinking).await;
                            self.close_active_text_segment(
                                &mut active_text,
                                &mut text_segments,
                                if matches!(event, AgentStreamEvent::Error(_)) {
                                    "error"
                                } else {
                                    "finish"
                                },
                            )
                            .await;
                            // review #1/#5: a pre-response provider-fault that the
                            // send loop will fail over must NOT reach the user —
                            // suppress the WS error event AND the error `tips` row
                            // at source, so a recovered turn shows only the backup
                            // model's output. Only the Error terminal with no
                            // emitted response and a positive suppressor verdict
                            // qualifies; everything else broadcasts/persists as before.
                            let suppress_error = !emitted_response
                                && matches!(event, AgentStreamEvent::Error(_))
                                && terminal
                                    .code()
                                    .zip(self.failover_suppressor.as_ref())
                                    .is_some_and(|(code, suppressor)| suppressor(code));
                            if suppress_error {
                                info!("StreamRelay suppressing pre-response error pending model failover");
                            } else {
                                if let Some(reason) = Self::incomplete_tool_reason(&event) {
                                    self.fail_active_tool_calls(&mut active_tool_calls, reason).await;
                                }
                                self.forward_to_websocket(&event);
                            }
                            let outcome = self
                                .finalize(
                                    &full_text_buffer,
                                    &text_segments,
                                    &event,
                                    terminal,
                                    emitted_response,
                                    suppress_error,
                                )
                                .await;
                            if self.complete_turn {
                                Self::complete_conversation_with_context(
                                    &self.repo,
                                    &self.broadcaster,
                                    &self.conversation_id,
                                    None,
                                    self.companion,
                                    self.companion_id.clone(),
                                    self.origin.clone(),
                                    self.channel_platform.clone(),
                                )
                                .await;
                            }
                            break outcome;
                        }
                        AgentStreamEvent::ToolCall(data) => {
                            // Plan D4: a forwarded/persisted tool call is an
                            // externally-visible action with a side effect — no
                            // failover after this, or the tool would re-run.
                            emitted_response = true;
                            match data.status {
                                ToolCallStatus::Running => {
                                    active_tool_calls.insert(data.call_id.clone(), data.clone());
                                }
                                ToolCallStatus::Completed | ToolCallStatus::Error => {
                                    active_tool_calls.remove(&data.call_id);
                                }
                            }
                            self.complete_active_thinking(&mut active_thinking).await;
                            self.close_active_text_segment(&mut active_text, &mut text_segments, "finish")
                                .await;
                            self.forward_to_websocket(&event);
                            self.persist_tool_call(data).await;
                        }
                        AgentStreamEvent::AcpToolCall(data) => {
                            // Plan D4: see ToolCall — an ACP tool call is a
                            // visible, side-effecting action; block failover.
                            emitted_response = true;
                            self.complete_active_thinking(&mut active_thinking).await;
                            self.close_active_text_segment(&mut active_text, &mut text_segments, "finish")
                                .await;
                            self.forward_to_websocket(&event);
                            self.persist_acp_tool_call(data).await;
                        }
                        AgentStreamEvent::ToolGroup(entries) => {
                            // Plan D4: see ToolCall — a tool group is a visible,
                            // side-effecting action; block failover.
                            emitted_response = true;
                            self.complete_active_thinking(&mut active_thinking).await;
                            self.close_active_text_segment(&mut active_text, &mut text_segments, "finish")
                                .await;
                            self.forward_to_websocket(&event);
                            self.persist_tool_group(entries).await;
                        }
                        AgentStreamEvent::AgentStatus(data) => {
                            self.forward_to_websocket(&event);
                            if data.backend == "nomi" && (data.status == "preparing" || data.status == "prepared") {
                                self.persist_agent_status(data).await;
                            }
                        }
                        AgentStreamEvent::Plan(data) => {
                            emitted_response = true;
                            self.complete_active_thinking(&mut active_thinking).await;
                            self.close_active_text_segment(
                                &mut active_text,
                                &mut text_segments,
                                "finish",
                            )
                            .await;
                            let plan_id = self.plan_message_id(data);
                            self.forward_to_websocket_with_msg_id(&plan_id, &event);
                            self.persist_plan(data).await;
                        }
                        AgentStreamEvent::TurnCompleted(metrics) => {
                            // Accumulate this turn's token usage into the
                            // conversation's running total (only when this relay was
                            // wired — `send_message` wires it solely for a turn whose
                            // conversation carries an `orchestrator_run_id`; that is
                            // where the gate lives). The worker reads it back after
                            // the turn settles to fill `orch_run_tasks.tokens`.
                            // `context_tokens` is a gauge (last-request occupancy), so
                            // per-turn COST is the additive `input + output`. Recorded
                            // BEFORE the turn claim releases, so the polling worker
                            // never races it.
                            if let Some(runtime_state) = self.runtime_state.as_ref() {
                                let turn_tokens =
                                    metrics.input_tokens.saturating_add(metrics.output_tokens);
                                runtime_state
                                    .add_turn_tokens(&self.conversation_id, turn_tokens as i64);
                            }
                            self.forward_to_websocket(&event);
                        }
                        _ => {
                            self.forward_to_websocket(&event);
                        }
                    }
                }
                Err(broadcast::error::RecvError::Closed) => {
                    let elapsed_ms = now_ms() - started_at;
                    warn!(
                        elapsed_ms,
                        text_len = full_text_buffer.len(),
                        "StreamRelay channel closed without terminal event"
                    );

                    self.complete_active_thinking(&mut active_thinking).await;
                    self.close_active_text_segment(&mut active_text, &mut text_segments, "finish")
                        .await;
                    // Channel closed without finish/error — still finalize
                    let outcome = self
                        .finalize(
                            &full_text_buffer,
                            &text_segments,
                            &AgentStreamEvent::Finish(nomifun_ai_agent::protocol::events::FinishEventData::default()),
                            RelayTerminal::ChannelClosed,
                            emitted_response,
                            // A channel-closed terminal is never an Error event,
                            // so there is nothing to suppress here.
                            false,
                        )
                        .await;
                    if self.complete_turn {
                        Self::complete_conversation_with_context(
                            &self.repo,
                            &self.broadcaster,
                            &self.conversation_id,
                            None,
                            self.companion,
                            self.companion_id.clone(),
                            self.origin.clone(),
                            self.channel_platform.clone(),
                        )
                        .await;
                    }
                    break outcome;
                }
                Err(broadcast::error::RecvError::Lagged(n)) => {
                    warn!(lagged = n, "Stream relay lagged, some events dropped");
                }
            }
        }
    }

    fn event_kind(event: &AgentStreamEvent) -> &'static str {
        match event {
            AgentStreamEvent::Start(_) => "Start",
            AgentStreamEvent::Text(_) => "Text",
            AgentStreamEvent::Tips(_) => "Tips",
            AgentStreamEvent::Thinking(_) => "Thinking",
            AgentStreamEvent::ToolCall(_) => "ToolCall",
            AgentStreamEvent::AcpToolCall(_) => "AcpToolCall",
            AgentStreamEvent::ToolGroup(_) => "ToolGroup",
            AgentStreamEvent::AgentStatus(_) => "AgentStatus",
            AgentStreamEvent::Plan(_) => "Plan",
            AgentStreamEvent::Permission(_) => "Permission",
            AgentStreamEvent::AcpPermission(_) => "AcpPermission",
            AgentStreamEvent::SkillSuggest(_) => "SkillSuggest",
            AgentStreamEvent::CronTrigger(_) => "CronTrigger",
            AgentStreamEvent::AcpModelInfo(_) => "AcpModelInfo",
            AgentStreamEvent::AcpModeInfo(_) => "AcpModeInfo",
            AgentStreamEvent::AcpConfigOption(_) => "AcpConfigOption",
            AgentStreamEvent::AcpSessionInfo(_) => "AcpSessionInfo",
            AgentStreamEvent::AcpContextUsage(_) => "AcpContextUsage",
            AgentStreamEvent::AcpPromptHookWarning(_) => "AcpPromptHookWarning",
            AgentStreamEvent::SlashCommandsUpdated(_) => "SlashCommandsUpdated",
            AgentStreamEvent::AvailableCommands(_) => "AvailableCommands",
            AgentStreamEvent::TurnCompleted(_) => "TurnCompleted",
            AgentStreamEvent::Finish(_) => "Finish",
            AgentStreamEvent::Error(_) => "Error",
            AgentStreamEvent::System(_) => "System",
            AgentStreamEvent::RequestTrace(_) => "RequestTrace",
            AgentStreamEvent::SessionAssigned(_) => "SessionAssigned",
        }
    }

    fn terminal_from_event(event: &AgentStreamEvent) -> RelayTerminal {
        match event {
            AgentStreamEvent::Error(data) => RelayTerminal::Error {
                code: data.code,
                retryable: data.retryable,
            },
            AgentStreamEvent::Finish(_) => RelayTerminal::Finish,
            _ => RelayTerminal::ChannelClosed,
        }
    }

    fn mint_segment_msg_id(used_primary: &mut bool, primary_msg_id: &str) -> String {
        if !*used_primary {
            *used_primary = true;
            primary_msg_id.to_owned()
        } else {
            ConversationService::mint_msg_id()
        }
    }

    /// The integer conversation key for repo calls and `MessageRow` builds.
    /// `StreamRelay.conversation_id` stays the public String form; this parses
    /// it to the i64 the DB layer now uses. A relay is only ever constructed
    /// with a real (numeric) conversation id, so a non-numeric value here is a
    /// programming error and falls back to `0` rather than failing the turn.
    fn conv_id(&self) -> i64 {
        self.conversation_id.parse().unwrap_or_default()
    }

    /// Forward an agent event to connected WebSocket clients.
    #[tracing::instrument(skip_all)]
    fn forward_to_websocket(&self, event: &AgentStreamEvent) {
        self.forward_to_websocket_with_msg_id(&self.msg_id, event);
    }

    #[tracing::instrument(skip_all)]
    fn forward_to_websocket_with_msg_id(&self, msg_id: &str, event: &AgentStreamEvent) {
        let mut event_data = match serde_json::to_value(event) {
            Ok(v) => v,
            Err(e) => {
                warn!(error = %ErrorChain(&e), "Failed to serialize agent event for WebSocket");
                return;
            }
        };
        // Nested ACP SDK payloads serialise as camelCase on their own;
        // force every object key down the tree to snake_case so the
        // wire contract stays uniform.
        normalize_keys_to_snake_case(&mut event_data);

        let payload = json!({
            "conversation_id": self.conv_id(),
            "msg_id": msg_id,
            "type": event_data.get("type").cloned().unwrap_or(json!("unknown")),
            "data": event_data.get("data").cloned().unwrap_or(json!({})),
            "hidden": false,
        });

        self.broadcast_stream_payload(payload);
    }

    /// Flush an active text segment to the database (create or update).
    #[tracing::instrument(skip_all)]
    async fn flush_text_segment(&self, segment: &mut TextSegmentState) {
        if segment.buffer.is_empty() {
            return;
        }

        let content = json!({ "content": segment.buffer }).to_string();

        if segment.record_created {
            let update = nomifun_db::MessageRowUpdate {
                content: Some(content),
                status: Some(Some("work".into())),
                hidden: None,
            };
            if let Err(e) = self.repo.update_message(&segment.id, &update).await {
                error!(error = %ErrorChain(&e), "Failed to update streaming text segment");
            }
        } else {
            let row = MessageRow {
                id: segment.id.clone(),
                conversation_id: self.conv_id(),
                msg_id: Some(segment.id.clone()),
                r#type: "text".into(),
                content,
                position: Some("left".into()),
                status: Some("work".into()),
                hidden: false,
                created_at: segment.created_at,
            };
            if let Err(e) = self.repo.insert_message(&row).await {
                error!(error = %ErrorChain(&e), "Failed to create streaming text segment");
            }
            segment.record_created = true;
        }
    }

    #[tracing::instrument(skip_all)]
    async fn finalize_text_segment(&self, segment: TextSegmentState, status: &str) -> Option<PersistedTextSegment> {
        if segment.buffer.is_empty() {
            return None;
        }

        let content = json!({ "content": segment.buffer }).to_string();
        if segment.record_created {
            let update = nomifun_db::MessageRowUpdate {
                content: Some(content),
                status: Some(Some(status.to_owned())),
                hidden: Some(false),
            };
            if let Err(e) = self.repo.update_message(&segment.id, &update).await {
                error!(error = %ErrorChain(&e), "Failed to finalize text segment");
            }
        } else {
            let row = MessageRow {
                id: segment.id.clone(),
                conversation_id: self.conv_id(),
                msg_id: Some(segment.id.clone()),
                r#type: "text".into(),
                content,
                position: Some("left".into()),
                status: Some(status.to_owned()),
                hidden: false,
                created_at: segment.created_at,
            };
            if let Err(e) = self.repo.insert_message(&row).await {
                error!(error = %ErrorChain(&e), "Failed to create finalized text segment");
            }
        }

        Some(PersistedTextSegment { id: segment.id })
    }

    /// Finalize assistant text on stream end and apply middleware rewrites.
    #[tracing::instrument(skip_all)]
    async fn finalize(
        &self,
        text: &str,
        text_segments: &[PersistedTextSegment],
        event: &AgentStreamEvent,
        terminal: RelayTerminal,
        emitted_response: bool,
        suppress_error: bool,
    ) -> RelayOutcome {
        let mut outcome = RelayOutcome {
            system_responses: Vec::new(),
            terminal,
            emitted_response,
            suppressed_error: None,
        };
        let status = match event {
            AgentStreamEvent::Error(_) => "error",
            _ => "finish",
        };

        if !text.is_empty() {
            let processed = self.process_final_text(text).await;
            let final_text = processed.message.trim().to_owned();
            let hidden = final_text.is_empty();

            if let Some(primary_segment) = text_segments.first() {
                if processed.message != text || hidden {
                    let content = json!({ "content": final_text }).to_string();
                    let update = nomifun_db::MessageRowUpdate {
                        content: Some(content),
                        status: Some(Some(status.to_owned())),
                        hidden: Some(hidden),
                    };
                    if let Err(e) = self.repo.update_message(&primary_segment.id, &update).await {
                        error!(error = %ErrorChain(&e), "Failed to rewrite finalized text segment");
                    }
                    self.send_final_text_override(&primary_segment.id, &processed.message, hidden);

                    for segment in text_segments.iter().skip(1) {
                        let hide_update = nomifun_db::MessageRowUpdate {
                            content: None,
                            status: Some(Some(status.to_owned())),
                            hidden: Some(true),
                        };
                        if let Err(e) = self.repo.update_message(&segment.id, &hide_update).await {
                            error!(error = %ErrorChain(&e), "Failed to hide superseded text segment");
                        }
                        self.send_final_text_override(&segment.id, "", true);
                    }
                } else {
                    for segment in text_segments {
                        let status_update = nomifun_db::MessageRowUpdate {
                            content: None,
                            status: Some(Some(status.to_owned())),
                            hidden: Some(false),
                        };
                        if let Err(e) = self.repo.update_message(&segment.id, &status_update).await {
                            error!(error = %ErrorChain(&e), "Failed to finalize text segment status");
                        }
                    }
                }
            } else if !hidden {
                let row = MessageRow {
                    id: self.msg_id.clone(),
                    conversation_id: self.conv_id(),
                    msg_id: Some(self.msg_id.clone()),
                    r#type: "text".into(),
                    content: json!({ "content": final_text }).to_string(),
                    position: Some("left".into()),
                    status: Some(status.to_owned()),
                    hidden: false,
                    created_at: now_ms(),
                };
                if let Err(e) = self.repo.insert_message(&row).await {
                    error!(error = %ErrorChain(&e), "Failed to create final fallback message");
                }
            }

            self.send_system_responses(&processed.system_responses);
            outcome.system_responses = processed.system_responses;
        } else if let AgentStreamEvent::Error(data) = event {
            if suppress_error {
                // review #1/#5: the send loop will (try to) fail over this
                // pre-response fault — do NOT persist the error tips row. Hand the
                // event back so the loop can re-surface it if the failover misses
                // (picker found no candidate), keeping queue-exhausted → original error.
                outcome.suppressed_error = Some(event.clone());
                return outcome;
            }
            // No text accumulated but got an error — store error as tips message.
            self.persist_error_tips(data).await;
        }

        outcome
    }

    /// Persist a terminal provider error as a `tips` message row (the "no text,
    /// got error" surface). Factored out so [`Self::surface_terminal_error`] can
    /// re-persist a previously-suppressed error on a missed failover (review #1/#5).
    async fn persist_error_tips(&self, data: &nomifun_ai_agent::protocol::events::ErrorEventData) {
        let content = json!({ "content": &data.message, "type": "error", "error": &data }).to_string();
        let row = MessageRow {
            id: ConversationService::mint_msg_id(),
            conversation_id: self.conv_id(),
            msg_id: None,
            r#type: "tips".into(),
            content,
            position: Some("left".into()),
            status: Some("error".into()),
            hidden: false,
            created_at: now_ms(),
        };
        if let Err(e) = self.repo.insert_message(&row).await {
            error!(error = %ErrorChain(&e), "Failed to store error message");
        }
    }

    #[tracing::instrument(skip_all)]
    async fn persist_agent_status(&self, data: &nomifun_ai_agent::protocol::events::AgentStatusEventData) {
        let id = format!("{}:agent_status:model_activity", self.msg_id);
        let content = serde_json::to_string(data).unwrap_or_else(|_| "{}".to_owned());
        let status = if data.status == "prepared" { "finish" } else { "work" };
        let existing = self
            .repo
            .get_message_by_msg_id(self.conv_id(), &id, "agent_status")
            .await
            .unwrap_or(None);

        if existing.is_some() {
            let update = nomifun_db::MessageRowUpdate {
                content: Some(content),
                status: Some(Some(status.to_owned())),
                hidden: Some(false),
            };
            if let Err(e) = self.repo.update_message(&id, &update).await {
                error!(
                    status = %data.status,
                    error = %ErrorChain(&e),
                    "Failed to update agent_status message"
                );
            }
            return;
        }

        let row = MessageRow {
            id: id.clone(),
            conversation_id: self.conv_id(),
            msg_id: Some(id),
            r#type: "agent_status".into(),
            content,
            position: Some("left".into()),
            status: Some(status.into()),
            hidden: false,
            created_at: now_ms(),
        };
        if let Err(e) = self.repo.insert_message(&row).await {
            error!(
                status = %data.status,
                error = %ErrorChain(&e),
                "Failed to persist agent_status message"
            );
        }
    }

    fn plan_session_id(&self, data: &PlanEventData) -> String {
        data.session_id
            .as_deref()
            .map(str::trim)
            .filter(|session_id| !session_id.is_empty())
            .unwrap_or(&self.msg_id)
            .to_owned()
    }

    fn plan_message_id(&self, data: &PlanEventData) -> String {
        format!("{}:plan:{}", self.msg_id, self.plan_session_id(data))
    }

    #[tracing::instrument(skip_all)]
    async fn persist_plan(&self, data: &PlanEventData) {
        let plan_id = self.plan_message_id(data);
        let session_id = self.plan_session_id(data);
        let status = if data.entries.iter().all(|entry| {
            entry.get("status").and_then(serde_json::Value::as_str) == Some("completed")
        }) {
            "finish"
        } else {
            "work"
        };
        let content = json!({
            "session_id": session_id,
            "entries": data.entries,
        })
        .to_string();

        let existing = self
            .repo
            .get_message_by_msg_id(self.conv_id(), &plan_id, "plan")
            .await
            .unwrap_or(None);

        if existing.is_some() {
            let update = nomifun_db::MessageRowUpdate {
                content: Some(content),
                status: Some(Some(status.to_owned())),
                hidden: Some(false),
            };
            if let Err(e) = self.repo.update_message(&plan_id, &update).await {
                error!(error = %ErrorChain(&e), "Failed to update plan message");
            }
            return;
        }

        let row = MessageRow {
            id: plan_id.clone(),
            conversation_id: self.conv_id(),
            msg_id: Some(plan_id),
            r#type: "plan".into(),
            content,
            position: Some("left".into()),
            status: Some(status.to_owned()),
            hidden: false,
            created_at: now_ms(),
        };
        if let Err(e) = self.repo.insert_message(&row).await {
            error!(error = %ErrorChain(&e), "Failed to persist plan message");
        }
    }

    #[tracing::instrument(skip_all)]
    async fn complete_active_thinking(&self, active_thinking: &mut Option<ThinkingSegmentState>) {
        let Some(segment) = active_thinking.take() else {
            return;
        };
        let duration_ms = (now_ms() - segment.started_at).max(0);
        self.send_thinking_done(&segment.id, duration_ms as u64);
        if segment.buffer.is_empty() {
            return;
        }
        let content = json!({
            "content": segment.buffer,
            "status": "done",
            "duration_ms": duration_ms,
        })
        .to_string();
        let row = MessageRow {
            id: segment.id.clone(),
            conversation_id: self.conv_id(),
            msg_id: Some(segment.id),
            r#type: "thinking".into(),
            content,
            position: Some("left".into()),
            status: Some("finish".into()),
            hidden: false,
            created_at: segment.started_at,
        };
        if let Err(e) = self.repo.insert_message(&row).await {
            error!(error = %ErrorChain(&e), "Failed to persist thinking message");
        }
    }

    #[tracing::instrument(skip_all)]
    async fn close_active_text_segment(
        &self,
        active_text: &mut Option<TextSegmentState>,
        text_segments: &mut Vec<PersistedTextSegment>,
        status: &str,
    ) {
        let Some(text_segment) = active_text.take() else {
            return;
        };
        if let Some(segment) = self.finalize_text_segment(text_segment, status).await {
            text_segments.push(segment);
        }
    }

    /// Persist a Gemini-style tool_call event.
    #[tracing::instrument(skip_all)]
    async fn persist_tool_call(&self, data: &nomifun_ai_agent::protocol::events::tool_call::ToolCallEventData) {
        if data.call_id.trim().is_empty() {
            warn!(
                tool = %data.name,
                status = ?data.status,
                "Skipping tool_call persistence because call_id is empty"
            );
            return;
        }

        let status = match data.status {
            ToolCallStatus::Running => "work",
            ToolCallStatus::Completed => "finish",
            ToolCallStatus::Error => "error",
        };
        let content = serde_json::to_string(data).unwrap_or_default();

        let existing = self
            .repo
            .get_message_by_msg_id(self.conv_id(), &data.call_id, "tool_call")
            .await
            .unwrap_or(None);

        if let Some(existing_row) = existing {
            let merged_content = Self::merge_json_content(&existing_row.content, &content);
            let update = nomifun_db::MessageRowUpdate {
                content: Some(merged_content),
                status: Some(Some(status.to_owned())),
                hidden: None,
            };
            if let Err(e) = self.repo.update_message(&data.call_id, &update).await {
                error!(
                    call_id = %data.call_id,
                    tool = %data.name,
                    status,
                    error = %ErrorChain(&e),
                    "Failed to update tool_call message"
                );
            } else {
                debug!(
                    call_id = %data.call_id,
                    tool = %data.name,
                    status,
                    "Updated tool_call message"
                );
            }
        } else {
            let row = MessageRow {
                id: data.call_id.clone(),
                conversation_id: self.conv_id(),
                msg_id: Some(data.call_id.clone()),
                r#type: "tool_call".into(),
                content,
                position: Some("left".into()),
                status: Some(status.to_owned()),
                hidden: false,
                created_at: now_ms(),
            };
            if let Err(e) = self.repo.insert_message(&row).await {
                error!(
                    call_id = %data.call_id,
                    tool = %data.name,
                    status,
                    error = %ErrorChain(&e),
                    "Failed to persist tool_call message"
                );
            } else {
                debug!(
                    call_id = %data.call_id,
                    tool = %data.name,
                    status,
                    "Persisted tool_call message"
                );
            }
        }
    }

    fn incomplete_tool_reason(event: &AgentStreamEvent) -> Option<&'static str> {
        match event {
            AgentStreamEvent::Error(_) => Some("error"),
            AgentStreamEvent::Finish(data) => match data.stop_reason {
                Some(nomifun_ai_agent::protocol::events::TurnStopReason::MaxTokens) => Some("max_tokens"),
                Some(nomifun_ai_agent::protocol::events::TurnStopReason::MaxTurnRequests) => {
                    Some("max_turn_requests")
                }
                Some(nomifun_ai_agent::protocol::events::TurnStopReason::Refusal) => Some("refusal"),
                Some(nomifun_ai_agent::protocol::events::TurnStopReason::Cancelled) => Some("cancelled"),
                Some(nomifun_ai_agent::protocol::events::TurnStopReason::EndTurn) => Some("end_turn"),
                None => Some("finish"),
            },
            _ => None,
        }
    }

    async fn fail_active_tool_calls(
        &self,
        active_tool_calls: &mut HashMap<String, ToolCallEventData>,
        reason: &str,
    ) {
        if active_tool_calls.is_empty() {
            return;
        }

        let output = format!("The turn ended before this tool completed: {reason}");
        let failed: Vec<ToolCallEventData> = active_tool_calls
            .drain()
            .map(|(_, mut data)| {
                data.status = ToolCallStatus::Error;
                data.output = Some(output.clone());
                data
            })
            .collect();

        for data in failed {
            let event = AgentStreamEvent::ToolCall(data);
            self.forward_to_websocket(&event);
            if let AgentStreamEvent::ToolCall(data) = &event {
                self.persist_tool_call(data).await;
            }
        }
    }

    /// Persist an ACP (Claude CLI) tool call event.
    /// First event (ToolCall) inserts; subsequent events (ToolCallUpdate) update.
    #[tracing::instrument(skip_all)]
    async fn persist_acp_tool_call(&self, data: &nomifun_ai_agent::protocol::events::tool_call::AcpToolCallEventData) {
        let tool_call_id = &data.update.tool_call_id;
        let status = match data.update.status {
            Some(AcpToolCallStatus::Pending) | None => "work",
            Some(AcpToolCallStatus::InProgress) => "work",
            Some(AcpToolCallStatus::Completed) => "finish",
            Some(AcpToolCallStatus::Failed) => "error",
        };

        let mut value = serde_json::to_value(data).unwrap_or_default();
        normalize_keys_to_snake_case(&mut value);
        let content = value.to_string();

        match data.update.session_update {
            AcpToolCallSessionUpdateKind::ToolCall => {
                let row = MessageRow {
                    id: tool_call_id.clone(),
                    conversation_id: self.conv_id(),
                    msg_id: Some(tool_call_id.clone()),
                    r#type: "acp_tool_call".into(),
                    content,
                    position: Some("left".into()),
                    status: Some(status.to_owned()),
                    hidden: false,
                    created_at: now_ms(),
                };
                if let Err(e) = self.repo.insert_message(&row).await {
                    error!(error = %ErrorChain(&e), "Failed to persist acp_tool_call message");
                }
            }
            AcpToolCallSessionUpdateKind::ToolCallUpdate => {
                let merged_content = self.merge_acp_tool_call_content(tool_call_id, &value).await;
                let update = nomifun_db::MessageRowUpdate {
                    content: Some(merged_content),
                    status: Some(Some(status.to_owned())),
                    hidden: None,
                };
                if let Err(e) = self.repo.update_message(tool_call_id, &update).await {
                    error!(error = %ErrorChain(&e), "Failed to update acp_tool_call message");
                }
            }
        }
    }

    /// Merge two JSON content strings: overlays non-null fields from `new_json`
    /// onto `existing_json`, preserving fields only present in the original.
    fn merge_json_content(existing_json: &str, new_json: &str) -> String {
        let mut base: serde_json::Value = serde_json::from_str(existing_json).unwrap_or_default();
        let new_value: serde_json::Value = serde_json::from_str(new_json).unwrap_or_default();
        if let (Some(base_obj), Some(new_obj)) = (base.as_object_mut(), new_value.as_object()) {
            for (key, val) in new_obj {
                if !val.is_null() {
                    base_obj.insert(key.clone(), val.clone());
                }
            }
        }
        base.to_string()
    }

    /// Merge an AcpToolCall update into the existing DB record.
    /// Reads the stored content, overlays non-null fields from the update,
    /// preserving fields like `raw_input` that the update event omits.
    async fn merge_acp_tool_call_content(&self, tool_call_id: &str, update_value: &serde_json::Value) -> String {
        let existing = self
            .repo
            .get_message_by_msg_id(self.conv_id(), tool_call_id, "acp_tool_call")
            .await
            .ok()
            .flatten();

        let Some(existing_row) = existing else {
            return update_value.to_string();
        };

        let mut base: serde_json::Value = serde_json::from_str(&existing_row.content).unwrap_or_default();
        if let (Some(base_update), Some(new_update)) = (
            base.get_mut("update").and_then(|v| v.as_object_mut()),
            update_value.get("update").and_then(|v| v.as_object()),
        ) {
            for (key, val) in new_update {
                if !val.is_null() {
                    base_update.insert(key.clone(), val.clone());
                }
            }
        }
        base.to_string()
    }

    /// Persist a tool_group event (array of tool summaries).
    #[tracing::instrument(skip_all)]
    async fn persist_tool_group(&self, entries: &[nomifun_ai_agent::protocol::events::tool_call::ToolGroupEntry]) {
        let all_done = entries
            .iter()
            .all(|e| matches!(e.status, ToolCallStatus::Completed | ToolCallStatus::Error));
        let status = if all_done { "finish" } else { "work" };
        let content = serde_json::to_string(entries).unwrap_or_default();

        let group_id = entries
            .first()
            .map(|e| e.call_id.clone())
            .unwrap_or_else(ConversationService::mint_msg_id);

        let existing = self
            .repo
            .get_message_by_msg_id(self.conv_id(), &group_id, "tool_group")
            .await
            .unwrap_or(None);

        if existing.is_some() {
            let update = nomifun_db::MessageRowUpdate {
                content: Some(content),
                status: Some(Some(status.to_owned())),
                hidden: None,
            };
            if let Err(e) = self.repo.update_message(&group_id, &update).await {
                error!(error = %ErrorChain(&e), "Failed to update tool_group message");
            }
        } else {
            let row = MessageRow {
                id: group_id.clone(),
                conversation_id: self.conv_id(),
                msg_id: Some(group_id),
                r#type: "tool_group".into(),
                content,
                position: Some("left".into()),
                status: Some(status.to_owned()),
                hidden: false,
                created_at: now_ms(),
            };
            if let Err(e) = self.repo.insert_message(&row).await {
                error!(error = %ErrorChain(&e), "Failed to persist tool_group message");
            }
        }
    }

    /// Send a `thinking` event with `status: "done"` to close the thinking UI.
    fn send_thinking_done(&self, msg_id: &str, duration: u64) {
        let thinking_done = AgentStreamEvent::Thinking(ThinkingEventData {
            content: String::new(),
            subject: None,
            duration: Some(duration),
            status: Some("done".into()),
        });
        self.forward_to_websocket_with_msg_id(msg_id, &thinking_done);
    }

    async fn process_final_text(&self, text: &str) -> MiddlewareResult {
        let middleware = MessageMiddleware::new(
            self.cron_service
                .as_ref()
                .map(|service| Box::new(SharedCronService(Arc::clone(service))) as Box<dyn ICronService>),
        );

        middleware.process(text, &self.user_id, &self.conversation_id).await
    }

    fn send_final_text_override(&self, msg_id: &str, text: &str, hidden: bool) {
        self.broadcast_stream_payload(json!({
            "conversation_id": self.conv_id(),
            "msg_id": msg_id,
            "type": "content",
            "data": { "content": text },
            "hidden": hidden,
            "replace": true,
        }));
    }

    fn send_system_responses(&self, responses: &[String]) {
        for response in responses {
            self.broadcast_stream_payload(json!({
                "conversation_id": self.conv_id(),
                "msg_id": ConversationService::mint_msg_id(),
                "type": "system",
                "data": response,
                "hidden": true,
            }));
        }
    }

    fn broadcast_stream_payload(&self, mut payload: serde_json::Value) {
        // Stamp the companion-companion + origin markers on every stream fragment
        // (the websocket consumers tolerate unknown fields; the companion collector
        // keys off them).
        if let Some(obj) = payload.as_object_mut() {
            obj.insert("companion".into(), json!(self.companion));
            obj.insert("companion_id".into(), json!(self.companion_id));
            obj.insert("origin".into(), json!(self.origin));
            obj.insert("channel_platform".into(), json!(self.channel_platform));
        }
        let msg = WebSocketMessage::new("message.stream", payload);
        self.broadcaster.broadcast(msg);
    }

    /// Emit `turn.completed` for the conversation, with the companion-companion
    /// wire markers and the turn's `origin` marker attached to the
    /// `turn.completed` payload (see [`Self::with_companion_context`] /
    /// [`Self::with_origin`]).
    #[tracing::instrument(skip_all, fields(conversation_id = %conversation_id))]
    pub async fn complete_conversation_with_context(
        repo: &Arc<dyn IConversationRepository>,
        broadcaster: &Arc<dyn EventBroadcaster>,
        conversation_id: &str,
        runtime: Option<ConversationRuntimeSummary>,
        companion: bool,
        companion_id: Option<String>,
        origin: Option<String>,
        channel_platform: Option<String>,
    ) {
        // The public id stays the String form; the repo + the i64 DTO fields on
        // the `turn.completed` payload use the integer key. A relay only ever
        // carries a numeric conversation id, so a parse failure here is a
        // programming error and degrades to `0` rather than failing the turn.
        let conv_id: i64 = conversation_id.parse().unwrap_or_default();
        let update = nomifun_db::ConversationRowUpdate {
            status: Some("finished".to_owned()),
            updated_at: Some(now_ms()),
            ..Default::default()
        };
        if let Err(e) = repo.update(conv_id, &update).await {
            error!(error = %ErrorChain(&e), "Failed to update conversation status");
        }

        let payload = json!({
            "conversation_id": conv_id,
            "session_id": conv_id,
            "status": "finished",
            "canSendMessage": true,
            "runtime": runtime,
            "companion": companion,
            "companion_id": companion_id,
            "origin": origin,
            "channel_platform": channel_platform,
        });
        let msg = WebSocketMessage::new("turn.completed", payload);
        broadcaster.broadcast(msg);

        debug!(conversation_id, status = "finished", "Turn completed");
    }
}

struct SharedCronService(Arc<dyn ICronService>);

#[async_trait::async_trait]
impl ICronService for SharedCronService {
    async fn create_job(
        &self,
        user_id: &str,
        conversation_id: &str,
        params: &crate::response_middleware::CronCreateParams,
    ) -> crate::response_middleware::CronCommandResult {
        self.0.create_job(user_id, conversation_id, params).await
    }

    async fn update_job(
        &self,
        user_id: &str,
        conversation_id: &str,
        params: &crate::response_middleware::CronUpdateParams,
    ) -> crate::response_middleware::CronCommandResult {
        self.0.update_job(user_id, conversation_id, params).await
    }

    async fn list_jobs(&self, user_id: &str, conversation_id: &str) -> crate::response_middleware::CronCommandResult {
        self.0.list_jobs(user_id, conversation_id).await
    }

    async fn delete_job(&self, user_id: &str, job_id: &str) -> crate::response_middleware::CronCommandResult {
        self.0.delete_job(user_id, job_id).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nomifun_ai_agent::protocol::events::{
        ErrorEventData, FinishEventData, PlanEventData, TextEventData, ThinkingEventData,
    };
    use nomifun_db::DbError;
    use std::sync::Mutex;

    // ── run() async tests ─────────────────────────────────────────

    #[tokio::test]
    async fn run_text_then_finish_persists_message() {
        let repo = Arc::new(RecordingRepo::new());
        let bus = Arc::new(nomifun_realtime::BroadcastEventBus::new(64));
        let (tx, _) = broadcast::channel(64);

        let relay = StreamRelay::new(
            "1".into(),
            "asst-1".into(),
            "user-1".into(),
            repo.clone(),
            bus.clone(),
            None,
        );

        let rx = tx.subscribe();

        // Send text events then finish
        tx.send(AgentStreamEvent::Text(TextEventData {
            content: "Hello ".into(),
        }))
        .unwrap();
        tx.send(AgentStreamEvent::Text(TextEventData {
            content: "World".into(),
        }))
        .unwrap();
        tx.send(AgentStreamEvent::Finish(FinishEventData::default())).unwrap();

        let outcome = relay.consume(rx).await;
        assert!(outcome.system_responses.is_empty());
        assert_eq!(outcome.terminal, RelayTerminal::Finish);
        // Plan D4: a turn that streamed Text is not pre-response.
        assert!(outcome.emitted_response);

        // Should have inserted a message with accumulated text
        let inserts = repo.take_inserts();
        assert_eq!(inserts.len(), 1);
        let msg = &inserts[0];
        assert_eq!(msg.conversation_id, 1);
        assert_eq!(msg.id, "asst-1");
        assert_eq!(msg.r#type, "text");
        assert_eq!(msg.status.as_deref(), Some("finish"));

        let content: serde_json::Value = serde_json::from_str(&msg.content).unwrap();
        assert_eq!(content["content"], "Hello World");
    }

    // UC-2b: a relay wired with runtime state accumulates the TurnCompleted token
    // usage (input + output) into the conversation's running total — the seam the
    // orchestrator worker reads after settle to fill `orch_run_tasks.tokens`.
    #[tokio::test]
    async fn turn_completed_accumulates_tokens_into_wired_runtime_state() {
        use nomifun_ai_agent::protocol::events::TurnCompletedEventData;

        let repo = Arc::new(RecordingRepo::new());
        let bus = Arc::new(nomifun_realtime::BroadcastEventBus::new(64));
        let (tx, _) = broadcast::channel(64);
        let runtime_state = Arc::new(ConversationRuntimeStateService::default());

        let relay = StreamRelay::new("42".into(), "asst-1".into(), "user-1".into(), repo, bus, None)
            .with_runtime_state(runtime_state.clone());
        let rx = tx.subscribe();

        // Two TurnCompleted events (e.g. a continuation) then Finish.
        tx.send(AgentStreamEvent::TurnCompleted(TurnCompletedEventData {
            input_tokens: 100,
            output_tokens: 40,
            ..Default::default()
        }))
        .unwrap();
        tx.send(AgentStreamEvent::TurnCompleted(TurnCompletedEventData {
            input_tokens: 30,
            output_tokens: 10,
            ..Default::default()
        }))
        .unwrap();
        tx.send(AgentStreamEvent::Finish(FinishEventData::default())).unwrap();

        let _ = relay.consume(rx).await;

        // (100+40) + (30+10) = 180, keyed by the relay's conversation id.
        assert_eq!(runtime_state.take_turn_tokens("42"), Some(180));
    }

    // Zero-regression: a relay WITHOUT runtime state wired (the default chat path)
    // records nothing — no accumulator entry for the conversation.
    #[tokio::test]
    async fn turn_completed_without_runtime_state_records_nothing() {
        use nomifun_ai_agent::protocol::events::TurnCompletedEventData;

        let repo = Arc::new(RecordingRepo::new());
        let bus = Arc::new(nomifun_realtime::BroadcastEventBus::new(64));
        let (tx, _) = broadcast::channel(64);
        let observer = Arc::new(ConversationRuntimeStateService::default());

        let relay = StreamRelay::new("42".into(), "asst-1".into(), "user-1".into(), repo, bus, None);
        let rx = tx.subscribe();

        tx.send(AgentStreamEvent::TurnCompleted(TurnCompletedEventData {
            input_tokens: 999,
            output_tokens: 1,
            ..Default::default()
        }))
        .unwrap();
        tx.send(AgentStreamEvent::Finish(FinishEventData::default())).unwrap();

        let _ = relay.consume(rx).await;

        // The relay was never given this runtime state, so it cannot have written.
        assert_eq!(observer.take_turn_tokens("42"), None);
    }

    #[tokio::test]
    async fn run_plan_event_persists_message_for_history_reload() {
        let repo = Arc::new(RecordingRepo::new());
        let bus = Arc::new(nomifun_realtime::BroadcastEventBus::new(64));
        let (tx, _) = broadcast::channel(64);

        let relay = StreamRelay::new(
            "1".into(),
            "asst-1".into(),
            "user-1".into(),
            repo.clone(),
            bus.clone(),
            None,
        );
        let rx = tx.subscribe();

        tx.send(AgentStreamEvent::Plan(PlanEventData {
            session_id: Some("session-1".into()),
            entries: vec![
                json!({ "content": "Inspect current renderer path", "status": "completed" }),
                json!({ "content": "Persist plan rows", "status": "in_progress" }),
            ],
        }))
        .unwrap();
        tx.send(AgentStreamEvent::Finish(FinishEventData::default())).unwrap();

        let outcome = relay.consume(rx).await;

        let inserts = repo.take_inserts();
        let plan_msg = inserts.iter().find(|m| m.r#type == "plan").expect("plan message must be persisted");
        assert_eq!(plan_msg.id, "asst-1:plan:session-1");
        assert_eq!(plan_msg.msg_id.as_deref(), Some("asst-1:plan:session-1"));
        assert_eq!(plan_msg.status.as_deref(), Some("work"));

        let content: serde_json::Value = serde_json::from_str(&plan_msg.content).unwrap();
        assert_eq!(content["session_id"], "session-1");
        assert_eq!(content["entries"].as_array().unwrap().len(), 2);
        assert_eq!(content["entries"][1]["status"], "in_progress");
        assert!(outcome.emitted_response);
    }

    #[tokio::test]
    async fn run_text_tool_text_splits_text_segments() {
        use nomifun_ai_agent::protocol::events::tool_call::{ToolCallEventData, ToolCallStatus};

        let repo = Arc::new(RecordingRepo::new());
        let bus = Arc::new(nomifun_realtime::BroadcastEventBus::new(64));
        let (tx, _) = broadcast::channel(64);

        let relay = StreamRelay::new(
            "1".into(),
            "asst-1".into(),
            "user-1".into(),
            repo.clone(),
            bus.clone(),
            None,
        );

        let mut ws_rx = bus.subscribe();
        let rx = tx.subscribe();

        tx.send(AgentStreamEvent::Text(TextEventData {
            content: "Alpha".into(),
        }))
        .unwrap();
        tx.send(AgentStreamEvent::ToolCall(ToolCallEventData {
            call_id: "tc-001".into(),
            name: "read_file".into(),
            args: json!({"path": "a.ts"}),
            status: ToolCallStatus::Running,
            description: None,
            input: None,
            output: None,
        }))
        .unwrap();
        tx.send(AgentStreamEvent::Text(TextEventData { content: "Beta".into() }))
            .unwrap();
        tx.send(AgentStreamEvent::Finish(FinishEventData::default())).unwrap();

        relay.consume(rx).await;

        let inserts = repo.take_inserts();
        let text_msgs: Vec<_> = inserts.iter().filter(|msg| msg.r#type == "text").collect();
        assert_eq!(text_msgs.len(), 2, "text should split across tool boundaries");
        assert_eq!(text_msgs[0].id, "asst-1");
        assert_ne!(text_msgs[0].id, text_msgs[1].id);

        let mut text_event_msg_ids = Vec::new();
        while let Ok(evt) = ws_rx.try_recv() {
            if evt.name == "message.stream" && (evt.data["type"] == "text" || evt.data["type"] == "content") {
                text_event_msg_ids.push(evt.data["msg_id"].as_str().unwrap_or_default().to_owned());
            }
        }
        assert_eq!(text_event_msg_ids.len(), 2);
        assert_eq!(text_event_msg_ids[0], "asst-1");
        assert_ne!(text_event_msg_ids[0], text_event_msg_ids[1]);
    }

    #[tokio::test]
    async fn run_error_with_no_text_stores_tips_message() {
        let repo = Arc::new(RecordingRepo::new());
        let bus = Arc::new(nomifun_realtime::BroadcastEventBus::new(64));
        let (tx, _) = broadcast::channel(64);

        let relay = StreamRelay::new(
            "1".into(),
            "asst-1".into(),
            "user-1".into(),
            repo.clone(),
            bus.clone(),
            None,
        );

        let rx = tx.subscribe();

        tx.send(AgentStreamEvent::Error(ErrorEventData::legacy(
            "Something went wrong",
            None,
        )))
        .unwrap();

        let outcome = relay.consume(rx).await;
        assert!(outcome.system_responses.is_empty());
        assert_eq!(
            outcome.terminal,
            RelayTerminal::Error {
                code: None,
                retryable: None
            }
        );
        // Plan D4: an error with no streamed Text is pre-response — the failover
        // seam is allowed to switch models on this kind of terminal error.
        assert!(!outcome.emitted_response);

        let inserts = repo.take_inserts();
        assert_eq!(inserts.len(), 1);
        let msg = &inserts[0];
        assert_eq!(msg.r#type, "tips");
        assert_eq!(msg.status.as_deref(), Some("error"));

        let content: serde_json::Value = serde_json::from_str(&msg.content).unwrap();
        assert_eq!(content["content"], "Something went wrong");
        assert_eq!(content["type"], "error");
    }

    #[tokio::test]
    async fn run_tool_call_then_error_is_post_response() {        // Plan D4 (review #4): a turn that forwarded/persisted a ToolCall and
        // THEN hit a provider fault must report `emitted_response == true`, so
        // the failover seam refuses to switch — re-running the turn would
        // re-execute the tool's side effect (and re-bill it).
        use nomifun_ai_agent::protocol::events::tool_call::{ToolCallEventData, ToolCallStatus};

        let repo = Arc::new(RecordingRepo::new());
        let bus = Arc::new(nomifun_realtime::BroadcastEventBus::new(64));
        let (tx, _) = broadcast::channel(64);

        let relay = StreamRelay::new(
            "1".into(),
            "asst-1".into(),
            "user-1".into(),
            repo.clone(),
            bus.clone(),
            None,
        );

        let rx = tx.subscribe();

        tx.send(AgentStreamEvent::ToolCall(ToolCallEventData {
            call_id: "tc-001".into(),
            name: "write_file".into(),
            args: json!({"path": "a.ts"}),
            status: ToolCallStatus::Completed,
            description: None,
            input: None,
            output: Some("ok".into()),
        }))
        .unwrap();
        tx.send(AgentStreamEvent::Error(ErrorEventData::legacy(
            "provider 503 after tool ran",
            None,
        )))
        .unwrap();

        let outcome = relay.consume(rx).await;
        assert!(outcome.terminal.is_error());
        // A tool action already ran this turn → NOT pre-response → never failed over.
        assert!(
            outcome.emitted_response,
            "a forwarded ToolCall must mark the turn as having emitted a response"
        );
    }

    #[tokio::test]
    async fn run_marks_active_tool_call_error_when_turn_hits_max_tokens() {
        use nomifun_ai_agent::protocol::events::tool_call::{ToolCallEventData, ToolCallStatus};
        use nomifun_ai_agent::protocol::events::TurnStopReason;

        let repo = Arc::new(RecordingRepo::new());
        let bus = Arc::new(nomifun_realtime::BroadcastEventBus::new(64));
        let (tx, _) = broadcast::channel(64);

        let relay = StreamRelay::new(
            "1".into(),
            "asst-1".into(),
            "user-1".into(),
            repo.clone(),
            bus.clone(),
            None,
        );

        let rx = tx.subscribe();

        tx.send(AgentStreamEvent::ToolCall(ToolCallEventData {
            call_id: "tc-write".into(),
            name: "Write".into(),
            args: json!({"file_path": "/tmp/index.html"}),
            status: ToolCallStatus::Running,
            description: None,
            input: Some(json!({"file_path": "/tmp/index.html"})),
            output: None,
        }))
        .unwrap();
        tx.send(AgentStreamEvent::Finish(FinishEventData {
            session_id: None,
            stop_reason: Some(TurnStopReason::MaxTokens),
        }))
        .unwrap();

        let outcome = relay.consume(rx).await;
        assert_eq!(outcome.terminal, RelayTerminal::Finish);

        let updates = repo.take_updates();
        let (_, update) = updates
            .iter()
            .find(|(id, _)| id == "tc-write")
            .expect("active tool call should be marked failed when the turn is truncated");
        assert_eq!(update.status.as_ref().map(|v| v.as_deref()), Some(Some("error")));
        let content: serde_json::Value = serde_json::from_str(update.content.as_deref().expect("updated content")).unwrap();
        assert_eq!(content["status"], "error");
        assert_eq!(content["output"], "The turn ended before this tool completed: max_tokens");
    }

    #[tokio::test]
    async fn run_suppresses_pre_response_error_when_failover_will_fire() {
        // review #1/#5: with a suppressor that accepts the fault's code, a
        // pre-response (no text) provider error must NOT broadcast a WS error
        // event NOR persist an error `tips` row — the user only ever sees the
        // backup model's turn. The swallowed event is handed back for re-surface.
        let repo = Arc::new(RecordingRepo::new());
        let bus = Arc::new(nomifun_realtime::BroadcastEventBus::new(64));
        let (tx, _) = broadcast::channel(64);

        let relay = StreamRelay::new(
            "1".into(),
            "asst-1".into(),
            "user-1".into(),
            repo.clone(),
            bus.clone(),
            None,
        )
        // Always-suppress predicate (the send loop passes is_provider_fault).
        .with_failover_suppressor(Arc::new(|_code| true));

        let mut ws_rx = bus.subscribe();
        let rx = tx.subscribe();
        tx.send(AgentStreamEvent::Error(ErrorEventData::legacy(
            "provider 503 pre-response",
            Some(nomifun_api_types::AgentErrorCode::UserLlmProviderGatewayError),
        )))
        .unwrap();

        let outcome = relay.consume(rx).await;
        assert!(outcome.terminal.is_error());
        // No error tips row persisted.
        let inserts = repo.take_inserts();
        assert!(
            !inserts.iter().any(|m| m.r#type == "tips"),
            "a suppressed pre-response error must not persist a tips row"
        );
        // No WS error event broadcast.
        let mut ws_events = vec![];
        while let Ok(evt) = ws_rx.try_recv() {
            ws_events.push(evt);
        }
        assert!(
            !ws_events
                .iter()
                .any(|evt| evt.name == "message.stream" && evt.data["type"] == "error"),
            "a suppressed pre-response error must not broadcast a WS error event"
        );
        // The swallowed event is handed back so the loop can re-surface on a miss.
        assert!(matches!(outcome.suppressed_error, Some(AgentStreamEvent::Error(_))));
    }

    #[tokio::test]
    async fn run_does_not_suppress_when_response_already_emitted() {
        // The suppressor must NOT fire post-response: a Text then a fault keeps
        // the error visible (failover would duplicate the streamed text).
        let repo = Arc::new(RecordingRepo::new());
        let bus = Arc::new(nomifun_realtime::BroadcastEventBus::new(64));
        let (tx, _) = broadcast::channel(64);

        let relay = StreamRelay::new(
            "1".into(),
            "asst-1".into(),
            "user-1".into(),
            repo.clone(),
            bus.clone(),
            None,
        )
        .with_failover_suppressor(Arc::new(|_code| true));

        let rx = tx.subscribe();
        tx.send(AgentStreamEvent::Text(TextEventData { content: "partial".into() }))
            .unwrap();
        tx.send(AgentStreamEvent::Error(ErrorEventData::legacy("fault after text", None)))
            .unwrap();

        let outcome = relay.consume(rx).await;
        assert!(outcome.emitted_response);
        assert!(
            outcome.suppressed_error.is_none(),
            "a post-response fault must never be suppressed"
        );
    }

    #[tokio::test]
    async fn run_send_error_injects_error_and_completes_turn() {
        let repo = Arc::new(RecordingRepo::new());
        let bus = Arc::new(nomifun_realtime::BroadcastEventBus::new(64));
        let (tx, _) = broadcast::channel(64);

        let relay = StreamRelay::new(
            "1".into(),
            "asst-1".into(),
            "user-1".into(),
            repo.clone(),
            bus.clone(),
            None,
        );

        let mut ws_rx = bus.subscribe();
        let rx = tx.subscribe();
        let (send_error_tx, send_error_rx) = tokio::sync::oneshot::channel();
        send_error_tx
            .send(AgentSendError::from_app_error(nomifun_common::AppError::BadGateway(
                "provider returned 401 invalid api key".into(),
            )))
            .unwrap();

        let outcome = relay.consume_with_send_error(rx, send_error_rx).await;
        assert!(outcome.system_responses.is_empty());
        assert_eq!(
            outcome.terminal,
            RelayTerminal::Error {
                code: Some(nomifun_api_types::AgentErrorCode::UserLlmProviderAuthFailed),
                retryable: Some(false)
            }
        );

        let inserts = repo.take_inserts();
        assert_eq!(inserts.len(), 1);
        assert_eq!(inserts[0].r#type, "tips");
        assert_eq!(inserts[0].status.as_deref(), Some("error"));
        let content: serde_json::Value = serde_json::from_str(&inserts[0].content).unwrap();
        assert_eq!(content["content"], "The model provider rejected the request");
        assert_eq!(content["type"], "error");
        assert_eq!(content["error"]["code"], "USER_LLM_PROVIDER_AUTH_FAILED");
        assert_eq!(content["error"]["ownership"], "user_llm_provider");
        assert_eq!(content["error"]["retryable"], false);
        assert_eq!(content["error"]["feedback_recommended"], false);
        assert_eq!(content["error"]["detail"], "provider returned 401 invalid api key");
        assert_eq!(content["error"]["resolution"]["kind"], "check_provider_credentials");
        assert_eq!(content["error"]["resolution"]["target"], "provider_settings");

        let mut ws_events = vec![];
        while let Ok(evt) = ws_rx.try_recv() {
            ws_events.push(evt);
        }

        let error_event = ws_events
            .iter()
            .find(|evt| evt.name == "message.stream" && evt.data["type"] == "error")
            .expect("send error should be forwarded as message.stream error");
        assert_eq!(error_event.data["data"]["code"], "USER_LLM_PROVIDER_AUTH_FAILED");
        assert_eq!(error_event.data["data"]["ownership"], "user_llm_provider");
        assert!(ws_events.iter().any(|evt| evt.name == "turn.completed"));
    }

    #[tokio::test]
    async fn run_send_error_keeps_existing_stream_error_when_it_arrives_first() {
        let repo = Arc::new(RecordingRepo::new());
        let bus = Arc::new(nomifun_realtime::BroadcastEventBus::new(64));
        let (tx, _) = broadcast::channel(64);

        let relay = StreamRelay::new(
            "1".into(),
            "asst-1".into(),
            "user-1".into(),
            repo.clone(),
            bus.clone(),
            None,
        );

        let rx = tx.subscribe();
        let send_error = AgentSendError::from_app_error(nomifun_common::AppError::BadGateway(
            "provider returned 401 invalid api key".into(),
        ));
        tx.send(AgentStreamEvent::Error(ErrorEventData::legacy(
            "stream already emitted",
            None,
        )))
        .unwrap();
        let (send_error_tx, send_error_rx) = tokio::sync::oneshot::channel();
        let delayed_send_error = tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            let _ = send_error_tx.send(send_error);
        });

        let outcome = relay.consume_with_send_error(rx, send_error_rx).await;
        delayed_send_error.await.unwrap();
        assert!(outcome.system_responses.is_empty());

        let inserts = repo.take_inserts();
        assert_eq!(inserts.len(), 1);
        assert_eq!(inserts[0].r#type, "tips");
        let content: serde_json::Value = serde_json::from_str(&inserts[0].content).unwrap();
        assert_eq!(content["content"], "stream already emitted");
        assert_eq!(content["type"], "error");
    }

    #[tokio::test]
    async fn run_send_error_uses_send_error_when_it_arrives_first() {
        let repo = Arc::new(RecordingRepo::new());
        let bus = Arc::new(nomifun_realtime::BroadcastEventBus::new(64));
        let (tx, _) = broadcast::channel(64);

        let relay = StreamRelay::new(
            "1".into(),
            "asst-1".into(),
            "user-1".into(),
            repo.clone(),
            bus.clone(),
            None,
        );

        let rx = tx.subscribe();
        let (send_error_tx, send_error_rx) = tokio::sync::oneshot::channel();
        send_error_tx
            .send(AgentSendError::from_app_error(nomifun_common::AppError::BadGateway(
                "provider returned 401 invalid api key".into(),
            )))
            .unwrap();
        let delayed_stream_error = tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            let _ = tx.send(AgentStreamEvent::Error(ErrorEventData::legacy(
                "stream already emitted",
                None,
            )));
        });

        let outcome = relay.consume_with_send_error(rx, send_error_rx).await;
        delayed_stream_error.await.unwrap();
        assert!(outcome.system_responses.is_empty());
        assert_eq!(
            outcome.terminal,
            RelayTerminal::Error {
                code: Some(nomifun_api_types::AgentErrorCode::UserLlmProviderAuthFailed),
                retryable: Some(false)
            }
        );

        let inserts = repo.take_inserts();
        assert_eq!(inserts.len(), 1);
        assert_eq!(inserts[0].r#type, "tips");
        let content: serde_json::Value = serde_json::from_str(&inserts[0].content).unwrap();
        assert_eq!(content["content"], "The model provider rejected the request");
        assert_eq!(content["type"], "error");
        assert_eq!(content["error"]["resolution"]["kind"], "check_provider_credentials");
        assert_eq!(content["error"]["resolution"]["target"], "provider_settings");
    }

    #[tokio::test]
    async fn run_thinking_tool_thinking_splits_thinking_segments() {
        use nomifun_ai_agent::protocol::events::tool_call::{ToolCallEventData, ToolCallStatus};

        let repo = Arc::new(RecordingRepo::new());
        let bus = Arc::new(nomifun_realtime::BroadcastEventBus::new(64));
        let (tx, _) = broadcast::channel(64);

        let relay = StreamRelay::new(
            "1".into(),
            "asst-1".into(),
            "user-1".into(),
            repo.clone(),
            bus.clone(),
            None,
        );

        let mut ws_rx = bus.subscribe();
        let rx = tx.subscribe();

        tx.send(AgentStreamEvent::Thinking(ThinkingEventData {
            content: "Plan A".into(),
            subject: None,
            duration: None,
            status: Some("thinking".into()),
        }))
        .unwrap();
        tx.send(AgentStreamEvent::ToolCall(ToolCallEventData {
            call_id: "tc-001".into(),
            name: "read_file".into(),
            args: json!({"path": "a.ts"}),
            status: ToolCallStatus::Running,
            description: None,
            input: None,
            output: None,
        }))
        .unwrap();
        tx.send(AgentStreamEvent::Thinking(ThinkingEventData {
            content: "Plan B".into(),
            subject: None,
            duration: None,
            status: Some("thinking".into()),
        }))
        .unwrap();
        tx.send(AgentStreamEvent::Finish(FinishEventData::default())).unwrap();

        relay.consume(rx).await;

        let inserts = repo.take_inserts();
        let thinking_msgs: Vec<_> = inserts.iter().filter(|msg| msg.r#type == "thinking").collect();
        assert_eq!(thinking_msgs.len(), 2, "thinking should split across tool boundaries");
        assert_eq!(thinking_msgs[0].msg_id.as_deref(), Some("asst-1"));
        assert_ne!(thinking_msgs[0].msg_id, thinking_msgs[1].msg_id);

        let mut done_msg_ids = Vec::new();
        while let Ok(evt) = ws_rx.try_recv() {
            if evt.name == "message.stream" && evt.data["type"] == "thinking" && evt.data["data"]["status"] == "done" {
                done_msg_ids.push(evt.data["msg_id"].as_str().unwrap_or_default().to_owned());
            }
        }
        assert_eq!(done_msg_ids.len(), 2);
        assert_eq!(done_msg_ids[0], "asst-1");
        assert_ne!(done_msg_ids[0], done_msg_ids[1]);
    }

    #[tokio::test]
    async fn run_thinking_then_text_uses_distinct_segment_ids() {
        let repo = Arc::new(RecordingRepo::new());
        let bus = Arc::new(nomifun_realtime::BroadcastEventBus::new(64));
        let (tx, _) = broadcast::channel(64);

        let relay = StreamRelay::new(
            "1".into(),
            "asst-1".into(),
            "user-1".into(),
            repo.clone(),
            bus.clone(),
            None,
        );

        let mut ws_rx = bus.subscribe();
        let rx = tx.subscribe();

        tx.send(AgentStreamEvent::Thinking(ThinkingEventData {
            content: "Plan first".into(),
            subject: None,
            duration: None,
            status: Some("thinking".into()),
        }))
        .unwrap();
        tx.send(AgentStreamEvent::Text(TextEventData {
            content: "Final answer".into(),
        }))
        .unwrap();
        tx.send(AgentStreamEvent::Finish(FinishEventData::default())).unwrap();

        relay.consume(rx).await;

        let inserts = repo.take_inserts();
        let thinking_msgs: Vec<_> = inserts.iter().filter(|msg| msg.r#type == "thinking").collect();
        let text_msgs: Vec<_> = inserts.iter().filter(|msg| msg.r#type == "text").collect();

        assert_eq!(thinking_msgs.len(), 1);
        assert_eq!(text_msgs.len(), 1);
        assert_eq!(thinking_msgs[0].id, "asst-1");
        assert_ne!(thinking_msgs[0].id, text_msgs[0].id);

        let mut text_msg_ids = Vec::new();
        let mut thinking_done_ids = Vec::new();
        while let Ok(evt) = ws_rx.try_recv() {
            if evt.name != "message.stream" {
                continue;
            }
            if evt.data["type"] == "text" || evt.data["type"] == "content" {
                text_msg_ids.push(evt.data["msg_id"].as_str().unwrap_or_default().to_owned());
            }
            if evt.data["type"] == "thinking" && evt.data["data"]["status"] == "done" {
                thinking_done_ids.push(evt.data["msg_id"].as_str().unwrap_or_default().to_owned());
            }
        }

        assert_eq!(thinking_done_ids, vec!["asst-1".to_string()]);
        assert_eq!(text_msg_ids.len(), 1);
        assert_ne!(text_msg_ids[0], "asst-1");
    }

    #[tokio::test]
    async fn run_channel_closed_finalizes() {
        let repo = Arc::new(RecordingRepo::new());
        let bus = Arc::new(nomifun_realtime::BroadcastEventBus::new(64));
        let (tx, _) = broadcast::channel(64);

        let relay = StreamRelay::new(
            "1".into(),
            "asst-1".into(),
            "user-1".into(),
            repo.clone(),
            bus.clone(),
            None,
        );

        let rx = tx.subscribe();

        // Send text then drop sender (channel closes without Finish)
        tx.send(AgentStreamEvent::Text(TextEventData {
            content: "partial".into(),
        }))
        .unwrap();
        drop(tx);

        let outcome = relay.consume(rx).await;
        assert!(outcome.system_responses.is_empty());

        // Should still persist the partial text
        let inserts = repo.take_inserts();
        assert_eq!(inserts.len(), 1);
        let content: serde_json::Value = serde_json::from_str(&inserts[0].content).unwrap();
        assert_eq!(content["content"], "partial");
    }

    #[tokio::test]
    async fn run_broadcasts_turn_completed() {
        let repo = Arc::new(RecordingRepo::new());
        let bus = Arc::new(nomifun_realtime::BroadcastEventBus::new(64));
        let (tx, _) = broadcast::channel(64);

        let relay = StreamRelay::new(
            "1".into(),
            "asst-1".into(),
            "user-1".into(),
            repo.clone(),
            bus.clone(),
            None,
        );

        // Subscribe to the bus before relay runs
        let mut ws_rx = bus.subscribe();
        let rx = tx.subscribe();

        tx.send(AgentStreamEvent::Finish(FinishEventData::default())).unwrap();

        let outcome = relay.consume(rx).await;
        assert!(outcome.system_responses.is_empty());

        // Collect WebSocket events
        let mut ws_events = vec![];
        while let Ok(evt) = ws_rx.try_recv() {
            ws_events.push(evt);
        }

        // Should have turn.completed event
        let turn_event = ws_events.iter().find(|e| e.name == "turn.completed");
        assert!(turn_event.is_some());
        let data = &turn_event.unwrap().data;
        assert_eq!(data["conversation_id"], 1);
        assert_eq!(data["session_id"], 1);
        assert_eq!(data["status"], "finished");
        assert_eq!(data["canSendMessage"], true);
    }

    #[tokio::test]
    async fn run_with_companion_context_stamps_markers_on_stream_and_turn() {
        let repo = Arc::new(RecordingRepo::new());
        let bus = Arc::new(nomifun_realtime::BroadcastEventBus::new(64));
        let (tx, _) = broadcast::channel(64);

        let relay = StreamRelay::new(
            "2".into(),
            "asst-1".into(),
            "user-1".into(),
            repo.clone(),
            bus.clone(),
            None,
        )
        .with_companion_context(true, Some("companion_42".into()));

        let mut ws_rx = bus.subscribe();
        let rx = tx.subscribe();
        tx.send(AgentStreamEvent::Text(TextEventData { content: "喵".into() }))
            .unwrap();
        tx.send(AgentStreamEvent::Finish(FinishEventData::default())).unwrap();
        relay.consume(rx).await;

        let mut ws_events = vec![];
        while let Ok(evt) = ws_rx.try_recv() {
            ws_events.push(evt);
        }
        let stream_evt = ws_events
            .iter()
            .find(|e| e.name == "message.stream")
            .expect("stream event broadcast");
        assert_eq!(stream_evt.data["companion"], true);
        assert_eq!(stream_evt.data["companion_id"], "companion_42");
        let turn_evt = ws_events
            .iter()
            .find(|e| e.name == "turn.completed")
            .expect("turn.completed broadcast");
        assert_eq!(turn_evt.data["companion"], true);
        assert_eq!(turn_evt.data["companion_id"], "companion_42");
    }

    #[tokio::test]
    async fn run_with_channel_platform_stamps_platform_on_stream_and_turn() {
        let repo = Arc::new(RecordingRepo::new());
        let bus = Arc::new(nomifun_realtime::BroadcastEventBus::new(64));
        let (tx, _) = broadcast::channel(64);

        let relay = StreamRelay::new(
            "3".into(),
            "asst-1".into(),
            "user-1".into(),
            repo.clone(),
            bus.clone(),
            None,
        )
        .with_companion_context(true, Some("companion_42".into()))
        .with_channel_platform(Some("telegram".into()));

        let mut ws_rx = bus.subscribe();
        let rx = tx.subscribe();
        tx.send(AgentStreamEvent::Text(TextEventData { content: "喵".into() }))
            .unwrap();
        tx.send(AgentStreamEvent::Finish(FinishEventData::default())).unwrap();
        relay.consume(rx).await;

        let mut ws_events = vec![];
        while let Ok(evt) = ws_rx.try_recv() {
            ws_events.push(evt);
        }
        let stream_evt = ws_events
            .iter()
            .find(|e| e.name == "message.stream")
            .expect("stream event broadcast");
        assert_eq!(stream_evt.data["channel_platform"], "telegram");
        let turn_evt = ws_events
            .iter()
            .find(|e| e.name == "turn.completed")
            .expect("turn.completed broadcast");
        assert_eq!(turn_evt.data["channel_platform"], "telegram");
    }

    #[tokio::test]
    async fn run_with_blank_channel_platform_normalizes_to_null() {
        let repo = Arc::new(RecordingRepo::new());
        let bus = Arc::new(nomifun_realtime::BroadcastEventBus::new(64));
        let (tx, _) = broadcast::channel(64);

        let relay = StreamRelay::new(
            "1".into(),
            "asst-1".into(),
            "user-1".into(),
            repo.clone(),
            bus.clone(),
            None,
        )
        .with_channel_platform(Some("   ".into()));

        let mut ws_rx = bus.subscribe();
        let rx = tx.subscribe();
        tx.send(AgentStreamEvent::Text(TextEventData { content: "hi".into() }))
            .unwrap();
        tx.send(AgentStreamEvent::Finish(FinishEventData::default())).unwrap();
        relay.consume(rx).await;

        let mut ws_events = vec![];
        while let Ok(evt) = ws_rx.try_recv() {
            ws_events.push(evt);
        }
        let stream_evt = ws_events.iter().find(|e| e.name == "message.stream").unwrap();
        assert!(stream_evt.data["channel_platform"].is_null());
    }

    #[tokio::test]
    async fn run_without_companion_context_keeps_markers_off() {
        let repo = Arc::new(RecordingRepo::new());
        let bus = Arc::new(nomifun_realtime::BroadcastEventBus::new(64));
        let (tx, _) = broadcast::channel(64);

        let relay = StreamRelay::new(
            "1".into(),
            "asst-1".into(),
            "user-1".into(),
            repo.clone(),
            bus.clone(),
            None,
        );

        let mut ws_rx = bus.subscribe();
        let rx = tx.subscribe();
        tx.send(AgentStreamEvent::Text(TextEventData { content: "hi".into() }))
            .unwrap();
        tx.send(AgentStreamEvent::Finish(FinishEventData::default())).unwrap();
        relay.consume(rx).await;

        let mut ws_events = vec![];
        while let Ok(evt) = ws_rx.try_recv() {
            ws_events.push(evt);
        }
        let stream_evt = ws_events.iter().find(|e| e.name == "message.stream").unwrap();
        assert_eq!(stream_evt.data["companion"], false);
        assert!(stream_evt.data["companion_id"].is_null());
        assert!(stream_evt.data["channel_platform"].is_null());
        let turn_evt = ws_events.iter().find(|e| e.name == "turn.completed").unwrap();
        assert_eq!(turn_evt.data["companion"], false);
        assert!(turn_evt.data["companion_id"].is_null());
        assert!(turn_evt.data["channel_platform"].is_null());
    }

    #[tokio::test]
    async fn run_with_origin_stamps_origin_on_stream_and_turn() {
        let repo = Arc::new(RecordingRepo::new());
        let bus = Arc::new(nomifun_realtime::BroadcastEventBus::new(64));
        let (tx, _) = broadcast::channel(64);

        let relay = StreamRelay::new(
            "1".into(),
            "asst-1".into(),
            "user-1".into(),
            repo.clone(),
            bus.clone(),
            None,
        )
        .with_origin(Some("companion".into()));

        let mut ws_rx = bus.subscribe();
        let rx = tx.subscribe();
        tx.send(AgentStreamEvent::Text(TextEventData {
            content: "正在创建报表任务".into(),
        }))
        .unwrap();
        tx.send(AgentStreamEvent::Finish(FinishEventData::default())).unwrap();
        relay.consume(rx).await;

        let mut ws_events = vec![];
        while let Ok(evt) = ws_rx.try_recv() {
            ws_events.push(evt);
        }
        let stream_evt = ws_events
            .iter()
            .find(|e| e.name == "message.stream")
            .expect("stream event broadcast");
        assert_eq!(stream_evt.data["origin"], "companion");
        let turn_evt = ws_events
            .iter()
            .find(|e| e.name == "turn.completed")
            .expect("turn.completed broadcast");
        assert_eq!(turn_evt.data["origin"], "companion");
    }

    #[tokio::test]
    async fn run_without_origin_keeps_origin_null_and_blank_normalizes() {
        let repo = Arc::new(RecordingRepo::new());
        let bus = Arc::new(nomifun_realtime::BroadcastEventBus::new(64));
        let (tx, _) = broadcast::channel(64);

        // Blank origin must normalize to None (owner speech).
        let relay = StreamRelay::new(
            "1".into(),
            "asst-1".into(),
            "user-1".into(),
            repo.clone(),
            bus.clone(),
            None,
        )
        .with_origin(Some("   ".into()));

        let mut ws_rx = bus.subscribe();
        let rx = tx.subscribe();
        tx.send(AgentStreamEvent::Text(TextEventData { content: "hi".into() }))
            .unwrap();
        tx.send(AgentStreamEvent::Finish(FinishEventData::default())).unwrap();
        relay.consume(rx).await;

        let mut ws_events = vec![];
        while let Ok(evt) = ws_rx.try_recv() {
            ws_events.push(evt);
        }
        let stream_evt = ws_events.iter().find(|e| e.name == "message.stream").unwrap();
        assert!(stream_evt.data["origin"].is_null());
        let turn_evt = ws_events.iter().find(|e| e.name == "turn.completed").unwrap();
        assert!(turn_evt.data["origin"].is_null());
    }

    #[tokio::test]
    async fn run_finalizes_with_cleaned_replacement_event() {
        let repo = Arc::new(RecordingRepo::new());
        let bus = Arc::new(nomifun_realtime::BroadcastEventBus::new(64));
        let (tx, _) = broadcast::channel(64);
        let relay = StreamRelay::new(
            "1".into(),
            "asst-1".into(),
            "user-1".into(),
            repo.clone(),
            bus.clone(),
            Some(Arc::new(MockCronService)),
        );

        let mut ws_rx = bus.subscribe();
        let rx = tx.subscribe();
        tx.send(AgentStreamEvent::Text(TextEventData {
            content: "Hello [CRON_LIST]".into(),
        }))
        .unwrap();
        tx.send(AgentStreamEvent::Finish(FinishEventData::default())).unwrap();

        let outcome = relay.consume(rx).await;
        assert_eq!(outcome.system_responses, vec!["[System: listed]".to_string()]);

        let inserts = repo.take_inserts();
        assert_eq!(inserts.len(), 1);
        let updates = repo.take_updates();
        let final_update = updates
            .iter()
            .find(|(id, update)| id == "asst-1" && update.content.is_some())
            .expect("expected cleaned final text update");
        let content: serde_json::Value = serde_json::from_str(final_update.1.content.as_deref().unwrap()).unwrap();
        assert_eq!(content["content"].as_str().map(str::trim), Some("Hello"));

        let mut ws_events = vec![];
        while let Ok(evt) = ws_rx.try_recv() {
            ws_events.push(evt);
        }

        let replacement = ws_events
            .iter()
            .find(|evt| evt.name == "message.stream" && evt.data["type"] == "content" && evt.data["replace"] == true);
        assert!(replacement.is_some());
        assert_eq!(
            replacement.unwrap().data["data"]["content"].as_str().map(str::trim),
            Some("Hello")
        );
    }

    // ── Tool persistence tests ────────────────────────────────────

    #[tokio::test]
    async fn run_tool_call_persists_message() {
        use nomifun_ai_agent::protocol::events::tool_call::{ToolCallEventData, ToolCallStatus};

        let repo = Arc::new(RecordingRepo::new());
        let bus = Arc::new(nomifun_realtime::BroadcastEventBus::new(64));
        let (tx, _) = broadcast::channel(64);

        let relay = StreamRelay::new(
            "1".into(),
            "asst-1".into(),
            "user-1".into(),
            repo.clone(),
            bus.clone(),
            None,
        );

        let rx = tx.subscribe();

        // First event: Running with input but no output
        tx.send(AgentStreamEvent::ToolCall(ToolCallEventData {
            call_id: "tc-001".into(),
            name: "image_gen".into(),
            args: json!({"prompt": "a cat"}),
            status: ToolCallStatus::Running,
            input: Some(json!({"prompt": "a cat", "size": "1024x1024"})),
            output: None,
            description: Some("Generate image".into()),
        }))
        .unwrap();
        // Second event: Completed with output but no input
        tx.send(AgentStreamEvent::ToolCall(ToolCallEventData {
            call_id: "tc-001".into(),
            name: "image_gen".into(),
            args: json!({"prompt": "a cat"}),
            status: ToolCallStatus::Completed,
            input: None,
            output: Some("image.png".into()),
            description: None,
        }))
        .unwrap();
        tx.send(AgentStreamEvent::Finish(FinishEventData::default())).unwrap();

        relay.consume(rx).await;

        let inserts = repo.take_inserts();
        let tool_msg = inserts.iter().find(|m| m.r#type == "tool_call");
        assert!(tool_msg.is_some());
        let msg = tool_msg.unwrap();
        assert_eq!(msg.id, "tc-001");
        assert_eq!(msg.status.as_deref(), Some("work"));

        let updates = repo.take_updates();
        let tool_update = updates.iter().find(|(id, _)| id == "tc-001");
        assert!(tool_update.is_some());
        let (_, upd) = tool_update.unwrap();
        assert_eq!(upd.status, Some(Some("finish".to_owned())));

        // Verify merge: input from first event preserved, output from second event added
        let merged: serde_json::Value = serde_json::from_str(upd.content.as_deref().unwrap()).unwrap();
        assert_eq!(merged["name"], "image_gen");
        assert_eq!(merged["status"], "completed");
        assert!(
            merged.get("input").is_some() && !merged["input"].is_null(),
            "input must be preserved after merge"
        );
        assert_eq!(merged["input"]["prompt"], "a cat");
        assert_eq!(merged["output"], "image.png");
        assert_eq!(merged["description"], "Generate image");
    }

    #[tokio::test]
    async fn run_acp_tool_call_inserts_then_updates() {
        use nomifun_ai_agent::protocol::events::tool_call::{
            AcpToolCallEventData, AcpToolCallSessionUpdateKind, AcpToolCallStatus, AcpToolCallUpdateData,
        };

        let repo = Arc::new(RecordingRepo::new());
        let bus = Arc::new(nomifun_realtime::BroadcastEventBus::new(64));
        let (tx, _) = broadcast::channel(64);

        let relay = StreamRelay::new(
            "1".into(),
            "asst-1".into(),
            "user-1".into(),
            repo.clone(),
            bus.clone(),
            None,
        );

        let rx = tx.subscribe();

        tx.send(AgentStreamEvent::AcpToolCall(AcpToolCallEventData {
            session_id: "sess-1".into(),
            update: AcpToolCallUpdateData {
                session_update: AcpToolCallSessionUpdateKind::ToolCall,
                tool_call_id: "atc-001".into(),
                status: Some(AcpToolCallStatus::InProgress),
                title: Some("Bash".into()),
                kind: None,
                raw_input: Some(json!({"command": "mv /tmp/a /tmp/b", "description": "Move file"})),
                raw_output: None,
                content: None,
                locations: None,
            },
            meta: None,
        }))
        .unwrap();

        tx.send(AgentStreamEvent::AcpToolCall(AcpToolCallEventData {
            session_id: "sess-1".into(),
            update: AcpToolCallUpdateData {
                session_update: AcpToolCallSessionUpdateKind::ToolCallUpdate,
                tool_call_id: "atc-001".into(),
                status: Some(AcpToolCallStatus::Completed),
                title: None,
                kind: None,
                raw_input: None,
                raw_output: Some(json!("Exit code: 0\nSTDOUT:\nSTDERR:")),
                content: None,
                locations: None,
            },
            meta: None,
        }))
        .unwrap();

        tx.send(AgentStreamEvent::Finish(FinishEventData::default())).unwrap();

        relay.consume(rx).await;

        let inserts = repo.take_inserts();
        let acp_msg = inserts.iter().find(|m| m.r#type == "acp_tool_call");
        assert!(acp_msg.is_some());
        let msg = acp_msg.unwrap();
        assert_eq!(msg.id, "atc-001");
        assert_eq!(msg.status.as_deref(), Some("work"));

        let updates = repo.take_updates();
        let acp_update = updates.iter().find(|(id, _)| id == "atc-001");
        assert!(acp_update.is_some());
        let (_, upd) = acp_update.unwrap();
        assert_eq!(upd.status, Some(Some("finish".to_owned())));

        // Verify merge: raw_input from ToolCall is preserved, raw_output from ToolCallUpdate is added
        let merged: serde_json::Value = serde_json::from_str(upd.content.as_deref().unwrap()).unwrap();
        let update_obj = merged.get("update").unwrap();
        assert!(
            update_obj.get("raw_input").is_some(),
            "raw_input must be preserved after merge"
        );
        assert_eq!(
            update_obj
                .get("raw_input")
                .unwrap()
                .get("command")
                .unwrap()
                .as_str()
                .unwrap(),
            "mv /tmp/a /tmp/b"
        );
        assert!(
            update_obj.get("raw_output").is_some(),
            "raw_output must be present after merge"
        );
    }

    #[tokio::test]
    async fn run_tool_group_persists_message() {
        use nomifun_ai_agent::protocol::events::tool_call::{ToolCallStatus, ToolGroupEntry};

        let repo = Arc::new(RecordingRepo::new());
        let bus = Arc::new(nomifun_realtime::BroadcastEventBus::new(64));
        let (tx, _) = broadcast::channel(64);

        let relay = StreamRelay::new(
            "1".into(),
            "asst-1".into(),
            "user-1".into(),
            repo.clone(),
            bus.clone(),
            None,
        );

        let rx = tx.subscribe();

        tx.send(AgentStreamEvent::ToolGroup(vec![
            ToolGroupEntry {
                call_id: "tg-001".into(),
                name: "search".into(),
                status: ToolCallStatus::Completed,
                description: Some("Web search".into()),
            },
            ToolGroupEntry {
                call_id: "tg-002".into(),
                name: "read_file".into(),
                status: ToolCallStatus::Completed,
                description: None,
            },
        ]))
        .unwrap();
        tx.send(AgentStreamEvent::Finish(FinishEventData::default())).unwrap();

        relay.consume(rx).await;

        let inserts = repo.take_inserts();
        let group_msg = inserts.iter().find(|m| m.r#type == "tool_group");
        assert!(group_msg.is_some());
        let msg = group_msg.unwrap();
        assert_eq!(msg.id, "tg-001");
        assert_eq!(msg.status.as_deref(), Some("finish"));

        let content: serde_json::Value = serde_json::from_str(&msg.content).unwrap();
        assert!(content.is_array());
        assert_eq!(content.as_array().unwrap().len(), 2);
    }

    // ── Helpers ──────────────────────────────────────────────────

    struct MockCronService;

    #[async_trait::async_trait]
    impl ICronService for MockCronService {
        async fn create_job(
            &self,
            _user_id: &str,
            _conversation_id: &str,
            _params: &crate::response_middleware::CronCreateParams,
        ) -> crate::response_middleware::CronCommandResult {
            crate::response_middleware::CronCommandResult {
                success: true,
                message: "created".into(),
            }
        }

        async fn update_job(
            &self,
            _user_id: &str,
            _conversation_id: &str,
            _params: &crate::response_middleware::CronUpdateParams,
        ) -> crate::response_middleware::CronCommandResult {
            crate::response_middleware::CronCommandResult {
                success: true,
                message: "updated".into(),
            }
        }

        async fn list_jobs(
            &self,
            _user_id: &str,
            _conversation_id: &str,
        ) -> crate::response_middleware::CronCommandResult {
            crate::response_middleware::CronCommandResult {
                success: true,
                message: "listed".into(),
            }
        }

        async fn delete_job(&self, _user_id: &str, _job_id: &str) -> crate::response_middleware::CronCommandResult {
            crate::response_middleware::CronCommandResult {
                success: true,
                message: "deleted".into(),
            }
        }
    }

    /// Recording repo that captures insert/update calls for assertions.
    struct RecordingRepo {
        inserts: Mutex<Vec<MessageRow>>,
        updates: Mutex<Vec<(String, nomifun_db::MessageRowUpdate)>>,
    }

    impl RecordingRepo {
        fn new() -> Self {
            Self {
                inserts: Mutex::new(vec![]),
                updates: Mutex::new(vec![]),
            }
        }

        fn take_inserts(&self) -> Vec<MessageRow> {
            std::mem::take(&mut self.inserts.lock().unwrap())
        }

        #[allow(dead_code)]
        fn take_updates(&self) -> Vec<(String, nomifun_db::MessageRowUpdate)> {
            std::mem::take(&mut self.updates.lock().unwrap())
        }
    }

    #[async_trait::async_trait]
    impl IConversationRepository for RecordingRepo {
        async fn get(&self, _id: i64) -> Result<Option<nomifun_db::models::ConversationRow>, DbError> {
            Ok(None)
        }
        async fn create(&self, _row: &nomifun_db::models::ConversationRow) -> Result<i64, DbError> {
            Ok(1)
        }
        async fn update(&self, _id: i64, _updates: &nomifun_db::ConversationRowUpdate) -> Result<(), DbError> {
            Ok(())
        }
        async fn delete(&self, _id: i64) -> Result<(), DbError> {
            Ok(())
        }
        async fn list_paginated(
            &self,
            _user_id: &str,
            _filters: &nomifun_db::ConversationFilters,
        ) -> Result<nomifun_common::PaginatedResult<nomifun_db::models::ConversationRow>, DbError> {
            Ok(nomifun_common::PaginatedResult {
                items: vec![],
                total: 0,
                has_more: false,
            })
        }
        async fn find_by_source_and_chat(
            &self,
            _user_id: &str,
            _source: &str,
            _chat_id: &str,
            _agent_type: &str,
        ) -> Result<Option<nomifun_db::models::ConversationRow>, DbError> {
            Ok(None)
        }
        async fn list_by_cron_job(
            &self,
            _user_id: &str,
            _cron_job_id: &str,
        ) -> Result<Vec<nomifun_db::models::ConversationRow>, DbError> {
            Ok(vec![])
        }
        async fn list_associated(
            &self,
            _user_id: &str,
            _conversation_id: i64,
        ) -> Result<Vec<nomifun_db::models::ConversationRow>, DbError> {
            Ok(vec![])
        }
        async fn get_messages(
            &self,
            _conv_id: i64,
            _page: u32,
            _page_size: u32,
            _order: nomifun_db::SortOrder,
        ) -> Result<nomifun_common::PaginatedResult<MessageRow>, DbError> {
            Ok(nomifun_common::PaginatedResult {
                items: vec![],
                total: 0,
                has_more: false,
            })
        }
        async fn insert_message(&self, row: &MessageRow) -> Result<(), DbError> {
            self.inserts.lock().unwrap().push(row.clone());
            Ok(())
        }
        async fn update_message(&self, id: &str, updates: &nomifun_db::MessageRowUpdate) -> Result<(), DbError> {
            self.updates.lock().unwrap().push((id.to_owned(), updates.clone()));
            Ok(())
        }
        async fn delete_messages_by_conversation(&self, _conv_id: i64) -> Result<(), DbError> {
            Ok(())
        }
        async fn get_message_by_msg_id(
            &self,
            _conv_id: i64,
            msg_id: &str,
            msg_type: &str,
        ) -> Result<Option<MessageRow>, DbError> {
            let inserts = self.inserts.lock().unwrap();
            Ok(inserts
                .iter()
                .find(|m| m.msg_id.as_deref() == Some(msg_id) && m.r#type == msg_type)
                .cloned())
        }
        async fn search_messages(
            &self,
            _user_id: &str,
            _keyword: &str,
            _page: u32,
            _page_size: u32,
        ) -> Result<nomifun_common::PaginatedResult<nomifun_db::MessageSearchRow>, DbError> {
            Ok(nomifun_common::PaginatedResult {
                items: vec![],
                total: 0,
                has_more: false,
            })
        }
    }
}

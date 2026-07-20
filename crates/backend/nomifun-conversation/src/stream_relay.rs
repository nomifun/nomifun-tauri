use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use nomifun_ai_agent::{
    AgentSendError, AgentStreamEvent,
    artifact_store::ArtifactStore,
    protocol::events::{
        FinishEventData, PlanEventData, ThinkingEventData, TurnStopReason,
        tool_call::{
            AcpToolCallSessionUpdateKind, AcpToolCallStatus, ToolCallEventData,
            ToolCallStatus, validate_artifact_receipt_integrity,
            validate_completed_artifact_contract,
        },
    },
};

use crate::response_middleware::{ICronService, MessageMiddleware, MiddlewareResult};
use crate::runtime_state::{AgentTurnCancellation, ConversationRuntimeStateService};
use nomifun_api_types::{AgentErrorCode, ConversationRuntimeSummary, WebSocketMessage};
use nomifun_common::{CompanionId, ErrorChain, MessageId, normalize_keys_to_snake_case, now_ms};

use crate::service::ConversationService;
use nomifun_db::{IConversationRepository, MessageRowUpdate, TurnArtifactMessageCommit};
use nomifun_db::models::MessageRow;
use nomifun_realtime::UserEventSink;
use serde_json::{Value, json};
use tokio::sync::{broadcast, oneshot};
use tracing::{debug, error, info, warn};

/// Number of text chunks to accumulate before flushing to the database.
const FLUSH_INTERVAL: u32 = 20;
const TURN_COMPLETION_PERSIST_GRACE: Duration = Duration::from_secs(1);
const TERMINAL_FINALIZATION_GRACE: Duration = Duration::from_secs(5);
const ARTIFACT_COMMIT_GRACE: Duration = Duration::from_secs(5);
const EVENT_SIDE_EFFECT_GRACE: Duration = Duration::from_secs(1);
const MAX_TERMINAL_ACTIVE_ITEMS: usize = 256;
const ARTIFACT_DELIVERY_COMMITTED_FIELD: &str = "artifact_delivery_committed";
const ARTIFACT_DELIVERY_PENDING_OUTPUT: &str =
    "Artifact delivery is pending final turn validation";

fn track_bounded<V>(map: &mut HashMap<String, V>, key: String, value: V, kind: &'static str) -> bool {
    if map.contains_key(&key) || map.len() < MAX_TERMINAL_ACTIVE_ITEMS {
        map.insert(key, value);
        true
    } else {
        warn!(kind, max = MAX_TERMINAL_ACTIVE_ITEMS, "Relay terminal tracking limit reached");
        false
    }
}

fn remember_bounded(set: &mut HashSet<String>, value: String, kind: &'static str) -> bool {
    if set.contains(&value) || set.len() < MAX_TERMINAL_ACTIVE_ITEMS {
        set.insert(value);
        true
    } else {
        warn!(kind, max = MAX_TERMINAL_ACTIVE_ITEMS, "Relay terminal deduplication limit reached");
        false
    }
}

/// Apply the normalized ToolCall artifact contract to an externally-produced
/// ACP update. Only locally verified `Artifact` receipts count; a remote
/// ResourceLink is a locator, not proof that a requested image/export exists.
fn validate_completed_acp_artifact_contract(
    data: &nomifun_ai_agent::protocol::events::tool_call::AcpToolCallEventData,
) -> Result<(), String> {
    if data.update.status != Some(AcpToolCallStatus::Completed) {
        return Ok(());
    }
    let artifacts = data
        .update
        .content
        .iter()
        .flatten()
        .filter_map(|item| match item {
            nomifun_ai_agent::protocol::events::AcpToolCallContentItem::Artifact {
                artifact,
                ..
            } => Some(artifact.clone()),
            _ => None,
        })
        .collect::<Vec<_>>();
    validate_artifact_receipt_integrity("ACP artifact delivery", &artifacts)
        .map_err(|error| format!("ACP {error}"))?;
    const IDENTITY_KEYS: &[&str] = &[
        "tool",
        "tool_name",
        "toolName",
        "name",
        "operation",
        "operation_name",
        "operationName",
    ];
    let mut identities = data.update.title.iter().map(String::as_str).collect::<Vec<_>>();
    for value in [&data.update.raw_input, &data.update.raw_output]
        .into_iter()
        .filter_map(Option::as_ref)
    {
        let Some(object) = value.as_object() else {
            continue;
        };
        identities.extend(
            IDENTITY_KEYS
                .iter()
                .filter_map(|key| object.get(*key).and_then(Value::as_str)),
        );
    }
    identities.sort_unstable();
    identities.dedup();

    for name in identities {
        validate_completed_artifact_contract(&ToolCallEventData {
            call_id: data.update.tool_call_id.clone(),
            name: name.to_owned(),
            args: data.update.raw_input.clone().unwrap_or(Value::Null),
            status: ToolCallStatus::Completed,
            input: None,
            output: None,
            description: None,
            artifacts: artifacts.clone(),
        })
        .map_err(|error| format!("ACP {error}"))?;
    }
    Ok(())
}

/// Materialize a provider's sparse ACP update against the latest lifecycle
/// snapshot before validating or persisting it. ACP `ToolCallUpdate` fields are
/// optional and prompt-boundary completion synthesis intentionally carries only
/// the call id, terminal status and verified receipts. Committing that sparse
/// frame directly would discard the tool identity/input that established the
/// artifact contract.
fn effective_acp_tool_call_projection(
    active: Option<&nomifun_ai_agent::protocol::events::tool_call::AcpToolCallEventData>,
    incoming: &nomifun_ai_agent::protocol::events::tool_call::AcpToolCallEventData,
) -> nomifun_ai_agent::protocol::events::tool_call::AcpToolCallEventData {
    let Some(active) = active else {
        return incoming.clone();
    };
    let mut effective = incoming.clone();
    if effective.session_id.trim().is_empty() {
        effective.session_id.clone_from(&active.session_id);
    }
    if effective.update.status.is_none() {
        effective.update.status = active.update.status;
    }
    if effective.update.title.is_none() {
        effective.update.title.clone_from(&active.update.title);
    }
    if effective.update.kind.is_none() {
        effective.update.kind = active.update.kind;
    }
    if effective.update.raw_input.is_none() {
        effective.update.raw_input.clone_from(&active.update.raw_input);
    }
    if effective.update.raw_output.is_none() {
        effective.update.raw_output.clone_from(&active.update.raw_output);
    }
    if effective.update.content.is_none() {
        effective.update.content.clone_from(&active.update.content);
    } else if effective.update.status == Some(AcpToolCallStatus::Completed) {
        // A synthesized completion carries an authoritative delivery receipt
        // list but no narration/diff/terminal blocks. Retain those non-delivery
        // blocks from the active snapshot while replacing (rather than
        // duplicating) provisional artifact/resource locators.
        let mut merged = active
            .update
            .content
            .iter()
            .flatten()
            .filter(|item| {
                !matches!(
                    item,
                    nomifun_ai_agent::protocol::events::AcpToolCallContentItem::Artifact { .. }
                        | nomifun_ai_agent::protocol::events::AcpToolCallContentItem::ResourceLink { .. }
                )
            })
            .cloned()
            .collect::<Vec<_>>();
        let mut seen = merged
            .iter()
            .filter_map(|item| serde_json::to_string(item).ok())
            .collect::<HashSet<_>>();
        for item in incoming.update.content.iter().flatten() {
            let duplicate = serde_json::to_string(item)
                .ok()
                .is_some_and(|encoded| !seen.insert(encoded));
            if !duplicate {
                merged.push(item.clone());
            }
        }
        effective.update.content = Some(merged);
    }
    if effective.update.locations.is_none() {
        effective.update.locations.clone_from(&active.update.locations);
    }
    if effective.meta.is_none() {
        effective.meta.clone_from(&active.meta);
    }
    effective
}

/// ToolGroup is a legacy summary event and has no artifact receipt field. A
/// Completed high-signal generator/exporter entry therefore cannot establish
/// delivery and must be corrected to Error before the enclosing Finish.
fn tool_group_artifact_contract_errors(
    entries: &[nomifun_ai_agent::protocol::events::tool_call::ToolGroupEntry],
    completed_artifact_tool_calls: &HashMap<String, ToolCallEventData>,
) -> Vec<(usize, String)> {
    entries
        .iter()
        .enumerate()
        .filter_map(|(index, entry)| {
            let paired_delivery = completed_artifact_tool_calls.get(&entry.call_id);
            let result = validate_completed_artifact_contract(&ToolCallEventData {
                call_id: entry.call_id.clone(),
                name: entry.name.clone(),
                args: paired_delivery
                    .map(|delivery| delivery.args.clone())
                    .unwrap_or(Value::Null),
                status: entry.status,
                input: None,
                output: None,
                description: entry.description.clone(),
                artifacts: paired_delivery
                    .map(|delivery| delivery.artifacts.clone())
                    .unwrap_or_default(),
            });
            result.err().map(|error| (index, error))
        })
        .collect()
}

fn tool_group_entry_has_artifact_contract(
    entry: &nomifun_ai_agent::protocol::events::tool_call::ToolGroupEntry,
) -> bool {
    validate_completed_artifact_contract(&ToolCallEventData {
        call_id: entry.call_id.clone(),
        name: entry.name.clone(),
        args: Value::Null,
        status: ToolCallStatus::Completed,
        input: None,
        output: None,
        description: entry.description.clone(),
        artifacts: Vec::new(),
    })
    .is_err()
}

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
    completed_duration_ms: Option<u64>,
}

/// Result returned after a relay turn has fully drained and finalized.
#[derive(Debug, Clone, Default)]
pub struct RelayOutcome {
    pub system_responses: Vec<String>,
    pub terminal: RelayTerminal,
    /// Normalized terminal reason carried by Finish. `Cancelled` is never a
    /// successful completion and must suppress failover, continuation, and
    /// post-turn writeback in the service send loop.
    pub stop_reason: Option<TurnStopReason>,
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
    /// Final visible assistant text after response middleware rewrites. Used by
    /// turn-final knowledge write-back after the relay has persisted the text and
    /// completed the turn.
    pub final_text: Option<String>,
    /// Message id of the visible text row that should own turn-final
    /// post-processing UI state. This may differ from the turn's primary msg_id
    /// when the turn starts with thinking/tool output before final text.
    pub final_text_msg_id: Option<String>,
}

fn turn_writeback_status_label(status: nomifun_knowledge::TurnWritebackStatus) -> &'static str {
    match status {
        nomifun_knowledge::TurnWritebackStatus::Disabled => "disabled",
        nomifun_knowledge::TurnWritebackStatus::NoCompleter => "no_completer",
        nomifun_knowledge::TurnWritebackStatus::NoCandidate => "no_candidate",
        nomifun_knowledge::TurnWritebackStatus::Written => "written",
        nomifun_knowledge::TurnWritebackStatus::Partial => "partial",
        nomifun_knowledge::TurnWritebackStatus::Failed => "failed",
    }
}

fn turn_writeback_phase_label(phase: nomifun_knowledge::TurnWritebackPhase) -> &'static str {
    match phase {
        nomifun_knowledge::TurnWritebackPhase::Extracting => "extracting",
        nomifun_knowledge::TurnWritebackPhase::Writing => "writing",
    }
}

fn turn_writeback_retryable(status: nomifun_knowledge::TurnWritebackStatus) -> bool {
    matches!(
        status,
        nomifun_knowledge::TurnWritebackStatus::NoCompleter
            | nomifun_knowledge::TurnWritebackStatus::Partial
            | nomifun_knowledge::TurnWritebackStatus::Failed
    )
}

fn turn_writeback_running_state(status: &str, attempt_id: &str, started_at: i64, updated_at: i64) -> Value {
    json!({
        "status": status,
        "attempt_id": attempt_id,
        "started_at": started_at,
        "updated_at": updated_at,
        "finished_at": Value::Null,
        "retryable": false,
        "candidates": 0,
        "written": [],
        "failures": [],
    })
}

fn turn_writeback_final_state(
    report: &nomifun_knowledge::TurnWritebackReport,
    status: &str,
    attempt_id: &str,
    started_at: i64,
    finished_at: i64,
) -> Value {
    json!({
        "status": status,
        "attempt_id": attempt_id,
        "started_at": started_at,
        "updated_at": finished_at,
        "finished_at": finished_at,
        "retryable": turn_writeback_retryable(report.status),
        "candidates": report.candidates,
        "written": report.written.iter().map(|w| json!({
            "kb_id": w.kb_id.clone(),
            "rel_path": w.final_rel_path.clone(),
            "staged": w.staged,
        })).collect::<Vec<_>>(),
        "failures": report.failures.iter().map(|f| json!({
            "kb_id": f.kb_id.clone(),
            "rel_path": f.rel_path.clone(),
            "error": f.error.clone(),
        })).collect::<Vec<_>>(),
    })
}

fn turn_writeback_event_payload(conversation_id: &str, msg_id: &str, state: &Value) -> Value {
    let mut payload = state.clone();
    if let Some(obj) = payload.as_object_mut() {
        obj.insert("conversation_id".to_owned(), json!(conversation_id));
        obj.insert("msg_id".to_owned(), json!(msg_id));
    }
    payload
}

async fn persist_turn_writeback_state(
    repo: &Arc<dyn IConversationRepository>,
    conversation_id: &str,
    msg_id: &str,
    state: &Value,
) {
    let row = match repo.get_message(conversation_id, msg_id).await {
        Ok(Some(row)) => row,
        Ok(None) => {
            debug!(conversation_id, msg_id, "skip writeback state persist; assistant message row not found");
            return;
        }
        Err(e) => {
            warn!(
                conversation_id,
                msg_id,
                error = %ErrorChain(&e),
                "failed to load assistant message for writeback state"
            );
            return;
        }
    };

    let mut content: Value =
        serde_json::from_str(&row.content).unwrap_or_else(|_| json!({ "content": row.content }));
    if !content.is_object() {
        content = json!({ "content": content });
    }
    if let Some(obj) = content.as_object_mut() {
        obj.insert("knowledge_writeback".to_owned(), state.clone());
    }

    let update = MessageRowUpdate {
        content: Some(content.to_string()),
        status: None,
        hidden: None,
    };
    if let Err(e) = repo.update_message(&row.id, &update).await {
        warn!(
            conversation_id,
            msg_id,
            error = %ErrorChain(&e),
            "failed to persist assistant message writeback state"
        );
    }
}

async fn emit_turn_writeback_state(
    repo: &Arc<dyn IConversationRepository>,
    user_events: &Arc<dyn UserEventSink>,
    user_id: &str,
    conversation_id: &str,
    msg_id: &str,
    state: Value,
) {
    persist_turn_writeback_state(repo, conversation_id, msg_id, &state).await;
    user_events.send_to_user(
        user_id,
        WebSocketMessage::new(
            "knowledge.writeback",
            turn_writeback_event_payload(conversation_id, msg_id, &state),
        ),
    );
}

pub(crate) async fn run_turn_writeback_report(
    service: Arc<nomifun_knowledge::KnowledgeService>,
    mut request: nomifun_knowledge::TurnWritebackRequest,
    repo: Arc<dyn IConversationRepository>,
    user_events: Arc<dyn UserEventSink>,
    user_id: String,
    conversation_id: String,
    msg_id: String,
    final_text: String,
) {
    if final_text.trim().is_empty() {
        return;
    }
    request.assistant_text = final_text;
    let started_at = now_ms();
    let attempt_id = format!("{msg_id}:{started_at}");
    emit_turn_writeback_state(
        &repo,
        &user_events,
        &user_id,
        &conversation_id,
        &msg_id,
        turn_writeback_running_state("started", &attempt_id, started_at, started_at),
    )
    .await;

    let progress_repo = Arc::clone(&repo);
    let progress_user_events = Arc::clone(&user_events);
    let progress_user_id = user_id.clone();
    let progress_conversation_id = conversation_id.clone();
    let progress_msg_id = msg_id.clone();
    let progress_attempt_id = attempt_id.clone();
    let report = service
        .finalize_turn_writeback_with_progress(request, move |phase| {
            let repo = Arc::clone(&progress_repo);
            let user_events = Arc::clone(&progress_user_events);
            let user_id = progress_user_id.clone();
            let conversation_id = progress_conversation_id.clone();
            let msg_id = progress_msg_id.clone();
            let attempt_id = progress_attempt_id.clone();
            let status = turn_writeback_phase_label(phase);
            async move {
                let updated_at = now_ms();
                emit_turn_writeback_state(
                    &repo,
                    &user_events,
                    &user_id,
                    &conversation_id,
                    &msg_id,
                    turn_writeback_running_state(status, &attempt_id, started_at, updated_at),
                )
                .await;
            }
        })
        .await;
    let status = turn_writeback_status_label(report.status);
    match report.status {
        nomifun_knowledge::TurnWritebackStatus::Written
        | nomifun_knowledge::TurnWritebackStatus::Partial => {
            info!(
                conversation_id = %conversation_id,
                msg_id = %msg_id,
                candidates = report.candidates,
                written = report.written.len(),
                failures = report.failures.len(),
                "turn-final knowledge write-back completed"
            );
        }
        nomifun_knowledge::TurnWritebackStatus::Failed => {
            warn!(
                conversation_id = %conversation_id,
                msg_id = %msg_id,
                candidates = report.candidates,
                failures = report.failures.len(),
                "turn-final knowledge write-back failed"
            );
        }
        other => {
            debug!(
                conversation_id = %conversation_id,
                msg_id = %msg_id,
                status = ?other,
                "turn-final knowledge write-back skipped"
            );
        }
    }
    let finished_at = now_ms();
    emit_turn_writeback_state(
        &repo,
        &user_events,
        &user_id,
        &conversation_id,
        &msg_id,
        turn_writeback_final_state(&report, status, &attempt_id, started_at, finished_at),
    )
    .await;
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
    /// Stable identity of the user-visible logical turn. This remains fixed
    /// across model failover and system continuations.
    root_turn_id: String,
    /// Identity of the current provider wire segment within `root_turn_id`.
    ///
    /// This is only a transport/stream identity. Durable child messages and
    /// artifact commits belong to `root_turn_id`, otherwise a continuation is
    /// grouped under a different turn after history hydration than it was on
    /// the live WebSocket stream.
    msg_id: String,
    user_id: String,
    repo: Arc<dyn IConversationRepository>,
    user_events: Arc<dyn UserEventSink>,
    cron_service: Option<Arc<dyn ICronService>>,
    complete_turn: bool,
    /// Companion-companion wire markers (from `conversation.extra.companion_session` /
    /// `.companion_id`), stamped onto every `message.stream` / `turn.completed`
    /// payload so the companion collector can classify the turn off the wire.
    companion: bool,
    companion_id: Option<CompanionId>,
    /// Originator of the user message that started this turn when it was NOT
    /// typed by the human owner (`"companion"` / `"cron"` / `"autowork"` /
    /// `"idmm"`; `None` = a real person). Stamped onto every `message.stream`
    /// / `turn.completed` payload of the turn so downstream consumers (the
    /// companion collector) can tell agent-driven replies from owner-driven work.
    origin: Option<String>,
    /// IM platform of a Channel Agent conversation (from
    /// `conversation.extra.channel_platform`, e.g. `"telegram"`; `None` = not
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
    /// Process-wide runtime state, used here only to accumulate this turn's
    /// `TurnCompleted` token usage (`input + output`) into the conversation's
    /// running total so the owning execution attempt can read it after the turn
    /// settles. `None` (the default) =
    /// no token accumulation (the common chat/companion path is unaffected).
    /// `ConversationService::send_message` wires it only when the authoritative
    /// Conversation↔Execution relation identifies an active attempt. Once wired,
    /// the relay always accumulates; it does not perform a second identity lookup.
    runtime_state: Option<Arc<ConversationRuntimeStateService>>,
    /// Generation-scoped service cancellation. This is independent of every
    /// backend transport, so a CLI/gateway that ignores its abort request cannot
    /// leave the relay waiting forever for a terminal event.
    cancellation: Option<AgentTurnCancellation>,
    /// Stable canonical row IDs for streamed sub-records that receive multiple
    /// updates during one relay. Protocol call/session IDs are correlation keys,
    /// never database entity IDs.
    derived_message_ids: std::sync::Mutex<HashMap<String, String>>,
    event_side_effect_circuit_open: AtomicBool,
    /// Canonical session workspace used to re-verify every local receipt at
    /// the final database commit barrier. Runtime event payloads are untrusted:
    /// a marker proves an atomic DB transition, not that bytes exist.
    artifact_workspace: Option<PathBuf>,
}

impl StreamRelay {
    async fn bounded_event_side_effect<T, F>(
        &self,
        deadline: tokio::time::Instant,
        label: &'static str,
        future: F,
    ) -> Option<T>
    where
        F: Future<Output = T>,
    {
        if self.event_side_effect_circuit_open.load(Ordering::Acquire) {
            return None;
        }
        let timed = tokio::time::timeout_at(deadline, future);
        let result = if let Some(cancellation) = self.cancellation.as_ref() {
            tokio::select! {
                biased;
                _ = cancellation.cancelled() => return None,
                result = timed => result,
            }
        } else {
            timed.await
        };
        match result {
            Ok(value) => Some(value),
            Err(_) => {
                self.event_side_effect_circuit_open
                    .store(true, Ordering::Release);
                warn!(
                    conversation_id = %self.conversation_id,
                    msg_id = %self.msg_id,
                    side_effect = label,
                    "Relay event side effect exceeded its shared hard bound; continuing to consume the stream"
                );
                None
            }
        }
    }

    pub fn new(
        conversation_id: String,
        msg_id: String,
        user_id: String,
        repo: Arc<dyn IConversationRepository>,
        user_events: Arc<dyn UserEventSink>,
        cron_service: Option<Arc<dyn ICronService>>,
    ) -> Self {
        let root_turn_id = msg_id.clone();
        Self {
            conversation_id,
            root_turn_id,
            msg_id,
            user_id,
            repo,
            user_events,
            cron_service,
            complete_turn: true,
            companion: false,
            companion_id: None,
            origin: None,
            channel_platform: None,
            failover_suppressor: None,
            runtime_state: None,
            cancellation: None,
            derived_message_ids: std::sync::Mutex::new(HashMap::new()),
            event_side_effect_circuit_open: AtomicBool::new(false),
            artifact_workspace: None,
        }
    }

    pub fn with_turn_completion(mut self, enabled: bool) -> Self {
        self.complete_turn = enabled;
        self
    }

    pub fn with_root_turn_id(mut self, turn_id: impl Into<String>) -> Self {
        self.root_turn_id = turn_id.into();
        self
    }

    /// Wire the process-wide runtime state so this relay accumulates each turn's
    /// `TurnCompleted` token usage into the conversation's running total (read
    /// back by the owning execution attempt after the turn settles). The
    /// Conversation service wires it only for an active attempt relation. Default
    /// chat and companion turns leave it unset.
    pub fn with_runtime_state(mut self, runtime_state: Arc<ConversationRuntimeStateService>) -> Self {
        self.runtime_state = Some(runtime_state);
        self
    }

    pub fn with_cancellation(mut self, cancellation: AgentTurnCancellation) -> Self {
        self.cancellation = Some(cancellation);
        self
    }

    pub fn with_artifact_workspace(mut self, workspace: impl Into<PathBuf>) -> Self {
        self.artifact_workspace = Some(workspace.into());
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
    pub fn with_companion_context(
        mut self,
        companion: bool,
        companion_id: Option<CompanionId>,
    ) -> Self {
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
    pub async fn surface_terminal_error(
        &self,
        event: &AgentStreamEvent,
        cancellation: &AgentTurnCancellation,
    ) -> bool {
        let AgentStreamEvent::Error(data) = event else {
            return false;
        };
        if !cancellation.try_claim_terminal_surface() {
            return false;
        }
        if cancellation.is_cancelled() {
            self.forward_to_websocket(&Self::cancelled_finish_event());
            cancellation.mark_terminal_observed();
            return false;
        }
        let error_message_id = ConversationService::mint_msg_id();
        self.forward_to_websocket_with_msg_id(&error_message_id, event);
        let persistence = tokio::time::timeout(
            TURN_COMPLETION_PERSIST_GRACE,
            self.persist_error_tips(&error_message_id, data),
        );
        tokio::select! {
            biased;
            _ = cancellation.cancelled() => {
                warn!(conversation_id = %self.conversation_id, "Cancelled while persisting re-surfaced terminal error");
            }
            result = persistence => {
                if result.is_err() {
                    warn!(conversation_id = %self.conversation_id, "Timed out persisting re-surfaced terminal error");
                }
            }
        }
        cancellation.mark_terminal_observed();
        true
    }

    /// Run the relay loop while also accepting a typed send failure from the
    /// task that called `AgentRuntimeControl::send_message`.
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
        send_error_rx: oneshot::Receiver<Result<(), AgentSendError>>,
    ) -> RelayOutcome {
        self.consume_inner(rx, Some(send_error_rx)).await
    }

    async fn consume_inner(
        self,
        mut rx: broadcast::Receiver<AgentStreamEvent>,
        mut send_error_rx: Option<oneshot::Receiver<Result<(), AgentSendError>>>,
    ) -> RelayOutcome {
        let started_at = now_ms();
        info!("StreamRelay started");

        let mut full_text_buffer = String::new();
        let mut text_segments: Vec<PersistedTextSegment> = Vec::new();
        let mut active_text: Option<TextSegmentState> = None;
        let mut active_thinking: Option<ThinkingSegmentState> = None;
        let mut active_tool_calls: HashMap<String, ToolCallEventData> = HashMap::new();
        let mut completed_artifact_tool_calls: HashMap<String, ToolCallEventData> = HashMap::new();
        let mut terminal_tool_calls: HashSet<String> = HashSet::new();
        let mut failed_terminal_tool_calls: HashSet<String> = HashSet::new();
        let mut active_acp_tool_calls: HashMap<
            String,
            nomifun_ai_agent::protocol::events::tool_call::AcpToolCallEventData,
        > = HashMap::new();
        let mut completed_artifact_acp_tool_calls: HashMap<
            String,
            nomifun_ai_agent::protocol::events::tool_call::AcpToolCallEventData,
        > = HashMap::new();
        let mut terminal_acp_tool_calls: HashSet<String> = HashSet::new();
        let mut failed_terminal_acp_tool_calls: HashSet<String> = HashSet::new();
        let mut active_tool_groups: HashMap<
            String,
            Vec<nomifun_ai_agent::protocol::events::tool_call::ToolGroupEntry>,
        > = HashMap::new();
        let mut active_plan_ids: HashSet<String> = HashSet::new();
        let mut active_agent_status: Option<nomifun_ai_agent::protocol::events::AgentStatusEventData> = None;
        let mut used_primary_segment_msg_id = false;
        let mut first_agent_event_logged = false;
        let mut first_visible_output_logged = false;
        let mut fatal_tracking_error: Option<String> = None;
        // Phase 3 (plan D4): tracks whether any externally-visible response has
        // been emitted this turn — assistant Text OR a forwarded/persisted tool
        // action. Surfaced on the RelayOutcome so the failover seam can restrict
        // switching to faults that produced NO visible output (no duplicate
        // text, no duplicate tool side effect / billing).
        let mut emitted_response = false;
        let mut send_error_done = send_error_rx.is_none();

        loop {
            let recv_result = if let Some(message) = fatal_tracking_error.take() {
                Ok(AgentStreamEvent::Error(
                    nomifun_ai_agent::protocol::events::ErrorEventData::legacy(
                        message,
                        Some(AgentErrorCode::NomifunStreamBroken),
                    ),
                ))
            } else {
                match (self.cancellation.as_ref(), send_error_done) {
                (Some(cancellation), true) => {
                    tokio::select! {
                        biased;
                        _ = cancellation.cancelled() => Ok(Self::cancelled_finish_event()),
                        recv = rx.recv() => recv,
                    }
                }
                (Some(cancellation), false) => {
                    tokio::select! {
                        biased;
                        _ = cancellation.cancelled() => Ok(Self::cancelled_finish_event()),
                        recv = rx.recv() => recv,
                        send_error = send_error_rx.as_mut().expect("send_error_rx exists while pending") => {
                            send_error_done = true;
                            match send_error {
                                Ok(Err(send_error)) => {
                                    warn!(
                                        code = ?send_error.code(),
                                        ownership = ?send_error.ownership(),
                                        "Injecting stream error for failed agent send"
                                    );
                                    Ok(AgentStreamEvent::Error(send_error.into_stream_error()))
                                }
                                Ok(Ok(())) => continue,
                                Err(_) => Ok(AgentStreamEvent::Error(
                                    nomifun_ai_agent::protocol::events::ErrorEventData::legacy(
                                        "Agent send task exited before reporting acceptance",
                                        None,
                                    ),
                                )),
                            }
                        }
                    }
                }
                (None, true) => rx.recv().await,
                (None, false) => {
                    tokio::select! {
                        recv = rx.recv() => recv,
                        send_error = send_error_rx.as_mut().expect("send_error_rx exists while pending") => {
                            send_error_done = true;
                            match send_error {
                                Ok(Err(send_error)) => {
                                    warn!(
                                        code = ?send_error.code(),
                                        ownership = ?send_error.ownership(),
                                        "Injecting stream error for failed agent send"
                                    );
                                    Ok(AgentStreamEvent::Error(send_error.into_stream_error()))
                                }
                                Ok(Ok(())) => continue,
                                Err(_) => Ok(AgentStreamEvent::Error(
                                    nomifun_ai_agent::protocol::events::ErrorEventData::legacy(
                                        "Agent send task exited before reporting acceptance",
                                        None,
                                    ),
                                )),
                            }
                        }
                    }
                }
            }
            };
            let recv_result = match recv_result {
                Err(broadcast::error::RecvError::Lagged(skipped)) => {
                    warn!(lagged = skipped, "Stream relay lagged; terminating the incomplete event stream");
                    Ok(AgentStreamEvent::Error(
                        nomifun_ai_agent::protocol::events::ErrorEventData::legacy(
                            format!(
                                "Agent event stream lagged and skipped {skipped} events; the turn was terminated to preserve terminal-state integrity"
                            ),
                            Some(AgentErrorCode::NomifunStreamBroken),
                        ),
                    ))
                }
                result => result,
            };

            match recv_result {
                Ok(mut event) => {
                    // Cancellation is authoritative even if `rx.recv()` won
                    // just before the token fired. Re-check after receive so a
                    // concurrently queued ordinary Finish cannot execute
                    // middleware/cron or be reported as successful.
                    if self
                        .cancellation
                        .as_ref()
                        .is_some_and(AgentTurnCancellation::is_cancelled)
                        && matches!(event, AgentStreamEvent::Finish(_) | AgentStreamEvent::Error(_))
                    {
                        event = Self::cancelled_finish_event();
                    }
                    if !first_agent_event_logged {
                        first_agent_event_logged = true;
                        info!(
                            event_type = Self::event_kind(&event),
                            elapsed_ms = now_ms().saturating_sub(started_at),
                            "StreamRelay received first agent event"
                        );
                    }
                    // Every non-terminal event shares one persistence budget.
                    // WebSocket forwarding happens first where applicable;
                    // a locked/failed DB must never prevent an already-queued
                    // Finish/Error from being consumed.
                    let event_side_effect_deadline =
                        tokio::time::Instant::now() + EVENT_SIDE_EFFECT_GRACE;

                    match &event {
                        AgentStreamEvent::Thinking(data) => {
                            if data.status.as_deref() == Some("done") {
                                let _ = self
                                    .bounded_event_side_effect(
                                        event_side_effect_deadline,
                                        "complete_thinking",
                                        self.complete_active_thinking(&mut active_thinking),
                                    )
                                    .await;
                                continue;
                            }

                            // Plan D4: a broadcast/persisted thinking segment is
                            // externally visible — once it streams we are no
                            // longer pre-response, so the failover seam stands down.
                            emitted_response = true;
                            let _ = self
                                .bounded_event_side_effect(
                                    event_side_effect_deadline,
                                    "close_text_before_thinking",
                                    self.close_active_text_segment(
                                        &mut active_text,
                                        &mut text_segments,
                                        "finish",
                                    ),
                                )
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
                                completed_duration_ms: None,
                            });
                            segment.buffer.push_str(&data.content);
                            self.forward_to_websocket_with_msg_id(&segment.id, &event);
                        }
                        AgentStreamEvent::Text(data) => {
                            let _ = self
                                .bounded_event_side_effect(
                                    event_side_effect_deadline,
                                    "complete_thinking_before_text",
                                    self.complete_active_thinking(&mut active_thinking),
                                )
                                .await;
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
                                let _ = self
                                    .bounded_event_side_effect(
                                        event_side_effect_deadline,
                                        "flush_text",
                                        self.flush_text_segment(segment),
                                    )
                                    .await;
                                segment.flush_counter = 0;
                            }
                        }
                        AgentStreamEvent::Finish(_) | AgentStreamEvent::Error(_) => {
                            if self
                                .cancellation
                                .as_ref()
                                .is_some_and(AgentTurnCancellation::is_cancelled)
                                && !Self::is_cancelled_finish(&event)
                            {
                                event = Self::cancelled_finish_event();
                            }
                            let mut terminal = Self::terminal_from_event(&event);
                            // Decide suppression before any persistence await.
                            // Terminal publication is scoped to the current
                            // wire segment. The send loop resets that scope for
                            // every continuation/failover resend, so ordinary
                            // intermediate terminals cannot mask cancellation
                            // of a later segment.
                            let mut suppress_error = !emitted_response
                                && matches!(event, AgentStreamEvent::Error(_))
                                && terminal
                                    .code()
                                    .zip(self.failover_suppressor.as_ref())
                                    .is_some_and(|(code, suppressor)| suppressor(code));
                            let mut terminal_claimed = false;
                            if !suppress_error {
                                terminal_claimed = self
                                    .cancellation
                                    .as_ref()
                                    .map(AgentTurnCancellation::try_claim_terminal_surface)
                                    .unwrap_or(true);
                                if !terminal_claimed {
                                    // A bounded stop fallback (or another
                                    // terminal publisher for this exact wire
                                    // segment) already won. Never publish or
                                    // middleware-process a late ordinary
                                    // terminal after that cancelled terminal.
                                    event = Self::cancelled_finish_event();
                                    terminal = Self::terminal_from_event(&event);
                                    suppress_error = false;
                                }
                            }

                            // Visible assistant-segment durability is a
                            // prerequisite for committing successful artifact
                            // receipts. If this bounded write cannot settle,
                            // convert Finish before the
                            // artifact commit gate so the ordinary terminal
                            // correction path retracts every provisional
                            // receipt instead of leaving a green artifact on an
                            // otherwise inconsistent turn.
                            let text_status = if matches!(event, AgentStreamEvent::Error(_))
                                || Self::is_cancelled_finish(&event)
                            {
                                "error"
                            } else {
                                "finish"
                            };
                            let (thinking_persistence_complete, text_persistence_complete) = match tokio::time::timeout(
                                TERMINAL_FINALIZATION_GRACE,
                                async {
                                    let thinking_complete = self
                                        .complete_active_thinking(&mut active_thinking)
                                        .await;
                                    let thinking_complete = if thinking_complete {
                                        true
                                    } else {
                                        self.retry_terminal_thinking_segment(&mut active_thinking)
                                            .await
                                    };
                                    self.close_active_text_segment(
                                        &mut active_text,
                                        &mut text_segments,
                                        text_status,
                                    )
                                    .await;
                                    let text_complete = self.retry_terminal_text_segment(
                                        &mut active_text,
                                        &mut text_segments,
                                        text_status,
                                    )
                                    .await;
                                    (thinking_complete, text_complete)
                                },
                            )
                            .await
                            {
                                Ok(complete) => complete,
                                Err(_) => {
                                    let thinking_complete = active_thinking.is_none();
                                    let text_complete = active_text.is_none();
                                    if !thinking_complete || !text_complete {
                                        warn!(
                                            conversation_id = %self.conversation_id,
                                            msg_id = %self.msg_id,
                                            "Assistant segment terminal persistence exceeded the hard bound"
                                        );
                                    }
                                    (thinking_complete, text_complete)
                                }
                            };
                            if (!thinking_persistence_complete || !text_persistence_complete)
                                && matches!(event, AgentStreamEvent::Finish(_))
                            {
                                event = Self::assistant_segment_persistence_error_event();
                                terminal = Self::terminal_from_event(&event);
                                suppress_error = false;
                            }

                            if terminal_claimed
                                && !Self::invalidates_completed_artifacts(&event)
                                && (!completed_artifact_tool_calls.is_empty()
                                    || !completed_artifact_acp_tool_calls.is_empty())
                            {
                                let commit_result = tokio::time::timeout(
                                    ARTIFACT_COMMIT_GRACE,
                                    self.commit_pending_artifact_deliveries(
                                        &completed_artifact_tool_calls,
                                        &completed_artifact_acp_tool_calls,
                                    ),
                                )
                                .await;

                                match commit_result {
                                    Ok(Ok(())) => {
                                        // The transaction is now the linearization
                                        // point for artifact success. Publish every
                                        // receipt-bearing Completed frame only after
                                        // all rows committed, and still before Finish.
                                        self.broadcast_committed_artifact_tool_calls(
                                            &completed_artifact_tool_calls,
                                        );
                                        self.broadcast_committed_artifact_acp_tool_calls(
                                            &completed_artifact_acp_tool_calls,
                                        );
                                        completed_artifact_tool_calls.clear();
                                        completed_artifact_acp_tool_calls.clear();
                                    }
                                    Ok(Err(commit_error)) => {
                                        error!(
                                            error = %ErrorChain(&commit_error),
                                            "Atomic artifact projection failed; rejecting turn success"
                                        );
                                        event = AgentStreamEvent::Error(
                                            nomifun_ai_agent::protocol::events::ErrorEventData::legacy(
                                                "The generated artifacts could not be committed to conversation history",
                                                Some(AgentErrorCode::NomifunStateInconsistent),
                                            ),
                                        );
                                        terminal = Self::terminal_from_event(&event);
                                        suppress_error = false;
                                    }
                                    Err(_) => {
                                        error!(
                                            conversation_id = %self.conversation_id,
                                            msg_id = %self.msg_id,
                                            "Atomic artifact projection timed out; rejecting turn success"
                                        );
                                        event = AgentStreamEvent::Error(
                                            nomifun_ai_agent::protocol::events::ErrorEventData::legacy(
                                                "Timed out while committing generated artifacts to conversation history",
                                                Some(AgentErrorCode::NomifunStateInconsistent),
                                            ),
                                        );
                                        terminal = Self::terminal_from_event(&event);
                                        suppress_error = false;
                                    }
                                }
                            }
                            // A terminal error is its own durable message, not
                            // another update of the assistant text/thinking
                            // message that happened to use the turn's primary
                            // wire id. Mint the identity once and use it for
                            // both the live frame and the persisted tips row;
                            // `turn_id` retains the owning turn relation.
                            let terminal_message_id = if matches!(event, AgentStreamEvent::Error(_))
                                && !suppress_error
                            {
                                ConversationService::mint_msg_id()
                            } else {
                                self.msg_id.clone()
                            };
                            let elapsed_ms = now_ms() - started_at;
                            let event_type = if matches!(event, AgentStreamEvent::Finish(_)) {
                                "Finish"
                            } else {
                                "Error"
                            };
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

                            let terminal_cleanup = async {
                            // Artifact corrections are the first terminal side
                            // effect and are all broadcast before any repository
                            // await. Even a wedged DB cannot leave strict live
                            // consumers with an earlier green receipt.
                            let invalidates_artifacts =
                                !suppress_error && Self::invalidates_completed_artifacts(&event);
                            let (failed_completed_tools, failed_completed_acp_tools) =
                                if invalidates_artifacts {
                                    let reason = Self::incomplete_tool_reason(&event)
                                        .unwrap_or("incomplete_turn");
                                    let tools = Self::take_failed_tool_calls(
                                        &mut completed_artifact_tool_calls,
                                        reason,
                                    );
                                    let acp_tools = Self::take_failed_acp_tool_calls(
                                        &mut completed_artifact_acp_tool_calls,
                                        reason,
                                    );
                                    self.broadcast_failed_tool_calls(&tools);
                                    self.broadcast_failed_acp_tool_calls(&acp_tools);
                                    (tools, acp_tools)
                                } else {
                                    (Vec::new(), Vec::new())
                                };

                            let _ = tokio::join!(
                                self.persist_failed_tool_calls(&failed_completed_tools),
                                self.persist_failed_acp_tool_calls(&failed_completed_acp_tools),
                            );
                            // review #1/#5: a pre-response provider-fault that the
                            // send loop will fail over must NOT reach the user —
                            // suppress the WS error event AND the error `tips` row
                            // at source, so a recovered turn shows only the backup
                            // model's output. Only the Error terminal with no
                            // emitted response and a positive suppressor verdict
                            // qualifies; everything else broadcasts/persists as before.
                            if suppress_error {
                                info!("StreamRelay suppressing pre-response error pending model failover");
                            } else {
                                if let Some(reason) = Self::incomplete_tool_reason(&event) {
                                    // A provider can emit a per-tool Completed frame and then
                                    // fail/cancel/truncate the enclosing turn. Artifact success
                                    // is a turn-level contract, so retract those receipts on an
                                    // unsuccessful terminal. A normal EndTurn/unspecified Finish
                                    // keeps already verified completed artifacts, while still
                                    // closing genuinely Running tools below.
                                    self.fail_active_tool_calls(&mut active_tool_calls, reason).await;
                                    self.fail_active_acp_tool_calls(&mut active_acp_tool_calls, reason).await;
                                    self.fail_active_tool_groups(&mut active_tool_groups, reason).await;
                                }
                            }
                            self.finalize_active_plans(
                                &mut active_plan_ids,
                                Self::plan_terminal_status(&event),
                            )
                            .await;
                            self.finalize_active_agent_status(
                                &mut active_agent_status,
                                Self::plan_terminal_status(&event),
                            )
                            .await;
                            let outcome = self
                                .finalize(
                                    &full_text_buffer,
                                    &text_segments,
                                    text_persistence_complete,
                                    &event,
                                    terminal,
                                    emitted_response,
                                    suppress_error,
                                    &terminal_message_id,
                                )
                                .await;
                            // Publish the terminal only after all lifecycle
                            // corrections. Strict consumers may stop reading at
                            // Error/Finish, so a receipt retraction sent after it
                            // would leave stale success visible.
                            if terminal_claimed {
                                self.forward_to_websocket_with_msg_id(&terminal_message_id, &event);
                            }
                            outcome
                            };
                            let outcome = match tokio::time::timeout(
                                TERMINAL_FINALIZATION_GRACE,
                                terminal_cleanup,
                            )
                            .await
                            {
                                Ok(outcome) => outcome,
                                Err(_) => {
                                    warn!(
                                        conversation_id = %self.conversation_id,
                                        msg_id = %self.msg_id,
                                        "Terminal relay finalization exceeded the hard bound"
                                    );
                                    if terminal_claimed {
                                        self.forward_to_websocket_with_msg_id(&terminal_message_id, &event);
                                    }
                                    RelayOutcome {
                                        system_responses: Vec::new(),
                                        terminal: Self::terminal_from_event(&event),
                                        stop_reason: match &event {
                                            AgentStreamEvent::Finish(data) => data.stop_reason,
                                            _ => None,
                                        },
                                        emitted_response,
                                        suppressed_error: suppress_error.then(|| event.clone()),
                                        final_text: (text_persistence_complete
                                            && !full_text_buffer.trim().is_empty())
                                            .then(|| full_text_buffer.trim().to_owned()),
                                        final_text_msg_id: text_persistence_complete
                                            .then(|| text_segments.last().map(|segment| segment.id.clone()))
                                            .flatten(),
                                    }
                                }
                            };
                            if terminal_claimed
                                && let Some(cancellation) = self.cancellation.as_ref()
                            {
                                // Relay persistence/finalization is complete
                                // and the authoritative Finish is already on
                                // the wire. The stop worker may now release the
                                // exact generation and publish turn.completed.
                                cancellation.mark_terminal_observed();
                            }
                            if self.complete_turn {
                                Self::complete_conversation_with_context(
                                    &self.repo,
                                    &self.user_events,
                                    &self.user_id,
                                    &self.conversation_id,
                                    Some(self.root_turn_id.clone()),
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
                            let has_artifact_delivery =
                                data.status == ToolCallStatus::Completed && !data.artifacts.is_empty();
                            let active_contract_source = active_tool_calls.get(&data.call_id).cloned();
                            let artifact_contract_error = if data.status == ToolCallStatus::Completed {
                                let terminal_error = validate_completed_artifact_contract(data).err();
                                terminal_error.or_else(|| {
                                    active_contract_source.as_ref().and_then(|active| {
                                        let mut effective = active.clone();
                                        effective.status = ToolCallStatus::Completed;
                                        effective.artifacts = data.artifacts.clone();
                                        validate_completed_artifact_contract(&effective).err()
                                    })
                                })
                            } else {
                                None
                            };
                            let mut tracking_overflow = false;
                            match data.status {
                                ToolCallStatus::Running => {
                                    if terminal_tool_calls.contains(&data.call_id) {
                                        warn!(
                                            call_id = %data.call_id,
                                            tool = %data.name,
                                            "Ignoring late running event for terminal tool call"
                                        );
                                        continue;
                                    }
                                    tracking_overflow |= !track_bounded(
                                        &mut active_tool_calls,
                                        data.call_id.clone(),
                                        data.clone(),
                                        "tool_call",
                                    );
                                }
                                ToolCallStatus::Completed | ToolCallStatus::Error => {
                                    if terminal_tool_calls.contains(&data.call_id) {
                                        if data.status == ToolCallStatus::Error
                                            && !failed_terminal_tool_calls.contains(&data.call_id)
                                        {
                                            tracking_overflow |= !remember_bounded(
                                                &mut failed_terminal_tool_calls,
                                                data.call_id.clone(),
                                                "failed_terminal_tool_call",
                                            );
                                        } else {
                                            warn!(
                                                call_id = %data.call_id,
                                                tool = %data.name,
                                                status = ?data.status,
                                                "Ignoring duplicate or non-failing terminal event for tool call"
                                            );
                                            continue;
                                        }
                                    } else {
                                        tracking_overflow |= !remember_bounded(
                                            &mut terminal_tool_calls,
                                            data.call_id.clone(),
                                            "terminal_tool_call",
                                        );
                                        if data.status == ToolCallStatus::Error {
                                            tracking_overflow |= !remember_bounded(
                                                &mut failed_terminal_tool_calls,
                                                data.call_id.clone(),
                                                "failed_terminal_tool_call",
                                            );
                                        }
                                    }
                                    active_tool_calls.remove(&data.call_id);
                                    if has_artifact_delivery && artifact_contract_error.is_none() {
                                        tracking_overflow |= !track_bounded(
                                            &mut completed_artifact_tool_calls,
                                            data.call_id.clone(),
                                            data.clone(),
                                            "completed_artifact_tool_call",
                                        );
                                    } else {
                                        completed_artifact_tool_calls.remove(&data.call_id);
                                    }
                                }
                            }
                            if tracking_overflow {
                                active_tool_calls.remove(&data.call_id);
                                completed_artifact_tool_calls.remove(&data.call_id);
                                let mut failed = data.clone();
                                failed.status = ToolCallStatus::Error;
                                failed.artifacts.clear();
                                failed.output = Some(
                                    "The turn exceeded its safe tool-lifecycle tracking limit; artifact delivery was rejected"
                                        .to_owned(),
                                );
                                let failed_event = AgentStreamEvent::ToolCall(failed.clone());
                                self.forward_to_websocket(&failed_event);
                                let _ = self
                                    .bounded_event_side_effect(
                                        event_side_effect_deadline,
                                        "persist_tool_tracking_overflow",
                                        self.persist_tool_call(&failed),
                                    )
                                    .await;
                                fatal_tracking_error = Some(
                                    "The agent emitted more tool lifecycle events than can be verified safely; the turn was terminated"
                                        .to_owned(),
                                );
                                continue;
                            }
                            if let Some(contract_error) = artifact_contract_error {
                                completed_artifact_tool_calls.remove(&data.call_id);
                                let mut failed = data.clone();
                                failed.status = ToolCallStatus::Error;
                                failed.artifacts.clear();
                                failed.output = Some(contract_error.clone());
                                let failed_event = AgentStreamEvent::ToolCall(failed.clone());
                                self.forward_to_websocket(&failed_event);
                                let _ = self
                                    .bounded_event_side_effect(
                                        event_side_effect_deadline,
                                        "persist_artifact_contract_failure",
                                        self.persist_tool_call(&failed),
                                    )
                                    .await;
                                fatal_tracking_error = Some(format!(
                                    "Artifact delivery contract failed; the turn was terminated: {contract_error}"
                                ));
                                continue;
                            }
                            let _ = self
                                .bounded_event_side_effect(
                                    event_side_effect_deadline,
                                    "complete_thinking_before_tool",
                                    self.complete_active_thinking(&mut active_thinking),
                                )
                                .await;
                            let _ = self
                                .bounded_event_side_effect(
                                    event_side_effect_deadline,
                                    "close_text_before_tool",
                                    self.close_active_text_segment(
                                        &mut active_text,
                                        &mut text_segments,
                                        "finish",
                                    ),
                                )
                                .await;
                            if has_artifact_delivery {
                                let identity_ready = matches!(
                                    self.bounded_event_side_effect(
                                        event_side_effect_deadline,
                                        "claim_artifact_tool_identity",
                                        self.try_derived_message_id("tool_call", &data.call_id),
                                    )
                                    .await,
                                    Some(Ok(_))
                                );
                                if !identity_ready {
                                    completed_artifact_tool_calls.remove(&data.call_id);
                                    let mut failed = data.clone();
                                    failed.status = ToolCallStatus::Error;
                                    failed.artifacts.clear();
                                    failed.output = Some(
                                        "Artifact delivery could not claim a durable message identity"
                                            .to_owned(),
                                    );
                                    self.forward_to_websocket(&AgentStreamEvent::ToolCall(failed));
                                    fatal_tracking_error = Some(
                                        "Artifact delivery could not be projected durably; the turn was terminated"
                                            .to_owned(),
                                    );
                                    continue;
                                }

                                // Do not expose a green receipt before the
                                // enclosing turn commits. Live clients receive
                                // the same receipt-free provisional lifecycle as
                                // history hydration; the full Completed frame is
                                // published by the terminal commit barrier.
                                let provisional = Self::provisional_artifact_tool_call(data);
                                self.forward_to_websocket(&AgentStreamEvent::ToolCall(provisional));
                                let _ = self
                                    .bounded_event_side_effect(
                                        event_side_effect_deadline,
                                        "persist_provisional_artifact_tool_call",
                                        self.persist_provisional_artifact_tool_call(data),
                                    )
                                    .await;
                            } else {
                                self.forward_to_websocket(&event);
                                let _ = self
                                    .bounded_event_side_effect(
                                        event_side_effect_deadline,
                                        "persist_tool_call",
                                        self.persist_tool_call(data),
                                    )
                                    .await;
                            }
                        }
                        AgentStreamEvent::AcpToolCall(data) => {
                            // Plan D4: see ToolCall — an ACP tool call is a
                            // visible, side-effecting action; block failover.
                            emitted_response = true;
                            let tool_call_id = data.update.tool_call_id.clone();
                            let effective_data = effective_acp_tool_call_projection(
                                active_acp_tool_calls.get(&tool_call_id),
                                data,
                            );
                            let has_artifact_delivery = effective_data
                                .update
                                .content
                                .as_ref()
                                .is_some_and(|items| {
                                    items.iter().any(|item| {
                                        matches!(
                                            item,
                                            nomifun_ai_agent::protocol::events::AcpToolCallContentItem::Artifact { .. }
                                                | nomifun_ai_agent::protocol::events::AcpToolCallContentItem::ResourceLink { .. }
                                        )
                                    })
                                });
                            let artifact_contract_error = if effective_data.update.status
                                == Some(AcpToolCallStatus::Completed)
                            {
                                validate_completed_acp_artifact_contract(&effective_data).err()
                            } else {
                                None
                            };
                            let mut tracking_overflow = false;
                            match effective_data.update.status {
                                Some(AcpToolCallStatus::Completed | AcpToolCallStatus::Failed) => {
                                    if terminal_acp_tool_calls.contains(&tool_call_id) {
                                        if effective_data.update.status == Some(AcpToolCallStatus::Failed)
                                            && !failed_terminal_acp_tool_calls.contains(&tool_call_id)
                                        {
                                            tracking_overflow |= !remember_bounded(
                                                &mut failed_terminal_acp_tool_calls,
                                                tool_call_id.clone(),
                                                "failed_terminal_acp_tool_call",
                                            );
                                        } else {
                                            warn!(
                                                tool_call_id,
                                                status = ?effective_data.update.status,
                                                "Ignoring duplicate or non-failing terminal ACP tool event"
                                            );
                                            continue;
                                        }
                                    } else {
                                        tracking_overflow |= !remember_bounded(
                                            &mut terminal_acp_tool_calls,
                                            tool_call_id.clone(),
                                            "terminal_acp_tool_call",
                                        );
                                        if effective_data.update.status == Some(AcpToolCallStatus::Failed) {
                                            tracking_overflow |= !remember_bounded(
                                                &mut failed_terminal_acp_tool_calls,
                                                tool_call_id.clone(),
                                                "failed_terminal_acp_tool_call",
                                            );
                                        }
                                    }
                                    active_acp_tool_calls.remove(&tool_call_id);
                                    if effective_data.update.status == Some(AcpToolCallStatus::Completed)
                                        && has_artifact_delivery
                                        && artifact_contract_error.is_none()
                                    {
                                        tracking_overflow |= !track_bounded(
                                            &mut completed_artifact_acp_tool_calls,
                                            tool_call_id.clone(),
                                            effective_data.clone(),
                                            "completed_artifact_acp_tool_call",
                                        );
                                    } else {
                                        completed_artifact_acp_tool_calls.remove(&tool_call_id);
                                    }
                                }
                                Some(AcpToolCallStatus::Pending | AcpToolCallStatus::InProgress) | None => {
                                    if terminal_acp_tool_calls.contains(&tool_call_id) {
                                        warn!(
                                            tool_call_id,
                                            "Ignoring late progress event for terminal ACP tool call"
                                        );
                                        continue;
                                    }
                                    tracking_overflow |= !track_bounded(
                                        &mut active_acp_tool_calls,
                                        tool_call_id.clone(),
                                        effective_data.clone(),
                                        "acp_tool_call",
                                    );
                                }
                            }
                            if tracking_overflow {
                                active_acp_tool_calls.remove(&tool_call_id);
                                completed_artifact_acp_tool_calls.remove(&tool_call_id);
                                let mut failed = effective_data.clone();
                                failed.update.session_update = AcpToolCallSessionUpdateKind::ToolCallUpdate;
                                failed.update.status = Some(AcpToolCallStatus::Failed);
                                failed.update.raw_output = Some(json!(
                                    "The turn exceeded its safe tool-lifecycle tracking limit; artifact delivery was rejected"
                                ));
                                if let Some(content) = failed.update.content.as_mut() {
                                    content.retain(|item| {
                                        !matches!(
                                            item,
                                            nomifun_ai_agent::protocol::events::AcpToolCallContentItem::Artifact { .. }
                                                | nomifun_ai_agent::protocol::events::AcpToolCallContentItem::ResourceLink { .. }
                                        )
                                    });
                                }
                                let failed_event = AgentStreamEvent::AcpToolCall(failed.clone());
                                self.forward_to_websocket(&failed_event);
                                let _ = self
                                    .bounded_event_side_effect(
                                        event_side_effect_deadline,
                                        "persist_acp_tracking_overflow",
                                        self.persist_acp_tool_call(&failed),
                                    )
                                    .await;
                                fatal_tracking_error = Some(
                                    "The agent emitted more ACP tool lifecycle events than can be verified safely; the turn was terminated"
                                        .to_owned(),
                                );
                                continue;
                            }
                            if let Some(contract_error) = artifact_contract_error {
                                completed_artifact_acp_tool_calls.remove(&tool_call_id);
                                let mut failed = effective_data.clone();
                                failed.update.session_update =
                                    AcpToolCallSessionUpdateKind::ToolCallUpdate;
                                failed.update.status = Some(AcpToolCallStatus::Failed);
                                failed.update.raw_output = Some(json!(contract_error.clone()));
                                if let Some(content) = failed.update.content.as_mut() {
                                    content.retain(|item| {
                                        !matches!(
                                            item,
                                            nomifun_ai_agent::protocol::events::AcpToolCallContentItem::Artifact { .. }
                                                | nomifun_ai_agent::protocol::events::AcpToolCallContentItem::ResourceLink { .. }
                                        )
                                    });
                                }
                                let failed_event = AgentStreamEvent::AcpToolCall(failed.clone());
                                self.forward_to_websocket(&failed_event);
                                let _ = self
                                    .bounded_event_side_effect(
                                        event_side_effect_deadline,
                                        "persist_acp_artifact_contract_failure",
                                        self.persist_acp_tool_call(&failed),
                                    )
                                    .await;
                                fatal_tracking_error = Some(format!(
                                    "ACP artifact delivery contract failed; the turn was terminated: {contract_error}"
                                ));
                                continue;
                            }
                            let _ = self
                                .bounded_event_side_effect(
                                    event_side_effect_deadline,
                                    "complete_thinking_before_acp_tool",
                                    self.complete_active_thinking(&mut active_thinking),
                                )
                                .await;
                            let _ = self
                                .bounded_event_side_effect(
                                    event_side_effect_deadline,
                                    "close_text_before_acp_tool",
                                    self.close_active_text_segment(
                                        &mut active_text,
                                        &mut text_segments,
                                        "finish",
                                    ),
                                )
                                .await;
                            if effective_data.update.status == Some(AcpToolCallStatus::Completed)
                                && has_artifact_delivery
                            {
                                let identity_ready = matches!(
                                    self.bounded_event_side_effect(
                                        event_side_effect_deadline,
                                        "claim_artifact_acp_tool_identity",
                                        self.try_derived_message_id(
                                            "acp_tool_call",
                                            &effective_data.update.tool_call_id,
                                        ),
                                    )
                                    .await,
                                    Some(Ok(_))
                                );
                                if !identity_ready {
                                    completed_artifact_acp_tool_calls.remove(&tool_call_id);
                                    let mut failed = effective_data.clone();
                                    failed.update.session_update =
                                        AcpToolCallSessionUpdateKind::ToolCallUpdate;
                                    failed.update.status = Some(AcpToolCallStatus::Failed);
                                    failed.update.raw_output = Some(json!(
                                        "Artifact delivery could not claim a durable message identity"
                                    ));
                                    if let Some(content) = failed.update.content.as_mut() {
                                        content.retain(|item| {
                                            !matches!(
                                                item,
                                                nomifun_ai_agent::protocol::events::AcpToolCallContentItem::Artifact { .. }
                                                    | nomifun_ai_agent::protocol::events::AcpToolCallContentItem::ResourceLink { .. }
                                            )
                                        });
                                    }
                                    self.forward_to_websocket(&AgentStreamEvent::AcpToolCall(failed));
                                    fatal_tracking_error = Some(
                                        "ACP artifact delivery could not be projected durably; the turn was terminated"
                                            .to_owned(),
                                    );
                                    continue;
                                }

                                let provisional =
                                    Self::provisional_artifact_acp_tool_call(&effective_data);
                                self.forward_to_websocket(&AgentStreamEvent::AcpToolCall(provisional));
                                let _ = self
                                    .bounded_event_side_effect(
                                        event_side_effect_deadline,
                                        "persist_provisional_artifact_acp_tool_call",
                                        self.persist_provisional_artifact_acp_tool_call(
                                            &effective_data,
                                        ),
                                    )
                                    .await;
                            } else {
                                self.forward_to_websocket(&AgentStreamEvent::AcpToolCall(
                                    effective_data.clone(),
                                ));
                                let _ = self
                                    .bounded_event_side_effect(
                                        event_side_effect_deadline,
                                        "persist_acp_tool_call",
                                        self.persist_acp_tool_call(&effective_data),
                                    )
                                    .await;
                            }
                        }
                        AgentStreamEvent::ToolGroup(entries) => {
                            // Plan D4: see ToolCall — a tool group is a visible,
                            // side-effecting action; block failover.
                            emitted_response = true;
                            let artifact_contract_errors = tool_group_artifact_contract_errors(
                                entries,
                                &completed_artifact_tool_calls,
                            );
                            if !artifact_contract_errors.is_empty() {
                                let mut failed = entries.clone();
                                let mut reasons = Vec::with_capacity(artifact_contract_errors.len());
                                for (index, contract_error) in artifact_contract_errors {
                                    if let Some(entry) = failed.get_mut(index) {
                                        entry.status = ToolCallStatus::Error;
                                        entry.description = Some(contract_error.clone());
                                    }
                                    reasons.push(contract_error);
                                }
                                if let Some(group_id) = failed.first().map(|entry| &entry.call_id) {
                                    active_tool_groups.remove(group_id);
                                }
                                let failed_event = AgentStreamEvent::ToolGroup(failed.clone());
                                self.forward_to_websocket(&failed_event);
                                let _ = self
                                    .bounded_event_side_effect(
                                        event_side_effect_deadline,
                                        "persist_tool_group_artifact_contract_failure",
                                        self.persist_tool_group(&failed),
                                    )
                                    .await;
                                fatal_tracking_error = Some(format!(
                                    "Tool-group artifact delivery contract failed; the turn was terminated: {}",
                                    reasons.join("; ")
                                ));
                                continue;
                            }
                            // ToolGroupEntry cannot carry a receipt or 2PC
                            // marker, so it can never be an authoritative
                            // artifact-success carrier. Suppress high-signal
                            // entries and rely on their detailed ToolCall row;
                            // retain unrelated summaries from a mixed group.
                            let visible_entries = entries
                                .iter()
                                .filter(|entry| !tool_group_entry_has_artifact_contract(entry))
                                .cloned()
                                .collect::<Vec<_>>();
                            let entries = visible_entries.as_slice();
                            if entries.is_empty() {
                                continue;
                            }
                            if let Some(group_id) = entries.first().map(|entry| entry.call_id.clone()) {
                                if entries.iter().any(|entry| entry.status == ToolCallStatus::Running) {
                                    let mut tracked_entries = entries.to_vec();
                                    tracked_entries.truncate(MAX_TERMINAL_ACTIVE_ITEMS);
                                    track_bounded(
                                        &mut active_tool_groups,
                                        group_id,
                                        tracked_entries,
                                        "tool_group",
                                    );
                                } else {
                                    active_tool_groups.remove(&group_id);
                                }
                            }
                            let _ = self
                                .bounded_event_side_effect(
                                    event_side_effect_deadline,
                                    "complete_thinking_before_tool_group",
                                    self.complete_active_thinking(&mut active_thinking),
                                )
                                .await;
                            let _ = self
                                .bounded_event_side_effect(
                                    event_side_effect_deadline,
                                    "close_text_before_tool_group",
                                    self.close_active_text_segment(
                                        &mut active_text,
                                        &mut text_segments,
                                        "finish",
                                    ),
                                )
                                .await;
                            self.forward_to_websocket(&AgentStreamEvent::ToolGroup(entries.to_vec()));
                            let _ = self
                                .bounded_event_side_effect(
                                    event_side_effect_deadline,
                                    "persist_tool_group",
                                    self.persist_tool_group(entries),
                                )
                                .await;
                        }
                        AgentStreamEvent::AgentStatus(data) => {
                            self.forward_to_websocket(&event);
                            if data.backend == "nomi" && (data.status == "preparing" || data.status == "prepared") {
                                active_agent_status = Some(data.clone());
                                let persisted = self
                                    .bounded_event_side_effect(
                                        event_side_effect_deadline,
                                        "persist_agent_status",
                                        self.persist_agent_status(data),
                                    )
                                    .await;
                                if data.status == "prepared" && persisted == Some(true) {
                                    active_agent_status = None;
                                }
                            }
                        }
                        AgentStreamEvent::Plan(data) => {
                            emitted_response = true;
                            let _ = self
                                .bounded_event_side_effect(
                                    event_side_effect_deadline,
                                    "complete_thinking_before_plan",
                                    self.complete_active_thinking(&mut active_thinking),
                                )
                                .await;
                            let _ = self
                                .bounded_event_side_effect(
                                    event_side_effect_deadline,
                                    "close_text_before_plan",
                                    self.close_active_text_segment(
                                        &mut active_text,
                                        &mut text_segments,
                                        "finish",
                                    ),
                                )
                                .await;
                            if let Some(source_call_id) = data.source_call_id.as_deref() {
                                let mut source = active_tool_calls.remove(source_call_id).unwrap_or_else(|| {
                                    ToolCallEventData {
                                        call_id: source_call_id.to_owned(),
                                        name: "update_plan".to_owned(),
                                        args: serde_json::Value::Null,
                                        status: ToolCallStatus::Running,
                                        input: None,
                                        output: None,
                                        description: None,
                                        artifacts: Vec::new(),
                                    }
                                });
                                source.status = ToolCallStatus::Completed;
                                source.output = Some("Plan updated".to_owned());
                                remember_bounded(
                                    &mut terminal_tool_calls,
                                    source_call_id.to_owned(),
                                    "terminal_tool_call",
                                );
                                let source_event = AgentStreamEvent::ToolCall(source.clone());
                                self.forward_to_websocket_hidden(&source_event);
                                let _ = self
                                    .bounded_event_side_effect(
                                        event_side_effect_deadline,
                                        "persist_plan_source_tool",
                                        self.persist_tool_call_with_hidden(&source, true),
                                    )
                                    .await;
                            }
                            let plan_id = self
                                .bounded_event_side_effect(
                                    event_side_effect_deadline,
                                    "resolve_plan_message_id",
                                    self.plan_message_id(data),
                                )
                                .await
                                .unwrap_or_else(|| {
                                    Self::mint_segment_msg_id(
                                        &mut used_primary_segment_msg_id,
                                        &self.msg_id,
                                    )
                                });
                            if data.entries.iter().all(|entry| {
                                entry.get("status").and_then(serde_json::Value::as_str) == Some("completed")
                            }) {
                                active_plan_ids.remove(&plan_id);
                            } else {
                                remember_bounded(
                                    &mut active_plan_ids,
                                    plan_id.clone(),
                                    "active_plan",
                                );
                            }
                            self.forward_to_websocket_with_msg_id(&plan_id, &event);
                            let _ = self
                                .bounded_event_side_effect(
                                    event_side_effect_deadline,
                                    "persist_plan",
                                    self.persist_plan(data),
                                )
                                .await;
                        }
                        AgentStreamEvent::TurnCompleted(metrics) => {
                            // Accumulate this turn's token usage into the owning
                            // execution attempt's conversation total. The caller
                            // already validated the explicit active relation.
                            // `context_tokens` is a gauge (last-request occupancy), so
                            // per-turn COST is the additive `input + output`. Recorded
                            // BEFORE the turn handle releases, so the polling attempt
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

                    let mut terminal_event = if self
                        .cancellation
                        .as_ref()
                        .is_some_and(AgentTurnCancellation::is_cancelled)
                    {
                        Self::cancelled_finish_event()
                    } else {
                        AgentStreamEvent::Error(
                            nomifun_ai_agent::protocol::events::ErrorEventData::legacy(
                                "Agent event channel closed before the turn completed",
                                None,
                            ),
                        )
                    };
                    if self
                        .cancellation
                        .as_ref()
                        .is_some_and(AgentTurnCancellation::is_cancelled)
                    {
                        terminal_event = Self::cancelled_finish_event();
                    }
                    let terminal_claimed = self
                        .cancellation
                        .as_ref()
                        .map(AgentTurnCancellation::try_claim_terminal_surface)
                        .unwrap_or(true);
                    let mut terminal = if Self::is_cancelled_finish(&terminal_event) {
                        RelayTerminal::Finish
                    } else {
                        RelayTerminal::ChannelClosed
                    };
                    let mut terminal_message_id = if matches!(terminal_event, AgentStreamEvent::Error(_)) {
                        ConversationService::mint_msg_id()
                    } else {
                        self.msg_id.clone()
                    };
                    let terminal_cleanup = async {
                        let incomplete_reason = if Self::is_cancelled_finish(&terminal_event) {
                            "cancelled"
                        } else {
                            "channel_closed"
                        };
                        let failed_completed_tools = Self::take_failed_tool_calls(
                            &mut completed_artifact_tool_calls,
                            incomplete_reason,
                        );
                        let failed_completed_acp_tools = Self::take_failed_acp_tool_calls(
                            &mut completed_artifact_acp_tool_calls,
                            incomplete_reason,
                        );
                        self.broadcast_failed_tool_calls(&failed_completed_tools);
                        self.broadcast_failed_acp_tool_calls(&failed_completed_acp_tools);
                        let _ = tokio::join!(
                            self.persist_failed_tool_calls(&failed_completed_tools),
                            self.persist_failed_acp_tool_calls(&failed_completed_acp_tools),
                        );
                        let thinking_persistence_complete = self
                            .complete_active_thinking(&mut active_thinking)
                            .await;
                        let thinking_persistence_complete = if thinking_persistence_complete {
                            true
                        } else {
                            self.retry_terminal_thinking_segment(&mut active_thinking)
                                .await
                        };
                        self.close_active_text_segment(
                            &mut active_text,
                            &mut text_segments,
                            "error",
                        )
                        .await;
                        self.fail_active_tool_calls(&mut active_tool_calls, incomplete_reason).await;
                        self.fail_active_acp_tool_calls(&mut active_acp_tool_calls, incomplete_reason)
                            .await;
                        self.fail_active_tool_groups(&mut active_tool_groups, incomplete_reason)
                            .await;
                        self.finalize_active_plans(
                            &mut active_plan_ids,
                            Self::plan_terminal_status(&terminal_event),
                        )
                        .await;
                        self.finalize_active_agent_status(
                            &mut active_agent_status,
                            Self::plan_terminal_status(&terminal_event),
                        )
                        .await;
                        let text_persistence_complete = self
                            .retry_terminal_text_segment(
                                &mut active_text,
                                &mut text_segments,
                                "error",
                            )
                            .await;
                        if (!thinking_persistence_complete || !text_persistence_complete)
                            && matches!(terminal_event, AgentStreamEvent::Finish(_))
                        {
                            terminal_event = Self::assistant_segment_persistence_error_event();
                            terminal = Self::terminal_from_event(&terminal_event);
                            terminal_message_id = ConversationService::mint_msg_id();
                        }
                        let outcome = self
                            .finalize(
                                &full_text_buffer,
                                &text_segments,
                                text_persistence_complete,
                                &terminal_event,
                                terminal,
                                emitted_response,
                                // A channel-closed terminal is never a
                                // suppressible provider failure.
                                false,
                                &terminal_message_id,
                            )
                            .await;
                        if terminal_claimed {
                            self.forward_to_websocket_with_msg_id(&terminal_message_id, &terminal_event);
                        }
                        outcome
                    };
                    let outcome = match tokio::time::timeout(
                        TERMINAL_FINALIZATION_GRACE,
                        terminal_cleanup,
                    )
                    .await
                    {
                        Ok(outcome) => outcome,
                        Err(_) => {
                            warn!(
                                conversation_id = %self.conversation_id,
                                msg_id = %self.msg_id,
                                "Channel-closed relay finalization exceeded the hard bound"
                            );
                            let thinking_persistence_complete = active_thinking.is_none();
                            let text_persistence_complete = active_text.is_none();
                            if (!thinking_persistence_complete || !text_persistence_complete)
                                && matches!(terminal_event, AgentStreamEvent::Finish(_))
                            {
                                terminal_event = Self::assistant_segment_persistence_error_event();
                                terminal_message_id = ConversationService::mint_msg_id();
                            }
                            if terminal_claimed {
                                self.forward_to_websocket_with_msg_id(&terminal_message_id, &terminal_event);
                            }
                            RelayOutcome {
                                system_responses: Vec::new(),
                                terminal: Self::terminal_from_event(&terminal_event),
                                stop_reason: match &terminal_event {
                                    AgentStreamEvent::Finish(data) => data.stop_reason,
                                    _ => None,
                                },
                                emitted_response,
                                suppressed_error: None,
                                final_text: (text_persistence_complete
                                    && !full_text_buffer.trim().is_empty())
                                    .then(|| full_text_buffer.trim().to_owned()),
                                final_text_msg_id: text_persistence_complete
                                    .then(|| text_segments.last().map(|segment| segment.id.clone()))
                                    .flatten(),
                            }
                        }
                    };
                    if terminal_claimed
                        && let Some(cancellation) = self.cancellation.as_ref()
                    {
                        cancellation.mark_terminal_observed();
                    }
                    if self.complete_turn {
                        Self::complete_conversation_with_context(
                            &self.repo,
                            &self.user_events,
                            &self.user_id,
                            &self.conversation_id,
                            Some(self.root_turn_id.clone()),
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
                Err(broadcast::error::RecvError::Lagged(_)) => {
                    unreachable!("lagged receive results are normalized to terminal errors")
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

    fn cancelled_finish_event() -> AgentStreamEvent {
        AgentStreamEvent::Finish(FinishEventData {
            session_id: None,
            stop_reason: Some(TurnStopReason::Cancelled),
        })
    }

    fn assistant_segment_persistence_error_event() -> AgentStreamEvent {
        AgentStreamEvent::Error(
            nomifun_ai_agent::protocol::events::ErrorEventData::legacy(
                "The assistant response could not be fully saved to conversation history",
                Some(AgentErrorCode::NomifunStateInconsistent),
            ),
        )
    }

    fn is_cancelled_finish(event: &AgentStreamEvent) -> bool {
        matches!(
            event,
            AgentStreamEvent::Finish(FinishEventData {
                stop_reason: Some(TurnStopReason::Cancelled),
                ..
            })
        )
    }

    /// Publish the bounded stop fallback when no backend/relay terminal was
    /// observed. The generation snapshot arbitrates the single publisher, so
    /// a late backend acknowledgement cannot duplicate the cancelled Finish.
    pub(crate) fn surface_cancelled_turn(
        &self,
        cancellation: &AgentTurnCancellation,
    ) -> bool {
        if !cancellation.try_claim_terminal_surface() {
            return false;
        }
        self.forward_to_websocket(&Self::cancelled_finish_event());
        cancellation.mark_terminal_observed();
        true
    }

    fn mint_segment_msg_id(used_primary: &mut bool, primary_msg_id: &str) -> String {
        if !*used_primary {
            *used_primary = true;
            primary_msg_id.to_owned()
        } else {
            ConversationService::mint_msg_id()
        }
    }

    /// The canonical Conversation ID used by repository calls and events.
    fn conv_id(&self) -> &str {
        &self.conversation_id
    }

    /// Forward an agent event to connected WebSocket clients.
    #[tracing::instrument(skip_all)]
    fn forward_to_websocket(&self, event: &AgentStreamEvent) {
        self.forward_to_websocket_with_msg_id(&self.msg_id, event);
    }

    fn forward_to_websocket_hidden(&self, event: &AgentStreamEvent) {
        self.forward_to_websocket_with_msg_id_and_visibility(&self.msg_id, event, true);
    }

    #[tracing::instrument(skip_all)]
    fn forward_to_websocket_with_msg_id(&self, msg_id: &str, event: &AgentStreamEvent) {
        self.forward_to_websocket_with_msg_id_and_visibility(msg_id, event, false);
    }

    fn forward_to_websocket_with_msg_id_and_visibility(
        &self,
        msg_id: &str,
        event: &AgentStreamEvent,
        hidden: bool,
    ) {
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
            "hidden": hidden,
        });

        self.broadcast_stream_payload(payload);
    }

    /// Insert a streamed assistant row, reconciling the cancellation-ambiguous case
    /// where SQLite committed the INSERT but its future returned an error (or a
    /// previous timed-out attempt was dropped before the caller observed it).
    /// We do not classify the insert error: SQLite uniqueness failures arrive as
    /// `DbError::Query`, and transport/executor errors can be ambiguous too.
    async fn insert_stream_message_with_reconciliation(
        &self,
        row: &MessageRow,
        operation: &'static str,
    ) -> bool {
        let insert_error = match self.repo.insert_message(row).await {
            Ok(()) => return true,
            Err(error) => error,
        };

        let existing = match self.repo.get_message(&row.conversation_id, &row.id).await {
            Ok(Some(existing)) => existing,
            Ok(None) => {
                error!(
                    error = %ErrorChain(&insert_error),
                    operation,
                    message_id = %row.id,
                    "Failed to insert stream segment and no committed row was found to reconcile"
                );
                return false;
            }
            Err(reconcile_error) => {
                error!(
                    error = %ErrorChain(&insert_error),
                    reconcile_error = %ErrorChain(&reconcile_error),
                    operation,
                    message_id = %row.id,
                    "Failed to inspect an ambiguous stream-segment insert"
                );
                return false;
            }
        };

        // IDs are globally canonical, but still fail closed before updating an
        // existing row: a collision must never overwrite another message type
        // or turn. get_message already scopes this lookup to the conversation.
        if existing.conversation_id != row.conversation_id
            || existing.r#type != row.r#type
            || existing.msg_id != row.msg_id
        {
            error!(
                error = %ErrorChain(&insert_error),
                operation,
                message_id = %row.id,
                stored_type = %existing.r#type,
                expected_type = %row.r#type,
                stored_msg_id = ?existing.msg_id,
                expected_msg_id = ?row.msg_id,
                "Refusing to reconcile an ambiguous stream insert with an incompatible row"
            );
            return false;
        }

        let update = MessageRowUpdate {
            content: Some(row.content.clone()),
            status: Some(row.status.clone()),
            hidden: Some(row.hidden),
        };
        match self.repo.update_message(&row.id, &update).await {
            Ok(()) => {
                warn!(
                    error = %ErrorChain(&insert_error),
                    operation,
                    message_id = %row.id,
                    "Reconciled an ambiguous stream-segment insert against its committed row"
                );
                true
            }
            Err(reconcile_error) => {
                error!(
                    error = %ErrorChain(&insert_error),
                    reconcile_error = %ErrorChain(&reconcile_error),
                    operation,
                    message_id = %row.id,
                    "Failed to reconcile an ambiguous stream-segment insert"
                );
                false
            }
        }
    }

    /// Flush an active text segment to the database (create or update).
    #[tracing::instrument(skip_all)]
    async fn flush_text_segment(&self, segment: &mut TextSegmentState) {
        if segment.buffer.is_empty() {
            return;
        }

        let content = json!({
            "content": segment.buffer,
            "turn_id": &self.root_turn_id,
        })
        .to_string();

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
                conversation_id: self.conversation_id.clone(),
                msg_id: Some(segment.id.clone()),
                r#type: "text".into(),
                content,
                position: Some("left".into()),
                status: Some("work".into()),
                hidden: false,
                created_at: segment.created_at,
            };
            if self
                .insert_stream_message_with_reconciliation(&row, "create_streaming_text")
                .await
            {
                segment.record_created = true;
            }
        }
    }

    #[tracing::instrument(skip_all)]
    async fn finalize_text_segment(
        &self,
        segment: &TextSegmentState,
        status: &str,
    ) -> Option<PersistedTextSegment> {
        if segment.buffer.is_empty() {
            return None;
        }

        let content = json!({
            "content": segment.buffer,
            "turn_id": &self.root_turn_id,
        })
        .to_string();
        if segment.record_created {
            let update = nomifun_db::MessageRowUpdate {
                content: Some(content),
                status: Some(Some(status.to_owned())),
                hidden: Some(false),
            };
            if let Err(e) = self.repo.update_message(&segment.id, &update).await {
                error!(error = %ErrorChain(&e), "Failed to finalize text segment");
                return None;
            }
        } else {
            let row = MessageRow {
                id: segment.id.clone(),
                conversation_id: self.conversation_id.clone(),
                msg_id: Some(segment.id.clone()),
                r#type: "text".into(),
                content,
                position: Some("left".into()),
                status: Some(status.to_owned()),
                hidden: false,
                created_at: segment.created_at,
            };
            if !self
                .insert_stream_message_with_reconciliation(&row, "create_finalized_text")
                .await
            {
                return None;
            }
        }

        Some(PersistedTextSegment {
            id: segment.id.clone(),
        })
    }

    /// Finalize assistant text on stream end and apply middleware rewrites.
    #[tracing::instrument(skip_all)]
    async fn finalize(
        &self,
        text: &str,
        text_segments: &[PersistedTextSegment],
        text_persistence_complete: bool,
        event: &AgentStreamEvent,
        terminal: RelayTerminal,
        emitted_response: bool,
        suppress_error: bool,
        terminal_message_id: &str,
    ) -> RelayOutcome {
        let mut outcome = RelayOutcome {
            system_responses: Vec::new(),
            terminal,
            stop_reason: match event {
                AgentStreamEvent::Finish(data) => data.stop_reason,
                _ => None,
            },
            emitted_response,
            suppressed_error: None,
            final_text: None,
            final_text_msg_id: None,
        };
        let cancelled = Self::is_cancelled_finish(event);
        let status = if matches!(event, AgentStreamEvent::Error(_)) || cancelled {
            "error"
        } else {
            "finish"
        };

        // Error is a first-class terminal record regardless of whether the
        // provider emitted partial text first. Persisting it only for empty
        // turns left the live Error frame unmatched after history hydration;
        // the renderer then carried that orphan into later turns. The error
        // message has its own canonical identity and an explicit owning turn.
        if let AgentStreamEvent::Error(data) = event
            && !suppress_error
        {
            self.persist_error_tips(terminal_message_id, data).await;
        }

        if !text.is_empty() {
            if !text_persistence_complete {
                error!(
                    conversation_id = %self.conversation_id,
                    msg_id = %self.msg_id,
                    "Assistant text terminal persistence failed after its bounded retry"
                );
                return outcome;
            }
            let processed = if cancelled {
                // A cancelled partial response is data to preserve, never a
                // completed instruction stream. In particular, do not execute
                // embedded cron commands or produce continuation responses.
                MiddlewareResult {
                    message: text.to_owned(),
                    display_message: None,
                    system_responses: Vec::new(),
                }
            } else {
                self.process_final_text(text).await
            };
            let final_text = processed.message.trim().to_owned();
            let hidden = final_text.is_empty();
            if !hidden {
                outcome.final_text = Some(final_text.clone());
            }

            if let Some(primary_segment) = text_segments.first() {
                if processed.message != text || hidden {
                    let content = json!({
                        "content": &final_text,
                        "turn_id": &self.root_turn_id,
                    })
                    .to_string();
                    let update = nomifun_db::MessageRowUpdate {
                        content: Some(content),
                        status: Some(Some(status.to_owned())),
                        hidden: Some(hidden),
                    };
                    match self.repo.update_message(&primary_segment.id, &update).await {
                        Ok(()) => {
                            self.send_final_text_override(&primary_segment.id, &final_text, hidden);

                            let mut all_superseded_hidden = true;
                            for segment in text_segments.iter().skip(1) {
                                let hide_update = nomifun_db::MessageRowUpdate {
                                    content: None,
                                    status: None,
                                    hidden: Some(true),
                                };
                                match self.repo.update_message(&segment.id, &hide_update).await {
                                    Ok(()) => self.send_final_text_override(&segment.id, "", true),
                                    Err(e) => {
                                        all_superseded_hidden = false;
                                        error!(error = %ErrorChain(&e), "Failed to hide superseded text segment");
                                    }
                                }
                            }
                            if all_superseded_hidden {
                                if !hidden {
                                    outcome.final_text_msg_id = Some(primary_segment.id.clone());
                                }
                            } else {
                                // Every emitted override now reflects an
                                // acknowledged row update, but a partial
                                // multi-row rewrite is not a coherent target
                                // for turn-final writeback.
                                outcome.final_text = None;
                            }
                        }
                        Err(e) => {
                            // The raw streamed segments are already durable.
                            // Keep the live UI on that same raw representation
                            // and do not claim that the middleware projection
                            // was persisted.
                            outcome.final_text = None;
                            error!(error = %ErrorChain(&e), "Failed to rewrite finalized text segment");
                        }
                    }
                } else {
                    outcome.final_text_msg_id = text_segments.last().map(|segment| segment.id.clone());
                    // Each segment was finalized at its own boundary. Preserve
                    // those statuses: a later provider failure belongs only to
                    // the active segment and must not rewrite earlier narration.
                }
            } else if !hidden {
                let row = MessageRow {
                    id: self.msg_id.clone(),
                    conversation_id: self.conversation_id.clone(),
                    msg_id: Some(self.msg_id.clone()),
                    r#type: "text".into(),
                    content: json!({
                        "content": final_text,
                        "turn_id": &self.root_turn_id,
                    })
                    .to_string(),
                    position: Some("left".into()),
                    status: Some(status.to_owned()),
                    hidden: false,
                    created_at: now_ms(),
                };
                match self.repo.insert_message(&row).await {
                    Ok(()) => outcome.final_text_msg_id = Some(row.id.clone()),
                    Err(e) => {
                        outcome.final_text = None;
                        error!(error = %ErrorChain(&e), "Failed to create final fallback message");
                    }
                }
            }

            self.send_system_responses(&processed.system_responses);
            outcome.system_responses = processed.system_responses;
        } else if matches!(event, AgentStreamEvent::Error(_)) {
            if suppress_error {
                // review #1/#5: the send loop will (try to) fail over this
                // pre-response fault — do NOT persist the error tips row. Hand the
                // event back so the loop can re-surface it if the failover misses
                // (picker found no candidate), keeping queue-exhausted → original error.
                outcome.suppressed_error = Some(event.clone());
                return outcome;
            }
        }

        outcome
    }

    /// Persist a terminal provider error as a `tips` message row (the "no text,
    /// got error" surface). Factored out so [`Self::surface_terminal_error`] can
    /// re-persist a previously-suppressed error on a missed failover (review #1/#5).
    async fn persist_error_tips(
        &self,
        message_id: &str,
        data: &nomifun_ai_agent::protocol::events::ErrorEventData,
    ) {
        let content = json!({
            "content": &data.message,
            "type": "error",
            "error": &data,
            "turn_id": &self.root_turn_id,
        })
        .to_string();
        let row = MessageRow {
            id: message_id.to_owned(),
            conversation_id: self.conversation_id.clone(),
            msg_id: Some(message_id.to_owned()),
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
    async fn persist_agent_status(
        &self,
        data: &nomifun_ai_agent::protocol::events::AgentStatusEventData,
    ) -> bool {
        let id = self.agent_status_message_id().await;
        let content = serde_json::to_string(data).unwrap_or_else(|_| "{}".to_owned());
        let status = match data.status.as_str() {
            "prepared" => "finish",
            "error" => "error",
            _ => "work",
        };
        let existing = match self.repo.get_message(self.conv_id(), &id).await {
            Ok(existing) => existing,
            Err(e) => {
                error!(
                    status = %data.status,
                    error = %ErrorChain(&e),
                    "Failed to load agent_status message"
                );
                return false;
            }
        };

        if existing.is_some() {
            let update = nomifun_db::MessageRowUpdate {
                content: Some(content),
                status: Some(Some(status.to_owned())),
                hidden: Some(false),
            };
            return match self.repo.update_message(&id, &update).await {
                Ok(()) => true,
                Err(e) => {
                    error!(
                        status = %data.status,
                        error = %ErrorChain(&e),
                        "Failed to update agent_status message"
                    );
                    false
                }
            };
        }

        let row = MessageRow {
            id: id.clone(),
            conversation_id: self.conversation_id.clone(),
            msg_id: Some(self.root_turn_id.clone()),
            r#type: "agent_status".into(),
            content,
            position: Some("left".into()),
            status: Some(status.into()),
            hidden: false,
            created_at: now_ms(),
        };
        self.insert_stream_message_with_reconciliation(&row, "persist_agent_status")
            .await
    }

    async fn agent_status_message_id(&self) -> String {
        self.derived_message_id("agent_status", "model_activity").await
    }

    async fn finalize_active_agent_status(
        &self,
        active_status: &mut Option<nomifun_ai_agent::protocol::events::AgentStatusEventData>,
        terminal_status: &str,
    ) -> bool {
        let Some(current) = active_status.as_ref() else {
            return true;
        };
        let final_status = if terminal_status == "finish" {
            "prepared"
        } else {
            "error"
        };
        let should_forward = current.status != final_status;
        let mut data = current.clone();
        data.status = final_status.to_owned();

        if !self.persist_agent_status(&data).await {
            return false;
        }

        if should_forward {
            self.forward_to_websocket(&AgentStreamEvent::AgentStatus(data));
        }
        *active_status = None;
        true
    }

    fn plan_session_id(&self, data: &PlanEventData) -> String {
        data.session_id
            .as_deref()
            .map(str::trim)
            .filter(|session_id| !session_id.is_empty())
            .unwrap_or(&self.root_turn_id)
            .to_owned()
    }

    async fn plan_message_id(&self, data: &PlanEventData) -> String {
        self.derived_message_id("plan", &self.plan_session_id(data)).await
    }

    #[tracing::instrument(skip_all)]
    async fn persist_plan(&self, data: &PlanEventData) {
        let plan_id = self.plan_message_id(data).await;
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
            conversation_id: self.conversation_id.clone(),
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
    async fn complete_active_thinking(
        &self,
        active_thinking: &mut Option<ThinkingSegmentState>,
    ) -> bool {
        let Some(segment) = active_thinking.as_mut() else {
            return true;
        };

        let duration_ms = match segment.completed_duration_ms {
            Some(duration_ms) => duration_ms,
            None => {
                let duration_ms = (now_ms() - segment.started_at).max(0) as u64;
                segment.completed_duration_ms = Some(duration_ms);
                self.send_thinking_done(&segment.id, duration_ms);
                duration_ms
            }
        };
        if segment.buffer.is_empty() {
            *active_thinking = None;
            return true;
        }

        let row = MessageRow {
            id: segment.id.clone(),
            conversation_id: self.conversation_id.clone(),
            msg_id: Some(segment.id.clone()),
            r#type: "thinking".into(),
            content: json!({
                "content": segment.buffer,
                "status": "done",
                "duration_ms": duration_ms,
            })
            .to_string(),
            position: Some("left".into()),
            status: Some("finish".into()),
            hidden: false,
            created_at: segment.started_at,
        };
        let persisted = self
            .insert_stream_message_with_reconciliation(&row, "complete_thinking")
            .await;
        if persisted {
            *active_thinking = None;
        }
        persisted
    }

    /// Retry a terminal thinking write once. The state remains owned by
    /// `active_thinking` until the repository acknowledges it, so cancellation
    /// of either attempt cannot discard the only durable-retry copy.
    async fn retry_terminal_thinking_segment(
        &self,
        active_thinking: &mut Option<ThinkingSegmentState>,
    ) -> bool {
        if active_thinking.is_some() {
            warn!(
                conversation_id = %self.conversation_id,
                msg_id = %self.msg_id,
                "Retrying assistant thinking terminal persistence"
            );
            self.complete_active_thinking(active_thinking).await
        } else {
            true
        }
    }

    #[tracing::instrument(skip_all)]
    async fn close_active_text_segment(
        &self,
        active_text: &mut Option<TextSegmentState>,
        text_segments: &mut Vec<PersistedTextSegment>,
        status: &str,
    ) {
        if active_text
            .as_ref()
            .is_some_and(|segment| segment.buffer.is_empty())
        {
            *active_text = None;
            return;
        }

        // Keep the in-memory segment authoritative until the repository has
        // acknowledged the terminal write. This future is deliberately used
        // behind the non-terminal side-effect timeout: taking the segment
        // before the await would drop its only retryable copy when that timeout
        // cancels the future, leaving the later terminal cleanup with nothing
        // to persist.
        let persisted = {
            let Some(text_segment) = active_text.as_ref() else {
                return;
            };
            self.finalize_text_segment(text_segment, status).await
        };
        let Some(segment) = persisted else {
            return;
        };

        *active_text = None;
        if text_segments.len() < MAX_TERMINAL_ACTIVE_ITEMS {
            text_segments.push(segment);
        } else {
            warn!(
                max = MAX_TERMINAL_ACTIVE_ITEMS,
                "Relay finalized-text tracking limit reached"
            );
        }
    }

    /// Retry a terminal text write once after the first close attempt failed.
    /// The enclosing terminal cleanup already owns the global hard deadline, so
    /// this adds recovery for transient SQLite errors without an unbounded loop.
    async fn retry_terminal_text_segment(
        &self,
        active_text: &mut Option<TextSegmentState>,
        text_segments: &mut Vec<PersistedTextSegment>,
        status: &str,
    ) -> bool {
        if active_text.is_some() {
            warn!(
                conversation_id = %self.conversation_id,
                msg_id = %self.msg_id,
                "Retrying assistant text terminal persistence"
            );
            self.close_active_text_segment(active_text, text_segments, status)
                .await;
        }
        active_text.is_none()
    }

    /// Persist a Gemini-style tool_call event.
    #[tracing::instrument(skip_all)]
    async fn persist_tool_call(&self, data: &nomifun_ai_agent::protocol::events::tool_call::ToolCallEventData) {
        self.persist_tool_call_with_hidden(data, false).await;
    }

    async fn persist_provisional_artifact_tool_call(
        &self,
        data: &nomifun_ai_agent::protocol::events::tool_call::ToolCallEventData,
    ) -> bool {
        let provisional = Self::provisional_artifact_tool_call(data);
        self.persist_tool_call_projection(&provisional, false, Some(false))
            .await
    }

    fn provisional_artifact_tool_call(data: &ToolCallEventData) -> ToolCallEventData {
        let mut provisional = data.clone();
        provisional.status = ToolCallStatus::Running;
        provisional.artifacts.clear();
        provisional.output = Some(ARTIFACT_DELIVERY_PENDING_OUTPUT.to_owned());
        provisional
    }

    async fn persist_tool_call_with_hidden(
        &self,
        data: &nomifun_ai_agent::protocol::events::tool_call::ToolCallEventData,
        hidden: bool,
    ) {
        let _ = self.persist_tool_call_projection(data, hidden, None).await;
    }

    async fn persist_tool_call_projection(
        &self,
        data: &nomifun_ai_agent::protocol::events::tool_call::ToolCallEventData,
        hidden: bool,
        artifact_delivery_committed: Option<bool>,
    ) -> bool {
        if data.call_id.trim().is_empty() {
            warn!(
                tool = %data.name,
                status = ?data.status,
                "Skipping tool_call persistence because call_id is empty"
            );
            return false;
        }

        let status = match data.status {
            ToolCallStatus::Running => "work",
            ToolCallStatus::Completed => "finish",
            ToolCallStatus::Error => "error",
        };
        let message_id = self.tool_message_id(&data.call_id).await;
        let mut content_value = serde_json::to_value(data).unwrap_or_default();
        if let Some(object) = content_value.as_object_mut() {
            object.insert("turn_id".to_owned(), json!(self.root_turn_id));
            if let Some(committed) = artifact_delivery_committed {
                object.insert(ARTIFACT_DELIVERY_COMMITTED_FIELD.to_owned(), json!(committed));
            }
            if data.status != ToolCallStatus::Completed {
                // Artifact receipts are a terminal-success contract. Force an
                // explicit empty array (the wire serializer normally skips an
                // empty Vec) so merging an Error over a malformed Running row
                // cannot retain provisional/stale receipts.
                object.insert("artifacts".to_owned(), json!([]));
            }
        }
        let content = content_value.to_string();

        let existing = match self.repo.get_message(self.conv_id(), &message_id).await {
            Ok(existing) => existing,
            Err(e) => {
                error!(
                    call_id = %data.call_id,
                    tool = %data.name,
                    status,
                    error = %ErrorChain(&e),
                    "Failed to load tool_call message before persistence"
                );
                return false;
            }
        };

        if let Some(existing_row) = existing {
            let existing_artifact_committed = serde_json::from_str::<Value>(&existing_row.content)
                .ok()
                .and_then(|value| {
                    value
                        .get(ARTIFACT_DELIVERY_COMMITTED_FIELD)
                        .and_then(Value::as_bool)
                })
                == Some(true);
            let terminal_conflict = match (existing_row.status.as_deref(), data.status) {
                (Some("finish"), ToolCallStatus::Completed | ToolCallStatus::Error)
                | (Some("error"), ToolCallStatus::Error) => false,
                // A newly verified artifact completion always starts a fresh
                // provisional projection. It may safely demote an uncommitted
                // or legacy finish row; an existing error remains absorbing.
                (Some("finish"), _)
                    if artifact_delivery_committed == Some(false)
                        && !existing_artifact_committed =>
                {
                    false
                }
                (Some("finish" | "error"), _) => true,
                _ => false,
            };
            if terminal_conflict {
                warn!(
                    call_id = %data.call_id,
                    tool = %data.name,
                    stored_status = ?existing_row.status,
                    incoming_status = ?data.status,
                    "Ignoring tool call transition away from persisted terminal state"
                );
                return false;
            }
            let merged_content = Self::merge_json_content(&existing_row.content, &content);
            let update = nomifun_db::MessageRowUpdate {
                content: Some(merged_content),
                status: Some(Some(status.to_owned())),
                hidden: hidden.then_some(true),
            };
            if let Err(e) = self.repo.update_message(&message_id, &update).await {
                error!(
                    call_id = %data.call_id,
                    tool = %data.name,
                    status,
                    error = %ErrorChain(&e),
                    "Failed to update tool_call message"
                );
                return false;
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
                id: message_id.clone(),
                conversation_id: self.conversation_id.clone(),
                msg_id: Some(self.root_turn_id.clone()),
                r#type: "tool_call".into(),
                content,
                position: Some("left".into()),
                status: Some(status.to_owned()),
                hidden,
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
                return false;
            } else {
                debug!(
                    call_id = %data.call_id,
                    tool = %data.name,
                    status,
                    "Persisted tool_call message"
                );
            }
        }
        true
    }

    async fn tool_message_id(&self, call_id: &str) -> String {
        self.derived_message_id("tool_call", call_id).await
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

    fn invalidates_completed_artifacts(event: &AgentStreamEvent) -> bool {
        match event {
            AgentStreamEvent::Error(_) => true,
            AgentStreamEvent::Finish(data) => !matches!(
                data.stop_reason,
                None | Some(nomifun_ai_agent::protocol::events::TurnStopReason::EndTurn)
            ),
            _ => false,
        }
    }

    fn committed_artifact_tool_content(
        &self,
        data: &ToolCallEventData,
    ) -> Result<String, nomifun_db::DbError> {
        if data.status != ToolCallStatus::Completed || data.artifacts.is_empty() {
            return Err(nomifun_db::DbError::Conflict(format!(
                "tool call '{}' is not a completed artifact delivery",
                data.call_id
            )));
        }
        let mut value = serde_json::to_value(data)
            .map_err(|error| nomifun_db::DbError::Conflict(error.to_string()))?;
        let object = value.as_object_mut().ok_or_else(|| {
            nomifun_db::DbError::Conflict(format!(
                "tool call '{}' did not serialize as an object",
                data.call_id
            ))
        })?;
        object.insert("turn_id".to_owned(), json!(self.root_turn_id));
        object.insert(ARTIFACT_DELIVERY_COMMITTED_FIELD.to_owned(), json!(true));
        Ok(value.to_string())
    }

    fn committed_artifact_acp_tool_content(
        &self,
        data: &nomifun_ai_agent::protocol::events::tool_call::AcpToolCallEventData,
    ) -> Result<String, nomifun_db::DbError> {
        let has_delivery = data.update.content.as_ref().is_some_and(|items| {
            items.iter().any(|item| {
                matches!(
                    item,
                    nomifun_ai_agent::protocol::events::AcpToolCallContentItem::Artifact { .. }
                        | nomifun_ai_agent::protocol::events::AcpToolCallContentItem::ResourceLink { .. }
                )
            })
        });
        if data.update.status != Some(AcpToolCallStatus::Completed) || !has_delivery {
            return Err(nomifun_db::DbError::Conflict(format!(
                "ACP tool call '{}' is not a completed artifact delivery",
                data.update.tool_call_id
            )));
        }
        let mut value = serde_json::to_value(data)
            .map_err(|error| nomifun_db::DbError::Conflict(error.to_string()))?;
        normalize_keys_to_snake_case(&mut value);
        let object = value.as_object_mut().ok_or_else(|| {
            nomifun_db::DbError::Conflict(format!(
                "ACP tool call '{}' did not serialize as an object",
                data.update.tool_call_id
            ))
        })?;
        object.insert("turn_id".to_owned(), json!(self.root_turn_id));
        object.insert(ARTIFACT_DELIVERY_COMMITTED_FIELD.to_owned(), json!(true));
        Ok(value.to_string())
    }

    async fn commit_pending_artifact_deliveries(
        &self,
        generic: &HashMap<String, ToolCallEventData>,
        acp: &HashMap<
            String,
            nomifun_ai_agent::protocol::events::tool_call::AcpToolCallEventData,
        >,
    ) -> Result<(), nomifun_db::DbError> {
        let has_local_receipts = generic.values().any(|data| !data.artifacts.is_empty())
            || acp.values().any(|data| {
                data.update.content.as_ref().is_some_and(|items| {
                    items.iter().any(|item| {
                        matches!(
                            item,
                            nomifun_ai_agent::protocol::events::AcpToolCallContentItem::Artifact { .. }
                        )
                    })
                })
            });
        if has_local_receipts {
            let workspace = self.artifact_workspace.as_ref().ok_or_else(|| {
                nomifun_db::DbError::Conflict(
                    "artifact delivery has no canonical session workspace for final verification".to_owned(),
                )
            })?;
            let store = ArtifactStore::new(workspace);
            for data in generic.values() {
                for artifact in &data.artifacts {
                    store.reverify_receipt(artifact).map_err(|error| {
                        nomifun_db::DbError::Conflict(format!(
                            "tool call '{}' artifact '{}' failed final verification: {error}",
                            data.call_id, artifact.id
                        ))
                    })?;
                }
            }
            for data in acp.values() {
                for item in data.update.content.iter().flatten() {
                    if let nomifun_ai_agent::protocol::events::AcpToolCallContentItem::Artifact {
                        artifact,
                        ..
                    } = item
                    {
                        store.reverify_receipt(artifact).map_err(|error| {
                            nomifun_db::DbError::Conflict(format!(
                                "ACP tool call '{}' artifact '{}' failed final verification: {error}",
                                data.update.tool_call_id, artifact.id
                            ))
                        })?;
                    }
                }
            }
        }

        let mut generic_calls = generic.values().collect::<Vec<_>>();
        generic_calls.sort_by(|left, right| left.call_id.cmp(&right.call_id));
        let mut acp_calls = acp.values().collect::<Vec<_>>();
        acp_calls.sort_by(|left, right| {
            left.update
                .tool_call_id
                .cmp(&right.update.tool_call_id)
        });

        let mut commits = Vec::with_capacity(generic_calls.len() + acp_calls.len());
        for data in generic_calls {
            commits.push(TurnArtifactMessageCommit {
                id: self
                    .try_derived_message_id("tool_call", &data.call_id)
                    .await?,
                message_type: "tool_call".to_owned(),
                content: self.committed_artifact_tool_content(data)?,
            });
        }
        for data in acp_calls {
            commits.push(TurnArtifactMessageCommit {
                id: self
                    .try_derived_message_id("acp_tool_call", &data.update.tool_call_id)
                    .await?,
                message_type: "acp_tool_call".to_owned(),
                content: self.committed_artifact_acp_tool_content(data)?,
            });
        }

        let expected_ids = commits
            .iter()
            .map(|message| message.id.as_str())
            .collect::<HashSet<_>>();
        let committed = self
            .repo
            .commit_turn_artifact_messages(
                self.conv_id(),
                &self.root_turn_id,
                &commits,
                now_ms(),
            )
            .await?;
        if committed.len() != commits.len()
            || committed
                .iter()
                .any(|row| !expected_ids.contains(row.id.as_str()))
        {
            return Err(nomifun_db::DbError::Conflict(
                "artifact commit returned an incomplete or mismatched durable batch".to_owned(),
            ));
        }
        Ok(())
    }

    fn broadcast_committed_artifact_tool_calls(
        &self,
        completed: &HashMap<String, ToolCallEventData>,
    ) {
        let mut completed = completed.values().collect::<Vec<_>>();
        completed.sort_by(|left, right| left.call_id.cmp(&right.call_id));
        for data in completed {
            self.forward_to_websocket(&AgentStreamEvent::ToolCall(data.clone()));
        }
    }

    fn broadcast_committed_artifact_acp_tool_calls(
        &self,
        completed: &HashMap<
            String,
            nomifun_ai_agent::protocol::events::tool_call::AcpToolCallEventData,
        >,
    ) {
        let mut completed = completed.values().collect::<Vec<_>>();
        completed.sort_by(|left, right| {
            left.update
                .tool_call_id
                .cmp(&right.update.tool_call_id)
        });
        for data in completed {
            self.forward_to_websocket(&AgentStreamEvent::AcpToolCall(data.clone()));
        }
    }

    fn plan_terminal_status(event: &AgentStreamEvent) -> &'static str {
        match event {
            AgentStreamEvent::Finish(data)
                if matches!(
                    data.stop_reason,
                    None | Some(nomifun_ai_agent::protocol::events::TurnStopReason::EndTurn)
                ) => "finish",
            AgentStreamEvent::Finish(_) | AgentStreamEvent::Error(_) => "error",
            _ => "error",
        }
    }

    async fn finalize_active_plans(&self, active_plan_ids: &mut HashSet<String>, status: &str) {
        if active_plan_ids.len() > MAX_TERMINAL_ACTIVE_ITEMS {
            warn!(count = active_plan_ids.len(), "Truncating active plans during terminal cleanup");
        }
        for plan_id in active_plan_ids.drain().take(MAX_TERMINAL_ACTIVE_ITEMS) {
            let update = nomifun_db::MessageRowUpdate {
                content: None,
                status: Some(Some(status.to_owned())),
                hidden: None,
            };
            if let Err(error) = self.repo.update_message(&plan_id, &update).await {
                error!(
                    plan_id,
                    status,
                    error = %ErrorChain(&error),
                    "Failed to finalize active plan"
                );
            }
        }
    }

    fn take_failed_tool_calls(
        active_tool_calls: &mut HashMap<String, ToolCallEventData>,
        reason: &str,
    ) -> Vec<ToolCallEventData> {
        if active_tool_calls.is_empty() {
            return Vec::new();
        }

        if active_tool_calls.len() > MAX_TERMINAL_ACTIVE_ITEMS {
            warn!(count = active_tool_calls.len(), "Truncating active tool calls during terminal cleanup");
        }
        active_tool_calls
            .drain()
            .take(MAX_TERMINAL_ACTIVE_ITEMS)
            .map(|(_, mut data)| {
                let output = if data.status == ToolCallStatus::Completed {
                    format!(
                        "The turn ended without a valid completed delivery for this tool: {reason}"
                    )
                } else {
                    format!("The turn ended before this tool completed: {reason}")
                };
                data.status = ToolCallStatus::Error;
                data.output = Some(output);
                data.artifacts.clear();
                data
            })
            .collect()
    }

    fn broadcast_failed_tool_calls(&self, failed: &[ToolCallEventData]) {
        for data in failed {
            let event = AgentStreamEvent::ToolCall(data.clone());
            self.forward_to_websocket(&event);
        }
    }

    async fn persist_failed_tool_calls(&self, failed: &[ToolCallEventData]) {
        for data in failed {
            self.persist_tool_call(data).await;
        }
    }

    async fn fail_active_tool_calls(
        &self,
        active_tool_calls: &mut HashMap<String, ToolCallEventData>,
        reason: &str,
    ) {
        let failed = Self::take_failed_tool_calls(active_tool_calls, reason);
        self.broadcast_failed_tool_calls(&failed);
        self.persist_failed_tool_calls(&failed).await;
    }

    fn take_failed_acp_tool_calls(
        active_tool_calls: &mut HashMap<
            String,
            nomifun_ai_agent::protocol::events::tool_call::AcpToolCallEventData,
        >,
        reason: &str,
    ) -> Vec<nomifun_ai_agent::protocol::events::tool_call::AcpToolCallEventData> {
        if active_tool_calls.len() > MAX_TERMINAL_ACTIVE_ITEMS {
            warn!(count = active_tool_calls.len(), "Truncating active ACP tool calls during terminal cleanup");
        }
        active_tool_calls
            .drain()
            .take(MAX_TERMINAL_ACTIVE_ITEMS)
            .map(|(_, mut data)| {
                let output = if data.update.status == Some(AcpToolCallStatus::Completed) {
                    format!(
                        "The turn ended without a valid completed delivery for this tool: {reason}"
                    )
                } else {
                    format!("The turn ended before this tool completed: {reason}")
                };
                data.update.session_update = AcpToolCallSessionUpdateKind::ToolCallUpdate;
                data.update.status = Some(AcpToolCallStatus::Failed);
                data.update.raw_output = Some(json!(output));
                if let Some(content) = data.update.content.as_mut() {
                    content.retain(|item| {
                        !matches!(
                            item,
                            nomifun_ai_agent::protocol::events::AcpToolCallContentItem::Artifact {
                                ..
                            } | nomifun_ai_agent::protocol::events::AcpToolCallContentItem::ResourceLink {
                                ..
                            }
                        )
                    });
                }
                data
            })
            .collect()
    }

    fn broadcast_failed_acp_tool_calls(
        &self,
        failed: &[nomifun_ai_agent::protocol::events::tool_call::AcpToolCallEventData],
    ) {
        for data in failed {
            let event = AgentStreamEvent::AcpToolCall(data.clone());
            self.forward_to_websocket(&event);
        }
    }

    async fn persist_failed_acp_tool_calls(
        &self,
        failed: &[nomifun_ai_agent::protocol::events::tool_call::AcpToolCallEventData],
    ) {
        for data in failed {
            self.persist_acp_tool_call(&data).await;
        }
    }

    async fn fail_active_acp_tool_calls(
        &self,
        active_tool_calls: &mut HashMap<
            String,
            nomifun_ai_agent::protocol::events::tool_call::AcpToolCallEventData,
        >,
        reason: &str,
    ) {
        let failed = Self::take_failed_acp_tool_calls(active_tool_calls, reason);
        self.broadcast_failed_acp_tool_calls(&failed);
        self.persist_failed_acp_tool_calls(&failed).await;
    }

    async fn fail_active_tool_groups(
        &self,
        active_tool_groups: &mut HashMap<
            String,
            Vec<nomifun_ai_agent::protocol::events::tool_call::ToolGroupEntry>,
        >,
        reason: &str,
    ) {
        if active_tool_groups.len() > MAX_TERMINAL_ACTIVE_ITEMS {
            warn!(count = active_tool_groups.len(), "Truncating active tool groups during terminal cleanup");
        }
        let failed: Vec<_> = active_tool_groups
            .drain()
            .take(MAX_TERMINAL_ACTIVE_ITEMS)
            .map(|(_, mut entries)| {
                entries.truncate(MAX_TERMINAL_ACTIVE_ITEMS);
                for entry in &mut entries {
                    if entry.status == ToolCallStatus::Running {
                        entry.status = ToolCallStatus::Error;
                        let detail = format!("The turn ended before this tool completed: {reason}");
                        entry.description = Some(match entry.description.take() {
                            Some(description) if !description.is_empty() => format!("{description}: {detail}"),
                            _ => detail,
                        });
                    }
                }
                entries
            })
            .collect();

        for entries in failed {
            let event = AgentStreamEvent::ToolGroup(entries.clone());
            self.forward_to_websocket(&event);
            self.persist_tool_group(&entries).await;
        }
    }

    /// Persist an ACP (Claude CLI) tool call event.
    /// First event (ToolCall) inserts; subsequent events (ToolCallUpdate) update.
    #[tracing::instrument(skip_all)]
    async fn persist_acp_tool_call(&self, data: &nomifun_ai_agent::protocol::events::tool_call::AcpToolCallEventData) {
        let _ = self.persist_acp_tool_call_projection(data, None).await;
    }

    async fn persist_provisional_artifact_acp_tool_call(
        &self,
        data: &nomifun_ai_agent::protocol::events::tool_call::AcpToolCallEventData,
    ) -> bool {
        let provisional = Self::provisional_artifact_acp_tool_call(data);
        self.persist_acp_tool_call_projection(&provisional, Some(false))
            .await
    }

    fn provisional_artifact_acp_tool_call(
        data: &nomifun_ai_agent::protocol::events::tool_call::AcpToolCallEventData,
    ) -> nomifun_ai_agent::protocol::events::tool_call::AcpToolCallEventData {
        let mut provisional = data.clone();
        provisional.update.status = Some(AcpToolCallStatus::InProgress);
        provisional.update.raw_output = Some(json!(ARTIFACT_DELIVERY_PENDING_OUTPUT));
        if let Some(content) = provisional.update.content.as_mut() {
            content.retain(|item| {
                !matches!(
                    item,
                    nomifun_ai_agent::protocol::events::AcpToolCallContentItem::Artifact { .. }
                        | nomifun_ai_agent::protocol::events::AcpToolCallContentItem::ResourceLink { .. }
                )
            });
        }
        provisional
    }

    async fn persist_acp_tool_call_projection(
        &self,
        data: &nomifun_ai_agent::protocol::events::tool_call::AcpToolCallEventData,
        artifact_delivery_committed: Option<bool>,
    ) -> bool {
        let tool_call_id = &data.update.tool_call_id;
        if tool_call_id.trim().is_empty() {
            warn!("Skipping ACP tool call persistence because tool_call_id is empty");
            return false;
        }
        let message_id = self.acp_tool_message_id(tool_call_id).await;
        let status = match data.update.status {
            Some(AcpToolCallStatus::Pending) | None => "work",
            Some(AcpToolCallStatus::InProgress) => "work",
            Some(AcpToolCallStatus::Completed) => "finish",
            Some(AcpToolCallStatus::Failed) => "error",
        };

        let mut value = serde_json::to_value(data).unwrap_or_default();
        normalize_keys_to_snake_case(&mut value);
        if let Some(object) = value.as_object_mut() {
            object.insert("turn_id".to_owned(), json!(self.root_turn_id));
            if let Some(committed) = artifact_delivery_committed {
                object.insert(ARTIFACT_DELIVERY_COMMITTED_FIELD.to_owned(), json!(committed));
            }
        }
        if data.update.status != Some(AcpToolCallStatus::Completed)
            && let Some(content) = value
                .get_mut("update")
                .and_then(|update| update.as_object_mut())
                .and_then(|update| update.get_mut("content"))
                .and_then(serde_json::Value::as_array_mut)
        {
            // A progress/failed frame may contain partial bytes or a remote
            // link, but those are not successful durable output. Keep text,
            // diffs, terminal diagnostics and artifact_error items only.
            content.retain(|item| {
                !matches!(
                    item.get("type").and_then(serde_json::Value::as_str),
                    Some("artifact" | "resource_link")
                )
            });
        }
        let content = value.to_string();

        let existing = match self.repo.get_message(self.conv_id(), &message_id).await {
            Ok(existing) => existing,
            Err(e) => {
                error!(
                    tool_call_id,
                    status,
                    error = %ErrorChain(&e),
                    "Failed to load ACP tool call before persistence"
                );
                return false;
            }
        };
        if let Some(existing_row) = existing {
            let existing_artifact_committed = serde_json::from_str::<Value>(&existing_row.content)
                .ok()
                .and_then(|value| {
                    value
                        .get(ARTIFACT_DELIVERY_COMMITTED_FIELD)
                        .and_then(Value::as_bool)
                })
                == Some(true);
            let terminal_conflict = match (existing_row.status.as_deref(), status) {
                (Some("finish"), "finish" | "error") | (Some("error"), "error") => false,
                (Some("finish"), _)
                    if artifact_delivery_committed == Some(false)
                        && !existing_artifact_committed =>
                {
                    false
                }
                (Some("finish" | "error"), _) => true,
                _ => false,
            };
            if terminal_conflict {
                warn!(
                    tool_call_id,
                    stored_status = ?existing_row.status,
                    incoming_status = status,
                    "Ignoring ACP tool transition away from persisted terminal state"
                );
                return false;
            }
            let merged_content = Self::merge_acp_tool_call_content(&existing_row.content, &value);
            let update = nomifun_db::MessageRowUpdate {
                content: Some(merged_content),
                status: Some(Some(status.to_owned())),
                hidden: None,
            };
            if let Err(e) = self.repo.update_message(&message_id, &update).await {
                error!(error = %ErrorChain(&e), "Failed to update acp_tool_call message");
                return false;
            }
            return true;
        }

        let row = MessageRow {
            id: message_id.clone(),
            conversation_id: self.conversation_id.clone(),
            msg_id: Some(self.root_turn_id.clone()),
            r#type: "acp_tool_call".into(),
            content,
            position: Some("left".into()),
            status: Some(status.to_owned()),
            hidden: false,
            created_at: now_ms(),
        };
        if let Err(e) = self.repo.insert_message(&row).await {
            error!(error = %ErrorChain(&e), "Failed to persist acp_tool_call message");
            return false;
        }
        true
    }

    async fn acp_tool_message_id(&self, tool_call_id: &str) -> String {
        self.derived_message_id("acp_tool_call", tool_call_id).await
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
    fn merge_acp_tool_call_content(existing_content: &str, update_value: &serde_json::Value) -> String {
        let mut base: serde_json::Value = serde_json::from_str(existing_content).unwrap_or_default();
        if let (Some(base_object), Some(update_object)) = (base.as_object_mut(), update_value.as_object()) {
            for (key, value) in update_object {
                if key != "update" && !value.is_null() {
                    base_object.insert(key.clone(), value.clone());
                }
            }
        }
        if let (Some(base_update), Some(new_update)) = (
            base.get_mut("update").and_then(|v| v.as_object_mut()),
            update_value.get("update").and_then(|v| v.as_object()),
        ) {
            for (key, val) in new_update {
                if !val.is_null() {
                    base_update.insert(key.clone(), val.clone());
                }
            }
            if new_update.get("status").and_then(serde_json::Value::as_str) == Some("failed")
                && let Some(content) = base_update
                    .get_mut("content")
                    .and_then(serde_json::Value::as_array_mut)
            {
                content.retain(|item| {
                    !matches!(
                        item.get("type").and_then(serde_json::Value::as_str),
                        Some("artifact" | "resource_link")
                    )
                });
            }
        }
        base.to_string()
    }

    /// Persist a tool_group event (array of tool summaries).
    #[tracing::instrument(skip_all)]
    async fn persist_tool_group(&self, entries: &[nomifun_ai_agent::protocol::events::tool_call::ToolGroupEntry]) {
        let status = if entries.iter().any(|entry| entry.status == ToolCallStatus::Error) {
            "error"
        } else if entries.iter().all(|entry| entry.status == ToolCallStatus::Completed) {
            "finish"
        } else {
            "work"
        };
        let content = serde_json::to_string(entries).unwrap_or_default();

        let source_group_id = entries
            .first()
            .map(|e| e.call_id.clone())
            .unwrap_or_else(ConversationService::mint_msg_id);
        let group_id = self.derived_message_id("tool_group", &source_group_id).await;

        let existing = self
            .repo
            .get_message(self.conv_id(), &group_id)
            .await
            .unwrap_or(None);

        if let Some(existing_row) = existing {
            let terminal_conflict = match (existing_row.status.as_deref(), status) {
                (Some("finish"), "finish") | (Some("error"), "error") => false,
                (Some("finish" | "error"), _) => true,
                _ => false,
            };
            if terminal_conflict {
                warn!(
                    group_id,
                    stored_status = ?existing_row.status,
                    incoming_status = status,
                    "Ignoring tool group transition away from persisted terminal state"
                );
                return;
            }
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
                conversation_id: self.conversation_id.clone(),
                msg_id: Some(self.root_turn_id.clone()),
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

        let cancellation = self
            .cancellation
            .as_ref()
            .map(AgentTurnCancellation::cancellation_token);
        middleware
            .process_with_cancellation(
                text,
                &self.user_id,
                &self.conversation_id,
                cancellation.as_ref(),
            )
            .await
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
            obj.insert("turn_id".into(), json!(self.root_turn_id));
            obj.insert("companion".into(), json!(self.companion));
            obj.insert("companion_id".into(), json!(self.companion_id));
            obj.insert("origin".into(), json!(self.origin));
            obj.insert("channel_platform".into(), json!(self.channel_platform));
        }
        let msg = WebSocketMessage::new("message.stream", payload);
        self.user_events.send_to_user(&self.user_id, msg);
    }

    /// Emit `turn.completed` for the conversation, with the companion-companion
    /// wire markers and the turn's `origin` marker attached to the
    /// `turn.completed` payload (see [`Self::with_companion_context`] /
    /// [`Self::with_origin`]).
    #[tracing::instrument(skip_all, fields(conversation_id = %conversation_id))]
    pub async fn complete_conversation_with_context(
        repo: &Arc<dyn IConversationRepository>,
        user_events: &Arc<dyn UserEventSink>,
        user_id: &str,
        conversation_id: &str,
        turn_id: Option<String>,
        runtime: Option<ConversationRuntimeSummary>,
        companion: bool,
        companion_id: Option<CompanionId>,
        origin: Option<String>,
        channel_platform: Option<String>,
    ) {
        Self::persist_conversation_finished(repo, conversation_id).await;
        Self::broadcast_turn_completed_with_context(
            user_events,
            user_id,
            conversation_id,
            turn_id,
            runtime,
            companion,
            companion_id,
            origin,
            channel_platform,
        );
    }

    pub async fn persist_conversation_finished(
        repo: &Arc<dyn IConversationRepository>,
        conversation_id: &str,
    ) {
        let update = nomifun_db::ConversationRowUpdate {
            status: Some("finished".to_owned()),
            updated_at: Some(now_ms()),
            ..Default::default()
        };
        match tokio::time::timeout(
            TURN_COMPLETION_PERSIST_GRACE,
            repo.update(conversation_id, &update),
        )
        .await
        {
            Ok(Ok(())) => {}
            Ok(Err(e)) => {
                error!(error = %ErrorChain(&e), "Failed to update conversation status");
            }
            Err(_) => {
                warn!(conversation_id, "Timed out updating conversation status");
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub fn broadcast_turn_completed_with_context(
        user_events: &Arc<dyn UserEventSink>,
        user_id: &str,
        conversation_id: &str,
        turn_id: Option<String>,
        runtime: Option<ConversationRuntimeSummary>,
        companion: bool,
        companion_id: Option<CompanionId>,
        origin: Option<String>,
        channel_platform: Option<String>,
    ) {
        let payload = json!({
            "conversation_id": conversation_id,
            "turn_id": turn_id,
            "status": "finished",
            "can_send_message": true,
            "runtime": runtime,
            "companion": companion,
            "companion_id": companion_id,
            "origin": origin,
            "channel_platform": channel_platform,
        });
        let msg = WebSocketMessage::new("turn.completed", payload);
        user_events.send_to_user(user_id, msg);

        debug!(conversation_id, status = "finished", "Turn completed");
    }

    async fn try_derived_message_id(
        &self,
        message_type: &str,
        correlation_key: &str,
    ) -> Result<String, nomifun_db::DbError> {
        let cache_key = format!("{message_type}\0{correlation_key}");
        if let Some(id) = self
            .derived_message_ids
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(&cache_key)
            .cloned()
        {
            return Ok(id);
        }

        let id = self
            .repo
            .claim_message_correlation(
                self.conv_id(),
                // Provider call/session ids are only guaranteed unique inside
                // one wire prompt. Continuations can legitimately reuse a call
                // id, so canonical row identity remains wire-scoped even though
                // the row's ownership (`msg_id`/content.turn_id) is root-scoped.
                &self.msg_id,
                message_type,
                correlation_key,
            )
            .await?;
        self.derived_message_ids
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(cache_key, id.clone());
        Ok(id)
    }

    async fn derived_message_id(&self, message_type: &str, correlation_key: &str) -> String {
        match self
            .try_derived_message_id(message_type, correlation_key)
            .await
        {
            Ok(id) => id,
            Err(error) => {
                error!(
                    message_type,
                    correlation_key,
                    error = %ErrorChain(&error),
                    "Failed to claim durable streamed-message correlation"
                );
                MessageId::new().into_string()
            }
        }
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
    use nomifun_common::{ConversationId, MessageId};
    use nomifun_db::DbError;
    use std::sync::{
        Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering as AtomicOrdering},
    };

    const TEST_ASSISTANT_MESSAGE_ID: &str = "msg_0190f5fe-7c00-7a00-8abc-012345678941";
    const TEST_TURN_A: &str = "msg_0190f5fe-7c00-7a00-8abc-012345678942";
    const TEST_TURN_B: &str = "msg_0190f5fe-7c00-7a00-8abc-012345678943";
    const TEST_USER_ID: &str = "user_0190f5fe-7c00-7a00-8abc-012345678944";

    fn test_conversation_id() -> String {
        ConversationId::new().into_string()
    }

    fn test_artifact(id: &str) -> nomifun_ai_agent::artifact_store::PersistedArtifact {
        nomifun_ai_agent::artifact_store::PersistedArtifact {
            id: id.into(),
            kind: nomifun_ai_agent::artifact_store::ArtifactKind::Image,
            mime_type: "image/png".into(),
            path: format!("/workspace/{id}.png"),
            relative_path: format!("nomifun-artifacts/{id}.png"),
            size_bytes: 10,
            sha256: "a".repeat(64),
        }
    }

    fn persisted_png_artifact(
        workspace: &std::path::Path,
    ) -> nomifun_ai_agent::artifact_store::PersistedArtifact {
        const ONE_PIXEL_PNG: &str =
            "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mNk+A8AAQUBAScY42YAAAAASUVORK5CYII=";
        ArtifactStore::new(workspace)
            .persist_inline(
                nomifun_ai_agent::artifact_store::ArtifactKind::Image,
                "image/png",
                ONE_PIXEL_PNG,
            )
            .expect("persist verified test PNG")
    }

    struct TestUserEventBus {
        sender: broadcast::Sender<WebSocketMessage<Value>>,
    }

    impl TestUserEventBus {
        fn new(capacity: usize) -> Self {
            let (sender, _) = broadcast::channel(capacity);
            Self { sender }
        }

        fn subscribe(&self) -> broadcast::Receiver<WebSocketMessage<Value>> {
            self.sender.subscribe()
        }
    }

    impl UserEventSink for TestUserEventBus {
        fn send_to_user(&self, _user_id: &str, event: WebSocketMessage<Value>) {
            let _ = self.sender.send(event);
        }
    }

    // ── run() async tests ─────────────────────────────────────────

    #[tokio::test]
    async fn run_text_then_finish_persists_message() {
        let repo = Arc::new(RecordingRepo::new());
        let bus = Arc::new(TestUserEventBus::new(64));
        let (tx, _) = broadcast::channel(64);

        let conversation_id = test_conversation_id();
        let relay = StreamRelay::new(
            conversation_id.clone(),
            TEST_ASSISTANT_MESSAGE_ID.into(),
            TEST_USER_ID.into(),
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
        assert_eq!(msg.conversation_id, conversation_id);
        assert_eq!(msg.id, TEST_ASSISTANT_MESSAGE_ID);
        assert_eq!(msg.r#type, "text");
        assert_eq!(msg.status.as_deref(), Some("finish"));

        let content: serde_json::Value = serde_json::from_str(&msg.content).unwrap();
        assert_eq!(content["content"], "Hello World");
    }

    #[tokio::test]
    async fn non_terminal_persistence_timeout_opens_a_turn_wide_circuit_breaker() {
        let relay = StreamRelay::new(
            test_conversation_id(),
            TEST_ASSISTANT_MESSAGE_ID.into(),
            TEST_USER_ID.into(),
            Arc::new(RecordingRepo::new()),
            Arc::new(TestUserEventBus::new(8)),
            None,
        );
        let first = relay
            .bounded_event_side_effect(
                tokio::time::Instant::now() + Duration::from_millis(1),
                "never_resolves",
                std::future::pending::<()>(),
            )
            .await;
        assert!(first.is_none());

        let polls = Arc::new(AtomicUsize::new(0));
        let polls_for_future = Arc::clone(&polls);
        let second = relay
            .bounded_event_side_effect(
                tokio::time::Instant::now() + Duration::from_secs(1),
                "must_not_poll",
                async move {
                    polls_for_future.fetch_add(1, AtomicOrdering::SeqCst);
                },
            )
            .await;
        assert!(second.is_none());
        assert_eq!(polls.load(AtomicOrdering::SeqCst), 0);
    }

    #[tokio::test]
    async fn failed_streaming_text_insert_is_retried_by_terminal_finalization() {
        let repo = Arc::new(RecordingRepo::new());
        repo.fail_next_message_insert();
        let bus = Arc::new(TestUserEventBus::new(64));
        let (tx, _) = broadcast::channel(64);
        let relay = StreamRelay::new(
            test_conversation_id(),
            TEST_ASSISTANT_MESSAGE_ID.into(),
            TEST_USER_ID.into(),
            repo.clone(),
            bus,
            None,
        );
        let rx = tx.subscribe();

        for _ in 0..FLUSH_INTERVAL {
            tx.send(AgentStreamEvent::Text(TextEventData {
                content: "x".into(),
            }))
            .unwrap();
        }
        tx.send(AgentStreamEvent::Finish(FinishEventData::default())).unwrap();

        let outcome = relay.consume(rx).await;

        assert_eq!(outcome.terminal, RelayTerminal::Finish);
        assert_eq!(outcome.final_text_msg_id.as_deref(), Some(TEST_ASSISTANT_MESSAGE_ID));
        let inserts = repo.take_inserts();
        assert_eq!(inserts.len(), 1, "the failed work insert must be retried as the terminal row");
        assert_eq!(inserts[0].status.as_deref(), Some("finish"));
        let content: Value = serde_json::from_str(&inserts[0].content).unwrap();
        assert_eq!(content["content"], "x".repeat(FLUSH_INTERVAL as usize));
        assert!(
            repo.take_updates().is_empty(),
            "a failed insert must not make finalization update a nonexistent row"
        );
    }

    #[tokio::test]
    async fn ambiguous_streaming_insert_is_reconciled_without_a_duplicate_row() {
        let repo = Arc::new(RecordingRepo::new());
        repo.commit_next_message_insert_then_error();
        let bus = Arc::new(TestUserEventBus::new(64));
        let (tx, _) = broadcast::channel(64);
        let relay = StreamRelay::new(
            test_conversation_id(),
            TEST_ASSISTANT_MESSAGE_ID.into(),
            TEST_USER_ID.into(),
            repo.clone(),
            bus,
            None,
        );
        let rx = tx.subscribe();

        for _ in 0..FLUSH_INTERVAL {
            tx.send(AgentStreamEvent::Text(TextEventData {
                content: "x".into(),
            }))
            .unwrap();
        }
        tx.send(AgentStreamEvent::Finish(FinishEventData::default())).unwrap();

        let outcome = relay.consume(rx).await;

        assert_eq!(
            outcome.final_text_msg_id.as_deref(),
            Some(TEST_ASSISTANT_MESSAGE_ID)
        );
        let inserts = repo.take_inserts();
        assert_eq!(
            inserts.len(),
            1,
            "a committed-but-unacknowledged insert must be reconciled, not duplicated"
        );
        let updates = repo.take_updates();
        assert_eq!(updates.len(), 2);
        assert_eq!(
            updates[0]
                .1
                .status
                .as_ref()
                .and_then(|status| status.as_deref()),
            Some("work"),
            "the ambiguous streaming insert is reconciled to its intended work state"
        );
        assert_eq!(
            updates[1]
                .1
                .status
                .as_ref()
                .and_then(|status| status.as_deref()),
            Some("finish")
        );
    }

    #[tokio::test]
    async fn persistent_terminal_insert_failure_surfaces_state_inconsistent_error() {
        let repo = Arc::new(RecordingRepo::new());
        repo.fail_message_inserts();
        let bus = Arc::new(TestUserEventBus::new(64));
        let mut ws_rx = bus.subscribe();
        let (tx, _) = broadcast::channel(64);
        let relay = StreamRelay::new(
            test_conversation_id(),
            TEST_ASSISTANT_MESSAGE_ID.into(),
            TEST_USER_ID.into(),
            repo.clone(),
            bus,
            None,
        );
        let rx = tx.subscribe();
        tx.send(AgentStreamEvent::Text(TextEventData {
            content: "visible but unavailable database".into(),
        }))
        .unwrap();
        tx.send(AgentStreamEvent::Finish(FinishEventData::default())).unwrap();

        let outcome = relay.consume(rx).await;

        assert!(outcome.terminal.is_error());
        assert_eq!(
            outcome.terminal.code(),
            Some(AgentErrorCode::NomifunStateInconsistent)
        );
        assert!(outcome.emitted_response);
        assert!(outcome.final_text.is_none());
        assert!(outcome.final_text_msg_id.is_none());
        assert!(
            repo.take_inserts().iter().all(|row| row.r#type != "text"),
            "no text row may be claimed after every insert attempt failed"
        );

        let mut stream_types = Vec::new();
        while let Ok(event) = ws_rx.try_recv() {
            if event.name == "message.stream" {
                stream_types.push(event.data["type"].clone());
            }
        }
        assert!(!stream_types.iter().any(|kind| *kind == json!("finish")));
        assert_eq!(stream_types.last(), Some(&json!("error")));
    }

    #[tokio::test]
    async fn failed_text_finalization_keeps_the_segment_retryable_and_untracked() {
        let repo = Arc::new(RecordingRepo::new());
        repo.fail_next_message_insert();
        let relay = StreamRelay::new(
            test_conversation_id(),
            TEST_ASSISTANT_MESSAGE_ID.into(),
            TEST_USER_ID.into(),
            repo.clone(),
            Arc::new(TestUserEventBus::new(8)),
            None,
        );
        let mut active_text = Some(TextSegmentState {
            id: TEST_ASSISTANT_MESSAGE_ID.into(),
            buffer: "durable answer".into(),
            created_at: now_ms(),
            record_created: false,
            flush_counter: 0,
        });
        let mut text_segments = Vec::new();

        relay
            .close_active_text_segment(&mut active_text, &mut text_segments, "finish")
            .await;

        assert!(active_text.is_some(), "a failed final write must retain the retry state");
        assert!(
            text_segments.is_empty(),
            "a failed final write must not be reported as a persisted segment"
        );

        relay
            .close_active_text_segment(&mut active_text, &mut text_segments, "finish")
            .await;

        assert!(active_text.is_none());
        assert_eq!(text_segments.len(), 1);
        let inserts = repo.take_inserts();
        assert_eq!(inserts.len(), 1);
        assert_eq!(inserts[0].status.as_deref(), Some("finish"));
    }

    #[tokio::test]
    async fn transient_terminal_update_failure_retries_the_existing_work_row() {
        let repo = Arc::new(RecordingRepo::new());
        repo.fail_next_message_update();
        let bus = Arc::new(TestUserEventBus::new(64));
        let (tx, _) = broadcast::channel(64);
        let relay = StreamRelay::new(
            test_conversation_id(),
            TEST_ASSISTANT_MESSAGE_ID.into(),
            TEST_USER_ID.into(),
            repo.clone(),
            bus,
            None,
        );
        let rx = tx.subscribe();

        for _ in 0..FLUSH_INTERVAL {
            tx.send(AgentStreamEvent::Text(TextEventData {
                content: "x".into(),
            }))
            .unwrap();
        }
        tx.send(AgentStreamEvent::Finish(FinishEventData::default())).unwrap();

        let outcome = relay.consume(rx).await;

        assert_eq!(outcome.final_text_msg_id.as_deref(), Some(TEST_ASSISTANT_MESSAGE_ID));
        let inserts = repo.take_inserts();
        assert_eq!(inserts.len(), 1, "the work row must not be inserted a second time");
        assert_eq!(inserts[0].status.as_deref(), Some("work"));
        let updates = repo.take_updates();
        assert_eq!(updates.len(), 1, "terminal finalization should retry exactly once");
        assert_eq!(updates[0].0, TEST_ASSISTANT_MESSAGE_ID);
        assert_eq!(
            updates[0].1.status.as_ref().and_then(|status| status.as_deref()),
            Some("finish")
        );
    }

    #[tokio::test]
    async fn persistent_terminal_update_failure_does_not_claim_or_insert_the_work_row() {
        let repo = Arc::new(RecordingRepo::new());
        repo.fail_message_updates();
        let bus = Arc::new(TestUserEventBus::new(64));
        let mut ws_rx = bus.subscribe();
        let (tx, _) = broadcast::channel(64);
        let relay = StreamRelay::new(
            test_conversation_id(),
            TEST_ASSISTANT_MESSAGE_ID.into(),
            TEST_USER_ID.into(),
            repo.clone(),
            bus,
            None,
        );
        let rx = tx.subscribe();

        for _ in 0..FLUSH_INTERVAL {
            tx.send(AgentStreamEvent::Text(TextEventData {
                content: "x".into(),
            }))
            .unwrap();
        }
        tx.send(AgentStreamEvent::Finish(FinishEventData::default())).unwrap();

        let outcome = relay.consume(rx).await;

        assert!(outcome.terminal.is_error());
        assert_eq!(
            outcome.terminal.code(),
            Some(AgentErrorCode::NomifunStateInconsistent)
        );
        assert!(outcome.emitted_response, "the visible text must continue to block failover");
        assert!(outcome.final_text.is_none());
        assert!(
            outcome.final_text_msg_id.is_none(),
            "an unfinalized work row must not be advertised as durable final text"
        );
        let inserts = repo.take_inserts();
        let text_rows: Vec<_> = inserts.iter().filter(|row| row.r#type == "text").collect();
        assert_eq!(
            text_rows.len(),
            1,
            "finalize must not fall back to a conflicting INSERT for an existing work row"
        );
        assert_eq!(text_rows[0].status.as_deref(), Some("work"));
        assert!(
            inserts.iter().any(|row| row.r#type == "tips" && row.status.as_deref() == Some("error")),
            "the state-inconsistent terminal must itself be durable"
        );
        assert!(repo.take_updates().is_empty());

        let mut stream_types = Vec::new();
        while let Ok(event) = ws_rx.try_recv() {
            if event.name == "message.stream" {
                stream_types.push(event.data["type"].clone());
            }
        }
        assert!(!stream_types.iter().any(|kind| *kind == json!("finish")));
        assert_eq!(stream_types.last(), Some(&json!("error")));
    }

    #[tokio::test]
    async fn text_persistence_failure_prevents_completed_artifact_commit() {
        use nomifun_ai_agent::protocol::events::tool_call::{ToolCallEventData, ToolCallStatus};

        let repo = Arc::new(RecordingRepo::new());
        repo.fail_message_updates();
        let bus = Arc::new(TestUserEventBus::new(128));
        let mut ws_rx = bus.subscribe();
        let (tx, _) = broadcast::channel(128);
        let workspace = std::env::temp_dir().join(format!(
            "nomifun-text-persistence-artifact-test-{}",
            MessageId::new().into_string()
        ));
        std::fs::create_dir_all(&workspace).expect("create test workspace");
        let artifact = persisted_png_artifact(&workspace);
        let relay = StreamRelay::new(
            test_conversation_id(),
            TEST_TURN_A.into(),
            TEST_USER_ID.into(),
            repo.clone(),
            bus,
            None,
        )
        .with_artifact_workspace(workspace.clone());
        let rx = tx.subscribe();

        tx.send(AgentStreamEvent::ToolCall(ToolCallEventData {
            call_id: "artifact-before-unpersisted-text".into(),
            name: "ImageGeneration".into(),
            args: json!({"prompt": "cat"}),
            status: ToolCallStatus::Completed,
            input: None,
            output: Some("generated".into()),
            description: None,
            artifacts: vec![artifact],
        }))
        .unwrap();
        for _ in 0..FLUSH_INTERVAL {
            tx.send(AgentStreamEvent::Text(TextEventData {
                content: "x".into(),
            }))
            .unwrap();
        }
        tx.send(AgentStreamEvent::Finish(FinishEventData::default())).unwrap();

        let outcome = relay.consume(rx).await;

        assert_eq!(
            outcome.terminal.code(),
            Some(AgentErrorCode::NomifunStateInconsistent)
        );
        let inserts = repo.take_inserts();
        let tool_row = inserts
            .iter()
            .find(|row| row.r#type == "tool_call")
            .expect("artifact tool has a provisional row");
        assert_eq!(tool_row.status.as_deref(), Some("work"));
        assert!(
            repo.take_updates().iter().all(|(id, update)| {
                id != &tool_row.id
                    || update.status.as_ref().and_then(|status| status.as_deref())
                        != Some("finish")
            }),
            "artifact receipt must not commit after assistant text durability fails"
        );

        let mut tool_statuses = Vec::new();
        let mut stream_types = Vec::new();
        while let Ok(event) = ws_rx.try_recv() {
            if event.name != "message.stream" {
                continue;
            }
            stream_types.push(event.data["type"].clone());
            if event.data["type"] == "tool_call"
                && let Some(status) = event.data["data"]["status"].as_str()
            {
                tool_statuses.push(status.to_owned());
            }
        }
        assert!(!tool_statuses.iter().any(|status| status == "completed"));
        assert_eq!(tool_statuses.last().map(String::as_str), Some("error"));
        assert!(!stream_types.iter().any(|kind| *kind == json!("finish")));
        assert_eq!(stream_types.last(), Some(&json!("error")));
        std::fs::remove_dir_all(workspace).expect("remove test workspace");
    }

    #[tokio::test]
    async fn timed_out_text_close_remains_available_for_terminal_retry() {
        let repo = Arc::new(RecordingRepo::new());
        repo.set_block_message_inserts(true);
        let relay = StreamRelay::new(
            test_conversation_id(),
            TEST_ASSISTANT_MESSAGE_ID.into(),
            TEST_USER_ID.into(),
            repo.clone(),
            Arc::new(TestUserEventBus::new(8)),
            None,
        );
        let mut active_text = Some(TextSegmentState {
            id: TEST_ASSISTANT_MESSAGE_ID.into(),
            buffer: "answer after a busy database".into(),
            created_at: now_ms(),
            record_created: false,
            flush_counter: 0,
        });
        let mut text_segments = Vec::new();

        let bounded = relay
            .bounded_event_side_effect(
                tokio::time::Instant::now() + Duration::from_millis(10),
                "close_text_before_tool",
                relay.close_active_text_segment(&mut active_text, &mut text_segments, "finish"),
            )
            .await;

        assert!(bounded.is_none());
        assert!(relay.event_side_effect_circuit_open.load(Ordering::Acquire));
        assert!(
            active_text.is_some(),
            "cancelling a non-terminal close must not consume the only text copy"
        );
        assert!(text_segments.is_empty());

        // Terminal cleanup bypasses the non-terminal circuit breaker. Once the
        // repository is responsive it must be able to write the retained text.
        repo.set_block_message_inserts(false);
        relay
            .close_active_text_segment(&mut active_text, &mut text_segments, "finish")
            .await;

        assert!(active_text.is_none());
        assert_eq!(text_segments.len(), 1);
        let inserts = repo.take_inserts();
        assert_eq!(inserts.len(), 1);
        let content: Value = serde_json::from_str(&inserts[0].content).unwrap();
        assert_eq!(content["content"], "answer after a busy database");
        assert_eq!(inserts[0].status.as_deref(), Some("finish"));
    }

    #[tokio::test]
    async fn timed_out_thinking_close_keeps_state_and_sends_done_once() {
        let repo = Arc::new(RecordingRepo::new());
        repo.set_block_message_inserts(true);
        let bus = Arc::new(TestUserEventBus::new(16));
        let mut ws_rx = bus.subscribe();
        let relay = StreamRelay::new(
            test_conversation_id(),
            TEST_ASSISTANT_MESSAGE_ID.into(),
            TEST_USER_ID.into(),
            repo.clone(),
            bus,
            None,
        );
        let mut active_thinking = Some(ThinkingSegmentState {
            id: TEST_ASSISTANT_MESSAGE_ID.into(),
            buffer: "reasoning".into(),
            started_at: now_ms(),
            completed_duration_ms: None,
        });

        let bounded = relay
            .bounded_event_side_effect(
                tokio::time::Instant::now() + Duration::from_millis(10),
                "complete_thinking_before_text",
                relay.complete_active_thinking(&mut active_thinking),
            )
            .await;

        assert!(bounded.is_none());
        assert!(active_thinking.is_some());
        assert!(repo.take_inserts().is_empty());

        repo.set_block_message_inserts(false);
        assert!(relay.complete_active_thinking(&mut active_thinking).await);
        assert!(active_thinking.is_none());
        let inserts = repo.take_inserts();
        assert_eq!(inserts.len(), 1);
        assert_eq!(inserts[0].r#type, "thinking");

        let done_count = std::iter::from_fn(|| ws_rx.try_recv().ok())
            .filter(|event| {
                event.name == "message.stream"
                    && event.data["type"] == "thinking"
                    && event.data["data"]["status"] == "done"
            })
            .count();
        assert_eq!(done_count, 1, "a persistence retry must not duplicate thinking.done");
    }

    #[tokio::test]
    async fn thinking_insert_reconcile_update_failure_remains_retryable() {
        let repo = Arc::new(RecordingRepo::new());
        repo.commit_next_message_insert_then_error();
        repo.fail_next_message_update();
        repo.reject_duplicate_message_inserts();
        let bus = Arc::new(TestUserEventBus::new(16));
        let mut ws_rx = bus.subscribe();
        let relay = StreamRelay::new(
            test_conversation_id(),
            TEST_ASSISTANT_MESSAGE_ID.into(),
            TEST_USER_ID.into(),
            repo.clone(),
            bus,
            None,
        );
        let mut active_thinking = Some(ThinkingSegmentState {
            id: TEST_ASSISTANT_MESSAGE_ID.into(),
            buffer: "reasoning".into(),
            started_at: now_ms(),
            completed_duration_ms: None,
        });

        assert!(!relay.complete_active_thinking(&mut active_thinking).await);
        assert!(active_thinking.is_some());
        assert!(relay.complete_active_thinking(&mut active_thinking).await);
        assert!(active_thinking.is_none());

        assert_eq!(repo.take_inserts().len(), 1);
        let updates = repo.take_updates();
        assert_eq!(updates.len(), 1);
        assert_eq!(updates[0].0, TEST_ASSISTANT_MESSAGE_ID);
        assert_eq!(
            updates[0].1.status.as_ref().and_then(|status| status.as_deref()),
            Some("finish")
        );
        let done_count = std::iter::from_fn(|| ws_rx.try_recv().ok())
            .filter(|event| {
                event.name == "message.stream"
                    && event.data["type"] == "thinking"
                    && event.data["data"]["status"] == "done"
            })
            .count();
        assert_eq!(done_count, 1);
    }

    #[tokio::test]
    async fn persistent_thinking_insert_failure_rejects_finish() {
        let repo = Arc::new(RecordingRepo::new());
        repo.fail_message_inserts();
        let bus = Arc::new(TestUserEventBus::new(32));
        let mut ws_rx = bus.subscribe();
        let (tx, _) = broadcast::channel(32);
        let relay = StreamRelay::new(
            test_conversation_id(),
            TEST_ASSISTANT_MESSAGE_ID.into(),
            TEST_USER_ID.into(),
            repo.clone(),
            bus,
            None,
        );
        let rx = tx.subscribe();
        tx.send(AgentStreamEvent::Thinking(ThinkingEventData {
            content: "visible reasoning".into(),
            subject: None,
            duration: None,
            status: Some("thinking".into()),
        }))
        .unwrap();
        tx.send(AgentStreamEvent::Finish(FinishEventData::default())).unwrap();

        let outcome = relay.consume(rx).await;

        assert_eq!(
            outcome.terminal.code(),
            Some(AgentErrorCode::NomifunStateInconsistent)
        );
        assert!(outcome.emitted_response);
        assert!(
            repo.take_inserts().iter().all(|row| row.r#type != "thinking"),
            "failed thinking writes must not be claimed as history"
        );
        let mut stream_types = Vec::new();
        while let Ok(event) = ws_rx.try_recv() {
            if event.name == "message.stream" {
                stream_types.push(event.data["type"].clone());
            }
        }
        assert!(!stream_types.iter().any(|kind| *kind == json!("finish")));
        assert_eq!(stream_types.last(), Some(&json!("error")));
    }

    #[tokio::test]
    async fn lagged_stream_with_live_sender_becomes_one_bounded_terminal_error() {
        let repo = Arc::new(RecordingRepo::new());
        let bus = Arc::new(TestUserEventBus::new(16));
        let mut ws_rx = bus.subscribe();
        let (tx, _) = broadcast::channel(1);
        let rx = tx.subscribe();
        tx.send(AgentStreamEvent::Finish(FinishEventData::default()))
            .unwrap();
        tx.send(AgentStreamEvent::Text(TextEventData {
            content: "overwrites the only finish".into(),
        }))
        .unwrap();

        let relay = StreamRelay::new(
            test_conversation_id(),
            TEST_ASSISTANT_MESSAGE_ID.into(),
            TEST_USER_ID.into(),
            repo,
            bus,
            None,
        );
        let outcome = tokio::time::timeout(Duration::from_secs(2), relay.consume(rx))
            .await
            .expect("live sender must not keep a lagged relay pending");

        assert_eq!(
            outcome.terminal.code(),
            Some(AgentErrorCode::NomifunStreamBroken)
        );
        assert_eq!(tx.receiver_count(), 0, "relay receiver is released after terminal fallback");
        let mut error_events = 0;
        while let Ok(event) = ws_rx.try_recv() {
            if event.name == "message.stream" && event.data["type"] == "error" {
                error_events += 1;
            }
        }
        assert_eq!(error_events, 1);
        assert!(tx.send(AgentStreamEvent::Finish(FinishEventData::default())).is_err());
    }

    // UC-2b: a relay wired with runtime state accumulates the TurnCompleted token
    // usage (input + output) into the conversation's running total — the seam the
    // owning execution attempt reads the accumulated total after settle.
    #[tokio::test]
    async fn turn_completed_accumulates_tokens_into_wired_runtime_state() {
        use nomifun_ai_agent::protocol::events::TurnCompletedEventData;

        let repo = Arc::new(RecordingRepo::new());
        let bus = Arc::new(TestUserEventBus::new(64));
        let (tx, _) = broadcast::channel(64);
        let runtime_state = Arc::new(ConversationRuntimeStateService::default());

        let conversation_id = test_conversation_id();
        let relay = StreamRelay::new(conversation_id.clone(), TEST_ASSISTANT_MESSAGE_ID.into(), TEST_USER_ID.into(), repo, bus, None)
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
        assert_eq!(runtime_state.take_turn_tokens(&conversation_id), Some(180));
    }

    // Zero-regression: a relay WITHOUT runtime state wired (the default chat path)
    // records nothing — no accumulator entry for the conversation.
    #[tokio::test]
    async fn turn_completed_without_runtime_state_records_nothing() {
        use nomifun_ai_agent::protocol::events::TurnCompletedEventData;

        let repo = Arc::new(RecordingRepo::new());
        let bus = Arc::new(TestUserEventBus::new(64));
        let (tx, _) = broadcast::channel(64);
        let observer = Arc::new(ConversationRuntimeStateService::default());

        let conversation_id = test_conversation_id();
        let relay = StreamRelay::new(conversation_id.clone(), TEST_ASSISTANT_MESSAGE_ID.into(), TEST_USER_ID.into(), repo, bus, None);
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
        assert_eq!(observer.take_turn_tokens(&conversation_id), None);
    }

    #[tokio::test]
    async fn run_plan_event_persists_message_for_history_reload() {
        let repo = Arc::new(RecordingRepo::new());
        let bus = Arc::new(TestUserEventBus::new(64));
        let (tx, _) = broadcast::channel(64);

        let relay = StreamRelay::new(
            test_conversation_id(),
            TEST_ASSISTANT_MESSAGE_ID.into(),
            TEST_USER_ID.into(),
            repo.clone(),
            bus.clone(),
            None,
        );
        let rx = tx.subscribe();

        tx.send(AgentStreamEvent::Plan(PlanEventData {
            session_id: Some("session-1".into()),
            source_call_id: None,
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
        MessageId::parse(&plan_msg.id).expect("plan row has a canonical message ID");
        assert_eq!(plan_msg.msg_id.as_deref(), Some(plan_msg.id.as_str()));
        assert_eq!(plan_msg.status.as_deref(), Some("work"));

        let content: serde_json::Value = serde_json::from_str(&plan_msg.content).unwrap();
        assert_eq!(content["session_id"], "session-1");
        assert_eq!(content["entries"].as_array().unwrap().len(), 2);
        assert_eq!(content["entries"][1]["status"], "in_progress");
        let updates = repo.take_updates();
        let (_, terminal_update) = updates
            .iter()
            .find(|(id, _)| id == &plan_msg.id)
            .expect("incomplete plan must be closed with the turn");
        assert_eq!(
            terminal_update.status.as_ref().map(|status| status.as_deref()),
            Some(Some("finish"))
        );
        assert!(outcome.emitted_response);
    }

    #[tokio::test]
    async fn run_plan_event_completes_and_hides_its_source_tool() {
        use nomifun_ai_agent::protocol::events::tool_call::{ToolCallEventData, ToolCallStatus};

        let repo = Arc::new(RecordingRepo::new());
        let bus = Arc::new(TestUserEventBus::new(64));
        let (tx, _) = broadcast::channel(64);
        let relay = StreamRelay::new(
            test_conversation_id(),
            TEST_ASSISTANT_MESSAGE_ID.into(),
            TEST_USER_ID.into(),
            repo.clone(),
            bus,
            None,
        );
        let rx = tx.subscribe();

        tx.send(AgentStreamEvent::ToolCall(ToolCallEventData {
            call_id: "tc-plan".into(),
            name: "update_plan".into(),
            args: json!({"plan": []}),
            status: ToolCallStatus::Running,
            input: Some(json!({"plan": []})),
            output: None,
            description: None,
            artifacts: Vec::new(),
        }))
        .unwrap();
        tx.send(AgentStreamEvent::Plan(PlanEventData {
            session_id: Some("update_plan".into()),
            source_call_id: Some("tc-plan".into()),
            entries: vec![json!({"content": "Build game", "status": "completed"})],
        }))
        .unwrap();
        tx.send(AgentStreamEvent::Error(ErrorEventData::legacy("later provider error", None)))
            .unwrap();

        relay.consume(rx).await;

        let source_id = repo
            .take_inserts()
            .into_iter()
            .find(|row| row.r#type == "tool_call")
            .expect("source tool must be persisted")
            .id;
        MessageId::parse(&source_id).expect("tool row has a canonical message ID");
        let updates = repo.take_updates();
        let source_updates: Vec<_> = updates
            .iter()
            .filter(|(id, _)| id == &source_id)
            .collect();
        assert_eq!(source_updates.len(), 1, "source tool must settle exactly once");
        let update = &source_updates[0].1;
        assert_eq!(update.status.as_ref().map(|v| v.as_deref()), Some(Some("finish")));
        assert_eq!(update.hidden, Some(true));
        let content: serde_json::Value =
            serde_json::from_str(update.content.as_deref().expect("completed source content")).unwrap();
        assert_eq!(content["status"], "completed");
    }

    #[tokio::test]
    async fn run_terminal_error_closes_preparing_agent_status() {
        use nomifun_ai_agent::protocol::events::AgentStatusEventData;

        let repo = Arc::new(RecordingRepo::new());
        let bus = Arc::new(TestUserEventBus::new(64));
        let (tx, _) = broadcast::channel(64);
        let relay = StreamRelay::new(
            test_conversation_id(),
            TEST_TURN_A.into(),
            TEST_USER_ID.into(),
            repo.clone(),
            bus,
            None,
        );
        let rx = tx.subscribe();
        tx.send(AgentStreamEvent::AgentStatus(AgentStatusEventData {
            backend: "nomi".into(),
            status: "preparing".into(),
            agent_name: Some("Nomi".into()),
            session_id: None,
        }))
        .unwrap();
        tx.send(AgentStreamEvent::Error(ErrorEventData::legacy("provider failed", None)))
            .unwrap();

        relay.consume(rx).await;

        let status_id = repo
            .take_inserts()
            .into_iter()
            .find(|row| row.r#type == "agent_status")
            .expect("agent status must be persisted")
            .id;
        MessageId::parse(&status_id).expect("agent status has a canonical message ID");
        let updates = repo.take_updates();
        let (_, update) = updates
            .iter()
            .find(|(id, _)| id == &status_id)
            .expect("preparing agent status must close on terminal error");
        assert_eq!(update.status.as_ref().map(|s| s.as_deref()), Some(Some("error")));
        let content: serde_json::Value = serde_json::from_str(update.content.as_deref().unwrap()).unwrap();
        assert_eq!(content["status"], "error");
    }

    #[tokio::test]
    async fn run_text_tool_text_splits_text_segments() {
        use nomifun_ai_agent::protocol::events::tool_call::{ToolCallEventData, ToolCallStatus};

        let repo = Arc::new(RecordingRepo::new());
        let bus = Arc::new(TestUserEventBus::new(64));
        let (tx, _) = broadcast::channel(64);

        let relay = StreamRelay::new(
            test_conversation_id(),
            TEST_ASSISTANT_MESSAGE_ID.into(),
            TEST_USER_ID.into(),
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
            artifacts: Vec::new(),
        }))
        .unwrap();
        tx.send(AgentStreamEvent::Text(TextEventData { content: "Beta".into() }))
            .unwrap();
        tx.send(AgentStreamEvent::Finish(FinishEventData::default())).unwrap();

        relay.consume(rx).await;

        let inserts = repo.take_inserts();
        let text_msgs: Vec<_> = inserts.iter().filter(|msg| msg.r#type == "text").collect();
        assert_eq!(text_msgs.len(), 2, "text should split across tool boundaries");
        assert_eq!(text_msgs[0].id, TEST_ASSISTANT_MESSAGE_ID);
        assert_ne!(text_msgs[0].id, text_msgs[1].id);

        let mut text_event_msg_ids = Vec::new();
        while let Ok(evt) = ws_rx.try_recv() {
            if evt.name == "message.stream" && (evt.data["type"] == "text" || evt.data["type"] == "content") {
                text_event_msg_ids.push(evt.data["msg_id"].as_str().unwrap_or_default().to_owned());
            }
        }
        assert_eq!(text_event_msg_ids.len(), 2);
        assert_eq!(text_event_msg_ids[0], TEST_ASSISTANT_MESSAGE_ID);
        assert_ne!(text_event_msg_ids[0], text_event_msg_ids[1]);
    }

    #[tokio::test]
    async fn terminal_error_does_not_relabel_completed_text_segments() {
        use nomifun_ai_agent::protocol::events::tool_call::{ToolCallEventData, ToolCallStatus};

        let repo = Arc::new(RecordingRepo::new());
        let bus = Arc::new(TestUserEventBus::new(64));
        let (tx, _) = broadcast::channel(64);
        let relay = StreamRelay::new(
            test_conversation_id(),
            TEST_TURN_A.into(),
            TEST_USER_ID.into(),
            repo.clone(),
            bus,
            None,
        );
        let rx = tx.subscribe();
        tx.send(AgentStreamEvent::Text(TextEventData { content: "Before".into() }))
            .unwrap();
        tx.send(AgentStreamEvent::ToolCall(ToolCallEventData {
            call_id: "tc-1".into(),
            name: "Read".into(),
            args: json!({}),
            status: ToolCallStatus::Completed,
            input: None,
            output: Some("ok".into()),
            description: None,
            artifacts: Vec::new(),
        }))
        .unwrap();
        tx.send(AgentStreamEvent::Text(TextEventData { content: "After".into() }))
            .unwrap();
        tx.send(AgentStreamEvent::Error(ErrorEventData::legacy("provider failed", None)))
            .unwrap();

        relay.consume(rx).await;

        let inserts = repo.take_inserts();
        let text_rows: Vec<_> = inserts.iter().filter(|row| row.r#type == "text").collect();
        assert_eq!(text_rows.len(), 2);
        assert_eq!(text_rows[0].status.as_deref(), Some("finish"));
        assert_eq!(text_rows[1].status.as_deref(), Some("error"));
        let updates = repo.take_updates();
        assert!(
            updates.iter().all(|(id, update)| {
                id != &text_rows[0].id
                    || update.status.as_ref().map(|status| status.as_deref()) != Some(Some("error"))
            }),
            "a later provider error must not corrupt an earlier completed text segment"
        );
    }

    #[tokio::test]
    async fn run_error_with_no_text_stores_tips_message() {
        let repo = Arc::new(RecordingRepo::new());
        let bus = Arc::new(TestUserEventBus::new(64));
        let (tx, _) = broadcast::channel(64);

        let relay = StreamRelay::new(
            test_conversation_id(),
            TEST_ASSISTANT_MESSAGE_ID.into(),
            TEST_USER_ID.into(),
            repo.clone(),
            bus.clone(),
            None,
        );

        let mut ws_rx = bus.subscribe();
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
        assert_eq!(msg.msg_id.as_deref(), Some(msg.id.as_str()));
        assert_ne!(msg.id, TEST_ASSISTANT_MESSAGE_ID);

        let content: serde_json::Value = serde_json::from_str(&msg.content).unwrap();
        assert_eq!(content["content"], "Something went wrong");
        assert_eq!(content["type"], "error");
        assert_eq!(content["turn_id"], TEST_ASSISTANT_MESSAGE_ID);

        let live_error = std::iter::from_fn(|| ws_rx.try_recv().ok())
            .find(|event| event.name == "message.stream" && event.data["type"] == "error")
            .expect("terminal error must be broadcast");
        assert_eq!(live_error.data["msg_id"], msg.id);
        assert_eq!(live_error.data["turn_id"], TEST_ASSISTANT_MESSAGE_ID);
    }

    #[tokio::test]
    async fn partial_text_error_persists_a_distinct_canonical_error_message() {
        let repo = Arc::new(RecordingRepo::new());
        let bus = Arc::new(TestUserEventBus::new(64));
        let (tx, _) = broadcast::channel(64);
        let relay = StreamRelay::new(
            test_conversation_id(),
            TEST_ASSISTANT_MESSAGE_ID.into(),
            TEST_USER_ID.into(),
            repo.clone(),
            bus,
            None,
        );
        let rx = tx.subscribe();
        tx.send(AgentStreamEvent::Text(TextEventData {
            content: "partial answer".into(),
        }))
        .unwrap();
        tx.send(AgentStreamEvent::Error(ErrorEventData::legacy("late provider failure", None)))
            .unwrap();

        relay.consume(rx).await;

        let inserts = repo.take_inserts();
        let text = inserts.iter().find(|row| row.r#type == "text").expect("partial text row");
        let error = inserts.iter().find(|row| row.r#type == "tips").expect("error tips row");
        assert_eq!(text.status.as_deref(), Some("error"));
        assert_eq!(error.status.as_deref(), Some("error"));
        assert_ne!(text.id, error.id, "text and terminal error need independent identities");
        assert_eq!(error.msg_id.as_deref(), Some(error.id.as_str()));
        let content: serde_json::Value = serde_json::from_str(&error.content).unwrap();
        assert_eq!(content["turn_id"], TEST_ASSISTANT_MESSAGE_ID);
    }

    #[tokio::test]
    async fn run_tool_call_then_error_is_post_response() {        // Plan D4 (review #4): a turn that forwarded/persisted a ToolCall and
        // THEN hit a provider fault must report `emitted_response == true`, so
        // the failover seam refuses to switch — re-running the turn would
        // re-execute the tool's side effect (and re-bill it).
        use nomifun_ai_agent::protocol::events::tool_call::{ToolCallEventData, ToolCallStatus};

        let repo = Arc::new(RecordingRepo::new());
        let bus = Arc::new(TestUserEventBus::new(64));
        let (tx, _) = broadcast::channel(64);

        let relay = StreamRelay::new(
            test_conversation_id(),
            TEST_ASSISTANT_MESSAGE_ID.into(),
            TEST_USER_ID.into(),
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
            artifacts: Vec::new(),
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
        let bus = Arc::new(TestUserEventBus::new(64));
        let (tx, _) = broadcast::channel(64);

        let relay = StreamRelay::new(
            test_conversation_id(),
            TEST_ASSISTANT_MESSAGE_ID.into(),
            TEST_USER_ID.into(),
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
            artifacts: Vec::new(),
        }))
        .unwrap();
        tx.send(AgentStreamEvent::Finish(FinishEventData {
            session_id: None,
            stop_reason: Some(TurnStopReason::MaxTokens),
        }))
        .unwrap();

        let outcome = relay.consume(rx).await;
        assert_eq!(outcome.terminal, RelayTerminal::Finish);

        let tool_id = repo
            .take_inserts()
            .into_iter()
            .find(|row| row.r#type == "tool_call")
            .expect("tool call must be persisted")
            .id;
        MessageId::parse(&tool_id).expect("tool row has a canonical message ID");
        let updates = repo.take_updates();
        let (_, update) = updates
            .iter()
            .find(|(id, _)| id == &tool_id)
            .expect("active tool call should be marked failed when the turn is truncated");
        assert_eq!(update.status.as_ref().map(|v| v.as_deref()), Some(Some("error")));
        let content: serde_json::Value = serde_json::from_str(update.content.as_deref().expect("updated content")).unwrap();
        assert_eq!(content["status"], "error");
        assert_eq!(content["output"], "The turn ended before this tool completed: max_tokens");
    }

    #[tokio::test]
    async fn run_scopes_tool_message_identity_to_the_turn() {
        use nomifun_ai_agent::protocol::events::tool_call::{ToolCallEventData, ToolCallStatus};

        let repo = Arc::new(RecordingRepo::new());
        for turn_id in [TEST_TURN_A, TEST_TURN_B] {
            let bus = Arc::new(TestUserEventBus::new(64));
            let (tx, _) = broadcast::channel(64);
            let relay = StreamRelay::new(
                test_conversation_id(),
                turn_id.into(),
                TEST_USER_ID.into(),
                repo.clone(),
                bus,
                None,
            );
            let rx = tx.subscribe();
            tx.send(AgentStreamEvent::ToolCall(ToolCallEventData {
                call_id: "provider-call-1".into(),
                name: "Read".into(),
                args: json!({"path": "a.txt"}),
                status: ToolCallStatus::Completed,
                input: None,
                output: Some("ok".into()),
                description: None,
                artifacts: Vec::new(),
            }))
            .unwrap();
            tx.send(AgentStreamEvent::Finish(FinishEventData::default())).unwrap();
            relay.consume(rx).await;
        }

        let inserts = repo.take_inserts();
        let ids: Vec<_> = inserts
            .iter()
            .filter(|row| row.r#type == "tool_call")
            .map(|row| row.id.as_str())
            .collect();
        assert_eq!(ids.len(), 2);
        assert!(ids.iter().all(|id| MessageId::parse(*id).is_ok()));
        assert_ne!(ids[0], ids[1], "the same provider call key is scoped by turn");
        let turns: Vec<_> = inserts
            .iter()
            .filter(|row| row.r#type == "tool_call")
            .map(|row| serde_json::from_str::<serde_json::Value>(&row.content).unwrap()["turn_id"].clone())
            .collect();
        assert_eq!(turns, [json!(TEST_TURN_A), json!(TEST_TURN_B)]);
    }

    #[tokio::test]
    async fn run_does_not_regress_a_terminal_tool_to_running() {
        use nomifun_ai_agent::protocol::events::tool_call::{ToolCallEventData, ToolCallStatus};

        let repo = Arc::new(RecordingRepo::new());
        let bus = Arc::new(TestUserEventBus::new(64));
        let (tx, _) = broadcast::channel(64);
        let relay = StreamRelay::new(
            test_conversation_id(),
            TEST_TURN_A.into(),
            TEST_USER_ID.into(),
            repo.clone(),
            bus,
            None,
        );
        let rx = tx.subscribe();
        let event = |status, output| {
            AgentStreamEvent::ToolCall(ToolCallEventData {
                call_id: "provider-call-1".into(),
                name: "Bash".into(),
                args: json!({"command": "true"}),
                status,
                input: None,
                output,
                description: None,
                artifacts: Vec::new(),
            })
        };
        tx.send(event(ToolCallStatus::Completed, Some("ok".into()))).unwrap();
        tx.send(event(ToolCallStatus::Running, None)).unwrap();
        tx.send(AgentStreamEvent::Finish(FinishEventData::default())).unwrap();

        relay.consume(rx).await;

        let updates = repo.take_updates();
        assert!(
            updates.iter().all(|(_, update)| update.status.as_ref().map(|s| s.as_deref()) != Some(Some("work"))),
            "a late running event must not overwrite a terminal tool"
        );
        assert!(
            updates.iter().all(|(_, update)| update.status.as_ref().map(|s| s.as_deref()) != Some(Some("error"))),
            "a late running event must not reactivate the tool for terminal cleanup"
        );
    }

    #[tokio::test]
    async fn run_does_not_forward_late_completed_artifact_after_tool_error() {
        use nomifun_ai_agent::artifact_store::{ArtifactKind, PersistedArtifact};
        use nomifun_ai_agent::protocol::events::tool_call::{ToolCallEventData, ToolCallStatus};

        let repo = Arc::new(RecordingRepo::new());
        let bus = Arc::new(TestUserEventBus::new(64));
        let mut ws_rx = bus.subscribe();
        let (tx, _) = broadcast::channel(64);
        let relay = StreamRelay::new(
            test_conversation_id(),
            TEST_TURN_A.into(),
            TEST_USER_ID.into(),
            repo.clone(),
            bus,
            None,
        );
        let rx = tx.subscribe();
        let event = |status, artifacts| {
            AgentStreamEvent::ToolCall(ToolCallEventData {
                call_id: "provider-call-1".into(),
                name: "ImageGeneration".into(),
                args: json!({"prompt": "cat"}),
                status,
                input: None,
                output: None,
                description: None,
                artifacts,
            })
        };
        tx.send(event(ToolCallStatus::Error, Vec::new())).unwrap();
        tx.send(event(
            ToolCallStatus::Completed,
            vec![PersistedArtifact {
                id: "stale".into(),
                kind: ArtifactKind::Image,
                mime_type: "image/png".into(),
                path: "/workspace/old.png".into(),
                relative_path: "nomifun-artifacts/old.png".into(),
                size_bytes: 10,
                sha256: "a".repeat(64),
            }],
        ))
        .unwrap();
        tx.send(AgentStreamEvent::Finish(FinishEventData::default())).unwrap();

        relay.consume(rx).await;

        let rows = repo.take_inserts();
        let row = rows
            .iter()
            .find(|row| row.r#type == "tool_call")
            .expect("failed tool call is persisted");
        assert_eq!(row.status.as_deref(), Some("error"));
        let content: serde_json::Value = serde_json::from_str(&row.content).unwrap();
        assert_eq!(content["artifacts"], json!([]));

        let mut tool_events = Vec::new();
        while let Ok(event) = ws_rx.try_recv() {
            if event.name == "message.stream" && event.data["type"] == "tool_call" {
                tool_events.push(event.data);
            }
        }
        assert_eq!(tool_events.len(), 1, "late terminal success must not reach live UI");
        assert_eq!(tool_events[0]["data"]["status"], "error");
    }

    #[tokio::test]
    async fn run_keeps_completed_artifact_after_successful_turn_finish() {
        use nomifun_ai_agent::protocol::events::tool_call::{ToolCallEventData, ToolCallStatus};

        let repo = Arc::new(RecordingRepo::new());
        let bus = Arc::new(TestUserEventBus::new(64));
        let mut ws_rx = bus.subscribe();
        let (tx, _) = broadcast::channel(64);
        let workspace = std::env::temp_dir().join(format!(
            "nomifun-conversation-artifact-test-{}",
            MessageId::new().into_string()
        ));
        std::fs::create_dir_all(&workspace).expect("create test workspace");
        let artifact = persisted_png_artifact(&workspace);
        let relay = StreamRelay::new(
            test_conversation_id(),
            TEST_TURN_A.into(),
            TEST_USER_ID.into(),
            repo.clone(),
            bus,
            None,
        )
        .with_artifact_workspace(workspace.clone());
        let rx = tx.subscribe();
        tx.send(AgentStreamEvent::ToolCall(ToolCallEventData {
            call_id: "artifact-success".into(),
            name: "ImageGeneration".into(),
            args: json!({"prompt": "cat"}),
            status: ToolCallStatus::Completed,
            input: None,
            output: Some("generated".into()),
            description: None,
            artifacts: vec![artifact],
        }))
        .unwrap();
        tx.send(AgentStreamEvent::Finish(FinishEventData::default())).unwrap();

        relay.consume(rx).await;

        let row = repo
            .take_inserts()
            .into_iter()
            .find(|row| row.r#type == "tool_call")
            .expect("artifact tool gets a provisional row");
        assert_eq!(row.status.as_deref(), Some("work"));
        let provisional: serde_json::Value = serde_json::from_str(&row.content).unwrap();
        assert_eq!(provisional["status"], "running");
        assert_eq!(provisional["artifacts"], json!([]));
        assert_eq!(provisional[ARTIFACT_DELIVERY_COMMITTED_FIELD], false);

        let updates = repo.take_updates();
        let committed = updates
            .iter()
            .rev()
            .find(|(id, update)| {
                id == &row.id
                    && update.status.as_ref().map(|s| s.as_deref()) == Some(Some("finish"))
            })
            .expect("successful enclosing turn promotes the artifact receipt");
        let committed_content: serde_json::Value =
            serde_json::from_str(committed.1.content.as_deref().expect("committed content")).unwrap();
        assert_eq!(committed_content["artifacts"].as_array().map(Vec::len), Some(1));
        assert_eq!(committed_content[ARTIFACT_DELIVERY_COMMITTED_FIELD], true);

        let mut tool_statuses = Vec::new();
        let mut stream_types = Vec::new();
        while let Ok(event) = ws_rx.try_recv() {
            if event.name != "message.stream" {
                continue;
            }
            stream_types.push(event.data["type"].clone());
            if event.data["type"] == "tool_call"
                && let Some(status) = event.data["data"]["status"].as_str()
            {
                tool_statuses.push(status.to_owned());
            }
        }
        assert_eq!(tool_statuses, ["running", "completed"]);
        assert_eq!(stream_types.last(), Some(&json!("finish")));
        std::fs::remove_dir_all(workspace).expect("remove test workspace");
    }

    #[tokio::test]
    async fn atomic_artifact_commit_failure_rejects_finish_and_leaves_only_provisional_history() {
        use nomifun_ai_agent::protocol::events::tool_call::{ToolCallEventData, ToolCallStatus};

        let repo = Arc::new(RecordingRepo::new());
        repo.fail_artifact_commits();
        let bus = Arc::new(TestUserEventBus::new(64));
        let mut ws_rx = bus.subscribe();
        let (tx, _) = broadcast::channel(64);
        let relay = StreamRelay::new(
            test_conversation_id(),
            TEST_TURN_A.into(),
            TEST_USER_ID.into(),
            repo.clone(),
            bus,
            None,
        );
        let rx = tx.subscribe();
        tx.send(AgentStreamEvent::ToolCall(ToolCallEventData {
            call_id: "artifact-commit-fails".into(),
            name: "ImageGeneration".into(),
            args: json!({"prompt": "cat"}),
            status: ToolCallStatus::Completed,
            input: None,
            output: Some("generated".into()),
            description: None,
            artifacts: vec![test_artifact("commit-fails")],
        }))
        .unwrap();
        tx.send(AgentStreamEvent::Finish(FinishEventData::default())).unwrap();

        let outcome = relay.consume(rx).await;
        assert!(outcome.terminal.is_error());
        assert_eq!(
            outcome.terminal.code(),
            Some(AgentErrorCode::NomifunStateInconsistent)
        );

        let row = repo
            .take_inserts()
            .into_iter()
            .find(|row| row.r#type == "tool_call")
            .expect("phase one persists a fail-closed row");
        assert_eq!(row.status.as_deref(), Some("work"));
        let content: Value = serde_json::from_str(&row.content).unwrap();
        assert_eq!(content["status"], "running");
        assert_eq!(content["artifacts"], json!([]));
        assert_eq!(content[ARTIFACT_DELIVERY_COMMITTED_FIELD], false);

        let mut tool_statuses = Vec::new();
        let mut stream_types = Vec::new();
        while let Ok(event) = ws_rx.try_recv() {
            if event.name != "message.stream" {
                continue;
            }
            stream_types.push(event.data["type"].clone());
            if event.data["type"] == "tool_call"
                && let Some(status) = event.data["data"]["status"].as_str()
            {
                tool_statuses.push(status.to_owned());
            }
        }
        assert_eq!(tool_statuses, ["running", "error"]);
        assert!(!stream_types.iter().any(|kind| *kind == json!("finish")));
        assert_eq!(stream_types.last(), Some(&json!("error")));
    }

    #[tokio::test(start_paused = true)]
    async fn atomic_artifact_commit_timeout_rejects_finish_without_exposing_completed_receipt() {
        use nomifun_ai_agent::protocol::events::tool_call::{ToolCallEventData, ToolCallStatus};

        let repo = Arc::new(RecordingRepo::new());
        repo.block_artifact_commits();
        let bus = Arc::new(TestUserEventBus::new(64));
        let mut ws_rx = bus.subscribe();
        let (tx, _) = broadcast::channel(64);
        let relay = StreamRelay::new(
            test_conversation_id(),
            TEST_TURN_A.into(),
            TEST_USER_ID.into(),
            repo.clone(),
            bus,
            None,
        );
        let rx = tx.subscribe();
        tx.send(AgentStreamEvent::ToolCall(ToolCallEventData {
            call_id: "artifact-commit-times-out".into(),
            name: "ImageGeneration".into(),
            args: json!({"prompt": "cat"}),
            status: ToolCallStatus::Completed,
            input: None,
            output: Some("generated".into()),
            description: None,
            artifacts: vec![test_artifact("commit-timeout")],
        }))
        .unwrap();
        tx.send(AgentStreamEvent::Finish(FinishEventData::default())).unwrap();

        let outcome = relay.consume(rx).await;
        assert_eq!(
            outcome.terminal.code(),
            Some(AgentErrorCode::NomifunStateInconsistent)
        );
        let row = repo
            .take_inserts()
            .into_iter()
            .find(|row| row.r#type == "tool_call")
            .expect("timeout leaves the provisional row intact");
        assert_eq!(row.status.as_deref(), Some("work"));
        let content: Value = serde_json::from_str(&row.content).unwrap();
        assert_eq!(content["artifacts"], json!([]));

        let mut observed_completed = false;
        let mut stream_types = Vec::new();
        while let Ok(event) = ws_rx.try_recv() {
            if event.name != "message.stream" {
                continue;
            }
            stream_types.push(event.data["type"].clone());
            observed_completed |= event.data["type"] == "tool_call"
                && event.data["data"]["status"] == "completed";
        }
        assert!(!observed_completed);
        assert!(!stream_types.iter().any(|kind| *kind == json!("finish")));
        assert_eq!(stream_types.last(), Some(&json!("error")));
    }

    #[tokio::test]
    async fn artifact_delivery_never_uses_random_message_identity_after_correlation_failure() {
        use nomifun_ai_agent::protocol::events::tool_call::{ToolCallEventData, ToolCallStatus};

        let repo = Arc::new(RecordingRepo::new());
        repo.fail_message_correlations();
        let bus = Arc::new(TestUserEventBus::new(64));
        let mut ws_rx = bus.subscribe();
        let (tx, _) = broadcast::channel(64);
        let relay = StreamRelay::new(
            test_conversation_id(),
            TEST_TURN_A.into(),
            TEST_USER_ID.into(),
            repo.clone(),
            bus,
            None,
        );
        let rx = tx.subscribe();
        tx.send(AgentStreamEvent::ToolCall(ToolCallEventData {
            call_id: "artifact-without-durable-id".into(),
            name: "ImageGeneration".into(),
            args: json!({"prompt": "cat"}),
            status: ToolCallStatus::Completed,
            input: None,
            output: Some("generated".into()),
            description: None,
            artifacts: vec![test_artifact("identity-failure")],
        }))
        .unwrap();
        tx.send(AgentStreamEvent::Finish(FinishEventData::default())).unwrap();

        let outcome = relay.consume(rx).await;
        assert!(outcome.terminal.is_error());
        assert!(repo.take_inserts().iter().all(|row| {
            row.r#type != "tool_call" || row.status.as_deref() != Some("finish")
        }));

        let mut saw_tool_error = false;
        let mut saw_tool_completed = false;
        let mut stream_types = Vec::new();
        while let Ok(event) = ws_rx.try_recv() {
            if event.name != "message.stream" {
                continue;
            }
            stream_types.push(event.data["type"].clone());
            if event.data["type"] == "tool_call" {
                saw_tool_error |= event.data["data"]["status"] == "error";
                saw_tool_completed |= event.data["data"]["status"] == "completed";
            }
        }
        assert!(saw_tool_error);
        assert!(!saw_tool_completed);
        assert!(!stream_types.iter().any(|kind| *kind == json!("finish")));
        assert_eq!(stream_types.last(), Some(&json!("error")));
    }

    #[tokio::test]
    async fn run_retracts_completed_artifact_when_enclosing_turn_errors() {
        use nomifun_ai_agent::protocol::events::tool_call::{ToolCallEventData, ToolCallStatus};

        let repo = Arc::new(RecordingRepo::new());
        let bus = Arc::new(TestUserEventBus::new(64));
        let mut ws_rx = bus.subscribe();
        let (tx, _) = broadcast::channel(64);
        let relay = StreamRelay::new(
            test_conversation_id(),
            TEST_TURN_A.into(),
            TEST_USER_ID.into(),
            repo.clone(),
            bus,
            None,
        );
        let rx = tx.subscribe();
        tx.send(AgentStreamEvent::ToolCall(ToolCallEventData {
            call_id: "artifact-then-error".into(),
            name: "ImageGeneration".into(),
            args: json!({"prompt": "cat"}),
            status: ToolCallStatus::Completed,
            input: None,
            output: Some("generated".into()),
            description: None,
            artifacts: vec![test_artifact("retracted")],
        }))
        .unwrap();
        tx.send(AgentStreamEvent::Error(ErrorEventData::legacy(
            "provider failed after artifact delivery",
            None,
        )))
        .unwrap();

        relay.consume(rx).await;

        let row = repo
            .take_inserts()
            .into_iter()
            .find(|row| row.r#type == "tool_call")
            .expect("completed artifact tool is persisted provisionally");
        assert_eq!(row.status.as_deref(), Some("work"));
        let provisional: serde_json::Value = serde_json::from_str(&row.content).unwrap();
        assert_eq!(provisional["artifacts"], json!([]));
        assert_eq!(provisional[ARTIFACT_DELIVERY_COMMITTED_FIELD], false);
        let updates = repo.take_updates();
        let correction = updates
            .iter()
            .rev()
            .find(|(id, _)| id == &row.id)
            .expect("global turn error must correct the completed artifact row");
        assert_eq!(
            correction.1.status.as_ref().map(|status| status.as_deref()),
            Some(Some("error"))
        );
        let content: serde_json::Value =
            serde_json::from_str(correction.1.content.as_deref().expect("corrected content")).unwrap();
        assert_eq!(content["status"], "error");
        assert_eq!(content["artifacts"], json!([]));

        let mut last_tool = None;
        let mut stream_types = Vec::new();
        while let Ok(event) = ws_rx.try_recv() {
            if event.name == "message.stream" {
                stream_types.push(event.data["type"].clone());
                if event.data["type"] == "tool_call" {
                    last_tool = Some(event.data);
                }
            }
        }
        let last_tool = last_tool.expect("live UI receives the terminal artifact correction");
        assert_eq!(last_tool["data"]["status"], "error");
        assert_eq!(last_tool["data"]["artifacts"], json!([]));
        assert_eq!(
            stream_types.last(),
            Some(&json!("error")),
            "the enclosing terminal must be published after artifact retraction"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn terminal_timeout_still_broadcasts_artifact_retraction_before_error() {
        use nomifun_ai_agent::protocol::events::tool_call::{ToolCallEventData, ToolCallStatus};

        let repo = Arc::new(RecordingRepo::new());
        let bus = Arc::new(TestUserEventBus::new(64));
        let mut ws_rx = bus.subscribe();
        let (tx, _) = broadcast::channel(64);
        let relay = StreamRelay::new(
            test_conversation_id(),
            TEST_TURN_A.into(),
            TEST_USER_ID.into(),
            repo.clone(),
            bus,
            None,
        );
        let rx = tx.subscribe();
        tx.send(AgentStreamEvent::ToolCall(ToolCallEventData {
            call_id: "artifact-before-wedged-db".into(),
            name: "ImageGeneration".into(),
            args: json!({"prompt": "cat"}),
            status: ToolCallStatus::Completed,
            input: None,
            output: Some("generated".into()),
            description: None,
            artifacts: vec![test_artifact("wedged-db")],
        }))
        .unwrap();
        tx.send(AgentStreamEvent::Error(ErrorEventData::legacy(
            "provider failed after artifact delivery",
            None,
        )))
        .unwrap();
        // The completed row above can be inserted, but its terminal correction
        // now wedges forever. Paused Tokio time advances directly to the relay's
        // hard terminal timeout.
        repo.block_message_updates();

        let outcome = relay.consume(rx).await;
        assert!(outcome.terminal.is_error());

        let provisional = repo
            .take_inserts()
            .into_iter()
            .find(|row| row.r#type == "tool_call")
            .expect("the pre-terminal artifact projection is durable");
        assert_eq!(provisional.status.as_deref(), Some("work"));
        let content: serde_json::Value = serde_json::from_str(&provisional.content).unwrap();
        assert_eq!(content["status"], "running");
        assert_eq!(content["artifacts"], json!([]));
        assert_eq!(content[ARTIFACT_DELIVERY_COMMITTED_FIELD], false);

        let mut final_tool_status = None;
        let mut stream_types = Vec::new();
        while let Ok(event) = ws_rx.try_recv() {
            if event.name != "message.stream" {
                continue;
            }
            stream_types.push(event.data["type"].clone());
            if event.data["type"] == "tool_call" {
                final_tool_status = event.data["data"]["status"].as_str().map(str::to_owned);
            }
        }
        assert_eq!(final_tool_status.as_deref(), Some("error"));
        assert_eq!(
            stream_types.last(),
            Some(&json!("error")),
            "hard-timeout fallback terminal must remain after the synchronous receipt retraction"
        );
    }

    #[tokio::test]
    async fn run_retracts_completed_acp_artifact_when_enclosing_turn_errors() {
        use nomifun_ai_agent::protocol::events::{
            AcpToolCallContentItem,
            tool_call::{
                AcpToolCallEventData, AcpToolCallSessionUpdateKind, AcpToolCallStatus,
                AcpToolCallUpdateData,
            },
        };

        let repo = Arc::new(RecordingRepo::new());
        let bus = Arc::new(TestUserEventBus::new(64));
        let mut ws_rx = bus.subscribe();
        let (tx, _) = broadcast::channel(64);
        let relay = StreamRelay::new(
            test_conversation_id(),
            TEST_TURN_A.into(),
            TEST_USER_ID.into(),
            repo.clone(),
            bus,
            None,
        );
        let rx = tx.subscribe();
        tx.send(AgentStreamEvent::AcpToolCall(AcpToolCallEventData {
            session_id: "session-artifact".into(),
            update: AcpToolCallUpdateData {
                session_update: AcpToolCallSessionUpdateKind::ToolCallUpdate,
                tool_call_id: "acp-artifact-then-error".into(),
                status: Some(AcpToolCallStatus::Completed),
                title: Some("Generate image".into()),
                kind: None,
                raw_input: None,
                raw_output: Some(json!("generated")),
                content: Some(vec![AcpToolCallContentItem::Artifact {
                    artifact: test_artifact("acp-retracted"),
                    source_uri: None,
                }]),
                locations: None,
            },
            meta: None,
        }))
        .unwrap();
        tx.send(AgentStreamEvent::Error(ErrorEventData::legacy(
            "provider failed after ACP artifact delivery",
            None,
        )))
        .unwrap();

        relay.consume(rx).await;

        let row = repo
            .take_inserts()
            .into_iter()
            .find(|row| row.r#type == "acp_tool_call")
            .expect("completed ACP artifact tool is persisted provisionally");
        assert_eq!(row.status.as_deref(), Some("work"));
        let provisional: serde_json::Value = serde_json::from_str(&row.content).unwrap();
        assert_eq!(provisional["update"]["status"], "in_progress");
        assert!(
            provisional["update"]["content"]
                .as_array()
                .is_some_and(Vec::is_empty)
        );
        assert_eq!(provisional[ARTIFACT_DELIVERY_COMMITTED_FIELD], false);
        let updates = repo.take_updates();
        let correction = updates
            .iter()
            .rev()
            .find(|(id, _)| id == &row.id)
            .expect("global turn error must correct the completed ACP artifact row");
        assert_eq!(
            correction.1.status.as_ref().map(|status| status.as_deref()),
            Some(Some("error"))
        );
        let content: serde_json::Value =
            serde_json::from_str(correction.1.content.as_deref().expect("corrected content")).unwrap();
        assert_eq!(content["update"]["status"], "failed");
        assert!(
            content["update"]["content"]
                .as_array()
                .is_some_and(Vec::is_empty),
            "failed ACP projection must remove artifact/resource-link delivery blocks"
        );

        let mut last_acp = None;
        let mut stream_types = Vec::new();
        while let Ok(event) = ws_rx.try_recv() {
            if event.name == "message.stream" {
                stream_types.push(event.data["type"].clone());
                if event.data["type"] == "acp_tool_call" {
                    last_acp = Some(event.data);
                }
            }
        }
        let last_acp = last_acp.expect("live UI receives the terminal ACP artifact correction");
        assert_eq!(last_acp["data"]["update"]["status"], "failed");
        assert!(
            last_acp["data"]["update"]["content"]
                .as_array()
                .is_some_and(Vec::is_empty)
        );
        assert_eq!(stream_types.last(), Some(&json!("error")));
    }

    #[tokio::test]
    async fn channel_close_retracts_completed_generic_and_acp_artifacts_before_terminal() {
        use nomifun_ai_agent::protocol::events::{
            AcpToolCallContentItem,
            tool_call::{
                AcpToolCallEventData, AcpToolCallSessionUpdateKind, AcpToolCallStatus,
                AcpToolCallUpdateData, ToolCallEventData, ToolCallStatus,
            },
        };

        let repo = Arc::new(RecordingRepo::new());
        let bus = Arc::new(TestUserEventBus::new(64));
        let mut ws_rx = bus.subscribe();
        let (tx, _) = broadcast::channel(64);
        let relay = StreamRelay::new(
            test_conversation_id(),
            TEST_TURN_A.into(),
            TEST_USER_ID.into(),
            repo.clone(),
            bus,
            None,
        );
        let rx = tx.subscribe();
        tx.send(AgentStreamEvent::ToolCall(ToolCallEventData {
            call_id: "generic-before-close".into(),
            name: "ImageGeneration".into(),
            args: json!({"prompt": "cat"}),
            status: ToolCallStatus::Completed,
            input: None,
            output: Some("generated".into()),
            description: None,
            artifacts: vec![test_artifact("generic-close")],
        }))
        .unwrap();
        tx.send(AgentStreamEvent::AcpToolCall(AcpToolCallEventData {
            session_id: "session-close".into(),
            update: AcpToolCallUpdateData {
                session_update: AcpToolCallSessionUpdateKind::ToolCallUpdate,
                tool_call_id: "acp-before-close".into(),
                status: Some(AcpToolCallStatus::Completed),
                title: Some("Generate image".into()),
                kind: None,
                raw_input: None,
                raw_output: Some(json!("generated")),
                content: Some(vec![AcpToolCallContentItem::Artifact {
                    artifact: test_artifact("acp-close"),
                    source_uri: None,
                }]),
                locations: None,
            },
            meta: None,
        }))
        .unwrap();
        drop(tx);

        let outcome = relay.consume(rx).await;
        assert_eq!(outcome.terminal, RelayTerminal::ChannelClosed);

        let rows = repo.take_inserts();
        let generic_id = rows
            .iter()
            .find(|row| row.r#type == "tool_call")
            .expect("generic artifact row")
            .id
            .clone();
        let acp_id = rows
            .iter()
            .find(|row| row.r#type == "acp_tool_call")
            .expect("ACP artifact row")
            .id
            .clone();
        let updates = repo.take_updates();
        for id in [generic_id, acp_id] {
            let update = updates
                .iter()
                .rev()
                .find(|(updated_id, _)| updated_id == &id)
                .expect("closed stream must retract every completed artifact lifecycle");
            assert_eq!(
                update.1.status.as_ref().map(|status| status.as_deref()),
                Some(Some("error"))
            );
        }

        let mut stream_types = Vec::new();
        while let Ok(event) = ws_rx.try_recv() {
            if event.name == "message.stream" {
                stream_types.push(event.data["type"].clone());
            }
        }
        assert_eq!(stream_types.last(), Some(&json!("error")));
        assert_eq!(
            stream_types
                .iter()
                .filter(|event_type| **event_type == json!("tool_call"))
                .count(),
            2,
            "completed generic tool plus its error correction are both visible"
        );
        assert_eq!(
            stream_types
                .iter()
                .filter(|event_type| **event_type == json!("acp_tool_call"))
                .count(),
            2,
            "completed ACP tool plus its error correction are both visible"
        );
    }

    #[tokio::test]
    async fn generic_artifact_tracking_limit_fails_closed_without_an_untracked_success() {
        use nomifun_ai_agent::protocol::events::tool_call::{ToolCallEventData, ToolCallStatus};

        let repo = Arc::new(RecordingRepo::new());
        let bus = Arc::new(TestUserEventBus::new(4096));
        let mut ws_rx = bus.subscribe();
        let (tx, _) = broadcast::channel(1024);
        let relay = StreamRelay::new(
            test_conversation_id(),
            TEST_TURN_A.into(),
            TEST_USER_ID.into(),
            repo.clone(),
            bus,
            None,
        );
        let rx = tx.subscribe();
        for index in 0..=MAX_TERMINAL_ACTIVE_ITEMS {
            tx.send(AgentStreamEvent::ToolCall(ToolCallEventData {
                call_id: format!("artifact-{index}"),
                name: "ImageGeneration".into(),
                args: json!({"prompt": "cat"}),
                status: ToolCallStatus::Completed,
                input: None,
                output: Some("generated".into()),
                description: None,
                artifacts: vec![test_artifact(&format!("artifact-{index}"))],
            }))
            .unwrap();
        }
        tx.send(AgentStreamEvent::Finish(FinishEventData::default())).unwrap();

        let outcome = relay.consume(rx).await;
        assert!(outcome.terminal.is_error());

        let mut final_statuses = HashMap::new();
        let mut stream_types = Vec::new();
        while let Ok(event) = ws_rx.try_recv() {
            if event.name != "message.stream" {
                continue;
            }
            stream_types.push(event.data["type"].clone());
            if event.data["type"] == "tool_call"
                && let (Some(call_id), Some(status)) = (
                    event.data["data"]["call_id"].as_str(),
                    event.data["data"]["status"].as_str(),
                )
            {
                final_statuses.insert(call_id.to_owned(), status.to_owned());
            }
        }
        assert_eq!(final_statuses.len(), MAX_TERMINAL_ACTIVE_ITEMS + 1);
        assert!(final_statuses.values().all(|status| status == "error"));
        assert_eq!(stream_types.last(), Some(&json!("error")));

        let rows = repo.take_inserts();
        assert_eq!(
            rows.iter().filter(|row| row.r#type == "tool_call").count(),
            MAX_TERMINAL_ACTIVE_ITEMS + 1
        );
        assert_eq!(
            repo.take_updates()
                .iter()
                .filter(|(_, update)| {
                    update.status.as_ref().map(|status| status.as_deref()) == Some(Some("error"))
                })
                .count(),
            MAX_TERMINAL_ACTIVE_ITEMS
        );
    }

    #[tokio::test]
    async fn acp_artifact_tracking_limit_fails_closed_without_an_untracked_success() {
        use nomifun_ai_agent::protocol::events::{
            AcpToolCallContentItem,
            tool_call::{
                AcpToolCallEventData, AcpToolCallSessionUpdateKind, AcpToolCallStatus,
                AcpToolCallUpdateData,
            },
        };

        let repo = Arc::new(RecordingRepo::new());
        let bus = Arc::new(TestUserEventBus::new(4096));
        let mut ws_rx = bus.subscribe();
        let (tx, _) = broadcast::channel(1024);
        let relay = StreamRelay::new(
            test_conversation_id(),
            TEST_TURN_A.into(),
            TEST_USER_ID.into(),
            repo,
            bus,
            None,
        );
        let rx = tx.subscribe();
        for index in 0..=MAX_TERMINAL_ACTIVE_ITEMS {
            tx.send(AgentStreamEvent::AcpToolCall(AcpToolCallEventData {
                session_id: "session-overflow".into(),
                update: AcpToolCallUpdateData {
                    session_update: AcpToolCallSessionUpdateKind::ToolCallUpdate,
                    tool_call_id: format!("acp-artifact-{index}"),
                    status: Some(AcpToolCallStatus::Completed),
                    title: Some("Generate image".into()),
                    kind: None,
                    raw_input: None,
                    raw_output: Some(json!("generated")),
                    content: Some(vec![AcpToolCallContentItem::Artifact {
                        artifact: test_artifact(&format!("acp-artifact-{index}")),
                        source_uri: None,
                    }]),
                    locations: None,
                },
                meta: None,
            }))
            .unwrap();
        }
        tx.send(AgentStreamEvent::Finish(FinishEventData::default())).unwrap();

        let outcome = relay.consume(rx).await;
        assert!(outcome.terminal.is_error());

        let mut final_statuses = HashMap::new();
        let mut stream_types = Vec::new();
        while let Ok(event) = ws_rx.try_recv() {
            if event.name != "message.stream" {
                continue;
            }
            stream_types.push(event.data["type"].clone());
            if event.data["type"] == "acp_tool_call"
                && let (Some(call_id), Some(status)) = (
                    event.data["data"]["update"]["tool_call_id"].as_str(),
                    event.data["data"]["update"]["status"].as_str(),
                )
            {
                final_statuses.insert(call_id.to_owned(), status.to_owned());
            }
        }
        assert_eq!(final_statuses.len(), MAX_TERMINAL_ACTIVE_ITEMS + 1);
        assert!(final_statuses.values().all(|status| status == "failed"));
        assert_eq!(stream_types.last(), Some(&json!("error")));
    }

    #[tokio::test]
    async fn persistence_does_not_regress_a_terminal_tool_after_relay_restart() {
        use nomifun_ai_agent::protocol::events::tool_call::{ToolCallEventData, ToolCallStatus};

        let repo = Arc::new(RecordingRepo::new());
        for status in [ToolCallStatus::Completed, ToolCallStatus::Running] {
            let bus = Arc::new(TestUserEventBus::new(64));
            let (tx, _) = broadcast::channel(64);
            let relay = StreamRelay::new(
                test_conversation_id(),
                TEST_TURN_A.into(),
                TEST_USER_ID.into(),
                repo.clone(),
                bus,
                None,
            );
            let rx = tx.subscribe();
            tx.send(AgentStreamEvent::ToolCall(ToolCallEventData {
                call_id: "provider-call-1".into(),
                name: "Bash".into(),
                args: json!({"command": "true"}),
                status,
                input: None,
                output: (status == ToolCallStatus::Completed).then(|| "ok".into()),
                description: None,
                artifacts: Vec::new(),
            }))
            .unwrap();
            tx.send(AgentStreamEvent::Finish(FinishEventData::default())).unwrap();
            relay.consume(rx).await;
        }

        let updates = repo.take_updates();
        assert!(
            updates.iter().all(|(_, update)| update.status.as_ref().map(|s| s.as_deref()) != Some(Some("work"))),
            "stored terminal state must reject a late running update after relay restart"
        );
    }

    #[tokio::test]
    async fn run_suppresses_pre_response_error_when_failover_will_fire() {
        // review #1/#5: with a suppressor that accepts the fault's code, a
        // pre-response (no text) provider error must NOT broadcast a WS error
        // event NOR persist an error `tips` row — the user only ever sees the
        // backup model's turn. The swallowed event is handed back for re-surface.
        let repo = Arc::new(RecordingRepo::new());
        let bus = Arc::new(TestUserEventBus::new(64));
        let (tx, _) = broadcast::channel(64);

        let relay = StreamRelay::new(
            test_conversation_id(),
            TEST_ASSISTANT_MESSAGE_ID.into(),
            TEST_USER_ID.into(),
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
        let bus = Arc::new(TestUserEventBus::new(64));
        let (tx, _) = broadcast::channel(64);

        let relay = StreamRelay::new(
            test_conversation_id(),
            TEST_ASSISTANT_MESSAGE_ID.into(),
            TEST_USER_ID.into(),
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
        let bus = Arc::new(TestUserEventBus::new(64));
        let (tx, _) = broadcast::channel(64);

        let relay = StreamRelay::new(
            test_conversation_id(),
            TEST_ASSISTANT_MESSAGE_ID.into(),
            TEST_USER_ID.into(),
            repo.clone(),
            bus.clone(),
            None,
        );

        let mut ws_rx = bus.subscribe();
        let rx = tx.subscribe();
        let (send_error_tx, send_error_rx) = tokio::sync::oneshot::channel();
        send_error_tx
            .send(Err(AgentSendError::from_app_error(nomifun_common::AppError::BadGateway(
                "provider returned 401 invalid api key".into(),
            ))))
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
        let bus = Arc::new(TestUserEventBus::new(64));
        let (tx, _) = broadcast::channel(64);

        let relay = StreamRelay::new(
            test_conversation_id(),
            TEST_ASSISTANT_MESSAGE_ID.into(),
            TEST_USER_ID.into(),
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
            let _ = send_error_tx.send(Err(send_error));
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
        let bus = Arc::new(TestUserEventBus::new(64));
        let (tx, _) = broadcast::channel(64);

        let relay = StreamRelay::new(
            test_conversation_id(),
            TEST_ASSISTANT_MESSAGE_ID.into(),
            TEST_USER_ID.into(),
            repo.clone(),
            bus.clone(),
            None,
        );

        let rx = tx.subscribe();
        let (send_error_tx, send_error_rx) = tokio::sync::oneshot::channel();
        send_error_tx
            .send(Err(AgentSendError::from_app_error(nomifun_common::AppError::BadGateway(
                "provider returned 401 invalid api key".into(),
            ))))
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
    async fn closed_send_task_signal_is_a_bounded_terminal_error() {
        let repo = Arc::new(RecordingRepo::new());
        let bus = Arc::new(TestUserEventBus::new(64));
        let (tx, _) = broadcast::channel(64);
        let relay = StreamRelay::new(
            test_conversation_id(),
            TEST_ASSISTANT_MESSAGE_ID.into(),
            TEST_USER_ID.into(),
            repo.clone(),
            bus,
            None,
        );
        let rx = tx.subscribe();
        let (send_tx, send_rx) = tokio::sync::oneshot::channel::<Result<(), AgentSendError>>();
        drop(send_tx);

        let outcome = tokio::time::timeout(
            std::time::Duration::from_millis(250),
            relay.consume_with_send_error(rx, send_rx),
        )
        .await
        .expect("closed send task signal must not leave the relay waiting");
        assert!(outcome.terminal.is_error());
        let inserts = repo.take_inserts();
        let tips = inserts
            .iter()
            .find(|row| row.r#type == "tips")
            .expect("abnormal send task exit must be persisted as an error");
        assert!(tips.content.contains("exited before reporting acceptance"));
    }

    #[tokio::test]
    async fn run_thinking_tool_thinking_splits_thinking_segments() {
        use nomifun_ai_agent::protocol::events::tool_call::{ToolCallEventData, ToolCallStatus};

        let repo = Arc::new(RecordingRepo::new());
        let bus = Arc::new(TestUserEventBus::new(64));
        let (tx, _) = broadcast::channel(64);

        let relay = StreamRelay::new(
            test_conversation_id(),
            TEST_ASSISTANT_MESSAGE_ID.into(),
            TEST_USER_ID.into(),
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
            artifacts: Vec::new(),
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
        assert_eq!(thinking_msgs[0].msg_id.as_deref(), Some(TEST_ASSISTANT_MESSAGE_ID));
        assert_ne!(thinking_msgs[0].msg_id, thinking_msgs[1].msg_id);

        let mut done_msg_ids = Vec::new();
        while let Ok(evt) = ws_rx.try_recv() {
            if evt.name == "message.stream" && evt.data["type"] == "thinking" && evt.data["data"]["status"] == "done" {
                done_msg_ids.push(evt.data["msg_id"].as_str().unwrap_or_default().to_owned());
            }
        }
        assert_eq!(done_msg_ids.len(), 2);
        assert_eq!(done_msg_ids[0], TEST_ASSISTANT_MESSAGE_ID);
        assert_ne!(done_msg_ids[0], done_msg_ids[1]);
    }

    #[tokio::test]
    async fn run_thinking_then_text_uses_distinct_segment_ids() {
        let repo = Arc::new(RecordingRepo::new());
        let bus = Arc::new(TestUserEventBus::new(64));
        let (tx, _) = broadcast::channel(64);

        let relay = StreamRelay::new(
            test_conversation_id(),
            TEST_ASSISTANT_MESSAGE_ID.into(),
            TEST_USER_ID.into(),
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

        let outcome = relay.consume(rx).await;

        let inserts = repo.take_inserts();
        let thinking_msgs: Vec<_> = inserts.iter().filter(|msg| msg.r#type == "thinking").collect();
        let text_msgs: Vec<_> = inserts.iter().filter(|msg| msg.r#type == "text").collect();

        assert_eq!(thinking_msgs.len(), 1);
        assert_eq!(text_msgs.len(), 1);
        assert_eq!(thinking_msgs[0].id, TEST_ASSISTANT_MESSAGE_ID);
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

        assert_eq!(thinking_done_ids, vec![TEST_ASSISTANT_MESSAGE_ID.to_string()]);
        assert_eq!(text_msg_ids.len(), 1);
        assert_ne!(text_msg_ids[0], TEST_ASSISTANT_MESSAGE_ID);
        assert_eq!(
            outcome.final_text_msg_id.as_deref(),
            Some(text_msg_ids[0].as_str()),
            "turn-final post-processing should target the final assistant text segment, not the thinking segment"
        );
    }

    #[tokio::test]
    async fn run_channel_closed_finalizes() {
        let repo = Arc::new(RecordingRepo::new());
        let bus = Arc::new(TestUserEventBus::new(64));
        let (tx, _) = broadcast::channel(64);

        let relay = StreamRelay::new(
            test_conversation_id(),
            TEST_ASSISTANT_MESSAGE_ID.into(),
            TEST_USER_ID.into(),
            repo.clone(),
            bus.clone(),
            None,
        );

        let mut ws_rx = bus.subscribe();
        let rx = tx.subscribe();

        // Send text then drop sender (channel closes without Finish)
        tx.send(AgentStreamEvent::Text(TextEventData {
            content: "partial".into(),
        }))
        .unwrap();
        drop(tx);

        let outcome = relay.consume(rx).await;
        assert!(outcome.system_responses.is_empty());

        // Preserve both pieces of terminal evidence: the partial assistant
        // text and a first-class canonical error row for the broken channel.
        let inserts = repo.take_inserts();
        assert_eq!(inserts.len(), 2);
        let text = inserts.iter().find(|row| row.r#type == "text").expect("partial text row");
        let error = inserts.iter().find(|row| row.r#type == "tips").expect("channel error row");
        assert_eq!(text.status.as_deref(), Some("error"));
        assert_eq!(error.status.as_deref(), Some("error"));
        let text_content: serde_json::Value = serde_json::from_str(&text.content).unwrap();
        assert_eq!(text_content["content"], "partial");
        assert_eq!(error.msg_id.as_deref(), Some(error.id.as_str()));
        let mut ws_events = Vec::new();
        while let Ok(event) = ws_rx.try_recv() {
            ws_events.push(event);
        }
        let live_error = ws_events
            .iter()
            .find(|event| event.name == "message.stream" && event.data["type"] == "error")
            .expect("unexpected channel closure must be visible as a terminal error");
        assert_eq!(live_error.data["msg_id"], error.id);
    }

    #[tokio::test]
    async fn run_broadcasts_turn_completed() {
        let repo = Arc::new(RecordingRepo::new());
        let bus = Arc::new(TestUserEventBus::new(64));
        let (tx, _) = broadcast::channel(64);

        let conversation_id = test_conversation_id();
        let relay = StreamRelay::new(
            conversation_id.clone(),
            TEST_ASSISTANT_MESSAGE_ID.into(),
            TEST_USER_ID.into(),
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
        assert_eq!(data["conversation_id"], conversation_id);
        assert_eq!(data["turn_id"], TEST_ASSISTANT_MESSAGE_ID);
        assert_eq!(data["status"], "finished");
        assert_eq!(data["can_send_message"], true);
    }

    #[tokio::test]
    async fn cancellation_token_injects_terminal_finish_without_backend_ack() {
        let repo = Arc::new(RecordingRepo::new());
        let bus = Arc::new(TestUserEventBus::new(64));
        let (_tx, rx) = broadcast::channel(64);
        let runtime_state = Arc::new(ConversationRuntimeStateService::default());
        let turn_handle = runtime_state
            .try_acquire_turn_with_wire_id(
                &test_conversation_id(),
                Some(TEST_ASSISTANT_MESSAGE_ID.to_owned()),
            )
            .expect("turn handle");
        let cancellation = turn_handle.turn_cancellation();
        cancellation.cancel();

        let mut ws_rx = bus.subscribe();
        let relay = StreamRelay::new(
            test_conversation_id(),
            TEST_ASSISTANT_MESSAGE_ID.into(),
            TEST_USER_ID.into(),
            repo,
            bus,
            None,
        )
        .with_cancellation(cancellation);

        let outcome = tokio::time::timeout(Duration::from_millis(250), relay.consume(rx))
            .await
            .expect("cancelled relay must not wait for the backend channel");
        assert_eq!(outcome.terminal, RelayTerminal::Finish);

        let mut ws_events = Vec::new();
        while let Ok(event) = ws_rx.try_recv() {
            ws_events.push(event);
        }
        let finish = ws_events
            .iter()
            .find(|event| event.name == "message.stream" && event.data["type"] == "finish")
            .expect("cancel must surface a terminal stream event");
        assert_eq!(finish.data["data"]["stop_reason"], "cancelled");
        let completed = ws_events
            .iter()
            .find(|event| event.name == "turn.completed")
            .expect("cancelled relay must complete the turn");
        assert_eq!(completed.data["turn_id"], TEST_ASSISTANT_MESSAGE_ID);
    }

    #[tokio::test]
    async fn cancellation_marks_streamed_partial_text_as_error() {
        let repo = Arc::new(RecordingRepo::new());
        let bus = Arc::new(TestUserEventBus::new(64));
        let (tx, rx) = broadcast::channel(64);
        let runtime_state = Arc::new(ConversationRuntimeStateService::default());
        let turn_handle = runtime_state
            .try_acquire_turn_with_wire_id(
                &test_conversation_id(),
                Some(TEST_ASSISTANT_MESSAGE_ID.to_owned()),
            )
            .expect("turn handle");
        let cancellation = turn_handle.turn_cancellation();
        let mut ws_rx = bus.subscribe();
        let relay = StreamRelay::new(
            test_conversation_id(),
            TEST_ASSISTANT_MESSAGE_ID.into(),
            TEST_USER_ID.into(),
            repo.clone(),
            bus,
            None,
        )
        .with_cancellation(cancellation.clone());
        let relay_task = tokio::spawn(relay.consume(rx));
        tx.send(AgentStreamEvent::Text(TextEventData {
            content: "partial before stop".into(),
        }))
        .unwrap();
        tokio::time::timeout(Duration::from_millis(250), async {
            loop {
                let event = ws_rx.recv().await.expect("stream event");
                if event.name == "message.stream" && event.data["type"] == "content" {
                    break;
                }
            }
        })
        .await
        .expect("partial text reached relay");
        cancellation.cancel();
        relay_task.await.expect("relay task");

        let text = repo
            .take_inserts()
            .into_iter()
            .find(|row| row.r#type == "text")
            .expect("partial text persisted");
        assert_eq!(text.status.as_deref(), Some("error"));
    }

    #[tokio::test]
    async fn fallback_cancel_winner_suppresses_late_ordinary_terminal() {
        let repo = Arc::new(RecordingRepo::new());
        let bus = Arc::new(TestUserEventBus::new(64));
        let (tx, rx) = broadcast::channel(64);
        let runtime_state = Arc::new(ConversationRuntimeStateService::default());
        let turn_handle = runtime_state
            .try_acquire_turn_with_wire_id(
                &test_conversation_id(),
                Some(TEST_ASSISTANT_MESSAGE_ID.to_owned()),
            )
            .expect("turn handle");
        let cancellation = turn_handle.turn_cancellation();
        let mut ws_rx = bus.subscribe();

        let fallback = StreamRelay::new(
            test_conversation_id(),
            TEST_ASSISTANT_MESSAGE_ID.into(),
            TEST_USER_ID.into(),
            repo.clone(),
            bus.clone(),
            None,
        );
        assert!(fallback.surface_cancelled_turn(&cancellation));

        tx.send(AgentStreamEvent::Finish(FinishEventData::default()))
            .unwrap();
        let relay = StreamRelay::new(
            test_conversation_id(),
            TEST_ASSISTANT_MESSAGE_ID.into(),
            TEST_USER_ID.into(),
            repo,
            bus,
            None,
        )
        .with_cancellation(cancellation);
        let outcome = relay.consume(rx).await;
        assert_eq!(outcome.stop_reason, Some(TurnStopReason::Cancelled));

        let mut terminal_count = 0;
        while let Ok(event) = ws_rx.try_recv() {
            if event.name == "message.stream"
                && matches!(event.data["type"].as_str(), Some("finish" | "error"))
            {
                terminal_count += 1;
            }
        }
        assert_eq!(terminal_count, 1, "one wire segment has one terminal publisher");
    }

    #[tokio::test]
    async fn run_with_companion_context_stamps_markers_on_stream_and_turn() {
        let repo = Arc::new(RecordingRepo::new());
        let bus = Arc::new(TestUserEventBus::new(64));
        let (tx, _) = broadcast::channel(64);

        let relay = StreamRelay::new(
            test_conversation_id(),
            TEST_ASSISTANT_MESSAGE_ID.into(),
            TEST_USER_ID.into(),
            repo.clone(),
            bus.clone(),
            None,
        )
        .with_companion_context(
            true,
            Some(
                CompanionId::parse("companion_0190f5fe-7c00-7a00-8abc-012345678942")
                    .unwrap(),
            ),
        );

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
        assert_eq!(
            stream_evt.data["companion_id"],
            "companion_0190f5fe-7c00-7a00-8abc-012345678942"
        );
        let turn_evt = ws_events
            .iter()
            .find(|e| e.name == "turn.completed")
            .expect("turn.completed broadcast");
        assert_eq!(turn_evt.data["companion"], true);
        assert_eq!(
            turn_evt.data["companion_id"],
            "companion_0190f5fe-7c00-7a00-8abc-012345678942"
        );
    }

    #[tokio::test]
    async fn run_with_channel_platform_stamps_platform_on_stream_and_turn() {
        let repo = Arc::new(RecordingRepo::new());
        let bus = Arc::new(TestUserEventBus::new(64));
        let (tx, _) = broadcast::channel(64);

        let relay = StreamRelay::new(
            "3".into(),
            TEST_ASSISTANT_MESSAGE_ID.into(),
            TEST_USER_ID.into(),
            repo.clone(),
            bus.clone(),
            None,
        )
        .with_companion_context(
            true,
            Some(
                CompanionId::parse("companion_0190f5fe-7c00-7a00-8abc-012345678942")
                    .unwrap(),
            ),
        )
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
        let bus = Arc::new(TestUserEventBus::new(64));
        let (tx, _) = broadcast::channel(64);

        let relay = StreamRelay::new(
            test_conversation_id(),
            TEST_ASSISTANT_MESSAGE_ID.into(),
            TEST_USER_ID.into(),
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
        let bus = Arc::new(TestUserEventBus::new(64));
        let (tx, _) = broadcast::channel(64);

        let relay = StreamRelay::new(
            test_conversation_id(),
            TEST_ASSISTANT_MESSAGE_ID.into(),
            TEST_USER_ID.into(),
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
        let bus = Arc::new(TestUserEventBus::new(64));
        let (tx, _) = broadcast::channel(64);

        let relay = StreamRelay::new(
            test_conversation_id(),
            TEST_ASSISTANT_MESSAGE_ID.into(),
            TEST_USER_ID.into(),
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
        let bus = Arc::new(TestUserEventBus::new(64));
        let (tx, _) = broadcast::channel(64);

        // Blank origin must normalize to None (owner speech).
        let relay = StreamRelay::new(
            test_conversation_id(),
            TEST_ASSISTANT_MESSAGE_ID.into(),
            TEST_USER_ID.into(),
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
        let bus = Arc::new(TestUserEventBus::new(64));
        let (tx, _) = broadcast::channel(64);
        let relay = StreamRelay::new(
            test_conversation_id(),
            TEST_ASSISTANT_MESSAGE_ID.into(),
            TEST_USER_ID.into(),
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
            .find(|(id, update)| id == TEST_ASSISTANT_MESSAGE_ID && update.content.is_some())
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

    #[tokio::test]
    async fn failed_final_rewrite_emits_no_unacknowledged_override_or_outcome() {
        let repo = Arc::new(RecordingRepo::new());
        repo.fail_next_message_update();
        let bus = Arc::new(TestUserEventBus::new(64));
        let mut ws_rx = bus.subscribe();
        let (tx, _) = broadcast::channel(64);
        let relay = StreamRelay::new(
            test_conversation_id(),
            TEST_ASSISTANT_MESSAGE_ID.into(),
            TEST_USER_ID.into(),
            repo.clone(),
            bus,
            Some(Arc::new(MockCronService)),
        );
        let rx = tx.subscribe();
        tx.send(AgentStreamEvent::Text(TextEventData {
            content: "Hello [CRON_LIST]".into(),
        }))
        .unwrap();
        tx.send(AgentStreamEvent::Finish(FinishEventData::default())).unwrap();

        let outcome = relay.consume(rx).await;

        assert!(outcome.final_text.is_none());
        assert!(outcome.final_text_msg_id.is_none());
        assert_eq!(outcome.system_responses, vec!["[System: listed]".to_string()]);
        assert!(repo.take_updates().is_empty());
        let inserts = repo.take_inserts();
        let raw: Value = serde_json::from_str(&inserts[0].content).unwrap();
        assert_eq!(raw["content"], "Hello [CRON_LIST]");
        assert!(
            std::iter::from_fn(|| ws_rx.try_recv().ok()).all(|event| {
                event.name != "message.stream" || event.data["replace"] != true
            }),
            "live replacement must wait for the database rewrite acknowledgement"
        );
    }

    #[tokio::test]
    async fn failed_superseded_hide_emits_only_acknowledged_overrides() {
        let repo = Arc::new(RecordingRepo::new());
        repo.fail_message_update_attempt(2);
        let bus = Arc::new(TestUserEventBus::new(128));
        let mut ws_rx = bus.subscribe();
        let (tx, _) = broadcast::channel(128);
        let relay = StreamRelay::new(
            test_conversation_id(),
            TEST_ASSISTANT_MESSAGE_ID.into(),
            TEST_USER_ID.into(),
            repo.clone(),
            bus,
            Some(Arc::new(MockCronService)),
        );
        let rx = tx.subscribe();
        tx.send(AgentStreamEvent::Text(TextEventData {
            content: "Alpha ".into(),
        }))
        .unwrap();
        tx.send(AgentStreamEvent::Thinking(ThinkingEventData {
            content: String::new(),
            subject: None,
            duration: None,
            status: Some("thinking".into()),
        }))
        .unwrap();
        tx.send(AgentStreamEvent::Text(TextEventData {
            content: "Beta [CRON_LIST]".into(),
        }))
        .unwrap();
        tx.send(AgentStreamEvent::Finish(FinishEventData::default())).unwrap();

        let outcome = relay.consume(rx).await;

        assert!(outcome.final_text.is_none());
        assert!(outcome.final_text_msg_id.is_none());
        let inserts = repo.take_inserts();
        let text_rows: Vec<_> = inserts.iter().filter(|row| row.r#type == "text").collect();
        assert_eq!(text_rows.len(), 2);
        let updates = repo.take_updates();
        assert_eq!(updates.len(), 1, "only the acknowledged primary rewrite is recorded");
        assert_eq!(updates[0].0, text_rows[0].id);

        let replacements: Vec<_> = std::iter::from_fn(|| ws_rx.try_recv().ok())
            .filter(|event| {
                event.name == "message.stream"
                    && event.data["type"] == "content"
                    && event.data["replace"] == true
            })
            .collect();
        assert_eq!(replacements.len(), 1);
        assert_eq!(replacements[0].data["msg_id"], text_rows[0].id);
        assert!(
            replacements
                .iter()
                .all(|event| event.data["msg_id"] != text_rows[1].id),
            "a failed hide must remain visible both live and after reload"
        );
    }

    // ── Tool persistence tests ────────────────────────────────────

    #[tokio::test]
    async fn run_tool_call_persists_message() {
        use nomifun_ai_agent::protocol::events::tool_call::{ToolCallEventData, ToolCallStatus};

        let repo = Arc::new(RecordingRepo::new());
        let bus = Arc::new(TestUserEventBus::new(64));
        let (tx, _) = broadcast::channel(64);

        let relay = StreamRelay::new(
            test_conversation_id(),
            TEST_ASSISTANT_MESSAGE_ID.into(),
            TEST_USER_ID.into(),
            repo.clone(),
            bus.clone(),
            None,
        );

        let rx = tx.subscribe();

        // First event: Running with input but no output
        tx.send(AgentStreamEvent::ToolCall(ToolCallEventData {
            call_id: "tc-001".into(),
            name: "read_file".into(),
            args: json!({"path": "notes.txt"}),
            status: ToolCallStatus::Running,
            input: Some(json!({"path": "notes.txt"})),
            output: None,
            description: Some("Read file".into()),
            artifacts: Vec::new(),
        }))
        .unwrap();
        // Second event: Completed with output but no input
        tx.send(AgentStreamEvent::ToolCall(ToolCallEventData {
            call_id: "tc-001".into(),
            name: "read_file".into(),
            args: json!({"path": "notes.txt"}),
            status: ToolCallStatus::Completed,
            input: None,
            output: Some("contents".into()),
            description: None,
            artifacts: Vec::new(),
        }))
        .unwrap();
        tx.send(AgentStreamEvent::Finish(FinishEventData::default())).unwrap();

        relay.consume(rx).await;

        let inserts = repo.take_inserts();
        let tool_msg = inserts.iter().find(|m| m.r#type == "tool_call");
        assert!(tool_msg.is_some());
        let msg = tool_msg.unwrap();
        MessageId::parse(&msg.id).expect("tool row has a canonical message ID");
        assert_eq!(msg.msg_id.as_deref(), Some(TEST_ASSISTANT_MESSAGE_ID));
        assert_eq!(msg.status.as_deref(), Some("work"));

        let updates = repo.take_updates();
        let tool_update = updates.iter().find(|(id, _)| id == &msg.id);
        assert!(tool_update.is_some());
        let (_, upd) = tool_update.unwrap();
        assert_eq!(upd.status, Some(Some("finish".to_owned())));

        // Verify merge: input from first event preserved, output from second event added
        let merged: serde_json::Value = serde_json::from_str(upd.content.as_deref().unwrap()).unwrap();
        assert_eq!(merged["name"], "read_file");
        assert_eq!(merged["status"], "completed");
        assert!(
            merged.get("input").is_some() && !merged["input"].is_null(),
            "input must be preserved after merge"
        );
        assert_eq!(merged["input"]["path"], "notes.txt");
        assert_eq!(merged["output"], "contents");
        assert_eq!(merged["description"], "Read file");
    }

    #[tokio::test]
    async fn completed_image_tool_without_receipt_fails_the_enclosing_turn() {
        use nomifun_ai_agent::protocol::events::tool_call::{ToolCallEventData, ToolCallStatus};

        let repo = Arc::new(RecordingRepo::new());
        let bus = Arc::new(TestUserEventBus::new(64));
        let mut ws_rx = bus.subscribe();
        let (tx, _) = broadcast::channel(64);
        let relay = StreamRelay::new(
            test_conversation_id(),
            TEST_ASSISTANT_MESSAGE_ID.into(),
            TEST_USER_ID.into(),
            repo.clone(),
            bus,
            None,
        );
        let rx = tx.subscribe();

        tx.send(AgentStreamEvent::ToolCall(ToolCallEventData {
            call_id: "empty-image-result".into(),
            name: "image_gen".into(),
            args: json!({"prompt": "cat"}),
            status: ToolCallStatus::Running,
            input: Some(json!({"prompt": "cat"})),
            output: None,
            description: Some("Generate image".into()),
            artifacts: Vec::new(),
        }))
        .unwrap();
        tx.send(AgentStreamEvent::ToolCall(ToolCallEventData {
            call_id: "empty-image-result".into(),
            name: "tool_result".into(),
            args: json!({}),
            status: ToolCallStatus::Completed,
            input: None,
            output: Some("success".into()),
            description: None,
            artifacts: Vec::new(),
        }))
        .unwrap();
        tx.send(AgentStreamEvent::Finish(FinishEventData::default())).unwrap();

        relay.consume(rx).await;

        let inserts = repo.take_inserts();
        let tool_row = inserts
            .iter()
            .find(|row| row.r#type == "tool_call")
            .expect("failed image result is persisted");
        let updates = repo.take_updates();
        let final_tool_update = updates
            .iter()
            .rev()
            .find(|(id, _)| id == &tool_row.id)
            .expect("tool terminal update");
        assert_eq!(final_tool_update.1.status.as_ref().and_then(|s| s.as_deref()), Some("error"));
        let content: serde_json::Value =
            serde_json::from_str(final_tool_update.1.content.as_deref().expect("tool content")).unwrap();
        assert_eq!(content["artifacts"], json!([]));
        assert_eq!(content["status"], "error");

        let mut saw_successful_finish = false;
        while let Ok(event) = ws_rx.try_recv() {
            saw_successful_finish |= event.name == "message.stream" && event.data["type"] == "finish";
        }
        assert!(!saw_successful_finish, "a receipt-less image result must not finish successfully");
    }

    #[tokio::test]
    async fn run_acp_tool_call_inserts_then_updates() {
        use nomifun_ai_agent::protocol::events::tool_call::{
            AcpToolCallEventData, AcpToolCallSessionUpdateKind, AcpToolCallStatus, AcpToolCallUpdateData,
        };

        let repo = Arc::new(RecordingRepo::new());
        let bus = Arc::new(TestUserEventBus::new(64));
        let (tx, _) = broadcast::channel(64);

        let relay = StreamRelay::new(
            test_conversation_id(),
            TEST_ASSISTANT_MESSAGE_ID.into(),
            TEST_USER_ID.into(),
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
        MessageId::parse(&msg.id).expect("ACP tool row has a canonical message ID");
        assert_eq!(msg.msg_id.as_deref(), Some(TEST_ASSISTANT_MESSAGE_ID));
        assert_eq!(msg.status.as_deref(), Some("work"));

        let updates = repo.take_updates();
        let acp_update = updates
            .iter()
            .find(|(id, _)| id == &msg.id);
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
    async fn external_acp_export_title_cannot_complete_without_a_verified_artifact() {
        use nomifun_ai_agent::protocol::events::tool_call::{
            AcpToolCallEventData, AcpToolCallSessionUpdateKind, AcpToolCallStatus,
            AcpToolCallUpdateData,
        };

        let repo = Arc::new(RecordingRepo::new());
        let bus = Arc::new(TestUserEventBus::new(64));
        let mut ws_rx = bus.subscribe();
        let (tx, _) = broadcast::channel(64);
        let relay = StreamRelay::new(
            test_conversation_id(),
            TEST_TURN_A.into(),
            TEST_USER_ID.into(),
            repo.clone(),
            bus,
            None,
        );
        let rx = tx.subscribe();

        tx.send(AgentStreamEvent::AcpToolCall(AcpToolCallEventData {
            session_id: "external-session".into(),
            update: AcpToolCallUpdateData {
                session_update: AcpToolCallSessionUpdateKind::ToolCall,
                tool_call_id: "external-export".into(),
                status: Some(AcpToolCallStatus::InProgress),
                title: Some("export_pdf".into()),
                kind: None,
                raw_input: Some(json!({"output_path": "report.pdf"})),
                raw_output: None,
                content: None,
                locations: None,
            },
            meta: None,
        }))
        .unwrap();
        // External runtimes commonly omit repeated title/input metadata on the
        // terminal delta. The active identity must remain authoritative.
        tx.send(AgentStreamEvent::AcpToolCall(AcpToolCallEventData {
            session_id: "external-session".into(),
            update: AcpToolCallUpdateData {
                session_update: AcpToolCallSessionUpdateKind::ToolCallUpdate,
                tool_call_id: "external-export".into(),
                status: Some(AcpToolCallStatus::Completed),
                title: None,
                kind: None,
                raw_input: None,
                raw_output: Some(json!({"ok": true})),
                content: None,
                locations: None,
            },
            meta: None,
        }))
        .unwrap();
        tx.send(AgentStreamEvent::Finish(FinishEventData::default()))
            .unwrap();

        let outcome = relay.consume(rx).await;
        assert!(outcome.terminal.is_error());

        let row = repo
            .take_inserts()
            .into_iter()
            .find(|row| row.r#type == "acp_tool_call")
            .expect("external ACP tool row");
        let updates = repo.take_updates();
        let (_, terminal) = updates
            .iter()
            .rev()
            .find(|(id, _)| id == &row.id)
            .expect("external ACP terminal correction");
        assert_eq!(
            terminal.status.as_ref().and_then(|status| status.as_deref()),
            Some("error")
        );
        let content: Value =
            serde_json::from_str(terminal.content.as_deref().expect("ACP correction content"))
                .unwrap();
        assert_eq!(content["update"]["status"], "failed");
        assert!(content["update"]["raw_output"]
            .as_str()
            .is_some_and(|message| message.contains("required verified artifacts")));

        let mut saw_finish = false;
        while let Ok(event) = ws_rx.try_recv() {
            saw_finish |= event.name == "message.stream" && event.data["type"] == "finish";
        }
        assert!(!saw_finish);
    }

    #[tokio::test]
    async fn external_acp_duplicate_receipt_cannot_satisfy_requested_image_count() {
        use nomifun_ai_agent::protocol::events::{
            AcpToolCallContentItem,
            tool_call::{
                AcpToolCallEventData, AcpToolCallSessionUpdateKind, AcpToolCallStatus,
                AcpToolCallUpdateData,
            },
        };

        let repo = Arc::new(RecordingRepo::new());
        let bus = Arc::new(TestUserEventBus::new(64));
        let mut ws_rx = bus.subscribe();
        let (tx, _) = broadcast::channel(64);
        let relay = StreamRelay::new(
            test_conversation_id(),
            TEST_TURN_A.into(),
            TEST_USER_ID.into(),
            repo.clone(),
            bus,
            None,
        );
        let rx = tx.subscribe();

        let first = test_artifact("external-duplicate");
        let mut duplicate = first.clone();
        duplicate.id = "external-duplicate-alias".into();
        tx.send(AgentStreamEvent::AcpToolCall(AcpToolCallEventData {
            session_id: "external-session".into(),
            update: AcpToolCallUpdateData {
                session_update: AcpToolCallSessionUpdateKind::ToolCallUpdate,
                tool_call_id: "external-image-count".into(),
                status: Some(AcpToolCallStatus::Completed),
                title: Some("image_gen".into()),
                kind: None,
                raw_input: Some(json!({"prompt": "two cats", "count": 2})),
                raw_output: Some(json!({"ok": true})),
                content: Some(vec![
                    AcpToolCallContentItem::Artifact {
                        artifact: first,
                        source_uri: None,
                    },
                    AcpToolCallContentItem::Artifact {
                        artifact: duplicate,
                        source_uri: None,
                    },
                ]),
                locations: None,
            },
            meta: None,
        }))
        .unwrap();
        tx.send(AgentStreamEvent::Finish(FinishEventData::default()))
            .unwrap();

        let outcome = relay.consume(rx).await;
        assert!(outcome.terminal.is_error());

        let row = repo
            .take_inserts()
            .into_iter()
            .find(|row| row.r#type == "acp_tool_call")
            .expect("failed external ACP count row");
        assert_eq!(row.status.as_deref(), Some("error"));
        let content: Value = serde_json::from_str(&row.content).unwrap();
        assert_eq!(content["update"]["status"], "failed");
        assert_eq!(content["update"]["content"], json!([]));
        assert!(content["update"]["raw_output"]
            .as_str()
            .is_some_and(|message| message.contains("same canonical artifact path")));

        let mut saw_completed = false;
        let mut saw_finish = false;
        while let Ok(event) = ws_rx.try_recv() {
            if event.name != "message.stream" {
                continue;
            }
            saw_completed |= event.data["type"] == "acp_tool_call"
                && event.data["data"]["update"]["status"] == "completed";
            saw_finish |= event.data["type"] == "finish";
        }
        assert!(!saw_completed);
        assert!(!saw_finish);
    }

    #[test]
    fn external_acp_receipt_ids_are_validated_without_tool_identity() {
        use nomifun_ai_agent::protocol::events::{
            AcpToolCallContentItem,
            tool_call::{
                AcpToolCallEventData, AcpToolCallSessionUpdateKind, AcpToolCallStatus,
                AcpToolCallUpdateData,
            },
        };

        let first = test_artifact("identity-free-first");
        let mut duplicate_id = test_artifact("identity-free-second");
        duplicate_id.id = first.id.clone();
        let result = validate_completed_acp_artifact_contract(&AcpToolCallEventData {
            session_id: "external-session".into(),
            update: AcpToolCallUpdateData {
                session_update: AcpToolCallSessionUpdateKind::ToolCallUpdate,
                tool_call_id: "identity-free-receipts".into(),
                status: Some(AcpToolCallStatus::Completed),
                title: None,
                kind: None,
                raw_input: None,
                raw_output: None,
                content: Some(vec![
                    AcpToolCallContentItem::Artifact {
                        artifact: first,
                        source_uri: None,
                    },
                    AcpToolCallContentItem::Artifact {
                        artifact: duplicate_id,
                        source_uri: None,
                    },
                ]),
                locations: None,
            },
            meta: None,
        });

        assert!(result.unwrap_err().contains("same artifact id more than once"));
    }

    #[tokio::test]
    async fn run_acp_terminal_update_without_start_is_upserted() {
        use nomifun_ai_agent::protocol::events::tool_call::{
            AcpToolCallEventData, AcpToolCallSessionUpdateKind, AcpToolCallStatus, AcpToolCallUpdateData,
        };

        let repo = Arc::new(RecordingRepo::new());
        let bus = Arc::new(TestUserEventBus::new(64));
        let (tx, _) = broadcast::channel(64);
        let relay = StreamRelay::new(
            test_conversation_id(),
            TEST_TURN_A.into(),
            TEST_USER_ID.into(),
            repo.clone(),
            bus,
            None,
        );
        let rx = tx.subscribe();
        tx.send(AgentStreamEvent::AcpToolCall(AcpToolCallEventData {
            session_id: "sess-1".into(),
            update: AcpToolCallUpdateData {
                session_update: AcpToolCallSessionUpdateKind::ToolCallUpdate,
                tool_call_id: "atc-001".into(),
                status: Some(AcpToolCallStatus::Completed),
                title: Some("Bash".into()),
                kind: None,
                raw_input: None,
                raw_output: Some(json!("Exit code: 0")),
                content: None,
                locations: None,
            },
            meta: None,
        }))
        .unwrap();
        tx.send(AgentStreamEvent::Finish(FinishEventData::default())).unwrap();

        relay.consume(rx).await;

        let inserts = repo.take_inserts();
        let row = inserts
            .iter()
            .find(|row| row.r#type == "acp_tool_call")
            .expect("terminal ACP update must survive a missing start event");
        MessageId::parse(&row.id).expect("ACP tool row has a canonical message ID");
        assert_eq!(row.status.as_deref(), Some("finish"));
        let content: serde_json::Value = serde_json::from_str(&row.content).unwrap();
        assert_eq!(content["turn_id"], TEST_TURN_A);
    }

    #[tokio::test]
    async fn run_marks_active_acp_tool_failed_when_turn_is_truncated() {
        use nomifun_ai_agent::protocol::events::{TurnStopReason, tool_call::{
            AcpToolCallEventData, AcpToolCallSessionUpdateKind, AcpToolCallStatus, AcpToolCallUpdateData,
        }};

        let repo = Arc::new(RecordingRepo::new());
        let bus = Arc::new(TestUserEventBus::new(64));
        let (tx, _) = broadcast::channel(64);
        let relay = StreamRelay::new(
            test_conversation_id(),
            TEST_TURN_A.into(),
            TEST_USER_ID.into(),
            repo.clone(),
            bus,
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
                raw_input: Some(json!({"command": "sleep 10"})),
                raw_output: None,
                content: None,
                locations: None,
            },
            meta: None,
        }))
        .unwrap();
        tx.send(AgentStreamEvent::Finish(FinishEventData {
            session_id: None,
            stop_reason: Some(TurnStopReason::MaxTokens),
        }))
        .unwrap();

        relay.consume(rx).await;

        let tool_id = repo
            .take_inserts()
            .into_iter()
            .find(|row| row.r#type == "acp_tool_call")
            .expect("ACP tool must be persisted")
            .id;
        MessageId::parse(&tool_id).expect("ACP tool row has a canonical message ID");
        let updates = repo.take_updates();
        let (_, update) = updates
            .iter()
            .find(|(id, _)| id == &tool_id)
            .expect("active ACP tool must be terminalized");
        assert_eq!(update.status.as_ref().map(|s| s.as_deref()), Some(Some("error")));
        let content: serde_json::Value = serde_json::from_str(update.content.as_deref().unwrap()).unwrap();
        assert_eq!(content["update"]["status"], "failed");
        assert_eq!(
            content["update"]["raw_output"],
            "The turn ended before this tool completed: max_tokens"
        );
    }

    #[tokio::test]
    async fn run_tool_group_persists_message() {
        use nomifun_ai_agent::protocol::events::tool_call::{ToolCallStatus, ToolGroupEntry};

        let repo = Arc::new(RecordingRepo::new());
        let bus = Arc::new(TestUserEventBus::new(64));
        let (tx, _) = broadcast::channel(64);

        let relay = StreamRelay::new(
            test_conversation_id(),
            TEST_ASSISTANT_MESSAGE_ID.into(),
            TEST_USER_ID.into(),
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
        MessageId::parse(&msg.id).expect("tool-group row has a canonical message ID");
        assert_eq!(msg.msg_id.as_deref(), Some(TEST_ASSISTANT_MESSAGE_ID));
        assert_eq!(msg.status.as_deref(), Some("finish"));

        let content: serde_json::Value = serde_json::from_str(&msg.content).unwrap();
        assert!(content.is_array());
        assert_eq!(content.as_array().unwrap().len(), 2);
    }

    #[tokio::test]
    async fn completed_artifact_tool_group_without_receipts_fails_the_enclosing_turn() {
        use nomifun_ai_agent::protocol::events::tool_call::{
            ToolCallStatus, ToolGroupEntry,
        };

        let repo = Arc::new(RecordingRepo::new());
        let bus = Arc::new(TestUserEventBus::new(64));
        let mut ws_rx = bus.subscribe();
        let (tx, _) = broadcast::channel(64);
        let relay = StreamRelay::new(
            test_conversation_id(),
            TEST_TURN_A.into(),
            TEST_USER_ID.into(),
            repo.clone(),
            bus,
            None,
        );
        let rx = tx.subscribe();

        tx.send(AgentStreamEvent::ToolGroup(vec![
            ToolGroupEntry {
                call_id: "group-image".into(),
                name: "image_gen".into(),
                status: ToolCallStatus::Completed,
                description: Some("generated".into()),
            },
            ToolGroupEntry {
                call_id: "group-export".into(),
                name: "export_pdf".into(),
                status: ToolCallStatus::Completed,
                description: Some("exported".into()),
            },
        ]))
        .unwrap();
        tx.send(AgentStreamEvent::Finish(FinishEventData::default()))
            .unwrap();

        let outcome = relay.consume(rx).await;
        assert!(outcome.terminal.is_error());

        let row = repo
            .take_inserts()
            .into_iter()
            .find(|row| row.r#type == "tool_group")
            .expect("failed artifact tool group row");
        assert_eq!(row.status.as_deref(), Some("error"));
        let content: Value = serde_json::from_str(&row.content).unwrap();
        assert_eq!(content[0]["status"], "error");
        assert_eq!(content[1]["status"], "error");
        assert!(content[0]["description"]
            .as_str()
            .is_some_and(|message| message.contains("required verified artifacts")));
        assert!(content[1]["description"]
            .as_str()
            .is_some_and(|message| message.contains("required verified artifacts")));

        let mut saw_finish = false;
        while let Ok(event) = ws_rx.try_recv() {
            saw_finish |= event.name == "message.stream" && event.data["type"] == "finish";
        }
        assert!(!saw_finish);
    }

    #[test]
    fn tool_group_count_contract_rejects_duplicate_paired_receipts() {
        use nomifun_ai_agent::protocol::events::tool_call::{
            ToolCallEventData, ToolCallStatus, ToolGroupEntry,
        };

        let first = test_artifact("group-count-duplicate");
        let mut duplicate = first.clone();
        duplicate.id = "group-count-alias".into();
        let paired = ToolCallEventData {
            call_id: "group-count".into(),
            name: "image_gen".into(),
            args: json!({"prompt": "two cats", "count": 2}),
            status: ToolCallStatus::Completed,
            input: None,
            output: Some("generated".into()),
            description: None,
            artifacts: vec![first, duplicate],
        };
        let completed = HashMap::from([(paired.call_id.clone(), paired)]);
        let entries = vec![ToolGroupEntry {
            call_id: "group-count".into(),
            name: "image_gen".into(),
            status: ToolCallStatus::Completed,
            description: Some("generated two images".into()),
        }];

        let errors = tool_group_artifact_contract_errors(&entries, &completed);
        assert_eq!(errors.len(), 1);
        assert!(errors[0].1.contains("same canonical artifact path"));
    }

    #[tokio::test]
    async fn artifact_tool_group_is_suppressed_when_receipt_commit_fails() {
        use nomifun_ai_agent::protocol::events::tool_call::{
            ToolCallEventData, ToolCallStatus, ToolGroupEntry,
        };

        let repo = Arc::new(RecordingRepo::new());
        repo.fail_artifact_commits();
        let bus = Arc::new(TestUserEventBus::new(64));
        let mut ws_rx = bus.subscribe();
        let (tx, _) = broadcast::channel(64);
        let workspace = std::env::temp_dir().join(format!(
            "nomifun-tool-group-2pc-test-{}",
            MessageId::new().into_string()
        ));
        std::fs::create_dir_all(&workspace).expect("create test workspace");
        let artifact = persisted_png_artifact(&workspace);
        let relay = StreamRelay::new(
            test_conversation_id(),
            TEST_TURN_A.into(),
            TEST_USER_ID.into(),
            repo.clone(),
            bus,
            None,
        )
        .with_artifact_workspace(workspace.clone());
        let rx = tx.subscribe();

        tx.send(AgentStreamEvent::ToolCall(ToolCallEventData {
            call_id: "group-2pc-image".into(),
            name: "image_gen".into(),
            args: json!({"prompt": "cat"}),
            status: ToolCallStatus::Completed,
            input: None,
            output: Some("generated".into()),
            description: None,
            artifacts: vec![artifact],
        }))
        .unwrap();
        tx.send(AgentStreamEvent::ToolGroup(vec![ToolGroupEntry {
            call_id: "group-2pc-image".into(),
            name: "image_gen".into(),
            status: ToolCallStatus::Completed,
            description: Some("generated".into()),
        }]))
        .unwrap();
        tx.send(AgentStreamEvent::Finish(FinishEventData::default()))
            .unwrap();

        let outcome = relay.consume(rx).await;
        assert_eq!(
            outcome.terminal.code(),
            Some(AgentErrorCode::NomifunStateInconsistent)
        );

        assert!(
            repo.take_inserts()
                .iter()
                .all(|row| row.r#type != "tool_group"),
            "receipt-less artifact summaries must never enter durable history"
        );
        assert!(
            repo.take_updates().iter().all(|(_, update)| {
                update
                    .content
                    .as_deref()
                    .and_then(|content| serde_json::from_str::<Value>(content).ok())
                    .is_none_or(|content| !content.is_array())
            }),
            "a suppressed artifact summary must not acquire an update row"
        );

        let mut saw_finish = false;
        while let Ok(event) = ws_rx.try_recv() {
            if event.name != "message.stream" {
                continue;
            }
            assert_ne!(event.data["type"], "tool_group");
            saw_finish |= event.data["type"] == "finish";
        }
        assert!(!saw_finish);
        std::fs::remove_dir_all(workspace).expect("remove test workspace");
    }

    #[tokio::test]
    async fn artifact_tool_group_is_suppressed_after_receipt_commit_succeeds() {
        use nomifun_ai_agent::protocol::events::tool_call::{
            ToolCallEventData, ToolCallStatus, ToolGroupEntry,
        };

        let repo = Arc::new(RecordingRepo::new());
        let bus = Arc::new(TestUserEventBus::new(64));
        let mut ws_rx = bus.subscribe();
        let (tx, _) = broadcast::channel(64);
        let workspace = std::env::temp_dir().join(format!(
            "nomifun-tool-group-2pc-success-test-{}",
            MessageId::new().into_string()
        ));
        std::fs::create_dir_all(&workspace).expect("create test workspace");
        let artifact = persisted_png_artifact(&workspace);
        let relay = StreamRelay::new(
            test_conversation_id(),
            TEST_TURN_A.into(),
            TEST_USER_ID.into(),
            repo.clone(),
            bus,
            None,
        )
        .with_artifact_workspace(workspace.clone());
        let rx = tx.subscribe();

        tx.send(AgentStreamEvent::ToolCall(ToolCallEventData {
            call_id: "group-2pc-success".into(),
            name: "image_gen".into(),
            args: json!({"prompt": "cat"}),
            status: ToolCallStatus::Completed,
            input: None,
            output: Some("generated".into()),
            description: None,
            artifacts: vec![artifact],
        }))
        .unwrap();
        tx.send(AgentStreamEvent::ToolGroup(vec![ToolGroupEntry {
            call_id: "group-2pc-success".into(),
            name: "image_gen".into(),
            status: ToolCallStatus::Completed,
            description: Some("generated".into()),
        }]))
        .unwrap();
        tx.send(AgentStreamEvent::Finish(FinishEventData::default()))
            .unwrap();

        let outcome = relay.consume(rx).await;
        assert!(matches!(outcome.terminal, RelayTerminal::Finish));

        assert!(
            repo.take_inserts()
                .iter()
                .all(|row| row.r#type != "tool_group"),
            "receipt-less artifact summaries must never enter durable history"
        );
        assert!(
            repo.take_updates().iter().all(|(_, update)| {
                update
                    .content
                    .as_deref()
                    .and_then(|content| serde_json::from_str::<Value>(content).ok())
                    .is_none_or(|content| !content.is_array())
            }),
            "a suppressed artifact summary must not acquire an update row"
        );

        let mut stream_types = Vec::new();
        while let Ok(event) = ws_rx.try_recv() {
            if event.name != "message.stream" {
                continue;
            }
            stream_types.push(event.data["type"].clone());
            assert_ne!(event.data["type"], "tool_group");
        }
        assert_eq!(stream_types.last(), Some(&json!("finish")));
        std::fs::remove_dir_all(workspace).expect("remove test workspace");
    }

    #[tokio::test]
    async fn run_tool_group_with_failed_entry_persists_error() {
        use nomifun_ai_agent::protocol::events::tool_call::{ToolCallStatus, ToolGroupEntry};

        let repo = Arc::new(RecordingRepo::new());
        let bus = Arc::new(TestUserEventBus::new(64));
        let (tx, _) = broadcast::channel(64);
        let relay = StreamRelay::new(
            test_conversation_id(),
            TEST_TURN_A.into(),
            TEST_USER_ID.into(),
            repo.clone(),
            bus,
            None,
        );
        let rx = tx.subscribe();
        tx.send(AgentStreamEvent::ToolGroup(vec![
            ToolGroupEntry {
                call_id: "tg-001".into(),
                name: "read_file".into(),
                status: ToolCallStatus::Completed,
                description: None,
            },
            ToolGroupEntry {
                call_id: "tg-002".into(),
                name: "write_file".into(),
                status: ToolCallStatus::Error,
                description: Some("permission denied".into()),
            },
        ]))
        .unwrap();
        tx.send(AgentStreamEvent::Finish(FinishEventData::default())).unwrap();

        relay.consume(rx).await;

        let inserts = repo.take_inserts();
        let row = inserts.iter().find(|row| row.r#type == "tool_group").unwrap();
        MessageId::parse(&row.id).expect("tool-group row has a canonical message ID");
        assert_eq!(row.msg_id.as_deref(), Some(TEST_TURN_A));
        assert_eq!(row.status.as_deref(), Some("error"));
    }

    #[tokio::test]
    async fn run_marks_active_tool_group_failed_when_channel_closes() {
        use nomifun_ai_agent::protocol::events::tool_call::{ToolCallStatus, ToolGroupEntry};

        let repo = Arc::new(RecordingRepo::new());
        let bus = Arc::new(TestUserEventBus::new(64));
        let (tx, _) = broadcast::channel(64);
        let relay = StreamRelay::new(
            test_conversation_id(),
            TEST_TURN_A.into(),
            TEST_USER_ID.into(),
            repo.clone(),
            bus,
            None,
        );
        let rx = tx.subscribe();
        tx.send(AgentStreamEvent::ToolGroup(vec![ToolGroupEntry {
            call_id: "tg-001".into(),
            name: "Bash".into(),
            status: ToolCallStatus::Running,
            description: Some("build".into()),
        }]))
        .unwrap();
        drop(tx);

        relay.consume(rx).await;

        let group_id = repo
            .take_inserts()
            .into_iter()
            .find(|row| row.r#type == "tool_group")
            .expect("tool group must be persisted")
            .id;
        MessageId::parse(&group_id).expect("tool-group row has a canonical message ID");
        let updates = repo.take_updates();
        let (_, update) = updates
            .iter()
            .find(|(id, _)| id == &group_id)
            .expect("active tool group must be terminalized on channel close");
        assert_eq!(update.status.as_ref().map(|s| s.as_deref()), Some(Some("error")));
        let content: serde_json::Value = serde_json::from_str(update.content.as_deref().unwrap()).unwrap();
        assert_eq!(content[0]["status"], "error");
        assert!(content[0]["description"].as_str().unwrap().contains("channel_closed"));
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
        correlations: Mutex<HashMap<(String, String, String, String), String>>,
        fail_next_message_insert: AtomicBool,
        commit_next_message_insert_then_error: AtomicBool,
        fail_message_inserts: AtomicBool,
        reject_duplicate_message_inserts: AtomicBool,
        block_message_inserts: AtomicBool,
        fail_next_message_update: AtomicBool,
        fail_message_updates: AtomicBool,
        message_update_attempts: AtomicUsize,
        fail_message_update_attempt: AtomicUsize,
        block_message_updates: AtomicBool,
        fail_message_correlations: AtomicBool,
        fail_artifact_commits: AtomicBool,
        block_artifact_commits: AtomicBool,
    }

    impl RecordingRepo {
        fn new() -> Self {
            Self {
                inserts: Mutex::new(vec![]),
                updates: Mutex::new(vec![]),
                correlations: Mutex::new(HashMap::new()),
                fail_next_message_insert: AtomicBool::new(false),
                commit_next_message_insert_then_error: AtomicBool::new(false),
                fail_message_inserts: AtomicBool::new(false),
                reject_duplicate_message_inserts: AtomicBool::new(false),
                block_message_inserts: AtomicBool::new(false),
                fail_next_message_update: AtomicBool::new(false),
                fail_message_updates: AtomicBool::new(false),
                message_update_attempts: AtomicUsize::new(0),
                fail_message_update_attempt: AtomicUsize::new(0),
                block_message_updates: AtomicBool::new(false),
                fail_message_correlations: AtomicBool::new(false),
                fail_artifact_commits: AtomicBool::new(false),
                block_artifact_commits: AtomicBool::new(false),
            }
        }

        fn fail_next_message_insert(&self) {
            self.fail_next_message_insert.store(true, AtomicOrdering::SeqCst);
        }

        fn commit_next_message_insert_then_error(&self) {
            self.commit_next_message_insert_then_error
                .store(true, AtomicOrdering::SeqCst);
        }

        fn fail_message_inserts(&self) {
            self.fail_message_inserts.store(true, AtomicOrdering::SeqCst);
        }

        fn reject_duplicate_message_inserts(&self) {
            self.reject_duplicate_message_inserts
                .store(true, AtomicOrdering::SeqCst);
        }

        fn set_block_message_inserts(&self, block: bool) {
            self.block_message_inserts.store(block, AtomicOrdering::SeqCst);
        }

        fn fail_next_message_update(&self) {
            self.fail_next_message_update.store(true, AtomicOrdering::SeqCst);
        }

        fn fail_message_updates(&self) {
            self.fail_message_updates.store(true, AtomicOrdering::SeqCst);
        }

        fn fail_message_update_attempt(&self, attempt: usize) {
            self.fail_message_update_attempt
                .store(attempt, AtomicOrdering::SeqCst);
        }

        fn block_message_updates(&self) {
            self.block_message_updates.store(true, AtomicOrdering::SeqCst);
        }

        fn fail_message_correlations(&self) {
            self.fail_message_correlations
                .store(true, AtomicOrdering::SeqCst);
        }

        fn fail_artifact_commits(&self) {
            self.fail_artifact_commits
                .store(true, AtomicOrdering::SeqCst);
        }

        fn block_artifact_commits(&self) {
            self.block_artifact_commits
                .store(true, AtomicOrdering::SeqCst);
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
        async fn get(&self, _id: &str) -> Result<Option<nomifun_db::models::ConversationRow>, DbError> {
            Ok(None)
        }
        async fn create(&self, row: &nomifun_db::models::ConversationRow) -> Result<String, DbError> {
            Ok(row.id.clone())
        }
        async fn update(&self, _id: &str, _updates: &nomifun_db::ConversationRowUpdate) -> Result<(), DbError> {
            Ok(())
        }
        async fn delete(&self, _id: &str) -> Result<(), DbError> {
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
            _conversation_id: &str,
        ) -> Result<Vec<nomifun_db::models::ConversationRow>, DbError> {
            Ok(vec![])
        }
        async fn get_messages(
            &self,
            _conv_id: &str,
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
        async fn get_message(&self, _conv_id: &str, message_id: &str) -> Result<Option<MessageRow>, DbError> {
            Ok(self
                .inserts
                .lock()
                .unwrap()
                .iter()
                .find(|row| row.id == message_id)
                .cloned())
        }
        async fn insert_message(&self, row: &MessageRow) -> Result<(), DbError> {
            if self.block_message_inserts.load(AtomicOrdering::SeqCst) {
                std::future::pending::<()>().await;
            }
            if self
                .commit_next_message_insert_then_error
                .swap(false, AtomicOrdering::SeqCst)
            {
                self.inserts.lock().unwrap().push(row.clone());
                return Err(DbError::Init(
                    "injected committed-but-unacknowledged message insert".to_owned(),
                ));
            }
            if self.fail_message_inserts.load(AtomicOrdering::SeqCst) {
                return Err(DbError::Conflict("injected message insert failure".to_owned()));
            }
            if self.fail_next_message_insert.swap(false, AtomicOrdering::SeqCst) {
                return Err(DbError::Conflict("injected message insert failure".to_owned()));
            }
            if self
                .reject_duplicate_message_inserts
                .load(AtomicOrdering::SeqCst)
                && self.inserts.lock().unwrap().iter().any(|existing| existing.id == row.id)
            {
                return Err(DbError::Conflict("injected duplicate message insert".to_owned()));
            }
            self.inserts.lock().unwrap().push(row.clone());
            Ok(())
        }
        async fn commit_turn_artifact_messages(
            &self,
            conversation_id: &str,
            turn_message_id: &str,
            messages: &[TurnArtifactMessageCommit],
            committed_at: i64,
        ) -> Result<Vec<MessageRow>, DbError> {
            if self.block_artifact_commits.load(AtomicOrdering::SeqCst) {
                std::future::pending::<()>().await;
            }
            if self.fail_artifact_commits.load(AtomicOrdering::SeqCst) {
                return Err(DbError::Conflict(
                    "injected atomic artifact commit failure".to_owned(),
                ));
            }

            let mut inserts = self.inserts.lock().unwrap();
            let mut updates = self.updates.lock().unwrap();
            for message in messages {
                if let Some(existing) = inserts.iter().find(|row| row.id == message.id)
                    && (existing.conversation_id != conversation_id
                        || existing.msg_id.as_deref() != Some(turn_message_id)
                        || existing.r#type != message.message_type
                        || existing.status.as_deref() != Some("work"))
                {
                    return Err(DbError::Conflict(
                        "injected repository found an incompatible provisional artifact row"
                            .to_owned(),
                    ));
                }
            }
            let mut committed = Vec::with_capacity(messages.len());
            for message in messages {
                if let Some(existing) = inserts.iter().find(|row| row.id == message.id) {
                    updates.push((
                        message.id.clone(),
                        nomifun_db::MessageRowUpdate {
                            content: Some(message.content.clone()),
                            status: Some(Some("finish".to_owned())),
                            hidden: None,
                        },
                    ));
                    let mut row = existing.clone();
                    row.content = message.content.clone();
                    row.status = Some("finish".to_owned());
                    committed.push(row);
                } else {
                    let row = MessageRow {
                        id: message.id.clone(),
                        conversation_id: conversation_id.to_owned(),
                        msg_id: Some(turn_message_id.to_owned()),
                        r#type: message.message_type.clone(),
                        content: message.content.clone(),
                        position: Some("left".to_owned()),
                        status: Some("finish".to_owned()),
                        hidden: false,
                        created_at: committed_at,
                    };
                    inserts.push(row.clone());
                    committed.push(row);
                }
            }
            Ok(committed)
        }
        async fn claim_message_correlation(
            &self,
            conversation_id: &str,
            turn_message_id: &str,
            message_type: &str,
            correlation_key: &str,
        ) -> Result<String, DbError> {
            if self.fail_message_correlations.load(AtomicOrdering::SeqCst) {
                return Err(DbError::Conflict(
                    "injected message correlation failure".to_owned(),
                ));
            }
            let key = (
                conversation_id.to_owned(),
                turn_message_id.to_owned(),
                message_type.to_owned(),
                correlation_key.to_owned(),
            );
            Ok(self
                .correlations
                .lock()
                .unwrap()
                .entry(key)
                .or_insert_with(|| MessageId::new().into_string())
                .clone())
        }
        async fn update_message(&self, id: &str, updates: &nomifun_db::MessageRowUpdate) -> Result<(), DbError> {
            if self.block_message_updates.load(AtomicOrdering::SeqCst) {
                std::future::pending::<()>().await;
            }
            let attempt = self
                .message_update_attempts
                .fetch_add(1, AtomicOrdering::SeqCst)
                + 1;
            if self.fail_message_updates.load(AtomicOrdering::SeqCst)
                || self.fail_next_message_update.swap(false, AtomicOrdering::SeqCst)
                || self.fail_message_update_attempt.load(AtomicOrdering::SeqCst) == attempt
            {
                return Err(DbError::Conflict("injected message update failure".to_owned()));
            }
            self.updates.lock().unwrap().push((id.to_owned(), updates.clone()));
            Ok(())
        }
        async fn delete_messages_by_conversation(&self, _conv_id: &str) -> Result<(), DbError> {
            Ok(())
        }
        async fn get_message_by_msg_id(
            &self,
            _conv_id: &str,
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

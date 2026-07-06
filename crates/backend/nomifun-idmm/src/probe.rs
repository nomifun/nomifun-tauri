//! `SessionProbe` — the target abstraction unifying conversation agents and
//! terminal/agent-CLI sessions. A probe normalizes a session's activity into a
//! `SessionSignal` stream (`observe`), injects wake/answer actions (`inject`),
//! and snapshots recent context for the sidecar (`snapshot_context`).

use std::collections::VecDeque;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use nomifun_ai_agent::{AcpPermissionEventData, AcpPermissionOptionKind, AcpToolCallKind, AgentStreamEvent, TurnStopReason};
use nomifun_ai_agent::task_manager::IWorkerTaskManager;
use nomifun_api_types::{ConfirmRequest, IdmmTargetKind, SendMessageRequest};
use nomifun_common::{AppError, Confirmation};
use nomifun_conversation::ConversationService;
use nomifun_db::{IConversationRepository, SortOrder};
use nomifun_terminal::TerminalDriver;
use tokio::sync::mpsc;

use crate::detector::{
    TerminalDetector, detect_chat_decision, detect_chat_open_question, has_open_intent, signal_from_agent_error,
};
use crate::signal::{DecisionKind, DecisionPrompt, DecisionSource, PermissionConfirm, SessionSignal, WakeAction};

/// Lightweight session metadata for gating + ownership.
#[derive(Debug, Clone)]
pub struct SessionDescription {
    pub kind: IdmmTargetKind,
    pub backend: Option<String>,
    pub user_id: String,
    pub alive: bool,
}

/// The capability IDMM needs from any supervised session.
#[async_trait]
pub trait SessionProbe: Send + Sync {
    fn target(&self) -> (IdmmTargetKind, String);
    /// Normalized signal stream. The implementation spawns the translation task;
    /// the receiver closes when the session ends.
    fn observe(&self, idle_threshold: Duration) -> mpsc::Receiver<SessionSignal>;
    /// Inject a wake/answer action into the session.
    async fn inject(&self, action: &WakeAction) -> Result<(), AppError>;
    /// Recent context for the sidecar (chat: last K messages; terminal: scrollback).
    async fn snapshot_context(&self, max_chars: usize) -> Result<String, AppError>;
    fn is_alive(&self) -> bool;
    async fn describe(&self) -> Result<SessionDescription, AppError>;
    /// The supervised session's own `(provider_id, model)`, used as the
    /// sidecar's bypass model when no dedicated backup is configured — so the
    /// sidecar tier works out-of-the-box on a plain desktop chat ("全托管" is one
    /// click). Default `None`; a terminal has no callable model of its own (its
    /// agent CLI manages that), so only `ConversationProbe` overrides this.
    async fn fallback_model(&self) -> Option<(String, String)> {
        None
    }
    /// On arm, the conversation's CURRENT pending decision (agent already asked
    /// and is waiting), if any. Default None — only ConversationProbe scans its
    /// last persisted assistant turn; terminals/mock get None.
    async fn pending_signal(&self) -> Option<SessionSignal> {
        None
    }
    /// Whether `turn_text` (the just-finished turn's assistant text, held IN
    /// MEMORY by the caller) is a pending decision IDMM's decision watch would
    /// answer — gated to plain-desktop. Unlike [`Self::pending_signal`] this reads
    /// NO persisted message rows, so a caller (AutoWork's decision-yield) can
    /// decide without racing the stream relay's status-flip write. Default false;
    /// only ConversationProbe overrides (terminal/mock have no chat-text turns).
    async fn decision_in_text(&self, _turn_text: &str) -> bool {
        false
    }
}

/// Pure mapping of one agent event to an optional signal. Unit-tested directly
/// so `ConversationProbe.observe` stays a thin wrapper. Returns `None` for
/// events that are neither activity nor a stall (rare; most map to `Working`).
pub fn map_agent_event(ev: &AgentStreamEvent) -> Option<SessionSignal> {
    match ev {
        AgentStreamEvent::Error(d) => Some(signal_from_agent_error(d)),
        // The stop_reason matters: a user cancel must NOT look like a clean
        // Done — policy needs to stand down (suppress nudges) rather than
        // treat the very next signal as a recoverable stall.
        AgentStreamEvent::Finish(d) => Some(if matches!(d.stop_reason, Some(TurnStopReason::Cancelled)) {
            SessionSignal::Cancelled
        } else {
            SessionSignal::Done
        }),
        AgentStreamEvent::Permission(v) => Some(SessionSignal::Decision(permission_decision_from_value(v))),
        AgentStreamEvent::AcpPermission(d) => Some(SessionSignal::Decision(permission_decision_from_acp(d))),
        // All other events are activity → reset idle.
        _ => Some(SessionSignal::Working),
    }
}

fn permission_text(v: &serde_json::Value) -> String {
    v.get("message")
        .or_else(|| v.get("title"))
        .and_then(|m| m.as_str())
        .unwrap_or("agent requested a permission decision")
        .to_string()
}

/// An ACP tool kind is safe to auto-approve without a model when it is
/// read-only / non-mutating. Edit/Execute must escalate to the sidecar (model
/// judges with the tool details) or a human — never blanket auto-approve.
fn acp_tool_is_safe(kind: Option<AcpToolCallKind>) -> bool {
    !matches!(kind, Some(AcpToolCallKind::Edit) | Some(AcpToolCallKind::Execute))
}

/// A `Confirmation.command_type` ("read"/"edit"/"execute") is auto-safe when
/// read-only (or unknown). Mirrors `acp_tool_is_safe` for the nomi/openclaw path.
fn command_type_is_safe(command_type: Option<&str>) -> bool {
    !matches!(command_type, Some("edit") | Some("execute"))
}

/// Build a permission decision from a raw `Permission(Value)` payload (a
/// serialized `Confirmation`). Falls back to a NON-confirmable text decision
/// when the payload lacks a usable call_id (rare; the structured `AcpPermission`
/// path is the live one).
fn permission_decision_from_value(v: &serde_json::Value) -> DecisionPrompt {
    match serde_json::from_value::<Confirmation>(v.clone()) {
        Ok(conf) if !conf.call_id.is_empty() => permission_decision_from_confirmation(&conf),
        _ => DecisionPrompt {
            text: permission_text(v),
            options: vec![],
            recommended: None,
            source: DecisionSource::Permission,
            kind: DecisionKind::Options,
            permission: None,
        },
    }
}

/// Build a structured permission decision from an ACP permission event. The
/// `Request` variant preserves per-option `kind`, so the conservatively-safe
/// "allow once" option is identified precisely.
fn permission_decision_from_acp(d: &AcpPermissionEventData) -> DecisionPrompt {
    match d {
        AcpPermissionEventData::Request(req) => {
            let safe_tool = acp_tool_is_safe(req.tool_call.kind);
            let options: Vec<(String, String)> =
                req.options.iter().map(|o| (o.name.clone(), o.option_id.clone())).collect();
            let safe_value = if safe_tool {
                req.options
                    .iter()
                    .find(|o| matches!(o.kind, AcpPermissionOptionKind::AllowOnce))
                    .map(|o| o.option_id.clone())
            } else {
                None
            };
            DecisionPrompt {
                text: req
                    .tool_call
                    .title
                    .clone()
                    .unwrap_or_else(|| "agent requested a tool permission".to_string()),
                options: req.options.iter().map(|o| o.name.clone()).collect(),
                recommended: None,
                source: DecisionSource::Permission,
                kind: DecisionKind::Options,
                permission: Some(PermissionConfirm {
                    call_id: req.tool_call.tool_call_id.clone(),
                    options,
                    safe_value,
                }),
            }
        }
        AcpPermissionEventData::Confirmation(conf) => permission_decision_from_confirmation(conf),
    }
}

/// Build a structured permission decision from a `Confirmation` (nomi/openclaw
/// path + the ACP `Confirmation` variant). The safe "proceed once" option is
/// matched by its submit-value token (kind isn't carried on a `Confirmation`).
fn permission_decision_from_confirmation(conf: &Confirmation) -> DecisionPrompt {
    let safe_tool = command_type_is_safe(conf.command_type.as_deref());
    let options: Vec<(String, String)> = conf
        .options
        .iter()
        .map(|o| (o.label.clone(), o.value.as_str().unwrap_or_default().to_string()))
        .collect();
    let safe_value = if safe_tool {
        options
            .iter()
            .map(|(_, v)| v.clone())
            .find(|v| {
                let low = v.to_lowercase();
                (low.contains("once") || low.contains("proceed") || low == "allow" || low == "yes")
                    && !low.contains("always")
                    && !crate::config::is_cancel_option(v)
            })
    } else {
        None
    };
    DecisionPrompt {
        text: conf.title.clone().filter(|t| !t.is_empty()).unwrap_or_else(|| conf.description.clone()),
        options: conf.options.iter().map(|o| o.label.clone()).collect(),
        recommended: None,
        source: DecisionSource::Permission,
        kind: DecisionKind::Options,
        permission: Some(PermissionConfirm {
            call_id: conf.call_id.clone(),
            options,
            safe_value,
        }),
    }
}

/// Pure decision for the conversation idle ticker. Extracted so the
/// user-cancel cross-check is unit-testable without driving the async loop.
///
/// A user stop must stand the supervisor down even on backends that never
/// emit `Finish(Cancelled)` (OpenClaw emits `Finish(None)`, Remote emits
/// nothing) — so a cancel stamp recorded since work started wins over the
/// idle nudge. This mirrors AutoWork's existing `user_cancelled_since`
/// double-safeguard; it does not replace the `Finish(Cancelled)` mapping.
fn idle_decision(saw_activity: bool, cancelled_since_work: bool) -> Option<SessionSignal> {
    if cancelled_since_work {
        // Reuse the existing stand-down path (Cancelled → on_user_cancel).
        Some(SessionSignal::Cancelled)
    } else if !saw_activity {
        Some(SessionSignal::Idle)
    } else {
        None
    }
}

/// Cap on the per-turn assistant-text buffer used for end-of-turn chat-decision
/// detection. A numbered menu + its prompt live near the end of a turn, so the
/// tail is what matters; this bounds memory on very long turns.
const TURN_TEXT_CAP: usize = 16_000;

/// Append a streamed assistant text chunk to the per-turn buffer, keeping only
/// the trailing `TURN_TEXT_CAP` chars (decisions appear at the end of a turn).
fn push_turn_text(buf: &mut String, chunk: &str) {
    buf.push_str(chunk);
    if buf.len() > TURN_TEXT_CAP {
        let cut = buf.len() - TURN_TEXT_CAP;
        // Snap to a char boundary so the truncation never splits a UTF-8 byte.
        let cut = (cut..=buf.len()).find(|&i| buf.is_char_boundary(i)).unwrap_or(buf.len());
        *buf = buf.split_off(cut);
    }
}

/// Whether `conversation.extra` marks this as a conversation whose
/// numbered-option menus are routed to a REMOTE human (channel master /
/// companion). IDMM must NOT auto-answer chat decisions for these — the menu is
/// the deliberate human-in-the-loop wire contract (channel `PendingDecisionStore`
/// / companion master reply). Mirrors the conversation layer's own canonical
/// routing definition (`companion_context_from_extra`): `channelPlatform`,
/// `companionId`, `companionSession`.
///
/// `desktopGateway`/`desktop_gateway` is deliberately NOT consulted: the
/// capability-bus super-gateway grants it to EVERY locally-trusted desktop
/// conversation, so it is a capability ENTITLEMENT, not a routing signal.
/// Treating it as routing made the decision watch silently inert on every
/// desktop conversation. ACP channel sessions carry only `desktopGateway` in
/// extra and so are caught instead by the row-level `channel_chat_id`
/// (see [`conversation_is_routed`]). Pure + unit-tested.
fn extra_marks_routed_conversation(extra: &str) -> bool {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(extra) else {
        return false;
    };
    let truthy_str = |k: &str| v.get(k).and_then(|x| x.as_str()).map(|s| !s.trim().is_empty()).unwrap_or(false);
    let truthy_bool = |k: &str| v.get(k).and_then(|x| x.as_bool()).unwrap_or(false);
    truthy_str("channelPlatform")
        || truthy_str("companionId")
        || truthy_bool("companionSession")
}

/// Whether a conversation routes its decisions to a REMOTE human, so IDMM must
/// NOT auto-answer them. Combines the extra-marker check
/// ([`extra_marks_routed_conversation`]) with the row-level `channel_chat_id`,
/// which is set for EVERY channel session — including ACP channel sessions
/// (e.g. claude/codex bound to an IM channel) that carry no companion extra
/// marker and would otherwise be indistinguishable from a plain desktop chat.
/// A blank `channel_chat_id` does not count.
fn conversation_is_routed(extra: &str, channel_chat_id: Option<&str>) -> bool {
    extra_marks_routed_conversation(extra)
        || channel_chat_id.map(|s| !s.trim().is_empty()).unwrap_or(false)
}

/// Decide the supervision signal for a chat-conversation turn-end (`Finish`).
///
/// Ordering (each guard wins over the next):
/// 1. A user cancel — `Finish(stop_reason=Cancelled)` OR the `cancelled_since_work`
///    cross-check stamp — stands the supervisor down (`Cancelled`), never an
///    auto-answer of a turn the user just stopped.
/// 2. A PLAIN-DESKTOP turn that ended on a numbered-option / "请回复编号" decision
///    is a `Decision` (kind=Options) stall (so the policy can auto-answer /
///    escalate), NOT a clean `Done` (which the Req3 normal-stop guard would
///    swallow as benign).
/// 3. A PLAIN-DESKTOP turn that ended on an OPEN-ended question with no options
///    (D6 纯问答) is a `Decision` (kind=OpenQuestion) stall — only the decision
///    watch's model tier may answer it (the rule tier never guesses an open
///    answer). The Finish only fires after work began, so this is by definition
///    a question DURING an unfinished task (work_in_progress).
/// 4. Otherwise the turn finished normally → `Done`.
///
/// Pure + unit-tested; `ConversationProbe::observe` is the only caller.
fn finish_signal(
    stop_reason: Option<TurnStopReason>,
    turn_text: &str,
    is_plain_desktop: bool,
    cancelled_since_work: bool,
) -> SessionSignal {
    if matches!(stop_reason, Some(TurnStopReason::Cancelled)) || cancelled_since_work {
        return SessionSignal::Cancelled;
    }
    if is_plain_desktop {
        if let Some(dp) = detect_chat_decision(turn_text) {
            return SessionSignal::Decision(dp);
        }
        if let Some(dp) = detect_chat_open_question(turn_text) {
            return SessionSignal::Decision(dp);
        }
    }
    SessionSignal::Done
}

/// Number of recent messages to fetch when scanning for the conversation's
/// CURRENT pending decision on arm. A decision menu + its prompt live in the
/// last assistant turn, but a burst of trailing tool_call/tips rows can follow
/// it, so we match `snapshot_context`'s window (20) — the scan still takes the
/// latest *text* row within the page, so a non-text tail never hides the menu.
const PENDING_SCAN_PAGE_SIZE: u32 = 20;

/// On-arm recovery of a pending tool-permission CONFIRMATION (the agent is
/// BLOCKED awaiting approval right now).
///
/// `observe()` subscribes only to FUTURE events, so an `AcpPermission`/
/// `Permission` the agent emitted BEFORE the watch armed is invisible to the
/// live lane; and `pending_signal_from_page` only scans persisted assistant
/// TEXT — a structured confirmation is not a chat-text row, so it was never
/// recovered. Result: arming 智能决策 while a tool-confirmation 选择项 is already on
/// screen left the agent blocked forever and IDMM silent ("完全不可用").
///
/// Recover it from the live task's pending-confirmation list directly — the same
/// `get_confirmations()` source `ConversationService::confirm`/`list_confirmations`
/// read — and map the first to a `Decision` exactly as [`map_agent_event`] maps a
/// live `AcpPermission`. Queried via the task manager (mirroring `observe()`'s
/// own `get_task`), NOT via `conversation_service`, so on-arm READ detection
/// never couples to the row-owner check. Returns `None` when there is no live
/// task or no pending confirmation. Pure given the task manager; the mapping is
/// the unit-tested [`permission_decision_from_confirmation`].
fn pending_confirmation_signal(
    task_manager: &Arc<dyn IWorkerTaskManager>,
    conversation_id: &str,
) -> Option<SessionSignal> {
    let conf = task_manager
        .get_task(conversation_id)?
        .get_confirmations()
        .into_iter()
        .next()?;
    Some(SessionSignal::Decision(permission_decision_from_confirmation(&conf)))
}

/// Pure scan for the conversation's CURRENT pending decision from a (newest-first)
/// page of recent messages — the on-arm replay for "IDMM enabled AFTER the agent
/// already asked and the turn ended". `observe()` subscribes only to FUTURE
/// events, so the already-emitted turn-end decision is never replayed; this
/// recovers it from what's already persisted.
///
/// Returns the recovered signal paired with the candidate row's `created_at`
/// (ms), so the caller can run the same user-cancel cross-check the live
/// idle/finish path uses (don't revive a turn the user just stopped).
///
/// Ordering mirrors `finish_signal` (decision then open-question), and gates on
/// PLAIN DESKTOP the same way `observe` does (routed channel/companion/desktop-
/// gateway conversations route menus to a remote human → never auto-answered).
///
/// IDEMPOTENCY: if the most-recent text message is NOT an assistant turn
/// (position != "left" — i.e. a user/idmm reply at "right" is the last speaker),
/// there is no pending assistant decision → `None`. The last-speaker check spans
/// hidden rows on purpose: IDMM's own injected answer persists as
/// `position:"right" hidden:true`, so once it has answered, that hidden reply is
/// the latest text and a re-arm's scan returns `None` (no re-fire).
///
/// TERMINAL STATUS: the latest "left" text row is only a pending decision when
/// it is a cleanly-FINISHED assistant turn (`status == Some("finish")`). This
/// mirrors the live path, which only fires on `Finish` — a row still in
/// `"work"`/`"pending"` (or any other / missing status) is mid-stream or not a
/// stable turn-end, so we don't auto-answer it.
///
/// Pure + unit-tested; `ConversationProbe::pending_signal` is the only caller.
fn pending_signal_from_page(extra: &str, messages: &[nomifun_db::models::MessageRow]) -> Option<(SessionSignal, i64)> {
    // Routed conversations route menus to a remote human — never auto-answer.
    if extra_marks_routed_conversation(extra) {
        return None;
    }
    // The page is newest-first (Desc). The last-speaker check considers hidden
    // rows too (an idmm answer is hidden:"right"); the decision-text extraction
    // below only runs when that latest text is a visible-or-not assistant "left"
    // turn — assistant decision menus are persisted non-hidden.
    let last = messages.iter().find(|m| m.r#type == "text")?;
    // Idempotency: the assistant is only waiting when it spoke last (position
    // "left"); a "right" reply means the user/idmm already answered.
    if last.position.as_deref() != Some("left") {
        return None;
    }
    // Only a cleanly-finished turn is a stable pending decision (mirror the live
    // path's Finish-only firing); a mid-stream / not-cleanly-ended row is not.
    if last.status.as_deref() != Some("finish") {
        return None;
    }
    let text = serde_json::from_str::<serde_json::Value>(&last.content)
        .ok()
        .and_then(|v| v.get("content").and_then(|c| c.as_str()).map(|s| s.to_string()))
        .unwrap_or_else(|| last.content.clone());
    if let Some(dp) = detect_chat_decision(&text) {
        return Some((SessionSignal::Decision(dp), last.created_at));
    }
    if let Some(dp) = detect_chat_open_question(&text) {
        return Some((SessionSignal::Decision(dp), last.created_at));
    }
    None
}

/// Supervises a chat conversation's agent task.
#[derive(Clone)]
pub struct ConversationProbe {
    pub task_manager: Arc<dyn IWorkerTaskManager>,
    pub conversation_service: ConversationService,
    pub conversation_repo: Arc<dyn IConversationRepository>,
    pub conversation_id: String,
    pub user_id: String,
}

#[async_trait]
impl SessionProbe for ConversationProbe {
    fn target(&self) -> (IdmmTargetKind, String) {
        (IdmmTargetKind::Conversation, self.conversation_id.clone())
    }

    fn observe(&self, idle_threshold: Duration) -> mpsc::Receiver<SessionSignal> {
        let (tx, rx) = mpsc::channel(64);
        // Attach lazily: if no agent exists yet there is nothing to observe; the
        // supervisor re-arms on the next loop tick / status fetch.
        let Some(instance) = self.task_manager.get_task(&self.conversation_id) else {
            // Closed receiver-with-no-sender-task: drop tx so observe yields nothing.
            return rx;
        };
        let mut sub = instance.as_task().subscribe();
        // Cloned into the observe task for the idle-tick user-cancel cross-check
        // and the plain-desktop gating lookup.
        let conversation_service = self.conversation_service.clone();
        let conversation_repo = self.conversation_repo.clone();
        let conversation_id = self.conversation_id.clone();
        tokio::spawn(async move {
            // Gate end-of-turn chat-decision detection to PLAIN DESKTOP
            // conversations: channel-master / companion / desktop-gateway
            // conversations route numbered-option menus to a remote human and
            // must NOT be auto-answered (the menu is their human-in-the-loop
            // wire contract). Default false (no text-scan) when the row/extra
            // can't be read — conservative: never hijack when unsure.
            let is_plain_desktop = match conversation_id.parse::<i64>() {
                Ok(id) => matches!(
                    conversation_repo.get(id).await,
                    Ok(Some(ref row)) if !conversation_is_routed(&row.extra, row.channel_chat_id.as_deref())
                ),
                Err(_) => false,
            };

            let mut ticker = tokio::time::interval(idle_threshold);
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            ticker.tick().await; // consume the immediate first tick
            let mut saw_activity = false;
            // Timestamp of the most recent `Working` transition (init at observe
            // start). A cancel stamp at or after this means the user stopped the
            // current work — the idle tick must stand down rather than nudge.
            let mut work_epoch_ms = nomifun_common::now_ms();
            // Per-turn assistant text, accumulated for end-of-turn chat-decision
            // detection (reset at each turn's Start and after each Finish).
            let mut turn_text = String::new();
            loop {
                tokio::select! {
                    ev = sub.recv() => match ev {
                        Ok(ev) => {
                            // Accumulate assistant text for the end-of-turn
                            // decision scan; reset it when a new turn starts.
                            match &ev {
                                AgentStreamEvent::Start(_) => turn_text.clear(),
                                AgentStreamEvent::Text(d) => push_turn_text(&mut turn_text, &d.content),
                                _ => {}
                            }
                            // Finish is resolved against the buffered turn text
                            // (a "请回复编号" turn-end is a Decision, not a clean
                            // Done); every other event keeps the pure mapping.
                            let sig = match &ev {
                                AgentStreamEvent::Finish(d) => {
                                    let cancelled = conversation_service
                                        .user_cancelled_since(&conversation_id, work_epoch_ms);
                                    let s = finish_signal(d.stop_reason, &turn_text, is_plain_desktop, cancelled);
                                    turn_text.clear();
                                    Some(s)
                                }
                                _ => map_agent_event(&ev),
                            };
                            if let Some(sig) = sig {
                                if matches!(sig, SessionSignal::Working) {
                                    saw_activity = true;
                                    work_epoch_ms = nomifun_common::now_ms();
                                }
                                if tx.send(sig).await.is_err() {
                                    break;
                                }
                            }
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                            let _ = tx.send(SessionSignal::Exited).await;
                            break;
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                    },
                    _ = ticker.tick() => {
                        let cancelled = conversation_service
                            .user_cancelled_since(&conversation_id, work_epoch_ms);
                        if cancelled {
                            tracing::debug!(
                                "IDMM idle-tick observed user cancel — standing down"
                            );
                        }
                        if let Some(sig) = idle_decision(saw_activity, cancelled) {
                            if tx.send(sig).await.is_err() {
                                break;
                            }
                        }
                        saw_activity = false;
                    }
                }
            }
        });
        rx
    }

    async fn inject(&self, action: &WakeAction) -> Result<(), AppError> {
        // Structured tool-permission approval: resolve the agent's pending
        // confirmation oneshot via `confirm` (a hidden chat message would never
        // clear it). `data` carries the submit-value under BOTH keys so either
        // backend resolves it (ACP reads `option_id`, nomi reads `value`).
        if let WakeAction::Confirm {
            call_id,
            value,
            always_allow,
        } = action
        {
            let req = ConfirmRequest {
                msg_id: String::new(),
                data: serde_json::json!({ "option_id": value, "value": value }),
                always_allow: *always_allow,
            };
            return self
                .conversation_service
                .confirm(&self.user_id, &self.conversation_id, call_id, req, &self.task_manager)
                .await;
        }
        // Model failover (D6): switch to the next queue candidate and re-drive
        // the turn via the conversation service's SHARED failover helper — the
        // same `perform_model_failover` the send-loop uses (one source of truth
        // for the swap). The helper resolves the effective config (session
        // override else global). It returns `Ok(false)` when NO switch happened
        // (failover disabled / queue exhausted / deps unregistered — and now also
        // when the conversation is not nomi: review #9 moved the ACP boundary gate
        // INTO `perform_model_failover`, so an ACP conversation reaching here is
        // safely rejected there and reports `Ok(false)`). On `Ok(false)` we do NOT
        // return early (review #8 — a misconfigured/exhausted slot must still
        // nudge): we fall through to the Retry path below so the watch ladder
        // keeps the turn moving instead of stalling on a burned slot.
        if matches!(action, WakeAction::Failover) {
            let switched = self
                .conversation_service
                .idmm_failover_conversation(&self.user_id, &self.conversation_id, &self.task_manager)
                .await?;
            if switched {
                return Ok(());
            }
            // No switch → fall through to the Retry nudge below.
        }
        let content = match action {
            WakeAction::Retry | WakeAction::Failover => "Please continue.".to_string(),
            WakeAction::SendText(s) | WakeAction::AnswerChoice(s) => s.clone(),
            WakeAction::Wait(_) | WakeAction::Stop(_) | WakeAction::Confirm { .. } => return Ok(()),
        };
        let req = SendMessageRequest {
            content,
            files: vec![],
            inject_skills: vec![],
            hidden: true,
            origin: Some("idmm".into()),
            channel_platform: None,
        };
        self.conversation_service
            .send_message(&self.user_id, &self.conversation_id, req, &self.task_manager)
            .await
            .map(|_| ())
    }

    async fn snapshot_context(&self, max_chars: usize) -> Result<String, AppError> {
        let conv_id = self
            .conversation_id
            .parse::<i64>()
            .map_err(|_| AppError::NotFound(format!("conversation {}", self.conversation_id)))?;
        let page = self
            .conversation_repo
            .get_messages(conv_id, 0, 20, SortOrder::Desc)
            .await
            .map_err(AppError::from)?;
        // Oldest→newest for readability (repo returned newest-first).
        let lines: Vec<String> = page
            .items
            .iter()
            .rev()
            .filter(|m| !m.hidden && m.r#type == "text")
            .map(|m| {
                let role = match m.position.as_deref() {
                    Some("right") => "user",
                    _ => "assistant",
                };
                let text = serde_json::from_str::<serde_json::Value>(&m.content)
                    .ok()
                    .and_then(|v| v.get("content").and_then(|c| c.as_str()).map(|s| s.to_string()))
                    .unwrap_or_else(|| m.content.clone());
                format!("{role}: {text}")
            })
            .collect();
        let joined = lines.join("\n");
        Ok(crate::util::tail_chars(&joined, max_chars))
    }

    fn is_alive(&self) -> bool {
        self.task_manager.get_task(&self.conversation_id).is_some()
    }

    async fn describe(&self) -> Result<SessionDescription, AppError> {
        let conv_id = self
            .conversation_id
            .parse::<i64>()
            .map_err(|_| AppError::NotFound(format!("conversation {}", self.conversation_id)))?;
        let row = self
            .conversation_repo
            .get(conv_id)
            .await
            .map_err(AppError::from)?;
        let (user_id, backend) = match row {
            Some(c) => (c.user_id, Some(c.r#type)),
            None => (self.user_id.clone(), None),
        };
        Ok(SessionDescription {
            kind: IdmmTargetKind::Conversation,
            backend,
            user_id,
            alive: self.is_alive(),
        })
    }

    async fn fallback_model(&self) -> Option<(String, String)> {
        let conv_id = self.conversation_id.parse::<i64>().ok()?;
        let row = self.conversation_repo.get(conv_id).await.ok()??;
        let pm = nomifun_conversation::task_options::provider_model_from_conversation_row(&row);
        if pm.provider_id.trim().is_empty() {
            return None;
        }
        Some((pm.provider_id, pm.model))
    }

    async fn pending_signal(&self) -> Option<SessionSignal> {
        let conv_id = self.conversation_id.parse::<i64>().ok()?;
        // Gate on plain-desktop via the row (same gating as `observe`): routed
        // conversations (channel/companion by extra marker, or any channel
        // session by row-level `channel_chat_id`) route decisions to a remote
        // human and must never be auto-answered.
        let row = self.conversation_repo.get(conv_id).await.ok()??;
        if conversation_is_routed(&row.extra, row.channel_chat_id.as_deref()) {
            return None;
        }
        // A live pending tool-permission confirmation means the agent is BLOCKED
        // on it right now. observe() missed any pre-arm permission event and the
        // text scan below never sees a structured confirmation, so this is the
        // ONLY lane that can recover it on arm — check it first (the block is the
        // most urgent pending decision). See [`pending_confirmation_signal`].
        if let Some(sig) = pending_confirmation_signal(&self.task_manager, &self.conversation_id) {
            return Some(sig);
        }
        let page = self
            .conversation_repo
            .get_messages(conv_id, 0, PENDING_SCAN_PAGE_SIZE, SortOrder::Desc)
            .await
            .ok()?;
        let (sig, candidate_at) = pending_signal_from_page(&row.extra, &page.items)?;
        // Respect a user cancel: if the user deliberately stopped at or after
        // this candidate decision row was written, do NOT auto-answer/revive it
        // — mirror the live idle/finish path's `user_cancelled_since` cross-check
        // (a stand-down the on-arm replay must honour just as the stream does).
        if self.conversation_service.user_cancelled_since(&self.conversation_id, candidate_at) {
            return None;
        }
        Some(sig)
    }

    async fn decision_in_text(&self, turn_text: &str) -> bool {
        if turn_text.trim().is_empty() {
            return false;
        }
        let Ok(conv_id) = self.conversation_id.parse::<i64>() else {
            return false;
        };
        // Plain-desktop gate (same as observe/pending_signal): routed channel /
        // companion conversations send menus to a remote human → never auto-answer.
        let Ok(Some(row)) = self.conversation_repo.get(conv_id).await else {
            return false;
        };
        if conversation_is_routed(&row.extra, row.channel_chat_id.as_deref()) {
            return false;
        }
        // Detect from the turn text itself (no persisted-row status dependency, so
        // no race with the relay): a numbered-option menu OR an open question.
        detect_chat_decision(turn_text).is_some() || detect_chat_open_question(turn_text).is_some()
    }
}

// ──────────────────────────────── TerminalProbe ───────────────────────────

/// Map a structured terminal lifecycle event to a supervision signal.
/// TurnEnd is NOT handled here — `terminal_turn_end_signal` resolves it (it may
/// be a Done OR a Decision(OpenQuestion), depending on the scrollback tail), so
/// the observe loop dispatches TurnEnd to that helper and consults this mapping
/// only for the OTHER kinds. ToolUse/SessionStart→Working (activity, arms
/// work-in-progress); Notification→Idle (claude's "agent is waiting for
/// input/permission" hook — the precise wait signal replacing the unreliable
/// byte-timeout idle; only claude registers it, so codex/unknown get no
/// idle-nudge). Decision/ProviderError content for the OPTIONS path still comes
/// from the byte-scan content channel, not lifecycle hooks.
fn map_lifecycle_event(kind: nomifun_terminal::LifecycleKind) -> Option<SessionSignal> {
    use nomifun_terminal::LifecycleKind;
    match kind {
        // TurnEnd is resolved by `terminal_turn_end_signal` before this mapping
        // is consulted; keep it as Done here as a defensive fallback in case a
        // future caller routes it through (never reached on the live path).
        LifecycleKind::TurnEnd => Some(SessionSignal::Done),
        LifecycleKind::ToolUse | LifecycleKind::SessionStart => Some(SessionSignal::Working),
        LifecycleKind::Notification => Some(SessionSignal::Idle),
    }
}

/// Chars of recent CLEANED scrollback to scan at a terminal turn-end for a
/// pending open-ended question. A trailing question + the chrome around it live
/// at the very end of the tail; this bounds the scan and the de-chrome work.
const TERMINAL_TURN_TAIL_CHARS: usize = 2500;

/// How many trailing NON-EMPTY logical lines of the cleaned scrollback form the
/// "recent region" examined for a turn-end open question (the assistant's last
/// paragraph + its surrounding TUI chrome).
const TERMINAL_TAIL_LINES: usize = 15;

/// Box-drawing / block / shade glyphs a TUI uses to frame its input box and
/// status rows. A line whose trimmed content is ONLY these (plus whitespace) is
/// pure chrome with no message, so it is stripped from the tail before the
/// open-question scan.
const BOX_DRAWING_CHARS: &[char] = &[
    '─', '│', '┌', '┐', '└', '┘', '├', '┤', '┬', '┴', '┼', '╭', '╮', '╰', '╯', '━', '┃', '═', '║',
    '█', '▌', '▐', '░', '▒', '▓',
];

/// Whether a cleaned logical line is TUI chrome carrying no assistant message,
/// so it can be stripped from the trailing region before the open-question scan.
/// Three shapes: (1) an input-box FRAME line — only box-drawing/whitespace plus an
/// optional bare prompt glyph (`❯`/`▶`/`>`) inside the borders (a TUI input box,
/// e.g. `│ >        │`, carries no message); (2) a bare prompt glyph with nothing
/// else; (3) a status/recap line starting with a known TUI status glyph
/// (`✻`/`※`/`⎿`/`●`) that carries NO `?`/`？` (a status line never poses the
/// question — keep any line that does).
fn is_terminal_chrome_line(line: &str) -> bool {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return true;
    }
    // (1) Only box-drawing / block / shade glyphs + whitespace + an optional bare
    // prompt glyph (`❯`/`▶`/`>`) — i.e. an empty input-box frame, not a message.
    if trimmed.chars().all(|c| {
        c.is_whitespace() || BOX_DRAWING_CHARS.contains(&c) || matches!(c, '❯' | '▶' | '>')
    }) {
        return true;
    }
    // (2) A bare prompt glyph with no other text.
    if matches!(trimmed, "❯" | "▶" | ">") {
        return true;
    }
    // (3) A TUI status/recap line that poses no question.
    let starts_status = trimmed.starts_with('✻')
        || trimmed.starts_with('※')
        || trimmed.starts_with('⎿')
        || trimmed.starts_with('●');
    if starts_status && !trimmed.contains('?') && !trimmed.contains('？') {
        return true;
    }
    false
}

/// The de-chromed recent region of a cleaned scrollback `tail`: the last
/// `TERMINAL_TAIL_LINES` NON-EMPTY logical lines, then with TRAILING chrome lines
/// (frames / bare prompts / question-less status rows) stripped until a content
/// line remains. The result is what `detect_chat_open_question` scans, so the
/// agent's actual last question sits at the END (where the chat detectors expect
/// the prompt line). Returns the joined region (may be empty if it was all chrome).
fn dechromed_tail_region(tail: &str) -> String {
    let mut lines: Vec<&str> = tail.lines().filter(|l| !l.trim().is_empty()).collect();
    if lines.len() > TERMINAL_TAIL_LINES {
        lines = lines.split_off(lines.len() - TERMINAL_TAIL_LINES);
    }
    while lines.last().is_some_and(|l| is_terminal_chrome_line(l)) {
        lines.pop();
    }
    lines.join("\n")
}

/// Whether a single de-chromed content line ENDS ON a question — used to gate
/// the terminal turn-end open-question scan so it only fires when the turn
/// actually closes on an interrogative, not when a `?` is buried mid-line.
///
/// A line ends on a question when its LAST sentence-terminating mark is `?`/`？`
/// — so `要不要试试？（需要你打开一个本地 URL）` (the `？` is the last terminator,
/// the parenthetical carries none) counts, while `打开 http://x?token=abc 看看，已完成。`
/// does NOT (the URL `?` is followed by a statement period, the last terminator).
/// A mark-less open-intent cue (`你希望…`, `should i…`) also counts as long as no
/// statement terminator (`。`/`.`/`!`/`！`) closes the line after it. Pure +
/// unit-tested via `terminal_pending_open_question`.
fn line_ends_on_question(line: &str) -> bool {
    let trimmed = line.trim();
    // The LAST sentence-terminating punctuation in the line decides how it ends:
    // a question only if that final terminator is `?`/`？`.
    let last_terminator = trimmed.chars().rev().find(|c| matches!(c, '?' | '？' | '。' | '.' | '!' | '！'));
    if matches!(last_terminator, Some('?') | Some('？')) {
        return true;
    }
    // No question mark closes the line — a mark-less open-intent phrasing still
    // counts, but only when no statement terminator closes the line after it
    // (i.e. the line did not END on a `。`/`.`/`!`/`！` statement).
    if last_terminator.is_none() && has_open_intent(&trimmed.to_lowercase()) {
        return true;
    }
    false
}

/// Scan the recent CLEANED scrollback `tail` for an OPEN-ENDED question the agent
/// ended its turn on, returning a `DecisionPrompt(OpenQuestion)` so the
/// RulePlusModel decision watch's bypass model can answer it. Options / y-n /
/// numbered prompts are NOT handled here — the byte-scan (`detect_decision`) owns
/// those, and `detect_chat_open_question` already returns `None` when the text
/// parses as a discrete-options decision, so a TUI task list / numbered menu is
/// never mis-emitted here (no double-answer with the byte-scan).
///
/// TRAILING-LINE GATE: only fires when the turn actually ENDS ON a question — the
/// LAST content line of the de-chromed region must itself end on an interrogative
/// (`line_ends_on_question`: final sentence terminator is `?`/`？`, or a mark-less
/// `has_open_intent` cue closes the line). A `?` buried mid-region OR mid-line (a
/// URL query string, a ternary in a code recap, a rhetorical mid-turn line) above
/// a final PLAIN statement is NOT a turn-end question → `None`, even though the
/// region as a whole contains a `?`. This is the real auto-answer case ("agent
/// ended its turn on a question") and cuts the wasted bypass-model sidecar calls
/// on common statement-ending turns.
///
/// `source` is overridden to `TerminalScan` (this came from PTY output, not chat
/// text) and `text` is set to the BEST question line — the LAST line in the
/// de-chromed region containing `?`/`？` (falling back to whatever
/// `detect_chat_open_question` returned). Pure + unit-tested.
fn terminal_pending_open_question(tail: &str) -> Option<DecisionPrompt> {
    let region = dechromed_tail_region(tail);
    if region.trim().is_empty() {
        return None;
    }
    // Trailing-line gate: the LAST content line must itself END ON a question,
    // else a `?` buried above (or mid-line within) a final statement would
    // trigger a wasted sidecar bypass call. dechromed_tail_region already dropped
    // empty + trailing-chrome lines, so the last `lines()` entry IS the trailing
    // content line.
    let last_line = region.lines().next_back().unwrap_or_default();
    if !line_ends_on_question(last_line) {
        return None;
    }
    let mut dp = detect_chat_open_question(&region)?;
    dp.source = DecisionSource::TerminalScan;
    if let Some(q) = region
        .lines()
        .rev()
        .map(str::trim)
        .find(|l| l.contains('?') || l.contains('？'))
    {
        dp.text = q.to_string();
    }
    Some(dp)
}

/// Resolve a terminal TurnEnd: scan the recent CLEANED scrollback tail for an
/// OPEN-ENDED question the agent ended its turn on, and emit
/// `Decision(OpenQuestion)` so the RulePlusModel decision watch's bypass model
/// can answer it (the model is the final judge — it returns `action=stop` if
/// there's no real question). Options / y-n / numbered prompts are NOT handled
/// here — the byte-scan (`detect_decision`) owns those, so this never competes
/// (avoids double-answer). Falls back to `Done`.
fn terminal_turn_end_signal(detector: &TerminalDetector) -> SessionSignal {
    let tail = detector.scrollback(TERMINAL_TURN_TAIL_CHARS);
    match terminal_pending_open_question(&tail) {
        Some(dp) => SessionSignal::Decision(dp),
        None => SessionSignal::Done,
    }
}

/// Dedupe guard for the observe task: decide whether `sig` should be sent given
/// the last-sent Decision text. Prevents double-answering the SAME prompt when
/// it surfaces twice in a row (the byte-scan Options path then the TurnEnd path,
/// or repeated TurnEnds for an unanswered prompt). A non-Decision signal that
/// marks a NEW turn (`Working`/`Done`/`Exited`) clears the memory so the same
/// prompt may legitimately fire again later. Returns `true` to send.
fn dedupe_should_send(sig: &SessionSignal, last_decision_text: &mut Option<String>) -> bool {
    match sig {
        SessionSignal::Decision(dp) => {
            if last_decision_text.as_deref() == Some(dp.text.as_str()) {
                return false;
            }
            *last_decision_text = Some(dp.text.clone());
            true
        }
        // A new turn resets the dedupe memory (a fresh prompt may repeat later).
        SessionSignal::Working | SessionSignal::Done | SessionSignal::Exited => {
            *last_decision_text = None;
            true
        }
        _ => true,
    }
}

/// Supervises a PTY-backed terminal session.
#[derive(Clone)]
pub struct TerminalProbe {
    pub driver: Arc<dyn TerminalDriver>,
    pub terminal_id: i64,
    /// Scrollback kept for sidecar context (shared with the observe task).
    scrollback: Arc<std::sync::Mutex<String>>,
    /// Text recently injected into the PTY, shared with the observe task's
    /// detector so the CLI's echo of our own keystrokes isn't re-detected as a
    /// stall (replaces the zero-width-tag scheme that corrupted the bytes the
    /// CLI read).
    recent_injections: Arc<std::sync::Mutex<VecDeque<String>>>,
}

impl TerminalProbe {
    /// Cap on tracked pending echoes (one per recent injection line).
    const MAX_PENDING_ECHO: usize = 16;

    pub fn new(driver: Arc<dyn TerminalDriver>, terminal_id: i64) -> Self {
        Self {
            driver,
            terminal_id,
            scrollback: Arc::new(std::sync::Mutex::new(String::new())),
            recent_injections: Arc::new(std::sync::Mutex::new(VecDeque::new())),
        }
    }

    /// Record an injected payload's lines as pending echoes to skip.
    fn note_injection(&self, text: &str) {
        if let Ok(mut pending) = self.recent_injections.lock() {
            for line in text.lines().map(str::trim).filter(|l| !l.is_empty()) {
                if pending.len() >= Self::MAX_PENDING_ECHO {
                    pending.pop_front();
                }
                pending.push_back(line.to_string());
            }
        }
    }
}

#[async_trait]
impl SessionProbe for TerminalProbe {
    fn target(&self) -> (IdmmTargetKind, String) {
        (IdmmTargetKind::Terminal, self.terminal_id.to_string())
    }

    fn observe(&self, idle_threshold: Duration) -> mpsc::Receiver<SessionSignal> {
        // Terminal idle is now lifecycle-driven (Notification → Idle); the
        // byte-timeout idle_threshold is no longer used for emission.
        let _ = idle_threshold;

        let (tx, rx) = mpsc::channel(64);
        let Some(mut out) = self.driver.subscribe_output(self.terminal_id) else {
            return rx;
        };
        let driver = self.driver.clone();
        let id = self.terminal_id;
        let scrollback = self.scrollback.clone();
        let recent_injections = self.recent_injections.clone();
        let mut lifecycle_rx = self.driver.subscribe_lifecycle(self.terminal_id);
        tokio::spawn(async move {
            let mut detector = TerminalDetector::with_echo_guard(recent_injections);
            let mut ticker = tokio::time::interval(Duration::from_secs(2));
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            // Dedupe guard: the text of the last Decision sent, so the SAME
            // prompt isn't answered twice (byte-scan Options then TurnEnd, or
            // repeated TurnEnds for an unanswered prompt). Cleared on a new turn.
            let mut last_decision_text: Option<String> = None;
            loop {
                tokio::select! {
                    chunk = out.recv() => match chunk {
                        Ok(bytes) => {
                            for sig in detector.feed(&bytes) {
                                if !dedupe_should_send(&sig, &mut last_decision_text) {
                                    continue;
                                }
                                if tx.send(sig).await.is_err() {
                                    return;
                                }
                            }
                            // Keep scrollback fresh for snapshot_context.
                            if let Ok(mut sb) = scrollback.lock() {
                                *sb = detector.scrollback(8000);
                            }
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                            let _ = tx.send(SessionSignal::Exited).await;
                            return;
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                    },
                    lifecycle_ev = async {
                        match lifecycle_rx.as_mut() {
                            Some(rx) => rx.recv().await,
                            None => std::future::pending::<Result<nomifun_terminal::TerminalLifecycleEvent, tokio::sync::broadcast::error::RecvError>>().await,
                        }
                    } => {
                        match lifecycle_ev {
                            Ok(ev) => {
                                // TurnEnd may be a Done OR a Decision(OpenQuestion)
                                // (the agent ended its turn on a question); resolve
                                // it from the scrollback tail. All other kinds keep
                                // the pure mapping.
                                let sig = match ev.kind {
                                    nomifun_terminal::LifecycleKind::TurnEnd => Some(terminal_turn_end_signal(&detector)),
                                    other => map_lifecycle_event(other),
                                };
                                if let Some(sig) = sig {
                                    if dedupe_should_send(&sig, &mut last_decision_text) {
                                        if tx.send(sig).await.is_err() {
                                            return;
                                        }
                                    }
                                }
                            }
                            Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                                let _ = tx.send(SessionSignal::Exited).await;
                                return;
                            }
                            Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                        }
                    },
                    _ = ticker.tick() => {
                        if !driver.is_alive(id) {
                            let _ = tx.send(SessionSignal::Exited).await;
                            return;
                        }
                    }
                }
            }
        });
        rx
    }

    async fn inject(&self, action: &WakeAction) -> Result<(), AppError> {
        let text = match action {
            WakeAction::Retry => "continue".to_string(),
            // 终端/ACP 自管模型(D7),不支持模型故障转移队列。Failover 降级为普通
            // 续聊 nudge(等价 Retry),让终端会话靠 CLI 自身的模型管理继续。
            WakeAction::Failover => "continue".to_string(),
            WakeAction::SendText(s) => s.clone(),
            WakeAction::AnswerChoice(s) => s.clone(),
            // Structured permissions don't exist on a PTY (terminal approvals are
            // plain y/n / numbered text), so a Confirm is a no-op here.
            WakeAction::Wait(_) | WakeAction::Stop(_) | WakeAction::Confirm { .. } => return Ok(()),
        };
        // IDMM's terminal session metadata only carries the declared backend, not
        // the full launcher command/args. Treat that declared backend as the
        // agent-family signal so backend-only terminal presets still get the
        // shared paste-then-CR submit path.
        let is_agent = self
            .driver
            .describe(self.terminal_id)
            .await
            .ok()
            .flatten()
            .and_then(|d| d.backend)
            .map(|b| nomifun_terminal::enhance::resolve_agent_family(&b, &[], Some(&b)).is_some())
            .unwrap_or(false);
        // Track the payload's lines so the CLI's echo of them isn't re-detected.
        self.note_injection(&text);
        match nomifun_terminal::encode_submit_chunks(&text, is_agent) {
            nomifun_terminal::SubmitChunks::Single(bytes) => self
                .driver
                .write_input(self.terminal_id, &bytes)
                .await
                .map_err(|e| AppError::Internal(format!("terminal inject failed: {e}"))),
            nomifun_terminal::SubmitChunks::PasteThenCr { paste, cr } => {
                self.driver
                    .write_input(self.terminal_id, &paste)
                    .await
                    .map_err(|e| AppError::Internal(format!("terminal inject failed: {e}")))?;
                tokio::time::sleep(nomifun_terminal::TERMINAL_SUBMIT_DELAY).await;
                self.driver
                    .write_input(self.terminal_id, &cr)
                    .await
                    .map_err(|e| AppError::Internal(format!("terminal inject failed: {e}")))
            }
        }
    }

    async fn snapshot_context(&self, max_chars: usize) -> Result<String, AppError> {
        let sb = self.scrollback.lock().map(|s| s.clone()).unwrap_or_default();
        Ok(crate::util::tail_chars(&sb, max_chars))
    }

    async fn pending_signal(&self) -> Option<SessionSignal> {
        // On arm: answer a question the terminal is ALREADY stuck on (the
        // supervisor calls this once when decision_watch is enabled). A dead PTY
        // has nothing pending. We only surface an OPEN question here — discrete
        // options are owned by the live byte-scan, which re-emits them on the
        // next output, so re-deriving them on arm would risk a double-answer.
        if !self.is_alive() {
            return None;
        }
        let sb = self.scrollback.lock().map(|s| s.clone()).unwrap_or_default();
        let tail = crate::util::tail_chars(&sb, TERMINAL_TURN_TAIL_CHARS);
        terminal_pending_open_question(&tail).map(SessionSignal::Decision)
    }

    fn is_alive(&self) -> bool {
        self.driver.is_alive(self.terminal_id)
    }

    async fn describe(&self) -> Result<SessionDescription, AppError> {
        let desc = self
            .driver
            .describe(self.terminal_id)
            .await
            .map_err(|e| AppError::Internal(format!("describe failed: {e}")))?;
        match desc {
            Some(d) => Ok(SessionDescription {
                kind: IdmmTargetKind::Terminal,
                backend: d.backend,
                user_id: d.user_id,
                alive: self.is_alive(),
            }),
            None => Err(AppError::NotFound(format!("terminal {} not found", self.terminal_id))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nomifun_api_types::{AgentErrorCode, AgentErrorOwnership, AgentStreamErrorData};

    #[test]
    fn map_agent_event_error_maps_to_provider_or_agent() {
        let ev = AgentStreamEvent::Error(AgentStreamErrorData::classified(
            "500",
            AgentErrorCode::UserLlmProviderGatewayError,
            AgentErrorOwnership::UserLlmProvider,
            None,
            true,
            false,
            None,
        ));
        assert!(matches!(
            map_agent_event(&ev),
            Some(SessionSignal::ProviderError { .. })
        ));
    }

    #[test]
    fn map_agent_event_finish_is_done() {
        let ev = AgentStreamEvent::Finish(Default::default());
        assert_eq!(map_agent_event(&ev), Some(SessionSignal::Done));
    }

    #[test]
    fn map_agent_event_finish_cancelled_is_user_cancel_not_done() {
        // A user stop arrives as Finish(stop_reason=Cancelled). Mapping it to
        // Done made IDMM treat the very next error/idle as a recoverable
        // stall and inject "Please continue." into a session the user had
        // just paused — it must surface as the distinct Cancelled signal.
        let ev = AgentStreamEvent::Finish(nomifun_ai_agent::FinishEventData {
            session_id: None,
            stop_reason: Some(TurnStopReason::Cancelled),
        });
        assert_eq!(map_agent_event(&ev), Some(SessionSignal::Cancelled));
        // Every other stop_reason stays Done (the turn genuinely ended).
        let ev = AgentStreamEvent::Finish(nomifun_ai_agent::FinishEventData {
            session_id: None,
            stop_reason: Some(TurnStopReason::EndTurn),
        });
        assert_eq!(map_agent_event(&ev), Some(SessionSignal::Done));
    }

    #[test]
    fn map_agent_event_permission_is_decision() {
        let ev = AgentStreamEvent::Permission(serde_json::json!({"message": "allow write?"}));
        match map_agent_event(&ev) {
            Some(SessionSignal::Decision(d)) => {
                assert_eq!(d.source, DecisionSource::Permission);
                assert!(d.text.contains("allow write"));
            }
            other => panic!("expected decision, got {other:?}"),
        }
    }

    fn confirmation(command_type: &str) -> Confirmation {
        use nomifun_common::ConfirmationOption;
        Confirmation {
            id: "c1".into(),
            call_id: "call-1".into(),
            title: Some("tool permission".into()),
            action: None,
            description: command_type.into(),
            command_type: Some(command_type.into()),
            options: vec![
                ConfirmationOption {
                    label: "Allow once".into(),
                    value: serde_json::json!("proceed_once"),
                    params: None,
                },
                ConfirmationOption {
                    label: "Always".into(),
                    value: serde_json::json!("proceed_always"),
                    params: None,
                },
                ConfirmationOption {
                    label: "Reject".into(),
                    value: serde_json::json!("cancel"),
                    params: None,
                },
            ],
            screenshot: None,
        }
    }

    #[test]
    fn permission_from_confirmation_read_is_auto_safe() {
        // A read-only tool: call_id + structured options preserved, and the
        // "proceed once" value is the conservatively-safe auto-approve value.
        let dp = permission_decision_from_confirmation(&confirmation("read"));
        let perm = dp.permission.expect("structured permission");
        assert_eq!(perm.call_id, "call-1");
        assert_eq!(perm.options.len(), 3);
        assert_eq!(perm.safe_value.as_deref(), Some("proceed_once"));
    }

    #[test]
    fn permission_from_confirmation_execute_has_no_safe_value() {
        // A write/exec tool must NOT carry an auto-safe value — it escalates to
        // the sidecar (model) or a human.
        let dp = permission_decision_from_confirmation(&confirmation("execute"));
        let perm = dp.permission.expect("structured permission");
        assert!(perm.safe_value.is_none(), "execute must not be auto-safe");
        assert_eq!(perm.call_id, "call-1");
    }

    #[test]
    fn idle_decision_cancel_takes_priority_over_idle() {
        // The core stop-respecting fix: when the user cancelled since work
        // started, the idle ticker must stand down via Cancelled — not nudge
        // via Idle — even if the backend never emitted Finish(Cancelled)
        // (OpenClaw emits Finish(None), Remote emits nothing).
        //
        //   saw_activity, cancelled_since_work → expected signal
        assert_eq!(idle_decision(false, true), Some(SessionSignal::Cancelled));
        assert_eq!(idle_decision(true, true), Some(SessionSignal::Cancelled));
        // No cancel: quiescent past the threshold is a recoverable stall → Idle.
        assert_eq!(idle_decision(false, false), Some(SessionSignal::Idle));
        // Activity since the last tick and no cancel: not stalled → no signal.
        assert_eq!(idle_decision(true, false), None);
    }

    #[test]
    fn idmm_single_line_stays_raw_plus_cr() {
        use nomifun_terminal::{encode_submit_chunks, SubmitChunks};
        // 单行答复（option label / continue）必须 raw+CR、一次写，绝不 bracketed-paste。
        assert_eq!(
            encode_submit_chunks("2) 方案B", false),
            SubmitChunks::Single("2) 方案B\r".as_bytes().to_vec())
        );
        assert_eq!(
            encode_submit_chunks("continue", true),
            SubmitChunks::Single(b"continue\r".to_vec())
        );
    }

    // ── Chat-conversation gating + end-of-turn decision signal ──

    #[test]
    fn plain_desktop_gating_excludes_routed_conversations() {
        // Channel master / companion conversations route numbered menus to a
        // REMOTE human — IDMM must not auto-answer them.
        assert!(extra_marks_routed_conversation(r#"{"channelPlatform":"telegram"}"#));
        assert!(extra_marks_routed_conversation(
            r#"{"companionSession":true,"companionId":"companion_42"}"#
        ));
        // A plain desktop conversation is NOT routed.
        assert!(!extra_marks_routed_conversation(r#"{"workspace":"/project"}"#));
        // Blank companionId / empty / invalid extra do not count as routed.
        assert!(!extra_marks_routed_conversation(r#"{"companionId":""}"#));
        assert!(!extra_marks_routed_conversation(""));
        assert!(!extra_marks_routed_conversation("{}"));
    }

    #[test]
    fn desktop_gateway_is_not_a_routing_marker() {
        // REGRESSION GUARD: the capability-bus super-gateway grants
        // `desktopGateway:true` to EVERY locally-trusted desktop conversation, so
        // it is a capability entitlement — NOT a routing signal. Treating it as
        // routing made 智能决策 inert on every desktop conversation. A conversation
        // carrying only desktopGateway (no channel/companion marker, no
        // channel_chat_id) must NOT be considered routed.
        assert!(!extra_marks_routed_conversation(r#"{"desktopGateway":true}"#));
        assert!(!extra_marks_routed_conversation(r#"{"desktop_gateway":true}"#));
        assert!(!conversation_is_routed(r#"{"desktopGateway":true}"#, None));
        assert!(!conversation_is_routed(r#"{"desktopGateway":true,"workspace":"/p"}"#, None));
    }

    #[test]
    fn conversation_is_routed_combines_extra_and_channel_chat_id() {
        // Genuine routing comes from the channel/companion extra markers…
        assert!(conversation_is_routed(r#"{"channelPlatform":"telegram"}"#, None));
        assert!(conversation_is_routed(r#"{"companionSession":true}"#, None));
        // …OR, for an ACP channel session that carries only desktopGateway in
        // extra, from the row-level channel_chat_id (set for EVERY channel
        // session, including ACP/Discord ones with no companion extra marker).
        assert!(conversation_is_routed(
            r#"{"desktopGateway":true,"backend":"claude"}"#,
            Some("im_chat_42")
        ));
        // A blank channel_chat_id does not count.
        assert!(!conversation_is_routed("{}", Some("   ")));
        assert!(!conversation_is_routed("{}", None));
    }

    fn decision_text() -> &'static str {
        "1) Canvas 渲染\n2) DOM + CSS\n请回复编号告诉我你的选择。"
    }

    #[test]
    fn finish_signal_user_cancel_stop_reason_wins() {
        assert_eq!(
            finish_signal(Some(TurnStopReason::Cancelled), decision_text(), true, false),
            SessionSignal::Cancelled
        );
    }

    #[test]
    fn finish_signal_cancel_since_work_wins_over_decision() {
        // Backend that doesn't emit Finish(Cancelled): the cross-check stamp
        // must stand the supervisor down rather than auto-answer a decision in
        // a turn the user just stopped.
        assert_eq!(
            finish_signal(None, decision_text(), true, true),
            SessionSignal::Cancelled
        );
    }

    #[test]
    fn finish_signal_plain_desktop_decision_emits_decision() {
        match finish_signal(Some(TurnStopReason::EndTurn), decision_text(), true, false) {
            SessionSignal::Decision(dp) => {
                assert_eq!(dp.source, DecisionSource::TextScan);
                assert_eq!(dp.options.len(), 2);
            }
            other => panic!("expected a TextScan decision, got {other:?}"),
        }
    }

    #[test]
    fn finish_signal_routed_conversation_decision_is_done() {
        // Same decision text, but NOT a plain desktop conversation → no
        // hijack; it stays a clean Done so the channel/companion human answers.
        assert_eq!(
            finish_signal(Some(TurnStopReason::EndTurn), decision_text(), false, false),
            SessionSignal::Done
        );
    }

    #[test]
    fn finish_signal_plain_desktop_non_decision_is_done() {
        assert_eq!(
            finish_signal(None, "好的，已经实现完成。", true, false),
            SessionSignal::Done
        );
    }

    #[test]
    fn finish_signal_plain_desktop_open_question_emits_open_question_decision() {
        // D6: an interrogative turn-end with no options is an OpenQuestion
        // Decision (only the model tier answers it), not a clean Done.
        let text = "我已经看过代码。你希望这个缓存的过期策略怎么设计？";
        match finish_signal(Some(TurnStopReason::EndTurn), text, true, false) {
            SessionSignal::Decision(dp) => {
                assert_eq!(dp.kind, DecisionKind::OpenQuestion);
                assert!(dp.options.is_empty());
            }
            other => panic!("expected an OpenQuestion decision, got {other:?}"),
        }
    }

    #[test]
    fn finish_signal_routed_conversation_open_question_is_done() {
        // Same open question, but a routed (channel/companion) conversation →
        // no auto-answer; stays Done so the remote human replies.
        let text = "我已经看过代码。你希望这个缓存的过期策略怎么设计？";
        assert_eq!(
            finish_signal(Some(TurnStopReason::EndTurn), text, false, false),
            SessionSignal::Done
        );
    }

    #[test]
    fn map_lifecycle_event_maps_kinds_to_signals() {
        use nomifun_terminal::LifecycleKind;
        // TurnEnd is now resolved by `terminal_turn_end_signal` (it scans
        // scrollback for a pending open question), so `map_lifecycle_event` is
        // only consulted for the OTHER kinds; it no longer carries TurnEnd.
        assert_eq!(map_lifecycle_event(LifecycleKind::ToolUse), Some(SessionSignal::Working));
        assert_eq!(map_lifecycle_event(LifecycleKind::SessionStart), Some(SessionSignal::Working));
        assert_eq!(map_lifecycle_event(LifecycleKind::Notification), Some(SessionSignal::Idle));
    }

    // ── Terminal TurnEnd open-question detection (the byte-scan owns Options) ──

    /// Feed a cleaned scrollback string into a fresh detector so the turn-end /
    /// pending-question helpers can run over `detector.scrollback(..)`. The
    /// detector strips ANSI itself; the strings here are already de-chromed
    /// CLEANED logical lines, so feeding them with a trailing newline produces
    /// the same lines back out of the scanner.
    fn detector_with_scrollback(text: &str) -> TerminalDetector {
        let mut d = TerminalDetector::new();
        let mut buf = text.to_string();
        if !buf.ends_with('\n') {
            buf.push('\n');
        }
        d.feed(buf.as_bytes());
        d
    }

    /// A realistic claude-TUI cleaned tail: the assistant ended its turn on an
    /// open-ended question, followed by status/recap chrome and a bare prompt.
    fn claude_open_question_tail() -> &'static str {
        "● 我们后面聊到外观时，可以给桌宠加一个呼吸动画，要不要试试？（需要你打开一个本地 URL）\n\
         ✻ Brewed for 1m 8s\n\
         ※ recap: 已经完成了基础布局\n\
         ❯ "
    }

    #[test]
    fn terminal_turn_end_open_question_emits_decision() {
        // The agent ended its turn with an open question, then TUI chrome. The
        // turn-end resolver must scan the de-chromed tail, find the question, and
        // emit an OpenQuestion Decision (so the model tier can answer it) — NOT
        // a swallowed Done.
        let d = detector_with_scrollback(claude_open_question_tail());
        match terminal_turn_end_signal(&d) {
            SessionSignal::Decision(dp) => {
                assert_eq!(dp.kind, DecisionKind::OpenQuestion);
                assert_eq!(dp.source, DecisionSource::TerminalScan);
                assert!(dp.options.is_empty(), "an open question carries no options");
                assert!(
                    dp.text.contains("要不要试试"),
                    "dp.text should be the question line, not the chrome; got {:?}",
                    dp.text
                );
            }
            other => panic!("expected an OpenQuestion Decision, got {other:?}"),
        }
    }

    #[test]
    fn terminal_turn_end_clean_finish_is_done() {
        // The agent just reported completion with no interrogative — the turn
        // genuinely ended, so the resolver falls back to Done.
        let d = detector_with_scrollback(
            "● 我已经把缓存层实现完成，并跑通了测试。\n\
             ✻ Brewed for 42s\n\
             ❯ ",
        );
        assert_eq!(terminal_turn_end_signal(&d), SessionSignal::Done);
    }

    #[test]
    fn terminal_turn_end_numbered_menu_not_open_question() {
        // A numbered menu / inline (1/2) token is a discrete-options decision the
        // byte-scan (detect_decision) owns — the turn-end open-question path must
        // NOT mis-emit it as an OpenQuestion (avoids double-answer). It falls back
        // to Done here (the byte-scan already emitted the Options Decision live).
        let d = detector_with_scrollback(
            "● 我准备了两套渲染方案：\n\
             1) Canvas 渲染：性能好\n\
             2) DOM + CSS：开发快\n\
             请回复编号告诉我你的选择。\n\
             ❯ ",
        );
        assert_eq!(
            terminal_turn_end_signal(&d),
            SessionSignal::Done,
            "a numbered menu is owned by the byte-scan Options path, not turn-end open-question"
        );

        // Also for an inline (1/2) token paired with a select word.
        let d2 = detector_with_scrollback(
            "● 请选择构建方式 (1/2)。\n\
             ❯ ",
        );
        assert_eq!(terminal_turn_end_signal(&d2), SessionSignal::Done);
    }

    #[test]
    fn terminal_pending_open_question_strips_trailing_chrome() {
        // Trailing box-drawing / status / bare-prompt chrome lines are stripped
        // so the question ABOVE them is the one found. A heavy claude-style box
        // input frame trails the question here.
        let tail = "● 你希望这个导出功能支持哪些文件格式？\n\
                     ╭──────────────────────────────────────╮\n\
                     │ >                                    │\n\
                     ╰──────────────────────────────────────╯\n\
                     ❯ ";
        let dp = terminal_pending_open_question(tail).expect("a pending open question");
        assert_eq!(dp.kind, DecisionKind::OpenQuestion);
        assert_eq!(dp.source, DecisionSource::TerminalScan);
        assert!(
            dp.text.contains("文件格式"),
            "the question above the chrome must be the text; got {:?}",
            dp.text
        );
    }

    #[test]
    fn terminal_pending_open_question_clean_finish_is_none() {
        // No interrogative anywhere in the de-chromed region → None.
        let tail = "● 我已经把缓存层实现完成，并跑通了测试。\n\
                     ✻ Brewed for 42s\n\
                     ❯ ";
        assert!(terminal_pending_open_question(tail).is_none());
    }

    #[test]
    fn terminal_oq_ignores_buried_question_url() {
        // Trailing-line gate: the `?` lives only INSIDE a URL query string in a
        // mid-line, and the LAST content line is a plain statement ("…已完成。").
        // Before the gate this fired (the region contained a `?`), wasting a
        // bypass-model sidecar call; now it must return None — the turn did NOT
        // end on a question.
        let tail = "● 打开 http://x?token=abc 看看，已完成。\n\
                     ✻ Brewed for 12s\n\
                     ❯ ";
        assert!(
            terminal_pending_open_question(tail).is_none(),
            "a `?` buried inside a URL above a final statement is not a turn-end question"
        );
    }

    #[test]
    fn terminal_oq_ignores_midturn_rhetorical() {
        // Trailing-line gate: an EARLIER line carries a `?` (a rhetorical mid-turn
        // line) but the LAST content line is a plain statement — not ending ON a
        // question → None. (The final statement is a plain prose line with no
        // status glyph, so de-chroming keeps it as the trailing content line
        // rather than stripping it and exposing the rhetorical question above.)
        let tail = "● 这个缓存策略真的合理吗？\n\
                     我重新检查后，已经按 LRU 实现完成并跑通了测试。\n\
                     ✻ Brewed for 30s\n\
                     ❯ ";
        assert!(
            terminal_pending_open_question(tail).is_none(),
            "an earlier rhetorical `?` above a final statement is not a turn-end question"
        );
    }

    #[test]
    fn terminal_dedupe_skips_repeated_decision_text() {
        // The observe dedupe guard: the same Decision text must not be emitted
        // twice in a row (byte-scan-then-TurnEnd for the same prompt, or repeated
        // TurnEnds for an unanswered prompt). A new turn (Working/Done) clears it.
        let mut last: Option<String> = None;
        let dp = |t: &str| DecisionPrompt {
            text: t.to_string(),
            options: vec![],
            recommended: None,
            source: DecisionSource::TerminalScan,
            kind: DecisionKind::OpenQuestion,
            permission: None,
        };
        // First emission of a prompt passes the guard.
        assert!(dedupe_should_send(&SessionSignal::Decision(dp("要不要试试？")), &mut last));
        // The identical prompt right after is suppressed.
        assert!(!dedupe_should_send(&SessionSignal::Decision(dp("要不要试试？")), &mut last));
        // A new-turn signal clears the memory.
        assert!(dedupe_should_send(&SessionSignal::Working, &mut last));
        // …so the same prompt may fire again on the next turn.
        assert!(dedupe_should_send(&SessionSignal::Decision(dp("要不要试试？")), &mut last));
        // A DIFFERENT prompt text is not suppressed.
        assert!(dedupe_should_send(&SessionSignal::Decision(dp("另一个问题？")), &mut last));
        // Non-decision signals always pass and do not themselves dedupe.
        assert!(dedupe_should_send(&SessionSignal::Done, &mut last));
        assert!(dedupe_should_send(&SessionSignal::Idle, &mut last));
    }

    // ── FakeDriver for observe() integration tests ──

    use nomifun_terminal::{TerminalLifecycleEvent, LifecycleKind};
    use nomifun_terminal::TerminalDriver as TerminalDriverTrait;
    use nomifun_terminal::error::TerminalError as TermError;

    struct FakeDriver {
        out_tx: tokio::sync::broadcast::Sender<Vec<u8>>,
        life_tx: Option<tokio::sync::broadcast::Sender<TerminalLifecycleEvent>>,
    }

    impl FakeDriver {
        fn new(with_lifecycle: bool) -> Self {
            let (out_tx, _) = tokio::sync::broadcast::channel(64);
            let life_tx = if with_lifecycle {
                let (tx, _) = tokio::sync::broadcast::channel(64);
                Some(tx)
            } else {
                None
            };
            Self { out_tx, life_tx }
        }
    }

    #[async_trait::async_trait]
    impl TerminalDriverTrait for FakeDriver {
        async fn write_input(&self, _id: i64, _bytes: &[u8]) -> Result<(), TermError> {
            unimplemented!()
        }
        fn subscribe_output(&self, _id: i64) -> Option<tokio::sync::broadcast::Receiver<Vec<u8>>> {
            Some(self.out_tx.subscribe())
        }
        fn is_alive(&self, _id: i64) -> bool {
            true
        }
        async fn describe(&self, _id: i64) -> Result<Option<nomifun_terminal::TerminalDescription>, TermError> {
            unimplemented!()
        }
        async fn read_autowork(&self, _id: i64) -> Result<Option<String>, TermError> {
            unimplemented!()
        }
        async fn write_autowork(&self, _id: i64, _autowork: Option<&str>) -> Result<(), TermError> {
            unimplemented!()
        }
        async fn read_idmm(&self, _id: i64) -> Result<Option<String>, TermError> {
            unimplemented!()
        }
        async fn write_idmm(&self, _id: i64, _idmm: Option<&str>) -> Result<(), TermError> {
            unimplemented!()
        }
        fn subscribe_lifecycle(&self, _id: i64) -> Option<tokio::sync::broadcast::Receiver<TerminalLifecycleEvent>> {
            self.life_tx.as_ref().map(|tx| tx.subscribe())
        }
    }

    #[tokio::test]
    async fn observe_maps_lifecycle_turn_end_to_done() {
        let driver = Arc::new(FakeDriver::new(true));
        let probe = TerminalProbe::new(driver.clone(), 1);
        let mut rx = probe.observe(Duration::from_secs(60));
        // Let the spawned task subscribe before we push.
        tokio::time::sleep(Duration::from_millis(50)).await;
        driver.life_tx.as_ref().unwrap().send(TerminalLifecycleEvent {
            terminal_id: 1,
            kind: LifecycleKind::TurnEnd,
            payload: serde_json::Value::Null,
        }).unwrap();
        let sig = tokio::time::timeout(Duration::from_secs(2), rx.recv())
            .await
            .expect("timed out")
            .expect("channel closed");
        assert_eq!(sig, SessionSignal::Done);
    }

    #[tokio::test]
    async fn observe_emits_decision_from_output_bytes() {
        let driver = Arc::new(FakeDriver::new(true));
        let probe = TerminalProbe::new(driver.clone(), 1);
        let mut rx = probe.observe(Duration::from_secs(60));
        tokio::time::sleep(Duration::from_millis(50)).await;
        // A line ending in "(y/n)" triggers detect_decision in TerminalDetector.
        driver.out_tx.send(b"Do you want to proceed? (y/n)\n".to_vec()).unwrap();
        let sig = tokio::time::timeout(Duration::from_secs(2), rx.recv())
            .await
            .expect("timed out")
            .expect("channel closed");
        assert!(matches!(sig, SessionSignal::Decision(_)), "expected Decision, got {sig:?}");
    }

    #[tokio::test]
    async fn observe_maps_notification_to_idle() {
        let driver = Arc::new(FakeDriver::new(true));
        let probe = TerminalProbe::new(driver.clone(), 1);
        let mut rx = probe.observe(Duration::from_secs(60));
        tokio::time::sleep(Duration::from_millis(50)).await;
        driver.life_tx.as_ref().unwrap().send(TerminalLifecycleEvent {
            terminal_id: 1,
            kind: LifecycleKind::Notification,
            payload: serde_json::Value::Null,
        }).unwrap();
        let sig = tokio::time::timeout(Duration::from_secs(2), rx.recv())
            .await
            .expect("timed out")
            .expect("channel closed");
        assert_eq!(sig, SessionSignal::Idle);
    }

    #[tokio::test]
    async fn observe_without_lifecycle_still_scans_output() {
        // lifecycle=None: no panic, content channel still works.
        let driver = Arc::new(FakeDriver::new(false));
        let probe = TerminalProbe::new(driver.clone(), 1);
        let mut rx = probe.observe(Duration::from_secs(60));
        tokio::time::sleep(Duration::from_millis(50)).await;
        driver.out_tx.send(b"Do you want to proceed? (y/n)\n".to_vec()).unwrap();
        let sig = tokio::time::timeout(Duration::from_secs(2), rx.recv())
            .await
            .expect("timed out")
            .expect("channel closed");
        assert!(matches!(sig, SessionSignal::Decision(_)), "expected Decision, got {sig:?}");
    }

    #[tokio::test]
    async fn observe_turn_end_open_question_emits_decision() {
        // The end-to-end terminal fix: the agent streams an open question, ends
        // its turn (TurnEnd lifecycle), and observe emits an OpenQuestion
        // Decision (scanned from scrollback) instead of swallowing it as Done.
        let driver = Arc::new(FakeDriver::new(true));
        let probe = TerminalProbe::new(driver.clone(), 1);
        let mut rx = probe.observe(Duration::from_secs(60));
        tokio::time::sleep(Duration::from_millis(50)).await;
        // The assistant's open question + chrome lands in scrollback first.
        driver
            .out_tx
            .send(claude_open_question_tail().as_bytes().to_vec())
            .unwrap();
        driver.out_tx.send(b"\n".to_vec()).unwrap();
        // Let the observe task drain the output chunks into the detector before
        // the turn-end fires (the select! could otherwise resolve TurnEnd first,
        // racing the scrollback the resolver scans).
        tokio::time::sleep(Duration::from_millis(100)).await;
        // Then the turn ends.
        driver
            .life_tx
            .as_ref()
            .unwrap()
            .send(TerminalLifecycleEvent {
                terminal_id: 1,
                kind: LifecycleKind::TurnEnd,
                payload: serde_json::Value::Null,
            })
            .unwrap();
        // Drain until we see the OpenQuestion Decision (output bytes alone don't
        // emit it; only the TurnEnd resolver does).
        let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
        loop {
            let sig = tokio::time::timeout_at(deadline, rx.recv())
                .await
                .expect("timed out waiting for the open-question decision")
                .expect("channel closed");
            if let SessionSignal::Decision(dp) = sig {
                assert_eq!(dp.kind, DecisionKind::OpenQuestion);
                assert!(dp.text.contains("要不要试试"), "got {:?}", dp.text);
                break;
            }
        }
    }

    #[tokio::test]
    async fn observe_turn_end_clean_finish_is_done() {
        // A clean (non-interrogative) turn-end still maps to Done.
        let driver = Arc::new(FakeDriver::new(true));
        let probe = TerminalProbe::new(driver.clone(), 1);
        let mut rx = probe.observe(Duration::from_secs(60));
        tokio::time::sleep(Duration::from_millis(50)).await;
        driver
            .out_tx
            .send("● 我已经把缓存层实现完成，并跑通了测试。\n".as_bytes().to_vec())
            .unwrap();
        driver
            .life_tx
            .as_ref()
            .unwrap()
            .send(TerminalLifecycleEvent {
                terminal_id: 1,
                kind: LifecycleKind::TurnEnd,
                payload: serde_json::Value::Null,
            })
            .unwrap();
        let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
        // No Decision should arrive; the turn-end signal is Done.
        let mut saw_done = false;
        while let Ok(Some(sig)) = tokio::time::timeout_at(deadline, rx.recv()).await {
            match sig {
                SessionSignal::Decision(dp) => panic!("clean finish must not be a Decision: {dp:?}"),
                SessionSignal::Done => {
                    saw_done = true;
                    break;
                }
                _ => {}
            }
        }
        assert!(saw_done, "expected a Done from the clean turn-end");
    }

    // ── D7: terminals self-manage their model → Failover degrades to Retry ──

    /// A driver that records the bytes written by `inject`, so a test can assert
    /// what a `WakeAction` was encoded to. Only `write_input`/`subscribe_*` matter.
    struct CapturingDriver {
        written: Arc<std::sync::Mutex<Vec<Vec<u8>>>>,
        backend: Option<String>,
    }

    #[async_trait::async_trait]
    impl TerminalDriverTrait for CapturingDriver {
        async fn write_input(&self, _id: i64, bytes: &[u8]) -> Result<(), TermError> {
            self.written.lock().unwrap().push(bytes.to_vec());
            Ok(())
        }
        fn subscribe_output(&self, _id: i64) -> Option<tokio::sync::broadcast::Receiver<Vec<u8>>> {
            None
        }
        fn is_alive(&self, _id: i64) -> bool {
            true
        }
        async fn describe(&self, _id: i64) -> Result<Option<nomifun_terminal::TerminalDescription>, TermError> {
            Ok(Some(nomifun_terminal::TerminalDescription {
                user_id: "u1".into(),
                cwd: ".".into(),
                command: "$SHELL".into(),
                args: vec![],
                backend: self.backend.clone(),
                mode: None,
                last_status: "running".into(),
            }))
        }
        async fn read_autowork(&self, _id: i64) -> Result<Option<String>, TermError> {
            unimplemented!()
        }
        async fn write_autowork(&self, _id: i64, _autowork: Option<&str>) -> Result<(), TermError> {
            unimplemented!()
        }
        async fn read_idmm(&self, _id: i64) -> Result<Option<String>, TermError> {
            unimplemented!()
        }
        async fn write_idmm(&self, _id: i64, _idmm: Option<&str>) -> Result<(), TermError> {
            unimplemented!()
        }
        fn subscribe_lifecycle(&self, _id: i64) -> Option<tokio::sync::broadcast::Receiver<TerminalLifecycleEvent>> {
            None
        }
    }

    #[tokio::test]
    async fn terminal_failover_degrades_to_continue_like_retry() {
        // The terminal probe does NOT support the model failover queue (D7): a
        // terminal/ACP CLI self-manages its model. A Failover must degrade to the
        // same "continue" nudge a Retry produces — never error, never a no-op.
        let written = Arc::new(std::sync::Mutex::new(Vec::new()));
        let driver = Arc::new(CapturingDriver {
            written: written.clone(),
            backend: None,
        });
        let probe = TerminalProbe::new(driver.clone(), 7);

        probe.inject(&WakeAction::Failover).await.expect("failover inject ok");
        probe.inject(&WakeAction::Retry).await.expect("retry inject ok");

        let w = written.lock().unwrap();
        assert_eq!(w.len(), 2, "both injects must write to the PTY");
        assert_eq!(w[0], w[1], "Failover must encode to the same bytes as Retry on a terminal");
        // "continue" is single-line → the shared encoder keeps it raw + CR, one write.
        assert_eq!(w[0], b"continue\r".to_vec(), "degrades to the continue nudge");
    }

    #[tokio::test]
    async fn terminal_inject_backend_agent_multiline_uses_paste_then_cr() {
        let written = Arc::new(std::sync::Mutex::new(Vec::new()));
        let driver = Arc::new(CapturingDriver {
            written: written.clone(),
            backend: Some("claude".into()),
        });
        let probe = TerminalProbe::new(driver.clone(), 7);

        probe
            .inject(&WakeAction::AnswerChoice("line one\nline two".into()))
            .await
            .expect("multiline answer inject ok");

        let w = written.lock().unwrap();
        assert_eq!(w.len(), 2, "agent multiline injection must split paste and CR");
        assert!(w[0].starts_with(b"\x1b[200~"));
        assert!(w[0].ends_with(b"\x1b[201~"));
        assert!(
            w[0].windows(b"line one\nline two".len()).any(|x| x == b"line one\nline two"),
            "paste body should contain the multiline answer"
        );
        assert_eq!(w[1], b"\r".to_vec(), "submit CR must be its own write");
    }

    #[tokio::test]
    async fn terminal_pending_signal_finds_open_question_in_scrollback() {
        // On arm: the terminal is ALREADY stuck at an open question. The
        // supervisor calls pending_signal once; it must scan the live scrollback
        // and surface the OpenQuestion Decision (so the model tier answers it),
        // mirroring the conversation probe's on-arm replay.
        let driver = Arc::new(FakeDriver::new(true));
        let probe = TerminalProbe::new(driver.clone(), 1);
        // Run observe so the scrollback Arc gets populated from the detector.
        let _rx = probe.observe(Duration::from_secs(60));
        tokio::time::sleep(Duration::from_millis(50)).await;
        driver
            .out_tx
            .send(claude_open_question_tail().as_bytes().to_vec())
            .unwrap();
        driver.out_tx.send(b"\n".to_vec()).unwrap();
        // Give the observe task time to refresh scrollback from the chunk.
        tokio::time::sleep(Duration::from_millis(100)).await;
        match probe.pending_signal().await {
            Some(SessionSignal::Decision(dp)) => {
                assert_eq!(dp.kind, DecisionKind::OpenQuestion);
                assert!(dp.text.contains("要不要试试"), "got {:?}", dp.text);
            }
            other => panic!("expected an OpenQuestion Decision on arm, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn terminal_pending_signal_clean_scrollback_is_none() {
        // No pending question in scrollback → None (nothing to answer on arm).
        let driver = Arc::new(FakeDriver::new(true));
        let probe = TerminalProbe::new(driver.clone(), 1);
        let _rx = probe.observe(Duration::from_secs(60));
        tokio::time::sleep(Duration::from_millis(50)).await;
        driver
            .out_tx
            .send("● 我已经把缓存层实现完成，并跑通了测试。\n".as_bytes().to_vec())
            .unwrap();
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert_eq!(probe.pending_signal().await, None);
    }

    // ── On-arm CURRENT pending decision (the "armed after the agent already
    //    asked" replay bug): the pure scan over the last persisted turn ──

    /// Build a persisted assistant/user `MessageRow` for the pending-signal scan.
    /// `position`: "left" = assistant, "right" = user/idmm reply. `text` is wrapped
    /// in the `{"content": …}` JSON the content column carries. Status defaults to
    /// the cleanly-finished "finish"; `msg_row_status` overrides it for the
    /// terminal-status gate tests.
    fn msg_row(position: &str, hidden: bool, r#type: &str, text: &str) -> nomifun_db::models::MessageRow {
        msg_row_status(position, hidden, r#type, text, Some("finish"))
    }

    /// Like `msg_row`, but with an explicit `status` (e.g. "work" for a still-
    /// streaming assistant turn). `created_at` defaults to 0; `msg_row_at`
    /// overrides it for the cancel-timestamp cross-check tests.
    fn msg_row_status(
        position: &str,
        hidden: bool,
        r#type: &str,
        text: &str,
        status: Option<&str>,
    ) -> nomifun_db::models::MessageRow {
        msg_row_at(position, hidden, r#type, text, status, 0)
    }

    /// Fully-specified row builder (adds an explicit `created_at`).
    fn msg_row_at(
        position: &str,
        hidden: bool,
        r#type: &str,
        text: &str,
        status: Option<&str>,
        created_at: i64,
    ) -> nomifun_db::models::MessageRow {
        nomifun_db::models::MessageRow {
            id: format!("m_{position}_{}", text.len()),
            conversation_id: 1,
            msg_id: None,
            r#type: r#type.to_string(),
            content: serde_json::json!({ "content": text }).to_string(),
            position: Some(position.to_string()),
            status: status.map(str::to_string),
            hidden,
            created_at,
        }
    }

    fn pending_decision_text() -> &'static str {
        "1) Canvas 渲染\n2) DOM + CSS\n请回复编号告诉我你的选择。"
    }

    #[test]
    fn pending_signal_last_assistant_options_menu_is_decision() {
        // The most-recent non-hidden text message is the assistant ("left") and
        // ends on a numbered-option menu → an on-arm Decision (plain desktop).
        // The page is newest-first (Desc), so the assistant menu is index 0.
        let msgs = vec![
            msg_row("left", false, "text", pending_decision_text()),
            msg_row("right", false, "text", "帮我选个渲染方案"),
        ];
        match pending_signal_from_page(r#"{"workspace":"/p"}"#, &msgs) {
            Some((SessionSignal::Decision(dp), _at)) => {
                assert_eq!(dp.source, DecisionSource::TextScan);
                assert_eq!(dp.options.len(), 2);
            }
            other => panic!("expected an options Decision, got {other:?}"),
        }
    }

    #[test]
    fn pending_signal_multi_question_design_prompt_is_open_question() {
        // REGRESSION (会话 27「中途开启智能决策不生效、完全没有决策记录」): the agent ended
        // its turn on a multi-part design questionnaire — several NUMBERED TOPICS,
        // some with bullet sub-options, closing on "请告诉我你的偏好…。". It is not a
        // pick-one menu (no 回复编号/选择 intent), so it must surface as an on-arm
        // OpenQuestion (the model tier answers it) — NOT fall through to `None`,
        // which left IDMM silent. `desktopGateway:true` mirrors the real conv-27
        // extra (a plain desktop conversation, not routed).
        let multi_q = "好的！先问你几个基础设计问题：\n\n\
                       1. **技术栈偏好**：你想用什么来写？\n   - 推荐：HTML5 + JS\n   - 或 Python\n\n\
                       2. **界面风格**：\n   - 复古像素风\n   - 现代简约风\n\n\
                       3. **核心规则**：撞墙死，还是穿墙继续？\n\n\
                       请告诉我你的偏好，我们一个一个敲定，然后我再开始写代码。";
        let msgs = vec![msg_row("left", false, "text", multi_q)];
        match pending_signal_from_page(r#"{"desktopGateway":true,"workspace":"/p"}"#, &msgs) {
            Some((SessionSignal::Decision(dp), _at)) => {
                assert_eq!(dp.kind, DecisionKind::OpenQuestion, "a multi-question prompt is an open question");
                assert!(dp.options.is_empty(), "an open question carries no enumerable options");
            }
            other => panic!("expected an OpenQuestion Decision, got {other:?}"),
        }
    }

    #[test]
    fn pending_signal_plain_desktop_with_gateway_flag_is_decision() {
        // REGRESSION (智能决策完全不可用): the capability-bus super-gateway grants
        // `desktopGateway:true` to EVERY locally-trusted desktop conversation, so
        // it can no longer mark a conversation as "routed to a remote human". A
        // plain desktop chat that ends on a numbered menu — and now always carries
        // `desktopGateway` — MUST still surface its on-arm pending decision, or the
        // decision watch never intervenes on any desktop conversation.
        let msgs = vec![msg_row("left", false, "text", pending_decision_text())];
        match pending_signal_from_page(r#"{"desktopGateway":true,"workspace":"/p"}"#, &msgs) {
            Some((SessionSignal::Decision(dp), _at)) => {
                assert_eq!(dp.source, DecisionSource::TextScan);
                assert_eq!(dp.options.len(), 2);
            }
            other => panic!("expected an options Decision for a plain desktop conversation, got {other:?}"),
        }
    }

    #[test]
    fn pending_signal_last_message_right_is_none_idempotent() {
        // IDEMPOTENCY: the last speaker is a (visible) user reply ("right") — the
        // assistant is NOT currently waiting. Newest-first (Desc): the reply is
        // index 0. (The hidden-idmm-reply variant is covered separately.)
        let msgs = vec![
            msg_row("right", false, "text", "我选 1) Canvas 渲染"),
            msg_row("left", false, "text", pending_decision_text()),
        ];
        assert_eq!(pending_signal_from_page(r#"{"workspace":"/p"}"#, &msgs), None);
    }

    #[test]
    fn pending_signal_routed_conversation_is_none() {
        // A routed (channel/companion/desktop-gateway) conversation must NOT be
        // auto-answered — the menu is its human-in-the-loop wire contract.
        let msgs = vec![msg_row("left", false, "text", pending_decision_text())];
        assert_eq!(pending_signal_from_page(r#"{"channelPlatform":"telegram"}"#, &msgs), None);
    }

    #[test]
    fn pending_signal_last_assistant_no_decision_is_none() {
        // A plain-desktop assistant turn with no decision / no open question is
        // not a pending decision.
        let msgs = vec![msg_row("left", false, "text", "好的，已经实现完成。")];
        assert_eq!(pending_signal_from_page(r#"{"workspace":"/p"}"#, &msgs), None);
    }

    #[test]
    fn pending_signal_skips_non_text_to_find_assistant_menu() {
        // Non-text rows (tool_call/tips) are skipped to find the most-recent
        // text turn. Newest-first (Desc): a trailing tool_call precedes the
        // visible assistant menu, which the scan must still find.
        let msgs = vec![
            msg_row("left", false, "tool_call", "{\"name\":\"read\"}"),
            msg_row("left", false, "text", pending_decision_text()),
        ];
        match pending_signal_from_page(r#"{"workspace":"/p"}"#, &msgs) {
            Some((SessionSignal::Decision(dp), _at)) => assert_eq!(dp.options.len(), 2),
            other => panic!("expected an options Decision, got {other:?}"),
        }
    }

    #[test]
    fn pending_signal_streaming_status_is_none_terminal_status_required() {
        // FIX #2: a position:"left" decision-menu turn that is still STREAMING
        // (status "work") is NOT a stable pending decision — mirror the live
        // path, which only fires on a cleanly-finished turn. The same row with
        // status "finish" IS a Decision.
        let streaming = vec![msg_row_status("left", false, "text", pending_decision_text(), Some("work"))];
        assert_eq!(
            pending_signal_from_page(r#"{"workspace":"/p"}"#, &streaming),
            None,
            "a mid-stream (status work) assistant turn must not be a pending decision"
        );
        // Also None for "pending" / None / any other non-terminal status.
        let pending = vec![msg_row_status("left", false, "text", pending_decision_text(), Some("pending"))];
        assert_eq!(pending_signal_from_page(r#"{"workspace":"/p"}"#, &pending), None);
        let no_status = vec![msg_row_status("left", false, "text", pending_decision_text(), None)];
        assert_eq!(pending_signal_from_page(r#"{"workspace":"/p"}"#, &no_status), None);

        // The SAME menu cleanly finished IS a pending decision.
        let finished = vec![msg_row_status("left", false, "text", pending_decision_text(), Some("finish"))];
        match pending_signal_from_page(r#"{"workspace":"/p"}"#, &finished) {
            Some((SessionSignal::Decision(dp), _at)) => assert_eq!(dp.options.len(), 2),
            other => panic!("expected an options Decision for a finished turn, got {other:?}"),
        }
    }

    #[test]
    fn pending_signal_surfaces_candidate_created_at() {
        // FIX #4 plumbing: the helper surfaces the candidate decision row's
        // created_at so the caller can run the user-cancel cross-check against
        // it. Newest-first: the assistant menu (created_at 4242) is index 0.
        let msgs = vec![msg_row_at("left", false, "text", pending_decision_text(), Some("finish"), 4242)];
        match pending_signal_from_page(r#"{"workspace":"/p"}"#, &msgs) {
            Some((SessionSignal::Decision(_), at)) => assert_eq!(at, 4242, "must surface the row's created_at"),
            other => panic!("expected a Decision carrying created_at, got {other:?}"),
        }
    }

    #[test]
    fn pending_signal_hidden_idmm_reply_blocks_refire() {
        // After IDMM answered, its injected reply persists as
        // position:"right" hidden:true and is the LATEST text — the last-speaker
        // check spans hidden rows, so a re-arm's scan returns None (no re-fire),
        // even though the assistant decision menu is still in the page.
        let msgs = vec![
            msg_row("right", true, "text", "1) Canvas 渲染"),
            msg_row("left", false, "text", pending_decision_text()),
        ];
        assert_eq!(pending_signal_from_page(r#"{"workspace":"/p"}"#, &msgs), None);
    }

    // ── A stub conversation repo proves `pending_signal` reads get()+get_messages
    //    and feeds the pure scan; non-numeric / missing id → None. ──

    struct StubConvRepo {
        row: Option<nomifun_db::models::ConversationRow>,
        messages: Vec<nomifun_db::models::MessageRow>,
    }

    #[async_trait]
    impl IConversationRepository for StubConvRepo {
        async fn get(&self, _id: i64) -> Result<Option<nomifun_db::models::ConversationRow>, nomifun_db::DbError> {
            Ok(self.row.clone())
        }
        async fn create(&self, _row: &nomifun_db::models::ConversationRow) -> Result<i64, nomifun_db::DbError> {
            unimplemented!()
        }
        async fn update(
            &self,
            _id: i64,
            _updates: &nomifun_db::ConversationRowUpdate,
        ) -> Result<(), nomifun_db::DbError> {
            unimplemented!()
        }
        async fn delete(&self, _id: i64) -> Result<(), nomifun_db::DbError> {
            unimplemented!()
        }
        async fn list_paginated(
            &self,
            _user_id: &str,
            _filters: &nomifun_db::ConversationFilters,
        ) -> Result<nomifun_common::PaginatedResult<nomifun_db::models::ConversationRow>, nomifun_db::DbError> {
            unimplemented!()
        }
        async fn find_by_source_and_chat(
            &self,
            _user_id: &str,
            _source: &str,
            _chat_id: &str,
            _agent_type: &str,
        ) -> Result<Option<nomifun_db::models::ConversationRow>, nomifun_db::DbError> {
            unimplemented!()
        }
        async fn list_by_cron_job(
            &self,
            _user_id: &str,
            _cron_job_id: &str,
        ) -> Result<Vec<nomifun_db::models::ConversationRow>, nomifun_db::DbError> {
            unimplemented!()
        }
        async fn list_associated(
            &self,
            _user_id: &str,
            _conversation_id: i64,
        ) -> Result<Vec<nomifun_db::models::ConversationRow>, nomifun_db::DbError> {
            unimplemented!()
        }
        async fn get_messages(
            &self,
            _conv_id: i64,
            _page: u32,
            _page_size: u32,
            _order: SortOrder,
        ) -> Result<nomifun_common::PaginatedResult<nomifun_db::models::MessageRow>, nomifun_db::DbError> {
            // Mirror the repo contract: newest-first (Desc) page.
            Ok(nomifun_common::PaginatedResult {
                items: self.messages.clone(),
                total: self.messages.len() as u64,
                has_more: false,
            })
        }
        async fn insert_message(&self, _message: &nomifun_db::models::MessageRow) -> Result<(), nomifun_db::DbError> {
            unimplemented!()
        }
        async fn update_message(
            &self,
            _id: &str,
            _updates: &nomifun_db::MessageRowUpdate,
        ) -> Result<(), nomifun_db::DbError> {
            unimplemented!()
        }
        async fn delete_messages_by_conversation(&self, _conv_id: i64) -> Result<(), nomifun_db::DbError> {
            unimplemented!()
        }
        async fn get_message_by_msg_id(
            &self,
            _conv_id: i64,
            _msg_id: &str,
            _msg_type: &str,
        ) -> Result<Option<nomifun_db::models::MessageRow>, nomifun_db::DbError> {
            unimplemented!()
        }
        async fn search_messages(
            &self,
            _user_id: &str,
            _keyword: &str,
            _page: u32,
            _page_size: u32,
        ) -> Result<nomifun_common::PaginatedResult<nomifun_db::MessageSearchRow>, nomifun_db::DbError> {
            unimplemented!()
        }
    }

    fn conv_row(extra: &str) -> nomifun_db::models::ConversationRow {
        nomifun_db::models::ConversationRow {
            id: 1,
            user_id: "u".into(),
            name: "c".into(),
            r#type: "nomi".into(),
            extra: extra.into(),
            model: None,
            status: Some("running".into()),
            source: None,
            channel_chat_id: None,
            pinned: false,
            pinned_at: None,
            cron_job_id: None,
            created_at: 0,
            updated_at: 0,
        }
    }

    async fn pending_signal_with_repo(conversation_id: &str, repo: Arc<StubConvRepo>) -> Option<SessionSignal> {
        pending_signal_with_repo_cancel(conversation_id, repo, None).await
    }

    /// Drive the exact I/O `ConversationProbe::pending_signal` performs (parse
    /// id → get() → get_messages() → pure scan → user-cancel cross-check)
    /// through the stub repo, without standing up a full ConversationService
    /// (whose `user_cancelled_since` is a pure timestamp compare we model here
    /// via `cancel_stamp_ms`: a cancel recorded at/after the candidate row's
    /// created_at stands the on-arm replay down, mirroring the live path).
    async fn pending_signal_with_repo_cancel(
        conversation_id: &str,
        repo: Arc<StubConvRepo>,
        cancel_stamp_ms: Option<i64>,
    ) -> Option<SessionSignal> {
        let Ok(id) = conversation_id.parse::<i64>() else {
            return None;
        };
        let Ok(Some(row)) = repo.get(id).await else {
            return None;
        };
        // Mirror `ConversationProbe::pending_signal`: routed conversations
        // (channel/companion extra markers, or any channel session via the
        // row-level channel_chat_id) are never auto-answered.
        if conversation_is_routed(&row.extra, row.channel_chat_id.as_deref()) {
            return None;
        }
        let page = repo.get_messages(id, 0, PENDING_SCAN_PAGE_SIZE, SortOrder::Desc).await.ok()?;
        let (sig, candidate_at) = pending_signal_from_page(&row.extra, &page.items)?;
        // Mirror `ConversationService::user_cancelled_since`: cancelled iff a
        // stamp exists at or after the candidate row's created_at.
        if cancel_stamp_ms.is_some_and(|stamped_at| stamped_at >= candidate_at) {
            return None;
        }
        Some(sig)
    }

    #[tokio::test]
    async fn pending_signal_non_numeric_id_is_none() {
        let repo = Arc::new(StubConvRepo {
            row: Some(conv_row(r#"{"workspace":"/p"}"#)),
            messages: vec![msg_row("left", false, "text", pending_decision_text())],
        });
        assert_eq!(pending_signal_with_repo("not-an-int", repo).await, None);
    }

    #[tokio::test]
    async fn pending_signal_through_repo_options_menu_is_decision() {
        let repo = Arc::new(StubConvRepo {
            row: Some(conv_row(r#"{"workspace":"/p"}"#)),
            messages: vec![msg_row("left", false, "text", pending_decision_text())],
        });
        match pending_signal_with_repo("1", repo).await {
            Some(SessionSignal::Decision(dp)) => assert_eq!(dp.options.len(), 2),
            other => panic!("expected an options Decision, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn pending_signal_through_repo_plain_desktop_with_gateway_is_decision() {
        // The real production state of every desktop conversation: extra carries
        // the super-gateway's `desktopGateway:true` and the row has NO
        // channel_chat_id. The on-arm pending decision must still be detected.
        let repo = Arc::new(StubConvRepo {
            row: Some(conv_row(r#"{"desktopGateway":true,"workspace":"/p"}"#)),
            messages: vec![msg_row("left", false, "text", pending_decision_text())],
        });
        match pending_signal_with_repo("1", repo).await {
            Some(SessionSignal::Decision(dp)) => assert_eq!(dp.options.len(), 2),
            other => panic!("expected an options Decision for a plain desktop conversation, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn pending_signal_acp_channel_session_is_none() {
        // An ACP channel session (e.g. claude bound to an IM channel) carries
        // only desktopGateway in extra but has a row-level channel_chat_id. Its
        // decisions route to the remote IM human via the channel relay, so IDMM
        // must NOT auto-answer them — closing the gap left by dropping
        // desktopGateway as a routing marker.
        let row = nomifun_db::models::ConversationRow {
            channel_chat_id: Some("im_chat_42".into()),
            ..conv_row(r#"{"desktopGateway":true,"backend":"claude"}"#)
        };
        let repo = Arc::new(StubConvRepo {
            row: Some(row),
            messages: vec![msg_row("left", false, "text", pending_decision_text())],
        });
        assert_eq!(pending_signal_with_repo("1", repo).await, None);
    }

    #[tokio::test]
    async fn pending_signal_cancelled_since_candidate_is_none() {
        // FIX #4: the user cancelled (stamp) at or after the candidate decision
        // row was written → the on-arm replay must stand down, not revive the
        // stopped turn. The candidate row's created_at is 100; a cancel at 100
        // (>=) suppresses it, while the same scan with no cancel fires.
        let repo = Arc::new(StubConvRepo {
            row: Some(conv_row(r#"{"workspace":"/p"}"#)),
            messages: vec![msg_row_at("left", false, "text", pending_decision_text(), Some("finish"), 100)],
        });
        // Cancelled at/after the row → None.
        assert_eq!(
            pending_signal_with_repo_cancel("1", repo.clone(), Some(100)).await,
            None,
            "a cancel at/after the candidate row must suppress the on-arm replay"
        );
        // A cancel strictly BEFORE the row (an older, unrelated stop) does not.
        match pending_signal_with_repo_cancel("1", repo.clone(), Some(99)).await {
            Some(SessionSignal::Decision(dp)) => assert_eq!(dp.options.len(), 2),
            other => panic!("a pre-candidate cancel must not suppress; got {other:?}"),
        }
        // No cancel at all → fires.
        match pending_signal_with_repo_cancel("1", repo, None).await {
            Some(SessionSignal::Decision(dp)) => assert_eq!(dp.options.len(), 2),
            other => panic!("expected an options Decision with no cancel; got {other:?}"),
        }
    }
}

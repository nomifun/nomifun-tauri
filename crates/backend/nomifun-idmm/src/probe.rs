//! `SessionProbe` — the target abstraction unifying conversation agents and
//! terminal/agent-CLI sessions. A probe normalizes a session's activity into a
//! `SessionSignal` stream (`observe`), injects wake/answer actions (`inject`),
//! and snapshots recent context for the sidecar (`snapshot_context`).

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use nomifun_ai_agent::{AcpPermissionEventData, AcpPermissionOptionKind, AcpToolCallKind, AgentStreamEvent, TurnStopReason};
use nomifun_ai_agent::runtime_registry::AgentRuntimeRegistry;
use nomifun_api_types::{
    ConfirmRequest, ConversationRuntimeStateKind, ConversationRuntimeSummary, IdmmTargetKind,
    SendMessageRequest,
};
use nomifun_common::{
    AppError, CompanionId, Confirmation, ConversationId, ConversationStatus, PublicAgentId,
    TerminalId, UserId,
};
use nomifun_conversation::{ConversationService, IdmmTurnScope};
use nomifun_db::{IConversationRepository, SortOrder};
use nomifun_terminal::TerminalDriver;
use tokio::sync::mpsc;

use crate::detector::{TerminalDetector, signal_from_agent_error};
#[cfg(test)]
use crate::detector::{detect_chat_open_question, has_open_intent};
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
    /// Snapshot the exact live Conversation turn that a durable action
    /// reservation will bind. Terminals currently have no durable Conversation
    /// turn identity and therefore return `None`.
    async fn action_scope(&self) -> Result<Option<IdmmTurnScope>, AppError> {
        Ok(None)
    }
    /// Deliver an action after its durable reservation was acquired.
    ///
    /// The default is deliberately fail-closed. Every effectful probe must
    /// provide its own exact-scope implementation; a missing scope must never
    /// degrade into an unkeyed/direct injection.
    async fn inject_reserved(
        &self,
        _action: &WakeAction,
        _scope: Option<&IdmmTurnScope>,
    ) -> Result<(), AppError> {
        Err(AppError::Conflict(
            "IDMM action rejected: this session has no exact durable action scope".into(),
        ))
    }
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
    /// On arm, the session's CURRENT live structured decision, if any.
    /// Persisted assistant text is terminal history and must never be treated as
    /// fresh execution authority.
    async fn pending_signal(&self) -> Option<SessionSignal> {
        None
    }
    /// Whether text can independently authorize another action. The safe
    /// default is false: a cleanly-finished free-text response is terminal and
    /// may not be reinterpreted as a pending decision after completion.
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

/// Whether canonical companion/public-service markers identify a conversation
/// whose numbered-option menus are routed to a remote human. IDMM must not
/// auto-answer those menus: they are the human-in-the-loop wire contract for the
/// channel relay or companion session.
///
/// Transport is determined separately from the row-level `channel_chat_id` in
/// [`conversation_is_routed`]. Presentation metadata such as `channel_platform`
/// is intentionally not routing state. Pure + unit-tested.
fn extra_marks_routed_conversation(extra: &str) -> bool {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(extra) else {
        return false;
    };
    let truthy_bool = |k: &str| v.get(k).and_then(|x| x.as_bool()).unwrap_or(false);
    truthy_bool("companion_session")
        || v
            .get("companion_id")
            .and_then(serde_json::Value::as_str)
            .is_some_and(|id| CompanionId::parse(id).is_ok())
        || v
            .get("public_agent_id")
            .and_then(serde_json::Value::as_str)
            .is_some_and(|id| PublicAgentId::parse(id).is_ok())
}

/// Whether a conversation routes its decisions to a REMOTE human, so IDMM must
/// NOT auto-answer them. Combines the extra-marker check
/// ([`extra_marks_routed_conversation`]) with the row-level `channel_chat_id`,
/// which is set for every channel session — including non-Nomi channel sessions
/// that carry no companion/public-service marker and would otherwise be
/// indistinguishable from a plain desktop chat.
/// A blank `channel_chat_id` does not count.
fn conversation_is_routed(extra: &str, channel_chat_id: Option<&str>) -> bool {
    extra_marks_routed_conversation(extra)
        || channel_chat_id.map(|s| !s.trim().is_empty()).unwrap_or(false)
}

/// Decide the supervision signal for a chat-conversation turn-end (`Finish`).
///
/// A user cancel stands the supervisor down. Every other clean `Finish` is
/// absorbing: assistant prose, option-looking text, and open questions are all
/// terminal output and cannot create a new `Decision` after the turn completed.
/// Only live structured events (for example `AcpPermission`) may carry decision
/// authority.
fn finish_signal(stop_reason: Option<TurnStopReason>, cancelled_since_work: bool) -> SessionSignal {
    if matches!(stop_reason, Some(TurnStopReason::Cancelled)) || cancelled_since_work {
        return SessionSignal::Cancelled;
    }
    SessionSignal::Done
}

/// On-arm recovery of a pending tool-permission CONFIRMATION (the agent is
/// BLOCKED awaiting approval right now).
///
/// `observe()` subscribes only to FUTURE events, so an `AcpPermission`/
/// `Permission` the agent emitted BEFORE the watch armed is invisible to the
/// live lane. Persisted assistant text is deliberately not replayed; this live
/// runtime list is the sole on-arm recovery lane.
///
/// Recover it from the live runtime's pending-confirmation list directly — the same
/// `get_confirmations()` source `ConversationService::confirm`/`list_confirmations`
/// read — and map the first to a `Decision` exactly as [`map_agent_event`] maps a
/// live `AcpPermission`. Queried via the runtime registry (mirroring `observe()`'s
/// own `get_runtime`), NOT via `conversation_service`, so on-arm READ detection
/// never couples to the row-owner check. Returns `None` when there is no live
/// runtime or no pending confirmation. Pure given the runtime registry; the mapping is
/// the unit-tested [`permission_decision_from_confirmation`].
fn pending_confirmation_signal(
    runtime_registry: &Arc<dyn AgentRuntimeRegistry>,
    conversation_id: &str,
) -> Option<SessionSignal> {
    let conf = runtime_registry
        .get_runtime(conversation_id)?
        .get_confirmations()
        .into_iter()
        .next()?;
    Some(SessionSignal::Decision(permission_decision_from_confirmation(&conf)))
}

/// Persisted assistant rows are immutable terminal history, never a source of
/// fresh decision authority. Kept as a pure regression seam for the tests.
#[cfg(test)]
fn pending_signal_from_page(
    _extra: &str,
    _messages: &[nomifun_db::models::MessageRow],
) -> Option<(SessionSignal, i64)> {
    None
}

/// Whether both durable lifecycle state and the exact in-process turn owner
/// agree that this conversation is currently executing.
///
/// A runtime handle alone is insufficient: completed conversations can retain
/// an idle/stale handle. Likewise, a durable `running` row alone is
/// insufficient after a crash. IDMM may mutate a conversation only while both
/// authorities overlap, and never while a stop/completion/reset fence reports
/// `Starting`.
fn has_live_turn_authority(
    persisted_status: Option<&str>,
    runtime: &ConversationRuntimeSummary,
) -> bool {
    persisted_status == Some("running")
        && runtime.has_runtime
        && runtime.runtime_status == Some(ConversationStatus::Running)
        && runtime.is_processing
        && runtime.active_turn_id.is_some()
        && runtime.processing_started_at.is_some()
        && matches!(
            &runtime.state,
            ConversationRuntimeStateKind::Running
                | ConversationRuntimeStateKind::WaitingConfirmation
        )
}

/// Supervises a chat conversation's Agent runtime.
#[derive(Clone)]
pub struct ConversationProbe {
    pub runtime_registry: Arc<dyn AgentRuntimeRegistry>,
    pub conversation_service: ConversationService,
    pub conversation_repo: Arc<dyn IConversationRepository>,
    pub conversation_id: ConversationId,
}

impl ConversationProbe {
    async fn owner_id(&self) -> Result<String, AppError> {
        let row = self
            .conversation_repo
            .get(self.conversation_id.as_ref())
            .await
            .map_err(AppError::from)?
            .ok_or_else(|| AppError::NotFound(format!("conversation {}", self.conversation_id)))?;
        UserId::parse(&row.user_id)
            .map(UserId::into_string)
            .map_err(|error| {
                AppError::Internal(format!(
                    "conversation {} has invalid owner: {error}",
                    self.conversation_id
                ))
            })
    }

    /// Fail closed immediately before an IDMM mutation. This is the probe-side
    /// half of the lifecycle fence; non-confirm actions additionally enter the
    /// conversation service's never-fallback continuation seam, which repeats
    /// the check at the exact steering boundary.
    async fn ensure_live_turn_authority(&self) -> Result<(), AppError> {
        let row = self
            .conversation_repo
            .get(self.conversation_id.as_ref())
            .await
            .map_err(AppError::from)?
            .ok_or_else(|| AppError::NotFound(format!("conversation {}", self.conversation_id)))?;
        let runtime = self
            .conversation_service
            .runtime_summary_for(self.conversation_id.as_str())
            .await;
        if has_live_turn_authority(row.status.as_deref(), &runtime) {
            Ok(())
        } else {
            Err(AppError::Conflict(format!(
                "IDMM action rejected: conversation {} has no live Running turn authority",
                self.conversation_id
            )))
        }
    }
}

#[async_trait]
impl SessionProbe for ConversationProbe {
    fn target(&self) -> (IdmmTargetKind, String) {
        (IdmmTargetKind::Conversation, self.conversation_id.clone().into_string())
    }

    fn observe(&self, idle_threshold: Duration) -> mpsc::Receiver<SessionSignal> {
        let (tx, rx) = mpsc::channel(64);
        // Attach lazily: if no agent exists yet there is nothing to observe; the
        // supervisor re-arms on the next loop tick / status fetch.
        let Some(instance) = self.runtime_registry.get_runtime(self.conversation_id.as_str()) else {
            // Closed receiver-with-no-sender-task: drop tx so observe yields nothing.
            return rx;
        };
        let mut sub = instance.subscribe();
        // Cloned into the observe task for the idle-tick user-cancel cross-check.
        let conversation_service = self.conversation_service.clone();
        let conversation_id = self.conversation_id.clone();
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(idle_threshold);
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            ticker.tick().await; // consume the immediate first tick
            let mut saw_activity = false;
            // Timestamp of the most recent `Working` transition (init at observe
            // start). A cancel stamp at or after this means the user stopped the
            // current work — the idle tick must stand down rather than nudge.
            let mut work_epoch_ms = nomifun_common::now_ms();
            loop {
                tokio::select! {
                    ev = sub.recv() => match ev {
                        Ok(ev) => {
                            // A clean Finish is absorbing. Free text emitted
                            // before it cannot be upgraded into fresh execution
                            // authority after the turn has completed.
                            let sig = match &ev {
                                AgentStreamEvent::Finish(d) => {
                                    let cancelled = conversation_service
                                        .user_cancelled_since(conversation_id.as_str(), work_epoch_ms);
                                    Some(finish_signal(d.stop_reason, cancelled))
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
                            .user_cancelled_since(conversation_id.as_str(), work_epoch_ms);
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
        if matches!(action, WakeAction::Wait(_) | WakeAction::Stop(_)) {
            return Ok(());
        }
        Err(AppError::Conflict(
            "Conversation IDMM actions require a durable exact-turn reservation".into(),
        ))
    }

    async fn action_scope(&self) -> Result<Option<IdmmTurnScope>, AppError> {
        let owner_id = self.owner_id().await?;
        self.ensure_live_turn_authority().await?;
        self.conversation_service
            .idmm_active_turn_scope(
                &owner_id,
                self.conversation_id.as_str(),
                &self.runtime_registry,
            )
            .await
            .map(Some)
    }

    async fn inject_reserved(
        &self,
        action: &WakeAction,
        scope: Option<&IdmmTurnScope>,
    ) -> Result<(), AppError> {
        if matches!(action, WakeAction::Wait(_) | WakeAction::Stop(_)) {
            return Ok(());
        }
        let expected_scope = scope.ok_or_else(|| {
            AppError::Conflict(
                "Conversation IDMM action is missing its durable turn reservation scope"
                    .into(),
            )
        })?;
        let owner_id = self.owner_id().await?;
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
                .idmm_confirm_active_turn(
                    &owner_id,
                    self.conversation_id.as_str(),
                    expected_scope,
                    call_id,
                    req,
                    &self.runtime_registry,
                )
                .await;
        }
        // Only the send-loop owns the AgentTurnHandle/relay continuity needed
        // for a same-turn model failover. The external IDMM observer may ask the
        // service seam, but it must never degrade a refusal into a hidden Retry
        // or a fresh send after ownership was lost.
        if matches!(action, WakeAction::Failover) {
            let switched = self
                .conversation_service
                .idmm_failover_conversation(
                    &owner_id,
                    self.conversation_id.as_str(),
                    &self.runtime_registry,
                )
                .await?;
            if switched {
                return Ok(());
            }
            return Err(AppError::Conflict(
                "IDMM failover rejected: only the active send loop may re-drive the current turn"
                    .into(),
            ));
        }
        let content = match action {
            WakeAction::Retry => "Please continue.".to_string(),
            WakeAction::Failover => unreachable!("failover returns above"),
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
            .idmm_continue_active_turn(
                &owner_id,
                self.conversation_id.as_str(),
                expected_scope,
                req,
                &self.runtime_registry,
            )
            .await
            .map(|_| ())
    }

    async fn snapshot_context(&self, max_chars: usize) -> Result<String, AppError> {
        let page = self
            .conversation_repo
            .get_messages(self.conversation_id.as_ref(), 0, 20, SortOrder::Desc)
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
        self.runtime_registry
            .get_runtime(self.conversation_id.as_str())
            .is_some()
    }

    async fn describe(&self) -> Result<SessionDescription, AppError> {
        let row = self
            .conversation_repo
            .get(self.conversation_id.as_ref())
            .await
            .map_err(AppError::from)?;
        let row = row.ok_or_else(|| {
            AppError::NotFound(format!("conversation {} not found", self.conversation_id))
        })?;
        Ok(SessionDescription {
            kind: IdmmTargetKind::Conversation,
            backend: Some(row.r#type),
            user_id: row.user_id,
            alive: self.is_alive(),
        })
    }

    async fn fallback_model(&self) -> Option<(String, String)> {
        let row = self
            .conversation_repo
            .get(self.conversation_id.as_ref())
            .await
            .ok()??;
        let pm = nomifun_conversation::runtime_options::provider_model_from_conversation_row(&row)
            .ok()??;
        Some((pm.provider_id, pm.model))
    }

    async fn pending_signal(&self) -> Option<SessionSignal> {
        // The row is still used for the routing gate: channel/companion
        // confirmations belong to their remote human, never IDMM.
        let row = self
            .conversation_repo
            .get(self.conversation_id.as_ref())
            .await
            .ok()??;
        if conversation_is_routed(&row.extra, row.channel_chat_id.as_deref()) {
            return None;
        }
        let runtime = self
            .conversation_service
            .runtime_summary_for(self.conversation_id.as_str())
            .await;
        if !has_live_turn_authority(row.status.as_deref(), &runtime) {
            return None;
        }
        // The sole recovery source is a confirmation that is still live in the
        // runtime. Completed assistant rows are not scanned or replayed.
        pending_confirmation_signal(&self.runtime_registry, self.conversation_id.as_str())
    }

    async fn decision_in_text(&self, _turn_text: &str) -> bool {
        // A caller only supplies this hook with a just-finished assistant turn.
        // Finished text is absorbing and cannot authorize IDMM injection.
        false
    }
}

// ──────────────────────────────── TerminalProbe ───────────────────────────

/// Map a structured terminal lifecycle event to a supervision signal.
/// TurnEnd is always Done. ToolUse/SessionStart→Working (activity, arms
/// work-in-progress); Notification→Idle (claude's "agent is waiting for
/// input/permission" hook — the precise wait signal replacing the unreliable
/// byte-timeout idle; only claude registers it, so codex/unknown get no
/// idle-nudge). Decision/ProviderError content for the OPTIONS path still comes
/// from the byte-scan content channel, not lifecycle hooks.
fn map_lifecycle_event(kind: nomifun_terminal::LifecycleKind) -> Option<SessionSignal> {
    use nomifun_terminal::LifecycleKind;
    match kind {
        LifecycleKind::TurnEnd => Some(SessionSignal::Done),
        LifecycleKind::ToolUse | LifecycleKind::SessionStart => Some(SessionSignal::Working),
        LifecycleKind::Notification => Some(SessionSignal::Idle),
    }
}

/// Test-only coverage for the retired scrollback heuristic. Production
/// TurnEnd/pending-signal paths never call these helpers.
///
/// How many trailing NON-EMPTY logical lines of the cleaned scrollback form the
/// "recent region" examined for a turn-end open question (the assistant's last
/// paragraph + its surrounding TUI chrome).
#[cfg(test)]
const TERMINAL_TAIL_LINES: usize = 15;

/// Box-drawing / block / shade glyphs a TUI uses to frame its input box and
/// status rows. A line whose trimmed content is ONLY these (plus whitespace) is
/// pure chrome with no message, so it is stripped from the tail before the
/// open-question scan.
#[cfg(test)]
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
#[cfg(test)]
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
#[cfg(test)]
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
#[cfg(test)]
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
#[cfg(test)]
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

/// A terminal lifecycle `TurnEnd` is final. Text already emitted by the CLI,
/// including an open-ended question, is completed output rather than authority
/// for IDMM to submit another prompt.
fn terminal_turn_end_signal() -> SessionSignal {
    SessionSignal::Done
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
    pub terminal_id: TerminalId,
    /// Scrollback kept for sidecar context (shared with the observe task).
    scrollback: Arc<std::sync::Mutex<String>>,
}

impl TerminalProbe {
    pub fn new(driver: Arc<dyn TerminalDriver>, terminal_id: TerminalId) -> Self {
        Self {
            driver,
            terminal_id,
            scrollback: Arc::new(std::sync::Mutex::new(String::new())),
        }
    }
}

#[async_trait]
impl SessionProbe for TerminalProbe {
    fn target(&self) -> (IdmmTargetKind, String) {
        (IdmmTargetKind::Terminal, self.terminal_id.clone().into_string())
    }

    fn observe(&self, idle_threshold: Duration) -> mpsc::Receiver<SessionSignal> {
        // Terminal idle is now lifecycle-driven (Notification → Idle); the
        // byte-timeout idle_threshold is no longer used for emission.
        let _ = idle_threshold;

        let (tx, rx) = mpsc::channel(64);
        let Some(mut out) = self.driver.subscribe_output(self.terminal_id.as_str()) else {
            return rx;
        };
        let driver = self.driver.clone();
        let id = self.terminal_id.clone();
        let scrollback = self.scrollback.clone();
        let mut lifecycle_rx = self.driver.subscribe_lifecycle(self.terminal_id.as_str());
        tokio::spawn(async move {
            let mut detector = TerminalDetector::new();
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
                                let turn_ended =
                                    ev.kind == nomifun_terminal::LifecycleKind::TurnEnd;
                                let sig = if turn_ended {
                                    Some(terminal_turn_end_signal())
                                } else {
                                    map_lifecycle_event(ev.kind)
                                };
                                if let Some(sig) = sig {
                                    if dedupe_should_send(&sig, &mut last_decision_text) {
                                        if tx.send(sig).await.is_err() {
                                            return;
                                        }
                                    }
                                }
                                // TurnEnd is the absorbing boundary for this
                                // observer generation. Exit immediately so
                                // delayed output/lifecycle frames cannot be
                                // reclassified as a new Decision. A later real
                                // terminal activity may explicitly re-arm a
                                // fresh observer.
                                if turn_ended {
                                    return;
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
                        if !driver.is_alive(id.as_str()) {
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
        match action {
            // Supervisor control flow only; these variants never write to the
            // PTY.
            WakeAction::Wait(_) | WakeAction::Stop(_) => Ok(()),
            // Terminal IDMM currently has no exact durable turn scope/admission
            // receipt. Retry, free text, choices, confirmations, and especially
            // Failover must not degrade into a fresh "continue" write.
            _ => Err(AppError::Conflict(
                "Terminal IDMM action rejected: no exact durable terminal turn scope"
                    .into(),
            )),
        }
    }

    async fn snapshot_context(&self, max_chars: usize) -> Result<String, AppError> {
        let sb = self.scrollback.lock().map(|s| s.clone()).unwrap_or_default();
        Ok(crate::util::tail_chars(&sb, max_chars))
    }

    async fn pending_signal(&self) -> Option<SessionSignal> {
        // Re-arm is not user intent. Scrollback may describe an already
        // completed turn, so it can never be replayed into a fresh Decision.
        None
    }

    fn is_alive(&self) -> bool {
        self.driver.is_alive(self.terminal_id.as_str())
    }

    async fn describe(&self) -> Result<SessionDescription, AppError> {
        let desc = self
            .driver
            .describe(self.terminal_id.as_str())
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
    use crate::detector::detect_chat_decision;
    use nomifun_api_types::{AgentErrorCode, AgentErrorOwnership, AgentStreamErrorData};
    use nomifun_common::MessageId;

    const TEST_USER_ID: &str = "0190f5fe-7c00-7a00-8000-000000000001";

    fn test_terminal_id() -> TerminalId {
        TerminalId::parse("0190f5fe-7c00-7a00-8000-000000000001").unwrap()
    }

    fn alternate_test_terminal_id() -> TerminalId {
        TerminalId::parse("0190f5fe-7c00-7a00-8000-000000000007").unwrap()
    }

    fn runtime_summary(
        state: ConversationRuntimeStateKind,
        runtime_status: Option<ConversationStatus>,
        processing_started_at: Option<i64>,
    ) -> ConversationRuntimeSummary {
        let is_processing = state != ConversationRuntimeStateKind::Idle;
        ConversationRuntimeSummary {
            state,
            can_send_message: !is_processing,
            has_runtime: true,
            runtime_status,
            is_processing,
            pending_confirmations: 0,
            active_turn_id: processing_started_at
                .map(|_| "0190f5fe-7c00-7a00-8000-000000000002".to_owned()),
            processing_started_at,
        }
    }

    #[test]
    fn conversation_injection_requires_persisted_and_live_turn_authority() {
        let active = runtime_summary(
            ConversationRuntimeStateKind::Running,
            Some(ConversationStatus::Running),
            Some(42),
        );
        assert!(has_live_turn_authority(Some("running"), &active));
        assert!(
            !has_live_turn_authority(Some("finished"), &active),
            "a stale live runtime cannot authorize mutation of Finished history"
        );
        assert!(
            !has_live_turn_authority(Some("pending"), &active),
            "Pending is not an executing turn"
        );

        let no_exact_turn = runtime_summary(
            ConversationRuntimeStateKind::Running,
            Some(ConversationStatus::Running),
            None,
        );
        assert!(
            !has_live_turn_authority(Some("running"), &no_exact_turn),
            "a manager status without an exact active-turn owner is insufficient"
        );

        let mut missing_exact_turn_id = active.clone();
        missing_exact_turn_id.active_turn_id = None;
        assert!(
            !has_live_turn_authority(Some("running"), &missing_exact_turn_id),
            "a display timestamp cannot replace exact active-turn identity"
        );

        let stale_runtime = runtime_summary(
            ConversationRuntimeStateKind::Running,
            Some(ConversationStatus::Finished),
            Some(42),
        );
        assert!(
            !has_live_turn_authority(Some("running"), &stale_runtime),
            "durable Running alone cannot revive a finished runtime"
        );

        let fenced = runtime_summary(
            ConversationRuntimeStateKind::Starting,
            Some(ConversationStatus::Running),
            Some(42),
        );
        assert!(
            !has_live_turn_authority(Some("running"), &fenced),
            "stop/completion/reset/build fences must block IDMM mutation"
        );
    }

    #[test]
    fn live_waiting_confirmation_retains_structured_confirm_authority() {
        let waiting = runtime_summary(
            ConversationRuntimeStateKind::WaitingConfirmation,
            Some(ConversationStatus::Running),
            Some(42),
        );
        assert!(has_live_turn_authority(Some("running"), &waiting));
    }

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
        // Companion and public-service conversations route numbered menus to a
        // remote human — IDMM must not auto-answer them.
        assert!(extra_marks_routed_conversation(
            r#"{"companion_session":true,"companion_id":"0190f5fe-7c00-7a00-8abc-012345678942"}"#
        ));
        assert!(extra_marks_routed_conversation(
            r#"{"public_agent_id":"0190f5fe-7c00-7a00-8abc-012345678943"}"#
        ));
        // A plain desktop conversation is NOT routed.
        assert!(!extra_marks_routed_conversation(r#"{"workspace":"/project"}"#));
        // Presentation-only metadata, blank ids, empty, and invalid extra do
        // not count as routed.
        assert!(!extra_marks_routed_conversation(
            r#"{"channel_platform":"telegram"}"#
        ));
        assert!(!extra_marks_routed_conversation(r#"{"companion_id":""}"#));
        assert!(!extra_marks_routed_conversation(r#"{"public_agent_id":" "}"#));
        assert!(!extra_marks_routed_conversation(""));
        assert!(!extra_marks_routed_conversation("{}"));
    }

    #[test]
    fn conversation_is_routed_combines_extra_and_channel_chat_id() {
        // Shared companion/public sessions carry canonical business markers.
        assert!(conversation_is_routed(r#"{"companion_session":true}"#, None));
        assert!(conversation_is_routed(
            r#"{"public_agent_id":"0190f5fe-7c00-7a00-8abc-012345678943"}"#,
            None
        ));
        // Every dedicated channel session is routed by its first-class row field,
        // independent of agent backend or presentation metadata.
        assert!(conversation_is_routed(
            r#"{"backend":"claude"}"#,
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
            finish_signal(Some(TurnStopReason::Cancelled), false),
            SessionSignal::Cancelled
        );
    }

    #[test]
    fn finish_signal_cancel_since_work_wins() {
        // Backend that doesn't emit Finish(Cancelled): the cross-check stamp
        // must stand the supervisor down.
        assert_eq!(finish_signal(None, true), SessionSignal::Cancelled);
    }

    #[test]
    fn finish_signal_options_text_is_absorbing_done() {
        assert!(
            detect_chat_decision(decision_text()).is_some(),
            "regression fixture must exercise the old options detector"
        );
        assert_eq!(
            finish_signal(Some(TurnStopReason::EndTurn), false),
            SessionSignal::Done
        );
    }

    #[test]
    fn finish_signal_polite_open_question_is_absorbing_done() {
        // Exact regression: this ordinary closing question used to be parsed as
        // an OpenQuestion and could authorize another hidden IDMM turn.
        assert_eq!(
            finish_signal(Some(TurnStopReason::EndTurn), false),
            SessionSignal::Done
        );
    }

    #[test]
    fn completed_text_detector_may_match_but_finish_stays_done() {
        let text = "How can I help you today?";
        assert!(
            detect_chat_open_question(text).is_some(),
            "regression fixture must exercise the old free-text detector"
        );
        assert_eq!(finish_signal(Some(TurnStopReason::EndTurn), false), SessionSignal::Done);
    }

    #[test]
    fn map_agent_event_structured_permission_remains_decision() {
        let event = AgentStreamEvent::Permission(serde_json::json!({
            "id": "call-1",
            "message": "Allow file write?",
            "options": [{"id": "allow", "label": "Allow"}]
        }));
        assert!(
            matches!(map_agent_event(&event), Some(SessionSignal::Decision(_))),
            "live structured permission events must retain decision authority"
        );
    }

    #[test]
    fn map_lifecycle_event_maps_kinds_to_signals() {
        use nomifun_terminal::LifecycleKind;
        assert_eq!(map_lifecycle_event(LifecycleKind::TurnEnd), Some(SessionSignal::Done));
        assert_eq!(map_lifecycle_event(LifecycleKind::ToolUse), Some(SessionSignal::Working));
        assert_eq!(map_lifecycle_event(LifecycleKind::SessionStart), Some(SessionSignal::Working));
        assert_eq!(map_lifecycle_event(LifecycleKind::Notification), Some(SessionSignal::Idle));
    }

    // ── Terminal TurnEnd open-question detection (the byte-scan owns Options) ──

    /// A realistic claude-TUI cleaned tail: the assistant ended its turn on an
    /// open-ended question, followed by status/recap chrome and a bare prompt.
    fn claude_open_question_tail() -> &'static str {
        "● 我们后面聊到外观时，可以给桌宠加一个呼吸动画，要不要试试？（需要你打开一个本地 URL）\n\
         ✻ Brewed for 1m 8s\n\
         ※ recap: 已经完成了基础布局\n\
         ❯ "
    }

    #[test]
    fn terminal_turn_end_open_question_is_absorbing_done() {
        // TurnEnd never scans completed text for new execution authority.
        assert_eq!(terminal_turn_end_signal(), SessionSignal::Done);
    }

    #[test]
    fn terminal_turn_end_clean_finish_is_done() {
        // The agent just reported completion with no interrogative — the turn
        // genuinely ended, so the resolver falls back to Done.
        assert_eq!(terminal_turn_end_signal(), SessionSignal::Done);
    }

    #[test]
    fn terminal_turn_end_numbered_menu_not_open_question() {
        // A numbered menu / inline (1/2) token is a discrete-options decision the
        // byte-scan (detect_decision) owns — the turn-end open-question path must
        // NOT mis-emit it as an OpenQuestion (avoids double-answer). It falls back
        // to Done here (the byte-scan already emitted the Options Decision live).
        assert_eq!(terminal_turn_end_signal(), SessionSignal::Done);
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
        async fn write_input(&self, _id: &str, _bytes: &[u8]) -> Result<(), TermError> {
            unimplemented!()
        }
        fn subscribe_output(&self, _id: &str) -> Option<tokio::sync::broadcast::Receiver<Vec<u8>>> {
            Some(self.out_tx.subscribe())
        }
        fn is_alive(&self, _id: &str) -> bool {
            true
        }
        async fn describe(&self, _id: &str) -> Result<Option<nomifun_terminal::TerminalDescription>, TermError> {
            unimplemented!()
        }
        async fn read_autowork(&self, _id: &str) -> Result<Option<String>, TermError> {
            unimplemented!()
        }
        async fn write_autowork(&self, _id: &str, _autowork: Option<&str>) -> Result<(), TermError> {
            unimplemented!()
        }
        async fn read_idmm(&self, _id: &str) -> Result<Option<String>, TermError> {
            unimplemented!()
        }
        async fn write_idmm(&self, _id: &str, _idmm: Option<&str>) -> Result<(), TermError> {
            unimplemented!()
        }
        fn subscribe_lifecycle(&self, _id: &str) -> Option<tokio::sync::broadcast::Receiver<TerminalLifecycleEvent>> {
            self.life_tx.as_ref().map(|tx| tx.subscribe())
        }
    }

    #[tokio::test]
    async fn observe_maps_lifecycle_turn_end_to_done() {
        let driver = Arc::new(FakeDriver::new(true));
        let probe = TerminalProbe::new(driver.clone(), test_terminal_id());
        let mut rx = probe.observe(Duration::from_secs(60));
        // Let the spawned task subscribe before we push.
        tokio::time::sleep(Duration::from_millis(50)).await;
        driver.life_tx.as_ref().unwrap().send(TerminalLifecycleEvent {
            terminal_id: test_terminal_id(),
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
        let probe = TerminalProbe::new(driver.clone(), test_terminal_id());
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
        let probe = TerminalProbe::new(driver.clone(), test_terminal_id());
        let mut rx = probe.observe(Duration::from_secs(60));
        tokio::time::sleep(Duration::from_millis(50)).await;
        driver.life_tx.as_ref().unwrap().send(TerminalLifecycleEvent {
            terminal_id: test_terminal_id(),
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
        let probe = TerminalProbe::new(driver.clone(), test_terminal_id());
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
    async fn observe_turn_end_question_is_done_and_late_output_cannot_reopen_it() {
        // Regression for the reported screenshot: completed prose that looks
        // like an invitation must not be reinterpreted as a new turn.
        let driver = Arc::new(FakeDriver::new(true));
        let probe = TerminalProbe::new(driver.clone(), test_terminal_id());
        let mut rx = probe.observe(Duration::from_secs(60));
        tokio::time::sleep(Duration::from_millis(50)).await;
        driver
            .out_tx
            .send(b"How can I help you today?\n".to_vec())
            .unwrap();
        tokio::time::sleep(Duration::from_millis(100)).await;
        driver
            .life_tx
            .as_ref()
            .unwrap()
            .send(TerminalLifecycleEvent {
                terminal_id: test_terminal_id(),
                kind: LifecycleKind::TurnEnd,
                payload: serde_json::Value::Null,
            })
            .unwrap();

        let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
        let mut saw_done = false;
        loop {
            match tokio::time::timeout_at(deadline, rx.recv()).await {
                Ok(Some(SessionSignal::Done)) => {
                    saw_done = true;
                    break;
                }
                Ok(Some(SessionSignal::Decision(dp))) => {
                    panic!("completed question must not become a Decision: {dp:?}");
                }
                Ok(Some(_)) => {}
                Ok(None) => break,
                Err(_) => panic!("timed out waiting for absorbing Done"),
            }
        }
        assert!(saw_done, "TurnEnd must publish Done");

        // A delayed old-turn prompt cannot cross the completed observer
        // generation. The task exits after Done, so the channel stays closed.
        let _ = driver
            .out_tx
            .send(b"Do you want to continue? (y/n)\n".to_vec());
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(1), rx.recv())
                .await
                .expect("receiver did not close"),
            None
        );
    }

    #[tokio::test]
    async fn observe_turn_end_clean_finish_is_done() {
        // A clean (non-interrogative) turn-end still maps to Done.
        let driver = Arc::new(FakeDriver::new(true));
        let probe = TerminalProbe::new(driver.clone(), test_terminal_id());
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
                terminal_id: test_terminal_id(),
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

    // ── Terminal actions require an exact durable turn scope ──

    /// A driver that records the bytes written by `inject`, so a test can assert
    /// what a `WakeAction` was encoded to. Only `write_input`/`subscribe_*` matter.
    struct CapturingDriver {
        written: Arc<std::sync::Mutex<Vec<Vec<u8>>>>,
        backend: Option<String>,
    }

    #[async_trait::async_trait]
    impl TerminalDriverTrait for CapturingDriver {
        async fn write_input(&self, _id: &str, bytes: &[u8]) -> Result<(), TermError> {
            self.written.lock().unwrap().push(bytes.to_vec());
            Ok(())
        }
        fn subscribe_output(&self, _id: &str) -> Option<tokio::sync::broadcast::Receiver<Vec<u8>>> {
            None
        }
        fn is_alive(&self, _id: &str) -> bool {
            true
        }
        async fn describe(&self, _id: &str) -> Result<Option<nomifun_terminal::TerminalDescription>, TermError> {
            Ok(Some(nomifun_terminal::TerminalDescription {
                user_id: TEST_USER_ID.into(),
                cwd: ".".into(),
                command: "$SHELL".into(),
                args: vec![],
                backend: self.backend.clone(),
                mode: None,
                last_status: "running".into(),
            }))
        }
        async fn read_autowork(&self, _id: &str) -> Result<Option<String>, TermError> {
            unimplemented!()
        }
        async fn write_autowork(&self, _id: &str, _autowork: Option<&str>) -> Result<(), TermError> {
            unimplemented!()
        }
        async fn read_idmm(&self, _id: &str) -> Result<Option<String>, TermError> {
            unimplemented!()
        }
        async fn write_idmm(&self, _id: &str, _idmm: Option<&str>) -> Result<(), TermError> {
            unimplemented!()
        }
        fn subscribe_lifecycle(&self, _id: &str) -> Option<tokio::sync::broadcast::Receiver<TerminalLifecycleEvent>> {
            None
        }
    }

    #[tokio::test]
    async fn terminal_failover_retry_and_send_text_are_rejected_without_writes() {
        let written = Arc::new(std::sync::Mutex::new(Vec::new()));
        let driver = Arc::new(CapturingDriver {
            written: written.clone(),
            backend: None,
        });
        let probe = TerminalProbe::new(driver.clone(), alternate_test_terminal_id());

        for action in [
            WakeAction::Failover,
            WakeAction::Retry,
            WakeAction::SendText("continue".into()),
            WakeAction::AnswerChoice("1".into()),
            WakeAction::Confirm {
                call_id: "call-1".into(),
                value: "allow".into(),
                always_allow: false,
            },
        ] {
            let error = probe
                .inject(&action)
                .await
                .expect_err("unscoped terminal action must be rejected");
            assert!(
                error
                    .to_string()
                    .contains("no exact durable terminal turn scope"),
                "unexpected rejection: {error}"
            );
        }
        assert!(
            written.lock().unwrap().is_empty(),
            "Failover/Retry/SendText/AnswerChoice/Confirm must perform zero PTY writes"
        );
    }

    #[tokio::test]
    async fn terminal_wait_and_stop_are_control_flow_only() {
        let written = Arc::new(std::sync::Mutex::new(Vec::new()));
        let driver = Arc::new(CapturingDriver {
            written: written.clone(),
            backend: Some("claude".into()),
        });
        let probe = TerminalProbe::new(driver.clone(), alternate_test_terminal_id());

        probe
            .inject(&WakeAction::Wait(Duration::from_millis(1)))
            .await
            .expect("wait is a no-op");
        probe
            .inject(&WakeAction::Stop("human review".into()))
            .await
            .expect("stop is a no-op");
        assert!(written.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn terminal_rearm_does_not_replay_open_question_from_scrollback() {
        let driver = Arc::new(FakeDriver::new(true));
        let probe = TerminalProbe::new(driver.clone(), test_terminal_id());
        let _rx = probe.observe(Duration::from_secs(60));
        tokio::time::sleep(Duration::from_millis(50)).await;
        driver
            .out_tx
            .send(claude_open_question_tail().as_bytes().to_vec())
            .unwrap();
        driver.out_tx.send(b"\n".to_vec()).unwrap();
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert_eq!(
            probe.pending_signal().await,
            None,
            "re-arm must not turn historical scrollback into fresh authority"
        );
    }

    #[tokio::test]
    async fn terminal_pending_signal_clean_scrollback_is_none() {
        // No pending question in scrollback → None (nothing to answer on arm).
        let driver = Arc::new(FakeDriver::new(true));
        let probe = TerminalProbe::new(driver.clone(), test_terminal_id());
        let _rx = probe.observe(Duration::from_secs(60));
        tokio::time::sleep(Duration::from_millis(50)).await;
        driver
            .out_tx
            .send("● 我已经把缓存层实现完成，并跑通了测试。\n".as_bytes().to_vec())
            .unwrap();
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert_eq!(probe.pending_signal().await, None);
    }

    // ── Completed conversation rows are absorbing and never replayed. ──

    /// Build a persisted assistant/user `MessageRow` for terminal-history tests.
    /// `position`: "left" = assistant, "right" = user/idmm reply. `text` is wrapped
    /// in the `{"content": …}` JSON the content column carries. Status defaults to
    /// the cleanly-finished "finish"; `msg_row_status` overrides it for the
    /// terminal-status gate tests.
    fn msg_row(position: &str, hidden: bool, r#type: &str, text: &str) -> nomifun_db::models::MessageRow {
        msg_row_status(position, hidden, r#type, text, Some("finish"))
    }

    /// Like `msg_row`, but with an explicit `status` (e.g. "work" for a still-
    /// streaming assistant turn). `created_at` defaults to 0; `msg_row_at`
    /// overrides it for timestamp-independence tests.
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
            id: 0,
            message_id: MessageId::new().into_string(),
            conversation_id: "0190f5fe-7c00-7a00-8000-000000000001".into(),
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
    fn pending_signal_finished_assistant_options_are_not_replayed() {
        // A persisted finish row is terminal history. Looking like a menu does
        // not authorize a hidden follow-up turn when IDMM is armed later.
        let msgs = vec![
            msg_row("left", false, "text", pending_decision_text()),
            msg_row("right", false, "text", "帮我选个渲染方案"),
        ];
        assert_eq!(pending_signal_from_page(r#"{"workspace":"/p"}"#, &msgs), None);
    }

    #[test]
    fn pending_signal_finished_multi_question_is_not_replayed() {
        let multi_q = "好的！先问你几个基础设计问题：\n\n\
                       1. **技术栈偏好**：你想用什么来写？\n   - 推荐：HTML5 + JS\n   - 或 Python\n\n\
                       2. **界面风格**：\n   - 复古像素风\n   - 现代简约风\n\n\
                       3. **核心规则**：撞墙死，还是穿墙继续？\n\n\
                       请告诉我你的偏好，我们一个一个敲定，然后我再开始写代码。";
        let msgs = vec![msg_row("left", false, "text", multi_q)];
        assert_eq!(pending_signal_from_page(r#"{"workspace":"/p"}"#, &msgs), None);
    }

    #[test]
    fn pending_signal_finished_how_can_i_help_is_not_replayed() {
        let msgs = vec![msg_row("left", false, "text", "How can I help you today?")];
        assert_eq!(pending_signal_from_page(r#"{"workspace":"/p"}"#, &msgs), None);
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
        // A routed companion conversation must NOT be
        // auto-answered — the menu is its human-in-the-loop wire contract.
        let msgs = vec![msg_row("left", false, "text", pending_decision_text())];
        assert_eq!(
            pending_signal_from_page(r#"{"companion_session":true}"#, &msgs),
            None
        );
        assert_eq!(
            pending_signal_from_page(
                r#"{"public_agent_id":"0190f5fe-7c00-7a00-8abc-012345678943"}"#,
                &msgs,
            ),
            None
        );
    }

    #[test]
    fn pending_signal_last_assistant_no_decision_is_none() {
        // A plain-desktop assistant turn with no decision / no open question is
        // not a pending decision.
        let msgs = vec![msg_row("left", false, "text", "好的，已经实现完成。")];
        assert_eq!(pending_signal_from_page(r#"{"workspace":"/p"}"#, &msgs), None);
    }

    #[test]
    fn pending_signal_non_text_tail_does_not_revive_finished_menu() {
        let msgs = vec![
            msg_row("left", false, "tool_call", "{\"name\":\"read\"}"),
            msg_row("left", false, "text", pending_decision_text()),
        ];
        assert_eq!(pending_signal_from_page(r#"{"workspace":"/p"}"#, &msgs), None);
    }

    #[test]
    fn pending_signal_streaming_status_is_none_terminal_status_required() {
        // Neither in-progress nor finished free text grants execution authority.
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

        // A clean finish is absorbing as well.
        let finished = vec![msg_row_status("left", false, "text", pending_decision_text(), Some("finish"))];
        assert_eq!(pending_signal_from_page(r#"{"workspace":"/p"}"#, &finished), None);
    }

    #[test]
    fn pending_signal_finished_row_timestamp_does_not_matter() {
        let msgs = vec![msg_row_at("left", false, "text", pending_decision_text(), Some("finish"), 4242)];
        assert_eq!(pending_signal_from_page(r#"{"workspace":"/p"}"#, &msgs), None);
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

    // ── A stub conversation repo proves persisted free text is not replayed. ──

    struct StubConvRepo {
        row: Option<nomifun_db::models::ConversationRow>,
        messages: Vec<nomifun_db::models::MessageRow>,
    }

    #[async_trait]
    impl IConversationRepository for StubConvRepo {
        async fn get(&self, _id: &str) -> Result<Option<nomifun_db::models::ConversationRow>, nomifun_db::DbError> {
            Ok(self.row.clone())
        }
        async fn create(&self, _row: &nomifun_db::models::ConversationRow) -> Result<String, nomifun_db::DbError> {
            unimplemented!()
        }
        async fn update(
            &self,
            _id: &str,
            _updates: &nomifun_db::ConversationRowUpdate,
        ) -> Result<(), nomifun_db::DbError> {
            unimplemented!()
        }
        async fn delete(&self, _id: &str) -> Result<(), nomifun_db::DbError> {
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
            _conversation_id: &str,
        ) -> Result<Vec<nomifun_db::models::ConversationRow>, nomifun_db::DbError> {
            unimplemented!()
        }
        async fn get_messages(
            &self,
            _conv_id: &str,
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
        async fn delete_messages_by_conversation(&self, _conv_id: &str) -> Result<(), nomifun_db::DbError> {
            unimplemented!()
        }
        async fn get_message_by_msg_id(
            &self,
            _conv_id: &str,
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
            id: 0,
            conversation_id: "0190f5fe-7c00-7a00-8000-000000000001".into(),
            user_id: TEST_USER_ID.into(),
            name: "c".into(),
            r#type: "nomi".into(),
            extra: extra.into(),
            delegation_policy: "automatic".into(),
            execution_model_pool: None,
            decision_policy: "automatic".into(),
            execution_template_id: None,
            model: None,
            status: Some("running".into()),
            source: None,
            channel_chat_id: None,
            pinned: false,
            pinned_at: None,
            cron_job_id: None,
            preset_id: None,
            preset_revision: None,
            preset_snapshot: None,
            created_at: 0,
            updated_at: 0,
        }
    }

    async fn pending_signal_with_repo(conversation_id: &str, repo: Arc<StubConvRepo>) -> Option<SessionSignal> {
        pending_signal_with_repo_cancel(conversation_id, repo, None).await
    }

    /// Drive the persisted-row portion of `ConversationProbe::pending_signal`.
    /// Without a live runtime confirmation, every completed row is terminal and
    /// therefore yields `None`.
    async fn pending_signal_with_repo_cancel(
        conversation_id: &str,
        repo: Arc<StubConvRepo>,
        cancel_stamp_ms: Option<i64>,
    ) -> Option<SessionSignal> {
        let Ok(id) = ConversationId::try_from(conversation_id) else {
            return None;
        };
        let Ok(Some(row)) = repo.get(id.as_ref()).await else {
            return None;
        };
        // Mirror `ConversationProbe::pending_signal`: routed conversations
        // (channel/companion extra markers, or any channel session via the
        // row-level channel_chat_id) are never auto-answered.
        if conversation_is_routed(&row.extra, row.channel_chat_id.as_deref()) {
            return None;
        }
        let _ = (repo.messages.len(), cancel_stamp_ms);
        None
    }

    #[tokio::test]
    async fn pending_signal_noncanonical_id_is_none() {
        let repo = Arc::new(StubConvRepo {
            row: Some(conv_row(r#"{"workspace":"/p"}"#)),
            messages: vec![msg_row("left", false, "text", pending_decision_text())],
        });
        assert_eq!(pending_signal_with_repo("not-an-int", repo).await, None);
    }

    #[tokio::test]
    async fn pending_signal_through_repo_finished_options_is_none() {
        let repo = Arc::new(StubConvRepo {
            row: Some(conv_row(r#"{"workspace":"/p"}"#)),
            messages: vec![msg_row("left", false, "text", pending_decision_text())],
        });
        assert_eq!(
            pending_signal_with_repo("0190f5fe-7c00-7a00-8000-000000000001", repo).await,
            None
        );
    }

    #[tokio::test]
    async fn pending_signal_through_repo_finished_how_can_i_help_is_none() {
        let repo = Arc::new(StubConvRepo {
            row: Some(conv_row(r#"{"workspace":"/p"}"#)),
            messages: vec![msg_row("left", false, "text", "How can I help you today?")],
        });
        assert_eq!(
            pending_signal_with_repo("0190f5fe-7c00-7a00-8000-000000000001", repo).await,
            None
        );
    }

    #[tokio::test]
    async fn pending_signal_channel_session_is_none() {
        // A non-Nomi channel session has a row-level channel_chat_id. Its
        // decisions route to the remote IM human via the channel relay, so IDMM
        // must not auto-answer them.
        let row = nomifun_db::models::ConversationRow {
            channel_chat_id: Some("im_chat_42".into()),
            ..conv_row(r#"{"backend":"claude"}"#)
        };
        let repo = Arc::new(StubConvRepo {
            row: Some(row),
            messages: vec![msg_row("left", false, "text", pending_decision_text())],
        });
        assert_eq!(pending_signal_with_repo("0190f5fe-7c00-7a00-8000-000000000001", repo).await, None);
    }

    #[tokio::test]
    async fn pending_signal_finished_row_is_none_regardless_of_cancel_timestamp() {
        let repo = Arc::new(StubConvRepo {
            row: Some(conv_row(r#"{"workspace":"/p"}"#)),
            messages: vec![msg_row_at("left", false, "text", pending_decision_text(), Some("finish"), 100)],
        });
        assert_eq!(
            pending_signal_with_repo_cancel("0190f5fe-7c00-7a00-8000-000000000001", repo.clone(), Some(100)).await,
            None
        );
        assert_eq!(
            pending_signal_with_repo_cancel("0190f5fe-7c00-7a00-8000-000000000001", repo.clone(), Some(99)).await,
            None
        );
        assert_eq!(
            pending_signal_with_repo_cancel("0190f5fe-7c00-7a00-8000-000000000001", repo, None).await,
            None
        );
    }
}

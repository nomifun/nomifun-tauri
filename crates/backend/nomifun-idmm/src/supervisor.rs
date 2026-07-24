//! The per-session supervisor loop + `IdmmManager` (lifecycle, live counters,
//! continuous memory, scheduler) + the `IdmmHandle` impl AutoWork calls.
//! The decision audit trail itself is persisted to the DB (`idmm_interventions`)
//! via the records repo; the supervisor keeps only live counters for `IdmmState`.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::time::{Duration, Instant};

use dashmap::DashMap;
use nomifun_api_types::{AutoWorkTargetKind, IdmmConfig, IdmmState, IdmmTargetKind, InterventionRecord};
use nomifun_common::{AppError, UserId, now_ms};
use nomifun_conversation::IdmmTurnScope;
use nomifun_db::{
    IIdmmInterventionRepository, IdmmActionReservationKey, IdmmActionReservationRow,
    IdmmActionReserveResult, IdmmActionSettleResult, IdmmActionSettlement,
    IdmmActionTurnIdentity, ReserveIdmmActionParams,
};
use nomifun_db::models::NewIdmmInterventionRow;
use sha2::{Digest, Sha256};
use tracing::{debug, info, warn};

use crate::events::IdmmEventEmitter;
use crate::policy::{PolicyState, PolicyStep, SidecarStep};
use crate::probe::SessionProbe;
use crate::sidecar::{OpenQuestionAsk, SidecarClient};
use crate::signal::{DecisionKind, SessionSignal, StallClass, WakeAction};

/// `detail`/`reason` are truncated to this many chars before persisting (the
/// row is an audit trail, not a transcript store — keeps a runaway model reply
/// from bloating the table).
const DETAIL_MAX_CHARS: usize = 2000;

/// Map IDMM's target kind to AutoWork's (for the IdmmHandle boundary).
fn from_autowork_kind(kind: AutoWorkTargetKind) -> IdmmTargetKind {
    match kind {
        AutoWorkTargetKind::Conversation => IdmmTargetKind::Conversation,
        AutoWorkTargetKind::Terminal => IdmmTargetKind::Terminal,
    }
}

/// Shared, observable state for one supervised target. Only the live counters
/// the `IdmmState` dot needs — the per-decision audit rows live in the DB
/// (`idmm_interventions`), read back via the records repo, not from here.
pub struct SupervisorShared {
    pub intervening: AtomicBool,
    pub count: AtomicU32,
    pub last_signal: std::sync::Mutex<Option<String>>,
    pub last_intervention_at: std::sync::Mutex<Option<i64>>,
}

impl Default for SupervisorShared {
    fn default() -> Self {
        Self {
            intervening: AtomicBool::new(false),
            count: AtomicU32::new(0),
            last_signal: std::sync::Mutex::new(None),
            last_intervention_at: std::sync::Mutex::new(None),
        }
    }
}

impl SupervisorShared {
    /// Bump the live counters surfaced in `IdmmState` (count + last-at). The
    /// record itself is persisted to the DB by the caller; this is not a store.
    fn record(&self, rec: &InterventionRecord) {
        self.count.fetch_add(1, Ordering::Relaxed);
        *self.last_intervention_at.lock().unwrap() = Some(rec.at);
    }
}

/// One supervised target's task handle.
struct SupervisorHandle {
    cancel: Arc<AtomicBool>,
    join: tokio::task::JoinHandle<()>,
    /// Monotonic id distinguishing this supervisor instance from a later
    /// re-arm on the same target, so a naturally-exiting loop's cleanup only
    /// removes its own entry (not a fresh one a concurrent `ensure` inserted).
    generation: u64,
    /// Exact Conversation turn generation supplied by the admission hook.
    /// `None` is used by Terminal/manual arms. A delayed hook may never replace
    /// a live handle for an equal or newer admitted generation.
    admitted_turn_generation: Option<u64>,
}

impl Drop for SupervisorHandle {
    fn drop(&mut self) {
        self.cancel.store(true, Ordering::SeqCst);
        self.join.abort();
    }
}

/// What the supervisor loop needs (probe is per-target; the rest are shared).
pub struct LoopDeps {
    pub sidecar: Arc<SidecarClient>,
    pub emitter: IdmmEventEmitter,
    /// Owns both the durable, fail-closed Conversation-action reservation and
    /// the human-readable intervention audit trail. A reservation/recovery/
    /// settlement error blocks injection. Only the post-settlement audit insert
    /// is best-effort, because it cannot authorize a second side effect.
    pub records: Arc<dyn IIdmmInterventionRepository>,
}

/// Exact action authority captured when a stall signal is observed.
///
/// Conversation signals and turns are separate asynchronous streams. A
/// supervisor can spend time in backoff or a sidecar call while the observed
/// turn finishes and a successor starts. Delayed work must therefore retain
/// the original turn identity; sampling a scope only when applying the action
/// would silently upgrade an old signal into authority over the successor.
#[derive(Clone, Debug)]
enum ObservedActionScope {
    Exact(IdmmTurnScope),
    Unavailable(String),
    Unsupported,
}

async fn capture_action_scope(
    probe: &Arc<dyn SessionProbe>,
    kind: IdmmTargetKind,
) -> ObservedActionScope {
    if kind == IdmmTargetKind::Terminal {
        // Terminal effects are currently disabled below because the supervisor
        // has no exact durable Terminal turn capability.
        return ObservedActionScope::Unsupported;
    }

    match probe.action_scope().await {
        Ok(Some(scope)) => ObservedActionScope::Exact(scope),
        Ok(None) => ObservedActionScope::Unavailable(
            "Conversation IDMM signal has no exact live turn scope".to_owned(),
        ),
        Err(error) => ObservedActionScope::Unavailable(format!(
            "Conversation IDMM signal scope capture failed: {error}"
        )),
    }
}

async fn revalidate_action_scope(
    probe: &Arc<dyn SessionProbe>,
    observed_scope: &ObservedActionScope,
) -> Result<(), String> {
    let ObservedActionScope::Exact(observed) = observed_scope else {
        return match observed_scope {
            ObservedActionScope::Unavailable(reason) => Err(reason.clone()),
            // Terminal effects are rejected independently at their delivery
            // boundary; supervision policy may still observe the PTY.
            ObservedActionScope::Unsupported => Ok(()),
            ObservedActionScope::Exact(_) => unreachable!(),
        };
    };

    match probe.action_scope().await {
        Ok(Some(current)) if current == *observed => Ok(()),
        Ok(_) => Err(
            "IDMM signal's exact turn scope is no longer current; delayed work was absorbed"
                .to_owned(),
        ),
        Err(error) => Err(format!(
            "IDMM signal's exact turn scope could not be revalidated: {error}"
        )),
    }
}

/// Run the supervision loop for one target until cancelled or the session exits.
/// Public + free-standing so it can be unit-tested with a mock probe + a sidecar
/// backed by a scripted completer.
pub async fn run_supervisor(
    probe: Arc<dyn SessionProbe>,
    cfg: IdmmConfig,
    deps: Arc<LoopDeps>,
    shared: Arc<SupervisorShared>,
    cancel: Arc<AtomicBool>,
) {
    run_supervisor_for_owner(probe, cfg, deps, shared, cancel, None, None).await;
}

/// Owner-bound supervisor entry used by `IdmmManager`. The public free-standing
/// wrapper above remains convenient for policy tests, while production always
/// supplies the owner resolved during arming and revalidates it here before any
/// probe observation, action, persistence, or realtime emission.
async fn run_supervisor_for_owner(
    probe: Arc<dyn SessionProbe>,
    cfg: IdmmConfig,
    deps: Arc<LoopDeps>,
    shared: Arc<SupervisorShared>,
    cancel: Arc<AtomicBool>,
    expected_owner_id: Option<String>,
    admitted_scope: Option<IdmmTurnScope>,
) {
    let (kind, target_id) = probe.target();
    // Resolve the persisted session owner once, before any realtime emission.
    // A target with no authoritative owner is not safe to supervise: publishing
    // to a guessed/default audience would expose private intervention state.
    let owner_id = match probe.describe().await {
        Ok(description) if UserId::parse(&description.user_id).is_ok() => description.user_id,
        Ok(_) => {
            warn!(target_id, ?kind, "IDMM target has no owner — supervision not started");
            return;
        }
        Err(error) => {
            warn!(target_id, ?kind, error = %error, "IDMM target owner resolution failed — supervision not started");
            return;
        }
    };
    if let Some(expected_owner_id) = expected_owner_id
        && owner_id != expected_owner_id
    {
        warn!(
            target_id,
            ?kind,
            expected_owner_id,
            actual_owner_id = owner_id,
            "IDMM target owner changed while arming — supervision not started"
        );
        return;
    }
    // Bind a Conversation supervisor to exactly one admitted turn. This scope
    // is immutable for the lifetime of the task: queued events do not carry
    // enough identity to sample safely after `recv`, because an old event may
    // wait in the channel until a successor turn is already current. The
    // ConversationService hook supplies the admission-time scope in production;
    // direct/manual arms capture once through the exact-authority boundary.
    let observed_scope = match admitted_scope {
        Some(scope) => ObservedActionScope::Exact(scope),
        None => capture_action_scope(&probe, kind).await,
    };
    if kind == IdmmTargetKind::Conversation
        && !matches!(observed_scope, ObservedActionScope::Exact(_))
    {
        debug!(
            target_id,
            "Conversation IDMM supervisor has no exact admitted turn scope; standing down"
        );
        return;
    }

    // The conversation idle ticker uses the decision watch's scan interval (idle
    // nudges are a decision-lane concern); fall back to the fault watch's when
    // the decision watch is off, then to a sane default.
    let interval_secs = if cfg.decision_watch.base.enabled {
        cfg.decision_watch.base.scan_interval_secs
    } else {
        cfg.fault_watch.base.scan_interval_secs
    };
    let idle = Duration::from_secs(interval_secs.max(1) as u64);
    let mut rx = probe.observe(idle);
    let mut policy = PolicyState::with_kind(cfg.clone(), kind);

    // On-arm recovery is restricted to CURRENT live decision authority: for a
    // conversation, a structured confirmation still present in the active
    // runtime; for a terminal, a prompt still visible in the live PTY. Finished
    // conversation text is never replayed. Evaluate that live decision once,
    // gated on the decision watch; resolving it clears the underlying pending
    // confirmation/prompt so a later re-arm cannot re-fire it.
    if cfg.decision_watch.base.enabled && !cancel.load(Ordering::SeqCst) {
        if let Some(sig) = probe.pending_signal().await {
            if let Err(reason) = revalidate_action_scope(&probe, &observed_scope).await {
                debug!(
                    target_id,
                    reason, "IDMM on-arm pending signal lost its exact turn scope"
                );
            } else {
                *shared.last_signal.lock().unwrap() = Some(signal_label(&sig));
                set_intervening(&shared, &deps, &owner_id, kind, &target_id, &cfg, true);
                let halted = handle_stall(
                    &probe,
                    &mut policy,
                    &deps,
                    &shared,
                    &owner_id,
                    kind,
                    &target_id,
                    &cfg,
                    &sig,
                    &observed_scope,
                )
                .await;
                set_intervening(&shared, &deps, &owner_id, kind, &target_id, &cfg, false);
                if halted {
                    warn!(
                        target_id,
                        "IDMM halted on the on-arm pending decision — standing down"
                    );
                    return;
                }
            }
        }
    }

    while !cancel.load(Ordering::SeqCst) {
        let Some(mut sig) = rx.recv().await else {
            break;
        };
        *shared.last_signal.lock().unwrap() = Some(signal_label(&sig));

        match &sig {
            SessionSignal::Working => {
                if kind == IdmmTargetKind::Conversation
                    && let Err(reason) =
                        revalidate_action_scope(&probe, &observed_scope).await
                {
                    debug!(
                        target_id,
                        reason,
                        "Conversation IDMM supervisor observed a different turn; standing down"
                    );
                    return;
                }
                policy.on_progress(&sig);
                set_intervening(&shared, &deps, &owner_id, kind, &target_id, &cfg, false);
                continue;
            }
            SessionSignal::Done => {
                policy.on_progress(&sig);
                set_intervening(&shared, &deps, &owner_id, kind, &target_id, &cfg, false);
                if kind == IdmmTargetKind::Conversation {
                    // A Conversation supervisor owns one turn generation only.
                    // The next explicit turn's hook creates a fresh instance.
                    return;
                }
                continue;
            }
            SessionSignal::Cancelled => {
                // The USER stopped this turn. Stand down: clear WIP and
                // suppress every stall until fresh Working shows a new turn —
                // "recovering" a deliberately-stopped session (hidden
                // "Please continue." injections) restarted work the user had
                // just paused.
                debug!(target_id, "IDMM user cancel — suppressing interventions until new work");
                policy.on_user_cancel();
                set_intervening(&shared, &deps, &owner_id, kind, &target_id, &cfg, false);
                if kind == IdmmTargetKind::Conversation {
                    return;
                }
                continue;
            }
            SessionSignal::Exited => {
                debug!(target_id, "IDMM target exited — supervisor standing down");
                break;
            }
            _ => {}
        }

        // Mid-turn-arm recovery: an Idle can mean the agent is blocked on a live
        // structured confirmation emitted before `observe()` subscribed (or a
        // terminal is still showing a live prompt). Re-check that current live
        // authority before using a generic nudge. A clean Done remains absorbing:
        // even a recovered signal is stopped by `peek_standby` until fresh
        // Working proves a new turn.
        if matches!(sig, SessionSignal::Idle)
            && cfg.decision_watch.base.enabled
            && let Some(recovered) = probe.pending_signal().await
        {
            sig = recovered;
        }

        // Completed/cancelled-turn guard: every trailing stall after Done or
        // Cancelled is benign. Stand by with no flag flicker, backoff, record,
        // or injection until a future live Working transition re-arms policy.
        if policy.peek_standby(&sig) {
            debug!(target_id, "IDMM standby (normal idle, no nudge)");
            continue;
        }

        // A stall. Sleep the backoff, then run the ladder.
        set_intervening(&shared, &deps, &owner_id, kind, &target_id, &cfg, true);
        let delay = policy.next_delay();
        if !delay.is_zero() {
            tokio::time::sleep(delay).await;
        }
        if cancel.load(Ordering::SeqCst) {
            break;
        }
        if let Err(reason) = revalidate_action_scope(&probe, &observed_scope).await {
            debug!(
                target_id,
                reason, "IDMM delayed signal lost its exact turn scope"
            );
            set_intervening(&shared, &deps, &owner_id, kind, &target_id, &cfg, false);
            if kind == IdmmTargetKind::Conversation {
                // Scope change is a generation boundary, not a transient
                // failure. Never keep an old task around to inspect successor
                // signals.
                return;
            }
            continue;
        }

        let halted = handle_stall(
            &probe,
            &mut policy,
            &deps,
            &shared,
            &owner_id,
            kind,
            &target_id,
            &cfg,
            &sig,
            &observed_scope,
        )
        .await;
        set_intervening(&shared, &deps, &owner_id, kind, &target_id, &cfg, false);
        if halted {
            // The policy decided this needs a human (retries/budget exhausted,
            // unanswerable decision). Halting must actually STOP supervision —
            // a halt that only logged kept the loop armed, and the sliding
            // budget window / reset counters resumed interventions later: the
            // unbounded "still running hours later" loop. The user re-enables
            // IDMM (or a config save re-arms it) when they want it back.
            warn!(target_id, "IDMM halted — supervision stands down until re-armed");
            break;
        }
    }
}

/// Run the ladder for a single stall signal. Returns `true` when the policy
/// halted (needs human) — the caller must stand down supervision.
#[allow(clippy::too_many_arguments)]
async fn handle_stall(
    probe: &Arc<dyn SessionProbe>,
    policy: &mut PolicyState,
    deps: &Arc<LoopDeps>,
    shared: &Arc<SupervisorShared>,
    owner_id: &str,
    kind: IdmmTargetKind,
    target_id: &str,
    cfg: &IdmmConfig,
    sig: &SessionSignal,
    observed_scope: &ObservedActionScope,
) -> bool {
    let now = Instant::now();
    match policy.on_stall(now, sig) {
        PolicyStep::Standby => {
            // Defensive: the supervisor short-circuits Standby BEFORE
            // calling handle_stall, but if a future code path reaches here,
            // treat it as a no-op (no intervention, no log entry).
            debug!(target_id, "IDMM standby (in handle_stall)");
            false
        }
        PolicyStep::Halt(reason) => {
            warn!(target_id, reason, "IDMM halting — needs human");
            emit_intervention(
                deps,
                shared,
                owner_id,
                kind,
                target_id,
                sig,
                "rule",
                "stop",
                "halted",
                Some(reason.clone()),
                EmitExtra::default(),
            )
            .await;
            true
        }
        PolicyStep::Rule(WakeAction::Wait(_)) => {
            // Deferred (min-interval) — do nothing this pass.
            debug!(target_id, "IDMM deferred (min interval)");
            false
        }
        PolicyStep::Rule(action) => {
            let application = apply_action(
                probe,
                deps,
                owner_id,
                kind,
                target_id,
                sig,
                &action,
                observed_scope,
            )
            .await;
            policy.record_for(now, sig);
            match application {
                ActionApplication::Absorbed => {
                    debug!(
                        target_id,
                        action = action.as_str(),
                        "Duplicate IDMM action reservation absorbed"
                    );
                }
                ActionApplication::Applied { intervention_id } => {
                    emit_intervention(
                        deps,
                        shared,
                        owner_id,
                        kind,
                        target_id,
                        sig,
                        "rule",
                        action.as_str(),
                        applied_outcome(&action),
                        None,
                        EmitExtra {
                            intervention_id,
                            detail: rule_detail(&action),
                            category: rule_category(sig),
                            ..Default::default()
                        },
                    )
                    .await;
                }
                ActionApplication::Failed {
                    intervention_id,
                    reason,
                } => {
                    warn!(
                        target_id,
                        action = action.as_str(),
                        reason,
                        "IDMM action failed closed"
                    );
                    emit_intervention(
                        deps,
                        shared,
                        owner_id,
                        kind,
                        target_id,
                        sig,
                        "rule",
                        action.as_str(),
                        "failed",
                        Some(reason),
                        EmitExtra {
                            intervention_id,
                            detail: rule_detail(&action),
                            category: rule_category(sig),
                            ..Default::default()
                        },
                    )
                    .await;
                }
                ActionApplication::Skipped { reason } => {
                    debug!(
                        target_id,
                        action = action.as_str(),
                        reason,
                        "IDMM action skipped without delivery"
                    );
                    emit_intervention(
                        deps,
                        shared,
                        owner_id,
                        kind,
                        target_id,
                        sig,
                        "rule",
                        action.as_str(),
                        "skipped",
                        Some(reason),
                        EmitExtra {
                            detail: rule_detail(&action),
                            category: rule_category(sig),
                            ..Default::default()
                        },
                    )
                    .await;
                }
            }
            false
        }
        PolicyStep::Sidecar { class, detail } => {
            run_sidecar(
                probe,
                policy,
                deps,
                shared,
                owner_id,
                kind,
                target_id,
                cfg,
                sig,
                observed_scope,
                class,
                &detail,
            )
            .await;
            policy.record_for(now, sig);
            false
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn run_sidecar(
    probe: &Arc<dyn SessionProbe>,
    policy: &mut PolicyState,
    deps: &Arc<LoopDeps>,
    shared: &Arc<SupervisorShared>,
    owner_id: &str,
    kind: IdmmTargetKind,
    target_id: &str,
    cfg: &IdmmConfig,
    sig: &SessionSignal,
    observed_scope: &ObservedActionScope,
    class: StallClass,
    detail: &str,
) {
    // Pick the active watch's base (bypass model + context budget) by lane: a
    // fault stall uses the fault watch; everything else the decision watch (D4).
    let is_fault = matches!(
        sig,
        SessionSignal::ProviderError { .. } | SessionSignal::AgentError { .. }
    );
    let base = if is_fault {
        &cfg.fault_watch.base
    } else {
        &cfg.decision_watch.base
    };
    // Always read recent context for the bypass model (the Phase-1 read_history
    // toggle is gone; the watch's scan scope / char cap govern the slice).
    let context = probe
        .snapshot_context(base.max_context_chars as usize)
        .await
        .unwrap_or_default();

    // The session's own model backs the bypass model when no dedicated backup is
    // configured (so "全托管" needs zero extra setup on a plain chat).
    let fallback = probe.fallback_model().await;
    // D6: an open question takes the free-text answer prompt; everything else the
    // option/permission/fault prompt.
    let open_question = match sig {
        SessionSignal::Decision(dp) if dp.kind == DecisionKind::OpenQuestion => Some(OpenQuestionAsk {
            question: &dp.text,
            max_answer_chars: cfg.decision_watch.strategy.categories.open_question.max_answer_chars,
        }),
        _ => None,
    };
    // The fault lane has no DecisionStrategy of its own (FaultWatchConfig carries
    // none — DecisionStrategy is the only strategy in the type system), so when a
    // fault escalates to its bypass model it deliberately reuses the decision
    // watch's strategy. A conscious documented choice, not an oversight; the
    // destructive veto still applies (see PolicyState::escalate_to_bypass).
    let outcome = deps
        .sidecar
        .decide(
            &base.bypass_model,
            &cfg.decision_watch.strategy,
            class,
            detail,
            &context,
            fallback,
            open_question,
        )
        .await;
    // The bypass provider/model the sidecar actually used (or attempted, on a
    // provider failure), taken from the decision outcome — never re-resolved
    // just for the audit row.
    let bypass_model = outcome
        .resolved
        .as_ref()
        .map(|(p, m)| if m.is_empty() { p.clone() } else { format!("{p}/{m}") });

    if outcome.provider_failed || outcome.decision.is_none() {
        // Conservative rule fallback. For a Decision this answers a safe option
        // / confirms a safe permission / stops — never injects "continue".
        let fb = PolicyState::conservative_fallback(sig);
        let reason = if outcome.provider_failed {
            "sidecar_provider_unavailable"
        } else {
            "sidecar_unparseable_response"
        };
        let application = apply_action(
            probe,
            deps,
            owner_id,
            kind,
            target_id,
            sig,
            &fb,
            observed_scope,
        )
        .await;
        emit_action_application(
            deps,
            shared,
            owner_id,
            kind,
            target_id,
            sig,
            "rule_fallback",
            &fb,
            // Why we fell back lives in `reason`; `outcome` stays canonical.
            Some(reason.to_string()),
            EmitExtra {
                detail: rule_detail(&fb),
                category: rule_category(sig),
                // The bypass attempt failed/was unparseable — record which model
                // was attempted, but no confidence.
                bypass_model: bypass_model.clone(),
                ..Default::default()
            },
            application,
        )
        .await;
        return;
    }

    let dec = outcome.decision.unwrap();
    match policy.on_sidecar(&dec) {
        SidecarStep::Apply(action) => {
            // A permission decision is resolved via confirm, so a model
            // answer_choice/send_text must be remapped to a structured Confirm.
            let action = finalize_action(sig, action);
            let application = apply_action(
                probe,
                deps,
                owner_id,
                kind,
                target_id,
                sig,
                &action,
                observed_scope,
            )
            .await;
            let reason = if dec.reason.is_empty() {
                None
            } else {
                Some(dec.reason.clone())
            };
            emit_action_application(
                deps,
                shared,
                owner_id,
                kind,
                target_id,
                sig,
                "sidecar",
                &action,
                reason,
                EmitExtra {
                    detail: rule_detail(&action),
                    category: rule_category(sig),
                    confidence: Some(dec.confidence),
                    bypass_model: bypass_model.clone(),
                    ..Default::default()
                },
                application,
            )
            .await;
        }
        SidecarStep::Halt(reason) => {
            warn!(target_id, reason, "IDMM sidecar decision halted");
            emit_intervention(
                deps,
                shared,
                owner_id,
                kind,
                target_id,
                sig,
                "sidecar",
                "stop",
                "halted",
                Some(reason.clone()),
                EmitExtra {
                    category: rule_category(sig),
                    confidence: Some(dec.confidence),
                    bypass_model: bypass_model.clone(),
                    ..Default::default()
                },
            )
            .await;
        }
        SidecarStep::Fallback => {
            let fb = PolicyState::conservative_fallback(sig);
            let application = apply_action(
                probe,
                deps,
                owner_id,
                kind,
                target_id,
                sig,
                &fb,
                observed_scope,
            )
            .await;
            emit_action_application(
                deps,
                shared,
                owner_id,
                kind,
                target_id,
                sig,
                "rule_fallback",
                &fb,
                Some("low_confidence_rulefallback".to_string()),
                EmitExtra {
                    detail: rule_detail(&fb),
                    category: rule_category(sig),
                    confidence: Some(dec.confidence),
                    bypass_model,
                    ..Default::default()
                },
                application,
            )
            .await;
        }
    }
}

#[derive(Debug)]
enum ActionApplication {
    /// The action was delivered. Conversation actions carry their durable
    /// reservation identity so the audit row uses the same UUID.
    Applied { intervention_id: Option<String> },
    /// No action was delivered, or delivery became ambiguous. The reservation
    /// remains terminal/absorbing, so this result must never be retried
    /// automatically on the same turn.
    Failed {
        intervention_id: Option<String>,
        reason: String,
    },
    /// An identical reservation already exists for this exact turn/action.
    /// Re-emission is intentionally silent and side-effect free.
    Absorbed,
    /// The target has no exact durable authority for this action, so no
    /// delivery was attempted.
    Skipped { reason: String },
}

fn application_from_settled_row(
    row: IdmmActionReservationRow,
    delivery_error: Option<String>,
) -> ActionApplication {
    let intervention_id = Some(row.reservation_id);
    match row.status.as_str() {
        "applied" => ActionApplication::Applied { intervention_id },
        "failed" => ActionApplication::Failed {
            intervention_id,
            reason: row
                .failure_reason
                .or(delivery_error)
                .unwrap_or_else(|| "IDMM action failed without a durable reason".to_owned()),
        },
        _ => ActionApplication::Failed {
            intervention_id,
            reason: delivery_error.unwrap_or_else(|| {
                "IDMM action delivery result remains durably ambiguous".to_owned()
            }),
        },
    }
}

#[allow(clippy::too_many_arguments)]
async fn apply_action(
    probe: &Arc<dyn SessionProbe>,
    deps: &Arc<LoopDeps>,
    owner_id: &str,
    kind: IdmmTargetKind,
    target_id: &str,
    sig: &SessionSignal,
    action: &WakeAction,
    observed_scope: &ObservedActionScope,
) -> ActionApplication {
    if let WakeAction::Wait(d) = action {
        if !d.is_zero() {
            tokio::time::sleep(*d).await;
        }
        return ActionApplication::Applied {
            intervention_id: None,
        };
    }
    if matches!(action, WakeAction::Stop(_)) {
        return ActionApplication::Applied {
            intervention_id: None,
        };
    }

    // A terminal currently exposes no exact durable turn/admission scope to
    // IDMM. Never degrade Retry/SendText/AnswerChoice/Confirm/Failover into an
    // unkeyed PTY write. User input uses the ordinary Terminal API and is
    // unaffected by this supervisor-only fence.
    if kind == IdmmTargetKind::Terminal {
        return ActionApplication::Skipped {
            reason: "Terminal IDMM action skipped: no exact durable terminal turn scope"
                .to_owned(),
        };
    }

    let scope = match observed_scope {
        ObservedActionScope::Exact(scope) => scope,
        ObservedActionScope::Unavailable(reason) => {
            return ActionApplication::Skipped {
                reason: reason.clone(),
            };
        }
        ObservedActionScope::Unsupported => {
            return ActionApplication::Skipped {
                reason: "Conversation IDMM action has no captured turn scope".to_owned(),
            };
        }
    };

    // Revalidation may observe the current turn, but it can only confirm the
    // identity captured with the signal. A successor is never adopted here.
    if let Err(reason) = revalidate_action_scope(probe, observed_scope).await {
        return ActionApplication::Skipped { reason };
    }
    let scope = scope.clone();
    let key = IdmmActionReservationKey {
        user_id: owner_id.to_owned(),
        conversation_id: target_id.to_owned(),
        turn_id: scope.wire_turn_id.clone(),
        turn_generation: scope.generation,
        action_identity: canonical_action_identity(sig, action),
    };
    let turn = IdmmActionTurnIdentity {
        user_id: owner_id.to_owned(),
        conversation_id: target_id.to_owned(),
        turn_id: scope.wire_turn_id.clone(),
        turn_generation: scope.generation,
    };

    // A previous supervisor may have been aborted after delivery but before
    // settlement. Conservatively fail every such reservation before admitting
    // another action on this exact turn; recovery never re-drives side effects.
    if let Err(error) = deps
        .records
        .recover_reserved_actions_for_turn(
            &turn,
            "prior IDMM delivery was interrupted before durable settlement",
            now_ms(),
        )
        .await
    {
        return ActionApplication::Failed {
            intervention_id: None,
            reason: format!(
                "IDMM action recovery persistence failed; injection blocked: {error}"
            ),
        };
    }

    let reserved = match deps
        .records
        .reserve_action(&ReserveIdmmActionParams {
            key: key.clone(),
            reserved_at: now_ms(),
        })
        .await
    {
        Ok(IdmmActionReserveResult::Reserved(row)) => row,
        Ok(
            IdmmActionReserveResult::AlreadyReserved(_)
            | IdmmActionReserveResult::Completed(_),
        ) => return ActionApplication::Absorbed,
        Err(error) => {
            return ActionApplication::Failed {
                intervention_id: None,
                reason: format!(
                    "IDMM action reservation persistence failed; injection blocked: {error}"
                ),
            };
        }
    };
    let reservation_id = reserved.reservation_id.clone();

    let delivery = probe.inject_reserved(action, Some(&scope)).await;
    let (settlement, delivery_error) = match delivery {
        Ok(()) => (IdmmActionSettlement::Applied, None),
        Err(error) => {
            let reason = truncate_detail(error.to_string());
            (
                IdmmActionSettlement::Failed {
                    reason: reason.clone(),
                },
                Some(reason),
            )
        }
    };
    match deps
        .records
        .settle_action(&key, &settlement, now_ms())
        .await
    {
        Ok(IdmmActionSettleResult::Settled(row))
        | Ok(IdmmActionSettleResult::AlreadySettled(row)) => {
            application_from_settled_row(row, delivery_error)
        }
        Err(error) => {
            // The durable `reserved` row is itself absorbing. A later
            // supervisor recovers it to failed; never retry the side effect.
            ActionApplication::Failed {
                intervention_id: Some(reservation_id),
                reason: format!(
                    "IDMM action result settlement failed and remains ambiguous: {error}"
                ),
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn emit_action_application(
    deps: &Arc<LoopDeps>,
    shared: &Arc<SupervisorShared>,
    owner_id: &str,
    kind: IdmmTargetKind,
    target_id: &str,
    sig: &SessionSignal,
    tier_used: &str,
    action: &WakeAction,
    success_reason: Option<String>,
    mut extra: EmitExtra,
    application: ActionApplication,
) {
    match application {
        ActionApplication::Absorbed => {
            debug!(
                target_id,
                action = action.as_str(),
                "Duplicate IDMM action reservation absorbed"
            );
        }
        ActionApplication::Applied { intervention_id } => {
            extra.intervention_id = intervention_id;
            emit_intervention(
                deps,
                shared,
                owner_id,
                kind,
                target_id,
                sig,
                tier_used,
                action.as_str(),
                applied_outcome(action),
                success_reason,
                extra,
            )
            .await;
        }
        ActionApplication::Failed {
            intervention_id,
            reason,
        } => {
            warn!(
                target_id,
                action = action.as_str(),
                reason,
                "IDMM action failed closed"
            );
            extra.intervention_id = intervention_id;
            let reason = match success_reason {
                Some(context) => Some(format!("{context}; {reason}")),
                None => Some(reason),
            };
            emit_intervention(
                deps,
                shared,
                owner_id,
                kind,
                target_id,
                sig,
                tier_used,
                action.as_str(),
                "failed",
                reason,
                extra,
            )
            .await;
        }
        ActionApplication::Skipped { reason } => {
            debug!(
                target_id,
                action = action.as_str(),
                reason,
                "IDMM action skipped without delivery"
            );
            let reason = match success_reason {
                Some(context) => Some(format!("{context}; {reason}")),
                None => Some(reason),
            };
            emit_intervention(
                deps,
                shared,
                owner_id,
                kind,
                target_id,
                sig,
                tier_used,
                action.as_str(),
                "skipped",
                reason,
                extra,
            )
            .await;
        }
    }
}

/// Translate a sidecar-chosen action against the stall it answers. A
/// tool-permission decision is resolved via the agent's confirm channel, so a
/// model `answer_choice`/`send_text` must become a structured `Confirm` (matched
/// to an option's submit-value, falling back to the safe value, else `Stop` —
/// never an unresolved chat reply). Non-permission stalls pass through.
fn finalize_action(sig: &SessionSignal, action: WakeAction) -> WakeAction {
    let SessionSignal::Decision(dp) = sig else {
        return action;
    };
    let Some(perm) = &dp.permission else {
        return action;
    };
    match action {
        WakeAction::AnswerChoice(text) | WakeAction::SendText(text) => {
            let value = perm
                .options
                .iter()
                .find(|(label, val)| val == &text || label == &text)
                .map(|(_, val)| val.clone())
                .or_else(|| perm.safe_value.clone());
            match value {
                Some(v) => WakeAction::Confirm {
                    call_id: perm.call_id.clone(),
                    value: v,
                    always_allow: false,
                },
                None => WakeAction::Stop("sidecar_permission_unmatched".into()),
            }
        }
        other => other,
    }
}

fn set_intervening(
    shared: &Arc<SupervisorShared>,
    deps: &Arc<LoopDeps>,
    owner_id: &str,
    kind: IdmmTargetKind,
    target_id: &str,
    cfg: &IdmmConfig,
    intervening: bool,
) {
    let prev = shared.intervening.swap(intervening, Ordering::Relaxed);
    if prev != intervening {
        // Status-changed events emitted by the supervisor's intervening flips
        // do not need to round-trip the persisted config — the GET endpoint
        // is the rehydration source. Pass None.
        let st = build_state(shared, kind, target_id, cfg, true, None);
        deps.emitter.emit_status_changed(owner_id, &st);
    }
}

/// The enriched fields an `emit_intervention` call may carry beyond the always-
/// present `tier_used`/`action`/`outcome`. Each call site fills what it knows
/// and leaves the rest at default (`Default::default()` ⇒ all `None`), per the
/// plan's "fill from data already available; leave None where unavailable".
#[derive(Default)]
struct EmitExtra {
    /// Durable reservation UUID for a Conversation action. Non-mutating and
    /// terminal actions mint a normal audit UUID at insertion time.
    intervention_id: Option<String>,
    /// "option" | "open_question" | "permission" | "fault" — the decision
    /// category, when the stall was a decision.
    category: Option<String>,
    /// What was chosen / answered (option text, free-text reply). Truncated.
    detail: Option<String>,
    /// Model confidence (sidecar tier only; `None` for rule decisions).
    confidence: Option<f32>,
    /// The bypass `provider/model` the sidecar used (`None` for rule decisions).
    bypass_model: Option<String>,
}

/// Truncate a string to `DETAIL_MAX_CHARS` chars (char-boundary safe).
fn truncate_detail(s: String) -> String {
    if s.chars().count() <= DETAIL_MAX_CHARS {
        return s;
    }
    s.chars().take(DETAIL_MAX_CHARS).collect()
}

/// Derive the watch lane from the signal: provider/agent faults are the
/// fault-watch lane; everything else (idle nudges, decisions) is decision-watch.
fn watch_for(sig: &SessionSignal) -> &'static str {
    match sig {
        SessionSignal::ProviderError { .. } | SessionSignal::AgentError { .. } => "fault",
        _ => "decision",
    }
}

/// Canonical disposition for an action we applied: a `Stop` means we stood down
/// (→ "halted"), anything actually injected is "applied". The free-form *why*
/// is carried separately in the record's `reason` field, never in `outcome`.
fn applied_outcome(action: &WakeAction) -> &'static str {
    if matches!(action, WakeAction::Stop(_)) {
        "halted"
    } else {
        "applied"
    }
}

fn identity_field(hasher: &mut Sha256, label: &str, value: &str) {
    for part in [label.as_bytes(), value.as_bytes()] {
        hasher.update((part.len() as u64).to_be_bytes());
        hasher.update(part);
    }
}

/// Stable, privacy-preserving identity for one semantic action on one signal.
///
/// The durable reservation key also carries the exact conversation turn scope.
/// Hashing the normalized signal plus the final action makes duplicate delivery
/// of the same live stall idempotent without storing prompts, provider errors,
/// or sidecar answers in the reservation table.
fn canonical_action_identity(sig: &SessionSignal, action: &WakeAction) -> String {
    let mut hasher = Sha256::new();
    identity_field(&mut hasher, "schema", "idmm-action-v1");
    match sig {
        SessionSignal::Working => identity_field(&mut hasher, "signal", "working"),
        SessionSignal::ProviderError {
            code,
            retryable,
            message,
        } => {
            identity_field(&mut hasher, "signal", "provider_error");
            identity_field(&mut hasher, "code", &format!("{code:?}"));
            identity_field(&mut hasher, "retryable", &format!("{retryable:?}"));
            identity_field(
                &mut hasher,
                "message",
                &message.split_whitespace().collect::<Vec<_>>().join(" "),
            );
        }
        SessionSignal::AgentError { retryable, message } => {
            identity_field(&mut hasher, "signal", "agent_error");
            identity_field(&mut hasher, "retryable", &format!("{retryable:?}"));
            identity_field(
                &mut hasher,
                "message",
                &message.split_whitespace().collect::<Vec<_>>().join(" "),
            );
        }
        SessionSignal::Idle => identity_field(&mut hasher, "signal", "idle"),
        SessionSignal::Decision(prompt) => {
            identity_field(&mut hasher, "signal", "decision");
            identity_field(&mut hasher, "kind", &format!("{:?}", prompt.kind));
            if let Some(permission) = &prompt.permission {
                // The call id is the backend's stable identity across the live
                // event and on-arm pending-confirmation recovery paths.
                identity_field(&mut hasher, "permission_call_id", &permission.call_id);
            } else {
                identity_field(
                    &mut hasher,
                    "prompt",
                    &prompt.text.split_whitespace().collect::<Vec<_>>().join(" "),
                );
                for option in &prompt.options {
                    identity_field(&mut hasher, "option", option);
                }
            }
        }
        SessionSignal::Done => identity_field(&mut hasher, "signal", "done"),
        SessionSignal::Cancelled => identity_field(&mut hasher, "signal", "cancelled"),
        SessionSignal::Exited => identity_field(&mut hasher, "signal", "exited"),
    }
    match action {
        WakeAction::Retry => identity_field(&mut hasher, "action", "retry"),
        WakeAction::SendText(text) => {
            identity_field(&mut hasher, "action", "send_text");
            identity_field(&mut hasher, "text", text);
        }
        WakeAction::AnswerChoice(text) => {
            identity_field(&mut hasher, "action", "answer_choice");
            identity_field(&mut hasher, "text", text);
        }
        WakeAction::Confirm {
            call_id,
            value,
            always_allow,
        } => {
            identity_field(&mut hasher, "action", "confirm");
            identity_field(&mut hasher, "call_id", call_id);
            identity_field(&mut hasher, "value", value);
            identity_field(&mut hasher, "always_allow", &always_allow.to_string());
        }
        WakeAction::Failover => identity_field(&mut hasher, "action", "failover"),
        WakeAction::Wait(delay) => {
            identity_field(&mut hasher, "action", "wait");
            identity_field(&mut hasher, "millis", &delay.as_millis().to_string());
        }
        WakeAction::Stop(reason) => {
            identity_field(&mut hasher, "action", "stop");
            identity_field(&mut hasher, "reason", reason);
        }
    }

    let digest = hasher.finalize();
    let mut encoded = String::with_capacity(digest.len() * 2);
    use std::fmt::Write as _;
    for byte in digest {
        let _ = write!(encoded, "{byte:02x}");
    }
    encoded
}

/// The human-meaningful "what was done" for a rule-tier action: the answer /
/// text we injected. Pure side-effect actions (retry/wait/stop) carry no detail.
fn rule_detail(action: &WakeAction) -> Option<String> {
    match action {
        WakeAction::AnswerChoice(t) | WakeAction::SendText(t) => Some(t.clone()),
        WakeAction::Confirm { value, .. } => Some(value.clone()),
        _ => None,
    }
}

/// Category of a rule/decision-tier decision (only set when the stall is a
/// decision): an open-ended question, a structured tool permission, or a
/// numbered/text option prompt.
fn rule_category(sig: &SessionSignal) -> Option<String> {
    let SessionSignal::Decision(dp) = sig else {
        return None;
    };
    Some(
        if dp.kind == DecisionKind::OpenQuestion {
            "open_question"
        } else if dp.permission.is_some() {
            "permission"
        } else {
            "option"
        }
        .to_string(),
    )
}

#[allow(clippy::too_many_arguments)]
async fn emit_intervention(
    deps: &Arc<LoopDeps>,
    shared: &Arc<SupervisorShared>,
    owner_id: &str,
    kind: IdmmTargetKind,
    target_id: &str,
    sig: &SessionSignal,
    tier_used: &str,
    action: &str,
    outcome: &str,
    reason: Option<String>,
    extra: EmitExtra,
) {
    let at = now_ms();
    let stall_class = stall_class_label(sig).to_string();
    let target_kind = kind.as_str().to_string();
    let watch = watch_for(sig).to_string();
    let reason = reason.map(truncate_detail);
    let detail = extra.detail.map(truncate_detail);
    let intervention_id = extra
        .intervention_id
        .unwrap_or_else(|| nomifun_common::IdmmInterventionId::new().into_string());

    // Best-effort post-settlement audit: Conversation side effects have already
    // crossed the separate durable reservation/settlement fence, so an insert
    // failure cannot authorize a retry. The DB remains the source of truth for
    // `/log`; the supervisor itself keeps only live counters (count / last-at).
    let row = NewIdmmInterventionRow {
        intervention_id,
        user_id: owner_id.to_owned(),
        target_kind: target_kind.clone(),
        target_id: target_id.to_string(),
        watch: watch.clone(),
        at,
        signal: stall_class.clone(),
        tier_used: tier_used.to_string(),
        category: extra.category.clone(),
        action: action.to_string(),
        detail: detail.clone(),
        reason: reason.clone(),
        confidence: extra.confidence.map(f64::from),
        bypass_model: extra.bypass_model.clone(),
        outcome: outcome.to_string(),
    };
    let inserted = match deps.records.insert(&row).await {
        Ok(inserted) => inserted,
        Err(e) => {
            warn!(target_id, error = %e, "IDMM intervention audit persist failed");
            // Do not publish a fabricated technical ID to the API/WS stream.
            // For Conversation actions, the reservation row still permanently
            // absorbs the identical exact-turn action.
            return;
        }
    };

    let rec = InterventionRecord {
        intervention_id: match nomifun_common::IdmmInterventionId::parse(inserted.intervention_id) {
            Ok(intervention_id) => intervention_id,
            Err(error) => {
                warn!(target_id, %error, "IDMM repository returned invalid intervention_id");
                return;
            }
        },
        target_kind,
        target_id: target_id.to_string(),
        watch,
        at,
        stall_class,
        tier_used: tier_used.to_string(),
        category: extra.category,
        action: action.to_string(),
        detail,
        outcome: outcome.to_string(),
        reason,
        confidence: extra.confidence,
        bypass_model: extra.bypass_model,
    };

    info!(
        target_id,
        stall = %rec.stall_class,
        tier = tier_used,
        action,
        outcome,
        "IDMM intervention"
    );
    shared.record(&rec);
    deps.emitter.emit_intervention(owner_id, &rec);
}

/// Build the live state for emission / API.
///
/// `config_persisted` carries the per-session config when one has been saved
/// to disk (so the frontend can rehydrate its form). Pass `None` for purely
/// runtime emissions where the persisted blob is not relevant (status-changed
/// events triggered by intervening flips, intervention-emit refreshes).
pub fn build_state(
    shared: &SupervisorShared,
    kind: IdmmTargetKind,
    target_id: &str,
    cfg: &IdmmConfig,
    sidecar_resolved: bool,
    config_persisted: Option<&IdmmConfig>,
) -> IdmmState {
    let intervening = shared.intervening.load(Ordering::Relaxed);
    let enabled = cfg.any_enabled();
    IdmmState {
        kind,
        target_id: target_id.to_string(),
        enabled,
        fault_enabled: cfg.fault_watch.base.enabled,
        decision_enabled: cfg.decision_watch.base.enabled,
        run_state: IdmmState::run_state(enabled, intervening),
        interventions_count: shared.count.load(Ordering::Relaxed),
        last_signal: shared.last_signal.lock().unwrap().clone(),
        last_intervention_at: *shared.last_intervention_at.lock().unwrap(),
        sidecar_provider_resolved: sidecar_resolved,
        config: config_persisted.cloned(),
    }
}

fn signal_label(sig: &SessionSignal) -> String {
    match sig {
        SessionSignal::Working => "working".into(),
        SessionSignal::ProviderError { message, .. } => format!("provider_error: {message}"),
        SessionSignal::AgentError { message, .. } => format!("agent_error: {message}"),
        SessionSignal::Idle => "idle".into(),
        SessionSignal::Decision(d) => format!("decision: {}", d.text),
        SessionSignal::Done => "done".into(),
        SessionSignal::Cancelled => "cancelled".into(),
        SessionSignal::Exited => "exited".into(),
    }
}

fn stall_class_label(sig: &SessionSignal) -> &'static str {
    match sig {
        SessionSignal::ProviderError { .. } | SessionSignal::AgentError { .. } => StallClass::ProviderError.as_str(),
        SessionSignal::Idle => StallClass::Idle.as_str(),
        SessionSignal::Decision(dp) => {
            if dp.kind == DecisionKind::OpenQuestion {
                StallClass::OpenQuestion.as_str()
            } else {
                StallClass::Decision.as_str()
            }
        }
        _ => "unknown",
    }
}

// ─────────────────────────────── IdmmManager ───────────────────────────────

/// Builds a `SessionProbe` for a target (so the manager can re-arm lazily).
pub trait ProbeFactory: Send + Sync {
    /// Build a probe for a target. Returns `None` if the target is gone / not
    /// buildable.
    fn build(&self, kind: IdmmTargetKind, target_id: &str) -> Option<Arc<dyn SessionProbe>>;
}

/// Reads persisted IDMM config for a target (impl in service.rs over the DB).
#[async_trait::async_trait]
pub trait ConfigReader: Send + Sync {
    async fn read(
        &self,
        user_id: &str,
        kind: IdmmTargetKind,
        target_id: &str,
    ) -> Result<IdmmConfig, AppError>;
}

/// Domain-qualified key for the per-target supervisor maps. Canonical v2 IDs
/// are already globally unique, while retaining `kind` in the runtime key keeps
/// domain dispatch explicit and prevents a mismatched kind/ID pair from
/// aliasing another supervisor (spec §2.2 C3).
type IdmmKey = (IdmmTargetKind, String);

/// Inner shared state, kept behind an `Arc` so the sync `IdmmHandle` seam can
/// clone it into a detached task (the lifecycle `ensure` is async).
pub struct IdmmInner {
    deps: Arc<LoopDeps>,
    /// `Arc` so each supervisor task can carry a cleanup guard that removes
    /// its own (generation-matched) entry when `run_supervisor` returns —
    /// without this, a naturally-exited supervisor (session Exited, probe
    /// found no Agent runtime) stayed in the map forever, `is_supervising`
    /// reported a live supervisor that wasn't there (AutoWork then "waited
    /// for IDMM recovery" that never came), and `ensure` could never re-arm.
    handles: Arc<DashMap<IdmmKey, SupervisorHandle>>,
    /// Shared state survives a handle's lifetime so the API can read counts/log
    /// even between re-arms.
    shared: DashMap<IdmmKey, Arc<SupervisorShared>>,
    factory: Arc<dyn ProbeFactory>,
    config_reader: Arc<dyn ConfigReader>,
    next_generation: std::sync::atomic::AtomicU64,
}

/// Removes a supervisor's handle from the map when its task ends — normal exit
/// OR abort/panic (Drop runs during unwind and on future drop). The generation
/// guard prevents clobbering a fresh handle a concurrent `ensure` inserted.
struct SupervisorCleanup {
    handles: Arc<DashMap<IdmmKey, SupervisorHandle>>,
    key: IdmmKey,
    generation: u64,
}

impl Drop for SupervisorCleanup {
    fn drop(&mut self) {
        self.handles
            .remove_if(&self.key, |_, h| h.generation == self.generation);
    }
}

impl IdmmInner {
    fn shared_for(&self, kind: IdmmTargetKind, target_id: &str) -> Arc<SupervisorShared> {
        self.shared
            .entry((kind, target_id.to_string()))
            .or_insert_with(|| Arc::new(SupervisorShared::default()))
            .clone()
    }

    fn is_supervising(&self, kind: IdmmTargetKind, target_id: &str) -> bool {
        self.handles
            .get(&(kind, target_id.to_string()))
            .map(|h| !h.cancel.load(Ordering::SeqCst) && !h.join.is_finished())
            .unwrap_or(false)
    }

    /// Whether `turn_text` (the just-finished turn's assistant text) is a pending
    /// decision the DECISION watch will answer — reuses the probe's
    /// `decision_in_text` (detects from the text itself, no persisted-row race),
    /// gated on the decision watch being enabled. Lets AutoWork yield a
    /// decision-ending turn to IDMM. False when the decision watch is off, the
    /// target is not buildable, or the text is not a decision.
    async fn has_pending_decision(&self, kind: IdmmTargetKind, target_id: &str, turn_text: &str) -> bool {
        let Some(probe) = self.factory.build(kind, target_id) else {
            return false;
        };
        let Ok(description) = probe.describe().await else {
            return false;
        };
        if UserId::parse(&description.user_id).is_err() || description.kind != kind {
            return false;
        }
        let Ok(cfg) = self
            .config_reader
            .read(&description.user_id, kind, target_id)
            .await
        else {
            return false;
        };
        if !cfg.decision_watch.base.enabled {
            return false;
        }
        probe.decision_in_text(turn_text).await
    }

    async fn ensure_with_scope(
        &self,
        kind: IdmmTargetKind,
        target_id: &str,
        admitted_scope: Option<IdmmTurnScope>,
    ) {
        let admitted_turn_generation = admitted_scope.as_ref().map(|scope| scope.generation);
        let Some(probe) = self.factory.build(kind, target_id) else {
            return;
        };
        let description = match probe.describe().await {
            Ok(description)
                if description.kind == kind && UserId::parse(&description.user_id).is_ok() => description,
            Ok(_) => {
                warn!(target_id, ?kind, "IDMM target has no valid owner — supervisor not armed");
                return;
            }
            Err(error) => {
                warn!(target_id, ?kind, error = %error, "IDMM target owner resolution failed — supervisor not armed");
                return;
            }
        };
        if let Some(current) = self.handles.get(&(kind, target_id.to_string())) {
            let current_is_live =
                !current.cancel.load(Ordering::SeqCst) && !current.join.is_finished();
            let current_is_same_or_newer = admitted_turn_generation.is_some_and(|incoming| {
                current
                    .admitted_turn_generation
                    .is_some_and(|current| current >= incoming)
            });
            if current_is_live
                && (admitted_turn_generation.is_none() || current_is_same_or_newer)
            {
                return;
            }
        }
        let cfg = match self
            .config_reader
            .read(&description.user_id, kind, target_id)
            .await
        {
            Ok(cfg) => cfg,
            Err(error) => {
                warn!(target_id, ?kind, error = %error, "IDMM owned config read failed — supervisor not armed");
                return;
            }
        };
        if !cfg.any_enabled() {
            return;
        }
        let shared = self.shared_for(kind, target_id);
        let cancel = Arc::new(AtomicBool::new(false));
        let generation = self.next_generation.fetch_add(1, Ordering::SeqCst);
        let key: IdmmKey = (kind, target_id.to_string());
        let cleanup = SupervisorCleanup {
            handles: self.handles.clone(),
            key: key.clone(),
            generation,
        };
        // The candidate cannot execute until it wins the generation-aware map
        // insertion below. A delayed old hook therefore has no action window
        // before it discovers that a newer turn already owns supervision.
        let (start_tx, start_rx) = tokio::sync::oneshot::channel();
        let join = tokio::spawn({
            let cfg = cfg.clone();
            let deps = self.deps.clone();
            let cancel = cancel.clone();
            let owner_id = description.user_id;
            async move {
                let _cleanup = cleanup;
                if start_rx.await.is_err() {
                    return;
                }
                run_supervisor_for_owner(
                    probe,
                    cfg,
                    deps,
                    shared,
                    cancel,
                    Some(owner_id),
                    admitted_scope,
                )
                .await;
            }
        });
        // The supervisor may exit (and clean up) before this insert runs — a
        // The task remains start-gated here; only the generation-aware winner
        // below can begin observing or applying policy.
        let candidate = SupervisorHandle {
            cancel,
            join,
            generation,
            admitted_turn_generation,
        };
        let installed = match self.handles.entry(key) {
            dashmap::mapref::entry::Entry::Vacant(entry) => {
                entry.insert(candidate);
                true
            }
            dashmap::mapref::entry::Entry::Occupied(mut entry) => {
                let current = entry.get();
                let current_is_live =
                    !current.cancel.load(Ordering::SeqCst) && !current.join.is_finished();
                let incoming_is_newer = admitted_turn_generation.is_some_and(|incoming| {
                    current
                        .admitted_turn_generation
                        .map_or(true, |current| incoming > current)
                });
                if !current_is_live || incoming_is_newer {
                    entry.insert(candidate);
                    true
                } else {
                    // Drop aborts the still start-gated task. Its cleanup guard
                    // is keyed by `generation` and cannot remove this winner.
                    drop(candidate);
                    false
                }
            }
        };
        if installed {
            let _ = start_tx.send(());
            info!(
                target_id,
                ?kind,
                admitted_turn_generation,
                "IDMM supervisor armed"
            );
        }
    }

    async fn ensure(&self, kind: IdmmTargetKind, target_id: &str) {
        self.ensure_with_scope(kind, target_id, None).await;
    }

    async fn replace_conversation_turn(
        &self,
        target_id: &str,
        admitted_scope: IdmmTurnScope,
    ) {
        // Detached turn hooks may complete out of order. Remove only an
        // older/unknown generation; a delayed hook for turn A must not cancel
        // an already-installed supervisor for successor B.
        let key = (IdmmTargetKind::Conversation, target_id.to_owned());
        let incoming_generation = admitted_scope.generation;
        self.handles.remove_if(&key, |_, current| {
            let current_is_live =
                !current.cancel.load(Ordering::SeqCst) && !current.join.is_finished();
            !current_is_live
                || current
                    .admitted_turn_generation
                    .map_or(true, |generation| generation < incoming_generation)
        });
        self.ensure_with_scope(
            IdmmTargetKind::Conversation,
            target_id,
            Some(admitted_scope),
        )
        .await;
    }

    fn stop(&self, kind: IdmmTargetKind, target_id: &str) {
        if self.handles.remove(&(kind, target_id.to_string())).is_some() {
            info!(target_id, ?kind, "IDMM supervisor stopped");
        }
    }
}

/// The IDMM lifecycle manager: owns supervisor handles + shared state, and
/// implements the AutoWork `IdmmHandle` seam. Cheaply `Clone` (Arc inner).
#[derive(Clone)]
pub struct IdmmManager {
    inner: Arc<IdmmInner>,
}

impl IdmmManager {
    pub fn new(deps: Arc<LoopDeps>, factory: Arc<dyn ProbeFactory>, config_reader: Arc<dyn ConfigReader>) -> Self {
        Self {
            inner: Arc::new(IdmmInner {
                deps,
                handles: Arc::new(DashMap::new()),
                shared: DashMap::new(),
                factory,
                config_reader,
                next_generation: std::sync::atomic::AtomicU64::new(0),
            }),
        }
    }

    /// Shared state for a target (created on demand), for the API to read.
    pub fn shared_for(&self, kind: IdmmTargetKind, target_id: &str) -> Arc<SupervisorShared> {
        self.inner.shared_for(kind, target_id)
    }

    /// Whether a supervisor task is currently live for the `(kind, target)`.
    pub fn is_supervising(&self, kind: IdmmTargetKind, target_id: &str) -> bool {
        self.inner.is_supervising(kind, target_id)
    }

    /// Start supervising (idempotent). Reads config; only arms if enabled.
    pub async fn ensure(&self, kind: IdmmTargetKind, target_id: &str) {
        self.inner.ensure(kind, target_id).await;
    }

    /// Stop supervising a target (drops the handle → cancels + aborts).
    pub fn stop(&self, kind: IdmmTargetKind, target_id: &str) {
        self.inner.stop(kind, target_id);
    }
}

/// AutoWork → IDMM seam. Sync method spawns the async `ensure` on a detached
/// task (AutoWork's loop must not block on it).
#[async_trait::async_trait]
impl nomifun_requirement::IdmmHandle for IdmmManager {
    fn ensure_supervising(&self, kind: AutoWorkTargetKind, target_id: &str) {
        let inner = self.inner.clone();
        let kind = from_autowork_kind(kind);
        let target_id = target_id.to_string();
        tokio::spawn(async move {
            inner.ensure(kind, &target_id).await;
        });
    }

    fn is_supervising(&self, kind: AutoWorkTargetKind, target_id: &str) -> bool {
        self.inner.is_supervising(from_autowork_kind(kind), target_id)
    }

    async fn has_pending_decision(&self, kind: AutoWorkTargetKind, target_id: &str, turn_text: &str) -> bool {
        self.inner
            .has_pending_decision(from_autowork_kind(kind), target_id, turn_text)
            .await
    }
}

/// ConversationService → IDMM seam. A user-driven desktop turn arms supervision
/// for the conversation (the path that has no AutoWork loop / boot-resume to do
/// it). Sync + fire-and-forget: spawns the async `ensure`, which is a no-op when
/// IDMM is disabled for the target or already supervising it.
impl nomifun_conversation::ConversationSupervisionHook for IdmmManager {
    fn on_turn_start(
        &self,
        conversation_id: &str,
        admitted_scope: nomifun_conversation::IdmmTurnScope,
    ) {
        let inner = self.inner.clone();
        let target_id = conversation_id.to_string();
        tokio::spawn(async move {
            inner
                .replace_conversation_turn(&target_id, admitted_scope)
                .await;
        });
    }
}

/// TerminalService → IDMM seam. A user-driven terminal has no AutoWork loop /
/// boot-resume to arm supervision, and (unlike a chat turn) fires on every input
/// chunk — so we guard on `is_supervising` BEFORE spawning to avoid a detached
/// `ensure` task per keystroke. The supervisor stands down on PTY exit / Halt;
/// the next activity (input / relaunch / create) re-arms it.
impl nomifun_terminal::TerminalSupervisionHook for IdmmManager {
    fn on_terminal_activity(&self, terminal_id: &str) {
        if self.inner.is_supervising(IdmmTargetKind::Terminal, terminal_id) {
            return;
        }
        let inner = self.inner.clone();
        let target_id = terminal_id.to_string();
        tokio::spawn(async move {
            inner.ensure(IdmmTargetKind::Terminal, &target_id).await;
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::probe::{SessionDescription, SessionProbe};
    use crate::sidecar::{Completer, SidecarClient};
    use crate::signal::{
        DecisionKind, DecisionPrompt, DecisionSource, PermissionConfirm, WakeAction,
    };
    use async_trait::async_trait;
    use nomifun_api_types::{IdmmConfig, WatchTier};
    use nomifun_db::DbError;
    use nomifun_db::models::ClientPreference;
    use std::sync::atomic::AtomicUsize;
    use std::sync::Mutex;
    use tokio::sync::{Barrier, mpsc};

    const TEST_USER_ID: &str = "0190f5fe-7c00-7a00-8000-000000000001";
    const CONVERSATION_TARGET_ID: &str =
        "0190f5fe-7c00-7a00-8000-000000000002";
    const CONVERSATION_TURN_ID: &str =
        "0190f5fe-7c00-7a00-8000-000000000004";
    const CONVERSATION_TURN_GENERATION: u64 = 7;
    const TERMINAL_TARGET_ID: &str =
        "0190f5fe-7c00-7a00-8000-000000000002";
    const TEST_PROVIDER_ID: &str = "0190f5fe-7c00-7a00-8000-000000000003";
    const TEST_BYPASS_MODEL: &str =
        "0190f5fe-7c00-7a00-8000-000000000003/m";

    #[test]
    fn action_identity_is_stable_for_duplicate_semantic_faults() {
        let first = SessionSignal::ProviderError {
            code: None,
            retryable: Some(true),
            message: "gateway   unavailable\nretry".into(),
        };
        let duplicate = SessionSignal::ProviderError {
            code: None,
            retryable: Some(true),
            message: "gateway unavailable retry".into(),
        };
        assert_eq!(
            canonical_action_identity(&first, &WakeAction::Retry),
            canonical_action_identity(&duplicate, &WakeAction::Retry)
        );
        assert_ne!(
            canonical_action_identity(&first, &WakeAction::Retry),
            canonical_action_identity(
                &first,
                &WakeAction::SendText("Please continue.".into())
            ),
            "the final action is part of durable idempotency"
        );
    }

    #[test]
    fn action_identity_uses_structured_permission_call_identity() {
        let decision = |call_id: &str, text: &str| {
            SessionSignal::Decision(DecisionPrompt {
                text: text.into(),
                options: vec!["Allow once".into(), "Reject".into()],
                recommended: None,
                source: DecisionSource::Permission,
                kind: DecisionKind::Options,
                permission: Some(PermissionConfirm {
                    call_id: call_id.into(),
                    options: vec![
                        ("Allow once".into(), "proceed_once".into()),
                        ("Reject".into(), "cancel".into()),
                    ],
                    safe_value: Some("proceed_once".into()),
                }),
            })
        };
        let action = WakeAction::Confirm {
            call_id: "call-1".into(),
            value: "proceed_once".into(),
            always_allow: false,
        };
        assert_eq!(
            canonical_action_identity(&decision("call-1", "Allow read?"), &action),
            canonical_action_identity(
                &decision("call-1", "Localized display text changed"),
                &action
            ),
            "live and recovered forms of the same confirmation must dedupe"
        );
        assert_ne!(
            canonical_action_identity(&decision("call-1", "Allow read?"), &action),
            canonical_action_identity(
                &decision("call-2", "Allow read?"),
                &WakeAction::Confirm {
                    call_id: "call-2".into(),
                    value: "proceed_once".into(),
                    always_allow: false,
                }
            )
        );
    }

    // ── Mock probe: scripted signal queue + captured injects ──
    struct MockProbe {
        signals: Mutex<Vec<SessionSignal>>,
        injected: Arc<Mutex<Vec<WakeAction>>>,
        target_id: String,
        kind: IdmmTargetKind,
        /// Scripted `pending_signal` results, popped one per call (mirroring a
        /// real `ConversationProbe::pending_signal`, which is consulted on arm AND
        /// — after the mid-turn-arm-recovery fix — on each Idle). An empty queue
        /// yields `None` (nothing pending), the default probes return.
        pending: Mutex<std::collections::VecDeque<Option<SessionSignal>>>,
    }
    impl MockProbe {
        fn new(signals: Vec<SessionSignal>) -> (Arc<Self>, Arc<Mutex<Vec<WakeAction>>>) {
            // Default to Conversation so tests exercise the post-Req3
            // Working→Idle nudge ladder. Terminal-specific tests use
            // `with_kind` to opt into the conservative-idle policy.
            Self::with_kind(signals, IdmmTargetKind::Conversation)
        }
        fn with_kind(
            signals: Vec<SessionSignal>,
            kind: IdmmTargetKind,
        ) -> (Arc<Self>, Arc<Mutex<Vec<WakeAction>>>) {
            let injected = Arc::new(Mutex::new(vec![]));
            (
                Arc::new(Self {
                    signals: Mutex::new(signals),
                    injected: injected.clone(),
                    target_id: match kind {
                        IdmmTargetKind::Conversation => CONVERSATION_TARGET_ID,
                        IdmmTargetKind::Terminal => TERMINAL_TARGET_ID,
                    }
                    .into(),
                    kind,
                    pending: Mutex::new(std::collections::VecDeque::new()),
                }),
                injected,
            )
        }
        /// Seed the on-arm pending decision the supervisor should evaluate ONCE
        /// before the observe loop (the "armed after the agent already asked" case).
        fn with_pending(self: Arc<Self>, sig: SessionSignal) -> Arc<Self> {
            self.pending.lock().unwrap().push_back(Some(sig));
            self
        }
        /// Script the exact sequence `pending_signal` returns across calls (on-arm
        /// then per-Idle), e.g. `[None, Some(decision)]` = nothing pending at arm,
        /// a decision pending at the first Idle (mid-turn-arm recovery).
        fn with_pending_seq(self: Arc<Self>, seq: Vec<Option<SessionSignal>>) -> Arc<Self> {
            self.pending.lock().unwrap().extend(seq);
            self
        }
    }
    #[async_trait]
    impl SessionProbe for MockProbe {
        fn target(&self) -> (IdmmTargetKind, String) {
            (self.kind, self.target_id.clone())
        }
        fn observe(&self, _idle: Duration) -> mpsc::Receiver<SessionSignal> {
            let (tx, rx) = mpsc::channel(64);
            let sigs = std::mem::take(&mut *self.signals.lock().unwrap());
            tokio::spawn(async move {
                for s in sigs {
                    if tx.send(s).await.is_err() {
                        return;
                    }
                }
                // Then exit so the loop terminates.
                let _ = tx.send(SessionSignal::Exited).await;
            });
            rx
        }
        async fn inject(&self, action: &WakeAction) -> Result<(), nomifun_common::AppError> {
            self.injected.lock().unwrap().push(action.clone());
            Ok(())
        }
        async fn action_scope(
            &self,
        ) -> Result<Option<IdmmTurnScope>, nomifun_common::AppError> {
            Ok((self.kind == IdmmTargetKind::Conversation).then(|| IdmmTurnScope {
                wire_turn_id: CONVERSATION_TURN_ID.into(),
                generation: CONVERSATION_TURN_GENERATION,
            }))
        }
        async fn inject_reserved(
            &self,
            action: &WakeAction,
            scope: Option<&IdmmTurnScope>,
        ) -> Result<(), nomifun_common::AppError> {
            match self.kind {
                IdmmTargetKind::Conversation
                    if scope
                        == Some(&IdmmTurnScope {
                            wire_turn_id: CONVERSATION_TURN_ID.into(),
                            generation: CONVERSATION_TURN_GENERATION,
                        }) => {}
                _ => {
                    return Err(nomifun_common::AppError::Conflict(
                        "mock rejected a missing or stale exact-turn reservation".into(),
                    ));
                }
            }
            self.inject(action).await
        }
        async fn snapshot_context(&self, _max: usize) -> Result<String, nomifun_common::AppError> {
            Ok("ctx".into())
        }
        fn is_alive(&self) -> bool {
            true
        }
        async fn describe(&self) -> Result<SessionDescription, nomifun_common::AppError> {
            Ok(SessionDescription {
                kind: self.kind,
                backend: Some("claude".into()),
                user_id: TEST_USER_ID.into(),
                alive: true,
            })
        }
        async fn pending_signal(&self) -> Option<SessionSignal> {
            self.pending.lock().unwrap().pop_front().flatten()
        }
        async fn decision_in_text(&self, turn_text: &str) -> bool {
            // Mirror ConversationProbe: a numbered menu or an open question (no DB).
            crate::detector::detect_chat_decision(turn_text).is_some()
                || crate::detector::detect_chat_open_question(turn_text).is_some()
        }
    }

    // ── Mock completer + prefs (reused shape from sidecar tests) ──
    /// Probe whose second scope read is held at a barrier. The first read is the
    /// signal-time capture; the second is pre-reservation revalidation.
    struct ScopeBarrierProbe {
        receiver: Mutex<Option<mpsc::Receiver<SessionSignal>>>,
        current_scope: Mutex<Option<IdmmTurnScope>>,
        scope_reads: AtomicUsize,
        revalidation_read: Option<usize>,
        revalidation_entered: Arc<Barrier>,
        release_revalidation: Arc<Barrier>,
        pending_entered: Option<Arc<Barrier>>,
        release_pending: Option<Arc<Barrier>>,
        injected: Arc<Mutex<Vec<WakeAction>>>,
        delivery_calls: AtomicUsize,
        pending: Mutex<Option<SessionSignal>>,
    }

    impl ScopeBarrierProbe {
        #[allow(clippy::type_complexity)]
        fn new(
            pending: Option<SessionSignal>,
        ) -> (
            Arc<Self>,
            mpsc::Sender<SessionSignal>,
            Arc<Mutex<Vec<WakeAction>>>,
            Arc<Barrier>,
            Arc<Barrier>,
        ) {
            let (tx, rx) = mpsc::channel(16);
            let injected = Arc::new(Mutex::new(Vec::new()));
            let revalidation_entered = Arc::new(Barrier::new(2));
            let release_revalidation = Arc::new(Barrier::new(2));
            (
                Arc::new(Self {
                    receiver: Mutex::new(Some(rx)),
                    current_scope: Mutex::new(Some(IdmmTurnScope {
                        wire_turn_id: CONVERSATION_TURN_ID.into(),
                        generation: CONVERSATION_TURN_GENERATION,
                    })),
                    scope_reads: AtomicUsize::new(0),
                    revalidation_read: Some(1),
                    revalidation_entered: revalidation_entered.clone(),
                    release_revalidation: release_revalidation.clone(),
                    pending_entered: None,
                    release_pending: None,
                    injected: injected.clone(),
                    delivery_calls: AtomicUsize::new(0),
                    pending: Mutex::new(pending),
                }),
                tx,
                injected,
                revalidation_entered,
                release_revalidation,
            )
        }

        #[allow(clippy::type_complexity)]
        fn with_pending_barrier() -> (
            Arc<Self>,
            mpsc::Sender<SessionSignal>,
            Arc<Mutex<Vec<WakeAction>>>,
            Arc<Barrier>,
            Arc<Barrier>,
        ) {
            let (tx, rx) = mpsc::channel(16);
            let injected = Arc::new(Mutex::new(Vec::new()));
            let pending_entered = Arc::new(Barrier::new(2));
            let release_pending = Arc::new(Barrier::new(2));
            (
                Arc::new(Self {
                    receiver: Mutex::new(Some(rx)),
                    current_scope: Mutex::new(Some(IdmmTurnScope {
                        wire_turn_id: CONVERSATION_TURN_ID.into(),
                        generation: CONVERSATION_TURN_GENERATION,
                    })),
                    scope_reads: AtomicUsize::new(0),
                    revalidation_read: None,
                    revalidation_entered: Arc::new(Barrier::new(1)),
                    release_revalidation: Arc::new(Barrier::new(1)),
                    pending_entered: Some(pending_entered.clone()),
                    release_pending: Some(release_pending.clone()),
                    injected: injected.clone(),
                    delivery_calls: AtomicUsize::new(0),
                    pending: Mutex::new(None),
                }),
                tx,
                injected,
                pending_entered,
                release_pending,
            )
        }

        fn replace_with_successor_scope(&self) {
            *self.current_scope.lock().unwrap() = Some(IdmmTurnScope {
                wire_turn_id: "0190f5fe-7c00-7a00-8000-000000000005".into(),
                generation: CONVERSATION_TURN_GENERATION + 1,
            });
        }
    }

    #[async_trait]
    impl SessionProbe for ScopeBarrierProbe {
        fn target(&self) -> (IdmmTargetKind, String) {
            (
                IdmmTargetKind::Conversation,
                CONVERSATION_TARGET_ID.into(),
            )
        }

        fn observe(&self, _idle: Duration) -> mpsc::Receiver<SessionSignal> {
            self.receiver
                .lock()
                .unwrap()
                .take()
                .expect("scope barrier probe can be observed only once")
        }

        async fn inject(&self, action: &WakeAction) -> Result<(), AppError> {
            self.injected.lock().unwrap().push(action.clone());
            Ok(())
        }

        async fn action_scope(&self) -> Result<Option<IdmmTurnScope>, AppError> {
            let read = self.scope_reads.fetch_add(1, Ordering::SeqCst);
            if self.revalidation_read == Some(read) {
                self.revalidation_entered.wait().await;
                self.release_revalidation.wait().await;
            }
            Ok(self.current_scope.lock().unwrap().clone())
        }

        async fn inject_reserved(
            &self,
            action: &WakeAction,
            scope: Option<&IdmmTurnScope>,
        ) -> Result<(), AppError> {
            self.delivery_calls.fetch_add(1, Ordering::SeqCst);
            let current = self.current_scope.lock().unwrap().clone();
            if scope != current.as_ref() {
                return Err(AppError::Conflict(
                    "scope barrier probe rejected stale turn scope".into(),
                ));
            }
            self.inject(action).await
        }

        async fn snapshot_context(&self, _max_chars: usize) -> Result<String, AppError> {
            Ok("ctx".into())
        }

        fn is_alive(&self) -> bool {
            true
        }

        async fn describe(&self) -> Result<SessionDescription, AppError> {
            Ok(SessionDescription {
                kind: IdmmTargetKind::Conversation,
                backend: Some("nomi".into()),
                user_id: TEST_USER_ID.into(),
                alive: true,
            })
        }

        async fn pending_signal(&self) -> Option<SessionSignal> {
            if let (Some(entered), Some(release)) =
                (&self.pending_entered, &self.release_pending)
            {
                entered.wait().await;
                release.wait().await;
            }
            self.pending.lock().unwrap().take()
        }
    }

    struct ScriptedCompleter(Mutex<Vec<Result<String, ()>>>);
    #[async_trait]
    impl Completer for ScriptedCompleter {
        async fn complete(&self, _p: &str, _m: &str, _s: &str, _u: &str) -> Result<String, ()> {
            let mut r = self.0.lock().unwrap();
            if r.is_empty() { Err(()) } else { r.remove(0) }
        }
    }
    #[derive(Default)]
    struct MockPrefs(Mutex<std::collections::HashMap<String, String>>);
    #[async_trait]
    impl nomifun_db::IClientPreferenceRepository for MockPrefs {
        async fn get_all(&self) -> Result<Vec<ClientPreference>, DbError> {
            Ok(vec![])
        }
        async fn get_by_keys(&self, keys: &[&str]) -> Result<Vec<ClientPreference>, DbError> {
            let m = self.0.lock().unwrap();
            Ok(keys
                .iter()
                .filter_map(|k| {
                    m.get(*k).map(|v| ClientPreference {
                        id: 0,
                        key: k.to_string(),
                        value: v.clone(),
                        updated_at: 0,
                    })
                })
                .collect())
        }
        async fn upsert_batch(&self, e: &[(&str, &str)]) -> Result<(), DbError> {
            let mut m = self.0.lock().unwrap();
            for (k, v) in e {
                m.insert(k.to_string(), v.to_string());
            }
            Ok(())
        }
        async fn delete_keys(&self, k: &[&str]) -> Result<(), DbError> {
            let mut m = self.0.lock().unwrap();
            for key in k {
                m.remove(*key);
            }
            Ok(())
        }
    }

    #[derive(Default)]
    struct NullBroadcaster;
    impl nomifun_realtime::UserEventSink for NullBroadcaster {
        fn send_to_user(
            &self,
            _user_id: &str,
            _event: nomifun_api_types::WebSocketMessage<serde_json::Value>,
        ) {
        }
    }

    // ── Mock record repo: captures every `insert` so a test can assert the
    //    persisted row's fields. `delete`/`clear_all`/`sweep` are inert; the
    //    list reads echo back the captured inserts. ──
    #[derive(Default)]
    struct RecordingRepo {
        inserted: Mutex<Vec<nomifun_db::models::IdmmInterventionRow>>,
        reservations: Mutex<Vec<IdmmActionReservationRow>>,
        /// When true, every persistence operation fails so action admission can
        /// be verified as fail-closed.
        fail: bool,
    }
    #[async_trait]
    impl nomifun_db::IIdmmInterventionRepository for RecordingRepo {
        async fn insert(
            &self,
            row: &nomifun_db::models::NewIdmmInterventionRow,
        ) -> Result<nomifun_db::models::IdmmInterventionRow, DbError> {
            if self.fail {
                self.inserted.lock().unwrap().push(nomifun_db::models::IdmmInterventionRow {
                    id: 0,
                    intervention_id: row.intervention_id.clone(),
                    user_id: row.user_id.clone(),
                    target_kind: row.target_kind.clone(),
                    target_id: row.target_id.clone(),
                    watch: row.watch.clone(),
                    at: row.at,
                    signal: row.signal.clone(),
                    tier_used: row.tier_used.clone(),
                    category: row.category.clone(),
                    action: row.action.clone(),
                    detail: row.detail.clone(),
                    reason: row.reason.clone(),
                    confidence: row.confidence,
                    bypass_model: row.bypass_model.clone(),
                    outcome: row.outcome.clone(),
                });
                return Err(DbError::Query(sqlx::Error::Protocol("boom".into())));
            }
            let inserted = nomifun_db::models::IdmmInterventionRow {
                id: self.inserted.lock().unwrap().len() as i64 + 1,
                intervention_id: row.intervention_id.clone(),
                user_id: row.user_id.clone(),
                target_kind: row.target_kind.clone(),
                target_id: row.target_id.clone(),
                watch: row.watch.clone(),
                at: row.at,
                signal: row.signal.clone(),
                tier_used: row.tier_used.clone(),
                category: row.category.clone(),
                action: row.action.clone(),
                detail: row.detail.clone(),
                reason: row.reason.clone(),
                confidence: row.confidence,
                bypass_model: row.bypass_model.clone(),
                outcome: row.outcome.clone(),
            };
            self.inserted.lock().unwrap().push(inserted.clone());
            Ok(inserted)
        }
        async fn list_for_target(
            &self,
            _user_id: &str,
            _kind: &str,
            _id: &str,
            _limit: i64,
        ) -> Result<Vec<nomifun_db::models::IdmmInterventionRow>, DbError> {
            Ok(self.inserted.lock().unwrap().clone())
        }
        async fn delete_for_target(
            &self,
            _user_id: &str,
            _kind: &str,
            _id: &str,
        ) -> Result<u64, DbError> {
            Ok(0)
        }
        async fn list_recent(
            &self,
            _user_id: &str,
            _limit: i64,
        ) -> Result<Vec<nomifun_db::models::IdmmInterventionRow>, DbError> {
            Ok(self.inserted.lock().unwrap().clone())
        }
        async fn clear_all(&self, _user_id: &str) -> Result<u64, DbError> {
            Ok(0)
        }
        async fn sweep_all_owners(&self, _cutoff_ms: i64, _per_user_cap: i64) -> Result<u64, DbError> {
            Ok(0)
        }

        async fn reserve_action(
            &self,
            params: &ReserveIdmmActionParams,
        ) -> Result<IdmmActionReserveResult, DbError> {
            if self.fail {
                return Err(DbError::Query(sqlx::Error::Protocol("boom".into())));
            }
            let mut reservations = self.reservations.lock().unwrap();
            if let Some(row) = reservations.iter().find(|row| {
                row.user_id == params.key.user_id
                    && row.conversation_id == params.key.conversation_id
                    && row.turn_id == params.key.turn_id
                    && row.turn_generation
                        == i64::try_from(params.key.turn_generation).unwrap_or(i64::MAX)
                    && row.action_identity == params.key.action_identity
            }) {
                return Ok(if row.status == "reserved" {
                    IdmmActionReserveResult::AlreadyReserved(row.clone())
                } else {
                    IdmmActionReserveResult::Completed(row.clone())
                });
            }
            let row = IdmmActionReservationRow {
                id: reservations.len() as i64 + 1,
                reservation_id: nomifun_common::IdmmInterventionId::new().into_string(),
                user_id: params.key.user_id.clone(),
                conversation_id: params.key.conversation_id.clone(),
                turn_id: params.key.turn_id.clone(),
                turn_generation: i64::try_from(params.key.turn_generation)
                    .map_err(|_| DbError::Init("mock turn generation overflow".into()))?,
                action_identity: params.key.action_identity.clone(),
                status: "reserved".into(),
                settlement_source: None,
                failure_reason: None,
                reserved_at: params.reserved_at,
                settled_at: None,
            };
            reservations.push(row.clone());
            Ok(IdmmActionReserveResult::Reserved(row))
        }

        async fn settle_action(
            &self,
            key: &IdmmActionReservationKey,
            settlement: &IdmmActionSettlement,
            settled_at: i64,
        ) -> Result<IdmmActionSettleResult, DbError> {
            if self.fail {
                return Err(DbError::Query(sqlx::Error::Protocol("boom".into())));
            }
            let mut reservations = self.reservations.lock().unwrap();
            let row = reservations
                .iter_mut()
                .find(|row| {
                    row.user_id == key.user_id
                        && row.conversation_id == key.conversation_id
                        && row.turn_id == key.turn_id
                        && row.turn_generation
                            == i64::try_from(key.turn_generation).unwrap_or(i64::MAX)
                        && row.action_identity == key.action_identity
                })
                .ok_or(DbError::Query(sqlx::Error::RowNotFound))?;
            if row.status != "reserved" {
                return Ok(IdmmActionSettleResult::AlreadySettled(row.clone()));
            }
            let (status, source, failure_reason) = match settlement {
                IdmmActionSettlement::Applied => ("applied", "execution", None),
                IdmmActionSettlement::Failed { reason } => {
                    ("failed", "execution", Some(reason.clone()))
                }
                IdmmActionSettlement::Recovered { reason } => {
                    ("failed", "recovery", Some(reason.clone()))
                }
            };
            row.status = status.into();
            row.settlement_source = Some(source.into());
            row.failure_reason = failure_reason;
            row.settled_at = Some(settled_at);
            Ok(IdmmActionSettleResult::Settled(row.clone()))
        }

        async fn list_reserved_actions_for_turn(
            &self,
            turn: &IdmmActionTurnIdentity,
        ) -> Result<Vec<IdmmActionReservationRow>, DbError> {
            if self.fail {
                return Err(DbError::Query(sqlx::Error::Protocol("boom".into())));
            }
            Ok(self
                .reservations
                .lock()
                .unwrap()
                .iter()
                .filter(|row| {
                    row.user_id == turn.user_id
                        && row.conversation_id == turn.conversation_id
                        && row.turn_id == turn.turn_id
                        && row.turn_generation
                            == i64::try_from(turn.turn_generation).unwrap_or(i64::MAX)
                        && row.status == "reserved"
                })
                .cloned()
                .collect())
        }

        async fn recover_reserved_actions_for_turn(
            &self,
            turn: &IdmmActionTurnIdentity,
            reason: &str,
            settled_at: i64,
        ) -> Result<Vec<IdmmActionReservationRow>, DbError> {
            if self.fail {
                return Err(DbError::Query(sqlx::Error::Protocol("boom".into())));
            }
            let mut reservations = self.reservations.lock().unwrap();
            let mut recovered = Vec::new();
            for row in reservations.iter_mut().filter(|row| {
                row.user_id == turn.user_id
                    && row.conversation_id == turn.conversation_id
                    && row.turn_id == turn.turn_id
                    && row.turn_generation
                        == i64::try_from(turn.turn_generation).unwrap_or(i64::MAX)
                    && row.status == "reserved"
            }) {
                row.status = "failed".into();
                row.settlement_source = Some("recovery".into());
                row.failure_reason = Some(reason.into());
                row.settled_at = Some(settled_at);
                recovered.push(row.clone());
            }
            Ok(recovered)
        }
    }

    fn deps_with(responses: Vec<Result<String, ()>>) -> Arc<LoopDeps> {
        deps_with_records(responses, Arc::new(RecordingRepo::default()))
    }

    /// Like `deps_with`, but lets the caller inject (and later inspect) the
    /// record repo — used by the persistence test.
    fn deps_with_records(responses: Vec<Result<String, ()>>, records: Arc<RecordingRepo>) -> Arc<LoopDeps> {
        let prefs = Arc::new(MockPrefs::default());
        prefs
            .0
            .lock()
            .unwrap()
            .insert(
                crate::sidecar::PREF_BACKUP_PROVIDER.into(),
                TEST_PROVIDER_ID.into(),
            );
        let comp = Arc::new(ScriptedCompleter(Mutex::new(responses)));
        let sidecar = Arc::new(SidecarClient::new(comp, prefs));
        let emitter = IdmmEventEmitter::new(Arc::new(NullBroadcaster));
        Arc::new(LoopDeps {
            sidecar,
            emitter,
            records,
        })
    }

    fn rule_cfg() -> IdmmConfig {
        let mut c = IdmmConfig::default();
        // Both watches enabled, RuleOnly. Low retries so tests escalate/halt fast.
        c.fault_watch.base.enabled = true;
        c.fault_watch.base.tier = WatchTier::RuleOnly;
        c.fault_watch.base.max_retries = 1;
        c.fault_watch.base.budget.min_interval_secs = 0;
        c.decision_watch.base.enabled = true;
        c.decision_watch.base.tier = WatchTier::RuleOnly;
        c.decision_watch.base.max_retries = 1;
        c.decision_watch.base.budget.min_interval_secs = 0;
        c.decision_watch.strategy.categories.option_decision.allow_unmarked_pick = false;
        c
    }
    fn sidecar_cfg() -> IdmmConfig {
        let mut c = IdmmConfig::default();
        c.fault_watch.base.enabled = true;
        c.fault_watch.base.tier = WatchTier::RulePlusModel;
        c.fault_watch.base.max_retries = 1;
        c.fault_watch.base.budget.min_interval_secs = 0;
        c.fault_watch.base.bypass_model = nomifun_api_types::BypassModelRef {
            provider_id: Some(TEST_PROVIDER_ID.into()),
            model: Some("m".into()),
        };
        c.decision_watch.base.enabled = true;
        c.decision_watch.base.tier = WatchTier::RulePlusModel;
        c.decision_watch.base.max_retries = 1;
        c.decision_watch.base.budget.min_interval_secs = 0;
        c.decision_watch.base.bypass_model = nomifun_api_types::BypassModelRef {
            provider_id: Some(TEST_PROVIDER_ID.into()),
            model: Some("m".into()),
        };
        c.decision_watch.strategy.categories.option_decision.allow_unmarked_pick = false;
        c
    }

    fn provider_err() -> SessionSignal {
        SessionSignal::ProviderError {
            code: None,
            retryable: Some(true),
            message: "500".into(),
        }
    }

    fn exact_observed_scope() -> ObservedActionScope {
        ObservedActionScope::Exact(IdmmTurnScope {
            wire_turn_id: CONVERSATION_TURN_ID.into(),
            generation: CONVERSATION_TURN_GENERATION,
        })
    }

    #[tokio::test(start_paused = true)]
    async fn delayed_old_turn_signal_cannot_upgrade_to_successor_scope() {
        let (probe, tx, injected, revalidation_entered, release_revalidation) =
            ScopeBarrierProbe::new(None);
        let records = Arc::new(RecordingRepo::default());
        let deps = deps_with_records(vec![], records.clone());
        let shared = Arc::new(SupervisorShared::default());
        let cancel = Arc::new(AtomicBool::new(false));
        let supervisor = tokio::spawn(run_supervisor(
            probe.clone(),
            rule_cfg(),
            deps,
            shared,
            cancel,
        ));

        tx.send(provider_err()).await.unwrap();
        tokio::time::timeout(Duration::from_secs(30), revalidation_entered.wait())
            .await
            .expect("old-turn action should reach pre-reservation revalidation");

        // The old turn finishes and the user explicitly starts a successor
        // while the old signal's action is delayed.
        probe.replace_with_successor_scope();
        tx.send(SessionSignal::Done).await.unwrap();
        tx.send(SessionSignal::Working).await.unwrap();
        tx.send(SessionSignal::Exited).await.unwrap();
        release_revalidation.wait().await;

        tokio::time::timeout(Duration::from_secs(30), supervisor)
            .await
            .expect("supervisor should drain queued terminal/successor signals")
            .unwrap();

        assert!(
            records.reservations.lock().unwrap().is_empty(),
            "an old signal must not reserve an action against the successor turn"
        );
        assert!(
            injected.lock().unwrap().is_empty(),
            "an old signal must not create a hidden continuation or steer the successor"
        );
        assert_eq!(
            probe.delivery_calls.load(Ordering::SeqCst),
            0,
            "stale scope must be rejected before the delivery boundary"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn delayed_on_arm_pending_signal_cannot_upgrade_to_successor_scope() {
        let pending = SessionSignal::Decision(DecisionPrompt {
            text: "Choose one".into(),
            options: vec!["A".into(), "B".into()],
            recommended: Some("A".into()),
            source: DecisionSource::Permission,
            kind: DecisionKind::Options,
            permission: None,
        });
        let (probe, tx, injected, revalidation_entered, release_revalidation) =
            ScopeBarrierProbe::new(Some(pending));
        let records = Arc::new(RecordingRepo::default());
        let deps = deps_with_records(vec![], records.clone());
        let shared = Arc::new(SupervisorShared::default());
        let cancel = Arc::new(AtomicBool::new(false));
        let supervisor = tokio::spawn(run_supervisor(
            probe.clone(),
            rule_cfg(),
            deps,
            shared,
            cancel,
        ));

        tokio::time::timeout(Duration::from_secs(30), revalidation_entered.wait())
            .await
            .expect("on-arm pending action should reach pre-reservation revalidation");

        probe.replace_with_successor_scope();
        tx.send(SessionSignal::Done).await.unwrap();
        tx.send(SessionSignal::Working).await.unwrap();
        tx.send(SessionSignal::Exited).await.unwrap();
        release_revalidation.wait().await;

        tokio::time::timeout(Duration::from_secs(30), supervisor)
            .await
            .expect("supervisor should finish after stale pending action is absorbed")
            .unwrap();

        assert!(
            records.reservations.lock().unwrap().is_empty(),
            "an on-arm decision from the old turn must not reserve against its successor"
        );
        assert!(
            injected.lock().unwrap().is_empty(),
            "an on-arm decision from the old turn must not reach the successor"
        );
        assert_eq!(probe.delivery_calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test(start_paused = true)]
    async fn queued_old_turn_signals_cannot_capture_successor_scope_on_receive() {
        let old_decision = SessionSignal::Decision(DecisionPrompt {
            text: "Choose one".into(),
            options: vec!["A".into(), "B".into()],
            recommended: Some("A".into()),
            source: DecisionSource::Permission,
            kind: DecisionKind::Options,
            permission: None,
        });

        for old_signal in [provider_err(), old_decision] {
            let (probe, tx, injected, pending_entered, release_pending) =
                ScopeBarrierProbe::with_pending_barrier();
            let records = Arc::new(RecordingRepo::default());
            let deps = deps_with_records(vec![], records.clone());
            let shared = Arc::new(SupervisorShared::default());
            let cancel = Arc::new(AtomicBool::new(false));

            // Queue the old generation's signal before the supervisor is
            // allowed to receive it. The task captures the old exact scope,
            // then pauses in its on-arm pending check.
            tx.send(old_signal).await.unwrap();
            let supervisor = tokio::spawn(run_supervisor(
                probe.clone(),
                rule_cfg(),
                deps,
                shared,
                cancel,
            ));
            tokio::time::timeout(Duration::from_secs(30), pending_entered.wait())
                .await
                .expect("supervisor should bind the old scope before receiving");

            // The old turn finishes and a successor becomes current while the
            // ProviderError/Decision is still queued. A recv-time scope sample
            // would upgrade the old signal here; the per-turn supervisor must
            // retain the old identity and fail revalidation instead.
            probe.replace_with_successor_scope();
            release_pending.wait().await;

            tokio::time::timeout(Duration::from_secs(30), supervisor)
                .await
                .expect("old per-turn supervisor should stand down on scope change")
                .unwrap();
            assert!(
                records.reservations.lock().unwrap().is_empty(),
                "queued old signal must not reserve against the successor"
            );
            assert!(
                injected.lock().unwrap().is_empty(),
                "queued old signal must not inject into the successor"
            );
            assert_eq!(probe.delivery_calls.load(Ordering::SeqCst), 0);
        }
    }

    #[tokio::test(start_paused = true)]
    async fn rule_retry_on_provider_error() {
        let (probe, injected) = MockProbe::new(vec![provider_err()]);
        let deps = deps_with(vec![]);
        let shared = Arc::new(SupervisorShared::default());
        let cancel = Arc::new(AtomicBool::new(false));
        // Run to completion (probe sends Exited after the scripted signals).
        run_supervisor(probe, rule_cfg(), deps, shared.clone(), cancel).await;
        let inj = injected.lock().unwrap();
        assert_eq!(inj.len(), 1);
        assert_eq!(inj[0], WakeAction::Retry);
        assert_eq!(shared.count.load(Ordering::Relaxed), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn fault_watch_failover_queue_injects_failover_action() {
        // D6 end-to-end through the supervisor: a fault watch that opts into the
        // model failover queue turns a provider error into a Failover inject (the
        // probe — here MockProbe — receives WakeAction::Failover and would route
        // it to the conversation service's shared failover helper). Without the
        // flag this same error is a plain Retry (see `rule_retry_on_provider_error`).
        let mut cfg = rule_cfg();
        cfg.fault_watch.use_failover_queue = true;
        let (probe, injected) = MockProbe::new(vec![provider_err()]);
        let deps = deps_with(vec![]);
        let shared = Arc::new(SupervisorShared::default());
        let cancel = Arc::new(AtomicBool::new(false));
        run_supervisor(probe, cfg, deps, shared.clone(), cancel).await;
        let inj = injected.lock().unwrap();
        assert_eq!(inj.len(), 1);
        assert_eq!(inj[0], WakeAction::Failover, "use_failover_queue must inject Failover, not Retry");
        assert_eq!(shared.count.load(Ordering::Relaxed), 1);
    }

    // ── Decision records persist to the repo with the right field values ──

    #[tokio::test(start_paused = true)]
    async fn intervention_persists_row_with_enriched_fields() {
        // A sidecar-tier chat decision: the model answers a choice. The emitted
        // record must be written to the repo with a local technical id,
        // target_kind=conversation, watch=decision (a decision, not a fault),
        // tier=sidecar, the chosen text in `detail`, the model's confidence, a
        // bypass_model, and outcome=applied.
        let (probe, _injected) = MockProbe::new(vec![chat_decision_signal()]);
        let records = Arc::new(RecordingRepo::default());
        let deps = deps_with_records(
            vec![Ok(
                r#"{"action":"answer_choice","text":"2) 方案B","confidence":0.82,"reason":"B 更稳"}"#.into(),
            )],
            records.clone(),
        );
        let shared = Arc::new(SupervisorShared::default());
        let cancel = Arc::new(AtomicBool::new(false));
        run_supervisor(probe, sidecar_cfg(), deps, shared, cancel).await;

        let rows = records.inserted.lock().unwrap();
        assert_eq!(rows.len(), 1, "exactly one intervention should be persisted; got {rows:?}");
        let row = &rows[0];
        assert!(
            nomifun_common::IdmmInterventionId::parse(&row.intervention_id).is_ok(),
            "the repository must preserve a canonical UUIDv7 business id"
        );
        assert_eq!(row.user_id, TEST_USER_ID);
        assert_eq!(row.target_kind, "conversation");
        assert_eq!(row.target_id, CONVERSATION_TARGET_ID);
        assert_eq!(row.watch, "decision");
        assert_eq!(row.signal, "decision");
        assert_eq!(row.tier_used, "sidecar");
        assert_eq!(row.category.as_deref(), Some("option"));
        assert_eq!(row.action, "answer_choice");
        assert_eq!(row.detail.as_deref(), Some("2) 方案B"));
        assert_eq!(row.reason.as_deref(), Some("B 更稳"));
        assert_eq!(row.confidence, Some(0.82_f32 as f64));
        assert_eq!(row.bypass_model.as_deref(), Some(TEST_BYPASS_MODEL));
        assert_eq!(row.outcome, "applied");
    }

    #[tokio::test(start_paused = true)]
    async fn fault_record_uses_fault_watch() {
        // A provider error → rule retry. The persisted row's watch lane is
        // `fault`, signal `provider_error`, tier `rule`.
        let (probe, _injected) = MockProbe::new(vec![provider_err()]);
        let records = Arc::new(RecordingRepo::default());
        let deps = deps_with_records(vec![], records.clone());
        let shared = Arc::new(SupervisorShared::default());
        let cancel = Arc::new(AtomicBool::new(false));
        run_supervisor(probe, rule_cfg(), deps, shared, cancel).await;

        let rows = records.inserted.lock().unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].watch, "fault");
        assert_eq!(rows[0].signal, "provider_error");
        assert_eq!(rows[0].tier_used, "rule");
        assert_eq!(rows[0].action, "retry");
    }

    #[tokio::test(start_paused = true)]
    async fn idle_recovers_pending_decision_missed_at_arm() {
        // REGRESSION (中途开启 IDMM 不生效): when armed mid-turn, observe() only sees
        // FUTURE events — if the decision's menu text streamed BEFORE subscribe,
        // the live Finish carries an empty turn_text → Done (the decision is
        // missed), and the on-arm pending_signal ran too early (turn not yet
        // finished). The agent then goes idle. On Idle the supervisor must
        // RE-CHECK the conversation's current pending decision and answer it —
        // not merely nudge "continue". Scripted pending_signal: None at arm, then
        // the decision at the Idle tick.
        let decision = SessionSignal::Decision(DecisionPrompt {
            text: "选哪个方案? (1/2)".into(),
            options: vec!["1) 方案A".into(), "2) 方案B".into()],
            recommended: Some("1) 方案A".into()),
            source: DecisionSource::TextScan,
            kind: DecisionKind::Options,
            permission: None,
        });
        let (probe, injected) = MockProbe::new(vec![SessionSignal::Working, SessionSignal::Idle]);
        let probe = probe.with_pending_seq(vec![None, Some(decision)]);
        // A sidecar response is available as a fallback, but the recommended
        // option means the rule tier answers directly (AnswerChoice).
        let deps = deps_with(vec![Ok(r#"{"action":"answer_choice","text":"1) 方案A","confidence":0.9}"#.into())]);
        let shared = Arc::new(SupervisorShared::default());
        let cancel = Arc::new(AtomicBool::new(false));
        run_supervisor(probe, sidecar_cfg(), deps, shared, cancel).await;

        let inj = injected.lock().unwrap();
        assert!(
            inj.iter().any(|a| matches!(a, WakeAction::AnswerChoice(_))),
            "on Idle the supervisor must recover + answer the pending decision (mid-turn arm), not just nudge; got {inj:?}"
        );
        assert!(
            !inj.iter().any(|a| matches!(a, WakeAction::SendText(t) if t == "continue")),
            "must answer the decision, not blindly nudge 'continue'; got {inj:?}"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn idle_with_no_pending_decision_still_nudges() {
        // GUARD: when there is genuinely NO pending decision, an Idle after work
        // still nudges "continue" (the recovery is additive, not a replacement).
        let (probe, injected) = MockProbe::new(vec![SessionSignal::Working, SessionSignal::Idle]);
        // pending_signal returns None at arm and at the idle tick.
        let probe = probe.with_pending_seq(vec![None, None]);
        let deps = deps_with(vec![]);
        let shared = Arc::new(SupervisorShared::default());
        let cancel = Arc::new(AtomicBool::new(false));
        run_supervisor(probe, rule_cfg(), deps, shared, cancel).await;
        let inj = injected.lock().unwrap();
        assert!(
            inj.iter().any(|a| matches!(a, WakeAction::SendText(t) if t == "continue")),
            "a real idle with nothing pending must still nudge 'continue'; got {inj:?}"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn idle_after_done_does_not_recover_a_late_pending_decision() {
        // A clean Done is authoritative even if a stale decision appears during
        // the following idle recovery scan. Reinterpreting it as live authority
        // used to inject a hidden answer and restart completed work.
        let decision = SessionSignal::Decision(DecisionPrompt {
            text: "选哪个? (1/2)".into(),
            options: vec!["1) A".into(), "2) B".into()],
            recommended: Some("1) A".into()),
            source: DecisionSource::TextScan,
            kind: DecisionKind::Options,
            permission: None,
        });
        let (probe, injected) =
            MockProbe::new(vec![SessionSignal::Working, SessionSignal::Done, SessionSignal::Idle]);
        let probe = probe.with_pending_seq(vec![None, Some(decision)]);
        let deps = deps_with(vec![]);
        let shared = Arc::new(SupervisorShared::default());
        let cancel = Arc::new(AtomicBool::new(false));
        run_supervisor(probe, sidecar_cfg(), deps, shared, cancel).await;
        assert!(
            injected.lock().unwrap().is_empty(),
            "no recovered decision may cross a clean Done boundary"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn halt_record_uses_canonical_outcome() {
        // An unmarked rule-only decision with no auto-pick/sidecar halts. The
        // persisted row's `outcome` must be the canonical "halted" token (so the
        // UI badge/enum renders), NOT the free-form halt reason — that belongs in
        // `reason`. Regression guard for the outcome-contract fix.
        let (probe, injected) = MockProbe::new(vec![chat_decision_signal()]);
        let records = Arc::new(RecordingRepo::default());
        let deps = deps_with_records(vec![], records.clone());
        let shared = Arc::new(SupervisorShared::default());
        let cancel = Arc::new(AtomicBool::new(false));
        run_supervisor(probe, rule_cfg(), deps, shared, cancel).await;

        assert!(injected.lock().unwrap().is_empty(), "an unmarked rule-only decision must halt, not inject");
        let rows = records.inserted.lock().unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].outcome, "halted", "outcome must be the canonical token");
        assert_eq!(rows[0].action, "stop");
        assert!(rows[0].reason.is_some(), "the free-form halt reason belongs in `reason`");
        assert_ne!(
            rows[0].outcome,
            rows[0].reason.clone().unwrap_or_default(),
            "the descriptive reason must not leak into `outcome`"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn reservation_persistence_failure_is_fail_closed() {
        // No Conversation action may be injected when durable recovery or
        // reservation persistence is unavailable.
        let (probe, injected) = MockProbe::new(vec![provider_err()]);
        let records = Arc::new(RecordingRepo {
            fail: true,
            ..Default::default()
        });
        let deps = deps_with_records(vec![], records.clone());
        let shared = Arc::new(SupervisorShared::default());
        let cancel = Arc::new(AtomicBool::new(false));
        run_supervisor(probe, rule_cfg(), deps, shared.clone(), cancel).await;
        // A best-effort failed-outcome audit was attempted, but admission
        // failed before delivery and no reservation row was created.
        assert_eq!(records.inserted.lock().unwrap().len(), 1);
        assert!(records.reservations.lock().unwrap().is_empty());
        assert!(injected.lock().unwrap().is_empty());
        assert_eq!(shared.count.load(Ordering::Relaxed), 0);
    }

    #[tokio::test(start_paused = true)]
    async fn terminal_effect_actions_are_skipped_before_probe_injection() {
        let (probe, injected) =
            MockProbe::with_kind(vec![], IdmmTargetKind::Terminal);
        let probe: Arc<dyn SessionProbe> = probe;
        let records = Arc::new(RecordingRepo::default());
        let deps = deps_with_records(vec![], records.clone());
        let signal = provider_err();

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
            let result = apply_action(
                &probe,
                &deps,
                TEST_USER_ID,
                IdmmTargetKind::Terminal,
                TERMINAL_TARGET_ID,
                &signal,
                &action,
                &ObservedActionScope::Unsupported,
            )
            .await;
            assert!(
                matches!(result, ActionApplication::Skipped { .. }),
                "terminal action {action:?} must be skipped"
            );
        }

        assert!(
            injected.lock().unwrap().is_empty(),
            "unscoped terminal actions must never reach SessionProbe::inject"
        );
        assert!(
            records.reservations.lock().unwrap().is_empty(),
            "Conversation reservations cannot be forged for a Terminal"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn duplicate_exact_turn_action_is_durably_absorbed() {
        let (probe, injected) = MockProbe::new(vec![]);
        let probe: Arc<dyn SessionProbe> = probe;
        let records = Arc::new(RecordingRepo::default());
        let deps = deps_with_records(vec![], records.clone());
        let signal = provider_err();

        let first = apply_action(
            &probe,
            &deps,
            TEST_USER_ID,
            IdmmTargetKind::Conversation,
            CONVERSATION_TARGET_ID,
            &signal,
            &WakeAction::Retry,
            &exact_observed_scope(),
        )
        .await;
        let duplicate = apply_action(
            &probe,
            &deps,
            TEST_USER_ID,
            IdmmTargetKind::Conversation,
            CONVERSATION_TARGET_ID,
            &signal,
            &WakeAction::Retry,
            &exact_observed_scope(),
        )
        .await;

        assert!(matches!(first, ActionApplication::Applied { .. }));
        assert!(matches!(duplicate, ActionApplication::Absorbed));
        assert_eq!(
            injected.lock().unwrap().as_slice(),
            &[WakeAction::Retry],
            "the same canonical action may cross an exact-turn boundary only once"
        );
        let reservations = records.reservations.lock().unwrap();
        assert_eq!(reservations.len(), 1);
        assert_eq!(reservations[0].status, "applied");
    }

    #[tokio::test(start_paused = true)]
    async fn ambiguous_reserved_action_is_recovered_without_redelivery() {
        let (probe, injected) = MockProbe::new(vec![]);
        let probe: Arc<dyn SessionProbe> = probe;
        let records = Arc::new(RecordingRepo::default());
        let signal = provider_err();
        let action_identity = canonical_action_identity(&signal, &WakeAction::Retry);
        records
            .reservations
            .lock()
            .unwrap()
            .push(IdmmActionReservationRow {
                id: 1,
                reservation_id: nomifun_common::IdmmInterventionId::new().into_string(),
                user_id: TEST_USER_ID.into(),
                conversation_id: CONVERSATION_TARGET_ID.into(),
                turn_id: CONVERSATION_TURN_ID.into(),
                turn_generation: CONVERSATION_TURN_GENERATION as i64,
                action_identity,
                status: "reserved".into(),
                settlement_source: None,
                failure_reason: None,
                reserved_at: now_ms() - 1,
                settled_at: None,
            });
        let deps = deps_with_records(vec![], records.clone());

        let result = apply_action(
            &probe,
            &deps,
            TEST_USER_ID,
            IdmmTargetKind::Conversation,
            CONVERSATION_TARGET_ID,
            &signal,
            &WakeAction::Retry,
            &exact_observed_scope(),
        )
        .await;

        assert!(matches!(result, ActionApplication::Absorbed));
        assert!(
            injected.lock().unwrap().is_empty(),
            "crash ambiguity is terminal and must never be redelivered"
        );
        let reservations = records.reservations.lock().unwrap();
        assert_eq!(reservations.len(), 1);
        assert_eq!(reservations[0].status, "failed");
        assert_eq!(
            reservations[0].settlement_source.as_deref(),
            Some("recovery")
        );
        assert!(reservations[0].settled_at.is_some());
    }

    #[tokio::test(start_paused = true)]
    async fn escalates_to_sidecar_and_applies_decision() {
        // Two provider errors: first → rule retry (max_retries=1), second → sidecar.
        let (probe, injected) = MockProbe::new(vec![provider_err(), provider_err()]);
        let deps = deps_with(vec![Ok(
            r#"{"action":"send_text","text":"do the thing","confidence":0.9}"#.into(),
        )]);
        let shared = Arc::new(SupervisorShared::default());
        let cancel = Arc::new(AtomicBool::new(false));
        run_supervisor(probe, sidecar_cfg(), deps, shared.clone(), cancel).await;
        let inj = injected.lock().unwrap();
        // retry, then sidecar's send_text
        assert!(inj.contains(&WakeAction::Retry));
        assert!(
            inj.iter()
                .any(|a| matches!(a, WakeAction::SendText(t) if t == "do the thing"))
        );
    }

    #[tokio::test(start_paused = true)]
    async fn sidecar_failure_triggers_rule_fallback() {
        let (probe, injected) = MockProbe::new(vec![provider_err(), provider_err()]);
        let deps = deps_with(vec![Err(())]); // provider fails
        let shared = Arc::new(SupervisorShared::default());
        let cancel = Arc::new(AtomicBool::new(false));
        run_supervisor(probe, sidecar_cfg(), deps, shared, cancel).await;
        let inj = injected.lock().unwrap();
        // fallback is conservative Retry
        assert!(inj.iter().filter(|a| **a == WakeAction::Retry).count() >= 1);
    }

    // ── Chat decision (TextScan) flows through the supervisor → answer ──

    fn chat_decision_signal() -> SessionSignal {
        SessionSignal::Decision(DecisionPrompt {
            text: "请选择一个方案？".into(),
            options: vec!["1) 方案A".into(), "2) 方案B".into()],
            recommended: None,
            source: DecisionSource::TextScan,
            kind: DecisionKind::Options,
            permission: None,
        })
    }

    #[tokio::test(start_paused = true)]
    async fn chat_decision_rule_autopick_injects_first_safe_answer() {
        // Rule tier (no sidecar) + auto_pick_unmarked: a desktop "方案 1/2/3"
        // decision is answered with the first safe option — no human, no model.
        let (probe, injected) = MockProbe::new(vec![chat_decision_signal()]);
        let deps = deps_with(vec![]);
        let shared = Arc::new(SupervisorShared::default());
        let cancel = Arc::new(AtomicBool::new(false));
        let mut cfg = rule_cfg();
        cfg.decision_watch.strategy.categories.option_decision.allow_unmarked_pick = true;
        run_supervisor(probe, cfg, deps, shared.clone(), cancel).await;
        let inj = injected.lock().unwrap();
        assert_eq!(
            inj.as_slice(),
            &[WakeAction::AnswerChoice("1) 方案A".into())],
            "rule auto-pick must answer the chat decision with the first safe option"
        );
        assert_eq!(shared.count.load(Ordering::Relaxed), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn chat_decision_sidecar_answers_choice() {
        // Sidecar tier: an unmarked chat decision escalates to the backup model,
        // which returns answer_choice → injected as the reply.
        let (probe, injected) = MockProbe::new(vec![chat_decision_signal()]);
        let deps = deps_with(vec![Ok(
            r#"{"action":"answer_choice","text":"2) 方案B","confidence":0.9}"#.into(),
        )]);
        let shared = Arc::new(SupervisorShared::default());
        let cancel = Arc::new(AtomicBool::new(false));
        run_supervisor(probe, sidecar_cfg(), deps, shared, cancel).await;
        let inj = injected.lock().unwrap();
        assert!(
            inj.iter().any(|a| matches!(a, WakeAction::AnswerChoice(t) if t == "2) 方案B")),
            "sidecar's answer_choice must be injected; got {inj:?}"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn chat_decision_rule_only_no_autopick_halts() {
        // Default rule tier, auto_pick_unmarked off, no sidecar: an unmarked
        // decision still halts to the human (regression guard — the fix never
        // silently answers when the user hasn't opted into auto-pick/sidecar).
        let (probe, injected) = MockProbe::new(vec![
            chat_decision_signal(),
            SessionSignal::Working,
            SessionSignal::Idle,
        ]);
        let deps = deps_with(vec![]);
        let shared = Arc::new(SupervisorShared::default());
        let cancel = Arc::new(AtomicBool::new(false));
        run_supervisor(probe, rule_cfg(), deps, shared, cancel).await;
        assert!(
            injected.lock().unwrap().is_empty(),
            "an unmarked decision with no auto-pick/sidecar must halt, not inject"
        );
    }

    // ── On-arm pending-decision scan (armed AFTER the agent already asked) ──

    #[tokio::test(start_paused = true)]
    async fn on_arm_pending_decision_fires_once_before_stream() {
        // The bug: the user enables the decision watch AFTER the agent already
        // asked a numbered-option question and the turn ended. `observe()` is a
        // fresh subscriber that only sees FUTURE events, so the already-emitted
        // turn-end decision is never replayed → the dot is armed but nothing
        // happens. The fix evaluates the conversation's CURRENT pending decision
        // ONCE at arm (before the loop). With decision_watch enabled + RuleOnly
        // auto_pick, that on-arm decision must be answered exactly once — here
        // the scripted stream itself carries NO signal (only Exited), so the
        // single answer can only have come from the on-arm scan.
        let (probe, injected) = MockProbe::new(vec![]);
        let probe = probe.with_pending(chat_decision_signal());
        let deps = deps_with(vec![]);
        let shared = Arc::new(SupervisorShared::default());
        let cancel = Arc::new(AtomicBool::new(false));
        let mut cfg = rule_cfg();
        cfg.decision_watch.strategy.categories.option_decision.allow_unmarked_pick = true;
        run_supervisor(probe, cfg, deps, shared.clone(), cancel).await;
        let inj = injected.lock().unwrap();
        assert_eq!(
            inj.as_slice(),
            &[WakeAction::AnswerChoice("1) 方案A".into())],
            "the on-arm pending decision must be answered exactly once before any streamed signal"
        );
        assert_eq!(shared.count.load(Ordering::Relaxed), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn on_arm_pending_decision_not_consulted_when_decision_watch_disabled() {
        // The on-arm scan is gated on the DECISION watch being enabled — a
        // fault-only configuration must NOT consult `pending_signal` (the
        // pending-decision lane is off), so even a seeded pending decision
        // produces no on-arm action.
        let (probe, injected) = MockProbe::new(vec![]);
        let probe = probe.with_pending(chat_decision_signal());
        let deps = deps_with(vec![]);
        let shared = Arc::new(SupervisorShared::default());
        let cancel = Arc::new(AtomicBool::new(false));
        let mut cfg = rule_cfg();
        // Only the fault watch is on; the decision watch (the pending-decision
        // lane) is disabled.
        cfg.decision_watch.base.enabled = false;
        cfg.decision_watch.strategy.categories.option_decision.allow_unmarked_pick = true;
        run_supervisor(probe, cfg, deps, shared.clone(), cancel).await;
        assert!(
            injected.lock().unwrap().is_empty(),
            "decision watch disabled → the on-arm pending-decision scan must not run"
        );
        assert_eq!(shared.count.load(Ordering::Relaxed), 0);
    }

    #[tokio::test(start_paused = true)]
    async fn progress_resets_then_idle_nudges() {
        // Working seeds work-in-progress → first Idle is a real stall and
        // produces ONE nudge. The trailing Working would re-arm WIP for a
        // follow-up Idle, but the scripted stream ends with Exited.
        let (probe, injected) = MockProbe::new(vec![SessionSignal::Working, SessionSignal::Idle, SessionSignal::Working]);
        let deps = deps_with(vec![]);
        let shared = Arc::new(SupervisorShared::default());
        let cancel = Arc::new(AtomicBool::new(false));
        run_supervisor(probe, rule_cfg(), deps, shared, cancel).await;
        let inj = injected.lock().unwrap();
        let nudges = inj
            .iter()
            .filter(|a| matches!(a, WakeAction::SendText(t) if t == "continue"))
            .count();
        assert_eq!(nudges, 1);
    }

    // ── Req3: normal-stop guard at the supervisor seam ──

    #[tokio::test(start_paused = true)]
    async fn idle_after_done_is_not_nudged() {
        // Working → Done → Idle: clean turn, then a benign idle. No nudge.
        let (probe, injected) = MockProbe::new(vec![
            SessionSignal::Working,
            SessionSignal::Done,
            SessionSignal::Idle,
        ]);
        let deps = deps_with(vec![]);
        let shared = Arc::new(SupervisorShared::default());
        let cancel = Arc::new(AtomicBool::new(false));
        run_supervisor(probe, rule_cfg(), deps, shared.clone(), cancel).await;
        let inj = injected.lock().unwrap();
        assert!(
            inj.iter()
                .all(|a| !matches!(a, WakeAction::SendText(t) if t == "continue")),
            "must not write 'continue' after a clean Done; got injected={inj:?}"
        );
        // No intervention recorded for the benign idle.
        assert_eq!(
            shared.count.load(Ordering::Relaxed),
            0,
            "Standby must not record an intervention"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn idle_after_working_without_done_still_nudges() {
        // Working → Idle (no Done): work-in-progress went silent → nudge.
        let (probe, injected) = MockProbe::new(vec![SessionSignal::Working, SessionSignal::Idle]);
        let deps = deps_with(vec![]);
        let shared = Arc::new(SupervisorShared::default());
        let cancel = Arc::new(AtomicBool::new(false));
        run_supervisor(probe, rule_cfg(), deps, shared, cancel).await;
        let inj = injected.lock().unwrap();
        assert!(
            inj.iter()
                .any(|a| matches!(a, WakeAction::SendText(t) if t == "continue")),
            "expected a 'continue' nudge after Working→Idle; got injected={inj:?}"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn terminal_idle_after_working_is_skipped_without_exact_scope() {
        let (probe, injected) = MockProbe::with_kind(
            vec![SessionSignal::Working, SessionSignal::Idle, SessionSignal::Idle],
            IdmmTargetKind::Terminal,
        );
        let deps = deps_with(vec![]);
        let shared = Arc::new(SupervisorShared::default());
        let cancel = Arc::new(AtomicBool::new(false));
        run_supervisor(probe, rule_cfg(), deps, shared, cancel).await;
        assert!(
            injected.lock().unwrap().is_empty(),
            "terminal Working→Idle has no exact durable action scope and must perform zero injection"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn provider_error_after_done_is_absorbed() {
        // Provider transports may deliver an error after their clean finish.
        // It belongs to the completed turn and must never trigger Retry.
        let (probe, injected) = MockProbe::new(vec![
            SessionSignal::Working,
            SessionSignal::Done,
            provider_err(),
        ]);
        let deps = deps_with(vec![]);
        let shared = Arc::new(SupervisorShared::default());
        let cancel = Arc::new(AtomicBool::new(false));
        run_supervisor(probe, rule_cfg(), deps, shared.clone(), cancel).await;
        assert!(
            injected.lock().unwrap().is_empty(),
            "a late provider error must not restart a completed turn"
        );
        assert_eq!(
            shared.count.load(Ordering::Relaxed),
            0,
            "absorbed late faults must not be recorded as interventions"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn queued_working_after_done_cannot_rearm_finished_conversation_supervisor() {
        let (probe, injected) = MockProbe::new(vec![
            SessionSignal::Working,
            SessionSignal::Done,
            SessionSignal::Working,
            provider_err(),
        ]);
        let deps = deps_with(vec![]);
        let shared = Arc::new(SupervisorShared::default());
        let cancel = Arc::new(AtomicBool::new(false));
        run_supervisor(probe, rule_cfg(), deps, shared, cancel).await;
        assert!(
            injected.lock().unwrap().is_empty(),
            "Done terminates the exact-turn supervisor; queued successor-like signals cannot re-arm it"
        );
    }

    // ── User-cancel stand-down + halt actually stops the loop ──

    #[tokio::test(start_paused = true)]
    async fn user_cancel_suppresses_trailing_stalls() {
        // The user stops the turn (Cancelled). The stopped turn's trailing
        // error and idle must NOT be "recovered" — injecting a hidden
        // "continue" here was the "I paused it and it started running again"
        // bug. Zero injections expected.
        let (probe, injected) = MockProbe::new(vec![
            SessionSignal::Working,
            SessionSignal::Cancelled,
            provider_err(),
            SessionSignal::Idle,
        ]);
        let deps = deps_with(vec![]);
        let shared = Arc::new(SupervisorShared::default());
        let cancel = Arc::new(AtomicBool::new(false));
        run_supervisor(probe, rule_cfg(), deps, shared, cancel).await;
        assert!(
            injected.lock().unwrap().is_empty(),
            "no intervention may follow a user cancel until new work starts"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn halt_stops_the_supervisor_loop() {
        // max_retries=1 (rule_cfg): error #1 → Retry, error #2 → Halt. The
        // halt must END supervision — historically it only logged, the loop
        // stayed armed, and later signals kept triggering interventions
        // (Working+Idle here would have nudged "continue"). After the break,
        // no further signal may produce an inject.
        let (probe, injected) = MockProbe::new(vec![
            provider_err(),
            provider_err(),
            SessionSignal::Working,
            SessionSignal::Idle,
        ]);
        let deps = deps_with(vec![]);
        let shared = Arc::new(SupervisorShared::default());
        let cancel = Arc::new(AtomicBool::new(false));
        run_supervisor(probe, rule_cfg(), deps, shared, cancel).await;
        let inj = injected.lock().unwrap();
        assert_eq!(
            inj.as_slice(),
            &[WakeAction::Retry],
            "exactly the pre-halt retry; nothing after the halt"
        );
    }

    // ── Handle lifecycle: a naturally-exited supervisor must not read as live ──

    struct FixedProbeFactory(Arc<MockProbe>);
    impl ProbeFactory for FixedProbeFactory {
        fn build(&self, _kind: IdmmTargetKind, _target_id: &str) -> Option<Arc<dyn SessionProbe>> {
            Some(self.0.clone())
        }
    }

    struct LiveConversationProbe {
        receiver: Mutex<Option<mpsc::Receiver<SessionSignal>>>,
        _sender: mpsc::Sender<SessionSignal>,
    }

    impl LiveConversationProbe {
        fn new() -> Arc<Self> {
            let (sender, receiver) = mpsc::channel(4);
            Arc::new(Self {
                receiver: Mutex::new(Some(receiver)),
                _sender: sender,
            })
        }
    }

    #[async_trait]
    impl SessionProbe for LiveConversationProbe {
        fn target(&self) -> (IdmmTargetKind, String) {
            (
                IdmmTargetKind::Conversation,
                CONVERSATION_TARGET_ID.into(),
            )
        }

        fn observe(&self, _idle: Duration) -> mpsc::Receiver<SessionSignal> {
            self.receiver
                .lock()
                .unwrap()
                .take()
                .expect("fresh live probe is observed once")
        }

        async fn inject(&self, _action: &WakeAction) -> Result<(), AppError> {
            Err(AppError::Conflict(
                "live generation-order probe does not inject".into(),
            ))
        }

        async fn action_scope(&self) -> Result<Option<IdmmTurnScope>, AppError> {
            Ok(None)
        }

        async fn snapshot_context(&self, _max_chars: usize) -> Result<String, AppError> {
            Ok(String::new())
        }

        fn is_alive(&self) -> bool {
            true
        }

        async fn describe(&self) -> Result<SessionDescription, AppError> {
            Ok(SessionDescription {
                kind: IdmmTargetKind::Conversation,
                backend: Some("nomi".into()),
                user_id: TEST_USER_ID.into(),
                alive: true,
            })
        }
    }

    struct FreshLiveProbeFactory;

    impl ProbeFactory for FreshLiveProbeFactory {
        fn build(
            &self,
            kind: IdmmTargetKind,
            _target_id: &str,
        ) -> Option<Arc<dyn SessionProbe>> {
            (kind == IdmmTargetKind::Conversation)
                .then(|| LiveConversationProbe::new() as Arc<dyn SessionProbe>)
        }
    }

    struct DomainProbeFactory {
        conversation: Arc<MockProbe>,
        terminal: Arc<MockProbe>,
    }

    impl ProbeFactory for DomainProbeFactory {
        fn build(&self, kind: IdmmTargetKind, _target_id: &str) -> Option<Arc<dyn SessionProbe>> {
            Some(match kind {
                IdmmTargetKind::Conversation => self.conversation.clone(),
                IdmmTargetKind::Terminal => self.terminal.clone(),
            })
        }
    }

    struct EnabledConfigReader(IdmmConfig);
    #[async_trait]
    impl ConfigReader for EnabledConfigReader {
        async fn read(
            &self,
            _user_id: &str,
            _kind: IdmmTargetKind,
            _target_id: &str,
        ) -> Result<IdmmConfig, AppError> {
            Ok(self.0.clone())
        }
    }

    #[tokio::test]
    async fn stale_turn_hook_cannot_cancel_or_replace_newer_supervisor() {
        let manager = IdmmManager::new(
            deps_with(vec![]),
            Arc::new(FreshLiveProbeFactory),
            Arc::new(EnabledConfigReader(rule_cfg())),
        );
        let old_scope = IdmmTurnScope {
            wire_turn_id: CONVERSATION_TURN_ID.into(),
            generation: CONVERSATION_TURN_GENERATION,
        };
        let successor_scope = IdmmTurnScope {
            wire_turn_id: "0190f5fe-7c00-7a00-8000-000000000005".into(),
            generation: CONVERSATION_TURN_GENERATION + 1,
        };

        manager
            .inner
            .replace_conversation_turn(CONVERSATION_TARGET_ID, old_scope.clone())
            .await;
        manager
            .inner
            .replace_conversation_turn(CONVERSATION_TARGET_ID, successor_scope.clone())
            .await;
        tokio::task::yield_now().await;

        let key = (
            IdmmTargetKind::Conversation,
            CONVERSATION_TARGET_ID.to_owned(),
        );
        let successor_handle_generation = {
            let handle = manager.inner.handles.get(&key).expect("successor handle");
            assert_eq!(
                handle.admitted_turn_generation,
                Some(successor_scope.generation)
            );
            assert!(!handle.join.is_finished(), "successor supervisor stays live");
            handle.generation
        };

        // Simulate a delayed fire-and-forget hook for the old turn resuming
        // after the successor is already installed.
        manager
            .inner
            .replace_conversation_turn(CONVERSATION_TARGET_ID, old_scope)
            .await;
        tokio::task::yield_now().await;

        let handle = manager
            .inner
            .handles
            .get(&key)
            .expect("newer supervisor must survive stale hook");
        assert_eq!(
            handle.admitted_turn_generation,
            Some(successor_scope.generation)
        );
        assert_eq!(
            handle.generation, successor_handle_generation,
            "stale hook and old cleanup guard must not replace/remove the winner"
        );
        assert!(!handle.join.is_finished());
    }

    #[tokio::test(start_paused = true)]
    async fn exited_supervisor_cleans_up_its_handle() {
        // The scripted probe sends only Exited → run_supervisor breaks
        // immediately. The handle must leave the map (cleanup guard) so
        // is_supervising goes false — a stale `true` made AutoWork wait for
        // an IDMM recovery that could never come AND blocked every re-arm.
        let (conversation_probe, _injected) = MockProbe::new(vec![]);
        let (terminal_probe, _injected) =
            MockProbe::with_kind(vec![], IdmmTargetKind::Terminal);
        let manager = IdmmManager::new(
            deps_with(vec![]),
            Arc::new(DomainProbeFactory {
                conversation: conversation_probe,
                terminal: terminal_probe,
            }),
            Arc::new(EnabledConfigReader(rule_cfg())),
        );
        manager
            .ensure(IdmmTargetKind::Conversation, CONVERSATION_TARGET_ID)
            .await;
        // Let the supervisor task run to its natural exit and clean up.
        for _ in 0..100 {
            if !manager.is_supervising(IdmmTargetKind::Conversation, CONVERSATION_TARGET_ID) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(
            !manager.is_supervising(IdmmTargetKind::Conversation, CONVERSATION_TARGET_ID),
            "a naturally-exited supervisor must not be reported as supervising"
        );
    }

    // ── C3 (spec §2.2): cross-domain supervisor isolation ───────────────────
    //
    // Conversation and terminal IDs occupy distinct canonical domains. The
    // supervisor handle/shared maps still key on `(kind, target_id)`, so both
    // domains remain independently addressable and stopping one never tears
    // down the other.

    #[tokio::test(start_paused = true)]
    async fn c3_conversation_and_terminal_are_supervised_independently() {
        // Time is paused, so the spawned supervisor tasks do not advance to
        // their Exited cleanup during the assertions — both handles stay live.
        let (conversation_probe, _injected) = MockProbe::new(vec![]);
        let (terminal_probe, _injected) =
            MockProbe::with_kind(vec![], IdmmTargetKind::Terminal);
        let manager = IdmmManager::new(
            deps_with(vec![]),
            Arc::new(DomainProbeFactory {
                conversation: conversation_probe,
                terminal: terminal_probe,
            }),
            Arc::new(EnabledConfigReader(rule_cfg())),
        );

        manager
            .ensure(IdmmTargetKind::Conversation, CONVERSATION_TARGET_ID)
            .await;
        manager
            .ensure(IdmmTargetKind::Terminal, TERMINAL_TARGET_ID)
            .await;

        assert!(
            manager.is_supervising(IdmmTargetKind::Conversation, CONVERSATION_TARGET_ID),
            "conversation supervised"
        );
        assert!(
            manager.is_supervising(IdmmTargetKind::Terminal, TERMINAL_TARGET_ID),
            "terminal supervised independently"
        );

        // Stop the conversation domain. The terminal supervisor must remain.
        manager.stop(IdmmTargetKind::Conversation, CONVERSATION_TARGET_ID);
        assert!(
            !manager.is_supervising(IdmmTargetKind::Conversation, CONVERSATION_TARGET_ID),
            "conversation stopped"
        );
        assert!(
            manager.is_supervising(IdmmTargetKind::Terminal, TERMINAL_TARGET_ID),
            "terminal must survive stopping the conversation"
        );
    }

    #[tokio::test]
    async fn c3_shared_state_is_per_domain_at_same_id() {
        // `shared_for(kind, id)` returns a domain-distinct handle, so a
        // conversation's intervention counters never read a terminal's (which
        // a bare-id key would have aliased).
        let (probe, _injected) = MockProbe::new(vec![]);
        let manager = IdmmManager::new(
            deps_with(vec![]),
            Arc::new(FixedProbeFactory(probe)),
            Arc::new(EnabledConfigReader(rule_cfg())),
        );
        let conv_shared = manager.shared_for(
            IdmmTargetKind::Conversation,
            CONVERSATION_TARGET_ID,
        );
        let term_shared = manager.shared_for(IdmmTargetKind::Terminal, TERMINAL_TARGET_ID);
        assert!(
            !Arc::ptr_eq(&conv_shared, &term_shared),
            "conversation and terminal must not share one SupervisorShared cell"
        );
    }

    // ── AutoWork↔IDMM seam: has_pending_decision lets AutoWork yield a
    //    decision-ending turn to IDMM instead of finalizing/racing it. ──

    #[tokio::test]
    async fn has_pending_decision_true_when_decision_watch_on_and_text_is_decision() {
        use nomifun_requirement::IdmmHandle;
        // decision watch enabled + the just-finished turn TEXT is a 选择题 → AutoWork
        // should yield (IDMM will answer it). Detection is from the text itself, so
        // there is no race with the relay's message-status persistence.
        let (probe, _injected) = MockProbe::new(vec![]);
        let manager = IdmmManager::new(
            deps_with(vec![]),
            Arc::new(FixedProbeFactory(probe)),
            Arc::new(EnabledConfigReader(rule_cfg())),
        );
        let menu = "1) 方案A\n2) 方案B\n请回复编号告诉我你的选择。";
        assert!(
            manager
                .has_pending_decision(
                    AutoWorkTargetKind::Conversation,
                    CONVERSATION_TARGET_ID,
                    menu,
                )
                .await
        );
        // …and plain prose with no question/menu is NOT a pending decision.
        assert!(
            !manager
                .has_pending_decision(
                    AutoWorkTargetKind::Conversation,
                    CONVERSATION_TARGET_ID,
                    "好的，已经实现完成。",
                )
                .await
        );
    }

    #[tokio::test]
    async fn has_pending_decision_false_when_decision_watch_off() {
        use nomifun_requirement::IdmmHandle;
        // Fault-only config (decision watch disabled): even a 选择题 turn text must
        // report no pending decision — AutoWork must NOT yield to an IDMM that will
        // not answer questions (else it would needlessly wait the watchdog out).
        let (probe, _injected) = MockProbe::new(vec![]);
        let mut cfg = rule_cfg();
        cfg.decision_watch.base.enabled = false;
        let manager = IdmmManager::new(
            deps_with(vec![]),
            Arc::new(FixedProbeFactory(probe)),
            Arc::new(EnabledConfigReader(cfg)),
        );
        let menu = "1) 方案A\n2) 方案B\n请回复编号告诉我你的选择。";
        assert!(
            !manager
                .has_pending_decision(
                    AutoWorkTargetKind::Conversation,
                    CONVERSATION_TARGET_ID,
                    menu,
                )
                .await
        );
    }
}

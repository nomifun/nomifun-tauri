//! The per-session supervisor loop + `IdmmManager` (lifecycle, live counters,
//! continuous memory, scheduler) + the `IdmmHandle` impl AutoWork calls.
//! The decision audit trail itself is persisted to the DB (`idmm_interventions`)
//! via the records repo; the supervisor keeps only live counters for `IdmmState`.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::time::{Duration, Instant};

use dashmap::DashMap;
use nomifun_api_types::{AutoWorkTargetKind, IdmmConfig, IdmmState, IdmmTargetKind, InterventionRecord};
use nomifun_common::{generate_prefixed_id, now_ms};
use nomifun_db::IIdmmInterventionRepository;
use nomifun_db::models::IdmmInterventionRow;
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
    /// Persists every intervention (the decision audit trail / "思路"). Writes
    /// are fail-open — a DB error only warns; it must NEVER block or fail the
    /// decision path (the supervisor's job is to keep the session alive, not to
    /// guarantee the audit row landed).
    pub records: Arc<dyn IIdmmInterventionRepository>,
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
    let (kind, target_id) = probe.target();
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

    // On-arm replay: `observe()` subscribes only to FUTURE events, so a decision
    // the agent already emitted before the watch was enabled (the turn ended,
    // then the user toggled 智能决策 on) is never seen — the dot arms but nothing
    // happens. Evaluate the conversation's CURRENT pending decision ONCE here,
    // before the loop, gated on the DECISION watch being enabled (the pending
    // decision lane). The same `handle_stall` ladder answers it; after the
    // injected reply lands it becomes the new last message (position "right"), so
    // a later re-arm's `pending_signal` returns None and won't re-fire.
    if cfg.decision_watch.base.enabled
        && !cancel.load(Ordering::SeqCst)
        && let Some(sig) = probe.pending_signal().await
    {
        *shared.last_signal.lock().unwrap() = Some(signal_label(&sig));
        set_intervening(&shared, &deps, kind, &target_id, &cfg, true);
        let halted = handle_stall(&probe, &mut policy, &deps, &shared, kind, &target_id, &cfg, &sig).await;
        set_intervening(&shared, &deps, kind, &target_id, &cfg, false);
        if halted {
            warn!(target_id, "IDMM halted on the on-arm pending decision — standing down");
            return;
        }
    }

    while !cancel.load(Ordering::SeqCst) {
        let Some(mut sig) = rx.recv().await else {
            break;
        };
        *shared.last_signal.lock().unwrap() = Some(signal_label(&sig));

        match &sig {
            SessionSignal::Working | SessionSignal::Done => {
                policy.on_progress(&sig);
                set_intervening(&shared, &deps, kind, &target_id, &cfg, false);
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
                set_intervening(&shared, &deps, kind, &target_id, &cfg, false);
                continue;
            }
            SessionSignal::Exited => {
                debug!(target_id, "IDMM target exited — supervisor standing down");
                break;
            }
            _ => {}
        }

        // Mid-turn-arm recovery: an Idle means the agent went quiet. The live
        // Finish may have MISSED a decision (IDMM was enabled MID-TURN, so the
        // menu text streamed before observe() subscribed and turn_text was empty
        // at Finish), or the agent is now blocked on a tool-permission whose
        // event the freshly-subscribed live lane never replayed. Re-check the
        // conversation's CURRENT pending decision and answer THAT rather than
        // only nudging "continue" — this is what makes "会话中途临时开启 IDMM"
        // reliably take effect. Idempotent: pending_signal returns None once IDMM
        // has answered (its reply is the last message / the confirmation cleared),
        // so it never re-fires the same decision.
        if matches!(sig, SessionSignal::Idle)
            && cfg.decision_watch.base.enabled
            && let Some(recovered) = probe.pending_signal().await
        {
            sig = recovered;
        }

        // Normal-stop guard: an Idle that follows a clean Done (or any
        // Idle for a terminal target) is benign — stand by quietly. No
        // intervening flag flicker, no backoff sleep, no log entry. The
        // supervisor remains armed for genuine errors / decisions / a
        // future Working that re-arms work-in-progress tracking.
        if policy.peek_standby(&sig) {
            debug!(target_id, "IDMM standby (normal idle, no nudge)");
            continue;
        }

        // A stall. Sleep the backoff, then run the ladder.
        set_intervening(&shared, &deps, kind, &target_id, &cfg, true);
        let delay = policy.next_delay();
        if !delay.is_zero() {
            tokio::time::sleep(delay).await;
        }
        if cancel.load(Ordering::SeqCst) {
            break;
        }

        let halted = handle_stall(&probe, &mut policy, &deps, &shared, kind, &target_id, &cfg, &sig).await;
        set_intervening(&shared, &deps, kind, &target_id, &cfg, false);
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
    kind: IdmmTargetKind,
    target_id: &str,
    cfg: &IdmmConfig,
    sig: &SessionSignal,
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
            apply_action(probe, &action).await;
            policy.record_for(now, sig);
            emit_intervention(
                deps,
                shared,
                kind,
                target_id,
                sig,
                "rule",
                action.as_str(),
                applied_outcome(&action),
                None,
                EmitExtra {
                    detail: rule_detail(&action),
                    category: rule_category(sig),
                    ..Default::default()
                },
            )
            .await;
            false
        }
        PolicyStep::Sidecar { class, detail } => {
            run_sidecar(probe, policy, deps, shared, kind, target_id, cfg, sig, class, &detail).await;
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
    kind: IdmmTargetKind,
    target_id: &str,
    cfg: &IdmmConfig,
    sig: &SessionSignal,
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
        apply_action(probe, &fb).await;
        emit_intervention(
            deps,
            shared,
            kind,
            target_id,
            sig,
            "rule_fallback",
            fb.as_str(),
            applied_outcome(&fb),
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
            apply_action(probe, &action).await;
            let reason = if dec.reason.is_empty() {
                None
            } else {
                Some(dec.reason.clone())
            };
            emit_intervention(
                deps,
                shared,
                kind,
                target_id,
                sig,
                "sidecar",
                action.as_str(),
                applied_outcome(&action),
                reason,
                EmitExtra {
                    detail: rule_detail(&action),
                    category: rule_category(sig),
                    confidence: Some(dec.confidence),
                    bypass_model: bypass_model.clone(),
                },
            )
            .await;
        }
        SidecarStep::Halt(reason) => {
            warn!(target_id, reason, "IDMM sidecar decision halted");
            emit_intervention(
                deps,
                shared,
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
            apply_action(probe, &fb).await;
            emit_intervention(
                deps,
                shared,
                kind,
                target_id,
                sig,
                "rule_fallback",
                fb.as_str(),
                applied_outcome(&fb),
                Some("low_confidence_rulefallback".to_string()),
                EmitExtra {
                    detail: rule_detail(&fb),
                    category: rule_category(sig),
                    confidence: Some(dec.confidence),
                    bypass_model,
                },
            )
            .await;
        }
    }
}

async fn apply_action(probe: &Arc<dyn SessionProbe>, action: &WakeAction) {
    if let WakeAction::Wait(d) = action {
        if !d.is_zero() {
            tokio::time::sleep(*d).await;
        }
        return;
    }
    if matches!(action, WakeAction::Stop(_)) {
        return;
    }
    if let Err(e) = probe.inject(action).await {
        warn!(error = %e, action = action.as_str(), "IDMM inject failed");
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
        deps.emitter.emit_status_changed(&st);
    }
}

/// The enriched fields an `emit_intervention` call may carry beyond the always-
/// present `tier_used`/`action`/`outcome`. Each call site fills what it knows
/// and leaves the rest at default (`Default::default()` ⇒ all `None`), per the
/// plan's "fill from data already available; leave None where unavailable".
#[derive(Default)]
struct EmitExtra {
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

    let rec = InterventionRecord {
        id: generate_prefixed_id("idmmrec"),
        target_kind: target_kind.clone(),
        target_id: target_id.to_string(),
        watch: watch.clone(),
        at,
        stall_class: stall_class.clone(),
        tier_used: tier_used.to_string(),
        category: extra.category.clone(),
        action: action.to_string(),
        detail: detail.clone(),
        outcome: outcome.to_string(),
        reason: reason.clone(),
        confidence: extra.confidence,
        bypass_model: extra.bypass_model.clone(),
    };

    // Fail-open: persist the audit row, but a DB error must never block or fail
    // the decision path — only warn. The DB is the sole source of truth for
    // `/log`; the supervisor itself keeps only live counters (count / last-at).
    let row = IdmmInterventionRow {
        id: rec.id.clone(),
        target_kind,
        target_id: target_id.to_string(),
        watch,
        at,
        signal: stall_class,
        tier_used: tier_used.to_string(),
        category: extra.category,
        action: action.to_string(),
        detail,
        reason,
        confidence: rec.confidence.map(f64::from),
        bypass_model: extra.bypass_model,
        outcome: outcome.to_string(),
    };
    if let Err(e) = deps.records.insert(&row).await {
        warn!(target_id, error = %e, "IDMM intervention persist failed (fail-open)");
    }

    info!(
        target_id,
        stall = %rec.stall_class,
        tier = tier_used,
        action,
        outcome,
        "IDMM intervention"
    );
    shared.record(&rec);
    deps.emitter.emit_intervention(&rec);
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
    async fn read(&self, kind: IdmmTargetKind, target_id: &str) -> IdmmConfig;
}

/// Domain-qualified key for the per-target supervisor maps. The integer
/// conversation/terminal ids can collide numerically (`conv#5` vs `term#5`), so
/// supervisor handles and shared state are keyed by `(kind, target_id)` — a bare
/// id would let one domain's supervisor stomp the other's (spec §2.2 C3). The
/// `api-types/idmm.rs` note that "ids never collide" referred to the old prefixed
/// strings; this composite key makes that guarantee true again under integers.
type IdmmKey = (IdmmTargetKind, String);

/// Inner shared state, kept behind an `Arc` so the sync `IdmmHandle` seam can
/// clone it into a detached task (the lifecycle `ensure` is async).
pub struct IdmmInner {
    deps: Arc<LoopDeps>,
    /// `Arc` so each supervisor task can carry a cleanup guard that removes
    /// its own (generation-matched) entry when `run_supervisor` returns —
    /// without this, a naturally-exited supervisor (session Exited, probe
    /// found no agent task) stayed in the map forever, `is_supervising`
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
        let cfg = self.config_reader.read(kind, target_id).await;
        if !cfg.decision_watch.base.enabled {
            return false;
        }
        match self.factory.build(kind, target_id) {
            Some(probe) => probe.decision_in_text(turn_text).await,
            None => false,
        }
    }

    async fn ensure(&self, kind: IdmmTargetKind, target_id: &str) {
        if self.is_supervising(kind, target_id) {
            return;
        }
        let cfg = self.config_reader.read(kind, target_id).await;
        if !cfg.any_enabled() {
            return;
        }
        let Some(probe) = self.factory.build(kind, target_id) else {
            return;
        };
        let shared = self.shared_for(kind, target_id);
        let cancel = Arc::new(AtomicBool::new(false));
        let generation = self.next_generation.fetch_add(1, Ordering::SeqCst);
        let key: IdmmKey = (kind, target_id.to_string());
        let cleanup = SupervisorCleanup {
            handles: self.handles.clone(),
            key: key.clone(),
            generation,
        };
        let join = tokio::spawn({
            let cfg = cfg.clone();
            let deps = self.deps.clone();
            let cancel = cancel.clone();
            async move {
                let _cleanup = cleanup;
                run_supervisor(probe, cfg, deps, shared, cancel).await;
            }
        });
        // The supervisor may exit (and clean up) before this insert runs — a
        // probe with no live agent returns a closed channel and the loop ends
        // immediately. The `is_finished` check in `is_supervising` keeps such
        // a dead entry from reading as live, and the next `ensure` simply
        // replaces it.
        self.handles.insert(
            key,
            SupervisorHandle {
                cancel,
                join,
                generation,
            },
        );
        info!(target_id, ?kind, "IDMM supervisor armed");
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
    fn on_turn_start(&self, conversation_id: &str) {
        let inner = self.inner.clone();
        let target_id = conversation_id.to_string();
        tokio::spawn(async move {
            inner.ensure(IdmmTargetKind::Conversation, &target_id).await;
        });
    }
}

/// TerminalService → IDMM seam. A user-driven terminal has no AutoWork loop /
/// boot-resume to arm supervision, and (unlike a chat turn) fires on every input
/// chunk — so we guard on `is_supervising` BEFORE spawning to avoid a detached
/// `ensure` task per keystroke. The supervisor stands down on PTY exit / Halt;
/// the next activity (input / relaunch / create) re-arms it.
impl nomifun_terminal::TerminalSupervisionHook for IdmmManager {
    fn on_terminal_activity(&self, terminal_id: i64) {
        if self.inner.is_supervising(IdmmTargetKind::Terminal, &terminal_id.to_string()) {
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
    use crate::signal::{DecisionKind, DecisionPrompt, DecisionSource, WakeAction};
    use async_trait::async_trait;
    use nomifun_api_types::{IdmmConfig, WatchTier};
    use nomifun_db::DbError;
    use nomifun_db::models::ClientPreference;
    use std::sync::Mutex;
    use tokio::sync::mpsc;

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
                    target_id: "t1".into(),
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
                user_id: "u".into(),
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
    impl nomifun_realtime::EventBroadcaster for NullBroadcaster {
        fn broadcast(&self, _e: nomifun_api_types::WebSocketMessage<serde_json::Value>) {}
    }

    // ── Mock record repo: captures every `insert` so a test can assert the
    //    persisted row's fields. `delete`/`clear_all`/`sweep` are inert; the
    //    list reads echo back the captured inserts. ──
    #[derive(Default)]
    struct RecordingRepo {
        inserted: Mutex<Vec<nomifun_db::models::IdmmInterventionRow>>,
        /// When true, `insert` returns Err to exercise the fail-open path.
        fail: bool,
    }
    #[async_trait]
    impl nomifun_db::IIdmmInterventionRepository for RecordingRepo {
        async fn insert(&self, row: &nomifun_db::models::IdmmInterventionRow) -> Result<(), DbError> {
            self.inserted.lock().unwrap().push(row.clone());
            if self.fail {
                return Err(DbError::Query(sqlx::Error::Protocol("boom".into())));
            }
            Ok(())
        }
        async fn list_for_target(
            &self,
            _kind: &str,
            _id: &str,
            _limit: i64,
        ) -> Result<Vec<nomifun_db::models::IdmmInterventionRow>, DbError> {
            Ok(self.inserted.lock().unwrap().clone())
        }
        async fn delete_for_target(&self, _kind: &str, _id: &str) -> Result<u64, DbError> {
            Ok(0)
        }
        async fn list_recent(&self, _limit: i64) -> Result<Vec<nomifun_db::models::IdmmInterventionRow>, DbError> {
            Ok(self.inserted.lock().unwrap().clone())
        }
        async fn clear_all(&self) -> Result<u64, DbError> {
            Ok(0)
        }
        async fn sweep(&self, _cutoff_ms: i64, _global_cap: i64) -> Result<u64, DbError> {
            Ok(0)
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
            .insert(crate::sidecar::PREF_BACKUP_PROVIDER.into(), "prov".into());
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
            provider_id: Some("prov".into()),
            model: Some("m".into()),
        };
        c.decision_watch.base.enabled = true;
        c.decision_watch.base.tier = WatchTier::RulePlusModel;
        c.decision_watch.base.max_retries = 1;
        c.decision_watch.base.budget.min_interval_secs = 0;
        c.decision_watch.base.bypass_model = nomifun_api_types::BypassModelRef {
            provider_id: Some("prov".into()),
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
        // record must be written to the repo with a `idmmrec_`-prefixed id,
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
        assert!(row.id.starts_with("idmmrec_"), "id must be idmmrec_-prefixed; got {}", row.id);
        assert_eq!(row.target_kind, "conversation");
        assert_eq!(row.target_id, "t1");
        assert_eq!(row.watch, "decision");
        assert_eq!(row.signal, "decision");
        assert_eq!(row.tier_used, "sidecar");
        assert_eq!(row.category.as_deref(), Some("option"));
        assert_eq!(row.action, "answer_choice");
        assert_eq!(row.detail.as_deref(), Some("2) 方案B"));
        assert_eq!(row.reason.as_deref(), Some("B 更稳"));
        assert_eq!(row.confidence, Some(0.82_f32 as f64));
        assert_eq!(row.bypass_model.as_deref(), Some("prov/m"));
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
    async fn idle_after_done_still_recovers_pending_decision() {
        // The most representative mid-turn-arm case: the missed decision arrived
        // as a Finish→Done (empty turn_text after arming mid-stream), so
        // work_in_progress is false and the following Idle would otherwise be a
        // benign standby. Recovery must STILL fire — it re-routes the Idle to the
        // decision BEFORE the standby check.
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
            injected.lock().unwrap().iter().any(|a| matches!(a, WakeAction::AnswerChoice(_))),
            "recovery must fire even when a prior Finish→Done made the Idle standby-eligible"
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
    async fn persist_failure_is_fail_open() {
        // The repo returns Err on insert; the decision must still be applied
        // (the WakeAction is injected) and the loop must not panic/abort.
        let (probe, injected) = MockProbe::new(vec![provider_err()]);
        let records = Arc::new(RecordingRepo {
            fail: true,
            ..Default::default()
        });
        let deps = deps_with_records(vec![], records.clone());
        let shared = Arc::new(SupervisorShared::default());
        let cancel = Arc::new(AtomicBool::new(false));
        run_supervisor(probe, rule_cfg(), deps, shared.clone(), cancel).await;
        // Insert was attempted, the action was still applied, the WS-side
        // in-memory counter still incremented (fail-open).
        assert_eq!(records.inserted.lock().unwrap().len(), 1);
        assert_eq!(injected.lock().unwrap().as_slice(), &[WakeAction::Retry]);
        assert_eq!(shared.count.load(Ordering::Relaxed), 1);
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
    async fn terminal_idle_after_working_nudges_with_lifecycle() {
        // Terminal probe — Working → Idle now produces a nudge because the
        // lifecycle channel gives real Working/Done signals. The old
        // conservative "never nudge" is replaced by the shared
        // work_in_progress rule.
        let (probe, injected) = MockProbe::with_kind(
            vec![SessionSignal::Working, SessionSignal::Idle, SessionSignal::Idle],
            IdmmTargetKind::Terminal,
        );
        let deps = deps_with(vec![]);
        let shared = Arc::new(SupervisorShared::default());
        let cancel = Arc::new(AtomicBool::new(false));
        run_supervisor(probe, rule_cfg(), deps, shared, cancel).await;
        let inj = injected.lock().unwrap();
        assert!(
            inj.iter()
                .any(|a| matches!(a, WakeAction::SendText(t) if t == "continue")),
            "terminal Working→Idle should nudge now; got injected={inj:?}"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn provider_error_after_done_still_retries() {
        // A clean Done clears WIP — but a subsequent provider error must
        // still kick the rule retry ladder. The Idle-guard is scoped to
        // Idle signals only.
        let (probe, injected) = MockProbe::new(vec![
            SessionSignal::Working,
            SessionSignal::Done,
            provider_err(),
        ]);
        let deps = deps_with(vec![]);
        let shared = Arc::new(SupervisorShared::default());
        let cancel = Arc::new(AtomicBool::new(false));
        run_supervisor(probe, rule_cfg(), deps, shared.clone(), cancel).await;
        let inj = injected.lock().unwrap();
        assert!(inj.iter().any(|a| *a == WakeAction::Retry));
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

    struct EnabledConfigReader(IdmmConfig);
    #[async_trait]
    impl ConfigReader for EnabledConfigReader {
        async fn read(&self, _kind: IdmmTargetKind, _target_id: &str) -> IdmmConfig {
            self.0.clone()
        }
    }

    #[tokio::test(start_paused = true)]
    async fn exited_supervisor_cleans_up_its_handle() {
        // The scripted probe sends only Exited → run_supervisor breaks
        // immediately. The handle must leave the map (cleanup guard) so
        // is_supervising goes false — a stale `true` made AutoWork wait for
        // an IDMM recovery that could never come AND blocked every re-arm.
        let (probe, _injected) = MockProbe::new(vec![]);
        let manager = IdmmManager::new(
            deps_with(vec![]),
            Arc::new(FixedProbeFactory(probe)),
            Arc::new(EnabledConfigReader(rule_cfg())),
        );
        manager.ensure(IdmmTargetKind::Conversation, "t1").await;
        // Let the supervisor task run to its natural exit and clean up.
        for _ in 0..100 {
            if !manager.is_supervising(IdmmTargetKind::Conversation, "t1") {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(
            !manager.is_supervising(IdmmTargetKind::Conversation, "t1"),
            "a naturally-exited supervisor must not be reported as supervising"
        );
    }

    // ── C3 (spec §2.2): cross-domain supervisor isolation ───────────────────
    //
    // After integerization a conversation and a terminal can share a numeric
    // target id ("5"). The supervisor handle/shared maps key on `(kind,
    // target_id)`, so `conv#5` and `term#5` are supervised INDEPENDENTLY and
    // stopping one must never tear down the other. (The `api-types/idmm.rs`
    // "ids never collide" assumption was true only under the old prefixes; the
    // composite key restores it.)

    #[tokio::test(start_paused = true)]
    async fn c3_conv5_and_term5_are_supervised_independently() {
        // Time is paused, so the spawned supervisor tasks do not advance to
        // their Exited cleanup during the assertions — both handles stay live.
        let (probe, _injected) = MockProbe::new(vec![]);
        let manager = IdmmManager::new(
            deps_with(vec![]),
            Arc::new(FixedProbeFactory(probe)),
            Arc::new(EnabledConfigReader(rule_cfg())),
        );

        // Arm BOTH domains at the same numeric id "5".
        manager.ensure(IdmmTargetKind::Conversation, "5").await;
        manager.ensure(IdmmTargetKind::Terminal, "5").await;

        assert!(
            manager.is_supervising(IdmmTargetKind::Conversation, "5"),
            "conv#5 supervised"
        );
        assert!(
            manager.is_supervising(IdmmTargetKind::Terminal, "5"),
            "term#5 supervised — its handle did not collide with conv#5"
        );

        // Stop the conversation domain. The terminal #5 supervisor must remain.
        manager.stop(IdmmTargetKind::Conversation, "5");
        assert!(
            !manager.is_supervising(IdmmTargetKind::Conversation, "5"),
            "conv#5 stopped"
        );
        assert!(
            manager.is_supervising(IdmmTargetKind::Terminal, "5"),
            "term#5 must SURVIVE stopping conv#5 (no cross-domain teardown)"
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
        let conv_shared = manager.shared_for(IdmmTargetKind::Conversation, "5");
        let term_shared = manager.shared_for(IdmmTargetKind::Terminal, "5");
        assert!(
            !Arc::ptr_eq(&conv_shared, &term_shared),
            "conv#5 and term#5 must NOT share one SupervisorShared cell"
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
                .has_pending_decision(AutoWorkTargetKind::Conversation, "t1", menu)
                .await
        );
        // …and plain prose with no question/menu is NOT a pending decision.
        assert!(
            !manager
                .has_pending_decision(AutoWorkTargetKind::Conversation, "t1", "好的，已经实现完成。")
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
                .has_pending_decision(AutoWorkTargetKind::Conversation, "t1", menu)
                .await
        );
    }
}

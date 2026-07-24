//! The intervention policy: signal routing to the relevant watch (fault /
//! decision), the escalation ladder (rule tier → bypass-model tier → halt),
//! per-watch budget/cooldown gating, strategy → behavior mapping (D5), the
//! open-question branch (D6), and exponential backoff. Pure and deterministic —
//! every time input is an injected `Instant`, so it is fully unit-testable
//! without a clock.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

use nomifun_api_types::{
    BlockedBehavior, BudgetConfig, CategoryMode, DecisionWatchConfig, FaultWatchConfig, IdmmConfig, IdmmTargetKind,
    Tendency, WatchBase, WatchTier,
};

use crate::config::{is_cancel_option, is_destructive};
use crate::prompt::SidecarDecision;
use crate::signal::{DecisionKind, SessionSignal, StallClass, WakeAction};

/// Exponential backoff applied before a retry/nudge (clamped at the last entry).
const BACKOFF_LADDER: &[Duration] = &[
    Duration::from_secs(10),
    Duration::from_secs(30),
    Duration::from_secs(120),
    Duration::from_secs(300),
];

/// Which watch a signal routes to (D4). Fault signals (provider/agent errors)
/// route to the fault watch; everything else (idle, decisions, open questions)
/// routes to the decision watch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WatchLane {
    Fault,
    Decision,
}

/// What the policy decides for a stall, at the rule tier.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PolicyStep {
    /// Apply this rule-tier action now (supervisor sleeps `next_delay()` first).
    Rule(WakeAction),
    /// Escalate to the bypass model (sidecar) with this stall context.
    Sidecar { class: StallClass, detail: String },
    /// Stop intervening and surface to the human (reason).
    Halt(String),
    /// The signal is benign in the current state (e.g. an Idle that follows a
    /// clean Done, a terminal Idle with no error/decision, or a signal routed to
    /// a disabled watch). The supervisor stands by quietly — no intervention, no
    /// log entry, no state change.
    Standby,
}

/// What the policy decides for a sidecar-returned decision.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SidecarStep {
    /// Apply the sidecar's action.
    Apply(WakeAction),
    /// Reject the decision and surface a reason (destructive / low confidence).
    Halt(String),
    /// Confidence below floor → do the conservative rule fallback instead.
    Fallback,
}

/// Per-watch budget bookkeeping (D4: 预算/最小间隔按值守各自计). Each watch keeps
/// its own sliding window + last-intervention timestamp so one watch's storm
/// throttling never starves the other.
#[derive(Default)]
struct WatchRuntime {
    /// Intervention timestamps within the last hour (budget window).
    window: VecDeque<Instant>,
    last_intervention: Option<Instant>,
}

/// Confidence floor below which a bypass-model decision falls back to the
/// conservative rule action. Phase-1's per-session `confidence_floor` is gone;
/// the dual-watch config carries no per-watch floor, so a single conservative
/// default applies (equivalent to Phase-1's `SidecarConfig::default` 0.0 would
/// have *never* fallen back — but the spec's safety posture wants low-confidence
/// guesses to fall back, so a modest floor is used).
const CONFIDENCE_FLOOR: f32 = 0.0;

/// Per-target mutable policy state. Holds BOTH watch configs (D4) and routes each
/// signal to the relevant one; `on_stall` reads that watch's base
/// (tier/max_retries/budget) + strategy.
pub struct PolicyState {
    fault_watch: FaultWatchConfig,
    decision_watch: DecisionWatchConfig,
    /// Which kind of session this state supervises. Stored for diagnostics and
    /// the public `with_kind` constructor contract; idle gating uses the shared
    /// `work_in_progress` rule for both kinds (see `idle_is_standby`).
    #[allow(dead_code)]
    kind: IdmmTargetKind,
    /// Per-watch budget runtime (fault / decision).
    fault_rt: WatchRuntime,
    decision_rt: WatchRuntime,
    /// Per-stall-class retry counters (shared across watches — keyed by class).
    retries: std::collections::HashMap<&'static str, u32>,
    /// Current backoff index (session-aliveness, shared).
    backoff_step: usize,
    /// True once `Working` has been observed and `Done` has NOT yet arrived to
    /// close the turn. Cleared on `Done` (and on `Exited`). Distinguishes a
    /// "work-in-progress went silent" Idle (nudge) from a "completed turn,
    /// waiting for the next instruction" Idle (Standby).
    work_in_progress: bool,
    /// A clean `Done` is an absorbing boundary for the turn that just closed.
    ///
    /// Agent transports can deliver a trailing provider error, idle tick, or
    /// decision after their finish event. None of those late signals carries
    /// authority to restart the completed turn. Only a fresh, live `Working`
    /// transition proves that a new turn exists and re-arms the policy.
    suppressed_after_done: bool,
    /// True after the user deliberately cancelled the turn
    /// (`SessionSignal::Cancelled`); cleared when fresh `Working` arrives. While
    /// set, every stall signal resolves to Standby.
    suppressed_after_cancel: bool,
}

impl PolicyState {
    /// Construct a policy for a conversation target.
    pub fn new(cfg: IdmmConfig) -> Self {
        Self::with_kind(cfg, IdmmTargetKind::Conversation)
    }

    pub fn with_kind(cfg: IdmmConfig, kind: IdmmTargetKind) -> Self {
        Self {
            fault_watch: cfg.fault_watch,
            decision_watch: cfg.decision_watch,
            kind,
            fault_rt: WatchRuntime::default(),
            decision_rt: WatchRuntime::default(),
            retries: std::collections::HashMap::new(),
            backoff_step: 0,
            work_in_progress: false,
            suppressed_after_done: false,
            suppressed_after_cancel: false,
        }
    }

    /// Reconstruct the full config (for diagnostics / state emission).
    pub fn config(&self) -> IdmmConfig {
        IdmmConfig {
            fault_watch: self.fault_watch.clone(),
            decision_watch: self.decision_watch.clone(),
        }
    }

    /// Which watch lane a signal belongs to (D4).
    fn lane_for(sig: &SessionSignal) -> WatchLane {
        match sig {
            SessionSignal::ProviderError { .. } | SessionSignal::AgentError { .. } => WatchLane::Fault,
            _ => WatchLane::Decision,
        }
    }

    /// The base config (tier/retries/budget) for a lane.
    fn base_for(&self, lane: WatchLane) -> &WatchBase {
        match lane {
            WatchLane::Fault => &self.fault_watch.base,
            WatchLane::Decision => &self.decision_watch.base,
        }
    }

    /// Whether the watch handling `sig` is enabled (D4: a disabled watch ignores
    /// its signals = no auto-recovery for that lane).
    fn watch_enabled(&self, sig: &SessionSignal) -> bool {
        self.base_for(Self::lane_for(sig)).enabled
    }

    /// Whether the lane's tier may escalate to the bypass model.
    fn has_sidecar(&self, lane: WatchLane) -> bool {
        self.base_for(lane).tier == WatchTier::RulePlusModel
    }

    fn budget(&self, lane: WatchLane) -> &BudgetConfig {
        &self.base_for(lane).budget
    }

    fn max_retries(&self, lane: WatchLane) -> u32 {
        self.base_for(lane).max_retries
    }

    fn runtime_mut(&mut self, lane: WatchLane) -> &mut WatchRuntime {
        match lane {
            WatchLane::Fault => &mut self.fault_rt,
            WatchLane::Decision => &mut self.decision_rt,
        }
    }

    /// The delay to sleep before applying the next rule action.
    pub fn next_delay(&self) -> Duration {
        let idx = self.backoff_step.min(BACKOFF_LADDER.len() - 1);
        BACKOFF_LADDER[idx]
    }

    /// Prune a lane's budget window relative to `now` (entries older than 1h).
    fn prune_window(rt: &mut WatchRuntime, now: Instant) {
        let hour = Duration::from_secs(3600);
        while let Some(&front) = rt.window.front() {
            if now.duration_since(front) > hour {
                rt.window.pop_front();
            } else {
                break;
            }
        }
    }

    /// True if a new intervention is allowed for `lane` under its budget +
    /// min-interval. `is_blocking_decision` exempts the min-interval check (NOT
    /// the per-hour cap): a blocking decision leaves the agent STALLED until
    /// answered, so it cannot run away, and min-interval-deferring it is a silent
    /// DROP (`on_stall`→`Rule(Wait)`→`handle_stall` no-op) that deadlocks the
    /// agent at the next 选择项 landing within `min_interval_secs` of the previous
    /// answer (会话 25 regression). min-interval still rate-limits the idle-nudge /
    /// retry lanes, where the agent is working and could otherwise be hammered.
    fn budget_ok(&mut self, lane: WatchLane, now: Instant, is_blocking_decision: bool) -> Result<(), String> {
        let max_per_hour = self.budget(lane).max_interventions_per_hour;
        let min_interval = self.budget(lane).min_interval_secs;
        let rt = self.runtime_mut(lane);
        Self::prune_window(rt, now);
        if rt.window.len() as u32 >= max_per_hour {
            return Err("budget_exhausted".into());
        }
        if !is_blocking_decision
            && let Some(last) = rt.last_intervention
            && now.duration_since(last) < Duration::from_secs(min_interval as u64)
        {
            return Err("min_interval".into());
        }
        Ok(())
    }

    fn bump_retry(&mut self, class: StallClass) -> u32 {
        let n = self.retries.entry(class.as_str()).or_insert(0);
        *n += 1;
        *n
    }

    /// Decide the rule-tier step for a stall signal, routing it to the relevant
    /// watch (D4) and applying that watch's strategy (D5/D6).
    pub fn on_stall(&mut self, now: Instant, sig: &SessionSignal) -> PolicyStep {
        // A cancelled or cleanly-completed turn cannot be recovered by late
        // transport signals. A fresh Working transition is the sole re-arm.
        if self.suppressed_after_cancel || self.suppressed_after_done {
            return PolicyStep::Standby;
        }
        // D4 routing: a signal whose watch is disabled is ignored (stand by).
        if !self.watch_enabled(sig) {
            return PolicyStep::Standby;
        }
        let lane = Self::lane_for(sig);

        // A blocking decision (the agent is STALLED awaiting an answer) is exempt
        // from the min-interval rate-limit — deferring it silently drops it and
        // deadlocks the agent — but still honours the per-hour cap.
        let is_blocking_decision = matches!(sig, SessionSignal::Decision(_));

        // Per-watch budget gate.
        match self.budget_ok(lane, now, is_blocking_decision) {
            Ok(()) => {}
            Err(reason) if reason == "budget_exhausted" => {
                return PolicyStep::Halt("budget_exhausted".into());
            }
            Err(_) => {
                return PolicyStep::Rule(WakeAction::Wait(self.next_delay()));
            }
        }

        match sig {
            SessionSignal::ProviderError { retryable, .. } => self.on_fault(lane, StallClass::ProviderError, *retryable),
            SessionSignal::AgentError { retryable, .. } => self.on_fault(lane, StallClass::ProviderError, *retryable),
            SessionSignal::Idle => {
                let class = StallClass::Idle;
                if self.idle_is_standby() {
                    return PolicyStep::Standby;
                }
                if self.bump_retry(class) <= self.max_retries(lane) {
                    PolicyStep::Rule(WakeAction::SendText("continue".into()))
                } else if self.has_sidecar(lane) {
                    PolicyStep::Sidecar {
                        class,
                        detail: "session idle after nudges".into(),
                    }
                } else {
                    PolicyStep::Halt("idle_nudges_exhausted".into())
                }
            }
            SessionSignal::Decision(dp) => {
                if dp.kind == DecisionKind::OpenQuestion {
                    self.on_open_question(lane, dp)
                } else {
                    self.on_decision(lane, dp)
                }
            }
            // Non-stall signals should not reach here; treat as no-op wait.
            SessionSignal::Working | SessionSignal::Done | SessionSignal::Cancelled | SessionSignal::Exited => {
                PolicyStep::Rule(WakeAction::Wait(Duration::from_secs(0)))
            }
        }
    }

    /// Fault-watch handling (provider/agent errors). The fault watch retries per
    /// its `max_retries` whenever it is enabled; an explicit "no retry" is
    /// expressed by disabling the watch (D5), so there is no separate off gate.
    fn on_fault(&mut self, lane: WatchLane, class: StallClass, retryable: Option<bool>) -> PolicyStep {
        if retryable == Some(false) {
            return if self.has_sidecar(lane) {
                PolicyStep::Sidecar {
                    class,
                    detail: "non-retryable provider/agent error".into(),
                }
            } else {
                PolicyStep::Halt("non_retryable_provider_error".into())
            };
        }
        if self.bump_retry(class) <= self.max_retries(lane) {
            // D6: 当故障值守开启「模型故障转移队列」时,发 Failover(切下一候选模型并
            // 重新驱动本轮)而非朴素 Retry(原模型重试)。会话探针经会话服务的共享
            // helper 落地;终端/ACP 不支持,探针把 Failover 降级回 Retry(D7)。
            if self.fault_watch.use_failover_queue {
                PolicyStep::Rule(WakeAction::Failover)
            } else {
                PolicyStep::Rule(WakeAction::Retry)
            }
        } else if self.has_sidecar(lane) {
            PolicyStep::Sidecar {
                class,
                detail: "provider/agent error persisted after rule retries".into(),
            }
        } else {
            PolicyStep::Halt("provider_error_retries_exhausted".into())
        }
    }

    /// Decision-watch handling of a discrete-options / permission decision (D5).
    fn on_decision(&mut self, lane: WatchLane, dp: &crate::signal::DecisionPrompt) -> PolicyStep {
        let class = StallClass::Decision;
        let strat = &self.decision_watch.strategy;
        let opt = &strat.categories.option_decision;
        let perm_rule = &strat.categories.permission;

        // Structured tool-permission decision → resolved via the agent's confirm
        // channel (WakeAction::Confirm), governed by the `permission` rule.
        if let Some(perm) = &dp.permission {
            if perm_rule.mode != CategoryMode::Auto {
                return PolicyStep::Halt("permission_mode_not_auto".into());
            }
            // Rule tier auto-approves ONLY the conservatively-safe case (a
            // read-only tool's "allow once"); risky writes/execs have no
            // `safe_value`, so they escalate to the model or halt to a human —
            // never a blanket auto-approve. `only_safe_value` (default true) is
            // the safety gate; `escalate_risky` (default true) routes the rest.
            if perm_rule.only_safe_value
                && let Some(value) = &perm.safe_value
            {
                return PolicyStep::Rule(WakeAction::Confirm {
                    call_id: perm.call_id.clone(),
                    value: value.clone(),
                    always_allow: false,
                });
            }
            return if perm_rule.escalate_risky && self.has_sidecar(lane) {
                PolicyStep::Sidecar {
                    class,
                    detail: format!(
                        "tool permission: {} | options (label => value): {}",
                        dp.text,
                        perm.options
                            .iter()
                            .map(|(l, v)| format!("{l} => {v}"))
                            .collect::<Vec<_>>()
                            .join(" || ")
                    ),
                }
            } else {
                PolicyStep::Halt("permission_decision_no_sidecar".into())
            };
        }

        // Numbered/text option decision. The `option_decision` rule mode gates it.
        if opt.mode != CategoryMode::Auto {
            // AskFirst / Off both stand down to a human this phase (no async ask
            // channel) → escalate to model if available, else halt.
            return if self.has_sidecar(lane) {
                PolicyStep::Sidecar {
                    class,
                    detail: decision_detail(dp),
                }
            } else {
                PolicyStep::Halt("option_mode_not_auto".into())
            };
        }

        let allow_destructive = !opt.never_destructive;
        if opt.prefer_recommended
            && let Some(rec) = &dp.recommended
            && (allow_destructive || !is_destructive(rec))
        {
            return PolicyStep::Rule(WakeAction::AnswerChoice(rec.clone()));
        }
        // Conservative rule-tier auto-pick (no backup model needed). `tendency`
        // influences daring: Conservative does NOT pick an unmarked option
        // (prefers escalate/halt); Balanced/Aggressive do when enabled.
        let tendency = strat.tendency;
        let dares_unmarked_pick =
            opt.allow_unmarked_pick && !matches!(tendency, Tendency::Conservative);
        if dares_unmarked_pick
            && let Some(pick) = first_safe_option(&dp.options, allow_destructive)
        {
            return PolicyStep::Rule(WakeAction::AnswerChoice(pick));
        }
        if self.has_sidecar(lane) {
            PolicyStep::Sidecar {
                class,
                detail: decision_detail(dp),
            }
        } else {
            // Conservative tendency + no model → respect on_blocked: PreferPause /
            // MustAsk both halt; PreferContinue also halts here (no safe option to
            // proceed with), but the reason distinguishes the policy intent.
            PolicyStep::Halt(match strat.on_blocked {
                BlockedBehavior::MustAsk => "ambiguous_decision_must_ask".into(),
                _ => "ambiguous_decision_no_sidecar".into(),
            })
        }
    }

    /// Open-question handling (D6 纯问答). **Only** the decision watch's model
    /// tier may answer, and only when `answer_open_questions` is on and the
    /// `open_question` rule mode is `Auto`. The rule tier NEVER answers an open
    /// question (it cannot safely guess an open-ended answer) → Halt.
    fn on_open_question(&mut self, lane: WatchLane, dp: &crate::signal::DecisionPrompt) -> PolicyStep {
        let class = StallClass::OpenQuestion;
        let dw = &self.decision_watch;
        let answerable = self.has_sidecar(lane)
            && dw.answer_open_questions
            && dw.strategy.categories.open_question.mode == CategoryMode::Auto;
        if answerable {
            PolicyStep::Sidecar {
                class,
                detail: format!("open question: {}", dp.text),
            }
        } else {
            // RuleOnly / answer disabled / mode!=Auto → never guess an open answer.
            PolicyStep::Halt("open_question_not_answerable_by_rule".into())
        }
    }

    /// Validate a sidecar decision (destructive veto, confidence floor) → step.
    /// Destructive veto honors the decision watch's `never_destructive` rule.
    pub fn on_sidecar(&self, dec: &SidecarDecision) -> SidecarStep {
        let action = match dec.action.as_str() {
            "retry" => WakeAction::Retry,
            "send_text" => WakeAction::SendText(dec.text.clone()),
            "answer_choice" => WakeAction::AnswerChoice(dec.text.clone()),
            "answer_text" => WakeAction::SendText(dec.text.clone()),
            "wait" => WakeAction::Wait(Duration::from_secs(dec.wait_secs)),
            "stop" => {
                return SidecarStep::Halt(if dec.reason.is_empty() {
                    "sidecar_requested_stop".into()
                } else {
                    dec.reason.clone()
                });
            }
            other => return SidecarStep::Halt(format!("unknown_sidecar_action:{other}")),
        };

        // Destructive veto on the text-bearing actions (decision watch's guard).
        // The fault lane reuses the decision watch's strategy when it escalates to
        // its bypass model (the fault watch has no strategy of its own), so this
        // veto still applies to fault-lane escalations — a documented choice, not
        // an oversight.
        let never_destructive = self.decision_watch.strategy.categories.option_decision.never_destructive;
        if never_destructive
            && let WakeAction::SendText(t) | WakeAction::AnswerChoice(t) = &action
            && is_destructive(t)
        {
            return SidecarStep::Halt("destructive_withheld".into());
        }

        if dec.confidence < CONFIDENCE_FLOOR {
            return SidecarStep::Fallback;
        }

        SidecarStep::Apply(action)
    }

    /// Record that an intervention happened for the lane handling `sig` (push the
    /// lane's window ts, advance backoff).
    pub fn record_for(&mut self, now: Instant, sig: &SessionSignal) {
        let lane = Self::lane_for(sig);
        let rt = self.runtime_mut(lane);
        rt.window.push_back(now);
        rt.last_intervention = Some(now);
        self.backoff_step = (self.backoff_step + 1).min(BACKOFF_LADDER.len() - 1);
    }

    /// Call when a Working/Done signal arrives → update progress state.
    pub fn on_progress(&mut self, sig: &SessionSignal) {
        self.backoff_step = 0;
        match sig {
            SessionSignal::Working => {
                self.work_in_progress = true;
                self.suppressed_after_done = false;
                self.suppressed_after_cancel = false;
            }
            SessionSignal::Done => {
                self.retries.clear();
                self.work_in_progress = false;
                self.suppressed_after_done = true;
                // Do not clear `suppressed_after_cancel`: a late Done emitted
                // after a user cancellation must not weaken the stricter
                // cancellation boundary.
            }
            _ => {}
        }
    }

    /// Call when the user deliberately cancelled the turn.
    pub fn on_user_cancel(&mut self) {
        self.work_in_progress = false;
        self.suppressed_after_cancel = true;
        self.retries.clear();
        self.backoff_step = 0;
    }

    /// Peek whether a stall signal would resolve to a benign Standby (so the
    /// supervisor can short-circuit). Pure — no state change. A signal routed to
    /// a disabled watch is also a Standby (D4).
    pub fn peek_standby(&self, sig: &SessionSignal) -> bool {
        self.suppressed_after_cancel
            || self.suppressed_after_done
            || !self.watch_enabled(sig)
            || (matches!(sig, SessionSignal::Idle) && self.idle_is_standby())
    }

    /// Whether a plain `Idle` signal should be treated as a benign Standby in the
    /// current state. A plain Idle is benign unless work is in progress and not
    /// yet closed by a clean Done.
    pub fn idle_is_standby(&self) -> bool {
        !self.work_in_progress
    }

    /// The conservative action used when the sidecar fails or returns Fallback.
    ///
    /// A `Decision` must NEVER fall back to `Retry`. For an OPEN question there is
    /// no safe option to pick, so the fallback is `Stop` (the rule tier never
    /// guesses an open answer — D6).
    pub fn conservative_fallback(sig: &SessionSignal) -> WakeAction {
        match sig {
            SessionSignal::Decision(dp) => {
                if dp.kind == DecisionKind::OpenQuestion {
                    return WakeAction::Stop("open_question_unanswerable_fallback".into());
                }
                if let Some(perm) = &dp.permission {
                    return match &perm.safe_value {
                        Some(v) => WakeAction::Confirm {
                            call_id: perm.call_id.clone(),
                            value: v.clone(),
                            always_allow: false,
                        },
                        None => WakeAction::Stop("permission_unanswerable_fallback".into()),
                    };
                }
                if let Some(rec) = &dp.recommended {
                    return WakeAction::AnswerChoice(rec.clone());
                }
                if let Some(pick) = first_safe_option(&dp.options, false) {
                    return WakeAction::AnswerChoice(pick);
                }
                WakeAction::Stop("decision_unanswerable_fallback".into())
            }
            _ => WakeAction::Retry,
        }
    }
}

/// The detail string carried to the sidecar for a numbered/text decision.
fn decision_detail(dp: &crate::signal::DecisionPrompt) -> String {
    if dp.options.is_empty() {
        format!("decision prompt: {}", dp.text)
    } else {
        format!("decision prompt: {} | options: {}", dp.text, dp.options.join(" || "))
    }
}

/// The first decision option safe to auto-pick: neither a cancel/decline choice
/// (`is_cancel_option`) nor — unless explicitly allowed — destructive
/// (`is_destructive`). Returns `None` when no option qualifies.
fn first_safe_option(options: &[String], allow_destructive: bool) -> Option<String> {
    options
        .iter()
        .find(|o| !is_cancel_option(o) && (allow_destructive || !is_destructive(o)))
        .cloned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::signal::{DecisionPrompt, DecisionSource, PermissionConfirm};
    use nomifun_api_types::{CategoryMode, IdmmConfig, WatchTier};

    // ── Config builders mirroring Phase-1 rule_cfg / sidecar_cfg intent ──

    /// A decision-watch RuleOnly config, with auto-pick OFF (so escalate/halt
    /// tests keep their intent regardless of the production default).
    fn rule_cfg() -> IdmmConfig {
        let mut c = IdmmConfig::default();
        c.decision_watch.base.enabled = true;
        c.decision_watch.base.tier = WatchTier::RuleOnly;
        c.decision_watch.strategy.categories.option_decision.allow_unmarked_pick = false;
        // Fault watch enabled (RuleOnly) so provider errors route + retry.
        c.fault_watch.base.enabled = true;
        c.fault_watch.base.tier = WatchTier::RuleOnly;
        c
    }

    /// A decision+fault-watch RulePlusModel config (escalates to sidecar).
    fn sidecar_cfg() -> IdmmConfig {
        let mut c = IdmmConfig::default();
        c.decision_watch.base.enabled = true;
        c.decision_watch.base.tier = WatchTier::RulePlusModel;
        c.decision_watch.strategy.categories.option_decision.allow_unmarked_pick = false;
        c.fault_watch.base.enabled = true;
        c.fault_watch.base.tier = WatchTier::RulePlusModel;
        c
    }

    fn provider_err(retryable: Option<bool>) -> SessionSignal {
        SessionSignal::ProviderError {
            code: None,
            retryable,
            message: "500".into(),
        }
    }

    #[test]
    fn provider_error_retries_then_escalates() {
        let mut p = PolicyState::new(sidecar_cfg());
        let now = Instant::now();
        for i in 0..5 {
            let t = now + Duration::from_secs(i * 60);
            let sig = provider_err(Some(true));
            assert_eq!(p.on_stall(t, &sig), PolicyStep::Rule(WakeAction::Retry));
            p.record_for(t, &sig);
        }
        let t = now + Duration::from_secs(6 * 60);
        assert!(matches!(
            p.on_stall(t, &provider_err(Some(true))),
            PolicyStep::Sidecar { .. }
        ));
    }

    #[test]
    fn provider_error_retryable_false_escalates_with_sidecar() {
        let mut p = PolicyState::new(sidecar_cfg());
        assert!(matches!(
            p.on_stall(Instant::now(), &provider_err(Some(false))),
            PolicyStep::Sidecar { .. }
        ));
    }

    #[test]
    fn provider_error_retryable_false_halts_rule_only() {
        let mut p = PolicyState::new(rule_cfg());
        assert_eq!(
            p.on_stall(Instant::now(), &provider_err(Some(false))),
            PolicyStep::Halt("non_retryable_provider_error".into())
        );
    }

    #[test]
    fn idle_nudges_then_escalates() {
        let mut p = PolicyState::new(sidecar_cfg());
        p.on_progress(&SessionSignal::Working);
        let now = Instant::now();
        for i in 0..5 {
            let t = now + Duration::from_secs(i * 60);
            assert_eq!(
                p.on_stall(t, &SessionSignal::Idle),
                PolicyStep::Rule(WakeAction::SendText("continue".into()))
            );
            p.record_for(t, &SessionSignal::Idle);
        }
        let t = now + Duration::from_secs(6 * 60);
        assert!(matches!(p.on_stall(t, &SessionSignal::Idle), PolicyStep::Sidecar { .. }));
    }

    // ── Normal-stop-vs-abnormal-stall guard ──

    #[test]
    fn idle_after_done_is_standby_no_nudge() {
        let mut p = PolicyState::new(sidecar_cfg());
        p.on_progress(&SessionSignal::Working);
        p.on_progress(&SessionSignal::Done);
        assert_eq!(p.on_stall(Instant::now(), &SessionSignal::Idle), PolicyStep::Standby);
    }

    #[test]
    fn idle_without_any_working_is_standby() {
        let mut p = PolicyState::new(sidecar_cfg());
        assert_eq!(p.on_stall(Instant::now(), &SessionSignal::Idle), PolicyStep::Standby);
    }

    #[test]
    fn idle_after_working_without_done_still_nudges() {
        let mut p = PolicyState::new(sidecar_cfg());
        p.on_progress(&SessionSignal::Working);
        assert_eq!(
            p.on_stall(Instant::now(), &SessionSignal::Idle),
            PolicyStep::Rule(WakeAction::SendText("continue".into()))
        );
    }

    #[test]
    fn working_after_done_rearms_work_in_progress() {
        let mut p = PolicyState::new(sidecar_cfg());
        p.on_progress(&SessionSignal::Working);
        p.on_progress(&SessionSignal::Done);
        p.on_progress(&SessionSignal::Working);
        assert_eq!(
            p.on_stall(Instant::now(), &SessionSignal::Idle),
            PolicyStep::Rule(WakeAction::SendText("continue".into()))
        );
    }

    // ── User-cancel suppression + bounded retries across retried turns ──

    #[test]
    fn working_does_not_clear_retry_counters() {
        let mut p = PolicyState::new(rule_cfg());
        let now = Instant::now();
        for i in 0..5 {
            let t = now + Duration::from_secs(i * 60);
            let sig = provider_err(Some(true));
            assert_eq!(
                p.on_stall(t, &sig),
                PolicyStep::Rule(WakeAction::Retry),
                "retry #{i} within max_retries"
            );
            p.record_for(t, &sig);
            p.on_progress(&SessionSignal::Working);
        }
        let t = now + Duration::from_secs(6 * 60);
        assert_eq!(
            p.on_stall(t, &provider_err(Some(true))),
            PolicyStep::Halt("provider_error_retries_exhausted".into()),
            "the 6th consecutive failing retry must halt despite interleaved Working"
        );
    }

    #[test]
    fn done_is_absorbing_and_fresh_working_starts_with_a_clean_retry_ladder() {
        let mut p = PolicyState::new(rule_cfg());
        let now = Instant::now();
        for i in 0..5 {
            let t = now + Duration::from_secs(i * 60);
            let sig = provider_err(Some(true));
            assert_eq!(p.on_stall(t, &sig), PolicyStep::Rule(WakeAction::Retry));
            p.record_for(t, &sig);
        }
        p.on_progress(&SessionSignal::Done);
        let t = now + Duration::from_secs(10 * 60);
        assert_eq!(
            p.on_stall(t, &provider_err(Some(true))),
            PolicyStep::Standby,
            "late faults cannot cross the completed-turn boundary"
        );
        p.on_progress(&SessionSignal::Working);
        assert_eq!(
            p.on_stall(t, &provider_err(Some(true))),
            PolicyStep::Rule(WakeAction::Retry),
            "a fresh live turn starts with the retry ladder reset"
        );
    }

    #[test]
    fn done_suppresses_every_late_stall_until_fresh_working() {
        let mut p = PolicyState::new(rule_autopick_cfg());
        p.on_progress(&SessionSignal::Working);
        p.on_progress(&SessionSignal::Done);

        let late_signals = [
            provider_err(Some(true)),
            SessionSignal::AgentError {
                retryable: Some(true),
                message: "late agent error".into(),
            },
            SessionSignal::Idle,
            decision_with(&["1) retry", "2) stop"], Some("1) retry")),
        ];
        for sig in late_signals {
            assert!(p.peek_standby(&sig), "late {sig:?} must be recognized as standby");
            assert_eq!(
                p.on_stall(Instant::now(), &sig),
                PolicyStep::Standby,
                "late {sig:?} must not inject after Done"
            );
        }

        p.on_progress(&SessionSignal::Working);
        assert_eq!(
            p.on_stall(Instant::now(), &provider_err(Some(true))),
            PolicyStep::Rule(WakeAction::Retry),
            "only fresh Working may re-arm recovery"
        );
    }

    #[test]
    fn user_cancel_suppresses_stalls_until_new_working() {
        let mut p = PolicyState::new(rule_cfg());
        p.on_progress(&SessionSignal::Working);
        p.on_user_cancel();
        assert!(p.peek_standby(&provider_err(Some(true))), "peek must short-circuit after cancel");
        assert_eq!(p.on_stall(Instant::now(), &provider_err(Some(true))), PolicyStep::Standby);
        assert_eq!(p.on_stall(Instant::now(), &SessionSignal::Idle), PolicyStep::Standby);
        p.on_progress(&SessionSignal::Working);
        assert_eq!(
            p.on_stall(Instant::now(), &provider_err(Some(true))),
            PolicyStep::Rule(WakeAction::Retry),
            "new work re-arms the ladder"
        );
    }

    #[test]
    fn late_done_does_not_weaken_user_cancel_suppression() {
        let mut p = PolicyState::new(rule_cfg());
        p.on_progress(&SessionSignal::Working);
        p.on_user_cancel();
        p.on_progress(&SessionSignal::Done);

        assert_eq!(
            p.on_stall(Instant::now(), &provider_err(Some(true))),
            PolicyStep::Standby,
            "a transport Done after Cancelled must not reopen recovery"
        );
        p.on_progress(&SessionSignal::Working);
        assert_eq!(
            p.on_stall(Instant::now(), &provider_err(Some(true))),
            PolicyStep::Rule(WakeAction::Retry)
        );
    }

    #[test]
    fn terminal_idle_after_working_now_nudges() {
        let mut p = PolicyState::with_kind(sidecar_cfg(), IdmmTargetKind::Terminal);
        p.on_progress(&SessionSignal::Working);
        assert_eq!(
            p.on_stall(Instant::now(), &SessionSignal::Idle),
            PolicyStep::Rule(WakeAction::SendText("continue".into()))
        );
    }

    #[test]
    fn terminal_idle_after_done_is_standby() {
        let mut p = PolicyState::with_kind(sidecar_cfg(), IdmmTargetKind::Terminal);
        p.on_progress(&SessionSignal::Working);
        p.on_progress(&SessionSignal::Done);
        assert_eq!(p.on_stall(Instant::now(), &SessionSignal::Idle), PolicyStep::Standby);
    }

    #[test]
    fn terminal_idle_without_working_is_standby() {
        let mut p = PolicyState::with_kind(sidecar_cfg(), IdmmTargetKind::Terminal);
        assert_eq!(p.on_stall(Instant::now(), &SessionSignal::Idle), PolicyStep::Standby);
    }

    #[test]
    fn provider_error_before_any_progress_is_still_a_cold_start_fault() {
        // No Working or Done has been observed, so this may be the first model
        // connection attempt failing before it could emit progress.
        let mut p = PolicyState::new(sidecar_cfg());
        assert_eq!(
            p.on_stall(Instant::now(), &provider_err(Some(true))),
            PolicyStep::Rule(WakeAction::Retry)
        );
    }

    // ── D6: fault watch with the failover queue emits Failover instead of Retry ──

    #[test]
    fn fault_watch_with_failover_queue_emits_failover_not_retry() {
        // When the fault watch is enabled AND opts into the model failover queue,
        // a retryable provider error resolves to Failover (switch the model) — not
        // a naive Retry on the same failing model.
        let mut c = rule_cfg();
        c.fault_watch.use_failover_queue = true;
        let mut p = PolicyState::new(c);
        assert_eq!(
            p.on_stall(Instant::now(), &provider_err(Some(true))),
            PolicyStep::Rule(WakeAction::Failover),
            "use_failover_queue must turn the rule-tier retry into a Failover"
        );
    }

    #[test]
    fn fault_watch_without_failover_queue_still_retries() {
        // Default (use_failover_queue = false): unchanged Retry behavior.
        let mut p = PolicyState::new(rule_cfg());
        assert_eq!(
            p.on_stall(Instant::now(), &provider_err(Some(true))),
            PolicyStep::Rule(WakeAction::Retry),
            "without the failover queue the fault watch keeps its Retry behavior"
        );
    }

    // ── D4: a disabled watch ignores its signals ──

    #[test]
    fn disabled_fault_watch_ignores_provider_error() {
        // Decision watch on, fault watch OFF → a provider error is ignored
        // (no auto-retry) = Standby.
        let mut c = IdmmConfig::default();
        c.decision_watch.base.enabled = true;
        // fault_watch stays default (disabled).
        let mut p = PolicyState::new(c);
        assert!(p.peek_standby(&provider_err(Some(true))));
        assert_eq!(p.on_stall(Instant::now(), &provider_err(Some(true))), PolicyStep::Standby);
    }

    #[test]
    fn disabled_decision_watch_ignores_idle_and_decision() {
        // Fault watch on, decision watch OFF → idle nudges and decisions ignored.
        let mut c = IdmmConfig::default();
        c.fault_watch.base.enabled = true;
        let mut p = PolicyState::new(c);
        p.on_progress(&SessionSignal::Working);
        assert_eq!(p.on_stall(Instant::now(), &SessionSignal::Idle), PolicyStep::Standby);
        assert_eq!(p.on_stall(Instant::now(), &decision(None)), PolicyStep::Standby);
    }

    fn decision(recommended: Option<&str>) -> SessionSignal {
        SessionSignal::Decision(DecisionPrompt {
            text: "proceed? (1/2)".into(),
            options: vec!["1) yes".into(), "2) no".into()],
            recommended: recommended.map(|s| s.to_string()),
            source: DecisionSource::TerminalScan,
            kind: DecisionKind::Options,
            permission: None,
        })
    }

    #[test]
    fn decision_with_recommended_auto_accepts() {
        let mut p = PolicyState::new(sidecar_cfg());
        assert_eq!(
            p.on_stall(Instant::now(), &decision(Some("1) yes"))),
            PolicyStep::Rule(WakeAction::AnswerChoice("1) yes".into()))
        );
    }

    #[test]
    fn decision_ambiguous_escalates() {
        let mut p = PolicyState::new(sidecar_cfg());
        assert!(matches!(p.on_stall(Instant::now(), &decision(None)), PolicyStep::Sidecar { .. }));
    }

    #[test]
    fn decision_destructive_recommended_not_auto_accepted() {
        let mut p = PolicyState::new(sidecar_cfg());
        assert!(matches!(
            p.on_stall(Instant::now(), &decision(Some("rm -rf /data"))),
            PolicyStep::Sidecar { .. }
        ));
    }

    // ── min_interval must NOT silently drop a blocking decision (会话 25 RCA) ──

    #[test]
    fn blocking_decision_within_min_interval_is_not_deferred() {
        // REGRESSION (会话 25「IDMM 答了一次就不再决策」): a blocking decision leaves the
        // agent STALLED until answered. min_interval is a rate-limit for the
        // idle-nudge/retry lanes (agent still working); applied to a blocking
        // decision it returns Rule(Wait), which handle_stall treats as "do
        // nothing this pass" → the decision is consumed and SILENTLY DROPPED →
        // the agent deadlocks at the next 选择项 that lands within min_interval_secs
        // of the previous answer (Q2 技术选型 arrived 5s after Q1 was answered). A
        // blocked decision cannot run away; the per-hour cap is the real guard.
        let mut c = sidecar_cfg();
        c.decision_watch.base.budget.min_interval_secs = 20;
        c.decision_watch.base.budget.max_interventions_per_hour = 30;
        let mut p = PolicyState::new(c);
        let now = Instant::now();
        let step1 = p.on_stall(now, &decision(Some("1) yes")));
        assert!(matches!(step1, PolicyStep::Rule(WakeAction::AnswerChoice(_))));
        p.record_for(now, &decision(Some("1) yes")));
        // A SECOND decision only 5s later (well inside the 20s min-interval).
        let t2 = now + Duration::from_secs(5);
        let step2 = p.on_stall(t2, &decision(Some("1) yes")));
        assert!(
            !matches!(step2, PolicyStep::Rule(WakeAction::Wait(_))),
            "a blocking decision within min_interval must not be deferred/dropped; got {step2:?}"
        );
        assert!(matches!(step2, PolicyStep::Rule(WakeAction::AnswerChoice(_))));
    }

    #[test]
    fn blocking_decision_still_capped_by_max_per_hour() {
        // The per-hour cap still applies to decisions (the real runaway guard):
        // once the window is full, even a blocking decision halts to a human.
        let mut c = sidecar_cfg();
        c.decision_watch.base.budget.max_interventions_per_hour = 2;
        c.decision_watch.base.budget.min_interval_secs = 0;
        let mut p = PolicyState::new(c);
        let now = Instant::now();
        for i in 0..2 {
            let t = now + Duration::from_secs(i);
            let _ = p.on_stall(t, &decision(Some("1) yes")));
            p.record_for(t, &decision(Some("1) yes")));
        }
        let t = now + Duration::from_secs(3);
        assert_eq!(
            p.on_stall(t, &decision(Some("1) yes"))),
            PolicyStep::Halt("budget_exhausted".into())
        );
    }

    #[test]
    fn idle_nudge_still_respects_min_interval() {
        // GUARD: min_interval STILL rate-limits the idle-nudge lane (agent
        // working — would otherwise be nudged every scan tick). Only blocking
        // DECISIONS are exempt from min_interval.
        let mut c = sidecar_cfg();
        c.decision_watch.base.budget.min_interval_secs = 20;
        let mut p = PolicyState::new(c);
        p.on_progress(&SessionSignal::Working);
        let now = Instant::now();
        let first = p.on_stall(now, &SessionSignal::Idle);
        assert_eq!(first, PolicyStep::Rule(WakeAction::SendText("continue".into())));
        p.record_for(now, &SessionSignal::Idle);
        let t2 = now + Duration::from_secs(5);
        assert!(
            matches!(p.on_stall(t2, &SessionSignal::Idle), PolicyStep::Rule(WakeAction::Wait(_))),
            "idle nudges must still respect min_interval"
        );
    }

    // ── Rule-tier conservative auto-pick (allow_unmarked_pick) ──

    fn decision_with(options: &[&str], recommended: Option<&str>) -> SessionSignal {
        SessionSignal::Decision(DecisionPrompt {
            text: "请选择一个方案？".into(),
            options: options.iter().map(|s| s.to_string()).collect(),
            recommended: recommended.map(|s| s.to_string()),
            source: DecisionSource::TextScan,
            kind: DecisionKind::Options,
            permission: None,
        })
    }

    fn rule_autopick_cfg() -> IdmmConfig {
        let mut c = rule_cfg();
        c.decision_watch.strategy.categories.option_decision.allow_unmarked_pick = true;
        c
    }

    #[test]
    fn decision_auto_pick_unmarked_picks_first_safe_option() {
        let mut p = PolicyState::new(rule_autopick_cfg());
        assert_eq!(
            p.on_stall(Instant::now(), &decision_with(&["1) 方案A", "2) 方案B"], None)),
            PolicyStep::Rule(WakeAction::AnswerChoice("1) 方案A".into()))
        );
    }

    #[test]
    fn decision_auto_pick_skips_cancel_option() {
        let mut p = PolicyState::new(rule_autopick_cfg());
        assert_eq!(
            p.on_stall(Instant::now(), &decision_with(&["1) 取消", "2) 方案B"], None)),
            PolicyStep::Rule(WakeAction::AnswerChoice("2) 方案B".into()))
        );
    }

    #[test]
    fn decision_auto_pick_skips_destructive_option() {
        let mut p = PolicyState::new(rule_autopick_cfg());
        assert_eq!(
            p.on_stall(Instant::now(), &decision_with(&["1) drop table data", "2) 安全迁移"], None)),
            PolicyStep::Rule(WakeAction::AnswerChoice("2) 安全迁移".into()))
        );
    }

    #[test]
    fn decision_auto_pick_all_unsafe_falls_through_to_halt() {
        let mut p = PolicyState::new(rule_autopick_cfg());
        assert_eq!(
            p.on_stall(Instant::now(), &decision_with(&["1) 取消", "2) rm -rf /data"], None)),
            PolicyStep::Halt("ambiguous_decision_no_sidecar".into())
        );
    }

    #[test]
    fn decision_auto_pick_off_is_unchanged_halt() {
        // Default rule_cfg (allow_unmarked_pick = false): unmarked + no sidecar → halt.
        let mut p = PolicyState::new(rule_cfg());
        assert_eq!(
            p.on_stall(Instant::now(), &decision_with(&["1) 方案A", "2) 方案B"], None)),
            PolicyStep::Halt("ambiguous_decision_no_sidecar".into())
        );
    }

    #[test]
    fn decision_recommended_wins_over_auto_pick() {
        let mut p = PolicyState::new(rule_autopick_cfg());
        assert_eq!(
            p.on_stall(Instant::now(), &decision_with(&["1) 方案A", "2) 方案B"], Some("2) 方案B"))),
            PolicyStep::Rule(WakeAction::AnswerChoice("2) 方案B".into()))
        );
    }

    // ── D5: tendency influences daring on an unmarked auto-pick ──

    #[test]
    fn conservative_tendency_does_not_auto_pick_unmarked() {
        // Even with allow_unmarked_pick on, a Conservative tendency declines to
        // pick an unmarked option and halts (rule-only, no sidecar).
        let mut c = rule_autopick_cfg();
        c.decision_watch.strategy.tendency = Tendency::Conservative;
        let mut p = PolicyState::new(c);
        assert_eq!(
            p.on_stall(Instant::now(), &decision_with(&["1) 方案A", "2) 方案B"], None)),
            PolicyStep::Halt("ambiguous_decision_no_sidecar".into())
        );
    }

    // ── D5: category-mode mapping ──

    #[test]
    fn option_mode_off_halts_rule_only() {
        // option_decision.mode = Off → never auto-decide (rule-only halts).
        let mut c = rule_autopick_cfg();
        c.decision_watch.strategy.categories.option_decision.mode = CategoryMode::Off;
        let mut p = PolicyState::new(c);
        assert_eq!(
            p.on_stall(Instant::now(), &decision_with(&["1) 方案A", "2) 方案B"], None)),
            PolicyStep::Halt("option_mode_not_auto".into())
        );
    }

    #[test]
    fn option_mode_ask_first_escalates_with_sidecar() {
        // AskFirst with a model tier escalates (no async ask channel this phase).
        let mut c = sidecar_cfg();
        c.decision_watch.strategy.categories.option_decision.mode = CategoryMode::AskFirst;
        let mut p = PolicyState::new(c);
        assert!(matches!(
            p.on_stall(Instant::now(), &decision_with(&["1) 方案A", "2) 方案B"], None)),
            PolicyStep::Sidecar { .. }
        ));
    }

    // ── Structured tool-permission decisions (safety gate) ──

    fn permission_decision(call_id: &str, safe: bool) -> SessionSignal {
        SessionSignal::Decision(DecisionPrompt {
            text: "tool permission".into(),
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
                safe_value: if safe { Some("proceed_once".into()) } else { None },
            }),
        })
    }

    #[test]
    fn permission_safe_rule_tier_auto_confirms() {
        let mut p = PolicyState::new(rule_cfg());
        assert_eq!(
            p.on_stall(Instant::now(), &permission_decision("call-1", true)),
            PolicyStep::Rule(WakeAction::Confirm {
                call_id: "call-1".into(),
                value: "proceed_once".into(),
                always_allow: false,
            })
        );
    }

    #[test]
    fn permission_risky_rule_only_halts() {
        let mut p = PolicyState::new(rule_cfg());
        assert_eq!(
            p.on_stall(Instant::now(), &permission_decision("call-1", false)),
            PolicyStep::Halt("permission_decision_no_sidecar".into())
        );
    }

    #[test]
    fn permission_risky_with_sidecar_escalates() {
        let mut p = PolicyState::new(sidecar_cfg());
        assert!(matches!(
            p.on_stall(Instant::now(), &permission_decision("call-1", false)),
            PolicyStep::Sidecar { .. }
        ));
    }

    #[test]
    fn conservative_fallback_permission_safe_confirms() {
        assert_eq!(
            PolicyState::conservative_fallback(&permission_decision("call-9", true)),
            WakeAction::Confirm {
                call_id: "call-9".into(),
                value: "proceed_once".into(),
                always_allow: false,
            }
        );
    }

    #[test]
    fn conservative_fallback_decision_never_retries() {
        assert_eq!(
            PolicyState::conservative_fallback(&decision_with(&["1) Canvas", "2) DOM"], None)),
            WakeAction::AnswerChoice("1) Canvas".into())
        );
        assert!(matches!(
            PolicyState::conservative_fallback(&decision_with(&["1) 取消"], None)),
            WakeAction::Stop(_)
        ));
    }

    // ── D6: open-question handling ──

    fn open_question() -> SessionSignal {
        SessionSignal::Decision(DecisionPrompt {
            text: "你希望缓存怎么设计？".into(),
            options: vec![],
            recommended: None,
            source: DecisionSource::TextScan,
            kind: DecisionKind::OpenQuestion,
            permission: None,
        })
    }

    /// A decision watch that may answer open questions (RulePlusModel + on).
    fn open_answer_cfg() -> IdmmConfig {
        let mut c = sidecar_cfg();
        c.decision_watch.answer_open_questions = true;
        c
    }

    #[test]
    fn open_question_model_tier_answers_via_sidecar() {
        let mut p = PolicyState::new(open_answer_cfg());
        match p.on_stall(Instant::now(), &open_question()) {
            PolicyStep::Sidecar { class, .. } => assert_eq!(class, StallClass::OpenQuestion),
            other => panic!("expected sidecar for an answerable open question, got {other:?}"),
        }
    }

    #[test]
    fn open_question_rule_only_never_answers() {
        // RuleOnly decision watch (no model) → never answers an open question.
        let mut p = PolicyState::new(rule_cfg());
        assert_eq!(
            p.on_stall(Instant::now(), &open_question()),
            PolicyStep::Halt("open_question_not_answerable_by_rule".into())
        );
    }

    #[test]
    fn open_question_model_tier_but_disabled_flag_halts() {
        // RulePlusModel but answer_open_questions=false → must NOT answer.
        let mut c = sidecar_cfg();
        c.decision_watch.answer_open_questions = false;
        let mut p = PolicyState::new(c);
        assert_eq!(
            p.on_stall(Instant::now(), &open_question()),
            PolicyStep::Halt("open_question_not_answerable_by_rule".into())
        );
    }

    #[test]
    fn open_question_mode_off_halts() {
        // answer on, but the open_question category mode is Off → no answer.
        let mut c = open_answer_cfg();
        c.decision_watch.strategy.categories.open_question.mode = CategoryMode::Off;
        let mut p = PolicyState::new(c);
        assert_eq!(
            p.on_stall(Instant::now(), &open_question()),
            PolicyStep::Halt("open_question_not_answerable_by_rule".into())
        );
    }

    #[test]
    fn conservative_fallback_open_question_stops() {
        // If the sidecar fails on an open question, the rule tier must STOP, not
        // guess a free-text answer.
        assert!(matches!(
            PolicyState::conservative_fallback(&open_question()),
            WakeAction::Stop(_)
        ));
    }

    // ── Default-config-equals-Phase-1-behavior regression guard (D5) ──

    #[test]
    fn default_decision_watch_enabled_reproduces_phase1_autopick() {
        // A decision watch enabled with ALL strategy defaults (D5 says defaults
        // == Phase-1 behavior) must auto-pick the first safe option on an
        // unmarked numbered decision — exactly as Phase-1 did with
        // auto_pick_unmarked=true (the production default).
        let mut c = IdmmConfig::default();
        c.decision_watch.base.enabled = true; // tier defaults RuleOnly
        let mut p = PolicyState::new(c);
        assert_eq!(
            p.on_stall(Instant::now(), &decision_with(&["1) 方案A", "2) 方案B"], None)),
            PolicyStep::Rule(WakeAction::AnswerChoice("1) 方案A".into())),
            "default decision watch must reproduce Phase-1 conservative auto-pick"
        );
    }

    #[test]
    fn default_fault_watch_enabled_reproduces_phase1_retry() {
        // A fault watch enabled with defaults must retry a retryable provider
        // error (Phase-1's auto_retry default).
        let mut c = IdmmConfig::default();
        c.fault_watch.base.enabled = true;
        let mut p = PolicyState::new(c);
        assert_eq!(
            p.on_stall(Instant::now(), &provider_err(Some(true))),
            PolicyStep::Rule(WakeAction::Retry),
            "default fault watch must reproduce Phase-1 auto-retry"
        );
    }

    // ── Budget / min-interval (now per-watch) ──

    #[test]
    fn budget_exhausts_after_max_per_hour() {
        let mut cfg = sidecar_cfg();
        cfg.fault_watch.base.budget = BudgetConfig {
            max_interventions_per_hour: 3,
            min_interval_secs: 0,
        };
        let mut p = PolicyState::new(cfg);
        let now = Instant::now();
        for i in 0..3 {
            let t = now + Duration::from_secs(i);
            let sig = provider_err(Some(true));
            let _ = p.on_stall(t, &sig);
            p.record_for(t, &sig);
        }
        let t = now + Duration::from_secs(4);
        assert_eq!(
            p.on_stall(t, &provider_err(Some(true))),
            PolicyStep::Halt("budget_exhausted".into())
        );
    }

    #[test]
    fn min_interval_defers() {
        let mut cfg = sidecar_cfg();
        cfg.fault_watch.base.budget.min_interval_secs = 60;
        let mut p = PolicyState::new(cfg);
        let now = Instant::now();
        let sig = provider_err(Some(true));
        let _ = p.on_stall(now, &sig);
        p.record_for(now, &sig);
        let t = now + Duration::from_secs(10);
        assert!(matches!(
            p.on_stall(t, &provider_err(Some(true))),
            PolicyStep::Rule(WakeAction::Wait(_))
        ));
    }

    #[test]
    fn per_watch_budgets_are_independent() {
        // Exhausting the fault watch's budget must NOT block the decision watch.
        let mut cfg = sidecar_cfg();
        cfg.fault_watch.base.budget = BudgetConfig {
            max_interventions_per_hour: 1,
            min_interval_secs: 0,
        };
        let mut p = PolicyState::new(cfg);
        let now = Instant::now();
        // Exhaust fault budget (1 allowed).
        let fe = provider_err(Some(true));
        let _ = p.on_stall(now, &fe);
        p.record_for(now, &fe);
        assert_eq!(p.on_stall(now, &provider_err(Some(true))), PolicyStep::Halt("budget_exhausted".into()));
        // The decision watch still answers a decision (its own budget intact).
        assert_eq!(
            p.on_stall(now, &decision(Some("1) yes"))),
            PolicyStep::Rule(WakeAction::AnswerChoice("1) yes".into()))
        );
    }

    #[test]
    fn sidecar_destructive_vetoed_when_not_allowed() {
        let p = PolicyState::new(sidecar_cfg());
        let dec = SidecarDecision {
            action: "send_text".into(),
            text: "rm -rf /".into(),
            wait_secs: 0,
            confidence: 0.99,
            reason: String::new(),
        };
        assert_eq!(p.on_sidecar(&dec), SidecarStep::Halt("destructive_withheld".into()));
    }

    #[test]
    fn sidecar_applies_answer_choice() {
        let p = PolicyState::new(sidecar_cfg());
        let dec = SidecarDecision {
            action: "answer_choice".into(),
            text: "2".into(),
            wait_secs: 0,
            confidence: 0.9,
            reason: String::new(),
        };
        assert_eq!(p.on_sidecar(&dec), SidecarStep::Apply(WakeAction::AnswerChoice("2".into())));
    }

    #[test]
    fn sidecar_answer_text_maps_to_send_text() {
        // D6: the open-question free-text answer action.
        let p = PolicyState::new(open_answer_cfg());
        let dec = SidecarDecision {
            action: "answer_text".into(),
            text: "用 LRU + 30 分钟 TTL".into(),
            wait_secs: 0,
            confidence: 0.8,
            reason: "balanced".into(),
        };
        assert_eq!(
            p.on_sidecar(&dec),
            SidecarStep::Apply(WakeAction::SendText("用 LRU + 30 分钟 TTL".into()))
        );
    }

    #[test]
    fn on_progress_resets_backoff_and_retries() {
        let mut p = PolicyState::new(sidecar_cfg());
        p.on_progress(&SessionSignal::Working);
        let now = Instant::now();
        for i in 0..3 {
            let t = now + Duration::from_secs(i * 60);
            let _ = p.on_stall(t, &SessionSignal::Idle);
            p.record_for(t, &SessionSignal::Idle);
        }
        assert!(p.next_delay() > BACKOFF_LADDER[0]);
        p.on_progress(&SessionSignal::Working);
        assert_eq!(p.next_delay(), BACKOFF_LADDER[0]);
        let t = now + Duration::from_secs(600);
        assert_eq!(
            p.on_stall(t, &SessionSignal::Idle),
            PolicyStep::Rule(WakeAction::SendText("continue".into()))
        );
    }

    #[test]
    fn backoff_sequence_is_exponential_clamped() {
        let mut p = PolicyState::new(sidecar_cfg());
        let now = Instant::now();
        let sig = provider_err(Some(true));
        assert_eq!(p.next_delay(), Duration::from_secs(10));
        p.record_for(now, &sig);
        assert_eq!(p.next_delay(), Duration::from_secs(30));
        p.record_for(now, &sig);
        assert_eq!(p.next_delay(), Duration::from_secs(120));
        p.record_for(now, &sig);
        assert_eq!(p.next_delay(), Duration::from_secs(300));
        p.record_for(now, &sig);
        assert_eq!(p.next_delay(), Duration::from_secs(300));
    }
}

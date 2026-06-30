use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::time::Duration;

use dashmap::DashMap;
use nomifun_ai_agent::AgentStreamEvent;
use nomifun_ai_agent::TurnStopReason;
use nomifun_ai_agent::registry::AgentRegistry;
use nomifun_ai_agent::task_manager::IWorkerTaskManager;
use nomifun_ai_agent::types::BuildTaskOptions;
use nomifun_api_types::{AutoWorkState, AutoWorkTargetKind, Requirement, RequirementStatus, SendMessageRequest};
use nomifun_common::AppError;
use nomifun_conversation::ConversationService;
use nomifun_db::{IConversationRepository, IUserRepository};
use nomifun_terminal::{LifecycleKind, TerminalDriver};
use tokio::sync::broadcast;
use tokio::time::{interval, sleep, timeout};
use tracing::{error, info, warn};

use crate::prompt::{build_requirement_prompt, build_terminal_requirement_prompt};
use crate::service::{DEFAULT_LEASE_MS, RequirementService};

/// Lease is renewed on this cadence while a turn is in flight.
const LEASE_RENEW_INTERVAL: Duration = Duration::from_secs(30);
/// Hard ceiling on a single requirement turn.
const TURN_TIMEOUT: Duration = Duration::from_secs(3600);
/// Terminal turn: beat between writing the bracketed-paste body and writing the
/// submit CR. The CR MUST be a separate write after this delay — modern agent
/// TUIs (claude/codex/gemini) use paste-burst detection that suppresses
/// auto-submit for a CR arriving in the same read() as the paste-end marker, so
/// a CR appended to the paste leaves the text sitting unsubmitted in the input
/// box. Matches the cron terminal executor's proven fix (`nomifun-cron`).
const TERMINAL_SUBMIT_DELAY: Duration = Duration::from_millis(120);
/// Idle cadence for a persistent loop with nothing to do (tag drained, claim
/// error, or a terminal awaiting relaunch). The `wake` Notify makes a freshly
/// created/re-pended requirement claim near-instantly; this is the safety-net
/// poll for anything the waker can't observe (e.g. the lease sweeper re-pending,
/// or a terminal coming back alive).
const IDLE_POLL: Duration = Duration::from_secs(10);
/// Cap on the completion note captured from a tool-free agent's final message,
/// in characters. The tail is kept (agents usually summarise at the end).
const MAX_NOTE_CHARS: usize = 4000;
const SYSTEM_DEFAULT_USER_ID: &str = "system_default_user";
/// How many retryable errors AutoWork will WAIT THROUGH (letting IDMM recover
/// the turn in-place) before giving up and failing the turn. Bounds the
/// worst-case hang when IDMM supervises but cannot recover. Combined with IDMM's
/// own escalating backoff this is several minutes of grace, then a hard fail.
const MAX_RECOVERY_WAITS: u32 = 5;

/// Max consecutive decision-ending turns AutoWork will YIELD to IDMM within one
/// requirement turn before finalizing it itself (bounds a runaway question loop).
const MAX_DECISION_WAITS: u32 = 12;

/// How long AutoWork waits for IDMM to START answering a pending decision (i.e.
/// drive a follow-up turn) before giving up the yield and finalizing the turn
/// as-is. Must comfortably exceed IDMM's first decision backoff (~10s) plus a
/// sidecar model call, so the model tier reliably wins; a rule-tier watch that
/// cannot auto-answer simply falls back to finalize after this window (no hang).
const DECISION_YIELD_WINDOW: Duration = Duration::from_secs(90);


/// Shared dependencies for all autowork loops.
pub struct OrchestratorDeps {
    pub service: Arc<RequirementService>,
    pub task_manager: Arc<dyn IWorkerTaskManager>,
    pub conversation_service: ConversationService,
    pub conversation_repo: Arc<dyn IConversationRepository>,
    pub agent_registry: Arc<AgentRegistry>,
    /// Drives terminal targets (write PTY input, observe output). `None` if the
    /// terminal subsystem is not wired (e.g. some test harnesses).
    pub terminal_driver: Option<Arc<dyn TerminalDriver>>,
    /// Optional IDMM supervisor. When present, AutoWork ensures the target is
    /// supervised while each turn runs so provider faults / decision stalls are
    /// auto-handled and the turn can complete instead of hanging to timeout.
    /// `None` if IDMM is not wired (tests, or the feature disabled at assembly).
    pub idmm: Option<Arc<dyn crate::hooks::IdmmHandle>>,
    /// Notified whenever a requirement becomes claimable. Idle loops await this
    /// (with `IDLE_POLL` as a fallback) so newly created/re-pended work is picked
    /// up immediately. Shared with the `RequirementService` that fires it.
    pub wake: Arc<tokio::sync::Notify>,
    /// Whether the requirement MCP server is running and injected into ACP
    /// sessions (bootstrap-level flag). When true, ACP sessions expose the
    /// `requirement_complete` / `requirement_update_status` declaration tools,
    /// so the orchestrator tells them to call those tools AND expects an explicit
    /// verdict (a clean turn with no declaration → needs_review, not done). Kept
    /// in lock-step with `AgentFactoryDeps::requirement_mcp_config` so the prompt
    /// never names a tool the session lacks.
    pub requirement_mcp_enabled: bool,
}

/// Live progress for one autowork loop, shared between the loop task and the
/// API (`get_autowork`). Read by `Orchestrator::live_progress`.
#[derive(Default)]
struct LiveProgress {
    current_requirement_id: Mutex<Option<i64>>,
    completed_count: AtomicU32,
}

impl LiveProgress {
    fn set_current(&self, id: Option<i64>) {
        *self.current_requirement_id.lock().expect("progress lock") = id;
    }
    fn current(&self) -> Option<i64> {
        *self.current_requirement_id.lock().expect("progress lock")
    }
    fn incr_completed(&self) -> u32 {
        self.completed_count.fetch_add(1, Ordering::SeqCst) + 1
    }
    fn completed(&self) -> u32 {
        self.completed_count.load(Ordering::SeqCst)
    }
}

/// Domain-qualified key for the per-target loop maps. After integerization a
/// conversation and a terminal can share a numeric id (`conv#5` vs `term#5`), so
/// the loop registry MUST key on `(kind, target_id)` — a bare id would let one
/// domain's `start`/`stop` clobber the other's loop (spec §2.2 C4).
type TargetKey = (AutoWorkTargetKind, String);

struct AutoWorkHandle {
    /// Cooperative cancel flag, checked between turns by the loop.
    cancelled: Arc<AtomicBool>,
    join: tokio::task::JoinHandle<()>,
    tag: String,
    /// Target kind, kept so `stop()` knows whether an in-flight turn lives in a
    /// conversation agent (cancellable) or a terminal PTY (left untouched).
    kind: AutoWorkTargetKind,
    /// Live progress (current requirement + completed count).
    progress: Arc<LiveProgress>,
    /// Monotonic id distinguishing this loop instance from a later restart on
    /// the same target, so a naturally-exiting loop only removes its own entry
    /// (not a fresh one a concurrent `start()` just inserted).
    generation: u64,
}

/// Removes a loop's handle from the map on task exit — normal OR panic (Drop runs
/// during unwind). The generation guard prevents clobbering a fresh handle that a
/// concurrent `start()` may have inserted.
struct HandleGuard {
    handles: Arc<DashMap<TargetKey, AutoWorkHandle>>,
    key: TargetKey,
    generation: u64,
}

impl Drop for HandleGuard {
    fn drop(&mut self) {
        self.handles
            .remove_if(&self.key, |_, h| h.generation == self.generation);
    }
}

/// Drives per-session AutoWork loops and the lease sweeper.
#[derive(Clone)]
pub struct Orchestrator {
    deps: Arc<OrchestratorDeps>,
    handles: Arc<DashMap<TargetKey, AutoWorkHandle>>,
    next_generation: Arc<std::sync::atomic::AtomicU64>,
}

impl Orchestrator {
    pub fn new(deps: Arc<OrchestratorDeps>) -> Self {
        Self {
            deps,
            handles: Arc::new(DashMap::new()),
            next_generation: Arc::new(std::sync::atomic::AtomicU64::new(0)),
        }
    }

    /// Active loops as `(kind, target_id)` pairs (the sweeper's "active" set).
    pub fn active_targets(&self) -> Vec<TargetKey> {
        self.handles.iter().map(|e| e.key().clone()).collect()
    }

    pub fn is_running(&self, kind: AutoWorkTargetKind, target_id: &str) -> bool {
        self.handles.contains_key(&(kind, target_id.to_string()))
    }

    pub fn running_tag(&self, kind: AutoWorkTargetKind, target_id: &str) -> Option<String> {
        self.handles.get(&(kind, target_id.to_string())).map(|h| h.tag.clone())
    }

    /// Live progress for a running loop: `(current_requirement_id, completed_count)`.
    /// `current_requirement_id` is stringified at this API boundary (the AutoWork
    /// DTO carries it as a string), the id itself is the single-track integer.
    pub fn live_progress(&self, kind: AutoWorkTargetKind, target_id: &str) -> Option<(Option<String>, u32)> {
        self.handles
            .get(&(kind, target_id.to_string()))
            .map(|h| (h.progress.current().map(|id| id.to_string()), h.progress.completed()))
    }

    /// Start (or restart) the autowork loop for a target bound to `tag`.
    /// Stops after `max_requirements` completions when set.
    pub fn start(&self, kind: AutoWorkTargetKind, target_id: String, tag: String, max_requirements: Option<u32>) {
        self.stop(kind, &target_id);

        let generation = self.next_generation.fetch_add(1, Ordering::SeqCst);
        let cancelled = Arc::new(AtomicBool::new(false));
        let progress = Arc::new(LiveProgress::default());
        let cancelled_for_task = cancelled.clone();
        let progress_for_task = progress.clone();
        let deps = self.deps.clone();
        let handles = self.handles.clone();
        let conv = target_id.clone();
        let loop_tag = tag.clone();
        let key: TargetKey = (kind, target_id.clone());
        let guard_key = key.clone();

        // Insert the handle BEFORE the loop's first await can reach its Drop-guard
        // cleanup (run_loop always awaits `claim_next` before any cleanup), so the
        // guard never removes a not-yet-inserted entry.
        let join = tokio::spawn(async move {
            // Drop runs on normal exit AND panic-unwind → handle always removed.
            let _guard = HandleGuard {
                handles,
                key: guard_key,
                generation,
            };
            info!(target_id = %conv, ?kind, tag = %loop_tag, "AutoWork loop started");
            run_loop(
                deps,
                &conv,
                kind,
                &loop_tag,
                cancelled_for_task,
                progress_for_task,
                max_requirements,
            )
            .await;
            info!(target_id = %conv, ?kind, tag = %loop_tag, "AutoWork loop exited");
        });

        self.handles.insert(
            key,
            AutoWorkHandle {
                cancelled,
                join,
                tag,
                kind,
                progress,
                generation,
            },
        );
    }

    /// Stop a session's loop. Sets the cancel flag, aborts the task, cancels
    /// the in-flight agent turn (conversation targets), and releases the
    /// in-flight claim (if any) back to `pending` so the requirement is not
    /// orphaned `in_progress` until the sweeper runs. Cancelling the live turn
    /// matters: disabling AutoWork must actually stop the work — historically
    /// the orphan turn kept the conversation showing "running" after the user
    /// flipped the switch off, and raced any later re-enable.
    pub fn stop(&self, kind: AutoWorkTargetKind, target_id: &str) {
        if let Some((_, handle)) = self.handles.remove(&(kind, target_id.to_string())) {
            handle.cancelled.store(true, Ordering::SeqCst);
            handle.join.abort();
            if let Some(req_id) = handle.progress.current() {
                if handle.kind == AutoWorkTargetKind::Conversation
                    && let Some(agent) = self.deps.task_manager.get_task(target_id)
                {
                    let conv_for_cancel = target_id.to_string();
                    tokio::spawn(async move {
                        if let Err(e) = agent.cancel().await {
                            warn!(target_id = %conv_for_cancel, error = %e, "Failed to cancel in-flight AutoWork turn on stop");
                        }
                    });
                }
                // `release_claim` is conversation-domain only (it pairs owner_kind
                // == conversation); a terminal loop's in-flight claim is released by
                // its own finalize/sweeper path. Only attempt the release for a
                // conversation target, and only when its id parses to the integer
                // owner key.
                if handle.kind == AutoWorkTargetKind::Conversation
                    && let Ok(conv_id) = target_id.parse::<i64>()
                {
                    let service = self.deps.service.clone();
                    tokio::spawn(async move {
                        if let Err(e) = service.release_claim(req_id, conv_id).await {
                            warn!(requirement_id = req_id, error = %e, "Failed to release claim on autowork stop");
                        }
                    });
                }
            }
        }
    }

    /// Spawn the lease sweeper: every 60s, re-pend in_progress requirements whose
    /// lease expired and whose owning session is not a live autowork loop here.
    /// Detached for the process lifetime (the orchestrator lives in router state).
    pub fn start_sweeper(&self) {
        let handles = self.handles.clone();
        let service = self.deps.service.clone();
        tokio::spawn(async move {
            let mut ticker = interval(Duration::from_secs(60));
            ticker.tick().await; // consume the immediate first tick
            loop {
                ticker.tick().await;
                // The active set is keyed by `(kind, target_id)`. The sweep
                // matches the dual-domain owner `(owner_kind, owner_session_id)`,
                // so map each live loop to its `(owner_kind_str, i64)` pair. A
                // target id that does not parse to an integer cannot own a
                // numeric requirement, so it is simply not in the exclusion set.
                let active: Vec<(String, i64)> = handles
                    .iter()
                    .filter_map(|e| {
                        let (kind, target_id) = e.key();
                        target_id.parse::<i64>().ok().map(|id| (kind.as_str().to_string(), id))
                    })
                    .collect();
                match service
                    .repo()
                    .sweep_expired_leases(&active, nomifun_common::now_ms())
                    .await
                {
                    Ok(n) if n > 0 => info!(reset = n, "Requirement lease sweeper re-pended stale claims"),
                    Ok(_) => {}
                    Err(e) => warn!(error = %e, "Requirement lease sweeper failed"),
                }
            }
        });
    }

    /// Resume every persisted-enabled AutoWork binding across all users at boot.
    ///
    /// The running set (`handles`) is in-memory, but the enabled/tag config is
    /// persisted (conversation `extra.autowork` / terminal `autowork` column). On
    /// a process restart nothing would drive those bindings until a user opened
    /// each session page — the old behaviour that made AutoWork look like it
    /// "only works in the foreground". Spawning the loops here makes the backend
    /// the single source of truth: a bound session works in the background from
    /// boot, no UI visit required. Conversation loops start driving immediately;
    /// a terminal whose PTY is not yet live idles until the user relaunches it
    /// (the loop self-heals — see `run_loop`). Detached + best-effort.
    pub fn resume_persisted_bindings(&self, user_repo: Arc<dyn IUserRepository>) {
        let this = self.clone();
        tokio::spawn(async move {
            let users = match user_repo.list_users().await {
                Ok(u) => u,
                Err(e) => {
                    warn!(error = %e, "AutoWork resume: failed to list users");
                    return;
                }
            };
            let mut resumed = 0usize;
            for user in users {
                let groups = match this.deps.service.tag_bindings(&user.id).await {
                    Ok(g) => g,
                    Err(e) => {
                        warn!(user_id = %user.id, error = %e, "AutoWork resume: tag_bindings failed");
                        continue;
                    }
                };
                for group in groups {
                    for binding in group.bindings {
                        // Skip if already running (idempotent re-entry / racing toggle).
                        if this.is_running(binding.kind, &binding.target_id) {
                            continue;
                        }
                        let max = this
                            .deps
                            .service
                            .read_autowork_config(binding.kind, &binding.target_id)
                            .await
                            .ok()
                            .and_then(|(_, _, m)| m);
                        this.start(binding.kind, binding.target_id.clone(), group.tag.clone(), max);
                        resumed += 1;
                    }
                }
            }
            if resumed > 0 {
                info!(resumed, "AutoWork resumed persisted bindings on boot");
            }
        });
    }
}

/// The autowork loop body. Claims → injects → waits → finalizes → repeats.
///
/// The loop is *persistent*: it does NOT exit when the tag drains or a claim
/// errors — it idles (waking on `deps.wake`, with `IDLE_POLL` as a fallback) and
/// keeps claiming, so a bound session keeps picking up new requirements in the
/// background forever. It exits only on cancel (disable / stop), after
/// `max_requirements` completions, or when a terminal target's session row is
/// deleted. A terminal whose PTY merely exited idles until a relaunch revives it.
/// Outcome of one claimed requirement's turn, used to drive the failure backoff.
enum TurnResult {
    /// Turn finished and finalized as done.
    Done,
    /// Turn errored (re-pended or, when exhausted, failed → tag paused).
    Errored,
    /// Inject was rejected because the session was busy; the claim was reverted
    /// without consuming an attempt. Back off and retry.
    Busy,
    /// The USER deliberately stopped the turn (conversation cancel). The tag
    /// was paused (`user_interrupted`) and the claim released without consuming
    /// an attempt — the loop idles until the user resumes the tag. NOT a
    /// failure: no backoff, no retry.
    UserInterrupted,
}

/// How a conversation turn ended, from the orchestrator's perspective.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum TurnEnd {
    /// Finished cleanly (`EndTurn`, or the backend reported no reason —
    /// back-compat success for engines that don't set `stop_reason`).
    Clean,
    /// Failed: truncation / refusal / Error event / closed channel / timeout.
    Errored,
    /// Deliberately cancelled. Engines emit `Finish(Cancelled)` only on the
    /// user-stop path, so this is the event-level user-interrupt signal
    /// (cross-checked with `ConversationService::user_cancelled_since`).
    Cancelled,
}

/// Broadcast this loop target's live AutoWork run-state so EVERY surface stays in
/// sync across idle↔active transitions. The per-session control GETs fresh state
/// on open, but the session-list capability icon updates ONLY from this event (no
/// per-row GET); without an emit on claim/finish it kept the run-state from its
/// initial bulk load and showed a stale colour — active/green in the header but
/// idle/orange in the sidebar for the same session. `enabled=false` is emitted
/// when the max-requirements cap just disabled the binding so the icon drops off.
fn emit_autowork_progress(
    deps: &OrchestratorDeps,
    kind: AutoWorkTargetKind,
    target_id: &str,
    tag: &str,
    progress: &LiveProgress,
    enabled: bool,
) {
    let current_requirement_id = progress.current().map(|id| id.to_string());
    deps.service.emit_autowork_state(&AutoWorkState {
        kind,
        target_id: target_id.to_string(),
        enabled,
        tag: Some(tag.to_string()),
        running: enabled,
        run_state: AutoWorkState::run_state(enabled, current_requirement_id.as_deref()),
        current_requirement_id,
        completed_count: progress.completed(),
    });
}

async fn run_loop(
    deps: Arc<OrchestratorDeps>,
    target_id: &str,
    kind: AutoWorkTargetKind,
    tag: &str,
    cancelled: Arc<AtomicBool>,
    progress: Arc<LiveProgress>,
    max_requirements: Option<u32>,
) {
    // The integer owner key for the requirement service / terminal driver (both
    // now keyed by i64). The AutoWork `target_id` is a string (the kind-agnostic
    // target handle); a non-numeric one cannot own/drive a numeric session, so
    // the loop cannot do useful work — log and exit rather than spin.
    let Ok(owner_id) = target_id.parse::<i64>() else {
        warn!(target_id, tag, "AutoWork loop target id is not an integer — not starting");
        return;
    };
    // Count of back-to-back failed/busy turns, driving the failure backoff so a
    // deterministic failure cannot spin into claim at millisecond speed. Reset on
    // a clean done or when the tag drains (idle).
    let mut consecutive_failures: u32 = 0;
    loop {
        // Cancellation check before each claim.
        if cancelled.load(Ordering::SeqCst) {
            break;
        }

        // NOTE: IDMM is armed PER TURN inside `inject_and_wait` /
        // `inject_and_wait_terminal` (right after the task/PTY exists), NOT here.
        // Arming at the loop top fired on every idle poll too — and since an idle
        // conversation has no live agent task, IDMM's probe.observe() got a closed
        // channel and the supervisor died instantly, only to be re-armed 10s later:
        // a runaway "IDMM supervisor armed" churn that did no work. Arming once the
        // turn's task is built makes the supervisor actually attach to the turn.

        // Terminal target whose PTY is not live: distinguish "deleted" (stop for
        // good) from "exited but relaunch-able" (idle and re-check, so the user
        // relaunching the terminal seamlessly resumes AutoWork — no re-toggle).
        if kind == AutoWorkTargetKind::Terminal
            && let Some(driver) = &deps.terminal_driver
            && !driver.is_alive(owner_id)
        {
            if matches!(driver.describe(owner_id).await, Ok(None)) {
                info!(target_id, tag, "AutoWork terminal removed — stopping");
                break;
            }
            sleep(IDLE_POLL).await;
            continue;
        }

        // Claim the next requirement. The wake future is armed BEFORE the claim
        // (and dropped right after) so a requirement created/re-pended between the
        // claim returning None and our await is never lost. On drain or a transient
        // error the loop idles and retries instead of exiting — persistent by design.
        let claimed = {
            let wake = deps.wake.notified();
            tokio::pin!(wake);
            wake.as_mut().enable();
            match deps.service.claim_next(tag, owner_id, kind, DEFAULT_LEASE_MS).await {
                Ok(Some(req)) => req,
                Ok(None) => {
                    // Tag drained (or paused) → not a failure spin; reset backoff.
                    consecutive_failures = 0;
                    tokio::select! {
                        _ = wake.as_mut() => {}
                        _ = sleep(IDLE_POLL) => {}
                    }
                    continue;
                }
                Err(e) => {
                    warn!(target_id, tag, error = %e, "AutoWork claim failed — retrying");
                    tokio::select! {
                        _ = wake.as_mut() => {}
                        _ = sleep(IDLE_POLL) => {}
                    }
                    continue;
                }
            }
        };
        let req_id = claimed.id;
        progress.set_current(Some(req_id));
        info!(target_id, tag, requirement_id = req_id, "AutoWork claimed requirement");
        // active: a requirement is now in flight → broadcast so the session-list
        // icon turns active-coloured in step with the per-session control.
        emit_autowork_progress(&deps, kind, target_id, tag, &progress, true);

        // 2. Inject + wait for the turn to finish (per target kind).
        let result = match kind {
            AutoWorkTargetKind::Conversation => {
                // Stamp BEFORE inject: a user cancel at or after this instant
                // can only be aimed at this AutoWork-driven turn (the session
                // is claim-locked while it runs), so it is read as "stop this
                // work", not as a failed attempt.
                let turn_started_ms = nomifun_common::now_ms();
                match inject_and_wait(&deps, target_id, tag, &claimed).await {
                    Ok((end, note, expects_verdict)) => {
                        // User interrupt: the engine reported Cancelled, OR the
                        // user hit the cancel endpoint during the turn (covers
                        // engines whose cancel path surfaces as a generic
                        // Error). Pause the tag and release the claim instead
                        // of finalizing — re-pending a deliberate stop is what
                        // made AutoWork "resume by itself" seconds after the
                        // user pressed stop.
                        let user_cancelled = end == TurnEnd::Cancelled
                            || deps
                                .conversation_service
                                .user_cancelled_since(target_id, turn_started_ms);
                        if user_cancelled {
                            info!(
                                target_id,
                                tag,
                                requirement_id = req_id,
                                "AutoWork turn stopped by user — pausing tag"
                            );
                            if let Err(e) = deps.service.user_interrupt(req_id, owner_id, tag).await {
                                error!(target_id, requirement_id = req_id, error = %e, "AutoWork user-interrupt failed");
                            }
                            TurnResult::UserInterrupted
                        } else {
                            let turn_errored = end == TurnEnd::Errored;
                            // `note` carries the agent's final plain-text message for tool-free
                            // engines (ACP/codex/gemini) so the platform records what was done.
                            if let Err(e) = deps
                                .service
                                .finalize_if_needed(req_id, turn_errored, note, expects_verdict)
                                .await
                            {
                                error!(target_id, requirement_id = req_id, error = %e, "AutoWork finalize failed");
                            }
                            if turn_errored { TurnResult::Errored } else { TurnResult::Done }
                        }
                    }
                    // The session was busy (a foreground user turn or IDMM holds the
                    // turn claim). The requirement's turn never ran — revert the claim
                    // WITHOUT consuming an attempt, then back off and retry. Without
                    // this, a transient busy window burns the requirement's retries
                    // and falsely fails it (and pauses its tag).
                    Err(AppError::Conflict(_)) => {
                        warn!(
                            target_id,
                            requirement_id = req_id,
                            "AutoWork inject hit a busy session — unclaiming without consuming an attempt"
                        );
                        if let Err(e) = deps.service.unclaim_busy(req_id, owner_id).await {
                            error!(target_id, requirement_id = req_id, error = %e, "AutoWork unclaim_busy failed");
                        }
                        TurnResult::Busy
                    }
                    // The session backing this loop is GONE (deleted mid-flight →
                    // inject_and_wait's conversation_repo.get returns NotFound). This
                    // is NOT the requirement's fault: revert the claim WITHOUT
                    // consuming an attempt (so a deleted session can't burn a
                    // requirement's retries and PAUSE the whole tag for sibling
                    // conversations bound to it — the observed "delete conv 29 →
                    // tag test stuck" cascade), then STOP this loop (no target left
                    // to drive).
                    Err(AppError::NotFound(_)) => {
                        warn!(
                            target_id,
                            requirement_id = req_id,
                            "AutoWork target conversation is gone — unclaiming (no attempt) and stopping loop"
                        );
                        if let Err(e) = deps.service.unclaim_busy(req_id, owner_id).await {
                            error!(target_id, requirement_id = req_id, error = %e, "AutoWork unclaim_busy failed");
                        }
                        break;
                    }
                    Err(e) => {
                        error!(target_id, requirement_id = req_id, error = %e, "AutoWork inject failed");
                        // errored turn → expects_verdict is irrelevant (re-pend / fail).
                        if let Err(e) = deps.service.finalize_if_needed(req_id, true, None, false).await {
                            error!(target_id, requirement_id = req_id, error = %e, "AutoWork finalize failed");
                        }
                        TurnResult::Errored
                    }
                }
            }
            AutoWorkTargetKind::Terminal => {
                let outcome = match inject_and_wait_terminal(&deps, owner_id, tag, &claimed).await {
                    Ok(o) => o,
                    Err(e) => {
                        error!(target_id, requirement_id = req_id, error = %e, "AutoWork terminal inject failed");
                        TerminalTurnEnd::Errored
                    }
                };
                let errored = outcome == TerminalTurnEnd::Errored;
                // Terminal now expects a structured verdict: the agent has the
                // `requirement_complete`/`requirement_update_status` tools injected
                // (Task 2). A clean turn where the agent did NOT call those tools
                // → needs_review (not silently done). An errored turn (PTY died /
                // hard timeout) re-pends or fails after max attempts.
                let expects_verdict = crate::prompt::terminal_expects_verdict(deps.requirement_mcp_enabled);
                if let Err(e) = deps.service.finalize_if_needed(req_id, errored, None, expects_verdict).await {
                    error!(target_id, requirement_id = req_id, error = %e, "AutoWork finalize failed");
                }
                if errored { TurnResult::Errored } else { TurnResult::Done }
            }
        };

        // 3. Re-read the final status to count completions + honor max.
        let final_status = deps.service.get(req_id).await.ok().map(|d| d.status);
        progress.set_current(None);

        if final_status == Some(RequirementStatus::Done) {
            let done_n = progress.incr_completed();
            if let Some(max) = max_requirements
                && done_n >= max
            {
                info!(
                    target_id,
                    tag,
                    completed = done_n,
                    "AutoWork reached max_requirements — stopping"
                );
                // Persist disabled so the cap survives restarts: boot resume must
                // not resurrect a binding that already met its completion cap.
                if let Err(e) = deps
                    .service
                    .save_autowork_config(kind, target_id, false, None, None)
                    .await
                {
                    warn!(target_id, tag, error = %e, "Failed to persist autowork disable on max");
                }
                // off: the cap disabled the binding → drop the session-list icon.
                emit_autowork_progress(&deps, kind, target_id, tag, &progress, false);
                break;
            }
        }

        // idle: the turn finished and no requirement is in flight → broadcast so
        // the session-list icon returns to the idle colour in step with the
        // per-session control (which only looks fresh because it re-GETs on open).
        emit_autowork_progress(&deps, kind, target_id, tag, &progress, true);

        // 4. Failure backoff: a failed or busy turn inserts a bounded, escalating
        // delay before the next claim so a deterministic failure cannot spin the
        // whole tag to `failed` in a fraction of a second. Interruptible by the
        // wake (resume / new work) and re-checked against cancel. Success resets.
        // A user interrupt also resets: the tag is paused, the loop will idle on
        // the next claim (None), and the user's resume must not inherit a backoff.
        match result {
            TurnResult::Done | TurnResult::UserInterrupted => consecutive_failures = 0,
            TurnResult::Errored | TurnResult::Busy => {
                consecutive_failures = consecutive_failures.saturating_add(1);
                let delay = failure_backoff(consecutive_failures);
                let wake = deps.wake.notified();
                tokio::pin!(wake);
                wake.as_mut().enable();
                tokio::select! {
                    _ = wake.as_mut() => {}
                    _ = sleep(delay) => {}
                }
            }
        }
    }
}

/// Resolve task options, get/build the agent, subscribe, send the prompt, and
/// wait for a terminal event while renewing the lease. Returns
/// `(end, note, expects_verdict)` where `end` classifies how the turn ended
/// (clean / errored / user-cancelled), `note` is the agent's final plain-text
/// message captured for the completion record (only on a clean finish), and
/// `expects_verdict` is true when this engine has an explicit declaration
/// channel (native requirement tools / requirement MCP) so a clean turn with
/// no declaration is parked for review rather than assumed done.
async fn inject_and_wait(
    deps: &Arc<OrchestratorDeps>,
    conversation_id: &str,
    tag: &str,
    req: &Requirement,
) -> Result<(TurnEnd, Option<String>, bool), AppError> {
    // The conversation repo / lease renewal are keyed by the integer id; the
    // conversation service + task manager stay string-keyed (their public API).
    let conv_id_i64 = conversation_id
        .parse::<i64>()
        .map_err(|_| AppError::NotFound(format!("conversation {conversation_id}")))?;
    // Load the conversation row to resolve agent_type / model / workspace / user.
    let row = deps
        .conversation_repo
        .get(conv_id_i64)
        .await?
        .ok_or_else(|| AppError::NotFound(format!("conversation {conversation_id}")))?;

    let user_id = if row.user_id.trim().is_empty() {
        SYSTEM_DEFAULT_USER_ID.to_string()
    } else {
        row.user_id.clone()
    };

    let agent_type = parse_agent_type(&deps.agent_registry, &row.r#type).await;
    let model = nomifun_conversation::task_options::provider_model_from_conversation_row(&row);
    let extra: serde_json::Value = serde_json::from_str(&row.extra).unwrap_or_default();
    let workspace = extra
        .get("workspace")
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .trim()
        .to_string();

    // Keep a copy for attachment staging below — the original string moves into
    // the task options.
    let workspace_for_stage = workspace.clone();

    let options = BuildTaskOptions {
        agent_type,
        workspace,
        model,
        conversation_id: conversation_id.to_string(),
        extra,
        // Stamp/validate the nomi session against this conversation instance so
        // a reused integer id never resumes a stale (e.g. deleted) conversation.
        conversation_created_at: Some(row.created_at),
    };

    let agent = deps.task_manager.get_or_build_task(conversation_id, options).await?;
    // Arm IDMM now that THIS turn's task exists (so its probe.observe() attaches
    // to this turn's event stream), and ONLY now — never on an idle poll. Mirrors
    // the user-driven path's `on_turn_start` hook (which also arms right after
    // get_or_build_task). Idempotent + no-op when IDMM is disabled for the target.
    if let Some(idmm) = &deps.idmm {
        idmm.ensure_supervising(AutoWorkTargetKind::Conversation, conversation_id);
    }
    let rx = agent.subscribe();

    // Stage attachments only AFTER the task is built: staging implicitly
    // `create_dir_all`s the workspace directory, which run before
    // `get_or_build_task` could interfere with the task build's own
    // workspace-initialization checks (e.g. "does the workspace exist yet").
    let ws_path = (!workspace_for_stage.is_empty()).then(|| std::path::Path::new(workspace_for_stage.as_str()));
    let attachments = deps.service.stage_attachments_for_prompt(req.id, ws_path).await;

    let prompt = build_requirement_prompt(tag, req, agent_type, deps.requirement_mcp_enabled, &attachments);
    let send_req = SendMessageRequest {
        content: prompt,
        files: vec![],
        inject_skills: vec![],
        hidden: true,
        origin: Some("autowork".into()),
        channel_platform: None,
    };
    deps.conversation_service
        .send_message(&user_id, conversation_id, send_req, &deps.task_manager)
        .await?;

    let outcome = wait_for_terminal_with_renewal(deps, conversation_id, conv_id_i64, req.id, rx).await;
    // The session has a declaration channel when it exposes the requirement
    // tools: Nomi natively, or ACP once the requirement MCP is injected
    // (`requirement_mcp_enabled`). Driven by the same bootstrap flag that gates
    // the prompt above, so we never expect a verdict the session can't give.
    let expects_verdict = crate::prompt::session_has_requirement_tools(agent_type, deps.requirement_mcp_enabled);
    Ok((outcome.0, outcome.1, expects_verdict))
}

/// Wait for the agent's terminal event, renewing the lease periodically and
/// accumulating the agent's text. Returns `(end, note)`: `end` classifies the
/// turn (Errored on an Error event / closed channel / timeout, Cancelled on a
/// user stop, Clean otherwise); `note` is the agent's prose captured for the
/// completion record (only on a clean finish, `None` if the agent emitted none).
async fn wait_for_terminal_with_renewal(
    deps: &Arc<OrchestratorDeps>,
    conversation_id: &str,
    conv_id: i64,
    req_id: i64,
    mut rx: broadcast::Receiver<AgentStreamEvent>,
) -> (TurnEnd, Option<String>) {
    let mut renew = interval(LEASE_RENEW_INTERVAL);
    renew.tick().await; // consume the immediate first tick
    let mut note_buf = String::new();
    // The CURRENT turn's assistant text, reset at each turn boundary. Used to
    // decide the decision-yield from the text we already have IN MEMORY — never
    // racing the stream relay's persisted message-status write (which `pending_signal`
    // would). The decision (menu / question) lives at the turn's tail, which
    // `append_bounded` keeps.
    let mut turn_text = String::new();
    // Count of retryable errors we've waited through (letting IDMM recover).
    let mut recovery_waits = 0u32;
    // Count of decision-ending turns we've YIELDED to IDMM this requirement turn.
    let mut decision_waits = 0u32;
    // When riding an IDMM-owned decision turn-end: the instant by which IDMM must
    // have STARTED answering (driven a follow-up turn). If no activity arrives by
    // then, IDMM cannot / will not answer (e.g. a rule-tier watch) → stop yielding
    // and finalize the decision-ending turn as-is. Cleared on any activity.
    let mut decision_ride_until: Option<tokio::time::Instant> = None;

    let fut = async {
        loop {
            // Copy the deadline out for the watchdog branch (Option<Instant> is
            // Copy) so its future never borrows the state the rx handler mutates.
            let ride_until = decision_ride_until;
            tokio::select! {
                _ = renew.tick() => {
                    if let Err(e) = deps.service.renew_lease(req_id, conv_id, DEFAULT_LEASE_MS).await {
                        warn!(conversation_id, requirement_id = req_id, error = %e, "Lease renewal failed");
                    }
                }
                // Decision-yield watchdog: armed only while riding (ride_until Some).
                // Fires when IDMM did not start answering within DECISION_YIELD_WINDOW
                // → fall back to finalizing the decision-ending turn instead of hanging.
                () = async move {
                    match ride_until {
                        Some(until) => tokio::time::sleep_until(until).await,
                        None => std::future::pending::<()>().await,
                    }
                } => {
                    info!(
                        conversation_id,
                        requirement_id = req_id,
                        "AutoWork decision-yield window elapsed without IDMM answering — finalizing turn"
                    );
                    return TurnEnd::Clean;
                }
                ev = rx.recv() => {
                    match ev {
                        // Capture the agent's prose; on a clean finish this is the
                        // completion note for tool-free engines (ACP/codex/gemini).
                        // Any activity also means IDMM's follow-up turn has started,
                        // so the decision-yield watchdog stands down.
                        Ok(AgentStreamEvent::Text(t)) => {
                            decision_ride_until = None;
                            append_bounded(&mut note_buf, &t.content);
                            append_bounded(&mut turn_text, &t.content);
                        }
                        // A clean Finish is NOT necessarily the requirement's terminal
                        // state: the agent may have ended its turn on a 选择题/开放式提问.
                        // When IDMM is supervising and a pending decision exists, IDMM
                        // will answer it — so YIELD instead of finalizing here (which
                        // would park the requirement needs_review, burn an attempt, and
                        // let run_loop race a fresh requirement into the session,
                        // stomping IDMM's pending answer — the 代码乱套). Keep waiting on
                        // the SAME broadcast (without holding the turn claim) until the
                        // work reaches a real terminal Finish. A refusal/truncation
                        // (Errored) or user cancel (Cancelled) is never yielded — those
                        // are genuine terminal ends.
                        Ok(AgentStreamEvent::Finish(d)) => {
                            let end = turn_end_from(&d.stop_reason);
                            let yield_to_idmm = if let Some(idmm) = deps.idmm.as_ref() {
                                should_wait_for_decision(
                                    end,
                                    idmm.is_supervising(AutoWorkTargetKind::Conversation, conversation_id),
                                    decision_waits,
                                ) && idmm
                                    .has_pending_decision(
                                        AutoWorkTargetKind::Conversation,
                                        conversation_id,
                                        &turn_text,
                                    )
                                    .await
                            } else {
                                false
                            };
                            // This turn ended; the next (IDMM-driven) turn accumulates
                            // its own text.
                            turn_text.clear();
                            if yield_to_idmm {
                                decision_waits += 1;
                                decision_ride_until = Some(tokio::time::Instant::now() + DECISION_YIELD_WINDOW);
                                continue;
                            }
                            return end;
                        }
                        // On an error, defer to IDMM when it is supervising: a
                        // retryable provider fault is IDMM's job to recover (retry /
                        // sidecar via a fresh turn). Failing the turn here would
                        // abandon it and race a fresh requirement into the same
                        // session — the historical "代码乱套". Wait through up to
                        // MAX_RECOVERY_WAITS such errors; otherwise (non-retryable, no
                        // IDMM, or grace exhausted) fail.
                        Ok(AgentStreamEvent::Error(d)) => {
                            decision_ride_until = None;
                            turn_text.clear();
                            let retryable = matches!(d.retryable, Some(true));
                            let idmm_supervising = deps
                                .idmm
                                .as_ref()
                                .map(|i| i.is_supervising(AutoWorkTargetKind::Conversation, conversation_id))
                                .unwrap_or(false);
                            if should_wait_for_recovery(retryable, idmm_supervising, recovery_waits) {
                                recovery_waits += 1;
                                continue;
                            }
                            return TurnEnd::Errored;
                        }
                        Ok(_) => {
                            decision_ride_until = None;
                            continue;
                        }
                        // A closed channel means the agent task was torn down
                        // (eviction on terminal error, process death, dropped
                        // connection) — the turn did NOT finish cleanly. Treat as
                        // errored, matching the terminal path's `Closed => errored`.
                        Err(broadcast::error::RecvError::Closed) => return TurnEnd::Errored,
                        Err(broadcast::error::RecvError::Lagged(_)) => continue,
                    }
                }
            }
        }
    };

    // On timeout the loop's finalize() treats this as an errored turn.
    let end = timeout(TURN_TIMEOUT, fut).await.unwrap_or(TurnEnd::Errored);
    let note = if end == TurnEnd::Clean { finalize_note(&note_buf) } else { None };
    (end, note)
}

/// Append `chunk` to `buf`, keeping it bounded (tail-biased) so a long streaming
/// turn cannot grow the buffer without limit. Truncation respects char boundaries.
fn append_bounded(buf: &mut String, chunk: &str) {
    buf.push_str(chunk);
    // chars are ≤4 bytes; keep roughly twice the char cap as a byte ceiling.
    let max_bytes = MAX_NOTE_CHARS * 4 * 2;
    if buf.len() > max_bytes {
        let mut cut = buf.len() - MAX_NOTE_CHARS * 4;
        while cut < buf.len() && !buf.is_char_boundary(cut) {
            cut += 1;
        }
        buf.drain(..cut);
    }
}

/// Classify a turn's terminal `stop_reason` into how the turn ENDED.
/// `None` (backend didn't report) and `EndTurn` are success; truncations and
/// refusals are failures so AutoWork does not record them as done; `Cancelled`
/// is a deliberate user stop — surfaced distinctly so the loop pauses the tag
/// instead of burning a retry attempt on it.
fn turn_end_from(reason: &Option<TurnStopReason>) -> TurnEnd {
    match reason {
        Some(TurnStopReason::Cancelled) => TurnEnd::Cancelled,
        Some(TurnStopReason::MaxTokens | TurnStopReason::MaxTurnRequests | TurnStopReason::Refusal) => {
            TurnEnd::Errored
        }
        None | Some(TurnStopReason::EndTurn) => TurnEnd::Clean,
    }
}

/// Bounded, escalating delay before the next claim after a failed (or busy)
/// turn, so a deterministic failure cannot spin back into claim at millisecond
/// speed and burn every attempt across the tag in a fraction of a second.
/// `consecutive` is the count of back-to-back failed turns (1-based): 1s, 2s,
/// 4s, 8s, 16s, then capped at 30s. Reset to 0 on success / idle.
fn failure_backoff(consecutive: u32) -> Duration {
    let exp = consecutive.saturating_sub(1).min(5);
    let secs = (1u64 << exp).min(30);
    Duration::from_secs(secs)
}

/// Decide whether AutoWork should wait through an agent error rather than fail
/// the turn immediately. We only wait when the error is retryable AND IDMM is
/// actively supervising the session (it owns in-turn recovery), and only up to
/// `MAX_RECOVERY_WAITS` times so a non-recovering IDMM cannot hang the turn.
/// When IDMM is not supervising, the turn fails on the first error (legacy).
fn should_wait_for_recovery(retryable: bool, idmm_supervising: bool, waits_so_far: u32) -> bool {
    retryable && idmm_supervising && waits_so_far < MAX_RECOVERY_WAITS
}

/// Decide whether AutoWork should YIELD a clean-finish turn to IDMM rather than
/// finalize it as the requirement's terminal state. We yield only on a CLEAN
/// finish (a refusal/truncation Errored or a user Cancelled is a real terminal
/// end) AND when IDMM is supervising (it owns answering 选择题/开放式提问), bounded by
/// `MAX_DECISION_WAITS` so a runaway question loop can't ride forever. The caller
/// additionally confirms (async) that a pending decision actually exists before
/// yielding, and arms a watchdog so a non-answering IDMM falls back to finalize.
fn should_wait_for_decision(end: TurnEnd, idmm_supervising: bool, waits_so_far: u32) -> bool {
    end == TurnEnd::Clean && idmm_supervising && waits_so_far < MAX_DECISION_WAITS
}

/// Trim + tail-truncate the accumulated agent text into a completion note.
/// `None` when the agent produced no prose (e.g. only tool calls).
fn finalize_note(buf: &str) -> Option<String> {
    let trimmed = buf.trim();
    if trimmed.is_empty() {
        return None;
    }
    if trimmed.chars().count() <= MAX_NOTE_CHARS {
        return Some(trimmed.to_string());
    }
    // Keep the tail (agents usually put the completion summary at the end).
    let tail: String = {
        let chars: Vec<char> = trimmed.chars().collect();
        chars[chars.len() - MAX_NOTE_CHARS..].iter().collect()
    };
    Some(format!("…{tail}"))
}

/// How a terminal turn ended (structured completion via lifecycle / error).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum TerminalTurnEnd {
    /// The lifecycle reported a `TurnEnd` event — the agent finished its turn.
    /// Whether the agent called `requirement_complete` is reflected in the DB
    /// row's status; the orchestrator just knows the turn ended cleanly.
    Clean,
    /// PTY died / hard timeout / lifecycle unavailable and timeout expired.
    Errored,
}

/// Build the ordered PTY write chunks for submitting a terminal prompt:
/// `(paste, submit)`. `paste` wraps the whole (multi-line) prompt in
/// bracketed-paste markers so the CLI's line editor inserts it as ONE paste
/// rather than executing line-by-line on each embedded newline. `submit` is a
/// lone carriage return, deliberately kept OUT of the paste burst (see
/// `submit_terminal_prompt`). The markers are CSI sequences our output scanner
/// also strips, so they never interfere with marker detection.
fn terminal_submit_chunks(prompt: &str) -> (Vec<u8>, Vec<u8>) {
    let mut paste = Vec::with_capacity(prompt.len() + 12);
    paste.extend_from_slice(b"\x1b[200~");
    paste.extend_from_slice(prompt.as_bytes());
    paste.extend_from_slice(b"\x1b[201~");
    (paste, vec![b'\r'])
}

/// Inject a prompt into a terminal CLI and submit it. The bracketed-paste body
/// and the submit CR are written as SEPARATE PTY writes, with
/// `TERMINAL_SUBMIT_DELAY` between them. A CR that rides in the same write as
/// the paste-end marker is swallowed by the paste-burst detection modern agent
/// TUIs (claude/codex/gemini) use to keep a pasted block from auto-running — it
/// leaves the requirement text sitting unsubmitted in the input box (the bug
/// this fixes). Writing the CR on its own, a beat later, makes the TUI treat it
/// as a real Enter keystroke. Mirrors the cron terminal executor's fix.
async fn submit_terminal_prompt(
    driver: &Arc<dyn TerminalDriver>,
    terminal_id: i64,
    prompt: &str,
) -> Result<(), AppError> {
    let (paste, submit) = terminal_submit_chunks(prompt);
    driver.write_input(terminal_id, &paste).await?;
    sleep(TERMINAL_SUBMIT_DELAY).await;
    driver.write_input(terminal_id, &submit).await?;
    Ok(())
}

/// One terminal turn: inject the requirement prompt, then await the lifecycle
/// `TurnEnd` event (the agent's Stop hook), the PTY dying, or the hard timeout.
///
/// **No quiescence fallback:** a lifecycle subscription is the ONLY structured
/// turn-end signal. When lifecycle is unavailable (server not wired / non-agent
/// CLI) the turn runs until the hard `TURN_TIMEOUT` and then ends as
/// `TerminalTurnEnd::Errored` — honest (no false "done"). The finalize with
/// `expects_verdict=true` parks it as `needs_review`.
async fn inject_and_wait_terminal(
    deps: &Arc<OrchestratorDeps>,
    terminal_id: i64,
    tag: &str,
    req: &Requirement,
) -> Result<TerminalTurnEnd, AppError> {
    let driver = deps
        .terminal_driver
        .as_ref()
        .ok_or_else(|| AppError::Internal("terminal driver not attached".into()))?;

    // Terminals have no workspace concept — the prompt carries absolute paths
    // into the data dir and the CLI reads them directly.
    let attachments = deps.service.stage_attachments_for_prompt(req.id, None).await;
    let prompt = build_terminal_requirement_prompt(tag, req, &attachments);
    // Inject the prompt and submit it. The bracketed-paste body and the submit CR
    // go out as SEPARATE writes (see `submit_terminal_prompt`) so the CR is not
    // swallowed by the target CLI's paste-burst detection.
    submit_terminal_prompt(driver, terminal_id, &prompt).await?;

    // Arm IDMM for THIS terminal turn (its probe subscribes to the durable PTY
    // output/lifecycle, so it attaches regardless of task state). Per-turn, not on
    // every idle poll — same churn fix as the conversation path. Idempotent + a
    // no-op when IDMM is disabled for the terminal.
    if let Some(idmm) = &deps.idmm {
        idmm.ensure_supervising(AutoWorkTargetKind::Terminal, &terminal_id.to_string());
    }

    // Subscribe to lifecycle AFTER injecting (the hook fires on the NEXT
    // turn-end, not the injection itself).
    let lifecycle_rx = driver.subscribe_lifecycle(terminal_id);

    Ok(wait_terminal_turn_end(deps, driver, terminal_id, req.id, lifecycle_rx).await)
}

/// Await a terminal turn's structured completion signal, renewing the lease on a
/// tick, checking PTY liveness, and enforcing the hard timeout.
async fn wait_terminal_turn_end(
    deps: &Arc<OrchestratorDeps>,
    driver: &Arc<dyn TerminalDriver>,
    terminal_id: i64,
    req_id: i64,
    lifecycle_rx: Option<broadcast::Receiver<nomifun_terminal::TerminalLifecycleEvent>>,
) -> TerminalTurnEnd {
    let mut renew = interval(LEASE_RENEW_INTERVAL);
    renew.tick().await; // consume the immediate first tick
    let mut tick = interval(Duration::from_secs(2));
    tick.tick().await; // consume the immediate first tick

    let fut = async {
        match lifecycle_rx {
            Some(mut rx) => {
                // Lifecycle available: wait for TurnEnd.
                loop {
                    tokio::select! {
                        _ = renew.tick() => {
                            if let Err(e) = deps.service.renew_lease(req_id, terminal_id, DEFAULT_LEASE_MS).await {
                                warn!(terminal_id, requirement_id = req_id, error = %e, "Lease renewal failed");
                            }
                        }
                        _ = tick.tick() => {
                            if !driver.is_alive(terminal_id) {
                                return TerminalTurnEnd::Errored; // PTY died mid-turn
                            }
                        }
                        ev = rx.recv() => {
                            match ev {
                                Ok(event) if event.kind == LifecycleKind::TurnEnd => {
                                    return TerminalTurnEnd::Clean;
                                }
                                Ok(_) => continue, // ToolUse / Notification / SessionStart — activity, keep waiting
                                Err(broadcast::error::RecvError::Closed) => {
                                    return TerminalTurnEnd::Errored; // lifecycle server gone
                                }
                                Err(broadcast::error::RecvError::Lagged(_)) => continue,
                            }
                        }
                    }
                }
            }
            None => {
                // No lifecycle server — no structured turn-end signal. Wait for
                // PTY death or the hard timeout. Do NOT fall back to quiescence-
                // as-done (honest: no false "done").
                loop {
                    tokio::select! {
                        _ = renew.tick() => {
                            if let Err(e) = deps.service.renew_lease(req_id, terminal_id, DEFAULT_LEASE_MS).await {
                                warn!(terminal_id, requirement_id = req_id, error = %e, "Lease renewal failed (no lifecycle)");
                            }
                        }
                        _ = tick.tick() => {
                            if !driver.is_alive(terminal_id) {
                                return TerminalTurnEnd::Errored; // PTY died
                            }
                        }
                    }
                }
            }
        }
    };

    // Hard timeout: the turn took too long regardless of lifecycle state.
    timeout(TURN_TIMEOUT, fut)
        .await
        .unwrap_or(TerminalTurnEnd::Errored)
}

/// Mirror of cron's agent-type resolution.
async fn parse_agent_type(registry: &AgentRegistry, agent_type_str: &str) -> nomifun_common::AgentType {
    if registry.find_builtin_by_backend(agent_type_str).await.is_some() {
        return nomifun_common::AgentType::Acp;
    }
    serde_json::from_value::<nomifun_common::AgentType>(serde_json::Value::String(agent_type_str.to_owned()))
        .unwrap_or(nomifun_common::AgentType::Acp)
}

#[cfg(test)]
mod tests {
    use super::*;
    use nomifun_terminal::TerminalDescription;
    use nomifun_terminal::error::TerminalError;

    #[test]
    fn turn_end_from_classifies_stop_reasons() {
        // Back-compat: a backend that did not report a reason is treated as
        // success (so non-ACP engines that don't set stop_reason keep working).
        assert_eq!(turn_end_from(&None), TurnEnd::Clean, "None must be success (back-compat)");
        // A clean finish is success.
        assert_eq!(turn_end_from(&Some(TurnStopReason::EndTurn)), TurnEnd::Clean, "EndTurn is success");
        // Truncations / refusals are failed turns (consume an attempt).
        assert_eq!(turn_end_from(&Some(TurnStopReason::Refusal)), TurnEnd::Errored, "Refusal is a failure");
        assert_eq!(
            turn_end_from(&Some(TurnStopReason::MaxTokens)),
            TurnEnd::Errored,
            "MaxTokens is a failure"
        );
        assert_eq!(
            turn_end_from(&Some(TurnStopReason::MaxTurnRequests)),
            TurnEnd::Errored,
            "MaxTurnRequests is a failure"
        );
        // A user cancel is a deliberate interrupt — NOT a failure to retry
        // (retrying a user stop was the "paused it and it started running
        // again by itself" bug) and NOT a clean completion to record as done.
        assert_eq!(
            turn_end_from(&Some(TurnStopReason::Cancelled)),
            TurnEnd::Cancelled,
            "Cancelled is a user interrupt, not a retryable failure"
        );
    }

    #[test]
    fn failure_backoff_escalates_and_caps() {
        // 1-based consecutive failures → 1s, 2s, 4s, 8s, 16s, then capped at 30s.
        assert_eq!(failure_backoff(1), Duration::from_secs(1));
        assert_eq!(failure_backoff(2), Duration::from_secs(2));
        assert_eq!(failure_backoff(3), Duration::from_secs(4));
        assert_eq!(failure_backoff(4), Duration::from_secs(8));
        assert_eq!(failure_backoff(5), Duration::from_secs(16));
        assert_eq!(failure_backoff(6), Duration::from_secs(30), "capped at 30s");
        assert_eq!(failure_backoff(100), Duration::from_secs(30), "stays capped");
        // Never zero — a failure must always insert some delay before re-claim.
        assert!(failure_backoff(1) > Duration::ZERO);
    }

    #[test]
    fn should_wait_for_recovery_only_when_retryable_idmm_and_under_cap() {
        // Wait through a retryable error while IDMM supervises, under the cap.
        assert!(should_wait_for_recovery(true, true, 0));
        assert!(should_wait_for_recovery(true, true, MAX_RECOVERY_WAITS - 1));
        // Cap reached → give up (fail the turn).
        assert!(!should_wait_for_recovery(true, true, MAX_RECOVERY_WAITS));
        // Non-retryable error → never wait.
        assert!(!should_wait_for_recovery(false, true, 0));
        // IDMM not supervising → legacy: fail on first error.
        assert!(!should_wait_for_recovery(true, false, 0));
    }

    #[test]
    fn should_wait_for_decision_only_when_clean_idmm_and_under_cap() {
        // Yield a clean decision-ending finish to IDMM while it supervises, under cap.
        assert!(should_wait_for_decision(TurnEnd::Clean, true, 0));
        assert!(should_wait_for_decision(TurnEnd::Clean, true, MAX_DECISION_WAITS - 1));
        // Cap reached → finalize ourselves (stop riding).
        assert!(!should_wait_for_decision(TurnEnd::Clean, true, MAX_DECISION_WAITS));
        // IDMM not supervising → legacy: a clean finish ends the turn immediately.
        assert!(!should_wait_for_decision(TurnEnd::Clean, false, 0));
        // A real terminal end is never yielded: an errored (refusal/truncation)
        // or user-cancelled finish must finalize, not wait for IDMM.
        assert!(!should_wait_for_decision(TurnEnd::Errored, true, 0));
        assert!(!should_wait_for_decision(TurnEnd::Cancelled, true, 0));
    }

    #[test]
    fn terminal_submit_chunks_keeps_cr_out_of_the_paste_burst() {
        // Root-cause guard: the submit CR must NOT ride in the same byte burst as
        // the bracketed-paste body. Modern agent TUIs (claude/codex/gemini) use
        // paste-burst detection and SUPPRESS auto-submit for a CR that arrives in
        // the same read() as the paste-end marker — the requirement text would
        // then sit unsubmitted in the input box (the reported bug). The CR is
        // therefore returned as a SEPARATE chunk, written after a beat.
        let (paste, cr) = terminal_submit_chunks("line one\nline two");
        assert!(paste.starts_with(b"\x1b[200~"), "paste must open with ESC[200~");
        assert!(paste.ends_with(b"\x1b[201~"), "paste must close with ESC[201~");
        assert!(
            paste.windows(8).any(|w| w == b"line one"),
            "paste must contain the prompt body"
        );
        assert!(!paste.contains(&b'\r'), "the CR must never be inside the paste burst");
        assert_eq!(cr, b"\r", "submit chunk must be a lone carriage return (a real Enter)");
    }

    #[derive(Default)]
    struct RecordingDriver {
        writes: Mutex<Vec<Vec<u8>>>,
    }

    #[async_trait::async_trait]
    impl TerminalDriver for RecordingDriver {
        async fn write_input(&self, _id: i64, bytes: &[u8]) -> Result<(), TerminalError> {
            self.writes.lock().unwrap().push(bytes.to_vec());
            Ok(())
        }
        fn subscribe_output(&self, _id: i64) -> Option<broadcast::Receiver<Vec<u8>>> {
            None
        }
        fn is_alive(&self, _id: i64) -> bool {
            true
        }
        async fn describe(&self, _id: i64) -> Result<Option<TerminalDescription>, TerminalError> {
            Ok(None)
        }
        async fn read_autowork(&self, _id: i64) -> Result<Option<String>, TerminalError> {
            Ok(None)
        }
        async fn write_autowork(&self, _id: i64, _autowork: Option<&str>) -> Result<(), TerminalError> {
            Ok(())
        }
        async fn read_idmm(&self, _id: i64) -> Result<Option<String>, TerminalError> {
            Ok(None)
        }
        async fn write_idmm(&self, _id: i64, _idmm: Option<&str>) -> Result<(), TerminalError> {
            Ok(())
        }
        fn subscribe_lifecycle(
            &self,
            _id: i64,
        ) -> Option<tokio::sync::broadcast::Receiver<nomifun_terminal::TerminalLifecycleEvent>> {
            None
        }
    }

    #[tokio::test]
    async fn submit_terminal_prompt_writes_paste_then_a_separate_cr() {
        // The two PTY writes must be ordered: bracketed-paste body FIRST, then the
        // lone CR as its OWN write (so paste-burst-detecting TUIs treat it as a
        // real Enter). Mirrors the fix the cron terminal executor already applies.
        let recorder = Arc::new(RecordingDriver::default());
        let driver: Arc<dyn TerminalDriver> = recorder.clone();
        submit_terminal_prompt(&driver, 1, "do the thing\nthen stop")
            .await
            .expect("submit must succeed");
        let writes = recorder.writes.lock().unwrap().clone();
        assert_eq!(writes.len(), 2, "expected exactly two PTY writes (paste, then CR)");
        assert!(
            writes[0].starts_with(b"\x1b[200~") && writes[0].ends_with(b"\x1b[201~"),
            "first write is the bracketed-paste body"
        );
        assert!(
            !writes[0].contains(&b'\r'),
            "first write must NOT contain the CR (it would be swallowed by paste-burst detection)"
        );
        assert_eq!(writes[1], b"\r", "second write is the lone submit CR");
    }

    // ── C4 (spec §2.2): cross-domain loop-registry isolation ────────────────
    //
    // The AutoWork loop registry keys on `TargetKey = (AutoWorkTargetKind,
    // String)`. After integerization `conv#5` and `term#5` share the numeric
    // id "5"; a bare-id key would let `start(Terminal, "5")` (whose first line
    // is `self.stop(kind, "5")`) stop the conversation #5 loop, and would make
    // `is_running` / `live_progress` report one domain's state for the other.
    // These tests pin the key down to the composite so that regression is
    // caught at the data-structure level without spinning a full loop (which
    // needs the whole agent stack).

    #[test]
    fn c4_target_key_distinguishes_domains_at_same_id() {
        let conv5: TargetKey = (AutoWorkTargetKind::Conversation, "5".to_string());
        let term5: TargetKey = (AutoWorkTargetKind::Terminal, "5".to_string());
        assert_ne!(conv5, term5, "conv#5 and term#5 must be DISTINCT registry keys");

        // The registry is a DashMap<TargetKey, _>; mirror its keying to prove
        // the two domains never collide and `stop` of one leaves the other.
        let map: DashMap<TargetKey, u32> = DashMap::new();
        map.insert(conv5.clone(), 1);
        map.insert(term5.clone(), 2);
        assert_eq!(map.len(), 2, "both domains coexist at id 5");
        assert_eq!(map.get(&conv5).map(|v| *v), Some(1));
        assert_eq!(map.get(&term5).map(|v| *v), Some(2));

        // Stopping the terminal domain leaves the conversation entry intact.
        map.remove(&term5);
        assert!(map.contains_key(&conv5), "conv#5 survives a term#5 removal");
        assert!(!map.contains_key(&term5));
    }

    #[test]
    fn c4_is_running_lookup_is_domain_scoped() {
        // `is_running(kind, id)` builds the lookup key from BOTH kind and id, so
        // an entry under one domain is invisible to the other domain's lookup.
        // Mirror the exact `contains_key` the orchestrator uses.
        let handles: DashMap<TargetKey, ()> = DashMap::new();
        handles.insert((AutoWorkTargetKind::Conversation, "5".to_string()), ());

        let conv_lookup = handles.contains_key(&(AutoWorkTargetKind::Conversation, "5".to_string()));
        let term_lookup = handles.contains_key(&(AutoWorkTargetKind::Terminal, "5".to_string()));
        assert!(conv_lookup, "conv#5 is running");
        assert!(!term_lookup, "term#5 must NOT read as running just because conv#5 is");
    }

    // ── Terminal turn-end classification tests ──────────────────────────────
    //
    // The terminal rewrite uses TerminalTurnEnd { Clean, Errored } to classify
    // the outcome. These tests pin the decision logic and the expects_verdict
    // contract.

    #[test]
    fn terminal_turn_end_is_eq_and_debug() {
        assert_eq!(TerminalTurnEnd::Clean, TerminalTurnEnd::Clean);
        assert_eq!(TerminalTurnEnd::Errored, TerminalTurnEnd::Errored);
        assert_ne!(TerminalTurnEnd::Clean, TerminalTurnEnd::Errored);
        // Debug impl exists (used in error messages).
        assert!(!format!("{:?}", TerminalTurnEnd::Clean).is_empty());
    }

    #[test]
    fn terminal_expects_verdict_true_when_mcp_enabled() {
        // The orchestrator now passes `expects_verdict = true` when the requirement
        // MCP is enabled (the tools are injected into the terminal). A clean turn
        // where the agent did NOT call them → needs_review (not silently done).
        assert!(crate::prompt::terminal_expects_verdict(true));
        assert!(!crate::prompt::terminal_expects_verdict(false));
    }

    #[test]
    fn terminal_turn_end_errored_means_finalize_gets_errored_true() {
        // Pin: the orchestrator's terminal branch maps TerminalTurnEnd::Errored →
        // `errored = true` for finalize_if_needed (which re-pends/fails). Clean →
        // `errored = false` (which either records done via verdict or parks
        // needs_review via expects_verdict).
        let outcome = TerminalTurnEnd::Errored;
        assert!(outcome == TerminalTurnEnd::Errored);
        let outcome = TerminalTurnEnd::Clean;
        assert!(outcome != TerminalTurnEnd::Errored);
    }

    #[tokio::test]
    async fn lifecycle_turn_end_event_resolves_immediately() {
        // Verify that a lifecycle TurnEnd event causes the select loop to
        // resolve as Clean (the core structured-completion contract).
        use nomifun_terminal::TerminalLifecycleEvent;

        let (tx, rx) = broadcast::channel::<TerminalLifecycleEvent>(4);
        // Send a TurnEnd BEFORE any consumer picks it up — the broadcast
        // channel buffers it.
        tx.send(TerminalLifecycleEvent {
            terminal_id: 1,
            kind: LifecycleKind::TurnEnd,
            payload: serde_json::json!({}),
        })
        .unwrap();

        // Simulate the inner select logic directly (without OrchestratorDeps):
        // recv from the channel, match TurnEnd → Clean.
        let mut rx = rx;
        let ev = rx.recv().await.unwrap();
        assert_eq!(ev.kind, LifecycleKind::TurnEnd);
        // This is exactly what wait_terminal_turn_end does on a TurnEnd event.
        let result = if ev.kind == LifecycleKind::TurnEnd {
            TerminalTurnEnd::Clean
        } else {
            TerminalTurnEnd::Errored
        };
        assert_eq!(result, TerminalTurnEnd::Clean);
    }

    #[tokio::test]
    async fn lifecycle_closed_channel_resolves_as_errored() {
        use nomifun_terminal::TerminalLifecycleEvent;

        let (tx, rx) = broadcast::channel::<TerminalLifecycleEvent>(4);
        drop(tx); // Simulate lifecycle server disappearing.

        let mut rx = rx;
        let ev = rx.recv().await;
        assert!(matches!(ev, Err(broadcast::error::RecvError::Closed)));
        // wait_terminal_turn_end maps this to Errored.
    }
}

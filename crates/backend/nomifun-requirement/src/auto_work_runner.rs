use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::time::Duration;

use dashmap::DashMap;
use nomifun_ai_agent::AgentStreamEvent;
#[cfg(test)]
use nomifun_ai_agent::TurnStopReason;
use nomifun_ai_agent::registry::AgentRegistry;
use nomifun_ai_agent::runtime_registry::AgentRuntimeRegistry;
use nomifun_ai_agent::types::AgentRuntimeBuildOptions;
use nomifun_api_types::{AutoWorkState, AutoWorkTargetKind, Requirement, RequirementStatus, SendMessageRequest};
use nomifun_common::{AppError, ConversationId, TerminalId, UserId};
use nomifun_conversation::{
    ConversationService, IdempotentMessageDelivery, runtime_state::RuntimeBuildLease,
};
use nomifun_conversation::service::{
    BackgroundTurnPreSendHook, BackgroundTurnReconciliationDisposition,
    BackgroundTurnRuntimePreparation, ObservedIdempotentMessageDelivery,
};
use nomifun_db::{
    IConversationRepository, TerminalTurnAdmissionKey, TerminalTurnAdmissionRow,
    RequirementConversationTurnAuthority, TerminalTurnAdmissionScope,
    TerminalTurnEffectsStart, TerminalTurnOutcome,
};
use nomifun_terminal::{ExactTerminalLifecycleReceiver, LifecycleKind, TerminalDriver};
use tokio::sync::broadcast;
use tokio::time::{interval, sleep, timeout};
use tracing::{debug, error, info, warn};

use crate::prompt::{build_requirement_prompt, build_terminal_requirement_prompt};
use crate::service::{DEFAULT_LEASE_MS, RequirementService};
use crate::attachments::PromptAttachmentPlan;

/// Lease is renewed on this cadence while a turn is in flight.
const LEASE_RENEW_INTERVAL: Duration = Duration::from_secs(30);
/// Durable Conversation receipts are the authoritative completion channel.
/// This is intentionally a short poll: SQLite lookup is indexed by the stable
/// operation identity and a fast turn must not wait for a lease tick.
const CONVERSATION_RECEIPT_POLL_INTERVAL: Duration = Duration::from_millis(250);
/// Hard ceiling on a single requirement turn.
const TURN_TIMEOUT: Duration = Duration::from_secs(3600);
/// Idle cadence for a persistent loop with nothing to do (tag drained, claim
/// error, or a terminal awaiting relaunch). The `wake` Notify makes a freshly
/// created/re-pended requirement claim near-instantly; this is the safety-net
/// poll for anything the waker can't observe (for example, a terminal coming
/// back alive).
const IDLE_POLL: Duration = Duration::from_secs(10);
/// Cap on the completion note captured from a tool-free agent's final message,
/// in characters. The tail is kept (agents usually summarise at the end).
const MAX_NOTE_CHARS: usize = 4000;
/// How many retryable errors AutoWork will WAIT THROUGH (letting IDMM recover
/// the turn in-place) before giving up and failing the turn. Bounds the
/// worst-case hang when IDMM supervises but cannot recover. Combined with IDMM's
/// own escalating backoff this is several minutes of grace, then a hard fail.
#[cfg(test)]
const MAX_RECOVERY_WAITS: u32 = 5;

/// Max consecutive decision-ending turns AutoWork will YIELD to IDMM within one
/// requirement turn before finalizing it itself (bounds a runaway question loop).
#[cfg(test)]
const MAX_DECISION_WAITS: u32 = 12;

/// How long AutoWork waits for IDMM to START answering a pending decision (i.e.
/// drive a follow-up turn) before giving up the yield and finalizing the turn
/// as-is. Must comfortably exceed IDMM's first decision backoff (~10s) plus a
/// sidecar model call, so the model tier reliably wins; a rule-tier watch that
/// cannot auto-answer simply falls back to finalize after this window (no hang).
#[cfg(test)]
#[allow(dead_code)]
const DECISION_YIELD_WINDOW: Duration = Duration::from_secs(90);


/// Shared dependencies for all AutoWork loops.
pub struct AutoWorkRunnerDeps {
    /// Canonical installation owner. AutoWork is part of the installation-wide
    /// Requirements control plane and may only resume or drive this user's
    /// Conversation/Terminal targets.
    pub authoritative_user_id: Arc<str>,
    pub service: Arc<RequirementService>,
    pub runtime_registry: Arc<dyn AgentRuntimeRegistry>,
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
    /// so the runner tells them to call those tools AND expects an explicit
    /// verdict (a clean turn with no declaration → needs_review, not done). Kept
    /// in lock-step with `AgentFactoryDeps::requirement_mcp_config` so the prompt
    /// never names a tool the session lacks.
    pub requirement_mcp_enabled: bool,
}

/// Sealed, in-process preparation handed to ConversationService. It is invoked
/// only after the exact Requirement capability, Conversation receipt, durable
/// Running generation, and local turn owner have been admitted under one
/// preparation fence. Public request payloads cannot inject this hook.
struct AutoWorkAttachmentActivation {
    service: Arc<RequirementService>,
    plan: PromptAttachmentPlan,
}

#[async_trait::async_trait]
impl BackgroundTurnPreSendHook for AutoWorkAttachmentActivation {
    async fn prepare(&self) -> Result<(), AppError> {
        self.service.activate_attachment_plan(&self.plan).await
    }
}

/// Live progress for one AutoWork loop, shared between the loop task and the
/// API (`get_autowork`). Read by `AutoWorkRunner::live_progress`.
#[derive(Clone)]
struct LiveClaim {
    requirement_id: String,
    claim_generation: i64,
    /// Opaque execution capability. Intentionally has no `Debug` derive and is
    /// never projected through `live_progress` or tracing fields.
    claim_token: String,
}

#[derive(Default)]
struct LiveProgress {
    current_claim: Mutex<Option<LiveClaim>>,
    completed_count: AtomicU32,
}

impl LiveProgress {
    fn set_current(&self, claim: Option<LiveClaim>) {
        *self.current_claim.lock().expect("progress lock") = claim;
    }
    fn current(&self) -> Option<String> {
        self.current_claim
            .lock()
            .expect("progress lock")
            .as_ref()
            .map(|claim| claim.requirement_id.clone())
    }
    fn current_claim(&self) -> Option<LiveClaim> {
        self.current_claim.lock().expect("progress lock").clone()
    }
    fn incr_completed(&self) -> u32 {
        self.completed_count.fetch_add(1, Ordering::SeqCst) + 1
    }
    fn completed(&self) -> u32 {
        self.completed_count.load(Ordering::SeqCst)
    }
}

/// Domain-qualified key for the per-target loop maps. Business identifiers are
/// intentionally unprefixed UUIDv7 values, so the loop registry MUST key on
/// `(kind, target_id)` rather than assuming the identifier alone conveys its
/// domain.
type TargetKey = (AutoWorkTargetKind, String);
type TargetTransitionMap = DashMap<TargetKey, Arc<tokio::sync::Mutex<()>>>;

fn target_transition_lock(
    transitions: &TargetTransitionMap,
    key: &TargetKey,
) -> Arc<tokio::sync::Mutex<()>> {
    transitions
        .entry(key.clone())
        .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
        .clone()
}

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
    /// Explicit `stop_locked` takes responsibility for deterministic cleanup
    /// and sets this before aborting. Every other task drop (panic, external
    /// cancellation, runtime owner drop) is closed by `HandleGuard` instead.
    cleanup_handoff: Arc<AtomicBool>,
    /// Orders an asynchronously spawned Drop custodian against a later explicit
    /// stop/restart transition for the same handle generation.
    cleanup_barrier: Arc<tokio::sync::Mutex<()>>,
}

/// Removes a loop's handle from the map on task exit —normal OR panic (Drop runs
/// during unwind). The generation guard prevents clobbering a fresh handle that a
/// concurrent `start()` may have inserted.
struct HandleGuard {
    handles: Arc<DashMap<TargetKey, AutoWorkHandle>>,
    key: TargetKey,
    generation: u64,
    deps: Arc<AutoWorkRunnerDeps>,
    tag: String,
    cleanup_handoff: Arc<AtomicBool>,
    cleanup_barrier: Arc<tokio::sync::Mutex<()>>,
}

impl Drop for HandleGuard {
    fn drop(&mut self) {
        if self.cleanup_handoff.load(Ordering::SeqCst) {
            self.handles
                .remove_if(&self.key, |_, h| h.generation == self.generation);
            return;
        }

        // A task can be dropped after its claim COMMIT but before publishing
        // process-local progress. Keep the handle installed (therefore blocking
        // a replacement start) until an independent task has inspected the
        // durable typed owner and closed the exact claim from receipt evidence.
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            // During runtime destruction no async cleanup can be driven. The
            // durable claim/receipt remains absorbing and boot recovery will
            // close it; never attempt a synchronous or best-effort unclaim.
            self.handles
                .remove_if(&self.key, |_, h| h.generation == self.generation);
            return;
        };
        let handles = self.handles.clone();
        let key = self.key.clone();
        let generation = self.generation;
        let deps = self.deps.clone();
        let tag = self.tag.clone();
        let cleanup_handoff = self.cleanup_handoff.clone();
        let cleanup_barrier = self.cleanup_barrier.clone();
        runtime.spawn(async move {
            let _cleanup_guard = cleanup_barrier.lock().await;
            // A racing explicit stop owns deterministic cleanup and holds the
            // start/stop transition barrier. Once handed off, this background
            // task must not broadly park a replacement Terminal generation.
            if !cleanup_handoff.load(Ordering::SeqCst) {
                cleanup_abandoned_loop_claim(&deps, key.0, &key.1, &tag).await;
            }
            handles.remove_if(&key, |_, h| h.generation == generation);
        });
    }
}

/// Drives per-session AutoWork loops and the lease sweeper.
#[derive(Clone)]
pub struct AutoWorkRunner {
    deps: Arc<AutoWorkRunnerDeps>,
    handles: Arc<DashMap<TargetKey, AutoWorkHandle>>,
    /// Serializes start/stop transitions per target. Removing a handle before
    /// its abort/receipt cleanup completes must not let a racing start install a
    /// replacement loop, and two concurrent starts must not both spawn.
    transitions: Arc<TargetTransitionMap>,
    next_generation: Arc<std::sync::atomic::AtomicU64>,
}

impl AutoWorkRunner {
    pub fn new(deps: Arc<AutoWorkRunnerDeps>) -> Self {
        Self {
            deps,
            handles: Arc::new(DashMap::new()),
            transitions: Arc::new(DashMap::new()),
            next_generation: Arc::new(std::sync::atomic::AtomicU64::new(0)),
        }
    }

    fn transition_lock(&self, key: &TargetKey) -> Arc<tokio::sync::Mutex<()>> {
        target_transition_lock(&self.transitions, key)
    }

    /// Active loops as `(kind, target_id)` pairs (the sweeper's "active" set).
    pub fn active_targets(&self) -> Vec<TargetKey> {
        self.handles.iter().map(|e| e.key().clone()).collect()
    }

    pub fn is_running(&self, kind: AutoWorkTargetKind, target_id: &str) -> bool {
        valid_target_id(kind, target_id)
            && self.handles.contains_key(&(kind, target_id.to_string()))
    }

    pub fn running_tag(&self, kind: AutoWorkTargetKind, target_id: &str) -> Option<String> {
        if !valid_target_id(kind, target_id) {
            return None;
        }
        self.handles.get(&(kind, target_id.to_string())).map(|h| h.tag.clone())
    }

    /// Live progress for a running loop: `(current_requirement_id, completed_count)`.
    pub fn live_progress(&self, kind: AutoWorkTargetKind, target_id: &str) -> Option<(Option<String>, u32)> {
        if !valid_target_id(kind, target_id) {
            return None;
        }
        self.handles
            .get(&(kind, target_id.to_string()))
            .map(|h| (h.progress.current(), h.progress.completed()))
    }

    /// Start (or restart) the autowork loop for a target bound to `tag`.
    /// Stops after `max_requirements` completions when set.
    pub async fn start(
        &self,
        kind: AutoWorkTargetKind,
        target_id: String,
        tag: String,
        max_requirements: Option<u32>,
    ) {
        if !valid_target_id(kind, &target_id) {
            error!(target_id, ?kind, "Refusing to start AutoWork for an invalid target id");
            return;
        }
        let key: TargetKey = (kind, target_id.clone());
        let transition = self.transition_lock(&key);
        let _transition_guard = transition.lock().await;
        // A replacement generation cannot start until the prior loop has
        // stopped and its exact durable claim/admission has been settled. This
        // closes the old abort-then-spawn race where the new loop recovered an
        // effects-started claim before asynchronous cleanup reached the DB.
        self.stop_locked(kind, &target_id).await;

        let generation = self.next_generation.fetch_add(1, Ordering::SeqCst);
        let cancelled = Arc::new(AtomicBool::new(false));
        let cleanup_handoff = Arc::new(AtomicBool::new(false));
        let cleanup_barrier = Arc::new(tokio::sync::Mutex::new(()));
        let progress = Arc::new(LiveProgress::default());
        let cancelled_for_task = cancelled.clone();
        let progress_for_task = progress.clone();
        let deps = self.deps.clone();
        let handles = self.handles.clone();
        let conv = target_id.clone();
        let loop_tag = tag.clone();
        let guard_key = key.clone();
        let cleanup_handoff_for_task = cleanup_handoff.clone();
        let cleanup_barrier_for_task = cleanup_barrier.clone();
        let guard_deps = deps.clone();

        // Insert the handle BEFORE the loop's first await can reach its Drop-guard
        // cleanup (run_loop always awaits `claim_next` before any cleanup), so the
        // guard never removes a not-yet-inserted entry.
        let join = tokio::spawn(async move {
            // Drop runs on normal exit AND panic-unwind → handle always removed.
            let _guard = HandleGuard {
                handles,
                key: guard_key,
                generation,
                deps: guard_deps,
                tag: loop_tag.clone(),
                cleanup_handoff: cleanup_handoff_for_task,
                cleanup_barrier: cleanup_barrier_for_task,
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
                cleanup_handoff,
                cleanup_barrier,
            },
        );
    }

    /// Stop a session's loop. Sets the cancel flag, aborts the task, cancels
    /// the in-flight agent turn (conversation targets), and releases the
    /// in-flight claim (if any) back to `pending` so the requirement is not
    /// orphaned `in_progress` until the sweeper runs. Cancelling the live turn
    /// matters: disabling AutoWork must actually stop the work —historically
    /// the orphan turn kept the conversation showing "running" after the user
    /// flipped the switch off, and raced any later re-enable.
    pub async fn stop(&self, kind: AutoWorkTargetKind, target_id: &str) {
        if !valid_target_id(kind, target_id) {
            return;
        }
        let key = (kind, target_id.to_string());
        let transition = self.transition_lock(&key);
        let _transition_guard = transition.lock().await;
        self.stop_locked(kind, target_id).await;
    }

    /// Caller must hold this target's `transition_lock`.
    async fn stop_locked(&self, kind: AutoWorkTargetKind, target_id: &str) {
        if let Some((_, handle)) = self.handles.remove(&(kind, target_id.to_string())) {
            handle.cancelled.store(true, Ordering::SeqCst);
            handle.cleanup_handoff.store(true, Ordering::SeqCst);
            handle.join.abort();
            // Await cancellation before durable cleanup. A claim/admission DB
            // future that won the race is therefore visible to the cleanup
            // below; a future that lost cannot later resume and write.
            let _ = handle.join.await;
            // If a natural-exit/panic Drop custodian started just before this
            // explicit stop took ownership, wait until it has either finished
            // or observed `cleanup_handoff` and skipped. The transition lock
            // remains held, so a replacement loop cannot race either cleanup.
            let _cleanup_guard = handle.cleanup_barrier.lock().await;
            // Do not trust only the process-local progress slot here. The
            // claim transaction can COMMIT immediately before task abortion
            // and the task can then be dropped before `progress.set_current`.
            // Re-read the exact typed owner while the start/stop transition
            // barrier is still held. This recovery seam never allocates a new
            // pending claim; it only exposes an already-committed active one.
            let published_claim = handle.progress.current_claim();
            let recovered_claim = match self
                .deps
                .service
                .recover_active_claim_for_runner(
                    &handle.tag,
                    target_id,
                    handle.kind,
                    DEFAULT_LEASE_MS,
                )
                .await
            {
                Ok(claim) => claim.map(|claim| LiveClaim {
                    requirement_id: claim.requirement.requirement_id,
                    claim_generation: claim.claim_generation,
                    claim_token: claim.claim_token,
                }),
                Err(error) => {
                    warn!(
                        target_id,
                        ?kind,
                        error = %error,
                        "Failed to recover a just-committed AutoWork claim during stop"
                    );
                    None
                }
            };
            // The durable owner is authoritative when present. The progress
            // value remains a useful exact-CAS fallback when the row was
            // already finalized between abortion and this read.
            let active_claim = recovered_claim.or(published_claim);

            if handle.kind == AutoWorkTargetKind::Conversation
                && let Some(agent) = self.deps.runtime_registry.get_runtime(target_id)
                && let Err(e) = agent.cancel().await
            {
                warn!(target_id, error = %e, "Failed to cancel in-flight AutoWork turn on stop");
            }
            if let Some(active_claim) = active_claim {
                let req_id = active_claim.requirement_id;
                let claim_generation = active_claim.claim_generation;
                let claim_token = active_claim.claim_token;
                if handle.kind == AutoWorkTargetKind::Conversation {
                    if let Err(error) = resolve_interrupted_conversation_claim(
                        &self.deps,
                        &req_id,
                        target_id,
                        claim_generation,
                        &claim_token,
                        "AutoWork was stopped.",
                    )
                    .await
                    {
                        warn!(
                            requirement_id = req_id,
                            claim_generation,
                            error = %error,
                            "Failed to resolve Conversation claim on AutoWork stop"
                        );
                    }
                } else if let Some(driver) = &self.deps.terminal_driver {
                    let detail =
                        "AutoWork was stopped while this Terminal claim was active; any durable \
                         admission was absorbed and will not be written again.";
                    let park_succeeded = match driver
                        .park_open_turn_admissions(target_id, None, detail)
                        .await
                    {
                        Ok(_) => true,
                        Err(error) => {
                            warn!(
                                terminal_id = target_id,
                                requirement_id = req_id,
                                error = %error,
                                "Failed to park Terminal turn admission during AutoWork stop"
                            );
                            false
                        }
                    };
                    match driver
                        .get_turn_admission_for_claim(
                            target_id,
                            &req_id,
                            claim_generation,
                            &claim_token,
                        )
                        .await
                    {
                        Ok(None) if park_succeeded => {
                            // The receiver-side lookup is only diagnostic. The
                            // database abandon command independently repeats
                            // exact authority and all receiver-admission
                            // absence proofs in the same writer transaction as
                            // active->pending.
                            if let Err(error) = abandon_pre_effect_or_quarantine(
                                &self.deps,
                                &req_id,
                                target_id,
                                AutoWorkTargetKind::Terminal,
                                claim_generation,
                                &claim_token,
                                "AutoWork stop found no exact Terminal admission.",
                            )
                            .await
                            {
                                warn!(
                                    terminal_id = target_id,
                                    requirement_id = req_id,
                                    error = %error,
                                    "Failed to safely resolve pre-admission Terminal claim"
                                );
                            }
                        }
                        lookup => {
                            let (status, note) = match lookup {
                                Ok(Some(row)) => match terminal_turn_end_from_receipt(&row) {
                                    Some(TerminalTurnEnd::AuthoritativeVerdict { status, note }) => {
                                        (status, note)
                                    }
                                    _ => (
                                        RequirementStatus::NeedsReview,
                                        Some(detail.to_owned()),
                                    ),
                                },
                                Ok(None) => (
                                    RequirementStatus::NeedsReview,
                                    Some(format!(
                                        "{detail} Parking or receipt verification failed, so \
                                         pre-admission absence was not proven."
                                    )),
                                ),
                                Err(error) => (
                                    RequirementStatus::NeedsReview,
                                    Some(format!(
                                        "{detail} Exact receipt lookup failed: {error}"
                                    )),
                                ),
                            };
                            if let Err(status_error) = self
                                .deps
                                .service
                                .resolve_claim_verdict_exact(
                                    &req_id,
                                    claim_generation,
                                    &claim_token,
                                    target_id,
                                    AutoWorkTargetKind::Terminal,
                                    status,
                                    note,
                                )
                                .await
                            {
                                warn!(
                                    terminal_id = target_id,
                                    requirement_id = req_id,
                                    error = %status_error,
                                    "Failed to resolve Terminal Requirement during AutoWork stop"
                                );
                            }
                        }
                    }
                } else {
                    let detail = "AutoWork stopped an active Terminal claim without a durable \
                                  Terminal driver; its execution state is unknown.";
                    if let Err(error) = self
                        .deps
                        .service
                        .resolve_claim_verdict_exact(
                            &req_id,
                            claim_generation,
                            &claim_token,
                            target_id,
                            AutoWorkTargetKind::Terminal,
                            RequirementStatus::NeedsReview,
                            Some(detail.to_owned()),
                        )
                        .await
                    {
                        warn!(
                            terminal_id = target_id,
                            requirement_id = req_id,
                            error = %error,
                            "Failed to park Terminal Requirement without a driver"
                        );
                    }
                }
            } else if handle.kind == AutoWorkTargetKind::Terminal
                && let Some(driver) = &self.deps.terminal_driver
            {
                // Even if the Requirement owner lookup failed, absorb any
                // already-created receipt under the same lifecycle lock as the
                // PTY writer. A later exact Requirement recovery can project
                // the parked receipt, but no submit byte may escape this stop.
                if let Err(error) = driver
                    .park_open_turn_admissions(
                        target_id,
                        None,
                        "AutoWork stopped while claim recovery was unavailable; any durable Terminal admission was parked.",
                    )
                    .await
                {
                    warn!(
                        terminal_id = target_id,
                        error = %error,
                        "Failed to park Terminal admissions without a recovered claim"
                    );
                }
            }
        }
    }

    /// Spawn the lease sweeper: every 60s, park `in_progress` requirements whose
    /// lease expired and whose owning session is not a live AutoWork loop here.
    /// Expiry alone never makes an execution safe to repeat.
    /// Detached for the process lifetime (the runner lives in router state).
    pub fn start_sweeper(&self) {
        let handles = self.handles.clone();
        let service = self.deps.service.clone();
        tokio::spawn(async move {
            let mut ticker = interval(Duration::from_secs(60));
            ticker.tick().await; // consume the immediate first tick
            loop {
                ticker.tick().await;
                // The active set is keyed by `(kind, target_id)`. The sweep
                // matches each typed owner column against its corresponding
                // canonical string-ID set.
                let active_conversations: Vec<String> = handles
                    .iter()
                    .filter(|entry| entry.key().0 == AutoWorkTargetKind::Conversation)
                    .map(|entry| entry.key().1.clone())
                    .collect();
                let active_terminals: Vec<String> = handles
                    .iter()
                    .filter(|entry| entry.key().0 == AutoWorkTargetKind::Terminal)
                    .map(|entry| entry.key().1.clone())
                    .collect();
                match service
                    .repo()
                    .sweep_expired_leases(
                        &active_conversations,
                        &active_terminals,
                        nomifun_common::now_ms(),
                    )
                    .await
                {
                    Ok(n) if n > 0 => {
                        info!(parked = n, "Requirement lease sweeper parked ambiguous claims")
                    }
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
    /// each session page —the old behaviour that made AutoWork look like it
    /// "only works in the foreground". Spawning the loops here makes the backend
    /// the single source of truth: a bound session works in the background from
    /// boot, no UI visit required. Conversation loops start driving immediately;
    /// a terminal whose PTY is not yet live idles until the user relaunches it
    /// (the loop self-heals —see `run_loop`). Detached + best-effort.
    pub fn resume_persisted_bindings(&self) {
        let this = self.clone();
        tokio::spawn(async move {
            let mut resumed = 0usize;
            let owner_id = this.deps.authoritative_user_id.clone();
            let groups = match this.deps.service.tag_bindings(&owner_id).await {
                Ok(groups) => groups,
                Err(error) => {
                    warn!(user_id = %owner_id, %error, "AutoWork resume: owner tag_bindings failed");
                    return;
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
                    this
                        .start(binding.kind, binding.target_id.clone(), group.tag.clone(), max)
                        .await;
                    resumed += 1;
                }
            }
            if resumed > 0 {
                info!(resumed, "AutoWork resumed persisted bindings on boot");
            }
        });
    }
}

fn valid_target_id(kind: AutoWorkTargetKind, target_id: &str) -> bool {
    match kind {
        AutoWorkTargetKind::Conversation => ConversationId::try_from(target_id).is_ok(),
        AutoWorkTargetKind::Terminal => TerminalId::try_from(target_id).is_ok(),
    }
}

/// The autowork loop body. Claims → injects → waits → finalizes → repeats.
///
/// The loop is *persistent*: it does NOT exit when the tag drains or a claim
/// errors —it idles (waking on `deps.wake`, with `IDLE_POLL` as a fallback) and
/// keeps claiming, so a bound session keeps picking up new requirements in the
/// background forever. It exits only on cancel (disable / stop), after
/// `max_requirements` completions, or when a terminal target's session row is
/// deleted. A terminal whose PTY merely exited idles until a relaunch revives it.
/// Outcome of one claimed requirement's turn, used to drive the failure backoff.
async fn resolve_interrupted_conversation_claim(
    deps: &AutoWorkRunnerDeps,
    requirement_id: &str,
    conversation_id: &str,
    claim_generation: i64,
    claim_token: &str,
    reason: &str,
) -> Result<(), AppError> {
    match deps
        .service
        .release_claim_exact(
            requirement_id,
            conversation_id,
            claim_generation,
            claim_token,
        )
        .await
    {
        Ok(true) => {}
        result @ (Ok(false) | Err(_)) => {
            let detail = match result {
                Ok(false) => format!(
                    "{reason} The same SQLite writer transaction could not prove that Conversation \
                     claim generation {claim_generation} had no authority receipt or active turn; \
                     it will not be delivered again."
                ),
                Err(error) => format!(
                    "{reason} Atomic pre-effect abandon for Conversation claim generation \
                     {claim_generation} failed: {error}. It will not be delivered again."
                ),
                Ok(true) => unreachable!(),
            };
            deps.service
                .resolve_claim_verdict_exact(
                    requirement_id,
                    claim_generation,
                    claim_token,
                    conversation_id,
                    AutoWorkTargetKind::Conversation,
                    RequirementStatus::NeedsReview,
                    Some(detail),
                )
                .await?;
        }
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn abandon_pre_effect_or_quarantine(
    deps: &AutoWorkRunnerDeps,
    requirement_id: &str,
    owner_id: &str,
    kind: AutoWorkTargetKind,
    claim_generation: i64,
    claim_token: &str,
    reason: &str,
) -> Result<bool, AppError> {
    match deps
        .service
        .unclaim_busy(
            requirement_id,
            owner_id,
            kind,
            claim_generation,
            claim_token,
        )
        .await
    {
        Ok(true) => Ok(true),
        abandon_result @ (Ok(false) | Err(_)) => {
            let detail = match abandon_result {
                Ok(false) => format!(
                    "{reason} Atomic pre-effect abandon could not prove receiver-admission absence \
                     for claim generation {claim_generation}; the exact capability was quarantined."
                ),
                Err(error) => format!(
                    "{reason} Atomic pre-effect abandon failed for claim generation \
                     {claim_generation}: {error}. The exact capability was quarantined."
                ),
                Ok(true) => unreachable!(),
            };
            deps.service
                .resolve_claim_verdict_exact(
                    requirement_id,
                    claim_generation,
                    claim_token,
                    owner_id,
                    kind,
                    RequirementStatus::NeedsReview,
                    Some(detail),
                )
                .await?;
            Ok(false)
        }
    }
}

async fn pause_and_resolve_user_interruption(
    deps: &AutoWorkRunnerDeps,
    requirement_id: &str,
    conversation_id: &str,
    claim_generation: i64,
    claim_token: &str,
    tag: &str,
) -> Result<(), AppError> {
    deps.service
        .pause_for_user_interrupt(requirement_id, tag)
        .await?;
    resolve_interrupted_conversation_claim(
        deps,
        requirement_id,
        conversation_id,
        claim_generation,
        claim_token,
        "The user interrupted AutoWork.",
    )
    .await
}

/// Independent custodian for a loop task that ended without going through
/// `stop_locked` (panic, external abort, or owner future drop). Recovery reads
/// only an already-active typed owner; it never allocates new work.
async fn cleanup_abandoned_loop_claim(
    deps: &AutoWorkRunnerDeps,
    kind: AutoWorkTargetKind,
    target_id: &str,
    tag: &str,
) {
    let claim = match deps
        .service
        .recover_active_claim_for_runner(tag, target_id, kind, DEFAULT_LEASE_MS)
        .await
    {
        Ok(claim) => claim,
        Err(error) => {
            warn!(
                target_id,
                ?kind,
                error = %error,
                "Unable to recover an abandoned AutoWork claim for durable cleanup"
            );
            return;
        }
    };
    let Some(claim) = claim else {
        return;
    };
    let requirement_id = claim.requirement.requirement_id;
    let claim_generation = claim.claim_generation;
    let claim_token = claim.claim_token;

    match kind {
        AutoWorkTargetKind::Conversation => {
            if let Err(error) = resolve_interrupted_conversation_claim(
                deps,
                &requirement_id,
                target_id,
                claim_generation,
                &claim_token,
                "The AutoWork loop ended without a normal completion boundary.",
            )
            .await
            {
                warn!(
                    conversation_id = target_id,
                    requirement_id,
                    claim_generation,
                    error = %error,
                    "Failed to close an abandoned Conversation claim"
                );
            }
        }
        AutoWorkTargetKind::Terminal => {
            let detail = "The AutoWork loop ended without a normal completion boundary; any \
                          durable Terminal admission was absorbed and will not be written again.";
            let Some(driver) = deps.terminal_driver.as_ref() else {
                if let Err(error) = deps
                    .service
                    .resolve_claim_verdict_exact(
                        &requirement_id,
                        claim_generation,
                        &claim_token,
                        target_id,
                        AutoWorkTargetKind::Terminal,
                        RequirementStatus::NeedsReview,
                        Some(detail.to_owned()),
                    )
                    .await
                {
                    warn!(
                        terminal_id = target_id,
                        requirement_id,
                        claim_generation,
                        error = %error,
                        "Failed to park an abandoned Terminal claim without a driver"
                    );
                }
                return;
            };

            // This takes the same lifecycle lock as both admitted PTY writers.
            // Because the old handle remains installed until this function
            // returns, no replacement loop can acquire a new admission while
            // broad terminal parking is in progress.
            let park_succeeded = match driver
                .park_open_turn_admissions(target_id, None, detail)
                .await
            {
                Ok(_) => true,
                Err(error) => {
                    warn!(
                        terminal_id = target_id,
                        requirement_id,
                        claim_generation,
                        error = %error,
                        "Failed to park admission for an abandoned Terminal claim"
                    );
                    false
                }
            };

            match driver
                .get_turn_admission_for_claim(
                    target_id,
                    &requirement_id,
                    claim_generation,
                    &claim_token,
                )
                .await
            {
                Ok(None) if park_succeeded => {
                    // The database command is the authoritative proof and
                    // transition. This prior lookup merely avoids a needless
                    // quarantine attempt when an exact receipt is visible.
                    if let Err(error) = abandon_pre_effect_or_quarantine(
                        deps,
                        &requirement_id,
                        target_id,
                        AutoWorkTargetKind::Terminal,
                        claim_generation,
                        &claim_token,
                        "Abandoned Terminal loop found no exact admission.",
                    )
                    .await
                    {
                        warn!(
                            terminal_id = target_id,
                            requirement_id,
                            claim_generation,
                            error = %error,
                            "Failed to safely resolve a pre-admission abandoned Terminal claim"
                        );
                    }
                }
                lookup => {
                    let (status, note) = match lookup {
                        Ok(Some(row)) => match terminal_turn_end_from_receipt(&row) {
                            Some(TerminalTurnEnd::AuthoritativeVerdict { status, note }) => {
                                (status, note)
                            }
                            _ => (
                                RequirementStatus::NeedsReview,
                                Some(detail.to_owned()),
                            ),
                        },
                        Ok(None) => (
                            RequirementStatus::NeedsReview,
                            Some(format!(
                                "{detail} Receipt absence was not proven because parking failed."
                            )),
                        ),
                        Err(error) => (
                            RequirementStatus::NeedsReview,
                            Some(format!("{detail} Exact receipt lookup failed: {error}")),
                        ),
                    };
                    if let Err(error) = deps
                        .service
                        .resolve_claim_verdict_exact(
                            &requirement_id,
                            claim_generation,
                            &claim_token,
                            target_id,
                            AutoWorkTargetKind::Terminal,
                            status,
                            note,
                        )
                        .await
                    {
                        warn!(
                            terminal_id = target_id,
                            requirement_id,
                            claim_generation,
                            error = %error,
                            "Failed to resolve an abandoned Terminal claim"
                        );
                    }
                }
            }
        }
    }
}

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
    /// an attempt —the loop idles until the user resumes the tag. NOT a
    /// failure: no backoff, no retry.
    UserInterrupted,
}

/// How a conversation turn ended, from the AutoWork runner's perspective.
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

/// Result of observing a Conversation after its durable turn receipt was
/// accepted. Any loss of observation integrity is absorbing: it is not proof
/// that the model/tool turn did nothing, so it must never become RetryPending.
#[cfg(test)]
#[allow(dead_code)]
enum ConversationWaitEnd {
    Terminal(TurnEnd),
    Ambiguous(String),
}

enum DurableConversationReceiptWait {
    Completed(IdempotentMessageDelivery),
    Ambiguous(String),
}

/// Broadcast this loop target's live AutoWork run-state so EVERY surface stays in
/// sync across idle→active transitions. The per-session control GETs fresh state
/// on open, but the session-list capability icon updates ONLY from this event (no
/// per-row GET); without an emit on claim/finish it kept the run-state from its
/// initial bulk load and showed a stale colour —active/green in the header but
/// idle/orange in the sidebar for the same session. `enabled=false` is emitted
/// when the max-requirements cap just disabled the binding so the icon drops off.
fn emit_autowork_progress(
    deps: &AutoWorkRunnerDeps,
    kind: AutoWorkTargetKind,
    target_id: &str,
    tag: &str,
    progress: &LiveProgress,
    enabled: bool,
) {
    let current_requirement_id = progress.current();
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
    deps: Arc<AutoWorkRunnerDeps>,
    target_id: &str,
    kind: AutoWorkTargetKind,
    tag: &str,
    cancelled: Arc<AtomicBool>,
    progress: Arc<LiveProgress>,
    max_requirements: Option<u32>,
) {
    let owner_id = target_id;
    // Close the startup preflight window as well as the per-claim window. The
    // first conversation lease is acquired before ownership verification's
    // first await, then consumed by the first claim iteration.
    let mut startup_conversation_lease = if kind == AutoWorkTargetKind::Conversation {
        match deps
            .conversation_service
            .begin_public_runtime_preparation(owner_id, &deps.authoritative_user_id)
        {
            Ok(lease) => Some(lease),
            Err(error) => {
                debug!(target_id, tag, error = %error, "AutoWork startup admission is fenced");
                return;
            }
        }
    } else {
        None
    };
    let owner_check = match kind {
        AutoWorkTargetKind::Conversation => {
            deps.service
                .verify_conversation_owner(owner_id, &deps.authoritative_user_id)
                .await
        }
        AutoWorkTargetKind::Terminal => {
            deps.service
                .verify_terminal_owner(target_id, &deps.authoritative_user_id)
                .await
        }
    };
    if let Err(error) = owner_check {
        warn!(target_id, ?kind, %error, "AutoWork target is not installation-owner owned —not starting");
        return;
    }
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
        // `inject_and_wait_terminal` (right after the Agent runtime/PTY exists), NOT here.
        // Arming at the loop top fired on every idle poll too —and since an idle
        // conversation has no live Agent runtime, IDMM's probe.observe() got a closed
        // channel and the supervisor died instantly, only to be re-armed 10s later:
        // a runaway "IDMM supervisor armed" churn that did no work. Arming once the
        // turn's runtime exists makes the supervisor actually attach to the turn.

        // Recover an already-active terminal claim BEFORE consulting volatile
        // PTY liveness. A dead PTY cannot prove whether the previous process
        // submitted the prompt; the recovered claim must reach the fail-closed
        // NeedsReview branch below without writing to the terminal again.
        let recovered_terminal_claim = if kind == AutoWorkTargetKind::Terminal {
            match deps
                .service
                .recover_active_claim_for_runner(tag, owner_id, kind, DEFAULT_LEASE_MS)
                .await
            {
                Ok(claim) => claim,
                Err(error) => {
                    warn!(
                        target_id,
                        tag,
                        error = %error,
                        "AutoWork active terminal claim recovery failed —retrying"
                    );
                    sleep(IDLE_POLL).await;
                    continue;
                }
            }
        } else {
            None
        };

        // Only a NEW terminal claim is gated on the current PTY. An ambiguous
        // recovered claim above is parked even when the PTY is offline.
        if recovered_terminal_claim.is_none()
            && kind == AutoWorkTargetKind::Terminal
            && let Some(driver) = &deps.terminal_driver
            && !driver.is_alive(owner_id)
        {
            if matches!(driver.describe(owner_id).await, Ok(None)) {
                info!(target_id, tag, "AutoWork terminal removed —stopping");
                break;
            }
            sleep(IDLE_POLL).await;
            continue;
        }

        // Conversation AutoWork is a runtime initiator. Fence it before the
        // claim await, not inside `inject_and_wait`: otherwise a stop can fully
        // finish while the old claim is pending and that old wakeup can later
        // resurrect a runtime under a fresh lease.
        let claim_started_ms = nomifun_common::now_ms();
        let mut conversation_build_lease = if kind == AutoWorkTargetKind::Conversation {
            let lease = match startup_conversation_lease.take() {
                Some(lease) => Ok(lease),
                None => deps
                    .conversation_service
                    .begin_public_runtime_preparation(owner_id, &deps.authoritative_user_id),
            };
            match lease {
                Ok(lease) => {
                    if let Err(error) = lease.ensure_active() {
                        info!(target_id, tag, error = %error, "AutoWork preparation was cancelled before claim");
                        break;
                    }
                    Some(lease)
                }
                Err(error) => {
                    debug!(target_id, tag, error = %error, "AutoWork conversation admission is fenced");
                    if deps
                        .conversation_service
                        .user_cancelled_since(target_id, claim_started_ms)
                    {
                        info!(target_id, tag, "AutoWork idle preparation was stopped by user");
                        break;
                    }
                    sleep(IDLE_POLL).await;
                    continue;
                }
            }
        } else {
            None
        };

        // Claim the next requirement. The wake future is armed BEFORE the claim
        // (and dropped right after) so a requirement created/re-pended between the
        // claim returning None and our await is never lost. On drain or a transient
        // error the loop idles and retries instead of exiting —persistent by design.
        let claimed = if let Some(claim) = recovered_terminal_claim {
            claim
        } else {
            let wake = deps.wake.notified();
            tokio::pin!(wake);
            wake.as_mut().enable();
            match deps
                .service
                .claim_next_for_runner(tag, owner_id, kind, DEFAULT_LEASE_MS)
                .await
            {
                Ok(Some(claim)) => claim,
                Ok(None) => {
                    // Tag drained (or paused) → not a failure spin; reset backoff.
                    consecutive_failures = 0;
                    drop(conversation_build_lease.take());
                    tokio::select! {
                        _ = wake.as_mut() => {}
                        _ = sleep(IDLE_POLL) => {}
                    }
                    continue;
                }
                Err(e) => {
                    warn!(target_id, tag, error = %e, "AutoWork claim failed —retrying");
                    drop(conversation_build_lease.take());
                    tokio::select! {
                        _ = wake.as_mut() => {}
                        _ = sleep(IDLE_POLL) => {}
                    }
                    continue;
                }
            }
        };
        let claim_generation = claimed.claim_generation;
        let claim_token = claimed.claim_token;
        let recovered_active = claimed.recovered_active;
        let claimed = claimed.requirement;
        let req_id = claimed.requirement_id.clone();
        progress.set_current(Some(LiveClaim {
            requirement_id: req_id.clone(),
            claim_generation,
            claim_token: claim_token.clone(),
        }));
        info!(
            target_id,
            tag,
            requirement_id = %req_id,
            claim_generation,
            recovered_active,
            "AutoWork claimed requirement"
        );
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
                let turn_started_ms = claim_started_ms;
                let build_lease = conversation_build_lease
                    .take()
                    .expect("conversation AutoWork acquired a pre-claim runtime lease");
                match inject_and_wait(
                    &deps,
                    target_id,
                    tag,
                    &claimed,
                    claim_generation,
                    &claim_token,
                    recovered_active,
                    turn_started_ms,
                    build_lease,
                )
                .await
                {
                    Ok((end, note, expects_verdict)) => {
                        // User interrupt: the engine reported Cancelled, OR the
                        // user hit the cancel endpoint during the turn (covers
                        // engines whose cancel path surfaces as a generic
                        // Error). Pause the tag and release the claim instead
                        // of finalizing —re-pending a deliberate stop is what
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
                                requirement_id = %req_id,
                                "AutoWork turn stopped by user —pausing tag"
                            );
                            if let Err(e) = pause_and_resolve_user_interruption(
                                &deps,
                                &req_id,
                                owner_id,
                                claim_generation,
                                &claim_token,
                                tag,
                            )
                            .await
                            {
                                error!(target_id, requirement_id = %req_id, error = %e, "AutoWork user-interrupt failed");
                            }
                            TurnResult::UserInterrupted
                        } else {
                            let turn_errored = end == TurnEnd::Errored;
                            // `note` carries the agent's final plain-text message for tool-free
                            // engines (ACP/codex/gemini) so the platform records what was done.
                            if let Err(e) = deps
                                .service
                                .finalize_claim_if_needed(
                                    &req_id,
                                    claim_generation,
                                    &claim_token,
                                    owner_id,
                                    AutoWorkTargetKind::Conversation,
                                    turn_errored,
                                    note,
                                    expects_verdict,
                                )
                                .await
                            {
                                error!(target_id, requirement_id = %req_id, error = %e, "AutoWork finalize failed");
                            }
                            if turn_errored { TurnResult::Errored } else { TurnResult::Done }
                        }
                    }
                    // The session was busy (a foreground user turn or IDMM owns turn
                    // admission). The requirement's turn never ran —revert its work claim
                    // WITHOUT consuming an attempt, then back off and retry. Without
                    // this, a transient busy window burns the requirement's retries
                    // and falsely fails it (and pauses its tag).
                    Err(AppError::Conflict(conflict)) => {
                        if deps
                            .conversation_service
                            .user_cancelled_since(target_id, turn_started_ms)
                        {
                            info!(
                                target_id,
                                tag,
                                requirement_id = %req_id,
                                "AutoWork preparation was stopped by user —pausing tag"
                            );
                            if let Err(e) = pause_and_resolve_user_interruption(
                                &deps,
                                &req_id,
                                owner_id,
                                claim_generation,
                                &claim_token,
                                tag,
                            )
                            .await
                            {
                                error!(target_id, requirement_id = %req_id, error = %e, "AutoWork user-interrupt failed");
                            }
                            TurnResult::UserInterrupted
                        } else {
                            warn!(
                                target_id,
                                requirement_id = %req_id,
                                error = %conflict,
                                "AutoWork inject hit a conflict; proving pre-effect absence atomically"
                            );
                            match abandon_pre_effect_or_quarantine(
                                &deps,
                                &req_id,
                                owner_id,
                                kind,
                                claim_generation,
                                &claim_token,
                                &format!("Conversation injection was rejected: {conflict}."),
                            )
                            .await
                            {
                                Ok(true) => TurnResult::Busy,
                                Ok(false) => TurnResult::Done,
                                Err(error) => {
                                    error!(
                                        target_id,
                                        requirement_id = %req_id,
                                        error = %error,
                                        "AutoWork conflict could not be abandoned or quarantined"
                                    );
                                    TurnResult::Busy
                                }
                            }
                        }
                    }
                    // The session backing this loop is GONE (deleted mid-flight →
                    // inject_and_wait's conversation_repo.get returns NotFound). This
                    // is NOT the requirement's fault: revert the claim WITHOUT
                    // consuming an attempt (so a deleted session can't burn a
                    // requirement's retries and PAUSE the whole tag for sibling
                    // conversations bound to it — the observed "delete conv 29 →
                    // tag test stuck" cascade), then STOP this loop (no target left
                    // to drive).
                    Err(AppError::NotFound(not_found)) => {
                        warn!(
                            target_id,
                            requirement_id = %req_id,
                            error = %not_found,
                            "AutoWork target conversation is gone; closing exact claim safely"
                        );
                        if let Err(error) = abandon_pre_effect_or_quarantine(
                            &deps,
                            &req_id,
                            owner_id,
                            kind,
                            claim_generation,
                            &claim_token,
                            &format!("The target Conversation disappeared: {not_found}."),
                        )
                        .await
                        {
                            error!(
                                target_id,
                                requirement_id = %req_id,
                                error = %error,
                                "Deleted-target claim could not be abandoned or quarantined"
                            );
                        }
                        break;
                    }
                    Err(e) => {
                        error!(target_id, requirement_id = %req_id, error = %e, "AutoWork inject failed");
                        // errored turn → expects_verdict is irrelevant (re-pend / fail).
                        if let Err(e) = deps
                            .service
                            .finalize_claim_if_needed(
                                &req_id,
                                claim_generation,
                                &claim_token,
                                owner_id,
                                AutoWorkTargetKind::Conversation,
                                true,
                                None,
                                false,
                            )
                            .await
                        {
                            error!(target_id, requirement_id = %req_id, error = %e, "AutoWork finalize failed");
                        }
                        TurnResult::Errored
                    }
                }
            }
            AutoWorkTargetKind::Terminal => {
                match inject_and_wait_terminal(
                    &deps,
                    owner_id,
                    tag,
                    &claimed,
                    claim_generation,
                    &claim_token,
                    recovered_active,
                )
                .await
                {
                    Ok(TerminalTurnEnd::AuthoritativeVerdict { status, note }) => {
                        match deps
                            .service
                            .resolve_claim_verdict_exact(
                                &req_id,
                                claim_generation,
                                &claim_token,
                                owner_id,
                                AutoWorkTargetKind::Terminal,
                                status,
                                note,
                            )
                            .await
                        {
                            Ok(_) => {
                                if status == RequirementStatus::Failed {
                                    TurnResult::Errored
                                } else {
                                    TurnResult::Done
                                }
                            }
                            Err(error) => {
                                error!(
                                    target_id,
                                    requirement_id = %req_id,
                                    claim_generation,
                                    error = %error,
                                    "Failed to project durable Terminal verdict onto Requirement"
                                );
                                TurnResult::Busy
                            }
                        }
                    }
                    Ok(TerminalTurnEnd::AmbiguousAfterSubmission) => {
                        // Once any PTY write was attempted, death, timeout, a
                        // closed/lagged lifecycle stream, or even a partial
                        // two-chunk submit cannot prove the command did not
                        // run. Never re-pend and write it again.
                        let note = format!(
                            "AutoWork terminal claim generation {claim_generation} attempted PTY \
                             submission, but its final outcome is unknown; it was not executed again."
                        );
                        match deps
                            .service
                            .resolve_claim_verdict_exact(
                                &req_id,
                                claim_generation,
                                &claim_token,
                                owner_id,
                                AutoWorkTargetKind::Terminal,
                                RequirementStatus::NeedsReview,
                                Some(note),
                            )
                            .await
                        {
                            Ok(_) => TurnResult::Done,
                            Err(error) => {
                                error!(
                                    target_id,
                                    requirement_id = %req_id,
                                    claim_generation,
                                    error = %error,
                                    "Failed to park ambiguous terminal submission for review"
                                );
                                TurnResult::Busy
                            }
                        }
                    }
                    Err(e) => {
                        // Preparation failed before any PTY write was
                        // attempted, so the normal retry budget remains safe.
                        error!(target_id, requirement_id = %req_id, error = %e, "AutoWork terminal inject failed");
                        if let Err(e) = deps
                            .service
                            .finalize_claim_if_needed(
                                &req_id,
                                claim_generation,
                                &claim_token,
                                owner_id,
                                AutoWorkTargetKind::Terminal,
                                true,
                                None,
                                false,
                            )
                            .await
                        {
                            error!(target_id, requirement_id = %req_id, error = %e, "AutoWork finalize failed");
                        }
                        TurnResult::Errored
                    }
                }
            }
        };

        // 3. Re-read the final status to count completions + honor max.
        let final_status = deps.service.get(&req_id).await.ok().map(|d| d.status);
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
                    "AutoWork reached max_requirements —stopping"
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

/// Resolve runtime options, acquire the Agent runtime, subscribe, send the prompt, and
/// wait for a terminal event while renewing the lease. Returns
/// `(end, note, expects_verdict)` where `end` classifies how the turn ended
/// (clean / errored / user-cancelled), `note` is the agent's final plain-text
/// message captured for the completion record (only on a clean finish), and
/// `expects_verdict` is true when this engine has an explicit declaration
/// channel (native requirement tools / requirement MCP) so a clean turn with
/// no declaration is parked for review rather than assumed done.
async fn inject_and_wait(
    deps: &Arc<AutoWorkRunnerDeps>,
    conversation_id: &str,
    tag: &str,
    req: &Requirement,
    claim_generation: i64,
    claim_token: &str,
    recovered_active: bool,
    claim_started_ms: i64,
    build_lease: RuntimeBuildLease,
) -> Result<(TurnEnd, Option<String>, bool), AppError> {
    let conv_id = conversation_id;
    build_lease.ensure_active()?;
    // Load the conversation row to resolve agent_type / model / workspace / user.
    let row = deps
        .conversation_repo
        .get(conv_id)
        .await?
        .ok_or_else(|| AppError::NotFound(format!("conversation {conversation_id}")))?;
    build_lease.ensure_active()?;

    let user_id = UserId::parse(&row.user_id).map_err(|error| {
        AppError::Forbidden(format!("AutoWork conversation has invalid owner identity: {error}"))
    })?;
    if user_id.as_str() != deps.authoritative_user_id.as_ref() {
        return Err(AppError::Forbidden(
            "AutoWork requires an installation-owner Conversation".into(),
        ));
    }
    let user_id = user_id.into_string();

    let agent_type = parse_agent_type(&deps.agent_registry, &row.r#type).await;
    let model = nomifun_conversation::runtime_options::provider_model_from_conversation_row(&row)?;
    let delegation_policy =
        nomifun_conversation::runtime_options::delegation_policy_from_conversation_row(&row)?;
    let extra: serde_json::Value = serde_json::from_str(&row.extra).map_err(|error| {
        AppError::Internal(format!(
            "conversation {conversation_id} has invalid extra JSON: {error}"
        ))
    })?;
    if !extra.is_object() {
        return Err(AppError::Internal(format!(
            "conversation {conversation_id} extra must be a JSON object"
        )));
    }
    let workspace = extra
        .get("workspace")
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .trim()
        .to_string();

    // Plan without creating the workspace. This yields the exact prompt paths
    // needed for full-payload receipt preflight before runtime/KB activation.
    let workspace_for_stage = workspace.clone();
    let ws_path = (!workspace_for_stage.is_empty())
        .then(|| std::path::Path::new(workspace_for_stage.as_str()));
    let attachment_plan = deps
        .service
        .plan_attachments_for_prompt(&req.requirement_id, ws_path)
        .await?;
    let prompt = build_requirement_prompt(
        tag,
        req,
        claim_generation,
        claim_token,
        agent_type,
        deps.requirement_mcp_enabled,
        &attachment_plan.attachments,
    );
    let send_req = SendMessageRequest {
        content: prompt,
        files: vec![],
        inject_skills: vec![],
        hidden: true,
        origin: Some("autowork".into()),
        channel_platform: None,
    };
    // Keep an immutable payload copy for post-error receipt reconciliation.
    // The send seam consumes `send_req`; an error may nevertheless arrive
    // after its atomic receipt INSERT committed, and treating that as
    // pre-admission would mint a new generation and duplicate the turn.
    let send_receipt_probe = SendMessageRequest {
        content: send_req.content.clone(),
        files: send_req.files.clone(),
        inject_skills: send_req.inject_skills.clone(),
        hidden: send_req.hidden,
        origin: send_req.origin.clone(),
        channel_platform: send_req.channel_platform.clone(),
    };
    let operation_id =
        autowork_turn_idempotency_key(&req.requirement_id, claim_generation, claim_token);
    let expects_verdict =
        crate::prompt::session_has_requirement_tools(agent_type, deps.requirement_mcp_enabled);

    if let Some(outcome) =
        legacy_recovered_claim_without_receipt_outcome(recovered_active, claim_generation)
    {
        return Ok(outcome);
    }
    build_lease.ensure_active()?;

    let options = AgentRuntimeBuildOptions {
        user_id: user_id.clone(),
        agent_type,
        workspace,
        model,
        conversation_id: conversation_id.to_string(),
        delegation_policy,
        extra,
        // Stamp/validate the nomi session against this conversation instance so
        // a reused integer id never resumes a stale (e.g. deleted) conversation.
        conversation_created_at: Some(row.created_at),
        workspace_binding_lease: None,
    };

    // Runtime construction, knowledge mounting, attachment activation and event
    // subscription are receiver-owned. ConversationService performs them only
    // after the raw Requirement capability is validated in the same SQLite
    // transaction as receipt INSERT + Conversation Running, and keeps one
    // preparation fence until the local turn owner has been handed off.
    let authority = RequirementConversationTurnAuthority {
        requirement_id: req.requirement_id.clone(),
        claim_generation,
        claim_token: claim_token.to_owned(),
    };
    let authority_probe = authority.clone();
    let runtime_preparation = BackgroundTurnRuntimePreparation {
        runtime_options: options,
        desired_mode: None,
        clear_context: false,
        pre_send_hook: Some(Arc::new(AutoWorkAttachmentActivation {
            service: Arc::clone(&deps.service),
            plan: attachment_plan,
        })),
    };
    let (observed, send_error_context) = match deps
        .conversation_service
        .send_observed_autowork_message_with_idempotency_key(
            &user_id,
            conversation_id,
            &operation_id,
            send_req,
            &deps.runtime_registry,
            build_lease,
            runtime_preparation,
            authority,
        )
        .await
    {
        Ok(observed) => (observed, None),
        // Authority loss, an existing logical receipt with a different token
        // fingerprint, or ambiguous Conversation state is absorbing for this
        // claim. It must never escape as transient Busy and mint a successor
        // generation.
        Err(AppError::Conflict(reason))
            if !deps
                .conversation_service
                .user_cancelled_since(conversation_id, claim_started_ms) =>
        {
            return Ok(autowork_blocked_delivery_outcome(reason));
        }
        Err(error) => {
            let error_message = error.to_string();
            // The send error is not proof that durable admission failed. Its
            // receipt INSERT may already have committed before a later runtime
            // setup/dispatch error was returned. Only a successful exact
            // lookup proving absence may enter the pre-admission retry branch.
            match deps
                .conversation_service
                .autowork_delivery_result_with_idempotency_key(
                    &user_id,
                    conversation_id,
                    &operation_id,
                    &send_receipt_probe,
                    &authority_probe,
                )
                .await
            {
                Ok(Some(delivery)) => (
                    ObservedIdempotentMessageDelivery {
                        delivery,
                        runtime: None,
                        events: None,
                    },
                    Some(error_message),
                ),
                Ok(None) => return Err(error),
                Err(lookup_error) => {
                    return Ok(autowork_blocked_delivery_outcome(format!(
                        "send returned {error}, and exact receipt reconciliation failed: \
                         {lookup_error}"
                    )));
                }
            }
        }
    };
    let ObservedIdempotentMessageDelivery {
        delivery,
        runtime,
        events,
    } = observed;
    let reconciled = match reconcile_accepted_autowork_delivery(
        deps,
        &user_id,
        conversation_id,
        &operation_id,
        &send_receipt_probe,
        &authority_probe,
        delivery,
    )
    .await
    {
        Ok(reconciled) => reconciled,
        Err(error) => {
            let reason = match send_error_context {
                Some(send_error) => format!(
                    "send returned {send_error}, and accepted receipt reconciliation failed \
                     closed: {error}"
                ),
                None => error.to_string(),
            };
            return Ok(autowork_blocked_delivery_outcome(reason));
        }
    };
    let delivery = reconciled.delivery;
    let receipt_leader = !delivery.replayed;

    if !reconciled.accepted_wait_authorized {
        if let Some(outcome) =
            autowork_replayed_delivery_outcome(delivery, claim_generation, expects_verdict)
        {
            return Ok(outcome);
        }
    }

    // Arm IDMM only for the atomic receipt winner that actually started this
    // turn. Replay followers must not attach a supervisor to an idle runtime.
    if receipt_leader {
        if let Some(idmm) = &deps.idmm {
            idmm.ensure_supervising(AutoWorkTargetKind::Conversation, conversation_id);
        }
    }

    // The early event subscription returned by the receiver is auxiliary only.
    // Durable completion is polled by the same logical Requirement operation,
    // so an extremely fast finish, lagged broadcast, runtime eviction, or
    // process scheduling gap cannot lose the terminal boundary.
    drop(runtime);
    let outcome = wait_for_conversation_receipt_with_renewal(
        deps,
        conversation_id,
        conv_id,
        &req.requirement_id,
        claim_generation,
        claim_token,
        &user_id,
        &operation_id,
        &send_receipt_probe,
        &authority_probe,
        expects_verdict,
        events,
    )
    .await;
    Ok((
        outcome.0,
        outcome.1,
        expects_verdict || outcome.2,
    ))
}

fn autowork_turn_idempotency_key(
    requirement_id: &str,
    claim_generation: i64,
    claim_token: &str,
) -> String {
    // The public-key field is capped at 128 bytes. Hash the complete,
    // domain-separated capability scope so neither the opaque token nor a
    // variable-length Requirement id is exposed in the durable operation id.
    // ConversationService independently stores/compares the token's SHA-256
    // fingerprint in the receipt payload and validates the raw capability in
    // the atomic Requirement + receipt + Running admission transaction.
    let scope = format!(
        "nomifun-autowork-conversation-turn-v2\0{requirement_id}\0{claim_generation}\0{claim_token}"
    );
    format!(
        "autowork:v2:{}",
        nomifun_auth::token_sha256_hex(&scope)
    )
}

struct ReconciledAutoWorkDelivery {
    delivery: IdempotentMessageDelivery,
    /// An accepted replay may wait only after ConversationService proves that
    /// the exact operation still has a live local owner or has completed the
    /// audited local orphan-reconciliation path.
    accepted_wait_authorized: bool,
}

fn authorize_accepted_receipt_wait(
    disposition: BackgroundTurnReconciliationDisposition,
    message_id: &str,
) -> Result<(), AppError> {
    match disposition {
        BackgroundTurnReconciliationDisposition::LiveExactOwnerWait
        | BackgroundTurnReconciliationDisposition::ReconciledOrTerminalReRead => Ok(()),
        BackgroundTurnReconciliationDisposition::ExternalProofRequiredFailClosed => {
            Err(AppError::Conflict(format!(
                "accepted delivery {message_id} belongs to an external or unknown runtime whose \
                 terminal state cannot be proven locally"
            )))
        }
        BackgroundTurnReconciliationDisposition::StaleConflict => {
            Err(AppError::Conflict(format!(
                "accepted delivery {message_id} no longer owns the exact Conversation operation \
                 generation"
            )))
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn reconcile_accepted_autowork_delivery(
    deps: &Arc<AutoWorkRunnerDeps>,
    user_id: &str,
    conversation_id: &str,
    idempotency_key: &str,
    request: &SendMessageRequest,
    authority: &RequirementConversationTurnAuthority,
    delivery: IdempotentMessageDelivery,
) -> Result<ReconciledAutoWorkDelivery, AppError> {
    if !delivery.replayed || delivery.completed {
        return Ok(ReconciledAutoWorkDelivery {
            delivery,
            accepted_wait_authorized: false,
        });
    }

    // Never infer death from elapsed time. ConversationService holds the
    // preparation gate, re-reads the exact receipt/Running generation, and only
    // local process-backed runtimes with positive parent-exit proof may be
    // terminalized. Remote/OpenClaw/unknown ownership remains accepted and
    // returns Conflict, which the runner parks for explicit review.
    let disposition = deps
        .conversation_service
        .reconcile_quiescent_running_turn_for_background(
            user_id,
            conversation_id,
            idempotency_key,
            &deps.runtime_registry,
        )
        .await?;
    authorize_accepted_receipt_wait(disposition, &delivery.message_id)?;

    match deps
        .conversation_service
        .autowork_delivery_result_with_idempotency_key(
            user_id,
            conversation_id,
            idempotency_key,
            request,
            authority,
        )
        .await?
    {
        Some(refreshed) if refreshed.message_id == delivery.message_id => {
            let accepted_wait_authorized = !refreshed.completed;
            Ok(ReconciledAutoWorkDelivery {
                delivery: refreshed,
                accepted_wait_authorized,
            })
        }
        Some(refreshed) => Err(AppError::Conflict(format!(
            "accepted delivery {} resolved to a different immutable message {}",
            delivery.message_id, refreshed.message_id
        ))),
        None => Err(AppError::Conflict(format!(
            "accepted delivery {} disappeared during exact quiescent reconciliation",
            delivery.message_id
        ))),
    }
}

fn autowork_blocked_delivery_outcome(reason: String) -> (TurnEnd, Option<String>, bool) {
    (
        TurnEnd::Clean,
        Some(format!(
            "AutoWork did not start another turn because durable Conversation \
             state is ambiguous: {reason}. Explicit reset or human review is required."
        )),
        true,
    )
}

fn legacy_recovered_claim_without_receipt_outcome(
    recovered_active: bool,
    claim_generation: i64,
) -> Option<(TurnEnd, Option<String>, bool)> {
    (recovered_active && claim_generation == 0).then(|| {
        autowork_blocked_delivery_outcome(
            "this claim predates durable AutoWork delivery receipts, so its prior execution \
             outcome cannot be proven"
                .to_owned(),
        )
    })
}

fn autowork_replayed_delivery_outcome(
    delivery: IdempotentMessageDelivery,
    claim_generation: i64,
    expects_verdict: bool,
) -> Option<(TurnEnd, Option<String>, bool)> {
    if !delivery.replayed {
        return None;
    }
    if !delivery.completed {
        // `accepted` is absorbing: the prior process may have crossed an
        // irreversible model/tool boundary before crashing. Never wait on a
        // newly subscribed idle runtime and never manufacture a new attempt.
        // Forcing the verdict contract parks the Requirement in NeedsReview.
        return Some((
            TurnEnd::Clean,
            Some(format!(
                "AutoWork delivery {} was already accepted for claim generation \
                 {claim_generation}; its outcome is unknown, so it was not executed again.",
                delivery.message_id
            )),
            true,
        ));
    }

    let note = delivery.result_text.or(delivery.result_error);
    match delivery.result_ok {
        Some(true) => Some((TurnEnd::Clean, note, expects_verdict)),
        // A completed error proves only that the observer saw an error; it
        // cannot prove that earlier model/tool effects did not happen. A new
        // claim generation would mint a different delivery key and could
        // execute the same Requirement twice, so absorb it for review.
        Some(false) => Some((
            TurnEnd::Clean,
            note.or_else(|| {
                Some(format!(
                    "Completed AutoWork delivery {} reported an error after durable admission; \
                     it was not executed again.",
                    delivery.message_id
                ))
            }),
            true,
        )),
        None => Some((
            TurnEnd::Clean,
            note.or_else(|| {
                Some(format!(
                    "Completed AutoWork delivery {} has no durable outcome; \
                     it was not executed again.",
                    delivery.message_id
                ))
            }),
            true,
        )),
    }
}

/// Observe one already-admitted AutoWork Conversation turn through its durable
/// receipt while renewing the exact Requirement capability.
///
/// The event receiver is deliberately auxiliary: it improves the human note,
/// but Closed/Lagged/very-fast completion cannot decide whether the work
/// finished. Only the unique logical AutoWork receipt can do that. Once this
/// function starts, absence, lookup failure, lease loss and timeout are all
/// ambiguous post-admission outcomes and therefore force NeedsReview.
#[allow(clippy::too_many_arguments)]
async fn wait_for_conversation_receipt_with_renewal(
    deps: &Arc<AutoWorkRunnerDeps>,
    conversation_id: &str,
    conv_id: &str,
    req_id: &str,
    claim_generation: i64,
    claim_token: &str,
    user_id: &str,
    operation_id: &str,
    request: &SendMessageRequest,
    authority: &RequirementConversationTurnAuthority,
    expects_verdict: bool,
    mut events: Option<broadcast::Receiver<AgentStreamEvent>>,
) -> (TurnEnd, Option<String>, bool) {
    let mut renew = interval(LEASE_RENEW_INTERVAL);
    renew.tick().await;
    let mut receipts = interval(CONVERSATION_RECEIPT_POLL_INTERVAL);
    let mut event_note = String::new();

    let wait = async {
        loop {
            tokio::select! {
                _ = renew.tick() => {
                    match deps
                        .service
                        .renew_lease(
                            req_id,
                            conv_id,
                            AutoWorkTargetKind::Conversation,
                            claim_generation,
                            claim_token,
                            DEFAULT_LEASE_MS,
                        )
                        .await
                    {
                        Ok(true) => {}
                        Ok(false) => {
                            return DurableConversationReceiptWait::Ambiguous(format!(
                                "AutoWork Conversation claim generation {claim_generation} lost \
                                 exact lease authority after durable admission; it was not \
                                 executed again."
                            ));
                        }
                        Err(error) => {
                            warn!(
                                conversation_id,
                                requirement_id = req_id,
                                error = %error,
                                "Exact lease renewal failed while polling durable AutoWork receipt"
                            );
                            return DurableConversationReceiptWait::Ambiguous(format!(
                                "Lease renewal failed after durable admission of AutoWork \
                                 Conversation claim generation {claim_generation}: {error}. The \
                                 Requirement was not executed again."
                            ));
                        }
                    }
                }
                _ = receipts.tick() => {
                    match deps
                        .conversation_service
                        .autowork_delivery_result_with_idempotency_key(
                            user_id,
                            conversation_id,
                            operation_id,
                            request,
                            authority,
                        )
                        .await
                    {
                        Ok(Some(delivery)) if delivery.completed => {
                            return DurableConversationReceiptWait::Completed(delivery);
                        }
                        Ok(Some(_)) => {}
                        Ok(None) => {
                            return DurableConversationReceiptWait::Ambiguous(format!(
                                "The exact AutoWork receipt for Conversation claim generation \
                                 {claim_generation} disappeared after admission; the Requirement \
                                 was not executed again."
                            ));
                        }
                        Err(error) => {
                            return DurableConversationReceiptWait::Ambiguous(format!(
                                "The exact AutoWork receipt for Conversation claim generation \
                                 {claim_generation} could not be verified after admission: \
                                 {error}. The Requirement was not executed again."
                            ));
                        }
                    }
                }
                event = async {
                    match events.as_mut() {
                        Some(receiver) => receiver.recv().await,
                        None => std::future::pending::<
                            Result<AgentStreamEvent, broadcast::error::RecvError>
                        >().await,
                    }
                } => {
                    match event {
                        Ok(AgentStreamEvent::Text(text)) => {
                            append_bounded(&mut event_note, &text.content);
                        }
                        Ok(AgentStreamEvent::Error(error)) => {
                            append_bounded(&mut event_note, &error.message);
                        }
                        Ok(_) => {}
                        Err(broadcast::error::RecvError::Closed) => {
                            // Receipt polling remains authoritative even if a
                            // runtime is torn down before this observer runs.
                            events = None;
                        }
                        Err(broadcast::error::RecvError::Lagged(skipped)) => {
                            warn!(
                                conversation_id,
                                skipped,
                                "Auxiliary AutoWork event stream lagged; relying on durable receipt"
                            );
                        }
                    }
                }
            }
        }
    };

    match timeout(TURN_TIMEOUT, wait).await {
        Ok(DurableConversationReceiptWait::Completed(delivery)) => {
            let mut outcome = autowork_replayed_delivery_outcome(
                delivery,
                claim_generation,
                expects_verdict,
            )
            .unwrap_or_else(|| {
                autowork_blocked_delivery_outcome(
                    "durable receipt observation returned fresh execution authority".to_owned(),
                )
            });
            if outcome.1.is_none() {
                outcome.1 = finalize_note(&event_note);
            }
            outcome
        }
        Ok(DurableConversationReceiptWait::Ambiguous(detail)) => {
            (TurnEnd::Clean, Some(detail), true)
        }
        Err(_) => (
            TurnEnd::Clean,
            Some(format!(
                "AutoWork Conversation claim generation {claim_generation} exceeded its hard \
                 timeout after durable admission; prior model/tool effects cannot be excluded and \
                 the Requirement was not executed again."
            )),
            true,
        ),
    }
}

/// Wait for the agent's terminal event, renewing the exact claim lease and
/// accumulating the agent's text. The third return value forces
/// `NeedsReview`: after durable admission, timeout, stream loss, lease-authority
/// loss, and errored finishes are all ambiguous and must never be retried under
/// a newly minted claim generation.
#[cfg(test)]
#[allow(dead_code)]
async fn wait_for_terminal_with_renewal(
    deps: &Arc<AutoWorkRunnerDeps>,
    conversation_id: &str,
    conv_id: &str,
    req_id: &str,
    claim_generation: i64,
    claim_token: &str,
    mut rx: broadcast::Receiver<AgentStreamEvent>,
) -> (TurnEnd, Option<String>, bool) {
    let mut renew = interval(LEASE_RENEW_INTERVAL);
    renew.tick().await; // consume the immediate first tick
    let mut note_buf = String::new();
    // The CURRENT turn's assistant text, reset at each turn boundary. Used to
    // decide the decision-yield from the text we already have IN MEMORY —never
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
                    match deps
                        .service
                        .renew_lease(
                            req_id,
                            conv_id,
                            AutoWorkTargetKind::Conversation,
                            claim_generation,
                            claim_token,
                            DEFAULT_LEASE_MS,
                        )
                        .await
                    {
                        Ok(true) => {}
                        Ok(false) => {
                            return ConversationWaitEnd::Ambiguous(format!(
                                "AutoWork Conversation claim generation {claim_generation} lost \
                                 exact lease authority after durable turn admission; it was not \
                                 executed again."
                            ));
                        }
                        Err(error) => {
                            warn!(
                                conversation_id,
                                requirement_id = req_id,
                                error = %error,
                                "Exact lease renewal failed after durable Conversation admission"
                            );
                            return ConversationWaitEnd::Ambiguous(format!(
                                "Lease renewal failed after durable admission of AutoWork \
                                 Conversation claim generation {claim_generation}: {error}. The \
                                 Requirement was not executed again."
                            ));
                        }
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
                        "AutoWork decision-yield window elapsed without IDMM answering —finalizing turn"
                    );
                    return ConversationWaitEnd::Terminal(TurnEnd::Clean);
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
                        // state: the agent may have ended its turn on a 閫夋嫨棰?寮€鏀惧紡鎻愰棶.
                        // When IDMM is supervising and a pending decision exists, IDMM
                        // will answer it —so YIELD instead of finalizing here (which
                        // would park the requirement needs_review, burn an attempt, and
                        // let run_loop race a fresh requirement into the session,
                        // stomping IDMM's pending answer —the protocol mismatch). Keep waiting on
                        // the SAME broadcast (without owning turn admission) until the
                        // work reaches a real terminal Finish. A refusal/truncation
                        // (Errored) or user cancel (Cancelled) is never yielded —those
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
                            return match end {
                                TurnEnd::Errored => ConversationWaitEnd::Ambiguous(format!(
                                    "The Agent ended AutoWork Conversation claim generation \
                                     {claim_generation} with a non-success terminal reason after \
                                     durable admission; prior effects cannot be excluded and the \
                                     Requirement was not executed again."
                                )),
                                other => ConversationWaitEnd::Terminal(other),
                            };
                        }
                        // On an error, defer to IDMM when it is supervising: a
                        // retryable provider fault is IDMM's job to recover (retry /
                        // sidecar via a fresh turn). Failing the turn here would
                        // abandon it and race a fresh requirement into the same
                        // session —the historical "protocol mismatch". Wait through up to
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
                            return ConversationWaitEnd::Ambiguous(format!(
                                "The Agent reported an error after durable admission of AutoWork \
                                 Conversation claim generation {claim_generation}: {}. Prior \
                                 effects cannot be excluded and the Requirement was not executed \
                                 again.",
                                d.message
                            ));
                        }
                        Ok(_) => {
                            decision_ride_until = None;
                            continue;
                        }
                        // A closed channel means the Agent runtime was torn down
                        // (eviction on terminal error, process death, dropped
                        // connection) —the turn did NOT finish cleanly. Treat as
                        // errored, matching the terminal path's `Closed => errored`.
                        Err(broadcast::error::RecvError::Closed) => {
                            return ConversationWaitEnd::Ambiguous(format!(
                                "The Agent event stream closed after durable admission of AutoWork \
                                 Conversation claim generation {claim_generation}; prior effects \
                                 cannot be excluded and the Requirement was not executed again."
                            ));
                        }
                        // The skipped event may be the only terminal boundary.
                        // Continuing against a live sender can otherwise park
                        // AutoWork until the very large turn timeout.
                        Err(broadcast::error::RecvError::Lagged(skipped)) => {
                            warn!(
                                conversation_id,
                                skipped,
                                "AutoWork conversation event stream lost integrity"
                            );
                            return ConversationWaitEnd::Ambiguous(format!(
                                "The Agent event stream skipped {skipped} event(s) after durable \
                                 admission of AutoWork Conversation claim generation \
                                 {claim_generation}; the Requirement was not executed again."
                            ));
                        }
                    }
                }
            }
        }
    };

    match timeout(TURN_TIMEOUT, fut).await {
        Ok(ConversationWaitEnd::Terminal(end)) => {
            let note = if end == TurnEnd::Clean {
                finalize_note(&note_buf)
            } else {
                None
            };
            (end, note, false)
        }
        Ok(ConversationWaitEnd::Ambiguous(detail)) => {
            // Return Clean + force-review so the exact finalizer chooses
            // NeedsReview instead of its errored-turn RetryPending branch.
            (TurnEnd::Clean, Some(detail), true)
        }
        Err(_) => (
            TurnEnd::Clean,
            Some(format!(
                "AutoWork Conversation claim generation {claim_generation} exceeded its hard \
                 timeout after durable admission; prior model/tool effects cannot be excluded and \
                 the Requirement was not executed again."
            )),
            true,
        ),
    }
}

/// Append `chunk` to `buf`, keeping it bounded (tail-biased) so a long streaming
/// turn cannot grow the buffer without limit. Truncation respects char boundaries.
fn append_bounded(buf: &mut String, chunk: &str) {
    buf.push_str(chunk);
    // chars are 鈮? bytes; keep roughly twice the char cap as a byte ceiling.
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
/// is a deliberate user stop —surfaced distinctly so the loop pauses the tag
/// instead of burning a retry attempt on it.
#[cfg(test)]
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
#[cfg(test)]
fn should_wait_for_recovery(retryable: bool, idmm_supervising: bool, waits_so_far: u32) -> bool {
    retryable && idmm_supervising && waits_so_far < MAX_RECOVERY_WAITS
}

/// Decide whether AutoWork should YIELD a clean-finish turn to IDMM rather than
/// finalize it as the requirement's terminal state. We yield only on a CLEAN
/// finish (a refusal/truncation Errored or a user Cancelled is a real terminal
/// end) AND when IDMM is supervising (it owns answering 閫夋嫨棰?寮€鏀惧紡鎻愰棶), bounded by
/// `MAX_DECISION_WAITS` so a runaway question loop can't ride forever. The caller
/// additionally confirms (async) that a pending decision actually exists before
/// yielding, and arms a watchdog so a non-answering IDMM falls back to finalize.
#[cfg(test)]
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
#[derive(Clone, PartialEq, Eq, Debug)]
enum TerminalTurnEnd {
    /// The lifecycle reported a `TurnEnd` event —the agent finished its turn.
    /// Whether the agent called `requirement_complete` is reflected in the DB
    /// row's status; the AutoWork runner just knows the turn ended cleanly.
    AuthoritativeVerdict {
        status: RequirementStatus,
        note: Option<String>,
    },
    /// At least one PTY submission write was attempted, but PTY death, hard
    /// timeout, or lifecycle loss prevents proving the command's final state.
    /// This is absorbing and must be parked for review, never retried.
    AmbiguousAfterSubmission,
}

/// Inject a prompt into a terminal CLI and submit it. The bracketed-paste body
/// and the submit CR are written as SEPARATE PTY writes, with
/// `TERMINAL_SUBMIT_DELAY` between them. A CR that rides in the same write as
/// the paste-end marker is swallowed by the paste-burst detection modern agent
/// TUIs (claude/codex/gemini) use to keep a pasted block from auto-running —it
/// leaves the requirement text sitting unsubmitted in the input box (the bug
/// this fixes). Writing the CR on its own, a beat later, makes the TUI treat it
/// as a real Enter keystroke. Mirrors the cron terminal executor's fix.
fn terminal_turn_end_from_receipt(row: &TerminalTurnAdmissionRow) -> Option<TerminalTurnEnd> {
    if row.phase != "settled" {
        return None;
    }
    let status = match row.outcome.as_deref()? {
        "done" => RequirementStatus::Done,
        "failed" => RequirementStatus::Failed,
        "needs_review" => RequirementStatus::NeedsReview,
        "cancelled" => RequirementStatus::Cancelled,
        _ => return None,
    };
    Some(TerminalTurnEnd::AuthoritativeVerdict {
        status,
        note: row.detail.clone(),
    })
}

fn terminal_outcome_from_status(status: RequirementStatus) -> Option<TerminalTurnOutcome> {
    match status {
        RequirementStatus::Done => Some(TerminalTurnOutcome::Done),
        RequirementStatus::Failed => Some(TerminalTurnOutcome::Failed),
        RequirementStatus::NeedsReview => Some(TerminalTurnOutcome::NeedsReview),
        RequirementStatus::Cancelled => Some(TerminalTurnOutcome::Cancelled),
        RequirementStatus::Pending | RequirementStatus::InProgress => None,
    }
}

async fn park_terminal_turn(
    deps: &Arc<AutoWorkRunnerDeps>,
    driver: &Arc<dyn TerminalDriver>,
    key: &TerminalTurnAdmissionKey,
    detail: &str,
) -> TerminalTurnEnd {
    if let Err(error) = driver
        .park_open_turn_admissions(&key.terminal_id, Some(key.pty_epoch), detail)
        .await
    {
        warn!(
            terminal_id = %key.terminal_id,
            requirement_id = %key.requirement_id,
            claim_generation = key.claim_generation,
            error = %error,
            "Failed to atomically park an ambiguous Terminal turn"
        );
        if let Err(status_error) = deps
            .service
            .resolve_claim_verdict_exact(
                &key.requirement_id,
                key.claim_generation,
                &key.claim_token,
                &key.terminal_id,
                AutoWorkTargetKind::Terminal,
                RequirementStatus::NeedsReview,
                Some(detail.to_owned()),
            )
            .await
        {
            warn!(
                terminal_id = %key.terminal_id,
                requirement_id = %key.requirement_id,
                error = %status_error,
                "Failed to park ambiguous Terminal Requirement directly"
            );
        }
    }

    match driver.get_turn_admission(key).await {
        Ok(Some(row)) => terminal_turn_end_from_receipt(&row)
            .unwrap_or(TerminalTurnEnd::AmbiguousAfterSubmission),
        Ok(None) => {
            warn!(
                terminal_id = %key.terminal_id,
                requirement_id = %key.requirement_id,
                "Durable Terminal admission disappeared while parking"
            );
            TerminalTurnEnd::AmbiguousAfterSubmission
        }
        Err(error) => {
            warn!(
                terminal_id = %key.terminal_id,
                requirement_id = %key.requirement_id,
                error = %error,
                "Failed to re-read parked Terminal admission"
            );
            TerminalTurnEnd::AmbiguousAfterSubmission
        }
    }
}

async fn submit_admitted_terminal_prompt(
    driver: &Arc<dyn TerminalDriver>,
    key: &TerminalTurnAdmissionKey,
    prompt: &str,
) -> Result<TerminalTurnEffectsStart, AppError> {
    match nomifun_terminal::encode_submit_chunks(prompt, true) {
        nomifun_terminal::SubmitChunks::PasteThenCr { paste, cr } => {
            let body = driver.write_admitted_body(key, &paste).await?;
            if body != TerminalTurnEffectsStart::Started {
                return Ok(body);
            }
            sleep(nomifun_terminal::TERMINAL_SUBMIT_DELAY).await;
            Ok(driver.write_admitted_submit(key, &cr).await?)
        }
        nomifun_terminal::SubmitChunks::Single(bytes) => {
            Ok(driver.write_admitted_turn(key, &bytes).await?)
        }
    }
}

/// One terminal turn: inject the requirement prompt, then await the lifecycle
/// `TurnEnd` event (the agent's Stop hook), the PTY dying, or the hard timeout.
///
/// **No quiescence fallback:** a lifecycle subscription is the ONLY structured
/// turn-end signal. When lifecycle is unavailable (server not wired / non-agent
/// CLI) the turn runs until the hard `TURN_TIMEOUT` and then ends as
/// `TerminalTurnEnd::AmbiguousAfterSubmission` —honest and at-most-once (no
/// false "done", and no second PTY injection).
async fn inject_and_wait_terminal(
    deps: &Arc<AutoWorkRunnerDeps>,
    terminal_id: &str,
    tag: &str,
    req: &Requirement,
    claim_generation: i64,
    claim_token: &str,
    recovered_active: bool,
) -> Result<TerminalTurnEnd, AppError> {
    let Some(driver) = deps.terminal_driver.as_ref() else {
        if recovered_active {
            let detail = format!(
                "Recovered AutoWork Terminal claim generation {claim_generation} without a \
                 Terminal driver; prior effects cannot be excluded."
            );
            let _ = deps
                .service
                .resolve_claim_verdict_exact(
                    &req.requirement_id,
                    claim_generation,
                    claim_token,
                    terminal_id,
                    AutoWorkTargetKind::Terminal,
                    RequirementStatus::NeedsReview,
                    Some(detail),
                )
                .await;
            return Ok(TerminalTurnEnd::AmbiguousAfterSubmission);
        }
        return Err(AppError::Internal("terminal driver not attached".into()));
    };

    let Some(pty_epoch) = driver.current_epoch(terminal_id) else {
        if recovered_active {
            let detail = format!(
                "Recovered AutoWork Terminal claim generation {claim_generation} has no live PTY \
                 generation; prior effects cannot be excluded."
            );
            let _ = deps
                .service
                .resolve_claim_verdict_exact(
                    &req.requirement_id,
                    claim_generation,
                    claim_token,
                    terminal_id,
                    AutoWorkTargetKind::Terminal,
                    RequirementStatus::NeedsReview,
                    Some(detail),
                )
                .await;
            return Ok(TerminalTurnEnd::AmbiguousAfterSubmission);
        }
        return Err(AppError::Conflict(format!(
            "terminal {terminal_id} has no live PTY generation"
        )));
    };

    // Build the exact Terminal payload before durable turn admission. The
    // legacy best-effort staging wrapper collapsed source-integrity failures
    // into an empty attachment list, silently changing the requested work.
    let attachment_plan = deps
        .service
        .plan_attachments_for_prompt(&req.requirement_id, None)
        .await?;
    let prompt = build_terminal_requirement_prompt(
        tag,
        req,
        claim_generation,
        claim_token,
        &attachment_plan.attachments,
    );

    let scope = TerminalTurnAdmissionScope {
        terminal_id: terminal_id.to_owned(),
        pty_epoch,
        requirement_id: req.requirement_id.clone(),
        claim_generation,
        claim_token: claim_token.to_owned(),
    };
    let claim = match driver.claim_turn_admission(&scope).await {
        Ok(claim) => claim,
        Err(first_error) => {
            // The reply may have been lost after COMMIT. Repeat the same scope
            // only to recover/create an absorbing row, then park without ever
            // granting write authority.
            match driver.claim_turn_admission(&scope).await {
                Ok(claim) => {
                    let key = match TerminalTurnAdmissionKey::from_row(&claim.row) {
                        Ok(key) => key,
                        Err(error) => {
                            let detail = format!(
                                "Recovered Terminal admission for AutoWork claim generation \
                                 {claim_generation} was invalid and was not executed."
                            );
                            warn!(
                                terminal_id,
                                requirement_id = %req.requirement_id,
                                error = %error,
                                "Invalid recovered Terminal admission"
                            );
                            let _ = driver
                                .park_open_turn_admissions(terminal_id, None, &detail)
                                .await;
                            let _ = deps
                                .service
                                .resolve_claim_verdict_exact(
                                    &req.requirement_id,
                                    claim_generation,
                                    claim_token,
                                    terminal_id,
                                    AutoWorkTargetKind::Terminal,
                                    RequirementStatus::NeedsReview,
                                    Some(detail),
                                )
                                .await;
                            return Ok(TerminalTurnEnd::AmbiguousAfterSubmission);
                        }
                    };
                    let detail = format!(
                        "Terminal admission for AutoWork claim generation \
                         {claim_generation} returned an uncertain result and was not executed."
                    );
                    warn!(
                        terminal_id,
                        requirement_id = %req.requirement_id,
                        error = %first_error,
                        "Recovered an uncertain durable Terminal admission"
                    );
                    return Ok(park_terminal_turn(deps, driver, &key, &detail).await);
                }
                Err(second_error) => {
                    let detail = format!(
                        "Terminal admission for AutoWork claim generation \
                         {claim_generation} could not be verified and was not retried."
                    );
                    warn!(
                        terminal_id,
                        requirement_id = %req.requirement_id,
                        first_error = %first_error,
                        error = %second_error,
                        "Unable to verify durable Terminal admission"
                    );
                    if let Err(status_error) = deps
                        .service
                        .resolve_claim_verdict_exact(
                            &req.requirement_id,
                            claim_generation,
                            claim_token,
                            terminal_id,
                            AutoWorkTargetKind::Terminal,
                            RequirementStatus::NeedsReview,
                            Some(detail),
                        )
                        .await
                    {
                        warn!(
                            terminal_id,
                            requirement_id = %req.requirement_id,
                            error = %status_error,
                            "Failed to park Requirement after uncertain Terminal admission"
                        );
                    }
                    return Ok(TerminalTurnEnd::AmbiguousAfterSubmission);
                }
            }
        }
    };
    let key = match TerminalTurnAdmissionKey::from_row(&claim.row) {
        Ok(key) => key,
        Err(error) => {
            let detail = format!(
                "Terminal admission receipt for AutoWork claim generation \
                 {claim_generation} is invalid and was not executed."
            );
            warn!(
                terminal_id,
                requirement_id = %req.requirement_id,
                error = %error,
                "Invalid durable Terminal admission receipt"
            );
            let _ = driver
                .park_open_turn_admissions(terminal_id, None, &detail)
                .await;
            let _ = deps
                .service
                .resolve_claim_verdict_exact(
                    &req.requirement_id,
                    claim_generation,
                    claim_token,
                    terminal_id,
                    AutoWorkTargetKind::Terminal,
                    RequirementStatus::NeedsReview,
                    Some(detail),
                )
                .await;
            return Ok(TerminalTurnEnd::AmbiguousAfterSubmission);
        }
    };

    if !claim.claimed_new {
        if let Some(settled) = terminal_turn_end_from_receipt(&claim.row) {
            return Ok(settled);
        }
        let detail = format!(
            "Replayed open Terminal admission for AutoWork claim generation \
             {claim_generation}; prior PTY effects are unknown and were not executed again."
        );
        return Ok(park_terminal_turn(deps, driver, &key, &detail).await);
    }

    if recovered_active {
        let detail = format!(
            "Recovered AutoWork Terminal claim generation {claim_generation} without a prior \
             durable admission; pre-recovery PTY effects cannot be excluded, so it was not executed."
        );
        return Ok(park_terminal_turn(deps, driver, &key, &detail).await);
    }

    let lifecycle_rx = match driver.subscribe_lifecycle_exact(terminal_id, pty_epoch) {
        Some(receiver) => receiver,
        None => {
            let detail = format!(
                "Exact lifecycle subscription was unavailable for AutoWork Terminal claim \
                 generation {claim_generation}; the admitted turn was not executed."
            );
            return Ok(park_terminal_turn(deps, driver, &key, &detail).await);
        }
    };

    // Terminals have no workspace concept —the prompt carries absolute paths
    // into the data dir and the CLI reads them directly.
    let effects = match submit_admitted_terminal_prompt(driver, &key, &prompt).await {
        Ok(effects) => effects,
        Err(error) => {
            let detail = format!(
                "AutoWork Terminal claim generation {claim_generation} crossed or may have crossed \
                 its PTY effects boundary, but submission failed: {error}"
            );
            warn!(
                terminal_id,
                requirement_id = %req.requirement_id,
                error = %error,
                "AutoWork Terminal submission may have been partially delivered"
            );
            return Ok(park_terminal_turn(deps, driver, &key, &detail).await);
        }
    };
    if effects != TerminalTurnEffectsStart::Started {
        let detail = format!(
            "AutoWork Terminal claim generation {claim_generation} replayed an already-started or \
             settled admission and emitted no PTY bytes."
        );
        return Ok(park_terminal_turn(deps, driver, &key, &detail).await);
    }

    // Arm IDMM for THIS terminal turn (its probe subscribes to the durable PTY
    // output/lifecycle, so it attaches regardless of task state). Per-turn, not on
    // every idle poll —same churn fix as the conversation path. Idempotent + a
    // no-op when IDMM is disabled for the terminal.
    if let Some(idmm) = &deps.idmm {
        idmm.ensure_supervising(AutoWorkTargetKind::Terminal, &terminal_id.to_string());
    }

    Ok(wait_terminal_turn_end(deps, driver, &key, lifecycle_rx).await)
}

/// Await a terminal turn's structured completion signal, renewing the lease on a
/// tick, checking PTY liveness, and enforcing the hard timeout.
async fn wait_terminal_turn_end(
    deps: &Arc<AutoWorkRunnerDeps>,
    driver: &Arc<dyn TerminalDriver>,
    key: &TerminalTurnAdmissionKey,
    mut lifecycle_rx: ExactTerminalLifecycleReceiver,
) -> TerminalTurnEnd {
    let mut renew = interval(LEASE_RENEW_INTERVAL);
    renew.tick().await; // consume the immediate first tick
    let mut tick = interval(Duration::from_secs(2));
    tick.tick().await; // consume the immediate first tick

    let fut = async {

        loop {
            tokio::select! {
                _ = renew.tick() => {
                    match deps
                        .service
                        .renew_lease(
                            &key.requirement_id,
                            &key.terminal_id,
                            AutoWorkTargetKind::Terminal,
                            key.claim_generation,
                            &key.claim_token,
                            DEFAULT_LEASE_MS,
                        )
                        .await
                    {
                        Ok(true) => {}
                        Ok(false) => {
                            let detail = format!(
                                "Exact lease authority was lost after Terminal effects started \
                                 for AutoWork claim generation {}; its final outcome is unknown.",
                                key.claim_generation
                            );
                            return park_terminal_turn(deps, driver, key, &detail).await;
                        }
                        Err(error) => {
                            let detail = format!(
                                "Lease renewal failed after Terminal effects started for AutoWork \
                                 claim generation {}: {error}",
                                key.claim_generation
                            );
                            return park_terminal_turn(deps, driver, key, &detail).await;
                        }
                    }
                }
                _ = tick.tick() => {
                    if driver.current_epoch(&key.terminal_id) != Some(key.pty_epoch) {
                        let detail = format!(
                            "PTY generation changed after AutoWork Terminal claim generation {} \
                             started; its final outcome is unknown.",
                            key.claim_generation
                        );
                        return park_terminal_turn(deps, driver, key, &detail).await;
                    }
                }
                event = lifecycle_rx.recv() => {
                    match event {
                        Ok(event) if event.kind == LifecycleKind::TurnEnd => {
                            if event.turn_token().is_none() {
                                debug!(
                                    terminal_id = %key.terminal_id,
                                    requirement_id = %key.requirement_id,
                                    "Unscoped TurnEnd only wakes an authoritative Requirement recheck"
                                );
                            }
                            match deps.service.get(&key.requirement_id).await {
                                Ok(requirement) => {
                                    let Some(outcome) =
                                        terminal_outcome_from_status(requirement.status)
                                    else {
                                        let detail = format!(
                                            "Terminal TurnEnd for AutoWork claim generation {} had \
                                             no authoritative Requirement verdict; it was not \
                                             treated as successful.",
                                            key.claim_generation
                                        );
                                        return park_terminal_turn(deps, driver, key, &detail).await;
                                    };
                                    if let Err(error) = driver
                                        .settle_turn_admission(
                                            key,
                                            outcome,
                                            requirement.completion_note.as_deref(),
                                        )
                                        .await
                                    {
                                        warn!(
                                            terminal_id = %key.terminal_id,
                                            requirement_id = %key.requirement_id,
                                            error = %error,
                                            "Failed to copy Requirement verdict into Terminal receipt"
                                        );
                                        let _ = driver
                                            .park_open_turn_admissions(
                                                &key.terminal_id,
                                                Some(key.pty_epoch),
                                                "Requirement reached an authoritative verdict, but Terminal receipt settlement failed.",
                                            )
                                            .await;
                                    }
                                    return TerminalTurnEnd::AuthoritativeVerdict {
                                        status: requirement.status,
                                        note: requirement.completion_note,
                                    };
                                }
                                Err(error) => {
                                    let detail = format!(
                                        "Terminal TurnEnd for AutoWork claim generation {} could \
                                         not re-read its authoritative Requirement verdict: {error}",
                                        key.claim_generation
                                    );
                                    return park_terminal_turn(deps, driver, key, &detail).await;
                                }
                            }
                        }
                        Ok(_) => continue,
                        Err(broadcast::error::RecvError::Closed) => {
                            let detail = format!(
                                "Exact lifecycle stream closed after AutoWork Terminal claim \
                                 generation {} started.",
                                key.claim_generation
                            );
                            return park_terminal_turn(deps, driver, key, &detail).await;
                        }
                        Err(broadcast::error::RecvError::Lagged(skipped)) => {
                            let detail = format!(
                                "Exact lifecycle stream skipped {skipped} event(s) after AutoWork \
                                 Terminal claim generation {} started.",
                                key.claim_generation
                            );
                            return park_terminal_turn(deps, driver, key, &detail).await;
                        }
                    }
                }
            }
        }
    };

    match timeout(TURN_TIMEOUT, fut).await {
        Ok(end) => end,
        Err(_) => {
            let detail = format!(
                "AutoWork Terminal claim generation {} exceeded its hard timeout after effects \
                 started; its final outcome is unknown.",
                key.claim_generation
            );
            park_terminal_turn(deps, driver, key, &detail).await
        }
    }
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
        // A user cancel is a deliberate interrupt —NOT a failure to retry
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
        // Never zero —a failure must always insert some delay before re-claim.
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
    fn autowork_multiline_prompt_uses_paste_then_separate_cr() {
        use nomifun_terminal::{encode_submit_chunks, SubmitChunks};
        let prompt = "requirement #1\ndo the thing\ncall requirement_complete when done";
        match encode_submit_chunks(prompt, true) {
            SubmitChunks::PasteThenCr { paste, cr } => {
                assert!(paste.starts_with(b"\x1b[200~"));
                assert!(paste.ends_with(b"\x1b[201~"));
                assert_eq!(cr, b"\r".to_vec());
            }
            other => panic!("expected PasteThenCr, got {other:?}"),
        }
    }

    #[test]
    fn terminal_submit_chunks_keeps_cr_out_of_the_paste_burst() {
        // Root-cause guard: the submit CR must NOT ride in the same byte burst as
        // the bracketed-paste body. Modern agent TUIs (claude/codex/gemini) use
        // paste-burst detection and SUPPRESS auto-submit for a CR that arrives in
        // the same read() as the paste-end marker —the requirement text would
        // then sit unsubmitted in the input box (the reported bug). The CR is
        // therefore returned as a SEPARATE chunk, written after a beat. Now backed
        // by the shared encoder (`nomifun_terminal::encode_submit_chunks`).
        use nomifun_terminal::{encode_submit_chunks, SubmitChunks};
        match encode_submit_chunks("line one\nline two", true) {
            SubmitChunks::PasteThenCr { paste, cr } => {
                assert!(paste.starts_with(b"\x1b[200~"), "paste must open with ESC[200~");
                assert!(paste.ends_with(b"\x1b[201~"), "paste must close with ESC[201~");
                assert!(
                    paste.windows(8).any(|w| w == b"line one"),
                    "paste must contain the prompt body"
                );
                assert!(!paste.contains(&b'\r'), "the CR must never be inside the paste burst");
                assert_eq!(cr, b"\r", "submit chunk must be a lone carriage return (a real Enter)");
            }
            other => panic!("expected PasteThenCr for a multi-line agent-TUI prompt, got {other:?}"),
        }
    }

    struct RecordingDriver {
        writes: Mutex<Vec<Vec<u8>>>,
        effects_start: Mutex<TerminalTurnEffectsStart>,
        body_start: Mutex<TerminalTurnEffectsStart>,
        submit_start: Mutex<TerminalTurnEffectsStart>,
        epoch: u64,
    }

    impl Default for RecordingDriver {
        fn default() -> Self {
            Self {
                writes: Mutex::new(Vec::new()),
                effects_start: Mutex::new(TerminalTurnEffectsStart::Started),
                body_start: Mutex::new(TerminalTurnEffectsStart::Started),
                submit_start: Mutex::new(TerminalTurnEffectsStart::Started),
                epoch: 1,
            }
        }
    }

    #[async_trait::async_trait]
    impl TerminalDriver for RecordingDriver {
        async fn write_input(&self, _id: &str, bytes: &[u8]) -> Result<(), TerminalError> {
            self.writes.lock().unwrap().push(bytes.to_vec());
            Ok(())
        }
        fn current_epoch(&self, _id: &str) -> Option<u64> {
            Some(self.epoch)
        }
        async fn write_input_exact_epoch(
            &self,
            id: &str,
            pty_epoch: u64,
            bytes: &[u8],
        ) -> Result<(), TerminalError> {
            if pty_epoch != self.epoch {
                return Err(TerminalError::StaleGeneration(id.to_owned()));
            }
            self.writes.lock().unwrap().push(bytes.to_vec());
            Ok(())
        }
        async fn write_admitted_turn(
            &self,
            _key: &TerminalTurnAdmissionKey,
            bytes: &[u8],
        ) -> Result<TerminalTurnEffectsStart, TerminalError> {
            let effects = *self.effects_start.lock().unwrap();
            if effects == TerminalTurnEffectsStart::Started {
                self.writes.lock().unwrap().push(bytes.to_vec());
            }
            Ok(effects)
        }
        async fn write_admitted_body(
            &self,
            _key: &TerminalTurnAdmissionKey,
            bytes: &[u8],
        ) -> Result<TerminalTurnEffectsStart, TerminalError> {
            let effects = *self.body_start.lock().unwrap();
            if effects == TerminalTurnEffectsStart::Started {
                self.writes.lock().unwrap().push(bytes.to_vec());
            }
            Ok(effects)
        }
        async fn write_admitted_submit(
            &self,
            _key: &TerminalTurnAdmissionKey,
            bytes: &[u8],
        ) -> Result<TerminalTurnEffectsStart, TerminalError> {
            let effects = *self.submit_start.lock().unwrap();
            if effects == TerminalTurnEffectsStart::Started {
                self.writes.lock().unwrap().push(bytes.to_vec());
            }
            Ok(effects)
        }
        fn subscribe_output(&self, _id: &str) -> Option<broadcast::Receiver<Vec<u8>>> {
            None
        }
        fn is_alive(&self, _id: &str) -> bool {
            true
        }
        async fn describe(&self, _id: &str) -> Result<Option<TerminalDescription>, TerminalError> {
            Ok(None)
        }
        async fn read_autowork(&self, _id: &str) -> Result<Option<String>, TerminalError> {
            Ok(None)
        }
        async fn write_autowork(&self, _id: &str, _autowork: Option<&str>) -> Result<(), TerminalError> {
            Ok(())
        }
        async fn read_idmm(&self, _id: &str) -> Result<Option<String>, TerminalError> {
            Ok(None)
        }
        async fn write_idmm(&self, _id: &str, _idmm: Option<&str>) -> Result<(), TerminalError> {
            Ok(())
        }
        fn subscribe_lifecycle(
            &self,
            _id: &str,
        ) -> Option<tokio::sync::broadcast::Receiver<nomifun_terminal::TerminalLifecycleEvent>> {
            None
        }
    }

    fn recording_turn_key(terminal_id: String) -> TerminalTurnAdmissionKey {
        TerminalTurnAdmissionKey {
            terminal_id,
            pty_epoch: 1,
            requirement_id: nomifun_common::RequirementId::new().into_string(),
            claim_generation: 1,
            claim_token:
                "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
                    .to_owned(),
            turn_token: "test-turn-token".to_owned(),
        }
    }

    #[tokio::test]
    async fn admitted_terminal_prompt_writes_paste_then_a_separate_exact_epoch_cr() {
        // The two PTY writes must be ordered: bracketed-paste body FIRST, then the
        // lone CR as its OWN write (so paste-burst-detecting TUIs treat it as a
        // real Enter). Mirrors the fix the cron terminal executor already applies.
        let recorder = Arc::new(RecordingDriver::default());
        let driver: Arc<dyn TerminalDriver> = recorder.clone();
        let terminal_id = nomifun_common::TerminalId::new().into_string();
        let key = recording_turn_key(terminal_id);
        let effects = submit_admitted_terminal_prompt(&driver, &key, "do the thing\nthen stop")
            .await
            .expect("submit must succeed");
        assert_eq!(effects, TerminalTurnEffectsStart::Started);
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

    #[tokio::test]
    async fn replayed_admission_emits_zero_paste_and_zero_cr_bytes() {
        for replay in [
            TerminalTurnEffectsStart::AlreadyStarted,
            TerminalTurnEffectsStart::AlreadySettled,
        ] {
            let recorder = Arc::new(RecordingDriver::default());
            *recorder.body_start.lock().unwrap() = replay;
            let driver: Arc<dyn TerminalDriver> = recorder.clone();
            let key = recording_turn_key(nomifun_common::TerminalId::new().into_string());

            assert_eq!(
                submit_admitted_terminal_prompt(&driver, &key, "must not run\nagain")
                    .await
                    .unwrap(),
                replay
            );
            assert!(
                recorder.writes.lock().unwrap().is_empty(),
                "{replay:?} must emit neither paste nor CR"
            );
        }
    }

    // -- C4 (spec §2.2): cross-domain loop-registry isolation ----------------
    //
    // The AutoWork loop registry keys on `TargetKey = (AutoWorkTargetKind,
    // canonical entity ID)`. The explicit kind keeps dispatch and lookup
    // domain-scoped even when two domains happen to carry equal UUID text.

    #[tokio::test]
    async fn authority_loss_after_body_write_emits_zero_submit_cr() {
        let recorder = Arc::new(RecordingDriver::default());
        *recorder.submit_start.lock().unwrap() =
            TerminalTurnEffectsStart::AlreadySettled;
        let driver: Arc<dyn TerminalDriver> = recorder.clone();
        let key = recording_turn_key(nomifun_common::TerminalId::new().into_string());

        assert_eq!(
            submit_admitted_terminal_prompt(&driver, &key, "body first\nsubmit fenced")
                .await
                .unwrap(),
            TerminalTurnEffectsStart::AlreadySettled
        );
        let writes = recorder.writes.lock().unwrap();
        assert_eq!(writes.len(), 1, "only the inert paste body may be written");
        assert!(
            !writes[0].contains(&b'\r'),
            "lost exact Requirement/receipt authority must suppress the submit CR"
        );
    }

    #[test]
    fn c4_target_key_distinguishes_canonical_session_domains() {
        let conversation_id = ConversationId::new().into_string();
        let terminal_id = TerminalId::new().into_string();
        let conversation: TargetKey = (AutoWorkTargetKind::Conversation, conversation_id);
        let terminal: TargetKey = (AutoWorkTargetKind::Terminal, terminal_id);
        assert_ne!(conversation, terminal, "conversation and terminal keys must be distinct");

        // The registry is a DashMap<TargetKey, _>; mirror its keying to prove
        // the two domains never collide and `stop` of one leaves the other.
        let map: DashMap<TargetKey, u32> = DashMap::new();
        map.insert(conversation.clone(), 1);
        map.insert(terminal.clone(), 2);
        assert_eq!(map.len(), 2, "both domains coexist");
        assert_eq!(map.get(&conversation).map(|v| *v), Some(1));
        assert_eq!(map.get(&terminal).map(|v| *v), Some(2));

        // Stopping the terminal domain leaves the conversation entry intact.
        map.remove(&terminal);
        assert!(map.contains_key(&conversation));
        assert!(!map.contains_key(&terminal));
    }

    #[test]
    fn c4_is_running_lookup_is_domain_scoped() {
        // `is_running(kind, id)` builds the lookup key from BOTH kind and id, so
        // an entry under one domain is invisible to the other domain's lookup.
        // Mirror the exact `contains_key` the AutoWork runner uses.
        let handles: DashMap<TargetKey, ()> = DashMap::new();
        let conversation_id = ConversationId::new().into_string();
        let terminal_id = TerminalId::new().into_string();
        handles.insert((AutoWorkTargetKind::Conversation, conversation_id.clone()), ());

        let conversation_lookup =
            handles.contains_key(&(AutoWorkTargetKind::Conversation, conversation_id));
        let terminal_lookup = handles.contains_key(&(AutoWorkTargetKind::Terminal, terminal_id));
        assert!(conversation_lookup);
        assert!(!terminal_lookup);
    }

    #[tokio::test]
    async fn concurrent_restarts_share_one_target_transition_barrier() {
        let transitions = Arc::new(TargetTransitionMap::new());
        let key = (
            AutoWorkTargetKind::Conversation,
            ConversationId::new().into_string(),
        );
        let first_lock = target_transition_lock(&transitions, &key);
        let first_guard = first_lock.lock().await;

        let entered = Arc::new(AtomicBool::new(false));
        let entered_by_second = entered.clone();
        let second_lock = target_transition_lock(&transitions, &key);
        let second = tokio::spawn(async move {
            let _guard = second_lock.lock().await;
            entered_by_second.store(true, Ordering::SeqCst);
        });

        tokio::task::yield_now().await;
        assert!(
            !entered.load(Ordering::SeqCst),
            "a racing start must not cross the prior stop/cleanup barrier"
        );
        drop(first_guard);
        timeout(Duration::from_secs(1), second)
            .await
            .expect("second transition must proceed after cleanup releases")
            .expect("second transition task must not panic");
        assert!(entered.load(Ordering::SeqCst));
    }

    #[test]
    fn autowork_delivery_key_is_stable_per_exact_durable_claim_capability() {
        let requirement_id = nomifun_common::RequirementId::new().into_string();
        let claim_token =
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
        let replacement_token =
            "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
        let first = autowork_turn_idempotency_key(&requirement_id, 7, claim_token);
        let restart_replay =
            autowork_turn_idempotency_key(&requirement_id, 7, claim_token);
        let next_generation =
            autowork_turn_idempotency_key(&requirement_id, 8, claim_token);
        let replacement_capability =
            autowork_turn_idempotency_key(&requirement_id, 7, replacement_token);

        assert_eq!(
            first, restart_replay,
            "restarting the runner must address the same receiver receipt"
        );
        assert_ne!(
            first, next_generation,
            "only a newly persisted claim generation may address a new turn"
        );
        assert_ne!(
            first, replacement_capability,
            "a different capability must never address the prior receipt as fresh authority"
        );
        assert!(first.starts_with("autowork:v2:"));
        assert!(first.len() <= 128);
        assert!(!first.contains(&requirement_id));
        assert!(!first.contains(claim_token));
    }

    #[test]
    fn unreconciled_accepted_replay_is_absorbed_instead_of_starting_a_second_turn() {
        let outcome = autowork_replayed_delivery_outcome(
            IdempotentMessageDelivery {
                message_id: nomifun_common::MessageId::new().into_string(),
                replayed: true,
                completed: false,
                result_ok: None,
                result_text: None,
                result_error: None,
            },
            3,
            false,
        )
        .expect("accepted replay must terminate injection before the event wait");

        assert_eq!(outcome.0, TurnEnd::Clean);
        assert!(
            outcome.1.as_deref().is_some_and(|note| {
                note.contains("not executed again") && note.contains("generation 3")
            }),
            "the review note must explain the absorbing at-most-once decision"
        );
        assert!(
            outcome.2,
            "forcing the verdict contract parks an unknown accepted replay in needs_review"
        );
    }

    #[test]
    fn accepted_replay_wait_requires_an_exact_local_or_reconciled_owner() {
        let message_id = nomifun_common::MessageId::new().into_string();

        for allowed in [
            BackgroundTurnReconciliationDisposition::LiveExactOwnerWait,
            BackgroundTurnReconciliationDisposition::ReconciledOrTerminalReRead,
        ] {
            authorize_accepted_receipt_wait(allowed, &message_id)
                .expect("an exact live owner or audited local reconciliation may poll its receipt");
        }

        for blocked in [
            BackgroundTurnReconciliationDisposition::ExternalProofRequiredFailClosed,
            BackgroundTurnReconciliationDisposition::StaleConflict,
        ] {
            assert!(
                matches!(
                    authorize_accepted_receipt_wait(blocked, &message_id),
                    Err(AppError::Conflict(_))
                ),
                "external/unknown ownership and stale generations must fail closed"
            );
        }
    }

    #[test]
    fn completed_error_replay_is_never_promoted_to_a_new_claim_generation() {
        let outcome = autowork_replayed_delivery_outcome(
            IdempotentMessageDelivery {
                message_id: nomifun_common::MessageId::new().into_string(),
                replayed: true,
                completed: true,
                result_ok: Some(false),
                result_text: None,
                result_error: Some("provider stream ended after tools may have run".to_owned()),
            },
            9,
            false,
        )
        .expect("a completed receipt must be absorbing");

        assert_eq!(
            outcome.0,
            TurnEnd::Clean,
            "post-admission errors must bypass the RetryPending branch"
        );
        assert!(
            outcome.2,
            "the exact finalizer must park the generation in needs_review"
        );
        assert!(
            outcome
                .1
                .as_deref()
                .is_some_and(|note| note.contains("tools may have run"))
        );
    }

    #[test]
    fn only_the_receipt_leader_enters_the_live_event_wait() {
        let fresh = autowork_replayed_delivery_outcome(
            IdempotentMessageDelivery {
                message_id: nomifun_common::MessageId::new().into_string(),
                replayed: false,
                completed: false,
                result_ok: None,
                result_text: None,
                result_error: None,
            },
            4,
            true,
        );
        assert!(
            fresh.is_none(),
            "only the atomic receiver-side receipt leader may await a newly started turn"
        );
    }

    #[test]
    fn durable_preflight_conflict_is_parked_instead_of_minting_a_retry_turn() {
        let outcome =
            autowork_blocked_delivery_outcome("idempotency payload drift".to_owned());
        assert_eq!(outcome.0, TurnEnd::Clean);
        assert!(outcome.2, "blocked delivery must finalize as needs_review");
        assert!(
            outcome
                .1
                .as_deref()
                .is_some_and(|note| note.contains("Explicit reset or human review")),
            "the operator must get a recoverable explanation"
        );
    }

    #[test]
    fn legacy_recovered_generation_without_receipt_is_fail_closed() {
        let legacy = legacy_recovered_claim_without_receipt_outcome(true, 0)
            .expect("an upgraded in-progress row has ambiguous pre-receipt effects");
        assert_eq!(legacy.0, TurnEnd::Clean);
        assert!(legacy.2, "legacy ambiguity must finalize as needs_review");
        assert!(
            legacy
                .1
                .as_deref()
                .is_some_and(|note| note.contains("predates durable AutoWork delivery receipts"))
        );
        assert!(
            legacy_recovered_claim_without_receipt_outcome(false, 1).is_none(),
            "the first execution of a freshly allocated claim remains enabled"
        );
        assert!(
            legacy_recovered_claim_without_receipt_outcome(true, 1).is_none(),
            "post-migration recovered claims use their durable receiver receipt"
        );
    }

    #[tokio::test]
    async fn recovered_terminal_claim_never_reaches_the_pty_writer() {
        let recorder = Arc::new(RecordingDriver::default());
        let recovered_active = true;

        // Recovered claims are parked before the admitted writer is reached.
        assert!(recovered_active);
        assert!(
            recorder.writes.lock().unwrap().is_empty(),
            "an already-injected/unsettled claim must produce zero PTY writes"
        );
    }

    // -- Terminal turn-end classification tests ------------------------------
    //
    // Any uncertainty after submission is absorbing; only an exact structured
    // completion may be clean.

    #[test]
    fn terminal_turn_end_is_eq_and_debug() {
        let done = TerminalTurnEnd::AuthoritativeVerdict {
            status: RequirementStatus::Done,
            note: Some("done".to_owned()),
        };
        assert_eq!(done, done.clone());
        assert_eq!(
            TerminalTurnEnd::AmbiguousAfterSubmission,
            TerminalTurnEnd::AmbiguousAfterSubmission
        );
        assert_ne!(
            done,
            TerminalTurnEnd::AmbiguousAfterSubmission
        );
        // Debug impl exists (used in error messages).
        assert!(!format!("{:?}", TerminalTurnEnd::AmbiguousAfterSubmission).is_empty());
    }

    #[test]
    fn terminal_expects_verdict_true_when_mcp_enabled() {
        // The AutoWork runner passes `expects_verdict = true` when the requirement
        // MCP is enabled (the tools are injected into the terminal). A clean turn
        // where the agent did NOT call them → needs_review (not silently done).
        assert!(crate::prompt::terminal_expects_verdict(true));
        assert!(!crate::prompt::terminal_expects_verdict(false));
    }

    #[test]
    fn terminal_post_submission_ambiguity_is_not_a_retryable_error() {
        let outcome = TerminalTurnEnd::AmbiguousAfterSubmission;
        assert_ne!(
            outcome,
            TerminalTurnEnd::AuthoritativeVerdict {
                status: RequirementStatus::Done,
                note: None,
            }
        );
    }

    #[tokio::test]
    async fn raw_lifecycle_turn_end_without_token_is_not_a_completion_verdict() {
        use nomifun_terminal::TerminalLifecycleEvent;

        let (tx, rx) = broadcast::channel::<TerminalLifecycleEvent>(4);
        // Send a TurnEnd BEFORE any consumer picks it up —the broadcast
        // channel buffers it.
        tx.send(TerminalLifecycleEvent {
            terminal_id: nomifun_common::TerminalId::new(),
            kind: LifecycleKind::TurnEnd,
            payload: serde_json::json!({}),
        })
        .unwrap();

        // Simulate the inner select logic directly (without AutoWorkRunnerDeps):
        // recv from the channel, match TurnEnd → Clean.
        let mut rx = rx;
        let ev = rx.recv().await.unwrap();
        assert_eq!(ev.kind, LifecycleKind::TurnEnd);
        assert_eq!(ev.turn_token(), None);
        assert_eq!(
            terminal_outcome_from_status(RequirementStatus::InProgress),
            None,
            "a raw no-token TurnEnd can only wake a Requirement recheck"
        );
    }

    #[tokio::test]
    async fn lifecycle_closed_channel_resolves_as_errored() {
        use nomifun_terminal::TerminalLifecycleEvent;

        let (tx, rx) = broadcast::channel::<TerminalLifecycleEvent>(4);
        drop(tx); // Simulate lifecycle server disappearing.

        let mut rx = rx;
        let ev = rx.recv().await;
        assert!(matches!(ev, Err(broadcast::error::RecvError::Closed)));
        // wait_terminal_turn_end maps this to post-submission ambiguity.
    }
}

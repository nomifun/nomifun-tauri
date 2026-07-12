use std::{
    sync::{
        Arc, Mutex, Weak,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

use crate::{
    CleanupReport, ExecutionError, ExecutionOutcome, ExecutionOwner, ExecutionPolicy,
    NormalizedExecutionRequest, OutputBuffer, OutputCursor, ProcessSnapshot, ProcessState,
    SessionId,
    io::FrozenOutput,
    platform::{ExitFact, ProcessOwner},
    registry::{
        CommitResult, LookupError, Registry, ReserveError, Retirement, SessionAction,
        StartReservation,
    },
};

const BACKGROUND_WAIT_HORIZON: Duration = Duration::from_secs(365 * 24 * 60 * 60);
const FINAL_OUTPUT_DRAIN: Duration = Duration::from_millis(120);
const MAX_INTERRUPT_GRACE: Duration = Duration::from_secs(1);
const MAX_TERMINATE_GRACE: Duration = Duration::from_secs(1);
const MAX_REAP_GRACE: Duration = Duration::from_secs(3);

#[derive(Clone, Debug)]
pub struct ExecutionHandle {
    pub owner: ExecutionOwner,
    pub session_id: SessionId,
    pub started_at: Instant,
}

pub struct SupervisorConfig {
    pub max_sessions: usize,
    pub reaper_interval: Duration,
}

impl Default for SupervisorConfig {
    fn default() -> Self {
        Self {
            max_sessions: 64,
            reaper_interval: Duration::from_secs(30),
        }
    }
}

#[derive(Debug)]
pub enum PollResult {
    Running {
        snapshot: ProcessSnapshot,
        output: crate::OutputSnapshot,
    },
    Finished(crate::ExecutionOutcome),
}

pub struct ProcessSupervisor {
    registry: Arc<Registry>,
    reaper_started: AtomicBool,
    reaper_stop: tokio_util::sync::CancellationToken,
    shutdown: Arc<ShutdownState>,
    reaper_interval: Duration,
}

struct ShutdownState {
    started: AtomicBool,
    report: tokio::sync::watch::Sender<Option<ShutdownReport>>,
}

pub(crate) struct Session {
    process: Arc<dyn ProcessOwner>,
    output: Arc<OutputBuffer>,
    policy: ExecutionPolicy,
    started_at: Instant,
    last_activity_at: Mutex<Instant>,
    state: Mutex<SessionState>,
    exit: tokio::sync::watch::Sender<Option<ExitObservation>>,
    lifecycle: tokio::sync::watch::Sender<u64>,
    #[cfg(test)]
    before_lost_commit: Mutex<Option<Arc<LostCommitHook>>>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
/// Outcomes for sessions that required cleanup after shutdown closed the start gate.
///
/// Sessions that had already completed naturally are omitted. A `Lost` outcome
/// whose cleanup report has `reaped == false` is an explicit unresolved
/// quarantine result; callers must retain it for reconciliation rather than
/// interpreting shutdown completion as proof that the OS process tree vanished.
pub struct ShutdownReport {
    pub sessions: Vec<ShutdownSessionReport>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
/// The exact owner, session id, and terminal cleanup outcome produced by shutdown.
pub struct ShutdownSessionReport {
    pub session_id: SessionId,
    pub owner: ExecutionOwner,
    pub outcome: ExecutionOutcome,
}

struct SessionState {
    process_state: ProcessState,
    exit_observation: Option<ExitObservation>,
    terminal: Option<TerminalRecord>,
}

#[derive(Clone)]
enum ExitObservation {
    Reaped {
        fact: ExitFact,
        observed_at: tokio::time::Instant,
    },
    WaitFailed { message: String },
}

#[derive(Clone)]
struct TerminalRecord {
    kind: TerminalKind,
    cleanup: CleanupReport,
}

#[derive(Clone)]
enum TerminalKind {
    Exited {
        fact: ExitFact,
        output: FrozenOutput,
    },
    Cancelled {
        output: FrozenOutput,
    },
    TimedOut {
        output: FrozenOutput,
    },
    Lost {
        last_known: ProcessSnapshot,
        output: FrozenOutput,
    },
}

enum StopStart {
    Leader,
    Follower,
    Terminal(TerminalRecord),
}

enum LostResolution {
    Installed(TerminalRecord),
    Existing(TerminalRecord),
    Reaped {
        fact: ExitFact,
        observed_at: tokio::time::Instant,
        cleanup: CleanupReport,
    },
}

#[derive(Clone, Copy)]
enum SignalStage {
    Interrupt,
    Terminate,
    ForceKill,
}

#[derive(Clone, Copy)]
enum StopCause {
    Cancelled,
    TimedOut,
}

struct StopBudget {
    started_at: tokio::time::Instant,
    stages: Vec<StageDeadline>,
    cleanup_deadline: tokio::time::Instant,
}

struct StageDeadline {
    stage: SignalStage,
    deadline: tokio::time::Instant,
}

#[cfg(test)]
struct LostCommitHook {
    reached: tokio::sync::Notify,
    release: tokio::sync::Notify,
}

impl ProcessSupervisor {
    pub fn new(config: SupervisorConfig) -> Arc<Self> {
        Arc::new(Self {
            registry: Arc::new(Registry::new(config.max_sessions)),
            reaper_started: AtomicBool::new(false),
            reaper_stop: tokio_util::sync::CancellationToken::new(),
            shutdown: Arc::new(ShutdownState {
                started: AtomicBool::new(false),
                report: tokio::sync::watch::channel(None).0,
            }),
            reaper_interval: config.reaper_interval,
        })
    }

    pub async fn start(
        self: &Arc<Self>,
        request: NormalizedExecutionRequest,
    ) -> Result<ExecutionHandle, ExecutionError> {
        self.ensure_reaper_started();
        let mut reservation = self.reserve_start_capacity().await?;
        let activity_registry = Arc::downgrade(&self.registry);
        let session_id = SessionId::new();
        let output = Arc::new(OutputBuffer::with_activity(
            request.policy.output_limit_bytes,
            Arc::new(move || {
                if let Some(registry) = activity_registry.upgrade() {
                    registry.touch_session(session_id, Instant::now());
                }
            }),
        ));
        let spawned = crate::platform::spawn(request.clone(), output.clone()).await?;
        self.register_reserved(
            request,
            spawned.owner,
            output,
            &mut reservation,
            session_id,
        )
        .await
    }

    #[cfg(test)]
    pub(crate) async fn register_owned(
        self: &Arc<Self>,
        request: NormalizedExecutionRequest,
        process: Arc<dyn ProcessOwner>,
        output: Arc<OutputBuffer>,
    ) -> Result<ExecutionHandle, ExecutionError> {
        self.ensure_reaper_started();
        let mut reservation = self.reserve_start_capacity().await?;
        self.register_reserved(
            request,
            process,
            output,
            &mut reservation,
            SessionId::new(),
        )
        .await
    }

    async fn register_reserved(
        self: &Arc<Self>,
        request: NormalizedExecutionRequest,
        process: Arc<dyn ProcessOwner>,
        output: Arc<OutputBuffer>,
        reservation: &mut StartReservation,
        session_id: SessionId,
    ) -> Result<ExecutionHandle, ExecutionError> {
        let started_at = Instant::now();
        let owner = request.owner;
        let policy = request.policy;
        let lease = policy.lease;
        let (exit, _exit_receiver) = tokio::sync::watch::channel(None);
        let (lifecycle, _lifecycle_receiver) = tokio::sync::watch::channel(0);
        let session = Arc::new(Session {
            process,
            output,
            policy,
            started_at,
            last_activity_at: Mutex::new(started_at),
            state: Mutex::new(SessionState {
                process_state: ProcessState::Running,
                exit_observation: None,
                terminal: None,
            }),
            exit,
            lifecycle,
            #[cfg(test)]
            before_lost_commit: Mutex::new(None),
        });
        let commit = self.registry.commit(
            reservation,
            session_id,
            owner.clone(),
            session.clone(),
            lease,
            started_at,
        );
        start_waiter(Arc::clone(&session));
        match commit {
            CommitResult::Active => {
                start_execution_deadline(session);
                Ok(ExecutionHandle {
                    owner,
                    session_id,
                    started_at,
                })
            }
            CommitResult::Retiring(retirement) => {
                self.start_retirement(retirement.clone());
                let _ = retirement.wait_outcome().await;
                Err(ExecutionError::SupervisorShuttingDown)
            }
        }
    }

    pub async fn status(
        &self,
        owner: &ExecutionOwner,
        session_id: &SessionId,
    ) -> Result<ProcessSnapshot, ExecutionError> {
        let action = self.session(owner, session_id)?;
        Ok(action.snapshot())
    }

    /// Return a completed outcome only after final output has been frozen,
    /// without renewing the session lease.
    ///
    /// Adapters use this for stale-binding cleanup. A natural exit that has
    /// merely been observed, but whose final output drain is still in flight,
    /// remains `None` so the caller cannot discard its last output.
    pub fn terminal_outcome_if_ready(
        &self,
        owner: &ExecutionOwner,
        session_id: &SessionId,
        cursor: OutputCursor,
    ) -> Result<Option<ExecutionOutcome>, ExecutionError> {
        let session = match self.registry.inspect(session_id, owner) {
            Ok(session) => session,
            Err(LookupError::NotFound) => {
                return Err(ExecutionError::SessionNotFound {
                    session_id: *session_id,
                });
            }
            Err(LookupError::OwnerMismatch) => {
                return Err(ExecutionError::OwnerMismatch {
                    session_id: *session_id,
                });
            }
        };
        Ok(session
            .terminal()
            .map(|terminal| session.outcome(&terminal, cursor)))
    }

    pub async fn poll(
        &self,
        owner: &ExecutionOwner,
        session_id: &SessionId,
        cursor: OutputCursor,
        yield_until: Instant,
    ) -> Result<PollResult, ExecutionError> {
        let action = self.session(owner, session_id)?;
        let session = action.session_arc();
        let mut exits = session.exit.subscribe();
        let mut lifecycle = session.lifecycle.subscribe();
        let yield_timer = tokio::time::sleep_until(tokio::time::Instant::from_std(yield_until));
        tokio::pin!(yield_timer);
        loop {
            if let Some(terminal) = session.terminal() {
                return Ok(PollResult::Finished(session.outcome(&terminal, cursor)));
            }
            if let Some(observation) = session.exit_observation() {
                return Ok(PollResult::Finished(
                    finish_observed_exit(&session, observation, cursor).await,
                ));
            }
            tokio::select! {
                changed = exits.changed() => {
                    changed.expect("execution exit watch closed while session is alive");
                }
                changed = lifecycle.changed() => {
                    changed.expect("execution lifecycle watch closed while session is alive");
                }
                () = &mut yield_timer => break,
            }
        }
        let snapshot = session.snapshot();
        let output = session.output.snapshot_from(cursor);
        if let Some(terminal) = session.terminal() {
            return Ok(PollResult::Finished(session.outcome(&terminal, cursor)));
        }
        if let Some(observation) = session.exit_observation() {
            return Ok(PollResult::Finished(
                finish_observed_exit(&session, observation, cursor).await,
            ));
        }
        Ok(PollResult::Running { snapshot, output })
    }

    /// Poll until terminal state, newly available output, or the yield
    /// deadline. Unlike [`Self::poll`], this is intended for incremental
    /// interactive adapters and may return `Running` as soon as output advances.
    pub async fn poll_until_activity(
        &self,
        owner: &ExecutionOwner,
        session_id: &SessionId,
        cursor: OutputCursor,
        yield_until: Instant,
    ) -> Result<PollResult, ExecutionError> {
        let action = self.session(owner, session_id)?;
        let session = action.session_arc();
        let mut exits = session.exit.subscribe();
        let mut lifecycle = session.lifecycle.subscribe();
        let mut output_changes = session.output.subscribe_changes();
        let yield_timer = tokio::time::sleep_until(tokio::time::Instant::from_std(yield_until));
        tokio::pin!(yield_timer);
        loop {
            if let Some(terminal) = session.terminal() {
                return Ok(PollResult::Finished(session.outcome(&terminal, cursor)));
            }
            if let Some(observation) = session.exit_observation() {
                return Ok(PollResult::Finished(
                    finish_observed_exit(&session, observation, cursor).await,
                ));
            }
            if session.output.snapshot_from(cursor).next_cursor > cursor {
                break;
            }
            tokio::select! {
                changed = exits.changed() => {
                    changed.expect("execution exit watch closed while session is alive");
                }
                changed = lifecycle.changed() => {
                    changed.expect("execution lifecycle watch closed while session is alive");
                }
                changed = output_changes.changed() => {
                    changed.expect("execution output watch closed while session is alive");
                }
                () = &mut yield_timer => break,
            }
        }
        let snapshot = session.snapshot();
        let output = session.output.snapshot_from(cursor);
        if let Some(terminal) = session.terminal() {
            return Ok(PollResult::Finished(session.outcome(&terminal, cursor)));
        }
        if let Some(observation) = session.exit_observation() {
            return Ok(PollResult::Finished(
                finish_observed_exit(&session, observation, cursor).await,
            ));
        }
        Ok(PollResult::Running { snapshot, output })
    }

    pub async fn write(
        &self,
        owner: &ExecutionOwner,
        session_id: &SessionId,
        bytes: &[u8],
    ) -> Result<(), ExecutionError> {
        let action = self.session(owner, session_id)?;
        action
            .process
            .write(bytes)
            .await
            .map_err(|error| owner_io_error("write stdin", error))?;
        Ok(())
    }

    pub async fn close_stdin(
        &self,
        owner: &ExecutionOwner,
        session_id: &SessionId,
    ) -> Result<(), ExecutionError> {
        let action = self.session(owner, session_id)?;
        action
            .process
            .close_stdin()
            .await
            .map_err(|error| owner_io_error("close stdin", error))?;
        Ok(())
    }

    pub async fn resize(
        &self,
        owner: &ExecutionOwner,
        session_id: &SessionId,
        cols: u16,
        rows: u16,
    ) -> Result<(), ExecutionError> {
        if cols == 0 || rows == 0 {
            return Err(ExecutionError::InvalidTransport {
                reason: "PTY dimensions must be non-zero".to_owned(),
            });
        }
        let action = self.session(owner, session_id)?;
        action
            .process
            .resize(cols, rows)
            .await
            .map_err(|error| owner_io_error("resize terminal", error))?;
        Ok(())
    }

    pub async fn interrupt(
        &self,
        owner: &ExecutionOwner,
        session_id: &SessionId,
    ) -> Result<(), ExecutionError> {
        let action = self.session(owner, session_id)?;
        action
            .process
            .interrupt()
            .await
            .map_err(|error| owner_io_error("interrupt process", error))?;
        Ok(())
    }

    pub async fn terminate(
        &self,
        owner: &ExecutionOwner,
        session_id: &SessionId,
    ) -> Result<ExecutionOutcome, ExecutionError> {
        let request_started_at = tokio::time::Instant::now();
        let action = self.session(owner, session_id)?;
        let session = action.session_arc();
        Ok(stop_session(
            session,
            &[SignalStage::Terminate, SignalStage::ForceKill],
            request_started_at,
            StopCause::Cancelled,
        )
        .await)
    }

    pub async fn cancel(
        &self,
        owner: &ExecutionOwner,
        session_id: &SessionId,
    ) -> Result<ExecutionOutcome, ExecutionError> {
        let request_started_at = tokio::time::Instant::now();
        let action = self.session(owner, session_id)?;
        let session = action.session_arc();
        Ok(stop_session(
            session,
            &[
                SignalStage::Interrupt,
                SignalStage::Terminate,
                SignalStage::ForceKill,
            ],
            request_started_at,
            StopCause::Cancelled,
        )
        .await)
    }

    /// Stop a session because its hard execution deadline elapsed. Unlike
    /// `poll`, this records a durable `TimedOut` terminal outcome.
    pub async fn timeout(
        &self,
        owner: &ExecutionOwner,
        session_id: &SessionId,
    ) -> Result<ExecutionOutcome, ExecutionError> {
        let request_started_at = tokio::time::Instant::now();
        let action = self.session(owner, session_id)?;
        let session = action.session_arc();
        Ok(stop_session(
            session,
            &[
                SignalStage::Interrupt,
                SignalStage::Terminate,
                SignalStage::ForceKill,
            ],
            request_started_at,
            StopCause::TimedOut,
        )
        .await)
    }

    pub fn heartbeat(&self, run_id: uuid::Uuid) -> usize {
        self.registry.heartbeat_run(run_id, Instant::now())
    }

    pub async fn shutdown(&self) -> ShutdownReport {
        if let Some(report) = self.shutdown.report.borrow().clone() {
            return report;
        }
        if self
            .shutdown
            .started
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
        {
            self.reaper_stop.cancel();
            self.registry.begin_shutdown();
            let registry = self.registry.clone();
            let shutdown = self.shutdown.clone();
            tokio::spawn(async move {
                run_shutdown(registry, shutdown).await;
            });
        }
        let mut reports = self.shutdown.report.subscribe();
        loop {
            if let Some(report) = reports.borrow_and_update().clone() {
                return report;
            }
            reports
                .changed()
                .await
                .expect("execution shutdown report sender unexpectedly closed");
        }
    }

    fn session(
        &self,
        owner: &ExecutionOwner,
        session_id: &SessionId,
    ) -> Result<SessionAction, ExecutionError> {
        match self
            .registry
            .begin_action(session_id, owner, Instant::now())
        {
            Ok(action) => Ok(action),
            Err(LookupError::NotFound) => {
                Err(ExecutionError::SessionNotFound {
                    session_id: *session_id,
                })
            }
            Err(LookupError::OwnerMismatch) => {
                Err(ExecutionError::OwnerMismatch {
                    session_id: *session_id,
                })
            }
        }
    }

    async fn reserve_start_capacity(
        self: &Arc<Self>,
    ) -> Result<StartReservation, ExecutionError> {
        loop {
            match self.registry.reserve_start() {
                Ok(reservation) => return Ok(reservation),
                Err(ReserveError::ShuttingDown) => {
                    return Err(ExecutionError::SupervisorShuttingDown);
                }
                Err(ReserveError::Capacity) => {
                    if self.registry.evict_oldest_finished() {
                        continue;
                    }
                    let mut retirements = self.registry.claim_expired(Instant::now());
                    retirements.extend(self.registry.pending_retirements());
                    for retirement in &retirements {
                        self.start_retirement(retirement.clone());
                    }
                    if let Some(retirement) = retirements.into_iter().next() {
                        let outcome = retirement.wait_outcome().await;
                        if outcome_reaped(&outcome) {
                            continue;
                        }
                        return Err(ExecutionError::CapacityExhausted {
                            max_sessions: self.registry.max_sessions(),
                        });
                    }
                    return Err(ExecutionError::CapacityExhausted {
                        max_sessions: self.registry.max_sessions(),
                    });
                }
            }
        }
    }

    fn ensure_reaper_started(self: &Arc<Self>) {
        if self
            .reaper_started
            .swap(true, Ordering::AcqRel)
        {
            return;
        }
        let weak = Arc::downgrade(self);
        let stop = self.reaper_stop.clone();
        let interval = self.reaper_interval.max(Duration::from_millis(1));
        tokio::spawn(async move {
            run_reaper(weak, stop, interval).await;
        });
    }

    fn start_retirement(&self, retirement: Arc<Retirement>) {
        start_retirement_driver(self.registry.clone(), retirement);
    }
}

async fn run_reaper(
    supervisor: Weak<ProcessSupervisor>,
    stop: tokio_util::sync::CancellationToken,
    interval: Duration,
) {
    let mut ticker = tokio::time::interval(interval);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    ticker.tick().await;
    loop {
        tokio::select! {
            () = stop.cancelled() => return,
            _ = ticker.tick() => {}
        }
        let Some(supervisor) = supervisor.upgrade() else {
            return;
        };
        let retirements = supervisor.registry.claim_expired(Instant::now());
        for retirement in retirements {
            supervisor.start_retirement(retirement);
        }
        drop(supervisor);
    }
}

fn start_retirement_driver(registry: Arc<Registry>, retirement: Arc<Retirement>) {
    if !retirement.try_start_driver() {
        return;
    }
    let worker_retirement = retirement.clone();
    let worker = tokio::spawn(async move {
        retire_session(worker_retirement.session()).await
    });
    tokio::spawn(async move {
        let outcome = match worker.await {
            Ok(outcome) => outcome,
            Err(error) => retirement
                .session()
                .force_lost(format!("retirement driver stopped unexpectedly: {error}")),
        };
        let proven_reaped = outcome_reaped(&outcome);
        registry.finish_retirement(&retirement, proven_reaped);
        retirement.complete(outcome);
    });
}

async fn retire_session(session: Arc<Session>) -> ExecutionOutcome {
    if let Some(terminal) = session.terminal() {
        return session.outcome(&terminal, OutputCursor::START);
    }
    stop_session(
        session,
        &[
            SignalStage::Interrupt,
            SignalStage::Terminate,
            SignalStage::ForceKill,
        ],
        tokio::time::Instant::now(),
        StopCause::Cancelled,
    )
    .await
}

fn outcome_reaped(outcome: &ExecutionOutcome) -> bool {
    match outcome {
        ExecutionOutcome::Exited { cleanup, .. }
        | ExecutionOutcome::Cancelled { cleanup, .. }
        | ExecutionOutcome::TimedOut { cleanup, .. }
        | ExecutionOutcome::Lost { cleanup, .. } => cleanup.reaped,
        ExecutionOutcome::SpawnFailed(_) => true,
    }
}

async fn run_shutdown(registry: Arc<Registry>, shutdown: Arc<ShutdownState>) {
    let mut changes = registry.subscribe_changes();
    loop {
        let snapshot = registry.shutdown_snapshot();
        for retirement in &snapshot.retirements {
            start_retirement_driver(registry.clone(), retirement.clone());
        }
        if snapshot.reservations == 0 {
            for retirement in &snapshot.retirements {
                let _ = retirement.wait_outcome().await;
            }
            let final_snapshot = registry.shutdown_snapshot();
            let same_retirements = snapshot.retirements.len() == final_snapshot.retirements.len()
                && snapshot.retirements.iter().all(|retirement| {
                    final_snapshot
                        .retirements
                        .iter()
                        .any(|candidate| candidate.id() == retirement.id())
                });
            if final_snapshot.reservations == 0 && same_retirements {
                let mut sessions = final_snapshot
                    .retirements
                    .iter()
                    .filter_map(|retirement| {
                        let outcome = retirement
                            .outcome()
                            .expect("awaited retirement must have a terminal outcome");
                        matches!(
                            outcome,
                            ExecutionOutcome::Cancelled { .. } | ExecutionOutcome::Lost { .. }
                        )
                        .then(|| ShutdownSessionReport {
                            session_id: retirement.id(),
                            owner: retirement.owner().clone(),
                            outcome,
                        })
                    })
                    .collect::<Vec<_>>();
                sessions.sort_by_key(|session| session.session_id);
                registry.complete_shutdown();
                shutdown.report.send_replace(Some(ShutdownReport { sessions }));
                return;
            }
            continue;
        }
        changes
            .changed()
            .await
            .expect("execution registry change sender unexpectedly closed");
    }
}

impl Session {
    pub(crate) fn is_reclaimable_terminal(&self) -> bool {
        self.terminal()
            .is_some_and(|terminal| terminal.cleanup.reaped)
    }

    pub(crate) fn touch_at(&self, now: Instant) {
        *self
            .last_activity_at
            .lock()
            .expect("execution session activity lock is poisoned") = now;
    }

    fn snapshot(&self) -> ProcessSnapshot {
        let process_state = {
            let state = self
                .state
                .lock()
                .expect("execution session state lock is poisoned");
            if state.process_state == ProcessState::Running {
                match &state.exit_observation {
                    Some(ExitObservation::Reaped { .. }) => ProcessState::Exited,
                    Some(ExitObservation::WaitFailed { .. }) => ProcessState::Lost,
                    None => ProcessState::Running,
                }
            } else {
                state.process_state
            }
        };
        ProcessSnapshot {
            pid: self.process.pid(),
            state: process_state,
            started_at: self.started_at,
            last_activity_at: *self
                .last_activity_at
                .lock()
                .expect("execution session activity lock is poisoned"),
        }
    }

    fn exit_observation(&self) -> Option<ExitObservation> {
        self.state
            .lock()
            .expect("execution session state lock is poisoned")
            .exit_observation
            .clone()
    }

    fn publish_exit_observation(&self, observation: ExitObservation) {
        let published = {
            let mut state = self
                .state
                .lock()
                .expect("execution session state lock is poisoned");
            if state.exit_observation.is_some() {
                false
            } else {
                state.exit_observation = Some(observation.clone());
                true
            }
        };
        if published {
            self.exit.send_replace(Some(observation));
        }
    }

    async fn wait_for_exit(&self) -> ExitObservation {
        let mut exits = self.exit.subscribe();
        loop {
            if let Some(observation) = self.exit_observation() {
                return observation;
            }
            exits
                .changed()
                .await
                .expect("execution exit watch closed while session is alive");
        }
    }

    fn terminal(&self) -> Option<TerminalRecord> {
        self.state
            .lock()
            .expect("execution session state lock is poisoned")
            .terminal
            .clone()
    }

    fn set_terminal(&self, record: TerminalRecord) -> TerminalRecord {
        let record = {
            let mut state = self
                .state
                .lock()
                .expect("execution session state lock is poisoned");
            if let Some(existing) = &state.terminal {
                return existing.clone();
            }
            state.process_state = match &record.kind {
                TerminalKind::Exited { .. } => ProcessState::Exited,
                TerminalKind::Cancelled { .. } => ProcessState::Cancelled,
                TerminalKind::TimedOut { .. } => ProcessState::TimedOut,
                TerminalKind::Lost { .. } => ProcessState::Lost,
            };
            state.terminal = Some(record.clone());
            record
        };
        self.notify_lifecycle();
        record
    }

    fn resolve_lost_or_reaped(
        &self,
        mut cleanup: CleanupReport,
        cleanup_started_at: tokio::time::Instant,
        error: &str,
    ) -> LostResolution {
        let last_activity_at = *self
            .last_activity_at
            .lock()
            .expect("execution session activity lock is poisoned");
        let mut state = self
            .state
            .lock()
            .expect("execution session state lock is poisoned");
        if let Some(existing) = &state.terminal {
            return LostResolution::Existing(existing.clone());
        }
        if let Some(ExitObservation::Reaped { fact, observed_at }) = &state.exit_observation {
            return LostResolution::Reaped {
                fact: fact.clone(),
                observed_at: *observed_at,
                cleanup,
            };
        }
        if let Some(ExitObservation::WaitFailed { message }) = &state.exit_observation {
            let wait_error = format!("wait_reaped: {message}");
            if !cleanup.errors.iter().any(|existing| existing == &wait_error) {
                cleanup.errors.push(wait_error);
            }
        }
        cleanup.errors.push(error.to_owned());
        cleanup.elapsed = cleanup_started_at.elapsed();
        let record = TerminalRecord {
            kind: TerminalKind::Lost {
                last_known: ProcessSnapshot {
                    pid: self.process.pid(),
                    state: ProcessState::Lost,
                    started_at: self.started_at,
                    last_activity_at,
                },
                output: self.output.freeze(),
            },
            cleanup,
        };
        state.process_state = ProcessState::Lost;
        state.terminal = Some(record.clone());
        drop(state);
        self.notify_lifecycle();
        LostResolution::Installed(record)
    }

    fn commit_known_lost(
        &self,
        mut cleanup: CleanupReport,
        cleanup_started_at: tokio::time::Instant,
        error: &str,
    ) -> TerminalRecord {
        cleanup.errors.push(error.to_owned());
        cleanup.elapsed = cleanup_started_at.elapsed();
        let record = TerminalRecord {
            kind: TerminalKind::Lost {
                last_known: lost_snapshot(self),
                output: self.output.freeze(),
            },
            cleanup,
        };
        self.set_terminal(record)
    }

    fn force_lost(&self, error: String) -> ExecutionOutcome {
        let terminal = self.set_terminal(TerminalRecord {
            kind: TerminalKind::Lost {
                last_known: lost_snapshot(self),
                output: self.output.freeze(),
            },
            cleanup: CleanupReport {
                errors: vec![error],
                ..CleanupReport::default()
            },
        });
        self.outcome(&terminal, OutputCursor::START)
    }

    fn outcome(&self, terminal: &TerminalRecord, cursor: OutputCursor) -> ExecutionOutcome {
        match &terminal.kind {
            TerminalKind::Exited { fact, output } => ExecutionOutcome::Exited {
                code: fact.code,
                signal: fact.signal,
                output: output.snapshot_from(cursor),
                cleanup: terminal.cleanup.clone(),
            },
            TerminalKind::Cancelled { output } => ExecutionOutcome::Cancelled {
                output: output.snapshot_from(cursor),
                cleanup: terminal.cleanup.clone(),
            },
            TerminalKind::TimedOut { output } => ExecutionOutcome::TimedOut {
                output: output.snapshot_from(cursor),
                cleanup: terminal.cleanup.clone(),
            },
            TerminalKind::Lost { last_known, output } => ExecutionOutcome::Lost {
                last_known: last_known.clone(),
                output: output.snapshot_from(cursor),
                cleanup: terminal.cleanup.clone(),
            },
        }
    }

    fn begin_stop(&self) -> StopStart {
        let start = {
            let mut state = self
                .state
                .lock()
                .expect("execution session state lock is poisoned");
            if let Some(terminal) = &state.terminal {
                return StopStart::Terminal(terminal.clone());
            }
            if state.process_state == ProcessState::Cancelling {
                StopStart::Follower
            } else {
                state.process_state = ProcessState::Cancelling;
                StopStart::Leader
            }
        };
        self.notify_lifecycle();
        start
    }

    async fn wait_for_terminal(&self) -> TerminalRecord {
        let mut lifecycle = self.lifecycle.subscribe();
        loop {
            if let Some(terminal) = self.terminal() {
                return terminal;
            }
            lifecycle
                .changed()
                .await
                .expect("execution lifecycle watch closed while session is alive");
        }
    }

    async fn wait_for_exit_until(
        &self,
        deadline: tokio::time::Instant,
    ) -> Option<ExitObservation> {
        if let Some(observation) = self.exit_observation() {
            return Some(observation);
        }
        let exit = self.wait_for_exit();
        tokio::pin!(exit);
        let timer = tokio::time::sleep_until(deadline);
        tokio::pin!(timer);
        tokio::select! {
            observation = &mut exit => Some(observation),
            () = &mut timer => None,
        }
    }

    fn try_set_natural_terminal(&self, record: TerminalRecord) -> Option<TerminalRecord> {
        let terminal = {
            let mut state = self
                .state
                .lock()
                .expect("execution session state lock is poisoned");
            if let Some(existing) = &state.terminal {
                return Some(existing.clone());
            }
            if state.process_state == ProcessState::Cancelling {
                return None;
            }
            state.process_state = match &record.kind {
                TerminalKind::Exited { .. } => ProcessState::Exited,
                TerminalKind::Cancelled { .. } => ProcessState::Cancelled,
                TerminalKind::TimedOut { .. } => ProcessState::TimedOut,
                TerminalKind::Lost { .. } => ProcessState::Lost,
            };
            state.terminal = Some(record.clone());
            record
        };
        self.notify_lifecycle();
        Some(terminal)
    }

    fn notify_lifecycle(&self) {
        self.lifecycle
            .send_modify(|version| *version = version.wrapping_add(1));
    }
}

fn start_waiter(session: Arc<Session>) {
    start_waiter_attempt(session, 0);
}

fn start_execution_deadline(session: Arc<Session>) {
    let Some(deadline) = session.policy.deadline else {
        return;
    };
    let mut lifecycle = session.lifecycle.subscribe();
    let session = Arc::downgrade(&session);
    tokio::spawn(async move {
        let deadline = tokio::time::Instant::from_std(deadline);
        let timer = tokio::time::sleep_until(deadline);
        tokio::pin!(timer);
        loop {
            tokio::select! {
                () = &mut timer => break,
                changed = lifecycle.changed() => {
                    if changed.is_err() {
                        return;
                    }
                    let Some(active) = session.upgrade() else {
                        return;
                    };
                    if active.terminal().is_some() {
                        return;
                    }
                }
            }
        }
        let Some(session) = session.upgrade() else {
            return;
        };
        let _ = stop_session(
            session,
            &[
                SignalStage::Interrupt,
                SignalStage::Terminate,
                SignalStage::ForceKill,
            ],
            tokio::time::Instant::now(),
            StopCause::TimedOut,
        )
        .await;
    });
}

fn completed_process_kind(
    session: &Session,
    fact: ExitFact,
    observed_at: tokio::time::Instant,
    output: FrozenOutput,
) -> TerminalKind {
    if session
        .policy
        .deadline
        .is_some_and(|deadline| observed_at >= tokio::time::Instant::from_std(deadline))
    {
        TerminalKind::TimedOut { output }
    } else {
        TerminalKind::Exited { fact, output }
    }
}

fn start_waiter_attempt(session: Arc<Session>, attempt: usize) {
    const MAX_WAITER_RESTARTS: usize = 1;
    let worker_session = session.clone();
    let worker = tokio::spawn(async move {
        let deadline = Instant::now()
            .checked_add(BACKGROUND_WAIT_HORIZON)
            .unwrap_or_else(Instant::now);
        worker_session.process.wait_reaped(deadline).await
    });
    tokio::spawn(async move {
        let observation = match worker.await {
            Ok(Ok(fact)) => ExitObservation::Reaped {
                fact,
                observed_at: tokio::time::Instant::now(),
            },
            Ok(Err(error)) => ExitObservation::WaitFailed {
                message: error.to_string(),
            },
            Err(_error) if attempt < MAX_WAITER_RESTARTS => {
                start_waiter_attempt(session, attempt + 1);
                return;
            }
            Err(error) => ExitObservation::WaitFailed {
                message: format!(
                    "waiter task stopped unexpectedly after {} attempts: {error}",
                    attempt + 1
                ),
            },
        };
        session.publish_exit_observation(observation.clone());
        let terminal = match observation {
            ExitObservation::Reaped { fact, observed_at } => {
                tokio::time::sleep_until(observed_at + FINAL_OUTPUT_DRAIN).await;
                TerminalRecord {
                    kind: completed_process_kind(
                        &session,
                        fact.clone(),
                        observed_at,
                        session.output.freeze(),
                    ),
                    cleanup: CleanupReport {
                        reaped: true,
                        errors: fact.cleanup_errors,
                        ..CleanupReport::default()
                    },
                }
            }
            ExitObservation::WaitFailed { message } => TerminalRecord {
                kind: TerminalKind::Lost {
                    last_known: lost_snapshot(&session),
                    output: session.output.freeze(),
                },
                cleanup: CleanupReport {
                    errors: vec![format!("wait_reaped: {message}")],
                    ..CleanupReport::default()
                },
            },
        };
        let _ = session.try_set_natural_terminal(terminal);
    });
}

async fn finish_observed_exit(
    session: &Session,
    observation: ExitObservation,
    cursor: OutputCursor,
) -> ExecutionOutcome {
    let terminal = match observation {
        ExitObservation::Reaped { fact, observed_at } => {
            tokio::time::sleep_until(observed_at + FINAL_OUTPUT_DRAIN).await;
            let natural = TerminalRecord {
                kind: completed_process_kind(
                    session,
                    fact.clone(),
                    observed_at,
                    session.output.freeze(),
                ),
                cleanup: CleanupReport {
                    reaped: true,
                    errors: fact.cleanup_errors,
                    ..CleanupReport::default()
                },
            };
            match session.try_set_natural_terminal(natural) {
                Some(terminal) => terminal,
                None => session.wait_for_terminal().await,
            }
        }
        ExitObservation::WaitFailed { message } => {
            let failed = TerminalRecord {
                kind: TerminalKind::Lost {
                    last_known: lost_snapshot(session),
                    output: session.output.freeze(),
                },
                cleanup: CleanupReport {
                    errors: vec![format!("wait_reaped: {message}")],
                    ..CleanupReport::default()
                },
            };
            match session.try_set_natural_terminal(failed) {
                Some(terminal) => terminal,
                None => session.wait_for_terminal().await,
            }
        }
    };
    session.outcome(&terminal, cursor)
}

async fn stop_session(
    session: Arc<Session>,
    stages: &[SignalStage],
    request_started_at: tokio::time::Instant,
    cause: StopCause,
) -> ExecutionOutcome {
    if let Some(observation @ ExitObservation::Reaped { .. }) = session.exit_observation() {
        return finish_observed_exit(&session, observation, OutputCursor::START).await;
    }
    match session.begin_stop() {
        StopStart::Terminal(terminal) => session.outcome(&terminal, OutputCursor::START),
        StopStart::Follower => {
            let terminal = session.wait_for_terminal().await;
            session.outcome(&terminal, OutputCursor::START)
        }
        StopStart::Leader => {
            let driver_session = session.clone();
            let budget = StopBudget::new(request_started_at, stages, &session.policy);
            let monitor_session = session.clone();
            tokio::spawn(async move {
                let worker = tokio::spawn(async move {
                    let _ = run_stop_escalation(&driver_session, &budget, cause).await;
                });
                if let Err(error) = worker.await {
                    monitor_session.force_lost(format!(
                        "cleanup driver stopped unexpectedly: {error}"
                    ));
                }
            });
            let terminal = session.wait_for_terminal().await;
            session.outcome(&terminal, OutputCursor::START)
        }
    }
}

async fn run_stop_escalation(
    session: &Session,
    budget: &StopBudget,
    cause: StopCause,
) -> ExecutionOutcome {
    let mut cleanup = CleanupReport::default();
    let mut wait_error_recorded = false;

    if let Some(observation) = session.exit_observation() {
        match observation {
            ExitObservation::Reaped { fact, observed_at } => {
                return finish_reaped(session, fact, observed_at, cleanup, budget, cause).await;
            }
            ExitObservation::WaitFailed { message } => {
                record_wait_error(&mut cleanup, &mut wait_error_recorded, &message);
            }
        }
    }
    if tokio::time::Instant::now() > budget.cleanup_deadline {
        return finish_lost(
            session,
            cleanup,
            budget,
            cause,
            "cleanup driver resumed after cleanup deadline without timely reap proof",
        )
        .await;
    }

    for stage_deadline in &budget.stages {
        if let Some(observation) = session.exit_observation() {
            match observation {
                ExitObservation::Reaped { fact, observed_at } => {
                    return finish_reaped(
                        session,
                        fact,
                        observed_at,
                        cleanup,
                        budget,
                        cause,
                    )
                    .await;
                }
                ExitObservation::WaitFailed { message } => {
                    record_wait_error(&mut cleanup, &mut wait_error_recorded, &message);
                }
            }
        }

        let stage = stage_deadline.stage;
        if tokio::time::Instant::now() > stage_deadline.deadline {
            cleanup.errors.push(format!(
                "{}: stage deadline elapsed before signal attempt",
                stage.label()
            ));
            continue;
        }
        stage.mark_attempted(&mut cleanup);
        match tokio::time::timeout_at(
            stage_deadline.deadline,
            stage.send(session.process.as_ref()),
        )
        .await
        {
            Ok(Ok(())) => {}
            Ok(Err(error)) => cleanup
                .errors
                .push(format!("{}: {error}", stage.label())),
            Err(_) => cleanup
                .errors
                .push(format!("{}: timed out", stage.label())),
        }
        tokio::task::yield_now().await;

        if let Some(observation) = session.wait_for_exit_until(stage_deadline.deadline).await {
            match observation {
                ExitObservation::Reaped { fact, observed_at } => {
                    return finish_reaped(
                        session,
                        fact,
                        observed_at,
                        cleanup,
                        budget,
                        cause,
                    )
                    .await;
                }
                ExitObservation::WaitFailed { message } => {
                    record_wait_error(&mut cleanup, &mut wait_error_recorded, &message);
                }
            }
        }
    }

    if let Some(ExitObservation::Reaped { fact, observed_at }) = session.exit_observation() {
        return finish_reaped(session, fact, observed_at, cleanup, budget, cause).await;
    }
    finish_lost(
        session,
        cleanup,
        budget,
        cause,
        "process was not proven reaped before cleanup deadline",
    )
    .await
}

async fn finish_reaped(
    session: &Session,
    fact: ExitFact,
    observed_at: tokio::time::Instant,
    mut cleanup: CleanupReport,
    budget: &StopBudget,
    cause: StopCause,
) -> ExecutionOutcome {
    cleanup.errors.extend(fact.cleanup_errors);
    if observed_at > budget.cleanup_deadline {
        cleanup.reaped = true;
        let terminal = session.commit_known_lost(
            cleanup,
            budget.started_at,
            "reap was observed after cleanup deadline",
        );
        return session.outcome(&terminal, OutputCursor::START);
    }
    let drain_until = (observed_at + FINAL_OUTPUT_DRAIN).min(budget.cleanup_deadline);
    tokio::time::sleep_until(drain_until).await;
    cleanup.reaped = true;
    cleanup.elapsed = budget.started_at.elapsed();
    let output = session.output.freeze();
    let terminal = session.set_terminal(TerminalRecord {
        kind: match cause {
            StopCause::Cancelled => TerminalKind::Cancelled { output },
            StopCause::TimedOut => TerminalKind::TimedOut { output },
        },
        cleanup,
    });
    session.outcome(&terminal, OutputCursor::START)
}

async fn finish_lost(
    session: &Session,
    cleanup: CleanupReport,
    budget: &StopBudget,
    cause: StopCause,
    error: &str,
) -> ExecutionOutcome {
    #[cfg(test)]
    {
        let hook = session
            .before_lost_commit
            .lock()
            .expect("lost commit hook lock should not be poisoned")
            .clone();
        if let Some(hook) = hook {
            hook.reached.notify_one();
            hook.release.notified().await;
        }
    }
    match session.resolve_lost_or_reaped(cleanup, budget.started_at, error) {
        LostResolution::Installed(terminal) | LostResolution::Existing(terminal) => {
            session.outcome(&terminal, OutputCursor::START)
        }
        LostResolution::Reaped {
            fact,
            observed_at,
            cleanup,
        } => finish_reaped(session, fact, observed_at, cleanup, budget, cause).await,
    }
}

fn lost_snapshot(session: &Session) -> ProcessSnapshot {
    let mut last_known = session.snapshot();
    last_known.state = ProcessState::Lost;
    last_known
}

fn record_wait_error(cleanup: &mut CleanupReport, recorded: &mut bool, message: &str) {
    if !*recorded {
        cleanup.errors.push(format!("wait_reaped: {message}"));
        *recorded = true;
    }
}

impl StopBudget {
    fn new(
        started_at: tokio::time::Instant,
        stages: &[SignalStage],
        policy: &ExecutionPolicy,
    ) -> Self {
        let mut deadline = started_at;
        let stages = stages
            .iter()
            .copied()
            .map(|stage| {
                deadline += stage.grace(policy);
                StageDeadline { stage, deadline }
            })
            .collect();
        Self {
            started_at,
            stages,
            cleanup_deadline: deadline,
        }
    }
}

impl SignalStage {
    fn label(self) -> &'static str {
        match self {
            Self::Interrupt => "interrupt",
            Self::Terminate => "terminate",
            Self::ForceKill => "force_kill",
        }
    }

    fn grace(self, policy: &ExecutionPolicy) -> Duration {
        match self {
            Self::Interrupt => policy.interrupt_grace.min(MAX_INTERRUPT_GRACE),
            Self::Terminate => policy.terminate_grace.min(MAX_TERMINATE_GRACE),
            Self::ForceKill => policy.reap_grace.min(MAX_REAP_GRACE),
        }
    }

    fn mark_attempted(self, cleanup: &mut CleanupReport) {
        match self {
            Self::Interrupt => cleanup.interrupt_attempted = true,
            Self::Terminate => cleanup.terminate_attempted = true,
            Self::ForceKill => cleanup.force_kill_attempted = true,
        }
    }

    async fn send(self, owner: &dyn ProcessOwner) -> std::io::Result<()> {
        match self {
            Self::Interrupt => owner.interrupt().await,
            Self::Terminate => owner.terminate().await,
            Self::ForceKill => owner.force_kill().await,
        }
    }
}

fn owner_io_error(action: &'static str, error: std::io::Error) -> ExecutionError {
    ExecutionError::Io {
        operation: action,
        reason: error.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use std::{
        collections::BTreeMap,
        future::{self, Future},
        io,
        sync::{
            Arc, Mutex,
            atomic::{AtomicBool, AtomicUsize, Ordering},
        },
        time::{Duration, Instant},
    };

    use async_trait::async_trait;

    use super::{
        CommitResult, ExitObservation, ProcessSupervisor, Session, SessionState,
        SupervisorConfig, outcome_reaped, start_waiter,
    };
    use crate::{
        CapabilityPolicy, CommandSpec, ExecutionOwner, ExecutionPolicy,
        ExecutionOutcome, NormalizedExecutionRequest, OutputBuffer, OutputCursor, OutputSnapshot,
        OutputStream, PollResult, ProcessState, SandboxPolicy, SessionId, Transport,
        platform::{ExitFact, ProcessOwner},
    };

    #[derive(Clone)]
    enum ReapPlan {
        Pending,
        Natural {
            after: Duration,
            fact: ExitFact,
        },
        OnSignal {
            signal: FakeSignal,
            fact: ExitFact,
            after: Duration,
        },
        Controlled {
            fact: ExitFact,
        },
    }

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    enum FakeSignal {
        Interrupt,
        Terminate,
        ForceKill,
    }

    #[derive(Clone)]
    struct FakeOwner {
        wait_calls: Arc<AtomicUsize>,
        writes: Arc<Mutex<Vec<Vec<u8>>>>,
        close_calls: Arc<AtomicUsize>,
        resizes: Arc<Mutex<Vec<(u16, u16)>>>,
        signal_calls: Arc<Mutex<Vec<&'static str>>>,
        signal_times: Arc<Mutex<Vec<(&'static str, tokio::time::Instant)>>>,
        resize_error: Arc<AtomicBool>,
        block_write: Arc<AtomicBool>,
        write_entered: Arc<AtomicBool>,
        write_release: Arc<tokio::sync::Notify>,
        reap_release: Arc<tokio::sync::Notify>,
        reap_plan: ReapPlan,
        exit_tx: tokio::sync::watch::Sender<Option<ExitFact>>,
        signal_errors: Arc<Mutex<Vec<FakeSignal>>>,
        panic_signal: Arc<Mutex<Option<FakeSignal>>>,
        panic_waits_remaining: Arc<AtomicUsize>,
    }

    impl FakeOwner {
        fn pending() -> Self {
            let (exit_tx, _exit_rx) = tokio::sync::watch::channel(None);
            Self {
                wait_calls: Arc::new(AtomicUsize::new(0)),
                writes: Arc::new(Mutex::new(Vec::new())),
                close_calls: Arc::new(AtomicUsize::new(0)),
                resizes: Arc::new(Mutex::new(Vec::new())),
                signal_calls: Arc::new(Mutex::new(Vec::new())),
                signal_times: Arc::new(Mutex::new(Vec::new())),
                resize_error: Arc::new(AtomicBool::new(false)),
                block_write: Arc::new(AtomicBool::new(false)),
                write_entered: Arc::new(AtomicBool::new(false)),
                write_release: Arc::new(tokio::sync::Notify::new()),
                reap_release: Arc::new(tokio::sync::Notify::new()),
                reap_plan: ReapPlan::Pending,
                exit_tx,
                signal_errors: Arc::new(Mutex::new(Vec::new())),
                panic_signal: Arc::new(Mutex::new(None)),
                panic_waits_remaining: Arc::new(AtomicUsize::new(0)),
            }
        }

        fn exits_after(after: Duration, code: i32) -> Self {
            let mut fake = Self::pending();
            fake.reap_plan = ReapPlan::Natural {
                after,
                fact: ExitFact {
                    code: Some(code),
                    signal: None,
                    cleanup_errors: Vec::new(),
                },
            };
            fake
        }

        fn exits_with_cleanup_error(message: &str) -> Self {
            let mut fake = Self::pending();
            fake.reap_plan = ReapPlan::Natural {
                after: Duration::ZERO,
                fact: ExitFact {
                    code: Some(0),
                    signal: None,
                    cleanup_errors: vec![message.to_owned()],
                },
            };
            fake
        }

        fn reaps_on(signal: FakeSignal, code: i32) -> Self {
            let mut fake = Self::pending();
            fake.reap_plan = ReapPlan::OnSignal {
                signal,
                fact: ExitFact {
                    code: Some(code),
                    signal: None,
                    cleanup_errors: Vec::new(),
                },
                after: Duration::ZERO,
            };
            fake
        }

        fn controlled_reap(code: i32) -> Self {
            let mut fake = Self::pending();
            fake.reap_plan = ReapPlan::Controlled {
                fact: ExitFact {
                    code: Some(code),
                    signal: None,
                    cleanup_errors: Vec::new(),
                },
            };
            fake
        }

        fn release_reap(&self) {
            self.reap_release.notify_one();
        }

        fn reaps_after_signal(signal: FakeSignal, after: Duration, code: i32) -> Self {
            let mut fake = Self::reaps_on(signal, code);
            let ReapPlan::OnSignal {
                after: reap_after, ..
            } = &mut fake.reap_plan
            else {
                unreachable!("reaps_on must create an on-signal plan");
            };
            *reap_after = after;
            fake
        }

        fn with_signal_errors(self, signals: &[FakeSignal]) -> Self {
            *self
                .signal_errors
                .lock()
                .expect("fake signal error lock should not be poisoned") = signals.to_vec();
            self
        }

        fn panics_on_signal(self, signal: FakeSignal) -> Self {
            *self
                .panic_signal
                .lock()
                .expect("fake panic signal lock should not be poisoned") = Some(signal);
            self
        }

        fn panics_while_waiting(self) -> Self {
            self.panic_waits_remaining.store(1, Ordering::SeqCst);
            self
        }

        fn always_panics_while_waiting(self) -> Self {
            self.panic_waits_remaining.store(usize::MAX, Ordering::SeqCst);
            self
        }

        fn with_resize_error(self) -> Self {
            self.resize_error.store(true, Ordering::SeqCst);
            self
        }

        fn blocking_write() -> Self {
            let fake = Self::pending();
            fake.block_write.store(true, Ordering::SeqCst);
            fake
        }

        fn wait_call_count(&self) -> usize {
            self.wait_calls.load(Ordering::SeqCst)
        }

        fn writes(&self) -> Vec<Vec<u8>> {
            self.writes
                .lock()
                .expect("fake write log lock should not be poisoned")
                .clone()
        }

        fn close_call_count(&self) -> usize {
            self.close_calls.load(Ordering::SeqCst)
        }

        fn resizes(&self) -> Vec<(u16, u16)> {
            self.resizes
                .lock()
                .expect("fake resize log lock should not be poisoned")
                .clone()
        }

        fn signal_calls(&self) -> Vec<&'static str> {
            self.signal_calls
                .lock()
                .expect("fake signal log lock should not be poisoned")
                .clone()
        }

        fn signal_times(&self) -> Vec<(&'static str, tokio::time::Instant)> {
            self.signal_times
                .lock()
                .expect("fake signal timing lock should not be poisoned")
                .clone()
        }

        fn signal(&self, signal: FakeSignal, label: &'static str) -> io::Result<()> {
            self.signal_calls
                .lock()
                .expect("fake signal log lock should not be poisoned")
                .push(label);
            self.signal_times
                .lock()
                .expect("fake signal timing lock should not be poisoned")
                .push((label, tokio::time::Instant::now()));
            if *self
                .panic_signal
                .lock()
                .expect("fake panic signal lock should not be poisoned")
                == Some(signal)
            {
                panic!("injected {label} panic");
            }
            if let ReapPlan::OnSignal {
                signal: trigger,
                fact,
                after,
            } = &self.reap_plan
                && *trigger == signal
            {
                let _ = after;
                self.exit_tx.send_replace(Some(fact.clone()));
            }
            if self
                .signal_errors
                .lock()
                .expect("fake signal error lock should not be poisoned")
                .contains(&signal)
            {
                Err(io::Error::other(format!("{label} failed")))
            } else {
                Ok(())
            }
        }
    }

    #[async_trait]
    impl ProcessOwner for FakeOwner {
        fn pid(&self) -> u32 {
            4_242
        }

        async fn write(&self, bytes: &[u8]) -> io::Result<()> {
            self.writes
                .lock()
                .expect("fake write log lock should not be poisoned")
                .push(bytes.to_vec());
            if self.block_write.load(Ordering::SeqCst) {
                self.write_entered.store(true, Ordering::SeqCst);
                self.write_release.notified().await;
            }
            Ok(())
        }

        async fn close_stdin(&self) -> io::Result<()> {
            self.close_calls.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }

        async fn resize(&self, cols: u16, rows: u16) -> io::Result<()> {
            self.resizes
                .lock()
                .expect("fake resize log lock should not be poisoned")
                .push((cols, rows));
            if self.resize_error.load(Ordering::SeqCst) {
                Err(io::Error::other("resize failed"))
            } else {
                Ok(())
            }
        }

        async fn interrupt(&self) -> io::Result<()> {
            self.signal(FakeSignal::Interrupt, "interrupt")
        }

        async fn terminate(&self) -> io::Result<()> {
            self.signal(FakeSignal::Terminate, "terminate")
        }

        async fn force_kill(&self) -> io::Result<()> {
            self.signal(FakeSignal::ForceKill, "force_kill")
        }

        async fn wait_reaped(&self, _deadline: Instant) -> io::Result<ExitFact> {
            self.wait_calls.fetch_add(1, Ordering::SeqCst);
            let remaining = self.panic_waits_remaining.load(Ordering::SeqCst);
            if remaining > 0 {
                if remaining != usize::MAX {
                    self.panic_waits_remaining.fetch_sub(1, Ordering::SeqCst);
                }
                panic!("injected wait_reaped panic");
            }
            match &self.reap_plan {
                ReapPlan::Pending => future::pending().await,
                ReapPlan::Natural { after, fact } => {
                    tokio::time::sleep(*after).await;
                    Ok(fact.clone())
                }
                ReapPlan::OnSignal { after, .. } => {
                    let mut exit = self.exit_tx.subscribe();
                    loop {
                        let fact = exit.borrow().clone();
                        if let Some(fact) = fact {
                            if !after.is_zero() {
                                tokio::time::sleep(*after).await;
                            }
                            return Ok(fact);
                        }
                        exit.changed().await.map_err(|_| {
                            io::Error::new(io::ErrorKind::BrokenPipe, "fake exit sender closed")
                        })?;
                    }
                }
                ReapPlan::Controlled { fact } => {
                    self.reap_release.notified().await;
                    Ok(fact.clone())
                }
            }
        }
    }

    #[tokio::test]
    async fn dropping_the_handle_does_not_remove_the_owned_process() {
        let fake = Arc::new(FakeOwner::pending());
        let supervisor = ProcessSupervisor::new(SupervisorConfig::default());
        let request = fake_request(ExecutionPolicy::default());
        let owner = request.owner.clone();
        let output = Arc::new(OutputBuffer::new(request.policy.output_limit_bytes));
        let handle = supervisor
            .register_owned(request, fake.clone(), output)
            .await
            .expect("fake process should register");
        let session_id = handle.session_id;

        let _ = handle;
        tokio::task::yield_now().await;

        let snapshot = supervisor
            .status(&owner, &session_id)
            .await
            .expect("registry should retain the process");
        assert_eq!(snapshot.pid, 4_242);
        assert_eq!(snapshot.state, ProcessState::Running);
        assert_eq!(fake.wait_call_count(), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn hard_deadline_classifies_a_just_after_deadline_exit_as_timed_out() {
        let deadline = tokio::time::Instant::now().into_std() + Duration::from_millis(10);
        let policy = ExecutionPolicy {
            deadline: Some(deadline),
            interrupt_grace: Duration::from_millis(20),
            terminate_grace: Duration::from_millis(20),
            reap_grace: Duration::from_millis(20),
            ..ExecutionPolicy::default()
        };
        let (supervisor, handle, fake, _output) = register_fake_with_policy(
            FakeOwner::exits_after(Duration::from_millis(11), 0),
            policy,
        )
        .await;

        wait_for_test_condition(|| fake.wait_call_count() == 1).await;
        tokio::task::yield_now().await;
        tokio::time::advance(Duration::from_millis(200)).await;
        tokio::task::yield_now().await;
        tokio::time::advance(Duration::from_millis(500)).await;
        wait_for_test_condition(|| {
            matches!(
                supervisor.terminal_outcome_if_ready(
                    &handle.owner,
                    &handle.session_id,
                    OutputCursor::START,
                ),
                Ok(Some(_))
            )
        })
        .await;
        let result = supervisor
            .poll(
                &handle.owner,
                &handle.session_id,
                OutputCursor::START,
                Instant::now(),
            )
            .await
            .expect("deadline outcome should remain pollable");

        assert!(matches!(
            &result,
            PollResult::Finished(ExecutionOutcome::TimedOut { .. })
        ), "unexpected deadline result: {result:?}");
    }

    #[tokio::test(start_paused = true)]
    async fn pre_deadline_exit_stays_exited_when_final_output_drain_crosses_deadline() {
        let deadline = tokio::time::Instant::now().into_std() + Duration::from_millis(10);
        let policy = ExecutionPolicy {
            deadline: Some(deadline),
            interrupt_grace: Duration::from_millis(20),
            terminate_grace: Duration::from_millis(20),
            reap_grace: Duration::from_millis(20),
            ..ExecutionPolicy::default()
        };
        let (supervisor, handle, fake, _output) = register_fake_with_policy(
            FakeOwner::exits_after(Duration::from_millis(9), 0),
            policy,
        )
        .await;
        let session = supervisor
            .session(&handle.owner, &handle.session_id)
            .expect("registered session")
            .session_arc();

        wait_for_test_condition(|| fake.wait_call_count() == 1).await;
        tokio::time::advance(Duration::from_millis(9)).await;
        wait_for_test_condition(|| session.exit_observation().is_some()).await;
        let Some(ExitObservation::Reaped { observed_at, .. }) = session.exit_observation() else {
            panic!("pre-deadline reap must be observed");
        };
        assert!(observed_at < tokio::time::Instant::from_std(deadline));
        tokio::time::advance(Duration::from_millis(200)).await;
        wait_for_test_condition(|| {
            matches!(
                supervisor.terminal_outcome_if_ready(
                    &handle.owner,
                    &handle.session_id,
                    OutputCursor::START,
                ),
                Ok(Some(_))
            )
        })
        .await;
        let result = supervisor
            .poll(
                &handle.owner,
                &handle.session_id,
                OutputCursor::START,
                Instant::now(),
            )
            .await
            .expect("natural outcome should remain pollable");

        assert!(matches!(
            &result,
            PollResult::Finished(ExecutionOutcome::Exited { code: Some(0), .. })
        ), "unexpected pre-deadline result: {result:?}");
    }

    #[tokio::test(start_paused = true)]
    async fn deadline_watcher_does_not_retain_an_evicted_terminal_session() {
        let policy = ExecutionPolicy {
            deadline: Some(
                tokio::time::Instant::now().into_std() + Duration::from_secs(3_600),
            ),
            ..ExecutionPolicy::default()
        };
        let (supervisor, handle, fake, _output) =
            register_fake_with_policy(FakeOwner::exits_after(Duration::ZERO, 0), policy).await;
        let session = supervisor
            .session(&handle.owner, &handle.session_id)
            .expect("registered session")
            .session_arc();
        let weak_session = Arc::downgrade(&session);
        drop(session);

        wait_for_test_condition(|| fake.wait_call_count() == 1).await;
        tokio::time::advance(Duration::from_millis(200)).await;
        tokio::task::yield_now().await;
        tokio::time::advance(Duration::from_millis(200)).await;
        wait_for_test_condition(|| {
            matches!(
                supervisor.terminal_outcome_if_ready(
                    &handle.owner,
                    &handle.session_id,
                    OutputCursor::START,
                ),
                Ok(Some(_))
            )
        })
        .await;

        assert!(supervisor.registry.evict_oldest_finished());
        tokio::task::yield_now().await;
        assert!(
            weak_session.upgrade().is_none(),
            "the sleeping deadline watcher retained the evicted session"
        );
    }

    #[tokio::test]
    async fn write_close_resize_and_interrupt_delegate_to_the_registered_owner() {
        let (supervisor, handle, fake, _output) = register_fake(FakeOwner::pending()).await;

        supervisor
            .write(&handle.owner, &handle.session_id, b"typed input")
            .await
            .expect("write should delegate");
        supervisor
            .close_stdin(&handle.owner, &handle.session_id)
            .await
            .expect("close should delegate");
        supervisor
            .resize(&handle.owner, &handle.session_id, 120, 40)
            .await
            .expect("resize should delegate");
        supervisor
            .interrupt(&handle.owner, &handle.session_id)
            .await
            .expect("interrupt should delegate");

        assert_eq!(fake.writes(), vec![b"typed input".to_vec()]);
        assert_eq!(fake.close_call_count(), 1);
        assert_eq!(fake.resizes(), vec![(120, 40)]);
        assert_eq!(fake.signal_calls(), vec!["interrupt"]);
        assert_eq!(fake.wait_call_count(), 1);
    }

    #[tokio::test]
    async fn resize_rejects_zero_dimensions_before_calling_the_owner() {
        let (supervisor, handle, fake, _output) = register_fake(FakeOwner::pending()).await;

        for (cols, rows) in [(0, 24), (80, 0)] {
            let error = supervisor
                .resize(&handle.owner, &handle.session_id, cols, rows)
                .await
                .expect_err("zero PTY dimensions should be rejected");
            assert_eq!(error.code(), "invalid_transport");
        }

        assert!(fake.resizes().is_empty());
    }

    #[tokio::test]
    async fn resize_owner_failures_use_the_stable_io_error_code() {
        let (supervisor, handle, fake, _output) =
            register_fake(FakeOwner::pending().with_resize_error()).await;

        let error = supervisor
            .resize(&handle.owner, &handle.session_id, 100, 30)
            .await
            .expect_err("owner resize failure should propagate");

        assert_eq!(error.code(), "io");
        assert!(error.to_string().contains("resize failed"));
        assert_eq!(fake.resizes(), vec![(100, 30)]);
    }

    #[tokio::test]
    async fn owner_io_failures_use_the_stable_io_error_code() {
        let fake = FakeOwner::pending().with_signal_errors(&[FakeSignal::Interrupt]);
        let (supervisor, handle, _fake, _output) = register_fake(fake).await;

        let error = supervisor
            .interrupt(&handle.owner, &handle.session_id)
            .await
            .expect_err("owner failure should propagate");

        assert_eq!(error.code(), "io");
        assert!(error.to_string().contains("interrupt failed"));
    }

    #[tokio::test]
    async fn awaited_owner_methods_do_not_hold_registry_or_session_locks() {
        let (supervisor, handle, fake, _output) = register_fake(FakeOwner::blocking_write()).await;
        let writing_supervisor = supervisor.clone();
        let writing_handle = handle.clone();
        let write = tokio::spawn(async move {
            writing_supervisor
                .write(
                    &writing_handle.owner,
                    &writing_handle.session_id,
                    b"blocked",
                )
                .await
        });
        wait_for_test_condition(|| fake.write_entered.load(Ordering::SeqCst)).await;

        let snapshot = tokio::time::timeout(
            Duration::from_millis(100),
            supervisor.status(&handle.owner, &handle.session_id),
        )
        .await
        .expect("status must not wait for the owner method")
        .expect("status should succeed");
        assert_eq!(snapshot.state, ProcessState::Running);

        fake.write_release.notify_waiters();
        write
            .await
            .expect("write task should join")
            .expect("write should finish");
    }

    #[tokio::test(start_paused = true)]
    async fn an_in_flight_action_prevents_reaper_retirement_until_it_completes() {
        let policy = ExecutionPolicy {
            lease: Duration::from_millis(10),
            ..ExecutionPolicy::default()
        };
        let (supervisor, handle, fake, _output) =
            register_fake_with_policy(FakeOwner::blocking_write(), policy).await;
        let writing_supervisor = supervisor.clone();
        let writing_handle = handle.clone();
        let write = tokio::spawn(async move {
            writing_supervisor
                .write(
                    &writing_handle.owner,
                    &writing_handle.session_id,
                    b"blocked",
                )
                .await
        });
        wait_for_test_condition(|| fake.write_entered.load(Ordering::SeqCst)).await;

        tokio::time::advance(Duration::from_secs(1)).await;
        tokio::task::yield_now().await;
        let claimed = supervisor.registry.claim_expired(Instant::now());
        assert!(
            claimed.is_empty(),
            "reaper claimed a session while an authenticated action was in flight"
        );
        assert!(
            fake.signal_calls().is_empty(),
            "reaper retired a session while an authenticated action was in flight"
        );

        fake.write_release.notify_waiters();
        write
            .await
            .expect("write task should join")
            .expect("write should finish");
        tokio::task::yield_now().await;
        assert!(fake.signal_calls().is_empty());
    }

    #[tokio::test(start_paused = true)]
    async fn concurrent_shutdown_is_single_flight_and_reports_one_exact_retirement() {
        let policy = ExecutionPolicy {
            interrupt_grace: Duration::from_millis(10),
            terminate_grace: Duration::from_millis(10),
            reap_grace: Duration::from_millis(10),
            ..ExecutionPolicy::default()
        };
        let (supervisor, handle, fake, _output) = register_fake_with_policy(
            FakeOwner::reaps_on(FakeSignal::Interrupt, 130),
            policy,
        )
        .await;

        let first = tokio::spawn({
            let supervisor = supervisor.clone();
            async move { supervisor.shutdown().await }
        });
        let second = tokio::spawn({
            let supervisor = supervisor.clone();
            async move { supervisor.shutdown().await }
        });
        let first = first.await.expect("first shutdown should join");
        let second = second.await.expect("second shutdown should join");

        assert_eq!(first, second);
        assert_eq!(first.sessions.len(), 1);
        assert_eq!(first.sessions[0].session_id, handle.session_id);
        assert_eq!(fake.signal_calls(), vec!["interrupt"]);
        assert_eq!(fake.wait_call_count(), 1);
    }

    #[tokio::test]
    async fn retiring_session_keeps_its_capacity_until_exact_reap_finishes() {
        let policy = ExecutionPolicy {
            lease: Duration::from_millis(10),
            interrupt_grace: Duration::from_millis(100),
            terminate_grace: Duration::from_millis(100),
            reap_grace: Duration::from_millis(500),
            ..ExecutionPolicy::default()
        };
        let fake = Arc::new(FakeOwner::reaps_after_signal(
            FakeSignal::ForceKill,
            Duration::from_millis(250),
            137,
        ));
        let supervisor = ProcessSupervisor::new(SupervisorConfig {
            max_sessions: 1,
            reaper_interval: Duration::from_secs(3_600),
        });
        let request = fake_request(policy);
        let owner = request.owner;
        let policy = request.policy;
        let output = Arc::new(OutputBuffer::new(policy.output_limit_bytes));
        let mut reservation = supervisor
            .registry
            .begin_test_reservation()
            .expect("test process should reserve the sole capacity slot");
        let session_id = SessionId::new();
        let started_at = Instant::now();
        let (exit, _exit_receiver) = tokio::sync::watch::channel(None);
        let (lifecycle, _lifecycle_receiver) = tokio::sync::watch::channel(0);
        let session = Arc::new(Session {
            process: fake.clone(),
            output,
            policy: policy.clone(),
            started_at,
            last_activity_at: Mutex::new(started_at),
            state: Mutex::new(SessionState {
                process_state: ProcessState::Running,
                exit_observation: None,
                terminal: None,
            }),
            exit,
            lifecycle,
            before_lost_commit: Mutex::new(None),
        });
        assert!(matches!(
            supervisor.registry.test_commit(
                &mut reservation,
                session_id,
                owner,
                session.clone(),
                policy.lease,
                started_at,
            ),
            CommitResult::Active
        ));
        start_waiter(session);

        let retirements = supervisor
            .registry
            .claim_expired(started_at + Duration::from_millis(11));
        assert_eq!(retirements.len(), 1);
        supervisor.start_retirement(retirements[0].clone());
        wait_for_test_condition(|| fake.wait_call_count() == 1).await;
        assert_eq!(supervisor.registry.counts(), (0, 1, 0));
        let capacity_error = match supervisor.registry.test_reserve_once() {
            Ok(_) => panic!("unreaped retirement released the sole capacity slot"),
            Err(error) => error,
        };
        assert_eq!(capacity_error, crate::registry::ReserveError::Capacity);

        let outcome = retirements[0].wait_outcome().await;
        assert!(outcome_reaped(&outcome), "unexpected retirement outcome: {outcome:?}");
        let reservation = supervisor
            .reserve_start_capacity()
            .await
            .expect("capacity should return after exact reap");
        drop(reservation);
    }

    #[tokio::test(start_paused = true)]
    async fn shutdown_waits_for_and_reports_a_reserved_session_that_commits_late() {
        let supervisor = ProcessSupervisor::new(SupervisorConfig::default());
        let request = fake_request(ExecutionPolicy {
            interrupt_grace: Duration::from_millis(10),
            terminate_grace: Duration::from_millis(10),
            reap_grace: Duration::from_millis(10),
            ..ExecutionPolicy::default()
        });
        let owner = request.owner.clone();
        let policy = request.policy.clone();
        let fake = Arc::new(FakeOwner::reaps_on(FakeSignal::Interrupt, 130));
        let output = Arc::new(OutputBuffer::new(policy.output_limit_bytes));
        let mut reservation = supervisor
            .registry
            .begin_test_reservation()
            .expect("test start should reserve capacity before shutdown");

        let shutdown = tokio::spawn({
            let supervisor = supervisor.clone();
            async move { supervisor.shutdown().await }
        });
        wait_for_test_condition(|| supervisor.shutdown.started.load(Ordering::Acquire)).await;
        assert_eq!(supervisor.registry.counts(), (0, 0, 1));

        let session_id = SessionId::new();
        let started_at = Instant::now();
        let (exit, _exit_receiver) = tokio::sync::watch::channel(None);
        let (lifecycle, _lifecycle_receiver) = tokio::sync::watch::channel(0);
        let session = Arc::new(Session {
            process: fake.clone(),
            output,
            policy: policy.clone(),
            started_at,
            last_activity_at: Mutex::new(started_at),
            state: Mutex::new(SessionState {
                process_state: ProcessState::Running,
                exit_observation: None,
                terminal: None,
            }),
            exit,
            lifecycle,
            before_lost_commit: Mutex::new(None),
        });
        let commit = supervisor.registry.test_commit(
            &mut reservation,
            session_id,
            owner.clone(),
            session.clone(),
            policy.lease,
            started_at,
        );
        start_waiter(session);
        let CommitResult::Retiring(retirement) = commit else {
            panic!("a post-shutdown reservation commit must retire, never become active");
        };
        supervisor.start_retirement(retirement);

        let report = shutdown.await.expect("shutdown task should join");
        assert_eq!(report.sessions.len(), 1);
        assert_eq!(report.sessions[0].session_id, session_id);
        assert_eq!(report.sessions[0].owner, owner);
        assert!(outcome_reaped(&report.sessions[0].outcome));
        assert_eq!(fake.signal_calls(), vec!["interrupt"]);
        assert_eq!(fake.wait_call_count(), 1);
    }

    #[tokio::test]
    async fn shutdown_waits_for_an_in_flight_action_before_retiring_the_session() {
        let policy = ExecutionPolicy {
            interrupt_grace: Duration::from_millis(10),
            terminate_grace: Duration::from_millis(10),
            reap_grace: Duration::from_millis(10),
            ..ExecutionPolicy::default()
        };
        let mut owner = FakeOwner::blocking_write();
        owner.reap_plan = ReapPlan::Controlled {
            fact: ExitFact {
                code: Some(130),
                signal: None,
                cleanup_errors: Vec::new(),
            },
        };
        let (supervisor, handle, fake, _output) =
            register_fake_with_policy(owner, policy).await;
        let writing = tokio::spawn({
            let supervisor = supervisor.clone();
            let handle = handle.clone();
            async move {
                supervisor
                    .write(&handle.owner, &handle.session_id, b"blocked")
                    .await
            }
        });
        wait_for_test_condition(|| fake.write_entered.load(Ordering::SeqCst)).await;

        let shutdown = tokio::spawn({
            let supervisor = supervisor.clone();
            async move { supervisor.shutdown().await }
        });
        tokio::task::yield_now().await;
        assert!(
            !shutdown.is_finished(),
            "shutdown must wait for the authenticated in-flight action"
        );
        assert!(fake.signal_calls().is_empty());

        fake.block_write.store(false, Ordering::SeqCst);
        fake.write_release.notify_waiters();
        writing
            .await
            .expect("write task should join")
            .expect("write should complete");
        fake.release_reap();

        let report = tokio::time::timeout(Duration::from_secs(1), shutdown)
            .await
            .expect("shutdown should finish after the action releases")
            .expect("shutdown task should join");
        assert_eq!(report.sessions.len(), 1);
        assert_eq!(report.sessions[0].session_id, handle.session_id);
        assert!(matches!(
            report.sessions[0].outcome,
            ExecutionOutcome::Cancelled { .. } | ExecutionOutcome::Lost { .. }
        ));
    }

    #[tokio::test(start_paused = true)]
    async fn shutdown_omits_an_exit_observed_during_the_final_output_drain() {
        let (supervisor, handle, _fake, _output) =
            register_fake(FakeOwner::exits_after(Duration::from_millis(5), 0)).await;
        tokio::time::advance(Duration::from_millis(6)).await;
        let session = supervisor
            .registry
            .get(&handle.session_id)
            .expect("exit-observed session should still be registered");
        wait_for_test_condition(|| session.exit_observation().is_some()).await;
        assert!(
            session.terminal().is_none(),
            "test must enter shutdown before the final output drain installs terminal state"
        );

        let shutdown = tokio::spawn({
            let supervisor = supervisor.clone();
            async move { supervisor.shutdown().await }
        });
        tokio::time::advance(Duration::from_millis(120)).await;
        let report = shutdown.await.expect("shutdown task should join");

        assert!(
            report.sessions.is_empty(),
            "naturally exited sessions are not shutdown cancellations: {:?}",
            report.sessions
        );
    }

    #[tokio::test(start_paused = true)]
    async fn terminal_inspection_does_not_renew_the_session_lease() {
        let policy = ExecutionPolicy {
            lease: Duration::from_millis(10),
            ..ExecutionPolicy::default()
        };
        let (supervisor, handle, _fake, _output) = register_fake_with_config(
            FakeOwner::pending(),
            policy,
            SupervisorConfig {
                max_sessions: 1,
                reaper_interval: Duration::from_secs(3_600),
            },
        )
        .await;
        let lease_started_at = supervisor
            .registry
            .get(&handle.session_id)
            .expect("registered session")
            .snapshot()
            .last_activity_at;

        for _ in 0..3 {
            assert!(
                supervisor
                    .terminal_outcome_if_ready(
                        &handle.owner,
                        &handle.session_id,
                        OutputCursor::START,
                    )
                    .expect("inspection should authenticate")
                    .is_none()
            );
            tokio::time::advance(Duration::from_millis(4)).await;
        }

        let retirements = supervisor
            .registry
            .claim_expired(lease_started_at + Duration::from_millis(12));
        assert_eq!(
            retirements.len(),
            1,
            "read-only terminal inspection must not renew an idle lease"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn terminal_inspection_waits_for_final_output_freeze() {
        let (supervisor, handle, _fake, output) =
            register_fake(FakeOwner::exits_after(Duration::from_millis(5), 0)).await;
        tokio::time::advance(Duration::from_millis(6)).await;
        let session = supervisor
            .registry
            .get(&handle.session_id)
            .expect("exit-observed session should remain registered");
        wait_for_test_condition(|| session.exit_observation().is_some()).await;
        output.push(OutputStream::Stdout, b"final-drain-output");

        assert!(
            supervisor
                .terminal_outcome_if_ready(
                    &handle.owner,
                    &handle.session_id,
                    OutputCursor::START,
                )
                .expect("inspection should authenticate")
                .is_none(),
            "an observed exit is not terminal until final output has frozen"
        );

        tokio::time::advance(Duration::from_millis(120)).await;
        wait_for_test_condition(|| session.terminal().is_some()).await;
        let outcome = supervisor
            .terminal_outcome_if_ready(
                &handle.owner,
                &handle.session_id,
                OutputCursor::START,
            )
            .expect("terminal inspection should succeed")
            .expect("terminal outcome should now be ready");
        let ExecutionOutcome::Exited { output, .. } = outcome else {
            panic!("natural exit should stay Exited");
        };
        assert_eq!(output.raw_bytes(), b"final-drain-output");
    }

    #[tokio::test]
    async fn retirement_signal_panic_becomes_lost_without_releasing_capacity() {
        let policy = ExecutionPolicy {
            lease: Duration::from_millis(10),
            interrupt_grace: Duration::from_millis(10),
            terminate_grace: Duration::from_millis(10),
            reap_grace: Duration::from_millis(10),
            ..ExecutionPolicy::default()
        };
        let (supervisor, _handle, _fake, _output) = register_fake_with_config(
            FakeOwner::pending().panics_on_signal(FakeSignal::Interrupt),
            policy,
            SupervisorConfig {
                max_sessions: 1,
                reaper_interval: Duration::from_secs(3_600),
            },
        )
        .await;
        let retirements = supervisor
            .registry
            .claim_expired(Instant::now() + Duration::from_secs(1));
        assert_eq!(retirements.len(), 1);
        supervisor.start_retirement(retirements[0].clone());

        let outcome = tokio::time::timeout(
            Duration::from_secs(1),
            retirements[0].wait_outcome(),
        )
        .await
        .expect("retirement panic must not strand outcome waiters");
        let ExecutionOutcome::Lost { cleanup, .. } = outcome else {
            panic!("retirement panic must become Lost");
        };
        assert!(!cleanup.reaped);
        assert!(
            cleanup
                .errors
                .iter()
                .any(|error| error.contains("cleanup driver stopped unexpectedly"))
        );
        assert_eq!(supervisor.registry.counts(), (0, 1, 0));
        assert_eq!(
            supervisor.registry.test_reserve_once().err(),
            Some(crate::registry::ReserveError::Capacity)
        );
    }

    #[tokio::test]
    async fn waiter_panic_restarts_the_exact_wait_and_recovers_reap_proof() {
        let (supervisor, handle, fake, _output) = register_fake(
            FakeOwner::reaps_on(FakeSignal::Interrupt, 130).panics_while_waiting(),
        )
        .await;

        let outcome = tokio::time::timeout(
            Duration::from_secs(1),
            supervisor.cancel(&handle.owner, &handle.session_id),
        )
        .await
        .expect("restarted waiter must remain bounded")
        .expect("owned session should remain observable");

        assert!(matches!(outcome, ExecutionOutcome::Cancelled { .. }));
        assert_eq!(fake.wait_call_count(), 2);
    }

    #[tokio::test]
    async fn repeated_waiter_panic_publishes_lost_instead_of_stranding_waiters() {
        let (supervisor, handle, fake, _output) =
            register_fake(FakeOwner::pending().always_panics_while_waiting()).await;

        let result = tokio::time::timeout(
            Duration::from_secs(1),
            supervisor.poll(
                &handle.owner,
                &handle.session_id,
                OutputCursor::START,
                Instant::now() + Duration::from_secs(10),
            ),
        )
        .await
        .expect("repeated waiter panic must wake poll")
        .expect("owned session should remain observable");

        let PollResult::Finished(ExecutionOutcome::Lost { cleanup, .. }) = result else {
            panic!("repeated waiter panic must become a terminal Lost outcome");
        };
        assert!(cleanup.errors.iter().any(|error| {
            error.contains("waiter task stopped unexpectedly after 2 attempts")
        }));
        assert_eq!(fake.wait_call_count(), 2);
    }

    #[tokio::test]
    async fn shutdown_reports_an_unreaped_lost_session_instead_of_omitting_it() {
        let (supervisor, handle, _fake, _output) =
            register_fake(FakeOwner::pending().always_panics_while_waiting()).await;
        let result = supervisor
            .poll(
                &handle.owner,
                &handle.session_id,
                OutputCursor::START,
                Instant::now() + Duration::from_secs(1),
            )
            .await
            .expect("repeated waiter failure should remain observable");
        let PollResult::Finished(ExecutionOutcome::Lost { cleanup, .. }) = result else {
            panic!("repeated waiter failure must become Lost");
        };
        assert!(!cleanup.reaped);

        let report = supervisor.shutdown().await;

        assert_eq!(report.sessions.len(), 1);
        assert_eq!(report.sessions[0].session_id, handle.session_id);
        assert!(matches!(
            &report.sessions[0].outcome,
            ExecutionOutcome::Lost { cleanup, .. } if !cleanup.reaped
        ));
    }

    #[tokio::test(start_paused = true)]
    async fn running_poll_honors_the_absolute_cursor_and_requested_yield() {
        let (supervisor, handle, _fake, output) = register_fake(FakeOwner::pending()).await;
        output.push(OutputStream::Stdout, b"abcdef");
        let observed_start = tokio::time::Instant::now();

        let result = supervisor
            .poll(
                &handle.owner,
                &handle.session_id,
                OutputCursor::new(2),
                Instant::now() + Duration::from_millis(40),
            )
            .await
            .expect("poll should succeed");

        let PollResult::Running { snapshot, output } = result else {
            panic!("pending process should return Running");
        };
        assert_eq!(snapshot.state, ProcessState::Running);
        assert_eq!(output.raw_bytes(), b"cdef");
        assert_eq!(output.next_cursor.offset(), 6);
        assert!(tokio::time::Instant::now() >= observed_start + Duration::from_millis(35));
    }

    #[tokio::test]
    async fn unknown_session_and_wrong_owner_fail_closed_with_stable_codes() {
        let (supervisor, handle, fake, _output) = register_fake(FakeOwner::pending()).await;
        let unknown_session = SessionId::new();
        let unknown = supervisor
            .status(&handle.owner, &unknown_session)
            .await
            .expect_err("unknown session must be rejected");
        let wrong_owner = ExecutionOwner::new(uuid::Uuid::now_v7(), uuid::Uuid::now_v7());
        let mismatch = supervisor
            .poll(
                &wrong_owner,
                &handle.session_id,
                OutputCursor::START,
                Instant::now(),
            )
            .await
            .expect_err("wrong owner must be rejected");
        let rejected_write = supervisor
            .write(&wrong_owner, &handle.session_id, b"must not be delegated")
            .await
            .expect_err("wrong-owner write must be rejected");
        let rejected_cancel = supervisor
            .cancel(&handle.owner, &unknown_session)
            .await
            .expect_err("unknown-session cancel must be rejected");

        assert_eq!(unknown.code(), "session_not_found");
        assert_eq!(mismatch.code(), "owner_mismatch");
        assert_eq!(rejected_write.code(), "owner_mismatch");
        assert_eq!(rejected_cancel.code(), "session_not_found");
        assert!(fake.writes().is_empty());
        assert_eq!(fake.close_call_count(), 0);
        assert!(fake.signal_calls().is_empty());
    }

    #[tokio::test(start_paused = true)]
    async fn natural_exit_wakes_poll_without_waiting_for_the_remaining_yield() {
        let observed_start = tokio::time::Instant::now();
        let (supervisor, handle, fake, _output) =
            register_fake(FakeOwner::exits_after(Duration::from_millis(20), 0)).await;

        let result = supervisor
            .poll(
                &handle.owner,
                &handle.session_id,
                OutputCursor::START,
                Instant::now() + Duration::from_secs(10),
            )
            .await
            .expect("poll should observe exit");

        assert!(matches!(
            result,
            PollResult::Finished(ExecutionOutcome::Exited { code: Some(0), .. })
        ));
        assert!(tokio::time::Instant::now() < observed_start + Duration::from_millis(250));
        assert_eq!(fake.wait_call_count(), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn shared_waiter_updates_status_when_the_process_exits_naturally() {
        let (supervisor, handle, _fake, _output) =
            register_fake(FakeOwner::exits_after(Duration::from_millis(5), 0)).await;
        tokio::time::sleep(Duration::from_millis(6)).await;
        tokio::task::yield_now().await;

        let snapshot = supervisor
            .status(&handle.owner, &handle.session_id)
            .await
            .expect("status should succeed");

        assert_eq!(snapshot.state, ProcessState::Exited);
    }

    #[tokio::test]
    async fn waiter_publishes_the_single_exit_fact_through_the_shared_watch_channel() {
        let (supervisor, handle, fake, _output) =
            register_fake(FakeOwner::reaps_on(FakeSignal::Interrupt, 130)).await;
        let session = supervisor
            .registry
            .get(&handle.session_id)
            .expect("registered session should exist");
        let mut exits = session.exit.subscribe();

        fake.interrupt()
            .await
            .expect("fake interrupt should trigger exit");
        tokio::time::timeout(Duration::from_secs(1), exits.changed())
            .await
            .expect("waiter should publish without blocking")
            .expect("watch sender should remain open");

        assert!(matches!(
            exits.borrow().as_ref(),
            Some(ExitObservation::Reaped {
                fact: ExitFact { code: Some(130), .. },
                ..
            })
        ));
        assert_eq!(fake.wait_call_count(), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn natural_exit_allows_a_bounded_final_output_drain() {
        let observed_start = tokio::time::Instant::now();
        let (supervisor, handle, _fake, output) =
            register_fake(FakeOwner::exits_after(Duration::from_millis(20), 0)).await;
        let late_output = output.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(100)).await;
            late_output.push(OutputStream::Stdout, b"drained");
        });

        let outcome = finished(
            supervisor
                .poll(
                    &handle.owner,
                    &handle.session_id,
                    OutputCursor::START,
                    Instant::now() + Duration::from_secs(10),
                )
                .await
                .expect("poll should observe exit"),
        );

        assert_eq!(outcome_output(&outcome).raw_bytes(), b"drained");
        assert!(tokio::time::Instant::now() <= observed_start + Duration::from_millis(150));
    }

    #[tokio::test(start_paused = true)]
    async fn natural_exit_preserves_nonfatal_platform_cleanup_diagnostics() {
        let (supervisor, handle, _fake, _output) = register_fake(
            FakeOwner::exits_with_cleanup_error("ConPTY close exceeded its deadline"),
        )
        .await;

        let outcome = finished(
            supervisor
                .poll(
                    &handle.owner,
                    &handle.session_id,
                    OutputCursor::START,
                    Instant::now() + Duration::from_secs(1),
                )
                .await
                .expect("terminal poll should succeed"),
        );

        let ExecutionOutcome::Exited { code, cleanup, .. } = outcome else {
            panic!("exact platform exit should remain Exited");
        };
        assert_eq!(code, Some(0));
        assert!(cleanup.reaped);
        assert_eq!(
            cleanup.errors,
            vec!["ConPTY close exceeded its deadline".to_owned()]
        );
    }

    #[tokio::test(start_paused = true)]
    async fn cancelled_exit_preserves_nonfatal_platform_cleanup_diagnostics() {
        let mut fake = FakeOwner::reaps_on(FakeSignal::Interrupt, 130);
        let ReapPlan::OnSignal { fact, .. } = &mut fake.reap_plan else {
            unreachable!("reaps_on must create an on-signal plan");
        };
        fact.cleanup_errors
            .push("ConPTY close exceeded its deadline".to_owned());
        let (supervisor, handle, _fake, _output) = register_fake(fake).await;

        let outcome = supervisor
            .cancel(&handle.owner, &handle.session_id)
            .await
            .expect("cancellation should resolve");

        let ExecutionOutcome::Cancelled { cleanup, .. } = outcome else {
            panic!("signalled exact exit should become Cancelled");
        };
        assert!(cleanup.reaped);
        assert_eq!(
            cleanup.errors,
            vec!["ConPTY close exceeded its deadline".to_owned()]
        );
    }

    #[tokio::test(start_paused = true)]
    async fn terminal_polls_are_idempotent_and_honor_each_absolute_cursor() {
        let (supervisor, handle, _fake, output) =
            register_fake(FakeOwner::exits_after(Duration::from_millis(5), 7)).await;
        output.push(OutputStream::Stdout, b"abcdef");

        let from_two = finished(
            supervisor
                .poll(
                    &handle.owner,
                    &handle.session_id,
                    OutputCursor::new(2),
                    Instant::now() + Duration::from_secs(10),
                )
                .await
                .expect("first terminal poll should succeed"),
        );
        let repeated = finished(
            supervisor
                .poll(
                    &handle.owner,
                    &handle.session_id,
                    OutputCursor::new(2),
                    Instant::now() + Duration::from_secs(10),
                )
                .await
                .expect("repeated terminal poll should succeed"),
        );
        let from_four = finished(
            supervisor
                .poll(
                    &handle.owner,
                    &handle.session_id,
                    OutputCursor::new(4),
                    Instant::now() + Duration::from_secs(10),
                )
                .await
                .expect("cursor-specific terminal poll should succeed"),
        );

        assert_eq!(from_two, repeated);
        assert_eq!(outcome_output(&from_two).raw_bytes(), b"cdef");
        assert_eq!(outcome_output(&from_four).raw_bytes(), b"ef");
    }

    #[tokio::test(start_paused = true)]
    async fn terminal_output_is_frozen_after_the_bounded_drain() {
        let (supervisor, handle, _fake, output) =
            register_fake(FakeOwner::exits_after(Duration::from_millis(5), 0)).await;
        output.push(OutputStream::Stdout, b"before terminal");
        let first = finished(
            supervisor
                .poll(
                    &handle.owner,
                    &handle.session_id,
                    OutputCursor::START,
                    Instant::now() + Duration::from_secs(10),
                )
                .await
                .expect("terminal poll should succeed"),
        );

        output.push(OutputStream::Stdout, b"too late");
        let repeated = finished(
            supervisor
                .poll(
                    &handle.owner,
                    &handle.session_id,
                    OutputCursor::START,
                    Instant::now() + Duration::from_secs(10),
                )
                .await
                .expect("repeated terminal poll should succeed"),
        );

        assert_eq!(first, repeated);
        assert_eq!(outcome_output(&repeated).raw_bytes(), b"before terminal");
    }

    #[tokio::test(start_paused = true)]
    async fn waiter_freezes_terminal_output_even_when_no_poll_is_waiting() {
        let (supervisor, handle, _fake, output) =
            register_fake(FakeOwner::exits_after(Duration::from_millis(5), 0)).await;
        output.push(OutputStream::Stdout, b"within drain");
        tokio::time::sleep(Duration::from_millis(130)).await;
        tokio::task::yield_now().await;
        output.push(OutputStream::Stdout, b"after drain");
        let poll_started = tokio::time::Instant::now();

        let outcome = finished(
            supervisor
                .poll(
                    &handle.owner,
                    &handle.session_id,
                    OutputCursor::START,
                    Instant::now() + Duration::from_secs(10),
                )
                .await
                .expect("terminal poll should succeed"),
        );

        assert_eq!(outcome_output(&outcome).raw_bytes(), b"within drain");
        assert!(tokio::time::Instant::now() < poll_started + Duration::from_millis(5));
    }

    #[tokio::test(start_paused = true)]
    async fn no_poller_output_freeze_uses_the_exact_120_millisecond_boundary() {
        let (supervisor, handle, _fake, output) =
            register_fake(FakeOwner::exits_after(Duration::from_millis(5), 0)).await;
        let inside = output.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(124)).await;
            inside.push(OutputStream::Stdout, b"inside");
        });
        let outside = output.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(126)).await;
            outside.push(OutputStream::Stdout, b"outside");
        });
        tokio::time::sleep(Duration::from_millis(130)).await;
        tokio::task::yield_now().await;

        let outcome = finished(
            supervisor
                .poll(
                    &handle.owner,
                    &handle.session_id,
                    OutputCursor::START,
                    Instant::now() + Duration::from_secs(10),
                )
                .await
                .expect("terminal poll should succeed"),
        );

        assert_eq!(outcome_output(&outcome).raw_bytes(), b"inside");
    }

    #[tokio::test(start_paused = true)]
    async fn cancellation_stops_after_a_proven_interrupt_reap() {
        let (supervisor, handle, fake, _output) =
            register_fake(FakeOwner::reaps_on(FakeSignal::Interrupt, 130)).await;

        let outcome = supervisor
            .cancel(&handle.owner, &handle.session_id)
            .await
            .expect("cancel should succeed");

        let ExecutionOutcome::Cancelled { cleanup, .. } = outcome else {
            panic!("a proven cancellation reap should be Cancelled");
        };
        assert!(cleanup.interrupt_attempted);
        assert!(!cleanup.terminate_attempted);
        assert!(!cleanup.force_kill_attempted);
        assert!(cleanup.reaped);
        assert_eq!(fake.signal_calls(), vec!["interrupt"]);
        assert_eq!(fake.wait_call_count(), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn cancellation_stops_after_a_proven_terminate_reap() {
        let (supervisor, handle, fake, _output) =
            register_fake(FakeOwner::reaps_on(FakeSignal::Terminate, 143)).await;

        let outcome = supervisor
            .cancel(&handle.owner, &handle.session_id)
            .await
            .expect("cancel should succeed");

        assert!(matches!(outcome, ExecutionOutcome::Cancelled { .. }));
        assert_eq!(fake.signal_calls(), vec!["interrupt", "terminate"]);
        assert_eq!(fake.wait_call_count(), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn cancellation_runs_full_escalation_before_a_force_kill_reap() {
        let observed_start = tokio::time::Instant::now();
        let (supervisor, handle, fake, _output) =
            register_fake(FakeOwner::reaps_on(FakeSignal::ForceKill, 137)).await;

        let outcome = supervisor
            .cancel(&handle.owner, &handle.session_id)
            .await
            .expect("cancel should succeed");

        assert!(matches!(outcome, ExecutionOutcome::Cancelled { .. }));
        assert_eq!(
            fake.signal_calls(),
            vec!["interrupt", "terminate", "force_kill"]
        );
        assert_eq!(fake.wait_call_count(), 1);
        let times = fake.signal_times();
        assert_eq!(times[0], ("interrupt", observed_start));
        assert_eq!(times[1], ("terminate", observed_start + Duration::from_secs(1)));
        assert_eq!(times[2], ("force_kill", observed_start + Duration::from_secs(2)));
        assert!(tokio::time::Instant::now() <= observed_start + Duration::from_secs(5));
    }

    #[tokio::test(start_paused = true)]
    async fn signal_errors_are_recorded_while_safe_escalation_continues() {
        let fake = FakeOwner::reaps_on(FakeSignal::ForceKill, 137).with_signal_errors(&[
            FakeSignal::Interrupt,
            FakeSignal::Terminate,
            FakeSignal::ForceKill,
        ]);
        let (supervisor, handle, fake, _output) = register_fake(fake).await;

        let outcome = supervisor
            .cancel(&handle.owner, &handle.session_id)
            .await
            .expect("signal failures belong in cleanup diagnostics");

        let ExecutionOutcome::Cancelled { cleanup, .. } = outcome else {
            panic!("the waiter still proved reap");
        };
        assert_eq!(cleanup.errors.len(), 3);
        assert!(cleanup.errors.iter().any(|error| error.contains("interrupt failed")));
        assert!(cleanup.errors.iter().any(|error| error.contains("terminate failed")));
        assert!(cleanup.errors.iter().any(|error| error.contains("force_kill failed")));
        assert_eq!(
            fake.signal_calls(),
            vec!["interrupt", "terminate", "force_kill"]
        );
    }

    #[tokio::test(start_paused = true)]
    async fn unproven_reap_returns_lost_after_the_bounded_cleanup_budget() {
        let observed_start = tokio::time::Instant::now();
        let (supervisor, handle, fake, _output) = register_fake(FakeOwner::pending()).await;

        let outcome = supervisor
            .cancel(&handle.owner, &handle.session_id)
            .await
            .expect("cleanup failure should be a typed outcome");

        let ExecutionOutcome::Lost {
            last_known,
            cleanup,
            ..
        } = outcome
        else {
            panic!("unproven reap must be Lost");
        };
        assert_eq!(last_known.state, ProcessState::Lost);
        assert!(!cleanup.reaped);
        assert!(cleanup.errors.iter().any(|error| error.contains("not proven reaped")));
        assert_eq!(
            fake.signal_calls(),
            vec!["interrupt", "terminate", "force_kill"]
        );
        assert_eq!(fake.wait_call_count(), 1);
        assert_eq!(
            tokio::time::Instant::now(),
            observed_start + Duration::from_secs(5)
        );
    }

    #[tokio::test(start_paused = true)]
    async fn unproven_reap_lost_wakes_a_far_future_poll_without_an_exit_fact() {
        let observed_start = tokio::time::Instant::now();
        let (supervisor, handle, _fake, _output) = register_fake(FakeOwner::pending()).await;
        let polling_supervisor = supervisor.clone();
        let polling_handle = handle.clone();
        let poll = tokio::spawn(async move {
            polling_supervisor
                .poll(
                    &polling_handle.owner,
                    &polling_handle.session_id,
                    OutputCursor::START,
                    Instant::now() + Duration::from_secs(100),
                )
                .await
        });
        tokio::task::yield_now().await;

        let cancelled = supervisor
            .cancel(&handle.owner, &handle.session_id)
            .await
            .expect("cancel should return a terminal outcome");
        assert!(matches!(cancelled, ExecutionOutcome::Lost { .. }));
        let polled = poll
            .await
            .expect("poll task should join")
            .expect("poll should return terminal state");

        assert!(matches!(
            polled,
            PollResult::Finished(ExecutionOutcome::Lost { .. })
        ));
        assert_eq!(
            tokio::time::Instant::now(),
            observed_start + Duration::from_secs(5)
        );
    }

    #[tokio::test(start_paused = true)]
    async fn final_output_drain_never_extends_cancellation_past_five_seconds() {
        let observed_start = tokio::time::Instant::now();
        let fake = FakeOwner::reaps_after_signal(
            FakeSignal::ForceKill,
            Duration::from_millis(2_900),
            137,
        );
        let (supervisor, handle, fake, _output) = register_fake(fake).await;

        let outcome = supervisor
            .cancel(&handle.owner, &handle.session_id)
            .await
            .expect("cancel should return a terminal outcome");

        assert!(
            matches!(outcome, ExecutionOutcome::Cancelled { .. }),
            "unexpected outcome {outcome:?}; signals: {:?}; elapsed: {:?}",
            fake.signal_calls(),
            tokio::time::Instant::now() - observed_start,
        );
        assert!(tokio::time::Instant::now() <= observed_start + Duration::from_secs(5));
    }

    #[tokio::test(start_paused = true)]
    async fn cancel_budget_is_anchored_before_the_detached_driver_runs() {
        let request_started_at = tokio::time::Instant::now();
        let (supervisor, handle, _fake, _output) =
            register_fake(FakeOwner::exits_after(Duration::from_secs(6), 0)).await;
        let cancellation = supervisor.cancel(&handle.owner, &handle.session_id);
        tokio::pin!(cancellation);
        poll_once_pending(cancellation.as_mut()).await;

        tokio::time::advance(Duration::from_secs(6)).await;
        let outcome = cancellation
            .await
            .expect("cancel should return a terminal outcome");

        let ExecutionOutcome::Lost { cleanup, .. } = outcome else {
            panic!("reap observed after the request deadline must be Lost");
        };
        assert!(cleanup
            .errors
            .iter()
            .any(|error| error.contains("after cleanup deadline")));
        assert_eq!(
            tokio::time::Instant::now(),
            request_started_at + Duration::from_secs(6)
        );
    }

    #[tokio::test(start_paused = true)]
    async fn terminate_budget_is_anchored_before_the_detached_driver_runs() {
        let request_started_at = tokio::time::Instant::now();
        let (supervisor, handle, _fake, _output) =
            register_fake(FakeOwner::exits_after(Duration::from_secs(5), 0)).await;
        let termination = supervisor.terminate(&handle.owner, &handle.session_id);
        tokio::pin!(termination);
        poll_once_pending(termination.as_mut()).await;

        tokio::time::advance(Duration::from_secs(5)).await;
        let outcome = termination
            .await
            .expect("terminate should return a terminal outcome");

        let ExecutionOutcome::Lost { cleanup, .. } = outcome else {
            panic!("reap observed after the terminate deadline must be Lost");
        };
        assert!(cleanup
            .errors
            .iter()
            .any(|error| error.contains("after cleanup deadline")));
        assert_eq!(
            tokio::time::Instant::now(),
            request_started_at + Duration::from_secs(5)
        );
    }

    #[tokio::test(start_paused = true)]
    async fn stop_budget_includes_elapsed_session_resolution_time() {
        let (supervisor, handle, _fake, _output) = register_fake(FakeOwner::pending()).await;
        let session = supervisor
            .session(&handle.owner, &handle.session_id)
            .expect("registered session must exist");
        let request_started_at = tokio::time::Instant::now();
        tokio::time::advance(Duration::from_secs(1)).await;

        let outcome = super::stop_session(
            session.session_arc(),
            &[
                super::SignalStage::Interrupt,
                super::SignalStage::Terminate,
                super::SignalStage::ForceKill,
            ],
            request_started_at,
            super::StopCause::Cancelled,
        )
        .await;

        let ExecutionOutcome::Lost { cleanup, .. } = outcome else {
            panic!("pending process must be Lost at the request-anchored deadline");
        };
        assert_eq!(cleanup.elapsed, Duration::from_secs(5));
        assert_eq!(
            tokio::time::Instant::now(),
            request_started_at + Duration::from_secs(5)
        );
    }

    #[tokio::test(start_paused = true)]
    async fn delayed_driver_uses_a_timely_recorded_reap_fact() {
        let request_started_at = tokio::time::Instant::now();
        let (supervisor, handle, _fake, _output) = register_fake(FakeOwner::exits_after(
            Duration::from_millis(4_900),
            0,
        ))
        .await;
        let session = supervisor
            .session(&handle.owner, &handle.session_id)
            .expect("registered session must exist");
        assert!(matches!(session.begin_stop(), super::StopStart::Leader));
        let budget = super::StopBudget::new(
            request_started_at,
            &[
                super::SignalStage::Interrupt,
                super::SignalStage::Terminate,
                super::SignalStage::ForceKill,
            ],
            &session.policy,
        );

        tokio::time::advance(Duration::from_millis(4_900)).await;
        wait_for_test_condition(|| session.exit_observation().is_some()).await;
        let Some(ExitObservation::Reaped { observed_at, .. }) = session.exit_observation() else {
            panic!("waiter must publish the timely reap fact");
        };
        assert!(observed_at <= budget.cleanup_deadline);
        tokio::time::advance(Duration::from_millis(1_100)).await;

        let outcome = super::run_stop_escalation(
            &session,
            &budget,
            super::StopCause::Cancelled,
        )
        .await;
        assert!(matches!(outcome, ExecutionOutcome::Cancelled { .. }));
    }

    #[tokio::test(start_paused = true)]
    async fn delayed_driver_never_accepts_a_late_recorded_reap_fact() {
        let request_started_at = tokio::time::Instant::now();
        let (supervisor, handle, _fake, _output) = register_fake(FakeOwner::exits_after(
            Duration::from_millis(5_001),
            0,
        ))
        .await;
        let session = supervisor
            .session(&handle.owner, &handle.session_id)
            .expect("registered session must exist");
        assert!(matches!(session.begin_stop(), super::StopStart::Leader));
        let budget = super::StopBudget::new(
            request_started_at,
            &[
                super::SignalStage::Interrupt,
                super::SignalStage::Terminate,
                super::SignalStage::ForceKill,
            ],
            &session.policy,
        );

        tokio::time::advance(Duration::from_millis(5_001)).await;
        wait_for_test_condition(|| session.exit_observation().is_some()).await;
        let Some(ExitObservation::Reaped { observed_at, .. }) = session.exit_observation() else {
            panic!("waiter must publish the late reap fact");
        };
        assert!(observed_at > budget.cleanup_deadline);
        tokio::time::advance(Duration::from_millis(999)).await;

        let outcome = super::run_stop_escalation(
            &session,
            &budget,
            super::StopCause::Cancelled,
        )
        .await;
        let ExecutionOutcome::Lost { cleanup, .. } = outcome else {
            panic!("a late recorded reap fact must remain Lost");
        };
        assert!(
            cleanup.reaped,
            "cleanup must report the eventual reap fact even when it was too late"
        );
        assert!(cleanup
            .errors
            .iter()
            .any(|error| error == "reap was observed after cleanup deadline"));
    }

    #[tokio::test(start_paused = true)]
    async fn reap_published_before_lost_commit_is_classified_by_its_timely_fact() {
        let request_started_at = tokio::time::Instant::now();
        let (supervisor, handle, fake, _output) =
            register_fake(FakeOwner::controlled_reap(130)).await;
        let session = supervisor
            .session(&handle.owner, &handle.session_id)
            .expect("registered session must exist");
        let hook = Arc::new(super::LostCommitHook {
            reached: tokio::sync::Notify::new(),
            release: tokio::sync::Notify::new(),
        });
        *session
            .before_lost_commit
            .lock()
            .expect("lost commit hook lock should not be poisoned") = Some(hook.clone());
        let cancelling_supervisor = supervisor.clone();
        let cancelling_handle = handle.clone();
        let cancellation = tokio::spawn(async move {
            cancelling_supervisor
                .cancel(&cancelling_handle.owner, &cancelling_handle.session_id)
                .await
        });

        hook.reached.notified().await;
        assert_eq!(
            tokio::time::Instant::now(),
            request_started_at + Duration::from_secs(5)
        );
        fake.release_reap();
        wait_for_test_condition(|| session.exit_observation().is_some()).await;
        let Some(ExitObservation::Reaped { observed_at, .. }) = session.exit_observation() else {
            panic!("waiter must publish the controlled timely fact");
        };
        assert!(observed_at <= request_started_at + Duration::from_secs(5));
        hook.release.notify_one();

        let outcome = cancellation
            .await
            .expect("cancellation task should join")
            .expect("cancel should return a terminal outcome");
        assert!(matches!(outcome, ExecutionOutcome::Cancelled { .. }));
        assert_eq!(fake.wait_call_count(), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn late_reap_published_before_lost_commit_is_truthfully_lost() {
        let request_started_at = tokio::time::Instant::now();
        let (supervisor, handle, fake, _output) =
            register_fake(FakeOwner::controlled_reap(137)).await;
        let session = supervisor
            .session(&handle.owner, &handle.session_id)
            .expect("registered session must exist");
        let hook = Arc::new(super::LostCommitHook {
            reached: tokio::sync::Notify::new(),
            release: tokio::sync::Notify::new(),
        });
        *session
            .before_lost_commit
            .lock()
            .expect("lost commit hook lock should not be poisoned") = Some(hook.clone());
        let cancelling_supervisor = supervisor.clone();
        let cancelling_handle = handle.clone();
        let cancellation = tokio::spawn(async move {
            cancelling_supervisor
                .cancel(&cancelling_handle.owner, &cancelling_handle.session_id)
                .await
        });

        hook.reached.notified().await;
        tokio::time::advance(Duration::from_millis(1)).await;
        fake.release_reap();
        wait_for_test_condition(|| session.exit_observation().is_some()).await;
        let Some(ExitObservation::Reaped { observed_at, .. }) = session.exit_observation() else {
            panic!("waiter must publish the controlled late fact");
        };
        assert!(observed_at > request_started_at + Duration::from_secs(5));
        hook.release.notify_one();

        let outcome = cancellation
            .await
            .expect("cancellation task should join")
            .expect("cancel should return a terminal outcome");
        let ExecutionOutcome::Lost { cleanup, .. } = outcome else {
            panic!("late fact must remain Lost");
        };
        assert!(cleanup.reaped);
        assert!(cleanup
            .errors
            .iter()
            .any(|error| error == "reap was observed after cleanup deadline"));
        assert_eq!(fake.wait_call_count(), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn reap_published_after_lost_commit_does_not_rewrite_terminal_cleanup() {
        let (supervisor, handle, fake, _output) =
            register_fake(FakeOwner::controlled_reap(137)).await;

        let first = supervisor
            .cancel(&handle.owner, &handle.session_id)
            .await
            .expect("cancel should return a terminal outcome");
        let ExecutionOutcome::Lost { cleanup, .. } = &first else {
            panic!("unproven reap must commit Lost");
        };
        assert!(!cleanup.reaped);

        fake.release_reap();
        let session = supervisor
            .session(&handle.owner, &handle.session_id)
            .expect("registered session must exist");
        wait_for_test_condition(|| session.exit_observation().is_some()).await;
        let repeated = finished(
            supervisor
                .poll(
                    &handle.owner,
                    &handle.session_id,
                    OutputCursor::START,
                    Instant::now() + Duration::from_secs(10),
                )
                .await
                .expect("terminal poll should succeed"),
        );

        assert_eq!(repeated, first);
        let ExecutionOutcome::Lost { cleanup, .. } = repeated else {
            panic!("late publication must not rewrite immutable Lost");
        };
        assert!(!cleanup.reaped);
        assert_eq!(fake.wait_call_count(), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn dropping_the_cancel_future_does_not_abandon_owned_cleanup() {
        let (supervisor, handle, fake, _output) = register_fake(FakeOwner::pending()).await;
        let cancelling_supervisor = supervisor.clone();
        let cancelling_handle = handle.clone();
        let cancelling = tokio::spawn(async move {
            cancelling_supervisor
                .cancel(&cancelling_handle.owner, &cancelling_handle.session_id)
                .await
        });
        wait_for_test_condition(|| !fake.signal_calls().is_empty()).await;

        cancelling.abort();
        let _ = cancelling.await;
        tokio::time::sleep(Duration::from_secs(6)).await;
        tokio::task::yield_now().await;

        let snapshot = supervisor
            .status(&handle.owner, &handle.session_id)
            .await
            .expect("owned cleanup should remain observable");
        assert_eq!(snapshot.state, ProcessState::Lost);
        assert_eq!(
            fake.signal_calls(),
            vec!["interrupt", "terminate", "force_kill"]
        );
        assert_eq!(fake.wait_call_count(), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn terminate_starts_at_the_terminate_stage_and_returns_a_terminal_outcome() {
        let (supervisor, handle, fake, _output) =
            register_fake(FakeOwner::reaps_on(FakeSignal::Terminate, 143)).await;

        let outcome = supervisor
            .terminate(&handle.owner, &handle.session_id)
            .await
            .expect("terminate should succeed");

        assert!(matches!(outcome, ExecutionOutcome::Cancelled { .. }));
        assert_eq!(fake.signal_calls(), vec!["terminate"]);
        assert_eq!(fake.wait_call_count(), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn cancelled_terminal_poll_is_idempotent() {
        let (supervisor, handle, _fake, output) =
            register_fake(FakeOwner::reaps_on(FakeSignal::Interrupt, 130)).await;
        output.push(OutputStream::Stdout, b"cancel output");
        let cancelled = supervisor
            .cancel(&handle.owner, &handle.session_id)
            .await
            .expect("cancel should succeed");

        let polled = finished(
            supervisor
                .poll(
                    &handle.owner,
                    &handle.session_id,
                    OutputCursor::START,
                    Instant::now() + Duration::from_secs(10),
                )
                .await
                .expect("terminal poll should succeed"),
        );

        assert_eq!(cancelled, polled);
    }

    fn finished(result: PollResult) -> ExecutionOutcome {
        match result {
            PollResult::Finished(outcome) => outcome,
            PollResult::Running { .. } => panic!("expected a terminal poll result"),
        }
    }

    fn outcome_output(outcome: &ExecutionOutcome) -> &OutputSnapshot {
        match outcome {
            ExecutionOutcome::Exited { output, .. }
            | ExecutionOutcome::Cancelled { output, .. }
            | ExecutionOutcome::TimedOut { output, .. }
            | ExecutionOutcome::Lost { output, .. } => output,
            ExecutionOutcome::SpawnFailed(_) => {
                panic!("outcome has no output snapshot")
            }
        }
    }

    async fn wait_for_test_condition(mut condition: impl FnMut() -> bool) {
        for _ in 0..1_024 {
            if condition() {
                return;
            }
            tokio::task::yield_now().await;
        }
        panic!("test condition was not reached after 1024 scheduler yields");
    }

    async fn poll_once_pending<F>(mut future: std::pin::Pin<&mut F>)
    where
        F: Future,
    {
        future::poll_fn(|context| match future.as_mut().poll(context) {
            std::task::Poll::Pending => std::task::Poll::Ready(()),
            std::task::Poll::Ready(_) => panic!("future unexpectedly completed on its first poll"),
        })
        .await;
    }

    async fn register_fake(
        fake: FakeOwner,
    ) -> (
        Arc<ProcessSupervisor>,
        super::ExecutionHandle,
        Arc<FakeOwner>,
        Arc<OutputBuffer>,
    ) {
        register_fake_with_policy(fake, ExecutionPolicy::default()).await
    }

    async fn register_fake_with_policy(
        fake: FakeOwner,
        policy: ExecutionPolicy,
    ) -> (
        Arc<ProcessSupervisor>,
        super::ExecutionHandle,
        Arc<FakeOwner>,
        Arc<OutputBuffer>,
    ) {
        register_fake_with_config(fake, policy, SupervisorConfig::default()).await
    }

    async fn register_fake_with_config(
        fake: FakeOwner,
        policy: ExecutionPolicy,
        config: SupervisorConfig,
    ) -> (
        Arc<ProcessSupervisor>,
        super::ExecutionHandle,
        Arc<FakeOwner>,
        Arc<OutputBuffer>,
    ) {
        let fake = Arc::new(fake);
        let supervisor = ProcessSupervisor::new(config);
        let request = fake_request(policy);
        let output = Arc::new(OutputBuffer::new(request.policy.output_limit_bytes));
        let handle = supervisor
            .register_owned(request, fake.clone(), output.clone())
            .await
            .expect("fake process should register");
        tokio::task::yield_now().await;
        (supervisor, handle, fake, output)
    }

    fn fake_request(policy: ExecutionPolicy) -> NormalizedExecutionRequest {
        let cwd = std::env::current_dir().expect("current directory should exist");
        NormalizedExecutionRequest {
            owner: ExecutionOwner::new(uuid::Uuid::now_v7(), uuid::Uuid::now_v7()),
            command: CommandSpec::Program {
                program: "fake-program".into(),
                args: Vec::new(),
            },
            cwd: cwd.clone(),
            env: BTreeMap::new(),
            transport: Transport::Pipe,
            policy,
            capability: CapabilityPolicy {
                cwd_roots: vec![cwd],
                sandbox: SandboxPolicy::UnrestrictedLocalOwner,
                allow_hand_off: false,
            },
        }
    }
}

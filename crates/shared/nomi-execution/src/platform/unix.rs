use std::{
    collections::VecDeque,
    io,
    os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd},
    os::unix::process::CommandExt,
    process::{
        Child as StdChild, ChildStderr as StdChildStderr, ChildStdin as StdChildStdin,
        ChildStdout as StdChildStdout, Command as StdCommand, ExitStatus,
    },
    process::Stdio,
    sync::{Arc, Mutex, OnceLock, mpsc},
    time::{Duration, Instant},
};

use async_trait::async_trait;
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWriteExt},
    process::{ChildStderr, ChildStdin, ChildStdout},
    sync::watch,
    task::JoinHandle,
};

use super::unix_pty::{PtyMaster, PtyPair};

use super::{
    ExitFact, ProcessOwner, SpawnedPlatformProcess,
    unix_protocol::{
        Deadline, Frame, FrameKind, Nonce, ProtocolError, SeqPacketPair, recv_expected, recv_frame,
        send_frame,
    },
};
#[cfg(target_os = "linux")]
use super::linux_watchdog::{
    FAULT_NONE, WatchdogConfig, capture_close_upper_exclusive, capture_starttime, run_watchdog,
};
#[cfg(all(test, target_os = "linux"))]
use super::linux_watchdog::{
    FAULT_EXIT_AFTER_ACK, FAULT_EXIT_AFTER_COMMIT_BEFORE_COMMITTED, FAULT_EXIT_AFTER_COMMITTED,
    FAULT_EXIT_BEFORE_ACK, FAULT_EXIT_BEFORE_BOOT_READY, FAULT_EXIT_BEFORE_REGISTRATION,
    FAULT_FAIL_FINAL_GROUP_KILL_ONCE, FAULT_SKIP_FINAL_GROUP_KILL, FAULT_WITHHOLD_ACK,
    EXIT_FAULT_AFTER_COMMIT_BEFORE_COMMITTED, EXIT_FAULT_BEFORE_ACK,
    EXIT_FAULT_BEFORE_BOOT_READY,
};
#[cfg(target_os = "macos")]
use super::macos_watchdog::{FAULT_NONE, WatchdogConfig, run_watchdog};
#[cfg(all(test, target_os = "macos"))]
use super::macos_watchdog::{
    FAULT_EXIT_AFTER_ACK, FAULT_EXIT_AFTER_COMMIT_BEFORE_COMMITTED, FAULT_EXIT_AFTER_COMMITTED,
    FAULT_EXIT_BEFORE_ACK, FAULT_EXIT_BEFORE_BOOT_READY, FAULT_EXIT_BEFORE_REGISTRATION,
    FAULT_FAIL_FINAL_GROUP_KILL_ONCE, FAULT_WITHHOLD_ACK,
    EXIT_FAULT_AFTER_COMMIT_BEFORE_COMMITTED, EXIT_FAULT_BEFORE_ACK,
    EXIT_FAULT_BEFORE_BOOT_READY,
};
use crate::{
    CleanupReport, CommandSpec, ExecutionError, NormalizedExecutionRequest, OutputBuffer,
    OutputStream, SandboxPolicy, ShellKind, SpawnFailure,
};

const READ_BUFFER_BYTES: usize = 8 * 1024;
const POST_EXIT_READER_DRAIN: Duration = Duration::from_millis(100);
const SETUP_TIMEOUT: Duration = Duration::from_secs(5);
const WATCHDOG_QUIESCING_WAIT: Duration = Duration::from_millis(100);
const GROUP_ABSENCE_WAIT: Duration = Duration::from_millis(100);
const CLEANUP_RETRY_DELAY: Duration = Duration::from_millis(10);
const CLEANUP_RETRY_MAX: Duration = Duration::from_secs(1);
const CLEANUP_ERROR_RETRY_MAX: Duration = Duration::from_secs(30);
const CLEANUP_RELAY_BATCH: usize = 64;
static UNIX_SPAWN_GATE: Mutex<()> = Mutex::new(());
static CLEANUP_RELAY: OnceLock<mpsc::Sender<CleanupJob>> = OnceLock::new();

#[cfg(test)]
#[derive(Clone, Copy, Default)]
enum TestSpawnFault {
    WatchdogDiesBeforeBootReady,
    WatchdogDiesBeforeRegistration,
    WatchdogDiesBeforeAck,
    WatchdogDiesAfterAck,
    WatchdogDiesAfterCommitBeforeCommitted,
    WatchdogDiesAfterCommitted,
    WithholdAck,
    #[cfg(target_os = "linux")]
    SkipFinalGroupKill,
    FailFinalGroupKillOnce,
    #[default]
    None,
}

#[cfg(test)]
#[derive(Clone, Copy, Default)]
enum TestRegistrationFault {
    ShortFrame,
    WrongNonce,
    #[default]
    None,
}

#[cfg(test)]
#[derive(Clone, Default)]
struct TestSpawnAudit {
    watchdog_reaps: Arc<std::sync::atomic::AtomicUsize>,
    leader_reaps: Arc<std::sync::atomic::AtomicUsize>,
    group_signals: Arc<std::sync::atomic::AtomicUsize>,
    watchdog_pid: Arc<std::sync::atomic::AtomicI32>,
    watchdog_status: Arc<std::sync::atomic::AtomicI32>,
    leader_pid: Arc<std::sync::atomic::AtomicI32>,
    cleanup_attempts: Arc<std::sync::atomic::AtomicUsize>,
    cleanup_owned_transitions: Arc<std::sync::atomic::AtomicUsize>,
    cleanup_retirements: Arc<std::sync::atomic::AtomicUsize>,
    failure_frames: Arc<std::sync::atomic::AtomicUsize>,
}

#[cfg(test)]
#[derive(Clone)]
struct TestBlockingTransactionPause {
    entered: Arc<tokio::sync::Notify>,
    release: Arc<(std::sync::Mutex<bool>, std::sync::Condvar)>,
}

#[cfg(test)]
struct TestBlockingTransactionRelease {
    release: Arc<(std::sync::Mutex<bool>, std::sync::Condvar)>,
}

#[cfg(test)]
impl Drop for TestBlockingTransactionRelease {
    fn drop(&mut self) {
        let (released, condition) = &*self.release;
        let mut released = match released.lock() {
            Ok(released) => released,
            Err(poisoned) => poisoned.into_inner(),
        };
        *released = true;
        condition.notify_all();
    }
}

#[cfg(test)]
impl TestBlockingTransactionPause {
    fn new() -> Self {
        Self {
            entered: Arc::new(tokio::sync::Notify::new()),
            release: Arc::new((std::sync::Mutex::new(false), std::sync::Condvar::new())),
        }
    }

    fn release_guard(&self) -> TestBlockingTransactionRelease {
        TestBlockingTransactionRelease {
            release: Arc::clone(&self.release),
        }
    }

    async fn wait_until_entered(&self) {
        self.entered.notified().await;
    }

    fn block(&self) {
        self.entered.notify_one();
        let (released, condition) = &*self.release;
        let mut released = match released.lock() {
            Ok(released) => released,
            Err(poisoned) => poisoned.into_inner(),
        };
        while !*released {
            released = match condition.wait(released) {
                Ok(released) => released,
                Err(poisoned) => poisoned.into_inner(),
            };
        }
    }

    fn release(&self) {
        let (released, condition) = &*self.release;
        let mut released = match released.lock() {
            Ok(released) => released,
            Err(poisoned) => poisoned.into_inner(),
        };
        *released = true;
        condition.notify_all();
    }
}

#[cfg(test)]
#[derive(Clone)]
struct TestCleanupHold {
    released: Arc<std::sync::atomic::AtomicBool>,
    attempted: Arc<tokio::sync::Notify>,
}

#[cfg(test)]
struct TestCleanupRelease {
    released: Arc<std::sync::atomic::AtomicBool>,
}

#[cfg(test)]
struct TestNotifyOnDrop(Arc<tokio::sync::Notify>);

#[cfg(test)]
impl Drop for TestNotifyOnDrop {
    fn drop(&mut self) {
        self.0.notify_one();
    }
}

#[cfg(test)]
impl Drop for TestCleanupRelease {
    fn drop(&mut self) {
        self.released
            .store(true, std::sync::atomic::Ordering::Release);
    }
}

#[cfg(test)]
impl TestCleanupHold {
    fn new() -> Self {
        Self {
            released: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            attempted: Arc::new(tokio::sync::Notify::new()),
        }
    }

    fn release_guard(&self) -> TestCleanupRelease {
        TestCleanupRelease {
            released: Arc::clone(&self.released),
        }
    }

    async fn wait_until_attempted(&self) {
        self.attempted.notified().await;
    }

    fn should_defer(&self) -> bool {
        if self.released.load(std::sync::atomic::Ordering::Acquire) {
            false
        } else {
            self.attempted.notify_one();
            true
        }
    }

    fn release(&self) {
        self.released
            .store(true, std::sync::atomic::Ordering::Release);
    }
}

#[cfg(test)]
impl TestSpawnAudit {
    fn record_watchdog_reap(&self, status: libc::c_int) {
        self.watchdog_status
            .store(status, std::sync::atomic::Ordering::SeqCst);
        self.watchdog_reaps
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    }

    fn record_leader_reap(&self) {
        self.leader_reaps
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    }
}

#[cfg(test)]
#[derive(Clone)]
struct TestStartPause {
    entered: Arc<tokio::sync::Notify>,
    release: Arc<tokio::sync::Notify>,
}

#[derive(Clone, Default)]
struct SpawnOptions {
    #[cfg(test)]
    fault: TestSpawnFault,
    #[cfg(test)]
    audit: TestSpawnAudit,
    #[cfg(test)]
    setup_timeout: Option<Duration>,
    #[cfg(test)]
    async_wrap_failure: bool,
    #[cfg(test)]
    lifecycle_start_delay: Option<Duration>,
    #[cfg(test)]
    lifecycle_terminal_delay: Option<Duration>,
    #[cfg(test)]
    after_leader_reap_pause: Option<TestBlockingTransactionPause>,
    #[cfg(test)]
    start_pause: Option<TestStartPause>,
    #[cfg(test)]
    blocking_transaction_pause: Option<TestBlockingTransactionPause>,
    #[cfg(test)]
    blocking_start_pause: Option<TestBlockingTransactionPause>,
    #[cfg(test)]
    blocking_worker_finished: Option<Arc<tokio::sync::Notify>>,
    #[cfg(test)]
    lifecycle_failure_before_cleanup: bool,
    #[cfg(test)]
    cleanup_hold: Option<TestCleanupHold>,
    #[cfg(test)]
    registration_fault: TestRegistrationFault,
}

struct StartCancellationGuard {
    cancelled: Arc<std::sync::atomic::AtomicBool>,
    armed: bool,
}

impl StartCancellationGuard {
    fn new() -> Self {
        Self {
            cancelled: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            armed: true,
        }
    }

    fn worker_flag(&self) -> Arc<std::sync::atomic::AtomicBool> {
        Arc::clone(&self.cancelled)
    }

    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for StartCancellationGuard {
    fn drop(&mut self) {
        if self.armed {
            self.cancelled
                .store(true, std::sync::atomic::Ordering::Release);
        }
    }
}

pub(super) async fn spawn_pipe(
    request: NormalizedExecutionRequest,
    output: Arc<OutputBuffer>,
) -> Result<SpawnedPlatformProcess, ExecutionError> {
    spawn_pipe_inner(request, output, SpawnOptions::default()).await
}

pub(super) async fn spawn_pty(
    request: NormalizedExecutionRequest,
    output: Arc<OutputBuffer>,
    cols: u16,
    rows: u16,
) -> Result<SpawnedPlatformProcess, ExecutionError> {
    spawn_inner(
        request,
        output,
        SpawnOptions::default(),
        SpawnTransport::Pty { cols, rows },
    )
    .await
}

async fn spawn_pipe_inner(
    request: NormalizedExecutionRequest,
    output: Arc<OutputBuffer>,
    options: SpawnOptions,
) -> Result<SpawnedPlatformProcess, ExecutionError> {
    spawn_inner(request, output, options, SpawnTransport::Pipe).await
}

#[derive(Clone, Copy)]
enum SpawnTransport {
    Pipe,
    Pty { cols: u16, rows: u16 },
}

async fn spawn_inner(
    request: NormalizedExecutionRequest,
    output: Arc<OutputBuffer>,
    options: SpawnOptions,
    transport: SpawnTransport,
) -> Result<SpawnedPlatformProcess, ExecutionError> {
    enforce_sandbox(&request)?;

    #[cfg(test)]
    let setup_timeout = options.setup_timeout.unwrap_or(SETUP_TIMEOUT);
    #[cfg(not(test))]
    let setup_timeout = SETUP_TIMEOUT;
    #[cfg(test)]
    let async_wrap_failure = options.async_wrap_failure;
    #[cfg(test)]
    let start_pause = options.start_pause.clone();
    #[cfg(test)]
    let blocking_transaction_pause = options.blocking_transaction_pause.clone();
    #[cfg(test)]
    let blocking_start_pause = options.blocking_start_pause.clone();
    #[cfg(test)]
    let blocking_worker_finished = options.blocking_worker_finished.clone();
    let deadline = Deadline::after(setup_timeout).map_err(protocol_spawn_failed)?;
    let async_deadline = tokio::time::Instant::now() + setup_timeout;
    let mut cancellation = StartCancellationGuard::new();
    let worker_cancelled = cancellation.worker_flag();
    let transaction = tokio::task::spawn_blocking(move || {
        #[cfg(test)]
        let _finished = blocking_worker_finished.map(TestNotifyOnDrop);
        #[cfg(test)]
        if let Some(pause) = blocking_start_pause {
            pause.block();
        }
        ensure_setup_active(deadline, &worker_cancelled)?;
        let mut transaction =
            spawn_transaction(request, options, deadline, &worker_cancelled, transport)?;
        #[cfg(test)]
        if let Some(pause) = blocking_transaction_pause {
            pause.block();
        }
        if worker_cancelled.load(std::sync::atomic::Ordering::Acquire) {
            return Err(transaction.post_exec_failure(
                "start_cancelled_during_transaction",
                io::Error::new(
                    io::ErrorKind::Interrupted,
                    "start future was cancelled while the blocking transaction owned the process",
                ),
            ));
        }
        transaction.start_lifecycle()
    });
    let committed = tokio::time::timeout_at(async_deadline, transaction)
        .await
        .map_err(|_| {
            start_lost_message(
                "spawn_transaction_deadline",
                "Unix spawn transaction exceeded its single setup deadline".to_owned(),
            )
        })?
        .map_err(|error| start_lost_message("spawn transaction join failed", error.to_string()))??;
    cancellation.disarm();
    #[cfg(test)]
    if let Some(pause) = start_pause {
        pause.entered.notify_one();
        pause.release.notified().await;
    }
    let CommittedSpawn { pid, io, lifecycle } = committed;
    #[cfg(test)]
    if async_wrap_failure {
        lifecycle.shutdown();
        return Err(async_wrap_start_lost(io::Error::other(
            "injected async stdio wrap failure",
        )));
    }
    let (io, readers) = match io {
        CommittedIo::Pipe {
            stdin,
            stdout,
            stderr,
        } => {
            let stdin = match ChildStdin::from_std(stdin) {
                Ok(value) => value,
                Err(error) => {
                    lifecycle.shutdown();
                    return Err(async_wrap_start_lost(error));
                }
            };
            let stdout = match ChildStdout::from_std(stdout) {
                Ok(value) => value,
                Err(error) => {
                    lifecycle.shutdown();
                    return Err(async_wrap_start_lost(error));
                }
            };
            let stderr = match ChildStderr::from_std(stderr) {
                Ok(value) => value,
                Err(error) => {
                    lifecycle.shutdown();
                    return Err(async_wrap_start_lost(error));
                }
            };
            (
                UnixIo::Pipe(tokio::sync::Mutex::new(Some(stdin))),
                vec![
                    tokio::spawn(read_stream(stdout, OutputStream::Stdout, output.clone())),
                    tokio::spawn(read_stream(stderr, OutputStream::Stderr, output)),
                ],
            )
        }
        CommittedIo::Pty(master) => {
            let master = match master.into_async() {
                Ok(master) => Arc::new(master),
                Err(error) => {
                    lifecycle.shutdown();
                    return Err(async_wrap_start_lost(error));
                }
            };
            let reader = tokio::spawn(super::unix_pty::read_output(
                Arc::clone(&master),
                output,
            ));
            (
                UnixIo::Pty(master),
                vec![reader],
            )
        }
    };

    Ok(SpawnedPlatformProcess {
        owner: Arc::new(UnixOwner {
            pid,
            lifecycle,
            io,
            readers: Mutex::new(readers),
        }),
    })
}

fn async_wrap_start_lost(error: io::Error) -> ExecutionError {
    ExecutionError::StartLost {
        failure: SpawnFailure {
            code: "async_process_wrap_failed".to_owned(),
            message: error.to_string(),
        },
        last_known: None,
        cleanup: CleanupReport {
            force_kill_attempted: true,
            reaped: false,
            errors: vec!["exact cleanup remains owned by the lifecycle worker".to_owned()],
            ..CleanupReport::default()
        },
    }
}

struct CommittedSpawn {
    pid: u32,
    io: CommittedIo,
    lifecycle: LifecycleHandle,
}

enum CommittedIo {
    Pipe {
        stdin: StdChildStdin,
        stdout: StdChildStdout,
        stderr: StdChildStderr,
    },
    Pty(PtyMaster),
}

#[derive(Clone)]
enum LifecycleCompletion {
    Running,
    Reaped(ExitFact),
    Failed {
        kind: io::ErrorKind,
        message: Arc<str>,
    },
}

struct LifecycleHandle {
    pgid: libc::pid_t,
    signal_gate: Arc<Mutex<SignalGate>>,
    completion: watch::Receiver<LifecycleCompletion>,
    #[cfg(test)]
    audit: TestSpawnAudit,
}

impl LifecycleHandle {
    fn shutdown(&self) {
        let mut gate = match self.signal_gate.lock() {
            Ok(gate) => gate,
            Err(poisoned) => poisoned.into_inner(),
        };
        if gate.phase != SignalPhase::Open {
            return;
        }
        gate.phase = SignalPhase::Closing;
        if !gate.final_kill_sent {
            // SAFETY: the worker still owns an unreaped leader/watchdog identity.
            let result = unsafe { libc::kill(-self.pgid, libc::SIGKILL) };
            if result == 0
                || io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH)
            {
                gate.final_kill_sent = true;
            }
            #[cfg(test)]
            self.audit
                .group_signals
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        }
        if let Some(control_fd) = gate.control_fd {
            // SAFETY: the worker cannot drop/reuse this descriptor while the shared
            // signal gate is locked.
            let _ = unsafe { libc::shutdown(control_fd, libc::SHUT_RDWR) };
        }
    }
}

impl Drop for LifecycleHandle {
    fn drop(&mut self) {
        self.shutdown();
    }
}

struct SpawnTransaction {
    child: Option<StdChild>,
    io: Option<TransactionIo>,
    watchdog_pid: Option<libc::pid_t>,
    control: Option<OwnedFd>,
    pgid: Option<libc::pid_t>,
    nonce: Nonce,
    cleanup_relay: mpsc::Sender<CleanupJob>,
    #[cfg(test)]
    lifecycle_start_delay: Option<Duration>,
    #[cfg(test)]
    lifecycle_terminal_delay: Option<Duration>,
    #[cfg(test)]
    after_leader_reap_pause: Option<TestBlockingTransactionPause>,
    #[cfg(test)]
    lifecycle_failure_before_cleanup: bool,
    #[cfg(test)]
    cleanup_hold: Option<TestCleanupHold>,
    disarmed: bool,
    #[cfg(test)]
    audit: TestSpawnAudit,
}

enum TransactionIo {
    Pipe,
    Pty(PtyMaster),
}

impl SpawnTransaction {
    fn reap_watchdog_before_setup_deadline(
        &mut self,
        deadline: Deadline,
    ) -> io::Result<libc::c_int> {
        let watchdog_pid = self
            .watchdog_pid
            .ok_or_else(|| io::Error::other("watchdog identity is missing"))?;
        let status = waitpid_exact_setup(watchdog_pid, deadline)?;
        self.watchdog_pid.take();
        #[cfg(test)]
        self.audit.record_watchdog_reap(status);
        Ok(status)
    }

    fn relay_owned(&mut self, signal_group: bool) -> CleanupReport {
        let mut cleanup = CleanupReport::default();
        cleanup.force_kill_attempted = signal_group && self.pgid.is_some();
        let job = CleanupJob {
            child: self.child.take(),
            watchdog_pid: self.watchdog_pid.take(),
            control: self.control.take(),
            pgid: self.pgid.take(),
            group_state: CleanupGroupState::new(signal_group),
            signal_gate: None,
            completion: None,
            failure_context: None,
            attempts: 0,
            last_error: None,
            watchdog_ownership_lost: false,
            leader_ownership_lost: false,
            retry_delay: CLEANUP_RETRY_DELAY,
            next_attempt: Instant::now(),
            absence_deadline: None,
            #[cfg(test)]
            audit: self.audit.clone(),
            #[cfg(test)]
            hold: self.cleanup_hold.clone(),
        };
        if let Err(error) = self.cleanup_relay.send(job) {
            cleanup
                .errors
                .push("cleanup relay unavailable; cleanup ran synchronously".to_owned());
            cleanup.reaped = error.0.run_to_completion();
        } else {
            cleanup
                .errors
                .push("exact cleanup transferred to durable relay".to_owned());
        }
        self.disarmed = true;
        cleanup
    }

    fn pre_exec_failure(
        &mut self,
        error: io::Error,
        deadline: Deadline,
    ) -> ExecutionError {
        self.control.take();
        match self.reap_watchdog_before_setup_deadline(deadline) {
            Ok(_) => {
                self.disarmed = true;
                spawn_failed(error)
            }
            Err(cleanup_error) => ExecutionError::StartLost {
                failure: SpawnFailure {
                    code: "spawn_cleanup_deferred".to_owned(),
                    message: error.to_string(),
                },
                last_known: None,
                cleanup: {
                    let mut cleanup = self.relay_owned(false);
                    cleanup
                        .errors
                        .push(format!("watchdog reap before setup deadline: {cleanup_error}"));
                    cleanup
                },
            },
        }
    }

    fn post_exec_failure(&mut self, code: &'static str, error: io::Error) -> ExecutionError {
        ExecutionError::StartLost {
            failure: SpawnFailure {
                code: code.to_owned(),
                message: error.to_string(),
            },
            last_known: None,
            cleanup: self.relay_owned(true),
        }
    }

    fn start_lifecycle(mut self) -> Result<CommittedSpawn, ExecutionError> {
        if self.child.is_none()
            || self.watchdog_pid.is_none()
            || self.control.is_none()
            || self.pgid.is_none()
        {
            return Err(self.post_exec_failure(
                "owner_transfer_failed",
                io::Error::other("committed ownership bundle is incomplete"),
            ));
        }
        let mut child = self
            .child
            .take()
            .expect("committed ownership bundle was validated");
        let pid = child.id();
        let io = match self.io.take() {
            Some(TransactionIo::Pipe) => {
                let stdin = child.stdin.take().ok_or_else(|| {
                    io::Error::other("committed Unix command is missing piped stdin")
                });
                let stdout = child.stdout.take().ok_or_else(|| {
                    io::Error::other("committed Unix command is missing piped stdout")
                });
                let stderr = child.stderr.take().ok_or_else(|| {
                    io::Error::other("committed Unix command is missing piped stderr")
                });
                match (stdin, stdout, stderr) {
                    (Ok(stdin), Ok(stdout), Ok(stderr)) => CommittedIo::Pipe {
                        stdin,
                        stdout,
                        stderr,
                    },
                    (stdin, stdout, stderr) => {
                        self.child = Some(child);
                        let error = stdin
                            .err()
                            .or_else(|| stdout.err())
                            .or_else(|| stderr.err())
                            .expect("one committed stdio handle is missing");
                        return Err(self.post_exec_failure("owner_transfer_failed", error));
                    }
                }
            }
            Some(TransactionIo::Pty(master)) => {
                child.stdin.take();
                child.stdout.take();
                child.stderr.take();
                CommittedIo::Pty(master)
            }
            None => {
                self.child = Some(child);
                return Err(self.post_exec_failure(
                    "owner_transfer_failed",
                    io::Error::other("committed Unix command transport is missing"),
                ));
            }
        };
        let watchdog_pid = self
            .watchdog_pid
            .take()
            .expect("committed ownership bundle was validated");
        let control = self
            .control
            .take()
            .expect("committed ownership bundle was validated");
        let pgid = self
            .pgid
            .take()
            .expect("committed ownership bundle was validated");
        let signal_gate = Arc::new(Mutex::new(SignalGate {
            phase: SignalPhase::Open,
            final_kill_sent: false,
            control_fd: Some(control.as_raw_fd()),
        }));
        let (completion_sender, completion) = watch::channel(LifecycleCompletion::Running);
        let job = LifecycleJob {
            child: Some(child),
            watchdog_pid: Some(watchdog_pid),
            control: Some(control),
            pgid,
            nonce: self.nonce,
            signal_gate: Arc::clone(&signal_gate),
            completion: Some(completion_sender),
            failure_context: None,
            cleanup_relay: self.cleanup_relay.clone(),
            #[cfg(test)]
            start_delay: self.lifecycle_start_delay,
            #[cfg(test)]
            terminal_delay: self.lifecycle_terminal_delay,
            #[cfg(test)]
            after_leader_reap_pause: self.after_leader_reap_pause.clone(),
            #[cfg(test)]
            fail_before_cleanup: self.lifecycle_failure_before_cleanup,
            #[cfg(test)]
            cleanup_hold: self.cleanup_hold.clone(),
            #[cfg(test)]
            audit: self.audit.clone(),
        };
        let launch_cell = Arc::new(Mutex::new(Some(job)));
        let worker_cell = Arc::clone(&launch_cell);
        let spawned = std::thread::Builder::new()
            .name(format!("nomifun-unix-lifecycle-{pid}"))
            .spawn(move || {
                let job = match worker_cell.lock() {
                    Ok(mut cell) => cell.take(),
                    Err(poisoned) => poisoned.into_inner().take(),
                };
                if let Some(job) = job {
                    job.run();
                }
            });
        if let Err(error) = spawned {
            let job = match launch_cell.lock() {
                Ok(mut cell) => cell.take(),
                Err(poisoned) => poisoned.into_inner().take(),
            };
            if let Some(mut job) = job {
                self.child = job.child.take();
                self.watchdog_pid = job.watchdog_pid.take();
                self.control = job.control.take();
                self.pgid = Some(job.pgid);
            }
            return Err(self.post_exec_failure("lifecycle_worker_spawn_failed", error));
        }
        self.disarmed = true;
        Ok(CommittedSpawn {
            pid,
            io,
            lifecycle: LifecycleHandle {
                pgid,
                signal_gate,
                completion,
                #[cfg(test)]
                audit: self.audit.clone(),
            },
        })
    }
}

impl Drop for SpawnTransaction {
    fn drop(&mut self) {
        if !self.disarmed {
            let _ = self.relay_owned(self.pgid.is_some());
        }
    }
}

struct CleanupJob {
    child: Option<StdChild>,
    watchdog_pid: Option<libc::pid_t>,
    control: Option<OwnedFd>,
    pgid: Option<libc::pid_t>,
    group_state: CleanupGroupState,
    signal_gate: Option<Arc<Mutex<SignalGate>>>,
    completion: Option<watch::Sender<LifecycleCompletion>>,
    failure_context: Option<(io::ErrorKind, Arc<str>)>,
    attempts: u64,
    last_error: Option<String>,
    watchdog_ownership_lost: bool,
    leader_ownership_lost: bool,
    retry_delay: Duration,
    next_attempt: Instant,
    absence_deadline: Option<Instant>,
    #[cfg(test)]
    audit: TestSpawnAudit,
    #[cfg(test)]
    hold: Option<TestCleanupHold>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CleanupGroupState {
    NotRequired,
    Pending,
    Sealed,
    Unsafe,
}

enum CleanupStep {
    Retry(CleanupJob),
    Finished { exact: bool },
}

impl CleanupGroupState {
    fn new(required: bool) -> Self {
        if required {
            Self::Pending
        } else {
            Self::NotRequired
        }
    }
}

impl CleanupJob {
    fn run_to_completion(mut self) -> bool {
        loop {
            let wait = self.next_attempt.saturating_duration_since(Instant::now());
            if !wait.is_zero() {
                std::thread::sleep(wait);
            }
            match self.run_once() {
                CleanupStep::Retry(job) => {
                    self = job;
                }
                CleanupStep::Finished { exact } => return exact,
            }
        }
    }

    fn is_due(&self, now: Instant) -> bool {
        self.next_attempt <= now
    }

    fn schedule_retry(&mut self, progress: bool, persistent_error: bool) {
        let maximum = if persistent_error {
            CLEANUP_ERROR_RETRY_MAX
        } else {
            CLEANUP_RETRY_MAX
        };
        self.retry_delay = if progress {
            CLEANUP_RETRY_DELAY
        } else {
            self.retry_delay
                .checked_mul(2)
                .unwrap_or(maximum)
                .min(maximum)
        };
        self.next_attempt = Instant::now()
            .checked_add(self.retry_delay)
            .unwrap_or_else(Instant::now);
    }

    fn validate_group_anchor(&mut self, errors: &mut Vec<String>) -> bool {
        if let Some(child) = self.child.as_ref()
            && !self.leader_ownership_lost
        {
            match leader_exit_observed(child.id() as libc::pid_t) {
                Ok(_) => return true,
                Err(error) if error.raw_os_error() == Some(libc::ECHILD) => {
                    self.leader_ownership_lost = true;
                    errors.push("leader exact ownership was lost before group sealing".to_owned());
                }
                Err(error) => errors.push(format!("leader anchor validation failed: {error}")),
            }
        }
        if let Some(watchdog_pid) = self.watchdog_pid
            && !self.watchdog_ownership_lost
        {
            match leader_exit_observed(watchdog_pid) {
                Ok(_) => return true,
                Err(error) if error.raw_os_error() == Some(libc::ECHILD) => {
                    self.watchdog_ownership_lost = true;
                    errors.push("watchdog exact ownership was lost before group sealing".to_owned());
                }
                Err(error) => errors.push(format!("watchdog anchor validation failed: {error}")),
            }
        }
        false
    }

    fn run_once(mut self) -> CleanupStep {
        debug_assert!(self.is_due(Instant::now()));
        self.control.take();
        self.attempts = self.attempts.saturating_add(1);
        #[cfg(test)]
        self.audit
            .cleanup_attempts
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        #[cfg(test)]
        if self.hold.as_ref().is_some_and(TestCleanupHold::should_defer) {
            self.last_error = Some("injected cleanup attempt remained unproven".to_owned());
            self.schedule_retry(false, false);
            return CleanupStep::Retry(self);
        }

        let mut errors = Vec::new();
        let before = (
            self.group_state,
            self.child.is_some(),
            self.watchdog_pid.is_some(),
        );
        if self.group_state == CleanupGroupState::Pending {
            if !self.validate_group_anchor(&mut errors) {
                let retryable_anchor = (self.child.is_some() && !self.leader_ownership_lost)
                    || (self.watchdog_pid.is_some() && !self.watchdog_ownership_lost);
                if !retryable_anchor {
                    self.group_state = CleanupGroupState::Unsafe;
                    errors.push(
                        "no exact identity remains safe for negative-PGID cleanup".to_owned(),
                    );
                }
            } else if let Some(pgid) = self.pgid {
                // SAFETY: validate_group_anchor just proved an unreaped exact direct child.
                let result = unsafe { libc::kill(-pgid, libc::SIGKILL) };
                #[cfg(test)]
                self.audit
                    .group_signals
                    .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                if result == 0 {
                    self.group_state = CleanupGroupState::Sealed;
                } else {
                    let error = io::Error::last_os_error();
                    if error.raw_os_error() == Some(libc::ESRCH) {
                        self.group_state = CleanupGroupState::Sealed;
                    } else {
                        errors.push(format!("group SIGKILL failed: {error}"));
                    }
                }
            } else {
                self.group_state = CleanupGroupState::Unsafe;
                errors.push("cleanup requires a group signal but has no PGID".to_owned());
            }
        }

        if matches!(
            self.group_state,
            CleanupGroupState::NotRequired | CleanupGroupState::Sealed
        ) {
            if let Some(watchdog_pid) = self.watchdog_pid
                && !self.watchdog_ownership_lost
            {
                match try_waitpid_exact(watchdog_pid) {
                    Ok(Some(status)) => {
                        self.watchdog_pid = None;
                        #[cfg(test)]
                        self.audit.record_watchdog_reap(status);
                        #[cfg(not(test))]
                        let _ = status;
                    }
                    Ok(None) => {
                        if self.group_state == CleanupGroupState::NotRequired {
                            // waitpid(WNOHANG) just proved this is still our unreaped child.
                            let kill_result = unsafe { libc::kill(watchdog_pid, libc::SIGKILL) };
                            if kill_result == -1 {
                                let error = io::Error::last_os_error();
                                if error.raw_os_error() != Some(libc::ESRCH) {
                                    errors.push(format!("watchdog SIGKILL failed: {error}"));
                                }
                            }
                        }
                    }
                    Err(error) if error.raw_os_error() == Some(libc::ECHILD) => {
                        self.watchdog_ownership_lost = true;
                        errors.push(
                            "watchdog exact ownership was lost; cached PID quarantined".to_owned(),
                        );
                    }
                    Err(error) => errors.push(format!("watchdog exact reap failed: {error}")),
                }
            }
            if let Some(child) = self.child.as_mut()
                && !self.leader_ownership_lost
            {
                match child.try_wait() {
                    Ok(Some(_)) => {
                        self.child = None;
                        #[cfg(test)]
                        self.audit.record_leader_reap();
                    }
                    Ok(None) if self.group_state == CleanupGroupState::NotRequired => {
                        match child.kill() {
                            Ok(()) => {}
                            Err(error) if error.kind() == io::ErrorKind::InvalidInput => {}
                            Err(error) => errors.push(format!("leader SIGKILL failed: {error}")),
                        }
                    }
                    Ok(None) => {}
                    Err(error) if error.raw_os_error() == Some(libc::ECHILD) => {
                        self.leader_ownership_lost = true;
                        errors.push(
                            "leader exact ownership was lost; cached identity quarantined"
                                .to_owned(),
                        );
                    }
                    Err(error) => errors.push(format!("leader exact reap failed: {error}")),
                }
            }
        }
        let ownership_lost = self.watchdog_ownership_lost || self.leader_ownership_lost;
        if self.watchdog_ownership_lost {
            self.watchdog_pid.take();
        }
        if self.leader_ownership_lost {
            self.child.take();
        }
        let direct_identities_reaped =
            self.child.is_none() && self.watchdog_pid.is_none() && !ownership_lost;
        let mut group_absent = self.group_state == CleanupGroupState::NotRequired;
        let mut containment_lost = false;
        if direct_identities_reaped && self.group_state == CleanupGroupState::Sealed {
            if let Some(pgid) = self.pgid {
                match probe_group_absent_once(pgid) {
                    Ok(true) => group_absent = true,
                    Ok(false) => {
                        let absence_deadline = self.absence_deadline.get_or_insert_with(|| {
                            Instant::now()
                                .checked_add(GROUP_ABSENCE_WAIT)
                                .unwrap_or_else(Instant::now)
                        });
                        if Instant::now() >= *absence_deadline {
                            containment_lost = true;
                            errors.push(
                                "process group still exists after relay exact reaps".to_owned(),
                            );
                        } else {
                            errors.push("process group absence is not yet proven".to_owned());
                        }
                    }
                    Err(error) => {
                        containment_lost = true;
                        errors.push(format!(
                            "process group absence is unproven after relay exact reaps: {error}"
                        ));
                    }
                }
            } else {
                containment_lost = true;
                errors.push("relay exact reaps completed without a PGID proof".to_owned());
            }
        }
        let exact_cleanup = direct_identities_reaped && group_absent;
        let lost_cleanup_terminal = ownership_lost
            && self.child.is_none()
            && self.watchdog_pid.is_none()
            && self.group_state != CleanupGroupState::Pending;
        let unproven_terminal = lost_cleanup_terminal || containment_lost;
        if unproven_terminal {
            self.pgid.take();
        }
        if let Some(signal_gate) = self.signal_gate.as_ref() {
            let mut gate = match signal_gate.lock() {
                Ok(gate) => gate,
                Err(poisoned) => poisoned.into_inner(),
            };
            gate.control_fd = None;
            if exact_cleanup || unproven_terminal {
                gate.phase = SignalPhase::Retired;
                #[cfg(test)]
                self.audit
                    .cleanup_retirements
                    .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            }
        }
        if exact_cleanup || unproven_terminal {
            if let Some(completion) = self.completion.take() {
                let diagnostics = if errors.is_empty() {
                    self.last_error.as_deref().unwrap_or("none").to_owned()
                } else {
                    errors.join("; ")
                };
                let message = if ownership_lost {
                    format!(
                        "lifecycle cleanup is unproven because exact child ownership was lost; cached identities were quarantined; last diagnostic: {diagnostics}"
                    )
                } else if containment_lost {
                    format!(
                        "lifecycle cleanup is unproven because process-group absence could not be established; last diagnostic: {diagnostics}"
                    )
                } else if self.attempts == 1 && diagnostics == "none" {
                    "lifecycle failed; exact cleanup completed on the durable relay".to_owned()
                } else {
                    format!(
                        "lifecycle failed; exact cleanup completed after {} relay attempts; last diagnostic: {diagnostics}",
                        self.attempts
                    )
                };
                let (kind, original) = self.failure_context.take().unwrap_or_else(|| {
                    (
                        io::ErrorKind::Other,
                        Arc::<str>::from("lifecycle failed before exact cleanup"),
                    )
                });
                completion.send_replace(LifecycleCompletion::Failed {
                    kind,
                    message: format!("{original}; {message}").into(),
                });
            }
            CleanupStep::Finished {
                exact: exact_cleanup,
            }
        } else {
            self.last_error = Some(if errors.is_empty() {
                "exact child identities are still exiting".to_owned()
            } else {
                errors.join("; ")
            });
            let after = (
                self.group_state,
                self.child.is_some(),
                self.watchdog_pid.is_some(),
            );
            let persistent_error = self.group_state == CleanupGroupState::Unsafe
                || self.watchdog_ownership_lost
                || self.leader_ownership_lost
                || !errors.is_empty();
            self.schedule_retry(before != after, persistent_error);
            CleanupStep::Retry(self)
        }
    }
}

fn cleanup_relay_sender() -> io::Result<mpsc::Sender<CleanupJob>> {
    if let Some(sender) = CLEANUP_RELAY.get() {
        return Ok(sender.clone());
    }
    let (sender, receiver) = mpsc::channel::<CleanupJob>();
    std::thread::Builder::new()
        .name("nomifun-unix-cleanup-relay".to_owned())
        .spawn(move || run_cleanup_relay(receiver))?;
    if CLEANUP_RELAY.set(sender.clone()).is_ok() {
        Ok(sender)
    } else {
        CLEANUP_RELAY
            .get()
            .cloned()
            .ok_or_else(|| io::Error::other("cleanup relay initialization raced"))
    }
}

fn run_cleanup_relay(receiver: mpsc::Receiver<CleanupJob>) {
    let mut pending = VecDeque::new();
    let mut disconnected = false;
    loop {
        if pending.is_empty() && !disconnected {
            match receiver.recv() {
                Ok(job) => pending.push_back(job),
                Err(_) => return,
            }
        }
        let now = Instant::now();
        let round_len = pending.len().min(CLEANUP_RELAY_BATCH);
        for _ in 0..round_len {
            let job = pending
                .pop_front()
                .expect("cleanup relay round length matches its queue");
            if !job.is_due(now) {
                pending.push_back(job);
            } else {
                match job.run_once() {
                    CleanupStep::Retry(job) => pending.push_back(job),
                    CleanupStep::Finished { .. } => {}
                }
            }
        }
        for _ in 0..CLEANUP_RELAY_BATCH {
            if disconnected {
                break;
            }
            match receiver.try_recv() {
                Ok(job) => pending.push_back(job),
                Err(mpsc::TryRecvError::Empty) => break,
                Err(mpsc::TryRecvError::Disconnected) => {
                    disconnected = true;
                    break;
                }
            }
        }
        if pending.is_empty() {
            if disconnected {
                return;
            }
            continue;
        }

        let wait = pending
            .iter()
            .map(|job| job.next_attempt)
            .min()
            .unwrap_or_else(Instant::now)
            .saturating_duration_since(Instant::now());
        if wait.is_zero() {
            continue;
        }
        if disconnected {
            let milliseconds = wait.as_millis().clamp(1, libc::c_int::MAX as u128);
            let _ = poll_delay(milliseconds as libc::c_int);
        } else {
            match receiver.recv_timeout(wait) {
                Ok(job) => pending.push_back(job),
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => disconnected = true,
            }
        }
    }
}

struct WatchdogNullGuard {
    descriptors: Vec<OwnedFd>,
}

impl WatchdogNullGuard {
    fn open() -> io::Result<Self> {
        let mut descriptors = Vec::with_capacity(4);
        loop {
            // SAFETY: the path is a static C string and the returned descriptor is
            // immediately adopted by OwnedFd.
            let descriptor = unsafe {
                libc::open(c"/dev/null".as_ptr(), libc::O_RDWR | libc::O_CLOEXEC)
            };
            if descriptor < 0 {
                return Err(io::Error::last_os_error());
            }
            // SAFETY: open returned a fresh owned descriptor.
            descriptors.push(unsafe { OwnedFd::from_raw_fd(descriptor) });
            if descriptor >= 3 {
                return Ok(Self { descriptors });
            }
        }
    }

    fn null_fd(&self) -> RawFd {
        self.descriptors
            .last()
            .expect("watchdog null guard always owns one descriptor")
            .as_raw_fd()
    }
}

fn ensure_setup_active(
    deadline: Deadline,
    cancelled: &std::sync::atomic::AtomicBool,
) -> Result<(), ExecutionError> {
    if cancelled.load(std::sync::atomic::Ordering::Acquire) {
        return Err(spawn_failed(io::Error::new(
            io::ErrorKind::Interrupted,
            "Unix spawn transaction was cancelled before ownership setup",
        )));
    }
    if deadline.is_expired().map_err(protocol_spawn_failed)? {
        return Err(spawn_failed(io::Error::new(
            io::ErrorKind::TimedOut,
            "Unix spawn transaction exceeded its shared setup deadline",
        )));
    }
    Ok(())
}

fn lock_spawn_gate(
    deadline: Deadline,
    cancelled: &std::sync::atomic::AtomicBool,
) -> Result<std::sync::MutexGuard<'static, ()>, ExecutionError> {
    loop {
        ensure_setup_active(deadline, cancelled)?;
        match UNIX_SPAWN_GATE.try_lock() {
            Ok(gate) => {
                ensure_setup_active(deadline, cancelled)?;
                return Ok(gate);
            }
            Err(std::sync::TryLockError::Poisoned(_)) => {
                return Err(spawn_failed(io::Error::other(
                    "Unix spawn gate is poisoned",
                )));
            }
            Err(std::sync::TryLockError::WouldBlock) => {
                if deadline.is_expired().map_err(protocol_spawn_failed)? {
                    return Err(spawn_failed(io::Error::new(
                        io::ErrorKind::TimedOut,
                        "Unix spawn gate exceeded the shared setup deadline",
                    )));
                }
                poll_delay(2).map_err(spawn_failed)?;
            }
        }
    }
}

fn spawn_transaction(
    request: NormalizedExecutionRequest,
    options: SpawnOptions,
    deadline: Deadline,
    cancelled: &std::sync::atomic::AtomicBool,
    transport: SpawnTransport,
) -> Result<SpawnTransaction, ExecutionError> {
    let _gate = lock_spawn_gate(deadline, cancelled)?;
    let cleanup_relay = cleanup_relay_sender().map_err(spawn_failed)?;
    let nonce = Nonce::new(uuid::Uuid::now_v7().into_bytes());
    let parent_pid = std::process::id() as libc::pid_t;
    #[cfg(target_os = "linux")]
    let parent_starttime = capture_starttime(parent_pid).map_err(spawn_failed)?;
    #[cfg(target_os = "macos")]
    let parent_starttime = 0;
    // Keep every stdio slot occupied while the protocol sockets are created, so
    // their descriptors are always outside the watchdog's stdio rewrite range.
    let watchdog_null = WatchdogNullGuard::open().map_err(spawn_failed)?;
    let (control_host, control_watchdog) = SeqPacketPair::new()
        .map_err(protocol_spawn_failed)?
        .into_fds();
    let (registration_child, registration_watchdog) = SeqPacketPair::new()
        .map_err(protocol_spawn_failed)?
        .into_fds();
    #[cfg(target_os = "linux")]
    let close_upper_exclusive = capture_close_upper_exclusive().map_err(spawn_failed)?;
    let pty = match transport {
        SpawnTransport::Pipe => None,
        SpawnTransport::Pty { cols, rows } => {
            Some(PtyPair::open(cols, rows).map_err(spawn_failed)?)
        }
    };
    // Prepare every fallible slave duplication before the watchdog fork, so a
    // descriptor-allocation failure cannot leave an unproven direct child.
    let pty_child_stdio = pty
        .as_ref()
        .map(PtyPair::child_stdio)
        .transpose()
        .map_err(spawn_failed)?;
    ensure_setup_active(deadline, cancelled)?;

    // SAFETY: the child branch immediately enters the raw watchdog and never unwinds.
    let watchdog_pid = unsafe { libc::fork() };
    if watchdog_pid < 0 {
        return Err(spawn_failed(io::Error::last_os_error()));
    }
    if watchdog_pid == 0 {
        let config = WatchdogConfig {
            parent_pid,
            parent_starttime,
            control_fd: control_watchdog.as_raw_fd(),
            registration_fd: registration_watchdog.as_raw_fd(),
            null_fd: watchdog_null.null_fd(),
            #[cfg(target_os = "linux")]
            close_upper_exclusive,
            external_session: matches!(transport, SpawnTransport::Pty { .. }),
            nonce,
            deadline,
            fault: watchdog_fault(&options),
        };
        // SAFETY: this is the dedicated fork child and run_watchdog never returns.
        unsafe { run_watchdog(config) };
    }
    drop(watchdog_null);
    #[cfg(test)]
    options
        .audit
        .watchdog_pid
        .store(watchdog_pid, std::sync::atomic::Ordering::SeqCst);
    drop(control_watchdog);
    drop(registration_watchdog);

    let mut transaction = SpawnTransaction {
        child: None,
        io: None,
        watchdog_pid: Some(watchdog_pid),
        control: Some(control_host),
        pgid: None,
        nonce,
        cleanup_relay,
        #[cfg(test)]
        lifecycle_start_delay: options.lifecycle_start_delay,
        #[cfg(test)]
        lifecycle_terminal_delay: options.lifecycle_terminal_delay,
        #[cfg(test)]
        after_leader_reap_pause: options.after_leader_reap_pause.clone(),
        #[cfg(test)]
        lifecycle_failure_before_cleanup: options.lifecycle_failure_before_cleanup,
        #[cfg(test)]
        cleanup_hold: options.cleanup_hold.clone(),
        disarmed: false,
        #[cfg(test)]
        audit: options.audit.clone(),
    };
    let control_fd = transaction
        .control
        .as_ref()
        .expect("transaction control initialized")
        .as_raw_fd();
    if let Err(error) = recv_expected(control_fd, nonce, FrameKind::BootReady, deadline)
        .and_then(|frame| validate_frame_identity(frame, 0, 0))
    {
        return Err(transaction.pre_exec_failure(protocol_io_error(error), deadline));
    }
    if let Err(error) = ensure_setup_active(deadline, cancelled) {
        return Err(transaction.pre_exec_failure(
            io::Error::new(io::ErrorKind::TimedOut, error.to_string()),
            deadline,
        ));
    }

    let registration_fd = registration_child.as_raw_fd();
    #[cfg(test)]
    let registration_fault = options.registration_fault;
    let mut command = std_command_for(&request.command);
    command
        .args(command_args(&request.command))
        .current_dir(&request.cwd)
        .envs(&request.env);
    let pty_slave_fd = pty.as_ref().map(PtyPair::slave_fd);
    let pty_master_fd = pty.as_ref().map(PtyPair::master_fd);
    match pty_child_stdio {
        None => {
            command
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped());
        }
        Some(stdio) => {
            command
                .stdin(stdio.stdin)
                .stdout(stdio.stdout)
                .stderr(stdio.stderr);
        }
    }
    // SAFETY: child_bootstrap uses only raw protocol and process/session syscalls.
    unsafe {
        command.pre_exec(move || {
            child_bootstrap(
                control_fd,
                registration_fd,
                nonce,
                deadline,
                pty_slave_fd,
                pty_master_fd,
                #[cfg(test)]
                registration_fault,
            )
        });
    }
    let spawned = command.spawn();
    drop(registration_child);
    let child = match spawned {
        Ok(child) => child,
        Err(error) => {
            let abort = Frame::new(FrameKind::Abort, nonce, 0, 0);
            let _ = send_frame(control_fd, &abort, deadline);
            return Err(transaction.pre_exec_failure(error, deadline));
        }
    };
    let transaction_io = match pty {
        Some(pty) => TransactionIo::Pty(pty.into_master()),
        None => TransactionIo::Pipe,
    };
    let pid = child.id() as libc::pid_t;
    #[cfg(test)]
    options
        .audit
        .leader_pid
        .store(pid, std::sync::atomic::Ordering::SeqCst);
    transaction.pgid = Some(pid);
    transaction.child = Some(child);
    transaction.io = Some(transaction_io);
    let commit = Frame::new(FrameKind::Commit, nonce, pid, pid);
    if let Err(error) = send_frame(control_fd, &commit, deadline)
        .and_then(|_| recv_expected(control_fd, nonce, FrameKind::Committed, deadline))
        .and_then(|frame| validate_frame_identity(frame, pid, pid).map(drop))
    {
        return Err(transaction.post_exec_failure(
            "ownership_commit_failed",
            protocol_io_error(error),
        ));
    }
    Ok(transaction)
}

fn validate_frame_identity(
    frame: Frame,
    expected_pid: libc::pid_t,
    expected_pgid: libc::pid_t,
) -> Result<Frame, ProtocolError> {
    if frame.pid() == expected_pid && frame.pgid() == expected_pgid {
        Ok(frame)
    } else {
        Err(ProtocolError::MalformedFrame)
    }
}

fn child_bootstrap(
    control_fd: libc::c_int,
    registration_fd: libc::c_int,
    nonce: Nonce,
    deadline: Deadline,
    pty_slave_fd: Option<RawFd>,
    pty_master_fd: Option<RawFd>,
    #[cfg(test)] registration_fault: TestRegistrationFault,
) -> io::Result<()> {
    // SAFETY: these descriptors are valid inherited protocol endpoints.
    unsafe { libc::close(control_fd) };
    if let Some(master_fd) = pty_master_fd {
        // The user child uses only the slave. Close the inherited master copy
        // before ACK so it cannot keep the PTY alive after the host closes it.
        unsafe { libc::close(master_fd) };
    }
    match pty_slave_fd {
        Some(slave_fd) => {
            // SAFETY: the helper performs only async-signal-safe syscalls and
            // returns a raw errno without allocating or consulting TLS.
            let errno = unsafe { bootstrap_controlling_terminal(slave_fd) };
            if errno != 0 {
                return Err(io::Error::from_raw_os_error(errno));
            }
        }
        None => {
            // Pipe children remain process-group leaders in the host session.
            if unsafe { libc::setpgid(0, 0) } == -1 {
                return Err(io::Error::last_os_error());
            }
        }
    }
    let pid = unsafe { libc::getpid() };
    #[cfg(test)]
    match registration_fault {
        TestRegistrationFault::ShortFrame => {
            let byte = [0_u8; 1];
            loop {
                // SAFETY: the byte is stack-owned and registration_fd is inherited.
                let sent = unsafe {
                    libc::send(
                        registration_fd,
                        byte.as_ptr().cast(),
                        byte.len(),
                        libc::MSG_NOSIGNAL,
                    )
                };
                if sent == byte.len() as libc::ssize_t {
                    return Err(io::Error::from_raw_os_error(libc::EPROTO));
                }
                let error = io::Error::last_os_error();
                if error.raw_os_error() != Some(libc::EINTR) {
                    return Err(error);
                }
            }
        }
        TestRegistrationFault::WrongNonce => {
            let mut bytes = nonce.as_bytes();
            bytes[0] ^= 0xff;
            let registration = Frame::new(FrameKind::Register, Nonce::new(bytes), pid, pid);
            send_frame(registration_fd, &registration, deadline)
                .map_err(|error| io::Error::from_raw_os_error(error.raw_errno()))?;
            return Err(io::Error::from_raw_os_error(libc::EPROTO));
        }
        TestRegistrationFault::None => {}
    }
    let registration = Frame::new(FrameKind::Register, nonce, pid, pid);
    send_frame(registration_fd, &registration, deadline)
        .map_err(|error| io::Error::from_raw_os_error(error.raw_errno()))?;
    let ack = recv_expected(registration_fd, nonce, FrameKind::Ack, deadline)
        .map_err(|error| io::Error::from_raw_os_error(error.raw_errno()))?;
    if ack.pid() != pid || ack.pgid() != pid || ack.nonce() != nonce {
        return Err(io::Error::from_raw_os_error(libc::EPROTO));
    }
    unsafe { libc::close(registration_fd) };
    Ok(())
}

/// Establishes a truthful controlling terminal in the post-fork child.
///
/// Returns zero on success or a raw errno on failure. This intentionally avoids
/// `io::Error::last_os_error`, formatting, allocation and unwinding between
/// `fork` and `exec`.
unsafe fn bootstrap_controlling_terminal(slave_fd: RawFd) -> libc::c_int {
    // A controlling terminal requires a fresh session. `setsid` also makes the
    // child the leader of a new process group with pgid=pid.
    if unsafe { libc::setsid() } == -1 {
        return unsafe { raw_errno() };
    }
    if unsafe { libc::ioctl(slave_fd, libc::TIOCSCTTY as _, 0) } == -1 {
        return unsafe { raw_errno() };
    }
    let pid = unsafe { libc::getpid() };
    if unsafe { libc::getsid(0) } != pid
        || unsafe { libc::getpgrp() } != pid
        || unsafe { libc::tcgetpgrp(slave_fd) } != pid
    {
        return libc::EPROTO;
    }
    0
}

#[cfg(target_os = "linux")]
unsafe fn raw_errno() -> libc::c_int {
    // SAFETY: libc exposes the current thread's errno slot.
    unsafe { *libc::__errno_location() }
}

#[cfg(target_os = "macos")]
unsafe fn raw_errno() -> libc::c_int {
    // SAFETY: libc exposes the current thread's errno slot.
    unsafe { *libc::__error() }
}

fn waitpid_exact_setup(pid: libc::pid_t, deadline: Deadline) -> io::Result<libc::c_int> {
    loop {
        let mut status = 0;
        // SAFETY: pid names the exact direct-child watchdog; status is writable.
        let waited = unsafe { libc::waitpid(pid, &mut status, libc::WNOHANG) };
        if waited == pid {
            return Ok(status);
        }
        if waited < 0 {
            let error = io::Error::last_os_error();
            if error.raw_os_error() == Some(libc::EINTR) {
                continue;
            }
            return Err(error);
        }
        if deadline.is_expired().map_err(|error| {
            io::Error::from_raw_os_error(error.raw_errno())
        })? {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "watchdog reap exceeded the shared setup deadline",
            ));
        }
        std::thread::sleep(Duration::from_millis(2));
    }
}

fn waitpid_exact_blocking(pid: libc::pid_t) -> io::Result<libc::c_int> {
    loop {
        let mut status = 0;
        // SAFETY: pid names one exact direct child and status is writable.
        let waited = unsafe { libc::waitpid(pid, &mut status, 0) };
        if waited == pid {
            return Ok(status);
        }
        let error = io::Error::last_os_error();
        if error.raw_os_error() != Some(libc::EINTR) {
            return Err(error);
        }
    }
}

fn try_waitpid_exact(pid: libc::pid_t) -> io::Result<Option<libc::c_int>> {
    loop {
        let mut status = 0;
        // SAFETY: pid names the exact unreaped direct child and status is writable.
        let waited = unsafe { libc::waitpid(pid, &mut status, libc::WNOHANG) };
        if waited == pid {
            return Ok(Some(status));
        }
        if waited == 0 {
            return Ok(None);
        }
        let error = io::Error::last_os_error();
        if error.raw_os_error() != Some(libc::EINTR) {
            return Err(error);
        }
    }
}

fn protocol_spawn_failed(error: super::unix_protocol::ProtocolError) -> ExecutionError {
    spawn_failed(protocol_io_error(error))
}

fn protocol_io_error(error: super::unix_protocol::ProtocolError) -> io::Error {
    io::Error::other(format!("Unix ownership protocol: {error:?}"))
}

fn start_lost_message(code: &'static str, message: String) -> ExecutionError {
    ExecutionError::StartLost {
        failure: SpawnFailure {
            code: code.to_owned(),
            message,
        },
        last_known: None,
        cleanup: CleanupReport::default(),
    }
}

fn watchdog_fault(options: &SpawnOptions) -> u8 {
    #[cfg(test)]
    {
        return match options.fault {
            TestSpawnFault::WatchdogDiesBeforeBootReady => FAULT_EXIT_BEFORE_BOOT_READY,
            TestSpawnFault::WatchdogDiesBeforeRegistration => FAULT_EXIT_BEFORE_REGISTRATION,
            TestSpawnFault::WatchdogDiesBeforeAck => FAULT_EXIT_BEFORE_ACK,
            TestSpawnFault::WatchdogDiesAfterAck => FAULT_EXIT_AFTER_ACK,
            TestSpawnFault::WatchdogDiesAfterCommitBeforeCommitted => {
                FAULT_EXIT_AFTER_COMMIT_BEFORE_COMMITTED
            }
            TestSpawnFault::WatchdogDiesAfterCommitted => FAULT_EXIT_AFTER_COMMITTED,
            TestSpawnFault::WithholdAck => FAULT_WITHHOLD_ACK,
            #[cfg(target_os = "linux")]
            TestSpawnFault::SkipFinalGroupKill => FAULT_SKIP_FINAL_GROUP_KILL,
            TestSpawnFault::FailFinalGroupKillOnce => FAULT_FAIL_FINAL_GROUP_KILL_ONCE,
            TestSpawnFault::None => FAULT_NONE,
        };
    }
    #[cfg(not(test))]
    {
        let _ = options;
        FAULT_NONE
    }
}

fn enforce_sandbox(request: &NormalizedExecutionRequest) -> Result<(), ExecutionError> {
    match &request.capability.sandbox {
        SandboxPolicy::UnrestrictedLocalOwner => Ok(()),
        SandboxPolicy::DenyExecution => Err(ExecutionError::CapabilityDenied {
            path: request.cwd.clone(),
            reason: "execution is denied by the sandbox policy".to_owned(),
        }),
        SandboxPolicy::MacSeatbelt { .. } => Err(ExecutionError::CapabilityDenied {
            path: request.cwd.clone(),
            reason: "macOS Seatbelt execution is pending and cannot run unrestricted".to_owned(),
        }),
    }
}

fn std_command_for(spec: &CommandSpec) -> StdCommand {
    match spec {
        CommandSpec::Program { program, .. } => StdCommand::new(program),
        CommandSpec::Shell { shell: ShellKind::Posix, .. } => StdCommand::new("/bin/sh"),
        CommandSpec::Shell { shell: ShellKind::PowerShell, .. } => StdCommand::new("pwsh"),
    }
}

fn command_args(spec: &CommandSpec) -> Vec<&std::ffi::OsStr> {
    match spec {
        CommandSpec::Program { args, .. } => args.iter().map(|arg| arg.as_os_str()).collect(),
        CommandSpec::Shell {
            shell: ShellKind::Posix,
            script,
        } => vec![std::ffi::OsStr::new("-c"), script.as_ref()],
        CommandSpec::Shell {
            shell: ShellKind::PowerShell,
            script,
        } => vec![
            std::ffi::OsStr::new("-NoLogo"),
            std::ffi::OsStr::new("-NoProfile"),
            std::ffi::OsStr::new("-NonInteractive"),
            std::ffi::OsStr::new("-Command"),
            script.as_ref(),
        ],
    }
}

fn spawn_failed(error: io::Error) -> ExecutionError {
    ExecutionError::SpawnFailed {
        failure: SpawnFailure {
            code: "spawn_failed".to_owned(),
            message: error.to_string(),
        },
    }
}

struct UnixOwner {
    pid: u32,
    lifecycle: LifecycleHandle,
    io: UnixIo,
    readers: Mutex<Vec<JoinHandle<io::Result<()>>>>,
}

enum UnixIo {
    Pipe(tokio::sync::Mutex<Option<ChildStdin>>),
    Pty(Arc<super::unix_pty::AsyncPtyMaster>),
}

struct SignalGate {
    phase: SignalPhase,
    final_kill_sent: bool,
    control_fd: Option<RawFd>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SignalPhase {
    Open,
    Closing,
    CleanupOwned,
    Retired,
}

struct LifecycleJob {
    child: Option<StdChild>,
    watchdog_pid: Option<libc::pid_t>,
    control: Option<OwnedFd>,
    pgid: libc::pid_t,
    nonce: Nonce,
    signal_gate: Arc<Mutex<SignalGate>>,
    completion: Option<watch::Sender<LifecycleCompletion>>,
    failure_context: Option<(io::ErrorKind, Arc<str>)>,
    cleanup_relay: mpsc::Sender<CleanupJob>,
    #[cfg(test)]
    start_delay: Option<Duration>,
    #[cfg(test)]
    terminal_delay: Option<Duration>,
    #[cfg(test)]
    after_leader_reap_pause: Option<TestBlockingTransactionPause>,
    #[cfg(test)]
    fail_before_cleanup: bool,
    #[cfg(test)]
    cleanup_hold: Option<TestCleanupHold>,
    #[cfg(test)]
    audit: TestSpawnAudit,
}

impl LifecycleJob {
    fn run(mut self) {
        #[cfg(test)]
        if let Some(delay) = self.start_delay {
            let milliseconds = delay.as_millis().min(libc::c_int::MAX as u128) as libc::c_int;
            let _ = poll_delay(milliseconds);
        }
        #[cfg(test)]
        let lifecycle_result = if self.fail_before_cleanup {
            Err(io::Error::other(
                "injected lifecycle failure before exact cleanup",
            ))
        } else {
            self.run_inner()
        };
        #[cfg(not(test))]
        let lifecycle_result = self.run_inner();
        let completion = match lifecycle_result {
            Ok(fact) => LifecycleCompletion::Reaped(fact),
            Err(error) => LifecycleCompletion::Failed {
                kind: error.kind(),
                message: error.to_string().into(),
            },
        };
        if self.child.is_none()
            && self.watchdog_pid.is_none()
            && let Some(sender) = self.completion.as_ref()
        {
            sender.send_replace(completion);
        } else if let LifecycleCompletion::Failed { kind, message } = completion {
            self.failure_context = Some((kind, message));
        }
    }

    fn run_inner(&mut self) -> io::Result<ExitFact> {
        let leader_pid = self.pgid;
        let control_fd = self
            .control
            .as_ref()
            .ok_or_else(|| io::Error::other("lifecycle control is missing"))?
            .as_raw_fd();
        let mut lifecycle_error: Option<io::Error> = None;
        let mut quiescing_seen = false;
        let mut host_kill_attempted = false;
        let mut leader_observed = false;

        loop {
            if leader_exit_observed(leader_pid)? {
                leader_observed = true;
                break;
            }
            let events = poll_control(control_fd, 50)?;
            if events == 0 {
                continue;
            }
            let mut peer_closed = false;
            if events & libc::POLLIN != 0 {
                match recv_lifecycle_frame(control_fd, self.nonce, leader_pid) {
                    Ok(FrameKind::Quiescing) => {
                        quiescing_seen = true;
                        self.close_signal_gate(false)?;
                        break;
                    }
                    Ok(FrameKind::Failure) => {
                        #[cfg(test)]
                        self.audit
                            .failure_frames
                            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                        lifecycle_error.get_or_insert_with(|| {
                            io::Error::other("watchdog reported failure after COMMITTED")
                        });
                    }
                    Ok(kind) => {
                        lifecycle_error.get_or_insert_with(|| {
                            io::Error::other(format!(
                                "unexpected lifecycle frame while leader was running: {kind:?}"
                            ))
                        });
                    }
                    Err(ProtocolError::PeerClosed) => peer_closed = true,
                    Err(error) => {
                        lifecycle_error.get_or_insert_with(|| protocol_io_error(error));
                    }
                }
            }
            if lifecycle_error.is_some()
                || peer_closed
                || events & (libc::POLLHUP | libc::POLLERR | libc::POLLNVAL) != 0
            {
                let final_kill_sent = self.close_signal_gate(false)?.0;
                if !final_kill_sent && !quiescing_seen {
                    if let Err(error) = self.host_fallback_kill(&mut host_kill_attempted) {
                        return Err(error);
                    }
                    lifecycle_error.get_or_insert_with(|| {
                        io::Error::other("watchdog control was lost while leader was running")
                    });
                }
                if events & libc::POLLIN == 0 {
                    poll_delay(5)?;
                }
            }
        }

        #[cfg(test)]
        if let Some(delay) = self.terminal_delay {
            let milliseconds = delay.as_millis().min(libc::c_int::MAX as u128) as libc::c_int;
            poll_delay(milliseconds)?;
        }

        let final_kill_sent = self.close_signal_gate(false)?.0;
        if lifecycle_error.is_some() && !final_kill_sent {
            if let Err(error) = self.host_fallback_kill(&mut host_kill_attempted) {
                return Err(error);
            }
        }

        let watchdog_deadline = Instant::now()
            .checked_add(WATCHDOG_QUIESCING_WAIT)
            .unwrap_or_else(Instant::now);
        let mut watchdog_status = None;
        while watchdog_status.is_none() && Instant::now() < watchdog_deadline {
            leader_observed |= leader_exit_observed(leader_pid)?;
            watchdog_status = self.try_reap_watchdog()?;
            if watchdog_status.is_some() {
                break;
            }
            let events = poll_control(control_fd, 10)?;
            if events & libc::POLLIN != 0 {
                match recv_lifecycle_frame(control_fd, self.nonce, leader_pid) {
                    Ok(FrameKind::Quiescing) => quiescing_seen = true,
                    Ok(FrameKind::Failure) => {
                        #[cfg(test)]
                        self.audit
                            .failure_frames
                            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                        lifecycle_error.get_or_insert_with(|| {
                            io::Error::other("watchdog final group kill failed")
                        });
                        if let Err(error) = self.host_fallback_kill(&mut host_kill_attempted) {
                            return Err(error);
                        }
                    }
                    Ok(kind) => {
                        lifecycle_error.get_or_insert_with(|| {
                            io::Error::other(format!(
                                "unexpected watchdog terminal frame: {kind:?}"
                            ))
                        });
                        if !final_kill_sent
                            && let Err(error) =
                                self.host_fallback_kill(&mut host_kill_attempted)
                        {
                            return Err(error);
                        }
                    }
                    Err(ProtocolError::PeerClosed) if quiescing_seen || final_kill_sent => {}
                    Err(error) => {
                        lifecycle_error.get_or_insert_with(|| protocol_io_error(error));
                        if !final_kill_sent
                            && let Err(error) =
                                self.host_fallback_kill(&mut host_kill_attempted)
                        {
                            return Err(error);
                        }
                    }
                }
            } else if events & (libc::POLLHUP | libc::POLLERR | libc::POLLNVAL) != 0 {
                poll_delay(5)?;
            }
        }
        if watchdog_status.is_none() {
            lifecycle_error.get_or_insert_with(|| {
                io::Error::new(io::ErrorKind::TimedOut, "watchdog did not terminate promptly")
            });
            if !final_kill_sent
                && let Err(error) = self.host_fallback_kill(&mut host_kill_attempted)
            {
                return Err(error);
            }
            watchdog_status = Some(self.reap_watchdog_blocking()?);
        }
        self.drain_terminal_frames(
            control_fd,
            leader_pid,
            &mut quiescing_seen,
            &mut lifecycle_error,
        )?;
        let watchdog_status = watchdog_status.expect("watchdog status was established");
        if !was_killed_by_group_sigkill(watchdog_status) {
            lifecycle_error.get_or_insert_with(|| {
                io::Error::other(format!(
                    "watchdog did not exit from SIGKILL: status={watchdog_status:#x}"
                ))
            });
        }
        if !quiescing_seen && !host_kill_attempted && !final_kill_sent {
            lifecycle_error.get_or_insert_with(|| {
                io::Error::other("watchdog exited without a valid QUIESCING frame")
            });
        }

        // Even a valid QUIESCING + SIGKILL pair cannot prove that no concurrent
        // actor killed only the watchdog. Make one final idempotent group-kill
        // attempt while the exact leader remains WNOWAIT-anchored.
        if let Err(error) = self.host_fallback_kill(&mut host_kill_attempted) {
            return Err(error);
        }
        while !leader_observed {
            leader_observed = leader_exit_observed(leader_pid)?;
            if !leader_observed {
                poll_delay(50)?;
            }
        }

        self.retire_control();
        let status = self.reap_leader()?;
        #[cfg(test)]
        if let Some(pause) = self.after_leader_reap_pause.as_ref() {
            pause.block();
        }
        self.retire_signal_identity();
        prove_group_absent(self.pgid)?;
        if let Some(error) = lifecycle_error {
            return Err(error);
        }
        exit_fact(status)
    }

    fn drain_terminal_frames(
        &self,
        control_fd: RawFd,
        leader_pid: libc::pid_t,
        quiescing_seen: &mut bool,
        lifecycle_error: &mut Option<io::Error>,
    ) -> io::Result<()> {
        loop {
            let events = poll_control(control_fd, 0)?;
            if events & libc::POLLIN == 0 {
                return Ok(());
            }
            match recv_lifecycle_frame(control_fd, self.nonce, leader_pid) {
                Ok(FrameKind::Quiescing) => *quiescing_seen = true,
                Ok(FrameKind::Failure) => {
                    #[cfg(test)]
                    self.audit
                        .failure_frames
                        .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    lifecycle_error.get_or_insert_with(|| {
                        io::Error::other("watchdog final group kill failed")
                    });
                }
                Ok(kind) => {
                    lifecycle_error.get_or_insert_with(|| {
                        io::Error::other(format!(
                            "unexpected queued watchdog terminal frame: {kind:?}"
                        ))
                    });
                }
                Err(ProtocolError::PeerClosed) => return Ok(()),
                Err(error) => {
                    lifecycle_error.get_or_insert_with(|| protocol_io_error(error));
                    return Ok(());
                }
            }
        }
    }

    fn close_signal_gate(&self, force_kill: bool) -> io::Result<(bool, bool)> {
        let mut gate = match self.signal_gate.lock() {
            Ok(gate) => gate,
            Err(poisoned) => poisoned.into_inner(),
        };
        if gate.phase == SignalPhase::Open {
            gate.phase = SignalPhase::Closing;
        }
        let mut sent_now = false;
        if force_kill && !gate.final_kill_sent {
            // SAFETY: exact unreaped leader identity still anchors this PGID.
            let result = unsafe { libc::kill(-self.pgid, libc::SIGKILL) };
            #[cfg(test)]
            self.audit
                .group_signals
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            if result == 0 {
                gate.final_kill_sent = true;
                sent_now = true;
            } else {
                let error = io::Error::last_os_error();
                if error.raw_os_error() == Some(libc::ESRCH) {
                    gate.final_kill_sent = true;
                } else {
                    return Err(error);
                }
            }
        }
        Ok((gate.final_kill_sent, sent_now))
    }

    fn host_fallback_kill(&self, attempted: &mut bool) -> io::Result<()> {
        if *attempted {
            return Ok(());
        }
        self.close_signal_gate(true)?;
        *attempted = true;
        Ok(())
    }

    fn try_reap_watchdog(&mut self) -> io::Result<Option<libc::c_int>> {
        let Some(pid) = self.watchdog_pid else {
            return Err(io::Error::other("watchdog wait already completed"));
        };
        let mut status = 0;
        // SAFETY: pid is the exact unreaped direct-child watchdog.
        let waited = unsafe { libc::waitpid(pid, &mut status, libc::WNOHANG) };
        if waited == 0 {
            return Ok(None);
        }
        if waited == pid {
            self.watchdog_pid = None;
            #[cfg(test)]
            self.audit.record_watchdog_reap(status);
            return Ok(Some(status));
        }
        let error = io::Error::last_os_error();
        if error.raw_os_error() == Some(libc::EINTR) {
            return Ok(None);
        }
        Err(error)
    }

    fn reap_watchdog_blocking(&mut self) -> io::Result<libc::c_int> {
        let pid = self
            .watchdog_pid
            .ok_or_else(|| io::Error::other("watchdog wait already completed"))?;
        let status = waitpid_exact_blocking(pid)?;
        self.watchdog_pid = None;
        #[cfg(test)]
        self.audit.record_watchdog_reap(status);
        Ok(status)
    }

    fn reap_leader(&mut self) -> io::Result<ExitStatus> {
        let child = self
            .child
            .as_mut()
            .ok_or_else(|| io::Error::other("leader wait already completed"))?;
        let status = child.wait()?;
        self.child = None;
        #[cfg(test)]
        self.audit.record_leader_reap();
        Ok(status)
    }

    fn retire_control(&mut self) {
        let mut gate = match self.signal_gate.lock() {
            Ok(gate) => gate,
            Err(poisoned) => poisoned.into_inner(),
        };
        gate.control_fd = None;
        self.control.take();
    }

    fn retire_signal_identity(&self) {
        let mut gate = match self.signal_gate.lock() {
            Ok(gate) => gate,
            Err(poisoned) => poisoned.into_inner(),
        };
        gate.control_fd = None;
        gate.phase = SignalPhase::Retired;
    }
}

impl Drop for LifecycleJob {
    fn drop(&mut self) {
        if self.child.is_none() && self.watchdog_pid.is_none() {
            self.retire_control();
            return;
        }
        {
            let mut gate = match self.signal_gate.lock() {
                Ok(gate) => gate,
                Err(poisoned) => poisoned.into_inner(),
            };
            gate.phase = SignalPhase::CleanupOwned;
            #[cfg(test)]
            self.audit
                .cleanup_owned_transitions
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            if let Some(fd) = gate.control_fd.take() {
                // SAFETY: ownership retirement and shutdown are serialized by the gate.
                let _ = unsafe { libc::shutdown(fd, libc::SHUT_RDWR) };
            }
        }
        let job = CleanupJob {
            child: self.child.take(),
            watchdog_pid: self.watchdog_pid.take(),
            control: self.control.take(),
            pgid: Some(self.pgid),
            group_state: CleanupGroupState::Pending,
            signal_gate: Some(Arc::clone(&self.signal_gate)),
            completion: self.completion.take(),
            failure_context: self.failure_context.take(),
            attempts: 0,
            last_error: None,
            watchdog_ownership_lost: false,
            leader_ownership_lost: false,
            retry_delay: CLEANUP_RETRY_DELAY,
            next_attempt: Instant::now(),
            absence_deadline: None,
            #[cfg(test)]
            audit: self.audit.clone(),
            #[cfg(test)]
            hold: self.cleanup_hold.clone(),
        };
        if let Err(error) = self.cleanup_relay.send(job) {
            let _ = error.0.run_to_completion();
        }
    }
}

impl Drop for UnixOwner {
    fn drop(&mut self) {
        match &mut self.io {
            UnixIo::Pipe(stdin) => {
                stdin.get_mut().take();
            }
            UnixIo::Pty(_) => {}
        }
        self.lifecycle.shutdown();
        let readers = match self.readers.get_mut() {
            Ok(readers) => std::mem::take(readers),
            Err(poisoned) => std::mem::take(poisoned.into_inner()),
        };
        for reader in readers {
            reader.abort();
        }
    }
}

#[async_trait]
impl ProcessOwner for UnixOwner {
    fn pid(&self) -> u32 {
        self.pid
    }

    async fn write(&self, bytes: &[u8]) -> io::Result<()> {
        match &self.io {
            UnixIo::Pipe(stdin) => {
                let mut stdin = stdin.lock().await;
                let stdin = stdin
                    .as_mut()
                    .ok_or_else(|| io::Error::new(io::ErrorKind::BrokenPipe, "stdin is closed"))?;
                stdin.write_all(bytes).await?;
                stdin.flush().await
            }
            UnixIo::Pty(master) => master.write_all(bytes).await,
        }
    }

    async fn close_stdin(&self) -> io::Result<()> {
        match &self.io {
            UnixIo::Pipe(stdin) => {
                stdin.lock().await.take();
                Ok(())
            }
            UnixIo::Pty(master) => master.close_input().await,
        }
    }

    async fn resize(&self, cols: u16, rows: u16) -> io::Result<()> {
        let gate = self
            .lifecycle
            .signal_gate
            .lock()
            .map_err(|_| io::Error::other("signal gate is poisoned"))?;
        if gate.phase != SignalPhase::Open {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                "process group is already quiescing",
            ));
        }
        match &self.io {
            UnixIo::Pipe(_) => Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "pipe transport does not support terminal resize",
            )),
            // Keep the lifecycle gate locked across the terminal ioctl. This
            // makes the state validation and resize atomic with respect to
            // quiescing/retirement.
            UnixIo::Pty(master) => master.resize(cols, rows),
        }
    }

    async fn interrupt(&self) -> io::Result<()> {
        self.signal_group(libc::SIGINT)
    }

    async fn terminate(&self) -> io::Result<()> {
        self.signal_group(libc::SIGTERM)
    }

    async fn force_kill(&self) -> io::Result<()> {
        self.signal_group(libc::SIGKILL)
    }

    async fn wait_reaped(&self, deadline: Instant) -> io::Result<ExitFact> {
        let mut completion = self.lifecycle.completion.clone();
        let lifecycle_result = loop {
            let state = completion.borrow().clone();
            match state {
                LifecycleCompletion::Running => {}
                LifecycleCompletion::Reaped(fact) => break Ok(fact),
                LifecycleCompletion::Failed { kind, message } => {
                    break Err(io::Error::new(kind, message.to_string()));
                }
            }
            let changed = tokio::time::timeout_at(
                tokio::time::Instant::from_std(deadline),
                completion.changed(),
            )
            .await;
            match changed {
                Ok(Ok(())) => {}
                Ok(Err(_)) => {
                    self.lifecycle.shutdown();
                    return Err(io::Error::other(
                        "lifecycle worker ended without a result",
                    ));
                }
                Err(_) => {
                    self.lifecycle.shutdown();
                    return Err(io::Error::new(
                        io::ErrorKind::TimedOut,
                        "leader reap timed out",
                    ));
                }
            }
        };
        let reader_result = join_readers(&self.readers, deadline).await;
        match (lifecycle_result, reader_result) {
            (Ok(fact), Ok(())) => Ok(fact),
            (Err(error), _) => Err(error),
            (Ok(_), Err(error)) => Err(error),
        }
    }
}

impl UnixOwner {
    fn signal_group(&self, signal: libc::c_int) -> io::Result<()> {
        let mut gate = self
            .lifecycle
            .signal_gate
            .lock()
            .map_err(|_| io::Error::other("signal gate is poisoned"))?;
        if gate.phase != SignalPhase::Open {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                "process group is already quiescing",
            ));
        }
        // SAFETY: the gate stays locked across validation and the negative-PGID syscall.
        let result = unsafe { libc::kill(-self.lifecycle.pgid, signal) };
        #[cfg(test)]
        self.lifecycle
            .audit
            .group_signals
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        if result == 0 {
            if signal == libc::SIGKILL {
                gate.final_kill_sent = true;
                gate.phase = SignalPhase::Closing;
            }
            Ok(())
        } else {
            Err(io::Error::last_os_error())
        }
    }
}

fn recv_lifecycle_frame(
    control_fd: RawFd,
    nonce: Nonce,
    pgid: libc::pid_t,
) -> Result<FrameKind, ProtocolError> {
    let deadline = Deadline::after(WATCHDOG_QUIESCING_WAIT)?;
    let frame = recv_frame(control_fd, nonce, deadline)?;
    if frame.pid() != pgid || frame.pgid() != pgid {
        return Err(ProtocolError::MalformedFrame);
    }
    Ok(frame.kind())
}

fn poll_control(control_fd: RawFd, timeout_ms: libc::c_int) -> io::Result<libc::c_short> {
    let mut descriptor = libc::pollfd {
        fd: control_fd,
        events: libc::POLLIN | libc::POLLHUP | libc::POLLERR,
        revents: 0,
    };
    loop {
        // SAFETY: descriptor points to one initialized pollfd.
        let result = unsafe { libc::poll(&mut descriptor, 1, timeout_ms) };
        if result >= 0 {
            return Ok(if result == 0 { 0 } else { descriptor.revents });
        }
        let error = io::Error::last_os_error();
        if error.raw_os_error() != Some(libc::EINTR) {
            return Err(error);
        }
    }
}

fn poll_delay(timeout_ms: libc::c_int) -> io::Result<()> {
    loop {
        // SAFETY: poll with no descriptors is an OS-backed bounded wait.
        let result = unsafe { libc::poll(std::ptr::null_mut(), 0, timeout_ms) };
        if result == 0 {
            return Ok(());
        }
        if result > 0 {
            continue;
        }
        let error = io::Error::last_os_error();
        if error.raw_os_error() != Some(libc::EINTR) {
            return Err(error);
        }
    }
}

fn was_killed_by_group_sigkill(status: libc::c_int) -> bool {
    libc::WIFSIGNALED(status) && libc::WTERMSIG(status) == libc::SIGKILL
}

fn leader_exit_observed(pid: libc::pid_t) -> io::Result<bool> {
    loop {
        let mut info = std::mem::MaybeUninit::<libc::siginfo_t>::zeroed();
        // SAFETY: waitid observes only the direct child and WNOWAIT preserves its identity.
        let result = unsafe {
            libc::waitid(
                libc::P_PID,
                pid as libc::id_t,
                info.as_mut_ptr(),
                libc::WEXITED | libc::WNOWAIT | libc::WNOHANG,
            )
        };
        if result == 0 {
            // SAFETY: waitid initialized siginfo on success.
            let info = unsafe { info.assume_init() };
            return Ok(unsafe { info.si_pid() } == pid);
        }
        let error = io::Error::last_os_error();
        if error.raw_os_error() != Some(libc::EINTR) {
            return Err(error);
        }
    }
}

fn exit_fact(status: ExitStatus) -> io::Result<ExitFact> {
    use std::os::unix::process::ExitStatusExt;
    Ok(ExitFact {
        code: status.code(),
        signal: status.signal(),
        cleanup_errors: Vec::new(),
    })
}

fn prove_group_absent(pgid: libc::pid_t) -> io::Result<()> {
    let deadline = Instant::now()
        .checked_add(GROUP_ABSENCE_WAIT)
        .unwrap_or_else(Instant::now);
    loop {
        match probe_group_absent_once(pgid) {
            Ok(true) => return Ok(()),
            Ok(false) => {}
            Err(error) => {
                return Err(io::Error::other(format!(
                    "process group absence is unproven after exact reaps: {error}"
                )));
            }
        }
        if Instant::now() >= deadline {
            return Err(io::Error::other(
                "process group still exists after exact reaps",
            ));
        }
        poll_delay(5)?;
    }
}

fn probe_group_absent_once(pgid: libc::pid_t) -> io::Result<bool> {
    // SAFETY: signal zero only probes the cached process-group identity.
    if unsafe { libc::kill(-pgid, 0) } == 0 {
        return Ok(false);
    }
    let error = io::Error::last_os_error();
    if error.raw_os_error() == Some(libc::ESRCH) {
        Ok(true)
    } else {
        Err(error)
    }
}

async fn read_stream(
    mut reader: impl AsyncRead + Unpin,
    stream: OutputStream,
    output: Arc<OutputBuffer>,
) -> io::Result<()> {
    let mut buffer = [0_u8; READ_BUFFER_BYTES];
    loop {
        let read = reader.read(&mut buffer).await?;
        if read == 0 {
            return Ok(());
        }
        output.push(stream, &buffer[..read]);
    }
}

async fn join_readers(
    readers: &Mutex<Vec<JoinHandle<io::Result<()>>>>,
    deadline: Instant,
) -> io::Result<()> {
    let mut readers = {
        let mut readers = readers
            .lock()
            .map_err(|_| io::Error::other("reader task mutex is poisoned"))?;
        std::mem::take(&mut *readers)
    };
    let drain_deadline = deadline.min(
        Instant::now()
            .checked_add(POST_EXIT_READER_DRAIN)
            .unwrap_or(deadline),
    );
    for index in 0..readers.len() {
        let result = tokio::time::timeout_at(
            tokio::time::Instant::from_std(drain_deadline),
            &mut readers[index],
        )
        .await;
        let error = match result {
            Err(_) => Some(io::Error::new(
                io::ErrorKind::TimedOut,
                "output reader timed out",
            )),
            Ok(Err(error)) => Some(io::Error::other(format!(
                "output reader task failed: {error}"
            ))),
            Ok(Ok(Err(error))) => Some(error),
            Ok(Ok(Ok(()))) => None,
        };
        if let Some(error) = error {
            for reader in &readers {
                reader.abort();
            }
            return Err(error);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::{
        collections::{BTreeMap, BTreeSet},
        ffi::OsString,
        fs,
        sync::{Arc, atomic::Ordering},
        time::{Duration, Instant},
    };

    use super::{
        SpawnOptions, SpawnTransport, TestSpawnAudit, TestSpawnFault, spawn_inner,
        spawn_pipe_inner,
    };
    use crate::{
        CapabilityPolicy, CommandSpec, ExecutionError, ExecutionOwner, ExecutionPolicy,
        NormalizedExecutionRequest, OutputBuffer, Transport,
    };

    async fn spawn_with_fault(
        request: NormalizedExecutionRequest,
        fault: TestSpawnFault,
        audit: &TestSpawnAudit,
    ) -> Result<super::SpawnedPlatformProcess, ExecutionError> {
        spawn_pipe_inner(
            request,
            Arc::new(OutputBuffer::new(1024)),
            SpawnOptions {
                fault,
                audit: audit.clone(),
                setup_timeout: None,
                ..SpawnOptions::default()
            },
        )
        .await
    }

    async fn spawn_pty_with_fault(
        request: NormalizedExecutionRequest,
        fault: TestSpawnFault,
        audit: &TestSpawnAudit,
    ) -> Result<super::SpawnedPlatformProcess, ExecutionError> {
        spawn_inner(
            request,
            Arc::new(OutputBuffer::new(1024)),
            SpawnOptions {
                fault,
                audit: audit.clone(),
                setup_timeout: None,
                ..SpawnOptions::default()
            },
            SpawnTransport::Pty { cols: 80, rows: 24 },
        )
        .await
    }

    async fn spawn_with_setup_timeout(
        request: NormalizedExecutionRequest,
        fault: TestSpawnFault,
        audit: &TestSpawnAudit,
        setup_timeout: Duration,
    ) -> Result<super::SpawnedPlatformProcess, ExecutionError> {
        spawn_pipe_inner(
            request,
            Arc::new(OutputBuffer::new(1024)),
            SpawnOptions {
                fault,
                audit: audit.clone(),
                setup_timeout: Some(setup_timeout),
                ..SpawnOptions::default()
            },
        )
        .await
    }

    async fn spawn_with_registration_fault(
        request: NormalizedExecutionRequest,
        fault: super::TestRegistrationFault,
        audit: &TestSpawnAudit,
    ) -> Result<super::SpawnedPlatformProcess, ExecutionError> {
        spawn_pipe_inner(
            request,
            Arc::new(OutputBuffer::new(1024)),
            SpawnOptions {
                audit: audit.clone(),
                registration_fault: fault,
                ..SpawnOptions::default()
            },
        )
        .await
    }

    #[tokio::test]
    #[serial_test::serial(unix_spawn)]
    async fn watchdog_death_before_registration_never_executes_user_marker() {
        let directory = tempfile::tempdir().expect("temporary directory should be created");
        let marker = directory.path().join("must-not-run.marker");
        let request = shell_request(&marker);
        let audit = TestSpawnAudit::default();

        let result = tokio::time::timeout(
            Duration::from_secs(6),
            spawn_with_fault(
                request,
                TestSpawnFault::WatchdogDiesBeforeRegistration,
                &audit,
            ),
        )
        .await
        .expect("registration failure must honor the shared setup deadline");
        if let Ok(spawned) = &result {
            let _ = spawned
                .owner
                .wait_reaped(Instant::now() + Duration::from_secs(2))
                .await;
        }

        assert!(result.is_err(), "watchdog loss before ACK must fail start");
        assert!(!marker.exists(), "user marker ran without watchdog ownership");
    }

    #[tokio::test]
    #[serial_test::serial(unix_spawn)]
    async fn watchdog_eof_before_boot_or_ack_never_executes_user_code() {
        for fault in [
            TestSpawnFault::WatchdogDiesBeforeBootReady,
            TestSpawnFault::WatchdogDiesBeforeAck,
        ] {
            let directory = tempfile::tempdir().expect("temporary directory should be created");
            let marker = directory.path().join("pre-ownership-eof.marker");
            let audit = TestSpawnAudit::default();

            let result = spawn_with_fault(shell_request(&marker), fault, &audit).await;
            if let Ok(spawned) = &result {
                let _ = spawned
                    .owner
                    .wait_reaped(Instant::now() + Duration::from_secs(1))
                    .await;
            }
            let reaped = wait_for_watchdog_reaps(&audit, 1, Duration::from_secs(2)).await;

            assert!(
                matches!(result, Err(ExecutionError::SpawnFailed { .. })),
                "pre-ACK watchdog loss must remain a proven SpawnFailed"
            );
            assert!(!marker.exists(), "user code ran without completed watchdog ownership");
            assert!(reaped, "pre-ownership watchdog was not exactly reaped");
            assert_eq!(audit.watchdog_reaps.load(Ordering::SeqCst), 1);
            assert_eq!(audit.group_signals.load(Ordering::SeqCst), 0);
            let watchdog_status = audit.watchdog_status.load(Ordering::SeqCst);
            let expected_exit = match fault {
                TestSpawnFault::WatchdogDiesBeforeBootReady => {
                    super::EXIT_FAULT_BEFORE_BOOT_READY
                }
                TestSpawnFault::WatchdogDiesBeforeAck => super::EXIT_FAULT_BEFORE_ACK,
                _ => unreachable!("loop contains only before-BOOT/before-ACK faults"),
            };
            assert!(
                libc::WIFEXITED(watchdog_status)
                    && libc::WEXITSTATUS(watchdog_status) == expected_exit,
                "faulted watchdog exit status was not preserved: {watchdog_status:#x}"
            );
        }
    }

    #[tokio::test]
    #[serial_test::serial(unix_spawn)]
    async fn watchdog_exit_after_commit_before_committed_is_start_lost_with_exact_cleanup() {
        let audit = TestSpawnAudit::default();

        let result = spawn_with_fault(
            request("/bin/sleep".into(), vec!["60".into()]),
            TestSpawnFault::WatchdogDiesAfterCommitBeforeCommitted,
            &audit,
        )
        .await;
        let leader = audit.leader_pid.load(Ordering::SeqCst);
        let cleaned = tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if audit.watchdog_reaps.load(Ordering::SeqCst) == 1
                    && audit.leader_reaps.load(Ordering::SeqCst) == 1
                {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .is_ok();

        assert!(
            matches!(
                result,
                Err(ExecutionError::StartLost { ref failure, .. })
                    if failure.code == "ownership_commit_failed"
            ),
            "post-COMMIT/pre-COMMITTED loss must be ownership_commit_failed StartLost"
        );
        assert!(cleaned, "post-COMMIT loss did not finish exact cleanup");
        assert_eq!(audit.group_signals.load(Ordering::SeqCst), 1);
        assert!(!process_exists(leader), "post-COMMIT leader remained observable");
        let watchdog_status = audit.watchdog_status.load(Ordering::SeqCst);
        assert!(
            libc::WIFEXITED(watchdog_status)
                && libc::WEXITSTATUS(watchdog_status)
                    == super::EXIT_FAULT_AFTER_COMMIT_BEFORE_COMMITTED,
            "post-COMMIT fault occurred at the wrong checkpoint: {watchdog_status:#x}"
        );
    }

    #[tokio::test]
    #[serial_test::serial(unix_spawn)]
    async fn malformed_registration_frames_never_execute_user_code() {
        for fault in [
            super::TestRegistrationFault::ShortFrame,
            super::TestRegistrationFault::WrongNonce,
        ] {
            let directory = tempfile::tempdir().expect("temporary directory should be created");
            let marker = directory.path().join("malformed-registration.marker");
            let audit = TestSpawnAudit::default();

            let result = spawn_with_registration_fault(shell_request(&marker), fault, &audit).await;
            if let Ok(spawned) = &result {
                let _ = spawned
                    .owner
                    .wait_reaped(Instant::now() + Duration::from_secs(1))
                    .await;
            }
            let reaped = wait_for_watchdog_reaps(&audit, 1, Duration::from_secs(2)).await;

            assert!(result.is_err(), "malformed registration unexpectedly started");
            assert!(!marker.exists(), "malformed registration executed user code");
            assert!(reaped, "malformed registration watchdog was not exactly reaped");
            assert_eq!(audit.watchdog_reaps.load(Ordering::SeqCst), 1);
        }
    }

    #[tokio::test]
    #[serial_test::serial(unix_spawn)]
    async fn invalid_exec_abort_reaps_watchdog_without_group_signal() {
        let request = request(
            "/definitely/not/a/nomifun-executable".into(),
            Vec::new(),
        );
        let audit = TestSpawnAudit::default();

        let result = tokio::time::timeout(
            Duration::from_secs(6),
            spawn_with_fault(request, TestSpawnFault::None, &audit),
        )
        .await
        .expect("invalid exec and ABORT must not deadlock");

        assert!(matches!(result, Err(ExecutionError::SpawnFailed { .. })));
        assert_eq!(audit.group_signals.load(Ordering::SeqCst), 0);
        assert_eq!(
            audit.watchdog_reaps.load(Ordering::SeqCst),
            1,
            "ABORT must precisely reap the direct-child watchdog"
        );
    }

    #[tokio::test]
    #[serial_test::serial(unix_spawn)]
    async fn watchdog_death_after_ack_is_start_lost_not_an_owned_session() {
        let directory = tempfile::tempdir().expect("temporary directory should be created");
        let marker = directory.path().join("ack-crossed.marker");
        let audit = TestSpawnAudit::default();

        let result = tokio::time::timeout(
            Duration::from_secs(6),
            spawn_with_fault(
                shell_request(&marker),
                TestSpawnFault::WatchdogDiesAfterAck,
                &audit,
            ),
        )
        .await
        .expect("post-ACK watchdog loss must honor the shared setup deadline");

        assert!(matches!(result, Err(ExecutionError::StartLost { .. })));
    }

    #[tokio::test]
    #[serial_test::serial(unix_spawn)]
    async fn watchdog_sigkill_after_committed_fails_closed_while_leader_is_running() {
        let directory = tempfile::tempdir().expect("temporary directory should be created");
        let descendant_marker = directory.path().join("post-commit-descendant.pid");
        let audit = TestSpawnAudit::default();
        let spawned = spawn_with_fault(
            long_running_group_request(&descendant_marker),
            TestSpawnFault::WatchdogDiesAfterCommitted,
            &audit,
        )
        .await
        .expect("the ownership barrier should cross COMMITTED before the watchdog fault");
        let leader = spawned.owner.pid() as libc::pid_t;
        let descendant = wait_for_pid_marker(&descendant_marker).await;

        let wait_started = Instant::now();
        let result = spawned
            .owner
            .wait_reaped(Instant::now() + Duration::from_secs(1))
            .await;
        let elapsed = wait_started.elapsed();
        let group_gone = wait_for_processes_gone([leader, descendant], Duration::from_millis(300)).await;
        let watchdog_reaped = audit.watchdog_reaps.load(Ordering::SeqCst);
        let watchdog_status = audit.watchdog_status.load(Ordering::SeqCst);

        assert!(result.is_err(), "post-COMMITTED watchdog loss must map to Lost");
        assert!(
            elapsed < Duration::from_millis(500),
            "watchdog health loss was not observed promptly while the leader was alive: {elapsed:?}"
        );
        assert!(group_gone, "watchdog loss left the owned leader/descendant group alive");
        assert_eq!(watchdog_reaped, 1, "the failed watchdog must be reaped exactly once");
        assert!(
            libc::WIFSIGNALED(watchdog_status)
                && libc::WTERMSIG(watchdog_status) == libc::SIGKILL,
            "the injected watchdog must be observed as exact SIGKILL: {watchdog_status:#x}"
        );
    }

    #[tokio::test]
    #[serial_test::serial(unix_spawn)]
    async fn external_pty_watchdog_failure_after_committed_fails_closed() {
        let audit = TestSpawnAudit::default();
        let spawned = spawn_pty_with_fault(
            request("/bin/sleep".into(), vec!["60".into()]),
            TestSpawnFault::WatchdogDiesAfterCommitted,
            &audit,
        )
        .await
        .expect("PTY child should commit before the injected watchdog death");
        let leader = spawned.owner.pid() as libc::pid_t;

        let result = spawned
            .owner
            .wait_reaped(Instant::now() + Duration::from_secs(3))
            .await;

        assert!(
            result.is_err(),
            "external watchdog loss after COMMITTED must remain a lifecycle failure"
        );
        assert_eq!(audit.watchdog_reaps.load(Ordering::SeqCst), 1);
        assert_eq!(audit.leader_reaps.load(Ordering::SeqCst), 1);
        assert!(audit.group_signals.load(Ordering::SeqCst) >= 1);
        assert!(!process_exists(leader));
    }

    #[tokio::test]
    #[serial_test::serial(unix_spawn)]
    async fn withheld_ack_uses_one_short_setup_deadline_and_never_executes_user_code() {
        let directory = tempfile::tempdir().expect("temporary directory should be created");
        let marker = directory.path().join("withheld-ack-must-not-run.marker");
        let audit = TestSpawnAudit::default();
        let started = Instant::now();

        let result = spawn_with_setup_timeout(
            shell_request(&marker),
            TestSpawnFault::WithholdAck,
            &audit,
            Duration::from_millis(100),
        )
        .await;
        let elapsed = started.elapsed();
        let reaped = wait_for_watchdog_reaps(&audit, 1, Duration::from_secs(2)).await;
        let watchdog = audit.watchdog_pid.load(Ordering::SeqCst);

        assert!(
            elapsed < Duration::from_millis(350),
            "withheld ACK stacked a second setup/cleanup deadline: {elapsed:?}"
        );
        assert!(
            matches!(result, Err(ExecutionError::StartLost { .. })),
            "cleanup unproven at the shared deadline must be StartLost"
        );
        assert!(!marker.exists(), "user marker executed before watchdog ACK");
        assert!(reaped, "the exact withheld-ACK watchdog was not eventually reaped");
        assert!(!process_exists(watchdog), "the withheld-ACK watchdog remained observable");
        assert_eq!(audit.watchdog_reaps.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    #[serial_test::serial(unix_spawn)]
    async fn dropping_owned_spawn_before_registration_kills_and_reaps_exact_children() {
        let audit = TestSpawnAudit::default();
        let spawned = spawn_with_fault(
            request(
                "/bin/sh".into(),
                vec![
                    "-c".into(),
                    "trap '' INT TERM; while :; do sleep 1; done".into(),
                ],
            ),
            TestSpawnFault::None,
            &audit,
        )
        .await
        .expect("real Unix process group should start");
        let pid = spawned.owner.pid() as libc::pid_t;
        assert!(process_exists(pid), "spawned leader should initially exist");

        drop(spawned);

        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if audit.group_signals.load(Ordering::SeqCst) == 1
                    && audit.watchdog_reaps.load(Ordering::SeqCst) == 1
                    && !process_exists(pid)
                {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("dropping an unregistered owner must complete exact cleanup");
    }

    #[tokio::test]
    #[serial_test::serial(unix_spawn)]
    async fn wait_deadline_shuts_down_lease_and_worker_finishes_exact_cleanup() {
        let audit = TestSpawnAudit::default();
        let spawned = spawn_with_fault(
            request(
                "/bin/sh".into(),
                vec!["-c".into(), "trap '' INT TERM; while :; do sleep 1; done".into()],
            ),
            TestSpawnFault::None,
            &audit,
        )
        .await
        .expect("long-running Unix process should start");
        let pid = spawned.owner.pid() as libc::pid_t;

        let wait = spawned
            .owner
            .wait_reaped(Instant::now() + Duration::from_millis(25))
            .await;

        assert_eq!(
            wait.expect_err("short wait deadline must expire").kind(),
            std::io::ErrorKind::TimedOut
        );
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if audit.group_signals.load(Ordering::SeqCst) == 1
                    && audit.watchdog_reaps.load(Ordering::SeqCst) == 1
                    && !process_exists(pid)
                {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("deadline cancellation must leave exact cleanup with the worker");
    }

    #[tokio::test(flavor = "current_thread")]
    #[serial_test::serial(unix_spawn)]
    async fn async_stdio_wrap_failure_returns_before_delayed_worker_cleanup() {
        let audit = TestSpawnAudit::default();
        let heartbeats = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let heartbeat_count = Arc::clone(&heartbeats);
        let heartbeat = tokio::spawn(async move {
            loop {
                heartbeat_count.fetch_add(1, Ordering::SeqCst);
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        });
        let started = Instant::now();

        let result = spawn_pipe_inner(
            request(
                "/bin/sh".into(),
                vec!["-c".into(), "while :; do sleep 1; done".into()],
            ),
            Arc::new(OutputBuffer::new(1024)),
            SpawnOptions {
                audit: audit.clone(),
                async_wrap_failure: true,
                lifecycle_start_delay: Some(Duration::from_millis(500)),
                ..SpawnOptions::default()
            },
        )
        .await;
        let elapsed = started.elapsed();
        let failure_code = match &result {
            Err(ExecutionError::StartLost { failure, .. }) => Some(failure.code.clone()),
            _ => None,
        };
        let leader = audit.leader_pid.load(Ordering::SeqCst);
        drop(result);
        let reaped = wait_for_watchdog_reaps(&audit, 1, Duration::from_secs(2)).await;
        heartbeat.abort();

        assert_eq!(failure_code.as_deref(), Some("async_process_wrap_failed"));
        assert!(
            elapsed < Duration::from_millis(200),
            "async wrap failure blocked on delayed exact cleanup: {elapsed:?}"
        );
        assert!(reaped, "lifecycle worker did not exactly reap the watchdog");
        assert_eq!(audit.leader_reaps.load(Ordering::SeqCst), 1);
        assert!(!process_exists(leader), "async-wrap leader remained observable");
        assert!(
            heartbeats.load(Ordering::SeqCst) > 0,
            "Tokio worker made no progress during the spawn transaction"
        );
    }

    #[tokio::test]
    #[serial_test::serial(unix_spawn)]
    async fn dropping_start_future_after_commit_leaves_cleanup_with_worker() {
        let audit = TestSpawnAudit::default();
        let pause = super::TestStartPause {
            entered: Arc::new(tokio::sync::Notify::new()),
            release: Arc::new(tokio::sync::Notify::new()),
        };
        let entered = Arc::clone(&pause.entered);
        let audit_for_start = audit.clone();
        let start = tokio::spawn(async move {
            spawn_pipe_inner(
                request("/bin/sleep".into(), vec!["60".into()]),
                Arc::new(OutputBuffer::new(1024)),
                SpawnOptions {
                    audit: audit_for_start,
                    start_pause: Some(pause),
                    ..SpawnOptions::default()
                },
            )
            .await
        });

        let reached_handoff = tokio::time::timeout(
            Duration::from_millis(500),
            entered.notified(),
        )
        .await
        .is_ok();
        let leader = audit.leader_pid.load(Ordering::SeqCst);
        start.abort();
        let cancelled = match tokio::time::timeout(Duration::from_secs(1), start).await {
            Ok(Err(error)) => error.is_cancelled(),
            Ok(Ok(Ok(spawned))) => {
                drop(spawned);
                false
            }
            _ => false,
        };
        let reaped = wait_for_watchdog_reaps(&audit, 1, Duration::from_secs(2)).await;

        assert!(
            reached_handoff,
            "start future never paused after the lifecycle worker took ownership"
        );
        assert!(cancelled, "start task did not observe future cancellation");
        assert!(reaped, "aborted start did not exactly reap its watchdog");
        assert_eq!(audit.leader_reaps.load(Ordering::SeqCst), 1);
        assert!(!process_exists(leader), "aborted-start leader remained observable");
        assert_eq!(audit.group_signals.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    #[serial_test::serial(unix_spawn)]
    async fn cancelling_start_inside_blocking_transaction_keeps_exact_ownership() {
        let audit = TestSpawnAudit::default();
        let pause = super::TestBlockingTransactionPause::new();
        let _release_guard = pause.release_guard();
        let pause_for_start = pause.clone();
        let audit_for_start = audit.clone();
        let start = tokio::spawn(async move {
            spawn_pipe_inner(
                request(
                    "/bin/sh".into(),
                    vec!["-c".into(), "while :; do sleep 1; done".into()],
                ),
                Arc::new(OutputBuffer::new(1024)),
                SpawnOptions {
                    audit: audit_for_start,
                    blocking_transaction_pause: Some(pause_for_start),
                    ..SpawnOptions::default()
                },
            )
            .await
        });

        let reached_transaction = tokio::time::timeout(
            Duration::from_secs(2),
            pause.wait_until_entered(),
        )
        .await
        .is_ok();
        let leader = audit.leader_pid.load(Ordering::SeqCst);
        let watchdog = audit.watchdog_pid.load(Ordering::SeqCst);
        start.abort();
        let cancelled = match tokio::time::timeout(Duration::from_secs(1), start).await {
            Ok(Err(error)) => error.is_cancelled(),
            Ok(Ok(Ok(spawned))) => {
                drop(spawned);
                false
            }
            _ => false,
        };
        pause.release();
        let exact_cleanup = tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if audit.watchdog_reaps.load(Ordering::SeqCst) == 1
                    && audit.leader_reaps.load(Ordering::SeqCst) == 1
                    && !process_exists(leader)
                    && !process_exists(watchdog)
                {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .is_ok();

        assert!(
            reached_transaction,
            "blocking spawn transaction never exposed its owned-identity window"
        );
        assert!(cancelled, "start task did not observe cancellation");
        assert!(exact_cleanup, "detached blocking transaction lost exact ownership");
        assert_eq!(audit.group_signals.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    #[serial_test::serial(unix_spawn)]
    async fn detached_blocking_start_cannot_fork_after_setup_deadline() {
        let directory = tempfile::tempdir().expect("temporary directory should be created");
        let marker = directory.path().join("expired-blocking-start.marker");
        let audit = TestSpawnAudit::default();
        let pause = super::TestBlockingTransactionPause::new();
        let _release_guard = pause.release_guard();
        let worker_finished = Arc::new(tokio::sync::Notify::new());
        let started = Instant::now();

        let result = spawn_pipe_inner(
            shell_request(&marker),
            Arc::new(OutputBuffer::new(1024)),
            SpawnOptions {
                audit: audit.clone(),
                setup_timeout: Some(Duration::from_millis(75)),
                blocking_start_pause: Some(pause.clone()),
                blocking_worker_finished: Some(Arc::clone(&worker_finished)),
                ..SpawnOptions::default()
            },
        )
        .await;
        let elapsed = started.elapsed();
        let blocking_worker_entered = tokio::time::timeout(
            Duration::from_secs(1),
            pause.wait_until_entered(),
        )
        .await
        .is_ok();
        pause.release();

        let worker_stopped = tokio::time::timeout(
            Duration::from_secs(2),
            worker_finished.notified(),
        )
        .await
        .is_ok();
        let leader = audit.leader_pid.load(Ordering::SeqCst);
        let watchdog = audit.watchdog_pid.load(Ordering::SeqCst);
        if leader > 1 || watchdog > 1 {
            let _ = tokio::time::timeout(Duration::from_secs(2), async {
                loop {
                    let watchdog_done = watchdog <= 1
                        || audit.watchdog_reaps.load(Ordering::SeqCst) >= 1;
                    let leader_done =
                        leader <= 1 || audit.leader_reaps.load(Ordering::SeqCst) >= 1;
                    if watchdog_done && leader_done
                    {
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(5)).await;
                }
            })
            .await;
        }

        assert!(blocking_worker_entered, "blocking worker never reached the queue seam");
        assert!(worker_stopped, "detached blocking worker did not finish");
        assert!(
            matches!(result, Err(ExecutionError::StartLost { .. })),
            "the async setup deadline must conservatively report StartLost"
        );
        assert!(
            elapsed < Duration::from_millis(250),
            "blocking queue time escaped the async setup deadline: {elapsed:?}"
        );
        assert_eq!(watchdog, 0, "expired detached transaction forked a watchdog");
        assert_eq!(leader, 0, "expired detached transaction forked user code");
        assert!(!marker.exists(), "expired detached transaction executed user code");
    }

    #[tokio::test]
    #[serial_test::serial(unix_spawn)]
    async fn cleanup_unproven_retries_without_blocking_relay_and_retires_signal_gate() {
        let held_audit = TestSpawnAudit::default();
        let cleanup_hold = super::TestCleanupHold::new();
        let _cleanup_release_guard = cleanup_hold.release_guard();
        let held = spawn_pipe_inner(
            request(
                "/bin/sh".into(),
                vec!["-c".into(), "while :; do sleep 1; done".into()],
            ),
            Arc::new(OutputBuffer::new(1024)),
            SpawnOptions {
                audit: held_audit.clone(),
                lifecycle_failure_before_cleanup: true,
                cleanup_hold: Some(cleanup_hold.clone()),
                ..SpawnOptions::default()
            },
        )
        .await
        .expect("held cleanup process should cross COMMITTED");
        let held_leader = held.owner.pid() as libc::pid_t;
        let hold_was_reached = tokio::time::timeout(
            Duration::from_secs(1),
            cleanup_hold.wait_until_attempted(),
        )
        .await
        .is_ok();

        let following_audit = TestSpawnAudit::default();
        let following = spawn_pipe_inner(
            request(
                "/bin/sh".into(),
                vec!["-c".into(), "while :; do sleep 1; done".into()],
            ),
            Arc::new(OutputBuffer::new(1024)),
            SpawnOptions {
                audit: following_audit.clone(),
                lifecycle_failure_before_cleanup: true,
                ..SpawnOptions::default()
            },
        )
        .await
        .expect("following cleanup process should cross COMMITTED");
        let following_leader = following.owner.pid() as libc::pid_t;
        let following_finished = tokio::time::timeout(Duration::from_millis(500), async {
            let result = following
                .owner
                .wait_reaped(Instant::now() + Duration::from_secs(1))
                .await;
            assert!(result.is_err(), "injected lifecycle failure must remain Lost");
        })
        .await
        .is_ok();

        assert!(hold_was_reached, "cleanup relay never attempted the held job");
        assert_eq!(
            held_audit.cleanup_owned_transitions.load(Ordering::SeqCst),
            1,
            "cleanup hold must occur after the gate transfers to CleanupOwned"
        );
        assert!(
            following_finished,
            "one cleanup-unproven job blocked the global cleanup relay"
        );
        assert_eq!(following_audit.watchdog_reaps.load(Ordering::SeqCst), 1);
        assert_eq!(following_audit.leader_reaps.load(Ordering::SeqCst), 1);
        assert!(!process_exists(following_leader));
        assert!(
            process_exists(held_leader),
            "held cleanup silently dropped or reaped the identity before release"
        );

        cleanup_hold.release();
        let held_result = held
            .owner
            .wait_reaped(Instant::now() + Duration::from_secs(2))
            .await;
        assert!(held_result.is_err(), "injected lifecycle failure must remain Lost");
        assert_eq!(held_audit.watchdog_reaps.load(Ordering::SeqCst), 1);
        assert_eq!(held_audit.leader_reaps.load(Ordering::SeqCst), 1);
        assert_eq!(held_audit.cleanup_retirements.load(Ordering::SeqCst), 1);
        assert!(!process_exists(held_leader));

        let signals_after_reap = held_audit.group_signals.load(Ordering::SeqCst);
        assert!(
            held.owner.force_kill().await.is_err(),
            "a retired relay-owned gate must reject negative-PGID signaling"
        );
        drop(held);
        tokio::time::sleep(Duration::from_millis(25)).await;
        assert_eq!(
            held_audit.group_signals.load(Ordering::SeqCst),
            signals_after_reap,
            "owner/handle drop signaled a PGID after relay exact reap"
        );
        drop(following);
    }

    #[test]
    fn cleanup_quarantines_echild_before_any_cached_identity_signal() {
        let audit = TestSpawnAudit::default();
        let signal_gate = Arc::new(std::sync::Mutex::new(super::SignalGate {
            phase: super::SignalPhase::CleanupOwned,
            final_kill_sent: false,
            control_fd: None,
        }));
        let (completion, completion_state) = tokio::sync::watch::channel(
            super::LifecycleCompletion::Running,
        );
        let impossible_child = libc::pid_t::MAX;
        let job = super::CleanupJob {
            child: None,
            watchdog_pid: Some(impossible_child),
            control: None,
            pgid: Some(impossible_child),
            group_state: super::CleanupGroupState::Pending,
            signal_gate: Some(Arc::clone(&signal_gate)),
            completion: Some(completion),
            failure_context: Some((
                std::io::ErrorKind::Other,
                Arc::<str>::from("injected exact ownership loss"),
            )),
            attempts: 0,
            last_error: None,
            watchdog_ownership_lost: false,
            leader_ownership_lost: false,
            retry_delay: super::CLEANUP_RETRY_DELAY,
            next_attempt: Instant::now(),
            absence_deadline: None,
            audit: audit.clone(),
            hold: None,
        };

        let step = job.run_once();

        assert!(matches!(
            step,
            super::CleanupStep::Finished { exact: false }
        ));
        assert_eq!(audit.group_signals.load(Ordering::SeqCst), 0);
        assert_eq!(
            signal_gate.lock().expect("signal gate lock").phase,
            super::SignalPhase::Retired
        );
        match completion_state.borrow().clone() {
            super::LifecycleCompletion::Failed { message, .. } => {
                assert!(message.contains("exact child ownership was lost"));
            }
            _ => panic!("ownership loss did not publish a terminal lifecycle failure"),
        }
    }

    #[test]
    fn relay_never_claims_exact_cleanup_while_group_absence_is_unproven() {
        let audit = TestSpawnAudit::default();
        let signal_gate = Arc::new(std::sync::Mutex::new(super::SignalGate {
            phase: super::SignalPhase::CleanupOwned,
            final_kill_sent: true,
            control_fd: None,
        }));
        let (completion, completion_state) = tokio::sync::watch::channel(
            super::LifecycleCompletion::Running,
        );
        let live_group = unsafe { libc::getpgrp() };
        let job = super::CleanupJob {
            child: None,
            watchdog_pid: None,
            control: None,
            pgid: Some(live_group),
            group_state: super::CleanupGroupState::Sealed,
            signal_gate: Some(Arc::clone(&signal_gate)),
            completion: Some(completion),
            failure_context: Some((
                std::io::ErrorKind::Other,
                Arc::<str>::from("injected group-absence failure"),
            )),
            attempts: 0,
            last_error: None,
            watchdog_ownership_lost: false,
            leader_ownership_lost: false,
            retry_delay: super::CLEANUP_RETRY_DELAY,
            next_attempt: Instant::now(),
            absence_deadline: Some(Instant::now() - Duration::from_millis(1)),
            audit: audit.clone(),
            hold: None,
        };

        let step = job.run_once();

        assert!(matches!(
            step,
            super::CleanupStep::Finished { exact: false }
        ));
        assert_eq!(audit.group_signals.load(Ordering::SeqCst), 0);
        assert_eq!(
            signal_gate.lock().expect("signal gate lock").phase,
            super::SignalPhase::Retired
        );
        match completion_state.borrow().clone() {
            super::LifecycleCompletion::Failed { message, .. } => {
                assert!(message.contains("process-group absence"));
            }
            _ => panic!("unproven group absence did not publish lifecycle failure"),
        }
    }

    #[tokio::test]
    #[serial_test::serial(unix_spawn)]
    async fn rapid_quick_exits_reap_each_exact_watchdog_once() {
        const PROCESS_COUNT: usize = 20;
        let audit = TestSpawnAudit::default();
        let mut starts = tokio::task::JoinSet::new();
        for _ in 0..PROCESS_COUNT {
            let audit = audit.clone();
            starts.spawn(async move {
                spawn_with_fault(
                    request("/bin/sh".into(), vec!["-c".into(), "exit 0".into()]),
                    TestSpawnFault::None,
                    &audit,
                )
                .await
            });
        }
        let mut spawned = Vec::with_capacity(PROCESS_COUNT);
        while let Some(result) = starts.join_next().await {
            spawned.push(
                result
                    .expect("rapid start task should not panic")
                    .expect("rapid quick-exit process should start"),
            );
        }
        let unique_pids = spawned
            .iter()
            .map(|process| process.owner.pid())
            .collect::<BTreeSet<_>>();
        let mut waits = tokio::task::JoinSet::new();
        for process in spawned {
            waits.spawn(async move {
                process
                    .owner
                    .wait_reaped(Instant::now() + Duration::from_secs(2))
                    .await
            });
        }
        while let Some(result) = waits.join_next().await {
            let fact = result
                .expect("rapid wait task should not panic")
                .expect("quick-exit lifecycle worker should finish exact cleanup");
            assert_eq!(fact.code, Some(0));
        }

        assert_eq!(unique_pids.len(), PROCESS_COUNT);
        assert_eq!(audit.watchdog_reaps.load(Ordering::SeqCst), PROCESS_COUNT);
        assert_eq!(audit.leader_reaps.load(Ordering::SeqCst), PROCESS_COUNT);
        assert_eq!(audit.group_signals.load(Ordering::SeqCst), PROCESS_COUNT);
    }

    #[tokio::test]
    #[serial_test::serial(unix_spawn)]
    async fn spawn_gate_queue_time_consumes_the_single_setup_deadline() {
        let directory = tempfile::tempdir().expect("temporary directory should be created");
        let marker = directory.path().join("queued-start-must-not-run.marker");
        let audit = TestSpawnAudit::default();
        let worker_finished = Arc::new(tokio::sync::Notify::new());
        let (locked_tx, locked_rx) = std::sync::mpsc::sync_channel(1);
        let blocker = std::thread::spawn(move || {
            let _gate = super::UNIX_SPAWN_GATE
                .lock()
                .expect("test spawn gate should not be poisoned");
            locked_tx.send(()).expect("test should observe locked gate");
            std::thread::sleep(Duration::from_millis(300));
        });
        locked_rx.recv().expect("test blocker should acquire spawn gate");
        let started = Instant::now();

        let result = spawn_pipe_inner(
            shell_request(&marker),
            Arc::new(OutputBuffer::new(1024)),
            SpawnOptions {
                audit: audit.clone(),
                setup_timeout: Some(Duration::from_millis(100)),
                blocking_worker_finished: Some(Arc::clone(&worker_finished)),
                ..SpawnOptions::default()
            },
        )
        .await;
        let elapsed = started.elapsed();
        blocker.join().expect("spawn gate blocker should exit");
        let worker_stopped = tokio::time::timeout(
            Duration::from_secs(2),
            worker_finished.notified(),
        )
        .await
        .is_ok();

        assert!(result.is_err(), "queued start must exhaust its original deadline");
        assert!(worker_stopped, "queued blocking worker did not finish");
        assert!(
            elapsed < Duration::from_millis(250),
            "spawn gate wait received a fresh setup budget: {elapsed:?}"
        );
        assert!(!marker.exists(), "expired queued start executed user code");
        assert_eq!(audit.watchdog_pid.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    #[serial_test::serial(unix_spawn)]
    async fn natural_exit_is_sealed_by_one_host_group_kill_before_leader_reap() {
        let audit = TestSpawnAudit::default();
        let spawned = spawn_with_fault(
            request("/bin/sh".into(), vec!["-c".into(), "exit 0".into()]),
            TestSpawnFault::None,
            &audit,
        )
        .await
        .expect("quick Unix command should start");

        let fact = spawned
            .owner
            .wait_reaped(Instant::now() + Duration::from_secs(2))
            .await
            .expect("watchdog should quiesce and reap the natural exit");

        assert_eq!(fact.code, Some(0));
        assert_eq!(fact.signal, None);
        assert_eq!(
            audit.group_signals.load(Ordering::SeqCst),
            1,
            "the host must seal the group while the exact leader remains WNOWAIT-anchored"
        );
        assert_eq!(audit.watchdog_reaps.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    #[serial_test::serial(unix_spawn)]
    async fn shutdown_during_post_reap_retirement_window_never_signals_cached_pgid() {
        let audit = TestSpawnAudit::default();
        let pause = super::TestBlockingTransactionPause::new();
        let _release_guard = pause.release_guard();
        let spawned = spawn_pipe_inner(
            request("/bin/sh".into(), vec!["-c".into(), "exit 0".into()]),
            Arc::new(OutputBuffer::new(1024)),
            SpawnOptions {
                audit: audit.clone(),
                after_leader_reap_pause: Some(pause.clone()),
                ..SpawnOptions::default()
            },
        )
        .await
        .expect("quick Unix command should start");
        let reached_post_reap = tokio::time::timeout(
            Duration::from_secs(2),
            pause.wait_until_entered(),
        )
        .await
        .is_ok();
        let signals_after_reap = audit.group_signals.load(Ordering::SeqCst);

        assert!(reached_post_reap, "lifecycle did not reach the post-reap seam");
        assert!(
            spawned.owner.force_kill().await.is_err(),
            "Closing gate accepted a post-reap force kill"
        );
        assert_eq!(audit.group_signals.load(Ordering::SeqCst), signals_after_reap);
        drop(spawned);
        assert_eq!(
            audit.group_signals.load(Ordering::SeqCst),
            signals_after_reap,
            "LifecycleHandle::drop signaled a cached PGID after leader reap"
        );
        pause.release();
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    #[serial_test::serial(unix_spawn)]
    async fn skipped_watchdog_group_kill_falls_back_before_leader_reap() {
        let directory = tempfile::tempdir().expect("temporary directory should be created");
        let descendant_marker = directory.path().join("fallback-descendant.pid");
        let audit = TestSpawnAudit::default();
        let spawned = spawn_with_fault(
            leader_first_group_request(&descendant_marker),
            TestSpawnFault::SkipFinalGroupKill,
            &audit,
        )
        .await
        .expect("leader with a background group member should start");
        let pgid = spawned.owner.pid() as libc::pid_t;
        let descendant = wait_for_pid_marker(&descendant_marker).await;

        let reaped = spawned
            .owner
            .wait_reaped(Instant::now() + Duration::from_secs(2))
            .await;
        let group_gone = wait_for_processes_gone([pgid, descendant], Duration::from_millis(300)).await;

        assert!(
            reaped.is_err(),
            "a failed watchdog final kill is infrastructure loss, not ordinary success"
        );
        assert_eq!(
            audit.group_signals.load(Ordering::SeqCst),
            1,
            "the host must issue one fallback group kill while identity is still anchored"
        );
        assert!(group_gone, "host fallback did not remove the same-group descendant");
        assert_eq!(
            audit.watchdog_reaps.load(Ordering::SeqCst),
            1,
            "the skipped-kill watchdog must be reaped exactly once"
        );
        let signals_after_reap = audit.group_signals.load(Ordering::SeqCst);
        assert!(
            spawned.owner.force_kill().await.is_err(),
            "the closed signal gate must reject post-reap signaling"
        );
        assert_eq!(
            audit.group_signals.load(Ordering::SeqCst),
            signals_after_reap,
            "no negative-PGID syscall is allowed after exact leader reap"
        );
    }

    #[tokio::test]
    #[serial_test::serial(unix_spawn)]
    async fn queued_watchdog_failure_is_drained_after_exact_watchdog_reap() {
        let audit = TestSpawnAudit::default();
        let spawned = spawn_pipe_inner(
            request("/bin/sh".into(), vec!["-c".into(), "exit 0".into()]),
            Arc::new(OutputBuffer::new(1024)),
            SpawnOptions {
                audit: audit.clone(),
                fault: TestSpawnFault::FailFinalGroupKillOnce,
                lifecycle_terminal_delay: Some(Duration::from_millis(500)),
                ..SpawnOptions::default()
            },
        )
        .await
        .expect("one-shot final-kill fault should cross COMMITTED");

        let result = spawned
            .owner
            .wait_reaped(Instant::now() + Duration::from_secs(2))
            .await;

        assert!(result.is_err(), "watchdog Failure must make lifecycle truth Lost");
        assert_eq!(
            audit.failure_frames.load(Ordering::SeqCst),
            1,
            "queued Failure was lost when watchdog reap won the terminal race"
        );
        assert_eq!(audit.watchdog_reaps.load(Ordering::SeqCst), 1);
        assert_eq!(audit.leader_reaps.load(Ordering::SeqCst), 1);
    }

    async fn wait_for_watchdog_reaps(
        audit: &TestSpawnAudit,
        expected: usize,
        timeout: Duration,
    ) -> bool {
        tokio::time::timeout(timeout, async {
            loop {
                if audit.watchdog_reaps.load(Ordering::SeqCst) >= expected {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .is_ok()
    }

    async fn wait_for_pid_marker(path: &std::path::Path) -> libc::pid_t {
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if let Ok(contents) = fs::read_to_string(path)
                    && let Ok(pid) = contents.trim().parse::<libc::pid_t>()
                {
                    return pid;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap_or_else(|_| panic!("PID marker was not published: {}", path.display()))
    }

    async fn wait_for_processes_gone(
        pids: impl IntoIterator<Item = libc::pid_t>,
        timeout: Duration,
    ) -> bool {
        let pids = pids.into_iter().collect::<Vec<_>>();
        tokio::time::timeout(timeout, async {
            loop {
                if pids.iter().all(|pid| !process_exists(*pid)) {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .is_ok()
    }

    fn process_exists(pid: libc::pid_t) -> bool {
        // SAFETY: signal zero only probes the supplied PID and has no process-side effect.
        if unsafe { libc::kill(pid, 0) } == 0 {
            return true;
        }
        std::io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH)
    }

    fn shell_request(marker: &std::path::Path) -> NormalizedExecutionRequest {
        request(
            "/bin/sh".into(),
            vec![
                "-c".into(),
                "printf owned > \"$1\"".into(),
                "nomifun-watchdog-test".into(),
                marker.as_os_str().to_owned(),
            ],
        )
    }

    fn long_running_group_request(marker: &std::path::Path) -> NormalizedExecutionRequest {
        shell_group_request(
            marker,
            "sleep 60 </dev/null >/dev/null 2>&1 & descendant=$!; printf '%s\\n' \"$descendant\" > \"$1\"; wait \"$descendant\"",
        )
    }

    #[cfg(target_os = "linux")]
    fn leader_first_group_request(marker: &std::path::Path) -> NormalizedExecutionRequest {
        shell_group_request(
            marker,
            "sleep 60 </dev/null >/dev/null 2>&1 & descendant=$!; printf '%s\\n' \"$descendant\" > \"$1\"",
        )
    }

    fn shell_group_request(
        marker: &std::path::Path,
        script: &'static str,
    ) -> NormalizedExecutionRequest {
        request(
            "/bin/sh".into(),
            vec![
                "-c".into(),
                script.into(),
                "nomifun-watchdog-test".into(),
                marker.as_os_str().to_owned(),
            ],
        )
    }

    fn request(program: OsString, args: Vec<OsString>) -> NormalizedExecutionRequest {
        let cwd = std::env::current_dir().expect("current directory should exist");
        NormalizedExecutionRequest {
            owner: ExecutionOwner::new(uuid::Uuid::now_v7(), uuid::Uuid::now_v7()),
            command: CommandSpec::Program { program, args },
            cwd: cwd.clone(),
            env: BTreeMap::new(),
            transport: Transport::Pipe,
            policy: ExecutionPolicy::default(),
            capability: CapabilityPolicy::local_owner(cwd),
        }
    }
}

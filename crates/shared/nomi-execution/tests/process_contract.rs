#![cfg(any(unix, windows))]

use std::{
    collections::BTreeMap,
    ffi::OsString,
    fs,
    path::Path,
    time::{Duration, Instant},
};

#[cfg(windows)]
use std::io;

use nomi_execution::{
    CapabilityPolicy, CommandSpec, ExecutionError, ExecutionOutcome, ExecutionOwner,
    ExecutionPolicy, NormalizedExecutionRequest, OutputCursor, PollResult, ProcessSupervisor,
    SupervisorConfig, Transport,
};

fn helper_binary() -> &'static str {
    option_env!("CARGO_BIN_EXE_execution_test_helper")
        .expect("Cargo did not build the execution_test_helper binary")
}

#[cfg(unix)]
fn low_fd_harness_binary() -> &'static str {
    option_env!("CARGO_BIN_EXE_low_fd_harness")
        .expect("Cargo did not build the low_fd_harness binary")
}

#[cfg(unix)]
fn fd_sentinel_harness_binary() -> &'static str {
    option_env!("CARGO_BIN_EXE_fd_sentinel_harness")
        .expect("Cargo did not build the fd_sentinel_harness binary")
}

fn request(program: impl Into<OsString>, args: impl IntoIterator<Item = OsString>) -> NormalizedExecutionRequest {
    let cwd = std::env::current_dir().expect("current directory should exist");
    NormalizedExecutionRequest {
        owner: ExecutionOwner::new(uuid::Uuid::now_v7(), uuid::Uuid::now_v7()),
        command: CommandSpec::Program {
            program: program.into(),
            args: args.into_iter().collect(),
        },
        cwd: cwd.clone(),
        env: BTreeMap::new(),
        transport: Transport::Pipe,
        policy: ExecutionPolicy::default(),
        capability: CapabilityPolicy::local_owner(cwd),
    }
}

fn helper_request(args: &[&str]) -> NormalizedExecutionRequest {
    request(
        helper_binary(),
        args.iter().map(OsString::from).collect::<Vec<_>>(),
    )
}

async fn wait_for_terminal(
    supervisor: &ProcessSupervisor,
    handle: &nomi_execution::ExecutionHandle,
) -> ExecutionOutcome {
    let result = tokio::time::timeout(
        Duration::from_secs(5),
        supervisor.poll(
            &handle.owner,
            &handle.session_id,
            OutputCursor::START,
            Instant::now() + Duration::from_secs(30),
        ),
    )
    .await
    .expect("terminal poll must stay bounded")
    .expect("terminal poll should succeed");
    match result {
        PollResult::Finished(outcome) => outcome,
        PollResult::Running { .. } => panic!("helper should have exited before the bounded poll"),
    }
}

#[tokio::test]
#[cfg(unix)]
async fn unix_pipe_preserves_zero_and_nonzero_exit_codes() {
    for expected in [0, 7] {
        let supervisor = ProcessSupervisor::new(SupervisorConfig::default());
        let handle = supervisor
            .start(helper_request(&["exit", &expected.to_string()]))
            .await
            .expect("Unix pipe helper should start");

        let poll_started = Instant::now();
        let outcome = tokio::time::timeout(
            Duration::from_millis(250),
            wait_for_terminal(&supervisor, &handle),
        )
        .await
        .expect("quick natural exit must wake a far-yield poll within 250 ms");
        assert!(poll_started.elapsed() < Duration::from_millis(250));
        let ExecutionOutcome::Exited { code, signal, .. } = outcome else {
            panic!("helper exit should produce Exited, got {outcome:?}");
        };
        assert_eq!(code, Some(expected));
        assert_eq!(signal, None);
    }
}

#[tokio::test]
#[cfg(unix)]
async fn public_supervisor_preserves_exit_codes_with_nofile_soft_limit_128() {
    let mut command = tokio::process::Command::new(low_fd_harness_binary());
    command.arg(helper_binary()).kill_on_drop(true);

    let output = tokio::time::timeout(Duration::from_secs(8), command.output())
        .await
        .expect("low-FD harness must stay within its bounded runtime")
        .expect("low-FD harness process should launch");

    assert!(
        output.status.success(),
        "public supervisor failed under RLIMIT_NOFILE=128: status={:?}, stdout={}, stderr={}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[tokio::test]
#[cfg(unix)]
async fn public_supervisor_closes_inherited_high_fd_sentinel() {
    let mut command = tokio::process::Command::new(fd_sentinel_harness_binary());
    command.arg(helper_binary()).kill_on_drop(true);

    let output = tokio::time::timeout(Duration::from_secs(12), command.output())
        .await
        .expect("high-FD sentinel harness must stay within its bounded runtime")
        .expect("high-FD sentinel harness process should launch");

    assert!(
        output.status.success(),
        "public supervisor retained an inherited FD >=4097: status={:?}, stdout={}, stderr={}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[tokio::test]
#[cfg(unix)]
async fn unix_pipe_round_trips_stdin_and_close_stdin_delivers_eof() {
    let supervisor = ProcessSupervisor::new(SupervisorConfig::default());
    let handle = supervisor
        .start(helper_request(&["echo-stdin"]))
        .await
        .expect("Unix pipe helper should start");

    supervisor
        .write(&handle.owner, &handle.session_id, b"hello\0world\n")
        .await
        .expect("stdin write should succeed");
    supervisor
        .close_stdin(&handle.owner, &handle.session_id)
        .await
        .expect("closing stdin should succeed");

    let outcome = wait_for_terminal(&supervisor, &handle).await;
    let ExecutionOutcome::Exited { code, output, .. } = outcome else {
        panic!("echo helper should produce Exited, got {outcome:?}");
    };
    assert_eq!(code, Some(0));
    assert_eq!(output.raw_bytes(), b"hello\0world\n");
}

#[tokio::test]
#[cfg(unix)]
async fn invalid_executable_is_a_stable_spawn_failure_without_a_session() {
    let supervisor = ProcessSupervisor::new(SupervisorConfig::default());
    let missing = Path::new("/definitely/not/a/nomifun-executable");

    let started = tokio::time::timeout(
        Duration::from_secs(6),
        supervisor.start(request(missing.as_os_str(), Vec::<OsString>::new())),
    )
    .await
    .expect("invalid executable spawn must finish within the shared setup deadline");
    let error = match started {
        Ok(_) => panic!("invalid executable must fail before a session is returned"),
        Err(error) => error,
    };

    assert_eq!(error.code(), "spawn_failed");
    assert!(matches!(error, ExecutionError::Transport { .. }) == false);
}

#[tokio::test]
#[cfg(unix)]
async fn cancel_removes_the_leader_and_same_group_grandchild() {
    let directory = tempfile::tempdir().expect("temporary directory should be created");
    let marker = directory.path().join("grandchild.pid");
    let supervisor = ProcessSupervisor::new(SupervisorConfig::default());
    let handle = supervisor
        .start(helper_request(&[
            "spawn-grandchild",
            marker.to_str().expect("temporary path should be UTF-8"),
        ]))
        .await
        .expect("grandchild helper should start");
    let leader = supervisor
        .status(&handle.owner, &handle.session_id)
        .await
        .expect("started leader should have status")
        .pid as libc::pid_t;
    let grandchild = wait_for_pid_marker(&marker).await as libc::pid_t;
    let mut cleanup = PidCleanup::new([leader, grandchild]);

    let outcome = tokio::time::timeout(
        Duration::from_secs(6),
        supervisor.cancel(&handle.owner, &handle.session_id),
    )
    .await
    .expect("group cancellation must stay within its frozen budget")
    .expect("group cancellation should resolve");

    let ExecutionOutcome::Cancelled { cleanup: report, .. } = outcome else {
        panic!("group cancellation should be terminal Cancelled, got {outcome:?}");
    };
    assert!(report.interrupt_attempted);
    wait_for_processes_gone([leader, grandchild]).await;
    cleanup.disarm();
}

#[tokio::test]
#[cfg(unix)]
async fn ignored_sigint_escalates_to_sigterm_and_removes_the_group() {
    let directory = tempfile::tempdir().expect("temporary directory should be created");
    let marker = directory.path().join("interrupt-ignoring-grandchild.pid");
    let supervisor = ProcessSupervisor::new(SupervisorConfig::default());
    let handle = supervisor
        .start(helper_request(&[
            "spawn-ignore-group",
            marker.to_str().expect("temporary path should be UTF-8"),
        ]))
        .await
        .expect("interrupt-ignoring group should start");
    let leader = supervisor
        .status(&handle.owner, &handle.session_id)
        .await
        .expect("started leader should have status")
        .pid as libc::pid_t;
    let grandchild = wait_for_pid_marker(&marker).await as libc::pid_t;
    let mut cleanup = PidCleanup::new([leader, grandchild]);
    let cancellation_started = Instant::now();

    let outcome = tokio::time::timeout(
        Duration::from_secs(4),
        supervisor.cancel(&handle.owner, &handle.session_id),
    )
    .await
    .expect("SIGINT-to-SIGTERM escalation must stay bounded")
    .expect("group cancellation should resolve");
    let elapsed = cancellation_started.elapsed();

    let ExecutionOutcome::Cancelled { cleanup: report, .. } = outcome else {
        panic!("escalated group cancellation should be Cancelled, got {outcome:?}");
    };
    assert!(report.interrupt_attempted);
    assert!(report.terminate_attempted);
    assert!(!report.force_kill_attempted);
    assert!(
        elapsed >= Duration::from_millis(900),
        "SIGTERM was sent before the one-second SIGINT grace: {elapsed:?}"
    );
    assert!(elapsed < Duration::from_secs(3));
    wait_for_processes_gone([leader, grandchild]).await;
    cleanup.disarm();
}

#[tokio::test]
#[cfg(unix)]
async fn leader_exit_does_not_publish_success_while_same_group_descendant_survives() {
    let directory = tempfile::tempdir().expect("temporary directory should be created");
    let marker = directory.path().join("leader-first-grandchild.pid");
    let supervisor = ProcessSupervisor::new(SupervisorConfig::default());
    let handle = supervisor
        .start(helper_request(&[
            "leader-first",
            marker.to_str().expect("temporary path should be UTF-8"),
        ]))
        .await
        .expect("leader-first helper should start");
    let leader = supervisor
        .status(&handle.owner, &handle.session_id)
        .await
        .expect("started leader should have status")
        .pid as libc::pid_t;
    let grandchild = wait_for_pid_marker(&marker).await as libc::pid_t;
    let mut cleanup = PidCleanup::new([leader, grandchild]);

    let outcome = tokio::time::timeout(
        Duration::from_millis(250),
        wait_for_terminal(&supervisor, &handle),
    )
    .await
    .expect("leader-first cleanup should finish inside the quick-exit boundary");

    let ExecutionOutcome::Exited { code, cleanup: report, .. } = outcome else {
        panic!("clean leader-first exit should remain Exited, got {outcome:?}");
    };
    assert_eq!(code, Some(0));
    assert!(report.reaped);
    wait_for_processes_gone([leader, grandchild]).await;
    cleanup.disarm();
}

#[tokio::test]
#[cfg(unix)]
async fn observable_setsid_escape_is_lost_instead_of_waiting_for_fake_pipe_eof() {
    let directory = tempfile::tempdir().expect("temporary directory should be created");
    let marker = directory.path().join("escaped-descendant.pid");
    let supervisor = ProcessSupervisor::new(SupervisorConfig::default());
    let handle = supervisor
        .start(helper_request(&[
            "setsid-escape",
            marker.to_str().expect("temporary path should be UTF-8"),
        ]))
        .await
        .expect("setsid escape helper should start");
    let escaped = wait_for_pid_marker(&marker).await as libc::pid_t;
    let mut cleanup = PidCleanup::new([escaped]);

    let result = tokio::time::timeout(
        Duration::from_secs(1),
        supervisor.poll(
            &handle.owner,
            &handle.session_id,
            OutputCursor::START,
            Instant::now() + Duration::from_secs(30),
        ),
    )
    .await
    .expect("an inherited pipe held by an escaped descendant must not stall the waiter")
    .expect("poll should resolve the escaped session");

    let PollResult::Finished(ExecutionOutcome::Lost { cleanup: report, .. }) = result else {
        panic!("detectable setsid escape must be Lost, got {result:?}");
    };
    assert!(
        report
            .errors
            .iter()
            .any(|error| error.contains("output reader timed out")),
        "Lost cleanup should identify the missing pipe EOF: {:?}",
        report.errors
    );
    cleanup.kill_all();
    wait_for_processes_gone([escaped]).await;
    cleanup.disarm();
}

#[cfg(unix)]
async fn wait_for_pid_marker(path: &Path) -> u32 {
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if let Ok(contents) = fs::read_to_string(path)
                && let Ok(pid) = contents.trim().parse::<u32>()
            {
                return pid;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("PID marker was not published: {}", path.display()))
}

#[cfg(unix)]
async fn wait_for_processes_gone(pids: impl IntoIterator<Item = libc::pid_t>) {
    let pids = pids.into_iter().collect::<Vec<_>>();
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if pids.iter().all(|pid| !process_exists(*pid)) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("processes still existed after cleanup: {pids:?}"));
}

#[cfg(unix)]
fn process_exists(pid: libc::pid_t) -> bool {
    // SAFETY: signal zero probes liveness without delivering a signal.
    if unsafe { libc::kill(pid, 0) } == 0 {
        return true;
    }
    std::io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH)
}

#[cfg(unix)]
struct PidCleanup {
    pids: Vec<libc::pid_t>,
    armed: bool,
}

#[cfg(unix)]
impl PidCleanup {
    fn new(pids: impl IntoIterator<Item = libc::pid_t>) -> Self {
        Self {
            pids: pids.into_iter().collect(),
            armed: true,
        }
    }

    fn kill_all(&self) {
        for pid in &self.pids {
            // SAFETY: the guard stores only PIDs published by this test's helpers.
            let _ = unsafe { libc::kill(*pid, libc::SIGKILL) };
        }
    }

    fn disarm(&mut self) {
        self.armed = false;
    }
}

#[cfg(unix)]
impl Drop for PidCleanup {
    fn drop(&mut self) {
        if self.armed {
            self.kill_all();
        }
    }
}

#[cfg(windows)]
#[tokio::test]
async fn natural_exit_returns_promptly() {
    for expected in [0, 7] {
        let supervisor = ProcessSupervisor::new(SupervisorConfig::default());
        let handle = supervisor
            .start(helper_request(&["exit", &expected.to_string()]))
            .await
            .expect("Windows pipe helper should start");

        let poll_started = Instant::now();
        let outcome = tokio::time::timeout(
            Duration::from_millis(250),
            wait_for_terminal(&supervisor, &handle),
        )
        .await
        .expect("quick natural exit must wake a far-yield poll within 250 ms");
        assert!(poll_started.elapsed() < Duration::from_millis(250));
        let ExecutionOutcome::Exited {
            code,
            signal,
            cleanup,
            ..
        } = outcome
        else {
            panic!("helper exit should produce Exited, got {outcome:?}");
        };
        assert_eq!(code, Some(expected));
        assert_eq!(signal, None);
        assert!(cleanup.reaped);
    }
}

#[cfg(windows)]
#[tokio::test]
async fn windows_pipe_round_trips_stdin_and_close_stdin_delivers_eof() {
    let supervisor = ProcessSupervisor::new(SupervisorConfig::default());
    let handle = supervisor
        .start(helper_request(&["echo-stdin"]))
        .await
        .expect("Windows pipe helper should start");

    supervisor
        .write(&handle.owner, &handle.session_id, b"hello\0world\n")
        .await
        .expect("stdin write should succeed");
    supervisor
        .close_stdin(&handle.owner, &handle.session_id)
        .await
        .expect("closing stdin should succeed");

    let outcome = wait_for_terminal(&supervisor, &handle).await;
    let ExecutionOutcome::Exited {
        code,
        signal,
        output,
        cleanup,
    } = outcome
    else {
        panic!("echo helper should produce Exited, got {outcome:?}");
    };
    assert_eq!(code, Some(0));
    assert_eq!(signal, None);
    assert_eq!(output.raw_bytes(), b"hello\0world\n");
    assert!(cleanup.reaped);
}

#[cfg(windows)]
#[tokio::test]
async fn windows_invalid_executable_is_a_stable_spawn_failure_without_a_session() {
    let directory = tempfile::tempdir().expect("temporary directory should be created");
    let missing = directory.path().join("definitely-missing-nomifun-executable.exe");
    let supervisor = ProcessSupervisor::new(SupervisorConfig::default());

    let started = tokio::time::timeout(
        Duration::from_secs(6),
        supervisor.start(request(missing.into_os_string(), Vec::<OsString>::new())),
    )
    .await
    .expect("invalid executable spawn must finish within the shared setup deadline");
    let error = match started {
        Ok(_) => panic!("invalid executable must fail before a session is returned"),
        Err(error) => error,
    };

    assert_eq!(error.code(), "spawn_failed");
    assert!(!matches!(error, ExecutionError::Transport { .. }));
}

#[cfg(windows)]
#[tokio::test]
async fn windows_cancel_reaps_the_leader_and_grandchild_within_five_seconds() {
    let directory = tempfile::tempdir().expect("temporary directory should be created");
    let marker = directory.path().join("grandchild.pid");
    let supervisor = ProcessSupervisor::new(SupervisorConfig::default());
    let handle = supervisor
        .start(request(
            helper_binary(),
            [
                OsString::from("spawn-grandchild"),
                marker.as_os_str().to_owned(),
            ],
        ))
        .await
        .expect("Windows grandchild helper should start");
    let leader_pid = supervisor
        .status(&handle.owner, &handle.session_id)
        .await
        .expect("started leader should have status")
        .pid;
    let leader =
        ExactWindowsProcess::open(leader_pid).expect("leader exact process handle should open");
    let grandchild_pid = wait_for_windows_pid_marker(&marker).await;
    let grandchild = ExactWindowsProcess::open(grandchild_pid)
        .expect("grandchild exact process handle should open");

    let cancellation_started = Instant::now();
    let outcome = tokio::time::timeout(
        Duration::from_secs(5),
        supervisor.cancel(&handle.owner, &handle.session_id),
    )
    .await
    .expect("Windows Job cancellation must finish within five seconds")
    .expect("Windows Job cancellation should resolve");
    let elapsed = cancellation_started.elapsed();

    let ExecutionOutcome::Cancelled { cleanup, .. } = outcome else {
        panic!("Windows Job cancellation should be terminal Cancelled, got {outcome:?}");
    };
    assert!(cleanup.interrupt_attempted);
    assert!(cleanup.terminate_attempted || cleanup.force_kill_attempted);
    assert!(cleanup.reaped);
    assert!(
        elapsed < Duration::from_secs(5),
        "Windows cancellation exceeded its frozen budget: {elapsed:?}"
    );

    leader
        .wait_terminated(Duration::from_secs(2), "leader")
        .await;
    grandchild
        .wait_terminated(Duration::from_secs(2), "grandchild")
        .await;
}

#[cfg(windows)]
#[tokio::test]
async fn windows_leader_exit_waits_for_job_descendant_cleanup_before_success() {
    let directory = tempfile::tempdir().expect("temporary directory should be created");
    let marker = directory.path().join("leader-first-grandchild.pid");
    let exit_gate = directory.path().join("leader-exit.gate");
    let supervisor = ProcessSupervisor::new(SupervisorConfig::default());
    let handle = supervisor
        .start(request(
            helper_binary(),
            [
                OsString::from("leader-first-gated"),
                marker.as_os_str().to_owned(),
                exit_gate.as_os_str().to_owned(),
            ],
        ))
        .await
        .expect("Windows leader-first helper should start");
    let grandchild_pid = wait_for_windows_pid_marker(&marker).await;
    let grandchild = ExactWindowsProcess::open(grandchild_pid)
        .expect("grandchild exact process handle should open");
    fs::write(&exit_gate, b"go").expect("leader exit gate should be published");

    let outcome = tokio::time::timeout(
        Duration::from_millis(250),
        wait_for_terminal(&supervisor, &handle),
    )
    .await
    .expect("leader-first Job cleanup should stay inside the quick-exit boundary");
    let ExecutionOutcome::Exited {
        code,
        signal,
        cleanup,
        ..
    } = outcome
    else {
        panic!("leader-first helper should remain a truthful Exited outcome, got {outcome:?}");
    };
    assert_eq!(code, Some(0));
    assert_eq!(signal, None);
    assert!(cleanup.reaped);
    grandchild
        .wait_terminated(Duration::from_secs(2), "leader-first grandchild")
        .await;
}

#[cfg(windows)]
#[tokio::test]
async fn windows_preserves_complex_unicode_argv_environment_and_cwd() {
    let directory = tempfile::tempdir().expect("temporary working directory should be created");
    let cwd = directory
        .path()
        .canonicalize()
        .expect("temporary working directory should canonicalize");
    let first = OsString::from("涓枃 spaced \\");
    let second = OsString::from(r#"quote " and trailing \\"#);
    let env_key = OsString::from("NOMIFUN_WINDOWS_ENV_CASE");
    let env_value = OsString::from("鍊?value");
    let mut execution = request(
        helper_binary(),
        [
            OsString::from("print-args-env-cwd"),
            first.clone(),
            second.clone(),
            env_key.clone(),
            cwd.as_os_str().to_owned(),
        ],
    );
    execution.cwd = cwd.clone();
    execution.capability = CapabilityPolicy::local_owner(cwd.clone());
    execution
        .env
        .insert(OsString::from("nomifun_windows_env_case"), env_value.clone());

    let supervisor = ProcessSupervisor::new(SupervisorConfig::default());
    let handle = supervisor
        .start(execution)
        .await
        .expect("complex Windows argv/env/cwd helper should start");
    let outcome = wait_for_terminal(&supervisor, &handle).await;
    let ExecutionOutcome::Exited { code, output, .. } = outcome else {
        panic!("complex Windows argv/env/cwd helper should exit, got {outcome:?}");
    };
    assert_eq!(code, Some(0));
    let expected = [first, second, env_value, cwd.into_os_string()]
        .into_iter()
        .map(|field| {
            let field = field.to_string_lossy();
            format!("{}:{field}\n", field.as_bytes().len())
        })
        .collect::<String>();
    assert_eq!(output.text(), expected);
}

#[cfg(windows)]
async fn wait_for_windows_pid_marker(path: &Path) -> u32 {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if let Ok(contents) = fs::read_to_string(path)
                && let Ok(pid) = contents.trim().parse::<u32>()
            {
                return pid;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("PID marker was not published: {}", path.display()))
}

#[cfg(windows)]
struct OwnedWindowsHandle(windows_sys::Win32::Foundation::HANDLE);

#[cfg(windows)]
impl OwnedWindowsHandle {
    fn new(
        handle: windows_sys::Win32::Foundation::HANDLE,
        operation: &'static str,
    ) -> io::Result<Self> {
        use windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE;

        if handle.is_null() || handle == INVALID_HANDLE_VALUE {
            Err(io::Error::new(
                io::Error::last_os_error().kind(),
                format!("{operation}: {}", io::Error::last_os_error()),
            ))
        } else {
            Ok(Self(handle))
        }
    }

    fn raw(&self) -> windows_sys::Win32::Foundation::HANDLE {
        self.0
    }
}

#[cfg(windows)]
impl Drop for OwnedWindowsHandle {
    fn drop(&mut self) {
        // SAFETY: the wrapper owns one valid kernel handle and closes it exactly once.
        let _ = unsafe { windows_sys::Win32::Foundation::CloseHandle(self.0) };
    }
}

#[cfg(windows)]
struct ExactWindowsProcess {
    pid: u32,
    handle: OwnedWindowsHandle,
}

#[cfg(windows)]
impl ExactWindowsProcess {
    fn open(pid: u32) -> io::Result<Self> {
        use windows_sys::Win32::System::Threading::{
            OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SYNCHRONIZE, PROCESS_TERMINATE,
        };

        // SAFETY: OpenProcess returns a new non-inheritable handle for the exact process object.
        let handle = OwnedWindowsHandle::new(
            unsafe {
                OpenProcess(
                    PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_SYNCHRONIZE | PROCESS_TERMINATE,
                    0,
                    pid,
                )
            },
            "OpenProcess",
        )?;
        Ok(Self { pid, handle })
    }

    fn raw(&self) -> windows_sys::Win32::Foundation::HANDLE {
        self.handle.raw()
    }

    async fn wait_terminated(&self, timeout: Duration, label: &str) {
        use windows_sys::Win32::Foundation::{WAIT_FAILED, WAIT_OBJECT_0, WAIT_TIMEOUT};
        use windows_sys::Win32::System::Threading::WaitForSingleObject;

        let deadline = Instant::now() + timeout;
        loop {
            // SAFETY: the exact process handle remains live while it is inspected.
            match unsafe { WaitForSingleObject(self.raw(), 0) } {
                WAIT_OBJECT_0 => return,
                WAIT_TIMEOUT if Instant::now() < deadline => {
                    tokio::time::sleep(Duration::from_millis(5)).await;
                }
                WAIT_TIMEOUT => panic!("{label} pid={} was still alive after {timeout:?}", self.pid),
                WAIT_FAILED => panic!(
                    "waiting for {label} pid={} failed: {}",
                    self.pid,
                    io::Error::last_os_error()
                ),
                result => panic!(
                    "waiting for {label} pid={} returned unexpected status {result:#x}",
                    self.pid
                ),
            }
        }
    }
}

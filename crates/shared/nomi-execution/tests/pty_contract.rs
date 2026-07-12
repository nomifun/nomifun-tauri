#![cfg(any(unix, windows))]

use std::{
    collections::BTreeMap,
    ffi::OsString,
    time::{Duration, Instant},
};

#[cfg(any(unix, windows))]
use std::{fs, path::Path};

use nomi_execution::{
    CapabilityPolicy, CommandSpec, ExecutionError, ExecutionOutcome, ExecutionOwner,
    ExecutionPolicy, NormalizedExecutionRequest, OutputCursor, OutputStream, PollResult,
    ProcessState, ProcessSupervisor, SupervisorConfig, Transport,
};
#[cfg(target_os = "macos")]
use nomi_execution::SandboxPolicy;

const PTY_COLS: u16 = 80;
const PTY_ROWS: u16 = 24;

fn helper_binary() -> &'static str {
    env!("CARGO_BIN_EXE_execution_test_helper")
}

fn helper_request(args: &[&str]) -> NormalizedExecutionRequest {
    let cwd = std::env::current_dir().expect("current directory should exist");
    NormalizedExecutionRequest {
        owner: ExecutionOwner::new(uuid::Uuid::now_v7(), uuid::Uuid::now_v7()),
        command: CommandSpec::Program {
            program: helper_binary().into(),
            args: args.iter().map(OsString::from).collect(),
        },
        cwd: cwd.clone(),
        env: BTreeMap::new(),
        transport: Transport::Pty {
            cols: PTY_COLS,
            rows: PTY_ROWS,
        },
        policy: ExecutionPolicy::default(),
        capability: CapabilityPolicy::local_owner(cwd),
    }
}

async fn start_pty(
    supervisor: &std::sync::Arc<ProcessSupervisor>,
    args: &[&str],
) -> Result<nomi_execution::ExecutionHandle, ExecutionError> {
    supervisor.start(helper_request(args)).await
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
    .expect("terminal PTY poll must stay bounded")
    .expect("terminal PTY poll should succeed");
    match result {
        PollResult::Finished(outcome) => outcome,
        PollResult::Running { .. } => panic!("PTY helper should have reached a terminal state"),
    }
}

fn contains_terminal_line(bytes: &[u8], line: &[u8]) -> bool {
    bytes.windows(line.len()).any(|window| window == line)
        || line.ends_with(b"\n")
            && {
                let mut crlf = line[..line.len() - 1].to_vec();
                crlf.extend_from_slice(b"\r\n");
                bytes
                    .windows(crlf.len())
                    .any(|window| window == crlf.as_slice())
            }
}

fn strip_terminal_controls(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut plain = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] != 0x1b {
            plain.push(bytes[index]);
            index += 1;
            continue;
        }
        index += 1;
        let Some(kind) = bytes.get(index).copied() else {
            break;
        };
        match kind {
            b'[' => {
                index += 1;
                while index < bytes.len() {
                    let byte = bytes[index];
                    index += 1;
                    if (0x40..=0x7e).contains(&byte) {
                        break;
                    }
                }
            }
            b']' => {
                index += 1;
                while index < bytes.len() {
                    if bytes[index] == 0x07 {
                        index += 1;
                        break;
                    }
                    if bytes[index] == 0x1b
                        && bytes.get(index + 1).copied() == Some(b'\\')
                    {
                        index += 2;
                        break;
                    }
                    index += 1;
                }
            }
            _ => index += 1,
        }
    }
    String::from_utf8(plain).expect("terminal control stripping preserves UTF-8 bytes")
}

#[tokio::test]
async fn pty_echoes_stdin_and_close_stdin_delivers_eof() {
    let supervisor = ProcessSupervisor::new(SupervisorConfig::default());
    let handle = start_pty(&supervisor, &["echo-stdin"])
        .await
        .expect("PTY echo helper should start");

    supervisor
        .write(&handle.owner, &handle.session_id, b"hello from pty")
        .await
        .expect("PTY stdin write should succeed");
    let close_result = supervisor
        .close_stdin(&handle.owner, &handle.session_id)
        .await;
    #[cfg(unix)]
    close_result.expect("closing Unix PTY stdin should succeed");
    #[cfg(windows)]
    {
        let error = close_result
            .expect_err("ConPTY must not overclaim a generic EOF contract");
        assert_eq!(error.code(), "io");
        assert!(
            error
                .to_string()
                .contains("cannot prove generic stdin EOF")
        );
    }

    let outcome = wait_for_terminal(&supervisor, &handle).await;
    let ExecutionOutcome::Exited {
        code,
        signal,
        output,
        cleanup,
    } = outcome
    else {
        panic!("PTY echo helper should exit, got {outcome:?}");
    };
    assert_eq!(code, Some(0));
    assert_eq!(signal, None);
    assert!(cleanup.reaped);
    assert!(
        output
            .chunks
            .iter()
            .all(|chunk| chunk.stream == OutputStream::Pty),
        "PTY output must use only the merged PTY stream: {:?}",
        output.chunks
    );
    assert!(
        output
            .raw_bytes()
            .windows(b"hello from pty".len())
            .any(|window| window == b"hello from pty"),
        "PTY echo did not contain the written bytes: {:?}",
        output.raw_bytes()
    );
}

#[cfg(unix)]
#[tokio::test]
async fn unix_pty_close_stdin_flushes_unterminated_canonical_input_then_eof() {
    let supervisor = ProcessSupervisor::new(SupervisorConfig::default());
    let handle = start_pty(&supervisor, &["echo-stdin"])
        .await
        .expect("PTY unterminated echo helper should start");

    supervisor
        .write(&handle.owner, &handle.session_id, b"unterminated")
        .await
        .expect("PTY stdin write should succeed");
    supervisor
        .close_stdin(&handle.owner, &handle.session_id)
        .await
        .expect("canonical PTY EOF should complete an unterminated line");

    let outcome = wait_for_terminal(&supervisor, &handle).await;
    let ExecutionOutcome::Exited { code, output, .. } = outcome else {
        panic!("PTY unterminated echo helper should exit, got {outcome:?}");
    };
    assert_eq!(code, Some(0));
    assert!(
        output
            .raw_bytes()
            .windows(b"unterminated".len())
            .any(|window| window == b"unterminated")
    );
}

#[tokio::test]
async fn pty_decodes_utf8_split_one_byte_at_a_time() {
    let supervisor = ProcessSupervisor::new(SupervisorConfig::default());
    let handle = start_pty(&supervisor, &["emit-split-utf8"])
        .await
        .expect("split UTF-8 PTY helper should start");

    let outcome = wait_for_terminal(&supervisor, &handle).await;
    let ExecutionOutcome::Exited { code, output, .. } = outcome else {
        panic!("split UTF-8 PTY helper should exit, got {outcome:?}");
    };
    assert_eq!(code, Some(0));
    assert_eq!(strip_terminal_controls(&output.text()), "中文🙂");
    assert_eq!(output.encoding.source_encoding, "utf-8");
    assert_eq!(output.encoding.decode_errors, 0);
    assert!(
        output
            .chunks
            .iter()
            .all(|chunk| chunk.stream == OutputStream::Pty)
    );
}

#[cfg(unix)]
#[tokio::test]
async fn pty_preserves_fast_output_after_a_prior_terminal_session() {
    let supervisor = ProcessSupervisor::new(SupervisorConfig::default());
    let warmup = start_pty(&supervisor, &["exit", "0"])
        .await
        .expect("warmup PTY helper should start");
    let _ = wait_for_terminal(&supervisor, &warmup).await;

    let handle = start_pty(&supervisor, &["emit-split-utf8"])
        .await
        .expect("fast PTY helper should start after warmup");
    let outcome = wait_for_terminal(&supervisor, &handle).await;
    let ExecutionOutcome::Exited { code, output, .. } = outcome else {
        panic!("fast PTY helper should exit, got {outcome:?}");
    };
    assert_eq!(code, Some(0));
    assert_eq!(strip_terminal_controls(&output.text()), "中文🙂");
}

#[tokio::test]
async fn quick_pty_exit_wakes_a_far_yield_within_one_second() {
    let supervisor = ProcessSupervisor::new(SupervisorConfig::default());
    let started = Instant::now();
    let handle = start_pty(&supervisor, &["exit", "0"])
        .await
        .expect("quick PTY helper should start");

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
    .expect("quick PTY exit must not wait for ConPTY EOF or the far yield")
    .expect("quick PTY poll should succeed");
    assert!(
        started.elapsed() < Duration::from_secs(1),
        "quick PTY command exceeded its one-second contract"
    );

    let PollResult::Finished(ExecutionOutcome::Exited {
        code,
        signal,
        cleanup,
        ..
    }) = result
    else {
        panic!("quick PTY helper should exit, got {result:?}");
    };
    assert_eq!(code, Some(0));
    assert_eq!(signal, None);
    assert!(cleanup.reaped);
}

#[cfg(target_os = "macos")]
#[tokio::test]
async fn macos_seatbelt_program_pty_blocks_out_of_root_writes() {
    // Darwin's trusted temporary directories are intentionally writable in
    // the profile. Keep both fixtures beside the checkout so `outside` really
    // exercises the declared write-root boundary.
    let fixture_root = std::env::current_dir().expect("current directory");
    let workspace = tempfile::tempdir_in(&fixture_root).expect("workspace");
    let outside = tempfile::tempdir_in(&fixture_root).expect("outside");
    let workspace = workspace.path().canonicalize().expect("canonical workspace");
    let outside_marker = outside
        .path()
        .canonicalize()
        .expect("canonical outside")
        .join("outside-pty.marker");
    let mut execution = helper_request(&[
        "write-file",
        outside_marker
            .to_str()
            .expect("temporary path should be UTF-8"),
    ]);
    execution.cwd = workspace.clone();
    execution.capability = CapabilityPolicy {
        cwd_roots: vec![workspace.clone()],
        sandbox: SandboxPolicy::MacSeatbelt {
            write_roots: vec![workspace],
        },
        allow_hand_off: false,
    };

    let supervisor = ProcessSupervisor::new(SupervisorConfig::default());
    let handle = supervisor
        .start(execution)
        .await
        .expect("Seatbelt PTY helper should start");
    let ExecutionOutcome::Exited { code, .. } = wait_for_terminal(&supervisor, &handle).await else {
        panic!("Seatbelt PTY helper must exit");
    };

    assert_ne!(code, Some(0));
    assert!(!outside_marker.exists());
}

#[tokio::test]
async fn running_pty_supports_poll_write_resize_and_cancel() {
    let supervisor = ProcessSupervisor::new(SupervisorConfig::default());
    let handle = start_pty(&supervisor, &["ignore-interrupt"])
        .await
        .expect("interactive PTY helper should start");

    let ready = tokio::time::timeout(Duration::from_secs(2), async {
        let mut cursor = OutputCursor::START;
        loop {
            let result = supervisor
                .poll(
                    &handle.owner,
                    &handle.session_id,
                    cursor,
                    Instant::now() + Duration::from_millis(25),
                )
                .await
                .expect("running PTY poll should succeed");
            match result {
                PollResult::Running { snapshot, output } => {
                    assert_eq!(snapshot.state, ProcessState::Running);
                    assert!(
                        output
                            .chunks
                            .iter()
                            .all(|chunk| chunk.stream == OutputStream::Pty)
                    );
                    if contains_terminal_line(&output.raw_bytes(), b"ready\n") {
                        break output;
                    }
                    cursor = output.next_cursor;
                }
                PollResult::Finished(outcome) => {
                    panic!("interactive PTY helper exited before readiness: {outcome:?}")
                }
            }
        }
    })
    .await
    .expect("interactive PTY helper should publish readiness");
    assert!(contains_terminal_line(&ready.raw_bytes(), b"ready\n"));

    supervisor
        .write(&handle.owner, &handle.session_id, b"interactive input\n")
        .await
        .expect("running PTY should accept input");
    supervisor
        .resize(&handle.owner, &handle.session_id, 132, 43)
        .await
        .expect("running PTY should resize");

    let outcome = tokio::time::timeout(
        Duration::from_secs(5),
        supervisor.cancel(&handle.owner, &handle.session_id),
    )
    .await
    .expect("PTY cancellation must stay within the five-second SLA")
    .expect("PTY cancellation should resolve");
    let ExecutionOutcome::Cancelled { cleanup, .. } = outcome else {
        panic!("PTY cancellation should be terminal Cancelled, got {outcome:?}");
    };
    assert!(cleanup.interrupt_attempted);
    assert!(cleanup.terminate_attempted || cleanup.force_kill_attempted);
    assert!(cleanup.reaped);
}

#[tokio::test]
async fn resize_rejects_zero_dimensions_without_mutating_the_session() {
    let supervisor = ProcessSupervisor::new(SupervisorConfig::default());
    let handle = start_pty(&supervisor, &["sleep", "60000"])
        .await
        .expect("PTY resize helper should start");

    for (cols, rows) in [(0, PTY_ROWS), (PTY_COLS, 0)] {
        let error = supervisor
            .resize(&handle.owner, &handle.session_id, cols, rows)
            .await
            .expect_err("zero PTY dimensions should be rejected");
        assert_eq!(error.code(), "invalid_transport");
    }

    let outcome = supervisor
        .cancel(&handle.owner, &handle.session_id)
        .await
        .expect("PTY resize helper cleanup should resolve");
    assert!(matches!(
        outcome,
        ExecutionOutcome::Cancelled { .. } | ExecutionOutcome::Lost { .. }
    ));
}

#[tokio::test]
async fn resize_after_terminal_close_fails_truthfully() {
    let supervisor = ProcessSupervisor::new(SupervisorConfig::default());
    let handle = start_pty(&supervisor, &["exit", "0"])
        .await
        .expect("PTY resize-after-exit helper should start");

    let outcome = wait_for_terminal(&supervisor, &handle).await;
    assert!(matches!(outcome, ExecutionOutcome::Exited { code: Some(0), .. }));

    let error = supervisor
        .resize(&handle.owner, &handle.session_id, 100, 30)
        .await
        .expect_err("a closed PTY must not report a successful resize");
    assert_eq!(error.code(), "io");
}

#[cfg(unix)]
#[tokio::test]
async fn unix_pty_cancellation_reaps_the_leader_and_grandchild_group() {
    let directory = tempfile::tempdir().expect("temporary directory should be created");
    let marker = directory.path().join("pty-grandchild.pid");
    let supervisor = ProcessSupervisor::new(SupervisorConfig::default());
    let marker_text = marker
        .to_str()
        .expect("temporary path should be representable as UTF-8");
    let handle = start_pty(&supervisor, &["spawn-grandchild", marker_text])
        .await
        .expect("Unix PTY grandchild helper should start");
    let leader = supervisor
        .status(&handle.owner, &handle.session_id)
        .await
        .expect("Unix PTY leader status should be available")
        .pid as libc::pid_t;
    let grandchild = wait_for_unix_pid_marker(&marker).await as libc::pid_t;
    let mut cleanup = UnixProcessCleanup::new([leader, grandchild]);

    let outcome = tokio::time::timeout(
        Duration::from_secs(5),
        supervisor.cancel(&handle.owner, &handle.session_id),
    )
    .await
    .expect("Unix PTY group cancellation must stay bounded")
    .expect("Unix PTY group cancellation should resolve");
    let ExecutionOutcome::Cancelled { cleanup: report, .. } = outcome else {
        panic!("Unix PTY group cancellation should be Cancelled, got {outcome:?}");
    };
    assert!(report.reaped);
    wait_for_unix_processes_gone([leader, grandchild]).await;
    cleanup.disarm();
}

#[cfg(unix)]
async fn wait_for_unix_pid_marker(path: &Path) -> u32 {
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

#[cfg(unix)]
async fn wait_for_unix_processes_gone(pids: impl IntoIterator<Item = libc::pid_t>) {
    let pids = pids.into_iter().collect::<Vec<_>>();
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if pids.iter().all(|pid| !unix_process_exists(*pid)) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("processes still existed after PTY cleanup: {pids:?}"));
}

#[cfg(unix)]
fn unix_process_exists(pid: libc::pid_t) -> bool {
    // SAFETY: signal zero probes liveness without delivering a signal.
    if unsafe { libc::kill(pid, 0) } == 0 {
        return true;
    }
    std::io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH)
}

#[cfg(unix)]
struct UnixProcessCleanup {
    pids: Vec<libc::pid_t>,
    armed: bool,
}

#[cfg(unix)]
impl UnixProcessCleanup {
    fn new(pids: impl IntoIterator<Item = libc::pid_t>) -> Self {
        Self {
            pids: pids.into_iter().collect(),
            armed: true,
        }
    }

    fn disarm(&mut self) {
        self.armed = false;
    }
}

#[cfg(unix)]
impl Drop for UnixProcessCleanup {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        for pid in &self.pids {
            // SAFETY: these exact PIDs were published by this test's helper.
            let _ = unsafe { libc::kill(*pid, libc::SIGKILL) };
        }
    }
}

#[cfg(windows)]
#[tokio::test]
async fn conpty_cancellation_reaps_the_leader_and_grandchild_job() {
    let directory = tempfile::tempdir().expect("temporary directory should be created");
    let marker = directory.path().join("conpty-grandchild.pid");
    let supervisor = ProcessSupervisor::new(SupervisorConfig::default());
    let marker_text = marker
        .to_str()
        .expect("temporary path should be representable as UTF-8");
    let handle = start_pty(&supervisor, &["spawn-grandchild", marker_text])
        .await
        .expect("ConPTY grandchild helper should start");
    let leader_pid = supervisor
        .status(&handle.owner, &handle.session_id)
        .await
        .expect("ConPTY leader status should be available")
        .pid;
    let leader = ExactWindowsProcess::open(leader_pid)
        .expect("ConPTY leader exact process handle should open");
    let grandchild_pid = wait_for_windows_pid_marker(&marker).await;
    let grandchild = ExactWindowsProcess::open(grandchild_pid)
        .expect("ConPTY grandchild exact process handle should open");

    let outcome = tokio::time::timeout(
        Duration::from_secs(5),
        supervisor.cancel(&handle.owner, &handle.session_id),
    )
    .await
    .expect("ConPTY Job cancellation must stay bounded")
    .expect("ConPTY Job cancellation should resolve");
    let ExecutionOutcome::Cancelled { cleanup, .. } = outcome else {
        panic!("ConPTY Job cancellation should be Cancelled, got {outcome:?}");
    };
    assert!(cleanup.reaped);

    leader
        .wait_terminated(Duration::from_secs(2), "ConPTY leader")
        .await;
    grandchild
        .wait_terminated(Duration::from_secs(2), "ConPTY grandchild")
        .await;
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
struct ExactWindowsProcess {
    pid: u32,
    handle: windows_sys::Win32::Foundation::HANDLE,
}

#[cfg(windows)]
impl ExactWindowsProcess {
    fn open(pid: u32) -> std::io::Result<Self> {
        use windows_sys::Win32::{
            Foundation::INVALID_HANDLE_VALUE,
            System::Threading::{
                OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SYNCHRONIZE,
            },
        };

        // SAFETY: OpenProcess returns a fresh non-inheritable handle for the exact PID.
        let handle = unsafe {
            OpenProcess(
                PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_SYNCHRONIZE,
                0,
                pid,
            )
        };
        if handle.is_null() || handle == INVALID_HANDLE_VALUE {
            Err(std::io::Error::last_os_error())
        } else {
            Ok(Self { pid, handle })
        }
    }

    async fn wait_terminated(&self, timeout: Duration, label: &str) {
        use windows_sys::Win32::{
            Foundation::{WAIT_FAILED, WAIT_OBJECT_0, WAIT_TIMEOUT},
            System::Threading::WaitForSingleObject,
        };

        let deadline = Instant::now() + timeout;
        loop {
            // SAFETY: the exact process handle remains live until this wrapper drops.
            match unsafe { WaitForSingleObject(self.handle, 0) } {
                WAIT_OBJECT_0 => return,
                WAIT_TIMEOUT if Instant::now() < deadline => {
                    tokio::time::sleep(Duration::from_millis(5)).await;
                }
                WAIT_TIMEOUT => panic!("{label} pid={} remained alive", self.pid),
                WAIT_FAILED => panic!(
                    "waiting for {label} pid={} failed: {}",
                    self.pid,
                    std::io::Error::last_os_error()
                ),
                result => panic!(
                    "waiting for {label} pid={} returned {result:#x}",
                    self.pid
                ),
            }
        }
    }
}

#[cfg(windows)]
impl Drop for ExactWindowsProcess {
    fn drop(&mut self) {
        // SAFETY: this wrapper uniquely owns the exact process handle.
        let _ = unsafe { windows_sys::Win32::Foundation::CloseHandle(self.handle) };
    }
}

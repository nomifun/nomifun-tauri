#![cfg(any(unix, windows))]

use std::{
    collections::{BTreeMap, BTreeSet},
    ffi::OsString,
    path::Path,
    time::{Duration, Instant},
};

use nomi_execution::{
    CapabilityPolicy, CommandSpec, ExecutionError, ExecutionHandle, ExecutionOutcome,
    ExecutionOwner, ExecutionPolicy, NormalizedExecutionRequest, OutputCursor, PollResult,
    ProcessState, ProcessSupervisor, SessionId, SupervisorConfig, Transport,
};
use uuid::Uuid;

const SHORT_GRACE: Duration = Duration::from_millis(25);

fn helper_binary() -> &'static str {
    env!("CARGO_BIN_EXE_execution_test_helper")
}

fn owner(run_id: Uuid, call_id: Uuid) -> ExecutionOwner {
    ExecutionOwner::new(run_id, call_id)
}

fn helper_request(
    owner: ExecutionOwner,
    args: Vec<OsString>,
    lease: Duration,
) -> NormalizedExecutionRequest {
    let cwd = std::env::current_dir().expect("current directory should exist");
    NormalizedExecutionRequest {
        owner,
        command: CommandSpec::Program {
            program: helper_binary().into(),
            args,
        },
        cwd: cwd.clone(),
        env: BTreeMap::new(),
        transport: Transport::Pipe,
        policy: ExecutionPolicy {
            lease,
            interrupt_grace: SHORT_GRACE,
            terminate_grace: SHORT_GRACE,
            reap_grace: SHORT_GRACE,
            ..ExecutionPolicy::default()
        },
        capability: CapabilityPolicy::local_owner(cwd),
    }
}

fn string_args(args: &[&str]) -> Vec<OsString> {
    args.iter().map(OsString::from).collect()
}

async fn best_effort_cancel(
    supervisor: &ProcessSupervisor,
    handle: &ExecutionHandle,
) {
    let _ = tokio::time::timeout(
        Duration::from_secs(2),
        supervisor.cancel(&handle.owner, &handle.session_id),
    )
    .await;
}

async fn finish_or_cancel(
    supervisor: &ProcessSupervisor,
    handle: &ExecutionHandle,
) {
    let polled = tokio::time::timeout(
        Duration::from_secs(2),
        supervisor.poll(
            &handle.owner,
            &handle.session_id,
            OutputCursor::START,
            Instant::now() + Duration::from_secs(2),
        ),
    )
    .await;
    if !matches!(polled, Ok(Ok(PollResult::Finished(_)))) {
        best_effort_cancel(supervisor, handle).await;
    }
}

async fn wait_for_finished(
    supervisor: &ProcessSupervisor,
    handle: &ExecutionHandle,
) -> ExecutionOutcome {
    let result = tokio::time::timeout(
        Duration::from_secs(2),
        supervisor.poll(
            &handle.owner,
            &handle.session_id,
            OutputCursor::START,
            Instant::now() + Duration::from_secs(2),
        ),
    )
    .await
    .expect("terminal poll must stay bounded")
    .expect("terminal poll should succeed");
    match result {
        PollResult::Finished(outcome) => outcome,
        PollResult::Running { .. } => panic!("helper should have reached a terminal state"),
    }
}

async fn wait_for_marker(path: &Path) {
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if path.is_file() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("helper marker was not published: {}", path.display()));
}

#[test]
fn session_ids_are_unique_uuid_v7_values() {
    let ids = (0..4_096).map(|_| SessionId::new()).collect::<Vec<_>>();
    let unique = ids.iter().copied().collect::<BTreeSet<_>>();

    assert_eq!(unique.len(), ids.len());
    assert!(
        ids.iter()
            .all(|session_id| session_id.as_uuid().get_version_num() == 7)
    );
}

#[tokio::test]
async fn session_actions_require_the_exact_run_and_call_owner() {
    let run_id = Uuid::now_v7();
    let call_id = Uuid::now_v7();
    let exact_owner = owner(run_id, call_id);
    let wrong_run = owner(Uuid::now_v7(), call_id);
    let wrong_call = owner(run_id, Uuid::now_v7());
    let supervisor = ProcessSupervisor::new(SupervisorConfig::default());
    let handle = supervisor
        .start(helper_request(
            exact_owner,
            string_args(&["sleep", "60000"]),
            Duration::from_secs(30),
        ))
        .await
        .expect("long-running helper should start");

    let mut error_codes = Vec::new();
    for wrong_owner in [&wrong_run, &wrong_call] {
        error_codes.push(
            supervisor
                .status(wrong_owner, &handle.session_id)
                .await
                .expect_err("wrong-owner status must be denied")
                .code(),
        );
        error_codes.push(
            supervisor
                .poll(
                    wrong_owner,
                    &handle.session_id,
                    OutputCursor::START,
                    Instant::now(),
                )
                .await
                .expect_err("wrong-owner poll must be denied")
                .code(),
        );
        error_codes.push(
            supervisor
                .write(wrong_owner, &handle.session_id, b"must not be written")
                .await
                .expect_err("wrong-owner write must be denied")
                .code(),
        );
        error_codes.push(
            supervisor
                .cancel(wrong_owner, &handle.session_id)
                .await
                .expect_err("wrong-owner cancel must be denied")
                .code(),
        );
    }

    let state_after_denials = supervisor
        .status(&handle.owner, &handle.session_id)
        .await
        .expect("wrong-owner calls must not remove the exact owner's session")
        .state;
    best_effort_cancel(&supervisor, &handle).await;

    assert!(
        error_codes.iter().all(|code| *code == "owner_mismatch"),
        "all wrong-run and wrong-call actions must fail closed: {error_codes:?}"
    );
    assert_eq!(state_after_denials, ProcessState::Running);
}

#[tokio::test]
async fn status_renews_the_visible_session_activity_timestamp() {
    let supervisor = ProcessSupervisor::new(SupervisorConfig {
        max_sessions: 4,
        reaper_interval: Duration::from_millis(10),
    });
    let handle = supervisor
        .start(helper_request(
            owner(Uuid::now_v7(), Uuid::now_v7()),
            string_args(&["sleep", "60000"]),
            Duration::from_secs(30),
        ))
        .await
        .expect("long-running helper should start");
    let before = supervisor
        .status(&handle.owner, &handle.session_id)
        .await
        .expect("initial status should resolve");

    tokio::time::sleep(Duration::from_millis(40)).await;
    let renewed = supervisor
        .status(&handle.owner, &handle.session_id)
        .await
        .expect("status should renew the session");
    let timestamp_advanced = renewed.last_activity_at > before.last_activity_at;

    best_effort_cancel(&supervisor, &handle).await;
    assert!(
        timestamp_advanced,
        "a successful owner-authenticated status action must renew session activity"
    );
}

#[tokio::test]
async fn process_output_renews_a_session_even_when_nobody_polls_it() {
    let lease = Duration::from_millis(120);
    let supervisor = ProcessSupervisor::new(SupervisorConfig {
        max_sessions: 1,
        reaper_interval: Duration::from_millis(10),
    });
    let handle = supervisor
        .start(helper_request(
            owner(Uuid::now_v7(), Uuid::now_v7()),
            string_args(&["emit-delayed", "8", "40"]),
            lease,
        ))
        .await
        .expect("periodic-output helper should start");

    tokio::time::sleep(Duration::from_millis(260)).await;
    let still_owned = supervisor
        .status(&handle.owner, &handle.session_id)
        .await
        .map(|snapshot| snapshot.state);

    finish_or_cancel(&supervisor, &handle).await;
    assert!(
        matches!(still_owned, Ok(ProcessState::Running | ProcessState::Exited)),
        "process output must renew the exact session without a poll: {still_owned:?}"
    );
}

#[tokio::test]
async fn poll_write_and_status_keep_sessions_alive_past_the_original_lease() {
    let run_id = Uuid::now_v7();
    let lease = Duration::from_millis(600);
    let supervisor = ProcessSupervisor::new(SupervisorConfig {
        max_sessions: 3,
        reaper_interval: Duration::from_millis(10),
    });
    let status_handle = supervisor
        .start(helper_request(
            owner(run_id, Uuid::now_v7()),
            string_args(&["sleep", "60000"]),
            lease,
        ))
        .await
        .expect("status-renewal helper should start");
    let poll_handle = supervisor
        .start(helper_request(
            owner(run_id, Uuid::now_v7()),
            string_args(&["sleep", "60000"]),
            lease,
        ))
        .await
        .expect("poll-renewal helper should start");
    let write_handle = supervisor
        .start(helper_request(
            owner(run_id, Uuid::now_v7()),
            string_args(&["echo-stdin"]),
            lease,
        ))
        .await
        .expect("write-renewal helper should start");

    tokio::time::sleep(Duration::from_millis(250)).await;
    supervisor
        .status(&status_handle.owner, &status_handle.session_id)
        .await
        .expect("status should renew its session");
    let poll_result = supervisor
        .poll(
            &poll_handle.owner,
            &poll_handle.session_id,
            OutputCursor::START,
            Instant::now(),
        )
        .await
        .expect("poll should renew its session");
    assert!(matches!(poll_result, PollResult::Running { .. }));
    supervisor
        .write(&write_handle.owner, &write_handle.session_id, b"still active")
        .await
        .expect("write should renew its session");

    // This crosses the original lease while remaining before the renewed one.
    tokio::time::sleep(Duration::from_millis(450)).await;
    let states = [
        supervisor
            .status(&status_handle.owner, &status_handle.session_id)
            .await
            .map(|snapshot| snapshot.state),
        supervisor
            .status(&poll_handle.owner, &poll_handle.session_id)
            .await
            .map(|snapshot| snapshot.state),
        supervisor
            .status(&write_handle.owner, &write_handle.session_id)
            .await
            .map(|snapshot| snapshot.state),
    ];

    for handle in [&status_handle, &poll_handle, &write_handle] {
        best_effort_cancel(&supervisor, handle).await;
    }

    assert!(
        states
            .iter()
            .all(|state| matches!(state, Ok(ProcessState::Running))),
        "poll, write, and status must each protect a live session past its original lease: \
         {states:?}"
    );
}

#[tokio::test]
async fn capacity_is_reserved_before_a_second_process_can_execute() {
    let directory = tempfile::tempdir().expect("temporary directory should be created");
    let marker = directory.path().join("capacity-overflow.pid");
    let supervisor = ProcessSupervisor::new(SupervisorConfig {
        max_sessions: 1,
        reaper_interval: Duration::from_secs(30),
    });
    let first = supervisor
        .start(helper_request(
            owner(Uuid::now_v7(), Uuid::now_v7()),
            string_args(&["sleep", "60000"]),
            Duration::from_secs(30),
        ))
        .await
        .expect("the first session should consume the only capacity slot");

    let second = tokio::time::timeout(
        Duration::from_secs(2),
        supervisor.start(helper_request(
            owner(Uuid::now_v7(), Uuid::now_v7()),
            vec![
                OsString::from("write-pid"),
                marker.as_os_str().to_owned(),
            ],
            Duration::from_secs(30),
        )),
    )
    .await
    .expect("capacity rejection must not wait for process startup");

    let error_code = match second {
        Ok(handle) => {
            finish_or_cancel(&supervisor, &handle).await;
            None
        }
        Err(error) => Some(error.code()),
    };
    let marker_exists = marker.exists();
    best_effort_cancel(&supervisor, &first).await;

    assert!(
        error_code == Some("capacity_exhausted") && !marker_exists,
        "capacity must be reserved before spawn: error={error_code:?}, \
         helper_executed={marker_exists}"
    );
}

#[tokio::test]
async fn an_expired_session_is_cancelled_reaped_and_removed() {
    let supervisor = ProcessSupervisor::new(SupervisorConfig {
        max_sessions: 4,
        reaper_interval: Duration::from_millis(10),
    });
    let handle = supervisor
        .start(helper_request(
            owner(Uuid::now_v7(), Uuid::now_v7()),
            string_args(&["sleep", "60000"]),
            Duration::from_millis(120),
        ))
        .await
        .expect("expiring helper should start");
    let pid = supervisor
        .status(&handle.owner, &handle.session_id)
        .await
        .expect("expiring helper should initially be registered")
        .pid;
    let process = ProcessProbe::new(pid);

    // Do not call any session action during this interval: every valid action is
    // itself owner activity and should renew the lease.
    tokio::time::sleep(Duration::from_millis(500)).await;
    let lookup_after_idle = supervisor.status(&handle.owner, &handle.session_id).await;
    let removed = matches!(
        lookup_after_idle,
        Err(ExecutionError::SessionNotFound { .. })
    );
    let reaped = process.is_gone();

    if !removed {
        best_effort_cancel(&supervisor, &handle).await;
    }
    if !reaped {
        process.force_kill();
    }

    assert!(
        removed && reaped,
        "lease expiry must run supervised cancellation to exact reap before forgetting the \
         session: removed={removed}, reaped={reaped}"
    );
}

#[tokio::test]
async fn run_heartbeat_renews_every_call_for_that_run_only() {
    let protected_run = Uuid::now_v7();
    let unprotected_run = Uuid::now_v7();
    let lease = Duration::from_millis(800);
    let supervisor = ProcessSupervisor::new(SupervisorConfig {
        max_sessions: 3,
        reaper_interval: Duration::from_millis(10),
    });
    let first = supervisor
        .start(helper_request(
            owner(protected_run, Uuid::now_v7()),
            string_args(&["sleep", "60000"]),
            lease,
        ))
        .await
        .expect("first protected helper should start");
    let second = supervisor
        .start(helper_request(
            owner(protected_run, Uuid::now_v7()),
            string_args(&["sleep", "60000"]),
            lease,
        ))
        .await
        .expect("second protected helper should start");
    let unprotected = supervisor
        .start(helper_request(
            owner(unprotected_run, Uuid::now_v7()),
            string_args(&["sleep", "60000"]),
            lease,
        ))
        .await
        .expect("unprotected helper should start");
    let unprotected_pid = supervisor
        .status(&unprotected.owner, &unprotected.session_id)
        .await
        .expect("unprotected helper status should resolve")
        .pid;
    let unprotected_process = ProcessProbe::new(unprotected_pid);

    tokio::time::sleep(Duration::from_millis(500)).await;
    let renewed = supervisor.heartbeat(protected_run);
    tokio::time::sleep(Duration::from_millis(450)).await;

    let first_state = supervisor
        .status(&first.owner, &first.session_id)
        .await
        .map(|snapshot| snapshot.state);
    let second_state = supervisor
        .status(&second.owner, &second.session_id)
        .await
        .map(|snapshot| snapshot.state);
    let unprotected_lookup = supervisor
        .status(&unprotected.owner, &unprotected.session_id)
        .await;
    let unprotected_removed = matches!(
        unprotected_lookup,
        Err(ExecutionError::SessionNotFound { .. })
    );
    let unprotected_reaped = unprotected_process.is_gone();

    for handle in [&first, &second] {
        best_effort_cancel(&supervisor, handle).await;
    }
    if !unprotected_removed {
        best_effort_cancel(&supervisor, &unprotected).await;
    }
    if !unprotected_reaped {
        unprotected_process.force_kill();
    }

    assert_eq!(renewed, 2, "heartbeat should renew both calls in one run");
    assert!(matches!(first_state, Ok(ProcessState::Running)));
    assert!(matches!(second_state, Ok(ProcessState::Running)));
    assert!(
        unprotected_removed && unprotected_reaped,
        "a heartbeat must not protect another run: removed={unprotected_removed}, \
         reaped={unprotected_reaped}"
    );
}

#[tokio::test]
async fn concurrent_starts_never_execute_more_than_the_reserved_capacity() {
    const ATTEMPTS: usize = 8;
    const CAPACITY: usize = 2;

    let directory = tempfile::tempdir().expect("temporary directory should be created");
    let markers = (0..ATTEMPTS)
        .map(|index| directory.path().join(format!("concurrent-{index}.pid")))
        .collect::<Vec<_>>();
    let supervisor = ProcessSupervisor::new(SupervisorConfig {
        max_sessions: CAPACITY,
        reaper_interval: Duration::from_secs(30),
    });
    let mut starts = Vec::new();
    for (index, marker) in markers.iter().cloned().enumerate() {
        let supervisor = supervisor.clone();
        starts.push(tokio::spawn(async move {
            let request = helper_request(
                owner(Uuid::now_v7(), Uuid::now_v7()),
                vec![
                    OsString::from("spawn-grandchild"),
                    marker.as_os_str().to_owned(),
                ],
                Duration::from_secs(30),
            );
            (index, supervisor.start(request).await)
        }));
    }

    let mut started = Vec::new();
    let mut rejected = Vec::new();
    for start in starts {
        let (index, result) = start.await.expect("start task should join");
        match result {
            Ok(handle) => started.push((index, handle)),
            Err(error) => rejected.push((index, error.code())),
        }
    }
    for (index, _) in &started {
        wait_for_marker(&markers[*index]).await;
    }
    // A wrongly spawned process that reports a late capacity error still gets
    // a chance to publish its marker before the absence assertion.
    tokio::time::sleep(Duration::from_millis(250)).await;
    let rejected_helpers_never_ran = rejected
        .iter()
        .all(|(index, _)| !markers[*index].exists());

    for (_, handle) in &started {
        best_effort_cancel(&supervisor, handle).await;
    }

    assert_eq!(
        started.len(),
        CAPACITY,
        "exactly the reserved capacity should start"
    );
    assert_eq!(rejected.len(), ATTEMPTS - CAPACITY);
    assert!(
        rejected
            .iter()
            .all(|(_, code)| *code == "capacity_exhausted"),
        "overflow starts must use the stable capacity error: {rejected:?}"
    );
    assert!(
        rejected_helpers_never_ran,
        "a capacity-rejected helper executed user code"
    );
}

#[tokio::test]
async fn a_failed_spawn_releases_its_capacity_reservation() {
    let directory = tempfile::tempdir().expect("temporary directory should be created");
    let supervisor = ProcessSupervisor::new(SupervisorConfig {
        max_sessions: 1,
        reaper_interval: Duration::from_secs(30),
    });
    let mut invalid = helper_request(
        owner(Uuid::now_v7(), Uuid::now_v7()),
        Vec::new(),
        Duration::from_secs(30),
    );
    invalid.command = CommandSpec::Program {
        program: directory
            .path()
            .join("definitely-missing-executable")
            .into_os_string(),
        args: Vec::new(),
    };

    let failed = supervisor
        .start(invalid)
        .await
        .expect_err("missing executable should fail before a session is returned");
    let valid = supervisor
        .start(helper_request(
            owner(Uuid::now_v7(), Uuid::now_v7()),
            string_args(&["sleep", "60000"]),
            Duration::from_secs(30),
        ))
        .await;

    if let Ok(handle) = &valid {
        best_effort_cancel(&supervisor, handle).await;
    }

    assert_eq!(failed.code(), "spawn_failed");
    assert!(
        valid.is_ok(),
        "failed spawn leaked the only capacity reservation: {valid:?}"
    );
}

#[tokio::test]
async fn capacity_pressure_evicts_a_finished_session_before_an_active_one() {
    let supervisor = ProcessSupervisor::new(SupervisorConfig {
        max_sessions: 2,
        reaper_interval: Duration::from_secs(30),
    });
    let finished = supervisor
        .start(helper_request(
            owner(Uuid::now_v7(), Uuid::now_v7()),
            string_args(&["exit", "0"]),
            Duration::from_secs(30),
        ))
        .await
        .expect("quick helper should start");
    let outcome = wait_for_finished(&supervisor, &finished).await;
    assert!(matches!(
        outcome,
        ExecutionOutcome::Exited { code: Some(0), .. }
    ));
    let protected = supervisor
        .start(helper_request(
            owner(Uuid::now_v7(), Uuid::now_v7()),
            string_args(&["sleep", "60000"]),
            Duration::from_secs(30),
        ))
        .await
        .expect("active protected helper should start");

    let newcomer = supervisor
        .start(helper_request(
            owner(Uuid::now_v7(), Uuid::now_v7()),
            string_args(&["sleep", "60000"]),
            Duration::from_secs(30),
        ))
        .await;
    let finished_lookup = supervisor
        .status(&finished.owner, &finished.session_id)
        .await;
    let protected_state = supervisor
        .status(&protected.owner, &protected.session_id)
        .await
        .map(|snapshot| snapshot.state);

    if let Ok(handle) = &newcomer {
        best_effort_cancel(&supervisor, handle).await;
    }
    best_effort_cancel(&supervisor, &protected).await;

    assert!(
        newcomer.is_ok(),
        "finished capacity should be reclaimed for a new session: {newcomer:?}"
    );
    assert!(matches!(
        finished_lookup,
        Err(ExecutionError::SessionNotFound { .. })
    ));
    assert!(
        matches!(protected_state, Ok(ProcessState::Running)),
        "capacity pressure silently evicted the active session: {protected_state:?}"
    );
}

#[tokio::test]
async fn shutdown_closes_the_start_gate_and_reports_every_active_session() {
    let supervisor = ProcessSupervisor::new(SupervisorConfig {
        max_sessions: 4,
        reaper_interval: Duration::from_secs(30),
    });
    let first = supervisor
        .start(helper_request(
            owner(Uuid::now_v7(), Uuid::now_v7()),
            string_args(&["sleep", "60000"]),
            Duration::from_secs(30),
        ))
        .await
        .expect("first shutdown helper should start");
    let second = supervisor
        .start(helper_request(
            owner(Uuid::now_v7(), Uuid::now_v7()),
            string_args(&["sleep", "60000"]),
            Duration::from_secs(30),
        ))
        .await
        .expect("second shutdown helper should start");
    let first_process = ProcessProbe::new(
        supervisor
            .status(&first.owner, &first.session_id)
            .await
            .expect("first shutdown helper status should resolve")
            .pid,
    );
    let second_process = ProcessProbe::new(
        supervisor
            .status(&second.owner, &second.session_id)
            .await
            .expect("second shutdown helper status should resolve")
            .pid,
    );

    let report = tokio::time::timeout(Duration::from_secs(6), supervisor.shutdown())
        .await
        .expect("supervisor shutdown must stay bounded");
    let expected = BTreeMap::from([
        (first.session_id, first.owner.clone()),
        (second.session_id, second.owner.clone()),
    ]);
    let mut reported = BTreeMap::new();
    let all_terminal = report.sessions.iter().all(|session| {
        reported.insert(session.session_id, session.owner.clone());
        matches!(
            &session.outcome,
            ExecutionOutcome::Cancelled { .. } | ExecutionOutcome::Lost { .. }
        )
    });

    let directory = tempfile::tempdir().expect("temporary directory should be created");
    let marker = directory.path().join("post-shutdown.pid");
    let post_shutdown = supervisor
        .start(helper_request(
            owner(Uuid::now_v7(), Uuid::now_v7()),
            vec![
                OsString::from("write-pid"),
                marker.as_os_str().to_owned(),
            ],
            Duration::from_secs(30),
        ))
        .await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    let rejected_before_spawn = matches!(
        &post_shutdown,
        Err(error) if error.code() == "supervisor_shutting_down"
    ) && !marker.exists();

    assert_eq!(reported, expected);
    assert!(
        all_terminal,
        "shutdown report contained a non-Cancelled/Lost active session: {:?}",
        report.sessions
    );
    assert!(first_process.is_gone());
    assert!(second_process.is_gone());
    assert!(
        rejected_before_spawn,
        "post-shutdown start was not rejected before user code: {post_shutdown:?}"
    );
}

#[tokio::test]
async fn shutdown_omits_already_finished_sessions_from_its_active_cleanup_report() {
    let supervisor = ProcessSupervisor::new(SupervisorConfig::default());
    let finished = supervisor
        .start(helper_request(
            owner(Uuid::now_v7(), Uuid::now_v7()),
            string_args(&["exit", "0"]),
            Duration::from_secs(30),
        ))
        .await
        .expect("quick helper should start");
    let outcome = wait_for_finished(&supervisor, &finished).await;
    assert!(matches!(
        outcome,
        ExecutionOutcome::Exited { code: Some(0), .. }
    ));

    let report = supervisor.shutdown().await;

    assert!(
        report.sessions.is_empty(),
        "shutdown cleanup report must contain only sessions that required shutdown: {:?}",
        report.sessions
    );
}

#[cfg(unix)]
struct ProcessProbe {
    pid: libc::pid_t,
}

#[cfg(unix)]
impl ProcessProbe {
    fn new(pid: u32) -> Self {
        Self {
            pid: pid as libc::pid_t,
        }
    }

    fn is_gone(&self) -> bool {
        // SAFETY: signal zero probes this test helper without delivering a signal.
        if unsafe { libc::kill(self.pid, 0) } == 0 {
            return false;
        }
        std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH)
    }

    fn force_kill(&self) {
        // SAFETY: the PID was returned for this test's directly owned helper.
        let _ = unsafe { libc::kill(self.pid, libc::SIGKILL) };
    }
}

#[cfg(unix)]
impl Drop for ProcessProbe {
    fn drop(&mut self) {
        if !self.is_gone() {
            self.force_kill();
        }
    }
}

#[cfg(windows)]
struct ProcessProbe {
    handle: windows_sys::Win32::Foundation::HANDLE,
}

#[cfg(windows)]
impl ProcessProbe {
    fn new(pid: u32) -> Self {
        use windows_sys::Win32::System::Threading::{
            OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SYNCHRONIZE, PROCESS_TERMINATE,
        };

        // SAFETY: OpenProcess creates a non-inheritable handle to the exact helper process.
        let handle = unsafe {
            OpenProcess(
                PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_SYNCHRONIZE | PROCESS_TERMINATE,
                0,
                pid,
            )
        };
        assert!(
            !handle.is_null(),
            "the exact helper process handle should open: {}",
            std::io::Error::last_os_error()
        );
        Self { handle }
    }

    fn is_gone(&self) -> bool {
        use windows_sys::Win32::Foundation::{WAIT_OBJECT_0, WAIT_TIMEOUT};
        use windows_sys::Win32::System::Threading::WaitForSingleObject;

        // SAFETY: the exact process handle remains owned by this probe.
        match unsafe { WaitForSingleObject(self.handle, 0) } {
            WAIT_OBJECT_0 => true,
            WAIT_TIMEOUT => false,
            _ => false,
        }
    }

    fn force_kill(&self) {
        use windows_sys::Win32::System::Threading::TerminateProcess;

        // SAFETY: the handle names this test's exact helper process and has terminate access.
        let _ = unsafe { TerminateProcess(self.handle, 1) };
    }
}

#[cfg(windows)]
impl Drop for ProcessProbe {
    fn drop(&mut self) {
        use windows_sys::Win32::Foundation::CloseHandle;

        if !self.is_gone() {
            self.force_kill();
        }
        // SAFETY: this probe owns one valid process handle and closes it exactly once.
        let _ = unsafe { CloseHandle(self.handle) };
    }
}

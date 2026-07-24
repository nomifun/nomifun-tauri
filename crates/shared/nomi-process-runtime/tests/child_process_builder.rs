use std::{
    ffi::OsString,
    path::Path,
    process::Stdio,
    time::Duration,
};

use nomi_process_runtime::{ChildProcessBuilder, kill_process_tree};

#[cfg(unix)]
mod unix {
    use super::*;
    use std::time::Instant;

    #[test]
    fn invalid_working_directory_fails_without_a_fresh_setup_timeout() {
        let directory = tempfile::tempdir().expect("temporary directory should be created");
        let missing = directory.path().join("missing");
        let mut builder = ChildProcessBuilder::new(env!("CARGO_BIN_EXE_process_test_helper"));
        builder.arg("exit-code").arg("0").current_dir(&missing);

        let started = Instant::now();
        let error = builder
            .spawn()
            .expect_err("an invalid working directory must fail");

        assert!(
            started.elapsed() < Duration::from_secs(1),
            "spawn failure waited for a new setup timeout: {:?}",
            started.elapsed()
        );
        assert_eq!(error.kind(), std::io::ErrorKind::NotFound);
    }

    #[test]
    fn exec_failure_after_registration_accepts_the_expected_std_reap() {
        let directory = tempfile::tempdir().expect("temporary directory should be created");
        let missing_program = directory.path().join("missing-program");
        let builder = ChildProcessBuilder::new(&missing_program);

        let error = builder
            .spawn()
            .expect_err("a missing executable must fail after pre_exec registration");
        let message = error.to_string();

        assert_eq!(error.kind(), std::io::ErrorKind::NotFound);
        assert!(!message.contains("reap child process"), "{message}");
        assert!(!message.contains("cached PGID quarantined"), "{message}");
    }

    #[tokio::test]
    async fn explicit_kill_reaps_the_child_and_grandchild_group() {
        let directory = tempfile::tempdir().expect("temporary directory should be created");
        let marker = directory.path().join("child-process-grandchild.pid");
        let mut builder = ChildProcessBuilder::new(env!("CARGO_BIN_EXE_process_test_helper"));
        builder
            .args([
                OsString::from("spawn-grandchild"),
                marker.as_os_str().to_owned(),
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        let mut child = builder.spawn().expect("child-process helper should start");
        let leader = child.id().expect("child-process helper should have a PID");
        let grandchild = wait_for_pid_marker(&marker).await;

        tokio::time::timeout(
            Duration::from_secs(6),
            kill_process_tree(&mut child),
        )
        .await
        .expect("child-process Unix cleanup should remain bounded")
        .expect("child-process Unix cleanup should succeed");

        assert_process_gone(leader);
        assert_process_gone(grandchild);
    }

    #[tokio::test]
    async fn natural_leader_exit_reaps_the_remaining_group_descendant() {
        let directory = tempfile::tempdir().expect("temporary directory should be created");
        let marker = directory.path().join("child-process-leader-first-grandchild.pid");
        let mut builder = ChildProcessBuilder::new(env!("CARGO_BIN_EXE_process_test_helper"));
        builder
            .args([
                OsString::from("leader-first"),
                marker.as_os_str().to_owned(),
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        let (mut child, cleanup) = builder
            .spawn_with_cleanup()
            .expect("child-process leader-first helper should start");
        let grandchild = wait_for_pid_marker(&marker).await;
        let status = child.wait().await.expect("child-process leader should be waitable");
        assert!(status.success());

        tokio::time::timeout(Duration::from_secs(6), cleanup.wait())
            .await
            .expect("Unix watchdog cleanup proof should remain bounded")
            .expect("Unix watchdog must prove the leader-first group empty");
        assert_process_gone(grandchild);
    }

    #[tokio::test]
    async fn output_waits_for_leader_first_descendant_cleanup() {
        let directory = tempfile::tempdir().expect("temporary directory should be created");
        let marker = directory.path().join("child-output-leader-first-grandchild.pid");
        let mut builder = ChildProcessBuilder::new(env!("CARGO_BIN_EXE_process_test_helper"));
        builder.args([
            OsString::from("leader-first"),
            marker.as_os_str().to_owned(),
        ]);

        let output = builder
            .output()
            .await
            .expect("output must include exact descendant cleanup");
        let grandchild = read_pid_marker_any_unix(&marker);

        assert!(output.status.success());
        assert_process_gone(grandchild);
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn cancelling_output_retains_cleanup_until_the_group_is_gone() {
        let directory = tempfile::tempdir().expect("temporary directory should be created");
        let marker = directory.path().join("cancelled-child-output-grandchild.pid");
        let mut builder = ChildProcessBuilder::new(env!("CARGO_BIN_EXE_process_test_helper"));
        builder.args([
            OsString::from("spawn-grandchild"),
            marker.as_os_str().to_owned(),
        ]);
        let output = tokio::spawn(builder.output());
        let grandchild = wait_for_pid_marker(&marker).await;

        output.abort();
        let _ = output.await;
        wait_for_process_gone(grandchild).await;
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    #[serial_test::serial]
    async fn abrupt_host_exit_reaps_the_child_and_grandchild_group() {
        let directory = tempfile::tempdir().expect("temporary directory should be created");
        let leader_marker = directory.path().join("child-process-parent-death-leader.pid");
        let grandchild_marker = directory
            .path()
            .join("child-process-parent-death-grandchild.pid");
        let status = std::process::Command::new(env!(
            "CARGO_BIN_EXE_child_parent_death_harness"
        ))
        .arg(env!("CARGO_BIN_EXE_process_test_helper"))
        .arg(&leader_marker)
        .arg(&grandchild_marker)
        .status()
        .expect("child-process parent-death harness should start");
        assert!(
            status.success(),
            "child-process parent-death harness failed before deliberate exit: {status:?}"
        );
        let leader = read_pid_marker(&leader_marker);
        let grandchild = read_pid_marker(&grandchild_marker);

        wait_for_process_gone(leader).await;
        wait_for_process_gone(grandchild).await;
    }

    async fn wait_for_pid_marker(path: &Path) -> u32 {
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if let Ok(contents) = std::fs::read_to_string(path)
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

    #[cfg(target_os = "linux")]
    fn read_pid_marker(path: &Path) -> u32 {
        std::fs::read_to_string(path)
            .unwrap_or_else(|error| panic!("PID marker should be readable at {}: {error}", path.display()))
            .trim()
            .parse()
            .unwrap_or_else(|error| panic!("PID marker should contain a PID at {}: {error}", path.display()))
    }

    async fn wait_for_process_gone(pid: u32) {
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if process_gone(pid) {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap_or_else(|_| panic!("PID {pid} did not disappear"));
    }

    fn assert_process_gone(pid: u32) {
        assert!(process_gone(pid), "PID {pid} should be gone");
    }

    fn read_pid_marker_any_unix(path: &Path) -> u32 {
        std::fs::read_to_string(path)
            .unwrap_or_else(|error| {
                panic!("PID marker should be readable at {}: {error}", path.display())
            })
            .trim()
            .parse()
            .unwrap_or_else(|error| {
                panic!("PID marker should contain a PID at {}: {error}", path.display())
            })
    }

    fn process_gone(pid: u32) -> bool {
        // SAFETY: signal zero only probes the exact test-helper PID.
        if unsafe { libc::kill(pid as libc::pid_t, 0) } == 0 {
            return false;
        }
        std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH)
    }
}

#[cfg(windows)]
mod windows {
    use super::*;
    use std::{io, time::Instant};

    use windows_sys::Win32::{
        Foundation::{CloseHandle, HANDLE, WAIT_OBJECT_0, WAIT_TIMEOUT},
        Storage::FileSystem::SYNCHRONIZE,
        System::Threading::{
            OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION, WaitForSingleObject,
        },
    };

    #[tokio::test]
    async fn explicit_kill_reaps_the_child_and_grandchild_job() {
        let directory = tempfile::tempdir().expect("temporary directory should be created");
        let marker = directory.path().join("child-process-grandchild.pid");
        let mut builder = ChildProcessBuilder::new(env!("CARGO_BIN_EXE_process_test_helper"));
        builder
            .args([
                OsString::from("spawn-grandchild"),
                marker.as_os_str().to_owned(),
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        let mut child = builder.spawn().expect("child-process helper should start");
        let leader_pid = child.id().expect("child-process helper should have a PID");
        let leader = ExactProcess::open(leader_pid).expect("leader process should open");
        let grandchild_pid = wait_for_pid_marker(&marker).await;
        let grandchild =
            ExactProcess::open(grandchild_pid).expect("grandchild process should open");

        tokio::time::timeout(
            Duration::from_secs(6),
            kill_process_tree(&mut child),
        )
        .await
        .expect("child-process Job cleanup should remain bounded")
        .expect("child-process Job cleanup should succeed");

        leader
            .wait_terminated(Duration::from_secs(2), "child-process leader")
            .await;
        grandchild
            .wait_terminated(Duration::from_secs(2), "child-process descendant")
            .await;
    }

    #[tokio::test]
    async fn natural_leader_exit_reaps_the_remaining_job_descendant() {
        let directory = tempfile::tempdir().expect("temporary directory should be created");
        let marker = directory.path().join("child-process-leader-first-grandchild.pid");
        let exit_gate = directory.path().join("child-process-leader-exit.gate");
        let mut builder = ChildProcessBuilder::new(env!("CARGO_BIN_EXE_process_test_helper"));
        builder
            .args([
                OsString::from("leader-first-gated"),
                marker.as_os_str().to_owned(),
                exit_gate.as_os_str().to_owned(),
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        let (mut child, cleanup) = builder
            .spawn_with_cleanup()
            .expect("child-process leader-first helper should start");
        let grandchild_pid = wait_for_pid_marker(&marker).await;
        let grandchild =
            ExactProcess::open(grandchild_pid).expect("grandchild process should open");
        std::fs::write(&exit_gate, b"go").expect("leader exit gate should be published");

        let status = tokio::time::timeout(Duration::from_secs(5), child.wait())
            .await
            .expect("child-process leader should exit promptly")
            .expect("child-process leader should be waitable");

        assert!(status.success());
        tokio::time::timeout(Duration::from_secs(6), cleanup.wait())
            .await
            .expect("Windows Job cleanup proof should remain bounded")
            .expect("Windows Job must prove the leader-first tree empty");
        grandchild
            .wait_terminated(Duration::ZERO, "child-process leader-first grandchild")
            .await;
    }

    #[tokio::test]
    async fn output_waits_for_leader_first_descendant_cleanup() {
        let directory = tempfile::tempdir().expect("temporary directory should be created");
        let marker = directory.path().join("child-output-leader-first-grandchild.pid");
        let mut builder = ChildProcessBuilder::new(env!("CARGO_BIN_EXE_process_test_helper"));
        builder.args([
            OsString::from("leader-first"),
            marker.as_os_str().to_owned(),
        ]);

        let output = builder
            .output()
            .await
            .expect("output must include exact descendant cleanup");
        let grandchild_pid = read_pid_marker(&marker);

        assert!(output.status.success());
        match ExactProcess::open(grandchild_pid) {
            Ok(grandchild) => {
                grandchild
                    .wait_terminated(Duration::ZERO, "leader-first output descendant")
                    .await;
            }
            Err(error) => assert_eq!(
                error.raw_os_error(),
                Some(87),
                "opening a cleaned descendant should fail only because the PID is gone"
            ),
        }
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn cancelling_output_retains_cleanup_until_the_job_is_empty() {
        let directory = tempfile::tempdir().expect("temporary directory should be created");
        let marker = directory.path().join("cancelled-child-output-grandchild.pid");
        let mut builder = ChildProcessBuilder::new(env!("CARGO_BIN_EXE_process_test_helper"));
        builder.args([
            OsString::from("spawn-grandchild"),
            marker.as_os_str().to_owned(),
        ]);
        let output = tokio::spawn(builder.output());
        let grandchild_pid = wait_for_pid_marker(&marker).await;
        let grandchild =
            ExactProcess::open(grandchild_pid).expect("grandchild process should open");

        output.abort();
        let _ = output.await;
        grandchild
            .wait_terminated(Duration::from_secs(8), "cancelled output descendant")
            .await;
    }

    #[tokio::test]
    async fn abrupt_host_exit_closes_the_child_and_grandchild_job() {
        let directory = tempfile::tempdir().expect("temporary directory should be created");
        let leader_marker = directory.path().join("child-process-parent-death-leader.pid");
        let grandchild_marker = directory
            .path()
            .join("child-process-parent-death-grandchild.pid");
        let exit_gate = directory.path().join("child-process-parent-death-exit.gate");
        let mut harness = std::process::Command::new(env!(
            "CARGO_BIN_EXE_child_parent_death_harness"
        ))
        .arg(env!("CARGO_BIN_EXE_process_test_helper"))
        .arg(&leader_marker)
        .arg(&grandchild_marker)
        .arg(&exit_gate)
        .spawn()
        .expect("child-process parent-death harness should start");
        let leader_pid = wait_for_pid_marker(&leader_marker).await;
        let grandchild_pid = wait_for_pid_marker(&grandchild_marker).await;
        let leader = ExactProcess::open(leader_pid).expect("leader process should open");
        let grandchild =
            ExactProcess::open(grandchild_pid).expect("grandchild process should open");
        std::fs::write(&exit_gate, b"go").expect("exit gate should be published");
        let status = harness
            .wait()
            .expect("child-process parent-death harness should reap");
        assert!(
            status.success(),
            "child-process parent-death harness failed before deliberate exit: {status:?}"
        );

        leader
            .wait_terminated(Duration::from_secs(5), "child-process parent-death leader")
            .await;
        grandchild
            .wait_terminated(Duration::from_secs(5), "child-process parent-death grandchild")
            .await;
    }

    async fn wait_for_pid_marker(path: &Path) -> u32 {
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if let Ok(contents) = std::fs::read_to_string(path)
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

    fn read_pid_marker(path: &Path) -> u32 {
        std::fs::read_to_string(path)
            .unwrap_or_else(|error| {
                panic!("PID marker should be readable at {}: {error}", path.display())
            })
            .trim()
            .parse()
            .unwrap_or_else(|error| {
                panic!("PID marker should contain a PID at {}: {error}", path.display())
            })
    }

    struct ExactProcess(HANDLE);

    impl ExactProcess {
        fn open(pid: u32) -> io::Result<Self> {
            // SAFETY: OpenProcess validates the PID and returns a fresh handle.
            let handle = unsafe {
                OpenProcess(
                    SYNCHRONIZE | PROCESS_QUERY_LIMITED_INFORMATION,
                    0,
                    pid,
                )
            };
            if handle.is_null() {
                Err(io::Error::last_os_error())
            } else {
                Ok(Self(handle))
            }
        }

        async fn wait_terminated(&self, timeout: Duration, label: &str) {
            let handle = self.0 as usize;
            let result = tokio::task::spawn_blocking(move || {
                // SAFETY: the exact process handle remains owned by `self`
                // until this worker joins.
                unsafe {
                    WaitForSingleObject(
                        handle as HANDLE,
                        u32::try_from(timeout.as_millis()).unwrap_or(u32::MAX),
                    )
                }
            })
            .await
            .expect("exact-process wait worker should join");
            assert_eq!(
                result, WAIT_OBJECT_0,
                "{label} did not terminate before {:?}; wait result={result}",
                Instant::now()
            );
            assert_ne!(result, WAIT_TIMEOUT);
        }
    }

    impl Drop for ExactProcess {
        fn drop(&mut self) {
            // SAFETY: this wrapper owns exactly one valid handle.
            let _ = unsafe { CloseHandle(self.0) };
        }
    }
}

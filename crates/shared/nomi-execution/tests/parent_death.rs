#![cfg(any(target_os = "linux", target_os = "macos", windows))]

#[cfg(target_os = "linux")]
use std::{
    collections::{BTreeMap, BTreeSet},
    fs, io,
    path::Path,
    process::{Child, Command, ExitStatus},
    time::Duration,
};

fn helper_binary() -> &'static str {
    env!("CARGO_BIN_EXE_execution_test_helper")
}

fn harness_binary() -> &'static str {
    env!("CARGO_BIN_EXE_parent_death_harness")
}

#[cfg(target_os = "linux")]
#[tokio::test]
#[serial_test::serial]
async fn abrupt_harness_exit_kills_and_reaps_the_owned_process_group() {
    assert_abrupt_harness_exit_kills_owned_group(false).await;
}

#[cfg(target_os = "linux")]
#[tokio::test]
#[serial_test::serial]
async fn abrupt_harness_exit_kills_and_reaps_the_owned_pty_session() {
    assert_abrupt_harness_exit_kills_owned_group(true).await;
}

#[cfg(target_os = "linux")]
async fn assert_abrupt_harness_exit_kills_owned_group(pty: bool) {
    let _subreaper = SubreaperGuard::install().expect("test process should become a subreaper");
    let baseline_children =
        direct_children().expect("baseline direct-child identities should be readable");
    let directory = tempfile::tempdir().expect("temporary directory should be created");
    let leader_marker = directory.path().join("leader.pid");
    let grandchild_marker = directory.path().join("grandchild.pid");
    let mut command = Command::new(harness_binary());
    command
        .arg(helper_binary())
        .arg(&leader_marker)
        .arg(&grandchild_marker);
    if pty {
        command.arg("pty");
    }
    let child = command.spawn().expect("parent-death harness should spawn");
    let mut harness = HarnessCleanup(Some(child));

    let status = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let child = harness.0.as_mut().expect("harness child should be owned");
            if let Some(status) = child.try_wait()? {
                return Ok::<_, io::Error>(status);
            }
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
    })
    .await
    .expect("parent-death harness should exit within its bounded setup window")
    .expect("harness should be reaped");
    harness.0.take();
    assert!(
        status.success(),
        "harness failed before deliberate _exit (pty={pty}): {status:?}"
    );
    let leader = read_pid(&leader_marker).expect("leader PID should be published");
    let grandchild = read_pid(&grandchild_marker).expect("grandchild PID should be published");
    let mut group_cleanup = GroupCleanup {
        pgid: leader,
        members: vec![leader, grandchild],
        armed: true,
    };

    let statuses = reap_owned_group(leader, grandchild, &baseline_children, pty)
        .await
        .expect("watchdog-owned descendants should become exactly reapable");

    if pty {
        assert_pty_leader_termination(statuses.get(&leader));
    } else {
        assert_sigkill(statuses.get(&leader), "leader");
    }
    assert_sigkill(statuses.get(&grandchild), "grandchild");
    assert!(
        statuses
            .iter()
            .any(|(pid, status)| *pid != leader && *pid != grandchild && was_sigkill(*status)),
        "the {} watchdog was not discovered and reaped: {statuses:?}",
        if pty {
            "external-session"
        } else {
            "process-group"
        }
    );
    assert!(!process_exists(leader));
    assert!(!process_exists(grandchild));
    group_cleanup.armed = false;
}

#[cfg(target_os = "linux")]
async fn reap_owned_group(
    leader: libc::pid_t,
    grandchild: libc::pid_t,
    baseline_children: &BTreeSet<libc::pid_t>,
    include_external_watchdog: bool,
) -> io::Result<BTreeMap<libc::pid_t, ExitStatus>> {
    let mut statuses = BTreeMap::new();
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let mut candidates = BTreeSet::from([leader, grandchild]);
            let direct = direct_children()?;
            for pid in &direct {
                if *pid == leader
                    || *pid == grandchild
                    || process_group(*pid) == Some(leader)
                    || (include_external_watchdog && !baseline_children.contains(pid))
                {
                    candidates.insert(*pid);
                }
            }
            for pid in candidates {
                if statuses.contains_key(&pid) {
                    continue;
                }
                if let Some(status) = reap_exact_if_ready(pid)? {
                    statuses.insert(pid, status);
                }
            }
            let remaining_owned_child = direct.into_iter().any(|pid| {
                pid == leader
                    || pid == grandchild
                    || process_group(pid) == Some(leader)
                    || (include_external_watchdog && !baseline_children.contains(&pid))
            });
            let watchdog_reaped = !include_external_watchdog
                || statuses.iter().any(|(pid, status)| {
                    *pid != leader && *pid != grandchild && was_sigkill(*status)
                });
            if !process_exists(leader)
                && !process_exists(grandchild)
                && !remaining_owned_child
                && watchdog_reaped
            {
                return Ok(statuses);
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "adopted process-group reap timed out"))?
}

#[cfg(target_os = "linux")]
fn direct_children() -> io::Result<BTreeSet<libc::pid_t>> {
    let mut children = BTreeSet::new();
    for task in fs::read_dir("/proc/self/task")? {
        let path = task?.path().join("children");
        let Ok(contents) = fs::read_to_string(path) else {
            continue;
        };
        for field in contents.split_whitespace() {
            if let Ok(pid) = field.parse::<libc::pid_t>() {
                children.insert(pid);
            }
        }
    }
    Ok(children)
}

#[cfg(target_os = "linux")]
fn process_group(pid: libc::pid_t) -> Option<libc::pid_t> {
    // SAFETY: getpgid only inspects the identity named by pid.
    let pgid = unsafe { libc::getpgid(pid) };
    (pgid >= 0).then_some(pgid)
}

#[cfg(target_os = "linux")]
fn reap_exact_if_ready(pid: libc::pid_t) -> io::Result<Option<ExitStatus>> {
    let mut status = 0;
    // SAFETY: pid is an exact child identity discovered from /proc or a published marker.
    let waited = unsafe { libc::waitpid(pid, &mut status, libc::WNOHANG) };
    if waited == pid {
        use std::os::unix::process::ExitStatusExt;
        return Ok(Some(ExitStatus::from_raw(status)));
    }
    if waited == 0 {
        return Ok(None);
    }
    let error = io::Error::last_os_error();
    if error.raw_os_error() == Some(libc::ECHILD) {
        Ok(None)
    } else {
        Err(error)
    }
}

#[cfg(target_os = "linux")]
fn assert_sigkill(status: Option<&ExitStatus>, label: &str) {
    assert!(
        status.is_some_and(|status| was_sigkill(*status)),
        "{label} was not reaped from SIGKILL: {status:?}"
    );
}

#[cfg(target_os = "linux")]
fn assert_pty_leader_termination(status: Option<&ExitStatus>) {
    use std::os::unix::process::ExitStatusExt;

    assert!(
        status.is_some_and(|status| matches!(
            status.signal(),
            Some(libc::SIGHUP | libc::SIGKILL)
        )),
        "PTY leader was not reaped from terminal hangup or watchdog SIGKILL: {status:?}"
    );
}

#[cfg(target_os = "linux")]
fn was_sigkill(status: ExitStatus) -> bool {
    use std::os::unix::process::ExitStatusExt;
    status.signal() == Some(libc::SIGKILL)
}

#[cfg(target_os = "linux")]
fn read_pid(path: &Path) -> io::Result<libc::pid_t> {
    fs::read_to_string(path)?
        .trim()
        .parse::<libc::pid_t>()
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
}

#[cfg(target_os = "linux")]
fn process_exists(pid: libc::pid_t) -> bool {
    // SAFETY: signal zero probes liveness without delivering a signal.
    if unsafe { libc::kill(pid, 0) } == 0 {
        return true;
    }
    io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH)
}

#[cfg(target_os = "linux")]
struct SubreaperGuard {
    previous: libc::c_int,
}

#[cfg(target_os = "linux")]
impl SubreaperGuard {
    fn install() -> io::Result<Self> {
        let mut previous = 0;
        // SAFETY: prctl writes one c_int to the supplied valid pointer.
        if unsafe { libc::prctl(libc::PR_GET_CHILD_SUBREAPER, &mut previous) } == -1 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: PR_SET_CHILD_SUBREAPER accepts the integral enabled flag.
        if unsafe { libc::prctl(libc::PR_SET_CHILD_SUBREAPER, 1) } == -1 {
            return Err(io::Error::last_os_error());
        }
        Ok(Self { previous })
    }
}

#[cfg(target_os = "linux")]
impl Drop for SubreaperGuard {
    fn drop(&mut self) {
        // SAFETY: restores the process-wide flag captured by install.
        let _ = unsafe { libc::prctl(libc::PR_SET_CHILD_SUBREAPER, self.previous) };
    }
}

#[cfg(target_os = "linux")]
struct HarnessCleanup(Option<Child>);

#[cfg(target_os = "linux")]
impl Drop for HarnessCleanup {
    fn drop(&mut self) {
        if let Some(child) = self.0.as_mut() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

#[cfg(target_os = "linux")]
struct GroupCleanup {
    pgid: libc::pid_t,
    members: Vec<libc::pid_t>,
    armed: bool,
}

#[cfg(target_os = "linux")]
impl Drop for GroupCleanup {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        // SAFETY: this guard owns the helper-created process group.
        let _ = unsafe { libc::kill(-self.pgid, libc::SIGKILL) };
        for pid in &self.members {
            // SAFETY: these exact PIDs were published by the harness and helper.
            let _ = unsafe { libc::kill(*pid, libc::SIGKILL) };
        }
    }
}

#[cfg(target_os = "macos")]
mod macos_parent_death {
    use std::{
        fs, io,
        os::fd::RawFd,
        path::Path,
        process::{Child, Command},
        time::{Duration, Instant},
    };

    use super::{harness_binary, helper_binary};

    #[tokio::test]
    #[serial_test::serial]
    async fn abrupt_harness_exit_kills_the_owned_process_group() {
        assert_abrupt_harness_exit_kills_owned_group(false).await;
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn abrupt_harness_exit_kills_the_owned_pty_session() {
        assert_abrupt_harness_exit_kills_owned_group(true).await;
    }

    async fn assert_abrupt_harness_exit_kills_owned_group(pty: bool) {
        let directory = tempfile::tempdir().expect("temporary directory should be created");
        let leader_marker = directory.path().join("leader.pid");
        let grandchild_marker = directory.path().join("grandchild.pid");
        let start_gate = directory.path().join("start.gate");
        let ready_gate = directory.path().join("ready.gate");
        let exit_gate = directory.path().join("exit.gate");
        let mut command = Command::new(harness_binary());
        command
            .arg(helper_binary())
            .arg(&leader_marker)
            .arg(&grandchild_marker);
        if pty {
            command.arg("pty");
        }
        let child = command
            .arg(&start_gate)
            .arg(&ready_gate)
            .arg(&exit_gate)
            .spawn()
            .expect("parent-death harness should spawn");
        let harness_pid = child.id();
        let mut harness = HarnessGuard(Some(child));
        let mut harness_watch =
            ProcessExitWatch::register(harness_pid).expect("harness kqueue watch should register");

        publish_gate(&start_gate);
        wait_for_gate(&ready_gate).await;
        let leader_pid = read_pid_marker(&leader_marker);
        let grandchild_pid = read_pid_marker(&grandchild_marker);
        let mut group_cleanup = GroupCleanup {
            pgid: leader_pid as libc::pid_t,
            members: [leader_pid as libc::pid_t, grandchild_pid as libc::pid_t],
            armed: true,
        };
        let mut leader_watch =
            ProcessExitWatch::register(leader_pid).expect("leader kqueue watch should register");
        let mut grandchild_watch = ProcessExitWatch::register(grandchild_pid)
            .expect("grandchild kqueue watch should register");

        publish_gate(&exit_gate);
        harness_watch
            .wait_terminated(Duration::from_secs(5), "harness")
            .await;
        let status = harness
            .0
            .as_mut()
            .expect("harness child should be owned")
            .wait()
            .expect("harness should be reaped");
        harness.0.take();
        assert!(
            status.success(),
            "harness failed before deliberate _exit (pty={pty}): {status:?}"
        );

        leader_watch
            .wait_terminated(Duration::from_secs(5), "leader")
            .await;
        grandchild_watch
            .wait_terminated(Duration::from_secs(5), "grandchild")
            .await;
        group_cleanup.armed = false;
    }

    fn publish_gate(path: &Path) {
        fs::write(path, b"go").unwrap_or_else(|error| {
            panic!("gate should be published at {}: {error}", path.display())
        });
    }

    async fn wait_for_gate(path: &Path) {
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if path.is_file() {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap_or_else(|_| panic!("gate was not published: {}", path.display()));
    }

    fn read_pid_marker(path: &Path) -> u32 {
        fs::read_to_string(path)
            .unwrap_or_else(|error| {
                panic!("PID marker should be readable at {}: {error}", path.display())
            })
            .trim()
            .parse()
            .unwrap_or_else(|error| {
                panic!("PID marker should contain a PID at {}: {error}", path.display())
            })
    }

    struct ProcessExitWatch {
        pid: u32,
        kqueue: RawFd,
    }

    impl ProcessExitWatch {
        fn register(pid: u32) -> io::Result<Self> {
            let native_pid = libc::pid_t::try_from(pid)
                .ok()
                .filter(|pid| *pid > 1)
                .ok_or_else(|| io::Error::from_raw_os_error(libc::EINVAL))?;
            let kqueue = loop {
                // SAFETY: kqueue returns a fresh descriptor or -1 with errno.
                let descriptor = unsafe { libc::kqueue() };
                if descriptor >= 0 {
                    break descriptor;
                }
                let error = io::Error::last_os_error();
                if error.raw_os_error() != Some(libc::EINTR) {
                    return Err(error);
                }
            };
            let change = libc::kevent {
                ident: native_pid as libc::uintptr_t,
                filter: libc::EVFILT_PROC,
                flags: libc::EV_ADD | libc::EV_ENABLE | libc::EV_RECEIPT,
                fflags: libc::NOTE_EXIT,
                data: 0,
                udata: std::ptr::null_mut(),
            };
            // SAFETY: the change and receipt buffers are valid for one event.
            let mut receipt: libc::kevent = unsafe { std::mem::zeroed() };
            let returned = loop {
                // SAFETY: the change and receipt buffers are valid for one event.
                let result = unsafe {
                    libc::kevent(
                        kqueue,
                        &change,
                        1,
                        &mut receipt,
                        1,
                        std::ptr::null(),
                    )
                };
                if result >= 0 {
                    break result;
                }
                let error = io::Error::last_os_error();
                if error.raw_os_error() != Some(libc::EINTR) {
                    // SAFETY: this descriptor was created above and is still owned here.
                    let _ = unsafe { libc::close(kqueue) };
                    return Err(error);
                }
            };
            if returned != 1
                || receipt.ident != native_pid as libc::uintptr_t
                || receipt.filter != libc::EVFILT_PROC
                || receipt.flags & libc::EV_ERROR == 0
                || receipt.data != 0
            {
                let error = if returned == 1 && receipt.data > 0 {
                    io::Error::from_raw_os_error(
                        i32::try_from(receipt.data).unwrap_or(libc::EPROTO),
                    )
                } else {
                    io::Error::from_raw_os_error(libc::EPROTO)
                };
                // SAFETY: this descriptor was created above and is still owned here.
                let _ = unsafe { libc::close(kqueue) };
                return Err(error);
            }
            Ok(Self { pid, kqueue })
        }

        async fn wait_terminated(&mut self, timeout: Duration, label: &str) {
            let deadline = Instant::now() + timeout;
            loop {
                let remaining = deadline.saturating_duration_since(Instant::now());
                let wait = remaining.min(Duration::from_millis(50));
                let timespec = libc::timespec {
                    tv_sec: wait.as_secs() as libc::time_t,
                    tv_nsec: wait.subsec_nanos() as libc::c_long,
                };
                // SAFETY: the event buffer and timeout are valid for one kevent wait.
                let mut event: libc::kevent = unsafe { std::mem::zeroed() };
                let returned = unsafe {
                    libc::kevent(
                        self.kqueue,
                        std::ptr::null(),
                        0,
                        &mut event,
                        1,
                        &timespec,
                    )
                };
                if returned == 1 {
                    let ident = event.ident;
                    let filter = event.filter;
                    let fflags = event.fflags;
                    let flags = event.flags;
                    assert_eq!(ident, self.pid as libc::uintptr_t);
                    assert_eq!(filter, libc::EVFILT_PROC);
                    assert_ne!(fflags & libc::NOTE_EXIT, 0);
                    assert_eq!(flags & libc::EV_ERROR, 0);
                    return;
                }
                if returned < 0 && io::Error::last_os_error().raw_os_error() != Some(libc::EINTR)
                {
                    panic!(
                        "waiting for {label} pid={} failed: {}",
                        self.pid,
                        io::Error::last_os_error()
                    );
                }
                if Instant::now() >= deadline {
                    panic!("{label} pid={} remained alive after {timeout:?}", self.pid);
                }
            }
        }
    }

    impl Drop for ProcessExitWatch {
        fn drop(&mut self) {
            // SAFETY: this wrapper owns exactly one kqueue descriptor.
            let _ = unsafe { libc::close(self.kqueue) };
        }
    }

    struct HarnessGuard(Option<Child>);

    impl Drop for HarnessGuard {
        fn drop(&mut self) {
            if let Some(child) = self.0.as_mut() {
                let _ = child.kill();
                let _ = child.wait();
            }
        }
    }

    struct GroupCleanup {
        pgid: libc::pid_t,
        members: [libc::pid_t; 2],
        armed: bool,
    }

    impl Drop for GroupCleanup {
        fn drop(&mut self) {
            if self.armed {
                // SAFETY: this panic guard owns the helper-created process group.
                let _ = unsafe { libc::kill(-self.pgid, libc::SIGKILL) };
                for pid in self.members {
                    // SAFETY: these exact PIDs were published for this test's process tree.
                    let _ = unsafe { libc::kill(pid, libc::SIGKILL) };
                }
            }
        }
    }
}

#[cfg(windows)]
mod windows_parent_death {
    use std::{
        fs,
        io,
        os::windows::io::AsRawHandle,
        path::Path,
        process::{Child, Command},
        time::{Duration, Instant},
    };

    use windows_sys::Win32::{
        Foundation::{
            CloseHandle, HANDLE, INVALID_HANDLE_VALUE, WAIT_FAILED, WAIT_OBJECT_0, WAIT_TIMEOUT,
        },
        System::{
            JobObjects::{
                AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
                JOBOBJECT_BASIC_ACCOUNTING_INFORMATION, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
                JobObjectBasicAccountingInformation, JobObjectExtendedLimitInformation,
                QueryInformationJobObject, SetInformationJobObject, TerminateJobObject,
            },
            Threading::{
                OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SYNCHRONIZE,
                PROCESS_TERMINATE, TerminateProcess, WaitForSingleObject,
            },
        },
    };

    use super::{harness_binary, helper_binary};

    #[tokio::test]
    #[serial_test::serial]
    async fn abrupt_harness_exit_closes_the_execution_job_and_kills_the_tree() {
        let directory = tempfile::tempdir().expect("temporary directory should be created");
        let leader_marker = directory.path().join("leader.pid");
        let grandchild_marker = directory.path().join("grandchild.pid");
        let start_gate = directory.path().join("start.gate");
        let exit_gate = directory.path().join("exit.gate");
        let fallback_job = TestJob::new().expect("fallback test Job should be created");

        let child = Command::new(harness_binary())
            .arg(helper_binary())
            .arg(&leader_marker)
            .arg(&grandchild_marker)
            .arg(&start_gate)
            .arg(&exit_gate)
            .spawn()
            .expect("parent-death harness should spawn");
        let harness_handle = duplicate_child_process_handle(&child)
            .expect("harness exact process handle should duplicate");
        fallback_job
            .assign(harness_handle.raw())
            .expect("harness should join the fallback test Job");
        let mut harness = HarnessGuard {
            child: Some(child),
            process: harness_handle,
        };

        publish_gate(&start_gate);
        let leader_pid = wait_for_pid_marker(&leader_marker).await;
        let grandchild_pid = wait_for_pid_marker(&grandchild_marker).await;
        let leader =
            ExactProcess::open(leader_pid).expect("leader exact process handle should open");
        let grandchild = ExactProcess::open(grandchild_pid)
            .expect("grandchild exact process handle should open");

        publish_gate(&exit_gate);
        harness
            .process
            .wait_terminated(Duration::from_secs(5), "harness")
            .await;
        let status = harness
            .child
            .as_mut()
            .expect("harness child should be owned")
            .wait()
            .expect("harness should be reaped");
        harness.child.take();
        assert!(
            status.success(),
            "harness failed before deliberate ExitProcess: {status:?}"
        );

        leader
            .wait_terminated(Duration::from_secs(5), "leader")
            .await;
        grandchild
            .wait_terminated(Duration::from_secs(5), "grandchild")
            .await;
        fallback_job
            .wait_empty(Duration::from_secs(5))
            .await
            .expect("fallback Job should become empty without being closed");
    }

    fn publish_gate(path: &Path) {
        fs::write(path, b"go").unwrap_or_else(|error| {
            panic!("gate should be published at {}: {error}", path.display())
        });
    }

    async fn wait_for_pid_marker(path: &Path) -> u32 {
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

    struct OwnedHandle(HANDLE);

    impl OwnedHandle {
        fn new(handle: HANDLE, operation: &'static str) -> io::Result<Self> {
            if handle.is_null() || handle == INVALID_HANDLE_VALUE {
                Err(io::Error::new(
                    io::Error::last_os_error().kind(),
                    format!("{operation}: {}", io::Error::last_os_error()),
                ))
            } else {
                Ok(Self(handle))
            }
        }

        fn raw(&self) -> HANDLE {
            self.0
        }
    }

    impl Drop for OwnedHandle {
        fn drop(&mut self) {
            // SAFETY: the wrapper owns one valid kernel handle and closes it exactly once.
            let _ = unsafe { CloseHandle(self.0) };
        }
    }

    struct ExactProcess {
        pid: u32,
        handle: OwnedHandle,
    }

    impl ExactProcess {
        fn open(pid: u32) -> io::Result<Self> {
            // SAFETY: OpenProcess returns one new non-inheritable exact process handle.
            let handle = OwnedHandle::new(
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

        fn raw(&self) -> HANDLE {
            self.handle.raw()
        }

        async fn wait_terminated(&self, timeout: Duration, label: &str) {
            let deadline = Instant::now() + timeout;
            loop {
                // SAFETY: the exact process handle remains live while it is inspected.
                match unsafe { WaitForSingleObject(self.raw(), 0) } {
                    WAIT_OBJECT_0 => return,
                    WAIT_TIMEOUT if Instant::now() < deadline => {
                        tokio::time::sleep(Duration::from_millis(5)).await;
                    }
                    WAIT_TIMEOUT => {
                        panic!("{label} pid={} remained alive after {timeout:?}", self.pid)
                    }
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

        fn terminate_best_effort(&self) {
            // SAFETY: the exact process handle remains live. Termination is panic cleanup only.
            let _ = unsafe { TerminateProcess(self.raw(), 1) };
            // SAFETY: bounded best-effort wait on the same exact handle.
            let _ = unsafe { WaitForSingleObject(self.raw(), 2_000) };
        }
    }

    fn duplicate_child_process_handle(child: &Child) -> io::Result<ExactProcess> {
        use windows_sys::Win32::{
            Foundation::{DUPLICATE_SAME_ACCESS, DuplicateHandle},
            System::Threading::GetCurrentProcess,
        };

        let mut duplicated = std::ptr::null_mut();
        // SAFETY: the child and pseudo-current-process handles are valid for the call. The
        // duplicate is explicitly non-inheritable and owned by the returned wrapper.
        if unsafe {
            DuplicateHandle(
                GetCurrentProcess(),
                child.as_raw_handle().cast(),
                GetCurrentProcess(),
                &mut duplicated,
                0,
                0,
                DUPLICATE_SAME_ACCESS,
            )
        } == 0
        {
            return Err(io::Error::last_os_error());
        }
        Ok(ExactProcess {
            pid: child.id(),
            handle: OwnedHandle::new(duplicated, "DuplicateHandle")?,
        })
    }

    struct TestJob {
        handle: OwnedHandle,
    }

    impl TestJob {
        fn new() -> io::Result<Self> {
            // SAFETY: null security/name pointers request an anonymous, non-inheritable Job.
            let handle = OwnedHandle::new(
                unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) },
                "CreateJobObjectW",
            )?;
            let mut information = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
            information.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
            // SAFETY: `information` is initialized for the requested information class.
            if unsafe {
                SetInformationJobObject(
                    handle.raw(),
                    JobObjectExtendedLimitInformation,
                    (&information as *const JOBOBJECT_EXTENDED_LIMIT_INFORMATION).cast(),
                    std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
                )
            } == 0
            {
                return Err(io::Error::last_os_error());
            }
            Ok(Self { handle })
        }

        fn assign(&self, process: HANDLE) -> io::Result<()> {
            // SAFETY: both handles remain live for the duration of the call.
            if unsafe { AssignProcessToJobObject(self.handle.raw(), process) } == 0 {
                Err(io::Error::last_os_error())
            } else {
                Ok(())
            }
        }

        async fn wait_empty(&self, timeout: Duration) -> io::Result<()> {
            let deadline = Instant::now() + timeout;
            loop {
                let mut information = JOBOBJECT_BASIC_ACCOUNTING_INFORMATION::default();
                // SAFETY: `information` is writable for the requested information class.
                if unsafe {
                    QueryInformationJobObject(
                        self.handle.raw(),
                        JobObjectBasicAccountingInformation,
                        (&mut information as *mut JOBOBJECT_BASIC_ACCOUNTING_INFORMATION).cast(),
                        std::mem::size_of::<JOBOBJECT_BASIC_ACCOUNTING_INFORMATION>() as u32,
                        std::ptr::null_mut(),
                    )
                } == 0
                {
                    return Err(io::Error::last_os_error());
                }
                if information.ActiveProcesses == 0 {
                    return Ok(());
                }
                if Instant::now() >= deadline {
                    return Err(io::Error::new(
                        io::ErrorKind::TimedOut,
                        format!(
                            "test Job still contained {} active processes",
                            information.ActiveProcesses
                        ),
                    ));
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        }
    }

    impl Drop for TestJob {
        fn drop(&mut self) {
            // SAFETY: this anonymous Job contains only processes created by this test.
            let _ = unsafe { TerminateJobObject(self.handle.raw(), 1) };
        }
    }

    struct HarnessGuard {
        child: Option<Child>,
        process: ExactProcess,
    }

    impl Drop for HarnessGuard {
        fn drop(&mut self) {
            if self.child.is_some() {
                self.process.terminate_best_effort();
                if let Some(child) = self.child.as_mut() {
                    let _ = child.wait();
                }
            }
        }
    }
}

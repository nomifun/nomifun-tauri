use std::{
    fs, io,
    os::{fd::RawFd, unix::ffi::OsStrExt},
};

use super::unix_protocol::{
    Deadline, Frame, FrameKind, Nonce, recv_expected, recv_frame, send_frame,
};

const FIRST_NON_STDIO_FD: RawFd = 3;
const FINAL_KILL_RETRY_MS: libc::c_int = 100;
const WATCHDOG_POLL_MS: libc::c_int = 50;
#[cfg(test)]
pub(super) const EXIT_FAULT_BEFORE_BOOT_READY: libc::c_int = 90;
#[cfg(test)]
pub(super) const EXIT_FAULT_BEFORE_ACK: libc::c_int = 91;
#[cfg(test)]
pub(super) const EXIT_FAULT_AFTER_COMMIT_BEFORE_COMMITTED: libc::c_int = 92;

pub(super) const FAULT_NONE: u8 = 0;
pub(super) const FAULT_EXIT_BEFORE_REGISTRATION: u8 = 1;
pub(super) const FAULT_EXIT_AFTER_ACK: u8 = 2;
#[cfg(test)]
pub(super) const FAULT_SKIP_FINAL_GROUP_KILL: u8 = 3;
#[cfg(test)]
pub(super) const FAULT_EXIT_AFTER_COMMITTED: u8 = 4;
#[cfg(test)]
pub(super) const FAULT_WITHHOLD_ACK: u8 = 5;
#[cfg(test)]
pub(super) const FAULT_EXIT_BEFORE_BOOT_READY: u8 = 6;
#[cfg(test)]
pub(super) const FAULT_EXIT_BEFORE_ACK: u8 = 7;
#[cfg(test)]
pub(super) const FAULT_EXIT_AFTER_COMMIT_BEFORE_COMMITTED: u8 = 8;
#[cfg(test)]
pub(super) const FAULT_FAIL_FINAL_GROUP_KILL_ONCE: u8 = 9;

#[cfg(test)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TestFaultCheckpoint {
    BeforeBootReady,
    BeforeAck,
    AfterCommitBeforeCommitted,
}

#[cfg(test)]
fn test_fault_matches(fault: u8, checkpoint: TestFaultCheckpoint) -> bool {
    matches!(
        (fault, checkpoint),
        (
            FAULT_EXIT_BEFORE_BOOT_READY,
            TestFaultCheckpoint::BeforeBootReady
        ) | (FAULT_EXIT_BEFORE_ACK, TestFaultCheckpoint::BeforeAck)
            | (
                FAULT_EXIT_AFTER_COMMIT_BEFORE_COMMITTED,
                TestFaultCheckpoint::AfterCommitBeforeCommitted
            )
    )
}

#[cfg(test)]
static FORCE_PROC_FALLBACK_PID: std::sync::atomic::AtomicI32 =
    std::sync::atomic::AtomicI32::new(0);

#[cfg(test)]
pub(super) fn force_next_proc_fallback_for_pid(pid: libc::pid_t) {
    assert!(pid > 1, "forced /proc fallback requires a live process identity");
    FORCE_PROC_FALLBACK_PID
        .compare_exchange(
            0,
            pid,
            std::sync::atomic::Ordering::SeqCst,
            std::sync::atomic::Ordering::SeqCst,
        )
        .expect("a forced /proc fallback is already armed");
}

#[cfg(test)]
fn take_forced_proc_fallback(pid: libc::pid_t) -> bool {
    FORCE_PROC_FALLBACK_PID
        .compare_exchange(
            pid,
            0,
            std::sync::atomic::Ordering::SeqCst,
            std::sync::atomic::Ordering::SeqCst,
        )
        .is_ok()
}

#[derive(Clone, Copy)]
pub(super) struct WatchdogConfig {
    pub(super) parent_pid: libc::pid_t,
    pub(super) parent_starttime: u64,
    pub(super) control_fd: RawFd,
    pub(super) registration_fd: RawFd,
    pub(super) null_fd: RawFd,
    pub(super) close_upper_exclusive: RawFd,
    /// A true PTY child is a session leader, so the watchdog cannot join the
    /// child's process group. In that mode it monitors the exact leader from
    /// outside the session, seals the group with `kill(-pgid, SIGKILL)`, then
    /// terminates itself with SIGKILL so the host observes the same lifecycle
    /// fact as the process-group-anchored pipe mode.
    pub(super) external_session: bool,
    pub(super) nonce: Nonce,
    pub(super) deadline: Deadline,
    pub(super) fault: u8,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct FdRange {
    first: RawFd,
    upper_exclusive: RawFd,
}

impl FdRange {
    const fn new(first: RawFd, upper_exclusive: RawFd) -> Self {
        Self {
            first,
            upper_exclusive,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct CloseRangeAttempt {
    syscall_first: u32,
    syscall_last: u32,
    fallback: Option<FdRange>,
}

impl CloseRangeAttempt {
    const fn new(syscall_first: u32, syscall_last: u32, fallback: Option<FdRange>) -> Self {
        Self {
            syscall_first,
            syscall_last,
            fallback,
        }
    }
}

fn optional_fd_range(first: RawFd, upper_exclusive: RawFd) -> Option<FdRange> {
    (first < upper_exclusive).then_some(FdRange::new(first, upper_exclusive))
}

fn planned_close_ranges(
    control_fd: RawFd,
    registration_fd: RawFd,
    close_upper_exclusive: RawFd,
) -> Result<[Option<FdRange>; 3], libc::c_int> {
    if control_fd < FIRST_NON_STDIO_FD
        || registration_fd < FIRST_NON_STDIO_FD
        || control_fd == registration_fd
        || control_fd >= close_upper_exclusive
        || registration_fd >= close_upper_exclusive
    {
        return Err(libc::EINVAL);
    }
    let (lower, upper) = if control_fd < registration_fd {
        (control_fd, registration_fd)
    } else {
        (registration_fd, control_fd)
    };
    Ok([
        optional_fd_range(FIRST_NON_STDIO_FD, lower),
        optional_fd_range(lower + 1, upper),
        optional_fd_range(upper + 1, close_upper_exclusive),
    ])
}

fn planned_close_range_attempts(
    control_fd: RawFd,
    registration_fd: RawFd,
    close_upper_exclusive: RawFd,
) -> Result<[Option<CloseRangeAttempt>; 3], libc::c_int> {
    let ranges = planned_close_ranges(
        control_fd,
        registration_fd,
        close_upper_exclusive,
    )?;
    let upper_kept_fd = control_fd.max(registration_fd);
    Ok([
        ranges[0].map(|range| {
            CloseRangeAttempt::new(
                range.first as u32,
                (range.upper_exclusive - 1) as u32,
                Some(range),
            )
        }),
        ranges[1].map(|range| {
            CloseRangeAttempt::new(
                range.first as u32,
                (range.upper_exclusive - 1) as u32,
                Some(range),
            )
        }),
        Some(CloseRangeAttempt::new(
            (upper_kept_fd + 1) as u32,
            u32::MAX,
            ranges[2],
        )),
    ])
}

fn close_range_needs_fallback(result: libc::c_long) -> bool {
    result != 0
}

fn select_close_upper_exclusive(
    soft_limit: u64,
    hard_limit: u64,
    observed_upper_exclusive: u64,
    kernel_upper_exclusive: u64,
) -> Result<RawFd, libc::c_int> {
    let effective_soft_limit = if soft_limit == libc::RLIM_INFINITY {
        kernel_upper_exclusive
    } else {
        soft_limit
    };
    let effective_hard_limit = if hard_limit == libc::RLIM_INFINITY {
        kernel_upper_exclusive
    } else {
        hard_limit
    };
    RawFd::try_from(
        effective_soft_limit
            .max(effective_hard_limit)
            .max(observed_upper_exclusive)
            .max(FIRST_NON_STDIO_FD as u64),
    )
    .map_err(|_| libc::EOVERFLOW)
}

pub(super) fn capture_close_upper_exclusive() -> io::Result<RawFd> {
    let mut limits = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    if unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut limits) } != 0 {
        return Err(io::Error::last_os_error());
    }

    let mut observed_upper_exclusive = FIRST_NON_STDIO_FD as u64;
    for entry in fs::read_dir("/proc/self/fd")? {
        let name = entry?.file_name();
        let descriptor = parse_u64_raw(name.as_bytes()).ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidData, "non-numeric /proc/self/fd entry")
        })?;
        observed_upper_exclusive = observed_upper_exclusive.max(
            descriptor
                .checked_add(1)
                .ok_or_else(|| io::Error::from_raw_os_error(libc::EOVERFLOW))?,
        );
    }

    let soft_limit = limits.rlim_cur as u64;
    let hard_limit = limits.rlim_max as u64;
    let kernel_upper_exclusive = if hard_limit == libc::RLIM_INFINITY {
        let bytes = fs::read("/proc/sys/fs/nr_open")?;
        parse_u64_raw(trim_ascii_whitespace(&bytes)).ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidData, "invalid /proc/sys/fs/nr_open")
        })?
    } else {
        0
    };
    select_close_upper_exclusive(
        soft_limit,
        hard_limit,
        observed_upper_exclusive,
        kernel_upper_exclusive,
    )
    .map_err(io::Error::from_raw_os_error)
}

fn trim_ascii_whitespace(mut bytes: &[u8]) -> &[u8] {
    while bytes.first().is_some_and(u8::is_ascii_whitespace) {
        bytes = &bytes[1..];
    }
    while bytes.last().is_some_and(u8::is_ascii_whitespace) {
        bytes = &bytes[..bytes.len() - 1];
    }
    bytes
}

#[derive(Clone, Copy)]
enum ProcessMonitor {
    Pidfd(RawFd),
    Proc(ProcIdentity),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum PidfdErrorAction { ProcFallback, IdentityExited, Fatal(i32) }
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct ProcIdentity { pub pid: libc::pid_t, pub ppid: libc::pid_t, pub pgrp: libc::pid_t, pub state: u8, pub starttime: u64 }
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum IdentityObservation { Alive, Stale, Exited }
#[cfg(test)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum WatchdogState { AwaitingDecision, ChildExitedPreCommit, Committed, Quiescing, Aborted }
#[cfg(test)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum WatchdogEvent { ChildExited, Commit, Abort }

pub(super) fn classify_pidfd_error(errno: i32) -> PidfdErrorAction { match errno { libc::ENOSYS|libc::EINVAL|libc::EPERM|libc::EACCES => PidfdErrorAction::ProcFallback, libc::ESRCH => PidfdErrorAction::IdentityExited, other => PidfdErrorAction::Fatal(other) } }
pub(super) fn compare_identity(expected: ProcIdentity, current: Option<ProcIdentity>) -> IdentityObservation { match current { None => IdentityObservation::Exited, Some(value) if value.state == b'Z' || value.state == b'X' => IdentityObservation::Exited, Some(value) if value.pid == expected.pid && value.starttime == expected.starttime => IdentityObservation::Alive, Some(_) => IdentityObservation::Stale } }
#[cfg(test)]
pub(super) fn next_state(state: WatchdogState, event: WatchdogEvent) -> Option<WatchdogState> { match (state,event) { (WatchdogState::AwaitingDecision,WatchdogEvent::ChildExited)=>Some(WatchdogState::ChildExitedPreCommit), (WatchdogState::ChildExitedPreCommit,WatchdogEvent::Commit)=>Some(WatchdogState::Quiescing), (WatchdogState::ChildExitedPreCommit,WatchdogEvent::Abort)=>Some(WatchdogState::Aborted), (WatchdogState::AwaitingDecision,WatchdogEvent::Commit)=>Some(WatchdogState::Committed), _=>None } }
fn ignored_graceful_signal_action() -> Result<libc::sigaction, libc::c_int> {
    // A zeroed sigaction is a fixed-size stack value. It does not allocate or
    // consult process-global Rust state in the post-fork watchdog.
    let mut action: libc::sigaction = unsafe { std::mem::zeroed() };
    action.sa_sigaction = libc::SIG_IGN;
    action.sa_flags = 0;
    if unsafe { libc::sigemptyset(&mut action.sa_mask) } == -1 {
        return Err(last_errno());
    }
    Ok(action)
}

unsafe fn install_signal_action(
    signal: libc::c_int,
    action: &libc::sigaction,
) -> Result<(), libc::c_int> {
    if unsafe { libc::sigaction(signal, action, std::ptr::null_mut()) } == -1 {
        Err(last_errno())
    } else {
        Ok(())
    }
}

unsafe fn ignore_graceful_signals() -> Result<(), libc::c_int> {
    let action = ignored_graceful_signal_action()?;
    unsafe { install_signal_action(libc::SIGINT, &action) }?;
    unsafe { install_signal_action(libc::SIGTERM, &action) }
}

pub(super) fn ignore_graceful_and_anchor(leader: libc::pid_t) -> io::Result<()> {
    unsafe { ignore_graceful_signals() }.map_err(io::Error::from_raw_os_error)?;
    if unsafe { libc::setpgid(0, leader) } == -1 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}
pub(super) fn capture_starttime(pid: libc::pid_t) -> io::Result<u64> { let bytes=fs::read(format!("/proc/{pid}/stat"))?; parse_proc_stat(&bytes,pid).map(|v|v.starttime).ok_or_else(||io::Error::new(io::ErrorKind::InvalidData,"invalid proc stat")) }
pub(super) fn parse_proc_stat(bytes: &[u8], pid: libc::pid_t) -> Option<ProcIdentity> { let close=bytes.iter().rposition(|b|*b==b')')?; let rest=bytes.get(close+2..)?; let fields=rest.split(|b|*b==b' '); let values=fields.filter(|v|!v.is_empty()).collect::<Vec<_>>(); if values.len()<20{return None} Some(ProcIdentity{pid, state:*values[0].first()?, ppid:std::str::from_utf8(values[1]).ok()?.parse().ok()?, pgrp:std::str::from_utf8(values[2]).ok()?.parse().ok()?, starttime:std::str::from_utf8(values[19]).ok()?.parse().ok()?}) }

/// Minimal direct-child watchdog bootstrap. The full committed-state monitor is
/// extended by the next TDD slice; every setup failure closes the protocol and
/// terminates this raw fork child without unwinding.
///
/// # Safety
/// Call only in the child branch immediately after `fork`.
pub(super) unsafe fn run_watchdog(config: WatchdogConfig) -> ! {
    let mut config = match unsafe { prepare_watchdog(config) } {
        Ok(config) => config,
        Err(_) => unsafe { libc::_exit(70) },
    };
    // SAFETY: sigaction changes dispositions only in the dedicated watchdog.
    if unsafe { ignore_graceful_signals() }.is_err() {
        unsafe { exit_watchdog(config, 71) };
    }
    let parent_monitor = match unsafe { open_monitor(config.parent_pid, config.parent_starttime) } {
        Ok(monitor) if unsafe { monitor_alive(monitor) } => monitor,
        _ => unsafe { exit_watchdog(config, 72) },
    };
    #[cfg(test)]
    if test_fault_matches(config.fault, TestFaultCheckpoint::BeforeBootReady) {
        unsafe { exit_watchdog(config, EXIT_FAULT_BEFORE_BOOT_READY) };
    }
    let boot = Frame::new(FrameKind::BootReady, config.nonce, 0, 0);
    if send_frame(config.control_fd, &boot, config.deadline).is_err() {
        unsafe { exit_watchdog(config, 73) };
    }
    if config.fault == FAULT_EXIT_BEFORE_REGISTRATION {
        unsafe { exit_watchdog(config, 74) };
    }
    let registration = match recv_expected(
        config.registration_fd,
        config.nonce,
        FrameKind::Register,
        config.deadline,
    ) {
        Ok(frame) => frame,
        Err(_) => unsafe { exit_watchdog(config, 75) },
    };
    let leader = registration.pid();
    if leader <= 1
        || registration.pgid() != leader
        || unsafe { libc::getpgid(leader) } != leader
    {
        unsafe { exit_watchdog(config, 76) };
    }
    if config.external_session {
        if unsafe { libc::getsid(leader) } != leader
            || unsafe { libc::getpgrp() } == leader
        {
            unsafe { exit_watchdog(config, 76) };
        }
    } else if ignore_graceful_and_anchor(leader).is_err()
        || unsafe { libc::getpgrp() } != leader
    {
        unsafe { exit_watchdog(config, 76) };
    }
    let child_monitor = match unsafe { open_monitor(leader, 0) } {
        Ok(monitor) if unsafe { monitor_alive(monitor) } => monitor,
        _ => unsafe { exit_watchdog(config, 77) },
    };
    if !unsafe { monitor_alive(parent_monitor) } {
        unsafe { quiesce_and_kill(config, leader, 78) };
    }
    #[cfg(test)]
    if test_fault_matches(config.fault, TestFaultCheckpoint::BeforeAck) {
        unsafe { exit_watchdog(config, EXIT_FAULT_BEFORE_ACK) };
    }
    #[cfg(test)]
    if config.fault == FAULT_WITHHOLD_ACK {
        unsafe { test_delay_ms(750) };
        unsafe { exit_watchdog(config, 88) };
    }
    let ack = Frame::new(FrameKind::Ack, config.nonce, leader, leader);
    if send_frame(config.registration_fd, &ack, config.deadline).is_err() {
        unsafe { exit_watchdog(config, 79) };
    }
    unsafe { libc::close(config.registration_fd) };
    config.registration_fd = -1;
    if config.fault == FAULT_EXIT_AFTER_ACK {
        unsafe { exit_watchdog(config, 80) };
    }
    let decision = match recv_frame(config.control_fd, config.nonce, config.deadline) {
        Ok(frame) => frame,
        Err(_) => unsafe { quiesce_and_kill(config, leader, 81) },
    };
    if !unsafe { monitor_alive(parent_monitor) } {
        unsafe { quiesce_and_kill(config, leader, 82) };
    }
    if !decision_frame_is_valid(decision, leader) {
        unsafe { quiesce_and_kill(config, leader, 84) };
    }
    match decision.kind() {
        FrameKind::Abort => unsafe { exit_watchdog(config, 0) },
        FrameKind::Commit => {
            #[cfg(test)]
            if test_fault_matches(
                config.fault,
                TestFaultCheckpoint::AfterCommitBeforeCommitted,
            ) {
                unsafe { exit_watchdog(config, EXIT_FAULT_AFTER_COMMIT_BEFORE_COMMITTED) };
            }
            let committed = Frame::new(FrameKind::Committed, config.nonce, leader, leader);
            if send_frame(config.control_fd, &committed, config.deadline).is_err() {
                unsafe { quiesce_and_kill(config, leader, 83) };
            }
            #[cfg(test)]
            if config.fault == FAULT_EXIT_AFTER_COMMITTED {
                unsafe { test_delay_ms(150) };
                unsafe { libc::kill(libc::getpid(), libc::SIGKILL) };
                unsafe { exit_watchdog(config, 89) };
            }
        }
        _ => unsafe { quiesce_and_kill(config, leader, 84) },
    }
    loop {
        if !unsafe { monitor_alive(parent_monitor) } || !unsafe { monitor_alive(child_monitor) } {
            unsafe { quiesce_and_kill(config, leader, 85) };
        }
        let mut control = libc::pollfd {
            fd: config.control_fd,
            events: libc::POLLIN | libc::POLLHUP | libc::POLLERR,
            revents: 0,
        };
        let ready = unsafe { libc::poll(&mut control, 1, WATCHDOG_POLL_MS) };
        if ready < 0 {
            if io::Error::last_os_error().raw_os_error() == Some(libc::EINTR) {
                continue;
            }
            unsafe { quiesce_and_kill(config, leader, 86) };
        }
        if ready > 0 {
            unsafe { quiesce_and_kill(config, leader, 87) };
        }
    }
}

fn decision_frame_is_valid(frame: Frame, leader: libc::pid_t) -> bool {
    match frame.kind() {
        FrameKind::Commit => frame.pid() == leader && frame.pgid() == leader,
        FrameKind::Abort => frame.pid() == 0 && frame.pgid() == 0,
        _ => false,
    }
}

#[cfg(test)]
unsafe fn test_delay_ms(milliseconds: libc::c_int) {
    loop {
        let result = unsafe { libc::poll(std::ptr::null_mut(), 0, milliseconds) };
        if result >= 0 || last_errno() != libc::EINTR {
            return;
        }
    }
}

unsafe fn quiesce_and_kill(
    config: WatchdogConfig,
    leader: libc::pid_t,
    _error_code: libc::c_int,
) -> ! {
    if let Ok(deadline) = Deadline::after(std::time::Duration::from_millis(50)) {
        let quiescing = Frame::new(FrameKind::Quiescing, config.nonce, leader, leader);
        let _ = send_frame(config.control_fd, &quiescing, deadline);
    }
    let mut kill_attempt = 0_u32;
    let kill_result = unsafe { try_final_group_kill(config, leader, kill_attempt) };
    let Some(retry_delay_ms) = final_kill_retry_delay_ms(kill_result) else {
        unsafe { finish_after_group_seal(config) };
    };
    if let Ok(deadline) = Deadline::after(std::time::Duration::from_millis(50)) {
        let failure = Frame::new(FrameKind::Failure, config.nonce, leader, leader);
        let _ = send_frame(config.control_fd, &failure, deadline);
    }
    loop {
        unsafe { raw_poll_delay(retry_delay_ms) };
        kill_attempt = kill_attempt.saturating_add(1);
        if !group_kill_needs_failure(unsafe {
            try_final_group_kill(config, leader, kill_attempt)
        }) {
            unsafe { finish_after_group_seal(config) };
        }
    }
}

fn group_kill_needs_failure(kill_result: libc::c_int) -> bool {
    kill_result != 0
}

fn final_kill_retry_delay_ms(kill_result: libc::c_int) -> Option<libc::c_int> {
    group_kill_needs_failure(kill_result).then_some(FINAL_KILL_RETRY_MS)
}

unsafe fn try_final_group_kill(
    _config: WatchdogConfig,
    leader: libc::pid_t,
    _attempt: u32,
) -> libc::c_int {
    #[cfg(test)]
    if _config.fault == FAULT_SKIP_FINAL_GROUP_KILL {
        return -1;
    }
    #[cfg(test)]
    if _config.fault == FAULT_FAIL_FINAL_GROUP_KILL_ONCE && _attempt == 0 {
        return -1;
    }
    unsafe { libc::kill(-leader, libc::SIGKILL) }
}

unsafe fn finish_after_group_seal(config: WatchdogConfig) -> ! {
    if config.external_session {
        // The PTY watchdog intentionally lives outside the child's session,
        // so the group SIGKILL cannot reap it as a side effect. Preserve the
        // lifecycle contract by terminating this exact watchdog with SIGKILL.
        unsafe { libc::kill(libc::getpid(), libc::SIGKILL) };
        unsafe { libc::_exit(127) };
    }
    unsafe { hold_for_group_sigkill() }
}

unsafe fn raw_poll_delay(milliseconds: libc::c_int) {
    loop {
        let result = unsafe { libc::poll(std::ptr::null_mut(), 0, milliseconds) };
        if result >= 0 || last_errno() != libc::EINTR {
            return;
        }
    }
}

unsafe fn hold_for_group_sigkill() -> ! {
    loop {
        unsafe { libc::pause() };
    }
}

unsafe fn prepare_watchdog(config: WatchdogConfig) -> Result<WatchdogConfig, libc::c_int> {
    let attempts = planned_close_range_attempts(
        config.control_fd,
        config.registration_fd,
        config.close_upper_exclusive,
    )?;
    if config.null_fd < 0
        || config.null_fd >= config.close_upper_exclusive
        || config.null_fd == config.control_fd
        || config.null_fd == config.registration_fd
    {
        return Err(libc::EINVAL);
    }
    unsafe { require_open_fd(config.null_fd) }?;
    unsafe { require_open_fd(config.control_fd) }?;
    unsafe { require_open_fd(config.registration_fd) }?;

    for descriptor in 0..=2 {
        unsafe { duplicate_to(config.null_fd, descriptor) }?;
    }
    unsafe { close_unknown_fds(attempts, config.control_fd, config.registration_fd) }?;
    Ok(config)
}

unsafe fn duplicate_to(source: RawFd, target: RawFd) -> Result<(), libc::c_int> {
    loop {
        if unsafe { libc::dup2(source, target) } >= 0 {
            return Ok(());
        }
        let errno = last_errno();
        if errno != libc::EINTR {
            return Err(errno);
        }
    }
}

unsafe fn close_unknown_fds(
    attempts: [Option<CloseRangeAttempt>; 3],
    control_fd: RawFd,
    registration_fd: RawFd,
) -> Result<(), libc::c_int> {
    let mut first_error = 0;
    for attempt in attempts.into_iter().flatten() {
        let result = unsafe {
            libc::syscall(
                libc::SYS_close_range,
                attempt.syscall_first,
                attempt.syscall_last,
                0_u32,
            )
        };
        if close_range_needs_fallback(result) {
            if let Some(range) = attempt.fallback {
                if let Err(errno) = unsafe { close_fd_range_fallback(range) } {
                    if first_error == 0 {
                        first_error = errno;
                    }
                }
            }
        }
    }
    if let Err(errno) = unsafe { require_open_fd(control_fd) } {
        if first_error == 0 {
            first_error = errno;
        }
    }
    if let Err(errno) = unsafe { require_open_fd(registration_fd) } {
        if first_error == 0 {
            first_error = errno;
        }
    }
    if first_error == 0 {
        Ok(())
    } else {
        Err(first_error)
    }
}

unsafe fn close_fd_range_fallback(range: FdRange) -> Result<(), libc::c_int> {
    let mut first_error = 0;
    let mut descriptor = range.first;
    while descriptor < range.upper_exclusive {
        if unsafe { libc::close(descriptor) } != 0 {
            let errno = last_errno();
            if errno != libc::EBADF && first_error == 0 {
                first_error = errno;
            }
        }
        descriptor += 1;
    }
    if first_error == 0 {
        Ok(())
    } else {
        Err(first_error)
    }
}

unsafe fn require_open_fd(descriptor: RawFd) -> Result<(), libc::c_int> {
    loop {
        if unsafe { libc::fcntl(descriptor, libc::F_GETFD) } >= 0 {
            return Ok(());
        }
        let errno = last_errno();
        if errno != libc::EINTR {
            return Err(errno);
        }
    }
}

unsafe fn open_monitor(
    pid: libc::pid_t,
    expected_starttime: u64,
) -> Result<ProcessMonitor, libc::c_int> {
    #[cfg(test)]
    if take_forced_proc_fallback(pid) {
        return unsafe { open_proc_monitor(pid, expected_starttime) };
    }
    let descriptor = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0) as RawFd };
    if descriptor >= 0 {
        return Ok(ProcessMonitor::Pidfd(descriptor));
    }
    match classify_pidfd_error(last_errno()) {
        PidfdErrorAction::ProcFallback => unsafe { open_proc_monitor(pid, expected_starttime) },
        PidfdErrorAction::IdentityExited => Err(libc::ESRCH),
        PidfdErrorAction::Fatal(errno) => Err(errno),
    }
}

unsafe fn open_proc_monitor(
    pid: libc::pid_t,
    expected_starttime: u64,
) -> Result<ProcessMonitor, libc::c_int> {
    let identity = unsafe { read_proc_identity_raw(pid) }.ok_or(libc::ESRCH)?;
    if expected_starttime != 0 && identity.starttime != expected_starttime {
        return Err(libc::ESTALE);
    }
    Ok(ProcessMonitor::Proc(identity))
}

unsafe fn monitor_alive(monitor: ProcessMonitor) -> bool {
    match monitor {
        ProcessMonitor::Pidfd(descriptor) => {
            let mut pollfd = libc::pollfd {
                fd: descriptor,
                events: libc::POLLIN,
                revents: 0,
            };
            unsafe { libc::poll(&mut pollfd, 1, 0) == 0 }
        }
        ProcessMonitor::Proc(expected) => matches!(
            compare_identity(expected, unsafe { read_proc_identity_raw(expected.pid) }),
            IdentityObservation::Alive
        ),
    }
}

unsafe fn read_proc_identity_raw(pid: libc::pid_t) -> Option<ProcIdentity> {
    let mut path = [0_u8; 64];
    let prefix = b"/proc/";
    path[..prefix.len()].copy_from_slice(prefix);
    let mut digits = [0_u8; 20];
    let count = decimal_pid(pid, &mut digits)?;
    path[prefix.len()..prefix.len() + count].copy_from_slice(&digits[..count]);
    let suffix = b"/stat\0";
    let offset = prefix.len() + count;
    path[offset..offset + suffix.len()].copy_from_slice(suffix);
    let descriptor = unsafe {
        libc::open(
            path.as_ptr().cast(),
            libc::O_RDONLY | libc::O_CLOEXEC,
        )
    };
    if descriptor < 0 {
        return None;
    }
    let mut bytes = [0_u8; 4096];
    let count = unsafe { libc::read(descriptor, bytes.as_mut_ptr().cast(), bytes.len()) };
    unsafe { libc::close(descriptor) };
    if count <= 0 {
        return None;
    }
    parse_proc_stat_noalloc(&bytes[..count as usize], pid)
}

fn parse_proc_stat_noalloc(bytes: &[u8], pid: libc::pid_t) -> Option<ProcIdentity> {
    let close = bytes.iter().rposition(|byte| *byte == b')')?;
    let mut input = bytes.get(close + 2..)?;
    let mut state = 0;
    let mut ppid = 0;
    let mut pgrp = 0;
    let mut starttime = 0;
    for field in 0..20 {
        while input.first() == Some(&b' ') {
            input = input.get(1..)?;
        }
        let end = input
            .iter()
            .position(|byte| *byte == b' ' || *byte == b'\n')
            .unwrap_or(input.len());
        let token = input.get(..end)?;
        match field {
            0 => state = *token.first()?,
            1 => ppid = parse_pid(token)?,
            2 => pgrp = parse_pid(token)?,
            19 => starttime = parse_u64_raw(token)?,
            _ => {}
        }
        input = input.get(end..)?;
    }
    Some(ProcIdentity {
        pid,
        ppid,
        pgrp,
        state,
        starttime,
    })
}

fn decimal_pid(mut pid: libc::pid_t, output: &mut [u8; 20]) -> Option<usize> {
    if pid <= 0 {
        return None;
    }
    let mut reverse = [0_u8; 20];
    let mut count = 0;
    while pid > 0 {
        reverse[count] = b'0' + (pid % 10) as u8;
        count += 1;
        pid /= 10;
    }
    for index in 0..count {
        output[index] = reverse[count - index - 1];
    }
    Some(count)
}

fn parse_pid(bytes: &[u8]) -> Option<libc::pid_t> {
    libc::pid_t::try_from(parse_u64_raw(bytes)?).ok()
}

fn parse_u64_raw(bytes: &[u8]) -> Option<u64> {
    let mut value = 0_u64;
    if bytes.is_empty() {
        return None;
    }
    for byte in bytes {
        if !byte.is_ascii_digit() {
            return None;
        }
        value = value.checked_mul(10)?.checked_add(u64::from(*byte - b'0'))?;
    }
    Some(value)
}

fn last_errno() -> libc::c_int {
    io::Error::last_os_error()
        .raw_os_error()
        .unwrap_or(libc::EIO)
}

unsafe fn exit_watchdog(config: WatchdogConfig, code: libc::c_int) -> ! {
    // SAFETY: these are the two protocol descriptors owned by this watchdog copy.
    unsafe { libc::close(config.control_fd) };
    unsafe { libc::close(config.registration_fd) };
    unsafe { libc::_exit(code) }
}

#[cfg(test)]
mod tests {
    use super::{
        CloseRangeAttempt, FdRange, IdentityObservation, PidfdErrorAction, ProcIdentity,
        ProcessMonitor, TestFaultCheckpoint, WatchdogEvent, WatchdogState,
        FAULT_EXIT_AFTER_COMMIT_BEFORE_COMMITTED, FAULT_EXIT_BEFORE_ACK,
        FAULT_EXIT_BEFORE_BOOT_READY, FAULT_SKIP_FINAL_GROUP_KILL, WatchdogConfig,
        capture_close_upper_exclusive, capture_starttime, classify_pidfd_error,
        close_range_needs_fallback, compare_identity, decision_frame_is_valid,
        final_kill_retry_delay_ms, force_next_proc_fallback_for_pid,
        group_kill_needs_failure, ignore_graceful_and_anchor, monitor_alive, next_state,
        ignored_graceful_signal_action, install_signal_action, open_monitor, parse_proc_stat,
        planned_close_range_attempts, planned_close_ranges, quiesce_and_kill,
        select_close_upper_exclusive, test_fault_matches,
    };
    use crate::platform::unix_protocol::{
        Deadline, Frame, FrameKind, Nonce, recv_expected,
    };

    struct ChildGroupGuard {
        leader: libc::pid_t,
        watchdog: libc::pid_t,
    }

    #[test]
    fn deterministic_protocol_faults_select_only_the_requested_boundary() {
        let cases = [
            (
                FAULT_EXIT_BEFORE_BOOT_READY,
                6,
                TestFaultCheckpoint::BeforeBootReady,
            ),
            (
                FAULT_EXIT_BEFORE_ACK,
                7,
                TestFaultCheckpoint::BeforeAck,
            ),
            (
                FAULT_EXIT_AFTER_COMMIT_BEFORE_COMMITTED,
                8,
                TestFaultCheckpoint::AfterCommitBeforeCommitted,
            ),
        ];

        for (fault, expected_value, checkpoint) in cases {
            assert_eq!(fault, expected_value);
            for candidate in [
                TestFaultCheckpoint::BeforeBootReady,
                TestFaultCheckpoint::BeforeAck,
                TestFaultCheckpoint::AfterCommitBeforeCommitted,
            ] {
                assert_eq!(
                    test_fault_matches(fault, candidate),
                    candidate == checkpoint,
                    "fault {fault} matched the wrong protocol boundary"
                );
            }
        }
    }

    #[test]
    #[serial_test::serial(unix_spawn)]
    fn forced_proc_fallback_uses_the_real_proc_monitor_path() {
        let pid = unsafe { libc::getpid() };
        let starttime = capture_starttime(pid).expect("current process identity should exist");
        force_next_proc_fallback_for_pid(pid);

        let monitor = unsafe { open_monitor(pid, starttime) }
            .expect("the forced /proc monitor should open for the current identity");
        assert!(matches!(
            monitor,
            ProcessMonitor::Proc(identity)
                if identity.pid == pid && identity.starttime == starttime
        ));
        assert!(unsafe { monitor_alive(monitor) });
    }

    #[test]
    fn ignored_sigaction_is_stack_configured_and_propagates_syscall_errors() {
        let action = ignored_graceful_signal_action()
            .expect("an empty ignored-signal action should be constructible");

        assert_eq!(action.sa_sigaction, libc::SIG_IGN);
        assert_eq!(action.sa_flags, 0);
        assert_eq!(unsafe { libc::sigismember(&action.sa_mask, libc::SIGINT) }, 0);
        assert_eq!(unsafe { libc::sigismember(&action.sa_mask, libc::SIGTERM) }, 0);
        assert_eq!(
            unsafe { install_signal_action(-1, &action) },
            Err(libc::EINVAL),
            "sigaction errors must be returned as raw errno"
        );
    }

    impl Drop for ChildGroupGuard {
        fn drop(&mut self) {
            unsafe {
                if self.leader > 1 {
                    libc::kill(-self.leader, libc::SIGKILL);
                }
                if self.watchdog > 1 {
                    libc::waitpid(self.watchdog, std::ptr::null_mut(), 0);
                }
                if self.leader > 1 {
                    libc::waitpid(self.leader, std::ptr::null_mut(), 0);
                }
            }
        }
    }

    #[test]
    fn dynamic_protocol_fds_split_the_close_bound_without_touching_kept_slots() {
        assert_eq!(
            planned_close_ranges(9, 4, 12),
            Ok([
                Some(FdRange::new(3, 4)),
                Some(FdRange::new(5, 9)),
                Some(FdRange::new(10, 12)),
            ])
        );
        assert_eq!(
            planned_close_ranges(4, 3, 6),
            Ok([None, None, Some(FdRange::new(5, 6))])
        );
        assert_eq!(planned_close_ranges(3, 4, 5), Ok([None, None, None]));
        assert_eq!(planned_close_ranges(3, 3, 64), Err(libc::EINVAL));
        assert_eq!(planned_close_ranges(3, 64, 64), Err(libc::EINVAL));
    }

    #[test]
    fn close_range_failure_falls_back_only_for_each_failed_segment() {
        let syscall_results: [libc::c_long; 3] = [0, -1, 0];
        assert_eq!(
            syscall_results.map(close_range_needs_fallback),
            [false, true, false]
        );

        let attempts = planned_close_range_attempts(9, 4, 12).expect("valid close plan");
        assert_eq!(
            attempts[2],
            Some(CloseRangeAttempt::new(
                10,
                u32::MAX,
                Some(FdRange::new(10, 12)),
            ))
        );
        let adjacent = planned_close_range_attempts(4, 3, 5).expect("valid adjacent plan");
        assert_eq!(
            adjacent[2],
            Some(CloseRangeAttempt::new(5, u32::MAX, None))
        );
    }

    #[test]
    fn captured_close_bound_covers_open_fds_after_the_soft_limit_was_lowered() {
        assert_eq!(
            select_close_upper_exclusive(64, 4_096, 8_193, 1_048_576),
            Ok(8_193)
        );
        assert_eq!(
            select_close_upper_exclusive(64, 4_096, 128, 1_048_576),
            Ok(4_096)
        );
        assert_eq!(
            select_close_upper_exclusive(64, libc::RLIM_INFINITY, 128, 1_048_576),
            Ok(1_048_576)
        );

        unsafe {
            let mut pipe = [-1; 2];
            assert_eq!(libc::pipe2(pipe.as_mut_ptr(), libc::O_CLOEXEC), 0);
            let upper = capture_close_upper_exclusive().expect("close bound should be captured");
            assert!(upper > pipe[0]);
            assert!(upper > pipe[1]);
            libc::close(pipe[0]);
            libc::close(pipe[1]);
        }
    }

    #[test]
    fn decision_frames_require_exact_commit_and_abort_payloads() {
        let nonce = Nonce::new([7; 16]);
        let leader = 4_242;

        assert!(decision_frame_is_valid(
            Frame::new(FrameKind::Commit, nonce, leader, leader),
            leader
        ));
        assert!(!decision_frame_is_valid(
            Frame::new(FrameKind::Commit, nonce, 0, 0),
            leader
        ));
        assert!(!decision_frame_is_valid(
            Frame::new(FrameKind::Commit, nonce, leader, leader + 1),
            leader
        ));
        assert!(decision_frame_is_valid(
            Frame::new(FrameKind::Abort, nonce, 0, 0),
            leader
        ));
        assert!(!decision_frame_is_valid(
            Frame::new(FrameKind::Abort, nonce, leader, leader),
            leader
        ));
        assert!(!decision_frame_is_valid(
            Frame::new(FrameKind::Ack, nonce, leader, leader),
            leader
        ));
    }

    #[test]
    fn failed_final_group_kill_requires_failure_reporting_while_anchor_is_held() {
        assert!(!group_kill_needs_failure(0));
        assert!(group_kill_needs_failure(-1));
        assert_eq!(final_kill_retry_delay_ms(0), None);
        assert_eq!(final_kill_retry_delay_ms(-1), Some(100));
    }

    #[test]
    fn failed_final_group_kill_keeps_anchor_until_host_fallback() {
        unsafe {
            let mut control = [-1; 2];
            assert_eq!(
                libc::socketpair(
                    libc::AF_UNIX,
                    libc::SOCK_SEQPACKET | libc::SOCK_CLOEXEC | libc::SOCK_NONBLOCK,
                    0,
                    control.as_mut_ptr(),
                ),
                0
            );
            let mut ready = [-1; 2];
            assert_eq!(libc::pipe2(ready.as_mut_ptr(), libc::O_CLOEXEC), 0);

            let leader = libc::fork();
            assert!(leader >= 0, "leader fork failed");
            if leader == 0 {
                libc::close(control[0]);
                libc::close(control[1]);
                libc::close(ready[0]);
                if libc::setpgid(0, 0) != 0 {
                    libc::_exit(91);
                }
                let byte = [1_u8];
                if libc::write(ready[1], byte.as_ptr().cast(), 1) != 1 {
                    libc::_exit(92);
                }
                loop {
                    libc::pause();
                }
            }
            assert!(leader > 1, "leader pid must identify the parent branch");
            let mut children = ChildGroupGuard {
                leader,
                watchdog: -1,
            };
            libc::close(ready[1]);
            let mut byte = [0_u8];
            assert_eq!(libc::read(ready[0], byte.as_mut_ptr().cast(), 1), 1);
            libc::close(ready[0]);
            assert_eq!(libc::getpgid(leader), leader);

            let nonce = Nonce::new([9; 16]);
            let deadline = Deadline::after(std::time::Duration::from_secs(2))
                .expect("test deadline should be representable");
            let watchdog = libc::fork();
            assert!(watchdog >= 0, "watchdog fork failed");
            if watchdog == 0 {
                libc::close(control[0]);
                if libc::setpgid(0, leader) != 0 {
                    libc::_exit(93);
                }
                let config = WatchdogConfig {
                    parent_pid: libc::getppid(),
                    parent_starttime: 0,
                    control_fd: control[1],
                    registration_fd: -1,
                    null_fd: -1,
                    close_upper_exclusive: 3,
                    external_session: false,
                    nonce,
                    deadline,
                    fault: FAULT_SKIP_FINAL_GROUP_KILL,
                };
                quiesce_and_kill(config, leader, 94);
            }
            assert!(watchdog > 1, "watchdog pid must identify the parent branch");
            children.watchdog = watchdog;
            libc::close(control[1]);

            let quiescing = recv_expected(control[0], nonce, FrameKind::Quiescing, deadline)
                .expect("watchdog should announce quiescing");
            assert_eq!((quiescing.pid(), quiescing.pgid()), (leader, leader));
            let failure = recv_expected(control[0], nonce, FrameKind::Failure, deadline)
                .expect("watchdog should report the failed final group kill");
            assert_eq!((failure.pid(), failure.pgid()), (leader, leader));
            libc::close(control[0]);

            assert_eq!(libc::getpgid(watchdog), leader);
            let mut premature_status = 0;
            assert_eq!(libc::waitpid(watchdog, &mut premature_status, libc::WNOHANG), 0);

            assert_eq!(libc::kill(-leader, libc::SIGKILL), 0);
            let mut watchdog_status = 0;
            let mut leader_status = 0;
            assert_eq!(libc::waitpid(watchdog, &mut watchdog_status, 0), watchdog);
            children.watchdog = -1;
            assert_eq!(libc::waitpid(leader, &mut leader_status, 0), leader);
            children.leader = -1;
            assert!(libc::WIFSIGNALED(watchdog_status));
            assert_eq!(libc::WTERMSIG(watchdog_status), libc::SIGKILL);
            assert!(libc::WIFSIGNALED(leader_status));
            assert_eq!(libc::WTERMSIG(leader_status), libc::SIGKILL);
        }
    }

    #[test]
    fn pidfd_fallback_is_limited_to_unsupported_or_policy_errors() {
        for errno in [libc::ENOSYS, libc::EINVAL, libc::EPERM, libc::EACCES] {
            assert_eq!(classify_pidfd_error(errno), PidfdErrorAction::ProcFallback);
        }
        assert_eq!(
            classify_pidfd_error(libc::ESRCH),
            PidfdErrorAction::IdentityExited
        );
        assert_eq!(
            classify_pidfd_error(libc::EMFILE),
            PidfdErrorAction::Fatal(libc::EMFILE)
        );
        assert_eq!(
            classify_pidfd_error(libc::EIO),
            PidfdErrorAction::Fatal(libc::EIO)
        );
    }

    #[test]
    fn proc_identity_never_treats_reuse_or_zombie_as_live() {
        let expected = identity(b'S', 91);
        assert_eq!(
            compare_identity(expected, Some(identity(b'S', 91))),
            IdentityObservation::Alive
        );
        assert_eq!(
            compare_identity(expected, Some(identity(b'S', 92))),
            IdentityObservation::Stale
        );
        assert_eq!(
            compare_identity(expected, Some(identity(b'Z', 91))),
            IdentityObservation::Exited
        );
        assert_eq!(
            compare_identity(expected, Some(identity(b'X', 91))),
            IdentityObservation::Exited
        );
        assert_eq!(
            compare_identity(expected, None),
            IdentityObservation::Exited
        );
    }

    #[test]
    fn proc_stat_parser_handles_spaces_and_closing_parens_in_comm() {
        let stat = b"4242 (a worker ) name) S 40 4242 40 0 0 0 1 2 3 4 5 6 7 8 9 10 11 12 987654 13";
        let parsed = parse_proc_stat(stat, 4242).expect("valid proc stat");

        assert_eq!(parsed.pid, 4242);
        assert_eq!(parsed.ppid, 40);
        assert_eq!(parsed.pgrp, 4242);
        assert_eq!(parsed.state, b'S');
        assert_eq!(parsed.starttime, 987654);
    }

    #[test]
    fn child_exit_before_host_decision_is_held_until_commit_or_abort() {
        assert_eq!(
            next_state(WatchdogState::AwaitingDecision, WatchdogEvent::ChildExited),
            Some(WatchdogState::ChildExitedPreCommit)
        );
        assert_eq!(
            next_state(WatchdogState::ChildExitedPreCommit, WatchdogEvent::Commit),
            Some(WatchdogState::Quiescing)
        );
        assert_eq!(
            next_state(WatchdogState::ChildExitedPreCommit, WatchdogEvent::Abort),
            Some(WatchdogState::Aborted)
        );
        assert_eq!(
            next_state(WatchdogState::AwaitingDecision, WatchdogEvent::Commit),
            Some(WatchdogState::Committed)
        );
    }

    #[test]
    fn watchdog_anchor_joins_the_exact_child_group() {
        unsafe {
            let mut ready = [-1; 2];
            assert_eq!(libc::pipe2(ready.as_mut_ptr(), libc::O_CLOEXEC), 0);

            let leader = libc::fork();
            assert!(leader >= 0, "leader fork failed");
            if leader == 0 {
                libc::close(ready[0]);
                if libc::setpgid(0, 0) == -1 {
                    libc::_exit(81);
                }
                let byte = [1_u8];
                if libc::write(ready[1], byte.as_ptr().cast(), 1) != 1 {
                    libc::_exit(82);
                }
                loop {
                    libc::pause();
                }
            }

            libc::close(ready[1]);
            let mut byte = [0_u8];
            assert_eq!(libc::read(ready[0], byte.as_mut_ptr().cast(), 1), 1);
            libc::close(ready[0]);
            assert_eq!(libc::getpgid(leader), leader);

            let anchor = libc::fork();
            assert!(anchor >= 0, "anchor fork failed");
            if anchor == 0 {
                let result = ignore_graceful_and_anchor(leader);
                libc::_exit(if result.is_ok() && libc::getpgrp() == leader {
                    0
                } else {
                    83
                });
            }

            let mut anchor_status = 0;
            assert_eq!(libc::waitpid(anchor, &mut anchor_status, 0), anchor);
            assert!(libc::WIFEXITED(anchor_status));
            assert_eq!(libc::WEXITSTATUS(anchor_status), 0);

            assert_eq!(libc::kill(-leader, libc::SIGKILL), 0);
            let mut leader_status = 0;
            assert_eq!(libc::waitpid(leader, &mut leader_status, 0), leader);
            assert!(libc::WIFSIGNALED(leader_status));
            assert_eq!(libc::WTERMSIG(leader_status), libc::SIGKILL);
        }
    }

    #[test]
    fn current_process_starttime_is_captured_without_lossy_identity() {
        let starttime = capture_starttime(unsafe { libc::getpid() })
            .expect("current process proc identity should be readable");
        assert!(starttime > 0);
    }

    const fn identity(state: u8, starttime: u64) -> ProcIdentity {
        ProcIdentity {
            pid: 7,
            ppid: 3,
            pgrp: 7,
            state,
            starttime,
        }
    }
}

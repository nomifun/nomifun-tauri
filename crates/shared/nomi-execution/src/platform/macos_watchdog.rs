use std::os::fd::RawFd;

use super::unix_protocol::{
    Deadline, Frame, FrameKind, Nonce, recv_expected, recv_frame, send_frame,
};

const FIRST_NON_STDIO_FD: RawFd = 3;
const FD_LIST_BATCH: usize = 64;
const FINAL_KILL_RETRY_MS: libc::c_int = 100;
const SETUP_EVENT_WAIT_MS: libc::c_long = 10;
const MONITOR_EVENT_WAIT_MS: libc::c_long = 50;
const EXIT_PREPARE_MS: u64 = 50;

const EXIT_PREPARE_FAILED: libc::c_int = 70;
const EXIT_SIGNAL_FAILED: libc::c_int = 71;
const EXIT_KQUEUE_FAILED: libc::c_int = 72;
const EXIT_PARENT_REGISTRATION_FAILED: libc::c_int = 73;
const EXIT_PARENT_GONE_BEFORE_BOOT: libc::c_int = 74;
const EXIT_BOOT_SEND_FAILED: libc::c_int = 75;
const EXIT_FAULT_BEFORE_REGISTRATION: libc::c_int = 76;
const EXIT_REGISTRATION_RECEIVE_FAILED: libc::c_int = 77;
const EXIT_REGISTRATION_INVALID: libc::c_int = 78;
const EXIT_ANCHOR_FAILED: libc::c_int = 79;
const EXIT_CHILD_REGISTRATION_FAILED: libc::c_int = 80;
const EXIT_ACK_SEND_FAILED: libc::c_int = 81;
const EXIT_FAULT_AFTER_ACK: libc::c_int = 82;
const EXIT_MONITOR_FAILED: libc::c_int = 83;
const EXIT_CONTROL_FAILED: libc::c_int = 84;
const EXIT_DECISION_INVALID: libc::c_int = 85;
const EXIT_COMMITTED_SEND_FAILED: libc::c_int = 86;
#[cfg(test)]
pub(super) const EXIT_FAULT_BEFORE_BOOT_READY: libc::c_int = 87;
#[cfg(test)]
pub(super) const EXIT_FAULT_BEFORE_ACK: libc::c_int = 88;
#[cfg(test)]
pub(super) const EXIT_FAULT_AFTER_COMMIT_BEFORE_COMMITTED: libc::c_int = 89;

pub(super) const FAULT_NONE: u8 = 0;
pub(super) const FAULT_EXIT_BEFORE_REGISTRATION: u8 = 1;
pub(super) const FAULT_EXIT_AFTER_ACK: u8 = 2;
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

#[derive(Clone, Copy)]
pub(super) struct WatchdogConfig {
    pub(super) parent_pid: libc::pid_t,
    // Kept in the shared platform shape. Darwin identity is bound by the
    // direct-parent relationship plus the registered kqueue knote, not /proc.
    pub(super) parent_starttime: u64,
    pub(super) control_fd: RawFd,
    pub(super) registration_fd: RawFd,
    pub(super) null_fd: RawFd,
    /// True for a controlling-PTY child that owns a separate session. Such a
    /// watchdog must remain outside the child's process group, seal that group
    /// explicitly, and then terminate itself with SIGKILL.
    pub(super) external_session: bool,
    pub(super) nonce: Nonce,
    pub(super) deadline: Deadline,
    pub(super) fault: u8,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct ReceiptSnapshot {
    ident: libc::uintptr_t,
    filter: i16,
    flags: u16,
    data: libc::intptr_t,
}

#[derive(Clone, Copy)]
struct EventSnapshot {
    ident: libc::uintptr_t,
    filter: i16,
    flags: u16,
    fflags: u32,
    data: libc::intptr_t,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum WatchdogState {
    AwaitingDecision,
    ChildExitedPreCommit,
    Committed,
    Quiescing,
    Aborted,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum WatchdogEvent {
    ChildExited,
    Commit,
    Abort,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ProcessEvent {
    ParentExited,
    ChildExited,
}

#[derive(Clone, Copy, Default)]
struct Observation {
    parent_exited: bool,
    child_exited: bool,
}

#[derive(Clone, Copy)]
struct ActiveWatchdog {
    config: WatchdogConfig,
    kqueue_fd: RawFd,
    leader_exit_observed: bool,
}

fn classify_receipt(
    expected_pid: libc::pid_t,
    returned: libc::c_int,
    receipt: ReceiptSnapshot,
) -> Result<(), libc::c_int> {
    if returned != 1
        || expected_pid <= 1
        || receipt.ident != expected_pid as libc::uintptr_t
        || receipt.filter != libc::EVFILT_PROC
        || receipt.flags & libc::EV_ERROR == 0
    {
        return Err(libc::EPROTO);
    }
    if receipt.data == 0 {
        return Ok(());
    }
    match libc::c_int::try_from(receipt.data) {
        Ok(errno) if errno > 0 => Err(errno),
        _ => Err(libc::EPROTO),
    }
}

fn next_state(state: WatchdogState, event: WatchdogEvent) -> Option<WatchdogState> {
    match (state, event) {
        (WatchdogState::AwaitingDecision, WatchdogEvent::ChildExited) => {
            Some(WatchdogState::ChildExitedPreCommit)
        }
        (WatchdogState::AwaitingDecision, WatchdogEvent::Commit) => {
            Some(WatchdogState::Committed)
        }
        (WatchdogState::AwaitingDecision, WatchdogEvent::Abort)
        | (WatchdogState::ChildExitedPreCommit, WatchdogEvent::Abort) => {
            Some(WatchdogState::Aborted)
        }
        (WatchdogState::ChildExitedPreCommit, WatchdogEvent::Commit)
        | (WatchdogState::Committed, WatchdogEvent::ChildExited) => {
            Some(WatchdogState::Quiescing)
        }
        _ => None,
    }
}

/// Runs the allocation-free direct-child Darwin watchdog.
///
/// # Safety
/// Call only in the child branch immediately after the single watchdog
/// `fork`. This function never unwinds or returns to the Rust runtime.
pub(super) unsafe fn run_watchdog(config: WatchdogConfig) -> ! {
    let config = match unsafe { prepare_watchdog(config) } {
        Ok(config) => config,
        Err(_) => unsafe { libc::_exit(EXIT_PREPARE_FAILED) },
    };
    let _ = config.parent_starttime;

    if unsafe { ignore_graceful_signals() }.is_err() {
        unsafe { exit_without_group(config, -1, EXIT_SIGNAL_FAILED) };
    }

    let kqueue_fd = match unsafe { open_kqueue() } {
        Ok(descriptor) => descriptor,
        Err(_) => unsafe { exit_without_group(config, -1, EXIT_KQUEUE_FAILED) },
    };
    let mut watchdog = ActiveWatchdog {
        config,
        kqueue_fd,
        leader_exit_observed: false,
    };

    if unsafe {
        register_process(
            watchdog.kqueue_fd,
            watchdog.config.parent_pid,
            watchdog.config.deadline,
        )
    }
    .is_err()
    {
        unsafe {
            exit_without_group(
                watchdog.config,
                watchdog.kqueue_fd,
                EXIT_PARENT_REGISTRATION_FAILED,
            )
        };
    }
    if !unsafe { parent_is_original(watchdog.config.parent_pid) }
        || match unsafe {
            observe_processes(
                watchdog.kqueue_fd,
                watchdog.config.parent_pid,
                0,
                0,
            )
        } {
            Ok(observation) => observation.parent_exited,
            Err(_) => true,
        }
    {
        unsafe {
            exit_without_group(
                watchdog.config,
                watchdog.kqueue_fd,
                EXIT_PARENT_GONE_BEFORE_BOOT,
            )
        };
    }

    #[cfg(test)]
    if test_fault_matches(
        watchdog.config.fault,
        TestFaultCheckpoint::BeforeBootReady,
    ) {
        unsafe {
            exit_without_group(
                watchdog.config,
                watchdog.kqueue_fd,
                EXIT_FAULT_BEFORE_BOOT_READY,
            )
        };
    }

    let boot = Frame::new(FrameKind::BootReady, watchdog.config.nonce, 0, 0);
    if send_frame(
        watchdog.config.control_fd,
        &boot,
        watchdog.config.deadline,
    )
    .is_err()
    {
        unsafe {
            exit_without_group(
                watchdog.config,
                watchdog.kqueue_fd,
                EXIT_BOOT_SEND_FAILED,
            )
        };
    }
    if watchdog.config.fault == FAULT_EXIT_BEFORE_REGISTRATION {
        unsafe {
            exit_without_group(
                watchdog.config,
                watchdog.kqueue_fd,
                EXIT_FAULT_BEFORE_REGISTRATION,
            )
        };
    }

    let registration = match recv_expected(
        watchdog.config.registration_fd,
        watchdog.config.nonce,
        FrameKind::Register,
        watchdog.config.deadline,
    ) {
        Ok(frame) => frame,
        Err(_) => unsafe {
            exit_without_group(
                watchdog.config,
                watchdog.kqueue_fd,
                EXIT_REGISTRATION_RECEIVE_FAILED,
            )
        },
    };
    let leader = registration.pid();
    if leader <= 1
        || registration.pgid() != leader
        || unsafe { libc::getpgid(leader) } != leader
    {
        unsafe {
            exit_without_group(
                watchdog.config,
                watchdog.kqueue_fd,
                EXIT_REGISTRATION_INVALID,
            )
        };
    }

    if watchdog.config.external_session {
        if unsafe { libc::getsid(leader) } != leader
            || unsafe { libc::getpgrp() } == leader
        {
            unsafe {
                exit_without_group(
                    watchdog.config,
                    watchdog.kqueue_fd,
                    EXIT_ANCHOR_FAILED,
                )
            };
        }
    } else if unsafe { libc::setpgid(0, leader) } == -1
        || unsafe { libc::getpgrp() } != leader
    {
        unsafe {
            exit_without_group(
                watchdog.config,
                watchdog.kqueue_fd,
                EXIT_ANCHOR_FAILED,
            )
        };
    }
    if unsafe {
        register_process(
            watchdog.kqueue_fd,
            leader,
            watchdog.config.deadline,
        )
    }
    .is_err()
    {
        if !unsafe { parent_is_original(watchdog.config.parent_pid) } {
            unsafe { quiesce_and_kill(watchdog, leader, EXIT_CHILD_REGISTRATION_FAILED) };
        }
        unsafe {
            exit_without_group(
                watchdog.config,
                watchdog.kqueue_fd,
                EXIT_CHILD_REGISTRATION_FAILED,
            )
        };
    }

    let post_registration = match unsafe {
        observe_processes(
            watchdog.kqueue_fd,
            watchdog.config.parent_pid,
            leader,
            0,
        )
    } {
        Ok(observation) => observation,
        Err(_) => {
            if !unsafe { parent_is_original(watchdog.config.parent_pid) } {
                unsafe { quiesce_and_kill(watchdog, leader, EXIT_MONITOR_FAILED) };
            }
            unsafe {
                exit_without_group(
                    watchdog.config,
                    watchdog.kqueue_fd,
                    EXIT_MONITOR_FAILED,
                )
            }
        }
    };
    watchdog.leader_exit_observed |= post_registration.child_exited;
    if post_registration.parent_exited
        || !unsafe { parent_is_original(watchdog.config.parent_pid) }
    {
        unsafe { quiesce_and_kill(watchdog, leader, EXIT_MONITOR_FAILED) };
    }
    if post_registration.child_exited || unsafe { libc::getpgid(leader) } != leader {
        unsafe {
            exit_without_group(
                watchdog.config,
                watchdog.kqueue_fd,
                EXIT_MONITOR_FAILED,
            )
        };
    }
    let registered = Frame::new(
        FrameKind::Registered,
        watchdog.config.nonce,
        leader,
        leader,
    );
    if send_frame(
        watchdog.config.control_fd,
        &registered,
        watchdog.config.deadline,
    )
    .is_err()
    {
        unsafe { quiesce_and_kill(watchdog, leader, EXIT_MONITOR_FAILED) };
    }


    #[cfg(test)]
    if test_fault_matches(watchdog.config.fault, TestFaultCheckpoint::BeforeAck) {
        unsafe {
            exit_without_group(
                watchdog.config,
                watchdog.kqueue_fd,
                EXIT_FAULT_BEFORE_ACK,
            )
        };
    }

    #[cfg(test)]
    if watchdog.config.fault == FAULT_WITHHOLD_ACK {
        unsafe { test_delay_ms(750) };
        unsafe {
            exit_without_group(
                watchdog.config,
                watchdog.kqueue_fd,
                EXIT_ACK_SEND_FAILED,
            )
        };
    }
    let ack = Frame::new(FrameKind::Ack, watchdog.config.nonce, leader, leader);
    if send_frame(
        watchdog.config.registration_fd,
        &ack,
        watchdog.config.deadline,
    )
    .is_err()
    {
        if !unsafe { parent_is_original(watchdog.config.parent_pid) } {
            unsafe { quiesce_and_kill(watchdog, leader, EXIT_ACK_SEND_FAILED) };
        }
        unsafe {
            exit_without_group(
                watchdog.config,
                watchdog.kqueue_fd,
                EXIT_ACK_SEND_FAILED,
            )
        };
    }
    unsafe { libc::close(watchdog.config.registration_fd) };
    watchdog.config.registration_fd = -1;
    if watchdog.config.fault == FAULT_EXIT_AFTER_ACK {
        unsafe {
            exit_without_group(
                watchdog.config,
                watchdog.kqueue_fd,
                EXIT_FAULT_AFTER_ACK,
            )
        };
    }

    let mut state = WatchdogState::AwaitingDecision;
    let decision = loop {
        let observation = match unsafe {
            observe_processes(
                watchdog.kqueue_fd,
                watchdog.config.parent_pid,
                leader,
                SETUP_EVENT_WAIT_MS,
            )
        } {
            Ok(observation) => observation,
            Err(_) => unsafe { quiesce_and_kill(watchdog, leader, EXIT_MONITOR_FAILED) },
        };
        watchdog.leader_exit_observed |= observation.child_exited;
        if observation.parent_exited
            || !unsafe { parent_is_original(watchdog.config.parent_pid) }
        {
            unsafe { quiesce_and_kill(watchdog, leader, EXIT_MONITOR_FAILED) };
        }
        if observation.child_exited {
            state = match next_state(state, WatchdogEvent::ChildExited) {
                Some(next) => next,
                None => unsafe { quiesce_and_kill(watchdog, leader, EXIT_MONITOR_FAILED) },
            };
        }
        match unsafe { deadline_expired(watchdog.config.deadline) } {
            Ok(false) => {}
            _ => unsafe { quiesce_and_kill(watchdog, leader, EXIT_CONTROL_FAILED) },
        }
        match unsafe { control_ready(watchdog.config.control_fd) } {
            Ok(false) => continue,
            Err(_) => unsafe { quiesce_and_kill(watchdog, leader, EXIT_CONTROL_FAILED) },
            Ok(true) => {}
        }
        let frame = match recv_frame(
            watchdog.config.control_fd,
            watchdog.config.nonce,
            watchdog.config.deadline,
        ) {
            Ok(frame) => frame,
            Err(_) => unsafe { quiesce_and_kill(watchdog, leader, EXIT_CONTROL_FAILED) },
        };
        let final_observation = match unsafe {
            observe_processes(
                watchdog.kqueue_fd,
                watchdog.config.parent_pid,
                leader,
                0,
            )
        } {
            Ok(observation) => observation,
            Err(_) => unsafe { quiesce_and_kill(watchdog, leader, EXIT_MONITOR_FAILED) },
        };
        watchdog.leader_exit_observed |= final_observation.child_exited;
        if final_observation.parent_exited
            || !unsafe { parent_is_original(watchdog.config.parent_pid) }
        {
            unsafe { quiesce_and_kill(watchdog, leader, EXIT_MONITOR_FAILED) };
        }
        if final_observation.child_exited {
            state = match next_state(state, WatchdogEvent::ChildExited) {
                Some(next) => next,
                None => unsafe { quiesce_and_kill(watchdog, leader, EXIT_MONITOR_FAILED) },
            };
        }
        if !decision_frame_is_valid(frame, leader) {
            unsafe { quiesce_and_kill(watchdog, leader, EXIT_DECISION_INVALID) };
        }
        break frame.kind();
    };

    match decision {
        FrameKind::Abort => {
            if next_state(state, WatchdogEvent::Abort) != Some(WatchdogState::Aborted) {
                unsafe { quiesce_and_kill(watchdog, leader, EXIT_DECISION_INVALID) };
            }
            unsafe { exit_without_group(watchdog.config, watchdog.kqueue_fd, 0) }
        }
        FrameKind::Commit => {
            let committed_state = match next_state(state, WatchdogEvent::Commit) {
                Some(next) => next,
                None => unsafe { quiesce_and_kill(watchdog, leader, EXIT_DECISION_INVALID) },
            };
            #[cfg(test)]
            if test_fault_matches(
                watchdog.config.fault,
                TestFaultCheckpoint::AfterCommitBeforeCommitted,
            ) {
                unsafe {
                    exit_without_group(
                        watchdog.config,
                        watchdog.kqueue_fd,
                        EXIT_FAULT_AFTER_COMMIT_BEFORE_COMMITTED,
                    )
                };
            }
            let committed = Frame::new(
                FrameKind::Committed,
                watchdog.config.nonce,
                leader,
                leader,
            );
            if send_frame(
                watchdog.config.control_fd,
                &committed,
                watchdog.config.deadline,
            )
            .is_err()
            {
                unsafe { quiesce_and_kill(watchdog, leader, EXIT_COMMITTED_SEND_FAILED) };
            }
            #[cfg(test)]
            if watchdog.config.fault == FAULT_EXIT_AFTER_COMMITTED {
                unsafe { test_delay_ms(150) };
                unsafe { libc::kill(libc::getpid(), libc::SIGKILL) };
                unsafe {
                    exit_without_group(
                        watchdog.config,
                        watchdog.kqueue_fd,
                        EXIT_FAULT_AFTER_ACK,
                    )
                };
            }
            if committed_state == WatchdogState::Quiescing {
                unsafe { quiesce_and_kill(watchdog, leader, EXIT_MONITOR_FAILED) };
            }
        }
        _ => unsafe { quiesce_and_kill(watchdog, leader, EXIT_DECISION_INVALID) },
    }

    loop {
        let observation = match unsafe {
            observe_processes(
                watchdog.kqueue_fd,
                watchdog.config.parent_pid,
                leader,
                MONITOR_EVENT_WAIT_MS,
            )
        } {
            Ok(observation) => observation,
            Err(_) => unsafe { quiesce_and_kill(watchdog, leader, EXIT_MONITOR_FAILED) },
        };
        watchdog.leader_exit_observed |= observation.child_exited;
        if observation.parent_exited
            || observation.child_exited
            || !unsafe { parent_is_original(watchdog.config.parent_pid) }
        {
            unsafe { quiesce_and_kill(watchdog, leader, EXIT_MONITOR_FAILED) };
        }
        match unsafe { control_ready(watchdog.config.control_fd) } {
            Ok(false) => {}
            _ => unsafe { quiesce_and_kill(watchdog, leader, EXIT_CONTROL_FAILED) },
        }
    }
}

#[cfg(test)]
unsafe fn test_delay_ms(milliseconds: libc::c_int) {
    loop {
        let returned = unsafe { libc::poll(std::ptr::null_mut(), 0, milliseconds) };
        if returned >= 0 || last_errno() != libc::EINTR {
            return;
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

unsafe fn prepare_watchdog(config: WatchdogConfig) -> Result<WatchdogConfig, libc::c_int> {
    if config.null_fd < 0
        || config.null_fd == config.control_fd
        || config.null_fd == config.registration_fd
        || config.control_fd < FIRST_NON_STDIO_FD
        || config.registration_fd < FIRST_NON_STDIO_FD
        || config.control_fd == config.registration_fd
    {
        return Err(libc::EINVAL);
    }
    unsafe { require_open_fd(config.null_fd) }?;
    unsafe { require_open_fd(config.control_fd) }?;
    unsafe { require_open_fd(config.registration_fd) }?;

    let mut stdio = 0;
    while stdio <= 2 {
        unsafe { duplicate_to(config.null_fd, stdio) }?;
        stdio += 1;
    }
    unsafe {
        close_unknown_fds(
            config.control_fd,
            config.registration_fd,
            config.deadline,
        )
    }?;
    Ok(config)
}

unsafe fn ignore_graceful_signals() -> Result<(), libc::c_int> {
    let mut action: libc::sigaction = unsafe { std::mem::zeroed() };
    action.sa_sigaction = libc::SIG_IGN;
    if unsafe { libc::sigemptyset(&mut action.sa_mask) } == -1 {
        return Err(last_errno());
    }
    if unsafe { libc::sigaction(libc::SIGINT, &action, std::ptr::null_mut()) } == -1
        || unsafe { libc::sigaction(libc::SIGTERM, &action, std::ptr::null_mut()) } == -1
    {
        return Err(last_errno());
    }
    Ok(())
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

const fn should_close_inherited_fd(
    descriptor: RawFd,
    control_fd: RawFd,
    registration_fd: RawFd,
) -> bool {
    descriptor >= FIRST_NON_STDIO_FD
        && descriptor != control_fd
        && descriptor != registration_fd
}

const fn fd_list_batch_complete(count: usize, capacity: usize) -> bool {
    count < capacity
}

fn deadline_active(deadline: Deadline) -> Result<(), libc::c_int> {
    match deadline.is_expired() {
        Ok(false) => Ok(()),
        Ok(true) => Err(libc::ETIMEDOUT),
        Err(error) => Err(error.raw_errno()),
    }
}

unsafe fn close_inherited_fd(
    descriptor: RawFd,
    deadline: Deadline,
) -> Result<(), libc::c_int> {
    loop {
        if unsafe { libc::close(descriptor) } == 0 {
            return Ok(());
        }
        let errno = last_errno();
        if errno == libc::EBADF {
            return Ok(());
        }
        if errno != libc::EINTR {
            return Err(errno);
        }
        deadline_active(deadline)?;
        // No other thread survives in this fork child and this path opens no
        // descriptors, so retrying an EINTR cannot target a reused descriptor.
    }
}

unsafe fn close_unknown_fds(
    control_fd: RawFd,
    registration_fd: RawFd,
    deadline: Deadline,
) -> Result<(), libc::c_int> {
    loop {
        deadline_active(deadline)?;
        // PROC_PIDLISTFDS is available from macOS 10.5. The caller supplies a
        // fixed stack buffer, so this post-fork path enumerates actual open
        // descriptors without allocating or walking the theoretical FD limit.
        let mut entries: [libc::proc_fdinfo; FD_LIST_BATCH] = unsafe { std::mem::zeroed() };
        let buffer_bytes = std::mem::size_of_val(&entries);
        let returned = unsafe {
            libc::proc_pidinfo(
                libc::getpid(),
                libc::PROC_PIDLISTFDS,
                0,
                entries.as_mut_ptr().cast(),
                buffer_bytes as libc::c_int,
            )
        };
        if returned <= 0 {
            let errno = last_errno();
            if errno == libc::EINTR {
                continue;
            }
            return Err(if errno == 0 { libc::EIO } else { errno });
        }
        let returned = returned as usize;
        let entry_bytes = std::mem::size_of::<libc::proc_fdinfo>();
        if returned > buffer_bytes || !returned.is_multiple_of(entry_bytes) {
            return Err(libc::EPROTO);
        }
        let count = returned / entry_bytes;
        let mut closed = 0_usize;
        for entry in &entries[..count] {
            let descriptor = entry.proc_fd;
            if should_close_inherited_fd(descriptor, control_fd, registration_fd) {
                unsafe { close_inherited_fd(descriptor, deadline) }?;
                closed += 1;
            }
        }
        if fd_list_batch_complete(count, FD_LIST_BATCH) {
            unsafe { require_open_fd(control_fd) }?;
            unsafe { require_open_fd(registration_fd) }?;
            return Ok(());
        }
        if closed == 0 {
            return Err(libc::ELOOP);
        }
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

unsafe fn open_kqueue() -> Result<RawFd, libc::c_int> {
    let descriptor = loop {
        let descriptor = unsafe { libc::kqueue() };
        if descriptor >= 0 {
            break descriptor;
        }
        let errno = last_errno();
        if errno != libc::EINTR {
            return Err(errno);
        }
    };
    loop {
        if unsafe { libc::fcntl(descriptor, libc::F_SETFD, libc::FD_CLOEXEC) } == 0 {
            return Ok(descriptor);
        }
        let errno = last_errno();
        if errno != libc::EINTR {
            unsafe { libc::close(descriptor) };
            return Err(errno);
        }
    }
}

unsafe fn register_process(
    kqueue_fd: RawFd,
    pid: libc::pid_t,
    deadline: Deadline,
) -> Result<(), libc::c_int> {
    if pid <= 1 {
        return Err(libc::EINVAL);
    }
    loop {
        let change = libc::kevent {
            ident: pid as libc::uintptr_t,
            filter: libc::EVFILT_PROC,
            flags: libc::EV_ADD | libc::EV_ENABLE | libc::EV_RECEIPT,
            fflags: libc::NOTE_EXIT,
            data: 0,
            udata: std::ptr::null_mut(),
        };
        let mut receipt: libc::kevent = unsafe { std::mem::zeroed() };
        let returned = unsafe {
            libc::kevent(
                kqueue_fd,
                &change,
                1,
                &mut receipt,
                1,
                std::ptr::null(),
            )
        };
        if returned >= 0 {
            return classify_receipt(pid, returned, receipt_snapshot(&receipt));
        }
        let errno = last_errno();
        if errno != libc::EINTR {
            return Err(errno);
        }
        match unsafe { deadline_expired(deadline) } {
            Ok(false) => {}
            Ok(true) => return Err(libc::ETIMEDOUT),
            Err(errno) => return Err(errno),
        }
    }
}

unsafe fn deadline_expired(deadline: Deadline) -> Result<bool, libc::c_int> {
    let mut now = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    if unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut now) } == -1 {
        return Err(last_errno());
    }
    let absolute = deadline.as_timespec();
    Ok(now.tv_sec > absolute.tv_sec
        || (now.tv_sec == absolute.tv_sec && now.tv_nsec >= absolute.tv_nsec))
}

unsafe fn observe_processes(
    kqueue_fd: RawFd,
    parent_pid: libc::pid_t,
    child_pid: libc::pid_t,
    wait_ms: libc::c_long,
) -> Result<Observation, libc::c_int> {
    let mut observation = Observation::default();
    if let Some(event) = unsafe { wait_process_event(kqueue_fd, wait_ms) }? {
        record_event(&mut observation, classify_process_event(event, parent_pid, child_pid)?);
    }
    loop {
        let Some(event) = (unsafe { wait_process_event(kqueue_fd, 0) })? else {
            return Ok(observation);
        };
        record_event(&mut observation, classify_process_event(event, parent_pid, child_pid)?);
    }
}

fn record_event(observation: &mut Observation, event: ProcessEvent) {
    match event {
        ProcessEvent::ParentExited => observation.parent_exited = true,
        ProcessEvent::ChildExited => observation.child_exited = true,
    }
}

fn classify_process_event(
    event: EventSnapshot,
    parent_pid: libc::pid_t,
    child_pid: libc::pid_t,
) -> Result<ProcessEvent, libc::c_int> {
    if event.filter != libc::EVFILT_PROC || event.fflags & libc::NOTE_EXIT == 0 {
        return Err(libc::EPROTO);
    }
    if event.flags & libc::EV_ERROR != 0 {
        return match libc::c_int::try_from(event.data) {
            Ok(errno) if errno > 0 => Err(errno),
            _ => Err(libc::EPROTO),
        };
    }
    if event.ident == parent_pid as libc::uintptr_t {
        return Ok(ProcessEvent::ParentExited);
    }
    if child_pid > 1 && event.ident == child_pid as libc::uintptr_t {
        return Ok(ProcessEvent::ChildExited);
    }
    Err(libc::EPROTO)
}

unsafe fn wait_process_event(
    kqueue_fd: RawFd,
    wait_ms: libc::c_long,
) -> Result<Option<EventSnapshot>, libc::c_int> {
    let timeout = libc::timespec {
        tv_sec: wait_ms / 1_000,
        tv_nsec: (wait_ms % 1_000) * 1_000_000,
    };
    let mut event: libc::kevent = unsafe { std::mem::zeroed() };
    let returned = unsafe {
        libc::kevent(
            kqueue_fd,
            std::ptr::null(),
            0,
            &mut event,
            1,
            &timeout,
        )
    };
    if returned == 0 {
        return Ok(None);
    }
    if returned == 1 {
        return Ok(Some(event_snapshot(&event)));
    }
    let errno = last_errno();
    if errno == libc::EINTR {
        Ok(None)
    } else {
        Err(errno)
    }
}

fn receipt_snapshot(event: &libc::kevent) -> ReceiptSnapshot {
    ReceiptSnapshot {
        ident: event.ident,
        filter: event.filter,
        flags: event.flags,
        data: event.data,
    }
}

fn event_snapshot(event: &libc::kevent) -> EventSnapshot {
    EventSnapshot {
        ident: event.ident,
        filter: event.filter,
        flags: event.flags,
        fflags: event.fflags,
        data: event.data,
    }
}

unsafe fn parent_is_original(parent_pid: libc::pid_t) -> bool {
    parent_pid > 1 && unsafe { libc::getppid() } == parent_pid
}

unsafe fn control_ready(control_fd: RawFd) -> Result<bool, libc::c_int> {
    loop {
        let mut descriptor = libc::pollfd {
            fd: control_fd,
            events: libc::POLLIN | libc::POLLHUP | libc::POLLERR,
            revents: 0,
        };
        let returned = unsafe { libc::poll(&mut descriptor, 1, 0) };
        if returned > 0 {
            return Ok(true);
        }
        if returned == 0 {
            return Ok(false);
        }
        let errno = last_errno();
        if errno != libc::EINTR {
            return Err(errno);
        }
    }
}

unsafe fn quiesce_and_kill(
    mut watchdog: ActiveWatchdog,
    leader: libc::pid_t,
    _reason: libc::c_int,
) -> ! {
    if let Ok(observation) = unsafe {
        observe_processes(
            watchdog.kqueue_fd,
            watchdog.config.parent_pid,
            leader,
            0,
        )
    } {
        watchdog.leader_exit_observed |= observation.child_exited;
    }
    if let Ok(deadline) = Deadline::after(std::time::Duration::from_millis(EXIT_PREPARE_MS)) {
        let quiescing = Frame::new(FrameKind::Quiescing, watchdog.config.nonce, leader, leader);
        let _ = send_frame(watchdog.config.control_fd, &quiescing, deadline);
    }
    let mut kill_attempt = 0_u32;
    let kill_result = unsafe {
        try_final_group_kill(
            watchdog.config,
            leader,
            kill_attempt,
            watchdog.leader_exit_observed,
        )
    };
    let Some(retry_delay_ms) = final_kill_retry_delay_ms(kill_result) else {
        unsafe { finish_after_group_seal(watchdog.config) };
    };
    if let Ok(deadline) = Deadline::after(std::time::Duration::from_millis(EXIT_PREPARE_MS)) {
        let failure = Frame::new(FrameKind::Failure, watchdog.config.nonce, leader, leader);
        let _ = send_frame(watchdog.config.control_fd, &failure, deadline);
    }
    loop {
        unsafe { raw_poll_delay(retry_delay_ms) };
        kill_attempt = kill_attempt.saturating_add(1);
        if let Ok(observation) = unsafe {
            observe_processes(
                watchdog.kqueue_fd,
                watchdog.config.parent_pid,
                leader,
                0,
            )
        } {
            watchdog.leader_exit_observed |= observation.child_exited;
        }
        if !group_kill_needs_failure(unsafe {
            try_final_group_kill(
                watchdog.config,
                leader,
                kill_attempt,
                watchdog.leader_exit_observed,
            )
        }) {
            unsafe { finish_after_group_seal(watchdog.config) };
        }
    }
}

unsafe fn try_final_group_kill(
    config: WatchdogConfig,
    leader: libc::pid_t,
    _attempt: u32,
    leader_exit_observed: bool,
) -> libc::c_int {
    #[cfg(test)]
    if config.fault == FAULT_FAIL_FINAL_GROUP_KILL_ONCE && _attempt == 0 {
        return -1;
    }
    let result = unsafe { libc::kill(-leader, libc::SIGKILL) };
    if result == 0 {
        return 0;
    }
    let errno = last_errno();
    // The original host remains our parent only while it still owns the sole
    // unreaped StdChild leader lease. This watchdog-side EPERM acceptance is
    // provisional: after reaping the watchdog, the host unconditionally
    // repeats the group seal under waitid(WNOWAIT) before it reaps the leader
    // and proves group absence.
    if errno == libc::ESRCH
        || (errno == libc::EPERM
            && leader_exit_observed
            && unsafe { parent_is_original(config.parent_pid) }
            && unsafe { group_contains_only_zombies_anchored_by(leader, leader) }
            && unsafe { parent_is_original(config.parent_pid) })
    {
        return 0;
    }
    -1
}

/// Darwin returns `EPERM` when a process group contains only zombies. The
/// caller must independently observe the anchor child's exit. The host path
/// directly retains an unreaped waitid lease; the external watchdog relies on
/// the protocol invariant described in `try_final_group_kill` and is followed
/// by the host's own leased seal. Legacy host supervision can instead use its
/// watchdog, which is an exact direct child that joined the same group. We
/// verify the anchor is present in both complete snapshots and every member is
/// still a zombie in the requested group. A full buffer or any
/// inconsistent/disappearing member fails closed.
pub(super) unsafe fn group_contains_only_zombies_anchored_by(
    leader: libc::pid_t,
    anchor: libc::pid_t,
) -> bool {
    const CAPACITY: usize = 64;
    const SNAPSHOT_ATTEMPTS: usize = 3;
    for attempt in 0..SNAPSHOT_ATTEMPTS {
        let mut first = [0 as libc::pid_t; CAPACITY];
        let Some(first_count) = (unsafe { complete_group_snapshot(leader, &mut first) }) else {
            return false;
        };
        let first = &first[..first_count];
        if !first.contains(&anchor) {
            return false;
        }
        let first_all_zombies = unsafe { all_members_are_exact_zombies(leader, first) };

        // Close the enumeration/inspection race with a second complete
        // snapshot. A reaper can remove a zombie between these calls, so retry
        // a small fixed number of times; no unstable snapshot is accepted.
        let mut second = [0 as libc::pid_t; CAPACITY];
        let second_count = unsafe { complete_group_snapshot(leader, &mut second) };
        if let Some(second_count) = second_count {
            let second = &second[..second_count];
            if first_all_zombies
                && same_member_set(first, second)
                && unsafe { all_members_are_exact_zombies(leader, second) }
            {
                return true;
            }
        }

        if attempt + 1 < SNAPSHOT_ATTEMPTS {
            unsafe { raw_poll_delay(1) };
        }
    }
    false
}

unsafe fn complete_group_snapshot(
    leader: libc::pid_t,
    members: &mut [libc::pid_t],
) -> Option<usize> {
    let count = unsafe {
        libc::proc_listpgrppids(
            leader,
            members.as_mut_ptr().cast(),
            std::mem::size_of_val(members) as libc::c_int,
        )
    };
    if count <= 0 || count as usize >= members.len() {
        return None;
    }
    let count = count as usize;
    member_snapshot_is_unique_and_safe(&members[..count]).then_some(count)
}

unsafe fn all_members_are_exact_zombies(
    leader: libc::pid_t,
    members: &[libc::pid_t],
) -> bool {
    members.iter().all(|&member| {
        let mut info = std::mem::MaybeUninit::<libc::proc_bsdinfo>::zeroed();
        let expected = std::mem::size_of::<libc::proc_bsdinfo>() as libc::c_int;
        let returned = unsafe {
            libc::proc_pidinfo(
                member,
                libc::PROC_PIDTBSDINFO,
                // XNU searches the zombie list only when this argument is nonzero.
                1,
                info.as_mut_ptr().cast(),
                expected,
            )
        };
        if returned != expected {
            return false;
        }
        // SAFETY: proc_pidinfo reported that it initialized the full structure.
        let info = unsafe { info.assume_init() };
        bsd_info_is_exact_zombie_group_member(leader, member, &info)
    })
}

fn same_member_set(left: &[libc::pid_t], right: &[libc::pid_t]) -> bool {
    left.len() == right.len()
        && member_snapshot_is_unique_and_safe(left)
        && member_snapshot_is_unique_and_safe(right)
        && left.iter().all(|member| right.contains(member))
}

fn member_snapshot_is_unique_and_safe(members: &[libc::pid_t]) -> bool {
    members.iter().enumerate().all(|(index, member)| {
        *member > 1 && !members[..index].iter().any(|prior| prior == member)
    })
}

fn bsd_info_is_exact_zombie_group_member(
    leader: libc::pid_t,
    member: libc::pid_t,
    info: &libc::proc_bsdinfo,
) -> bool {
    info.pbi_pid == member as u32
        && info.pbi_pgid == leader as u32
        && info.pbi_status == libc::SZOMB
}

unsafe fn finish_after_group_seal(config: WatchdogConfig) -> ! {
    if config.external_session {
        unsafe { libc::kill(libc::getpid(), libc::SIGKILL) };
        unsafe { libc::_exit(127) };
    }
    unsafe { hold_for_group_sigkill() }
}

fn group_kill_needs_failure(kill_result: libc::c_int) -> bool {
    kill_result != 0
}

fn final_kill_retry_delay_ms(kill_result: libc::c_int) -> Option<libc::c_int> {
    group_kill_needs_failure(kill_result).then_some(FINAL_KILL_RETRY_MS)
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

unsafe fn exit_without_group(
    config: WatchdogConfig,
    kqueue_fd: RawFd,
    code: libc::c_int,
) -> ! {
    if kqueue_fd >= 0 {
        unsafe { libc::close(kqueue_fd) };
    }
    if config.control_fd >= 0 {
        unsafe { libc::close(config.control_fd) };
    }
    if config.registration_fd >= 0 {
        unsafe { libc::close(config.registration_fd) };
    }
    unsafe { libc::_exit(code) }
}

fn last_errno() -> libc::c_int {
    unsafe { *libc::__error() }
}

#[cfg(test)]
mod tests {
    use super::{
        FAULT_EXIT_AFTER_COMMIT_BEFORE_COMMITTED, FAULT_EXIT_BEFORE_ACK,
        FAULT_EXIT_BEFORE_BOOT_READY, ReceiptSnapshot, TestFaultCheckpoint, WatchdogEvent,
        WatchdogState, classify_receipt, fd_list_batch_complete, final_kill_retry_delay_ms,
        bsd_info_is_exact_zombie_group_member, group_kill_needs_failure, next_state,
        member_snapshot_is_unique_and_safe, same_member_set, should_close_inherited_fd,
        test_fault_matches,
    };

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
    fn darwin_fd_sweep_closes_only_actual_unknown_descriptors() {
        let control_fd = 5;
        let registration_fd = 7;
        let listed = [0, 1, 2, 3, control_fd, registration_fd, 4_097, 999_999];
        let closed = listed
            .into_iter()
            .filter(|descriptor| {
                should_close_inherited_fd(*descriptor, control_fd, registration_fd)
            })
            .collect::<Vec<_>>();

        assert_eq!(closed, vec![3, 4_097, 999_999]);
        assert!(!fd_list_batch_complete(64, 64));
        assert!(fd_list_batch_complete(5, 64));
    }

    #[test]
    fn failed_final_group_kill_requires_failure_reporting_while_anchor_is_held() {
        assert!(!group_kill_needs_failure(0));
        assert!(group_kill_needs_failure(-1));
        assert_eq!(final_kill_retry_delay_ms(0), None);
        assert_eq!(final_kill_retry_delay_ms(-1), Some(100));
    }

    #[test]
    fn zombie_group_member_requires_exact_pid_pgid_and_zombie_status() {
        let leader = 4_242;
        let member = 4_243;
        let mut info = unsafe { std::mem::zeroed::<libc::proc_bsdinfo>() };
        info.pbi_pid = member as u32;
        info.pbi_pgid = leader as u32;
        info.pbi_status = libc::SZOMB;

        assert!(bsd_info_is_exact_zombie_group_member(
            leader, member, &info,
        ));
        info.pbi_status = libc::SRUN;
        assert!(!bsd_info_is_exact_zombie_group_member(
            leader, member, &info,
        ));
        info.pbi_status = libc::SZOMB;
        info.pbi_pid += 1;
        assert!(!bsd_info_is_exact_zombie_group_member(
            leader, member, &info,
        ));
        info.pbi_pid = member as u32;
        info.pbi_pgid += 1;
        assert!(!bsd_info_is_exact_zombie_group_member(
            leader, member, &info,
        ));

        assert!(same_member_set(&[leader, member], &[member, leader]));
        assert!(!same_member_set(&[leader], &[leader, member]));
        assert!(!same_member_set(&[leader, member], &[leader, member + 1]));
        assert!(!same_member_set(&[leader, leader], &[leader, member]));
        assert!(member_snapshot_is_unique_and_safe(&[leader, member]));
        assert!(!member_snapshot_is_unique_and_safe(&[leader, leader]));
        assert!(!member_snapshot_is_unique_and_safe(&[0, leader]));
    }

    #[test]
    fn kqueue_registration_requires_an_exact_success_receipt() {
        let pid = 4_242;
        let receipt = ReceiptSnapshot {
            ident: pid as libc::uintptr_t,
            filter: libc::EVFILT_PROC,
            flags: libc::EV_ERROR,
            data: 0,
        };

        assert_eq!(classify_receipt(pid, 1, receipt), Ok(()));
        assert_eq!(
            classify_receipt(pid + 1, 1, receipt),
            Err(libc::EPROTO)
        );
        assert_eq!(classify_receipt(pid, 0, receipt), Err(libc::EPROTO));
        assert_eq!(
            classify_receipt(
                pid,
                1,
                ReceiptSnapshot {
                    flags: 0,
                    ..receipt
                }
            ),
            Err(libc::EPROTO)
        );
        assert_eq!(
            classify_receipt(
                pid,
                1,
                ReceiptSnapshot {
                    filter: libc::EVFILT_READ,
                    ..receipt
                }
            ),
            Err(libc::EPROTO)
        );
    }

    #[test]
    fn kqueue_registration_surfaces_the_receipt_errno() {
        let pid = 77;
        let receipt = ReceiptSnapshot {
            ident: pid as libc::uintptr_t,
            filter: libc::EVFILT_PROC,
            flags: libc::EV_ERROR,
            data: libc::ESRCH as libc::intptr_t,
        };

        assert_eq!(classify_receipt(pid, 1, receipt), Err(libc::ESRCH));
    }

    #[test]
    fn child_exit_before_decision_preserves_commit_abort_semantics() {
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
        assert_eq!(
            next_state(WatchdogState::AwaitingDecision, WatchdogEvent::Abort),
            Some(WatchdogState::Aborted)
        );
        assert_eq!(
            next_state(WatchdogState::Committed, WatchdogEvent::ChildExited),
            Some(WatchdogState::Quiescing)
        );
    }
}

use std::{
    os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd},
    time::Duration,
};

const MAGIC: [u8; 4] = *b"NFXP";
const VERSION: u16 = 1;
const FRAME_BYTES: usize = 32;
#[cfg(target_os = "macos")]
const STREAM_PREFIX_BYTES: usize = 2;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Nonce([u8; 16]);

impl Nonce {
    pub(super) const fn new(bytes: [u8; 16]) -> Self {
        Self(bytes)
    }

    pub(super) const fn as_bytes(self) -> [u8; 16] {
        self.0
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u16)]
pub(super) enum FrameKind {
    BootReady = 1,
    Register = 2,
    Ack = 3,
    Commit = 4,
    Abort = 5,
    Committed = 6,
    Quiescing = 7,
    Failure = 8,
    Registered = 9,
}

impl FrameKind {
    const fn from_wire(value: u16) -> Option<Self> {
        match value {
            1 => Some(Self::BootReady),
            2 => Some(Self::Register),
            3 => Some(Self::Ack),
            4 => Some(Self::Commit),
            5 => Some(Self::Abort),
            6 => Some(Self::Committed),
            7 => Some(Self::Quiescing),
            8 => Some(Self::Failure),
            9 => Some(Self::Registered),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Frame {
    kind: FrameKind,
    nonce: Nonce,
    pid: libc::pid_t,
    pgid: libc::pid_t,
}

impl Frame {
    pub(super) const fn new(
        kind: FrameKind,
        nonce: Nonce,
        pid: libc::pid_t,
        pgid: libc::pid_t,
    ) -> Self {
        Self {
            kind,
            nonce,
            pid,
            pgid,
        }
    }

    pub(super) const fn kind(self) -> FrameKind {
        self.kind
    }

    pub(super) const fn nonce(self) -> Nonce {
        self.nonce
    }

    pub(super) const fn pid(self) -> libc::pid_t {
        self.pid
    }

    pub(super) const fn pgid(self) -> libc::pid_t {
        self.pgid
    }

    const fn encode(self) -> [u8; FRAME_BYTES] {
        let mut bytes = [0_u8; FRAME_BYTES];
        bytes[0] = MAGIC[0];
        bytes[1] = MAGIC[1];
        bytes[2] = MAGIC[2];
        bytes[3] = MAGIC[3];
        let version = VERSION.to_be_bytes();
        bytes[4] = version[0];
        bytes[5] = version[1];
        let kind = (self.kind as u16).to_be_bytes();
        bytes[6] = kind[0];
        bytes[7] = kind[1];
        let nonce = self.nonce.as_bytes();
        let mut index = 0;
        while index < nonce.len() {
            bytes[8 + index] = nonce[index];
            index += 1;
        }
        let pid = self.pid.to_be_bytes();
        bytes[24] = pid[0];
        bytes[25] = pid[1];
        bytes[26] = pid[2];
        bytes[27] = pid[3];
        let pgid = self.pgid.to_be_bytes();
        bytes[28] = pgid[0];
        bytes[29] = pgid[1];
        bytes[30] = pgid[2];
        bytes[31] = pgid[3];
        bytes
    }

    fn decode(bytes: &[u8; FRAME_BYTES], expected_nonce: Nonce) -> Result<Self, ProtocolError> {
        if bytes[..4] != MAGIC || u16::from_be_bytes([bytes[4], bytes[5]]) != VERSION {
            return Err(ProtocolError::MalformedFrame);
        }
        let kind = FrameKind::from_wire(u16::from_be_bytes([bytes[6], bytes[7]]))
            .ok_or(ProtocolError::MalformedFrame)?;
        let mut nonce = [0_u8; 16];
        nonce.copy_from_slice(&bytes[8..24]);
        let nonce = Nonce::new(nonce);
        if nonce != expected_nonce {
            return Err(ProtocolError::WrongNonce);
        }
        Ok(Self {
            kind,
            nonce,
            pid: libc::pid_t::from_be_bytes([bytes[24], bytes[25], bytes[26], bytes[27]]),
            pgid: libc::pid_t::from_be_bytes([bytes[28], bytes[29], bytes[30], bytes[31]]),
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum ProtocolOperation {
    SocketPair,
    ConfigureFd,
    #[cfg(target_os = "macos")]
    SetNoSigpipe,
    ClockGettime,
    Poll,
    Send,
    Receive,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum ProtocolError {
    Syscall {
        operation: ProtocolOperation,
        errno: libc::c_int,
    },
    DeadlineOverflow,
    DeadlineExceeded,
    PeerClosed,
    ShortFrame {
        received: usize,
    },
    MalformedFrame,
    WrongNonce,
    UnexpectedKind {
        expected: FrameKind,
        received: FrameKind,
    },
}

impl ProtocolError {
    pub(super) const fn raw_errno(self) -> libc::c_int {
        match self {
            Self::Syscall { errno, .. } => errno,
            Self::DeadlineOverflow => libc::EOVERFLOW,
            Self::DeadlineExceeded => libc::ETIMEDOUT,
            Self::PeerClosed => libc::EPIPE,
            Self::ShortFrame { .. }
            | Self::MalformedFrame
            | Self::WrongNonce
            | Self::UnexpectedKind { .. } => libc::EPROTO,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Deadline {
    absolute: libc::timespec,
}

impl Deadline {
    pub(super) fn after(duration: Duration) -> Result<Self, ProtocolError> {
        let now = monotonic_now()?;
        let added_seconds: libc::time_t = duration
            .as_secs()
            .try_into()
            .map_err(|_| ProtocolError::DeadlineOverflow)?;
        let mut seconds = now
            .tv_sec
            .checked_add(added_seconds)
            .ok_or(ProtocolError::DeadlineOverflow)?;
        let mut nanoseconds = now.tv_nsec + libc::c_long::from(duration.subsec_nanos());
        if nanoseconds >= 1_000_000_000 {
            seconds = seconds
                .checked_add(1)
                .ok_or(ProtocolError::DeadlineOverflow)?;
            nanoseconds -= 1_000_000_000;
        }
        Ok(Self {
            absolute: libc::timespec {
                tv_sec: seconds,
                tv_nsec: nanoseconds,
            },
        })
    }

    #[cfg(target_os = "macos")]
    pub(super) const fn as_timespec(self) -> libc::timespec {
        self.absolute
    }

    pub(super) fn is_expired(self) -> Result<bool, ProtocolError> {
        Ok(remaining_nanoseconds(self.absolute, monotonic_now()?) <= 0)
    }

    fn poll_timeout_ms(self) -> Result<libc::c_int, ProtocolError> {
        let remaining = remaining_nanoseconds(self.absolute, monotonic_now()?);
        if remaining <= 0 {
            return Ok(0);
        }
        let milliseconds = (remaining + 999_999) / 1_000_000;
        Ok(milliseconds.min(libc::c_int::MAX as i128) as libc::c_int)
    }
}

pub(super) struct SeqPacketPair {
    first: OwnedFd,
    second: OwnedFd,
}

impl SeqPacketPair {
    pub(super) fn new() -> Result<Self, ProtocolError> {
        let mut raw = [-1; 2];
        #[cfg(target_os = "linux")]
        let socket_type = libc::SOCK_SEQPACKET | libc::SOCK_CLOEXEC | libc::SOCK_NONBLOCK;
        #[cfg(target_os = "macos")]
        let socket_type = libc::SOCK_STREAM;
        if unsafe { libc::socketpair(libc::AF_UNIX, socket_type, 0, raw.as_mut_ptr()) } == -1 {
            return Err(syscall_error(ProtocolOperation::SocketPair));
        }
        let pair = Self {
            first: unsafe { OwnedFd::from_raw_fd(raw[0]) },
            second: unsafe { OwnedFd::from_raw_fd(raw[1]) },
        };
        configure_fd(pair.first.as_raw_fd())?;
        configure_fd(pair.second.as_raw_fd())?;
        #[cfg(target_os = "macos")]
        {
            set_no_sigpipe(pair.first.as_raw_fd())?;
            set_no_sigpipe(pair.second.as_raw_fd())?;
        }
        Ok(pair)
    }

    #[cfg(test)]
    pub(super) const fn first(&self) -> &OwnedFd {
        &self.first
    }

    #[cfg(test)]
    pub(super) const fn second(&self) -> &OwnedFd {
        &self.second
    }

    pub(super) fn into_fds(self) -> (OwnedFd, OwnedFd) {
        (self.first, self.second)
    }
}

pub(super) fn send_frame(
    fd: RawFd,
    frame: &Frame,
    deadline: Deadline,
) -> Result<(), ProtocolError> {
    let encoded = frame.encode();
    #[cfg(target_os = "linux")]
    let bytes: &[u8] = &encoded;
    #[cfg(target_os = "macos")]
    let mut stream_frame = [0_u8; STREAM_PREFIX_BYTES + FRAME_BYTES];
    #[cfg(target_os = "macos")]
    {
        stream_frame[..STREAM_PREFIX_BYTES]
            .copy_from_slice(&(FRAME_BYTES as u16).to_be_bytes());
        stream_frame[STREAM_PREFIX_BYTES..].copy_from_slice(&encoded);
    }
    #[cfg(target_os = "macos")]
    let bytes: &[u8] = &stream_frame;
    let mut offset = 0_usize;
    #[cfg(target_os = "linux")]
    let send_flags = libc::MSG_NOSIGNAL;
    #[cfg(target_os = "macos")]
    let send_flags = 0;
    loop {
        if deadline.is_expired()? {
            return Err(ProtocolError::DeadlineExceeded);
        }
        let sent = unsafe {
            libc::send(
                fd,
                bytes.as_ptr().add(offset).cast(),
                bytes.len() - offset,
                send_flags,
            )
        };
        if sent > 0 {
            #[cfg(target_os = "linux")]
            if sent != bytes.len() as libc::ssize_t {
                return Err(ProtocolError::ShortFrame {
                    received: sent as usize,
                });
            }
            offset += sent as usize;
            if offset == bytes.len() {
                return Ok(());
            }
            #[cfg(target_os = "linux")]
            return Err(ProtocolError::ShortFrame {
                received: offset,
            });
            #[cfg(target_os = "macos")]
            continue;
        }
        if sent == 0 {
            return Err(ProtocolError::ShortFrame { received: offset });
        }
        let errno = last_errno();
        if errno == libc::EINTR {
            if deadline.is_expired()? {
                return Err(ProtocolError::DeadlineExceeded);
            }
            continue;
        }
        if is_would_block(errno) {
            poll_ready(fd, libc::POLLOUT, deadline)?;
            continue;
        }
        return Err(ProtocolError::Syscall {
            operation: ProtocolOperation::Send,
            errno,
        });
    }
}

pub(super) fn recv_frame(
    fd: RawFd,
    expected_nonce: Nonce,
    deadline: Deadline,
) -> Result<Frame, ProtocolError> {
    #[cfg(target_os = "linux")]
    let mut bytes = [0_u8; FRAME_BYTES + 1];
    #[cfg(target_os = "macos")]
    let mut bytes = [0_u8; STREAM_PREFIX_BYTES + FRAME_BYTES];
    #[cfg(target_os = "linux")]
    let offset = 0_usize;
    #[cfg(target_os = "macos")]
    let mut offset = 0_usize;
    loop {
        if deadline.is_expired()? {
            return Err(ProtocolError::DeadlineExceeded);
        }
        #[cfg(target_os = "linux")]
        let receive_len = bytes.len();
        #[cfg(target_os = "linux")]
        let receive_flags = libc::MSG_TRUNC;
        #[cfg(target_os = "macos")]
        let receive_len = bytes.len() - offset;
        #[cfg(target_os = "macos")]
        let receive_flags = 0;
        let received = unsafe {
            libc::recv(
                fd,
                bytes.as_mut_ptr().add(offset).cast(),
                receive_len,
                receive_flags,
            )
        };
        if received == 0 {
            return if offset == 0 {
                Err(ProtocolError::PeerClosed)
            } else {
                #[cfg(target_os = "linux")]
                let received = offset;
                #[cfg(target_os = "macos")]
                let received = offset.saturating_sub(STREAM_PREFIX_BYTES);
                Err(ProtocolError::ShortFrame { received })
            };
        }
        if received > 0 {
            #[cfg(target_os = "linux")]
            {
                if received == FRAME_BYTES as libc::ssize_t {
                    let frame_bytes: &[u8; FRAME_BYTES] = (&bytes[..FRAME_BYTES])
                        .try_into()
                        .map_err(|_| ProtocolError::MalformedFrame)?;
                    return Frame::decode(frame_bytes, expected_nonce);
                }
                if received as usize > FRAME_BYTES {
                    return Err(ProtocolError::MalformedFrame);
                }
                return Err(ProtocolError::ShortFrame {
                    received: received as usize,
                });
            }
            #[cfg(target_os = "macos")]
            {
                offset += received as usize;
                if offset >= STREAM_PREFIX_BYTES {
                    let declared = u16::from_be_bytes([bytes[0], bytes[1]]) as usize;
                    if declared != FRAME_BYTES {
                        return Err(ProtocolError::MalformedFrame);
                    }
                }
                if offset == STREAM_PREFIX_BYTES + FRAME_BYTES {
                    let frame_bytes: &[u8; FRAME_BYTES] =
                        (&bytes[STREAM_PREFIX_BYTES..])
                        .try_into()
                        .map_err(|_| ProtocolError::MalformedFrame)?;
                    return Frame::decode(frame_bytes, expected_nonce);
                }
                continue;
            }
        }
        let errno = last_errno();
        if errno == libc::EINTR {
            if deadline.is_expired()? {
                return Err(ProtocolError::DeadlineExceeded);
            }
            continue;
        }
        if is_would_block(errno) {
            poll_ready(fd, libc::POLLIN, deadline)?;
            continue;
        }
        return Err(ProtocolError::Syscall {
            operation: ProtocolOperation::Receive,
            errno,
        });
    }
}

pub(super) fn recv_expected(
    fd: RawFd,
    expected_nonce: Nonce,
    expected_kind: FrameKind,
    deadline: Deadline,
) -> Result<Frame, ProtocolError> {
    let frame = recv_frame(fd, expected_nonce, deadline)?;
    if frame.kind() != expected_kind {
        return Err(ProtocolError::UnexpectedKind {
            expected: expected_kind,
            received: frame.kind(),
        });
    }
    Ok(frame)
}

fn configure_fd(fd: RawFd) -> Result<(), ProtocolError> {
    let descriptor_flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
    if descriptor_flags == -1
        || unsafe { libc::fcntl(fd, libc::F_SETFD, descriptor_flags | libc::FD_CLOEXEC) } == -1
    {
        return Err(syscall_error(ProtocolOperation::ConfigureFd));
    }
    let status_flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if status_flags == -1
        || unsafe { libc::fcntl(fd, libc::F_SETFL, status_flags | libc::O_NONBLOCK) } == -1
    {
        return Err(syscall_error(ProtocolOperation::ConfigureFd));
    }
    Ok(())
}

#[cfg(target_os = "macos")]
fn set_no_sigpipe(fd: RawFd) -> Result<(), ProtocolError> {
    let enabled: libc::c_int = 1;
    if unsafe {
        libc::setsockopt(
            fd,
            libc::SOL_SOCKET,
            libc::SO_NOSIGPIPE,
            (&enabled as *const libc::c_int).cast(),
            std::mem::size_of_val(&enabled) as libc::socklen_t,
        )
    } == -1
    {
        return Err(syscall_error(ProtocolOperation::SetNoSigpipe));
    }
    Ok(())
}

fn poll_ready(fd: RawFd, events: libc::c_short, deadline: Deadline) -> Result<(), ProtocolError> {
    loop {
        let timeout = deadline.poll_timeout_ms()?;
        if timeout == 0 && deadline.is_expired()? {
            return Err(ProtocolError::DeadlineExceeded);
        }
        let mut descriptor = libc::pollfd {
            fd,
            events,
            revents: 0,
        };
        let result = unsafe { libc::poll(&mut descriptor, 1, timeout) };
        if result > 0 {
            return Ok(());
        }
        if result == 0 {
            if deadline.is_expired()? {
                return Err(ProtocolError::DeadlineExceeded);
            }
            continue;
        }
        let errno = last_errno();
        if errno != libc::EINTR {
            return Err(ProtocolError::Syscall {
                operation: ProtocolOperation::Poll,
                errno,
            });
        }
        if deadline.is_expired()? {
            return Err(ProtocolError::DeadlineExceeded);
        }
    }
}

fn monotonic_now() -> Result<libc::timespec, ProtocolError> {
    let mut now = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    if unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut now) } == -1 {
        return Err(syscall_error(ProtocolOperation::ClockGettime));
    }
    Ok(now)
}

const fn remaining_nanoseconds(deadline: libc::timespec, now: libc::timespec) -> i128 {
    (deadline.tv_sec as i128 - now.tv_sec as i128) * 1_000_000_000
        + (deadline.tv_nsec as i128 - now.tv_nsec as i128)
}

const fn is_would_block(errno: libc::c_int) -> bool {
    errno == libc::EAGAIN || errno == libc::EWOULDBLOCK
}

fn syscall_error(operation: ProtocolOperation) -> ProtocolError {
    ProtocolError::Syscall {
        operation,
        errno: last_errno(),
    }
}

#[cfg(target_os = "linux")]
fn last_errno() -> libc::c_int {
    unsafe { *libc::__errno_location() }
}

#[cfg(target_os = "macos")]
fn last_errno() -> libc::c_int {
    unsafe { *libc::__error() }
}

#[cfg(test)]
mod tests {
    use std::{
        os::fd::{AsRawFd, RawFd},
        time::Duration,
    };

    use super::{
        recv_expected, recv_frame, send_frame, Deadline, Frame, FrameKind, Nonce, ProtocolError,
        ProtocolOperation, SeqPacketPair, FRAME_BYTES,
    };

    const TEST_NONCE: Nonce = Nonce::new([
        0x00, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88, 0x99, 0xaa, 0xbb, 0xcc, 0xdd,
        0xee, 0xff,
    ]);

    #[test]
    fn frame_round_trips_over_seqpacket() {
        let pair = SeqPacketPair::new().expect("create socket pair");
        let frame = Frame::new(FrameKind::Register, TEST_NONCE, 12_345, 12_345);
        let deadline = Deadline::after(Duration::from_secs(1)).expect("create deadline");

        send_frame(pair.first().as_raw_fd(), &frame, deadline).expect("send frame");
        let received = recv_expected(
            pair.second().as_raw_fd(),
            TEST_NONCE,
            FrameKind::Register,
            deadline,
        )
        .expect("receive frame");

        assert_eq!(received, frame);
    }

    #[test]
    fn expired_deadline_never_sends_a_frame() {
        let pair = SeqPacketPair::new().expect("create socket pair");
        let frame = Frame::new(FrameKind::Commit, TEST_NONCE, 12_345, 12_345);

        assert_eq!(
            send_frame(
                pair.first().as_raw_fd(),
                &frame,
                Deadline::after(Duration::ZERO).expect("create expired deadline"),
            ),
            Err(ProtocolError::DeadlineExceeded)
        );
        let mut byte = 0_u8;
        let received = unsafe {
            libc::recv(
                pair.second().as_raw_fd(),
                (&mut byte as *mut u8).cast(),
                1,
                libc::MSG_DONTWAIT,
            )
        };
        assert_eq!(received, -1);
        let errno = std::io::Error::last_os_error().raw_os_error();
        assert!(errno == Some(libc::EAGAIN) || errno == Some(libc::EWOULDBLOCK));
    }

    #[test]
    fn expired_deadline_does_not_consume_a_queued_frame() {
        let pair = SeqPacketPair::new().expect("create socket pair");
        let frame = Frame::new(FrameKind::Ack, TEST_NONCE, 91, 91);
        send_frame(
            pair.first().as_raw_fd(),
            &frame,
            Deadline::after(Duration::from_secs(1)).expect("create send deadline"),
        )
        .expect("queue frame");

        assert_eq!(
            recv_frame(
                pair.second().as_raw_fd(),
                TEST_NONCE,
                Deadline::after(Duration::ZERO).expect("create expired deadline"),
            ),
            Err(ProtocolError::DeadlineExceeded)
        );
        assert_eq!(
            recv_frame(
                pair.second().as_raw_fd(),
                TEST_NONCE,
                Deadline::after(Duration::from_secs(1)).expect("create receive deadline"),
            )
            .expect("expired receive must leave the packet queued"),
            frame
        );
    }

    #[test]
    fn dead_peer_send_reports_epipe_without_delivering_sigpipe() {
        let signal_guard = SigpipeMaskGuard::block();
        assert!(!signal_guard.sigpipe_pending());

        let pair = SeqPacketPair::new().expect("create socket pair");
        let (sender, peer) = pair.into_fds();
        drop(peer);
        let frame = Frame::new(FrameKind::Commit, TEST_NONCE, 44, 44);

        let error = send_frame(
            sender.as_raw_fd(),
            &frame,
            Deadline::after(Duration::from_secs(1)).expect("create deadline"),
        )
        .expect_err("send to a closed peer must fail");

        assert_eq!(
            error,
            ProtocolError::Syscall {
                operation: ProtocolOperation::Send,
                errno: libc::EPIPE,
            }
        );
        assert!(!signal_guard.take_pending_sigpipe());
    }

    #[test]
    fn wrong_nonce_is_rejected() {
        let pair = SeqPacketPair::new().expect("create socket pair");
        let frame = Frame::new(FrameKind::Ack, TEST_NONCE, 81, 81);
        let deadline = Deadline::after(Duration::from_secs(1)).expect("create deadline");
        send_frame(pair.first().as_raw_fd(), &frame, deadline).expect("send frame");

        let wrong_nonce = Nonce::new([0x5a; 16]);
        assert_eq!(
            recv_frame(pair.second().as_raw_fd(), wrong_nonce, deadline),
            Err(ProtocolError::WrongNonce)
        );
    }

    #[test]
    fn short_frame_is_rejected_with_observed_length() {
        let pair = SeqPacketPair::new().expect("create socket pair");
        #[cfg(target_os = "linux")]
        send_raw(pair.first().as_raw_fd(), &[1, 2, 3]);
        #[cfg(target_os = "macos")]
        {
            send_stream_raw(
                pair.first().as_raw_fd(),
                FRAME_BYTES as u16,
                &[1, 2, 3],
            );
            unsafe { libc::shutdown(pair.first().as_raw_fd(), libc::SHUT_WR) };
        }

        assert_eq!(
            recv_frame(
                pair.second().as_raw_fd(),
                TEST_NONCE,
                Deadline::after(Duration::from_secs(1)).expect("create deadline"),
            ),
            Err(ProtocolError::ShortFrame { received: 3 })
        );
    }

    #[test]
    fn malformed_magic_is_rejected() {
        let pair = SeqPacketPair::new().expect("create socket pair");
        let mut malformed = Frame::new(FrameKind::Ack, TEST_NONCE, 91, 91).encode();
        malformed[0] ^= 0xff;
        send_raw(pair.first().as_raw_fd(), &malformed);

        assert_eq!(
            recv_frame(
                pair.second().as_raw_fd(),
                TEST_NONCE,
                Deadline::after(Duration::from_secs(1)).expect("create deadline"),
            ),
            Err(ProtocolError::MalformedFrame)
        );
    }

    #[test]
    fn oversized_packet_is_malformed_instead_of_short() {
        let pair = SeqPacketPair::new().expect("create socket pair");
        send_raw(pair.first().as_raw_fd(), &[0xa5; FRAME_BYTES + 1]);

        assert_eq!(
            recv_frame(
                pair.second().as_raw_fd(),
                TEST_NONCE,
                Deadline::after(Duration::from_secs(1)).expect("create deadline"),
            ),
            Err(ProtocolError::MalformedFrame)
        );
    }

    #[test]
    fn clean_peer_close_is_explicit_eof() {
        let pair = SeqPacketPair::new().expect("create socket pair");
        let (receiver, peer) = pair.into_fds();
        drop(peer);

        assert_eq!(
            recv_frame(
                receiver.as_raw_fd(),
                TEST_NONCE,
                Deadline::after(Duration::from_secs(1)).expect("create deadline"),
            ),
            Err(ProtocolError::PeerClosed)
        );
    }

    fn send_raw(fd: RawFd, bytes: &[u8]) {
        #[cfg(target_os = "macos")]
        {
            send_stream_raw(fd, bytes.len() as u16, bytes);
        }
        #[cfg(target_os = "linux")]
        let sent = unsafe {
            libc::send(
                fd,
                bytes.as_ptr().cast(),
                bytes.len(),
                libc::MSG_NOSIGNAL,
            )
        };
        #[cfg(target_os = "linux")]
        assert_eq!(sent, bytes.len() as libc::ssize_t);
    }

    #[cfg(target_os = "macos")]
    fn send_stream_raw(fd: RawFd, declared: u16, bytes: &[u8]) {
        let prefix = declared.to_be_bytes();
        for part in [&prefix[..], bytes] {
            let mut offset = 0;
            while offset < part.len() {
                let sent = unsafe {
                    libc::send(
                        fd,
                        part.as_ptr().add(offset).cast(),
                        part.len() - offset,
                        0,
                    )
                };
                assert!(sent > 0);
                offset += sent as usize;
            }
        }
    }

    struct SigpipeMaskGuard {
        previous: libc::sigset_t,
        sigpipe: libc::sigset_t,
    }

    impl SigpipeMaskGuard {
        fn block() -> Self {
            let mut previous = unsafe { std::mem::zeroed() };
            let mut sigpipe = unsafe { std::mem::zeroed() };
            unsafe {
                assert_eq!(libc::sigemptyset(&mut sigpipe), 0);
                assert_eq!(libc::sigaddset(&mut sigpipe, libc::SIGPIPE), 0);
                assert_eq!(
                    libc::pthread_sigmask(libc::SIG_BLOCK, &sigpipe, &mut previous),
                    0
                );
            }
            Self { previous, sigpipe }
        }

        fn sigpipe_pending(&self) -> bool {
            let mut pending = unsafe { std::mem::zeroed() };
            unsafe {
                assert_eq!(libc::sigpending(&mut pending), 0);
                libc::sigismember(&pending, libc::SIGPIPE) == 1
            }
        }

        fn take_pending_sigpipe(&self) -> bool {
            if !self.sigpipe_pending() {
                return false;
            }
            let mut signal = 0;
            unsafe {
                assert_eq!(libc::sigwait(&self.sigpipe, &mut signal), 0);
            }
            assert_eq!(signal, libc::SIGPIPE);
            true
        }
    }

    impl Drop for SigpipeMaskGuard {
        fn drop(&mut self) {
            unsafe {
                if self.sigpipe_pending() {
                    let mut signal = 0;
                    libc::sigwait(&self.sigpipe, &mut signal);
                }
                libc::pthread_sigmask(libc::SIG_SETMASK, &self.previous, std::ptr::null_mut());
            }
        }
    }
}

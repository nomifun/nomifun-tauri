use std::{
    io,
    os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd},
    process::Stdio,
    sync::{Arc, Mutex},
};

use tokio::io::unix::AsyncFd;

use crate::{OutputBuffer, OutputStream};

const READ_BUFFER_BYTES: usize = 8 * 1024;

pub(super) struct PtyPair {
    master: OwnedFd,
    slave: OwnedFd,
}

pub(super) struct PtyChildStdio {
    pub(super) stdin: Stdio,
    pub(super) stdout: Stdio,
    pub(super) stderr: Stdio,
}

impl PtyPair {
    pub(super) fn open(cols: u16, rows: u16) -> io::Result<Self> {
        let mut master = -1;
        let mut slave = -1;
        let mut size = libc::winsize {
            ws_row: rows,
            ws_col: cols,
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        // SAFETY: both descriptor pointers and the winsize pointer are valid;
        // null name/termios request the platform defaults.
        let result = unsafe {
            libc::openpty(
                &mut master,
                &mut slave,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                &mut size,
            )
        };
        if result != 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: openpty returned two fresh owned descriptors.
        let pair = Self {
            master: unsafe { OwnedFd::from_raw_fd(master) },
            slave: unsafe { OwnedFd::from_raw_fd(slave) },
        };
        set_cloexec(pair.master.as_raw_fd())?;
        set_cloexec(pair.slave.as_raw_fd())?;
        Ok(pair)
    }

    pub(super) fn slave_fd(&self) -> RawFd {
        self.slave.as_raw_fd()
    }

    pub(super) fn master_fd(&self) -> RawFd {
        self.master.as_raw_fd()
    }

    pub(super) fn child_stdio(&self) -> io::Result<PtyChildStdio> {
        Ok(PtyChildStdio {
            stdin: duplicate_stdio(self.slave.as_raw_fd())?,
            stdout: duplicate_stdio(self.slave.as_raw_fd())?,
            stderr: duplicate_stdio(self.slave.as_raw_fd())?,
        })
    }

    pub(super) fn into_master(self) -> PtyMaster {
        let Self { master, slave } = self;
        drop(slave);
        PtyMaster(master)
    }
}

pub(super) struct PtyMaster(OwnedFd);

impl PtyMaster {
    pub(super) fn into_async(self) -> io::Result<AsyncPtyMaster> {
        set_nonblocking(self.0.as_raw_fd())?;
        Ok(AsyncPtyMaster {
            fd: AsyncFd::new(self.0)?,
            input: tokio::sync::Mutex::new(PtyInputState { closed: false }),
            resize_gate: Mutex::new(()),
        })
    }
}

pub(super) struct AsyncPtyMaster {
    fd: AsyncFd<OwnedFd>,
    /// Serializes writes and canonical EOF as one state transition, so no
    /// write can race past a successfully delivered EOF.
    input: tokio::sync::Mutex<PtyInputState>,
    /// Serializes resize syscalls without forcing the synchronous owner
    /// contract to block on an async mutex.
    resize_gate: Mutex<()>,
}

struct PtyInputState {
    closed: bool,
}

impl AsyncPtyMaster {
    pub(super) async fn write_all(&self, mut bytes: &[u8]) -> io::Result<()> {
        let input = self.input.lock().await;
        if input.closed {
            return Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "PTY stdin is closed",
            ));
        }
        while !bytes.is_empty() {
            let mut ready = self.fd.writable().await?;
            match ready.try_io(|fd| write_once(fd.get_ref().as_raw_fd(), bytes)) {
                Ok(Ok(written)) => bytes = &bytes[written..],
                Ok(Err(error)) => return Err(error),
                Err(_) => continue,
            }
        }
        Ok(())
    }

    pub(super) async fn close_input(&self) -> io::Result<()> {
        let mut input = self.input.lock().await;
        if input.closed {
            return Ok(());
        }
        let mut termios = unsafe { std::mem::zeroed::<libc::termios>() };
        // SAFETY: termios points to writable storage and the master is live.
        if unsafe { libc::tcgetattr(self.fd.get_ref().as_raw_fd(), &mut termios) } == -1 {
            return Err(io::Error::last_os_error());
        }
        if termios.c_lflag & libc::ICANON == 0 {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "PTY stdin EOF requires canonical terminal mode",
            ));
        }
        let eof = termios.c_cc[libc::VEOF];
        if eof == 0 || eof == libc::_POSIX_VDISABLE as libc::cc_t {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "PTY canonical EOF character is disabled",
            ));
        }
        // In canonical mode, VEOF first submits any unterminated buffered
        // bytes; a second VEOF on the now-empty line produces the zero-length
        // read that consumers observe as EOF.
        write_bytes(&self.fd, &[eof, eof]).await?;
        input.closed = true;
        Ok(())
    }

    pub(super) fn resize(&self, cols: u16, rows: u16) -> io::Result<()> {
        if cols == 0 || rows == 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "PTY dimensions must be non-zero",
            ));
        }
        let _gate = self
            .resize_gate
            .lock()
            .map_err(|_| io::Error::other("PTY resize gate is poisoned"))?;
        let size = libc::winsize {
            ws_row: rows,
            ws_col: cols,
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        // SAFETY: size is initialized and the PTY master remains live.
        if unsafe {
            libc::ioctl(
                self.fd.get_ref().as_raw_fd(),
                libc::TIOCSWINSZ as _,
                &size,
            )
        } == -1
        {
            Err(io::Error::last_os_error())
        } else {
            Ok(())
        }
    }

    async fn read(&self, buffer: &mut [u8]) -> io::Result<usize> {
        loop {
            let mut ready = self.fd.readable().await?;
            match ready.try_io(|fd| read_once(fd.get_ref().as_raw_fd(), buffer)) {
                Ok(Ok(read)) => return Ok(read),
                Ok(Err(error)) if error.raw_os_error() == Some(libc::EIO) => return Ok(0),
                Ok(Err(error)) => return Err(error),
                Err(_) => continue,
            }
        }
    }
}

fn duplicate_stdio(fd: RawFd) -> io::Result<Stdio> {
    // SAFETY: F_DUPFD_CLOEXEC creates a distinct owned descriptor.
    let duplicated = unsafe { libc::fcntl(fd, libc::F_DUPFD_CLOEXEC, 3) };
    if duplicated < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: fcntl returned a fresh owned descriptor.
    Ok(Stdio::from(unsafe { OwnedFd::from_raw_fd(duplicated) }))
}

pub(super) async fn read_output(
    master: Arc<AsyncPtyMaster>,
    output: Arc<OutputBuffer>,
) -> io::Result<()> {
    let mut buffer = [0_u8; READ_BUFFER_BYTES];
    loop {
        let read = master.read(&mut buffer).await?;
        if read == 0 {
            return Ok(());
        }
        output.push(OutputStream::Pty, &buffer[..read]);
    }
}

async fn write_bytes(fd: &AsyncFd<OwnedFd>, mut bytes: &[u8]) -> io::Result<()> {
    while !bytes.is_empty() {
        let mut ready = fd.writable().await?;
        match ready.try_io(|fd| write_once(fd.get_ref().as_raw_fd(), bytes)) {
            Ok(Ok(written)) => bytes = &bytes[written..],
            Ok(Err(error)) => return Err(error),
            Err(_) => continue,
        }
    }
    Ok(())
}

fn read_once(fd: RawFd, buffer: &mut [u8]) -> io::Result<usize> {
    // SAFETY: buffer is writable for its full length and fd is live.
    let read = unsafe { libc::read(fd, buffer.as_mut_ptr().cast(), buffer.len()) };
    if read >= 0 {
        Ok(read as usize)
    } else {
        Err(io::Error::last_os_error())
    }
}

fn write_once(fd: RawFd, bytes: &[u8]) -> io::Result<usize> {
    // SAFETY: bytes is readable for its full length and fd is live.
    let written = unsafe { libc::write(fd, bytes.as_ptr().cast(), bytes.len()) };
    if written > 0 {
        Ok(written as usize)
    } else if written == 0 {
        Err(io::Error::new(
            io::ErrorKind::WriteZero,
            "PTY master accepted zero input bytes",
        ))
    } else {
        Err(io::Error::last_os_error())
    }
}

fn set_cloexec(fd: RawFd) -> io::Result<()> {
    // SAFETY: fd is live for the duration of these fcntl operations.
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
    if flags == -1
        || unsafe { libc::fcntl(fd, libc::F_SETFD, flags | libc::FD_CLOEXEC) } == -1
    {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

fn set_nonblocking(fd: RawFd) -> io::Result<()> {
    // SAFETY: fd is live for the duration of these fcntl operations.
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags == -1
        || unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } == -1
    {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

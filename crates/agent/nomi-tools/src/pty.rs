//! Lightweight PTY wrapper for the agent layer's interactive terminal tools.
//!
//! Ported (and de-dependency-ed) from `nomifun-terminal::pty::PtyHandle`. The
//! backend crate drags in db/auth/realtime/knowledge, so the agent layer must
//! not depend on it; this is a ~120-line reimplementation that keeps only the
//! parts `exec_command` / `write_stdin` need:
//!
//! - openpty + spawn a child in the PTY,
//! - a reader thread that fans output out over a broadcast channel,
//! - a waiter thread that is the **single source of truth for exit** (the
//!   reader's EOF must NOT be used for exit: Windows ConPTY masters never EOF
//!   when the child dies),
//! - `write` (stdin), `kill` (process-group SIGKILL on Unix).
//!
//! Differences from the original `PtyHandle`: broadcast is the only output
//! channel (no scrollback / reconnect — MVP doesn't reconnect), and exit state
//! is exposed via atomics rather than callbacks so the collection loop can poll
//! it without capturing closures across threads.

use std::collections::HashMap;
use std::io::{Read, Write};
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use portable_pty::{ChildKiller, CommandBuilder, MasterPty, PtySize, native_pty_system};
use tokio::sync::{Notify, broadcast};

/// Bounded fan-out buffer for the live output stream (in chunks). A lagging
/// subscriber drops oldest chunks rather than stalling the reader thread; the
/// collection loop tolerates `Lagged` by continuing.
const OUTPUT_BROADCAST_CAP: usize = 512;
/// Durable byte backlog for output produced while no tool call is actively
/// subscribed. This prevents short-lived commands and background output between
/// `exec_command`/`write_stdin` calls from disappearing.
const OUTPUT_BACKLOG_MAX_BYTES: usize = 4 * 1024 * 1024;

/// Grace period after the child exits before the exit code is published, so the
/// reader thread can drain output still buffered in the PTY (notably on Windows,
/// where the ConPTY master does not reach EOF on child exit).
const EXIT_DRAIN_GRACE: Duration = Duration::from_millis(120);

/// Sentinel for "exit code not yet known / unavailable".
const EXIT_UNKNOWN: i32 = i32::MIN;

#[derive(Default)]
struct OutputBacklog {
    base_offset: usize,
    bytes: Vec<u8>,
}

impl OutputBacklog {
    fn push(&mut self, chunk: &[u8]) {
        self.bytes.extend_from_slice(chunk);
        if self.bytes.len() > OUTPUT_BACKLOG_MAX_BYTES {
            let drain = self.bytes.len() - OUTPUT_BACKLOG_MAX_BYTES;
            self.bytes.drain(..drain);
            self.base_offset = self.base_offset.saturating_add(drain);
        }
    }

    fn snapshot_from(&self, offset: usize) -> (Vec<u8>, usize) {
        let start = offset.max(self.base_offset);
        let rel = start.saturating_sub(self.base_offset).min(self.bytes.len());
        (
            self.bytes[rel..].to_vec(),
            self.base_offset.saturating_add(self.bytes.len()),
        )
    }
}

/// Parameters for spawning a PTY-backed child.
pub struct PtyParams {
    pub program: String,
    pub args: Vec<String>,
    pub cwd: String,
    pub env: HashMap<String, String>,
    pub cols: u16,
    pub rows: u16,
}

/// A live PTY session: master writer + a killer split off the child, plus the
/// output fan-out and exit/close state shared with the reader/waiter threads.
pub struct Pty {
    /// The PTY master, retained for the life of the session. **Must not be
    /// dropped while the child is alive**: on Windows, releasing the last
    /// `MasterPty` handle closes the ConPTY, which makes a freshly-spawned child
    /// (notably `cmd.exe`) abort during init with `STATUS_DLL_INIT_FAILED`
    /// (0xC0000142) and produce no output. We keep no resize API in this MVP, so
    /// the master is otherwise inert — but it has to stay alive.
    _master: Mutex<Box<dyn MasterPty + Send>>,
    writer: Mutex<Box<dyn Write + Send>>,
    /// A killer split from the child (via `clone_killer`) so `kill()` can signal
    /// the process while the waiter thread is parked in the blocking `wait()`.
    killer: Mutex<Box<dyn ChildKiller + Send + Sync>>,
    out_tx: broadcast::Sender<Vec<u8>>,
    backlog: Arc<Mutex<OutputBacklog>>,
    /// Child has exited (set by the waiter thread — the only source of truth).
    exited: Arc<AtomicBool>,
    /// Child exit code, or `EXIT_UNKNOWN`. Set by the waiter thread.
    exit_code: Arc<AtomicI32>,
    /// Reader reached EOF (output stream closed). Set by the reader thread.
    closed: Arc<AtomicBool>,
    /// Notifies collection loops when the output stream closes so they can wake
    /// and re-evaluate the `exited && closed` finish condition.
    closed_notify: Arc<Notify>,
    /// Direct child pid. The child is its own process-group leader (portable-pty
    /// calls `setsid()` on the slave), so this is also the process-group id used
    /// to kill the whole tree.
    pid: Option<u32>,
}

impl Pty {
    /// Spawn a child inside a fresh PTY. Returns immediately; output flows over
    /// the broadcast channel and exit is recorded by the waiter thread.
    pub fn spawn(p: PtyParams) -> Result<Arc<Self>, String> {
        let pair = native_pty_system()
            .openpty(PtySize {
                rows: p.rows,
                cols: p.cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(|e| format!("openpty: {e}"))?;

        let mut cmd = CommandBuilder::new(&p.program);
        for a in &p.args {
            cmd.arg(a);
        }
        if !p.cwd.is_empty() {
            cmd.cwd(&p.cwd);
        }
        for (k, v) in &p.env {
            cmd.env(k, v);
        }

        let child = pair
            .slave
            .spawn_command(cmd)
            .map_err(|e| format!("spawn '{}': {e}", p.program))?;
        // Drop the slave so the master sees EOF when the child exits (Unix).
        drop(pair.slave);

        let writer = pair
            .master
            .take_writer()
            .map_err(|e| format!("take_writer: {e}"))?;
        let mut reader = pair
            .master
            .try_clone_reader()
            .map_err(|e| format!("clone_reader: {e}"))?;
        // Retain the master for the life of the session (see the field docs):
        // dropping it on Windows closes the ConPTY and aborts the child's init.
        // The writer and reader were already split off above.
        let master = pair.master;

        let pid = child.process_id();
        let killer = child.clone_killer();
        let (out_tx, _) = broadcast::channel::<Vec<u8>>(OUTPUT_BROADCAST_CAP);
        let backlog = Arc::new(Mutex::new(OutputBacklog::default()));

        let exited = Arc::new(AtomicBool::new(false));
        let exit_code = Arc::new(AtomicI32::new(EXIT_UNKNOWN));
        let closed = Arc::new(AtomicBool::new(false));
        let closed_notify = Arc::new(Notify::new());

        let handle = Arc::new(Pty {
            _master: Mutex::new(master),
            writer: Mutex::new(writer),
            killer: Mutex::new(killer),
            out_tx: out_tx.clone(),
            backlog: backlog.clone(),
            exited: exited.clone(),
            exit_code: exit_code.clone(),
            closed: closed.clone(),
            closed_notify: closed_notify.clone(),
            pid,
        });

        // Reader thread: stream PTY output. On Windows the ConPTY master does
        // NOT reach EOF when the child exits, so this loop can outlive the child;
        // it ends on EOF (Unix / master dropped) or read error. Exit is reported
        // by the waiter thread below, NOT by this loop's EOF.
        let closed_r = closed.clone();
        let closed_notify_r = closed_notify.clone();
        let backlog_r = backlog.clone();
        std::thread::spawn(move || {
            let mut buf = [0u8; 8192];
            loop {
                match reader.read(&mut buf) {
                    Ok(0) => break, // EOF (Unix, or master dropped)
                    Ok(n) => {
                        // Err just means no live receivers — harmless.
                        let chunk = buf[..n].to_vec();
                        if let Ok(mut backlog) = backlog_r.lock() {
                            backlog.push(&chunk);
                            let _ = out_tx.send(chunk);
                        } else {
                            let _ = out_tx.send(chunk);
                        }
                    }
                    Err(_) => break,
                }
            }
            closed_r.store(true, Ordering::Release);
            closed_notify_r.notify_waiters();
        });

        // Waiter thread: block directly on the child and record its exit exactly
        // once. This is the source of truth for exit — relying on the reader's
        // EOF would never fire on Windows ConPTY (the master stays open after the
        // child dies).
        std::thread::spawn(move || {
            let mut child = child;
            let code = child
                .wait()
                .ok()
                .map(|status| status.exit_code() as i32)
                .unwrap_or(EXIT_UNKNOWN);
            // Brief grace so the reader can drain output still buffered in the
            // PTY before consumers tear the session down on this signal.
            std::thread::sleep(EXIT_DRAIN_GRACE);
            exit_code.store(code, Ordering::Release);
            exited.store(true, Ordering::Release);
            closed_notify.notify_waiters();
        });

        Ok(handle)
    }

    /// Write bytes to the PTY (the child's stdin).
    pub fn write(&self, bytes: &[u8]) -> Result<(), String> {
        let mut w = self.writer.lock().map_err(|_| "pty writer poisoned")?;
        w.write_all(bytes).map_err(|e| e.to_string())?;
        w.flush().map_err(|e| e.to_string())
    }

    /// Subscribe to the live output byte-stream. Each PTY chunk is delivered as a
    /// `Vec<u8>`; a lagging receiver drops oldest chunks. Subscribe **before**
    /// writing/spawning-dependent reads to avoid missing the echo.
    pub fn subscribe(&self) -> broadcast::Receiver<Vec<u8>> {
        self.out_tx.subscribe()
    }

    /// Subscribe to live output and atomically snapshot bytes emitted since
    /// `offset`. Returns `(receiver, already_buffered_bytes, next_offset)`.
    pub fn subscribe_from(&self, offset: usize) -> (broadcast::Receiver<Vec<u8>>, Vec<u8>, usize) {
        match self.backlog.lock() {
            Ok(backlog) => {
                let rx = self.out_tx.subscribe();
                let (snapshot, next_offset) = backlog.snapshot_from(offset);
                (rx, snapshot, next_offset)
            }
            Err(_) => (self.out_tx.subscribe(), Vec::new(), offset),
        }
    }

    /// Snapshot bytes emitted since `offset` without subscribing.
    pub fn snapshot_from(&self, offset: usize) -> (Vec<u8>, usize) {
        match self.backlog.lock() {
            Ok(backlog) => backlog.snapshot_from(offset),
            Err(_) => (Vec::new(), offset),
        }
    }

    /// Whether the child has exited (waiter thread is the source of truth).
    pub fn has_exited(&self) -> bool {
        self.exited.load(Ordering::Acquire)
    }

    /// The child's exit code, if the waiter thread has recorded it.
    pub fn exit_code(&self) -> Option<i32> {
        let c = self.exit_code.load(Ordering::Acquire);
        if c == EXIT_UNKNOWN { None } else { Some(c) }
    }

    /// Whether the output stream has closed (reader hit EOF).
    pub fn output_closed(&self) -> bool {
        self.closed.load(Ordering::Acquire)
    }

    /// A handle to the notifier that fires when output closes or the child exits.
    pub fn closed_notify(&self) -> Arc<Notify> {
        self.closed_notify.clone()
    }

    /// The direct child pid (also the process-group id on Unix).
    pub fn pid(&self) -> Option<u32> {
        self.pid
    }

    /// Terminate the child process **and its descendants**.
    ///
    /// `portable-pty`'s `Child::kill()` only signals the direct child pid, which
    /// can leave grandchildren alive. The child is its own process-group leader
    /// (the slave is spawned with `setsid()`), so on Unix we additionally SIGKILL
    /// the whole process group via the negative pid to reap the entire tree. The
    /// split killer works even while the waiter thread is blocked in `wait()`.
    pub fn kill(&self) {
        #[cfg(unix)]
        if let Some(pid) = self.pid {
            // Negative pid → signal the process group led by `pid`.
            unsafe {
                libc::kill(-(pid as i32), libc::SIGKILL);
            }
        }
        if let Ok(mut killer) = self.killer.lock() {
            let _ = killer.kill();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::pty_test_helper_program;
    use std::time::Instant;

    fn wait_for(deadline_ms: u64, mut cond: impl FnMut() -> bool) -> bool {
        let start = Instant::now();
        while start.elapsed() < Duration::from_millis(deadline_ms) {
            if cond() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        cond()
    }

    #[test]
    fn exit_fires_when_child_exits_on_its_own() {
        // The waiter thread (not reader EOF) must record exit.
        #[cfg(windows)]
        let (program, args) = (
            std::env::var("ComSpec").unwrap_or_else(|_| "C:\\Windows\\System32\\cmd.exe".into()),
            vec!["/c".to_owned(), "exit".to_owned(), "0".to_owned()],
        );
        #[cfg(not(windows))]
        let (program, args) = ("sh".to_owned(), vec!["-c".to_owned(), "exit 0".to_owned()]);

        let pty = Pty::spawn(PtyParams {
            program,
            args,
            cwd: String::new(),
            env: HashMap::new(),
            cols: 80,
            rows: 24,
        })
        .expect("spawn");

        assert!(
            wait_for(5000, || pty.has_exited()),
            "waiter thread must record exit when the child exits on its own"
        );
        assert_eq!(pty.exit_code(), Some(0));
    }

    #[cfg(unix)]
    #[test]
    fn kill_terminates_process_group() {
        // Long-lived child; the cross-platform helper sleeps instead of `sleep`.
        let pty = Pty::spawn(PtyParams {
            program: pty_test_helper_program(),
            args: vec!["sleep".into(), "60000".into()],
            cwd: String::new(),
            env: HashMap::new(),
            cols: 80,
            rows: 24,
        })
        .expect("spawn helper sleep");

        let pid = pty.pid().expect("pid") as i32;
        // Existence probe (signal 0).
        assert_eq!(unsafe { libc::kill(pid, 0) }, 0, "child should be alive");

        pty.kill();

        assert!(
            wait_for(5000, || pty.has_exited()),
            "kill() should terminate the child and the waiter should record exit"
        );
    }

    #[test]
    fn write_then_read_echo() {
        // The helper's `echo-stdin` echoes each stdin line back on the PTY
        // (cross-platform stand-in for `cat`). Subscribe before writing.
        let pty = Pty::spawn(PtyParams {
            program: pty_test_helper_program(),
            args: vec!["echo-stdin".into()],
            cwd: String::new(),
            env: HashMap::new(),
            cols: 80,
            rows: 24,
        })
        .expect("spawn helper echo-stdin");

        let mut rx = pty.subscribe();
        pty.write(b"hello_pty\n").expect("write");

        // Drain whatever arrives within a generous window.
        let mut got = Vec::new();
        let start = Instant::now();
        while start.elapsed() < Duration::from_millis(2000) {
            match rx.try_recv() {
                Ok(chunk) => got.extend_from_slice(&chunk),
                Err(broadcast::error::TryRecvError::Empty) => {
                    if String::from_utf8_lossy(&got).contains("hello_pty") {
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(20));
                }
                Err(broadcast::error::TryRecvError::Lagged(_)) => continue,
                Err(broadcast::error::TryRecvError::Closed) => break,
            }
        }
        assert!(
            String::from_utf8_lossy(&got).contains("hello_pty"),
            "echo-stdin should echo back stdin, got: {:?}",
            String::from_utf8_lossy(&got)
        );
        pty.kill();
    }
}

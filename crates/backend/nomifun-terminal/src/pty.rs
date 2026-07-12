//! Low-level PTY wrapper: spawns a child in a pseudo-terminal, streams its
//! output through a callback, and keeps a bounded scrollback buffer for
//! reconnect. Built on `portable-pty` (cross-platform: macOS/Linux + Windows
//! ConPTY).

use std::collections::HashMap;
use std::io::{Read, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use portable_pty::{ChildKiller, CommandBuilder, MasterPty, PtySize, native_pty_system};
use tokio::sync::broadcast;

use crate::error::TerminalError;

/// Max bytes retained for reconnect scrollback (~256 KB).
const SCROLLBACK_CAP: usize = 256 * 1024;

/// Bounded fan-out buffer for the live output stream (in chunks). A lagging
/// subscriber (e.g. a slow AutoWork watcher) drops oldest chunks rather than
/// stalling the reader; AutoWork tolerates this (it scans for a marker/quiescence
/// on whatever it receives). The WebSocket path is unaffected — it is driven by
/// the `on_output` callback, not this channel.
const OUTPUT_BROADCAST_CAP: usize = 512;

/// Grace period after a child exits before the exit is reported, so the reader
/// thread can drain output still buffered in the PTY (notably on Windows, where
/// the ConPTY master does not reach EOF on child exit).
const EXIT_DRAIN_GRACE: Duration = Duration::from_millis(120);

/// A live PTY: master handle + writer + child + scrollback.
pub struct PtyHandle {
    master: Mutex<Box<dyn MasterPty + Send>>,
    writer: Mutex<Box<dyn Write + Send>>,
    /// A killer split from the child (via `clone_killer`) so `kill()` can signal
    /// the process while the waiter thread is parked in the blocking `wait()`.
    killer: Mutex<Box<dyn ChildKiller + Send + Sync>>,
    scrollback: Arc<Mutex<Vec<u8>>>,
    /// Set whenever new bytes land in `scrollback`, cleared by
    /// [`take_dirty_scrollback`]. Lets the debounced persistence flusher skip
    /// idle sessions instead of rewriting an unchanged 256 KB buffer every tick.
    dirty: Arc<AtomicBool>,
    /// Live output fan-out. Each PTY chunk is published here in addition to the
    /// scrollback + `on_output` callback, so in-process consumers (AutoWork) can
    /// observe the stream without touching the WebSocket path.
    out_tx: broadcast::Sender<Vec<u8>>,
    /// Direct child pid. The child is its own session/process-group leader
    /// (portable-pty calls `setsid()` on the slave), so this is also the
    /// process-group id used to kill the whole tree.
    pid: Option<u32>,
    /// Monotonic spawn generation, assigned by the service. The exit callback
    /// only tears the session down if this epoch is still the live one for the
    /// id — a relaunch kills the old child then immediately spawns a
    /// higher-epoch replacement, so the killed predecessor's (drain-grace-
    /// delayed) exit callback becomes a no-op instead of closing the fresh PTY.
    epoch: u64,
}

/// Parameters for spawning a PTY.
pub struct SpawnParams {
    pub program: String,
    pub args: Vec<String>,
    pub cwd: String,
    pub env: HashMap<String, String>,
    pub cols: u16,
    pub rows: u16,
}

/// Remove inherited locale variables that have higher POSIX precedence than a
/// locale override supplied for this PTY session. `CommandBuilder` snapshots
/// the parent environment at construction, so merely adding `LC_CTYPE` or
/// `LANG` does not make it effective when the parent has `LC_ALL`/`LC_CTYPE`.
#[cfg(unix)]
fn remove_inherited_locale_vars_shadowing_overrides(
    cmd: &mut CommandBuilder,
    env: &HashMap<String, String>,
) {
    if env.contains_key("LC_ALL") {
        // The session supplies the highest-precedence category itself.
        return;
    }
    if env.contains_key("LC_CTYPE") {
        cmd.env_remove("LC_ALL");
    } else if env.contains_key("LANG") {
        cmd.env_remove("LC_ALL");
        cmd.env_remove("LC_CTYPE");
    }
}

impl PtyHandle {
    /// Spawn a child in a new PTY.
    ///
    /// `on_output` is invoked (on a blocking reader thread) for every chunk of
    /// bytes read from the PTY. `on_exit` is invoked once when the child exits,
    /// with the child's exit code (if available) and a final snapshot of the
    /// scrollback (taken after the drain grace, so it includes the tail) — the
    /// caller persists this so the output survives the process even between
    /// debounced flushes. `epoch` is the service-assigned spawn generation
    /// stored on the handle (see the field docs); the caller uses it to ignore
    /// a stale exit callback after a relaunch.
    pub fn spawn<FOut, FExit>(
        params: SpawnParams,
        epoch: u64,
        on_output: FOut,
        on_exit: FExit,
    ) -> Result<Arc<Self>, TerminalError>
    where
        FOut: Fn(Vec<u8>) + Send + 'static,
        FExit: FnOnce(Option<i32>, Vec<u8>) + Send + 'static,
    {
        let pty_system = native_pty_system();
        let pair = pty_system
            .openpty(PtySize {
                rows: params.rows,
                cols: params.cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(|e| TerminalError::Spawn(format!("openpty: {e}")))?;

        let mut cmd = CommandBuilder::new(&params.program);
        for arg in &params.args {
            cmd.arg(arg);
        }
        if !params.cwd.is_empty() {
            cmd.cwd(&params.cwd);
        }
        #[cfg(unix)]
        remove_inherited_locale_vars_shadowing_overrides(&mut cmd, &params.env);
        for (k, v) in &params.env {
            cmd.env(k, v);
        }

        let child = pair
            .slave
            .spawn_command(cmd)
            .map_err(|e| TerminalError::Spawn(format!("spawn '{}': {e}", params.program)))?;
        // Drop the slave so the master sees EOF when the child exits.
        drop(pair.slave);

        let writer = pair
            .master
            .take_writer()
            .map_err(|e| TerminalError::Spawn(format!("take_writer: {e}")))?;
        let mut reader = pair
            .master
            .try_clone_reader()
            .map_err(|e| TerminalError::Spawn(format!("clone_reader: {e}")))?;

        let scrollback = Arc::new(Mutex::new(Vec::<u8>::new()));
        let dirty = Arc::new(AtomicBool::new(false));
        let pid = child.process_id();
        let (out_tx, _) = broadcast::channel::<Vec<u8>>(OUTPUT_BROADCAST_CAP);
        // Split a killer off the child so `kill()` can signal the process while
        // the waiter thread below is parked in the blocking `child.wait()`.
        let killer = child.clone_killer();

        let handle = Arc::new(PtyHandle {
            master: Mutex::new(pair.master),
            writer: Mutex::new(writer),
            killer: Mutex::new(killer),
            scrollback: scrollback.clone(),
            dirty: dirty.clone(),
            out_tx: out_tx.clone(),
            pid,
            epoch,
        });

        // Reader thread: stream PTY output (reads are synchronous). On Windows
        // the ConPTY master does NOT reach EOF when the child exits, so this
        // loop can outlive the child; it ends when the master is dropped (the
        // PtyHandle is released) or the read errors. Exit is reported by the
        // separate waiter thread below, NOT by this loop's EOF.
        let scrollback_reader = scrollback.clone();
        let dirty_reader = dirty.clone();
        std::thread::spawn(move || {
            let mut buf = [0u8; 8192];
            loop {
                match reader.read(&mut buf) {
                    Ok(0) => break, // EOF (Unix, or master dropped)
                    Ok(n) => {
                        let chunk = buf[..n].to_vec();
                        append_scrollback(&scrollback_reader, &chunk);
                        dirty_reader.store(true, Ordering::Relaxed);
                        // Fan out to in-process subscribers (AutoWork). Err just
                        // means no live receivers — harmless.
                        let _ = out_tx.send(chunk.clone());
                        on_output(chunk);
                    }
                    Err(_) => break,
                }
            }
        });

        // Waiter thread: block directly on the child and report its exit exactly
        // once. This is the source of truth for exit — relying on the reader's
        // EOF would never fire on Windows ConPTY (the master stays open after
        // the child dies).
        let scrollback_waiter = scrollback.clone();
        std::thread::spawn(move || {
            let mut child = child;
            let code = child.wait().ok().map(|status| status.exit_code() as i32);
            // Brief grace so the reader can drain output still buffered in the
            // PTY before the caller tears the session down on this signal.
            std::thread::sleep(EXIT_DRAIN_GRACE);
            // Snapshot AFTER the grace so the persisted final scrollback includes
            // the tail the reader just drained.
            let final_scrollback = scrollback_waiter.lock().expect("scrollback lock").clone();
            on_exit(code, final_scrollback);
        });

        Ok(handle)
    }

    /// Write bytes to the PTY (the child's stdin).
    pub fn write(&self, bytes: &[u8]) -> Result<(), TerminalError> {
        let mut writer = self.writer.lock().expect("pty writer lock");
        writer.write_all(bytes)?;
        writer.flush()?;
        Ok(())
    }

    /// Resize the PTY window.
    pub fn resize(&self, cols: u16, rows: u16) -> Result<(), TerminalError> {
        self.master
            .lock()
            .expect("pty master lock")
            .resize(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(|e| TerminalError::Spawn(format!("resize: {e}")))
    }

    /// Terminate the child process **and its descendants**.
    ///
    /// `portable-pty`'s `Child::kill()` only signals the direct child pid, which
    /// can leave grandchildren (e.g. a `claude`/`vim` launched by the shell)
    /// alive. The child is its own process-group leader (the slave is spawned
    /// with `setsid()`), so on Unix we additionally SIGKILL the whole process
    /// group via the negative pid to reap the entire tree.
    pub fn kill(&self) -> Result<(), TerminalError> {
        #[cfg(unix)]
        if let Some(pid) = self.pid {
            // Negative pid → signal the process group led by `pid`.
            unsafe {
                libc::kill(-(pid as i32), libc::SIGKILL);
            }
        }
        // Best-effort direct kill too (covers the non-group / Windows path).
        // Uses the split killer, which works even while the waiter thread is
        // blocked in `child.wait()`.
        let _ = self.killer.lock().expect("pty killer lock").kill();
        Ok(())
    }

    /// Snapshot of the current scrollback bytes (for reconnect).
    pub fn scrollback(&self) -> Vec<u8> {
        self.scrollback.lock().expect("scrollback lock").clone()
    }

    /// If new output has landed since the last call (or spawn), clear the dirty
    /// flag and return a snapshot to persist; otherwise return `None`. Used by
    /// the debounced flusher so an idle session is never rewritten.
    ///
    /// Note the flag is cleared *before* the snapshot is read. A chunk arriving
    /// in that window re-sets the flag, so it is caught next tick — at worst the
    /// snapshot already includes it (a harmless redundant write next tick),
    /// never a lost update.
    pub fn take_dirty_scrollback(&self) -> Option<Vec<u8>> {
        if self.dirty.swap(false, Ordering::Relaxed) {
            Some(self.scrollback())
        } else {
            None
        }
    }

    /// Subscribe to the live output byte-stream (in-process fan-out). Each PTY
    /// chunk is delivered as a `Vec<u8>`; a lagging receiver drops oldest chunks.
    pub fn subscribe_output(&self) -> broadcast::Receiver<Vec<u8>> {
        self.out_tx.subscribe()
    }

    /// The direct child pid (also the process-group id).
    pub fn pid(&self) -> Option<u32> {
        self.pid
    }

    /// The service-assigned spawn generation (see the field docs). Used by the
    /// service to ignore a stale exit callback from a relaunched-over PTY.
    pub fn epoch(&self) -> u64 {
        self.epoch
    }
}

fn append_scrollback(scrollback: &Arc<Mutex<Vec<u8>>>, chunk: &[u8]) {
    let mut sb = scrollback.lock().expect("scrollback lock");
    sb.extend_from_slice(chunk);
    if sb.len() > SCROLLBACK_CAP {
        let overflow = sb.len() - SCROLLBACK_CAP;
        sb.drain(0..overflow);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn explicit_lc_ctype_removes_inherited_lc_all() {
        let mut cmd = CommandBuilder::new("sh");
        cmd.env("LC_ALL", "C");
        let env = HashMap::from([("LC_CTYPE".to_owned(), "UTF-8".to_owned())]);

        remove_inherited_locale_vars_shadowing_overrides(&mut cmd, &env);

        assert!(cmd.get_env("LC_ALL").is_none());
    }

    #[cfg(unix)]
    #[test]
    fn explicit_lang_removes_inherited_character_locale_overrides() {
        let mut cmd = CommandBuilder::new("sh");
        cmd.env("LC_ALL", "C");
        cmd.env("LC_CTYPE", "zh_CN.GB18030");
        let env = HashMap::from([("LANG".to_owned(), "en_US.UTF-8".to_owned())]);

        remove_inherited_locale_vars_shadowing_overrides(&mut cmd, &env);

        assert!(cmd.get_env("LC_ALL").is_none());
        assert!(cmd.get_env("LC_CTYPE").is_none());
    }

    #[cfg(unix)]
    #[test]
    fn explicit_lc_all_keeps_highest_precedence_override() {
        let mut cmd = CommandBuilder::new("sh");
        cmd.env("LC_ALL", "C");
        let env = HashMap::from([("LC_ALL".to_owned(), "en_US.UTF-8".to_owned())]);

        remove_inherited_locale_vars_shadowing_overrides(&mut cmd, &env);
        for (key, value) in &env {
            cmd.env(key, value);
        }

        assert_eq!(
            cmd.get_env("LC_ALL"),
            Some(std::ffi::OsStr::new("en_US.UTF-8"))
        );
    }

    #[test]
    fn scrollback_is_bounded() {
        let sb = Arc::new(Mutex::new(Vec::<u8>::new()));
        let big = vec![b'x'; SCROLLBACK_CAP + 5000];
        append_scrollback(&sb, &big);
        assert_eq!(sb.lock().unwrap().len(), SCROLLBACK_CAP);
    }

    #[test]
    fn scrollback_keeps_most_recent_bytes() {
        let sb = Arc::new(Mutex::new(Vec::<u8>::new()));
        append_scrollback(&sb, &vec![b'a'; SCROLLBACK_CAP]);
        append_scrollback(&sb, b"TAIL");
        let data = sb.lock().unwrap();
        assert_eq!(data.len(), SCROLLBACK_CAP);
        assert_eq!(&data[data.len() - 4..], b"TAIL");
    }

    #[test]
    fn dirty_flag_set_by_output_then_cleared_by_take() {
        use std::sync::atomic::{AtomicBool, Ordering};

        // A child that emits known output then exits on its own.
        #[cfg(windows)]
        let (program, args) = (
            std::env::var("ComSpec").unwrap_or_else(|_| "C:\\Windows\\System32\\cmd.exe".into()),
            vec!["/c".to_owned(), "echo".to_owned(), "hello".to_owned()],
        );
        #[cfg(not(windows))]
        let (program, args) = ("sh".to_owned(), vec!["-c".to_owned(), "printf hello".to_owned()]);

        let exited = Arc::new(AtomicBool::new(false));
        let exited_cb = exited.clone();
        let handle = PtyHandle::spawn(
            SpawnParams {
                program,
                args,
                cwd: String::new(),
                env: std::collections::HashMap::new(),
                cols: 80,
                rows: 24,
            },
            0,
            |_chunk| {},
            move |_code, _sb| exited_cb.store(true, Ordering::SeqCst),
        )
        .expect("spawn");

        // Wait for exit (on_exit fires after the reader has drained final bytes).
        for _ in 0..250 {
            if exited.load(Ordering::SeqCst) {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        // Small settle so the reader thread has appended the last chunk.
        std::thread::sleep(std::time::Duration::from_millis(50));

        // Output landed → dirty → first take returns the snapshot.
        let snap = handle.take_dirty_scrollback().expect("dirty after output");
        assert!(
            String::from_utf8_lossy(&snap).contains("hello"),
            "snapshot should contain the emitted output, got {:?}",
            String::from_utf8_lossy(&snap)
        );
        // No new output since the take → second take is None (idle, skip write).
        assert!(
            handle.take_dirty_scrollback().is_none(),
            "a session with no new output must not be re-flushed"
        );
    }

    #[test]
    fn exit_fires_when_child_exits_on_its_own() {
        use std::sync::atomic::{AtomicBool, Ordering};

        // A child that exits immediately on its own (no kill). On Windows the
        // ConPTY master never EOFs on exit, so a reader-EOF-gated design never
        // fires on_exit here; the dedicated waiter thread must.
        #[cfg(windows)]
        let (program, args) = (
            std::env::var("ComSpec").unwrap_or_else(|_| "C:\\Windows\\System32\\cmd.exe".into()),
            vec!["/c".to_owned(), "exit".to_owned(), "0".to_owned()],
        );
        #[cfg(not(windows))]
        let (program, args) = ("sh".to_owned(), vec!["-c".to_owned(), "exit 0".to_owned()]);

        let exited = Arc::new(AtomicBool::new(false));
        let exited_cb = exited.clone();
        let _handle = PtyHandle::spawn(
            SpawnParams {
                program,
                args,
                cwd: String::new(),
                env: std::collections::HashMap::new(),
                cols: 80,
                rows: 24,
            },
            0,
            |_chunk| {},
            move |_code, _sb| exited_cb.store(true, Ordering::SeqCst),
        )
        .expect("spawn");

        let mut fired = false;
        for _ in 0..250 {
            if exited.load(Ordering::SeqCst) {
                fired = true;
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        assert!(fired, "on_exit must fire when the child exits on its own");
    }

    #[cfg(unix)]
    #[test]
    fn kill_terminates_the_process() {
        use std::sync::atomic::{AtomicBool, Ordering};
        let exited = Arc::new(AtomicBool::new(false));
        let exited_cb = exited.clone();
        // A long-lived child: sleep 60s. kill() must terminate it promptly.
        let handle = PtyHandle::spawn(
            SpawnParams {
                program: "sleep".into(),
                args: vec!["60".into()],
                cwd: String::new(),
                env: std::collections::HashMap::new(),
                cols: 80,
                rows: 24,
            },
            0,
            |_chunk| {},
            move |_code, _sb| exited_cb.store(true, Ordering::SeqCst),
        )
        .expect("spawn sleep");

        let pid = handle.pid().expect("pid") as i32;
        // Process exists right after spawn (signal 0 = existence probe).
        assert_eq!(unsafe { libc::kill(pid, 0) }, 0, "child should be alive");

        handle.kill().expect("kill");

        // Within a short window the reader hits EOF and on_exit fires.
        let mut gone = false;
        for _ in 0..200 {
            if exited.load(Ordering::SeqCst) {
                gone = true;
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        assert!(gone, "kill() should terminate the child and trigger on_exit");
    }
}

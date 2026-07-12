//! `PersistentShell`: a single long-lived shell process in a PTY whose working
//! directory and environment persist across sequential commands — the
//! difference between `BashTool`'s stateless one-shot (`cd foo` forgotten on the
//! next call) and a real interactive session.
//!
//! # Completion protocol (controlled sentinel)
//!
//! After each submitted command the shell is told to print a unique sentinel
//! line carrying the command's exit status:
//!
//! ```text
//! <command>
//! printf '__NOMI_END_<nonce>__%d__\n' "$?"
//! ```
//!
//! We then read PTY output until that exact `__NOMI_END_<nonce>__<rc>__` line
//! appears; everything before it is the command's output and `<rc>` is its exit
//! code. Input echo is disabled (`stty -echo`) and the prompt is blanked at init
//! so the captured output contains only the command's own stdout/stderr.
//!
//! This is **not** the unreliable "scrape markers out of an interactive TUI's
//! redrawing output" mechanism that was removed from terminal AutoWork: the
//! shell is a line-oriented program whose command line we fully control, and the
//! sentinel is emitted by a `printf` we appended — the standard technique used
//! by every persistent-shell coding tool. Detection is exact, not heuristic.
//!
//! Unix-only. The host falls back to the stateless `BashTool` on Windows / when
//! the feature is disabled.

#![cfg(unix)]

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use tokio::sync::Mutex;

use crate::pty::{Pty, PtyParams};

/// Ctrl-C (ETX): interrupts the foreground command on a PTY.
const CTRL_C: u8 = 0x03;

/// How long to wait for the shell to reach its first ready sentinel at spawn.
const INIT_READY_TIMEOUT: Duration = Duration::from_millis(5_000);

/// After a Ctrl-C on timeout, how long to wait for the interrupted command's
/// sentinel to flush before giving up and respawning the shell.
const INTERRUPT_RESYNC_GRACE: Duration = Duration::from_millis(1_000);

/// Outcome of running one command in the persistent shell.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShellOutcome {
    /// Combined stdout/stderr (PTY-interleaved), with carriage returns stripped.
    pub output: String,
    /// The command's exit code.
    pub exit_code: i32,
    /// True when the command did not finish within the timeout (the shell was
    /// interrupted/respawned and `output` holds whatever had been produced).
    pub timed_out: bool,
}

/// A long-lived shell whose cwd/env persist across `run` calls. Commands are
/// serialized through an internal lock (a single shell process cannot interleave
/// commands), so this is cheap to share via `Arc`.
pub struct PersistentShell {
    /// The directory the shell is (re)spawned in — also the recovery cwd after a
    /// timeout forces a respawn.
    spawn_cwd: String,
    /// Monotonic sentinel nonce; each command gets a fresh one.
    seq: AtomicU64,
    /// The live shell + its output accumulator, guarded so commands serialize.
    inner: Mutex<Option<Arc<Pty>>>,
}

impl PersistentShell {
    /// Create a shell rooted at `cwd`. The shell process is spawned lazily on the
    /// first `run` (and respawned automatically if it dies or a timeout forces a
    /// reset).
    pub fn new(cwd: impl Into<String>) -> Self {
        Self {
            spawn_cwd: cwd.into(),
            seq: AtomicU64::new(1),
            inner: Mutex::new(None),
        }
    }

    /// Run `command`, returning its output and exit code. cwd/env mutations
    /// (`cd`, `export`) persist for subsequent calls. On timeout the foreground
    /// command is interrupted and, if it cannot be re-synced, the shell is
    /// respawned at the original cwd (losing in-shell state) and `timed_out` is
    /// set.
    pub async fn run(&self, command: &str, timeout: Duration) -> Result<ShellOutcome, String> {
        let mut guard = self.inner.lock().await;

        // (Re)spawn if there is no live shell.
        if guard.as_ref().map(|p| p.has_exited()).unwrap_or(true) {
            *guard = Some(self.spawn_ready().await?);
        }
        let pty = guard.as_ref().expect("just ensured present").clone();

        let nonce = self.seq.fetch_add(1, Ordering::Relaxed);
        match Self::exec(&pty, command, nonce, timeout).await {
            Ok(outcome) => Ok(outcome),
            Err(partial) => {
                // Timed out: interrupt, try to re-sync to the (now-aborted)
                // command's sentinel, else respawn. Subscribe before the Ctrl-C
                // write so the flushed sentinel is not missed.
                let mut rx = pty.subscribe();
                pty.write(&[CTRL_C]).ok();
                let mut sink = String::new();
                if let Some(rc) = Self::collect_until_sentinel(
                    &mut rx,
                    &Self::sentinel_prefix(nonce),
                    INTERRUPT_RESYNC_GRACE,
                    &mut sink,
                )
                .await
                {
                    return Ok(ShellOutcome {
                        output: partial,
                        exit_code: rc,
                        timed_out: true,
                    });
                }
                // Unrecoverable — drop this shell so the next call respawns.
                pty.kill();
                *guard = None;
                Ok(ShellOutcome {
                    output: partial,
                    exit_code: 124, // conventional timeout exit code
                    timed_out: true,
                })
            }
        }
    }

    /// Spawn a shell and drive it to a known-clean state: echo off, blank
    /// prompts, then a priming sentinel we wait for so init noise is drained.
    async fn spawn_ready(&self) -> Result<Arc<Pty>, String> {
        let pty = Pty::spawn(PtyParams {
            program: "sh".to_owned(),
            args: Vec::new(),
            cwd: self.spawn_cwd.clone(),
            env: HashMap::new(),
            cols: 200,
            rows: 50,
        })?;

        // Subscribe BEFORE writing so the priming sentinel is not missed.
        let mut rx = pty.subscribe();
        // Disable input echo and blank the prompts so captured output is just the
        // command's own stdout/stderr, then prime with sentinel 0.
        let init = "stty -echo 2>/dev/null; PS1=''; PS2=''; unset PROMPT_COMMAND 2>/dev/null\n";
        pty.write(init.as_bytes())?;
        pty.write(Self::sentinel_command(0).as_bytes())?;

        let mut sink = String::new();
        if Self::collect_until_sentinel(&mut rx, &Self::sentinel_prefix(0), INIT_READY_TIMEOUT, &mut sink)
            .await
            .is_none()
        {
            pty.kill();
            return Err("persistent shell did not become ready".to_owned());
        }
        Ok(pty)
    }

    /// Submit `command` plus its sentinel and collect output until the sentinel
    /// arrives. `Err(partial_output)` on timeout / stream close.
    async fn exec(
        pty: &Arc<Pty>,
        command: &str,
        nonce: u64,
        timeout: Duration,
    ) -> Result<ShellOutcome, String> {
        // Subscribe BEFORE writing to avoid missing output.
        let mut rx = pty.subscribe();
        // command on its own line (submits it), then the sentinel printf reading
        // the command's `$?`.
        let submission = format!("{command}\n{}", Self::sentinel_command(nonce));
        pty.write(submission.as_bytes()).map_err(|_| String::new())?;

        let prefix = Self::sentinel_prefix(nonce);
        let mut buf = String::new();
        match Self::collect_until_sentinel(&mut rx, &prefix, timeout, &mut buf).await {
            Some(_) => {
                let (start, rc) = Self::find_sentinel(&buf, &prefix).expect("sentinel just matched");
                Ok(ShellOutcome {
                    output: Self::clean(&buf[..start]),
                    exit_code: rc,
                    timed_out: false,
                })
            }
            None => Err(Self::extract_output(&buf, &prefix)),
        }
    }

    /// Read from `rx` into `sink` until a parseable sentinel for `prefix` appears
    /// or `timeout` elapses / the stream closes. Returns the parsed exit code.
    async fn collect_until_sentinel(
        rx: &mut tokio::sync::broadcast::Receiver<Vec<u8>>,
        prefix: &str,
        timeout: Duration,
        sink: &mut String,
    ) -> Option<i32> {
        // A sentinel may already be present if the caller pre-filled `sink`.
        if let Some((_, rc)) = Self::find_sentinel(sink, prefix) {
            return Some(rc);
        }
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                return None;
            }
            match tokio::time::timeout(remaining, rx.recv()).await {
                Ok(Ok(chunk)) => {
                    sink.push_str(&String::from_utf8_lossy(&chunk));
                    if let Some((_, rc)) = Self::find_sentinel(sink, prefix) {
                        return Some(rc);
                    }
                }
                // Lagged: a chunk was dropped; keep going (best-effort output).
                Ok(Err(tokio::sync::broadcast::error::RecvError::Lagged(_))) => continue,
                // Stream closed (shell died) or timeout — give up.
                Ok(Err(_)) | Err(_) => return None,
            }
        }
    }

    /// The `printf` that emits sentinel `nonce` carrying the prior command's `$?`.
    fn sentinel_command(nonce: u64) -> String {
        format!("printf '__NOMI_END_{nonce}__%d__\\n' \"$?\"\n")
    }

    /// The literal prefix that precedes the exit code in the emitted sentinel.
    fn sentinel_prefix(nonce: u64) -> String {
        format!("__NOMI_END_{nonce}__")
    }

    /// Scan **all** occurrences of `prefix` in `buf` and return the byte offset of
    /// the first one followed by `<digits>__` (the real sentinel), plus the code.
    /// Earlier occurrences with a non-numeric tail (e.g. the echoed `printf
    /// '...%d__'` command line, if echo was on) are skipped — this makes
    /// detection robust without depending on `stty -echo` timing.
    fn find_sentinel(buf: &str, prefix: &str) -> Option<(usize, i32)> {
        let mut search_from = 0;
        while let Some(rel) = buf[search_from..].find(prefix) {
            let start = search_from + rel;
            let after = &buf[start + prefix.len()..];
            if let Some(end) = after.find("__")
                && let Ok(rc) = after[..end].parse::<i32>()
            {
                return Some((start, rc));
            }
            search_from = start + prefix.len();
        }
        None
    }

    /// Output before the sentinel, used on the timeout path where no code parsed.
    fn extract_output(buf: &str, prefix: &str) -> String {
        match Self::find_sentinel(buf, prefix).map(|(s, _)| s).or_else(|| buf.find(prefix)) {
            Some(start) => Self::clean(&buf[..start]),
            None => Self::clean(buf),
        }
    }

    /// Strip carriage returns and a single trailing newline from captured output.
    fn clean(s: &str) -> String {
        let s = s.replace('\r', "");
        s.strip_suffix('\n').unwrap_or(&s).to_owned()
    }

    /// Test-only: the live shell's pid, if spawned.
    #[cfg(test)]
    async fn pid_for_test(&self) -> Option<u32> {
        self.inner.lock().await.as_ref().and_then(|p| p.pid())
    }
}

impl Drop for PersistentShell {
    /// Kill the live shell (and its process group) on teardown. Without this the
    /// child `sh` lingers: its stdin never reaches EOF while the PTY reader
    /// thread still holds the master open, so it would outlive the session.
    fn drop(&mut self) {
        if let Some(pty) = self.inner.get_mut().take() {
            pty.kill();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn shell() -> PersistentShell {
        PersistentShell::new(std::env::temp_dir().to_string_lossy().into_owned())
    }

    const T: Duration = Duration::from_millis(8_000);

    #[tokio::test]
    async fn runs_command_and_returns_stdout() {
        let sh = shell();
        let out = sh.run("echo hello_shell", T).await.expect("run");
        assert_eq!(out.exit_code, 0, "output: {:?}", out.output);
        assert!(out.output.contains("hello_shell"), "got: {:?}", out.output);
        assert!(!out.timed_out);
    }

    #[tokio::test]
    async fn reports_nonzero_exit_code() {
        let sh = shell();
        // A subshell so the nonzero exit does not terminate the persistent shell.
        let out = sh.run("(exit 7)", T).await.expect("run");
        assert_eq!(out.exit_code, 7, "got: {:?}", out);
        assert!(!out.timed_out);
    }

    #[tokio::test]
    async fn cwd_persists_across_commands() {
        let dir = tempfile::tempdir().unwrap();
        let sub = dir.path().join("nested");
        std::fs::create_dir(&sub).unwrap();
        let sh = PersistentShell::new(dir.path().to_string_lossy().into_owned());

        sh.run(&format!("cd {}", sub.display()), T).await.expect("cd");
        let out = sh.run("pwd", T).await.expect("pwd");
        assert!(
            out.output.contains("nested"),
            "cwd should persist across commands, got: {:?}",
            out.output
        );
    }

    #[tokio::test]
    async fn env_persists_across_commands() {
        let sh = shell();
        sh.run("export NOMI_TEST_VAR=persisted_value", T).await.expect("export");
        let out = sh.run("echo $NOMI_TEST_VAR", T).await.expect("echo");
        assert!(
            out.output.contains("persisted_value"),
            "exported env should persist, got: {:?}",
            out.output
        );
    }

    #[tokio::test]
    async fn timeout_is_recoverable() {
        let sh = shell();
        let out = sh
            .run("sleep 30", Duration::from_millis(600))
            .await
            .expect("run");
        assert!(out.timed_out, "sleep 30 with a 600ms budget must time out");
        // The shell must remain usable for the next command after a timeout.
        let after = sh.run("echo recovered", T).await.expect("post-timeout run");
        assert_eq!(after.exit_code, 0);
        assert!(after.output.contains("recovered"), "got: {:?}", after.output);
    }

    #[tokio::test]
    async fn kills_shell_process_on_drop() {
        let sh = shell();
        sh.run("true", T).await.expect("spawn shell");
        let pid = sh.pid_for_test().await.expect("pid") as i32;
        assert_eq!(unsafe { libc::kill(pid, 0) }, 0, "shell alive before drop");

        drop(sh);

        let start = std::time::Instant::now();
        let mut dead = false;
        while start.elapsed() < Duration::from_millis(3000) {
            if unsafe { libc::kill(pid, 0) } != 0 {
                dead = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(dead, "the shell process must be killed when PersistentShell is dropped");
    }
}

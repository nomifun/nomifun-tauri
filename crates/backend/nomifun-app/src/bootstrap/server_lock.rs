//! Exclusive per-data-dir server lock.
//!
//! Every host built for the same channel (the desktop shell's embedded backend,
//! `nomifun-web`, the `nomicore` bin) defaults to one shared data directory
//! (see [`crate::cli::default_data_dir`]). Two live backends on the same
//! directory would double-fire every cron job (each process arms its own timers
//! from the shared DB), fight over channel polling (Telegram allows a single
//! `getUpdates` consumer and the on-disk update watermark is
//! last-writer-wins, reopening the dedup window), interleave writes to the
//! rolling log file, and — worst — race the SQLite corruption-recovery path,
//! which renames a database the other process still holds open.
//!
//! So the server takes an OS-level exclusive lock on `{data_dir}/server.lock`
//! before touching the data layer and holds it for the process lifetime. A
//! second server fails fast with an actionable message instead of silently
//! corrupting shared state.
//!
//! The lock is advisory (`flock` on Unix, `LockFileEx` on Windows, via `fs2`
//! — the same dependency nomifun-db's migrate lock uses) and is released by
//! the OS when the process exits *or crashes*; a leftover `server.lock` FILE
//! is harmless and needs no staleness heuristics. Read-only companions are
//! deliberately unaffected: `nomicore doctor` opens the DB without this lock
//! (it is designed to run while the server is alive), and the `mcp-*` stdio
//! helpers never touch the data dir at all.

use std::fs::{File, OpenOptions};
use std::path::Path;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use fs2::FileExt;

/// Lock file name under the data dir. The lock lives on the open handle, not
/// on the file's existence — the file itself is just an address.
pub const SERVER_LOCK_FILE: &str = "server.lock";

/// How long to keep retrying a *contended* lock before giving up. A desktop
/// `relaunch()` spawns the new process before the old one has exited, so for a
/// short window both are alive and the old one still holds this lock; without a
/// wait the new process would fail its boot with a spurious "already in use"
/// dialog. The old process releases on exit (OS-level), normally within a
/// second or two, so a few seconds of retry absorbs the handoff while a genuine
/// second instance still surfaces the error promptly after the window.
const LOCK_HANDOFF_TIMEOUT: Duration = Duration::from_secs(8);

/// Poll interval while waiting out a contended lock.
const LOCK_RETRY_INTERVAL: Duration = Duration::from_millis(150);

/// Sidecar naming the current holder (pid + exe), written by the winner AFTER
/// acquiring. It must be a separate, never-locked file: on Windows the
/// exclusive `LockFileEx` range makes `server.lock` itself unreadable to the
/// losing process, so a breadcrumb stored inside the lock file could never
/// reach the error message that needs it.
const SERVER_LOCK_INFO_FILE: &str = "server.lock.info";

/// Held by [`super::ServerEnvironment`] for the process lifetime; dropping it
/// (process exit) releases the lock.
#[derive(Debug)]
pub struct ServerLock {
    _file: File,
}

pub(super) fn acquire_server_lock(data_dir: &Path) -> Result<ServerLock> {
    acquire_server_lock_with_timeout(data_dir, LOCK_HANDOFF_TIMEOUT)
}

/// Inner implementation parameterized by the contention-retry window so tests
/// can exercise immediate failure (`Duration::ZERO`) without waiting it out.
fn acquire_server_lock_with_timeout(data_dir: &Path, timeout: Duration) -> Result<ServerLock> {
    std::fs::create_dir_all(data_dir)
        .with_context(|| format!("failed to create data dir {}", data_dir.display()))?;

    let path = data_dir.join(SERVER_LOCK_FILE);
    let file = OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .open(&path)
        .with_context(|| format!("failed to open server lock {}", path.display()))?;

    let deadline = Instant::now() + timeout;
    loop {
        match file.try_lock_exclusive() {
            Ok(()) => break,
            Err(e) => {
                // Only a CONTENDED lock means "another backend is running".
                // Anything else (filesystems without lock support — NFS sans
                // lockd, some FUSE mounts — report ENOLCK/EOPNOTSUPP) must
                // surface as the IO error it is, or the user gets sent hunting
                // for an instance that doesn't exist.
                if e.raw_os_error() != fs2::lock_contended_error().raw_os_error() {
                    return Err(anyhow::Error::new(e)
                        .context(format!("failed to lock {} (filesystem without lock support?)", path.display())));
                }
                // Contended: most likely a restart handoff where the previous
                // process has not finished exiting. Retry until the deadline
                // before declaring a real second instance.
                if Instant::now() >= deadline {
                    let holder = std::fs::read_to_string(data_dir.join(SERVER_LOCK_INFO_FILE)).unwrap_or_default();
                    let holder = holder.trim();
                    bail!(
                        "data directory {} is already in use by another running NomiFun backend{} — \
                         close the other instance (the desktop app, `bun run web` / `dev:webui`, or `nomicore`) \
                         and retry, or point this one at its own directory via NOMIFUN_DATA_DIR / --data-dir",
                        data_dir.display(),
                        if holder.is_empty() {
                            String::new()
                        } else {
                            format!(" ({holder})")
                        },
                    );
                }
                std::thread::sleep(LOCK_RETRY_INTERVAL);
            }
        }
    }

    // Best-effort holder breadcrumb for the next contender's error message.
    // Failures here must not fail the boot — the lock is already held.
    let exe = std::env::current_exe()
        .ok()
        .and_then(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()))
        .unwrap_or_else(|| "unknown".to_owned());
    let _ = std::fs::write(
        data_dir.join(SERVER_LOCK_INFO_FILE),
        format!("pid {} • {exe}\n", std::process::id()),
    );

    Ok(ServerLock { _file: file })
}

#[cfg(test)]
mod tests {
    use super::{acquire_server_lock, acquire_server_lock_with_timeout};

    /// Both `flock` (per open-file-description) and `LockFileEx` (per handle)
    /// conflict across two handles within one process, so this exercises the
    /// real contention path portably.
    #[test]
    fn second_lock_on_same_dir_fails_until_first_released() {
        let dir = tempfile::tempdir().expect("tempdir");

        let first = acquire_server_lock(dir.path()).expect("first lock must succeed");

        // Zero timeout = the fail-fast path, exercised directly so the test does
        // not wait out the restart-handoff retry window.
        let err = acquire_server_lock_with_timeout(dir.path(), std::time::Duration::ZERO)
            .expect_err("second lock must fail while held");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("already in use"),
            "error should explain the conflict, got: {msg}"
        );
        assert!(
            msg.contains("NOMIFUN_DATA_DIR"),
            "error should point at the escape hatch, got: {msg}"
        );
        let pid = std::process::id().to_string();
        assert!(
            msg.contains(&pid),
            "error should name the holder via the sidecar breadcrumb, got: {msg}"
        );

        drop(first);
        let _again = acquire_server_lock(dir.path()).expect("lock must be reacquirable after release");
    }

    #[test]
    fn creates_missing_data_dir() {
        let dir = tempfile::tempdir().expect("tempdir");
        let nested = dir.path().join("not-yet").join("created");
        let _lock = acquire_server_lock(&nested).expect("lock should create the data dir");
        assert!(nested.is_dir());
    }

    /// A restart hands the data dir from the old process to the new one: the new
    /// process can reach `acquire_server_lock` before the old one has dropped its
    /// lock. Acquisition must wait out that brief window instead of failing fast.
    #[test]
    fn acquire_waits_out_a_brief_handoff_window() {
        use std::time::Duration;

        let dir = tempfile::tempdir().expect("tempdir");
        let first = acquire_server_lock(dir.path()).expect("first lock must succeed");

        // Mimic the old process exiting ~300ms into the new process's boot.
        let releaser = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(300));
            drop(first);
        });

        // The new process acquires once the holder releases, rather than erroring.
        let lock = acquire_server_lock(dir.path()).expect("must acquire after the holder releases");

        releaser.join().unwrap();
        drop(lock);
    }
}

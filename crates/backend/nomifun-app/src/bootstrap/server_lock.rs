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
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
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
const CANONICAL_SERVER_DATABASE_FILE: &str = "nomifun-backend.db";

fn is_canonical_server_database_file(path: &Path) -> bool {
    let Some(file_name) = path.file_name() else {
        return false;
    };
    #[cfg(windows)]
    {
        file_name
            .to_string_lossy()
            .eq_ignore_ascii_case(CANONICAL_SERVER_DATABASE_FILE)
    }
    #[cfg(not(windows))]
    {
        file_name == std::ffi::OsStr::new(CANONICAL_SERVER_DATABASE_FILE)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct FileSystemIdentity {
    volume: u64,
    index: u64,
}

#[cfg(unix)]
fn file_system_identity(path: &Path) -> io::Result<FileSystemIdentity> {
    use std::os::unix::fs::MetadataExt;

    let metadata = std::fs::metadata(path)?;
    Ok(FileSystemIdentity {
        volume: metadata.dev(),
        index: metadata.ino(),
    })
}

#[cfg(windows)]
fn file_system_identity(path: &Path) -> io::Result<FileSystemIdentity> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::Storage::FileSystem::{
        BY_HANDLE_FILE_INFORMATION, CreateFileW, FILE_FLAG_BACKUP_SEMANTICS,
        FILE_READ_ATTRIBUTES, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE,
        GetFileInformationByHandle, OPEN_EXISTING,
    };

    let mut wide = path.as_os_str().encode_wide().collect::<Vec<_>>();
    wide.push(0);
    // SAFETY: `wide` is NUL-terminated and remains alive through both Win32
    // calls. The checked handle is closed exactly once.
    unsafe {
        let handle = CreateFileW(
            wide.as_ptr(),
            FILE_READ_ATTRIBUTES,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            std::ptr::null(),
            OPEN_EXISTING,
            FILE_FLAG_BACKUP_SEMANTICS,
            std::ptr::null_mut(),
        );
        if handle == INVALID_HANDLE_VALUE {
            return Err(io::Error::last_os_error());
        }
        let mut information = BY_HANDLE_FILE_INFORMATION::default();
        let succeeded = GetFileInformationByHandle(handle, &mut information) != 0;
        let information_error = (!succeeded).then(io::Error::last_os_error);
        let _ = CloseHandle(handle);
        if let Some(error) = information_error {
            return Err(error);
        }
        Ok(FileSystemIdentity {
            volume: u64::from(information.dwVolumeSerialNumber),
            index: (u64::from(information.nFileIndexHigh) << 32)
                | u64::from(information.nFileIndexLow),
        })
    }
}

#[cfg(not(any(unix, windows)))]
fn file_system_identity(path: &Path) -> io::Result<FileSystemIdentity> {
    let canonical = std::fs::canonicalize(path)?;
    let mut hash = 1469598103934665603_u64;
    for byte in canonical.as_os_str().as_encoded_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(1099511628211);
    }
    Ok(FileSystemIdentity {
        volume: 0,
        index: hash,
    })
}

/// Held by [`super::ServerEnvironment`] for the process lifetime; dropping it
/// (process exit) releases the lock.
#[derive(Debug)]
pub struct ServerLock {
    _file: File,
    data_dir_path: PathBuf,
    data_dir_identity: FileSystemIdentity,
}

/// Unforgeable proof that this process owns the canonical server lock for one
/// exact data directory.
///
/// The proof retains the lock handle itself. Startup orphan reconciliation may
/// therefore never outlive (or merely assume) the database-ownership lock.
/// Acquisition proves that no previous backend still owns that lock, but is
/// deliberately not terminal proof for processes or blocking side effects that
/// backend may have spawned.
#[derive(Debug)]
pub struct BootServerLockAuthority {
    _server_lock: Arc<ServerLock>,
    data_dir_path: PathBuf,
    data_dir_identity: FileSystemIdentity,
}

impl BootServerLockAuthority {
    /// Canonical path used only for diagnostics; authorization comparisons use
    /// OS file identity, not lexical path equality.
    pub fn protected_data_dir(&self) -> &Path {
        &self.data_dir_path
    }

    /// Return whether `data_dir` resolves to the exact directory protected by
    /// this authority. Canonical identities make symlink, relative-path, and
    /// Windows path-spelling aliases converge on one lock domain.
    pub fn protects_data_dir(&self, data_dir: &Path) -> Result<bool> {
        Ok(file_system_identity(data_dir).with_context(|| {
            format!(
                "failed to identify data directory {} while validating boot authority",
                data_dir.display()
            )
        })? == self.data_dir_identity)
    }

    /// Verify both the configured database path and SQLite's live `main`
    /// attachment belong to the exact directory whose server lock is retained.
    pub async fn protects_database(
        &self,
        database: &nomifun_db::Database,
        configured_database: &Path,
    ) -> Result<bool> {
        if !is_canonical_server_database_file(configured_database) {
            return Ok(false);
        }
        let configured_parent = configured_database.parent().ok_or_else(|| {
            anyhow::anyhow!(
                "configured database has no parent directory: {}",
                configured_database.display()
            )
        })?;
        if file_system_identity(configured_parent).with_context(|| {
            format!(
                "failed to identify configured database directory {}",
                configured_parent.display()
            )
        })? != self.data_dir_identity
        {
            return Ok(false);
        }
        let configured_database_identity =
            file_system_identity(configured_database).with_context(|| {
                format!(
                    "failed to identify configured database {} while validating boot authority",
                    configured_database.display()
                )
            })?;

        let database_files: Vec<(i64, String, String)> =
            nomifun_db::sqlx::query_as("PRAGMA database_list")
                .fetch_all(database.pool())
                .await?;
        let Some(live_database) = database_files
            .into_iter()
            .find_map(|(_seq, name, file)| (name == "main").then_some(file))
            .filter(|file| !file.is_empty())
        else {
            return Ok(false);
        };
        let live_database = Path::new(&live_database);
        if !is_canonical_server_database_file(live_database) {
            return Ok(false);
        }
        let Some(live_parent) = live_database.parent() else {
            return Ok(false);
        };
        if file_system_identity(live_parent).with_context(|| {
            format!(
                "failed to identify live SQLite database directory {}",
                live_parent.display()
            )
        })? != self.data_dir_identity
        {
            return Ok(false);
        }
        let live_database_identity = file_system_identity(live_database).with_context(|| {
            format!(
                "failed to identify live SQLite database {} while validating boot authority",
                live_database.display()
            )
        })?;
        Ok(live_database_identity == configured_database_identity)
    }
}

pub(super) fn acquire_server_lock(data_dir: &Path) -> Result<ServerLock> {
    acquire_server_lock_with_timeout(data_dir, LOCK_HANDOFF_TIMEOUT)
}

/// Inner implementation parameterized by the contention-retry window so tests
/// can exercise immediate failure (`Duration::ZERO`) without waiting it out.
fn acquire_server_lock_with_timeout(data_dir: &Path, timeout: Duration) -> Result<ServerLock> {
    std::fs::create_dir_all(data_dir)
        .with_context(|| format!("failed to create data dir {}", data_dir.display()))?;
    let data_dir_identity = std::fs::canonicalize(data_dir)
        .with_context(|| format!("failed to resolve data dir {}", data_dir.display()))?;
    let data_dir_file_identity = file_system_identity(&data_dir_identity)
        .with_context(|| format!("failed to identify data dir {}", data_dir_identity.display()))?;

    let path = data_dir_identity.join(SERVER_LOCK_FILE);
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
                    let holder =
                        std::fs::read_to_string(data_dir_identity.join(SERVER_LOCK_INFO_FILE))
                            .unwrap_or_default();
                    let holder = holder.trim();
                    bail!(
                        "data directory {} is already in use by another running NomiFun backend{} — \
                         close the other instance (the desktop app, `bun run web` / `dev:webui`, or `nomicore`) \
                         and retry, or point this one at its own directory via NOMIFUN_DATA_DIR / --data-dir",
                        data_dir_identity.display(),
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
        data_dir_identity.join(SERVER_LOCK_INFO_FILE),
        format!("pid {} • {exe}\n", std::process::id()),
    );

    Ok(ServerLock {
        _file: file,
        data_dir_path: data_dir_identity,
        data_dir_identity: data_dir_file_identity,
    })
}

impl ServerLock {
    pub(super) fn boot_authority(self: &Arc<Self>) -> BootServerLockAuthority {
        BootServerLockAuthority {
            _server_lock: Arc::clone(self),
            data_dir_path: self.data_dir_path.clone(),
            data_dir_identity: self.data_dir_identity,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{acquire_server_lock, acquire_server_lock_with_timeout};
    use std::sync::Arc;

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

    #[test]
    fn boot_authority_retains_lock_and_uses_directory_file_identity() {
        let root = tempfile::tempdir().expect("tempdir");
        let data_dir = root.path().join("data");
        let other_dir = root.path().join("other");
        std::fs::create_dir_all(data_dir.join("child")).unwrap();
        std::fs::create_dir_all(&other_dir).unwrap();

        let lock = Arc::new(acquire_server_lock(&data_dir).expect("lock"));
        let authority = lock.boot_authority();
        assert!(authority.protects_data_dir(&data_dir).unwrap());
        assert!(
            authority
                .protects_data_dir(&data_dir.join("child").join(".."))
                .unwrap(),
            "relative lexical aliases must resolve to the same directory identity"
        );
        assert!(!authority.protects_data_dir(&other_dir).unwrap());

        drop(lock);
        acquire_server_lock_with_timeout(&data_dir, std::time::Duration::ZERO)
            .expect_err("authority must retain the underlying OS lock");
        drop(authority);
        let _again = acquire_server_lock(&data_dir).expect("dropping authority releases final lock owner");
    }

    #[test]
    fn boot_authority_remains_opaque_non_clone_lock_ownership() {
        let source = include_str!("server_lock.rs");
        let production_source = source
            .split_once("#[cfg(test)]")
            .expect("server lock source must contain tests")
            .0;
        let declaration_start = production_source
            .find("/// Unforgeable proof")
            .expect("authority documentation marker");
        let declaration_end = production_source[declaration_start..]
            .find("impl BootServerLockAuthority")
            .expect("authority implementation");
        let declaration =
            &production_source[declaration_start..declaration_start + declaration_end];
        assert!(
            !declaration.contains("Clone")
                && !production_source.contains("impl Clone for BootServerLockAuthority"),
            "duplicating boot authority must remain an explicit ServerEnvironment action"
        );
        assert!(declaration.contains("_server_lock: Arc<ServerLock>"));
        assert!(
            declaration
                .lines()
                .filter(|line| line.contains(':'))
                .all(|line| !line.trim_start().starts_with("pub ")),
            "external crates must not be able to forge authority fields"
        );
    }

    #[cfg(unix)]
    #[test]
    fn boot_authority_accepts_unix_symlink_alias_by_file_identity() {
        use std::os::unix::fs::symlink;

        let root = tempfile::tempdir().expect("tempdir");
        let data_dir = root.path().join("data");
        let alias = root.path().join("data-alias");
        std::fs::create_dir_all(&data_dir).unwrap();
        symlink(&data_dir, &alias).unwrap();

        let lock = Arc::new(acquire_server_lock(&data_dir).expect("lock"));
        let authority = lock.boot_authority();
        assert!(authority.protects_data_dir(&alias).unwrap());
    }

    #[cfg(windows)]
    #[test]
    fn boot_authority_accepts_windows_junction_and_case_aliases_by_file_identity() {
        let root = tempfile::tempdir().expect("tempdir");
        let data_dir = root.path().join("DataIdentity");
        let alias = root.path().join("data-junction");
        std::fs::create_dir_all(&data_dir).unwrap();
        junction::create(&data_dir, &alias).unwrap();

        let lock = Arc::new(acquire_server_lock(&data_dir).expect("lock"));
        let authority = lock.boot_authority();
        assert!(authority.protects_data_dir(&alias).unwrap());

        let case_alias = data_dir.with_file_name("dataidentity");
        assert!(
            authority.protects_data_dir(&case_alias).unwrap(),
            "Windows casing aliases must compare by volume/file index"
        );
        junction::delete(&alias).unwrap();
    }

    #[tokio::test]
    async fn boot_authority_rejects_other_directory_and_other_database_file() {
        let root = tempfile::tempdir().expect("tempdir");
        let data_dir = root.path().join("data");
        let other_dir = root.path().join("other");
        std::fs::create_dir_all(&data_dir).unwrap();
        std::fs::create_dir_all(&other_dir).unwrap();
        let database_path = data_dir.join("nomifun-backend.db");
        let other_file_path = data_dir.join("other.db");
        let other_dir_path = other_dir.join("nomifun-backend.db");
        let database = nomifun_db::init_database(&database_path).await.unwrap();
        let other_file = nomifun_db::init_database(&other_file_path).await.unwrap();
        let other_database = nomifun_db::init_database(&other_dir_path).await.unwrap();

        let lock = Arc::new(acquire_server_lock(&data_dir).expect("lock"));
        let authority = lock.boot_authority();
        assert!(
            authority
                .protects_database(&database, &database_path)
                .await
                .unwrap()
        );
        assert!(
            !authority
                .protects_database(&other_file, &other_file_path)
                .await
                .unwrap(),
            "a different SQLite file in the locked directory is not canonical"
        );
        assert!(
            !authority
                .protects_database(&other_database, &other_dir_path)
                .await
                .unwrap(),
            "a database under another directory is outside the lock authority"
        );

        database.close().await;
        other_file.close().await;
        other_database.close().await;

        let in_directory_alias = data_dir.join("database-alias.db");
        std::fs::hard_link(&database_path, &in_directory_alias)
            .expect("test filesystem must support a same-volume database hardlink");
        let alias_database = nomifun_db::init_database(&in_directory_alias).await.unwrap();
        assert!(
            !authority
                .protects_database(&alias_database, &database_path)
                .await
                .unwrap(),
            "a live SQLite alias with a non-canonical filename must be rejected"
        );
        alias_database.close().await;

        let hardlink_dir = root.path().join("hardlink-outside-lock");
        std::fs::create_dir_all(&hardlink_dir).unwrap();
        let external_hardlink = hardlink_dir.join("nomifun-backend.db");
        std::fs::hard_link(&database_path, &external_hardlink)
            .expect("test filesystem must support a same-volume database hardlink");
        let hardlink_database = nomifun_db::init_database(&external_hardlink).await.unwrap();
        assert!(
            !authority
                .protects_database(&hardlink_database, &database_path)
                .await
                .unwrap(),
            "matching file identity cannot authorize a live SQLite path outside the locked directory"
        );
        hardlink_database.close().await;
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

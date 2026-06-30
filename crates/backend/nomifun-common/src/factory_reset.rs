//! Factory reset: arm a marker file, then perform the wipe early on the *next*
//! boot — before the DB pool opens or any background loop starts.
//!
//! Why arm-then-reboot instead of wiping in place: the `SqlitePool` is cloned
//! across every service, many background loops (AutoWork persistent loop, cron,
//! channel orchestrator, knowledge resume, companion service, IDMM …) write to the DB
//! continuously and there is no global "pause all" switch, and on Windows an
//! open connection handle blocks deleting the `.db` file. Doing the wipe at the
//! very start of boot — after `acquire_server_lock` but before `init_database`
//! — sidesteps all of that: we hold the exclusive lock, no pool is open, and no
//! loop is running.
//!
//! Flow:
//!   1. `POST /api/system/factory-reset` → [`write_marker`]
//!   2. Frontend relaunches the desktop shell.
//!   3. Next boot → [`apply_pending_reset`] deletes the DB family + derived data
//!      and clears the marker; `init_database` then recreates a fresh schema.

use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::error::AppError;
use crate::timestamp::now_ms;

/// Marker file under the data dir. Its presence means a reset is pending.
pub const RESET_MARKER_FILE: &str = "factory-reset.pending";

/// Scope of a factory reset. Only `Full` exists today; the enum leaves room to
/// add a DB-only variant later without changing the marker format.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum ResetScope {
    /// Wipe the database AND derived on-disk data (true factory reset).
    #[default]
    Full,
}

/// Contents of the pending-reset marker file.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResetMarker {
    #[serde(default)]
    pub scope: ResetScope,
    /// Reserved: keep a `.factory-backup.<ts>` copy of the DB before wiping.
    /// Currently always `false` (product decision: no backup, truly delete).
    #[serde(default)]
    pub backup: bool,
    /// Epoch millis when the reset was requested (informational).
    #[serde(default)]
    pub requested_at: i64,
}

impl Default for ResetMarker {
    fn default() -> Self {
        Self { scope: ResetScope::Full, backup: false, requested_at: 0 }
    }
}

impl ResetMarker {
    /// Build a marker stamped with the current time.
    pub fn new(scope: ResetScope) -> Self {
        Self { scope, backup: false, requested_at: now_ms() }
    }
}

/// SQLite DB family (relative to data_dir). Mirrors `nomifun-db`'s on-disk
/// layout: the database, its WAL sidecars, and the cross-process migrate lock.
/// `server.lock` / `server.lock.info` are intentionally excluded — this process
/// holds that lock for its whole lifetime.
///
/// Order matters for partial-failure safety: sidecars and the migrate lock are
/// removed first and the main `.db` last, so a half-completed wipe never leaves
/// a freshly created DB paired with a stale `-wal`/`-shm`.
const DB_FAMILY: &[&str] = &[
    "nomifun-backend.db-wal",
    "nomifun-backend.db-shm",
    "nomifun-backend.db.migrate.lock",
    "nomifun-backend.db",
];

/// Derived data directories (relative to data_dir) wiped on a `Full` reset.
/// Names are kept as literals here so `nomifun-common` need not depend on the
/// domain crates; they mirror those crates' constants:
///   `knowledge`   → `nomifun_knowledge::KB_MANAGED_REL_DIR`
///   `companion`         → `nomifun_companion::PET_*_REL_DIR` (whole tree incl. `memory.db`)
///   `attachments` → `nomifun_requirement` `ATTACHMENTS_REL_DIR`
///   `cron`        → parent of `nomifun_cron::CRON_SKILLS_REL_DIR`
///   `conversations` → per-conversation workspaces (also handled under work_dir)
/// Intentionally NOT wiped (regenerable or in-use): `logs` (tracing holds an
/// open handle), `runtime`, `bun-cache`, `bun-tmp`, `builtin-skills` (the next
/// boot re-materializes them).
const DERIVED_DIRS: &[&str] = &[
    "conversations",
    "attachments",
    "knowledge",
    "companion",
    "cron",
    "preview-history",
    "nomi-sessions",
    "nomi-health-check-sessions",
    "browser-profile",
];

fn marker_path(data_dir: &Path) -> PathBuf {
    data_dir.join(RESET_MARKER_FILE)
}

/// Arm a factory reset: write the marker. The actual wipe happens on next boot.
pub fn write_marker(data_dir: &Path, marker: &ResetMarker) -> Result<(), AppError> {
    let json = serde_json::to_vec_pretty(marker)
        .map_err(|e| AppError::Internal(format!("serialize factory-reset marker: {e}")))?;
    std::fs::write(marker_path(data_dir), json)
        .map_err(|e| AppError::Internal(format!("write factory-reset marker: {e}")))?;
    Ok(())
}

/// Read the pending-reset marker, if any. A present-but-malformed marker is
/// treated as a default (`Full`) reset rather than silently ignored — once a
/// reset is armed it must not be skipped.
pub fn read_marker(data_dir: &Path) -> Option<ResetMarker> {
    let bytes = std::fs::read(marker_path(data_dir)).ok()?;
    Some(serde_json::from_slice(&bytes).unwrap_or_default())
}

/// Remove the marker file (idempotent).
pub fn clear_marker(data_dir: &Path) {
    let _ = std::fs::remove_file(marker_path(data_dir));
}

/// If a reset marker is present, perform the wipe and clear the marker.
/// Returns `Ok(true)` if a reset was applied, `Ok(false)` if there was nothing
/// to do. Must be called early in boot (after the server lock is held, before
/// the database is opened).
///
/// The DB family removal is treated as fatal (it must succeed for a clean
/// reinit; at this boot stage nothing holds those handles). Derived-data
/// removal is best-effort: failures are logged and boot continues.
pub fn apply_pending_reset(data_dir: &Path, work_dir: &Path) -> Result<bool, AppError> {
    let Some(marker) = read_marker(data_dir) else {
        return Ok(false);
    };
    tracing::warn!(
        target: "factory_reset",
        scope = ?marker.scope,
        requested_at = marker.requested_at,
        "factory-reset marker found — wiping database and derived data"
    );

    // 1. DB family — core; must succeed.
    for name in DB_FAMILY {
        let path = data_dir.join(name);
        remove_path_with_retry(&path).map_err(|e| {
            AppError::Internal(format!("factory reset: failed to remove {}: {e}", path.display()))
        })?;
    }

    // 2. Derived data dirs — best-effort.
    let mut targets: Vec<PathBuf> = DERIVED_DIRS.iter().map(|d| data_dir.join(d)).collect();
    if work_dir != data_dir {
        // Conversation workspaces live under work_dir when it has been relocated
        // away from data_dir.
        targets.push(work_dir.join("conversations"));
    }
    for path in targets {
        if let Err(e) = remove_path_with_retry(&path) {
            tracing::warn!(
                target: "factory_reset",
                path = %path.display(),
                error = %e,
                "factory reset: could not remove derived path (continuing)"
            );
        }
    }

    // 2b. The persisted work-dir override (dir-config.json) is a user preference,
    // not regenerable data, so it is not in DERIVED_DIRS — but a true factory
    // reset must drop it too, or work_dir would stay pointed at the custom
    // location next boot instead of returning to the default. Best-effort.
    let dir_config = data_dir.join(crate::dir_config::DIR_CONFIG_FILE);
    if let Err(e) = remove_path_with_retry(&dir_config) {
        tracing::warn!(
            target: "factory_reset",
            path = %dir_config.display(),
            error = %e,
            "factory reset: could not remove dir-config override (continuing)"
        );
    }

    // 3. Clear the marker so the next boot is normal.
    clear_marker(data_dir);
    tracing::warn!(target: "factory_reset", "factory reset complete — a fresh database will be created");
    Ok(true)
}

/// Remove a file, directory tree, or symlink. Missing paths are a no-op. On
/// Windows, transient sharing/lock/access errors are retried with backoff
/// (mirrors `nomifun-db`'s startup file-op retry for raw OS errors 5/32/33).
fn remove_path_with_retry(path: &Path) -> std::io::Result<()> {
    const MAX_ATTEMPTS: u32 = 5;
    for attempt in 1..=MAX_ATTEMPTS {
        let result = match std::fs::symlink_metadata(path) {
            Ok(meta) if meta.is_dir() => std::fs::remove_dir_all(path),
            Ok(_) => std::fs::remove_file(path),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(e) => Err(e),
        };
        match result {
            Ok(()) => return Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(e) if attempt < MAX_ATTEMPTS && is_retryable(&e) => {
                std::thread::sleep(Duration::from_millis(80 * u64::from(attempt)));
            }
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

/// Windows transient file-op errors: 5 = access denied, 32 = sharing violation,
/// 33 = lock violation.
fn is_retryable(e: &std::io::Error) -> bool {
    matches!(e.raw_os_error(), Some(5) | Some(32) | Some(33))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn touch(path: &Path) {
        std::fs::write(path, b"x").unwrap();
    }

    #[test]
    fn no_marker_is_noop() {
        let dir = std::env::temp_dir().join(format!("nomifun-fr-noop-{}", now_ms()));
        std::fs::create_dir_all(&dir).unwrap();
        assert_eq!(apply_pending_reset(&dir, &dir).unwrap(), false);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn full_reset_wipes_targets_keeps_logs_and_clears_marker() {
        let dir = std::env::temp_dir().join(format!("nomifun-fr-full-{}", now_ms()));
        std::fs::create_dir_all(&dir).unwrap();

        // DB family + sidecars.
        touch(&dir.join("nomifun-backend.db"));
        touch(&dir.join("nomifun-backend.db-wal"));
        touch(&dir.join("nomifun-backend.db-shm"));
        touch(&dir.join("nomifun-backend.db.migrate.lock"));
        // Derived data dirs.
        for d in ["conversations", "attachments", "knowledge", "companion", "cron", "browser-profile"] {
            std::fs::create_dir_all(dir.join(d)).unwrap();
            touch(&dir.join(d).join("inner.txt"));
        }
        // Preserved dirs / our lock.
        std::fs::create_dir_all(dir.join("logs")).unwrap();
        touch(&dir.join("logs").join("app.log"));
        std::fs::create_dir_all(dir.join("runtime")).unwrap();
        touch(&dir.join("server.lock"));

        write_marker(&dir, &ResetMarker::new(ResetScope::Full)).unwrap();
        assert!(dir.join(RESET_MARKER_FILE).exists());

        assert_eq!(apply_pending_reset(&dir, &dir).unwrap(), true);

        // DB family gone.
        for f in DB_FAMILY {
            assert!(!dir.join(f).exists(), "{f} should be deleted");
        }
        // Derived data gone.
        for d in ["conversations", "attachments", "knowledge", "companion", "cron", "browser-profile"] {
            assert!(!dir.join(d).exists(), "{d} should be deleted");
        }
        // Preserved.
        assert!(dir.join("logs").join("app.log").exists(), "logs must be preserved");
        assert!(dir.join("runtime").exists(), "runtime must be preserved");
        assert!(dir.join("server.lock").exists(), "server.lock must be preserved");
        // Marker cleared → next boot is normal.
        assert!(!dir.join(RESET_MARKER_FILE).exists(), "marker must be cleared");
        assert_eq!(apply_pending_reset(&dir, &dir).unwrap(), false);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn malformed_marker_still_triggers_reset() {
        let dir = std::env::temp_dir().join(format!("nomifun-fr-bad-{}", now_ms()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(RESET_MARKER_FILE), b"not json").unwrap();
        touch(&dir.join("nomifun-backend.db"));

        assert_eq!(apply_pending_reset(&dir, &dir).unwrap(), true);
        assert!(!dir.join("nomifun-backend.db").exists());
        assert!(!dir.join(RESET_MARKER_FILE).exists());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn full_reset_clears_dir_config_override() {
        let dir = std::env::temp_dir().join(format!("nomifun-fr-dircfg-{}", now_ms()));
        std::fs::create_dir_all(&dir).unwrap();
        // A persisted work-dir override must not survive a "reset to defaults".
        std::fs::write(dir.join(crate::dir_config::DIR_CONFIG_FILE), br#"{"work_dir":"/custom/ws"}"#).unwrap();
        write_marker(&dir, &ResetMarker::new(ResetScope::Full)).unwrap();

        assert_eq!(apply_pending_reset(&dir, &dir).unwrap(), true);
        assert!(
            !dir.join(crate::dir_config::DIR_CONFIG_FILE).exists(),
            "dir-config override must be cleared on factory reset"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }
}

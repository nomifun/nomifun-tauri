//! Pre-boot directory configuration: persist the user's chosen working
//! directory so it survives a restart and is readable *before* the in-process
//! backend resolves `work_dir`.
//!
//! Why a file (not the database): under Tauri the backend is linked in-process,
//! and `work_dir` is resolved early in `bootstrap::init_environment` — before
//! the SQLite pool is opened. A config written by the running backend therefore
//! has to land somewhere that the *next* boot can read before any service
//! exists. A small JSON file under `data_dir` fits: `data_dir` is fixed for the
//! lifetime of the install (it does not change when `work_dir` does) and is
//! resolved at the very start of boot.
//!
//! Unlike [`crate::factory_reset`]'s one-shot marker (armed, consumed, deleted),
//! this config is *persistent*: it is kept until the user changes the directory
//! again, so every subsequent boot honors the choice.
//!
//! Flow:
//!   1. `POST /api/system/work-dir` → [`set_work_dir`] writes `dir-config.json`.
//!   2. Frontend relaunches the desktop shell.
//!   3. Next boot → `bootstrap::work_dir::resolve_work_dir` calls
//!      [`persisted_work_dir`] and uses the stored path.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::AppError;

/// Config file under the data dir holding pre-boot directory overrides.
pub const DIR_CONFIG_FILE: &str = "dir-config.json";

/// Persisted pre-boot directory overrides. Optional fields so an absent value
/// means "fall back to the normal resolution"; the struct leaves room to add
/// more pre-boot dirs later without breaking older files.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct DirConfig {
    /// User-chosen conversation workspace root. `None` ⇒ no override.
    #[serde(default)]
    pub work_dir: Option<PathBuf>,
}

fn config_path(data_dir: &Path) -> PathBuf {
    data_dir.join(DIR_CONFIG_FILE)
}

/// Read the persisted dir config. A missing or malformed file yields
/// [`DirConfig::default`] — a broken override must never block boot.
pub fn read(data_dir: &Path) -> DirConfig {
    match std::fs::read(config_path(data_dir)) {
        Ok(bytes) => serde_json::from_slice(&bytes).unwrap_or_default(),
        Err(_) => DirConfig::default(),
    }
}

/// The persisted working directory, if any and usable. Filters out empty and
/// non-absolute paths (a relative override is meaningless this early in boot).
pub fn persisted_work_dir(data_dir: &Path) -> Option<PathBuf> {
    let work_dir = read(data_dir).work_dir?;
    if work_dir.as_os_str().is_empty() || !work_dir.is_absolute() {
        return None;
    }
    Some(work_dir)
}

/// Persist `work_dir` as the pre-boot working-directory override. Read-modify-
/// write so future fields on [`DirConfig`] are preserved.
pub fn set_work_dir(data_dir: &Path, work_dir: &Path) -> Result<(), AppError> {
    let mut config = read(data_dir);
    config.work_dir = Some(work_dir.to_path_buf());
    let json = serde_json::to_vec_pretty(&config)
        .map_err(|e| AppError::Internal(format!("serialize dir-config: {e}")))?;
    std::fs::write(config_path(data_dir), json)
        .map_err(|e| AppError::Internal(format!("write dir-config: {e}")))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::timestamp::now_ms;

    fn temp_data_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("nomifun-dircfg-{tag}-{}", now_ms()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn set_then_read_roundtrips_the_work_dir() {
        let data_dir = temp_data_dir("roundtrip");
        let work_dir = data_dir.join("my-workspace"); // absolute (under temp dir)

        set_work_dir(&data_dir, &work_dir).unwrap();

        assert_eq!(read(&data_dir).work_dir.as_deref(), Some(work_dir.as_path()));
        assert_eq!(persisted_work_dir(&data_dir).as_deref(), Some(work_dir.as_path()));

        let _ = std::fs::remove_dir_all(&data_dir);
    }

    #[test]
    fn missing_file_is_default_and_no_override() {
        let data_dir = temp_data_dir("missing");
        assert!(read(&data_dir).work_dir.is_none());
        assert!(persisted_work_dir(&data_dir).is_none());
        let _ = std::fs::remove_dir_all(&data_dir);
    }

    #[test]
    fn malformed_file_falls_back_to_default() {
        let data_dir = temp_data_dir("malformed");
        std::fs::write(config_path(&data_dir), b"not json at all").unwrap();
        assert!(read(&data_dir).work_dir.is_none());
        assert!(persisted_work_dir(&data_dir).is_none());
        let _ = std::fs::remove_dir_all(&data_dir);
    }

    #[test]
    fn persisted_work_dir_rejects_relative_path() {
        let data_dir = temp_data_dir("relative");
        std::fs::write(config_path(&data_dir), br#"{"work_dir":"relative/ws"}"#).unwrap();
        // read() surfaces the raw stored value, persisted_work_dir() filters it out.
        assert_eq!(read(&data_dir).work_dir, Some(PathBuf::from("relative/ws")));
        assert!(persisted_work_dir(&data_dir).is_none());
        let _ = std::fs::remove_dir_all(&data_dir);
    }

    #[test]
    fn persisted_work_dir_rejects_empty_path() {
        let data_dir = temp_data_dir("empty");
        std::fs::write(config_path(&data_dir), br#"{"work_dir":""}"#).unwrap();
        assert!(persisted_work_dir(&data_dir).is_none());
        let _ = std::fs::remove_dir_all(&data_dir);
    }

    #[test]
    fn set_work_dir_overwrites_previous_value() {
        let data_dir = temp_data_dir("overwrite");
        let first = data_dir.join("ws-a");
        let second = data_dir.join("ws-b");

        set_work_dir(&data_dir, &first).unwrap();
        set_work_dir(&data_dir, &second).unwrap();

        assert_eq!(read(&data_dir).work_dir.as_deref(), Some(second.as_path()));
        let _ = std::fs::remove_dir_all(&data_dir);
    }
}

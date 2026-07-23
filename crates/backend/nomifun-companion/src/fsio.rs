//! Shared file-IO helpers for the companion domain's small JSON config files.
//! Writes use a same-directory temporary file, fsync it, and atomically replace
//! the target. Required documents distinguish absence from corruption so a
//! damaged side store is never silently rewritten from fabricated defaults.

use std::io::Write;
use std::path::Path;

use serde::Serialize;
use serde::de::DeserializeOwned;

/// Atomically persist `value` as pretty JSON to `{dir}/{file}`: write a
/// securely-created same-directory temp file, fsync it, then atomically replace
/// the target. `tempfile::persist` uses replacement semantics on both Unix and
/// Windows; dropping a failed temp file removes it.
pub(crate) fn save_json_atomic(dir: &Path, file: &str, value: &impl Serialize) -> std::io::Result<()> {
    let raw = serde_json::to_vec_pretty(value)
        .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))?;
    save_bytes_atomic(dir, file, &raw)
}

/// Atomically persist raw `bytes` to `{dir}/{file}`.
pub(crate) fn save_bytes_atomic(dir: &Path, file: &str, bytes: &[u8]) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)?;
    let path = dir.join(file);
    let mut tmp = tempfile::Builder::new()
        .prefix(&format!(".{file}.tmp."))
        .tempfile_in(dir)?;
    tmp.write_all(bytes)?;
    tmp.as_file_mut().sync_all()?;
    tmp.persist(&path).map(|_| ()).map_err(|error| error.error)?;
    sync_dir(dir)
}

#[cfg(unix)]
pub(crate) fn sync_dir(dir: &Path) -> std::io::Result<()> {
    std::fs::File::open(dir)?.sync_all()
}

#[cfg(not(unix))]
pub(crate) fn sync_dir(_dir: &Path) -> std::io::Result<()> {
    Ok(())
}

/// Remove one managed file or directory entry without following symlinks.
/// Missing entries are idempotently successful.
pub(crate) fn remove_path_entry(path: &Path) -> std::io::Result<()> {
    let metadata = match std::fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
    };
    if metadata.file_type().is_symlink() || metadata.is_file() {
        std::fs::remove_file(path)
    } else if metadata.is_dir() {
        std::fs::remove_dir_all(path)
    } else {
        Err(std::io::Error::other(format!(
            "unsupported filesystem entry type: {}",
            path.display()
        )))
    }?;
    if let Some(parent) = path.parent() {
        sync_dir(parent)?;
    }
    Ok(())
}

/// Load an optional JSON document. Only `NotFound` maps to `None`; unreadable
/// or malformed data is an error.
pub(crate) fn load_json_optional<T: DeserializeOwned>(path: &Path) -> std::io::Result<Option<T>> {
    let raw = match std::fs::read(path) {
        Ok(raw) => raw,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    serde_json::from_slice(&raw)
        .map(Some)
        .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))
}

/// Load optional state whose absence has a real default. Corrupt or unreadable
/// files do not use that default.
pub(crate) fn load_json_missing_or_default<T: DeserializeOwned + Default>(
    path: &Path,
) -> std::io::Result<T> {
    Ok(load_json_optional(path)?.unwrap_or_default())
}

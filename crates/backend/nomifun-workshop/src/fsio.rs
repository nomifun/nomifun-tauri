//! Atomic temp+rename writes for the workshop domain's on-disk artifacts
//! (canvas docs + asset binaries). Mirrors `nomifun-public-agent::fsio` but is
//! async (asset payloads can be tens of MB, so blocking the runtime on a sync
//! write is unacceptable) and byte-oriented (docs/binaries, not typed configs).

use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};

static SAVE_SEQ: AtomicU64 = AtomicU64::new(0);

/// Atomically persist `bytes` to `{dir}/{file}` (unique-temp + rename). Creates
/// `dir` if missing. On any failure the temp file is best-effort removed.
pub(crate) async fn save_bytes_atomic(dir: &Path, file: &str, bytes: &[u8]) -> std::io::Result<()> {
    tokio::fs::create_dir_all(dir).await?;
    let path = dir.join(file);
    let seq = SAVE_SEQ.fetch_add(1, Ordering::Relaxed);
    let tmp = dir.join(format!(".{file}.tmp.{}.{seq}", std::process::id()));
    let result = async {
        tokio::fs::write(&tmp, bytes).await?;
        tokio::fs::rename(&tmp, &path).await
    }
    .await;
    if result.is_err() {
        let _ = tokio::fs::remove_file(&tmp).await;
    }
    result
}

/// Read a file to bytes, or `None` when it does not exist. Other IO errors
/// propagate.
pub(crate) async fn read_bytes_opt(path: &Path) -> std::io::Result<Option<Vec<u8>>> {
    match tokio::fs::read(path).await {
        Ok(bytes) => Ok(Some(bytes)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn atomic_write_then_read_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let sub = dir.path().join("a").join("b");
        save_bytes_atomic(&sub, "x.bin", b"hello").await.unwrap();
        let read = read_bytes_opt(&sub.join("x.bin")).await.unwrap();
        assert_eq!(read.as_deref(), Some(&b"hello"[..]));
        // no temp files linger
        let leftover = std::fs::read_dir(&sub).unwrap().filter(|e| {
            e.as_ref().unwrap().file_name().to_string_lossy().contains(".tmp.")
        });
        assert_eq!(leftover.count(), 0);
    }

    #[tokio::test]
    async fn read_missing_is_none() {
        let dir = tempfile::tempdir().unwrap();
        assert!(read_bytes_opt(&dir.path().join("nope")).await.unwrap().is_none());
    }
}

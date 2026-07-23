//! MODNet matting-model proxy: download the ML cutout model **once** from an
//! upstream mirror, cache it on disk, and let the webview fetch it from the
//! local backend (`GET /api/companion/matting-model`).
//!
//! Why this exists — the DIY custom-figure flow was "根本用不了": the renderer
//! used to lazy-download the 25 MB model directly from `huggingface.co` inside
//! the matting Web Worker, wrapped in a 30 s timeout that also covered the
//! download. The download alone takes ~36 s on a good connection (and never
//! completes behind the GFW), so the first attempt always timed out, fell back
//! to heuristic flood-fill, and dead-ended any real photo at `MATTE_FAILED`.
//!
//! Moving acquisition to the backend fixes the root cause: the webview always
//! reaches `127.0.0.1` (no remote origin, no CORS, no GFW for the local hop),
//! the model is fetched once and persisted to disk (survives restarts), and the
//! upstream fetch can try a China-friendly mirror before huggingface.

use std::path::{Path, PathBuf};

use nomifun_common::AppError;
use std::io::Write;
use tokio::sync::Mutex;

/// Cached model filename under `{data_dir}/companion/models/`.
pub const MODEL_FILENAME: &str = "modnet.onnx";

/// Upstream sources, tried in order. `hf-mirror.com` is the standard
/// China-friendly HuggingFace mirror; `huggingface.co` is the canonical
/// fallback for everyone else. Same path on both.
const UPSTREAMS: &[&str] = &[
    "https://hf-mirror.com/Xenova/modnet/resolve/main/onnx/model.onnx",
    "https://huggingface.co/Xenova/modnet/resolve/main/onnx/model.onnx",
];

/// Sanity floor: the real model is ~25 MB. Anything smaller is an error page
/// or a truncated transfer — reject it so we don't cache garbage that bricks
/// inference forever.
const MIN_VALID_BYTES: u64 = 8 * 1024 * 1024;
/// Ceiling guard against a mirror that streams something absurd.
const MAX_VALID_BYTES: u64 = 64 * 1024 * 1024;

/// Connect timeout per upstream attempt. The *total* transfer is intentionally
/// uncapped — a slow 25 MB download must be allowed to finish (that uncapped
/// completion is the whole point of moving it off the worker's 30 s timer).
const CONNECT_TIMEOUT_SECS: u64 = 15;

fn is_valid_size(len: u64) -> bool {
    (MIN_VALID_BYTES..=MAX_VALID_BYTES).contains(&len)
}

/// Return the on-disk path to the cached model, downloading it from an upstream
/// mirror on first use. Concurrency-safe: a `lock` serializes first-time
/// downloads so N concurrent callers trigger exactly one fetch (double-checked
/// against the disk both before and after acquiring the lock).
pub async fn ensure_model(models_dir: &Path, lock: &Mutex<()>) -> Result<PathBuf, AppError> {
    let path = models_dir.join(MODEL_FILENAME);

    // Fast path: already cached and plausibly intact — no lock, no network.
    if let Ok(meta) = tokio::fs::metadata(&path).await
        && is_valid_size(meta.len())
    {
        return Ok(path);
    }

    // Slow path: serialize so concurrent first-hits share one download.
    let _guard = lock.lock().await;
    // Re-check under the lock: a racing caller may have just finished.
    if let Ok(meta) = tokio::fs::metadata(&path).await
        && is_valid_size(meta.len())
    {
        return Ok(path);
    }

    tokio::fs::create_dir_all(models_dir)
        .await
        .map_err(|e| AppError::Internal(format!("create models dir: {e}")))?;

    let client = reqwest::Client::builder()
        .connect_timeout(std::time::Duration::from_secs(CONNECT_TIMEOUT_SECS))
        .build()
        .map_err(|e| AppError::Internal(format!("build http client: {e}")))?;

    let mut last_err = String::from("no upstream attempted");
    for url in UPSTREAMS {
        match download_one(&client, url).await {
            Ok(bytes) if is_valid_size(bytes.len() as u64) => {
                write_atomic(&path, &bytes).await?;
                tracing::info!(url, bytes = bytes.len(), "matting model cached");
                return Ok(path);
            }
            Ok(bytes) => {
                last_err = format!("{url}: implausible size {} bytes", bytes.len());
                tracing::warn!(url, bytes = bytes.len(), "matting model upstream returned implausible size; trying next");
            }
            Err(e) => {
                last_err = format!("{url}: {e}");
                tracing::warn!(url, error = %e, "matting model upstream failed; trying next");
            }
        }
    }

    Err(AppError::Internal(format!(
        "无法获取抠图模型(所有上游均失败): {last_err}"
    )))
}

async fn download_one(client: &reqwest::Client, url: &str) -> Result<Vec<u8>, String> {
    let res = client.get(url).send().await.map_err(|e| e.to_string())?;
    if !res.status().is_success() {
        return Err(format!("HTTP {}", res.status()));
    }
    res.bytes().await.map(|b| b.to_vec()).map_err(|e| e.to_string())
}

/// Write to a securely-created same-directory tempfile, fsync it, atomically
/// replace the cache entry, then fsync the parent directory. A crash or
/// concurrent process can never expose a partial model or collide on a fixed
/// `.partial` name.
async fn write_atomic(path: &Path, bytes: &[u8]) -> Result<(), AppError> {
    let path = path.to_path_buf();
    let bytes = bytes.to_vec();
    tokio::task::spawn_blocking(move || {
        let parent = path
            .parent()
            .ok_or_else(|| AppError::Internal("matting model path has no parent".into()))?;
        std::fs::create_dir_all(parent)
            .map_err(|error| AppError::Internal(format!("create models dir: {error}")))?;
        let mut temp = tempfile::Builder::new()
            .prefix(".modnet.onnx.tmp.")
            .tempfile_in(parent)
            .map_err(|error| AppError::Internal(format!("create model tempfile: {error}")))?;
        temp.write_all(&bytes)
            .and_then(|_| temp.as_file_mut().sync_all())
            .map_err(|error| AppError::Internal(format!("write model tempfile: {error}")))?;
        temp.persist(&path)
            .map_err(|error| AppError::Internal(format!("commit model: {}", error.error)))?;
        crate::fsio::sync_dir(parent)
            .map_err(|error| AppError::Internal(format!("fsync models dir: {error}")))
    })
    .await
    .map_err(|error| AppError::Internal(format!("model write task failed: {error}")))?
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn size_validation_bounds() {
        assert!(!is_valid_size(0));
        assert!(!is_valid_size(1024)); // an error page
        assert!(is_valid_size(25 * 1024 * 1024)); // the real model
        assert!(!is_valid_size(128 * 1024 * 1024)); // absurd
    }

    #[tokio::test]
    async fn ensure_returns_cached_without_network() {
        let dir = tempfile::tempdir().unwrap();
        let models = dir.path().join("models");
        std::fs::create_dir_all(&models).unwrap();
        // Seed a plausibly-sized file so ensure_model takes the fast path and
        // never touches the network.
        let path = models.join(MODEL_FILENAME);
        std::fs::write(&path, vec![0u8; MIN_VALID_BYTES as usize + 1]).unwrap();
        let lock = Mutex::new(());
        let got = ensure_model(&models, &lock).await.unwrap();
        assert_eq!(got, path);
    }

    #[tokio::test]
    async fn ensure_ignores_undersized_cache_and_then_fails_offline_cleanly() {
        let dir = tempfile::tempdir().unwrap();
        let models = dir.path().join("models");
        std::fs::create_dir_all(&models).unwrap();
        // A truncated/garbage cache must NOT be served; with no network the
        // call fails cleanly (it does not return the bad file).
        std::fs::write(models.join(MODEL_FILENAME), b"not a model").unwrap();
        let lock = Mutex::new(());
        // We can't guarantee offline in CI, so only assert the undersized file
        // is never returned as-is: either it re-downloads a valid model, or it
        // errors — but it never returns the 11-byte path content.
        if let Ok(p) = ensure_model(&models, &lock).await {
            let len = std::fs::metadata(&p).unwrap().len();
            assert!(is_valid_size(len), "must not serve undersized cache");
        }
    }
}

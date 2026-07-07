//! Built-in integration of the upstream [`obra/superpowers`](https://github.com/obra/superpowers)
//! skills library.
//!
//! superpowers is a collection of methodology skills (`SKILL.md` files: TDD,
//! systematic-debugging, brainstorming, …). We embed the upstream corpus at
//! build time (the offline *baseline*) and optionally refresh it at runtime
//! from GitHub releases (the *overlay*). The "effective" directory prefers the
//! overlay and falls back to the baseline, so the feature works offline out of
//! the box and never degrades when a download fails.
//!
//! Design: `docs/superpowers/specs/2026-07-07-superpowers-integration-design.md`.

use std::path::{Path, PathBuf};

use include_dir::{Dir, include_dir};
use sha2::{Digest, Sha256};
use tracing::warn;

use crate::error::ExtensionError;
use crate::startup_materialize::{MaterializeLockGuard, commit_staging_dir, write_dir_recursive};

/// Embedded upstream superpowers corpus — the offline baseline. Contains the
/// 14 skill directories plus `LICENSE` and `VERSION`.
static SUPERPOWERS: Dir<'static> = include_dir!("$CARGO_MANIFEST_DIR/assets/superpowers");

/// Upstream release tag of the embedded baseline corpus (e.g. `6.0.3`).
pub const SUPERPOWERS_BUNDLED_VERSION: &str = include_str!("../../assets/superpowers/VERSION");

/// Expose the embedded superpowers corpus for startup materialization.
/// Consumers outside this crate should not depend on `include_dir` directly.
pub fn superpowers_corpus() -> &'static Dir<'static> {
    &SUPERPOWERS
}

/// Deterministic SHA-256 fingerprint of the embedded superpowers corpus.
/// Same scheme as [`crate::skill_service::builtin_skills_corpus_fingerprint`]:
/// sorted `(relative_path, contents)` pairs, NUL-separated, lowercase hex.
pub fn superpowers_corpus_fingerprint() -> String {
    let mut files: Vec<(String, &'static [u8])> = Vec::new();
    collect_corpus_files(&SUPERPOWERS, &mut files);
    files.sort_by(|a, b| a.0.cmp(&b.0));

    let mut hasher = Sha256::new();
    for (path, contents) in files {
        hasher.update(path.as_bytes());
        hasher.update([0]);
        hasher.update(contents);
        hasher.update([0]);
    }
    format!("{:x}", hasher.finalize())
}

fn collect_corpus_files(dir: &'static Dir<'static>, out: &mut Vec<(String, &'static [u8])>) {
    for file in dir.files() {
        out.push((file.path().to_string_lossy().into_owned(), file.contents()));
    }
    for subdir in dir.dirs() {
        collect_corpus_files(subdir, out);
    }
}

// ---------------------------------------------------------------------------
// Baseline materialization + effective directory resolution
// ---------------------------------------------------------------------------

/// Directory (under the app data dir) holding the materialized embedded
/// baseline corpus.
pub const SUPERPOWERS_BASELINE_DIR: &str = "superpowers-baseline";
/// Directory (under the app data dir) holding the hot-updated overlay corpus,
/// when present. Takes precedence over the baseline.
pub const SUPERPOWERS_OVERLAY_DIR: &str = "superpowers";

const BASELINE_STAGING: &str = ".superpowers-baseline.tmp";
const BASELINE_OLD: &str = ".superpowers-baseline.old";
const BASELINE_LOCK: &str = ".superpowers-baseline.lock";
const VERSION_FILE: &str = ".version";

/// Materialization stamp for the baseline: upstream tag + content fingerprint,
/// so asset-only edits refresh `{data_dir}/superpowers-baseline` even when the
/// upstream tag is unchanged during development.
pub fn superpowers_baseline_version() -> String {
    format!(
        "{}+sp.{}",
        SUPERPOWERS_BUNDLED_VERSION.trim(),
        &superpowers_corpus_fingerprint()[..12]
    )
}

/// Materialize the embedded baseline corpus to `{data_dir}/superpowers-baseline`.
/// Gated on a `.version` file (like the builtin-skills corpus): returns
/// `Ok(true)` if a write happened, `Ok(false)` if the gate said "up to date".
/// On refresh failure with a usable existing tree, keeps the old tree and
/// returns `Ok(false)` rather than erroring.
pub async fn materialize_superpowers_baseline(data_dir: &Path) -> Result<bool, ExtensionError> {
    let target = data_dir.join(SUPERPOWERS_BASELINE_DIR);
    let version = superpowers_baseline_version();

    if version_file_matches(&target, &version).await {
        return Ok(false);
    }
    let _guard = MaterializeLockGuard::acquire_named(data_dir, BASELINE_LOCK).await?;
    if version_file_matches(&target, &version).await {
        return Ok(false);
    }

    match materialize_baseline_unlocked(data_dir, &version).await {
        Ok(()) => Ok(true),
        Err(e) if baseline_looks_usable(&target).await => {
            warn!(error = %e, target = %target.display(), "superpowers baseline refresh failed; keeping existing tree");
            Ok(false)
        }
        Err(e) => Err(e),
    }
}

async fn materialize_baseline_unlocked(data_dir: &Path, version: &str) -> Result<(), ExtensionError> {
    let target = data_dir.join(SUPERPOWERS_BASELINE_DIR);
    let staging = data_dir.join(BASELINE_STAGING);
    let old = data_dir.join(BASELINE_OLD);

    tokio::fs::create_dir_all(data_dir).await?;
    if staging.exists() {
        let _ = tokio::fs::remove_dir_all(&staging).await;
    }
    tokio::fs::create_dir_all(&staging).await?;
    write_dir_recursive(superpowers_corpus(), &staging).await?;
    tokio::fs::write(staging.join(VERSION_FILE), version).await?;
    commit_staging_dir(&target, &staging, &old).await
}

async fn version_file_matches(target: &Path, version: &str) -> bool {
    match tokio::fs::read_to_string(target.join(VERSION_FILE)).await {
        Ok(s) => s == version,
        Err(_) => false,
    }
}

async fn baseline_looks_usable(target: &Path) -> bool {
    target.is_dir()
        && tokio::fs::metadata(target.join(VERSION_FILE))
            .await
            .map(|m| m.is_file())
            .unwrap_or(false)
}

/// The directory whose immediate children are superpowers skill directories.
/// Prefers a populated hot-updated overlay (`{data_dir}/superpowers`), else the
/// embedded baseline. Returns the baseline path even if neither exists yet
/// (the caller materializes the baseline at startup). This is the path fed to
/// the nomi engine's `extra_skill_dirs` and linked into ACP workspaces.
pub fn effective_superpowers_dir(data_dir: &Path) -> PathBuf {
    let overlay = data_dir.join(SUPERPOWERS_OVERLAY_DIR);
    if dir_has_skill(&overlay) {
        return overlay;
    }
    data_dir.join(SUPERPOWERS_BASELINE_DIR)
}

/// True if `dir` contains at least one immediate subdirectory with a
/// `SKILL.md` — i.e. it holds at least one real skill.
fn dir_has_skill(dir: &Path) -> bool {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return false;
    };
    for entry in entries.flatten() {
        if entry.path().join("SKILL.md").is_file() {
            return true;
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn corpus_contains_core_skills() {
        let mut files = Vec::new();
        collect_corpus_files(&SUPERPOWERS, &mut files);
        let paths: Vec<String> = files.iter().map(|(p, _)| p.replace('\\', "/")).collect();

        assert!(
            paths.iter().any(|p| p.ends_with("using-superpowers/SKILL.md")),
            "using-superpowers bootstrap must be embedded; got {paths:?}"
        );
        assert!(
            paths.iter().any(|p| p.ends_with("test-driven-development/SKILL.md")),
            "tdd skill must be embedded"
        );
        assert!(
            paths.iter().any(|p| p.ends_with("systematic-debugging/SKILL.md")),
            "systematic-debugging skill must be embedded"
        );
        assert_eq!(
            paths.iter().filter(|p| p.ends_with("SKILL.md")).count(),
            14,
            "all 14 upstream skills must be embedded"
        );
    }

    #[test]
    fn fingerprint_is_stable_hex() {
        let fp = superpowers_corpus_fingerprint();
        assert_eq!(fp.len(), 64, "sha-256 hex length");
        assert!(fp.chars().all(|c| c.is_ascii_hexdigit()));
        assert_eq!(fp, superpowers_corpus_fingerprint(), "fingerprint deterministic");
    }

    #[test]
    fn bundled_version_nonempty() {
        assert!(!SUPERPOWERS_BUNDLED_VERSION.trim().is_empty());
    }

    #[tokio::test]
    async fn baseline_materializes_and_gates() {
        let tmp = TempDir::new().unwrap();
        let wrote = materialize_superpowers_baseline(tmp.path()).await.unwrap();
        assert!(wrote, "first materialize should write");

        let baseline = tmp.path().join(SUPERPOWERS_BASELINE_DIR);
        assert!(
            baseline.join("using-superpowers").join("SKILL.md").is_file(),
            "baseline must contain the using-superpowers bootstrap"
        );
        assert_eq!(
            std::fs::read_to_string(baseline.join(VERSION_FILE)).unwrap(),
            superpowers_baseline_version()
        );

        let wrote_again = materialize_superpowers_baseline(tmp.path()).await.unwrap();
        assert!(!wrote_again, "version gate should skip the second materialize");
    }

    #[tokio::test]
    async fn effective_dir_prefers_populated_overlay() {
        let tmp = TempDir::new().unwrap();
        materialize_superpowers_baseline(tmp.path()).await.unwrap();

        // No overlay yet → baseline.
        assert_eq!(
            effective_superpowers_dir(tmp.path()),
            tmp.path().join(SUPERPOWERS_BASELINE_DIR)
        );

        // A populated overlay (has a skill dir) wins.
        let overlay_skill = tmp.path().join(SUPERPOWERS_OVERLAY_DIR).join("test-driven-development");
        tokio::fs::create_dir_all(&overlay_skill).await.unwrap();
        tokio::fs::write(overlay_skill.join("SKILL.md"), b"---\nname: x\n---\n")
            .await
            .unwrap();
        assert_eq!(
            effective_superpowers_dir(tmp.path()),
            tmp.path().join(SUPERPOWERS_OVERLAY_DIR)
        );
    }

    #[tokio::test]
    async fn effective_dir_ignores_empty_overlay() {
        let tmp = TempDir::new().unwrap();
        // An empty overlay dir (no skill inside) must not shadow the baseline.
        tokio::fs::create_dir_all(tmp.path().join(SUPERPOWERS_OVERLAY_DIR))
            .await
            .unwrap();
        assert_eq!(
            effective_superpowers_dir(tmp.path()),
            tmp.path().join(SUPERPOWERS_BASELINE_DIR)
        );
    }
}

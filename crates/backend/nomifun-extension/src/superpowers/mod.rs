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

pub mod update;

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
    // Skills are materialized under `.nomi/skills/` so that when this root is
    // passed to the nomi loader as an extra skill dir, its `--add-dir`
    // expansion (`<root>/.nomi/skills`) discovers them. Placing them at the
    // top level would make the loader look in a non-existent `.nomi/skills`
    // and silently load nothing.
    let skills_dir = staging.join(".nomi").join("skills");
    tokio::fs::create_dir_all(&skills_dir).await?;
    write_dir_recursive(superpowers_corpus(), &skills_dir).await?;
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

/// The superpowers skill root to feed the nomi engine (`extra_skill_dirs`) and
/// link into ACP workspaces. Its skills live under `<root>/.nomi/skills/`.
///
/// Prefers a populated hot-updated overlay (`{data_dir}/superpowers`), but only
/// when the overlay is not strictly older than the embedded baseline — so an
/// app upgrade that ships a newer baseline is never shadowed by a stale overlay.
/// Falls back to the embedded baseline (`{data_dir}/superpowers-baseline`), whose
/// path is returned even if neither exists yet (the caller materializes the
/// baseline at startup).
pub fn effective_superpowers_dir(data_dir: &Path) -> PathBuf {
    let overlay = data_dir.join(SUPERPOWERS_OVERLAY_DIR);
    if root_has_superpowers_skills(&overlay) {
        let overlay_version = read_root_version(&overlay).unwrap_or_default();
        if overlay_not_older_than(&overlay_version, SUPERPOWERS_BUNDLED_VERSION.trim()) {
            return overlay;
        }
    }
    data_dir.join(SUPERPOWERS_BASELINE_DIR)
}

/// True if `root` holds superpowers skills in its `.nomi/skills/` layout (i.e.
/// at least one subdirectory there has a `SKILL.md`).
fn root_has_superpowers_skills(root: &Path) -> bool {
    let skills = root.join(".nomi").join("skills");
    let Ok(entries) = std::fs::read_dir(&skills) else {
        return false;
    };
    entries.flatten().any(|e| e.path().join("SKILL.md").is_file())
}

/// Read a root's `.version` stamp, if present and non-empty.
fn read_root_version(root: &Path) -> Option<String> {
    std::fs::read_to_string(root.join(VERSION_FILE))
        .ok()
        .map(|s| s.trim().to_owned())
        .filter(|s| !s.is_empty())
}

/// Whether an overlay of version `overlay` should still win over the embedded
/// `baseline` version. True unless the baseline is strictly newer (semver;
/// build metadata ignored). Non-semver versions keep the overlay.
fn overlay_not_older_than(overlay: &str, baseline: &str) -> bool {
    match (semver::Version::parse(overlay), semver::Version::parse(baseline)) {
        (Ok(o), Ok(b)) => o >= b,
        _ => true,
    }
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
            baseline
                .join(".nomi")
                .join("skills")
                .join("using-superpowers")
                .join("SKILL.md")
                .is_file(),
            "baseline must expose skills under .nomi/skills so the nomi loader finds them"
        );
        assert_eq!(
            std::fs::read_to_string(baseline.join(VERSION_FILE)).unwrap(),
            superpowers_baseline_version()
        );

        let wrote_again = materialize_superpowers_baseline(tmp.path()).await.unwrap();
        assert!(!wrote_again, "version gate should skip the second materialize");
    }

    /// Write a fake overlay skill (under the `.nomi/skills/` layout) with a
    /// `.version` stamp.
    async fn seed_overlay(root: &std::path::Path, version: &str) {
        let skill = root.join(".nomi").join("skills").join("test-driven-development");
        tokio::fs::create_dir_all(&skill).await.unwrap();
        tokio::fs::write(skill.join("SKILL.md"), b"---\nname: x\n---\n")
            .await
            .unwrap();
        tokio::fs::write(root.join(VERSION_FILE), version).await.unwrap();
    }

    #[tokio::test]
    async fn effective_dir_prefers_overlay_when_not_older() {
        let tmp = TempDir::new().unwrap();
        materialize_superpowers_baseline(tmp.path()).await.unwrap();

        // No overlay yet → baseline.
        assert_eq!(
            effective_superpowers_dir(tmp.path()),
            tmp.path().join(SUPERPOWERS_BASELINE_DIR)
        );

        // A populated overlay newer than the baseline wins.
        seed_overlay(&tmp.path().join(SUPERPOWERS_OVERLAY_DIR), "999.0.0").await;
        assert_eq!(
            effective_superpowers_dir(tmp.path()),
            tmp.path().join(SUPERPOWERS_OVERLAY_DIR)
        );
    }

    #[tokio::test]
    async fn effective_dir_falls_back_when_overlay_older_than_baseline() {
        let tmp = TempDir::new().unwrap();
        // Overlay has skills but is older than the embedded baseline (e.g. the
        // app upgraded past a stale hot-updated overlay) → baseline must win.
        seed_overlay(&tmp.path().join(SUPERPOWERS_OVERLAY_DIR), "0.0.1").await;
        assert_eq!(
            effective_superpowers_dir(tmp.path()),
            tmp.path().join(SUPERPOWERS_BASELINE_DIR),
            "a stale overlay must not shadow a newer embedded baseline"
        );
    }

    #[tokio::test]
    async fn effective_dir_ignores_empty_overlay() {
        let tmp = TempDir::new().unwrap();
        // An empty overlay dir (no .nomi/skills) must not shadow the baseline.
        tokio::fs::create_dir_all(tmp.path().join(SUPERPOWERS_OVERLAY_DIR))
            .await
            .unwrap();
        assert_eq!(
            effective_superpowers_dir(tmp.path()),
            tmp.path().join(SUPERPOWERS_BASELINE_DIR)
        );
    }
}

//! Hot-update the superpowers overlay from a GitHub release.
//!
//! Downloads the upstream release zip (proxy-aware, host-allowlisted, size- and
//! time-bounded), verifies its integrity (optional sha256), safely extracts it,
//! locates the `skills/` tree inside the archive, and atomically swaps it into
//! `{data_dir}/superpowers` (the overlay that [`super::effective_superpowers_dir`]
//! prefers over the embedded baseline). Every failure leaves the existing overlay
//! untouched — a bad download can never degrade a working install.

use std::path::{Path, PathBuf};
use std::time::Duration;

use sha2::{Digest, Sha256};
use tracing::info;

use super::{SUPERPOWERS_OVERLAY_DIR, VERSION_FILE};
use crate::error::ExtensionError;
use crate::startup_materialize::{MaterializeLockGuard, commit_staging_dir};

/// GitHub repository the superpowers corpus is refreshed from. Overridable via
/// `NOMIFUN_SUPERPOWERS_REPO` (for forks / testing).
pub const SUPERPOWERS_REPO_DEFAULT: &str = "obra/superpowers";

const OVERLAY_LOCK: &str = ".superpowers.lock";
const OVERLAY_STAGING: &str = ".superpowers.tmp";
const OVERLAY_OLD: &str = ".superpowers.old";
const DL_TMP: &str = ".superpowers-dl.tmp";

/// Hosts a superpowers download may originate from (initial URL; GitHub redirects
/// release/zipball URLs to codeload/objects, which reqwest follows automatically).
const ALLOWED_HOSTS: &[&str] = &[
    "github.com",
    "api.github.com",
    "codeload.github.com",
    "objects.githubusercontent.com",
    "raw.githubusercontent.com",
];

const MAX_DOWNLOAD_BYTES: usize = 50 * 1024 * 1024;
const DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(120);

/// A resolved superpowers release available for install.
#[derive(Debug, Clone)]
pub struct SuperpowersRelease {
    /// Upstream release tag / version (e.g. `6.0.4`).
    pub version: String,
    /// URL of the release zip (GitHub `zipball_url` or an attached `.zip` asset).
    pub zip_url: String,
    /// Optional lowercase-hex SHA-256 of the zip for integrity verification.
    pub sha256: Option<String>,
}

/// The superpowers repo slug, honoring the `NOMIFUN_SUPERPOWERS_REPO` override.
pub fn superpowers_repo() -> String {
    std::env::var("NOMIFUN_SUPERPOWERS_REPO")
        .ok()
        .map(|v| v.trim().to_owned())
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| SUPERPOWERS_REPO_DEFAULT.to_owned())
}

/// Version stamp of the currently-installed overlay, if any
/// (`{data_dir}/superpowers/.version`). Used by the updater to decide whether a
/// fetched release is newer than what is already installed.
pub fn installed_overlay_version(data_dir: &Path) -> Option<String> {
    std::fs::read_to_string(data_dir.join(SUPERPOWERS_OVERLAY_DIR).join(VERSION_FILE))
        .ok()
        .map(|s| s.trim().to_owned())
        .filter(|s| !s.is_empty())
}

/// Download `release` and install it as the superpowers overlay. Builds its own
/// proxy-aware HTTP client. Any failure is returned without disturbing the
/// existing overlay.
pub async fn install_superpowers_overlay(
    data_dir: &Path,
    release: &SuperpowersRelease,
) -> Result<(), ExtensionError> {
    let bytes = fetch_bytes(&release.zip_url).await?;
    install_superpowers_overlay_from_bytes(data_dir, &release.version, &bytes, release.sha256.as_deref()).await?;
    info!(version = %release.version, "superpowers overlay updated");
    Ok(())
}

/// Query GitHub for the latest superpowers release. Builds its own proxy-aware
/// client. Returns the resolved [`SuperpowersRelease`] (zipball URL, no sha256 —
/// GitHub does not publish a digest for source zipballs).
pub async fn fetch_latest_release() -> Result<SuperpowersRelease, ExtensionError> {
    let repo = superpowers_repo();
    let url = format!("https://api.github.com/repos/{repo}/releases/latest");
    let bytes = fetch_bytes(&url).await?;
    parse_latest_release(&bytes)
}

/// Parse the GitHub `releases/latest` JSON payload into a [`SuperpowersRelease`].
/// Pure — unit-testable without the network. Extracts `tag_name` (with any
/// leading `v` stripped for clean version comparison) and `zipball_url`.
pub fn parse_latest_release(bytes: &[u8]) -> Result<SuperpowersRelease, ExtensionError> {
    let json: serde_json::Value = serde_json::from_slice(bytes)
        .map_err(|e| ExtensionError::Download(format!("parsing GitHub release JSON: {e}")))?;
    let tag = json
        .get("tag_name")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| ExtensionError::Download("release JSON missing tag_name".into()))?;
    let zip_url = json
        .get("zipball_url")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| ExtensionError::Download("release JSON missing zipball_url".into()))?;
    let version = tag
        .strip_prefix('v')
        .or_else(|| tag.strip_prefix('V'))
        .unwrap_or(tag)
        .to_owned();
    Ok(SuperpowersRelease {
        version,
        zip_url: zip_url.to_owned(),
        sha256: None,
    })
}

/// Decide whether a fetched release `latest` should replace the current
/// effective version `current`. Prefers semver (install only on a strictly newer
/// version, ignoring build metadata); falls back to plain inequality when either
/// side is not valid semver. Empty `latest` never installs.
pub fn should_install_release(latest: &str, current: &str) -> bool {
    let (latest, current) = (latest.trim(), current.trim());
    if latest.is_empty() {
        return false;
    }
    match (semver::Version::parse(latest), semver::Version::parse(current)) {
        (Ok(l), Ok(c)) => l > c,
        _ => latest != current,
    }
}

/// Fetch `url` into memory with host allowlisting, a timeout, and a size cap.
async fn fetch_bytes(url: &str) -> Result<Vec<u8>, ExtensionError> {
    if !host_allowed(url) {
        return Err(ExtensionError::Verify(format!("download host not allowlisted: {url}")));
    }
    let client = nomifun_net::http_client();
    let resp = tokio::time::timeout(
        DOWNLOAD_TIMEOUT,
        client.get(url).header("User-Agent", "nomifun").send(),
    )
    .await
    .map_err(|_| ExtensionError::Download(format!("timeout requesting {url}")))?
    .map_err(|e| ExtensionError::Download(format!("request failed for {url}: {e}")))?;

    if !resp.status().is_success() {
        return Err(ExtensionError::Download(format!("HTTP {} for {url}", resp.status())));
    }

    let bytes = tokio::time::timeout(DOWNLOAD_TIMEOUT, resp.bytes())
        .await
        .map_err(|_| ExtensionError::Download(format!("timeout reading body of {url}")))?
        .map_err(|e| ExtensionError::Download(format!("reading body of {url}: {e}")))?;

    if bytes.len() > MAX_DOWNLOAD_BYTES {
        return Err(ExtensionError::Download(format!(
            "archive too large: {} bytes (max {MAX_DOWNLOAD_BYTES})",
            bytes.len()
        )));
    }
    Ok(bytes.to_vec())
}

/// Install a superpowers overlay from already-fetched zip bytes. The testable
/// core: no network. Verifies sha256 (if given), safely extracts, locates the
/// skills tree, and atomically swaps it into `{data_dir}/superpowers`.
pub(crate) async fn install_superpowers_overlay_from_bytes(
    data_dir: &Path,
    version: &str,
    zip_bytes: &[u8],
    expected_sha256: Option<&str>,
) -> Result<(), ExtensionError> {
    // Integrity first — before touching the filesystem, so a bad archive can
    // never disturb the existing overlay.
    if let Some(expected) = expected_sha256 {
        verify_sha256(zip_bytes, expected)?;
    }

    let _guard = MaterializeLockGuard::acquire_named(data_dir, OVERLAY_LOCK).await?;

    tokio::fs::create_dir_all(data_dir).await?;
    let dl_tmp = data_dir.join(DL_TMP);
    let overlay_staging = data_dir.join(OVERLAY_STAGING);
    let overlay_old = data_dir.join(OVERLAY_OLD);
    let target = data_dir.join(SUPERPOWERS_OVERLAY_DIR);

    // Fresh temps (tolerate leftovers from a crashed run).
    for p in [&dl_tmp, &overlay_staging] {
        if p.exists() {
            let _ = tokio::fs::remove_dir_all(p).await;
        }
    }
    tokio::fs::create_dir_all(&dl_tmp).await?;

    // Write + extract the archive off the reactor.
    let zip_path = dl_tmp.join("archive.zip");
    tokio::fs::write(&zip_path, zip_bytes).await?;
    let extract_dir = dl_tmp.join("extract");
    {
        let zp = zip_path.clone();
        let ed = extract_dir.clone();
        tokio::task::spawn_blocking(move || crate::zip_safe::extract_zip_archive(&zp, &ed))
            .await
            .map_err(|e| ExtensionError::Download(format!("extract task failed: {e}")))??;
    }

    // Locate the skills tree inside the archive (GitHub zipball wraps it in
    // `<repo>-<sha>/skills/`), and lay it out under the overlay's `.nomi/skills/`
    // (the layout the nomi loader expects from an extra skill root), stamping the
    // version at the overlay root.
    let skills_root = locate_skills_root(&extract_dir).ok_or_else(|| {
        ExtensionError::Verify("no skills directory found in downloaded archive".into())
    })?;
    tokio::fs::create_dir_all(overlay_staging.join(".nomi")).await?;
    tokio::fs::rename(&skills_root, overlay_staging.join(".nomi").join("skills")).await?;
    tokio::fs::write(overlay_staging.join(VERSION_FILE), version).await?;

    // Atomic swap into place (reuses the Windows-safe rename/restore logic).
    commit_staging_dir(&target, &overlay_staging, &overlay_old).await?;

    // Best-effort cleanup of the extraction scratch.
    let _ = tokio::fs::remove_dir_all(&dl_tmp).await;
    Ok(())
}

/// True if `url` is https and its host is in [`ALLOWED_HOSTS`].
fn host_allowed(url: &str) -> bool {
    let Some(rest) = url.strip_prefix("https://") else {
        return false;
    };
    let authority = rest.split('/').next().unwrap_or("");
    // Strip any userinfo and port.
    let host = authority.rsplit('@').next().unwrap_or(authority);
    let host = host.split(':').next().unwrap_or(host);
    ALLOWED_HOSTS.iter().any(|h| host.eq_ignore_ascii_case(h))
}

fn verify_sha256(bytes: &[u8], expected: &str) -> Result<(), ExtensionError> {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    let actual = format!("{:x}", hasher.finalize());
    if actual.eq_ignore_ascii_case(expected.trim()) {
        Ok(())
    } else {
        Err(ExtensionError::Verify(format!(
            "sha256 mismatch: expected {}, got {actual}",
            expected.trim()
        )))
    }
}

/// Find the directory whose immediate children are skill dirs (each with a
/// `SKILL.md`), preferring a `skills/` directory. Handles the GitHub source
/// zipball shape (`<repo>-<sha>/skills/…`) and a bare skills archive.
fn locate_skills_root(root: &Path) -> Option<PathBuf> {
    if dir_has_skill_child(&root.join("skills")) {
        return Some(root.join("skills"));
    }
    if let Some(top) = single_subdir(root) {
        if dir_has_skill_child(&top.join("skills")) {
            return Some(top.join("skills"));
        }
        if dir_has_skill_child(&top) {
            return Some(top);
        }
    }
    if dir_has_skill_child(root) {
        return Some(root.to_path_buf());
    }
    None
}

/// True if `dir` has at least one immediate subdirectory containing a `SKILL.md`.
fn dir_has_skill_child(dir: &Path) -> bool {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return false;
    };
    entries.flatten().any(|e| e.path().join("SKILL.md").is_file())
}

/// The sole immediate subdirectory of `root`, or `None` if there are zero or
/// more than one (loose files are ignored).
fn single_subdir(root: &Path) -> Option<PathBuf> {
    let mut subdirs = std::fs::read_dir(root)
        .ok()?
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.is_dir());
    let first = subdirs.next()?;
    if subdirs.next().is_some() {
        return None;
    }
    Some(first)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Cursor, Write};
    use tempfile::TempDir;

    fn build_zip(entries: &[(&str, &[u8])]) -> Vec<u8> {
        let mut buf = Vec::new();
        {
            let mut w = zip::ZipWriter::new(Cursor::new(&mut buf));
            let opts = zip::write::SimpleFileOptions::default();
            for (name, data) in entries {
                w.start_file(*name, opts).unwrap();
                w.write_all(data).unwrap();
            }
            w.finish().unwrap();
        }
        buf
    }

    /// A minimal GitHub-zipball-shaped archive: everything under one top dir,
    /// skills nested under `skills/`.
    fn github_zipball() -> Vec<u8> {
        build_zip(&[
            ("obra-superpowers-abc123/README.md", b"# superpowers"),
            (
                "obra-superpowers-abc123/skills/brainstorming/SKILL.md",
                b"---\nname: brainstorming\n---\n",
            ),
            (
                "obra-superpowers-abc123/skills/test-driven-development/SKILL.md",
                b"---\nname: test-driven-development\n---\n",
            ),
        ])
    }

    #[tokio::test]
    async fn installs_overlay_from_github_zipball() {
        let tmp = TempDir::new().unwrap();
        let zip = github_zipball();
        install_superpowers_overlay_from_bytes(tmp.path(), "6.0.4", &zip, None)
            .await
            .unwrap();

        let overlay = tmp.path().join(SUPERPOWERS_OVERLAY_DIR);
        assert!(overlay.join(".nomi/skills/brainstorming/SKILL.md").is_file());
        assert!(overlay.join(".nomi/skills/test-driven-development/SKILL.md").is_file());
        assert_eq!(installed_overlay_version(tmp.path()).as_deref(), Some("6.0.4"));
        // effective dir now prefers the overlay (newer than the baseline version).
        assert_eq!(super::super::effective_superpowers_dir(tmp.path()), overlay);
        // extraction scratch is cleaned up.
        assert!(!tmp.path().join(DL_TMP).exists());
    }

    #[tokio::test]
    async fn sha256_mismatch_rejected_and_overlay_preserved() {
        let tmp = TempDir::new().unwrap();
        // Pre-existing overlay with a sentinel.
        let overlay = tmp.path().join(SUPERPOWERS_OVERLAY_DIR);
        std::fs::create_dir_all(overlay.join("existing")).unwrap();
        std::fs::write(overlay.join("existing/SKILL.md"), b"keep").unwrap();

        let zip = github_zipball();
        let err = install_superpowers_overlay_from_bytes(tmp.path(), "6.0.4", &zip, Some("deadbeef"))
            .await
            .unwrap_err();
        assert!(matches!(err, ExtensionError::Verify(_)));
        // Existing overlay untouched.
        assert!(overlay.join("existing/SKILL.md").is_file());
    }

    #[tokio::test]
    async fn archive_without_skills_is_rejected() {
        let tmp = TempDir::new().unwrap();
        let zip = build_zip(&[("obra-superpowers-abc123/README.md", b"no skills here")]);
        let err = install_superpowers_overlay_from_bytes(tmp.path(), "6.0.4", &zip, None)
            .await
            .unwrap_err();
        assert!(matches!(err, ExtensionError::Verify(_)));
    }

    #[test]
    fn host_allowlist_accepts_github_and_rejects_others() {
        assert!(host_allowed("https://api.github.com/repos/obra/superpowers/zipball/v6.0.3"));
        assert!(host_allowed("https://codeload.github.com/obra/superpowers/legacy.zip/refs/tags/v6"));
        assert!(host_allowed("https://objects.githubusercontent.com/x"));
        assert!(!host_allowed("http://github.com/x"), "http not allowed");
        assert!(!host_allowed("https://evil.example.com/superpowers.zip"));
        assert!(!host_allowed("https://github.com.evil.com/x"));
        assert!(!host_allowed("not-a-url"));
    }

    #[test]
    fn sha256_verify_matches() {
        let bytes = b"hello superpowers";
        let mut h = Sha256::new();
        h.update(bytes);
        let digest = format!("{:x}", h.finalize());
        assert!(verify_sha256(bytes, &digest).is_ok());
        assert!(verify_sha256(bytes, &digest.to_uppercase()).is_ok());
        assert!(verify_sha256(bytes, "00").is_err());
    }

    #[test]
    fn parses_github_release_json_and_strips_v_prefix() {
        let json = br#"{
            "tag_name": "v6.0.4",
            "name": "6.0.4",
            "zipball_url": "https://api.github.com/repos/obra/superpowers/zipball/v6.0.4"
        }"#;
        let r = parse_latest_release(json).unwrap();
        assert_eq!(r.version, "6.0.4", "leading v stripped for clean comparison");
        assert_eq!(r.zip_url, "https://api.github.com/repos/obra/superpowers/zipball/v6.0.4");
        assert!(r.sha256.is_none());

        // A tag with no v prefix is preserved verbatim.
        let json2 = br#"{"tag_name":"7.1.0","zipball_url":"https://api.github.com/z"}"#;
        assert_eq!(parse_latest_release(json2).unwrap().version, "7.1.0");
    }

    #[test]
    fn parse_rejects_missing_or_invalid_fields() {
        assert!(parse_latest_release(br#"{"name":"x"}"#).is_err(), "missing tag_name");
        assert!(
            parse_latest_release(br#"{"tag_name":"v1"}"#).is_err(),
            "missing zipball_url"
        );
        assert!(parse_latest_release(b"not json").is_err());
    }

    #[test]
    fn should_install_release_prefers_semver_then_falls_back() {
        assert!(should_install_release("6.0.4", "6.0.3"), "newer installs");
        assert!(!should_install_release("6.0.3", "6.0.3"), "same skips");
        assert!(!should_install_release("6.0.2", "6.0.3"), "downgrade blocked");
        // Baseline stamp carries build metadata (`+sp.<fp>`), which semver ignores.
        assert!(should_install_release("6.0.4", "6.0.3+sp.abcdef012345"));
        assert!(!should_install_release("6.0.3", "6.0.3+sp.abcdef012345"));
        // Non-semver versions fall back to plain inequality.
        assert!(should_install_release("nightly-2", "nightly-1"));
        assert!(!should_install_release("nightly-1", "nightly-1"));
        // Empty latest never installs.
        assert!(!should_install_release("", "6.0.3"));
    }
}

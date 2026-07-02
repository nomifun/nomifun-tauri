use std::sync::Arc;

use nomifun_api_types::{GitHubReleaseAsset, UpdateCheckRequest, UpdateCheckResult, UpdateReleaseInfo};
use nomifun_common::AppError;
use serde::Deserialize;

const DEFAULT_REPO: &str = "nomifun/nomifun-app";
const GITHUB_API_BASE: &str = "https://api.github.com";

/// Service that checks GitHub Releases for available updates.
#[derive(Clone)]
pub struct VersionCheckService {
    http_client: HttpClientFactory,
    current_version: String,
    /// Base URL for GitHub API. Defaults to `https://api.github.com`.
    /// Configurable for testing with mock servers.
    api_base: String,
}

type HttpClientFactory = Arc<dyn Fn() -> reqwest::Client + Send + Sync>;

impl VersionCheckService {
    pub fn new(http_client: reqwest::Client, current_version: String) -> Self {
        Self {
            http_client: Arc::new(move || http_client.clone()),
            current_version,
            api_base: GITHUB_API_BASE.to_owned(),
        }
    }

    pub fn new_dynamic(current_version: String) -> Self {
        Self {
            http_client: Arc::new(nomifun_net::http_client),
            current_version,
            api_base: GITHUB_API_BASE.to_owned(),
        }
    }

    /// Create a service with a custom API base URL (for testing).
    #[doc(hidden)]
    pub fn with_api_base(http_client: reqwest::Client, current_version: String, api_base: String) -> Self {
        Self {
            http_client: Arc::new(move || http_client.clone()),
            current_version,
            api_base,
        }
    }

    fn http_client(&self) -> reqwest::Client {
        (self.http_client)()
    }

    /// Check for updates against GitHub Releases.
    pub async fn check_update(&self, req: &UpdateCheckRequest) -> Result<UpdateCheckResult, AppError> {
        let repo = resolve_repo(req.repo.as_deref());
        let releases = self.fetch_releases(&repo).await?;

        let current = parse_version(&self.current_version)
            .ok_or_else(|| AppError::Internal(format!("invalid current version: {}", self.current_version)))?;

        let platform = crate::sysinfo::get_system_info();
        let best = find_best_release(
            &releases,
            &current,
            req.include_prerelease,
            &platform.platform,
            &platform.arch,
        );

        match best {
            Some(info) => Ok(UpdateCheckResult {
                current_version: self.current_version.clone(),
                update_available: true,
                latest: Some(info),
            }),
            None => Ok(UpdateCheckResult {
                current_version: self.current_version.clone(),
                update_available: false,
                latest: None,
            }),
        }
    }

    /// Fetch releases from GitHub API with pagination.
    ///
    /// Requests up to 100 releases per page (GitHub max). For most repositories
    /// a single page is sufficient, but we follow `Link: <..>; rel="next"` headers
    /// to collect additional pages (up to 5 pages / 500 releases).
    async fn fetch_releases(&self, repo: &str) -> Result<Vec<GitHubRelease>, AppError> {
        const PER_PAGE: u32 = 100;
        const MAX_PAGES: u32 = 5;

        let mut all_releases = Vec::new();
        let mut page = 1u32;
        let http_client = self.http_client();

        loop {
            let url = format!(
                "{}/repos/{repo}/releases?per_page={PER_PAGE}&page={page}",
                self.api_base
            );
            let resp = http_client
                .get(&url)
                .header("Accept", "application/vnd.github+json")
                .header("User-Agent", "nomicore")
                .send()
                .await
                .map_err(|e| AppError::BadGateway(format!("GitHub API request failed: {e}")))?;

            if !resp.status().is_success() {
                let status = resp.status();
                let body = resp.text().await.unwrap_or_default();
                return Err(AppError::BadGateway(format!("GitHub API returned {status}: {body}")));
            }

            let has_next = resp
                .headers()
                .get("link")
                .and_then(|v| v.to_str().ok())
                .is_some_and(|v| v.contains("rel=\"next\""));

            let batch: Vec<GitHubRelease> = resp
                .json()
                .await
                .map_err(|e| AppError::BadGateway(format!("Failed to parse GitHub releases: {e}")))?;

            let batch_len = batch.len();
            all_releases.extend(batch);

            page += 1;
            if !has_next || batch_len < PER_PAGE as usize || page > MAX_PAGES {
                break;
            }
        }

        Ok(all_releases)
    }
}

/// Resolve the GitHub repo from request or env or default.
fn resolve_repo(from_request: Option<&str>) -> String {
    if let Some(r) = from_request
        && !r.is_empty()
    {
        return r.to_owned();
    }
    if let Ok(v) = std::env::var("NOMIFUN_GITHUB_REPO")
        && !v.is_empty()
    {
        return v;
    }
    DEFAULT_REPO.to_owned()
}

/// Parse a version string, stripping a leading `v` if present.
fn parse_version(s: &str) -> Option<semver::Version> {
    let stripped = s.strip_prefix('v').unwrap_or(s);
    semver::Version::parse(stripped).ok()
}

/// Find the best available release that is newer than `current`.
fn find_best_release(
    releases: &[GitHubRelease],
    current: &semver::Version,
    include_prerelease: bool,
    platform: &str,
    arch: &str,
) -> Option<UpdateReleaseInfo> {
    let mut best: Option<(semver::Version, &GitHubRelease)> = None;

    for release in releases {
        // Skip drafts always
        if release.draft {
            continue;
        }
        // Skip prereleases unless requested
        if release.prerelease && !include_prerelease {
            continue;
        }
        let version = match parse_version(&release.tag_name) {
            Some(v) => v,
            None => continue,
        };
        // Must be newer than current
        if version <= *current {
            continue;
        }
        // Keep the highest version
        let dominated = best.as_ref().is_none_or(|(v, _)| version > *v);
        if dominated {
            best = Some((version, release));
        }
    }

    best.map(|(version, release)| {
        let assets: Vec<GitHubReleaseAsset> = release
            .assets
            .iter()
            .map(|a| GitHubReleaseAsset {
                name: a.name.clone(),
                url: a.browser_download_url.clone(),
                size: a.size,
                content_type: a.content_type.clone(),
            })
            .collect();

        let recommended_asset = find_recommended_asset(&assets, platform, arch);

        UpdateReleaseInfo {
            tag_name: release.tag_name.clone(),
            version: version.to_string(),
            name: release.name.clone(),
            body: release.body.clone(),
            html_url: release.html_url.clone(),
            published_at: release.published_at.clone(),
            prerelease: release.prerelease,
            draft: release.draft,
            assets,
            recommended_asset,
        }
    })
}

/// Match the best asset for the given platform and architecture.
///
/// Uses filename heuristics: the asset name should contain a platform
/// keyword and an architecture keyword.
fn find_recommended_asset(assets: &[GitHubReleaseAsset], platform: &str, arch: &str) -> Option<GitHubReleaseAsset> {
    let platform_keywords = platform_keywords(platform);
    let arch_keywords = arch_keywords(arch);

    assets
        .iter()
        .find(|a| {
            let name = a.name.to_lowercase();
            let has_platform = platform_keywords.iter().any(|k| name.contains(k));
            let has_arch = arch_keywords.iter().any(|k| name.contains(k));
            has_platform && has_arch
        })
        .cloned()
}

/// Return filename keywords that identify the given platform.
fn platform_keywords(platform: &str) -> Vec<&'static str> {
    match platform {
        "darwin" => vec!["darwin", "macos", "mac", "osx"],
        "win32" => vec!["win", "windows"],
        "linux" => vec!["linux"],
        _ => vec![],
    }
}

/// Return filename keywords that identify the given architecture.
fn arch_keywords(arch: &str) -> Vec<&'static str> {
    match arch {
        "x64" => vec!["x64", "x86_64", "amd64"],
        "arm64" => vec!["arm64", "aarch64"],
        _ => vec![],
    }
}

// ---------------------------------------------------------------------------
// GitHub API response types (internal, not exposed)
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct GitHubRelease {
    tag_name: String,
    name: Option<String>,
    body: Option<String>,
    html_url: String,
    published_at: Option<String>,
    prerelease: bool,
    draft: bool,
    assets: Vec<GitHubAsset>,
}

#[derive(Debug, Deserialize)]
struct GitHubAsset {
    name: String,
    browser_download_url: String,
    size: u64,
    content_type: Option<String>,
}

// ---------------------------------------------------------------------------
// Unit tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_resolve_repo_from_request() {
        assert_eq!(resolve_repo(Some("org/repo")), "org/repo");
    }

    #[test]
    fn test_resolve_repo_empty_request() {
        let result = resolve_repo(Some(""));
        // Falls back to env or default
        assert!(!result.is_empty());
    }

    #[test]
    fn test_resolve_repo_none() {
        let result = resolve_repo(None);
        assert!(!result.is_empty());
    }

    #[test]
    fn test_parse_version_plain() {
        let v = parse_version("1.2.3").unwrap();
        assert_eq!(v, semver::Version::new(1, 2, 3));
    }

    #[test]
    fn test_parse_version_with_v_prefix() {
        let v = parse_version("v2.0.0").unwrap();
        assert_eq!(v, semver::Version::new(2, 0, 0));
    }

    #[test]
    fn test_parse_version_prerelease() {
        let v = parse_version("v3.0.0-beta.1").unwrap();
        assert_eq!(v.major, 3);
        assert!(!v.pre.is_empty());
    }

    #[test]
    fn test_parse_version_invalid() {
        assert!(parse_version("not-a-version").is_none());
    }

    #[test]
    fn test_platform_keywords_darwin() {
        let kw = platform_keywords("darwin");
        assert!(kw.contains(&"darwin"));
        assert!(kw.contains(&"macos"));
    }

    #[test]
    fn test_platform_keywords_win32() {
        let kw = platform_keywords("win32");
        assert!(kw.contains(&"win"));
        assert!(kw.contains(&"windows"));
    }

    #[test]
    fn test_arch_keywords_x64() {
        let kw = arch_keywords("x64");
        assert!(kw.contains(&"x64"));
        assert!(kw.contains(&"x86_64"));
        assert!(kw.contains(&"amd64"));
    }

    #[test]
    fn test_arch_keywords_arm64() {
        let kw = arch_keywords("arm64");
        assert!(kw.contains(&"arm64"));
        assert!(kw.contains(&"aarch64"));
    }

    fn make_release(tag: &str, draft: bool, prerelease: bool, assets: Vec<GitHubAsset>) -> GitHubRelease {
        GitHubRelease {
            tag_name: tag.to_owned(),
            name: Some(format!("Release {tag}")),
            body: None,
            html_url: format!("https://github.com/org/repo/releases/tag/{tag}"),
            published_at: Some("2026-01-01T00:00:00Z".to_owned()),
            prerelease,
            draft,
            assets,
        }
    }

    fn make_asset(name: &str) -> GitHubAsset {
        GitHubAsset {
            name: name.to_owned(),
            browser_download_url: format!("https://github.com/download/{name}"),
            size: 100_000,
            content_type: Some("application/octet-stream".to_owned()),
        }
    }

    #[test]
    fn test_find_best_release_newer_version() {
        let current = semver::Version::new(1, 0, 0);
        let releases = vec![
            make_release("v1.1.0", false, false, vec![]),
            make_release("v2.0.0", false, false, vec![]),
            make_release("v0.9.0", false, false, vec![]),
        ];
        let best = find_best_release(&releases, &current, false, "darwin", "arm64");
        assert!(best.is_some());
        assert_eq!(best.unwrap().version, "2.0.0");
    }

    #[test]
    fn test_find_best_release_no_update() {
        let current = semver::Version::new(3, 0, 0);
        let releases = vec![
            make_release("v1.0.0", false, false, vec![]),
            make_release("v2.0.0", false, false, vec![]),
        ];
        let best = find_best_release(&releases, &current, false, "darwin", "arm64");
        assert!(best.is_none());
    }

    #[test]
    fn test_find_best_release_skips_draft() {
        let current = semver::Version::new(1, 0, 0);
        let releases = vec![
            make_release("v5.0.0", true, false, vec![]), // draft — skip
            make_release("v2.0.0", false, false, vec![]),
        ];
        let best = find_best_release(&releases, &current, false, "darwin", "arm64");
        assert_eq!(best.unwrap().version, "2.0.0");
    }

    #[test]
    fn test_find_best_release_skips_prerelease_unless_included() {
        let current = semver::Version::new(1, 0, 0);
        let releases = vec![
            make_release("v3.0.0-beta.1", false, true, vec![]),
            make_release("v2.0.0", false, false, vec![]),
        ];

        // Without prerelease
        let best = find_best_release(&releases, &current, false, "darwin", "arm64");
        assert_eq!(best.unwrap().version, "2.0.0");

        // With prerelease
        let best = find_best_release(&releases, &current, true, "darwin", "arm64");
        assert_eq!(best.unwrap().version, "3.0.0-beta.1");
    }

    #[test]
    fn test_find_best_release_invalid_tag_skipped() {
        let current = semver::Version::new(1, 0, 0);
        let releases = vec![
            make_release("not-semver", false, false, vec![]),
            make_release("v2.0.0", false, false, vec![]),
        ];
        let best = find_best_release(&releases, &current, false, "darwin", "arm64");
        assert_eq!(best.unwrap().version, "2.0.0");
    }

    #[test]
    fn test_find_recommended_asset_darwin_arm64() {
        let assets = vec![
            GitHubReleaseAsset {
                name: "app-2.0.0-win-x64.exe".into(),
                url: "https://example.com/win.exe".into(),
                size: 100,
                content_type: None,
            },
            GitHubReleaseAsset {
                name: "app-2.0.0-darwin-arm64.dmg".into(),
                url: "https://example.com/mac.dmg".into(),
                size: 200,
                content_type: None,
            },
            GitHubReleaseAsset {
                name: "app-2.0.0-linux-x64.deb".into(),
                url: "https://example.com/linux.deb".into(),
                size: 150,
                content_type: None,
            },
        ];
        let rec = find_recommended_asset(&assets, "darwin", "arm64");
        assert!(rec.is_some());
        assert!(rec.unwrap().name.contains("darwin-arm64"));
    }

    #[test]
    fn test_find_recommended_asset_linux_x64() {
        let assets = vec![GitHubReleaseAsset {
            name: "app-linux-amd64.tar.gz".into(),
            url: "https://example.com/linux.tar.gz".into(),
            size: 150,
            content_type: None,
        }];
        let rec = find_recommended_asset(&assets, "linux", "x64");
        assert!(rec.is_some());
    }

    #[test]
    fn test_find_recommended_asset_no_match() {
        let assets = vec![GitHubReleaseAsset {
            name: "app-win-x64.exe".into(),
            url: "https://example.com/win.exe".into(),
            size: 100,
            content_type: None,
        }];
        let rec = find_recommended_asset(&assets, "darwin", "arm64");
        assert!(rec.is_none());
    }

    #[test]
    fn test_find_best_release_with_asset_matching() {
        let current = semver::Version::new(1, 0, 0);
        let releases = vec![make_release(
            "v2.0.0",
            false,
            false,
            vec![
                make_asset("app-2.0.0-win-x64.exe"),
                make_asset("app-2.0.0-darwin-arm64.dmg"),
            ],
        )];
        let best = find_best_release(&releases, &current, false, "darwin", "arm64").unwrap();
        assert!(best.recommended_asset.is_some());
        assert!(best.recommended_asset.unwrap().name.contains("darwin-arm64"));
    }

    #[test]
    fn test_find_best_release_equal_version_not_update() {
        let current = semver::Version::new(2, 0, 0);
        let releases = vec![make_release("v2.0.0", false, false, vec![])];
        let best = find_best_release(&releases, &current, false, "darwin", "arm64");
        assert!(best.is_none(), "equal version should not be an update");
    }
}

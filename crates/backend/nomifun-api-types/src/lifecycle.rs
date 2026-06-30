use serde::{Deserialize, Serialize};

/// Response for `GET /api/system/info`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SystemInfoResponse {
    pub cache_dir: String,
    pub work_dir: String,
    pub log_dir: String,
    /// Operating system: `darwin`, `win32`, or `linux`.
    pub platform: String,
    /// CPU architecture: `x64` or `arm64`.
    pub arch: String,
}

/// Request body for `POST /api/system/check-update`.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct UpdateCheckRequest {
    #[serde(default)]
    pub include_prerelease: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub repo: Option<String>,
}

/// Request body for `POST /api/system/work-dir`: persist the user-chosen
/// conversation workspace root. Applied on the next boot (the backend resolves
/// `work_dir` before any service starts), so the client restarts after this.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UpdateWorkDirRequest {
    pub work_dir: String,
}

/// Response for `POST /api/system/check-update`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct UpdateCheckResult {
    pub current_version: String,
    pub update_available: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub latest: Option<UpdateReleaseInfo>,
}

/// A single GitHub Release entry.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct UpdateReleaseInfo {
    pub tag_name: String,
    pub version: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub body: Option<String>,
    pub html_url: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub published_at: Option<String>,
    pub prerelease: bool,
    pub draft: bool,
    pub assets: Vec<GitHubReleaseAsset>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub recommended_asset: Option<GitHubReleaseAsset>,
}

/// A downloadable asset attached to a GitHub Release.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct GitHubReleaseAsset {
    pub name: String,
    pub url: String,
    pub size: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content_type: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // -- SystemInfoResponse --

    #[test]
    fn test_system_info_response_serialization() {
        let resp = SystemInfoResponse {
            cache_dir: "/home/user/.cache/nomifun".into(),
            work_dir: "/home/user/.local/share/nomifun".into(),
            log_dir: "/home/user/.local/state/nomifun/logs".into(),
            platform: "linux".into(),
            arch: "x64".into(),
        };
        let json = serde_json::to_value(&resp).unwrap();
        assert_eq!(json["cache_dir"], "/home/user/.cache/nomifun");
        assert_eq!(json["work_dir"], "/home/user/.local/share/nomifun");
        assert_eq!(json["log_dir"], "/home/user/.local/state/nomifun/logs");
        assert_eq!(json["platform"], "linux");
        assert_eq!(json["arch"], "x64");
        // Verify snake_case
        assert!(json.get("cacheDir").is_none());
    }

    #[test]
    fn test_system_info_response_roundtrip() {
        let original = SystemInfoResponse {
            cache_dir: "/tmp/cache".into(),
            work_dir: "/tmp/work".into(),
            log_dir: "/tmp/logs".into(),
            platform: "darwin".into(),
            arch: "arm64".into(),
        };
        let serialized = serde_json::to_string(&original).unwrap();
        let parsed: SystemInfoResponse = serde_json::from_str(&serialized).unwrap();
        assert_eq!(parsed, original);
    }

    // -- UpdateCheckRequest --

    #[test]
    fn test_update_check_request_default() {
        let raw = json!({});
        let req: UpdateCheckRequest = serde_json::from_value(raw).unwrap();
        assert!(!req.include_prerelease);
        assert!(req.repo.is_none());
    }

    #[test]
    fn test_update_check_request_with_options() {
        let raw = json!({
            "include_prerelease": true,
            "repo": "nomifun/nomifun-app"
        });
        let req: UpdateCheckRequest = serde_json::from_value(raw).unwrap();
        assert!(req.include_prerelease);
        assert_eq!(req.repo.as_deref(), Some("nomifun/nomifun-app"));
    }

    // -- UpdateWorkDirRequest --

    #[test]
    fn test_update_work_dir_request_deserializes_snake_case() {
        let raw = json!({ "work_dir": "/Users/me/Workspaces/nomi" });
        let req: UpdateWorkDirRequest = serde_json::from_value(raw).unwrap();
        assert_eq!(req.work_dir, "/Users/me/Workspaces/nomi");
    }

    // -- UpdateCheckResult --

    #[test]
    fn test_update_check_result_no_update() {
        let result = UpdateCheckResult {
            current_version: "1.5.0".into(),
            update_available: false,
            latest: None,
        };
        let json = serde_json::to_value(&result).unwrap();
        assert_eq!(json["current_version"], "1.5.0");
        assert_eq!(json["update_available"], false);
        assert!(json.get("latest").is_none());
    }

    #[test]
    fn test_update_check_result_with_update() {
        let result = UpdateCheckResult {
            current_version: "1.0.0".into(),
            update_available: true,
            latest: Some(UpdateReleaseInfo {
                tag_name: "v2.0.0".into(),
                version: "2.0.0".into(),
                name: Some("Version 2.0.0".into()),
                body: Some("Major release".into()),
                html_url: "https://github.com/nomifun/nomifun-app/releases/tag/v2.0.0".into(),
                published_at: Some("2026-04-01T00:00:00Z".into()),
                prerelease: false,
                draft: false,
                assets: vec![GitHubReleaseAsset {
                    name: "nomifun-2.0.0-darwin-arm64.dmg".into(),
                    url: "https://github.com/download/nomifun.dmg".into(),
                    size: 85_000_000,
                    content_type: Some("application/x-apple-diskimage".into()),
                }],
                recommended_asset: None,
            }),
        };
        let json = serde_json::to_value(&result).unwrap();
        assert_eq!(json["update_available"], true);
        assert_eq!(json["latest"]["tag_name"], "v2.0.0");
        assert_eq!(json["latest"]["version"], "2.0.0");
        assert!(!json["latest"]["html_url"].as_str().unwrap().is_empty());
        assert_eq!(json["latest"]["assets"].as_array().unwrap().len(), 1);
        assert_eq!(json["latest"]["assets"][0]["name"], "nomifun-2.0.0-darwin-arm64.dmg");
        assert_eq!(json["latest"]["assets"][0]["size"], 85_000_000_u64);
    }

    // -- GitHubReleaseAsset --

    #[test]
    fn test_github_release_asset_serialization() {
        let asset = GitHubReleaseAsset {
            name: "app.zip".into(),
            url: "https://example.com/app.zip".into(),
            size: 1024,
            content_type: None,
        };
        let json = serde_json::to_value(&asset).unwrap();
        assert_eq!(json["name"], "app.zip");
        assert_eq!(json["url"], "https://example.com/app.zip");
        assert_eq!(json["size"], 1024);
        assert!(json.get("content_type").is_none());
    }

    #[test]
    fn test_github_release_asset_with_content_type() {
        let asset = GitHubReleaseAsset {
            name: "app.exe".into(),
            url: "https://example.com/app.exe".into(),
            size: 50_000_000,
            content_type: Some("application/octet-stream".into()),
        };
        let json = serde_json::to_value(&asset).unwrap();
        assert_eq!(json["content_type"], "application/octet-stream");
    }

    // -- UpdateReleaseInfo --

    #[test]
    fn test_update_release_info_minimal() {
        let info = UpdateReleaseInfo {
            tag_name: "v1.0.0".into(),
            version: "1.0.0".into(),
            name: None,
            body: None,
            html_url: "https://github.com/org/repo/releases/tag/v1.0.0".into(),
            published_at: None,
            prerelease: false,
            draft: false,
            assets: vec![],
            recommended_asset: None,
        };
        let json = serde_json::to_value(&info).unwrap();
        assert_eq!(json["tag_name"], "v1.0.0");
        assert!(json.get("name").is_none());
        assert!(json.get("body").is_none());
        assert!(json.get("published_at").is_none());
        assert!(json.get("recommended_asset").is_none());
    }

    #[test]
    fn test_update_release_info_prerelease() {
        let info = UpdateReleaseInfo {
            tag_name: "v2.0.0-beta.1".into(),
            version: "2.0.0-beta.1".into(),
            name: Some("Beta 1".into()),
            body: None,
            html_url: "https://github.com/org/repo/releases/tag/v2.0.0-beta.1".into(),
            published_at: None,
            prerelease: true,
            draft: false,
            assets: vec![],
            recommended_asset: None,
        };
        let json = serde_json::to_value(&info).unwrap();
        assert_eq!(json["prerelease"], true);
        assert_eq!(json["draft"], false);
    }
}

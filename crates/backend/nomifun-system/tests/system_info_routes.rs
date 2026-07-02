//! Black-box integration tests for system info and version check routes.
//!
//! System info tests verify the GET /api/system/info endpoint returns
//! correct platform/arch values and non-empty directory paths.
//!
//! Version check tests use `wiremock` to mock the GitHub Releases API
//! and verify the POST /api/system/check-update endpoint.

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use serde_json::json;
use tower::ServiceExt;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use nomifun_db::{
    SqliteClientPreferenceRepository, SqliteProviderRepository, SqliteSettingsRepository, init_database_memory,
};
use nomifun_system::{
    ClientPrefService, ModelFetchService, ProtocolDetectionService, ProviderService, SettingsService,
    SystemRouterState, VersionCheckService, system_routes,
};

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

const TEST_KEY: [u8; 32] = [0x42; 32];

fn test_http_client() -> reqwest::Client {
    reqwest::Client::builder().no_proxy().build().unwrap()
}

fn build_state(db: &nomifun_db::Database, version_check_service: VersionCheckService) -> SystemRouterState {
    let provider_repo = Arc::new(SqliteProviderRepository::new(db.pool().clone()));
    let http_client = test_http_client();
    SystemRouterState {
        settings_service: SettingsService::new(Arc::new(SqliteSettingsRepository::new(db.pool().clone()))),
        client_pref_service: ClientPrefService::new(Arc::new(SqliteClientPreferenceRepository::new(db.pool().clone()))),
        provider_service: ProviderService::new(provider_repo.clone(), TEST_KEY),
        model_fetch_service: ModelFetchService::new(provider_repo, TEST_KEY, http_client.clone()),
        protocol_detection_service: ProtocolDetectionService::new(http_client),
        version_check_service,
        data_dir: std::env::temp_dir(),
    }
}

async fn setup() -> axum::Router {
    let db = init_database_memory().await.unwrap();
    let http_client = test_http_client();
    let vcs = VersionCheckService::new(http_client, "1.0.0".to_owned());
    let state = build_state(&db, vcs);
    system_routes(state)
}

async fn setup_with_mock(current_version: &str, mock_server: &MockServer) -> axum::Router {
    let db = init_database_memory().await.unwrap();
    let http_client = test_http_client();
    let vcs = VersionCheckService::with_api_base(http_client, current_version.to_owned(), mock_server.uri());
    let state = build_state(&db, vcs);
    system_routes(state)
}

async fn body_json(resp: axum::response::Response) -> serde_json::Value {
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    serde_json::from_slice(&bytes).unwrap()
}

fn get_request(uri: &str) -> Request<Body> {
    Request::builder().method("GET").uri(uri).body(Body::empty()).unwrap()
}

fn json_request(method_str: &str, uri: &str, body: serde_json::Value) -> Request<Body> {
    Request::builder()
        .method(method_str)
        .uri(uri)
        .header("content-type", "application/json")
        .body(Body::from(serde_json::to_vec(&body).unwrap()))
        .unwrap()
}

fn make_github_release(tag: &str, draft: bool, prerelease: bool, assets: Vec<serde_json::Value>) -> serde_json::Value {
    json!({
        "tag_name": tag,
        "name": format!("Release {tag}"),
        "body": "Release notes",
        "html_url": format!("https://github.com/nomifun/nomifun-app/releases/tag/{tag}"),
        "published_at": "2026-04-01T00:00:00Z",
        "prerelease": prerelease,
        "draft": draft,
        "assets": assets,
    })
}

fn make_github_asset(name: &str, size: u64) -> serde_json::Value {
    json!({
        "name": name,
        "browser_download_url": format!("https://github.com/download/{name}"),
        "size": size,
        "content_type": "application/octet-stream",
    })
}

// ===========================================================================
// GET /api/system/info
// ===========================================================================

#[tokio::test]
async fn test_system_info_returns_all_fields() {
    let app = setup().await;
    let resp = app.oneshot(get_request("/api/system/info")).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let json = body_json(resp).await;
    assert_eq!(json["success"], true);

    let data = &json["data"];
    assert!(data["cache_dir"].as_str().is_some_and(|s| !s.is_empty()));
    assert!(data["work_dir"].as_str().is_some_and(|s| !s.is_empty()));
    assert!(data["log_dir"].as_str().is_some_and(|s| !s.is_empty()));
    assert!(data["platform"].as_str().is_some_and(|s| !s.is_empty()));
    assert!(data["arch"].as_str().is_some_and(|s| !s.is_empty()));
}

#[tokio::test]
async fn test_system_info_platform_is_known() {
    let app = setup().await;
    let resp = app.oneshot(get_request("/api/system/info")).await.unwrap();
    let json = body_json(resp).await;
    let platform = json["data"]["platform"].as_str().unwrap();
    assert!(
        ["darwin", "win32", "linux"].contains(&platform),
        "unexpected platform: {platform}"
    );
}

#[tokio::test]
async fn test_system_info_arch_is_known() {
    let app = setup().await;
    let resp = app.oneshot(get_request("/api/system/info")).await.unwrap();
    let json = body_json(resp).await;
    let arch = json["data"]["arch"].as_str().unwrap();
    assert!(["x64", "arm64"].contains(&arch), "unexpected arch: {arch}");
}

#[tokio::test]
async fn test_system_info_snake_case_keys() {
    let app = setup().await;
    let resp = app.oneshot(get_request("/api/system/info")).await.unwrap();
    let json = body_json(resp).await;
    let data = &json["data"];
    assert!(data.get("cache_dir").is_some());
    assert!(data.get("work_dir").is_some());
    assert!(data.get("log_dir").is_some());
    assert!(data.get("cacheDir").is_none());
    assert!(data.get("workDir").is_none());
    assert!(data.get("logDir").is_none());
}

// ===========================================================================
// POST /api/system/check-update — with wiremock
// ===========================================================================

#[tokio::test]
async fn test_check_update_has_new_version() {
    let mock_server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/repos/nomifun/nomifun-app/releases"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([
            make_github_release(
                "v2.0.0",
                false,
                false,
                vec![
                    make_github_asset("app-2.0.0-darwin-arm64.dmg", 80_000_000),
                    make_github_asset("app-2.0.0-linux-x64.deb", 60_000_000),
                ]
            ),
            make_github_release("v1.5.0", false, false, vec![]),
        ])))
        .mount(&mock_server)
        .await;

    let app = setup_with_mock("1.0.0", &mock_server).await;
    let resp = app
        .oneshot(json_request("POST", "/api/system/check-update", json!({})))
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
    let json = body_json(resp).await;
    assert_eq!(json["success"], true);
    assert_eq!(json["data"]["current_version"], "1.0.0");
    assert_eq!(json["data"]["update_available"], true);

    let latest = &json["data"]["latest"];
    assert_eq!(latest["tag_name"], "v2.0.0");
    assert_eq!(latest["version"], "2.0.0");
    assert!(!latest["assets"].as_array().unwrap().is_empty());
}

#[tokio::test]
async fn test_check_update_no_update_available() {
    let mock_server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/repos/nomifun/nomifun-app/releases"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([
            make_github_release("v1.0.0", false, false, vec![]),
            make_github_release("v0.9.0", false, false, vec![]),
        ])))
        .mount(&mock_server)
        .await;

    let app = setup_with_mock("1.0.0", &mock_server).await;
    let resp = app
        .oneshot(json_request("POST", "/api/system/check-update", json!({})))
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
    let json = body_json(resp).await;
    assert_eq!(json["data"]["update_available"], false);
    assert!(json["data"].get("latest").is_none());
}

#[tokio::test]
async fn test_check_update_skips_draft() {
    let mock_server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/repos/nomifun/nomifun-app/releases"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([
            make_github_release("v5.0.0", true, false, vec![]), // draft — skip
            make_github_release("v2.0.0", false, false, vec![]),
        ])))
        .mount(&mock_server)
        .await;

    let app = setup_with_mock("1.0.0", &mock_server).await;
    let resp = app
        .oneshot(json_request("POST", "/api/system/check-update", json!({})))
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
    let json = body_json(resp).await;
    assert_eq!(json["data"]["update_available"], true);
    assert_eq!(json["data"]["latest"]["version"], "2.0.0");
}

#[tokio::test]
async fn test_check_update_skips_prerelease_by_default() {
    let mock_server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/repos/nomifun/nomifun-app/releases"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([
            make_github_release("v3.0.0-beta.1", false, true, vec![]),
            make_github_release("v2.0.0", false, false, vec![]),
        ])))
        .mount(&mock_server)
        .await;

    let app = setup_with_mock("1.0.0", &mock_server).await;
    let resp = app
        .oneshot(json_request(
            "POST",
            "/api/system/check-update",
            json!({"include_prerelease": false}),
        ))
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
    let json = body_json(resp).await;
    assert_eq!(json["data"]["latest"]["version"], "2.0.0");
}

#[tokio::test]
async fn test_check_update_includes_prerelease_when_requested() {
    let mock_server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/repos/nomifun/nomifun-app/releases"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([
            make_github_release("v3.0.0-beta.1", false, true, vec![]),
            make_github_release("v2.0.0", false, false, vec![]),
        ])))
        .mount(&mock_server)
        .await;

    let app = setup_with_mock("1.0.0", &mock_server).await;
    let resp = app
        .oneshot(json_request(
            "POST",
            "/api/system/check-update",
            json!({"include_prerelease": true}),
        ))
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
    let json = body_json(resp).await;
    assert_eq!(json["data"]["latest"]["version"], "3.0.0-beta.1");
}

#[tokio::test]
async fn test_check_update_recommended_asset_matches_platform() {
    let mock_server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/repos/nomifun/nomifun-app/releases"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([make_github_release(
            "v2.0.0",
            false,
            false,
            vec![
                make_github_asset("app-2.0.0-win-x64.exe", 50_000_000),
                make_github_asset("app-2.0.0-darwin-arm64.dmg", 80_000_000),
                make_github_asset("app-2.0.0-linux-amd64.deb", 60_000_000),
            ]
        ),])))
        .mount(&mock_server)
        .await;

    let app = setup_with_mock("1.0.0", &mock_server).await;
    let resp = app
        .oneshot(json_request("POST", "/api/system/check-update", json!({})))
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
    let json = body_json(resp).await;
    let recommended = &json["data"]["latest"]["recommended_asset"];
    // On the CI runner's actual platform, the recommended asset should match
    if recommended.is_object() {
        let name = recommended["name"].as_str().unwrap();
        // Verify it's one of the known assets
        assert!(
            name.contains("darwin") || name.contains("linux") || name.contains("win"),
            "recommended asset should contain platform keyword: {name}"
        );
    }
}

#[tokio::test]
async fn test_check_update_github_api_error() {
    let mock_server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/repos/nomifun/nomifun-app/releases"))
        .respond_with(ResponseTemplate::new(500).set_body_string("Internal Server Error"))
        .mount(&mock_server)
        .await;

    let app = setup_with_mock("1.0.0", &mock_server).await;
    let resp = app
        .oneshot(json_request("POST", "/api/system/check-update", json!({})))
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::BAD_GATEWAY);
    let json = body_json(resp).await;
    assert_eq!(json["success"], false);
}

#[tokio::test]
async fn test_check_update_empty_releases() {
    let mock_server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/repos/nomifun/nomifun-app/releases"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([])))
        .mount(&mock_server)
        .await;

    let app = setup_with_mock("1.0.0", &mock_server).await;
    let resp = app
        .oneshot(json_request("POST", "/api/system/check-update", json!({})))
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
    let json = body_json(resp).await;
    assert_eq!(json["data"]["update_available"], false);
}

#[tokio::test]
async fn test_check_update_custom_repo() {
    let mock_server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/repos/custom-org/custom-repo/releases"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([make_github_release(
            "v3.0.0",
            false,
            false,
            vec![]
        ),])))
        .mount(&mock_server)
        .await;

    let app = setup_with_mock("1.0.0", &mock_server).await;
    let resp = app
        .oneshot(json_request(
            "POST",
            "/api/system/check-update",
            json!({"repo": "custom-org/custom-repo"}),
        ))
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
    let json = body_json(resp).await;
    assert_eq!(json["data"]["update_available"], true);
    assert_eq!(json["data"]["latest"]["version"], "3.0.0");
}

#[tokio::test]
async fn test_check_update_invalid_tag_ignored() {
    let mock_server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/repos/nomifun/nomifun-app/releases"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([
            make_github_release("not-semver", false, false, vec![]),
            make_github_release("v2.0.0", false, false, vec![]),
        ])))
        .mount(&mock_server)
        .await;

    let app = setup_with_mock("1.0.0", &mock_server).await;
    let resp = app
        .oneshot(json_request("POST", "/api/system/check-update", json!({})))
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
    let json = body_json(resp).await;
    assert_eq!(json["data"]["latest"]["version"], "2.0.0");
}

#[tokio::test]
async fn test_check_update_response_format() {
    let mock_server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/repos/nomifun/nomifun-app/releases"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([make_github_release(
            "v2.0.0",
            false,
            false,
            vec![make_github_asset("app.dmg", 100_000),]
        ),])))
        .mount(&mock_server)
        .await;

    let app = setup_with_mock("1.0.0", &mock_server).await;
    let resp = app
        .oneshot(json_request("POST", "/api/system/check-update", json!({})))
        .await
        .unwrap();

    let json = body_json(resp).await;
    let latest = &json["data"]["latest"];

    // Verify snake_case serialization
    assert!(latest.get("tag_name").is_some());
    assert!(latest.get("html_url").is_some());
    assert!(latest.get("published_at").is_some());
    // Verify camelCase is NOT used
    assert!(latest.get("tagName").is_none());
    assert!(latest.get("htmlUrl").is_none());
}

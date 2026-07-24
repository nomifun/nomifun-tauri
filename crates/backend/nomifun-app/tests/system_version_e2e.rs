//! System info, version check, and full system flow E2E tests.

mod common;

use axum::http::StatusCode;
use serde_json::json;
use tower::ServiceExt;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use common::{
    body_json, build_app, build_app_with_mock_version, delete_with_token, get_with_token, json_with_token,
    setup_and_login,
};

// ===========================================================================
// System info
// ===========================================================================

#[tokio::test]
async fn system_info_with_auth() {
    let (mut app, services) = build_app().await;
    let (token, _csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;

    let resp = app.oneshot(get_with_token("/api/system/info", &token)).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let json = body_json(resp).await;
    assert_eq!(json["success"], true);

    let data = &json["data"];
    assert!(data["cache_dir"].as_str().is_some_and(|s| !s.is_empty()));
    assert!(data["work_dir"].as_str().is_some_and(|s| !s.is_empty()));
    assert!(data["log_dir"].as_str().is_some_and(|s| !s.is_empty()));
    assert!(["darwin", "win32", "linux"].contains(&data["platform"].as_str().unwrap()));
    assert!(["x64", "arm64"].contains(&data["arch"].as_str().unwrap()));
}

// ===========================================================================
// Version check
// ===========================================================================

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

#[tokio::test]
async fn version_check_has_update_with_auth() {
    let mock_server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/repos/nomifun/nomifun-app/releases"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([make_github_release(
            "v2.0.0",
            false,
            false,
            vec![make_github_asset("app-2.0.0-darwin-arm64.dmg", 80_000_000),]
        ),])))
        .mount(&mock_server)
        .await;

    let (mut app, services) = build_app_with_mock_version("1.0.0", &mock_server).await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;

    let req = json_with_token("POST", "/api/system/check-update", json!({}), &token, &csrf);
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let json = body_json(resp).await;
    assert_eq!(json["data"]["update_available"], true);
    assert_eq!(json["data"]["latest"]["version"], "2.0.0");
}

#[tokio::test]
async fn version_check_no_update_with_auth() {
    let mock_server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/repos/nomifun/nomifun-app/releases"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([make_github_release(
            "v1.0.0",
            false,
            false,
            vec![]
        ),])))
        .mount(&mock_server)
        .await;

    let (mut app, services) = build_app_with_mock_version("1.0.0", &mock_server).await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;

    let req = json_with_token("POST", "/api/system/check-update", json!({}), &token, &csrf);
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let json = body_json(resp).await;
    assert_eq!(json["data"]["update_available"], false);
}

#[tokio::test]
async fn version_check_github_error_with_auth() {
    let mock_server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/repos/nomifun/nomifun-app/releases"))
        .respond_with(ResponseTemplate::new(500).set_body_string("Internal Error"))
        .mount(&mock_server)
        .await;

    let (mut app, services) = build_app_with_mock_version("1.0.0", &mock_server).await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;

    let req = json_with_token("POST", "/api/system/check-update", json!({}), &token, &csrf);
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_GATEWAY);
}

// ===========================================================================
// Full authenticated flow — settings + providers round-trip
// ===========================================================================

#[tokio::test]
async fn full_system_flow_e2e() {
    let (mut app, services) = build_app().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;

    // 1. Get default settings
    let resp = app
        .clone()
        .oneshot(get_with_token("/api/settings", &token))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let json = body_json(resp).await;
    assert_eq!(json["data"]["language"], "en-US");

    // 2. Update language
    let req = json_with_token("PATCH", "/api/settings", json!({"language": "zh-CN"}), &token, &csrf);
    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let json = body_json(resp).await;
    assert_eq!(json["data"]["language"], "zh-CN");

    // 3. Write client preferences
    let req = json_with_token(
        "PUT",
        "/api/settings/client",
        json!({"theme": "dark", "sidebar.width": 280}),
        &token,
        &csrf,
    );
    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    // 4. Verify preferences
    let resp = app
        .clone()
        .oneshot(get_with_token("/api/settings/client", &token))
        .await
        .unwrap();
    let json = body_json(resp).await;
    assert_eq!(json["data"]["theme"], "dark");
    assert_eq!(json["data"]["sidebar.width"], 280);

    // 5. Get system info
    let resp = app
        .clone()
        .oneshot(get_with_token("/api/system/info", &token))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let json = body_json(resp).await;
    assert!(json["data"]["platform"].as_str().is_some());

    // 6. Create provider
    let req = json_with_token(
        "POST",
        "/api/providers",
        json!({
            "platform": "openai",
            "name": "OpenAI",
            "base_url": "https://api.openai.com",
            "api_key": "sk-proj-test-key-1234"
        }),
        &token,
        &csrf,
    );
    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);
    let json = body_json(resp).await;
    let provider_id = json["data"]["provider_id"].as_str().unwrap().to_string();
    assert_eq!(json["data"]["api_key"], "sk-proj-test-key-1234");

    // 7. List providers
    let resp = app
        .clone()
        .oneshot(get_with_token("/api/providers", &token))
        .await
        .unwrap();
    let json = body_json(resp).await;
    let providers = json["data"].as_array().unwrap();
    assert_eq!(providers.len(), 2);
    assert!(
        providers
            .iter()
            .any(|provider| provider["platform"] == "nomifun-free-model")
    );
    assert!(
        providers
            .iter()
            .any(|provider| provider["provider_id"].as_str() == Some(provider_id.as_str()))
    );

    // 8. Delete provider
    let resp = app
        .oneshot(delete_with_token(
            &format!("/api/providers/{provider_id}"),
            &token,
            &csrf,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
}

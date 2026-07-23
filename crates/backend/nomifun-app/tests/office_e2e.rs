//! E2E tests for office HTTP endpoints.
//!
//! Covers test-plan items:
//! - AU-1/AU-2: Unauthenticated access rejected
//! - SH-1..SH-7: Snapshot CRUD (save, list, get-content, not-found, trim, isolation, target combos)
//! - SO-1/SO-2: Star Office detection (no service available)
//! - DC-1/DC-4/DC-9: Document conversion (Excel→JSON, file not found, invalid target)
//! - RP-2/RP-4: Proxy SSRF protection (inactive port rejected)
//! - WP-4: Word preview start when officecli not available
//!
//! Items requiring real officecli or mock HTTP backends (WP-1..3, WP-5..6, EP-1..2,
//! PP-1..3, RP-1/RP-3, RP-5..7, SO-5..6, DC-5..8) are tested at the service
//! integration level in `nomifun-office/tests/`.

mod common;

use std::sync::Arc;

use axum::http::StatusCode;
use serde_json::json;
use tower::ServiceExt;

use common::{body_json, get_request, json_with_token, setup_and_login};

use nomifun_app::{AppConfig, AppServices, build_module_states, create_router_with_states};
use nomifun_office::{
    ConversionService, OfficeRouterState, OfficecliWatchManager, ProxyService, SnapshotService, StarOfficeDetector,
};

// ── Helpers ──────────────────────────────────────────────────────────

async fn build_office_app() -> (axum::Router, AppServices, tempfile::TempDir) {
    let default_roots = vec![
        std::env::temp_dir(),
        dirs::home_dir().unwrap_or_else(std::env::temp_dir),
    ];
    build_office_app_with_roots(default_roots).await
}

async fn build_office_app_with_roots(
    allowed_roots: Vec<std::path::PathBuf>,
) -> (axum::Router, AppServices, tempfile::TempDir) {
    let tmp = tempfile::TempDir::new().unwrap();
    let data_dir = tmp.path().to_path_buf();

    let db = nomifun_db::init_database_memory().await.unwrap();
    let config = AppConfig {
        data_dir: data_dir.clone(),
        work_dir: data_dir,
        ..Default::default()
    };
    let services = AppServices::from_config(db, &config).await.unwrap();
    let (mut states, _) = build_module_states(&services).await;

    states.office = build_test_office_state(tmp.path(), allowed_roots);

    let router = create_router_with_states(&services, states);
    (router, services, tmp)
}

fn build_test_office_state(data_dir: &std::path::Path, allowed_roots: Vec<std::path::PathBuf>) -> OfficeRouterState {
    use nomifun_office::error::OfficeError;
    use nomifun_office::types::DocType;
    use nomifun_office::{ProcessHandle, ProcessSpawner};

    struct NoopSpawner;

    #[async_trait::async_trait]
    impl ProcessSpawner for NoopSpawner {
        async fn spawn_officecli(
            &self,
            _file_path: &str,
            _port: u16,
            _doc_type: DocType,
        ) -> Result<Box<dyn ProcessHandle>, OfficeError> {
            Err(OfficeError::OfficecliNotFound)
        }
        async fn install_officecli(&self) -> Result<(), OfficeError> {
            Err(OfficeError::InstallFailed("not available in test".into()))
        }
        async fn is_officecli_installed(&self) -> bool {
            false
        }
        async fn check_update(&self, _doc_type: DocType) -> Result<(), OfficeError> {
            Ok(())
        }
    }

    struct NoopBroadcaster;
    impl nomifun_realtime::UserEventSink for NoopBroadcaster {
        fn send_to_user(
            &self,
            _user_id: &str,
            _event: nomifun_api_types::WebSocketMessage<serde_json::Value>,
        ) {
        }
    }

    let spawner: Arc<dyn ProcessSpawner> = Arc::new(NoopSpawner);
    let bc: Arc<dyn nomifun_realtime::UserEventSink> = Arc::new(NoopBroadcaster);
    let wm = Arc::new(OfficecliWatchManager::new(spawner, bc));

    let snapshot = Arc::new(SnapshotService::new(data_dir));
    let detector = Arc::new(StarOfficeDetector::local());
    let conversion = Arc::new(ConversionService::new(None));
    let proxy = Arc::new(ProxyService::new(wm.clone()));

    OfficeRouterState {
        watch_manager: wm,
        snapshot_service: snapshot,
        star_office_detector: detector,
        conversion_service: conversion,
        proxy_service: proxy,
        allowed_roots,
    }
}

fn snapshot_target() -> serde_json::Value {
    json!({"content_type": "markdown", "file_path": "/a.md"})
}

// ── AU-1/AU-2: Unauthenticated requests ─────────────────────────────

#[tokio::test]
async fn au1_unauthenticated_preview_start_returns_403() {
    let (app, _services, _tmp) = build_office_app().await;
    let req = common::get_request("/api/word-preview/start");
    let resp = app.oneshot(req).await.unwrap();
    assert!(
        resp.status() == StatusCode::UNAUTHORIZED || resp.status() == StatusCode::FORBIDDEN,
        "expected 401 or 403, got {}",
        resp.status()
    );
}

#[tokio::test]
async fn au2_unauthenticated_all_office_endpoints() {
    let endpoints = [
        "/api/word-preview/start",
        "/api/excel-preview/start",
        "/api/ppt-preview/start",
        "/api/preview-history/list",
        "/api/preview-history/save",
        "/api/star-office/detect",
        "/api/document/convert",
    ];

    for endpoint in endpoints {
        let (app, _services, _tmp) = build_office_app().await;
        let body = json!({});
        let req = axum::http::Request::builder()
            .method("POST")
            .uri(endpoint)
            .header("content-type", "application/json")
            .body(axum::body::Body::from(serde_json::to_vec(&body).unwrap()))
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert!(
            resp.status() == StatusCode::UNAUTHORIZED || resp.status() == StatusCode::FORBIDDEN,
            "endpoint {endpoint}: expected 401 or 403, got {}",
            resp.status()
        );
    }
}

// ── WP-4: Word preview start (officecli not available) ───────────────

#[tokio::test]
async fn wp4_word_preview_officecli_not_available() {
    let (mut app, services, tmp) = build_office_app().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "pass123").await;

    let file_path = tmp.path().join("test.docx");
    std::fs::write(&file_path, b"docx").unwrap();

    let body = json!({"file_path": file_path.to_str().unwrap()});
    let req = json_with_token("POST", "/api/word-preview/start", body, &token, &csrf);
    let resp = app.clone().oneshot(req).await.unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
    let json = body_json(resp).await;
    assert_eq!(json["success"], true);
    let url = json["data"]["url"].as_str().unwrap();
    assert!(url.is_empty(), "url should be empty when officecli unavailable");
    assert_eq!(json["data"]["error"], "OFFICECLI_INSTALL_FAILED");
}

#[tokio::test]
async fn wp5_word_preview_with_workspace_accepts_non_sandbox_path() {
    let sandbox = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    let file_path = outside.path().join("demo.docx");
    std::fs::write(&file_path, b"docx").unwrap();

    let (mut app, services, _tmp) = build_office_app_with_roots(vec![sandbox.path().to_path_buf()]).await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "pass123").await;

    let body = json!({
        "file_path": file_path.to_str().unwrap(),
        "workspace": outside.path().to_str().unwrap()
    });
    let req = json_with_token("POST", "/api/word-preview/start", body, &token, &csrf);
    let resp = app.clone().oneshot(req).await.unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
    let json = body_json(resp).await;
    assert_eq!(json["success"], true);
    assert_eq!(json["data"]["error"], "OFFICECLI_INSTALL_FAILED");
}

#[tokio::test]
async fn wp6_word_preview_without_workspace_rejects_non_sandbox_path() {
    let sandbox = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    let file_path = outside.path().join("demo.docx");
    std::fs::write(&file_path, b"docx").unwrap();

    let (mut app, services, _tmp) = build_office_app_with_roots(vec![sandbox.path().to_path_buf()]).await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "pass123").await;

    let body = json!({
        "file_path": file_path.to_str().unwrap()
    });
    let req = json_with_token("POST", "/api/word-preview/start", body, &token, &csrf);
    let resp = app.clone().oneshot(req).await.unwrap();

    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    let json = body_json(resp).await;
    assert_eq!(json["code"], "PATH_OUTSIDE_SANDBOX");
}

#[tokio::test]
async fn ep1_excel_preview_with_workspace_accepts_non_sandbox_path() {
    let sandbox = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    let file_path = outside.path().join("demo.xlsx");
    std::fs::write(&file_path, b"xlsx").unwrap();

    let (mut app, services, _tmp) = build_office_app_with_roots(vec![sandbox.path().to_path_buf()]).await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "pass123").await;

    let body = json!({
        "file_path": file_path.to_str().unwrap(),
        "workspace": outside.path().to_str().unwrap()
    });
    let req = json_with_token("POST", "/api/excel-preview/start", body, &token, &csrf);
    let resp = app.clone().oneshot(req).await.unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
    let json = body_json(resp).await;
    assert_eq!(json["success"], true);
    assert_eq!(json["data"]["error"], "OFFICECLI_INSTALL_FAILED");
}

#[tokio::test]
async fn pp1_ppt_preview_with_workspace_accepts_non_sandbox_path() {
    let sandbox = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    let file_path = outside.path().join("demo.pptx");
    std::fs::write(&file_path, b"pptx").unwrap();

    let (mut app, services, _tmp) = build_office_app_with_roots(vec![sandbox.path().to_path_buf()]).await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "pass123").await;

    let body = json!({
        "file_path": file_path.to_str().unwrap(),
        "workspace": outside.path().to_str().unwrap()
    });
    let req = json_with_token("POST", "/api/ppt-preview/start", body, &token, &csrf);
    let resp = app.clone().oneshot(req).await.unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
    let json = body_json(resp).await;
    assert_eq!(json["success"], true);
    assert_eq!(json["data"]["error"], "OFFICECLI_INSTALL_FAILED");
}

// ── SH-1: Save snapshot ─────────────────────────────────────────────

#[tokio::test]
async fn sh1_save_snapshot() {
    let (mut app, services, _tmp) = build_office_app().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "pass123").await;

    let body = json!({
        "target": snapshot_target(),
        "content": "# Hello World"
    });
    let req = json_with_token("POST", "/api/preview-history/save", body, &token, &csrf);
    let resp = app.clone().oneshot(req).await.unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
    let json = body_json(resp).await;
    assert_eq!(json["success"], true);

    let data = &json["data"];
    assert!(data["snapshot_id"].is_string());
    assert!(!data["snapshot_id"].as_str().unwrap().is_empty());
    assert!(data.get("id").is_none());
    assert!(data["created_at"].is_number());
    assert_eq!(data["size"], 13); // "# Hello World".len()
    assert_eq!(data["content_type"], "markdown");
}

// ── SH-2: List snapshots ────────────────────────────────────────────

#[tokio::test]
async fn sh2_list_snapshots() {
    let (mut app, services, _tmp) = build_office_app().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "pass123").await;

    for i in 0..3 {
        let body = json!({
            "target": snapshot_target(),
            "content": format!("content {i}")
        });
        let req = json_with_token("POST", "/api/preview-history/save", body, &token, &csrf);
        let resp = app.clone().oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    let body = json!({"target": snapshot_target()});
    let req = json_with_token("POST", "/api/preview-history/list", body, &token, &csrf);
    let resp = app.clone().oneshot(req).await.unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
    let json = body_json(resp).await;
    assert_eq!(json["success"], true);

    let snapshots = json["data"].as_array().unwrap();
    assert_eq!(snapshots.len(), 3);
}

// ── SH-3: Get snapshot content ──────────────────────────────────────

#[tokio::test]
async fn sh3_get_snapshot_content() {
    let (mut app, services, _tmp) = build_office_app().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "pass123").await;

    let save_body = json!({
        "target": snapshot_target(),
        "content": "# Hello"
    });
    let req = json_with_token("POST", "/api/preview-history/save", save_body, &token, &csrf);
    let resp = app.clone().oneshot(req).await.unwrap();
    let save_json = body_json(resp).await;
    let snapshot_id = save_json["data"]["snapshot_id"].as_str().unwrap();

    let get_body = json!({
        "target": snapshot_target(),
        "snapshot_id": snapshot_id
    });
    let req = json_with_token("POST", "/api/preview-history/get-content", get_body, &token, &csrf);
    let resp = app.clone().oneshot(req).await.unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
    let json = body_json(resp).await;
    assert_eq!(json["success"], true);
    assert_eq!(json["data"]["content"], "# Hello");
    assert_eq!(json["data"]["snapshot"]["snapshot_id"], snapshot_id);
    assert!(json["data"]["snapshot"].get("id").is_none());
}

// ── SH-4: Get nonexistent snapshot ──────────────────────────────────

#[tokio::test]
async fn sh4_get_nonexistent_snapshot() {
    let (mut app, services, _tmp) = build_office_app().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "pass123").await;

    let body = json!({
        "target": snapshot_target(),
        "snapshot_id": "0190f5fe-7c00-7a00-8000-000000000099"
    });
    let req = json_with_token("POST", "/api/preview-history/get-content", body, &token, &csrf);
    let resp = app.clone().oneshot(req).await.unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
    let json = body_json(resp).await;
    assert_eq!(json["success"], true);
    assert!(json["data"].is_null());
}

// ── SH-5: Snapshot trimming at 50 limit ─────────────────────────────

#[tokio::test]
async fn sh5_snapshot_trim_at_limit() {
    let (mut app, services, _tmp) = build_office_app().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "pass123").await;

    for i in 0..52 {
        let body = json!({
            "target": snapshot_target(),
            "content": format!("snap {i}")
        });
        let req = json_with_token("POST", "/api/preview-history/save", body, &token, &csrf);
        let resp = app.clone().oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    let body = json!({"target": snapshot_target()});
    let req = json_with_token("POST", "/api/preview-history/list", body, &token, &csrf);
    let resp = app.clone().oneshot(req).await.unwrap();

    let json = body_json(resp).await;
    let snapshots = json["data"].as_array().unwrap();
    assert!(
        snapshots.len() <= 50,
        "expected at most 50 snapshots, got {}",
        snapshots.len()
    );
}

// ── SH-6: Different targets are isolated ────────────────────────────

#[tokio::test]
async fn sh6_different_targets_isolated() {
    let (mut app, services, _tmp) = build_office_app().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "pass123").await;

    let target_a = json!({"content_type": "markdown", "file_path": "/a.md"});
    let target_b = json!({"content_type": "code", "file_path": "/b.rs"});

    let body_a = json!({"target": target_a, "content": "AAA"});
    let req = json_with_token("POST", "/api/preview-history/save", body_a, &token, &csrf);
    app.clone().oneshot(req).await.unwrap();

    let body_b = json!({"target": target_b, "content": "BBB"});
    let req = json_with_token("POST", "/api/preview-history/save", body_b, &token, &csrf);
    app.clone().oneshot(req).await.unwrap();

    let list_a = json!({"target": target_a});
    let req = json_with_token("POST", "/api/preview-history/list", list_a, &token, &csrf);
    let resp = app.clone().oneshot(req).await.unwrap();
    let json_a = body_json(resp).await;
    assert_eq!(json_a["data"].as_array().unwrap().len(), 1);

    let list_b = json!({"target": target_b});
    let req = json_with_token("POST", "/api/preview-history/list", list_b, &token, &csrf);
    let resp = app.clone().oneshot(req).await.unwrap();
    let json_b = body_json(resp).await;
    assert_eq!(json_b["data"].as_array().unwrap().len(), 1);
}

// ── SH-7: Target with multiple fields produces different hash ───────

#[tokio::test]
async fn sh7_target_field_combination_different_hash() {
    let (mut app, services, _tmp) = build_office_app().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "pass123").await;

    let target_simple = json!({"content_type": "markdown", "file_path": "/a.md"});
    let target_complex = json!({
        "content_type": "markdown",
        "file_path": "/a.md",
        "workspace": "/ws",
        "conversation_id": "0190f5fe-7c00-7a00-8000-000000000016"
    });

    let body = json!({"target": target_simple, "content": "simple"});
    let req = json_with_token("POST", "/api/preview-history/save", body, &token, &csrf);
    app.clone().oneshot(req).await.unwrap();

    let body = json!({"target": target_complex, "content": "complex"});
    let req = json_with_token("POST", "/api/preview-history/save", body, &token, &csrf);
    app.clone().oneshot(req).await.unwrap();

    let list = json!({"target": target_simple});
    let req = json_with_token("POST", "/api/preview-history/list", list, &token, &csrf);
    let resp = app.clone().oneshot(req).await.unwrap();
    let json = body_json(resp).await;
    assert_eq!(
        json["data"].as_array().unwrap().len(),
        1,
        "simple target should only have 1 snapshot"
    );

    let list = json!({"target": target_complex});
    let req = json_with_token("POST", "/api/preview-history/list", list, &token, &csrf);
    let resp = app.clone().oneshot(req).await.unwrap();
    let json = body_json(resp).await;
    assert_eq!(
        json["data"].as_array().unwrap().len(),
        1,
        "complex target should only have 1 snapshot"
    );
}

// ── SO-1: Star Office detect — no service available ─────────────────

#[tokio::test]
async fn so1_detect_no_service() {
    let (mut app, services, _tmp) = build_office_app().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "pass123").await;

    let body = json!({});
    let req = json_with_token("POST", "/api/star-office/detect", body, &token, &csrf);
    let resp = app.clone().oneshot(req).await.unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
    let json = body_json(resp).await;
    assert_eq!(json["success"], true);
    assert!(json["data"]["url"].is_null());
}

// ── SO-2: Star Office detect with preferred URL ─────────────────────

#[tokio::test]
async fn so2_detect_with_preferred_url() {
    let (mut app, services, _tmp) = build_office_app().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "pass123").await;

    let body = json!({"preferred_url": "http://localhost:19000"});
    let req = json_with_token("POST", "/api/star-office/detect", body, &token, &csrf);
    let resp = app.clone().oneshot(req).await.unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
    let json = body_json(resp).await;
    assert_eq!(json["success"], true);
    assert!(json["data"]["url"].is_null());
}

// ── DC-1: Excel → JSON ──────────────────────────────────────────────

#[tokio::test]
async fn dc1_excel_to_json() {
    let (mut app, services, tmp) = build_office_app().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "pass123").await;

    let xlsx_path = tmp.path().join("test.xlsx");
    create_test_xlsx(&xlsx_path);

    let body = json!({
        "file_path": xlsx_path.to_str().unwrap(),
        "to": "excel-json"
    });
    let req = json_with_token("POST", "/api/document/convert", body, &token, &csrf);
    let resp = app.clone().oneshot(req).await.unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
    let json = body_json(resp).await;
    assert_eq!(json["success"], true);
    assert_eq!(json["data"]["to"], "excel-json");
    assert_eq!(json["data"]["result"]["success"], true);

    let sheets = json["data"]["result"]["data"]["sheets"].as_array().unwrap();
    assert!(!sheets.is_empty());
    assert!(sheets[0]["name"].is_string());
    assert!(sheets[0]["data"].is_array());
}

// ── DC-4: Excel → JSON (file not found) ─────────────────────────────

#[tokio::test]
async fn dc4_excel_file_not_found() {
    let (mut app, services, _tmp) = build_office_app().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "pass123").await;

    let body = json!({
        "file_path": "/nonexistent/file.xlsx",
        "to": "excel-json"
    });
    let req = json_with_token("POST", "/api/document/convert", body, &token, &csrf);
    let resp = app.clone().oneshot(req).await.unwrap();

    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let json = body_json(resp).await;
    assert_eq!(json["code"], "BAD_REQUEST");
}

#[tokio::test]
async fn dc5_document_convert_rejects_outside_sandbox() {
    let sandbox = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    let xlsx_path = outside.path().join("test.xlsx");
    create_test_xlsx(&xlsx_path);

    let (mut app, services, _tmp) = build_office_app_with_roots(vec![sandbox.path().to_path_buf()]).await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "pass123").await;

    let body = json!({
        "file_path": xlsx_path.to_str().unwrap(),
        "to": "excel-json"
    });
    let req = json_with_token("POST", "/api/document/convert", body, &token, &csrf);
    let resp = app.clone().oneshot(req).await.unwrap();

    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    let json = body_json(resp).await;
    assert_eq!(json["code"], "PATH_OUTSIDE_SANDBOX");
}

// ── DC-9: Invalid conversion target ─────────────────────────────────

#[tokio::test]
async fn dc9_invalid_conversion_target() {
    let (mut app, services, _tmp) = build_office_app().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "pass123").await;

    let body = json!({
        "file_path": "/path/to/file.txt",
        "to": "invalid"
    });
    let req = json_with_token("POST", "/api/document/convert", body, &token, &csrf);
    let resp = app.clone().oneshot(req).await.unwrap();

    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

// ── RP-2: PPT proxy SSRF protection ─────────────────────────────────

#[tokio::test]
async fn rp2_ppt_proxy_ssrf_inactive_port() {
    let (app, _services, _tmp) = build_office_app().await;

    let req = get_request("/api/ppt-proxy/8080/index.html");
    let resp = app.oneshot(req).await.unwrap();

    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
}

// ── RP-4: Office watch proxy SSRF protection ────────────────────────

#[tokio::test]
async fn rp4_office_watch_proxy_ssrf_inactive_port() {
    let (app, _services, _tmp) = build_office_app().await;

    let req = get_request("/api/office-watch-proxy/9999/index.html");
    let resp = app.oneshot(req).await.unwrap();

    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
}

// ── RP-root: proxy root path (no trailing path segment) ─────────────

#[tokio::test]
async fn ppt_proxy_root_path_returns_non_404() {
    let (app, _services, _tmp) = build_office_app().await;

    let req = get_request("/api/ppt-proxy/19999");
    let resp = app.oneshot(req).await.unwrap();

    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn office_watch_proxy_root_path_returns_non_404() {
    let (app, _services, _tmp) = build_office_app().await;

    let req = get_request("/api/office-watch-proxy/19999");
    let resp = app.oneshot(req).await.unwrap();

    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
}

// ── Test utilities ──────────────────────────────────────────────────

fn create_test_xlsx(path: &std::path::Path) {
    use rust_xlsxwriter::Workbook;

    let mut workbook = Workbook::new();
    let worksheet = workbook.add_worksheet();
    worksheet.write_string(0, 0, "Name").unwrap();
    worksheet.write_string(0, 1, "Age").unwrap();
    worksheet.write_string(1, 0, "Alice").unwrap();
    worksheet.write_number(1, 1, 30.0).unwrap();
    workbook.save(path).unwrap();
}

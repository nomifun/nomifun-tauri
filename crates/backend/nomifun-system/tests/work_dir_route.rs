//! Integration tests for `POST /api/system/work-dir` — persisting the user's
//! chosen working directory to the pre-boot dir config that the next boot reads.

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use serde_json::json;
use tower::ServiceExt;

use nomifun_db::{
    SqliteClientPreferenceRepository, SqliteProviderRepository, SqliteSettingsRepository, init_database_memory,
};
use nomifun_system::{
    ClientPrefService, ModelFetchService, ProtocolDetectionService, ProviderService, SettingsService,
    SystemRouterState, VersionCheckService, system_routes,
};

const TEST_KEY: [u8; 32] = [0x42; 32];

fn unique_data_dir(tag: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("nomifun-wdroute-{tag}-{}", nomifun_common::now_ms()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

async fn setup(data_dir: std::path::PathBuf) -> axum::Router {
    let db = init_database_memory().await.unwrap();
    let provider_repo = Arc::new(SqliteProviderRepository::new(db.pool().clone()));
    let http_client = reqwest::Client::new();
    let state = SystemRouterState {
        settings_service: SettingsService::new(Arc::new(SqliteSettingsRepository::new(db.pool().clone()))),
        client_pref_service: ClientPrefService::new(Arc::new(SqliteClientPreferenceRepository::new(db.pool().clone()))),
        provider_service: ProviderService::new(provider_repo.clone(), TEST_KEY),
        model_fetch_service: ModelFetchService::new(provider_repo, TEST_KEY, http_client.clone()),
        model_profile_service: nomifun_system::ModelProfileService::new(std::sync::Arc::new(
            nomifun_db::SqliteModelProfileRepository::new(db.pool().clone()),
        )),
        protocol_detection_service: ProtocolDetectionService::new(http_client.clone()),
        version_check_service: VersionCheckService::new(http_client, "1.0.0".to_owned()),
        data_dir,
    };
    system_routes(state)
}

fn post(uri: &str, body: serde_json::Value) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri(uri)
        .header("content-type", "application/json")
        .body(Body::from(serde_json::to_vec(&body).unwrap()))
        .unwrap()
}

async fn body_json(resp: axum::response::Response) -> serde_json::Value {
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    serde_json::from_slice(&bytes).unwrap()
}

#[tokio::test]
async fn persists_valid_work_dir_and_creates_it() {
    let data_dir = unique_data_dir("valid");
    let target = data_dir.join("chosen-workspace"); // absolute, does not exist yet
    let app = setup(data_dir.clone()).await;

    let resp = app
        .oneshot(post("/api/system/work-dir", json!({ "work_dir": target.to_str().unwrap() })))
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(body_json(resp).await["success"], true);
    // The handler creates the directory and persists the choice so the next
    // boot's resolve_work_dir picks it up.
    assert!(target.is_dir(), "target work dir should have been created");
    assert_eq!(
        nomifun_common::dir_config::persisted_work_dir(&data_dir).as_deref(),
        Some(target.as_path())
    );

    let _ = std::fs::remove_dir_all(&data_dir);
}

#[tokio::test]
async fn rejects_relative_work_dir() {
    let data_dir = unique_data_dir("relative");
    let app = setup(data_dir.clone()).await;

    let resp = app
        .oneshot(post("/api/system/work-dir", json!({ "work_dir": "relative/path" })))
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    assert!(nomifun_common::dir_config::persisted_work_dir(&data_dir).is_none());

    let _ = std::fs::remove_dir_all(&data_dir);
}

#[tokio::test]
async fn rejects_blank_work_dir() {
    let data_dir = unique_data_dir("blank");
    let app = setup(data_dir.clone()).await;

    let resp = app
        .oneshot(post("/api/system/work-dir", json!({ "work_dir": "   " })))
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    assert!(nomifun_common::dir_config::persisted_work_dir(&data_dir).is_none());

    let _ = std::fs::remove_dir_all(&data_dir);
}

#[tokio::test]
async fn rejects_work_dir_with_edge_whitespace_segment() {
    let data_dir = unique_data_dir("ws-space");
    // A path segment that begins/ends with whitespace ("bad ") — the repo refuses
    // these for conversation workspaces (workspace_path_has_edge_whitespace_segment),
    // so the work dir gatekeeper must reject it up front (deterministically, with
    // the dedicated error code) instead of relying on the OS to fail create_dir_all.
    let bad = data_dir.join("bad ").join("inner");
    let app = setup(data_dir.clone()).await;

    let resp = app
        .oneshot(post("/api/system/work-dir", json!({ "work_dir": bad.to_str().unwrap() })))
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let body = body_json(resp).await;
    assert_eq!(body["code"], "WORKSPACE_PATH_EDGE_WHITESPACE_UNSUPPORTED");
    assert!(nomifun_common::dir_config::persisted_work_dir(&data_dir).is_none());

    let _ = std::fs::remove_dir_all(&data_dir);
}

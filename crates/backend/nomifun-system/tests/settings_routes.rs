//! Black-box integration tests for system settings routes.
//!
//! Tests exercise the HTTP layer (request → handler → response) via
//! `tower::ServiceExt::oneshot`, without authentication middleware.
//! Auth protection is verified at the app-level E2E tests (task 3.9).

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use tower::ServiceExt;

use nomifun_db::{
    SqliteClientPreferenceRepository, SqliteProviderRepository, SqliteSettingsRepository, init_database_memory,
};
use nomifun_system::{
    ClientPrefService, ModelFetchService, ProtocolDetectionService, ProviderService, SettingsService,
    SystemRouterState, VersionCheckService, settings_routes,
};

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

const TEST_ENCRYPTION_KEY: [u8; 32] = [0x42; 32];

fn build_state(db: &nomifun_db::Database) -> SystemRouterState {
    let provider_repo = Arc::new(SqliteProviderRepository::new(db.pool().clone()));
    let http_client = reqwest::Client::new();
    SystemRouterState {
        settings_service: SettingsService::new(Arc::new(SqliteSettingsRepository::new(db.pool().clone()))),
        client_pref_service: ClientPrefService::new(Arc::new(SqliteClientPreferenceRepository::new(db.pool().clone()))),
        provider_service: ProviderService::new(provider_repo.clone(), TEST_ENCRYPTION_KEY),
        model_fetch_service: ModelFetchService::new(provider_repo, TEST_ENCRYPTION_KEY, http_client.clone()),
        model_profile_service: nomifun_system::ModelProfileService::new(std::sync::Arc::new(
            nomifun_db::SqliteModelProfileRepository::new(db.pool().clone()),
        )),
        managed_model_service: None,
        protocol_detection_service: ProtocolDetectionService::new(http_client.clone()),
        version_check_service: VersionCheckService::new(http_client, "0.1.0".to_owned()),
        data_dir: std::env::temp_dir(),
    }
}

async fn setup() -> (axum::Router, nomifun_db::Database) {
    let db = init_database_memory().await.unwrap();
    let state = build_state(&db);
    (settings_routes(state), db)
}

async fn body_json(resp: axum::response::Response) -> serde_json::Value {
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    serde_json::from_slice(&bytes).unwrap()
}

fn get_request(uri: &str) -> Request<Body> {
    Request::builder().method("GET").uri(uri).body(Body::empty()).unwrap()
}

fn json_request(method: &str, uri: &str, body: serde_json::Value) -> Request<Body> {
    Request::builder()
        .method(method)
        .uri(uri)
        .header("content-type", "application/json")
        .body(Body::from(serde_json::to_vec(&body).unwrap()))
        .unwrap()
}

// ===========================================================================
// System Settings (GET/PATCH /api/settings)
// ===========================================================================

#[tokio::test]
async fn get_settings_default_values() {
    let (app, _db) = setup().await;
    let resp = app.oneshot(get_request("/api/settings")).await.unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
    let json = body_json(resp).await;
    assert_eq!(json["success"], true);
    assert_eq!(json["data"]["language"], "en-US");
    assert_eq!(json["data"]["notification_enabled"], true);
    assert_eq!(json["data"]["cron_notification_enabled"], false);
    assert_eq!(json["data"]["command_queue_enabled"], false);
    assert_eq!(json["data"]["save_upload_to_workspace"], false);
}

#[tokio::test]
async fn patch_settings_single_field() {
    let (app, _db) = setup().await;
    let req = json_request("PATCH", "/api/settings", serde_json::json!({"language": "zh-CN"}));
    let resp = app.oneshot(req).await.unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
    let json = body_json(resp).await;
    assert_eq!(json["success"], true);
    assert_eq!(json["data"]["language"], "zh-CN");
    // Others remain default
    assert_eq!(json["data"]["notification_enabled"], true);
    assert_eq!(json["data"]["cron_notification_enabled"], false);
}

#[tokio::test]
async fn patch_settings_multiple_fields() {
    let (app, _db) = setup().await;
    let req = json_request(
        "PATCH",
        "/api/settings",
        serde_json::json!({
            "notification_enabled": false,
            "command_queue_enabled": true
        }),
    );
    let resp = app.oneshot(req).await.unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
    let json = body_json(resp).await;
    assert_eq!(json["data"]["notification_enabled"], false);
    assert_eq!(json["data"]["command_queue_enabled"], true);
    assert_eq!(json["data"]["language"], "en-US");
}

#[tokio::test]
async fn patch_settings_empty_body() {
    let (app, _db) = setup().await;
    let req = json_request("PATCH", "/api/settings", serde_json::json!({}));
    let resp = app.oneshot(req).await.unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
    let json = body_json(resp).await;
    assert_eq!(json["data"]["language"], "en-US");
}

#[tokio::test]
async fn patch_settings_unsupported_language_rejected() {
    let (app, _db) = setup().await;
    let req = json_request(
        "PATCH",
        "/api/settings",
        serde_json::json!({"language": "invalid-lang"}),
    );
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn patch_settings_type_error_rejected() {
    let (app, _db) = setup().await;
    let req = json_request(
        "PATCH",
        "/api/settings",
        serde_json::json!({"notification_enabled": "yes"}),
    );
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn patch_settings_unknown_field_ignored() {
    let (app, _db) = setup().await;
    let req = json_request("PATCH", "/api/settings", serde_json::json!({"unknown_field": 123}));
    let resp = app.oneshot(req).await.unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
    let json = body_json(resp).await;
    assert_eq!(json["data"]["language"], "en-US");
}

#[tokio::test]
async fn patch_then_get_reflects_changes() {
    let (app, db) = setup().await;

    // First PATCH to update
    let req = json_request(
        "PATCH",
        "/api/settings",
        serde_json::json!({"language": "zh-CN", "save_upload_to_workspace": true}),
    );
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    // Build a fresh router with the same DB to GET
    let app2 = settings_routes(build_state(&db));

    let resp = app2.oneshot(get_request("/api/settings")).await.unwrap();
    let json = body_json(resp).await;
    assert_eq!(json["data"]["language"], "zh-CN");
    assert_eq!(json["data"]["save_upload_to_workspace"], true);
}

// ===========================================================================
// Client Preferences (GET/PUT /api/settings/client)
// ===========================================================================

#[tokio::test]
async fn get_client_prefs_empty() {
    let (app, _db) = setup().await;
    let resp = app.oneshot(get_request("/api/settings/client")).await.unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
    let json = body_json(resp).await;
    assert_eq!(json["success"], true);
    assert_eq!(json["data"], serde_json::json!({}));
}

#[tokio::test]
async fn put_and_get_boolean_value() {
    let (app, db) = setup().await;

    let req = json_request(
        "PUT",
        "/api/settings/client",
        serde_json::json!({"system.closeToTray": true}),
    );
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let app2 = settings_routes(build_state(&db));

    let resp = app2.oneshot(get_request("/api/settings/client")).await.unwrap();
    let json = body_json(resp).await;
    assert_eq!(json["data"]["system.closeToTray"], true);
}

#[tokio::test]
async fn put_and_get_number_value() {
    let (app, db) = setup().await;

    let req = json_request("PUT", "/api/settings/client", serde_json::json!({"companion.size": 360}));
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let app2 = settings_routes(build_state(&db));

    let resp = app2.oneshot(get_request("/api/settings/client")).await.unwrap();
    let json = body_json(resp).await;
    assert_eq!(json["data"]["companion.size"], 360);
}

#[tokio::test]
async fn put_and_get_string_value() {
    let (app, db) = setup().await;

    let req = json_request("PUT", "/api/settings/client", serde_json::json!({"theme": "dark"}));
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let app2 = settings_routes(build_state(&db));

    let resp = app2.oneshot(get_request("/api/settings/client")).await.unwrap();
    let json = body_json(resp).await;
    assert_eq!(json["data"]["theme"], "dark");
}

#[tokio::test]
async fn put_null_deletes_key() {
    let (app, db) = setup().await;

    // First write a value
    let req = json_request("PUT", "/api/settings/client", serde_json::json!({"theme": "dark"}));
    app.oneshot(req).await.unwrap();

    // Then delete it with null
    let app2 = settings_routes(build_state(&db));
    let req = json_request("PUT", "/api/settings/client", serde_json::json!({"theme": null}));
    app2.oneshot(req).await.unwrap();

    // Verify it's gone
    let app3 = settings_routes(build_state(&db));
    let resp = app3.oneshot(get_request("/api/settings/client")).await.unwrap();
    let json = body_json(resp).await;
    assert_eq!(json["data"], serde_json::json!({}));
}

#[tokio::test]
async fn put_batch_write() {
    let (app, db) = setup().await;

    let req = json_request(
        "PUT",
        "/api/settings/client",
        serde_json::json!({"a": 1, "b": "x", "c": true}),
    );
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let app2 = settings_routes(build_state(&db));

    let resp = app2.oneshot(get_request("/api/settings/client")).await.unwrap();
    let json = body_json(resp).await;
    assert_eq!(json["data"]["a"], 1);
    assert_eq!(json["data"]["b"], "x");
    assert_eq!(json["data"]["c"], true);
}

#[tokio::test]
async fn get_client_prefs_with_keys_filter() {
    let (app, db) = setup().await;

    // Write several values
    let req = json_request(
        "PUT",
        "/api/settings/client",
        serde_json::json!({"a": 1, "b": 2, "c": 3}),
    );
    app.oneshot(req).await.unwrap();

    // Fetch with key filter
    let app2 = settings_routes(build_state(&db));

    let resp = app2
        .oneshot(get_request("/api/settings/client?keys=a,c"))
        .await
        .unwrap();
    let json = body_json(resp).await;

    let data = json["data"].as_object().unwrap();
    assert_eq!(data.len(), 2);
    assert_eq!(data["a"], 1);
    assert_eq!(data["c"], 3);
}

#[tokio::test]
async fn put_overwrite_existing_value() {
    let (app, db) = setup().await;

    let req = json_request("PUT", "/api/settings/client", serde_json::json!({"k": "v1"}));
    app.oneshot(req).await.unwrap();

    let app2 = settings_routes(build_state(&db));
    let req = json_request("PUT", "/api/settings/client", serde_json::json!({"k": "v2"}));
    app2.oneshot(req).await.unwrap();

    let app3 = settings_routes(build_state(&db));
    let resp = app3.oneshot(get_request("/api/settings/client")).await.unwrap();
    let json = body_json(resp).await;
    assert_eq!(json["data"]["k"], "v2");
}

#[tokio::test]
async fn put_empty_key_rejected() {
    let (app, _db) = setup().await;
    let req = json_request("PUT", "/api/settings/client", serde_json::json!({"": true}));
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn put_long_key_rejected() {
    let (app, _db) = setup().await;
    let long_key = "x".repeat(256);
    let req = json_request("PUT", "/api/settings/client", serde_json::json!({long_key: true}));
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

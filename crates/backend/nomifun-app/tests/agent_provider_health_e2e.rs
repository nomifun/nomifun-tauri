//! Provider health-check route auth and validation tests.

mod common;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::json;
use tower::ServiceExt;

use common::{body_json, build_app, json_with_token, setup_and_login};

#[tokio::test]
async fn provider_health_check_unauthenticated_is_rejected() {
    let (app, _services) = build_app().await;

    let req = Request::builder()
        .method("POST")
        .uri("/api/agents/provider-health-check")
        .header("content-type", "application/json")
        .body(Body::from(
            serde_json::to_vec(&json!({"provider_id": "0190f5fe-7c00-7a00-8000-000000000010", "model": "gpt-4o"})).unwrap(),
        ))
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();

    assert!(
        resp.status() == StatusCode::UNAUTHORIZED || resp.status() == StatusCode::FORBIDDEN,
        "expected auth rejection, got {}",
        resp.status()
    );
}

#[tokio::test]
async fn provider_health_check_requires_csrf_for_post() {
    let (mut app, services) = build_app().await;
    let (token, _csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;

    let req = Request::builder()
        .method("POST")
        .uri("/api/agents/provider-health-check")
        .header("content-type", "application/json")
        .header("authorization", format!("Bearer {token}"))
        .body(Body::from(
            serde_json::to_vec(&json!({"provider_id": "0190f5fe-7c00-7a00-8000-000000000010", "model": "gpt-4o"})).unwrap(),
        ))
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();

    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn provider_health_check_validates_required_fields() {
    let (mut app, services) = build_app().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;

    let req = json_with_token(
        "POST",
        "/api/agents/provider-health-check",
        json!({"provider_id": "", "model": "gpt-4o"}),
        &token,
        &csrf,
    );
    let resp = app.oneshot(req).await.unwrap();

    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let json = body_json(resp).await;
    assert_eq!(json["code"], "BAD_REQUEST");
    assert!(
        json["error"]
            .as_str()
            .is_some_and(|message| message.contains("invalid provider_id")),
        "expected canonical UUIDv7 provider_id contract error, got {json}"
    );
}

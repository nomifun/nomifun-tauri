//! E2E tests for the 创意工坊 (Creative Workshop) public read-only file channel
//! (M10a). The binary serve routes must be reachable WITHOUT credentials (so
//! `<img>`/`<video>` subresource loads work under the desktop's local-trust
//! policy), while every management route stays authenticated.

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use http_body_util::BodyExt;
use tower::ServiceExt;

use common::{body_json, build_app, get_request, json_with_token, setup_and_login};

/// A text asset uploaded (with auth) is then served over the public file
/// channel with NO credentials — 200 + bytes + Content-Type + Cache-Control.
#[tokio::test]
async fn workshop_file_channel_serves_without_auth() {
    let (mut app, services) = build_app().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "pass123").await;

    // Register a text asset (authenticated management route).
    let create = json_with_token(
        "POST",
        "/api/workshop/assets",
        serde_json::json!({ "kind": "text", "title": "notes", "text_content": "hello workshop" }),
        &token,
        &csrf,
    );
    let resp = app.clone().oneshot(create).await.unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED, "text asset should be created");
    let json = body_json(resp).await;
    let asset_id = json["data"]["asset_id"]
        .as_str()
        .expect("asset id")
        .to_owned();
    assert!(json["data"].get("id").is_none());
    nomifun_common::WorkshopAssetId::parse(&asset_id)
        .expect("asset id must be a bare UUIDv7");

    // Serve it back over the PUBLIC channel with no auth header / cookie.
    let resp = app
        .clone()
        .oneshot(get_request(&format!("/api/workshop/files/{asset_id}")))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK, "public file serve must not require auth");
    assert_eq!(
        resp.headers()[header::CONTENT_TYPE],
        "text/plain; charset=utf-8",
        "correct Content-Type"
    );
    assert_eq!(
        resp.headers()[header::CACHE_CONTROL],
        "private, max-age=3600",
        "Cache-Control present"
    );
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    assert_eq!(&bytes[..], b"hello workshop");
}

/// An unknown id on the public channel is a clean 404 — NOT 401/403. A 401/403
/// would mean the route is still auth-gated (the very failure mode this split
/// exists to avoid).
#[tokio::test]
async fn workshop_public_serve_missing_is_404_not_auth_rejected() {
    let (app, _services) = build_app().await;

    for uri in [
        "/api/workshop/files/0190f5fe-7c00-7a00-8000-000000009991",
        "/api/workshop/canvas-thumbs/0190f5fe-7c00-7a00-8000-000000009992",
    ] {
        let resp = app.clone().oneshot(get_request(uri)).await.unwrap();
        assert_eq!(
            resp.status(),
            StatusCode::NOT_FOUND,
            "{uri} must be a clean 404 (auth-exempt), got {}",
            resp.status()
        );
    }
}

/// Every OTHER workshop route stays authenticated: unauthenticated GETs are
/// rejected (401/403), never served.
#[tokio::test]
async fn workshop_management_routes_still_require_auth() {
    let (app, _services) = build_app().await;

    for uri in ["/api/workshop/canvases", "/api/workshop/assets"] {
        let resp = app.clone().oneshot(get_request(uri)).await.unwrap();
        assert!(
            resp.status() == StatusCode::UNAUTHORIZED || resp.status() == StatusCode::FORBIDDEN,
            "{uri} must stay auth-gated, got {}",
            resp.status()
        );
    }

    // A write route without auth is likewise rejected.
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/workshop/canvases")
                .header("content-type", "application/json")
                .body(Body::from("{}"))
                .unwrap(),
        )
        .await
        .unwrap();
    assert!(
        resp.status() == StatusCode::UNAUTHORIZED || resp.status() == StatusCode::FORBIDDEN,
        "POST create canvas must stay auth-gated, got {}",
        resp.status()
    );
}

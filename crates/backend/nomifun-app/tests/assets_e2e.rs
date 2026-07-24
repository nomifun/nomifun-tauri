use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use tower::ServiceExt;

use common::build_app;

#[tokio::test]
async fn public_logo_assets_do_not_require_auth() {
    let (app, _services) = build_app().await;
    let response = app
        .oneshot(
            Request::builder()
                .uri("/api/assets/logos/ai-major/claude.svg")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers()[header::CONTENT_TYPE], "image/svg+xml");
    assert_eq!(
        response.headers()[header::CACHE_CONTROL],
        "public, max-age=31536000, immutable"
    );
    assert!(response.headers().contains_key(header::ETAG));
}

#[tokio::test]
async fn public_logo_assets_honor_if_none_match() {
    let (app, _services) = build_app().await;
    let first = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/assets/logos/ai-major/claude.svg")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let etag = first.headers()[header::ETAG].clone();

    let response = app
        .oneshot(
            Request::builder()
                .uri("/api/assets/logos/ai-major/claude.svg")
                .header(header::IF_NONE_MATCH, etag)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::NOT_MODIFIED);
}

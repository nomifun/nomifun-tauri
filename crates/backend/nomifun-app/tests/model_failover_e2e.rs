//! Phase-3 model-failover config route tests (review #6/#12): GET defaults to
//! disabled, PUT round-trips the queue, and the path matches the frontend
//! `agentModelFailover` (`/api/agent/model-failover`).

mod common;

use axum::http::StatusCode;
use serde_json::json;
use tower::ServiceExt;

use common::{body_json, build_app, get_with_token, json_with_token, setup_and_login};

#[tokio::test]
async fn model_failover_get_defaults_to_disabled_with_auth() {
    let (mut app, services) = build_app().await;
    let (token, _csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;

    let resp = app
        .oneshot(get_with_token("/api/agent/model-failover", &token))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let json = body_json(resp).await;
    assert_eq!(json["success"], true);
    // Unset pref → ModelFailoverConfig::default() = disabled.
    assert_eq!(json["data"]["enabled"], false);
    assert_eq!(json["data"]["queue"], json!([]));
}

#[tokio::test]
async fn model_failover_put_then_get_roundtrips_with_auth() {
    let (mut app, services) = build_app().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;

    let cfg = json!({
        "enabled": true,
        "queue": [
            {"provider_id": "0190f5fe-7c00-7a00-8000-000000000010", "model": "m1"},
            {"provider_id": "0190f5fe-7c00-7a00-8000-000000000011", "model": "m2"}
        ],
        "max_switches": 3,
        "stamp_unhealthy": false
    });

    let req = json_with_token("PUT", "/api/agent/model-failover", cfg.clone(), &token, &csrf);
    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    // PUT echoes the saved config back.
    let json = body_json(resp).await;
    assert_eq!(json["data"]["enabled"], true);
    assert_eq!(json["data"]["max_switches"], 3);
    assert_eq!(json["data"]["queue"][1]["provider_id"], "0190f5fe-7c00-7a00-8000-000000000011");

    let resp = app
        .oneshot(get_with_token("/api/agent/model-failover", &token))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let json = body_json(resp).await;
    assert_eq!(json["data"]["enabled"], true);
    assert_eq!(json["data"]["stamp_unhealthy"], false);
    assert_eq!(json["data"]["queue"][0]["model"], "m1");
    assert_eq!(json["data"]["queue"][1]["model"], "m2");
}

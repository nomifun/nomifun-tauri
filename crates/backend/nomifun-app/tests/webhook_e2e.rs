//! E2E tests for the Webhook management + tag-settings + tag-bindings endpoints.

mod common;

use axum::http::StatusCode;
use serde_json::json;
use tower::ServiceExt;

use common::{body_json, build_app, delete_with_token, get_request, get_with_token, json_with_token, setup_and_login};

#[tokio::test]
async fn unauthenticated_webhook_list_is_rejected() {
    let (app, _services) = build_app().await;
    let resp = app.oneshot(get_request("/api/webhooks")).await.unwrap();
    assert!(
        resp.status() == StatusCode::UNAUTHORIZED || resp.status() == StatusCode::FORBIDDEN,
        "expected 401/403, got {}",
        resp.status()
    );
}

#[tokio::test]
async fn webhook_crud_and_secret_is_hidden() {
    let (mut app, services) = build_app().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;

    // create (with a secret)
    let body = json!({
        "name": "Team bot",
        "url": "https://open.feishu.cn/open-apis/bot/v2/hook/abc",
        "platform": "lark",
        "description": "notify",
        "secret": "s3cr3t",
        "enabled": true
    });
    let resp = app
        .clone()
        .oneshot(json_with_token("POST", "/api/webhooks", body, &token, &csrf))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);
    let json = body_json(resp).await;
    let webhook_id = json["data"]["webhook_id"].as_str().unwrap().to_owned();
    assert!(nomifun_common::validate_uuidv7(&webhook_id).is_ok());
    assert!(json["data"].get("id").is_none());
    assert_eq!(json["data"]["name"], "Team bot");
    // secret must NOT be echoed; has_secret signals presence.
    assert_eq!(json["data"]["has_secret"], true);
    assert!(json["data"].get("secret").is_none(), "secret must never be returned");

    // list
    let resp = app
        .clone()
        .oneshot(get_with_token("/api/webhooks", &token))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(body_json(resp).await["data"].as_array().unwrap().len(), 1);

    // update: rename + clear secret
    let resp = app
        .clone()
        .oneshot(json_with_token(
            "PUT",
            &format!("/api/webhooks/{webhook_id}"),
            json!({ "name": "Renamed", "secret": null, "enabled": false }),
            &token,
            &csrf,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let json = body_json(resp).await;
    assert_eq!(json["data"]["name"], "Renamed");
    assert_eq!(json["data"]["has_secret"], false);
    assert_eq!(json["data"]["enabled"], false);

    // delete
    let resp = app
        .clone()
        .oneshot(delete_with_token(&format!("/api/webhooks/{webhook_id}"), &token, &csrf))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    // get after delete → 404
    let resp = app
        .clone()
        .oneshot(get_with_token(&format!("/api/webhooks/{webhook_id}"), &token))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn webhook_create_validates_required_fields() {
    let (mut app, services) = build_app().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;
    let resp = app
        .clone()
        .oneshot(json_with_token(
            "POST",
            "/api/webhooks",
            json!({ "name": "  ", "url": "https://x" }),
            &token,
            &csrf,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn webhook_create_rejects_legacy_id_field() {
    let (mut app, services) = build_app().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;
    let resp = app
        .clone()
        .oneshot(json_with_token(
            "POST",
            "/api/webhooks",
            json!({ "id": 42, "name": "legacy", "url": "https://x" }),
            &token,
            &csrf,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn webhook_routes_reject_noncanonical_business_ids() {
    let (mut app, services) = build_app().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;

    for webhook_id in [
        "42",
        "550e8400-e29b-41d4-a716-446655440000",
        "0190F5FE-7C00-7A00-8000-000000000042",
        "webhook_0190f5fe-7c00-7a00-8000-000000000042",
    ] {
        let resp = app
            .clone()
            .oneshot(get_with_token(
                &format!("/api/webhooks/{webhook_id}"),
                &token,
            ))
            .await
            .unwrap();
        assert_eq!(
            resp.status(),
            StatusCode::BAD_REQUEST,
            "GET accepted invalid webhook_id {webhook_id:?}"
        );

        let resp = app
            .clone()
            .oneshot(json_with_token(
                "PUT",
                &format!("/api/webhooks/{webhook_id}"),
                json!({ "name": "invalid" }),
                &token,
                &csrf,
            ))
            .await
            .unwrap();
        assert_eq!(
            resp.status(),
            StatusCode::BAD_REQUEST,
            "PUT accepted invalid webhook_id {webhook_id:?}"
        );

        let resp = app
            .clone()
            .oneshot(delete_with_token(
                &format!("/api/webhooks/{webhook_id}"),
                &token,
                &csrf,
            ))
            .await
            .unwrap();
        assert_eq!(
            resp.status(),
            StatusCode::BAD_REQUEST,
            "DELETE accepted invalid webhook_id {webhook_id:?}"
        );

        let resp = app
            .clone()
            .oneshot(json_with_token(
                "POST",
                &format!("/api/webhooks/{webhook_id}/test"),
                json!({}),
                &token,
                &csrf,
            ))
            .await
            .unwrap();
        assert_eq!(
            resp.status(),
            StatusCode::BAD_REQUEST,
            "test route accepted invalid webhook_id {webhook_id:?}"
        );
    }
}

#[tokio::test]
async fn webhook_test_unreachable_url_is_bad_gateway() {
    let (mut app, services) = build_app().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;
    // create a webhook pointing at an unroutable address.
    let resp = app
        .clone()
        .oneshot(json_with_token(
            "POST",
            "/api/webhooks",
            json!({ "name": "bad", "url": "http://127.0.0.1:1/hook", "platform": "lark" }),
            &token,
            &csrf,
        ))
        .await
        .unwrap();
    let webhook_id = body_json(resp).await["data"]["webhook_id"]
        .as_str()
        .unwrap()
        .to_owned();

    // /test invokes the sender; the connection fails → 502 Bad Gateway. This
    // proves the route + sender are wired (we can't reach real Lark in tests).
    let resp = app
        .clone()
        .oneshot(json_with_token(
            "POST",
            &format!("/api/webhooks/{webhook_id}/test"),
            json!({}),
            &token,
            &csrf,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_GATEWAY);
}

#[tokio::test]
async fn tag_settings_get_default_and_upsert() {
    let (mut app, services) = build_app().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;

    // unset tag → default (unbound) shape
    let resp = app
        .clone()
        .oneshot(get_with_token("/api/tags/alpha/settings", &token))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let json = body_json(resp).await;
    assert_eq!(json["data"]["tag"], "alpha");
    assert!(json["data"]["webhook_id"].is_null());

    // create a webhook to bind
    let resp = app
        .clone()
        .oneshot(json_with_token(
            "POST",
            "/api/webhooks",
            json!({ "name": "wh", "url": "https://x/hook", "platform": "lark" }),
            &token,
            &csrf,
        ))
        .await
        .unwrap();
    let wh_id = body_json(resp).await["data"]["webhook_id"]
        .as_str()
        .unwrap()
        .to_owned();

    // bind it to the tag
    let resp = app
        .clone()
        .oneshot(json_with_token(
            "PUT",
            "/api/tags/alpha/settings",
            json!({ "webhook_id": &wh_id, "description": "queue alpha" }),
            &token,
            &csrf,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let json = body_json(resp).await;
    assert_eq!(json["data"]["webhook_id"], wh_id);
    assert_eq!(json["data"]["description"], "queue alpha");

    // binding a non-existent webhook → 400
    let resp = app
        .clone()
        .oneshot(json_with_token(
            "PUT",
            "/api/tags/alpha/settings",
            json!({ "webhook_id": "0190f5fe-7c00-7a00-8000-000000000999" }),
            &token,
            &csrf,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);

    // Hard cut: every non-canonical representation is rejected.
    for webhook_id in [
        json!(42),
        json!("42"),
        json!("550e8400-e29b-41d4-a716-446655440000"),
        json!("0190F5FE-7C00-7A00-8000-000000000042"),
        json!("webhook_0190f5fe-7c00-7a00-8000-000000000042"),
    ] {
        let resp = app
            .clone()
            .oneshot(json_with_token(
                "PUT",
                "/api/tags/alpha/settings",
                json!({ "webhook_id": webhook_id }),
                &token,
                &csrf,
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    // hard cut: the old generic id field is rejected.
    let resp = app
        .clone()
        .oneshot(json_with_token(
            "PUT",
            "/api/tags/alpha/settings",
            json!({ "id": wh_id }),
            &token,
            &csrf,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn tag_bindings_lists_enabled_autowork_conversations() {
    let (mut app, services) = build_app().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;

    // empty initially
    let resp = app
        .clone()
        .oneshot(get_with_token("/api/requirements/tag-bindings", &token))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(body_json(resp).await["data"].as_array().unwrap().len(), 0);

    // create a conversation, then enable AutoWork on it for tag "x"
    let resp = app
        .clone()
        .oneshot(json_with_token(
            "POST",
            "/api/conversations",
            json!({ "type": "acp", "name": "Conv X", "extra": { "workspace": "/project" } }),
            &token,
            &csrf,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);
    let conv_id = body_json(resp).await["data"]["conversation_id"]
        .as_str()
        .unwrap()
        .to_owned()
        .to_string();

    let resp = app
        .clone()
        .oneshot(json_with_token(
            "POST",
            "/api/requirements/autowork",
            json!({ "kind": "conversation", "target_id": conv_id, "enabled": true, "tag": "x" }),
            &token,
            &csrf,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    // tag-bindings now groups the conversation under "x"
    let resp = app
        .clone()
        .oneshot(get_with_token("/api/requirements/tag-bindings", &token))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let json = body_json(resp).await;
    let groups = json["data"].as_array().unwrap();
    let x = groups.iter().find(|g| g["tag"] == "x").expect("tag x present");
    assert_eq!(x["bindings"].as_array().unwrap().len(), 1);
    assert_eq!(x["bindings"][0]["target_id"], conv_id);
}

#[tokio::test]
async fn admin_disable_of_idle_target_is_allowed() {
    // from_admin disable of an idle (not actively executing) target succeeds —
    // the guard only blocks active targets, which require a live in-progress
    // requirement that this lightweight test does not set up.
    let (mut app, services) = build_app().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;

    let resp = app
        .clone()
        .oneshot(json_with_token(
            "POST",
            "/api/conversations",
            json!({ "type": "acp", "name": "Conv Y", "extra": { "workspace": "/project" } }),
            &token,
            &csrf,
        ))
        .await
        .unwrap();
    let conv_id = body_json(resp).await["data"]["conversation_id"]
        .as_str()
        .unwrap()
        .to_owned()
        .to_string();

    // enable then admin-disable (idle) → both OK
    for enabled in [true, false] {
        let resp = app
            .clone()
            .oneshot(json_with_token(
                "POST",
                "/api/requirements/autowork",
                json!({ "kind": "conversation", "target_id": conv_id, "enabled": enabled, "tag": "x", "from_admin": true }),
                &token,
                &csrf,
            ))
            .await
            .unwrap();
        assert_eq!(
            resp.status(),
            StatusCode::OK,
            "enabled={enabled} should be allowed for an idle target"
        );
    }
}

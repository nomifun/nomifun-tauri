//! End-to-end integration tests for the complete authentication flow.
//!
//! These tests exercise the full application stack (security headers, CSRF,
//! auth routes) via `nomifun_app::create_router`, covering test-plan items
//! T12 (security middleware), T13 (token extraction), T14 (initial bootstrap).

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use http_body_util::BodyExt;
use tower::ServiceExt;

use nomifun_app::{AppConfig, AppServices};

// ---------------------------------------------------------------------------
// Test helpers
// ---------------------------------------------------------------------------

async fn build_app() -> (axum::Router, AppServices) {
    let db = nomifun_db::init_database_memory().await.unwrap();
    let services = AppServices::from_config(db, &AppConfig::default()).await.unwrap();
    let router = nomifun_app::create_router(&services).await;
    (router, services)
}

async fn body_json(resp: axum::response::Response) -> serde_json::Value {
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    serde_json::from_slice(&bytes).unwrap()
}

/// Extract the CSRF token from a Set-Cookie header.
fn extract_csrf_token(resp: &axum::response::Response) -> Option<String> {
    resp.headers()
        .get_all(header::SET_COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .find(|s| s.starts_with("nomifun-csrf-token="))
        .map(|s| {
            s.strip_prefix("nomifun-csrf-token=")
                .unwrap()
                .split(';')
                .next()
                .unwrap()
                .to_owned()
        })
}

/// Extract the session token from a Set-Cookie header.
fn extract_session_token(resp: &axum::response::Response) -> Option<String> {
    resp.headers()
        .get_all(header::SET_COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .find(|s| s.starts_with("nomifun-session="))
        .and_then(|s| {
            let value = s.strip_prefix("nomifun-session=")?.split(';').next()?.to_owned();
            if value.is_empty() { None } else { Some(value) }
        })
}

fn get_request(uri: &str) -> Request<Body> {
    Request::builder().method("GET").uri(uri).body(Body::empty()).unwrap()
}

fn get_with_token(uri: &str, token: &str) -> Request<Body> {
    Request::builder()
        .method("GET")
        .uri(uri)
        .header("authorization", format!("Bearer {token}"))
        .body(Body::empty())
        .unwrap()
}

fn get_with_cookie(uri: &str, token: &str) -> Request<Body> {
    Request::builder()
        .method("GET")
        .uri(uri)
        .header("cookie", format!("nomifun-session={token}"))
        .body(Body::empty())
        .unwrap()
}

fn post_json_login(uri: &str, body: &str) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri(uri)
        .header("content-type", "application/json")
        .body(Body::from(body.to_owned()))
        .unwrap()
}

fn post_json_with_csrf(uri: &str, body: &str, token: &str, csrf: &str) -> Request<Body> {
    json_with_csrf("POST", uri, body, token, csrf)
}

fn json_with_csrf(
    method: &str,
    uri: &str,
    body: &str,
    token: &str,
    csrf: &str,
) -> Request<Body> {
    Request::builder()
        .method(method)
        .uri(uri)
        .header("content-type", "application/json")
        .header("authorization", format!("Bearer {token}"))
        .header("x-csrf-token", csrf)
        .header("cookie", format!("nomifun-csrf-token={csrf}"))
        .body(Body::from(body.to_owned()))
        .unwrap()
}

/// Set up a user and login, returning (session_token, csrf_token).
///
/// The installation owner already owns `username = "admin"` with an empty
/// hash; if the test uses that name, overwrite the owner row in place. Other
/// usernames use the normal create_user path.
async fn setup_and_login(
    app: &mut axum::Router,
    services: &AppServices,
    username: &str,
    password: &str,
) -> (String, String) {
    // Create user
    let hash = nomifun_auth::hash_password(password).unwrap();
    if username == "admin" {
        services
            .user_repo
            .set_system_user_credentials(username, &hash)
            .await
            .unwrap();
    } else {
        services.user_repo.create_user(username, &hash).await.unwrap();
    }

    // Get CSRF token from a GET request first
    let resp = app.clone().oneshot(get_request("/api/auth/status")).await.unwrap();
    let csrf = extract_csrf_token(&resp).expect("CSRF cookie should be set");

    // Login (exempt from CSRF)
    let body = format!(r#"{{"username":"{username}","password":"{password}"}}"#);
    let resp = app.clone().oneshot(post_json_login("/login", &body)).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK, "login should succeed");

    let json = body_json(resp).await;
    let token = json["token"].as_str().unwrap().to_owned();

    (token, csrf)
}

#[tokio::test]
async fn remote_owner_login_cannot_write_external_mcp_registration() {
    let (mut app, services) = build_app().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;
    let workspace = tempfile::tempdir().unwrap();
    let body = serde_json::json!({
        "cwd": workspace.path().to_string_lossy(),
        "family": "claude",
    })
    .to_string();
    let response = app
        .oneshot(json_with_csrf(
            "POST",
            "/api/terminals/register-knowledge",
            &body,
            &token,
            &csrf,
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    assert!(
        !workspace.path().join(".mcp.json").exists(),
        "remote authentication must never mutate host MCP config"
    );
}

// ===========================================================================
// T12. Security Middleware
// ===========================================================================

#[tokio::test]
async fn t12_1_security_headers_on_all_responses() {
    let (app, _services) = build_app().await;

    let resp = app.oneshot(get_request("/health")).await.unwrap();

    assert_eq!(resp.headers().get("x-frame-options").unwrap(), "DENY");
    assert_eq!(resp.headers().get("x-content-type-options").unwrap(), "nosniff");
    assert_eq!(resp.headers().get("x-xss-protection").unwrap(), "1; mode=block");
    assert_eq!(
        resp.headers().get("referrer-policy").unwrap(),
        "strict-origin-when-cross-origin"
    );
}

#[tokio::test]
async fn t12_1_security_headers_on_error_responses() {
    let (app, _services) = build_app().await;

    // 404 response should still have security headers
    let resp = app.oneshot(get_request("/nonexistent")).await.unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    assert_eq!(resp.headers().get("x-frame-options").unwrap(), "DENY");
}

#[tokio::test]
async fn t12_2_csrf_blocks_post_without_token() {
    let (mut app, services) = build_app().await;
    let (token, _csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;

    // POST /logout without CSRF token → 403
    let req = Request::builder()
        .method("POST")
        .uri("/logout")
        .header("authorization", format!("Bearer {token}"))
        .body(Body::empty())
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();

    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    let json = body_json(resp).await;
    assert!(
        json["error"].as_str().unwrap_or("").contains("CSRF"),
        "error message should mention CSRF"
    );
}

#[tokio::test]
async fn t12_2_csrf_allows_post_with_valid_token() {
    let (mut app, services) = build_app().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;

    // POST /logout with valid CSRF token → 200
    let req = post_json_with_csrf("/logout", "", &token, &csrf);
    let resp = app.oneshot(req).await.unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
}

#[tokio::test]
async fn t12_2_csrf_exempt_paths() {
    let (app, _services) = build_app().await;

    // POST /login is exempt from CSRF
    let req = post_json_login("/login", r#"{"username":"x","password":"y"}"#);
    let resp = app.clone().oneshot(req).await.unwrap();
    // Should get 401 (auth failure), not 403 (CSRF failure)
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);

    // POST /api/auth/qr-login is exempt from CSRF
    let req = post_json_login("/api/auth/qr-login", r#"{"qr_token":"fake"}"#);
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn t12_3_session_cookie_attributes() {
    let (app, services) = build_app().await;
    let hash = nomifun_auth::hash_password("StrongP@ss1").unwrap();
    // The installation owner is initialized with username='admin'; overwrite
    // its empty password in place instead of creating a duplicate.
    services
        .user_repo
        .set_system_user_credentials("admin", &hash)
        .await
        .unwrap();

    let req = post_json_login("/login", r#"{"username":"admin","password":"StrongP@ss1"}"#);
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let set_cookie = resp
        .headers()
        .get_all(header::SET_COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .find(|s| s.starts_with("nomifun-session="))
        .expect("session cookie should be set");

    assert!(set_cookie.contains("HttpOnly"));
    assert!(set_cookie.contains("SameSite="));
    assert!(set_cookie.contains("Max-Age="));
    // Max-Age should be 30 days
    let expected_max_age = format!("Max-Age={}", 30 * 24 * 60 * 60);
    assert!(set_cookie.contains(&expected_max_age));
}

// ===========================================================================
// T13. Token Extraction Strategy
// ===========================================================================

#[tokio::test]
async fn t13_1_authorization_header_takes_priority() {
    let (mut app, services) = build_app().await;
    let (token, _csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;

    // Both header and cookie present; header should be used
    let req = Request::builder()
        .method("GET")
        .uri("/api/auth/user")
        .header("authorization", format!("Bearer {token}"))
        .header("cookie", "nomifun-session=invalid_token")
        .body(Body::empty())
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
    let json = body_json(resp).await;
    assert_eq!(json["user"]["username"], "admin");
}

#[tokio::test]
async fn t13_2_cookie_fallback() {
    let (mut app, services) = build_app().await;
    let (token, _csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;

    // Only cookie, no Authorization header
    let req = get_with_cookie("/api/auth/user", &token);
    let resp = app.oneshot(req).await.unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
    let json = body_json(resp).await;
    assert_eq!(json["user"]["username"], "admin");
}

#[tokio::test]
async fn t13_3_no_token_fails() {
    let (app, _services) = build_app().await;

    let req = get_request("/api/auth/user");
    let resp = app.oneshot(req).await.unwrap();

    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn installation_control_plane_uses_canonical_owner_identity() {
    let (mut app, services) = build_app().await;
    let installation_owner =
        nomifun_db::installation_owner_id(services.database.pool()).await.unwrap();
    assert_eq!(
        services.authoritative_user_id.as_ref(),
        installation_owner
    );

    let (owner_token, owner_csrf) =
        setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;
    let (secondary_token, secondary_csrf) =
        setup_and_login(&mut app, &services, "secondary", "StrongP@ss2").await;

    let owner = app
        .clone()
        .oneshot(get_with_token("/api/settings", &owner_token))
        .await
        .unwrap();
    assert_eq!(owner.status(), StatusCode::OK);

    let denied = app
        .clone()
        .oneshot(get_with_token("/api/settings", &secondary_token))
        .await
        .unwrap();
    assert_eq!(denied.status(), StatusCode::FORBIDDEN);
    let denied_body = body_json(denied).await;
    assert!(
        denied_body["error"]
            .as_str()
            .unwrap_or_default()
            .contains("owner"),
        "owner denial should be explicit: {denied_body}"
    );

    // A secondary authenticated identity still owns its own Conversation data;
    // the installation-owner gate must not collapse every API into one global
    // account or silently replace the caller id.
    let conversations = app
        .clone()
        .oneshot(get_with_token("/api/conversations", &secondary_token))
        .await
        .unwrap();
    assert_eq!(conversations.status(), StatusCode::OK);

    // Conversation auxiliary operations are user-scoped, not merely
    // authentication-scoped. Historically these handlers discarded
    // CurrentUser and looked up the integer id directly, which exposed an
    // owner's workspace/runtime controls to a secondary user who guessed it.
    let owner_conversation = app
        .clone()
        .oneshot(post_json_with_csrf(
            "/api/conversations",
            r#"{"type":"nomi","name":"owner private conversation","extra":{}}"#,
            &owner_token,
            &owner_csrf,
        ))
        .await
        .unwrap();
    assert_eq!(owner_conversation.status(), StatusCode::CREATED);
    let owner_conversation = body_json(owner_conversation).await;
    let owner_conversation_id = owner_conversation["data"]["conversation_id"]
        .as_str()
        .unwrap()
        .to_owned();

    for suffix in [
        "mode",
        "model",
        "usage",
        "slash-commands",
        "openclaw/runtime",
        "workspace?path=/",
    ] {
        let response = app
            .clone()
            .oneshot(get_with_token(
                &format!("/api/conversations/{owner_conversation_id}/{suffix}"),
                &secondary_token,
            ))
            .await
            .unwrap();
        assert_eq!(
            response.status(),
            StatusCode::NOT_FOUND,
            "secondary principal crossed Conversation ownership through {suffix}"
        );
    }

    for (method, suffix, body) in [
        ("PUT", "mode", r#"{"mode":"code"}"#),
        ("PUT", "model", r#"{"model_id":"forged-model"}"#),
        ("POST", "side-question", r#"{"question":"leak state"}"#),
    ] {
        let response = app
            .clone()
            .oneshot(json_with_csrf(
                method,
                &format!("/api/conversations/{owner_conversation_id}/{suffix}"),
                body,
                &secondary_token,
                &secondary_csrf,
            ))
            .await
            .unwrap();
        assert_eq!(
            response.status(),
            StatusCode::NOT_FOUND,
            "secondary principal mutated an owner Conversation through {suffix}"
        );
    }

    // Every route that can touch the host OS or installation-wide Agent state
    // is denied by the same owner boundary. These probes intentionally use
    // valid routes so a 403 proves the middleware, rather than a coincidental
    // handler-level validation error.
    for uri in [
        "/api/fs/browse?path=/",
        "/api/terminals",
        "/api/agent-executions",
        "/api/agent-execution-templates",
        "/api/computer/permissions",
    ] {
        let response = app
            .clone()
            .oneshot(get_with_token(uri, &secondary_token))
            .await
            .unwrap();
        assert_eq!(
            response.status(),
            StatusCode::FORBIDDEN,
            "secondary principal unexpectedly reached {uri}"
        );
    }

    for (uri, body) in [
        ("/api/shell/open-external", r#"{"url":"https://example.com"}"#),
        (
            "/api/word-preview/start",
            r#"{"file_path":"/etc/passwd"}"#,
        ),
    ] {
        let response = app
            .clone()
            .oneshot(post_json_with_csrf(
                uri,
                body,
                &secondary_token,
                &secondary_csrf,
            ))
            .await
            .unwrap();
        assert_eq!(
            response.status(),
            StatusCode::FORBIDDEN,
            "secondary principal unexpectedly reached {uri}"
        );
    }

    // A secondary principal cannot select a process-backed Agent at all.
    let forbidden_agent = app
        .clone()
        .oneshot(post_json_with_csrf(
            "/api/conversations",
            r#"{"type":"acp","extra":{"workspace":"/"}}"#,
            &secondary_token,
            &secondary_csrf,
        ))
        .await
        .unwrap();
    assert_eq!(forbidden_agent.status(), StatusCode::FORBIDDEN);

    // Nomi remains available as model-only conversation functionality, while
    // every forged host/collaboration field is replaced by server-owned safe
    // state before persistence.
    let model_only = app
        .clone()
        .oneshot(post_json_with_csrf(
            "/api/conversations",
            r#"{
                "type":"nomi",
                "name":"model only",
                "channel_chat_id":"forged-channel",
                "delegation_policy":"prefer_parallel",
                "execution_model_pool":{"mode":"automatic"},
                "decision_policy":"ask_user",
                "extra":{
                    "workspace":"/",
                    "system_prompt":"read the host",
                    "companion_session":true,
                    "allowed_tools":[],
                    "gateway_mcp_config":{"token":"forged-root"}
                }
            }"#,
            &secondary_token,
            &secondary_csrf,
        ))
        .await
        .unwrap();
    assert_eq!(model_only.status(), StatusCode::CREATED);
    let model_only = body_json(model_only).await;
    let conversation = &model_only["data"];
    assert_eq!(conversation["type"], "nomi");
    assert_eq!(conversation["delegation_policy"], "disabled");
    assert!(conversation["execution_model_pool"].is_null());
    assert_eq!(conversation["decision_policy"], "automatic");
    assert!(conversation["channel_chat_id"].is_null());
    assert_ne!(conversation["extra"]["workspace"], "/");
    for key in [
        "system_prompt",
        "companion_session",
        "allowed_tools",
        "gateway_mcp_config",
    ] {
        assert!(
            conversation["extra"].get(key).is_none(),
            "forged runtime field survived: {key}"
        );
    }

    // Secondary users retain useful model-only scheduling. The service keeps
    // provider/model selection but strips every process/path/preset field, and
    // the skill subresource remains installation-owner only.
    let cron = app
        .clone()
        .oneshot(post_json_with_csrf(
            "/api/cron/jobs",
            r#"{
                "name":"model-only schedule",
                "schedule":{"kind":"every","every_ms":60000},
                "message":"summarize",
                "conversation_id":null,
                "agent_type":"nomi",
                "created_by":"user",
                "execution_mode":"new_conversation",
                "agent_config":{
                    "backend":"0190f5fe-7c00-7a00-8000-000000000015",
                    "name":"Nomi",
                    "model_id":"model-safe",
                    "cli_path":"/bin/sh",
                    "workspace":"/",
                    "mode":"yolo",
                    "config_options":{"host":"true"}
                }
            }"#,
            &secondary_token,
            &secondary_csrf,
        ))
        .await
        .unwrap();
    assert_eq!(cron.status(), StatusCode::CREATED);
    let cron = body_json(cron).await;
    let cron_id = cron["data"]["cron_job_id"].as_str().unwrap();
    let cron_config = &cron["data"]["metadata"]["agent_config"];
    assert_eq!(
        cron_config["backend"],
        "0190f5fe-7c00-7a00-8000-000000000015"
    );
    assert_eq!(cron_config["model_id"], "model-safe");
    for key in ["cli_path", "workspace", "mode", "config_options", "preset_id"] {
        assert!(
            cron_config.get(key).is_none() || cron_config[key].is_null(),
            "model-only cron retained host field {key}: {cron_config}"
        );
    }

    let cron_skill = app
        .clone()
        .oneshot(post_json_with_csrf(
            &format!("/api/cron/jobs/{cron_id}/skill"),
            r#"{"content":"---\nname: forbidden\ndescription: host skill\n---\nrun host steps"}"#,
            &secondary_token,
            &secondary_csrf,
        ))
        .await
        .unwrap();
    assert_eq!(cron_skill.status(), StatusCode::FORBIDDEN);

    // Model-only messages cannot smuggle host files or turn-scoped skills into
    // the otherwise valid text conversation.
    let conversation_id = conversation["conversation_id"].as_str().unwrap().to_owned();
    let mut attachment_request = post_json_with_csrf(
        &format!("/api/conversations/{conversation_id}/messages"),
        r#"{"content":"inspect","files":["/etc/passwd"],"inject_skills":["shell"]}"#,
        &secondary_token,
        &secondary_csrf,
    );
    attachment_request.headers_mut().insert(
        "idempotency-key",
        axum::http::HeaderValue::from_static("0190f5fe-7c00-7a00-8000-000000000779"),
    );
    let attachment_attempt = app
        .oneshot(attachment_request)
        .await
        .unwrap();
    assert_eq!(attachment_attempt.status(), StatusCode::FORBIDDEN);
}

// ===========================================================================
// T14. Initial Bootstrap Flow
// ===========================================================================

#[tokio::test]
async fn t14_1_fresh_system_needs_setup() {
    let (app, _services) = build_app().await;

    let resp = app.oneshot(get_request("/api/auth/status")).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let json = body_json(resp).await;
    assert_eq!(json["success"], true);
    assert_eq!(json["needs_setup"], true);
}

#[tokio::test]
async fn t14_2_setup_then_login() {
    let (app, services) = build_app().await;

    // Fresh system: needsSetup=true
    let resp = app.clone().oneshot(get_request("/api/auth/status")).await.unwrap();
    let json = body_json(resp).await;
    assert_eq!(json["needs_setup"], true);

    // Set installation-owner credentials (simulating initial setup)
    let hash = nomifun_auth::hash_password("Admin@Pass1").unwrap();
    services
        .user_repo
        .set_system_user_credentials("admin", &hash)
        .await
        .unwrap();

    // Now needsSetup=false
    let resp = app.clone().oneshot(get_request("/api/auth/status")).await.unwrap();
    let json = body_json(resp).await;
    assert_eq!(json["needs_setup"], false);

    // Login with new credentials
    let req = post_json_login("/login", r#"{"username":"admin","password":"Admin@Pass1"}"#);
    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let json = body_json(resp).await;
    assert_eq!(json["success"], true);
    assert_eq!(json["user"]["username"], "admin");

    // Authenticated status check
    let token = json["token"].as_str().unwrap();
    let req = get_with_token("/api/auth/status", token);
    let resp = app.oneshot(req).await.unwrap();
    let json = body_json(resp).await;
    assert_eq!(json["is_authenticated"], true);
    assert_eq!(json["needs_setup"], false);
}

// ===========================================================================
// Full E2E Flow: setup → login → get user → change password → logout
// ===========================================================================

#[tokio::test]
async fn full_auth_flow_e2e() {
    let (app, services) = build_app().await;

    // 1. Check initial status
    let resp = app.clone().oneshot(get_request("/api/auth/status")).await.unwrap();
    let csrf = extract_csrf_token(&resp).expect("CSRF cookie on first request");
    let json = body_json(resp).await;
    assert_eq!(json["needs_setup"], true);

    // 2. Setup user
    let hash = nomifun_auth::hash_password("Initial@Pass1").unwrap();
    services
        .user_repo
        .set_system_user_credentials("admin", &hash)
        .await
        .unwrap();

    // 3. Login
    let req = post_json_login("/login", r#"{"username":"admin","password":"Initial@Pass1"}"#);
    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let session_token = extract_session_token(&resp).expect("session cookie set");
    let json = body_json(resp).await;
    let token = json["token"].as_str().unwrap().to_owned();

    // Verify session token matches response body token
    assert_eq!(session_token, token);

    // 4. Get current user
    let req = get_with_token("/api/auth/user", &token);
    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let json = body_json(resp).await;
    assert_eq!(json["user"]["username"], "admin");

    // 5. Change password (needs CSRF)
    let req = post_json_with_csrf(
        "/api/auth/change-password",
        r#"{"current_password":"Initial@Pass1","new_password":"Updated@Pass2"}"#,
        &token,
        &csrf,
    );
    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    // 6. Old token invalidated after password change
    let req = get_with_token("/api/auth/user", &token);
    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);

    // 7. Login with new password
    let req = post_json_login("/login", r#"{"username":"admin","password":"Updated@Pass2"}"#);
    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let json = body_json(resp).await;
    let new_token = json["token"].as_str().unwrap().to_owned();

    // 8. Logout (needs CSRF)
    let req = post_json_with_csrf("/logout", "", &new_token, &csrf);
    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    // 9. Token invalid after logout
    let req = get_with_token("/api/auth/user", &new_token);
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
}

// ===========================================================================
// CSRF cookie is set on first response
// ===========================================================================

#[tokio::test]
async fn csrf_cookie_set_on_first_get() {
    let (app, _services) = build_app().await;

    let resp = app.oneshot(get_request("/health")).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let csrf = extract_csrf_token(&resp);
    assert!(csrf.is_some(), "CSRF cookie should be set on first request");
    assert_eq!(csrf.unwrap().len(), 64, "CSRF token should be 64 hex chars");
}

//! E2E tests for cron job HTTP endpoints.
//!
//! Covers test-plan items: CJ-1..CJ-12, SK-1..SK-6, SC-3..SC-8, AU-1..AU-2,
//! RN-1..RN-2.
//! Items requiring real AI execution (RN-1, EV-*, SR-*, OC-*, CD-*) are tested
//! at the service integration level in `nomifun-cron/tests/service_integration.rs`.

mod common;

use axum::http::{HeaderValue, StatusCode};
use serde_json::json;
use tower::ServiceExt;

use nomifun_db::{ICronRepository, SqliteCronRepository};

use common::{body_json, build_app, delete_with_token, get_request, get_with_token, json_with_token, setup_and_login};

// Deterministic canonical UUIDv7 values keep conversation fixtures readable.
// Each test gets an isolated database, so reusing these values across tests
// cannot collide.
const TEST_CONV_1: &str = "0190f5fe-7c00-7a00-8abc-012345678901";
const TEST_CONV_2: &str = "0190f5fe-7c00-7a00-8abc-012345678902";
const TEST_CONV_3: &str = "0190f5fe-7c00-7a00-8abc-012345678903";
const MISSING_CRON_JOB_ID: &str = "0190f5fe-7c00-7a00-8abc-012345678999";
const SECONDARY_PROVIDER_ID: &str = "0190f5fe-7c00-7a00-8abc-012345679993";
const FORGED_CUSTOM_AGENT_ID: &str = "0190f5fe-7c00-7a00-8abc-012345679994";

// ── Helpers ──────────────────────────────────────────────────────────

fn create_job_body(name: &str) -> serde_json::Value {
    json!({
        "name": name,
        "schedule": { "kind": "every", "every_ms": 60000, "description": "every minute" },
        "message": "test message",
        "conversation_id": TEST_CONV_1,
        "conversation_title": "Test Conv",
        "agent_type": "acp",
        "created_by": "user"
    })
}

fn create_at_job_body(name: &str, at_ms: i64) -> serde_json::Value {
    json!({
        "name": name,
        "schedule": { "kind": "at", "at_ms": at_ms, "description": "once" },
        "message": "at message",
        "conversation_id": TEST_CONV_1,
        "agent_type": "acp",
        "created_by": "user"
    })
}

fn create_cron_job_body(name: &str, expr: &str) -> serde_json::Value {
    json!({
        "name": name,
        "schedule": { "kind": "cron", "expr": expr },
        "message": "cron message",
        "conversation_id": TEST_CONV_1,
        "agent_type": "acp",
        "created_by": "user"
    })
}

async fn create_job(app: &mut axum::Router, token: &str, csrf: &str, body: serde_json::Value) -> serde_json::Value {
    let req = json_with_token("POST", "/api/cron/jobs", body, token, csrf);
    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);
    let json = body_json(resp).await;
    assert_eq!(json["success"], true);
    json["data"].clone()
}

fn run_now_request_with_key(
    cron_job_id: &str,
    token: &str,
    csrf: &str,
    idempotency_key: &str,
) -> axum::http::Request<axum::body::Body> {
    let mut request = json_with_token(
        "POST",
        &format!("/api/cron/jobs/{cron_job_id}/run"),
        json!({}),
        token,
        csrf,
    );
    request.headers_mut().insert(
        "idempotency-key",
        HeaderValue::try_from(idempotency_key).expect("test idempotency key"),
    );
    request
}

fn run_now_request(
    cron_job_id: &str,
    token: &str,
    csrf: &str,
) -> axum::http::Request<axum::body::Body> {
    run_now_request_with_key(
        cron_job_id,
        token,
        csrf,
        &nomifun_common::generate_id(),
    )
}

/// Seed a minimal conversation row so a cron job carrying this logical
/// `conversation_id` can be resolved by the application. The owner is
/// resolved by application bootstrap from the database's
/// `installation_identity` singleton.
async fn seed_conversation(services: &nomifun_app::AppServices, id: &str) {
    sqlx::query(
        "INSERT INTO conversations (conversation_id, user_id, name, type, created_at, updated_at) \
         VALUES (?, ?, 'Seeded Conv', 'acp', 0, 0)",
    )
    .bind(id)
    .bind(services.authoritative_user_id.as_ref())
    .execute(services.database.pool())
    .await
    .unwrap();
}

// ── AU-1/AU-2: Unauthenticated requests ─────────────────────────────

#[tokio::test]
async fn au1_unauthenticated_list_returns_403() {
    let (app, _services) = build_app().await;
    let req = get_request("/api/cron/jobs");
    let resp = app.oneshot(req).await.unwrap();
    assert!(
        resp.status() == StatusCode::UNAUTHORIZED || resp.status() == StatusCode::FORBIDDEN,
        "expected 401 or 403, got {}",
        resp.status()
    );
}

#[tokio::test]
async fn au2_unauthenticated_all_endpoints() {
    let (app, _services) = build_app().await;

    let endpoints = vec![
        ("GET", "/api/cron/jobs"),
        ("GET", concat!("/api/cron/jobs/", "0190f5fe-7c00-7a00-8abc-012345678998")),
        (
            "GET",
            concat!(
                "/api/cron/jobs/",
                "0190f5fe-7c00-7a00-8abc-012345678998",
                "/skill"
            ),
        ),
        (
            "DELETE",
            concat!(
                "/api/cron/jobs/",
                "0190f5fe-7c00-7a00-8abc-012345678998",
                "/skill"
            ),
        ),
    ];

    for (method, uri) in endpoints {
        let req = axum::http::Request::builder()
            .method(method)
            .uri(uri)
            .body(axum::body::Body::empty())
            .unwrap();
        let resp = app.clone().oneshot(req).await.unwrap();
        assert!(
            resp.status() == StatusCode::UNAUTHORIZED || resp.status() == StatusCode::FORBIDDEN,
            "{method} {uri} expected 401/403, got {}",
            resp.status()
        );
    }
}

#[tokio::test]
async fn au3_authenticated_users_cannot_observe_or_mutate_each_others_cron_jobs() {
    let (mut app, services) = build_app().await;
    let (owner_token, owner_csrf) =
        setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;

    let create_conversation = json_with_token(
        "POST",
        "/api/conversations",
        json!({
            "type": "acp",
            "name": "Owner Cron Conversation",
            "extra": { "workspace": "/project" }
        }),
        &owner_token,
        &owner_csrf,
    );
    let response = app.clone().oneshot(create_conversation).await.unwrap();
    assert_eq!(response.status(), StatusCode::CREATED);
    let conversation_id = body_json(response).await["data"]["conversation_id"]
        .as_str()
        .unwrap()
        .to_owned();

    let mut body = create_job_body("Private Owner Job");
    body["conversation_id"] = json!(conversation_id);
    let created = create_job(&mut app, &owner_token, &owner_csrf, body).await;
    let job_id = created["cron_job_id"].as_str().unwrap().to_owned();

    let (foreign_token, foreign_csrf) =
        setup_and_login(&mut app, &services, "secondary", "An0therStrongP@ss!").await;

    let response = app
        .clone()
        .oneshot(get_with_token("/api/cron/jobs", &foreign_token))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert!(body_json(response).await["data"].as_array().unwrap().is_empty());

    let foreign_requests = [
        get_with_token(&format!("/api/cron/jobs/{job_id}"), &foreign_token),
        get_with_token(&format!("/api/cron/jobs/{job_id}/runs"), &foreign_token),
        get_with_token(
            &format!("/api/cron/jobs/{job_id}/conversations"),
            &foreign_token,
        ),
        get_with_token(&format!("/api/cron/jobs/{job_id}/skill"), &foreign_token),
        json_with_token(
            "PUT",
            &format!("/api/cron/jobs/{job_id}"),
            json!({ "name": "Forged" }),
            &foreign_token,
            &foreign_csrf,
        ),
        run_now_request(&job_id, &foreign_token, &foreign_csrf),
        json_with_token(
            "POST",
            &format!("/api/cron/jobs/{job_id}/skill"),
            json!({ "content": "---\nname: forged\n---\nForeign content" }),
            &foreign_token,
            &foreign_csrf,
        ),
        delete_with_token(
            &format!("/api/cron/jobs/{job_id}/skill"),
            &foreign_token,
            &foreign_csrf,
        ),
        delete_with_token(
            &format!("/api/cron/jobs/{job_id}"),
            &foreign_token,
            &foreign_csrf,
        ),
    ];
    for request in foreign_requests {
        let response = app.clone().oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    // Cron itself is user-scoped rather than installation-owner-only. A
    // secondary principal may manage its own Nomi model-only schedule; the
    // service strips every host-capability field before persistence.
    sqlx::query(
        "INSERT INTO providers (\
            provider_id, platform, name, base_url, api_key_encrypted, models, enabled, \
            capabilities, created_at, updated_at\
         ) VALUES (?, 'openai', 'secondary-safe', \
                   'https://example.invalid', 'encrypted', \
                   '[\"model-secondary\"]', 1, '[]', 1, 1)",
    )
    .bind(SECONDARY_PROVIDER_ID)
    .execute(services.database.pool())
    .await
    .unwrap();
    let response = app
        .clone()
        .oneshot(json_with_token(
            "POST",
            "/api/cron/jobs",
            json!({
                "name": "Secondary Model-only Job",
                "schedule": {
                    "kind": "every",
                    "every_ms": 600_000,
                    "description": "every ten minutes"
                },
                "message": "model-only scheduled work",
                "agent_type": "nomi",
                "created_by": "user",
                "execution_mode": "new_conversation",
                "agent_config": {
                    "backend": SECONDARY_PROVIDER_ID,
                    "name": "Nomi",
                    "model_id": "model-secondary",
                    "cli_path": "/bin/sh",
                    "custom_agent_id": FORGED_CUSTOM_AGENT_ID,
                    "mode": "yolo",
                    "config_options": { "host": "true" },
                    "workspace": "/unsafe",
                    "clear_context_each_run": true
                }
            }),
            &foreign_token,
            &foreign_csrf,
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::CREATED);
    let secondary_job = body_json(response).await["data"].clone();
    let secondary_job_id = secondary_job["cron_job_id"].as_str().unwrap().to_owned();
    let config = &secondary_job["metadata"]["agent_config"];
    assert_eq!(secondary_job["metadata"]["agent_type"], "nomi");
    assert_eq!(config["backend"], SECONDARY_PROVIDER_ID);
    assert_eq!(config["model_id"], "model-secondary");
    for removed in [
        "cli_path",
        "custom_agent_id",
        "mode",
        "config_options",
        "workspace",
    ] {
        assert!(
            config.get(removed).is_none(),
            "model-only cron leaked host field {removed}: {config}"
        );
    }

    let response = app
        .clone()
        .oneshot(get_with_token("/api/cron/jobs", &foreign_token))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let jobs = body_json(response).await["data"].as_array().unwrap().clone();
    assert_eq!(jobs.len(), 1);
    assert_eq!(jobs[0]["cron_job_id"], json!(secondary_job_id));

    let response = app
        .clone()
        .oneshot(json_with_token(
            "PUT",
            &format!("/api/cron/jobs/{secondary_job_id}"),
            json!({ "name": "Secondary Model-only Job Updated" }),
            &foreign_token,
            &foreign_csrf,
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        body_json(response).await["data"]["name"],
        "Secondary Model-only Job Updated"
    );

    // Skill files execute on the host and remain installation-owner-only even
    // for a secondary user's own otherwise-valid model-only Cron aggregate.
    for request in [
        get_with_token(
            &format!("/api/cron/jobs/{secondary_job_id}/skill"),
            &foreign_token,
        ),
        json_with_token(
            "POST",
            &format!("/api/cron/jobs/{secondary_job_id}/skill"),
            json!({ "content": "---\nname: forbidden\n---\nHost work" }),
            &foreign_token,
            &foreign_csrf,
        ),
        delete_with_token(
            &format!("/api/cron/jobs/{secondary_job_id}/skill"),
            &foreign_token,
            &foreign_csrf,
        ),
    ] {
        let response = app.clone().oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
    }

    let response = app
        .clone()
        .oneshot(delete_with_token(
            &format!("/api/cron/jobs/{secondary_job_id}"),
            &foreign_token,
            &foreign_csrf,
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    let response = app
        .oneshot(get_with_token(
            &format!("/api/cron/jobs/{job_id}"),
            &owner_token,
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let retained = body_json(response).await;
    assert_eq!(retained["data"]["name"], "Private Owner Job");
    assert_eq!(retained["data"]["state"]["run_count"], 0);
}

// ── CJ-1: Create cron job ───────────────────────────────────────────

#[tokio::test]
async fn cj1_create_cron_job() {
    let (mut app, services) = build_app().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;

    seed_conversation(&services, TEST_CONV_1).await;
    let data = create_job(&mut app, &token, &csrf, create_job_body("Daily Report")).await;

    assert!(nomifun_common::CronJobId::parse(
        data["cron_job_id"].as_str().expect("cron_job_id UUIDv7 string")
    )
    .is_ok());
    assert!(data.get("id").is_none());
    assert_eq!(data["name"], "Daily Report");
    assert_eq!(data["enabled"], true);
    assert!(data["state"]["next_run_at_ms"].as_i64().is_some());
    assert_eq!(data["state"]["run_count"], 0);
    assert_eq!(data["message"], "test message");
    assert_eq!(data["execution_mode"], "existing");
    assert_eq!(data["metadata"]["conversation_id"], TEST_CONV_1);
    assert_eq!(data["metadata"]["agent_type"], "acp");
    assert_eq!(data["metadata"]["created_by"], "user");
}

// ── CJ-2: Create three schedule types ────────────────────────────────

#[tokio::test]
async fn cj2_create_three_schedule_types() {
    let (mut app, services) = build_app().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;

    let now = nomifun_common::now_ms();

    seed_conversation(&services, TEST_CONV_1).await;
    let at = create_job(&mut app, &token, &csrf, create_at_job_body("At Job", now + 3_600_000)).await;
    assert_eq!(at["schedule"]["kind"], "at");
    assert!(at["state"]["next_run_at_ms"].as_i64().unwrap() > now);

    let every = create_job(&mut app, &token, &csrf, create_job_body("Every Job")).await;
    assert_eq!(every["schedule"]["kind"], "every");
    let next = every["state"]["next_run_at_ms"].as_i64().unwrap();
    assert!((next - now - 60000).abs() < 3000);

    let cron = create_job(
        &mut app,
        &token,
        &csrf,
        create_cron_job_body("Cron Job", "0 */5 * * * *"),
    )
    .await;
    assert_eq!(cron["schedule"]["kind"], "cron");
    assert!(cron["state"]["next_run_at_ms"].as_i64().unwrap() > now);
}

// ── CJ-3: Create parameter validation ────────────────────────────────

#[tokio::test]
async fn cj3_create_missing_required_fields() {
    let (mut app, services) = build_app().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;

    let invalid_bodies = vec![
        (
            "name",
            json!({"schedule": {"kind": "every", "every_ms": 60000}, "conversation_id": TEST_CONV_1, "agent_type": "acp", "created_by": "user"}),
        ),
        (
            "schedule",
            json!({"name": "X", "conversation_id": TEST_CONV_1, "agent_type": "acp", "created_by": "user"}),
        ),
        (
            "agent_type",
            json!({"name": "X", "schedule": {"kind": "every", "every_ms": 60000}, "conversation_id": TEST_CONV_1, "created_by": "user"}),
        ),
        (
            "created_by",
            json!({"name": "X", "schedule": {"kind": "every", "every_ms": 60000}, "conversation_id": TEST_CONV_1, "agent_type": "acp"}),
        ),
    ];

    for (missing_field, body) in invalid_bodies {
        let req = json_with_token("POST", "/api/cron/jobs", body, &token, &csrf);
        let resp = app.clone().oneshot(req).await.unwrap();
        assert_eq!(
            resp.status(),
            StatusCode::BAD_REQUEST,
            "missing {missing_field} should return 400"
        );
    }
}

#[tokio::test]
async fn cj3b_create_rejects_workspace_with_edge_whitespace_segment() {
    let (mut app, services) = build_app().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;

    let body = json!({
        "name": "Whitespace Workspace",
        "schedule": { "kind": "every", "every_ms": 60000, "description": "every minute" },
        "message": "test message",
        "agent_type": "acp",
        "created_by": "user",
        "execution_mode": "new_conversation",
        "agent_config": {
            "backend": "acp",
            "name": "Cron Agent",
            "workspace": "/Users/zhoukai/Documents/Archive "
        }
    });

    let req = json_with_token("POST", "/api/cron/jobs", body, &token, &csrf);
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);

    let json = body_json(resp).await;
    assert_eq!(json["code"], "WORKSPACE_PATH_EDGE_WHITESPACE_UNSUPPORTED");
    assert!(
        json["error"]
            .as_str()
            .unwrap()
            .contains("begins or ends with whitespace")
    );
}

// ── CJ-4: Get single job ────────────────────────────────────────────

#[tokio::test]
async fn cj4_get_single_job() {
    let (mut app, services) = build_app().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;

    seed_conversation(&services, TEST_CONV_1).await;
    let created = create_job(&mut app, &token, &csrf, create_job_body("Get Test")).await;
    let job_id = created["cron_job_id"].as_str().unwrap().to_owned();

    let req = get_with_token(&format!("/api/cron/jobs/{job_id}"), &token);
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let json = body_json(resp).await;
    assert_eq!(json["data"]["cron_job_id"], json!(job_id));
    assert!(json["data"].get("id").is_none());
    assert_eq!(json["data"]["name"], "Get Test");
}

// ── CJ-5: Get nonexistent job ────────────────────────────────────────

#[tokio::test]
async fn cj5_get_nonexistent() {
    let (mut app, services) = build_app().await;
    let (token, _csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;

    let req = get_with_token(&format!("/api/cron/jobs/{MISSING_CRON_JOB_ID}"), &token);
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn cj5b_run_now_legacy_workspace_uses_runtime_edge_whitespace_code() {
    let (mut app, services) = build_app().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;
    let cron_repo = SqliteCronRepository::new(services.database.pool().clone());
    let now = nomifun_common::now_ms();

    let job_id = nomifun_common::CronJobId::new().into_string();
    cron_repo
        .insert(&nomifun_db::models::CronJobRow {
            id: 0,
            cron_job_id: job_id.clone(),
            user_id: services.authoritative_user_id.to_string(),
            name: "Legacy Workspace".into(),
            enabled: true,
            schedule_revision: 1,
            schedule_kind: "every".into(),
            schedule_value: "60000".into(),
            schedule_tz: None,
            schedule_description: Some("every minute".into()),
            payload_message: "test message".into(),
            execution_mode: "new_conversation".into(),
            agent_config: Some(
                json!({
                    "backend": "acp",
                    "name": "Cron Agent",
                    "workspace": "/Users/zhoukai/Documents/Archive "
                })
                .to_string(),
            ),
            preset_id: None,
            preset_revision: None,
            preset_snapshot: None,
            conversation_id: None,
            conversation_title: None,
            agent_type: "acp".into(),
            created_by: "user".into(),
            skill_content: None,
            description: None,
            created_at: now,
            updated_at: now,
            next_run_at: Some(now + 60_000),
            last_run_at: None,
            last_status: None,
            last_error: None,
            run_count: 0,
            retry_count: 0,
            max_retries: 3,
        })
        .await
        .unwrap();

    let req = run_now_request(&job_id, &token, &csrf);
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);

    let json = body_json(resp).await;
    assert_eq!(json["code"], "WORKSPACE_PATH_EDGE_WHITESPACE_RUNTIME_UNSUPPORTED");
    assert!(
        json["error"]
            .as_str()
            .unwrap()
            .contains("begins or ends with whitespace")
    );
    assert_eq!(json["details"]["operation"], "runtime");
}

// ── CJ-6: List all jobs ─────────────────────────────────────────────

#[tokio::test]
async fn cj6_list_all_jobs() {
    let (mut app, services) = build_app().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;

    seed_conversation(&services, TEST_CONV_1).await;
    for i in 0..3 {
        create_job(&mut app, &token, &csrf, create_job_body(&format!("Job {i}"))).await;
    }

    let req = get_with_token("/api/cron/jobs", &token);
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let json = body_json(resp).await;
    let items = json["data"].as_array().unwrap();
    assert!(items.len() >= 3);
}

// ── CJ-7: List by conversation ID ───────────────────────────────────

#[tokio::test]
async fn cj7_list_by_conversation() {
    let (mut app, services) = build_app().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;

    seed_conversation(&services, TEST_CONV_2).await;
    seed_conversation(&services, TEST_CONV_3).await;
    let mut body_a = create_job_body("Job A");
    body_a["conversation_id"] = json!(TEST_CONV_2);
    create_job(&mut app, &token, &csrf, body_a).await;

    let mut body_b = create_job_body("Job B");
    body_b["conversation_id"] = json!(TEST_CONV_2);
    create_job(&mut app, &token, &csrf, body_b).await;

    let mut body_c = create_job_body("Job C");
    body_c["conversation_id"] = json!(TEST_CONV_3);
    create_job(&mut app, &token, &csrf, body_c).await;

    let req = get_with_token(&format!("/api/cron/jobs?conversation_id={TEST_CONV_2}"), &token);
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let json = body_json(resp).await;
    let items = json["data"].as_array().unwrap();
    assert_eq!(items.len(), 2);
}

// ── CJ-8: Update job ────────────────────────────────────────────────

#[tokio::test]
async fn cj8_update_job() {
    let (mut app, services) = build_app().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;

    seed_conversation(&services, TEST_CONV_1).await;
    let created = create_job(&mut app, &token, &csrf, create_job_body("Original")).await;
    let job_id = created["cron_job_id"].as_str().unwrap().to_owned();

    let update_body = json!({"name": "Updated Name", "enabled": false});
    let req = json_with_token("PUT", &format!("/api/cron/jobs/{job_id}"), update_body, &token, &csrf);
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let json = body_json(resp).await;
    assert_eq!(json["data"]["name"], "Updated Name");
    assert_eq!(json["data"]["enabled"], false);
    assert!(
        json["data"]["metadata"]["updated_at"].as_str().unwrap().to_owned() >= created["metadata"]["created_at"].as_str().unwrap().to_owned()
    );
}

// ── CJ-9: Update schedule type ──────────────────────────────────────

#[tokio::test]
async fn cj9_update_schedule_type() {
    let (mut app, services) = build_app().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;

    seed_conversation(&services, TEST_CONV_1).await;
    let created = create_job(&mut app, &token, &csrf, create_job_body("Schedule Change")).await;
    let job_id = created["cron_job_id"].as_str().unwrap().to_owned();

    let update_body = json!({"schedule": {"kind": "cron", "expr": "0 */5 * * * *"}});
    let req = json_with_token("PUT", &format!("/api/cron/jobs/{job_id}"), update_body, &token, &csrf);
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let json = body_json(resp).await;
    assert_eq!(json["data"]["schedule"]["kind"], "cron");
    assert!(json["data"]["state"]["next_run_at_ms"].as_i64().is_some());
}

#[tokio::test]
async fn cj9b_update_schedule_preserves_existing_timezone_when_omitted() {
    let (mut app, services) = build_app().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;

    seed_conversation(&services, TEST_CONV_1).await;
    let created = create_job(
        &mut app,
        &token,
        &csrf,
        json!({
            "name": "Schedule Change With Timezone",
            "schedule": { "kind": "cron", "expr": "0 0 9 * * *", "tz": "Asia/Shanghai" },
            "message": "cron message",
            "conversation_id": TEST_CONV_1,
            "agent_type": "acp",
            "created_by": "user"
        }),
    )
    .await;
    let job_id = created["cron_job_id"].as_str().unwrap().to_owned();

    let update_body = json!({"schedule": {"kind": "cron", "expr": "0 30 9 * * *"}});
    let req = json_with_token("PUT", &format!("/api/cron/jobs/{job_id}"), update_body, &token, &csrf);
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let json = body_json(resp).await;
    assert_eq!(json["data"]["schedule"]["kind"], "cron");
    assert_eq!(json["data"]["schedule"]["expr"], "0 30 9 * * *");
    assert_eq!(json["data"]["schedule"]["tz"], "Asia/Shanghai");
}

// ── CJ-10: Update nonexistent ────────────────────────────────────────

#[tokio::test]
async fn cj10_update_nonexistent() {
    let (mut app, services) = build_app().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;

    let update_body = json!({"name": "X"});
    let req = json_with_token(
        "PUT",
        &format!("/api/cron/jobs/{MISSING_CRON_JOB_ID}"),
        update_body,
        &token,
        &csrf,
    );
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

// ── CJ-11: Delete job ───────────────────────────────────────────────

#[tokio::test]
async fn cj11_delete_job() {
    let (mut app, services) = build_app().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;

    seed_conversation(&services, TEST_CONV_1).await;
    let created = create_job(&mut app, &token, &csrf, create_job_body("To Delete")).await;
    let job_id = created["cron_job_id"].as_str().unwrap().to_owned();

    let req = delete_with_token(&format!("/api/cron/jobs/{job_id}"), &token, &csrf);
    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let req = get_with_token(&format!("/api/cron/jobs/{job_id}"), &token);
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

// ── CJ-12: Delete nonexistent ────────────────────────────────────────

#[tokio::test]
async fn cj12_delete_nonexistent() {
    let (mut app, services) = build_app().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;

    let req = delete_with_token(
        &format!("/api/cron/jobs/{MISSING_CRON_JOB_ID}"),
        &token,
        &csrf,
    );
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

// ── RN-2: Run now nonexistent ────────────────────────────────────────

#[tokio::test]
async fn rn0_run_now_requires_exactly_one_idempotency_key() {
    let (mut app, services) = build_app().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;
    let uri = format!("/api/cron/jobs/{MISSING_CRON_JOB_ID}/run");

    let missing = json_with_token("POST", &uri, json!({}), &token, &csrf);
    let response = app.clone().oneshot(missing).await.unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);

    let mut duplicate = json_with_token("POST", &uri, json!({}), &token, &csrf);
    duplicate
        .headers_mut()
        .append("idempotency-key", HeaderValue::from_static("first"));
    duplicate
        .headers_mut()
        .append("idempotency-key", HeaderValue::from_static("second"));
    let response = app.oneshot(duplicate).await.unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn rn1_run_now_returns_conversation_id_for_new_conversation_job() {
    let (mut app, services) = build_app().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;
    let workspace = std::env::current_dir()
        .expect("Cron E2E current directory")
        .to_string_lossy()
        .into_owned();

    let create_conv_req = json_with_token(
        "POST",
        "/api/conversations",
        json!({
            "type": "acp",
            "name": "Run Now Source",
            "extra": {
                "agent_id": "0190f5fe-7c00-7a00-8000-000000000103",
                "agent_source": "builtin",
                "backend": "gemini",
                "workspace": workspace
            }
        }),
        &token,
        &csrf,
    );
    let create_conv_resp = app.clone().oneshot(create_conv_req).await.unwrap();
    assert_eq!(create_conv_resp.status(), StatusCode::CREATED);
    let created_conv = body_json(create_conv_resp).await;
    let conversation_id = created_conv["data"]["conversation_id"]
        .as_str()
        .unwrap()
        .to_owned();

    let mut body = create_job_body("Run Now Job");
    body["conversation_id"] = json!(conversation_id);
    let created = create_job(&mut app, &token, &csrf, body).await;
    let job_id = created["cron_job_id"].as_str().unwrap().to_owned();

    let idempotency_key = nomifun_common::generate_id();
    let req = run_now_request_with_key(&job_id, &token, &csrf, &idempotency_key);
    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let body = body_json(resp).await;
    assert_eq!(body["data"]["conversation_id"], json!(conversation_id));

    let replay = run_now_request_with_key(&job_id, &token, &csrf, &idempotency_key);
    let replay = app.oneshot(replay).await.unwrap();
    assert_eq!(replay.status(), StatusCode::OK);
    let replay = body_json(replay).await;
    assert_eq!(replay["data"]["conversation_id"], json!(conversation_id));
}

#[tokio::test]
async fn rn2_run_now_nonexistent() {
    let (mut app, services) = build_app().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;

    let req = run_now_request(MISSING_CRON_JOB_ID, &token, &csrf);
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

// ── SK-1: Save skill ────────────────────────────────────────────────

#[tokio::test]
async fn sk1_save_skill() {
    let (mut app, services) = build_app().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;

    seed_conversation(&services, TEST_CONV_1).await;
    let created = create_job(&mut app, &token, &csrf, create_job_body("Skill Job")).await;
    let job_id = created["cron_job_id"].as_str().unwrap().to_owned();

    let skill_body = json!({"content": "---\nname: test\ndescription: test skill\n---\nDo something"});
    let req = json_with_token(
        "POST",
        &format!("/api/cron/jobs/{job_id}/skill"),
        skill_body,
        &token,
        &csrf,
    );
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
}

// ── SK-2: Has skill (true) ──────────────────────────────────────────

#[tokio::test]
async fn sk2_has_skill_true() {
    let (mut app, services) = build_app().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;

    seed_conversation(&services, TEST_CONV_1).await;
    let created = create_job(&mut app, &token, &csrf, create_job_body("Skill Check")).await;
    let job_id = created["cron_job_id"].as_str().unwrap().to_owned();

    let skill_body = json!({"content": "---\nname: x\n---\nContent"});
    let req = json_with_token(
        "POST",
        &format!("/api/cron/jobs/{job_id}/skill"),
        skill_body,
        &token,
        &csrf,
    );
    app.clone().oneshot(req).await.unwrap();

    let req = get_with_token(&format!("/api/cron/jobs/{job_id}/skill"), &token);
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let json = body_json(resp).await;
    assert_eq!(json["data"]["has_skill"], true);
}

// ── SK-3: Has skill (false) ─────────────────────────────────────────

#[tokio::test]
async fn sk3_has_skill_false() {
    let (mut app, services) = build_app().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;

    seed_conversation(&services, TEST_CONV_1).await;
    let created = create_job(&mut app, &token, &csrf, create_job_body("No Skill")).await;
    let job_id = created["cron_job_id"].as_str().unwrap().to_owned();

    let req = get_with_token(&format!("/api/cron/jobs/{job_id}/skill"), &token);
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let json = body_json(resp).await;
    assert_eq!(json["data"]["has_skill"], false);
}

// ── SK-4: Save empty skill ──────────────────────────────────────────

#[tokio::test]
async fn sk4_save_empty_skill() {
    let (mut app, services) = build_app().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;

    seed_conversation(&services, TEST_CONV_1).await;
    let created = create_job(&mut app, &token, &csrf, create_job_body("Empty Skill")).await;
    let job_id = created["cron_job_id"].as_str().unwrap().to_owned();

    let skill_body = json!({"content": ""});
    let req = json_with_token(
        "POST",
        &format!("/api/cron/jobs/{job_id}/skill"),
        skill_body,
        &token,
        &csrf,
    );
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

// ── SK-5: Save placeholder skill ────────────────────────────────────

#[tokio::test]
async fn sk5_save_placeholder_skill() {
    let (mut app, services) = build_app().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;

    seed_conversation(&services, TEST_CONV_1).await;
    let created = create_job(&mut app, &token, &csrf, create_job_body("Placeholder Skill")).await;
    let job_id = created["cron_job_id"].as_str().unwrap().to_owned();

    let skill_body = json!({"content": "TODO: fill in later"});
    let req = json_with_token(
        "POST",
        &format!("/api/cron/jobs/{job_id}/skill"),
        skill_body,
        &token,
        &csrf,
    );
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

// ── SK-6: Save skill for nonexistent job ─────────────────────────────

#[tokio::test]
async fn sk6_save_skill_nonexistent() {
    let (mut app, services) = build_app().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;

    let skill_body = json!({"content": "---\nname: x\n---\nOk"});
    let req = json_with_token(
        "POST",
        &format!("/api/cron/jobs/{MISSING_CRON_JOB_ID}/skill"),
        skill_body,
        &token,
        &csrf,
    );
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

// ── SK-7: Delete existing skill ──────────────────────────────────────

#[tokio::test]
async fn sk7_delete_skill() {
    let (mut app, services) = build_app().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;

    seed_conversation(&services, TEST_CONV_1).await;
    let created = create_job(&mut app, &token, &csrf, create_job_body("Delete Skill Job")).await;
    let job_id = created["cron_job_id"].as_str().unwrap().to_owned();

    let save_req = json_with_token(
        "POST",
        &format!("/api/cron/jobs/{job_id}/skill"),
        json!({"content": "---\nname: delete-me\n---\nContent"}),
        &token,
        &csrf,
    );
    let save_resp = app.clone().oneshot(save_req).await.unwrap();
    assert_eq!(save_resp.status(), StatusCode::OK);

    let delete_req = delete_with_token(&format!("/api/cron/jobs/{job_id}/skill"), &token, &csrf);
    let delete_resp = app.clone().oneshot(delete_req).await.unwrap();
    assert_eq!(delete_resp.status(), StatusCode::OK);

    let has_req = get_with_token(&format!("/api/cron/jobs/{job_id}/skill"), &token);
    let has_resp = app.oneshot(has_req).await.unwrap();
    assert_eq!(has_resp.status(), StatusCode::OK);

    let json = body_json(has_resp).await;
    assert_eq!(json["data"]["has_skill"], false);
}

// ── SK-8: Delete skill for nonexistent job ───────────────────────────

#[tokio::test]
async fn sk8_delete_skill_nonexistent() {
    let (mut app, services) = build_app().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;

    let req = delete_with_token(
        &format!("/api/cron/jobs/{MISSING_CRON_JOB_ID}/skill"),
        &token,
        &csrf,
    );
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

// ── SC-5: Invalid cron expression ────────────────────────────────────

#[tokio::test]
async fn sc5_invalid_cron_expression() {
    let (mut app, services) = build_app().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;

    let body = create_cron_job_body("Invalid Cron", "invalid cron");
    let req = json_with_token("POST", "/api/cron/jobs", body, &token, &csrf);
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

// ── SC-6: Cron with timezone ─────────────────────────────────────────

#[tokio::test]
async fn sc6_cron_with_timezone() {
    let (mut app, services) = build_app().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;

    let body = json!({
        "name": "Shanghai Job",
        "schedule": { "kind": "cron", "expr": "0 0 9 * * *", "tz": "Asia/Shanghai" },
        "message": "hello",
        "conversation_id": TEST_CONV_1,
        "agent_type": "acp",
        "created_by": "user"
    });

    seed_conversation(&services, TEST_CONV_1).await;
    let data = create_job(&mut app, &token, &csrf, body).await;
    let now = nomifun_common::now_ms();
    assert!(data["state"]["next_run_at_ms"].as_i64().unwrap() > now);
}

// ── SC-7: Every zero interval ────────────────────────────────────────

#[tokio::test]
async fn sc7_every_zero_interval() {
    let (mut app, services) = build_app().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;

    let body = json!({
        "name": "Zero Interval",
        "schedule": { "kind": "every", "every_ms": 0 },
        "message": "x",
        "conversation_id": TEST_CONV_1,
        "agent_type": "acp",
        "created_by": "user"
    });
    let req = json_with_token("POST", "/api/cron/jobs", body, &token, &csrf);
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

// ── SC-8: Every negative interval ────────────────────────────────────

#[tokio::test]
async fn sc8_every_negative_interval() {
    let (mut app, services) = build_app().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;

    let body = json!({
        "name": "Negative Interval",
        "schedule": { "kind": "every", "every_ms": -1000 },
        "message": "x",
        "conversation_id": TEST_CONV_1,
        "agent_type": "acp",
        "created_by": "user"
    });
    let req = json_with_token("POST", "/api/cron/jobs", body, &token, &csrf);
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

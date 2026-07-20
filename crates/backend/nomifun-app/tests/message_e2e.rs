//! E2E tests for message listing, search, pagination, and auth protection.

mod common;

use axum::body::Body;
use axum::http::header::CACHE_CONTROL;
use axum::http::StatusCode;
use serde_json::json;
use tower::ServiceExt;

use common::{body_json, build_app, build_app_with_mock_agents, get_request, get_with_token, setup_and_login};
use nomifun_common::MessageId;
use nomifun_db::{ConversationRowUpdate, IConversationRepository};

const MISSING_CONVERSATION_ID: &str = "conv_0190f5fe-7c00-7a00-8abc-012345679990";
const MISSING_MESSAGE_ID: &str = "msg_0190f5fe-7c00-7a00-8abc-012345679989";
const TEST_CRON_JOB_ID: &str = "cron_0190f5fe-7c00-7a00-8abc-012345679988";

// ── Helpers ───────────────────────────────────────────────────────────

fn create_conv_body(name: &str) -> serde_json::Value {
    json!({
        "type": "acp",
        "name": name,
        "extra": { "workspace": "/project", "backend": "gemini" }
    })
}

async fn create_conversation(app: &mut axum::Router, token: &str, csrf: &str, name: &str) -> String {
    let req = common::json_with_token("POST", "/api/conversations", create_conv_body(name), token, csrf);
    let resp = app.clone().oneshot(req).await.unwrap();
    let json = common::body_json(resp).await;
    json["data"]["id"].as_str().unwrap().to_owned()
}

async fn insert_message(
    services: &nomifun_app::AppServices,
    conv_id: &str,
    content: &str,
    created_at: i64,
) -> String {
    let repo = nomifun_db::SqliteConversationRepository::new(services.database.pool().clone());
    let message_id = MessageId::new().into_string();
    let msg = nomifun_db::models::MessageRow {
        id: message_id.clone(),
        conversation_id: conv_id.to_owned(),
        msg_id: None,
        r#type: "text".into(),
        content: serde_json::json!({"content": content}).to_string(),
        position: Some("right".into()),
        status: Some("finish".into()),
        hidden: false,
        created_at,
    };
    nomifun_db::IConversationRepository::insert_message(&repo, &msg)
        .await
        .unwrap();
    message_id
}

async fn update_conversation_workspace(services: &nomifun_app::AppServices, conv_id: &str, workspace: &str) {
    let repo = nomifun_db::SqliteConversationRepository::new(services.database.pool().clone());
    IConversationRepository::update(
        &repo,
        conv_id,
        &ConversationRowUpdate {
            extra: Some(json!({ "workspace": workspace, "backend": "gemini" }).to_string()),
            ..Default::default()
        },
    )
    .await
    .unwrap();
}

async fn insert_acp_tool_message(
    services: &nomifun_app::AppServices,
    conv_id: &str,
    tool_call_id: &str,
    output: &str,
    created_at: i64,
) -> String {
    let repo = nomifun_db::SqliteConversationRepository::new(services.database.pool().clone());
    let message_id = MessageId::new().into_string();
    let msg = nomifun_db::models::MessageRow {
        id: message_id.clone(),
        conversation_id: conv_id.to_owned(),
        msg_id: Some(message_id.clone()),
        r#type: "acp_tool_call".into(),
        content: serde_json::json!({
            "session_id": "session-1",
            "update": {
                "session_update": "tool_call",
                "tool_call_id": tool_call_id,
                "status": "completed",
                "title": "rg",
                "kind": "search",
                "raw_input": { "pattern": "needle", "path": "." },
                "content": [{
                    "type": "content",
                    "content": { "type": "text", "text": output }
                }]
            }
        })
        .to_string(),
        position: Some("left".into()),
        status: Some("finish".into()),
        hidden: false,
        created_at,
    };
    nomifun_db::IConversationRepository::insert_message(&repo, &msg)
        .await
        .unwrap();
    message_id
}

async fn upsert_artifact(services: &nomifun_app::AppServices, artifact: nomifun_db::ConversationArtifactRow) -> String {
    let repo = nomifun_db::SqliteConversationRepository::new(services.database.pool().clone());
    nomifun_db::IConversationRepository::upsert_artifact(&repo, &artifact)
        .await
        .unwrap()
        .id
}

/// Seed a minimal `cron_jobs` parent row so artifacts referencing it satisfy
/// the `conversation_artifacts.cron_job_id -> cron_jobs(id)` foreign key.
async fn seed_cron_job(services: &nomifun_app::AppServices, id: &str) {
    sqlx::query(
        "INSERT INTO cron_jobs \
            (id, user_id, name, schedule_kind, schedule_value, payload_message, agent_type, created_by, created_at, updated_at) \
         VALUES (?, ?, 'Job', 'every', '60000', 'msg', 'acp', 'user', 0, 0)",
    )
    .bind(id)
    .bind(services.authoritative_user_id.as_ref())
    .execute(services.database.pool())
    .await
    .unwrap();
}

// ── T8: Message list ──────────────────────────────────────────────────

#[tokio::test]
async fn t8_1_messages_empty() {
    let (mut app, services) = build_app().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;
    let conv_id = create_conversation(&mut app, &token, &csrf, "Empty Conv").await;

    let resp = app
        .oneshot(get_with_token(
            &format!("/api/conversations/{conv_id}/messages"),
            &token,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(resp.headers().get(CACHE_CONTROL).unwrap(), "no-store");
    let json = body_json(resp).await;
    assert_eq!(json["data"]["items"].as_array().unwrap().len(), 0);
    assert_eq!(json["data"]["total"], 0);
}

#[tokio::test]
async fn t8_2_messages_pagination() {
    let (mut app, services) = build_app().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;
    let conv_id = create_conversation(&mut app, &token, &csrf, "Paginated Conv").await;

    // Insert 10 messages
    for i in 0..10 {
        insert_message(
            &services,
            &conv_id,
            &format!("Message {i}"),
            1000 + i * 100,
        )
        .await;
    }

    // Page 1, page_size 3
    let resp = app
        .clone()
        .oneshot(get_with_token(
            &format!("/api/conversations/{conv_id}/messages?page=1&page_size=3"),
            &token,
        ))
        .await
        .unwrap();
    let json = body_json(resp).await;
    assert_eq!(json["data"]["items"].as_array().unwrap().len(), 3);
    assert_eq!(json["data"]["total"], 10);
    assert_eq!(json["data"]["has_more"], true);

    // Last page
    let resp = app
        .oneshot(get_with_token(
            &format!("/api/conversations/{conv_id}/messages?page=4&page_size=3"),
            &token,
        ))
        .await
        .unwrap();
    let json = body_json(resp).await;
    assert_eq!(json["data"]["items"].as_array().unwrap().len(), 1);
    assert_eq!(json["data"]["has_more"], false);
}

#[tokio::test]
async fn t8_2b_messages_compact_mode_truncates_large_tool_payload() {
    let (mut app, services) = build_app().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;
    let conv_id = create_conversation(&mut app, &token, &csrf, "Compact Tool Conv").await;
    let large_output = "match line\n".repeat(10_000);

    insert_acp_tool_message(&services, &conv_id, "tool-big", &large_output, 1000).await;

    let resp = app
        .clone()
        .oneshot(get_with_token(
            &format!("/api/conversations/{conv_id}/messages?content_mode=compact"),
            &token,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let json = body_json(resp).await;
    let content = &json["data"]["items"][0]["content"];
    let preview = content["update"]["content"][0]["content"]["text"].as_str().unwrap();

    assert_eq!(content["_compact"]["truncated"], true);
    assert!(preview.len() < large_output.len());
    assert!(!preview.contains(&large_output));
}

#[tokio::test]
async fn t8_2c_get_message_returns_full_tool_payload() {
    let (mut app, services) = build_app().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;
    let conv_id = create_conversation(&mut app, &token, &csrf, "Tool Detail Conv").await;
    let large_output = "wide rg output\n".repeat(10_000);

    let message_id = insert_acp_tool_message(&services, &conv_id, "tool-detail", &large_output, 1000).await;

    let resp = app
        .clone()
        .oneshot(get_with_token(
            &format!("/api/conversations/{conv_id}/messages/{message_id}"),
            &token,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let json = body_json(resp).await;

    assert_eq!(
        json["data"]["content"]["update"]["content"][0]["content"]["text"]
            .as_str()
            .unwrap(),
        large_output
    );
}

#[tokio::test]
async fn t8_2d_get_message_requires_auth() {
    let (mut app, services) = build_app().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;
    let conv_id = create_conversation(&mut app, &token, &csrf, "Tool Detail Auth Conv").await;

    let resp = app
        .oneshot(get_request(&format!(
            "/api/conversations/{conv_id}/messages/tool-detail"
        )))
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn t8_2e_get_message_not_found_returns_specific_error() {
    let (mut app, services) = build_app().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;
    let conv_id = create_conversation(&mut app, &token, &csrf, "Tool Detail Missing Conv").await;

    let resp = app
        .oneshot(get_with_token(
            &format!("/api/conversations/{conv_id}/messages/{MISSING_MESSAGE_ID}"),
            &token,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    let json = body_json(resp).await;

    assert_eq!(json["code"], "NOT_FOUND");
    assert!(
        json["error"]
            .as_str()
            .unwrap()
            .contains(&format!("Message {MISSING_MESSAGE_ID} not found"))
    );
}

#[tokio::test]
async fn t8_2f_get_message_does_not_leak_cross_user_conversation() {
    let (mut app, services) = build_app().await;
    let (owner_token, owner_csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;
    let owner_conv_id = create_conversation(&mut app, &owner_token, &owner_csrf, "Owner Tool Conv").await;
    let owner_message_id =
        insert_acp_tool_message(&services, &owner_conv_id, "owner-tool", "private output", 1000).await;

    let (other_token, _other_csrf) = setup_and_login(&mut app, &services, "other-user", "StrongP@ss2").await;

    let resp = app
        .oneshot(get_with_token(
            &format!("/api/conversations/{owner_conv_id}/messages/{owner_message_id}"),
            &other_token,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    let json = body_json(resp).await;

    assert_eq!(json["code"], "NOT_FOUND");
    assert!(
        json["error"]
            .as_str()
            .unwrap()
            .contains(&format!("Conversation {owner_conv_id} not found"))
    );
    assert!(!json["error"].as_str().unwrap().contains(&owner_message_id));
    assert!(!json["error"].as_str().unwrap().contains("private output"));
}

#[tokio::test]
async fn t8_3_messages_order_asc_default() {
    let (mut app, services) = build_app().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;
    let conv_id = create_conversation(&mut app, &token, &csrf, "Order Test").await;

    insert_message(&services, &conv_id, "Old", 1000).await;
    insert_message(&services, &conv_id, "Mid", 2000).await;
    insert_message(&services, &conv_id, "New", 3000).await;

    let resp = app
        .oneshot(get_with_token(
            &format!("/api/conversations/{conv_id}/messages"),
            &token,
        ))
        .await
        .unwrap();
    let json = body_json(resp).await;
    let items = json["data"]["items"].as_array().unwrap();
    // ASC order (default): oldest first
    assert!(items[0]["created_at"].as_i64().unwrap() < items[1]["created_at"].as_i64().unwrap());
    assert!(items[1]["created_at"].as_i64().unwrap() < items[2]["created_at"].as_i64().unwrap());
}

#[tokio::test]
async fn t8_4_messages_order_asc() {
    let (mut app, services) = build_app().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;
    let conv_id = create_conversation(&mut app, &token, &csrf, "ASC Test").await;

    insert_message(&services, &conv_id, "Old", 1000).await;
    insert_message(&services, &conv_id, "Mid", 2000).await;
    insert_message(&services, &conv_id, "New", 3000).await;

    let resp = app
        .oneshot(get_with_token(
            &format!("/api/conversations/{conv_id}/messages?order=ASC"),
            &token,
        ))
        .await
        .unwrap();
    let json = body_json(resp).await;
    let items = json["data"]["items"].as_array().unwrap();
    // ASC order: oldest first
    assert!(items[0]["created_at"].as_i64().unwrap() < items[1]["created_at"].as_i64().unwrap());
    assert!(items[1]["created_at"].as_i64().unwrap() < items[2]["created_at"].as_i64().unwrap());
}

#[tokio::test]
async fn t8_5_messages_conversation_not_found() {
    let (mut app, services) = build_app().await;
    let (token, _csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;

    let resp = app
        .oneshot(get_with_token(
            &format!("/api/conversations/{MISSING_CONVERSATION_ID}/messages"),
            &token,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn t8_6_messages_requires_auth() {
    let (app, _services) = build_app().await;
    let resp = app
        .oneshot(get_request("/api/conversations/some-id/messages"))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn t8_7_messages_exclude_legacy_cron_rows() {
    let (mut app, services) = build_app().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;
    let conv_id = create_conversation(&mut app, &token, &csrf, "Legacy Filter").await;

    insert_message(&services, &conv_id, "Visible", 1000).await;

    let repo = nomifun_db::SqliteConversationRepository::new(services.database.pool().clone());
    for (ty, content) in [
        (
            "cron_trigger",
            json!({
                "cron_job_id": TEST_CRON_JOB_ID,
                "cron_job_name": "Daily",
                "triggered_at": 2000
            }),
        ),
        (
            "skill_suggest",
            json!({
                "cron_job_id": TEST_CRON_JOB_ID,
                "name": "daily-report",
                "description": "Daily report",
                "skillContent": "---\nname: daily-report\n---\nUse it."
            }),
        ),
    ] {
        let msg = nomifun_db::models::MessageRow {
            id: MessageId::new().into_string(),
            conversation_id: conv_id.clone(),
            msg_id: None,
            r#type: ty.into(),
            content: content.to_string(),
            position: Some("center".into()),
            status: Some("finish".into()),
            hidden: false,
            created_at: 2000,
        };
        nomifun_db::IConversationRepository::insert_message(&repo, &msg)
            .await
            .unwrap();
    }

    let resp = app
        .oneshot(get_with_token(
            &format!("/api/conversations/{conv_id}/messages"),
            &token,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let json = body_json(resp).await;
    let items = json["data"]["items"].as_array().unwrap();
    assert_eq!(items.len(), 1);
    assert_eq!(json["data"]["total"], 1);
    assert_eq!(items[0]["type"], "text");
    assert_eq!(items[0]["content"]["content"], "Visible");
}

#[tokio::test]
async fn t8_8_artifacts_list_and_patch_status() {
    let (mut app, services) = build_app().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;
    let conv_id = create_conversation(&mut app, &token, &csrf, "Artifacts").await;
    seed_cron_job(&services, TEST_CRON_JOB_ID).await;

    let artifact_id = upsert_artifact(
        &services,
        nomifun_db::ConversationArtifactRow {
            id: nomifun_common::ConversationArtifactId::new().into_string(),
            conversation_id: conv_id.clone(),
            cron_job_id: Some(TEST_CRON_JOB_ID.into()),
            kind: "skill_suggest".into(),
            status: "active".into(),
            payload: json!({
                "cron_job_id": TEST_CRON_JOB_ID,
                "name": "daily-report",
                "description": "Daily report",
                "skillContent": "---\nname: daily-report\n---\nUse it."
            })
            .to_string(),
            created_at: 1000,
            updated_at: 1000,
        },
    )
    .await;

    let resp = app
        .clone()
        .oneshot(get_with_token(
            &format!("/api/conversations/{conv_id}/artifacts"),
            &token,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let json = body_json(resp).await;
    let items = json["data"].as_array().unwrap();
    assert_eq!(items.len(), 1);
    assert_eq!(items[0]["id"], artifact_id);
    assert_eq!(items[0]["kind"], "skill_suggest");
    assert_eq!(items[0]["status"], "active");

    let patch_req = common::json_with_token(
        "PATCH",
        &format!("/api/conversations/{conv_id}/artifacts/{artifact_id}"),
        json!({ "status": "dismissed" }),
        &token,
        &csrf,
    );
    let patch_resp = app.oneshot(patch_req).await.unwrap();
    assert_eq!(patch_resp.status(), StatusCode::OK);
    let patch_json = body_json(patch_resp).await;
    assert_eq!(patch_json["data"]["status"], "dismissed");
}

// ── T9: Message search ────────────────────────────────────────────────

#[tokio::test]
async fn t9_1_search_keyword_match() {
    let (mut app, services) = build_app().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;

    let conv_id = create_conversation(&mut app, &token, &csrf, "Search Conv").await;
    insert_message(&services, &conv_id, "Rust is great", 1000).await;
    insert_message(&services, &conv_id, "Python is also nice", 2000).await;

    let resp = app
        .oneshot(get_with_token("/api/messages/search?keyword=Rust", &token))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let json = body_json(resp).await;
    let items = json["data"]["items"].as_array().unwrap();
    assert_eq!(items.len(), 1);
    assert_eq!(items[0]["conversation"]["name"], "Search Conv");
}

#[tokio::test]
async fn t9_2_search_no_match() {
    let (mut app, services) = build_app().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;

    let conv_id = create_conversation(&mut app, &token, &csrf, "No Match Conv").await;
    insert_message(&services, &conv_id, "Hello world", 1000).await;

    let resp = app
        .oneshot(get_with_token("/api/messages/search?keyword=xxxxnotexist", &token))
        .await
        .unwrap();
    let json = body_json(resp).await;
    assert_eq!(json["data"]["items"].as_array().unwrap().len(), 0);
    assert_eq!(json["data"]["total"], 0);
}

#[tokio::test]
async fn t9_3_search_pagination() {
    let (mut app, services) = build_app().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;

    let conv_id = create_conversation(&mut app, &token, &csrf, "Search Paged").await;
    for i in 0..5 {
        insert_message(
            &services,
            &conv_id,
            &format!("Matching keyword {i}"),
            1000 + i * 100,
        )
        .await;
    }

    let resp = app
        .clone()
        .oneshot(get_with_token(
            "/api/messages/search?keyword=Matching&page=1&page_size=2",
            &token,
        ))
        .await
        .unwrap();
    let json = body_json(resp).await;
    assert_eq!(json["data"]["items"].as_array().unwrap().len(), 2);
    assert_eq!(json["data"]["total"], 5);
    assert_eq!(json["data"]["has_more"], true);
}

#[tokio::test]
async fn t9_4_search_empty_keyword() {
    let (mut app, services) = build_app().await;
    let (token, _csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;

    let resp = app
        .oneshot(get_with_token("/api/messages/search?keyword=", &token))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn t9_5_search_requires_auth() {
    let (app, _services) = build_app().await;
    let resp = app
        .oneshot(get_request("/api/messages/search?keyword=test"))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
}

// ── T12.4: SQL injection safety ───────────────────────────────────────

#[tokio::test]
async fn t12_4_search_sql_injection_safe() {
    let (mut app, services) = build_app().await;
    let (token, _csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;

    let resp = app
        .oneshot(get_with_token(
            "/api/messages/search?keyword=';%20DROP%20TABLE%20messages;%20--",
            &token,
        ))
        .await
        .unwrap();
    // Should not crash; just return empty results
    assert_eq!(resp.status(), StatusCode::OK);
    let json = body_json(resp).await;
    assert_eq!(json["data"]["items"].as_array().unwrap().len(), 0);
}

// ── Message response field validation ─────────────────────────────────

#[tokio::test]
async fn message_response_has_correct_fields() {
    let (mut app, services) = build_app().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;

    let conv_id = create_conversation(&mut app, &token, &csrf, "Field Check").await;
    insert_message(&services, &conv_id, "Content check", 5000).await;

    let resp = app
        .oneshot(get_with_token(
            &format!("/api/conversations/{conv_id}/messages"),
            &token,
        ))
        .await
        .unwrap();
    let json = body_json(resp).await;
    let msg = &json["data"]["items"][0];

    // Verify snake_case fields exist
    assert!(msg.get("id").is_some());
    assert!(msg.get("conversation_id").is_some());
    assert!(msg.get("type").is_some());
    assert!(msg.get("content").is_some());
    assert!(msg.get("position").is_some());
    assert!(msg.get("status").is_some());
    assert!(msg.get("created_at").is_some());
    // Verify no camelCase leaks
    assert!(msg.get("conversationId").is_none());
    assert!(msg.get("createdAt").is_none());
    assert!(msg.get("msgId").is_none());
}

// ── Delete cascades messages ──────────────────────────────────────────

#[tokio::test]
async fn delete_conversation_cascades_messages() {
    let (mut app, services) = build_app().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;

    let conv_id = create_conversation(&mut app, &token, &csrf, "Cascade Test").await;
    insert_message(&services, &conv_id, "msg 1", 1000).await;
    insert_message(&services, &conv_id, "msg 2", 2000).await;

    // Delete the conversation
    let resp = app
        .clone()
        .oneshot(common::delete_with_token(
            &format!("/api/conversations/{conv_id}"),
            &token,
            &csrf,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    // Search for messages from the deleted conversation should return nothing
    let resp = app
        .oneshot(get_with_token("/api/messages/search?keyword=msg", &token))
        .await
        .unwrap();
    let json = body_json(resp).await;
    assert_eq!(json["data"]["items"].as_array().unwrap().len(), 0);
}

// ── Cross-conversation search ─────────────────────────────────────────

#[tokio::test]
async fn search_across_multiple_conversations() {
    let (mut app, services) = build_app().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;

    let conv1 = create_conversation(&mut app, &token, &csrf, "Conv Alpha").await;
    let conv2 = create_conversation(&mut app, &token, &csrf, "Conv Beta").await;

    insert_message(&services, &conv1, "Rust review needed", 1000).await;
    insert_message(&services, &conv2, "Rust performance tips", 2000).await;
    insert_message(&services, &conv2, "Python patterns", 3000).await;

    let resp = app
        .oneshot(get_with_token("/api/messages/search?keyword=Rust", &token))
        .await
        .unwrap();
    let json = body_json(resp).await;
    let items = json["data"]["items"].as_array().unwrap();
    assert_eq!(items.len(), 2);
    assert_eq!(json["data"]["total"], 2);
}

// ── T2.1: Send message ──────────────────────────────────────────────

#[tokio::test]
async fn t2_1_send_message_accepted() {
    let (mut app, services) = build_app().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;
    let conv_id = create_conversation(&mut app, &token, &csrf, "Send Test").await;

    let body = json!({ "content": "Hello AI" });
    let req = common::json_with_token(
        "POST",
        &format!("/api/conversations/{conv_id}/messages"),
        body,
        &token,
        &csrf,
    );
    let resp = app.oneshot(req).await.unwrap();
    // The stub agent factory returns an error, so we expect 500
    // (the route itself is wired correctly — 202 when factory is real)
    // In E2E with stub factory, the get_or_create_runtime fails.
    // We verify the route is reachable and returns an error (not 404/405).
    // 400 may occur when the stub environment lacks valid backend configuration.
    let status = resp.status();
    assert!(
        status == StatusCode::ACCEPTED
            || status == StatusCode::INTERNAL_SERVER_ERROR
            || status == StatusCode::BAD_REQUEST,
        "Expected 202, 400, or 500 (stub factory), got {status}"
    );

    if status == StatusCode::ACCEPTED {
        let body: serde_json::Value =
            serde_json::from_slice(&axum::body::to_bytes(resp.into_body(), usize::MAX).await.unwrap()).unwrap();
        assert!(body["success"].as_bool().unwrap());
        assert!(body["data"]["msg_id"].as_str().is_some_and(|s| !s.is_empty()));
    }
}

#[tokio::test]
async fn t2_1_send_message_empty_content_bad_request() {
    let (mut app, services) = build_app().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;
    let conv_id = create_conversation(&mut app, &token, &csrf, "Empty Content").await;

    let body = json!({ "content": "" });
    let req = common::json_with_token(
        "POST",
        &format!("/api/conversations/{conv_id}/messages"),
        body,
        &token,
        &csrf,
    );
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn t2_1_send_message_conversation_not_found() {
    let (mut app, services) = build_app().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;

    let body = json!({ "content": "Hello" });
    let req = common::json_with_token(
        "POST",
        &format!("/api/conversations/{MISSING_CONVERSATION_ID}/messages"),
        body,
        &token,
        &csrf,
    );
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn t2_1b_send_message_pathological_workspace_returns_runtime_whitespace_code() {
    let (mut app, services) = build_app_with_mock_agents().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;
    let conv_id = create_conversation(&mut app, &token, &csrf, "Legacy Workspace").await;
    update_conversation_workspace(&services, &conv_id, "/tmp/my project ").await;

    let body = json!({ "content": "Hello" });
    let req = common::json_with_token(
        "POST",
        &format!("/api/conversations/{conv_id}/messages"),
        body,
        &token,
        &csrf,
    );
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);

    let json = body_json(resp).await;
    assert_eq!(json["code"], "WORKSPACE_PATH_EDGE_WHITESPACE_RUNTIME_UNSUPPORTED");
    assert_eq!(json["details"]["workspace_path"], "/tmp/my project ");
    assert_eq!(json["details"]["operation"], "runtime");
}

/// Regression for the macOS per-user data dir: `~/Library/Application
/// Support/NomiFun/Nomi/conversations/...` contains interior whitespace and
/// every conversation auto-provisioned under it must remain sendable.
#[tokio::test]
async fn t2_1c_send_message_accepts_interior_whitespace_workspace() {
    let (mut app, services) = build_app_with_mock_agents().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;
    let conv_id = create_conversation(&mut app, &token, &csrf, "macOS Workspace").await;

    let temp = tempfile::tempdir().unwrap();
    let workspace = temp.path().join("Application Support").join("Nomi").join("conversations").join("nomi-temp-1");
    std::fs::create_dir_all(&workspace).unwrap();
    update_conversation_workspace(&services, &conv_id, &workspace.to_string_lossy()).await;

    let body = json!({ "content": "Hello" });
    let req = common::json_with_token(
        "POST",
        &format!("/api/conversations/{conv_id}/messages"),
        body,
        &token,
        &csrf,
    );
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::ACCEPTED);

    let json = body_json(resp).await;
    assert!(json["success"].as_bool().unwrap());
    assert!(json["data"]["msg_id"].as_str().is_some_and(|s| !s.is_empty()));
}

#[tokio::test]
async fn t2_1d_accepted_user_message_is_immediately_readable_without_cache() {
    let (mut app, services) = build_app_with_mock_agents().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;
    let conv_id = create_conversation(&mut app, &token, &csrf, "Message Readback").await;

    let temp = tempfile::tempdir().unwrap();
    let workspace = temp.path().join("conversation");
    std::fs::create_dir_all(&workspace).unwrap();
    update_conversation_workspace(&services, &conv_id, &workspace.to_string_lossy()).await;

    let req = common::json_with_token(
        "POST",
        &format!("/api/conversations/{conv_id}/messages"),
        json!({ "content": "survives conversation reload" }),
        &token,
        &csrf,
    );
    let send_resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(send_resp.status(), StatusCode::ACCEPTED);
    let send_json = body_json(send_resp).await;
    let msg_id = send_json["data"]["msg_id"].as_str().unwrap();

    // The accepted response is sent only after the user row commits. A fresh
    // history read must therefore see that exact row, and must never be served
    // from a WebView/browser cache when the conversation is revisited.
    let history_resp = app
        .oneshot(get_with_token(
            &format!("/api/conversations/{conv_id}/messages"),
            &token,
        ))
        .await
        .unwrap();
    assert_eq!(history_resp.status(), StatusCode::OK);
    assert_eq!(history_resp.headers().get(CACHE_CONTROL).unwrap(), "no-store");
    let history_json = body_json(history_resp).await;
    let user_message = history_json["data"]["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|message| message["msg_id"].as_str() == Some(msg_id))
        .expect("accepted user message must be present in immediate history readback");

    assert_eq!(user_message["position"], "right");
    assert_eq!(user_message["content"]["content"], "survives conversation reload");
}

#[tokio::test]
async fn t2_1_send_message_requires_auth() {
    let (app, _services) = build_app().await;

    let body = json!({ "content": "Hello" });
    let req = axum::http::Request::builder()
        .method("POST")
        .uri("/api/conversations/some-id/messages")
        .header("content-type", "application/json")
        .body(Body::from(serde_json::to_vec(&body).unwrap()))
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
}

// ── T2.2: Stop stream ───────────────────────────────────────────────

#[tokio::test]
async fn t2_2_stop_stream_conversation_not_found() {
    let (mut app, services) = build_app().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;

    let req = common::json_with_token(
        "POST",
        &format!("/api/conversations/{MISSING_CONVERSATION_ID}/cancel"),
        json!({}),
        &token,
        &csrf,
    );
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn t2_2_stop_stream_requires_auth() {
    let (app, _services) = build_app().await;

    let req = axum::http::Request::builder()
        .method("POST")
        .uri("/api/conversations/some-id/cancel")
        .header("content-type", "application/json")
        .body(Body::empty())
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
}

// ── T2.3: Warmup ────────────────────────────────────────────────────

#[tokio::test]
async fn t2_3_warmup_conversation_not_found() {
    let (mut app, services) = build_app().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;

    let req = common::json_with_token(
        "POST",
        &format!("/api/conversations/{MISSING_CONVERSATION_ID}/warmup"),
        json!({}),
        &token,
        &csrf,
    );
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn t2_3b_warmup_pathological_workspace_returns_runtime_whitespace_code() {
    let (mut app, services) = build_app_with_mock_agents().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;
    let conv_id = create_conversation(&mut app, &token, &csrf, "Legacy Warmup").await;
    update_conversation_workspace(&services, &conv_id, "/tmp/my project ").await;

    let req = common::json_with_token(
        "POST",
        &format!("/api/conversations/{conv_id}/warmup"),
        json!({}),
        &token,
        &csrf,
    );
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);

    let json = body_json(resp).await;
    assert_eq!(json["code"], "WORKSPACE_PATH_EDGE_WHITESPACE_RUNTIME_UNSUPPORTED");
    assert_eq!(json["details"]["workspace_path"], "/tmp/my project ");
    assert_eq!(json["details"]["operation"], "runtime");
}

#[tokio::test]
async fn t2_3_warmup_requires_auth() {
    let (app, _services) = build_app().await;

    let req = axum::http::Request::builder()
        .method("POST")
        .uri("/api/conversations/some-id/warmup")
        .header("content-type", "application/json")
        .body(Body::empty())
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
}

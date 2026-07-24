//! E2E tests for the Platform Gateway MCP server (`nomifun-gateway`).
//!
//! The gateway is not part of the axum app router — it is a separate
//! localhost HTTP server started by `AppServices::from_config` and
//! deps-wired by `create_router`. These tests exercise the full stack the
//! way the stdio bridge does: authenticated `POST /tool` against the real
//! port, with real services (memory db) behind it.

mod common;

use common::build_app;

const TEST_CONV_1: &str = "0190f5fe-7c00-7a00-8abc-012345678901";
const TEST_CONV_2: &str = "0190f5fe-7c00-7a00-8abc-012345678902";
const TEST_OWNER_CALLER: &str = "0190f5fe-7c00-7a00-8abc-012345678903";
const TEST_SECONDARY_CALLER: &str = "0190f5fe-7c00-7a00-8abc-012345678904";
const TEST_COMPANION_CALLER: &str = "0190f5fe-7c00-7a00-8abc-012345678905";
const TEST_USER_GATEWAY: &str = "0190f5fe-7c00-7a00-8abc-012345678911";
const TEST_USER_COMPANION: &str = "0190f5fe-7c00-7a00-8abc-012345678912";
const TEST_USER_A: &str = "0190f5fe-7c00-7a00-8abc-012345678913";
const TEST_USER_B: &str = "0190f5fe-7c00-7a00-8abc-012345678914";
const TEST_USER_SECONDARY: &str = "0190f5fe-7c00-7a00-8abc-012345678915";
const TEST_COMPANION: &str = "0190f5fe-7c00-7a00-8abc-012345678921";
const TEST_PROVIDER: &str = "0190f5fe-7c00-7a00-8abc-012345678931";

use serde_json::{Value, json};

struct Gateway {
    config: nomifun_api_types::GatewayMcpConfig,
    client: reqwest::Client,
}

fn local_http_client() -> reqwest::Client {
    reqwest::Client::builder().no_proxy().build().unwrap()
}

impl Gateway {
    fn from_services(services: &nomifun_app::AppServices) -> Self {
        let cfg = services
            .gateway_mcp_config
            .as_ref()
            .expect("gateway MCP server must start in tests");
        Self {
            config: cfg.clone(),
            client: local_http_client(),
        }
    }

    async fn call(&self, tool: &str, caller_conv: &str, user_id: &str, args: Value) -> Value {
        self.call_with_companion(tool, caller_conv, user_id, None, args)
            .await
    }

    async fn call_with_companion(
        &self,
        tool: &str,
        caller_conv: &str,
        user_id: &str,
        companion_id: Option<&str>,
        args: Value,
    ) -> Value {
        let child = self
            .config
            .issue_for_conversation(user_id, caller_conv, companion_id, None, None, &[])
            .expect("valid Gateway child capability");
        let access = &child.bootstrap.access;
        let resp = self
            .client
            .post(format!("http://127.0.0.1:{}/tool", self.config.port()))
            .header("Authorization", format!("Bearer {}", access.token))
            .header(
                "Idempotency-Key",
                format!("gateway-e2e-{}", uuid::Uuid::new_v4()),
            )
            .json(&json!({
                "tool": tool,
                "args": args,
                "session": access.claims,
            }))
            .send()
            .await
            .expect("gateway reachable");
        resp.json().await.expect("json body")
    }
}

/// Seed a user + one conversation directly (the gateway scopes everything by
/// user id; HTTP signup/login is irrelevant to what we exercise here).
///
/// `conv_id` is a canonical conversation ID.
async fn seed_user_and_conversation(services: &nomifun_app::AppServices, user_id: &str, conv_id: &str) {
    seed_user_and_conversation_with_extra(services, user_id, conv_id, "{}").await;
}

/// Same as [`seed_user_and_conversation`] but with a caller-provided `extra`
/// JSON (e.g. a Channel Agent session carrying `companion_id`).
async fn seed_user_and_conversation_with_extra(
    services: &nomifun_app::AppServices,
    user_id: &str,
    conv_id: &str,
    extra: &str,
) {
    sqlx::query("INSERT OR IGNORE INTO users (id, username, password_hash, created_at, updated_at) VALUES (?, ?, 'hash', 0, 0)")
        .bind(user_id)
        .bind(format!("user-{user_id}"))
        .execute(services.database.pool())
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO conversations \
         (id, user_id, name, type, extra, delegation_policy, created_at, updated_at) \
         VALUES (?, ?, ?, 'nomi', ?, 'disabled', 0, 0)",
    )
    .bind(conv_id)
    .bind(user_id)
    .bind(format!("Conv {conv_id}"))
    .bind(extra)
    .execute(services.database.pool())
    .await
    .unwrap();
}

/// Seed an enabled provider so tools that resolve a default model (e.g.
/// `nomi_cron_create` auto-filling a model-less nomi conversation) can
/// complete their fallback chain.
async fn seed_provider(services: &nomifun_app::AppServices, provider_id: &str, model: &str) {
    sqlx::query(
        "INSERT OR IGNORE INTO providers \
         (provider_id, platform, name, base_url, api_key_encrypted, models, enabled, created_at, updated_at) \
         VALUES (?, 'openai', ?, 'http://127.0.0.1:1', 'k', ?, 1, 0, 0)",
    )
    .bind(provider_id)
    .bind(format!("Provider {provider_id}"))
    .bind(format!("[\"{model}\"]"))
    .execute(services.database.pool())
    .await
    .unwrap();
}

fn result_of(body: &Value) -> &Value {
    body.get("result")
        .unwrap_or_else(|| panic!("expected result, got {body}"))
}

fn error_of(body: &Value) -> &str {
    body.get("error")
        .and_then(Value::as_str)
        .unwrap_or_else(|| panic!("expected error, got {body}"))
}

// ── auth ─────────────────────────────────────────────────────────────

#[tokio::test]
async fn gw_unauthenticated_tool_call_is_rejected() {
    let (_app, services) = build_app().await;
    let cfg = services.gateway_mcp_config.as_ref().unwrap();
    let resp = local_http_client()
        .post(format!("http://127.0.0.1:{}/tool", cfg.port()))
        .json(&json!({"tool": "nomi_list_conversations", "args": {}}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 401);
}

#[tokio::test]
async fn gw_unknown_tool_returns_error() {
    let (_app, services) = build_app().await;
    let gw = Gateway::from_services(&services);
    let body = gw
        .call(
            "nomi_explode_desktop",
            TEST_OWNER_CALLER,
            services.authoritative_user_id.as_ref(),
            json!({}),
        )
        .await;
    assert!(error_of(&body).contains("Unknown tool"));
}

#[tokio::test]
async fn gw_secondary_session_cannot_invoke_owner_only_tool_with_valid_session_token() {
    let (_app, services) = build_app().await;
    let gw = Gateway::from_services(&services);
    let body = gw
        .call(
            "nomi_system_get_settings",
            TEST_SECONDARY_CALLER,
            TEST_USER_SECONDARY,
            json!({}),
        )
        .await;
    assert_eq!(error_of(&body), "session_capability_denied");
}

// ── conversations ────────────────────────────────────────────────────

#[tokio::test]
async fn gw_list_conversations_returns_rows_with_runtime_state() {
    let (_app, services) = build_app().await;
    seed_user_and_conversation(&services, TEST_USER_GATEWAY, TEST_CONV_1).await;
    seed_user_and_conversation(&services, TEST_USER_GATEWAY, TEST_CONV_2).await;
    let gw = Gateway::from_services(&services);

    let body = gw.call("nomi_list_conversations", TEST_CONV_1, TEST_USER_GATEWAY, json!({})).await;
    let result = result_of(&body);
    assert_eq!(result["total"], json!(2));
    let convs = result["conversations"].as_array().unwrap();
    assert_eq!(convs.len(), 2);
    for conv in convs {
        assert_eq!(conv["runtime_state"], json!("idle"));
    }
    let self_marked: Vec<bool> = convs.iter().map(|c| c["is_self"].as_bool().unwrap()).collect();
    assert!(self_marked.contains(&true), "caller conversation flagged is_self");
}

#[tokio::test]
async fn gw_issuer_rejects_missing_user_identity_before_bridge_spawn() {
    let (_app, services) = build_app().await;
    let gw = Gateway::from_services(&services);
    assert_eq!(
        gw.config
            .issue_for_conversation("", TEST_CONV_1, None, None, None, &[])
            .unwrap_err(),
        nomifun_common::LoopbackCapabilityError::InvalidIdentity
    );
}

#[tokio::test]
async fn gw_list_conversations_excludes_companion_sessions() {
    let (_app, services) = build_app().await;
    // A companion (work-partner) single session…
    seed_user_and_conversation_with_extra(
        &services,
        TEST_USER_COMPANION,
        TEST_CONV_1,
        &format!(r#"{{"companion_session":true,"companion_id":"{TEST_COMPANION}"}}"#),
    )
    .await;
    // …and a plain session with no binding.
    seed_user_and_conversation(&services, TEST_USER_COMPANION, TEST_CONV_2).await;
    let gw = Gateway::from_services(&services);

    let body = gw
        .call("nomi_list_conversations", TEST_CONV_2, TEST_USER_COMPANION, json!({}))
        .await;
    let result = result_of(&body);
    let convs = result["conversations"].as_array().unwrap().clone();

    // Product rule: a companion's own work-partner session is not part of the
    // session list — it is filtered out of BOTH the page and the total.
    assert_eq!(convs.len(), 1, "companion session excluded, got {convs:?}");
    assert_eq!(result["total"], json!(1));
    assert!(
        convs.iter().all(|c| c["conversation_id"] != json!(TEST_CONV_1)),
        "companion-bound conversation must be absent"
    );

    // The surviving entry is the plain session, with no companion binding.
    // Conversation IDs cross the gateway boundary as canonical strings.
    let plain = &convs[0];
    assert_eq!(plain["conversation_id"], json!(TEST_CONV_2));
    assert_eq!(plain["companion_id"], json!(null), "no binding → null, got {plain}");
    assert_eq!(plain["is_companion_companion"], json!(false));
}

#[tokio::test]
async fn gw_plain_conversation_cannot_create_a_top_level_conversation() {
    let (_app, services) = build_app().await;
    let gw = Gateway::from_services(&services);
    let before: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM conversations")
        .fetch_one(services.database.pool())
        .await
        .unwrap();

    let body = gw
        .call(
            "nomi_create_conversation",
            TEST_CONV_1,
            services.authoritative_user_id.as_ref(),
            json!({"name": "must not exist", "agent_type": "acp", "backend": "codex"}),
        )
        .await;
    assert_eq!(error_of(&body), "session_capability_denied");

    let after: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM conversations")
        .fetch_one(services.database.pool())
        .await
        .unwrap();
    assert_eq!(after, before, "denied creation must not persist a row");
}

#[tokio::test]
async fn gw_companion_can_create_a_top_level_conversation() {
    let (_app, services) = build_app().await;
    let gw = Gateway::from_services(&services);

    let body = gw
        .call_with_companion(
            "nomi_create_conversation",
            TEST_COMPANION_CALLER,
            services.authoritative_user_id.as_ref(),
            Some(TEST_COMPANION),
            json!({"name": "伙伴创建的会话", "agent_type": "acp", "backend": "codex"}),
        )
        .await;
    let created = result_of(&body);
    assert_eq!(created["name"], json!("伙伴创建的会话"));
    assert_eq!(created["agent_type"], json!("acp"));

    let persisted: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM conversations WHERE name = '伙伴创建的会话'",
    )
    .fetch_one(services.database.pool())
    .await
    .unwrap();
    assert_eq!(persisted, 1);
}

#[tokio::test]
async fn gw_send_to_own_conversation_is_refused() {
    let (_app, services) = build_app().await;
    seed_user_and_conversation(&services, TEST_USER_GATEWAY, TEST_CONV_1).await;
    let gw = Gateway::from_services(&services);
    let body = gw
        .call(
            "nomi_send_to_conversation",
            TEST_CONV_1,
            TEST_USER_GATEWAY,
            json!({"conversation_id": TEST_CONV_1, "content": "hi me"}),
        )
        .await;
    assert!(error_of(&body).contains("self_injection_forbidden"));
}

#[tokio::test]
async fn gw_delete_own_conversation_is_refused_but_other_succeeds() {
    let (_app, services) = build_app().await;
    seed_user_and_conversation(&services, TEST_USER_GATEWAY, TEST_CONV_1).await;
    seed_user_and_conversation(&services, TEST_USER_GATEWAY, TEST_CONV_2).await;
    let gw = Gateway::from_services(&services);

    let body = gw
        .call(
            "nomi_delete_conversation",
            TEST_CONV_1,
            TEST_USER_GATEWAY,
            json!({"conversation_id": TEST_CONV_1, "confirm": true}),
        )
        .await;
    assert!(error_of(&body).contains("self_deletion_forbidden"));

    let body = gw
        .call(
            "nomi_delete_conversation",
            TEST_CONV_1,
            TEST_USER_GATEWAY,
            json!({"conversation_id": TEST_CONV_2, "confirm": true}),
        )
        .await;
    // `delete` echoes the canonical conversation ID.
    assert_eq!(result_of(&body)["deleted"], json!(TEST_CONV_2));

    let body = gw.call("nomi_list_conversations", TEST_CONV_1, TEST_USER_GATEWAY, json!({})).await;
    assert_eq!(result_of(&body)["total"], json!(1));
}

#[tokio::test]
async fn gw_conversation_status_reports_idle_and_messages() {
    let (_app, services) = build_app().await;
    seed_user_and_conversation(&services, TEST_USER_GATEWAY, TEST_CONV_1).await;
    let gw = Gateway::from_services(&services);
    let body = gw
        .call(
            "nomi_conversation_status",
            TEST_CONV_1,
            TEST_USER_GATEWAY,
            json!({"conversation_id": TEST_CONV_1}),
        )
        .await;
    let result = result_of(&body);
    assert_eq!(result["conversation_id"], json!(TEST_CONV_1));
    assert_eq!(result["runtime"]["state"], json!("idle"));
    assert!(result.get("recent_messages").is_some());
}

#[tokio::test]
async fn gw_user_isolation_hides_other_users_conversations() {
    let (_app, services) = build_app().await;
    seed_user_and_conversation(&services, TEST_USER_A, TEST_CONV_1).await;
    seed_user_and_conversation(&services, TEST_USER_B, TEST_CONV_2).await;
    let gw = Gateway::from_services(&services);

    // user_b cannot read or delete user_a's conversation.
    let body = gw
        .call(
            "nomi_conversation_status",
            TEST_CONV_2,
            TEST_USER_B,
            json!({"conversation_id": TEST_CONV_1}),
        )
        .await;
    assert!(error_of(&body).contains("not found"), "got {body}");
    let body = gw
        .call(
            "nomi_delete_conversation",
            TEST_CONV_2,
            TEST_USER_B,
            json!({"conversation_id": TEST_CONV_1, "confirm": true}),
        )
        .await;
    assert!(error_of(&body).contains("not found"), "got {body}");
}

// ── cron ─────────────────────────────────────────────────────────────

#[tokio::test]
async fn gw_cron_create_list_update_delete_roundtrip() {
    let (_app, services) = build_app().await;
    seed_user_and_conversation(&services, TEST_USER_GATEWAY, TEST_CONV_1).await;
    // The conversation is seeded without a model: cron creation must resolve
    // one via the fallback chain (companion profile → first enabled provider), so a
    // provider has to exist — a model-less desktop is refused with guidance.
    seed_provider(&services, TEST_PROVIDER, "test-model").await;
    let gw = Gateway::from_services(&services);

    let body = gw
        .call(
            "nomi_cron_create",
            TEST_CONV_1,
            TEST_USER_GATEWAY,
            json!({"name": "晨报", "cron": "0 9 * * *", "description": "每天 9 点", "message": "写晨报"}),
        )
        .await;
    // The create result became an object when the duplicate-guard and the
    // model-fallback note were introduced: {"message": ..., "model_note": ...}.
    let created = result_of(&body);
    let created_msg = created["message"].as_str().unwrap().to_owned();
    assert!(created_msg.contains("晨报"), "got {created_msg}");
    // The seeded conversation had no model — the fallback chain must have
    // auto-selected the only enabled provider and said so.
    let model_note = created["model_note"].as_str().unwrap_or_default();
    assert!(model_note.contains(TEST_PROVIDER) || model_note.contains("test-model"), "got model_note {model_note}");

    let body = gw.call("nomi_cron_list", TEST_CONV_1, TEST_USER_GATEWAY, json!({})).await;
    let jobs = result_of(&body).as_array().unwrap();
    assert_eq!(jobs.len(), 1);
    let cron_job_id = jobs[0]["cron_job_id"].as_str().unwrap().to_owned();
    assert_eq!(jobs[0]["name"], json!("晨报"));

    let body = gw
        .call(
            "nomi_cron_update",
            TEST_CONV_1,
            TEST_USER_GATEWAY,
            json!({"cron_job_id": cron_job_id, "name": "晚报", "cron": "0 21 * * *", "message": "写晚报"}),
        )
        .await;
    assert!(result_of(&body).as_str().unwrap().contains("晚报"));

    let body = gw
        .call("nomi_cron_delete", TEST_CONV_1, TEST_USER_GATEWAY, json!({"cron_job_id": cron_job_id, "confirm": true}))
        .await;
    assert!(result_of(&body).as_str().unwrap().contains("Deleted"));

    let body = gw.call("nomi_cron_list", TEST_CONV_1, TEST_USER_GATEWAY, json!({})).await;
    assert!(result_of(&body).as_array().unwrap().is_empty());
}

// ── global memory ────────────────────────────────────────────────────

#[tokio::test]
async fn gw_memory_save_list_update_delete_roundtrip() {
    let (_app, services) = build_app().await;
    let gw = Gateway::from_services(&services);

    let body = gw
        .call(
            "nomi_memory_save",
            TEST_OWNER_CALLER,
            services.authoritative_user_id.as_ref(),
            json!({"content": "主人喜欢深色主题", "kind": "preference", "tags": ["ui"]}),
        )
        .await;
    let memory_id = result_of(&body)["memory_id"].as_str().unwrap().to_owned();
    assert!(result_of(&body).get("id").is_none());

    let body = gw
        .call(
            "nomi_memory_list",
            TEST_OWNER_CALLER,
            services.authoritative_user_id.as_ref(),
            json!({"query": "深色主题"}),
        )
        .await;
    let memories = result_of(&body).as_array().unwrap();
    assert_eq!(memories.len(), 1);
    assert_eq!(memories[0]["kind"], json!("preference"));
    assert!(memories[0].get("memory_id").is_some());
    assert!(memories[0].get("id").is_none());

    let body = gw
        .call(
            "nomi_memory_update",
            TEST_OWNER_CALLER,
            services.authoritative_user_id.as_ref(),
            json!({"memory_id": memory_id, "pinned": true}),
        )
        .await;
    assert!(result_of(&body).as_str().unwrap().contains("updated"));

    let body = gw
        .call(
            "nomi_memory_delete",
            TEST_OWNER_CALLER,
            services.authoritative_user_id.as_ref(),
            json!({"memory_id": memory_id, "confirm": true}),
        )
        .await;
    assert!(result_of(&body).as_str().unwrap().contains("deleted"));

    let body = gw
        .call(
            "nomi_memory_list",
            TEST_OWNER_CALLER,
            services.authoritative_user_id.as_ref(),
            json!({"query": "深色主题"}),
        )
        .await;
    assert!(result_of(&body).as_array().unwrap().is_empty());
}

#[tokio::test]
async fn gw_memory_update_with_no_fields_is_rejected() {
    let (_app, services) = build_app().await;
    let gw = Gateway::from_services(&services);
    let body = gw
        .call(
            "nomi_memory_update",
            TEST_OWNER_CALLER,
            services.authoritative_user_id.as_ref(),
            json!({"memory_id": "0190f5fe-7c00-7a00-8abc-012345678999"}),
        )
        .await;
    assert!(error_of(&body).contains("nothing to update"));
}

// ── requirements ─────────────────────────────────────────────────────

#[tokio::test]
async fn gw_requirement_create_list_update_delete_roundtrip() {
    let (_app, services) = build_app().await;
    let gw = Gateway::from_services(&services);

    let body = gw
        .call(
            "nomi_requirement_create",
            TEST_OWNER_CALLER,
            services.authoritative_user_id.as_ref(),
            json!({"title": "修复登录页样式", "content": "按钮溢出", "tag": "前端"}),
        )
        .await;
    let req_id = result_of(&body)["requirement_id"].as_str().unwrap().to_owned();
    assert!(result_of(&body).get("id").is_none());
    assert_eq!(result_of(&body)["created_by"], json!("agent"));

    let body = gw
        .call(
            "nomi_requirement_list",
            TEST_OWNER_CALLER,
            services.authoritative_user_id.as_ref(),
            json!({"tag": "前端"}),
        )
        .await;
    let items = result_of(&body)["items"].as_array().unwrap().clone();
    assert_eq!(items.len(), 1);
    assert!(items[0].get("requirement_id").is_some());
    assert!(items[0].get("id").is_none());

    let body = gw
        .call(
            "nomi_requirement_update",
            TEST_OWNER_CALLER,
            services.authoritative_user_id.as_ref(),
            json!({"requirement_id": req_id, "title": "修复登录页按钮溢出"}),
        )
        .await;
    assert_eq!(result_of(&body)["title"], json!("修复登录页按钮溢出"));

    let body = gw
        .call(
            "nomi_requirement_delete",
            TEST_OWNER_CALLER,
            services.authoritative_user_id.as_ref(),
            json!({"requirement_id": req_id, "confirm": true}),
        )
        .await;
    assert!(result_of(&body).as_str().unwrap().contains("deleted"));
}

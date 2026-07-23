//! Application-level contract for the unified Agent Execution boundary.
//!
//! The execution crate owns lifecycle behavior; this test proves the app mounts
//! that facade behind authentication and wires it to the same seven-table SQLite
//! repository used by the rest of the process. No model call is needed.

mod common;

use std::sync::Arc;

use axum::http::StatusCode;
use tower::ServiceExt;

use common::{
    body_json, build_app, delete_with_token, get_request, get_with_token, json_with_token,
    setup_and_login,
};
use nomifun_common::{
    AdaptationPolicy, AgentExecutionEventKind, AgentExecutionStatus, DecisionPolicy,
    DelegationPolicy, PlanGate,
};
use nomifun_db::{
    CreateAgentExecutionParams, IAgentExecutionRepository, NewAgentExecutionEvent,
    NewAgentExecutionParticipant, SqliteAgentExecutionRepository,
};

const NOMI_AGENT_ID: &str = "0190f5fe-7c00-7a00-8000-000000000114";

async fn seed_test_provider(services: &nomifun_app::AppServices) {
    nomifun_db::sqlx::query(
        "INSERT INTO providers (\
            provider_id, platform, name, base_url, api_key_encrypted, models, enabled, \
            capabilities, created_at, updated_at\
         ) VALUES ('0190f5fe-7c00-7a00-8000-000000000013', 'openai', 'test', 'https://example.invalid', \
                   'encrypted', '[\"model_test\"]', 1, '[]', 1, 1)",
    )
    .execute(services.database.pool())
    .await
    .unwrap();
}

#[tokio::test]
async fn execution_routes_are_authenticated_and_legacy_routes_are_gone() {
    let (mut app, services) = build_app().await;

    let response = app
        .clone()
        .oneshot(get_request("/api/agent-executions"))
        .await
        .unwrap();
    assert!(
        matches!(
            response.status(),
            StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN
        ),
        "Agent Execution list must be protected, got {}",
        response.status()
    );

    let (token, _csrf) =
        setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;
    let response = app
        .clone()
        .oneshot(get_with_token("/api/orchestrator/fleets", &token))
        .await
        .unwrap();
    assert_eq!(
        response.status(),
        StatusCode::NOT_FOUND,
        "the removed Fleet/Orchestrator surface must not remain as an alias"
    );
}

#[tokio::test]
async fn repository_execution_round_trips_through_the_app_engine() {
    let (mut app, services) = build_app().await;
    let installation_owner = services.authoritative_user_id.to_string();
    seed_test_provider(&services).await;
    let (token, csrf) =
        setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;

    let repository: Arc<dyn IAgentExecutionRepository> = Arc::new(
        SqliteAgentExecutionRepository::new(services.database.pool().clone()),
    );
    let row = repository
        .create_execution_with_participants(
            &installation_owner,
            &CreateAgentExecutionParams {
                goal: "验证统一执行边界".to_owned(),
                status: AgentExecutionStatus::Paused,
                plan_gate: PlanGate::Automatic,
                adaptation_policy: AdaptationPolicy::Fixed,
                decision_policy: DecisionPolicy::Automatic,
                delegation_policy: DelegationPolicy::Automatic,
                max_parallel: 2,
                work_dir: None,
                lead_conversation_id: None,
                initial_plan_input: r#"{"mode":"automatic"}"#.to_owned(),
            },
            &[NewAgentExecutionParticipant {
                participant_id: nomifun_common::generate_id(),
                source_agent_id: NOMI_AGENT_ID.to_owned(),
                preset_id: None,
                preset_revision: None,
                preset_snapshot: None,
                provider_id: Some("0190f5fe-7c00-7a00-8000-000000000013".to_owned()),
                model: Some("model_test".to_owned()),
                role: Some("tester".to_owned()),
                capability: None,
                constraints: None,
                description: Some("immutable execution participant".to_owned()),
                system_prompt: None,
                enabled_skills: "[]".to_owned(),
                disabled_builtin_skills: "[]".to_owned(),
                sort_order: 0,
            }],
            &NewAgentExecutionEvent {
                event_type: AgentExecutionEventKind::Created,
                step_id: None,
                attempt_id: None,
                actor: nomifun_common::AgentExecutionActor::user(
                    &installation_owner,
                ),
                payload: serde_json::json!({"status":"paused"}).to_string(),
            },
        )
        .await
        .unwrap();

    assert!(nomifun_common::validate_uuidv7(&row.execution_id).is_ok());

    let response = app
        .clone()
        .oneshot(get_with_token("/api/agent-executions", &token))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body = body_json(response).await;
    let listed = body["data"].as_array().unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0]["execution_id"], row.execution_id);
    assert!(listed[0].get("id").is_none());
    assert_eq!(listed[0]["goal"], "验证统一执行边界");
    assert_eq!(listed[0]["status"], "paused");

    let response = app
        .clone()
        .oneshot(get_with_token(
            &format!("/api/agent-executions/{}", row.execution_id),
            &token,
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let detail = body_json(response).await;
    assert_eq!(
        detail["data"]["execution"]["execution_id"],
        row.execution_id
    );
    assert!(detail["data"]["execution"].get("id").is_none());
    assert_eq!(detail["data"]["participants"].as_array().unwrap().len(), 1);
    assert!(detail["data"]["steps"].as_array().unwrap().is_empty());
    assert!(detail["data"]["attempts"].as_array().unwrap().is_empty());

    let response = app
        .clone()
        .oneshot(delete_with_token(
            &format!(
                "/api/agent-executions/{}?expected_version={}",
                row.execution_id, row.version
            ),
            &token,
            &csrf,
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    let response = app
        .oneshot(get_with_token(
            &format!("/api/agent-executions/{}", row.execution_id),
            &token,
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn collaboration_template_routes_are_owner_scoped_crud() {
    let (mut app, services) = build_app().await;
    seed_test_provider(&services).await;
    let response = app
        .clone()
        .oneshot(get_request("/api/agent-execution-templates"))
        .await
        .unwrap();
    assert!(matches!(
        response.status(),
        StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN
    ));

    let (token, csrf) =
        setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;
    let response = app
        .clone()
        .oneshot(json_with_token(
            "POST",
            "/api/agent-execution-templates",
            serde_json::json!({
                "name": "发布前检查",
                "description": "可复用协作输入",
                "max_parallel": 3,
                "work_dir": "/tmp/project",
                "context": {"ticket":"NOMI-37"},
                "participants": [{
                    "source_agent_id": NOMI_AGENT_ID,
                    "provider_id": "0190f5fe-7c00-7a00-8000-000000000013",
                    "model": "model_test",
                    "role": "reviewer"
                }]
            }),
            &token,
            &csrf,
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::CREATED);
    let created = body_json(response).await;
    let template_id = created["data"]["execution_template_id"]
        .as_str()
        .unwrap()
        .to_owned();
    assert!(created["data"].get("id").is_none());
    assert!(nomifun_common::validate_uuidv7(&template_id).is_ok());
    assert_eq!(created["data"]["participants"].as_array().unwrap().len(), 1);

    let response = app
        .clone()
        .oneshot(get_with_token("/api/agent-execution-templates", &token))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(body_json(response).await["data"].as_array().unwrap().len(), 1);

    let response = app
        .clone()
        .oneshot(json_with_token(
            "PUT",
            &format!("/api/agent-execution-templates/{template_id}"),
            serde_json::json!({
                "expected_version": 0,
                "name": "发布前完整检查"
            }),
            &token,
            &csrf,
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let updated = body_json(response).await;
    assert_eq!(updated["data"]["version"], 1);
    assert_eq!(updated["data"]["name"], "发布前完整检查");

    let response = app
        .clone()
        .oneshot(delete_with_token(
            &format!(
                "/api/agent-execution-templates/{template_id}?expected_version=1"
            ),
            &token,
            &csrf,
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    let response = app
        .oneshot(get_with_token(
            &format!("/api/agent-execution-templates/{template_id}"),
            &token,
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

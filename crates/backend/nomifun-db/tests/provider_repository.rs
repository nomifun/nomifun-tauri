//! Black-box integration tests for IProviderRepository.
//!
//! Tests exercise the public trait interface against an in-memory SQLite database.

use std::sync::Arc;

use nomifun_db::{
    CreateProviderParams, CreateTerminalParams, DbError, IConversationRepository,
    IProviderRepository, ITerminalRepository, SqliteConversationRepository,
    SqliteProviderRepository, SqliteTerminalRepository, UpdateProviderParams,
    init_database_memory,
};
use nomifun_db::models::ConversationRow;

const CALLER_PROVIDER_ID: &str = "0190f5fe-7c00-7a00-8000-000000000010";
const DUPLICATE_PROVIDER_ID: &str = "0190f5fe-7c00-7a00-8000-000000000011";

async fn repo() -> Arc<dyn IProviderRepository> {
    let db = init_database_memory().await.unwrap();
    Arc::new(SqliteProviderRepository::new(db.pool().clone()))
}

async fn insert_provider(
    repository: &SqliteProviderRepository,
    provider_id: &'static str,
    name: &'static str,
) {
    repository
        .create(CreateProviderParams {
            provider_id: Some(provider_id),
            name,
            ..sample_params()
        })
        .await
        .unwrap();
}

fn sample_params() -> CreateProviderParams<'static> {
    CreateProviderParams {
        provider_id: None,
        platform: "anthropic",
        name: "Anthropic",
        base_url: "https://api.anthropic.com",
        api_key_encrypted: "enc_key_data",
        models: r#"["claude-sonnet-4-20250514"]"#,
        enabled: true,
        capabilities: r#"[{"type":"text"}]"#,
        model_context_limits: None,
        model_protocols: None,
        model_descriptions: None,
        model_enabled: None,
        model_health: None,
        bedrock_config: None,
        is_full_url: false,
        sort_order: None,
    }
}

// -- Empty state --

#[tokio::test]
async fn list_returns_empty_when_no_providers() {
    let r = repo().await;
    assert!(r.list().await.unwrap().is_empty());
}

// -- Create --

#[tokio::test]
async fn create_returns_provider_with_generated_id() {
    let r = repo().await;
    let p = r.create(sample_params()).await.unwrap();

    assert!(p.id > 0);
    nomifun_common::validate_uuidv7(&p.provider_id).unwrap();
    assert_eq!(p.platform, "anthropic");
    assert_eq!(p.name, "Anthropic");
    assert_eq!(p.base_url, "https://api.anthropic.com");
    assert!(p.enabled);
    assert!(p.created_at > 0);
}

#[tokio::test]
async fn create_stores_json_fields_as_strings() {
    let r = repo().await;
    let p = r.create(sample_params()).await.unwrap();

    assert_eq!(p.models, r#"["claude-sonnet-4-20250514"]"#);
    assert_eq!(p.capabilities, r#"[{"type":"text"}]"#);
}

#[tokio::test]
async fn create_accepts_canonical_caller_supplied_uuidv7() {
    let r = repo().await;
    let p = r
        .create(CreateProviderParams {
            provider_id: Some(CALLER_PROVIDER_ID),
            ..sample_params()
        })
        .await
        .unwrap();

    assert_eq!(p.provider_id, CALLER_PROVIDER_ID);
    nomifun_common::ProviderId::parse(&p.provider_id).unwrap();
}

#[tokio::test]
async fn create_rejects_invalid_caller_supplied_provider_id() {
    let r = repo().await;
    let err = r
        .create(CreateProviderParams {
            provider_id: Some("my-custom-id"),
            ..sample_params()
        })
        .await
        .unwrap_err();

    assert!(
        matches!(err, DbError::Conflict(ref message) if message.contains("invalid provider_id")),
        "expected invalid provider_id conflict, got: {err:?}"
    );
    assert!(r.list().await.unwrap().is_empty());
}

#[tokio::test]
async fn create_duplicate_canonical_caller_id_returns_conflict() {
    let r = repo().await;
    r.create(CreateProviderParams {
        provider_id: Some(DUPLICATE_PROVIDER_ID),
        ..sample_params()
    })
    .await
    .unwrap();

    let err = r
        .create(CreateProviderParams {
            provider_id: Some(DUPLICATE_PROVIDER_ID),
            ..sample_params()
        })
        .await
        .unwrap_err();

    assert!(
        matches!(err, DbError::Conflict(_)),
        "expected conflict, got: {err:?}"
    );
}

#[tokio::test]
async fn create_with_all_optional_fields() {
    let r = repo().await;
    let p = r
        .create(CreateProviderParams {
            model_context_limits: Some(r#"{"m1":128000}"#),
            model_protocols: Some(r#"{"m1":"openai"}"#),
            model_descriptions: Some(r#"{"m1":"擅长前端"}"#),
            model_enabled: Some(r#"{"m1":true}"#),
            model_health: Some(r#"{"m1":{"status":"healthy"}}"#),
            bedrock_config: Some(r#"{"region":"us-east-1"}"#),
            ..sample_params()
        })
        .await
        .unwrap();

    assert_eq!(p.model_protocols.as_deref(), Some(r#"{"m1":"openai"}"#));
    assert_eq!(p.model_descriptions.as_deref(), Some(r#"{"m1":"擅长前端"}"#));
    assert_eq!(p.model_enabled.as_deref(), Some(r#"{"m1":true}"#));
    assert!(p.model_health.is_some());
    assert!(p.bedrock_config.is_some());
}

// -- Find by ID --

#[tokio::test]
async fn find_by_id_existing_returns_provider() {
    let r = repo().await;
    let created = r.create(sample_params()).await.unwrap();

    let found = r.find_by_id(&created.provider_id).await.unwrap().unwrap();
    assert_eq!(found.id, created.id);
    assert_eq!(found.provider_id, created.provider_id);
    assert_eq!(found.name, "Anthropic");
}

#[tokio::test]
async fn find_by_id_nonexistent_returns_none() {
    let r = repo().await;
    assert!(r.find_by_id("no_such_id").await.unwrap().is_none());
}

// -- List --

#[tokio::test]
async fn list_returns_all_providers_in_creation_order() {
    let r = repo().await;
    let first = r.create(sample_params()).await.unwrap();
    let second = r
        .create(CreateProviderParams {
            platform: "openai",
            name: "OpenAI",
            ..sample_params()
        })
        .await
        .unwrap();

    let all = r.list().await.unwrap();
    assert_eq!(all.len(), 2);
    assert_eq!(all[0].id, first.id);
    assert_eq!(all[1].id, second.id);
}

#[tokio::test]
async fn provider_sort_order_defaults_to_append_order() {
    let r = repo().await;
    let first = r.create(sample_params()).await.unwrap();
    let second = r
        .create(CreateProviderParams {
            platform: "openai",
            name: "OpenAI",
            ..sample_params()
        })
        .await
        .unwrap();

    assert_eq!(first.sort_order, 0);
    assert_eq!(second.sort_order, 1);

    let all = r.list().await.unwrap();
    assert_eq!(all.len(), 2);
    assert_eq!(all[0].id, first.id);
    assert_eq!(all[1].id, second.id);
}

#[tokio::test]
async fn provider_sort_order_controls_list_priority() {
    let r = repo().await;
    let first = r.create(sample_params()).await.unwrap();
    let second = r
        .create(CreateProviderParams {
            platform: "openai",
            name: "OpenAI",
            ..sample_params()
        })
        .await
        .unwrap();

    r.update(
        &first.provider_id,
        UpdateProviderParams {
            sort_order: Some(1),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    r.update(
        &second.provider_id,
        UpdateProviderParams {
            sort_order: Some(0),
            ..Default::default()
        },
    )
    .await
    .unwrap();

    let all = r.list().await.unwrap();
    assert_eq!(all.len(), 2);
    assert_eq!(all[0].id, second.id);
    assert_eq!(all[0].sort_order, 0);
    assert_eq!(all[1].id, first.id);
    assert_eq!(all[1].sort_order, 1);
}

// -- Update --

#[tokio::test]
async fn update_partial_fields_preserves_others() {
    let r = repo().await;
    let created = r.create(sample_params()).await.unwrap();

    let updated = r
        .update(
            &created.provider_id,
            UpdateProviderParams {
                name: Some("New Name"),
                enabled: Some(false),
                ..Default::default()
            },
        )
        .await
        .unwrap();

    assert_eq!(updated.name, "New Name");
    assert!(!updated.enabled);
    assert_eq!(updated.platform, "anthropic");
    assert_eq!(updated.base_url, "https://api.anthropic.com");
}

#[tokio::test]
async fn update_api_key_changes_encrypted_value() {
    let r = repo().await;
    let created = r.create(sample_params()).await.unwrap();

    let updated = r
        .update(
            &created.provider_id,
            UpdateProviderParams {
                api_key_encrypted: Some("new_encrypted"),
                ..Default::default()
            },
        )
        .await
        .unwrap();

    assert_eq!(updated.api_key_encrypted, "new_encrypted");
}

#[tokio::test]
async fn update_optional_fields_can_be_set_and_cleared() {
    let r = repo().await;
    let created = r.create(sample_params()).await.unwrap();
    assert!(created.bedrock_config.is_none());

    // Set
    let with_config = r
        .update(
            &created.provider_id,
            UpdateProviderParams {
                bedrock_config: Some(Some(r#"{"region":"eu-west-1"}"#)),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert!(with_config.bedrock_config.is_some());

    // Clear
    let cleared = r
        .update(
            &created.provider_id,
            UpdateProviderParams {
                bedrock_config: Some(None),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert!(cleared.bedrock_config.is_none());
}

#[tokio::test]
async fn update_nonexistent_returns_not_found() {
    let r = repo().await;
    let err = r
        .update("nonexistent", UpdateProviderParams::default())
        .await
        .unwrap_err();
    assert!(matches!(err, DbError::NotFound(_)), "expected NotFound, got: {err:?}");
}

#[tokio::test]
async fn update_advances_updated_at() {
    let r = repo().await;
    let created = r.create(sample_params()).await.unwrap();

    let updated = r
        .update(
            &created.provider_id,
            UpdateProviderParams {
                name: Some("Changed"),
                ..Default::default()
            },
        )
        .await
        .unwrap();

    assert!(updated.updated_at >= created.updated_at);
    assert_eq!(updated.created_at, created.created_at);
}

// -- Delete --

#[tokio::test]
async fn delete_removes_provider() {
    let r = repo().await;
    let created = r.create(sample_params()).await.unwrap();

    r.delete(&created.provider_id).await.unwrap();
    assert!(r.find_by_id(&created.provider_id).await.unwrap().is_none());
}

#[tokio::test]
async fn delete_nonexistent_returns_not_found() {
    let r = repo().await;
    let err = r.delete("nonexistent").await.unwrap_err();
    assert!(matches!(err, DbError::NotFound(_)), "expected NotFound, got: {err:?}");
}

#[tokio::test]
async fn delete_does_not_affect_other_providers() {
    let r = repo().await;
    let p1 = r.create(sample_params()).await.unwrap();
    let p2 = r
        .create(CreateProviderParams {
            name: "Other",
            ..sample_params()
        })
        .await
        .unwrap();

    r.delete(&p1.provider_id).await.unwrap();

    let all = r.list().await.unwrap();
    assert_eq!(all.len(), 1);
    assert_eq!(all[0].id, p2.id);
}

#[tokio::test]
async fn delete_clears_all_idmm_session_bypass_references_but_preserves_watch_config() {
    const DELETED_PROVIDER: &str = "0190f5fe-7c00-7a00-8000-000000000020";
    const RETAINED_PROVIDER: &str = "0190f5fe-7c00-7a00-8000-000000000021";

    let db = init_database_memory().await.unwrap();
    let provider_repo = SqliteProviderRepository::new(db.pool().clone());
    insert_provider(&provider_repo, DELETED_PROVIDER, "deleted").await;
    insert_provider(&provider_repo, RETAINED_PROVIDER, "retained").await;

    let owner = nomifun_db::installation_owner_id(db.pool()).await.unwrap();
    let conversation_repo = SqliteConversationRepository::new(db.pool().clone());
    let conversation_id = nomifun_common::ConversationId::new().into_string();
    conversation_repo
        .create(&ConversationRow {
            id: 0,
            conversation_id: conversation_id.clone(),
            user_id: owner.clone(),
            name: "IDMM cleanup".to_owned(),
            r#type: "nomi".to_owned(),
            extra: r#"{"workspace":"/tmp/idmm"}"#.to_owned(),
            delegation_policy: "automatic".to_owned(),
            execution_model_pool: None,
            decision_policy: "automatic".to_owned(),
            execution_template_id: None,
            model: None,
            status: Some("pending".to_owned()),
            source: Some("nomifun".to_owned()),
            channel_chat_id: None,
            pinned: false,
            pinned_at: None,
            cron_job_id: None,
            preset_id: None,
            preset_revision: None,
            preset_snapshot: None,
            created_at: 1,
            updated_at: 1,
        })
        .await
        .unwrap();
    let conversation_idmm = serde_json::json!({
        "fault_watch": {
            "enabled": true,
            "scan_interval_secs": 23,
            "bypass_model": {
                "provider_id": DELETED_PROVIDER,
                "model": "fault-deleted"
            }
        },
        "decision_watch": {
            "enabled": true,
            "scan_interval_secs": 41,
            "bypass_model": {
                "provider_id": RETAINED_PROVIDER,
                "model": "decision-retained"
            }
        }
    })
    .to_string();
    conversation_repo
        .update_idmm(&conversation_id, Some(&conversation_idmm))
        .await
        .unwrap();

    let terminal_repo = SqliteTerminalRepository::new(db.pool().clone());
    let terminal = terminal_repo
        .create(&CreateTerminalParams {
            id: nomifun_common::TerminalId::new(),
            name: "IDMM cleanup".to_owned(),
            cwd: "/tmp".to_owned(),
            command: "$SHELL".to_owned(),
            args: "[]".to_owned(),
            env: None,
            backend: None,
            mode: None,
            cols: 80,
            rows: 24,
            user_id: nomifun_common::UserId::parse(owner).unwrap(),
        })
        .await
        .unwrap();
    let terminal_idmm = serde_json::json!({
        "fault_watch": {
            "enabled": true,
            "max_retries": 8,
            "bypass_model": {
                "provider_id": RETAINED_PROVIDER,
                "model": "fault-retained"
            }
        },
        "decision_watch": {
            "enabled": true,
            "max_retries": 5,
            "bypass_model": {
                "provider_id": DELETED_PROVIDER,
                "model": "decision-deleted"
            }
        }
    })
    .to_string();
    terminal_repo
        .update_idmm(terminal.terminal_id.as_str(), Some(&terminal_idmm))
        .await
        .unwrap();

    provider_repo.delete(DELETED_PROVIDER).await.unwrap();

    let conversation = conversation_repo.get(&conversation_id).await.unwrap().unwrap();
    let extra: serde_json::Value = serde_json::from_str(&conversation.extra).unwrap();
    assert!(extra["idmm"]["fault_watch"].get("bypass_model").is_none());
    assert_eq!(extra["idmm"]["fault_watch"]["enabled"], true);
    assert_eq!(extra["idmm"]["fault_watch"]["scan_interval_secs"], 23);
    assert_eq!(
        extra["idmm"]["decision_watch"]["bypass_model"]["provider_id"],
        RETAINED_PROVIDER
    );
    assert_eq!(extra["workspace"], "/tmp/idmm");

    let terminal_idmm: serde_json::Value = serde_json::from_str(
        &terminal_repo
            .get_idmm(terminal.terminal_id.as_str())
            .await
            .unwrap()
            .unwrap(),
    )
    .unwrap();
    assert_eq!(
        terminal_idmm["fault_watch"]["bypass_model"]["provider_id"],
        RETAINED_PROVIDER
    );
    assert!(
        terminal_idmm["decision_watch"]
            .get("bypass_model")
            .is_none()
    );
    assert_eq!(terminal_idmm["decision_watch"]["enabled"], true);
    assert_eq!(terminal_idmm["decision_watch"]["max_retries"], 5);
}

#[tokio::test]
async fn delete_fails_closed_on_malformed_cron_provider_json() {
    let db = init_database_memory().await.unwrap();
    let repository = SqliteProviderRepository::new(db.pool().clone());
    insert_provider(&repository, CALLER_PROVIDER_ID, "malformed guard").await;
    let owner = nomifun_db::installation_owner_id(db.pool()).await.unwrap();

    sqlx::query("PRAGMA ignore_check_constraints = ON")
        .execute(db.pool())
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO cron_jobs (\
            cron_job_id, user_id, name, schedule_kind, schedule_value, payload_message, \
            agent_type, agent_config, created_by, created_at, updated_at\
         ) VALUES (?, ?, 'malformed provider binding', 'every', '60000', '', \
                   'nomi', '{', 'user', 1, 1)",
    )
    .bind(nomifun_common::CronJobId::new().as_str())
    .bind(owner)
    .execute(db.pool())
    .await
    .unwrap();
    sqlx::query("PRAGMA ignore_check_constraints = OFF")
        .execute(db.pool())
        .await
        .unwrap();

    let error = repository.delete(CALLER_PROVIDER_ID).await.unwrap_err();
    assert!(matches!(error, DbError::Conflict(_)));
    assert!(
        repository
            .find_by_id(CALLER_PROVIDER_ID)
            .await
            .unwrap()
            .is_some()
    );
    let cron_count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM cron_jobs WHERE agent_config = '{'")
            .fetch_one(db.pool())
            .await
            .unwrap();
    assert_eq!(cron_count, 1, "failed deletion must not mutate the cron row");
}

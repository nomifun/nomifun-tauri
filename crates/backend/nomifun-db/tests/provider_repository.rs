//! Black-box integration tests for IProviderRepository.
//!
//! Tests exercise the public trait interface against an in-memory SQLite database.

use std::sync::Arc;

use nomifun_db::{
    CreateProviderParams, DbError, IProviderRepository, SqliteProviderRepository, UpdateProviderParams,
    init_database_memory,
};

async fn repo() -> Arc<dyn IProviderRepository> {
    let db = init_database_memory().await.unwrap();
    Arc::new(SqliteProviderRepository::new(db.pool().clone()))
}

fn sample_params() -> CreateProviderParams<'static> {
    CreateProviderParams {
        id: None,
        platform: "anthropic",
        name: "Anthropic",
        base_url: "https://api.anthropic.com",
        api_key_encrypted: "enc_key_data",
        models: r#"["claude-sonnet-4-20250514"]"#,
        enabled: true,
        capabilities: r#"[{"type":"text"}]"#,
        context_limit: Some(200000),
        model_protocols: None,
        model_descriptions: None,
        model_enabled: None,
        model_health: None,
        bedrock_config: None,
        is_full_url: false,
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

    assert!(!p.id.is_empty());
    assert_eq!(p.platform, "anthropic");
    assert_eq!(p.name, "Anthropic");
    assert_eq!(p.base_url, "https://api.anthropic.com");
    assert!(p.enabled);
    assert_eq!(p.context_limit, Some(200000));
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
async fn create_with_all_optional_fields() {
    let r = repo().await;
    let p = r
        .create(CreateProviderParams {
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

    let found = r.find_by_id(&created.id).await.unwrap().unwrap();
    assert_eq!(found.id, created.id);
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

// -- Update --

#[tokio::test]
async fn update_partial_fields_preserves_others() {
    let r = repo().await;
    let created = r.create(sample_params()).await.unwrap();

    let updated = r
        .update(
            &created.id,
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
            &created.id,
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
            &created.id,
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
            &created.id,
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
            &created.id,
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

    r.delete(&created.id).await.unwrap();
    assert!(r.find_by_id(&created.id).await.unwrap().is_none());
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

    r.delete(&p1.id).await.unwrap();

    let all = r.list().await.unwrap();
    assert_eq!(all.len(), 1);
    assert_eq!(all[0].id, p2.id);
}

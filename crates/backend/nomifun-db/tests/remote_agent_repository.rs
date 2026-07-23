//! Black-box integration tests for `IRemoteAgentRepository`.
//!
//! Tests exercise the repository trait interface without knowledge of
//! the underlying SQLite implementation details.

use std::sync::Arc;

use nomifun_common::RemoteAgentId;
use nomifun_db::{
    CreateRemoteAgentParams, DbError, IRemoteAgentRepository, SqliteRemoteAgentRepository,
    UpdateRemoteAgentParams, init_database_memory,
};

fn missing_id() -> RemoteAgentId {
    RemoteAgentId::parse("0190f5fe-7c00-7a00-8000-000000000999").unwrap()
}

fn business_id(row: &nomifun_db::models::RemoteAgentRow) -> RemoteAgentId {
    RemoteAgentId::parse(&row.remote_agent_id).expect("repository returned canonical remote_agent_id")
}

async fn repo() -> (Arc<dyn IRemoteAgentRepository>, nomifun_db::Database) {
    let db = init_database_memory().await.unwrap();
    let r = Arc::new(SqliteRemoteAgentRepository::new(db.pool().clone()));
    (r as Arc<dyn IRemoteAgentRepository>, db)
}

fn bearer_params() -> CreateRemoteAgentParams<'static> {
    CreateRemoteAgentParams {
        name: "Remote Server",
        protocol: "acp",
        url: "wss://remote.example.com",
        auth_type: "bearer",
        auth_token: Some("encrypted_bearer_token"),
        allow_insecure: false,
        avatar: None,
        description: Some("Production agent"),
        device_id: None,
        device_public_key: None,
        device_private_key: None,
        device_token: None,
    }
}

fn openclaw_params() -> CreateRemoteAgentParams<'static> {
    CreateRemoteAgentParams {
        name: "OpenClaw Agent",
        protocol: "openClaw",
        url: "wss://openclaw.example.com",
        auth_type: "none",
        auth_token: None,
        allow_insecure: false,
        avatar: Some("https://example.com/avatar.png"),
        description: None,
        device_id: Some("dev-abc-123"),
        device_public_key: Some("enc_ed25519_pub"),
        device_private_key: Some("enc_ed25519_priv"),
        device_token: Some("enc_device_tok"),
    }
}

// -- 1.1 Create Remote Agent --

#[tokio::test]
async fn create_bearer_agent_returns_complete_object() {
    let (r, _db) = repo().await;
    let agent = r.create(bearer_params()).await.unwrap();

    assert!(agent.id > 0);
    assert!(RemoteAgentId::parse(&agent.remote_agent_id).is_ok());
    assert_eq!(agent.name, "Remote Server");
    assert_eq!(agent.protocol, "acp");
    assert_eq!(agent.url, "wss://remote.example.com");
    assert_eq!(agent.auth_type, "bearer");
    assert_eq!(agent.auth_token.as_deref(), Some("encrypted_bearer_token"));
    assert!(!agent.allow_insecure);
    assert_eq!(agent.status, "unknown");
    assert!(agent.last_connected_at.is_none());
    assert!(agent.created_at > 0);
    assert!(agent.updated_at > 0);
}

#[tokio::test]
async fn create_openclaw_agent_includes_device_fields() {
    let (r, _db) = repo().await;
    let agent = r.create(openclaw_params()).await.unwrap();

    assert_eq!(agent.protocol, "openClaw");
    assert_eq!(agent.device_id.as_deref(), Some("dev-abc-123"));
    assert_eq!(agent.device_public_key.as_deref(), Some("enc_ed25519_pub"));
    assert_eq!(agent.device_private_key.as_deref(), Some("enc_ed25519_priv"));
    assert_eq!(agent.device_token.as_deref(), Some("enc_device_tok"));
}

// -- 1.2 List Remote Agents --

#[tokio::test]
async fn list_empty_returns_empty_vec() {
    let (r, _db) = repo().await;
    let agents = r.list().await.unwrap();
    assert!(agents.is_empty());
}

#[tokio::test]
async fn list_returns_all_agents_ordered() {
    let (r, _db) = repo().await;
    let a1 = r.create(bearer_params()).await.unwrap();
    let a2 = r.create(openclaw_params()).await.unwrap();

    let all = r.list().await.unwrap();
    assert_eq!(all.len(), 2);
    assert_eq!(all[0].id, a1.id);
    assert_eq!(all[1].id, a2.id);
}

// -- 1.3 Get Single Remote Agent --

#[tokio::test]
async fn find_by_id_returns_full_record() {
    let (r, _db) = repo().await;
    let created = r.create(bearer_params()).await.unwrap();

    let found = r.find_by_id(&business_id(&created)).await.unwrap().unwrap();
    assert_eq!(found.id, created.id);
    assert_eq!(found.name, "Remote Server");
    assert_eq!(found.auth_token.as_deref(), Some("encrypted_bearer_token"));
    assert_eq!(found.description.as_deref(), Some("Production agent"));
}

#[tokio::test]
async fn find_by_id_nonexistent_returns_none() {
    let (r, _db) = repo().await;
    let result = r.find_by_id(&missing_id()).await.unwrap();
    assert!(result.is_none());
}

// -- 1.4 Update Remote Agent --

#[tokio::test]
async fn update_name_only_preserves_other_fields() {
    let (r, _db) = repo().await;
    let created = r.create(bearer_params()).await.unwrap();

    let updated = r
        .update(
            &business_id(&created),
            UpdateRemoteAgentParams {
                name: Some("New Name"),
                ..Default::default()
            },
        )
        .await
        .unwrap();

    assert_eq!(updated.name, "New Name");
    assert_eq!(updated.protocol, created.protocol);
    assert_eq!(updated.url, created.url);
    assert_eq!(updated.auth_type, created.auth_type);
    assert_eq!(updated.auth_token, created.auth_token);
    assert_eq!(updated.allow_insecure, created.allow_insecure);
}

#[tokio::test]
async fn update_multiple_fields() {
    let (r, _db) = repo().await;
    let created = r.create(bearer_params()).await.unwrap();

    let updated = r
        .update(
            &business_id(&created),
            UpdateRemoteAgentParams {
                name: Some("Updated"),
                url: Some("wss://new-url.example.com"),
                auth_token: Some(Some("new_encrypted_token")),
                ..Default::default()
            },
        )
        .await
        .unwrap();

    assert_eq!(updated.name, "Updated");
    assert_eq!(updated.url, "wss://new-url.example.com");
    assert_eq!(updated.auth_token.as_deref(), Some("new_encrypted_token"));
}

#[tokio::test]
async fn update_nonexistent_returns_not_found() {
    let (r, _db) = repo().await;
    let err = r
        .update(&missing_id(), UpdateRemoteAgentParams::default())
        .await
        .unwrap_err();
    assert!(matches!(err, DbError::NotFound(_)));
}

#[tokio::test]
async fn update_can_clear_optional_fields() {
    let (r, _db) = repo().await;
    let created = r.create(bearer_params()).await.unwrap();
    assert!(created.description.is_some());
    assert!(created.auth_token.is_some());

    let updated = r
        .update(
            &business_id(&created),
            UpdateRemoteAgentParams {
                description: Some(None),
                auth_token: Some(None),
                ..Default::default()
            },
        )
        .await
        .unwrap();

    assert!(updated.description.is_none());
    assert!(updated.auth_token.is_none());
}

#[tokio::test]
async fn update_persists_to_database() {
    let (r, _db) = repo().await;
    let created = r.create(bearer_params()).await.unwrap();

    r.update(
        &business_id(&created),
        UpdateRemoteAgentParams {
            name: Some("Persisted Name"),
            ..Default::default()
        },
    )
    .await
    .unwrap();

    let found = r.find_by_id(&business_id(&created)).await.unwrap().unwrap();
    assert_eq!(found.name, "Persisted Name");
}

// -- 1.5 Delete Remote Agent --

#[tokio::test]
async fn delete_existing_removes_record() {
    let (r, _db) = repo().await;
    let created = r.create(bearer_params()).await.unwrap();

    r.delete(&business_id(&created)).await.unwrap();

    let found = r.find_by_id(&business_id(&created)).await.unwrap();
    assert!(found.is_none());
}

#[tokio::test]
async fn delete_nonexistent_returns_not_found() {
    let (r, _db) = repo().await;
    let err = r.delete(&missing_id()).await.unwrap_err();
    assert!(matches!(err, DbError::NotFound(_)));
}

#[tokio::test]
async fn delete_one_does_not_affect_others() {
    let (r, _db) = repo().await;
    let a1 = r.create(bearer_params()).await.unwrap();
    let a2 = r.create(openclaw_params()).await.unwrap();

    r.delete(&business_id(&a1)).await.unwrap();

    let remaining = r.list().await.unwrap();
    assert_eq!(remaining.len(), 1);
    assert_eq!(remaining[0].id, a2.id);
}

// -- Status updates --

#[tokio::test]
async fn update_status_to_connected_with_timestamp() {
    let (r, _db) = repo().await;
    let created = r.create(bearer_params()).await.unwrap();

    let ts = nomifun_common::now_ms();
    r.update_status(&business_id(&created), "connected", Some(ts))
        .await
        .unwrap();

    let found = r.find_by_id(&business_id(&created)).await.unwrap().unwrap();
    assert_eq!(found.status, "connected");
    assert_eq!(found.last_connected_at, Some(ts));
}

#[tokio::test]
async fn update_status_to_error_without_timestamp() {
    let (r, _db) = repo().await;
    let created = r.create(bearer_params()).await.unwrap();

    r.update_status(&business_id(&created), "error", None)
        .await
        .unwrap();

    let found = r.find_by_id(&business_id(&created)).await.unwrap().unwrap();
    assert_eq!(found.status, "error");
    assert!(found.last_connected_at.is_none());
}

#[tokio::test]
async fn update_status_nonexistent_returns_not_found() {
    let (r, _db) = repo().await;
    let err = r
        .update_status(&missing_id(), "connected", None)
        .await
        .unwrap_err();
    assert!(matches!(err, DbError::NotFound(_)));
}

// -- Full CRUD lifecycle --

#[tokio::test]
async fn full_crud_lifecycle() {
    let (r, _db) = repo().await;

    // Create
    let created = r.create(bearer_params()).await.unwrap();
    assert_eq!(created.name, "Remote Server");

    // Read
    let found = r.find_by_id(&business_id(&created)).await.unwrap().unwrap();
    assert_eq!(found.id, created.id);

    // Update
    let updated = r
        .update(
            &business_id(&created),
            UpdateRemoteAgentParams {
                name: Some("Renamed Server"),
                description: Some(Some("Updated desc")),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(updated.name, "Renamed Server");
    assert_eq!(updated.description.as_deref(), Some("Updated desc"));

    // Update status
    r.update_status(
        &business_id(&created),
        "connected",
        Some(nomifun_common::now_ms()),
    )
        .await
        .unwrap();
    let after_status = r.find_by_id(&business_id(&created)).await.unwrap().unwrap();
    assert_eq!(after_status.status, "connected");

    // Delete
    r.delete(&business_id(&created)).await.unwrap();
    assert!(
        r.find_by_id(&business_id(&created))
            .await
            .unwrap()
            .is_none()
    );

    // List should be empty
    assert!(r.list().await.unwrap().is_empty());
}

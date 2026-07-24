use std::path::PathBuf;
use std::sync::Arc;

use nomifun_ai_agent::AcpSessionSyncService;
use nomifun_ai_agent::AcpSkillManager;
use nomifun_ai_agent::factory::{AgentFactoryDeps, build_agent_factory};
use nomifun_ai_agent::registry::AgentRegistry;
use nomifun_ai_agent::types::AgentRuntimeBuildOptions;
use nomifun_common::{AgentType, ConversationId, ProviderWithModel, encrypt_string};
use nomifun_db::{
    CreateProviderParams, IAcpSessionRepository, IProviderRepository, SqliteAcpSessionRepository,
    SqliteAgentMetadataRepository, SqliteProviderRepository, SqliteRemoteAgentRepository, init_database_memory,
};

const TEST_OWNER_ID: &str = "0190f5fe-7c00-7a00-8000-000000000001";
const PROVIDER_ID_1: &str = "0190f5fe-7c00-7a00-8000-000000000001";
const PROVIDER_ID_2: &str = "0190f5fe-7c00-7a00-8000-000000000002";
const MISSING_PROVIDER_ID: &str = "0190f5fe-7c00-7a00-8000-000000000099";

fn test_encryption_key() -> [u8; 32] {
    [0xABu8; 32]
}

async fn setup() -> (
    Arc<dyn IProviderRepository>,
    Arc<SqliteRemoteAgentRepository>,
    Arc<AgentRegistry>,
    Arc<AcpSessionSyncService>,
) {
    let db = init_database_memory().await.unwrap();
    let pool = db.pool().clone();
    let provider_repo: Arc<dyn IProviderRepository> = Arc::new(SqliteProviderRepository::new(pool.clone()));
    let remote_agent_repo = Arc::new(SqliteRemoteAgentRepository::new(pool.clone()));
    let metadata_repo = Arc::new(SqliteAgentMetadataRepository::new(pool.clone()));
    let registry = AgentRegistry::new(metadata_repo);
    registry.hydrate().await.unwrap();
    let session_repo: Arc<dyn IAcpSessionRepository> = Arc::new(SqliteAcpSessionRepository::new(pool));
    let acp_agent_service = AcpSessionSyncService::new(session_repo);
    (provider_repo, remote_agent_repo, registry, acp_agent_service)
}

async fn insert_test_provider(repo: &dyn IProviderRepository, id: &str, platform: &str) {
    let key = test_encryption_key();
    let encrypted_api_key = encrypt_string("sk-test-key-12345", &key).unwrap();
    repo.create(CreateProviderParams {
        provider_id: Some(id),
        platform,
        name: "Test Provider",
        base_url: "https://api.example.com/v1",
        api_key_encrypted: &encrypted_api_key,
        models: r#"["gpt-4o","gpt-5.4"]"#,
        enabled: true,
        capabilities: "[]",
        model_context_limits: None,
        model_protocols: None,
        model_descriptions: None,
        model_enabled: None,
        model_health: None,
        bedrock_config: None,
        is_full_url: false,
        sort_order: None,
    })
    .await
    .unwrap();
}

fn make_factory(
    provider_repo: Arc<dyn IProviderRepository>,
    remote_agent_repo: Arc<SqliteRemoteAgentRepository>,
    agent_registry: Arc<AgentRegistry>,
    acp_agent_service: Arc<AcpSessionSyncService>,
) -> nomifun_ai_agent::runtime_registry::AgentRuntimeFactory {
    let tmp = tempfile::TempDir::new().unwrap();
    let skill_paths = Arc::new(nomifun_extension::resolve_skill_paths(tmp.path(), tmp.path()));
    build_agent_factory(AgentFactoryDeps {
        authoritative_user_id: Arc::from(TEST_OWNER_ID),
        cron_sink_factory: None,
        gateway_mcp_config: None,
        open_mcp_config: None,
        computer_mcp_config: None,
        browser_mcp_config: None,
        client_prefs: None,
        settings_repo: None,
        companion_prompt: None,
        public_agent_provider: None,
        companion_skill_sink: None,
        skill_manager: AcpSkillManager::new(skill_paths),
        remote_agent_repo,
        provider_repo,
        encryption_key: test_encryption_key(),
        agent_registry,
        acp_agent_service,
        data_dir: PathBuf::from("/tmp/nomi-test"),
        work_dir: PathBuf::from("/tmp/nomi-test"),
        backend_binary_path: Arc::new(PathBuf::from("/tmp/nomi-test/nomicore")),
        requirement_mcp_config: None,
        knowledge_mcp_config: None,
        mcp_server_repo: None,
        requirement_sink: None,
        companion_sink: None,
        knowledge_retrieval: None,
        knowledge_writeback: None,
    })
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn nomi_factory_returns_unavailable_when_no_providers_configured() {
    // With NO providers in the DB, a conversation bound to any provider id has
    // nothing to fall back to → ProviderUnavailable (the friendly terminal error,
    // surfaced to the user as "no usable model" rather than a raw provider id).
    let (provider_repo, remote_agent_repo, agent_registry, acp_agent_service) = setup().await;
    let factory = make_factory(provider_repo, remote_agent_repo, agent_registry, acp_agent_service);

    let options = AgentRuntimeBuildOptions {
        user_id: TEST_OWNER_ID.into(),
        agent_type: AgentType::Nomi,
        workspace: "/tmp/test-workspace".into(),
        model: Some(ProviderWithModel {
            provider_id: MISSING_PROVIDER_ID.into(),
            model: "gpt-4o".into(),
            use_model: None,
        }),
        conversation_id: ConversationId::new().into_string(),
        delegation_policy: Default::default(),
        conversation_created_at: None,
        workspace_binding_lease: None,
        extra: serde_json::json!({}),
    };

    let result = factory(options).await;
    match result {
        Ok(_) => panic!("Expected ProviderUnavailable error when no providers configured, got Ok"),
        Err(e) => {
            let err_msg = e.to_string();
            assert!(
                err_msg.contains("No usable model provider"),
                "Expected ProviderUnavailable error, got: {err_msg}"
            );
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn nomi_factory_falls_back_to_first_enabled_when_bound_provider_missing() {
    // A conversation bound to a DELETED provider must NOT hard-fail while an
    // enabled provider still exists — it falls back to the first enabled model
    // instead of erroring with "Provider '<id>' not found".
    let (provider_repo, remote_agent_repo, agent_registry, acp_agent_service) = setup().await;
    insert_test_provider(&*provider_repo, PROVIDER_ID_1, "openai").await;
    let factory = make_factory(provider_repo, remote_agent_repo, agent_registry, acp_agent_service);

    let options = AgentRuntimeBuildOptions {
        user_id: TEST_OWNER_ID.into(),
        agent_type: AgentType::Nomi,
        workspace: "/tmp/test-workspace".into(),
        model: Some(ProviderWithModel {
            provider_id: MISSING_PROVIDER_ID.into(),
            model: "gpt-4o".into(),
            use_model: None,
        }),
        conversation_id: ConversationId::new().into_string(),
        delegation_policy: Default::default(),
        conversation_created_at: None,
        workspace_binding_lease: None,
        extra: serde_json::json!({}),
    };

    let result = factory(options).await;
    assert!(result.is_ok(), "Expected fallback Ok, got: {:?}", result.err());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn nomi_factory_resolves_provider_from_db() {
    let (provider_repo, remote_agent_repo, agent_registry, acp_agent_service) = setup().await;
    insert_test_provider(&*provider_repo, PROVIDER_ID_1, "openai").await;
    let factory = make_factory(provider_repo, remote_agent_repo, agent_registry, acp_agent_service);

    let options = AgentRuntimeBuildOptions {
        user_id: TEST_OWNER_ID.into(),
        agent_type: AgentType::Nomi,
        workspace: "/tmp/test-workspace".into(),
        model: Some(ProviderWithModel {
            provider_id: PROVIDER_ID_1.into(),
            model: "gpt-4o".into(),
            use_model: None,
        }),
        conversation_id: ConversationId::new().into_string(),
        delegation_policy: Default::default(),
        conversation_created_at: None,
        workspace_binding_lease: None,
        extra: serde_json::json!({ "max_tokens": 2048 }),
    };

    let result = factory(options).await;
    assert!(result.is_ok(), "Expected Ok, got: {:?}", result.err());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn nomi_factory_respects_use_model_override() {
    let (provider_repo, remote_agent_repo, agent_registry, acp_agent_service) = setup().await;
    insert_test_provider(&*provider_repo, PROVIDER_ID_2, "openai").await;
    let factory = make_factory(provider_repo, remote_agent_repo, agent_registry, acp_agent_service);

    let options = AgentRuntimeBuildOptions {
        user_id: TEST_OWNER_ID.into(),
        agent_type: AgentType::Nomi,
        workspace: "/tmp/test-workspace".into(),
        model: Some(ProviderWithModel {
            provider_id: PROVIDER_ID_2.into(),
            model: "gpt-4o".into(),
            use_model: Some("gpt-5.4".into()),
        }),
        conversation_id: ConversationId::new().into_string(),
        delegation_policy: Default::default(),
        conversation_created_at: None,
        workspace_binding_lease: None,
        extra: serde_json::json!({}),
    };

    let result = factory(options).await;
    assert!(result.is_ok(), "Expected Ok, got: {:?}", result.err());
}

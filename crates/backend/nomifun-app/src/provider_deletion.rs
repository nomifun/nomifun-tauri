//! App-layer aggregation of every subsystem's provider-in-use scan.
//!
//! `nomifun-app` is the only layer that sees the companion, public-agent, IDMM
//! and Agent Execution subsystems at once, so the cross-subsystem
//! [`ProviderDeletionCoordinator`](nomifun_system::provider_deletion::ProviderDeletionCoordinator)
//! is implemented here and injected into `ProviderService` (see
//! `router::state::build_system_state`). Deletion then refuses an in-use provider
//! (409 `PROVIDER_IN_USE`). The provider repository owns the deletion-time
//! logical-reference guards and soft-reference cleanup in one transaction; this
//! coordinator exists only for friendly, labeled product errors.

use std::sync::Arc;

use nomifun_common::{
    AppError, ProviderId, ProviderLifecycleBarrier, ProviderUsage, ProviderUsageFeature,
};
use nomifun_db::{
    IAgentExecutionRepository, IAgentExecutionTemplateRepository,
    IClientPreferenceRepository, IConversationRepository,
};
use nomifun_idmm::sidecar::PREF_BACKUP_PROVIDER;
use nomifun_system::provider_deletion::ProviderDeletionCoordinator;

/// Aggregates every subsystem's provider-in-use scan behind the single
/// `ProviderDeletionCoordinator` hook `ProviderService::delete` calls.
pub struct AppProviderDeletionCoordinator {
    pub provider_lifecycle: Arc<ProviderLifecycleBarrier>,
    pub companion: Arc<nomifun_companion::CompanionService>,
    pub public_agent: Arc<nomifun_public_agent::PublicAgentService>,
    pub workshop: Arc<nomifun_workshop::WorkshopService>,
    pub client_prefs: Arc<dyn IClientPreferenceRepository>,
    pub execution_repo: Arc<dyn IAgentExecutionRepository>,
    pub execution_template_repo: Arc<dyn IAgentExecutionTemplateRepository>,
    pub conversation_repo: Arc<dyn IConversationRepository>,
}

#[async_trait::async_trait]
impl ProviderDeletionCoordinator for AppProviderDeletionCoordinator {
    fn provider_lifecycle_barrier(&self) -> Option<Arc<ProviderLifecycleBarrier>> {
        Some(self.provider_lifecycle.clone())
    }

    async fn cleanup_soft_references(&self, provider_id: &str) -> Result<(), AppError> {
        self.workshop
            .clear_provider_references_under_lifecycle_write_guard(provider_id)
            .await
    }

    async fn usages(&self, provider_id: &str) -> Result<Vec<ProviderUsage>, AppError> {
        ProviderId::parse(provider_id)
            .map_err(|error| AppError::BadRequest(format!("invalid provider_id: {error}")))?;

        let mut out = Vec::new();
        out.extend(self.companion.providers_in_use(provider_id).await);
        out.extend(self.public_agent.providers_in_use(provider_id).await);

        // 智能决策 (smart decision): the global backup model is a hard binding
        // surfaced here as a friendly product error. Per-session watch bypass
        // models are soft references: the provider repository clears matching
        // `bypass_model` objects from Conversation and terminal JSON in the same
        // transaction that deletes the Provider.
        let rows = self
            .client_prefs
            .get_by_keys(&[PREF_BACKUP_PROVIDER])
            .await
            .map_err(|e| AppError::Internal(format!("read idmm backup pref: {e}")))?;
        if rows
            .iter()
            .any(|r| r.key == PREF_BACKUP_PROVIDER && r.value == provider_id)
        {
            out.push(ProviderUsage {
                feature: ProviderUsageFeature::SmartDecision,
                label: "智能决策·备份模型".into(),
                target_id: None,
            });
        }

        // The top-level Conversation model is its current lead and therefore a
        // hard provider binding. Collaborator-only pool entries remain soft
        // references and are removed only after this guard passes.
        let conversations = self
            .conversation_repo
            .list_conversations_using_model_provider(provider_id)
            .await
            .map_err(|e| AppError::Internal(format!("scan Conversation models: {e}")))?;
        for (id, name) in conversations {
            out.push(ProviderUsage {
                feature: ProviderUsageFeature::Conversation,
                label: name,
                target_id: Some(id.to_string()),
            });
        }

        // Current Agent Execution participants remain live provider bindings
        // even after completed/failed settlement: retry and adopt can reopen
        // those aggregates. Only cancelled or tombstoned executions are
        // permanently inert and therefore stop blocking provider deletion.
        let executions = self
            .execution_repo
            .list_reopenable_provider_usages(provider_id)
            .await
            .map_err(|e| AppError::Internal(format!("scan Agent Executions: {e}")))?;
        for (id, goal) in executions {
            out.push(ProviderUsage {
                feature: ProviderUsageFeature::AgentExecution,
                label: goal,
                target_id: Some(id),
            });
        }
        let templates = self
            .execution_template_repo
            .list_templates_using_provider(provider_id)
            .await
            .map_err(|e| AppError::Internal(format!("scan Agent Execution templates: {e}")))?;
        for (id, name) in templates {
            out.push(ProviderUsage {
                feature: ProviderUsageFeature::AgentExecution,
                label: name,
                target_id: Some(id),
            });
        }
        Ok(out)
    }

}

#[cfg(test)]
mod tests {
    use super::*;
    use nomifun_common::{
        AdaptationPolicy, AgentExecutionEventKind, AgentExecutionStatus, DecisionPolicy,
        DelegationPolicy, PlanGate, ProviderUsageFeature, WorkshopNodeId,
    };
    use nomifun_db::{
        IAgentExecutionRepository, IAgentExecutionTemplateRepository,
        IClientPreferenceRepository, IProviderRepository, SqliteAgentExecutionRepository,
        SqliteAgentExecutionTemplateRepository, SqliteClientPreferenceRepository,
        SqliteProviderRepository, init_database_memory,
    };
    use std::sync::Arc;

    const NOMI_AGENT_ID: &str = "0190f5fe-7c00-7a00-8000-000000000114";

    /// Minimal completer so `CompanionService::start` needs no live provider — the
    /// deletion-guard tests never trigger a distillation call.
    struct NoopCompleter;

    #[async_trait::async_trait]
    impl nomifun_companion::learner::CompanionCompleter for NoopCompleter {
        async fn complete(
            &self,
            _provider_id: &str,
            _model: &str,
            _system: &str,
            _user: &str,
            _max_tokens: u32,
        ) -> Result<String, nomifun_common::AppError> {
            Ok("{}".into())
        }
    }

    /// Build a real coordinator over an in-memory DB + tempdir-backed companion /
    /// public-agent services — mirrors the app's `build_system_state` construction
    /// (minus the live provider completer). Returns the `Database` so its in-memory
    /// pool outlives the coordinator.
    async fn coordinator(
        dir: &std::path::Path,
    ) -> (AppProviderDeletionCoordinator, Arc<nomifun_db::Database>) {
        let db = Arc::new(init_database_memory().await.unwrap());
        let installation_owner = nomifun_db::installation_owner_id(db.pool()).await.unwrap();
        for provider_id in [
            "0190f5fe-7c00-7a00-8000-000000000021",
            "0190f5fe-7c00-7a00-8000-000000000022",
            "0190f5fe-7c00-7a00-8000-000000000023",
            "0190f5fe-7c00-7a00-8000-000000000024",
            "0190f5fe-7c00-7a00-8000-000000000025",
            "0190f5fe-7c00-7a00-8000-000000000026",
        ] {
            nomifun_db::sqlx::query(
                "INSERT INTO providers (\
                    provider_id, platform, name, base_url, api_key_encrypted, models, enabled, \
                    capabilities, created_at, updated_at\
                 ) VALUES (?, 'openai', ?, 'https://example.invalid', 'encrypted', \
                           '[]', 1, '[]', 1, 1)",
            )
            .bind(provider_id)
            .bind(provider_id)
            .execute(db.pool())
            .await
            .unwrap();
        }
        let companion = nomifun_companion::CompanionService::start(
            dir,
            Arc::new(nomifun_realtime::BroadcastEventBus::new(16)),
            &installation_owner,
            Arc::new(NoopCompleter),
            Arc::new(nomifun_extension::skill_service::resolve_skill_paths(dir, dir)),
        )
        .await
        .unwrap();
        let public_agent = nomifun_public_agent::PublicAgentService::start(dir);
        let client_prefs: Arc<dyn IClientPreferenceRepository> =
            Arc::new(SqliteClientPreferenceRepository::new(db.pool().clone()));
        let execution_repo: Arc<dyn IAgentExecutionRepository> =
            Arc::new(SqliteAgentExecutionRepository::new(db.pool().clone()));
        let execution_template_repo: Arc<dyn IAgentExecutionTemplateRepository> =
            Arc::new(SqliteAgentExecutionTemplateRepository::new(db.pool().clone()));
        let conversation_repo: Arc<dyn IConversationRepository> =
            Arc::new(nomifun_db::SqliteConversationRepository::new(db.pool().clone()));
        let provider_lifecycle = Arc::new(ProviderLifecycleBarrier::new());
        (
            AppProviderDeletionCoordinator {
                provider_lifecycle: provider_lifecycle.clone(),
                companion,
                public_agent,
                workshop: nomifun_workshop::WorkshopService::start_with_provider_lifecycle(
                    dir,
                    Arc::new(nomifun_db::SqliteWorkshopRepository::new(db.pool().clone())),
                    provider_lifecycle,
                ),
                client_prefs,
                execution_repo,
                execution_template_repo,
                conversation_repo,
            },
            db,
        )
    }

    #[tokio::test]
    async fn aggregates_idmm_backup_usage() {
        let dir = tempfile::tempdir().unwrap();
        let (coord, _db) = coordinator(dir.path()).await;
        coord
            .client_prefs
            .upsert_batch(&[(nomifun_idmm::sidecar::PREF_BACKUP_PROVIDER, "0190f5fe-7c00-7a00-8000-000000000026")])
            .await
            .unwrap();
        let usages = coord.usages("0190f5fe-7c00-7a00-8000-000000000026").await.unwrap();
        assert!(
            usages
                .iter()
                .any(|u| matches!(u.feature, ProviderUsageFeature::SmartDecision)),
            "global idmm backup provider should surface as a SmartDecision usage"
        );
        assert!(coord.usages("0190f5fe-7c00-7a00-8000-000000000027").await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn usages_rejects_empty_provider_id() {
        let dir = tempfile::tempdir().unwrap();
        let (coord, _db) = coordinator(dir.path()).await;
        coord
            .client_prefs
            .upsert_batch(&[(nomifun_idmm::sidecar::PREF_BACKUP_PROVIDER, "0190f5fe-7c00-7a00-8000-000000000026")])
            .await
            .unwrap();
        assert!(matches!(coord.usages("").await, Err(AppError::BadRequest(_))));
    }

    #[tokio::test]
    async fn aggregates_reopenable_agent_execution_usage() {
        let dir = tempfile::tempdir().unwrap();
        let (coord, db) = coordinator(dir.path()).await;
        let installation_owner = nomifun_db::installation_owner_id(db.pool()).await.unwrap();
        let execution = coord
            .execution_repo
            .create_execution_with_participants(
                &installation_owner,
                &nomifun_db::CreateAgentExecutionParams {
                    goal: "正在使用受保护模型".into(),
                    status: AgentExecutionStatus::Paused,
                    plan_gate: PlanGate::Automatic,
                    adaptation_policy: AdaptationPolicy::Fixed,
                    decision_policy: DecisionPolicy::Automatic,
                    delegation_policy: DelegationPolicy::Automatic,
                    max_parallel: 1,
                    work_dir: None,
                    lead_conversation_id: None,
                    initial_plan_input: r#"{"mode":"automatic"}"#.to_owned(),
                },
                &[nomifun_db::NewAgentExecutionParticipant {
                    participant_id: nomifun_common::generate_id(),
                    source_agent_id: NOMI_AGENT_ID.into(),
                    preset_id: None,
                    preset_revision: None,
                    preset_snapshot: None,
                    provider_id: Some("0190f5fe-7c00-7a00-8000-000000000022".into()),
                    model: Some("model_in_use".into()),
                    role: None,
                    capability: None,
                    constraints: None,
                    description: None,
                    system_prompt: None,
                    enabled_skills: "[]".into(),
                    disabled_builtin_skills: "[]".into(),
                    sort_order: 0,
                }],
                &nomifun_db::NewAgentExecutionEvent {
                    event_type: AgentExecutionEventKind::Created,
                    step_id: None,
                    attempt_id: None,
                    actor: nomifun_common::AgentExecutionActor::user(&installation_owner),
                    payload: "{}".into(),
                },
            )
            .await
            .unwrap();

        let usages = coord.usages("0190f5fe-7c00-7a00-8000-000000000022").await.unwrap();
        assert_eq!(usages.len(), 1);
        assert_eq!(usages[0].feature, ProviderUsageFeature::AgentExecution);
        assert_eq!(usages[0].label, "正在使用受保护模型");
        assert_eq!(
            usages[0].target_id.as_deref(),
            Some(execution.execution_id.as_str())
        );

        sqlx::query("UPDATE agent_executions SET status = 'running' WHERE execution_id = ?")
            .bind(&execution.execution_id)
            .execute(db.pool())
            .await
            .unwrap();
        sqlx::query("UPDATE agent_executions SET status = 'completed' WHERE execution_id = ?")
            .bind(&execution.execution_id)
            .execute(db.pool())
            .await
            .unwrap();
        assert_eq!(
            coord.usages("0190f5fe-7c00-7a00-8000-000000000022").await.unwrap().len(),
            1,
            "a completed execution can be reopened by retry/adopt and must retain its provider binding"
        );

        sqlx::query("UPDATE agent_executions SET status = 'running' WHERE execution_id = ?")
            .bind(&execution.execution_id)
            .execute(db.pool())
            .await
            .unwrap();
        sqlx::query("UPDATE agent_executions SET status = 'cancelled' WHERE execution_id = ?")
            .bind(&execution.execution_id)
            .execute(db.pool())
            .await
            .unwrap();
        assert!(
            coord.usages("0190f5fe-7c00-7a00-8000-000000000022").await.unwrap().is_empty(),
            "a cancelled execution can never reopen and must not retain a live provider binding"
        );
    }

    #[tokio::test]
    async fn aggregates_saved_agent_execution_template_usage() {
        let dir = tempfile::tempdir().unwrap();
        let (coord, db) = coordinator(dir.path()).await;
        let installation_owner = nomifun_db::installation_owner_id(db.pool()).await.unwrap();
        let template = coord
            .execution_template_repo
            .create_template(
                &installation_owner,
                &nomifun_db::CreateAgentExecutionTemplateParams {
                    name: "长期协作方案".to_owned(),
                    description: None,
                    max_parallel: Some(2),
                    work_dir: None,
                    context: None,
                    participants: vec![nomifun_db::NewAgentExecutionTemplateParticipant {
                        template_participant_id: nomifun_common::generate_id(),
                        source_agent_id: NOMI_AGENT_ID.to_owned(),
                        preset_id: None,
                        preset_revision: None,
                        preset_snapshot: None,
                        provider_id: Some("0190f5fe-7c00-7a00-8000-000000000025".to_owned()),
                        model: Some("model_template".to_owned()),
                        role: None,
                        capability: None,
                        constraints: None,
                        description: None,
                        system_prompt: None,
                        enabled_skills: "[]".to_owned(),
                        disabled_builtin_skills: "[]".to_owned(),
                        sort_order: 0,
                    }],
                },
            )
            .await
            .unwrap();

        let usages = coord.usages("0190f5fe-7c00-7a00-8000-000000000025").await.unwrap();
        assert_eq!(usages.len(), 1);
        assert_eq!(usages[0].feature, ProviderUsageFeature::AgentExecution);
        assert_eq!(usages[0].label, "长期协作方案");
        assert_eq!(
            usages[0].target_id.as_deref(),
            Some(template.template.execution_template_id.as_str())
        );
        assert!(
            coord
                .usages("0190f5fe-7c00-7a00-8000-000000000024")
                .await
                .unwrap()
                .is_empty(),
            "the frozen preset snapshot is audit data; the concrete participant row is the only live provider binding"
        );
    }

    #[tokio::test]
    async fn provider_delete_atomically_strips_failover_queue_entry() {
        use nomifun_conversation::model_failover::{
            get_global_failover_config, set_global_failover_config,
        };
        let dir = tempfile::tempdir().unwrap();
        let (coord, db) = coordinator(dir.path()).await;
        let mut cfg = get_global_failover_config(&coord.client_prefs).await;
        cfg.queue = vec![
            nomifun_common::ProviderWithModel {
                provider_id: "0190f5fe-7c00-7a00-8000-000000000026".into(),
                model: "m".into(),
                use_model: None,
            },
            nomifun_common::ProviderWithModel {
                provider_id: "0190f5fe-7c00-7a00-8000-000000000023".into(),
                model: "m2".into(),
                use_model: None,
            },
        ];
        set_global_failover_config(&coord.client_prefs, &cfg)
            .await
            .unwrap();

        SqliteProviderRepository::new(db.pool().clone())
            .delete("0190f5fe-7c00-7a00-8000-000000000026")
            .await
            .unwrap();
        let after = get_global_failover_config(&coord.client_prefs).await;
        assert_eq!(after.queue.len(), 1);
        assert_eq!(after.queue[0].provider_id, "0190f5fe-7c00-7a00-8000-000000000023");
    }

    #[tokio::test]
    async fn active_conversation_lead_is_a_hard_provider_binding() {
        use nomifun_db::{IConversationRepository, SqliteConversationRepository, models::ConversationRow};

        let dir = tempfile::tempdir().unwrap();
        let (coord, db) = coordinator(dir.path()).await;
        let installation_owner = nomifun_db::installation_owner_id(db.pool()).await.unwrap();
        let conversation_repo = SqliteConversationRepository::new(db.pool().clone());
        let now = nomifun_common::now_ms();
        let conversation_id = conversation_repo
            .create(&ConversationRow {
                id: 0,
                conversation_id: nomifun_common::ConversationId::new().into_string(),
                user_id: installation_owner,
                name: "受保护主会话".into(),
                r#type: "nomi".into(),
                extra: "{}".into(),
                delegation_policy: "automatic".into(),
                execution_model_pool: None,
                decision_policy: "automatic".into(),
                execution_template_id: None,
                model: Some(
                    serde_json::json!({
                        "provider_id": "0190f5fe-7c00-7a00-8000-000000000021",
                        "model": "catalog-name",
                        "use_model": "effective-name"
                    })
                    .to_string(),
                ),
                status: Some("pending".into()),
                source: Some("nomifun".into()),
                channel_chat_id: None,
                pinned: false,
                pinned_at: None,
                cron_job_id: None,
                preset_id: None,
                preset_revision: None,
                preset_snapshot: None,
                created_at: now,
                updated_at: now,
            })
            .await
            .unwrap();

        let usages = coord.usages("0190f5fe-7c00-7a00-8000-000000000021").await.unwrap();
        assert_eq!(usages.len(), 1);
        assert_eq!(usages[0].feature, ProviderUsageFeature::Conversation);
        assert_eq!(usages[0].label, "受保护主会话");
        assert_eq!(usages[0].target_id, Some(conversation_id.to_string()));
    }

    #[tokio::test]
    async fn provider_delete_atomically_strips_persisted_conversation_execution_pool() {
        use nomifun_db::{IConversationRepository, SqliteConversationRepository, models::ConversationRow};

        let dir = tempfile::tempdir().unwrap();
        let (_coord, db) = coordinator(dir.path()).await;
        let installation_owner = nomifun_db::installation_owner_id(db.pool()).await.unwrap();
        let conversation_repo = SqliteConversationRepository::new(db.pool().clone());
        let now = nomifun_common::now_ms();
        let conversation_id = conversation_repo
            .create(&ConversationRow {
                id: 0,
                conversation_id: nomifun_common::ConversationId::new().into_string(),
                user_id: installation_owner,
                name: "cleanup target".into(),
                r#type: "nomi".into(),
                extra: serde_json::json!({
                    "workspace": "/keep"
                })
                .to_string(),
                delegation_policy: "automatic".into(),
                execution_model_pool: Some(serde_json::json!({
                    "mode": "range",
                    "models": [
                        { "provider_id": "0190f5fe-7c00-7a00-8000-000000000026", "model": "gone" },
                        { "provider_id": "0190f5fe-7c00-7a00-8000-000000000023", "model": "live" }
                    ]
                }).to_string()),
                decision_policy: "automatic".into(),
                execution_template_id: None,
                model: Some(
                    serde_json::json!({
                        "provider_id": "0190f5fe-7c00-7a00-8000-000000000023",
                        "model": "live"
                    })
                    .to_string(),
                ),
                status: Some("pending".into()),
                source: Some("nomifun".into()),
                channel_chat_id: None,
                pinned: false,
                pinned_at: None,
                cron_job_id: None,
                preset_id: None,
                preset_revision: None,
                preset_snapshot: None,
                created_at: now,
                updated_at: now,
            })
            .await
            .unwrap();

        SqliteProviderRepository::new(db.pool().clone())
            .delete("0190f5fe-7c00-7a00-8000-000000000026")
            .await
            .unwrap();

        let cleaned = conversation_repo.get(&conversation_id).await.unwrap().unwrap();
        let extra: serde_json::Value = serde_json::from_str(&cleaned.extra).unwrap();
        assert_eq!(extra["workspace"], "/keep");
        let model_pool: serde_json::Value = serde_json::from_str(
            cleaned.execution_model_pool.as_deref().unwrap(),
        )
        .unwrap();
        assert_eq!(
            model_pool,
            serde_json::json!({
                "mode": "range",
                "models": [{ "provider_id": "0190f5fe-7c00-7a00-8000-000000000023", "model": "live" }]
            })
        );
    }

    #[tokio::test]
    async fn provider_service_delete_cleans_workshop_soft_references() {
        let dir = tempfile::tempdir().unwrap();
        let (coord, db) = coordinator(dir.path()).await;
        let workshop = coord.workshop.clone();
        let canvas = workshop
            .create_canvas(Some("provider cleanup integration".into()))
            .await
            .unwrap();
        let deleted_provider_id = "0190f5fe-7c00-7a00-8000-000000000026";
        let surviving_provider_id = "0190f5fe-7c00-7a00-8000-000000000023";
        workshop
            .save_doc(
                &canvas.canvas_id,
                &serde_json::json!({
                    "schema": 1,
                    "nodes": [
                        {
                            "id": WorkshopNodeId::new().into_string(),
                            "kind": "generator",
                            "data": {
                                "providerId": deleted_provider_id,
                                "model": "delete-me",
                                "prompt": "preserve"
                            }
                        },
                        {
                            "id": WorkshopNodeId::new().into_string(),
                            "kind": "generator",
                            "data": {
                                "providerId": surviving_provider_id,
                                "model": "keep-me"
                            }
                        }
                    ],
                    "edges": []
                }),
            )
            .await
            .unwrap();

        let provider_repo = Arc::new(SqliteProviderRepository::new(db.pool().clone()));
        let provider_service =
            nomifun_system::ProviderService::new(provider_repo.clone(), [0u8; 32])
                .with_deletion_coordinator(Arc::new(coord));
        provider_service.delete(deleted_provider_id).await.unwrap();

        assert!(provider_repo.find_by_id(deleted_provider_id).await.unwrap().is_none());
        let cleaned = workshop.get_canvas(&canvas.canvas_id).await.unwrap().doc;
        assert!(cleaned["nodes"][0]["data"].get("providerId").is_none());
        assert!(cleaned["nodes"][0]["data"].get("model").is_none());
        assert_eq!(cleaned["nodes"][0]["data"]["prompt"], "preserve");
        assert_eq!(
            cleaned["nodes"][1]["data"]["providerId"],
            serde_json::json!(surviving_provider_id)
        );
        assert_eq!(cleaned["nodes"][1]["data"]["model"], "keep-me");
    }
}

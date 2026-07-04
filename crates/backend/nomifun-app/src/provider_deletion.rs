//! App-layer aggregation of every subsystem's provider-in-use scan + soft-ref cleanup.
//!
//! `nomifun-app` is the only layer that sees the companion, public-agent, IDMM
//! and orchestrator subsystems at once, so the cross-subsystem
//! [`ProviderDeletionCoordinator`](nomifun_system::provider_deletion::ProviderDeletionCoordinator)
//! is implemented here and injected into `ProviderService` (see
//! `router::state::build_system_state`). Deletion then refuses an in-use provider
//! (409 `PROVIDER_IN_USE`) and, on a successful delete, strips the single soft
//! reference this v1 owns: the global model-failover queue.

use std::sync::Arc;

use nomifun_common::{AppError, ProviderUsage, ProviderUsageFeature};
use nomifun_conversation::model_failover::{get_global_failover_config, set_global_failover_config};
use nomifun_db::{IClientPreferenceRepository, IFleetRepository};
use nomifun_idmm::sidecar::PREF_BACKUP_PROVIDER;
use nomifun_system::provider_deletion::ProviderDeletionCoordinator;

/// Aggregates every subsystem's provider-in-use scan behind the single
/// `ProviderDeletionCoordinator` hook `ProviderService::delete` calls.
pub struct AppProviderDeletionCoordinator {
    pub companion: Arc<nomifun_companion::CompanionService>,
    pub public_agent: Arc<nomifun_public_agent::PublicAgentService>,
    pub client_prefs: Arc<dyn IClientPreferenceRepository>,
    pub fleet_repo: Arc<dyn IFleetRepository>,
}

#[async_trait::async_trait]
impl ProviderDeletionCoordinator for AppProviderDeletionCoordinator {
    async fn usages(&self, provider_id: &str) -> Result<Vec<ProviderUsage>, AppError> {
        // Central belt-and-suspenders guard: the empty provider_id is the
        // "unconfigured" sentinel. The companion / public-agent scans already skip
        // it, but bailing here also keeps the fleet SQL (`provider_id = ''`) and the
        // idmm-backup pref compare from ever false-matching an unconfigured slot.
        if provider_id.is_empty() {
            return Ok(Vec::new());
        }

        let mut out = Vec::new();
        out.extend(self.companion.providers_in_use(provider_id).await);
        out.extend(self.public_agent.providers_in_use(provider_id).await);

        // 智能决策 (smart decision): v1 covers ONLY the global backup model
        // (`idmm_backup_provider_id`). Per-conversation watch `bypass_model` is out
        // of scope — no cross-user session-enumeration repo exists (see plan
        // constraint); component B backstops it.
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

        // 智能编排 (orchestration): every fleet whose members reference the provider.
        let fleets = self
            .fleet_repo
            .fleets_using_provider(provider_id)
            .await
            .map_err(|e| AppError::Internal(format!("scan fleets: {e}")))?;
        for (id, name) in fleets {
            out.push(ProviderUsage {
                feature: ProviderUsageFeature::Orchestrator,
                label: name,
                target_id: Some(id),
            });
        }
        Ok(out)
    }

    async fn cleanup_soft_refs(&self, provider_id: &str) -> Result<(), AppError> {
        // v1 strips ONLY the global model-failover queue. `idmm_backup_*` is a
        // PROTECTED reference (blocks deletion in `usages`, never reaches cleanup);
        // channel `assistant.{platform}.defaultModel` is backstopped by component B.
        let mut cfg = get_global_failover_config(&self.client_prefs).await;
        let before = cfg.queue.len();
        cfg.queue.retain(|m| m.provider_id != provider_id);
        if cfg.queue.len() != before {
            set_global_failover_config(&self.client_prefs, &cfg).await?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nomifun_common::ProviderUsageFeature;
    use nomifun_db::{
        IClientPreferenceRepository, IFleetRepository, SqliteClientPreferenceRepository,
        SqliteFleetRepository, init_database_memory,
    };
    use std::sync::Arc;

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
        let companion = nomifun_companion::CompanionService::start(
            dir,
            Arc::new(nomifun_realtime::BroadcastEventBus::new(16)),
            Arc::new(NoopCompleter),
            Arc::new(nomifun_extension::skill_service::resolve_skill_paths(dir, dir)),
        )
        .await
        .unwrap();
        let public_agent = nomifun_public_agent::PublicAgentService::start(dir);
        let client_prefs: Arc<dyn IClientPreferenceRepository> =
            Arc::new(SqliteClientPreferenceRepository::new(db.pool().clone()));
        let fleet_repo: Arc<dyn IFleetRepository> =
            Arc::new(SqliteFleetRepository::new(db.pool().clone()));
        (
            AppProviderDeletionCoordinator {
                companion,
                public_agent,
                client_prefs,
                fleet_repo,
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
            .upsert_batch(&[(nomifun_idmm::sidecar::PREF_BACKUP_PROVIDER, "prov_x")])
            .await
            .unwrap();
        let usages = coord.usages("prov_x").await.unwrap();
        assert!(
            usages
                .iter()
                .any(|u| matches!(u.feature, ProviderUsageFeature::SmartDecision)),
            "global idmm backup provider should surface as a SmartDecision usage"
        );
        assert!(coord.usages("prov_none").await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn usages_skips_empty_provider_id() {
        // Central belt-and-suspenders guard: even with a backup pref set, the empty
        // "unconfigured" sentinel must never aggregate a usage (protects the fleet
        // SQL `= ''` match + idmm pref compare from false positives).
        let dir = tempfile::tempdir().unwrap();
        let (coord, _db) = coordinator(dir.path()).await;
        coord
            .client_prefs
            .upsert_batch(&[(nomifun_idmm::sidecar::PREF_BACKUP_PROVIDER, "prov_x")])
            .await
            .unwrap();
        assert!(coord.usages("").await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn cleanup_strips_failover_queue_entry() {
        use nomifun_conversation::model_failover::{
            get_global_failover_config, set_global_failover_config,
        };
        let dir = tempfile::tempdir().unwrap();
        let (coord, _db) = coordinator(dir.path()).await;
        let mut cfg = get_global_failover_config(&coord.client_prefs).await;
        cfg.queue = vec![
            nomifun_common::ProviderWithModel {
                provider_id: "prov_x".into(),
                model: "m".into(),
                use_model: None,
            },
            nomifun_common::ProviderWithModel {
                provider_id: "prov_keep".into(),
                model: "m2".into(),
                use_model: None,
            },
        ];
        set_global_failover_config(&coord.client_prefs, &cfg)
            .await
            .unwrap();

        coord.cleanup_soft_refs("prov_x").await.unwrap();
        let after = get_global_failover_config(&coord.client_prefs).await;
        assert_eq!(after.queue.len(), 1);
        assert_eq!(after.queue[0].provider_id, "prov_keep");
    }
}

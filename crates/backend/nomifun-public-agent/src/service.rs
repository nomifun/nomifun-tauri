//! `PublicAgentService` — bundles the roster [`PublicAgentRegistry`] with the
//! per-agent [`crate::audit`] log and resolves data-dir paths. This is the
//! single handle the API routes and the runtime provider talk to.

use std::path::PathBuf;
use std::sync::Arc;

use nomifun_common::{
    AppError, ProviderId, PublicAgentId, SharedProviderLifecycleBarrier,
};
use nomifun_db::IProviderRepository;
use serde_json::Value;

use crate::audit::{self, AuditEntry, AuditPage, AuditQuery};
use crate::config::PublicAgentConfig;
use crate::registry::PublicAgentRegistry;

pub struct PublicAgentService {
    registry: Arc<PublicAgentRegistry>,
    dir: PathBuf,
    startup_audit: tokio::sync::OnceCell<Result<(), String>>,
}

impl PublicAgentService {
    /// Scan `{data_dir}/public-agents/` into a live service.
    pub fn start(data_dir: &std::path::Path) -> Arc<Self> {
        Self::start_with_provider_lifecycle(data_dir, None, None)
    }

    pub fn start_with_provider_lifecycle(
        data_dir: &std::path::Path,
        provider_repo: Option<Arc<dyn IProviderRepository>>,
        provider_lifecycle: Option<SharedProviderLifecycleBarrier>,
    ) -> Arc<Self> {
        let dir = data_dir.join(crate::PUBLIC_AGENTS_REL_DIR);
        Arc::new(Self {
            registry: Arc::new(PublicAgentRegistry::scan_with_provider_lifecycle(
                dir.clone(),
                provider_repo,
                provider_lifecycle,
            )),
            dir,
            startup_audit: tokio::sync::OnceCell::new(),
        })
    }

    fn agent_dir(&self, public_agent_id: &PublicAgentId) -> PathBuf {
        self.dir.join(public_agent_id.as_str())
    }

    async fn ensure_ready(&self) -> Result<(), AppError> {
        let result = self
            .startup_audit
            .get_or_init(|| async {
                self.registry
                    .validate_provider_references_on_startup()
                    .await
                    .map_err(|error| error.to_string())
            })
            .await;
        result
            .as_ref()
            .map(|_| ())
            .map_err(|error| AppError::Internal(error.clone()))
    }

    // ---- roster CRUD ----

    pub async fn list(&self) -> Result<Vec<PublicAgentConfig>, AppError> {
        self.ensure_ready().await?;
        self.registry.list_checked().await
    }

    pub async fn get(
        &self,
        public_agent_id: &str,
    ) -> Result<PublicAgentConfig, AppError> {
        self.ensure_ready().await?;
        let public_agent_id = parse_public_agent_id(public_agent_id)?;
        self.registry
            .get_checked(&public_agent_id)
            .await
            ?
            .ok_or_else(|| {
                AppError::NotFound(format!(
                    "public agent {public_agent_id} not found"
                ))
            })
    }

    pub async fn exists(&self, public_agent_id: &str) -> bool {
        if self.ensure_ready().await.is_err() {
            return false;
        }
        let Ok(public_agent_id) = PublicAgentId::parse(public_agent_id) else {
            return false;
        };
        self.registry.exists(&public_agent_id).await
    }

    pub async fn create(&self, name: &str) -> Result<PublicAgentConfig, AppError> {
        self.ensure_ready().await?;
        let created = self.registry.create(name).await?;
        self.record_event(
            created.public_agent_id.as_str(),
            "lifecycle",
            "created",
        )
        .await;
        Ok(created)
    }

    /// RFC 7396 merge-patch. Logs a lifecycle audit event when `enabled` flips
    /// (owner-visible change trail).
    pub async fn patch(
        &self,
        public_agent_id: &str,
        patch: Value,
    ) -> Result<PublicAgentConfig, AppError> {
        self.ensure_ready().await?;
        let public_agent_id = parse_public_agent_id(public_agent_id)?;
        let prev_enabled = self
            .registry
            .get(&public_agent_id)
            .await
            .map(|a| a.enabled);
        let next = self.registry.patch(&public_agent_id, patch).await?;
        if let Some(prev) = prev_enabled {
            if prev != next.enabled {
                let detail = if next.enabled { "enabled" } else { "disabled" };
                self.record_event(
                    public_agent_id.as_str(),
                    "lifecycle",
                    detail,
                )
                .await;
            }
        }
        Ok(next)
    }

    /// Apply a resolved preset while preserving the public companion's brand,
    /// greeting, service policy, audit history and serving state. Security
    /// clamps are enforced later by the agent factory and are never sourced
    /// from the preset.
    pub async fn apply_preset_snapshot(
        &self,
        public_agent_id: &str,
        mut snapshot: nomifun_api_types::ResolvedPresetSnapshot,
    ) -> Result<PublicAgentConfig, AppError> {
        if snapshot.target != nomifun_api_types::PresetTarget::PublicCompanion {
            return Err(AppError::BadRequest(
                "preset snapshot target must be public_companion".into(),
            ));
        }
        let resolved_model = snapshot.resolved_model.take();
        let mut patch = serde_json::json!({ "applied_preset": snapshot });
        if let Some(model) = resolved_model {
            if let Some(provider_id) = model.provider_id {
                patch["model"] = serde_json::json!({
                    "provider_id": provider_id,
                    "model": model.model,
                });
            }
        }
        if let Some(snapshot) = patch.get("applied_preset").cloned() {
            if let Some(ids) = snapshot.get("knowledge_base_ids") {
                patch["knowledge_base_ids"] = ids.clone();
            }
            if snapshot
                .get("knowledge_policy")
                .and_then(|policy| policy.get("grounded"))
                .and_then(Value::as_bool)
                == Some(true)
            {
                // A strict preset can tighten a public companion. A non-strict
                // preset may never weaken an existing grounded service.
                patch["grounded_mode"] = Value::Bool(true);
            }
        }
        self.patch(public_agent_id, patch).await
    }

    pub async fn delete(&self, public_agent_id: &str) -> Result<(), AppError> {
        self.ensure_ready().await?;
        let public_agent_id = parse_public_agent_id(public_agent_id)?;
        self.registry.remove(&public_agent_id).await.map(|_| ())
    }

    // ---- provider usage ----

    /// Report every public agent whose model is backed by `provider_id`
    /// (feeds the provider-deletion guard). Each hit is labelled by the agent
    /// name and deep-links via its public-agent identity.
    pub async fn providers_in_use(&self, provider_id: &str) -> Vec<nomifun_common::ProviderUsage> {
        let Ok(provider_id) = ProviderId::parse(provider_id) else {
            return Vec::new();
        };
        if let Err(error) = self
            .registry
            .validate_provider_references_under_existing_guard()
            .await
        {
            return vec![nomifun_common::ProviderUsage {
                feature: nomifun_common::ProviderUsageFeature::PublicCompanion,
                label: format!("对外伙伴 Provider 引用审计失败（{error}）"),
                target_id: None,
            }];
        }
        if let Some(error) = self.registry.health_error().await {
            // The provider deletion coordinator already holds the lifecycle
            // write guard here. A corrupt side store may hide an arbitrary
            // reference, so conservatively block every Provider deletion.
            return vec![nomifun_common::ProviderUsage {
                feature: nomifun_common::ProviderUsageFeature::PublicCompanion,
                label: format!("对外伙伴数据不可读（{error}）"),
                target_id: None,
            }];
        }
        self.registry
            .list()
            .await
            .into_iter()
            .filter(|a| a.model.as_ref().is_some_and(|model| model.provider_id == provider_id))
            .map(|a| nomifun_common::ProviderUsage {
                feature: nomifun_common::ProviderUsageFeature::PublicCompanion,
                label: a.name,
                target_id: Some(a.public_agent_id.into_string()),
            })
            .collect()
    }

    // ---- audit ----

    /// Record an inbound turn (best-effort; never fails the caller). Retention
    /// is read from the agent's own config; unknown agent → no-op.
    pub async fn record_turn(
        &self,
        public_agent_id: &str,
        surface: &str,
        platform: Option<&str>,
        text: &str,
    ) {
        if self.ensure_ready().await.is_err() {
            return;
        }
        let Ok(public_agent_id) = PublicAgentId::parse(public_agent_id) else {
            return;
        };
        let Some(cfg) = self.registry.get(&public_agent_id).await else {
            return;
        };
        let entry = AuditEntry::turn(surface, platform.map(str::to_owned), text);
        if let Err(error) = audit::append(
            &self.agent_dir(&public_agent_id),
            &entry,
            cfg.audit_retention_days,
        ) {
            tracing::warn!(
                %error,
                %public_agent_id,
                "public-agent audit append failed"
            );
        }
    }

    /// Record a lifecycle / config event (best-effort).
    pub async fn record_event(
        &self,
        public_agent_id: &str,
        kind: &str,
        detail: impl Into<String>,
    ) {
        if self.ensure_ready().await.is_err() {
            return;
        }
        let Ok(public_agent_id) = PublicAgentId::parse(public_agent_id) else {
            return;
        };
        let retention = self
            .registry
            .get(&public_agent_id)
            .await
            .map(|a| a.audit_retention_days)
            .unwrap_or(0);
        let entry = AuditEntry::event(kind, detail);
        if let Err(error) =
            audit::append(&self.agent_dir(&public_agent_id), &entry, retention)
        {
            tracing::warn!(
                %error,
                %public_agent_id,
                "public-agent audit event append failed"
            );
        }
    }

    /// Search / paginate the audit log. Registry corruption fails closed rather
    /// than being misreported as a missing agent.
    pub async fn search_audit(
        &self,
        public_agent_id: &str,
        query: AuditQuery,
    ) -> Result<AuditPage, AppError> {
        self.ensure_ready().await?;
        let public_agent_id = parse_public_agent_id(public_agent_id)?;
        if self
            .registry
            .get_checked(&public_agent_id)
            .await?
            .is_none()
        {
            return Err(AppError::NotFound(format!(
                "public agent {public_agent_id} not found"
            )));
        }
        audit::search(&self.agent_dir(&public_agent_id), &query)
    }

    /// Delete audit day-files older than `older_than_days`; returns the count.
    pub async fn delete_audit(
        &self,
        public_agent_id: &str,
        older_than_days: u32,
    ) -> Result<usize, AppError> {
        self.ensure_ready().await?;
        let public_agent_id = parse_public_agent_id(public_agent_id)?;
        if self
            .registry
            .get_checked(&public_agent_id)
            .await?
            .is_none()
        {
            return Err(AppError::NotFound(format!(
                "public agent {public_agent_id} not found"
            )));
        }
        audit::delete_older_than(
            &self.agent_dir(&public_agent_id),
            older_than_days,
        )
    }
}

fn parse_public_agent_id(public_agent_id: &str) -> Result<PublicAgentId, AppError> {
    PublicAgentId::parse(public_agent_id)
        .map_err(|error| AppError::BadRequest(format!("invalid public-agent id: {error}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn create_patch_audit_flow() {
        let d = tempfile::tempdir().unwrap();
        let svc = PublicAgentService::start(d.path());
        let a = svc.create("客服").await.unwrap();
        assert!(a.enabled);

        // A turn is audited under the agent.
        svc.record_turn(
            a.public_agent_id.as_str(),
            "channel",
            Some("telegram"),
            "请问怎么退货",
        )
        .await;
        let page = svc
            .search_audit(
                a.public_agent_id.as_str(),
                AuditQuery {
                    limit: 50,
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        assert!(page.entries.iter().any(|e| e.kind == "turn" && e.detail == "请问怎么退货"));
        // create() logged a lifecycle event too.
        assert!(page.entries.iter().any(|e| e.kind == "lifecycle" && e.detail == "created"));

        // Disabling logs a lifecycle event.
        let patched = svc
            .patch(
                a.public_agent_id.as_str(),
                serde_json::json!({ "enabled": false }),
            )
            .await
            .unwrap();
        assert!(!patched.enabled);
        let page2 = svc
            .search_audit(
                a.public_agent_id.as_str(),
                AuditQuery {
                    limit: 50,
                    kind: Some("lifecycle".into()),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        assert!(page2.entries.iter().any(|e| e.detail == "disabled"));

        // Unknown agent → NotFound on search.
        assert!(
            svc.search_audit("not-a-public-agent-id", AuditQuery::default())
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn providers_in_use_detects_public_agent_model() {
        let d = tempfile::tempdir().unwrap();
        let svc = PublicAgentService::start(d.path());
        let a = svc.create("客服").await.unwrap();
        let provider_id = ProviderId::new();
        svc.patch(
            a.public_agent_id.as_str(),
            serde_json::json!({"model":{"provider_id":provider_id,"model":"m"}}),
        )
        .await
        .unwrap();

        let hits = svc.providers_in_use(provider_id.as_str()).await;
        assert!(hits.iter().any(|u| {
            u.label == "客服"
                && u.target_id.as_deref()
                    == Some(a.public_agent_id.as_str())
        }));
        assert!(svc.providers_in_use("not-a-provider-id").await.is_empty());
    }
}

//! `PublicAgentService` — bundles the roster [`PublicAgentRegistry`] with the
//! per-agent [`crate::audit`] log and resolves data-dir paths. This is the
//! single handle the API routes and the runtime provider talk to.

use std::path::PathBuf;
use std::sync::Arc;

use nomifun_common::AppError;
use serde_json::Value;

use crate::audit::{self, AuditEntry, AuditPage, AuditQuery};
use crate::config::PublicAgentConfig;
use crate::registry::PublicAgentRegistry;

pub struct PublicAgentService {
    registry: Arc<PublicAgentRegistry>,
    dir: PathBuf,
}

impl PublicAgentService {
    /// Scan `{data_dir}/public-agents/` into a live service.
    pub fn start(data_dir: &std::path::Path) -> Arc<Self> {
        let dir = data_dir.join(crate::PUBLIC_AGENTS_REL_DIR);
        Arc::new(Self {
            registry: Arc::new(PublicAgentRegistry::scan(dir.clone())),
            dir,
        })
    }

    fn agent_dir(&self, id: &str) -> PathBuf {
        self.dir.join(id)
    }

    // ---- roster CRUD ----

    pub async fn list(&self) -> Vec<PublicAgentConfig> {
        self.registry.list().await
    }

    pub async fn get(&self, id: &str) -> Result<PublicAgentConfig, AppError> {
        self.registry
            .get(id)
            .await
            .ok_or_else(|| AppError::NotFound(format!("public agent {id} not found")))
    }

    pub async fn exists(&self, id: &str) -> bool {
        self.registry.exists(id).await
    }

    pub async fn create(&self, name: &str) -> Result<PublicAgentConfig, AppError> {
        let created = self.registry.create(name).await?;
        self.record_event(&created.id, "lifecycle", "created").await;
        Ok(created)
    }

    /// RFC 7396 merge-patch. Logs a lifecycle audit event when `enabled` flips
    /// (owner-visible change trail).
    pub async fn patch(&self, id: &str, patch: Value) -> Result<PublicAgentConfig, AppError> {
        let prev_enabled = self.registry.get(id).await.map(|a| a.enabled);
        let next = self.registry.patch(id, patch).await?;
        if let Some(prev) = prev_enabled {
            if prev != next.enabled {
                let detail = if next.enabled { "enabled" } else { "disabled" };
                self.record_event(id, "lifecycle", detail).await;
            }
        }
        Ok(next)
    }

    pub async fn delete(&self, id: &str) -> Result<(), AppError> {
        self.registry.remove(id).await.map(|_| ())
    }

    // ---- audit ----

    /// Record an inbound turn (best-effort; never fails the caller). Retention
    /// is read from the agent's own config; unknown agent → no-op.
    pub async fn record_turn(&self, id: &str, surface: &str, platform: Option<&str>, text: &str) {
        let Some(cfg) = self.registry.get(id).await else { return };
        let entry = AuditEntry::turn(surface, platform.map(str::to_owned), text);
        if let Err(e) = audit::append(&self.agent_dir(id), &entry, cfg.audit_retention_days) {
            tracing::warn!(error = %e, id, "public-agent audit append failed");
        }
    }

    /// Record a lifecycle / config event (best-effort).
    pub async fn record_event(&self, id: &str, kind: &str, detail: impl Into<String>) {
        let retention = self.registry.get(id).await.map(|a| a.audit_retention_days).unwrap_or(0);
        let entry = AuditEntry::event(kind, detail);
        if let Err(e) = audit::append(&self.agent_dir(id), &entry, retention) {
            tracing::warn!(error = %e, id, "public-agent audit event append failed");
        }
    }

    /// Search / paginate the audit log. Errors only if the agent is unknown.
    pub async fn search_audit(&self, id: &str, query: AuditQuery) -> Result<AuditPage, AppError> {
        if !self.registry.exists(id).await {
            return Err(AppError::NotFound(format!("public agent {id} not found")));
        }
        Ok(audit::search(&self.agent_dir(id), &query))
    }

    /// Delete audit day-files older than `older_than_days`; returns the count.
    pub async fn delete_audit(&self, id: &str, older_than_days: u32) -> Result<usize, AppError> {
        if !self.registry.exists(id).await {
            return Err(AppError::NotFound(format!("public agent {id} not found")));
        }
        Ok(audit::delete_older_than(&self.agent_dir(id), older_than_days))
    }
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
        svc.record_turn(&a.id, "channel", Some("telegram"), "请问怎么退货").await;
        let page = svc
            .search_audit(&a.id, AuditQuery { limit: 50, ..Default::default() })
            .await
            .unwrap();
        assert!(page.entries.iter().any(|e| e.kind == "turn" && e.detail == "请问怎么退货"));
        // create() logged a lifecycle event too.
        assert!(page.entries.iter().any(|e| e.kind == "lifecycle" && e.detail == "created"));

        // Disabling logs a lifecycle event.
        let patched = svc.patch(&a.id, serde_json::json!({ "enabled": false })).await.unwrap();
        assert!(!patched.enabled);
        let page2 = svc
            .search_audit(&a.id, AuditQuery { limit: 50, kind: Some("lifecycle".into()), ..Default::default() })
            .await
            .unwrap();
        assert!(page2.entries.iter().any(|e| e.detail == "disabled"));

        // Unknown agent → NotFound on search.
        assert!(svc.search_audit("pubagent_nope", AuditQuery::default()).await.is_err());
    }
}

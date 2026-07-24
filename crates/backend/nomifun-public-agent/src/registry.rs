//! The public-companion roster: atomic per-agent JSON files plus a registry-
//! private seq high-watermark so a deleted agent's short number is never reused.
//! Mirrors the shape of `nomifun-companion::registry` but is a wholly separate
//! store under `public-agents/`.

use std::collections::BTreeMap;
use std::path::PathBuf;

use nomifun_common::{AppError, PublicAgentId, SharedProviderLifecycleBarrier};
use nomifun_db::IProviderRepository;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::sync::RwLock;

use crate::config::PublicAgentConfig;
use crate::fsio::{load_json_optional, save_json_atomic};

const CONFIG_FILE: &str = "config.json";
/// Registry-private seq watermark file (hidden, alongside the agent dirs).
const SEQ_STATE_FILE: &str = ".seq.json";

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct SeqState {
    last_seq: u64,
}

/// RFC 7396 merge-patch (subset): recurse into objects, replace scalars, and
/// treat a JSON `null` as "remove the key".
fn json_merge_patch(target: &mut Value, patch: &Value) {
    match (target, patch) {
        (Value::Object(t), Value::Object(p)) => {
            for (k, v) in p {
                if v.is_null() {
                    t.remove(k);
                } else {
                    json_merge_patch(t.entry(k.clone()).or_insert(Value::Null), v);
                }
            }
        }
        (t, p) => *t = p.clone(),
    }
}

/// The in-memory roster + persisted seq watermark.
pub struct PublicAgentRegistry {
    dir: PathBuf,
    agents: RwLock<BTreeMap<PublicAgentId, PublicAgentConfig>>,
    watermark: RwLock<u64>,
    provider_repo: Option<std::sync::Arc<dyn IProviderRepository>>,
    provider_lifecycle: Option<SharedProviderLifecycleBarrier>,
    scan_error: RwLock<Option<String>>,
}

fn max_live_seq(agents: &BTreeMap<PublicAgentId, PublicAgentConfig>) -> u64 {
    agents.values().map(|a| a.seq).max().unwrap_or(0)
}

impl PublicAgentRegistry {
    /// Scan `{data_dir}/public-agents/*/config.json` into memory and load the
    /// seq watermark. Any unreadable, malformed, or identity-mismatched durable
    /// entry marks the whole registry unhealthy; mutation paths then fail and
    /// reads expose no partial roster.
    pub fn scan(dir: PathBuf) -> Self {
        Self::scan_with_provider_lifecycle(dir, None, None)
    }

    pub fn scan_with_provider_lifecycle(
        dir: PathBuf,
        provider_repo: Option<std::sync::Arc<dyn IProviderRepository>>,
        provider_lifecycle: Option<SharedProviderLifecycleBarrier>,
    ) -> Self {
        let mut agents = BTreeMap::new();
        let mut scan_error = None;
        match std::fs::read_dir(&dir) {
            Ok(entries) => {
                for entry in entries {
                    let entry = match entry {
                        Ok(entry) => entry,
                        Err(error) => {
                            scan_error = Some(format!(
                                "scan public-agent directory {}: {error}",
                                dir.display()
                            ));
                            break;
                        }
                    };
                    let file_type = match entry.file_type() {
                        Ok(file_type) => file_type,
                        Err(error) => {
                            scan_error = Some(format!(
                                "inspect public-agent entry {}: {error}",
                                entry.path().display()
                            ));
                            break;
                        }
                    };
                    let path = entry.path();
                    if file_type.is_symlink() {
                        scan_error = Some(format!(
                            "public-agent side store contains a symlink entry {}",
                            path.display()
                        ));
                        break;
                    }
                    if file_type.is_file() {
                        if entry.file_name().to_string_lossy() != SEQ_STATE_FILE {
                            scan_error = Some(format!(
                                "public-agent side store contains unexpected file {}",
                                path.display()
                            ));
                            break;
                        }
                        continue;
                    }
                    if !file_type.is_dir() {
                        scan_error = Some(format!(
                            "public-agent side store contains unsupported entry {}",
                            path.display()
                        ));
                        break;
                    }
                    let cfg_path = path.join(CONFIG_FILE);
                    let cfg_metadata = match std::fs::symlink_metadata(&cfg_path) {
                        Ok(metadata) => metadata,
                        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                            scan_error = Some(format!(
                                "public-agent directory {} is missing {CONFIG_FILE}",
                                path.display()
                            ));
                            break;
                        }
                        Err(error) => {
                            scan_error = Some(format!(
                                "inspect public-agent config {}: {error}",
                                cfg_path.display()
                            ));
                            break;
                        }
                    };
                    if !cfg_metadata.is_file() || cfg_metadata.file_type().is_symlink() {
                        scan_error = Some(format!(
                            "public-agent config {} is not a real regular file",
                            cfg_path.display()
                        ));
                        break;
                    }
                    let cfg = match load_json_optional::<PublicAgentConfig>(&cfg_path) {
                        Ok(Some(cfg)) => cfg,
                        Ok(None) => unreachable!("config metadata was checked above"),
                        Err(error) => {
                            scan_error = Some(format!(
                                "load public-agent config {}: {error}",
                                cfg_path.display()
                            ));
                            break;
                        }
                    };
                    if let Err(error) = cfg.validate() {
                        scan_error = Some(format!(
                            "invalid public-agent config {}: {error}",
                            cfg_path.display()
                        ));
                        break;
                    }
                    if entry.file_name().to_string_lossy() != cfg.public_agent_id.as_str() {
                        scan_error = Some(format!(
                            "public-agent config {} has public_agent_id '{}' that does not match its directory",
                            cfg_path.display(),
                            cfg.public_agent_id
                        ));
                        break;
                    }
                    if agents
                        .insert(cfg.public_agent_id.clone(), cfg)
                        .is_some()
                    {
                        scan_error = Some(
                            "duplicate public-agent identity found during scan".into(),
                        );
                        break;
                    }
                    let contents = match std::fs::read_dir(&path) {
                        Ok(contents) => contents,
                        Err(error) => {
                            scan_error = Some(format!(
                                "scan public-agent directory {}: {error}",
                                path.display()
                            ));
                            break;
                        }
                    };
                    for child in contents {
                        let child = match child {
                            Ok(child) => child,
                            Err(error) => {
                                scan_error = Some(format!(
                                    "scan public-agent directory {}: {error}",
                                    path.display()
                                ));
                                break;
                            }
                        };
                        let child_path = child.path();
                        let child_type = match child.file_type() {
                            Ok(file_type) => file_type,
                            Err(error) => {
                                scan_error = Some(format!(
                                    "inspect public-agent entry {}: {error}",
                                    child_path.display()
                                ));
                                break;
                            }
                        };
                        let child_name = child.file_name().to_string_lossy().into_owned();
                        if child_type.is_symlink()
                            || (child_name != CONFIG_FILE
                                && child_name != crate::audit::AUDIT_DIR)
                        {
                            scan_error = Some(format!(
                                "public-agent directory contains unexpected entry {}",
                                child_path.display()
                            ));
                            break;
                        }
                        if child_name == CONFIG_FILE && !child_type.is_file() {
                            scan_error = Some(format!(
                                "public-agent config {} is not a regular file",
                                child_path.display()
                            ));
                            break;
                        }
                        if child_name == crate::audit::AUDIT_DIR && !child_type.is_dir() {
                            scan_error = Some(format!(
                                "public-agent audit path {} is not a real directory",
                                child_path.display()
                            ));
                            break;
                        }
                    }
                    if scan_error.is_some() {
                        break;
                    }
                    if let Err(error) = crate::audit::validate_agent_audit(&path) {
                        scan_error = Some(format!(
                            "invalid public-agent audit store {}: {error}",
                            path.display()
                        ));
                        break;
                    }
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                scan_error = Some(format!(
                    "scan public-agent directory {}: {error}",
                    dir.display()
                ));
            }
        }
        let seq_state = if scan_error.is_none() {
            match load_json_optional::<SeqState>(&dir.join(SEQ_STATE_FILE)) {
                Ok(Some(state)) => state,
                Ok(None) => SeqState::default(),
                Err(error) => {
                    scan_error = Some(format!(
                        "load public-agent sequence watermark {}: {error}",
                        dir.join(SEQ_STATE_FILE).display()
                    ));
                    SeqState::default()
                }
            }
        } else {
            SeqState::default()
        };
        if let Some(error) = scan_error.as_deref() {
            tracing::error!(%error, "public-agent side store failed closed");
            agents.clear();
        }
        let watermark = seq_state.last_seq.max(max_live_seq(&agents));
        Self {
            dir,
            agents: RwLock::new(agents),
            watermark: RwLock::new(watermark),
            provider_repo,
            provider_lifecycle,
            scan_error: RwLock::new(scan_error),
        }
    }

    fn agent_dir(&self, public_agent_id: &PublicAgentId) -> PathBuf {
        self.dir.join(public_agent_id.as_str())
    }

    pub async fn list(&self) -> Vec<PublicAgentConfig> {
        if self.scan_error.read().await.is_some() {
            return Vec::new();
        }
        let mut v: Vec<_> = self.agents.read().await.values().cloned().collect();
        // Newest-first by seq (then created_at) for a stable roster order.
        v.sort_by(|a, b| b.seq.cmp(&a.seq).then(b.created_at.cmp(&a.created_at)));
        v
    }

    pub async fn list_checked(&self) -> Result<Vec<PublicAgentConfig>, AppError> {
        self.ensure_healthy().await?;
        let _provider_guard = if let Some(barrier) = self.provider_lifecycle.as_ref() {
            Some(barrier.read().await)
        } else {
            None
        };
        let configs = self.list().await;
        for cfg in &configs {
            self.validate_provider_reference(cfg).await?;
        }
        Ok(configs)
    }

    pub async fn get(
        &self,
        public_agent_id: &PublicAgentId,
    ) -> Option<PublicAgentConfig> {
        if self.scan_error.read().await.is_some() {
            return None;
        }
        self.agents.read().await.get(public_agent_id).cloned()
    }

    pub async fn get_checked(
        &self,
        public_agent_id: &PublicAgentId,
    ) -> Result<Option<PublicAgentConfig>, AppError> {
        self.ensure_healthy().await?;
        let _provider_guard = if let Some(barrier) = self.provider_lifecycle.as_ref() {
            Some(barrier.read().await)
        } else {
            None
        };
        let cfg = self.get(public_agent_id).await;
        if let Some(cfg) = cfg.as_ref() {
            self.validate_provider_reference(cfg).await?;
        }
        Ok(cfg)
    }

    pub async fn exists(&self, public_agent_id: &PublicAgentId) -> bool {
        if self.scan_error.read().await.is_some() {
            return false;
        }
        self.agents.read().await.contains_key(public_agent_id)
    }

    pub(crate) async fn health_error(&self) -> Option<String> {
        self.scan_error.read().await.clone()
    }

    async fn ensure_healthy(&self) -> Result<(), AppError> {
        match self.scan_error.read().await.as_deref() {
            Some(error) => Err(AppError::Internal(format!(
                "public-agent side store is unavailable: {error}"
            ))),
            None => Ok(()),
        }
    }

    /// Allocate the next never-reused seq, durably advance the watermark,
    /// persist `{public_agent_id}/config.json`, then insert. A profile-write
    /// failure may burn a display number but can never make that number reusable.
    pub async fn create(&self, name: &str) -> Result<PublicAgentConfig, AppError> {
        self.ensure_healthy().await?;
        let name = name.trim();
        if name.is_empty() {
            return Err(AppError::BadRequest("name must not be empty".into()));
        }
        // Lock order: watermark before the roster map.
        let mut watermark = self.watermark.write().await;
        let mut agents = self.agents.write().await;
        let seq = (*watermark).max(max_live_seq(&agents)) + 1;
        let cfg = PublicAgentConfig::new(name, seq);
        cfg.validate()?;
        self.advance_watermark(&mut watermark, seq)?;
        self.persist(&cfg)?;
        agents.insert(cfg.public_agent_id.clone(), cfg.clone());
        Ok(cfg)
    }

    /// RFC 7396 merge-patch over one agent's config. `public_agent_id` / `seq` /
    /// `created_at` are immutable (stripped from the patch). The removed generic
    /// `id` is not stripped or aliased, so strict config deserialization rejects it.
    pub async fn patch(
        &self,
        public_agent_id: &PublicAgentId,
        mut patch: Value,
    ) -> Result<PublicAgentConfig, AppError> {
        self.ensure_healthy().await?;
        if let Some(obj) = patch.as_object_mut() {
            obj.remove("public_agent_id");
            obj.remove("seq");
            obj.remove("created_at");
        }
        // Provider lifecycle barrier precedes the roster lock. Provider
        // deletion holds the write side, then scans the roster.
        let _provider_guard = if let Some(barrier) = self.provider_lifecycle.as_ref() {
            Some(barrier.read().await)
        } else {
            None
        };
        let mut agents = self.agents.write().await;
        let cur = agents
            .get(public_agent_id)
            .ok_or_else(|| {
                AppError::NotFound(format!(
                    "public agent {public_agent_id} not found"
                ))
            })?;
        let mut value = serde_json::to_value(cur).map_err(|e| AppError::Internal(e.to_string()))?;
        json_merge_patch(&mut value, &patch);
        let mut next: PublicAgentConfig =
            serde_json::from_value(value).map_err(|e| AppError::BadRequest(e.to_string()))?;
        // Preserve immutable identity regardless of a hostile patch.
        next.public_agent_id = cur.public_agent_id.clone();
        next.seq = cur.seq;
        next.created_at = cur.created_at;
        next.validate()?;
        if let (Some(provider_repo), Some(model)) =
            (self.provider_repo.as_ref(), next.model.as_ref())
            && provider_repo
                .find_by_id(model.provider_id.as_str())
                .await
                .map_err(|error| {
                    AppError::Internal(format!("check public-agent model provider: {error}"))
                })?
                .is_none()
        {
            return Err(AppError::NotFound(format!(
                "provider '{}' not found",
                model.provider_id
            )));
        }
        self.persist(&next)?;
        agents.insert(public_agent_id.clone(), next.clone());
        Ok(next)
    }

    /// Remove an agent's config dir and drop it from the roster.
    pub async fn remove(
        &self,
        public_agent_id: &PublicAgentId,
    ) -> Result<PublicAgentConfig, AppError> {
        self.ensure_healthy().await?;
        let _provider_guard = if let Some(barrier) = self.provider_lifecycle.as_ref() {
            Some(barrier.read().await)
        } else {
            None
        };
        let mut agents = self.agents.write().await;
        let removed = agents
            .get(public_agent_id)
            .cloned()
            .ok_or_else(|| {
                AppError::NotFound(format!(
                    "public agent {public_agent_id} not found"
                ))
            })?;
        crate::fsio::remove_path_entry(&self.agent_dir(public_agent_id))
            .map_err(|error| AppError::Internal(format!("remove public-agent directory: {error}")))?;
        agents.remove(public_agent_id);
        Ok(removed)
    }

    pub(crate) async fn validate_provider_reference(
        &self,
        cfg: &PublicAgentConfig,
    ) -> Result<(), AppError> {
        let (Some(provider_repo), Some(model)) =
            (self.provider_repo.as_ref(), cfg.model.as_ref())
        else {
            return Ok(());
        };
        if provider_repo
            .find_by_id(model.provider_id.as_str())
            .await
            .map_err(|error| {
                AppError::Internal(format!(
                    "check public-agent '{}' model provider: {error}",
                    cfg.public_agent_id
                ))
            })?
            .is_none()
        {
            return Err(AppError::Internal(format!(
                "public agent '{}' references missing provider '{}'",
                cfg.public_agent_id, model.provider_id
            )));
        }
        Ok(())
    }

    /// Validate every loaded Provider parent before the service becomes
    /// observable. The lifecycle read guard covers the complete audit, so a
    /// concurrent Provider deletion cannot pass its usage scan and remove a
    /// parent between two checks. Failure permanently poisons this registry
    /// instance and clears its in-memory roster; callers then fail closed.
    pub(crate) async fn validate_provider_references_on_startup(
        &self,
    ) -> Result<(), AppError> {
        self.ensure_healthy().await?;
        let _provider_guard = if let Some(barrier) = self.provider_lifecycle.as_ref() {
            Some(barrier.read().await)
        } else {
            None
        };
        self.validate_provider_references_under_existing_guard().await
    }

    /// Validate the loaded roster without acquiring the lifecycle barrier.
    /// Provider deletion calls this while it already owns the write guard;
    /// taking a nested read guard there would deadlock.
    pub(crate) async fn validate_provider_references_under_existing_guard(
        &self,
    ) -> Result<(), AppError> {
        self.ensure_healthy().await?;
        let configs: Vec<_> = self.agents.read().await.values().cloned().collect();
        for cfg in &configs {
            if let Err(error) = self.validate_provider_reference(cfg).await {
                let detail = format!(
                    "public-agent startup Provider audit failed for '{}': {error}",
                    cfg.public_agent_id
                );
                *self.scan_error.write().await = Some(detail.clone());
                self.agents.write().await.clear();
                return Err(AppError::Internal(detail));
            }
        }
        Ok(())
    }

    fn persist(&self, cfg: &PublicAgentConfig) -> Result<(), AppError> {
        cfg.validate()?;
        save_json_atomic(
            &self.agent_dir(&cfg.public_agent_id),
            CONFIG_FILE,
            cfg,
        )
            .map_err(|e| AppError::Internal(format!("persist public agent: {e}")))
    }

    fn advance_watermark(
        &self,
        watermark: &mut u64,
        seq: u64,
    ) -> Result<(), AppError> {
        if seq <= *watermark {
            return Ok(());
        }
        save_json_atomic(&self.dir, SEQ_STATE_FILE, &SeqState { last_seq: seq })
            .map_err(|error| {
                AppError::Internal(format!(
                    "save public-agent sequence watermark: {error}"
                ))
            })?;
        *watermark = seq;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nomifun_db::{
        CreateProviderParams, IProviderRepository, SqliteProviderRepository,
        init_database_memory,
    };
    use nomifun_common::ProviderLifecycleBarrier;
    use std::sync::Arc;

    fn reg(dir: &std::path::Path) -> PublicAgentRegistry {
        PublicAgentRegistry::scan(dir.to_path_buf())
    }

    #[tokio::test]
    async fn create_get_patch_remove_roundtrip() {
        let d = tempfile::tempdir().unwrap();
        let r = reg(d.path());
        let a = r.create("甲").await.unwrap();
        assert_eq!(a.seq, 1);
        assert!(r.exists(&a.public_agent_id).await);

        let patched = r
            .patch(
                &a.public_agent_id,
                serde_json::json!({ "name": "甲队", "grounded_mode": false }),
            )
            .await
            .unwrap();
        assert_eq!(patched.name, "甲队");
        assert!(!patched.grounded_mode);
        assert_eq!(
            patched.public_agent_id,
            a.public_agent_id,
            "public_agent_id immutable"
        );
        assert_eq!(patched.seq, 1, "seq immutable");

        // Persisted: a fresh scan sees the patch.
        let r2 = reg(d.path());
        assert_eq!(
            r2.get(&a.public_agent_id).await.unwrap().name,
            "甲队"
        );

        r.remove(&a.public_agent_id).await.unwrap();
        assert!(!r.exists(&a.public_agent_id).await);
    }

    #[tokio::test]
    async fn seq_is_never_reused_after_delete() {
        let d = tempfile::tempdir().unwrap();
        let r = reg(d.path());
        let a = r.create("A").await.unwrap();
        let b = r.create("B").await.unwrap();
        assert_eq!(a.seq, 1);
        assert_eq!(b.seq, 2);
        r.remove(&b.public_agent_id).await.unwrap();
        let c = r.create("C").await.unwrap();
        assert_eq!(c.seq, 3, "deleted #2 must not be reused");
    }

    #[tokio::test]
    async fn patch_cannot_forge_identity() {
        let d = tempfile::tempdir().unwrap();
        let r = reg(d.path());
        let a = r.create("A").await.unwrap();
        let patched = r
            .patch(
                &a.public_agent_id,
                serde_json::json!({
                    "public_agent_id": PublicAgentId::new(),
                    "seq": 999,
                    "name": "A2"
                }),
            )
            .await
            .unwrap();
        assert_eq!(patched.public_agent_id, a.public_agent_id);
        assert_eq!(patched.seq, 1);
        assert_eq!(patched.name, "A2");

        assert!(
            r.patch(
                &a.public_agent_id,
                serde_json::json!({ "id": PublicAgentId::new() }),
            )
            .await
            .is_err()
        );
    }

    #[tokio::test]
    async fn scan_fails_closed_on_malformed_and_directory_mismatched_identities() {
        let d = tempfile::tempdir().unwrap();
        let malformed_dir = d.path().join("not-a-public-agent-id");
        std::fs::create_dir_all(&malformed_dir).unwrap();
        std::fs::write(
            malformed_dir.join(CONFIG_FILE),
            r#"{"public_agent_id":"not-a-public-agent-id","name":"bad"}"#,
        )
        .unwrap();

        let canonical = PublicAgentConfig::new("mismatch", 1);
        let wrong_dir = d.path().join(PublicAgentId::new().as_str());
        save_json_atomic(&wrong_dir, CONFIG_FILE, &canonical).unwrap();

        let r = reg(d.path());
        assert!(r.list().await.is_empty());
        assert!(!r.exists(&canonical.public_agent_id).await);
        assert!(r.health_error().await.is_some());
    }

    #[tokio::test]
    async fn patch_rejects_noncanonical_provider_and_knowledge_ids_atomically() {
        let d = tempfile::tempdir().unwrap();
        let r = reg(d.path());
        let a = r.create("A").await.unwrap();

        assert!(
            r.patch(
                &a.public_agent_id,
                serde_json::json!({"model":{"provider_id":"not-a-provider-id","model":"m"}}),
            )
            .await
            .is_err()
        );
        assert!(
            r.patch(
                &a.public_agent_id,
                serde_json::json!({"knowledge_base_ids":["not-a-knowledge-base-id"]}),
            )
                .await
                .is_err()
        );
        assert_eq!(r.get(&a.public_agent_id).await.unwrap(), a);
        assert_eq!(
            reg(d.path()).get(&a.public_agent_id).await.unwrap(),
            a
        );
    }

    #[tokio::test]
    async fn startup_provider_audit_fails_closed_on_orphan() {
        let d = tempfile::tempdir().unwrap();
        let provider_id = nomifun_common::ProviderId::new();
        let agent = PublicAgentConfig {
            model: Some(crate::config::PublicAgentModel {
                provider_id,
                model: "m".into(),
            }),
            ..PublicAgentConfig::new("orphan", 1)
        };
        save_json_atomic(
            &d.path().join(agent.public_agent_id.as_str()),
            CONFIG_FILE,
            &agent,
        )
        .unwrap();

        let db = init_database_memory().await.unwrap();
        let repo: Arc<dyn IProviderRepository> =
            Arc::new(SqliteProviderRepository::new(db.pool().clone()));
        let registry = PublicAgentRegistry::scan_with_provider_lifecycle(
            d.path().to_path_buf(),
            Some(repo),
            Some(Arc::new(ProviderLifecycleBarrier::new())),
        );

        assert!(
            registry
                .validate_provider_references_on_startup()
                .await
                .is_err()
        );
        assert!(registry.health_error().await.is_some());
        assert!(registry.list().await.is_empty());
    }

    #[tokio::test]
    async fn startup_provider_audit_accepts_existing_parent() {
        let d = tempfile::tempdir().unwrap();
        let db = init_database_memory().await.unwrap();
        let repo = Arc::new(SqliteProviderRepository::new(db.pool().clone()));
        let provider_id = nomifun_common::ProviderId::new();
        repo.create(CreateProviderParams {
            provider_id: Some(provider_id.as_str()),
            platform: "openai",
            name: "provider",
            base_url: "https://example.invalid",
            api_key_encrypted: "encrypted",
            models: r#"["m"]"#,
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
        let agent = PublicAgentConfig {
            model: Some(crate::config::PublicAgentModel {
                provider_id,
                model: "m".into(),
            }),
            ..PublicAgentConfig::new("valid", 1)
        };
        save_json_atomic(
            &d.path().join(agent.public_agent_id.as_str()),
            CONFIG_FILE,
            &agent,
        )
        .unwrap();

        let repo: Arc<dyn IProviderRepository> = repo;
        let registry = PublicAgentRegistry::scan_with_provider_lifecycle(
            d.path().to_path_buf(),
            Some(repo),
            Some(Arc::new(ProviderLifecycleBarrier::new())),
        );

        registry
            .validate_provider_references_on_startup()
            .await
            .unwrap();
        assert_eq!(registry.list().await, vec![agent]);
    }
}

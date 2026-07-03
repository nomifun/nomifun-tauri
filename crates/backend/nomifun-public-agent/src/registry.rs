//! The public-companion roster: atomic per-agent JSON files plus a registry-
//! private seq high-watermark so a deleted agent's short number is never reused.
//! Mirrors the shape of `nomifun-companion::registry` but is a wholly separate
//! store under `public-agents/`.

use std::collections::BTreeMap;
use std::path::PathBuf;

use nomifun_common::AppError;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::sync::RwLock;

use crate::config::PublicAgentConfig;
use crate::fsio::{load_json_or_default, save_json_atomic};

const CONFIG_FILE: &str = "config.json";
/// Registry-private seq watermark file (hidden, alongside the agent dirs).
const SEQ_STATE_FILE: &str = ".seq.json";

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
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
    agents: RwLock<BTreeMap<String, PublicAgentConfig>>,
    watermark: RwLock<u64>,
}

fn max_live_seq(agents: &BTreeMap<String, PublicAgentConfig>) -> u64 {
    agents.values().filter_map(|a| a.seq).max().unwrap_or(0)
}

impl PublicAgentRegistry {
    /// Scan `{data_dir}/public-agents/*/config.json` into memory and load the
    /// seq watermark. Corrupt / id-less configs are skipped.
    pub fn scan(dir: PathBuf) -> Self {
        let mut agents = BTreeMap::new();
        if let Ok(entries) = std::fs::read_dir(&dir) {
            for entry in entries.flatten() {
                if !entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                    continue;
                }
                let cfg_path = entry.path().join(CONFIG_FILE);
                let cfg: PublicAgentConfig = load_json_or_default(&cfg_path);
                if !cfg.id.trim().is_empty() {
                    agents.insert(cfg.id.clone(), cfg);
                }
            }
        }
        let seq_state: SeqState = load_json_or_default(&dir.join(SEQ_STATE_FILE));
        let watermark = seq_state.last_seq.max(max_live_seq(&agents));
        Self {
            dir,
            agents: RwLock::new(agents),
            watermark: RwLock::new(watermark),
        }
    }

    fn agent_dir(&self, id: &str) -> PathBuf {
        self.dir.join(id)
    }

    pub async fn list(&self) -> Vec<PublicAgentConfig> {
        let mut v: Vec<_> = self.agents.read().await.values().cloned().collect();
        // Newest-first by seq (then created_at) for a stable roster order.
        v.sort_by(|a, b| b.seq.cmp(&a.seq).then(b.created_at.cmp(&a.created_at)));
        v
    }

    pub async fn get(&self, id: &str) -> Option<PublicAgentConfig> {
        self.agents.read().await.get(id).cloned()
    }

    pub async fn exists(&self, id: &str) -> bool {
        self.agents.read().await.contains_key(id)
    }

    /// Allocate the next never-reused seq, persist `{id}/config.json`, insert.
    pub async fn create(&self, name: &str) -> Result<PublicAgentConfig, AppError> {
        let name = name.trim();
        if name.is_empty() {
            return Err(AppError::BadRequest("name must not be empty".into()));
        }
        // Lock order: watermark before the roster map.
        let mut watermark = self.watermark.write().await;
        let mut agents = self.agents.write().await;
        let seq = (*watermark).max(max_live_seq(&agents)) + 1;
        let mut cfg = PublicAgentConfig::new(name);
        cfg.seq = Some(seq);
        self.persist(&cfg)?;
        self.advance_watermark(&mut watermark, seq);
        agents.insert(cfg.id.clone(), cfg.clone());
        Ok(cfg)
    }

    /// RFC 7396 merge-patch over one agent's config. `id` / `seq` / `created_at`
    /// are immutable (stripped from the patch).
    pub async fn patch(&self, id: &str, mut patch: Value) -> Result<PublicAgentConfig, AppError> {
        if let Some(obj) = patch.as_object_mut() {
            obj.remove("id");
            obj.remove("seq");
            obj.remove("created_at");
        }
        let mut agents = self.agents.write().await;
        let cur = agents
            .get(id)
            .ok_or_else(|| AppError::NotFound(format!("public agent {id} not found")))?;
        let mut value = serde_json::to_value(cur).map_err(|e| AppError::Internal(e.to_string()))?;
        json_merge_patch(&mut value, &patch);
        let mut next: PublicAgentConfig =
            serde_json::from_value(value).map_err(|e| AppError::BadRequest(e.to_string()))?;
        // Preserve immutable identity regardless of a hostile patch.
        next.id = cur.id.clone();
        next.seq = cur.seq;
        next.created_at = cur.created_at;
        self.persist(&next)?;
        agents.insert(id.to_owned(), next.clone());
        Ok(next)
    }

    /// Remove an agent's config dir and drop it from the roster.
    pub async fn remove(&self, id: &str) -> Result<PublicAgentConfig, AppError> {
        let mut agents = self.agents.write().await;
        let removed = agents
            .remove(id)
            .ok_or_else(|| AppError::NotFound(format!("public agent {id} not found")))?;
        if let Err(e) = std::fs::remove_dir_all(self.agent_dir(id)) {
            tracing::warn!(error = %e, id, "remove public-agent dir failed (roster entry dropped)");
        }
        Ok(removed)
    }

    fn persist(&self, cfg: &PublicAgentConfig) -> Result<(), AppError> {
        save_json_atomic(&self.agent_dir(&cfg.id), CONFIG_FILE, cfg)
            .map_err(|e| AppError::Internal(format!("persist public agent: {e}")))
    }

    fn advance_watermark(&self, watermark: &mut u64, seq: u64) {
        if seq <= *watermark {
            return;
        }
        *watermark = seq;
        if let Err(e) = save_json_atomic(&self.dir, SEQ_STATE_FILE, &SeqState { last_seq: seq }) {
            tracing::warn!(error = %e, "save public-agent seq watermark failed");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reg(dir: &std::path::Path) -> PublicAgentRegistry {
        PublicAgentRegistry::scan(dir.to_path_buf())
    }

    #[tokio::test]
    async fn create_get_patch_remove_roundtrip() {
        let d = tempfile::tempdir().unwrap();
        let r = reg(d.path());
        let a = r.create("甲").await.unwrap();
        assert_eq!(a.seq, Some(1));
        assert!(r.exists(&a.id).await);

        let patched = r
            .patch(&a.id, serde_json::json!({ "name": "甲队", "grounded_mode": false }))
            .await
            .unwrap();
        assert_eq!(patched.name, "甲队");
        assert!(!patched.grounded_mode);
        assert_eq!(patched.id, a.id, "id immutable");
        assert_eq!(patched.seq, Some(1), "seq immutable");

        // Persisted: a fresh scan sees the patch.
        let r2 = reg(d.path());
        assert_eq!(r2.get(&a.id).await.unwrap().name, "甲队");

        r.remove(&a.id).await.unwrap();
        assert!(!r.exists(&a.id).await);
    }

    #[tokio::test]
    async fn seq_is_never_reused_after_delete() {
        let d = tempfile::tempdir().unwrap();
        let r = reg(d.path());
        let a = r.create("A").await.unwrap();
        let b = r.create("B").await.unwrap();
        assert_eq!(a.seq, Some(1));
        assert_eq!(b.seq, Some(2));
        r.remove(&b.id).await.unwrap();
        let c = r.create("C").await.unwrap();
        assert_eq!(c.seq, Some(3), "deleted #2 must not be reused");
    }

    #[tokio::test]
    async fn patch_cannot_forge_identity() {
        let d = tempfile::tempdir().unwrap();
        let r = reg(d.path());
        let a = r.create("A").await.unwrap();
        let patched = r
            .patch(&a.id, serde_json::json!({ "id": "pubagent_evil", "seq": 999, "name": "A2" }))
            .await
            .unwrap();
        assert_eq!(patched.id, a.id);
        assert_eq!(patched.seq, Some(1));
        assert_eq!(patched.name, "A2");
    }
}

//! `CompanionSkillStoreSink` — bridges the companion's skill registry + on-disk
//! SKILL.md bodies to the `nomifun_ai_agent::CompanionSkillSink` trait the agent
//! engine consumes for skill auto-use (design §7).
//!
//! `active_skills` feeds the per-turn `when_to_use` index (the `CompanionSkillContributor`);
//! `load_skill_body` resolves a named skill's SKILL.md on demand (the `companion_skill` tool).
//! Both scope to the default companion (the owner of mined skills) plus shared skills.

use std::sync::Arc;

use async_trait::async_trait;
use nomifun_ai_agent::{CompanionSkillSink, SkillListing};
use nomifun_extension::constants::SKILL_MANIFEST_FILE;
use nomifun_extension::skill_service::{self, SkillPaths, SkillScope};

use crate::collector::SharedConfig;
use crate::store::CompanionStore;

pub struct CompanionSkillStoreSink {
    pub store: CompanionStore,
    pub config: SharedConfig,
    pub skill_paths: Arc<SkillPaths>,
}

impl CompanionSkillStoreSink {
    /// The companion that owns mined skills (default companion).
    async fn owner(&self) -> Option<String> {
        self.config.read().await.default_companion_id.clone()
    }

    fn scope_of(companion_id: Option<&str>) -> SkillScope {
        companion_id
            .map(|id| SkillScope::Companion(id.to_owned()))
            .unwrap_or(SkillScope::Shared)
    }
}

#[async_trait]
impl CompanionSkillSink for CompanionSkillStoreSink {
    async fn active_skills(&self) -> Vec<SkillListing> {
        let Some(owner) = self.owner().await else {
            return Vec::new();
        };
        let skills = self.store.list_skills(&owner, true).await.unwrap_or_default();
        let mut out = Vec::new();
        for s in skills.into_iter().filter(|s| s.status == "active") {
            let scope = Self::scope_of(s.scope_companion_id.as_deref());
            // when_to_use index uses the SKILL.md description (what the skill does).
            if let Ok(dir) = skill_service::skill_dir_for(&self.skill_paths, &scope, &s.skill_name, false) {
                if let Ok((_, desc)) = skill_service::read_skill_info(&dir).await {
                    out.push(SkillListing { name: s.skill_name, when_to_use: desc });
                }
            }
        }
        out
    }

    async fn load_skill_body(&self, name: &str) -> Option<String> {
        let owner = self.owner().await;
        // Prefer the owner's companion-scoped skill (record usage against the owner),
        // then fall back to the ownerless shared scope.
        if let Some(owner) = owner {
            if let Ok(dir) = skill_service::skill_dir_for(&self.skill_paths, &SkillScope::Companion(owner.clone()), name, false) {
                if let Ok(body) = tokio::fs::read_to_string(dir.join(SKILL_MANIFEST_FILE)).await {
                    let _ = self
                        .store
                        .record_skill_usage_by_name(
                            Some(&owner),
                            name,
                            nomifun_common::now_ms(),
                        )
                        .await;
                    return Some(body);
                }
            }
        }
        if let Ok(dir) = skill_service::skill_dir_for(&self.skill_paths, &SkillScope::Shared, name, false) {
            if let Ok(body) = tokio::fs::read_to_string(dir.join(SKILL_MANIFEST_FILE)).await {
                let _ = self
                    .store
                    .record_skill_usage_by_name(None, name, nomifun_common::now_ms())
                    .await;
                return Some(body);
            }
        }
        None
    }
}

//! `PublicAgentProvider` impl: bridges the public-agent domain into the agent
//! factory's runtime. The factory (nomifun-ai-agent) owns the trait + the small
//! `PublicAgentRuntime` DTO; this crate depends on the factory (never the
//! reverse — no cycle), exactly like `nomifun-companion` does for its persona
//! provider.
//!
//! A public-agent session is clamped to `PublicService` by the factory purely
//! from the presence of `extra.public_agent_id` (fail-safe). This provider only
//! supplies the LIVE persona / policy / grounded flag / bound-KB set / model, so
//! a deleted or unresolvable agent still yields a hard-clamped, persona-less
//! session.

use async_trait::async_trait;
use nomifun_ai_agent::{PublicAgentProvider, PublicAgentRuntime};
use nomifun_common::ProviderWithModel;

use crate::config::PublicAgentConfig;
use crate::service::PublicAgentService;

impl PublicAgentConfig {
    /// Project the persisted config into the runtime DTO the factory consumes.
    fn to_runtime(&self) -> PublicAgentRuntime {
        PublicAgentRuntime {
            name: self.name.clone(),
            greeting: self.greeting.clone(),
            tone: self.tone.clone(),
            service_policy: self.service_policy.clone(),
            grounded_mode: self.grounded_mode,
            knowledge_base_ids: self.knowledge_base_ids.clone(),
            model: ProviderWithModel {
                provider_id: self.model.provider_id.clone(),
                model: self.model.model.clone(),
                use_model: self.model.use_model.clone(),
            },
        }
    }
}

#[async_trait]
impl PublicAgentProvider for PublicAgentService {
    async fn resolve_public_agent(&self, id: &str) -> Option<PublicAgentRuntime> {
        // A disabled agent still resolves (so the owner can preview it); the
        // channel layer decides whether to serve. Unknown id → None.
        self.get(id).await.ok().map(|cfg| cfg.to_runtime())
    }

    async fn record_public_agent_turn(
        &self,
        id: &str,
        surface: &str,
        platform: Option<&str>,
        text: &str,
    ) {
        // Best-effort audit (never fails the turn).
        self.record_turn(id, surface, platform, text).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn resolve_maps_config_to_runtime() {
        let d = tempfile::tempdir().unwrap();
        let svc = PublicAgentService::start(d.path());
        let a = svc.create("客服台").await.unwrap();
        svc.patch(
            &a.id,
            serde_json::json!({
                "greeting": "您好",
                "grounded_mode": true,
                "knowledge_base_ids": ["kb_1", "kb_2"],
                "model": { "provider_id": "prov", "model": "m", "use_model": "m-2" }
            }),
        )
        .await
        .unwrap();

        let rt = PublicAgentProvider::resolve_public_agent(&*svc, &a.id).await.unwrap();
        assert_eq!(rt.name, "客服台");
        assert_eq!(rt.greeting, "您好");
        assert!(rt.grounded_mode);
        assert_eq!(rt.knowledge_base_ids, vec!["kb_1".to_string(), "kb_2".to_string()]);
        assert_eq!(rt.model.use_model.as_deref(), Some("m-2"));

        // Unknown id → None.
        assert!(PublicAgentProvider::resolve_public_agent(&*svc, "pubagent_nope").await.is_none());
    }
}

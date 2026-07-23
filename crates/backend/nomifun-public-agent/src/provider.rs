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
    fn to_runtime(&self) -> Option<PublicAgentRuntime> {
        let model = self.model.as_ref()?;
        let provider_id = model.provider_id.to_string();
        Some(PublicAgentRuntime {
            name: self.name.clone(),
            greeting: self.greeting.clone(),
            tone: self.tone.clone(),
            preset_instructions: self
                .applied_preset
                .as_ref()
                .map(|snapshot| snapshot.instructions.clone())
                .unwrap_or_default(),
            service_policy: self.service_policy.clone(),
            grounded_mode: self.grounded_mode,
            knowledge_base_ids: self.knowledge_base_ids.clone(),
            model: ProviderWithModel {
                provider_id,
                model: model.model.clone(),
                use_model: None,
            },
        })
    }
}

#[async_trait]
impl PublicAgentProvider for PublicAgentService {
    async fn resolve_public_agent(
        &self,
        public_agent_id: &str,
    ) -> Option<PublicAgentRuntime> {
        // A disabled agent still resolves (so the owner can preview it); the
        // channel layer decides whether to serve. Unknown id → None.
        self.get(public_agent_id)
            .await
            .ok()
            .and_then(|cfg| cfg.to_runtime())
    }

    async fn record_public_agent_turn(
        &self,
        public_agent_id: &str,
        surface: &str,
        platform: Option<&str>,
        text: &str,
    ) {
        // Best-effort audit (never fails the turn).
        self.record_turn(public_agent_id, surface, platform, text)
            .await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nomifun_common::{KnowledgeBaseId, ProviderId};

    #[tokio::test]
    async fn resolve_maps_config_to_runtime() {
        let d = tempfile::tempdir().unwrap();
        let svc = PublicAgentService::start(d.path());
        let a = svc.create("客服台").await.unwrap();
        let first_kb = KnowledgeBaseId::new();
        let second_kb = KnowledgeBaseId::new();
        let provider_id = ProviderId::new();
        svc.patch(
            a.public_agent_id.as_str(),
            serde_json::json!({
                "greeting": "您好",
                "grounded_mode": true,
                "knowledge_base_ids": [first_kb, second_kb],
                "model": { "provider_id": provider_id, "model": "m" }
            }),
        )
        .await
        .unwrap();

        let rt = PublicAgentProvider::resolve_public_agent(
            &*svc,
            a.public_agent_id.as_str(),
        )
        .await
        .unwrap();
        assert_eq!(rt.name, "客服台");
        assert_eq!(rt.greeting, "您好");
        assert!(rt.grounded_mode);
        assert_eq!(rt.knowledge_base_ids, vec![first_kb, second_kb]);
        assert_eq!(rt.model.provider_id, provider_id.to_string());
        assert_eq!(rt.model.use_model, None);

        // Unknown id → None.
        assert!(
            PublicAgentProvider::resolve_public_agent(&*svc, "not-a-public-agent-id")
                .await
                .is_none()
        );
    }

    #[tokio::test]
    async fn unconfigured_agent_does_not_resolve_to_empty_provider_id() {
        let d = tempfile::tempdir().unwrap();
        let svc = PublicAgentService::start(d.path());
        let agent = svc.create("未配置").await.unwrap();
        assert!(
            PublicAgentProvider::resolve_public_agent(
                &*svc,
                agent.public_agent_id.as_str(),
            )
                .await
                .is_none()
        );
    }
}

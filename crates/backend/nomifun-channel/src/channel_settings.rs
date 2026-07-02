use std::sync::Arc;

use nomifun_common::ProviderWithModel;
use nomifun_db::IClientPreferenceRepository;
use tracing::debug;

use crate::error::ChannelError;
use crate::types::PluginType;

const DEFAULT_BACKEND: &str = "nomi";
const DEFAULT_AGENT_TYPE: &str = "nomi";

/// Per-plugin agent/model configuration read from `client_preferences`.
///
/// Keys follow the pattern established by the old Electron frontend:
/// - `assistant.{platform}.agent`       → JSON `{"backend":"claude","name":"Claude"}`
/// - `assistant.{platform}.defaultModel` → JSON `{"id":"provider_id","use_model":"model_name"}`
pub struct ChannelSettingsService {
    pref_repo: Arc<dyn IClientPreferenceRepository>,
}

/// Resolved agent configuration for a channel platform.
///
/// `backend` is only meaningful for ACP agents (claude, gemini, codex, …).
/// Non-ACP agent types (nomi, nanobot, remote, …) have `backend = None`.
#[derive(Debug, Clone)]
pub struct ResolvedAgentConfig {
    pub agent_type: String,
    pub backend: Option<String>,
}

/// Resolved model configuration for a channel platform.
#[derive(Debug, Clone)]
pub struct ResolvedModelConfig {
    pub provider_id: String,
    pub model: String,
    pub use_model: Option<String>,
}

impl ChannelSettingsService {
    pub fn new(pref_repo: Arc<dyn IClientPreferenceRepository>) -> Self {
        Self { pref_repo }
    }

    /// Reads the agent configuration for a platform from `client_preferences`.
    ///
    /// Supports two data formats:
    /// - **New:** `{"agent_type":"acp","backend":"claude","name":"Claude"}`
    /// - **Legacy:** `{"backend":"claude","name":"Claude"}` (no agent_type field)
    ///
    /// Falls back to `agent_type=nomi, backend=None` when no config exists.
    pub async fn get_agent_config(&self, platform: PluginType) -> Result<ResolvedAgentConfig, ChannelError> {
        let key = agent_key(platform);
        let prefs = self.pref_repo.get_by_keys(&[&key]).await?;

        let Some(pref) = prefs.into_iter().next() else {
            return Ok(default_agent_config());
        };

        let parsed: serde_json::Value = serde_json::from_str(&pref.value).unwrap_or_default();

        if let Some(at) = parsed["agent_type"].as_str() {
            let backend = if at == "acp" {
                parsed["backend"].as_str().map(|s| s.to_owned())
            } else {
                None
            };

            debug!(platform = %platform, agent_type = %at, backend = ?backend, "resolved channel agent config (new format)");

            return Ok(ResolvedAgentConfig {
                agent_type: at.to_owned(),
                backend,
            });
        }

        let raw_backend = parsed["backend"].as_str().unwrap_or(DEFAULT_BACKEND).to_owned();
        let agent_type = backend_to_agent_type(&raw_backend);
        let backend = if agent_type == "acp" { Some(raw_backend) } else { None };

        debug!(platform = %platform, agent_type = %agent_type, backend = ?backend, "resolved channel agent config (legacy format)");

        Ok(ResolvedAgentConfig { agent_type, backend })
    }

    /// Opt-in: whether inbound messages on this platform should be filed as
    /// tracked requirements (IM → requirement pipeline) instead of getting an
    /// immediate AI reply. Default false (absent key → off).
    pub async fn get_route_to_requirement(&self, platform: PluginType) -> Result<bool, ChannelError> {
        let key = route_to_requirement_key(platform);
        let prefs = self.pref_repo.get_by_keys(&[&key]).await?;
        Ok(prefs
            .into_iter()
            .next()
            .map(|p| {
                let v = p.value.trim();
                v == "true" || serde_json::from_str::<bool>(v).unwrap_or(false)
            })
            .unwrap_or(false))
    }

    /// The board tag (column) under which IM-routed requirements are filed.
    /// Defaults to "inbox" when unset.
    pub async fn get_requirement_tag(&self, platform: PluginType) -> Result<String, ChannelError> {
        let key = requirement_tag_key(platform);
        let prefs = self.pref_repo.get_by_keys(&[&key]).await?;
        Ok(prefs
            .into_iter()
            .next()
            .map(|p| p.value.trim().trim_matches('"').to_string())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "inbox".to_string()))
    }

    /// Reads the model configuration for a platform from `client_preferences`.
    ///
    /// Returns `None` when no model is configured (common for ACP agents).
    pub async fn get_model_config(&self, platform: PluginType) -> Result<Option<ResolvedModelConfig>, ChannelError> {
        let key = model_key(platform);
        let prefs = self.pref_repo.get_by_keys(&[&key]).await?;

        let Some(pref) = prefs.into_iter().next() else {
            return Ok(None);
        };

        let parsed: serde_json::Value = serde_json::from_str(&pref.value).unwrap_or_default();

        let provider_id = parsed["id"].as_str().unwrap_or_default().to_owned();
        let use_model = parsed["use_model"].as_str().map(|s| s.to_owned());

        if provider_id.is_empty() && use_model.is_none() {
            return Ok(None);
        }

        debug!(platform = %platform, provider_id = %provider_id, use_model = ?use_model, "resolved channel model config");

        Ok(Some(ResolvedModelConfig {
            provider_id: provider_id.clone(),
            model: use_model.clone().unwrap_or_default(),
            use_model,
        }))
    }
    /// Reads the companion bound to a platform's master agent from
    /// `client_preferences` (key `assistant.{platform}.companionId`).
    ///
    /// Returns `None` when the key is absent or stores an empty string —
    /// "no explicit binding, use the default companion". Tolerates both a JSON
    /// string (`"companion_x"`) and a raw unquoted value.
    pub async fn get_master_agent_companion_id(&self, platform: PluginType) -> Result<Option<String>, ChannelError> {
        let key = master_agent_companion_key(platform);
        let prefs = self.pref_repo.get_by_keys(&[&key]).await?;

        let Some(pref) = prefs.into_iter().next() else {
            return Ok(None);
        };

        let raw = match serde_json::from_str::<serde_json::Value>(&pref.value) {
            Ok(serde_json::Value::String(s)) => s,
            Ok(serde_json::Value::Null) => String::new(),
            Ok(_) => pref.value,
            Err(_) => pref.value,
        };
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            return Ok(None);
        }
        Ok(Some(trimmed.to_owned()))
    }

    /// Writes (or clears) the companion bound to a platform's master agent
    /// (key `assistant.{platform}.companionId`). `None` / empty string deletes the
    /// key — back to "default companion" semantics.
    ///
    /// MUTUAL EXCLUSIVITY: a platform bot serves EITHER a companion OR a public
    /// agent, never both — so writing a companion binding also clears that
    /// platform's public-agent binding (see `set_master_agent_public_agent_id`
    /// for the symmetric clear).
    ///
    /// Persistence only: the channel routes layer pairs this with the same
    /// session reset as the masterAgent switch so the next inbound message
    /// recreates conversations under the new binding.
    pub async fn set_master_agent_companion_id(
        &self,
        platform: PluginType,
        companion_id: Option<&str>,
    ) -> Result<(), ChannelError> {
        let key = master_agent_companion_key(platform);
        match companion_id.map(str::trim).filter(|s| !s.is_empty()) {
            Some(id) => {
                let value = serde_json::Value::String(id.to_owned()).to_string();
                self.pref_repo.upsert_batch(&[(&key, &value)]).await?;
                // Mutual exclusivity: binding a companion drops any public-agent
                // binding on this platform.
                self.pref_repo
                    .delete_keys(&[&master_agent_public_agent_key(platform)])
                    .await?;
            }
            None => {
                self.pref_repo.delete_keys(&[&key]).await?;
            }
        }
        Ok(())
    }

    /// Reads the 对外伙伴 (public agent) bound to a platform's master agent from
    /// `client_preferences` (key `assistant.{platform}.publicAgentId`). Mirrors
    /// [`Self::get_master_agent_companion_id`]: `None` when the key is absent or
    /// stores an empty string; tolerates both a JSON string (`"pubagent_x"`) and a
    /// raw unquoted value.
    pub async fn get_master_agent_public_agent_id(
        &self,
        platform: PluginType,
    ) -> Result<Option<String>, ChannelError> {
        let key = master_agent_public_agent_key(platform);
        let prefs = self.pref_repo.get_by_keys(&[&key]).await?;

        let Some(pref) = prefs.into_iter().next() else {
            return Ok(None);
        };

        let raw = match serde_json::from_str::<serde_json::Value>(&pref.value) {
            Ok(serde_json::Value::String(s)) => s,
            Ok(serde_json::Value::Null) => String::new(),
            Ok(_) => pref.value,
            Err(_) => pref.value,
        };
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            return Ok(None);
        }
        Ok(Some(trimmed.to_owned()))
    }

    /// Writes (or clears) the 对外伙伴 (public agent) bound to a platform's master
    /// agent (key `assistant.{platform}.publicAgentId`). `None` / empty string
    /// deletes the key.
    ///
    /// MUTUAL EXCLUSIVITY: a platform bot serves EITHER a public agent OR a
    /// companion, never both — so writing a public-agent binding also clears that
    /// platform's companion binding (the symmetric clear lives in
    /// [`Self::set_master_agent_companion_id`]).
    pub async fn set_master_agent_public_agent_id(
        &self,
        platform: PluginType,
        public_agent_id: Option<&str>,
    ) -> Result<(), ChannelError> {
        let key = master_agent_public_agent_key(platform);
        match public_agent_id.map(str::trim).filter(|s| !s.is_empty()) {
            Some(id) => {
                let value = serde_json::Value::String(id.to_owned()).to_string();
                self.pref_repo.upsert_batch(&[(&key, &value)]).await?;
                // Mutual exclusivity: binding a public agent drops any companion
                // binding on this platform.
                self.pref_repo
                    .delete_keys(&[&master_agent_companion_key(platform)])
                    .await?;
            }
            None => {
                self.pref_repo.delete_keys(&[&key]).await?;
            }
        }
        Ok(())
    }
}

fn agent_key(platform: PluginType) -> String {
    format!("assistant.{platform}.agent")
}

fn master_agent_companion_key(platform: PluginType) -> String {
    format!("assistant.{platform}.companionId")
}

fn master_agent_public_agent_key(platform: PluginType) -> String {
    format!("assistant.{platform}.publicAgentId")
}

fn model_key(platform: PluginType) -> String {
    format!("assistant.{platform}.defaultModel")
}

fn route_to_requirement_key(platform: PluginType) -> String {
    format!("assistant.{platform}.routeToRequirement")
}

fn requirement_tag_key(platform: PluginType) -> String {
    format!("assistant.{platform}.requirementTag")
}

fn default_agent_config() -> ResolvedAgentConfig {
    ResolvedAgentConfig {
        agent_type: DEFAULT_AGENT_TYPE.to_owned(),
        backend: None,
    }
}

/// Maps a backend identifier to the corresponding `AgentType` serde name.
///
/// ACP-style backends (claude, gemini, codex, etc.) all map to "acp".
/// Non-ACP backends map to their specific agent type.
fn backend_to_agent_type(backend: &str) -> String {
    match backend {
        "nomi" | "nomi-cli" => "nomi".to_owned(),
        "openclaw-gateway" => "openclaw-gateway".to_owned(),
        "nanobot" => "nanobot".to_owned(),
        "remote" => "remote".to_owned(),
        _ => {
            // All ACP-compatible backends: claude, gemini, codex, codebuddy, opencode, qwen, copilot, droid, kimi, etc.
            "acp".to_owned()
        }
    }
}

/// Builds a `ProviderWithModel` from the resolved config, or returns
/// the empty default when no model is configured.
pub fn resolved_model_to_provider(model: Option<&ResolvedModelConfig>) -> ProviderWithModel {
    match model {
        Some(m) => ProviderWithModel {
            provider_id: m.provider_id.clone(),
            model: m.model.clone(),
            use_model: m.use_model.clone(),
        },
        None => ProviderWithModel {
            provider_id: String::new(),
            model: String::new(),
            use_model: None,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nomifun_db::DbError;
    use nomifun_db::models::ClientPreference;
    use std::sync::Mutex;

    struct MockPrefRepo {
        data: Mutex<Vec<(String, String)>>,
    }

    impl MockPrefRepo {
        fn new() -> Self {
            Self {
                data: Mutex::new(Vec::new()),
            }
        }

        fn with_data(entries: Vec<(&str, &str)>) -> Self {
            Self {
                data: Mutex::new(entries.into_iter().map(|(k, v)| (k.to_owned(), v.to_owned())).collect()),
            }
        }
    }

    #[async_trait::async_trait]
    impl IClientPreferenceRepository for MockPrefRepo {
        async fn get_all(&self) -> Result<Vec<ClientPreference>, DbError> {
            let data = self.data.lock().unwrap();
            Ok(data
                .iter()
                .map(|(k, v)| ClientPreference {
                    key: k.clone(),
                    value: v.clone(),
                    updated_at: 0,
                })
                .collect())
        }

        async fn get_by_keys(&self, keys: &[&str]) -> Result<Vec<ClientPreference>, DbError> {
            let data = self.data.lock().unwrap();
            Ok(data
                .iter()
                .filter(|(k, _)| keys.contains(&k.as_str()))
                .map(|(k, v)| ClientPreference {
                    key: k.clone(),
                    value: v.clone(),
                    updated_at: 0,
                })
                .collect())
        }

        async fn upsert_batch(&self, entries: &[(&str, &str)]) -> Result<(), DbError> {
            let mut data = self.data.lock().unwrap();
            for (key, value) in entries {
                if let Some(existing) = data.iter_mut().find(|(k, _)| k == key) {
                    existing.1 = value.to_string();
                } else {
                    data.push((key.to_string(), value.to_string()));
                }
            }
            Ok(())
        }

        async fn delete_keys(&self, keys: &[&str]) -> Result<(), DbError> {
            let mut data = self.data.lock().unwrap();
            data.retain(|(k, _)| !keys.contains(&k.as_str()));
            Ok(())
        }
    }

    // ── backend_to_agent_type ─────────────────────────────────────────

    #[test]
    fn acp_backends_map_to_acp() {
        for backend in &[
            "claude",
            "gemini",
            "codex",
            "codebuddy",
            "opencode",
            "qwen",
            "copilot",
            "droid",
            "kimi",
        ] {
            assert_eq!(backend_to_agent_type(backend), "acp", "backend: {backend}");
        }
    }

    #[test]
    fn nomi_backends_map_to_nomi() {
        assert_eq!(backend_to_agent_type("nomi"), "nomi");
        assert_eq!(backend_to_agent_type("nomi-cli"), "nomi");
    }

    #[test]
    fn non_acp_backends_map_correctly() {
        assert_eq!(backend_to_agent_type("openclaw-gateway"), "openclaw-gateway");
        assert_eq!(backend_to_agent_type("nanobot"), "nanobot");
        assert_eq!(backend_to_agent_type("remote"), "remote");
    }

    #[test]
    fn unknown_backend_defaults_to_acp() {
        assert_eq!(backend_to_agent_type("unknown"), "acp");
    }

    // ── get_agent_config ──────────────────────────────────────────────

    #[tokio::test]
    async fn agent_config_returns_default_when_no_pref() {
        let repo = Arc::new(MockPrefRepo::new());
        let svc = ChannelSettingsService::new(repo);

        let config = svc.get_agent_config(PluginType::Telegram).await.unwrap();
        assert_eq!(config.agent_type, "nomi");
        assert!(config.backend.is_none());
    }

    #[tokio::test]
    async fn agent_config_reads_acp_from_preferences() {
        let repo = Arc::new(MockPrefRepo::with_data(vec![(
            "assistant.telegram.agent",
            r#"{"backend":"codex","name":"Codex"}"#,
        )]));
        let svc = ChannelSettingsService::new(repo);

        let config = svc.get_agent_config(PluginType::Telegram).await.unwrap();
        assert_eq!(config.agent_type, "acp");
        assert_eq!(config.backend.as_deref(), Some("codex"));
    }

    #[tokio::test]
    async fn agent_config_nomi_has_no_backend() {
        let repo = Arc::new(MockPrefRepo::with_data(vec![(
            "assistant.lark.agent",
            r#"{"backend":"nomi","name":"Nomi"}"#,
        )]));
        let svc = ChannelSettingsService::new(repo);

        let config = svc.get_agent_config(PluginType::Lark).await.unwrap();
        assert_eq!(config.agent_type, "nomi");
        assert!(config.backend.is_none());
    }

    // ── get_agent_config (new format) ──────────────────────────────────

    #[tokio::test]
    async fn agent_config_reads_new_format_acp() {
        let repo = Arc::new(MockPrefRepo::with_data(vec![(
            "assistant.telegram.agent",
            r#"{"agent_type":"acp","backend":"claude","name":"Claude"}"#,
        )]));
        let svc = ChannelSettingsService::new(repo);

        let config = svc.get_agent_config(PluginType::Telegram).await.unwrap();
        assert_eq!(config.agent_type, "acp");
        assert_eq!(config.backend.as_deref(), Some("claude"));
    }

    #[tokio::test]
    async fn agent_config_reads_new_format_nomi() {
        let repo = Arc::new(MockPrefRepo::with_data(vec![(
            "assistant.lark.agent",
            r#"{"agent_type":"nomi","name":"Nomi"}"#,
        )]));
        let svc = ChannelSettingsService::new(repo);

        let config = svc.get_agent_config(PluginType::Lark).await.unwrap();
        assert_eq!(config.agent_type, "nomi");
        assert!(config.backend.is_none());
    }

    #[tokio::test]
    async fn agent_config_reads_new_format_openclaw() {
        let repo = Arc::new(MockPrefRepo::with_data(vec![(
            "assistant.weixin.agent",
            r#"{"agent_type":"openclaw-gateway","name":"OpenClaw"}"#,
        )]));
        let svc = ChannelSettingsService::new(repo);

        let config = svc.get_agent_config(PluginType::Weixin).await.unwrap();
        assert_eq!(config.agent_type, "openclaw-gateway");
        assert!(config.backend.is_none());
    }

    // ── get_model_config ──────────────────────────────────────────────

    #[tokio::test]
    async fn model_config_returns_none_when_no_pref() {
        let repo = Arc::new(MockPrefRepo::new());
        let svc = ChannelSettingsService::new(repo);

        let config = svc.get_model_config(PluginType::Telegram).await.unwrap();
        assert!(config.is_none());
    }

    #[tokio::test]
    async fn model_config_reads_from_preferences() {
        let repo = Arc::new(MockPrefRepo::with_data(vec![(
            "assistant.weixin.defaultModel",
            r#"{"id":"490fdb4e","use_model":"global.anthropic.claude-opus-4-6-v1"}"#,
        )]));
        let svc = ChannelSettingsService::new(repo);

        let config = svc.get_model_config(PluginType::Weixin).await.unwrap().unwrap();
        assert_eq!(config.provider_id, "490fdb4e");
        assert_eq!(config.use_model.as_deref(), Some("global.anthropic.claude-opus-4-6-v1"));
    }

    #[tokio::test]
    async fn model_config_returns_none_for_empty_values() {
        let repo = Arc::new(MockPrefRepo::with_data(vec![(
            "assistant.telegram.defaultModel",
            r#"{"id":"","use_model":null}"#,
        )]));
        let svc = ChannelSettingsService::new(repo);

        let config = svc.get_model_config(PluginType::Telegram).await.unwrap();
        assert!(config.is_none());
    }

    // ── get/set_master_agent_companion_id ───────────────────────────────────

    #[tokio::test]
    async fn companion_id_returns_none_when_no_pref() {
        let repo = Arc::new(MockPrefRepo::new());
        let svc = ChannelSettingsService::new(repo);
        assert!(svc.get_master_agent_companion_id(PluginType::Telegram).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn companion_id_reads_json_string_value() {
        let repo = Arc::new(MockPrefRepo::with_data(vec![(
            "assistant.telegram.companionId",
            "\"companion_abc\"",
        )]));
        let svc = ChannelSettingsService::new(repo);
        assert_eq!(
            svc.get_master_agent_companion_id(PluginType::Telegram).await.unwrap().as_deref(),
            Some("companion_abc")
        );
    }

    #[tokio::test]
    async fn companion_id_reads_raw_unquoted_value() {
        let repo = Arc::new(MockPrefRepo::with_data(vec![("assistant.lark.companionId", "companion_raw")]));
        let svc = ChannelSettingsService::new(repo);
        assert_eq!(
            svc.get_master_agent_companion_id(PluginType::Lark).await.unwrap().as_deref(),
            Some("companion_raw")
        );
    }

    #[tokio::test]
    async fn companion_id_empty_string_treated_as_unset() {
        let repo = Arc::new(MockPrefRepo::with_data(vec![
            ("assistant.telegram.companionId", "\"\""),
            ("assistant.lark.companionId", "\"  \""),
            ("assistant.weixin.companionId", "null"),
        ]));
        let svc = ChannelSettingsService::new(repo);
        assert!(svc.get_master_agent_companion_id(PluginType::Telegram).await.unwrap().is_none());
        assert!(svc.get_master_agent_companion_id(PluginType::Lark).await.unwrap().is_none());
        assert!(svc.get_master_agent_companion_id(PluginType::Weixin).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn companion_id_set_then_get_roundtrip_and_clear() {
        let repo = Arc::new(MockPrefRepo::new());
        let svc = ChannelSettingsService::new(repo);

        svc.set_master_agent_companion_id(PluginType::Dingtalk, Some("companion_77"))
            .await
            .unwrap();
        assert_eq!(
            svc.get_master_agent_companion_id(PluginType::Dingtalk).await.unwrap().as_deref(),
            Some("companion_77")
        );
        // Other platforms unaffected.
        assert!(svc.get_master_agent_companion_id(PluginType::Telegram).await.unwrap().is_none());

        // None clears the binding (key deleted).
        svc.set_master_agent_companion_id(PluginType::Dingtalk, None).await.unwrap();
        assert!(svc.get_master_agent_companion_id(PluginType::Dingtalk).await.unwrap().is_none());

        // Empty string also clears.
        svc.set_master_agent_companion_id(PluginType::Dingtalk, Some("companion_88"))
            .await
            .unwrap();
        svc.set_master_agent_companion_id(PluginType::Dingtalk, Some("  "))
            .await
            .unwrap();
        assert!(svc.get_master_agent_companion_id(PluginType::Dingtalk).await.unwrap().is_none());
    }

    // ── get/set_master_agent_public_agent_id + mutual exclusivity ───────

    #[tokio::test]
    async fn public_agent_id_returns_none_when_no_pref() {
        let repo = Arc::new(MockPrefRepo::new());
        let svc = ChannelSettingsService::new(repo);
        assert!(
            svc.get_master_agent_public_agent_id(PluginType::Telegram)
                .await
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    async fn public_agent_id_set_get_roundtrip_and_clear() {
        let repo = Arc::new(MockPrefRepo::new());
        let svc = ChannelSettingsService::new(repo);

        svc.set_master_agent_public_agent_id(PluginType::Telegram, Some("pubagent_1"))
            .await
            .unwrap();
        assert_eq!(
            svc.get_master_agent_public_agent_id(PluginType::Telegram)
                .await
                .unwrap()
                .as_deref(),
            Some("pubagent_1")
        );

        // None clears the binding.
        svc.set_master_agent_public_agent_id(PluginType::Telegram, None)
            .await
            .unwrap();
        assert!(
            svc.get_master_agent_public_agent_id(PluginType::Telegram)
                .await
                .unwrap()
                .is_none()
        );
    }

    /// A platform bot serves EITHER a companion OR a public agent, never both:
    /// setting one binding clears the other, in both directions.
    #[tokio::test]
    async fn companion_and_public_agent_bindings_are_mutually_exclusive() {
        let repo = Arc::new(MockPrefRepo::new());
        let svc = ChannelSettingsService::new(repo);

        // Companion first.
        svc.set_master_agent_companion_id(PluginType::Telegram, Some("companion_1"))
            .await
            .unwrap();
        assert_eq!(
            svc.get_master_agent_companion_id(PluginType::Telegram).await.unwrap().as_deref(),
            Some("companion_1")
        );

        // Binding a public agent clears the companion binding.
        svc.set_master_agent_public_agent_id(PluginType::Telegram, Some("pubagent_1"))
            .await
            .unwrap();
        assert_eq!(
            svc.get_master_agent_public_agent_id(PluginType::Telegram)
                .await
                .unwrap()
                .as_deref(),
            Some("pubagent_1")
        );
        assert!(
            svc.get_master_agent_companion_id(PluginType::Telegram).await.unwrap().is_none(),
            "setting a public agent must clear the companion binding"
        );

        // Binding a companion again clears the public-agent binding.
        svc.set_master_agent_companion_id(PluginType::Telegram, Some("companion_2"))
            .await
            .unwrap();
        assert_eq!(
            svc.get_master_agent_companion_id(PluginType::Telegram).await.unwrap().as_deref(),
            Some("companion_2")
        );
        assert!(
            svc.get_master_agent_public_agent_id(PluginType::Telegram)
                .await
                .unwrap()
                .is_none(),
            "setting a companion must clear the public-agent binding"
        );
    }

    // ── resolved_model_to_provider ────────────────────────────────────

    #[test]
    fn resolved_model_converts_to_provider() {
        let model = ResolvedModelConfig {
            provider_id: "abc".into(),
            model: "gpt-5".into(),
            use_model: Some("gpt-5".into()),
        };
        let p = resolved_model_to_provider(Some(&model));
        assert_eq!(p.provider_id, "abc");
        assert_eq!(p.model, "gpt-5");
        assert_eq!(p.use_model.as_deref(), Some("gpt-5"));
    }

    #[test]
    fn none_model_produces_empty_provider() {
        let p = resolved_model_to_provider(None);
        assert!(p.provider_id.is_empty());
        assert!(p.model.is_empty());
        assert!(p.use_model.is_none());
    }
}

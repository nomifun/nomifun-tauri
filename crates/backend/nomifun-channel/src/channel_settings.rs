use std::sync::Arc;

use nomifun_common::{CompanionId, ProviderId, ProviderWithModel};
use nomifun_db::IClientPreferenceRepository;
use tracing::debug;

use crate::error::ChannelError;
use crate::types::PluginType;

const DEFAULT_AGENT_TYPE: &str = "nomi";

/// Per-plugin agent/model configuration read from `client_preferences`.
///
/// Keys use the v3 fixed-shape preference contract:
/// - `channels.{platform}.agent`       → JSON `{"agent_type":"acp","backend":"claude","name":"Claude"}`
/// - `channels.{platform}.defaultModel` → JSON `{"provider_id":"provider_id","model":"model_name"}`
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
}

impl ChannelSettingsService {
    pub fn new(pref_repo: Arc<dyn IClientPreferenceRepository>) -> Self {
        Self { pref_repo }
    }

    /// Reads the agent configuration for a platform from `client_preferences`.
    ///
    /// Falls back to `agent_type=nomi, backend=None` when no config exists.
    pub async fn get_agent_config(&self, platform: PluginType) -> Result<ResolvedAgentConfig, ChannelError> {
        let key = agent_key(platform);
        let prefs = self.pref_repo.get_by_keys(&[&key]).await?;

        let Some(pref) = prefs.into_iter().next() else {
            return Ok(default_agent_config());
        };

        let parsed: serde_json::Value = serde_json::from_str(&pref.value).map_err(|error| {
            ChannelError::InvalidConfig(format!(
                "invalid agent preference for {platform}: {error}"
            ))
        })?;
        let object = parsed.as_object().ok_or_else(|| {
            ChannelError::InvalidConfig(format!(
                "agent preference for {platform} must be an object"
            ))
        })?;
        if object.contains_key("id") {
            return Err(ChannelError::InvalidConfig(format!(
                "agent preference for {platform} uses forbidden generic 'id'"
            )));
        }
        let agent_type = object
            .get("agent_type")
            .and_then(serde_json::Value::as_str)
            .filter(|value| !value.is_empty() && value.trim() == *value)
            .ok_or_else(|| {
                ChannelError::InvalidConfig(format!(
                    "agent preference for {platform} has no valid agent_type"
                ))
            })?
            .to_owned();
        let backend = match agent_type.as_str() {
            "acp" => Some(
                object
                    .get("backend")
                    .and_then(serde_json::Value::as_str)
                    .filter(|value| !value.is_empty() && value.trim() == *value)
                    .ok_or_else(|| {
                        ChannelError::InvalidConfig(format!(
                            "ACP agent preference for {platform} has no valid backend"
                        ))
                    })?
                    .to_owned(),
            ),
            _ => {
                if object.contains_key("backend") {
                    return Err(ChannelError::InvalidConfig(format!(
                        "non-ACP agent preference for {platform} must not contain backend"
                    )));
                }
                None
            }
        };

        debug!(platform = %platform, agent_type = %agent_type, backend = ?backend, "resolved channel agent config");
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

        let parsed: serde_json::Value = serde_json::from_str(&pref.value).map_err(|error| {
            ChannelError::InvalidConfig(format!(
                "invalid model preference for {platform}: {error}"
            ))
        })?;
        let Some(object) = parsed.as_object() else {
            return Err(ChannelError::InvalidConfig(format!(
                "model preference for {platform} must be an object"
            )));
        };
        if object.contains_key("id") {
            return Err(ChannelError::InvalidConfig(format!(
                "model preference for {platform} uses legacy 'id'; use 'provider_id'"
            )));
        }
        let provider_id = object
            .get("provider_id")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| {
                ChannelError::InvalidConfig(format!(
                    "model preference for {platform} has no provider_id"
                ))
            })?;
        let model = object
            .get("model")
            .and_then(serde_json::Value::as_str)
            .filter(|model| !model.is_empty() && model.trim() == *model)
            .ok_or_else(|| {
                ChannelError::InvalidConfig(format!(
                    "model preference for {platform} has no valid model"
                ))
            })?
            .to_owned();
        let provider_id = ProviderId::try_from(provider_id)
            .map_err(|error| {
                ChannelError::InvalidConfig(format!(
                    "invalid provider ID in model preference for {platform}: {error}"
                ))
            })?
            .into_string();

        debug!(platform = %platform, provider_id = %provider_id, model = %model, "resolved channel model config");

        Ok(Some(ResolvedModelConfig {
            provider_id: provider_id.clone(),
            model,
        }))
    }
    /// Reads the companion bound to a channel platform from
    /// `client_preferences` (key `channels.{platform}.companion_id`).
    ///
    /// Returns `None` when the key is absent or stores JSON `null`. A binding
    /// must otherwise be a JSON string containing a canonical Companion ID.
    pub async fn get_channel_companion_id(&self, platform: PluginType) -> Result<Option<String>, ChannelError> {
        let key = channel_companion_key(platform);
        let prefs = self.pref_repo.get_by_keys(&[&key]).await?;

        let Some(pref) = prefs.into_iter().next() else {
            return Ok(None);
        };

        let raw = match serde_json::from_str::<serde_json::Value>(&pref.value) {
            Ok(serde_json::Value::String(value)) => value,
            Ok(serde_json::Value::Null) => return Ok(None),
            Ok(_) | Err(_) => {
                return Err(ChannelError::InvalidConfig(format!(
                    "companion preference for {platform} must be a canonical ID string or null"
                )));
            }
        };
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            return Err(ChannelError::InvalidConfig(format!(
                "companion preference for {platform} must not contain an empty ID"
            )));
        }
        let companion_id = CompanionId::try_from(trimmed).map_err(|error| {
            ChannelError::InvalidConfig(format!(
                "invalid companion ID in preference for {platform}: {error}"
            ))
        })?;
        Ok(Some(companion_id.into_string()))
    }

    /// Writes (or clears) the companion bound to a channel platform
    /// (key `channels.{platform}.companion_id`). `None` deletes the key and
    /// leaves the platform unbound; an empty or malformed ID is rejected.
    ///
    /// Persistence only: the channel routes layer pairs this with a session reset
    /// so the next inbound message resolves the new binding.
    pub async fn set_channel_companion_id(
        &self,
        platform: PluginType,
        companion_id: Option<&str>,
    ) -> Result<(), ChannelError> {
        let key = channel_companion_key(platform);
        match companion_id {
            Some(id) => {
                let id = CompanionId::try_from(id).map_err(|error| {
                    ChannelError::InvalidConfig(format!(
                        "invalid companion ID for {platform}: {error}"
                    ))
                })?;
                let value = serde_json::Value::String(id.into_string()).to_string();
                self.pref_repo.upsert_batch(&[(&key, &value)]).await?;
            }
            None => {
                self.pref_repo.delete_keys(&[&key]).await?;
            }
        }
        Ok(())
    }
}

fn agent_key(platform: PluginType) -> String {
    format!("channels.{platform}.agent")
}

fn channel_companion_key(platform: PluginType) -> String {
    format!("channels.{platform}.companion_id")
}

fn model_key(platform: PluginType) -> String {
    format!("channels.{platform}.defaultModel")
}

fn route_to_requirement_key(platform: PluginType) -> String {
    format!("channels.{platform}.routeToRequirement")
}

fn requirement_tag_key(platform: PluginType) -> String {
    format!("channels.{platform}.requirementTag")
}

fn default_agent_config() -> ResolvedAgentConfig {
    ResolvedAgentConfig {
        agent_type: DEFAULT_AGENT_TYPE.to_owned(),
        backend: None,
    }
}

/// Builds a `ProviderWithModel` only when a validated model configuration is
/// present. Absence stays `None`; it is never encoded as an object with an
/// empty provider ID.
pub fn resolved_model_to_provider(model: Option<&ResolvedModelConfig>) -> Option<ProviderWithModel> {
    model.map(|m| ProviderWithModel {
        provider_id: m.provider_id.clone(),
        model: m.model.clone(),
        use_model: None,
    })
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
                    id: 0,
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
                    id: 0,
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
    async fn agent_config_rejects_legacy_backend_only_shape() {
        let repo = Arc::new(MockPrefRepo::with_data(vec![(
            "channels.telegram.agent",
            r#"{"backend":"codex","name":"Codex"}"#,
        )]));
        let svc = ChannelSettingsService::new(repo);

        assert!(matches!(
            svc.get_agent_config(PluginType::Telegram).await,
            Err(ChannelError::InvalidConfig(_))
        ));
    }

    #[tokio::test]
    async fn agent_config_nomi_has_no_backend() {
        let repo = Arc::new(MockPrefRepo::with_data(vec![(
            "channels.lark.agent",
            r#"{"agent_type":"nomi","name":"Nomi"}"#,
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
            "channels.telegram.agent",
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
            "channels.lark.agent",
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
            "channels.weixin.agent",
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
        const PROVIDER_ID: &str = "0190f5fe-7c00-7a00-8abc-012345678901";
        let repo = Arc::new(MockPrefRepo::with_data(vec![(
            "channels.weixin.defaultModel",
            r#"{"provider_id":"0190f5fe-7c00-7a00-8abc-012345678901","model":"global.anthropic.claude-opus-4-6-v1"}"#,
        )]));
        let svc = ChannelSettingsService::new(repo);

        let config = svc.get_model_config(PluginType::Weixin).await.unwrap().unwrap();
        assert_eq!(config.provider_id, PROVIDER_ID);
        assert_eq!(config.model, "global.anthropic.claude-opus-4-6-v1");
    }

    #[tokio::test]
    async fn model_config_rejects_empty_provider_id() {
        let repo = Arc::new(MockPrefRepo::with_data(vec![(
            "channels.telegram.defaultModel",
            r#"{"provider_id":"","model":"channel-test-model"}"#,
        )]));
        let svc = ChannelSettingsService::new(repo);

        assert!(matches!(
            svc.get_model_config(PluginType::Telegram).await,
            Err(ChannelError::InvalidConfig(_))
        ));
    }

    #[tokio::test]
    async fn model_config_rejects_legacy_id_field() {
        let repo = Arc::new(MockPrefRepo::with_data(vec![(
            "channels.telegram.defaultModel",
            r#"{"id":"0190f5fe-7c00-7a00-8abc-012345678901","use_model":"channel-test-model"}"#,
        )]));
        let svc = ChannelSettingsService::new(repo);

        let error = svc.get_model_config(PluginType::Telegram).await.unwrap_err();
        assert!(matches!(error, ChannelError::InvalidConfig(_)));
        assert!(error.to_string().contains("legacy 'id'"));
    }

    // ── get/set_channel_companion_id ────────────────────────────────────────

    #[tokio::test]
    async fn companion_id_returns_none_when_no_pref() {
        let repo = Arc::new(MockPrefRepo::new());
        let svc = ChannelSettingsService::new(repo);
        assert!(svc.get_channel_companion_id(PluginType::Telegram).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn companion_id_reads_json_string_value() {
        const COMPANION_ID: &str =
            "0190f5fe-7c00-7a00-8abc-012345678901";
        let repo = Arc::new(MockPrefRepo::with_data(vec![(
            "channels.telegram.companion_id",
            "\"0190f5fe-7c00-7a00-8abc-012345678901\"",
        )]));
        let svc = ChannelSettingsService::new(repo);
        assert_eq!(
            svc.get_channel_companion_id(PluginType::Telegram).await.unwrap().as_deref(),
            Some(COMPANION_ID)
        );
    }

    #[tokio::test]
    async fn companion_id_rejects_raw_unquoted_value() {
        let repo = Arc::new(MockPrefRepo::with_data(vec![(
            "channels.lark.companion_id",
            "companion_raw",
        )]));
        let svc = ChannelSettingsService::new(repo);
        assert!(matches!(
            svc.get_channel_companion_id(PluginType::Lark).await,
            Err(ChannelError::InvalidConfig(_))
        ));
    }

    #[tokio::test]
    async fn companion_id_accepts_null_but_rejects_empty_strings() {
        let repo = Arc::new(MockPrefRepo::with_data(vec![
            ("channels.telegram.companion_id", "\"\""),
            ("channels.lark.companion_id", "\"  \""),
            ("channels.weixin.companion_id", "null"),
        ]));
        let svc = ChannelSettingsService::new(repo);
        assert!(matches!(
            svc.get_channel_companion_id(PluginType::Telegram).await,
            Err(ChannelError::InvalidConfig(_))
        ));
        assert!(matches!(
            svc.get_channel_companion_id(PluginType::Lark).await,
            Err(ChannelError::InvalidConfig(_))
        ));
        assert!(svc.get_channel_companion_id(PluginType::Weixin).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn companion_id_set_then_get_roundtrip_and_clear() {
        const COMPANION_ID: &str =
            "0190f5fe-7c00-7a00-8abc-012345678901";
        let repo = Arc::new(MockPrefRepo::new());
        let svc = ChannelSettingsService::new(repo);

        svc.set_channel_companion_id(PluginType::Dingtalk, Some(COMPANION_ID))
            .await
            .unwrap();
        assert_eq!(
            svc.get_channel_companion_id(PluginType::Dingtalk).await.unwrap().as_deref(),
            Some(COMPANION_ID)
        );
        // Other platforms unaffected.
        assert!(svc.get_channel_companion_id(PluginType::Telegram).await.unwrap().is_none());

        // None clears the binding (key deleted).
        svc.set_channel_companion_id(PluginType::Dingtalk, None).await.unwrap();
        assert!(svc.get_channel_companion_id(PluginType::Dingtalk).await.unwrap().is_none());

        // Empty input is invalid rather than another spelling of "unbound".
        svc.set_channel_companion_id(PluginType::Dingtalk, Some(COMPANION_ID))
            .await
            .unwrap();
        assert!(matches!(
            svc.set_channel_companion_id(PluginType::Dingtalk, Some("  ")).await,
            Err(ChannelError::InvalidConfig(_))
        ));
        assert_eq!(
            svc.get_channel_companion_id(PluginType::Dingtalk)
                .await
                .unwrap()
                .as_deref(),
            Some(COMPANION_ID)
        );
    }

    // ── resolved_model_to_provider ────────────────────────────────────

    #[test]
    fn resolved_model_converts_to_provider() {
        let model = ResolvedModelConfig {
            provider_id: "0190f5fe-7c00-7a00-8abc-012345678901".into(),
            model: "gpt-5".into(),
        };
        let p = resolved_model_to_provider(Some(&model)).unwrap();
        assert_eq!(
            p.provider_id,
            "0190f5fe-7c00-7a00-8abc-012345678901"
        );
        assert_eq!(p.model, "gpt-5");
        assert!(p.use_model.is_none());
    }

    #[test]
    fn none_model_stays_absent() {
        assert!(resolved_model_to_provider(None).is_none());
    }
}

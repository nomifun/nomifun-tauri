//! Persisted companion configuration: opt-in collection switches, learning model,
//! persona, appearance and quiet-hours. Stored as `config.json` under the companion
//! dir with atomic temp+rename writes (same pattern as cron skill files).

use nomifun_common::ProviderWithModel;
use serde::{Deserialize, Serialize};

/// The roster character every companion falls back to when none is configured.
pub(crate) const DEFAULT_CHARACTER: &str = "mochi";

/// Which event sources the user has opted into collecting. The work-event
/// sources all default OFF; `companion_dialogues` (direct conversations with the
/// companions) defaults ON — talking to the companion is itself the opt-in.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct CollectConfig {
    pub chat_user_messages: bool,
    pub chat_assistant_replies: bool,
    pub requirements: bool,
    pub cron_runs: bool,
    pub conversation_lifecycle: bool,
    pub terminal_sessions: bool,
    /// Tool-call capture from owner work sessions: tool NAME + normalized param
    /// SHAPE only (sorted top-level arg keys + JSON types), never values. The
    /// primary mining signal for skill self-evolution (design §5.1).
    pub tool_calls: bool,
    /// Companion-dialogue capture: owner messages + companion replies inside companion
    /// (companion / Channel Agent) conversations.
    pub companion_dialogues: bool,
}

impl Default for CollectConfig {
    fn default() -> Self {
        Self {
            chat_user_messages: false,
            chat_assistant_replies: false,
            requirements: false,
            cron_runs: false,
            conversation_lifecycle: false,
            terminal_sessions: false,
            tool_calls: false,
            companion_dialogues: true,
        }
    }
}

impl CollectConfig {
    /// Whether any of the opt-in *work-event* sources is enabled (UI
    /// onboarding hint). Deliberately excludes `companion_dialogues`, which is on
    /// by default and would make this vacuously true.
    pub fn any_enabled(&self) -> bool {
        self.chat_user_messages
            || self.chat_assistant_replies
            || self.requirements
            || self.cron_runs
            || self.conversation_lifecycle
            || self.terminal_sessions
            || self.tool_calls
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct PersistedProviderModel {
    #[serde(deserialize_with = "deserialize_provider_id")]
    provider_id: String,
    model: String,
}

fn deserialize_provider_id<'de, D>(deserializer: D) -> Result<String, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let raw = String::deserialize(deserializer)?;
    nomifun_common::ProviderId::parse(raw)
        .map(nomifun_common::ProviderId::into_string)
        .map_err(serde::de::Error::custom)
}

/// Deserialize the only persisted Provider-reference shape accepted by the
/// companion side store: exactly `{provider_id, model}`. `use_model` is a
/// runtime DTO concern and is deliberately not a side-store field.
pub(crate) fn deserialize_optional_model<'de, D>(
    deserializer: D,
) -> Result<Option<ProviderWithModel>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let model = Option::<PersistedProviderModel>::deserialize(deserializer)?;
    model
        .map(|model| {
            let model = ProviderWithModel {
                provider_id: model.provider_id,
                model: model.model,
                use_model: None,
            };
            model.validate().map_err(serde::de::Error::custom)?;
            Ok(model)
        })
        .transpose()
}

pub(crate) fn serialize_optional_model<S>(
    model: &Option<ProviderWithModel>,
    serializer: S,
) -> Result<S::Ok, S::Error>
where
    S: serde::Serializer,
{
    match model {
        None => serializer.serialize_none(),
        Some(model) => {
            model.validate().map_err(serde::ser::Error::custom)?;
            if model.use_model.is_some() {
                return Err(serde::ser::Error::custom(
                    "companion side-store model must use exactly {provider_id, model}",
                ));
            }
            Some(PersistedProviderModel {
                provider_id: model.provider_id.clone(),
                model: model.model.clone(),
            })
            .serialize(serializer)
        }
    }
}

/// Persona settings injected into the chat/learn system prompts.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct PersonaConfig {
    /// One of `lively` | `calm` | `sassy`.
    pub preset: String,
    /// Free-form extra persona instructions appended by the user.
    pub custom: String,
}

impl Default for PersonaConfig {
    fn default() -> Self {
        Self {
            preset: "lively".into(),
            custom: String::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn collect_defaults_are_explicit() {
        let config = CollectConfig::default();
        assert!(config.companion_dialogues);
        assert!(!config.any_enabled());
    }
}

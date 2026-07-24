//! Strict serde helpers shared by public DTOs.

use nomifun_common::{
    AgentExecutionId, AgentExecutionTemplateId, AgentId, AttachmentId, ChannelPluginId,
    ChannelSessionId, ChannelUserId, CompanionId, ConversationId, CronJobId, CronJobRunId,
    MessageId, PresetId, ProviderId, ProviderWithModel, PublicAgentId, RequirementId, TerminalId,
    UserId,
};

pub(crate) fn deserialize_model_name<'de, D>(deserializer: D) -> Result<String, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = <String as serde::Deserialize>::deserialize(deserializer)?;
    validate_model_name(&value).map(|_| value).map_err(serde::de::Error::custom)
}

pub(crate) fn deserialize_optional_model_name<'de, D>(
    deserializer: D,
) -> Result<Option<String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = <Option<String> as serde::Deserialize>::deserialize(deserializer)?;
    value
        .map(|value| {
            validate_model_name(&value)
                .map(|_| value)
                .map_err(serde::de::Error::custom)
        })
        .transpose()
}

fn validate_model_name(value: &str) -> Result<(), &'static str> {
    if value.is_empty() || value.trim() != value {
        Err("model must be a non-empty trimmed natural key")
    } else {
        Ok(())
    }
}

pub(crate) fn validate_optional_provider_model_pair(
    provider_id: Option<&str>,
    model: Option<&str>,
) -> Result<(), String> {
    match (provider_id, model) {
        (None, None) => Ok(()),
        (Some(provider_id), Some(model)) => {
            ProviderId::parse(provider_id)
                .map_err(|error| format!("invalid provider_id: {error}"))?;
            validate_model_name(model).map_err(str::to_owned)
        }
        _ => Err("provider_id and model must be supplied together or both omitted".to_owned()),
    }
}

pub(crate) fn deserialize_optional_provider_with_model<'de, D>(
    deserializer: D,
) -> Result<Option<ProviderWithModel>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = <Option<ProviderWithModel> as serde::Deserialize>::deserialize(deserializer)?;
    if let Some(value) = value.as_ref() {
        value.validate().map_err(serde::de::Error::custom)?;
    }
    Ok(value)
}

macro_rules! string_id_deserializers {
    ($required:ident, $optional:ident, $id:ty) => {
        #[allow(dead_code)]
        pub(crate) fn $required<'de, D>(deserializer: D) -> Result<String, D::Error>
        where
            D: serde::Deserializer<'de>,
        {
            let value = <String as serde::Deserialize>::deserialize(deserializer)?;
            <$id>::parse(value.clone())
                .map(|_| value)
                .map_err(serde::de::Error::custom)
        }

        #[allow(dead_code)]
        pub(crate) fn $optional<'de, D>(
            deserializer: D,
        ) -> Result<Option<String>, D::Error>
        where
            D: serde::Deserializer<'de>,
        {
            let value = <Option<String> as serde::Deserialize>::deserialize(deserializer)?;
            value
                .map(|value| {
                    <$id>::parse(value.clone())
                        .map(|_| value)
                        .map_err(serde::de::Error::custom)
                })
                .transpose()
        }
    };
}

string_id_deserializers!(
    deserialize_conversation_id,
    deserialize_optional_conversation_id,
    ConversationId
);
string_id_deserializers!(
    deserialize_terminal_id,
    deserialize_optional_terminal_id,
    TerminalId
);
string_id_deserializers!(
    deserialize_execution_id,
    deserialize_optional_execution_id,
    AgentExecutionId
);
string_id_deserializers!(
    deserialize_execution_template_id,
    deserialize_optional_execution_template_id,
    AgentExecutionTemplateId
);
string_id_deserializers!(
    deserialize_cron_job_id,
    deserialize_optional_cron_job_id,
    CronJobId
);
string_id_deserializers!(
    deserialize_cron_job_run_id,
    deserialize_optional_cron_job_run_id,
    CronJobRunId
);

pub(crate) fn deserialize_optional_execution_step_id<'de, D>(
    deserializer: D,
) -> Result<Option<String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    deserialize_optional_uuidv7(deserializer)
}

pub(crate) fn deserialize_optional_execution_attempt_id<'de, D>(
    deserializer: D,
) -> Result<Option<String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    deserialize_optional_uuidv7(deserializer)
}

pub(crate) fn deserialize_uuidv7<'de, D>(deserializer: D) -> Result<String, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = <String as serde::Deserialize>::deserialize(deserializer)?;
    nomifun_common::validate_uuidv7(&value)
        .map(|_| value)
        .map_err(serde::de::Error::custom)
}

pub(crate) fn deserialize_uuidv7_vec<'de, D>(deserializer: D) -> Result<Vec<String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    <Vec<String> as serde::Deserialize>::deserialize(deserializer)?
        .into_iter()
        .map(|value| {
            nomifun_common::validate_uuidv7(&value)
                .map(|_| value)
                .map_err(serde::de::Error::custom)
        })
        .collect()
}

pub(crate) fn deserialize_optional_uuidv7_vec<'de, D>(
    deserializer: D,
) -> Result<Option<Vec<String>>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    <Option<Vec<String>> as serde::Deserialize>::deserialize(deserializer)?
        .map(|values| {
            values
                .into_iter()
                .map(|value| {
                    nomifun_common::validate_uuidv7(&value)
                        .map(|_| value)
                        .map_err(serde::de::Error::custom)
                })
                .collect()
        })
        .transpose()
}

fn deserialize_optional_uuidv7<'de, D>(
    deserializer: D,
) -> Result<Option<String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = <Option<String> as serde::Deserialize>::deserialize(deserializer)?;
    value
        .map(|value| {
            nomifun_common::validate_uuidv7(&value)
                .map(|_| value)
                .map_err(serde::de::Error::custom)
        })
        .transpose()
}
string_id_deserializers!(
    deserialize_message_id,
    deserialize_optional_message_id,
    MessageId
);
string_id_deserializers!(deserialize_user_id, deserialize_optional_user_id, UserId);
string_id_deserializers!(
    deserialize_provider_id,
    deserialize_optional_provider_id,
    ProviderId
);
string_id_deserializers!(
    deserialize_channel_plugin_id,
    deserialize_optional_channel_plugin_id,
    ChannelPluginId
);
string_id_deserializers!(
    deserialize_channel_session_id,
    deserialize_optional_channel_session_id,
    ChannelSessionId
);
string_id_deserializers!(
    deserialize_channel_user_id,
    deserialize_optional_channel_user_id,
    ChannelUserId
);
string_id_deserializers!(
    deserialize_companion_id,
    deserialize_optional_companion_id,
    CompanionId
);
string_id_deserializers!(
    deserialize_public_agent_id,
    deserialize_optional_public_agent_id,
    PublicAgentId
);
string_id_deserializers!(
    deserialize_preset_id,
    deserialize_optional_preset_id,
    PresetId
);
string_id_deserializers!(
    deserialize_attachment_id,
    deserialize_optional_attachment_id,
    AttachmentId
);
string_id_deserializers!(
    deserialize_requirement_id,
    deserialize_optional_requirement_id,
    RequirementId
);
// Preset resources use the same bare UUIDv7 business identity everywhere.
// Keep this named helper for the skill-resource DTOs, but do not reintroduce
// the old catalog-key/preset-id union.
string_id_deserializers!(
    deserialize_preset_reference,
    deserialize_optional_preset_reference,
    PresetId
);

macro_rules! string_id_vec_deserializer {
    ($name:ident, $id:ty) => {
        pub(crate) fn $name<'de, D>(deserializer: D) -> Result<Vec<String>, D::Error>
        where
            D: serde::Deserializer<'de>,
        {
            <Vec<String> as serde::Deserialize>::deserialize(deserializer)?
                .into_iter()
                .map(|value| {
                    <$id>::parse(value.clone())
                        .map(|_| value)
                        .map_err(serde::de::Error::custom)
                })
                .collect()
        }
    };
}

string_id_vec_deserializer!(deserialize_attachment_ids, AttachmentId);
string_id_vec_deserializer!(deserialize_requirement_ids, RequirementId);

pub(crate) fn deserialize_preset_tag_key<'de, D>(
    deserializer: D,
) -> Result<String, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = <String as serde::Deserialize>::deserialize(deserializer)?;
    if is_natural_key(&value) {
        Ok(value)
    } else {
        Err(serde::de::Error::custom(
            "expected a canonical preset tag natural key",
        ))
    }
}

pub(crate) fn deserialize_agent_id<'de, D>(deserializer: D) -> Result<String, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = <String as serde::Deserialize>::deserialize(deserializer)?;
    validate_agent_id::<D::Error>(value)
}

pub(crate) fn deserialize_optional_agent_id<'de, D>(
    deserializer: D,
) -> Result<Option<String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = <Option<String> as serde::Deserialize>::deserialize(deserializer)?;
    value.map(validate_agent_id::<D::Error>).transpose()
}

fn validate_agent_id<E>(value: String) -> Result<String, E>
where
    E: serde::de::Error,
{
    AgentId::parse(value.clone())
        .map(|_| value)
        .map_err(E::custom)
}

fn is_natural_key(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 255
        && value.bytes().all(|byte| {
            byte.is_ascii_lowercase()
                || byte.is_ascii_digit()
                || matches!(byte, b'_' | b'-' | b'.' | b':')
        })
}

/// Deserialize a canonical conversation-or-terminal entity ID.
///
/// The wire representation is string-only. Numeric JSON values, malformed
/// UUIDs, and IDs from any other entity namespace are rejected.
pub(crate) fn deserialize_session_target_id<'de, D>(
    deserializer: D,
) -> Result<String, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = <String as serde::Deserialize>::deserialize(deserializer)?;
    if ConversationId::parse(value.clone()).is_ok() || TerminalId::parse(value.clone()).is_ok() {
        Ok(value)
    } else {
        Err(serde::de::Error::custom(
            "expected a canonical conversation or terminal entity ID",
        ))
    }
}

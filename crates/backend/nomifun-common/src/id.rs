use std::fmt;

use uuid::{Uuid, Version};

/// Length of a canonical lowercase hyphenated UUID.
pub const UUID_STRING_LEN: usize = 36;

/// Error returned when an ID is not a canonical lowercase UUIDv7.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum UuidV7Error {
    #[error("UUID must be a canonical lowercase hyphenated UUID")]
    InvalidFormat,
    #[error("UUID must be version 7")]
    WrongVersion,
    #[error("UUID must use the RFC 4122 variant")]
    WrongVariant,
}

/// Validate a standalone canonical lowercase hyphenated RFC-4122 UUIDv7.
pub fn validate_uuidv7(value: &str) -> Result<Uuid, UuidV7Error> {
    if value.len() != UUID_STRING_LEN {
        return Err(UuidV7Error::InvalidFormat);
    }
    let uuid = Uuid::parse_str(value).map_err(|_| UuidV7Error::InvalidFormat)?;
    if uuid.hyphenated().to_string() != value {
        return Err(UuidV7Error::InvalidFormat);
    }
    if uuid.get_version() != Some(Version::SortRand) {
        return Err(UuidV7Error::WrongVersion);
    }
    if uuid.get_variant() != uuid::Variant::RFC4122 {
        return Err(UuidV7Error::WrongVariant);
    }
    Ok(uuid)
}

/// Generate a canonical lowercase hyphenated UUIDv7 string.
pub fn generate_id() -> String {
    Uuid::now_v7().to_string()
}

/// Shared behavior implemented by every strongly typed entity ID.
pub trait EntityId:
    Clone
    + Eq
    + Ord
    + std::hash::Hash
    + AsRef<str>
    + fmt::Display
    + std::str::FromStr<Err = UuidV7Error>
{
    /// Mint a new UUIDv7 identifier.
    fn new() -> Self;

    /// Return the canonical string representation.
    fn as_str(&self) -> &str {
        self.as_ref()
    }
}

macro_rules! define_entity_id {
    ($(#[$meta:meta])* $name:ident) => {
        $(#[$meta])*
        #[derive(
            Clone,
            Debug,
            PartialEq,
            Eq,
            PartialOrd,
            Ord,
            Hash,
            serde::Serialize,
        )]
        #[serde(transparent)]
        pub struct $name(String);

        impl $name {
            /// Mint a new UUIDv7 identifier of this domain type.
            pub fn new() -> Self {
                Self(generate_id())
            }

            /// Parse and validate a canonical lowercase UUIDv7.
            pub fn parse(value: impl Into<String>) -> Result<Self, UuidV7Error> {
                let value = value.into();
                validate_uuidv7(&value)?;
                Ok(Self(value))
            }

            /// Return the canonical string representation.
            pub fn as_str(&self) -> &str {
                &self.0
            }

            /// Consume the typed ID and return its canonical string.
            pub fn into_string(self) -> String {
                self.0
            }
        }

        impl EntityId for $name {
            fn new() -> Self {
                Self::new()
            }
        }

        impl Default for $name {
            fn default() -> Self {
                Self::new()
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str(&self.0)
            }
        }

        impl std::str::FromStr for $name {
            type Err = UuidV7Error;

            fn from_str(value: &str) -> Result<Self, Self::Err> {
                Self::parse(value)
            }
        }

        impl TryFrom<String> for $name {
            type Error = UuidV7Error;

            fn try_from(value: String) -> Result<Self, Self::Error> {
                Self::parse(value)
            }
        }

        impl TryFrom<&str> for $name {
            type Error = UuidV7Error;

            fn try_from(value: &str) -> Result<Self, Self::Error> {
                Self::parse(value)
            }
        }

        impl From<$name> for String {
            fn from(value: $name) -> Self {
                value.0
            }
        }

        impl<'de> serde::Deserialize<'de> for $name {
            fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
            where
                D: serde::Deserializer<'de>,
            {
                let value = <String as serde::Deserialize>::deserialize(deserializer)?;
                Self::parse(value).map_err(serde::de::Error::custom)
            }
        }

        impl AsRef<str> for $name {
            fn as_ref(&self) -> &str {
                &self.0
            }
        }

        impl std::ops::Deref for $name {
            type Target = str;

            fn deref(&self) -> &Self::Target {
                &self.0
            }
        }

        impl std::borrow::Borrow<str> for $name {
            fn borrow(&self) -> &str {
                &self.0
            }
        }
    };
}

define_entity_id!(
    /// Globally unique conversation identifier.
    ConversationId
);
define_entity_id!(
    /// Globally unique terminal-session identifier.
    TerminalId
);
define_entity_id!(
    /// Globally unique remote-agent identifier.
    RemoteAgentId
);
define_entity_id!(
    /// Globally unique user identifier.
    UserId
);
define_entity_id!(
    /// Globally unique provider configuration identifier.
    ProviderId
);
define_entity_id!(
    /// Globally unique agent identifier.
    ///
    /// Builtin catalog lineage is stored separately as `source_key`
    /// (for example, `agent_builtin_claude`) rather than being embedded in
    /// the UUID.
    AgentId
);
define_entity_id!(
    /// Globally unique preset identifier.
    ///
    /// Builtin and extension catalog lineage is stored separately as
    /// `source_key`; this ID is always a bare UUIDv7.
    PresetId
);
define_entity_id!(
    /// Globally unique message identifier.
    MessageId
);
define_entity_id!(
    /// Globally unique knowledge-base identifier.
    KnowledgeBaseId
);
define_entity_id!(
    /// Globally unique knowledge-binding identifier.
    ///
    /// This is the product-facing business ID. The SQLite row `id` remains
    /// an implementation-only local technical key.
    KnowledgeBindingId
);
define_entity_id!(
    /// Globally unique connector-credential identifier.
    ConnectorCredentialId
);
define_entity_id!(
    /// Globally unique attachment identifier.
    AttachmentId
);
define_entity_id!(
    /// Globally unique preview-history snapshot identifier.
    PreviewSnapshotId
);
define_entity_id!(
    /// Globally unique IDMM intervention audit-record identifier.
    ///
    /// This is the product-facing business ID. The SQLite row `id` remains
    /// an implementation-only autoincrement key for ordering and eviction.
    IdmmInterventionId
);
define_entity_id!(
    /// Globally unique requirement identifier.
    RequirementId
);
define_entity_id!(
    /// Globally unique receipt identifier for a durable tool artifact.
    ///
    /// This identifies a tool-output receipt embedded in a message and is
    /// distinct from the UUIDv7 `conversation_artifact_id` of a row in
    /// `conversation_artifacts`. Neither identity is a SQLite technical key.
    PersistedArtifactId
);
define_entity_id!(
    /// Globally unique agent-execution template identifier.
    AgentExecutionTemplateId
);
define_entity_id!(
    /// Globally unique agent-execution identifier.
    AgentExecutionId
);
define_entity_id!(
    /// Globally unique step identifier within an agent execution.
    AgentExecutionStepId
);
define_entity_id!(
    /// Globally unique attempt identifier within an agent execution.
    AgentExecutionAttemptId
);
define_entity_id!(
    /// Globally unique scheduled-task identifier.
    CronJobId
);
define_entity_id!(
    /// Globally unique scheduled-task run identifier.
    CronJobRunId
);
define_entity_id!(
    /// Globally unique channel-plugin identifier.
    ChannelPluginId
);
define_entity_id!(
    /// Globally unique channel-session identifier.
    ChannelSessionId
);
define_entity_id!(
    /// Globally unique authorized channel-user identifier.
    ChannelUserId
);
define_entity_id!(
    /// Globally unique companion identifier.
    CompanionId
);
define_entity_id!(
    /// Globally unique companion memory identifier.
    CompanionMemoryId
);
define_entity_id!(
    /// Globally unique companion suggestion identifier.
    CompanionSuggestionId
);
define_entity_id!(
    /// Globally unique companion learn-run identifier.
    CompanionLearnRunId
);
define_entity_id!(
    /// Globally unique companion collected-event identifier.
    CompanionEventId
);
define_entity_id!(
    /// Globally unique companion skill identifier.
    CompanionSkillId
);
define_entity_id!(
    /// Globally unique companion skill-pattern identifier.
    CompanionSkillPatternId
);
define_entity_id!(
    /// Globally unique companion session-window identifier.
    CompanionSessionWindowId
);
define_entity_id!(
    /// Globally unique figure-library entry identifier.
    ///
    /// Figure metadata is stored in the durable figure index and the ID also
    /// names the corresponding image file, so it is an entity ID even though
    /// it does not live in the main SQLite database.
    FigureId
);
define_entity_id!(
    /// Globally unique public-agent audit-entry identifier.
    ///
    /// Audit entries are durable JSONL records and may be paged or exported
    /// independently of the public-agent profile that owns them.
    PublicAgentAuditEntryId
);
define_entity_id!(
    /// Globally unique companion evolution-feedback identifier.
    CompanionEvolutionFeedbackId
);
define_entity_id!(
    /// Globally unique public-agent identifier.
    PublicAgentId
);
define_entity_id!(
    /// Globally unique workshop-canvas identifier.
    WorkshopCanvasId
);
define_entity_id!(
    /// Globally unique workshop-asset identifier.
    WorkshopAssetId
);
define_entity_id!(
    /// Globally unique creation-task identifier.
    CreationTaskId
);
define_entity_id!(
    /// Globally unique node identifier within a durable workshop canvas doc.
    WorkshopNodeId
);
define_entity_id!(
    /// Globally unique edge identifier within a durable workshop canvas doc.
    WorkshopEdgeId
);
define_entity_id!(
    /// Globally unique MCP-server configuration identifier.
    McpServerId
);
define_entity_id!(
    /// Globally unique webhook configuration identifier.
    WebhookId
);
define_entity_id!(
    /// Globally unique conversation-artifact identifier.
    ConversationArtifactId
);
define_entity_id!(
    /// Globally unique preset-tag identifier.
    PresetTagId
);
define_entity_id!(
    /// Globally unique execution-participant identifier.
    AgentExecutionParticipantId
);
define_entity_id!(
    /// Globally unique execution-template participant identifier.
    AgentExecutionTemplateParticipantId
);

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn generate_id_is_canonical_uuid_v7() {
        let id = generate_id();
        assert_eq!(id.len(), UUID_STRING_LEN);
        assert_eq!(validate_uuidv7(&id).unwrap().to_string(), id);
    }

    #[test]
    fn uuidv7_validation_rejects_prefixed_legacy_and_noncanonical_values() {
        let valid = generate_id();
        assert_eq!(
            validate_uuidv7("conv_0190f5fe-7c00-7a00-8000-000000000003"),
            Err(UuidV7Error::InvalidFormat)
        );
        assert_eq!(
            validate_uuidv7("550e8400-e29b-41d4-a716-446655440000"),
            Err(UuidV7Error::WrongVersion)
        );
        assert_eq!(
            validate_uuidv7(&valid.to_ascii_uppercase()),
            Err(UuidV7Error::InvalidFormat)
        );
        assert_eq!(
            validate_uuidv7(&valid.replace('-', "")),
            Err(UuidV7Error::InvalidFormat)
        );
        assert_eq!(
            validate_uuidv7(&format!("{valid}\n")),
            Err(UuidV7Error::InvalidFormat)
        );
    }

    #[test]
    fn generated_ids_are_unique() {
        let ids: HashSet<String> = (0..10_000).map(|_| generate_id()).collect();
        assert_eq!(ids.len(), 10_000);
    }

    #[test]
    fn generated_uuid_v7_ids_sort_by_later_millisecond() {
        let earlier = generate_id();
        std::thread::sleep(std::time::Duration::from_millis(2));
        let later = generate_id();
        assert!(later > earlier, "{later} should sort after {earlier}");
    }

    #[test]
    fn typed_id_roundtrips_display_parse_as_ref_and_string() {
        let id = ConversationId::new();
        let text = id.to_string();
        assert_eq!(id.as_ref(), text);
        assert_eq!(id.as_str(), text);
        assert_eq!(text.parse::<ConversationId>().unwrap(), id);
        assert_eq!(String::from(id.clone()), text);
        assert_eq!(id.into_string(), text);
    }

    #[test]
    fn typed_id_serde_is_transparent_and_validating() {
        let id = ConversationId::new();
        let json = serde_json::to_string(&id).unwrap();
        assert_eq!(json, format!("\"{id}\""));
        assert_eq!(serde_json::from_str::<ConversationId>(&json).unwrap(), id);
        assert!(serde_json::from_str::<ConversationId>(
            "\"conv_0190f5fe-7c00-7a00-8000-000000000003\""
        )
        .is_err());
        assert!(serde_json::from_str::<ConversationId>("42").is_err());
    }
}

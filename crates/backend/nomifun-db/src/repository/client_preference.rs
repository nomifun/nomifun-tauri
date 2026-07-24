use crate::error::DbError;
use crate::models::ClientPreference;

const IDMM_BACKUP_PROVIDER_KEY: &str = "idmm_backup_provider_id";
const MODEL_FAILOVER_KEY: &str = "agent.model_failover";
const COLLABORATION_MODELS_KEY: &str = "nomi.collaborationModels";
const NOMI_DEFAULT_MODEL_KEY: &str = "nomi.defaultModel";
const KNOWLEDGE_AUTOGEN_MODEL_KEY: &str = "knowledge.autogenModel";
const IMAGE_GENERATION_MODEL_KEY: &str = "tools.imageGenerationModel";
const SPEECH_TO_TEXT_KEY: &str = "tools.speechToText";

/// Client preference data access abstraction.
///
/// Provides CRUD operations on the generic key-value `client_preferences` table.
#[async_trait::async_trait]
pub trait IClientPreferenceRepository: Send + Sync {
    /// Returns all client preferences.
    async fn get_all(&self) -> Result<Vec<ClientPreference>, DbError>;

    /// Returns preferences for the given keys only.
    /// Keys that don't exist are simply omitted from the result.
    async fn get_by_keys(&self, keys: &[&str]) -> Result<Vec<ClientPreference>, DbError>;

    /// Inserts or updates a batch of key-value pairs.
    async fn upsert_batch(&self, entries: &[(&str, &str)]) -> Result<(), DbError>;

    /// Deletes the given keys.
    async fn delete_keys(&self, keys: &[&str]) -> Result<(), DbError>;

    /// Applies upserts and deletes as one logical update.
    ///
    /// SQLite overrides this method so Provider parent validation and all
    /// preference changes share one writer transaction. Test doubles may rely
    /// on this default implementation when transaction semantics are
    /// irrelevant to the test.
    async fn update_batch(
        &self,
        upserts: &[(&str, &str)],
        delete_keys: &[&str],
    ) -> Result<(), DbError> {
        self.delete_keys(delete_keys).await?;
        self.upsert_batch(upserts).await
    }
}

#[derive(Debug)]
pub(crate) struct NormalizedProviderPreference {
    pub value: String,
    pub provider_ids: Vec<String>,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum ProviderPreferenceDeleteAction {
    Keep,
    Restrict,
    Delete,
    Update(String),
}

#[derive(Debug, Clone, Copy)]
enum ProviderPreferenceKind {
    IdmmBackupProvider,
    ModelFailover,
    CollaborationModels,
    RequiredModelObject,
    OptionalObjectProviderId,
}

fn provider_preference_kind(key: &str) -> Option<ProviderPreferenceKind> {
    match key {
        IDMM_BACKUP_PROVIDER_KEY => Some(ProviderPreferenceKind::IdmmBackupProvider),
        MODEL_FAILOVER_KEY => Some(ProviderPreferenceKind::ModelFailover),
        COLLABORATION_MODELS_KEY => Some(ProviderPreferenceKind::CollaborationModels),
        NOMI_DEFAULT_MODEL_KEY | KNOWLEDGE_AUTOGEN_MODEL_KEY | IMAGE_GENERATION_MODEL_KEY => {
            Some(ProviderPreferenceKind::RequiredModelObject)
        }
        SPEECH_TO_TEXT_KEY => Some(ProviderPreferenceKind::OptionalObjectProviderId),
        _ if is_channel_default_model_key(key) => {
            Some(ProviderPreferenceKind::RequiredModelObject)
        }
        _ => None,
    }
}

fn is_channel_default_model_key(key: &str) -> bool {
    key.strip_prefix("channels.")
        .and_then(|rest| rest.strip_suffix(".defaultModel"))
        .is_some_and(|platform| !platform.is_empty())
}

fn invalid_preference(key: &str, message: impl std::fmt::Display) -> DbError {
    DbError::Conflict(format!("invalid client preference '{key}': {message}"))
}

fn canonical_provider_id(
    key: &str,
    path: &str,
    value: &str,
) -> Result<String, DbError> {
    nomifun_common::ProviderId::parse(value)
        .map(nomifun_common::ProviderId::into_string)
        .map_err(|error| {
            invalid_preference(
                key,
                format!("Provider ID at {path} is not a canonical UUIDv7: {error}"),
            )
        })
}

fn parse_json(key: &str, value: &str) -> Result<serde_json::Value, DbError> {
    serde_json::from_str(value)
        .map_err(|error| invalid_preference(key, format!("value must be valid JSON: {error}")))
}

fn require_object<'a>(
    key: &str,
    path: &str,
    value: &'a serde_json::Value,
) -> Result<&'a serde_json::Map<String, serde_json::Value>, DbError> {
    value
        .as_object()
        .ok_or_else(|| invalid_preference(key, format!("{path} must be an object")))
}

fn require_array<'a>(
    key: &str,
    path: &str,
    value: &'a serde_json::Value,
) -> Result<&'a Vec<serde_json::Value>, DbError> {
    value
        .as_array()
        .ok_or_else(|| invalid_preference(key, format!("{path} must be an array")))
}

fn required_provider_field(
    key: &str,
    path: &str,
    object: &serde_json::Map<String, serde_json::Value>,
    field: &str,
) -> Result<String, DbError> {
    let value = object
        .get(field)
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| {
            invalid_preference(
                key,
                format!("{path}.{field} must be a canonical Provider UUIDv7 string"),
            )
        })?;
    canonical_provider_id(key, &format!("{path}.{field}"), value)
}

fn reject_legacy_provider_id_field(
    key: &str,
    path: &str,
    object: &serde_json::Map<String, serde_json::Value>,
) -> Result<(), DbError> {
    if object.contains_key("id") {
        return Err(invalid_preference(
            key,
            format!("{path}.id is a legacy Provider field; use {path}.provider_id"),
        ));
    }
    Ok(())
}

fn require_model_field(
    key: &str,
    path: &str,
    object: &serde_json::Map<String, serde_json::Value>,
) -> Result<(), DbError> {
    let model = object
        .get("model")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| {
            invalid_preference(
                key,
                format!("{path}.model must be a non-empty trimmed string"),
            )
        })?;
    if model.is_empty() || model.trim() != model {
        return Err(invalid_preference(
            key,
            format!("{path}.model must be a non-empty trimmed string"),
        ));
    }
    Ok(())
}

fn optional_provider_field(
    key: &str,
    path: &str,
    object: &serde_json::Map<String, serde_json::Value>,
    field: &str,
) -> Result<Option<String>, DbError> {
    match object.get(field) {
        None | Some(serde_json::Value::Null) => Ok(None),
        Some(serde_json::Value::String(value)) => {
            canonical_provider_id(key, &format!("{path}.{field}"), value).map(Some)
        }
        Some(_) => Err(invalid_preference(
            key,
            format!("{path}.{field} must be null or a canonical Provider UUIDv7 string"),
        )),
    }
}

fn parse_idmm_backup_provider(
    key: &str,
    value: &str,
) -> Result<NormalizedProviderPreference, DbError> {
    // The specialized IDMM service stores this scalar as bare text. Accept a
    // JSON string as well so the generic preference endpoint converges on the
    // same fixed persisted representation instead of creating a second shape.
    let provider_id = serde_json::from_str::<String>(value)
        .unwrap_or_else(|_| value.to_owned());
    let provider_id = canonical_provider_id(key, "$", &provider_id)?;
    Ok(NormalizedProviderPreference {
        value: provider_id.clone(),
        provider_ids: vec![provider_id],
    })
}

fn parse_json_provider_preference(
    key: &str,
    value: &str,
    kind: ProviderPreferenceKind,
) -> Result<NormalizedProviderPreference, DbError> {
    let parsed = parse_json(key, value)?;
    let mut provider_ids = Vec::new();

    match kind {
        ProviderPreferenceKind::ModelFailover => {
            let object = require_object(key, "$", &parsed)?;
            if let Some(queue) = object.get("queue") {
                for (index, item) in require_array(key, "$.queue", queue)?.iter().enumerate() {
                    let path = format!("$.queue[{index}]");
                    let item = require_object(key, &path, item)?;
                    provider_ids.push(required_provider_field(
                        key,
                        &path,
                        item,
                        "provider_id",
                    )?);
                }
            }
        }
        ProviderPreferenceKind::CollaborationModels => {
            for (index, item) in require_array(key, "$", &parsed)?.iter().enumerate() {
                let path = format!("$[{index}]");
                let item = require_object(key, &path, item)?;
                provider_ids.push(required_provider_field(
                    key,
                    &path,
                    item,
                    "provider_id",
                )?);
            }
        }
        ProviderPreferenceKind::RequiredModelObject => {
            let object = require_object(key, "$", &parsed)?;
            reject_legacy_provider_id_field(key, "$", object)?;
            require_model_field(key, "$", object)?;
            provider_ids.push(required_provider_field(
                key,
                "$",
                object,
                "provider_id",
            )?);
        }
        ProviderPreferenceKind::OptionalObjectProviderId => {
            let object = require_object(key, "$", &parsed)?;
            if let Some(provider_id) =
                optional_provider_field(key, "$", object, "provider_id")?
            {
                provider_ids.push(provider_id);
            }
        }
        ProviderPreferenceKind::IdmmBackupProvider => unreachable!(),
    }

    provider_ids.sort();
    provider_ids.dedup();
    Ok(NormalizedProviderPreference {
        value: parsed.to_string(),
        provider_ids,
    })
}

pub(crate) fn normalize_provider_preference(
    key: &str,
    value: &str,
) -> Result<NormalizedProviderPreference, DbError> {
    let Some(kind) = provider_preference_kind(key) else {
        return Ok(NormalizedProviderPreference {
            value: value.to_owned(),
            provider_ids: Vec::new(),
        });
    };

    match kind {
        ProviderPreferenceKind::IdmmBackupProvider => {
            parse_idmm_backup_provider(key, value)
        }
        _ => parse_json_provider_preference(key, value, kind),
    }
}

pub(crate) fn provider_preference_delete_action(
    key: &str,
    value: &str,
    provider_id: &str,
) -> Result<ProviderPreferenceDeleteAction, DbError> {
    let Some(kind) = provider_preference_kind(key) else {
        return Ok(ProviderPreferenceDeleteAction::Keep);
    };

    if matches!(kind, ProviderPreferenceKind::IdmmBackupProvider) {
        let normalized = parse_idmm_backup_provider(key, value)?;
        return Ok(if normalized.provider_ids.iter().any(|id| id == provider_id) {
            ProviderPreferenceDeleteAction::Restrict
        } else {
            ProviderPreferenceDeleteAction::Keep
        });
    }

    let normalized = parse_json_provider_preference(key, value, kind)?;
    if !normalized.provider_ids.iter().any(|id| id == provider_id) {
        return Ok(ProviderPreferenceDeleteAction::Keep);
    }

    let mut parsed = parse_json(key, &normalized.value)?;
    match kind {
        ProviderPreferenceKind::ModelFailover => {
            let queue = parsed
                .as_object_mut()
                .and_then(|object| object.get_mut("queue"))
                .and_then(serde_json::Value::as_array_mut)
                .expect("validated model failover queue");
            queue.retain(|item| {
                item.get("provider_id").and_then(serde_json::Value::as_str)
                    != Some(provider_id)
            });
            Ok(ProviderPreferenceDeleteAction::Update(parsed.to_string()))
        }
        ProviderPreferenceKind::CollaborationModels => {
            let models = parsed
                .as_array_mut()
                .expect("validated collaboration model array");
            models.retain(|item| {
                item.get("provider_id").and_then(serde_json::Value::as_str)
                    != Some(provider_id)
            });
            Ok(ProviderPreferenceDeleteAction::Update(parsed.to_string()))
        }
        ProviderPreferenceKind::RequiredModelObject => {
            Ok(ProviderPreferenceDeleteAction::Delete)
        }
        ProviderPreferenceKind::OptionalObjectProviderId => {
            parsed
                .as_object_mut()
                .expect("validated optional Provider preference")
                .insert("provider_id".to_owned(), serde_json::Value::Null);
            Ok(ProviderPreferenceDeleteAction::Update(parsed.to_string()))
        }
        ProviderPreferenceKind::IdmmBackupProvider => unreachable!(),
    }
}

#[cfg(test)]
mod provider_reference_tests {
    use super::*;

    const PROVIDER_A: &str = "0190f5fe-7c00-7a00-8000-000000000001";
    const PROVIDER_B: &str = "0190f5fe-7c00-7a00-8000-000000000002";

    #[test]
    fn registry_extracts_every_supported_provider_reference_shape() {
        let cases = [
            (IDMM_BACKUP_PROVIDER_KEY, PROVIDER_A.to_owned(), 1),
            (
                MODEL_FAILOVER_KEY,
                serde_json::json!({
                    "queue": [
                        {"provider_id": PROVIDER_A, "model": "a"},
                        {"provider_id": PROVIDER_B, "model": "b"}
                    ]
                })
                .to_string(),
                2,
            ),
            (
                COLLABORATION_MODELS_KEY,
                serde_json::json!([{"provider_id": PROVIDER_A, "model": "a"}])
                    .to_string(),
                1,
            ),
            (
                NOMI_DEFAULT_MODEL_KEY,
                serde_json::json!({"provider_id": PROVIDER_A, "model": "a"}).to_string(),
                1,
            ),
            (
                KNOWLEDGE_AUTOGEN_MODEL_KEY,
                serde_json::json!({"provider_id": PROVIDER_A, "model": "a"})
                    .to_string(),
                1,
            ),
            (
                IMAGE_GENERATION_MODEL_KEY,
                serde_json::json!({"provider_id": PROVIDER_A, "model": "a"}).to_string(),
                1,
            ),
            (
                SPEECH_TO_TEXT_KEY,
                serde_json::json!({"enabled": true, "provider_id": PROVIDER_A})
                    .to_string(),
                1,
            ),
            (
                "channels.telegram.defaultModel",
                serde_json::json!({"provider_id": PROVIDER_A, "model": "a"}).to_string(),
                1,
            ),
        ];

        for (key, value, expected_count) in cases {
            let normalized = normalize_provider_preference(key, &value).unwrap();
            assert_eq!(
                normalized.provider_ids.len(),
                expected_count,
                "unexpected Provider reference count for {key}"
            );
        }
    }

    #[test]
    fn registry_rejects_noncanonical_or_malformed_registered_values() {
        for (key, value) in [
            (IDMM_BACKUP_PROVIDER_KEY, "prov_legacy"),
            (
                MODEL_FAILOVER_KEY,
                r#"{"queue":[{"provider_id":"prov_legacy","model":"a"}]}"#,
            ),
            (COLLABORATION_MODELS_KEY, r#"[{"model":"a"}]"#),
            (NOMI_DEFAULT_MODEL_KEY, r#"{"id":42}"#),
            (KNOWLEDGE_AUTOGEN_MODEL_KEY, "not-json"),
            (IMAGE_GENERATION_MODEL_KEY, r#"[]"#),
            (SPEECH_TO_TEXT_KEY, r#"{"provider_id":42}"#),
            (
                "channels.telegram.defaultModel",
                r#"{"id":"0190f5fe-7c00-4a00-8000-000000000001"}"#,
            ),
        ] {
            assert!(
                normalize_provider_preference(key, value).is_err(),
                "{key} unexpectedly accepted malformed Provider reference data"
            );
        }
    }

    #[test]
    fn registry_rejects_legacy_id_for_default_model_objects() {
        for key in [
            NOMI_DEFAULT_MODEL_KEY,
            IMAGE_GENERATION_MODEL_KEY,
            "channels.telegram.defaultModel",
        ] {
            let value = serde_json::json!({
                "id": PROVIDER_A,
                "use_model": "a",
            })
            .to_string();
            let error = normalize_provider_preference(key, &value).unwrap_err();
            assert!(
                error.to_string().contains("legacy Provider field"),
                "{key} returned an unexpected error: {error}"
            );
        }
    }

    #[test]
    fn delete_actions_filter_arrays_delete_defaults_and_null_optional_reference() {
        let collaboration = serde_json::json!([
            {"provider_id": PROVIDER_A, "model": "first"},
            {"provider_id": PROVIDER_B, "model": "keep"},
            {"provider_id": PROVIDER_A, "model": "last"}
        ])
        .to_string();
        let ProviderPreferenceDeleteAction::Update(collaboration) =
            provider_preference_delete_action(
                COLLABORATION_MODELS_KEY,
                &collaboration,
                PROVIDER_A,
            )
            .unwrap()
        else {
            panic!("collaboration models must be filtered");
        };
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&collaboration).unwrap(),
            serde_json::json!([{"provider_id": PROVIDER_B, "model": "keep"}])
        );

        assert_eq!(
            provider_preference_delete_action(
                NOMI_DEFAULT_MODEL_KEY,
                &serde_json::json!({"provider_id": PROVIDER_A, "model": "a"})
                    .to_string(),
                PROVIDER_A,
            )
            .unwrap(),
            ProviderPreferenceDeleteAction::Delete
        );

        let ProviderPreferenceDeleteAction::Update(speech) =
            provider_preference_delete_action(
                SPEECH_TO_TEXT_KEY,
                &serde_json::json!({
                    "enabled": true,
                    "provider_id": PROVIDER_A,
                    "model": "whisper"
                })
                .to_string(),
                PROVIDER_A,
            )
            .unwrap()
        else {
            panic!("speech preference must be updated");
        };
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&speech).unwrap(),
            serde_json::json!({
                "enabled": true,
                "provider_id": null,
                "model": "whisper"
            })
        );
    }
}

//! Resolves a model pool and reusable presets into immutable, execution-scoped
//! Agent participant snapshots.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use nomifun_api_types::{
    ExecutionModelPool, ExecutionModelRef, ParticipantCapability, PresetOverrides, PresetTarget,
    ResolvedPresetSnapshot, infer_model_modalities,
};
use nomifun_common::{
    AppError, MAX_AGENT_EXECUTION_MODELS, MAX_AGENT_EXECUTION_PARTICIPANTS, ProviderId,
    NOMI_AGENT_ID,
};
#[cfg(test)]
use nomifun_common::generate_id;
use nomifun_db::{IProviderRepository, NewAgentExecutionParticipant};
use nomifun_preset::PresetService;
use serde_json::Value;

#[derive(Clone)]
pub(crate) struct ParticipantResolver {
    provider_repo: Arc<dyn IProviderRepository>,
    preset_service: Arc<PresetService>,
}

impl ParticipantResolver {
    pub fn new(
        provider_repo: Arc<dyn IProviderRepository>,
        preset_service: Arc<PresetService>,
    ) -> Self {
        Self {
            provider_repo,
            preset_service,
        }
    }

    pub async fn resolve(
        &self,
        pool: &ExecutionModelPool,
        lead_model: Option<&ExecutionModelRef>,
    ) -> Result<Vec<NewAgentExecutionParticipant>, AppError> {
        pool.validate().map_err(AppError::BadRequest)?;
        let providers = self
            .provider_repo
            .list()
            .await
            .map_err(|error| AppError::Internal(format!("list model providers: {error}")))?;
        let mut catalog = HashMap::new();
        let mut catalog_order = Vec::new();
        for provider in providers.iter().filter(|provider| provider.enabled) {
            if ProviderId::try_from(provider.provider_id.as_str()).is_err() {
                return Err(AppError::Internal(
                    "enabled provider has a non-canonical persisted id".to_owned(),
                ));
            }
            let models: Vec<String> = serde_json::from_str(&provider.models).map_err(|error| {
                AppError::Internal(format!(
                    "provider {} has invalid persisted models: {error}",
                    provider.provider_id
                ))
            })?;
            let enabled: serde_json::Map<String, Value> = provider
                .model_enabled
                .as_deref()
                .map(serde_json::from_str)
                .transpose()
                .map_err(|error| {
                    AppError::Internal(format!(
                        "provider {} has invalid persisted model_enabled: {error}",
                        provider.provider_id
                    ))
                })?
                .unwrap_or_default();
            let descriptions: HashMap<String, String> = provider
                .model_descriptions
                .as_deref()
                .map(serde_json::from_str)
                .transpose()
                .map_err(|error| {
                    AppError::Internal(format!(
                        "provider {} has invalid persisted model_descriptions: {error}",
                        provider.provider_id
                    ))
                })?
                .unwrap_or_default();
            if enabled.values().any(|value| !value.is_boolean()) {
                return Err(AppError::Internal(format!(
                    "provider {} has a non-boolean model_enabled value",
                    provider.provider_id
                )));
            }
            for raw_model in models {
                let model = raw_model.trim().to_owned();
                if model.is_empty() || model != raw_model {
                    return Err(AppError::Internal(format!(
                        "provider {} has an invalid persisted model id",
                        provider.provider_id
                    )));
                }
                if enabled.get(&model).and_then(Value::as_bool).unwrap_or(true) {
                    let key = (provider.provider_id.clone(), model.clone());
                    if catalog
                        .insert(key.clone(), descriptions.get(&model).cloned())
                        .is_none()
                    {
                        catalog_order.push(ExecutionModelRef {
                            provider_id: key.0,
                            model: key.1,
                        });
                    }
                }
            }
        }

        let requested = match pool {
            ExecutionModelPool::Single { model } => vec![model.clone()],
            ExecutionModelPool::Automatic => {
                let mut models = Vec::with_capacity(MAX_AGENT_EXECUTION_MODELS);
                if let Some(lead) = lead_model {
                    models.push(lead.clone());
                }
                models.extend(
                    catalog_order
                        .iter()
                        .filter(|candidate| Some(*candidate) != lead_model)
                        .take(MAX_AGENT_EXECUTION_MODELS.saturating_sub(models.len()))
                        .cloned(),
                );
                models
            }
            ExecutionModelPool::Range { models } => {
                if models.len() > MAX_AGENT_EXECUTION_MODELS {
                    return Err(AppError::BadRequest(format!(
                        "execution model range exceeds {MAX_AGENT_EXECUTION_MODELS} models"
                    )));
                }
                models.clone()
            }
        };
        let mut seen = HashSet::new();
        let mut models = Vec::new();
        for model in requested {
            let key = (model.provider_id, model.model);
            if !catalog.contains_key(&key) {
                return Err(AppError::BadRequest(format!(
                    "model {}/{} is missing or disabled",
                    key.0, key.1
                )));
            }
            if seen.insert(key.clone()) {
                models.push(ExecutionModelRef {
                    provider_id: key.0,
                    model: key.1,
                });
            }
        }
        if models.is_empty() {
            return Err(AppError::ProviderUnavailable(
                "no enabled provider/model can participate in this execution".to_owned(),
            ));
        }

        if let Some(lead) = lead_model {
            let Some(index) = models.iter().position(|model| model == lead) else {
                return Err(AppError::BadRequest(
                    "lead_model must belong to the resolved execution model pool".to_owned(),
                ));
            };
            models.swap(0, index);
        }

        let mut snapshots = Vec::new();
        for model in &models {
            let description = catalog
                .get(&(model.provider_id.clone(), model.model.clone()))
                .cloned()
                .flatten()
                .map(|value| value.trim().to_owned())
                .filter(|value| !value.is_empty());
            snapshots.push(NewAgentExecutionParticipant {
                participant_id:
                    nomifun_common::AgentExecutionParticipantId::new().into_string(),
                source_agent_id: NOMI_AGENT_ID.to_owned(),
                preset_id: None,
                preset_revision: None,
                preset_snapshot: None,
                provider_id: Some(model.provider_id.clone()),
                model: Some(model.model.clone()),
                role: None,
                capability: Some(
                    serde_json::to_string(&ParticipantCapability {
                        strengths: vec![],
                        modalities: infer_model_modalities(&model.model),
                        tools: false,
                        reasoning: "medium".to_owned(),
                        cost_tier: "standard".to_owned(),
                        speed_tier: "standard".to_owned(),
                    })
                    .map_err(|error| AppError::Internal(format!("encode capability: {error}")))?,
                ),
                constraints: None,
                description,
                system_prompt: None,
                enabled_skills: "[]".to_owned(),
                disabled_builtin_skills: "[]".to_owned(),
                sort_order: snapshots.len() as i64,
            });
        }

        // Presets enrich routing but never widen the caller's model pool.
        let mut presets = match self.preset_service.list().await {
            Ok(presets) => presets,
            Err(error) => {
                tracing::warn!(%error, "participant resolution continuing without presets");
                return Ok(snapshots);
            }
        };
        presets.sort_by(|left, right| left.preset_id.cmp(&right.preset_id));
        for preset in presets
            .into_iter()
            .filter(|preset| preset.enabled && preset.auto_selectable)
        {
            if snapshots.len() >= MAX_AGENT_EXECUTION_PARTICIPANTS {
                tracing::warn!(
                    limit = MAX_AGENT_EXECUTION_PARTICIPANTS,
                    "execution participant budget reached; remaining automatic presets were not materialized"
                );
                break;
            }
            let resolved = match self
                .preset_service
                .resolve(
                    &preset.preset_id,
                    PresetTarget::ExecutionStep,
                    None,
                    PresetOverrides::default(),
                )
                .await
            {
                Ok(resolved) => resolved,
                Err(error) => {
                    tracing::warn!(preset_id = %preset.preset_id, %error, "skipping unresolved execution preset");
                    continue;
                }
            };
            let Some(resolved_model) = resolved.resolved_model.as_ref() else {
                continue;
            };
            let pair = models.iter().find(|candidate| {
                candidate.model == resolved_model.model
                    && resolved_model
                        .provider_id
                        .as_ref()
                        .is_none_or(|expected| expected == &candidate.provider_id)
            });
            let Some(pair) = pair else {
                continue;
            };
            let provider_id = pair.provider_id.clone();
            let model = pair.model.clone();
            let description = resolved
                .routing_description
                .clone()
                .or(preset.description.clone());
            let mut capability = derive_capability(
                &preset.audience_tags,
                &preset.scenario_tags,
                description.as_deref(),
                !resolved.included_skills.is_empty(),
            );
            for modality in infer_model_modalities(&model) {
                if !capability.modalities.contains(&modality) {
                    capability.modalities.push(modality);
                }
            }
            snapshots.push(NewAgentExecutionParticipant {
                participant_id:
                    nomifun_common::AgentExecutionParticipantId::new().into_string(),
                source_agent_id: resolved
                    .resolved_agent_id
                    .clone()
                    .unwrap_or_else(|| NOMI_AGENT_ID.to_owned()),
                preset_id: Some(preset.preset_id),
                preset_revision: Some(resolved.preset_revision),
                preset_snapshot: Some(serde_json::to_string(&resolved).map_err(|error| {
                    AppError::Internal(format!("encode preset snapshot: {error}"))
                })?),
                provider_id: Some(provider_id),
                model: Some(model),
                role: Some(preset.name),
                capability: Some(serde_json::to_string(&capability).map_err(|error| {
                    AppError::Internal(format!("encode participant capability: {error}"))
                })?),
                constraints: None,
                description,
                system_prompt: (!resolved.instructions.trim().is_empty())
                    .then_some(resolved.instructions.clone()),
                enabled_skills: serde_json::to_string(&resolved.included_skills).map_err(
                    |error| AppError::Internal(format!("encode participant skills: {error}")),
                )?,
                disabled_builtin_skills: serde_json::to_string(&resolved.excluded_auto_skills)
                    .map_err(|error| {
                        AppError::Internal(format!("encode participant exclusions: {error}"))
                    })?,
                sort_order: snapshots.len() as i64,
            });
        }
        Ok(snapshots)
    }

    /// Preserve the authenticated caller's frozen preset as the first Agent
    /// participant without widening the already-resolved model authority.
    pub(crate) fn prepend_frozen_lead(
        participants: &mut Vec<NewAgentExecutionParticipant>,
        snapshot: &ResolvedPresetSnapshot,
        lead_model: Option<&ExecutionModelRef>,
    ) -> Result<(), AppError> {
        if let Some(index) = participants.iter().position(|participant| {
            participant.preset_id.as_deref() == Some(snapshot.preset_id.as_str())
                && participant.preset_revision == Some(snapshot.preset_revision)
                && lead_model.is_none_or(|lead| {
                    participant.provider_id.as_deref() == Some(lead.provider_id.as_str())
                        && participant.model.as_deref() == Some(lead.model.as_str())
                })
        }) {
            let participant = participants.remove(index);
            participants.insert(0, participant);
            for (index, participant) in participants.iter_mut().enumerate() {
                participant.sort_order = index as i64;
            }
            return Ok(());
        }
        let model = lead_model
            .cloned()
            .or_else(|| {
                let resolved = snapshot.resolved_model.as_ref()?;
                Some(ExecutionModelRef {
                    provider_id: resolved.provider_id.clone()?,
                    model: resolved.model.clone(),
                })
            })
            .or_else(|| {
                participants.iter().find_map(|participant| {
                    Some(ExecutionModelRef {
                        provider_id: participant.provider_id.clone()?,
                        model: participant.model.clone()?,
                    })
                })
            })
            .ok_or_else(|| {
                AppError::BadRequest(
                    "the calling Agent preset has no model inside the execution authority"
                        .to_owned(),
                )
            })?;
        ExecutionModelPool::Single {
            model: model.clone(),
        }
        .validate()
        .map_err(AppError::BadRequest)?;
        let matching_model_index = participants.iter().position(|participant| {
            participant.provider_id.as_deref() == Some(model.provider_id.as_str())
                && participant.model.as_deref() == Some(model.model.as_str())
        });
        let Some(matching_model_index) = matching_model_index else {
            return Err(AppError::BadRequest(format!(
                "the calling Agent model {}/{} is outside the execution model pool",
                model.provider_id, model.model
            )));
        };

        // The authenticated frozen Agent is the concrete lead identity for
        // this model. Replace the first matching template/base participant at
        // every size so participant count and model authority never widen.
        participants.remove(matching_model_index);

        for participant in participants.iter_mut() {
            participant.sort_order += 1;
        }
        participants.insert(
            0,
            NewAgentExecutionParticipant {
                participant_id:
                    nomifun_common::AgentExecutionParticipantId::new().into_string(),
                source_agent_id: snapshot
                    .resolved_agent_id
                    .clone()
                    .unwrap_or_else(|| NOMI_AGENT_ID.to_owned()),
                preset_id: Some(snapshot.preset_id.clone()),
                preset_revision: Some(snapshot.preset_revision),
                preset_snapshot: Some(serde_json::to_string(snapshot).map_err(|error| {
                    AppError::Internal(format!("encode calling Agent preset snapshot: {error}"))
                })?),
                provider_id: Some(model.provider_id),
                model: Some(model.model),
                role: Some(snapshot.preset_name.clone()),
                capability: Some(
                    serde_json::to_string(&derive_capability(
                        &[],
                        &[],
                        snapshot.routing_description.as_deref(),
                        !snapshot.included_skills.is_empty(),
                    ))
                    .map_err(|error| {
                        AppError::Internal(format!("encode calling Agent capability: {error}"))
                    })?,
                ),
                constraints: None,
                description: snapshot.routing_description.clone(),
                system_prompt: (!snapshot.instructions.trim().is_empty())
                    .then(|| snapshot.instructions.clone()),
                enabled_skills: serde_json::to_string(&snapshot.included_skills).map_err(
                    |error| {
                        AppError::Internal(format!("encode calling Agent skills: {error}"))
                    },
                )?,
                disabled_builtin_skills: serde_json::to_string(
                    &snapshot.excluded_auto_skills,
                )
                .map_err(|error| {
                    AppError::Internal(format!(
                        "encode calling Agent builtin exclusions: {error}"
                    ))
                })?,
                sort_order: 0,
            },
        );
        Ok(())
    }

    /// Make an explicitly selected calling model the deterministic planner
    /// lead of a template without adding a participant or widening authority.
    pub(crate) fn promote_lead_model(
        &self,
        participants: &mut Vec<NewAgentExecutionParticipant>,
        lead_model: &ExecutionModelRef,
    ) -> Result<(), AppError> {
        promote_model_to_front(participants, lead_model)
    }
}

fn promote_model_to_front(
    participants: &mut Vec<NewAgentExecutionParticipant>,
    lead_model: &ExecutionModelRef,
) -> Result<(), AppError> {
    ExecutionModelPool::Single {
        model: lead_model.clone(),
    }
    .validate()
    .map_err(AppError::BadRequest)?;
    let index = participants
        .iter()
        .position(|participant| {
            participant.provider_id.as_deref() == Some(lead_model.provider_id.as_str())
                && participant.model.as_deref() == Some(lead_model.model.as_str())
        })
        .ok_or_else(|| {
            AppError::BadRequest(format!(
                "the calling Agent model {}/{} is not present in the selected collaboration template",
                lead_model.provider_id, lead_model.model
            ))
        })?;
    let participant = participants.remove(index);
    participants.insert(0, participant);
    for (index, participant) in participants.iter_mut().enumerate() {
        participant.sort_order = index as i64;
    }
    Ok(())
}

fn derive_capability(
    audience_tags: &[String],
    scenario_tags: &[String],
    description: Option<&str>,
    has_skills: bool,
) -> ParticipantCapability {
    const KEYWORDS: &[(&str, &str)] = &[
        ("cod", "coding"),
        ("program", "coding"),
        ("develop", "coding"),
        ("writ", "writing"),
        ("文案", "writing"),
        ("research", "research"),
        ("调研", "research"),
        ("search", "research"),
        ("analy", "analysis"),
        ("分析", "analysis"),
        ("design", "design"),
        ("设计", "design"),
        ("translat", "translation"),
        ("翻译", "translation"),
        ("plan", "planning"),
        ("规划", "planning"),
    ];
    let mut inputs: Vec<String> = audience_tags
        .iter()
        .chain(scenario_tags)
        .map(|value| value.to_lowercase())
        .collect();
    if let Some(description) = description {
        inputs.push(description.to_lowercase());
    }
    let mut strengths = Vec::new();
    for (needle, strength) in KEYWORDS {
        if inputs.iter().any(|value| value.contains(needle))
            && !strengths.iter().any(|value| value == strength)
        {
            strengths.push((*strength).to_owned());
        }
    }
    ParticipantCapability {
        strengths,
        modalities: vec![],
        tools: has_skills,
        reasoning: "medium".to_owned(),
        cost_tier: "standard".to_owned(),
        speed_tier: "standard".to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nomifun_api_types::{PresetKnowledgePolicy, PresetTarget};

    const PROVIDER_1: &str = "0190f5fe-7c00-7a00-8000-000000000001";
    const PROVIDER_2: &str = "0190f5fe-7c00-7a00-8000-000000000002";
    const LEAD_PROVIDER: &str = "0190f5fe-7c00-7a00-8000-000000000010";
    const OUTSIDE_PROVIDER: &str = "0190f5fe-7c00-7a00-8000-000000000099";
    const NOMI_AGENT_ID: &str = "0190f5fe-7c00-7a00-8000-000000000114";
    const LEAD_PRESET_ID: &str = "0190f5fe-7c00-7a00-8000-000000000115";

    fn participant(
        participant_id: &str,
        provider_id: &str,
        model: &str,
        sort_order: i64,
    ) -> NewAgentExecutionParticipant {
        NewAgentExecutionParticipant {
            participant_id: participant_id.to_owned(),
            source_agent_id: NOMI_AGENT_ID.to_owned(),
            preset_id: None,
            preset_revision: None,
            preset_snapshot: None,
            provider_id: Some(provider_id.to_owned()),
            model: Some(model.to_owned()),
            role: None,
            capability: None,
            constraints: None,
            description: None,
            system_prompt: None,
            enabled_skills: "[]".to_owned(),
            disabled_builtin_skills: "[]".to_owned(),
            sort_order,
        }
    }

    fn snapshot() -> ResolvedPresetSnapshot {
        ResolvedPresetSnapshot {
            preset_id: LEAD_PRESET_ID.to_owned(),
            preset_revision: 7,
            preset_name: "Lead".to_owned(),
            target: PresetTarget::ExecutionStep,
            routing_description: None,
            instructions: "lead instructions".to_owned(),
            resolved_agent_id: Some(NOMI_AGENT_ID.to_owned()),
            resolved_agent_type: None,
            resolved_agent_backend: None,
            resolved_model: None,
            included_skills: vec![],
            excluded_auto_skills: vec![],
            knowledge_policy: PresetKnowledgePolicy::default(),
            knowledge_base_ids: vec![],
            warnings: vec![],
        }
    }

    #[test]
    fn explicit_template_lead_is_promoted_without_widening_authority() {
        let mut participants = vec![
            participant("0190f5fe-7c00-7a00-8000-000000000101", PROVIDER_1, "m1", 0),
            participant("0190f5fe-7c00-7a00-8000-000000000102", PROVIDER_2, "m2", 1),
        ];
        promote_model_to_front(
            &mut participants,
            &ExecutionModelRef {
                provider_id: PROVIDER_2.to_owned(),
                model: "m2".to_owned(),
            },
        )
        .unwrap();

        assert_eq!(participants.len(), 2);
        assert_eq!(
            participants[0].participant_id,
            "0190f5fe-7c00-7a00-8000-000000000102"
        );
        assert_eq!(participants[0].sort_order, 0);
        assert_eq!(participants[1].sort_order, 1);
    }

    #[test]
    fn explicit_template_lead_must_belong_to_the_template() {
        let mut participants = vec![participant(
            "0190f5fe-7c00-7a00-8000-000000000101",
            PROVIDER_1,
            "m1",
            0,
        )];
        let error = promote_model_to_front(
            &mut participants,
            &ExecutionModelRef {
                provider_id: OUTSIDE_PROVIDER.to_owned(),
                model: "m2".to_owned(),
            },
        )
        .unwrap_err();
        assert!(matches!(error, AppError::BadRequest(_)));
        assert_eq!(participants.len(), 1);
    }

    #[test]
    fn frozen_lead_replaces_a_matching_model_at_every_template_size() {
        for size in [2_usize, MAX_AGENT_EXECUTION_PARTICIPANTS] {
            let mut participants = vec![
                participant(
                    "0190f5fe-7c00-7a00-8000-000000000101",
                    PROVIDER_2,
                    "m2",
                    0,
                ),
                participant(
                    "0190f5fe-7c00-7a00-8000-000000000102",
                    LEAD_PROVIDER,
                    "lead-model",
                    1,
                ),
            ];
            while participants.len() < size {
                let index = participants.len();
                participants.push(participant(
                    &generate_id(),
                    PROVIDER_2,
                    &format!("model-{index}"),
                    index as i64,
                ));
            }

            ParticipantResolver::prepend_frozen_lead(
                &mut participants,
                &snapshot(),
                Some(&ExecutionModelRef {
                    provider_id: LEAD_PROVIDER.to_owned(),
                    model: "lead-model".to_owned(),
                }),
            )
            .unwrap();

            assert_eq!(participants.len(), size);
            assert_eq!(
                participants[0].preset_id.as_deref(),
                Some(LEAD_PRESET_ID)
            );
            assert_eq!(participants[0].sort_order, 0);
            assert!(!participants.iter().any(|participant| {
                participant.participant_id == "0190f5fe-7c00-7a00-8000-000000000102"
            }));
        }
    }

    #[test]
    fn frozen_lead_replaces_the_first_same_model_participant_deterministically() {
        let mut participants = vec![
            participant(
                "0190f5fe-7c00-7a00-8000-000000000101",
                LEAD_PROVIDER,
                "lead-model",
                0,
            ),
            participant(
                "0190f5fe-7c00-7a00-8000-000000000102",
                LEAD_PROVIDER,
                "lead-model",
                1,
            ),
            participant(
                "0190f5fe-7c00-7a00-8000-000000000103",
                PROVIDER_2,
                "m2",
                2,
            ),
        ];
        ParticipantResolver::prepend_frozen_lead(
            &mut participants,
            &snapshot(),
            Some(&ExecutionModelRef {
                provider_id: LEAD_PROVIDER.to_owned(),
                model: "lead-model".to_owned(),
            }),
        )
        .unwrap();

        assert_eq!(participants.len(), 3);
        assert_eq!(
            participants[0].preset_id.as_deref(),
            Some(LEAD_PRESET_ID)
        );
        assert!(!participants.iter().any(|participant| {
            participant.participant_id == "0190f5fe-7c00-7a00-8000-000000000101"
        }));
        assert!(participants.iter().any(|participant| {
            participant.participant_id == "0190f5fe-7c00-7a00-8000-000000000102"
        }));
    }
}

//! IDMM business logic: config persistence (conversation `extra.idmm` /
//! `terminal_sessions.idmm`), state assembly, global settings, and the
//! `ConfigReader` + `ProbeFactory` impls that let `IdmmManager` (re)build probes
//! and read config lazily. No axum here.
//!
//! Construction is layered to avoid a cycle: `ProbeDeps` (probe build + config
//! read) needs no manager; it backs the factory/config-reader, those back the
//! `IdmmManager`, and the `IdmmService` composes `ProbeDeps` + sidecar + manager.

use std::sync::Arc;

use async_trait::async_trait;
use nomifun_ai_agent::runtime_registry::AgentRuntimeRegistry;
use nomifun_api_types::{IdmmConfig, IdmmSettings, IdmmState, IdmmTargetKind, InterventionRecord};
use nomifun_common::{AppError, ConversationId, TerminalId, UserId};
use nomifun_conversation::ConversationService;
use nomifun_db::models::IdmmInterventionRow;
use nomifun_db::{IClientPreferenceRepository, IConversationRepository, IIdmmInterventionRepository};
use nomifun_terminal::TerminalDriver;

use crate::probe::{ConversationProbe, SessionProbe, TerminalProbe};
use crate::sidecar::{PREF_BACKUP_MODEL, PREF_BACKUP_PROVIDER, PREF_DEFAULT_STEERING, SidecarClient};
use crate::supervisor::{ConfigReader, IdmmManager, ProbeFactory, build_state};

/// Public log/activity reads are deliberately bounded even if an internal
/// caller forgets to clamp an HTTP/tool parameter.  The durable feed itself is
/// capped independently by the repository janitor.
const MAX_ACTIVITY_READ_LIMIT: i64 = 500;

/// Validate the kind-agnostic IDMM target handle in its declared entity domain.
fn validate_target_id(kind: IdmmTargetKind, target_id: &str) -> Result<&str, AppError> {
    let valid = match kind {
        IdmmTargetKind::Conversation => ConversationId::try_from(target_id).is_ok(),
        IdmmTargetKind::Terminal => TerminalId::try_from(target_id).is_ok(),
    };
    valid
        .then_some(target_id)
        .ok_or_else(|| AppError::NotFound(format!("session {target_id}")))
}

/// Map a persisted row to the API/WS `InterventionRecord` DTO.
fn row_to_record(row: IdmmInterventionRow) -> Result<InterventionRecord, AppError> {
    Ok(InterventionRecord {
        intervention_id: nomifun_common::IdmmInterventionId::parse(row.intervention_id)
            .map_err(|error| AppError::Internal(format!("invalid stored intervention_id: {error}")))?,
        target_kind: row.target_kind,
        target_id: row.target_id,
        watch: row.watch,
        at: row.at,
        stall_class: row.signal,
        tier_used: row.tier_used,
        category: row.category,
        action: row.action,
        detail: row.detail,
        outcome: row.outcome,
        reason: row.reason,
        confidence: row.confidence.map(|c| c as f32),
        bypass_model: row.bypass_model,
    })
}

/// Collaborators needed to build probes + read config (NO manager; breaks the
/// construction cycle). Shared by the factory, config-reader, and service.
pub struct ProbeDeps {
    pub conversation_service: ConversationService,
    pub conversation_repo: Arc<dyn IConversationRepository>,
    pub terminal_driver: Arc<dyn TerminalDriver>,
    pub runtime_registry: Arc<dyn AgentRuntimeRegistry>,
}

impl ProbeDeps {
    /// Read the persisted per-session config (default when none / store absent).
    pub async fn read_config(&self, kind: IdmmTargetKind, target_id: &str) -> Result<IdmmConfig, AppError> {
        let raw: Option<serde_json::Value> = match kind {
            IdmmTargetKind::Conversation => {
                let Some(row) = self.conversation_repo.get(validate_target_id(kind, target_id)?).await? else {
                    return Ok(IdmmConfig::default());
                };
                let extra: serde_json::Value = serde_json::from_str(&row.extra).unwrap_or_default();
                extra.get("idmm").cloned()
            }
            IdmmTargetKind::Terminal => match self
                .terminal_driver
                .read_idmm(validate_target_id(kind, target_id)?)
                .await
                .map_err(|e| AppError::Internal(format!("read_idmm failed: {e}")))?
            {
                Some(s) => serde_json::from_str(&s).ok(),
                None => None,
            },
        };
        Ok(raw.and_then(|v| serde_json::from_value(v).ok()).unwrap_or_default())
    }

    /// Resolve the target's authoritative persisted owner. This is shared by
    /// API authorization and the background supervisor so they cannot drift.
    async fn target_owner(&self, kind: IdmmTargetKind, target_id: &str) -> Result<String, AppError> {
        let owner_id = match kind {
            IdmmTargetKind::Conversation => self
                .conversation_repo
                .get(validate_target_id(kind, target_id)?)
                .await?
                .ok_or_else(|| AppError::NotFound(format!("conversation {target_id} not found")))?
                .user_id,
            IdmmTargetKind::Terminal => self
                .terminal_driver
                .describe(validate_target_id(kind, target_id)?)
                .await
                .map_err(|e| AppError::Internal(format!("describe failed: {e}")))?
                .ok_or_else(|| AppError::NotFound(format!("terminal {target_id} not found")))?
                .user_id,
        };
        UserId::parse(&owner_id)
            .map(UserId::into_string)
            .map_err(|error| AppError::Internal(format!("IDMM target has invalid owner: {error}")))
    }

    async fn verify_target_owner(
        &self,
        kind: IdmmTargetKind,
        target_id: &str,
        user_id: &str,
    ) -> Result<(), AppError> {
        require_user_id(user_id)?;
        if self.target_owner(kind, target_id).await? != user_id {
            return Err(AppError::Forbidden("not your IDMM target".into()));
        }
        Ok(())
    }

    fn build_probe(&self, kind: IdmmTargetKind, target_id: &str) -> Option<Arc<dyn SessionProbe>> {
        validate_target_id(kind, target_id).ok()?;
        match kind {
            IdmmTargetKind::Conversation => Some(Arc::new(ConversationProbe {
                runtime_registry: self.runtime_registry.clone(),
                conversation_service: self.conversation_service.clone(),
                conversation_repo: self.conversation_repo.clone(),
                conversation_id: ConversationId::parse(target_id).ok()?,
            })),
            IdmmTargetKind::Terminal => {
                // An invalid terminal target cannot map to a PTY, so no probe
                // is constructed.
                Some(Arc::new(TerminalProbe::new(
                    self.terminal_driver.clone(),
                    TerminalId::parse(target_id).ok()?,
                )))
            }
        }
    }
}

impl ProbeFactory for ProbeDeps {
    fn build(&self, kind: IdmmTargetKind, target_id: &str) -> Option<Arc<dyn SessionProbe>> {
        self.build_probe(kind, target_id)
    }
}

#[async_trait]
impl ConfigReader for ProbeDeps {
    async fn read(
        &self,
        user_id: &str,
        kind: IdmmTargetKind,
        target_id: &str,
    ) -> Result<IdmmConfig, AppError> {
        self.verify_target_owner(kind, target_id, user_id).await?;
        self.read_config(kind, target_id).await
    }
}

/// IDMM's API-facing service (config persistence, state, settings, log).
#[derive(Clone)]
pub struct IdmmService {
    probe_deps: Arc<ProbeDeps>,
    client_prefs: Arc<dyn IClientPreferenceRepository>,
    sidecar: Arc<SidecarClient>,
    manager: IdmmManager,
    records: Arc<dyn IIdmmInterventionRepository>,
}

impl IdmmService {
    pub fn new(
        probe_deps: Arc<ProbeDeps>,
        client_prefs: Arc<dyn IClientPreferenceRepository>,
        sidecar: Arc<SidecarClient>,
        manager: IdmmManager,
        records: Arc<dyn IIdmmInterventionRepository>,
    ) -> Self {
        Self {
            probe_deps,
            client_prefs,
            sidecar,
            manager,
            records,
        }
    }

    pub fn manager(&self) -> &IdmmManager {
        &self.manager
    }

    /// Whether the RulePlusModel tier has a resolvable bypass model: a per-watch
    /// override / global default, or, for a conversation target, the
    /// conversation's own selected model (which becomes the bypass model, so the
    /// model tier works with zero extra config on a plain chat). Terminals have
    /// no own callable model (their agent CLI owns the model), so they still need
    /// an explicit backup. Feeds both `validate` and the
    /// `sidecar_provider_resolved` state flag the frontend gates its toggle on.
    ///
    /// Checks both watches' bypass models (either resolving satisfies the
    /// requirement. `validate` only demands a backup when an enabled watch is on
    /// the model tier, and both watches resolve through the same global default).
    async fn sidecar_backup_resolvable(&self, kind: IdmmTargetKind, target_id: &str, cfg: &IdmmConfig) -> bool {
        if self.sidecar.backup_resolvable(&cfg.decision_watch.base.bypass_model).await
            || self.sidecar.backup_resolvable(&cfg.fault_watch.base.bypass_model).await
        {
            return true;
        }
        if kind == IdmmTargetKind::Conversation
            && let Ok(id) = ConversationId::try_from(target_id)
            && let Ok(Some(row)) = self.probe_deps.conversation_repo.get(id.as_ref()).await
        {
            return nomifun_conversation::runtime_options::provider_model_from_conversation_row(&row)
                .is_ok_and(|model| model.is_some());
        }
        false
    }

    // -- Config persistence -------------------------------------------------

    /// Validate + persist a per-session config, then arm/stop supervision.
    pub async fn save_config(
        &self,
        user_id: &str,
        kind: IdmmTargetKind,
        target_id: &str,
        cfg: &IdmmConfig,
    ) -> Result<(), AppError> {
        self.verify_target_owner(kind, target_id, user_id).await?;
        let backup_resolvable = self.sidecar_backup_resolvable(kind, target_id, cfg).await;
        crate::config::validate(cfg, backup_resolvable).map_err(AppError::BadRequest)?;

        match kind {
            IdmmTargetKind::Conversation => {
                let blob =
                    serde_json::to_string(cfg).map_err(|e| AppError::Internal(e.to_string()))?;
                self.probe_deps
                    .conversation_repo
                    .update_idmm(validate_target_id(kind, target_id)?, Some(&blob))
                    .await?;
            }
            IdmmTargetKind::Terminal => {
                let s = serde_json::to_string(cfg).map_err(|e| AppError::Internal(e.to_string()))?;
                self.probe_deps
                    .terminal_driver
                    .write_idmm(validate_target_id(kind, target_id)?, Some(&s))
                    .await
                    .map_err(|e| AppError::Internal(format!("write_idmm failed: {e}")))?;
            }
        }

        if cfg.any_enabled() {
            self.manager.ensure(kind, target_id).await;
        } else {
            self.manager.stop(kind, target_id);
        }
        Ok(())
    }

    /// Read the persisted per-session config. Returns `Ok(None)` when no
    /// config has been saved for this target (the frontend should then seed
    /// the form from `IdmmSettings.default_steering_prompt` instead of from a
    /// blank `IdmmConfig::default()`).
    pub async fn read_config_persisted(
        &self,
        user_id: &str,
        kind: IdmmTargetKind,
        target_id: &str,
    ) -> Result<Option<IdmmConfig>, AppError> {
        self.verify_target_owner(kind, target_id, user_id).await?;
        self.read_config_persisted_unchecked(kind, target_id).await
    }

    /// Read after the caller has crossed the owner boundary.  Kept private so
    /// every API/capability path must carry an authenticated `user_id`.
    async fn read_config_persisted_unchecked(
        &self,
        kind: IdmmTargetKind,
        target_id: &str,
    ) -> Result<Option<IdmmConfig>, AppError> {
        let raw: Option<serde_json::Value> = match kind {
            IdmmTargetKind::Conversation => {
                let Some(row) = self
                    .probe_deps
                    .conversation_repo
                    .get(validate_target_id(kind, target_id)?)
                    .await?
                else {
                    return Ok(None);
                };
                let extra: serde_json::Value = serde_json::from_str(&row.extra).unwrap_or_default();
                extra.get("idmm").cloned()
            }
            IdmmTargetKind::Terminal => match self
                .probe_deps
                .terminal_driver
                .read_idmm(validate_target_id(kind, target_id)?)
                .await
                .map_err(|e| AppError::Internal(format!("read_idmm failed: {e}")))?
            {
                Some(s) => serde_json::from_str(&s).ok(),
                None => None,
            },
        };
        Ok(raw.and_then(|v| serde_json::from_value(v).ok()))
    }

    /// Assemble the live state (config + manager runtime + backup resolvability).
    /// Includes the persisted config (when one exists) so the frontend can
    /// rehydrate its form without losing user input on remount (Req4).
    pub async fn build_state(
        &self,
        user_id: &str,
        kind: IdmmTargetKind,
        target_id: &str,
    ) -> Result<IdmmState, AppError> {
        self.verify_target_owner(kind, target_id, user_id).await?;
        let persisted = self.read_config_persisted_unchecked(kind, target_id).await?;
        let cfg = persisted.clone().unwrap_or_default();
        let shared = self.manager.shared_for(kind, target_id);
        let resolved = self.sidecar_backup_resolvable(kind, target_id, &cfg).await;
        Ok(build_state(
            &shared,
            kind,
            target_id,
            &cfg,
            resolved,
            persisted.as_ref(),
        ))
    }

    /// Recent intervention log for a target (most-recent-first), read from the
    /// persisted audit table; the DB is the sole source of truth (the supervisor
    /// itself keeps only live counters, not a record ring). `limit` caps the rows.
    pub async fn log(
        &self,
        user_id: &str,
        kind: IdmmTargetKind,
        target_id: &str,
        limit: i64,
    ) -> Result<Vec<InterventionRecord>, AppError> {
        self.verify_target_owner(kind, target_id, user_id).await?;
        let limit = limit.clamp(1, MAX_ACTIVITY_READ_LIMIT);
        let rows = self
            .records
            .list_for_target(user_id, kind.as_str(), target_id, limit)
            .await?;
        rows.into_iter().map(row_to_record).collect()
    }

    /// Clear all persisted intervention records for a target. Returns the count
    /// removed. Manual log clearing and the session-delete cascade both route here.
    pub async fn clear_log(
        &self,
        user_id: &str,
        kind: IdmmTargetKind,
        target_id: &str,
    ) -> Result<u64, AppError> {
        self.verify_target_owner(kind, target_id, user_id).await?;
        Ok(self
            .records
            .delete_for_target(user_id, kind.as_str(), target_id)
            .await?)
    }

    /// Cross-session feed for one authenticated owner (most-recent-first).
    pub async fn recent_activity(
        &self,
        user_id: &str,
        limit: i64,
    ) -> Result<Vec<InterventionRecord>, AppError> {
        require_user_id(user_id)?;
        let rows = self
            .records
            .list_recent(user_id, limit.clamp(1, MAX_ACTIVITY_READ_LIMIT))
            .await?;
        rows.into_iter().map(row_to_record).collect()
    }

    /// Clear one owner's activity across all of their targets.
    pub async fn clear_activity(&self, user_id: &str) -> Result<u64, AppError> {
        require_user_id(user_id)?;
        Ok(self.records.clear_all(user_id).await?)
    }

    /// Force one ladder pass now (manual "act now"): ensures supervision is
    /// running; the actual pass happens on the next observed signal.
    pub async fn intervene_now(
        &self,
        user_id: &str,
        kind: IdmmTargetKind,
        target_id: &str,
    ) -> Result<(), AppError> {
        self.verify_target_owner(kind, target_id, user_id).await?;
        self.manager.ensure(kind, target_id).await;
        Ok(())
    }

    // -- Global settings (client_preferences) -------------------------------

    pub async fn get_settings(&self) -> Result<IdmmSettings, AppError> {
        let rows = self
            .client_prefs
            .get_by_keys(&[PREF_BACKUP_PROVIDER, PREF_BACKUP_MODEL, PREF_DEFAULT_STEERING])
            .await?;
        let mut s = IdmmSettings::default();
        for r in rows {
            match r.key.as_str() {
                PREF_BACKUP_PROVIDER => {
                    nomifun_common::ProviderId::parse(&r.value).map_err(|error| {
                        AppError::Internal(format!(
                            "stored IDMM backup provider id is invalid: {error}"
                        ))
                    })?;
                    s.backup_provider_id = Some(r.value);
                }
                PREF_BACKUP_MODEL if !r.value.trim().is_empty() => s.backup_model = Some(r.value),
                PREF_DEFAULT_STEERING => s.default_steering_prompt = r.value,
                _ => {}
            }
        }
        Ok(s)
    }

    pub async fn set_settings(&self, settings: &IdmmSettings) -> Result<(), AppError> {
        let mut entries: Vec<(&str, &str)> = Vec::new();
        let provider = settings.backup_provider_id.as_deref().map(|provider_id| {
            nomifun_common::ProviderId::parse(provider_id).map_err(|error| {
                AppError::BadRequest(format!("invalid backup provider id: {error}"))
            })
        }).transpose()?.map(|id| id.into_string());
        let model = settings
            .backup_model
            .as_deref()
            .map(str::trim)
            .filter(|m| !m.is_empty());
        let mut delete_keys: Vec<&str> = Vec::new();
        if let Some(p) = provider.as_deref() {
            entries.push((PREF_BACKUP_PROVIDER, p));
        } else {
            delete_keys.push(PREF_BACKUP_PROVIDER);
        }
        if provider.is_some() {
            if let Some(m) = model {
                entries.push((PREF_BACKUP_MODEL, m));
            } else {
                delete_keys.push(PREF_BACKUP_MODEL);
            }
        } else {
            delete_keys.push(PREF_BACKUP_MODEL);
        }
        entries.push((PREF_DEFAULT_STEERING, settings.default_steering_prompt.as_str()));
        if !delete_keys.is_empty() {
            self.client_prefs.delete_keys(&delete_keys).await?;
        }
        self.client_prefs.upsert_batch(&entries).await?;
        Ok(())
    }

    /// Verify an IDMM target against its authoritative persisted owner before
    /// reading config/log state or mutating supervision. Both target kinds go
    /// through this one boundary so a newly added route cannot accidentally
    /// protect terminals while exposing conversations.
    pub async fn verify_target_owner(
        &self,
        kind: IdmmTargetKind,
        target_id: &str,
        user_id: &str,
    ) -> Result<(), AppError> {
        require_user_id(user_id)?;
        self.probe_deps
            .verify_target_owner(kind, target_id, user_id)
            .await
    }
}

fn require_user_id(user_id: &str) -> Result<(), AppError> {
    UserId::parse(user_id)
        .map(|_| ())
        .map_err(|_| AppError::Forbidden("invalid IDMM owner identity".into()))
}

// Service-level persistence + validation + settings are covered end-to-end by
// `nomifun-app/tests/idmm_e2e.rs` against a real in-memory database (per
// AGENTS.md: prefer a real DB over brittle stubs of the agent/conversation
// stack). The pure pieces (config validation, policy, detector, sidecar) are
// unit-tested in their own modules.

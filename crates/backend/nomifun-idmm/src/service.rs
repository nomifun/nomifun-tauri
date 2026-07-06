//! IDMM business logic: config persistence (conversation `extra.idmm` /
//! `terminal_sessions.idmm`), state assembly, global settings, and the
//! `ConfigReader` + `ProbeFactory` impls that let `IdmmManager` (re)build probes
//! and read config lazily. No axum here.
//!
//! Construction is layered to avoid a cycle: `ProbeDeps` (probe build + config
//! read) needs no manager → it backs the factory/config-reader → those back the
//! `IdmmManager` → the `IdmmService` composes `ProbeDeps` + sidecar + manager.

use std::sync::Arc;

use async_trait::async_trait;
use nomifun_ai_agent::task_manager::IWorkerTaskManager;
use nomifun_api_types::{IdmmConfig, IdmmSettings, IdmmState, IdmmTargetKind, InterventionRecord};
use nomifun_common::AppError;
use nomifun_conversation::ConversationService;
use nomifun_db::models::IdmmInterventionRow;
use nomifun_db::{IClientPreferenceRepository, IConversationRepository, IIdmmInterventionRepository};
use nomifun_terminal::TerminalDriver;

use crate::probe::{ConversationProbe, SessionProbe, TerminalProbe};
use crate::sidecar::{PREF_BACKUP_MODEL, PREF_BACKUP_PROVIDER, PREF_DEFAULT_STEERING, SidecarClient};
use crate::supervisor::{ConfigReader, IdmmManager, ProbeFactory, build_state};

const SYSTEM_DEFAULT_USER_ID: &str = "system_default_user";

/// Parse an IDMM string `target_id` (the kind-agnostic target handle on the
/// IDMM DTO) into the integer key the conversation repo / terminal driver now
/// use. A non-numeric id yields an explicit NotFound (spec §2.5/§7.4).
fn parse_target_id(target_id: &str) -> Result<i64, AppError> {
    target_id
        .parse::<i64>()
        .map_err(|_| AppError::NotFound(format!("session {target_id}")))
}

/// Map a persisted row to the API/WS `InterventionRecord` DTO.
fn row_to_record(row: IdmmInterventionRow) -> InterventionRecord {
    InterventionRecord {
        id: row.id,
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
    }
}

/// Collaborators needed to build probes + read config (NO manager → breaks the
/// construction cycle). Shared by the factory, config-reader, and service.
pub struct ProbeDeps {
    pub conversation_service: ConversationService,
    pub conversation_repo: Arc<dyn IConversationRepository>,
    pub terminal_driver: Arc<dyn TerminalDriver>,
    pub task_manager: Arc<dyn IWorkerTaskManager>,
}

impl ProbeDeps {
    /// Read the persisted per-session config (default when none / store absent).
    pub async fn read_config(&self, kind: IdmmTargetKind, target_id: &str) -> Result<IdmmConfig, AppError> {
        let raw: Option<serde_json::Value> = match kind {
            IdmmTargetKind::Conversation => {
                let Some(row) = self.conversation_repo.get(parse_target_id(target_id)?).await? else {
                    return Ok(IdmmConfig::default());
                };
                let extra: serde_json::Value = serde_json::from_str(&row.extra).unwrap_or_default();
                extra.get("idmm").cloned()
            }
            IdmmTargetKind::Terminal => match self
                .terminal_driver
                .read_idmm(parse_target_id(target_id)?)
                .await
                .map_err(|e| AppError::Internal(format!("read_idmm failed: {e}")))?
            {
                Some(s) => serde_json::from_str(&s).ok(),
                None => None,
            },
        };
        Ok(raw.and_then(|v| serde_json::from_value(v).ok()).unwrap_or_default())
    }

    fn build_probe(&self, kind: IdmmTargetKind, target_id: &str) -> Option<Arc<dyn SessionProbe>> {
        match kind {
            IdmmTargetKind::Conversation => Some(Arc::new(ConversationProbe {
                task_manager: self.task_manager.clone(),
                conversation_service: self.conversation_service.clone(),
                conversation_repo: self.conversation_repo.clone(),
                conversation_id: target_id.to_string(),
                user_id: SYSTEM_DEFAULT_USER_ID.to_string(),
            })),
            IdmmTargetKind::Terminal => {
                // A non-numeric terminal target cannot map to a PTY → no probe.
                let id = target_id.parse::<i64>().ok()?;
                Some(Arc::new(TerminalProbe::new(self.terminal_driver.clone(), id)))
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
    async fn read(&self, kind: IdmmTargetKind, target_id: &str) -> IdmmConfig {
        self.read_config(kind, target_id).await.unwrap_or_default()
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
    /// override / global default, OR — for a conversation target — the
    /// conversation's own selected model (which becomes the bypass model, so the
    /// model tier works with zero extra config on a plain chat). Terminals have
    /// no own callable model (their agent CLI owns the model), so they still need
    /// an explicit backup. Feeds both `validate` and the
    /// `sidecar_provider_resolved` state flag the frontend gates its toggle on.
    ///
    /// Checks both watches' bypass models (either resolving satisfies the
    /// requirement — `validate` only demands a backup when an enabled watch is on
    /// the model tier, and both watches resolve through the same global default).
    async fn sidecar_backup_resolvable(&self, kind: IdmmTargetKind, target_id: &str, cfg: &IdmmConfig) -> bool {
        if self.sidecar.backup_resolvable(&cfg.decision_watch.base.bypass_model).await
            || self.sidecar.backup_resolvable(&cfg.fault_watch.base.bypass_model).await
        {
            return true;
        }
        if kind == IdmmTargetKind::Conversation
            && let Ok(id) = parse_target_id(target_id)
            && let Ok(Some(row)) = self.probe_deps.conversation_repo.get(id).await
        {
            let pm = nomifun_conversation::task_options::provider_model_from_conversation_row(&row);
            return !pm.provider_id.trim().is_empty();
        }
        false
    }

    // ── Config persistence ──

    /// Validate + persist a per-session config, then arm/stop supervision.
    pub async fn save_config(&self, kind: IdmmTargetKind, target_id: &str, cfg: &IdmmConfig) -> Result<(), AppError> {
        let backup_resolvable = self.sidecar_backup_resolvable(kind, target_id, cfg).await;
        crate::config::validate(cfg, backup_resolvable).map_err(AppError::BadRequest)?;

        match kind {
            IdmmTargetKind::Conversation => {
                let blob = serde_json::to_value(cfg).map_err(|e| AppError::Internal(e.to_string()))?;
                self.probe_deps
                    .conversation_service
                    .update_extra(target_id, serde_json::json!({ "idmm": blob }))
                    .await?;
            }
            IdmmTargetKind::Terminal => {
                let s = serde_json::to_string(cfg).map_err(|e| AppError::Internal(e.to_string()))?;
                self.probe_deps
                    .terminal_driver
                    .write_idmm(parse_target_id(target_id)?, Some(&s))
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
        kind: IdmmTargetKind,
        target_id: &str,
    ) -> Result<Option<IdmmConfig>, AppError> {
        let raw: Option<serde_json::Value> = match kind {
            IdmmTargetKind::Conversation => {
                let Some(row) = self.probe_deps.conversation_repo.get(parse_target_id(target_id)?).await? else {
                    return Ok(None);
                };
                let extra: serde_json::Value = serde_json::from_str(&row.extra).unwrap_or_default();
                extra.get("idmm").cloned()
            }
            IdmmTargetKind::Terminal => match self
                .probe_deps
                .terminal_driver
                .read_idmm(parse_target_id(target_id)?)
                .await
                .map_err(|e| AppError::Internal(format!("read_idmm failed: {e}")))?
            {
                Some(s) => serde_json::from_str(&s).ok(),
                None => None,
            },
        };
        Ok(raw.and_then(|v| serde_json::from_value(v).ok()))
    }

    pub async fn read_config(&self, kind: IdmmTargetKind, target_id: &str) -> Result<IdmmConfig, AppError> {
        self.probe_deps.read_config(kind, target_id).await
    }

    /// Assemble the live state (config + manager runtime + backup resolvability).
    /// Includes the persisted config (when one exists) so the frontend can
    /// rehydrate its form without losing user input on remount (Req4).
    pub async fn build_state(&self, kind: IdmmTargetKind, target_id: &str) -> Result<IdmmState, AppError> {
        let persisted = self.read_config_persisted(kind, target_id).await?;
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
    /// persisted audit table — the DB is the sole source of truth (the supervisor
    /// itself keeps only live counters, not a record ring). `limit` caps the rows.
    pub async fn log(&self, kind: IdmmTargetKind, target_id: &str, limit: i64) -> Result<Vec<InterventionRecord>, AppError> {
        let rows = self
            .records
            .list_for_target(kind.as_str(), target_id, limit)
            .await?;
        Ok(rows.into_iter().map(row_to_record).collect())
    }

    /// Clear all persisted intervention records for a target. Returns the count
    /// removed. (Manual "清空记录" + the session-delete cascade both route here.)
    pub async fn clear_log(&self, kind: IdmmTargetKind, target_id: &str) -> Result<u64, AppError> {
        Ok(self
            .records
            .delete_for_target(kind.as_str(), target_id)
            .await?)
    }

    /// Cross-session recent intervention feed (most-recent-first across ALL
    /// targets), read from the persisted audit table. `limit` caps the rows.
    pub async fn recent_activity(&self, limit: i64) -> Result<Vec<InterventionRecord>, AppError> {
        let rows = self.records.list_recent(limit).await?;
        Ok(rows.into_iter().map(row_to_record).collect())
    }

    /// Clear EVERY persisted intervention record across all targets. Returns the
    /// count removed (manual "清空全部记录").
    pub async fn clear_all_activity(&self) -> Result<u64, AppError> {
        Ok(self.records.clear_all().await?)
    }

    /// Force one ladder pass now (manual "act now"): ensures supervision is
    /// running; the actual pass happens on the next observed signal.
    pub async fn intervene_now(&self, kind: IdmmTargetKind, target_id: &str) -> Result<(), AppError> {
        self.manager.ensure(kind, target_id).await;
        Ok(())
    }

    // ── Global settings (client_preferences) ──

    pub async fn get_settings(&self) -> Result<IdmmSettings, AppError> {
        let rows = self
            .client_prefs
            .get_by_keys(&[PREF_BACKUP_PROVIDER, PREF_BACKUP_MODEL, PREF_DEFAULT_STEERING])
            .await?;
        let mut s = IdmmSettings::default();
        for r in rows {
            match r.key.as_str() {
                PREF_BACKUP_PROVIDER if !r.value.trim().is_empty() => s.backup_provider_id = Some(r.value),
                PREF_BACKUP_MODEL if !r.value.trim().is_empty() => s.backup_model = Some(r.value),
                PREF_DEFAULT_STEERING => s.default_steering_prompt = r.value,
                _ => {}
            }
        }
        Ok(s)
    }

    pub async fn set_settings(&self, settings: &IdmmSettings) -> Result<(), AppError> {
        let mut entries: Vec<(&str, &str)> = Vec::new();
        let provider = settings
            .backup_provider_id
            .as_deref()
            .map(str::trim)
            .filter(|p| !p.is_empty());
        let model = settings
            .backup_model
            .as_deref()
            .map(str::trim)
            .filter(|m| !m.is_empty());
        let mut delete_keys: Vec<&str> = Vec::new();
        if let Some(p) = provider {
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

    /// Verify a terminal target belongs to `user_id` (data isolation).
    pub async fn verify_terminal_owner(&self, terminal_id: &str, user_id: &str) -> Result<(), AppError> {
        let desc = self
            .probe_deps
            .terminal_driver
            .describe(parse_target_id(terminal_id)?)
            .await
            .map_err(|e| AppError::Internal(format!("describe failed: {e}")))?
            .ok_or_else(|| AppError::NotFound(format!("terminal {terminal_id} not found")))?;
        if desc.user_id != user_id {
            return Err(AppError::Forbidden("not your terminal".into()));
        }
        Ok(())
    }
}

// Service-level persistence + validation + settings are covered end-to-end by
// `nomifun-app/tests/idmm_e2e.rs` against a real in-memory database (per
// AGENTS.md: prefer a real DB over brittle stubs of the agent/conversation
// stack). The pure pieces (config validation, policy, detector, sidecar) are
// unit-tested in their own modules.

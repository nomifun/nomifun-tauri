//! `CompanionService` bundles the shared config, the companion registry, the store,
//! collector stats, learner and the companion-thread manager into the single
//! object the routes layer talks to.

use std::path::PathBuf;
use std::sync::Arc;

use nomifun_common::{
    AppError, CompanionId, CompanionSkillId, ProviderId, ProviderUsage, ProviderUsageFeature,
    ProviderWithModel, SharedProviderLifecycleBarrier,
};
use nomifun_db::IProviderRepository;
use serde::Serialize;
use tokio::sync::{Mutex, RwLock};

use crate::collector::{self, Collector, SharedConfig};
use crate::archiver::Archiver;
use crate::companion::{CompanionThreads, build_companion_system_prompt};
use crate::events::CompanionEventEmitter;
use crate::evolution::{EvolutionEngine, NoopTranscriptSource};
use crate::gamify::level_for_xp;
use crate::learner::{Learner, CompanionCompleter};
use crate::profile::{CompanionProfileConfig, SharedCompanionConfig};
use crate::registry::{CompanionRegistry, json_merge_patch};
use crate::skill_sink::CompanionSkillStoreSink;
use crate::store::{CompanionThread, MemoryFilter, MemoryPage, MemoryScope, CompanionLearnRun, CompanionMemory, CompanionSkill, CompanionStore, CompanionSuggestion, SuggestionPage};
use nomifun_extension::skill_service::{self, SkillPaths, SkillScope};
use nomifun_extension::constants::SKILL_MANIFEST_FILE;

/// Map the nullable stored owner to the extension skill scope.
fn scope_for(scope_companion_id: Option<&str>) -> SkillScope {
    scope_companion_id
        .map(|id| SkillScope::Companion(id.to_owned()))
        .unwrap_or(SkillScope::Shared)
}

async fn validate_provider_reference(
    provider_repo: Option<&Arc<dyn IProviderRepository>>,
    model: Option<&ProviderWithModel>,
    label: &str,
) -> Result<(), AppError> {
    let (Some(provider_repo), Some(model)) = (provider_repo, model) else {
        return Ok(());
    };
    if provider_repo
        .find_by_id(&model.provider_id)
        .await
        .map_err(|error| AppError::Internal(format!("check {label} provider: {error}")))?
        .is_none()
    {
        return Err(AppError::NotFound(format!(
            "provider '{}' referenced by {label} not found",
            model.provider_id
        )));
    }
    Ok(())
}

/// A skill registry row + its SKILL.md `description` (frontmatter), flattened for the UI list.
#[derive(Debug, Clone, Serialize)]
pub struct CompanionSkillView {
    #[serde(flatten)]
    pub skill: CompanionSkill,
    pub description: String,
}

/// One page of skill list rows enriched with their SKILL.md descriptions.
#[derive(Debug, Clone, Serialize)]
pub struct CompanionSkillViewPage {
    pub items: Vec<CompanionSkillView>,
    pub total: i64,
}

/// A skill registry row + its raw SKILL.md body, for the in-app editor.
#[derive(Debug, Clone, Serialize)]
pub struct CompanionSkillContent {
    pub skill: CompanionSkill,
    pub content: String,
}

#[derive(Debug, Serialize)]
pub struct CompanionStatus {
    /// Which companion this status describes; `None` for the shared-only fallback.
    pub companion_id: Option<String>,
    pub xp: i64,
    pub level: i64,
    pub mood: String,
    pub memories_active: i64,
    pub memories_archived: i64,
    pub suggestions_new: i64,
    /// This companion's active (usable) skills — drives the "专精 N 技能" expertise badge.
    pub skills_active: i64,
    pub model_configured: bool,
    pub collect_any_enabled: bool,
    pub last_learn: Option<CompanionLearnRun>,
}

/// "What I learned this week" digest. Skills are per-companion; memories/learn-runs are
/// global (memory.db is one shared store, learn_runs has no companion column) — the UI labels this honestly.
#[derive(Debug, Serialize)]
pub struct CompanionWeeklyDigest {
    pub since_ms: i64,
    pub skills_learned: i64,
    pub skills_active_new: i64,
    pub memories_added: i64,
    pub learn_runs: i64,
    pub new_skill_names: Vec<String>,
    pub recent_summaries: Vec<String>,
}

#[derive(Debug, Serialize)]
pub struct SourceStats {
    pub source: String,
    pub today: u64,
    pub total: u64,
}

/// Post-delete cascade hook for companion removal. Registered by the app assembly
/// (e.g. knowledge-binding cleanup wrapping `KnowledgeService`) so this crate
/// stays free of those dependencies. Implementations must swallow their own
/// failures (warn, never panic) — the companion is already gone when they run.
#[async_trait::async_trait]
pub trait CompanionCleanupHook: Send + Sync {
    async fn on_companion_deleted(&self, companion_id: &str);
    /// Called after a companion's chat model changed (best-effort). Lets the host
    /// react to the single-source-of-truth model switch — e.g. clear the IM
    /// channel sessions bound to this companion so they recreate with the new model
    /// on the next inbound message. Default no-op so existing hooks (knowledge
    /// cleanup) need not implement it.
    async fn on_companion_model_changed(&self, _companion_id: &str) {}
}

pub struct CompanionService {
    /// Canonical owner of host-control-plane companion conversations. Resolved
    /// once from the authoritative user repository and propagated explicitly to
    /// every conversation adapter; never inferred from a username or DB literal.
    authoritative_user_id: Arc<str>,
    /// Shared multi-companion home (`{data_dir}/companion/shared`): config + events + db.
    shared_dir: PathBuf,
    /// Cached ML assets (`{data_dir}/companion/models`): the MODNet matting model
    /// proxied + served locally (see [`crate::matting_model`]).
    models_dir: PathBuf,
    /// Serializes first-time matting-model downloads (one fetch for N callers).
    model_lock: Mutex<()>,
    /// Shared custom-figure library home (`{data_dir}/companion/figures`).
    figures_dir: PathBuf,
    /// Serializes figure-library index read-modify-write.
    figures_lock: Mutex<()>,
    config: SharedConfig,
    registry: Arc<CompanionRegistry>,
    store: CompanionStore,
    emitter: CompanionEventEmitter,
    learner: Arc<Learner>,
    /// Skill-evolution engine; held so on-demand drafting (learn-by-demonstration) can
    /// reach it, not just the background tick.
    evolution: Arc<EvolutionEngine>,
    /// Resolved skill paths (`{data_dir}/skills/...`), shared with the evolution
    /// engine and the agent-facing skill sink.
    skill_paths: Arc<SkillPaths>,
    /// Companion thread management (real nomi conversations). Unset when the
    /// host wires the companion without a conversation service (tests).
    companion: tokio::sync::OnceCell<CompanionThreads>,
    /// Session-window archiver; late-wired (with a real conversation port) in
    /// [`Self::attach_companion`], since it needs the conversation service.
    /// Held so a future "archive now" can reach it. Unset in tests.
    archiver: std::sync::OnceLock<Arc<Archiver>>,
    /// Delete-cascade hooks, late-wired by the app assembly (same pattern as
    /// `companion`). Empty when never set (tests).
    cleanup_hooks: std::sync::OnceLock<Vec<Arc<dyn CompanionCleanupHook>>>,
    provider_repo: Option<Arc<dyn IProviderRepository>>,
    provider_lifecycle: Option<SharedProviderLifecycleBarrier>,
}

impl CompanionService {
    /// Construct the service from the v3 companion layout, open the shared
    /// store, scan the companion roster, and spawn background tasks.
    pub async fn start(
        data_dir: &std::path::Path,
        bus: Arc<nomifun_realtime::BroadcastEventBus>,
        owner_id: &str,
        completer: Arc<dyn CompanionCompleter>,
        skill_paths: Arc<SkillPaths>,
    ) -> Result<Arc<Self>, AppError> {
        Self::start_with_provider_lifecycle(
            data_dir,
            bus,
            owner_id,
            completer,
            skill_paths,
            None,
            None,
        )
        .await
    }

    pub async fn start_with_provider_lifecycle(
        data_dir: &std::path::Path,
        bus: Arc<nomifun_realtime::BroadcastEventBus>,
        owner_id: &str,
        completer: Arc<dyn CompanionCompleter>,
        skill_paths: Arc<SkillPaths>,
        provider_repo: Option<Arc<dyn IProviderRepository>>,
        provider_lifecycle: Option<SharedProviderLifecycleBarrier>,
    ) -> Result<Arc<Self>, AppError> {
        let owner_id = owner_id.trim();
        if owner_id.is_empty() {
            return Err(AppError::Internal(
                "authoritative companion owner id must not be empty".into(),
            ));
        }
        let authoritative_user_id: Arc<str> = Arc::from(owner_id);
        let shared_dir = data_dir.join(crate::COMPANION_SHARED_REL_DIR);
        let companions_dir = data_dir.join(crate::COMPANION_COMPANIONS_REL_DIR);
        let models_dir = data_dir.join(crate::COMPANION_MODELS_REL_DIR);
        let figures_dir = data_dir.join(crate::COMPANION_FIGURES_REL_DIR);
        let config: SharedConfig = Arc::new(RwLock::new(SharedCompanionConfig::load(&shared_dir)?));
        let registry = Arc::new(CompanionRegistry::scan_with_provider_lifecycle(
            companions_dir,
            shared_dir.clone(),
            provider_repo.clone(),
            provider_lifecycle.clone(),
        )?);
        let live_companion_ids = registry
            .ids()
            .await
            .into_iter()
            .collect::<std::collections::HashSet<_>>();
        if let Some(default_companion_id) = config.read().await.default_companion_id.as_deref()
            && !live_companion_ids.contains(default_companion_id)
        {
            return Err(AppError::Internal(format!(
                "shared companion config references missing default companion '{}'",
                default_companion_id
            )));
        }
        // Existing side-store references are hard bindings too. Audit them
        // before any background worker can consume a model. Normal Provider
        // deletion takes the write side of the same barrier and therefore
        // cannot race this check.
        let _provider_guard = if let Some(barrier) = provider_lifecycle.as_ref() {
            Some(barrier.read().await)
        } else {
            None
        };
        validate_provider_reference(
            provider_repo.as_ref(),
            config.read().await.learn.model.as_ref(),
            "shared learn",
        )
        .await?;
        validate_provider_reference(
            provider_repo.as_ref(),
            config.read().await.evolve.model.as_ref(),
            "shared evolve",
        )
        .await?;
        registry.validate_provider_references_under_guard().await?;
        drop(_provider_guard);
        // The persistent v3 side-store is authoritative. Never hide a corrupt,
        // locked, or pre-v3 database behind a throwaway in-memory replacement:
        // doing so would make writes appear successful and then disappear.
        let store = CompanionStore::open(&shared_dir).await?;
        store.validate_companion_references(&live_companion_ids).await?;
        collector::validate_event_store(&shared_dir)?;
        crate::figures::validate_store(&figures_dir)?;
        let live_figure_ids = crate::figures::id_set(&figures_dir)?;
        registry.validate_figure_references(&live_figure_ids).await?;
        let skills = store.list_all_skills().await?;
        crate::skill_io::validate_store(&skill_paths, &skills).await?;
        let emitter = CompanionEventEmitter::new(bus.clone(), authoritative_user_id.to_string());

        Collector::new(shared_dir.clone(), config.clone(), store.clone()).spawn(bus);

        let learner = Arc::new(Learner {
            companion_dir: shared_dir.clone(),
            config: config.clone(),
            store: store.clone(),
            registry: registry.clone(),
            completer: completer.clone(),
            emitter: emitter.clone(),
            run_lock: Arc::new(Mutex::new(())),
        });
        learner.clone().spawn();

        // Skill self-evolution engine (design §5): independent background loop that
        // mines repeated tool sequences and drafts reviewable skills. Shares the
        // collector event stream + completer with the learner but runs its own tick.
        let evolution = Arc::new(EvolutionEngine {
            companion_dir: shared_dir.clone(),
            config: config.clone(),
            store: store.clone(),
            registry: registry.clone(),
            completer: completer.clone(),
            emitter: emitter.clone(),
            skill_paths: skill_paths.clone(),
            // Real conversation-store-backed source is late-wired in `attach_companion`
            // (the conversation service is built after this). Noop = drafts degrade to
            // tool-name steps until then.
            transcript: std::sync::RwLock::new(Arc::new(NoopTranscriptSource)),
            run_lock: Arc::new(Mutex::new(())),
        });
        evolution.clone().spawn();

        Ok(Arc::new(Self {
            authoritative_user_id,
            shared_dir,
            models_dir,
            model_lock: Mutex::new(()),
            figures_dir,
            figures_lock: Mutex::new(()),
            config,
            registry,
            store,
            emitter,
            learner,
            evolution,
            skill_paths,
            companion: tokio::sync::OnceCell::new(),
            archiver: std::sync::OnceLock::new(),
            cleanup_hooks: std::sync::OnceLock::new(),
            provider_repo,
            provider_lifecycle,
        }))
    }

    /// Late-wire the companion thread manager (depends on the conversation
    /// service, which is built after the companion service in app startup).
    pub fn attach_companion(
        &self,
        conversations: Arc<nomifun_conversation::ConversationService>,
        runtime_registry: Arc<dyn nomifun_ai_agent::AgentRuntimeRegistry>,
    ) {
        // Also wire the real transcript source so skill drafting rehydrates the actual
        // (redacted) session transcript from the conversation store — the durable single
        // source of truth — instead of degrading to tool-name steps.
        self.evolution.set_transcript(Arc::new(crate::evolution::ConversationTranscriptSource::new(
            conversations.conversation_repo().clone(),
        )));
        // Spawn the session-window archiver now that a real conversation port
        // exists. The loop no-ops every tick while `archive.enabled` is false
        // (opt-in), so an unconfigured install pays nothing. `OnceLock::set`
        // guards against a double-spawn if attach is ever called twice.
        if self.archiver.get().is_none() {
            let archiver = Arc::new(Archiver {
                store: self.store.clone(),
                config: self.config.clone(),
                registry: self.registry.clone(),
                // Reuse the learn completer + model — one background LLM config.
                completer: self.learner.completer.clone(),
                port: Arc::new(crate::archive_port::ConversationArchivePort::new(
                    self.authoritative_user_id.clone(),
                    conversations.clone(),
                )),
                run_lock: Arc::new(Mutex::new(())),
            });
            if self.archiver.set(archiver.clone()).is_ok() {
                archiver.spawn();
            }
        }
        let _ = self.companion.set(CompanionThreads {
            authoritative_user_id: self.authoritative_user_id.clone(),
            store: self.store.clone(),
            config: self.config.clone(),
            registry: self.registry.clone(),
            conversations,
            runtime_registry,
        });
    }

    /// Late-wire the delete-cascade hooks (depends on services built after
    /// the companion service in app startup, e.g. `KnowledgeService`). First call
    /// wins; later calls are ignored (`OnceLock` semantics).
    pub fn set_cleanup_hooks(&self, hooks: Vec<Arc<dyn CompanionCleanupHook>>) {
        let _ = self.cleanup_hooks.set(hooks);
    }

    /// Build the `CompanionMemorySink` the agent factory needs — gives every
    /// companion_session conversation the recall/save/recent-events tools.
    pub fn memory_sink(&self) -> Arc<dyn nomifun_ai_agent::CompanionMemorySink> {
        Arc::new(crate::companion::CompanionStoreSink {
            store: self.store.clone(),
            config: self.config.clone(),
            emitter: self.emitter.clone(),
            companion_dir: self.shared_dir.clone(),
        })
    }

    /// Build the `CompanionSkillSink` the agent factory needs — gives companion_session
    /// conversations the `companion_skill` tool + the per-turn when_to_use injection
    /// over this companion's self-evolved + shared skills (design §7).
    pub fn skill_sink(&self) -> Arc<dyn nomifun_ai_agent::CompanionSkillSink> {
        Arc::new(CompanionSkillStoreSink {
            store: self.store.clone(),
            config: self.config.clone(),
            skill_paths: self.skill_paths.clone(),
        })
    }

    fn companion(&self) -> Result<&CompanionThreads, AppError> {
        self.companion
            .get()
            .ok_or_else(|| AppError::Internal("companion threads not wired".into()))
    }

    // ----- companions -----

    /// All companion profiles, oldest first.
    pub async fn list_companions(&self) -> Vec<CompanionProfileConfig> {
        self.registry.list().await
    }

    /// Every desktop-companion reference to `provider_id`: per-companion chat
    /// model + the shared learn/evolve models. Malformed provider IDs never match.
    /// The provider deletion coordinator invokes this while holding the shared
    /// lifecycle write guard; all parent checks below therefore observe the
    /// same deletion-critical snapshot.
    pub async fn providers_in_use(&self, provider_id: &str) -> Vec<ProviderUsage> {
        let Ok(provider_id) = ProviderId::try_from(provider_id) else {
            return Vec::new();
        };
        if let Err(error) = self
            .registry
            .validate_provider_references_under_guard()
            .await
        {
            return vec![ProviderUsage {
                feature: ProviderUsageFeature::DesktopCompanion,
                label: format!("桌面伙伴 Provider 引用审计失败（{error}）"),
                target_id: None,
            }];
        }
        let mut out = Vec::new();
        for p in self.list_companions().await {
            if p.model.as_ref().is_some_and(|model| model.provider_id == provider_id.as_str()) {
                out.push(ProviderUsage {
                    feature: ProviderUsageFeature::DesktopCompanion,
                    label: p.name.clone(),
                    target_id: Some(p.companion_id.clone()),
                });
            }
        }
        let shared = self.get_config().await;
        if let Err(error) = self.validate_shared_provider_models(&shared).await {
            return vec![ProviderUsage {
                feature: ProviderUsageFeature::DesktopCompanion,
                label: format!("桌面伙伴共享 Provider 引用审计失败（{error}）"),
                target_id: None,
            }];
        }
        if shared
            .learn
            .model
            .as_ref()
            .is_some_and(|model| model.provider_id == provider_id.as_str())
        {
            out.push(ProviderUsage {
                feature: ProviderUsageFeature::DesktopCompanion,
                label: "共享学习模型".into(),
                target_id: None,
            });
        }
        if shared
            .evolve
            .model
            .as_ref()
            .is_some_and(|model| model.provider_id == provider_id.as_str())
        {
            out.push(ProviderUsage {
                feature: ProviderUsageFeature::DesktopCompanion,
                label: "共享进化模型".into(),
                target_id: None,
            });
        }
        out
    }

    /// Create a companion. The first companion ever created automatically becomes the
    /// default companion (shared config saved + broadcast).
    pub async fn create_companion(&self, name: &str, character: &str) -> Result<CompanionProfileConfig, AppError> {
        let profile = self.registry.create(name, character).await?;
        let updated_shared = {
            let mut cfg = self.config.write().await;
            if cfg.default_companion_id.is_none() {
                cfg.default_companion_id = Some(profile.companion_id.clone());
                if let Err(e) = cfg.save(&self.shared_dir) {
                    // The pointer survives in memory; warn rather than fail
                    // the creation (the companion itself is already persisted).
                    tracing::warn!(error = %e, "save shared companion config (default_companion_id) failed");
                }
                Some(cfg.clone())
            } else {
                None
            }
        };
        if let Some(cfg) = updated_shared {
            self.emitter.emit_shared_config_updated(&cfg);
        }
        self.emitter.emit_companion_created(&profile);
        // Auto-create the companion's single companion session, but only when its
        // model is already configured (a session can't be minted without one)
        // and the companion manager is wired (it isn't in tests). Best-effort:
        // a failure here must never fail companion creation — the session is lazily
        // ensured later when the UI calls POST .../companion/threads after a
        // model is set.
        if profile.model.is_some()
            && let Ok(companion) = self.companion()
            && let Err(e) = companion.create(&profile.companion_id, None).await
        {
            tracing::warn!(error = %e, companion_id = %profile.companion_id, "auto-create companion session failed; will be ensured lazily");
        }
        Ok(profile)
    }

    pub async fn get_companion(&self, id: &str) -> Result<CompanionProfileConfig, AppError> {
        self.registry
            .get(id)
            .await
            .ok_or_else(|| AppError::NotFound(format!("companion '{id}' not found")))
    }

    /// Apply a server-resolved preset without replacing companion identity or
    /// learned state. The frozen snapshot is persisted on the profile so new
    /// companion sessions and remote channel turns reuse the same capability
    /// template even if the source preset is edited later.
    pub async fn apply_preset_snapshot(
        &self,
        id: &str,
        mut snapshot: nomifun_api_types::ResolvedPresetSnapshot,
    ) -> Result<CompanionProfileConfig, AppError> {
        if snapshot.target != nomifun_api_types::PresetTarget::Companion {
            return Err(AppError::BadRequest(
                "preset snapshot target must be companion".into(),
            ));
        }
        let resolved_model = snapshot.resolved_model.take();
        let mut patch = serde_json::json!({ "applied_preset": snapshot });
        if let Some(model) = resolved_model {
            if let Some(provider_id) = model.provider_id {
                patch["model"] = serde_json::json!({
                    "provider_id": provider_id,
                    "model": model.model,
                });
            }
        }
        let profile = self.patch_companion(id, patch).await?;
        self.propagate_preset_to_companion(&profile).await;
        Ok(profile)
    }

    /// RFC 7396 partial update of one companion's profile. When the patch changes the
    /// model into a new configured value, the new model (唯一事实源 =
    /// profile.model) is propagated to the companion's single companion conversation
    /// row so the next turn uses it — the conversation row `model` was only a
    /// create-time snapshot. If the companion had no session yet but the model just
    /// became configured, the session is auto-ensured (idempotent). All of the
    /// companion-side work is best-effort: it never fails the patch.
    pub async fn patch_companion(&self, id: &str, patch: serde_json::Value) -> Result<CompanionProfileConfig, AppError> {
        // Snapshot the pre-patch model so we can tell whether this patch
        // actually changed it (RFC 7396 patches need not mention `model`).
        let prev = self.registry.get(id).await;
        let prev_model = prev.as_ref().and_then(|p| p.model.clone());
        let prev_name = prev.as_ref().map(|p| p.name.clone());
        let profile = self.registry.patch(id, patch).await?;
        self.emitter.emit_companion_updated(&profile.companion_id, &profile);

        let model_changed = prev_model.as_ref() != profile.model.as_ref();
        if model_changed {
            if profile.model.is_some() {
                self.propagate_model_to_companion(&profile).await;
            }
            // 通知宿主：模型已切换（唯一事实源）。当前用于清理该伙伴绑定的
            // IM 渠道会话，使其下轮重建拾取新模型（或正确地因未配置而拒绝）。
            // best-effort，不阻断 patch。
            if let Some(hooks) = self.cleanup_hooks.get() {
                for hook in hooks {
                    hook.on_companion_model_changed(&profile.companion_id).await;
                }
            }
        }
        // 改名跟随：名字变了就把已存在的伙伴会话工作区目录迁到新 pretty 名
        // （best-effort，不为改名新建会话；agent 运行中占用则保留旧名下次再迁）。
        if prev_name.as_deref() != Some(profile.name.as_str()) {
            self.reconcile_companion_workspace(&profile).await;
        }
        Ok(profile)
    }

    /// Best-effort：把伙伴「已存在」会话的工作区目录收敛到当前名字。无会话则跳过
    /// （下次 create() 自然用新名）；companion 未接线（测试）则跳过。绝不阻断 patch。
    async fn reconcile_companion_workspace(&self, profile: &CompanionProfileConfig) {
        let Ok(companion) = self.companion() else { return };
        let threads = match companion.list(&profile.companion_id).await {
            Ok(t) => t,
            Err(e) => {
                tracing::warn!(error = %e, companion_id = %profile.companion_id, "list threads for workspace reconcile failed");
                return;
            }
        };
        if let Some(thread) = threads.into_iter().next() {
            companion.reconcile_thread_workspace(profile, &thread.conversation_id).await;
        }
    }

    /// Best-effort: push the companion's configured model onto its single companion
    /// conversation row, auto-ensuring the session first if the model just
    /// became configured (so setting a model immediately gives the partner a
    /// usable session). Swallows every error (companion may be unwired in
    /// tests); a failure here must not fail the patch that triggered it.
    async fn propagate_model_to_companion(&self, profile: &CompanionProfileConfig) {
        let Some(model) = profile.model.as_ref() else { return };
        let Ok(companion) = self.companion() else { return };
        // Idempotent ensure: returns the existing session, or mints one now
        // that the model is configured. This also yields the conversation id
        // to retarget.
        let conversation_id = match companion.create(&profile.companion_id, None).await {
            Ok(thread) => thread.conversation_id,
            Err(e) => {
                tracing::warn!(error = %e, companion_id = %profile.companion_id, "ensure companion session for model propagation failed");
                return;
            }
        };
        if let Err(e) = companion.set_model(&profile.companion_id, &conversation_id, model).await {
            tracing::warn!(error = %e, companion_id = %profile.companion_id, "propagate model to companion conversation failed");
        }
    }

    /// Best-effort live propagation for an existing companion session. New
    /// sessions already consume `profile.applied_preset` in `create()`.
    async fn propagate_preset_to_companion(&self, profile: &CompanionProfileConfig) {
        let Some(snapshot) = profile.applied_preset.as_ref() else { return };
        let Ok(companion) = self.companion() else { return };
        let conversation_id = match companion.create(&profile.companion_id, None).await {
            Ok(thread) => thread.conversation_id,
            Err(error) => {
                tracing::warn!(%error, companion_id = %profile.companion_id, "ensure companion session for preset propagation failed");
                return;
            }
        };
        let system_prompt = build_companion_system_prompt(
            &self.store,
            profile,
            None,
            self.config.read().await.smart_collaboration,
        )
        .await;
        if let Err(error) = companion
            .set_preset(&profile.companion_id, &conversation_id, system_prompt, snapshot)
            .await
        {
            tracing::warn!(%error, companion_id = %profile.companion_id, "propagate preset to companion conversation failed");
        }
    }

    /// Delete a companion: cascade-delete its companion conversations, clear its
    /// per-companion store rows, remove the profile, and re-point the default companion
    /// if it was this one.
    pub async fn delete_companion(&self, id: &str) -> Result<(), AppError> {
        // Existence gate first so a bad id 404s before any side effect.
        self.get_companion(id).await?;
        // Cascade the companion's companion threads through the full delete path
        // (kills running agents + removes the real conversations). When the
        // companion manager isn't wired (tests), the thread rows still go
        // away below via delete_companion_rows. Any failure aborts the delete:
        // proceeding would drop the companion (and its thread registry rows) while
        // the conversations live on as orphans — the user can simply retry.
        let threads = self.store.list_companion_threads(Some(id)).await?;
        if let Ok(companion) = self.companion() {
            for t in &threads {
                companion.delete(id, &t.conversation_id).await.map_err(|e| {
                    AppError::Internal(format!(
                        "cascade-delete companion thread '{}' failed, companion kept: {e}",
                        t.conversation_id
                    ))
                })?;
            }
        }
        // T3.3 knowledge binding cleanup lives in the cleanup hooks below
        // (the app assembly registers a KnowledgeService-backed hook that
        // drops the ('companion', id) binding row).
        self.remove_companion_skill_trees(id)?;
        self.store.delete_companion_rows(id).await?;
        self.registry.remove(id).await?;
        // Post-removal cascade hooks. The companion is already gone, so a failing
        // hook must never fail the delete — implementations warn internally.
        if let Some(hooks) = self.cleanup_hooks.get() {
            for hook in hooks {
                hook.on_companion_deleted(id).await;
            }
        }
        // Default pointer: hand it to the oldest surviving companion (or clear it).
        let updated_shared = {
            let mut cfg = self.config.write().await;
            if cfg.default_companion_id.as_deref() == Some(id) {
                cfg.default_companion_id = self
                    .registry
                    .list()
                    .await
                    .first()
                    .map(|p| p.companion_id.clone());
                if let Err(e) = cfg.save(&self.shared_dir) {
                    tracing::warn!(error = %e, "save shared companion config (default_companion_id) failed");
                }
                Some(cfg.clone())
            } else {
                None
            }
        };
        if let Some(cfg) = updated_shared {
            self.emitter.emit_shared_config_updated(&cfg);
        }
        self.emitter.emit_companion_deleted(id);
        Ok(())
    }

    /// Companion-scoped skill bodies are durable children of the companion
    /// logical owner. Delete both active and draft trees before dropping the
    /// SQLite rows; if filesystem cleanup fails, the registry profile remains
    /// and the delete can be retried without hiding orphaned files.
    fn remove_companion_skill_trees(&self, companion_id: &str) -> Result<(), AppError> {
        let companion_id = CompanionId::try_from(companion_id)
            .map_err(|error| AppError::BadRequest(format!("invalid companion id: {error}")))?;
        for path in [
            skill_service::companion_skills_root(&self.skill_paths)
                .join(companion_id.as_str()),
            skill_service::drafts_root(&self.skill_paths).join(companion_id.as_str()),
        ] {
            crate::fsio::remove_path_entry(&path).map_err(|error| {
                AppError::Internal(format!(
                    "remove companion skill tree {}: {error}",
                    path.display()
                ))
            })?;
        }
        Ok(())
    }

    /// One companion's status: per-companion xp/level, shared mood + memory/suggestion
    /// counters, that companion's companion model flag.
    pub async fn companion_status(&self, id: &str) -> Result<CompanionStatus, AppError> {
        let profile = self.get_companion(id).await?;
        let cfg = self.config.read().await.clone();
        let xp = self.store.get_companion_state_i64(id, "xp").await?;
        Ok(CompanionStatus {
            companion_id: Some(profile.companion_id),
            xp,
            level: level_for_xp(xp),
            mood: self.store.get_state("mood").await?.unwrap_or_else(|| "content".into()),
            memories_active: self.store.count_memories("active").await?,
            memories_archived: self.store.count_memories("archived").await?,
            suggestions_new: self.store.count_suggestions("new").await?,
            skills_active: self.store.count_active_skills(id).await?,
            model_configured: profile.model.is_some(),
            collect_any_enabled: cfg.collect.any_enabled(),
            last_learn: self.store.list_learn_runs(1).await?.into_iter().next(),
        })
    }

    /// Aggregate "what I learned this week" for the Overview digest card.
    pub async fn weekly_digest(&self, companion_id: &str, since_ms: i64) -> Result<CompanionWeeklyDigest, AppError> {
        let skills_learned = self.store.count_skills_since(companion_id, since_ms, None).await?;
        let skills_active_new = self.store.count_skills_since(companion_id, since_ms, Some("active")).await?;
        let memories_added = self.store.count_memories_since(since_ms).await?;
        let new_skill_names = self.store.list_skill_names_since(companion_id, since_ms, 12).await?;
        let runs = self.store.list_learn_runs(50).await?;
        let learn_runs = runs.iter().filter(|r| r.started_at >= since_ms).count() as i64;
        let recent_summaries: Vec<String> =
            runs.iter().filter(|r| r.started_at >= since_ms).filter_map(|r| r.summary.clone()).take(5).collect();
        Ok(CompanionWeeklyDigest {
            since_ms,
            skills_learned,
            skills_active_new,
            memories_added,
            learn_runs,
            new_skill_names,
            recent_summaries,
        })
    }

    /// The effective default companion: the shared pointer, or the oldest
    /// companion when no pointer has been configured. Startup rejects a stored
    /// dangling pointer, so a configured value is always live here.
    pub async fn default_companion_id(&self) -> Option<String> {
        let configured = self.config.read().await.default_companion_id.clone();
        if let Some(configured) = configured {
            return Some(configured);
        }
        self.registry.list().await.first().map(|p| p.companion_id.clone())
    }

    // ----- DIY custom figure (spec §3 存储与回显) -----

    /// Ingest an uploaded figure image for one companion (two-phase upload: the
    /// file already sits under the temp upload root). Unknown companion → 404
    /// before any filesystem work; validation failures map to 400/403.
    pub async fn ingest_figure(&self, companion_id: &str, source_path: &str) -> Result<(), AppError> {
        self.get_companion(companion_id).await?;
        crate::figure::ingest_figure(self.registry.companions_dir(), companion_id, std::path::Path::new(source_path))
    }

    /// One companion's stored figure bytes + mtime (unix seconds, the ETag input).
    /// Unknown companion or missing figure file → 404.
    pub async fn read_figure(&self, companion_id: &str) -> Result<(Vec<u8>, u64), AppError> {
        self.get_companion(companion_id).await?;
        crate::figure::read_figure(self.registry.companions_dir(), companion_id)
            ?
            .ok_or_else(|| AppError::NotFound(format!("companion '{companion_id}' has no custom figure")))
    }

    // ----- matting model proxy (fixes the DIY 30s-timeout dead-end) -----

    /// Ensure the MODNet matting model is cached on disk and return its bytes.
    /// Downloads from a mirror on first use (uncapped, so a slow 25 MB transfer
    /// completes instead of being killed by the old in-worker 30 s timeout).
    pub async fn matting_model_bytes(&self) -> Result<Vec<u8>, AppError> {
        let path = crate::matting_model::ensure_model(&self.models_dir, &self.model_lock).await?;
        tokio::fs::read(&path)
            .await
            .map_err(|e| AppError::Internal(format!("read matting model: {e}")))
    }

    // ----- custom-figure library (decoupled from companions) -----

    /// All saved library figures, newest first.
    pub async fn list_figures(&self) -> Result<Vec<crate::figures::FigureMeta>, AppError> {
        crate::figures::list(&self.figures_dir)
    }

    /// Ingest an uploaded cutout as a new reusable library figure.
    pub async fn create_figure(
        &self,
        source_path: &str,
        name: &str,
        aspect: f32,
        head_box: crate::profile::HeadBox,
        size_tier: &str,
    ) -> Result<crate::figures::FigureMeta, AppError> {
        let _guard = self.figures_lock.lock().await;
        crate::figures::create(
            &self.figures_dir,
            std::path::Path::new(source_path),
            name,
            aspect,
            head_box,
            size_tier,
        )
    }

    /// One library figure's image bytes + mtime (unix seconds). Unknown id → 404.
    pub async fn read_figure_image(&self, figure_id: &str) -> Result<(Vec<u8>, u64), AppError> {
        crate::figures::read_image(&self.figures_dir, figure_id)
            ?
            .ok_or_else(|| AppError::NotFound(format!("figure '{figure_id}' not found")))
    }

    /// Rename a library figure. Unknown id → 404.
    pub async fn rename_figure(&self, figure_id: &str, name: &str) -> Result<crate::figures::FigureMeta, AppError> {
        self.update_figure(figure_id, crate::figures::FigureUpdate { name: Some(name.to_owned()), head_box: None, size_tier: None }).await
    }

    /// Update editable library-figure metadata. Framing/size changes are synced
    /// into active custom companions that reference the library figure.
    pub async fn update_figure(
        &self,
        figure_id: &str,
        update: crate::figures::FigureUpdate,
    ) -> Result<crate::figures::FigureMeta, AppError> {
        let sync_users = update.head_box.is_some() || update.size_tier.is_some();
        let updated = {
            let _guard = self.figures_lock.lock().await;
            crate::figures::update(&self.figures_dir, figure_id, update)?
        };
        if sync_users {
            self.sync_figure_to_active_companions(&updated).await;
        }
        Ok(updated)
    }

    async fn sync_figure_to_active_companions(&self, figure: &crate::figures::FigureMeta) {
        let users: Vec<_> = self
            .registry
            .list()
            .await
            .into_iter()
            .filter(|p| {
                p.character == "custom"
                    && p.appearance
                        .custom_figure
                        .as_ref()
                        .and_then(|cf| cf.figure_id.as_deref())
                        == Some(figure.figure_id.as_str())
            })
            .collect();
        for profile in users {
            let patch = serde_json::json!({
                "appearance": {"custom_figure": {
                    "aspect": figure.aspect,
                    "head_box": {"x": figure.head_box.x, "y": figure.head_box.y, "w": figure.head_box.w, "h": figure.head_box.h},
                    "size_tier": figure.size_tier.clone(),
                    "figure_id": figure.figure_id.clone(),
                }},
            });
            if let Err(e) = self.patch_companion(&profile.companion_id, patch).await {
                tracing::warn!(error = %e, companion_id = %profile.companion_id, figure_id = %figure.figure_id, "sync updated library figure metadata to companion failed");
            }
        }
    }

    /// Number of hard profile references to this library figure. A stored
    /// `figure_id` remains a logical binding even while a built-in character is
    /// selected; callers must explicitly clear or replace it before deletion.
    async fn figure_user_count(&self, figure_id: &str) -> usize {
        self.registry
            .list()
            .await
            .iter()
            .filter(|p| {
                p.appearance
                    .custom_figure
                    .as_ref()
                    .and_then(|cf| cf.figure_id.as_deref())
                    == Some(figure_id)
            })
            .count()
    }

    /// Delete a library figure (image + index entry). Unknown id → 404. A figure
    /// still referenced by a companion is refused (`Conflict`): deleting it would leave
    /// that companion's `custom_figure.figure_id` dangling and its window image 404ing.
    /// Only unused figures may be deleted.
    pub async fn delete_figure(&self, figure_id: &str) -> Result<(), AppError> {
        let _guard = self.figures_lock.lock().await;
        let users = self.figure_user_count(figure_id).await;
        if users > 0 {
            return Err(AppError::Conflict(format!(
                "figure '{figure_id}' is used by {users} companion(s) and cannot be deleted"
            )));
        }
        crate::figures::remove(&self.figures_dir, figure_id)
    }

    // ----- companion session (per companion, single session) -----

    /// Idempotent ensure of the companion's single companion session.
    pub async fn create_companion_thread(
        &self,
        companion_id: &str,
        title: Option<String>,
    ) -> Result<CompanionThread, AppError> {
        self.companion()?.create(companion_id, title).await
    }

    pub async fn companion_active_thread(&self, companion_id: &str) -> Result<Option<String>, AppError> {
        self.companion()?.active_thread_id(companion_id).await
    }

    // ----- shared config -----

    pub async fn get_config(&self) -> SharedCompanionConfig {
        self.config.read().await.clone()
    }

    pub async fn update_config(&self, new_config: SharedCompanionConfig) -> Result<SharedCompanionConfig, AppError> {
        let _provider_guard = if let Some(barrier) = self.provider_lifecycle.as_ref() {
            Some(barrier.read().await)
        } else {
            None
        };
        self.validate_shared_provider_models(&new_config).await?;
        self.validate_default_companion_reference(&new_config).await?;
        {
            // Hold the write lock across the disk save so two full-config
            // writers can't interleave save/update and diverge disk vs memory.
            let mut cfg = self.config.write().await;
            new_config
                .save(&self.shared_dir)
                .map_err(|e| AppError::Internal(format!("save shared companion config: {e}")))?;
            *cfg = new_config.clone();
        }
        self.emitter.emit_shared_config_updated(&new_config);
        Ok(new_config)
    }

    /// RFC 7396-style partial update: merge `patch` over the current shared
    /// config under the write lock. Lets concurrent writers (settings
    /// toggles, default-companion switches) update disjoint fields without
    /// clobbering each other the way full-object PUTs do.
    pub async fn patch_config(&self, patch: serde_json::Value) -> Result<SharedCompanionConfig, AppError> {
        if !patch.is_object() {
            return Err(AppError::BadRequest("config patch must be a JSON object".into()));
        }
        // Provider lifecycle barrier precedes the shared-config lock. Provider
        // deletion holds the write side, then scans this config.
        let _provider_guard = if let Some(barrier) = self.provider_lifecycle.as_ref() {
            Some(barrier.read().await)
        } else {
            None
        };
        let merged = {
            let mut cfg = self.config.write().await;
            let mut value = serde_json::to_value(&*cfg)
                .map_err(|e| AppError::Internal(format!("serialize shared companion config: {e}")))?;
            json_merge_patch(&mut value, &patch);
            let merged: SharedCompanionConfig =
                serde_json::from_value(value).map_err(|e| AppError::BadRequest(format!("invalid config patch: {e}")))?;
            self.validate_shared_provider_models(&merged).await?;
            self.validate_default_companion_reference(&merged).await?;
            merged
                .save(&self.shared_dir)
                .map_err(|e| AppError::Internal(format!("save shared companion config: {e}")))?;
            *cfg = merged.clone();
            merged
        };
        self.emitter.emit_shared_config_updated(&merged);
        Ok(merged)
    }

    async fn validate_shared_provider_models(
        &self,
        config: &SharedCompanionConfig,
    ) -> Result<(), AppError> {
        self.validate_provider_model(config.learn.model.as_ref(), "shared learn")
            .await?;
        self.validate_provider_model(config.evolve.model.as_ref(), "shared evolve")
            .await
    }

    async fn validate_default_companion_reference(
        &self,
        config: &SharedCompanionConfig,
    ) -> Result<(), AppError> {
        let Some(default_companion_id) = config.default_companion_id.as_deref() else {
            return Ok(());
        };
        if self.registry.get(default_companion_id).await.is_none() {
            return Err(AppError::NotFound(format!(
                "default companion '{}' not found",
                default_companion_id
            )));
        }
        Ok(())
    }

    async fn validate_provider_model(
        &self,
        model: Option<&ProviderWithModel>,
        label: &str,
    ) -> Result<(), AppError> {
        validate_provider_reference(self.provider_repo.as_ref(), model, label).await
    }

    /// First-launch consent: apply self-evolution default-ON exactly once (design §9, 默认开).
    /// Turns work-source collection + learn + evolve ON via `patch_config` (atomic save + emit +
    /// live Arc propagation), guarded by a one-time global KV flag so it NEVER re-applies and
    /// never re-enables after the user later turns things off. Raw `Default` impls stay `false`
    /// (existing users are never silently enabled by a serde back-fill).
    pub async fn apply_default_on_consent(&self) -> Result<SharedCompanionConfig, AppError> {
        const CONSENT_KEY: &str = "self_evolution_consent";
        if self.store.get_state(CONSENT_KEY).await?.is_some() {
            return Ok(self.config.read().await.clone()); // idempotent: already consented
        }
        // Default-on set applied once on first consent. Deliberately EXCLUDES the
        // model/agent OUTPUT side: `chat_assistant_replies` (long full replies) and
        // `cron_runs` (untruncated agent output) stay opt-in — the user flips them on in
        // the Collect tab if wanted. Skill mining keys off `tool_calls` and memory
        // distillation off the user-request side, so neither core loop needs the output side.
        let patch = serde_json::json!({
            "collect": {
                "tool_calls": true,
                "chat_user_messages": true,
                "requirements": true,
                "conversation_lifecycle": true
            },
            "learn": { "enabled": true },
            "evolve": { "enabled": true }
        });
        let cfg = self.patch_config(patch).await?;
        self.store.set_state(CONSENT_KEY, "1").await?;
        Ok(cfg)
    }

    /// Master kill switch (design §9, 一键全关): stop ALL collection (incl. `companion_dialogues`,
    /// which `any_enabled()` deliberately excludes), learning, and evolution in one atomic write.
    /// Leaves models/intervals intact so re-enable needs no reconfiguration, and does NOT clear the
    /// consent flag (a user who explicitly disabled is never silently re-enabled). Purging already-
    /// collected events is a separate `clear_events` call.
    pub async fn disable_all(&self) -> Result<SharedCompanionConfig, AppError> {
        let patch = serde_json::json!({
            "collect": {
                "chat_user_messages": false,
                "chat_assistant_replies": false,
                "requirements": false,
                "cron_runs": false,
                "conversation_lifecycle": false,
                "terminal_sessions": false,
                "tool_calls": false,
                "companion_dialogues": false
            },
            "learn": { "enabled": false },
            "evolve": { "enabled": false }
        });
        self.patch_config(patch).await
    }

    /// Newest `limit` collected events (cross-companion), for the transparency viewer. Events are
    /// already sanitized at collection time ({ts,source,name,data}; tool_calls = name + param shape,
    /// never values). Reuses `read_recent_events` (bounded window, never loads full history).
    pub fn recent_events(
        &self,
        limit: usize,
    ) -> Result<Vec<collector::CollectedEvent>, AppError> {
        collector::read_recent_events(&self.shared_dir, limit)
    }

    // ----- status -----

    /// Aggregate status: the default companion's status. With no companions at
    /// all, return a zeroed shared-only snapshot (xp 0 / level 1 / no model).
    pub async fn status(&self) -> Result<CompanionStatus, AppError> {
        if let Some(companion_id) = self.default_companion_id().await {
            return self.companion_status(&companion_id).await;
        }
        let cfg = self.config.read().await.clone();
        Ok(CompanionStatus {
            companion_id: None,
            xp: 0,
            level: level_for_xp(0),
            mood: self.store.get_state("mood").await?.unwrap_or_else(|| "content".into()),
            memories_active: self.store.count_memories("active").await?,
            memories_archived: self.store.count_memories("archived").await?,
            suggestions_new: self.store.count_suggestions("new").await?,
            skills_active: 0,
            model_configured: false,
            collect_any_enabled: cfg.collect.any_enabled(),
            last_learn: self.store.list_learn_runs(1).await?.into_iter().next(),
        })
    }

    // ----- memories -----

    pub async fn list_memories(&self, filter: &MemoryFilter) -> Result<Vec<CompanionMemory>, AppError> {
        self.store.list_memories(filter).await
    }

    pub async fn list_memory_page(&self, filter: &MemoryFilter) -> Result<MemoryPage, AppError> {
        self.store.list_memory_page(filter).await
    }

    // ----- session-window day digests (伙伴会话归档回看) -----

    /// Archived day-digests for one companion. `since`/`until` are inclusive
    /// `YYYYMMDD` bounds (empty = open). When both are empty, returns the most
    /// recent `limit` digests (newest first); otherwise the range (ascending).
    pub async fn list_day_digests(
        &self,
        companion_id: &str,
        since: &str,
        until: &str,
        limit: i64,
    ) -> Result<Vec<crate::store::SessionWindow>, AppError> {
        if since.is_empty() && until.is_empty() {
            self.store.list_digests(companion_id, limit).await
        } else {
            self.store.digests_in_range(companion_id, since, until).await
        }
    }

    /// "去年今日" — archived digests whose day-of-year (`MMDD`) matches, excluding
    /// today. `mmdd` is a 4-char `MMDD`.
    pub async fn digests_on_this_day(
        &self,
        companion_id: &str,
        mmdd: &str,
        exclude_day: &str,
        limit: i64,
    ) -> Result<Vec<crate::store::SessionWindow>, AppError> {
        self.store.digests_on_day_of_year(companion_id, mmdd, exclude_day, limit).await
    }
    pub async fn add_memory(&self, kind: &str, content: &str, tags: &[String], scope: MemoryScope) -> Result<CompanionMemory, AppError> {
        if !crate::store::MEMORY_KINDS.contains(&kind) {
            return Err(AppError::BadRequest(format!("invalid memory kind '{kind}'")));
        }
        let content = content.trim();
        if content.is_empty() {
            return Err(AppError::BadRequest("memory content is empty".into()));
        }
        // Dedup-merge (parity with the companion sink's save_memory): a
        // similar active memory is reinforced and returned instead of
        // inserting a near-duplicate. This guards the gateway
        // `nomi_memory_save` path, which previously inserted blindly.
        // Only for shared adds — a private add must not be silently folded into
        // an existing (possibly shared, or another companion's) memory.
        if scope == MemoryScope::Shared {
            if let Some(id) = self.store.find_similar_active(kind, content).await? {
                self.store.reinforce_memories(std::slice::from_ref(&id)).await?;
                if let Some(existing) = self.store.get_memory(&id).await? {
                    return Ok(existing);
                }
            }
        }
        let mem = self.store.insert_memory_scoped(kind, content, tags, 0.8, "manual", scope).await?;
        self.emitter.emit_memory_created(&mem);
        Ok(mem)
    }

    pub async fn update_memory(
        &self,
        memory_id: &str,
        content: Option<&str>,
        pinned: Option<bool>,
        status: Option<&str>,
        scope: Option<MemoryScope>,
    ) -> Result<(), AppError> {
        if let Some(status) = status {
            if status != "active" && status != "archived" {
                return Err(AppError::BadRequest(format!("invalid memory status '{status}'")));
            }
        }
        self.store
            .update_memory(memory_id, content, pinned, status, scope)
            .await?;
        // Notify open surfaces with the post-edit row (best-effort; a missing
        // row already errored above).
        if let Ok(Some(updated)) = self.store.get_memory(memory_id).await {
            self.emitter.emit_memory_updated(&updated);
        }
        Ok(())
    }

    pub async fn delete_memory(&self, memory_id: &str) -> Result<(), AppError> {
        self.store.delete_memory(memory_id).await?;
        self.emitter.emit_memory_deleted(memory_id);
        Ok(())
    }

    // ----- suggestions -----

    pub async fn list_suggestions(&self, status: Option<&str>, limit: i64) -> Result<Vec<CompanionSuggestion>, AppError> {
        self.store.list_suggestions(status, limit).await
    }

    pub async fn list_suggestion_page(
        &self,
        status: Option<&str>,
        limit: i64,
        offset: i64,
    ) -> Result<SuggestionPage, AppError> {
        self.store.list_suggestion_page(status, limit, offset).await
    }

    pub async fn decide_suggestion(
        &self,
        suggestion_id: &str,
        accept: bool,
    ) -> Result<CompanionSuggestion, AppError> {
        let (decided, newly) = self
            .store
            .decide_suggestion(suggestion_id, accept)
            .await?;
        // Gate side effects on `newly`: deciding is idempotent, so a stale
        // card / double-click / cross-surface repeat returns Ok without
        // re-awarding xp or re-broadcasting.
        if accept && newly {
            // Shared achievement: every companion grows when the owner accepts a
            // suggestion (spec ruling 2).
            let _ = self.store.add_xp_all(&self.registry.ids().await, 20).await;
            // create_skill suggestions materialize on accept: promote the reviewed
            // draft SKILL.md to the active dir + flip the registry row to active
            // (design §6). Inside the `newly` gate → re-accept never re-materializes.
            // A materialize failure must not fail the decide (idempotency / UX): log it.
            if decided.kind == "create_skill" {
                if let Some(action) = &decided.action {
                    if let Err(e) = self.materialize_create_skill(action).await {
                        tracing::warn!(error = %e, suggestion_id, "failed to materialize accepted skill");
                    }
                }
            }
        }
        // Rejecting a create_skill suggestion records correction feedback so the
        // originating mined pattern is suppressed from re-proposal (纠偏回流), and
        // archives the draft row. Inside `newly` → idempotent.
        if !accept && newly && decided.kind == "create_skill" {
            if let Some(action) = &decided.action {
                if let Err(e) = self.reject_create_skill(action).await {
                    tracing::warn!(error = %e, suggestion_id, "failed to record skill rejection");
                }
            }
        }
        if newly {
            // Let every open surface (panel, desktop bubble, console) drop the
            // now-decided card live instead of 404ing on a stale snapshot.
            self.emitter.emit_suggestion_decided(&decided);
        }
        Ok(decided)
    }

    /// Promote a reviewed skill draft to active on suggestion-accept (design §6).
    /// Reads the draft SKILL.md and rewrites it into the companion's active dir,
    /// then flips the registry row to `active` and emits `skill-learned`.
    /// Caller gates this inside the `newly` branch so it never runs twice.
    async fn materialize_create_skill(&self, action: &serde_json::Value) -> Result<(), AppError> {
        let Some(companion_skill_id) = action
            .get("companion_skill_id")
            .and_then(|value| value.as_str())
            .filter(|value| nomifun_common::validate_uuidv7(value).is_ok())
        else {
            return Ok(());
        };
        let Some(companion_id) = action
            .get("companion_id")
            .and_then(|v| v.as_str())
            .and_then(|id| CompanionId::try_from(id).ok())
        else {
            return Ok(());
        };
        // Delegate to the single idempotent skill-decide path (also used by the
        // Skills-tab review UI). draft→active promote + emit happen there.
        self.decide_companion_skill(
            companion_id.as_str(),
            companion_skill_id,
            true,
            None,
        )
        .await
        .map(|_| ())
    }

    /// Rejecting a create_skill suggestion → delegate to the single idempotent skill-decide
    /// path (accept=false), which archives the draft + records signature feedback (纠偏回流).
    async fn reject_create_skill(&self, action: &serde_json::Value) -> Result<(), AppError> {
        let Some(companion_skill_id) = action
            .get("companion_skill_id")
            .and_then(|value| value.as_str())
            .filter(|value| nomifun_common::validate_uuidv7(value).is_ok())
        else {
            return Ok(());
        };
        let Some(companion_id) = action
            .get("companion_id")
            .and_then(|v| v.as_str())
            .and_then(|id| CompanionId::try_from(id).ok())
        else {
            return Ok(());
        };
        self.decide_companion_skill(
            companion_id.as_str(),
            companion_skill_id,
            false,
            None,
        )
        .await
        .map(|_| ())
    }

    /// List a companion's skills for the UI (active/draft/archived). Each row gets its
    /// SKILL.md `description` read from disk. A missing or malformed durable
    /// body is a side-store integrity failure, not an empty description.
    pub async fn list_companion_skills(
        &self,
        companion_id: &str,
        include_shared: bool,
    ) -> Result<Vec<CompanionSkillView>, AppError> {
        let skills = self.store.list_skills(companion_id, include_shared).await?;
        Ok(self.skill_views(skills).await?)
    }

    /// List one page of companion skills for the UI. Only skills on the selected page
    /// have their SKILL.md frontmatter read from disk.
    pub async fn list_companion_skill_page(
        &self,
        companion_id: &str,
        include_shared: bool,
        status: Option<&str>,
        limit: i64,
        offset: i64,
    ) -> Result<CompanionSkillViewPage, AppError> {
        let page = self
            .store
            .list_skill_page(companion_id, include_shared, status, limit, offset)
            .await?;
        Ok(CompanionSkillViewPage {
            items: self.skill_views(page.items).await?,
            total: page.total,
        })
    }

    async fn skill_views(
        &self,
        skills: Vec<CompanionSkill>,
    ) -> Result<Vec<CompanionSkillView>, AppError> {
        let mut out = Vec::with_capacity(skills.len());
        for skill in skills {
            let scope = scope_for(skill.scope_companion_id.as_deref());
            let draft = skill.status == "draft";
            let dir = skill_service::skill_dir_for(
                &self.skill_paths,
                &scope,
                &skill.skill_name,
                draft,
            )
            .map_err(|error| {
                AppError::Internal(format!(
                    "resolve durable skill '{}' path: {error}",
                    skill.skill_name
                ))
            })?;
            let (_, description) = skill_service::read_skill_info(&dir)
                .await
                .map_err(|error| {
                    AppError::Internal(format!(
                        "read durable skill '{}' from {}: {error}",
                        skill.skill_name,
                        dir.display()
                    ))
                })?;
            out.push(CompanionSkillView { skill, description });
        }
        Ok(out)
    }

    /// Read one skill's registry row + raw SKILL.md body for the in-app editor.
    pub async fn get_companion_skill_content(
        &self,
        companion_id: &str,
        companion_skill_id: &str,
    ) -> Result<CompanionSkillContent, AppError> {
        let skill = self
            .store
            .get_owned_skill(companion_id, companion_skill_id)
            .await?
            .ok_or_else(|| {
                AppError::NotFound(format!(
                    "companion skill {companion_skill_id} not found"
                ))
            })?;
        let scope = scope_for(skill.scope_companion_id.as_deref());
        let draft = skill.status == "draft";
        let dir = skill_service::skill_dir_for(
            &self.skill_paths,
            &scope,
            &skill.skill_name,
            draft,
        )
            .map_err(|e| AppError::Internal(format!("resolve skill dir: {e}")))?;
        let content = tokio::fs::read_to_string(dir.join(SKILL_MANIFEST_FILE))
            .await
            .map_err(|e| AppError::Internal(format!("read durable skill content: {e}")))?;
        Ok(CompanionSkillContent { skill, content })
    }

    /// Edit a skill's SKILL.md body in place. `content` must be a full valid SKILL.md
    /// (frontmatter + non-empty description) — `write_skill` rejects otherwise → BadRequest.
    pub async fn write_companion_skill_content(
        &self,
        companion_id: &str,
        companion_skill_id: &str,
        content: &str,
    ) -> Result<(), AppError> {
        let skill = self
            .store
            .get_owned_skill(companion_id, companion_skill_id)
            .await?
            .ok_or_else(|| {
                AppError::NotFound(format!(
                    "companion skill {companion_skill_id} not found"
                ))
            })?;
        let scope = scope_for(skill.scope_companion_id.as_deref());
        let draft = skill.status == "draft";
        crate::skill_io::write_skill(
            &self.skill_paths,
            &scope,
            draft,
            &skill.skill_name,
            content,
        )
        .await?;
        Ok(())
    }

    /// Review a draft skill. accept → promote draft SKILL.md to active + status active +
    /// emit skill-learned; reject → status archived + record reject feedback. IDEMPOTENT:
    /// a re-decide on a non-draft row is a no-op returning the row (the `newly`-gate analogue).
    pub async fn decide_companion_skill(
        &self,
        companion_id: &str,
        companion_skill_id: &str,
        accept: bool,
        reason: Option<&str>,
    ) -> Result<CompanionSkill, AppError> {
        let skill = self
            .store
            .get_owned_skill(companion_id, companion_skill_id)
            .await?
            .ok_or_else(|| {
                AppError::NotFound(format!(
                    "companion skill {companion_skill_id} not found"
                ))
            })?;
        if skill.status != "draft" {
            return Ok(skill); // idempotent: already decided
        }
        let scope = SkillScope::Companion(companion_id.to_owned());
        let (draft_dir, active_dir) =
            crate::skill_io::promote_draft(&self.skill_paths, &scope, &skill.skill_name).await?;
        if let Err(error) = self
            .store
            .set_skill_status(
                companion_skill_id,
                if accept { "active" } else { "archived" },
            )
            .await
        {
            let rollback = crate::skill_io::rollback_promotion(&draft_dir, &active_dir);
            return Err(match rollback {
                Ok(()) => error,
                Err(rollback_error) => AppError::Internal(format!(
                    "{error}; additionally failed to roll back skill promotion: {rollback_error}"
                )),
            });
        }
        if accept {
            self.emitter.emit_skill_learned(
                companion_id,
                companion_skill_id,
                &skill.skill_name,
            );
        } else {
            let feedback_id =
                nomifun_common::CompanionEvolutionFeedbackId::new().into_string();
            let signature_snapshot = (!skill.signature.is_empty())
                .then_some(skill.signature.as_str());
            self.store
                .record_feedback(
                    &feedback_id,
                    companion_skill_id,
                    &skill.skill_name,
                    skill.skill_pattern_id.as_deref(),
                    signature_snapshot,
                    "reject",
                    reason,
                    nomifun_common::now_ms(),
                )
                .await?;
            // Suppress the originating mined pattern from re-proposal (纠偏回流).
            if let Some(skill_pattern_id) = skill.skill_pattern_id.as_deref() {
                self.store
                    .mark_pattern_status(skill_pattern_id, "rejected")
                    .await?;
            }
        }
        Ok(self
            .store
            .get_owned_skill(companion_id, companion_skill_id)
            .await?
            .unwrap_or(skill))
    }

    /// Learn-by-demonstration (P5 T2-B): reconstruct a tool-name sequence from `conversation_id`'s
    /// collected tool-calls and draft a reviewable skill from it. Requires `collect.tool_calls` to
    /// have been on for that session. Returns the drafted skill name.
    pub async fn draft_skill_from_session(&self, companion_id: &str, conversation_id: &str) -> Result<Option<String>, AppError> {
        let events = crate::collector::read_recent_events(&self.shared_dir, 1000)?;
        let mut steps: Vec<String> = Vec::new();
        let mut call_ids: Vec<String> = Vec::new();
        let mut start_ts = i64::MAX;
        let mut end_ts = 0i64;
        for ev in events.iter() {
            if ev.source != "tool_calls" {
                continue;
            }
            let conv = ev.data.get("conversation_id");
            let matches = conv
                .and_then(|value| value.as_str())
                .is_some_and(|value| value == conversation_id);
            if !matches {
                continue;
            }
            if let Some(name) = ev.data.get("name").and_then(|n| n.as_str()) {
                if steps.last().map(|s| s != name).unwrap_or(true) {
                    steps.push(name.to_owned());
                }
            }
            if let Some(cid) = ev.data.get("call_id").and_then(|c| c.as_str()).filter(|c| !c.is_empty()) {
                call_ids.push(cid.to_owned());
            }
            start_ts = start_ts.min(ev.ts);
            end_ts = end_ts.max(ev.ts);
        }
        if steps.len() < 2 {
            return Err(AppError::BadRequest("这个会话还没有足够的工具操作可以学习成技能".into()));
        }
        // Whole-session anchor: rehydrate this conversation's real transcript for richer drafting.
        let anchor = crate::evolution::TranscriptAnchor {
            conversation_id: conversation_id.to_owned(),
            start_ts: if start_ts == i64::MAX { 0 } else { start_ts },
            end_ts,
            pad_turns: 0,
            call_ids,
        };
        self.evolution.draft_from_episode(steps, anchor, companion_id).await
    }

    /// Gift a skill from one companion to another (互教): copy the SKILL.md into the recipient's
    /// scope + insert a `source="gifted"` row. Rejects self-gift and recipient name collisions
    /// (the insert UPSERT would otherwise silently overwrite the recipient's same-named skill).
    pub async fn gift_companion_skill(
        &self,
        from_companion_id: &str,
        companion_skill_id: &str,
        to_companion_id: &str,
    ) -> Result<CompanionSkill, AppError> {
        if from_companion_id == to_companion_id {
            return Err(AppError::BadRequest("不能赠送给自己".into()));
        }
        let src = self
            .store
            .get_owned_skill(from_companion_id, companion_skill_id)
            .await?
            .ok_or_else(|| {
                AppError::NotFound(format!(
                    "companion skill {companion_skill_id} not found"
                ))
            })?;
        if self
            .store
            .find_owned_skill_by_name(to_companion_id, &src.skill_name)
            .await?
            .is_some()
        {
            return Err(AppError::BadRequest(format!(
                "对方已有同名技能「{}」",
                src.skill_name
            )));
        }
        crate::skill_io::copy_skill(
            &self.skill_paths,
            &SkillScope::Companion(from_companion_id.to_owned()),
            &SkillScope::Companion(to_companion_id.to_owned()),
            &src.skill_name,
        )
        .await?;
        let now = nomifun_common::now_ms();
        let gifted = CompanionSkill {
            companion_skill_id: CompanionSkillId::new().into_string(),
            skill_name: src.skill_name.clone(),
            scope_kind: "companion".into(),
            scope_companion_id: Some(to_companion_id.to_owned()),
            status: "active".into(),
            source: "gifted".into(),
            confidence: src.confidence,
            provenance_event_ids: vec![],
            strength: 1.0,
            version: 1,
            skill_pattern_id: None,
            usage_count: 0,
            last_used_at: None,
            created_at: now,
            updated_at: now,
            signature: String::new(),
        };
        if let Err(error) = self.store.insert_skill(&gifted).await {
            let target = skill_service::skill_dir_for(
                &self.skill_paths,
                &SkillScope::Companion(to_companion_id.to_owned()),
                &src.skill_name,
                false,
            )
            .map_err(|path_error| {
                AppError::Internal(format!("resolve gifted skill for rollback: {path_error}"))
            })?;
            crate::fsio::remove_path_entry(&target).map_err(|cleanup_error| {
                AppError::Internal(format!(
                    "{error}; additionally failed to remove orphaned gifted skill {}: {cleanup_error}",
                    target.display()
                ))
            })?;
            return Err(error);
        }
        self.emitter.emit_skill_learned(
            to_companion_id,
            &gifted.companion_skill_id,
            &gifted.skill_name,
        );
        Ok(gifted)
    }

    // ----- learning -----

    pub async fn run_learn_now(&self) -> Result<CompanionLearnRun, AppError> {
        self.learner.run_once().await
    }

    pub async fn list_learn_runs(&self, limit: i64) -> Result<Vec<CompanionLearnRun>, AppError> {
        self.store.list_learn_runs(limit).await
    }

    // ----- events -----

    pub fn event_stats(&self) -> Result<Vec<SourceStats>, AppError> {
        Ok(collector::event_stats(&self.shared_dir)?
            .into_iter()
            .map(|(source, (today, total))| SourceStats { source, today, total })
            .collect())
    }

    pub fn clear_events(&self) -> Result<(), AppError> {
        collector::clear_events(&self.shared_dir).map_err(|e| AppError::Internal(format!("clear companion events: {e}")))
    }
}

/// The factory-facing persona prompt provider: Channel Agent sessions
/// carry `companion_session` but no persisted `system_prompt`, so the nomi factory
/// asks the bound companion for a fresh persona (with current memory snapshot) at
/// every agent build. The persona is built **only** for an explicitly-bound, live
/// companion; `companion_id: None` or a dead id yields no persona — an unbound
/// channel is hosted by no companion (no default-companion fallback; 历史债
/// 「渠道与远程连接默认由默认伙伴接待」已废除，连接由用户为每个伙伴显式配置).
#[async_trait::async_trait]
impl nomifun_ai_agent::CompanionPromptProvider for CompanionService {
    async fn build_system_prompt(&self, companion_id: Option<&str>, channel_platform: Option<&str>) -> Option<String> {
        let companion_id = CompanionId::try_from(companion_id?).ok()?;
        let profile = self.registry.get(companion_id.as_str()).await?;
        let smart = self.config.read().await.smart_collaboration;
        Some(crate::companion::build_companion_system_prompt(&self.store, &profile, channel_platform, smart).await)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nomifun_realtime::BroadcastEventBus;

    fn companion_fixture(sequence: u64) -> String {
        let raw = format!("0190f5fe-7c00-7a00-8abc-{sequence:012}");
        nomifun_common::CompanionId::try_from(raw.as_str()).unwrap().into_string()
    }

    fn conversation_fixture(sequence: u64) -> String {
        let raw = format!("0190f5fe-7c00-7a00-8abc-{sequence:012}");
        nomifun_common::ConversationId::try_from(raw.as_str()).unwrap().into_string()
    }

    fn provider_fixture(sequence: u64) -> String {
        let raw = format!("0190f5fe-7c00-7a00-8abc-{sequence:012}");
        nomifun_common::ProviderId::try_from(raw.as_str()).unwrap().into_string()
    }

    const MALFORMED_COMPANION_ID: &str = "not-a-companion-id";
    const MALFORMED_PROVIDER_ID: &str = "not-a-provider-id";

    struct NoopCompleter;

    #[async_trait::async_trait]
    impl CompanionCompleter for NoopCompleter {
        async fn complete(&self, _p: &str, _m: &str, _s: &str, _u: &str, _t: u32) -> Result<String, AppError> {
            Ok("{}".into())
        }
    }

    async fn service(data_dir: &std::path::Path) -> Arc<CompanionService> {
        CompanionService::start(
            data_dir,
            Arc::new(BroadcastEventBus::new(16)),
            "owner-a",
            Arc::new(NoopCompleter),
            Arc::new(nomifun_extension::skill_service::resolve_skill_paths(data_dir, data_dir)),
        )
        .await
        .unwrap()
    }

    #[tokio::test]
    async fn start_rejects_missing_authoritative_owner() {
        let dir = tempfile::tempdir().unwrap();
        let result = CompanionService::start(
            dir.path(),
            Arc::new(BroadcastEventBus::new(16)),
            "  ",
            Arc::new(NoopCompleter),
            Arc::new(nomifun_extension::skill_service::resolve_skill_paths(
                dir.path(),
                dir.path(),
            )),
        )
        .await;

        assert!(matches!(result, Err(AppError::Internal(message)) if message.contains("owner id")));
    }

    #[tokio::test]
    async fn providers_in_use_detects_companion_chat_model() {
        let dir = tempfile::tempdir().unwrap();
        let svc = service(dir.path()).await;
        let cid = svc.registry.create("大聪明", "ink").await.unwrap().companion_id;
        let provider_id = provider_fixture(1);
        svc.patch_companion(&cid, serde_json::json!({"model":{"provider_id": provider_id,"model":"m"}})).await.unwrap();

        let hits = svc.providers_in_use(&provider_id).await;
        assert!(hits.iter().any(|u| u.label == "大聪明" && u.target_id.as_deref() == Some(cid.as_str())));
        assert!(svc.providers_in_use(&provider_fixture(99)).await.is_empty());
    }

    #[tokio::test]
    async fn providers_in_use_detects_shared_learn_model() {
        let dir = tempfile::tempdir().unwrap();
        let svc = service(dir.path()).await;
        let provider_id = provider_fixture(2);
        svc.patch_config(serde_json::json!({"learn":{"model":{"provider_id": provider_id,"model":"m"}}})).await.unwrap();
        let hits = svc.providers_in_use(&provider_id).await;
        assert!(hits.iter().any(|u| matches!(u.feature, nomifun_common::ProviderUsageFeature::DesktopCompanion)));
    }

    #[tokio::test]
    async fn providers_in_use_detects_shared_evolve_model() {
        let dir = tempfile::tempdir().unwrap();
        let svc = service(dir.path()).await;
        let provider_id = provider_fixture(3);
        svc.patch_config(serde_json::json!({"evolve":{"model":{"provider_id": provider_id,"model":"m"}}})).await.unwrap();
        let hits = svc.providers_in_use(&provider_id).await;
        assert!(
            hits.iter()
                .any(|u| u.label == "共享进化模型" && u.target_id.is_none())
        );
    }

    #[tokio::test]
    async fn providers_in_use_rejects_malformed_provider_id() {
        let dir = tempfile::tempdir().unwrap();
        let svc = service(dir.path()).await;
        svc.registry.create("未配置", "ink").await.unwrap();
        for malformed in ["", MALFORMED_PROVIDER_ID] {
            assert!(svc.providers_in_use(malformed).await.is_empty());
        }
    }

    #[tokio::test]
    async fn accepting_create_skill_promotes_draft_and_is_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let svc = service(dir.path()).await;
        let companion = svc.registry.create("测试", "ink").await.unwrap();
        let cid = companion.companion_id;

        // A reviewed draft: SKILL.md on disk (draft dir) + a draft registry row.
        let input = nomifun_extension::skill_service::SkillDraftInput {
            name: "demo".into(),
            description: "演示技能".into(),
            when_to_use: None,
            allowed_tools: None,
            paths: None,
            body: "步骤".into(),
        };
        let scope = SkillScope::Companion(cid.clone());
        skill_service::create_skill(&svc.skill_paths, &scope, true, &input).await.unwrap();
        let now = nomifun_common::now_ms();
        svc.store
            .insert_skill(&crate::store::CompanionSkill {
            companion_skill_id: nomifun_common::generate_id(),
                skill_name: "demo".into(),
                scope_kind: "companion".into(),
                scope_companion_id: Some(cid.clone()),
                status: "draft".into(),
                source: "mined".into(),
                confidence: 0.9,
                provenance_event_ids: vec![],
                strength: 1.0,
                version: 1,
                skill_pattern_id: None,
                usage_count: 0,
                last_used_at: None,
                created_at: now,
                updated_at: now,
                signature: String::new(),
            })
            .await
            .unwrap();
        let demo_skill_id = svc.store.find_owned_skill_by_name(&cid, "demo").await.unwrap().unwrap().companion_skill_id;
        let action = serde_json::json!({"type": "create_skill", "companion_skill_id": demo_skill_id, "companion_id": cid});
        let sug = svc.store.insert_suggestion("create_skill", "学会 demo", "body", Some(&action)).await.unwrap();

        // Accept → promote draft to active.
        svc.decide_suggestion(&sug.suggestion_id, true).await.unwrap();
        let active_md = svc.skill_paths.user_skills_dir.join("companion").join(&cid).join("demo").join("SKILL.md");
        let draft_dir = svc.skill_paths.user_skills_dir.join("_drafts").join(&cid).join("demo");
        assert!(active_md.exists(), "active SKILL.md missing at {}", active_md.display());
        assert!(!draft_dir.exists(), "promoted draft must be removed");
        assert_eq!(svc.store.find_owned_skill_by_name(&cid, "demo").await.unwrap().unwrap().status, "active");
        let xp1 = svc.store.get_companion_state_i64(&cid, "xp").await.unwrap();
        assert!(xp1 >= 20, "accept should grant shared XP");

        // Re-accept → idempotent: no re-award, status unchanged.
        svc.decide_suggestion(&sug.suggestion_id, true).await.unwrap();
        let xp2 = svc.store.get_companion_state_i64(&cid, "xp").await.unwrap();
        assert_eq!(xp1, xp2, "re-accept must not re-award xp");
        assert_eq!(svc.store.find_owned_skill_by_name(&cid, "demo").await.unwrap().unwrap().status, "active");
    }

    #[tokio::test]
    async fn delete_companion_removes_active_and_draft_skill_trees() {
        let dir = tempfile::tempdir().unwrap();
        let svc = service(dir.path()).await;
        let cid = svc.registry.create("测试", "ink").await.unwrap().companion_id;
        let input = nomifun_extension::skill_service::SkillDraftInput {
            name: "cleanup".into(),
            description: "删除测试".into(),
            when_to_use: None,
            allowed_tools: None,
            paths: None,
            body: "步骤".into(),
        };
        let scope = SkillScope::Companion(cid.clone());
        skill_service::create_skill(&svc.skill_paths, &scope, false, &input)
            .await
            .unwrap();
        skill_service::create_skill(&svc.skill_paths, &scope, true, &input)
            .await
            .unwrap();
        let active_root =
            skill_service::companion_skills_root(&svc.skill_paths).join(&cid);
        let draft_root = skill_service::drafts_root(&svc.skill_paths).join(&cid);
        assert!(active_root.exists());
        assert!(draft_root.exists());

        svc.delete_companion(&cid).await.unwrap();

        assert!(!active_root.exists());
        assert!(!draft_root.exists());
    }

    /// Seed a draft skill (SKILL.md on disk + registry row).
    async fn seed_draft_skill(svc: &CompanionService, cid: &str, name: &str) {
        let input = nomifun_extension::skill_service::SkillDraftInput {
            name: name.into(),
            description: "原始描述".into(),
            when_to_use: None,
            allowed_tools: None,
            paths: None,
            body: "步骤".into(),
        };
        skill_service::create_skill(&svc.skill_paths, &SkillScope::Companion(cid.to_owned()), true, &input)
            .await
            .unwrap();
        let now = nomifun_common::now_ms();
        svc.store
            .insert_skill(&CompanionSkill {
            companion_skill_id: nomifun_common::generate_id(),
                skill_name: name.into(),
                scope_kind: "companion".into(),
                scope_companion_id: Some(cid.to_owned()),
                status: "draft".into(),
                source: "mined".into(),
                confidence: 0.5,
                provenance_event_ids: vec![],
                strength: 1.0,
                version: 1,
                skill_pattern_id: None,
                usage_count: 0,
                last_used_at: None,
                created_at: now,
                updated_at: now,
                signature: String::new(),
            })
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn list_companion_skills_fails_closed_on_missing_file() {
        let dir = tempfile::tempdir().unwrap();
        let svc = service(dir.path()).await;
        let cid = svc.registry.create("测试", "ink").await.unwrap().companion_id;
        seed_draft_skill(&svc, &cid, "alpha").await; // SKILL.md on disk
        // beta: registry row only, NO SKILL.md → fail closed.
        let now = nomifun_common::now_ms();
        svc.store
            .insert_skill(&CompanionSkill {
            companion_skill_id: nomifun_common::generate_id(),
                skill_name: "beta".into(),
                scope_kind: "companion".into(),
                scope_companion_id: Some(cid.clone()),
                status: "draft".into(),
                source: "mined".into(),
                confidence: 0.5,
                provenance_event_ids: vec![],
                strength: 1.0,
                version: 1,
                skill_pattern_id: None,
                usage_count: 0,
                last_used_at: None,
                created_at: now,
                updated_at: now,
                signature: String::new(),
            })
            .await
            .unwrap();
        assert!(svc.list_companion_skills(&cid, false).await.is_err());
    }

    #[tokio::test]
    async fn list_companion_skill_page_enriches_only_current_page() {
        let dir = tempfile::tempdir().unwrap();
        let svc = service(dir.path()).await;
        let cid = svc.registry.create("测试", "ink").await.unwrap().companion_id;
        seed_draft_skill(&svc, &cid, "alpha").await;
        seed_draft_skill(&svc, &cid, "beta").await;

        let page = svc
            .list_companion_skill_page(&cid, false, Some("draft"), 1, 0)
            .await
            .unwrap();

        assert_eq!(page.total, 2);
        assert_eq!(page.items.len(), 1);
        assert_eq!(page.items[0].description, "原始描述");
    }

    #[tokio::test]
    async fn get_and_write_skill_content_roundtrip_and_validate() {
        let dir = tempfile::tempdir().unwrap();
        let svc = service(dir.path()).await;
        let cid = svc.registry.create("测试", "ink").await.unwrap().companion_id;
        seed_draft_skill(&svc, &cid, "demo").await;

        assert!(svc.get_companion_skill_content(&cid, &svc.store.find_owned_skill_by_name(&cid, "demo").await.unwrap().unwrap().companion_skill_id).await.unwrap().content.contains("原始描述"));
        // edit with a valid full SKILL.md
        let new_md = "---\nname: demo\ndescription: 改后描述\n---\n\n新步骤\n";
        svc.write_companion_skill_content(&cid, &svc.store.find_owned_skill_by_name(&cid, "demo").await.unwrap().unwrap().companion_skill_id, new_md).await.unwrap();
        assert!(svc.get_companion_skill_content(&cid, &svc.store.find_owned_skill_by_name(&cid, "demo").await.unwrap().unwrap().companion_skill_id).await.unwrap().content.contains("改后描述"));
        // empty description → BadRequest; missing skill → NotFound
        assert!(svc.write_companion_skill_content(&cid, &svc.store.find_owned_skill_by_name(&cid, "demo").await.unwrap().unwrap().companion_skill_id, "---\nname: demo\ndescription:\n---\nx").await.is_err());
        assert!(svc.get_companion_skill_content(&cid, &nomifun_common::generate_id()).await.is_err());
    }

    #[tokio::test]
    async fn decide_companion_skill_accept_reject_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let svc = service(dir.path()).await;
        let cid = svc.registry.create("测试", "ink").await.unwrap().companion_id;

        seed_draft_skill(&svc, &cid, "acc").await;
        let r = svc.decide_companion_skill(&cid, &svc.store.find_owned_skill_by_name(&cid, "acc").await.unwrap().unwrap().companion_skill_id, true, None).await.unwrap();
        assert_eq!(r.status, "active");
        assert!(svc.skill_paths.user_skills_dir.join("companion").join(&cid).join("acc").join("SKILL.md").exists());
        // re-accept on a non-draft row is an idempotent no-op
        assert_eq!(svc.decide_companion_skill(&cid, &svc.store.find_owned_skill_by_name(&cid, "acc").await.unwrap().unwrap().companion_skill_id, true, None).await.unwrap().status, "active");

        seed_draft_skill(&svc, &cid, "rej").await;
        let r3 = svc.decide_companion_skill(&cid, &svc.store.find_owned_skill_by_name(&cid, "rej").await.unwrap().unwrap().companion_skill_id, false, Some("太窄")).await.unwrap();
        assert_eq!(r3.status, "archived");
        assert!(
            svc.skill_paths
                .user_skills_dir
                .join("companion")
                .join(&cid)
                .join("rej")
                .join("SKILL.md")
                .exists()
        );
        assert!(
            !svc.skill_paths
                .user_skills_dir
                .join("_drafts")
                .join(&cid)
                .join("rej")
                .exists()
        );
    }

    #[tokio::test]
    async fn rejecting_skill_suppresses_its_originating_pattern() {
        let dir = tempfile::tempdir().unwrap();
        let svc = service(dir.path()).await;
        let cid = svc.registry.create("测试", "ink").await.unwrap().companion_id;
        let input = nomifun_extension::skill_service::SkillDraftInput {
            name: "rej-sig".into(),
            description: "d".into(),
            when_to_use: None,
            allowed_tools: None,
            paths: None,
            body: "b".into(),
        };
        skill_service::create_skill(&svc.skill_paths, &SkillScope::Companion(cid.clone()), true, &input).await.unwrap();
        let now = nomifun_common::now_ms();
        svc.store
            .insert_skill(&CompanionSkill {
            companion_skill_id: nomifun_common::generate_id(),
                skill_name: "rej-sig".into(),
                scope_kind: "companion".into(),
                scope_companion_id: Some(cid.clone()),
                status: "draft".into(),
                source: "mined".into(),
                confidence: 0.5,
                provenance_event_ids: vec![],
                strength: 1.0,
                version: 1,
                skill_pattern_id: None,
                usage_count: 0,
                last_used_at: None,
                created_at: now,
                updated_at: now,
                signature: "sig-XYZ".into(),
            })
            .await
            .unwrap();
        assert!(!svc.store.is_signature_rejected("sig-XYZ").await.unwrap());
        svc.decide_companion_skill(&cid, &svc.store.find_owned_skill_by_name(&cid, "rej-sig").await.unwrap().unwrap().companion_skill_id, false, Some("不通用")).await.unwrap();
        assert!(
            svc.store.is_signature_rejected("sig-XYZ").await.unwrap(),
            "rejecting a skill must suppress its originating mined pattern"
        );
    }

    #[tokio::test]
    async fn weekly_digest_counts_recent_skills() {
        let dir = tempfile::tempdir().unwrap();
        let svc = service(dir.path()).await;
        let cid = svc.registry.create("测试", "ink").await.unwrap().companion_id;
        let now = nomifun_common::now_ms();
        svc.store
            .insert_skill(&CompanionSkill {
            companion_skill_id: nomifun_common::generate_id(),
                skill_name: "recent".into(),
                scope_kind: "companion".into(),
                scope_companion_id: Some(cid.clone()),
                status: "active".into(),
                source: "mined".into(),
                confidence: 0.5,
                provenance_event_ids: vec![],
                strength: 1.0,
                version: 1,
                skill_pattern_id: None,
                usage_count: 0,
                last_used_at: None,
                created_at: now,
                updated_at: now,
                signature: String::new(),
            })
            .await
            .unwrap();
        let digest = svc.weekly_digest(&cid, now - 7 * 86_400_000).await.unwrap();
        assert_eq!(digest.skills_learned, 1);
        assert_eq!(digest.skills_active_new, 1);
        assert!(digest.new_skill_names.contains(&"recent".to_string()));
    }

    #[tokio::test]
    async fn draft_from_session_requires_activity() {
        let dir = tempfile::tempdir().unwrap();
        let svc = service(dir.path()).await;
        let cid = svc.registry.create("测试", "ink").await.unwrap().companion_id;
        // No collected tool-call activity for this conversation → BadRequest, not a panic.
        assert!(svc.draft_skill_from_session(&cid, &conversation_fixture(90)).await.is_err());
    }

    #[tokio::test]
    async fn gift_copies_skill_to_recipient_and_guards() {
        let dir = tempfile::tempdir().unwrap();
        let svc = service(dir.path()).await;
        let a = svc.registry.create("A", "ink").await.unwrap().companion_id;
        let b = svc.registry.create("B", "ink").await.unwrap().companion_id;
        let input = nomifun_extension::skill_service::SkillDraftInput {
            name: "share-me".into(),
            description: "d".into(),
            when_to_use: None,
            allowed_tools: None,
            paths: None,
            body: "b".into(),
        };
        skill_service::create_skill(&svc.skill_paths, &SkillScope::Companion(a.clone()), false, &input).await.unwrap();
        let now = nomifun_common::now_ms();
        svc.store
            .insert_skill(&CompanionSkill {
            companion_skill_id: nomifun_common::generate_id(),
                skill_name: "share-me".into(),
                scope_kind: "companion".into(),
                scope_companion_id: Some(a.clone()),
                status: "active".into(),
                source: "mined".into(),
                confidence: 0.7,
                provenance_event_ids: vec![],
                strength: 1.0,
                version: 1,
                skill_pattern_id: None,
                usage_count: 0,
                last_used_at: None,
                created_at: now,
                updated_at: now,
                signature: String::new(),
            })
            .await
            .unwrap();
        assert!(svc.gift_companion_skill(&a, &svc.store.find_owned_skill_by_name(&a, "share-me").await.unwrap().unwrap().companion_skill_id, &a).await.is_err(), "self-gift rejected");
        let source_skill_id = svc.store.find_owned_skill_by_name(&a, "share-me").await.unwrap().unwrap().companion_skill_id;
        let g = svc.gift_companion_skill(&a, &source_skill_id, &b).await.unwrap();
        assert_eq!(g.scope_companion_id.as_deref(), Some(b.as_str()));
        assert_eq!(g.source, "gifted");
        assert_eq!(svc.store.find_owned_skill_by_name(&b, "share-me").await.unwrap().unwrap().status, "active");
        assert!(svc.skill_paths.user_skills_dir.join("companion").join(&b).join("share-me").join("SKILL.md").exists());
        assert!(svc.gift_companion_skill(&a, &source_skill_id, &b).await.is_err(), "name collision rejected");
    }

    #[tokio::test]
    async fn recent_events_returns_newest_window() {
        let dir = tempfile::tempdir().unwrap();
        let svc = service(dir.path()).await;
        let base = nomifun_common::now_ms();
        for i in 0..5 {
            crate::collector::append_event(
                &svc.shared_dir,
                &crate::collector::CollectedEvent {
                    event_id: nomifun_common::generate_id(),
                    ts: base + i,
                    source: "tool_calls".into(),
                    name: "tool.call".into(),
                    data: serde_json::json!({ "name": format!("t{i}") }),
                },
            )
            .unwrap();
        }
        assert_eq!(svc.recent_events(3).unwrap().len(), 3);
        assert_eq!(svc.recent_events(100).unwrap().len(), 5);
    }

    #[tokio::test]
    async fn disable_all_turns_everything_off_but_keeps_models() {
        let dir = tempfile::tempdir().unwrap();
        let svc = service(dir.path()).await;
        let provider_id = provider_fixture(4);
        svc.patch_config(serde_json::json!({
            "collect": { "tool_calls": true, "chat_user_messages": true, "companion_dialogues": true },
            "learn": { "enabled": true, "interval_minutes": 30, "model": { "provider_id": provider_id, "model": "m" } },
            "evolve": { "enabled": true, "model": { "provider_id": provider_id, "model": "m" } }
        }))
        .await
        .unwrap();

        svc.disable_all().await.unwrap();
        let cfg = svc.config.read().await;
        assert!(!cfg.collect.tool_calls);
        assert!(!cfg.collect.chat_user_messages);
        assert!(!cfg.collect.companion_dialogues, "kill switch must turn OFF companion_dialogues");
        assert!(!cfg.learn.enabled);
        assert!(!cfg.evolve.enabled);
        // models + interval preserved so re-enable needs no reconfig
        assert_eq!(cfg.learn.model.as_ref().unwrap().provider_id, provider_id);
        assert_eq!(cfg.learn.interval_minutes, 30);
        assert_eq!(cfg.evolve.model.as_ref().unwrap().provider_id, provider_id);
    }

    #[tokio::test]
    async fn consent_applies_once_and_never_reenables_after_disable() {
        let dir = tempfile::tempdir().unwrap();
        let svc = service(dir.path()).await;
        // fresh: work sources + learn + evolve all off (Default untouched)
        assert!(!svc.config.read().await.collect.tool_calls);
        assert!(!svc.config.read().await.learn.enabled);

        // first-launch consent → default-on applied + flag set
        svc.apply_default_on_consent().await.unwrap();
        {
            let cfg = svc.config.read().await;
            assert!(cfg.collect.tool_calls);
            assert!(cfg.collect.chat_user_messages);
            assert!(cfg.collect.requirements);
            assert!(cfg.collect.conversation_lifecycle);
            // OUTPUT side stays opt-in: long model replies + untruncated cron output
            // are NOT auto-enabled (user-request-only collection policy).
            assert!(!cfg.collect.chat_assistant_replies, "model replies must stay opt-in");
            assert!(!cfg.collect.cron_runs, "cron output must stay opt-in");
            assert!(cfg.learn.enabled);
            assert!(cfg.evolve.enabled);
        }
        assert_eq!(svc.store.get_state("self_evolution_consent").await.unwrap().as_deref(), Some("1"));

        // user explicitly kills everything
        svc.disable_all().await.unwrap();
        assert!(!svc.config.read().await.collect.tool_calls);

        // re-consent must be an idempotent no-op (flag set) — NEVER silently re-enable
        svc.apply_default_on_consent().await.unwrap();
        assert!(!svc.config.read().await.collect.tool_calls, "must not re-enable after explicit disable");
        assert!(!svc.config.read().await.learn.enabled);
    }

    #[tokio::test]
    async fn config_writes_cannot_reset_seq_watermark() {
        let dir = tempfile::tempdir().unwrap();
        let svc = service(dir.path()).await;
        let a = svc.create_companion("甲", "ink").await.unwrap();
        let b = svc.create_companion("乙", "boo").await.unwrap();
        assert_eq!(b.seq, 2);
        svc.delete_companion(&b.companion_id).await.unwrap();

        // A full-object PUT of the shared config (the route body simply has
        // no watermark field — it lives in the registry's own state file)…
        let put = SharedCompanionConfig {
            default_companion_id: Some(a.companion_id.clone()),
            ..SharedCompanionConfig::default()
        };
        svc.update_config(put).await.unwrap();
        // …and a merge patch.
        svc.patch_config(serde_json::json!({"learn": {"enabled": true}}))
            .await
            .unwrap();

        // Neither write path can hand out the deleted companion's number again.
        let c = svc.create_companion("丙", "mochi").await.unwrap();
        assert_eq!(c.seq, 3);

        // The watermark file is independent of the shared config file, which
        // carries no watermark field at all.
        let shared_dir = dir.path().join(crate::COMPANION_SHARED_REL_DIR);
        assert_eq!(crate::registry::CompanionSeqState::load(&shared_dir).unwrap().last_companion_seq, 3);
        let cfg_raw = std::fs::read_to_string(SharedCompanionConfig::config_path(&shared_dir)).unwrap();
        assert!(!cfg_raw.contains("last_companion_seq"), "config.json must not carry the watermark: {cfg_raw}");
    }

    #[tokio::test]
    async fn create_companion_first_becomes_default() {
        let dir = tempfile::tempdir().unwrap();
        let svc = service(dir.path()).await;
        assert!(svc.list_companions().await.is_empty());
        assert_eq!(svc.default_companion_id().await, None);

        let first = svc.create_companion("毛球", "ink").await.unwrap();
        assert_eq!(svc.get_config().await.default_companion_id.as_deref(), Some(first.companion_id.as_str()));
        assert_eq!(svc.default_companion_id().await.as_deref(), Some(first.companion_id.as_str()));
        // Persisted, not just in memory.
        let on_disk =
            SharedCompanionConfig::load(&dir.path().join(crate::COMPANION_SHARED_REL_DIR)).unwrap();
        assert_eq!(on_disk.default_companion_id.as_deref(), Some(first.companion_id.as_str()));

        // A second companion never steals the default.
        let second = svc.create_companion("墨墨", "boo").await.unwrap();
        assert_eq!(svc.get_config().await.default_companion_id.as_deref(), Some(first.companion_id.as_str()));
        assert_ne!(first.companion_id, second.companion_id);
        assert_eq!(svc.list_companions().await.len(), 2);
    }

    #[tokio::test]
    async fn delete_companion_repoints_default_and_clears_rows() {
        let dir = tempfile::tempdir().unwrap();
        let svc = service(dir.path()).await;
        let a = svc.create_companion("甲", "ink").await.unwrap();
        let b = svc.create_companion("乙", "boo").await.unwrap();
        assert_eq!(svc.get_config().await.default_companion_id.as_deref(), Some(a.companion_id.as_str()));

        // Give A per-companion rows that must vanish with it.
        svc.store.add_companion_xp(&a.companion_id, 42).await.unwrap();
        let conversation = conversation_fixture(1);
        svc.store.insert_companion_thread(&conversation, &a.companion_id, "甲聊").await.unwrap();
        svc.store.add_companion_xp(&b.companion_id, 7).await.unwrap();

        svc.delete_companion(&a.companion_id).await.unwrap();

        assert!(matches!(svc.get_companion(&a.companion_id).await, Err(AppError::NotFound(_))));
        assert_eq!(svc.store.get_companion_state_i64(&a.companion_id, "xp").await.unwrap(), 0);
        assert!(!svc.store.is_companion_thread(&conversation).await.unwrap());
        // Default re-pointed to the survivor; survivor untouched.
        assert_eq!(svc.get_config().await.default_companion_id.as_deref(), Some(b.companion_id.as_str()));
        assert_eq!(svc.store.get_companion_state_i64(&b.companion_id, "xp").await.unwrap(), 7);

        // Deleting the last companion clears the default entirely.
        svc.delete_companion(&b.companion_id).await.unwrap();
        assert_eq!(svc.get_config().await.default_companion_id, None);
        assert_eq!(svc.default_companion_id().await, None);

        assert!(matches!(svc.delete_companion(&a.companion_id).await, Err(AppError::NotFound(_))));
    }

    #[tokio::test]
    async fn delete_companion_invokes_cleanup_hooks() {
        struct RecordingHook(std::sync::Mutex<Vec<String>>);

        #[async_trait::async_trait]
        impl CompanionCleanupHook for RecordingHook {
            async fn on_companion_deleted(&self, companion_id: &str) {
                self.0.lock().unwrap().push(companion_id.to_owned());
            }
        }

        let dir = tempfile::tempdir().unwrap();
        let svc = service(dir.path()).await;
        let hook = Arc::new(RecordingHook(std::sync::Mutex::new(Vec::new())));
        svc.set_cleanup_hooks(vec![hook.clone() as Arc<dyn CompanionCleanupHook>]);

        let p = svc.create_companion("丙", "ink").await.unwrap();
        svc.delete_companion(&p.companion_id).await.unwrap();
        assert_eq!(hook.0.lock().unwrap().as_slice(), &[p.companion_id.clone()]);

        // A failed delete (unknown id) must not fire the hooks again.
        assert!(matches!(svc.delete_companion(&p.companion_id).await, Err(AppError::NotFound(_))));
        assert_eq!(hook.0.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn decide_suggestion_awards_every_companion() {
        let dir = tempfile::tempdir().unwrap();
        let svc = service(dir.path()).await;
        let a = svc.create_companion("甲", "ink").await.unwrap();
        let b = svc.create_companion("乙", "boo").await.unwrap();

        let s = svc
            .store
            .insert_suggestion("insight", "洞察", "试试看", None)
            .await
            .unwrap();
        let decided = svc.decide_suggestion(&s.suggestion_id, true).await.unwrap();
        assert_eq!(decided.status, "accepted");
        assert_eq!(svc.store.get_companion_state_i64(&a.companion_id, "xp").await.unwrap(), 20);
        assert_eq!(svc.store.get_companion_state_i64(&b.companion_id, "xp").await.unwrap(), 20);
        // Shared/global xp stays untouched.
        assert_eq!(svc.store.get_state_i64("xp").await.unwrap(), 0);

        // Idempotent: re-accepting the same suggestion (stale card / double
        // click / cross-surface repeat) must NOT re-award xp.
        let again = svc.decide_suggestion(&s.suggestion_id, true).await.unwrap();
        assert_eq!(again.status, "accepted");
        assert_eq!(svc.store.get_companion_state_i64(&a.companion_id, "xp").await.unwrap(), 20);
        assert_eq!(svc.store.get_companion_state_i64(&b.companion_id, "xp").await.unwrap(), 20);

        // Dismissals award nothing.
        let s2 = svc
            .store
            .insert_suggestion("insight", "再来", "不要", None)
            .await
            .unwrap();
        svc.decide_suggestion(&s2.suggestion_id, false).await.unwrap();
        assert_eq!(svc.store.get_companion_state_i64(&a.companion_id, "xp").await.unwrap(), 20);
    }

    #[tokio::test]
    async fn status_uses_default_companion_and_survives_no_companions() {
        let dir = tempfile::tempdir().unwrap();
        let svc = service(dir.path()).await;

        // No companions yet: zeroed fallback.
        let empty = svc.status().await.unwrap();
        assert_eq!(empty.companion_id, None);
        assert_eq!(empty.xp, 0);
        assert_eq!(empty.level, 1);
        assert!(!empty.model_configured);

        let a = svc.create_companion("甲", "ink").await.unwrap();
        svc.store.add_companion_xp(&a.companion_id, 150).await.unwrap();
        let status = svc.status().await.unwrap();
        assert_eq!(status.companion_id.as_deref(), Some(a.companion_id.as_str()));
        assert_eq!(status.xp, 150);
        assert_eq!(status.level, 2);

        // Per-companion status for a second companion reads its own xp.
        let b = svc.create_companion("乙", "boo").await.unwrap();
        let sb = svc.companion_status(&b.companion_id).await.unwrap();
        assert_eq!(sb.companion_id.as_deref(), Some(b.companion_id.as_str()));
        assert_eq!(sb.xp, 0);
        assert!(matches!(svc.companion_status(MALFORMED_COMPANION_ID).await, Err(AppError::NotFound(_))));
    }

    #[tokio::test]
    async fn start_rejects_dangling_default_companion_reference() {
        let dir = tempfile::tempdir().unwrap();
        let svc = service(dir.path()).await;
        let _a = svc.create_companion("甲", "ink").await.unwrap();
        let _b = svc.create_companion("乙", "boo").await.unwrap();

        let shared_dir = dir.path().join(crate::COMPANION_SHARED_REL_DIR);
        let mut corrupt = svc.get_config().await;
        corrupt.default_companion_id = Some(companion_fixture(999));
        corrupt.save(&shared_dir).unwrap();
        drop(svc);
        let restarted = CompanionService::start(
            dir.path(),
            Arc::new(BroadcastEventBus::new(16)),
            "owner-a",
            Arc::new(NoopCompleter),
            Arc::new(nomifun_extension::skill_service::resolve_skill_paths(
                dir.path(),
                dir.path(),
            )),
        )
        .await;
        assert!(
            matches!(restarted, Err(AppError::Internal(message)) if message.contains("missing default companion"))
        );
    }

    #[tokio::test]
    async fn patch_companion_emits_and_persists() {
        let dir = tempfile::tempdir().unwrap();
        let svc = service(dir.path()).await;
        let a = svc.create_companion("甲", "ink").await.unwrap();
        let patched = svc
            .patch_companion(&a.companion_id, serde_json::json!({"name": "新名", "appearance": {"companion_enabled": true}}))
            .await
            .unwrap();
        assert_eq!(patched.name, "新名");
        assert!(patched.appearance.companion_enabled);
        assert_eq!(svc.get_companion(&a.companion_id).await.unwrap().name, "新名");
    }

    #[tokio::test]
    async fn patch_companion_model_change_is_best_effort_when_companion_unwired() {
        // Setting a model triggers companion-session ensure + model
        // propagation, both best-effort. With no companion wired (this test
        // harness), the patch must still succeed and persist the model — the
        // model唯一事实源 (profile.model) is always written regardless.
        let dir = tempfile::tempdir().unwrap();
        let svc = service(dir.path()).await;
        let a = svc.create_companion("甲", "ink").await.unwrap();
        assert!(a.model.is_none());
        let provider_id = provider_fixture(5);

        let patched = svc
            .patch_companion(&a.companion_id, serde_json::json!({"model": {"provider_id": provider_id, "model": "claude-fable-5"}}))
            .await
            .unwrap();
        assert_eq!(patched.model.as_ref().unwrap().provider_id, provider_id);
        assert_eq!(svc.get_companion(&a.companion_id).await.unwrap().model.unwrap().model, "claude-fable-5");

        // A non-model patch on an already-configured companion also succeeds (no
        // spurious propagation path, model unchanged).
        let renamed = svc.patch_companion(&a.companion_id, serde_json::json!({"name": "甲改"})).await.unwrap();
        assert_eq!(renamed.name, "甲改");
        assert_eq!(renamed.model.as_ref().unwrap().provider_id, provider_id);
    }

    #[tokio::test]
    async fn add_memory_dedups_into_existing_active_memory() {
        let dir = tempfile::tempdir().unwrap();
        let svc = service(dir.path()).await;

        let first = svc
            .add_memory("preference", "主人喜欢深色主题", &["ui".into()], MemoryScope::Shared)
            .await
            .unwrap();
        assert_eq!(svc.store.count_memories("active").await.unwrap(), 1);

        // Same content (modulo case/whitespace) merges: reinforced, no new row.
        let again = svc.add_memory("preference", " 主人喜欢深色主题 ", &[], MemoryScope::Shared).await.unwrap();
        assert_eq!(again.memory_id, first.memory_id);
        assert_eq!(svc.store.count_memories("active").await.unwrap(), 1);
        assert!(again.strength > first.strength, "dedup hit must reinforce the existing memory");

        // Genuinely different content still inserts.
        let other = svc.add_memory("preference", "主人喜欢浅色代码字体", &[], MemoryScope::Shared).await.unwrap();
        assert_ne!(other.memory_id, first.memory_id);
        assert_eq!(svc.store.count_memories("active").await.unwrap(), 2);

        // Validation untouched.
        assert!(svc.add_memory("bogus", "x", &[], MemoryScope::Shared).await.is_err());
        assert!(svc.add_memory("task", "   ", &[], MemoryScope::Shared).await.is_err());
    }

    #[tokio::test]
    async fn companion_prompt_provider_builds_only_for_bound_companion() {
        use nomifun_ai_agent::CompanionPromptProvider;
        let dir = tempfile::tempdir().unwrap();
        let svc = service(dir.path()).await;

        // No companions: no persona.
        assert!(svc.build_system_prompt(None, None).await.is_none());

        let a = svc.create_companion("毛球", "ink").await.unwrap();
        let b = svc.create_companion("墨墨", "boo").await.unwrap();
        // No companion_id → NO persona (历史债「渠道默认由默认伙伴接待」已废除；不再回落默认伙伴).
        assert!(svc.build_system_prompt(None, None).await.is_none());
        // Explicit, live companion → its persona.
        let b_prompt = svc.build_system_prompt(Some(&b.companion_id), None).await.unwrap();
        assert!(b_prompt.contains("你是 墨墨"));
        // Dead explicit id → NO persona (no default fallback).
        assert!(svc.build_system_prompt(Some(MALFORMED_COMPANION_ID), None).await.is_none());
        let _ = a;
    }

    // ----- custom-figure library: in-use figures must not be deletable -----

    /// A real 7×5 lossless WebP (VP8L) — the same bytes the figures.rs/figure.rs
    /// tests use, so `create_figure`'s validator accepts it.
    fn webp_bytes() -> Vec<u8> {
        vec![
            0x52, 0x49, 0x46, 0x46, 0x1E, 0x00, 0x00, 0x00, 0x57, 0x45, 0x42, 0x50, 0x56, 0x50,
            0x38, 0x4C, 0x11, 0x00, 0x00, 0x00, 0x2F, 0x06, 0x00, 0x01, 0x00, 0x07, 0x50, 0x8A,
            0x2A, 0xD4, 0xA3, 0xFF, 0x81, 0x88, 0xE8, 0x7F, 0x00, 0x00,
        ]
    }

    /// A unique scratch dir under the upload sandbox root (`{temp}/nomifun`) —
    /// figure sources must canonicalize inside it (see
    /// [`crate::figure::validate_figure_source`]).
    fn upload_scratch() -> tempfile::TempDir {
        let root = std::env::temp_dir().join("nomifun");
        std::fs::create_dir_all(&root).unwrap();
        tempfile::Builder::new().prefix("companionsvc-fig-").tempdir_in(root).unwrap()
    }

    fn webp_source(upload: &tempfile::TempDir, name: &str) -> std::path::PathBuf {
        let p = upload.path().join(name);
        std::fs::write(&p, webp_bytes()).unwrap();
        p
    }

    /// Patch links a companion to a library figure via `appearance.custom_figure.figure_id`.
    fn link_patch(fig: &crate::figures::FigureMeta) -> serde_json::Value {
        serde_json::json!({
            "character": "custom",
            "appearance": {"custom_figure": {
                "aspect": fig.aspect,
                "head_box": {"x": fig.head_box.x, "y": fig.head_box.y, "w": fig.head_box.w, "h": fig.head_box.h},
                "size_tier": fig.size_tier,
                "figure_id": fig.figure_id,
            }},
        })
    }

    #[tokio::test]
    async fn update_figure_syncs_active_companion_custom_figure_metadata() {
        let dir = tempfile::tempdir().unwrap();
        let upload = upload_scratch();
        let svc = service(dir.path()).await;

        let fig = svc
            .create_figure(
                webp_source(&upload, "editable.webp").to_str().unwrap(),
                "旧形象",
                0.7,
                crate::profile::HeadBox { x: 0.3, y: 0.0, w: 0.4, h: 0.4 },
                "m",
            )
            .await
            .unwrap();
        let companion = svc.create_companion("可可", "custom").await.unwrap();
        svc.patch_companion(&companion.companion_id, link_patch(&fig)).await.unwrap();

        let next_head = crate::profile::HeadBox { x: 0.12, y: 0.18, w: 0.36, h: 0.42 };
        let updated = svc
            .update_figure(
                &fig.figure_id,
                crate::figures::FigureUpdate { name: Some("新形象".to_owned()), head_box: Some(next_head.clone()), size_tier: Some("l".to_owned()) },
            )
            .await
            .unwrap();

        assert_eq!(updated.name, "新形象");
        assert_eq!(updated.head_box, next_head);
        assert_eq!(updated.size_tier, "l");

        let synced = svc.get_companion(&companion.companion_id).await.unwrap();
        let custom = synced.appearance.custom_figure.unwrap();
        assert_eq!(custom.figure_id.as_deref(), Some(fig.figure_id.as_str()));
        assert_eq!(custom.aspect, fig.aspect);
        assert_eq!(custom.head_box, next_head);
        assert_eq!(custom.size_tier, "l");
    }

    #[tokio::test]
    async fn update_figure_preserves_per_companion_size_px_override() {
        // The 总览 size slider writes a per-companion `size_px` override onto the
        // companion's custom_figure. Editing the LIBRARY figure (head_box/tier)
        // fans out via sync_figure_to_active_companions, whose RFC 7396 patch never
        // mentions size_px — so the per-companion override must survive the sync.
        let dir = tempfile::tempdir().unwrap();
        let upload = upload_scratch();
        let svc = service(dir.path()).await;

        let fig = svc
            .create_figure(
                webp_source(&upload, "sized.webp").to_str().unwrap(),
                "旧形象",
                0.7,
                crate::profile::HeadBox { x: 0.3, y: 0.0, w: 0.4, h: 0.4 },
                "m",
            )
            .await
            .unwrap();
        let companion = svc.create_companion("可可", "custom").await.unwrap();
        svc.patch_companion(&companion.companion_id, link_patch(&fig)).await.unwrap();
        // Slider sets a per-companion override (merge-patch, like the UI does).
        svc.patch_companion(
            &companion.companion_id,
            serde_json::json!({"appearance": {"custom_figure": {"size_px": 333.0}}}),
        )
        .await
        .unwrap();

        // Editing the library figure's tier fans out to the companion.
        svc.update_figure(
            &fig.figure_id,
            crate::figures::FigureUpdate { name: None, head_box: None, size_tier: Some("l".to_owned()) },
        )
        .await
        .unwrap();

        let synced = svc.get_companion(&companion.companion_id).await.unwrap();
        let custom = synced.appearance.custom_figure.unwrap();
        assert_eq!(custom.size_tier, "l"); // library tier change applied
        assert_eq!(custom.size_px, Some(333.0)); // per-companion override preserved
    }

    #[tokio::test]
    async fn delete_figure_refuses_while_a_companion_uses_it() {
        let dir = tempfile::tempdir().unwrap();
        let upload = upload_scratch();
        let svc = service(dir.path()).await;

        let fig = svc
            .create_figure(
                webp_source(&upload, "a.webp").to_str().unwrap(),
                "阿狸",
                0.7,
                crate::profile::HeadBox { x: 0.3, y: 0.0, w: 0.4, h: 0.4 },
                "m",
            )
            .await
            .unwrap();
        let companion = svc.create_companion("毛球", "custom").await.unwrap();
        svc.patch_companion(&companion.companion_id, link_patch(&fig)).await.unwrap();

        // In use → delete is refused with Conflict, and the figure survives.
        assert!(matches!(svc.delete_figure(&fig.figure_id).await, Err(AppError::Conflict(_))));
        assert!(svc.list_figures().await.unwrap().iter().any(|f| f.figure_id == fig.figure_id));

        // Re-point the companion to a built-in character → figure is now unused → deletable.
        svc.patch_companion(&companion.companion_id, serde_json::json!({"character": "ink", "appearance": {"custom_figure": null}}))
            .await
            .unwrap();
        svc.delete_figure(&fig.figure_id).await.unwrap();
        assert!(svc.list_figures().await.unwrap().iter().all(|f| f.figure_id != fig.figure_id));
    }

    #[tokio::test]
    async fn delete_figure_allows_unused_and_after_only_user_is_deleted() {
        let dir = tempfile::tempdir().unwrap();
        let upload = upload_scratch();
        let svc = service(dir.path()).await;

        // An unused figure deletes straight away.
        let unused = svc
            .create_figure(
                webp_source(&upload, "u.webp").to_str().unwrap(),
                "未用",
                1.0,
                crate::profile::HeadBox { x: 0.3, y: 0.0, w: 0.4, h: 0.4 },
                "m",
            )
            .await
            .unwrap();
        svc.delete_figure(&unused.figure_id).await.unwrap();
        assert!(svc.list_figures().await.unwrap().is_empty());

        // A figure freed by deleting its only user becomes deletable.
        let fig = svc
            .create_figure(
                webp_source(&upload, "b.webp").to_str().unwrap(),
                "在用",
                0.7,
                crate::profile::HeadBox { x: 0.3, y: 0.0, w: 0.4, h: 0.4 },
                "m",
            )
            .await
            .unwrap();
        let companion = svc.create_companion("墨墨", "custom").await.unwrap();
        svc.patch_companion(&companion.companion_id, link_patch(&fig)).await.unwrap();
        assert!(matches!(svc.delete_figure(&fig.figure_id).await, Err(AppError::Conflict(_))));

        svc.delete_companion(&companion.companion_id).await.unwrap();
        svc.delete_figure(&fig.figure_id).await.unwrap();
        assert!(svc.list_figures().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn delete_figure_requires_explicitly_clearing_hidden_reference() {
        let dir = tempfile::tempdir().unwrap();
        let upload = upload_scratch();
        let svc = service(dir.path()).await;

        let fig = svc
            .create_figure(
                webp_source(&upload, "yx.webp").to_str().unwrap(),
                "云霄",
                0.56,
                crate::profile::HeadBox { x: 0.0, y: 0.0, w: 1.0, h: 1.0 },
                "l",
            )
            .await
            .unwrap();
        let companion = svc.create_companion("墨墨", "custom").await.unwrap();
        svc.patch_companion(&companion.companion_id, link_patch(&fig)).await.unwrap();
        // While the companion's character is `custom`, the figure is genuinely in use.
        assert!(matches!(svc.delete_figure(&fig.figure_id).await, Err(AppError::Conflict(_))));

        // Switching render mode alone does not erase the durable logical
        // reference, so deletion remains blocked.
        svc.patch_companion(&companion.companion_id, serde_json::json!({"character": "ink"}))
            .await
            .unwrap();
        assert!(matches!(svc.delete_figure(&fig.figure_id).await, Err(AppError::Conflict(_))));

        // Explicitly clear the binding, then deletion is safe.
        svc.patch_companion(
            &companion.companion_id,
            serde_json::json!({"appearance": {"custom_figure": null}}),
        )
        .await
        .unwrap();
        svc.delete_figure(&fig.figure_id).await.unwrap();
        assert!(svc.list_figures().await.unwrap().iter().all(|f| f.figure_id != fig.figure_id));
    }
}

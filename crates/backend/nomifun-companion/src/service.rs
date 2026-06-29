//! `CompanionService` bundles the shared config, the companion registry, the store,
//! collector stats, learner and the companion-thread manager into the single
//! object the routes layer talks to.

use std::path::PathBuf;
use std::sync::Arc;

use nomifun_common::AppError;
use serde::Serialize;
use tokio::sync::{Mutex, RwLock};

use crate::collector::{self, Collector, SharedConfig};
use crate::companion::CompanionThreads;
use crate::events::CompanionEventEmitter;
use crate::evolution::{EvolutionEngine, NoopTranscriptSource};
use crate::gamify::level_for_xp;
use crate::learner::{Learner, CompanionCompleter};
use crate::profile::{CompanionProfileConfig, SharedCompanionConfig};
use crate::registry::{CompanionRegistry, json_merge_patch};
use crate::skill_sink::CompanionSkillStoreSink;
use crate::store::{CompanionThread, MemoryFilter, MemoryScope, CompanionLearnRun, CompanionMemory, CompanionSkill, CompanionStore, CompanionSuggestion};
use nomifun_extension::skill_service::{self, SkillPaths, SkillScope};
use nomifun_extension::constants::SKILL_MANIFEST_FILE;

/// Map a stored `scope_companion_id` ("" = shared) to a [`SkillScope`].
fn scope_for(scope_companion_id: &str) -> SkillScope {
    if scope_companion_id.is_empty() {
        SkillScope::Shared
    } else {
        SkillScope::Companion(scope_companion_id.to_owned())
    }
}

/// A skill registry row + its SKILL.md `description` (frontmatter), flattened for the UI list.
#[derive(Debug, Clone, Serialize)]
pub struct CompanionSkillView {
    #[serde(flatten)]
    pub skill: CompanionSkill,
    pub description: String,
}

/// A skill registry row + its raw SKILL.md body, for the in-app editor.
#[derive(Debug, Clone, Serialize)]
pub struct CompanionSkillContent {
    pub skill: CompanionSkill,
    pub content: String,
}

#[derive(Debug, Serialize)]
pub struct CompanionStatus {
    /// Which companion this status describes (empty only on the no-companions fallback).
    pub companion_id: String,
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
    /// Delete-cascade hooks, late-wired by the app assembly (same pattern as
    /// `companion`). Empty when never set (tests).
    cleanup_hooks: std::sync::OnceLock<Vec<Arc<dyn CompanionCleanupHook>>>,
}

impl CompanionService {
    /// Construct the service: migrate any legacy single-companion layout, open the
    /// shared store, scan the companion roster, and spawn the collector + learner
    /// background tasks.
    pub async fn start(
        data_dir: &std::path::Path,
        bus: Arc<nomifun_realtime::BroadcastEventBus>,
        completer: Arc<dyn CompanionCompleter>,
        skill_paths: Arc<SkillPaths>,
    ) -> Result<Arc<Self>, AppError> {
        // Pre-rename installs keep their data under `{data}/pet`; move it to
        // `{data}/companion` before anything reads the new paths. Best-effort.
        crate::migrate::migrate_pet_dir_to_companion(data_dir);
        // Legacy companion/nomi → companion/shared + first companion. Idempotent; an io error
        // must never brick boot (the legacy data stays where it was).
        let first_companion_id = match crate::migrate::migrate_legacy_layout(data_dir) {
            Ok(id) => id,
            Err(e) => {
                tracing::warn!(error = %e, "legacy companion layout migration failed; continuing without it");
                None
            }
        };

        let shared_dir = data_dir.join(crate::COMPANION_SHARED_REL_DIR);
        let companions_dir = data_dir.join(crate::COMPANION_COMPANIONS_REL_DIR);
        let models_dir = data_dir.join(crate::COMPANION_MODELS_REL_DIR);
        let figures_dir = data_dir.join(crate::COMPANION_FIGURES_REL_DIR);
        let config: SharedConfig = Arc::new(RwLock::new(SharedCompanionConfig::load(&shared_dir)));
        let registry = Arc::new(CompanionRegistry::scan(companions_dir, shared_dir.clone()));
        // Number companions that predate the seq rollout (incl. a first companion the
        // legacy migration above just minted) before anything observes the
        // roster. Idempotent, so running it every boot is a standing retry.
        registry.backfill_missing_seqs().await;
        // A corrupt/locked memory.db must never brick backend boot — fall
        // back to an in-memory store (companion features degrade, app survives).
        let store = match CompanionStore::open(&shared_dir).await {
            Ok(store) => store,
            Err(e) => {
                tracing::error!(error = %e, "companion store open failed; falling back to in-memory store");
                CompanionStore::open_memory().await?
            }
        };
        // First-companion id: just minted by the migration above, or recorded in
        // the marker by an earlier boot. Re-reading the marker every boot
        // makes the (idempotent) backfill below a standing retry — a boot
        // where it failed (store unavailable, crash) gets healed by the next
        // one instead of stranding the legacy rows forever.
        let first_companion_id = first_companion_id.or_else(|| {
            std::fs::read_to_string(
                data_dir
                    .join(crate::COMPANION_REL_DIR)
                    .join(crate::migrate::MIGRATED_MARKER),
            )
            .ok()
            .map(|s| s.trim().to_owned())
            .filter(|s| !s.is_empty())
        });
        // Migration minted the first companion: claim legacy thread rows and
        // move the global xp/active-thread kv into its per-companion slots. On
        // failure the rows keep their empty companion_id and the runtime falls
        // back to the default companion for attribution.
        if let Some(companion_id) = &first_companion_id
            && let Err(e) = store.backfill_first_companion(companion_id).await
        {
            tracing::warn!(error = %e, companion_id, "first-companion backfill failed; legacy rows stay unattributed");
        }
        let emitter = CompanionEventEmitter::new(bus.clone());

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
            cleanup_hooks: std::sync::OnceLock::new(),
        }))
    }

    /// Late-wire the companion thread manager (depends on the conversation
    /// service, which is built after the companion service in app startup).
    pub fn attach_companion(
        &self,
        conversations: Arc<nomifun_conversation::ConversationService>,
        task_manager: Arc<dyn nomifun_ai_agent::IWorkerTaskManager>,
    ) {
        // Also wire the real transcript source so skill drafting rehydrates the actual
        // (redacted) session transcript from the conversation store — the durable single
        // source of truth — instead of degrading to tool-name steps.
        self.evolution.set_transcript(Arc::new(crate::evolution::ConversationTranscriptSource::new(
            conversations.conversation_repo().clone(),
        )));
        let _ = self.companion.set(CompanionThreads {
            store: self.store.clone(),
            config: self.config.clone(),
            registry: self.registry.clone(),
            conversations,
            task_manager,
        });
    }

    /// Late-wire the delete-cascade hooks (depends on services built after
    /// the companion service in app startup, e.g. `KnowledgeService`). First call
    /// wins; later calls are ignored (`OnceLock` semantics).
    pub fn set_cleanup_hooks(&self, hooks: Vec<Arc<dyn CompanionCleanupHook>>) {
        let _ = self.cleanup_hooks.set(hooks);
    }

    /// Build the `CompanionMemorySink` the agent factory needs — gives every
    /// companionSession conversation the recall/save/recent-events tools.
    pub fn memory_sink(&self) -> Arc<dyn nomifun_ai_agent::CompanionMemorySink> {
        Arc::new(crate::companion::CompanionStoreSink {
            store: self.store.clone(),
            config: self.config.clone(),
            emitter: self.emitter.clone(),
            companion_dir: self.shared_dir.clone(),
        })
    }

    /// Build the `CompanionSkillSink` the agent factory needs — gives companionSession
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

    /// Create a companion. The first companion ever created automatically becomes the
    /// default companion (shared config saved + broadcast).
    pub async fn create_companion(&self, name: &str, character: &str) -> Result<CompanionProfileConfig, AppError> {
        let profile = self.registry.create(name, character).await?;
        let updated_shared = {
            let mut cfg = self.config.write().await;
            if cfg.default_companion_id.is_empty() {
                cfg.default_companion_id = profile.id.clone();
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
        if profile.model.is_configured()
            && let Ok(companion) = self.companion()
            && let Err(e) = companion.create(&profile.id, None).await
        {
            tracing::warn!(error = %e, companion_id = %profile.id, "auto-create companion session failed; will be ensured lazily");
        }
        Ok(profile)
    }

    pub async fn get_companion(&self, id: &str) -> Result<CompanionProfileConfig, AppError> {
        self.registry
            .get(id)
            .await
            .ok_or_else(|| AppError::NotFound(format!("companion '{id}' not found")))
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
        let prev_model = prev.as_ref().map(|p| p.model.clone());
        let prev_name = prev.as_ref().map(|p| p.name.clone());
        let profile = self.registry.patch(id, patch).await?;
        self.emitter.emit_companion_updated(&profile.id, &profile);

        let model_changed = prev_model.as_ref() != Some(&profile.model);
        if model_changed && profile.model.is_configured() {
            self.propagate_model_to_companion(&profile).await;
            // 通知宿主：模型已切换（唯一事实源）。当前用于清理该伙伴绑定的
            // IM 渠道会话，使其下轮重建拾取新模型。best-effort，不阻断 patch。
            if let Some(hooks) = self.cleanup_hooks.get() {
                for hook in hooks {
                    hook.on_companion_model_changed(&profile.id).await;
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
        let threads = match companion.list(&profile.id).await {
            Ok(t) => t,
            Err(e) => {
                tracing::warn!(error = %e, companion_id = %profile.id, "list threads for workspace reconcile failed");
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
        let Ok(companion) = self.companion() else { return };
        // Idempotent ensure: returns the existing session, or mints one now
        // that the model is configured. This also yields the conversation id
        // to retarget.
        let conversation_id = match companion.create(&profile.id, None).await {
            Ok(thread) => thread.conversation_id,
            Err(e) => {
                tracing::warn!(error = %e, companion_id = %profile.id, "ensure companion session for model propagation failed");
                return;
            }
        };
        let model = nomifun_common::ProviderWithModel {
            provider_id: profile.model.provider_id.clone(),
            model: profile.model.model.clone(),
            use_model: Some(profile.model.model.clone()),
        };
        if let Err(e) = companion.set_model(&profile.id, &conversation_id, &model).await {
            tracing::warn!(error = %e, companion_id = %profile.id, "propagate model to companion conversation failed");
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
            if cfg.default_companion_id == id {
                cfg.default_companion_id = self
                    .registry
                    .list()
                    .await
                    .first()
                    .map(|p| p.id.clone())
                    .unwrap_or_default();
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

    /// One companion's status: per-companion xp/level, shared mood + memory/suggestion
    /// counters, that companion's companion model flag.
    pub async fn companion_status(&self, id: &str) -> Result<CompanionStatus, AppError> {
        let profile = self.get_companion(id).await?;
        let cfg = self.config.read().await.clone();
        let xp = self.store.get_companion_state_i64(id, "xp").await?;
        Ok(CompanionStatus {
            companion_id: profile.id,
            xp,
            level: level_for_xp(xp),
            mood: self.store.get_state("mood").await?.unwrap_or_else(|| "content".into()),
            memories_active: self.store.count_memories("active").await?,
            memories_archived: self.store.count_memories("archived").await?,
            suggestions_new: self.store.count_suggestions("new").await?,
            skills_active: self.store.count_active_skills(id).await?,
            model_configured: profile.model.is_configured(),
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

    /// The effective default companion: the shared pointer when it names a live
    /// companion, else the oldest companion, else `None` (no companions at all).
    pub async fn default_companion_id(&self) -> Option<String> {
        let configured = self.config.read().await.default_companion_id.clone();
        if !configured.is_empty() && self.registry.get(&configured).await.is_some() {
            return Some(configured);
        }
        self.registry.list().await.first().map(|p| p.id.clone())
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
    pub async fn list_figures(&self) -> Vec<crate::figures::FigureMeta> {
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
                        == Some(figure.id.as_str())
            })
            .collect();
        for profile in users {
            let patch = serde_json::json!({
                "appearance": {"custom_figure": {
                    "aspect": figure.aspect,
                    "head_box": {"x": figure.head_box.x, "y": figure.head_box.y, "w": figure.head_box.w, "h": figure.head_box.h},
                    "size_tier": figure.size_tier.clone(),
                    "figure_id": figure.id.clone(),
                }},
            });
            if let Err(e) = self.patch_companion(&profile.id, patch).await {
                tracing::warn!(error = %e, companion_id = %profile.id, figure_id = %figure.id, "sync updated library figure metadata to companion failed");
            }
        }
    }

    /// Number of companions **actively rendering** this library figure
    /// (`character == "custom"` AND `appearance.custom_figure.figure_id == figure_id`).
    /// The `character` gate is essential: switching a companion to a built-in
    /// character leaves a stale `custom_figure.figure_id` behind, and that orphan
    /// must NOT pin the figure as "in use" (it isn't rendered) — otherwise the
    /// figure becomes undeletable forever. Only the `custom` character renders it.
    async fn figure_user_count(&self, figure_id: &str) -> usize {
        self.registry
            .list()
            .await
            .iter()
            .filter(|p| {
                p.character == "custom"
                    && p.appearance
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
        let id = self.companion()?.active_thread_id(companion_id).await?;
        Ok(id.filter(|s| !s.is_empty()))
    }

    // ----- shared config -----

    pub async fn get_config(&self) -> SharedCompanionConfig {
        self.config.read().await.clone()
    }

    pub async fn update_config(&self, new_config: SharedCompanionConfig) -> Result<SharedCompanionConfig, AppError> {
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
        let merged = {
            let mut cfg = self.config.write().await;
            let mut value = serde_json::to_value(&*cfg)
                .map_err(|e| AppError::Internal(format!("serialize shared companion config: {e}")))?;
            json_merge_patch(&mut value, &patch);
            let merged: SharedCompanionConfig =
                serde_json::from_value(value).map_err(|e| AppError::BadRequest(format!("invalid config patch: {e}")))?;
            merged
                .save(&self.shared_dir)
                .map_err(|e| AppError::Internal(format!("save shared companion config: {e}")))?;
            *cfg = merged.clone();
            merged
        };
        self.emitter.emit_shared_config_updated(&merged);
        Ok(merged)
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
    pub fn recent_events(&self, limit: usize) -> Vec<collector::CollectedEvent> {
        collector::read_recent_events(&self.shared_dir, limit)
    }

    // ----- status -----

    /// Legacy aggregate status: the default companion's status. With no companions at
    /// all, a zeroed shared-only snapshot (xp 0 / level 1 / no model).
    pub async fn status(&self) -> Result<CompanionStatus, AppError> {
        if let Some(companion_id) = self.default_companion_id().await {
            return self.companion_status(&companion_id).await;
        }
        let cfg = self.config.read().await.clone();
        Ok(CompanionStatus {
            companion_id: String::new(),
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
        id: &str,
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
        self.store.update_memory(id, content, pinned, status, scope).await?;
        // Notify open surfaces with the post-edit row (best-effort; a missing
        // row already errored above).
        if let Ok(Some(updated)) = self.store.get_memory(id).await {
            self.emitter.emit_memory_updated(&updated);
        }
        Ok(())
    }

    pub async fn delete_memory(&self, id: &str) -> Result<(), AppError> {
        self.store.delete_memory(id).await?;
        self.emitter.emit_memory_deleted(id);
        Ok(())
    }

    // ----- suggestions -----

    pub async fn list_suggestions(&self, status: Option<&str>, limit: i64) -> Result<Vec<CompanionSuggestion>, AppError> {
        self.store.list_suggestions(status, limit).await
    }

    pub async fn decide_suggestion(&self, id: &str, accept: bool) -> Result<CompanionSuggestion, AppError> {
        let (decided, newly) = self.store.decide_suggestion(id, accept).await?;
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
                        tracing::warn!(error = %e, suggestion_id = %id, "failed to materialize accepted skill");
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
                    tracing::warn!(error = %e, suggestion_id = %id, "failed to record skill rejection");
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
        let name = action.get("name").and_then(|v| v.as_str()).unwrap_or("");
        let companion_id = action.get("companion_id").and_then(|v| v.as_str()).unwrap_or("");
        if name.is_empty() || companion_id.is_empty() {
            return Ok(()); // malformed action — nothing to materialize
        }
        // Delegate to the single idempotent skill-decide path (also used by the
        // Skills-tab review UI). draft→active promote + emit happen there.
        self.decide_companion_skill(companion_id, name, true, None).await.map(|_| ())
    }

    /// Rejecting a create_skill suggestion → delegate to the single idempotent skill-decide
    /// path (accept=false), which archives the draft + records signature feedback (纠偏回流).
    async fn reject_create_skill(&self, action: &serde_json::Value) -> Result<(), AppError> {
        let name = action.get("name").and_then(|v| v.as_str()).unwrap_or("");
        let companion_id = action.get("companion_id").and_then(|v| v.as_str()).unwrap_or("");
        if name.is_empty() || companion_id.is_empty() {
            return Ok(());
        }
        self.decide_companion_skill(companion_id, name, false, None).await.map(|_| ())
    }

    /// List a companion's skills for the UI (active/draft/archived). Each row gets its
    /// SKILL.md `description` read from disk (the store has no description column); a
    /// missing/unreadable file degrades to `description = ""` rather than failing the list.
    pub async fn list_companion_skills(
        &self,
        companion_id: &str,
        include_shared: bool,
    ) -> Result<Vec<CompanionSkillView>, AppError> {
        let skills = self.store.list_skills(companion_id, include_shared).await?;
        let mut out = Vec::with_capacity(skills.len());
        for skill in skills {
            let scope = scope_for(&skill.scope_companion_id);
            let draft = skill.status == "draft";
            let description = match skill_service::skill_dir_for(&self.skill_paths, &scope, &skill.skill_name, draft) {
                Ok(dir) => skill_service::read_skill_info(&dir).await.map(|(_, d)| d).unwrap_or_default(),
                Err(_) => String::new(),
            };
            out.push(CompanionSkillView { skill, description });
        }
        Ok(out)
    }

    /// Read one skill's registry row + raw SKILL.md body for the in-app editor.
    pub async fn get_companion_skill_content(
        &self,
        companion_id: &str,
        name: &str,
    ) -> Result<CompanionSkillContent, AppError> {
        let skill = self
            .store
            .get_skill(companion_id, name)
            .await?
            .ok_or_else(|| AppError::NotFound(format!("skill {name} not found")))?;
        let scope = scope_for(&skill.scope_companion_id);
        let draft = skill.status == "draft";
        let dir = skill_service::skill_dir_for(&self.skill_paths, &scope, name, draft)
            .map_err(|e| AppError::Internal(format!("resolve skill dir: {e}")))?;
        let content = tokio::fs::read_to_string(dir.join(SKILL_MANIFEST_FILE))
            .await
            .map_err(|e| AppError::NotFound(format!("read skill content: {e}")))?;
        Ok(CompanionSkillContent { skill, content })
    }

    /// Edit a skill's SKILL.md body in place. `content` must be a full valid SKILL.md
    /// (frontmatter + non-empty description) — `write_skill` rejects otherwise → BadRequest.
    pub async fn write_companion_skill_content(
        &self,
        companion_id: &str,
        name: &str,
        content: &str,
    ) -> Result<(), AppError> {
        let skill = self
            .store
            .get_skill(companion_id, name)
            .await?
            .ok_or_else(|| AppError::NotFound(format!("skill {name} not found")))?;
        let scope = scope_for(&skill.scope_companion_id);
        let draft = skill.status == "draft";
        skill_service::write_skill(&self.skill_paths, &scope, draft, name, content)
            .await
            .map_err(|e| AppError::BadRequest(format!("invalid skill content: {e}")))?;
        Ok(())
    }

    /// Review a draft skill. accept → promote draft SKILL.md to active + status active +
    /// emit skill-learned; reject → status archived + record reject feedback. IDEMPOTENT:
    /// a re-decide on a non-draft row is a no-op returning the row (the `newly`-gate analogue).
    pub async fn decide_companion_skill(
        &self,
        companion_id: &str,
        name: &str,
        accept: bool,
        reason: Option<&str>,
    ) -> Result<CompanionSkill, AppError> {
        let skill = self
            .store
            .get_skill(companion_id, name)
            .await?
            .ok_or_else(|| AppError::NotFound(format!("skill {name} not found")))?;
        if skill.status != "draft" {
            return Ok(skill); // idempotent: already decided
        }
        if accept {
            let scope = SkillScope::Companion(companion_id.to_owned());
            let draft_dir = skill_service::skill_dir_for(&self.skill_paths, &scope, name, true)
                .map_err(|e| AppError::Internal(format!("resolve draft skill dir: {e}")))?;
            let draft_md = tokio::fs::read_to_string(draft_dir.join(SKILL_MANIFEST_FILE))
                .await
                .map_err(|e| AppError::Internal(format!("read draft skill: {e}")))?;
            skill_service::write_skill(&self.skill_paths, &scope, false, name, &draft_md)
                .await
                .map_err(|e| AppError::Internal(format!("promote skill to active: {e}")))?;
            self.store.set_skill_status(companion_id, name, "active").await?;
            self.emitter.emit_skill_learned(companion_id, name);
        } else {
            self.store.set_skill_status(companion_id, name, "archived").await?;
            let fid = nomifun_common::generate_prefixed_id("evf");
            let sig = if skill.signature.is_empty() { None } else { Some(skill.signature.as_str()) };
            self.store
                .record_feedback(&fid, name, sig, "reject", reason, nomifun_common::now_ms())
                .await?;
            // Suppress the originating mined pattern from re-proposal (纠偏回流).
            if let Some(s) = sig {
                self.store.mark_pattern_status(s, "rejected").await?;
            }
        }
        Ok(self.store.get_skill(companion_id, name).await?.unwrap_or(skill))
    }

    /// Learn-by-demonstration (P5 T2-B): reconstruct a tool-name sequence from `conversation_id`'s
    /// collected tool-calls and draft a reviewable skill from it. Requires `collect.tool_calls` to
    /// have been on for that session. Returns the drafted skill name.
    pub async fn draft_skill_from_session(&self, companion_id: &str, conversation_id: &str) -> Result<Option<String>, AppError> {
        let events = crate::collector::read_recent_events(&self.shared_dir, 1000);
        let mut steps: Vec<String> = Vec::new();
        let mut call_ids: Vec<String> = Vec::new();
        let mut start_ts = i64::MAX;
        let mut end_ts = 0i64;
        for ev in events.iter() {
            if ev.source != "tool_calls" {
                continue;
            }
            let conv = ev.data.get("conversation_id");
            let matches = conv.and_then(|c| c.as_str()).map(|s| s == conversation_id).unwrap_or(false)
                || conv.and_then(|c| c.as_i64()).map(|n| n.to_string() == conversation_id).unwrap_or(false);
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
    pub async fn gift_companion_skill(&self, from_id: &str, name: &str, to_id: &str) -> Result<CompanionSkill, AppError> {
        if from_id == to_id {
            return Err(AppError::BadRequest("不能赠送给自己".into()));
        }
        let src = self
            .store
            .get_skill(from_id, name)
            .await?
            .ok_or_else(|| AppError::NotFound(format!("skill {name} not found")))?;
        if self.store.get_skill(to_id, name).await?.is_some() {
            return Err(AppError::BadRequest(format!("对方已有同名技能「{name}」")));
        }
        skill_service::copy_skill(
            &self.skill_paths,
            &SkillScope::Companion(from_id.to_owned()),
            &SkillScope::Companion(to_id.to_owned()),
            name,
        )
        .await
        .map_err(|e| AppError::Internal(format!("copy skill: {e}")))?;
        let now = nomifun_common::now_ms();
        let gifted = CompanionSkill {
            skill_name: name.to_owned(),
            scope_kind: "companion".into(),
            scope_companion_id: to_id.to_owned(),
            status: "active".into(),
            source: "gifted".into(),
            confidence: src.confidence,
            provenance: vec![],
            strength: 1.0,
            version: 1,
            superseded_by: None,
            usage_count: 0,
            last_used_at: None,
            created_at: now,
            updated_at: now,
            signature: String::new(),
        };
        self.store.insert_skill(&gifted).await?;
        self.emitter.emit_skill_learned(to_id, name);
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

    pub fn event_stats(&self) -> Vec<SourceStats> {
        collector::event_stats(&self.shared_dir)
            .into_iter()
            .map(|(source, (today, total))| SourceStats { source, today, total })
            .collect()
    }

    pub fn clear_events(&self) -> Result<(), AppError> {
        collector::clear_events(&self.shared_dir).map_err(|e| AppError::Internal(format!("clear companion events: {e}")))
    }
}

/// The factory-facing persona prompt provider: channel master-agent sessions
/// carry `companionSession` but no persisted `system_prompt`, so the nomi factory
/// asks the bound companion for a fresh persona (with current memory snapshot) at
/// every agent build. The persona is built **only** for an explicitly-bound, live
/// companion; `companion_id: None` or a dead id yields no persona — an unbound
/// channel is hosted by no companion (no default-companion fallback; 历史债
/// 「渠道与远程连接默认由默认伙伴接待」已废除，连接由用户为每个伙伴显式配置).
#[async_trait::async_trait]
impl nomifun_ai_agent::CompanionPromptProvider for CompanionService {
    async fn build_system_prompt(&self, companion_id: Option<&str>, channel_platform: Option<&str>) -> Option<String> {
        let profile = self.registry.get(companion_id.map(str::trim).filter(|id| !id.is_empty())?).await?;
        Some(crate::companion::build_companion_system_prompt(&self.store, &profile, channel_platform).await)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nomifun_realtime::BroadcastEventBus;

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
            Arc::new(NoopCompleter),
            Arc::new(nomifun_extension::skill_service::resolve_skill_paths(data_dir, data_dir)),
        )
        .await
        .unwrap()
    }

    #[tokio::test]
    async fn accepting_create_skill_promotes_draft_and_is_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let svc = service(dir.path()).await;
        let companion = svc.registry.create("测试", "ink").await.unwrap();
        let cid = companion.id;

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
                skill_name: "demo".into(),
                scope_kind: "companion".into(),
                scope_companion_id: cid.clone(),
                status: "draft".into(),
                source: "mined".into(),
                confidence: 0.9,
                provenance: vec![],
                strength: 1.0,
                version: 1,
                superseded_by: None,
                usage_count: 0,
                last_used_at: None,
                created_at: now,
                updated_at: now,
                signature: String::new(),
            })
            .await
            .unwrap();
        let action = serde_json::json!({"type": "create_skill", "name": "demo", "companion_id": cid});
        let sug = svc.store.insert_suggestion("create_skill", "学会 demo", "body", Some(&action)).await.unwrap();

        // Accept → promote draft to active.
        svc.decide_suggestion(&sug.id, true).await.unwrap();
        let active_md = svc.skill_paths.user_skills_dir.join("companion").join(&cid).join("demo").join("SKILL.md");
        assert!(active_md.exists(), "active SKILL.md missing at {}", active_md.display());
        assert_eq!(svc.store.get_skill(&cid, "demo").await.unwrap().unwrap().status, "active");
        let xp1 = svc.store.get_companion_state_i64(&cid, "xp").await.unwrap();
        assert!(xp1 >= 20, "accept should grant shared XP");

        // Re-accept → idempotent: no re-award, status unchanged.
        svc.decide_suggestion(&sug.id, true).await.unwrap();
        let xp2 = svc.store.get_companion_state_i64(&cid, "xp").await.unwrap();
        assert_eq!(xp1, xp2, "re-accept must not re-award xp");
        assert_eq!(svc.store.get_skill(&cid, "demo").await.unwrap().unwrap().status, "active");
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
                skill_name: name.into(),
                scope_kind: "companion".into(),
                scope_companion_id: cid.to_owned(),
                status: "draft".into(),
                source: "mined".into(),
                confidence: 0.5,
                provenance: vec![],
                strength: 1.0,
                version: 1,
                superseded_by: None,
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
    async fn list_companion_skills_has_description_and_degrades_on_missing_file() {
        let dir = tempfile::tempdir().unwrap();
        let svc = service(dir.path()).await;
        let cid = svc.registry.create("测试", "ink").await.unwrap().id;
        seed_draft_skill(&svc, &cid, "alpha").await; // SKILL.md on disk
        // beta: registry row only, NO SKILL.md → description "" (don't fail list)
        let now = nomifun_common::now_ms();
        svc.store
            .insert_skill(&CompanionSkill {
                skill_name: "beta".into(),
                scope_kind: "companion".into(),
                scope_companion_id: cid.clone(),
                status: "draft".into(),
                source: "mined".into(),
                confidence: 0.5,
                provenance: vec![],
                strength: 1.0,
                version: 1,
                superseded_by: None,
                usage_count: 0,
                last_used_at: None,
                created_at: now,
                updated_at: now,
                signature: String::new(),
            })
            .await
            .unwrap();
        let views = svc.list_companion_skills(&cid, false).await.unwrap();
        assert_eq!(views.iter().find(|v| v.skill.skill_name == "alpha").unwrap().description, "原始描述");
        assert_eq!(views.iter().find(|v| v.skill.skill_name == "beta").unwrap().description, "");
    }

    #[tokio::test]
    async fn get_and_write_skill_content_roundtrip_and_validate() {
        let dir = tempfile::tempdir().unwrap();
        let svc = service(dir.path()).await;
        let cid = svc.registry.create("测试", "ink").await.unwrap().id;
        seed_draft_skill(&svc, &cid, "demo").await;

        assert!(svc.get_companion_skill_content(&cid, "demo").await.unwrap().content.contains("原始描述"));
        // edit with a valid full SKILL.md
        let new_md = "---\nname: demo\ndescription: 改后描述\n---\n\n新步骤\n";
        svc.write_companion_skill_content(&cid, "demo", new_md).await.unwrap();
        assert!(svc.get_companion_skill_content(&cid, "demo").await.unwrap().content.contains("改后描述"));
        // empty description → BadRequest; missing skill → NotFound
        assert!(svc.write_companion_skill_content(&cid, "demo", "---\nname: demo\ndescription:\n---\nx").await.is_err());
        assert!(svc.get_companion_skill_content(&cid, "nope").await.is_err());
    }

    #[tokio::test]
    async fn decide_companion_skill_accept_reject_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let svc = service(dir.path()).await;
        let cid = svc.registry.create("测试", "ink").await.unwrap().id;

        seed_draft_skill(&svc, &cid, "acc").await;
        let r = svc.decide_companion_skill(&cid, "acc", true, None).await.unwrap();
        assert_eq!(r.status, "active");
        assert!(svc.skill_paths.user_skills_dir.join("companion").join(&cid).join("acc").join("SKILL.md").exists());
        // re-accept on a non-draft row is an idempotent no-op
        assert_eq!(svc.decide_companion_skill(&cid, "acc", true, None).await.unwrap().status, "active");

        seed_draft_skill(&svc, &cid, "rej").await;
        let r3 = svc.decide_companion_skill(&cid, "rej", false, Some("太窄")).await.unwrap();
        assert_eq!(r3.status, "archived");
    }

    #[tokio::test]
    async fn rejecting_skill_suppresses_its_originating_pattern() {
        let dir = tempfile::tempdir().unwrap();
        let svc = service(dir.path()).await;
        let cid = svc.registry.create("测试", "ink").await.unwrap().id;
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
                skill_name: "rej-sig".into(),
                scope_kind: "companion".into(),
                scope_companion_id: cid.clone(),
                status: "draft".into(),
                source: "mined".into(),
                confidence: 0.5,
                provenance: vec![],
                strength: 1.0,
                version: 1,
                superseded_by: None,
                usage_count: 0,
                last_used_at: None,
                created_at: now,
                updated_at: now,
                signature: "sig-XYZ".into(),
            })
            .await
            .unwrap();
        assert!(!svc.store.is_signature_rejected("sig-XYZ").await.unwrap());
        svc.decide_companion_skill(&cid, "rej-sig", false, Some("不通用")).await.unwrap();
        assert!(
            svc.store.is_signature_rejected("sig-XYZ").await.unwrap(),
            "rejecting a skill must suppress its originating mined pattern"
        );
    }

    #[tokio::test]
    async fn weekly_digest_counts_recent_skills() {
        let dir = tempfile::tempdir().unwrap();
        let svc = service(dir.path()).await;
        let cid = svc.registry.create("测试", "ink").await.unwrap().id;
        let now = nomifun_common::now_ms();
        svc.store
            .insert_skill(&CompanionSkill {
                skill_name: "recent".into(),
                scope_kind: "companion".into(),
                scope_companion_id: cid.clone(),
                status: "active".into(),
                source: "mined".into(),
                confidence: 0.5,
                provenance: vec![],
                strength: 1.0,
                version: 1,
                superseded_by: None,
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
        let cid = svc.registry.create("测试", "ink").await.unwrap().id;
        // No collected tool-call activity for this conversation → BadRequest, not a panic.
        assert!(svc.draft_skill_from_session(&cid, "conv-none").await.is_err());
    }

    #[tokio::test]
    async fn gift_copies_skill_to_recipient_and_guards() {
        let dir = tempfile::tempdir().unwrap();
        let svc = service(dir.path()).await;
        let a = svc.registry.create("A", "ink").await.unwrap().id;
        let b = svc.registry.create("B", "ink").await.unwrap().id;
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
                skill_name: "share-me".into(),
                scope_kind: "companion".into(),
                scope_companion_id: a.clone(),
                status: "active".into(),
                source: "mined".into(),
                confidence: 0.7,
                provenance: vec![],
                strength: 1.0,
                version: 1,
                superseded_by: None,
                usage_count: 0,
                last_used_at: None,
                created_at: now,
                updated_at: now,
                signature: String::new(),
            })
            .await
            .unwrap();
        assert!(svc.gift_companion_skill(&a, "share-me", &a).await.is_err(), "self-gift rejected");
        let g = svc.gift_companion_skill(&a, "share-me", &b).await.unwrap();
        assert_eq!(g.scope_companion_id, b);
        assert_eq!(g.source, "gifted");
        assert_eq!(svc.store.get_skill(&b, "share-me").await.unwrap().unwrap().status, "active");
        assert!(svc.skill_paths.user_skills_dir.join("companion").join(&b).join("share-me").join("SKILL.md").exists());
        assert!(svc.gift_companion_skill(&a, "share-me", &b).await.is_err(), "name collision rejected");
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
                    ts: base + i,
                    source: "tool_calls".into(),
                    name: "tool.call".into(),
                    data: serde_json::json!({ "name": format!("t{i}") }),
                },
            )
            .unwrap();
        }
        assert_eq!(svc.recent_events(3).len(), 3);
        assert_eq!(svc.recent_events(100).len(), 5);
    }

    #[tokio::test]
    async fn disable_all_turns_everything_off_but_keeps_models() {
        let dir = tempfile::tempdir().unwrap();
        let svc = service(dir.path()).await;
        svc.patch_config(serde_json::json!({
            "collect": { "tool_calls": true, "chat_user_messages": true, "companion_dialogues": true },
            "learn": { "enabled": true, "interval_minutes": 30, "model": { "provider_id": "p", "model": "m" } },
            "evolve": { "enabled": true, "model": { "provider_id": "p", "model": "m" } }
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
        assert_eq!(cfg.learn.model.provider_id, "p");
        assert_eq!(cfg.learn.interval_minutes, 30);
        assert_eq!(cfg.evolve.model.provider_id, "p");
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
    async fn start_backfills_pre_seq_profiles() {
        let dir = tempfile::tempdir().unwrap();
        // A pre-rollout install: profiles already on disk, none numbered.
        let companions_dir = dir.path().join(crate::COMPANION_COMPANIONS_REL_DIR);
        let mut older = CompanionProfileConfig::new("老宠", "ink");
        older.created_at = 1_000;
        older.save(&companions_dir.join(&older.id)).unwrap();
        let mut newer = CompanionProfileConfig::new("新宠", "boo");
        newer.created_at = 2_000;
        newer.save(&companions_dir.join(&newer.id)).unwrap();

        let svc = service(dir.path()).await;

        // Boot numbered them oldest-first and persisted everything.
        let companions = svc.list_companions().await;
        assert_eq!(companions.iter().map(|p| p.seq).collect::<Vec<_>>(), vec![Some(1), Some(2)]);
        assert_eq!(CompanionProfileConfig::load(&companions_dir.join(&older.id)).seq, Some(1));
        let shared_dir = dir.path().join(crate::COMPANION_SHARED_REL_DIR);
        assert_eq!(crate::registry::CompanionSeqState::load(&shared_dir).last_companion_seq, 2);

        // New creations continue the numbering.
        let third = svc.create_companion("三号", "mochi").await.unwrap();
        assert_eq!(third.seq, Some(3));
    }

    #[tokio::test]
    async fn config_writes_cannot_reset_seq_watermark() {
        let dir = tempfile::tempdir().unwrap();
        let svc = service(dir.path()).await;
        let a = svc.create_companion("甲", "ink").await.unwrap();
        let b = svc.create_companion("乙", "boo").await.unwrap();
        assert_eq!(b.seq, Some(2));
        svc.delete_companion(&b.id).await.unwrap();

        // A full-object PUT of the shared config (the route body simply has
        // no watermark field — it lives in the registry's own state file)…
        let put = SharedCompanionConfig {
            default_companion_id: a.id.clone(),
            ..SharedCompanionConfig::default()
        };
        svc.update_config(put).await.unwrap();
        // …and a merge patch.
        svc.patch_config(serde_json::json!({"learn": {"enabled": true}}))
            .await
            .unwrap();

        // Neither write path can hand out the deleted companion's number again.
        let c = svc.create_companion("丙", "mochi").await.unwrap();
        assert_eq!(c.seq, Some(3));

        // The watermark file is independent of the shared config file, which
        // carries no watermark field at all.
        let shared_dir = dir.path().join(crate::COMPANION_SHARED_REL_DIR);
        assert_eq!(crate::registry::CompanionSeqState::load(&shared_dir).last_companion_seq, 3);
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
        assert_eq!(svc.get_config().await.default_companion_id, first.id);
        assert_eq!(svc.default_companion_id().await.as_deref(), Some(first.id.as_str()));
        // Persisted, not just in memory.
        let on_disk = SharedCompanionConfig::load(&dir.path().join(crate::COMPANION_SHARED_REL_DIR));
        assert_eq!(on_disk.default_companion_id, first.id);

        // A second companion never steals the default.
        let second = svc.create_companion("墨墨", "boo").await.unwrap();
        assert_eq!(svc.get_config().await.default_companion_id, first.id);
        assert_ne!(first.id, second.id);
        assert_eq!(svc.list_companions().await.len(), 2);
    }

    #[tokio::test]
    async fn delete_companion_repoints_default_and_clears_rows() {
        let dir = tempfile::tempdir().unwrap();
        let svc = service(dir.path()).await;
        let a = svc.create_companion("甲", "ink").await.unwrap();
        let b = svc.create_companion("乙", "boo").await.unwrap();
        assert_eq!(svc.get_config().await.default_companion_id, a.id);

        // Give A per-companion rows that must vanish with it.
        svc.store.add_companion_xp(&a.id, 42).await.unwrap();
        svc.store.insert_companion_thread("conv_a", &a.id, "甲聊").await.unwrap();
        svc.store.add_companion_xp(&b.id, 7).await.unwrap();

        svc.delete_companion(&a.id).await.unwrap();

        assert!(matches!(svc.get_companion(&a.id).await, Err(AppError::NotFound(_))));
        assert_eq!(svc.store.get_companion_state_i64(&a.id, "xp").await.unwrap(), 0);
        assert!(!svc.store.is_companion_thread("conv_a").await.unwrap());
        // Default re-pointed to the survivor; survivor untouched.
        assert_eq!(svc.get_config().await.default_companion_id, b.id);
        assert_eq!(svc.store.get_companion_state_i64(&b.id, "xp").await.unwrap(), 7);

        // Deleting the last companion clears the default entirely.
        svc.delete_companion(&b.id).await.unwrap();
        assert_eq!(svc.get_config().await.default_companion_id, "");
        assert_eq!(svc.default_companion_id().await, None);

        assert!(matches!(svc.delete_companion(&a.id).await, Err(AppError::NotFound(_))));
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
        svc.delete_companion(&p.id).await.unwrap();
        assert_eq!(hook.0.lock().unwrap().as_slice(), &[p.id.clone()]);

        // A failed delete (unknown id) must not fire the hooks again.
        assert!(matches!(svc.delete_companion(&p.id).await, Err(AppError::NotFound(_))));
        assert_eq!(hook.0.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn start_retries_backfill_from_marker() {
        let dir = tempfile::tempdir().unwrap();
        // A previous boot migrated (marker written) but its backfill failed:
        // the store still holds unclaimed legacy rows and the global kv.
        let legacy = dir.path().join(crate::COMPANION_REL_DIR);
        std::fs::create_dir_all(&legacy).unwrap();
        std::fs::write(legacy.join(crate::migrate::MIGRATED_MARKER), "companion_first").unwrap();
        {
            let store = CompanionStore::open(&dir.path().join(crate::COMPANION_SHARED_REL_DIR)).await.unwrap();
            store.insert_companion_thread("conv_l", "", "旧线程").await.unwrap();
            store.set_state("xp", "55").await.unwrap();
        }

        // Boot does not re-run the migration (marker gate), but it re-reads
        // the marker and retries the idempotent backfill.
        let svc = service(dir.path()).await;

        assert_eq!(svc.store.get_companion_state_i64("companion_first", "xp").await.unwrap(), 55);
        let threads = svc.store.list_companion_threads(Some("companion_first")).await.unwrap();
        assert_eq!(threads.len(), 1);
        assert_eq!(threads[0].conversation_id, "conv_l");
        // The global kv was consumed by the move.
        assert_eq!(svc.store.get_state("xp").await.unwrap(), None);
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
        let decided = svc.decide_suggestion(&s.id, true).await.unwrap();
        assert_eq!(decided.status, "accepted");
        assert_eq!(svc.store.get_companion_state_i64(&a.id, "xp").await.unwrap(), 20);
        assert_eq!(svc.store.get_companion_state_i64(&b.id, "xp").await.unwrap(), 20);
        // Legacy global xp stays untouched.
        assert_eq!(svc.store.get_state_i64("xp").await.unwrap(), 0);

        // Idempotent: re-accepting the same suggestion (stale card / double
        // click / cross-surface repeat) must NOT re-award xp.
        let again = svc.decide_suggestion(&s.id, true).await.unwrap();
        assert_eq!(again.status, "accepted");
        assert_eq!(svc.store.get_companion_state_i64(&a.id, "xp").await.unwrap(), 20);
        assert_eq!(svc.store.get_companion_state_i64(&b.id, "xp").await.unwrap(), 20);

        // Dismissals award nothing.
        let s2 = svc
            .store
            .insert_suggestion("insight", "再来", "不要", None)
            .await
            .unwrap();
        svc.decide_suggestion(&s2.id, false).await.unwrap();
        assert_eq!(svc.store.get_companion_state_i64(&a.id, "xp").await.unwrap(), 20);
    }

    #[tokio::test]
    async fn status_uses_default_companion_and_survives_no_companions() {
        let dir = tempfile::tempdir().unwrap();
        let svc = service(dir.path()).await;

        // No companions yet: zeroed fallback.
        let empty = svc.status().await.unwrap();
        assert_eq!(empty.companion_id, "");
        assert_eq!(empty.xp, 0);
        assert_eq!(empty.level, 1);
        assert!(!empty.model_configured);

        let a = svc.create_companion("甲", "ink").await.unwrap();
        svc.store.add_companion_xp(&a.id, 150).await.unwrap();
        let status = svc.status().await.unwrap();
        assert_eq!(status.companion_id, a.id);
        assert_eq!(status.xp, 150);
        assert_eq!(status.level, 2);

        // Per-companion status for a second companion reads its own xp.
        let b = svc.create_companion("乙", "boo").await.unwrap();
        let sb = svc.companion_status(&b.id).await.unwrap();
        assert_eq!(sb.companion_id, b.id);
        assert_eq!(sb.xp, 0);
        assert!(matches!(svc.companion_status("companion_missing").await, Err(AppError::NotFound(_))));
    }

    #[tokio::test]
    async fn default_companion_id_falls_back_to_oldest_when_pointer_is_dead() {
        let dir = tempfile::tempdir().unwrap();
        let svc = service(dir.path()).await;
        let a = svc.create_companion("甲", "ink").await.unwrap();
        let _b = svc.create_companion("乙", "boo").await.unwrap();

        // Point the shared config at a companion that does not exist.
        svc.patch_config(serde_json::json!({"default_companion_id": "companion_ghost"}))
            .await
            .unwrap();
        assert_eq!(svc.default_companion_id().await.as_deref(), Some(a.id.as_str()));
    }

    #[tokio::test]
    async fn patch_companion_emits_and_persists() {
        let dir = tempfile::tempdir().unwrap();
        let svc = service(dir.path()).await;
        let a = svc.create_companion("甲", "ink").await.unwrap();
        let patched = svc
            .patch_companion(&a.id, serde_json::json!({"name": "新名", "appearance": {"companion_enabled": true}}))
            .await
            .unwrap();
        assert_eq!(patched.name, "新名");
        assert!(patched.appearance.companion_enabled);
        assert_eq!(svc.get_companion(&a.id).await.unwrap().name, "新名");
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
        assert!(!a.model.is_configured());

        let patched = svc
            .patch_companion(&a.id, serde_json::json!({"model": {"provider_id": "prov_x", "model": "claude-fable-5"}}))
            .await
            .unwrap();
        assert!(patched.model.is_configured());
        assert_eq!(patched.model.provider_id, "prov_x");
        assert_eq!(svc.get_companion(&a.id).await.unwrap().model.model, "claude-fable-5");

        // A non-model patch on an already-configured companion also succeeds (no
        // spurious propagation path, model unchanged).
        let renamed = svc.patch_companion(&a.id, serde_json::json!({"name": "甲改"})).await.unwrap();
        assert_eq!(renamed.name, "甲改");
        assert_eq!(renamed.model.provider_id, "prov_x");
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
        assert_eq!(again.id, first.id);
        assert_eq!(svc.store.count_memories("active").await.unwrap(), 1);
        assert!(again.strength > first.strength, "dedup hit must reinforce the existing memory");

        // Genuinely different content still inserts.
        let other = svc.add_memory("preference", "主人喜欢浅色代码字体", &[], MemoryScope::Shared).await.unwrap();
        assert_ne!(other.id, first.id);
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
        let b_prompt = svc.build_system_prompt(Some(&b.id), None).await.unwrap();
        assert!(b_prompt.contains("你是 墨墨"));
        // Dead explicit id → NO persona (no default fallback).
        assert!(svc.build_system_prompt(Some("companion_ghost"), None).await.is_none());
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
                "figure_id": fig.id,
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
        svc.patch_companion(&companion.id, link_patch(&fig)).await.unwrap();

        let next_head = crate::profile::HeadBox { x: 0.12, y: 0.18, w: 0.36, h: 0.42 };
        let updated = svc
            .update_figure(
                &fig.id,
                crate::figures::FigureUpdate { name: Some("新形象".to_owned()), head_box: Some(next_head.clone()), size_tier: Some("l".to_owned()) },
            )
            .await
            .unwrap();

        assert_eq!(updated.name, "新形象");
        assert_eq!(updated.head_box, next_head);
        assert_eq!(updated.size_tier, "l");

        let synced = svc.get_companion(&companion.id).await.unwrap();
        let custom = synced.appearance.custom_figure.unwrap();
        assert_eq!(custom.figure_id.as_deref(), Some(fig.id.as_str()));
        assert_eq!(custom.aspect, fig.aspect);
        assert_eq!(custom.head_box, next_head);
        assert_eq!(custom.size_tier, "l");
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
        svc.patch_companion(&companion.id, link_patch(&fig)).await.unwrap();

        // In use → delete is refused with Conflict, and the figure survives.
        assert!(matches!(svc.delete_figure(&fig.id).await, Err(AppError::Conflict(_))));
        assert!(svc.list_figures().await.iter().any(|f| f.id == fig.id));

        // Re-point the companion to a built-in character → figure is now unused → deletable.
        svc.patch_companion(&companion.id, serde_json::json!({"character": "ink", "appearance": {"custom_figure": null}}))
            .await
            .unwrap();
        svc.delete_figure(&fig.id).await.unwrap();
        assert!(svc.list_figures().await.iter().all(|f| f.id != fig.id));
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
        svc.delete_figure(&unused.id).await.unwrap();
        assert!(svc.list_figures().await.is_empty());

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
        svc.patch_companion(&companion.id, link_patch(&fig)).await.unwrap();
        assert!(matches!(svc.delete_figure(&fig.id).await, Err(AppError::Conflict(_))));

        svc.delete_companion(&companion.id).await.unwrap();
        svc.delete_figure(&fig.id).await.unwrap();
        assert!(svc.list_figures().await.is_empty());
    }

    #[tokio::test]
    async fn delete_figure_ignores_orphaned_link_after_switch_to_builtin_character() {
        // Regression: switching a companion from a custom figure to a BUILT-IN
        // character left `custom_figure.figure_id` behind (UI patched only
        // `character`). That orphan must not pin the figure as "in use" — it is
        // no longer rendered — or the library figure becomes undeletable forever.
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
        svc.patch_companion(&companion.id, link_patch(&fig)).await.unwrap();
        // While the companion's character is `custom`, the figure is genuinely in use.
        assert!(matches!(svc.delete_figure(&fig.id).await, Err(AppError::Conflict(_))));

        // Switch to a built-in character WITHOUT clearing custom_figure (the bug
        // that orphaned the link). The figure is no longer rendered → deletable.
        svc.patch_companion(&companion.id, serde_json::json!({"character": "ink"}))
            .await
            .unwrap();
        svc.delete_figure(&fig.id).await.unwrap();
        assert!(svc.list_figures().await.iter().all(|f| f.id != fig.id));
    }
}

//! The scheduled learning loop: every tick, if enabled and due, read new
//! collected events, run one LLM distillation call, and apply the output
//! (memories / reinforcement / supersedes / suggestions / mood / diary).

use std::path::PathBuf;
use std::sync::Arc;

use nomifun_ai_agent::nomi_config;
use nomifun_ai_agent::{one_shot_completion, resolve_provider_config, user_message};
use nomifun_common::{AppError, CompanionLearnRunId, now_ms};
use nomifun_db::IProviderRepository;
use tokio::sync::Mutex;

use crate::collector::{SharedConfig, read_events_since};
use crate::events::CompanionEventEmitter;
use crate::prompt::{self, LEARN_MAX_TOKENS};
use crate::registry::CompanionRegistry;
use crate::store::{MemoryFilter, CompanionLearnRun, CompanionStore};

const MAX_EVENTS_PER_RUN: usize = 300;
const TICK_SECONDS: u64 = 60;
/// After this many consecutive scheduled runs fail to parse, the batch is
/// abandoned (cursor advanced) instead of re-burning tokens forever.
const PARSE_FAIL_GIVE_UP_RUNS: i64 = 3;

/// LLM seam so tests can run the learner without a live provider.
/// (Companion chat runs on the real agent engine; this trait only serves
/// the scheduled learning distillation calls.)
#[async_trait::async_trait]
pub trait CompanionCompleter: Send + Sync {
    async fn complete(&self, provider_id: &str, model: &str, system: &str, user: &str, max_tokens: u32)
    -> Result<String, AppError>;
}

/// Production completer: provider row → nomi Config → one-shot completion.
pub struct LiveCompanionCompleter {
    pub provider_repo: Arc<dyn IProviderRepository>,
    pub encryption_key: [u8; 32],
    pub workspace: PathBuf,
}

impl LiveCompanionCompleter {
    async fn resolve(&self, provider_id: &str, model: &str) -> Result<nomi_config::config::Config, AppError> {
        resolve_provider_config(
            &self.provider_repo,
            &self.encryption_key,
            provider_id,
            model,
            &self.workspace,
        )
        .await
    }
}

#[async_trait::async_trait]
impl CompanionCompleter for LiveCompanionCompleter {
    async fn complete(
        &self,
        provider_id: &str,
        model: &str,
        system: &str,
        user: &str,
        max_tokens: u32,
    ) -> Result<String, AppError> {
        let cfg = self.resolve(provider_id, model).await?;
        one_shot_completion(&cfg, system, vec![user_message(user)], max_tokens).await
    }
}

pub struct Learner {
    pub companion_dir: PathBuf,
    pub config: SharedConfig,
    pub store: CompanionStore,
    /// Companion roster: learn-run XP is a shared achievement granted to every companion.
    pub registry: Arc<CompanionRegistry>,
    pub completer: Arc<dyn CompanionCompleter>,
    pub emitter: CompanionEventEmitter,
    /// Re-entrancy guard shared between the tick loop and "run now".
    pub run_lock: Arc<Mutex<()>>,
}

impl Learner {
    /// Spawn the periodic tick loop.
    pub fn spawn(self: Arc<Self>) {
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(std::time::Duration::from_secs(TICK_SECONDS));
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                interval.tick().await;
                let (enabled, interval_minutes) = {
                    let cfg = self.config.read().await;
                    (cfg.learn.enabled, cfg.learn.interval_minutes.max(5) as i64)
                };
                if !enabled {
                    continue;
                }
                let last_run = self.store.get_state_i64("last_learn_ts").await.unwrap_or(0);
                if now_ms() - last_run < interval_minutes * 60_000 {
                    continue;
                }
                if let Err(e) = self.run_once().await {
                    tracing::warn!(error = %e, "companion scheduled learn run failed");
                }
            }
        });
    }

    /// One learning run. Returns the persisted run record.
    pub async fn run_once(&self) -> Result<CompanionLearnRun, AppError> {
        let Ok(_guard) = self.run_lock.try_lock() else {
            return Err(AppError::Conflict("a learn run is already in progress".into()));
        };
        let started_at = now_ms();
        // Stamp first so a crashed/failed run doesn't hot-loop the scheduler.
        self.store.set_state("last_learn_ts", &started_at.to_string()).await?;

        let model = { self.config.read().await.learn.model.clone() };
        let mut run = CompanionLearnRun {
            learn_run_id: CompanionLearnRunId::new().into_string(),
            started_at,
            finished_at: None,
            status: "ok".into(),
            events_processed: 0,
            memories_added: 0,
            suggestions_added: 0,
            error: None,
            summary: None,
        };

        let Some(model) = model else {
            run.status = "model_unconfigured".into();
            run.finished_at = Some(now_ms());
            self.store.insert_learn_run(&run).await?;
            return Ok(run);
        };

        let cursor = self.store.get_state_i64("learn_cursor_ts").await?;
        let (events, truncated) =
            read_events_since(&self.companion_dir, cursor, MAX_EVENTS_PER_RUN)?;
        if events.is_empty() {
            run.status = "no_events".into();
            run.finished_at = Some(now_ms());
            self.store.insert_learn_run(&run).await?;
            return Ok(run);
        }
        run.events_processed = events.len() as i64;
        let new_cursor = events.last().map(|e| e.ts).unwrap_or(cursor);

        // 选项A：共享学习产出只由「默认体」窗口呈现，避免 N 个伙伴窗口同时弹气泡（提示风暴）。
        let target = {
            let did = { self.config.read().await.default_companion_id.clone() };
            self.registry.resolve_default(did.as_deref()).await
        };

        if let Some(target) = target.as_deref() {
            self.emitter.emit_learn_started(target);
        }

        // Existing-memory digest for reinforcement/conflict matching, plus
        // the pending suggestions so the model can avoid re-raising them.
        let existing = self
            .store
            .list_memories(&MemoryFilter {
                status: Some("active".into()),
                limit: 120,
                ..Default::default()
            })
            .await?;
        let pending_suggestions = self.store.list_suggestions(Some("new"), 50).await.unwrap_or_default();
        let event_lines: Vec<String> = events
            .iter()
            .map(|event| {
                serde_json::to_string(event).map_err(|error| {
                    AppError::Internal(format!("serialize collected event: {error}"))
                })
            })
            .collect::<Result<_, _>>()?;
        let user_prompt = prompt::build_learn_prompt(&existing, &pending_suggestions, &event_lines, truncated);

        // One retry on parse failure (the model occasionally wraps in prose).
        let mut parsed = None;
        let mut last_err = String::new();
        let mut provider_failed = false;
        for attempt in 0..2 {
            match self
                .completer
                .complete(&model.provider_id, &model.model, prompt::LEARN_SYSTEM, &user_prompt, LEARN_MAX_TOKENS)
                .await
            {
                Ok(raw) => match prompt::parse_learn_output(&raw) {
                    Ok(out) => {
                        parsed = Some(out);
                        break;
                    }
                    Err(e) => {
                        last_err = e;
                        tracing::debug!(attempt, error = %last_err, "companion learn output unparseable");
                    }
                },
                Err(e) => {
                    last_err = e.to_string();
                    provider_failed = true;
                    break; // provider failure: don't burn a retry
                }
            }
        }

        let Some(output) = parsed else {
            run.status = "error".into();
            run.error = Some(last_err);
            run.finished_at = Some(now_ms());
            // Provider failure is transient: keep the cursor so the same
            // events retry once the provider recovers. Parse failure is the
            // model misformatting — retry the batch a few scheduled runs,
            // then advance past it so a consistently-confused model can't
            // re-burn tokens on the same batch forever.
            if !provider_failed {
                let streak = self.store.get_state_i64("learn_parse_fail_streak").await.unwrap_or(0) + 1;
                if streak >= PARSE_FAIL_GIVE_UP_RUNS {
                    self.store.set_state("learn_cursor_ts", &new_cursor.to_string()).await?;
                    self.store.set_state("learn_parse_fail_streak", "0").await?;
                    tracing::warn!(events = run.events_processed, "companion learn batch abandoned after repeated parse failures");
                } else {
                    self.store.set_state("learn_parse_fail_streak", &streak.to_string()).await?;
                }
            }
            self.store.insert_learn_run(&run).await?;
            if let Some(target) = target.as_deref() {
                self.emitter.emit_learn_finished(target, &run);
            }
            return Ok(run);
        };
        let _ = self.store.set_state("learn_parse_fail_streak", "0").await;

        // Apply: decay first, then reinforce/supersede/insert.
        let _ = self.store.decay_memories().await;
        self.store
            .reinforce_memories(&output.reinforce_memory_ids)
            .await?;
        self.store
            .archive_memories(&output.supersede_memory_ids)
            .await?;

        let prior_active = self.store.count_memories("active").await.unwrap_or(0);
        for m in &output.memories {
            if self.store.find_similar_active(&m.kind, &m.content).await?.is_some() {
                continue;
            }
            self.store
                .insert_memory(&m.kind, &m.content, &m.tags, m.importance, "learn")
                .await?;
            run.memories_added += 1;
        }
        // First-preference milestone: the moment nomi visibly "gets" you.
        if prior_active == 0 && run.memories_added > 0 {
            let milestone = self
                .store
                .insert_suggestion(
                    "insight",
                    "nomi 学会了关于你的第一条记忆！",
                    "我开始懂你了，快来记忆页看看吧～",
                    Some(&serde_json::json!({"type": "navigate", "to": "/nomi?tab=memories"})),
                )
                .await?;
            run.suggestions_added += 1;
            if let Some(target) = target.as_deref() {
                self.emitter.emit_suggestion_created(target, &milestone);
            }
        }
        for s in output.suggestions.iter().take(3) {
            // Insert-side dedup backstop: even when the model ignores the
            // "don't repeat pending suggestions" rule, a similar status='new'
            // suggestion blocks the duplicate. The hit is not silently
            // dropped: the existing suggestion is touched (created_at bumped)
            // so repeated evidence re-floats it instead of vanishing.
            if let Some(existing_id) = self.store.find_similar_suggestion(&s.kind, &s.title, &s.body).await? {
                if let Err(e) = self.store.touch_suggestion(&existing_id).await {
                    tracing::warn!(error = %e, suggestion_id = %existing_id, "companion learn failed to touch duplicate suggestion");
                }
                continue;
            }
            let created = self
                .store
                .insert_suggestion(&s.kind, &s.title, &s.body, s.action.as_ref())
                .await?;
            run.suggestions_added += 1;
            if let Some(target) = target.as_deref() {
                self.emitter.emit_suggestion_created(target, &created);
            }
        }

        if let Some(mood) = &output.mood {
            self.store.set_state("mood", mood).await?;
            if let Some(target) = target.as_deref() {
                self.emitter.emit_mood_changed(target, mood);
            }
        }
        run.summary = output.diary;

        // XP: 1 per event + 5 per new memory — a shared achievement, granted
        // to every companion in the roster (spec ruling 2: the family grows
        // together on the shared learning loop).
        let _ = self
            .store
            .add_xp_all(
                &self.registry.ids().await,
                run.events_processed + run.memories_added * 5,
            )
            .await;

        self.store.set_state("learn_cursor_ts", &new_cursor.to_string()).await?;
        run.finished_at = Some(now_ms());
        self.store.insert_learn_run(&run).await?;
        if let Some(target) = target.as_deref() {
            self.emitter.emit_learn_finished(target, &run);
        }
        Ok(run)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::collector::{CollectedEvent, append_event};
    use crate::profile::SharedCompanionConfig;
    use nomifun_api_types::WebSocketMessage;
    use nomifun_realtime::BroadcastEventBus;
    use tokio::sync::RwLock;

    struct CannedCompleter(String);

    #[async_trait::async_trait]
    impl CompanionCompleter for CannedCompleter {
        async fn complete(&self, _p: &str, _m: &str, _s: &str, _u: &str, _t: u32) -> Result<String, AppError> {
            Ok(self.0.clone())
        }
    }

    /// Learner over a temp dir with one registered companion (so the shared XP
    /// grant has someone to land on). Returns the learner + that companion's id.
    async fn make_learner(dir: &std::path::Path, reply: &str) -> (Learner, String) {
        let mut config = SharedCompanionConfig::default();
        config.learn.model = Some(nomifun_common::ProviderWithModel {
            provider_id: nomifun_common::ProviderId::new().into_string(),
            model: "test-model".into(),
            use_model: None,
        });
        let registry = Arc::new(
            CompanionRegistry::scan(dir.join("companions"), dir.join("shared"))
                .unwrap(),
        );
        let companion = registry.create("测试宠", "ink").await.unwrap();
        let learner = Learner {
            companion_dir: dir.to_path_buf(),
            config: Arc::new(RwLock::new(config)),
            store: CompanionStore::open_memory().await.unwrap(),
            registry,
            completer: Arc::new(CannedCompleter(reply.to_owned())),
            emitter: CompanionEventEmitter::new(Arc::new(BroadcastEventBus::new(16)), "owner-a"),
            run_lock: Arc::new(Mutex::new(())),
        };
        (learner, companion.companion_id)
    }

    fn seed_event(dir: &std::path::Path) {
        append_event(
            dir,
            &CollectedEvent {
                event_id: nomifun_common::generate_id(),
                ts: now_ms(),
                source: "chat_user_messages".into(),
                name: "message.userCreated".into(),
                data: serde_json::json!({"content": "帮我看看 Rust 编译错误"}),
            },
        )
        .unwrap();
    }

    #[tokio::test]
    async fn run_once_applies_learn_output() {
        let dir = tempfile::tempdir().unwrap();
        seed_event(dir.path());
        let reply = r#"{"memories":[{"kind":"profile","content":"主人是 Rust 工程师","importance":0.9}],
            "suggestions":[{"kind":"insight","title":"洞察","body":"最近常调编译错误"}],
            "mood":"content","diary":"今天陪主人修了 bug～"}"#;
        let (learner, companion_id) = make_learner(dir.path(), reply).await;
        let run = learner.run_once().await.unwrap();
        assert_eq!(run.status, "ok");
        assert_eq!(run.events_processed, 1);
        assert_eq!(run.memories_added, 1);
        // 1 real suggestion + 1 first-memory milestone
        assert_eq!(run.suggestions_added, 2);
        assert_eq!(learner.store.get_state("mood").await.unwrap().unwrap(), "content");
        assert!(learner.store.get_state_i64("learn_cursor_ts").await.unwrap() > 0);
        // Shared XP grant lands on every registered companion (1 event + 1*5).
        assert_eq!(learner.store.get_companion_state_i64(&companion_id, "xp").await.unwrap(), 6);
        assert_eq!(learner.store.get_state_i64("xp").await.unwrap(), 0);
        // Cursor advanced: a second run sees no events.
        let run2 = learner.run_once().await.unwrap();
        assert_eq!(run2.status, "no_events");
    }

    #[tokio::test]
    async fn run_once_skips_duplicate_pending_suggestions() {
        let dir = tempfile::tempdir().unwrap();
        seed_event(dir.path());
        let reply = r#"{"suggestions":[{"kind":"insight","title":"最近常调编译错误","body":"建议看看构建脚本"}]}"#;
        let (learner, _) = make_learner(dir.path(), reply).await;

        let run1 = learner.run_once().await.unwrap();
        assert_eq!(run1.suggestions_added, 1);
        assert_eq!(learner.store.count_suggestions("new").await.unwrap(), 1);
        let first = &learner.store.list_suggestions(Some("new"), 10).await.unwrap()[0];
        let (first_suggestion_id, first_created_at) =
            (first.suggestion_id.clone(), first.created_at);

        // Same model output over a new event batch: the pending suggestion
        // blocks the duplicate, and the dedup hit touches it (created_at
        // bumped) instead of silently dropping the repeated evidence.
        // (Sleep keeps the new event's ms timestamp past the advanced
        // cursor and guarantees a strictly larger touch timestamp.)
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        seed_event(dir.path());
        let run2 = learner.run_once().await.unwrap();
        assert_eq!(run2.status, "ok");
        assert_eq!(run2.suggestions_added, 0);
        assert_eq!(learner.store.count_suggestions("new").await.unwrap(), 1);
        let touched = &learner.store.list_suggestions(Some("new"), 10).await.unwrap()[0];
        assert_eq!(
            touched.suggestion_id, first_suggestion_id,
            "dedup must keep the existing suggestion"
        );
        assert!(
            touched.created_at > first_created_at,
            "dedup hit must touch the existing suggestion ({} -> {})",
            first_created_at,
            touched.created_at
        );

        // Once decided, the same suggestion may be raised again.
        let pending = learner.store.list_suggestions(Some("new"), 10).await.unwrap();
        learner
            .store
            .decide_suggestion(&pending[0].suggestion_id, false)
            .await
            .unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        seed_event(dir.path());
        let run3 = learner.run_once().await.unwrap();
        assert_eq!(run3.suggestions_added, 1);
    }

    #[tokio::test]
    async fn run_once_records_error_on_garbage_output() {
        let dir = tempfile::tempdir().unwrap();
        seed_event(dir.path());
        let (learner, _) = make_learner(dir.path(), "我不会输出 JSON").await;
        let run = learner.run_once().await.unwrap();
        assert_eq!(run.status, "error");
        assert!(run.error.is_some());
    }

    #[tokio::test]
    async fn run_once_skips_when_model_unconfigured() {
        let dir = tempfile::tempdir().unwrap();
        seed_event(dir.path());
        let (learner, _) = make_learner(dir.path(), "{}").await;
        learner.config.write().await.learn.model = Default::default();
        let run = learner.run_once().await.unwrap();
        assert_eq!(run.status, "model_unconfigured");
    }

    #[derive(Default)]
    struct RecordingBroadcaster {
        events: std::sync::Mutex<Vec<WebSocketMessage<serde_json::Value>>>,
    }
    impl nomifun_realtime::UserEventSink for RecordingBroadcaster {
        fn send_to_user(&self, _user_id: &str, e: WebSocketMessage<serde_json::Value>) {
            self.events.lock().unwrap().push(e);
        }
    }

    #[tokio::test]
    async fn learn_events_scoped_to_default_companion() {
        let dir = tempfile::tempdir().unwrap();
        seed_event(dir.path());
        let reply = r#"{"memories":[{"kind":"profile","content":"主人是 Rust 工程师","importance":0.9}],
            "suggestions":[{"kind":"insight","title":"洞察","body":"最近常调编译错误"}],
            "mood":"content","diary":"今天陪主人修了 bug～"}"#;

        let mut config = SharedCompanionConfig::default();
        config.learn.model = Some(nomifun_common::ProviderWithModel {
            provider_id: nomifun_common::ProviderId::new().into_string(),
            model: "test-model".into(),
            use_model: None,
        });
        let registry = Arc::new(
            CompanionRegistry::scan(
                dir.path().join("companions"),
                dir.path().join("shared"),
            )
            .unwrap(),
        );
        let _a = registry.create("甲", "ink").await.unwrap();
        let b = registry.create("乙", "ink").await.unwrap();
        config.default_companion_id = Some(b.companion_id.clone()); // 默认体 = 乙

        let bc = Arc::new(RecordingBroadcaster::default());
        let learner = Learner {
            companion_dir: dir.path().to_path_buf(),
            config: Arc::new(RwLock::new(config)),
            store: CompanionStore::open_memory().await.unwrap(),
            registry,
            completer: Arc::new(CannedCompleter(reply.to_owned())),
            emitter: CompanionEventEmitter::new(bc.clone(), "owner-a"),
            run_lock: Arc::new(Mutex::new(())),
        };
        learner.run_once().await.unwrap();

        let events = bc.events.lock().unwrap().clone();
        for name in [
            "companion.suggestion-created",
            "companion.mood-changed",
            "companion.learn-finished",
            "companion.learn-started",
        ] {
            let evs: Vec<_> = events.iter().filter(|e| e.name == name).collect();
            assert!(!evs.is_empty(), "expected at least one {name} event");
            for e in evs {
                assert_eq!(
                    e.data.get("companion_id").and_then(|v| v.as_str()),
                    Some(b.companion_id.as_str()),
                    "{name} 必须 scope 到默认体 乙"
                );
            }
        }
    }
}

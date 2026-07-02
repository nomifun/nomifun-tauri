//! Session-window archiving loop (伙伴会话窗口归档).
//!
//! A companion has exactly one long-lived chat thread. Left unbounded its engine
//! context only ever grows, so simple chats get expensive and the companion
//! can't take on dense work. This loop cuts that thread into **windows**: when a
//! window goes idle for `idle_minutes`, its messages are compressed into a
//! day-partitioned digest (stored in `companion_session_windows`), the live
//! engine context is durably reset (`clear_context`, via [`ArchiveConversationPort`]),
//! and a fresh window begins — so the live context stays small while long-term
//! continuity is preserved as retrievable day-digests ("去年今日发生的事情").
//!
//! Complements — never duplicates — the [`crate::learner`]: the learner distills
//! atomic facts from the event log into `companion_memories`; the archiver writes
//! per-session narrative digests from the window's chat messages. Different
//! sources, different tables, no double processing. Both are opt-in background
//! LLM loops driven by the shared learn model.

use std::sync::Arc;
use std::time::Duration;

use nomifun_common::{AppError, now_ms};
use tokio::sync::Mutex;

use crate::collector::SharedConfig;
use crate::config::ModelConfig;
use crate::learner::CompanionCompleter;
use crate::prompt;
use crate::registry::CompanionRegistry;
use crate::store::CompanionStore;

const TICK_SECONDS: u64 = 60;
/// Cap the messages fed into one digest so a very long session can't blow the
/// prompt (most-recent are kept).
const MAX_DIGEST_LINES: usize = 300;
/// Truncate any single message this long before it enters the digest prompt.
const MAX_CHARS_PER_MSG: usize = 1500;

/// One message belonging to a session window, already normalized to (is_user,
/// text) by the conversation-domain port — the archiver never parses the raw
/// `messages.content` JSON itself.
#[derive(Debug, Clone)]
pub struct WindowMessage {
    /// True for the master's messages (position=right), false for the companion's.
    pub is_user: bool,
    pub content: String,
    pub created_at: i64,
}

/// Seam over the conversation domain. The archiver only needs to (a) read a
/// window's messages and (b) durably reset the live engine context. A trait
/// keeps `nomifun-companion` from hard-depending on conversation internals and
/// lets tests run without a live `ConversationService` (mirrors the
/// `IdmmHandle` seam in `nomifun-requirement`).
#[async_trait::async_trait]
pub trait ArchiveConversationPort: Send + Sync {
    /// Messages of `conversation_id` with `created_at > since_ts`, oldest first.
    async fn window_messages(&self, conversation_id: &str, since_ts: i64) -> Result<Vec<WindowMessage>, AppError>;
    /// Durably reset the conversation's agent context: warm the agent if needed,
    /// then `clear_context` so the engine forgets the archived window while the
    /// visible transcript stays. Best-effort; the caller tolerates failure.
    async fn reset_context(&self, conversation_id: &str) -> Result<(), AppError>;
}

/// Sweep thresholds (subset of [`crate::SharedArchiveConfig`] needed by [`decide`]).
#[derive(Debug, Clone)]
pub struct ArchiveThresholds {
    pub idle_minutes: u32,
    pub min_chars: usize,
}

/// What a sweep decided to do with one companion's open window.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SweepAction {
    /// Still active (idle < threshold) — leave the window open.
    Wait,
    /// Idle & has enough content — summarize, reset context, roll a new window.
    Archive,
    /// Idle but too little content — roll the boundary forward, no digest/reset.
    Skip,
}

/// Pure decision so the trigger rule is unit-testable without any IO.
///
/// - Not idle yet → `Wait` (an active session is never cut, even across midnight).
/// - Idle + ≥1 user message + ≥`min_chars` of content → `Archive`.
/// - Idle otherwise → `Skip` (trivial/empty window, don't burn digest tokens).
pub fn decide(idle_ms: i64, user_messages: usize, total_chars: usize, cfg: &ArchiveThresholds) -> SweepAction {
    let idle_threshold_ms = (cfg.idle_minutes.max(1) as i64) * 60_000;
    if idle_ms < idle_threshold_ms {
        return SweepAction::Wait;
    }
    if user_messages >= 1 && total_chars >= cfg.min_chars {
        SweepAction::Archive
    } else {
        SweepAction::Skip
    }
}

/// Format window messages into role-tagged digest lines, bounded in count and
/// per-message length so the prompt stays small.
fn format_lines(msgs: &[WindowMessage]) -> Vec<String> {
    let start = msgs.len().saturating_sub(MAX_DIGEST_LINES);
    msgs[start..]
        .iter()
        .map(|m| {
            let who = if m.is_user { "用户" } else { "伙伴" };
            let content: String = m.content.chars().take(MAX_CHARS_PER_MSG).collect();
            format!("[{who}] {content}")
        })
        .collect()
}

pub struct Archiver {
    pub store: CompanionStore,
    pub config: SharedConfig,
    pub registry: Arc<CompanionRegistry>,
    /// Reuses the learn LLM seam (same model config, `cfg.learn.model`).
    pub completer: Arc<dyn CompanionCompleter>,
    pub port: Arc<dyn ArchiveConversationPort>,
    /// Guards against overlapping sweeps (tick vs. a future "run now").
    pub run_lock: Arc<Mutex<()>>,
}

impl Archiver {
    /// Spawn the periodic sweep loop. No-op each tick while `archive.enabled` is
    /// false, so an unconfigured install pays nothing.
    pub fn spawn(self: Arc<Self>) {
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_secs(TICK_SECONDS));
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                interval.tick().await;
                let enabled = { self.config.read().await.archive.enabled };
                if !enabled {
                    continue;
                }
                if let Err(e) = self.sweep_once().await {
                    tracing::warn!(error = %e, "companion archive sweep failed");
                }
            }
        });
    }

    /// One sweep over every companion. Skips entirely when disabled or when a
    /// sweep is already running.
    pub async fn sweep_once(&self) -> Result<(), AppError> {
        let Ok(_guard) = self.run_lock.try_lock() else {
            return Ok(());
        };
        let (enabled, thresholds, model) = {
            let cfg = self.config.read().await;
            (
                cfg.archive.enabled,
                ArchiveThresholds { idle_minutes: cfg.archive.idle_minutes, min_chars: cfg.archive.min_chars },
                cfg.learn.model.clone(),
            )
        };
        if !enabled {
            return Ok(());
        }
        for id in self.registry.ids().await {
            if let Err(e) = self.sweep_companion(&id, &thresholds, &model).await {
                tracing::warn!(companion = %id, error = %e, "companion archive sweep failed");
            }
        }
        Ok(())
    }

    /// Sweep one companion's active window. Returns the action taken (also used
    /// by tests). Errors are per-companion and never abort the whole sweep.
    pub async fn sweep_companion(
        &self,
        companion_id: &str,
        thresholds: &ArchiveThresholds,
        model: &ModelConfig,
    ) -> Result<SweepAction, AppError> {
        // No active chat thread → nothing to archive.
        let Some(conversation_id) = crate::companion::active_thread_ptr(&self.store, companion_id)
            .await?
            .filter(|s| !s.is_empty())
        else {
            return Ok(SweepAction::Wait);
        };

        // First-ever window covers all pre-existing history (boundary 0) so the
        // initial archive bootstraps from — and resets — a possibly-bloated
        // legacy thread. Rolled windows set their own boundary (close time).
        let window = self.store.ensure_open_window(companion_id, &conversation_id, 0).await?;
        let msgs = self.port.window_messages(&conversation_id, window.boundary_ts).await?;

        let now = now_ms();
        let user_messages = msgs.iter().filter(|m| m.is_user).count();
        let total_chars: usize = msgs.iter().map(|m| m.content.chars().count()).sum();
        let last_activity = msgs.iter().map(|m| m.created_at).max().unwrap_or(window.started_at);
        self.store.touch_window(&window.id, last_activity, msgs.len() as i64).await?;

        let idle_ms = now - last_activity;
        match decide(idle_ms, user_messages, total_chars, thresholds) {
            SweepAction::Wait => Ok(SweepAction::Wait),
            SweepAction::Skip => {
                self.store.close_window(&window.id, "skipped", None, None, 0).await?;
                self.store.ensure_open_window(companion_id, &conversation_id, now).await?;
                Ok(SweepAction::Skip)
            }
            SweepAction::Archive => {
                let lines = format_lines(&msgs);
                let day = crate::companion::format_date(window.started_at);
                let user_prompt = prompt::build_archive_prompt(&day, &lines);
                // Provider failure is transient: leave the window open to retry
                // on a later tick (same policy as the learner keeping its cursor).
                let raw = match self
                    .completer
                    .complete(&model.provider_id, &model.model, prompt::ARCHIVE_SYSTEM, &user_prompt, prompt::ARCHIVE_MAX_TOKENS)
                    .await
                {
                    Ok(raw) => raw,
                    Err(e) => {
                        tracing::warn!(companion = %companion_id, error = %e, "archive digest LLM call failed; will retry");
                        return Ok(SweepAction::Wait);
                    }
                };
                // Parse failure = the model misformatted. Don't retry forever
                // (token burn): give up on this window's digest, close it as
                // skipped, and roll — the raw transcript stays visible.
                let out = match prompt::parse_archive_output(&raw) {
                    Ok(out) => out,
                    Err(e) => {
                        tracing::warn!(companion = %companion_id, error = %e, "archive digest unparseable; skipping window");
                        self.store.close_window(&window.id, "skipped", None, None, 0).await?;
                        self.store.ensure_open_window(companion_id, &conversation_id, now).await?;
                        return Ok(SweepAction::Skip);
                    }
                };
                let token_estimate = (total_chars / 4) as i64;
                self.store
                    .close_window(&window.id, "archived", Some(&out.summary), out.highlights_json().as_deref(), token_estimate)
                    .await?;
                // Durable context reset so the next window starts small. Best-
                // effort: a failed reset must not lose the digest we just stored.
                if let Err(e) = self.port.reset_context(&conversation_id).await {
                    tracing::warn!(companion = %companion_id, error = %e, "reset_context after archive failed");
                }
                self.store.ensure_open_window(companion_id, &conversation_id, now).await?;
                Ok(SweepAction::Archive)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::profile::SharedCompanionConfig;
    use std::sync::Mutex as StdMutex;
    use tokio::sync::RwLock;

    fn thr(idle_minutes: u32, min_chars: usize) -> ArchiveThresholds {
        ArchiveThresholds { idle_minutes, min_chars }
    }

    #[test]
    fn decide_waits_while_active() {
        // 10 min idle, threshold 30 → still active.
        assert_eq!(decide(10 * 60_000, 5, 500, &thr(30, 60)), SweepAction::Wait);
    }

    #[test]
    fn decide_archives_when_idle_with_content() {
        assert_eq!(decide(40 * 60_000, 3, 500, &thr(30, 60)), SweepAction::Archive);
    }

    #[test]
    fn decide_skips_idle_trivial_window() {
        // Idle but no user messages.
        assert_eq!(decide(40 * 60_000, 0, 500, &thr(30, 60)), SweepAction::Skip);
        // Idle, has a user message, but too little content.
        assert_eq!(decide(40 * 60_000, 1, 10, &thr(30, 60)), SweepAction::Skip);
    }

    #[test]
    fn format_lines_tags_and_bounds() {
        let msgs = vec![
            WindowMessage { is_user: true, content: "帮我看看这个 bug".into(), created_at: 1 },
            WindowMessage { is_user: false, content: "好的~".into(), created_at: 2 },
        ];
        let lines = format_lines(&msgs);
        assert_eq!(lines[0], "[用户] 帮我看看这个 bug");
        assert_eq!(lines[1], "[伙伴] 好的~");
    }

    struct CannedCompleter {
        reply: String,
        calls: Arc<StdMutex<usize>>,
    }
    #[async_trait::async_trait]
    impl CompanionCompleter for CannedCompleter {
        async fn complete(&self, _p: &str, _m: &str, _s: &str, _u: &str, _t: u32) -> Result<String, AppError> {
            *self.calls.lock().unwrap() += 1;
            Ok(self.reply.clone())
        }
    }

    struct MockPort {
        msgs: Vec<WindowMessage>,
        resets: Arc<StdMutex<Vec<String>>>,
    }
    #[async_trait::async_trait]
    impl ArchiveConversationPort for MockPort {
        async fn window_messages(&self, _conversation_id: &str, since_ts: i64) -> Result<Vec<WindowMessage>, AppError> {
            Ok(self.msgs.iter().filter(|m| m.created_at > since_ts).cloned().collect())
        }
        async fn reset_context(&self, conversation_id: &str) -> Result<(), AppError> {
            self.resets.lock().unwrap().push(conversation_id.to_owned());
            Ok(())
        }
    }

    async fn make_archiver(
        dir: &std::path::Path,
        msgs: Vec<WindowMessage>,
        reply: &str,
    ) -> (Archiver, String, Arc<StdMutex<Vec<String>>>, Arc<StdMutex<usize>>) {
        let mut config = SharedCompanionConfig::default();
        config.archive.enabled = true;
        config.archive.idle_minutes = 30;
        config.archive.min_chars = 20;
        config.learn.model.provider_id = "prov_t".into();
        config.learn.model.model = "test-model".into();
        let registry = Arc::new(CompanionRegistry::scan(dir.join("companions"), dir.join("shared")));
        let companion = registry.create("测试宠", "ink").await.unwrap();
        let store = CompanionStore::open_memory().await.unwrap();
        // Point the companion at a chat thread the port will answer for.
        crate::companion::set_active_thread_ptr(&store, &companion.id, "conv1").await.unwrap();
        let resets = Arc::new(StdMutex::new(Vec::new()));
        let calls = Arc::new(StdMutex::new(0usize));
        let archiver = Archiver {
            store,
            config: Arc::new(RwLock::new(config)),
            registry,
            completer: Arc::new(CannedCompleter { reply: reply.to_owned(), calls: calls.clone() }),
            port: Arc::new(MockPort { msgs, resets: resets.clone() }),
            run_lock: Arc::new(Mutex::new(())),
        };
        (archiver, companion.id, resets, calls)
    }

    const GOOD_DIGEST: &str = r#"{"summary":"今天陪主人修了一下午 Rust 编译错误。","topics":["Rust","编译"],"mood":"content"}"#;

    #[tokio::test]
    async fn active_window_is_not_archived() {
        let dir = tempfile::tempdir().unwrap();
        // Recent activity → idle < 30 min → Wait.
        let msgs = vec![
            WindowMessage { is_user: true, content: "在忙 Rust 报错，帮我看看这段生命周期".into(), created_at: now_ms() - 60_000 },
            WindowMessage { is_user: false, content: "好的，我看看".into(), created_at: now_ms() - 30_000 },
        ];
        let (archiver, id, resets, calls) = make_archiver(dir.path(), msgs, GOOD_DIGEST).await;
        let action = archiver
            .sweep_companion(&id, &thr(30, 20), &archiver.config.read().await.learn.model.clone())
            .await
            .unwrap();
        assert_eq!(action, SweepAction::Wait);
        assert!(archiver.store.list_digests(&id, 10).await.unwrap().is_empty());
        assert!(resets.lock().unwrap().is_empty(), "must not reset an active window");
        assert_eq!(*calls.lock().unwrap(), 0, "no LLM call for an active window");
        // The open window is still open with rolling activity recorded.
        assert!(archiver.store.open_window(&id).await.unwrap().is_some());
    }

    #[tokio::test]
    async fn idle_window_is_archived_reset_and_rolled() {
        let dir = tempfile::tempdir().unwrap();
        // Last activity 40 min ago → idle > 30 → Archive.
        let old = now_ms() - 40 * 60_000;
        let msgs = vec![
            WindowMessage { is_user: true, content: "帮我把这个 Rust 生命周期报错修一下，改用 Arc 传状态".into(), created_at: old - 2000 },
            WindowMessage { is_user: false, content: "好的，已经改好并通过编译~".into(), created_at: old },
        ];
        let (archiver, id, resets, calls) = make_archiver(dir.path(), msgs, GOOD_DIGEST).await;
        let action = archiver
            .sweep_companion(&id, &thr(30, 20), &archiver.config.read().await.learn.model.clone())
            .await
            .unwrap();
        assert_eq!(action, SweepAction::Archive);

        let digests = archiver.store.list_digests(&id, 10).await.unwrap();
        assert_eq!(digests.len(), 1, "one archived digest");
        assert!(digests[0].digest.as_deref().unwrap().contains("Rust"));
        assert_eq!(digests[0].status, "archived");
        assert!(digests[0].token_estimate > 0);

        assert_eq!(resets.lock().unwrap().as_slice(), &["conv1".to_string()], "context was reset once");
        assert_eq!(*calls.lock().unwrap(), 1, "exactly one digest LLM call");

        // A fresh window is open, boundary rolled past the archived messages.
        let open = archiver.store.open_window(&id).await.unwrap().unwrap();
        assert_eq!(open.status, "open");
        assert!(open.boundary_ts > old, "new window excludes the archived messages");
    }

    #[tokio::test]
    async fn idle_trivial_window_is_skipped_no_llm() {
        let dir = tempfile::tempdir().unwrap();
        let old = now_ms() - 40 * 60_000;
        // Idle, but only the companion spoke (0 user messages) → Skip.
        let msgs = vec![WindowMessage { is_user: false, content: "在的~".into(), created_at: old }];
        let (archiver, id, resets, calls) = make_archiver(dir.path(), msgs, GOOD_DIGEST).await;
        let action = archiver
            .sweep_companion(&id, &thr(30, 20), &archiver.config.read().await.learn.model.clone())
            .await
            .unwrap();
        assert_eq!(action, SweepAction::Skip);
        assert!(archiver.store.list_digests(&id, 10).await.unwrap().is_empty(), "skip produces no digest");
        assert!(resets.lock().unwrap().is_empty(), "skip does not reset context");
        assert_eq!(*calls.lock().unwrap(), 0, "skip makes no LLM call");
        // Boundary rolled so the trivial message won't be re-scanned forever.
        assert!(archiver.store.open_window(&id).await.unwrap().unwrap().boundary_ts > 0);
    }

    #[tokio::test]
    async fn unparseable_digest_skips_window() {
        let dir = tempfile::tempdir().unwrap();
        let old = now_ms() - 40 * 60_000;
        let msgs = vec![WindowMessage { is_user: true, content: "帮我看看这个很长的 bug 报错信息吧".into(), created_at: old }];
        let (archiver, id, resets, _calls) = make_archiver(dir.path(), msgs, "我不会输出 JSON").await;
        let action = archiver
            .sweep_companion(&id, &thr(30, 20), &archiver.config.read().await.learn.model.clone())
            .await
            .unwrap();
        assert_eq!(action, SweepAction::Skip, "unparseable digest degrades to skip");
        assert!(archiver.store.list_digests(&id, 10).await.unwrap().is_empty());
        assert!(resets.lock().unwrap().is_empty(), "no reset when we couldn't summarize");
    }
}

//! EvolutionEngine — 后台技能进化循环（design §5）。
//!
//! 镜像 `crate::learner::Learner` 的 tick/cursor/run_lock 脚手架，但独立调度：
//! 挖矿（`miner`，确定性）→ 起草（one_shot）→ 评审（one_shot）→ 物化为待审草稿 SKILL.md
//! + `create_skill` 建议卡。失败只记进 `EvolveRun` + `tracing::warn!`，**绝不 `emit_error`**
//! （后台副任务红线）。蒸馏走 `CompanionCompleter`（选 model，非 agent）。

use std::path::PathBuf;
use std::sync::Arc;

use nomifun_common::{AppError, CompanionSkillId, generate_id, now_ms};
use nomifun_extension::constants::SKILL_MANIFEST_FILE;
use nomifun_extension::skill_service::{self, SkillDraftInput, SkillPaths, SkillScope};
use tokio::sync::Mutex;

use crate::collector::{SharedConfig, read_events_since};
use crate::events::CompanionEventEmitter;
use crate::evolution::miner::{mine_patterns, mine_reflection_candidates, MinedPattern};
use crate::evolution::prompt::{self, DraftOutput};
use crate::evolution::transcript::{render_transcript, TranscriptAnchor, TranscriptSource};
use crate::learner::CompanionCompleter;
use crate::registry::CompanionRegistry;
use crate::store::{CompanionSkill, CompanionStore};

const MAX_EVENTS_PER_RUN: usize = 500;
const TICK_SECONDS: u64 = 60;
const DRAFT_MAX_TOKENS: u32 = 1200;
const CRITIC_MAX_TOKENS: u32 = 256;
/// 一次最多起草几个新技能（避免单轮爆量骚扰）。
const MAX_DRAFTS_PER_RUN: usize = 3;
/// 任务后反思的最小步数门槛（单会话工具序列折叠后 ≥ 此值才作反思候选）。
const REFLECT_MIN_STEPS: usize = 4;
/// 重水合转录行的单行字符上限（控 drafter 上下文成本）。
const DRAFT_LINE_CHARS: usize = 240;
/// 喂给 drafter 的转录行数上限（窗口可能跨多轮）。
const DRAFT_MAX_LINES: usize = 40;
/// 一次进化运行的小结（P1 仅返回，不落表）。
#[derive(Debug, Clone)]
pub struct EvolveRun {
    pub evolve_run_id: String,
    pub started_at: i64,
    pub finished_at: Option<i64>,
    pub status: String,
    pub events_processed: i64,
    pub patterns_found: i64,
    pub drafts_created: i64,
    pub error: Option<String>,
}

pub struct EvolutionEngine {
    pub companion_dir: PathBuf,
    pub config: SharedConfig,
    pub store: CompanionStore,
    pub registry: Arc<CompanionRegistry>,
    pub completer: Arc<dyn CompanionCompleter>,
    pub emitter: CompanionEventEmitter,
    pub skill_paths: Arc<SkillPaths>,
    /// 重水合源（会话库 = 唯一内容源）。`start()` 时为 Noop（会话库晚于伴随服务装配，
    /// 见 `attach_companion`），装配后经 [`set_transcript`] 换成真实适配器。未装配/会话已删
    /// → 起草降级回工具名步骤。`std::sync::RwLock` 因 `attach_companion` 非 async；读出 Arc
    /// 即刻 drop guard，绝不跨 await 持锁。
    pub transcript: std::sync::RwLock<Arc<dyn TranscriptSource>>,
    /// 与 Learner 各自独立的再入守卫。
    pub run_lock: Arc<Mutex<()>>,
}

impl EvolutionEngine {
    /// 晚装配重水合源（会话库适配器在伴随服务之后构建）。
    pub fn set_transcript(&self, src: Arc<dyn TranscriptSource>) {
        *self.transcript.write().expect("transcript lock poisoned") = src;
    }

    /// 为 `anchor` 重水合一段脱敏转录,渲染成 drafter 上下文行。无源/会话已删/锚为空 →
    /// 空(drafter 仅凭工具名步骤起草——优雅降级,绝不阻塞)。
    async fn rehydrate_lines(&self, anchor: &TranscriptAnchor) -> Vec<String> {
        if anchor.conversation_id.is_empty() {
            return Vec::new();
        }
        let src = { self.transcript.read().expect("transcript lock poisoned").clone() };
        match src.window(anchor).await {
            Ok(Some(turns)) => {
                let mut lines = render_transcript(&turns, DRAFT_LINE_CHARS);
                lines.truncate(DRAFT_MAX_LINES);
                lines
            }
            Ok(None) => Vec::new(),
            Err(e) => {
                tracing::debug!(error = %e, "transcript rehydration failed; drafting from steps only");
                Vec::new()
            }
        }
    }
    /// 启动周期 tick 循环。
    pub fn spawn(self: Arc<Self>) {
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(std::time::Duration::from_secs(TICK_SECONDS));
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                interval.tick().await;
                let (enabled, interval_minutes) = {
                    let cfg = self.config.read().await;
                    (cfg.evolve.enabled, cfg.evolve.interval_minutes.max(5) as i64)
                };
                if !enabled {
                    continue;
                }
                let last_run = self.store.get_state_i64("last_evolve_ts").await.unwrap_or(0);
                if now_ms() - last_run < interval_minutes * 60_000 {
                    continue;
                }
                if let Err(e) = self.run_once().await {
                    tracing::warn!(error = %e, "companion evolution run failed");
                }
            }
        });
    }

    /// 一次进化运行。失败绝不 emit_error；状态写进返回的 EvolveRun。
    pub async fn run_once(&self) -> Result<EvolveRun, AppError> {
        let Ok(_guard) = self.run_lock.try_lock() else {
            return Err(AppError::Conflict("an evolution run is already in progress".into()));
        };
        let started_at = now_ms();
        // 先 stamp，崩溃/失败也不会让 60s 调度热循环。
        self.store.set_state("last_evolve_ts", &started_at.to_string()).await?;

        // Skill health/decay pass (P5 T1-B): runs every evolution tick, before the
        // model-configured gate, so unused mined skills fade even when no draft is produced.
        // Fire-and-forget; never emit_error. Emits skill-archived for live UI refresh.
        let (half_life, archive_threshold) = {
            let cfg = self.config.read().await;
            (cfg.evolve.skill_half_life_days, cfg.evolve.skill_archive_threshold)
        };
        if let Ok(archived) = self.store.decay_skills(half_life, archive_threshold).await {
            for skill in archived {
                let owner = {
                    let did = { self.config.read().await.default_companion_id.clone() };
                    self.registry.resolve_default(did.as_deref()).await
                };
                if let Some(owner) = owner.as_deref() {
                    self.emitter.emit_skill_archived(
                        owner,
                        &skill.companion_skill_id,
                        &skill.skill_name,
                    );
                }
            }
        }

        let (model, min_count, min_distinct, reflect_enabled, auto_activate, auto_threshold) = {
            let cfg = self.config.read().await;
            // One model for the whole flywheel: fall back to the learn model when no
            // dedicated evolve model is configured, so default-on works out of the box
            // once the user has set the shared learning model.
            let model = cfg.evolve.model.clone().or_else(|| cfg.learn.model.clone());
            (
                model,
                cfg.evolve.min_pattern_count,
                cfg.evolve.min_distinct_sessions,
                cfg.evolve.reflect_enabled,
                cfg.evolve.auto_activate,
                cfg.evolve.auto_threshold,
            )
        };
        let mut run = EvolveRun {
            // This summary is returned to the current caller only. It is not
            // persisted, exported, or referenced after the call, so use an
            // operation token rather than registering a durable entity type.
            evolve_run_id: generate_id(),
            started_at,
            finished_at: None,
            status: "ok".into(),
            events_processed: 0,
            patterns_found: 0,
            drafts_created: 0,
            error: None,
        };

        let Some(model) = model else {
            run.status = "model_unconfigured".into();
            run.finished_at = Some(now_ms());
            return Ok(run);
        };

        // 自生成技能必须归属某个伙伴（伙伴级专属成长）。默认体解析复用 registry 单一事实源。
        let Some(owner) = ({
            let did = { self.config.read().await.default_companion_id.clone() };
            self.registry.resolve_default(did.as_deref()).await
        }) else {
            run.status = "no_companion".into();
            run.finished_at = Some(now_ms());
            return Ok(run);
        };

        let cursor = self.store.get_state_i64("evolve_cursor_ts").await?;
        let (events, _truncated) =
            read_events_since(&self.companion_dir, cursor, MAX_EVENTS_PER_RUN)?;
        if events.is_empty() {
            run.status = "no_events".into();
            run.finished_at = Some(now_ms());
            return Ok(run);
        }
        run.events_processed = events.len() as i64;
        let new_cursor = events.last().map(|e| e.ts).unwrap_or(cursor);

        let patterns = mine_patterns(&events, min_count, min_distinct);
        run.patterns_found = patterns.len() as i64;

        // Candidates = repeated patterns first, then (if enabled) single complex sessions
        // for post-task reflection. Reflection candidates have distinct_sessions=1 → low
        // confidence → always reviewed, never auto-activated.
        let mut candidates = patterns;
        if reflect_enabled {
            candidates.extend(mine_reflection_candidates(&events, REFLECT_MIN_STEPS, MAX_DRAFTS_PER_RUN));
        }

        let mut provider_failed = false;
        for p in candidates {
            if run.drafts_created as usize >= MAX_DRAFTS_PER_RUN {
                break;
            }
            match self
                .process_candidate(&p, &owner, &model.provider_id, &model.model, min_distinct, auto_activate, auto_threshold)
                .await
            {
                Ok(true) => run.drafts_created += 1,
                Ok(false) => {}
                Err(e) => {
                    // Provider failure: terminate the run and keep the cursor for retry.
                    run.error = Some(e.to_string());
                    provider_failed = true;
                    break;
                }
            }
        }

        // provider 失败：保 cursor（下轮重试该批）；否则推进。
        if provider_failed {
            if run.status == "ok" {
                run.status = "error".into();
            }
        } else {
            self.store.set_state("evolve_cursor_ts", &new_cursor.to_string()).await?;
        }
        run.finished_at = Some(now_ms());
        Ok(run)
    }

    /// Process one candidate (mined pattern or reflection) through draft→critic→materialize.
    /// Returns `Ok(true)` if a skill was produced (draft or auto-activated), `Ok(false)` if
    /// skipped (rejected/already-drafted/critic-reject/invalid/disk-fail), and `Err` ONLY on
    /// provider failure (the caller terminates the run and keeps the cursor). Never `emit_error`.
    #[allow(clippy::too_many_arguments)]
    async fn process_candidate(
        &self,
        p: &MinedPattern,
        owner: &str,
        provider_id: &str,
        model: &str,
        min_distinct: usize,
        auto_activate: bool,
        auto_threshold: f64,
    ) -> Result<bool, AppError> {
        // Skip rejected (negative-sample) or already-drafted signatures.
        if self.store.is_signature_rejected(&p.signature).await.unwrap_or(false) {
            return Ok(false);
        }
        if matches!(
            self.store
                .find_pattern_by_signature(&p.signature)
                .await
                .unwrap_or(None)
                .map(|pattern| pattern.status),
            Some(status) if status == "drafted"
        ) {
            return Ok(false);
        }
        let anchor = p.example_event_ids.first().cloned().ok_or_else(|| {
            AppError::Internal(format!(
                "mined pattern {:?} has no example event id",
                p.signature
            ))
        })?;
        let pattern = self
            .store
            .bump_pattern(&p.signature, &p.anchor.conversation_id, &anchor, now_ms())
            .await?;

        // Draft (1 retry). A completer error → Err (caller terminates + keeps cursor).
        // Rehydrate the real (redacted) transcript window for this pattern so the drafter
        // sees actual how-to, not just tool names; degrades to steps-only when unavailable.
        let context = self.rehydrate_lines(&p.anchor).await;
        let draft_user = prompt::build_draft_prompt(p, &context);
        let mut draft: Option<DraftOutput> = None;
        for attempt in 0..2 {
            match self.completer.complete(provider_id, model, prompt::DRAFT_SYSTEM, &draft_user, DRAFT_MAX_TOKENS).await {
                Ok(raw) => match prompt::parse_draft_output(&raw) {
                    Ok(d) if !d.name.trim().is_empty() && !d.description.trim().is_empty() => {
                        draft = Some(d);
                        break;
                    }
                    Ok(_) => tracing::debug!(attempt, "evolution draft missing name/description"),
                    Err(e) => tracing::debug!(attempt, error = %e, "evolution draft unparseable"),
                },
                Err(e) => return Err(e),
            }
        }
        let Some(draft) = draft else { return Ok(false) };

        // Critic.
        let critic_user = prompt::build_critic_prompt(&draft, p);
        let approved = match self.completer.complete(provider_id, model, prompt::CRITIC_SYSTEM, &critic_user, CRITIC_MAX_TOKENS).await {
            Ok(raw) => prompt::parse_critic_output(&raw).map(|v| v.approve).unwrap_or(false),
            Err(e) => return Err(e),
        };
        // Mark drafted (approved or not) so the same signature isn't re-judged every run.
        self.store
            .mark_pattern_status(&pattern.skill_pattern_id, "drafted")
            .await
            .ok();
        if !approved {
            return Ok(false);
        }

        let name = sanitize_skill_name(&draft.name);
        if name.is_empty() {
            return Ok(false);
        }
        let scope = SkillScope::Companion(owner.to_owned());

        // Evolve-in-place: if a near-identically-named active/draft skill exists, MERGE into it
        // (improve + version bump) instead of creating a near-duplicate (P5 T2-A). Provider error
        // → Err (terminate); any other failure degrades to the normal create path below.
        if let Ok(Some(existing)) = self.store.find_similar_skill(owner, &name).await {
            if let Ok(Some(row)) = self.store.get_skill(&existing.companion_skill_id).await {
                let draft_dir = row.status == "draft";
                if let Ok(dir) =
                    skill_service::skill_dir_for(&self.skill_paths, &scope, &existing.skill_name, draft_dir)
                {
                    if let Ok(existing_body) = tokio::fs::read_to_string(dir.join(SKILL_MANIFEST_FILE)).await {
                        let merge_user = prompt::build_merge_prompt(&existing_body, &draft, p);
                        match self.completer.complete(provider_id, model, prompt::MERGE_SYSTEM, &merge_user, DRAFT_MAX_TOKENS).await {
                            Ok(raw) => {
                                if let Ok(merged) = prompt::parse_draft_output(&raw) {
                                    if !merged.description.trim().is_empty() && !merged.body.trim().is_empty() {
                                        let merged_input = SkillDraftInput {
                                            name: existing.skill_name.clone(),
                                            description: merged.description,
                                            when_to_use: merged.when_to_use,
                                            allowed_tools: None,
                                            paths: None,
                                            body: merged.body,
                                        };
                                        let md = skill_service::build_skill_md(&merged_input);
                                        if crate::skill_io::write_skill(&self.skill_paths, &scope, draft_dir, &existing.skill_name, &md).await.is_ok() {
                                            let _ = self.store.bump_skill_version(&existing.companion_skill_id).await;
                                            self.emitter.emit_skill_learned(
                                                owner,
                                                &existing.companion_skill_id,
                                                &existing.skill_name,
                                            );
                                            self.store.mark_pattern_status(&pattern.skill_pattern_id, "drafted").await.ok();
                                            return Ok(true);
                                        }
                                    }
                                }
                            }
                            Err(e) => return Err(e),
                        }
                    }
                }
            }
            // merge attempt failed softly → fall through to normal create.
        }

        let input = SkillDraftInput {
            name: name.clone(),
            description: draft.description.clone(),
            when_to_use: draft.when_to_use.clone(),
            allowed_tools: None,
            paths: None,
            body: draft.body.clone(),
        };
        let confidence = ((p.distinct_sessions as f64) / ((min_distinct + 2) as f64)).clamp(0.3, 0.95);
        // High-confidence auto-activation only when the user opted in AND confidence clears
        // the bar (repetition-derived; single-session reflections never reach it).
        let auto = auto_activate && confidence >= auto_threshold;

        if let Err(e) = crate::skill_io::create_skill(&self.skill_paths, &scope, /* draft= */ !auto, &input).await {
            tracing::warn!(error = %e, skill = %name, "evolution failed to write skill");
            return Ok(false);
        }
        let now = now_ms();
        let skill = CompanionSkill {
            companion_skill_id: CompanionSkillId::new().into_string(),
            skill_name: name.clone(),
            scope_kind: "companion".into(),
            scope_companion_id: Some(owner.to_owned()),
            status: if auto { "active".into() } else { "draft".into() },
            source: "mined".into(),
            confidence,
            provenance_event_ids: p.example_event_ids.clone(),
            strength: 1.0,
            version: 1,
            skill_pattern_id: Some(pattern.skill_pattern_id.clone()),
            usage_count: 0,
            last_used_at: None,
            created_at: now,
            updated_at: now,
            signature: p.signature.clone(),
        };
        if let Err(e) = self.store.insert_skill(&skill).await {
            if let Err(cleanup_error) = crate::fsio::remove_path_entry(
                &skill_service::skill_dir_for(&self.skill_paths, &scope, &name, !auto)
                    .map_err(|path_error| {
                        AppError::Internal(format!(
                            "resolve failed skill write for cleanup: {path_error}"
                        ))
                    })?,
            ) {
                tracing::error!(
                    error = %cleanup_error,
                    skill = %name,
                    "evolution failed to roll back orphaned skill directory"
                );
            }
            tracing::warn!(error = %e, "evolution failed to insert skill row");
            return Ok(false);
        }

        if auto {
            // Auto-activated: no review card, but emit skill-learned so the UI toasts and
            // the skill shows as active (the user can still archive it — "see + undo").
                self.emitter.emit_skill_learned(
                    owner,
                    &skill.companion_skill_id,
                    &skill.skill_name,
                );
        } else {
            let action = serde_json::json!({
                "type": "create_skill",
                "companion_skill_id": skill.companion_skill_id,
                "companion_id": owner,
            });
            let title = format!("我学会了一个新技能：{name}");
            let body = format!("你做过「{}」这套操作，我把它固化成了技能，采纳后我就能自动帮你做。", draft.description);
            if let Ok(created) = self.store.insert_suggestion("create_skill", &title, &body, Some(&action)).await {
                self.emitter.emit_suggestion_created(&owner, &created);
            }
            self.emitter.emit_skill_drafted(
                owner,
                &skill.companion_skill_id,
                &skill.skill_name,
            );
        }
        Ok(true)
    }

    /// On-demand "learn by demonstration" (P5 T2-B): draft a skill from a single demonstrated
    /// tool-name sequence, bypassing the miner/dedup/critic (the user is deliberately teaching).
    /// Always a reviewable draft, `source="demonstrated"` (never decays, never auto-activates).
    /// `anchor` rehydrates the real session transcript for richer drafting (whole-conversation
    /// window from the caller); degrades to steps-only when unavailable.
    /// Returns the drafted skill name, or `None` if the model produced nothing usable.
    pub async fn draft_from_episode(
        &self,
        steps: Vec<String>,
        anchor: TranscriptAnchor,
        owner: &str,
    ) -> Result<Option<String>, AppError> {
        if steps.len() < 2 {
            return Ok(None);
        }
        nomifun_common::CompanionId::try_from(owner)
            .map_err(|error| AppError::BadRequest(format!("invalid companion id: {error}")))?;
        let model = {
            let cfg = self.config.read().await;
            cfg.evolve.model.clone().or_else(|| cfg.learn.model.clone())
        };
        let Some(model) = model else {
            return Err(AppError::BadRequest("尚未配置学习模型".into()));
        };
        let p = MinedPattern {
            signature: crate::evolution::tool_call_signature(&steps),
            steps: steps.clone(),
            count: 1,
            distinct_sessions: 1,
            example_event_ids: vec![],
            anchor,
        };
        let context = self.rehydrate_lines(&p.anchor).await;
        let draft_user = prompt::build_draft_prompt(&p, &context);
        let mut draft: Option<DraftOutput> = None;
        for _ in 0..2 {
            match self.completer.complete(&model.provider_id, &model.model, prompt::DRAFT_SYSTEM, &draft_user, DRAFT_MAX_TOKENS).await {
                Ok(raw) => {
                    if let Ok(d) = prompt::parse_draft_output(&raw) {
                        if !d.name.trim().is_empty() && !d.description.trim().is_empty() {
                            draft = Some(d);
                            break;
                        }
                    }
                }
                Err(e) => return Err(e),
            }
        }
        let Some(draft) = draft else { return Ok(None) };
        let name = sanitize_skill_name(&draft.name);
        if name.is_empty() {
            return Ok(None);
        }
        let input = SkillDraftInput {
            name: name.clone(),
            description: draft.description.clone(),
            when_to_use: draft.when_to_use.clone(),
            allowed_tools: None,
            paths: None,
            body: draft.body.clone(),
        };
        let scope = SkillScope::Companion(owner.to_owned());
        crate::skill_io::create_skill(&self.skill_paths, &scope, true, &input)
            .await
            .map_err(|e| AppError::Internal(format!("write demonstrated skill: {e}")))?;
        let now = now_ms();
        if let Err(error) = self
            .store
            .insert_skill(&CompanionSkill {
                companion_skill_id: CompanionSkillId::new().into_string(),
                skill_name: name.clone(),
                scope_kind: "companion".into(),
                scope_companion_id: Some(owner.to_owned()),
                status: "draft".into(),
                source: "demonstrated".into(),
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
        {
            let draft_dir = skill_service::skill_dir_for(&self.skill_paths, &scope, &name, true)
                .map_err(|path_error| {
                    AppError::Internal(format!(
                        "resolve demonstrated skill for rollback: {path_error}"
                    ))
                })?;
            crate::fsio::remove_path_entry(&draft_dir).map_err(|cleanup_error| {
                AppError::Internal(format!(
                    "{error}; additionally failed to remove orphaned demonstrated skill {}: {cleanup_error}",
                    draft_dir.display()
                ))
            })?;
            return Err(error);
        }
        let companion_skill_id = self
            .store
            .find_owned_skill_by_name(owner, &name)
            .await?
            .ok_or_else(|| {
                AppError::Internal("demonstrated skill row disappeared after insert".into())
            })?
            .companion_skill_id;
        let action = serde_json::json!({
            "type": "create_skill",
            "companion_skill_id": companion_skill_id,
            "companion_id": owner,
        });
        let title = format!("我学会了你示范的技能：{name}");
        let body = format!("照你示范的「{}」整理成了技能，采纳后我就能复用。", draft.description);
        if let Ok(created) = self
            .store
            .insert_suggestion("create_skill", &title, &body, Some(&action))
            .await
        {
            self.emitter.emit_suggestion_created(&owner, &created);
        }
        self.emitter.emit_skill_drafted(owner, &companion_skill_id, &name);
        Ok(Some(name))
    }
}

/// 归一化技能名 → kebab-case 合法目录名（create_skill 再过 validate_filename）。
/// 全非 ASCII（无可用字符）→ 空串，调用方跳过。
fn sanitize_skill_name(raw: &str) -> String {
    let mut s: String = raw
        .trim()
        .to_lowercase()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '-' })
        .collect();
    while s.contains("--") {
        s = s.replace("--", "-");
    }
    s.trim_matches('-').chars().take(64).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::collector::{CollectedEvent, append_event};
    use crate::evolution::transcript::test_util::StubTranscript;
    use crate::evolution::transcript::{NoopTranscriptSource, TranscriptTurn};
    use crate::profile::SharedCompanionConfig;
    use nomifun_realtime::BroadcastEventBus;
    use tokio::sync::RwLock;

    fn conversation_fixture(sequence: u64) -> String {
        let raw = format!("0190f5fe-7c00-7a00-8abc-{sequence:012}");
        nomifun_common::ConversationId::try_from(raw.as_str()).unwrap().into_string()
    }

    /// 按 system 提示区分起草/评审两次调用。
    struct ScriptedCompleter {
        draft: String,
        approve: bool,
    }
    #[async_trait::async_trait]
    impl CompanionCompleter for ScriptedCompleter {
        async fn complete(&self, _p: &str, _m: &str, system: &str, _u: &str, _t: u32) -> Result<String, AppError> {
            if system == prompt::DRAFT_SYSTEM {
                Ok(self.draft.clone())
            } else {
                Ok(format!("{{\"approve\":{}}}", self.approve))
            }
        }
    }

    /// Records every draft `user` prompt so tests can assert what the drafter actually saw.
    struct CapturingCompleter {
        draft: String,
        approve: bool,
        draft_prompts: Arc<tokio::sync::Mutex<Vec<String>>>,
    }
    #[async_trait::async_trait]
    impl CompanionCompleter for CapturingCompleter {
        async fn complete(&self, _p: &str, _m: &str, system: &str, user: &str, _t: u32) -> Result<String, AppError> {
            if system == prompt::DRAFT_SYSTEM {
                self.draft_prompts.lock().await.push(user.to_owned());
                Ok(self.draft.clone())
            } else {
                Ok(format!("{{\"approve\":{}}}", self.approve))
            }
        }
    }

    fn test_skill_paths(dir: &std::path::Path) -> Arc<SkillPaths> {
        Arc::new(SkillPaths {
            data_dir: dir.to_path_buf(),
            user_skills_dir: dir.join("skills"),
            cron_skills_dir: dir.join("cron/skills"),
            builtin_skills_dir: dir.join("builtin-skills"),
            builtin_rules_dir: dir.join("rules"),
            preset_rules_dir: dir.join("preset-rules"),
            preset_skills_dir: dir.join("preset-skills"),
        })
    }

    fn seed_tool_calls(dir: &std::path::Path) {
        let base = now_ms();
        let mut k = 0i64;
        for conv in [conversation_fixture(1), conversation_fixture(2), conversation_fixture(3)] {
            for tool in ["grep", "read", "edit"] {
                k += 1;
                append_event(
                    dir,
                    &CollectedEvent {
                        event_id: nomifun_common::generate_id(),
                        ts: base + k,
                        source: "tool_calls".into(),
                        name: "tool.call".into(),
                        data: serde_json::json!({"name": tool, "conversation_id": conv, "call_id": format!("{conv}-{tool}")}),
                    },
                )
                .unwrap();
            }
        }
    }

    async fn make_engine(dir: &std::path::Path, draft: &str, approve: bool) -> (EvolutionEngine, String) {
        make_engine_with(dir, Arc::new(ScriptedCompleter { draft: draft.to_owned(), approve })).await
    }

    async fn make_engine_with(dir: &std::path::Path, completer: Arc<dyn CompanionCompleter>) -> (EvolutionEngine, String) {
        let mut config = SharedCompanionConfig::default();
        config.evolve.enabled = true;
        config.evolve.model = Some(nomifun_common::ProviderWithModel {
            provider_id: nomifun_common::ProviderId::new().into_string(),
            model: "test-model".into(),
            use_model: None,
        });
        config.evolve.min_pattern_count = 3;
        config.evolve.min_distinct_sessions = 2;
        let registry = Arc::new(
            CompanionRegistry::scan(dir.join("companions"), dir.join("shared"))
                .unwrap(),
        );
        let companion = registry.create("测试", "ink").await.unwrap();
        config.default_companion_id = Some(companion.companion_id.clone());
        let engine = EvolutionEngine {
            companion_dir: dir.to_path_buf(),
            config: Arc::new(RwLock::new(config)),
            store: CompanionStore::open_memory().await.unwrap(),
            registry,
            completer,
            emitter: CompanionEventEmitter::new(Arc::new(BroadcastEventBus::new(16)), "owner-a"),
            skill_paths: test_skill_paths(dir),
            transcript: std::sync::RwLock::new(Arc::new(NoopTranscriptSource)),
            run_lock: Arc::new(Mutex::new(())),
        };
        (engine, companion.companion_id)
    }

    #[tokio::test]
    async fn run_once_mines_drafts_and_suggests() {
        let dir = tempfile::tempdir().unwrap();
        seed_tool_calls(dir.path());
        let draft = r#"{"name":"grep-read-edit","description":"查找并修改代码","when_to_use":"改 bug 时","body":"步骤"}"#;
        let (engine, cid) = make_engine(dir.path(), draft, true).await;
        let run = engine.run_once().await.unwrap();
        assert_eq!(run.status, "ok");
        assert!(run.patterns_found >= 1, "expected a mined pattern");
        assert_eq!(run.drafts_created, 1);
        // 注册表一条 draft 技能
        let skills = engine.store.list_skills(&cid, false).await.unwrap();
        assert_eq!(skills.len(), 1);
        assert_eq!(skills[0].status, "draft");
        assert_eq!(skills[0].source, "mined");
        // 一条 create_skill 建议卡
        let sugs = engine.store.list_suggestions(Some("new"), 10).await.unwrap();
        assert_eq!(sugs.len(), 1);
        assert_eq!(sugs[0].kind, "create_skill");
        // 草稿 SKILL.md 落盘
        let draft_md = dir.path().join("skills/_drafts").join(&cid).join("grep-read-edit/SKILL.md");
        assert!(draft_md.exists(), "draft SKILL.md missing at {}", draft_md.display());
        // cursor 推进；二次运行无新事件
        assert!(engine.store.get_state_i64("evolve_cursor_ts").await.unwrap() > 0);
        let run2 = engine.run_once().await.unwrap();
        assert_eq!(run2.drafts_created, 0);
    }

    #[tokio::test]
    async fn run_once_skips_when_model_unconfigured() {
        let dir = tempfile::tempdir().unwrap();
        seed_tool_calls(dir.path());
        let (engine, _) = make_engine(dir.path(), "{}", true).await;
        engine.config.write().await.evolve.model = Default::default();
        let run = engine.run_once().await.unwrap();
        assert_eq!(run.status, "model_unconfigured");
    }

    #[tokio::test]
    async fn run_once_critic_reject_creates_no_skill() {
        let dir = tempfile::tempdir().unwrap();
        seed_tool_calls(dir.path());
        let draft = r#"{"name":"x","description":"d","body":"b"}"#;
        let (engine, cid) = make_engine(dir.path(), draft, false).await;
        let run = engine.run_once().await.unwrap();
        assert_eq!(run.drafts_created, 0);
        assert_eq!(engine.store.list_skills(&cid, false).await.unwrap().len(), 0);
    }

    #[tokio::test]
    async fn evolve_falls_back_to_learn_model_when_unconfigured() {
        let dir = tempfile::tempdir().unwrap();
        seed_tool_calls(dir.path());
        let draft = r#"{"name":"gre","description":"d","when_to_use":"w","body":"b"}"#;
        let (engine, _cid) = make_engine(dir.path(), draft, true).await;
        {
            let mut cfg = engine.config.write().await;
            cfg.evolve.model = Default::default(); // no dedicated evolve model
            cfg.learn.model = Some(nomifun_common::ProviderWithModel {
                provider_id: nomifun_common::ProviderId::new().into_string(),
                model: "test-model".into(),
                use_model: None,
            }); // learn model configured
        }
        let run = engine.run_once().await.unwrap();
        assert_ne!(run.status, "model_unconfigured", "should fall back to the learn model");
        assert_eq!(run.drafts_created, 1);
    }

    fn seed_repeated(dir: &std::path::Path, convs: &[String], tools: &[&str]) {
        let base = now_ms();
        let mut k = 0i64;
        for conv in convs {
            for tool in tools {
                k += 1;
                append_event(
                    dir,
                    &CollectedEvent {
                        event_id: nomifun_common::generate_id(),
                        ts: base + k,
                        source: "tool_calls".into(),
                        name: "tool.call".into(),
                        data: serde_json::json!({"name": tool, "conversation_id": conv, "call_id": format!("{conv}-{tool}-{k}")}),
                    },
                )
                .unwrap();
            }
        }
    }

    #[tokio::test]
    async fn high_confidence_pattern_auto_activates_when_enabled() {
        let dir = tempfile::tempdir().unwrap();
        // 4 distinct sessions repeating the same 3-step pattern → confidence ≥ 0.85.
        seed_repeated(
            dir.path(),
            &[conversation_fixture(1), conversation_fixture(2), conversation_fixture(3), conversation_fixture(4)],
            &["grep", "read", "edit"],
        );
        let draft = r#"{"name":"auto-skill","description":"d","when_to_use":"w","body":"b"}"#;
        let (engine, cid) = make_engine(dir.path(), draft, true).await;
        engine.config.write().await.evolve.auto_activate = true;
        let run = engine.run_once().await.unwrap();
        assert_eq!(run.drafts_created, 1);
        let skills = engine.store.list_skills(&cid, false).await.unwrap();
        assert_eq!(skills.len(), 1);
        assert_eq!(skills[0].status, "active", "high-confidence pattern should auto-activate");
        assert!(dir.path().join("skills/companion").join(&cid).join("auto-skill").join("SKILL.md").exists());
        // auto path emits no review card
        assert!(engine.store.list_suggestions(Some("new"), 10).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn reflection_drafts_single_complex_session_and_never_auto_activates() {
        let dir = tempfile::tempdir().unwrap();
        // one session, a long non-repeating tool sequence (5 steps) → reflection candidate.
        seed_repeated(dir.path(), &[conversation_fixture(5)], &["grep", "read", "edit", "write", "bash"]);
        let draft = r#"{"name":"reflect-skill","description":"d","when_to_use":"w","body":"b"}"#;
        let (engine, cid) = make_engine(dir.path(), draft, true).await;
        // even with auto on, a single-session reflection (distinct=1, low confidence) stays a draft.
        engine.config.write().await.evolve.auto_activate = true;
        let run = engine.run_once().await.unwrap();
        assert_eq!(run.drafts_created, 1);
        let skills = engine.store.list_skills(&cid, false).await.unwrap();
        assert_eq!(skills.len(), 1);
        assert_eq!(skills[0].status, "draft", "single-session reflection must be reviewed, not auto-activated");
    }

    struct VersioningCompleter;
    #[async_trait::async_trait]
    impl CompanionCompleter for VersioningCompleter {
        async fn complete(&self, _p: &str, _m: &str, system: &str, _u: &str, _t: u32) -> Result<String, AppError> {
            if system == prompt::DRAFT_SYSTEM {
                Ok(r#"{"name":"grep-read-edit-flow","description":"d","when_to_use":"w","body":"new"}"#.into())
            } else if system == prompt::CRITIC_SYSTEM {
                Ok(r#"{"approve":true}"#.into())
            } else {
                // MERGE_SYSTEM
                Ok(r#"{"name":"grep-read-edit","description":"merged desc","when_to_use":"w","body":"merged body"}"#.into())
            }
        }
    }

    #[tokio::test]
    async fn evolve_improves_similar_skill_in_place_not_duplicate() {
        let dir = tempfile::tempdir().unwrap();
        seed_repeated(
            dir.path(),
            &[conversation_fixture(1), conversation_fixture(2), conversation_fixture(3)],
            &["grep", "read", "edit"],
        );
        let (engine, cid) = make_engine_with(dir.path(), Arc::new(VersioningCompleter)).await;
        // Pre-existing active skill whose name the new draft ("grep-read-edit-flow") is similar to.
        let input = SkillDraftInput {
            name: "grep-read-edit".into(),
            description: "原始".into(),
            when_to_use: None,
            allowed_tools: None,
            paths: None,
            body: "old".into(),
        };
        skill_service::create_skill(&engine.skill_paths, &SkillScope::Companion(cid.clone()), false, &input).await.unwrap();
        let now = now_ms();
        engine
            .store
            .insert_skill(&CompanionSkill {
            companion_skill_id: nomifun_common::generate_id(),
                skill_name: "grep-read-edit".into(),
                scope_kind: "companion".into(),
                scope_companion_id: Some(cid.clone()),
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
                signature: "old-sig".into(),
            })
            .await
            .unwrap();

        engine.run_once().await.unwrap();
        let skills = engine.store.list_skills(&cid, false).await.unwrap();
        // No duplicate created; the similar existing skill was improved in place + version bumped.
        assert_eq!(skills.len(), 1, "should evolve in place, not duplicate");
        assert_eq!(skills[0].skill_name, "grep-read-edit");
        assert_eq!(skills[0].version, 2, "version should bump on evolve-in-place");
    }

    #[tokio::test]
    async fn draft_from_episode_creates_demonstrated_draft() {
        let dir = tempfile::tempdir().unwrap();
        let draft = r#"{"name":"demo-flow","description":"d","when_to_use":"w","body":"b"}"#;
        let (engine, cid) = make_engine(dir.path(), draft, true).await;
        let name = engine
            .draft_from_episode(vec!["grep".into(), "read".into(), "edit".into()], TranscriptAnchor::default(), &cid)
            .await
            .unwrap();
        assert_eq!(name.as_deref(), Some("demo-flow"));
        let skills = engine.store.list_skills(&cid, false).await.unwrap();
        assert_eq!(skills.len(), 1);
        assert_eq!(skills[0].source, "demonstrated", "demonstrated skills are exempt from decay");
        assert_eq!(skills[0].status, "draft", "demonstration always produces a reviewable draft");
    }

    /// 守门:重水合命中 → drafter 看到真实(脱敏)转录内容,而非仅工具名。
    #[tokio::test]
    async fn process_candidate_drafts_from_rehydrated_transcript() {
        let dir = tempfile::tempdir().unwrap();
        seed_tool_calls(dir.path());
        let draft = r#"{"name":"grep-read-edit","description":"d","when_to_use":"w","body":"b"}"#;
        let seen = Arc::new(tokio::sync::Mutex::new(Vec::new()));
        let completer = Arc::new(CapturingCompleter { draft: draft.into(), approve: true, draft_prompts: seen.clone() });
        let (engine, _cid) = make_engine_with(dir.path(), completer).await;
        engine.set_transcript(Arc::new(StubTranscript::with(vec![
            TranscriptTurn::user("把日志里的错误找出来改掉"),
            TranscriptTurn::tool("grep", Some("pattern=ERROR".into()), Some("命中 3 处".into())),
        ])));
        engine.run_once().await.unwrap();
        let prompts = seen.lock().await;
        let dp = prompts.iter().find(|p| p.contains("可复用技能")).expect("a draft prompt was issued");
        assert!(dp.contains("实际操作过程"), "rehydrated transcript section missing: {dp}");
        assert!(dp.contains("把日志里的错误找出来改掉"), "user content missing: {dp}");
        assert!(dp.contains("命中 3 处"), "tool result missing: {dp}");
    }

    /// 守门:悬空指针(无源,默认 Noop)→ 降级回工具名步骤,不报错、照常起草、无转录段。
    #[tokio::test]
    async fn process_candidate_degrades_when_transcript_missing() {
        let dir = tempfile::tempdir().unwrap();
        seed_tool_calls(dir.path());
        let draft = r#"{"name":"grep-read-edit","description":"d","when_to_use":"w","body":"b"}"#;
        let seen = Arc::new(tokio::sync::Mutex::new(Vec::new()));
        let completer = Arc::new(CapturingCompleter { draft: draft.into(), approve: true, draft_prompts: seen.clone() });
        let (engine, cid) = make_engine_with(dir.path(), completer).await; // transcript stays Noop
        let run = engine.run_once().await.unwrap();
        assert!(run.drafts_created >= 1, "must still draft from steps alone");
        let prompts = seen.lock().await;
        let dp = prompts.iter().find(|p| p.contains("可复用技能")).expect("a draft prompt was issued");
        assert!(!dp.contains("实际操作过程"), "degraded draft must carry no transcript section: {dp}");
        // The pattern steps still drive the draft.
        assert!(dp.contains("grep"), "steps still present: {dp}");
        let skills = engine.store.list_skills(&cid, false).await.unwrap();
        assert_eq!(skills.len(), 1);
    }
}

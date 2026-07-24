//! Companion chat threads: real `type='nomi'` conversations driven by the
//! full agent engine (plan mode / skills / slash commands / MCP), flavored
//! with the owning companion's persona system prompt and the companion memory
//! tools.
//!
//! The companion domain owns only a thin thread registry (which conversation ids
//! are companion threads + titles + owning companion); messages, streaming,
//! persistence and lifecycle belong to the conversation domain. Companion
//! threads are marked `extra.companion_session = true` so (a) the agent factory
//! registers the memory tools, and (b) the main sidebar filters them out;
//! `extra.companion_id` records the owning companion for persona/knowledge selection.

use std::sync::Arc;

use async_trait::async_trait;
use nomifun_ai_agent::CompanionMemorySink;
use nomifun_api_types::CreateConversationRequest;
use nomifun_common::{AppError, ProviderWithModel};
use nomifun_conversation::ConversationService;

use crate::collector::{self, SharedConfig};
use crate::events::CompanionEventEmitter;
use crate::profile::CompanionProfileConfig;
use crate::registry::CompanionRegistry;
use crate::store::{CompanionThread, MEMORY_KINDS, MemoryFilter, MemoryScope, CompanionStore};

/// Per-companion runtime-state key holding that companion's active companion thread.
pub(crate) const ACTIVE_THREAD_KEY: &str = "companion_active_thread";

const MEMORY_CHAR_BUDGET: usize = 6000;
const MEMORY_PER_KIND: i64 = 5;
/// How many recent day-digests to inject into a new window's system prompt, and
/// the char budget for that block (separate from the memory snapshot budget).
const DIGEST_INJECT_COUNT: i64 = 3;
const DIGEST_CHAR_BUDGET: usize = 2000;

/// "YYYY-MM-DD" (local time) for memory timestamps surfaced to the model —
/// dating each memory lets the companion treat old task/requirement entries as
/// history instead of standing orders.
pub(crate) fn format_date(ts_ms: i64) -> String {
    use chrono::TimeZone;
    chrono::Local
        .timestamp_millis_opt(ts_ms)
        .single()
        .map(|d| d.format("%Y-%m-%d").to_string())
        .unwrap_or_else(|| "????-??-??".into())
}

/// Read one companion's active-thread pointer. Absence is represented by no KV
/// row, never by an empty ID value.
pub(crate) async fn active_thread_ptr(store: &CompanionStore, companion_id: &str) -> Result<Option<String>, AppError> {
    nomifun_common::CompanionId::try_from(companion_id)
        .map_err(|error| AppError::BadRequest(format!("invalid companion_id: {error}")))?;
    let value = store.get_companion_state(companion_id, ACTIVE_THREAD_KEY).await?;
    value
        .map(|conversation_id| {
            nomifun_common::ConversationId::try_from(conversation_id.as_str())
                .map(|_| conversation_id)
                .map_err(|error| AppError::Internal(format!("invalid stored active conversation_id: {error}")))
        })
        .transpose()
}

/// Write or clear one companion's active-thread pointer.
pub(crate) async fn set_active_thread_ptr(
    store: &CompanionStore,
    companion_id: &str,
    conversation_id: Option<&str>,
) -> Result<(), AppError> {
    nomifun_common::CompanionId::try_from(companion_id)
        .map_err(|error| AppError::BadRequest(format!("invalid companion_id: {error}")))?;
    match conversation_id {
        Some(conversation_id) => {
            nomifun_common::ConversationId::try_from(conversation_id)
                .map_err(|error| AppError::BadRequest(format!("invalid conversation_id: {error}")))?;
            store.set_companion_state(companion_id, ACTIVE_THREAD_KEY, conversation_id).await
        }
        None => store.delete_companion_state(companion_id, ACTIVE_THREAD_KEY).await,
    }
}

/// Build the persona system prompt for a companion conversation. The prompt
/// persists on the conversation row for its whole life, so it only embeds
/// durable facts (persona + a memory digest snapshot); volatile state
/// (level/mood) is described in relative terms and the model is pointed at
/// `recall_memories` for anything newer than the snapshot.
///
/// `channel_platform` flavors the prompt for remote IM Channel Agent
/// sessions: the companion acknowledges it is serving the owner through that
/// platform and that it can drive the whole desktop via the `nomi_*` tools.
pub async fn build_companion_system_prompt(
    store: &CompanionStore,
    profile: &CompanionProfileConfig,
    channel_platform: Option<&str>,
    smart_collaboration: bool,
) -> String {
    let memories = store
        .memories_for_injection(&profile.companion_id, MEMORY_PER_KIND, MEMORY_CHAR_BUDGET)
        .await
        .unwrap_or_default();

    let name = if profile.name.trim().is_empty() { "nomi" } else { profile.name.trim() };
    let remote = channel_platform.map(|p| !p.is_empty()).unwrap_or(false);
    // Remote (IM) snapshot only carries stable identity/preference/knowledge
    // memories. task/episode/affective entries are stale to-dos that, injected
    // into a remote prompt, drive the partner to re-dispatch old work
    // (badcase 2) — so they are filtered out of the remote snapshot entirely.
    let memories: Vec<_> = if remote {
        memories
            .into_iter()
            .filter(|m| matches!(m.kind.as_str(), "profile" | "preference" | "knowledge"))
            .collect()
    } else {
        memories
    };
    let flavor = crate::prompt::persona_flavor(&profile.persona.preset);
    // NOTE: the persona prompt deliberately does NOT pin a reply language. The
    // companion must answer in the app's UI language, which is decided live at
    // agent-build time and appended as a final directive in
    // `nomifun-ai-agent::factory::nomi` (read from system settings). Re-adding a
    // hardcoded 「用中文」 here would freeze the language into the persisted prompt
    // and reintroduce the always-Chinese bug.
    let mut system = format!(
        "你是 {name}，一只住在主人电脑里的电子伙伴伙伴。{flavor}\n\
         你和主人对话时语气符合你的人格；回复简洁直接，先结论后细节。\n\
         你拥有完整的工具能力（读写文件、执行命令、技能、计划模式等），主人请你做事时大胆去做。\n\
         但行事前遵守两条规则：\
         ① 任何创建类操作（会话/定时任务/需求等）之前，先用对应的 list 工具查重；已有同名或同义的项就不要重复创建，除非主人在本轮对话中明确要求再建一个。\
         ② 主人的请求缺少必要配置（如模型供应商/模型）时，先用列表类工具查可用项，自动选一个合理默认（比如第一个可用供应商）并告知主人，或用一句话向主人确认——不要带着空配置硬创建，也不要长篇追问。\n\
         你还有三个专属记忆工具：recall_memories（搜你对主人的长期记忆）、save_memory（记住主人告诉你的重要事）、\
         list_recent_events（看主人最近的工作活动）。当主人提到值得长期记住的偏好/约定/计划时主动 save_memory，宁缺毋滥；\
         下面的记忆节选是开聊时的快照，拿不准时先 recall_memories 查最新。"
    );
    if let Some(platform) = channel_platform.filter(|p| !p.is_empty()) {
        system.push_str(&format!(
            "\n\n主人此刻正通过 {platform} 远程和你说话。此刻你是一个通过 IM 陪主人聊天、答疑、出主意的对话助手：\
             你可以用 nomi_list_conversations / nomi_conversation_status 等只读工具帮主人了解桌面上正在跑的会话状态并转述，\
             也可以用 nomi_memory_* 维护你的长期记忆。\
             当主人主动告诉你某个会话卡在决策上、或你查看到它在等人选择（runtime_state 为 WaitingConfirmation，或 pending_confirmations > 0）时，\
             你可以替主人转达：先用 nomi_list_confirmations(conversation_id) 读出待决项和选项，\
             把问题和选项以编号列表发给主人（如「1. 允许  2. 拒绝」），主人回复编号后，\
             用 nomi_resolve_confirmation(conversation_id, call_id, option) 提交对应选项的 value，别擅自替主人做选择。\
             远程消息排版要适合 IM 阅读：短段落，少用大型 markdown 结构。\n\
             【硬性规则】除非主人在本轮消息中明确要求，否则禁止创建会话、向其他会话派发任务、创建定时任务或需求；\
             禁止依据历史记忆主动执行任何操作。你的默认动作是回答与建议，不是替主人去办事。"
        ));
    } else {
        system.push_str(
            "\n\n你还是整台 Nomi 桌面的总管家：用 nomi_* 工具可以查看/操作所有会话、定时任务、长期记忆和需求平台。\
             删除类操作先向主人复述目标确认后再执行。",
        );
    }
    if !profile.persona.custom.trim().is_empty() {
        system.push_str(&format!("\n主人对你的额外设定：{}", profile.persona.custom.trim()));
    }
    if let Some(snapshot) = profile.applied_preset.as_ref()
        && !snapshot.instructions.trim().is_empty()
    {
        system.push_str(&format!(
            "\n\n## 当前设定：{}\n{}",
            snapshot.preset_name,
            snapshot.instructions.trim()
        ));
    }
    system.push_str(
        "\n\n## 知识沉淀技巧\n\
         除了轻量的全局记忆，你还能把成体系的资料沉淀为知识库，让会话/终端长期受益：\n\
         - 何时沉淀：某领域的问题反复出现、主人明确想留存一批资料、或遇到值得长期参考的 URL 资料源。\n\
         - 动作序列：nomi_knowledge_create_base 建库（可直接带 urls，snapshot 模式会在后台抓快照并生成梗概，\
         立即返回不必等待、切勿重复建库）→ \
         nomi_knowledge_write_file 写入你整理好的 markdown → nomi_knowledge_autogen 刷新梗概 → \
         nomi_knowledge_set_binding 把库绑定到目标会话/终端/你自己（kind=\"companion\"）。绑定变更在目标下次任务启动时生效。\n\
         - 分工边界：全局记忆（nomi_memory_*）只放轻量的个人事实与偏好；知识库放成体系、可检索的领域资料。闲聊琐事不要建库。",
    );
    // 终端操作能力（本地会话；远程 IM 走 PROFILE_LITE，无 terminal 域，不注入）。
    if !remote {
        system.push_str(
            "\n\n## 操作终端会话\n\
             主人电脑上还有「终端会话」(PTY，跑 shell 或 claude/codex/gemini 等 CLI)，你可以直接驱动：\n\
             - nomi_list_terminals 看有哪些终端及状态(running/exited)；nomi_create_terminal 新建(preset: shell|claude|codex|gemini)。\n\
             - nomi_terminal_send(id, text) 把命令或一段话发进去并【直接执行】——不用自己补回车、不用 base64，agent CLI 的粘贴提交也已处理好；\
             要等它跑完并拿结果时带 wait=true(可选 timeout_secs)，会回执 settle_reason 与输出尾巴。\n\
             - nomi_terminal_read_output(id) 读终端最近输出(已去除控制符)，用来查看命令结果或排查。\n\
             - 目标终端已退出(exited)时先 nomi_terminal_relaunch 再发送；kill/delete 这类破坏性操作会要你确认后再做。\n\
             主人在终端页能实时看到你的输入与执行，放心大胆地用。",
        );
    }
    // 智能协作提示只注入本地会话；远程 IM 不具备持久 Agent 委派权限。
    if smart_collaboration && !remote {
        system.push_str(
            "\n\n## 复杂任务：Agent 协作\n\
             统一用 nomi_delegate 委派：并行交给多个 Agent 时传 strategy=parallel 和 tasks，\
             不经规划直接并行开工；任务真正复杂、需要有依赖关系的任务图时，传 strategy=planned 和 goal，\
             交给执行引擎自动拆解并行。主人能在画布上实时看到每个协作 Agent 的状态与产出。\
             派发后直接告诉主人已在后台执行、进度见画布，然后正常继续——不必守着轮询：全部完成\
             或失败时系统会自动把结果回执给你，届时你再向主人汇总产出；若失败，先用 nomi_execution_get \
             看清哪个节点与 last_error，再用 nomi_execution_update 的 adjust/configure/retry 操作恢复。\
             试过几种仍不成就如实问主人怎么办。这样你全程在场，重活也不会挤占你和主人的对话上下文。\
             主人若中途问进度，才用 nomi_execution_get 查一次。\
             简单、单步、几句话能答的事，直接自己做，别为小事创建持久执行。",
        );
    }
    if !memories.is_empty() {
        system.push_str(
            "\n\n## 你对主人的记忆（节选，可用 recall_memories 查更多）\n\
             下面是带日期的历史记忆快照，只用来帮你理解主人。注意：任务/需求类条目（task 等）可能早已完成或过期——\
             无论该记忆来自本快照，还是运行中通过 recall_memories 等工具检索到的结果，都适用同一条规则：\
             未经主人在本轮对话中明确要求，禁止据此主动创建会话/定时任务/需求，也禁止重复执行任何历史请求。\n",
        );
        for m in &memories {
            system.push_str(&format!("- [{}|{}] {}\n", format_date(m.created_at), m.kind, m.content));
        }
    }
    // Recent day-digests (伙伴会话窗口归档): give a freshly-reset window continuity
    // without replaying raw transcript. Local sessions only — remote (IM) prompts
    // stay identity-only and lean. Naturally empty when archiving is off (there are
    // no archived windows), so this costs nothing until the feature is enabled.
    if !remote
        && let Ok(digests) = store.list_digests(&profile.companion_id, DIGEST_INJECT_COUNT).await
        && !digests.is_empty()
    {
        system.push_str(
            "\n\n## 最近的会话回顾（按天归档的日记，帮你记起最近和主人聊过什么，仅供理解上下文）\n",
        );
        let mut used = 0usize;
        for d in &digests {
            let Some(summary) = &d.digest else { continue };
            let line = format!("- [{}] {}\n", format_date(d.started_at), summary.trim());
            used += line.len();
            if used > DIGEST_CHAR_BUDGET {
                break;
            }
            system.push_str(&line);
        }
    }
    system
}

/// reconcile 的纯决策结果。
#[derive(Debug, PartialEq, Eq)]
enum WorkspaceAction {
    /// current 已是 desired，无需动。
    Noop,
    /// current 为空：在 desired 处新建。
    Create(std::path::PathBuf),
    /// 把 current 目录移动到 desired（伙伴改名后的目录跟随）。
    Move {
        from: std::path::PathBuf,
        to: std::path::PathBuf,
    },
    /// current 是外来路径（如 temp cwd）：留置不动，勿孤立已写文件。
    Leave,
}

/// 纯：按 profile 算出目标工作区目录：
/// `{workspaces_dir}/{seq}_{净化名}`（净化名为空则仅 `{seq}`）。
fn compute_desired_workspace_dir(
    workspaces_dir: &std::path::Path,
    profile: &CompanionProfileConfig,
) -> std::path::PathBuf {
    let seg = nomifun_common::sanitize_dir_segment(&profile.name);
    let leaf = if seg.is_empty() {
        profile.seq.to_string()
    } else {
        format!("{}_{}", profile.seq, seg)
    };
    workspaces_dir.join(leaf)
}

/// 纯：根据 current(extra.workspace，已 trim) 与工作区树，决策动作。
fn plan_workspace_reconcile(
    current: &str,
    desired: &std::path::Path,
    workspaces_dir: &std::path::Path,
) -> WorkspaceAction {
    let current = current.trim();
    if current.is_empty() {
        return WorkspaceAction::Create(desired.to_path_buf());
    }
    let cur = std::path::Path::new(current);
    if cur == desired {
        return WorkspaceAction::Noop;
    }
    if cur.starts_with(workspaces_dir) {
        WorkspaceAction::Move { from: cur.to_path_buf(), to: desired.to_path_buf() }
    } else {
        WorkspaceAction::Leave
    }
}

/// 执行一个 reconcile 动作的落盘部分；返回应写入 `extra.workspace` 的新路径
/// （None = 保留 current 不变）。尽力而为：移动失败（占用/目标非空）返回 None。
fn apply_workspace_action(action: WorkspaceAction) -> Option<std::path::PathBuf> {
    match action {
        WorkspaceAction::Noop | WorkspaceAction::Leave => None,
        WorkspaceAction::Create(dir) => {
            if let Err(e) = std::fs::create_dir_all(&dir) {
                tracing::warn!(error = %e, dir = %dir.display(), "create companion workspace dir failed");
            }
            Some(dir)
        }
        WorkspaceAction::Move { from, to } => {
            if let Some(parent) = to.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            // 目标已存在：仅当其为空目录时安全推进（删空再 rename）；非空则保留 current。
            if to.exists() {
                let empty = std::fs::read_dir(&to)
                    .map(|mut d| d.next().is_none())
                    .unwrap_or(false);
                if empty {
                    let _ = std::fs::remove_dir(&to);
                } else {
                    tracing::warn!(to = %to.display(), "companion workspace target exists and is non-empty; keeping current");
                    return None;
                }
            }
            match std::fs::rename(&from, &to) {
                Ok(()) => Some(to),
                Err(e) => {
                    tracing::warn!(error = %e, from = %from.display(), to = %to.display(), "move companion workspace failed; keeping current");
                    None
                }
            }
        }
    }
}

#[cfg(test)]
mod workspace_path_tests {
    use super::*;
    use std::path::{Path, PathBuf};

    fn profile(seq: u64, name: &str) -> CompanionProfileConfig {
        CompanionProfileConfig::new(name, "ink", seq)
    }

    #[test]
    fn desired_uses_seq_and_sanitized_name() {
        let ws = Path::new("/data/companion/workspaces");
        let p = profile(1, "毛球");
        assert_eq!(
            compute_desired_workspace_dir(ws, &p),
            PathBuf::from("/data/companion/workspaces/1_毛球")
        );
    }

    #[test]
    fn desired_seq_only_when_name_sanitizes_empty() {
        let ws = Path::new("/data/companion/workspaces");
        let p = profile(7, "///");
        assert_eq!(
            compute_desired_workspace_dir(ws, &p),
            PathBuf::from("/data/companion/workspaces/7")
        );
    }

    #[test]
    fn plan_empty_current_creates() {
        let desired = Path::new("/ws/1_x");
        let ws = Path::new("/ws");
        assert_eq!(
            plan_workspace_reconcile("", desired, ws),
            WorkspaceAction::Create(desired.to_path_buf())
        );
    }

    #[test]
    fn plan_current_equals_desired_noop() {
        let desired = Path::new("/ws/1_x");
        let ws = Path::new("/ws");
        assert_eq!(
            plan_workspace_reconcile("/ws/1_x", desired, ws),
            WorkspaceAction::Noop
        );
    }

    #[test]
    fn plan_outside_tree_is_left_untouched() {
        let desired = Path::new("/ws/1_x");
        let ws = Path::new("/ws");
        assert_eq!(
            plan_workspace_reconcile("/cs/id/workspace", desired, ws),
            WorkspaceAction::Leave
        );
    }

    #[test]
    fn plan_renamed_within_tree_moves() {
        let desired = Path::new("/ws/1_new");
        let ws = Path::new("/ws");
        assert_eq!(
            plan_workspace_reconcile("/ws/1_old", desired, ws),
            WorkspaceAction::Move { from: PathBuf::from("/ws/1_old"), to: desired.to_path_buf() }
        );
    }

    #[test]
    fn plan_foreign_temp_cwd_left_untouched() {
        let desired = Path::new("/ws/1_x");
        let ws = Path::new("/ws");
        assert_eq!(
            plan_workspace_reconcile("/data/conversations/nomi-temp-9", desired, ws),
            WorkspaceAction::Leave
        );
    }
}

#[cfg(test)]
mod workspace_apply_tests {
    use super::*;

    #[test]
    fn create_makes_dir_and_returns_path() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("ws/1_x");
        let out = apply_workspace_action(WorkspaceAction::Create(dir.clone()));
        assert_eq!(out, Some(dir.clone()));
        assert!(dir.is_dir());
    }

    #[test]
    fn move_preserves_files_and_returns_target() {
        let tmp = tempfile::tempdir().unwrap();
        let from = tmp.path().join("cs/id/workspace");
        std::fs::create_dir_all(&from).unwrap();
        std::fs::write(from.join("a.txt"), "hi").unwrap();
        let to = tmp.path().join("ws/1_毛球");
        let out = apply_workspace_action(WorkspaceAction::Move { from: from.clone(), to: to.clone() });
        assert_eq!(out, Some(to.clone()));
        assert!(!from.exists());
        assert_eq!(std::fs::read_to_string(to.join("a.txt")).unwrap(), "hi");
    }

    #[test]
    fn move_into_existing_empty_target_succeeds() {
        let tmp = tempfile::tempdir().unwrap();
        let from = tmp.path().join("ws/1_old");
        std::fs::create_dir_all(&from).unwrap();
        std::fs::write(from.join("a.txt"), "x").unwrap();
        let to = tmp.path().join("ws/1_new");
        std::fs::create_dir_all(&to).unwrap(); // 预先存在且为空
        let out = apply_workspace_action(WorkspaceAction::Move { from: from.clone(), to: to.clone() });
        assert_eq!(out, Some(to.clone()));
        assert_eq!(std::fs::read_to_string(to.join("a.txt")).unwrap(), "x");
    }

    #[test]
    fn move_into_existing_nonempty_target_keeps_current() {
        let tmp = tempfile::tempdir().unwrap();
        let from = tmp.path().join("ws/1_old");
        std::fs::create_dir_all(&from).unwrap();
        let to = tmp.path().join("ws/1_new");
        std::fs::create_dir_all(&to).unwrap();
        std::fs::write(to.join("occupied.txt"), "keep").unwrap(); // 目标非空
        let out = apply_workspace_action(WorkspaceAction::Move { from: from.clone(), to: to.clone() });
        assert_eq!(out, None); // 不覆盖，保留 current
        assert!(from.exists());
        assert_eq!(std::fs::read_to_string(to.join("occupied.txt")).unwrap(), "keep");
    }

    #[test]
    fn noop_and_leave_return_none() {
        assert_eq!(apply_workspace_action(WorkspaceAction::Noop), None);
        assert_eq!(apply_workspace_action(WorkspaceAction::Leave), None);
    }
}

/// Thread management over the real conversation domain. Every method is
/// scoped to one companion — threads are owned, listed and activated per companion.
pub struct CompanionThreads {
    /// Canonical instance owner resolved from the user repository at startup.
    /// Companion conversations are host-control-plane resources and must never
    /// infer their owner from a username or a hard-coded database identifier.
    pub authoritative_user_id: Arc<str>,
    pub store: CompanionStore,
    pub config: SharedConfig,
    pub registry: Arc<CompanionRegistry>,
    pub conversations: Arc<ConversationService>,
    pub runtime_registry: Arc<dyn nomifun_ai_agent::AgentRuntimeRegistry>,
}

impl CompanionThreads {
    /// `NotFound` unless `conversation_id` is a registered thread owned by
    /// `companion_id`.
    async fn assert_owned(&self, companion_id: &str, conversation_id: &str) -> Result<(), AppError> {
        if self.store.thread_companion_id(conversation_id).await?.as_deref() != Some(companion_id) {
            return Err(AppError::NotFound(format!(
                "companion thread '{conversation_id}' not found for companion '{companion_id}'"
            )));
        }
        Ok(())
    }

    /// 把某线程落盘工作区收敛到伙伴目标（seq+name）目录：统管首次创建和改名跟随。
    /// 幂等 + 尽力而为，绝不让调用方失败；被占用则保留当前路径下次再试。
    pub(crate) async fn reconcile_thread_workspace(&self, profile: &CompanionProfileConfig, conversation_id: &str) {
        let resp = match self
            .conversations
            .get(self.authoritative_user_id.as_ref(), conversation_id)
            .await
        {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!(error = %e, conversation_id, "fetch companion thread for workspace reconcile failed");
                return;
            }
        };
        let current = resp
            .extra
            .get("workspace")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .trim()
            .to_string();
        let workspaces_dir = self.registry.workspaces_dir();
        let desired = compute_desired_workspace_dir(&workspaces_dir, profile);
        let action = plan_workspace_reconcile(&current, &desired, &workspaces_dir);
        if let Some(new_path) = apply_workspace_action(action) {
            let new_str = new_path.to_string_lossy().into_owned();
            if new_str != current
                && let Err(e) = self
                    .conversations
                    .update_extra(conversation_id, serde_json::json!({ "workspace": new_str }))
                    .await
            {
                tracing::warn!(error = %e, conversation_id, "update companion workspace extra failed");
            }
        }
    }

    /// 该线程落盘工作区——仅当它位于 pretty 工作区树（解耦树）之下时返回。外来/temp/
    /// 空路径返回 None。
    /// 必须在删除会话「之前」读（删除会丢 extra）。
    async fn thread_workspace_under_tree(&self, conversation_id: &str) -> Option<std::path::PathBuf> {
        let resp = self
            .conversations
            .get(self.authoritative_user_id.as_ref(), conversation_id)
            .await
            .ok()?;
        let ws = resp.extra.get("workspace").and_then(|v| v.as_str())?.trim().to_string();
        if ws.is_empty() {
            return None;
        }
        let path = std::path::PathBuf::from(&ws);
        if path.starts_with(self.registry.workspaces_dir()) {
            Some(path)
        } else {
            None
        }
    }

    /// Idempotent ensure of the companion's SINGLE companion thread (work-partner
    /// single-session invariant): if the companion already has a live companion
    /// conversation, return it; only mint a new one when none exists. Minting
    /// requires the companion's `profile.model` to be configured (else BadRequest).
    /// `title` only applies when a brand-new thread is created.
    pub async fn create(&self, companion_id: &str, title: Option<String>) -> Result<CompanionThread, AppError> {
        let profile = self
            .registry
            .get(companion_id)
            .await
            .ok_or_else(|| AppError::NotFound(format!("companion '{companion_id}' not found")))?;
        // Single-session ensure: list (which prunes threads whose backing
        // conversation was deleted out-of-band) and reuse the survivor.
        if let Some(existing) = self.list(companion_id).await?.into_iter().next() {
            // 收敛工作区：首次补建 / 改名跟随（best-effort）。
            // 外来 temp cwd 仍留置不动（见 plan_workspace_reconcile 的 Leave 分支：
            // 移动 live cwd 会孤立已写文件）。新伙伴走下面的 create 分支直接落 pretty 名。
            self.reconcile_thread_workspace(&profile, &existing.conversation_id).await;
            let _ = set_active_thread_ptr(&self.store, companion_id, Some(&existing.conversation_id)).await;
            return Ok(existing);
        }
        let Some(model) = profile.model.as_ref() else {
            return Err(AppError::BadRequest("companion model not configured".into()));
        };
        let smart_collaboration = self.config.read().await.smart_collaboration;
        let system_prompt = build_companion_system_prompt(&self.store, &profile, None, smart_collaboration).await;
        let title = title
            .filter(|t| !t.trim().is_empty())
            .unwrap_or_else(|| format!("和 {} 聊天", profile.name));

        // 固定专属工作目录（见名知意）：{data_dir}/companion/workspaces/{seq}_{名字}。
        // 既是 agent 的 cwd，也是「聊天」Tab 里可浏览但锁定（不可改）的工作路径。
        // conversation.create 不会为用户提供的 workspace 建目录，必须在此 mkdir。
        let workspace_dir = compute_desired_workspace_dir(
            &self.registry.workspaces_dir(),
            &profile,
        );
        if let Err(e) = std::fs::create_dir_all(&workspace_dir) {
            tracing::warn!(error = %e, dir = %workspace_dir.display(), "create companion workspace dir failed");
        }
        let workspace = workspace_dir.to_string_lossy().into_owned();

        let req = CreateConversationRequest {
            r#type: nomifun_common::AgentType::Nomi,
            name: Some(title.clone()),
            model: Some(model.clone()),
            source: None,
            channel_chat_id: None,
            preset_id: None,
            preset_overrides: None,
            delegation_policy: Default::default(),
            execution_model_pool: None,
            decision_policy: Default::default(),
            execution_template_id: None,
            extra: {
                let extra = serde_json::json!({
                "companion_session": true,
                "companion_id": companion_id,
                "system_prompt": system_prompt,
                // `build_companion_system_prompt` already includes the frozen
                // preset instructions. Prevent the generic conversation path
                // from appending the same block a second time.
                "preset_instructions_embedded": true,
                // Fixed private work folder (locked, browsable in the chat tab's file
                // sidebar). Marks the conversation as a custom (non-temp) workspace, so
                // no skill symlinks are wired — the companion uses gateway tools, not skills.
                "workspace": workspace,
                // No explicit session_mode here: the Nomi factory defaults every
                // companion-owned session to "yolo" auto-approval (see
                // factory/nomi.rs) — the companion chat has no interactive
                // approval UI, so a tool call under Default mode would park forever
                // (聊天永久「思考中」). The companion's prompt is what guards destructive
                // ops (复述确认), not an approval gate.
                });
                extra
            },
        };
        let created = if let Some(snapshot) = profile.applied_preset.clone() {
            self.conversations
                .create_from_preset_snapshot(self.authoritative_user_id.as_ref(), req, snapshot)
                .await?
        } else {
            self.conversations.create(self.authoritative_user_id.as_ref(), req).await?
        };
        let created_id = created.conversation_id;
        // Register; if the registry write fails, reap the just-created
        // conversation — an unregistered companion row is invisible to every
        // surface (sidebar filters companion_session, thread list never shows it).
        let thread = match self.store.insert_companion_thread(&created_id, companion_id, &title).await {
            Ok(thread) => thread,
            Err(e) => {
                let _ = self
                    .conversations
                    .delete(self.authoritative_user_id.as_ref(), &created_id)
                    .await;
                return Err(e);
            }
        };
        let _ = set_active_thread_ptr(&self.store, companion_id, Some(&created_id)).await;
        Ok(thread)
    }

    /// List one companion's threads, pruning registry entries whose conversation
    /// was deleted out-of-band (e.g. via the conversation API). Also clears
    /// the companion's active pointer when it referenced a pruned thread.
    pub async fn list(&self, companion_id: &str) -> Result<Vec<CompanionThread>, AppError> {
        let mut threads = self.store.list_companion_threads(Some(companion_id)).await?;
        let mut pruned = Vec::new();
        let mut removed_ids: Vec<String> = Vec::new();
        for t in threads.drain(..) {
            match self
                .conversations
                .get(self.authoritative_user_id.as_ref(), &t.conversation_id)
                .await
            {
                // A companion session is valid only when it's a `nomi` conversation — the
                // companion chat UI (ChatTab/CompanionConversation) renders nomi only.
                Ok(resp) if resp.r#type == nomifun_common::AgentType::Nomi => pruned.push(t),
                // Missing (deleted out-of-band) OR type-mismatched (e.g. a stale `acp`
                // conversation left by a different build's ACP-companion feature, which this
                // nomi-only build can't render → "走神" with no chat). Drop the registry
                // pointer so `create` mints a fresh nomi session; the orphaned conversation
                // row stays hidden (extra.companion_session filters it from every list).
                Ok(_) | Err(AppError::NotFound(_)) => {
                    let _ = self.store.delete_companion_thread(&t.conversation_id).await;
                    removed_ids.push(t.conversation_id);
                }
                Err(_) => pruned.push(t), // transient error: keep listing
            }
        }
        if !removed_ids.is_empty()
            && let Ok(Some(active)) = active_thread_ptr(&self.store, companion_id).await
            && removed_ids.iter().any(|id| *id == active)
        {
            let _ = set_active_thread_ptr(&self.store, companion_id, None).await;
        }
        Ok(pruned)
    }

    pub async fn active_thread_id(&self, companion_id: &str) -> Result<Option<String>, AppError> {
        active_thread_ptr(&self.store, companion_id).await
    }

    /// Delete a thread: drop the registry row and the underlying conversation.
    pub async fn delete(&self, companion_id: &str, conversation_id: &str) -> Result<(), AppError> {
        self.assert_owned(companion_id, conversation_id).await?;
        // 删会话会丢 extra，先抓 pretty 树内的工作区路径以便显式清理。
        let workspace = self.thread_workspace_under_tree(conversation_id).await;
        // Conversation first (kills the running agent via delete hooks);
        // tolerate already-deleted rows.
        match self
            .conversations
            .delete(self.authoritative_user_id.as_ref(), conversation_id)
            .await
        {
            Ok(()) | Err(AppError::NotFound(_)) => {}
            Err(e) => return Err(e),
        }
        self.store.delete_companion_thread(conversation_id).await?;
        if active_thread_ptr(&self.store, companion_id).await?.as_deref() == Some(conversation_id) {
            let _ = set_active_thread_ptr(&self.store, companion_id, None).await;
        }
        if let Some(ws) = workspace {
            match std::fs::remove_dir_all(&ws) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => tracing::warn!(error = %e, ws = %ws.display(), "remove companion workspace dir failed"),
            }
        }
        Ok(())
    }

    /// Propagate the companion's model (唯一事实源 = profile.model) onto its single
    /// companion conversation ROW so the next turn uses the new model. The
    /// conversation row `model` was only a create-time snapshot; this keeps it
    /// in sync after a `PATCH /api/companion/companions/{id}` model change. Idempotent and
    /// best-effort at the call site. `companion_id` must own the thread.
    pub async fn set_model(
        &self,
        companion_id: &str,
        conversation_id: &str,
        model: &ProviderWithModel,
    ) -> Result<(), AppError> {
        self.assert_owned(companion_id, conversation_id).await?;
        self.conversations
            .update(
                self.authoritative_user_id.as_ref(),
                conversation_id,
                nomifun_api_types::UpdateConversationRequest {
                    name: None,
                    pinned: None,
                    model: Some(ProviderWithModel {
                        provider_id: model.provider_id.clone(),
                        model: model.model.clone(),
                        use_model: model.use_model.clone(),
                    }),
                    delegation_policy: None,
                    execution_model_pool: None,
                    decision_policy: None,
                    execution_template_id: None,
                    extra: None,
                },
                &self.runtime_registry,
            )
            .await
            .map(|_| ())
    }

    /// Replace only the reusable preset-derived portion of an existing
    /// companion thread. The companion id, memory, history, workspace and
    /// process-issued platform capability stays untouched.
    pub async fn set_preset(
        &self,
        companion_id: &str,
        conversation_id: &str,
        system_prompt: String,
        snapshot: &nomifun_api_types::ResolvedPresetSnapshot,
    ) -> Result<(), AppError> {
        self.assert_owned(companion_id, conversation_id).await?;
        self.conversations
            .update(
                self.authoritative_user_id.as_ref(),
                conversation_id,
                nomifun_api_types::UpdateConversationRequest {
                    name: None,
                    pinned: None,
                    model: None,
                    delegation_policy: None,
                    execution_model_pool: None,
                    decision_policy: None,
                    execution_template_id: None,
                    extra: Some(serde_json::json!({
                        "system_prompt": system_prompt,
                        "preset_instructions_embedded": true,
                        "preset_id": snapshot.preset_id.clone(),
                        "preset_revision": snapshot.preset_revision,
                        "preset_snapshot": snapshot,
                        "skills": snapshot.included_skills.clone(),
                        "exclude_auto_inject_skills": snapshot.excluded_auto_skills.clone(),
                        "preset_knowledge_binding": true,
                    })),
                },
                &self.runtime_registry,
            )
            .await
            .map(|_| ())
    }
}

/// `CompanionMemorySink` implementation over the shared companion store — the
/// bridge that gives the real agent engine access to the companions' memories and
/// activity feed. Memories are shared; only the save XP is attributed to the
/// owning companion (thread registry lookup, falling back to the default companion).
pub struct CompanionStoreSink {
    pub store: CompanionStore,
    pub config: SharedConfig,
    pub emitter: CompanionEventEmitter,
    pub companion_dir: std::path::PathBuf,
}

impl CompanionStoreSink {
    /// The companion a save should credit: the thread's owner, else the default
    /// companion, else nobody (XP skipped).
    async fn xp_target(&self, conversation_id: &str) -> Option<String> {
        if let Ok(Some(companion_id)) = self.store.thread_companion_id(conversation_id).await {
            return Some(companion_id);
        }
        self.config.read().await.default_companion_id.clone()
    }
}

/// Mirror a companion `save` into the nomi agent's file-memory at `dir` (the
/// §3.4 "消两库割裂" bridge). Best-effort; the deterministic content hash gives a
/// stable filename so re-saving the same fact overwrites rather than duplicates.
/// Companion memories are about the user, so they map to `MemoryType::User`.
fn mirror_memory_to_nomi(dir: &std::path::Path, kind: &str, content: &str) -> std::io::Result<()> {
    use std::hash::{Hash, Hasher};

    use nomi_memory::types::{MemoryEntry, MemoryFrontmatter, MemoryType};

    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    content.hash(&mut hasher);
    let name = format!("companion-{kind}-{:x}", hasher.finish());
    let title: String = content.chars().take(60).collect();
    let frontmatter = MemoryFrontmatter {
        name: Some(name),
        description: Some(format!("[companion:{kind}] {title}")),
        memory_type: Some(MemoryType::User),
        usage_count: None,
        last_used: None,
    };
    let entry = MemoryEntry::new(frontmatter, content.to_string());
    let path = nomi_memory::store::write_memory(dir, &entry)
        .map_err(|e| std::io::Error::other(e.to_string()))?;
    let filename = path.file_name().and_then(|f| f.to_str()).unwrap_or_default();
    // Best-effort index update (a missing/locked index must not fail the mirror).
    let _ = nomi_memory::index::append_index_entry(&dir.join("MEMORY.md"), &title, filename, &title);
    Ok(())
}

#[async_trait]
impl CompanionMemorySink for CompanionStoreSink {
    async fn recall(&self, conversation_id: &str, query: &str, kind: Option<&str>, include_archived: bool) -> Result<String, String> {
        // Scope recall to the owning companion: shared memories + this
        // companion's own private ones. Mirrors the prompt-injection scope so a
        // companion never recalls another's private memories.
        let scope_companion_id = self.xp_target(conversation_id).await;
        let filter = MemoryFilter {
            kind: kind.map(str::to_owned),
            q: Some(query.to_owned()),
            status: if include_archived { None } else { Some("active".into()) },
            scope_companion_id,
            limit: 20,
            offset: 0,
        };
        let memories = self.store.list_memories(&filter).await.map_err(|e| e.to_string())?;
        if memories.is_empty() {
            return Ok("没有找到相关记忆。".into());
        }
        let mut out = String::new();
        for m in memories {
            out.push_str(&format!(
                "- [{}|{}|强度{:.0}%{}] {}\n",
                format_date(m.created_at),
                m.kind,
                m.strength * 100.0,
                if m.status == "archived" { "|已归档" } else { "" },
                m.content
            ));
        }
        Ok(out)
    }

    async fn save(&self, conversation_id: &str, kind: &str, content: &str, tags: &[String]) -> Result<String, String> {
        if !MEMORY_KINDS.contains(&kind) {
            return Err(format!("kind 必须是 {MEMORY_KINDS:?} 之一"));
        }
        let content = content.trim();
        if content.is_empty() {
            return Err("content 不能为空".into());
        }
        match self.store.find_similar_active(kind, content).await {
            Ok(Some(_)) => return Ok("已有相似记忆，无需重复保存。".into()),
            Ok(None) => {}
            Err(e) => return Err(e.to_string()),
        }
        // The companion that owns this chat owns the memory. Chat-saved memories
        // are PRIVATE to that companion; if no owner resolves, fall back to shared.
        let owner = self.xp_target(conversation_id).await;
        let scope = owner.clone().map(MemoryScope::Companion).unwrap_or(MemoryScope::Shared);
        let mem = self
            .store
            .insert_memory_scoped(kind, content, tags, 0.8, "chat", scope)
            .await
            .map_err(|e| e.to_string())?;
        // Exclusive-interaction XP: credit the owning companion only (spec ruling
        // 2). Chat-saved memories are PRIVATE to that companion; the learner hub
        // still writes shared memory.
        if let Some(companion_id) = &owner {
            let _ = self.store.add_companion_xp(companion_id, 5).await;
        }
        self.emitter.emit_memory_created(&mem);
        // §3.4 bridge (opt-in): mirror the save into the nomi agent's file-memory
        // so it recalls companion-learned facts. Off unless an operator sets a
        // target dir; best-effort so a mirror failure never fails the save.
        let bridge_dir = self.config.read().await.bridge_to_memory_dir.clone();
        if let Some(dir) = bridge_dir.filter(|d| !d.trim().is_empty()) {
            if let Err(e) = mirror_memory_to_nomi(std::path::Path::new(&dir), kind, content) {
                tracing::warn!(target: "nomifun_companion", error = %e, "companion→nomi memory bridge write failed");
            }
        }
        Ok(format!("已保存记忆（{kind}）：{content}"))
    }

    async fn recent_events(&self, limit: usize) -> Result<String, String> {
        let events = collector::read_recent_events(&self.companion_dir, limit)
            .map_err(|error| error.to_string())?;
        if events.is_empty() {
            return Ok("最近没有采集到事件（采集可能未开启）。".into());
        }
        use chrono::TimeZone;
        let mut out = String::new();
        for e in &events {
            let ts = chrono::Local
                .timestamp_millis_opt(e.ts)
                .single()
                .map(|d| d.format("%m-%d %H:%M").to_string())
                .unwrap_or_default();
            let brief = e
                .data
                .get("content")
                .and_then(|c| c.as_str())
                .map(|s| s.chars().take(80).collect::<String>())
                .unwrap_or_else(|| e.name.clone());
            out.push_str(&format!("- [{ts}|{}] {brief}\n", e.source));
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::PersonaConfig;
    use crate::profile::SharedCompanionConfig;
    use nomifun_realtime::BroadcastEventBus;
    use tokio::sync::RwLock;

    fn companion_fixture(sequence: u64) -> String {
        let raw = format!("0190f5fe-7c00-7a00-8abc-{sequence:012}");
        nomifun_common::CompanionId::try_from(raw.as_str()).unwrap().into_string()
    }

    fn conversation_fixture(sequence: u64) -> String {
        let raw = format!("0190f5fe-7c00-7a00-8abc-{sequence:012}");
        nomifun_common::ConversationId::try_from(raw.as_str()).unwrap().into_string()
    }

    #[test]
    fn mirror_memory_to_nomi_writes_a_file_and_indexes_it() {
        let dir = tempfile::tempdir().unwrap();
        mirror_memory_to_nomi(dir.path(), "preference", "用户偏好用 pnpm 而非 npm").unwrap();

        // A memory file was written and references the content.
        let files: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.path().extension().map(|x| x == "md").unwrap_or(false))
            .filter(|e| e.file_name() != "MEMORY.md")
            .collect();
        assert_eq!(files.len(), 1, "exactly one memory file written");
        let body = std::fs::read_to_string(files[0].path()).unwrap();
        assert!(body.contains("pnpm"), "memory body carries the content: {body}");
        assert!(body.contains("companion:preference"), "frontmatter notes the companion kind");

        // MEMORY.md index references it.
        let index = std::fs::read_to_string(dir.path().join("MEMORY.md")).unwrap_or_default();
        assert!(index.contains("pnpm"), "index references the bridged memory: {index}");

        // Deterministic name → re-saving the same fact overwrites (no duplicate).
        mirror_memory_to_nomi(dir.path(), "preference", "用户偏好用 pnpm 而非 npm").unwrap();
        let count = std::fs::read_dir(dir.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.path().extension().map(|x| x == "md").unwrap_or(false))
            .filter(|e| e.file_name() != "MEMORY.md")
            .count();
        assert_eq!(count, 1, "same content must not duplicate the memory file");
    }

    fn sink(dir: &std::path::Path, store: CompanionStore, config: SharedCompanionConfig) -> CompanionStoreSink {
        CompanionStoreSink {
            store,
            config: Arc::new(RwLock::new(config)),
            emitter: CompanionEventEmitter::new(Arc::new(BroadcastEventBus::new(16)), "owner-a"),
            companion_dir: dir.to_path_buf(),
        }
    }

    #[tokio::test]
    async fn sink_save_recall_roundtrip_with_dedup() {
        let dir = tempfile::tempdir().unwrap();
        let store = CompanionStore::open_memory().await.unwrap();
        let owned_conversation = conversation_fixture(1);
        let unknown_conversation = conversation_fixture(2);
        let owner = companion_fixture(1);
        let default_companion = companion_fixture(2);
        store.insert_companion_thread(&owned_conversation, &owner, "聊").await.unwrap();
        let mut config = SharedCompanionConfig::default();
        config.default_companion_id = Some(default_companion.clone());
        let s = sink(dir.path(), store.clone(), config);

        let saved = s.save(&owned_conversation, "preference", "主人喜欢先结论后细节", &[]).await.unwrap();
        assert!(saved.contains("已保存"));
        assert_eq!(store.count_memories("active").await.unwrap(), 1);
        // XP credited to the owning companion, not the default and not globally.
        assert_eq!(store.get_companion_state_i64(&owner, "xp").await.unwrap(), 5);
        assert_eq!(store.get_companion_state_i64(&default_companion, "xp").await.unwrap(), 0);
        assert_eq!(store.get_state_i64("xp").await.unwrap(), 0);

        let dup = s.save(&owned_conversation, "preference", "主人喜欢先结论后细节", &[]).await.unwrap();
        assert!(dup.contains("相似"));
        assert_eq!(store.count_memories("active").await.unwrap(), 1);
        assert_eq!(store.get_companion_state_i64(&owner, "xp").await.unwrap(), 5);

        // Unregistered conversation falls back to the default companion.
        let other = s.save(&unknown_conversation, "knowledge", "cargo check 是门禁", &[]).await.unwrap();
        assert!(other.contains("已保存"));
        assert_eq!(store.get_companion_state_i64(&default_companion, "xp").await.unwrap(), 5);

        let hits = s.recall(&owned_conversation, "结论", None, false).await.unwrap();
        assert!(hits.contains("先结论后细节"));
        let miss = s.recall(&owned_conversation, "不存在xyz", None, false).await.unwrap();
        assert!(miss.contains("没有找到"));

        assert!(s.save(&owned_conversation, "bogus", "x", &[]).await.is_err());
        assert!(s.save(&owned_conversation, "task", "  ", &[]).await.is_err());
    }

    #[tokio::test]
    async fn sink_save_without_any_companion_skips_xp() {
        let dir = tempfile::tempdir().unwrap();
        let store = CompanionStore::open_memory().await.unwrap();
        let s = sink(dir.path(), store.clone(), SharedCompanionConfig::default());
        let saved = s.save(&conversation_fixture(3), "task", "明天修 bug", &[]).await.unwrap();
        assert!(saved.contains("已保存"));
        assert_eq!(store.get_state_i64("xp").await.unwrap(), 0);
    }

    #[tokio::test]
    async fn sink_recent_events_reads_collected_files() {
        let dir = tempfile::tempdir().unwrap();
        let store = CompanionStore::open_memory().await.unwrap();
        let s = sink(dir.path(), store, SharedCompanionConfig::default());
        assert!(s.recent_events(5).await.unwrap().contains("没有采集到"));

        collector::append_event(
            dir.path(),
            &collector::CollectedEvent {
                event_id: nomifun_common::generate_id(),
                ts: nomifun_common::now_ms(),
                source: "chat_user_messages".into(),
                name: "message.userCreated".into(),
                data: serde_json::json!({"content": "帮我看看 Rust 编译错误"}),
            },
        )
        .unwrap();
        assert!(s.recent_events(5).await.unwrap().contains("Rust 编译错误"));
    }

    #[tokio::test]
    async fn companion_system_prompt_does_not_force_a_reply_language() {
        // Regression (always-Chinese bug): the persona prompt must NOT pin a
        // reply language. The old hardcoded 「你和主人对话时用中文」 made the
        // companion answer in Chinese regardless of the app language. Reply
        // language is now decided at agent-build time from the app setting
        // (nomifun-ai-agent::factory::nomi), so baking it here would freeze it
        // into the persisted prompt and reintroduce the bug.
        let store = CompanionStore::open_memory().await.unwrap();
        let profile = CompanionProfileConfig::new("毛球", "ink", 1);
        for platform in [None, Some("telegram")] {
            let prompt = build_companion_system_prompt(&store, &profile, platform, false).await;
            assert!(
                !prompt.contains("用中文"),
                "{platform:?}: persona must not force Chinese: {prompt}"
            );
            // The language-neutral persona guidance is retained.
            assert!(prompt.contains("回复简洁直接，先结论后细节"), "{platform:?}");
        }
    }

    #[tokio::test]
    async fn companion_system_prompt_uses_profile_name_persona_and_memories() {
        let store = CompanionStore::open_memory().await.unwrap();
        store
            .insert_memory("preference", "主人喜欢中文回复", &[], 0.9, "learn")
            .await
            .unwrap();
        let mut profile = CompanionProfileConfig::new("毛球", "ink", 1);
        profile.persona = PersonaConfig {
            preset: "calm".into(),
            custom: "叫主人「老大」".into(),
        };
        let prompt = build_companion_system_prompt(&store, &profile, None, false).await;
        assert!(prompt.contains("你是 毛球"));
        assert!(!prompt.contains("你是 nomi"));
        assert!(prompt.contains("沉稳温柔"));
        assert!(prompt.contains("老大"));
        // The owner-custom persona block must precede the knowledge-curation
        // section — otherwise the custom text hangs under that heading.
        assert!(
            prompt.find("老大").unwrap() < prompt.find("## 知识沉淀技巧").unwrap(),
            "persona custom must come before the knowledge-curation section"
        );
        assert!(prompt.contains("主人喜欢中文回复"));
        assert!(prompt.contains("save_memory"));
        // Anti-replay guardrails: dated memories + the "history snapshot, do
        // not re-execute" clause (covering recall_memories tool results too)
        // + the dedup/config behavior rules.
        let today = format_date(nomifun_common::now_ms());
        assert!(prompt.contains(&format!("[{today}|preference]")), "memories must carry their date");
        assert!(prompt.contains("禁止据此主动创建"));
        assert!(
            prompt.contains("recall_memories 等工具检索到的结果"),
            "guardrail must explicitly cover tool-retrieved memories"
        );
        assert!(prompt.contains("list 工具查重"));
        assert!(prompt.contains("合理默认"));
    }

    #[tokio::test]
    async fn companion_system_prompt_injects_recent_day_digests_local_only() {
        let store = CompanionStore::open_memory().await.unwrap();
        let profile = CompanionProfileConfig::new("毛球", "ink", 1);
        // Seed one archived day-digest for this companion.
        let w = store.ensure_open_window(&profile.companion_id, &conversation_fixture(5), 0).await.unwrap();
        store
            .close_window(
                &w.session_window_id,
                "archived",
                Some("今天陪主人修了一下午 Rust 编译错误"),
                None,
                20,
            )
            .await
            .unwrap();

        // Local session gets the day-digest recap block.
        let local = build_companion_system_prompt(&store, &profile, None, false).await;
        assert!(local.contains("最近的会话回顾"), "local prompt must inject recent day-digests");
        assert!(local.contains("今天陪主人修了一下午 Rust 编译错误"));

        // Remote (IM) prompt stays identity-only — no day-digest recap.
        let remote = build_companion_system_prompt(&store, &profile, Some("telegram"), false).await;
        assert!(!remote.contains("最近的会话回顾"), "remote prompt must not inject day-digests");
    }

    #[tokio::test]
    async fn companion_system_prompt_smart_collaboration_nudge_local_only() {
        let store = CompanionStore::open_memory().await.unwrap();
        let profile = CompanionProfileConfig::new("毛球", "ink", 1);

        // Off → no collaboration nudge.
        let off = build_companion_system_prompt(&store, &profile, None, false).await;
        assert!(!off.contains("协作 Agent"), "no nudge when smart collaboration is off");

        // On + local → nudge present, teaching the unified delegation surface.
        let on = build_companion_system_prompt(&store, &profile, None, true).await;
        assert!(on.contains("协作 Agent"), "local prompt must teach Agent delegation when enabled");
        assert!(on.contains("nomi_delegate"));
        assert!(on.contains("strategy=parallel"));
        assert!(on.contains("strategy=planned"));
        assert!(on.contains("nomi_execution_get"));
        assert!(on.contains("nomi_execution_update"));

        // On + remote → still no nudge (collaboration tools deny Remote).
        let remote = build_companion_system_prompt(&store, &profile, Some("telegram"), true).await;
        assert!(!remote.contains("协作 Agent"), "remote must never get the collaboration nudge");
    }

    #[tokio::test]
    async fn local_prompt_teaches_terminal_tools_remote_does_not() {
        // Local companion threads carry the terminal domain (nomi_terminal_*),
        // so their prompt must teach driving PTY sessions. Remote (IM) sessions
        // run PROFILE_LITE with no terminal domain, so they must NOT be taught
        // the terminal tools (would advertise capabilities that hard-deny).
        let store = CompanionStore::open_memory().await.unwrap();
        let profile = CompanionProfileConfig::new("毛球", "ink", 1);
        let local = build_companion_system_prompt(&store, &profile, None, false).await;
        assert!(local.contains("nomi_terminal_send"), "local prompt must teach terminal send");
        assert!(local.contains("nomi_terminal_read_output"));

        let remote = build_companion_system_prompt(&store, &profile, Some("wecom"), false).await;
        assert!(!remote.contains("nomi_terminal_send"), "remote (IM) prompt must not teach terminal tools");
    }

    #[tokio::test]
    async fn companion_system_prompt_teaches_knowledge_curation() {
        let store = CompanionStore::open_memory().await.unwrap();
        let profile = CompanionProfileConfig::new("毛球", "ink", 1);
        // Both local companion threads and remote (IM) master sessions carry
        // the gateway tools, so both flavors must teach the curation flow.
        for platform in [None, Some("telegram")] {
            let prompt = build_companion_system_prompt(&store, &profile, platform, false).await;
            assert!(prompt.contains("## 知识沉淀技巧"), "{platform:?}");
            // The action sequence names every tool in pipeline order.
            let seq = ["nomi_knowledge_create_base", "nomi_knowledge_write_file", "nomi_knowledge_autogen", "nomi_knowledge_set_binding"];
            let mut last = 0;
            for tool in seq {
                let pos = prompt.find(tool).unwrap_or_else(|| panic!("{platform:?}: prompt must mention {tool}"));
                assert!(pos > last, "{platform:?}: {tool} out of pipeline order");
                last = pos;
            }
            // Binding to itself uses kind="companion"; changes apply at next task start.
            assert!(prompt.contains("kind=\"companion\""), "{platform:?}");
            assert!(prompt.contains("下次任务"), "{platform:?}");
            // Division of labor: global memory vs knowledge bases.
            assert!(prompt.contains("闲聊琐事"), "{platform:?}");
        }
    }

    #[tokio::test]
    async fn remote_prompt_forbids_proactive_dispatch_and_filters_task_memories() {
        // Badcase 2: in remote (IM) mode the partner must NOT be framed as a
        // task-dispatching 总管家, must carry the hard no-proactive-action rule,
        // and the memory snapshot must drop task/episode entries (stale to-dos
        // that drive re-dispatch) while keeping identity/preference/knowledge.
        let store = CompanionStore::open_memory().await.unwrap();
        store.insert_memory("task", "上周让你做导出功能", &[], 0.9, "learn").await.unwrap();
        store.insert_memory("episode", "昨天聊了部署", &[], 0.9, "learn").await.unwrap();
        store.insert_memory("preference", "主人喜欢中文回复", &[], 0.9, "learn").await.unwrap();
        store.insert_memory("profile", "主人是 Rust 工程师", &[], 0.9, "learn").await.unwrap();
        let profile = CompanionProfileConfig::new("毛球", "ink", 1);

        let remote = build_companion_system_prompt(&store, &profile, Some("telegram"), false).await;
        // No proactive-dispatch framing.
        assert!(!remote.contains("总管家"), "remote must not frame the partner as 总管家");
        assert!(!remote.contains("nomi_send_to_conversation"), "remote must not advertise task dispatch");
        // The hard rule is present.
        assert!(remote.contains("除非主人在本轮消息中明确要求"));
        assert!(remote.contains("禁止依据历史记忆主动执行任何操作"));
        // Snapshot keeps stable kinds, drops task/episode.
        assert!(remote.contains("主人喜欢中文回复"));
        assert!(remote.contains("主人是 Rust 工程师"));
        assert!(!remote.contains("上周让你做导出功能"), "task memory must be filtered out of the remote snapshot");
        assert!(!remote.contains("昨天聊了部署"), "episode memory must be filtered out of the remote snapshot");

        // Local mode is unchanged: still the 总管家, full snapshot incl. task.
        let local = build_companion_system_prompt(&store, &profile, None, false).await;
        assert!(local.contains("总管家"), "local desktop companion stays the 总管家");
        assert!(local.contains("上周让你做导出功能"), "local snapshot still includes task memories");
    }

    #[tokio::test]
    async fn sink_recall_output_carries_memory_dates() {
        let dir = tempfile::tempdir().unwrap();
        let store = CompanionStore::open_memory().await.unwrap();
        store
            .insert_memory("task", "主人想做导出功能", &[], 0.8, "learn")
            .await
            .unwrap();
        let s = sink(dir.path(), store, SharedCompanionConfig::default());
        let hits = s.recall(&conversation_fixture(4), "导出", None, false).await.unwrap();
        let today = format_date(nomifun_common::now_ms());
        assert!(hits.contains(&format!("[{today}|task|")), "recall lines must be dated: {hits}");
    }

    #[tokio::test]
    async fn active_thread_pointer_is_isolated_per_companion() {
        let store = CompanionStore::open_memory().await.unwrap();
        let companion_a = companion_fixture(1);
        let companion_b = companion_fixture(2);
        let companion_unknown = companion_fixture(3);
        let conversation_a = conversation_fixture(1);
        let conversation_b = conversation_fixture(2);
        set_active_thread_ptr(&store, &companion_a, Some(&conversation_a)).await.unwrap();
        set_active_thread_ptr(&store, &companion_b, Some(&conversation_b)).await.unwrap();
        assert_eq!(active_thread_ptr(&store, &companion_a).await.unwrap().as_deref(), Some(conversation_a.as_str()));
        assert_eq!(active_thread_ptr(&store, &companion_b).await.unwrap().as_deref(), Some(conversation_b.as_str()));
        // Clearing one companion's pointer never touches the other's.
        set_active_thread_ptr(&store, &companion_a, None).await.unwrap();
        assert_eq!(active_thread_ptr(&store, &companion_a).await.unwrap(), None);
        assert_eq!(active_thread_ptr(&store, &companion_b).await.unwrap().as_deref(), Some(conversation_b.as_str()));
        // Unknown companions read back as unset.
        assert_eq!(active_thread_ptr(&store, &companion_unknown).await.unwrap(), None);
    }
}

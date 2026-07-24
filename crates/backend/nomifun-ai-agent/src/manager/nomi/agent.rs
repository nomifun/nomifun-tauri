use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use nomi_agent::bootstrap::AgentBootstrap;
use nomi_agent::companion_tools::{
    CompanionMemorySink, CompanionSkillContributor, CompanionSkillSink, CompanionSkillTool, ListRecentEventsTool,
    RecallMemoriesTool, SaveMemoryTool,
};
use nomi_agent::engine::AgentEngine;
use nomi_agent::knowledge_tools::{KnowledgeReadTool, KnowledgeSearchTool, KnowledgeWriteTool};
use nomi_agent::output::OutputSink;
use nomi_agent::requirement_tools::{RequirementCompleteTool, RequirementSink, RequirementUpdateStatusTool};
use nomi_agent::cron_tools::{CronCreateTool, CronDeleteTool, CronListTool, CronSink};
use nomi_agent::session::Session;
use nomi_config::config::{CliArgs, Config};
use nomi_mcp::manager::McpManager;
use nomi_protocol::commands::SessionMode;
#[cfg(feature = "browser-use")]
use nomi_protocol::events::ToolCategory;
use nomi_protocol::{ToolApprovalManager, ToolApprovalResult};
use nomi_types::message::ContentBlock;
use nomifun_api_types::{AgentModeResponse, SlashCommandItem};
use nomifun_common::{
    AgentKillReason, AgentType, AppError, Confirmation, ConversationStatus, ErrorChain, TimestampMs, now_ms,
};
use serde_json::Value;
use tokio::sync::{Mutex, Notify, broadcast};
use tracing::{debug, error, info};

use crate::runtime_state::AgentRuntimeState;
use crate::runtime_handle::SystemResourceNoticeDelivery;
use crate::capability::backend_output_sink::BackendOutputSink;
use crate::capability::backend_protocol_sink::BackendProtocolSink;
use crate::protocol::events::{AgentStreamEvent, TurnCompletedEventData, TurnStopReason};
use crate::protocol::send_error::AgentSendError;
use crate::types::{NomiResolvedConfig, SendMessageData};

use super::image_attachments::{ImageAttachmentError, load_image_blocks};

fn apply_provider_context_budget(config: &mut Config, context_limit: Option<u64>) {
    config.compact.context_window = nomi_config::compact::resolve_context_window(
        context_limit,
        config.compact.context_window,
    );
    config.max_tokens =
        nomi_config::compact::fit_context_budget(&mut config.compact, config.max_tokens);
}

struct TurnTeardownFence {
    pending: AtomicBool,
    changed: Notify,
}

impl TurnTeardownFence {
    fn new() -> Self {
        Self {
            pending: AtomicBool::new(false),
            changed: Notify::new(),
        }
    }

    fn begin(&self) {
        self.pending.store(true, Ordering::Release);
    }

    fn complete(&self) {
        self.pending.store(false, Ordering::Release);
        self.changed.notify_waiters();
    }

    async fn wait_until_clear(&self) {
        loop {
            let changed = self.changed.notified();
            if !self.pending.load(Ordering::Acquire) {
                return;
            }
            changed.await;
        }
    }
}

pub struct NomiAgentManager {
    runtime: AgentRuntimeState,
    backend_output_sink: Arc<BackendOutputSink>,
    engine: Mutex<AgentEngine>,
    /// Shared authority for every shell/tool process owned by this runtime.
    ///
    /// Kept outside the engine mutex so an explicit stop (which deliberately
    /// races an in-flight `execute_turn`) can fence process-tree teardown
    /// without first waiting for the turn to release the engine.
    process_supervisor: Option<Arc<nomi_process_runtime::ProcessSupervisor>>,
    /// Synchronous tombstone for an abnormal turn unwind. The old guard sets
    /// this before `send_message` can release `turn_gate`; a successor must not
    /// admit work until exact process cleanup and terminalization clear it.
    turn_teardown_fence: Arc<TurnTeardownFence>,
    /// Static slash command metadata captured at bootstrap so UI lookups do
    /// not wait behind an active `engine.execute_turn()` turn.
    slash_commands: Vec<SlashCommandItem>,
    /// Holds `Arc<McpManager>` instances alive for the duration of this agent's
    /// lifetime. The managers are not accessed after construction — they exist
    /// solely so their underlying MCP connections outlive the engine's event
    /// loop. Rust drops them here, in field-declaration order, after `engine`
    /// and `runtime` are dropped. See the explicit `Drop` impl below.
    #[allow(dead_code)] // intentional: lifetime-extension only; see Drop impl
    mcp_managers: Vec<Arc<McpManager>>,
    /// Main-process backstop for renewable loopback MCP capabilities. Bridge
    /// children revoke on clean exit; this guard covers abrupt child/runtime
    /// teardown and construction failure.
    loopback_capability_leases: nomifun_common::LoopbackCapabilityLeaseSet,
    approval_manager: Arc<ToolApprovalManager>,
    confirmations: Arc<std::sync::RwLock<Vec<Confirmation>>>,
    /// Durable per-turn cancellation token. Unlike `Notify`, cancellation is
    /// retained when kill arrives before `send_message` reaches its select.
    turn_cancel: std::sync::Mutex<tokio_util::sync::CancellationToken>,
    active_turn: Arc<std::sync::Mutex<Option<crate::runtime_state::AgentRuntimeTurn>>>,
    /// Serializes turn admission, steering admission, terminal transition and
    /// permanent task shutdown. Terminal cleanup and `steer()` must share this
    /// exact gate: checking Running and pushing into the inbox as two separate
    /// operations lets a late interjection escape into the next explicit turn.
    lifecycle_gate: Arc<std::sync::Mutex<()>>,
    /// Holds for the complete send lifecycle. A second send waits here and
    /// re-checks `closing` only after the active turn has unwound, preventing
    /// it from replacing the active turn's cancellation token.
    turn_gate: Mutex<()>,
    /// Permanent once `kill` is requested; prevents a raced clone from
    /// admitting another turn after runtime-registry eviction.
    closing: AtomicBool,
    /// Mid-turn steering interjections pushed by `steer()` and drained by the
    /// engine at its loop boundaries. Shared (clone of this Arc handed to the
    /// engine via `set_steering_inbox` each turn). Entries belong exclusively
    /// to the current `active_turn`; terminal transition and the next turn's
    /// admission clear leftovers under `lifecycle_gate`, so an old generation
    /// can never be replayed by a later explicit turn. `std::sync::Mutex` —
    /// locked only for brief push/drain, never across an await.
    steering_inbox: Arc<std::sync::Mutex<std::collections::VecDeque<String>>>,
    /// Trusted host resource state. This queue is mounted on the engine for
    /// every turn but is injected only through the provider's top-level system
    /// context, never through a user message. It persists while the runtime is
    /// idle so notices do not need to start a synthetic turn.
    system_resource_inbox: Arc<std::sync::Mutex<std::collections::VecDeque<String>>>,
    /// Target directory for post-session memory distillation. `None` =
    /// this session never distills (companion red line, or no base dir).
    /// Set once at construction.
    distill_dir: Option<PathBuf>,
    /// Optional attachment-read boundary for restricted sessions. This mirrors
    /// the native file tools' `write_root`: channel/remote/public sessions are
    /// confined to their workspace, while a local desktop session (`None`)
    /// may choose any absolute local file through the OS file picker.
    image_read_root: Option<PathBuf>,
    /// Provider config snapshot reused by the exact post-turn distillation child.
    distill_cfg: Arc<nomi_config::config::Config>,
    /// One-shot knowledge reminder prepended to the FIRST user turn of a session
    /// that has bound bases — keeps the retrieval protocol adjacent to the task
    /// (the system-prompt section alone is too far from the user message to
    /// reliably fire). Mirrors the ACP `KnowledgeContextHook`. `None` once
    /// consumed or when no bases are mounted.
    knowledge_prelude: std::sync::Mutex<Option<String>>,
    /// When set, each user message is augmented with auto-retrieved KB hits
    /// (proactive RAG) keyed on the message text. `(retrieval sink, bound kb_ids)`.
    knowledge_auto_rag: Option<(
        Arc<dyn nomi_agent::knowledge_tools::KnowledgeRetrievalSink>,
        Vec<nomifun_common::KnowledgeBaseId>,
    )>,
}

impl Drop for NomiAgentManager {
    fn drop(&mut self) {
        self.backend_output_sink.cancel_active_tool_calls(
            "The agent manager was dropped before this tool call reached a terminal state.",
        );
        self.loopback_capability_leases.revoke_all();
    }
}

/// Whether the knowledge_search tool should be registered for this session.
pub(crate) fn should_register_knowledge_search(
    has_sink: bool,
    kb_ids: &[nomifun_common::KnowledgeBaseId],
) -> bool {
    has_sink && !kb_ids.is_empty()
}

/// Whether the native knowledge_write (回血) tool should be registered: a
/// write-back sink was wired AND the session actually has bound bases to write
/// to. The factory only passes a sink when write-back is enabled on the
/// binding, so this also gates on the user's opt-in.
pub(crate) fn should_register_knowledge_write(
    has_sink: bool,
    bases: &[(nomifun_common::KnowledgeBaseId, String)],
) -> bool {
    has_sink && !bases.is_empty()
}

/// Tool name of the native knowledge write-back tool. Allow-listed past the
/// approval gate (DIRECT/STAGED writes go to the user's own managed base, and
/// companion/channel sessions have no confirmation UI), mirroring the companion
/// memory tools.
pub(crate) const KNOWLEDGE_WRITE_TOOL_NAME: &str = "knowledge_write";

/// Cap on race-tail re-runs within a single turn-claim. The race window is
/// sub-millisecond, so a tiny bound guarantees termination even if a steerer
/// pushes during every pass. Any leftover after the cap is absorbed by this
/// turn's terminal transition; steering is never transferred to another turn.
const MAX_STEERING_RACE_TAIL_RERUNS: usize = 3;

/// Atomically close one exact manager turn and absorb every steering entry
/// that was admitted for it.
///
/// `AgentRuntimeState` already fences terminal events by `AgentRuntimeTurn`,
/// but the steering queue lives outside that state. Without this outer gate a
/// steerer can observe Running, lose the race to `terminal`, and then enqueue
/// after the engine's final drain. The next explicit turn would mount the same
/// queue and execute stale work. Holding `lifecycle_gate` across the exact
/// active-turn check, runtime terminal transition, queue clear and owner
/// release makes that outcome impossible.
fn terminalize_exact_nomi_turn(
    runtime: &AgentRuntimeState,
    lifecycle_gate: &Arc<std::sync::Mutex<()>>,
    active_turn: &Arc<std::sync::Mutex<Option<crate::runtime_state::AgentRuntimeTurn>>>,
    steering_inbox: &Arc<std::sync::Mutex<std::collections::VecDeque<String>>>,
    turn: crate::runtime_state::AgentRuntimeTurn,
    terminal: impl FnOnce(
        &AgentRuntimeState,
        crate::runtime_state::AgentRuntimeTurn,
    ) -> bool,
) -> bool {
    let _lifecycle = lifecycle_gate
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let mut active = active_turn.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    if active.as_ref() != Some(&turn) {
        return false;
    }

    let emitted = terminal(runtime, turn);
    steering_inbox
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .clear();
    *active = None;
    emitted
}

/// Cap on automatic continuation passes when a model response is truncated
/// before the task is complete. This keeps the UX moving without letting a bad
/// prompt or provider loop spend unbounded tokens.
const MAX_TRUNCATION_AUTO_CONTINUES: usize = 2;

fn truncation_continuation_prompt(attempt: usize, max_attempts: usize, reason: &str) -> String {
    format!(
        "[Automatic continuation {attempt}/{max_attempts}]\n\
The previous pass reached {reason} before the user's request was fully delivered.\n\n\
Recovery mode:\n\
- Continue the same task from the last valid state. Do not restart unless necessary.\n\
- If a file write or tool argument was interrupted, do not repeat the same oversized call.\n\
- Do not call Write with a full large file in one call.\n\
- First create a small complete deliverable that satisfies the user request end-to-end.\n\
- Then improve it by using Bash, Edit, or Write to append or edit in chunks; keep each chunk small.\n\
- For HTML/CSS/JS deliverables, prefer a concise valid file first, then add sections/styles incrementally.\n\
- Before finalizing, verify the target file exists in the active workspace and briefly report what was created."
    )
}

/// Prepend the one-shot knowledge prelude to the first user turn, if present.
pub(crate) fn apply_knowledge_prelude(prelude: Option<String>, content: &str) -> String {
    match prelude {
        Some(p) if !p.is_empty() => format!("{p}\n\n{content}"),
        _ => content.to_owned(),
    }
}

/// Prepend auto-retrieved knowledge-base hits to the user's message so the model
/// has relevant domain context without first having to call `knowledge_search`
/// (proactive RAG). Pure; returns `content` unchanged when there are no hits.
pub(crate) fn prepend_knowledge_context(
    hits: &[nomi_agent::knowledge_tools::KnowledgeHit],
    content: String,
) -> String {
    if hits.is_empty() {
        return content;
    }
    let mut block = String::from(
        "[Relevant knowledge-base context, retrieved automatically for this message \
         — to open a full document, call knowledge_read with the exact opaque handle shown below; \
         copy the handle unchanged and do not rebuild it from the path:]\n",
    );
    for h in hits {
        block.push_str(&format!(
            "- {}/{} § {}\n  {}\n  handle: {}\n",
            h.kb_name, h.rel_path, h.heading, h.snippet, h.handle
        ));
    }
    format!("{block}\n{content}")
}

/// Normalize the engine's [`StopReason`](nomi_types::message::StopReason) into
/// the cross-backend [`TurnStopReason`] carried on `TurnCompleted` / `Finish`.
/// `EndTurn`/`ToolUse` are clean completions; `MaxTokens`/`MaxTurns` mean the
/// turn was truncated and did NOT accomplish its goal (AutoWork / IDMM read
/// this to avoid treating a capped turn as success).
pub(crate) fn map_engine_stop_reason(
    reason: nomi_types::message::StopReason,
) -> TurnStopReason {
    use nomi_types::message::StopReason;
    match reason {
        StopReason::EndTurn | StopReason::ToolUse => TurnStopReason::EndTurn,
        StopReason::MaxTokens => TurnStopReason::MaxTokens,
        StopReason::MaxTurns => TurnStopReason::MaxTurnRequests,
    }
}

impl NomiAgentManager {
    pub async fn new(
        conversation_id: String,
        workspace: String,
        config_extra: NomiResolvedConfig,
        resume_session: Option<Session>,
        requirement_sink: Option<Arc<dyn RequirementSink>>,
        companion_sink: Option<Arc<dyn CompanionMemorySink>>,
        knowledge_retrieval_sink: Option<Arc<dyn nomi_agent::knowledge_tools::KnowledgeRetrievalSink>>,
        knowledge_kb_ids: Vec<nomifun_common::KnowledgeBaseId>,
        knowledge_prelude: Option<String>,
        knowledge_writeback_sink: Option<Arc<dyn nomi_agent::knowledge_tools::KnowledgeWritebackSink>>,
        knowledge_write_bases: Vec<(nomifun_common::KnowledgeBaseId, String)>,
        knowledge_writeback_staged: bool,
        companion_skill_sink: Option<Arc<dyn CompanionSkillSink>>,
    ) -> Result<Self, AppError> {
        let runtime = AgentRuntimeState::new(conversation_id.clone(), workspace.clone(), 128);
        let loopback_capability_leases = config_extra.loopback_capability_leases.clone();
        let image_read_root = config_extra
            .write_root
            .as_deref()
            .map(str::trim)
            .filter(|root| !root.is_empty())
            .map(PathBuf::from);

        // Companion red line: companion-companion sessions (companion_sink present)
        // NEVER distill into file-based memory — their persona memory belongs
        // to the companion SQLite store + learner. Otherwise the target is the
        // project-level auto-memory dir (same resolution as the engine's
        // bootstrap `auto_memory_dir(cwd)`). A run-time origin check in
        // `send_message` is the second gate (cron/autowork/idmm turns).
        let distill_dir: Option<PathBuf> = if companion_sink.is_some() {
            None
        } else {
            nomi_memory::paths::auto_memory_dir(std::path::Path::new(&workspace))
        };

        let backend_output_sink = Arc::new(
            BackendOutputSink::new(runtime.event_sender())
                .with_distill_dir(distill_dir.clone())
                .with_artifact_workspace(&workspace),
        );
        let sink: Arc<dyn OutputSink> = backend_output_sink.clone();

        let cli_args = CliArgs {
            provider: Some(config_extra.provider.clone()),
            api_key: Some(config_extra.api_key.clone()),
            base_url: config_extra.base_url.clone(),
            model: Some(config_extra.model.clone()),
            max_tokens: Some(config_extra.max_tokens),
            max_turns: config_extra.max_turns,
            system_prompt: config_extra.system_prompt.clone(),
            profile: None,
            auto_approve: config_extra.session_mode.as_deref() == Some("yolo"),
            project_dir: Some(PathBuf::from(&workspace)),
        };

        let mut config =
            Config::resolve(&cli_args).map_err(|e| AppError::Internal(format!("Config resolve failed: {e}")))?;

        // Backend-specific overrides
        config.bedrock = config_extra.bedrock_config;
        config.session.enabled = true;
        config.session.directory = config_extra.session_directory.to_string_lossy().into_owned();

        if let Some(field) = config_extra.compat_overrides.max_tokens_field {
            config.compat.max_tokens_field = Some(field);
        }
        if let Some(path) = config_extra.compat_overrides.api_path {
            config.compat.api_path = Some(path);
        }
        if let Some(required) = config_extra.compat_overrides.require_reasoning_content {
            config.compat.require_reasoning_content = Some(required);
        }
        // 图片支持 override(主动剔除):工厂据 VisionUnsupportedRegistry 命中注入
        // Some(false),灌进 compat.supports_image → build_messages 发送时剔图。
        // None → 保持 Config::resolve 的默认(supports_image()==true),行为不变。
        if let Some(supports_image) = config_extra.compat_overrides.supports_image {
            config.compat.supports_image = Some(supports_image);
        }

        // Make the engine compact against the provider's declared context
        // window when set (else keep the resolved default). Same value the
        // context-usage gauge reports as the denominator.
        apply_provider_context_budget(&mut config, config_extra.context_limit);

        if !config_extra.extra_mcp_servers.is_empty() {
            config.mcp.servers.extend(config_extra.extra_mcp_servers.clone());
        }

        // Session-level opt-in for desktop/browser automation tools. The
        // bootstrap registers them only when these flags are set.
        if config_extra.computer_use {
            config.tools.computer.enabled = true;
        }
        if config_extra.browser_use {
            config.tools.browser.enabled = true;
        }
        // Per-session 工具白名单（工厂已算好；bootstrap 的 retain_named
        // 会安装持久注册策略，后续 post-build / dynamic 工具也受同一策略约束）。
        // Embedded AgentExecution 的 host composition 不写入 ToolsConfig，
        // 而是在 bootstrap builder 上单独注入。
        config.tools.builtin_allowlist = config_extra.allowed_tools.clone();
        // 原生文件工具写根钳制（Write/Edit/ApplyPatch），按会话信任面由工厂解析：
        // 本地桌面 = None（不钳制，OS 用户全权，今日行为）；渠道/远程/对外 =
        // Some(workspace)（收窄到会话工作区）。仅在有非空值时覆盖，故桌面会话保留
        // Config::resolve 的默认（空 = 不钳制），且用户在 config.toml 里显式设置的
        // write_root 不被无谓清空。与 gateway file-service 的 PathAuthority 同源。
        if let Some(root) = config_extra.write_root.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
            config.tools.write_root = root.to_owned();
        }
        // F1-sec: 把会话的 evaluate「全权模式」LIVE 值灌进 BrowserConfig.full_power（bootstrap 据它
        // 构造 BrowserTool::with_policy 的 evaluate gate）。默认 false（default-deny）。
        config.tools.browser.full_power = config_extra.browser_full_power;
        // SD-6: 把会话的持久登录 LIVE 值灌进 BrowserConfig.persistent_login（bootstrap 据它
        // 构造 BrowserTool::with_policy 的 evaluate 互斥门）。产品默认 true（由 factory host_default 实现）。
        config.tools.browser.persistent_login = config_extra.browser_persistent_login;
        // P7A: 把会话的 site-memory LIVE 值灌进 BrowserConfig.site_memory（默认 OFF，opt-in）。
        // bootstrap 据它给 BrowserTool 注入文件型 SiteMemorySink（跨会话记住站点结构）。
        config.tools.browser.site_memory = config_extra.browser_site_memory;
        // P7B: 把会话的 visual-fallback LIVE 值灌进 BrowserConfig.visual_fallback（默认 OFF，opt-in）。
        // bootstrap 据它给 BrowserTool 注入会话模型的 VisualLocator（锚定失败时截图交视觉模型定位再点）。
        config.tools.browser.visual_fallback = config_extra.browser_visual_fallback;
        config.tools.browser.unrestricted_approval = config_extra.browser_unrestricted_approval;
        // 「浏览器模式」两开关（与上面的 opt-in 开关正交，每会话 LIVE）：
        // - 静默：silent(默认 false) → headless。facade 由 !headless 得 headful；无显示器时引擎本就强制 headless。
        // - 来源：source("managed"/"system") → facade 解析为 ChromeSource（system=系统 Chrome/Edge 优先，
        //   未探到回退 managed）。红线不变：两种来源都用专属 user-data-dir。
        config.tools.browser.headless = config_extra.browser_silent;
        config.tools.browser.source = config_extra.browser_source.clone();

        // Companion memory tools only touch the companion's own memory.db — never
        // user files — so they skip the approval gate in every session mode
        // (Default mode auto-approves nothing by category, which would park
        // every save_memory call on a confirmation the companion bubble can't show).
        if companion_sink.is_some() {
            config.tools.allow_list.extend([
                "recall_memories".to_owned(),
                "save_memory".to_owned(),
                "list_recent_events".to_owned(),
            ]);
        }
        // Companion self-evolved skill invocation (yolo, no approval UI) — must be
        // allow-listed BEFORE bootstrap or the call parks forever. Registration of
        // the tool + the per-turn skill ContextContributor happens after build().
        if companion_skill_sink.is_some() {
            config.tools.allow_list.push("companion_skill".to_owned());
        }

        // The native knowledge_write (回血) tool writes only into the user's own
        // bound knowledge base (DIRECT → base body; STAGED → review inbox) via
        // the backend service. Allow-list it so it bypasses the per-call approval
        // gate — under SessionMode::Default nothing is auto-approved, which would
        // park every write-back on a confirmation many surfaces (channel /
        // companion) cannot even show. Same posture as the companion memory
        // tools above. Must be set BEFORE bootstrap so it reaches the engine's
        // allow_list. Registration of the tool itself happens after build().
        let register_knowledge_write =
            should_register_knowledge_write(knowledge_writeback_sink.is_some(), &knowledge_write_bases);
        if register_knowledge_write {
            config.tools.allow_list.push(KNOWLEDGE_WRITE_TOOL_NAME.to_owned());
        }

        let is_resume = resume_session.is_some();
        let provider_label = config.provider_label.clone();
        let goal_spec = config_extra.goal.clone();

        // Snapshot the resolved provider config for the exact distillation child
        // (the engine consumes `config` next). Cheap one-time clone.
        let distill_cfg = Arc::new(config.clone());

        // P3-X1: create the session's shared approval manager BEFORE bootstrap, and apply the
        // initial session mode here, so the native BrowserTool (constructed inside
        // bootstrap.build) receives this same Arc and reads the LIVE runtime mode through it —
        // a mid-session set_mode to yolo then arms the facade redline gate immediately, not
        // pinned to the construction-time auto_approve snapshot. The same Arc is installed on
        // the engine via set_approval_manager below, so facade and tool execution share one cell.
        let approval_manager = Arc::new(ToolApprovalManager::new());
        if let Some(mode_str) = &config_extra.session_mode {
            let mode = parse_session_mode(mode_str);
            approval_manager.set_mode(mode);
            info!(
                conversation_id = %conversation_id,
                session_mode = mode_str,
                "Nomi initial session mode applied"
            );
        }

        // Phase D: the session's confirmation store, created BEFORE bootstrap so the desktop
        // approval gate can share the SAME Arc the `BackendProtocolSink` (below) uses. Holds
        // pending tool-approvals + browser takeover/egress approvals; the frontend renders
        // them (MessagePermission) and resolves via `confirm`.
        let confirmations = Arc::new(std::sync::RwLock::new(Vec::new()));

        let mut bootstrap = AgentBootstrap::new(config, &workspace, sink)
            .goal(goal_spec)
            .install_embedded_agent_execution(
                config_extra.install_embedded_agent_execution,
            )
            .approval_manager(approval_manager.clone());
        // P3-X2: thread the per-pet browser secret vault (path + machine-bound key) so the
        // native BrowserTool loads the registered credentials (`secret:NAME`, origin-gated)
        // and derives the firewall domain allowlist from their allowed_origins (裁决⑤). None
        // (browser-use off / no pet) → empty store + unrestricted egress (current behavior).
        if let Some(vault) = config_extra.browser_secret_vault {
            bootstrap = bootstrap.browser_secret_source(vault.vault_path, vault.key);
        }
        // Phase D: when the user opted into takeover/approval (`agent.browserUse.takeover`),
        // give bootstrap a desktop approval gate sharing the session's confirmation store +
        // approval manager — it surfaces irreversible actions / gated cross-origin POSTs
        // (SD-5) to the user via the existing confirmation UI and awaits a decision. Absent →
        // fail-closed (irreversible stays Blocked, gated egress fails). Threaded into the
        // native BrowserTool inside bootstrap (no-op if browser-use is off).
        #[cfg(feature = "browser-use")]
        if should_install_browser_approval_gate(
            config_extra.browser_takeover,
            config_extra.browser_unrestricted_approval,
            approval_manager.as_ref(),
        ) {
            let gate = crate::manager::nomi::browser_approval::DesktopApprovalGate::new(
                runtime.event_sender(),
                confirmations.clone(),
                approval_manager.clone(),
                config_extra.browser_unrestricted_approval,
            );
            bootstrap = bootstrap.approval_gate(Arc::new(gate));
        }
        if let Some(session) = resume_session {
            info!(
                conversation_id = %conversation_id,
                session_id = %session.id,
                message_count = session.messages.len(),
                "Resuming nomi session"
            );
            bootstrap = bootstrap.resume(session);
        }

        let result = bootstrap
            .build()
            .await
            .map_err(|e| AppError::Internal(format!("Agent bootstrap failed: {e}")))?;

        let mut engine = result.engine;
        if let Some(sink) = requirement_sink {
            engine
                .registry_mut()
                .register(Box::new(RequirementCompleteTool::new(
                    sink.clone(),
                    conversation_id.clone(),
                )));
            engine
                .registry_mut()
                .register(Box::new(RequirementUpdateStatusTool::new(
                    sink,
                    conversation_id.clone(),
                )));
            debug!(conversation_id = %conversation_id, "Registered requirement native tools");
        }
        if let Some(sink) = companion_sink {
            engine
                .registry_mut()
                .register(Box::new(RecallMemoriesTool::new(sink.clone(), conversation_id.clone())));
            engine
                .registry_mut()
                .register(Box::new(SaveMemoryTool::new(sink.clone(), conversation_id.clone())));
            engine
                .registry_mut()
                .register(Box::new(ListRecentEventsTool::new(sink)));
            debug!(conversation_id = %conversation_id, "Registered companion memory tools");
        }
        // Companion self-evolved skills (design §7): the native `companion_skill`
        // tool resolves a learned skill's body on demand, and the per-turn
        // ContextContributor injects the active skills' when_to_use index so the
        // model knows what it can invoke. Only present for companion sessions
        // (factory gates on overrides.companion). Empty skill set → the
        // contributor is a no-op (returns None each turn).
        if let Some(skill_sink) = companion_skill_sink {
            engine
                .registry_mut()
                .register(Box::new(CompanionSkillTool::new(skill_sink.clone())));
            engine.register_context_contributor(Arc::new(CompanionSkillContributor::new(skill_sink)));
            debug!(conversation_id = %conversation_id, "Registered companion skill tool + contributor");
        }
        // Capture a handle for proactive RAG before the sink/ids are consumed
        // by tool registration below (only when bound bases make search valid).
        let knowledge_auto_rag = knowledge_retrieval_sink
            .as_ref()
            .filter(|_| should_register_knowledge_search(true, &knowledge_kb_ids))
            .map(|s| (s.clone(), knowledge_kb_ids.clone()));
        if let Some(sink) = knowledge_retrieval_sink {
            if should_register_knowledge_search(true, &knowledge_kb_ids) {
                engine
                    .registry_mut()
                    .register(Box::new(KnowledgeSearchTool::new(sink.clone(), knowledge_kb_ids.clone())));
                engine
                    .registry_mut()
                    .register(Box::new(KnowledgeReadTool::new(sink, knowledge_kb_ids)));
                debug!(conversation_id = %conversation_id, "Registered knowledge_search + knowledge_read tools");
            }
        }
        // Native knowledge_write (回血): registered only when the binding has
        // write-back enabled (factory passes the sink) AND there are bound bases.
        // STAGED placement is encoded as `WriteMode::Staged { scope }` (the
        // service prepends `_inbox/{scope}/`); DIRECT writes the base body. The
        // tool was already added to the engine allow_list above so it bypasses
        // the approval gate. Placement enforcement now lives in the service
        // (write_document), so the tool only forwards the mode.
        if let Some(sink) = knowledge_writeback_sink {
            if register_knowledge_write {
                let mode = if knowledge_writeback_staged {
                    nomi_agent::knowledge_tools::WriteMode::Staged { scope: conversation_id.clone() }
                } else {
                    nomi_agent::knowledge_tools::WriteMode::Direct
                };
                let bound_kb_ids: Vec<nomifun_common::KnowledgeBaseId> =
                    knowledge_write_bases.iter().map(|(id, _)| id.clone()).collect();
                engine
                    .registry_mut()
                    .register(Box::new(KnowledgeWriteTool::new(sink, knowledge_write_bases, mode, bound_kb_ids)));
                debug!(
                    conversation_id = %conversation_id,
                    staged = knowledge_writeback_staged,
                    "Registered knowledge_write tool"
                );
            }
        }
        if !is_resume && let Err(e) = engine.init_session(&provider_label, &workspace, Some(&conversation_id)) {
            error!(
                conversation_id = %conversation_id,
                error = %ErrorChain(&*e),
                "Failed to init session, continuing without persistence"
            );
        }

        // Stamp the owning-conversation identity onto the session so a future
        // conversation that reuses this integer id cannot resume it (the factory
        // rejects a mismatching `owner_token` on load). Idempotent; no-op for a
        // resumed session the factory already migrated, and for None (no token).
        engine.stamp_owner_token(config_extra.owner_token.clone());

        let protocol_sink = BackendProtocolSink::new(runtime.event_sender(), confirmations.clone());
        engine.set_approval_manager(approval_manager.clone());
        engine.set_protocol_writer(Arc::new(protocol_sink));
        let slash_commands = engine
            .slash_command_list()
            .into_iter()
            .map(|(command, description)| SlashCommandItem { command, description })
            .collect();

        runtime.transition_to(ConversationStatus::Pending);
        let process_supervisor = engine.process_supervisor_handle();

        Ok(Self {
            runtime,
            backend_output_sink,
            engine: Mutex::new(engine),
            process_supervisor,
            turn_teardown_fence: Arc::new(TurnTeardownFence::new()),
            slash_commands,
            mcp_managers: result.mcp_managers,
            loopback_capability_leases,
            approval_manager,
            confirmations,
            turn_cancel: std::sync::Mutex::new(tokio_util::sync::CancellationToken::new()),
            active_turn: Arc::new(std::sync::Mutex::new(None)),
            lifecycle_gate: Arc::new(std::sync::Mutex::new(())),
            turn_gate: Mutex::new(()),
            closing: AtomicBool::new(false),
            steering_inbox: Arc::new(std::sync::Mutex::new(std::collections::VecDeque::new())),
            system_resource_inbox: Arc::new(std::sync::Mutex::new(
                std::collections::VecDeque::new(),
            )),
            distill_dir,
            image_read_root,
            distill_cfg,
            knowledge_prelude: std::sync::Mutex::new(knowledge_prelude),
            knowledge_auto_rag,
        })
    }

    fn request_stop(
        &self,
        reason: Option<AgentKillReason>,
        operation: &'static str,
        close_permanently: bool,
    ) -> bool {
        let _lifecycle = self
            .lifecycle_gate
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if close_permanently {
            self.closing.store(true, Ordering::Release);
        }
        let was_running = self.runtime.status() == Some(ConversationStatus::Running);
        let runtime_turn = *self.active_turn.lock().unwrap_or_else(|e| e.into_inner());

        self.turn_cancel
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .cancel();
        // Stop rejects all queued interjections for this generation. A steer
        // that races after this point takes the same lifecycle gate, observes
        // the cancelled token, and returns false instead of claiming delivery.
        self.steering_inbox
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clear();

        if let Ok(mut confs) = self.confirmations.write() {
            confs.clear();
        }

        if was_running {
            // The durable token above wakes the active turn's select branch.
            // That branch drops the in-flight engine/tool future before it
            // settles frontend tool state, so a late success cannot race a
            // cancellation Error.
        } else if !close_permanently {
            // Idle / Pending / between turns: there is no in-flight run to wake,
            // so notify_waiters would be a no-op AND no terminal event would ever
            // be broadcast — a relay subscribed to this conversation would hang
            // forever in a 'running' spinner. Emit the terminal event directly.
            // Idempotent via AgentRuntimeState's absorbing-state guard (a later real
            // Finish is absorbed). A later reusable turn receives a fresh token.
            self.backend_output_sink.cancel_active_tool_calls(
                "The tool call was cancelled before the turn could finish.",
            );
            if let Some(runtime_turn) = runtime_turn {
                self.runtime.emit_finish_for_turn(
                    runtime_turn,
                    None,
                    Some(TurnStopReason::Cancelled),
                );
            } else {
                // Initial Pending/idle managers have no accepted turn token,
                // but their stop contract still requires a terminal boundary.
                // Between real turns the absorbing Finished state makes this
                // a no-op, so it cannot terminate a later reset turn.
                self.runtime
                    .emit_finish_with_reason(None, Some(TurnStopReason::Cancelled));
            }
            *self.active_turn.lock().unwrap_or_else(|e| e.into_inner()) = None;
        }

        info!(
            conversation_id = %self.runtime.conversation_id(),
            ?reason,
            was_running,
            operation,
            "Nomi stop signal requested"
        );
        was_running
    }
}

#[async_trait::async_trait]
impl crate::runtime_handle::AgentRuntimeControl for NomiAgentManager {
    fn agent_type(&self) -> AgentType {
        AgentType::Nomi
    }

    fn conversation_id(&self) -> &str {
        self.runtime.conversation_id()
    }

    fn workspace(&self) -> &str {
        self.runtime.workspace()
    }

    fn status(&self) -> Option<ConversationStatus> {
        self.runtime.status()
    }

    fn is_transport_healthy(&self) -> bool {
        self.runtime.is_transport_healthy()
    }

    fn last_activity_at(&self) -> TimestampMs {
        self.runtime.last_activity_at()
    }

    fn touch_activity(&self) {
        self.runtime.bump_activity();
    }

    fn subscribe(&self) -> broadcast::Receiver<AgentStreamEvent> {
        self.runtime.subscribe()
    }

    async fn send_message(&self, data: SendMessageData) -> Result<(), AgentSendError> {
        let started_at = now_ms();
        info!(
            conversation_id = %self.runtime.conversation_id(),
            msg_id = %data.msg_id,
            "Nomi send_message started"
        );
        let _turn = self.turn_gate.lock().await;
        self.turn_teardown_fence.wait_until_clear().await;
        // Capture the exact process authority before admitting the new runtime
        // generation. The turn termination guard can then fence subprocesses
        // even if the engine future panics and its mutex guard unwinds.
        let process_supervisor = self.process_supervisor.clone();
        let (turn_cancel, runtime_turn) = {
            let _lifecycle = self
                .lifecycle_gate
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if self.closing.load(Ordering::Acquire) {
                return Err(AgentSendError::from_app_error(AppError::Conflict(
                    "Agent runtime is shutting down; retry on the replacement runtime".to_owned(),
                )));
            }
            // Backstop for abnormal teardown and data written by older builds:
            // a fresh explicit turn never inherits steering from a prior
            // generation. Normal terminalization performs the same clear.
            self.steering_inbox
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .clear();
            let token = tokio_util::sync::CancellationToken::new();
            *self
                .turn_cancel
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner()) = token.clone();
            self.runtime.bump_activity();
            let runtime_turn = self.runtime.reset_for_new_turn(ConversationStatus::Running);
            *self.active_turn.lock().unwrap_or_else(|e| e.into_inner()) = Some(runtime_turn);
            (token, runtime_turn)
        };

        // Backstop: guarantee a terminal event even if this turn unwinds
        // abnormally (engine panic / early-return). Disarmed on the normal path
        // after the real terminal event is emitted below. (Phase 0 F0.2)
        let mut term_guard = TurnTerminationGuard {
            runtime: self.runtime.clone(),
            turn: runtime_turn,
            active_turn: Arc::clone(&self.active_turn),
            lifecycle_gate: Arc::clone(&self.lifecycle_gate),
            steering_inbox: Arc::clone(&self.steering_inbox),
            backend_output_sink: self.backend_output_sink.clone(),
            process_supervisor: process_supervisor.clone(),
            mcp_managers: self.mcp_managers.clone(),
            turn_teardown_fence: Arc::clone(&self.turn_teardown_fence),
            armed: true,
        };

        // Every asynchronous operation after entering Running belongs to this
        // durable cancellation domain. Preparation and execution converge on
        // one cancellation terminal below.
        let accepted_turn = 'accepted: {
            let prepare_turn = async {
                // A provider already known not to support vision receives the
                // text turn unchanged. This capability lock and all attachment
                // work remain cancellable.
                let supports_image = self.engine.lock().await.compat().supports_image();
                let image_blocks = if supports_image {
                    load_image_blocks(&data.files, self.image_read_root.as_deref()).await?
                } else {
                    Vec::new()
                };

                // Proactive RAG is best-effort, but never allowed to make a
                // cancelled turn wait forever.
                let knowledge_hits = if let Some((sink, kb_ids)) = &self.knowledge_auto_rag {
                    match sink.search(kb_ids, &data.content, 3).await {
                        Ok(hits) if !hits.is_empty() => Some(hits),
                        _ => None,
                    }
                } else {
                    None
                };

                let engine = self.engine.lock().await;
                Ok::<_, ImageAttachmentError>((image_blocks, knowledge_hits, engine))
            };
            let preparation = tokio::select! {
                biased;
                _ = turn_cancel.cancelled() => break 'accepted None,
                prepared = prepare_turn => prepared,
            };

            let (image_blocks, knowledge_hits, mut engine) = match preparation {
                Ok(prepared) => prepared,
                Err(error) => {
                    let send_error = AgentSendError::from_app_error(AppError::BadRequest(format!(
                        "Invalid parameters: {error}"
                    )));
                    self.backend_output_sink.fail_active_tool_calls(
                        "The turn failed while loading its attachments.",
                    );
                    let stream_error = send_error.stream_error().clone();
                    term_guard.terminalize(move |runtime, turn| {
                        runtime.emit_error_data_for_turn(turn, stream_error)
                    });
                    return Err(send_error);
                }
            };

            // Consume the one-shot prelude only after every cancellable
            // preparation await has completed successfully.
            let prelude = self
                .knowledge_prelude
                .lock()
                .expect("knowledge_prelude lock poisoned")
                .take();
            let content = apply_knowledge_prelude(prelude, &data.content);
            let content = match knowledge_hits {
                Some(hits) => prepend_knowledge_context(&hits, content),
                None => content,
            };

            self.backend_output_sink.begin_artifact_delivery_turn();
            engine.set_steering_inbox(Some(self.steering_inbox.clone()));
            engine.set_system_resource_inbox(Some(self.system_resource_inbox.clone()));

            // Each iteration runs one engine pass inside the same accepted
            // Agent turn. Re-run only for steering race-tail interjections or
            // bounded output truncation continuation.
            let mut run_content = Vec::with_capacity(1 + image_blocks.len());
            run_content.push(ContentBlock::Text { text: content });
            run_content.extend(image_blocks);
            let mut race_tail_reruns = 0usize;
            let mut truncation_auto_continues = 0usize;
            let result = loop {
                let current_content = std::mem::take(&mut run_content);
                // Cancellation has one fail-closed lifecycle: drop the in-flight
                // engine/tool future immediately, then roll back the provisional
                // turn state. Awaiting arbitrary tool code here is unsafe because a
                // tool is not required to observe a cancellation token.
                let r = tokio::select! {
                    biased;
                    _ = turn_cancel.cancelled() => {
                        info!(
                            conversation_id = %self.runtime.conversation_id(),
                            "Nomi engine.execute_turn() cancelled by stop signal"
                        );
                        engine.abort_current_turn("Tool execution canceled by user");
                        engine.set_steering_inbox(None);
                        engine.set_system_resource_inbox(None);
                        break 'accepted None;
                    }
                    res = engine.execute_turn_with_content(current_content, &data.msg_id) => res,
                };

                // Race-tail: only a clean Ok can carry leftover steering worth a
                // re-run (a cancel/abort intentionally drops the turn). Bounded so a
                // continuous steerer cannot spin this forever; leftover past the cap
                // is absorbed by this exact turn's terminal transition.
                if let Ok(agent_result) = &r
                    && agent_result.stop_reason == nomi_types::message::StopReason::EndTurn
                {
                    if race_tail_reruns < MAX_STEERING_RACE_TAIL_RERUNS {
                        let leftover: Vec<String> = {
                            let mut q = self.steering_inbox.lock().unwrap_or_else(|e| e.into_inner());
                            q.drain(..).collect()
                        };
                        if !leftover.is_empty() {
                            race_tail_reruns += 1;
                            info!(
                                conversation_id = %self.runtime.conversation_id(),
                                count = leftover.len(),
                                "Nomi steering race-tail: re-running with leftover interjection(s)"
                            );
                            // NOTE: the re-run reuses `data.msg_id`, so the engine emits a
                            // second StreamStart under the same id for this logical turn.
                            // Benign — the UI keeps the same assistant bubble; a fresh id
                            // would instead spawn a new bubble. Intentional for this rare tail.
                            run_content = vec![ContentBlock::Text {
                                text: leftover.join("\n\n"),
                            }];
                            continue;
                        }
                    } else {
                        tracing::warn!(
                            conversation_id = %self.runtime.conversation_id(),
                            "Nomi steering race-tail cap reached; leftover belongs to this turn and will be discarded at terminal"
                        );
                    }
                }
                if let Ok(agent_result) = &r {
                    // Only a token-length truncation can be continued safely. A
                    // MaxTurns result means the model already consumed the full
                    // per-pass request budget; starting a fresh pass resets the
                    // engine's loop guard and used to multiply a deterministic
                    // tool loop by the host continuation cap.
                    if agent_result.stop_reason == nomi_types::message::StopReason::MaxTokens {
                        let reason = "the output token limit";
                        if truncation_auto_continues < MAX_TRUNCATION_AUTO_CONTINUES {
                            truncation_auto_continues += 1;
                            info!(
                                conversation_id = %self.runtime.conversation_id(),
                                attempt = truncation_auto_continues,
                                max_attempts = MAX_TRUNCATION_AUTO_CONTINUES,
                                stop_reason = ?agent_result.stop_reason,
                                "Nomi turn truncated; auto-continuing before Finish"
                            );
                            self.backend_output_sink
                                .truncate_active_tool_calls_for_auto_continue(reason);
                            run_content = vec![ContentBlock::Text {
                                text: truncation_continuation_prompt(
                                    truncation_auto_continues,
                                    MAX_TRUNCATION_AUTO_CONTINUES,
                                    reason,
                                ),
                            }];
                            continue;
                        }
                        tracing::warn!(
                            conversation_id = %self.runtime.conversation_id(),
                            attempts = truncation_auto_continues,
                            stop_reason = ?agent_result.stop_reason,
                            "Nomi truncation auto-continue cap reached; emitting final Finish"
                        );
                    }
                }
                break r;
            };

            engine.set_steering_inbox(None);
            engine.set_system_resource_inbox(None);
            Some((result, engine))
        };

        let Some((result, engine)) = accepted_turn else {
            self.backend_output_sink.cancel_active_tool_calls(
                "The tool call was cancelled because the user stopped the turn.",
            );
            // A stopped turn may have dropped an arbitrary hook/skill/tool
            // future. Its registered child process remains owned by the shared
            // supervisor, so do not publish the business terminal until every
            // such tree has an exact reap outcome. This fence intentionally
            // runs only for explicit/abnormal cancellation: a successful turn
            // may leave a user-requested background exec session alive.
            term_guard.fence_cancelled_processes().await?;
            term_guard.terminalize(|runtime, turn| {
                runtime.emit_finish_for_turn(
                    turn,
                    None,
                    Some(TurnStopReason::Cancelled),
                )
            });
            return Ok(());
        };

        let elapsed_ms = now_ms() - started_at;
        self.runtime.bump_activity();

        let outcome = match result {
            Ok(agent_result) => {
                let stop_reason = map_engine_stop_reason(agent_result.stop_reason);
                info!(
                    conversation_id = %self.runtime.conversation_id(),
                    elapsed_ms,
                    turns = agent_result.turns,
                    input_tokens = agent_result.usage.input_tokens,
                    output_tokens = agent_result.usage.output_tokens,
                    ?stop_reason,
                    "Nomi engine.execute_turn() completed; closing exact post-turn effects before Finish"
                );

                self.backend_output_sink.fail_active_tool_calls(&format!(
                    "The model turn ended with {stop_reason:?} before this tool call reached a terminal state."
                ));

                if let Err(delivery_error) = self
                    .backend_output_sink
                    .finish_artifact_delivery_turn()
                {
                    error!(
                        conversation_id = %self.runtime.conversation_id(),
                        elapsed_ms,
                        error = %delivery_error,
                        "Nomi turn ended without satisfying every artifact-delivery obligation"
                    );
                    let send_error = AgentSendError::from_app_error(AppError::Internal(format!(
                        "Artifact delivery failed: {delivery_error}"
                    )));
                    let stream_error = send_error.stream_error().clone();
                    term_guard.terminalize(move |runtime, turn| {
                        runtime.emit_error_data_for_turn(turn, stream_error)
                    });
                    return Err(send_error);
                }

                // Phase 3 observability: a per-turn metrics event the UI shows as
                // duration / token cost and telemetry records. Purely additive and
                // non-terminal — emitted via `emit()` so it does NOT flip the
                // absorbing Finished state before the real `Finish` below.
                let context_tokens = engine.context_tokens();
                let context_window = engine.context_window();
                self.runtime.emit(AgentStreamEvent::TurnCompleted(TurnCompletedEventData {
                    elapsed_ms,
                    input_tokens: agent_result.usage.input_tokens,
                    output_tokens: agent_result.usage.output_tokens,
                    cache_creation_tokens: agent_result.usage.cache_creation_tokens,
                    cache_read_tokens: agent_result.usage.cache_read_tokens,
                    context_tokens,
                    context_window,
                    stop_reason: Some(stop_reason),
                }));

                // —— Post-session memory distillation (exact turn child) ——
                // Eligibility gates, cheapest first:
                //   1. host opt-in flag (token cost; default off)
                //   2. this session distills at all (distill_dir set; companion
                //      red line already zeroed it at construction)
                //   3. run-time origin empty (cron/autowork/idmm turns excluded,
                //      same rule as the collector's payload_origin red line)
                // All satisfied → snapshot the just-saved transcript, release
                // the engine lock, then await the complete provider+apply
                // effect. Finish is forbidden until this child closes; stop
                // drops the provider future before it can reach the synchronous
                // apply stage. Distill failures remain best-effort and never
                // masquerade as a failed model turn.
                let origin_is_human = data
                    .origin
                    .as_deref()
                    .map(str::trim)
                    .unwrap_or("")
                    .is_empty();
                let distill_job = if origin_is_human
                    && super::distill::distill_enabled()
                    && let Some(dir) = self.distill_dir.clone()
                {
                    let transcript = engine.messages_transcript();
                    let cfg = self.distill_cfg.clone();
                    Some((cfg, dir, transcript))
                } else {
                    None
                };
                drop(engine); // never hold the engine mutex across the provider call

                let distill_completed = match distill_job {
                    Some((cfg, dir, transcript)) => {
                        super::distill::run_distill_exact_turn(
                            &turn_cancel,
                            cfg,
                            dir,
                            transcript,
                        )
                        .await
                    }
                    None => !turn_cancel.is_cancelled(),
                };
                if !distill_completed || turn_cancel.is_cancelled() {
                    self.backend_output_sink.cancel_active_tool_calls(
                        "The tool call was cancelled because the user stopped the turn.",
                    );
                    term_guard.fence_cancelled_processes().await?;
                    term_guard.terminalize(|runtime, turn| {
                        runtime.emit_finish_for_turn(
                            turn,
                            None,
                            Some(TurnStopReason::Cancelled),
                        )
                    });
                    return Ok(());
                }

                term_guard.terminalize(|runtime, turn| {
                    runtime.emit_finish_for_turn(turn, None, Some(stop_reason))
                });
                Ok(())
            }
            Err(e) => {
                let error_msg = format!("Nomi agent error: {e}");
                error!(
                    conversation_id = %self.runtime.conversation_id(),
                    elapsed_ms,
                    error = %ErrorChain(&e),
                    "Nomi engine.execute_turn() failed, emitting terminal Error"
                );
                let send_error = nomi_engine_error_to_send_error(error_msg);
                self.backend_output_sink.fail_active_tool_calls(&format!(
                    "The model/provider turn failed before this tool call completed: {e}"
                ));
                let stream_error = send_error.stream_error().clone();
                term_guard.terminalize(move |runtime, turn| {
                    runtime.emit_error_data_for_turn(turn, stream_error)
                });
                Err(send_error)
            }
        };

        // Every normal branch above atomically emitted its exact terminal and
        // absorbed that generation's remaining steering before reaching here.
        outcome
    }

    async fn cancel(&self) -> Result<(), AppError> {
        self.request_stop(None, "cancel", false);
        Ok(())
    }

    fn kill(&self, reason: Option<AgentKillReason>) -> Result<(), AppError> {
        let was_running = self.request_stop(reason, "kill", true);
        self.loopback_capability_leases.revoke_all();
        if !was_running {
            schedule_nomi_cancelled_terminal_after_process_fence(
                self.runtime.clone(),
                Arc::clone(&self.active_turn),
                Arc::clone(&self.lifecycle_gate),
                Arc::clone(&self.steering_inbox),
                self.backend_output_sink.clone(),
                self.process_supervisor.clone(),
                self.mcp_managers.clone(),
            )?;
        }
        Ok(())
    }
}

/// RAII backstop guaranteeing a terminal event is broadcast for a turn even if
/// `send_message` unwinds abnormally (engine panic / unexpected early-return).
/// On the normal path `send_message` emits the real terminal event and then
/// disarms the guard; on an abnormal unwind the still-armed guard fires on drop.
/// The emit is idempotent via `AgentRuntimeState`'s absorbing-state guard, so this can
/// never leak a spurious terminal event past a real one. (Phase 0 F0.2)
struct TurnTerminationGuard {
    runtime: AgentRuntimeState,
    turn: crate::runtime_state::AgentRuntimeTurn,
    active_turn: Arc<std::sync::Mutex<Option<crate::runtime_state::AgentRuntimeTurn>>>,
    lifecycle_gate: Arc<std::sync::Mutex<()>>,
    steering_inbox: Arc<std::sync::Mutex<std::collections::VecDeque<String>>>,
    backend_output_sink: Arc<BackendOutputSink>,
    process_supervisor: Option<Arc<nomi_process_runtime::ProcessSupervisor>>,
    mcp_managers: Vec<Arc<McpManager>>,
    turn_teardown_fence: Arc<TurnTeardownFence>,
    armed: bool,
}

impl TurnTerminationGuard {
    fn terminalize(
        &mut self,
        terminal: impl FnOnce(
            &AgentRuntimeState,
            crate::runtime_state::AgentRuntimeTurn,
        ) -> bool,
    ) -> bool {
        let emitted = terminalize_exact_nomi_turn(
            &self.runtime,
            &self.lifecycle_gate,
            &self.active_turn,
            &self.steering_inbox,
            self.turn,
            terminal,
        );
        self.armed = false;
        emitted
    }

    async fn fence_cancelled_processes(&self) -> Result<(), AppError> {
        shutdown_mcp_managers_exact(&self.mcp_managers).await?;
        let Some(supervisor) = &self.process_supervisor else {
            return Ok(());
        };
        let report = supervisor.quiesce().await;
        if report.is_exact() {
            Ok(())
        } else {
            Err(AppError::Internal(format!(
                "Nomi process-tree teardown for conversation {} was not exact: {}",
                self.runtime.conversation_id(),
                describe_quiesce_failure(&report),
            )))
        }
    }
}

impl Drop for TurnTerminationGuard {
    fn drop(&mut self) {
        if self.armed {
            // This store happens synchronously while the old send still owns
            // `turn_gate`. Therefore a queued successor can never slip between
            // the unwind and the asynchronous process-tree fence.
            self.turn_teardown_fence.begin();
            self.backend_output_sink.cancel_active_tool_calls(
                "The turn ended unexpectedly before this tool call reached a terminal state.",
            );
            let runtime = self.runtime.clone();
            let conversation_id = runtime.conversation_id().to_owned();
            let turn = self.turn;
            let active_turn = Arc::clone(&self.active_turn);
            let lifecycle_gate = Arc::clone(&self.lifecycle_gate);
            let steering_inbox = Arc::clone(&self.steering_inbox);
            let process_supervisor = self.process_supervisor.clone();
            let mcp_managers = self.mcp_managers.clone();
            let turn_teardown_fence = Arc::clone(&self.turn_teardown_fence);
            let terminalize = move || {
                terminalize_exact_nomi_turn(
                    &runtime,
                    &lifecycle_gate,
                    &active_turn,
                    &steering_inbox,
                    turn,
                    |runtime, turn| {
                        runtime.emit_finish_for_turn(
                            turn,
                            None,
                            Some(TurnStopReason::Cancelled),
                        )
                    },
                );
                turn_teardown_fence.complete();
            };

            let Some(supervisor) = process_supervisor else {
                terminalize();
                return;
            };
            let Ok(runtime_handle) = tokio::runtime::Handle::try_current() else {
                error!(
                    conversation_id = %self.runtime.conversation_id(),
                    "Nomi turn unwound outside a Tokio runtime; refusing to publish a terminal state before process-tree teardown"
                );
                return;
            };
            runtime_handle.spawn(async move {
                if let Err(error) = shutdown_mcp_managers_exact(&mcp_managers).await {
                    error!(
                        conversation_id = %conversation_id,
                        error = %error,
                        "Nomi turn MCP teardown was not exact; retaining non-terminal quarantine"
                    );
                    return;
                }
                let report = supervisor.quiesce().await;
                if !report.is_exact() {
                    error!(
                        conversation_id = %conversation_id,
                        failure = %describe_quiesce_failure(&report),
                        "Nomi turn process-tree teardown was not exact; retaining non-terminal quarantine"
                    );
                    return;
                }
                terminalize();
            });
        }
    }
}

async fn shutdown_mcp_managers_exact(
    managers: &[Arc<McpManager>],
) -> Result<(), AppError> {
    let mut failures = Vec::new();
    for manager in managers {
        if let Err(error) = manager.shutdown().await {
            failures.push(error.to_string());
        }
    }
    if failures.is_empty() {
        Ok(())
    } else {
        Err(AppError::Internal(format!(
            "Nomi MCP shutdown was not exact: {}",
            failures.join(" | ")
        )))
    }
}

fn describe_quiesce_failure(report: &nomi_process_runtime::QuiesceReport) -> String {
    let mut details = report.errors.clone();
    for session in &report.sessions {
        if let nomi_process_runtime::ProcessOutcome::Lost { cleanup, .. } = &session.outcome
            && !cleanup.reaped
        {
            details.push(format!(
                "session {} owner {}/{} was not reaped: {}",
                session.session_id,
                session.owner.invocation_id,
                session.owner.call_id,
                cleanup.errors.join("; "),
            ));
        }
    }
    if details.is_empty() {
        "one or more process sessions lack exact reap proof".to_owned()
    } else {
        details.join(" | ")
    }
}

fn schedule_nomi_cancelled_terminal_after_process_fence(
    runtime: AgentRuntimeState,
    active_turn: Arc<std::sync::Mutex<Option<crate::runtime_state::AgentRuntimeTurn>>>,
    lifecycle_gate: Arc<std::sync::Mutex<()>>,
    steering_inbox: Arc<std::sync::Mutex<std::collections::VecDeque<String>>>,
    backend_output_sink: Arc<BackendOutputSink>,
    process_supervisor: Option<Arc<nomi_process_runtime::ProcessSupervisor>>,
    mcp_managers: Vec<Arc<McpManager>>,
) -> Result<(), AppError> {
    let terminalize = move || {
        backend_output_sink.cancel_active_tool_calls(
            "The tool call was cancelled before the turn could finish.",
        );
        let _lifecycle = lifecycle_gate
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        steering_inbox
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clear();
        let runtime_turn = active_turn
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take();
        if let Some(runtime_turn) = runtime_turn {
            runtime.emit_finish_for_turn(
                runtime_turn,
                None,
                Some(TurnStopReason::Cancelled),
            );
        } else {
            runtime.emit_finish_with_reason(None, Some(TurnStopReason::Cancelled));
        }
    };

    let Some(supervisor) = process_supervisor else {
        terminalize();
        return Ok(());
    };
    let runtime_handle = tokio::runtime::Handle::try_current().map_err(|_| {
        AppError::Internal(
            "Cannot schedule Nomi process-tree teardown outside a Tokio runtime".to_owned(),
        )
    })?;
    runtime_handle.spawn(async move {
        if let Err(error) = shutdown_mcp_managers_exact(&mcp_managers).await {
            error!(
                error = %error,
                "Idle Nomi kill could not prove exact MCP teardown; retaining non-terminal quarantine"
            );
            return;
        }
        let report = supervisor.quiesce().await;
        if !report.is_exact() {
            error!(
                failure = %describe_quiesce_failure(&report),
                "Idle Nomi kill could not prove exact process-tree teardown; retaining non-terminal quarantine"
            );
            return;
        }
        terminalize();
    });
    Ok(())
}

impl NomiAgentManager {
    pub fn kill_and_wait(
        &self,
        reason: Option<AgentKillReason>,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), AppError>> + Send>> {
        let kill_result = crate::runtime_handle::AgentRuntimeControl::kill(self, reason);
        let runtime = self.runtime.clone();
        let process_supervisor = self.process_supervisor.clone();
        let mcp_managers = self.mcp_managers.clone();
        Box::pin(async move {
            kill_result?;
            shutdown_mcp_managers_exact(&mcp_managers).await?;
            if let Some(supervisor) = process_supervisor {
                let report = supervisor.quiesce().await;
                if !report.is_exact() {
                    return Err(AppError::Internal(format!(
                        "Nomi process-tree teardown for conversation {} was not exact: {}",
                        runtime.conversation_id(),
                        describe_quiesce_failure(&report),
                    )));
                }
            }
            // No total timeout: runtime-registry quarantine remains authoritative
            // until this exact manager publishes its process-fenced terminal.
            runtime.wait_until_finished_unbounded().await;
            Ok(())
        })
    }

    /// Register the native cron tools (cron_create / cron_list / cron_delete)
    /// backed by `sink`. Called by the factory right after construction, before
    /// the first turn, so the tools are advertised on the first model request.
    pub async fn register_cron_sink(&self, sink: Arc<dyn CronSink>) {
        let mut engine = self.engine.lock().await;
        let reg = engine.registry_mut();
        reg.register(Box::new(CronCreateTool::new(sink.clone())));
        reg.register(Box::new(CronListTool::new(sink.clone())));
        reg.register(Box::new(CronDeleteTool::new(sink)));
        debug!(conversation_id = %self.runtime.conversation_id(), "Registered cron native tools");
    }
}

/// Nomi-specific operations reached through `AgentRuntimeHandle::Nomi(..)`
/// matches in the routes + services.
impl NomiAgentManager {
    /// Push a user interjection into the running turn's steering inbox.
    /// Returns `Ok(true)` if a turn is live and the message was queued for
    /// mid-turn injection; `Ok(false)` if no turn is running (caller should
    /// fall back to a normal send). Never blocks on the engine lock.
    ///
    /// Admission is serialized with exact terminal transition. A steer that
    /// wins the gate belongs to the current active turn and is either consumed
    /// by that turn or absorbed when it terminates. A steer that loses to
    /// terminal returns `false`; it is never queued for the next explicit turn.
    pub fn steer(&self, text: String) -> Result<bool, AppError> {
        let _lifecycle = self
            .lifecycle_gate
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if self.closing.load(Ordering::Acquire)
            || self.runtime.status() != Some(ConversationStatus::Running)
            || self
                .active_turn
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .is_none()
            || self
                .turn_cancel
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .is_cancelled()
        {
            return Ok(false);
        }
        self.steering_inbox
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push_back(text);
        self.runtime.bump_activity();
        Ok(true)
    }

    /// Queue trusted host-side resource state without starting a turn.
    ///
    /// The dedicated inbox is mounted on the engine during every real Nomi
    /// turn. The engine drains it into top-level system context immediately
    /// before the next provider request, so an idle runtime retains the notice
    /// until the next user-initiated model call and an active runtime can see it
    /// at its next model boundary. It never enters the user transcript.
    pub fn notify_system_resource(
        &self,
        notice: String,
    ) -> Result<SystemResourceNoticeDelivery, AppError> {
        let notice = notice.trim();
        if notice.is_empty() {
            return Err(AppError::BadRequest(
                "System resource notice must not be empty".into(),
            ));
        }

        let _lifecycle = self
            .lifecycle_gate
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if self.closing.load(Ordering::Acquire) {
            return Err(AppError::Conflict(
                "Agent runtime is shutting down and cannot accept resource notifications"
                    .to_owned(),
            ));
        }

        let delivery = if self.runtime.status() == Some(ConversationStatus::Running) {
            SystemResourceNoticeDelivery::ActiveTurn
        } else {
            SystemResourceNoticeDelivery::NextModelCall
        };
        self.system_resource_inbox
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .push_back(notice.to_owned());
        self.runtime.bump_activity();
        Ok(delivery)
    }

    pub fn confirm(&self, _msg_id: &str, call_id: &str, data: Value, always_allow: bool) -> Result<(), AppError> {
        if let Ok(mut confs) = self.confirmations.write() {
            confs.retain(|c| c.call_id != call_id);
        }

        let value = data.get("value").and_then(|v| v.as_str()).unwrap_or("cancel");

        let is_cancel = value == "cancel";

        debug!(
            conversation_id = %self.runtime.conversation_id(),
            call_id,
            value,
            always_allow,
            "Nomi confirm"
        );

        if is_cancel {
            self.approval_manager.resolve(
                call_id,
                ToolApprovalResult::Denied {
                    reason: "User denied the tool request".into(),
                },
            );
        } else {
            let scope = if always_allow {
                nomi_protocol::commands::ApprovalScope::Always
            } else {
                nomi_protocol::commands::ApprovalScope::Once
            };
            self.approval_manager.approve(call_id, scope);
        }
        Ok(())
    }

    pub fn get_confirmations(&self) -> Vec<Confirmation> {
        self.confirmations.read().map(|c| c.clone()).unwrap_or_default()
    }

    pub fn check_approval(&self, action: &str, _command_type: Option<&str>) -> bool {
        self.approval_manager.is_auto_approved(action)
    }

    pub async fn mode(&self) -> Result<AgentModeResponse, AppError> {
        Ok(AgentModeResponse {
            mode: self.approval_manager.current_mode(),
            initialized: true,
        })
    }

    pub async fn set_mode(&self, mode: &str) -> Result<(), AppError> {
        let prev = self.approval_manager.current_mode();
        self.approval_manager.set_mode(parse_session_mode(mode));
        info!(
            conversation_id = %self.runtime.conversation_id(),
            from = prev,
            to = mode,
            "Nomi session mode switched"
        );
        Ok(())
    }

    pub async fn get_slash_commands(&self) -> Result<Vec<SlashCommandItem>, AppError> {
        Ok(self.slash_commands.clone())
    }

    /// Clear the conversation context ("release model context"): stop any
    /// in-flight turn, then empty the engine's message history + compaction
    /// state and persist the now-empty session. The agent stays alive; the
    /// next prompt starts from a clean slate.
    pub async fn clear_context(&self) -> Result<(), AppError> {
        info!(
            conversation_id = %self.runtime.conversation_id(),
            "Clearing Nomi context"
        );
        // Signal any in-flight engine.execute_turn() to abort so we don't clear
        // mid-turn; the engine lock below then waits for it to release.
        self.request_stop(None, "clear_context", false);
        let mut engine = self.engine.lock().await;
        engine.clear_context();
        Ok(())
    }

    /// Rewind the last user turn (edit & resubmit the most recent user message):
    /// stop any in-flight turn, then truncate the engine's in-memory transcript
    /// back to that turn's start so a fresh send re-runs without the stale turn.
    /// Returns BadRequest when there is no valid anchor (e.g. context was
    /// compacted away) so the caller can surface a retriable error.
    pub async fn rewind_last_turn(&self) -> Result<(), AppError> {
        info!(
            conversation_id = %self.runtime.conversation_id(),
            "Rewinding last Nomi turn"
        );
        self.request_stop(None, "rewind_last_turn", false);
        let mut engine = self.engine.lock().await;
        if !engine.rewind_last_turn() {
            return Err(AppError::BadRequest(
                "无法回退上一轮（上下文可能已被压缩），请清空上下文后重试".into(),
            ));
        }
        Ok(())
    }

    /// Test-only accessor for the resolved distillation target directory.
    #[cfg(test)]
    pub(crate) fn distill_dir_for_test(&self) -> Option<&std::path::Path> {
        self.distill_dir.as_deref()
    }
}

fn parse_session_mode(s: &str) -> SessionMode {
    match s {
        "auto_edit" => SessionMode::AutoEdit,
        "yolo" => SessionMode::Yolo,
        _ => SessionMode::Default,
    }
}

#[cfg(feature = "browser-use")]
fn should_install_browser_approval_gate(
    browser_takeover: bool,
    browser_unrestricted_approval: bool,
    approval_manager: &ToolApprovalManager,
) -> bool {
    browser_takeover
        || browser_unrestricted_approval
        || approval_manager.is_auto_approved(&ToolCategory::Irreversible.to_string())
}

fn nomi_engine_error_to_send_error(error_msg: String) -> AgentSendError {
    let lower = error_msg.to_ascii_lowercase();
    if lower.contains("provider error") || lower.contains("provider:") || lower.contains("api error:") {
        return AgentSendError::from_app_error(AppError::BadGateway(error_msg));
    }
    AgentSendError::from_app_error(AppError::Internal(error_msg))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::events::ToolCallStatus;
    use crate::runtime_handle::AgentRuntimeControl;
    use nomi_protocol::events::ToolCategory;
    use nomi_providers::{LlmProvider, ProviderError};
    use nomi_tools::{registry::ToolRegistry, Tool};
    use nomi_types::llm::{LlmEvent, LlmRequest};
    use nomi_types::message::{ContentBlock, Role, StopReason};
    use nomi_types::tool::ToolResult;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn make_test_config() -> NomiResolvedConfig {
        NomiResolvedConfig {
            provider: "anthropic".into(),
            api_key: "sk-test-key".into(),
            model: "claude-sonnet-4-20250514".into(),
            base_url: None,
            system_prompt: None,
            max_tokens: 4096,
            max_turns: None,
            context_limit: None,
            compat_overrides: Default::default(),
            session_directory: std::env::temp_dir().join("nomi-test-sessions"),
            session_mode: None,
            extra_mcp_servers: std::collections::HashMap::new(),
            loopback_capability_leases: Default::default(),
            bedrock_config: None,
            computer_use: false,
            browser_use: false,
            browser_silent: true,
            browser_source: "managed".to_owned(),
            browser_full_power: false,
            browser_persistent_login: false,
            browser_site_memory: false,
            browser_takeover: false,
            browser_unrestricted_approval: false,
            browser_visual_fallback: false,
            goal: None,
            browser_secret_vault: None,
            owner_token: None,
            install_embedded_agent_execution: true,
            allowed_tools: Vec::new(),
            write_root: None,
        }
    }

    struct ScriptedProvider {
        calls: AtomicUsize,
        responses: Vec<Vec<LlmEvent>>,
        requests: std::sync::Mutex<Vec<LlmRequest>>,
    }

    impl ScriptedProvider {
        fn new(responses: Vec<Vec<LlmEvent>>) -> Self {
            Self {
                calls: AtomicUsize::new(0),
                responses,
                requests: std::sync::Mutex::new(Vec::new()),
            }
        }

        fn calls(&self) -> usize {
            self.calls.load(Ordering::SeqCst)
        }

        fn requests(&self) -> Vec<LlmRequest> {
            self.requests
                .lock()
                .expect("request capture lock poisoned")
                .clone()
        }
    }

    #[async_trait::async_trait]
    impl LlmProvider for ScriptedProvider {
        async fn stream(
            &self,
            request: &LlmRequest,
        ) -> Result<tokio::sync::mpsc::Receiver<LlmEvent>, ProviderError> {
            self.requests
                .lock()
                .expect("request capture lock poisoned")
                .push(request.clone());
            let index = self.calls.fetch_add(1, Ordering::SeqCst);
            let events = self
                .responses
                .get(index)
                .unwrap_or_else(|| panic!("unexpected provider call {index}"))
                .clone();
            let (tx, rx) = tokio::sync::mpsc::channel(events.len().max(1));
            tokio::spawn(async move {
                for event in events {
                    let _ = tx.send(event).await;
                }
            });
            Ok(rx)
        }
    }

    struct BlockingProvider {
        calls: AtomicUsize,
        called: tokio::sync::Semaphore,
        senders: std::sync::Mutex<Vec<tokio::sync::mpsc::Sender<LlmEvent>>>,
        requests: std::sync::Mutex<Vec<LlmRequest>>,
    }

    impl BlockingProvider {
        fn new() -> Self {
            Self {
                calls: AtomicUsize::new(0),
                called: tokio::sync::Semaphore::new(0),
                senders: std::sync::Mutex::new(Vec::new()),
                requests: std::sync::Mutex::new(Vec::new()),
            }
        }

        fn requests(&self) -> Vec<LlmRequest> {
            self.requests
                .lock()
                .expect("request capture lock poisoned")
                .clone()
        }
    }

    #[async_trait::async_trait]
    impl LlmProvider for BlockingProvider {
        async fn stream(
            &self,
            request: &LlmRequest,
        ) -> Result<tokio::sync::mpsc::Receiver<LlmEvent>, ProviderError> {
            self.requests
                .lock()
                .expect("request capture lock poisoned")
                .push(request.clone());
            self.calls.fetch_add(1, Ordering::SeqCst);
            let (tx, rx) = tokio::sync::mpsc::channel(1);
            self.senders.lock().expect("sender lock poisoned").push(tx);
            self.called.add_permits(1);
            Ok(rx)
        }
    }

    struct HangingTool {
        started: Arc<tokio::sync::Semaphore>,
    }

    #[async_trait::async_trait]
    impl Tool for HangingTool {
        fn name(&self) -> &str {
            "hanging_tool"
        }

        fn description(&self) -> &str {
            "A test tool that never returns"
        }

        fn input_schema(&self) -> serde_json::Value {
            serde_json::json!({"type": "object", "properties": {}})
        }

        fn is_concurrency_safe(&self, _input: &serde_json::Value) -> bool {
            false
        }

        async fn execute(&self, _input: serde_json::Value) -> ToolResult {
            self.started.add_permits(1);
            std::future::pending::<ToolResult>().await
        }

        fn category(&self) -> ToolCategory {
            ToolCategory::Exec
        }
    }

    /// Advertises the production `Write` route for stream-boundary tests that
    /// never commit or execute the call. Tool progress from an unadvertised
    /// name is intentionally rejected by the engine, so those fixtures must
    /// model the same request authority as a real desktop Nomi session.
    struct PreviewOnlyWriteTool;

    #[async_trait::async_trait]
    impl Tool for PreviewOnlyWriteTool {
        fn name(&self) -> &str {
            "Write"
        }

        fn description(&self) -> &str {
            "Test-only advertised write tool"
        }

        fn input_schema(&self) -> serde_json::Value {
            serde_json::json!({
                "type": "object",
                "properties": {
                    "file_path": { "type": "string" },
                    "content": { "type": "string" }
                },
                "required": ["file_path", "content"]
            })
        }

        fn is_concurrency_safe(&self, _input: &serde_json::Value) -> bool {
            false
        }

        async fn execute(&self, _input: serde_json::Value) -> ToolResult {
            ToolResult::error("PreviewOnlyWriteTool must not be executed")
        }

        fn category(&self) -> ToolCategory {
            ToolCategory::Exec
        }
    }

    struct MissingImageArtifactTool;

    #[async_trait::async_trait]
    impl Tool for MissingImageArtifactTool {
        fn name(&self) -> &str {
            "image_gen"
        }

        fn description(&self) -> &str {
            "Test-only image generator that falsely reports text success"
        }

        fn input_schema(&self) -> serde_json::Value {
            serde_json::json!({"type": "object", "properties": {}})
        }

        fn is_concurrency_safe(&self, _input: &serde_json::Value) -> bool {
            true
        }

        async fn execute(&self, _input: serde_json::Value) -> ToolResult {
            ToolResult::text("generated successfully")
        }

        fn category(&self) -> ToolCategory {
            ToolCategory::Info
        }
    }

    struct HangingKnowledgeSink {
        started: Arc<tokio::sync::Semaphore>,
    }

    #[async_trait::async_trait]
    impl nomi_agent::knowledge_tools::KnowledgeRetrievalSink for HangingKnowledgeSink {
        async fn search(
            &self,
            _kb_ids: &[nomifun_common::KnowledgeBaseId],
            _query: &str,
            _limit: usize,
        ) -> Result<Vec<nomi_agent::knowledge_tools::KnowledgeHit>, String> {
            self.started.add_permits(1);
            std::future::pending().await
        }

        async fn read_document(
            &self,
            _kb_ids: &[nomifun_common::KnowledgeBaseId],
            _handle: &str,
        ) -> Result<String, String> {
            Err("not used by this test".to_owned())
        }
    }

    fn make_test_engine_config() -> Config {
        let mut config = Config::resolve(&CliArgs {
            provider: Some("anthropic".into()),
            api_key: Some("sk-test-key".into()),
            base_url: None,
            model: Some("claude-sonnet-4-20250514".into()),
            max_tokens: Some(4096),
            max_turns: Some(10),
            system_prompt: None,
            profile: None,
            auto_approve: true,
            project_dir: Some(PathBuf::from("/project")),
        })
        .expect("test config should resolve");
        config.session.enabled = false;
        config
    }

    #[test]
    fn provider_context_budget_is_applied_before_engine_bootstrap() {
        let mut config = make_test_engine_config();
        config.max_tokens = 8192;

        apply_provider_context_budget(&mut config, Some(4096));

        assert_eq!(config.compact.context_window, 4096);
        assert_eq!(config.max_tokens, 1024);
        assert_eq!(config.compact.output_reserve, 1024);
        assert_eq!(config.compact.autocompact_buffer, 512);
        assert_eq!(config.compact.emergency_buffer, 256);
    }

    fn make_agent_with_provider(provider: Arc<dyn LlmProvider>) -> NomiAgentManager {
        make_agent_with_provider_and_max_turns(provider, Some(10))
    }

    fn make_agent_with_provider_and_max_turns(
        provider: Arc<dyn LlmProvider>,
        max_turns: Option<usize>,
    ) -> NomiAgentManager {
        let runtime = AgentRuntimeState::new("conv-auto-continue", "/project", 128);
        let backend_output_sink = Arc::new(BackendOutputSink::new(runtime.event_sender()));
        let output: Arc<dyn OutputSink> = backend_output_sink.clone();
        let mut config = make_test_engine_config();
        config.max_turns = max_turns;
        let mut engine = AgentEngine::new_with_provider(
            provider,
            config.clone(),
            ToolRegistry::new(),
            output,
            PathBuf::from("/project"),
        );
        let approval_manager = Arc::new(ToolApprovalManager::new());
        let confirmations = Arc::new(std::sync::RwLock::new(Vec::new()));
        let protocol_sink = BackendProtocolSink::new(
            runtime.event_sender(),
            confirmations.clone(),
        );
        engine.set_approval_manager(approval_manager.clone());
        engine.set_protocol_writer(Arc::new(protocol_sink));

        NomiAgentManager {
            runtime,
            backend_output_sink,
            engine: Mutex::new(engine),
            process_supervisor: None,
            turn_teardown_fence: Arc::new(TurnTeardownFence::new()),
            slash_commands: Vec::new(),
            mcp_managers: Vec::new(),
            loopback_capability_leases: Default::default(),
            approval_manager,
            confirmations,
            turn_cancel: std::sync::Mutex::new(tokio_util::sync::CancellationToken::new()),
            active_turn: Arc::new(std::sync::Mutex::new(None)),
            lifecycle_gate: Arc::new(std::sync::Mutex::new(())),
            turn_gate: Mutex::new(()),
            closing: AtomicBool::new(false),
            steering_inbox: Arc::new(std::sync::Mutex::new(std::collections::VecDeque::new())),
            system_resource_inbox: Arc::new(std::sync::Mutex::new(
                std::collections::VecDeque::new(),
            )),
            distill_dir: None,
            image_read_root: None,
            distill_cfg: Arc::new(config),
            knowledge_prelude: std::sync::Mutex::new(None),
            knowledge_auto_rag: None,
        }
    }

    #[tokio::test]
    async fn idle_system_resource_notice_waits_for_next_real_model_call() {
        let provider = Arc::new(ScriptedProvider::new(vec![vec![LlmEvent::Done {
            stop_reason: StopReason::EndTurn,
            usage: Default::default(),
        }]]));
        let agent = make_agent_with_provider(provider.clone());

        let delivery = agent
            .notify_system_resource(
                "terminal term-idle transitioned to exited (exit_code=0)".to_owned(),
            )
            .unwrap();
        assert_eq!(delivery, SystemResourceNoticeDelivery::NextModelCall);
        assert_eq!(
            provider.calls(),
            0,
            "queuing resource state must not synthesize a model turn"
        );

        agent
            .send_message(SendMessageData {
                content: "continue".into(),
                msg_id: "msg-after-idle-resource".into(),
                files: Vec::new(),
                inject_skills: Vec::new(),
                origin: None,
            })
            .await
            .unwrap();

        let requests = provider.requests();
        assert_eq!(requests.len(), 1);
        assert!(
            requests[0]
                .system
                .contains("terminal term-idle transitioned to exited (exit_code=0)")
        );
        assert!(
            requests[0].messages.iter().all(|message| {
                message.content.iter().all(|block| {
                    !matches!(
                        block,
                        ContentBlock::Text { text }
                            if text.contains("terminal term-idle transitioned to exited")
                    )
                })
            }),
            "resource state must not be presented as a user message"
        );
    }

    #[test]
    fn running_system_resource_notice_enters_active_runtime_inbox() {
        let provider = Arc::new(ScriptedProvider::new(Vec::new()));
        let agent = make_agent_with_provider(provider);
        agent.runtime.transition_to(ConversationStatus::Running);

        let delivery = agent
            .notify_system_resource("terminal term-live was closed".to_owned())
            .unwrap();

        assert_eq!(delivery, SystemResourceNoticeDelivery::ActiveTurn);
        assert_eq!(
            agent
                .system_resource_inbox
                .lock()
                .unwrap()
                .iter()
                .map(String::as_str)
                .collect::<Vec<_>>(),
            vec!["terminal term-live was closed"]
        );
    }

    fn assert_single_cancelled_finish_without_running_tools(
        agent: &NomiAgentManager,
        rx: &mut broadcast::Receiver<AgentStreamEvent>,
    ) {
        let events = std::iter::from_fn(|| rx.try_recv().ok()).collect::<Vec<_>>();
        let finish_reasons = events
            .iter()
            .filter_map(|event| match event {
                AgentStreamEvent::Finish(data) => Some(data.stop_reason),
                _ => None,
            })
            .collect::<Vec<_>>();

        assert_eq!(finish_reasons, vec![Some(TurnStopReason::Cancelled)]);
        assert!(
            !events.iter().any(|event| matches!(
                event,
                AgentStreamEvent::ToolCall(data) if data.status == ToolCallStatus::Running
            )),
            "cancelled preparation must not leave a Running tool card"
        );
        assert_eq!(agent.status(), Some(ConversationStatus::Finished));
    }

    #[tokio::test]
    async fn send_message_files_reach_provider_as_image_content() {
        let provider = Arc::new(ScriptedProvider::new(vec![vec![LlmEvent::Done {
            stop_reason: StopReason::EndTurn,
            usage: Default::default(),
        }]]));
        let agent = make_agent_with_provider(provider.clone());
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("attached.png");
        image::DynamicImage::ImageRgb8(image::RgbImage::from_pixel(
            16,
            12,
            image::Rgb([40, 80, 120]),
        ))
        .save_with_format(&path, image::ImageFormat::Png)
        .unwrap();

        agent
            .send_message(SendMessageData {
                content: "What is shown?".into(),
                msg_id: "msg-image".into(),
                files: vec![path.to_string_lossy().into_owned()],
                inject_skills: Vec::new(),
                origin: None,
            })
            .await
            .unwrap();

        let requests = provider.requests();
        assert_eq!(requests.len(), 1);
        let user = requests[0]
            .messages
            .iter()
            .find(|message| message.role == Role::User)
            .expect("provider request should contain the user message");
        assert!(matches!(
            &user.content[..],
            [ContentBlock::Text { text }, ContentBlock::Image { media_type, data }]
                if text == "What is shown?"
                    && media_type == "image/png"
                    && !data.is_empty()
        ));
    }

    #[tokio::test]
    async fn send_message_skips_image_io_for_a_known_text_only_model() {
        let provider = Arc::new(ScriptedProvider::new(vec![vec![LlmEvent::Done {
            stop_reason: StopReason::EndTurn,
            usage: Default::default(),
        }]]));
        let runtime = AgentRuntimeState::new("conv-text-only", "/project", 128);
        let backend_output_sink = Arc::new(BackendOutputSink::new(runtime.event_sender()));
        let output: Arc<dyn OutputSink> = backend_output_sink.clone();
        let mut config = make_test_engine_config();
        config.compat.supports_image = Some(false);
        let mut engine = AgentEngine::new_with_provider(
            provider.clone(),
            config.clone(),
            ToolRegistry::new(),
            output,
            PathBuf::from("/project"),
        );
        let approval_manager = Arc::new(ToolApprovalManager::new());
        engine.set_approval_manager(approval_manager.clone());
        let agent = NomiAgentManager {
            runtime,
            backend_output_sink,
            engine: Mutex::new(engine),
            process_supervisor: None,
            turn_teardown_fence: Arc::new(TurnTeardownFence::new()),
            slash_commands: Vec::new(),
            mcp_managers: Vec::new(),
            loopback_capability_leases: Default::default(),
            approval_manager,
            confirmations: Arc::new(std::sync::RwLock::new(Vec::new())),
            turn_cancel: std::sync::Mutex::new(tokio_util::sync::CancellationToken::new()),
            active_turn: Arc::new(std::sync::Mutex::new(None)),
            lifecycle_gate: Arc::new(std::sync::Mutex::new(())),
            turn_gate: Mutex::new(()),
            closing: AtomicBool::new(false),
            steering_inbox: Arc::new(std::sync::Mutex::new(std::collections::VecDeque::new())),
            system_resource_inbox: Arc::new(std::sync::Mutex::new(
                std::collections::VecDeque::new(),
            )),
            distill_dir: None,
            image_read_root: None,
            distill_cfg: Arc::new(config),
            knowledge_prelude: std::sync::Mutex::new(None),
            knowledge_auto_rag: None,
        };
        let attachment_dir = tempfile::tempdir().unwrap();
        let missing_image = attachment_dir
            .path()
            .join("missing-text-only-attachment.png")
            .to_string_lossy()
            .into_owned();

        agent
            .send_message(SendMessageData {
                content: "Answer using text only.".into(),
                msg_id: "msg-text-only".into(),
                files: vec![missing_image],
                inject_skills: Vec::new(),
                origin: None,
            })
            .await
            .expect("known text-only models should ignore image attachments");

        let requests = provider.requests();
        assert_eq!(requests.len(), 1);
        let user = requests[0]
            .messages
            .iter()
            .find(|message| message.role == Role::User)
            .expect("provider request should contain the user message");
        assert!(matches!(
            &user.content[..],
            [ContentBlock::Text { text }] if text == "Answer using text only."
        ));
    }

    #[tokio::test]
    async fn send_message_auto_continues_after_max_tokens_before_finish() {
        let provider = Arc::new(ScriptedProvider::new(vec![
            vec![
                LlmEvent::TextDelta("partial".into()),
                LlmEvent::Done {
                    stop_reason: StopReason::MaxTokens,
                    usage: Default::default(),
                },
            ],
            vec![
                LlmEvent::TextDelta(" done".into()),
                LlmEvent::Done {
                    stop_reason: StopReason::EndTurn,
                    usage: Default::default(),
                },
            ],
        ]));
        let agent = make_agent_with_provider(provider.clone());
        let mut rx = agent.subscribe();

        agent
            .send_message(SendMessageData {
                content: "create the file".into(),
                msg_id: "msg-1".into(),
                files: Vec::new(),
                inject_skills: Vec::new(),
                origin: None,
            })
            .await
            .unwrap();

        assert_eq!(provider.calls(), 2, "MaxTokens should trigger one continuation pass");

        let mut completed_reasons = Vec::new();
        let mut finish_reason = None;
        while let Ok(event) = rx.try_recv() {
            match event {
                AgentStreamEvent::TurnCompleted(data) => completed_reasons.push(data.stop_reason),
                AgentStreamEvent::Finish(data) => finish_reason = data.stop_reason,
                _ => {}
            }
        }

        assert_eq!(completed_reasons, vec![Some(TurnStopReason::EndTurn)]);
        assert_eq!(finish_reason, Some(TurnStopReason::EndTurn));
    }

    #[tokio::test]
    async fn missing_generated_artifact_emits_error_and_never_normal_finish() {
        let provider = Arc::new(ScriptedProvider::new(vec![
            vec![
                LlmEvent::ToolUse {
                    id: "missing-image".into(),
                    name: "image_gen".into(),
                    input: serde_json::json!({}),
                    extra: None,
                },
                LlmEvent::Done {
                    stop_reason: StopReason::ToolUse,
                    usage: Default::default(),
                },
            ],
            vec![LlmEvent::Done {
                stop_reason: StopReason::EndTurn,
                usage: Default::default(),
            }],
        ]));
        let mut agent = make_agent_with_provider(provider);
        assert!(
            agent
                .engine
                .get_mut()
                .registry_mut()
                .register(Box::new(MissingImageArtifactTool))
        );
        let mut rx = agent.subscribe();

        let result = agent
            .send_message(SendMessageData {
                content: "generate an image".into(),
                msg_id: "msg-missing-image".into(),
                files: Vec::new(),
                inject_skills: Vec::new(),
                origin: None,
            })
            .await;

        assert!(result.is_err());
        let events = std::iter::from_fn(|| rx.try_recv().ok()).collect::<Vec<_>>();
        assert!(events.iter().any(|event| matches!(
            event,
            AgentStreamEvent::ToolCall(data)
                if data.call_id == "nomi-missing-image"
                    && data.status == ToolCallStatus::Error
                    && data.artifacts.is_empty()
        )));
        assert!(events.iter().any(|event| matches!(event, AgentStreamEvent::Error(_))));
        assert!(
            !events
                .iter()
                .any(|event| matches!(event, AgentStreamEvent::Finish(_))),
            "artifact-delivery failure must not be followed by a normal Finish"
        );
        assert!(
            !events
                .iter()
                .any(|event| matches!(event, AgentStreamEvent::TurnCompleted(_)))
        );
    }

    #[tokio::test]
    async fn provider_error_never_publishes_uncommitted_tool_progress() {
        let provider = Arc::new(ScriptedProvider::new(vec![
            vec![
                LlmEvent::ToolUseDelta {
                    id: "stale-preview".into(),
                    name: "Write".into(),
                    input: Some(serde_json::json!({"file_path": "/tmp/stale.html"})),
                },
                LlmEvent::ToolUse {
                    id: "stale-preview".into(),
                    name: "Write".into(),
                    input: serde_json::json!({
                        "file_path": "/tmp/stale.html",
                        "content": "not committed"
                    }),
                    extra: None,
                },
                LlmEvent::Error("malformed structured tool arguments".into()),
            ],
            vec![LlmEvent::Done {
                stop_reason: StopReason::MaxTokens,
                usage: Default::default(),
            }],
            vec![LlmEvent::Done {
                stop_reason: StopReason::EndTurn,
                usage: Default::default(),
            }],
        ]));
        let agent = make_agent_with_provider(provider.clone());
        let mut rx = agent.subscribe();

        let first_result = agent
            .send_message(SendMessageData {
                content: "trigger malformed structured progress".into(),
                msg_id: "msg-error".into(),
                files: Vec::new(),
                inject_skills: Vec::new(),
                origin: None,
            })
            .await;
        assert!(first_result.is_err());

        let first_statuses = std::iter::from_fn(|| rx.try_recv().ok())
            .filter_map(|event| match event {
                AgentStreamEvent::ToolCall(data) if data.call_id == "nomi-stale-preview" => {
                    Some(data.status)
                }
                _ => None,
            })
            .collect::<Vec<_>>();
        assert!(
            first_statuses.is_empty(),
            "partial provider progress must never enter the frontend lifecycle"
        );

        agent
            .send_message(SendMessageData {
                content: "start a clean turn".into(),
                msg_id: "msg-next".into(),
                files: Vec::new(),
                inject_skills: Vec::new(),
                origin: None,
            })
            .await
            .unwrap();

        let resurrected = std::iter::from_fn(|| rx.try_recv().ok()).any(|event| {
            matches!(
                event,
                AgentStreamEvent::ToolCall(data) if data.call_id == "nomi-stale-preview"
            )
        });
        assert!(
            !resurrected,
            "a later MaxTokens continuation must not recover a failed prior call"
        );
        assert_eq!(provider.calls(), 3);
    }

    #[tokio::test]
    async fn cancel_drains_committed_tool_progress() {
        let provider = Arc::new(BlockingProvider::new());
        let started = Arc::new(tokio::sync::Semaphore::new(0));
        let mut agent = make_agent_with_provider(provider.clone());
        agent
            .engine
            .get_mut()
            .registry_mut()
            .register(Box::new(HangingTool {
                started: Arc::clone(&started),
            }));
        let agent = Arc::new(agent);
        let mut rx = agent.subscribe();
        let send_task = {
            let agent = Arc::clone(&agent);
            tokio::spawn(async move {
                agent
                    .send_message(SendMessageData {
                        content: "start a committed tool call".into(),
                        msg_id: "msg-cancel-tool".into(),
                        files: Vec::new(),
                        inject_skills: Vec::new(),
                        origin: None,
                    })
                    .await
            })
        };
        provider.called.acquire().await.unwrap().forget();

        let provider_tx = provider
            .senders
            .lock()
            .expect("sender lock poisoned")
            .last()
            .expect("blocking provider sender")
            .clone();
        provider_tx
            .send(LlmEvent::ToolUse {
                id: "cancel-committed".into(),
                name: "hanging_tool".into(),
                input: serde_json::json!({}),
                extra: None,
            })
            .await
            .unwrap();
        provider_tx
            .send(LlmEvent::Done {
                stop_reason: StopReason::ToolUse,
                usage: Default::default(),
            })
            .await
            .unwrap();
        drop(provider_tx);
        provider
            .senders
            .lock()
            .expect("sender lock poisoned")
            .pop()
            .expect("blocking provider sender should still be registered");

        tokio::time::timeout(std::time::Duration::from_secs(1), started.acquire())
            .await
            .expect("committed tool should start execution")
            .expect("start semaphore should remain open")
            .forget();

        tokio::time::timeout(std::time::Duration::from_secs(1), async {
            loop {
                if let AgentStreamEvent::ToolCall(data) = rx.recv().await.unwrap()
                    && data.call_id == "nomi-cancel-committed"
                    && data.status == ToolCallStatus::Running
                {
                    break;
                }
            }
        })
        .await
        .expect("tool progress should reach the frontend before cancellation");

        agent.cancel().await.unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(1), send_task)
            .await
            .expect("cancelled send should unwind")
            .unwrap()
            .unwrap();

        let remaining_events = std::iter::from_fn(|| rx.try_recv().ok()).collect::<Vec<_>>();
        let terminal_events = remaining_events
            .iter()
            .filter_map(|event| match event {
                AgentStreamEvent::ToolCall(data) if data.call_id == "nomi-cancel-committed" => {
                    Some(data)
                }
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(terminal_events.len(), 1);
        assert_eq!(terminal_events[0].status, ToolCallStatus::Error);
        assert_eq!(
            terminal_events[0].output.as_deref(),
            Some("Tool execution canceled by user")
        );
        assert_eq!(
            remaining_events
                .iter()
                .filter_map(|event| match event {
                    AgentStreamEvent::Finish(data) => Some(data.stop_reason),
                    _ => None,
                })
                .collect::<Vec<_>>(),
            vec![Some(TurnStopReason::Cancelled)]
        );
        assert_eq!(agent.status(), Some(ConversationStatus::Finished));
    }

    #[tokio::test]
    async fn cancel_drops_a_hung_tool_and_emits_one_error_terminal() {
        let provider = Arc::new(ScriptedProvider::new(vec![vec![
            LlmEvent::ToolUse {
                id: "hang-1".into(),
                name: "hanging_tool".into(),
                input: serde_json::json!({}),
                extra: None,
            },
            LlmEvent::Done {
                stop_reason: StopReason::ToolUse,
                usage: Default::default(),
            },
        ]]));
        let started = Arc::new(tokio::sync::Semaphore::new(0));
        let mut agent = make_agent_with_provider(provider.clone());
        agent
            .engine
            .get_mut()
            .registry_mut()
            .register(Box::new(HangingTool {
                started: Arc::clone(&started),
            }));
        let agent = Arc::new(agent);
        let mut rx = agent.subscribe();

        let send_task = {
            let agent = Arc::clone(&agent);
            tokio::spawn(async move {
                agent
                    .send_message(SendMessageData {
                        content: "run the hanging tool".into(),
                        msg_id: "msg-hanging-tool".into(),
                        files: Vec::new(),
                        inject_skills: Vec::new(),
                        origin: None,
                    })
                    .await
            })
        };

        tokio::time::timeout(std::time::Duration::from_secs(1), started.acquire())
            .await
            .expect("hanging tool should start")
            .expect("start semaphore should remain open")
            .forget();

        agent.cancel().await.unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(1), send_task)
            .await
            .expect("cancellation must not await a hung tool")
            .unwrap()
            .unwrap();

        let statuses = std::iter::from_fn(|| rx.try_recv().ok())
            .filter_map(|event| match event {
                AgentStreamEvent::ToolCall(data) if data.call_id == "nomi-hang-1" => {
                    Some(data.status)
                }
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(
            statuses,
            vec![ToolCallStatus::Running, ToolCallStatus::Error],
            "a cancelled hung tool must terminate once and never complete"
        );
        assert_eq!(provider.calls(), 1);
    }

    #[tokio::test]
    async fn cancel_interrupts_hung_knowledge_preparation() {
        let provider = Arc::new(ScriptedProvider::new(Vec::new()));
        let started = Arc::new(tokio::sync::Semaphore::new(0));
        let retrieval: Arc<dyn nomi_agent::knowledge_tools::KnowledgeRetrievalSink> =
            Arc::new(HangingKnowledgeSink {
                started: Arc::clone(&started),
            });
        let mut agent = make_agent_with_provider(provider.clone());
        agent.knowledge_auto_rag = Some((retrieval, vec![nomifun_common::KnowledgeBaseId::new()]));
        let agent = Arc::new(agent);
        let mut rx = agent.subscribe();

        let send_task = {
            let agent = Arc::clone(&agent);
            tokio::spawn(async move {
                agent
                    .send_message(SendMessageData {
                        content: "search the mounted knowledge base".into(),
                        msg_id: "msg-hanging-rag".into(),
                        files: Vec::new(),
                        inject_skills: Vec::new(),
                        origin: None,
                    })
                    .await
            })
        };

        tokio::time::timeout(std::time::Duration::from_secs(1), started.acquire())
            .await
            .expect("knowledge preparation should start")
            .expect("start semaphore should remain open")
            .forget();
        agent.cancel().await.unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(1), send_task)
            .await
            .expect("cancellation must interrupt a hung knowledge search")
            .unwrap()
            .unwrap();

        assert_eq!(provider.calls(), 0);
        assert_single_cancelled_finish_without_running_tools(&agent, &mut rx);
    }

    #[tokio::test]
    async fn cancel_interrupts_engine_lock_preparation() {
        let provider = Arc::new(ScriptedProvider::new(Vec::new()));
        let agent = Arc::new(make_agent_with_provider(provider.clone()));
        let mut rx = agent.subscribe();
        let engine_guard = agent.engine.lock().await;

        let send_task = {
            let agent = Arc::clone(&agent);
            tokio::spawn(async move {
                agent
                    .send_message(SendMessageData {
                        content: "wait for engine preparation".into(),
                        msg_id: "msg-blocked-engine-lock".into(),
                        files: Vec::new(),
                        inject_skills: Vec::new(),
                        origin: None,
                    })
                    .await
            })
        };

        tokio::time::timeout(std::time::Duration::from_secs(1), async {
            while agent.status() != Some(ConversationStatus::Running) {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("turn should enter Running before waiting for the engine lock");

        agent.cancel().await.unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(1), send_task)
            .await
            .expect("cancellation must interrupt engine-lock preparation")
            .unwrap()
            .unwrap();

        assert_eq!(provider.calls(), 0);
        assert_single_cancelled_finish_without_running_tools(&agent, &mut rx);
        drop(engine_guard);
    }

    #[tokio::test]
    async fn send_message_does_not_auto_continue_after_max_turns() {
        let provider = Arc::new(ScriptedProvider::new(vec![
            vec![
                LlmEvent::ToolUse {
                    id: "loop-1".into(),
                    name: "ToolSearch".into(),
                    input: serde_json::json!({"query": "missing_loop_tool"}),
                    extra: None,
                },
                LlmEvent::Done {
                    stop_reason: StopReason::ToolUse,
                    usage: Default::default(),
                },
            ],
            // This response is present only to expose a regression: the old
            // host policy issued a second provider pass after MaxTurns.
            vec![LlmEvent::Done {
                stop_reason: StopReason::EndTurn,
                usage: Default::default(),
            }],
        ]));
        let agent = make_agent_with_provider_and_max_turns(provider.clone(), Some(1));
        {
            let mut engine = agent.engine.lock().await;
            let deferred_state = engine.registry_mut().deferred_state();
            engine.registry_mut().register(Box::new(
                nomi_tools::tool_search::ToolSearchTool::new(deferred_state),
            ));
        }
        let mut rx = agent.subscribe();

        agent
            .send_message(SendMessageData {
                content: "keep calling the tool".into(),
                msg_id: "msg-max-turns".into(),
                files: Vec::new(),
                inject_skills: Vec::new(),
                origin: None,
            })
            .await
            .unwrap();

        assert_eq!(
            provider.calls(),
            1,
            "MaxTurns must terminate the host turn instead of resetting the engine budget"
        );

        let mut completed_reasons = Vec::new();
        let mut finish_reason = None;
        while let Ok(event) = rx.try_recv() {
            match event {
                AgentStreamEvent::TurnCompleted(data) => completed_reasons.push(data.stop_reason),
                AgentStreamEvent::Finish(data) => finish_reason = data.stop_reason,
                _ => {}
            }
        }
        assert_eq!(
            completed_reasons,
            vec![Some(TurnStopReason::MaxTurnRequests)]
        );
        assert_eq!(finish_reason, Some(TurnStopReason::MaxTurnRequests));
    }

    #[tokio::test]
    async fn max_turns_absorbs_late_steering_before_the_next_explicit_turn() {
        let provider = Arc::new(BlockingProvider::new());
        let mut agent = make_agent_with_provider_and_max_turns(provider.clone(), Some(1));
        {
            let deferred_state = agent.engine.get_mut().registry_mut().deferred_state();
            assert!(
                agent.engine.get_mut().registry_mut().register(Box::new(
                    nomi_tools::tool_search::ToolSearchTool::new(deferred_state),
                ))
            );
        }
        let agent = Arc::new(agent);
        let mut rx = agent.subscribe();

        let first_send = {
            let agent = Arc::clone(&agent);
            tokio::spawn(async move {
                agent
                    .send_message(SendMessageData {
                        content: "start".into(),
                        msg_id: "msg-max-turns-late-steer".into(),
                        files: Vec::new(),
                        inject_skills: Vec::new(),
                        origin: None,
                    })
                    .await
            })
        };
        tokio::time::timeout(std::time::Duration::from_secs(1), provider.called.acquire())
            .await
            .expect("first provider pass should start")
            .expect("provider start semaphore should remain open")
            .forget();

        // The interjection is admitted while this exact generation is Running,
        // after the provider request has already begun. A MaxTurns terminal is
        // not eligible for the race-tail continuation, so terminalization must
        // absorb this queue entry under the lifecycle gate.
        agent
            .steering_inbox
            .lock()
            .unwrap()
            .push_back("late user direction".to_owned());
        let first_provider_tx = provider
            .senders
            .lock()
            .expect("sender lock poisoned")
            .pop()
            .expect("first blocking provider sender");
        first_provider_tx
            .send(LlmEvent::ToolUse {
                id: "late-steer-max-turns-tool".into(),
                name: "ToolSearch".into(),
                input: serde_json::json!({"query": "missing_late_steer_tool"}),
                extra: None,
            })
            .await
            .unwrap();
        first_provider_tx
            .send(LlmEvent::Done {
                stop_reason: StopReason::ToolUse,
                usage: Default::default(),
            })
            .await
            .unwrap();
        drop(first_provider_tx);
        tokio::time::timeout(std::time::Duration::from_secs(1), first_send)
            .await
            .expect("MaxTurns send should terminate")
            .unwrap()
            .unwrap();

        assert_eq!(provider.calls.load(Ordering::SeqCst), 1);
        assert!(
            agent.steering_inbox.lock().unwrap().is_empty(),
            "a terminal generation must absorb its late steering"
        );

        let mut starts = 0;
        let mut completed_reasons = Vec::new();
        let mut finish_reason = None;
        while let Ok(event) = rx.try_recv() {
            match event {
                AgentStreamEvent::Start(_) => starts += 1,
                AgentStreamEvent::TurnCompleted(data) => completed_reasons.push(data.stop_reason),
                AgentStreamEvent::Finish(data) => finish_reason = data.stop_reason,
                _ => {}
            }
        }
        assert_eq!(starts, 1, "MaxTurns must not start a race-tail engine pass");
        assert_eq!(
            completed_reasons,
            vec![Some(TurnStopReason::MaxTurnRequests)]
        );
        assert_eq!(finish_reason, Some(TurnStopReason::MaxTurnRequests));

        let second_send = {
            let agent = Arc::clone(&agent);
            tokio::spawn(async move {
                agent
                    .send_message(SendMessageData {
                        content: "next explicit turn".into(),
                        msg_id: "msg-after-max-turns".into(),
                        files: Vec::new(),
                        inject_skills: Vec::new(),
                        origin: None,
                    })
                    .await
            })
        };
        tokio::time::timeout(std::time::Duration::from_secs(1), provider.called.acquire())
            .await
            .expect("second explicit turn should reach the provider")
            .expect("provider start semaphore should remain open")
            .forget();

        let requests = provider.requests();
        assert_eq!(requests.len(), 2);
        assert!(
            !requests[1].messages.iter().any(|message| {
                message.content.iter().any(|block| {
                    matches!(
                        block,
                        ContentBlock::Text { text } if text.contains("late user direction")
                    )
                })
            }),
            "the next explicit turn must never inherit terminal steering"
        );

        let second_provider_tx = provider
            .senders
            .lock()
            .expect("sender lock poisoned")
            .pop()
            .expect("second blocking provider sender");
        second_provider_tx
            .send(LlmEvent::Done {
                stop_reason: StopReason::EndTurn,
                usage: Default::default(),
            })
            .await
            .unwrap();
        drop(second_provider_tx);
        tokio::time::timeout(std::time::Duration::from_secs(1), second_send)
            .await
            .expect("second explicit turn should terminate")
            .unwrap()
            .unwrap();
        assert!(agent.steering_inbox.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn max_tokens_continuation_prompt_forbids_repeating_large_write() {
        let provider = Arc::new(ScriptedProvider::new(vec![
            vec![
                LlmEvent::ToolUseDelta {
                    id: "call-large-write".into(),
                    name: "Write".into(),
                    input: None,
                },
                LlmEvent::Done {
                    stop_reason: StopReason::MaxTokens,
                    usage: Default::default(),
                },
            ],
            vec![LlmEvent::Done {
                stop_reason: StopReason::EndTurn,
                usage: Default::default(),
            }],
        ]));
        let mut agent = make_agent_with_provider(provider.clone());
        assert!(
            agent
                .engine
                .get_mut()
                .registry_mut()
                .register(Box::new(PreviewOnlyWriteTool)),
            "Write must be advertised in the request that emits its progress"
        );
        let mut rx = agent.subscribe();

        agent
            .send_message(SendMessageData {
                content: "create a polished single page site".into(),
                msg_id: "msg-1".into(),
                files: Vec::new(),
                inject_skills: Vec::new(),
                origin: None,
            })
            .await
            .unwrap();

        let requests = provider.requests();
        assert_eq!(requests.len(), 2);
        let continuation_text = requests[1]
            .messages
            .iter()
            .rev()
            .find(|message| message.role == Role::User)
            .and_then(|message| {
                message.content.iter().find_map(|block| match block {
                    ContentBlock::Text { text } => Some(text.as_str()),
                    _ => None,
                })
            })
            .expect("continuation request should contain a user text prompt");

        assert!(continuation_text.contains("Do not call Write with a full large file in one call"));
        assert!(continuation_text.contains("First create a small complete deliverable"));
        assert!(continuation_text.contains("append or edit in chunks"));
        assert!(continuation_text.contains("verify the target file exists"));

        let leaked_partial_call = std::iter::from_fn(|| rx.try_recv().ok()).any(|event| {
            matches!(
                event,
                AgentStreamEvent::ToolCall(data) if data.call_id == "nomi-call-large-write"
            )
        });
        assert!(
            !leaked_partial_call,
            "an uncommitted partial tool call must never enter the frontend lifecycle"
        );
    }

    #[cfg(feature = "browser-use")]
    #[test]
    fn yolo_session_installs_browser_approval_gate_without_takeover_pref() {
        let mgr = ToolApprovalManager::new();
        mgr.set_mode(SessionMode::Yolo);

        assert!(
            should_install_browser_approval_gate(false, false, &mgr),
            "full-auto/yolo sessions need the Browser gate so gated egress can approve without UI"
        );
    }

    #[cfg(feature = "browser-use")]
    #[test]
    fn default_session_without_browser_approval_pref_does_not_install_browser_gate() {
        let mgr = ToolApprovalManager::new();

        assert!(
            !should_install_browser_approval_gate(false, false, &mgr),
            "default sessions without Browser approval prefs should keep the old fail-closed path"
        );
    }

    #[tokio::test]
    async fn nomi_agent_returns_correct_type() {
        let agent = NomiAgentManager::new("conv-1".into(), "/project".into(), make_test_config(), None, None, None, None, Vec::new(), None, None, Vec::new(), false, None)
            .await
            .unwrap();
        assert_eq!(agent.agent_type(), AgentType::Nomi);
        assert_eq!(agent.workspace(), "/project");
        assert_eq!(agent.conversation_id(), "conv-1");
    }

    #[tokio::test]
    async fn manager_threads_embedded_agent_execution_host_composition_into_bootstrap() {
        let mut config = make_test_config();
        config.install_embedded_agent_execution = false;
        let agent = NomiAgentManager::new(
            "conv-no-embedded-execution".into(),
            "/project".into(),
            config,
            None,
            None,
            None,
            None,
            Vec::new(),
            None,
            None,
            Vec::new(),
            false,
            None,
        )
        .await
        .unwrap();

        let names = agent.engine.lock().await.tool_names();
        assert!(
            !names.iter().any(|name| name == "nomi_delegate"),
            "manager must preserve the backend host-composition decision"
        );
    }

    // -- distillation eligibility gates (construction-time) -------------------

    struct StubCompanionSink;

    #[async_trait::async_trait]
    impl nomi_agent::companion_tools::CompanionMemorySink for StubCompanionSink {
        async fn recall(&self, _conv: &str, _q: &str, _kind: Option<&str>, _archived: bool) -> Result<String, String> {
            Ok(String::new())
        }
        async fn save(&self, _conv: &str, _kind: &str, _content: &str, _tags: &[String]) -> Result<String, String> {
            Ok(String::new())
        }
        async fn recent_events(&self, _limit: usize) -> Result<String, String> {
            Ok(String::new())
        }
    }

    #[tokio::test]
    async fn companion_session_never_distills() {
        // Companion red line: a session with a companion sink must have NO
        // distill target, so no post-turn distillation child is admitted.
        let sink: Arc<dyn nomi_agent::companion_tools::CompanionMemorySink> = Arc::new(StubCompanionSink);
        let agent = NomiAgentManager::new(
            "conv-companion".into(),
            "/project".into(),
            make_test_config(),
            None,
            None,
            Some(sink),
            None,
            Vec::new(),
            None,
            None,
            Vec::new(),
            false,
            None,
        )
        .await
        .unwrap();
        assert!(
            agent.distill_dir_for_test().is_none(),
            "companion sessions must not distill into file-based memory"
        );
    }

    #[tokio::test]
    async fn normal_session_resolves_a_distill_dir() {
        // A normal work session (no companion sink) gets a project-level
        // memory dir as its distill target. The runtime origin gate and the
        // enable flag are checked separately at send time.
        let agent = NomiAgentManager::new(
            "conv-normal".into(),
            "/project".into(),
            make_test_config(),
            None,
            None,
            None,
            None,
            Vec::new(),
            None,
            None,
            Vec::new(),
            false,
            None,
        )
        .await
        .unwrap();
        // auto_memory_dir resolves unless the platform has no config dir; on
        // the test host it should be Some.
        assert!(
            agent.distill_dir_for_test().is_some(),
            "normal sessions should resolve a distill target dir"
        );
    }

    #[tokio::test]
    async fn nomi_agent_initial_status_is_pending() {
        let agent = NomiAgentManager::new("conv-1".into(), "/project".into(), make_test_config(), None, None, None, None, Vec::new(), None, None, Vec::new(), false, None)
            .await
            .unwrap();
        assert_eq!(agent.status(), Some(ConversationStatus::Pending));
    }

    #[tokio::test]
    async fn nomi_agent_subscribe_returns_receiver() {
        let agent = NomiAgentManager::new("conv-1".into(), "/project".into(), make_test_config(), None, None, None, None, Vec::new(), None, None, Vec::new(), false, None)
            .await
            .unwrap();
        let _rx = agent.subscribe();
    }

    #[tokio::test]
    async fn nomi_agent_kill_succeeds() {
        let agent = NomiAgentManager::new("conv-1".into(), "/project".into(), make_test_config(), None, None, None, None, Vec::new(), None, None, Vec::new(), false, None)
            .await
            .unwrap();
        assert!(agent.kill(None).is_ok());
        // Idle kill publishes Finished only after its exact process-tree fence.
        // Waiting for the terminal is intentionally unbounded; the registry
        // retains quarantine authority while teardown is unresolved.
        agent.runtime.wait_until_finished_unbounded().await;
        assert_eq!(agent.status(), Some(ConversationStatus::Finished));
    }

    #[tokio::test]
    async fn nomi_agent_kill_with_reason_succeeds() {
        let agent = NomiAgentManager::new("conv-1".into(), "/project".into(), make_test_config(), None, None, None, None, Vec::new(), None, None, Vec::new(), false, None)
            .await
            .unwrap();
        assert!(agent.kill(Some(AgentKillReason::IdleTimeout)).is_ok());
    }

    #[tokio::test]
    async fn nomi_agent_kill_running_turn_cancels_turn_token() {
        let agent = NomiAgentManager::new("conv-1".into(), "/project".into(), make_test_config(), None, None, None, None, Vec::new(), None, None, Vec::new(), false, None)
            .await
            .unwrap();
        agent.runtime.reset_for_new_turn(ConversationStatus::Running);

        let token = agent
            .turn_cancel
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone();
        assert!(!token.is_cancelled());

        agent
            .kill(Some(AgentKillReason::ConversationDeleted))
            .expect("kill should request stop");

        assert!(token.is_cancelled(), "running kill must cancel the active turn token");
    }

    #[tokio::test]
    async fn nomi_agent_kill_idle_turn_does_not_leave_stale_stop_signal() {
        let agent = NomiAgentManager::new("conv-1".into(), "/project".into(), make_test_config(), None, None, None, None, Vec::new(), None, None, Vec::new(), false, None)
            .await
            .unwrap();

        agent
            .kill(Some(AgentKillReason::ConversationDeleted))
            .expect("idle kill should be harmless");

        assert!(agent.closing.load(Ordering::Acquire));
    }

    #[tokio::test]
    async fn nomi_agent_kill_is_durable_and_rejects_raced_turn_admission() {
        let agent = make_agent_with_provider(Arc::new(ScriptedProvider::new(vec![])));

        agent
            .kill(Some(AgentKillReason::ConversationDeleted))
            .expect("kill should close the task");

        assert!(agent.closing.load(Ordering::Acquire));
        assert!(
            agent
                .turn_cancel
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .is_cancelled(),
            "kill cancellation must remain observable even without a registered waiter"
        );
        let error = agent
            .send_message(SendMessageData {
                content: "must not start".into(),
                msg_id: "raced-after-kill".into(),
                files: Vec::new(),
                inject_skills: Vec::new(),
                origin: None,
            })
            .await
            .expect_err("closed manager must reject a raced clone");
        assert!(
            error
                .stream_error()
                .detail
                .as_deref()
                .is_some_and(|detail| detail.contains("shutting down"))
        );
    }

    #[tokio::test]
    async fn kill_cancels_active_turn_and_rejects_second_queued_send() {
        let provider = Arc::new(BlockingProvider::new());
        let agent = Arc::new(make_agent_with_provider(provider.clone()));
        let first = {
            let agent = Arc::clone(&agent);
            tokio::spawn(async move {
                agent
                    .send_message(SendMessageData {
                        content: "first".into(),
                        msg_id: "first".into(),
                        files: Vec::new(),
                        inject_skills: Vec::new(),
                        origin: None,
                    })
                    .await
            })
        };
        provider.called.acquire().await.unwrap().forget();

        let second = {
            let agent = Arc::clone(&agent);
            tokio::spawn(async move {
                agent
                    .send_message(SendMessageData {
                        content: "second".into(),
                        msg_id: "second".into(),
                        files: Vec::new(),
                        inject_skills: Vec::new(),
                        origin: None,
                    })
                    .await
            })
        };
        tokio::time::sleep(std::time::Duration::from_millis(30)).await;

        agent
            .kill(Some(AgentKillReason::ConversationDeleted))
            .expect("kill should close every admitted send");

        let first_result = tokio::time::timeout(std::time::Duration::from_millis(200), first)
            .await
            .expect("active turn must observe kill")
            .unwrap();
        assert!(first_result.is_ok());
        assert!(second.await.unwrap().is_err(), "queued send must see closing task");
        assert_eq!(provider.calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn nomi_agent_confirmations_initially_empty() {
        let agent = NomiAgentManager::new("conv-1".into(), "/project".into(), make_test_config(), None, None, None, None, Vec::new(), None, None, Vec::new(), false, None)
            .await
            .unwrap();
        assert!(agent.get_confirmations().is_empty());
    }

    #[tokio::test]
    async fn nomi_agent_get_slash_commands_does_not_wait_for_engine_lock() {
        let agent = NomiAgentManager::new("conv-1".into(), "/project".into(), make_test_config(), None, None, None, None, Vec::new(), None, None, Vec::new(), false, None)
            .await
            .unwrap();

        let _engine_guard = agent.engine.lock().await;
        let commands = tokio::time::timeout(std::time::Duration::from_millis(50), agent.get_slash_commands())
            .await
            .expect("slash command metadata should not wait for an active turn execution")
            .unwrap();

        assert!(!commands.is_empty());
    }

    #[tokio::test]
    async fn nomi_agent_check_approval_returns_false_by_default() {
        let agent = NomiAgentManager::new("conv-1".into(), "/project".into(), make_test_config(), None, None, None, None, Vec::new(), None, None, Vec::new(), false, None)
            .await
            .unwrap();
        assert!(!agent.check_approval("any_action", None));
    }

    #[tokio::test]
    async fn idle_cancel_emits_finish_and_transitions() {
        // Cancelling an agent with no in-flight run must still emit a terminal
        // event and transition to Finished — otherwise a subscribed relay hangs
        // forever in a 'running' spinner because no Finish/Error ever arrives.
        // (Phase 0 F0.2)
        let agent = NomiAgentManager::new("conv-stop".into(), "/project".into(), make_test_config(), None, None, None, None, Vec::new(), None, None, Vec::new(), false, None)
            .await
            .unwrap();
        let mut rx = agent.subscribe();

        agent.cancel().await.unwrap();

        assert_eq!(agent.status(), Some(ConversationStatus::Finished));
        match rx.try_recv() {
            Ok(AgentStreamEvent::Finish(_)) => {}
            other => panic!("expected Finish on idle cancel, got {:?}", other),
        }
        // A later reusable turn replaces this cancelled token during admission.
        assert!(
            agent
                .turn_cancel
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .is_cancelled()
        );
    }

    #[tokio::test]
    async fn termination_guard_emits_finish_on_armed_drop() {
        // The guard backstops panics / unexpected early-returns in send_message:
        // if the turn unwinds without emitting a terminal event, dropping the
        // armed guard must still broadcast one so the relay does not hang. (F0.2)
        let rt = AgentRuntimeState::new("c-guard", "/w", 16);
        let mut rx = rt.subscribe();
        let backend_output_sink = Arc::new(BackendOutputSink::new(rt.event_sender()));
        let turn = rt.reset_for_new_turn(ConversationStatus::Running);
        let active_turn = Arc::new(std::sync::Mutex::new(Some(turn)));
        let lifecycle_gate = Arc::new(std::sync::Mutex::new(()));
        let steering_inbox = Arc::new(std::sync::Mutex::new(
            std::collections::VecDeque::from(["unconsumed steer".to_owned()]),
        ));
        backend_output_sink.emit_tool_call("guarded-call", "Write", "{}");
        assert!(matches!(rx.try_recv(), Ok(AgentStreamEvent::ToolCall(_))));
        {
            let _g = TurnTerminationGuard {
                runtime: rt.clone(),
                turn,
                active_turn: Arc::clone(&active_turn),
                lifecycle_gate,
                steering_inbox: Arc::clone(&steering_inbox),
                backend_output_sink,
                process_supervisor: None,
                mcp_managers: Vec::new(),
                turn_teardown_fence: Arc::new(TurnTeardownFence::new()),
                armed: true,
            };
        }
        match rx.try_recv() {
            Ok(AgentStreamEvent::ToolCall(data)) => {
                assert_eq!(data.call_id, "nomi-guarded-call");
                assert_eq!(data.status, ToolCallStatus::Error);
                assert_eq!(data.description.as_deref(), Some("Tool call cancelled"));
            }
            other => panic!("expected tool cleanup on armed drop, got {:?}", other),
        }
        match rx.try_recv() {
            Ok(AgentStreamEvent::Finish(_)) => {}
            other => panic!("expected Finish after tool cleanup, got {:?}", other),
        }
        assert_eq!(rt.status(), Some(ConversationStatus::Finished));
        assert!(active_turn.lock().unwrap().is_none());
        assert!(steering_inbox.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn termination_guard_exact_terminalization_absorbs_steering_and_emits_once() {
        // On the normal path the guard owns both the terminal event and the
        // exact generation's steering cleanup. Its later Drop must stay silent.
        let rt = AgentRuntimeState::new("c-guard2", "/w", 16);
        let mut rx = rt.subscribe();
        let backend_output_sink = Arc::new(BackendOutputSink::new(rt.event_sender()));
        let turn = rt.reset_for_new_turn(ConversationStatus::Running);
        let active_turn = Arc::new(std::sync::Mutex::new(Some(turn)));
        let lifecycle_gate = Arc::new(std::sync::Mutex::new(()));
        let steering_inbox = Arc::new(std::sync::Mutex::new(
            std::collections::VecDeque::from(["tail steer".to_owned()]),
        ));
        {
            let mut g = TurnTerminationGuard {
                runtime: rt.clone(),
                turn,
                active_turn: Arc::clone(&active_turn),
                lifecycle_gate,
                steering_inbox: Arc::clone(&steering_inbox),
                backend_output_sink,
                process_supervisor: None,
                mcp_managers: Vec::new(),
                turn_teardown_fence: Arc::new(TurnTeardownFence::new()),
                armed: true,
            };
            assert!(g.terminalize(|runtime, turn| {
                runtime.emit_finish_for_turn(turn, None, Some(TurnStopReason::EndTurn))
            }));
        }
        assert!(matches!(rx.try_recv(), Ok(AgentStreamEvent::Finish(_))));
        assert!(matches!(rx.try_recv(), Err(broadcast::error::TryRecvError::Empty)));
        assert!(active_turn.lock().unwrap().is_none());
        assert!(steering_inbox.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn runtime_can_emit_error_and_finish() {
        let agent = NomiAgentManager::new("conv-err".into(), "/project".into(), make_test_config(), None, None, None, None, Vec::new(), None, None, Vec::new(), false, None)
            .await
            .unwrap();
        let mut rx = agent.subscribe();

        agent.runtime.emit_error("test error");
        // emit_error sets status to Finished, so emit_finish is a no-op here.
        // We emit directly for the Finish broadcast path test:
        agent
            .runtime
            .emit(AgentStreamEvent::Finish(crate::protocol::events::FinishEventData {
                session_id: None,
                stop_reason: None,
            }));

        match rx.try_recv().unwrap() {
            AgentStreamEvent::Error(data) => assert_eq!(data.message, "test error"),
            other => panic!("Expected Error, got {:?}", other),
        }
        match rx.try_recv().unwrap() {
            AgentStreamEvent::Finish(_) => {}
            other => panic!("Expected Finish, got {:?}", other),
        }
    }

    #[test]
    fn nomi_provider_connection_error_is_user_llm_provider_error() {
        let send_error = nomi_engine_error_to_send_error(
            "Nomi agent error: Provider error: Connection error: Signable request error: failed to create canonical request"
                .to_owned(),
        );

        assert_eq!(
            send_error.code(),
            Some(nomifun_api_types::AgentErrorCode::UserLlmProviderConfigError)
        );
        assert_eq!(
            send_error.ownership(),
            Some(nomifun_api_types::AgentErrorOwnership::UserLlmProvider)
        );
        assert_eq!(send_error.stream_error().retryable, Some(false));
    }

    #[test]
    fn nomi_api_connection_error_is_user_llm_provider_network_error() {
        let send_error = nomi_engine_error_to_send_error(
            "Nomi agent error: API error: Connection error: error decoding response body".to_owned(),
        );

        assert_eq!(
            send_error.code(),
            Some(nomifun_api_types::AgentErrorCode::UserLlmProviderNetworkError)
        );
        assert_eq!(
            send_error.ownership(),
            Some(nomifun_api_types::AgentErrorOwnership::UserLlmProvider)
        );
        assert_eq!(send_error.stream_error().retryable, Some(true));
    }
}

#[cfg(test)]
mod knowledge_search_gate_tests {
    use super::should_register_knowledge_search;
    use nomifun_common::KnowledgeBaseId;

    #[test]
    fn registers_only_with_sink_and_bound_bases() {
        let kb_id = KnowledgeBaseId::new();
        assert!(should_register_knowledge_search(true, std::slice::from_ref(&kb_id)));
        assert!(!should_register_knowledge_search(true, &[]));
        assert!(!should_register_knowledge_search(false, &[kb_id]));
    }
}

#[cfg(test)]
mod knowledge_write_gate_tests {
    use super::should_register_knowledge_write;
    use nomifun_common::KnowledgeBaseId;

    #[test]
    fn registers_only_with_sink_and_bound_bases() {
        let bases = vec![(KnowledgeBaseId::new(), "Finance".to_owned())];
        assert!(should_register_knowledge_write(true, &bases));
        // No bound bases → nothing to write to, even with a sink.
        assert!(!should_register_knowledge_write(true, &[]));
        // No sink (write-back disabled or standalone) → never registered.
        assert!(!should_register_knowledge_write(false, &bases));
    }
}

#[cfg(test)]
mod knowledge_prelude_tests {
    use super::apply_knowledge_prelude;
    use super::prepend_knowledge_context;
    #[test]
    fn prepend_knowledge_context_formats_hits_and_passthrough_when_empty() {
        use nomi_agent::knowledge_tools::KnowledgeHit;
        assert_eq!(prepend_knowledge_context(&[], "hi".to_string()), "hi");

        let hits = vec![KnowledgeHit {
            kb_id: nomifun_common::KnowledgeBaseId::new(),
            handle: "h".into(),
            kb_name: "Docs".into(),
            rel_path: "a/b.md".into(),
            heading: "Title".into(),
            snippet: "the snippet".into(),
        }];
        let out = prepend_knowledge_context(&hits, "do the task".to_string());
        assert!(out.contains("Docs/a/b.md"));
        assert!(out.contains("Title"));
        assert!(out.contains("the snippet"));
        assert!(out.contains("handle: h"), "proactive hit must expose its opaque handle: {out}");
        assert!(
            out.contains("knowledge_read") && out.contains("unchanged"),
            "proactive guidance must tell the model to copy the handle unchanged: {out}"
        );
        assert!(out.ends_with("do the task"), "original message preserved at end");
    }
    #[test]
    fn prepends_when_present_and_passthrough_when_absent() {
        let out = apply_knowledge_prelude(Some("[KB: A]".into()), "do the thing");
        assert!(out.starts_with("[KB: A]"));
        assert!(out.ends_with("do the thing"));
        assert_eq!(apply_knowledge_prelude(None, "do the thing"), "do the thing");
        assert_eq!(apply_knowledge_prelude(Some(String::new()), "x"), "x");
    }
}

#[cfg(test)]
mod turn_completed_mapping_tests {
    use super::map_engine_stop_reason;
    use crate::protocol::events::TurnStopReason;
    use nomi_types::message::StopReason;

    #[test]
    fn maps_engine_stop_reason_to_normalized_turn_reason() {
        // A natural finish is a clean EndTurn.
        assert_eq!(map_engine_stop_reason(StopReason::EndTurn), TurnStopReason::EndTurn);
        // ToolUse as the terminal reason means the engine handed back between
        // tool batches without a refusal/limit — treat as a normal completion.
        assert_eq!(map_engine_stop_reason(StopReason::ToolUse), TurnStopReason::EndTurn);
        // Truncation by token budget — the turn did NOT accomplish its goal.
        assert_eq!(map_engine_stop_reason(StopReason::MaxTokens), TurnStopReason::MaxTokens);
        // Per-turn request cap — likewise a truncated turn.
        assert_eq!(map_engine_stop_reason(StopReason::MaxTurns), TurnStopReason::MaxTurnRequests);
    }
}

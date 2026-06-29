use std::path::PathBuf;
use std::sync::Arc;

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
use nomi_protocol::{ToolApprovalManager, ToolApprovalResult};
use nomifun_api_types::{AgentModeResponse, SlashCommandItem};
use nomifun_common::{
    AgentKillReason, AgentType, AppError, Confirmation, ConversationStatus, ErrorChain, TimestampMs, now_ms,
};
use serde_json::Value;
use tokio::sync::{Mutex, Notify, broadcast};
use tracing::{debug, error, info};

use crate::agent_runtime::AgentRuntime;
use crate::capability::backend_output_sink::BackendOutputSink;
use crate::capability::backend_protocol_sink::BackendProtocolSink;
use crate::protocol::events::{AgentStreamEvent, TurnCompletedEventData, TurnStopReason};
use crate::protocol::send_error::AgentSendError;
use crate::types::{NomiResolvedConfig, SendMessageData};

pub struct NomiAgentManager {
    runtime: AgentRuntime,
    engine: Mutex<AgentEngine>,
    /// Static slash command metadata captured at bootstrap so UI lookups do
    /// not wait behind an active `engine.run()` turn.
    slash_commands: Vec<SlashCommandItem>,
    /// Holds `Arc<McpManager>` instances alive for the duration of this agent's
    /// lifetime. The managers are not accessed after construction — they exist
    /// solely so their underlying MCP connections outlive the engine's event
    /// loop. Rust drops them here, in field-declaration order, after `engine`
    /// and `runtime` are dropped. See the explicit `Drop` impl below.
    #[allow(dead_code)] // intentional: lifetime-extension only; see Drop impl
    mcp_managers: Vec<Arc<McpManager>>,
    approval_manager: Arc<ToolApprovalManager>,
    confirmations: Arc<std::sync::RwLock<Vec<Confirmation>>>,
    /// Signalled by `cancel()` to abort an in-flight `engine.run()` via
    /// `tokio::select!` in `send_message()`.
    cancel_notify: Arc<Notify>,
    /// Mid-turn steering interjections pushed by `steer()` and drained by the
    /// engine at its loop boundaries. Shared (clone of this Arc handed to the
    /// engine via `set_steering_inbox` each turn). `std::sync::Mutex` — locked
    /// only for brief push/drain, never across an await.
    steering_inbox: Arc<std::sync::Mutex<std::collections::VecDeque<String>>>,
    /// Target directory for post-session memory distillation. `None` =
    /// this session never distills (companion red line, or no base dir).
    /// Set once at construction.
    distill_dir: Option<PathBuf>,
    /// Provider config snapshot reused for the background distillation call.
    distill_cfg: Arc<nomi_config::config::Config>,
    /// One-shot knowledge reminder prepended to the FIRST user turn of a session
    /// that has bound bases — keeps the retrieval protocol adjacent to the task
    /// (the system-prompt section alone is too far from the user message to
    /// reliably fire). Mirrors the ACP `KnowledgeContextHook`. `None` once
    /// consumed or when no bases are mounted.
    knowledge_prelude: std::sync::Mutex<Option<String>>,
    /// When set, each user message is augmented with auto-retrieved KB hits
    /// (proactive RAG) keyed on the message text. `(retrieval sink, bound kb_ids)`.
    knowledge_auto_rag: Option<(Arc<dyn nomi_agent::knowledge_tools::KnowledgeRetrievalSink>, Vec<String>)>,
}

impl Drop for NomiAgentManager {
    fn drop(&mut self) {
        // McpManagers are held alive by the `mcp_managers` field specifically
        // so they outlive the agent's event loop. No explicit cleanup is needed
        // here — the Arc drop path releases each McpManager's underlying MCP
        // connection. This impl exists to document the intentional Drop-order
        // semantics rather than as a lint escape hatch.
    }
}

/// Whether the knowledge_search tool should be registered for this session.
pub(crate) fn should_register_knowledge_search(has_sink: bool, kb_ids: &[String]) -> bool {
    has_sink && !kb_ids.is_empty()
}

/// Whether the native knowledge_write (回血) tool should be registered: a
/// write-back sink was wired AND the session actually has bound bases to write
/// to. The factory only passes a sink when write-back is enabled on the
/// binding, so this also gates on the user's opt-in.
pub(crate) fn should_register_knowledge_write(has_sink: bool, bases: &[(String, String)]) -> bool {
    has_sink && !bases.is_empty()
}

/// Tool name of the native knowledge write-back tool. Allow-listed past the
/// approval gate (DIRECT/STAGED writes go to the user's own managed base, and
/// companion/channel sessions have no confirmation UI), mirroring the companion
/// memory tools.
pub(crate) const KNOWLEDGE_WRITE_TOOL_NAME: &str = "knowledge_write";

/// Cap on race-tail re-runs within a single turn-claim. The race window is
/// sub-millisecond, so a tiny bound guarantees termination even if a steerer
/// pushes during every pass; any leftover after the cap is left in the inbox
/// and drained by the NEXT turn (late delivery, never lost).
const MAX_STEERING_RACE_TAIL_RERUNS: usize = 3;

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
         — open the full document with knowledge_read if you need more:]\n",
    );
    for h in hits {
        block.push_str(&format!(
            "- {}/{} § {}\n  {}\n",
            h.kb_name, h.rel_path, h.heading, h.snippet
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
        knowledge_kb_ids: Vec<String>,
        knowledge_prelude: Option<String>,
        knowledge_writeback_sink: Option<Arc<dyn nomi_agent::knowledge_tools::KnowledgeWritebackSink>>,
        knowledge_write_bases: Vec<(String, String)>,
        knowledge_writeback_staged: bool,
        companion_skill_sink: Option<Arc<dyn CompanionSkillSink>>,
    ) -> Result<Self, AppError> {
        let runtime = AgentRuntime::new(conversation_id.clone(), workspace.clone(), 128);

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

        let sink: Arc<dyn OutputSink> = Arc::new(
            BackendOutputSink::new(runtime.event_sender()).with_distill_dir(distill_dir.clone()),
        );

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

        // Make the engine compact against the provider's declared context
        // window when set (else keep the resolved default). Same value the
        // context-usage gauge reports as the denominator.
        config.compact.context_window = nomi_config::compact::resolve_context_window(
            config_extra.context_limit,
            config.compact.context_window,
        );

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

        // Snapshot the resolved provider config for the background distillation
        // call (the engine consumes `config` next). Cheap one-time clone.
        let distill_cfg = Arc::new(config.clone());

        // P3-X1: create the session's shared approval manager BEFORE bootstrap, and apply the
        // initial session mode here, so the native BrowserTool (constructed inside
        // bootstrap.build) receives this same Arc and reads the LIVE runtime mode through it —
        // a mid-session set_mode to yolo then arms the facade redline gate immediately, not
        // pinned to the construction-time auto_approve snapshot. The same Arc is installed on
        // the engine via set_approval_manager below, so facade and orchestration share one cell.
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
        if config_extra.browser_takeover {
            let gate = crate::manager::nomi::browser_approval::DesktopApprovalGate::new(
                runtime.event_sender(),
                confirmations.clone(),
                approval_manager.clone(),
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
                .register(Box::new(RequirementCompleteTool::new(sink.clone())));
            engine
                .registry_mut()
                .register(Box::new(RequirementUpdateStatusTool::new(sink)));
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
                let bound_kb_ids: Vec<String> =
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

        Ok(Self {
            runtime,
            engine: Mutex::new(engine),
            slash_commands,
            mcp_managers: result.mcp_managers,
            approval_manager,
            confirmations,
            cancel_notify: Arc::new(Notify::new()),
            steering_inbox: Arc::new(std::sync::Mutex::new(std::collections::VecDeque::new())),
            distill_dir,
            distill_cfg,
            knowledge_prelude: std::sync::Mutex::new(knowledge_prelude),
            knowledge_auto_rag,
        })
    }

    fn request_stop(&self, reason: Option<AgentKillReason>, operation: &'static str) {
        let was_running = self.runtime.status() == Some(ConversationStatus::Running);

        if let Ok(mut confs) = self.confirmations.write() {
            confs.clear();
        }

        if was_running {
            // An in-flight engine.run() exists: wake its select! so the turn's
            // None branch aborts cleanly and emits the terminal event.
            self.cancel_notify.notify_waiters();
        } else {
            // Idle / Pending / between turns: there is no in-flight run to wake,
            // so notify_waiters would be a no-op AND no terminal event would ever
            // be broadcast — a relay subscribed to this conversation would hang
            // forever in a 'running' spinner. Emit the terminal event directly.
            // Idempotent via AgentRuntime's absorbing-state guard (a later real
            // Finish is absorbed), and we deliberately do NOT notify_waiters here
            // so no stale cancellation signal leaks into the next turn. (F0.2)
            self.runtime
                .emit_finish_with_reason(None, Some(TurnStopReason::Cancelled));
        }

        info!(
            conversation_id = %self.runtime.conversation_id(),
            ?reason,
            was_running,
            operation,
            "Nomi stop signal requested"
        );
    }
}

#[async_trait::async_trait]
impl crate::agent_task::IAgentTask for NomiAgentManager {
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

    fn last_activity_at(&self) -> TimestampMs {
        self.runtime.last_activity_at()
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
        self.runtime.bump_activity();
        self.runtime.reset_for_new_turn(ConversationStatus::Running);

        // Backstop: guarantee a terminal event even if this turn unwinds
        // abnormally (engine panic / early-return). Disarmed on the normal path
        // after the real terminal event is emitted below. (Phase 0 F0.2)
        let mut term_guard = TurnTerminationGuard {
            runtime: self.runtime.clone(),
            armed: true,
        };

        let prelude = self
            .knowledge_prelude
            .lock()
            .expect("knowledge_prelude lock poisoned")
            .take();
        let content = apply_knowledge_prelude(prelude, &data.content);
        // Proactive RAG: retrieve top KB hits for this message and prepend them
        // so the model has domain context up front. Best-effort — any failure or
        // empty result leaves the message unchanged.
        let content = if let Some((sink, kb_ids)) = &self.knowledge_auto_rag {
            match sink.search(kb_ids, &data.content, 3).await {
                Ok(hits) if !hits.is_empty() => prepend_knowledge_context(&hits, content),
                _ => content,
            }
        } else {
            content
        };

        let mut engine = self.engine.lock().await;
        engine.set_steering_inbox(Some(self.steering_inbox.clone()));

        // Each iteration runs one engine pass. The in-engine injection points
        // catch ~all steering mid-turn; this loop only re-runs to absorb the
        // sub-millisecond tail where a steer landed AFTER the engine's final
        // drain but before run returned — so no interjection is ever lost, and
        // the whole thing stays inside one turn-claim (no 409).
        let mut run_input = content;
        let mut race_tail_reruns = 0usize;
        let result = loop {
            let r = if self.distill_cfg.tools.cooperative_cancel {
                // Cooperative cancel (F0.4, opt-in): install a token, cancel it on
                // the stop signal, and AWAIT the run to a clean finish instead of
                // dropping its future mid-flight — so `self.messages` stays
                // consistent. Trades a little mid-tool stop-latency (the run is
                // awaited, not dropped) for that consistency; the engine checks the
                // token between turns and while awaiting the model stream.
                let cancel = tokio_util::sync::CancellationToken::new();
                engine.set_cancel_token(Some(cancel.clone()));
                let notify = self.cancel_notify.clone();
                let canceller = {
                    let cancel = cancel.clone();
                    tokio::spawn(async move {
                        notify.notified().await;
                        cancel.cancel();
                    })
                };
                let res = engine.run(&run_input, &data.msg_id).await;
                canceller.abort();
                engine.set_cancel_token(None); // reset for the next reused turn
                if cancel.is_cancelled() {
                    info!(
                        conversation_id = %self.runtime.conversation_id(),
                        "Nomi engine.run() cooperatively cancelled by stop signal"
                    );
                    None
                } else {
                    Some(res)
                }
            } else {
                tokio::select! {
                    res = engine.run(&run_input, &data.msg_id) => Some(res),
                    _ = self.cancel_notify.notified() => {
                        info!(
                            conversation_id = %self.runtime.conversation_id(),
                            "Nomi engine.run() cancelled by stop signal"
                        );
                        engine.abort_current_turn("Tool execution canceled by user");
                        None
                    }
                }
            };

            // Race-tail: only a clean Ok can carry leftover steering worth a
            // re-run (a cancel/abort intentionally drops the turn). Bounded so a
            // continuous steerer cannot spin this forever; leftover past the cap
            // stays queued for the next turn.
            if let Some(Ok(_)) = &r {
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
                        run_input = leftover.join("\n\n");
                        continue;
                    }
                } else {
                    tracing::warn!(
                        conversation_id = %self.runtime.conversation_id(),
                        "Nomi steering race-tail cap reached; any leftover deferred to next turn"
                    );
                }
            }
            break r;
        };

        engine.set_steering_inbox(None);

        let elapsed_ms = now_ms() - started_at;
        self.runtime.bump_activity();

        let outcome = match result {
            Some(Ok(agent_result)) => {
                let stop_reason = map_engine_stop_reason(agent_result.stop_reason);
                info!(
                    conversation_id = %self.runtime.conversation_id(),
                    elapsed_ms,
                    turns = agent_result.turns,
                    input_tokens = agent_result.usage.input_tokens,
                    output_tokens = agent_result.usage.output_tokens,
                    ?stop_reason,
                    "Nomi engine.run() completed, emitting Finish"
                );

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

                // —— Post-session memory distillation (fire-and-forget) ——
                // Eligibility gates, cheapest first:
                //   1. host opt-in flag (token cost; default off)
                //   2. this session distills at all (distill_dir set; companion
                //      red line already zeroed it at construction)
                //   3. run-time origin empty (cron/autowork/idmm turns excluded,
                //      same rule as the collector's payload_origin red line)
                // All satisfied → snapshot the just-saved transcript, release
                // the engine lock, then spawn the background task. Distillation
                // never blocks the reply and never surfaces errors.
                let origin_is_human = data
                    .origin
                    .as_deref()
                    .map(str::trim)
                    .unwrap_or("")
                    .is_empty();
                if origin_is_human
                    && super::distill::distill_enabled()
                    && let Some(dir) = self.distill_dir.clone()
                {
                    let transcript = engine.messages_transcript();
                    drop(engine); // release the engine lock before the LLM call
                    let cfg = self.distill_cfg.clone();
                    tokio::spawn(async move {
                        super::distill::run_distill(cfg, dir, transcript).await;
                    });
                }

                self.runtime.emit_finish_with_reason(None, Some(stop_reason));
                Ok(())
            }
            Some(Err(e)) => {
                let error_msg = format!("Nomi agent error: {e}");
                error!(
                    conversation_id = %self.runtime.conversation_id(),
                    elapsed_ms,
                    error = %ErrorChain(&e),
                    "Nomi engine.run() failed, emitting Error+Finish"
                );
                let send_error = nomi_engine_error_to_send_error(error_msg);
                self.runtime.emit_error_data(send_error.stream_error().clone());
                self.runtime.emit_finish(None);
                Err(send_error)
            }
            None => {
                // User-initiated stop: a deliberate, clean termination — NOT a
                // fault. Emit Finish(Cancelled) instead of the historical
                // Error("Stopped by user") + Finish(None): the Error made
                // AutoWork classify the turn as failed (re-pend → re-claim →
                // re-inject seconds after the user pressed stop) and made IDMM
                // treat the stop as a recoverable stall (injecting a hidden
                // "Please continue."). Downstream consumers now see the
                // explicit Cancelled stop_reason and stand down.
                self.runtime.emit_finish_with_reason(None, Some(TurnStopReason::Cancelled));
                Ok(())
            }
        };

        // Normal completion: a real terminal event was emitted above, so the
        // backstop is no longer needed. (Phase 0 F0.2)
        term_guard.disarm();
        outcome
    }

    async fn cancel(&self) -> Result<(), AppError> {
        self.request_stop(None, "cancel");
        Ok(())
    }

    fn kill(&self, reason: Option<AgentKillReason>) -> Result<(), AppError> {
        self.request_stop(reason, "kill");
        Ok(())
    }
}

/// RAII backstop guaranteeing a terminal event is broadcast for a turn even if
/// `send_message` unwinds abnormally (engine panic / unexpected early-return).
/// On the normal path `send_message` emits the real terminal event and then
/// disarms the guard; on an abnormal unwind the still-armed guard fires on drop.
/// The emit is idempotent via `AgentRuntime`'s absorbing-state guard, so this can
/// never leak a spurious terminal event past a real one. (Phase 0 F0.2)
struct TurnTerminationGuard {
    runtime: AgentRuntime,
    armed: bool,
}

impl TurnTerminationGuard {
    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for TurnTerminationGuard {
    fn drop(&mut self) {
        if self.armed {
            self.runtime
                .emit_finish_with_reason(None, Some(TurnStopReason::Cancelled));
        }
    }
}

impl NomiAgentManager {
    pub fn kill_and_wait(
        &self,
        reason: Option<AgentKillReason>,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>> {
        let _ = crate::agent_task::IAgentTask::kill(self, reason);
        Box::pin(std::future::ready(()))
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

/// Nomi-specific operations reached through `AgentInstance::Nomi(..)`
/// matches in the routes + services.
impl NomiAgentManager {
    /// Push a user interjection into the running turn's steering inbox.
    /// Returns `Ok(true)` if a turn is live and the message was queued for
    /// mid-turn injection; `Ok(false)` if no turn is running (caller should
    /// fall back to a normal send). Never blocks on the engine lock.
    ///
    /// Teardown race: a push that passes the `Running` gate just as the turn
    /// ends (after the engine's final drain and the bounded race-tail) is NOT
    /// lost — it stays in the inbox, whose `Arc` persists on `self`, and the
    /// next turn's `set_steering_inbox` re-mounts the same queue, so the engine
    /// drains it then. Late delivery, never dropped (no-loss guarantee); we
    /// deliberately do not `clear()` on teardown.
    pub fn steer(&self, text: String) -> Result<bool, AppError> {
        if self.runtime.status() != Some(ConversationStatus::Running) {
            return Ok(false);
        }
        self.steering_inbox
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push_back(text);
        self.runtime.bump_activity();
        Ok(true)
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
        // Signal any in-flight engine.run() to abort so we don't clear
        // mid-turn; the engine lock below then waits for it to release.
        self.request_stop(None, "clear_context");
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
        self.request_stop(None, "rewind_last_turn");
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
    use crate::agent_task::IAgentTask;

    async fn assert_no_stop_signal(agent: &NomiAgentManager) {
        let notified = agent.cancel_notify.notified();
        tokio::pin!(notified);

        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(20), &mut notified)
                .await
                .is_err(),
            "idle stop must not leave a stale cancellation signal for the next turn"
        );
    }

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
            bedrock_config: None,
            computer_use: false,
            browser_use: false,
            browser_full_power: false,
            browser_persistent_login: false,
            browser_site_memory: false,
            browser_takeover: false,
            browser_visual_fallback: false,
            goal: None,
            browser_secret_vault: None,
            owner_token: None,
        }    }

    #[tokio::test]
    async fn nomi_agent_returns_correct_type() {
        let agent = NomiAgentManager::new("conv-1".into(), "/project".into(), make_test_config(), None, None, None, None, Vec::new(), None, None, Vec::new(), false, None)
            .await
            .unwrap();
        assert_eq!(agent.agent_type(), AgentType::Nomi);
        assert_eq!(agent.workspace(), "/project");
        assert_eq!(agent.conversation_id(), "conv-1");
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
        // distill target, so the background task never spawns.
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
        // Idle kill now emits a terminal event and transitions to Finished so a
        // subscribed relay cannot hang in a 'running' spinner (Phase 0 F0.2);
        // task-manager removal still owns the heavier lifecycle cleanup.
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
    async fn nomi_agent_kill_running_turn_sends_stop_signal() {
        let agent = NomiAgentManager::new("conv-1".into(), "/project".into(), make_test_config(), None, None, None, None, Vec::new(), None, None, Vec::new(), false, None)
            .await
            .unwrap();
        agent.runtime.reset_for_new_turn(ConversationStatus::Running);

        let notified = agent.cancel_notify.notified();
        tokio::pin!(notified);
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(20), &mut notified)
                .await
                .is_err()
        );

        agent
            .kill(Some(AgentKillReason::ConversationDeleted))
            .expect("kill should request stop");

        tokio::time::timeout(std::time::Duration::from_millis(50), &mut notified)
            .await
            .expect("running kill should wake in-flight turn");
    }

    #[tokio::test]
    async fn nomi_agent_kill_idle_turn_does_not_leave_stale_stop_signal() {
        let agent = NomiAgentManager::new("conv-1".into(), "/project".into(), make_test_config(), None, None, None, None, Vec::new(), None, None, Vec::new(), false, None)
            .await
            .unwrap();

        agent
            .kill(Some(AgentKillReason::ConversationDeleted))
            .expect("idle kill should be harmless");

        assert_no_stop_signal(&agent).await;
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
            .expect("slash command metadata should not wait for an active engine run")
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
        // ...but it must NOT leave a stale cancellation signal for the next turn.
        assert_no_stop_signal(&agent).await;
    }

    #[tokio::test]
    async fn termination_guard_emits_finish_on_armed_drop() {
        // The guard backstops panics / unexpected early-returns in send_message:
        // if the turn unwinds without emitting a terminal event, dropping the
        // armed guard must still broadcast one so the relay does not hang. (F0.2)
        let rt = AgentRuntime::new("c-guard", "/w", 16);
        let mut rx = rt.subscribe();
        {
            let _g = TurnTerminationGuard {
                runtime: rt.clone(),
                armed: true,
            };
        }
        match rx.try_recv() {
            Ok(AgentStreamEvent::Finish(_)) => {}
            other => panic!("expected Finish on armed drop, got {:?}", other),
        }
        assert_eq!(rt.status(), Some(ConversationStatus::Finished));
    }

    #[tokio::test]
    async fn termination_guard_silent_when_disarmed() {
        // On the normal path send_message disarms the guard after emitting the
        // real terminal event; a disarmed guard must stay silent on drop.
        let rt = AgentRuntime::new("c-guard2", "/w", 16);
        let mut rx = rt.subscribe();
        {
            let mut g = TurnTerminationGuard {
                runtime: rt.clone(),
                armed: true,
            };
            g.disarm();
        }
        assert!(matches!(
            rx.try_recv(),
            Err(broadcast::error::TryRecvError::Empty)
        ));
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
    #[test]
    fn registers_only_with_sink_and_bound_bases() {
        assert!(should_register_knowledge_search(true, &["kb1".into()]));
        assert!(!should_register_knowledge_search(true, &[]));
        assert!(!should_register_knowledge_search(false, &["kb1".into()]));
    }
}

#[cfg(test)]
mod knowledge_write_gate_tests {
    use super::should_register_knowledge_write;
    #[test]
    fn registers_only_with_sink_and_bound_bases() {
        let bases = vec![("kb1".to_owned(), "Finance".to_owned())];
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
            kb_id: "k".into(),
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

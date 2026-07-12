use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use nomi_config::compact::CompactConfig;
use nomi_config::config::Config;
use nomi_config::hooks::HookEngine;
use nomi_protocol::events::ToolCategory;
use nomi_providers::{LlmProvider, ProviderError, create_provider};
use nomi_tools::registry::ToolRegistry;
use nomi_types::llm::{LlmEvent, LlmRequest};
use nomi_types::message::{ContentBlock, Message, Role, StopReason, TokenUsage};
use nomi_types::skill_types::{ContextModifier, PlanModeTransition, effort_to_string};
use serde_json::Value;
use tracing::Instrument;

use crate::cache_diagnostics::{CacheBreakDetector, CacheDiagnostic, CacheStats};
use crate::compact::state::CompactState;
use crate::compact::{auto, emergency, estimate, micro};
use crate::confirm::ToolConfirmer;
use crate::orchestration::{
    ExecutionControl, SKIPPED_AFTER_PRIOR_ERROR, execute_tool_calls,
    execute_tool_calls_with_approval,
};
use crate::output::OutputSink;
use crate::plan::prompt as plan_prompt;
use crate::plan::state::PlanState;
use crate::session::{Session, SessionManager};

/// Decide how a prompt-cache-break diagnostic should surface to the user.
/// Returns the info-level message to emit, or `None` to stay silent.
///
/// All diagnostics — including a `FullMiss` — are gated behind the opt-in
/// `cache_diagnostics` flag and are INFO, never errors. A full miss is not a
/// failure: the prompt cache merely lapsed, most often a benign server-side TTL
/// expiry during the idle gap between turns (e.g. between AutoWork tasks).
/// Emitting it as an error previously made the AutoWork orchestrator treat a
/// perfectly good turn as failed (re-pend, and eventually a tag pause).
fn cache_diagnostic_message(diag: &CacheDiagnostic, diagnostics_enabled: bool) -> Option<String> {
    if !diagnostics_enabled {
        return None;
    }
    Some(match diag {
        CacheDiagnostic::FullMiss { cause } => format!("Cache full miss: {cause:?}"),
        CacheDiagnostic::PartialMiss { hit_rate, cause } => {
            format!("Cache: {:.0}% hit rate (cause: {cause:?})", hit_rate * 100.0)
        }
        CacheDiagnostic::Healthy { hit_rate } => {
            format!("Cache: {:.0}% hit rate", hit_rate * 100.0)
        }
    })
}

/// Maximum characters kept from a single tool-result body in the distillation
/// transcript. Tool outputs can be huge (file dumps, search results); the
/// distiller only needs a hint of what happened, not the full payload.
const TRANSCRIPT_TOOL_RESULT_MAX: usize = 600;

/// If the provider stream stays alive but silent for this long, surface a
/// lightweight progress event so the UI does not look frozen while the model is
/// generating a large tool-call argument.
const STREAM_IDLE_ACTIVITY_AFTER: Duration = Duration::from_millis(1_200);

/// Render the conversation history as a role-tagged plain-text transcript for
/// post-session memory distillation.
///
/// Rules (mirroring codex `serialize_filtered_rollout_response_items`):
/// - User / assistant text blocks are kept with a `[role]` prefix.
/// - Tool calls become `[tool <name>] <compact args>` (args truncated).
/// - Tool results become `[tool result(<err?>)] <body>` (body truncated to
///   `TRANSCRIPT_TOOL_RESULT_MAX`).
/// - Thinking blocks are dropped entirely.
fn render_transcript(messages: &[Message]) -> String {
    let mut out = String::new();
    for msg in messages {
        let role = match msg.role {
            Role::User => "user",
            Role::Assistant => "assistant",
            Role::System => "system",
            Role::Tool => "tool",
        };
        for block in &msg.content {
            match block {
                ContentBlock::Text { text } => {
                    let text = text.trim();
                    if text.is_empty() {
                        continue;
                    }
                    out.push_str(&format!("[{role}] {text}\n"));
                }
                ContentBlock::ToolUse { name, input, .. } => {
                    let args = input.to_string();
                    let args = truncate_chars(&args, TRANSCRIPT_TOOL_RESULT_MAX);
                    out.push_str(&format!("[tool {name}] {args}\n"));
                }
                ContentBlock::ToolResult {
                    content, is_error, ..
                } => {
                    let body = truncate_chars(content.trim(), TRANSCRIPT_TOOL_RESULT_MAX);
                    let tag = if *is_error { " error" } else { "" };
                    out.push_str(&format!("[tool result{tag}] {body}\n"));
                }
                // Drop thinking: it's reasoning scratch, not durable signal.
                ContentBlock::Thinking { .. } => {}
                // Images have no textual representation in the transcript.
                ContentBlock::Image { .. } => {}
            }
        }
    }
    out
}

/// Truncate `s` to at most `max` characters (char-boundary safe), appending an
/// ellipsis marker when truncation occurred.
fn truncate_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_owned();
    }
    let truncated: String = s.chars().take(max).collect();
    format!("{truncated}…(truncated)")
}

/// Hard safety-net turn cap applied when the session does not configure
/// `max_turns` (the production default is `None`). Without this, a model stuck
/// in a tool-call loop runs forever, burning tokens and appearing "stuck" to
/// the user. A user-configured `Some(n)` is always respected as-is — this only
/// bounds the otherwise-unbounded `None` case. Mirrors Claude Code's ~200-turn
/// guard. See docs/superpowers/specs/2026-06-21-nomi-agent-overhaul-design.md §5 F0.3.
const DEFAULT_SAFETY_MAX_TURNS: usize = 200;

/// Strictest image-count limit among the supported message providers. Amazon
/// Bedrock Converse rejects a request containing more than 20 images.
const MAX_PROVIDER_REQUEST_IMAGES: usize = 20;

#[derive(Debug, Default, PartialEq, Eq)]
struct ToolEfficiencyStats {
    model_turn_attempts: usize,
    model_turns_with_tools: usize,
    total_tool_calls: usize,
    max_calls_in_model_turn: usize,
    exec_command_script_calls: usize,
    batch_read_files_requested: usize,
    error_results: usize,
    skipped_after_prior_error: usize,
    cooperative_cancelled: bool,
}

impl ToolEfficiencyStats {
    fn observe_model_turn_attempt(&mut self) {
        self.model_turn_attempts = self.model_turn_attempts.saturating_add(1);
    }

    fn observe_calls(&mut self, registry: &ToolRegistry, blocks: &[ContentBlock]) {
        let calls = blocks
            .iter()
            .filter_map(|block| match block {
                ContentBlock::ToolUse { name, input, .. } => {
                    let input = registry
                        .get(name)
                        .map(|tool| {
                            nomi_tools::coerce_input_to_schema(&tool.input_schema(), input.clone())
                        })
                        .unwrap_or_else(|| input.clone());
                    Some((name.as_str(), input))
                }
                _ => None,
            })
            .collect::<Vec<_>>();
        if calls.is_empty() {
            return;
        }

        self.model_turns_with_tools = self.model_turns_with_tools.saturating_add(1);
        self.total_tool_calls = self.total_tool_calls.saturating_add(calls.len());
        self.max_calls_in_model_turn = self.max_calls_in_model_turn.max(calls.len());
        for (name, input) in &calls {
            if *name == "exec_command" && input.get("script").is_some() {
                self.exec_command_script_calls =
                    self.exec_command_script_calls.saturating_add(1);
            }
            if *name == "Read"
                && let Some(paths) = input.get("file_paths").and_then(Value::as_array)
            {
                self.batch_read_files_requested = self
                    .batch_read_files_requested
                    .saturating_add(paths.len());
            }
        }
    }

    fn observe_cooperative_cancellation(&mut self) {
        self.cooperative_cancelled = true;
    }

    fn terminal_dimensions(
        &self,
        result: &Result<AgentResult, AgentError>,
    ) -> (&'static str, &'static str, &'static str, usize) {
        if self.cooperative_cancelled {
            let turns = result
                .as_ref()
                .map(|result| result.turns)
                .unwrap_or(self.model_turn_attempts);
            return ("cancelled", "cancelled", "none", turns);
        }

        match result {
            Ok(result) => (
                "ok",
                match result.stop_reason {
                    StopReason::EndTurn => "end_turn",
                    StopReason::ToolUse => "tool_use",
                    StopReason::MaxTokens => "max_tokens",
                    StopReason::MaxTurns => "max_turns",
                },
                "none",
                result.turns,
            ),
            Err(error) => (
                "error",
                "error",
                match error {
                    AgentError::ApiError(_) => "api_error",
                    AgentError::Provider(_) => "provider_error",
                    AgentError::UserAborted => "user_aborted",
                    AgentError::ContextTooLong { .. } => "context_too_long",
                },
                self.model_turn_attempts,
            ),
        }
    }

    fn observe_results(&mut self, blocks: &[ContentBlock]) {
        for block in blocks {
            let ContentBlock::ToolResult {
                content, is_error, ..
            } = block
            else {
                continue;
            };
            if *is_error {
                self.error_results = self.error_results.saturating_add(1);
            }
            if *is_error && content == SKIPPED_AFTER_PRIOR_ERROR {
                self.skipped_after_prior_error =
                    self.skipped_after_prior_error.saturating_add(1);
            }
        }
    }

    fn log(
        &self,
        session_id: &str,
        msg_id: &str,
        result: &Result<AgentResult, AgentError>,
    ) {
        let (terminal, stop_reason, error_kind, agent_turns) =
            self.terminal_dimensions(result);
        tracing::info!(
            target: "nomi_agent::tool_efficiency",
            session_id,
            msg_id,
            agent_turns,
            stop_reason,
            terminal,
            error_kind,
            model_turn_attempts = self.model_turn_attempts,
            model_turns_with_tools = self.model_turns_with_tools,
            tool_calls_total = self.total_tool_calls,
            max_calls_in_model_turn = self.max_calls_in_model_turn,
            exec_command_script_calls = self.exec_command_script_calls,
            batch_read_files_requested = self.batch_read_files_requested,
            tool_error_results = self.error_results,
            skipped_after_prior_error = self.skipped_after_prior_error,
            "agent tool efficiency summary"
        );
    }
}

/// Consecutive turns with the identical tool-call signature that trip the
/// stagnation nudge. 3 is already a degenerate loop (same action, same args,
/// thrice) — well clear of legitimate retries/polling.
pub(crate) const STAGNATION_THRESHOLD: usize = 3;

pub struct AgentEngine {
    provider: Arc<dyn LlmProvider>,
    tools: ToolRegistry,
    messages: Vec<Message>,
    system_prompt: String,
    model: String,
    max_tokens: u32,
    max_turns: Option<usize>,
    total_usage: TokenUsage,
    thinking: Option<nomi_types::llm::ThinkingConfig>,
    /// Resolved provider compat settings (for capability validation)
    compat: nomi_config::compat::ProviderCompat,
    confirmer: Arc<Mutex<ToolConfirmer>>,
    hooks: Option<HookEngine>,
    session_manager: Option<SessionManager>,
    current_session: Option<Session>,
    output: Arc<dyn OutputSink>,
    current_msg_id: String,
    approval_manager: Option<Arc<nomi_protocol::ToolApprovalManager>>,
    protocol_writer: Option<Arc<dyn nomi_protocol::writer::ProtocolEmitter>>,
    allow_list: Vec<String>,
    /// Persisted reasoning effort, updated by skill context modifiers.
    /// Carried into each turn's LlmRequest.reasoning_effort.
    current_reasoning_effort: Option<String>,
    /// Compaction configuration (thresholds, enabled flag, etc.)
    compact_config: CompactConfig,
    /// Runtime compaction state (circuit breaker, last input tokens)
    compact_state: CompactState,
    /// Runtime plan mode state (active flag, pre-plan allow-list, plan file path)
    plan_state: PlanState,
    /// Shared flag read by EnterPlanMode/ExitPlanMode tools to validate transitions.
    /// Updated by the engine when processing PlanModeTransition modifiers.
    plan_active_flag: Option<Arc<AtomicBool>>,
    /// Prompt cache break detector for diagnostics.
    cache_detector: CacheBreakDetector,
    compaction_level: nomi_compact::CompactionLevel,
    toon_enabled: bool,
    /// How many recent image-bearing tool results keep their images.
    max_recent_images: usize,
    commands: crate::commands::CommandRegistry,
    /// Opt-in goal-driven continuation. `None` (the default) means the engine
    /// behaves exactly as before — no continuation, no `update_goal` tool.
    goal: Option<crate::goal::runtime::GoalRuntime>,
    /// Optional cooperative-cancellation token. When set (by the host manager),
    /// the engine checks it at the top of each turn and while awaiting the model
    /// stream, returning cleanly instead of being abruptly dropped mid-flight.
    /// `None` (the default) is byte-for-byte the previous behaviour. (F0.4)
    cancel_token: Option<tokio_util::sync::CancellationToken>,
    /// Detects degenerate loops (the identical tool call repeated turn after
    /// turn) and triggers a one-time corrective nudge. Always on — a safety net
    /// alongside the hard `max_turns` cap. (Loop-agent robustness)
    stagnation_guard: crate::loop_guard::StagnationGuard,
    /// Host-registered per-turn context sources (§3.5). Empty by default →
    /// system prompt unchanged; the backend registers contributors to inject
    /// dynamic context (knowledge RAG, memory, …) without the engine hard-coding
    /// each source.
    context_contributors: Vec<std::sync::Arc<dyn crate::context_contributor::ContextContributor>>,
    /// Optional steering inbox: a shared queue the host manager pushes
    /// mid-turn user interjections into. Drained at two loop boundaries
    /// (after a tool-result message, and when a turn would otherwise end)
    /// so the model sees the interjection on its next step WITHOUT a turn
    /// restart. `None` (the default) = byte-for-byte previous behaviour.
    /// Mirrors `cancel_token`'s shared-handle pattern.
    steering_inbox: Option<Arc<Mutex<std::collections::VecDeque<String>>>>,
    /// Owns every supervised command launched by this engine's command tools.
    /// Bootstrap installs it; direct/test constructors leave it empty.
    process_supervisor: Option<Arc<nomi_execution::ProcessSupervisor>>,
    /// transcript 长度锚点：最近一个 turn 的用户消息 push 之前的 messages.len()。
    /// 供 rewind_last_turn 把内存历史回退到最后一个用户 turn 之前（编辑最近一条
    /// 用户消息重跑）。压缩会重写整个 messages 使下标失效，故压缩时清空；
    /// clear_context 时一并清空。仅内存态，不持久化到 session。
    last_turn_start_len: Option<usize>,
}

impl AgentEngine {
    pub fn new(
        config: Config,
        tools: ToolRegistry,
        output: Arc<dyn OutputSink>,
        cwd: PathBuf,
    ) -> Self {
        let provider = create_provider(&config);
        Self::new_with_provider(provider, config, tools, output, cwd)
    }

    /// Create an engine with an externally-provided provider (for sub-agent sharing)
    pub fn new_with_provider(
        provider: Arc<dyn LlmProvider>,
        config: Config,
        tools: ToolRegistry,
        output: Arc<dyn OutputSink>,
        cwd: PathBuf,
    ) -> Self {
        let system_prompt = config.system_prompt.clone().unwrap_or_default();
        let confirmer =
            ToolConfirmer::new(config.tools.auto_approve, config.tools.allow_list.clone());

        let session_manager = if config.session.enabled {
            Some(SessionManager::new(
                config.session.directory.clone().into(),
                config.session.max_sessions,
            ))
        } else {
            None
        };

        let allow_list = config.tools.allow_list.clone();
        let compact_config = config.compact.clone();

        Self {
            provider,
            tools,
            messages: Vec::new(),
            system_prompt,
            model: config.model,
            max_tokens: config.max_tokens,
            max_turns: config.max_turns,
            total_usage: TokenUsage::default(),
            thinking: config.thinking,
            compat: config.compat.clone(),
            confirmer: Arc::new(Mutex::new(confirmer)),
            hooks: Some(HookEngine::new(config.hooks.clone(), cwd.clone())),
            session_manager,
            current_session: None,
            output,
            current_msg_id: String::new(),
            approval_manager: None,
            protocol_writer: None,
            allow_list,
            current_reasoning_effort: None,
            compact_config,
            compact_state: CompactState::new(),
            plan_state: PlanState::default(),
            plan_active_flag: None,
            cache_detector: CacheBreakDetector::new(),
            compaction_level: config.compact.compaction,
            toon_enabled: config.compact.toon,
            max_recent_images: config.tools.max_recent_images,
            commands: crate::commands::default_registry(),
            goal: None,
            cancel_token: None,
            stagnation_guard: crate::loop_guard::StagnationGuard::new(crate::engine::STAGNATION_THRESHOLD),
            context_contributors: Vec::new(),
            steering_inbox: None,
            process_supervisor: None,
            last_turn_start_len: None,
        }
    }

    /// Create from a resumed session
    pub fn resume(
        config: Config,
        tools: ToolRegistry,
        output: Arc<dyn OutputSink>,
        session: Session,
        cwd: PathBuf,
    ) -> Self {
        let provider = create_provider(&config);
        Self::resume_with_provider(provider, config, tools, output, session, cwd)
    }

    /// Create from a resumed session with an externally-provided provider
    pub fn resume_with_provider(
        provider: Arc<dyn LlmProvider>,
        config: Config,
        tools: ToolRegistry,
        output: Arc<dyn OutputSink>,
        session: Session,
        cwd: PathBuf,
    ) -> Self {
        let system_prompt = config.system_prompt.clone().unwrap_or_default();
        let confirmer =
            ToolConfirmer::new(config.tools.auto_approve, config.tools.allow_list.clone());

        let session_manager = if config.session.enabled {
            Some(SessionManager::new(
                config.session.directory.clone().into(),
                config.session.max_sessions,
            ))
        } else {
            None
        };

        let allow_list = config.tools.allow_list.clone();
        let compact_config = config.compact.clone();

        Self {
            provider,
            tools,
            messages: session.messages.clone(),
            system_prompt,
            model: config.model.clone(),
            max_tokens: config.max_tokens,
            max_turns: config.max_turns,
            total_usage: session.total_usage.clone(),
            thinking: config.thinking,
            compat: config.compat.clone(),
            confirmer: Arc::new(Mutex::new(confirmer)),
            hooks: Some(HookEngine::new(config.hooks.clone(), cwd)),
            session_manager,
            current_session: Some(session),
            output,
            current_msg_id: String::new(),
            approval_manager: None,
            protocol_writer: None,
            allow_list,
            current_reasoning_effort: None,
            compact_config,
            compact_state: CompactState::new(),
            plan_state: PlanState::default(),
            plan_active_flag: None,
            cache_detector: CacheBreakDetector::new(),
            compaction_level: config.compact.compaction,
            toon_enabled: config.compact.toon,
            max_recent_images: config.tools.max_recent_images,
            commands: crate::commands::default_registry(),
            goal: None,
            cancel_token: None,
            stagnation_guard: crate::loop_guard::StagnationGuard::new(crate::engine::STAGNATION_THRESHOLD),
            context_contributors: Vec::new(),
            steering_inbox: None,
            process_supervisor: None,
            last_turn_start_len: None,
        }
    }

    pub fn set_process_supervisor(
        &mut self,
        supervisor: Arc<nomi_execution::ProcessSupervisor>,
    ) {
        assert!(
            self.process_supervisor.is_none(),
            "process supervisor may only be installed once"
        );
        self.process_supervisor = Some(supervisor);
    }

    /// Explicitly wind down all command sessions owned by this engine.
    pub async fn shutdown_processes(&self) -> Option<nomi_execution::ShutdownReport> {
        let supervisor = self.process_supervisor.as_ref()?;
        Some(supervisor.shutdown().await)
    }

    pub fn compaction_level(&self) -> nomi_compact::CompactionLevel {
        self.compaction_level
    }

    /// Get a reference to the shared provider
    pub fn provider(&self) -> &Arc<dyn LlmProvider> {
        &self.provider
    }

    /// Get a reference to the resolved compat settings
    pub fn compat(&self) -> &nomi_config::compat::ProviderCompat {
        &self.compat
    }

    pub fn tool_names(&self) -> Vec<String> {
        self.tools.tool_names()
    }

    pub fn registry_mut(&mut self) -> &mut ToolRegistry {
        &mut self.tools
    }

    /// Enable goal-driven continuation (opt-in). Registers the `update_goal`
    /// tool and installs a `GoalRuntime` that injects a continuation prompt at
    /// each natural-termination point until the goal is proven complete /
    /// blocked, or the auto-continuation cap (or `max_turns`) is hit.
    pub fn set_goal(&mut self, objective: String, max_auto_continuations: usize) {
        let rt = crate::goal::runtime::GoalRuntime::new(objective, max_auto_continuations);
        self.tools
            .register(Box::new(crate::goal::tool::UpdateGoalTool::new(
                rt.shared_state(),
            )));
        self.goal = Some(rt);
    }

    /// Initialize a new session for this engine run
    pub fn init_session(
        &mut self,
        provider_name: &str,
        cwd: &str,
        session_id: Option<&str>,
    ) -> anyhow::Result<()> {
        if let Some(mgr) = &self.session_manager {
            let session = mgr.create(provider_name, &self.model, cwd, session_id)?;
            tracing::info!(target: "nomi_agent", session_id = %session.id, provider = %provider_name, model = %self.model, "session started");
            self.current_session = Some(session);
        }
        Ok(())
    }

    /// Get the current session ID (if sessions are enabled and initialized)
    pub fn current_session_id(&self) -> Option<String> {
        self.current_session.as_ref().map(|s| s.id.clone())
    }

    /// Current context occupancy: the last request's prompt token count
    /// (system + all messages + tool results). 0 before the first model call.
    /// Numerator for the context-usage gauge.
    pub fn context_tokens(&self) -> u64 {
        self.compact_state.last_input_tokens
    }

    /// The engine's effective context budget (what it compacts against).
    /// Denominator for the gauge. = CompactConfig.context_window.
    pub fn context_window(&self) -> u64 {
        self.compact_config.context_window as u64
    }

    /// Install (or clear) the cooperative-cancellation token for subsequent
    /// runs. When set, [`Self::run`] observes cancellation at the top of each
    /// turn and while awaiting the model stream, winding down cleanly with
    /// `self.messages` left consistent. Inert when `None`. (Phase 0 F0.4)
    pub fn set_cancel_token(&mut self, token: Option<tokio_util::sync::CancellationToken>) {
        self.cancel_token = token;
    }

    /// Install (or clear) the steering inbox. Called by the host manager
    /// before/after a turn, mirroring `set_cancel_token`.
    pub fn set_steering_inbox(
        &mut self,
        inbox: Option<Arc<Mutex<std::collections::VecDeque<String>>>>,
    ) {
        self.steering_inbox = inbox;
    }

    /// Take all currently-queued steering interjections (FIFO). Empty when
    /// no inbox is installed. Lock is held only for the drain; a poisoned
    /// lock degrades to its inner value rather than panicking the turn.
    fn drain_steering(&self) -> Vec<String> {
        match &self.steering_inbox {
            Some(inbox) => {
                let mut q = inbox.lock().unwrap_or_else(|e| e.into_inner());
                q.drain(..).collect()
            }
            None => Vec::new(),
        }
    }

    /// Register a per-turn [`ContextContributor`] (§3.5). The backend uses this
    /// to inject dynamic context (knowledge RAG, memory, …) into the system
    /// prompt without the engine hard-coding the source. No-op effect on prompts
    /// until at least one is registered.
    pub fn register_context_contributor(
        &mut self,
        contributor: std::sync::Arc<dyn crate::context_contributor::ContextContributor>,
    ) {
        self.context_contributors.push(contributor);
    }

    /// Get a reference to the output sink
    pub fn output(&self) -> &dyn OutputSink {
        self.output.as_ref()
    }

    /// A readable transcript of the conversation history (role-tagged text,
    /// with tool use / tool results compressed and truncated; thinking blocks
    /// dropped). Used by post-session memory distillation as a read-only
    /// snapshot — it never mutates engine state.
    pub fn messages_transcript(&self) -> String {
        render_transcript(&self.messages)
    }

    pub fn set_approval_manager(&mut self, mgr: Arc<nomi_protocol::ToolApprovalManager>) {
        self.approval_manager = Some(mgr);
    }

    pub fn set_protocol_writer(&mut self, writer: Arc<dyn nomi_protocol::writer::ProtocolEmitter>) {
        self.protocol_writer = Some(writer);
    }

    /// Set the initial reasoning effort override (used by sub-agents spawned with an effort override).
    pub fn set_initial_reasoning_effort(&mut self, effort: Option<String>) {
        self.current_reasoning_effort = effort;
    }

    /// Set the shared plan-mode active flag.
    ///
    /// This flag is shared with EnterPlanMode/ExitPlanMode tools so they can
    /// validate transitions (e.g. reject double-entry).  The engine updates
    /// the flag when processing `PlanModeTransition` context modifiers.
    pub fn set_plan_active_flag(&mut self, flag: Arc<AtomicBool>) {
        self.plan_active_flag = Some(flag);
    }

    /// Default thinking budget when "enabled" is requested without a specific budget.
    const DEFAULT_THINKING_BUDGET: u32 = 10_000;

    /// Apply a runtime config update received from the protocol layer.
    ///
    /// Returns a list of human-readable change descriptions for the Info event.
    /// Empty list means no fields were changed.
    pub fn apply_config_update(
        &mut self,
        model: Option<String>,
        thinking: Option<String>,
        thinking_budget: Option<u32>,
        effort: Option<String>,
        compaction: Option<String>,
    ) -> Vec<String> {
        let mut changes = Vec::new();

        if let Some(new_model) = model {
            let old = std::mem::replace(&mut self.model, new_model.clone());
            changes.push(format!("model: {old} → {new_model}"));
        }

        if let Some(thinking_str) = thinking {
            if !self.compat.supports_thinking() {
                changes.push("thinking: not supported by current provider".to_string());
            } else {
                match thinking_str.as_str() {
                    "enabled" => {
                        let budget = thinking_budget.unwrap_or(Self::DEFAULT_THINKING_BUDGET);
                        self.thinking = Some(nomi_types::llm::ThinkingConfig::Enabled {
                            budget_tokens: budget,
                        });
                        changes.push(format!("thinking: enabled (budget: {budget})"));
                    }
                    "disabled" => {
                        self.thinking = Some(nomi_types::llm::ThinkingConfig::Disabled);
                        changes.push("thinking: disabled".to_string());
                    }
                    other => {
                        changes.push(format!("thinking: ignored invalid value \"{other}\""));
                    }
                }
            }
        } else if let Some(new_budget) = thinking_budget
            && let Some(nomi_types::llm::ThinkingConfig::Enabled { budget_tokens }) =
                &mut self.thinking
        {
            *budget_tokens = new_budget;
            changes.push(format!("thinking budget: {new_budget}"));
        }

        if let Some(new_effort) = effort {
            if new_effort.is_empty() {
                self.current_reasoning_effort = None;
                changes.push("effort: cleared".to_string());
            } else if !self.compat.supports_effort() {
                changes.push("effort: not supported by current provider".to_string());
            } else {
                let levels = self.compat.effort_levels();
                if !levels.is_empty() && !levels.iter().any(|l| l == &new_effort) {
                    changes.push(format!(
                        "effort: invalid level \"{}\" (valid: {})",
                        new_effort,
                        levels.join(", ")
                    ));
                } else {
                    let old = self
                        .current_reasoning_effort
                        .replace(new_effort.clone())
                        .unwrap_or_else(|| "none".to_string());
                    changes.push(format!("effort: {old} → {new_effort}"));
                }
            }
        }

        if let Some(ref level_str) = compaction {
            match level_str.parse::<nomi_compact::CompactionLevel>() {
                Ok(new_level) => {
                    let old = self.compaction_level.to_string();
                    self.compaction_level = new_level;
                    changes.push(format!("compaction: {old} → {new_level}"));
                }
                Err(e) => {
                    changes.push(format!("compaction: invalid ({e})"));
                }
            }
        }

        changes
    }

    /// Handle a slash command. Returns `None` if input is not a recognized command.
    pub async fn handle_command(
        &mut self,
        input: &str,
    ) -> Option<Result<crate::commands::CommandResult, anyhow::Error>> {
        let input = input.trim();
        let without_slash = input.strip_prefix('/')?;
        let (name, args) = match without_slash.split_once(char::is_whitespace) {
            Some((n, rest)) => (n, rest.trim()),
            None => (without_slash, ""),
        };

        let cmd = self.commands.find(name)?;

        // We need to borrow self mutably for CommandContext while also
        // borrowing self.commands immutably (already done above via find()).
        // Use a raw pointer to break the borrow conflict — safe because
        // the command is not modified during execution.
        let cmd_ptr = cmd as *const dyn crate::commands::SlashCommand;

        let mut ctx = crate::commands::CommandContext {
            messages: &mut self.messages,
            compact_state: &mut self.compact_state,
            compact_config: &self.compact_config,
            provider: Arc::clone(&self.provider),
            model: &self.model,
            output: self.output.as_ref(),
            registry: &self.commands,
        };

        // SAFETY: cmd_ptr points to a command inside self.commands which is only
        // borrowed immutably and not mutated during execute().
        let result = unsafe { &*cmd_ptr }.execute(&mut ctx, args).await;
        Some(result)
    }

    /// Run the agent loop with user input
    pub async fn run(&mut self, user_input: &str, msg_id: &str) -> Result<AgentResult, AgentError> {
        let session_id = self
            .current_session
            .as_ref()
            .map(|s| s.id.clone())
            .unwrap_or_default();
        let span = tracing::info_span!(
            target: "nomi_agent",
            "agent_run",
            session_id = %session_id,
            msg_id = %msg_id,
        );
        let mut efficiency = ToolEfficiencyStats::default();
        async {
            let result = self.run_inner(user_input, msg_id, &mut efficiency).await;
            efficiency.log(&session_id, msg_id, &result);
            result
        }
        .instrument(span)
        .await
    }

    /// Return metadata for all registered slash commands.
    pub fn slash_command_list(&self) -> Vec<(String, String)> {
        self.commands
            .all()
            .iter()
            .map(|cmd| (cmd.name().to_string(), cmd.description().to_string()))
            .collect()
    }

    async fn run_inner(
        &mut self,
        user_input: &str,
        msg_id: &str,
        efficiency: &mut ToolEfficiencyStats,
    ) -> Result<AgentResult, AgentError> {
        // Slash command interception — before any LLM call
        if let Some(result) = self.handle_command(user_input).await {
            let cmd_name = user_input.split_whitespace().next().unwrap_or(user_input);
            return match result {
                Ok(crate::commands::CommandResult::Exit) => {
                    tracing::info!(command = cmd_name, "Slash command executed: exit");
                    Err(AgentError::UserAborted)
                }
                Ok(crate::commands::CommandResult::Continue) => {
                    tracing::info!(command = cmd_name, "Slash command executed");
                    Ok(AgentResult {
                        text: String::new(),
                        stop_reason: StopReason::EndTurn,
                        usage: TokenUsage::default(),
                        turns: 0,
                    })
                }
                Err(e) => {
                    tracing::error!(command = cmd_name, error = %e, "Slash command failed");
                    Err(AgentError::ApiError(e.to_string()))
                }
            };
        }

        self.current_msg_id = msg_id.to_string();
        self.output.emit_stream_start(msg_id);
        // 记录本 turn 的起始锚点（用户消息 push 之前），供 rewind_last_turn 回退。
        self.last_turn_start_len = Some(self.messages.len());
        self.messages.push(Message::now(
            Role::User,
            vec![ContentBlock::Text {
                text: user_input.to_string(),
            }],
        ));

        let mut turn: usize = 0;
        loop {
            // Hard safety net: an unconfigured (`None`) max_turns still gets a
            // bounded cap so a runaway tool-call loop cannot run forever. A
            // user-configured limit is respected verbatim.
            let limit = self.max_turns.unwrap_or(DEFAULT_SAFETY_MAX_TURNS);
            if turn >= limit {
                self.save_session();
                return Ok(AgentResult {
                    text: String::new(),
                    stop_reason: StopReason::MaxTurns,
                    usage: self.total_usage.clone(),
                    turns: turn,
                });
            }
            // Cooperative cancellation between turns: self.messages is consistent
            // here (the prior turn appended its tool results), so return cleanly.
            // Inert unless a token was installed via set_cancel_token. (F0.4)
            if let Some(token) = &self.cancel_token
                && token.is_cancelled()
            {
                efficiency.observe_cooperative_cancellation();
                self.save_session();
                return Ok(AgentResult {
                    text: String::new(),
                    stop_reason: StopReason::EndTurn,
                    usage: self.total_usage.clone(),
                    turns: turn,
                });
            }

            // Enforce the per-request provider ceiling on preloaded/resumed
            // history as well as newly appended tool results. This must happen
            // before compaction because autocompaction can itself call the
            // provider with the current conversation.
            self.prune_old_tool_images();

            // Pre-send token estimate (§3.1): feed the CURRENT message size into
            // the compaction watermark so a turn that grew large (a big tool
            // result, or a large first message) compacts BEFORE the request
            // rather than failing with PromptTooLong and wasting a round-trip.
            // Only ever RAISES the watermark, and reuses the existing autocompact
            // thresholds + circuit breaker, so it cannot over-compact a small
            // context or loop.
            let pre_send_estimate =
                estimate::estimate_tokens_from_messages(&self.messages);
            self.compact_state.last_input_tokens =
                self.compact_state.last_input_tokens.max(pre_send_estimate);

            // Run multi-level compaction before each API call.
            self.run_compaction().await?;

            // Build tool list: filter based on plan mode state
            let tools = if self.plan_state.is_active {
                // Plan mode: only Info-category tools (excluding EnterPlanMode)
                self.tools.to_tool_defs_filtered(|t| {
                    t.category() == ToolCategory::Info && t.name() != "EnterPlanMode"
                })
            } else {
                // Normal mode: all tools except ExitPlanMode
                self.tools
                    .to_tool_defs_filtered(|t| t.name() != "ExitPlanMode")
            };

            // Build system prompt: append plan mode instructions when active
            let system = if self.plan_state.is_active {
                format!(
                    "{}\n\n{}",
                    self.system_prompt,
                    plan_prompt::plan_mode_instructions()
                )
            } else {
                self.system_prompt.clone()
            };

            // §3.5: let registered contributors inject dynamic per-turn context
            // (knowledge RAG, memory, …). No-op when none are registered.
            let system = if self.context_contributors.is_empty() {
                system
            } else {
                let mut extras = Vec::new();
                for contributor in &self.context_contributors {
                    if let Some(extra) = contributor.pre_turn_context().await {
                        extras.push(extra);
                    }
                }
                crate::context_contributor::merge_pre_turn_context(system, extras)
            };

            // Record prompt state for cache diagnostics
            self.cache_detector.record_request(&system, &tools);

            let request = LlmRequest {
                model: self.model.clone(),
                system,
                messages: self.messages.clone(),
                tools,
                max_tokens: self.max_tokens,
                thinking: self.thinking.clone(),
                reasoning_effort: self.current_reasoning_effort.clone(),
            };

            efficiency.observe_model_turn_attempt();
            let stream_start = std::time::Instant::now();
            let mut rx = self.provider.stream(&request).await?;
            let mut assistant_text = String::new();
            let mut thinking_text = String::new();
            let mut thinking_signature: Option<String> = None;
            let mut tool_calls: Vec<ContentBlock> = Vec::new();
            let mut stop_reason = StopReason::EndTurn;
            let mut turn_usage = TokenUsage::default();

            let mut cancelled_midstream = false;
            let mut idle_activity_active = false;
            let mut first_token_logged = false;
            loop {
                let event = match &self.cancel_token {
                    Some(token) => {
                        tokio::select! {
                            biased;
                            _ = token.cancelled() => {
                                cancelled_midstream = true;
                                break;
                            }
                            ev = tokio::time::timeout(STREAM_IDLE_ACTIVITY_AFTER, rx.recv()) => ev,
                        }
                    }
                    None => tokio::time::timeout(STREAM_IDLE_ACTIVITY_AFTER, rx.recv()).await,
                };
                let event = match event {
                    Ok(event) => event,
                    Err(_) => {
                        if !idle_activity_active {
                            self.output
                                .emit_model_activity(&self.current_msg_id, "preparing");
                            idle_activity_active = true;
                        }
                        continue;
                    }
                };
                if idle_activity_active {
                    self.output
                        .emit_model_activity(&self.current_msg_id, "prepared");
                    idle_activity_active = false;
                }
                let Some(event) = event else { break };
                // Time-to-first-token: elapsed from issuing the request to the
                // first content-bearing event of the turn. Always logged at debug;
                // surfaced as INFO only when the user opted into cache diagnostics
                // (same gate as cache-break diagnostics). Purely observational.
                if !first_token_logged
                    && matches!(
                        &event,
                        LlmEvent::TextDelta(_)
                            | LlmEvent::ThinkingDelta(_)
                            | LlmEvent::ToolUse { .. }
                            | LlmEvent::ToolUseDelta { .. }
                    )
                {
                    first_token_logged = true;
                    let ttft_ms = stream_start.elapsed().as_millis();
                    tracing::debug!(
                        target: "nomi_agent",
                        ttft_ms,
                        turn = turn + 1,
                        "first token received"
                    );
                    if self.compact_config.cache_diagnostics {
                        self.output
                            .emit_info(&format!("TTFT: {ttft_ms} ms (turn {})", turn + 1));
                    }
                }
                match event {
                    LlmEvent::TextDelta(text) => {
                        self.output.emit_text_delta(&text, &self.current_msg_id);
                        assistant_text.push_str(&text);
                    }
                    LlmEvent::ToolUse {
                        id,
                        name,
                        input,
                        extra,
                    } => {
                        if id.trim().is_empty() {
                            tracing::error!(
                                target: "nomi_agent",
                                tool = %name,
                                "provider emitted tool call with empty tool_use_id"
                            );
                        } else {
                            tracing::debug!(
                                target: "nomi_agent",
                                tool_use_id = %id,
                                tool = %name,
                                "provider tool call received"
                            );
                        }
                        let input_str = serde_json::to_string(&input).unwrap_or_default();
                        self.output.emit_tool_call(&id, &name, &input_str);
                        tool_calls.push(ContentBlock::ToolUse {
                            id,
                            name,
                            input,
                            extra,
                        });
                    }
                    LlmEvent::ToolUseDelta { id, name, input } => {
                        let input_str = input
                            .as_ref()
                            .map(serde_json::to_string)
                            .and_then(Result::ok);
                        self.output
                            .emit_tool_call_delta(&id, &name, input_str.as_deref());
                    }
                    LlmEvent::ThinkingDelta(text) => {
                        self.output.emit_thinking(&text, &self.current_msg_id);
                        thinking_text.push_str(&text);
                    }
                    LlmEvent::ThinkingSignature(signature) => {
                        thinking_signature = Some(signature);
                    }
                    LlmEvent::Done {
                        stop_reason: sr,
                        usage,
                    } => {
                        stop_reason = sr;
                        turn_usage = usage;
                    }
                    LlmEvent::Error(e) => {
                        efficiency.observe_calls(&self.tools, &tool_calls);
                        return Err(AgentError::ApiError(e));
                    }
                }
            }

            efficiency.observe_calls(&self.tools, &tool_calls);
            if cancelled_midstream {
                // Cooperative cancel while awaiting the model stream: stop before
                // pushing this turn's assistant message so self.messages stays
                // consistent (no dangling tool_use). The host maps this to a
                // Finish(Cancelled) terminal event via the token. (Phase 0 F0.4)
                efficiency.observe_cooperative_cancellation();
                self.save_session();
                return Ok(AgentResult {
                    text: assistant_text,
                    stop_reason: StopReason::EndTurn,
                    usage: self.total_usage.clone(),
                    turns: turn + 1,
                });
            }

            self.total_usage.input_tokens += turn_usage.input_tokens;
            self.total_usage.output_tokens += turn_usage.output_tokens;
            self.total_usage.cache_creation_tokens += turn_usage.cache_creation_tokens;
            self.total_usage.cache_read_tokens += turn_usage.cache_read_tokens;

            // Track per-turn input tokens for compaction watermark.
            // Use max(provider_reported, local_estimate) as a safety net:
            // some providers (e.g. DeepSeek with prefix caching) underreport
            // prompt_tokens, causing compaction to never trigger.
            let local_estimate = estimate::estimate_tokens_from_messages(&self.messages);
            let effective_watermark = turn_usage.input_tokens.max(local_estimate);

            if local_estimate > turn_usage.input_tokens
                && local_estimate.saturating_sub(turn_usage.input_tokens) > 10_000
            {
                self.output.emit_info(&format!(
                    "Token watermark override: provider={}, local_estimate={}, using={}",
                    turn_usage.input_tokens, local_estimate, effective_watermark
                ));
            }

            self.compact_state.last_input_tokens = effective_watermark;

            // Cache break detection
            let cache_stats = CacheStats {
                input_tokens: turn_usage.input_tokens,
                cache_read_tokens: turn_usage.cache_read_tokens,
                cache_creation_tokens: turn_usage.cache_creation_tokens,
            };
            if let Some(diagnostic) = self.cache_detector.check_response(cache_stats) {
                // A cache break is a diagnostic, not an error: surface it as INFO
                // only when the user opted into cache diagnostics. Never emit_error
                // here — a benign TTL expiry must not look like a failed turn to
                // the AutoWork orchestrator.
                if let Some(msg) = cache_diagnostic_message(&diagnostic, self.compact_config.cache_diagnostics) {
                    self.output.emit_info(&msg);
                }
            }

            let mut assistant_content: Vec<ContentBlock> = Vec::new();
            if !thinking_text.is_empty() || thinking_signature.is_some() {
                assistant_content.push(ContentBlock::Thinking {
                    thinking: thinking_text,
                    signature: thinking_signature,
                });
            }
            if !assistant_text.is_empty() {
                assistant_content.push(ContentBlock::Text {
                    text: assistant_text.clone(),
                });
            }
            assistant_content.extend(tool_calls.clone());

            self.messages
                .push(Message::now(Role::Assistant, assistant_content));

            if tool_calls.is_empty() {
                // Steering interjection (point B): a user message injected
                // mid-turn extends a would-end turn instead of returning, so
                // the model incorporates it on the next step. Mirrors the
                // goal-continuation below; valid ordering (assistant→user).
                let steered = self.drain_steering();
                if !steered.is_empty() {
                    for text in steered {
                        self.messages
                            .push(Message::now(Role::User, vec![ContentBlock::Text { text }]));
                    }
                    self.save_session();
                    turn += 1;
                    continue;
                }

                // Goal-driven continuation hook (only fires for opt-in goal
                // sessions). Compute the continuation first so the immutable
                // borrow of `self.goal` ends before we mutate `self.messages`.
                let continuation = self.goal.as_ref().and_then(|g| g.maybe_continuation());
                if let Some(cont) = continuation {
                    self.messages.push(cont);
                    self.save_session();
                    turn += 1;
                    continue; // don't return — run another turn toward the goal
                }
                self.save_session();
                return Ok(AgentResult {
                    text: assistant_text,
                    stop_reason,
                    usage: self.total_usage.clone(),
                    turns: turn + 1,
                });
            }

            // Loop-stagnation guard: observe this turn's tool-call signature
            // before executing. If the model has issued the identical call(s)
            // STAGNATION_THRESHOLD turns running, the nudge is appended to this
            // turn's tool-result message below so the model course-corrects next
            // turn. (Computed here while `tool_calls` is in scope.)
            let stagnation_nudge = self
                .stagnation_guard
                .observe(crate::loop_guard::tool_calls_signature(&tool_calls));

            let outcome = if let Some(ref approval_mgr) = self.approval_manager {
                // JSON stream mode: use protocol-based approval
                let writer = self
                    .protocol_writer
                    .as_ref()
                    .expect("protocol writer required for approval");
                let auto_approve = self.confirmer.lock().unwrap().is_auto_approve();
                match execute_tool_calls_with_approval(
                    &self.tools,
                    &tool_calls,
                    approval_mgr,
                    writer,
                    &self.current_msg_id,
                    auto_approve,
                    &self.allow_list,
                    self.hooks.as_mut(),
                    self.compaction_level,
                    self.toon_enabled,
                )
                .await
                {
                    Ok(o) => o,
                    Err(ExecutionControl::Quit) => {
                        self.save_session();
                        return Err(AgentError::UserAborted);
                    }
                }
            } else {
                // Terminal mode: use interactive confirmation
                match execute_tool_calls(
                    &self.tools,
                    &tool_calls,
                    &self.confirmer,
                    self.hooks.as_mut(),
                    self.compaction_level,
                    self.toon_enabled,
                )
                .await
                {
                    Ok(o) => o,
                    Err(ExecutionControl::Quit) => {
                        self.save_session();
                        return Err(AgentError::UserAborted);
                    }
                }
            };
            efficiency.observe_results(&outcome.results);

            // Apply any context modifiers from skill executions before the next turn
            self.apply_context_modifiers(&outcome.modifiers);

            // Display tool results
            for result in &outcome.results {
                if let ContentBlock::ToolResult {
                    tool_use_id,
                    content,
                    is_error,
                    ..
                } = result
                {
                    let tool_name = tool_calls
                        .iter()
                        .find_map(|c| {
                            if let ContentBlock::ToolUse { id, name, .. } = c
                                && id == tool_use_id
                            {
                                return Some(name.as_str());
                            }
                            None
                        })
                        .unwrap_or("unknown");
                    let status = if *is_error { "error" } else { "completed" };
                    if tool_use_id.trim().is_empty() {
                        tracing::error!(
                            target: "nomi_agent",
                            tool = %tool_name,
                            status,
                            "tool result has empty tool_use_id"
                        );
                    } else {
                        tracing::debug!(
                            target: "nomi_agent",
                            tool_use_id = %tool_use_id,
                            tool = %tool_name,
                            status,
                            "tool result emitted"
                        );
                    }
                    self.output
                        .emit_tool_result(tool_use_id, tool_name, *is_error, content);
                }
            }

            // Append the stagnation nudge (if any) as a trailing text block on
            // this turn's user/tool-result message, so the model sees it next
            // turn without creating a second consecutive user message.
            let mut tool_result_blocks = outcome.results;
            if stagnation_nudge {
                tracing::warn!(
                    target: "nomi_agent",
                    "loop-stagnation guard fired: identical tool call(s) repeated {STAGNATION_THRESHOLD}x — injecting corrective nudge"
                );
                tool_result_blocks.push(ContentBlock::Text {
                    text: crate::loop_guard::STAGNATION_NUDGE.to_string(),
                });
            }
            // Steering interjection (point A): append any queued steer messages
            // as trailing Text blocks on THIS turn's tool-result message, so the
            // model sees them next turn without a second consecutive user
            // message. Mirrors the stagnation nudge above.
            for text in self.drain_steering() {
                tool_result_blocks.push(ContentBlock::Text { text });
            }
            self.messages
                .push(Message::now(Role::User, tool_result_blocks));
            self.prune_old_tool_images();

            // Save session after each turn
            self.save_session();
            turn += 1;
        }
    }

    /// Keep at most `max_recent_images` individual images, additionally bounded
    /// by the strictest supported provider request limit. The text part of each
    /// result is preserved.
    fn prune_old_tool_images(&mut self) {
        let mut keep = self.max_recent_images.min(MAX_PROVIDER_REQUEST_IMAGES);
        for msg in self.messages.iter_mut().rev() {
            for block in msg.content.iter_mut().rev() {
                if let ContentBlock::ToolResult {
                    content, images, ..
                } = block
                    && !images.is_empty()
                {
                    if keep == 0 {
                        let removed = images.len();
                        images.clear();
                        content.push_str(&format!(
                            "\n({removed} image attachment(s) from this tool result were omitted by the recent-image/provider request limit.)"
                        ));
                    } else if images.len() > keep {
                        let retained = keep;
                        let removed = images.len() - retained;
                        images.truncate(keep);
                        keep = 0;
                        content.push_str(&format!(
                            "\n(Only the first {retained} image attachment(s) in this tool result remain; {removed} later attachment(s) were omitted by the recent-image/provider request limit.)"
                        ));
                    } else {
                        keep -= images.len();
                    }
                }
            }
        }
    }

    /// Run the multi-level compaction pipeline before each API call.
    ///
    /// Execution order: microcompact → autocompact → emergency check.
    /// After a successful autocompact the emergency check is skipped
    /// because the context has been significantly reduced.
    async fn run_compaction(&mut self) -> Result<(), AgentError> {
        // 1. Microcompact (lightweight, no LLM call)
        if micro::should_microcompact(&self.messages, &self.compact_config) {
            let result = micro::microcompact(&mut self.messages, &self.compact_config);
            if result.cleared_count > 0 {
                self.output.emit_info(&format!(
                    "Microcompact: cleared {} tool results (~{} tokens freed)",
                    result.cleared_count, result.estimated_tokens_freed
                ));
            }
        }

        // 2. Autocompact (LLM summarization)
        let mut compacted = false;
        let should_compact =
            auto::should_autocompact(self.compact_state.last_input_tokens, &self.compact_config);
        if should_compact {
            tracing::info!(target: "nomi_agent", last_input_tokens = self.compact_state.last_input_tokens, "context compaction triggered");
            let threshold = if let Some(pct) = self.compact_config.autocompact_threshold_pct {
                let t = self.compact_config.context_window * pct as usize / 100;
                self.output.emit_info(&format!(
                    "Autocompact threshold: {} tokens ({}% of {})",
                    t, pct, self.compact_config.context_window
                ));
                t
            } else {
                self.compact_config
                    .context_window
                    .saturating_sub(self.compact_config.output_reserve)
                    .saturating_sub(self.compact_config.autocompact_buffer)
            };
            let _ = threshold;
        }
        if should_compact && !self.compact_state.is_circuit_broken(&self.compact_config) {
            let provider = Arc::clone(&self.provider);
            match auto::autocompact(
                provider.as_ref(),
                &self.messages,
                &self.model,
                &self.compact_config,
                &mut self.compact_state,
            )
            .await
            {
                Ok(result) => {
                    self.output.emit_info(&format!(
                        "Autocompact: summarized {} messages ({} tokens → compact)",
                        result.messages_summarized, result.pre_compact_tokens
                    ));
                    self.messages = result.messages;
                    self.last_turn_start_len = None;
                    compacted = true;
                }
                Err(auto::CompactError::CircuitBroken { .. }) => {
                    // Already tripped; logged at circuit-breaker level
                }
                Err(e) => {
                    self.output
                        .emit_warning(&format!("Autocompact failed: {}", e));
                }
            }
        } else if should_compact {
            self.output.emit_info(&format!(
                "Autocompact: skipped (circuit breaker tripped after {} consecutive failures, \
                 last_input_tokens={})",
                self.compact_state.consecutive_failures, self.compact_state.last_input_tokens
            ));
        } else if !self.compact_config.enabled {
            let threshold = if let Some(pct) = self.compact_config.autocompact_threshold_pct {
                self.compact_config.context_window * pct as usize / 100
            } else {
                self.compact_config
                    .context_window
                    .saturating_sub(self.compact_config.output_reserve)
                    .saturating_sub(self.compact_config.autocompact_buffer)
            };
            if self.compact_state.last_input_tokens as usize >= threshold {
                self.output.emit_info(&format!(
                    "Autocompact: disabled (compact.enabled=false, \
                     last_input_tokens={}, threshold={})",
                    self.compact_state.last_input_tokens, threshold
                ));
            }
        }

        // 3. Emergency check (skip if autocompact just succeeded)
        if !compacted
            && emergency::is_at_emergency_limit(
                self.compact_state.last_input_tokens,
                &self.compact_config,
            )
        {
            return Err(AgentError::ContextTooLong {
                input_tokens: self.compact_state.last_input_tokens,
                limit: self
                    .compact_config
                    .context_window
                    .saturating_sub(self.compact_config.emergency_buffer),
            });
        }

        Ok(())
    }

    /// Run stop hooks when the agent session ends
    pub async fn run_stop_hooks(&self) {
        if let Some(hook_engine) = &self.hooks {
            let messages = hook_engine.run_stop().await;
            for msg in messages {
                tracing::info!(target: "nomi_agent", hook_message = %msg, "stop hook output");
            }
        }
    }

    /// Apply context modifiers collected from skill tool executions.
    fn apply_context_modifiers(&mut self, modifiers: &[Option<ContextModifier>]) {
        for modifier in modifiers.iter().flatten() {
            if let Some(ref model) = modifier.model {
                self.model = model.clone();
            }
            if let Some(effort) = modifier.effort {
                self.current_reasoning_effort = Some(effort_to_string(effort));
            }
            for tool_name in &modifier.allowed_tools {
                if !self.allow_list.contains(tool_name) {
                    self.allow_list.push(tool_name.clone());
                }
                self.confirmer.lock().unwrap().add_to_allow_list(tool_name);
            }

            // Handle plan mode transitions
            if let Some(ref transition) = modifier.plan_mode_transition {
                match transition {
                    PlanModeTransition::Enter => {
                        self.plan_state.pre_plan_allow_list = self.allow_list.clone();
                        self.plan_state.is_active = true;
                        if let Some(ref flag) = self.plan_active_flag {
                            flag.store(true, Ordering::Release);
                        }
                    }
                    PlanModeTransition::Exit { .. } => {
                        self.plan_state.is_active = false;
                        self.allow_list = self.plan_state.pre_plan_allow_list.clone();
                        if let Some(ref flag) = self.plan_active_flag {
                            flag.store(false, Ordering::Release);
                        }
                    }
                }
            }
        }
    }

    fn save_session(&mut self) {
        if let (Some(mgr), Some(session)) = (&self.session_manager, &mut self.current_session) {
            session.messages = self.messages.clone();
            session.total_usage = self.total_usage.clone();
            session.updated_at = chrono::Utc::now();
            if let Err(e) = mgr.save(session) {
                self.output
                    .emit_warning(&format!("Failed to save session: {}", e));
            }
            if let Err(e) = mgr.update_index_for(session) {
                self.output
                    .emit_warning(&format!("Failed to update session index: {}", e));
            }
        }
    }

    /// Stamp the owning-conversation token onto the current session and persist
    /// it. Idempotent (no-op when already equal, or when `token` is `None`).
    /// Called right after a session is created or resumed so the
    /// per-conversation-instance identity (see [`crate::session::Session::owner_token`])
    /// is written to disk — resume paths reject a stale session left by a prior
    /// conversation that reused this id.
    pub fn stamp_owner_token(&mut self, token: Option<String>) {
        let Some(token) = token else { return };
        let needs = match &self.current_session {
            Some(s) => s.owner_token.as_deref() != Some(token.as_str()),
            None => false,
        };
        if !needs {
            return;
        }
        if let Some(s) = &mut self.current_session {
            s.owner_token = Some(token);
        }
        self.save_session();
    }

    /// Clear the conversation context: drop all in-memory messages, reset
    /// compaction state and accumulated token usage, and persist the now-empty
    /// session so a process restart does not reload the old history.
    ///
    /// This is the engine-level primitive behind the backend "clear context"
    /// operation (mirrors the interactive `/clear` slash command, which mutates
    /// the same `messages` + `compact_state`). The session id is preserved so
    /// the conversation keeps its identity; only its contents are emptied.
    pub fn clear_context(&mut self) {
        self.messages.clear();
        self.last_turn_start_len = None;
        self.compact_state = CompactState::new();
        self.total_usage = TokenUsage::default();
        self.save_session();
    }

    /// 把内存 transcript 回退到最近一个 turn 的用户消息之前（丢弃最后一个用户
    /// turn 及其后内容），用于"编辑最近一条用户消息并重跑"。成功返回 true；
    /// 无有效锚点（如已被压缩清空、或越界）返回 false，调用方应回退处理。
    pub fn rewind_last_turn(&mut self) -> bool {
        let Some(start) = self.last_turn_start_len else {
            return false;
        };
        if start > self.messages.len() {
            self.last_turn_start_len = None;
            return false;
        }
        self.messages.truncate(start);
        self.last_turn_start_len = None;
        self.save_session();
        true
    }

    /// Close a partially recorded turn after the host cancels execution.
    ///
    /// Providers in the Anthropic family require every assistant `tool_use` to
    /// be followed immediately by user `tool_result` blocks. If the host drops
    /// `run()` while tools are executing, the assistant `tool_use` message may
    /// already be in memory without its matching results. Add synthetic error
    /// results so the next request can safely reuse this history.
    pub fn abort_current_turn(&mut self, reason: &str) {
        let Some(last_message) = self.messages.last() else {
            return;
        };
        if last_message.role != Role::Assistant {
            return;
        }

        let pending_results: Vec<_> = last_message
            .content
            .iter()
            .filter_map(|block| {
                let ContentBlock::ToolUse { id, name, .. } = block else {
                    return None;
                };
                Some((id.clone(), name.clone()))
            })
            .collect();

        if pending_results.is_empty() {
            return;
        }

        let result_blocks = pending_results
            .into_iter()
            .map(|(tool_use_id, name)| {
                tracing::info!(
                    target: "nomi_agent",
                    tool_use_id = %tool_use_id,
                    tool = %name,
                    "closing pending tool_use after abort"
                );
                self.output
                    .emit_tool_result(&tool_use_id, &name, true, reason);
                ContentBlock::ToolResult {
                    tool_use_id,
                    content: reason.to_string(),
                    is_error: true,
                    images: Vec::new(),
                }
            })
            .collect();

        self.messages.push(Message::now(Role::User, result_blocks));
        self.save_session();
    }
}

impl Drop for AgentEngine {
    fn drop(&mut self) {
        let Some(supervisor) = self.process_supervisor.take() else {
            return;
        };
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            runtime.spawn(async move {
                let _ = supervisor.shutdown().await;
            });
        } else {
            let _ = std::thread::Builder::new()
                .name("nomi-engine-process-cleanup".to_owned())
                .spawn(move || {
                    let Ok(runtime) = tokio::runtime::Builder::new_current_thread()
                        .enable_all()
                        .build()
                    else {
                        return;
                    };
                    let _ = runtime.block_on(supervisor.shutdown());
                });
        }
    }
}

// ---------------------------------------------------------------------------
// set_config tests — apply_config_update()
// ---------------------------------------------------------------------------

#[cfg(test)]
mod set_config_tests {
    use std::sync::{Arc, Mutex};

    use nomi_providers::{LlmProvider, ProviderError};
    use nomi_tools::registry::ToolRegistry;
    use nomi_types::llm::{LlmEvent, LlmRequest};
    use nomi_types::message::{ContentBlock, Role};

    use crate::confirm::ToolConfirmer;
    use crate::output::OutputSink;

    struct NullOutput;
    impl OutputSink for NullOutput {
        fn emit_text_delta(&self, _: &str, _: &str) {}
        fn emit_thinking(&self, _: &str, _: &str) {}
        fn emit_tool_call(&self, _: &str, _: &str, _: &str) {}
        fn emit_tool_result(&self, _: &str, _: &str, _: bool, _: &str) {}
        fn emit_stream_start(&self, _: &str) {}
        fn emit_stream_end(&self, _: &str, _: usize, _: u64, _: u64, _: u64, _: u64) {}
        fn emit_error(&self, _: &str) {}
        fn emit_info(&self, _: &str) {}
    }

    struct NullProvider;
    #[async_trait::async_trait]
    impl LlmProvider for NullProvider {
        async fn stream(
            &self,
            _: &LlmRequest,
        ) -> Result<tokio::sync::mpsc::Receiver<LlmEvent>, ProviderError> {
            let (_tx, rx) = tokio::sync::mpsc::channel(1);
            Ok(rx)
        }
    }

    /// Emits one tool call every turn forever — used to verify the runaway-loop
    /// safety net. With `max_turns: None` the engine must still terminate.
    struct LoopProvider;
    #[async_trait::async_trait]
    impl LlmProvider for LoopProvider {
        async fn stream(
            &self,
            _: &LlmRequest,
        ) -> Result<tokio::sync::mpsc::Receiver<LlmEvent>, ProviderError> {
            let (tx, rx) = tokio::sync::mpsc::channel(2);
            let _ = tx
                .send(LlmEvent::ToolUse {
                    id: "loop".to_string(),
                    name: "noop".to_string(),
                    input: serde_json::json!({}),
                    extra: None,
                })
                .await;
            let _ = tx
                .send(LlmEvent::Done {
                    stop_reason: nomi_types::message::StopReason::ToolUse,
                    usage: Default::default(),
                })
                .await;
            Ok(rx)
        }
    }

    /// Never delivers an event within a test window — the engine must abandon
    /// the in-flight stream when its cancellation token fires.
    struct SlowProvider;
    #[async_trait::async_trait]
    impl LlmProvider for SlowProvider {
        async fn stream(
            &self,
            _: &LlmRequest,
        ) -> Result<tokio::sync::mpsc::Receiver<LlmEvent>, ProviderError> {
            let (tx, rx) = tokio::sync::mpsc::channel(2);
            tokio::spawn(async move {
                tokio::time::sleep(std::time::Duration::from_secs(30)).await;
                let _ = tx
                    .send(LlmEvent::Done {
                        stop_reason: nomi_types::message::StopReason::EndTurn,
                        usage: Default::default(),
                    })
                    .await;
            });
            Ok(rx)
        }
    }

    /// Turn 1 issues a single tool call (then ToolUse stop); turn 2 ends the
    /// turn (EndTurn stop, no tools). Used to verify steering injection point A
    /// rides along the tool-result message.
    struct ToolThenStopProvider {
        calls: std::sync::atomic::AtomicUsize,
    }
    #[async_trait::async_trait]
    impl LlmProvider for ToolThenStopProvider {
        async fn stream(
            &self,
            _: &LlmRequest,
        ) -> Result<tokio::sync::mpsc::Receiver<LlmEvent>, ProviderError> {
            let n = self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let (tx, rx) = tokio::sync::mpsc::channel(4);
            if n == 0 {
                let _ = tx
                    .send(LlmEvent::ToolUse {
                        id: "t1".to_string(),
                        name: "noop".to_string(), // unknown tool → error tool-result; loop continues
                        input: serde_json::json!({}),
                        extra: None,
                    })
                    .await;
                let _ = tx
                    .send(LlmEvent::Done {
                        stop_reason: nomi_types::message::StopReason::ToolUse,
                        usage: Default::default(),
                    })
                    .await;
            } else {
                let _ = tx
                    .send(LlmEvent::Done {
                        stop_reason: nomi_types::message::StopReason::EndTurn,
                        usage: Default::default(),
                    })
                    .await;
            }
            Ok(rx)
        }
    }

    fn make_engine(model: &str) -> super::AgentEngine {
        super::AgentEngine {
            provider: Arc::new(NullProvider),
            tools: ToolRegistry::new(),
            messages: vec![],
            system_prompt: String::new(),
            model: model.to_string(),
            max_tokens: 4096,
            max_turns: Some(10),
            total_usage: Default::default(),
            thinking: None,
            compat: nomi_config::compat::ProviderCompat::anthropic_defaults(),
            confirmer: Arc::new(Mutex::new(ToolConfirmer::new(true, vec![]))),
            hooks: None,
            session_manager: None,
            current_session: None,
            output: Arc::new(NullOutput),
            current_msg_id: String::new(),
            approval_manager: None,
            protocol_writer: None,
            allow_list: vec![],
            current_reasoning_effort: None,
            compact_config: nomi_config::compact::CompactConfig::default(),
            compact_state: super::CompactState::new(),
            plan_state: Default::default(),
            plan_active_flag: None,
            cache_detector: super::CacheBreakDetector::new(),
            compaction_level: nomi_compact::CompactionLevel::default(),
            toon_enabled: false,
            max_recent_images: 3,
            commands: crate::commands::default_registry(),
            goal: None,
            cancel_token: None,
            stagnation_guard: crate::loop_guard::StagnationGuard::new(crate::engine::STAGNATION_THRESHOLD),
            context_contributors: Vec::new(),
            steering_inbox: None,
            process_supervisor: None,
            last_turn_start_len: None,
        }
    }

    #[test]
    fn context_accessors_report_window_and_last_input() {
        let mut engine = make_engine("ctx-accessors");
        assert_eq!(engine.context_window(), engine.compact_config.context_window as u64);
        engine.compact_state.last_input_tokens = 12_345;
        assert_eq!(engine.context_tokens(), 12_345);
    }

    #[test]
    fn rewind_last_turn_truncates_to_marker() {
        use nomi_types::message::{ContentBlock, Message, Role};
        let mut engine = make_engine("rewind");
        // 既有历史：U0, A0
        engine.messages.push(Message::now(Role::User, vec![ContentBlock::Text { text: "u0".into() }]));
        engine.messages.push(Message::now(Role::Assistant, vec![ContentBlock::Text { text: "a0".into() }]));
        // 标记最后一个 turn 起始 = 当前长度(2)，再 push U1（被中断的 turn）
        engine.last_turn_start_len = Some(engine.messages.len());
        engine.messages.push(Message::now(Role::User, vec![ContentBlock::Text { text: "u1".into() }]));
        assert_eq!(engine.messages.len(), 3);

        assert!(engine.rewind_last_turn());
        assert_eq!(engine.messages.len(), 2); // U1 被回退
        assert!(engine.last_turn_start_len.is_none()); // 锚点被消费

        // 再次回退无锚点 → false
        assert!(!engine.rewind_last_turn());
    }

    #[test]
    fn rewind_last_turn_rejects_stale_marker() {
        let mut engine = make_engine("rewind-stale");
        // 锚点越界（如压缩后未清理的极端情况）→ 拒绝
        engine.last_turn_start_len = Some(5);
        assert!(!engine.rewind_last_turn());
    }

    fn make_engine_with_compat(
        model: &str,
        compat: nomi_config::compat::ProviderCompat,
    ) -> super::AgentEngine {
        let mut engine = make_engine(model);
        engine.compat = compat;
        engine
    }

    #[tokio::test]
    async fn safety_net_caps_unbounded_loop() {
        // A model stuck in a tool-call loop with no configured max_turns must
        // still terminate at the hard safety net (200), not run forever.
        let mut engine = make_engine("safety-net-model");
        engine.max_turns = None;
        engine.provider = Arc::new(LoopProvider);

        let res = tokio::time::timeout(
            std::time::Duration::from_secs(8),
            engine.run("go", "msg-safety"),
        )
        .await
        .expect("engine.run must terminate via the safety net, not hang forever");

        let result = res.expect("engine.run returned Ok");
        assert_eq!(result.stop_reason, nomi_types::message::StopReason::MaxTurns);
        assert_eq!(result.turns, 200);
    }

    #[tokio::test]
    async fn cooperative_cancel_abandons_inflight_stream() {
        // With a cancellation token set, the engine must abandon a stream that is
        // blocked waiting for the first event and return cleanly — without
        // pushing this turn's assistant message, so self.messages stays
        // consistent (no dangling tool_use). (Phase 0 F0.4)
        let mut engine = make_engine("coop-model");
        engine.provider = Arc::new(SlowProvider);
        let token = tokio_util::sync::CancellationToken::new();
        engine.set_cancel_token(Some(token.clone()));

        let t2 = token.clone();
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            t2.cancel();
        });

        let res = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            engine.run("go", "m-coop"),
        )
        .await
        .expect("cooperative cancel must abandon the in-flight stream, not block on it");

        let result = res.expect("engine.run returned Ok");
        assert_eq!(result.turns, 1, "cancelled during the first turn");
        assert!(token.is_cancelled());
        // Only the original user message is present — no half-built assistant turn.
        assert_eq!(engine.messages.len(), 1);
    }

    #[tokio::test]
    async fn steering_extends_a_would_end_turn() {
        // NullProvider makes every turn a no-tool turn that would END. A steer
        // message present at turn-end must extend the turn by one (point B),
        // appended as a fresh User message (assistant→user ordering is valid).
        let mut engine = make_engine("steer-b");
        let inbox = std::sync::Arc::new(std::sync::Mutex::new(
            std::collections::VecDeque::from(["please also do X".to_string()]),
        ));
        engine.set_steering_inbox(Some(inbox.clone()));

        let res = engine.run("go", "m-b").await.expect("engine.run ok");

        assert_eq!(res.turns, 2, "the steer message extends the turn by one");
        // [User "go", Assistant[], User "please also do X", Assistant[]]
        assert_eq!(engine.messages.len(), 4);
        let injected = &engine.messages[2];
        assert_eq!(injected.role, Role::User);
        assert!(
            injected
                .content
                .iter()
                .any(|b| matches!(b, ContentBlock::Text { text } if text == "please also do X")),
            "injected user message must carry the steer text"
        );
        assert!(inbox.lock().unwrap().is_empty(), "inbox drained");
    }

    #[tokio::test]
    async fn steering_rides_along_tool_result_message() {
        // Turn 1 issues a tool call; the steer message must be appended as a
        // trailing Text block ON the tool-result User message (point A) — never
        // as a second consecutive User message.
        let mut engine = make_engine("steer-a");
        engine.provider = std::sync::Arc::new(ToolThenStopProvider {
            calls: std::sync::atomic::AtomicUsize::new(0),
        });
        let inbox = std::sync::Arc::new(std::sync::Mutex::new(
            std::collections::VecDeque::from(["wait, focus on Y".to_string()]),
        ));
        engine.set_steering_inbox(Some(inbox.clone()));

        let res = engine.run("go", "m-a").await.expect("engine.run ok");

        assert_eq!(res.turns, 2);
        // messages: [User "go", Assistant[ToolUse], User[ToolResult, Text "wait, focus on Y"], Assistant[]]
        let tool_result_msg = &engine.messages[2];
        assert_eq!(tool_result_msg.role, Role::User);
        assert!(
            tool_result_msg
                .content
                .iter()
                .any(|b| matches!(b, ContentBlock::Text { text } if text == "wait, focus on Y")),
            "steer text must ride along the tool-result message"
        );
        for w in engine.messages.windows(2) {
            assert!(
                !(w[0].role == Role::User && w[1].role == Role::User),
                "must not create consecutive user messages"
            );
        }
        assert!(inbox.lock().unwrap().is_empty());
    }

    #[test]
    fn set_config_changes_model() {
        let mut engine = make_engine("old-model");
        let changes = engine.apply_config_update(Some("new-model".into()), None, None, None, None);
        assert_eq!(engine.model, "new-model");
        assert_eq!(changes.len(), 1);
        assert!(changes[0].contains("old-model"));
        assert!(changes[0].contains("new-model"));
    }

    #[test]
    fn set_config_none_model_no_change() {
        let mut engine = make_engine("current");
        let changes = engine.apply_config_update(None, None, None, None, None);
        assert_eq!(engine.model, "current");
        assert!(changes.is_empty());
    }

    #[test]
    fn set_config_same_model_still_reports_change() {
        let mut engine = make_engine("same");
        let changes = engine.apply_config_update(Some("same".into()), None, None, None, None);
        assert_eq!(changes.len(), 1);
    }

    #[test]
    fn set_config_empty_string_model_accepted() {
        let mut engine = make_engine("real-model");
        engine.apply_config_update(Some(String::new()), None, None, None, None);
        assert_eq!(engine.model, "");
    }

    #[test]
    fn set_config_model_does_not_affect_other_state() {
        let mut engine = make_engine("m");
        engine.current_reasoning_effort = Some("high".into());
        engine.apply_config_update(Some("new-m".into()), None, None, None, None);
        assert_eq!(engine.model, "new-m");
        assert_eq!(engine.current_reasoning_effort.as_deref(), Some("high"));
    }

    // --- Cycle 2: Effort config tests ---

    #[test]
    fn set_config_changes_effort() {
        let mut engine =
            make_engine_with_compat("m", nomi_config::compat::ProviderCompat::openai_defaults());
        assert!(engine.current_reasoning_effort.is_none());
        let changes = engine.apply_config_update(None, None, None, Some("high".into()), None);
        assert_eq!(engine.current_reasoning_effort.as_deref(), Some("high"));
        assert_eq!(changes.len(), 1);
        assert!(changes[0].contains("high"));
    }

    #[test]
    fn set_config_clears_effort_with_empty_string() {
        let mut engine = make_engine("m");
        engine.current_reasoning_effort = Some("high".into());
        let changes = engine.apply_config_update(None, None, None, Some(String::new()), None);
        assert!(engine.current_reasoning_effort.is_none());
        assert_eq!(changes.len(), 1);
    }

    // --- Cycle 2: Thinking config tests ---

    #[test]
    fn set_config_enables_thinking() {
        let mut engine = make_engine("m");
        let changes =
            engine.apply_config_update(None, Some("enabled".into()), Some(16000), None, None);
        match &engine.thinking {
            Some(nomi_types::llm::ThinkingConfig::Enabled { budget_tokens }) => {
                assert_eq!(*budget_tokens, 16000);
            }
            other => panic!("expected Enabled, got: {other:?}"),
        }
        assert_eq!(changes.len(), 1);
    }

    #[test]
    fn set_config_disables_thinking() {
        let mut engine = make_engine("m");
        engine.thinking = Some(nomi_types::llm::ThinkingConfig::Enabled {
            budget_tokens: 8000,
        });
        let changes = engine.apply_config_update(None, Some("disabled".into()), None, None, None);
        match &engine.thinking {
            Some(nomi_types::llm::ThinkingConfig::Disabled) => {}
            other => panic!("expected Disabled, got: {other:?}"),
        }
        assert_eq!(changes.len(), 1);
    }

    #[test]
    fn set_config_thinking_enabled_default_budget() {
        let mut engine = make_engine("m");
        let changes = engine.apply_config_update(None, Some("enabled".into()), None, None, None);
        match &engine.thinking {
            Some(nomi_types::llm::ThinkingConfig::Enabled { budget_tokens }) => {
                assert!(*budget_tokens > 0);
            }
            other => panic!("expected Enabled with default budget, got: {other:?}"),
        }
        assert_eq!(changes.len(), 1);
    }

    #[test]
    fn set_config_invalid_thinking_ignored() {
        let mut engine = make_engine("m");
        engine.thinking = Some(nomi_types::llm::ThinkingConfig::Enabled {
            budget_tokens: 8000,
        });
        let changes =
            engine.apply_config_update(None, Some("invalid_value".into()), None, None, None);
        match &engine.thinking {
            Some(nomi_types::llm::ThinkingConfig::Enabled { budget_tokens }) => {
                assert_eq!(*budget_tokens, 8000);
            }
            other => panic!("expected Enabled unchanged, got: {other:?}"),
        }
        assert_eq!(changes.len(), 1);
        assert!(changes[0].contains("invalid") || changes[0].contains("ignored"));
    }

    // --- Cycle 2: Combined fields test ---

    #[test]
    fn set_config_all_fields_at_once() {
        let compat = nomi_config::compat::ProviderCompat {
            supports_thinking: Some(true),
            supports_effort: Some(true),
            effort_levels: Some(vec!["low".into()]),
            ..Default::default()
        };
        let mut engine = make_engine_with_compat("old-model", compat);
        let changes = engine.apply_config_update(
            Some("new-model".into()),
            Some("enabled".into()),
            Some(12000),
            Some("low".into()),
            None,
        );
        assert_eq!(engine.model, "new-model");
        assert_eq!(engine.current_reasoning_effort.as_deref(), Some("low"));
        match &engine.thinking {
            Some(nomi_types::llm::ThinkingConfig::Enabled { budget_tokens }) => {
                assert_eq!(*budget_tokens, 12000);
            }
            other => panic!("expected Enabled, got: {other:?}"),
        }
        assert_eq!(changes.len(), 3);
    }

    // --- Cycle 2: White-box edge case tests ---

    #[test]
    fn set_config_thinking_budget_only_updates_existing_enabled() {
        let mut engine = make_engine("m");
        engine.thinking = Some(nomi_types::llm::ThinkingConfig::Enabled {
            budget_tokens: 5000,
        });
        let changes = engine.apply_config_update(None, None, Some(20000), None, None);
        match &engine.thinking {
            Some(nomi_types::llm::ThinkingConfig::Enabled { budget_tokens }) => {
                assert_eq!(*budget_tokens, 20000);
            }
            other => panic!("expected Enabled with 20000, got: {other:?}"),
        }
        assert_eq!(changes.len(), 1);
    }

    #[test]
    fn set_config_thinking_budget_ignored_when_disabled() {
        let mut engine = make_engine("m");
        engine.thinking = Some(nomi_types::llm::ThinkingConfig::Disabled);
        let changes = engine.apply_config_update(None, None, Some(20000), None, None);
        match &engine.thinking {
            Some(nomi_types::llm::ThinkingConfig::Disabled) => {}
            other => panic!("expected Disabled unchanged, got: {other:?}"),
        }
        assert!(changes.is_empty());
    }

    #[test]
    fn set_config_effort_valid_values() {
        let compat = nomi_config::compat::ProviderCompat {
            supports_effort: Some(true),
            effort_levels: Some(vec![
                "low".into(),
                "medium".into(),
                "high".into(),
                "max".into(),
            ]),
            ..Default::default()
        };
        for value in ["low", "medium", "high", "max"] {
            let mut engine = make_engine_with_compat("m", compat.clone());
            engine.apply_config_update(None, None, None, Some(value.to_string()), None);
            assert_eq!(
                engine.current_reasoning_effort.as_deref(),
                Some(value),
                "effort should be set to {value}"
            );
        }
    }

    // --- Capability validation tests ---

    #[test]
    fn set_config_thinking_rejected_when_unsupported() {
        let mut engine =
            make_engine_with_compat("m", nomi_config::compat::ProviderCompat::openai_defaults());
        let changes = engine.apply_config_update(None, Some("enabled".into()), None, None, None);
        assert!(changes.iter().any(|c| c.contains("not supported")));
        assert!(engine.thinking.is_none());
    }

    #[test]
    fn set_config_effort_rejected_when_unsupported() {
        let mut engine = make_engine("m"); // anthropic defaults: supports_effort = false
        let changes = engine.apply_config_update(None, None, None, Some("high".into()), None);
        assert!(changes.iter().any(|c| c.contains("not supported")));
        assert!(engine.current_reasoning_effort.is_none());
    }

    #[test]
    fn set_config_effort_rejected_invalid_level() {
        let mut engine =
            make_engine_with_compat("m", nomi_config::compat::ProviderCompat::openai_defaults());
        let changes = engine.apply_config_update(None, None, None, Some("max".into()), None);
        assert!(changes.iter().any(|c| c.contains("invalid")));
        assert!(engine.current_reasoning_effort.is_none());
    }

    #[test]
    fn set_config_effort_clear_always_works() {
        let mut engine = make_engine("m"); // anthropic defaults: supports_effort = false
        engine.current_reasoning_effort = Some("high".into());
        let changes = engine.apply_config_update(None, None, None, Some(String::new()), None);
        assert!(engine.current_reasoning_effort.is_none());
        assert!(changes.iter().any(|c| c.contains("cleared")));
    }
}

// ---------------------------------------------------------------------------
// Phase 6 tests — apply_context_modifiers()
// ---------------------------------------------------------------------------

#[cfg(test)]
mod phase6_tests {
    use std::sync::{Arc, Mutex};

    use nomi_providers::{LlmProvider, ProviderError};
    use nomi_tools::registry::ToolRegistry;
    use nomi_types::llm::{LlmEvent, LlmRequest};
    use nomi_types::skill_types::{ContextModifier, EffortLevel};

    use crate::confirm::ToolConfirmer;
    use crate::output::OutputSink;

    struct NullOutput;
    impl OutputSink for NullOutput {
        fn emit_text_delta(&self, _: &str, _: &str) {}
        fn emit_thinking(&self, _: &str, _: &str) {}
        fn emit_tool_call(&self, _: &str, _: &str, _: &str) {}
        fn emit_tool_result(&self, _: &str, _: &str, _: bool, _: &str) {}
        fn emit_stream_start(&self, _: &str) {}
        fn emit_stream_end(&self, _: &str, _: usize, _: u64, _: u64, _: u64, _: u64) {}
        fn emit_error(&self, _: &str) {}
        fn emit_info(&self, _: &str) {}
    }

    struct NullProvider;
    #[async_trait::async_trait]
    impl LlmProvider for NullProvider {
        async fn stream(
            &self,
            _: &LlmRequest,
        ) -> Result<tokio::sync::mpsc::Receiver<LlmEvent>, ProviderError> {
            let (_tx, rx) = tokio::sync::mpsc::channel(1);
            Ok(rx)
        }
    }

    fn make_engine(model: &str, allow_list: Vec<String>) -> super::AgentEngine {
        super::AgentEngine {
            provider: Arc::new(NullProvider),
            tools: ToolRegistry::new(),
            messages: vec![],
            system_prompt: String::new(),
            model: model.to_string(),
            max_tokens: 4096,
            max_turns: Some(10),
            total_usage: Default::default(),
            thinking: None,
            compat: nomi_config::compat::ProviderCompat::anthropic_defaults(),
            confirmer: Arc::new(Mutex::new(ToolConfirmer::new(true, allow_list.clone()))),
            hooks: None,
            session_manager: None,
            current_session: None,
            output: Arc::new(NullOutput),
            current_msg_id: String::new(),
            approval_manager: None,
            protocol_writer: None,
            allow_list,
            current_reasoning_effort: None,
            compact_config: nomi_config::compact::CompactConfig::default(),
            compact_state: super::CompactState::new(),
            plan_state: Default::default(),
            plan_active_flag: None,
            cache_detector: super::CacheBreakDetector::new(),
            compaction_level: nomi_compact::CompactionLevel::default(),
            toon_enabled: false,
            max_recent_images: 3,
            commands: crate::commands::default_registry(),
            goal: None,
            cancel_token: None,
            stagnation_guard: crate::loop_guard::StagnationGuard::new(crate::engine::STAGNATION_THRESHOLD),
            context_contributors: Vec::new(),
            steering_inbox: None,
            process_supervisor: None,
            last_turn_start_len: None,
        }
    }

    #[test]
    fn tc_6_21_model_override_applied() {
        let mut engine = make_engine("original-model", vec![]);
        let modifiers = vec![Some(ContextModifier {
            model: Some("override-model".to_string()),
            ..Default::default()
        })];
        engine.apply_context_modifiers(&modifiers);
        assert_eq!(engine.model, "override-model");
    }

    #[test]
    fn tc_6_22_effort_override_applied() {
        let mut engine = make_engine("m", vec![]);
        let modifiers = vec![Some(ContextModifier {
            effort: Some(EffortLevel::High),
            ..Default::default()
        })];
        engine.apply_context_modifiers(&modifiers);
        assert_eq!(engine.current_reasoning_effort.as_deref(), Some("high"));
    }

    #[test]
    fn tc_6_22b_effort_all_variants() {
        for (level, expected) in [
            (EffortLevel::Low, "low"),
            (EffortLevel::Medium, "medium"),
            (EffortLevel::High, "high"),
            (EffortLevel::Max, "max"),
        ] {
            let mut engine = make_engine("m", vec![]);
            engine.apply_context_modifiers(&[Some(ContextModifier {
                effort: Some(level),
                ..Default::default()
            })]);
            assert_eq!(
                engine.current_reasoning_effort.as_deref(),
                Some(expected),
                "EffortLevel::{level:?} should map to {expected:?}"
            );
        }
    }

    #[test]
    fn tc_6_23_allowed_tools_no_duplicates() {
        let mut engine = make_engine("m", vec!["Bash".to_string()]);
        let modifiers = vec![Some(ContextModifier {
            allowed_tools: vec!["Bash".to_string(), "Read".to_string()],
            ..Default::default()
        })];
        engine.apply_context_modifiers(&modifiers);
        let bash_count = engine
            .allow_list
            .iter()
            .filter(|t| t.as_str() == "Bash")
            .count();
        assert_eq!(bash_count, 1, "Bash should appear exactly once");
        assert!(engine.allow_list.contains(&"Read".to_string()));
    }

    #[test]
    fn tc_6_24_none_modifiers_skipped() {
        let mut engine = make_engine("original", vec![]);
        engine.apply_context_modifiers(&[None, None]);
        assert_eq!(engine.model, "original");
        assert!(engine.current_reasoning_effort.is_none());
    }

    #[test]
    fn tc_6_25_empty_modifiers_no_change() {
        let mut engine = make_engine("current-model", vec![]);
        engine.apply_context_modifiers(&[]);
        assert_eq!(engine.model, "current-model");
        assert!(engine.allow_list.is_empty());
    }

    #[test]
    fn tc_6_26_none_model_does_not_overwrite() {
        let mut engine = make_engine("current-model", vec![]);
        engine.apply_context_modifiers(&[Some(ContextModifier {
            allowed_tools: vec!["Bash".to_string()],
            ..Default::default()
        })]);
        assert_eq!(engine.model, "current-model");
        assert!(engine.allow_list.contains(&"Bash".to_string()));
    }

    #[test]
    fn tc_6_27_multiple_modifiers_stacked() {
        let mut engine = make_engine("initial", vec![]);
        let modifiers = vec![
            Some(ContextModifier {
                model: Some("model-a".to_string()),
                allowed_tools: vec!["Bash".to_string()],
                ..Default::default()
            }),
            Some(ContextModifier {
                model: Some("model-b".to_string()),
                allowed_tools: vec!["Read".to_string()],
                ..Default::default()
            }),
        ];
        engine.apply_context_modifiers(&modifiers);
        assert_eq!(engine.model, "model-b", "last model wins");
        assert!(engine.allow_list.contains(&"Bash".to_string()));
        assert!(engine.allow_list.contains(&"Read".to_string()));
    }

    #[test]
    fn tc_6_28_modifier_applied_after_tool_execution_not_during() {
        let mut engine = make_engine("original", vec![]);
        let model_before = engine.model.clone();
        let modifiers = vec![Some(ContextModifier {
            model: Some("new-model".to_string()),
            ..Default::default()
        })];
        assert_eq!(engine.model, model_before);
        engine.apply_context_modifiers(&modifiers);
        assert_eq!(engine.model, "new-model");
        assert_eq!(model_before, "original");
    }
}

// ---------------------------------------------------------------------------
// Phase 2 tests — run_compaction()
// ---------------------------------------------------------------------------

#[cfg(test)]
mod compact_tests {
    use super::MAX_PROVIDER_REQUEST_IMAGES;
    use std::sync::{Arc, Mutex};

    use nomi_config::compact::CompactConfig;
    use nomi_providers::{LlmProvider, ProviderError};
    use nomi_tools::registry::ToolRegistry;
    use nomi_types::llm::{LlmEvent, LlmRequest};
    use nomi_types::message::{ContentBlock, Message, Role, StopReason};
    use serde_json::json;

    use crate::compact::state::CompactState;
    use crate::confirm::ToolConfirmer;
    use crate::output::OutputSink;

    struct NullOutput;
    impl OutputSink for NullOutput {
        fn emit_text_delta(&self, _: &str, _: &str) {}
        fn emit_thinking(&self, _: &str, _: &str) {}
        fn emit_tool_call(&self, _: &str, _: &str, _: &str) {}
        fn emit_tool_result(&self, _: &str, _: &str, _: bool, _: &str) {}
        fn emit_stream_start(&self, _: &str) {}
        fn emit_stream_end(&self, _: &str, _: usize, _: u64, _: u64, _: u64, _: u64) {}
        fn emit_error(&self, _: &str) {}
        fn emit_info(&self, _: &str) {}
    }

    #[derive(Default)]
    struct RecordingOutput {
        tool_results: Mutex<Vec<(String, String, bool, String)>>,
    }

    impl OutputSink for RecordingOutput {
        fn emit_text_delta(&self, _: &str, _: &str) {}
        fn emit_thinking(&self, _: &str, _: &str) {}
        fn emit_tool_call(&self, _: &str, _: &str, _: &str) {}
        fn emit_tool_result(&self, tool_use_id: &str, name: &str, is_error: bool, content: &str) {
            self.tool_results.lock().unwrap().push((
                tool_use_id.to_string(),
                name.to_string(),
                is_error,
                content.to_string(),
            ));
        }
        fn emit_stream_start(&self, _: &str) {}
        fn emit_stream_end(&self, _: &str, _: usize, _: u64, _: u64, _: u64, _: u64) {}
        fn emit_error(&self, _: &str) {}
        fn emit_info(&self, _: &str) {}
    }

    struct NullProvider;
    #[async_trait::async_trait]
    impl LlmProvider for NullProvider {
        async fn stream(
            &self,
            _: &LlmRequest,
        ) -> Result<tokio::sync::mpsc::Receiver<LlmEvent>, ProviderError> {
            let (_tx, rx) = tokio::sync::mpsc::channel(1);
            Ok(rx)
        }
    }

    #[derive(Default)]
    struct RecordingProvider {
        request_image_counts: Mutex<Vec<usize>>,
    }

    #[async_trait::async_trait]
    impl LlmProvider for RecordingProvider {
        async fn stream(
            &self,
            request: &LlmRequest,
        ) -> Result<tokio::sync::mpsc::Receiver<LlmEvent>, ProviderError> {
            self.request_image_counts
                .lock()
                .unwrap()
                .push(count_images(&request.messages));
            let (tx, rx) = tokio::sync::mpsc::channel(1);
            tx.try_send(LlmEvent::Done {
                stop_reason: StopReason::EndTurn,
                usage: Default::default(),
            })
            .unwrap();
            Ok(rx)
        }
    }

    fn make_compact_engine(
        compact_config: CompactConfig,
        compact_state: CompactState,
        messages: Vec<Message>,
    ) -> super::AgentEngine {
        make_compact_engine_with_output(
            compact_config,
            compact_state,
            messages,
            Arc::new(NullOutput),
        )
    }

    fn make_compact_engine_with_output(
        compact_config: CompactConfig,
        compact_state: CompactState,
        messages: Vec<Message>,
        output: Arc<dyn OutputSink>,
    ) -> super::AgentEngine {
        super::AgentEngine {
            provider: Arc::new(NullProvider),
            tools: ToolRegistry::new(),
            messages,
            system_prompt: String::new(),
            model: "test-model".to_string(),
            max_tokens: 4096,
            max_turns: Some(10),
            total_usage: Default::default(),
            thinking: None,
            compat: nomi_config::compat::ProviderCompat::anthropic_defaults(),
            confirmer: Arc::new(Mutex::new(ToolConfirmer::new(true, vec![]))),
            hooks: None,
            session_manager: None,
            current_session: None,
            output,
            current_msg_id: String::new(),
            approval_manager: None,
            protocol_writer: None,
            allow_list: vec![],
            current_reasoning_effort: None,
            compact_config,
            compact_state,
            plan_state: Default::default(),
            plan_active_flag: None,
            cache_detector: super::CacheBreakDetector::new(),
            compaction_level: nomi_compact::CompactionLevel::default(),
            toon_enabled: false,
            max_recent_images: 3,
            commands: crate::commands::default_registry(),
            goal: None,
            cancel_token: None,
            stagnation_guard: crate::loop_guard::StagnationGuard::new(crate::engine::STAGNATION_THRESHOLD),
            context_contributors: Vec::new(),
            steering_inbox: None,
            process_supervisor: None,
            last_turn_start_len: None,
        }
    }

    fn tool_use_msg(id: &str, name: &str) -> Message {
        Message::new(
            Role::Assistant,
            vec![ContentBlock::ToolUse {
                id: id.to_string(),
                name: name.to_string(),
                input: json!({}),
                extra: None,
            }],
        )
    }

    fn tool_use_msg_with_two_calls(first_id: &str, second_id: &str) -> Message {
        Message::new(
            Role::Assistant,
            vec![
                ContentBlock::ToolUse {
                    id: first_id.to_string(),
                    name: "Read".to_string(),
                    input: json!({}),
                    extra: None,
                },
                ContentBlock::ToolUse {
                    id: second_id.to_string(),
                    name: "Bash".to_string(),
                    input: json!({}),
                    extra: None,
                },
            ],
        )
    }

    fn tool_result_msg(id: &str, content: &str) -> Message {
        Message::new(
            Role::User,
            vec![ContentBlock::ToolResult {
                tool_use_id: id.to_string(),
                content: content.to_string(),
                is_error: false,
                images: Vec::new(),
            }],
        )
    }

    fn tool_result_msg_with_image(id: &str) -> Message {
        Message::new(
            Role::User,
            vec![ContentBlock::ToolResult {
                tool_use_id: id.to_string(),
                content: "screenshot".to_string(),
                is_error: false,
                images: vec![nomi_types::tool::ToolImage {
                    media_type: "image/png".to_string(),
                    data: "aGk=".to_string(),
                }],
            }],
        )
    }

    fn count_images(messages: &[Message]) -> usize {
        messages
            .iter()
            .flat_map(|m| &m.content)
            .map(|b| match b {
                ContentBlock::ToolResult { images, .. } => images.len(),
                _ => 0,
            })
            .sum()
    }

    #[test]
    fn prune_old_tool_images_keeps_most_recent() {
        let mut engine = make_compact_engine(
            CompactConfig::default(),
            CompactState::new(),
            (0..5).map(|i| tool_result_msg_with_image(&format!("call_{i}"))).collect(),
        );
        engine.max_recent_images = 3;
        engine.prune_old_tool_images();
        assert_eq!(count_images(&engine.messages), 3);
        // The two oldest lost their images; text content survives.
        for (i, msg) in engine.messages.iter().enumerate() {
            if let ContentBlock::ToolResult { images, content, .. } = &msg.content[0] {
                assert_eq!(images.is_empty(), i < 2, "msg {i}");
                assert!(content.starts_with("screenshot"));
                assert_eq!(content.contains("attachment(s)"), i < 2);
            }
        }
    }

    #[test]
    fn prune_old_tool_images_noop_under_limit() {
        let mut engine = make_compact_engine(
            CompactConfig::default(),
            CompactState::new(),
            vec![tool_result_msg_with_image("call_0")],
        );
        engine.max_recent_images = 3;
        engine.prune_old_tool_images();
        assert_eq!(count_images(&engine.messages), 1);
    }

    #[test]
    fn prune_old_tool_images_counts_images_inside_one_result() {
        let mut message = tool_result_msg_with_image("batch");
        let ContentBlock::ToolResult { images, .. } = &mut message.content[0] else {
            unreachable!();
        };
        let image = images[0].clone();
        images.resize(25, image);
        let mut engine = make_compact_engine(
            CompactConfig::default(),
            CompactState::new(),
            vec![message],
        );
        engine.max_recent_images = 100;

        engine.prune_old_tool_images();

        assert_eq!(count_images(&engine.messages), MAX_PROVIDER_REQUEST_IMAGES);
        let ContentBlock::ToolResult { content, .. } = &engine.messages[0].content[0] else {
            unreachable!();
        };
        assert!(content.contains("5 later attachment(s) were omitted"));
    }

    #[tokio::test]
    async fn first_request_prunes_images_from_preloaded_or_resumed_history() {
        let mut message = tool_result_msg_with_image("legacy-batch");
        let ContentBlock::ToolResult { images, .. } = &mut message.content[0] else {
            unreachable!();
        };
        let image = images[0].clone();
        images.resize(25, image);

        let provider = Arc::new(RecordingProvider::default());
        let mut engine = make_compact_engine(
            CompactConfig::default(),
            CompactState::new(),
            vec![message],
        );
        engine.provider = provider.clone();
        engine.max_recent_images = 100;

        engine.run("continue", "resume-image-limit").await.unwrap();

        assert_eq!(
            *provider.request_image_counts.lock().unwrap(),
            vec![MAX_PROVIDER_REQUEST_IMAGES]
        );
        assert_eq!(count_images(&engine.messages), MAX_PROVIDER_REQUEST_IMAGES);
    }

    #[test]
    fn abort_current_turn_closes_pending_tool_uses() {
        let output = Arc::new(RecordingOutput::default());
        let mut engine = make_compact_engine_with_output(
            CompactConfig::default(),
            CompactState::new(),
            vec![
                Message::new(
                    Role::User,
                    vec![ContentBlock::Text {
                        text: "run tools".to_string(),
                    }],
                ),
                tool_use_msg_with_two_calls("call_read", "call_bash"),
            ],
            output.clone(),
        );

        engine.abort_current_turn("Tool execution canceled by user");

        let last = engine.messages.last().expect("synthetic result message");
        assert_eq!(last.role, Role::User);
        assert_eq!(last.content.len(), 2);
        assert!(
            matches!(&last.content[0], ContentBlock::ToolResult { tool_use_id, content, is_error, .. }
                if tool_use_id == "call_read" && content == "Tool execution canceled by user" && *is_error)
        );
        assert!(
            matches!(&last.content[1], ContentBlock::ToolResult { tool_use_id, content, is_error, .. }
                if tool_use_id == "call_bash" && content == "Tool execution canceled by user" && *is_error)
        );

        let emitted = output.tool_results.lock().unwrap();
        assert_eq!(emitted.len(), 2);
        assert_eq!(
            emitted[0],
            (
                "call_read".into(),
                "Read".into(),
                true,
                "Tool execution canceled by user".into()
            )
        );
        assert_eq!(
            emitted[1],
            (
                "call_bash".into(),
                "Bash".into(),
                true,
                "Tool execution canceled by user".into()
            )
        );
    }

    // -- Emergency check fires when at limit --

    #[tokio::test]
    async fn emergency_fires_when_at_limit() {
        let config = CompactConfig {
            context_window: 200_000,
            emergency_buffer: 3_000,
            ..Default::default()
        };
        let mut state = CompactState::new();
        state.last_input_tokens = 198_000; // >= 197k limit

        let mut engine = make_compact_engine(config, state, vec![]);
        let result = engine.run_compaction().await;

        match result {
            Err(super::AgentError::ContextTooLong {
                input_tokens,
                limit,
            }) => {
                assert_eq!(input_tokens, 198_000);
                assert_eq!(limit, 197_000);
            }
            other => panic!("expected ContextTooLong, got: {:?}", other),
        }
    }

    // -- Emergency does not fire when below limit --

    #[tokio::test]
    async fn emergency_silent_below_limit() {
        let config = CompactConfig::default();
        let mut state = CompactState::new();
        state.last_input_tokens = 190_000; // below 197k

        let mut engine = make_compact_engine(config, state, vec![]);
        assert!(engine.run_compaction().await.is_ok());
    }

    // -- Microcompact runs when count trigger fires --

    #[tokio::test]
    async fn microcompact_clears_old_results() {
        // 12 tool results with keep_recent=3 (threshold=6) → should clear 9
        let mut messages = Vec::new();
        for i in 0..12 {
            let id = format!("t{i}");
            messages.push(tool_use_msg(&id, "Read"));
            messages.push(tool_result_msg(&id, &format!("data-{i}")));
        }

        let config = CompactConfig {
            micro_keep_recent: 3,
            ..Default::default()
        };
        let state = CompactState::new();

        let mut engine = make_compact_engine(config, state, messages);
        engine.run_compaction().await.unwrap();

        // Last 3 tool results should be preserved
        let cleared_count = engine
            .messages
            .iter()
            .flat_map(|m| &m.content)
            .filter(|b| {
                matches!(b, ContentBlock::ToolResult { content, .. } if content == "[Tool result cleared]")
            })
            .count();

        assert_eq!(cleared_count, 9);
    }

    // -- Disabled config skips micro and auto but not emergency --

    #[tokio::test]
    async fn disabled_config_skips_micro_auto() {
        let mut messages = Vec::new();
        for i in 0..12 {
            let id = format!("t{i}");
            messages.push(tool_use_msg(&id, "Read"));
            messages.push(tool_result_msg(&id, &format!("data-{i}")));
        }

        let config = CompactConfig {
            enabled: false,
            micro_keep_recent: 3,
            ..Default::default()
        };
        let state = CompactState::new();

        let mut engine = make_compact_engine(config, state, messages);
        engine.run_compaction().await.unwrap();

        // Nothing should be cleared (microcompact skipped)
        let cleared_count = engine
            .messages
            .iter()
            .flat_map(|m| &m.content)
            .filter(|b| {
                matches!(b, ContentBlock::ToolResult { content, .. } if content == "[Tool result cleared]")
            })
            .count();

        assert_eq!(
            cleared_count, 0,
            "microcompact should be skipped when disabled"
        );
    }

    #[tokio::test]
    async fn disabled_config_still_fires_emergency() {
        let config = CompactConfig {
            enabled: false,
            context_window: 200_000,
            emergency_buffer: 3_000,
            ..Default::default()
        };
        let mut state = CompactState::new();
        state.last_input_tokens = 198_000;

        let mut engine = make_compact_engine(config, state, vec![]);
        let result = engine.run_compaction().await;

        assert!(
            matches!(result, Err(super::AgentError::ContextTooLong { .. })),
            "emergency should fire even when disabled"
        );
    }

    // -- Zero tokens on first turn does not trigger anything --

    #[tokio::test]
    async fn first_turn_zero_tokens_no_compaction() {
        let config = CompactConfig::default();
        let state = CompactState::new(); // last_input_tokens = 0

        let mut engine = make_compact_engine(config, state, vec![]);
        assert!(engine.run_compaction().await.is_ok());
        assert_eq!(engine.compact_state.last_input_tokens, 0);
    }

    // -- Circuit broken prevents autocompact, emergency still fires --

    #[tokio::test]
    async fn circuit_broken_skips_auto_but_emergency_fires() {
        let config = CompactConfig {
            context_window: 200_000,
            emergency_buffer: 3_000,
            max_failures: 3,
            ..Default::default()
        };
        let mut state = CompactState::new();
        state.last_input_tokens = 198_000; // triggers both auto and emergency
        state.consecutive_failures = 3; // circuit broken

        let mut engine = make_compact_engine(config, state, vec![]);
        let result = engine.run_compaction().await;

        // Auto is skipped due to circuit breaker; emergency fires
        assert!(matches!(
            result,
            Err(super::AgentError::ContextTooLong { .. })
        ));
    }
}

// ---------------------------------------------------------------------------
// Phase 3 tests — plan mode integration in apply_context_modifiers()
// ---------------------------------------------------------------------------

#[cfg(test)]
mod plan_mode_tests {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex};

    use nomi_providers::{LlmProvider, ProviderError};
    use nomi_tools::registry::ToolRegistry;
    use nomi_types::llm::{LlmEvent, LlmRequest};
    use nomi_types::skill_types::{ContextModifier, PlanModeTransition};

    use crate::compact::state::CompactState;
    use crate::confirm::ToolConfirmer;
    use crate::output::OutputSink;
    use crate::plan::state::PlanState;

    struct NullOutput;
    impl OutputSink for NullOutput {
        fn emit_text_delta(&self, _: &str, _: &str) {}
        fn emit_thinking(&self, _: &str, _: &str) {}
        fn emit_tool_call(&self, _: &str, _: &str, _: &str) {}
        fn emit_tool_result(&self, _: &str, _: &str, _: bool, _: &str) {}
        fn emit_stream_start(&self, _: &str) {}
        fn emit_stream_end(&self, _: &str, _: usize, _: u64, _: u64, _: u64, _: u64) {}
        fn emit_error(&self, _: &str) {}
        fn emit_info(&self, _: &str) {}
    }

    struct NullProvider;
    #[async_trait::async_trait]
    impl LlmProvider for NullProvider {
        async fn stream(
            &self,
            _: &LlmRequest,
        ) -> Result<tokio::sync::mpsc::Receiver<LlmEvent>, ProviderError> {
            let (_tx, rx) = tokio::sync::mpsc::channel(1);
            Ok(rx)
        }
    }

    fn make_plan_engine(allow_list: Vec<String>) -> super::AgentEngine {
        let flag = Arc::new(AtomicBool::new(false));
        super::AgentEngine {
            provider: Arc::new(NullProvider),
            tools: ToolRegistry::new(),
            messages: vec![],
            system_prompt: String::new(),
            model: "test-model".to_string(),
            max_tokens: 4096,
            max_turns: Some(10),
            total_usage: Default::default(),
            thinking: None,
            compat: nomi_config::compat::ProviderCompat::anthropic_defaults(),
            confirmer: Arc::new(Mutex::new(ToolConfirmer::new(true, allow_list.clone()))),
            hooks: None,
            session_manager: None,
            current_session: None,
            output: Arc::new(NullOutput),
            current_msg_id: String::new(),
            approval_manager: None,
            protocol_writer: None,
            allow_list,
            current_reasoning_effort: None,
            compact_config: nomi_config::compact::CompactConfig::default(),
            compact_state: CompactState::new(),
            plan_state: PlanState::default(),
            plan_active_flag: Some(flag),
            cache_detector: super::CacheBreakDetector::new(),
            compaction_level: nomi_compact::CompactionLevel::default(),
            toon_enabled: false,
            max_recent_images: 3,
            commands: crate::commands::default_registry(),
            goal: None,
            cancel_token: None,
            stagnation_guard: crate::loop_guard::StagnationGuard::new(crate::engine::STAGNATION_THRESHOLD),
            context_contributors: Vec::new(),
            steering_inbox: None,
            process_supervisor: None,
            last_turn_start_len: None,
        }
    }

    // --- TC-3.5-03: Enter transition activates plan mode ---

    #[test]
    fn enter_transition_activates_plan_mode() {
        let mut engine = make_plan_engine(vec!["Read".into(), "Bash".into()]);
        let modifiers = vec![Some(ContextModifier {
            plan_mode_transition: Some(PlanModeTransition::Enter),
            ..Default::default()
        })];

        engine.apply_context_modifiers(&modifiers);

        assert!(engine.plan_state.is_active, "plan mode should be active");
        assert_eq!(
            engine.plan_state.pre_plan_allow_list,
            vec!["Read".to_string(), "Bash".to_string()],
            "pre_plan_allow_list should capture original allow_list"
        );
    }

    // --- TC-3.5-03 supplement: shared flag updated on enter ---

    #[test]
    fn enter_transition_updates_shared_flag() {
        let mut engine = make_plan_engine(vec![]);
        let flag = engine.plan_active_flag.clone().unwrap();
        assert!(!flag.load(Ordering::Acquire));

        engine.apply_context_modifiers(&[Some(ContextModifier {
            plan_mode_transition: Some(PlanModeTransition::Enter),
            ..Default::default()
        })]);

        assert!(flag.load(Ordering::Acquire), "shared flag should be true");
    }

    // --- TC-3.5-04: Exit transition deactivates plan mode and restores allow_list ---

    #[test]
    fn exit_transition_deactivates_and_restores() {
        let mut engine = make_plan_engine(vec!["Read".into(), "Bash".into()]);

        // Enter plan mode first
        engine.apply_context_modifiers(&[Some(ContextModifier {
            plan_mode_transition: Some(PlanModeTransition::Enter),
            ..Default::default()
        })]);
        assert!(engine.plan_state.is_active);

        // Modify allow_list while in plan mode (simulating a skill adding tools)
        engine.allow_list.push("NewTool".into());

        // Exit plan mode
        engine.apply_context_modifiers(&[Some(ContextModifier {
            plan_mode_transition: Some(PlanModeTransition::Exit { plan_content: None }),
            ..Default::default()
        })]);

        assert!(!engine.plan_state.is_active, "plan mode should be inactive");
        assert_eq!(
            engine.allow_list,
            vec!["Read".to_string(), "Bash".to_string()],
            "allow_list should be restored to pre-plan state"
        );
    }

    // --- TC-3.5-04 supplement: shared flag updated on exit ---

    #[test]
    fn exit_transition_updates_shared_flag() {
        let mut engine = make_plan_engine(vec![]);
        let flag = engine.plan_active_flag.clone().unwrap();

        // Enter
        engine.apply_context_modifiers(&[Some(ContextModifier {
            plan_mode_transition: Some(PlanModeTransition::Enter),
            ..Default::default()
        })]);
        assert!(flag.load(Ordering::Acquire));

        // Exit
        engine.apply_context_modifiers(&[Some(ContextModifier {
            plan_mode_transition: Some(PlanModeTransition::Exit { plan_content: None }),
            ..Default::default()
        })]);
        assert!(
            !flag.load(Ordering::Acquire),
            "shared flag should be false after exit"
        );
    }

    // --- TC-3.5-05: No transition does not affect plan state ---

    #[test]
    fn no_transition_does_not_affect_plan_state() {
        let mut engine = make_plan_engine(vec![]);

        engine.apply_context_modifiers(&[Some(ContextModifier {
            model: Some("new-model".into()),
            plan_mode_transition: None,
            ..Default::default()
        })]);

        assert_eq!(engine.model, "new-model");
        assert!(
            !engine.plan_state.is_active,
            "plan state should remain inactive"
        );
    }

    // --- Enter + other modifiers applied together ---

    #[test]
    fn enter_with_model_override_both_applied() {
        let mut engine = make_plan_engine(vec![]);

        engine.apply_context_modifiers(&[Some(ContextModifier {
            model: Some("planning-model".into()),
            plan_mode_transition: Some(PlanModeTransition::Enter),
            ..Default::default()
        })]);

        assert!(engine.plan_state.is_active);
        assert_eq!(engine.model, "planning-model");
    }

    // --- No plan_active_flag set does not panic ---

    #[test]
    fn enter_without_flag_does_not_panic() {
        let mut engine = make_plan_engine(vec![]);
        engine.plan_active_flag = None;

        engine.apply_context_modifiers(&[Some(ContextModifier {
            plan_mode_transition: Some(PlanModeTransition::Enter),
            ..Default::default()
        })]);

        assert!(engine.plan_state.is_active);
    }
}

#[cfg(test)]
mod handle_command_tests {
    use std::sync::{Arc, Mutex};

    use nomi_providers::{LlmProvider, ProviderError};
    use nomi_tools::registry::ToolRegistry;
    use nomi_types::llm::{LlmEvent, LlmRequest};
    use nomi_types::message::{ContentBlock, Message, Role};

    use crate::compact::state::CompactState;
    use crate::confirm::ToolConfirmer;
    use crate::output::OutputSink;

    struct NullOutput;
    impl OutputSink for NullOutput {
        fn emit_text_delta(&self, _: &str, _: &str) {}
        fn emit_thinking(&self, _: &str, _: &str) {}
        fn emit_tool_call(&self, _: &str, _: &str, _: &str) {}
        fn emit_tool_result(&self, _: &str, _: &str, _: bool, _: &str) {}
        fn emit_stream_start(&self, _: &str) {}
        fn emit_stream_end(&self, _: &str, _: usize, _: u64, _: u64, _: u64, _: u64) {}
        fn emit_error(&self, _: &str) {}
        fn emit_info(&self, _: &str) {}
    }

    struct NullProvider;
    #[async_trait::async_trait]
    impl LlmProvider for NullProvider {
        async fn stream(
            &self,
            _: &LlmRequest,
        ) -> Result<tokio::sync::mpsc::Receiver<LlmEvent>, ProviderError> {
            let (_tx, rx) = tokio::sync::mpsc::channel(1);
            Ok(rx)
        }
    }

    fn make_engine() -> super::AgentEngine {
        super::AgentEngine {
            provider: Arc::new(NullProvider),
            tools: ToolRegistry::new(),
            messages: vec![],
            system_prompt: String::new(),
            model: "test-model".to_string(),
            max_tokens: 4096,
            max_turns: Some(10),
            total_usage: Default::default(),
            thinking: None,
            compat: nomi_config::compat::ProviderCompat::anthropic_defaults(),
            confirmer: Arc::new(Mutex::new(ToolConfirmer::new(true, vec![]))),
            hooks: None,
            session_manager: None,
            current_session: None,
            output: Arc::new(NullOutput),
            current_msg_id: String::new(),
            approval_manager: None,
            protocol_writer: None,
            allow_list: vec![],
            current_reasoning_effort: None,
            compact_config: nomi_config::compact::CompactConfig::default(),
            compact_state: CompactState::new(),
            plan_state: Default::default(),
            plan_active_flag: None,
            cache_detector: super::CacheBreakDetector::new(),
            compaction_level: nomi_compact::CompactionLevel::default(),
            toon_enabled: false,
            max_recent_images: 3,
            commands: crate::commands::default_registry(),
            goal: None,
            cancel_token: None,
            stagnation_guard: crate::loop_guard::StagnationGuard::new(crate::engine::STAGNATION_THRESHOLD),
            context_contributors: Vec::new(),
            steering_inbox: None,
            process_supervisor: None,
            last_turn_start_len: None,
        }
    }

    #[tokio::test]
    async fn handle_command_quit() {
        let mut engine = make_engine();
        let result = engine.handle_command("/quit").await;
        assert!(matches!(
            result,
            Some(Ok(crate::commands::CommandResult::Exit))
        ));
    }

    #[tokio::test]
    async fn handle_command_exit_alias() {
        let mut engine = make_engine();
        let result = engine.handle_command("/exit").await;
        assert!(matches!(
            result,
            Some(Ok(crate::commands::CommandResult::Exit))
        ));
    }

    #[tokio::test]
    async fn handle_command_unknown() {
        let mut engine = make_engine();
        let result = engine.handle_command("/nonexistent").await;
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn handle_command_clear() {
        let mut engine = make_engine();
        engine.messages.push(Message::new(
            Role::User,
            vec![ContentBlock::Text {
                text: "hello".to_string(),
            }],
        ));
        assert_eq!(engine.messages.len(), 1);

        let result = engine.handle_command("/clear").await;
        assert!(matches!(
            result,
            Some(Ok(crate::commands::CommandResult::Continue))
        ));
        assert!(engine.messages.is_empty());
        assert_eq!(engine.compact_state.last_input_tokens, 0);
    }

    #[tokio::test]
    async fn handle_command_with_args() {
        let mut engine = make_engine();
        let result = engine.handle_command("/help compact").await;
        assert!(matches!(
            result,
            Some(Ok(crate::commands::CommandResult::Continue))
        ));
    }

    #[tokio::test]
    async fn handle_command_not_a_command() {
        let mut engine = make_engine();
        let result = engine.handle_command("hello world").await;
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn run_intercepts_help_returns_zero_turns() {
        let mut engine = make_engine();
        let result = engine.run("/help", "msg-1").await.unwrap();
        assert_eq!(result.turns, 0);
        assert_eq!(result.usage.input_tokens, 0);
    }

    #[tokio::test]
    async fn run_intercepts_quit_returns_user_aborted() {
        let mut engine = make_engine();
        let err = engine.run("/quit", "msg-1").await.unwrap_err();
        assert!(matches!(err, super::AgentError::UserAborted));
    }

    #[test]
    fn slash_command_list_returns_all() {
        let engine = make_engine();
        let list = engine.slash_command_list();
        assert!(list.len() >= 4);
        let names: Vec<&str> = list.iter().map(|(n, _)| n.as_str()).collect();
        assert!(names.contains(&"help"));
        assert!(names.contains(&"compact"));
        assert!(names.contains(&"clear"));
        assert!(names.contains(&"quit"));
    }
}

#[derive(Debug)]
pub struct AgentResult {
    pub text: String,
    pub stop_reason: StopReason,
    pub usage: TokenUsage,
    pub turns: usize,
}

#[derive(Debug, thiserror::Error)]
pub enum AgentError {
    #[error("API error: {0}")]
    ApiError(String),
    #[error("Provider error: {0}")]
    Provider(#[from] ProviderError),
    #[error("User aborted the session")]
    UserAborted,
    #[error("Context window nearly full ({input_tokens} tokens used, limit {limit})")]
    ContextTooLong { input_tokens: u64, limit: usize },
}

#[cfg(test)]
mod cache_diagnostic_tests {
    use super::cache_diagnostic_message;
    use crate::cache_diagnostics::{CacheBreakCause, CacheDiagnostic};

    #[test]
    fn full_miss_is_silent_by_default_and_never_an_error() {
        // A full cache miss — including a benign server-side TTL expiry during the
        // idle gap between AutoWork turns — must NOT surface unless diagnostics are
        // explicitly enabled, and must NEVER be an error. Before the fix this path
        // called emit_error, which the AutoWork orchestrator treated as a FAILED
        // turn (re-pend / tag pause).
        let diag = CacheDiagnostic::FullMiss { cause: CacheBreakCause::TtlExpiry };
        assert_eq!(cache_diagnostic_message(&diag, false), None);
    }

    #[test]
    fn full_miss_surfaces_as_info_text_when_diagnostics_enabled() {
        let diag = CacheDiagnostic::FullMiss { cause: CacheBreakCause::TtlExpiry };
        assert_eq!(
            cache_diagnostic_message(&diag, true).as_deref(),
            Some("Cache full miss: TtlExpiry")
        );
    }

    #[test]
    fn healthy_and_partial_are_gated_by_the_flag() {
        let healthy = CacheDiagnostic::Healthy { hit_rate: 0.9 };
        assert_eq!(cache_diagnostic_message(&healthy, false), None);
        assert!(cache_diagnostic_message(&healthy, true).is_some());

        let partial = CacheDiagnostic::PartialMiss { hit_rate: 0.5, cause: CacheBreakCause::TtlExpiry };
        assert_eq!(cache_diagnostic_message(&partial, false), None);
        assert!(cache_diagnostic_message(&partial, true).is_some());
    }
}

#[cfg(test)]
mod transcript_tests {
    use super::{render_transcript, truncate_chars};
    use nomi_types::message::{ContentBlock, Message, Role};
    use serde_json::json;

    #[test]
    fn render_tags_roles_and_keeps_text() {
        let messages = vec![
            Message::new(Role::User, vec![ContentBlock::Text { text: "fix the bug".into() }]),
            Message::new(
                Role::Assistant,
                vec![ContentBlock::Text { text: "done".into() }],
            ),
        ];
        let t = render_transcript(&messages);
        assert!(t.contains("[user] fix the bug"));
        assert!(t.contains("[assistant] done"));
    }

    #[test]
    fn render_drops_thinking() {
        let messages = vec![Message::new(
            Role::Assistant,
            vec![
                ContentBlock::Thinking {
                    thinking: "secret reasoning".into(),
                    signature: None,
                },
                ContentBlock::Text { text: "visible".into() },
            ],
        )];
        let t = render_transcript(&messages);
        assert!(!t.contains("secret reasoning"));
        assert!(t.contains("[assistant] visible"));
    }

    #[test]
    fn render_compresses_tool_use_and_result() {
        let messages = vec![
            Message::new(
                Role::Assistant,
                vec![ContentBlock::ToolUse {
                    id: "t1".into(),
                    name: "Read".into(),
                    input: json!({"path": "/tmp/a.txt"}),
                    extra: None,
                }],
            ),
            Message::new(
                Role::Tool,
                vec![ContentBlock::ToolResult {
                    tool_use_id: "t1".into(),
                    content: "file body".into(),
                    is_error: false,
                    images: vec![],
                }],
            ),
        ];
        let t = render_transcript(&messages);
        assert!(t.contains("[tool Read]"));
        assert!(t.contains("/tmp/a.txt"));
        assert!(t.contains("[tool result] file body"));
    }

    #[test]
    fn render_marks_tool_error() {
        let messages = vec![Message::new(
            Role::Tool,
            vec![ContentBlock::ToolResult {
                tool_use_id: "t1".into(),
                content: "boom".into(),
                is_error: true,
                images: vec![],
            }],
        )];
        let t = render_transcript(&messages);
        assert!(t.contains("[tool result error] boom"));
    }

    #[test]
    fn truncate_keeps_short_and_cuts_long() {
        assert_eq!(truncate_chars("short", 100), "short");
        let long = "x".repeat(700);
        let cut = truncate_chars(&long, 600);
        assert!(cut.contains("(truncated)"));
        assert!(cut.chars().count() < 700);
    }
}

#[cfg(test)]
mod tool_efficiency_tests {
    use super::{AgentResult, ToolEfficiencyStats};
    use crate::orchestration::SKIPPED_AFTER_PRIOR_ERROR;
    use nomi_execution::{CapabilityPolicy, ProcessSupervisor, SupervisorConfig};
    use nomi_tools::{
        exec_command::ExecCommandTool, process_store::ProcessStore, read::ReadTool,
        registry::ToolRegistry,
    };
    use nomi_types::message::ContentBlock;
    use nomi_types::message::{StopReason, TokenUsage};
    use serde_json::json;
    use std::sync::Arc;

    fn efficiency_registry() -> ToolRegistry {
        let cwd = std::env::current_dir().expect("current directory");
        let mut registry = ToolRegistry::new();
        registry.register(Box::new(ReadTool::new(None, Some(cwd.clone()))));
        registry.register(Box::new(ExecCommandTool::new(
            ProcessSupervisor::new(SupervisorConfig::default()),
            Arc::new(ProcessStore::new()),
            cwd.clone(),
            CapabilityPolicy::local_owner(cwd),
        )));
        registry
    }

    #[test]
    fn accounting_distinguishes_parallel_width_scripts_and_batch_reads() {
        let calls = vec![
            ContentBlock::ToolUse {
                id: "exec".into(),
                name: "exec_command".into(),
                input: json!({ "script": "print('x')", "language": "python", "timeout": 1000 }),
                extra: None,
            },
            ContentBlock::ToolUse {
                id: "read".into(),
                name: "Read".into(),
                input: json!({ "file_paths": ["a", "b", "c"] }),
                extra: None,
            },
            ContentBlock::ToolUse {
                id: "grep".into(),
                name: "Grep".into(),
                input: json!({ "pattern": "needle" }),
                extra: None,
            },
        ];
        let mut stats = ToolEfficiencyStats::default();
        let registry = efficiency_registry();

        stats.observe_model_turn_attempt();
        stats.observe_model_turn_attempt();
        stats.observe_calls(&registry, &calls);
        stats.observe_calls(&registry, &calls[..1]);

        assert_eq!(stats.model_turn_attempts, 2);
        assert_eq!(stats.model_turns_with_tools, 2);
        assert_eq!(stats.total_tool_calls, 4);
        assert_eq!(stats.max_calls_in_model_turn, 3);
        assert_eq!(stats.exec_command_script_calls, 2);
        assert_eq!(stats.batch_read_files_requested, 3);
    }

    #[test]
    fn accounting_uses_the_same_schema_coercion_as_execution() {
        let calls = vec![
            ContentBlock::ToolUse {
                id: "exec".into(),
                name: "exec_command".into(),
                input: serde_json::Value::String(
                    r#"{"script":"print(1)","language":"python","timeout":1000}"#.into(),
                ),
                extra: None,
            },
            ContentBlock::ToolUse {
                id: "read".into(),
                name: "Read".into(),
                input: json!({ "file_paths": "[\"a\",\"b\"]" }),
                extra: None,
            },
        ];
        let mut stats = ToolEfficiencyStats::default();

        stats.observe_calls(&efficiency_registry(), &calls);

        assert_eq!(stats.exec_command_script_calls, 1);
        assert_eq!(stats.batch_read_files_requested, 2);
    }

    #[test]
    fn cooperative_cancel_has_a_distinct_terminal_classification() {
        let mut stats = ToolEfficiencyStats::default();
        stats.observe_cooperative_cancellation();
        let result = Ok(AgentResult {
            text: String::new(),
            stop_reason: StopReason::EndTurn,
            usage: TokenUsage::default(),
            turns: 2,
        });

        assert_eq!(
            stats.terminal_dimensions(&result),
            ("cancelled", "cancelled", "none", 2)
        );
    }

    #[test]
    fn accounting_counts_errors_and_prior_error_skips() {
        let results = vec![
            ContentBlock::ToolResult {
                tool_use_id: "failed".into(),
                content: "boom".into(),
                is_error: true,
                images: vec![],
            },
            ContentBlock::ToolResult {
                tool_use_id: "skipped".into(),
                content: SKIPPED_AFTER_PRIOR_ERROR.into(),
                is_error: true,
                images: vec![],
            },
        ];
        let mut stats = ToolEfficiencyStats::default();

        stats.observe_results(&results);

        assert_eq!(stats.error_results, 2);
        assert_eq!(stats.skipped_after_prior_error, 1);
    }
}

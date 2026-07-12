use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use tokio::sync::Semaphore;

use nomi_config::config::Config;
use nomi_execution::{CapabilityPolicy, ProcessSupervisor, SupervisorConfig};
use nomi_providers::LlmProvider;
use nomi_tools::bash::BashTool;
use nomi_tools::edit::EditTool;
use nomi_tools::glob::GlobTool;
use nomi_tools::grep::GrepTool;
use nomi_tools::read::ReadTool;
use nomi_tools::registry::ToolRegistry;
use nomi_tools::write::WriteTool;
use nomi_types::message::TokenUsage;

use crate::engine::AgentEngine;
use crate::output::OutputSink;
use crate::output::null_sink::NullSink;

// Re-export from nomi-types — single source of truth
pub use nomi_types::spawner::{ForkOverrides, Spawner, SubAgentConfig, SubAgentResult};

/// Spawns independent child agents that share the parent's LLM provider.
///
/// Sub-agents use a [`NullSink`] so their streaming output is silently
/// discarded.  Results are collected via `engine.run()` and returned to the
/// parent which emits them as a single `tool_result` event — matching the
/// Claude Code pattern where only the parent writes to stdout.
pub struct AgentSpawner {
    provider: Arc<dyn LlmProvider>,
    base_config: Config,
    cwd: PathBuf,
    /// Shared across `clone_for_spawn` so the concurrency cap is global to this
    /// spawner (all fan-outs draw from the same permit pool).
    concurrency: Arc<Semaphore>,
    /// Optional shared token ceiling (opt-in; None = uncapped).
    token_budget: Option<Arc<TokenBudget>>,
    execution_capability: CapabilityPolicy,
    write_root: Option<PathBuf>,
    parent_tool_scope: ToolScope,
}

impl AgentSpawner {
    pub fn new(provider: Arc<dyn LlmProvider>, config: Config, cwd: PathBuf) -> Self {
        let parent_tool_scope = ToolScope::from_config(&config.tools.builtin_allowlist);
        let execution_capability = CapabilityPolicy::local_owner(cwd.clone());
        Self {
            provider,
            base_config: config,
            cwd,
            concurrency: Arc::new(Semaphore::new(MAX_CONCURRENT_SUBAGENTS)),
            token_budget: None,
            execution_capability,
            write_root: None,
            parent_tool_scope,
        }
    }

    pub fn with_execution_policy(
        mut self,
        capability: CapabilityPolicy,
        write_root: Option<PathBuf>,
        parent_allowlist: Vec<String>,
    ) -> Self {
        self.execution_capability = capability;
        self.write_root = write_root;
        self.parent_tool_scope = ToolScope::from_config(&parent_allowlist);
        self
    }

    /// Enable a shared cumulative token ceiling for all sub-agents (§3.4).
    pub fn with_token_budget(mut self, budget: Option<Arc<TokenBudget>>) -> Self {
        self.token_budget = budget;
        self
    }

    /// Spawn a single sub-agent and wait for result.
    pub async fn spawn_one(&self, sub_config: SubAgentConfig) -> SubAgentResult {
        self.spawn_one_with_board(sub_config, None).await
    }

    /// Spawn a single sub-agent, optionally giving it the shared task board so it
    /// can coordinate with siblings (TIER 2, §3.4).
    async fn spawn_one_with_board(
        &self,
        sub_config: SubAgentConfig,
        board: Option<Arc<crate::taskboard::TaskBoard>>,
    ) -> SubAgentResult {
        // Shared token ceiling (opt-in): refuse to start a new sub-agent once
        // the budget is spent, rather than launching work that won't be paid for.
        if let Some(budget) = &self.token_budget
            && !budget.can_begin()
        {
            return SubAgentResult {
                name: sub_config.name,
                text: "Sub-agent skipped: shared token budget exhausted.".to_string(),
                usage: TokenUsage::default(),
                turns: 0,
                is_error: true,
            };
        }

        let mut config = self.base_config.clone();
        config.max_turns = Some(sub_config.max_turns);
        config.max_tokens = sub_config.max_tokens;
        if let Some(sp) = sub_config.system_prompt.clone() {
            config.system_prompt = Some(sp);
        }
        config.session.enabled = false;

        tracing::info!(target: "nomi_agent", cwd = %self.cwd.display(), "sub-agent spawned with workspace cwd");

        let agent_name = sub_config.name.clone();
        let board_arg = board.map(|b| (b, agent_name.as_str()));
        let supervisor = ProcessSupervisor::new(SupervisorConfig::default());
        let role_scope = ToolScope::from_role(&sub_config.allowed_tools);
        let tools = match build_tool_registry(
            self.parent_tool_scope.intersect(&role_scope),
            &self.cwd,
            &self.execution_capability,
            self.write_root.as_deref(),
            Arc::clone(&supervisor),
            board_arg,
        ) {
            Ok(tools) => tools,
            Err(error) => {
                return SubAgentResult {
                    name: sub_config.name,
                    text: format!("Sub-agent capability denied: {error}"),
                    usage: TokenUsage::default(),
                    turns: 0,
                    is_error: true,
                };
            }
        };
        let output: Arc<dyn OutputSink> = Arc::new(NullSink);
        let mut engine = AgentEngine::new_with_provider(
            self.provider.clone(),
            config,
            tools,
            output,
            self.cwd.clone(),
        );
        engine.set_process_supervisor(Arc::clone(&supervisor));

        let result =
            run_subagent(engine, sub_config.name, &sub_config.prompt, supervisor).await;
        // Charge the shared budget for what this sub-agent actually consumed.
        if let Some(budget) = &self.token_budget {
            budget.record(result.usage.input_tokens + result.usage.output_tokens);
        }
        result
    }

    /// Spawn multiple sub-agents in parallel, bounded by the shared concurrency
    /// permit pool so a large fan-out cannot start every engine at once.
    pub async fn spawn_parallel(&self, sub_configs: Vec<SubAgentConfig>) -> Vec<SubAgentResult> {
        let tasks: Vec<_> = sub_configs
            .into_iter()
            .map(|config| {
                let spawner = self.clone_for_spawn();
                async move { spawner.spawn_one(config).await }
            })
            .collect();

        run_bounded(self.concurrency.clone(), tasks)
            .await
            .into_iter()
            .map(|joined| {
                joined.unwrap_or_else(|e| SubAgentResult {
                    name: "unknown".to_string(),
                    text: format!("Task join error: {}", e),
                    usage: TokenUsage::default(),
                    turns: 0,
                    is_error: true,
                })
            })
            .collect()
    }

    /// Like [`Self::spawn_parallel`] but every sub-agent shares one in-memory
    /// task board (TIER 2 coordination, §3.4): siblings can claim work to avoid
    /// duplication and report progress to one another via the `shared_tasks` tool.
    pub async fn spawn_parallel_coordinated(
        &self,
        sub_configs: Vec<SubAgentConfig>,
    ) -> Vec<SubAgentResult> {
        let board = Arc::new(crate::taskboard::TaskBoard::new());
        let tasks: Vec<_> = sub_configs
            .into_iter()
            .map(|config| {
                let spawner = self.clone_for_spawn();
                let board = board.clone();
                async move { spawner.spawn_one_with_board(config, Some(board)).await }
            })
            .collect();

        run_bounded(self.concurrency.clone(), tasks)
            .await
            .into_iter()
            .map(|joined| {
                joined.unwrap_or_else(|e| SubAgentResult {
                    name: "unknown".to_string(),
                    text: format!("Task join error: {}", e),
                    usage: TokenUsage::default(),
                    turns: 0,
                    is_error: true,
                })
            })
            .collect()
    }

    fn clone_for_spawn(&self) -> Self {
        Self {
            provider: self.provider.clone(),
            base_config: self.base_config.clone(),
            cwd: self.cwd.clone(),
            concurrency: self.concurrency.clone(),
            token_budget: self.token_budget.clone(),
            execution_capability: self.execution_capability.clone(),
            write_root: self.write_root.clone(),
            parent_tool_scope: self.parent_tool_scope.clone(),
        }
    }

    /// Delegate the parent's repository-scoped authority to the exact detached
    /// worktree represented by `worktree`.
    ///
    /// The path is accepted only after [`nomi_tools::worktree::Worktree`]
    /// verifies its live Git registration. Capability and write roots are then
    /// translated relative to the source workspace instead of granting the
    /// worktree's parent directory.
    fn clone_for_isolated_worktree(
        &self,
        worktree: &nomi_tools::worktree::Worktree,
    ) -> Result<Self, String> {
        let source = self
            .cwd
            .canonicalize()
            .map_err(|error| format!("invalid source workspace {}: {error}", self.cwd.display()))?;
        if !self.execution_capability.cwd_roots.iter().any(|root| {
            root.canonicalize()
                .is_ok_and(|root| source.starts_with(root))
        }) {
            return Err(format!(
                "source workspace {} is outside inherited capability roots",
                source.display()
            ));
        }
        let cwd = worktree.verified_cwd(&source)?;
        let mut child = self.clone_for_spawn();
        child.cwd = cwd.clone();
        child.execution_capability.cwd_roots = vec![cwd.clone()];
        child.write_root = Some(match &self.write_root {
            Some(root) => translate_root_to_worktree(&source, &cwd, root, "write")?,
            None => cwd.clone(),
        });
        if let nomi_execution::SandboxPolicy::MacSeatbelt { write_roots } =
            &mut child.execution_capability.sandbox
        {
            let mut translated = Vec::with_capacity(write_roots.len());
            for root in write_roots.iter() {
                translated.push(translate_root_to_worktree(
                    &source,
                    &cwd,
                    root,
                    "Seatbelt write",
                )?);
            }
            *write_roots = translated;
        }
        Ok(child)
    }

    /// Like [`Self::spawn_parallel`] but each sub-agent runs in its own detached
    /// git worktree, so concurrent edits don't clobber the shared tree or each
    /// other (§3.4 worktree isolation). Each result has the sub-agent's changes
    /// appended as a unified diff for the parent to review/apply; the worktree is
    /// removed afterwards. Falls back to plain `spawn_parallel` when the
    /// workspace is not a git repo.
    pub async fn spawn_parallel_isolated(&self, sub_configs: Vec<SubAgentConfig>) -> Vec<SubAgentResult> {
        if !nomi_tools::worktree::is_git_repo(&self.cwd) {
            tracing::warn!(target: "nomi_agent", "isolate requested but workspace is not a git repo — running without worktree isolation");
            return self.spawn_parallel(sub_configs).await;
        }
        let tasks: Vec<_> = sub_configs
            .into_iter()
            .map(|config| {
                let base = self.clone_for_spawn();
                let repo = self.cwd.clone();
                async move {
                    match nomi_tools::worktree::Worktree::create(&repo) {
                        Ok(wt) => {
                            let spawner = match base.clone_for_isolated_worktree(&wt) {
                                Ok(spawner) => spawner,
                                Err(error) => {
                                    return SubAgentResult {
                                        name: config.name,
                                        text: format!(
                                            "Worktree isolation capability setup failed: {error}"
                                        ),
                                        usage: TokenUsage::default(),
                                        turns: 0,
                                        is_error: true,
                                    };
                                }
                            };
                            let mut result = spawner.spawn_one(config).await;
                            match wt.capture_diff() {
                                Ok(diff) if !diff.trim().is_empty() => {
                                    result.text = format!(
                                        "{}\n\n--- Changes (made in an isolated worktree; apply with ApplyPatch if you want them) ---\n{}",
                                        result.text, diff
                                    );
                                }
                                _ => {}
                            }
                            result // `wt` dropped here → worktree removed
                        }
                        Err(e) => SubAgentResult {
                            name: config.name,
                            text: format!("Worktree isolation failed: {e}"),
                            usage: TokenUsage::default(),
                            turns: 0,
                            is_error: true,
                        },
                    }
                }
            })
            .collect();

        run_bounded(self.concurrency.clone(), tasks)
            .await
            .into_iter()
            .map(|joined| {
                joined.unwrap_or_else(|e| SubAgentResult {
                    name: "unknown".to_string(),
                    text: format!("Task join error: {}", e),
                    usage: TokenUsage::default(),
                    turns: 0,
                    is_error: true,
                })
            })
            .collect()
    }
}

#[async_trait]
impl Spawner for AgentSpawner {
    async fn spawn_fork(
        &self,
        sub_config: SubAgentConfig,
        overrides: ForkOverrides,
    ) -> SubAgentResult {
        let mut config = self.base_config.clone();
        config.max_turns = Some(sub_config.max_turns);
        config.max_tokens = sub_config.max_tokens;
        if let Some(sp) = sub_config.system_prompt.clone() {
            config.system_prompt = Some(sp);
        }
        config.session.enabled = false;
        if let Some(model) = overrides.model.clone() {
            config.model = model;
        }

        let supervisor = ProcessSupervisor::new(SupervisorConfig::default());
        let role_scope = ToolScope::from_role(&overrides.allowed_tools);
        let tools = match build_tool_registry(
            self.parent_tool_scope.intersect(&role_scope),
            &self.cwd,
            &self.execution_capability,
            self.write_root.as_deref(),
            Arc::clone(&supervisor),
            None,
        ) {
            Ok(tools) => tools,
            Err(error) => {
                return SubAgentResult {
                    name: sub_config.name,
                    text: format!("Sub-agent capability denied: {error}"),
                    usage: TokenUsage::default(),
                    turns: 0,
                    is_error: true,
                };
            }
        };
        let output: Arc<dyn OutputSink> = Arc::new(NullSink);
        let mut engine = AgentEngine::new_with_provider(
            self.provider.clone(),
            config,
            tools,
            output,
            self.cwd.clone(),
        );
        engine.set_process_supervisor(Arc::clone(&supervisor));
        engine.set_initial_reasoning_effort(overrides.effort.clone());

        run_subagent(engine, sub_config.name, &sub_config.prompt, supervisor).await
    }
}

/// Wall-clock cap for a single sub-agent run. Without this, a sub-agent stuck
/// in a tool-call loop (up to its max_turns) or on a hung provider stream blocks
/// the parent indefinitely — a production hang risk. (Phase 2 multi-agent)
const SUBAGENT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(300);

/// Global cap on concurrently-running sub-agents across all fan-outs from one
/// spawner. Without it a single `Spawn` of N tasks (or several concurrent
/// `Spawn` tool calls sharing the spawner) starts N engines/provider streams at
/// once — a resource and provider-rate-limit storm. (Phase 2 multi-agent §3.4)
const MAX_CONCURRENT_SUBAGENTS: usize = 8;

/// Optional cumulative token ceiling shared across every sub-agent of a spawner
/// (§3.4 "共享 token 预算"). Opt-in: absent → uncapped (default, zero change).
/// A *soft* ceiling — concurrent sub-agents each pass `can_begin` before any of
/// them `record`s, so one fan-out may slightly overshoot; its job is to bound
/// cumulative spend across successive `Spawn` calls / nested fan-outs, not to
/// be an exact per-call limit.
pub struct TokenBudget {
    remaining: std::sync::atomic::AtomicU64,
}

impl TokenBudget {
    pub fn new(limit: u64) -> Self {
        Self {
            remaining: std::sync::atomic::AtomicU64::new(limit),
        }
    }

    /// Whether there is budget left to start another sub-agent.
    pub fn can_begin(&self) -> bool {
        self.remaining.load(std::sync::atomic::Ordering::Relaxed) > 0
    }

    /// Deduct `tokens` from the remaining budget (saturating at 0).
    pub fn record(&self, tokens: u64) {
        let _ = self.remaining.fetch_update(
            std::sync::atomic::Ordering::Relaxed,
            std::sync::atomic::Ordering::Relaxed,
            |cur| Some(cur.saturating_sub(tokens)),
        );
    }

    pub fn remaining(&self) -> u64 {
        self.remaining.load(std::sync::atomic::Ordering::Relaxed)
    }
}

/// Run `tasks` concurrently with at most `semaphore`'s permits in flight at
/// once. Each task holds a permit for its whole run; excess tasks queue until a
/// slot frees. Returns each task's output (or its `JoinError` if the task
/// panicked) — successful results in input order, the rare panics appended.
///
/// Uses a `JoinSet`, so if THIS future is dropped (e.g. the parent turn is
/// cancelled while a fan-out is in flight) every still-running sub-agent task is
/// aborted instead of detaching and burning compute/tokens unattended.
async fn run_bounded<F, T>(
    semaphore: Arc<Semaphore>,
    tasks: Vec<F>,
) -> Vec<Result<T, tokio::task::JoinError>>
where
    F: std::future::Future<Output = T> + Send + 'static,
    T: Send + 'static,
{
    let n = tasks.len();
    let mut set = tokio::task::JoinSet::new();
    for (idx, task) in tasks.into_iter().enumerate() {
        let sem = semaphore.clone();
        set.spawn(async move {
            // Held for the task's whole lifetime; dropped on completion to free
            // the slot for a queued task.
            let _permit = sem.acquire_owned().await.expect("subagent semaphore never closed");
            (idx, task.await)
        });
    }

    let mut slots: Vec<Option<T>> = (0..n).map(|_| None).collect();
    let mut panics: Vec<tokio::task::JoinError> = Vec::new();
    while let Some(joined) = set.join_next().await {
        match joined {
            Ok((idx, val)) => slots[idx] = Some(val),
            // A panicked task loses its index; collect its error and append it.
            Err(e) => panics.push(e),
        }
    }

    let mut results: Vec<Result<T, tokio::task::JoinError>> = Vec::with_capacity(n);
    for value in slots.into_iter().flatten() {
        results.push(Ok(value));
    }
    results.extend(panics.into_iter().map(Err));
    results
}

/// Map a (timeout-wrapped) engine run outcome to a SubAgentResult. Extracted so
/// the timeout/error/success mapping is unit-testable without a live engine.
fn map_subagent_outcome(
    name: String,
    outcome: Result<
        Result<crate::engine::AgentResult, crate::engine::AgentError>,
        tokio::time::error::Elapsed,
    >,
    timeout_secs: u64,
) -> SubAgentResult {
    match outcome {
        Ok(Ok(result)) => SubAgentResult {
            name,
            text: result.text,
            usage: result.usage,
            turns: result.turns,
            is_error: false,
        },
        Ok(Err(e)) => SubAgentResult {
            name,
            text: format!("Sub-agent error: {}", e),
            usage: TokenUsage::default(),
            turns: 0,
            is_error: true,
        },
        Err(_elapsed) => SubAgentResult {
            name,
            text: format!("Sub-agent timed out after {timeout_secs}s and was aborted"),
            usage: TokenUsage::default(),
            turns: 0,
            is_error: true,
        },
    }
}

/// Run an engine to completion under a wall-clock timeout, mapping the outcome.
async fn run_subagent(
    mut engine: AgentEngine,
    name: String,
    prompt: &str,
    supervisor: Arc<ProcessSupervisor>,
) -> SubAgentResult {
    let mut shutdown_guard = SupervisorShutdownOnDrop::new(Arc::clone(&supervisor));
    let outcome = tokio::time::timeout(SUBAGENT_TIMEOUT, engine.run(prompt, "")).await;
    let mut result = map_subagent_outcome(name, outcome, SUBAGENT_TIMEOUT.as_secs());
    let shutdown = supervisor.shutdown().await;
    shutdown_guard.disarm();
    if shutdown.sessions.is_empty() {
        result
    } else {
        let unresolved = shutdown.sessions.iter().any(|session| {
            matches!(
                &session.outcome,
                nomi_execution::ExecutionOutcome::Lost { cleanup, .. } if !cleanup.reaped
            )
        });
        result.text.push_str(&format!(
            "\n\nSupervised process cleanup retired {} active session(s).",
            shutdown.sessions.len()
        ));
        if unresolved {
            result
                .text
                .push_str(" At least one process tree could not be proven reaped.");
            result.is_error = true;
        }
        result
    }
}

struct SupervisorShutdownOnDrop {
    supervisor: Arc<ProcessSupervisor>,
    armed: bool,
}

impl SupervisorShutdownOnDrop {
    fn new(supervisor: Arc<ProcessSupervisor>) -> Self {
        Self {
            supervisor,
            armed: true,
        }
    }

    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for SupervisorShutdownOnDrop {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        enqueue_supervisor_shutdown(Arc::clone(&self.supervisor));
    }
}

fn enqueue_supervisor_shutdown(supervisor: Arc<ProcessSupervisor>) {
    use std::sync::OnceLock;
    use tokio::sync::mpsc;

    static CLEANUP_RELAY: OnceLock<mpsc::UnboundedSender<Arc<ProcessSupervisor>>> =
        OnceLock::new();
    let relay = CLEANUP_RELAY.get_or_init(|| {
        let (sender, mut receiver) = mpsc::unbounded_channel::<Arc<ProcessSupervisor>>();
        std::thread::Builder::new()
            .name("nomi-subagent-process-cleanup".to_owned())
            .spawn(move || {
                let runtime = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .expect("build sub-agent process cleanup runtime");
                runtime.block_on(async move {
                    while let Some(supervisor) = receiver.recv().await {
                        let _ = supervisor.shutdown().await;
                    }
                });
            })
            .expect("start sub-agent process cleanup relay");
        sender
    });
    let _ = relay.send(supervisor);
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum ToolScope {
    Unrestricted,
    Restricted(std::collections::BTreeSet<String>),
}

impl ToolScope {
    fn from_config(allowed: &[String]) -> Self {
        if allowed.is_empty() {
            Self::Unrestricted
        } else {
            Self::Restricted(allowed.iter().cloned().collect())
        }
    }

    fn from_role(allowed: &[String]) -> Self {
        Self::from_config(allowed)
    }

    fn intersect(&self, other: &Self) -> Self {
        match (self, other) {
            (Self::Unrestricted, scope) | (scope, Self::Unrestricted) => scope.clone(),
            (Self::Restricted(left), Self::Restricted(right)) => {
                Self::Restricted(left.intersection(right).cloned().collect())
            }
        }
    }

    fn allows(&self, name: &str) -> bool {
        match self {
            Self::Unrestricted => true,
            Self::Restricted(allowed) => allowed.contains(name),
        }
    }
}

fn build_tool_registry(
    scope: ToolScope,
    cwd: &Path,
    parent_capability: &CapabilityPolicy,
    write_root: Option<&Path>,
    supervisor: Arc<ProcessSupervisor>,
    board: Option<(Arc<crate::taskboard::TaskBoard>, &str)>,
) -> Result<ToolRegistry, String> {
    let cwd = cwd
        .canonicalize()
        .map_err(|error| format!("invalid child cwd {}: {error}", cwd.display()))?;
    let mut capability = parent_capability.clone();
    let mut canonical_roots = Vec::with_capacity(capability.cwd_roots.len());
    for root in &capability.cwd_roots {
        canonical_roots.push(root.canonicalize().map_err(|error| {
            format!(
                "invalid inherited capability root {}: {error}",
                root.display()
            )
        })?);
    }
    if !canonical_roots.iter().any(|root| cwd.starts_with(root)) {
        return Err(format!(
            "child cwd {} is outside inherited capability roots",
            cwd.display()
        ));
    }
    capability.cwd_roots = vec![cwd.clone()];
    let inherited_write_root = match write_root {
        Some(root) => narrow_root_to_child(&cwd, root, "write")?,
        None => cwd.clone(),
    };
    if let nomi_execution::SandboxPolicy::MacSeatbelt { write_roots } =
        &mut capability.sandbox
    {
        let mut narrowed = Vec::with_capacity(write_roots.len());
        for root in write_roots.iter() {
            narrowed.push(narrow_root_to_child(&cwd, root, "Seatbelt write")?);
        }
        *write_roots = narrowed;
    }
    let all_tools: Vec<(&str, Box<dyn nomi_tools::Tool>)> = vec![
        ("Read", Box::new(ReadTool::new(None, Some(cwd.clone())))),
        (
            "Write",
            Box::new(
                WriteTool::new(None)
                    .with_write_root(Some(inherited_write_root.clone()))
                    .with_cwd(Some(cwd.clone())),
            ),
        ),
        (
            "Edit",
            Box::new(
                EditTool::new(None)
                    .with_write_root(Some(inherited_write_root))
                    .with_cwd(Some(cwd.clone())),
            ),
        ),
        (
            "Bash",
            Box::new(BashTool::new(
                supervisor,
                cwd.clone(),
                capability,
            )),
        ),
        ("Grep", Box::new(GrepTool::new(cwd.clone()))),
        ("Glob", Box::new(GlobTool::new(cwd.clone()))),
    ];

    let mut registry = ToolRegistry::new();
    for (name, tool) in all_tools {
        if scope.allows(name) {
            registry.register(tool);
        }
    }
    // Coordinated fan-out: give this sub-agent the shared task board so siblings
    // can claim work and report progress to one another. Always registered when
    // a board is present (not subject to the role tool-allowlist).
    if let Some((board, agent_name)) = board {
        registry.register(Box::new(crate::taskboard::TaskBoardTool::new(board, agent_name)));
    }
    Ok(registry)
}

fn narrow_root_to_child(cwd: &Path, root: &Path, label: &str) -> Result<PathBuf, String> {
    let root = root
        .canonicalize()
        .map_err(|error| format!("invalid inherited {label} root {}: {error}", root.display()))?;
    if cwd.starts_with(&root) {
        Ok(cwd.to_path_buf())
    } else if root.starts_with(cwd) {
        Ok(root)
    } else {
        Err(format!(
            "child cwd {} and inherited {label} root {} do not overlap",
            cwd.display(),
            root.display()
        ))
    }
}

fn translate_root_to_worktree(
    source: &Path,
    worktree: &Path,
    root: &Path,
    label: &str,
) -> Result<PathBuf, String> {
    let root = root
        .canonicalize()
        .map_err(|error| format!("invalid inherited {label} root {}: {error}", root.display()))?;
    if source.starts_with(&root) {
        return Ok(worktree.to_path_buf());
    }
    let relative = root.strip_prefix(source).map_err(|_| {
        format!(
            "source workspace {} and inherited {label} root {} do not overlap",
            source.display(),
            root.display()
        )
    })?;
    let translated = worktree.join(relative);
    if translated.is_dir() {
        Ok(translated)
    } else {
        Err(format!(
            "translated {label} root does not exist in isolated worktree: {}",
            translated.display()
        ))
    }
}

#[cfg(test)]
mod phase7_tests {
    use super::{
        ForkOverrides, SubAgentConfig, ToolScope, build_tool_registry, map_subagent_outcome,
    };
    use crate::engine::{AgentError, AgentResult};
    use async_trait::async_trait;
    use nomi_execution::{CapabilityPolicy, ProcessSupervisor, SandboxPolicy, SupervisorConfig};
    use nomi_config::compat::ProviderCompat;
    use nomi_config::config::{Config, ProviderType};
    use nomi_providers::{LlmProvider, ProviderError};
    use nomi_types::llm::{LlmEvent, LlmRequest};
    use nomi_types::message::{StopReason, TokenUsage};
    use std::collections::BTreeSet;
    use std::sync::Arc;

    fn test_registry(
        scope: ToolScope,
        cwd: &std::path::Path,
        capability: CapabilityPolicy,
    ) -> Result<nomi_tools::registry::ToolRegistry, String> {
        build_tool_registry(
            scope,
            cwd,
            &capability,
            Some(cwd),
            ProcessSupervisor::new(SupervisorConfig::default()),
            None,
        )
    }

    fn test_config() -> Config {
        Config {
            provider_label: "openai".into(),
            provider: ProviderType::OpenAI,
            api_key: "sk-test".into(),
            base_url: "http://localhost:0".into(),
            model: "gpt-test-model".into(),
            max_tokens: 1024,
            max_turns: Some(5),
            system_prompt: None,
            project_instructions: Default::default(),
            thinking: None,
            prompt_caching: false,
            compat: ProviderCompat::openai_defaults(),
            tools: Default::default(),
            session: Default::default(),
            compact: Default::default(),
            plan: Default::default(),
            file_cache: Default::default(),
            hooks: Default::default(),
            bedrock: None,
            vertex: None,
            mcp: Default::default(),
            logging: Default::default(),
        }
    }

    #[tokio::test]
    async fn map_subagent_outcome_timeout_is_error() {
        // Produce a genuine Elapsed by timing out a never-completing future.
        let elapsed: Result<Result<AgentResult, AgentError>, _> =
            tokio::time::timeout(std::time::Duration::from_nanos(1), std::future::pending()).await;
        assert!(elapsed.is_err(), "precondition: should have elapsed");

        let r = map_subagent_outcome("agent-x".to_string(), elapsed, 300);
        assert!(r.is_error, "a timed-out sub-agent must be reported as an error");
        assert_eq!(r.name, "agent-x");
        assert!(r.text.contains("timed out"), "text: {}", r.text);
        assert_eq!(r.turns, 0);
    }

    #[test]
    fn map_subagent_outcome_ok_maps_result() {
        let ok: Result<Result<AgentResult, AgentError>, tokio::time::error::Elapsed> =
            Ok(Ok(AgentResult {
                text: "done".to_string(),
                stop_reason: StopReason::EndTurn,
                usage: TokenUsage::default(),
                turns: 3,
            }));
        let r = map_subagent_outcome("a".to_string(), ok, 300);
        assert!(!r.is_error);
        assert_eq!(r.text, "done");
        assert_eq!(r.turns, 3);
    }

    #[test]
    fn map_subagent_outcome_engine_error_is_error() {
        let err: Result<Result<AgentResult, AgentError>, tokio::time::error::Elapsed> =
            Ok(Err(AgentError::UserAborted));
        let r = map_subagent_outcome("a".to_string(), err, 300);
        assert!(r.is_error);
        assert!(r.text.contains("Sub-agent error"), "text: {}", r.text);
    }

    #[test]
    fn tc_7_1_fork_overrides_default_values() {
        let o = ForkOverrides::default();
        assert!(o.model.is_none());
        assert!(o.effort.is_none());
        assert!(o.allowed_tools.is_empty());
    }

    #[test]
    fn tc_7_40_build_tool_registry_empty_allowed_registers_all() {
        let cwd = std::env::temp_dir().canonicalize().unwrap();
        let registry = test_registry(
            ToolScope::Unrestricted,
            &cwd,
            CapabilityPolicy::local_owner(cwd.clone()),
        )
        .unwrap();
        for name in &["Read", "Write", "Edit", "Bash", "Grep", "Glob"] {
            assert!(
                registry.get(name).is_some(),
                "tool '{name}' should be registered"
            );
        }
    }

    #[test]
    fn tc_7_43_build_tool_registry_filters_to_allowed() {
        let cwd = std::env::temp_dir().canonicalize().unwrap();
        let registry = test_registry(
            ToolScope::Restricted(BTreeSet::from(["Bash".to_string(), "Read".to_string()])),
            &cwd,
            CapabilityPolicy::local_owner(cwd.clone()),
        )
        .unwrap();
        assert!(registry.get("Bash").is_some());
        assert!(registry.get("Read").is_some());
        assert!(registry.get("Write").is_none());
    }

    #[test]
    fn tool_scope_intersection_is_monotonic_and_disjoint_means_deny_all() {
        let read = ToolScope::Restricted(BTreeSet::from(["Read".to_string()]));
        let bash = ToolScope::Restricted(BTreeSet::from(["Bash".to_string()]));
        let unrestricted = ToolScope::Unrestricted;

        assert_eq!(unrestricted.intersect(&read), read);
        assert_eq!(
            read.intersect(&bash),
            ToolScope::Restricted(BTreeSet::new())
        );
    }

    #[test]
    fn disjoint_parent_and_role_scopes_register_no_builtin_tools() {
        let cwd = std::env::temp_dir().canonicalize().unwrap();
        let parent = ToolScope::Restricted(BTreeSet::from(["Read".to_string()]));
        let role = ToolScope::Restricted(BTreeSet::from(["Bash".to_string()]));
        let registry = test_registry(
            parent.intersect(&role),
            &cwd,
            CapabilityPolicy::local_owner(cwd.clone()),
        )
        .unwrap();

        assert!(registry.tool_names().is_empty());
    }

    #[tokio::test]
    async fn inherited_deny_execution_is_not_downgraded() {
        let cwd = std::env::temp_dir().canonicalize().unwrap();
        let capability = CapabilityPolicy {
            cwd_roots: vec![cwd.clone()],
            sandbox: SandboxPolicy::DenyExecution,
            allow_hand_off: false,
        };
        let registry = test_registry(
            ToolScope::Restricted(BTreeSet::from(["Bash".to_string()])),
            &cwd,
            capability,
        )
        .unwrap();
        let bash = registry.get("Bash").expect("Bash should be registered");
        let result = bash
            .execute(serde_json::json!({"command": "echo must_not_run"}))
            .await;

        assert!(result.is_error);
        assert!(result.content.contains("capability_denied"));
    }

    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn child_registry_inherits_seatbelt_and_cannot_write_outside_child_cwd() {
        // `tempfile::tempdir()` lives inside Darwin's trusted user temporary
        // directory, which the Seatbelt profile intentionally keeps writable.
        // Place this fixture beside the checkout so the sibling path actually
        // exercises the child-root restriction.
        let parent = tempfile::tempdir_in(std::env::current_dir().unwrap()).unwrap();
        let child = parent.path().join("child");
        std::fs::create_dir(&child).unwrap();
        let parent_root = parent.path().canonicalize().unwrap();
        let child_root = child.canonicalize().unwrap();
        let outside = parent_root.join("outside-child.marker");
        let registry = build_tool_registry(
            ToolScope::Restricted(BTreeSet::from(["Bash".to_string()])),
            &child_root,
            &CapabilityPolicy {
                cwd_roots: vec![parent_root.clone()],
                sandbox: SandboxPolicy::MacSeatbelt {
                    write_roots: vec![parent_root],
                },
                allow_hand_off: false,
            },
            None,
            ProcessSupervisor::new(SupervisorConfig::default()),
            None,
        )
        .expect("child registry");
        let bash = registry.get("Bash").expect("Bash");

        let inside = child_root.join("inside-child.marker");
        let allowed = bash
            .execute(serde_json::json!({
                "command": format!("printf allowed > '{}'", inside.display())
            }))
            .await;
        assert!(!allowed.is_error, "{}", allowed.content);
        assert!(inside.exists());

        let denied = bash
            .execute(serde_json::json!({
                "command": format!("printf denied > '{}'", outside.display())
            }))
            .await;
        assert!(denied.is_error, "{}", denied.content);
        assert!(!outside.exists());
    }

    #[test]
    fn child_cwd_outside_inherited_roots_is_rejected() {
        let parent = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let capability = CapabilityPolicy::local_owner(
            parent.path().canonicalize().unwrap(),
        );
        let result = test_registry(
            ToolScope::Unrestricted,
            outside.path(),
            capability,
        );

        assert!(result.is_err());
    }

    #[test]
    fn isolated_worktree_translates_authority_to_the_exact_verified_worktree() {
        use super::AgentSpawner;

        struct NeverCalledProvider;

        #[async_trait]
        impl LlmProvider for NeverCalledProvider {
            async fn stream(
                &self,
                _request: &LlmRequest,
            ) -> Result<tokio::sync::mpsc::Receiver<LlmEvent>, ProviderError> {
                panic!("isolated capability validation must happen before provider use")
            }
        }

        fn git(args: &[&str], cwd: &std::path::Path) {
            let output = std::process::Command::new("git")
                .arg("-C")
                .arg(cwd)
                .args(args)
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "git {args:?} failed: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }

        let parent = tempfile::tempdir().unwrap();
        git(&["init", "-q"], parent.path());
        git(&["config", "user.email", "t@t"], parent.path());
        git(&["config", "user.name", "t"], parent.path());
        std::fs::write(parent.path().join("tracked.txt"), "tracked\n").unwrap();
        let nested = parent.path().join("packages").join("a");
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::write(nested.join("nested.txt"), "nested\n").unwrap();
        git(&["add", "-A"], parent.path());
        git(&["commit", "-q", "-m", "init"], parent.path());
        let nested_root = nested.canonicalize().unwrap();
        let spawner = AgentSpawner::new(
            Arc::new(NeverCalledProvider),
            test_config(),
            nested_root.clone(),
        )
        .with_execution_policy(
            CapabilityPolicy::local_owner(nested_root.clone()),
            Some(nested_root),
            Vec::new(),
        );
        let worktree =
            nomi_tools::worktree::Worktree::create(&nested).expect("create worktree");
        let expected = worktree.verified_cwd(&nested).expect("verified worktree cwd");

        let isolated = spawner
            .clone_for_isolated_worktree(&worktree)
            .expect("trusted Worktree handle may delegate exact authority");
        assert_eq!(isolated.cwd, expected);
        assert_eq!(isolated.execution_capability.cwd_roots, vec![expected.clone()]);
        assert_eq!(isolated.write_root, Some(expected));
        assert_ne!(
            isolated.cwd,
            worktree.verified_path().expect("worktree root"),
            "nested parent cwd must not delegate the whole repository worktree"
        );
    }

    #[test]
    fn child_cwd_with_disjoint_inherited_write_root_is_rejected() {
        let capability_root = tempfile::tempdir().unwrap();
        let child = capability_root.path().join("child");
        let sibling_write_root = capability_root.path().join("write-only-sibling");
        std::fs::create_dir(&child).unwrap();
        std::fs::create_dir(&sibling_write_root).unwrap();
        let capability =
            CapabilityPolicy::local_owner(capability_root.path().canonicalize().unwrap());
        let result = build_tool_registry(
            ToolScope::Unrestricted,
            &child,
            &capability,
            Some(&sibling_write_root),
            ProcessSupervisor::new(SupervisorConfig::default()),
            None,
        );

        assert!(result.is_err());
    }

    #[tokio::test]
    async fn ordinary_child_without_parent_write_root_is_still_scoped_to_its_cwd() {
        let parent = tempfile::tempdir().unwrap();
        let child = parent.path().join("child");
        std::fs::create_dir(&child).unwrap();
        let capability =
            CapabilityPolicy::local_owner(parent.path().canonicalize().unwrap());
        let registry = build_tool_registry(
            ToolScope::Restricted(BTreeSet::from(["Write".to_string()])),
            &child,
            &capability,
            None,
            ProcessSupervisor::new(SupervisorConfig::default()),
            None,
        )
        .expect("child registry");
        let outside = parent.path().join("outside.txt");

        let result = registry
            .get("Write")
            .expect("Write")
            .execute(serde_json::json!({
                "file_path": outside.to_string_lossy(),
                "content": "must not escape"
            }))
            .await;

        assert!(result.is_error, "{}", result.content);
        assert!(!outside.exists());
    }

    #[test]
    fn tc_7_sub_agent_config_original_fields_intact() {
        let config = SubAgentConfig {
            name: "test-agent".to_string(),
            prompt: "do the task".to_string(),
            max_turns: 5,
            max_tokens: 1024,
            system_prompt: Some("you are helpful".to_string()),
            allowed_tools: Vec::new(),
        };
        assert_eq!(config.name, "test-agent");
        assert_eq!(config.max_turns, 5);
    }

    #[tokio::test]
    async fn run_bounded_caps_concurrency_and_returns_all() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicUsize, Ordering::SeqCst};
        use std::time::Duration;
        use tokio::sync::Semaphore;

        let cap = 3;
        let n = 12;
        let peak = Arc::new(AtomicUsize::new(0));
        let inflight = Arc::new(AtomicUsize::new(0));
        let sem = Arc::new(Semaphore::new(cap));

        let tasks: Vec<_> = (0..n)
            .map(|i| {
                let peak = peak.clone();
                let inflight = inflight.clone();
                async move {
                    let now = inflight.fetch_add(1, SeqCst) + 1;
                    peak.fetch_max(now, SeqCst);
                    tokio::time::sleep(Duration::from_millis(40)).await;
                    inflight.fetch_sub(1, SeqCst);
                    i
                }
            })
            .collect();

        let results = super::run_bounded(sem, tasks).await;
        assert_eq!(results.len(), n, "every task must produce a result");
        let mut values: Vec<usize> = results.into_iter().map(|r| r.expect("no join error")).collect();
        values.sort_unstable();
        assert_eq!(values, (0..n).collect::<Vec<_>>(), "results preserve all tasks in order");
        assert!(
            peak.load(SeqCst) <= cap,
            "peak concurrency {} must not exceed the cap {cap}",
            peak.load(SeqCst)
        );
        assert!(peak.load(SeqCst) >= 2, "tasks should have run concurrently up to the cap");
    }

    #[tokio::test]
    async fn run_bounded_aborts_in_flight_tasks_when_dropped() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicUsize, Ordering::SeqCst};
        use std::time::Duration;
        use tokio::sync::Semaphore;

        let completed = Arc::new(AtomicUsize::new(0));
        let sem = Arc::new(Semaphore::new(8));
        let tasks: Vec<_> = (0..8)
            .map(|_| {
                let completed = completed.clone();
                async move {
                    tokio::time::sleep(Duration::from_millis(500)).await;
                    completed.fetch_add(1, SeqCst);
                }
            })
            .collect();

        // Start the fan-out, then drop its future after 50ms (simulating the
        // parent turn being cancelled mid-flight).
        {
            let fut = super::run_bounded(sem, tasks);
            tokio::select! {
                _ = fut => {}
                _ = tokio::time::sleep(Duration::from_millis(50)) => {}
            }
        } // `fut` dropped here → JoinSet drops → tasks aborted

        // Give any *non-aborted* task time to reach its increment.
        tokio::time::sleep(Duration::from_millis(700)).await;
        assert_eq!(
            completed.load(SeqCst),
            0,
            "dropping run_bounded must abort in-flight sub-agent tasks (no detached compute)"
        );
    }

    #[test]
    fn token_budget_blocks_once_exhausted() {
        use super::TokenBudget;
        let b = TokenBudget::new(100);
        assert!(b.can_begin());
        b.record(60);
        assert_eq!(b.remaining(), 40);
        assert!(b.can_begin(), "40 left → another sub-agent may start");
        b.record(60); // saturating past zero
        assert_eq!(b.remaining(), 0);
        assert!(!b.can_begin(), "exhausted budget blocks further sub-agents");
    }
}

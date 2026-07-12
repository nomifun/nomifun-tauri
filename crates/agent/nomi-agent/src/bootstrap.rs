use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use nomi_config::config::Config;
use nomi_mcp::manager::McpManager;
use nomi_providers::LlmProvider;

use crate::engine::AgentEngine;
use crate::output::OutputSink;
use crate::session::Session;

/// **extract-llm: session-model adapter for `BrowserTool`'s extract seam.**
///
/// Wraps the session's [`LlmProvider`] + model params so `act(Extract)` can do real
/// LLM-driven structured extraction. The browser engine itself stays LLM-free — this
/// adapter lives at the bootstrap/facade layer (架构铁律). `complete(prompt)` issues a
/// minimal one-shot request — the (already spotlighted, untrusted-data-wrapped) extract
/// prompt as a single user message, no tools, no system prompt, no extended thinking
/// (extraction is mechanical) — and collects the streamed text deltas into the full
/// completion. Reuses the session's own model (user decision: "extract 用会话模型"), so
/// there is no separate model/cost surface. `None` provider → seam stays unwired (the
/// facade returns its deterministic `<data>` payload = zero-regression graceful degrade).
#[cfg(feature = "browser-use")]
struct SessionExtractModel {
    provider: Arc<dyn LlmProvider>,
    model: String,
    max_tokens: u32,
}

#[cfg(feature = "browser-use")]
#[async_trait::async_trait]
impl nomi_browser::extract::ExtractModel for SessionExtractModel {
    async fn complete(&self, prompt: &str) -> Result<String, String> {
        use nomi_types::llm::{LlmEvent, LlmRequest};
        use nomi_types::message::{ContentBlock, Message, Role};

        let request = LlmRequest {
            model: self.model.clone(),
            system: String::new(),
            messages: vec![Message::new(
                Role::User,
                vec![ContentBlock::Text { text: prompt.to_string() }],
            )],
            tools: Vec::new(),
            max_tokens: self.max_tokens,
            thinking: None,
            reasoning_effort: None,
        };
        let mut rx = self
            .provider
            .stream(&request)
            .await
            .map_err(|e| format!("extract model stream failed: {e}"))?;
        let mut out = String::new();
        while let Some(event) = rx.recv().await {
            match event {
                LlmEvent::TextDelta(t) => out.push_str(&t),
                LlmEvent::Done { .. } => break,
                LlmEvent::Error(e) => return Err(format!("extract model error: {e}")),
                // No tools / no thinking requested → other events aren't expected; ignore.
                _ => {}
            }
        }
        Ok(out)
    }
}

/// **P7B: session-model adapter for `BrowserTool`'s visual-fallback locator seam.**
///
/// Wraps the session's [`LlmProvider`] + model params so the facade can do vision-based
/// element location when DOM/aria anchoring fails (a `ref` went stale/detached). The
/// browser engine stays LLM-free — like [`SessionExtractModel`], this adapter lives at the
/// bootstrap/facade layer (架构铁律). `locate` sends one multimodal user message — a strict
/// "return JSON bounding box" instruction plus the (already engine-redacted) screenshot as a
/// [`ContentBlock::Image`] — and parses the model's pixel-box reply. No system prompt, no
/// tools, no extended thinking. Reuses the session's own model (same decision as Extract: no
/// separate model/cost surface). Gated behind `agent.browserUse.visualFallback` (host_default
/// OFF) because every fallback round-trips the vision model — a real token cost.
#[cfg(feature = "browser-use")]
struct SessionVisualLocator {
    provider: Arc<dyn LlmProvider>,
    model: String,
    max_tokens: u32,
}

#[cfg(feature = "browser-use")]
impl SessionVisualLocator {
    /// Shared multimodal round-trip: one user message (`prompt` text + a base64 PNG) → stream →
    /// collected text. No system prompt, no tools, no thinking (vision locating is mechanical).
    /// Both [`VisualLocator::locate`] (bbox) and [`VisualLocator::locate_labeled`] (SoM label)
    /// share this; only the prompt + reply parser differ.
    async fn run_vision(&self, prompt: String, png: &[u8]) -> Result<String, String> {
        use base64::Engine as _;
        use nomi_types::llm::{LlmEvent, LlmRequest};
        use nomi_types::message::{ContentBlock, Message, Role};

        let data = base64::engine::general_purpose::STANDARD.encode(png);
        let request = LlmRequest {
            model: self.model.clone(),
            system: String::new(),
            messages: vec![Message::new(
                Role::User,
                vec![
                    ContentBlock::Text { text: prompt },
                    ContentBlock::Image { media_type: "image/png".to_string(), data },
                ],
            )],
            tools: Vec::new(),
            max_tokens: self.max_tokens,
            thinking: None,
            reasoning_effort: None,
        };

        let mut rx = self
            .provider
            .stream(&request)
            .await
            .map_err(|e| format!("visual locator stream failed: {e}"))?;
        let mut out = String::new();
        while let Some(event) = rx.recv().await {
            match event {
                LlmEvent::TextDelta(t) => out.push_str(&t),
                LlmEvent::Done { .. } => break,
                LlmEvent::Error(e) => return Err(format!("visual locator error: {e}")),
                // No tools / no thinking requested → other events aren't expected; ignore.
                _ => {}
            }
        }
        Ok(out)
    }
}

#[cfg(feature = "browser-use")]
#[async_trait::async_trait]
impl nomi_browser::visual_fallback::VisualLocator for SessionVisualLocator {
    async fn locate(
        &self,
        screenshot: &[u8],
        instruction: &str,
    ) -> Result<nomi_browser::visual_fallback::VisualLocateResult, String> {
        // Strict-JSON instruction. The screenshot is at device-pixel resolution; the model
        // returns the box in that same image-pixel space (the facade divides by the live DPR
        // before dispatching the click). A not-found answer is `{"confidence": 0}`.
        let prompt = format!(
            "You are a precise UI element locator. The attached PNG is a screenshot of a web \
             page rendered at device-pixel resolution; the origin (0,0) is its top-left corner. \
             Find the single element described below and return its bounding box in IMAGE PIXEL \
             coordinates.\n\n\
             Target element: {instruction}\n\n\
             Respond with ONLY a JSON object — no markdown, no code fence, no prose:\n\
             {{\"x\": <left px>, \"y\": <top px>, \"width\": <px>, \"height\": <px>, \"confidence\": <0..1>}}\n\
             If you cannot confidently find the element, respond with {{\"confidence\": 0}}."
        );
        let out = self.run_vision(prompt, screenshot).await?;
        parse_visual_locate_result(&out)
    }

    async fn locate_labeled(
        &self,
        annotated_screenshot: &[u8],
        instruction: &str,
        n_labels: usize,
    ) -> Result<nomi_browser::visual_fallback::SomLabelResult, String> {
        // SoM mode: the screenshot has numbered labels (1..=n_labels) drawn on its clickable
        // elements. The model picks the label that matches the target — a finite choice, far
        // more reliable than free-form pixel regression. A not-found answer is `{"label": 0}`.
        let prompt = format!(
            "You are a precise UI element selector. The attached PNG is a screenshot of a web \
             page with numbered labels 1 to {n_labels} drawn on its interactive elements (each \
             label sits at the top-left of its element's box). Identify which single label \
             number marks the element described below.\n\n\
             Target element: {instruction}\n\n\
             Respond with ONLY a JSON object — no markdown, no code fence, no prose:\n\
             {{\"label\": <integer 1..{n_labels}>, \"confidence\": <0..1>}}\n\
             If none of the labeled elements matches, respond with {{\"label\": 0, \"confidence\": 0}}."
        );
        let out = self.run_vision(prompt, annotated_screenshot).await?;
        parse_som_locate_result(&out, n_labels)
    }
}

/// Parse the vision model's reply into a [`VisualLocateResult`](nomi_browser::visual_fallback::VisualLocateResult).
///
/// Tolerates models that wrap the JSON in prose or a ```` ```json ```` fence by extracting the
/// outermost `{...}` span. A reply with `confidence == 0`, a missing/zero box, or any
/// non-numeric field is treated as "element not found" (an `Err`) — the facade then surfaces
/// the original anchor error rather than clicking a bogus coordinate.
#[cfg(feature = "browser-use")]
fn parse_visual_locate_result(
    raw: &str,
) -> Result<nomi_browser::visual_fallback::VisualLocateResult, String> {
    use nomi_browser::visual_fallback::{PixelBox, VisualLocateResult};

    let trimmed = raw.trim();
    let start = trimmed
        .find('{')
        .ok_or_else(|| format!("visual locator returned no JSON object: {trimmed:.200}"))?;
    let end = trimmed
        .rfind('}')
        .filter(|e| *e >= start)
        .ok_or_else(|| format!("visual locator JSON span malformed: {trimmed:.200}"))?;
    let json_str = &trimmed[start..=end];
    let v: serde_json::Value = serde_json::from_str(json_str)
        .map_err(|e| format!("visual locator JSON parse failed ({e}): {json_str:.200}"))?;

    let confidence = v.get("confidence").and_then(serde_json::Value::as_f64).unwrap_or(0.0);
    let num = |k: &str| v.get(k).and_then(serde_json::Value::as_f64);
    match (num("x"), num("y"), num("width"), num("height")) {
        (Some(x), Some(y), Some(width), Some(height))
            if confidence > 0.0 && width > 0.0 && height > 0.0 =>
        {
            Ok(VisualLocateResult {
                pixel_box: PixelBox { x, y, width, height },
                confidence: confidence.clamp(0.0, 1.0),
            })
        }
        _ => Err(format!(
            "visual locator could not locate the element (confidence={confidence})"
        )),
    }
}

/// Parse the vision model's SoM reply into a [`SomLabelResult`](nomi_browser::visual_fallback::SomLabelResult).
///
/// Same JSON-extraction tolerance as [`parse_visual_locate_result`] (handles prose / ```` ```json ````
/// fences). The label MUST be an integer in `1..=n_labels` with `confidence > 0`; `label == 0`,
/// out-of-range, missing, or zero-confidence all map to `Err` ("no label matched") so the facade
/// surfaces the original anchor error rather than indexing a bogus / out-of-bounds label.
#[cfg(feature = "browser-use")]
fn parse_som_locate_result(
    raw: &str,
    n_labels: usize,
) -> Result<nomi_browser::visual_fallback::SomLabelResult, String> {
    use nomi_browser::visual_fallback::SomLabelResult;

    let trimmed = raw.trim();
    let start = trimmed
        .find('{')
        .ok_or_else(|| format!("SoM locator returned no JSON object: {trimmed:.200}"))?;
    let end = trimmed
        .rfind('}')
        .filter(|e| *e >= start)
        .ok_or_else(|| format!("SoM locator JSON span malformed: {trimmed:.200}"))?;
    let json_str = &trimmed[start..=end];
    let v: serde_json::Value = serde_json::from_str(json_str)
        .map_err(|e| format!("SoM locator JSON parse failed ({e}): {json_str:.200}"))?;

    let confidence = v.get("confidence").and_then(serde_json::Value::as_f64).unwrap_or(0.0);
    // Accept integer or float-encoded label; reject anything not in 1..=n_labels.
    let label = v.get("label").and_then(serde_json::Value::as_f64);
    match label {
        Some(l) if l.fract() == 0.0 && l >= 1.0 && (l as usize) <= n_labels && confidence > 0.0 => {
            Ok(SomLabelResult {
                label: l as usize,
                confidence: confidence.clamp(0.0, 1.0),
            })
        }
        _ => Err(format!(
            "SoM locator picked no valid label (label={label:?}, n_labels={n_labels}, confidence={confidence})"
        )),
    }
}

/// Result of bootstrapping an agent engine with all features initialized.
pub struct BootstrapResult {
    pub engine: AgentEngine,
    pub provider: Arc<dyn LlmProvider>,
    pub mcp_managers: Vec<Arc<McpManager>>,
    pub has_mcp: bool,
}

/// Builder for creating a fully-initialized `AgentEngine`.
///
/// Encapsulates the complete initialization pipeline so all consumers
/// (CLI, backend, sub-agents) get consistent behavior:
///
/// - System prompt always includes model identity, working directory, date
/// - Tool usage guidance is always injected
/// - AGENTS.md is loaded from the workspace hierarchy
/// - Skills, MCP, plan mode, spawn are enabled based on `Config` fields
pub struct AgentBootstrap {
    config: Config,
    workspace: String,
    output: Arc<dyn OutputSink>,
    provider: Option<Arc<dyn LlmProvider>>,
    resume_session: Option<Session>,
    extra_skill_dirs: Vec<PathBuf>,
    goal: Option<crate::goal::runtime::GoalSpec>,
    /// **P3-X1: the session's shared runtime approval-mode handle** (the same
    /// `Arc<ToolApprovalManager>` the host later installs on the engine via
    /// `set_approval_manager`). When present it is threaded into the native
    /// `BrowserTool` so its fail-closed redline gate reads the *runtime* session mode
    /// LIVE — a mid-session `set_mode` to yolo arms the gate immediately, instead of
    /// being pinned to the construction-time `auto_approve` snapshot. Hosts that have
    /// no protocol approval manager (e.g. the interactive REPL) leave it `None` and the
    /// facade falls back to the construction-time snapshot (unchanged behavior).
    approval_manager: Option<Arc<nomi_protocol::ToolApprovalManager>>,
    /// **P3-X2: the session's per-pet browser secret vault source** (vault file path +
    /// machine-bound 32-byte key). Threaded into the native `BrowserTool` so it can lazily
    /// load the registered credentials (`secret:NAME` resolves, origin-gated) and derive the
    /// firewall domain allowlist from the same per-pet `allowed_origins` (裁决⑤). Stored as
    /// the raw pieces (NOT the `nomi_browser` type) so the field exists regardless of the
    /// `browser-use` feature; the `BrowserSecretSource` is constructed only at the
    /// feature-gated `with_policy` call site. `None` → empty store + unrestricted egress.
    #[cfg_attr(not(feature = "browser-use"), allow(dead_code))]
    browser_secret_source: Option<(PathBuf, [u8; 32])>,
    /// **Phase D: the session's browser approval gate** (human takeover + SD-5 cross-origin
    /// egress). Threaded into the native `BrowserTool` so an irreversible action in a bypass
    /// session — and a gated cross-origin POST — is surfaced to the user and awaited. `None`
    /// (default) → fail-closed (current behavior). Feature-gated: the trait is in `nomi_browser`.
    #[cfg(feature = "browser-use")]
    approval_gate: Option<Arc<dyn nomi_browser::BrowserApprovalGate>>,
}

impl AgentBootstrap {
    pub fn new(config: Config, workspace: impl Into<String>, output: Arc<dyn OutputSink>) -> Self {
        Self {
            config,
            workspace: workspace.into(),
            output,
            provider: None,
            resume_session: None,
            extra_skill_dirs: Vec::new(),
            goal: None,
            approval_manager: None,
            browser_secret_source: None,
            #[cfg(feature = "browser-use")]
            approval_gate: None,
        }
    }

    /// Use a pre-created provider instead of creating one from config.
    pub fn provider(mut self, provider: Arc<dyn LlmProvider>) -> Self {
        self.provider = Some(provider);
        self
    }

    /// Enable goal-driven continuation for this session (opt-in). Omit it (the
    /// default) and the engine behaves exactly as before.
    pub fn goal(mut self, goal: Option<crate::goal::runtime::GoalSpec>) -> Self {
        self.goal = goal;
        self
    }

    /// **P3-X1: provide the session's shared `Arc<ToolApprovalManager>`** so the native
    /// `BrowserTool`'s redline gate reads the *runtime* approval mode LIVE (a mid-session
    /// `set_mode` to yolo arms the gate immediately). Pass the *same* Arc that is later
    /// installed on the engine via `set_approval_manager`, so the facade and orchestration
    /// observe one mode cell with zero drift. Omit it (the default) to keep the
    /// construction-time `auto_approve` snapshot as the (fail-closed) source of truth.
    pub fn approval_manager(mut self, mgr: Arc<nomi_protocol::ToolApprovalManager>) -> Self {
        self.approval_manager = Some(mgr);
        self
    }

    /// **P3-X2: provide the session's per-pet browser secret vault source** so the native
    /// `BrowserTool` can load the user-registered credentials (`secret:NAME`, origin-gated)
    /// and derive the firewall domain allowlist from the same per-pet `allowed_origins`
    /// (裁决⑤). Takes the raw pieces (vault file path + machine-bound 32-byte key) so backend
    /// callers (`nomifun-ai-agent`) need not depend on `nomi-browser` to wire it. Omit it
    /// (the default) to keep an empty store + unrestricted egress (current behavior).
    pub fn browser_secret_source(mut self, vault_path: PathBuf, key: [u8; 32]) -> Self {
        self.browser_secret_source = Some((vault_path, key));
        self
    }

    /// **Phase D: provide the browser approval gate** (host impl raises a `Confirmation`
    /// and awaits the shared `ToolApprovalManager`). Threaded into the native `BrowserTool`,
    /// enabling human takeover of irreversible actions + SD-5 cross-origin egress approval.
    /// Omit it (the default) → fail-closed (irreversible stays Blocked, gated egress fails).
    #[cfg(feature = "browser-use")]
    pub fn approval_gate(mut self, gate: Arc<dyn nomi_browser::BrowserApprovalGate>) -> Self {
        self.approval_gate = Some(gate);
        self
    }

    /// Resume from a previously saved session.
    pub fn resume(mut self, session: Session) -> Self {
        self.resume_session = Some(session);
        self
    }

    /// Add extra directories to scan for skills.
    pub fn extra_skill_dirs(mut self, dirs: Vec<PathBuf>) -> Self {
        self.extra_skill_dirs = dirs;
        self
    }

    /// Read-only access to the config (for session management before build).
    pub fn config(&self) -> &Config {
        &self.config
    }

    /// Build the fully-initialized engine.
    pub async fn build(mut self) -> anyhow::Result<BootstrapResult> {
        let cwd = &self.workspace;
        let cwd_path = std::path::Path::new(cwd);

        tracing::info!(target: "nomi_agent", workspace = %cwd, "agent bootstrap: workspace cwd resolved");

        let provider = self
            .provider
            .unwrap_or_else(|| nomi_providers::create_provider(&self.config));

        let memory_dir = nomi_memory::paths::auto_memory_dir(cwd_path);

        let file_cache = if self.config.file_cache.enabled {
            Some(Arc::new(std::sync::RwLock::new(
                nomi_tools::file_cache::FileStateCache::new(&self.config.file_cache),
            )))
        } else {
            None
        };

        let mut registry = nomi_tools::registry::ToolRegistry::new();
        // Opt-in write-root containment (§3.6): when `tools.write_root` is set,
        // resolve it to an absolute path the write tools enforce. Empty = off.
        let write_root: Option<std::path::PathBuf> = {
            let wr = self.config.tools.write_root.trim();
            if wr.is_empty() {
                None
            } else {
                Some(std::path::PathBuf::from(wr))
            }
        };
        registry.register(Box::new(nomi_tools::read::ReadTool::new(
            file_cache.clone(),
            Some(cwd_path.to_path_buf()),
        )));
        registry.register(Box::new(
            nomi_tools::write::WriteTool::new(file_cache.clone())
                .with_write_root(write_root.clone())
                .with_cwd(Some(cwd_path.to_path_buf())),
        ));
        registry.register(Box::new(
            nomi_tools::edit::EditTool::new(file_cache.clone())
                .with_write_root(write_root.clone())
                .with_cwd(Some(cwd_path.to_path_buf())),
        ));
        registry.register(Box::new(
            nomi_tools::apply_patch::ApplyPatchTool::new(file_cache)
                .with_write_root(write_root.clone())
                .with_cwd(Some(cwd_path.to_path_buf())),
        ));
        // Experimental `Lsp` code-navigation tool: registered only when at least
        // one language server is configured (default off → no behaviour change).
        {
            let mut lsp_map: std::collections::HashMap<String, Vec<String>> =
                std::collections::HashMap::new();
            for entry in &self.config.tools.lsp_servers {
                if entry.command.is_empty() {
                    continue;
                }
                for ext in &entry.extensions {
                    lsp_map.insert(ext.trim_start_matches('.').to_ascii_lowercase(), entry.command.clone());
                }
            }
            if !lsp_map.is_empty() {
                registry.register(Box::new(nomi_tools::lsp::LspTool::new(
                    lsp_map,
                    cwd_path.to_path_buf(),
                )));
            }
        }
        // Native `remember` tool: persist durable project/user memories mid-session
        // to the file-based long-term memory (injected into future sessions).
        if let Some(mem_dir) = memory_dir.clone() {
            registry.register(Box::new(crate::memory_tools::RememberTool::new(mem_dir)));
        }
        let process_supervisor =
            nomi_execution::ProcessSupervisor::new(nomi_execution::SupervisorConfig::default());
        let execution_capability = nomi_execution::CapabilityPolicy {
            cwd_roots: vec![cwd_path.to_path_buf()],
            sandbox: if self.config.tools.bash_sandbox {
                nomi_execution::SandboxPolicy::MacSeatbelt {
                    write_roots: vec![cwd_path.to_path_buf()],
                }
            } else {
                nomi_execution::SandboxPolicy::UnrestrictedLocalOwner
            },
            allow_hand_off: false,
        };
        if self.config.tools.persistent_shell {
            tracing::warn!(
                target: "nomi_agent",
                "tools.persistent_shell is ignored; Bash now always uses supervised one-shot execution"
            );
        }
        registry.register(Box::new(nomi_tools::bash::BashTool::new(
            Arc::clone(&process_supervisor),
            cwd_path.to_path_buf(),
            execution_capability.clone(),
        )));
        registry.register(Box::new(nomi_tools::grep::GrepTool::new(
            cwd_path.to_path_buf(),
        )));
        registry.register(Box::new(nomi_tools::glob::GlobTool::new(
            cwd_path.to_path_buf(),
        )));

        // Legacy interactive schemas share the same supervisor as Bash. The
        // ProcessStore is only a numeric-id adapter; it owns no OS process.
        // Register these before the builtin-name snapshot so an MCP tool with
        // the same name is namespaced instead of shadowing the native tool.
        let process_store = Arc::new(nomi_tools::process_store::ProcessStore::new());
        registry.register(Box::new(nomi_tools::exec_command::ExecCommandTool::new(
            Arc::clone(&process_supervisor),
            Arc::clone(&process_store),
            cwd_path.to_path_buf(),
            execution_capability.clone(),
        )));
        registry.register(Box::new(nomi_tools::write_stdin::WriteStdinTool::new(
            Arc::clone(&process_supervisor),
            Arc::clone(&process_store),
        )));

        let builtin_names: Vec<String> = registry.tool_names();

        let mut mcp_managers: Vec<Arc<McpManager>> = Vec::new();
        let mcp_manager = if !self.config.mcp.servers.is_empty() {
            match McpManager::connect_all(&self.config.mcp.servers).await {
                Ok(mgr) => {
                    let mgr = Arc::new(mgr);
                    nomi_mcp::tool_proxy::register_mcp_tools(
                        &mut registry,
                        &mgr,
                        &builtin_names,
                        &self.config.mcp.servers,
                    );
                    mcp_managers.push(mgr.clone());
                    Some(mgr)
                }
                Err(e) => {
                    self.output
                        .emit_warning(&format!("MCP initialization error: {e}"));
                    None
                }
            }
        } else {
            None
        };
        let has_mcp = mcp_manager.is_some();

        let skills = nomi_skills::loader::load_all_skills(
            cwd_path,
            &self.extra_skill_dirs,
            false,
            mcp_manager.as_deref(),
        )
        .await;

        let agents_snapshot = crate::agents_md::resolve_agents_md(
            cwd_path,
            &self.config.project_instructions,
        );
        for file in &agents_snapshot.files {
            tracing::debug!(
                target: "nomi_agent",
                path = %file.path.display(),
                scope = if file.is_global { "user" } else { "project" },
                "agent bootstrap: loaded instruction file"
            );
        }
        for diagnostic in &agents_snapshot.diagnostics {
            tracing::warn!(
                target: "nomi_agent",
                message = %diagnostic.message(),
                "agent bootstrap: instruction diagnostic"
            );
        }

        let mut prompt_cache = crate::context::SystemPromptCache::new();
        prompt_cache.set_agents_md(agents_snapshot.formatted);
        let system_prompt = crate::context::build_system_prompt(
            &mut prompt_cache,
            self.config.system_prompt.as_deref(),
            cwd,
            &self.config.model,
            &skills,
            None,
            memory_dir.as_deref(),
            false,
            self.config.compact.toon,
            self.config.tools.browser.enabled,
        );
        self.config.system_prompt = Some(system_prompt);

        let skills_arc = Arc::new(skills);
        let skill_checker = nomi_skills::permissions::SkillPermissionChecker::new(
            self.config.tools.skills.deny.clone(),
            self.config.tools.skills.allow.clone(),
            self.config.tools.auto_approve,
        );
        registry.register(Box::new(crate::skill_tool::SkillTool::new(
            skills_arc,
            cwd.to_string(),
            skill_checker,
        )));

        // 进程内 Spawn 门控（默认开）：桌面后端会话由工厂置 false —— 子 agent
        // 改走可视化的 nomi_spawn 编排扇出（DAG 画布/转录）；CLI/独立模式保持
        // 进程内 Spawn（其唯一扇出手段）。
        if self.config.tools.in_process_spawn {
            let spawner = Arc::new(
                crate::spawner::AgentSpawner::new(
                    provider.clone(),
                    self.config.clone(),
                    cwd_path.to_path_buf(),
                )
                .with_execution_policy(
                    execution_capability.clone(),
                    write_root.clone(),
                    self.config.tools.builtin_allowlist.clone(),
                )
                .with_token_budget(
                    self.config
                        .tools
                        .subagent_token_budget
                        .map(|limit| Arc::new(crate::spawner::TokenBudget::new(limit))),
                ),
            );
            registry.register(Box::new(crate::spawn_tool::SpawnTool::new(spawner)));
        }

        let plan_active_flag = Arc::new(AtomicBool::new(false));
        if self.config.plan.enabled {
            registry.register(Box::new(crate::plan::tools::EnterPlanModeTool::new(
                Arc::clone(&plan_active_flag),
            )));
            registry.register(Box::new(crate::plan::tools::ExitPlanModeTool::new(
                Arc::clone(&plan_active_flag),
            )));
        }

        #[cfg(feature = "computer-use")]
        if self.config.tools.computer.enabled {
            tracing::info!(
                target: "nomi_agent",
                "computer-use ENABLED: registering the Computer tool (observe / click_element / \
                 launch / type / scroll). Desktop control is available to this session."
            );
            registry.register(Box::new(nomi_computer::ComputerTool::new(
                &self.config.tools.computer,
            )));
        }
        #[cfg(feature = "computer-use")]
        if !self.config.tools.computer.enabled {
            tracing::info!(
                target: "nomi_agent",
                "computer-use DISABLED for this session (config.tools.computer.enabled = false); \
                 the Computer tool is NOT registered — the agent falls back to the shell."
            );
        }
        #[cfg(not(feature = "computer-use"))]
        if self.config.tools.computer.enabled {
            tracing::warn!(
                target: "nomi_agent",
                "computer use enabled in config but this build lacks the computer-use feature"
            );
        }

        // Native browser-use (in-process self-hosted CDP engine). The native
        // BrowserTool registers under the name "Browser" (actions: navigate /
        // observe / screenshot / capabilities) and is the sole browser path. The
        // engine launches lazily on the first action — registering it never starts
        // a browser.
        #[cfg(feature = "browser-use")]
        if self.config.tools.browser.enabled {
            tracing::info!(
                target: "nomi_agent",
                "browser-use ENABLED: registering the native Browser tool (navigate / observe / \
                 screenshot / capabilities). The managed Chromium launches lazily on first use."
            );
            // F1-sec: thread the session-bypass policy + evaluate full-power into the
            // facade so its independent fail-closed redline gate (裁决⑧) actually fires
            // and the evaluate gate (裁决⑨) reflects the user's opt-in. `auto_approve`
            // is `true` iff orchestration approval is bypassed (yolo / companion-forced-yolo
            // / --auto-approve — see BrowserTool::session_bypasses_approval doc); the
            // `browser.full_power` flag carries the LIVE `agent.browserUse.fullPower` pref
            // (set by the backend factory per session). Constructed here where the full
            // Config is in scope.
            //
            // P3-G2: pass the session working directory `cwd` (= self.workspace) as the
            // per-session/per-pet workspace. It's the natural isolation point — for a
            // companion session it is `{companion_id}/workspace` (companion.rs sets
            // extra.workspace, which the manager resolves to this cwd); for a non-companion
            // session it's the conversation's own working dir. Downloads (E4) land in its
            // `downloads/` subdir instead of a temp dir. The non-companion
            // `{data_dir}/browser-profiles/{conversation_id}` subdivision (默认④) needs the
            // data_dir + conversation_id which the bootstrap does not hold (they live in the
            // upper manager/factory) — the cwd is already per-conversation isolated, so we
            // pass it directly (simplest correct) and leave the finer browser-profiles
            // layout to W4/deployment wiring (the field signature already supports it).
            // P3-X1: thread the session's shared runtime approval-mode handle (the same
            // Arc<ToolApprovalManager> the host installs via set_approval_manager) so the
            // facade's redline gate reads the LIVE session mode — a mid-session set_mode to
            // yolo arms it immediately, instead of being pinned to the auto_approve snapshot
            // above. `None` (e.g. the interactive REPL, which has no protocol approval
            // manager) → the facade falls back to the construction-time snapshot (unchanged).
            let mut browser_tool = nomi_browser::BrowserTool::with_policy(
                &self.config.tools.browser,
                self.config.tools.auto_approve,
                self.config.tools.browser.full_power,
                self.config.tools.browser.persistent_login,
                Some(PathBuf::from(cwd)),
                self.approval_manager.clone(),
                // P3-X2: per-pet secret vault source (vault path + machine-bound key) so the
                // facade lazily loads registered credentials + derives the firewall domain
                // allowlist from their allowed_origins (裁决⑤). None → empty store + unrestricted.
                self.browser_secret_source
                    .clone()
                    .map(|(vault_path, key)| nomi_browser::BrowserSecretSource { vault_path, key }),
            );
            // P7A: site-memory opt-in (LIVE pref `agent.browserUse.siteMemory`, default OFF). When
            // ON, inject a file-backed sink so the agent remembers site structure across sessions
            // (entries injected into observe as untrusted hints; secret-sourced entries dropped by
            // the store). Root is GLOBAL (browser identity is globally shared, NOT per-session) —
            // same `browser-data` root the gateway/engine use. OFF → no sink (zero behavior change).
            if self.config.tools.browser.site_memory {
                let sm_root = nomi_config::config::app_config_dir()
                    .map(|d| d.join("browser-data").join("site-memory"))
                    .unwrap_or_else(|| std::env::temp_dir().join("nomi-browser-data").join("site-memory"));
                let sink = nomi_browser::site_memory::FileSiteMemorySink::new(sm_root);
                let store = std::sync::Arc::new(nomi_browser::site_memory::SiteMemoryStore::new(
                    Box::new(sink),
                ));
                browser_tool = browser_tool.with_site_memory(store);
            }
            // extract-llm: reuse the session provider (the same `provider` driving this engine,
            // created above if none was injected) so act(Extract) does real LLM extraction. No
            // pref — extract is opt-in per-call by the agent; this only enables the capability.
            let extract_model = Arc::new(SessionExtractModel {
                provider: provider.clone(),
                model: self.config.model.clone(),
                max_tokens: self.config.max_tokens,
            });
            browser_tool = browser_tool.with_extract_model(extract_model);
            // P7B: visual fallback opt-in (LIVE pref `agent.browserUse.visualFallback`, default
            // OFF). When ON, inject a session-model `VisualLocator`: if DOM/aria anchoring fails
            // (a `ref` went stale/detached), the facade screenshots the page and asks the vision
            // model to locate the target by description, then clicks the DPR-mapped CSS point.
            // Reuses the session provider/model (no separate cost surface). OFF → no locator
            // injected, so the facade's fallback stays Unavailable (zero behavior change).
            if self.config.tools.browser.visual_fallback {
                let locator = Arc::new(SessionVisualLocator {
                    provider: provider.clone(),
                    model: self.config.model.clone(),
                    max_tokens: self.config.max_tokens,
                });
                browser_tool = browser_tool
                    .with_visual_locator(locator)
                    .with_visual_fallback_enabled(true);
            }
            // Phase D: thread the host approval gate (human takeover + SD-5 egress). When the
            // gate is present it surfaces irreversible actions / gated cross-origin POSTs to the
            // user and awaits a decision; absent → fail-closed (current behavior).
            if let Some(gate) = self.approval_gate.clone() {
                browser_tool = browser_tool.with_approval_gate(gate);
            }
            registry.register(Box::new(browser_tool));
        }
        #[cfg(not(feature = "browser-use"))]
        if self.config.tools.browser.enabled {
            tracing::debug!(
                target: "nomi_agent",
                "browser-use enabled in config but this build lacks the browser-use feature; the \
                 native Browser tool is not registered."
            );
        }

        // codex-style stateless todo checklist tool. Always registered (not
        // deferred), surfaced to the frontend via the Plan event bridge.
        registry.register(Box::new(nomi_tools::update_plan::UpdatePlanTool::new()));

        // Per-node 工具白名单（受限角色的编排 worker）：非空时只保留白名单内的
        // 工具（含 MCP 代理）。放在全部注册之后、ToolSearch 快照与引擎构造之前
        // —— registry 没有 unregister，这是唯一收口点。空 = 不限制（默认）。
        registry.retain_named(&self.config.tools.builtin_allowlist);

        let tool_defs_snapshot = registry.to_tool_defs();
        registry.register(Box::new(nomi_tools::tool_search::ToolSearchTool::new(
            tool_defs_snapshot,
        )));

        let mut engine = if let Some(session) = self.resume_session {
            AgentEngine::resume_with_provider(
                provider.clone(),
                self.config,
                registry,
                self.output,
                session,
                cwd_path.to_path_buf(),
            )
        } else {
            AgentEngine::new_with_provider(
                provider.clone(),
                self.config,
                registry,
                self.output,
                cwd_path.to_path_buf(),
            )
        };
        engine.set_plan_active_flag(plan_active_flag);
        engine.set_process_supervisor(Arc::clone(&process_supervisor));
        if let Some(spec) = self.goal {
            engine.set_goal(spec.objective, spec.max_auto_continuations);
        }

        Ok(BootstrapResult {
            engine,
            provider,
            mcp_managers,
            has_mcp,
        })
    }
}

#[cfg(all(test, feature = "browser-use"))]
mod visual_locator_tests {
    use super::{parse_som_locate_result, parse_visual_locate_result};

    #[test]
    fn parses_clean_json_box() {
        let r = parse_visual_locate_result(
            r#"{"x": 100, "y": 200, "width": 40, "height": 20, "confidence": 0.9}"#,
        )
        .expect("clean JSON should parse");
        assert_eq!(r.pixel_box.x, 100.0);
        assert_eq!(r.pixel_box.y, 200.0);
        assert_eq!(r.pixel_box.width, 40.0);
        assert_eq!(r.pixel_box.height, 20.0);
        assert!((r.confidence - 0.9).abs() < 1e-9);
    }

    #[test]
    fn extracts_json_from_code_fence() {
        // Vision models love wrapping JSON in a ```json fence — tolerate it.
        let raw = "```json\n{\"x\": 1, \"y\": 2, \"width\": 3, \"height\": 4, \"confidence\": 0.5}\n```";
        let r = parse_visual_locate_result(raw).expect("fenced JSON should parse");
        assert_eq!(r.pixel_box.width, 3.0);
    }

    #[test]
    fn extracts_json_from_surrounding_prose() {
        let raw = "Sure! Here is the box: {\"x\": 5, \"y\": 6, \"width\": 7, \"height\": 8, \"confidence\": 0.8} — hope that helps.";
        let r = parse_visual_locate_result(raw).expect("prose-wrapped JSON should parse");
        assert_eq!(r.pixel_box.x, 5.0);
    }

    #[test]
    fn confidence_clamped_to_unit_interval() {
        let r = parse_visual_locate_result(
            r#"{"x": 1, "y": 1, "width": 1, "height": 1, "confidence": 1.7}"#,
        )
        .expect("over-unit confidence should still parse");
        assert_eq!(r.confidence, 1.0);
    }

    #[test]
    fn not_found_confidence_zero_is_err() {
        assert!(parse_visual_locate_result(r#"{"confidence": 0}"#).is_err());
    }

    #[test]
    fn zero_sized_box_is_err() {
        // A box with no area can't be a click target → not-found.
        assert!(
            parse_visual_locate_result(
                r#"{"x": 1, "y": 1, "width": 0, "height": 10, "confidence": 0.9}"#
            )
            .is_err()
        );
    }

    #[test]
    fn missing_box_fields_is_err() {
        // Confidence present but no coordinates → can't act.
        assert!(parse_visual_locate_result(r#"{"confidence": 0.9}"#).is_err());
    }

    #[test]
    fn non_json_reply_is_err() {
        assert!(parse_visual_locate_result("I could not find that element.").is_err());
    }

    // ── SoM label parser (parse_som_locate_result) ──

    #[test]
    fn som_parses_valid_label() {
        let r = parse_som_locate_result(r#"{"label": 3, "confidence": 0.88}"#, 10)
            .expect("valid label should parse");
        assert_eq!(r.label, 3);
        assert!((r.confidence - 0.88).abs() < 1e-9);
    }

    #[test]
    fn som_extracts_label_from_code_fence() {
        let raw = "```json\n{\"label\": 7, \"confidence\": 0.6}\n```";
        let r = parse_som_locate_result(raw, 12).expect("fenced JSON should parse");
        assert_eq!(r.label, 7);
    }

    #[test]
    fn som_label_zero_is_err() {
        // {"label": 0} is the model's "none matched" sentinel.
        assert!(parse_som_locate_result(r#"{"label": 0, "confidence": 0}"#, 10).is_err());
    }

    #[test]
    fn som_label_out_of_range_is_err() {
        // Hallucinated label beyond n_labels must NOT index the label_map (would be OOB).
        assert!(parse_som_locate_result(r#"{"label": 99, "confidence": 0.9}"#, 12).is_err());
    }

    #[test]
    fn som_label_below_one_is_err() {
        assert!(parse_som_locate_result(r#"{"label": -1, "confidence": 0.9}"#, 12).is_err());
    }

    #[test]
    fn som_non_integer_label_is_err() {
        // A fractional label is not a valid 1-based index.
        assert!(parse_som_locate_result(r#"{"label": 2.5, "confidence": 0.9}"#, 12).is_err());
    }

    #[test]
    fn som_zero_confidence_is_err() {
        assert!(parse_som_locate_result(r#"{"label": 3, "confidence": 0}"#, 10).is_err());
    }

    #[test]
    fn som_missing_label_is_err() {
        assert!(parse_som_locate_result(r#"{"confidence": 0.9}"#, 10).is_err());
    }

    #[test]
    fn som_boundary_label_n_is_ok() {
        // label == n_labels is in range (inclusive upper bound).
        let r = parse_som_locate_result(r#"{"label": 5, "confidence": 0.7}"#, 5)
            .expect("label == n_labels is valid");
        assert_eq!(r.label, 5);
    }
}

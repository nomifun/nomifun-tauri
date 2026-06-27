//! Terminal session orchestration: persists metadata, owns live PTYs, and
//! bridges PTY output/exit to the realtime event bus.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64;
use dashmap::DashMap;
use nomifun_api_types::{CreateTerminalRequest, TerminalSessionResponse};
use nomifun_common::OnTerminalDelete;
use nomifun_db::{CreateTerminalParams, ITerminalRepository};
use tracing::{info, warn};

use crate::driver::{TerminalDescription, TerminalDriver};
use crate::error::TerminalError;
use crate::events::TerminalEventEmitter;
use crate::pty::{PtyHandle, SpawnParams};
use crate::types::{resolve_command, row_to_response};

/// Set default environment that describes the emulator the PTY is actually
/// wired to: `xterm.js` — an xterm-compatible, truecolor-capable terminal.
///
/// `portable-pty` seeds the child from the app process's own environment. A
/// macOS app launched from Finder/Dock/launchd inherits a *minimal* environment
/// with **no `TERM`** (the same reason `nomifun-runtime::shell_env` has to
/// repair `PATH`). With no `TERM`, interactive programs fall back to a dumb,
/// monochrome, no-cursor-control mode: `claude` renders gray-on-gray and zsh's
/// `zle` emits no cursor-movement/erase sequences for Backspace, so a delete
/// looks like the cursor just walks backward without removing the character.
///
/// These are *defaults*: an explicit per-session env (or an inherited value the
/// caller chose to forward) still wins via `or_insert`. We deliberately set
/// `xterm-256color` regardless of any inherited `TERM`, because the child talks
/// to xterm.js, not to whatever terminal happened to launch the app.
fn apply_emulator_env_defaults(env: &mut HashMap<String, String>) {
    env.entry("TERM".to_owned()).or_insert_with(|| "xterm-256color".to_owned());
    env.entry("COLORTERM".to_owned()).or_insert_with(|| "truecolor".to_owned());
}

/// Interval between debounced scrollback persistence passes. Each pass writes
/// only the *dirty* live sessions (see [`PtyHandle::take_dirty_scrollback`]), so
/// idle terminals are never rewritten. A hard app kill loses at most this much
/// of the most recent output for a still-live session — bounded and acceptable
/// (a process that exits flushes its final scrollback immediately via `on_exit`).
const SCROLLBACK_FLUSH_INTERVAL: Duration = Duration::from_secs(5);

/// Hook the IDMM layer registers so a user-driven terminal session (re)arms
/// intelligent-decision supervision on activity. Defined here (the lower crate)
/// so `nomifun-terminal` need not depend on `nomifun-idmm`; `IdmmManager`
/// implements it and `nomifun-app` injects the impl via
/// [`TerminalService::with_terminal_supervision_hook`]. Mirrors
/// `nomifun_conversation::ConversationSupervisionHook`.
///
/// Fire-and-forget, called on create / relaunch / user input. Unlike a chat
/// turn (one fire per turn), a terminal fires on every input chunk, so the impl
/// MUST be a cheap no-op when IDMM is disabled for the terminal or already
/// supervising it (e.g. guard on `is_supervising` before spawning).
pub trait TerminalSupervisionHook: Send + Sync {
    fn on_terminal_activity(&self, terminal_id: i64);
}

/// Manages terminal sessions: DB-persisted metadata + live in-memory PTYs.
#[derive(Clone)]
pub struct TerminalService {
    repo: Arc<dyn ITerminalRepository>,
    emitter: TerminalEventEmitter,
    /// Backend-managed default work dir; responses derive `is_default_workpath`
    /// from it (constructor-injected like `ConversationService`'s work_dir).
    work_dir: std::path::PathBuf,
    live: Arc<DashMap<i64, Arc<PtyHandle>>>,
    /// Sessions created with `defer_spawn` that have not spawned their PTY yet.
    /// The first `resize` (carrying the real fitted size) consumes the marker and
    /// spawns the PTY at that size, so a full-screen TUI never draws at the 80×24
    /// default first. In-memory only: a deferred row that is never resized (e.g.
    /// an app crash before the client mounts) is healed by `reconcile_on_boot`.
    pending_spawn: Arc<DashMap<i64, ()>>,
    /// Late-wired knowledge service (assembly order: knowledge comes up after
    /// the terminal singleton, mirroring `ConversationService`). `None` means
    /// knowledge features are silently skipped (best-effort contract).
    knowledge: Arc<std::sync::RwLock<Option<Arc<nomifun_knowledge::KnowledgeService>>>>,
    /// Hooks notified after a terminal row is deleted (registration order),
    /// mirroring `ConversationService::delete_hooks`. Used by `nomifun-app` to
    /// wire `RequirementService::clear_owner_for_session` so a deleted terminal
    /// drops its requirements' dual-domain owner (no FK to cascade, spec §9.B).
    delete_hooks: Arc<std::sync::RwLock<Vec<Arc<dyn OnTerminalDelete>>>>,
    /// Late-wired IDMM supervision hook (same slot pattern as `delete_hooks`).
    /// Fired on create/relaunch/input so a user-driven terminal re-arms 智能决策
    /// even after a supervisor stood down (Halt) or the PTY was relaunched —
    /// the terminal analogue of `ConversationSupervisionHook`. `None` outside
    /// IDMM-enabled hosts (tests, webui-only).
    supervision_hook: Arc<std::sync::RwLock<Option<Arc<dyn TerminalSupervisionHook>>>>,
    /// Monotonic PTY spawn generation. Every `spawn_pty` mints the next value
    /// and stamps it on the handle + its exit callback, so a relaunch's killed
    /// predecessor (whose exit callback fires after the drain grace) can be told
    /// apart from the live handle and ignored — without it, that stale callback
    /// removes the fresh PTY and marks the session exited ("restart" → "close").
    next_epoch: Arc<std::sync::atomic::AtomicU64>,
    /// Scoped knowledge-search MCP connection (port/token/binary). Late-wired by
    /// `with_knowledge_mcp_config`. `None` → no MCP injection (knowledge tool off).
    knowledge_mcp_config: Arc<std::sync::RwLock<Option<nomifun_api_types::KnowledgeMcpConfig>>>,
    /// Scoped requirement MCP connection (port/token/binary). Late-wired by
    /// `with_requirement_mcp_config`. `None` → no requirement tool injection.
    /// When wired, `build_enhancement` unconditionally injects the requirement MCP
    /// server into every terminal spawn — `apply_enhancement` only renders it for
    /// agent CLIs (claude/codex), so shell/unknown CLIs never see it.
    requirement_mcp_config: Arc<std::sync::RwLock<Option<nomifun_api_types::RequirementMcpConfig>>>,
    /// Platform-private dir for per-terminal CLI config (e.g. claude mcp.json).
    /// NEVER the user's cwd. Defaults to a temp subdir until wired.
    mcp_config_dir: Arc<std::sync::RwLock<std::path::PathBuf>>,
    /// Absolute path to the MCP endpoint beacon file written by the backend on
    /// boot. Passed to spawned PTYs as `NOMI_MCP_ENDPOINTS_FILE` so the knowledge
    /// stdio bridge can discover the endpoint precisely (no data-dir resolution
    /// needed). Wired by `with_mcp_endpoints_path`. `None` → env not set (bridge
    /// falls back to its own data-dir resolution or legacy env vars).
    mcp_endpoints_path: Arc<std::sync::RwLock<Option<String>>>,
    /// Late-wired terminal lifecycle server (Plan 2). Hooks call back here.
    terminal_lifecycle: Arc<std::sync::RwLock<Option<Arc<crate::lifecycle::TerminalLifecycleServer>>>>,
    /// Absolute path to the backend binary, used in lifecycle hook commands.
    lifecycle_binary_path: Arc<std::sync::RwLock<Option<String>>>,
}

impl TerminalService {
    pub fn new(
        repo: Arc<dyn ITerminalRepository>,
        emitter: TerminalEventEmitter,
        work_dir: std::path::PathBuf,
    ) -> Self {
        Self {
            repo,
            emitter,
            work_dir,
            live: Arc::new(DashMap::new()),
            pending_spawn: Arc::new(DashMap::new()),
            knowledge: Arc::new(std::sync::RwLock::new(None)),
            delete_hooks: Arc::new(std::sync::RwLock::new(Vec::new())),
            supervision_hook: Arc::new(std::sync::RwLock::new(None)),
            next_epoch: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            knowledge_mcp_config: Arc::new(std::sync::RwLock::new(None)),
            requirement_mcp_config: Arc::new(std::sync::RwLock::new(None)),
            mcp_config_dir: Arc::new(std::sync::RwLock::new(std::env::temp_dir().join("nomi-terminal-mcp"))),
            mcp_endpoints_path: Arc::new(std::sync::RwLock::new(None)),
            terminal_lifecycle: Arc::new(std::sync::RwLock::new(None)),
            lifecycle_binary_path: Arc::new(std::sync::RwLock::new(None)),
        }
    }

    /// Register a hook notified after a terminal session is deleted.
    ///
    /// Hooks dispatch sequentially in registration order from `delete()`. Used
    /// by `nomifun-app` to wire `RequirementService` so a deleted terminal
    /// clears the owner of any requirement it owned (spec §9.B).
    pub fn with_delete_hook(&self, hook: Arc<dyn OnTerminalDelete>) {
        if let Ok(mut guard) = self.delete_hooks.write() {
            guard.push(hook);
        }
    }

    /// Late-wire the knowledge service (same pattern as
    /// `ConversationService::with_knowledge_service`). Interior mutability so
    /// already-cloned handles (cron executor, AutoWork driver) see it too.
    pub fn with_knowledge_service(&self, service: Arc<nomifun_knowledge::KnowledgeService>) {
        if let Ok(mut guard) = self.knowledge.write() {
            *guard = Some(service);
        }
    }

    /// Late-wire the IDMM supervision hook (interior mutable; already-cloned
    /// router/driver handles see it too). Called by `nomifun-app` so a
    /// user-driven terminal arms 智能决策 supervision.
    pub fn with_terminal_supervision_hook(&self, hook: Arc<dyn TerminalSupervisionHook>) {
        if let Ok(mut guard) = self.supervision_hook.write() {
            *guard = Some(hook);
        }
    }

    /// Fire the supervision hook (fire-and-forget; no-op when unset). Cheap to
    /// call on every input — the hook impl guards on already-supervising.
    fn arm_supervision(&self, id: i64) {
        if let Ok(guard) = self.supervision_hook.read()
            && let Some(hook) = guard.as_ref()
        {
            hook.on_terminal_activity(id);
        }
    }

    /// Late-wire the knowledge MCP connection + the platform-private config dir
    /// used for per-terminal CLI MCP config. Mirrors `with_knowledge_service`.
    pub fn with_knowledge_mcp_config(
        &self,
        config: nomifun_api_types::KnowledgeMcpConfig,
        config_dir: std::path::PathBuf,
    ) {
        if let Ok(mut g) = self.knowledge_mcp_config.write() {
            *g = Some(config);
        }
        if let Ok(mut g) = self.mcp_config_dir.write() {
            *g = config_dir;
        }
    }

    /// Wire the absolute path to the MCP endpoint beacon so spawned PTYs receive
    /// `NOMI_MCP_ENDPOINTS_FILE` in their env. This lets the knowledge stdio bridge
    /// discover the endpoint precisely without computing the data-dir path itself.
    pub fn with_mcp_endpoints_path(&self, path: String) {
        if let Ok(mut g) = self.mcp_endpoints_path.write() {
            *g = Some(path);
        }
    }

    fn knowledge_mcp_config(&self) -> Option<nomifun_api_types::KnowledgeMcpConfig> {
        self.knowledge_mcp_config.read().ok().and_then(|g| g.clone())
    }

    /// Late-wire the requirement MCP connection so terminal launches can inject
    /// the `requirement_complete`/`requirement_update_status` tools into agent CLIs.
    /// Mirrors `with_knowledge_mcp_config`. Interior mutability so already-cloned
    /// handles (cron executor, AutoWork driver) see it too.
    pub fn with_requirement_mcp_config(&self, config: nomifun_api_types::RequirementMcpConfig) {
        if let Ok(mut g) = self.requirement_mcp_config.write() {
            *g = Some(config);
        }
    }

    fn requirement_mcp_config(&self) -> Option<nomifun_api_types::RequirementMcpConfig> {
        self.requirement_mcp_config.read().ok().and_then(|g| g.clone())
    }

    /// Late-wire the terminal lifecycle server (Plan 2). Once wired, new PTY
    /// spawns inject hook commands so CLI hooks call back to this server.
    /// `binary_path` is the absolute path to the backend executable used in hook
    /// command strings.
    pub fn with_terminal_lifecycle(
        &self,
        server: Arc<crate::lifecycle::TerminalLifecycleServer>,
        binary_path: String,
    ) {
        if let Ok(mut g) = self.terminal_lifecycle.write() {
            *g = Some(server);
        }
        if let Ok(mut g) = self.lifecycle_binary_path.write() {
            *g = Some(binary_path);
        }
    }

    /// Subscribe to lifecycle events for a specific terminal. Returns `None` if
    /// no lifecycle server is wired (graceful degradation).
    pub fn subscribe_lifecycle(
        &self,
        terminal_id: i64,
    ) -> Option<tokio::sync::broadcast::Receiver<crate::lifecycle::TerminalLifecycleEvent>> {
        self.terminal_lifecycle
            .read()
            .ok()
            .and_then(|g| g.as_ref().map(|s| s.subscribe(terminal_id)))
    }

    /// Build the launch enhancement for a spawn: knowledge_search MCP when bases
    /// are mounted AND the MCP server is wired; requirement MCP when the requirement
    /// server is wired (unconditional — scoped by terminal_id + owner_kind);
    /// lifecycle hooks when the lifecycle server is wired. Empty otherwise (honest
    /// no-op).
    fn build_enhancement(&self, kb_ids: &[String], terminal_id: i64) -> crate::enhance::TerminalLaunchEnhancement {
        let mut enh = crate::enhance::TerminalLaunchEnhancement::default();
        if !kb_ids.is_empty() && let Some(cfg) = self.knowledge_mcp_config() {
            use nomifun_api_types::KnowledgeMcpConfig as K;
            // Only PORT and TOKEN are baked — kb_ids is NO LONGER injected into the
            // bridge env. The bridge discovers scope at runtime by reporting its cwd
            // to the in-process server. PORT/TOKEN are kept as an env fallback for
            // when the beacon file is absent (safety net).
            let env = std::collections::HashMap::from([
                (K::ENV_PORT.to_owned(), cfg.port.to_string()),
                (K::ENV_TOKEN.to_owned(), cfg.token.clone()),
            ]);
            enh.mcp_servers.push(crate::enhance::McpServerSpec {
                name: K::SERVER_NAME.to_owned(),
                command: cfg.binary_path.clone(),
                args: vec!["mcp-knowledge-stdio".to_owned()],
                env,
            });
        }
        // Requirement MCP injection: always inject when the requirement server is
        // wired (D2 verdict: always-inject for agent CLIs, NOT gated on AutoWork).
        // `apply_enhancement` only renders MCP servers for known agent CLIs
        // (claude/codex), so shell/unknown CLIs never receive this. Scoped by
        // terminal_id + owner_kind so verify_scope confines mutations to this terminal.
        if let Some(cfg) = self.requirement_mcp_config() {
            use nomifun_api_types::RequirementMcpConfig as R;
            let env = std::collections::HashMap::from([
                (R::ENV_PORT.to_owned(), cfg.port.to_string()),
                (R::ENV_TOKEN.to_owned(), cfg.token.clone()),
                (R::ENV_CONVERSATION_ID.to_owned(), terminal_id.to_string()),
                (R::ENV_OWNER_KIND.to_owned(), "terminal".to_owned()),
            ]);
            enh.mcp_servers.push(crate::enhance::McpServerSpec {
                name: R::SERVER_NAME.to_owned(),
                command: cfg.binary_path.clone(),
                args: vec!["mcp-requirement-stdio".to_owned()],
                env,
            });
        }
        // Lifecycle hook wiring (Plan 2): if the server is running, inject
        // the hook config + env so the CLI calls back on turn boundaries.
        // Guard: skip hook injection if binary_path is empty (startup logic
        // error — emitting a broken ` terminal-hook ...` command with no
        // program is worse than launching without hooks).
        if let Ok(guard) = self.terminal_lifecycle.read() {
            if let Some(server) = guard.as_ref() {
                let binary_path = self.lifecycle_binary_path.read()
                    .ok()
                    .and_then(|g| g.clone())
                    .unwrap_or_default();
                if !binary_path.is_empty() {
                    enh.lifecycle = Some(crate::enhance::LifecycleHookWiring {
                        port: server.http_port(),
                        token: server.auth_token().to_owned(),
                        terminal_id,
                        binary_path,
                    });
                }
            }
        }
        enh
    }

    /// Platform-private per-terminal config dir (NEVER the user cwd).
    fn session_mcp_dir(&self, id: i64) -> std::path::PathBuf {
        let base = self.mcp_config_dir.read().map(|g| g.clone()).unwrap_or_else(|_| std::env::temp_dir());
        base.join(id.to_string())
    }

    /// Create a session: persist the row, spawn the PTY, wire output/exit.
    pub async fn create(
        &self,
        user_id: &str,
        req: CreateTerminalRequest,
    ) -> Result<TerminalSessionResponse, TerminalError> {
        let name = req
            .name
            .clone()
            .filter(|n| !n.trim().is_empty())
            .unwrap_or_else(|| default_name(&req.command, req.backend.as_deref()));
        let args_json = serde_json::to_string(&req.args)?;
        let env_json = req.env.as_ref().map(serde_json::to_string).transpose()?;

        // The id is minted by the DB (INTEGER PRIMARY KEY AUTOINCREMENT) and
        // returned on the row — never client-generated.
        let row = self
            .repo
            .create(&CreateTerminalParams {
                name,
                cwd: req.cwd.clone(),
                command: req.command.clone(),
                args: args_json,
                env: env_json,
                backend: req.backend.clone(),
                mode: req.mode.clone(),
                cols: req.cols as i64,
                rows: req.rows as i64,
                user_id: user_id.to_owned(),
            })
            .await?;
        let id = row.id;

        // Knowledge integration — strictly best-effort: persist the
        // create-time binding, mount the bound bases into the workspace and
        // materialize the README contract. Failures only warn; the PTY always
        // launches.
        if let Some(kb_ids) = req.knowledge_base_ids.as_ref().filter(|ids| !ids.is_empty()) {
            self.bind_knowledge(id, &req.cwd, kb_ids).await;
        }

        if req.defer_spawn {
            // Defer the PTY until the first `resize` carries the real terminal
            // size (interactive path), so a full-screen TUI draws at the correct
            // dimensions from frame one — no 80×24→real jump, no stale-frame
            // replay. Knowledge mounts + spawn happen then (see `spawn_deferred`,
            // mirroring `relaunch`). The row is already 'running'; the spawn is
            // imminent (the client fits-and-resizes on mount) and a crash before
            // it is healed by `reconcile_on_boot`.
            self.pending_spawn.insert(id, ());
            let resp = row_to_response(&row, None, &self.work_dir);
            self.emitter.emit_created(&resp);
            info!(terminal_id = id, "terminal session created (spawn deferred to first resize)");
            return Ok(resp);
        }

        let kb_ids = self.sync_knowledge_workspace(id, &req.cwd, &req.command, &req.args).await;

        self.spawn_pty(
            id,
            &req.command,
            &req.args,
            &req.cwd,
            req.env.clone(),
            req.cols,
            req.rows,
            kb_ids,
            req.backend.as_deref(),
        )?;

        let resp = row_to_response(&row, None, &self.work_dir);
        self.emitter.emit_created(&resp);
        info!(terminal_id = id, "terminal session created");
        // Arm IDMM supervision for the fresh PTY (no-op if disabled / already on).
        self.arm_supervision(id);
        Ok(resp)
    }

    /// Spawn (or respawn) the PTY for a session id and register it as live.
    #[allow(clippy::too_many_arguments)]
    fn spawn_pty(
        &self,
        id: i64,
        command: &str,
        args: &[String],
        cwd: &str,
        env: Option<HashMap<String, String>>,
        cols: u16,
        rows: u16,
        kb_ids: Vec<String>,
        backend: Option<&str>,
    ) -> Result<(), TerminalError> {
        let (program, resolved_args) = resolve_command(command, args);
        // Inject platform capabilities (MCP + lifecycle hooks) into the
        // native CLI launch. Unknown CLIs are returned unchanged (honest).
        let (resolved_args, hook_env) = {
            let enh = self.build_enhancement(&kb_ids, id);
            if enh.is_empty() {
                (resolved_args, Vec::new())
            } else {
                let session_dir = self.session_mcp_dir(id);
                crate::enhance::apply_enhancement(&program, resolved_args, &enh, &session_dir, backend)
            }
        };
        let mut env: HashMap<String, String> = env.unwrap_or_default();
        // Describe the xterm.js emulator the PTY talks to (TERM/COLORTERM), so a
        // Finder/launchd-launched macOS app — which inherits no TERM — still gets
        // color + correct backspace rendering. Defaults only; explicit env wins.
        apply_emulator_env_defaults(&mut env);
        for (k, v) in hook_env {
            env.insert(k, v);
        }
        // Pass the beacon file path so the knowledge stdio bridge discovers the
        // endpoint precisely (no data-dir guessing). The bridge reads this env var
        // as priority-1 in `read_beacon_for_bridge`.
        if let Ok(guard) = self.mcp_endpoints_path.read() {
            if let Some(path) = guard.as_ref() {
                env.insert("NOMI_MCP_ENDPOINTS_FILE".to_owned(), path.clone());
            }
        }

        let emitter_out = self.emitter.clone();
        let on_output = move |chunk: Vec<u8>| {
            emitter_out.emit_output(id, BASE64.encode(&chunk));
        };

        let emitter_exit = self.emitter.clone();
        let repo_exit = self.repo.clone();
        let live_exit = self.live.clone();
        // Mint this PTY's spawn generation; the handle stores it and the exit
        // callback below compares against it.
        let epoch = self.next_epoch.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        // Capture the runtime handle now (we're on the async create path); the
        // exit callback runs on the PTY reader's OS thread, which has no
        // ambient Tokio runtime, so `tokio::spawn` there would panic.
        let rt = tokio::runtime::Handle::current();
        let on_exit = move |code: Option<i32>, scrollback: Vec<u8>| {
            // Tear down ONLY if this PTY is still the live one for the id. A
            // relaunch removes the old handle, kills it, then immediately
            // inserts a fresh higher-epoch handle under the same id; the killed
            // child's exit callback fires later (after EXIT_DRAIN_GRACE). An
            // unconditional teardown here would then remove the FRESH handle and
            // mark the session exited — turning "restart" into "close". The
            // epoch guard makes the stale callback a no-op (also covers delete:
            // the row/handle are already gone, so we skip a doomed status write).
            if live_exit.remove_if(&id, |_, h| h.epoch() == epoch).is_none() {
                return;
            }
            emitter_exit.emit_exit(id, code);
            // Persist the terminal status off the reader thread, onto the runtime.
            let repo = repo_exit.clone();
            rt.spawn(async move {
                if let Err(e) = repo.update_status(id, "exited", code.map(i64::from)).await {
                    warn!(terminal_id = id, error = %e, "failed to persist terminal exit status");
                }
                // Persist the FINAL scrollback so the output survives a restart
                // even if the process exited between debounced flushes — this is
                // what captures the tail the periodic flusher may not have reached.
                if let Err(e) = repo.save_scrollback(id, &scrollback).await {
                    warn!(terminal_id = id, error = %e, "failed to persist final terminal scrollback");
                }
            });
        };

        let handle = PtyHandle::spawn(
            SpawnParams {
                program,
                args: resolved_args,
                cwd: cwd.to_owned(),
                env,
                cols,
                rows,
            },
            epoch,
            on_output,
            on_exit,
        )?;
        self.live.insert(id, handle);

        // Plan-2 lifecycle consumer (proof of channel): subscribe to this
        // terminal's lifecycle events and log them. Plan 3/4 will replace this
        // with real AutoWork/IDMM consumers.
        if let Some(mut rx) = self.subscribe_lifecycle(id) {
            tokio::spawn(async move {
                loop {
                    match rx.recv().await {
                        Ok(ev) => {
                            info!(terminal_id = ev.terminal_id, kind = ?ev.kind, "terminal lifecycle event");
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                            warn!(terminal_id = id, lagged = n, "lifecycle consumer lagged");
                        }
                    }
                }
            });
        }

        Ok(())
    }

    fn knowledge_service(&self) -> Option<Arc<nomifun_knowledge::KnowledgeService>> {
        self.knowledge.read().ok().and_then(|g| g.clone())
    }

    /// Persist the create-time knowledge selection. Best-effort: a missing
    /// knowledge service or a failed write only warns.
    ///
    /// The binding is keyed by **workpath** (spec §7), not the per-session id —
    /// the exact key the session-header `KnowledgeControl` and the mount
    /// resolver (`ensure_mounts_for_session`) read. Writing the legacy
    /// `("terminal", id)` key here is what made the create-time picker invisible
    /// in the session header (it reads `("workpath", key)`, which had no row)
    /// and let any workpath row silently shadow the selection at mount time.
    async fn bind_knowledge(&self, id: i64, cwd: &str, kb_ids: &[String]) {
        let Some(ks) = self.knowledge_service() else {
            warn!(terminal_id = id, "knowledge_base_ids given but no knowledge service is wired — skipping binding");
            return;
        };
        // Use the SAME key derivation as `sync_knowledge_workspace` so the write
        // lands on exactly the row the mount + header read back.
        let wp_key = nomifun_knowledge::session_workpath_key(std::path::Path::new(cwd), &self.work_dir);
        // Read-modify-write: set the bases and enable the binding WITHOUT
        // clobbering any writeback ("回血") config already on this workpath
        // (e.g. configured from the homepage or the session header).
        let mut binding = ks
            .get_binding(nomifun_knowledge::WORKPATH_BINDING_KIND, &wp_key)
            .await
            .unwrap_or_default();
        binding.enabled = true;
        binding.kb_ids = kb_ids.to_vec();
        if let Err(e) = ks
            .set_binding(nomifun_knowledge::WORKPATH_BINDING_KIND, &wp_key, binding)
            .await
        {
            warn!(terminal_id = id, error = %e, "failed to persist terminal workpath knowledge binding");
        }
    }

    /// Sync this terminal's bound knowledge bases into `{cwd}/.nomi/knowledge/`
    /// and materialize the standalone README contract next to the mounts.
    /// Returns the mounted base ids (empty = nothing mounted), which are baked
    /// into the injected knowledge_search MCP env so the model cannot widen the
    /// searchable set. The README's `has_search_tool` claim is honest: it only
    /// asserts the tool exists when the MCP is launch-injected — true for
    /// Claude/Codex (including wrappers like `stepcode claude`). Gemini gets
    /// the tool via one-click registration, not launch, so it's false here.
    /// Never blocks the launch — failures degrade to warnings.
    async fn sync_knowledge_workspace(&self, id: i64, cwd: &str, command: &str, args: &[String]) -> Vec<String> {
        let Some(ks) = self.knowledge_service() else {
            return Vec::new();
        };
        let id_str = id.to_string();
        // Workpath-first (session-list unification spec §7): the binding
        // belongs to the workspace path, not the terminal session.
        // `session_workpath_key` maps a backend-managed default cwd — one
        // under `work_dir`, the same root `row_to_response` uses for the
        // `is_default_workpath` flag — to the `__default__` sentinel, and a
        // custom cwd to its normalized key. The knowledge service looks up
        // the `('workpath', key)` row first and only falls back to the legacy
        // `('terminal', id)` binding on a full miss.
        let cwd_path = std::path::Path::new(cwd);
        let wp_key = nomifun_knowledge::session_workpath_key(cwd_path, &self.work_dir);
        let outcome = ks
            .ensure_mounts_for_session(&wp_key, "terminal", &id_str, cwd_path)
            .await;
        if outcome.mounts.is_empty() {
            return Vec::new();
        }
        // Determine whether the knowledge_search MCP tool will ACTUALLY be
        // launch-injected for this terminal. The tool is injected only when
        // (a) the MCP config is wired and (b) the CLI resolves to Claude or
        // Codex (including wrappers like `stepcode claude`). Gemini gets the
        // tool via one-click registration (.gemini/settings.json), NOT at
        // launch, so it is false here; unknown CLIs likewise.
        let (program, prog_args) = crate::types::resolve_command(command, args);
        let tool_available = self.knowledge_mcp_config().is_some()
            && matches!(
                crate::enhance::resolve_agent_family(&program, &prog_args, None),
                Some(crate::enhance::AgentCli::Claude) | Some(crate::enhance::AgentCli::Codex)
            );

        if let Some(readme) = nomifun_knowledge::build_knowledge_context(
            &outcome.mounts,
            &nomifun_knowledge::KnowledgeContextOptions {
                format: nomifun_knowledge::KnowledgeContextFormat::TerminalReadme,
                writeback: outcome.writeback,
                writeback_mode: Some(&outcome.writeback_mode),
                writeback_eagerness: Some(&outcome.writeback_eagerness),
                target_id: &id_str,
                has_search_tool: tool_available,
                has_write_tool: false,
            },
        ) {
            // README.md is on the mount engine's MANAGED_KEEP whitelist, so later
            // syncs never sweep it away.
            let dir = std::path::Path::new(cwd).join(nomifun_knowledge::KB_MOUNT_REL_DIR);
            if let Err(e) = async {
                tokio::fs::create_dir_all(&dir).await?;
                tokio::fs::write(dir.join("README.md"), readme).await
            }
            .await
            {
                warn!(terminal_id = id, error = %e, "failed to write knowledge README — continuing");
            }
        }
        // kb_ids: extracted from mount outcome. Field is `id` on KnowledgeMountInfo.
        outcome.mounts.iter().map(|m| m.id.clone()).collect()
    }

    /// List sessions for a user.
    pub async fn list(&self, user_id: &str) -> Result<Vec<TerminalSessionResponse>, TerminalError> {
        let rows = self.repo.list_by_user(user_id).await?;
        Ok(rows.iter().map(|r| row_to_response(r, None, &self.work_dir)).collect())
    }

    /// Get one session, including a base64 scrollback snapshot. A live session
    /// returns its in-memory scrollback; a session with no live PTY (e.g. after
    /// an app restart) falls back to the persisted snapshot so the frontend can
    /// still replay its history.
    pub async fn get(&self, id: i64) -> Result<TerminalSessionResponse, TerminalError> {
        let row = self
            .repo
            .get_by_id(id)
            .await?
            .ok_or_else(|| TerminalError::NotFound(id.to_string()))?;
        let scrollback = match self.live.get(&id) {
            Some(h) => Some(BASE64.encode(h.scrollback())),
            None => self.repo.load_scrollback(id).await?.map(|b| BASE64.encode(b)),
        };
        Ok(row_to_response(&row, scrollback, &self.work_dir))
    }

    /// Enumerate entries under `path` (workspace-relative) inside this terminal
    /// session's working directory. The root is server-authoritative — derived
    /// from the row's `cwd`, never a client-supplied path — so listing it grants
    /// no capability beyond the shell the user already runs there. The
    /// `..`-rejection + boundary/depth guards and the optional case-insensitive
    /// `search` filter live in [`nomifun_file::list_workspace_level`]. The exact
    /// terminal analogue of `ConversationService::browse_workspace`.
    pub async fn browse_workspace(
        &self,
        id: i64,
        path: &str,
        search: Option<&str>,
    ) -> Result<Vec<nomifun_api_types::WorkspaceEntry>, nomifun_common::AppError> {
        let row = self
            .repo
            .get_by_id(id)
            .await
            .map_err(|e| nomifun_common::AppError::Internal(format!("Failed to load terminal session: {e}")))?
            .ok_or_else(|| nomifun_common::AppError::NotFound(format!("Terminal session '{id}' not found")))?;
        if row.cwd.trim().is_empty() {
            return Err(nomifun_common::AppError::BadRequest(
                "Terminal session has no working directory".into(),
            ));
        }
        nomifun_file::list_workspace_level(std::path::Path::new(&row.cwd), path, search)
    }

    /// Boot reconciliation: flip every ghost `running` row to `exited`. At
    /// startup the in-memory live PTY map is empty, so any row still flagged
    /// `running` is a process that died with the previous app run. Making the
    /// state honest is what (a) lets the frontend show the relaunch entry +
    /// replay persisted scrollback instead of a black screen, and (b) makes a
    /// cron-bound terminal's fire-time `live` check take the relaunch path
    /// rather than writing to a dead handle. Call once at boot, before cron
    /// init. Returns the number of rows reconciled.
    pub async fn reconcile_on_boot(&self) -> Result<u64, TerminalError> {
        let n = self.repo.mark_all_running_exited().await?;
        if n > 0 {
            info!(reconciled = n, "terminal boot reconciliation: ghost 'running' sessions marked exited");
        }
        Ok(n)
    }

    /// Spawn the background scrollback persistence loop. Every
    /// [`SCROLLBACK_FLUSH_INTERVAL`] it persists each *dirty* live session's
    /// scrollback so a restart can replay history. Idle sessions are skipped.
    /// Spawn exactly once at boot (the service is cheaply cloneable — Arc fields).
    pub fn spawn_scrollback_flusher(&self) {
        let svc = self.clone();
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(SCROLLBACK_FLUSH_INTERVAL);
            // The first tick fires immediately; skip it (nothing to flush yet).
            ticker.tick().await;
            loop {
                ticker.tick().await;
                svc.flush_dirty_scrollback().await;
            }
        });
    }

    /// One persistence pass: write every dirty live session's scrollback to the
    /// DB. Snapshots are collected from the `DashMap` synchronously (no await
    /// held across shard locks), then written outside the iterator.
    async fn flush_dirty_scrollback(&self) {
        let pending: Vec<(i64, Vec<u8>)> = self
            .live
            .iter()
            .filter_map(|e| e.value().take_dirty_scrollback().map(|sb| (*e.key(), sb)))
            .collect();
        for (id, sb) in pending {
            if let Err(e) = self.repo.save_scrollback(id, &sb).await {
                warn!(terminal_id = id, error = %e, "failed to persist terminal scrollback");
            }
        }
    }

    /// Write base64-encoded bytes to the PTY.
    pub async fn input(&self, id: i64, data_b64: &str) -> Result<(), TerminalError> {
        let bytes = BASE64
            .decode(data_b64)
            .map_err(|e| TerminalError::InvalidInput(format!("base64: {e}")))?;
        let handle = self
            .live
            .get(&id)
            .ok_or_else(|| TerminalError::NotFound(id.to_string()))?;
        handle.write(&bytes)?;
        // Re-arm IDMM supervision on user activity (no-op if disabled / already
        // supervising) — covers re-arm after a prior supervisor stood down.
        self.arm_supervision(id);
        Ok(())
    }

    /// Resize the PTY and persist the new dimensions.
    pub async fn resize(&self, id: i64, cols: u16, rows: u16) -> Result<(), TerminalError> {
        // Deferred-spawn sessions spawn their PTY on the FIRST resize, at the
        // real fitted size — so a full-screen TUI (claude) draws correctly from
        // frame one instead of at 80×24 then jumping. `remove` is atomic, so when
        // two near-simultaneous resizes race (rAF + ResizeObserver), exactly one
        // wins the spawn; the loser falls through to the live-handle resize below.
        if self.pending_spawn.remove(&id).is_some() {
            return self.spawn_deferred(id, cols, rows).await;
        }
        {
            let handle = self
                .live
                .get(&id)
                .ok_or_else(|| TerminalError::NotFound(id.to_string()))?;
            handle.resize(cols, rows)?;
        }
        self.repo.update_size(id, cols as i64, rows as i64).await?;
        Ok(())
    }

    /// Spawn the PTY for a deferred-create session at the given (real) size,
    /// reading its command/cwd/env from the persisted row and re-syncing
    /// knowledge mounts (mirrors `relaunch` — the documented moment a binding
    /// takes effect). Persists the real size and arms IDMM supervision.
    async fn spawn_deferred(&self, id: i64, cols: u16, rows: u16) -> Result<(), TerminalError> {
        let row = self
            .repo
            .get_by_id(id)
            .await?
            .ok_or_else(|| TerminalError::NotFound(id.to_string()))?;
        let args = crate::types::parse_args(&row.args);
        let env: Option<HashMap<String, String>> = row.env.as_deref().and_then(|s| serde_json::from_str(s).ok());
        let kb_ids = self.sync_knowledge_workspace(id, &row.cwd, &row.command, &args).await;
        self.spawn_pty(id, &row.command, &args, &row.cwd, env, cols, rows, kb_ids, row.backend.as_deref())?;
        self.repo.update_size(id, cols as i64, rows as i64).await?;
        self.arm_supervision(id);
        info!(terminal_id = id, cols, rows, "deferred terminal spawned on first resize");
        Ok(())
    }

    /// Kill the child process (session row remains, status flips to exited via on_exit).
    pub async fn kill(&self, id: i64) -> Result<(), TerminalError> {
        let handle = self
            .live
            .get(&id)
            .ok_or_else(|| TerminalError::NotFound(id.to_string()))?;
        handle.kill()
    }

    /// Kill (if live) and delete the session row.
    pub async fn delete(&self, id: i64) -> Result<(), TerminalError> {
        // Drop any pending deferred-spawn marker so a never-resized session does
        // not leak (and cannot later spawn against a deleted row).
        self.pending_spawn.remove(&id);
        if let Some((_, handle)) = self.live.remove(&id) {
            let _ = handle.kill();
        }
        self.repo.delete(id).await?;
        // Best-effort cleanup of the per-terminal private MCP config dir so
        // `terminal-mcp/<id>/` doesn't accumulate forever. Ignore errors: the
        // dir may not exist for shell terminals that never got enhancement.
        let _ = std::fs::remove_dir_all(self.session_mcp_dir(id));
        // Snapshot the hook list under the read lock, then drop the guard before
        // awaiting — `RwLockReadGuard` is not `Send` (mirrors ConversationService).
        let hooks: Vec<Arc<dyn OnTerminalDelete>> =
            self.delete_hooks.read().map(|guard| guard.clone()).unwrap_or_default();
        for hook in hooks {
            hook.on_terminal_deleted(id).await;
        }
        self.emitter.emit_removed(id);
        Ok(())
    }

    /// Relaunch a session **in place**: kill the old PTY (if any) and spawn a
    /// fresh child for the SAME session id, reusing the stored command/cwd/env.
    /// The session keeps its id, name and sidebar entry — only the underlying
    /// process is replaced. A PTY child cannot be resumed once it exits, so a
    /// new process is unavoidable; reusing the id keeps continuity for the user.
    pub async fn relaunch(&self, id: i64) -> Result<TerminalSessionResponse, TerminalError> {
        let row = self
            .repo
            .get_by_id(id)
            .await?
            .ok_or_else(|| TerminalError::NotFound(id.to_string()))?;

        // Tear down any still-running PTY for this id first.
        if let Some((_, handle)) = self.live.remove(&id) {
            let _ = handle.kill();
        }
        // A relaunch spawns fresh now, so a pending deferred-spawn marker (if the
        // session was never resized) is obsolete — clear it to avoid a later
        // resize spawning a second PTY for the same id.
        self.pending_spawn.remove(&id);

        let args = crate::types::parse_args(&row.args);
        let env: Option<HashMap<String, String>> = row.env.as_deref().and_then(|s| serde_json::from_str(s).ok());
        // Re-sync knowledge mounts + README on every relaunch: this is the
        // documented moment a binding change (via KnowledgeControl) takes
        // effect for a terminal session.
        let kb_ids = self.sync_knowledge_workspace(id, &row.cwd, &row.command, &args).await;
        if let Err(e) = self.spawn_pty(
            id,
            &row.command,
            &args,
            &row.cwd,
            env,
            row.cols as u16,
            row.rows as u16,
            kb_ids,
            row.backend.as_deref(),
        ) {
            // The old PTY is already removed + killed; if the fresh spawn fails
            // the session has no process. Record it as exited deterministically —
            // the predecessor's exit callback is epoch-guarded now and will NOT
            // flip the status for us, so without this a failed relaunch would
            // leave a ghost "running" row (only healed at the next boot).
            let _ = self.repo.update_status(id, "exited", None).await;
            return Err(e);
        }

        // The fresh process starts with empty scrollback — drop the persisted
        // snapshot so a later restart does not replay the *previous* process's
        // output as this one's history. Best-effort: the new handle's flushes
        // repopulate it. (Common path — relaunch of an already-exited session —
        // has no pending exit callback to re-persist the old snapshot.)
        if let Err(e) = self.repo.clear_scrollback(id).await {
            warn!(terminal_id = id, error = %e, "failed to clear persisted scrollback on relaunch");
        }

        // Reset status to running and broadcast the refreshed session.
        self.repo.update_status(id, "running", None).await?;
        let updated = self
            .repo
            .get_by_id(id)
            .await?
            .ok_or_else(|| TerminalError::NotFound(id.to_string()))?;
        let resp = row_to_response(&updated, None, &self.work_dir);
        self.emitter.emit_updated(&resp);
        info!(terminal_id = id, "terminal session relaunched in place");
        // Re-arm IDMM supervision for the fresh PTY (the old supervisor stood
        // down when the previous PTY exited).
        self.arm_supervision(id);
        Ok(resp)
    }

    /// Rename a session and/or toggle its pinned state. Broadcasts the update.
    pub async fn update_meta(
        &self,
        id: i64,
        name: Option<String>,
        pinned: Option<bool>,
    ) -> Result<TerminalSessionResponse, TerminalError> {
        let name = name.map(|n| n.trim().to_owned()).filter(|n| !n.is_empty());
        self.repo.update_meta(id, name.as_deref(), pinned).await?;
        let row = self
            .repo
            .get_by_id(id)
            .await?
            .ok_or_else(|| TerminalError::NotFound(id.to_string()))?;
        let resp = row_to_response(&row, None, &self.work_dir);
        self.emitter.emit_updated(&resp);
        Ok(resp)
    }
}

#[async_trait::async_trait]
impl TerminalDriver for TerminalService {
    async fn write_input(&self, id: i64, bytes: &[u8]) -> Result<(), TerminalError> {
        let handle = self
            .live
            .get(&id)
            .ok_or_else(|| TerminalError::NotFound(id.to_string()))?;
        handle.write(bytes)
    }

    fn subscribe_output(&self, id: i64) -> Option<tokio::sync::broadcast::Receiver<Vec<u8>>> {
        self.live.get(&id).map(|h| h.subscribe_output())
    }

    fn is_alive(&self, id: i64) -> bool {
        self.live.contains_key(&id)
    }

    async fn describe(&self, id: i64) -> Result<Option<TerminalDescription>, TerminalError> {
        let Some(row) = self.repo.get_by_id(id).await? else {
            return Ok(None);
        };
        Ok(Some(TerminalDescription {
            user_id: row.user_id,
            cwd: row.cwd,
            command: row.command,
            args: crate::types::parse_args(&row.args),
            backend: row.backend,
            mode: row.mode,
            last_status: row.last_status,
        }))
    }

    async fn read_autowork(&self, id: i64) -> Result<Option<String>, TerminalError> {
        let Some(row) = self.repo.get_by_id(id).await? else {
            return Ok(None);
        };
        Ok(row.autowork)
    }

    async fn write_autowork(&self, id: i64, autowork: Option<&str>) -> Result<(), TerminalError> {
        self.repo.update_autowork(id, autowork).await?;
        Ok(())
    }

    async fn read_idmm(&self, id: i64) -> Result<Option<String>, TerminalError> {
        self.repo.get_idmm(id).await.map_err(Into::into)
    }

    async fn write_idmm(&self, id: i64, idmm: Option<&str>) -> Result<(), TerminalError> {
        self.repo.update_idmm(id, idmm).await?;
        Ok(())
    }

    fn subscribe_lifecycle(
        &self,
        id: i64,
    ) -> Option<tokio::sync::broadcast::Receiver<crate::lifecycle::TerminalLifecycleEvent>> {
        // Delegate to the inherent method (same name, same signature).
        // Rust resolves `self.subscribe_lifecycle(id)` to the inherent impl which
        // takes priority over the trait method, so this is unambiguous.
        TerminalService::subscribe_lifecycle(self, id)
    }
}

fn default_name(command: &str, backend: Option<&str>) -> String {
    if let Some(b) = backend
        && !b.trim().is_empty()
    {
        return b.to_owned();
    }
    if command == crate::types::SHELL_SENTINEL {
        "Shell".to_owned()
    } else {
        command.to_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nomifun_api_types::WebSocketMessage;
    use nomifun_realtime::EventBroadcaster;
    use std::sync::Mutex;
    use std::time::Duration;

    // --- Emulator env defaults -------------------------------------------

    #[test]
    fn emulator_env_defaults_fill_term_and_colorterm_when_absent() {
        // A Finder/launchd-launched macOS app inherits no TERM; without a
        // default the PTY child (zsh/claude) falls back to a dumb, monochrome,
        // no-cursor-control mode (gray output + backspace that doesn't erase).
        let mut env: HashMap<String, String> = HashMap::new();
        apply_emulator_env_defaults(&mut env);
        assert_eq!(env.get("TERM").map(String::as_str), Some("xterm-256color"));
        assert_eq!(env.get("COLORTERM").map(String::as_str), Some("truecolor"));
    }

    #[test]
    fn emulator_env_defaults_preserve_explicit_term() {
        // An explicit per-session TERM must win; COLORTERM is still defaulted.
        let mut env: HashMap<String, String> = HashMap::new();
        env.insert("TERM".to_owned(), "screen-256color".to_owned());
        apply_emulator_env_defaults(&mut env);
        assert_eq!(env.get("TERM").map(String::as_str), Some("screen-256color"));
        assert_eq!(env.get("COLORTERM").map(String::as_str), Some("truecolor"));
    }

    // End-to-end: prove the injected TERM/COLORTERM actually reach the spawned
    // child through the REAL PtyHandle path (not just the env map). This mirrors
    // the Finder-launch case: `cmd.env()` overrides portable-pty's inherited
    // base, so the child sees xterm-256color regardless of the parent's env.
    #[cfg(unix)]
    #[test]
    fn pty_child_actually_receives_injected_term() {
        use crate::pty::{PtyHandle, SpawnParams};
        use std::sync::atomic::{AtomicBool, Ordering};

        let mut env: HashMap<String, String> = HashMap::new();
        apply_emulator_env_defaults(&mut env); // exactly what spawn_pty does

        let captured = Arc::new(Mutex::new(Vec::<u8>::new()));
        let cap = captured.clone();
        let done = Arc::new(AtomicBool::new(false));
        let done_cb = done.clone();
        let _handle = PtyHandle::spawn(
            SpawnParams {
                program: "sh".to_owned(),
                args: vec![
                    "-c".to_owned(),
                    "printf 'TERM=[%s] CT=[%s]\\n' \"$TERM\" \"$COLORTERM\"".to_owned(),
                ],
                cwd: String::new(),
                env,
                cols: 80,
                rows: 24,
            },
            0,
            move |chunk| cap.lock().unwrap().extend_from_slice(&chunk),
            move |_code, _sb| done_cb.store(true, Ordering::SeqCst),
        )
        .expect("spawn sh");

        for _ in 0..250 {
            if done.load(Ordering::SeqCst) {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        std::thread::sleep(Duration::from_millis(50));
        let out = String::from_utf8_lossy(&captured.lock().unwrap()).to_string();
        assert!(
            out.contains("TERM=[xterm-256color]"),
            "child must receive the injected TERM, got: {out:?}"
        );
        assert!(
            out.contains("CT=[truecolor]"),
            "child must receive the injected COLORTERM, got: {out:?}"
        );
    }

    // --- In-memory repo --------------------------------------------------

    #[derive(Default)]
    struct MemRepo {
        rows: Mutex<HashMap<i64, nomifun_db::TerminalSessionRow>>,
        next_id: std::sync::atomic::AtomicI64,
        scrollback: Mutex<HashMap<i64, Vec<u8>>>,
    }

    #[async_trait::async_trait]
    impl ITerminalRepository for MemRepo {
        async fn create(
            &self,
            p: &CreateTerminalParams,
        ) -> Result<nomifun_db::TerminalSessionRow, nomifun_db::DbError> {
            // SQLite mints the id; the mock allocates a monotonic i64.
            let id = self.next_id.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
            let row = nomifun_db::TerminalSessionRow {
                id,
                name: p.name.clone(),
                cwd: p.cwd.clone(),
                command: p.command.clone(),
                args: p.args.clone(),
                env: p.env.clone(),
                backend: p.backend.clone(),
                mode: p.mode.clone(),
                cols: p.cols,
                rows: p.rows,
                created_at: 1,
                updated_at: 1,
                last_status: "running".into(),
                exit_code: None,
                user_id: p.user_id.clone(),
                pinned: false,
                pinned_at: None,
                autowork: None,
                idmm: None,
            };
            self.rows.lock().unwrap().insert(id, row.clone());
            Ok(row)
        }
        async fn get_by_id(&self, id: i64) -> Result<Option<nomifun_db::TerminalSessionRow>, nomifun_db::DbError> {
            Ok(self.rows.lock().unwrap().get(&id).cloned())
        }
        async fn list_by_user(
            &self,
            user_id: &str,
        ) -> Result<Vec<nomifun_db::TerminalSessionRow>, nomifun_db::DbError> {
            Ok(self
                .rows
                .lock()
                .unwrap()
                .values()
                .filter(|r| r.user_id == user_id)
                .cloned()
                .collect())
        }
        async fn update_status(
            &self,
            id: i64,
            status: &str,
            exit_code: Option<i64>,
        ) -> Result<(), nomifun_db::DbError> {
            let mut rows = self.rows.lock().unwrap();
            let row = rows
                .get_mut(&id)
                .ok_or_else(|| nomifun_db::DbError::NotFound(id.to_string()))?;
            row.last_status = status.to_owned();
            row.exit_code = exit_code;
            Ok(())
        }
        async fn update_size(&self, id: i64, cols: i64, rows_: i64) -> Result<(), nomifun_db::DbError> {
            let mut rows = self.rows.lock().unwrap();
            let row = rows
                .get_mut(&id)
                .ok_or_else(|| nomifun_db::DbError::NotFound(id.to_string()))?;
            row.cols = cols;
            row.rows = rows_;
            Ok(())
        }
        async fn update_meta(
            &self,
            id: i64,
            name: Option<&str>,
            pinned: Option<bool>,
        ) -> Result<(), nomifun_db::DbError> {
            let mut rows = self.rows.lock().unwrap();
            let row = rows
                .get_mut(&id)
                .ok_or_else(|| nomifun_db::DbError::NotFound(id.to_string()))?;
            if let Some(n) = name {
                row.name = n.to_owned();
            }
            if let Some(p) = pinned {
                row.pinned = p;
                row.pinned_at = if p { Some(2) } else { None };
            }
            Ok(())
        }
        async fn delete(&self, id: i64) -> Result<(), nomifun_db::DbError> {
            self.rows
                .lock()
                .unwrap()
                .remove(&id)
                .map(|_| ())
                .ok_or_else(|| nomifun_db::DbError::NotFound(id.to_string()))
        }
        async fn update_autowork(&self, id: i64, autowork: Option<&str>) -> Result<(), nomifun_db::DbError> {
            let mut rows = self.rows.lock().unwrap();
            let row = rows
                .get_mut(&id)
                .ok_or_else(|| nomifun_db::DbError::NotFound(id.to_string()))?;
            row.autowork = autowork.map(|s| s.to_owned());
            Ok(())
        }
        async fn update_idmm(&self, id: i64, idmm: Option<&str>) -> Result<(), nomifun_db::DbError> {
            let mut rows = self.rows.lock().unwrap();
            let row = rows
                .get_mut(&id)
                .ok_or_else(|| nomifun_db::DbError::NotFound(id.to_string()))?;
            row.idmm = idmm.map(|s| s.to_owned());
            Ok(())
        }
        async fn get_idmm(&self, id: i64) -> Result<Option<String>, nomifun_db::DbError> {
            Ok(self.rows.lock().unwrap().get(&id).and_then(|r| r.idmm.clone()))
        }
        async fn mark_all_running_exited(&self) -> Result<u64, nomifun_db::DbError> {
            let mut rows = self.rows.lock().unwrap();
            let mut n = 0u64;
            for row in rows.values_mut() {
                if row.last_status == "running" {
                    row.last_status = "exited".to_owned();
                    row.exit_code = None;
                    n += 1;
                }
            }
            Ok(n)
        }
        async fn save_scrollback(&self, id: i64, data: &[u8]) -> Result<(), nomifun_db::DbError> {
            self.scrollback.lock().unwrap().insert(id, data.to_vec());
            Ok(())
        }
        async fn load_scrollback(&self, id: i64) -> Result<Option<Vec<u8>>, nomifun_db::DbError> {
            Ok(self.scrollback.lock().unwrap().get(&id).cloned())
        }
        async fn clear_scrollback(&self, id: i64) -> Result<(), nomifun_db::DbError> {
            self.scrollback.lock().unwrap().remove(&id);
            Ok(())
        }
    }

    // --- Capturing broadcaster ------------------------------------------

    #[derive(Default, Clone)]
    struct CapturingBroadcaster {
        events: Arc<Mutex<Vec<WebSocketMessage<serde_json::Value>>>>,
    }

    impl EventBroadcaster for CapturingBroadcaster {
        fn broadcast(&self, event: WebSocketMessage<serde_json::Value>) {
            self.events.lock().unwrap().push(event);
        }
    }

    fn service() -> (TerminalService, CapturingBroadcaster) {
        let bc = CapturingBroadcaster::default();
        let emitter = TerminalEventEmitter::new(Arc::new(bc.clone()));
        // work_dir == temp_dir, the same dir `req()` uses as cwd, so create/get
        // responses flag these sessions as default-workpath.
        let svc = TerminalService::new(Arc::new(MemRepo::default()), emitter, std::env::temp_dir());
        (svc, bc)
    }

    fn req(command: &str, args: &[&str]) -> CreateTerminalRequest {
        CreateTerminalRequest {
            name: None,
            cwd: std::env::temp_dir().to_string_lossy().into_owned(),
            command: command.into(),
            args: args.iter().map(|s| s.to_string()).collect(),
            env: None,
            backend: None,
            mode: None,
            cols: 80,
            rows: 24,
            defer_spawn: false,
            knowledge_base_ids: None,
        }
    }

    // --- In-memory knowledge repo + fixture ------------------------------

    #[derive(Default)]
    struct MemKbRepo {
        bases: Mutex<HashMap<String, nomifun_db::models::KnowledgeBaseRow>>,
        // Keyed by (target_kind, target_id) → (binding row, ordered kb_ids).
        // kb_ids live in the junction now, so the mock carries them alongside.
        bindings: Mutex<HashMap<(String, String), (nomifun_db::models::KnowledgeBindingRow, Vec<String>)>>,
        next_binding_id: std::sync::atomic::AtomicI64,
    }

    #[async_trait::async_trait]
    impl nomifun_db::IKnowledgeRepository for MemKbRepo {
        async fn insert_base(&self, row: &nomifun_db::models::KnowledgeBaseRow) -> Result<(), nomifun_db::DbError> {
            self.bases.lock().unwrap().insert(row.id.clone(), row.clone());
            Ok(())
        }
        async fn update_base(&self, row: &nomifun_db::models::KnowledgeBaseRow) -> Result<(), nomifun_db::DbError> {
            self.bases.lock().unwrap().insert(row.id.clone(), row.clone());
            Ok(())
        }
        async fn delete_base(&self, id: &str) -> Result<(), nomifun_db::DbError> {
            self.bases.lock().unwrap().remove(id);
            Ok(())
        }
        async fn get_base(&self, id: &str) -> Result<Option<nomifun_db::models::KnowledgeBaseRow>, nomifun_db::DbError> {
            Ok(self.bases.lock().unwrap().get(id).cloned())
        }
        async fn list_bases(&self) -> Result<Vec<nomifun_db::models::KnowledgeBaseRow>, nomifun_db::DbError> {
            Ok(self.bases.lock().unwrap().values().cloned().collect())
        }
        async fn get_binding(
            &self,
            target_kind: &str,
            target_id: &str,
        ) -> Result<Option<(nomifun_db::models::KnowledgeBindingRow, Vec<String>)>, nomifun_db::DbError> {
            Ok(self
                .bindings
                .lock()
                .unwrap()
                .get(&(target_kind.to_owned(), target_id.to_owned()))
                .cloned())
        }
        #[allow(clippy::too_many_arguments)]
        async fn set_binding(
            &self,
            target_kind: &str,
            target_id: &str,
            kb_ids: &[String],
            enabled: bool,
            writeback: bool,
            writeback_mode: &str,
            writeback_eagerness: &str,
            _channel_write_enabled: bool,
            updated_at: nomifun_common::TimestampMs,
        ) -> Result<i64, nomifun_db::DbError> {
            let key = (target_kind.to_owned(), target_id.to_owned());
            let mut bindings = self.bindings.lock().unwrap();
            // Preserve an existing binding_id on replace; allocate otherwise.
            let binding_id = bindings.get(&key).map(|(row, _)| row.binding_id).unwrap_or_else(|| {
                self.next_binding_id
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
                    + 1
            });
            let mut row = nomifun_db::models::KnowledgeBindingRow {
                binding_id,
                target_kind: target_kind.to_owned(),
                target_workpath: None,
                target_conv_id: None,
                target_term_id: None,
                target_companion_id: None,
                enabled,
                writeback,
                writeback_mode: writeback_mode.to_owned(),
                writeback_eagerness: writeback_eagerness.to_owned(),
                channel_write_enabled: _channel_write_enabled,
                updated_at,
            };
            match target_kind {
                "workpath" => row.target_workpath = Some(target_id.to_owned()),
                "conversation" => row.target_conv_id = target_id.parse::<i64>().ok(),
                "terminal" => row.target_term_id = target_id.parse::<i64>().ok(),
                "companion" => row.target_companion_id = Some(target_id.to_owned()),
                _ => {}
            }
            bindings.insert(key, (row, kb_ids.to_vec()));
            Ok(binding_id)
        }
        async fn delete_binding(&self, target_kind: &str, target_id: &str) -> Result<(), nomifun_db::DbError> {
            self.bindings
                .lock()
                .unwrap()
                .remove(&(target_kind.to_owned(), target_id.to_owned()));
            Ok(())
        }
        async fn list_bindings_using_kb(&self, _kb_id: &str) -> Result<Vec<nomifun_db::models::KnowledgeBindingRow>, nomifun_db::DbError> {
            Ok(vec![])
        }
        async fn list_knowledge_tags(&self) -> Result<Vec<nomifun_db::models::KnowledgeTagRow>, nomifun_db::DbError> {
            Ok(vec![])
        }
        async fn create_knowledge_tag(&self, _params: nomifun_db::models::CreateKnowledgeTagParams) -> Result<(), nomifun_db::DbError> {
            Ok(())
        }
        async fn update_knowledge_tag(&self, _key: &str, _params: nomifun_db::models::UpdateKnowledgeTagParams) -> Result<(), nomifun_db::DbError> {
            Ok(())
        }
        async fn delete_knowledge_tag(&self, _key: &str) -> Result<(), nomifun_db::DbError> {
            Ok(())
        }
    }

    /// A `KnowledgeService` over an in-memory repo with one registered base
    /// (`kb_test`) rooted at `kb_root`. The returned `TempDir` is the service
    /// data dir — keep it alive for the test's duration.
    fn knowledge_fixture(kb_root: &std::path::Path) -> (Arc<nomifun_knowledge::KnowledgeService>, tempfile::TempDir) {
        let data_dir = tempfile::TempDir::new().unwrap();
        let repo = MemKbRepo::default();
        repo.bases.lock().unwrap().insert(
            "kb_test".into(),
            nomifun_db::models::KnowledgeBaseRow {
                id: "kb_test".into(),
                name: "Domain Notes".into(),
                description: "test base".into(),
                root_path: kb_root.to_string_lossy().into_owned(),
                managed: true,
                extra: "{}".into(),
                created_at: 1,
                updated_at: 1,
                tags: None,
            },
        );
        let emitter = nomifun_knowledge::KnowledgeEventEmitter::new(Arc::new(CapturingBroadcaster::default()));
        (
            Arc::new(nomifun_knowledge::KnowledgeService::new(
                Arc::new(repo),
                data_dir.path(),
                emitter,
            )),
            data_dir,
        )
    }

    #[tokio::test]
    async fn create_with_kb_ids_binds_and_writes_readme() {
        let kb_root = tempfile::TempDir::new().unwrap();
        std::fs::write(kb_root.path().join("guide.md"), "# Guide\nbody").unwrap();
        let (ks, _data) = knowledge_fixture(kb_root.path());
        let (svc, _bc) = service();
        svc.with_knowledge_service(ks.clone());

        let cwd = tempfile::TempDir::new().unwrap();
        let mut request = req("cat", &[]);
        request.cwd = cwd.path().to_string_lossy().into_owned();
        request.knowledge_base_ids = Some(vec!["kb_test".into()]);
        let resp = svc.create("u", request).await.unwrap();

        // Create-time kb_ids persist an enabled binding under the session's
        // WORKPATH (spec §7) — the exact key the session-header KnowledgeControl
        // and the mount resolver read. (The test `work_dir` == temp_dir and the
        // cwd is under it, so this resolves to the default-workpath sentinel.)
        let wp_key = nomifun_knowledge::session_workpath_key(cwd.path(), &std::env::temp_dir());
        let binding = ks.get_binding("workpath", &wp_key).await.unwrap();
        assert!(binding.enabled, "create-time kb_ids must enable the binding");
        assert_eq!(binding.kb_ids, vec!["kb_test".to_owned()]);
        // The legacy per-session key must NOT be written — that mismatch is the
        // bug this fix closes (header/mount read workpath, create wrote terminal).
        let legacy = ks.get_binding("terminal", &resp.id.to_string()).await.unwrap();
        assert!(!legacy.enabled && legacy.kb_ids.is_empty(), "legacy (terminal, id) key must stay unbound");

        // The README contract is materialized inside the mount dir.
        let readme = cwd.path().join(".nomi").join("knowledge").join("README.md");
        assert!(readme.exists(), "README.md must be written at {readme:?}");
        let text = std::fs::read_to_string(&readme).unwrap();
        assert!(
            text.starts_with("# Knowledge bases"),
            "terminal README must be the standalone document, got: {}",
            &text[..text.len().min(80)]
        );
        assert!(text.contains("Domain Notes"));

        svc.kill(resp.id).await.ok();
    }

    /// A user-picked working directory (NOT under the managed work dir) binds
    /// under its normalized path key — the common real-world case, and exactly
    /// the key the session header reads back via `workpathKeyForTerminal`.
    #[tokio::test]
    async fn create_with_kb_ids_binds_under_custom_workpath() {
        let kb_root = tempfile::TempDir::new().unwrap();
        std::fs::write(kb_root.path().join("guide.md"), "# Guide\nbody").unwrap();
        let (ks, _data) = knowledge_fixture(kb_root.path());

        // Distinct managed work_dir; the cwd is a sibling temp dir, so it is NOT
        // a managed/default workspace and keys by its normalized path.
        let work_dir = tempfile::TempDir::new().unwrap();
        let cwd = tempfile::TempDir::new().unwrap();
        let bc = CapturingBroadcaster::default();
        let emitter = TerminalEventEmitter::new(Arc::new(bc.clone()));
        let svc = TerminalService::new(Arc::new(MemRepo::default()), emitter, work_dir.path().to_path_buf());
        svc.with_knowledge_service(ks.clone());

        let mut request = req("cat", &[]);
        request.cwd = cwd.path().to_string_lossy().into_owned();
        request.knowledge_base_ids = Some(vec!["kb_test".into()]);
        let resp = svc.create("u", request).await.unwrap();

        let key = nomifun_knowledge::workpath_key(&cwd.path().to_string_lossy());
        assert_ne!(key, "__default__", "a user-picked dir must get a dedicated workpath key");
        let binding = ks.get_binding("workpath", &key).await.unwrap();
        assert!(binding.enabled, "create-time kb_ids must enable the workpath binding");
        assert_eq!(binding.kb_ids, vec!["kb_test".to_owned()]);
        // End-to-end: the README contract materializes from the workpath binding.
        assert!(
            cwd.path().join(".nomi").join("knowledge").join("README.md").exists(),
            "the workpath binding must drive the mount + README"
        );

        svc.kill(resp.id).await.ok();
    }

    /// Read-modify-write: binding kb_ids at create time must NOT clobber the
    /// writeback ("回血") config already set on the workpath (e.g. configured
    /// from the homepage or the session header).
    #[tokio::test]
    async fn create_with_kb_ids_preserves_existing_workpath_writeback() {
        let kb_root = tempfile::TempDir::new().unwrap();
        std::fs::write(kb_root.path().join("guide.md"), "# Guide\nbody").unwrap();
        let (ks, _data) = knowledge_fixture(kb_root.path());
        let work_dir = tempfile::TempDir::new().unwrap();
        let cwd = tempfile::TempDir::new().unwrap();
        let bc = CapturingBroadcaster::default();
        let emitter = TerminalEventEmitter::new(Arc::new(bc.clone()));
        let svc = TerminalService::new(Arc::new(MemRepo::default()), emitter, work_dir.path().to_path_buf());
        svc.with_knowledge_service(ks.clone());

        let key = nomifun_knowledge::workpath_key(&cwd.path().to_string_lossy());
        // Pre-existing workpath binding with writeback ON and a different base.
        ks.set_binding(
            "workpath",
            &key,
            nomifun_knowledge::KnowledgeBinding {
                enabled: true,
                writeback: true,
                writeback_mode: "direct".into(),
                writeback_eagerness: "aggressive".into(),
                kb_ids: vec!["kb_old".into()],
                channel_write_enabled: false,
            },
        )
        .await
        .unwrap();

        let mut request = req("cat", &[]);
        request.cwd = cwd.path().to_string_lossy().into_owned();
        request.knowledge_base_ids = Some(vec!["kb_test".into()]);
        let resp = svc.create("u", request).await.unwrap();

        let binding = ks.get_binding("workpath", &key).await.unwrap();
        assert_eq!(binding.kb_ids, vec!["kb_test".to_owned()], "create selection replaces the base list");
        assert!(binding.writeback, "writeback flag must be preserved");
        assert_eq!(binding.writeback_mode, "direct", "writeback mode must be preserved");
        assert_eq!(binding.writeback_eagerness, "aggressive", "writeback eagerness must be preserved");

        svc.kill(resp.id).await.ok();
    }

    #[tokio::test]
    async fn relaunch_resyncs_knowledge_and_rewrites_readme() {
        let kb_root = tempfile::TempDir::new().unwrap();
        std::fs::write(kb_root.path().join("guide.md"), "# Guide\nbody").unwrap();
        let (ks, _data) = knowledge_fixture(kb_root.path());
        let (svc, _bc) = service();
        svc.with_knowledge_service(ks.clone());

        let cwd = tempfile::TempDir::new().unwrap();
        let mut request = req("cat", &[]);
        request.cwd = cwd.path().to_string_lossy().into_owned();
        let resp = svc.create("u", request).await.unwrap(); // no kb ids yet
        let readme = cwd.path().join(".nomi").join("knowledge").join("README.md");
        assert!(!readme.exists(), "no binding yet → no README");

        // Bind afterwards (the KnowledgeControl UI path), then relaunch in place
        // — the documented way for a binding change to take effect.
        ks.set_binding(
            "terminal",
            &resp.id.to_string(),
            nomifun_knowledge::KnowledgeBinding {
                enabled: true,
                writeback: false,
                writeback_mode: "staged".into(),
                writeback_eagerness: "conservative".into(),
                kb_ids: vec!["kb_test".into()],
                channel_write_enabled: false,
            },
        )
        .await
        .unwrap();
        svc.relaunch(resp.id).await.unwrap();
        assert!(readme.exists(), "relaunch must re-sync mounts and write the README");

        // Unbind, relaunch again: the mount engine clears the whole mount dir
        // (README included), so stale knowledge guidance never lingers.
        ks.set_binding(
            "terminal",
            &resp.id.to_string(),
            nomifun_knowledge::KnowledgeBinding::default(),
        )
        .await
        .unwrap();
        svc.relaunch(resp.id).await.unwrap();
        assert!(
            !readme.exists(),
            "relaunch after unbinding must sweep the README with the mounts"
        );

        svc.kill(resp.id).await.ok();
    }

    #[tokio::test]
    async fn create_with_kb_ids_without_knowledge_service_still_spawns() {
        let (svc, _bc) = service(); // knowledge service NOT wired
        let cwd = tempfile::TempDir::new().unwrap();
        let mut request = req("cat", &[]);
        request.cwd = cwd.path().to_string_lossy().into_owned();
        request.knowledge_base_ids = Some(vec!["kb_zzz".into()]);

        let resp = svc.create("u", request).await.unwrap();
        assert_eq!(resp.last_status, "running", "knowledge is best-effort, never blocks");
        assert!(!cwd.path().join(".nomi").join("knowledge").join("README.md").exists());
        svc.kill(resp.id).await.ok();
    }

    async fn wait_for<F: Fn() -> bool>(pred: F, ms: u64) -> bool {
        for _ in 0..(ms / 20).max(1) {
            if pred() {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        pred()
    }

    fn collect_output(bc: &CapturingBroadcaster) -> String {
        let evts = bc.events.lock().unwrap();
        let mut out = Vec::new();
        for e in evts.iter().filter(|e| e.name == "terminal.output") {
            if let Some(b64) = e.data.get("data_b64").and_then(|v| v.as_str()) {
                out.extend_from_slice(&BASE64.decode(b64).unwrap_or_default());
            }
        }
        String::from_utf8_lossy(&out).into_owned()
    }

    #[tokio::test]
    async fn spawn_echo_streams_output_and_exits() {
        let (svc, bc) = service();
        let resp = svc.create("u", req("printf", &["hi-there"])).await.unwrap();
        assert_eq!(resp.last_status, "running");

        let got = wait_for(|| collect_output(&bc).contains("hi-there"), 4000).await;
        assert!(
            got,
            "expected 'hi-there' in streamed output, got: {:?}",
            collect_output(&bc)
        );

        // Exit event eventually fires and status is persisted.
        let exited = wait_for(
            || bc.events.lock().unwrap().iter().any(|e| e.name == "terminal.exit"),
            4000,
        )
        .await;
        assert!(exited, "expected terminal.exit event");

        // The exit callback persists "exited" to the DB via the runtime handle
        // (regression: it previously ran on the reader thread and panicked).
        let mut persisted = false;
        for _ in 0..200 {
            if svc
                .get(resp.id)
                .await
                .map(|s| s.last_status == "exited")
                .unwrap_or(false)
            {
                persisted = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(persisted, "expected the session row to be marked exited");
    }

    #[tokio::test]
    async fn input_is_echoed_back_by_cat() {
        let (svc, bc) = service();
        let resp = svc.create("u", req("cat", &[])).await.unwrap();
        svc.input(resp.id, &BASE64.encode("ping\n")).await.unwrap();

        let got = wait_for(|| collect_output(&bc).contains("ping"), 4000).await;
        assert!(got, "cat should echo input, got: {:?}", collect_output(&bc));
        svc.kill(resp.id).await.unwrap();
    }

    #[tokio::test]
    async fn driver_subscribe_output_and_write_input() {
        use crate::driver::TerminalDriver;
        let (svc, _bc) = service();
        let resp = svc.create("u", req("cat", &[])).await.unwrap();

        // Subscribe via the driver seam, then write raw bytes; the echo must
        // arrive on the broadcast stream (independent of the WS path).
        let mut rx = svc
            .subscribe_output(resp.id)
            .expect("live session has an output stream");
        assert!(svc.is_alive(resp.id), "session should be alive");
        TerminalDriver::write_input(&svc, resp.id, b"hello-driver\n")
            .await
            .unwrap();

        let mut seen = Vec::new();
        let got = tokio::time::timeout(Duration::from_secs(4), async {
            loop {
                match rx.recv().await {
                    Ok(chunk) => {
                        seen.extend_from_slice(&chunk);
                        if String::from_utf8_lossy(&seen).contains("hello-driver") {
                            return true;
                        }
                    }
                    Err(_) => return false,
                }
            }
        })
        .await
        .unwrap_or(false);
        assert!(
            got,
            "driver output stream should deliver the echo, got: {:?}",
            String::from_utf8_lossy(&seen)
        );

        // describe + autowork round-trip through the driver.
        let desc = svc.describe(resp.id).await.unwrap().unwrap();
        assert_eq!(desc.last_status, "running");
        assert_eq!(desc.cwd, std::env::temp_dir().to_string_lossy());
        svc.write_autowork(resp.id, Some(r#"{"enabled":true,"tag":"t"}"#))
            .await
            .unwrap();
        assert_eq!(
            svc.read_autowork(resp.id).await.unwrap().as_deref(),
            Some(r#"{"enabled":true,"tag":"t"}"#)
        );

        // idmm round-trip through the driver (set, read, clear).
        svc.write_idmm(resp.id, Some(r#"{"enabled":true,"tier":"rule"}"#))
            .await
            .unwrap();
        assert_eq!(
            svc.read_idmm(resp.id).await.unwrap().as_deref(),
            Some(r#"{"enabled":true,"tier":"rule"}"#)
        );
        svc.write_idmm(resp.id, None).await.unwrap();
        assert_eq!(svc.read_idmm(resp.id).await.unwrap(), None);

        svc.kill(resp.id).await.unwrap();
    }

    #[tokio::test]
    async fn get_returns_scrollback_and_resize_persists() {
        let (svc, _bc) = service();
        let resp = svc.create("u", req("cat", &[])).await.unwrap();
        // cwd == work_dir (the fixture's temp_dir) → the derived flag is set
        // on both the create and GET responses.
        assert!(resp.is_default_workpath);
        svc.input(resp.id, &BASE64.encode("xyz\n")).await.unwrap();
        wait_for(|| true, 200).await;

        let got = svc.get(resp.id).await.unwrap();
        assert!(got.scrollback_b64.is_some());
        assert!(got.is_default_workpath);

        svc.resize(resp.id, 120, 40).await.unwrap();
        let after = svc.get(resp.id).await.unwrap();
        assert_eq!((after.cols, after.rows), (120, 40));
        svc.kill(resp.id).await.unwrap();
    }

    #[tokio::test]
    async fn reconcile_on_boot_marks_running_exited() {
        let (svc, _bc) = service();
        // `cat` stays alive, so its row stays `running` until reconciliation.
        let id = svc.create("u", req("cat", &[])).await.unwrap().id;

        let n = svc.reconcile_on_boot().await.unwrap();
        assert_eq!(n, 1, "the one running session must be reconciled");
        assert_eq!(
            svc.get(id).await.unwrap().last_status,
            "exited",
            "boot reconciliation flips ghost running → exited"
        );
        svc.kill(id).await.unwrap();
    }

    #[tokio::test]
    async fn get_replays_persisted_scrollback_after_process_exits() {
        let (svc, bc) = service();
        // Emits known output, then exits on its own.
        let id = svc.create("u", req("printf", &["restore-me"])).await.unwrap().id;

        // Exit drops the live handle and persists the final scrollback.
        assert!(wait_for(|| bc.events.lock().unwrap().iter().any(|e| e.name == "terminal.exit"), 4000).await);
        // `on_exit` persists on a spawned task — give it a beat to land.
        tokio::time::sleep(Duration::from_millis(250)).await;

        // No live handle now → `get` must fall back to the persisted snapshot.
        let resp = svc.get(id).await.unwrap();
        let b64 = resp.scrollback_b64.expect("persisted scrollback must be returned when not live");
        let bytes = BASE64.decode(b64).unwrap();
        assert!(
            String::from_utf8_lossy(&bytes).contains("restore-me"),
            "restored scrollback should contain the process output, got {:?}",
            String::from_utf8_lossy(&bytes)
        );
    }

    #[tokio::test]
    async fn flush_persists_dirty_live_scrollback() {
        let (svc, _bc) = service();
        let id = svc.create("u", req("cat", &[])).await.unwrap().id;
        // Feed a line; the PTY echoes it (and cat re-emits) → scrollback dirty.
        svc.input(id, &BASE64.encode("echoline\n")).await.unwrap();

        // Actually wait for the echoed bytes to land in the live scrollback
        // (the reader thread is async); `get` reads the live handle's buffer
        // without clearing the dirty flag, so the subsequent flush still fires.
        let mut landed = false;
        for _ in 0..100 {
            if let Some(b64) = svc.get(id).await.unwrap().scrollback_b64
                && String::from_utf8_lossy(&BASE64.decode(b64).unwrap()).contains("echoline")
            {
                landed = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(landed, "cat should echo the fed input into the live scrollback");

        svc.flush_dirty_scrollback().await;

        let persisted = svc.repo.load_scrollback(id).await.unwrap();
        let bytes = persisted.expect("a dirty live session must be flushed to the DB");
        assert!(
            String::from_utf8_lossy(&bytes).contains("echoline"),
            "flushed scrollback should contain the live output, got {:?}",
            String::from_utf8_lossy(&bytes)
        );
        svc.kill(id).await.unwrap();
    }

    #[tokio::test]
    async fn deferred_create_spawns_on_first_resize() {
        let (svc, _bc) = service();
        let mut r = req("cat", &[]);
        r.defer_spawn = true;
        let id = svc.create("u", r).await.unwrap().id;

        // Not spawned yet: no live PTY → no scrollback, and input fails (the
        // root-cause fix — `claude` never draws at 80×24 before the real size).
        assert!(svc.get(id).await.unwrap().scrollback_b64.is_none());
        assert!(matches!(
            svc.input(id, &BASE64.encode("x")).await.unwrap_err(),
            TerminalError::NotFound(_)
        ));

        // First resize spawns the PTY at the requested (real) size.
        svc.resize(id, 120, 40).await.unwrap();
        // Now live: input works and the persisted size is the resize size.
        svc.input(id, &BASE64.encode("hi\n")).await.unwrap();
        let got = svc.get(id).await.unwrap();
        assert_eq!((got.cols, got.rows), (120, 40), "deferred spawn must adopt the first-resize size");
        svc.kill(id).await.unwrap();
    }

    #[tokio::test]
    async fn non_deferred_create_spawns_immediately() {
        let (svc, _bc) = service();
        // `req` defaults `defer_spawn = false` — headless/cron behaviour unchanged.
        let id = svc.create("u", req("cat", &[])).await.unwrap().id;
        // Live without any resize: input succeeds immediately.
        svc.input(id, &BASE64.encode("hi\n")).await.unwrap();
        svc.kill(id).await.unwrap();
    }

    #[tokio::test]
    async fn unknown_id_is_not_found() {
        let (svc, _bc) = service();
        assert!(matches!(svc.get(999_999).await.unwrap_err(), TerminalError::NotFound(_)));
        assert!(matches!(
            svc.input(999_999, &BASE64.encode("x")).await.unwrap_err(),
            TerminalError::NotFound(_)
        ));
        assert!(matches!(
            svc.delete(999_999).await.unwrap_err(),
            TerminalError::Database(_)
        ));
    }

    #[tokio::test]
    async fn browse_workspace_lists_cwd_entries() {
        // A session whose cwd is a temp dir containing one file → the workspace
        // listing returns that file (server derives the root from the row's
        // cwd, never a client-supplied path).
        let (svc, _bc) = service();
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("hello.txt"), "hi").unwrap();
        let mut create = req("cat", &[]);
        create.cwd = dir.path().to_string_lossy().into_owned();
        create.defer_spawn = true; // no live PTY needed to list files
        let resp = svc.create("u", create).await.unwrap();

        let entries = svc.browse_workspace(resp.id, "", None).await.unwrap();
        assert!(
            entries.iter().any(|e| e.name == "hello.txt" && e.entry_type == "file"),
            "expected hello.txt in {entries:?}"
        );
    }

    #[tokio::test]
    async fn browse_workspace_unknown_id_is_not_found() {
        let (svc, _bc) = service();
        assert!(matches!(
            svc.browse_workspace(999_999, "", None).await.unwrap_err(),
            nomifun_common::AppError::NotFound(_)
        ));
    }

    #[tokio::test]
    async fn browse_workspace_rejects_parent_traversal() {
        // `../` must be rejected by list_workspace_level's `..` guard, surfacing
        // as a BadRequest (HTTP 400) — the path stays scoped to the cwd root.
        let (svc, _bc) = service();
        let dir = tempfile::tempdir().unwrap();
        let mut create = req("cat", &[]);
        create.cwd = dir.path().to_string_lossy().into_owned();
        create.defer_spawn = true;
        let resp = svc.create("u", create).await.unwrap();

        let err = svc.browse_workspace(resp.id, "../", None).await.unwrap_err();
        assert!(
            matches!(err, nomifun_common::AppError::BadRequest(_)),
            "`../` must map to BadRequest (400)"
        );
    }

    #[tokio::test]
    async fn invalid_base64_input_is_rejected() {
        let (svc, _bc) = service();
        let resp = svc.create("u", req("cat", &[])).await.unwrap();
        let err = svc.input(resp.id, "!!!not-base64!!!").await.unwrap_err();
        assert!(matches!(err, TerminalError::InvalidInput(_)));
        svc.kill(resp.id).await.unwrap();
    }

    #[tokio::test]
    async fn delete_removes_and_emits() {
        let (svc, bc) = service();
        let resp = svc.create("u", req("cat", &[])).await.unwrap();
        svc.delete(resp.id).await.unwrap();
        assert!(matches!(
            svc.get(resp.id).await.unwrap_err(),
            TerminalError::NotFound(_)
        ));
        assert!(bc.events.lock().unwrap().iter().any(|e| e.name == "terminal.removed"));
    }

    #[tokio::test]
    async fn relaunch_reuses_same_id_and_marks_running() {
        let (svc, bc) = service();
        // short-lived child so it exits, then relaunch in place.
        let resp = svc.create("u", req("printf", &["x"])).await.unwrap();
        let id = resp.id.clone();
        // wait until it exits
        let exited = wait_for(
            || bc.events.lock().unwrap().iter().any(|e| e.name == "terminal.exit"),
            4000,
        )
        .await;
        assert!(exited);

        let relaunched = svc.relaunch(id).await.unwrap();
        assert_eq!(relaunched.id, id, "relaunch must reuse the same session id");
        assert_eq!(relaunched.last_status, "running");
        assert!(
            bc.events.lock().unwrap().iter().any(|e| e.name == "terminal.updated"),
            "relaunch should emit terminal.updated, not create a new session"
        );
        svc.delete(id).await.ok();
    }

    /// Relaunching a RUNNING session must leave a fresh running PTY — the killed
    /// predecessor's exit callback (fired after EXIT_DRAIN_GRACE) must NOT tear
    /// down the replacement or mark the session exited. Regression: "重启"
    /// closed the terminal because that stale callback ran `live.remove` +
    /// status→exited unconditionally on the same id.
    #[tokio::test]
    async fn relaunch_running_session_survives_stale_exit_callback() {
        let (svc, _bc) = service();
        // A long-lived child (cat blocks reading the PTY) → genuinely running.
        let resp = svc.create("u", req("cat", &[])).await.unwrap();
        let id = resp.id;
        assert!(svc.live.contains_key(&id), "freshly created session must be live");

        svc.relaunch(id).await.unwrap();

        // Wait well past EXIT_DRAIN_GRACE (~120ms) so the OLD child's exit
        // callback has definitely fired before we assert.
        tokio::time::sleep(Duration::from_millis(400)).await;

        assert!(
            svc.live.contains_key(&id),
            "the fresh PTY must remain live after the predecessor's stale exit callback"
        );
        let got = svc.get(id).await.unwrap();
        assert_eq!(got.last_status, "running", "relaunch must not leave the session exited");

        svc.delete(id).await.ok();
    }

    #[tokio::test]
    async fn relaunch_unknown_is_not_found() {
        let (svc, _bc) = service();
        assert!(matches!(
            svc.relaunch(999_999).await.unwrap_err(),
            TerminalError::NotFound(_)
        ));
    }

    #[tokio::test]
    async fn with_knowledge_mcp_config_is_stored() {
        let (svc, _bc) = service();
        let cfg = nomifun_api_types::KnowledgeMcpConfig {
            port: 51123,
            token: "tok".into(),
            binary_path: "/opt/nomi/nomicore".into(),
        };
        svc.with_knowledge_mcp_config(cfg.clone(), std::env::temp_dir().join("nomi-term-mcp"));
        assert_eq!(svc.knowledge_mcp_config().map(|c| c.port), Some(51123));
    }

    /// Three-way gate of `build_enhancement`:
    /// 1. empty kb_ids → no enhancement (no MCP server).
    /// 2. kb_ids present but no `knowledge_mcp_config` wired → no enhancement.
    /// 3. kb_ids present AND config wired → exactly one `McpServerSpec` with
    ///    PORT+TOKEN env (no KB_IDS — scope resolved at runtime by bridge's cwd).
    #[tokio::test]
    async fn build_enhancement_three_way_gate() {
        use nomifun_api_types::KnowledgeMcpConfig as K;
        let (svc, _bc) = service();

        // Case 1: empty kb_ids → always empty regardless of config.
        svc.with_knowledge_mcp_config(
            K { port: 51123, token: "tok".into(), binary_path: "nomicore".into() },
            std::env::temp_dir(),
        );
        let enh = svc.build_enhancement(&[], 1);
        assert!(enh.mcp_servers.is_empty(), "empty kb_ids must yield no MCP servers");

        // Case 2: kb_ids present but NO knowledge_mcp_config wired → empty.
        let (svc2, _bc2) = service(); // fresh service, config NOT wired
        let enh = svc2.build_enhancement(&["kb_1".into()], 1);
        assert!(enh.mcp_servers.is_empty(), "no config wired must yield no MCP servers");

        // Case 3: kb_ids present AND config wired → one McpServerSpec.
        let enh = svc.build_enhancement(&["kb_1".into(), "kb_2".into()], 1);
        assert_eq!(enh.mcp_servers.len(), 1, "expected exactly one MCP server spec");
        let spec = &enh.mcp_servers[0];
        assert_eq!(spec.name, K::SERVER_NAME);
        assert_eq!(spec.args, vec!["mcp-knowledge-stdio".to_owned()]);
        assert_eq!(spec.env.get(K::ENV_PORT).map(String::as_str), Some("51123"));
        assert_eq!(spec.env.get(K::ENV_TOKEN).map(String::as_str), Some("tok"));
        // ENV_KB_IDS must NOT be present — scope is resolved at runtime by cwd.
        assert!(
            spec.env.get(K::ENV_KB_IDS).is_none(),
            "ENV_KB_IDS must not be baked into the bridge env (runtime cwd scope)"
        );
    }

    #[tokio::test]
    async fn update_meta_renames_and_pins_and_emits() {
        let (svc, bc) = service();
        let resp = svc.create("u", req("cat", &[])).await.unwrap();
        let updated = svc
            .update_meta(resp.id, Some("Renamed".into()), Some(true))
            .await
            .unwrap();
        assert_eq!(updated.name, "Renamed");
        assert!(updated.pinned);
        assert!(bc.events.lock().unwrap().iter().any(|e| e.name == "terminal.updated"));
        // blank name is ignored (keeps prior)
        let again = svc.update_meta(resp.id, Some("   ".into()), None).await.unwrap();
        assert_eq!(again.name, "Renamed");
        svc.delete(resp.id).await.ok();
    }

    #[tokio::test]
    async fn spawn_injects_knowledge_mcp_for_claude_when_kb_ids_present() {
        use crate::enhance::{apply_enhancement, McpServerSpec, TerminalLaunchEnhancement};
        use std::collections::HashMap;
        // Pure unit-level verification of the injector contract: construct an
        // enhancement → claude argv contains --mcp-config.
        let dir = tempfile::TempDir::new().unwrap();
        let cfg = nomifun_api_types::KnowledgeMcpConfig {
            port: 9,
            token: "t".into(),
            binary_path: "nomicore".into(),
        };
        let enh = TerminalLaunchEnhancement {
            mcp_servers: vec![McpServerSpec {
                name: nomifun_api_types::KnowledgeMcpConfig::SERVER_NAME.into(),
                command: cfg.binary_path.clone(),
                args: vec!["mcp-knowledge-stdio".into()],
                env: HashMap::from([
                    (nomifun_api_types::KnowledgeMcpConfig::ENV_PORT.into(), "9".into()),
                    (nomifun_api_types::KnowledgeMcpConfig::ENV_TOKEN.into(), "t".into()),
                ]),
            }],
            lifecycle: None,
        };
        let (argv, _env) = apply_enhancement("claude", vec![], &enh, dir.path(), None);
        assert!(argv.iter().any(|a| a == "--mcp-config"));
    }

    #[tokio::test]
    async fn subscribe_lifecycle_none_without_server() {
        // Without a lifecycle server wired, the trait method must return None.
        let (svc, _bc) = service();
        let driver: &dyn TerminalDriver = &svc;
        assert!(driver.subscribe_lifecycle(1).is_none());
    }

    #[tokio::test]
    async fn subscribe_lifecycle_some_and_receives_event() {
        use crate::lifecycle::{TerminalLifecycleServer, LifecycleKind};

        let (svc, _bc) = service();
        let srv = Arc::new(TerminalLifecycleServer::start().await.expect("start lifecycle server"));
        svc.with_terminal_lifecycle(srv.clone(), "nomicore".into());

        // Via the trait object, subscribe_lifecycle should now return Some.
        let driver: &dyn TerminalDriver = &svc;
        let mut rx = driver.subscribe_lifecycle(42).expect("must be Some when lifecycle is wired");

        // Broadcast an event through the lifecycle server.
        let url = format!("http://127.0.0.1:{}/hook", srv.http_port());
        let body = serde_json::json!({
            "terminal_id": 42,
            "kind": "turn_end",
            "payload": {"last_assistant_message": "hello"}
        });
        let client = reqwest::Client::new();
        let resp = client
            .post(&url)
            .json(&body)
            .bearer_auth(srv.auth_token())
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);

        // The subscriber should receive the event.
        let ev = tokio::time::timeout(Duration::from_secs(2), rx.recv())
            .await
            .expect("timeout waiting for lifecycle event")
            .expect("recv error");
        assert_eq!(ev.terminal_id, 42);
        assert_eq!(ev.kind, LifecycleKind::TurnEnd);
    }

    #[tokio::test]
    async fn with_requirement_mcp_config_is_stored() {
        let (svc, _bc) = service();
        let cfg = nomifun_api_types::RequirementMcpConfig {
            port: 52222,
            token: "req-tok".into(),
            binary_path: "/opt/nomi/nomicore".into(),
        };
        svc.with_requirement_mcp_config(cfg.clone());
        let stored = svc.requirement_mcp_config().expect("must be Some after wiring");
        assert_eq!(stored.port, 52222);
        assert_eq!(stored.token, "req-tok");
    }

    /// `build_enhancement` with ONLY requirement_mcp_config wired (no kb_ids):
    /// must produce a non-empty enhancement with the requirement MCP server spec.
    #[tokio::test]
    async fn build_enhancement_requirement_only_no_kb() {
        use nomifun_api_types::RequirementMcpConfig as R;
        let (svc, _bc) = service();
        svc.with_requirement_mcp_config(R {
            port: 9876,
            token: "rtok".into(),
            binary_path: "/usr/bin/nomicore".into(),
        });

        // No kb_ids, terminal_id = 42
        let enh = svc.build_enhancement(&[], 42);
        assert!(!enh.is_empty(), "requirement MCP alone must produce a non-empty enhancement");
        assert_eq!(enh.mcp_servers.len(), 1);
        let spec = &enh.mcp_servers[0];
        assert_eq!(spec.name, R::SERVER_NAME);
        assert_eq!(spec.command, "/usr/bin/nomicore");
        assert_eq!(spec.args, vec!["mcp-requirement-stdio".to_owned()]);
        assert_eq!(spec.env.get(R::ENV_PORT).map(String::as_str), Some("9876"));
        assert_eq!(spec.env.get(R::ENV_TOKEN).map(String::as_str), Some("rtok"));
        assert_eq!(
            spec.env.get(R::ENV_CONVERSATION_ID).map(String::as_str),
            Some("42"),
            "ENV_CONVERSATION_ID must carry the terminal_id"
        );
        assert_eq!(
            spec.env.get(R::ENV_OWNER_KIND).map(String::as_str),
            Some("terminal"),
            "ENV_OWNER_KIND must be 'terminal'"
        );
    }

    /// When both knowledge AND requirement MCP are wired, `build_enhancement`
    /// produces BOTH server specs.
    #[tokio::test]
    async fn build_enhancement_knowledge_and_requirement_coexist() {
        use nomifun_api_types::{KnowledgeMcpConfig as K, RequirementMcpConfig as R};
        let (svc, _bc) = service();
        svc.with_knowledge_mcp_config(
            K { port: 111, token: "ktok".into(), binary_path: "nomicore".into() },
            std::env::temp_dir(),
        );
        svc.with_requirement_mcp_config(R {
            port: 222,
            token: "rtok".into(),
            binary_path: "nomicore".into(),
        });

        let enh = svc.build_enhancement(&["kb_x".into()], 7);
        assert_eq!(enh.mcp_servers.len(), 2, "both knowledge + requirement MCP servers");
        let names: Vec<&str> = enh.mcp_servers.iter().map(|s| s.name.as_str()).collect();
        assert!(names.contains(&K::SERVER_NAME));
        assert!(names.contains(&R::SERVER_NAME));
    }

    /// `apply_enhancement` renders the requirement MCP for `claude` (--mcp-config)
    /// but does NOT render it for `/bin/bash` (unknown CLI → honest no-op).
    #[tokio::test]
    async fn apply_enhancement_requirement_renders_for_claude_not_shell() {
        use crate::enhance::{apply_enhancement, McpServerSpec, TerminalLaunchEnhancement};
        use nomifun_api_types::RequirementMcpConfig as R;
        use std::collections::HashMap;

        let dir = tempfile::TempDir::new().unwrap();
        let enh = TerminalLaunchEnhancement {
            mcp_servers: vec![McpServerSpec {
                name: R::SERVER_NAME.into(),
                command: "/opt/nomi/nomicore".into(),
                args: vec!["mcp-requirement-stdio".into()],
                env: HashMap::from([
                    (R::ENV_PORT.into(), "9876".into()),
                    (R::ENV_TOKEN.into(), "rtok".into()),
                    (R::ENV_CONVERSATION_ID.into(), "42".into()),
                    (R::ENV_OWNER_KIND.into(), "terminal".into()),
                ]),
            }],
            lifecycle: None,
        };

        // claude → --mcp-config present (renders MCP)
        let (argv, _env) = apply_enhancement("claude", vec![], &enh, dir.path(), None);
        assert!(argv.iter().any(|a| a == "--mcp-config"), "claude must get --mcp-config");
        // Verify the mcp.json contains the requirement server
        let mcp_path = argv.iter().position(|a| a == "--mcp-config").map(|i| &argv[i + 1]).unwrap();
        let doc: serde_json::Value = serde_json::from_slice(&std::fs::read(mcp_path).unwrap()).unwrap();
        assert!(doc["mcpServers"][R::SERVER_NAME].is_object(), "mcp.json must contain the requirement server");
        assert_eq!(doc["mcpServers"][R::SERVER_NAME]["env"][R::ENV_OWNER_KIND], "terminal");

        // codex → renders via -c overrides
        let (argv, _env) = apply_enhancement("codex", vec![], &enh, dir.path(), None);
        let joined = argv.join(" ");
        assert!(joined.contains(&format!("mcp_servers.{}.command=", R::SERVER_NAME)));

        // unknown CLI (bash) → no injection (honest)
        let (argv, _env) = apply_enhancement("/bin/bash", vec!["-l".into()], &enh, dir.path(), None);
        assert_eq!(argv, vec!["-l".to_owned()], "/bin/bash must get NO MCP injection");
    }
}

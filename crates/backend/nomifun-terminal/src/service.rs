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
    env.entry("TERM".to_owned())
        .or_insert_with(|| "xterm-256color".to_owned());
    env.entry("COLORTERM".to_owned())
        .or_insert_with(|| "truecolor".to_owned());
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

/// ANSI-stripped tail of a terminal's scrollback, for read-back by agents.
#[derive(Debug, Clone)]
pub struct TerminalOutputTail {
    /// Human-readable text (control/escape sequences removed).
    pub text: String,
    /// True when older output was dropped to fit `max_bytes`.
    pub truncated: bool,
    /// The session's status: "running" | "exited" | "error".
    pub status: String,
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
    terminal_lifecycle:
        Arc<std::sync::RwLock<Option<Arc<crate::lifecycle::TerminalLifecycleServer>>>>,
    /// Absolute path to the backend binary, used in lifecycle hook commands.
    lifecycle_binary_path: Arc<std::sync::RwLock<Option<String>>>,
    /// Late-wired LLM completer for auto-titling agent (claude/codex) sessions
    /// from their first turn. `None` → titles fall back to the user's first input
    /// line (no LLM); the feature never hard-depends on a provider being wired.
    title_completer: Arc<std::sync::RwLock<Option<Arc<dyn crate::title::TerminalTitleCompleter>>>>,
    /// Per-terminal once-guard for auto-titling: a key is claimed by whichever of
    /// the first-input (shell) / first-TurnEnd (agent) seams fires first, so a
    /// title is generated at most once per session.
    titled: Arc<DashMap<i64, ()>>,
    /// Accumulates the FIRST line of user input per terminal (until newline / a
    /// 200-char cap) — the title source. Dropped once a title is set.
    first_input: Arc<DashMap<i64, String>>,
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
            mcp_config_dir: Arc::new(std::sync::RwLock::new(
                std::env::temp_dir().join("nomi-terminal-mcp"),
            )),
            mcp_endpoints_path: Arc::new(std::sync::RwLock::new(None)),
            terminal_lifecycle: Arc::new(std::sync::RwLock::new(None)),
            lifecycle_binary_path: Arc::new(std::sync::RwLock::new(None)),
            title_completer: Arc::new(std::sync::RwLock::new(None)),
            titled: Arc::new(DashMap::new()),
            first_input: Arc::new(DashMap::new()),
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
        self.knowledge_mcp_config
            .read()
            .ok()
            .and_then(|g| g.clone())
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
        self.requirement_mcp_config
            .read()
            .ok()
            .and_then(|g| g.clone())
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

    /// Late-wire the auto-title LLM completer (interior mutable, same slot pattern
    /// as the other `with_*` setters). `None` keeps the fallback-only behaviour.
    pub fn with_title_completer(&self, completer: Arc<dyn crate::title::TerminalTitleCompleter>) {
        if let Ok(mut g) = self.title_completer.write() {
            *g = Some(completer);
        }
    }

    /// Build the launch enhancement for a spawn: knowledge_search MCP when bases
    /// are mounted AND the MCP server is wired; requirement MCP when the requirement
    /// server is wired (unconditional — scoped by terminal_id + owner_kind);
    /// lifecycle hooks when the lifecycle server is wired. Empty otherwise (honest
    /// no-op).
    fn build_enhancement(
        &self,
        kb_ids: &[String],
        terminal_id: i64,
    ) -> crate::enhance::TerminalLaunchEnhancement {
        let mut enh = crate::enhance::TerminalLaunchEnhancement::default();
        if !kb_ids.is_empty()
            && let Some(cfg) = self.knowledge_mcp_config()
        {
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
                let binary_path = self
                    .lifecycle_binary_path
                    .read()
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
        let base = self
            .mcp_config_dir
            .read()
            .map(|g| g.clone())
            .unwrap_or_else(|_| std::env::temp_dir());
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
        if let Some(kb_ids) = req
            .knowledge_base_ids
            .as_ref()
            .filter(|ids| !ids.is_empty())
        {
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
            info!(
                terminal_id = id,
                "terminal session created (spawn deferred to first resize)"
            );
            return Ok(resp);
        }

        let kb_ids = self
            .sync_knowledge_workspace(id, &req.cwd, &req.command, &req.args)
            .await;

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
                crate::enhance::apply_enhancement(
                    &program,
                    resolved_args,
                    &enh,
                    &session_dir,
                    backend,
                )
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
        let epoch = self
            .next_epoch
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
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
            if live_exit
                .remove_if(&id, |_, h| h.epoch() == epoch)
                .is_none()
            {
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

        // Plan-2 lifecycle consumer: subscribe to this terminal's lifecycle
        // events. On the FIRST `TurnEnd` of an agent session, auto-title from the
        // assistant's first message (prefixed with the user's first prompt, if
        // captured) via the wired LLM completer.
        if let Some(mut rx) = self.subscribe_lifecycle(id) {
            let svc = self.clone();
            tokio::spawn(async move {
                let mut titled_fired = false;
                loop {
                    match rx.recv().await {
                        Ok(ev) => {
                            info!(terminal_id = ev.terminal_id, kind = ?ev.kind, "terminal lifecycle event");
                            if !titled_fired && ev.kind == crate::lifecycle::LifecycleKind::TurnEnd
                            {
                                titled_fired = true;
                                let assistant = ev
                                    .payload
                                    .get("last_assistant_message")
                                    .and_then(|v| v.as_str())
                                    .unwrap_or("")
                                    .to_owned();
                                let prompt = svc
                                    .first_input
                                    .get(&id)
                                    .map(|v| v.clone())
                                    .unwrap_or_default();
                                let content =
                                    if !assistant.trim().is_empty() && !prompt.trim().is_empty() {
                                        format!("用户首条输入：{prompt}\n助手回复：{assistant}")
                                    } else if !assistant.trim().is_empty() {
                                        assistant
                                    } else {
                                        prompt
                                    };
                                svc.maybe_autotitle(id, Some(content)).await;
                            }
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
            warn!(
                terminal_id = id,
                "knowledge_base_ids given but no knowledge service is wired — skipping binding"
            );
            return;
        };
        // Use the SAME key derivation as `sync_knowledge_workspace` so the write
        // lands on exactly the row the mount + header read back.
        let wp_key =
            nomifun_knowledge::session_workpath_key(std::path::Path::new(cwd), &self.work_dir);
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
    async fn sync_knowledge_workspace(
        &self,
        id: i64,
        cwd: &str,
        command: &str,
        args: &[String],
    ) -> Vec<String> {
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
                // The same scoped MCP bridge that exposes knowledge_search also
                // exposes knowledge_write, so point the model at the tool (not
                // the file-write prose) when write-back is on and the tool will
                // actually be injected — mirroring the ACP assembler. `mounts`
                // is already non-empty here (early return above).
                has_write_tool: tool_available && outcome.writeback,
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
        Ok(rows
            .iter()
            .map(|r| row_to_response(r, None, &self.work_dir))
            .collect())
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
            None => self
                .repo
                .load_scrollback(id)
                .await?
                .map(|b| BASE64.encode(b)),
        };
        Ok(row_to_response(&row, scrollback, &self.work_dir))
    }

    /// Read the terminal's scrollback as ANSI-stripped text, keeping at most
    /// `max_bytes` from the TAIL. The terminal analogue of a conversation's
    /// transcript read-back — what an agent uses to SEE a command's result.
    pub async fn read_output_tail(
        &self,
        id: i64,
        max_bytes: usize,
    ) -> Result<TerminalOutputTail, TerminalError> {
        let resp = self.get(id).await?;
        let raw = resp
            .scrollback_b64
            .and_then(|b64| BASE64.decode(b64).ok())
            .unwrap_or_default();
        let text = crate::ansi::strip_ansi(&raw);
        let (tail, truncated) = tail_on_char_boundary(&text, max_bytes);
        Ok(TerminalOutputTail {
            text: tail,
            truncated,
            status: resp.last_status,
        })
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
            .map_err(|e| {
                nomifun_common::AppError::Internal(format!("Failed to load terminal session: {e}"))
            })?
            .ok_or_else(|| {
                nomifun_common::AppError::NotFound(format!("Terminal session '{id}' not found"))
            })?;
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
            info!(
                reconciled = n,
                "terminal boot reconciliation: ghost 'running' sessions marked exited"
            );
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
        // Capture the first input line for auto-titling (cheap no-op once titled).
        self.capture_first_input(id, &bytes);
        Ok(())
    }

    /// Submit a block of text to a terminal so it EXECUTES (as if typed + Enter).
    /// Resolves the target's agent family from its stored command/args/backend to
    /// choose the correct submit sequence (bracketed-paste + separated CR for
    /// agent TUIs, raw + CR for single lines / shells). Uses the raw PTY write
    /// path — this is deliberate driving, so it does NOT arm IDMM supervision or
    /// auto-title the way `input` (user typing) does. `Err(NotFound)` if not live.
    pub async fn submit_text(&self, id: i64, text: &str) -> Result<(), TerminalError> {
        if !self.live.contains_key(&id) {
            return Err(TerminalError::NotFound(id.to_string()));
        }
        let is_agent = match self.describe(id).await? {
            Some(d) => {
                let (program, prog_args) = crate::types::resolve_command(&d.command, &d.args);
                crate::enhance::resolve_agent_family(&program, &prog_args, d.backend.as_deref())
                    .is_some()
            }
            None => false,
        };
        match crate::submit::encode_submit_chunks(text, is_agent) {
            crate::submit::SubmitChunks::Single(bytes) => self.write_input(id, &bytes).await,
            crate::submit::SubmitChunks::PasteThenCr { paste, cr } => {
                self.write_input(id, &paste).await?;
                tokio::time::sleep(crate::submit::TERMINAL_SUBMIT_DELAY).await;
                self.write_input(id, &cr).await
            }
        }
    }

    /// Wait for a terminal turn to settle after a submit. Agent CLIs with
    /// lifecycle hooks (claude/codex) resolve via the structured `TurnEnd` event;
    /// shells and gemini fall back to an output-quiescence window
    /// (`IDLE_SETTLE_WINDOW`). Never dresses `Idle` up as definitive completion.
    pub async fn await_turn_settle(
        &self,
        id: i64,
        timeout: std::time::Duration,
    ) -> crate::submit::SettleReason {
        use crate::submit::SettleReason;

        let desc = match self.describe(id).await {
            Ok(Some(d)) => d,
            _ => return SettleReason::Exited,
        };
        let lifecycle_capable = crate::enhance::terminal_autowork_capable(
            &desc.command,
            &desc.args,
            desc.backend.as_deref(),
        );

        if lifecycle_capable {
            if let Some(mut rx) = self.subscribe_lifecycle(id) {
                let mut tick = tokio::time::interval(std::time::Duration::from_secs(2));
                tick.tick().await; // consume the immediate first tick
                let fut = async {
                    loop {
                        tokio::select! {
                            _ = tick.tick() => {
                                // The lifecycle channel is owned by
                                // TerminalLifecycleServer for the whole app lifetime
                                // (not tied to the PTY), so a dead PTY never closes it
                                // and `Closed` effectively never fires. Poll liveness so
                                // a crashed/killed agent terminal reports Exited promptly
                                // instead of riding the full caller timeout to a
                                // dishonest Timeout. Mirrors AutoWork's
                                // `wait_terminal_turn_end`.
                                if !self.is_alive(id) {
                                    return SettleReason::Exited;
                                }
                            }
                            ev = rx.recv() => {
                                match ev {
                                    Ok(event) if event.kind == crate::lifecycle::LifecycleKind::TurnEnd => {
                                        return SettleReason::TurnEnd;
                                    }
                                    Ok(_) => continue,
                                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                                    Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                                        return SettleReason::Exited;
                                    }
                                }
                            }
                        }
                    }
                };
                return tokio::time::timeout(timeout, fut)
                    .await
                    .unwrap_or(SettleReason::Timeout);
            }
        }

        // Idle-quiescence fallback: reset a short quiet-timer on every output
        // chunk; if it elapses, presume settled.
        let Some(mut out_rx) = self.subscribe_output(id) else {
            return SettleReason::Exited;
        };
        let overall = tokio::time::sleep(timeout);
        tokio::pin!(overall);
        loop {
            let quiet = tokio::time::sleep(crate::submit::IDLE_SETTLE_WINDOW);
            tokio::select! {
                _ = &mut overall => return SettleReason::Timeout,
                _ = quiet => return SettleReason::Idle,
                r = out_rx.recv() => match r {
                    Ok(_) => continue,
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                        return SettleReason::Exited;
                    }
                }
            }
        }
    }

    /// Accumulate the first line of user input for a session, then auto-title
    /// from it. Fires for ALL sessions (shell AND agent CLIs): titling from the
    /// user's first input line is reliable and immediate, independent of the
    /// agent lifecycle hook (which may not fire / needs a configured provider).
    /// For agent sessions this input fires before the assistant replies, so it
    /// wins the once-guard over the (best-effort) TurnEnd LLM path. Cheap no-op
    /// once a title has been claimed.
    fn capture_first_input(&self, id: i64, bytes: &[u8]) {
        if self.titled.contains_key(&id) {
            return;
        }
        // Enter is detected on the RAW bytes (a TUI submits with \r, which
        // strip_ansi drops). The captured TEXT is strip_ansi'd so the mouse
        // reports / focus events / cursor-key sequences a TUI like claude makes
        // xterm send are never accumulated — they are not typed text. strip_ansi
        // keeps \n + printable and removes \r and all other C0 controls.
        let had_newline = bytes.iter().any(|&b| b == b'\r' || b == b'\n');
        let cleaned = crate::ansi::strip_ansi(bytes);
        {
            let mut buf = self.first_input.entry(id).or_default();
            for ch in cleaned.chars() {
                if ch == '\n' {
                    break;
                }
                if buf.chars().count() >= 200 {
                    break;
                }
                buf.push(ch);
            }
        }
        if !had_newline {
            return;
        }
        // Skip a bare Enter (empty first line): keep waiting for real input so the
        // one-shot trigger isn't wasted on an empty title.
        let first_line_empty = self
            .first_input
            .get(&id)
            .map(|v| v.trim().is_empty())
            .unwrap_or(true);
        if first_line_empty {
            return;
        }
        let svc = self.clone();
        tokio::spawn(async move {
            svc.maybe_autotitle(id, None).await;
        });
    }

    /// Generate a session title at most once, without clobbering a manual rename.
    ///
    /// `llm_source` is the rich content (agent first-turn text) to summarize via
    /// the wired completer; `None`, no completer, or a failed/empty completion
    /// falls back to the captured first input line. Guards: (1) a per-terminal
    /// once-claim, and (2) the row's name must still equal the mechanical
    /// `default_name` — if the user (or a prior auto-title) already changed it,
    /// we never overwrite. Best-effort: every failure path only logs.
    async fn maybe_autotitle(&self, id: i64, llm_source: Option<String>) {
        // (1) Atomic once-claim: the first of the input/TurnEnd seams wins.
        if self.titled.insert(id, ()).is_some() {
            return;
        }
        // (2) Don't clobber a custom name (manual rename, create-time name, or a
        // command that isn't the mechanical default).
        let Ok(Some(row)) = self.repo.get_by_id(id).await else {
            return;
        };
        if row.name != default_name(&row.command, row.backend.as_deref()) {
            self.first_input.remove(&id);
            return;
        }

        let completer = self.title_completer.read().ok().and_then(|g| g.clone());
        let first_input = self
            .first_input
            .get(&id)
            .map(|v| v.clone())
            .unwrap_or_default();

        // Prefer the LLM summary; fall back to the first input line.
        let mut title = String::new();
        if let (Some(c), Some(src)) = (completer.as_ref(), llm_source.as_ref()) {
            let src = src.chars().take(2000).collect::<String>();
            if !src.trim().is_empty() {
                match c.summarize(&src).await {
                    Ok(t) => title = crate::title::clamp_title(&t, crate::title::TITLE_MAX_CHARS),
                    Err(e) => {
                        warn!(terminal_id = id, error = %e, "auto-title LLM failed; using fallback")
                    }
                }
            }
        }
        if title.is_empty() {
            title = crate::title::fallback_title(&first_input, crate::title::TITLE_MAX_CHARS);
        }
        self.first_input.remove(&id);

        if title.is_empty() {
            // Nothing usable yet — release the once-guard so a later, real input
            // can try again (never permanently block titling on a junk first line).
            self.titled.remove(&id);
            return;
        }
        if title == row.name {
            return;
        }
        info!(terminal_id = id, title = %title, "auto-title: setting terminal session title");
        if let Err(e) = self.update_meta(id, Some(title), None).await {
            warn!(terminal_id = id, error = %e, "failed to persist auto-generated terminal title");
        }
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
        let env: Option<HashMap<String, String>> = row
            .env
            .as_deref()
            .and_then(|s| serde_json::from_str(s).ok());
        let kb_ids = self
            .sync_knowledge_workspace(id, &row.cwd, &row.command, &args)
            .await;
        self.spawn_pty(
            id,
            &row.command,
            &args,
            &row.cwd,
            env,
            cols,
            rows,
            kb_ids,
            row.backend.as_deref(),
        )?;
        self.repo.update_size(id, cols as i64, rows as i64).await?;
        self.arm_supervision(id);
        info!(
            terminal_id = id,
            cols, rows, "deferred terminal spawned on first resize"
        );
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
        // Drop per-session auto-title bookkeeping.
        self.titled.remove(&id);
        self.first_input.remove(&id);
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
        let hooks: Vec<Arc<dyn OnTerminalDelete>> = self
            .delete_hooks
            .read()
            .map(|guard| guard.clone())
            .unwrap_or_default();
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
        let env: Option<HashMap<String, String>> = row
            .env
            .as_deref()
            .and_then(|s| serde_json::from_str(s).ok());
        // Re-sync knowledge mounts + README on every relaunch: this is the
        // documented moment a binding change (via KnowledgeControl) takes
        // effect for a terminal session.
        let kb_ids = self
            .sync_knowledge_workspace(id, &row.cwd, &row.command, &args)
            .await;
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

    /// Fall back to a clean login shell **in place**: kill the (possibly wedged)
    /// current child and spawn the platform shell for the SAME session id, then
    /// rewrite the stored launch identity to the shell sentinel so the session is
    /// permanently a shell (a later restart / boot-reconcile relaunches a shell,
    /// not the dead agent CLI, and the mechanical name becomes `Shell`).
    ///
    /// This is the escape hatch for a claude/codex TUI that left the terminal
    /// garbled and unresponsive: the user can always get back to a usable shell
    /// without the dead-page/disabled-composer state. Structurally identical to
    /// [`relaunch`] (same id, fresh epoch, status→running, emit `terminal.updated`
    /// which re-enables the frontend composer) — only the launch target differs.
    pub async fn relaunch_as_shell(
        &self,
        id: i64,
    ) -> Result<TerminalSessionResponse, TerminalError> {
        let row = self
            .repo
            .get_by_id(id)
            .await?
            .ok_or_else(|| TerminalError::NotFound(id.to_string()))?;

        // Tear down any still-running (or wedged) PTY for this id first.
        if let Some((_, handle)) = self.live.remove(&id) {
            let _ = handle.kill();
        }
        self.pending_spawn.remove(&id);

        // Persist the shell identity BEFORE spawning so a crash mid-relaunch (or a
        // later boot-reconcile) still relaunches a shell, never the dead agent CLI.
        self.repo
            .update_command(id, crate::types::SHELL_SENTINEL, "[]", None)
            .await?;

        // Re-sync knowledge mounts for the cwd (same contract as `relaunch`); a
        // shell never gets MCP/tool injection (apply_enhancement no-ops for it).
        let kb_ids = self
            .sync_knowledge_workspace(id, &row.cwd, crate::types::SHELL_SENTINEL, &[])
            .await;
        if let Err(e) = self.spawn_pty(
            id,
            crate::types::SHELL_SENTINEL,
            &[],
            &row.cwd,
            None,
            row.cols as u16,
            row.rows as u16,
            kb_ids,
            None,
        ) {
            let _ = self.repo.update_status(id, "exited", None).await;
            return Err(e);
        }

        // Fresh process → drop the previous (agent) scrollback so a later restart
        // doesn't replay it as this shell's history.
        if let Err(e) = self.repo.clear_scrollback(id).await {
            warn!(terminal_id = id, error = %e, "failed to clear persisted scrollback on shell fallback");
        }

        self.repo.update_status(id, "running", None).await?;
        let updated = self
            .repo
            .get_by_id(id)
            .await?
            .ok_or_else(|| TerminalError::NotFound(id.to_string()))?;
        let resp = row_to_response(&updated, None, &self.work_dir);
        self.emitter.emit_updated(&resp);
        info!(
            terminal_id = id,
            "terminal session fell back to a clean shell in place"
        );
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

    /// Tear down EVERY terminal session on real app exit: kill all live PTYs and
    /// delete all session rows (scrollback drops via FK CASCADE). The next launch
    /// then starts with a clean list instead of a pile of dirty `exited` ghosts.
    ///
    /// MUST be called only on a real quit (desktop tray-quit / `RunEvent::Exit`),
    /// never on close-to-tray — see `apps/desktop/src/main.rs`. Returns the number
    /// of rows deleted. Best-effort on the PTY kills (a failed kill only warns; the
    /// OS reaps the tree on process exit anyway).
    pub async fn shutdown_cleanup(&self) -> Result<u64, TerminalError> {
        for entry in self.live.iter() {
            if let Err(e) = entry.value().kill() {
                warn!(terminal_id = *entry.key(), error = %e, "failed to kill PTY during shutdown cleanup");
            }
        }
        self.live.clear();
        self.pending_spawn.clear();
        self.titled.clear();
        self.first_input.clear();
        let n = self.repo.delete_all().await?;
        info!(
            deleted = n,
            "terminal shutdown cleanup: all sessions killed and removed"
        );
        Ok(n)
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

/// Return the last `max_bytes` of `s` on a UTF-8 char boundary, plus whether it
/// was truncated. Cheap and allocation-light for the common (no-truncation) path.
fn tail_on_char_boundary(s: &str, max_bytes: usize) -> (String, bool) {
    if s.len() <= max_bytes {
        return (s.to_owned(), false);
    }
    let mut start = s.len() - max_bytes;
    while start < s.len() && !s.is_char_boundary(start) {
        start += 1;
    }
    (s[start..].to_owned(), true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::submit::SettleReason;
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
            let id = self
                .next_id
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
                + 1;
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
        async fn get_by_id(
            &self,
            id: i64,
        ) -> Result<Option<nomifun_db::TerminalSessionRow>, nomifun_db::DbError> {
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
        async fn update_size(
            &self,
            id: i64,
            cols: i64,
            rows_: i64,
        ) -> Result<(), nomifun_db::DbError> {
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
        async fn delete_all(&self) -> Result<u64, nomifun_db::DbError> {
            let mut rows = self.rows.lock().unwrap();
            let n = rows.len() as u64;
            rows.clear();
            self.scrollback.lock().unwrap().clear();
            Ok(n)
        }
        async fn update_command(
            &self,
            id: i64,
            command: &str,
            args: &str,
            backend: Option<&str>,
        ) -> Result<(), nomifun_db::DbError> {
            let mut rows = self.rows.lock().unwrap();
            let row = rows
                .get_mut(&id)
                .ok_or_else(|| nomifun_db::DbError::NotFound(id.to_string()))?;
            row.command = command.to_owned();
            row.args = args.to_owned();
            row.backend = backend.map(|s| s.to_owned());
            Ok(())
        }
        async fn update_autowork(
            &self,
            id: i64,
            autowork: Option<&str>,
        ) -> Result<(), nomifun_db::DbError> {
            let mut rows = self.rows.lock().unwrap();
            let row = rows
                .get_mut(&id)
                .ok_or_else(|| nomifun_db::DbError::NotFound(id.to_string()))?;
            row.autowork = autowork.map(|s| s.to_owned());
            Ok(())
        }
        async fn update_idmm(
            &self,
            id: i64,
            idmm: Option<&str>,
        ) -> Result<(), nomifun_db::DbError> {
            let mut rows = self.rows.lock().unwrap();
            let row = rows
                .get_mut(&id)
                .ok_or_else(|| nomifun_db::DbError::NotFound(id.to_string()))?;
            row.idmm = idmm.map(|s| s.to_owned());
            Ok(())
        }
        async fn get_idmm(&self, id: i64) -> Result<Option<String>, nomifun_db::DbError> {
            Ok(self
                .rows
                .lock()
                .unwrap()
                .get(&id)
                .and_then(|r| r.idmm.clone()))
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

    /// A title completer test double: returns `"{prefix}{n}"`, incrementing `n`
    /// each call, so a test can prove `summarize` ran exactly once.
    struct FakeTitler {
        calls: std::sync::atomic::AtomicUsize,
        prefix: String,
    }

    impl FakeTitler {
        fn new(prefix: &str) -> Self {
            Self {
                calls: std::sync::atomic::AtomicUsize::new(0),
                prefix: prefix.to_owned(),
            }
        }
    }

    #[async_trait::async_trait]
    impl crate::title::TerminalTitleCompleter for FakeTitler {
        async fn summarize(&self, _content: &str) -> Result<String, nomifun_common::AppError> {
            let n = self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(format!("{}{n}", self.prefix))
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
        bindings: Mutex<
            HashMap<(String, String), (nomifun_db::models::KnowledgeBindingRow, Vec<String>)>,
        >,
        next_binding_id: std::sync::atomic::AtomicI64,
    }

    #[async_trait::async_trait]
    impl nomifun_db::IKnowledgeRepository for MemKbRepo {
        async fn insert_base(
            &self,
            row: &nomifun_db::models::KnowledgeBaseRow,
        ) -> Result<(), nomifun_db::DbError> {
            self.bases
                .lock()
                .unwrap()
                .insert(row.id.clone(), row.clone());
            Ok(())
        }
        async fn update_base(
            &self,
            row: &nomifun_db::models::KnowledgeBaseRow,
        ) -> Result<(), nomifun_db::DbError> {
            self.bases
                .lock()
                .unwrap()
                .insert(row.id.clone(), row.clone());
            Ok(())
        }
        async fn delete_base(&self, id: &str) -> Result<(), nomifun_db::DbError> {
            self.bases.lock().unwrap().remove(id);
            Ok(())
        }
        async fn get_base(
            &self,
            id: &str,
        ) -> Result<Option<nomifun_db::models::KnowledgeBaseRow>, nomifun_db::DbError> {
            Ok(self.bases.lock().unwrap().get(id).cloned())
        }
        async fn list_bases(
            &self,
        ) -> Result<Vec<nomifun_db::models::KnowledgeBaseRow>, nomifun_db::DbError> {
            Ok(self.bases.lock().unwrap().values().cloned().collect())
        }
        async fn get_binding(
            &self,
            target_kind: &str,
            target_id: &str,
        ) -> Result<
            Option<(nomifun_db::models::KnowledgeBindingRow, Vec<String>)>,
            nomifun_db::DbError,
        > {
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
            let binding_id = bindings
                .get(&key)
                .map(|(row, _)| row.binding_id)
                .unwrap_or_else(|| {
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
        async fn delete_binding(
            &self,
            target_kind: &str,
            target_id: &str,
        ) -> Result<(), nomifun_db::DbError> {
            self.bindings
                .lock()
                .unwrap()
                .remove(&(target_kind.to_owned(), target_id.to_owned()));
            Ok(())
        }
        async fn list_bindings_using_kb(
            &self,
            _kb_id: &str,
        ) -> Result<Vec<nomifun_db::models::KnowledgeBindingRow>, nomifun_db::DbError> {
            Ok(vec![])
        }
        async fn list_knowledge_tags(
            &self,
        ) -> Result<Vec<nomifun_db::models::KnowledgeTagRow>, nomifun_db::DbError> {
            Ok(vec![])
        }
        async fn create_knowledge_tag(
            &self,
            _params: nomifun_db::models::CreateKnowledgeTagParams,
        ) -> Result<(), nomifun_db::DbError> {
            Ok(())
        }
        async fn update_knowledge_tag(
            &self,
            _key: &str,
            _params: nomifun_db::models::UpdateKnowledgeTagParams,
        ) -> Result<(), nomifun_db::DbError> {
            Ok(())
        }
        async fn delete_knowledge_tag(&self, _key: &str) -> Result<(), nomifun_db::DbError> {
            Ok(())
        }
    }

    /// A `KnowledgeService` over an in-memory repo with one registered base
    /// (`kb_test`) rooted at `kb_root`. The returned `TempDir` is the service
    /// data dir — keep it alive for the test's duration.
    fn knowledge_fixture(
        kb_root: &std::path::Path,
    ) -> (Arc<nomifun_knowledge::KnowledgeService>, tempfile::TempDir) {
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
        let emitter = nomifun_knowledge::KnowledgeEventEmitter::new(Arc::new(
            CapturingBroadcaster::default(),
        ));
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
        assert!(
            binding.enabled,
            "create-time kb_ids must enable the binding"
        );
        assert_eq!(binding.kb_ids, vec!["kb_test".to_owned()]);
        // The legacy per-session key must NOT be written — that mismatch is the
        // bug this fix closes (header/mount read workpath, create wrote terminal).
        let legacy = ks
            .get_binding("terminal", &resp.id.to_string())
            .await
            .unwrap();
        assert!(
            !legacy.enabled && legacy.kb_ids.is_empty(),
            "legacy (terminal, id) key must stay unbound"
        );

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
        let svc = TerminalService::new(
            Arc::new(MemRepo::default()),
            emitter,
            work_dir.path().to_path_buf(),
        );
        svc.with_knowledge_service(ks.clone());

        let mut request = req("cat", &[]);
        request.cwd = cwd.path().to_string_lossy().into_owned();
        request.knowledge_base_ids = Some(vec!["kb_test".into()]);
        let resp = svc.create("u", request).await.unwrap();

        let key = nomifun_knowledge::workpath_key(&cwd.path().to_string_lossy());
        assert_ne!(
            key, "__default__",
            "a user-picked dir must get a dedicated workpath key"
        );
        let binding = ks.get_binding("workpath", &key).await.unwrap();
        assert!(
            binding.enabled,
            "create-time kb_ids must enable the workpath binding"
        );
        assert_eq!(binding.kb_ids, vec!["kb_test".to_owned()]);
        // End-to-end: the README contract materializes from the workpath binding.
        assert!(
            cwd.path()
                .join(".nomi")
                .join("knowledge")
                .join("README.md")
                .exists(),
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
        let svc = TerminalService::new(
            Arc::new(MemRepo::default()),
            emitter,
            work_dir.path().to_path_buf(),
        );
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
        assert_eq!(
            binding.kb_ids,
            vec!["kb_test".to_owned()],
            "create selection replaces the base list"
        );
        assert!(binding.writeback, "writeback flag must be preserved");
        assert_eq!(
            binding.writeback_mode, "direct",
            "writeback mode must be preserved"
        );
        assert_eq!(
            binding.writeback_eagerness, "aggressive",
            "writeback eagerness must be preserved"
        );

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
        assert!(
            readme.exists(),
            "relaunch must re-sync mounts and write the README"
        );

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
        assert_eq!(
            resp.last_status, "running",
            "knowledge is best-effort, never blocks"
        );
        assert!(
            !cwd.path()
                .join(".nomi")
                .join("knowledge")
                .join("README.md")
                .exists()
        );
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
    async fn read_output_tail_strips_ansi_and_tails() {
        let (svc, _bc) = service();
        let id = svc.create("u", req("cat", &[])).await.unwrap().id;
        svc.submit_text(id, "marker-xyz").await.unwrap();

        // 等回显落入 scrollback。
        let mut ok = false;
        for _ in 0..40 {
            let out = svc.read_output_tail(id, 65536).await.unwrap();
            if out.text.contains("marker-xyz") {
                assert!(!out.truncated);
                assert_eq!(out.status, "running");
                // strip_ansi 已去除裸控制符：不应含 ESC。
                assert!(!out.text.contains('\u{1b}'));
                ok = true;
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        assert!(ok, "tail should contain the echoed marker");

        // 极小上界 → 截断标记为真。
        let tiny = svc.read_output_tail(id, 4).await.unwrap();
        assert!(tiny.truncated);
        assert!(tiny.text.len() <= 4 + 3); // 允许 char-boundary 前移少量
        svc.delete(id).await.ok();
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
            || {
                bc.events
                    .lock()
                    .unwrap()
                    .iter()
                    .any(|e| e.name == "terminal.exit")
            },
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
    async fn submit_text_single_line_executes_via_cat_echo() {
        // cat 会回显收到的字节。shell 后端(None) → 单行 raw+CR 一次写。
        let (svc, _bc) = service();
        let id = svc.create("u", req("cat", &[])).await.unwrap().id;
        svc.submit_text(id, "hello-world").await.unwrap();

        // 等 cat 把 "hello-world\r" 回显进 live scrollback。
        let mut seen = false;
        for _ in 0..40 {
            if let Ok(resp) = svc.get(id).await {
                if let Some(b64) = resp.scrollback_b64 {
                    let s = String::from_utf8_lossy(&BASE64.decode(b64).unwrap()).to_string();
                    if s.contains("hello-world") {
                        seen = true;
                        break;
                    }
                }
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        assert!(seen, "cat should echo the submitted single line");
        svc.delete(id).await.ok();
    }

    #[tokio::test]
    async fn submit_text_not_found_when_not_live() {
        let (svc, _bc) = service();
        assert!(matches!(
            svc.submit_text(999_999, "x").await.unwrap_err(),
            TerminalError::NotFound(_)
        ));
    }

    #[tokio::test]
    async fn await_turn_settle_idle_when_shell_goes_quiet() {
        // cat(shell,无 lifecycle) 回显后即安静 → Idle。
        let (svc, _bc) = service();
        let id = svc.create("u", req("cat", &[])).await.unwrap().id;
        svc.submit_text(id, "ping").await.unwrap();
        let reason = svc
            .await_turn_settle(id, std::time::Duration::from_secs(5))
            .await;
        assert_eq!(reason, SettleReason::Idle);
        svc.delete(id).await.ok();
    }

    #[tokio::test]
    async fn await_turn_settle_timeout_when_output_never_quiets() {
        // yes 持续刷输出 → 永不 idle；超时短于 idle window → Timeout。
        let (svc, _bc) = service();
        let id = svc.create("u", req("yes", &[])).await.unwrap().id;
        let reason = svc
            .await_turn_settle(id, std::time::Duration::from_millis(400))
            .await;
        assert_eq!(reason, SettleReason::Timeout);
        svc.delete(id).await.ok();
    }

    #[tokio::test]
    async fn await_turn_settle_turn_end_via_lifecycle_for_agent_backend() {
        use crate::lifecycle::TerminalLifecycleServer;
        let (svc, _bc) = service();
        let srv = std::sync::Arc::new(TerminalLifecycleServer::start().await.unwrap());
        svc.with_terminal_lifecycle(srv.clone(), "nomicore".into());

        // 进程用 cat，但声明 backend=claude → 被判为 lifecycle-capable，走 TurnEnd 路径。
        let request = nomifun_api_types::CreateTerminalRequest {
            name: None,
            cwd: std::env::temp_dir().to_string_lossy().into_owned(),
            command: "cat".into(),
            args: vec![],
            env: None,
            backend: Some("claude".into()),
            mode: Some("default".into()),
            cols: 80,
            rows: 24,
            defer_spawn: false,
            knowledge_base_ids: None,
        };
        let id = svc.create("u", request).await.unwrap().id;

        // settle future 与 POST future 用 tokio::join! 同任务并发（svc 非 Clone，
        // 借用即可）。settle 先被 poll → 内部 subscribe_lifecycle 建立订阅；post
        // 延迟 150ms 再发 turn_end hook，事件不会漏。
        let url = format!("http://127.0.0.1:{}/hook", srv.http_port());
        let token = srv.auth_token().to_owned();
        let client = reqwest::Client::builder().no_proxy().build().unwrap();
        let body = serde_json::json!({"terminal_id": id, "kind": "turn_end", "payload": {}});
        let settle = svc.await_turn_settle(id, std::time::Duration::from_secs(5));
        let post = async {
            tokio::time::sleep(std::time::Duration::from_millis(150)).await;
            client.post(&url).json(&body).bearer_auth(&token).send().await.unwrap();
        };
        let (reason, _) = tokio::join!(settle, post);
        assert_eq!(reason, SettleReason::TurnEnd);
        svc.delete(id).await.ok();
    }

    #[tokio::test]
    async fn await_turn_settle_exited_when_lifecycle_pty_dies() {
        use crate::lifecycle::TerminalLifecycleServer;
        let (svc, _bc) = service();
        let srv = std::sync::Arc::new(TerminalLifecycleServer::start().await.unwrap());
        svc.with_terminal_lifecycle(srv.clone(), "nomicore".into());

        // backend=claude → lifecycle-capable branch. The process is `cat` (long-lived
        // until killed). The lifecycle channel is app-lifetime and never closes on PTY
        // death, so WITHOUT the liveness poll this would ride the full 10s timeout and
        // return a dishonest Timeout. WITH it, the 2s tick observes the dead PTY and
        // returns Exited within a few seconds.
        let request = nomifun_api_types::CreateTerminalRequest {
            name: None,
            cwd: std::env::temp_dir().to_string_lossy().into_owned(),
            command: "cat".into(),
            args: vec![],
            env: None,
            backend: Some("claude".into()),
            mode: Some("default".into()),
            cols: 80,
            rows: 24,
            defer_spawn: false,
            knowledge_base_ids: None,
        };
        let id = svc.create("u", request).await.unwrap().id;

        // Kill the PTY and let the exit callback drop it from the live map.
        svc.kill(id).await.unwrap();
        tokio::time::sleep(Duration::from_millis(400)).await;
        assert!(
            !svc.live.contains_key(&id),
            "the killed PTY must be gone from the live map before we await settle"
        );

        let started = std::time::Instant::now();
        let reason = svc
            .await_turn_settle(id, std::time::Duration::from_secs(10))
            .await;
        let elapsed = started.elapsed();
        assert_eq!(reason, SettleReason::Exited);
        assert!(
            elapsed < Duration::from_secs(5),
            "must resolve via the 2s liveness tick, not ride the 10s timeout (elapsed {elapsed:?})"
        );
        svc.delete(id).await.ok();
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
        let id = svc
            .create("u", req("printf", &["restore-me"]))
            .await
            .unwrap()
            .id;

        // Exit drops the live handle and persists the final scrollback.
        assert!(
            wait_for(
                || bc
                    .events
                    .lock()
                    .unwrap()
                    .iter()
                    .any(|e| e.name == "terminal.exit"),
                4000
            )
            .await
        );
        // `on_exit` persists on a spawned task — give it a beat to land.
        tokio::time::sleep(Duration::from_millis(250)).await;

        // No live handle now → `get` must fall back to the persisted snapshot.
        let resp = svc.get(id).await.unwrap();
        let b64 = resp
            .scrollback_b64
            .expect("persisted scrollback must be returned when not live");
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
        assert!(
            landed,
            "cat should echo the fed input into the live scrollback"
        );

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
        assert_eq!(
            (got.cols, got.rows),
            (120, 40),
            "deferred spawn must adopt the first-resize size"
        );
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
    async fn relaunch_as_shell_swaps_command_and_emits_updated() {
        let (svc, bc) = service();
        // Start as an "agent" session (backend label set), then fall back to shell.
        let mut r = req("cat", &[]);
        r.backend = Some("claude".into());
        let id = svc.create("u", r).await.unwrap().id;
        bc.events.lock().unwrap().clear();

        let resp = svc.relaunch_as_shell(id).await.unwrap();
        assert_eq!(
            resp.command,
            crate::types::SHELL_SENTINEL,
            "command rewritten to the shell sentinel"
        );
        assert_eq!(resp.backend, None, "agent backend label cleared");
        assert_eq!(resp.last_status, "running", "fresh shell is running");
        // The row is persisted as a shell, so its mechanical name is now `Shell`.
        let row = svc.get(id).await.unwrap();
        assert_eq!(row.command, crate::types::SHELL_SENTINEL);
        // A terminal.updated event re-enables the frontend composer.
        let emitted_updated = bc
            .events
            .lock()
            .unwrap()
            .iter()
            .any(|e| e.name == "terminal.updated");
        assert!(
            emitted_updated,
            "relaunch_as_shell must emit terminal.updated"
        );

        svc.kill(id).await.ok();
    }

    #[tokio::test]
    async fn shutdown_cleanup_kills_and_deletes_all_sessions() {
        let (svc, _bc) = service();
        let a = svc.create("u", req("cat", &[])).await.unwrap().id;
        let b = svc.create("u", req("cat", &[])).await.unwrap().id;
        assert!(svc.live.contains_key(&a) && svc.live.contains_key(&b));

        let n = svc.shutdown_cleanup().await.unwrap();
        assert_eq!(n, 2, "both rows deleted");
        // Live map drained and rows gone — next launch starts clean.
        assert!(svc.live.is_empty());
        assert!(svc.list("u").await.unwrap().is_empty());
        // Idempotent on an already-empty service.
        assert_eq!(svc.shutdown_cleanup().await.unwrap(), 0);
    }

    async fn wait_for_name(svc: &TerminalService, id: i64, expected: &str, ms: u64) -> bool {
        for _ in 0..(ms / 20).max(1) {
            if svc
                .get(id)
                .await
                .map(|s| s.name == expected)
                .unwrap_or(false)
            {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        svc.get(id)
            .await
            .map(|s| s.name == expected)
            .unwrap_or(false)
    }

    #[tokio::test]
    async fn shell_session_autotitles_from_first_input_line() {
        let (svc, _bc) = service();
        // command "cat", no backend → is_agent=false, mechanical name "cat".
        let id = svc.create("u", req("cat", &[])).await.unwrap().id;
        // First input line (ends with CR) → fallback title from that line.
        svc.input(id, &BASE64.encode("echo hello world\r"))
            .await
            .unwrap();
        assert!(
            wait_for_name(&svc, id, "echo hello world", 4000).await,
            "shell title should fall back to the first input line, got {:?}",
            svc.get(id).await.unwrap().name
        );
        svc.kill(id).await.ok();
    }

    #[tokio::test]
    async fn agent_session_also_autotitles_from_first_input_line() {
        let (svc, _bc) = service();
        // An agent session (backend "claude", mechanical name "claude") must ALSO
        // title from the first input line — independent of the (possibly-absent)
        // TurnEnd lifecycle hook / a configured provider. No completer is wired
        // here, so it takes the first-N-chars path.
        let mut r = req("cat", &[]);
        r.backend = Some("claude".into());
        let id = svc.create("u", r).await.unwrap().id;
        svc.input(id, &BASE64.encode("你好\r")).await.unwrap();
        assert!(
            wait_for_name(&svc, id, "你好", 4000).await,
            "agent session should title from first input, got {:?}",
            svc.get(id).await.unwrap().name
        );
        svc.kill(id).await.ok();
    }

    #[tokio::test]
    async fn autotitle_strips_tui_mouse_and_focus_sequences() {
        // A claude-style TUI enables mouse tracking + focus reporting, so xterm
        // sends focus events (CSI I) and SGR mouse reports (CSI < … M) into the
        // PTY before the user's typed text. These must be stripped, not titled.
        let (svc, _bc) = service();
        let id = svc.create("u", req("cat", &[])).await.unwrap().id;
        let noisy = "\u{1b}[I\u{1b}[<35;29;26M\u{1b}[<0;30;25m你好\r";
        svc.input(id, &BASE64.encode(noisy)).await.unwrap();
        assert!(
            wait_for_name(&svc, id, "你好", 4000).await,
            "TUI control sequences must be stripped from the title, got {:?}",
            svc.get(id).await.unwrap().name
        );
        svc.kill(id).await.ok();
    }

    #[tokio::test]
    async fn autotitle_uses_completer_result_for_llm_source() {
        let (svc, _bc) = service();
        svc.with_title_completer(Arc::new(FakeTitler::new("title-")));
        // backend "claude" → mechanical name "claude" (the agent label).
        let mut r = req("cat", &[]);
        r.backend = Some("claude".into());
        let id = svc.create("u", r).await.unwrap().id;

        svc.maybe_autotitle(id, Some("user deployed prod; assistant confirmed".into()))
            .await;
        assert_eq!(
            svc.get(id).await.unwrap().name,
            "title-0",
            "LLM summary becomes the title"
        );
        svc.kill(id).await.ok();
    }

    #[tokio::test]
    async fn autotitle_skips_when_name_is_custom() {
        let (svc, _bc) = service();
        svc.with_title_completer(Arc::new(FakeTitler::new("auto-")));
        let id = svc.create("u", req("cat", &[])).await.unwrap().id;
        // A manual rename makes name != default_name → never overwritten.
        svc.update_meta(id, Some("我的终端".into()), None)
            .await
            .unwrap();
        svc.maybe_autotitle(id, Some("content".into())).await;
        assert_eq!(
            svc.get(id).await.unwrap().name,
            "我的终端",
            "must not clobber a manual rename"
        );
        svc.kill(id).await.ok();
    }

    #[tokio::test]
    async fn autotitle_fires_at_most_once() {
        let (svc, _bc) = service();
        svc.with_title_completer(Arc::new(FakeTitler::new("t")));
        let id = svc.create("u", req("cat", &[])).await.unwrap().id;
        svc.maybe_autotitle(id, Some("a".into())).await;
        assert_eq!(svc.get(id).await.unwrap().name, "t0");
        // Second call is a no-op (once-guard): the completer is NOT called again,
        // so the name stays "t0" (not "t1").
        svc.maybe_autotitle(id, Some("b".into())).await;
        assert_eq!(svc.get(id).await.unwrap().name, "t0");
        svc.kill(id).await.ok();
    }

    #[tokio::test]
    async fn unknown_id_is_not_found() {
        let (svc, _bc) = service();
        assert!(matches!(
            svc.get(999_999).await.unwrap_err(),
            TerminalError::NotFound(_)
        ));
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
            entries
                .iter()
                .any(|e| e.name == "hello.txt" && e.entry_type == "file"),
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

        let err = svc
            .browse_workspace(resp.id, "../", None)
            .await
            .unwrap_err();
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
        assert!(
            bc.events
                .lock()
                .unwrap()
                .iter()
                .any(|e| e.name == "terminal.removed")
        );
    }

    #[tokio::test]
    async fn relaunch_reuses_same_id_and_marks_running() {
        let (svc, bc) = service();
        // short-lived child so it exits, then relaunch in place.
        let resp = svc.create("u", req("printf", &["x"])).await.unwrap();
        let id = resp.id.clone();
        // wait until it exits
        let exited = wait_for(
            || {
                bc.events
                    .lock()
                    .unwrap()
                    .iter()
                    .any(|e| e.name == "terminal.exit")
            },
            4000,
        )
        .await;
        assert!(exited);

        let relaunched = svc.relaunch(id).await.unwrap();
        assert_eq!(relaunched.id, id, "relaunch must reuse the same session id");
        assert_eq!(relaunched.last_status, "running");
        assert!(
            bc.events
                .lock()
                .unwrap()
                .iter()
                .any(|e| e.name == "terminal.updated"),
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
        assert!(
            svc.live.contains_key(&id),
            "freshly created session must be live"
        );

        svc.relaunch(id).await.unwrap();

        // Wait well past EXIT_DRAIN_GRACE (~120ms) so the OLD child's exit
        // callback has definitely fired before we assert.
        tokio::time::sleep(Duration::from_millis(400)).await;

        assert!(
            svc.live.contains_key(&id),
            "the fresh PTY must remain live after the predecessor's stale exit callback"
        );
        let got = svc.get(id).await.unwrap();
        assert_eq!(
            got.last_status, "running",
            "relaunch must not leave the session exited"
        );

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
            K {
                port: 51123,
                token: "tok".into(),
                binary_path: "nomicore".into(),
            },
            std::env::temp_dir(),
        );
        let enh = svc.build_enhancement(&[], 1);
        assert!(
            enh.mcp_servers.is_empty(),
            "empty kb_ids must yield no MCP servers"
        );

        // Case 2: kb_ids present but NO knowledge_mcp_config wired → empty.
        let (svc2, _bc2) = service(); // fresh service, config NOT wired
        let enh = svc2.build_enhancement(&["kb_1".into()], 1);
        assert!(
            enh.mcp_servers.is_empty(),
            "no config wired must yield no MCP servers"
        );

        // Case 3: kb_ids present AND config wired → one McpServerSpec.
        let enh = svc.build_enhancement(&["kb_1".into(), "kb_2".into()], 1);
        assert_eq!(
            enh.mcp_servers.len(),
            1,
            "expected exactly one MCP server spec"
        );
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
        assert!(
            bc.events
                .lock()
                .unwrap()
                .iter()
                .any(|e| e.name == "terminal.updated")
        );
        // blank name is ignored (keeps prior)
        let again = svc
            .update_meta(resp.id, Some("   ".into()), None)
            .await
            .unwrap();
        assert_eq!(again.name, "Renamed");
        svc.delete(resp.id).await.ok();
    }

    #[tokio::test]
    async fn spawn_injects_knowledge_mcp_for_claude_when_kb_ids_present() {
        use crate::enhance::{McpServerSpec, TerminalLaunchEnhancement, apply_enhancement};
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
                    (
                        nomifun_api_types::KnowledgeMcpConfig::ENV_PORT.into(),
                        "9".into(),
                    ),
                    (
                        nomifun_api_types::KnowledgeMcpConfig::ENV_TOKEN.into(),
                        "t".into(),
                    ),
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
        use crate::lifecycle::{LifecycleKind, TerminalLifecycleServer};

        let (svc, _bc) = service();
        let srv = Arc::new(
            TerminalLifecycleServer::start()
                .await
                .expect("start lifecycle server"),
        );
        svc.with_terminal_lifecycle(srv.clone(), "nomicore".into());

        // Via the trait object, subscribe_lifecycle should now return Some.
        let driver: &dyn TerminalDriver = &svc;
        let mut rx = driver
            .subscribe_lifecycle(42)
            .expect("must be Some when lifecycle is wired");

        // Broadcast an event through the lifecycle server.
        let url = format!("http://127.0.0.1:{}/hook", srv.http_port());
        let body = serde_json::json!({
            "terminal_id": 42,
            "kind": "turn_end",
            "payload": {"last_assistant_message": "hello"}
        });
        let client = reqwest::Client::builder().no_proxy().build().unwrap();
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
        let stored = svc
            .requirement_mcp_config()
            .expect("must be Some after wiring");
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
        assert!(
            !enh.is_empty(),
            "requirement MCP alone must produce a non-empty enhancement"
        );
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
            K {
                port: 111,
                token: "ktok".into(),
                binary_path: "nomicore".into(),
            },
            std::env::temp_dir(),
        );
        svc.with_requirement_mcp_config(R {
            port: 222,
            token: "rtok".into(),
            binary_path: "nomicore".into(),
        });

        let enh = svc.build_enhancement(&["kb_x".into()], 7);
        assert_eq!(
            enh.mcp_servers.len(),
            2,
            "both knowledge + requirement MCP servers"
        );
        let names: Vec<&str> = enh.mcp_servers.iter().map(|s| s.name.as_str()).collect();
        assert!(names.contains(&K::SERVER_NAME));
        assert!(names.contains(&R::SERVER_NAME));
    }

    /// `apply_enhancement` renders the requirement MCP for `claude` (--mcp-config)
    /// but does NOT render it for `/bin/bash` (unknown CLI → honest no-op).
    #[tokio::test]
    async fn apply_enhancement_requirement_renders_for_claude_not_shell() {
        use crate::enhance::{McpServerSpec, TerminalLaunchEnhancement, apply_enhancement};
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
        assert!(
            argv.iter().any(|a| a == "--mcp-config"),
            "claude must get --mcp-config"
        );
        // Verify the mcp.json contains the requirement server
        let mcp_path = argv
            .iter()
            .position(|a| a == "--mcp-config")
            .map(|i| &argv[i + 1])
            .unwrap();
        let doc: serde_json::Value =
            serde_json::from_slice(&std::fs::read(mcp_path).unwrap()).unwrap();
        assert!(
            doc["mcpServers"][R::SERVER_NAME].is_object(),
            "mcp.json must contain the requirement server"
        );
        assert_eq!(
            doc["mcpServers"][R::SERVER_NAME]["env"][R::ENV_OWNER_KIND],
            "terminal"
        );

        // codex → renders via -c overrides
        let (argv, _env) = apply_enhancement("codex", vec![], &enh, dir.path(), None);
        let joined = argv.join(" ");
        assert!(joined.contains(&format!("mcp_servers.{}.command=", R::SERVER_NAME)));

        // unknown CLI (bash) → no injection (honest)
        let (argv, _env) =
            apply_enhancement("/bin/bash", vec!["-l".into()], &enh, dir.path(), None);
        assert_eq!(
            argv,
            vec!["-l".to_owned()],
            "/bin/bash must get NO MCP injection"
        );
    }
}

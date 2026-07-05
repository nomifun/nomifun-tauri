use crate::agent_runtime::AgentRuntime;
use crate::capability::PromptCtx;
use crate::capability::cli_process::CliAgentProcess;
use crate::capability::prompt_pipeline::PromptPipeline;
use crate::capability::skill_manager::AcpSkillManager;
use crate::factory::acp_assembler::AcpSessionParams;
use crate::manager::acp::{
    AcpSession, AcpSessionEvent, KnowledgeContextHook, ModelIdentityReminderHook, PermissionRouter,
    SessionNewPreludeHook,
};
use crate::manager::process_registry::{register_session_process, unregister_agent_process};
use crate::protocol::acp::AcpProtocol;
use crate::protocol::error::{AcpError, CloseReason};
use crate::protocol::events::{AgentStreamEvent, FinishEventData, TurnStopReason};
use crate::protocol::send_error::AgentSendError;
use crate::registry::CatalogSender;
use crate::shared_kernel::{ModeId, ModelId, SessionId as DomainSessionId};
use crate::types::SendMessageData;
use agent_client_protocol::schema::{
    CancelNotification, SessionId, SessionModelState, SessionNotification, UsageUpdate,
};
use nomifun_api_types::{AgentHandshake, SlashCommandItem};
use nomifun_common::{
    AgentKillReason, AgentType, AppError, ConversationStatus, ErrorChain, TimestampMs, normalize_keys_to_snake_case,
};
use serde_json::Value;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{Mutex, RwLock, broadcast, mpsc};
use tracing::{debug, error, info, warn};

/// The user-visible body inside an [`AppError`].
///
/// `AppError`'s `Display` prefixes every variant with its HTTP status name
/// (`"Bad gateway: ..."`, `"Not found: ..."`, etc.). That's correct for HTTP
/// response bodies, but the WebSocket `error` event we broadcast goes straight
/// to the renderer and gets shown verbatim — the prefix only adds noise. Strip
/// it so the user sees the upstream message.
///
/// `pub(super)` so the close-path helpers in `agent_close.rs` can reuse the
/// same prefix-stripping logic when fabricating the `Failed { display }` arm.
pub(super) fn user_facing_message(err: &AppError) -> String {
    let full = err.to_string();
    // Each variant's Display starts with `"<Tag>: "`. Find the first ": " and
    // return what follows. Variants without a colon (e.g. `RateLimited` →
    // "Rate limited") fall through to the full string.
    full.split_once(": ").map(|(_, rest)| rest.to_owned()).unwrap_or(full)
}

use super::codex_sandbox;
use super::mode_normalize::normalize_requested_mode;

/// Grace period before force-killing an ACP process (ms).
const ACP_KILL_GRACE_MS: u64 = 500;

/// Decompose a child `ExitStatus` (or its absence) into the
/// `(exit_code, signal)` pair that `AcpError::StartupCrash` /
/// `AcpError::Disconnected` carry.
///
/// `None` ⇒ wait failed; we have no actionable info to pass on.
/// On Unix, terminating signals surface via `ExitStatusExt::signal()`; the
/// numeric value is rendered as `Some("signal:N")`. On Windows there are no
/// POSIX signals, so `signal` stays `None` and the upstream exit code is the
/// only diagnostic.
///
/// `pub(super)` so the close-path helpers in `agent_close.rs` can read the
/// child's status when a `send_message` fails after init.
pub(super) fn exit_status_parts(exit: Option<std::process::ExitStatus>) -> (Option<i32>, Option<String>) {
    let Some(status) = exit else {
        return (None, None);
    };
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        if let Some(sig) = status.signal() {
            return (status.code(), Some(format!("signal:{sig}")));
        }
    }
    (status.code(), None)
}

fn initial_mode_from_params(params: &AcpSessionParams) -> Option<ModeId> {
    // Prefer the last-persisted mode; for brand-new conversations
    // fall back to `AcpBuildExtra::session_mode` so the first turn
    // still honours the caller's choice.
    params
        .session_snapshot
        .as_ref()
        .and_then(|s| s.current_mode_id.as_ref())
        .map(|m| normalize_requested_mode(&params.metadata, m.as_str()))
        .or_else(|| {
            params
                .config
                .session_mode
                .as_ref()
                .map(|m| normalize_requested_mode(&params.metadata, m))
        })
        .filter(|m| !m.is_empty())
        .map(ModeId::new)
}

fn confirm_option_id(data: &Value) -> Option<String> {
    match data {
        Value::String(v) => Some(v.clone()),
        Value::Object(map) => map
            .get("option_id")
            .or_else(|| map.get("optionId"))
            .or_else(|| map.get("value"))
            .and_then(Value::as_str)
            .map(ToOwned::to_owned),
        _ => None,
    }
}

/// Serialize an external value (typically an ACP SDK struct that emits
/// camelCase) and normalise every object key to snake_case before it
/// leaves the backend. All handshake columns, WebSocket payloads, and
/// HTTP responses share this rule — callers should go through this
/// helper instead of `serde_json::to_value` directly.
pub(super) fn sdk_to_snake_value<T: serde::Serialize>(value: &T) -> Option<Value> {
    let mut v = serde_json::to_value(value).ok()?;
    normalize_keys_to_snake_case(&mut v);
    Some(v)
}

/// Manages a single ACP Agent instance.
///
/// ACP is the most complex agent type, supporting 20+ CLI sub-backends
/// (Claude, Qwen, CodeBuddy, Codex, etc.). Communication now happens via
/// the `agent-client-protocol` SDK's JSON-RPC transport, replacing the
/// previous hand-crafted JSON-over-stdin/stdout approach.
pub struct AcpAgentManager {
    /// Pre-computed, immutable session parameters assembled by the factory.
    pub(super) params: Arc<AcpSessionParams>,

    /// Session aggregate root — owns desired/observed/advertised state.
    /// Single in-memory source of truth for session lifecycle, modes,
    /// models, config, and all runtime data previously split across
    /// `AcpRuntimeSnapshot` and `AcpState`.
    pub(super) session: RwLock<AcpSession>,

    /// Shared runtime holding status, last_activity, and the event
    /// broadcast channel. `pub(super)` so sibling modules (session_flow,
    /// event_tracker) can call `self.runtime.emit(...)` directly.
    ///
    /// Lifecycle: written by `IAgentTask::send_message` (Running →
    /// Finished/Error), `stop` (emit_finish), and `kill` (emit_error).
    /// `emit_finish` / `emit_error` are idempotent in the Finished
    /// absorbing state — multiple calls are safe.
    pub(super) runtime: AgentRuntime,

    /// ACP protocol handle (SDK connection).
    pub(super) protocol: AcpProtocol,

    /// Routes permission requests from the protocol layer to the user
    /// and back. Owns the receiver channel, pending map, and closing flag.
    pub(super) permission_router: Arc<PermissionRouter>,

    /// Shared skill manager — used to discover skills for first-message injection.
    pub(super) skill_manager: Arc<AcpSkillManager>,

    /// Domain event sender — session aggregate events are forwarded here
    /// for the persistence consumer (`AcpSessionSyncService`).
    pub(super) domain_event_tx: mpsc::Sender<AcpSessionEvent>,

    /// Outbound prompt transformation chain. Constructed once at build
    /// time with the two built-in hooks; not swapped at runtime.
    pub(super) pipeline: PromptPipeline,

    /// Underlying CLI process (for lifecycle management: kill, is_running).
    /// `pub(super)` so the close-path helpers in `agent_close.rs` can read
    /// `exit_status` and peek stderr without going through a wrapper method.
    pub(super) process: Arc<CliAgentProcess>,

    /// Mutex for serializing session operations (new/load/send).
    session_lock: Mutex<()>,
}

impl AcpAgentManager {
    /// Create a new ACP agent manager by spawning a CLI subprocess and
    /// establishing an ACP protocol connection.
    ///
    /// `params` is the pre-computed, immutable session bundle assembled by
    /// `assemble_acp_params` in the factory layer. `catalog_tx` is the
    /// MPSC sender used for the one-shot initialize handshake write;
    /// session-driven fields flow through the `CatalogForwarder` the
    /// factory spawns after construction.
    pub async fn build(
        params: Arc<AcpSessionParams>,
        skill_manager: Arc<AcpSkillManager>,
        catalog_tx: &CatalogSender,
    ) -> Result<
        (
            Self,
            mpsc::Receiver<AcpSessionEvent>,
            mpsc::Receiver<SessionNotification>,
        ),
        AppError,
    > {
        let (this, domain_event_rx, notification_rx) = AcpAgentManager::new(params, skill_manager).await?;
        this.init(catalog_tx).await;
        Ok((this, domain_event_rx, notification_rx))
    }

    async fn new(
        params: Arc<AcpSessionParams>,
        skill_manager: Arc<AcpSkillManager>,
    ) -> Result<
        (
            Self,
            mpsc::Receiver<AcpSessionEvent>,
            mpsc::Receiver<SessionNotification>,
        ),
        AppError,
    > {
        let initial_mode = initial_mode_from_params(&params);
        let mut command_spec = params.command_spec.clone();
        codex_sandbox::prepare_command_spec_for_agent(
            &params.metadata,
            initial_mode.as_ref().map(|m| m.as_str()),
            &params.data_dir,
            &mut command_spec,
        )
        .await;

        let process = Arc::new(
            CliAgentProcess::spawn_for_sdk(command_spec.clone(), &params.data_dir).await?,
        );
        register_session_process(
            &params.data_dir,
            Arc::clone(&process),
            params.conversation_id.clone(),
            AgentType::Acp,
            params.metadata.backend.clone(),
            Some(format!(
                "{} {}",
                command_spec.command.display(),
                command_spec.args.join(" ")
            )),
        )?;
        let (stdin, stdout) = process.take_stdio().await.ok_or_else(|| {
            error!(conversation_id = %params.conversation_id, "Failed to take stdio from CLI process");
            let _ = unregister_agent_process(&params.data_dir, process.pid());
            AppError::Internal("Failed to take stdio from CLI process".into())
        })?;

        // Dedicated channel for raw SDK SessionNotifications → session tracker.
        // This channel is separate from event_tx so the tracker never re-applies
        // events that were broadcast for the UI (e.g. from emit_snapshot_events).
        let (notification_tx, notification_rx) = mpsc::channel::<SessionNotification>(256);
        let (domain_event_tx, domain_event_rx) = mpsc::channel(256);
        let (permission_tx, permission_rx) = mpsc::channel(32);
        let runtime = AgentRuntime::new(params.conversation_id.clone(), params.workspace.path.clone(), 256);

        // Race the handshake against process exit. The SDK's stdout EOF
        // detection can lag (observed: 30s on Windows when the agent dies
        // 70ms in — ELECTRON-1BT), so we explicitly watch the child. If
        // it dies before init completes, surface a `StartupCrash` carrying
        // the buffered stderr instead of waiting out the timeout.
        let connect_fut = AcpProtocol::connect(stdin, stdout, runtime.event_sender(), permission_tx, notification_tx);
        tokio::pin!(connect_fut);
        let protocol = tokio::select! {
            biased;
            exit = process.wait_for_exit() => {
                let stderr = process.peek_stderr_tail(64).await;
                let (exit_code, signal) = exit_status_parts(exit);
                error!(
                    conversation_id = %params.conversation_id,
                    exit_code = ?exit_code,
                    signal = ?signal,
                    stderr = %stderr,
                    "Agent process exited before ACP handshake completed"
                );
                let _ = unregister_agent_process(&params.data_dir, process.pid());
                return Err(AppError::from(AcpError::StartupCrash { exit_code, signal, stderr }));
            }
            res = &mut connect_fut => res.map_err(|e| {
                error!(
                    conversation_id = %params.conversation_id,
                    error = %ErrorChain(&e),
                    "Failed to establish ACP protocol connection"
                );
                let _ = unregister_agent_process(&params.data_dir, process.pid());
                AppError::from(e)
            })?,
        };
        let permission_router = Arc::new(PermissionRouter::new(permission_rx));

        let snapshot = params.session_snapshot.as_ref();

        let (initial_model, initial_config) = (
            snapshot.and_then(|s| s.current_model_id.clone()).or_else(|| {
                params
                    .config
                    .current_model_id
                    .as_ref()
                    .filter(|m| !m.is_empty())
                    .map(|m| ModelId::new(m.clone()))
            }),
            snapshot.map(|s| s.config_selections.clone()).unwrap_or_default(),
        );

        let session = AcpSession::new(initial_mode, initial_model, initial_config);

        let pipeline = PromptPipeline::new(vec![
            // KnowledgeContextHook runs first so its block ends up closest to
            // the user's task in the folded first prompt (each hook prepends).
            Arc::new(KnowledgeContextHook),
            Arc::new(SessionNewPreludeHook),
            Arc::new(ModelIdentityReminderHook),
        ]);

        let manager = Self {
            params,
            session: RwLock::new(session),
            runtime,
            process,
            protocol,
            session_lock: Mutex::new(()),
            permission_router,
            skill_manager,
            domain_event_tx,
            pipeline,
        };
        Ok((manager, domain_event_rx, notification_rx))
    }

    async fn init(&self, catalog_tx: &CatalogSender) {
        let init_handshake = AgentHandshake {
            agent_capabilities: self.protocol.agent_capabilities().and_then(|c| sdk_to_snake_value(&c)),
            auth_methods: self.protocol.auth_methods().and_then(|m| sdk_to_snake_value(&m)),
            ..Default::default()
        };
        if init_handshake.agent_capabilities.is_some() || init_handshake.auth_methods.is_some() {
            catalog_tx.send_partial(self.params.metadata.id.clone(), init_handshake);
        }

        // Seed the observed/advertised layers (observed mode/model, cached
        // context_usage) from the persisted snapshot. Desired fields are
        // already populated via `AcpSession::new`.
        if let Some(snapshot) = self.params.session_snapshot.as_ref() {
            let mut session = self.session.write().await;
            session.preload_persisted(snapshot);
            // Preload did not come from the user this turn — drain so the
            // persistence consumer doesn't echo the DB back into itself.
            session.drain_events();
        }
        if let Some(agent_capabilities) = self.protocol.agent_capabilities() {
            let mut session = self.session.write().await;
            session.apply_advertised_capabilities(agent_capabilities);
        }
        if let Some(auth_methods) = self.protocol.auth_methods() {
            let mut session = self.session.write().await;
            session.apply_advertised_auth_methods(auth_methods);
        }
    }
}

impl AcpAgentManager {
    pub(crate) async fn mode(&self) -> Result<nomifun_api_types::AgentModeResponse, AppError> {
        let desired = self
            .session
            .read()
            .await
            .desired_mode()
            .map(|mode| normalize_requested_mode(&self.params.metadata, mode))
            .filter(|mode| !mode.is_empty());
        Ok(nomifun_api_types::AgentModeResponse {
            mode: self
                .session
                .read()
                .await
                .modes()
                .map(|modes| modes.current_mode_id.to_string())
                .or(desired)
                .unwrap_or_else(|| normalize_requested_mode(&self.params.metadata, "default")),
            initialized: self.session_id().await.is_some(),
        })
    }

    pub(crate) fn is_claude_backend(&self) -> bool {
        self.params.metadata.backend.as_deref() == Some("claude")
    }

    /// Cached model info from the ACP backend, if any has been received.
    pub(crate) async fn model(&self) -> Option<SessionModelState> {
        self.session.read().await.model_info().cloned()
    }

    /// Cached context usage info from the ACP backend.
    pub(crate) async fn usage(&self) -> Option<UsageUpdate> {
        self.session.read().await.context_usage().cloned()
    }

    /// Set the mode for the current session.
    pub(crate) async fn set_mode(&self, mode: &str) -> Result<(), AppError> {
        let normalized_mode = normalize_requested_mode(&self.params.metadata, mode);
        if normalized_mode.is_empty() {
            return Ok(());
        }
        codex_sandbox::sync_managed_home_for_agent(
            &self.params.metadata,
            Some(&normalized_mode),
            &self.params.data_dir,
        )
        .await;
        let session_id = self.session.read().await.session_id().map(ToOwned::to_owned);

        // Write desired — the aggregate root's legitimate intent write-point.
        {
            let mut session = self.session.write().await;
            session.set_desired_mode(ModeId::new(&normalized_mode));
            self.commit_session_changes(&mut session).await;
        }

        // If a session is open, reconcile to the CLI. `reconcile_session`
        // is the sole call-site of `protocol.set_mode` and the sole
        // observed/advertised write-point — on success it calls
        // `apply_observed_mode`, which syncs both layers and emits
        // `ObservedModeSynced`. `get_mode()` reflects the change as soon
        // as the SDK call returns.
        if let Some(sid) = session_id {
            // PUT /mode is best-effort by design: the desired layer is
            // already updated, and any SDK failure (including
            // SessionNotFound) is handled on the next ensure_session
            // pass when the user retries or sends a message.
            if let Err(e) = self.reconcile_session(&sid).await {
                debug!(
                    conversation_id = %self.params.conversation_id,
                    error = %e,
                    "set_mode: reconcile failed; desired layer kept for next ensure_session"
                );
            }
        }
        Ok(())
    }

    /// Set the model for the current session.
    ///
    /// Mirrors `set_mode`: writes user intent into the aggregate's Desired
    /// layer, then delegates to `reconcile_session` for the SDK call.
    /// `reconcile_session` is the sole call-site of `protocol.set_model` —
    /// it also handles the observed sync since the CLI does not emit a
    /// CurrentModelUpdate notification after `session/set_model`.
    pub(crate) async fn set_model(&self, model_id: &str) -> Result<(), AppError> {
        let session_id = self.session.read().await.session_id().map(ToOwned::to_owned);

        {
            let mut session = self.session.write().await;
            if !session.can_select_model(model_id) {
                warn!(
                    conversation_id = %self.params.conversation_id,
                    model_id = %model_id,
                    "set_model rejected unavailable ACP model"
                );
                return Err(AppError::BadRequest(format!(
                    "Model '{model_id}' is not available for this ACP session"
                )));
            }
            session.set_desired_model(ModelId::new(model_id));
            self.commit_session_changes(&mut session).await;
        }

        if let Some(sid) = session_id {
            if let Err(e) = self.reconcile_session(&sid).await {
                debug!(
                    conversation_id = %self.params.conversation_id,
                    error = %e,
                    "set_model: reconcile failed; desired layer kept for next ensure_session"
                );
            }
        } else {
            return Err(AppError::BadRequest("No active session".into()));
        }
        Ok(())
    }

    /// Return available slash commands from the session aggregate.
    pub(crate) async fn load_slash_commands(&self) -> Result<Vec<SlashCommandItem>, AppError> {
        let session = self.session.read().await;
        let items = session
            .available_commands()
            .map(|cmds| {
                cmds.iter()
                    .map(|c| SlashCommandItem {
                        command: c.name.clone(),
                        description: c.description.clone(),
                    })
                    .collect()
            })
            .unwrap_or_default();
        Ok(items)
    }
}

impl AcpAgentManager {
    /// Current ACP session ID, if a session has been established.
    pub async fn session_id(&self) -> Option<String> {
        self.session.read().await.session_id().map(ToOwned::to_owned)
    }

    /// Restore a previously persisted session_id (e.g. from DB on task rebuild).
    /// Enables resume path on next send_message instead of creating a fresh session.
    ///
    /// Deliberately leaves `opened = false`: the CLI child process is
    /// brand new and still needs `session/load` (or claude-meta-resume) to
    /// re-attach to the persisted session before the next prompt. Subsequent
    /// turns — once the resume handshake has run — take the short path.
    pub async fn set_session_id(&self, sid: String) {
        let mut session = self.session.write().await;
        session.set_session_id(DomainSessionId::new(sid));
        session.drain_events();
    }

    /// Vendor label this session was spawned as (e.g. "claude"), if any.
    pub fn backend(&self) -> Option<&str> {
        self.params.metadata.backend.as_deref()
    }

    /// Agent metadata id this session was spawned from.
    pub fn agent_id(&self) -> &str {
        &self.params.metadata.id
    }

    /// Whether the configured agent supports side questions.
    pub fn supports_side_question(&self) -> bool {
        self.params.metadata.behavior_policy.supports_side_question
    }
}

impl AcpAgentManager {
    /// Ensure the ACP session is opened with the CLI. Does not send a
    /// prompt. Returns the session id that subsequent prompts should use
    /// (may differ from the input when claude-meta-resume rewrites it).
    ///
    /// Three paths mirror `ensure_session_and_send`:
    /// 1. No sid at all → `open_session_new`
    /// 2. Sid present but CLI has not opened it (fresh task) → `open_session_resume`
    /// 3. Already opened → noop, return the existing sid
    #[tracing::instrument(skip_all, fields(conversation_id = %self.params.conversation_id))]
    async fn ensure_session_opened(&self) -> Result<String, AppError> {
        debug!("Ensuring ACP session is opened");
        let _lock = self.session_lock.lock().await;

        let (session_id, opened) = {
            let s = self.session.read().await;
            (s.session_id().map(ToOwned::to_owned), s.is_opened())
        };

        let sid = match (session_id, opened) {
            (None, _) => self.open_session_new().await?,
            (Some(sid), false) => self.open_session_resume(&sid).await?,
            (Some(sid), true) => sid,
        };

        {
            let mut s = self.session.write().await;
            s.mark_opened();
            self.commit_session_changes(&mut s).await;
        }
        Ok(sid)
    }

    /// Initialize or resume a session, then send the user message.
    ///
    /// The prompt is passed through `self.pipeline.pre_send` before being
    /// forwarded to the CLI. Each hook in the pipeline reads one-shot flags
    /// on `AcpSession` (e.g. `pending_session_new_prelude`,
    /// `pending_model_notice`) and prepends the appropriate block when set.
    ///
    /// Returns `true` when the turn was cancelled (see
    /// [`prompt_existing_session`]'s contract — the terminal Finish for the
    /// turn was already emitted; callers must not emit another).
    async fn ensure_session_and_send(&self, data: &SendMessageData) -> Result<bool, AppError> {
        let sid = self.ensure_session_opened().await?;
        self.runtime.reset_for_new_turn(ConversationStatus::Running);

        let content = {
            let mut s = self.session.write().await;
            let mut ctx = PromptCtx {
                session: &mut s,
                params: &self.params,
                skill_manager: &self.skill_manager,
                runtime: &self.runtime,
            };
            let transformed = self.pipeline.pre_send(&mut ctx, data.content.clone()).await;
            self.commit_session_changes(&mut s).await;
            transformed
        };

        let data = SendMessageData {
            content,
            ..data.clone()
        };
        self.prompt_existing_session(&data, Some(&sid)).await
    }

    /// Pre-open the ACP session without sending a prompt. Called by the
    /// factory after `AcpAgentManager::build` so `POST /warmup` returns
    /// only after the session is ready to accept `set_mode` / `set_model`
    /// / `prompt`. Idempotent — if already opened, returns immediately.
    #[tracing::instrument(skip_all, fields(conversation_id = %self.params.conversation_id))]
    pub async fn warmup_session(&self) -> Result<(), AppError> {
        info!("Warming up ACP session");
        let result = self.ensure_session_opened().await.map(|_sid| ());
        match &result {
            Ok(()) => info!("ACP session warmed up"),
            Err(e) => warn!(error = %ErrorChain(e), "ACP session warmup failed"),
        }
        result
    }
}

#[async_trait::async_trait]
impl crate::agent_task::IAgentTask for AcpAgentManager {
    fn agent_type(&self) -> AgentType {
        AgentType::Acp
    }

    fn conversation_id(&self) -> &str {
        &self.params.conversation_id
    }

    fn workspace(&self) -> &str {
        &self.params.workspace.path
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

    #[tracing::instrument(skip_all, fields(conversation_id = %self.params.conversation_id, msg_id = %data.msg_id))]
    async fn send_message(&self, data: SendMessageData) -> Result<(), AgentSendError> {
        self.runtime.bump_activity();

        match self.ensure_session_and_send(&data).await {
            Ok(_cancelled) => {
                info!("ACP send_message completed");
                // The terminal Finish for this turn was already emitted from
                // exactly one place: `prompt_existing_session` on a normal
                // finish (with the real stop_reason), or `cancel()` on a user
                // cancel. Emitting a fallback Finish here would at best be
                // absorbed and at worst — when the cancel-ack returns after a
                // NEW turn already reset the status to Running — terminate
                // that new turn. So: no terminal emission here.
                Ok(())
            }
            Err(err) => {
                // Build a CloseReason that captures whatever context we still
                // have. Two cases matter:
                //   1. The CLI process has already exited — we can read the
                //      exit code/signal directly and run the stderr tail
                //      through the redaction allowlist, even if the SDK
                //      surfaced the failure as a generic JSON-RPC error.
                //   2. The process is still alive — fall back to the existing
                //      stderr-augmentation heuristic for the SDK's "default
                //      Internal error" shape; otherwise the user-facing form
                //      of the AppError is the best we can do.
                let close_reason = self.build_close_reason_from_error(&err).await;

                // Operator log: full error chain + the (raw, pre-redaction)
                // stderr peek so on-call can correlate. The redacted summary
                // is what reaches the UI.
                //
                // When the CLI process actually exited, surface the exit code
                // and signal as STRUCTURED fields (not just buried inside the
                // summary string) so a native fault — SIGSEGV/SIGABRT/SIGBUS,
                // e.g. a crash inside the Computer/a11y C-FFI that no Rust
                // error boundary can catch — is greppable in the operator log
                // and unambiguously distinguishable from a clean non-zero exit
                // or a Rust panic (exit code 101). This is the signal needed to
                // tell a native crash apart from a recoverable error when
                // triaging a restart loop.
                let summary = close_reason.user_facing_message();
                if let CloseReason::ProcessExited { exit_code, signal, .. } = &close_reason {
                    error!(
                        error = %ErrorChain(&err),
                        exit_code = ?exit_code,
                        signal = ?signal,
                        close_reason_summary = %summary,
                        "ACP send_message failed: CLI process exited"
                    );
                } else {
                    error!(error = %ErrorChain(&err), close_reason_summary = %summary, "ACP send_message failed");
                }

                {
                    let mut session = self.session.write().await;
                    session.record_close_reason(Some(close_reason));
                }
                let send_error = AgentSendError::from_app_error(err);
                self.runtime.emit_error_data(send_error.stream_error().clone());
                Err(send_error)
            }
        }
    }

    #[tracing::instrument(skip_all, fields(conversation_id = %self.params.conversation_id))]
    async fn cancel(&self) -> Result<(), AppError> {
        info!("Cancelling ACP session");
        let session_id = self.session.read().await.session_id().map(ToOwned::to_owned);
        if let Some(sid) = &session_id {
            self.protocol
                .cancel(CancelNotification::new(SessionId::new(sid.as_str())));
        }
        self.permission_router.cancel_all();

        // Distinguish a deliberate user-cancel from a crash: the toast can
        // say "Conversation cancelled" instead of a generic "session closed".
        // We still emit Finish (not Error) here — cancel is a clean
        // termination — but record the close reason so anyone consulting
        // the aggregate state for diagnostics sees the canonical signal.
        {
            let mut session = self.session.write().await;
            session.record_close_reason(Some(CloseReason::UserCancel));
        }

        // Force status to Finished and emit unconditionally, bypassing the
        // absorbing-state guard. This ensures StreamRelay always receives
        // its terminal event regardless of prior state. The Finish carries
        // stop_reason=Cancelled so downstream consumers (AutoWork, IDMM) see
        // a deliberate user stop, NOT a clean completion — emitting `None`
        // here historically made AutoWork finalize a user-aborted requirement
        // as done and immediately claim the next one.
        self.runtime.reset_for_new_turn(ConversationStatus::Finished);
        self.runtime
            .emit(AgentStreamEvent::Finish(FinishEventData {
                session_id: None,
                stop_reason: Some(TurnStopReason::Cancelled),
            }));

        Ok(())
    }

    fn kill(&self, reason: Option<AgentKillReason>) -> Result<(), AppError> {
        info!(
            conversation_id = %self.params.conversation_id,
            ?reason,
            "Killing ACP agent"
        );

        // Mark closing to prevent reconnect attempts
        self.permission_router.set_closing();

        // Cancel the current session if active
        if let Ok(session) = self.session.try_read()
            && let Some(sid) = session.session_id()
        {
            self.protocol.cancel(CancelNotification::new(SessionId::new(sid)));
        }

        let process = Arc::clone(&self.process);
        let grace = Duration::from_millis(ACP_KILL_GRACE_MS);
        let conversation_id = self.params.conversation_id.clone();
        let pid = process.pid();

        tokio::spawn(async move {
            if let Err(e) = process.kill(grace).await {
                // Tag the failure with conversation_id + pid so Sentry can
                // group these and ops can correlate with the matching
                // "Killing ACP agent" log line. ELECTRON-1E9: an unannotated
                // failure here on Windows left the CLI subprocess running
                // while the manager believed it had been torn down,
                // producing the "no reply / second send hangs" symptom.
                error!(
                    %conversation_id,
                    pid,
                    error = %ErrorChain(&e),
                    "Failed to kill ACP process"
                );
            } else {
                debug!(%conversation_id, pid, "ACP process kill completed");
            }
        });

        self.permission_router.cancel_all();

        // m1 fix: emit error with the kill reason so the status goes to
        // Finished and subscribers see a terminal event. Idempotent.
        // Source of truth for the toast text is `CloseReason::Killed`.
        let close_reason = CloseReason::Killed { reason };
        let message = close_reason.user_facing_message();
        if let Ok(mut session) = self.session.try_write() {
            session.record_close_reason(Some(close_reason));
        }
        self.runtime.emit_error(message);

        Ok(())
    }
}

impl AcpAgentManager {
    pub fn kill_and_wait(
        &self,
        reason: Option<AgentKillReason>,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>> {
        let _ = crate::agent_task::IAgentTask::kill(self, reason);
        let process = Arc::clone(&self.process);
        let grace = Duration::from_millis(ACP_KILL_GRACE_MS);
        Box::pin(async move {
            let _ = process.kill(grace).await;
        })
    }

    /// Pending ACP permission prompts recoverable through the conversation
    /// confirmation API.
    pub fn get_confirmations(&self) -> Vec<nomifun_common::Confirmation> {
        self.permission_router.get_confirmations()
    }

    /// Submit a permission response for a pending tool call. ACP confirms
    /// always carry an `option_id`; `always_allow` is consumed by the CLI
    /// and is not reflected in the local approval memory (the ACP CLI
    /// tracks its own).
    pub fn confirm(
        &self,
        _msg_id: &str,
        call_id: &str,
        data: serde_json::Value,
        _always_allow: bool,
    ) -> Result<(), AppError> {
        let option_id = confirm_option_id(&data)
            .ok_or_else(|| AppError::BadRequest("ACP confirmation requires an option_id string".into()))?;

        self.permission_router
            .confirm(call_id, option_id, &self.params.conversation_id)
    }
}

impl AcpAgentManager {
    /// Clear the conversation context in place ("release model context").
    ///
    /// Cancels any in-flight turn, forgets pending permission prompts, and
    /// drops the cached ACP `session_id` + `opened` flag. The CLI subprocess
    /// stays alive (warm), but the next prompt takes the `open_session_new`
    /// branch in [`Self::ensure_session_opened`] — a brand-new ACP session
    /// with zero history, with `pending_session_new_prelude` re-injecting the
    /// first-message context. The persisted `acp_session` row is reset
    /// separately by the conversation service so a process rebuild also starts
    /// fresh instead of resuming the old session.
    pub(crate) async fn clear_context(&self) -> Result<(), AppError> {
        info!(
            conversation_id = %self.params.conversation_id,
            "Clearing ACP context"
        );
        let _lock = self.session_lock.lock().await;

        // Stop any in-flight turn on the session we are about to forget.
        let session_id = self.session.read().await.session_id().map(ToOwned::to_owned);
        if let Some(sid) = &session_id {
            self.protocol
                .cancel(CancelNotification::new(SessionId::new(sid.as_str())));
        }
        self.permission_router.cancel_all();

        // Drop the local sid + opened flag → next ensure_session_opened opens
        // a fresh session/new with no accumulated context.
        {
            let mut session = self.session.write().await;
            session.clear_session_id();
            self.commit_session_changes(&mut session).await;
        }
        Ok(())
    }
}

// `augment_with_stderr` and `build_close_reason_from_error` live in
// `agent_close.rs` to keep this file under the 1000-line budget.

#[cfg(test)]
mod tests {
    use super::{exit_status_parts, user_facing_message};
    use nomifun_common::AppError;

    #[test]
    fn exit_status_parts_handles_missing_status() {
        assert_eq!(exit_status_parts(None), (None, None));
    }

    #[cfg(unix)]
    #[test]
    fn exit_status_parts_extracts_unix_exit_code() {
        // ExitStatus::from_raw is the only stable constructor. On Unix the
        // low 8 bits are the signal; bits 8..15 are the exit code when the
        // process exited normally.
        use std::os::unix::process::ExitStatusExt;
        let status = std::process::ExitStatus::from_raw(1 << 8); // exit 1
        let (code, signal) = exit_status_parts(Some(status));
        assert_eq!(code, Some(1));
        assert_eq!(signal, None);
    }

    #[test]
    fn strips_bad_gateway_prefix() {
        let err = AppError::BadGateway("API Error: Internal server error".into());
        assert_eq!(user_facing_message(&err), "API Error: Internal server error");
    }

    #[test]
    fn strips_not_found_prefix() {
        let err = AppError::NotFound("user 42".into());
        assert_eq!(user_facing_message(&err), "user 42");
    }

    #[test]
    fn rate_limited_has_no_colon_returns_full_string() {
        let err = AppError::RateLimited;
        assert_eq!(user_facing_message(&err), "Rate limited");
    }

    #[test]
    fn nested_colons_only_strip_first() {
        // "Bad gateway: Internal error: API Error: ..." → keep everything after the first ": "
        let err = AppError::BadGateway("Internal error: API Error: Internal server error".into());
        assert_eq!(
            user_facing_message(&err),
            "Internal error: API Error: Internal server error"
        );
    }

    // ---- augment_with_stderr behavioral tests ------------------------------
    //
    // We can't easily construct a real AcpAgentManager in a unit test (it
    // needs the full ACP plumbing). Instead we test the *composition* of
    // Task 3's peek_stderr_tail + Task 4's extract_error_message + this
    // task's "SDK default Display" shape detection by spawning a real
    // CliAgentProcess that writes the chosen stderr, then running the same
    // detection+peek+extract pipeline against it.
    //
    // The helper below MIRRORS `AcpAgentManager::augment_with_stderr`. If
    // you change the production helper (e.g. the prefix string, peek line
    // count, or extractor module path) update this helper to match.

    use super::CliAgentProcess;
    use std::sync::Arc;
    use std::time::Duration;

    /// Spawn a sh subprocess that writes `stderr_payload` to stderr then
    /// exits with `exit_code`. Used to simulate ACP CLI crashes/exits in
    /// close-path tests. Lines containing `'` are escaped for the heredoc.
    async fn spawn_with_stderr_and_exit(stderr_payload: &str, exit_code: u8) -> Arc<CliAgentProcess> {
        use nomifun_common::CommandSpec;
        let payload = stderr_payload.replace('\'', "'\\''");
        let script = format!("cat <<'EOF' >&2\n{payload}\nEOF\nexit {exit_code}");
        let config = CommandSpec {
            command: "sh".into(),
            args: vec!["-c".into(), script],
            env: vec![],
            cwd: None,
        };
        let proc = CliAgentProcess::spawn(config).await.unwrap();
        tokio::time::timeout(Duration::from_secs(5), proc.wait_for_exit())
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(100)).await;
        Arc::new(proc)
    }

    async fn spawn_with_stderr(stderr_payload: &str) -> Arc<CliAgentProcess> {
        spawn_with_stderr_and_exit(stderr_payload, 0).await
    }

    async fn augment_via_process(proc: &Arc<CliAgentProcess>, err: &AppError) -> Option<String> {
        const SDK_DEFAULT_BAD_GATEWAY_PREFIX: &str = "Bad gateway: Agent internal error (code ";
        let display = err.to_string();
        let is_default_internal = display.starts_with(SDK_DEFAULT_BAD_GATEWAY_PREFIX) && display.ends_with(')');
        if !is_default_internal {
            return None;
        }
        // Mirror the production STDERR_PEEK_LINES (32). If you change one, change both.
        let tail = proc.peek_stderr_tail(32).await;
        super::super::stderr_error_extractor::extract_error_message(&tail)
    }

    #[tokio::test]
    async fn augments_when_codex_usage_limit_in_stderr() {
        let stderr = "\u{1b}[2m2026-05-13T20:01:21Z\u{1b}[0m \u{1b}[31mERROR\u{1b}[0m codex_acp::thread: Unhandled error during turn: You've hit your usage limit. Try again later. Some(UsageLimitExceeded)";
        let proc = spawn_with_stderr(stderr).await;
        let err = AppError::BadGateway("Agent internal error (code -32603)".into());

        let augmented = augment_via_process(&proc, &err).await;
        let msg = augmented.expect("must augment when stderr matches allowlist");
        assert!(msg.to_lowercase().contains("usage limit"), "got {msg}");
    }

    #[tokio::test]
    async fn does_not_augment_when_message_is_specific() {
        // 1BF case: SDK already gave us a real message → don't second-guess.
        let proc = spawn_with_stderr("ERROR something: usage limit exceeded").await;
        let err = AppError::BadGateway("Internal error: API Error: Internal server error".into());

        assert!(augment_via_process(&proc, &err).await.is_none());
    }

    #[tokio::test]
    async fn returns_none_when_stderr_has_no_allowlisted_keywords() {
        let stderr = "ERROR widget_loader: failed to load module 'foo'";
        let proc = spawn_with_stderr(stderr).await;
        let err = AppError::BadGateway("Agent internal error (code -32603)".into());

        assert!(augment_via_process(&proc, &err).await.is_none());
    }

    // Close-reason compositional tests live in `agent_close.rs` so that
    // (a) `agent.rs` stays under the 1000-line budget, and (b) the test
    // suite for the close-path helpers sits next to the production logic.
}

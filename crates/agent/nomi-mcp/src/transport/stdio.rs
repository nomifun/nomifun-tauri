use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use nomi_process_runtime::{
    ChildProcessBuilder, ChildProcessCleanup, kill_process_tree,
};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, BufWriter};
use tokio::process::{Child, ChildStdin, ChildStdout};
use tokio::sync::{Mutex, watch};
use tokio_util::sync::CancellationToken;

use super::{McpError, McpTransport};
use crate::protocol::{
    ClientCapabilities, ClientInfo, InitializeParams, JsonRpcRequest, JsonRpcResponse,
};

/// Maximum number of automatic respawns within [`RESPAWN_WINDOW`] before the
/// transport gives up and surfaces a hard error. Without a ceiling a server
/// that crashes on every `initialize` would spin forever (crashloop). Codex's
/// rmcp client makes the same trade-off — bounded restarts, then fail loud.
const MAX_RESPAWNS: u32 = 3;

/// Sliding window over which [`MAX_RESPAWNS`] is counted. A server that is
/// healthy for this long resets its respawn budget, so a single crash months
/// into a long session does not consume the lifetime quota.
const RESPAWN_WINDOW: Duration = Duration::from_secs(60);

/// Base backoff before the first respawn; doubled per consecutive attempt
/// (200ms, 400ms, 800ms…) and capped at [`MAX_BACKOFF`]. Gives a flapping
/// child a moment to settle without stalling the caller for long.
const BASE_BACKOFF: Duration = Duration::from_millis(200);
const MAX_BACKOFF: Duration = Duration::from_secs(2);

/// Immutable parameters needed to (re)spawn the child and redo the MCP
/// handshake. Captured once at construction so respawn never needs the caller.
struct SpawnSpec {
    command: String,
    args: Vec<String>,
    env: HashMap<String, String>,
    init_params: InitializeParams,
}

/// The live child process and its piped stdio. Replaced wholesale on respawn so
/// a half-dead connection (e.g. stdin alive but stdout at EOF) is never reused.
struct Connection {
    stdin: BufWriter<ChildStdin>,
    stdout: BufReader<ChildStdout>,
    child: Child,
    cleanup: ChildProcessCleanup,
    cleanup_result: Option<watch::Sender<Option<CleanupOutcome>>>,
}

type CleanupOutcome = Result<(), String>;

pub(crate) struct ConnectionCleanupRegistry {
    receipts: std::sync::Mutex<Vec<watch::Receiver<Option<CleanupOutcome>>>>,
    failure: std::sync::Mutex<Option<String>>,
}

impl ConnectionCleanupRegistry {
    pub(crate) fn new() -> Arc<Self> {
        Arc::new(Self {
            receipts: std::sync::Mutex::new(Vec::new()),
            failure: std::sync::Mutex::new(None),
        })
    }

    /// Register the cleanup obligation before the connection can be published
    /// or used. A dropped connection therefore closes an unresolved receipt
    /// instead of disappearing from the exact-shutdown proof.
    fn track_connection(&self) -> watch::Sender<Option<CleanupOutcome>> {
        let (result_tx, result_rx) = watch::channel(None);
        self.receipts
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .push(result_rx);
        result_tx
    }

    fn retire(
        self: &Arc<Self>,
        mut conn: Connection,
    ) -> Result<watch::Receiver<Option<CleanupOutcome>>, McpError> {
        let result_tx = conn.cleanup_result.take().ok_or_else(|| {
            McpError::Transport("MCP connection cleanup was already retired".to_owned())
        })?;
        let result_rx = result_tx.subscribe();
        let registry = Arc::clone(self);
        let runtime = match tokio::runtime::Handle::try_current() {
            Ok(runtime) => runtime,
            Err(_) => {
                let error =
                    "Cannot schedule MCP connection cleanup outside a Tokio runtime".to_owned();
                *self
                    .failure
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(error.clone());
                result_tx.send_replace(Some(Err(error.clone())));
                return Err(McpError::Transport(error));
            }
        };
        runtime.spawn(async move {
            let outcome = StdioTransport::close_connection(conn)
                .await
                .map_err(|error| error.to_string());
            if let Err(error) = &outcome {
                let mut failure = registry
                    .failure
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                if failure.is_none() {
                    *failure = Some(error.clone());
                }
            }
            result_tx.send_replace(Some(outcome));
        });
        Ok(result_rx)
    }

    async fn wait_receipt(
        mut receipt: watch::Receiver<Option<CleanupOutcome>>,
    ) -> Result<(), McpError> {
        loop {
            if let Some(result) = receipt.borrow().clone() {
                return result.map_err(McpError::Transport);
            }
            receipt.changed().await.map_err(|_| {
                McpError::Transport(
                    "MCP cleanup custodian stopped without an exact result".to_owned(),
                )
            })?;
        }
    }

    pub(crate) async fn wait_all(&self) -> Result<(), McpError> {
        let receipts = self
            .receipts
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone();
        let mut first_error = None;
        for receipt in receipts {
            if let Err(error) = Self::wait_receipt(receipt).await
                && first_error.is_none()
            {
                first_error = Some(error.to_string());
            }
        }
        if let Some(error) = self
            .failure
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
        {
            first_error.get_or_insert(error);
        }
        match first_error {
            Some(error) => Err(McpError::Transport(error)),
            None => Ok(()),
        }
    }
}

struct ConnectionOwner {
    conn: Option<Connection>,
    cleanup_registry: Arc<ConnectionCleanupRegistry>,
}

impl ConnectionOwner {
    fn new(conn: Connection, cleanup_registry: Arc<ConnectionCleanupRegistry>) -> Self {
        Self {
            conn: Some(conn),
            cleanup_registry,
        }
    }

    fn as_mut(&mut self) -> &mut Connection {
        self.conn
            .as_mut()
            .expect("MCP connection owner used after transfer")
    }

    fn take(&mut self) -> Connection {
        self.conn
            .take()
            .expect("MCP connection owner transferred twice")
    }

    async fn retire(mut self) -> Result<(), McpError> {
        let conn = self.take();
        let receipt = self.cleanup_registry.retire(conn)?;
        ConnectionCleanupRegistry::wait_receipt(receipt).await
    }
}

impl Drop for ConnectionOwner {
    fn drop(&mut self) {
        if let Some(conn) = self.conn.take()
            && let Err(error) = self.cleanup_registry.retire(conn)
        {
            let mut failure = self
                .cleanup_registry
                .failure
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if failure.is_none() {
                *failure = Some(error.to_string());
            }
        }
    }
}

/// Stdio transport: communicates with an MCP server via a child process's
/// stdin/stdout. On a detected pipe failure (EOF / broken pipe), it transparently
/// respawns the child and re-runs the `initialize` handshake, with a bounded
/// retry budget to avoid crashlooping.
pub struct StdioTransport {
    conn: Arc<Mutex<Option<Connection>>>,
    spec: SpawnSpec,
    next_id: AtomicU64,
    /// Respawn bookkeeping: count within the current window + window start.
    respawn_state: Mutex<RespawnState>,
    /// Absorbing close authority. Set before waiting for request/respawn locks
    /// so no delayed backoff or handshake can commit a replacement child.
    closed: AtomicBool,
    closing: CancellationToken,
    respawn_gate: Mutex<()>,
    close_error: std::sync::Mutex<Option<String>>,
    cleanup_registry: Arc<ConnectionCleanupRegistry>,
}

#[derive(Default)]
struct RespawnState {
    /// Respawns inside the current window.
    count: u32,
    /// When the current window began (monotonic). `None` until the first respawn.
    window_start: Option<std::time::Instant>,
}

impl StdioTransport {
    /// Spawn a child process and return the transport.
    ///
    /// `init_params` are retained so a respawn can replay the `initialize`
    /// handshake without the manager's involvement.
    pub async fn spawn(
        command: &str,
        args: &[String],
        env: &HashMap<String, String>,
    ) -> Result<Self, McpError> {
        // Default handshake params; the manager normally drives `initialize`
        // itself on first connect, but a respawn must be self-contained.
        let init_params = InitializeParams {
            protocol_version: "2025-03-26".to_string(),
            capabilities: ClientCapabilities {
                tools: Some(serde_json::json!({})),
            },
            client_info: ClientInfo {
                name: "nomi".to_string(),
                version: "0.3.0".to_string(),
            },
        };
        Self::spawn_with_init(command, args, env, init_params).await
    }

    /// Spawn with explicit handshake params (kept for the respawn path and for
    /// callers that want to control the `initialize` payload).
    pub async fn spawn_with_init(
        command: &str,
        args: &[String],
        env: &HashMap<String, String>,
        init_params: InitializeParams,
    ) -> Result<Self, McpError> {
        Self::spawn_with_cleanup_registry(
            command,
            args,
            env,
            init_params,
            ConnectionCleanupRegistry::new(),
        )
        .await
    }

    pub(crate) async fn spawn_with_cleanup_registry(
        command: &str,
        args: &[String],
        env: &HashMap<String, String>,
        init_params: InitializeParams,
        cleanup_registry: Arc<ConnectionCleanupRegistry>,
    ) -> Result<Self, McpError> {
        let spec = SpawnSpec {
            command: command.to_string(),
            args: args.to_vec(),
            env: env.clone(),
            init_params,
        };
        let conn = Self::spawn_child(&spec, &cleanup_registry).await?;
        Ok(Self {
            conn: Arc::new(Mutex::new(Some(conn))),
            spec,
            next_id: AtomicU64::new(1),
            respawn_state: Mutex::new(RespawnState::default()),
            closed: AtomicBool::new(false),
            closing: CancellationToken::new(),
            respawn_gate: Mutex::new(()),
            close_error: std::sync::Mutex::new(None),
            cleanup_registry,
        })
    }

    /// Launch the child process and capture its piped stdio.
    async fn spawn_child(
        spec: &SpawnSpec,
        cleanup_registry: &ConnectionCleanupRegistry,
    ) -> Result<Connection, McpError> {
        let mut cmd = ChildProcessBuilder::new(&spec.command);
        cmd.args(&spec.args)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::inherit())
            .envs(&spec.env);
        // Put the child in its own process group so killing it takes down any
        // grandchildren (npx → node, etc.) instead of orphaning them.
        // CREATE_NO_WINDOW: MCP stdio servers (npx/node/bun/python) must not
        // flash a console window under a GUI host.

        let (mut child, cleanup) = cmd.spawn_with_cleanup().map_err(|e| {
            McpError::Transport(format!("Failed to spawn '{}': {}", spec.command, e))
        })?;

        let (Some(stdin), Some(stdout)) = (child.stdin.take(), child.stdout.take()) else {
            let kill_result = kill_process_tree(&mut child).await;
            let cleanup_result = cleanup.wait().await;
            return Err(McpError::Transport(format!(
                "Failed to capture child stdio; kill={kill_result:?}; cleanup={cleanup_result:?}"
            )));
        };

        Ok(Connection {
            stdin: BufWriter::new(stdin),
            stdout: BufReader::new(stdout),
            child,
            cleanup,
            cleanup_result: Some(cleanup_registry.track_connection()),
        })
    }

    fn closed_error(&self) -> McpError {
        McpError::Transport(format!("MCP stdio transport '{}' is closed", self.spec.command))
    }

    fn ensure_open(&self) -> Result<(), McpError> {
        if self.closed.load(Ordering::Acquire) {
            Err(self.closed_error())
        } else {
            Ok(())
        }
    }

    async fn close_connection(mut conn: Connection) -> Result<(), McpError> {
        let _ = conn.stdin.shutdown().await;
        drop(conn.stdin);
        drop(conn.stdout);
        let kill_result = kill_process_tree(&mut conn.child).await;
        let cleanup_result = conn.cleanup.wait().await;
        kill_result?;
        cleanup_result?;
        Ok(())
    }

    /// Get the next request ID
    pub fn next_id(&self) -> u64 {
        self.next_id.fetch_add(1, Ordering::Relaxed)
    }

    /// Serialize and write a JSON-RPC message to the child's stdin (one line +
    /// newline + flush). Errors here mean the write pipe is broken.
    async fn send_on(conn: &mut Connection, req: &JsonRpcRequest) -> Result<(), McpError> {
        let json = serde_json::to_string(req)
            .map_err(|e| McpError::Transport(format!("JSON serialize error: {}", e)))?;

        conn.stdin
            .write_all(json.as_bytes())
            .await
            .map_err(|e| McpError::Transport(format!("Write to stdin failed: {}", e)))?;
        conn.stdin
            .write_all(b"\n")
            .await
            .map_err(|e| McpError::Transport(format!("Write newline failed: {}", e)))?;
        conn.stdin
            .flush()
            .await
            .map_err(|e| McpError::Transport(format!("Flush stdin failed: {}", e)))?;
        Ok(())
    }

    /// Read a single JSON-RPC response line from the child's stdout, skipping
    /// blank lines. A zero-byte read means the child closed stdout (EOF).
    async fn read_response_on(conn: &mut Connection) -> Result<JsonRpcResponse, McpError> {
        let mut line = String::new();
        loop {
            line.clear();
            let bytes_read = conn
                .stdout
                .read_line(&mut line)
                .await
                .map_err(|e| McpError::Transport(format!("Read from stdout failed: {}", e)))?;

            if bytes_read == 0 {
                return Err(McpError::Transport("Child process stdout closed".into()));
            }

            let trimmed = line.trim();
            if !trimmed.is_empty() {
                let response: JsonRpcResponse = serde_json::from_str(trimmed).map_err(|e| {
                    McpError::Transport(format!(
                        "Failed to parse JSON-RPC response: {} — raw: {}",
                        e, trimmed
                    ))
                })?;
                return Ok(response);
            }
        }
    }

    /// One round-trip on the given connection: write request, read response,
    /// surface any JSON-RPC error. Used both directly and during the re-handshake.
    async fn roundtrip_on(
        conn: &mut Connection,
        req: &JsonRpcRequest,
    ) -> Result<JsonRpcResponse, McpError> {
        Self::send_on(conn, req).await?;
        let response = Self::read_response_on(conn).await?;
        if let Some(err) = &response.error {
            return Err(McpError::JsonRpc {
                code: err.code,
                message: err.message.clone(),
            });
        }
        Ok(response)
    }

    /// True for failures that indicate the child/pipe is gone and a respawn is
    /// warranted. JSON-RPC application errors (the server answered, just with an
    /// error) and serialize/parse failures are NOT respawn-worthy — respawning
    /// would not change the outcome.
    fn is_pipe_failure(err: &McpError) -> bool {
        match err {
            McpError::Transport(msg) => {
                msg.contains("stdout closed")
                    || msg.contains("Write to stdin failed")
                    || msg.contains("Write newline failed")
                    || msg.contains("Flush stdin failed")
                    || msg.contains("Read from stdout failed")
            }
            McpError::Io(_) => true,
            _ => false,
        }
    }

    /// Respawn the child and replay the `initialize` + `notifications/initialized`
    /// handshake, honouring the bounded retry budget. On success the live
    /// connection is swapped in place. Returns an error (without panicking) when
    /// the budget is exhausted or the new child fails to handshake.
    async fn respawn(&self) -> Result<(), McpError> {
        let _respawn = self.respawn_gate.lock().await;
        self.ensure_open()?;
        // Enforce the crashloop ceiling within a sliding window.
        {
            let mut state = self.respawn_state.lock().await;
            let now = std::time::Instant::now();
            match state.window_start {
                Some(start) if now.duration_since(start) <= RESPAWN_WINDOW => {
                    if state.count >= MAX_RESPAWNS {
                        return Err(McpError::Transport(format!(
                            "MCP stdio server '{}' exceeded {} respawns within {}s; giving up",
                            self.spec.command,
                            MAX_RESPAWNS,
                            RESPAWN_WINDOW.as_secs()
                        )));
                    }
                    state.count += 1;
                }
                _ => {
                    // First respawn, or the previous window has elapsed → reset.
                    state.window_start = Some(now);
                    state.count = 1;
                }
            }
        }

        // Backoff before respawning (exponential, capped). Read attempt count
        // again under the lock-free local; `count` was just incremented above.
        let attempt = {
            let state = self.respawn_state.lock().await;
            state.count
        };
        let backoff = BASE_BACKOFF
            .saturating_mul(1u32 << attempt.saturating_sub(1).min(5))
            .min(MAX_BACKOFF);
        tokio::select! {
            _ = self.closing.cancelled() => return Err(self.closed_error()),
            _ = tokio::time::sleep(backoff) => {}
        }
        self.ensure_open()?;

        tracing::warn!(
            target: "nomi_mcp",
            command = %self.spec.command,
            attempt,
            backoff_ms = backoff.as_millis() as u64,
            "respawning crashed MCP stdio server"
        );

        // Spawn a fresh child and run the handshake on it before publishing it,
        // so a half-initialized child never becomes the live connection.
        let mut new_conn = ConnectionOwner::new(
            Self::spawn_child(&self.spec, &self.cleanup_registry).await?,
            Arc::clone(&self.cleanup_registry),
        );
        if self.closed.load(Ordering::Acquire) {
            let closed = self.closed_error();
            let cleanup = new_conn.retire().await;
            return match cleanup {
                Ok(()) => Err(closed),
                Err(cleanup) => Err(McpError::Transport(format!(
                    "{closed}; replacement cleanup failed: {cleanup}"
                ))),
            };
        }

        let init_req = JsonRpcRequest::new(
            1,
            "initialize",
            Some(serde_json::to_value(&self.spec.init_params).map_err(|e| {
                McpError::InitFailed(format!("Failed to serialize init params: {}", e))
            })?),
        );
        let handshake = async {
            Self::roundtrip_on(new_conn.as_mut(), &init_req)
                .await
                .map_err(|e| McpError::InitFailed(format!("respawn initialize failed: {e}")))?;
            let initialized = JsonRpcRequest::notification("notifications/initialized", None);
            Self::send_on(new_conn.as_mut(), &initialized).await
        };
        let handshake_result = tokio::select! {
            _ = self.closing.cancelled() => Err(self.closed_error()),
            result = handshake => result,
        };
        if let Err(handshake_error) = handshake_result {
            let cleanup = new_conn.retire().await;
            return match cleanup {
                Ok(()) => Err(handshake_error),
                Err(cleanup) => Err(McpError::Transport(format!(
                    "{handshake_error}; replacement cleanup failed: {cleanup}"
                ))),
            };
        }

        // Swap in the healthy connection. The old `Connection` is dropped here;
        // `kill_on_drop(true)` reaps the dead child's process group.
        let old_conn = {
            let mut conn = self.conn.lock().await;
            if let Err(closed) = self.ensure_open() {
                drop(conn);
                let cleanup = new_conn.retire().await;
                return match cleanup {
                    Ok(()) => Err(closed),
                    Err(cleanup) => Err(McpError::Transport(format!(
                        "{closed}; replacement cleanup failed: {cleanup}"
                    ))),
                };
            }
            conn.replace(new_conn.take())
        };
        if let Some(old_conn) = old_conn {
            let receipt = self.cleanup_registry.retire(old_conn)?;
            ConnectionCleanupRegistry::wait_receipt(receipt).await?;
        }

        tracing::info!(
            target: "nomi_mcp",
            command = %self.spec.command,
            "MCP stdio server respawned and re-initialized"
        );
        Ok(())
    }
}

#[async_trait]
impl McpTransport for StdioTransport {
    async fn request(&self, req: &JsonRpcRequest) -> Result<JsonRpcResponse, McpError> {
        self.ensure_open()?;
        // First attempt on the current connection.
        let first = {
            let mut conn = self.conn.lock().await;
            let conn = conn.as_mut().ok_or_else(|| self.closed_error())?;
            tokio::select! {
                _ = self.closing.cancelled() => Err(self.closed_error()),
                result = Self::roundtrip_on(conn, req) => result,
            }
        };

        match first {
            Ok(resp) => Ok(resp),
            // Do NOT auto-respawn while the handshake itself is in flight: the
            // respawn path *runs* `initialize`, so retrying an `initialize`
            // request afterwards would double-initialize the fresh child and
            // desync the protocol. First-connect handshake failures are already
            // handled non-fatally by the manager.
            Err(err) if Self::is_pipe_failure(&err) && !is_handshake_method(&req.method) => {
                // The child/pipe died. Respawn + re-handshake, then retry once.
                self.respawn().await?;
                let mut conn = self.conn.lock().await;
                let conn = conn.as_mut().ok_or_else(|| self.closed_error())?;
                tokio::select! {
                    _ = self.closing.cancelled() => Err(self.closed_error()),
                    result = Self::roundtrip_on(conn, req) => result,
                }
            }
            Err(err) => Err(err),
        }
    }

    async fn notify(&self, req: &JsonRpcRequest) -> Result<(), McpError> {
        self.ensure_open()?;
        let first = {
            let mut conn = self.conn.lock().await;
            let conn = conn.as_mut().ok_or_else(|| self.closed_error())?;
            tokio::select! {
                _ = self.closing.cancelled() => Err(self.closed_error()),
                result = Self::send_on(conn, req) => result,
            }
        };
        match first {
            Ok(()) => Ok(()),
            Err(err) if Self::is_pipe_failure(&err) && !is_handshake_method(&req.method) => {
                self.respawn().await?;
                let mut conn = self.conn.lock().await;
                let conn = conn.as_mut().ok_or_else(|| self.closed_error())?;
                tokio::select! {
                    _ = self.closing.cancelled() => Err(self.closed_error()),
                    result = Self::send_on(conn, req) => result,
                }
            }
            Err(err) => Err(err),
        }
    }

    async fn close(&self) -> Result<(), McpError> {
        self.closed.store(true, Ordering::Release);
        self.closing.cancel();
        let _respawn = self.respawn_gate.lock().await;
        let conn = self.conn.lock().await.take();
        let result = if let Some(conn) = conn {
            match self.cleanup_registry.retire(conn) {
                Ok(receipt) => ConnectionCleanupRegistry::wait_receipt(receipt).await,
                Err(error) => Err(error),
            }
        } else {
            Ok(())
        }
        .and(self.cleanup_registry.wait_all().await);
        if let Err(error) = &result {
            *self
                .close_error
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(error.to_string());
        }
        match self
            .close_error
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
        {
            Some(error) => Err(McpError::Transport(error)),
            None => result,
        }
    }
}

impl Drop for StdioTransport {
    fn drop(&mut self) {
        self.closed.store(true, Ordering::Release);
        self.closing.cancel();

        if let Ok(mut conn) = self.conn.try_lock() {
            if let Some(conn) = conn.take()
                && let Err(error) = self.cleanup_registry.retire(conn)
            {
                let mut failure = self
                    .cleanup_registry
                    .failure
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                if failure.is_none() {
                    *failure = Some(error.to_string());
                }
            }
            return;
        }

        // A cancelled request/handshake may still be unwinding its async mutex
        // guard while the transport itself is dropped. Keep the slot alive and
        // transfer the connection only after that guard releases. The cleanup
        // receipt was registered at spawn time, so manager shutdown cannot
        // mistake this asynchronous hand-off for a completed cleanup.
        let conn = Arc::clone(&self.conn);
        let cleanup_registry = Arc::clone(&self.cleanup_registry);
        match tokio::runtime::Handle::try_current() {
            Ok(runtime) => {
                runtime.spawn(async move {
                    if let Some(conn) = conn.lock().await.take()
                        && let Ok(receipt) = cleanup_registry.retire(conn)
                    {
                        let _ = ConnectionCleanupRegistry::wait_receipt(receipt).await;
                    }
                });
            }
            Err(_) => {
                let mut failure = self
                    .cleanup_registry
                    .failure
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                if failure.is_none() {
                    *failure = Some(
                        "Cannot schedule dropped MCP transport cleanup outside a Tokio runtime"
                            .to_owned(),
                    );
                }
            }
        }
    }
}

/// Handshake methods must not trigger the auto-respawn retry (respawn already
/// replays the handshake; retrying would double-initialize the new child).
fn is_handshake_method(method: &str) -> bool {
    matches!(method, "initialize" | "notifications/initialized")
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------
#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(not(windows))]
    use serde_json::json;

    /// Pure-unit checks on the failure classifier — these need no child process.
    #[test]
    fn pipe_failure_classifies_transport_eof_and_io() {
        assert!(StdioTransport::is_pipe_failure(&McpError::Transport(
            "Child process stdout closed".into()
        )));
        assert!(StdioTransport::is_pipe_failure(&McpError::Transport(
            "Write to stdin failed: broken pipe".into()
        )));
        assert!(StdioTransport::is_pipe_failure(&McpError::Transport(
            "Read from stdout failed: x".into()
        )));
        assert!(StdioTransport::is_pipe_failure(&McpError::Io(
            std::io::Error::new(std::io::ErrorKind::BrokenPipe, "boom")
        )));
    }

    #[test]
    fn pipe_failure_excludes_jsonrpc_and_parse_errors() {
        // A JSON-RPC application error means the server answered — not a dead
        // pipe; respawning would not help, so it must NOT be classified as one.
        assert!(!StdioTransport::is_pipe_failure(&McpError::JsonRpc {
            code: -32601,
            message: "method not found".into(),
        }));
        // A parse failure is a protocol/serialize issue, not a broken pipe.
        assert!(!StdioTransport::is_pipe_failure(&McpError::Transport(
            "Failed to parse JSON-RPC response: x — raw: {".into()
        )));
    }

    #[test]
    fn handshake_methods_are_excluded_from_respawn() {
        assert!(is_handshake_method("initialize"));
        assert!(is_handshake_method("notifications/initialized"));
        assert!(!is_handshake_method("tools/call"));
        assert!(!is_handshake_method("tools/list"));
    }

    #[cfg(any(windows, unix))]
    #[tokio::test]
    async fn dropped_transport_exactly_retires_spawn_registered_connection() {
        let cleanup_registry = ConnectionCleanupRegistry::new();
        let init_params = InitializeParams {
            protocol_version: "2025-03-26".to_owned(),
            capabilities: ClientCapabilities {
                tools: Some(serde_json::json!({})),
            },
            client_info: ClientInfo {
                name: "nomi-test".to_owned(),
                version: "0".to_owned(),
            },
        };
        #[cfg(windows)]
        let (command, args) = (
            "cmd.exe",
            vec![
                "/D".to_owned(),
                "/S".to_owned(),
                "/C".to_owned(),
                "ping -n 120 127.0.0.1 >nul".to_owned(),
            ],
        );
        #[cfg(unix)]
        let (command, args) = (
            "/bin/sh",
            vec!["-c".to_owned(), "exec sleep 120".to_owned()],
        );

        let transport = StdioTransport::spawn_with_cleanup_registry(
            command,
            &args,
            &HashMap::new(),
            init_params,
            Arc::clone(&cleanup_registry),
        )
        .await
        .expect("spawn long-lived mock MCP server");

        assert_eq!(
            cleanup_registry
                .receipts
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .len(),
            1,
            "the cleanup obligation must be registered at spawn time"
        );

        drop(transport);
        tokio::time::timeout(Duration::from_secs(30), cleanup_registry.wait_all())
            .await
            .expect("drop custodian must not hang")
            .expect("drop custodian must prove exact cleanup");
    }

    // -----------------------------------------------------------------------
    // Respawn integration test: a mock stdio MCP server that crashes once.
    // Uses /bin/sh, so it is gated to unix.
    // -----------------------------------------------------------------------

    /// Write a mock MCP stdio server shell script that speaks line-delimited
    /// JSON-RPC. The script tracks how many times it has been *launched* via a
    /// shared counter file: launch #1 answers `initialize` + exactly one
    /// `tools/call`, then exits (EOF) to simulate a crash. Launch #2+ answers
    /// `initialize` and then every `tools/call` indefinitely.
    #[cfg(unix)]
    fn write_mock_server(dir: &std::path::Path) -> std::path::PathBuf {
        use std::io::Write;
        let launch_counter = dir.join("launches");
        let script_path = dir.join("mock_server.sh");
        // The script reads JSON-RPC lines from stdin and replies on stdout.
        // `initialize` → result; `tools/call` → a text content result; the
        // `notifications/initialized` notification gets no reply.
        let script = format!(
            r#"#!/bin/sh
COUNTER="{counter}"
# Record this launch (atomic-enough for a single-writer test).
n=$(cat "$COUNTER" 2>/dev/null || echo 0)
n=$((n + 1))
echo "$n" > "$COUNTER"
calls=0
while IFS= read -r line; do
  case "$line" in
    *'"method":"initialize"'*)
      printf '{{"jsonrpc":"2.0","id":1,"result":{{"protocolVersion":"2025-03-26","capabilities":{{}},"serverInfo":{{"name":"mock","version":"0"}}}}}}\n'
      ;;
    *'notifications/initialized'*)
      : # notification, no response
      ;;
    *'"method":"tools/call"'*)
      calls=$((calls + 1))
      # On the very first launch, crash right after answering one call.
      if [ "$n" -eq 1 ] && [ "$calls" -ge 1 ]; then
        printf '{{"jsonrpc":"2.0","id":0,"result":{{"content":[{{"type":"text","text":"before-crash"}}]}}}}\n'
        exit 0
      fi
      printf '{{"jsonrpc":"2.0","id":0,"result":{{"content":[{{"type":"text","text":"after-respawn"}}]}}}}\n'
      ;;
    *)
      : # ignore anything else
      ;;
  esac
done
"#,
            counter = launch_counter.display()
        );
        let mut f = std::fs::File::create(&script_path).unwrap();
        f.write_all(script.as_bytes()).unwrap();
        f.flush().unwrap();
        drop(f);
        // Make it executable.
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(&script_path).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&script_path, perms).unwrap();
        script_path
    }

    #[cfg(unix)]
    async fn handshake(transport: &StdioTransport) {
        // Drive the same handshake the manager would, so the first connection
        // is fully initialized before we exercise tools/call.
        let init = JsonRpcRequest::new(1, "initialize", Some(json!({})));
        transport.request(&init).await.expect("initialize");
        let initialized = JsonRpcRequest::notification("notifications/initialized", None);
        transport.notify(&initialized).await.expect("initialized");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn respawn_recovers_after_child_crash() {
        let tmp = std::env::temp_dir().join(format!("nomi_mcp_respawn_{}", std::process::id()));
        std::fs::create_dir_all(&tmp).unwrap();
        let script = write_mock_server(&tmp);

        let transport =
            StdioTransport::spawn("/bin/sh", &[script.to_string_lossy().into_owned()], &HashMap::new())
                .await
                .expect("spawn mock server");

        handshake(&transport).await;

        // First tools/call: the child answers "before-crash" then exits (EOF).
        // The next call detects the dead pipe, respawns, re-handshakes, retries.
        let call = JsonRpcRequest::new(2, "tools/call", Some(json!({"name": "t", "arguments": {}})));
        let r1 = transport.request(&call).await.expect("first call ok");
        assert_eq!(
            r1.result.unwrap()["content"][0]["text"],
            "before-crash",
            "first call should be served by the original child"
        );

        // The second call lands after the child has exited → triggers respawn.
        // It must succeed against the freshly respawned (stable) child.
        let r2 = transport
            .request(&call)
            .await
            .expect("second call must recover via respawn");
        assert_eq!(
            r2.result.unwrap()["content"][0]["text"],
            "after-respawn",
            "second call should be served by the respawned child"
        );

        // The respawn counter must show exactly one respawn (launch #2).
        let launches: u32 = std::fs::read_to_string(tmp.join("launches"))
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        assert!(
            launches >= 2,
            "child should have been launched at least twice (got {launches})"
        );

        let _ = transport.close().await;
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn respawn_budget_is_bounded() {
        // A server that exits immediately on every launch must not respawn
        // forever: after MAX_RESPAWNS the transport surfaces a hard error.
        use std::io::Write;
        let tmp = std::env::temp_dir().join(format!("nomi_mcp_crashloop_{}", std::process::id()));
        std::fs::create_dir_all(&tmp).unwrap();
        let script_path = tmp.join("always_crash.sh");
        // Answers initialize once, then exits the moment a tools/call arrives —
        // and the respawn's own re-handshake initialize also gets answered, but
        // the subsequent retried tools/call again hits EOF → respawn → ...
        let script = r#"#!/bin/sh
while IFS= read -r line; do
  case "$line" in
    *'"method":"initialize"'*)
      printf '{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":"2025-03-26","capabilities":{}}}\n'
      ;;
    *'"method":"tools/call"'*)
      exit 0
      ;;
    *) : ;;
  esac
done
"#;
        let mut f = std::fs::File::create(&script_path).unwrap();
        f.write_all(script.as_bytes()).unwrap();
        f.flush().unwrap();
        drop(f);
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(&script_path).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&script_path, perms).unwrap();

        let transport = StdioTransport::spawn(
            "/bin/sh",
            &[script_path.to_string_lossy().into_owned()],
            &HashMap::new(),
        )
        .await
        .expect("spawn crashloop server");
        handshake(&transport).await;

        let call = JsonRpcRequest::new(2, "tools/call", Some(json!({"name": "t", "arguments": {}})));
        // Each request EOFs and respawns once, then the retried call EOFs again
        // → that request returns Err. Repeated requests keep respawning, but the
        // per-window budget (MAX_RESPAWNS) must stop the bleeding: once exhausted,
        // respawn() itself errors instead of forking yet another doomed child.
        // Every attempt must therefore return Err (never hang, never panic).
        for i in 0..(MAX_RESPAWNS as usize + 3) {
            let result = transport.request(&call).await;
            assert!(
                result.is_err(),
                "attempt {i}: a server that crashes every call must surface an error, not hang"
            );
        }

        // After the budget is spent, respawn() must report the crashloop ceiling
        // rather than silently keep trying.
        let final_err = transport.request(&call).await.unwrap_err();
        let msg = final_err.to_string();
        assert!(
            msg.contains("exceeded") && msg.contains("respawns"),
            "expected a crashloop-ceiling error once the budget is spent, got: {msg}"
        );

        let _ = transport.close().await;
        let _ = std::fs::remove_dir_all(&tmp);
    }
}

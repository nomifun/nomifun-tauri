//! Terminal lifecycle channel: the structured "events OUT" half of the terminal
//! capability design. Native CLI hooks (claude --settings hooks / codex hooks)
//! invoke `nomicore terminal-hook`, which POSTs the event here; this server
//! broadcasts a normalized `TerminalLifecycleEvent` per terminal_id. Consumers
//! (Plan 3 AutoWork completion, Plan 4 IDMM supervision) subscribe — replacing
//! the byte-stream scraping that could never see real turn boundaries.

use std::sync::Arc;

use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::routing::post;
use axum::{Json, Router};
use dashmap::DashMap;
use nomifun_common::{TerminalId, generate_id};
use serde::{Deserialize, Serialize};
use tokio::net::TcpListener;
use tokio::sync::broadcast;
use tracing::{debug, warn};

const PAYLOAD_PTY_EPOCH: &str = "_nomi_pty_epoch";
const PAYLOAD_TURN_TOKEN: &str = "_nomi_turn_token";
const PAYLOAD_RAW_HOOK: &str = "_nomi_hook_payload";

/// Normalized lifecycle event kind (CLI-agnostic). Mapped from each CLI's hook
/// event by the `--event <kind>` arg baked into the hook command at injection.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LifecycleKind {
    /// The agent finished a turn (claude `Stop` / codex `Stop`).
    TurnEnd,
    /// A tool call completed (claude/codex `PostToolUse`) — activity signal.
    ToolUse,
    /// The agent is waiting / surfaced a notification (claude `Notification`).
    Notification,
    /// Session started (claude/codex `SessionStart`).
    SessionStart,
}

impl LifecycleKind {
    /// Parse the wire `kind` string used in the hook command's `--event` arg.
    pub fn from_wire(s: &str) -> Option<Self> {
        match s {
            "turn_end" => Some(Self::TurnEnd),
            "tool_use" => Some(Self::ToolUse),
            "notification" => Some(Self::Notification),
            "session_start" => Some(Self::SessionStart),
            _ => None,
        }
    }
}

/// One lifecycle event broadcast to subscribers of a terminal.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TerminalLifecycleEvent {
    pub terminal_id: TerminalId,
    pub kind: LifecycleKind,
    /// The CLI's raw hook payload (StopRequest/PostToolUse JSON), opaque to the
    /// channel; consumers extract what they need (e.g. last_assistant_message).
    pub payload: serde_json::Value,
}

impl TerminalLifecycleEvent {
    /// PTY spawn generation stamped by the trusted local lifecycle server.
    ///
    /// Legacy/unscoped events intentionally return `None` and can therefore
    /// never satisfy an exact-generation waiter.
    pub fn pty_epoch(&self) -> Option<u64> {
        self.payload.get(PAYLOAD_PTY_EPOCH)?.as_u64()
    }

    /// Per-turn token when a future CLI hook protocol can prove it. Current
    /// native CLI hooks cannot carry this value, so server-originated events
    /// return `None` and must be treated as activity rather than completion
    /// authority.
    pub fn turn_token(&self) -> Option<&str> {
        self.payload.get(PAYLOAD_TURN_TOKEN)?.as_str()
    }
}

/// Lifecycle receiver fenced to one exact live PTY generation.
///
/// The underlying channel may contain delayed events from a killed/relaunched
/// child. Those events, plus legacy events with no epoch, are discarded. A
/// lagged receiver is surfaced to the caller because silently skipping a gap
/// would make completion ambiguous.
pub struct ExactTerminalLifecycleReceiver {
    terminal_id: TerminalId,
    pty_epoch: u64,
    inner: broadcast::Receiver<TerminalLifecycleEvent>,
}

impl ExactTerminalLifecycleReceiver {
    fn new(
        terminal_id: TerminalId,
        pty_epoch: u64,
        inner: broadcast::Receiver<TerminalLifecycleEvent>,
    ) -> Self {
        Self {
            terminal_id,
            pty_epoch,
            inner,
        }
    }

    pub async fn recv(
        &mut self,
    ) -> Result<TerminalLifecycleEvent, broadcast::error::RecvError> {
        loop {
            let event = self.inner.recv().await?;
            if event.terminal_id == self.terminal_id
                && event.pty_epoch() == Some(self.pty_epoch)
            {
                return Ok(event);
            }
        }
    }
}

// ---------------------------------------------------------------------------
// TerminalLifecycleServer — house-pattern in-process HTTP server
// ---------------------------------------------------------------------------

#[derive(Clone)]
struct LifecycleState {
    auth_token: String,
    channels: Arc<DashMap<TerminalId, broadcast::Sender<TerminalLifecycleEvent>>>,
}

/// In-process HTTP server that receives lifecycle hook POSTs from the
/// `nomicore terminal-hook` shim and broadcasts them per terminal_id.
pub struct TerminalLifecycleServer {
    http_port: u16,
    auth_token: String,
    channels: Arc<DashMap<TerminalId, broadcast::Sender<TerminalLifecycleEvent>>>,
    _handle: tokio::task::JoinHandle<()>,
}

#[derive(Deserialize)]
struct HookPost {
    terminal_id: TerminalId,
    pty_epoch: u64,
    kind: LifecycleKind,
    #[serde(default)]
    payload: serde_json::Value,
}

impl TerminalLifecycleServer {
    /// Bind `127.0.0.1:0`, mint a random bearer token, and start serving
    /// `POST /hook`. Mirrors `KnowledgeMcpServer::start()`.
    pub async fn start() -> Result<Self, String> {
        let auth_token = generate_id();
        let channels: Arc<DashMap<TerminalId, broadcast::Sender<TerminalLifecycleEvent>>> =
            Arc::new(DashMap::new());
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .map_err(|e| format!("bind terminal lifecycle listener: {e}"))?;
        let http_port = listener.local_addr().map_err(|e| e.to_string())?.port();

        let state = LifecycleState {
            auth_token: auth_token.clone(),
            channels: channels.clone(),
        };

        let app = Router::new()
            .route("/hook", post(handle_hook))
            .with_state(state);

        let handle = tokio::spawn(async move {
            if let Err(e) = axum::serve(listener, app).await {
                warn!(error = %e, "Terminal lifecycle server exited with error");
            }
        });

        debug!(http_port, "Terminal lifecycle server started");

        Ok(Self {
            http_port,
            auth_token,
            channels,
            _handle: handle,
        })
    }

    pub fn http_port(&self) -> u16 {
        self.http_port
    }

    pub fn auth_token(&self) -> &str {
        &self.auth_token
    }

    /// Subscribe to a terminal's lifecycle events (lazily creates the channel).
    pub fn subscribe(&self, terminal_id: &TerminalId) -> broadcast::Receiver<TerminalLifecycleEvent> {
        self.channels
            .entry(terminal_id.clone())
            .or_insert_with(|| broadcast::channel(64).0)
            .subscribe()
    }

    /// Subscribe while rejecting every event not stamped with `pty_epoch`.
    pub fn subscribe_exact(
        &self,
        terminal_id: &TerminalId,
        pty_epoch: u64,
    ) -> ExactTerminalLifecycleReceiver {
        ExactTerminalLifecycleReceiver::new(
            terminal_id.clone(),
            pty_epoch,
            self.subscribe(terminal_id),
        )
    }
}

async fn handle_hook(
    State(state): State<LifecycleState>,
    headers: HeaderMap,
    Json(post): Json<HookPost>,
) -> StatusCode {
    let token = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.strip_prefix("Bearer "))
        .unwrap_or("");
    if token != state.auth_token {
        return StatusCode::UNAUTHORIZED;
    }
    // Hook stdin is untrusted CLI payload. Always overwrite the reserved
    // protocol metadata so a payload cannot spoof another PTY generation or a
    // per-turn completion token.
    let mut payload = match post.payload {
        serde_json::Value::Object(object) => object,
        raw => {
            let mut object = serde_json::Map::new();
            object.insert(PAYLOAD_RAW_HOOK.to_owned(), raw);
            object
        }
    };
    payload.insert(PAYLOAD_PTY_EPOCH.to_owned(), post.pty_epoch.into());
    payload.insert(PAYLOAD_TURN_TOKEN.to_owned(), serde_json::Value::Null);
    let ev = TerminalLifecycleEvent {
        terminal_id: post.terminal_id.clone(),
        kind: post.kind,
        payload: serde_json::Value::Object(payload),
    };
    if let Some(tx) = state.channels.get(&post.terminal_id) {
        let _ = tx.send(ev);
    }
    // No subscriber yet → drop silently (consumer attaches on demand). 200 either way.
    StatusCode::OK
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kind_parses_from_wire_event_string() {
        assert_eq!(
            LifecycleKind::from_wire("turn_end"),
            Some(LifecycleKind::TurnEnd)
        );
        assert_eq!(
            LifecycleKind::from_wire("tool_use"),
            Some(LifecycleKind::ToolUse)
        );
        assert_eq!(
            LifecycleKind::from_wire("notification"),
            Some(LifecycleKind::Notification)
        );
        assert_eq!(
            LifecycleKind::from_wire("session_start"),
            Some(LifecycleKind::SessionStart)
        );
        assert_eq!(LifecycleKind::from_wire("bogus"), None);
    }

    #[tokio::test]
    async fn post_hook_broadcasts_to_subscriber_and_rejects_bad_token() {
        let srv = TerminalLifecycleServer::start().await.expect("start");
        let terminal_id = nomifun_common::TerminalId::new();
        let mut rx = srv.subscribe(&terminal_id);
        let url = format!("http://127.0.0.1:{}/hook", srv.http_port());
        let body = serde_json::json!({"terminal_id":terminal_id,"pty_epoch":17,"kind":"turn_end","payload":{"last_assistant_message":"done"}});
        let client = reqwest::Client::builder().no_proxy().build().unwrap();
        // bad token → 401
        let bad = client
            .post(&url)
            .json(&body)
            .bearer_auth("wrong")
            .send()
            .await
            .unwrap();
        assert_eq!(bad.status(), 401);
        // good token → 200 + subscriber receives
        let ok = client
            .post(&url)
            .json(&body)
            .bearer_auth(srv.auth_token())
            .send()
            .await
            .unwrap();
        assert_eq!(ok.status(), 200);
        let ev = tokio::time::timeout(std::time::Duration::from_secs(2), rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(ev.terminal_id.to_string(), terminal_id.to_string());
        assert_eq!(ev.kind, LifecycleKind::TurnEnd);
        assert_eq!(ev.pty_epoch(), Some(17));
        assert_eq!(ev.turn_token(), None);
    }

    #[tokio::test]
    async fn post_hook_rejects_noncanonical_terminal_id() {
        let srv = TerminalLifecycleServer::start().await.expect("start");
        let url = format!("http://127.0.0.1:{}/hook", srv.http_port());
        let body = serde_json::json!({"terminal_id":"term_test","pty_epoch":1,"kind":"turn_end","payload":{}});
        let response = reqwest::Client::builder()
            .no_proxy()
            .build()
            .unwrap()
            .post(url)
            .json(&body)
            .bearer_auth(srv.auth_token())
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);
    }

    #[tokio::test]
    async fn post_hook_requires_epoch_and_reserved_metadata_cannot_be_spoofed() {
        let srv = TerminalLifecycleServer::start().await.expect("start");
        let terminal_id = TerminalId::new();
        let mut rx = srv.subscribe(&terminal_id);
        let url = format!("http://127.0.0.1:{}/hook", srv.http_port());
        let client = reqwest::Client::builder().no_proxy().build().unwrap();

        let missing = client
            .post(&url)
            .json(&serde_json::json!({
                "terminal_id": terminal_id,
                "kind": "turn_end",
                "payload": {}
            }))
            .bearer_auth(srv.auth_token())
            .send()
            .await
            .unwrap();
        assert_eq!(missing.status(), StatusCode::UNPROCESSABLE_ENTITY);

        let accepted = client
            .post(&url)
            .json(&serde_json::json!({
                "terminal_id": terminal_id,
                "pty_epoch": 23,
                "kind": "turn_end",
                "payload": {
                    "_nomi_pty_epoch": 999,
                    "_nomi_turn_token": "spoofed"
                }
            }))
            .bearer_auth(srv.auth_token())
            .send()
            .await
            .unwrap();
        assert_eq!(accepted.status(), StatusCode::OK);
        let event = rx.recv().await.unwrap();
        assert_eq!(event.pty_epoch(), Some(23));
        assert_eq!(event.turn_token(), None);
    }

    #[tokio::test]
    async fn exact_receiver_discards_old_and_unscoped_events() {
        let srv = TerminalLifecycleServer::start().await.expect("start");
        let terminal_id = TerminalId::new();
        let mut rx = srv.subscribe_exact(&terminal_id, 9);
        let tx = srv.channels.get(&terminal_id).unwrap().clone();
        tx.send(TerminalLifecycleEvent {
            terminal_id: terminal_id.clone(),
            kind: LifecycleKind::TurnEnd,
            payload: serde_json::json!({}),
        })
        .unwrap();
        tx.send(TerminalLifecycleEvent {
            terminal_id: terminal_id.clone(),
            kind: LifecycleKind::TurnEnd,
            payload: serde_json::json!({"_nomi_pty_epoch": 8}),
        })
        .unwrap();
        tx.send(TerminalLifecycleEvent {
            terminal_id,
            kind: LifecycleKind::TurnEnd,
            payload: serde_json::json!({"_nomi_pty_epoch": 9}),
        })
        .unwrap();

        let event = tokio::time::timeout(std::time::Duration::from_secs(1), rx.recv())
            .await
            .expect("exact epoch event")
            .unwrap();
        assert_eq!(event.pty_epoch(), Some(9));
    }
}

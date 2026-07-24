use std::sync::Arc;
use std::time::Duration;

use axum::extract::WebSocketUpgrade;
use axum::extract::ws::{CloseFrame, Message, WebSocket};
use axum::http::HeaderMap;
use axum::response::IntoResponse;
use futures_util::{SinkExt, StreamExt};
use nomifun_api_types::WebSocketMessage;
use serde_json::{Value, json};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info};

use crate::manager::{TokenAuthenticator, WebSocketManager};
use crate::router::MessageRouter;
use crate::types::{ConnectionId, PER_CONNECTION_BUFFER, WebSocketCloseCode, WsOutbound};

/// Extracts a JWT token from WebSocket upgrade request headers.
///
/// Injected by `nomifun-app` — wraps `nomifun_auth::extract_token_from_ws_headers`
/// so that `nomifun-realtime` does not depend on `nomifun-auth` directly.
pub type TokenExtractor = Arc<dyn Fn(&HeaderMap) -> Option<String> + Send + Sync>;

/// Shared state required by the WebSocket upgrade handler.
#[derive(Clone)]
pub struct WsHandlerState {
    pub manager: Arc<WebSocketManager>,
    pub router: Arc<dyn MessageRouter>,
    pub token_authenticator: TokenAuthenticator,
    pub token_extractor: TokenExtractor,
}

/// Axum handler for HTTP → WebSocket upgrade.
///
/// Extracts a JWT token from the request headers, validates it,
/// and upgrades the connection to WebSocket on success.
/// On authentication failure, sends `auth-expired` and closes with 1008.
///
/// When the token is carried via `Sec-WebSocket-Protocol`, the server
/// echoes the protocol header back so the client handshake succeeds.
pub async fn ws_upgrade_handler(
    ws: WebSocketUpgrade,
    headers: HeaderMap,
    axum::extract::State(state): axum::extract::State<WsHandlerState>,
) -> impl IntoResponse {
    let token = (state.token_extractor)(&headers);

    // Echo Sec-WebSocket-Protocol so clients using it for auth
    // receive a valid subprotocol negotiation response.
    let ws = if let Some(protocol) = headers
        .get("sec-websocket-protocol")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_owned())
    {
        ws.protocols([protocol])
    } else {
        ws
    };

    ws.on_upgrade(move |socket| async move {
        handle_socket(socket, token, state).await;
    })
}

/// Post-upgrade connection handler.
///
/// Validates the token, registers the client, spawns send/recv loops.
async fn handle_socket(socket: WebSocket, token: Option<String>, state: WsHandlerState) {
    let Some(token) = token else {
        send_close_no_token(socket).await;
        return;
    };

    let Some(user_id) = (state.token_authenticator)(&token) else {
        send_auth_expired_and_close(socket).await;
        return;
    };

    let (tx, rx) = mpsc::channel::<WsOutbound>(PER_CONNECTION_BUFFER);
    let conn_id = state.manager.add_client(user_id, token, tx);
    let cancellation = state
        .manager
        .connection_cancellation(conn_id)
        .expect("newly registered websocket has a cancellation signal");

    info!(%conn_id, "websocket connection established");

    let (ws_sender, ws_receiver) = socket.split();

    let mut send_handle = tokio::spawn(send_loop(
        conn_id,
        rx,
        ws_sender,
        cancellation.clone(),
    ));
    let mut send_loop_finished = false;
    let server_close = tokio::select! {
        _ = recv_loop(conn_id, ws_receiver, &state) => false,
        _ = cancellation.cancelled() => true,
        _ = &mut send_handle => {
            // A heartbeat policy close is queued before its manager entry is
            // removed. Once that queue drains, the send loop ending must also
            // terminate an otherwise-idle receive loop.
            send_loop_finished = true;
            false
        },
    };

    if server_close {
        // Let the send loop put a real close frame on the wire. A bounded wait
        // avoids a stalled socket sink retaining the task indefinitely.
        let _ = tokio::time::timeout(Duration::from_secs(1), &mut send_handle).await;
    }
    if !send_loop_finished {
        send_handle.abort();
    }
    state.manager.remove_client(conn_id);
    info!(%conn_id, "websocket connection closed");
}

/// Send a close frame with 1008 when no token is provided.
async fn send_close_no_token(mut socket: WebSocket) {
    let close = Message::Close(Some(CloseFrame {
        code: WebSocketCloseCode::PolicyViolation.as_u16(),
        reason: "no token provided".into(),
    }));
    let _ = socket.send(close).await;
}

/// Send `auth-expired` event then close with 1008.
async fn send_auth_expired_and_close(mut socket: WebSocket) {
    let auth_expired = WebSocketMessage::new("auth-expired", json!({"message": "Token expired or invalid"}));
    if let Ok(text) = serde_json::to_string(&auth_expired) {
        let _ = socket.send(Message::Text(text.into())).await;
    }
    let close = Message::Close(Some(CloseFrame {
        code: WebSocketCloseCode::PolicyViolation.as_u16(),
        reason: "authentication failed".into(),
    }));
    let _ = socket.send(close).await;
}

// -------------------------------------------------------------------
// Send loop
// -------------------------------------------------------------------

/// Reads `WsOutbound` from the per-connection channel and forwards
/// them to the WebSocket sink.
async fn send_loop(
    conn_id: ConnectionId,
    mut rx: mpsc::Receiver<WsOutbound>,
    mut sender: futures_util::stream::SplitSink<WebSocket, Message>,
    cancellation: CancellationToken,
) {
    loop {
        let outbound = tokio::select! {
            biased;
            _ = cancellation.cancelled() => {
                let close = Message::Close(Some(CloseFrame {
                    code: WebSocketCloseCode::NormalClosure.as_u16(),
                    reason: "server resync requested".into(),
                }));
                let _ = sender.send(close).await;
                break;
            }
            outbound = rx.recv() => match outbound {
                Some(outbound) => outbound,
                None => break,
            },
        };
        let msg = match outbound {
            WsOutbound::Text(text) => Message::Text(text.into()),
            WsOutbound::Close(code, reason) => Message::Close(Some(CloseFrame {
                code: code.as_u16(),
                reason: reason.into(),
            })),
        };
        if sender.send(msg).await.is_err() {
            debug!(%conn_id, "send loop: socket write failed, exiting");
            break;
        }
    }
}

// -------------------------------------------------------------------
// Receive loop
// -------------------------------------------------------------------

/// Reads messages from the WebSocket stream, parses JSON, routes.
async fn recv_loop(
    conn_id: ConnectionId,
    mut receiver: futures_util::stream::SplitStream<WebSocket>,
    state: &WsHandlerState,
) {
    while let Some(result) = receiver.next().await {
        let msg = match result {
            Ok(m) => m,
            Err(e) => {
                debug!(%conn_id, error = %e, "recv error, closing");
                break;
            }
        };

        match msg {
            Message::Text(text) => {
                handle_text_message(conn_id, &text, state);
            }
            Message::Close(_) => {
                debug!(%conn_id, "received close frame");
                break;
            }
            // Ping/Pong at the WebSocket protocol level are handled
            // automatically by axum/tungstenite. Binary frames are ignored.
            _ => {}
        }
    }
}

/// Process a text message: parse JSON, dispatch to built-in or router.
fn handle_text_message(conn_id: ConnectionId, text: &str, state: &WsHandlerState) {
    let parsed: Result<WebSocketMessage<Value>, _> = serde_json::from_str(text);

    let msg = match parsed {
        Ok(m) => m,
        Err(_) => {
            send_error_response(state, conn_id);
            return;
        }
    };

    match msg.name.as_str() {
        "pong" => {
            state.manager.update_last_ping(conn_id);
        }
        "subscribe-show-open" => {
            handle_subscribe_show_open(state, conn_id, msg.data);
        }
        name => {
            state.router.route(conn_id, name, msg.data);
        }
    }
}

/// Send an error response for invalid message format.
fn send_error_response(state: &WsHandlerState, conn_id: ConnectionId) {
    let error = json!({
        "error": "Invalid message format",
        "expected": r#"{ "name": "event-name", "data": {...} }"#
    });

    if let Ok(text) = serde_json::to_string(&error) {
        state.manager.send_raw_to(conn_id, WsOutbound::Text(text));
    }
}

/// Handle `subscribe-show-open`: reply with `show-open-request`.
///
/// The inbound `data` is the @office-ai/platform bridge envelope
/// `{ id, data: <user-params> }`. The renderer awaits a callback whose event
/// name embeds `id` (`subscribe.callback-show-open<id>`), so we must echo it
/// back; without it, the frontend's `useDirectorySelection` hook builds the
/// wrong callback name and the original `invoke()` Promise never resolves.
///
/// `isFileMode` is `true` when `properties` contains `openFile`
/// but NOT `openDirectory`.
fn handle_subscribe_show_open(state: &WsHandlerState, conn_id: ConnectionId, data: Value) {
    let id = data.get("id").and_then(|v| v.as_str()).unwrap_or("").to_owned();
    let inner = data.get("data").unwrap_or(&Value::Null);

    let properties = inner
        .get("properties")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();

    let has_open_file = properties.iter().any(|v| v.as_str() == Some("openFile"));
    let has_open_directory = properties.iter().any(|v| v.as_str() == Some("openDirectory"));

    let is_file_mode = has_open_file && !has_open_directory;

    let response = WebSocketMessage::new(
        "show-open-request",
        json!({
            "id": id,
            "properties": properties,
            "isFileMode": is_file_mode,
        }),
    );

    state.manager.send_to(conn_id, response);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_state(manager: Arc<WebSocketManager>) -> WsHandlerState {
        WsHandlerState {
            manager,
            router: Arc::new(crate::router::NoopMessageRouter),
            token_authenticator: Arc::new(|_| Some("user".to_owned())),
            token_extractor: Arc::new(|_| None),
        }
    }

    #[test]
    fn subscribe_show_open_file_mode() {
        let manager = Arc::new(WebSocketManager::new());
        let (tx, mut rx) = mpsc::channel(PER_CONNECTION_BUFFER);
        let conn_id = manager.add_client("user".into(), "tok".into(), tx);
        let state = test_state(manager);

        let data = json!({"id": "abc123", "data": {"properties": ["openFile"]}});
        handle_subscribe_show_open(&state, conn_id, data);

        let msg = rx.try_recv().unwrap();
        match msg {
            WsOutbound::Text(text) => {
                let parsed: Value = serde_json::from_str(&text).unwrap();
                assert_eq!(parsed["name"], "show-open-request");
                assert_eq!(parsed["data"]["id"], "abc123");
                assert_eq!(parsed["data"]["isFileMode"], true);
                assert_eq!(parsed["data"]["properties"], json!(["openFile"]));
            }
            _ => panic!("expected Text"),
        }
    }

    #[test]
    fn subscribe_show_open_directory_mode() {
        let manager = Arc::new(WebSocketManager::new());
        let (tx, mut rx) = mpsc::channel(PER_CONNECTION_BUFFER);
        let conn_id = manager.add_client("user".into(), "tok".into(), tx);
        let state = test_state(manager);

        let data = json!({"id": "dir1", "data": {"properties": ["openDirectory"]}});
        handle_subscribe_show_open(&state, conn_id, data);

        let msg = rx.try_recv().unwrap();
        match msg {
            WsOutbound::Text(text) => {
                let parsed: Value = serde_json::from_str(&text).unwrap();
                assert_eq!(parsed["data"]["id"], "dir1");
                assert_eq!(parsed["data"]["isFileMode"], false);
            }
            _ => panic!("expected Text"),
        }
    }

    #[test]
    fn subscribe_show_open_mixed_mode() {
        let manager = Arc::new(WebSocketManager::new());
        let (tx, mut rx) = mpsc::channel(PER_CONNECTION_BUFFER);
        let conn_id = manager.add_client("user".into(), "tok".into(), tx);
        let state = test_state(manager);

        let data = json!({"id": "mixed", "data": {"properties": ["openFile", "openDirectory"]}});
        handle_subscribe_show_open(&state, conn_id, data);

        let msg = rx.try_recv().unwrap();
        match msg {
            WsOutbound::Text(text) => {
                let parsed: Value = serde_json::from_str(&text).unwrap();
                assert_eq!(parsed["data"]["id"], "mixed");
                assert_eq!(parsed["data"]["isFileMode"], false);
            }
            _ => panic!("expected Text"),
        }
    }

    #[test]
    fn subscribe_show_open_empty_properties() {
        let manager = Arc::new(WebSocketManager::new());
        let (tx, mut rx) = mpsc::channel(PER_CONNECTION_BUFFER);
        let conn_id = manager.add_client("user".into(), "tok".into(), tx);
        let state = test_state(manager);

        let data = json!({"id": "empty", "data": {"properties": []}});
        handle_subscribe_show_open(&state, conn_id, data);

        let msg = rx.try_recv().unwrap();
        match msg {
            WsOutbound::Text(text) => {
                let parsed: Value = serde_json::from_str(&text).unwrap();
                assert_eq!(parsed["data"]["id"], "empty");
                assert_eq!(parsed["data"]["isFileMode"], false);
            }
            _ => panic!("expected Text"),
        }
    }

    #[test]
    fn subscribe_show_open_missing_properties() {
        let manager = Arc::new(WebSocketManager::new());
        let (tx, mut rx) = mpsc::channel(PER_CONNECTION_BUFFER);
        let conn_id = manager.add_client("user".into(), "tok".into(), tx);
        let state = test_state(manager);

        handle_subscribe_show_open(&state, conn_id, json!({"id": "noprops", "data": {}}));

        let msg = rx.try_recv().unwrap();
        match msg {
            WsOutbound::Text(text) => {
                let parsed: Value = serde_json::from_str(&text).unwrap();
                assert_eq!(parsed["data"]["id"], "noprops");
                assert_eq!(parsed["data"]["isFileMode"], false);
                assert_eq!(parsed["data"]["properties"], json!([]));
            }
            _ => panic!("expected Text"),
        }
    }

    #[test]
    fn subscribe_show_open_missing_id_falls_back_to_empty_string() {
        let manager = Arc::new(WebSocketManager::new());
        let (tx, mut rx) = mpsc::channel(PER_CONNECTION_BUFFER);
        let conn_id = manager.add_client("user".into(), "tok".into(), tx);
        let state = test_state(manager);

        handle_subscribe_show_open(&state, conn_id, json!({}));

        let msg = rx.try_recv().unwrap();
        match msg {
            WsOutbound::Text(text) => {
                let parsed: Value = serde_json::from_str(&text).unwrap();
                assert_eq!(parsed["data"]["id"], "");
                assert_eq!(parsed["data"]["isFileMode"], false);
                assert_eq!(parsed["data"]["properties"], json!([]));
            }
            _ => panic!("expected Text"),
        }
    }

    #[test]
    fn text_message_pong_updates_last_ping() {
        let manager = Arc::new(WebSocketManager::new());
        let (tx, _rx) = mpsc::channel(PER_CONNECTION_BUFFER);
        let conn_id = manager.add_client("user".into(), "tok".into(), tx);
        let state = test_state(manager);

        std::thread::sleep(std::time::Duration::from_millis(5));

        handle_text_message(conn_id, r#"{"name":"pong","data":{}}"#, &state);
        // No panic = success (update_last_ping was called)
    }

    #[test]
    fn text_message_invalid_json_sends_error() {
        let manager = Arc::new(WebSocketManager::new());
        let (tx, mut rx) = mpsc::channel(PER_CONNECTION_BUFFER);
        let conn_id = manager.add_client("user".into(), "tok".into(), tx);
        let state = test_state(manager);

        handle_text_message(conn_id, "not json", &state);

        let msg = rx.try_recv().unwrap();
        match msg {
            WsOutbound::Text(text) => {
                let parsed: Value = serde_json::from_str(&text).unwrap();
                assert_eq!(parsed["error"], "Invalid message format");
                assert!(parsed["expected"].is_string());
            }
            _ => panic!("expected error text"),
        }
    }

    #[test]
    fn text_message_missing_fields_sends_error() {
        let manager = Arc::new(WebSocketManager::new());
        let (tx, mut rx) = mpsc::channel(PER_CONNECTION_BUFFER);
        let conn_id = manager.add_client("user".into(), "tok".into(), tx);
        let state = test_state(manager);

        handle_text_message(conn_id, r#"{"foo":"bar"}"#, &state);

        let msg = rx.try_recv().unwrap();
        match msg {
            WsOutbound::Text(text) => {
                let parsed: Value = serde_json::from_str(&text).unwrap();
                assert_eq!(parsed["error"], "Invalid message format");
            }
            _ => panic!("expected error text"),
        }
    }

    #[test]
    fn text_message_routes_unknown_to_router() {
        use std::sync::atomic::{AtomicBool, Ordering};

        struct TestRouter {
            called: AtomicBool,
        }
        impl MessageRouter for TestRouter {
            fn route(&self, _conn_id: ConnectionId, _name: &str, _data: Value) {
                self.called.store(true, Ordering::Relaxed);
            }
        }

        let manager = Arc::new(WebSocketManager::new());
        let (tx, _rx) = mpsc::channel(PER_CONNECTION_BUFFER);
        let conn_id = manager.add_client("user".into(), "tok".into(), tx);

        let router = Arc::new(TestRouter {
            called: AtomicBool::new(false),
        });
        let state = WsHandlerState {
            manager,
            router: router.clone(),
            token_authenticator: Arc::new(|_| Some("user".to_owned())),
            token_extractor: Arc::new(|_| None),
        };

        handle_text_message(
            conn_id,
            r#"{"name":"conversation.send-message","data":{"text":"hi"}}"#,
            &state,
        );

        assert!(router.called.load(Ordering::Relaxed));
    }

    #[test]
    fn error_response_to_disconnected_client_is_noop() {
        let manager = Arc::new(WebSocketManager::new());
        let (tx, rx) = mpsc::channel(PER_CONNECTION_BUFFER);
        let conn_id = manager.add_client("user".into(), "tok".into(), tx);
        drop(rx); // close channel

        let state = test_state(manager.clone());

        // Should not panic — client will be removed
        send_error_response(&state, conn_id);
        assert_eq!(manager.client_count(), 0);
    }
}

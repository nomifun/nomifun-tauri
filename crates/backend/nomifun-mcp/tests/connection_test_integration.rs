//! Integration tests for McpConnectionTestService.
//!
//! Tests from test-plan §2 (Connection Test):
//! - CT-3: Command not found (ENOENT)
//! - CT-4: URL not reachable
//! - CT-5: Needs OAuth authentication (401)
//! - CT-6: Timeout
//! - SSE auth probe (M-33 coverage)

use std::collections::HashMap;
use std::time::Duration;

use nomifun_mcp::McpConnectionTestService;
use nomifun_mcp::McpServerTransport;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::mpsc;

fn make_service() -> McpConnectionTestService {
    McpConnectionTestService::new(test_http_client())
}

fn make_service_with_timeout(timeout: Duration) -> McpConnectionTestService {
    McpConnectionTestService::new(test_http_client()).with_timeout(timeout)
}

fn test_http_client() -> reqwest::Client {
    reqwest::Client::builder()
        .no_proxy()
        .build()
        .expect("test http client")
}

// ---------------------------------------------------------------------------
// CT-3: Command not found (ENOENT)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn stdio_nonexistent_command_returns_not_found_error() {
    let svc = make_service();
    let transport = McpServerTransport::Stdio {
        command: "nonexistent-mcp-cmd-xyz-12345".into(),
        args: vec![],
        env: HashMap::new(),
    };

    let result = svc.test_connection("test-server", &transport).await;

    assert!(!result.success);
    let error = result.error.as_deref().unwrap();
    assert!(
        error.contains("Command not found"),
        "expected 'Command not found' in: {error}"
    );
    assert!(result.tools.is_none());
    assert!(result.needs_auth.is_none());
}

// ---------------------------------------------------------------------------
// CT-4: URL not reachable
// ---------------------------------------------------------------------------

#[tokio::test]
async fn http_unreachable_url_returns_connection_error() {
    let svc = make_service_with_timeout(Duration::from_secs(5));
    let transport = McpServerTransport::Http {
        url: "http://127.0.0.1:1/mcp-unreachable".into(),
        headers: HashMap::new(),
    };

    let result = svc.test_connection("test-http", &transport).await;

    assert!(!result.success);
    let error = result.error.as_deref().unwrap();
    assert!(
        error.contains("Connection failed"),
        "expected connection failure in: {error}"
    );
}

#[tokio::test]
async fn sse_unreachable_url_returns_connection_error() {
    let svc = make_service_with_timeout(Duration::from_secs(5));
    let transport = McpServerTransport::Sse {
        url: "http://127.0.0.1:1/sse-unreachable".into(),
        headers: HashMap::new(),
    };

    let result = svc.test_connection("test-sse", &transport).await;

    assert!(!result.success);
    let error = result.error.as_deref().unwrap();
    assert!(
        error.contains("Connection failed"),
        "expected connection failure in: {error}"
    );
}

// ---------------------------------------------------------------------------
// CT-5: HTTP 401 Unauthorized -> needsAuth
// ---------------------------------------------------------------------------

#[tokio::test]
async fn http_401_returns_needs_auth() {
    // Spin up a mock server that returns 401 with WWW-Authenticate
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    let server_handle = tokio::spawn(async move {
        let app = axum::Router::new().route(
            "/mcp",
            axum::routing::post(|| async {
                (
                    axum::http::StatusCode::UNAUTHORIZED,
                    [(axum::http::header::WWW_AUTHENTICATE, "Bearer realm=\"mcp-server\"")],
                    "",
                )
            }),
        );
        axum::serve(listener, app).await.unwrap();
    });

    let svc = make_service();
    let transport = McpServerTransport::Http {
        url: format!("http://{}/mcp", addr),
        headers: HashMap::new(),
    };

    let result = svc.test_connection("auth-server", &transport).await;

    assert!(!result.success);
    assert_eq!(result.needs_auth, Some(true));
    assert!(result.auth_method.is_some());
    assert!(result.www_authenticate.is_some());
    assert!(result.error.is_none());

    server_handle.abort();
}

#[tokio::test]
async fn sse_401_returns_needs_auth() {
    // Spin up a mock server that returns 401 for GET
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    let server_handle = tokio::spawn(async move {
        let app = axum::Router::new().route(
            "/sse",
            axum::routing::get(|| async {
                (
                    axum::http::StatusCode::UNAUTHORIZED,
                    [(axum::http::header::WWW_AUTHENTICATE, "Bearer realm=\"mcp-sse\"")],
                    "",
                )
            }),
        );
        axum::serve(listener, app).await.unwrap();
    });

    let svc = make_service();
    let transport = McpServerTransport::Sse {
        url: format!("http://{}/sse", addr),
        headers: HashMap::new(),
    };

    let result = svc.test_connection("sse-auth", &transport).await;

    assert!(!result.success);
    assert_eq!(result.needs_auth, Some(true));
    assert!(result.www_authenticate.is_some());

    server_handle.abort();
}

#[tokio::test]
async fn sse_connection_test_uses_string_jsonrpc_ids() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (event_tx, event_rx) = mpsc::unbounded_channel::<String>();

    let server_handle = tokio::spawn(async move {
        let mut event_rx = Some(event_rx);
        while let Ok((stream, _)) = listener.accept().await {
            let event_tx = event_tx.clone();
            let rx = event_rx.take();
            tokio::spawn(async move {
                let _ = handle_string_id_sse_connection(stream, event_tx, rx).await;
            });
        }
    });

    let svc = make_service_with_timeout(Duration::from_secs(5));
    let transport = McpServerTransport::Sse {
        url: format!("http://{}/sse", addr),
        headers: HashMap::new(),
    };

    let result = svc.test_connection("string-id-sse", &transport).await;

    assert!(result.success, "expected string-id SSE server to connect: {result:?}");
    let tools = result.tools.unwrap();
    assert_eq!(tools.len(), 1);
    assert_eq!(tools[0].name, "strict_string_id_tool");

    server_handle.abort();
}

async fn handle_string_id_sse_connection(
    mut stream: tokio::net::TcpStream,
    event_tx: mpsc::UnboundedSender<String>,
    event_rx: Option<mpsc::UnboundedReceiver<String>>,
) -> std::io::Result<()> {
    let (request, body) = read_http_request(&mut stream).await?;
    if request.starts_with("GET /sse ") {
        let mut event_rx = event_rx.expect("SSE GET should be the first connection");
        stream
            .write_all(
                b"HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncache-control: no-cache\r\n\r\n",
            )
            .await?;
        stream
            .write_all(b"event: endpoint\ndata: /messages\n\n")
            .await?;
        stream.flush().await?;
        while let Some(message) = event_rx.recv().await {
            stream
                .write_all(format!("event: message\ndata: {message}\n\n").as_bytes())
                .await?;
            stream.flush().await?;
        }
        return Ok(());
    }

    if request.starts_with("POST /messages ") {
        let body: serde_json::Value = serde_json::from_slice(&body).unwrap();
        let method = body["method"].as_str().unwrap_or_default();
        match method {
            "initialize" | "tools/list" => {
                let Some(id) = body["id"].as_str() else {
                    write_http_response(
                        &mut stream,
                        "400 Bad Request",
                        "Bad request: id expected a string",
                    )
                    .await?;
                    return Ok(());
                };
                let response = match method {
                    "initialize" => serde_json::json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "result": {
                            "protocolVersion": "2024-11-05",
                            "capabilities": {},
                            "serverInfo": { "name": "strict-string-id", "version": "1.0.0" }
                        }
                    }),
                    _ => serde_json::json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "result": {
                            "tools": [
                                { "name": "strict_string_id_tool", "description": "Requires string JSON-RPC ids" }
                            ]
                        }
                    }),
                };
                event_tx.send(response.to_string()).unwrap();
                write_http_response(&mut stream, "202 Accepted", "").await?;
            }
            "notifications/initialized" => {
                write_http_response(&mut stream, "202 Accepted", "").await?;
            }
            _ => {
                write_http_response(&mut stream, "400 Bad Request", "unknown method").await?;
            }
        }
        return Ok(());
    }

    write_http_response(&mut stream, "404 Not Found", "").await
}

async fn read_http_request(stream: &mut tokio::net::TcpStream) -> std::io::Result<(String, Vec<u8>)> {
    let mut buffer = Vec::new();
    let mut chunk = [0_u8; 1024];
    let header_end = loop {
        let n = stream.read(&mut chunk).await?;
        if n == 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "connection closed before headers",
            ));
        }
        buffer.extend_from_slice(&chunk[..n]);
        if let Some(pos) = find_header_end(&buffer) {
            break pos;
        }
    };

    let header = String::from_utf8_lossy(&buffer[..header_end]).to_string();
    let content_length = header
        .lines()
        .find_map(|line| line.strip_prefix("content-length:").or_else(|| line.strip_prefix("Content-Length:")))
        .and_then(|value| value.trim().parse::<usize>().ok())
        .unwrap_or(0);

    let body_start = header_end + 4;
    let mut body = buffer[body_start..].to_vec();
    while body.len() < content_length {
        let n = stream.read(&mut chunk).await?;
        if n == 0 {
            break;
        }
        body.extend_from_slice(&chunk[..n]);
    }
    body.truncate(content_length);

    Ok((header, body))
}

fn find_header_end(buffer: &[u8]) -> Option<usize> {
    buffer.windows(4).position(|window| window == b"\r\n\r\n")
}

async fn write_http_response(
    stream: &mut tokio::net::TcpStream,
    status: &str,
    body: &str,
) -> std::io::Result<()> {
    let response = format!(
        "HTTP/1.1 {status}\r\ncontent-length: {}\r\n\r\n{body}",
        body.len()
    );
    stream.write_all(response.as_bytes()).await?;
    stream.flush().await
}

// ---------------------------------------------------------------------------
// CT-6: Timeout
// ---------------------------------------------------------------------------

#[tokio::test]
async fn stdio_timeout_returns_timeout_error() {
    // Use `sleep` which produces no stdout — our protocol read will block
    let svc = make_service_with_timeout(Duration::from_secs(1));
    let transport = McpServerTransport::Stdio {
        command: "sleep".into(),
        args: vec!["60".into()],
        env: HashMap::new(),
    };

    let result = svc.test_connection("timeout-server", &transport).await;

    assert!(!result.success);
    let error = result.error.as_deref().unwrap();
    assert!(error.contains("timed out"), "expected timeout in: {error}");
}

// ---------------------------------------------------------------------------
// HTTP non-success status
// ---------------------------------------------------------------------------

#[tokio::test]
async fn http_500_returns_error_with_status() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    let server_handle = tokio::spawn(async move {
        let app = axum::Router::new().route(
            "/mcp",
            axum::routing::post(|| async { axum::http::StatusCode::INTERNAL_SERVER_ERROR }),
        );
        axum::serve(listener, app).await.unwrap();
    });

    let svc = make_service();
    let transport = McpServerTransport::Http {
        url: format!("http://{}/mcp", addr),
        headers: HashMap::new(),
    };

    let result = svc.test_connection("error-server", &transport).await;

    assert!(!result.success);
    let error = result.error.as_deref().unwrap();
    assert!(error.contains("500"), "expected HTTP 500 in: {error}");

    server_handle.abort();
}

// ---------------------------------------------------------------------------
// HTTP transport with custom headers
// ---------------------------------------------------------------------------

#[tokio::test]
async fn http_custom_headers_are_sent() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    let server_handle = tokio::spawn(async move {
        let app = axum::Router::new().route(
            "/mcp",
            axum::routing::post(|headers: axum::http::HeaderMap| async move {
                // Verify the custom header was received
                if headers.get("x-api-key").and_then(|v| v.to_str().ok()) == Some("secret") {
                    // Return a valid initialize response
                    axum::Json(serde_json::json!({
                        "jsonrpc": "2.0",
                        "id": 1,
                        "result": {
                            "protocolVersion": "2024-11-05",
                            "capabilities": {},
                            "serverInfo": { "name": "test", "version": "1.0" }
                        }
                    }))
                } else {
                    // Return error if header missing
                    axum::Json(serde_json::json!({
                        "jsonrpc": "2.0",
                        "id": 1,
                        "error": { "code": -1, "message": "Missing API key" }
                    }))
                }
            }),
        );
        axum::serve(listener, app).await.unwrap();
    });

    let svc = make_service();
    let mut headers = HashMap::new();
    headers.insert("X-Api-Key".into(), "secret".into());
    let transport = McpServerTransport::Http {
        url: format!("http://{}/mcp", addr),
        headers,
    };

    let result = svc.test_connection("header-server", &transport).await;

    // The server returns a valid initialize response for request id=1,
    // but the subsequent tools/list (id=2) will also hit the same handler.
    // Either way, the first request should succeed (no initialize error).
    // The tools/list might succeed or fail depending on how the mock handles id=2.
    // For this test, we just verify the custom header was sent (no "Missing API key" error).
    if let Some(ref error) = result.error {
        assert!(
            !error.contains("Missing API key"),
            "Custom header should have been sent"
        );
    }

    server_handle.abort();
}

// ---------------------------------------------------------------------------
// Stdio with args and env
// ---------------------------------------------------------------------------

#[tokio::test]
async fn stdio_with_args_spawns_correctly() {
    // Use echo as a simple command that exits immediately
    // Since echo doesn't speak MCP, we expect a protocol error (not a spawn error)
    let svc = make_service_with_timeout(Duration::from_secs(3));
    let transport = McpServerTransport::Stdio {
        command: "echo".into(),
        args: vec!["hello".into()],
        env: HashMap::new(),
    };

    let result = svc.test_connection("echo-server", &transport).await;

    // echo outputs "hello\n" then exits — not valid JSON-RPC
    assert!(!result.success);
    let error = result.error.as_deref().unwrap();
    // Should be a protocol error, not a spawn error
    assert!(!error.contains("Command not found"), "echo should be found");
}

// Protocol types, helpers, and message builders for MCP connection testing.
//
// This module implements the minimal JSON-RPC 2.0 subset needed for the
// MCP handshake: initialize → initialized → tools/list.

use std::collections::HashMap;

use nomifun_api_types::{McpAuthMethod, McpConnectionTestErrorCode, McpConnectionTestResult, McpToolResponse};
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::mpsc;

use std::time::Duration;

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

const PROTOCOL_VERSION: &str = "2024-11-05";
const CLIENT_NAME: &str = "nomifun-mcp-test";
const CLIENT_VERSION: &str = "1.0.0";

// ---------------------------------------------------------------------------
// JSON-RPC message types
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize)]
pub(super) struct JsonRpcRequest {
    pub jsonrpc: &'static str,
    pub id: String,
    pub method: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub params: Option<serde_json::Value>,
}

#[derive(Debug, Serialize)]
pub(super) struct JsonRpcNotification {
    pub jsonrpc: &'static str,
    pub method: String,
}

#[derive(Debug, Deserialize)]
pub(super) struct JsonRpcResponse {
    #[allow(dead_code)]
    pub jsonrpc: String,
    pub id: Option<JsonRpcId>,
    pub result: Option<serde_json::Value>,
    pub error: Option<JsonRpcError>,
}

#[allow(dead_code)]
#[derive(Debug, Deserialize)]
#[serde(untagged)]
pub(super) enum JsonRpcId {
    String(String),
    Number(i64),
}

#[derive(Debug, Deserialize)]
pub(super) struct JsonRpcError {
    pub code: i64,
    pub message: String,
}

#[derive(Debug, Deserialize)]
struct ToolsListResult {
    tools: Vec<McpToolInfo>,
}

#[derive(Debug, Deserialize)]
struct McpToolInfo {
    name: String,
    description: Option<String>,
    #[serde(rename = "inputSchema")]
    input_schema: Option<serde_json::Value>,
}

/// A single SSE event parsed from the stream.
pub(super) struct SseEvent {
    pub event_type: String,
    pub data: String,
}

// ---------------------------------------------------------------------------
// Stdio protocol helpers
// ---------------------------------------------------------------------------

/// Run the MCP protocol handshake over stdio (newline-delimited JSON-RPC).
pub(super) async fn run_stdio_protocol(
    mut stdin: tokio::process::ChildStdin,
    stdout: tokio::process::ChildStdout,
) -> McpConnectionTestResult {
    let mut reader = BufReader::new(stdout);

    // 1. initialize
    if let Err(e) = write_jsonrpc_line(&mut stdin, &build_initialize_request(1)).await {
        return error_result(
            McpConnectionTestErrorCode::ProtocolError,
            format!("Failed to send initialize: {e}"),
            Some(serde_json::json!({ "transport": "stdio", "stage": "initialize_send" })),
        );
    }
    let init_resp = match read_jsonrpc_response(&mut reader).await {
        Ok(r) => r,
        Err(e) => {
            return error_result(
                McpConnectionTestErrorCode::ProtocolError,
                format!("initialize response: {e}"),
                Some(serde_json::json!({ "transport": "stdio", "stage": "initialize_response" })),
            );
        }
    };
    if let Some(err) = init_resp.error {
        return rpc_error_result("initialize", &err);
    }

    // 2. initialized notification
    if let Err(e) = write_jsonrpc_line(&mut stdin, &build_initialized_notification()).await {
        return error_result(
            McpConnectionTestErrorCode::ProtocolError,
            format!("Failed to send initialized: {e}"),
            Some(serde_json::json!({ "transport": "stdio", "stage": "initialized_send" })),
        );
    }

    // 3. tools/list
    if let Err(e) = write_jsonrpc_line(&mut stdin, &build_tools_list_request(2)).await {
        return error_result(
            McpConnectionTestErrorCode::ProtocolError,
            format!("Failed to send tools/list: {e}"),
            Some(serde_json::json!({ "transport": "stdio", "stage": "tools_list_send" })),
        );
    }
    let tools_resp = match read_jsonrpc_response(&mut reader).await {
        Ok(r) => r,
        Err(e) => {
            return error_result(
                McpConnectionTestErrorCode::ProtocolError,
                format!("tools/list response: {e}"),
                Some(serde_json::json!({ "transport": "stdio", "stage": "tools_list_response" })),
            );
        }
    };
    if let Some(err) = tools_resp.error {
        return rpc_error_result("tools/list", &err);
    }

    success_result(tools_resp.result)
}

/// Write a JSON-RPC message as a newline-delimited line to stdin.
async fn write_jsonrpc_line<T: Serialize>(stdin: &mut tokio::process::ChildStdin, msg: &T) -> std::io::Result<()> {
    let json = serde_json::to_string(msg).map_err(std::io::Error::other)?;
    stdin.write_all(json.as_bytes()).await?;
    stdin.write_all(b"\n").await?;
    stdin.flush().await
}

/// Read the next JSON-RPC response from stdout.
///
/// Skips server notifications (messages without an `id` field) and
/// non-JSON lines (e.g. logging output).
async fn read_jsonrpc_response(reader: &mut BufReader<tokio::process::ChildStdout>) -> Result<JsonRpcResponse, String> {
    let mut line = String::new();
    loop {
        line.clear();
        let n = reader
            .read_line(&mut line)
            .await
            .map_err(|e| format!("I/O error: {e}"))?;
        if n == 0 {
            return Err("Server closed stdout before responding".into());
        }
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        if let Ok(resp) = serde_json::from_str::<JsonRpcResponse>(trimmed)
            && resp.id.is_some()
        {
            return Ok(resp);
        }
    }
}

// ---------------------------------------------------------------------------
// SSE helpers
// ---------------------------------------------------------------------------

/// Read SSE events from a streaming HTTP response and forward via channel.
pub(super) async fn read_sse_events(mut resp: reqwest::Response, tx: mpsc::Sender<SseEvent>) {
    let mut buffer = String::new();
    loop {
        match resp.chunk().await {
            Ok(Some(chunk)) => {
                // Normalize line endings for consistent parsing
                let text = String::from_utf8_lossy(&chunk);
                buffer.push_str(&text.replace("\r\n", "\n"));
                while let Some(event) = parse_next_sse_event(&mut buffer) {
                    if tx.send(event).await.is_err() {
                        return;
                    }
                }
            }
            Ok(None) | Err(_) => return,
        }
    }
}

/// Parse the next complete SSE event from the buffer.
///
/// An SSE event is terminated by a blank line (`\n\n`).
/// Returns `None` if no complete event is available yet.
fn parse_next_sse_event(buffer: &mut String) -> Option<SseEvent> {
    let end = buffer.find("\n\n")?;
    let event_text: String = buffer.drain(..end + 2).collect();

    let mut event_type = String::new();
    let mut data_parts: Vec<&str> = Vec::new();

    for line in event_text.lines() {
        if let Some(rest) = line.strip_prefix("event:") {
            event_type = rest.trim().to_owned();
        } else if let Some(rest) = line.strip_prefix("data:") {
            // SSE spec: strip one leading space if present
            data_parts.push(rest.strip_prefix(' ').unwrap_or(rest));
        }
    }

    Some(SseEvent {
        event_type,
        data: data_parts.join("\n"),
    })
}

/// Wait for an `endpoint` event from the SSE stream and resolve the URL.
pub(super) async fn wait_for_endpoint(
    event_rx: &mut mpsc::Receiver<SseEvent>,
    base_url: &str,
) -> Result<String, String> {
    loop {
        match event_rx.recv().await {
            Some(event) if event.event_type == "endpoint" => {
                return resolve_endpoint_url(base_url, &event.data);
            }
            Some(_) => continue,
            None => return Err("SSE stream closed before endpoint event".into()),
        }
    }
}

/// Wait for the next JSON-RPC response from the SSE stream.
pub(super) async fn wait_for_jsonrpc_response(
    event_rx: &mut mpsc::Receiver<SseEvent>,
) -> Result<JsonRpcResponse, String> {
    loop {
        match event_rx.recv().await {
            Some(event) if event.event_type == "message" => {
                let resp: JsonRpcResponse =
                    serde_json::from_str(&event.data).map_err(|e| format!("Invalid JSON-RPC in SSE: {e}"))?;
                if resp.id.is_some() {
                    return Ok(resp);
                }
            }
            Some(_) => continue,
            None => return Err("SSE stream closed before response".into()),
        }
    }
}

/// Resolve a potentially relative endpoint URL against a base URL.
fn resolve_endpoint_url(base_url: &str, endpoint: &str) -> Result<String, String> {
    let base = reqwest::Url::parse(base_url).map_err(|e| format!("Invalid base URL: {e}"))?;
    base.join(endpoint)
        .map(|u| u.to_string())
        .map_err(|e| format!("Invalid endpoint URL: {e}"))
}

// ---------------------------------------------------------------------------
// HTTP helpers
// ---------------------------------------------------------------------------

/// Build an HTTP header map from a string-to-string map.
pub(super) fn build_http_headers(headers: &HashMap<String, String>) -> reqwest::header::HeaderMap {
    let mut map = reqwest::header::HeaderMap::new();
    for (k, v) in headers {
        if let (Ok(name), Ok(val)) = (
            reqwest::header::HeaderName::from_bytes(k.as_bytes()),
            reqwest::header::HeaderValue::from_str(v),
        ) {
            map.insert(name, val);
        }
    }
    map
}

/// Parse a JSON-RPC response from an HTTP response body.
///
/// Handles both `application/json` and `text/event-stream` content types
/// (Streamable HTTP servers may respond with either).
pub(super) async fn parse_http_response(resp: reqwest::Response) -> Result<JsonRpcResponse, String> {
    let is_sse = resp
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|ct| ct.contains("text/event-stream"));

    let body = resp.text().await.map_err(|e| format!("Failed to read response: {e}"))?;

    if is_sse {
        extract_jsonrpc_from_sse(&body)
    } else {
        serde_json::from_str(&body).map_err(|e| format!("Invalid JSON-RPC response: {e}"))
    }
}

/// Extract the first JSON-RPC response from SSE event data.
fn extract_jsonrpc_from_sse(body: &str) -> Result<JsonRpcResponse, String> {
    for line in body.lines() {
        if let Some(data) = line.strip_prefix("data:") {
            let data = data.strip_prefix(' ').unwrap_or(data);
            if let Ok(resp) = serde_json::from_str::<JsonRpcResponse>(data)
                && resp.id.is_some()
            {
                return Ok(resp);
            }
        }
    }
    Err("No JSON-RPC response found in SSE data".into())
}

// ---------------------------------------------------------------------------
// JSON-RPC message builders
// ---------------------------------------------------------------------------

pub(super) fn build_initialize_request(id: u64) -> JsonRpcRequest {
    JsonRpcRequest {
        jsonrpc: "2.0",
        id: id.to_string(),
        method: "initialize".into(),
        params: Some(serde_json::json!({
            "protocolVersion": PROTOCOL_VERSION,
            "capabilities": {},
            "clientInfo": {
                "name": CLIENT_NAME,
                "version": CLIENT_VERSION
            }
        })),
    }
}

pub(super) fn build_initialized_notification() -> JsonRpcNotification {
    JsonRpcNotification {
        jsonrpc: "2.0",
        method: "notifications/initialized".into(),
    }
}

pub(super) fn build_tools_list_request(id: u64) -> JsonRpcRequest {
    JsonRpcRequest {
        jsonrpc: "2.0",
        id: id.to_string(),
        method: "tools/list".into(),
        params: None,
    }
}

// ---------------------------------------------------------------------------
// Result builders
// ---------------------------------------------------------------------------

pub(super) fn success_result(tools_value: Option<serde_json::Value>) -> McpConnectionTestResult {
    let tools = tools_value
        .and_then(|v| serde_json::from_value::<ToolsListResult>(v).ok())
        .map(|r| {
            r.tools
                .into_iter()
                .map(|t| McpToolResponse {
                    name: t.name,
                    description: t.description,
                    input_schema: t.input_schema,
                })
                .collect()
        })
        .unwrap_or_default();

    McpConnectionTestResult {
        success: true,
        tools: Some(tools),
        error: None,
        code: None,
        details: None,
        needs_auth: None,
        auth_method: None,
        www_authenticate: None,
    }
}

pub(super) fn error_result(
    code: McpConnectionTestErrorCode,
    msg: String,
    details: Option<serde_json::Value>,
) -> McpConnectionTestResult {
    McpConnectionTestResult {
        success: false,
        tools: None,
        error: Some(msg),
        code: Some(code),
        details,
        needs_auth: None,
        auth_method: None,
        www_authenticate: None,
    }
}

pub(super) fn timeout_result(duration: Duration) -> McpConnectionTestResult {
    error_result(
        McpConnectionTestErrorCode::Timeout,
        format!("Connection test timed out after {}s", duration.as_secs()),
        Some(serde_json::json!({ "timeout_seconds": duration.as_secs() })),
    )
}

pub(super) fn spawn_error_result(command: &str, error: &std::io::Error) -> McpConnectionTestResult {
    match error.kind() {
        std::io::ErrorKind::NotFound => {
            let runtime = missing_command_runtime(command);
            error_result(
                McpConnectionTestErrorCode::CommandNotFound,
                command_not_found_message(command),
                Some(serde_json::json!({
                    "command": command,
                    "runtime": runtime,
                })),
            )
        }
        std::io::ErrorKind::PermissionDenied => error_result(
            McpConnectionTestErrorCode::CommandPermissionDenied,
            format!("Permission denied: {command}"),
            Some(serde_json::json!({ "command": command })),
        ),
        _ => error_result(
            McpConnectionTestErrorCode::CommandStartFailed,
            format!("Failed to start '{command}': {error}"),
            Some(serde_json::json!({
                "command": command,
                "io_error": error.to_string(),
            })),
        ),
    }
}

fn command_basename(command: &str) -> String {
    let mut command_name = command
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or(command)
        .to_ascii_lowercase();
    for suffix in [".exe", ".cmd", ".bat"] {
        if let Some(stripped) = command_name.strip_suffix(suffix) {
            command_name = stripped.to_owned();
            break;
        }
    }
    command_name
}

fn missing_command_runtime(command: &str) -> &'static str {
    let command_name = command_basename(command);
    match command_name.as_str() {
        "npx" | "npm" | "node" | "pnpx" => "node",
        "bun" | "bunx" => "bun",
        "uv" | "uvx" => "uv",
        "python" | "python3" => "python",
        "deno" => "deno",
        _ => "generic",
    }
}

fn command_not_found_message(command: &str) -> String {
    match missing_command_runtime(command) {
        "node" => format!(
            "Command not found: {command}. Install Node.js (which includes npm/npx), then restart Nomi or configure this MCP server to use an absolute command path."
        ),
        "bun" => format!(
            "Command not found: {command}. Install Bun (which includes bun/bunx), then restart Nomi or configure this MCP server to use an absolute command path."
        ),
        "uv" => format!(
            "Command not found: {command}. Install uv, then restart Nomi or configure this MCP server to use an absolute command path."
        ),
        "python" => format!(
            "Command not found: {command}. Install Python, then restart Nomi or configure this MCP server to use an absolute command path."
        ),
        "deno" => format!(
            "Command not found: {command}. Install Deno, then restart Nomi or configure this MCP server to use an absolute command path."
        ),
        _ => format!(
            "Command not found: {command}. Install the command or configure this MCP server to use an absolute command path."
        ),
    }
}

pub(super) fn rpc_error_result(method: &str, err: &JsonRpcError) -> McpConnectionTestResult {
    error_result(
        McpConnectionTestErrorCode::RpcError,
        format!("{method} error: {} (code {})", err.message, err.code),
        Some(serde_json::json!({
            "method": method,
            "rpc_code": err.code,
        })),
    )
}

pub(super) fn auth_result(headers: &reqwest::header::HeaderMap) -> McpConnectionTestResult {
    let www_authenticate = headers
        .get("www-authenticate")
        .and_then(|v| v.to_str().ok())
        .map(String::from);

    let auth_method = www_authenticate.as_deref().map(detect_auth_method);

    McpConnectionTestResult {
        success: false,
        tools: None,
        error: None,
        code: None,
        details: None,
        needs_auth: Some(true),
        auth_method,
        www_authenticate,
    }
}

fn detect_auth_method(www_authenticate: &str) -> McpAuthMethod {
    let lower = www_authenticate.to_lowercase();
    if lower.contains("bearer") || lower.contains("oauth") {
        McpAuthMethod::Oauth
    } else {
        McpAuthMethod::Basic
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // -- SSE event parsing ------------------------------------------------

    #[test]
    fn parse_sse_event_basic() {
        let mut buf = "event: endpoint\ndata: /messages\n\n".to_string();
        let event = parse_next_sse_event(&mut buf).unwrap();
        assert_eq!(event.event_type, "endpoint");
        assert_eq!(event.data, "/messages");
        assert!(buf.is_empty());
    }

    #[test]
    fn parse_sse_event_with_json_data() {
        let mut buf = "event: message\ndata: {\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{}}\n\n".to_string();
        let event = parse_next_sse_event(&mut buf).unwrap();
        assert_eq!(event.event_type, "message");
        assert!(event.data.contains("jsonrpc"));
    }

    #[test]
    fn parse_sse_event_multiline_data() {
        let mut buf = "event: message\ndata: line1\ndata: line2\n\n".to_string();
        let event = parse_next_sse_event(&mut buf).unwrap();
        assert_eq!(event.data, "line1\nline2");
    }

    #[test]
    fn parse_sse_event_no_leading_space() {
        let mut buf = "event: test\ndata:no-space\n\n".to_string();
        let event = parse_next_sse_event(&mut buf).unwrap();
        assert_eq!(event.data, "no-space");
    }

    #[test]
    fn parse_sse_event_incomplete_returns_none() {
        let mut buf = "event: endpoint\ndata: /msg".to_string();
        assert!(parse_next_sse_event(&mut buf).is_none());
        assert_eq!(buf, "event: endpoint\ndata: /msg");
    }

    #[test]
    fn parse_sse_event_empty_buffer() {
        let mut buf = String::new();
        assert!(parse_next_sse_event(&mut buf).is_none());
    }

    #[test]
    fn parse_sse_event_multiple_in_buffer() {
        let mut buf = "event: a\ndata: 1\n\nevent: b\ndata: 2\n\n".to_string();
        let first = parse_next_sse_event(&mut buf).unwrap();
        assert_eq!(first.event_type, "a");
        assert_eq!(first.data, "1");
        let second = parse_next_sse_event(&mut buf).unwrap();
        assert_eq!(second.event_type, "b");
        assert_eq!(second.data, "2");
    }

    // -- URL resolution ---------------------------------------------------

    #[test]
    fn resolve_absolute_endpoint() {
        let result = resolve_endpoint_url("https://example.com/sse", "https://other.com/messages");
        assert_eq!(result.unwrap(), "https://other.com/messages");
    }

    #[test]
    fn resolve_relative_endpoint() {
        let result = resolve_endpoint_url("https://example.com/sse", "/messages?s=123");
        assert_eq!(result.unwrap(), "https://example.com/messages?s=123");
    }

    #[test]
    fn resolve_relative_path_endpoint() {
        let result = resolve_endpoint_url("https://example.com/mcp/sse", "messages");
        assert_eq!(result.unwrap(), "https://example.com/mcp/messages");
    }

    #[test]
    fn resolve_invalid_base_url() {
        let result = resolve_endpoint_url("not-a-url", "/messages");
        assert!(result.is_err());
    }

    // -- Auth detection ---------------------------------------------------

    #[test]
    fn detect_bearer_as_oauth() {
        assert!(matches!(
            detect_auth_method("Bearer realm=\"mcp\""),
            McpAuthMethod::Oauth
        ));
    }

    #[test]
    fn detect_oauth_keyword() {
        assert!(matches!(
            detect_auth_method("OAuth realm=\"mcp\""),
            McpAuthMethod::Oauth
        ));
    }

    #[test]
    fn detect_basic_auth() {
        assert!(matches!(
            detect_auth_method("Basic realm=\"mcp\""),
            McpAuthMethod::Basic
        ));
    }

    // -- Result builders --------------------------------------------------

    #[test]
    fn success_result_with_tools() {
        let tools_json = serde_json::json!({
            "tools": [
                { "name": "read_file", "description": "Read a file" },
                { "name": "write_file" }
            ]
        });
        let result = success_result(Some(tools_json));
        assert!(result.success);
        let tools = result.tools.unwrap();
        assert_eq!(tools.len(), 2);
        assert_eq!(tools[0].name, "read_file");
        assert_eq!(tools[0].description.as_deref(), Some("Read a file"));
        assert!(tools[1].description.is_none());
    }

    #[test]
    fn success_result_empty_tools() {
        let tools_json = serde_json::json!({ "tools": [] });
        let result = success_result(Some(tools_json));
        assert!(result.success);
        assert!(result.tools.unwrap().is_empty());
    }

    #[test]
    fn success_result_none_gives_empty_tools() {
        let result = success_result(None);
        assert!(result.success);
        assert!(result.tools.unwrap().is_empty());
    }

    #[test]
    fn success_result_malformed_gives_empty_tools() {
        let result = success_result(Some(serde_json::json!("not an object")));
        assert!(result.success);
        assert!(result.tools.unwrap().is_empty());
    }

    #[test]
    fn error_result_fields() {
        let result = error_result(
            McpConnectionTestErrorCode::ProtocolError,
            "something broke".into(),
            Some(serde_json::json!({ "stage": "initialize" })),
        );
        assert!(!result.success);
        assert_eq!(result.error.as_deref(), Some("something broke"));
        assert_eq!(result.code, Some(McpConnectionTestErrorCode::ProtocolError));
        assert_eq!(result.details.unwrap()["stage"], "initialize");
        assert!(result.tools.is_none());
        assert!(result.needs_auth.is_none());
    }

    #[test]
    fn timeout_result_message() {
        let result = timeout_result(Duration::from_secs(30));
        assert!(!result.success);
        assert!(result.error.as_deref().unwrap().contains("30s"));
        assert_eq!(result.code, Some(McpConnectionTestErrorCode::Timeout));
    }

    #[test]
    fn spawn_error_not_found() {
        let err = std::io::Error::new(std::io::ErrorKind::NotFound, "not found");
        let result = spawn_error_result("npx", &err);
        let error = result.error.as_deref().unwrap();
        assert!(error.contains("Command not found: npx"));
        assert!(error.contains("Install Node.js"));
        assert!(error.contains("absolute command path"));
        assert_eq!(result.code, Some(McpConnectionTestErrorCode::CommandNotFound));
        assert_eq!(result.details.as_ref().unwrap()["runtime"], "node");
    }

    #[test]
    fn spawn_error_not_found_generic_command() {
        let err = std::io::Error::new(std::io::ErrorKind::NotFound, "not found");
        let result = spawn_error_result("missing-mcp", &err);
        let error = result.error.as_deref().unwrap();
        assert!(error.contains("Command not found: missing-mcp"));
        assert!(error.contains("Install the command"));
        assert!(error.contains("absolute command path"));
        assert_eq!(result.details.as_ref().unwrap()["runtime"], "generic");
    }

    #[test]
    fn spawn_error_not_found_bun_command() {
        let err = std::io::Error::new(std::io::ErrorKind::NotFound, "not found");
        let result = spawn_error_result("bunx", &err);
        let error = result.error.as_deref().unwrap();
        assert!(error.contains("Command not found: bunx"));
        assert!(error.contains("Install Bun"));
        assert_eq!(result.details.as_ref().unwrap()["runtime"], "bun");
    }

    #[test]
    fn spawn_error_not_found_uv_command() {
        let err = std::io::Error::new(std::io::ErrorKind::NotFound, "not found");
        let result = spawn_error_result("uvx", &err);
        let error = result.error.as_deref().unwrap();
        assert!(error.contains("Command not found: uvx"));
        assert!(error.contains("Install uv"));
        assert_eq!(result.details.as_ref().unwrap()["runtime"], "uv");
    }

    #[test]
    fn spawn_error_not_found_python_command() {
        let err = std::io::Error::new(std::io::ErrorKind::NotFound, "not found");
        let result = spawn_error_result("python3", &err);
        let error = result.error.as_deref().unwrap();
        assert!(error.contains("Command not found: python3"));
        assert!(error.contains("Install Python"));
        assert_eq!(result.details.as_ref().unwrap()["runtime"], "python");
    }

    #[test]
    fn spawn_error_not_found_deno_command() {
        let err = std::io::Error::new(std::io::ErrorKind::NotFound, "not found");
        let result = spawn_error_result("deno", &err);
        let error = result.error.as_deref().unwrap();
        assert!(error.contains("Command not found: deno"));
        assert!(error.contains("Install Deno"));
        assert_eq!(result.details.as_ref().unwrap()["runtime"], "deno");
    }

    #[test]
    fn spawn_error_permission_denied() {
        let err = std::io::Error::new(std::io::ErrorKind::PermissionDenied, "denied");
        let result = spawn_error_result("./script.sh", &err);
        assert!(result.error.as_deref().unwrap().contains("Permission denied"));
        assert_eq!(result.code, Some(McpConnectionTestErrorCode::CommandPermissionDenied));
    }

    #[test]
    fn spawn_error_other() {
        let err = std::io::Error::other("broken pipe");
        let result = spawn_error_result("cmd", &err);
        assert!(result.error.as_deref().unwrap().contains("Failed to start"));
        assert_eq!(result.code, Some(McpConnectionTestErrorCode::CommandStartFailed));
    }

    #[test]
    fn auth_result_with_bearer() {
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert("www-authenticate", "Bearer realm=\"mcp\"".parse().unwrap());
        let result = auth_result(&headers);
        assert!(!result.success);
        assert_eq!(result.needs_auth, Some(true));
        assert!(matches!(result.auth_method, Some(McpAuthMethod::Oauth)));
        assert!(result.www_authenticate.is_some());
        assert!(result.code.is_none());
    }

    #[test]
    fn auth_result_without_www_authenticate() {
        let headers = reqwest::header::HeaderMap::new();
        let result = auth_result(&headers);
        assert_eq!(result.needs_auth, Some(true));
        assert!(result.auth_method.is_none());
        assert!(result.www_authenticate.is_none());
    }

    // -- JSON-RPC builders ------------------------------------------------

    #[test]
    fn initialize_request_structure() {
        let req = build_initialize_request(1);
        assert_eq!(req.jsonrpc, "2.0");
        assert_eq!(req.id, "1");
        assert_eq!(req.method, "initialize");
        let params = req.params.unwrap();
        assert_eq!(params["protocolVersion"], PROTOCOL_VERSION);
        assert_eq!(params["clientInfo"]["name"], CLIENT_NAME);
    }

    #[test]
    fn initialized_notification_structure() {
        let n = build_initialized_notification();
        assert_eq!(n.jsonrpc, "2.0");
        assert_eq!(n.method, "notifications/initialized");
    }

    #[test]
    fn tools_list_request_structure() {
        let req = build_tools_list_request(2);
        assert_eq!(req.id, "2");
        assert_eq!(req.method, "tools/list");
        assert!(req.params.is_none());
    }

    #[test]
    fn jsonrpc_requests_serialize_ids_as_strings() {
        let init = serde_json::to_value(build_initialize_request(1)).unwrap();
        assert_eq!(init["id"], serde_json::json!("1"));

        let tools = serde_json::to_value(build_tools_list_request(2)).unwrap();
        assert_eq!(tools["id"], serde_json::json!("2"));
    }

    #[test]
    fn jsonrpc_response_accepts_string_id() {
        let resp: JsonRpcResponse =
            serde_json::from_str(r#"{"jsonrpc":"2.0","id":"1","result":{}}"#).unwrap();
        assert!(matches!(resp.id, Some(JsonRpcId::String(ref id)) if id == "1"));
    }

    // -- HTTP header builder ----------------------------------------------

    #[test]
    fn build_headers_from_map() {
        let mut map = HashMap::new();
        map.insert("Authorization".into(), "Bearer tok".into());
        map.insert("X-Custom".into(), "val".into());
        let headers = build_http_headers(&map);
        assert_eq!(headers.get("authorization").unwrap().to_str().unwrap(), "Bearer tok");
        assert_eq!(headers.get("x-custom").unwrap().to_str().unwrap(), "val");
    }

    #[test]
    fn build_headers_empty() {
        let headers = build_http_headers(&HashMap::new());
        assert!(headers.is_empty());
    }

    // -- extract_jsonrpc_from_sse -----------------------------------------

    #[test]
    fn extract_jsonrpc_from_sse_basic() {
        let body = "event: message\ndata: {\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{}}\n";
        let resp = extract_jsonrpc_from_sse(body).unwrap();
        assert!(matches!(resp.id, Some(JsonRpcId::Number(1))));
    }

    #[test]
    fn extract_jsonrpc_skips_notifications() {
        let body =
            "data: {\"jsonrpc\":\"2.0\",\"method\":\"log\"}\ndata: {\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{}}\n";
        let resp = extract_jsonrpc_from_sse(body).unwrap();
        assert!(matches!(resp.id, Some(JsonRpcId::Number(1))));
    }

    #[test]
    fn extract_jsonrpc_from_sse_no_response() {
        let body = "data: not json\ndata: {\"jsonrpc\":\"2.0\",\"method\":\"log\"}\n";
        assert!(extract_jsonrpc_from_sse(body).is_err());
    }
}

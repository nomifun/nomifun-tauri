//! `nomicore mcp-knowledge-stdio` subcommand: MCP stdio server for the
//! per-session knowledge-search tool (`knowledge_search`).
//!
//! Spawned by ACP agent CLIs (claude / codex / gemini) when the knowledge MCP
//! is injected into a session that has knowledge bases mounted. Uses the `rmcp`
//! crate (Rust MCP SDK) for protocol handling so it is byte-compatible with each
//! CLI's MCP client.
//!
//! Tool calls are forwarded as authenticated HTTP POSTs to the in-process
//! `KnowledgeMcpServer` running in the main backend process at
//! `http://127.0.0.1:{port}/tool`. This stdio→HTTP hop exists because the
//! spawned process cannot share the main process's knowledge services, and
//! because claude / codex / gemini advertise stdio-only MCP capabilities (a
//! direct HTTP MCP server would be dropped by the ACP capability filter).
//!
//! The bridge discovers the backend endpoint via the MCP beacon file (written by
//! the backend on boot), falling back to `NOMI_KB_MCP_PORT`/`NOMI_KB_MCP_TOKEN`
//! env vars for compatibility. Scope is determined at runtime: the bridge reports
//! its `cwd` to the in-process server, which resolves the bound knowledge bases
//! for that working directory. The model cannot widen the scope — it only
//! passes a query; the server decides which bases to search.

// Pre-existing layout convention (mirrors requirement_stdio / team_guide): the
// `forward_tool` impl block lives after the test module.
#![allow(clippy::items_after_test_module)]

use std::process::ExitCode;

use nomifun_api_types::KnowledgeMcpConfig;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::{schemars, service::ServiceExt, tool, tool_router, transport};
use serde::Deserialize;

pub async fn run_knowledge_stdio() -> ExitCode {
    // --- Endpoint discovery: beacon first, env fallback ---
    let (port, token) = if let Some(ep) = crate::mcp_endpoints::read_beacon_for_bridge()
        .and_then(|e| e.knowledge)
    {
        eprintln!("[mcp-knowledge-stdio] Endpoint resolved via beacon");
        (ep.port, ep.token)
    } else {
        // Fallback to legacy env vars (internal terminal spawn still sets these
        // as a safety net; external terminals without a beacon file also land here).
        let port: u16 = match std::env::var(KnowledgeMcpConfig::ENV_PORT) {
            Ok(p) => match p.parse() {
                Ok(v) => v,
                Err(e) => {
                    eprintln!("[mcp-knowledge-stdio] ERROR: invalid {}: {e}", KnowledgeMcpConfig::ENV_PORT);
                    return ExitCode::from(1);
                }
            },
            Err(_) => {
                eprintln!(
                    "[mcp-knowledge-stdio] ERROR: nomifun desktop not running or restarted — \
                     restart this terminal / ensure desktop is running \
                     (neither beacon file nor {} env found)",
                    KnowledgeMcpConfig::ENV_PORT
                );
                return ExitCode::from(1);
            }
        };
        let token = match std::env::var(KnowledgeMcpConfig::ENV_TOKEN) {
            Ok(t) => t,
            Err(_) => {
                eprintln!(
                    "[mcp-knowledge-stdio] ERROR: nomifun desktop not running or restarted — \
                     restart this terminal / ensure desktop is running \
                     (missing {})",
                    KnowledgeMcpConfig::ENV_TOKEN
                );
                return ExitCode::from(1);
            }
        };
        (port, token)
    };

    // Capture the bridge process's current working directory at startup. The
    // in-process server uses it to resolve which knowledge bases are bound to
    // this workspace (runtime cwd scope — no baked kb_ids).
    let cwd = std::env::current_dir()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_default();

    eprintln!("[mcp-knowledge-stdio] Started OK. PORT={port}, cwd={cwd}");

    let http_client = super::stdio_common::build_bridge_http_client();

    let server = KnowledgeStdioServer {
        port,
        token,
        cwd,
        http_client,
    };

    let transport = transport::io::stdio();
    match server.serve(transport).await {
        Ok(peer) => {
            eprintln!("[mcp-knowledge-stdio] MCP session started, waiting for completion...");
            if let Err(e) = peer.waiting().await {
                eprintln!("[mcp-knowledge-stdio] Session ended with error: {e}");
            } else {
                eprintln!("[mcp-knowledge-stdio] Session ended normally");
            }
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("[mcp-knowledge-stdio] Failed to start MCP server: {e}");
            ExitCode::from(1)
        }
    }
}

#[derive(Clone)]
struct KnowledgeStdioServer {
    port: u16,
    token: String,
    cwd: String,
    http_client: reqwest::Client,
}

#[derive(Deserialize, schemars::JsonSchema)]
struct SearchParams {
    /// The search query. Phrase it as the topic or question you need information
    /// about; the bases are ranked by relevance to this text.
    query: String,
    /// Optional maximum number of ranked results to return.
    #[serde(default)]
    limit: Option<u32>,
}

#[derive(Deserialize, schemars::JsonSchema)]
struct ReadParams {
    /// The opaque `handle` from a knowledge_search result.
    handle: String,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub(crate) struct WriteParams {
    /// To UPDATE an existing document: the opaque `handle` from a knowledge_search result.
    #[serde(default)]
    handle: Option<String>,
    /// For a NEW document: which knowledge base to write to (its name).
    #[serde(default)]
    base: Option<String>,
    /// For a NEW document: relative markdown path within the base (e.g. "terms.md"). Must end with .md.
    #[serde(default)]
    rel_path: Option<String>,
    /// The FULL markdown content to store.
    content: String,
}

#[tool_router(server_handler)]
impl KnowledgeStdioServer {
    #[tool(
        name = "knowledge_search",
        description = "Search the knowledge bases mounted into THIS session for relevant documents. Call this FIRST, before answering from memory, when the task touches any topic the bases may cover. Returns ranked results with an opaque `handle`; read a full result by calling knowledge_read with that exact handle. Copy the handle unchanged and do not rebuild it from the path."
    )]
    async fn knowledge_search(&self, Parameters(params): Parameters<SearchParams>) -> String {
        eprintln!("[mcp-knowledge-stdio] tools/call: knowledge_search");
        self.forward_tool(params.query, params.limit).await
    }

    #[tool(
        name = "knowledge_read",
        description = "Read the FULL markdown of a knowledge document by the `handle` returned by knowledge_search. Use this before updating a document so you can merge into its current content, then write it back with knowledge_write passing the same `handle`."
    )]
    async fn knowledge_read(&self, Parameters(params): Parameters<ReadParams>) -> String {
        eprintln!("[mcp-knowledge-stdio] tools/call: knowledge_read");
        self.forward_read(params.handle).await
    }

    #[tool(
        name = "knowledge_write",
        description = "Persist reusable knowledge INTO a mounted knowledge base. To UPDATE an existing document, pass its `handle` from a knowledge_search result; to CREATE a new one, pass `base` + a descriptive `.md` `rel_path`. Always include the full markdown `content`. Whether the write lands directly or is staged for review is decided by the workspace's write-back setting — you do not manage placement."
    )]
    async fn knowledge_write(&self, Parameters(params): Parameters<WriteParams>) -> String {
        eprintln!("[mcp-knowledge-stdio] tools/call: knowledge_write");
        self.forward_write(params).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn all_tool_schemas_have_properties_field() {
        let router = KnowledgeStdioServer::tool_router();
        let tools = router.list_all();
        assert!(!tools.is_empty(), "knowledge bridge must register at least one tool");
        for tool in &tools {
            assert!(
                tool.input_schema.contains_key("properties"),
                "Tool '{}' schema missing 'properties' field: {:?}. OpenAI API rejects schemas without it.",
                tool.name,
                tool.input_schema,
            );
        }
    }

    #[test]
    fn registers_knowledge_search_tool() {
        let router = KnowledgeStdioServer::tool_router();
        let names: Vec<String> = router.list_all().iter().map(|t| t.name.to_string()).collect();
        assert!(names.contains(&"knowledge_search".to_string()), "got {names:?}");
    }

    #[test]
    fn build_search_body_contains_cwd_and_no_kb_ids() {
        let body = build_search_body("how to deploy", Some(5), "/home/user/project");
        assert_eq!(body["tool"], "knowledge_search");
        assert_eq!(body["args"]["query"], "how to deploy");
        assert_eq!(body["args"]["limit"], 5);
        assert_eq!(body["cwd"], "/home/user/project");
        // Must NOT contain kb_ids — scope is resolved by the server from cwd.
        assert!(body.get("kb_ids").is_none(), "body must not contain kb_ids, got: {body}");
    }

    #[test]
    fn build_search_body_with_none_limit() {
        let body = build_search_body("query", None, "/tmp");
        assert_eq!(body["args"]["limit"], serde_json::Value::Null);
        assert_eq!(body["cwd"], "/tmp");
    }

    #[test]
    fn build_search_body_with_empty_cwd() {
        let body = build_search_body("query", None, "");
        assert_eq!(body["cwd"], "");
    }

    #[test]
    fn registers_read_and_write_tools() {
        let router = KnowledgeStdioServer::tool_router();
        let names: Vec<String> = router.list_all().iter().map(|t| t.name.to_string()).collect();
        assert!(names.contains(&"knowledge_read".to_string()), "got {names:?}");
        assert!(names.contains(&"knowledge_write".to_string()), "got {names:?}");
        let tools = router.list_all();
        let search = tools
            .iter()
            .find(|tool| tool.name.as_ref() == "knowledge_search")
            .expect("knowledge_search registered");
        let description = search.description.as_deref().unwrap_or_default();
        assert!(description.contains("knowledge_read") && description.contains("handle"));
        assert!(!description.contains("Read tool"));
    }

    #[test]
    fn build_read_body_shape() {
        let body = build_read_body("kdoc_abc", "/wd");
        assert_eq!(body["tool"], "knowledge_read");
        assert_eq!(body["args"]["handle"], "kdoc_abc");
        assert_eq!(body["cwd"], "/wd");
        assert!(body.get("kb_ids").is_none(), "scope is server-resolved: {body}");
    }

    #[test]
    fn build_write_body_handle_and_path_variants() {
        let upd = WriteParams { handle: Some("kdoc_x".into()), base: None, rel_path: None, content: "merged".into() };
        let b = build_write_body(&upd, "/wd");
        assert_eq!(b["tool"], "knowledge_write");
        assert_eq!(b["args"]["handle"], "kdoc_x");
        assert_eq!(b["args"]["content"], "merged");
        assert_eq!(b["cwd"], "/wd");
        assert!(b.get("kb_ids").is_none(), "no kb_ids/policy in body: {b}");

        let create = WriteParams { handle: None, base: Some("Finance".into()), rel_path: Some("terms.md".into()), content: "c".into() };
        let b2 = build_write_body(&create, "/wd");
        assert_eq!(b2["args"]["base"], "Finance");
        assert_eq!(b2["args"]["rel_path"], "terms.md");
        assert_eq!(b2["args"]["handle"], serde_json::Value::Null);
    }
}

/// Build the HTTP body for forwarding a knowledge_search tool call to the
/// in-process `KnowledgeMcpServer`. Pure function — easily testable.
///
/// Shape: `{"tool":"knowledge_search","args":{"query":..,"limit":..},"cwd":..}`
/// No `kb_ids` — scope is resolved by the server at runtime from `cwd`.
pub(crate) fn build_search_body(query: &str, limit: Option<u32>, cwd: &str) -> serde_json::Value {
    serde_json::json!({
        "tool": "knowledge_search",
        "args": { "query": query, "limit": limit },
        "cwd": cwd,
    })
}

/// Body for `knowledge_read`. No `kb_ids` — the server scope-checks the handle
/// against the cwd-resolved bases.
pub(crate) fn build_read_body(handle: &str, cwd: &str) -> serde_json::Value {
    serde_json::json!({
        "tool": "knowledge_read",
        "args": { "handle": handle },
        "cwd": cwd,
    })
}

/// Body for `knowledge_write`. No `kb_ids`/policy — the server resolves the
/// write policy from the cwd workpath binding (model cannot widen it).
pub(crate) fn build_write_body(p: &WriteParams, cwd: &str) -> serde_json::Value {
    serde_json::json!({
        "tool": "knowledge_write",
        "args": { "handle": p.handle, "base": p.base, "rel_path": p.rel_path, "content": p.content },
        "cwd": cwd,
    })
}

impl KnowledgeStdioServer {
    async fn forward_tool(&self, query: String, limit: Option<u32>) -> String {
        let body = build_search_body(&query, limit, &self.cwd);
        super::stdio_common::forward_tool_http(
            &self.http_client,
            self.port,
            &self.token,
            "mcp-knowledge-stdio",
            &body,
            false,
        )
        .await
    }

    async fn forward_read(&self, handle: String) -> String {
        let body = build_read_body(&handle, &self.cwd);
        super::stdio_common::forward_tool_http(
            &self.http_client,
            self.port,
            &self.token,
            "mcp-knowledge-stdio",
            &body,
            false,
        )
        .await
    }

    async fn forward_write(&self, params: WriteParams) -> String {
        let body = build_write_body(&params, &self.cwd);
        super::stdio_common::forward_tool_http(
            &self.http_client,
            self.port,
            &self.token,
            "mcp-knowledge-stdio",
            &body,
            false,
        )
        .await
    }
}

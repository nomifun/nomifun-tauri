//! Native `knowledge_search` tool: lets an in-process agent search the
//! knowledge bases mounted into its session through a `KnowledgeRetrievalSink`
//! trait object. The backend injects a concrete sink scoped to the session's
//! bound bases; standalone `nomi-cli` passes `None` and the tool is absent.
//!
//! Mirrors `requirement_tools.rs`: trait here, impl in `nomifun-ai-agent`.

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{Value, json};

use nomi_protocol::events::ToolCategory;
use nomi_tools::Tool;
use nomi_types::tool::{JsonSchema, ToolResult};

/// One retrieval hit. `rel_path` is relative to the base root. `handle` is the
/// opaque token the model passes to `knowledge_read` / `knowledge_write` so it
/// never has to reconstruct a path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KnowledgeHit {
    pub kb_id: String,
    pub handle: String,
    pub kb_name: String,
    pub rel_path: String,
    pub heading: String,
    pub snippet: String,
}

/// Backend seam for knowledge retrieval. Implemented by the backend over its
/// `KnowledgeService`; `nomi-agent` only depends on this trait.
#[async_trait]
pub trait KnowledgeRetrievalSink: Send + Sync {
    /// Search the given bases for `query`, returning up to `limit` ranked hits.
    async fn search(&self, kb_ids: &[String], query: &str, limit: usize) -> Result<Vec<KnowledgeHit>, String>;
    /// Return the full markdown for the document addressed by the opaque
    /// `handle`, scoped to `kb_ids` (a handle outside the set is an error).
    async fn read_document(&self, kb_ids: &[String], handle: &str) -> Result<String, String>;
}

/// `knowledge_search` — search the bases mounted into this session. Holds the
/// session's bound `kb_ids` (opaque to the model) plus the shared sink.
pub struct KnowledgeSearchTool {
    sink: Arc<dyn KnowledgeRetrievalSink>,
    kb_ids: Vec<String>,
}

impl KnowledgeSearchTool {
    pub fn new(sink: Arc<dyn KnowledgeRetrievalSink>, kb_ids: Vec<String>) -> Self {
        Self { sink, kb_ids }
    }
}

/// Default and max hits returned to the model.
const DEFAULT_LIMIT: usize = 8;
const MAX_LIMIT: usize = 20;

#[async_trait]
impl Tool for KnowledgeSearchTool {
    fn name(&self) -> &str {
        "knowledge_search"
    }

    fn description(&self) -> &str {
        "Search the knowledge bases mounted into THIS session for relevant documents. \
         Call this FIRST, before answering from memory, whenever the task or question touches \
         any topic the mounted bases may cover. Returns ranked results as `base / path — heading` \
         with a snippet and an opaque `handle`; read the full document by calling knowledge_read \
         with that exact handle. Copy the handle unchanged and do not rebuild it from the path. \
         This searches the real base content directly (not the workspace mount), so it always \
         finds matches even when Grep/Glob cannot."
    }

    fn input_schema(&self) -> JsonSchema {
        json!({
            "type": "object",
            "properties": {
                "query": {
                    "type": "string",
                    "description": "What to look for. Use the user's topic words; natural language is fine."
                },
                "limit": {
                    "type": "integer",
                    "description": "Max results to return (default 8, max 20)"
                }
            },
            "required": ["query"]
        })
    }

    fn is_concurrency_safe(&self, _input: &Value) -> bool {
        true
    }

    fn is_deferred(&self) -> bool {
        // NOT deferred: the retrieval protocol instructs the agent to call this
        // directly, so its schema must be visible up front (same rationale as
        // requirement_complete).
        false
    }

    async fn execute(&self, input: Value) -> ToolResult {
        let query = match input.get("query").and_then(|v| v.as_str()) {
            Some(s) if !s.trim().is_empty() => s.trim(),
            _ => return ToolResult::error("Missing required 'query' string"),
        };
        if self.kb_ids.is_empty() {
            return ToolResult::text("No knowledge bases are mounted in this session.");
        }
        let limit = input
            .get("limit")
            .and_then(|v| v.as_u64())
            .map(|n| (n as usize).clamp(1, MAX_LIMIT))
            .unwrap_or(DEFAULT_LIMIT);

        match self.sink.search(&self.kb_ids, query, limit).await {
            Ok(hits) if hits.is_empty() => ToolResult::text(format!(
                "No matches for \"{query}\" in the mounted knowledge bases. \
                 Try different terms, or list files with Glob under the base path."
            )),
            Ok(hits) => ToolResult::text(format_hits(query, &hits)),
            Err(e) => ToolResult::error(format!("knowledge_search failed: {e}")),
        }
    }

    fn category(&self) -> ToolCategory {
        ToolCategory::Info
    }

    fn describe(&self, input: &Value) -> String {
        let q = input.get("query").and_then(|v| v.as_str()).unwrap_or("");
        format!("knowledge_search '{q}'")
    }
}

/// Render hits as a compact, agent-actionable list.
fn format_hits(query: &str, hits: &[KnowledgeHit]) -> String {
    let mut out = format!("{} result(s) for \"{}\":\n", hits.len(), query);
    for (i, h) in hits.iter().enumerate() {
        out.push_str(&format!(
            "{}. [{}] {} — {}\n   {}\n   handle: {}\n",
            i + 1,
            h.kb_name,
            h.rel_path,
            if h.heading.is_empty() { "(no heading)" } else { &h.heading },
            h.snippet,
            h.handle,
        ));
    }
    out.push_str(
        "\nTo read a full document, call knowledge_read with its `handle`. \
         To update one, call knowledge_write with that same `handle` (do NOT rebuild the path).",
    );
    out
}

// ── knowledge_read ─────────────────────────────────────────────────────────

/// `knowledge_read` — fetch a full document by the opaque `handle` from a prior
/// `knowledge_search` result. Removes all path arithmetic from the read→update
/// loop. Holds the session's bound `kb_ids` so a handle outside them is denied.
pub struct KnowledgeReadTool {
    sink: Arc<dyn KnowledgeRetrievalSink>,
    kb_ids: Vec<String>,
}

impl KnowledgeReadTool {
    pub fn new(sink: Arc<dyn KnowledgeRetrievalSink>, kb_ids: Vec<String>) -> Self {
        Self { sink, kb_ids }
    }
}

#[async_trait]
impl Tool for KnowledgeReadTool {
    fn name(&self) -> &str {
        "knowledge_read"
    }

    fn description(&self) -> &str {
        "Read the FULL markdown of a knowledge document by the `handle` returned by knowledge_search. \
         Use this before updating a document so you can merge into its current content, then write it \
         back with knowledge_write passing the same `handle`."
    }

    fn input_schema(&self) -> JsonSchema {
        json!({
            "type": "object",
            "properties": {
                "handle": { "type": "string", "description": "The opaque `handle` from a knowledge_search result." }
            },
            "required": ["handle"]
        })
    }

    fn is_concurrency_safe(&self, _input: &Value) -> bool {
        true
    }

    fn is_deferred(&self) -> bool {
        false
    }

    async fn execute(&self, input: Value) -> ToolResult {
        let handle = match input.get("handle").and_then(Value::as_str) {
            Some(s) if !s.trim().is_empty() => s.trim(),
            _ => return ToolResult::error("Missing required 'handle' string"),
        };
        match self.sink.read_document(&self.kb_ids, handle).await {
            Ok(content) => ToolResult::text(content),
            Err(e) => ToolResult::error(format!("knowledge_read failed: {e}")),
        }
    }

    fn category(&self) -> ToolCategory {
        ToolCategory::Info
    }

    fn describe(&self, _input: &Value) -> String {
        "knowledge_read".to_owned()
    }
}

// ── knowledge_write ──────────────────────────────────────────────────────

/// Backend seam for knowledge write-back. Implemented by the backend over its
/// `KnowledgeService::write_file`; `nomi-agent` only depends on this trait.
///
/// Deliberately a thin write primitive (kb_id + rel_path + content, overwrite
/// semantics) — exactly mirroring [`KnowledgeRetrievalSink`]. All the policy
/// (which bases are writable, staged-vs-direct placement) is baked into
/// [`KnowledgeWriteTool`] at construction so the model can never widen it.
/// What the model addressed for a write: an opaque `handle` (preferred — from
/// `knowledge_search`/`knowledge_read`) or an explicit base + relative path
/// (create a new document).
#[derive(Debug, Clone)]
pub enum WriteTarget {
    Handle(String),
    Path { kb_id: String, rel_path: String },
}

/// Placement mode for a write-back, decided by the backend per session/surface
/// and baked into [`KnowledgeWriteTool`] at construction. `Staged{scope}` → the
/// backend places the write under a review inbox keyed by `scope`; `Direct` →
/// the base body. The tool never builds the final path itself.
#[derive(Debug, Clone)]
pub enum WriteMode {
    Staged { scope: String },
    Direct,
}

/// A model-issued write, resolved by the backend [`KnowledgeWritebackSink`]
/// (handle/path → existing doc or new file) before placement is applied.
#[derive(Debug, Clone)]
pub struct WriteRequest {
    pub target: WriteTarget,
    pub content: String,
    pub mode: WriteMode,
    /// Bases this session may write to; the backend rejects a handle/path
    /// outside this set.
    pub bound_kb_ids: Vec<String>,
}

/// What actually happened, for the tool's confirmation message.
#[derive(Debug, Clone)]
pub struct WriteReceipt {
    pub final_rel_path: String,
    pub staged: bool,
    pub updated: bool,
}

/// Backend seam for knowledge write-back. Implemented by the backend over its
/// `KnowledgeService::write_document`; `nomi-agent` only depends on this trait.
/// The backend resolves the target (fixing path confusion / locating the
/// existing doc), enforces placement, and writes — the tool layer only forwards
/// the model's intent.
#[async_trait]
pub trait KnowledgeWritebackSink: Send + Sync {
    async fn write(&self, req: WriteRequest) -> Result<WriteReceipt, String>;
}

/// `knowledge_write` — persist reusable knowledge straight into a bound base.
///
/// This is the write-back ("回血") counterpart of [`KnowledgeSearchTool`]. The
/// generic file `Write` tool cannot do this reliably in a nomi chat session:
/// it has no workspace cwd (so the relative mount path the prompt advertises
/// resolves against the process cwd, missing the base) AND it sits behind the
/// approval gate. This tool resolves the base by name within the bound set,
/// applies the session's staged/direct placement, and writes through the
/// backend service directly — the backend adds it to the allow-list so it
/// bypasses the per-call approval prompt (same posture as companion memory
/// tools and `knowledge_search`).
pub struct KnowledgeWriteTool {
    sink: Arc<dyn KnowledgeWritebackSink>,
    /// Bound bases as `(kb_id, name)`. The model selects by `name`; `kb_id` is
    /// opaque to it. Used to resolve `base` → `kb_id` for the create path.
    bases: Vec<(String, String)>,
    /// Placement mode baked at construction (Staged inbox scope, or Direct).
    mode: WriteMode,
    /// Bases this session may write to (forwarded to the backend for scope
    /// enforcement). Mirrors the search/read tools' `kb_ids`.
    bound_kb_ids: Vec<String>,
}

impl KnowledgeWriteTool {
    pub fn new(
        sink: Arc<dyn KnowledgeWritebackSink>,
        bases: Vec<(String, String)>,
        mode: WriteMode,
        bound_kb_ids: Vec<String>,
    ) -> Self {
        Self { sink, bases, mode, bound_kb_ids }
    }

    /// One-line description of the bound bases for the schema/description.
    fn base_names(&self) -> Vec<&str> {
        self.bases.iter().map(|(_, name)| name.as_str()).collect()
    }
}

/// Resolve the model-supplied base name to a bound `(kb_id, name)`. When the
/// model omits `requested` and exactly one base is bound, that base is used.
/// Matching is case-insensitive and whitespace-trimmed. `Err` carries a
/// ready-to-return, model-actionable message.
fn resolve_write_base<'a>(
    bases: &'a [(String, String)],
    requested: Option<&str>,
) -> Result<&'a (String, String), String> {
    if bases.is_empty() {
        return Err("No knowledge bases are mounted in this session, so there is nothing to write to.".to_owned());
    }
    match requested.map(str::trim).filter(|s| !s.is_empty()) {
        Some(name) => bases
            .iter()
            .find(|(_, n)| n.trim().eq_ignore_ascii_case(name))
            .ok_or_else(|| {
                let names = bases.iter().map(|(_, n)| n.as_str()).collect::<Vec<_>>().join(", ");
                format!("Unknown knowledge base \"{name}\". Mounted bases are: {names}. Pass one of these exact names as `base`.")
            }),
        None => {
            if bases.len() == 1 {
                Ok(&bases[0])
            } else {
                let names = bases.iter().map(|(_, n)| n.as_str()).collect::<Vec<_>>().join(", ");
                Err(format!(
                    "Multiple knowledge bases are mounted ({names}). Specify which one with the `base` argument."
                ))
            }
        }
    }
}

/// Validate + normalize a model-supplied relative markdown path. Mirrors the
/// backend `safe_md_path` contract early so the model gets an actionable error
/// instead of a generic service rejection: `.md` only, no absolute paths, no
/// `.`/`..`/empty components (traversal), backslashes normalized to `/`.
fn normalize_write_rel_path(rel_path: &str) -> Result<String, String> {
    let raw = rel_path.trim().replace('\\', "/");
    if raw.is_empty() {
        return Err("`rel_path` must not be empty.".to_owned());
    }
    if raw.starts_with('/') {
        return Err(format!("`rel_path` must be relative to the base, not absolute: \"{rel_path}\"."));
    }
    for seg in raw.split('/') {
        if seg.is_empty() || seg == "." || seg == ".." {
            return Err(format!("`rel_path` contains an invalid path segment: \"{rel_path}\"."));
        }
    }
    if !raw.to_ascii_lowercase().ends_with(".md") {
        return Err(format!("Knowledge files must be markdown: `rel_path` has to end with .md (got \"{rel_path}\")."));
    }
    Ok(raw)
}

#[async_trait]
impl Tool for KnowledgeWriteTool {
    fn name(&self) -> &str {
        "knowledge_write"
    }

    fn description(&self) -> &str {
        "Persist reusable knowledge (conclusions, domain facts, decisions, lessons) INTO a mounted \
         knowledge base so it survives across sessions. Use THIS tool for write-back — never the generic \
         Write/Edit file tools. To UPDATE an existing document, pass its `handle` from a knowledge_search \
         result (read it first with knowledge_read, merge, then write the full content). To CREATE a new \
         document, pass `base` + a descriptive `.md` `rel_path`. Never rebuild paths by hand."
    }

    fn input_schema(&self) -> JsonSchema {
        let names = self.base_names();
        let base_desc = if names.len() <= 1 {
            "For a NEW document: which knowledge base to write to (its name). Optional when only one base is mounted.".to_owned()
        } else {
            format!("For a NEW document: which knowledge base to write to. Must be one of: {}.", names.join(", "))
        };
        json!({
            "type": "object",
            "properties": {
                "handle": {
                    "type": "string",
                    "description": "To UPDATE an existing document: the opaque `handle` from a knowledge_search result. Do not rebuild paths."
                },
                "base": { "type": "string", "description": base_desc },
                "rel_path": {
                    "type": "string",
                    "description": "For a NEW document only: relative markdown path within the base, e.g. \"terms.md\". Must end with .md. Ignored when `handle` is given."
                },
                "content": {
                    "type": "string",
                    "description": "The FULL markdown content to store (overwrite semantics for updates). Keep it self-contained and free of session-specific noise."
                }
            },
            "required": ["content"]
        })
    }

    fn is_concurrency_safe(&self, _input: &Value) -> bool {
        false
    }

    fn is_deferred(&self) -> bool {
        // NOT deferred: the write-back contract instructs the agent to call this
        // directly, so its schema must be visible up front (same rationale as
        // knowledge_search / requirement_complete).
        false
    }

    async fn execute(&self, input: Value) -> ToolResult {
        let Some(content) = input.get("content").and_then(Value::as_str) else {
            return ToolResult::error("Missing required 'content' string");
        };
        if content.trim().is_empty() {
            return ToolResult::error("'content' is empty — refusing to write a blank knowledge file");
        }
        let target = if let Some(handle) =
            input.get("handle").and_then(Value::as_str).map(str::trim).filter(|s| !s.is_empty())
        {
            WriteTarget::Handle(handle.to_owned())
        } else {
            let rel_path = match input.get("rel_path").and_then(Value::as_str) {
                Some(p) => match normalize_write_rel_path(p) {
                    Ok(p) => p,
                    Err(e) => return ToolResult::error(e),
                },
                None => {
                    return ToolResult::error(
                        "Pass either `handle` (to update an existing document) or `rel_path` (to create a new one).",
                    );
                }
            };
            let kb_id = match resolve_write_base(&self.bases, input.get("base").and_then(Value::as_str)) {
                Ok(b) => b.0.clone(),
                Err(e) => return ToolResult::error(e),
            };
            WriteTarget::Path { kb_id, rel_path }
        };
        let req = WriteRequest {
            target,
            content: content.to_owned(),
            mode: self.mode.clone(),
            bound_kb_ids: self.bound_kb_ids.clone(),
        };
        match self.sink.write(req).await {
            Ok(r) => {
                let verb = if r.updated { "Updated" } else { "Saved" };
                let note = if r.staged {
                    " (STAGED to the review inbox; the user merges it into the base later)"
                } else {
                    ""
                };
                ToolResult::text(format!("{verb} knowledge document at {}{note}.", r.final_rel_path))
            }
            Err(e) => ToolResult::error(format!("knowledge_write failed: {e}")),
        }
    }

    fn category(&self) -> ToolCategory {
        ToolCategory::Edit
    }

    fn describe(&self, input: &Value) -> String {
        let base = input.get("base").and_then(|v| v.as_str()).unwrap_or("");
        let path = input.get("rel_path").and_then(|v| v.as_str()).unwrap_or("");
        if base.is_empty() {
            format!("knowledge_write '{path}'")
        } else {
            format!("knowledge_write '{base}/{path}'")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct FakeSink {
        hits: Vec<KnowledgeHit>,
        last_query: std::sync::Mutex<String>,
    }

    #[async_trait]
    impl KnowledgeRetrievalSink for FakeSink {
        async fn search(&self, _kb_ids: &[String], query: &str, _limit: usize) -> Result<Vec<KnowledgeHit>, String> {
            *self.last_query.lock().unwrap() = query.to_owned();
            Ok(self.hits.clone())
        }
        async fn read_document(&self, _kb_ids: &[String], _handle: &str) -> Result<String, String> {
            Ok(String::new())
        }
    }

    fn tool_with(hits: Vec<KnowledgeHit>, kb_ids: Vec<String>) -> (KnowledgeSearchTool, Arc<FakeSink>) {
        let sink = Arc::new(FakeSink { hits, last_query: std::sync::Mutex::new(String::new()) });
        (KnowledgeSearchTool::new(sink.clone(), kb_ids), sink)
    }

    #[test]
    fn search_description_requires_knowledge_read_handle() {
        let (tool, _) = tool_with(vec![], vec!["kb1".into()]);
        let description = tool.description();
        assert!(description.contains("knowledge_read") && description.contains("handle"));
        assert!(!description.contains("Read tool using the given path"));
    }

    #[tokio::test]
    async fn formats_hits_and_passes_trimmed_query() {
        let hits = vec![KnowledgeHit {
            kb_id: "kb1".into(),
            handle: "kdoc_abc".into(),
            kb_name: "运维手册".into(),
            rel_path: "deploy/rollback.md".into(),
            heading: "回滚流程".into(),
            snippet: "回滚分三步……".into(),
        }];
        let (tool, sink) = tool_with(hits, vec!["kb1".into()]);
        let res = tool.execute(json!({"query": "  回滚  "})).await;
        assert!(!res.is_error, "{res:?}");
        assert!(res.content.contains("运维手册"));
        assert!(res.content.contains("deploy/rollback.md"));
        assert!(res.content.contains("回滚流程"));
        assert!(res.content.contains("kdoc_abc"), "handle must appear: {}", res.content);
        assert!(
            res.content.contains("knowledge_read") || res.content.contains("knowledge_write"),
            "must instruct handle use: {}",
            res.content
        );
        assert_eq!(*sink.last_query.lock().unwrap(), "回滚", "query must be trimmed");
    }

    #[tokio::test]
    async fn empty_kb_ids_returns_no_bases_message() {
        let (tool, _sink) = tool_with(vec![], vec![]);
        let res = tool.execute(json!({"query": "x"})).await;
        assert!(!res.is_error);
        assert!(res.content.contains("No knowledge bases are mounted"));
    }

    #[tokio::test]
    async fn missing_query_is_error() {
        let (tool, _sink) = tool_with(vec![], vec!["kb1".into()]);
        let res = tool.execute(json!({})).await;
        assert!(res.is_error);
        assert!(res.content.contains("Missing required 'query'"));
    }

    #[tokio::test]
    async fn no_hits_suggests_alternatives() {
        let (tool, _sink) = tool_with(vec![], vec!["kb1".into()]);
        let res = tool.execute(json!({"query": "无此主题"})).await;
        assert!(!res.is_error);
        assert!(res.content.contains("No matches"));
    }

    // ── knowledge_read ───────────────────────────────────────────────

    struct FakeReadSink;
    #[async_trait]
    impl KnowledgeRetrievalSink for FakeReadSink {
        async fn search(&self, _: &[String], _: &str, _: usize) -> Result<Vec<KnowledgeHit>, String> {
            Ok(vec![])
        }
        async fn read_document(&self, kb_ids: &[String], handle: &str) -> Result<String, String> {
            if handle == "kdoc_ok" && kb_ids == ["kb1"] {
                Ok("FULL DOC".into())
            } else {
                Err("not found".into())
            }
        }
    }

    #[tokio::test]
    async fn read_tool_returns_full_content_by_handle() {
        let tool = KnowledgeReadTool::new(Arc::new(FakeReadSink), vec!["kb1".into()]);
        let ok = tool.execute(json!({"handle": "kdoc_ok"})).await;
        assert!(!ok.is_error && ok.content.contains("FULL DOC"), "{ok:?}");
        let bad = tool.execute(json!({"handle": "kdoc_no"})).await;
        assert!(bad.is_error);
        let missing = tool.execute(json!({})).await;
        assert!(missing.is_error);
    }

    // ── knowledge_write ──────────────────────────────────────────────

    #[derive(Default)]
    struct FakeWriteSink {
        last: std::sync::Mutex<Option<WriteRequest>>,
        fail: bool,
    }

    #[async_trait]
    impl KnowledgeWritebackSink for FakeWriteSink {
        async fn write(&self, req: WriteRequest) -> Result<WriteReceipt, String> {
            if self.fail {
                return Err("disk full".to_owned());
            }
            let staged = matches!(req.mode, WriteMode::Staged { .. });
            let final_rel_path = match &req.target {
                WriteTarget::Handle(h) => h.clone(),
                WriteTarget::Path { rel_path, .. } => rel_path.clone(),
            };
            *self.last.lock().unwrap() = Some(req);
            Ok(WriteReceipt { final_rel_path, staged, updated: true })
        }
    }

    fn write_tool(bases: Vec<(&str, &str)>, mode: WriteMode) -> (KnowledgeWriteTool, Arc<FakeWriteSink>) {
        let sink = Arc::new(FakeWriteSink::default());
        let bases: Vec<(String, String)> = bases.into_iter().map(|(i, n)| (i.to_owned(), n.to_owned())).collect();
        let bound: Vec<String> = bases.iter().map(|(i, _)| i.clone()).collect();
        (KnowledgeWriteTool::new(sink.clone(), bases, mode, bound), sink)
    }

    fn direct() -> WriteMode {
        WriteMode::Direct
    }
    fn staged(scope: &str) -> WriteMode {
        WriteMode::Staged { scope: scope.to_owned() }
    }

    // resolve_write_base ------------------------------------------------

    #[test]
    fn resolve_base_single_default_and_by_name() {
        let one = vec![("kb1".to_owned(), "金融知识库".to_owned())];
        assert_eq!(resolve_write_base(&one, None).unwrap().0, "kb1");
        // Case-insensitive + trimmed name match across multiple bases.
        let many = vec![("kb1".to_owned(), "Finance".to_owned()), ("kb2".to_owned(), "Ops".to_owned())];
        assert_eq!(resolve_write_base(&many, Some("  finance ")).unwrap().0, "kb1");
    }

    #[test]
    fn resolve_base_errors_are_actionable() {
        assert!(resolve_write_base(&[], None).unwrap_err().contains("nothing to write"));
        let many = vec![("kb1".to_owned(), "Finance".to_owned()), ("kb2".to_owned(), "Ops".to_owned())];
        // Ambiguous (no base arg, >1 base) names the choices.
        let amb = resolve_write_base(&many, None).unwrap_err();
        assert!(amb.contains("Finance") && amb.contains("Ops"), "{amb}");
        // Unknown name lists valid ones.
        let unk = resolve_write_base(&many, Some("Legal")).unwrap_err();
        assert!(unk.contains("Legal") && unk.contains("Finance"), "{unk}");
    }

    // normalize_write_rel_path ------------------------------------------

    #[test]
    fn rel_path_validation() {
        assert_eq!(normalize_write_rel_path("a\\b.md").unwrap(), "a/b.md");
        assert_eq!(normalize_write_rel_path("  notes/x.MD ").unwrap(), "notes/x.MD");
        assert!(normalize_write_rel_path("").is_err());
        assert!(normalize_write_rel_path("/abs.md").is_err());
        assert!(normalize_write_rel_path("../escape.md").is_err());
        assert!(normalize_write_rel_path("a/../b.md").is_err());
        assert!(normalize_write_rel_path("a//b.md").is_err());
        assert!(normalize_write_rel_path("notes.txt").is_err());
    }

    // execute -----------------------------------------------------------

    #[tokio::test]
    async fn write_by_base_and_path_builds_path_target() {
        let (tool, sink) = write_tool(vec![("kb1", "金融知识库")], direct());
        let res = tool.execute(json!({"rel_path": "terms.md", "content": "# 术语\nPER = 市盈率"})).await;
        assert!(!res.is_error, "{res:?}");
        let req = sink.last.lock().unwrap().clone().unwrap();
        match &req.target {
            WriteTarget::Path { kb_id, rel_path } => {
                assert_eq!(kb_id, "kb1");
                assert_eq!(rel_path, "terms.md");
            }
            other => panic!("expected Path target, got {other:?}"),
        }
        assert!(req.content.contains("市盈率"));
        assert!(matches!(req.mode, WriteMode::Direct));
        assert!(!res.content.contains("STAGED"));
    }

    #[tokio::test]
    async fn write_by_handle_builds_handle_target() {
        let (tool, sink) = write_tool(vec![("kb1", "Finance")], staged("conv-7"));
        let res = tool.execute(json!({"handle": "kdoc_xyz", "content": "merged"})).await;
        assert!(!res.is_error, "{res:?}");
        let req = sink.last.lock().unwrap().clone().unwrap();
        assert!(matches!(req.target, WriteTarget::Handle(ref h) if h == "kdoc_xyz"));
        assert!(matches!(req.mode, WriteMode::Staged { ref scope } if scope == "conv-7"));
        assert!(res.content.contains("STAGED"));
    }

    #[tokio::test]
    async fn execute_rejects_missing_and_blank_inputs() {
        let (tool, _sink) = write_tool(vec![("kb1", "Finance")], direct());
        assert!(tool.execute(json!({"content": "x"})).await.is_error, "no handle or rel_path");
        assert!(tool.execute(json!({"rel_path": "a.md"})).await.is_error, "missing content");
        assert!(tool.execute(json!({"handle": "kdoc_x", "content": "   "})).await.is_error, "blank content");
        assert!(tool.execute(json!({"rel_path": "a.txt", "content": "x"})).await.is_error, "non-md rel_path");
    }

    #[tokio::test]
    async fn execute_surfaces_sink_error() {
        let sink = Arc::new(FakeWriteSink { fail: true, ..Default::default() });
        let tool = KnowledgeWriteTool::new(sink, vec![("kb1".into(), "Finance".into())], WriteMode::Direct, vec!["kb1".into()]);
        let res = tool.execute(json!({"handle": "kdoc_x", "content": "x"})).await;
        assert!(res.is_error);
        assert!(res.content.contains("disk full"));
    }

    #[tokio::test]
    async fn execute_multi_base_requires_base_arg_for_create() {
        let (tool, _sink) = write_tool(vec![("kb1", "Finance"), ("kb2", "Ops")], direct());
        let res = tool.execute(json!({"rel_path": "a.md", "content": "x"})).await;
        assert!(res.is_error);
        assert!(res.content.contains("Specify"));
    }
}

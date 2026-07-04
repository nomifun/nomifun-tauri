//! Knowledge-base capabilities (registry form): catalog (list/create), markdown
//! file writes, AI overview autogen, server-side URL fetch, and the per-target
//! knowledge binding. The intricate per-surface write policy is preserved
//! verbatim from the legacy tool. The `channel_write_enabled` field — previously
//! readable at runtime but ABSENT from the MCP schema (a confirmed drift) — is
//! now a declared param, so the single typed struct fixes the drift.

use std::collections::HashSet;
use std::sync::Arc;

use nomifun_api_types::{KnowledgeSource, KnowledgeSourceEntry, KnowledgeSourceMode};
use nomifun_knowledge::source_url::truncate_to_bytes;
use nomifun_knowledge::{
    KnowledgeBinding, UrlFetcher, WriteRequest, WriteSurface, WriteTargetSpec, resolve_write_policy,
};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::deps::{CallerCtx, GatewayDeps};
use crate::registry::{Capability, CapabilityMeta, DangerTier};
use crate::server::ok;

/// Response cap for `nomi_knowledge_fetch_url` markdown bodies.
const FETCH_URL_MAX_BYTES: usize = 64 * 1024;

// ── param structs (single source: schema + runtime) ──────────────────────

#[derive(Deserialize, JsonSchema)]
struct ListBasesParams {
    /// Case-insensitive substring filter over base name/description.
    #[serde(default)]
    query: Option<String>,
}

#[derive(Deserialize, JsonSchema)]
struct CreateBaseParams {
    /// Display name for the new knowledge base.
    name: String,
    /// Optional description (also auto-generated later if URL sources are given).
    #[serde(default)]
    description: Option<String>,
    /// Optional seed URLs to ingest.
    #[serde(default)]
    urls: Option<Vec<String>>,
    /// Ingestion mode for the seed URLs: "snapshot" (default, fetched once) or "live".
    #[serde(default)]
    mode: Option<String>,
}

#[derive(Deserialize, JsonSchema)]
struct WriteFileParams {
    /// Target knowledge base id (from nomi_knowledge_list_bases).
    kb_id: String,
    /// Relative .md path inside the base (no traversal; .md only).
    rel_path: String,
    /// Markdown document content (written verbatim, not trimmed).
    content: String,
}

#[derive(Deserialize, JsonSchema)]
struct AutogenParams {
    /// Knowledge base id to (re)generate the AI overview for.
    kb_id: String,
    /// Overwrite the existing root README too (default false).
    #[serde(default)]
    overwrite_readme: Option<bool>,
}

#[derive(Deserialize, JsonSchema)]
struct FetchUrlParams {
    /// The URL to fetch + convert to markdown (SSRF-guarded; no private/loopback targets).
    url: String,
}

#[derive(Deserialize, JsonSchema)]
struct GetBindingParams {
    /// Target kind: "conversation" | "terminal" | "companion".
    kind: String,
    /// The target id whose binding to read.
    target_id: String,
}

#[derive(Deserialize, JsonSchema)]
struct SetBindingParams {
    /// Target kind: "conversation" | "terminal" | "companion".
    kind: String,
    /// The target id whose binding to set.
    target_id: String,
    /// Enable (true) or disable (false) the binding.
    enabled: bool,
    /// Replacement list of bound base ids (omit to keep the current list).
    #[serde(default)]
    kb_ids: Option<Vec<String>>,
    /// Write-back ("回血") switch (omit to keep).
    #[serde(default)]
    writeback: Option<bool>,
    /// Write-back mode: "staged" | "direct" (omit to keep).
    #[serde(default)]
    writeback_mode: Option<String>,
    /// Write-back disposition: "conservative" | "aggressive" (omit to keep).
    #[serde(default)]
    writeback_eagerness: Option<String>,
    /// Allow write-back from external IM channel sessions (omit to keep).
    #[serde(default)]
    channel_write_enabled: Option<bool>,
}

// ── shared pure helpers (also used by caps_terminal's bind-on-create) ─────

/// Resolve the write surface for a gateway caller.
pub(crate) fn gateway_surface(channel_platform: Option<&str>, companion_id: Option<&str>) -> WriteSurface {
    if channel_platform.is_some() {
        WriteSurface::ExternalChannel
    } else if companion_id.is_some() {
        WriteSurface::Companion
    } else {
        WriteSurface::RegularChat
    }
}

fn first_unknown_id<'a>(requested: &'a [String], known: &HashSet<&str>) -> Option<&'a str> {
    requested.iter().find(|id| !known.contains(id.as_str())).map(String::as_str)
}

fn unknown_kb_error(id: &str) -> Value {
    json!({ "error": format!("unknown knowledge base id '{id}'; call nomi_knowledge_list_bases for valid ids") })
}

/// Reject unknown base ids up front. Shared with `caps_terminal`'s bind-on-create.
pub(crate) async fn ensure_known_kb_ids(deps: &GatewayDeps, ids: &[String]) -> Result<(), Value> {
    if ids.is_empty() {
        return Ok(());
    }
    // ID-existence check only — use the disk-free registry lookup, NOT
    // `list_bases()`, which walks every base's directory tree and would hang
    // binding operations whenever a base is rooted on a slow/offline NAS mount.
    let known_ids = match deps.knowledge_service.list_base_ids().await {
        Ok(ids) => ids,
        Err(e) => return Err(json!({ "error": e.to_string() })),
    };
    let known: HashSet<&str> = known_ids.iter().map(String::as_str).collect();
    match first_unknown_id(ids, &known) {
        Some(bad) => Err(unknown_kb_error(bad)),
        None => Ok(()),
    }
}

fn create_base_note(snapshot_urls: usize) -> String {
    if snapshot_urls == 0 {
        return "base created; it only takes effect on a target after nomi_knowledge_set_binding binds it there"
            .to_owned();
    }
    format!(
        "库已创建；{snapshot_urls} 条 URL 正在后台抓取快照并自动生成梗概，请勿重复创建。\
         稍后可用 nomi_knowledge_list_bases 查看（description 出现即完成）；\
         若较长时间后 description 仍未出现，说明抓取可能失败——可改用 nomi_knowledge_fetch_url \
         自行抓取内容、nomi_knowledge_write_file 落库，再 nomi_knowledge_autogen 生成梗概。\
         库仍需 nomi_knowledge_set_binding 绑定到目标后才会生效。"
    )
}

/// Parse the optional `urls` + `mode` into a URL [`KnowledgeSource`].
fn parse_url_source(urls: Option<Vec<String>>, mode: Option<&str>) -> Result<Option<KnowledgeSource>, Value> {
    let urls: Vec<String> = urls
        .unwrap_or_default()
        .into_iter()
        .map(|s| s.trim().to_owned())
        .filter(|s| !s.is_empty())
        .collect();
    let mode = match mode {
        None | Some("snapshot") => KnowledgeSourceMode::Snapshot,
        Some("live") => KnowledgeSourceMode::Live,
        Some(other) => {
            return Err(json!({ "error": format!("unknown mode '{other}' (expected snapshot | live)") }));
        }
    };
    if urls.is_empty() {
        return Ok(None);
    }
    Ok(Some(KnowledgeSource {
        kind: "url".into(),
        mode,
        entries: urls
            .into_iter()
            .map(|url| KnowledgeSourceEntry { url, title: None, ..Default::default() })
            .collect(),
        last_fetched_at: None,
        credential_ref: None,
        scope: None,
        sync: None,
    }))
}

async fn fetch_url_with(fetcher: &UrlFetcher, url: &str) -> Value {
    match fetcher.fetch_page(url).await {
        Ok(page) => {
            let capped = page.markdown.len() > FETCH_URL_MAX_BYTES;
            let markdown = truncate_to_bytes(&page.markdown, FETCH_URL_MAX_BYTES);
            ok(json!({
                "url": url,
                "final_url": page.final_url,
                "title": page.title,
                "markdown": markdown,
                "truncated": page.truncated || capped,
            }))
        }
        Err(e) => json!({ "error": e.to_string() }),
    }
}

// ── handlers ──────────────────────────────────────────────────────────────

async fn list_bases(deps: Arc<GatewayDeps>, p: ListBasesParams) -> Value {
    let query = p.query.map(|q| q.to_lowercase());
    match deps.knowledge_service.list_bases().await {
        Ok(bases) => {
            let items: Vec<Value> = bases
                .iter()
                .filter(|b| {
                    query
                        .as_deref()
                        .is_none_or(|q| b.name.to_lowercase().contains(q) || b.description.to_lowercase().contains(q))
                })
                .map(|b| {
                    json!({
                        "id": b.id,
                        "name": b.name,
                        "description": b.description,
                        "file_count": b.file_count,
                        "root_exists": b.root_exists,
                    })
                })
                .collect();
            ok(json!({ "total": items.len(), "bases": items }))
        }
        Err(e) => json!({ "error": e.to_string() }),
    }
}

async fn create_base(deps: Arc<GatewayDeps>, p: CreateBaseParams) -> Value {
    let name = p.name.trim().to_owned();
    if name.is_empty() {
        return json!({ "error": "missing required field: name" });
    }
    let description = p.description.unwrap_or_default();
    let source = match parse_url_source(p.urls, p.mode.as_deref()) {
        Ok(s) => s,
        Err(e) => return e,
    };
    let snapshot_urls = source
        .as_ref()
        .filter(|s| s.mode == KnowledgeSourceMode::Snapshot)
        .map(|s| s.entries.len())
        .unwrap_or(0);
    // `create_base_with_background_fetch` spawns a background fetch task and so
    // consumes an owned `Arc<Self>` — clone the service handle for it.
    match deps
        .knowledge_service
        .clone()
        .create_base_with_background_fetch(&name, &description, None, source)
        .await
    {
        Ok(info) => ok(json!({
            "id": info.id,
            "name": info.name,
            "description": info.description,
            "file_count": info.file_count,
            "note": create_base_note(snapshot_urls),
        })),
        Err(e) => json!({ "error": e.to_string() }),
    }
}

async fn write_file(deps: Arc<GatewayDeps>, ctx: CallerCtx, p: WriteFileParams) -> Value {
    let surface = gateway_surface(ctx.channel_platform.as_deref(), ctx.companion_id.as_deref());
    let (scope, binding) = match (surface, ctx.companion_id.as_deref()) {
        (WriteSurface::Companion, Some(cid)) => (
            cid.to_owned(),
            deps.knowledge_service.get_binding("companion", cid).await.unwrap_or_default(),
        ),
        _ => (
            ctx.conversation_id.clone(),
            KnowledgeBinding { enabled: true, writeback: true, ..Default::default() },
        ),
    };
    let policy = resolve_write_policy(surface, &binding, &scope);
    let bound_kb_ids = deps.knowledge_service.resolve_kb_ids_for_cwd("").await;
    let req = WriteRequest {
        spec: WriteTargetSpec::Path { kb_id: p.kb_id, rel_path: p.rel_path },
        content: p.content,
        policy,
        bound_kb_ids,
    };
    match deps.knowledge_service.write_document(req).await {
        Ok(out) => ok(json!({
            "kb_id": out.kb_id,
            "rel_path": out.final_rel_path,
            "staged": out.staged,
            "updated": matches!(out.op, nomifun_knowledge::WriteOp::Update),
            "note": "written via the unified write path (placement enforced by session policy); after substantial additions refresh the overview via nomi_knowledge_autogen",
        })),
        Err(e) => json!({ "error": e.to_string() }),
    }
}

async fn autogen(deps: Arc<GatewayDeps>, p: AutogenParams) -> Value {
    match deps
        .knowledge_service
        .generate_overview(&p.kb_id, p.overwrite_readme.unwrap_or(false), None)
        .await
    {
        Ok(outcome) => ok(outcome),
        Err(e) => json!({ "error": e.to_string() }),
    }
}

async fn fetch_url(_deps: Arc<GatewayDeps>, p: FetchUrlParams) -> Value {
    fetch_url_with(&UrlFetcher::default(), &p.url).await
}

async fn get_binding(deps: Arc<GatewayDeps>, p: GetBindingParams) -> Value {
    match deps.knowledge_service.get_binding(&p.kind, &p.target_id).await {
        Ok(binding) => ok(binding),
        Err(e) => json!({ "error": e.to_string() }),
    }
}

async fn set_binding(deps: Arc<GatewayDeps>, p: SetBindingParams) -> Value {
    let mut binding = match deps.knowledge_service.get_binding(&p.kind, &p.target_id).await {
        Ok(b) => b,
        Err(e) => return json!({ "error": e.to_string() }),
    };
    binding.enabled = p.enabled;
    if let Some(ids) = p.kb_ids {
        binding.kb_ids = ids;
    }
    if let Some(wb) = p.writeback {
        binding.writeback = wb;
    }
    if let Some(mode) = p.writeback_mode {
        binding.writeback_mode = mode;
    }
    if let Some(eagerness) = p.writeback_eagerness {
        binding.writeback_eagerness = eagerness;
    }
    if let Some(channel_write) = p.channel_write_enabled {
        binding.channel_write_enabled = channel_write;
    }
    if p.enabled && binding.kb_ids.is_empty() {
        return json!({ "error": "kb_ids must not be empty when enabling a binding; call nomi_knowledge_list_bases for valid ids" });
    }
    if let Err(e) = ensure_known_kb_ids(&deps, &binding.kb_ids).await {
        return e;
    }
    match deps.knowledge_service.set_binding(&p.kind, &p.target_id, binding).await {
        Ok(binding) => ok(json!({
            "binding": binding,
            "note": "binding saved; bases are mounted into the target's workspace at its NEXT task start"
        })),
        Err(e) => json!({ "error": e.to_string() }),
    }
}

pub(crate) fn register(out: &mut Vec<Capability>) {
    out.push(Capability::new::<ListBasesParams, _, _>(
        CapabilityMeta::new("nomi_knowledge_list_bases", "knowledge", "List knowledge bases (optionally filtered).", DangerTier::Read),
        |deps, _ctx, p| list_bases(deps, p),
    ));
    out.push(Capability::new::<CreateBaseParams, _, _>(
        CapabilityMeta::new("nomi_knowledge_create_base", "knowledge", "Create a new managed knowledge base, optionally seeded with URL sources (fetched in the background).", DangerTier::Write),
        |deps, _ctx, p| create_base(deps, p),
    ));
    out.push(Capability::new::<WriteFileParams, _, _>(
        CapabilityMeta::new("nomi_knowledge_write_file", "knowledge", "Create/update one markdown document in a base (placement enforced by per-surface policy).", DangerTier::Write),
        write_file,
    ));
    out.push(Capability::new::<AutogenParams, _, _>(
        CapabilityMeta::new("nomi_knowledge_autogen", "knowledge", "Generate the AI overview (description + root README) for a base.", DangerTier::Write),
        |deps, _ctx, p| autogen(deps, p),
    ));
    out.push(Capability::new::<FetchUrlParams, _, _>(
        CapabilityMeta::new("nomi_knowledge_fetch_url", "knowledge", "Server-side fetch + HTML→markdown of a URL (SSRF-guarded).", DangerTier::Read),
        |deps, _ctx, p| fetch_url(deps, p),
    ));
    out.push(Capability::new::<GetBindingParams, _, _>(
        CapabilityMeta::new("nomi_knowledge_get_binding", "knowledge", "Read the knowledge binding for one target (conversation/terminal/companion).", DangerTier::Read),
        |deps, _ctx, p| get_binding(deps, p),
    ));
    out.push(Capability::new::<SetBindingParams, _, _>(
        CapabilityMeta::new("nomi_knowledge_set_binding", "knowledge", "Set the bound base list / toggle a target's knowledge binding and write-back knobs.", DangerTier::Write),
        |deps, _ctx, p| set_binding(deps, p),
    ));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gateway_surface_from_ctx() {
        assert!(matches!(gateway_surface(Some("lark"), Some("c1")), WriteSurface::ExternalChannel));
        assert!(matches!(gateway_surface(None, Some("c1")), WriteSurface::Companion));
        assert!(matches!(gateway_surface(None, None), WriteSurface::RegularChat));
    }

    #[test]
    fn url_source_defaults_to_snapshot_and_filters_blank_urls() {
        let src = parse_url_source(Some(vec!["https://e.com/a".into(), "  ".into(), "https://e.com/b ".into()]), None)
            .unwrap()
            .expect("non-empty urls must yield a source");
        assert_eq!(src.kind, "url");
        assert_eq!(src.mode, KnowledgeSourceMode::Snapshot);
        let urls: Vec<&str> = src.entries.iter().map(|e| e.url.as_str()).collect();
        assert_eq!(urls, vec!["https://e.com/a", "https://e.com/b"]);
    }

    #[test]
    fn url_source_live_mode_and_unknown_mode() {
        let src = parse_url_source(Some(vec!["https://e.com".into()]), Some("live")).unwrap().unwrap();
        assert_eq!(src.mode, KnowledgeSourceMode::Live);
        let err = parse_url_source(Some(vec!["https://e.com".into()]), Some("weekly")).unwrap_err();
        assert!(err["error"].as_str().unwrap().contains("weekly"));
    }

    #[test]
    fn url_source_absent_or_empty_urls_is_none() {
        assert!(parse_url_source(None, None).unwrap().is_none());
        assert!(parse_url_source(Some(vec![]), None).unwrap().is_none());
        assert!(parse_url_source(Some(vec!["  ".into()]), None).unwrap().is_none());
    }

    #[test]
    fn first_unknown_id_finds_in_request_order() {
        let known: HashSet<&str> = ["kb_a", "kb_b"].into();
        assert_eq!(first_unknown_id(&["kb_a".into(), "kb_b".into()], &known), None);
        assert_eq!(first_unknown_id(&["kb_a".into(), "kb_x".into()], &known), Some("kb_x"));
    }

    #[test]
    fn create_base_note_signals_background_fetch() {
        let note = create_base_note(3);
        assert!(note.contains("3 条 URL") && note.contains("后台") && note.contains("请勿重复创建"));
        let plain = create_base_note(0);
        assert!(plain.contains("nomi_knowledge_set_binding") && !plain.contains("后台"));
    }
}

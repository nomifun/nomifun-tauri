//! Extended knowledge-base capabilities (registry form): base detail / update /
//! delete, file listing / read / delete, inbox review (list / merge / discard),
//! full-text search, and user tag CRUD. Supplements `caps_knowledge.rs` which
//! owns the core catalog + binding + write + autogen + fetch-url tools.

use std::sync::Arc;

use nomifun_common::KnowledgeBaseId;
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::deps::GatewayDeps;
use crate::registry::{Capability, CapabilityMeta, DangerTier, Surface};
use crate::server::ok;

/// Hard cap on `read_file` content returned to the model (256 KiB). Larger
/// documents are truncated with a trailing note so the agent knows it is
/// incomplete and can refine its query.
const READ_FILE_MAX_BYTES: usize = 256 * 1024;

// ── param structs ────────────────────────────────────────────────────────────

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct GetBaseParams {
    /// Knowledge base id (from nomi_knowledge_list_bases).
    #[schemars(schema_with = "crate::id_schema::canonical_uuid_v7_schema")]
    kb_id: KnowledgeBaseId,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct UpdateBaseParams {
    /// Knowledge base id to update.
    #[schemars(schema_with = "crate::id_schema::canonical_uuid_v7_schema")]
    kb_id: KnowledgeBaseId,
    /// New display name (omit to keep).
    #[serde(default)]
    name: Option<String>,
    /// New description (omit to keep; empty string clears it).
    #[serde(default)]
    description: Option<String>,
    /// Replacement tag key list (omit to keep; empty array clears all tags).
    #[serde(default)]
    tags: Option<Vec<String>>,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct DeleteBaseParams {
    /// Knowledge base id to delete.
    #[schemars(schema_with = "crate::id_schema::canonical_uuid_v7_schema")]
    kb_id: KnowledgeBaseId,
    /// Also remove the managed directory on disk (default false; only allowed
    /// for managed bases).
    #[serde(default)]
    purge: Option<bool>,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct ListFilesParams {
    /// Knowledge base id whose files to list.
    #[schemars(schema_with = "crate::id_schema::canonical_uuid_v7_schema")]
    kb_id: KnowledgeBaseId,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct ReadFileParams {
    /// Knowledge base id.
    #[schemars(schema_with = "crate::id_schema::canonical_uuid_v7_schema")]
    kb_id: KnowledgeBaseId,
    /// Relative .md path inside the base (forward slashes, no traversal).
    rel_path: String,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct DeleteFileParams {
    /// Knowledge base id.
    #[schemars(schema_with = "crate::id_schema::canonical_uuid_v7_schema")]
    kb_id: KnowledgeBaseId,
    /// Relative .md path of the file to delete (forward slashes, no traversal).
    rel_path: String,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct ListInboxParams {
    /// Knowledge base id whose staged inbox to list.
    #[schemars(schema_with = "crate::id_schema::canonical_uuid_v7_schema")]
    kb_id: KnowledgeBaseId,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct MergeInboxParams {
    /// Knowledge base id.
    #[schemars(schema_with = "crate::id_schema::canonical_uuid_v7_schema")]
    kb_id: KnowledgeBaseId,
    /// Scope (session id that staged the proposal).
    scope: String,
    /// Relative .md path of the staged proposal (mirrors the target base path).
    rel_path: String,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct DiscardInboxParams {
    /// Knowledge base id.
    #[schemars(schema_with = "crate::id_schema::canonical_uuid_v7_schema")]
    kb_id: KnowledgeBaseId,
    /// Scope (session id that staged the proposal).
    scope: String,
    /// Relative .md path of the staged proposal.
    rel_path: String,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct SearchParams {
    /// Knowledge base ids to search (at least one required).
    #[schemars(schema_with = "crate::id_schema::canonical_uuid_v7_array_schema")]
    kb_ids: Vec<KnowledgeBaseId>,
    /// Free-text query (matched against file paths, headings, and content).
    query: String,
    /// Maximum results to return (default 20, clamped to 1..=100).
    #[serde(default)]
    limit: Option<usize>,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct ListTagsParams {
    // No parameters — lists all tags.
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct CreateTagParams {
    /// Human-readable label for the tag (the key is auto-derived from it).
    label: String,
    /// Optional color string (hex or named; omit for no color).
    #[serde(default)]
    color: Option<String>,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct DeleteTagParams {
    /// Tag key to delete (from nomi_knowledge_list_tags).
    key: String,
}

// ── handlers ─────────────────────────────────────────────────────────────────

async fn get_base(deps: Arc<GatewayDeps>, p: GetBaseParams) -> Value {
    match deps.knowledge_service.get_base_info(p.kb_id.as_str()).await {
        Ok(info) => ok(info),
        Err(e) => json!({"error": e.to_string()}),
    }
}

async fn update_base(deps: Arc<GatewayDeps>, p: UpdateBaseParams) -> Value {
    if p.name.is_none() && p.description.is_none() && p.tags.is_none() {
        return json!({"error": "nothing to update: provide at least one of name / description / tags"});
    }
    match deps
        .knowledge_service
        .update_base(p.kb_id.as_str(), p.name.as_deref(), p.description.as_deref(), p.tags)
        .await
    {
        Ok(info) => ok(json!({
            "knowledge_base_id": info.knowledge_base_id,
            "name": info.name,
            "description": info.description,
            "tags": info.tags,
            "file_count": info.file_count,
        })),
        Err(e) => json!({"error": e.to_string()}),
    }
}

async fn delete_base(deps: Arc<GatewayDeps>, p: DeleteBaseParams) -> Value {
    let purge = p.purge.unwrap_or(false);
    match deps.knowledge_service.delete_base(p.kb_id.as_str(), purge).await {
        Ok(()) => ok(json!({
            "deleted": p.kb_id,
            "purged": purge,
        })),
        Err(e) => json!({"error": e.to_string()}),
    }
}

async fn list_files(deps: Arc<GatewayDeps>, p: ListFilesParams) -> Value {
    match deps.knowledge_service.list_files(p.kb_id.as_str()).await {
        Ok(files) => ok(json!({
            "kb_id": p.kb_id,
            "total": files.len(),
            "files": files,
        })),
        Err(e) => json!({"error": e.to_string()}),
    }
}

async fn read_file(deps: Arc<GatewayDeps>, p: ReadFileParams) -> Value {
    match deps.knowledge_service.read_file(p.kb_id.as_str(), &p.rel_path).await {
        Ok(file) => {
            let truncated = file.content.len() > READ_FILE_MAX_BYTES;
            let content = if truncated {
                let bytes = &file.content.as_bytes()[..READ_FILE_MAX_BYTES];
                let boundary = bytes.iter().rposition(|&b| b == b'\n').unwrap_or(READ_FILE_MAX_BYTES);
                let slice = &file.content[..boundary];
                format!("{slice}\n\n[…truncated — {total} bytes total; narrow your query or read in sections]", total = file.content.len())
            } else {
                file.content
            };
            ok(json!({
                "kb_id": p.kb_id,
                "rel_path": file.rel_path,
                "content": content,
                "size": file.size,
                "truncated": truncated,
            }))
        }
        Err(e) => json!({"error": e.to_string()}),
    }
}

async fn delete_file(deps: Arc<GatewayDeps>, p: DeleteFileParams) -> Value {
    match deps.knowledge_service.delete_file(p.kb_id.as_str(), &p.rel_path).await {
        Ok(()) => ok(json!({
            "deleted": format!("{}/{}", p.kb_id, p.rel_path),
        })),
        Err(e) => json!({"error": e.to_string()}),
    }
}

async fn list_inbox(deps: Arc<GatewayDeps>, p: ListInboxParams) -> Value {
    match deps.knowledge_service.list_inbox(p.kb_id.as_str()).await {
        Ok(entries) => ok(json!({
            "kb_id": p.kb_id,
            "total": entries.len(),
            "entries": entries,
        })),
        Err(e) => json!({"error": e.to_string()}),
    }
}

async fn merge_inbox(deps: Arc<GatewayDeps>, p: MergeInboxParams) -> Value {
    match deps
        .knowledge_service
        .merge_inbox(p.kb_id.as_str(), &p.scope, &p.rel_path)
        .await
    {
        Ok(result) => ok(json!({
            "merged_path": result.merged_path,
            "note": "inbox proposal accepted and merged into the base body",
        })),
        Err(e) => json!({"error": e.to_string()}),
    }
}

async fn discard_inbox(deps: Arc<GatewayDeps>, p: DiscardInboxParams) -> Value {
    match deps
        .knowledge_service
        .discard_inbox(p.kb_id.as_str(), &p.scope, &p.rel_path)
        .await
    {
        Ok(()) => ok(json!({
            "discarded": format!("{}/{}/{}", p.kb_id, p.scope, p.rel_path),
            "note": "inbox proposal rejected and removed",
        })),
        Err(e) => json!({"error": e.to_string()}),
    }
}

async fn search(deps: Arc<GatewayDeps>, p: SearchParams) -> Value {
    if p.kb_ids.is_empty() {
        return json!({"error": "kb_ids must not be empty"});
    }
    let query = p.query.trim();
    if query.is_empty() {
        return json!({"error": "query must not be empty"});
    }
    let limit = p.limit.unwrap_or(20).clamp(1, 100);
    match deps.knowledge_service.search_bases(&p.kb_ids, query, limit).await {
        Ok(hits) => ok(json!({
            "total": hits.len(),
            "hits": hits,
        })),
        Err(e) => json!({"error": e.to_string()}),
    }
}

async fn list_tags(deps: Arc<GatewayDeps>, _p: ListTagsParams) -> Value {
    match deps.knowledge_service.list_tags().await {
        Ok(tags) => ok(json!({
            "total": tags.len(),
            "tags": tags,
        })),
        Err(e) => json!({"error": e.to_string()}),
    }
}

async fn create_tag(deps: Arc<GatewayDeps>, p: CreateTagParams) -> Value {
    let label = p.label.trim();
    if label.is_empty() {
        return json!({"error": "label must not be empty"});
    }
    match deps.knowledge_service.create_tag(label, p.color).await {
        Ok(tag) => ok(tag),
        Err(e) => json!({"error": e.to_string()}),
    }
}

async fn delete_tag(deps: Arc<GatewayDeps>, p: DeleteTagParams) -> Value {
    match deps.knowledge_service.delete_tag(&p.key).await {
        Ok(()) => ok(json!({
            "deleted": p.key,
            "note": "tag removed from all bases that referenced it",
        })),
        Err(e) => json!({"error": e.to_string()}),
    }
}

// ── registration ─────────────────────────────────────────────────────────────

pub(crate) fn register(out: &mut Vec<Capability>) {
    // ── base detail / mutation ────────────────────────────────────────────
    out.push(Capability::new::<GetBaseParams, _, _>(
        CapabilityMeta::new(
            "nomi_knowledge_get_base",
            "knowledge",
            "Get full detail of one knowledge base (id, name, description, source, tags, file count, etc.).",
            DangerTier::Read,
        ),
        |deps, _ctx, p| get_base(deps, p),
    ));
    out.push(Capability::new::<UpdateBaseParams, _, _>(
        CapabilityMeta::new(
            "nomi_knowledge_update_base",
            "knowledge",
            "Update a knowledge base's name, description, or assigned tags.",
            DangerTier::Write,
        ),
        |deps, _ctx, p| update_base(deps, p),
    ));
    out.push(Capability::new::<DeleteBaseParams, _, _>(
        CapabilityMeta::new(
            "nomi_knowledge_delete_base",
            "knowledge",
            "Delete a knowledge base registration (optionally purge its managed directory).",
            DangerTier::Destructive,
        )
        .deny_on(&[Surface::Channel]),
        |deps, _ctx, p| delete_base(deps, p),
    ));

    // ── file access ──────────────────────────────────────────────────────
    out.push(Capability::new::<ListFilesParams, _, _>(
        CapabilityMeta::new(
            "nomi_knowledge_list_files",
            "knowledge",
            "List all markdown files in a knowledge base (paths, sizes, modification times).",
            DangerTier::Read,
        ),
        |deps, _ctx, p| list_files(deps, p),
    ));
    out.push(Capability::new::<ReadFileParams, _, _>(
        CapabilityMeta::new(
            "nomi_knowledge_read_file",
            "knowledge",
            "Read one markdown document from a knowledge base (truncated at 256 KiB).",
            DangerTier::Read,
        ),
        |deps, _ctx, p| read_file(deps, p),
    ));
    out.push(Capability::new::<DeleteFileParams, _, _>(
        CapabilityMeta::new(
            "nomi_knowledge_delete_file",
            "knowledge",
            "Delete one markdown file from a knowledge base.",
            DangerTier::Destructive,
        )
        .deny_on(&[Surface::Channel]),
        |deps, _ctx, p| delete_file(deps, p),
    ));

    // ── inbox (staged write-back review) ─────────────────────────────────
    out.push(Capability::new::<ListInboxParams, _, _>(
        CapabilityMeta::new(
            "nomi_knowledge_list_inbox",
            "knowledge",
            "List pending staged write-back proposals (inbox) for a knowledge base.",
            DangerTier::Read,
        ),
        |deps, _ctx, p| list_inbox(deps, p),
    ));
    out.push(Capability::new::<MergeInboxParams, _, _>(
        CapabilityMeta::new(
            "nomi_knowledge_merge_inbox",
            "knowledge",
            "Accept a staged inbox proposal: merge it into the base body and remove the staged copy.",
            DangerTier::Write,
        ),
        |deps, _ctx, p| merge_inbox(deps, p),
    ));
    out.push(Capability::new::<DiscardInboxParams, _, _>(
        CapabilityMeta::new(
            "nomi_knowledge_discard_inbox",
            "knowledge",
            "Reject and remove a staged inbox proposal without merging.",
            DangerTier::Write,
        ),
        |deps, _ctx, p| discard_inbox(deps, p),
    ));

    // ── search ───────────────────────────────────────────────────────────
    out.push(Capability::new::<SearchParams, _, _>(
        CapabilityMeta::new(
            "nomi_knowledge_search",
            "knowledge",
            "Full-text search across one or more knowledge bases (ranked by relevance).",
            DangerTier::Read,
        ),
        |deps, _ctx, p| search(deps, p),
    ));

    // ── user tags ────────────────────────────────────────────────────────
    out.push(Capability::new::<ListTagsParams, _, _>(
        CapabilityMeta::new(
            "nomi_knowledge_list_tags",
            "knowledge",
            "List all user-defined knowledge tags (key, label, color, sort order).",
            DangerTier::Read,
        ),
        |deps, _ctx, p| list_tags(deps, p),
    ));
    out.push(Capability::new::<CreateTagParams, _, _>(
        CapabilityMeta::new(
            "nomi_knowledge_create_tag",
            "knowledge",
            "Create a new user-defined tag for categorizing knowledge bases.",
            DangerTier::Write,
        ),
        |deps, _ctx, p| create_tag(deps, p),
    ));
    out.push(Capability::new::<DeleteTagParams, _, _>(
        CapabilityMeta::new(
            "nomi_knowledge_delete_tag",
            "knowledge",
            "Delete a user tag (also strips it from all bases that reference it).",
            DangerTier::Destructive,
        )
        .deny_on(&[Surface::Channel]),
        |deps, _ctx, p| delete_tag(deps, p),
    ));
}

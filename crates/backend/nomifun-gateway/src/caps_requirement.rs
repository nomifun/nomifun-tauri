//! Requirement-domain capabilities (registry form), backed by the requirement
//! service — the desktop's task/requirement board driving AutoWork scheduling.
//!
//! Migrated from `tools_requirement.rs` onto the capability registry: the
//! `*Params` structs are now the single source (schema + runtime deserialization),
//! eliminating hand-parsing via `require_str`/`opt_str`/`require_i64`.

use std::sync::Arc;

use nomifun_api_types::{
    CreateRequirementRequest, ListRequirementsQuery, RequirementStatus,
    UpdateRequirementRequest,
};
use nomifun_common::RequirementId;
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::deps::GatewayDeps;
use crate::registry::{Capability, CapabilityMeta, DangerTier};
use crate::server::ok;

/// How many requirements the duplicate guard inspects (same tag, newest pages
/// first would be ideal; the repo orders deterministically and tags rarely
/// exceed this).
const DEDUP_SCAN_PAGE_SIZE: u32 = 200;

// --- Params structs --------------------------------------------------------

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct RequirementListParams {
    /// Filter by tag (the AutoWork grouping/scheduling dimension).
    #[serde(default)]
    tag: Option<String>,
    /// Filter by status: pending / in_progress / done / failed / cancelled / needs_review.
    #[serde(default)]
    status: Option<String>,
    /// Full-text filter over title/content.
    #[serde(default)]
    query: Option<String>,
    /// Page number (default 1).
    #[serde(default)]
    page: Option<u32>,
    /// Page size (default 20, max 200).
    #[serde(default)]
    page_size: Option<u32>,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct RequirementCreateParams {
    /// Requirement title.
    title: String,
    /// Detailed requirement content (markdown).
    #[serde(default)]
    content: Option<String>,
    /// Tag — the grouping AutoWork schedules by.
    tag: String,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct RequirementUpdateParams {
    /// The stable business id of the requirement to update (from nomi_requirement_list).
    #[schemars(schema_with = "crate::id_schema::canonical_uuid_v7_schema")]
    requirement_id: RequirementId,
    /// New title (omit to keep).
    #[serde(default)]
    title: Option<String>,
    /// New content (omit to keep).
    #[serde(default)]
    content: Option<String>,
    /// New tag (omit to keep).
    #[serde(default)]
    tag: Option<String>,
    /// New status: pending / in_progress / done / failed / cancelled.
    #[serde(default)]
    status: Option<String>,
    /// Note recorded with a status change (recommended for done/failed).
    #[serde(default)]
    completion_note: Option<String>,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct RequirementDeleteParams {
    /// The stable business id of the requirement to delete. Confirm the target with the user first.
    #[schemars(schema_with = "crate::id_schema::canonical_uuid_v7_schema")]
    requirement_id: RequirementId,
}

// --- Helpers ---------------------------------------------------------------

/// Parse an optional status string into the typed enum, returning a structured
/// error the LLM can self-correct from.
fn parse_status(raw: Option<&str>) -> Result<Option<RequirementStatus>, Value> {
    match raw {
        None => Ok(None),
        Some(s) => serde_json::from_value::<RequirementStatus>(json!(s))
            .map(Some)
            .map_err(|_| json!({"error": format!("invalid status '{s}'")})),
    }
}

/// Duplicate-create guard: an OPEN requirement (pending / in_progress /
/// needs_review) under the same tag with the same (trimmed,
/// ASCII-case-insensitive) title counts as a duplicate. Done / failed /
/// cancelled requirements never block a re-create.
pub(crate) fn is_open_duplicate(
    status: &RequirementStatus,
    existing_title: &str,
    new_title: &str,
) -> bool {
    matches!(
        status,
        RequirementStatus::Pending | RequirementStatus::InProgress | RequirementStatus::NeedsReview
    ) && existing_title.trim().eq_ignore_ascii_case(new_title.trim())
}

// --- Handlers --------------------------------------------------------------

async fn list(deps: Arc<GatewayDeps>, p: RequirementListParams) -> Value {
    let status = match parse_status(p.status.as_deref()) {
        Ok(s) => s,
        Err(e) => return e,
    };
    let query = ListRequirementsQuery {
        tag: p.tag,
        status,
        conversation_id: None,
        q: p.query,
        order_by: None,
        order: None,
        page: p.page,
        page_size: p.page_size,
    };
    match deps.requirement_service.list(&query).await {
        Ok(result) => ok(result),
        Err(e) => json!({"error": e.to_string()}),
    }
}

/// Creates a requirement after checking the duplicate guard: an open
/// requirement with the same tag + title (trimmed, case-insensitive) returns
/// the existing one instead of creating a twin.
async fn create(deps: Arc<GatewayDeps>, p: RequirementCreateParams) -> Value {
    let title = p.title;
    let tag = p.tag;

    // -- T4 duplicate guard -----------------------------------------------
    let dedup_query = ListRequirementsQuery {
        tag: Some(tag.clone()),
        status: None,
        conversation_id: None,
        q: None,
        order_by: None,
        order: None,
        page: Some(1),
        page_size: Some(DEDUP_SCAN_PAGE_SIZE),
    };
    match deps.requirement_service.list(&dedup_query).await {
        Ok(page) => {
            if let Some(existing) = page
                .items
                .iter()
                .find(|r| is_open_duplicate(&r.status, &r.title, &title))
            {
                return ok(json!({
                    "duplicate": true,
                    "existing_requirement": existing,
                    "note": "an open requirement with this tag + title already exists — nothing was created. Use nomi_requirement_update to refine it; only create a second one if the owner explicitly asked for a duplicate this turn."
                }));
            }
        }
        Err(e) => return json!({"error": e.to_string()}),
    }

    let req = CreateRequirementRequest {
        title,
        content: p.content.unwrap_or_default(),
        tag,
        order_key: None,
        status: None,
        created_by: Some("agent".to_owned()),
        attachments: vec![],
    };
    match deps.requirement_service.create(req).await {
        Ok(requirement) => ok(requirement),
        Err(e) => json!({"error": e.to_string()}),
    }
}

async fn update(deps: Arc<GatewayDeps>, p: RequirementUpdateParams) -> Value {
    let status = match parse_status(p.status.as_deref()) {
        Ok(s) => s,
        Err(e) => return e,
    };
    let req = UpdateRequirementRequest {
        title: p.title,
        content: p.content,
        tag: p.tag,
        order_key: None,
        status,
        completion_note: p.completion_note,
        add_attachments: vec![],
        remove_attachment_ids: vec![],
    };
    match deps
        .requirement_service
        .update(p.requirement_id.as_str(), req)
        .await
    {
        Ok(requirement) => ok(requirement),
        Err(e) => json!({"error": e.to_string()}),
    }
}

async fn delete(deps: Arc<GatewayDeps>, p: RequirementDeleteParams) -> Value {
    match deps
        .requirement_service
        .delete(p.requirement_id.as_str())
        .await
    {
        Ok(()) => json!({"result": format!("requirement {} deleted", p.requirement_id)}),
        Err(e) => json!({"error": e.to_string()}),
    }
}

// --- Registration ----------------------------------------------------------

/// Register the requirement-domain capabilities.
pub(crate) fn register(out: &mut Vec<Capability>) {
    out.push(Capability::new::<RequirementListParams, _, _>(
        CapabilityMeta::new(
            "nomi_requirement_list",
            "requirement",
            "List requirements (the desktop task board). Paginated; filter by tag / status / full-text query.",
            DangerTier::Read,
        ),
        |deps, _ctx, p| list(deps, p),
    ));
    out.push(Capability::new::<RequirementCreateParams, _, _>(
        CapabilityMeta::new(
            "nomi_requirement_create",
            "requirement",
            "Create a new requirement. Refuses to create a duplicate of an open requirement with the same tag + title (returns the existing one instead).",
            DangerTier::Write,
        ),
        |deps, _ctx, p| create(deps, p),
    ));
    out.push(Capability::new::<RequirementUpdateParams, _, _>(
        CapabilityMeta::new(
            "nomi_requirement_update",
            "requirement",
            "Update a requirement's title, content, tag, status, or completion note.",
            DangerTier::Write,
        ),
        |deps, _ctx, p| update(deps, p),
    ));
    out.push(Capability::new::<RequirementDeleteParams, _, _>(
        CapabilityMeta::new(
            "nomi_requirement_delete",
            "requirement",
            "Permanently delete a requirement. Confirm the target with the user first.",
            DangerTier::Destructive,
        ),
        |deps, _ctx, p| delete(deps, p),
    ));
}

// --- Tests -----------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn open_statuses_with_same_title_are_duplicates() {
        for status in [
            RequirementStatus::Pending,
            RequirementStatus::InProgress,
            RequirementStatus::NeedsReview,
        ] {
            assert!(
                is_open_duplicate(&status, "Fix login bug", "  fix login BUG "),
                "{status:?} should block a same-title re-create"
            );
        }
    }

    #[test]
    fn closed_statuses_never_block_recreate() {
        for status in [
            RequirementStatus::Done,
            RequirementStatus::Failed,
            RequirementStatus::Cancelled,
        ] {
            assert!(
                !is_open_duplicate(&status, "Fix login bug", "Fix login bug"),
                "{status:?} must not block a re-create"
            );
        }
    }

    #[test]
    fn different_titles_are_not_duplicates() {
        assert!(!is_open_duplicate(
            &RequirementStatus::Pending,
            "Fix login bug",
            "Fix logout bug"
        ));
    }

    #[test]
    fn requirement_mutation_params_use_named_business_ids() {
        let update: RequirementUpdateParams = serde_json::from_value(json!({
            "requirement_id": REQUIREMENT_ID,
            "status": "done"
        }))
        .unwrap();
        assert_eq!(update.requirement_id.as_str(), REQUIREMENT_ID);

        let delete: RequirementDeleteParams =
            serde_json::from_value(json!({"requirement_id": REQUIREMENT_ID})).unwrap();
        assert_eq!(delete.requirement_id.as_str(), REQUIREMENT_ID);

        assert!(
            serde_json::from_value::<RequirementUpdateParams>(json!({
                "id": REQUIREMENT_ID,
                "status": "done"
            }))
            .is_err()
        );
        assert!(
            serde_json::from_value::<RequirementDeleteParams>(json!({"id": REQUIREMENT_ID}))
                .is_err()
        );
    }

    #[test]
    fn requirement_mutation_tool_schemas_expose_named_business_ids() {
        use crate::registry::{Registry, Surface};

        let specs = Registry::global().tool_specs(Surface::Desktop);
        for name in ["nomi_requirement_update", "nomi_requirement_delete"] {
            let spec = specs.iter().find(|spec| spec.name == name).expect("tool registered");
            let properties = spec
                .input_schema
                .get("properties")
                .and_then(Value::as_object)
                .expect("tool properties");
            assert!(properties.contains_key("requirement_id"), "{name}");
            assert!(!properties.contains_key("id"), "{name}");
        }
    }

    const REQUIREMENT_ID: &str = "0190f5fe-7c00-7a00-8abc-012345678901";
}

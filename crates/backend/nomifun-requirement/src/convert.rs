use nomifun_api_types::{Requirement, RequirementStatus};
use nomifun_db::models::RequirementRow;

/// Map a DB row to the API response object.
pub fn row_to_dto(row: &RequirementRow) -> Requirement {
    Requirement {
        requirement_id: row.requirement_id.clone(),
        display_no: row.display_no,
        title: row.title.clone(),
        content: row.content.clone(),
        tag: row.tag.clone(),
        order_key: row.order_key.clone(),
        status: RequirementStatus::from_db(&row.status),
        completion_note: row.completion_note.clone(),
        owner_conversation_id: row.owner_conversation_id.clone(),
        owner_terminal_id: row.owner_terminal_id.clone(),
        started_at: row.started_at,
        completed_at: row.completed_at,
        attempt_count: row.attempt_count,
        created_by: row.created_by.clone(),
        created_at: row.created_at,
        updated_at: row.updated_at,
        attachments: Vec::new(),
    }
}

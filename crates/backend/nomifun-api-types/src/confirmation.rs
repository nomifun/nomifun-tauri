use nomifun_common::Confirmation;
use serde::{Deserialize, Serialize};

// ── Request types ──────────────────────────────────────────────────

/// Body for `POST /api/conversations/:id/confirmations/:callId/confirm`.
#[derive(Debug, Deserialize)]
pub struct ConfirmRequest {
    pub msg_id: String,
    pub data: serde_json::Value,
    #[serde(default)]
    pub always_allow: bool,
}

// ── Query types ────────────────────────────────────────────────────

/// Query parameters for `GET /api/conversations/:id/approvals/check`.
#[derive(Debug, Deserialize)]
pub struct ApprovalCheckQuery {
    pub action: String,
    #[serde(default)]
    pub command_type: Option<String>,
}

// ── Response types ─────────────────────────────────────────────────

/// Response for the approval check endpoint.
#[derive(Debug, Clone, Serialize)]
pub struct ApprovalCheckResponse {
    pub approved: bool,
}

/// Alias: confirmations list returns `Vec<Confirmation>` directly
/// (the `Confirmation` type from `nomifun-common` already has camelCase serde).
pub type ConfirmationListResponse = Vec<Confirmation>;

#[cfg(test)]
mod tests {
    use super::*;
    use nomifun_common::ConfirmationOption;
    use serde_json::json;

    #[test]
    fn deserialize_confirm_request_full() {
        let raw = json!({
            "msg_id": "msg-001",
            "data": { "label": "Allow", "value": "allow" },
            "always_allow": true
        });
        let req: ConfirmRequest = serde_json::from_value(raw).unwrap();
        assert_eq!(req.msg_id, "msg-001");
        assert!(req.always_allow);
        assert_eq!(req.data["value"], "allow");
    }

    #[test]
    fn deserialize_confirm_request_minimal() {
        let raw = json!({
            "msg_id": "msg-001",
            "data": "allow"
        });
        let req: ConfirmRequest = serde_json::from_value(raw).unwrap();
        assert_eq!(req.msg_id, "msg-001");
        assert!(!req.always_allow);
    }

    #[test]
    fn deserialize_confirm_request_missing_msg_id() {
        let raw = json!({ "data": "allow" });
        assert!(serde_json::from_value::<ConfirmRequest>(raw).is_err());
    }

    #[test]
    fn deserialize_confirm_request_missing_data() {
        let raw = json!({ "msg_id": "msg-001" });
        assert!(serde_json::from_value::<ConfirmRequest>(raw).is_err());
    }

    #[test]
    fn deserialize_approval_check_query() {
        let raw = json!({
            "action": "edit_file",
            "command_type": "bash"
        });
        let q: ApprovalCheckQuery = serde_json::from_value(raw).unwrap();
        assert_eq!(q.action, "edit_file");
        assert_eq!(q.command_type.as_deref(), Some("bash"));
    }

    #[test]
    fn deserialize_approval_check_query_no_command_type() {
        let raw = json!({ "action": "edit_file" });
        let q: ApprovalCheckQuery = serde_json::from_value(raw).unwrap();
        assert_eq!(q.action, "edit_file");
        assert!(q.command_type.is_none());
    }

    #[test]
    fn deserialize_approval_check_query_missing_action() {
        let raw = json!({});
        assert!(serde_json::from_value::<ApprovalCheckQuery>(raw).is_err());
    }

    #[test]
    fn serialize_approval_check_response() {
        let resp = ApprovalCheckResponse { approved: true };
        let json = serde_json::to_value(&resp).unwrap();
        assert_eq!(json["approved"], true);
    }

    #[test]
    fn confirmation_list_response_is_vec() {
        let list: ConfirmationListResponse = vec![Confirmation {
            id: "c1".into(),
            call_id: "call-1".into(),
            title: Some("Test".into()),
            action: Some("edit_file".into()),
            description: "Edit main.rs".into(),
            command_type: None,
            options: vec![ConfirmationOption {
                label: "Allow".into(),
                value: json!("allow"),
                params: None,
            }],
            screenshot: None,
        }];
        let json = serde_json::to_value(&list).unwrap();
        let arr = json.as_array().unwrap();
        assert_eq!(arr.len(), 1);
        assert_eq!(arr[0]["call_id"], "call-1");
        assert_eq!(arr[0]["action"], "edit_file");
    }
}

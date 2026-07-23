use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// A. Extension query responses
// ---------------------------------------------------------------------------

/// Summary of a loaded extension returned by `GET /api/extensions`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ExtensionSummaryResponse {
    pub name: String,
    pub version: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub enabled: bool,
    pub source: String,
}

/// Response for `GET /api/hub/extensions`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct HubExtensionListResponse {
    pub extensions: Vec<HubExtensionListItem>,
}

/// Single item in the Hub extension list.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct HubExtensionListItem {
    pub name: String,
    pub version: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub author: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub icon: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,
    #[serde(default)]
    pub bundled: bool,
    pub status: String,
}

// ---------------------------------------------------------------------------
// B. Permission responses
// ---------------------------------------------------------------------------

/// Response for `POST /api/extensions/permissions`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PermissionSummaryResponse {
    pub permissions: serde_json::Value,
    pub risk_level: String,
    pub details: Vec<PermissionDetailResponse>,
}

/// A single permission entry in the summary response.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PermissionDetailResponse {
    pub permission: String,
    pub level: String,
    pub description: String,
}

// ---------------------------------------------------------------------------
// C. Extension management requests
// ---------------------------------------------------------------------------

/// Request body for `POST /api/hub/install` and `POST /api/hub/update`.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InstallExtensionRequest {
    pub name: String,
}

/// Request body for `POST /api/extensions/enable`.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EnableExtensionRequest {
    pub name: String,
}

/// Request body for `POST /api/extensions/disable`.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DisableExtensionRequest {
    pub name: String,
    #[serde(default)]
    pub reason: Option<String>,
}

/// Request body for `POST /api/extensions/permissions`.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GetPermissionsRequest {
    pub name: String,
}

/// Request body for `POST /api/extensions/risk-level`.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GetRiskLevelRequest {
    pub name: String,
}

/// Request body for `POST /api/extensions/i18n`.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GetI18nRequest {
    pub locale: String,
}

/// Response for Hub install/update/uninstall operations.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct HubOperationResponse {
    pub success: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub msg: Option<String>,
}

/// Hub update info returned by `POST /api/hub/check-updates`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct HubUpdateInfo {
    pub name: String,
    pub current_version: String,
    pub latest_version: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_extension_summary_response_serde() {
        let resp = ExtensionSummaryResponse {
            name: "my-ext".into(),
            version: "1.0.0".into(),
            display_name: Some("My Extension".into()),
            description: None,
            enabled: true,
            source: "env".into(),
        };
        let json = serde_json::to_value(&resp).unwrap();
        assert_eq!(json["name"], "my-ext");
        assert_eq!(json["display_name"], "My Extension");
        assert_eq!(json["enabled"], true);
        assert!(json.get("description").is_none());
    }

    #[test]
    fn test_hub_extension_list_item_serde() {
        let item = HubExtensionListItem {
            name: "cool-ext".into(),
            version: "2.0.0".into(),
            display_name: None,
            description: Some("Cool extension".into()),
            author: Some("Author".into()),
            icon: None,
            tags: vec!["tools".into()],
            bundled: false,
            status: "installed".into(),
        };
        let json = serde_json::to_value(&item).unwrap();
        assert_eq!(json["status"], "installed");
        assert_eq!(json["tags"], json!(["tools"]));
        assert!(json.get("display_name").is_none());
    }

    #[test]
    fn test_install_extension_request_deserialize() {
        let raw = json!({"name": "test-ext"});
        let req: InstallExtensionRequest = serde_json::from_value(raw).unwrap();
        assert_eq!(req.name, "test-ext");
    }

    #[test]
    fn test_enable_extension_request_deserialize() {
        let raw = json!({"name": "my-ext"});
        let req: EnableExtensionRequest = serde_json::from_value(raw).unwrap();
        assert_eq!(req.name, "my-ext");
    }

    #[test]
    fn test_disable_extension_request_with_reason() {
        let raw = json!({"name": "bad-ext", "reason": "Security concern"});
        let req: DisableExtensionRequest = serde_json::from_value(raw).unwrap();
        assert_eq!(req.name, "bad-ext");
        assert_eq!(req.reason.as_deref(), Some("Security concern"));
    }

    #[test]
    fn test_disable_extension_request_without_reason() {
        let raw = json!({"name": "ext"});
        let req: DisableExtensionRequest = serde_json::from_value(raw).unwrap();
        assert!(req.reason.is_none());
    }

    #[test]
    fn test_get_permissions_request() {
        let raw = json!({"name": "my-ext"});
        let req: GetPermissionsRequest = serde_json::from_value(raw).unwrap();
        assert_eq!(req.name, "my-ext");
    }

    #[test]
    fn test_get_i18n_request() {
        let raw = json!({"locale": "zh-CN"});
        let req: GetI18nRequest = serde_json::from_value(raw).unwrap();
        assert_eq!(req.locale, "zh-CN");
    }

    #[test]
    fn test_permission_summary_response_serde() {
        let resp = PermissionSummaryResponse {
            permissions: json!({"storage": true, "events": true}),
            risk_level: "moderate".into(),
            details: vec![PermissionDetailResponse {
                permission: "network".into(),
                level: "limited".into(),
                description: "Access to api.example.com only".into(),
            }],
        };
        let json = serde_json::to_value(&resp).unwrap();
        assert_eq!(json["permissions"]["storage"], true);
        assert_eq!(json["risk_level"], "moderate");
        assert_eq!(json["details"][0]["permission"], "network");
        assert_eq!(json["details"][0]["level"], "limited");
    }

    #[test]
    fn test_hub_operation_response_success() {
        let resp = HubOperationResponse {
            success: true,
            msg: None,
        };
        let json = serde_json::to_value(&resp).unwrap();
        assert_eq!(json["success"], true);
        assert!(json.get("msg").is_none());
    }

    #[test]
    fn test_hub_operation_response_failure() {
        let resp = HubOperationResponse {
            success: false,
            msg: Some("Extension not found".into()),
        };
        let json = serde_json::to_value(&resp).unwrap();
        assert_eq!(json["success"], false);
        assert_eq!(json["msg"], "Extension not found");
    }

    #[test]
    fn test_hub_update_info_serde() {
        let info = HubUpdateInfo {
            name: "ext".into(),
            current_version: "1.0.0".into(),
            latest_version: "2.0.0".into(),
        };
        let json = serde_json::to_value(&info).unwrap();
        assert_eq!(json["current_version"], "1.0.0");
        assert_eq!(json["latest_version"], "2.0.0");
    }
}

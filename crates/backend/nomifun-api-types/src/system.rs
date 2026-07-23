use std::collections::HashMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Response for `GET /api/settings`.
///
/// Returns all backend system settings with their current values.
/// When no settings exist in the database, the service layer returns defaults.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SystemSettingsResponse {
    pub language: String,
    pub notification_enabled: bool,
    pub cron_notification_enabled: bool,
    pub command_queue_enabled: bool,
    pub save_upload_to_workspace: bool,
}

impl Default for SystemSettingsResponse {
    fn default() -> Self {
        Self {
            language: "en-US".to_owned(),
            notification_enabled: true,
            cron_notification_enabled: false,
            command_queue_enabled: false,
            save_upload_to_workspace: false,
        }
    }
}

/// Request body for `PATCH /api/settings`.
///
/// All fields are optional — only the fields present in the request body
/// are updated. Unknown or retired fields are rejected.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct UpdateSettingsRequest {
    pub language: Option<String>,
    pub notification_enabled: Option<bool>,
    pub cron_notification_enabled: Option<bool>,
    pub command_queue_enabled: Option<bool>,
    pub save_upload_to_workspace: Option<bool>,
}

impl UpdateSettingsRequest {
    /// Returns `true` if all fields are `None` (no-op update).
    pub fn is_empty(&self) -> bool {
        self.language.is_none()
            && self.notification_enabled.is_none()
            && self.cron_notification_enabled.is_none()
            && self.command_queue_enabled.is_none()
            && self.save_upload_to_workspace.is_none()
    }
}

/// Response for `GET /api/settings/client`.
///
/// A flat key-value map where values can be any JSON type (string,
/// number, boolean). The service layer deserializes stored JSON strings
/// back to their original types.
pub type ClientPreferencesResponse = HashMap<String, Value>;

/// Request body for `PUT /api/settings/client`.
///
/// A flat key-value map for batch updates. A `null` value means
/// the key should be deleted. Non-null values are persisted as-is.
pub type UpdateClientPreferencesRequest = HashMap<String, Value>;

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // -- SystemSettingsResponse --

    #[test]
    fn test_settings_response_default() {
        let resp = SystemSettingsResponse::default();
        assert_eq!(resp.language, "en-US");
        assert!(resp.notification_enabled);
        assert!(!resp.cron_notification_enabled);
        assert!(!resp.command_queue_enabled);
        assert!(!resp.save_upload_to_workspace);
    }

    #[test]
    fn test_settings_response_serialization_snake_case() {
        let resp = SystemSettingsResponse::default();
        let json = serde_json::to_value(&resp).unwrap();
        assert_eq!(json["language"], "en-US");
        assert_eq!(json["notification_enabled"], true);
        assert_eq!(json["cron_notification_enabled"], false);
        assert_eq!(json["command_queue_enabled"], false);
        assert_eq!(json["save_upload_to_workspace"], false);
        // Verify snake_case, not camelCase
        assert!(json.get("notificationEnabled").is_none());
        assert!(json.get("cronNotificationEnabled").is_none());
    }

    #[test]
    fn test_settings_response_deserialization_snake_case() {
        let raw = json!({
            "language": "zh-CN",
            "notification_enabled": false,
            "cron_notification_enabled": true,
            "command_queue_enabled": true,
            "save_upload_to_workspace": true
        });
        let resp: SystemSettingsResponse = serde_json::from_value(raw).unwrap();
        assert_eq!(resp.language, "zh-CN");
        assert!(!resp.notification_enabled);
        assert!(resp.cron_notification_enabled);
        assert!(resp.command_queue_enabled);
        assert!(resp.save_upload_to_workspace);
    }

    #[test]
    fn test_settings_response_roundtrip() {
        let original = SystemSettingsResponse {
            language: "zh-CN".to_owned(),
            notification_enabled: false,
            cron_notification_enabled: true,
            command_queue_enabled: true,
            save_upload_to_workspace: true,
        };
        let json = serde_json::to_string(&original).unwrap();
        let parsed: SystemSettingsResponse = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, original);
    }

    // -- UpdateSettingsRequest --

    #[test]
    fn test_update_request_partial_fields() {
        let raw = r#"{"language":"zh-CN"}"#;
        let req: UpdateSettingsRequest = serde_json::from_str(raw).unwrap();
        assert_eq!(req.language.as_deref(), Some("zh-CN"));
        assert!(req.notification_enabled.is_none());
        assert!(req.cron_notification_enabled.is_none());
        assert!(req.command_queue_enabled.is_none());
        assert!(req.save_upload_to_workspace.is_none());
    }

    #[test]
    fn test_update_request_empty_body() {
        let raw = r#"{}"#;
        let req: UpdateSettingsRequest = serde_json::from_str(raw).unwrap();
        assert!(req.is_empty());
    }

    #[test]
    fn test_update_request_multiple_fields() {
        let raw = json!({
            "notification_enabled": false,
            "command_queue_enabled": true
        });
        let req: UpdateSettingsRequest = serde_json::from_value(raw).unwrap();
        assert!(req.language.is_none());
        assert_eq!(req.notification_enabled, Some(false));
        assert_eq!(req.command_queue_enabled, Some(true));
        assert!(!req.is_empty());
    }

    #[test]
    fn test_update_request_rejects_camel_case() {
        let raw = r#"{"notificationEnabled":true}"#;
        assert!(serde_json::from_str::<UpdateSettingsRequest>(raw).is_err());
    }

    #[test]
    fn test_update_request_rejects_unknown_field() {
        let raw = r#"{"unknownField":123}"#;
        assert!(serde_json::from_str::<UpdateSettingsRequest>(raw).is_err());
    }

    // -- ClientPreferencesResponse / UpdateClientPreferencesRequest --

    #[test]
    fn test_client_preferences_response_empty() {
        let resp: ClientPreferencesResponse = HashMap::new();
        let json = serde_json::to_value(&resp).unwrap();
        assert_eq!(json, json!({}));
    }

    #[test]
    fn test_client_preferences_response_mixed_types() {
        let mut resp: ClientPreferencesResponse = HashMap::new();
        resp.insert("system.closeToTray".into(), json!(false));
        resp.insert("companion.size".into(), json!(280));
        resp.insert("theme".into(), json!("dark"));
        resp.insert("ui.zoomFactor".into(), json!(1.0));

        let json = serde_json::to_value(&resp).unwrap();
        assert_eq!(json["system.closeToTray"], false);
        assert_eq!(json["companion.size"], 280);
        assert_eq!(json["theme"], "dark");
        assert_eq!(json["ui.zoomFactor"], 1.0);
    }

    #[test]
    fn test_update_client_preferences_with_null_delete() {
        let raw = json!({
            "theme": null,
            "companion.size": 360
        });
        let req: UpdateClientPreferencesRequest = serde_json::from_value(raw).unwrap();
        assert!(req["theme"].is_null());
        assert_eq!(req["companion.size"], 360);
    }
}

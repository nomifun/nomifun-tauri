use nomifun_common::UserId;
use serde::{Deserialize, Serialize};

/// Public user info returned in API responses.
///
/// Contains only the fields safe to expose to clients.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PublicUser {
    pub user_id: UserId,
    pub username: String,
}

/// Login request body for `POST /login`.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LoginRequest {
    pub username: String,
    pub password: String,
}

/// Login success response for `POST /login` and `POST /api/auth/qr-login`.
#[derive(Debug, Serialize, Deserialize)]
pub struct LoginResponse {
    pub success: bool,
    pub message: String,
    pub user: PublicUser,
    pub token: String,
}

impl LoginResponse {
    pub fn new(user: PublicUser, token: String) -> Self {
        Self {
            success: true,
            message: "Login successful".to_owned(),
            user,
            token,
        }
    }
}

/// Change password request body for `POST /api/auth/change-password`.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ChangePasswordRequest {
    pub current_password: String,
    pub new_password: String,
}

/// QR code login request body for `POST /api/auth/qr-login`.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct QrLoginRequest {
    pub qr_token: String,
}

/// Auth status response for `GET /api/auth/status`.
#[derive(Debug, Serialize, Deserialize)]
pub struct AuthStatusResponse {
    pub success: bool,
    pub needs_setup: bool,
    pub user_count: u64,
    pub is_authenticated: bool,
}

/// Refresh token request body for `POST /api/auth/refresh`.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RefreshTokenRequest {
    pub token: String,
}

/// User info response for `GET /api/auth/user`.
#[derive(Debug, Serialize)]
pub struct UserInfoResponse {
    pub success: bool,
    pub user: PublicUser,
}

/// Refresh token response for `POST /api/auth/refresh`.
#[derive(Debug, Serialize)]
pub struct RefreshResponse {
    pub success: bool,
    pub token: String,
}

/// WebSocket token response for `GET /api/ws-token`.
#[derive(Debug, Serialize)]
pub struct WsTokenResponse {
    pub success: bool,
    pub ws_token: String,
    pub expires_in: u64,
}

// ---------------------------------------------------------------------------
// WebUI admin credential endpoints (local-only)
// ---------------------------------------------------------------------------

/// Change password request body for `POST /api/webui/change-password`.
///
/// No current_password field — this endpoint is local-mode only and assumes
/// the caller is the trusted Electron main process.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WebuiChangePasswordRequest {
    pub new_password: String,
}

/// Change username request body for `POST /api/webui/change-username`.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WebuiChangeUsernameRequest {
    pub new_username: String,
}

/// Response for `POST /api/webui/change-username`.
#[derive(Debug, Serialize, Deserialize)]
pub struct WebuiChangeUsernameResponse {
    pub username: String,
}

/// Response for `POST /api/webui/reset-password`.
///
/// Returns the freshly generated plaintext password. This is the only time
/// the caller sees it — subsequent reads hit the bcrypt hash only.
#[derive(Debug, Serialize, Deserialize)]
pub struct WebuiResetPasswordResponse {
    pub new_password: String,
}

/// Response for `POST /api/webui/generate-qr-token`.
///
/// Only the token and expiry are returned. URL assembly (host + port) is the
/// caller's responsibility, since only the Electron main process knows which
/// lanIP/port the WebUI is exposed on.
#[derive(Debug, Serialize, Deserialize)]
pub struct WebuiGenerateQrTokenResponse {
    pub token: String,
    pub expires_at_ms: i64,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_public_user_serialization() {
        let user = PublicUser {
            user_id: UserId::new(),
            username: "admin".into(),
        };
        let json = serde_json::to_value(&user).unwrap();
        assert_eq!(json["user_id"], user.user_id.as_str());
        assert_eq!(json["username"], "admin");
    }

    #[test]
    fn test_login_request_deserialization() {
        let raw = r#"{"username":"admin","password":"secret123"}"#;
        let req: LoginRequest = serde_json::from_str(raw).unwrap();
        assert_eq!(req.username, "admin");
        assert_eq!(req.password, "secret123");
    }

    #[test]
    fn test_login_request_missing_field() {
        let raw = r#"{"username":"admin"}"#;
        let result = serde_json::from_str::<LoginRequest>(raw);
        assert!(result.is_err());
    }

    #[test]
    fn test_login_response_new() {
        let user = PublicUser {
            user_id: UserId::new(),
            username: "admin".into(),
        };
        let resp = LoginResponse::new(user.clone(), "jwt_token".into());
        assert!(resp.success);
        assert_eq!(resp.message, "Login successful");
        assert_eq!(resp.user, user);
        assert_eq!(resp.token, "jwt_token");
    }

    #[test]
    fn test_login_response_serialization() {
        let user_id = UserId::new();
        let resp = LoginResponse::new(
            PublicUser {
                user_id: user_id.clone(),
                username: "admin".into(),
            },
            "eyJhbGciOi".into(),
        );
        let json = serde_json::to_value(&resp).unwrap();
        assert_eq!(json["success"], true);
        assert_eq!(json["message"], "Login successful");
        assert_eq!(json["user"]["user_id"], user_id.as_str());
        assert_eq!(json["user"]["username"], "admin");
        assert_eq!(json["token"], "eyJhbGciOi");
    }

    #[test]
    fn test_change_password_request_snake_case() {
        let raw = r#"{"current_password":"old123","new_password":"new456"}"#;
        let req: ChangePasswordRequest = serde_json::from_str(raw).unwrap();
        assert_eq!(req.current_password, "old123");
        assert_eq!(req.new_password, "new456");
    }

    #[test]
    fn test_change_password_request_camel_case_rejected() {
        let raw = r#"{"currentPassword":"old","newPassword":"new"}"#;
        let result = serde_json::from_str::<ChangePasswordRequest>(raw);
        assert!(result.is_err());
    }

    #[test]
    fn test_qr_login_request_snake_case() {
        let raw = r#"{"qr_token":"abc123"}"#;
        let req: QrLoginRequest = serde_json::from_str(raw).unwrap();
        assert_eq!(req.qr_token, "abc123");
    }

    #[test]
    fn test_qr_login_request_camel_case_rejected() {
        let raw = r#"{"qrToken":"abc"}"#;
        let result = serde_json::from_str::<QrLoginRequest>(raw);
        assert!(result.is_err());
    }

    #[test]
    fn test_auth_status_response_snake_case() {
        let resp = AuthStatusResponse {
            success: true,
            needs_setup: true,
            user_count: 0,
            is_authenticated: false,
        };
        let json = serde_json::to_value(&resp).unwrap();
        assert_eq!(json["success"], true);
        assert_eq!(json["needs_setup"], true);
        assert_eq!(json["user_count"], 0);
        assert_eq!(json["is_authenticated"], false);
        // Verify snake_case keys exist, not camelCase
        assert!(json.get("needsSetup").is_none());
        assert!(json.get("userCount").is_none());
        assert!(json.get("isAuthenticated").is_none());
    }

    #[test]
    fn test_auth_status_response_deserialization() {
        let raw = json!({
            "success": true,
            "needs_setup": false,
            "user_count": 3,
            "is_authenticated": true
        });
        let resp: AuthStatusResponse = serde_json::from_value(raw).unwrap();
        assert!(resp.success);
        assert!(!resp.needs_setup);
        assert_eq!(resp.user_count, 3);
        assert!(resp.is_authenticated);
    }

    #[test]
    fn test_refresh_token_request_deserialization() {
        let raw = r#"{"token":"eyJhbGciOiJIUzI1NiJ9"}"#;
        let req: RefreshTokenRequest = serde_json::from_str(raw).unwrap();
        assert_eq!(req.token, "eyJhbGciOiJIUzI1NiJ9");
    }

    #[test]
    fn test_refresh_token_request_missing_token() {
        let raw = r#"{}"#;
        let result = serde_json::from_str::<RefreshTokenRequest>(raw);
        assert!(result.is_err());
    }

    #[test]
    fn public_user_rejects_noncanonical_user_id() {
        let value = json!({"user_id": "1", "username": "admin"});
        assert!(serde_json::from_value::<PublicUser>(value).is_err());
    }
}

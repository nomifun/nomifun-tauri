//! Black-box tests for auth DTO serialization/deserialization.

use nomifun_api_types::{
    AuthStatusResponse, ChangePasswordRequest, LoginRequest, LoginResponse, PublicUser, QrLoginRequest,
    RefreshTokenRequest,
};
use nomifun_common::UserId;

// --- LoginRequest ---

#[test]
fn login_request_valid_json() {
    let json = r#"{"username":"admin","password":"secret123"}"#;
    let req: LoginRequest = serde_json::from_str(json).unwrap();
    assert_eq!(req.username, "admin");
    assert_eq!(req.password, "secret123");
}

#[test]
fn login_request_missing_password() {
    let json = r#"{"username":"admin"}"#;
    assert!(serde_json::from_str::<LoginRequest>(json).is_err());
}

#[test]
fn login_request_missing_username() {
    let json = r#"{"password":"secret"}"#;
    assert!(serde_json::from_str::<LoginRequest>(json).is_err());
}

#[test]
fn login_request_empty_body() {
    let json = r#"{}"#;
    assert!(serde_json::from_str::<LoginRequest>(json).is_err());
}

#[test]
fn login_request_rejects_extra_fields() {
    let json = r#"{"username":"admin","password":"secret","extra":"ignored"}"#;
    assert!(serde_json::from_str::<LoginRequest>(json).is_err());
}

// --- LoginResponse ---

#[test]
fn login_response_serialization_matches_spec() {
    let user_id = UserId::new();
    let resp = LoginResponse::new(
        PublicUser {
            user_id: user_id.clone(),
            username: "admin".into(),
        },
        "eyJhbGciOiJIUzI1NiJ9".into(),
    );
    let json = serde_json::to_value(&resp).unwrap();

    assert_eq!(json["success"], true);
    assert_eq!(json["message"], "Login successful");
    assert_eq!(json["user"]["user_id"], user_id.as_str());
    assert_eq!(json["user"]["username"], "admin");
    assert_eq!(json["token"], "eyJhbGciOiJIUzI1NiJ9");
}

// --- ChangePasswordRequest ---

#[test]
fn change_password_request_snake_case() {
    let json = r#"{"current_password":"old_pass","new_password":"new_pass_123"}"#;
    let req: ChangePasswordRequest = serde_json::from_str(json).unwrap();
    assert_eq!(req.current_password, "old_pass");
    assert_eq!(req.new_password, "new_pass_123");
}

#[test]
fn change_password_request_rejects_camel_case() {
    let json = r#"{"currentPassword":"old","newPassword":"new"}"#;
    assert!(serde_json::from_str::<ChangePasswordRequest>(json).is_err());
}

#[test]
fn change_password_request_missing_new_password() {
    let json = r#"{"current_password":"old"}"#;
    assert!(serde_json::from_str::<ChangePasswordRequest>(json).is_err());
}

// --- QrLoginRequest ---

#[test]
fn qr_login_request_snake_case() {
    let json = r#"{"qr_token":"token_abc_123"}"#;
    let req: QrLoginRequest = serde_json::from_str(json).unwrap();
    assert_eq!(req.qr_token, "token_abc_123");
}

#[test]
fn qr_login_request_rejects_camel_case() {
    let json = r#"{"qrToken":"abc"}"#;
    assert!(serde_json::from_str::<QrLoginRequest>(json).is_err());
}

#[test]
fn qr_login_request_missing_token() {
    let json = r#"{}"#;
    assert!(serde_json::from_str::<QrLoginRequest>(json).is_err());
}

// --- AuthStatusResponse ---

#[test]
fn auth_status_response_needs_setup() {
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
}

#[test]
fn auth_status_response_authenticated() {
    let resp = AuthStatusResponse {
        success: true,
        needs_setup: false,
        user_count: 2,
        is_authenticated: true,
    };
    let json = serde_json::to_value(&resp).unwrap();

    assert_eq!(json["needs_setup"], false);
    assert_eq!(json["user_count"], 2);
    assert_eq!(json["is_authenticated"], true);
}

#[test]
fn auth_status_response_uses_snake_case_keys() {
    let resp = AuthStatusResponse {
        success: true,
        needs_setup: false,
        user_count: 1,
        is_authenticated: true,
    };
    let json = serde_json::to_value(&resp).unwrap();
    let obj = json.as_object().unwrap();

    assert!(obj.contains_key("needs_setup"));
    assert!(obj.contains_key("user_count"));
    assert!(obj.contains_key("is_authenticated"));
    assert!(!obj.contains_key("needsSetup"));
    assert!(!obj.contains_key("userCount"));
    assert!(!obj.contains_key("isAuthenticated"));
}

#[test]
fn auth_status_response_round_trip() {
    let original = AuthStatusResponse {
        success: true,
        needs_setup: true,
        user_count: 5,
        is_authenticated: false,
    };
    let serialized = serde_json::to_string(&original).unwrap();
    let deserialized: AuthStatusResponse = serde_json::from_str(&serialized).unwrap();

    assert_eq!(deserialized.success, original.success);
    assert_eq!(deserialized.needs_setup, original.needs_setup);
    assert_eq!(deserialized.user_count, original.user_count);
    assert_eq!(deserialized.is_authenticated, original.is_authenticated);
}

// --- RefreshTokenRequest ---

#[test]
fn refresh_token_request_valid() {
    let json = r#"{"token":"eyJhbGciOiJIUzI1NiJ9.test"}"#;
    let req: RefreshTokenRequest = serde_json::from_str(json).unwrap();
    assert_eq!(req.token, "eyJhbGciOiJIUzI1NiJ9.test");
}

#[test]
fn refresh_token_request_missing_token() {
    let json = r#"{}"#;
    assert!(serde_json::from_str::<RefreshTokenRequest>(json).is_err());
}

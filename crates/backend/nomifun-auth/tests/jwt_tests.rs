//! JWT service integration tests.
//!
//! Tests the full lifecycle of JWT operations: sign, verify, blacklist, rotate.

use nomifun_auth::{AuthError, JwtService, resolve_jwt_secret};

const USER_1: &str = "0190f5fe-7c00-7a00-8000-000000000001";
const USER_2: &str = "0190f5fe-7c00-7a00-8000-000000000002";
const USER_3: &str = "0190f5fe-7c00-7a00-8000-000000000003";
const USER_42: &str = "0190f5fe-7c00-7a00-8000-000000000042";

#[test]
fn full_lifecycle_sign_verify_blacklist() {
    let service = JwtService::new("integration_test_secret".into());

    // Sign a token
    let token = service.sign(USER_42, "testuser").unwrap();
    assert!(!token.is_empty());

    // Verify the token
    let payload = service.verify(&token).unwrap();
    assert_eq!(payload.user_id.as_str(), USER_42);
    assert_eq!(payload.username, "testuser");

    // Blacklist the token
    service.blacklist_token(&token);

    // Verification should now fail
    let result = service.verify(&token);
    assert!(matches!(result, Err(AuthError::TokenBlacklisted)));
}

#[test]
fn secret_rotation_invalidates_all_tokens() {
    let service = JwtService::new("original_secret".into());

    let token1 = service.sign(USER_1, "alice").unwrap();
    let token2 = service.sign(USER_2, "bob").unwrap();

    // Both tokens are valid
    assert!(service.verify(&token1).is_ok());
    assert!(service.verify(&token2).is_ok());

    // Rotate the secret
    service.rotate_secret().unwrap();

    // Both old tokens are now invalid
    assert!(service.verify(&token1).is_err());
    assert!(service.verify(&token2).is_err());

    // New tokens with the new secret work
    let new_token = service.sign(USER_3, "charlie").unwrap();
    let payload = service.verify(&new_token).unwrap();
    assert_eq!(payload.user_id.as_str(), USER_3);
}

#[test]
fn resolve_secret_priority_order() {
    // Environment variable takes precedence
    let (secret, generated) = resolve_jwt_secret(Some("env"), Some("db"));
    assert_eq!(secret, "env");
    assert!(!generated);

    // Database value is fallback
    let (secret, generated) = resolve_jwt_secret(None, Some("db"));
    assert_eq!(secret, "db");
    assert!(!generated);

    // Random generation as last resort
    let (secret, generated) = resolve_jwt_secret(None, None);
    assert!(!secret.is_empty());
    assert!(generated);

    // Generated secrets are unique
    let (secret2, _) = resolve_jwt_secret(None, None);
    assert_ne!(secret, secret2);
}

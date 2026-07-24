use nomifun_common::*;

// --- ID generation ---

#[test]
fn test_generate_id_returns_canonical_uuid_v7() {
    let id = generate_id();
    assert_eq!(id.len(), UUID_STRING_LEN);
    assert_eq!(validate_uuidv7(&id).unwrap().to_string(), id);
    assert!(!id.contains('_'));
}

#[test]
fn test_id_is_time_ordered() {
    // UUIDv7 carries its timestamp in the high-order bytes.
    let earlier = generate_id();
    std::thread::sleep(std::time::Duration::from_millis(2));
    let later = generate_id();
    assert!(later > earlier, "{later} should sort after {earlier}");
}

#[test]
fn test_id_uniqueness_across_calls() {
    let ids: std::collections::HashSet<String> = (0..1000).map(|_| generate_id()).collect();
    assert_eq!(ids.len(), 1000);
}

#[test]
fn test_id_time_ordering() {
    let id1 = generate_id();
    let id2 = generate_id();
    assert!(id2 >= id1, "UUID v7 should be time-ordered");
}

#[test]
fn test_typed_id_uses_unprefixed_uuid_v7() {
    let id = ConversationId::new();
    assert_eq!(id.as_str().len(), UUID_STRING_LEN);
    assert_eq!(validate_uuidv7(id.as_str()).unwrap().to_string(), id.as_str());
    assert!(!id.as_str().contains('_'));
}

// --- Timestamp ---

#[test]
fn test_now_ms_returns_positive() {
    assert!(now_ms() > 0);
}

#[test]
fn test_now_ms_monotonic() {
    let t1 = now_ms();
    let t2 = now_ms();
    assert!(t2 >= t1);
}

// --- Crypto ---

#[test]
fn test_encrypt_decrypt_roundtrip() {
    let key = [0xAB_u8; 32];
    let encrypted = encrypt_string("hello world", &key).unwrap();
    let decrypted = decrypt_string(&encrypted, &key).unwrap();
    assert_eq!(decrypted, "hello world");
}

#[test]
fn test_encrypt_decrypt_empty_string() {
    let key = [0xCD_u8; 32];
    let encrypted = encrypt_string("", &key).unwrap();
    let decrypted = decrypt_string(&encrypted, &key).unwrap();
    assert_eq!(decrypted, "");
}

#[test]
fn test_encrypt_decrypt_unicode() {
    let key = [0xEF_u8; 32];
    let encrypted = encrypt_string("你好世界🌍", &key).unwrap();
    let decrypted = decrypt_string(&encrypted, &key).unwrap();
    assert_eq!(decrypted, "你好世界🌍");
}

#[test]
fn test_decrypt_wrong_key_fails() {
    let key = [0x11_u8; 32];
    let encrypted = encrypt_string("secret", &key).unwrap();
    let wrong_key = [0x22_u8; 32];
    assert!(decrypt_string(&encrypted, &wrong_key).is_err());
}

#[test]
fn test_encrypt_same_plaintext_different_ciphertext() {
    let key = [0x33_u8; 32];
    let e1 = encrypt_string("test", &key).unwrap();
    let e2 = encrypt_string("test", &key).unwrap();
    assert_ne!(e1, e2, "random nonce should produce different ciphertexts");
}

#[test]
fn test_encrypt_large_text() {
    let key = [0x44_u8; 32];
    let large = "x".repeat(1_000_000);
    let encrypted = encrypt_string(&large, &key).unwrap();
    let decrypted = decrypt_string(&encrypted, &key).unwrap();
    assert_eq!(decrypted, large);
}

// --- AppError ---

#[test]
fn test_app_error_status_codes() {
    use axum::http::StatusCode;

    assert_eq!(AppError::NotFound("x".into()).status_code(), StatusCode::NOT_FOUND);
    assert_eq!(AppError::BadRequest("x".into()).status_code(), StatusCode::BAD_REQUEST);
    assert_eq!(
        AppError::Unauthorized("x".into()).status_code(),
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(AppError::Forbidden("x".into()).status_code(), StatusCode::FORBIDDEN);
    assert_eq!(AppError::Conflict("x".into()).status_code(), StatusCode::CONFLICT);
    assert_eq!(AppError::RateLimited.status_code(), StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(
        AppError::Internal("x".into()).status_code(),
        StatusCode::INTERNAL_SERVER_ERROR
    );
    assert_eq!(AppError::BadGateway("x".into()).status_code(), StatusCode::BAD_GATEWAY);
    assert_eq!(AppError::Timeout("x".into()).status_code(), StatusCode::BAD_GATEWAY);
}

#[test]
fn test_app_error_json_format() {
    use axum::response::IntoResponse;
    let resp = AppError::NotFound("user 123".into()).into_response();
    assert_eq!(resp.status(), axum::http::StatusCode::NOT_FOUND);
}

// --- PaginatedResult ---

#[test]
fn test_paginated_result_serialize() {
    let result = PaginatedResult {
        items: vec!["a", "b"],
        total: 100,
        has_more: true,
    };
    let json = serde_json::to_value(&result).unwrap();
    assert_eq!(json["has_more"], true);
    assert_eq!(json["total"], 100);
}

#[test]
fn test_paginated_result_empty() {
    let result: PaginatedResult<i32> = PaginatedResult {
        items: vec![],
        total: 0,
        has_more: false,
    };
    let json = serde_json::to_value(&result).unwrap();
    assert_eq!(json["items"], serde_json::json!([]));
}

// --- Enums ---

#[test]
fn test_enum_serde_roundtrip() {
    let roundtrip_cases: Vec<(&str, AgentType)> = vec![
        (r#""acp""#, AgentType::Acp),
        (r#""nanobot""#, AgentType::Nanobot),
        (r#""openclaw-gateway""#, AgentType::OpenclawGateway),
    ];
    for (json_str, expected) in roundtrip_cases {
        let parsed: AgentType = serde_json::from_str(json_str).unwrap();
        assert_eq!(parsed, expected);
        let serialized = serde_json::to_string(&expected).unwrap();
        assert_eq!(serialized, json_str);
    }
}

// --- Constants ---

#[test]
fn test_constants_values() {
    assert_eq!(constants::DEFAULT_PORT, 25808);
    assert_eq!(constants::HEARTBEAT_INTERVAL_MS, 30_000);
    assert_eq!(constants::BODY_LIMIT, 10 * 1024 * 1024);
    assert_eq!(constants::COOKIE_NAME, "nomifun-session");
    assert!(constants::SUPPORTED_IMAGE_EXTENSIONS.contains(&".png"));
}

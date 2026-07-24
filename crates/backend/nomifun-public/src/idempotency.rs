use axum::http::HeaderMap;
use sha2::{Digest, Sha256};

pub(crate) const CONVERSATION_SEND_TOOL: &str =
    "nomi_send_to_conversation";
const IDEMPOTENCY_KEY_HEADER: &str = "idempotency-key";
const MAX_IDEMPOTENCY_KEY_LEN: usize = 128;

fn client_idempotency_key(headers: &HeaderMap) -> Result<&str, &'static str> {
    let mut values = headers.get_all(IDEMPOTENCY_KEY_HEADER).iter();
    let Some(value) = values.next() else {
        return Err("missing Idempotency-Key header");
    };
    if values.next().is_some() {
        return Err("expected exactly one Idempotency-Key header");
    }
    let value = value
        .to_str()
        .map_err(|_| "Idempotency-Key must be visible ASCII")?;
    if value.is_empty()
        || value.len() > MAX_IDEMPOTENCY_KEY_LEN
        || !value.bytes().all(|byte| (0x21..=0x7e).contains(&byte))
    {
        return Err("Idempotency-Key must contain 1..=128 visible ASCII bytes");
    }
    Ok(value)
}

/// Bind a caller-selected replay token to the authenticated companion and
/// capability. The external key is not trusted or globally unique by itself;
/// the resulting bounded token is safe to pass into the conversation receipt
/// boundary, where target owner/conversation and payload are checked again.
pub(crate) fn remote_operation_id(
    headers: &HeaderMap,
    companion_id: &str,
    tool_name: &str,
) -> Result<String, &'static str> {
    fn hash_field(hasher: &mut Sha256, value: &str) {
        hasher.update((value.len() as u64).to_be_bytes());
        hasher.update(value.as_bytes());
    }

    let client_key = client_idempotency_key(headers)?;
    let mut hasher = Sha256::new();
    hasher.update(b"nomifun-remote-tool:v1\0");
    hash_field(&mut hasher, companion_id);
    hash_field(&mut hasher, tool_name);
    hash_field(&mut hasher, client_key);
    Ok(format!("remote-tool-v1-{:x}", hasher.finalize()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;

    #[test]
    fn header_requires_exactly_one_legal_value() {
        assert!(client_idempotency_key(&HeaderMap::new()).is_err());

        let mut duplicate = HeaderMap::new();
        duplicate.append(
            IDEMPOTENCY_KEY_HEADER,
            HeaderValue::from_static("first"),
        );
        duplicate.append(
            IDEMPOTENCY_KEY_HEADER,
            HeaderValue::from_static("second"),
        );
        assert!(client_idempotency_key(&duplicate).is_err());

        for illegal in ["", "contains space"] {
            let mut headers = HeaderMap::new();
            headers.insert(
                IDEMPOTENCY_KEY_HEADER,
                HeaderValue::from_str(illegal).unwrap(),
            );
            assert!(client_idempotency_key(&headers).is_err());
        }

        let mut oversized = HeaderMap::new();
        oversized.insert(
            IDEMPOTENCY_KEY_HEADER,
            HeaderValue::from_str(&"x".repeat(129)).unwrap(),
        );
        assert!(client_idempotency_key(&oversized).is_err());
    }

    #[test]
    fn remote_key_is_stable_and_authenticated_companion_scoped() {
        let mut headers = HeaderMap::new();
        headers.insert(
            IDEMPOTENCY_KEY_HEADER,
            HeaderValue::from_static("caller-retry-7"),
        );
        let first = remote_operation_id(
            &headers,
            "companion-a",
            CONVERSATION_SEND_TOOL,
        )
        .unwrap();
        let retry = remote_operation_id(
            &headers,
            "companion-a",
            CONVERSATION_SEND_TOOL,
        )
        .unwrap();
        let other_companion = remote_operation_id(
            &headers,
            "companion-b",
            CONVERSATION_SEND_TOOL,
        )
        .unwrap();

        assert_eq!(first, retry);
        assert_ne!(first, other_companion);
        assert!(first.len() <= MAX_IDEMPOTENCY_KEY_LEN);
        assert!(
            first
                .bytes()
                .all(|byte| (0x21..=0x7e).contains(&byte))
        );
    }
}

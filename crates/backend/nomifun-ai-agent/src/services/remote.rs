use std::sync::Arc;
use std::time::Duration;

use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64;
use ed25519_dalek::SigningKey;
use nomifun_api_types::{
    CreateRemoteAgentRequest, HandshakeResponse, RemoteAgentListItem, RemoteAgentResponse,
    TestRemoteAgentConnectionRequest, UpdateRemoteAgentRequest,
};
use nomifun_common::{
    AppError, RemoteAgentId, RemoteAgentAuthType, RemoteAgentProtocol, RemoteAgentStatus, decrypt_string, encrypt_string,
};
use nomifun_db::models::RemoteAgentRow;
use nomifun_db::{IRemoteAgentRepository, UpdateRemoteAgentParams};
use sha2::Digest;
use tracing::warn;

use crate::manager::openclaw::connection::{AuthConfig, OpenClawConnection};
use crate::manager::openclaw::device_identity::{
    DeviceIdentity, generate_ephemeral_identity, identity_from_secret_bytes,
};

/// Service layer for Remote Agent CRUD and connection management.
#[derive(Clone)]
pub struct RemoteAgentService {
    repo: Arc<dyn IRemoteAgentRepository>,
    encryption_key: [u8; 32],
}

impl RemoteAgentService {
    pub fn new(repo: Arc<dyn IRemoteAgentRepository>, encryption_key: [u8; 32]) -> Self {
        Self { repo, encryption_key }
    }

    /// List all remote agents (auth_token omitted).
    pub async fn list(&self) -> Result<Vec<RemoteAgentListItem>, AppError> {
        let rows = self.repo.list().await.map_err(db_err)?;
        rows.into_iter().map(|r| self.row_to_list_item(r)).collect()
    }

    /// Get a single remote agent by ID (auth_token masked).
    pub async fn get(&self, id: &RemoteAgentId) -> Result<RemoteAgentResponse, AppError> {
        let row = self
            .repo
            .find_by_id(id)
            .await
            .map_err(db_err)?
            .ok_or_else(|| AppError::NotFound(format!("Remote agent '{id}' not found")))?;
        self.row_to_response(row)
    }

    /// Create a new remote agent. OpenClaw protocol auto-generates Ed25519 keys.
    pub async fn create(&self, req: CreateRemoteAgentRequest) -> Result<RemoteAgentResponse, AppError> {
        validate_create_request(&req)?;

        let encrypted_token = req
            .auth_token
            .as_deref()
            .map(|t| encrypt_string(t, &self.encryption_key))
            .transpose()?;

        let (device_id, device_public_key, device_private_key) = if req.protocol == RemoteAgentProtocol::OpenClaw {
            let (id, pub_key, priv_key) = generate_device_keypair(&self.encryption_key)?;
            (Some(id), Some(pub_key), Some(priv_key))
        } else {
            (None, None, None)
        };

        let row = self
            .repo
            .create(nomifun_db::CreateRemoteAgentParams {
                name: &req.name,
                protocol: &enum_to_str(&req.protocol),
                url: &req.url,
                auth_type: &enum_to_str(&req.auth_type),
                auth_token: encrypted_token.as_deref(),
                allow_insecure: req.allow_insecure,
                avatar: req.avatar.as_deref(),
                description: req.description.as_deref(),
                device_id: device_id.as_deref(),
                device_public_key: device_public_key.as_deref(),
                device_private_key: device_private_key.as_deref(),
                device_token: None,
            })
            .await
            .map_err(db_err)?;

        self.row_to_response(row)
    }

    /// Update an existing remote agent.
    pub async fn update(&self, id: &RemoteAgentId, req: UpdateRemoteAgentRequest) -> Result<RemoteAgentResponse, AppError> {
        let existing = self
            .repo
            .find_by_id(id)
            .await
            .map_err(db_err)?
            .ok_or_else(|| AppError::NotFound(format!("Remote agent '{id}' not found")))?;
        let existing_protocol = parse_protocol(&existing.protocol);
        let effective_protocol = req.protocol.unwrap_or(existing_protocol);
        if effective_protocol != RemoteAgentProtocol::OpenClaw {
            return Err(AppError::BadRequest(
                "Only the OpenClaw remote protocol is currently implemented. Hermes remains available as a local ACP CLI (`hermes acp`).".into(),
            ));
        }
        if existing_protocol != RemoteAgentProtocol::OpenClaw {
            return Err(AppError::BadRequest(
                "Legacy remote-agent rows cannot be converted in place because they have no OpenClaw device identity. Delete this row and create a new OpenClaw remote agent.".into(),
            ));
        }
        let encrypted_token = match &req.auth_token {
            Some(Some(t)) => Some(Some(encrypt_string(t, &self.encryption_key)?)),
            Some(None) => Some(None),
            None => None,
        };

        let effective_name = req.name.as_deref().unwrap_or(&existing.name);
        if effective_name.trim().is_empty() {
            return Err(AppError::BadRequest("name must not be empty".into()));
        }
        let effective_url = req.url.as_deref().unwrap_or(&existing.url);
        validate_ws_url(effective_url)?;
        let effective_auth_type = req
            .auth_type
            .unwrap_or_else(|| parse_auth_type(&existing.auth_type));
        if effective_auth_type != RemoteAgentAuthType::None {
            let has_credential = match &req.auth_token {
                Some(Some(token)) => !token.trim().is_empty(),
                Some(None) => false,
                None => existing.auth_token.is_some(),
            };
            if !has_credential {
                return Err(AppError::BadRequest(
                    "A credential is required for the selected authentication type".into(),
                ));
            }
        }

        let protocol_str = req.protocol.map(|p| enum_to_str(&p));
        let auth_type_str = req.auth_type.map(|a| enum_to_str(&a));

        let params = UpdateRemoteAgentParams {
            name: req.name.as_deref(),
            protocol: protocol_str.as_deref(),
            url: req.url.as_deref(),
            auth_type: auth_type_str.as_deref(),
            auth_token: encrypted_token.as_ref().map(|o| o.as_deref()),
            allow_insecure: req.allow_insecure,
            avatar: req.avatar.as_ref().map(|o| o.as_deref()),
            description: req.description.as_ref().map(|o| o.as_deref()),
        };

        let mut row = self.repo.update(id, params).await.map_err(|e| match e {
            nomifun_db::DbError::NotFound(msg) => AppError::NotFound(msg),
            other => AppError::Internal(other.to_string()),
        })?;

        if remote_connection_identity_changed(&existing, &row) && row.device_token.is_some() {
            self.repo.update_device_token(id, None).await.map_err(db_err)?;
            row.device_token = None;
        }

        self.row_to_response(row)
    }

    /// Delete a remote agent.
    pub async fn delete(&self, id: &RemoteAgentId) -> Result<(), AppError> {
        self.repo.delete(id).await.map_err(|e| match e {
            nomifun_db::DbError::NotFound(msg) => AppError::NotFound(msg),
            other => AppError::Internal(other.to_string()),
        })
    }

    /// Test a remote OpenClaw connection, including protocol authentication and
    /// device handshake, without persisting it.
    pub async fn test_connection(&self, req: TestRemoteAgentConnectionRequest) -> Result<(), AppError> {
        validate_ws_url(&req.url)?;
        let identity = generate_ephemeral_identity();
        let auth = auth_config(
            req.auth_type.unwrap_or(RemoteAgentAuthType::None),
            req.auth_token,
            None,
        )?;
        let (connection, _) = tokio::time::timeout(
            Duration::from_secs(15),
            OpenClawConnection::connect_with_options(&req.url, auth, &identity, req.allow_insecure),
        )
        .await
        .map_err(|_| AppError::Timeout("Connection timed out after 15 seconds".into()))??;
        connection.close().await;
        Ok(())
    }

    /// OpenClaw protocol handshake (15s timeout).
    pub async fn handshake(&self, id: &RemoteAgentId) -> Result<HandshakeResponse, AppError> {
        let row = self
            .repo
            .find_by_id(id)
            .await
            .map_err(db_err)?
            .ok_or_else(|| AppError::NotFound(format!("Remote agent '{id}' not found")))?;

        let protocol = parse_protocol(&row.protocol);
        if protocol != RemoteAgentProtocol::OpenClaw {
            return Err(AppError::BadRequest(
                "Handshake is only supported for OpenClaw protocol".into(),
            ));
        }

        validate_ws_url(&row.url)?;

        let token = row
            .auth_token
            .as_deref()
            .map(|encrypted| decrypt_string(encrypted, &self.encryption_key))
            .transpose()?;
        let device_token = row
            .device_token
            .as_deref()
            .map(|encrypted| decrypt_string(encrypted, &self.encryption_key))
            .transpose()?;
        let auth = auth_config(parse_auth_type(&row.auth_type), token, device_token)?;
        let identity = identity_from_row(&row, &self.encryption_key)?;
        let connect_result = tokio::time::timeout(
            Duration::from_secs(15),
            OpenClawConnection::connect_with_options(&row.url, auth, &identity, row.allow_insecure),
        )
        .await;

        match connect_result {
            Ok(Ok((connection, hello))) => {
                connection.close().await;
                if let Some(device_token) = hello.auth.device_token {
                    let encrypted = encrypt_string(&device_token, &self.encryption_key)?;
                    self.repo
                        .update_device_token(&id, Some(&encrypted))
                        .await
                        .map_err(db_err)?;
                }
                let now = nomifun_common::now_ms();
                let _ = self.repo.update_status(&id, "connected", Some(now)).await;
                Ok(HandshakeResponse {
                    status: "ok".to_string(),
                    error: None,
                })
            }
            Ok(Err(e)) => {
                if is_pairing_required_error(&e) {
                    let _ = self.repo.update_status(&id, "pending", None).await;
                    Ok(HandshakeResponse {
                        status: "pending_approval".to_string(),
                        error: Some(e.to_string()),
                    })
                } else {
                    let _ = self.repo.update_status(&id, "error", None).await;
                    Err(e)
                }
            }
            Err(_) => {
                let _ = self.repo.update_status(&id, "error", None).await;
                Err(AppError::Timeout("Handshake timed out after 15 seconds".into()))
            }
        }
    }

    // ── Private helpers ──────────────────────────────────────────

    fn row_to_list_item(&self, row: RemoteAgentRow) -> Result<RemoteAgentListItem, AppError> {
        let remote_agent_id = RemoteAgentId::parse(row.remote_agent_id.clone())
            .map_err(|e| AppError::Internal(format!("invalid stored remote_agent_id: {e}")))?;
        Ok(RemoteAgentListItem {
            remote_agent_id,
            name: row.name,
            protocol: parse_protocol(&row.protocol),
            url: row.url,
            auth_type: parse_auth_type(&row.auth_type),
            allow_insecure: row.allow_insecure,
            avatar: row.avatar,
            description: row.description,
            status: parse_status(&row.status),
            last_connected_at: row.last_connected_at,
            created_at: row.created_at,
            updated_at: row.updated_at,
        })
    }

    fn row_to_response(&self, row: RemoteAgentRow) -> Result<RemoteAgentResponse, AppError> {
        let remote_agent_id = RemoteAgentId::parse(row.remote_agent_id.clone())
            .map_err(|e| AppError::Internal(format!("invalid stored remote_agent_id: {e}")))?;
        let masked_token =
            row.auth_token
                .as_deref()
                .map(|encrypted| match decrypt_string(encrypted, &self.encryption_key) {
                    Ok(plain) => mask_token(&plain),
                    Err(e) => {
                        warn!("Failed to decrypt auth_token for agent {}: {e}", row.remote_agent_id);
                        "***".to_string()
                    }
                });

        let device_public_key = row
            .device_public_key
            .as_deref()
            .map(|encrypted| decrypt_string(encrypted, &self.encryption_key).unwrap_or_else(|_| "***".to_string()));

        Ok(RemoteAgentResponse {
            remote_agent_id,
            name: row.name,
            protocol: parse_protocol(&row.protocol),
            url: row.url,
            auth_type: parse_auth_type(&row.auth_type),
            auth_token: masked_token,
            allow_insecure: row.allow_insecure,
            avatar: row.avatar,
            description: row.description,
            device_id: row.device_id,
            device_public_key,
            status: parse_status(&row.status),
            last_connected_at: row.last_connected_at,
            created_at: row.created_at,
            updated_at: row.updated_at,
        })
    }
}

// ── Validation ──────────────────────────────────────────────────

fn validate_create_request(req: &CreateRemoteAgentRequest) -> Result<(), AppError> {
    if req.name.trim().is_empty() {
        return Err(AppError::BadRequest("name must not be empty".into()));
    }
    if req.url.trim().is_empty() {
        return Err(AppError::BadRequest("url must not be empty".into()));
    }
    if req.protocol != RemoteAgentProtocol::OpenClaw {
        return Err(AppError::BadRequest(
            "Only the OpenClaw remote protocol is currently implemented. Hermes remains available as a local ACP CLI (`hermes acp`).".into(),
        ));
    }
    validate_ws_url(&req.url)?;
    if req.auth_type != RemoteAgentAuthType::None {
        require_credential(req.auth_token.clone(), "Credential")?;
    }
    Ok(())
}

fn validate_ws_url(url: &str) -> Result<(), AppError> {
    let parsed = url::Url::parse(url)
        .map_err(|error| AppError::BadRequest(format!("Invalid remote WebSocket URL: {error}")))?;
    if parsed.scheme() != "ws" && parsed.scheme() != "wss" {
        return Err(AppError::BadRequest("URL must use ws:// or wss:// protocol".into()));
    }
    if parsed.host_str().is_none() {
        return Err(AppError::BadRequest("Remote WebSocket URL must include a host".into()));
    }
    if !parsed.username().is_empty() || parsed.password().is_some() {
        return Err(AppError::BadRequest(
            "Remote WebSocket URL must not embed credentials".into(),
        ));
    }
    Ok(())
}

fn remote_connection_identity_changed(before: &RemoteAgentRow, after: &RemoteAgentRow) -> bool {
    before.url != after.url || before.auth_type != after.auth_type || before.auth_token != after.auth_token
}

fn auth_config(
    auth_type: RemoteAgentAuthType,
    credential: Option<String>,
    device_token: Option<String>,
) -> Result<Option<AuthConfig>, AppError> {
    match auth_type {
        RemoteAgentAuthType::None => Ok(device_token.map(|device_token| AuthConfig {
            token: None,
            device_token: Some(device_token),
            password: None,
        })),
        RemoteAgentAuthType::Bearer => Ok(Some(AuthConfig {
            token: Some(require_credential(credential, "Bearer token")?),
            device_token,
            password: None,
        })),
        RemoteAgentAuthType::Password => Ok(Some(AuthConfig {
            token: None,
            device_token,
            password: Some(require_credential(credential, "Password")?),
        })),
    }
}

fn identity_from_row(row: &RemoteAgentRow, encryption_key: &[u8; 32]) -> Result<DeviceIdentity, AppError> {
    match (row.device_id.as_deref(), row.device_private_key.as_deref()) {
        (Some(device_id), Some(encrypted_private_key)) => {
            let private_b64 = decrypt_string(encrypted_private_key, encryption_key)?;
            let private_bytes = BASE64
                .decode(private_b64)
                .map_err(|error| AppError::Internal(format!("Invalid remote device private key: {error}")))?;
            identity_from_secret_bytes(device_id.to_owned(), &private_bytes)
        }
        (None, None) => Err(AppError::Internal(
            "Remote agent has no dedicated OpenClaw device identity; delete and re-create the remote agent configuration".into(),
        )),
        _ => Err(AppError::Internal(
            "Remote agent device identity is incomplete; re-create the remote agent configuration".into(),
        )),
    }
}

fn require_credential(value: Option<String>, label: &str) -> Result<String, AppError> {
    value
        .filter(|credential| !credential.trim().is_empty())
        .ok_or_else(|| AppError::BadRequest(format!("{label} is required for the selected authentication type")))
}

fn is_pairing_required_error(error: &AppError) -> bool {
    let message = error.to_string().to_ascii_lowercase();
    message.contains("pairing required")
        || message.contains("pairing_required")
        || message.contains("pairing-required")
        || message.contains("not_paired")
        || message.contains("not-paired")
        || message.contains("device_not_paired")
        || message.contains("not paired")
        || message.contains("device pairing")
}

// ── Token masking ───────────────────────────────────────────────

fn mask_token(token: &str) -> String {
    let suffix: String = token.chars().rev().take(4).collect::<Vec<_>>().into_iter().rev().collect();
    if token.chars().count() <= 4 {
        "***".to_string()
    } else {
        format!("***{suffix}")
    }
}

// ── Ed25519 key generation ──────────────────────────────────────

fn generate_device_keypair(encryption_key: &[u8; 32]) -> Result<(String, String, String), AppError> {
    let mut rng_bytes = [0u8; 32];
    getrandom::getrandom(&mut rng_bytes).map_err(|e| AppError::Internal(format!("RNG failure: {e}")))?;

    let signing_key = SigningKey::from_bytes(&rng_bytes);
    let verifying_key = signing_key.verifying_key();

    let device_id = hex::encode(sha2::Sha256::digest(verifying_key.as_bytes()));

    // Encode keys as base64 before encrypting
    let pub_b64 = BASE64.encode(verifying_key.as_bytes());
    let priv_b64 = BASE64.encode(signing_key.to_bytes());

    let encrypted_pub = encrypt_string(&pub_b64, encryption_key)?;
    let encrypted_priv = encrypt_string(&priv_b64, encryption_key)?;

    Ok((device_id, encrypted_pub, encrypted_priv))
}

// ── Enum serialization helpers ──────────────────────────────────

fn enum_to_str<T: serde::Serialize>(val: &T) -> String {
    serde_json::to_string(val)
        .unwrap_or_default()
        .trim_matches('"')
        .to_string()
}

fn enum_from_str<T: serde::de::DeserializeOwned>(s: &str) -> Option<T> {
    serde_json::from_str(&format!("\"{s}\"")).ok()
}

fn parse_protocol(s: &str) -> RemoteAgentProtocol {
    enum_from_str(s).unwrap_or(RemoteAgentProtocol::Acp)
}

fn parse_auth_type(s: &str) -> RemoteAgentAuthType {
    enum_from_str(s).unwrap_or(RemoteAgentAuthType::None)
}

fn parse_status(s: &str) -> RemoteAgentStatus {
    enum_from_str(s).unwrap_or(RemoteAgentStatus::Unknown)
}

fn db_err(e: nomifun_db::DbError) -> AppError {
    AppError::Internal(e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mask_token_long() {
        assert_eq!(mask_token("my-secret-token-1234"), "***1234");
    }

    #[test]
    fn mask_token_short() {
        assert_eq!(mask_token("ab"), "***");
    }

    #[test]
    fn mask_token_exactly_four() {
        assert_eq!(mask_token("abcd"), "***");
    }

    #[test]
    fn mask_token_five() {
        assert_eq!(mask_token("abcde"), "***bcde");
    }

    #[test]
    fn mask_token_unicode_is_char_safe() {
        assert_eq!(mask_token("密码令牌五"), "***码令牌五");
    }

    #[test]
    fn validate_ws_url_accepts_ws() {
        assert!(validate_ws_url("ws://localhost:8080").is_ok());
    }

    #[test]
    fn validate_ws_url_accepts_wss() {
        assert!(validate_ws_url("wss://remote.example.com").is_ok());
    }

    #[test]
    fn validate_ws_url_rejects_http() {
        assert!(validate_ws_url("http://example.com").is_err());
    }

    #[test]
    fn validate_ws_url_rejects_https() {
        assert!(validate_ws_url("https://example.com").is_err());
    }

    #[test]
    fn validate_ws_url_rejects_prefix_smuggling() {
        assert!(validate_ws_url("wss://").is_err());
        assert!(validate_ws_url("ws://user:pass@example.com").is_err());
        assert!(validate_ws_url("ws://example.com bad").is_err());
    }

    #[test]
    fn pairing_required_error_recognizes_gateway_shapes() {
        for error in [
            AppError::Unauthorized("PAIRING_REQUIRED: pairing required".into()),
            AppError::Unauthorized("NOT_PAIRED: device is not paired".into()),
            AppError::BadGateway(r#"AUTH_FAILED; details={"code":"PAIRING_REQUIRED"}"#.into()),
            AppError::BadGateway("Device pairing must be approved".into()),
        ] {
            assert!(is_pairing_required_error(&error), "{error}");
        }
    }

    #[test]
    fn pairing_required_error_rejects_other_auth_failures() {
        assert!(!is_pairing_required_error(&AppError::Unauthorized(
            "Invalid bearer token".into()
        )));
    }

    #[test]
    fn remote_connection_identity_detects_endpoint_or_credential_changes() {
        let before = sample_remote_agent_row();
        let mut after = before.clone();
        assert!(!remote_connection_identity_changed(&before, &after));

        after.url = "wss://other.example.com".into();
        assert!(remote_connection_identity_changed(&before, &after));

        after = before.clone();
        after.auth_type = "password".into();
        assert!(remote_connection_identity_changed(&before, &after));

        after = before.clone();
        after.auth_token = Some("different-ciphertext".into());
        assert!(remote_connection_identity_changed(&before, &after));

        after = before.clone();
        after.name = "Renamed".into();
        assert!(!remote_connection_identity_changed(&before, &after));
    }

    #[test]
    fn generate_device_keypair_produces_valid_output() {
        let key = [0x42u8; 32];
        let (id, pub_key, priv_key) = generate_device_keypair(&key).unwrap();

        assert_eq!(id.len(), 64);
        assert!(!pub_key.is_empty());
        assert!(!priv_key.is_empty());

        // Decrypt and verify the keys decode correctly
        let pub_b64 = decrypt_string(&pub_key, &key).unwrap();
        let priv_b64 = decrypt_string(&priv_key, &key).unwrap();

        let pub_bytes = BASE64.decode(&pub_b64).unwrap();
        let priv_bytes = BASE64.decode(&priv_b64).unwrap();
        assert_eq!(pub_bytes.len(), 32);
        assert_eq!(priv_bytes.len(), 32);

        // Verify the keypair is consistent
        let signing = SigningKey::from_bytes(&priv_bytes.try_into().unwrap());
        let verifying = signing.verifying_key();
        assert_eq!(verifying.as_bytes(), pub_bytes.as_slice());
    }

    #[test]
    fn enum_to_str_protocol() {
        assert_eq!(enum_to_str(&RemoteAgentProtocol::OpenClaw), "openclaw");
        assert_eq!(enum_to_str(&RemoteAgentProtocol::ZeroClaw), "zeroclaw");
        assert_eq!(enum_to_str(&RemoteAgentProtocol::Acp), "acp");
    }

    #[test]
    fn enum_to_str_auth_type() {
        assert_eq!(enum_to_str(&RemoteAgentAuthType::Bearer), "bearer");
        assert_eq!(enum_to_str(&RemoteAgentAuthType::Password), "password");
        assert_eq!(enum_to_str(&RemoteAgentAuthType::None), "none");
    }

    #[test]
    fn parse_protocol_known_values() {
        assert_eq!(parse_protocol("openclaw"), RemoteAgentProtocol::OpenClaw);
        assert_eq!(parse_protocol("zeroclaw"), RemoteAgentProtocol::ZeroClaw);
        assert_eq!(parse_protocol("acp"), RemoteAgentProtocol::Acp);
    }

    #[test]
    fn parse_protocol_unknown_defaults() {
        assert_eq!(parse_protocol("unknown_proto"), RemoteAgentProtocol::Acp);
    }

    #[test]
    fn parse_auth_type_known_values() {
        assert_eq!(parse_auth_type("bearer"), RemoteAgentAuthType::Bearer);
        assert_eq!(parse_auth_type("password"), RemoteAgentAuthType::Password);
        assert_eq!(parse_auth_type("none"), RemoteAgentAuthType::None);
    }

    #[test]
    fn parse_status_known_values() {
        assert_eq!(parse_status("unknown"), RemoteAgentStatus::Unknown);
        assert_eq!(parse_status("connected"), RemoteAgentStatus::Connected);
        assert_eq!(parse_status("pending"), RemoteAgentStatus::Pending);
        assert_eq!(parse_status("error"), RemoteAgentStatus::Error);
    }

    fn sample_remote_agent_row() -> RemoteAgentRow {
        RemoteAgentRow {
            id: 1,
            remote_agent_id: nomifun_common::RemoteAgentId::new().into_string(),
            name: "Remote".into(),
            protocol: "openclaw".into(),
            url: "wss://example.com".into(),
            auth_type: "bearer".into(),
            auth_token: Some("encrypted-token".into()),
            allow_insecure: false,
            avatar: None,
            description: None,
            device_id: Some("device-id".into()),
            device_public_key: Some("public-key".into()),
            device_private_key: Some("private-key".into()),
            device_token: Some("device-token".into()),
            status: "unknown".into(),
            last_connected_at: None,
            created_at: 1,
            updated_at: 1,
        }
    }
}

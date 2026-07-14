use std::fs;
use std::path::{Path, PathBuf};

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use ed25519_dalek::{SigningKey, VerifyingKey};
use nomifun_common::AppError;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tracing::{debug, warn};

use super::protocol::{CLIENT_ID, CLIENT_MODE, DeviceAuthParams};

#[derive(Clone, Debug)]
pub struct DeviceIdentity {
    pub device_id: String,
    pub signing_key: SigningKey,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct StoredIdentity {
    version: u32,
    device_id: String,
    public_key_pem: String,
    private_key_pem: String,
    #[serde(default)]
    created_at_ms: Option<i64>,
}

fn default_identity_path() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".openclaw")
        .join("identity")
        .join("device.json")
}

pub fn load_or_create_identity(custom_path: Option<&Path>) -> Result<DeviceIdentity, AppError> {
    let path = custom_path.map(PathBuf::from).unwrap_or_else(default_identity_path);

    if let Ok(identity) = load_identity(&path) {
        return Ok(identity);
    }

    let identity = generate_identity();
    if let Err(e) = save_identity(&path, &identity) {
        warn!(error = %e, "Failed to save device identity, continuing with ephemeral key");
    }
    Ok(identity)
}

/// Reconstruct a device identity from a persisted Ed25519 secret key.
///
/// Remote-agent rows keep a dedicated device id/keypair so different gateways
/// do not share the process-global `~/.openclaw` identity. The caller is
/// responsible for decrypting the database value before invoking this helper.
pub fn identity_from_secret_bytes(device_id: String, secret: &[u8]) -> Result<DeviceIdentity, AppError> {
    let secret: [u8; 32] = secret
        .try_into()
        .map_err(|_| AppError::Internal("Invalid Ed25519 private key length".into()))?;
    let signing_key = SigningKey::from_bytes(&secret);
    let derived_id = derive_device_id(&signing_key.verifying_key());
    if device_id != derived_id {
        debug!(
            stored_device_id = %device_id,
            derived_device_id = %derived_id,
            "Remote device ID mismatch, using the public-key fingerprint"
        );
    }
    Ok(DeviceIdentity {
        device_id: derived_id,
        signing_key,
    })
}

fn load_identity(path: &Path) -> Result<DeviceIdentity, AppError> {
    let content =
        fs::read_to_string(path).map_err(|e| AppError::Internal(format!("Failed to read device identity: {e}")))?;

    let stored: StoredIdentity = serde_json::from_str(&content)
        .map_err(|e| AppError::Internal(format!("Failed to parse device identity: {e}")))?;

    if stored.version != 1 {
        return Err(AppError::Internal(format!(
            "Unsupported device identity version: {}",
            stored.version
        )));
    }

    let signing_key = pem_to_signing_key(&stored.private_key_pem)?;

    let derived_id = derive_device_id(&signing_key.verifying_key());
    if derived_id != stored.device_id {
        debug!("Device ID mismatch, using derived ID");
    }

    Ok(DeviceIdentity {
        device_id: derived_id,
        signing_key,
    })
}

fn save_identity(path: &Path, identity: &DeviceIdentity) -> Result<(), AppError> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .map_err(|e| AppError::Internal(format!("Failed to create identity directory: {e}")))?;
    }

    let (pub_pem, priv_pem) = signing_key_to_pem(&identity.signing_key);

    let stored = StoredIdentity {
        version: 1,
        device_id: identity.device_id.clone(),
        public_key_pem: pub_pem,
        private_key_pem: priv_pem,
        created_at_ms: Some(nomifun_common::now_ms()),
    };

    let json = serde_json::to_string_pretty(&stored)
        .map_err(|e| AppError::Internal(format!("Failed to serialize device identity: {e}")))?;

    fs::write(path, format!("{json}\n"))
        .map_err(|e| AppError::Internal(format!("Failed to write device identity: {e}")))?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(path, fs::Permissions::from_mode(0o600));
    }

    Ok(())
}

pub(crate) fn generate_identity() -> DeviceIdentity {
    let mut secret = [0u8; 32];
    getrandom::getrandom(&mut secret).expect("Failed to generate random bytes");
    let signing_key = SigningKey::from_bytes(&secret);
    let device_id = derive_device_id(&signing_key.verifying_key());
    DeviceIdentity { device_id, signing_key }
}

/// Generate an in-memory identity without reading or writing `~/.openclaw`.
/// Connection tests use this so a dry run has no filesystem side effects.
pub(crate) fn generate_ephemeral_identity() -> DeviceIdentity {
    generate_identity()
}

fn derive_device_id(verifying_key: &VerifyingKey) -> String {
    let raw = verifying_key.as_bytes();
    let hash = Sha256::digest(raw);
    hex::encode(hash)
}

pub fn build_device_auth_params(
    identity: &DeviceIdentity,
    nonce: Option<&str>,
    token: Option<&str>,
    platform: &str,
    device_family: Option<&str>,
) -> DeviceAuthParams {
    let role = "operator";
    let scopes = "operator.admin";
    let signed_at = nomifun_common::now_ms();

    let payload = build_auth_payload(
        &identity.device_id,
        CLIENT_ID,
        CLIENT_MODE,
        role,
        scopes,
        signed_at,
        token,
        nonce,
        platform,
        device_family,
    );

    let signature = sign_payload(&identity.signing_key, &payload);
    let public_key = public_key_base64url(&identity.signing_key);

    DeviceAuthParams {
        id: identity.device_id.clone(),
        public_key,
        signature,
        signed_at,
        nonce: nonce.map(String::from),
    }
}

#[allow(clippy::too_many_arguments)]
fn build_auth_payload(
    device_id: &str,
    client_id: &str,
    client_mode: &str,
    role: &str,
    scopes: &str,
    signed_at_ms: i64,
    token: Option<&str>,
    nonce: Option<&str>,
    platform: &str,
    device_family: Option<&str>,
) -> String {
    let version = if nonce.is_some() { "v3" } else { "v1" };
    let token_str = token.unwrap_or("");

    let mut parts = vec![
        version.to_owned(),
        device_id.to_owned(),
        client_id.to_owned(),
        client_mode.to_owned(),
        role.to_owned(),
        scopes.to_owned(),
        signed_at_ms.to_string(),
        token_str.to_owned(),
    ];

    if version == "v3" {
        parts.push(nonce.unwrap_or("").to_owned());
        parts.push(normalize_device_metadata_for_auth(platform));
        parts.push(normalize_device_metadata_for_auth(device_family.unwrap_or("")));
    }

    parts.join("|")
}

fn normalize_device_metadata_for_auth(value: &str) -> String {
    value.trim().to_ascii_lowercase()
}

fn sign_payload(signing_key: &SigningKey, payload: &str) -> String {
    use ed25519_dalek::Signer;
    let signature = signing_key.sign(payload.as_bytes());
    URL_SAFE_NO_PAD.encode(signature.to_bytes())
}

fn public_key_base64url(signing_key: &SigningKey) -> String {
    let vk = signing_key.verifying_key();
    let raw = vk.as_bytes();
    URL_SAFE_NO_PAD.encode(raw)
}

// ── PEM Encoding/Decoding ───────────────────────────────────────────────

const ED25519_PKCS8_PREFIX: [u8; 16] = [
    0x30, 0x2e, 0x02, 0x01, 0x00, 0x30, 0x05, 0x06, 0x03, 0x2b, 0x65, 0x70, 0x04, 0x22, 0x04, 0x20,
];

const ED25519_SPKI_PREFIX: [u8; 12] = [0x30, 0x2a, 0x30, 0x05, 0x06, 0x03, 0x2b, 0x65, 0x70, 0x03, 0x21, 0x00];

fn pem_to_signing_key(pem: &str) -> Result<SigningKey, AppError> {
    let der = pem_decode(pem, "PRIVATE KEY")?;

    if der.len() == ED25519_PKCS8_PREFIX.len() + 32 && der[..ED25519_PKCS8_PREFIX.len()] == ED25519_PKCS8_PREFIX {
        let raw: [u8; 32] = der[ED25519_PKCS8_PREFIX.len()..]
            .try_into()
            .map_err(|_| AppError::Internal("Invalid Ed25519 private key length".into()))?;
        return Ok(SigningKey::from_bytes(&raw));
    }

    Err(AppError::Internal("Unrecognized Ed25519 PKCS8 format".into()))
}

fn signing_key_to_pem(key: &SigningKey) -> (String, String) {
    let mut priv_der = Vec::with_capacity(ED25519_PKCS8_PREFIX.len() + 32);
    priv_der.extend_from_slice(&ED25519_PKCS8_PREFIX);
    priv_der.extend_from_slice(key.as_bytes());
    let priv_pem = pem_encode(&priv_der, "PRIVATE KEY");

    let mut pub_der = Vec::with_capacity(ED25519_SPKI_PREFIX.len() + 32);
    pub_der.extend_from_slice(&ED25519_SPKI_PREFIX);
    pub_der.extend_from_slice(key.verifying_key().as_bytes());
    let pub_pem = pem_encode(&pub_der, "PUBLIC KEY");

    (pub_pem, priv_pem)
}

fn pem_encode(der: &[u8], label: &str) -> String {
    use base64::engine::general_purpose::STANDARD;
    let b64 = STANDARD.encode(der);
    let mut pem = format!("-----BEGIN {label}-----\n");
    for chunk in b64.as_bytes().chunks(64) {
        pem.push_str(std::str::from_utf8(chunk).unwrap());
        pem.push('\n');
    }
    pem.push_str(&format!("-----END {label}-----\n"));
    pem
}

fn pem_decode(pem: &str, label: &str) -> Result<Vec<u8>, AppError> {
    use base64::engine::general_purpose::STANDARD;
    let begin = format!("-----BEGIN {label}-----");
    let end = format!("-----END {label}-----");

    let b64: String = pem.lines().filter(|line| !line.starts_with("-----")).collect();

    if !pem.contains(&begin) || !pem.contains(&end) {
        return Err(AppError::Internal(format!("Invalid PEM: missing {label} markers")));
    }

    STANDARD
        .decode(b64)
        .map_err(|e| AppError::Internal(format!("Failed to decode PEM base64: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::Verifier;

    #[test]
    fn generate_and_derive_id() {
        let identity = generate_identity();
        assert_eq!(identity.device_id.len(), 64);
        let re_derived = derive_device_id(&identity.signing_key.verifying_key());
        assert_eq!(identity.device_id, re_derived);
    }

    #[test]
    fn sign_and_verify_roundtrip() {
        let identity = generate_identity();
        let payload = "v2|device123|gateway-client|backend|operator|operator.admin|1700000000000||nonce123";
        let signature_b64 = sign_payload(&identity.signing_key, payload);

        let sig_bytes = URL_SAFE_NO_PAD.decode(&signature_b64).unwrap();
        let signature = ed25519_dalek::Signature::from_bytes(sig_bytes.as_slice().try_into().unwrap());
        identity
            .signing_key
            .verifying_key()
            .verify(payload.as_bytes(), &signature)
            .unwrap();
    }

    #[test]
    fn public_key_base64url_is_correct_length() {
        let identity = generate_identity();
        let b64 = public_key_base64url(&identity.signing_key);
        let raw = URL_SAFE_NO_PAD.decode(&b64).unwrap();
        assert_eq!(raw.len(), 32);
    }

    #[test]
    fn build_auth_payload_v1() {
        let payload = build_auth_payload(
            "abc123",
            "gateway-client",
            "backend",
            "operator",
            "operator.admin",
            1700000000000,
            None,
            None,
            "",
            None,
        );
        assert_eq!(
            payload,
            "v1|abc123|gateway-client|backend|operator|operator.admin|1700000000000|"
        );
    }

    #[test]
    fn build_auth_payload_v3_with_nonce() {
        let payload = build_auth_payload(
            "abc123",
            "gateway-client",
            "backend",
            "operator",
            "operator.admin",
            1700000000000,
            Some("tok"),
            Some("nonce123"),
            "WinDows",
            Some("DeskTop"),
        );
        assert_eq!(
            payload,
            "v3|abc123|gateway-client|backend|operator|operator.admin|1700000000000|tok|nonce123|windows|desktop"
        );
    }

    #[test]
    fn pem_roundtrip() {
        let identity = generate_identity();
        let (pub_pem, priv_pem) = signing_key_to_pem(&identity.signing_key);

        assert!(pub_pem.starts_with("-----BEGIN PUBLIC KEY-----"));
        assert!(priv_pem.starts_with("-----BEGIN PRIVATE KEY-----"));

        let recovered = pem_to_signing_key(&priv_pem).unwrap();
        assert_eq!(identity.signing_key.as_bytes(), recovered.as_bytes());
    }

    #[test]
    fn save_and_load_identity() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("device.json");

        let identity = generate_identity();
        save_identity(&path, &identity).unwrap();
        let loaded = load_identity(&path).unwrap();

        assert_eq!(identity.device_id, loaded.device_id);
        assert_eq!(identity.signing_key.as_bytes(), loaded.signing_key.as_bytes());
    }

    #[test]
    fn build_device_auth_params_produces_valid_signature() {
        let identity = generate_identity();
        let params = build_device_auth_params(&identity, Some("nonce-x"), None, "windows", None);

        assert_eq!(params.id, identity.device_id);
        assert_eq!(params.nonce.as_deref(), Some("nonce-x"));

        let pub_raw = URL_SAFE_NO_PAD.decode(&params.public_key).unwrap();
        assert_eq!(pub_raw.len(), 32);

        let sig_bytes = URL_SAFE_NO_PAD.decode(&params.signature).unwrap();
        assert_eq!(sig_bytes.len(), 64);
    }

    #[test]
    fn build_device_auth_params_v3_binds_matching_platform_and_family() {
        let identity = generate_identity();
        let params = build_device_auth_params(
            &identity,
            Some("nonce-x"),
            Some("device-token"),
            "windows",
            Some("windows"),
        );
        let payload = build_auth_payload(
            &identity.device_id,
            CLIENT_ID,
            CLIENT_MODE,
            "operator",
            "operator.admin",
            params.signed_at,
            Some("device-token"),
            Some("nonce-x"),
            "windows",
            Some("windows"),
        );
        let sig_bytes = URL_SAFE_NO_PAD.decode(&params.signature).unwrap();
        let signature = ed25519_dalek::Signature::from_bytes(sig_bytes.as_slice().try_into().unwrap());

        identity
            .signing_key
            .verifying_key()
            .verify(payload.as_bytes(), &signature)
            .unwrap();
    }
}

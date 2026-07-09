//! Application configuration parsed from CLI arguments + key derivation.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use nomifun_auth::AuthPolicy;
use sha2::{Digest, Sha256};

const DATA_ENCRYPTION_KEY_FILE: &str = "encryption_key";

/// Application configuration parsed from CLI arguments.
#[derive(Debug, Clone)]
pub struct AppConfig {
    pub host: String,
    pub port: u16,
    pub data_dir: PathBuf,
    pub work_dir: PathBuf,
    pub app_version: String,
    /// Authentication policy (single source of truth, replaces the old
    /// `local: bool`). Desktop = `TrustLocalToken`; standalone web = `Required`;
    /// `--insecure-no-auth` / `--local` = `NoAuth`.
    pub auth_policy: AuthPolicy,
    /// Per-boot secret the desktop's own webview presents to be trusted as the
    /// local client. Only `Some` under `AuthPolicy::TrustLocalToken`.
    pub local_trust_secret: Option<Arc<str>>,
}

impl AppConfig {
    /// Format as `host:port` for socket binding.
    pub fn socket_addr(&self) -> String {
        format!("{}:{}", self.host, self.port)
    }

    /// Path to the SQLite database file.
    pub fn database_path(&self) -> PathBuf {
        self.data_dir.join("nomifun-backend.db")
    }
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            host: nomifun_common::constants::DEFAULT_HOST.to_string(),
            port: nomifun_common::constants::DEFAULT_PORT,
            data_dir: PathBuf::from("data"),
            work_dir: PathBuf::from("data"),
            app_version: env!("CARGO_PKG_VERSION").to_string(),
            auth_policy: AuthPolicy::Required,
            local_trust_secret: None,
        }
    }
}

/// Derive a 32-byte encryption key from the JWT secret using SHA-256.
pub fn derive_encryption_key(jwt_secret: &str) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(b"nomifun-encryption-key:");
    hasher.update(jwt_secret.as_bytes());
    hasher.finalize().into()
}

/// Load the app's persistent data-encryption key, creating it on first boot.
///
/// Existing installs historically derived this key from the JWT secret. When no
/// key file exists, we persist that current derived key so old ciphertext remains
/// readable while future JWT rotation no longer changes the data key.
pub fn load_or_create_data_encryption_key(data_dir: &Path, jwt_secret: &str) -> anyhow::Result<[u8; 32]> {
    let key_path = data_dir.join(DATA_ENCRYPTION_KEY_FILE);

    if key_path.exists() {
        let raw = fs::read_to_string(&key_path)?;
        return parse_hex_key(raw.trim(), &key_path);
    }

    fs::create_dir_all(data_dir)?;
    let key = derive_encryption_key(jwt_secret);
    let tmp_path = key_path.with_extension("tmp");
    #[cfg(unix)]
    {
        use std::io::{ErrorKind, Write};
        use std::os::unix::fs::OpenOptionsExt;

        match fs::remove_file(&tmp_path) {
            Ok(()) => {}
            Err(e) if e.kind() == ErrorKind::NotFound => {}
            Err(e) => return Err(e.into()),
        }

        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&tmp_path)?;
        file.write_all(hex_encode_key(&key).as_bytes())?;
        file.sync_all()?;
    }
    #[cfg(not(unix))]
    {
        fs::write(&tmp_path, hex_encode_key(&key))?;
    }
    fs::rename(&tmp_path, &key_path)?;
    Ok(key)
}

fn hex_encode_key(key: &[u8; 32]) -> String {
    let mut out = String::with_capacity(64);
    for byte in key {
        out.push(nibble_to_hex(byte >> 4));
        out.push(nibble_to_hex(byte & 0x0f));
    }
    out
}

fn nibble_to_hex(nibble: u8) -> char {
    match nibble {
        0..=9 => (b'0' + nibble) as char,
        10..=15 => (b'a' + (nibble - 10)) as char,
        _ => unreachable!("nibble is masked to four bits"),
    }
}

fn parse_hex_key(raw: &str, path: &Path) -> anyhow::Result<[u8; 32]> {
    if raw.len() != 64 {
        anyhow::bail!(
            "Invalid data encryption key at {}: expected 64 hex characters, got {}",
            path.display(),
            raw.len()
        );
    }

    let mut key = [0u8; 32];
    for (index, byte) in key.iter_mut().enumerate() {
        let offset = index * 2;
        let hi = hex_value(raw.as_bytes()[offset], path)?;
        let lo = hex_value(raw.as_bytes()[offset + 1], path)?;
        *byte = (hi << 4) | lo;
    }
    Ok(key)
}

fn hex_value(byte: u8, path: &Path) -> anyhow::Result<u8> {
    match byte {
        b'0'..=b'9' => Ok(byte - b'0'),
        b'a'..=b'f' => Ok(byte - b'a' + 10),
        b'A'..=b'F' => Ok(byte - b'A' + 10),
        _ => anyhow::bail!("Invalid data encryption key at {}: non-hex character", path.display()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_app_config_default() {
        let config = AppConfig::default();
        assert_eq!(config.host, "127.0.0.1");
        assert_eq!(config.port, 25808);
        assert_eq!(config.data_dir, PathBuf::from("data"));
        assert_eq!(config.app_version, env!("CARGO_PKG_VERSION"));
    }

    #[test]
    fn test_app_config_socket_addr() {
        let config = AppConfig {
            host: "0.0.0.0".to_string(),
            port: 3000,
            ..Default::default()
        };
        assert_eq!(config.socket_addr(), "0.0.0.0:3000");
    }

    #[test]
    fn test_app_config_database_path() {
        let config = AppConfig {
            data_dir: PathBuf::from("/tmp/nomifun"),
            ..Default::default()
        };
        assert_eq!(config.database_path(), PathBuf::from("/tmp/nomifun/nomifun-backend.db"));
    }

    #[test]
    fn data_encryption_key_is_persisted_independently_of_jwt_secret() {
        let tmp = tempfile::tempdir().unwrap();
        let first = load_or_create_data_encryption_key(tmp.path(), "jwt-secret-before").unwrap();
        let second = load_or_create_data_encryption_key(tmp.path(), "jwt-secret-after").unwrap();

        assert_eq!(first, derive_encryption_key("jwt-secret-before"));
        assert_eq!(second, first);
        assert_ne!(second, derive_encryption_key("jwt-secret-after"));
    }
}

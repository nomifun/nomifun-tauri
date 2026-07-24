use nomifun_common::{ConnectorCredentialId, TimestampMs};
use serde::{Deserialize, Serialize};
use sqlx::{Row, sqlite::SqliteRow};

/// Row in `connector_credentials` — encrypted credentials for a source connector
/// (feishu / notion / …). `payload_encrypted` is an opaque AES-256-GCM ciphertext;
/// the service layer holds the key and (de)serializes the JSON payload (e.g.
/// `{ "app_id": ..., "app_secret": ... }`). Secrets never appear on the wire —
/// API responses expose only `credential_id` / `kind` / `name`; the local
/// technical `id` never leaves the repository implementation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConnectorCredentialRow {
    pub credential_id: ConnectorCredentialId,
    /// Connector discriminator: "feishu", "notion", …
    pub kind: String,
    /// User-facing label.
    pub name: String,
    /// AES-256-GCM ciphertext of the JSON credential payload.
    pub payload_encrypted: String,
    pub created_at: TimestampMs,
    pub updated_at: TimestampMs,
}

impl<'row> sqlx::FromRow<'row, SqliteRow> for ConnectorCredentialRow {
    fn from_row(row: &'row SqliteRow) -> Result<Self, sqlx::Error> {
        let raw_id: String = row.try_get("credential_id")?;
        let credential_id = ConnectorCredentialId::parse(raw_id)
            .map_err(|error| sqlx::Error::Decode(Box::new(error)))?;
        Ok(Self {
            credential_id,
            kind: row.try_get("kind")?,
            name: row.try_get("name")?,
            payload_encrypted: row.try_get("payload_encrypted")?,
            created_at: row.try_get("created_at")?,
            updated_at: row.try_get("updated_at")?,
        })
    }
}

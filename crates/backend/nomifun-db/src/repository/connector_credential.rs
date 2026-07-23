use crate::error::DbError;
use crate::models::ConnectorCredentialRow;
use nomifun_common::ConnectorCredentialId;

/// Data access for `connector_credentials`. Stores already-encrypted payloads —
/// the service layer handles encryption/decryption (mirrors providers' api_key).
#[async_trait::async_trait]
pub trait IConnectorCredentialRepository: Send + Sync {
    /// All credentials, ordered by creation time ascending.
    async fn list(&self) -> Result<Vec<ConnectorCredentialRow>, DbError>;

    /// One credential by business ID, or `None`.
    async fn get(
        &self,
        credential_id: &ConnectorCredentialId,
    ) -> Result<Option<ConnectorCredentialRow>, DbError>;

    /// Insert a new credential (id generated) and return the stored row.
    async fn create(&self, kind: &str, name: &str, payload_encrypted: &str) -> Result<ConnectorCredentialRow, DbError>;

    /// Delete by business ID. `DbError::NotFound` when absent.
    async fn delete(&self, credential_id: &ConnectorCredentialId) -> Result<(), DbError>;
}

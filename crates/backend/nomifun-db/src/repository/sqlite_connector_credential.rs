use sqlx::SqlitePool;

use crate::error::DbError;
use crate::models::ConnectorCredentialRow;
use crate::repository::IConnectorCredentialRepository;
use nomifun_common::ConnectorCredentialId;

/// SQLite-backed [`IConnectorCredentialRepository`].
#[derive(Clone, Debug)]
pub struct SqliteConnectorCredentialRepository {
    pool: SqlitePool,
}

impl SqliteConnectorCredentialRepository {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }
}

#[async_trait::async_trait]
impl IConnectorCredentialRepository for SqliteConnectorCredentialRepository {
    async fn list(&self) -> Result<Vec<ConnectorCredentialRow>, DbError> {
        let rows = sqlx::query_as::<_, ConnectorCredentialRow>(
            "SELECT credential_id, kind, name, payload_encrypted, created_at, updated_at \
             FROM connector_credentials ORDER BY created_at ASC, id ASC",
        )
        .fetch_all(&self.pool)
        .await?;
        Ok(rows)
    }

    async fn get(
        &self,
        credential_id: &ConnectorCredentialId,
    ) -> Result<Option<ConnectorCredentialRow>, DbError> {
        let row = sqlx::query_as::<_, ConnectorCredentialRow>(
            "SELECT credential_id, kind, name, payload_encrypted, created_at, updated_at \
             FROM connector_credentials WHERE credential_id = ?",
        )
            .bind(credential_id.as_str())
            .fetch_optional(&self.pool)
            .await?;
        Ok(row)
    }

    async fn create(&self, kind: &str, name: &str, payload_encrypted: &str) -> Result<ConnectorCredentialRow, DbError> {
        let credential_id = ConnectorCredentialId::new();
        let now = nomifun_common::now_ms();
        sqlx::query(
            "INSERT INTO connector_credentials \
             (credential_id, kind, name, payload_encrypted, created_at, updated_at) \
             VALUES (?, ?, ?, ?, ?, ?)",
        )
        .bind(credential_id.as_str())
        .bind(kind)
        .bind(name)
        .bind(payload_encrypted)
        .bind(now)
        .bind(now)
        .execute(&self.pool)
        .await?;
        Ok(ConnectorCredentialRow {
            credential_id,
            kind: kind.to_owned(),
            name: name.to_owned(),
            payload_encrypted: payload_encrypted.to_owned(),
            created_at: now,
            updated_at: now,
        })
    }

    async fn delete(&self, credential_id: &ConnectorCredentialId) -> Result<(), DbError> {
        let mut tx = self.pool.begin().await?;

        // Obtain SQLite's writer lock and prove the business ID exists before
        // checking JSON references. The local integer row ID stays inside this
        // repository and is never accepted from or returned to callers.
        let local_id: Option<i64> = sqlx::query_scalar(
            "SELECT id FROM connector_credentials WHERE credential_id = ?",
        )
        .bind(credential_id.as_str())
        .fetch_optional(&mut *tx)
        .await?;
        let Some(local_id) = local_id else {
            return Err(DbError::NotFound(format!(
                "connector credential {credential_id}"
            )));
        };
        sqlx::query(
            "UPDATE connector_credentials SET updated_at = updated_at WHERE id = ?",
        )
        .bind(local_id)
        .execute(&mut *tx)
        .await?;

        let referencing_base: Option<String> = sqlx::query_scalar(
            "SELECT knowledge_base_id FROM knowledge_bases \
             WHERE json_valid(extra) \
               AND json_type(extra, '$.source.credentialRef') = 'text' \
               AND json_extract(extra, '$.source.credentialRef') = ? \
             LIMIT 1",
        )
        .bind(credential_id.as_str())
        .fetch_optional(&mut *tx)
        .await?;
        if let Some(knowledge_base_id) = referencing_base {
            return Err(DbError::Conflict(format!(
                "connector credential {credential_id} is still referenced by knowledge base {knowledge_base_id}"
            )));
        }

        let res = sqlx::query("DELETE FROM connector_credentials WHERE id = ?")
            .bind(local_id)
            .execute(&mut *tx)
            .await?;
        if res.rows_affected() == 0 {
            return Err(DbError::NotFound(format!(
                "connector credential {credential_id}"
            )));
        }
        tx.commit().await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::init_database_memory;

    #[tokio::test]
    async fn connector_credential_crud_roundtrip() {
        let db = init_database_memory().await.unwrap();
        let repo = SqliteConnectorCredentialRepository::new(db.pool().clone());

        let row = repo.create("feishu", "我的飞书", "ENC(payload)").await.unwrap();
        nomifun_common::validate_uuidv7(row.credential_id.as_str()).unwrap();

        let got = repo.get(&row.credential_id).await.unwrap().unwrap();
        assert_eq!(got.kind, "feishu");
        assert_eq!(got.name, "我的飞书");
        assert_eq!(got.payload_encrypted, "ENC(payload)");

        // A second credential of the same kind is allowed (different tenant).
        repo.create("feishu", "另一个飞书", "ENC(other)").await.unwrap();
        assert_eq!(repo.list().await.unwrap().len(), 2);

        repo.delete(&row.credential_id).await.unwrap();
        assert!(repo.get(&row.credential_id).await.unwrap().is_none());
        assert!(matches!(repo.delete(&row.credential_id).await, Err(DbError::NotFound(_))), "second delete errors");
    }

    #[tokio::test]
    async fn local_technical_id_is_not_the_repository_business_key() {
        let db = init_database_memory().await.unwrap();
        let credential_id = ConnectorCredentialId::new();
        sqlx::query(
            "INSERT INTO connector_credentials \
             (id, credential_id, kind, name, payload_encrypted, created_at, updated_at) \
             VALUES (999, ?, 'feishu', 'stored', 'ENC(payload)', 1, 1)",
        )
        .bind(credential_id.as_str())
        .execute(db.pool())
        .await
        .unwrap();

        let repo = SqliteConnectorCredentialRepository::new(db.pool().clone());
        let row = repo.get(&credential_id).await.unwrap().unwrap();
        assert_eq!(row.credential_id, credential_id);
        assert_eq!(row.name, "stored");
    }

    #[tokio::test]
    async fn delete_restricts_live_knowledge_source_reference() {
        let db = init_database_memory().await.unwrap();
        let repo = SqliteConnectorCredentialRepository::new(db.pool().clone());
        let row = repo.create("feishu", "linked", "ENC(payload)").await.unwrap();
        let knowledge_base_id = nomifun_common::KnowledgeBaseId::new();
        let extra = serde_json::json!({
            "source": {
                "kind": "feishu",
                "mode": "snapshot",
                "credentialRef": row.credential_id,
                "scope": { "space_id": "space" }
            }
        })
        .to_string();
        sqlx::query(
            "INSERT INTO knowledge_bases \
             (knowledge_base_id, name, root_path, extra, created_at, updated_at) \
             VALUES (?, 'linked', '/tmp/linked', ?, 1, 1)",
        )
        .bind(knowledge_base_id.as_str())
        .bind(extra)
        .execute(db.pool())
        .await
        .unwrap();

        assert!(matches!(
            repo.delete(&row.credential_id).await,
            Err(DbError::Conflict(_))
        ));
        assert!(repo.get(&row.credential_id).await.unwrap().is_some());
    }
}

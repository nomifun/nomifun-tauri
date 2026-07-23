//! Source-connector framework. A connector pulls remote documents (Feishu
//! wiki, Notion, …) into a managed knowledge base's `snapshots/` dir as
//! markdown — the **"snapshot-as-seam"** invariant: connectors only produce the
//! same markdown-file shape the URL source already does, so retrieval / mount /
//! search / TOC stay untouched.
//!
//! The trait is intentionally read-oriented for v1: `push_document` is reserved
//! (default `Err`) for future bidirectional sync but not implemented. Webhook
//! subscription is optional (default no-op); the baseline is poll-based
//! `list_documents` + `fetch_document`.

use async_trait::async_trait;
use nomifun_common::{AppError, ConnectorCredentialId};
use serde::{Deserialize, Serialize};

/// A decrypted connector credential, ready to authenticate against the remote.
/// `payload` is the connector-specific JSON (e.g. `{ "app_id", "app_secret" }`
/// for Feishu) decrypted by the service layer from `connector_credentials`.
#[derive(Debug, Clone)]
pub struct ConnectorCredential {
    /// Present only for a persisted credential. Validation probes are not
    /// durable entities and therefore carry no synthetic/empty identifier.
    pub credential_id: Option<ConnectorCredentialId>,
    pub kind: String,
    pub name: String,
    pub payload: serde_json::Value,
}

/// Identity returned by a successful credential validation (for the UI's
/// "test connection" affordance).
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ConnectorIdentity {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tenant_name: Option<String>,
    #[serde(default)]
    pub scopes_available: Vec<String>,
}

/// Connector-specific scope of what to sync, e.g. a Feishu wiki space
/// (`{ "type": "wiki_space", "space_id": "..." }`) or a Notion page list.
#[derive(Debug, Clone, Default)]
pub struct ConnectorScope(pub serde_json::Value);

/// Incremental-sync cursor. `last_sync_at` drives modified-since filtering;
/// `opaque` carries connector-specific paging/state across runs.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SyncCursor {
    #[serde(default)]
    pub last_sync_at: Option<i64>,
    #[serde(default)]
    pub opaque: serde_json::Value,
}

/// A reference to a remote document discovered by `list_documents` (metadata
/// only; the body is fetched lazily by `fetch_document`).
#[derive(Debug, Clone)]
pub struct RemoteDocRef {
    pub remote_id: String,
    pub title: String,
    /// Last-edit time (epoch ms) for incremental filtering.
    pub edit_time: i64,
    pub doc_type: String,
}

/// One page of a paginated `list_documents` call.
#[derive(Debug, Clone, Default)]
pub struct SyncPage {
    pub docs: Vec<RemoteDocRef>,
    /// Remote ids that disappeared since the cursor (moved to `_trash/`).
    pub deleted_ids: Vec<String>,
    pub next_page_token: Option<String>,
    pub updated_cursor: SyncCursor,
}

/// A fetched remote document converted to markdown, ready to snapshot.
#[derive(Debug, Clone)]
pub struct FetchedConnectorDoc {
    pub remote_id: String,
    pub title: String,
    pub markdown: String,
    pub edit_time: i64,
    /// Canonical web URL for the snapshot frontmatter (if any).
    pub source_url: Option<String>,
}

/// Result of registering a webhook (push-based sync). Optional capability.
#[derive(Debug, Clone)]
pub struct WebhookSubscription {
    pub subscription_id: String,
    pub expires_at: Option<i64>,
}

/// Reserved for future bidirectional sync (`push_document`).
#[derive(Debug, Clone)]
pub struct PushDocumentRequest {
    pub remote_id: String,
    pub markdown: String,
}

/// A pluggable source connector. Implementors own the remote API + format
/// conversion; the sync coordinator drives them and writes snapshots.
#[async_trait]
pub trait KnowledgeConnector: Send + Sync {
    /// Discriminator stored in `extra.source.kind` (e.g. "feishu").
    fn kind(&self) -> &'static str;

    /// Validate credentials; returns connector identity. Used at credential
    /// registration time and the UI "test connection" action.
    async fn validate_credentials(&self, cred: &ConnectorCredential) -> Result<ConnectorIdentity, AppError>;

    /// Enumerate documents in `scope`, paginated. With a populated `cursor`,
    /// returns only docs changed since `last_sync_at` (incremental); empty
    /// cursor = full sync. Removed docs are reported in `deleted_ids`.
    async fn list_documents(
        &self,
        cred: &ConnectorCredential,
        scope: &ConnectorScope,
        cursor: &SyncCursor,
        page_token: Option<&str>,
    ) -> Result<SyncPage, AppError>;

    /// Fetch one document and convert it to markdown (connector owns the
    /// format conversion, e.g. Feishu blocks → md via `feishu_md`).
    async fn fetch_document(&self, cred: &ConnectorCredential, doc: &RemoteDocRef) -> Result<FetchedConnectorDoc, AppError>;

    /// Optional push-based sync. Default: poll-only (no webhook).
    async fn subscribe_webhook(
        &self,
        _cred: &ConnectorCredential,
        _scope: &ConnectorScope,
        _callback_url: &str,
    ) -> Result<Option<WebhookSubscription>, AppError> {
        Ok(None)
    }

    /// Reserved for future bidirectional sync. Default: unsupported.
    async fn push_document(&self, _cred: &ConnectorCredential, _doc: &PushDocumentRequest) -> Result<(), AppError> {
        Err(AppError::BadRequest("push_document is not supported by this connector".into()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    struct MockConnector;

    #[async_trait]
    impl KnowledgeConnector for MockConnector {
        fn kind(&self) -> &'static str {
            "mock"
        }
        async fn validate_credentials(&self, _cred: &ConnectorCredential) -> Result<ConnectorIdentity, AppError> {
            Ok(ConnectorIdentity { tenant_name: Some("Acme".into()), scopes_available: vec!["wiki".into()] })
        }
        async fn list_documents(
            &self,
            _cred: &ConnectorCredential,
            _scope: &ConnectorScope,
            _cursor: &SyncCursor,
            _page_token: Option<&str>,
        ) -> Result<SyncPage, AppError> {
            Ok(SyncPage {
                docs: vec![RemoteDocRef {
                    remote_id: "d1".into(),
                    title: "Doc One".into(),
                    edit_time: 100,
                    doc_type: "docx".into(),
                }],
                ..Default::default()
            })
        }
        async fn fetch_document(&self, _cred: &ConnectorCredential, doc: &RemoteDocRef) -> Result<FetchedConnectorDoc, AppError> {
            Ok(FetchedConnectorDoc {
                remote_id: doc.remote_id.clone(),
                title: doc.title.clone(),
                markdown: format!("# {}\n\nbody", doc.title),
                edit_time: doc.edit_time,
                source_url: None,
            })
        }
    }

    #[tokio::test]
    async fn trait_is_object_safe_and_defaults_apply() {
        let c: Arc<dyn KnowledgeConnector> = Arc::new(MockConnector);
        assert_eq!(c.kind(), "mock");
        let cred = ConnectorCredential {
            credential_id: None,
            kind: "mock".into(),
            name: "n".into(),
            payload: serde_json::json!({}),
        };
        assert_eq!(c.validate_credentials(&cred).await.unwrap().tenant_name.as_deref(), Some("Acme"));
        let page = c.list_documents(&cred, &ConnectorScope::default(), &SyncCursor::default(), None).await.unwrap();
        assert_eq!(page.docs.len(), 1);
        let doc = c.fetch_document(&cred, &page.docs[0]).await.unwrap();
        assert!(doc.markdown.contains("Doc One"));
        // Default capabilities.
        assert!(c.subscribe_webhook(&cred, &ConnectorScope::default(), "http://cb").await.unwrap().is_none());
        assert!(c.push_document(&cred, &PushDocumentRequest { remote_id: "d1".into(), markdown: "x".into() }).await.is_err());
    }
}

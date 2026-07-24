//! Feishu (Lark) knowledge connector — syncs documents from a Feishu wiki space
//! into a managed knowledge base using Feishu's Open API with a self-built app
//! (tenant_access_token, no OAuth redirect).
//!
//! # Architecture
//! - Token caching with 30-minute safety margin, automatic refresh on 401.
//! - Rate limiting: minimum 250ms between requests (≈4 QPS), 429 retry with
//!   `x-ogw-ratelimit-reset` header.
//! - Configurable `base_url` for wiremock test injection.

use std::sync::Arc;
use std::time::{Duration, Instant};

use nomifun_common::AppError;
use reqwest::Client;
use serde::Deserialize;
use serde_json::Value;
use tokio::sync::Mutex;
use tracing::warn;

use crate::connector::{
    ConnectorCredential, ConnectorIdentity, ConnectorScope, FetchedConnectorDoc,
    KnowledgeConnector, RemoteDocRef, SyncCursor, SyncPage,
};
use crate::feishu_md::blocks_to_markdown;

/// Safety margin subtracted from the token's stated expiry to avoid using a
/// nearly-expired token.
const TOKEN_SAFETY_MARGIN: Duration = Duration::from_secs(30 * 60);

/// Minimum interval between outgoing HTTP requests (rate limit).
const MIN_REQUEST_INTERVAL: Duration = Duration::from_millis(250);

// ─── Token cache ────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
struct CachedToken {
    token: String,
    expires_at: Instant,
}

// ─── Internal API response structures ───────────────────────────────────────

#[derive(Debug, Deserialize)]
struct FeishuEnvelope<T> {
    code: i64,
    msg: String,
    data: Option<T>,
}

#[derive(Debug, Deserialize)]
struct TokenData {
    tenant_access_token: String,
    expire: i64, // seconds
}

#[derive(Debug, Deserialize)]
struct WikiNodesData {
    items: Option<Vec<WikiNode>>,
    has_more: Option<bool>,
    page_token: Option<String>,
}

#[derive(Debug, Deserialize)]
#[allow(dead_code)]
struct WikiNode {
    node_token: Option<String>,
    obj_token: Option<String>,
    obj_type: Option<String>,
    title: Option<String>,
    obj_edit_time: Option<String>,
}

#[derive(Debug, Deserialize)]
struct DocBlocksData {
    items: Option<Vec<Value>>,
    has_more: Option<bool>,
    page_token: Option<String>,
}

// ─── Connector ──────────────────────────────────────────────────────────────

/// Feishu knowledge connector.
pub struct FeishuConnector {
    base_url: String,
    client: Client,
    token_cache: Arc<Mutex<Option<CachedToken>>>,
    last_request: Arc<Mutex<Option<Instant>>>,
}

impl FeishuConnector {
    /// Create a connector with the default Feishu base URL.
    pub fn new() -> Self {
        Self::with_base_url("https://open.feishu.cn".to_string())
    }

    /// Create a connector with a custom base URL (for testing with wiremock).
    pub fn with_base_url(base_url: String) -> Self {
        Self::with_client(base_url, Client::new())
    }

    fn with_client(base_url: String, client: Client) -> Self {
        Self {
            base_url,
            client,
            token_cache: Arc::new(Mutex::new(None)),
            last_request: Arc::new(Mutex::new(None)),
        }
    }

    /// Extract app_id and app_secret from credential payload.
    fn parse_credential(credential: &ConnectorCredential) -> Result<(String, String), AppError> {
        let app_id = credential
            .payload
            .get("app_id")
            .and_then(|v| v.as_str())
            .ok_or_else(|| AppError::BadRequest("missing app_id in credential payload".into()))?
            .to_string();
        let app_secret = credential
            .payload
            .get("app_secret")
            .and_then(|v| v.as_str())
            .ok_or_else(|| {
                AppError::BadRequest("missing app_secret in credential payload".into())
            })?
            .to_string();
        Ok((app_id, app_secret))
    }

    /// Extract space_id from scope.
    fn parse_scope(scope: &ConnectorScope) -> Result<String, AppError> {
        scope
            .0
            .get("space_id")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
            .ok_or_else(|| AppError::BadRequest("missing space_id in scope".into()))
    }

    /// Acquire a valid tenant_access_token, fetching a new one if needed.
    async fn get_token(&self, credential: &ConnectorCredential) -> Result<String, AppError> {
        // Check cache first
        {
            let cache = self.token_cache.lock().await;
            if let Some(ref cached) = *cache {
                if Instant::now() < cached.expires_at {
                    return Ok(cached.token.clone());
                }
            }
        }
        // Fetch new token
        self.fetch_token(credential).await
    }

    /// Fetch a new tenant_access_token from Feishu and cache it.
    async fn fetch_token(&self, credential: &ConnectorCredential) -> Result<String, AppError> {
        let (app_id, app_secret) = Self::parse_credential(credential)?;

        self.rate_limit().await;

        let url = format!(
            "{}/open-apis/auth/v3/tenant_access_token/internal",
            self.base_url
        );
        let body = serde_json::json!({
            "app_id": app_id,
            "app_secret": app_secret,
        });

        let resp = self
            .client
            .post(&url)
            .json(&body)
            .send()
            .await
            .map_err(|e| AppError::BadGateway(format!("feishu token request failed: {e}")))?;

        let status = resp.status();
        let text = resp
            .text()
            .await
            .map_err(|e| AppError::BadGateway(format!("feishu token read body: {e}")))?;

        if !status.is_success() {
            return Err(AppError::BadGateway(format!(
                "feishu token endpoint returned {status}: {text}"
            )));
        }

        let envelope: FeishuEnvelope<TokenData> = serde_json::from_str(&text).map_err(|e| {
            AppError::BadGateway(format!("feishu token parse error: {e}, body: {text}"))
        })?;

        if envelope.code != 0 {
            return Err(AppError::BadGateway(format!(
                "feishu token error code={}, msg={}",
                envelope.code, envelope.msg
            )));
        }

        let data = envelope
            .data
            .ok_or_else(|| AppError::BadGateway("feishu token response missing data".into()))?;

        let expires_at = Instant::now()
            + Duration::from_secs(data.expire.max(0) as u64)
            - TOKEN_SAFETY_MARGIN.min(Duration::from_secs(data.expire.max(0) as u64));

        let token = data.tenant_access_token;

        // Cache it
        {
            let mut cache = self.token_cache.lock().await;
            *cache = Some(CachedToken {
                token: token.clone(),
                expires_at,
            });
        }

        Ok(token)
    }

    /// Invalidate the cached token (used on 401 retry).
    async fn invalidate_token(&self) {
        let mut cache = self.token_cache.lock().await;
        *cache = None;
    }

    /// Enforce minimum interval between requests.
    async fn rate_limit(&self) {
        let mut last = self.last_request.lock().await;
        if let Some(prev) = *last {
            let elapsed = prev.elapsed();
            if elapsed < MIN_REQUEST_INTERVAL {
                tokio::time::sleep(MIN_REQUEST_INTERVAL - elapsed).await;
            }
        }
        *last = Some(Instant::now());
    }

    /// Make an authenticated GET request with bounded retries: token refresh on
    /// 401 and rate-limit back-off on 429, up to `MAX_ATTEMPTS` total. A second
    /// transient failure no longer aborts the whole sync (the previous code
    /// retried each condition exactly once, then surfaced any further 429/401
    /// as a hard error).
    async fn authed_get(
        &self,
        credential: &ConnectorCredential,
        url: &str,
    ) -> Result<String, AppError> {
        const MAX_ATTEMPTS: u32 = 3;
        const MAX_BACKOFF_SECS: u64 = 30;

        let mut token = self.get_token(credential).await?;
        let mut attempt = 0u32;
        loop {
            attempt += 1;
            self.rate_limit().await;

            let resp = self
                .client
                .get(url)
                .header("Authorization", format!("Bearer {token}"))
                .send()
                .await
                .map_err(|e| AppError::BadGateway(format!("feishu request failed: {e}")))?;

            let status = resp.status();

            // 429 rate limit — back off (capped) and retry while attempts remain.
            if status.as_u16() == 429 && attempt < MAX_ATTEMPTS {
                let reset_secs = resp
                    .headers()
                    .get("x-ogw-ratelimit-reset")
                    .and_then(|v| v.to_str().ok())
                    .and_then(|s| s.parse::<u64>().ok())
                    .unwrap_or(1)
                    .min(MAX_BACKOFF_SECS);
                warn!("feishu 429 rate limit (attempt {attempt}/{MAX_ATTEMPTS}), sleeping {reset_secs}s");
                tokio::time::sleep(Duration::from_secs(reset_secs)).await;
                continue;
            }

            // 401 — invalidate + refresh the token and retry while attempts remain.
            if status.as_u16() == 401 && attempt < MAX_ATTEMPTS {
                warn!("feishu 401 unauthorized (attempt {attempt}/{MAX_ATTEMPTS}), refreshing token");
                self.invalidate_token().await;
                token = self.fetch_token(credential).await?;
                continue;
            }

            let text = resp
                .text()
                .await
                .map_err(|e| AppError::BadGateway(format!("feishu response body: {e}")))?;

            if !status.is_success() {
                return Err(AppError::BadGateway(format!(
                    "feishu returned {status}: {text}"
                )));
            }

            return Ok(text);
        }
    }

    /// Parse a Feishu API envelope, checking `code == 0`.
    fn parse_envelope<T: serde::de::DeserializeOwned>(text: &str) -> Result<T, AppError> {
        let envelope: FeishuEnvelope<T> = serde_json::from_str(text).map_err(|e| {
            AppError::BadGateway(format!("feishu parse error: {e}, body: {text}"))
        })?;
        if envelope.code != 0 {
            return Err(AppError::BadGateway(format!(
                "feishu error code={}, msg={}",
                envelope.code, envelope.msg
            )));
        }
        envelope
            .data
            .ok_or_else(|| AppError::BadGateway("feishu response missing data field".into()))
    }
}

#[async_trait::async_trait]
impl KnowledgeConnector for FeishuConnector {
    fn kind(&self) -> &'static str {
        "feishu"
    }

    fn source_url_for_remote_id(&self, remote_id: &str) -> Option<String> {
        Some(format!("https://open.feishu.cn/docx/{remote_id}"))
    }

    async fn validate_credentials(
        &self,
        credential: &ConnectorCredential,
    ) -> Result<ConnectorIdentity, AppError> {
        // Fetching a token proves the app_id/secret are valid.
        self.invalidate_token().await;
        self.fetch_token(credential).await?;
        Ok(ConnectorIdentity {
            tenant_name: None,
            scopes_available: vec!["wiki".into()],
        })
    }

    async fn list_documents(
        &self,
        credential: &ConnectorCredential,
        scope: &ConnectorScope,
        cursor: &SyncCursor,
        page_token: Option<&str>,
    ) -> Result<SyncPage, AppError> {
        let space_id = Self::parse_scope(scope)?;

        let mut url = format!(
            "{}/open-apis/wiki/v2/spaces/{}/nodes?page_size=50",
            self.base_url, space_id
        );
        if let Some(pt) = page_token {
            url.push_str(&format!("&page_token={pt}"));
        }

        let text = self.authed_get(credential, &url).await?;
        let data: WikiNodesData = Self::parse_envelope(&text)?;

        let items = data.items.unwrap_or_default();
        let mut docs = Vec::new();

        for node in items {
            // Only keep docx nodes
            let obj_type = node.obj_type.as_deref().unwrap_or("");
            if obj_type != "docx" {
                continue;
            }

            let obj_token = match node.obj_token {
                Some(ref t) if !t.is_empty() => t.clone(),
                _ => continue,
            };

            let title = node.title.unwrap_or_default();
            let edit_time: i64 = node
                .obj_edit_time
                .as_deref()
                .unwrap_or("0")
                .parse()
                .unwrap_or(0);

            // Incremental filtering: skip docs older than last_sync_at
            if let Some(last_sync) = cursor.last_sync_at {
                if edit_time <= last_sync {
                    continue;
                }
            }

            docs.push(RemoteDocRef {
                remote_id: obj_token,
                title,
                edit_time,
                doc_type: "docx".to_string(),
            });
        }

        let next_page_token = if data.has_more.unwrap_or(false) {
            data.page_token
        } else {
            None
        };

        // The updated cursor captures the max edit_time seen so far
        let max_edit = docs.iter().map(|d| d.edit_time).max();
        let updated_last_sync = match (cursor.last_sync_at, max_edit) {
            (Some(prev), Some(new)) => Some(prev.max(new)),
            (None, Some(new)) => Some(new),
            (prev, None) => prev,
        };

        Ok(SyncPage {
            docs,
            deleted_ids: Vec::new(),
            next_page_token,
            updated_cursor: SyncCursor {
                last_sync_at: updated_last_sync,
                opaque: serde_json::Value::Null,
            },
        })
    }

    async fn fetch_document(
        &self,
        credential: &ConnectorCredential,
        doc: &RemoteDocRef,
    ) -> Result<FetchedConnectorDoc, AppError> {
        let remote_id = doc.remote_id.as_str();
        // Paginate all blocks
        let mut all_blocks: Vec<Value> = Vec::new();
        let mut page_token: Option<String> = None;

        loop {
            let mut url = format!(
                "{}/open-apis/docx/v1/documents/{}/blocks?page_size=500",
                self.base_url, remote_id
            );
            if let Some(ref pt) = page_token {
                url.push_str(&format!("&page_token={pt}"));
            }

            let text = self.authed_get(credential, &url).await?;
            let data: DocBlocksData = Self::parse_envelope(&text)?;

            if let Some(items) = data.items {
                all_blocks.extend(items);
            }

            // Advance only on an explicit non-empty next token. A malformed
            // response (`has_more=true` but missing/empty page_token) would
            // otherwise re-request the first page forever (each iteration also
            // sleeps on the rate limiter, so it hangs the whole sync).
            match data.page_token {
                Some(tok) if data.has_more.unwrap_or(false) && !tok.is_empty() => {
                    page_token = Some(tok);
                }
                _ => break,
            }
        }

        let markdown = blocks_to_markdown(&all_blocks);

        // Best-effort source URL (uses the default feishu host for the doc link,
        // not the base_url which may be a test server).
        let source_url = self
            .source_url_for_remote_id(remote_id)
            .expect("Feishu document URLs are deterministic");

        Ok(FetchedConnectorDoc {
            remote_id: doc.remote_id.clone(),
            title: doc.title.clone(),
            markdown,
            edit_time: doc.edit_time,
            source_url: Some(source_url),
        })
    }
}

// ─── Tests ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use wiremock::matchers::{method, path, query_param};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn test_credential() -> ConnectorCredential {
        ConnectorCredential {
            credential_id: None,
            kind: "feishu".into(),
            name: "test".into(),
            payload: json!({
                "app_id": "cli_test123",
                "app_secret": "secret_abc"
            }),
        }
    }

    fn test_scope() -> ConnectorScope {
        ConnectorScope(json!({"space_id": "space_xyz"}))
    }

    fn test_connector(server: &MockServer) -> FeishuConnector {
        FeishuConnector::with_client(
            server.uri(),
            Client::builder().no_proxy().build().expect("test http client"),
        )
    }

    fn token_response() -> Value {
        json!({
            "code": 0,
            "msg": "ok",
            "data": {
                "tenant_access_token": "t-fake-token-abc",
                "expire": 7200
            }
        })
    }

    fn wiki_nodes_response() -> Value {
        json!({
            "code": 0,
            "msg": "ok",
            "data": {
                "items": [
                    {
                        "node_token": "node1",
                        "obj_token": "doc_obj_1",
                        "obj_type": "docx",
                        "title": "Design Doc",
                        "obj_edit_time": "1700000000000"
                    },
                    {
                        "node_token": "node2",
                        "obj_token": "sheet_obj_2",
                        "obj_type": "sheet",
                        "title": "Budget Sheet",
                        "obj_edit_time": "1700000001000"
                    },
                    {
                        "node_token": "node3",
                        "obj_token": "doc_obj_3",
                        "obj_type": "docx",
                        "title": "API Reference",
                        "obj_edit_time": "1700000002000"
                    }
                ],
                "has_more": false,
                "page_token": null
            }
        })
    }

    fn doc_blocks_response() -> Value {
        json!({
            "code": 0,
            "msg": "ok",
            "data": {
                "items": [
                    {
                        "block_id": "doc_root",
                        "parent_id": "",
                        "block_type": 1,
                        "children": ["blk_h", "blk_p"]
                    },
                    {
                        "block_id": "blk_h",
                        "parent_id": "doc_root",
                        "block_type": 3,
                        "children": [],
                        "heading1": {
                            "elements": [{"text_run": {"content": "Hello Feishu"}}]
                        }
                    },
                    {
                        "block_id": "blk_p",
                        "parent_id": "doc_root",
                        "block_type": 2,
                        "children": [],
                        "text": {
                            "elements": [{"text_run": {"content": "This is a test document."}}]
                        }
                    }
                ],
                "has_more": false,
                "page_token": null
            }
        })
    }

    async fn setup_token_mock(server: &MockServer) {
        Mock::given(method("POST"))
            .and(path("/open-apis/auth/v3/tenant_access_token/internal"))
            .respond_with(ResponseTemplate::new(200).set_body_json(token_response()))
            .mount(server)
            .await;
    }

    #[tokio::test]
    async fn test_validate_credentials_success() {
        let server = MockServer::start().await;
        setup_token_mock(&server).await;

        let connector = test_connector(&server);
        let cred = test_credential();

        let identity = connector.validate_credentials(&cred).await.unwrap();
        assert!(identity.tenant_name.is_none());
        assert_eq!(identity.scopes_available, vec!["wiki"]);
    }

    #[tokio::test]
    async fn test_validate_credentials_bad_secret() {
        let server = MockServer::start().await;

        Mock::given(method("POST"))
            .and(path("/open-apis/auth/v3/tenant_access_token/internal"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "code": 10003,
                "msg": "app_secret is invalid",
                "data": null
            })))
            .mount(&server)
            .await;

        let connector = test_connector(&server);
        let cred = test_credential();

        let err = connector.validate_credentials(&cred).await.unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("10003"), "expected code in error: {msg}");
        assert!(msg.contains("app_secret is invalid"), "expected msg: {msg}");
    }

    #[tokio::test]
    async fn test_list_documents_filters_non_docx() {
        let server = MockServer::start().await;
        setup_token_mock(&server).await;

        Mock::given(method("GET"))
            .and(path("/open-apis/wiki/v2/spaces/space_xyz/nodes"))
            .and(query_param("page_size", "50"))
            .respond_with(ResponseTemplate::new(200).set_body_json(wiki_nodes_response()))
            .mount(&server)
            .await;

        let connector = test_connector(&server);
        let cred = test_credential();
        let scope = test_scope();
        let cursor = SyncCursor::default();

        let page = connector
            .list_documents(&cred, &scope, &cursor, None)
            .await
            .unwrap();

        // Should have 2 docx docs, not the sheet
        assert_eq!(page.docs.len(), 2);
        assert_eq!(page.docs[0].remote_id, "doc_obj_1");
        assert_eq!(page.docs[0].title, "Design Doc");
        assert_eq!(page.docs[0].edit_time, 1700000000000);
        assert_eq!(page.docs[0].doc_type, "docx");

        assert_eq!(page.docs[1].remote_id, "doc_obj_3");
        assert_eq!(page.docs[1].title, "API Reference");
        assert_eq!(page.docs[1].edit_time, 1700000002000);

        assert!(page.next_page_token.is_none());
        assert!(page.deleted_ids.is_empty());
    }

    #[tokio::test]
    async fn test_list_documents_incremental_cursor() {
        let server = MockServer::start().await;
        setup_token_mock(&server).await;

        Mock::given(method("GET"))
            .and(path("/open-apis/wiki/v2/spaces/space_xyz/nodes"))
            .respond_with(ResponseTemplate::new(200).set_body_json(wiki_nodes_response()))
            .mount(&server)
            .await;

        let connector = test_connector(&server);
        let cred = test_credential();
        let scope = test_scope();

        // Set cursor to filter out doc_obj_1 (edit_time 1700000000000)
        let cursor = SyncCursor {
            last_sync_at: Some(1700000000000),
            opaque: serde_json::Value::Null,
        };

        let page = connector
            .list_documents(&cred, &scope, &cursor, None)
            .await
            .unwrap();

        // Only doc_obj_3 (edit_time 1700000002000 > 1700000000000) should remain
        assert_eq!(page.docs.len(), 1);
        assert_eq!(page.docs[0].remote_id, "doc_obj_3");
        assert_eq!(page.docs[0].edit_time, 1700000002000);

        // Updated cursor should reflect the max
        assert_eq!(page.updated_cursor.last_sync_at, Some(1700000002000));
    }

    #[tokio::test]
    async fn test_fetch_document_converts_blocks() {
        let server = MockServer::start().await;
        setup_token_mock(&server).await;

        Mock::given(method("GET"))
            .and(path("/open-apis/docx/v1/documents/doc_obj_1/blocks"))
            .and(query_param("page_size", "500"))
            .respond_with(ResponseTemplate::new(200).set_body_json(doc_blocks_response()))
            .mount(&server)
            .await;

        let connector = test_connector(&server);
        let cred = test_credential();

        let doc = connector
            .fetch_document(
                &cred,
                &RemoteDocRef {
                    remote_id: "doc_obj_1".into(),
                    title: "Hello Feishu".into(),
                    edit_time: 1700000000000,
                    doc_type: "docx".into(),
                },
            )
            .await
            .unwrap();

        assert_eq!(doc.remote_id, "doc_obj_1");
        assert_eq!(doc.edit_time, 1700000000000);
        assert!(doc.markdown.contains("# Hello Feishu"));
        assert!(doc.markdown.contains("This is a test document."));
        assert!(doc.source_url.as_deref().unwrap_or("").contains("doc_obj_1"));
    }

    #[tokio::test]
    async fn test_error_code_surfaces_app_error() {
        let server = MockServer::start().await;
        setup_token_mock(&server).await;

        Mock::given(method("GET"))
            .and(path("/open-apis/wiki/v2/spaces/space_xyz/nodes"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "code": 99991,
                "msg": "internal server error from feishu",
                "data": null
            })))
            .mount(&server)
            .await;

        let connector = test_connector(&server);
        let cred = test_credential();
        let scope = test_scope();
        let cursor = SyncCursor::default();

        let err = connector
            .list_documents(&cred, &scope, &cursor, None)
            .await
            .unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("99991"), "error should contain code: {msg}");
        assert!(
            msg.contains("internal server error from feishu"),
            "error should contain msg: {msg}"
        );
    }

    #[tokio::test]
    async fn test_token_cache_reuses_token() {
        let server = MockServer::start().await;

        // Set up token mock with expect(1) — should only be called once
        Mock::given(method("POST"))
            .and(path("/open-apis/auth/v3/tenant_access_token/internal"))
            .respond_with(ResponseTemplate::new(200).set_body_json(token_response()))
            .expect(1)
            .mount(&server)
            .await;

        Mock::given(method("GET"))
            .and(path("/open-apis/wiki/v2/spaces/space_xyz/nodes"))
            .respond_with(ResponseTemplate::new(200).set_body_json(wiki_nodes_response()))
            .mount(&server)
            .await;

        let connector = test_connector(&server);
        let cred = test_credential();
        let scope = test_scope();
        let cursor = SyncCursor::default();

        // First call — fetches token
        let _page1 = connector
            .list_documents(&cred, &scope, &cursor, None)
            .await
            .unwrap();

        // Second call — should reuse cached token (no second POST to token endpoint)
        let _page2 = connector
            .list_documents(&cred, &scope, &cursor, None)
            .await
            .unwrap();

        // wiremock will verify expect(1) on drop — if token was requested twice,
        // the test will panic with "Expected exactly 1 matching request, got 2"
    }

    #[tokio::test]
    async fn test_pagination_has_more() {
        let server = MockServer::start().await;
        setup_token_mock(&server).await;

        // First page with has_more=true
        Mock::given(method("GET"))
            .and(path("/open-apis/wiki/v2/spaces/space_xyz/nodes"))
            .and(query_param("page_size", "50"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "code": 0,
                "msg": "ok",
                "data": {
                    "items": [{
                        "node_token": "n1",
                        "obj_token": "d1",
                        "obj_type": "docx",
                        "title": "Page 1 Doc",
                        "obj_edit_time": "1700000000000"
                    }],
                    "has_more": true,
                    "page_token": "next_page_abc"
                }
            })))
            .mount(&server)
            .await;

        let connector = test_connector(&server);
        let cred = test_credential();
        let scope = test_scope();
        let cursor = SyncCursor::default();

        let page = connector
            .list_documents(&cred, &scope, &cursor, None)
            .await
            .unwrap();

        assert_eq!(page.docs.len(), 1);
        assert_eq!(page.next_page_token, Some("next_page_abc".to_string()));
    }

    #[tokio::test]
    async fn test_kind_returns_feishu() {
        let connector = FeishuConnector::new();
        assert_eq!(connector.kind(), "feishu");
    }

    #[tokio::test]
    async fn test_missing_app_id_errors() {
        let connector = FeishuConnector::new();
        let cred = ConnectorCredential {
            credential_id: None,
            kind: "feishu".into(),
            name: "bad".into(),
            payload: json!({"app_secret": "s"}),
        };
        let err = connector.validate_credentials(&cred).await.unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("app_id"), "should mention app_id: {msg}");
    }

    #[tokio::test]
    async fn test_missing_space_id_errors() {
        let server = MockServer::start().await;
        setup_token_mock(&server).await;

        let connector = test_connector(&server);
        let cred = test_credential();
        let bad_scope = ConnectorScope(json!({}));
        let cursor = SyncCursor::default();

        let err = connector
            .list_documents(&cred, &bad_scope, &cursor, None)
            .await
            .unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("space_id"), "should mention space_id: {msg}");
    }
}

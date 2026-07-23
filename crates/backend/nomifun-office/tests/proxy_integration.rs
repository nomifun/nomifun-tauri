use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use nomifun_api_types::WebSocketMessage;
use nomifun_office::{OfficeError, PreviewAccess};
use nomifun_office::proxy::{ProxyError, ProxyService};
use nomifun_office::types::DocType;
use nomifun_office::watch_manager::{OfficecliWatchManager, ProcessHandle, ProcessSpawner};
use nomifun_realtime::UserEventSink;
use tokio::io::AsyncWriteExt;
use tokio::net::TcpListener;

// ---------------------------------------------------------------------------
// Test infrastructure
// ---------------------------------------------------------------------------

struct MockProcessHandle {
    alive: AtomicBool,
}

impl MockProcessHandle {
    fn new() -> Self {
        Self {
            alive: AtomicBool::new(true),
        }
    }
}

impl ProcessHandle for MockProcessHandle {
    fn kill(&self) {
        self.alive.store(false, Ordering::SeqCst);
    }

    fn is_alive(&self) -> bool {
        self.alive.load(Ordering::SeqCst)
    }
}

struct HttpMockSpawner {
    response_template: String,
}

#[async_trait::async_trait]
impl ProcessSpawner for HttpMockSpawner {
    async fn spawn_officecli(
        &self,
        _file_path: &str,
        port: u16,
        _doc_type: DocType,
    ) -> Result<Box<dyn ProcessHandle>, OfficeError> {
        let resp = self.response_template.replace("__PORT__", &port.to_string());
        tokio::spawn(async move {
            let listener = TcpListener::bind(format!("127.0.0.1:{port}")).await.unwrap();
            for _ in 0..10 {
                if let Ok((mut stream, _)) = listener.accept().await {
                    let resp = resp.clone();
                    tokio::spawn(async move {
                        let mut buf = vec![0u8; 4096];
                        let _ = tokio::io::AsyncReadExt::read(&mut stream, &mut buf).await;
                        let _ = stream.write_all(resp.as_bytes()).await;
                        let _ = stream.shutdown().await;
                    });
                }
            }
        });
        Ok(Box::new(MockProcessHandle::new()))
    }

    async fn install_officecli(&self) -> Result<(), OfficeError> {
        Ok(())
    }

    async fn is_officecli_installed(&self) -> bool {
        true
    }

    async fn check_update(&self, _doc_type: DocType) -> Result<(), OfficeError> {
        Ok(())
    }
}

struct TcpOnlySpawner;

#[async_trait::async_trait]
impl ProcessSpawner for TcpOnlySpawner {
    async fn spawn_officecli(
        &self,
        _file_path: &str,
        port: u16,
        _doc_type: DocType,
    ) -> Result<Box<dyn ProcessHandle>, OfficeError> {
        let listener = std::net::TcpListener::bind(format!("127.0.0.1:{port}"))
            .map_err(|e| OfficeError::StartFailed(e.to_string()))?;
        std::mem::forget(listener);
        Ok(Box::new(MockProcessHandle::new()))
    }

    async fn install_officecli(&self) -> Result<(), OfficeError> {
        Ok(())
    }

    async fn is_officecli_installed(&self) -> bool {
        true
    }

    async fn check_update(&self, _doc_type: DocType) -> Result<(), OfficeError> {
        Ok(())
    }
}

struct NoopBroadcaster;

impl UserEventSink for NoopBroadcaster {
    fn send_to_user(&self, _user_id: &str, _event: WebSocketMessage<serde_json::Value>) {}
}

fn build_http_response(status: u16, headers: &[(&str, &str)], body: &str) -> String {
    let status_text = match status {
        200 => "OK",
        302 => "Found",
        404 => "Not Found",
        _ => "Unknown",
    };

    let mut resp = format!("HTTP/1.1 {status} {status_text}\r\n");
    for (k, v) in headers {
        resp.push_str(&format!("{k}: {v}\r\n"));
    }
    if !headers.iter().any(|(k, _)| k.to_lowercase() == "content-length") {
        resp.push_str(&format!("Content-Length: {}\r\n", body.len()));
    }
    resp.push_str("\r\n");
    resp.push_str(body);
    resp
}

async fn setup_proxy(
    doc_type: DocType,
    response_template: &str,
) -> (ProxyService, PreviewAccess, tempfile::TempDir) {
    let spawner = HttpMockSpawner {
        response_template: response_template.to_owned(),
    };
    let mgr = Arc::new(OfficecliWatchManager::new(Arc::new(spawner), Arc::new(NoopBroadcaster)));

    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("test.docx");
    std::fs::write(&file, b"test").unwrap();

    let access = mgr
        .start("0190f5fe-7c00-7a00-8abc-012345678901", file.to_str().unwrap(), doc_type)
        .await
        .unwrap();
    let proxy = ProxyService::new(mgr);

    (proxy, access, dir)
}

async fn setup_ssrf_proxy(doc_type: DocType) -> (ProxyService, PreviewAccess, tempfile::TempDir) {
    let mgr = Arc::new(OfficecliWatchManager::new(
        Arc::new(TcpOnlySpawner),
        Arc::new(NoopBroadcaster),
    ));

    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("test.docx");
    std::fs::write(&file, b"test").unwrap();

    let access = mgr
        .start("0190f5fe-7c00-7a00-8abc-012345678901", file.to_str().unwrap(), doc_type)
        .await
        .unwrap();
    let proxy = ProxyService::new(mgr);

    (proxy, access, dir)
}

// ---------------------------------------------------------------------------
// RP-2: PPT proxy SSRF protection — guessed capability rejected
// ---------------------------------------------------------------------------

#[tokio::test]
async fn rp2_ppt_proxy_ssrf_rejects_guessed_capability() {
    let (proxy, _access, _dir) = setup_ssrf_proxy(DocType::Ppt).await;

    let guessed = "0".repeat(64);
    let result = proxy.forward(&guessed, "/index.html", DocType::Ppt, &[]).await;

    let err = result.unwrap_err();
    assert!(matches!(err, ProxyError::InvalidCapability));
}

// ---------------------------------------------------------------------------
// RP-4: Office watch proxy rejects legacy port-only paths
// ---------------------------------------------------------------------------

#[tokio::test]
async fn rp4_office_watch_proxy_rejects_legacy_port_capability() {
    let (proxy, _access, _dir) = setup_ssrf_proxy(DocType::Word).await;

    let result = proxy.forward_watch("9999", "/", &[]).await;

    assert!(matches!(result.unwrap_err(), ProxyError::InvalidCapability));
}

// ---------------------------------------------------------------------------
// SSRF: wrong doc_type rejected even when port is active
// ---------------------------------------------------------------------------

#[tokio::test]
async fn ssrf_wrong_doc_type_rejected() {
    let (proxy, access, _dir) = setup_ssrf_proxy(DocType::Word).await;

    let result = proxy
        .forward(&access.capability, "/index.html", DocType::Ppt, &[])
        .await;

    assert!(matches!(result.unwrap_err(), ProxyError::InvalidCapability));
}

#[tokio::test]
async fn malformed_capability_is_rejected_without_upstream_access() {
    let (proxy, _access, _dir) = setup_ssrf_proxy(DocType::Word).await;

    let result = proxy
        .forward("not-a-capability", "/index.html", DocType::Word, &[])
        .await;

    assert!(matches!(result.unwrap_err(), ProxyError::InvalidCapability));
}

#[tokio::test]
async fn stopped_capability_is_revoked_before_proxying() {
    let mgr = Arc::new(OfficecliWatchManager::new(
        Arc::new(TcpOnlySpawner),
        Arc::new(NoopBroadcaster),
    ));
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("test.docx");
    std::fs::write(&file, b"test").unwrap();
    let path = file.to_string_lossy().into_owned();
    let access = mgr.start("0190f5fe-7c00-7a00-8abc-012345678901", &path, DocType::Word).await.unwrap();
    let proxy = ProxyService::new(Arc::clone(&mgr));

    mgr.stop("0190f5fe-7c00-7a00-8abc-012345678901", DocType::Word, &access.capability)
        .await;
    let result = proxy
        .forward(&access.capability, "/", DocType::Word, &[])
        .await;

    assert!(matches!(result.unwrap_err(), ProxyError::InvalidCapability));
}

// ---------------------------------------------------------------------------
// H-1-13.8 fix: forward_watch accepts Excel session ports
// ---------------------------------------------------------------------------

#[tokio::test]
async fn forward_watch_accepts_excel_session_capability() {
    let response = build_http_response(200, &[("Content-Type", "text/plain")], "Excel preview");
    let (proxy, access, _dir) = setup_proxy(DocType::Excel, &response).await;

    let result = proxy
        .forward_watch(&access.capability, "/", &[])
        .await
        .unwrap();

    assert_eq!(result.status, 200);
    let body = String::from_utf8(result.body).unwrap();
    assert!(body.contains("Excel preview"));
}

// ---------------------------------------------------------------------------
// forward_watch accepts Word session ports
// ---------------------------------------------------------------------------

#[tokio::test]
async fn forward_watch_accepts_word_session_capability() {
    let response = build_http_response(200, &[("Content-Type", "text/plain")], "Word preview");
    let (proxy, access, _dir) = setup_proxy(DocType::Word, &response).await;

    let result = proxy
        .forward_watch(&access.capability, "/", &[])
        .await
        .unwrap();

    assert_eq!(result.status, 200);
    let body = String::from_utf8(result.body).unwrap();
    assert!(body.contains("Word preview"));
}

// ---------------------------------------------------------------------------
// forward_watch rejects PPT session ports
// ---------------------------------------------------------------------------

#[tokio::test]
async fn forward_watch_rejects_ppt_session_capability() {
    let (proxy, access, _dir) = setup_ssrf_proxy(DocType::Ppt).await;

    let result = proxy.forward_watch(&access.capability, "/", &[]).await;

    assert!(matches!(result.unwrap_err(), ProxyError::InvalidCapability));
}

// ---------------------------------------------------------------------------
// RP-1 / RP-3: Proxy forwards plain text response
// ---------------------------------------------------------------------------

#[tokio::test]
async fn rp1_rp3_proxy_forwards_plain_text() {
    let response = build_http_response(200, &[("Content-Type", "text/plain")], "Hello from preview");
    let (proxy, access, _dir) = setup_proxy(DocType::Ppt, &response).await;

    let result = proxy
        .forward(&access.capability, "/index.html", DocType::Ppt, &[])
        .await
        .unwrap();

    assert_eq!(result.status, 200);
    let body = String::from_utf8(result.body).unwrap();
    assert!(body.contains("Hello from preview"));
}

// ---------------------------------------------------------------------------
// RP-5: HTML injection — navigation guard script injected
// ---------------------------------------------------------------------------

#[tokio::test]
async fn rp5_proxy_injects_navigation_guard_in_html() {
    let html_body = "<html><head><title>Preview</title></head><body>Content</body></html>";
    let response = build_http_response(200, &[("Content-Type", "text/html")], html_body);
    let (proxy, access, _dir) = setup_proxy(DocType::Word, &response).await;

    let result = proxy
        .forward(&access.capability, "/", DocType::Word, &[])
        .await
        .unwrap();

    assert_eq!(result.status, 200);
    let body = String::from_utf8(result.body).unwrap();
    assert!(body.contains("<script>"), "should inject navigation guard script");
    assert!(
        body.contains(&format!("'/api/office-watch-proxy/{}'", access.capability)),
        "guard should reference correct proxy base path"
    );
    assert!(
        body.contains("<title>Preview</title>"),
        "should preserve original HTML content"
    );
}

// ---------------------------------------------------------------------------
// RP-5b: Non-HTML response should NOT inject script
// ---------------------------------------------------------------------------

#[tokio::test]
async fn rp5b_proxy_does_not_inject_in_json() {
    let response = build_http_response(200, &[("Content-Type", "application/json")], r#"{"ok":true}"#);
    let (proxy, access, _dir) = setup_proxy(DocType::Ppt, &response).await;

    let result = proxy
        .forward(&access.capability, "/api/data", DocType::Ppt, &[])
        .await
        .unwrap();

    let body = String::from_utf8(result.body).unwrap();
    assert!(!body.contains("<script>"), "should not inject script in JSON responses");
}

// ---------------------------------------------------------------------------
// RP-7: Hop-by-hop header stripping
// ---------------------------------------------------------------------------

#[tokio::test]
async fn rp7_proxy_strips_hop_by_hop_headers() {
    let response = build_http_response(
        200,
        &[
            ("Content-Type", "text/plain"),
            ("Connection", "keep-alive"),
            ("Keep-Alive", "timeout=5"),
            ("Set-Cookie", "nomifun-session=attacker"),
            ("X-Frame-Options", "DENY"),
            ("X-Custom", "preserved"),
        ],
        "body",
    );
    let (proxy, access, _dir) = setup_proxy(DocType::Word, &response).await;

    let result = proxy
        .forward(&access.capability, "/", DocType::Word, &[])
        .await
        .unwrap();

    let header_names: Vec<&str> = result.headers.iter().map(|(k, _)| k.as_str()).collect();
    assert!(
        !header_names.contains(&"connection"),
        "connection header should be stripped"
    );
    assert!(
        !header_names.contains(&"keep-alive"),
        "keep-alive header should be stripped"
    );
    assert!(!header_names.contains(&"set-cookie"), "upstream must not inject app cookies");
    assert!(
        !header_names.contains(&"x-frame-options"),
        "the outer app policy owns iframe ancestors"
    );
    assert!(header_names.contains(&"x-custom"), "custom header should be preserved");
}

// ---------------------------------------------------------------------------
// Frame ancestor policy belongs to the outer security middleware
// ---------------------------------------------------------------------------

#[tokio::test]
async fn proxy_does_not_emit_a_conflicting_x_frame_options_policy() {
    let response = build_http_response(200, &[("Content-Type", "text/plain")], "body");
    let (proxy, access, _dir) = setup_proxy(DocType::Ppt, &response).await;

    let result = proxy
        .forward(&access.capability, "/", DocType::Ppt, &[])
        .await
        .unwrap();

    assert!(result.headers.iter().all(|(key, _)| key != "x-frame-options"));
}

// ---------------------------------------------------------------------------
// HTML content-length stripped after injection
// ---------------------------------------------------------------------------

#[tokio::test]
async fn proxy_removes_content_length_for_html() {
    let html_body = "<html><head></head><body></body></html>";
    let response = build_http_response(200, &[("Content-Type", "text/html")], html_body);
    let (proxy, access, _dir) = setup_proxy(DocType::Word, &response).await;

    let result = proxy
        .forward(&access.capability, "/", DocType::Word, &[])
        .await
        .unwrap();

    let has_cl = result.headers.iter().any(|(k, _)| k == "content-length");
    assert!(
        !has_cl,
        "content-length should be stripped for HTML responses (body size changed after injection)"
    );
}

// ---------------------------------------------------------------------------
// Non-HTML content-length preserved
// ---------------------------------------------------------------------------

#[tokio::test]
async fn proxy_preserves_content_length_for_non_html() {
    let response = build_http_response(200, &[("Content-Type", "application/json")], r#"{"ok":true}"#);
    let (proxy, access, _dir) = setup_proxy(DocType::Ppt, &response).await;

    let result = proxy
        .forward(&access.capability, "/api/data", DocType::Ppt, &[])
        .await
        .unwrap();

    let has_cl = result.headers.iter().any(|(k, _)| k == "content-length");
    assert!(has_cl, "content-length should be preserved for non-HTML");
}

// ---------------------------------------------------------------------------
// RP-6: Location rewriting
// ---------------------------------------------------------------------------

#[tokio::test]
async fn rp6_proxy_rewrites_location_header() {
    let response_template = build_http_response(
        302,
        &[
            ("Content-Type", "text/html"),
            ("Location", "http://localhost:__PORT__/new/path"),
        ],
        "",
    );
    let (proxy, access, _dir) = setup_proxy(DocType::Ppt, &response_template).await;

    let result = proxy
        .forward(&access.capability, "/old", DocType::Ppt, &[])
        .await
        .unwrap();

    assert_eq!(result.status, 302);
    let location = result
        .headers
        .iter()
        .find(|(k, _)| k == "location")
        .map(|(_, v)| v.as_str());
    assert_eq!(
        location,
        Some(format!("/api/ppt-proxy/{}/new/path", access.capability).as_str())
    );
}

// ---------------------------------------------------------------------------
// RP-6b: Location rewriting for root-relative paths
// ---------------------------------------------------------------------------

#[tokio::test]
async fn rp6b_proxy_rewrites_root_relative_location() {
    let response_template = build_http_response(302, &[("Content-Type", "text/html"), ("Location", "/redirected")], "");
    let (proxy, access, _dir) = setup_proxy(DocType::Word, &response_template).await;

    let result = proxy
        .forward(&access.capability, "/old", DocType::Word, &[])
        .await
        .unwrap();

    let location = result
        .headers
        .iter()
        .find(|(k, _)| k == "location")
        .map(|(_, v)| v.as_str());
    assert_eq!(
        location,
        Some(format!("/api/office-watch-proxy/{}/redirected", access.capability).as_str())
    );
}

// ---------------------------------------------------------------------------
// Proxy forwards 404 status correctly
// ---------------------------------------------------------------------------

#[tokio::test]
async fn proxy_forwards_404_status() {
    let response = build_http_response(404, &[("Content-Type", "text/plain")], "Not Found");
    let (proxy, access, _dir) = setup_proxy(DocType::Word, &response).await;

    let result = proxy
        .forward(&access.capability, "/missing", DocType::Word, &[])
        .await
        .unwrap();

    assert_eq!(result.status, 404);
}

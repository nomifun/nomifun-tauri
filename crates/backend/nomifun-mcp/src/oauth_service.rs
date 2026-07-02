use std::sync::Arc;
use std::time::Duration;

use nomifun_api_types::{OAuthLoginResponse, OAuthStatusResponse};
use nomifun_common::{TimestampMs, now_ms};
use nomifun_db::{IOAuthTokenRepository, UpsertOAuthTokenParams};
use oauth2::basic::BasicClient;
use oauth2::{
    AuthUrl, AuthorizationCode, ClientId, CsrfToken, PkceCodeChallenge, PkceCodeVerifier, RedirectUrl, RefreshToken,
    TokenResponse, TokenUrl,
};
use serde::Deserialize;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::Mutex;
use tracing::{debug, warn};

use crate::error::McpError;

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// Default timeout for the OAuth callback server waiting for the redirect.
const CALLBACK_TIMEOUT: Duration = Duration::from_secs(120);

/// Default OAuth client ID for MCP servers (public client, no secret).
const DEFAULT_CLIENT_ID: &str = "nomifun";

/// Token expiry safety margin (refresh 5 minutes before expiration).
const EXPIRY_MARGIN_MS: i64 = 5 * 60 * 1000;

// ---------------------------------------------------------------------------
// Discovery response
// ---------------------------------------------------------------------------

/// OAuth Authorization Server Metadata (RFC 8414) — subset of fields we need.
#[derive(Debug, Deserialize)]
struct OAuthServerMetadata {
    authorization_endpoint: String,
    token_endpoint: String,
}

// ---------------------------------------------------------------------------
// Pending login state
// ---------------------------------------------------------------------------

/// State held while waiting for the OAuth callback redirect.
///
/// Stores endpoint URLs rather than the typed `BasicClient` to avoid
/// complex generic type parameters from the `oauth2` crate.
struct PendingLogin {
    csrf_token: CsrfToken,
    pkce_verifier: PkceCodeVerifier,
    auth_url: String,
    token_url: String,
    redirect_url: String,
}

// ---------------------------------------------------------------------------
// McpOAuthService
// ---------------------------------------------------------------------------

/// Service for MCP server OAuth 2.0 PKCE authentication.
///
/// Manages the full lifecycle: discovery → authorize → callback → token
/// exchange → storage → refresh → logout.
#[derive(Clone)]
pub struct McpOAuthService {
    token_repo: Arc<dyn IOAuthTokenRepository>,
    http_client: HttpClientFactory,
    /// Mutex protecting the pending login state (only one login at a time).
    pending: Arc<Mutex<Option<PendingLogin>>>,
}

type HttpClientFactory = Arc<dyn Fn() -> reqwest::Client + Send + Sync>;

impl McpOAuthService {
    pub fn new(token_repo: Arc<dyn IOAuthTokenRepository>, http_client: reqwest::Client) -> Self {
        Self {
            token_repo,
            http_client: Arc::new(move || http_client.clone()),
            pending: Arc::new(Mutex::new(None)),
        }
    }

    pub fn new_dynamic(token_repo: Arc<dyn IOAuthTokenRepository>) -> Self {
        Self {
            token_repo,
            http_client: Arc::new(nomifun_net::http_client),
            pending: Arc::new(Mutex::new(None)),
        }
    }

    fn http_client(&self) -> reqwest::Client {
        (self.http_client)()
    }

    // -----------------------------------------------------------------------
    // Public API
    // -----------------------------------------------------------------------

    /// Check whether the given server URL has a valid (non-expired) OAuth token.
    pub async fn check_oauth_status(&self, server_url: &str) -> Result<OAuthStatusResponse, McpError> {
        let authenticated = self.has_valid_token(server_url).await?;
        Ok(OAuthStatusResponse { authenticated })
    }

    /// Start the OAuth PKCE login flow for the given MCP server URL.
    ///
    /// 1. Discover authorization/token endpoints
    /// 2. Generate PKCE challenge
    /// 3. Start local callback server on a random port
    /// 4. Build authorization URL and open it in the system browser
    /// 5. Wait for the redirect with the authorization code
    /// 6. Exchange code for tokens and persist them
    pub async fn login(&self, server_url: &str) -> Result<OAuthLoginResponse, McpError> {
        let (authorize_url, listener) = self.prepare_login_flow(server_url).await?;

        // Open browser.
        debug!(url = %authorize_url, "Opening browser for OAuth authorization");
        if let Err(e) = open::that(&authorize_url) {
            warn!("Failed to open browser: {e}");
        }

        // Wait for callback.
        let code = match self.wait_for_callback(listener).await {
            Ok(code) => code,
            Err(e) => {
                self.clear_pending().await;
                return Ok(OAuthLoginResponse {
                    success: false,
                    error: Some(e.to_string()),
                });
            }
        };

        // Exchange code for tokens.
        match self.exchange_code(server_url, code).await {
            Ok(()) => Ok(OAuthLoginResponse {
                success: true,
                error: None,
            }),
            Err(e) => {
                self.clear_pending().await;
                Ok(OAuthLoginResponse {
                    success: false,
                    error: Some(e.to_string()),
                })
            }
        }
    }

    /// Logout from the given MCP server URL (delete stored token).
    ///
    /// Idempotent: returns Ok even if no token was stored.
    pub async fn logout(&self, server_url: &str) -> Result<(), McpError> {
        match self.token_repo.delete(server_url).await {
            Ok(()) => {
                debug!(server_url, "OAuth token deleted");
                Ok(())
            }
            Err(nomifun_db::DbError::NotFound(_)) => {
                debug!(server_url, "No OAuth token to delete (idempotent)");
                Ok(())
            }
            Err(e) => Err(McpError::Database(e)),
        }
    }

    /// Return the list of server URLs that have stored OAuth tokens.
    pub async fn get_authenticated_servers(&self) -> Result<Vec<String>, McpError> {
        let urls = self.token_repo.list_authenticated_urls().await?;
        Ok(urls)
    }

    /// Get a valid access token for the given server URL.
    ///
    /// If the stored token is expired and a refresh token is available,
    /// automatically refreshes before returning.
    /// Returns `None` if no token is stored for this URL.
    pub async fn get_token(&self, server_url: &str) -> Result<Option<String>, McpError> {
        let row = match self.token_repo.get_by_url(server_url).await? {
            Some(row) => row,
            None => return Ok(None),
        };

        // Check if token is expired (with safety margin).
        if let Some(expires_at) = row.expires_at {
            let now = now_ms();
            if now >= expires_at - EXPIRY_MARGIN_MS
                && let Some(ref refresh_token) = row.refresh_token
            {
                match self.refresh_token(server_url, refresh_token).await {
                    Ok(new_token) => return Ok(Some(new_token)),
                    Err(e) => {
                        warn!(
                            server_url,
                            error = %e,
                            "Token refresh failed, returning expired token"
                        );
                    }
                }
            }
        }

        Ok(Some(row.access_token))
    }

    // -----------------------------------------------------------------------
    // Internal helpers
    // -----------------------------------------------------------------------

    /// Discover endpoints, build OAuth client, generate PKCE, bind callback
    /// server, store pending state, and return the authorization URL + listener.
    async fn prepare_login_flow(&self, server_url: &str) -> Result<(String, TcpListener), McpError> {
        let metadata = self.discover_endpoints(server_url).await?;

        let auth_url_str = metadata.authorization_endpoint.clone();
        let token_url_str = metadata.token_endpoint.clone();

        let auth_url = AuthUrl::new(metadata.authorization_endpoint)
            .map_err(|e| McpError::OAuth(format!("Invalid auth URL: {e}")))?;
        let token_url =
            TokenUrl::new(metadata.token_endpoint).map_err(|e| McpError::OAuth(format!("Invalid token URL: {e}")))?;

        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .map_err(|e| McpError::OAuth(format!("Failed to bind callback server: {e}")))?;
        let callback_port = listener
            .local_addr()
            .map_err(|e| McpError::OAuth(format!("Failed to get callback port: {e}")))?
            .port();

        let redirect_url_str = format!("http://127.0.0.1:{callback_port}/callback");
        let redirect = RedirectUrl::new(redirect_url_str.clone())
            .map_err(|e| McpError::OAuth(format!("Invalid redirect URL: {e}")))?;

        let client = BasicClient::new(ClientId::new(DEFAULT_CLIENT_ID.to_string()))
            .set_auth_uri(auth_url)
            .set_token_uri(token_url)
            .set_redirect_uri(redirect);

        let (pkce_challenge, pkce_verifier) = PkceCodeChallenge::new_random_sha256();

        let (authorize_url, csrf_token) = client
            .authorize_url(CsrfToken::new_random)
            .set_pkce_challenge(pkce_challenge)
            .url();

        {
            let mut pending = self.pending.lock().await;
            *pending = Some(PendingLogin {
                csrf_token,
                pkce_verifier,
                auth_url: auth_url_str,
                token_url: token_url_str,
                redirect_url: redirect_url_str,
            });
        }

        Ok((authorize_url.to_string(), listener))
    }

    /// Check if a valid (non-expired) token exists for the URL.
    async fn has_valid_token(&self, server_url: &str) -> Result<bool, McpError> {
        let row = match self.token_repo.get_by_url(server_url).await? {
            Some(row) => row,
            None => return Ok(false),
        };

        if let Some(expires_at) = row.expires_at
            && now_ms() >= expires_at
        {
            return Ok(false);
        }

        Ok(true)
    }

    /// Discover OAuth authorization server metadata.
    ///
    /// Tries `.well-known/oauth-authorization-server` first,
    /// falls back to `.well-known/openid-configuration`.
    async fn discover_endpoints(&self, server_url: &str) -> Result<OAuthServerMetadata, McpError> {
        let base = server_url.trim_end_matches('/');

        let well_known_url = format!("{base}/.well-known/oauth-authorization-server");
        if let Ok(metadata) = self.fetch_metadata(&well_known_url).await {
            debug!(server_url, "Discovered OAuth metadata via RFC 8414");
            return Ok(metadata);
        }

        let oidc_url = format!("{base}/.well-known/openid-configuration");
        if let Ok(metadata) = self.fetch_metadata(&oidc_url).await {
            debug!(server_url, "Discovered OAuth metadata via OIDC");
            return Ok(metadata);
        }

        Err(McpError::OAuth(format!(
            "Failed to discover OAuth endpoints for '{server_url}': \
             no .well-known/oauth-authorization-server or \
             .well-known/openid-configuration found"
        )))
    }

    /// Fetch and parse OAuth server metadata from a URL.
    async fn fetch_metadata(&self, url: &str) -> Result<OAuthServerMetadata, McpError> {
        let client = self.http_client();
        let resp = client
            .get(url)
            .send()
            .await
            .map_err(|e| McpError::OAuth(format!("HTTP request failed: {e}")))?;

        if !resp.status().is_success() {
            return Err(McpError::OAuth(format!("Metadata endpoint returned {}", resp.status())));
        }

        resp.json()
            .await
            .map_err(|e| McpError::OAuth(format!("Failed to parse metadata: {e}")))
    }

    /// Wait for the OAuth callback redirect on the given listener.
    async fn wait_for_callback(&self, listener: TcpListener) -> Result<String, McpError> {
        let (code_tx, code_rx) = tokio::sync::oneshot::channel::<Result<String, McpError>>();
        let pending = self.pending.clone();

        tokio::spawn(async move {
            let result = Self::handle_callback_connection(listener, pending).await;
            let _ = code_tx.send(result);
        });

        match tokio::time::timeout(CALLBACK_TIMEOUT, code_rx).await {
            Ok(Ok(result)) => result,
            Ok(Err(_)) => Err(McpError::OAuth("Callback channel closed unexpectedly".to_string())),
            Err(_) => Err(McpError::OAuth(
                "OAuth callback timed out — no redirect received within 120s".to_string(),
            )),
        }
    }

    /// Handle a single HTTP connection on the callback server.
    async fn handle_callback_connection(
        listener: TcpListener,
        pending: Arc<Mutex<Option<PendingLogin>>>,
    ) -> Result<String, McpError> {
        let (mut stream, _) = listener
            .accept()
            .await
            .map_err(|e| McpError::OAuth(format!("Failed to accept connection: {e}")))?;

        let mut buf = vec![0u8; 4096];
        let n = stream
            .read(&mut buf)
            .await
            .map_err(|e| McpError::OAuth(format!("Failed to read request: {e}")))?;

        let request = String::from_utf8_lossy(&buf[..n]);
        let (code, state) = parse_callback_query(&request)?;

        // Validate CSRF state.
        let guard = pending.lock().await;
        let pending_login = guard
            .as_ref()
            .ok_or_else(|| McpError::OAuth("No pending login state".to_string()))?;

        if state != *pending_login.csrf_token.secret() {
            return Err(McpError::OAuth("CSRF state mismatch".to_string()));
        }

        // Send a success response to the browser.
        let response = "HTTP/1.1 200 OK\r\n\
            Content-Type: text/html; charset=utf-8\r\n\
            Connection: close\r\n\r\n\
            <html><body><h1>Authorization successful!</h1>\
            <p>You can close this window and return to Nomi.</p>\
            </body></html>";

        let _ = stream.write_all(response.as_bytes()).await;

        Ok(code)
    }

    /// Build a no-redirect reqwest client for OAuth token exchange.
    fn build_no_redirect_client() -> Result<reqwest::Client, McpError> {
        nomifun_net::proxy::apply_detected_proxy(reqwest::ClientBuilder::new())
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|e| McpError::OAuth(format!("Failed to build HTTP client: {e}")))
    }

    /// Exchange the authorization code for tokens and persist them.
    async fn exchange_code(&self, server_url: &str, code: String) -> Result<(), McpError> {
        let (auth_url_str, token_url_str, redirect_url_str, pkce_verifier) = {
            let mut guard = self.pending.lock().await;
            let pending = guard
                .take()
                .ok_or_else(|| McpError::OAuth("No pending login state".to_string()))?;
            (
                pending.auth_url,
                pending.token_url,
                pending.redirect_url,
                pending.pkce_verifier,
            )
        };

        let auth_url = AuthUrl::new(auth_url_str).map_err(|e| McpError::OAuth(format!("Invalid auth URL: {e}")))?;
        let token_url = TokenUrl::new(token_url_str).map_err(|e| McpError::OAuth(format!("Invalid token URL: {e}")))?;
        let redirect =
            RedirectUrl::new(redirect_url_str).map_err(|e| McpError::OAuth(format!("Invalid redirect URL: {e}")))?;

        let client = BasicClient::new(ClientId::new(DEFAULT_CLIENT_ID.to_string()))
            .set_auth_uri(auth_url)
            .set_token_uri(token_url)
            .set_redirect_uri(redirect);

        let http_client = Self::build_no_redirect_client()?;

        let token_result = client
            .exchange_code(AuthorizationCode::new(code))
            .set_pkce_verifier(pkce_verifier)
            .request_async(&http_client)
            .await
            .map_err(|e| McpError::OAuth(format!("Token exchange failed: {e}")))?;

        self.persist_token(server_url, &token_result).await?;
        debug!(server_url, "OAuth tokens stored successfully");
        Ok(())
    }

    /// Refresh an expired access token using the refresh token.
    async fn refresh_token(&self, server_url: &str, refresh_token_value: &str) -> Result<String, McpError> {
        let metadata = self.discover_endpoints(server_url).await?;
        let token_url =
            TokenUrl::new(metadata.token_endpoint).map_err(|e| McpError::OAuth(format!("Invalid token URL: {e}")))?;

        let client = BasicClient::new(ClientId::new(DEFAULT_CLIENT_ID.to_string())).set_token_uri(token_url);

        let http_client = Self::build_no_redirect_client()?;

        let refresh_token = RefreshToken::new(refresh_token_value.to_string());
        let token_result = client
            .exchange_refresh_token(&refresh_token)
            .request_async(&http_client)
            .await
            .map_err(|e| McpError::OAuth(format!("Token refresh failed: {e}")))?;

        let new_access_token = token_result.access_token().secret().clone();

        let expires_at: Option<TimestampMs> = token_result.expires_in().map(|d| now_ms() + d.as_millis() as i64);

        // Prefer new refresh_token if provided, otherwise keep the old one.
        let new_refresh = token_result
            .refresh_token()
            .map(|t| t.secret().as_str())
            .unwrap_or(refresh_token_value);

        self.token_repo
            .upsert(UpsertOAuthTokenParams {
                server_url,
                access_token: &new_access_token,
                refresh_token: Some(new_refresh),
                token_type: "bearer",
                expires_at,
            })
            .await?;

        debug!(server_url, "OAuth token refreshed successfully");
        Ok(new_access_token)
    }

    /// Persist token response to DB.
    async fn persist_token<TR: TokenResponse>(&self, server_url: &str, token_result: &TR) -> Result<(), McpError> {
        let expires_at: Option<TimestampMs> = token_result.expires_in().map(|d| now_ms() + d.as_millis() as i64);

        self.token_repo
            .upsert(UpsertOAuthTokenParams {
                server_url,
                access_token: token_result.access_token().secret(),
                refresh_token: token_result.refresh_token().map(|t| t.secret().as_str()),
                token_type: "bearer",
                expires_at,
            })
            .await?;

        Ok(())
    }

    /// Clear the pending login state.
    async fn clear_pending(&self) {
        let mut guard = self.pending.lock().await;
        *guard = None;
    }
}

// ---------------------------------------------------------------------------
// Query parameter parsing
// ---------------------------------------------------------------------------

/// Parse `code` and `state` from the first line of an HTTP request.
///
/// Expects: `GET /callback?code=xxx&state=yyy HTTP/1.1`
fn parse_callback_query(request: &str) -> Result<(String, String), McpError> {
    let first_line = request
        .lines()
        .next()
        .ok_or_else(|| McpError::OAuth("Empty HTTP request".to_string()))?;

    let path = first_line
        .split_whitespace()
        .nth(1)
        .ok_or_else(|| McpError::OAuth("Malformed HTTP request line".to_string()))?;

    let query_str = path
        .split_once('?')
        .map(|(_, q)| q)
        .ok_or_else(|| McpError::OAuth("No query parameters in callback".to_string()))?;

    let mut code = None;
    let mut state = None;

    for pair in query_str.split('&') {
        if let Some((key, value)) = pair.split_once('=') {
            match key {
                "code" => code = Some(url_decode(value)),
                "state" => state = Some(url_decode(value)),
                _ => {}
            }
        }
    }

    let code = code.ok_or_else(|| McpError::OAuth("Missing 'code' in callback".to_string()))?;
    let state = state.ok_or_else(|| McpError::OAuth("Missing 'state' in callback".to_string()))?;

    Ok((code, state))
}

/// Minimal percent-decoding for query parameter values.
fn url_decode(input: &str) -> String {
    let mut result = String::with_capacity(input.len());
    let mut chars = input.bytes();

    while let Some(b) = chars.next() {
        if b == b'%' {
            let hi = chars.next();
            let lo = chars.next();
            if let (Some(h), Some(l)) = (hi, lo) {
                let hex = [h, l];
                if let Ok(s) = std::str::from_utf8(&hex)
                    && let Ok(byte) = u8::from_str_radix(s, 16)
                {
                    result.push(byte as char);
                    continue;
                }
                // Malformed percent-encoding: keep as-is.
                result.push('%');
                result.push(h as char);
                result.push(l as char);
            }
        } else if b == b'+' {
            result.push(' ');
        } else {
            result.push(b as char);
        }
    }

    result
}

// ---------------------------------------------------------------------------
// Unit tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // -- parse_callback_query ------------------------------------------------

    #[test]
    fn parse_valid_callback_query() {
        let request = "GET /callback?code=abc123&state=xyz789 HTTP/1.1\r\nHost: localhost\r\n";
        let (code, state) = parse_callback_query(request).unwrap();
        assert_eq!(code, "abc123");
        assert_eq!(state, "xyz789");
    }

    #[test]
    fn parse_callback_query_reversed_params() {
        let request = "GET /callback?state=s1&code=c1 HTTP/1.1\r\n";
        let (code, state) = parse_callback_query(request).unwrap();
        assert_eq!(code, "c1");
        assert_eq!(state, "s1");
    }

    #[test]
    fn parse_callback_query_with_extra_params() {
        let request = "GET /callback?code=c&foo=bar&state=s HTTP/1.1\r\n";
        let (code, state) = parse_callback_query(request).unwrap();
        assert_eq!(code, "c");
        assert_eq!(state, "s");
    }

    #[test]
    fn parse_callback_query_missing_code() {
        let request = "GET /callback?state=s HTTP/1.1\r\n";
        let err = parse_callback_query(request).unwrap_err();
        assert!(err.to_string().contains("Missing 'code'"));
    }

    #[test]
    fn parse_callback_query_missing_state() {
        let request = "GET /callback?code=c HTTP/1.1\r\n";
        let err = parse_callback_query(request).unwrap_err();
        assert!(err.to_string().contains("Missing 'state'"));
    }

    #[test]
    fn parse_callback_query_no_query_string() {
        let request = "GET /callback HTTP/1.1\r\n";
        let err = parse_callback_query(request).unwrap_err();
        assert!(err.to_string().contains("No query parameters"));
    }

    #[test]
    fn parse_callback_query_empty_request() {
        let err = parse_callback_query("").unwrap_err();
        assert!(err.to_string().contains("Empty HTTP request"));
    }

    // -- url_decode ----------------------------------------------------------

    #[test]
    fn url_decode_no_encoding() {
        assert_eq!(url_decode("hello"), "hello");
    }

    #[test]
    fn url_decode_percent_encoded() {
        assert_eq!(url_decode("hello%20world"), "hello world");
    }

    #[test]
    fn url_decode_plus_sign() {
        assert_eq!(url_decode("hello+world"), "hello world");
    }

    #[test]
    fn url_decode_special_characters() {
        assert_eq!(url_decode("%3D%26%3F"), "=&?");
    }

    #[test]
    fn url_decode_mixed() {
        assert_eq!(url_decode("a%20b+c%3Dd"), "a b c=d");
    }

    // -- McpOAuthService construction ----------------------------------------

    #[test]
    fn service_clone_is_independent() {
        let repo: Arc<dyn IOAuthTokenRepository> = Arc::new(MockTokenRepo);
        let http = reqwest::Client::new();
        let svc = McpOAuthService::new(repo, http);
        let _clone = svc.clone();
    }

    // -- Mock repositories ---------------------------------------------------

    struct MockTokenRepo;

    #[async_trait::async_trait]
    impl IOAuthTokenRepository for MockTokenRepo {
        async fn get_by_url(&self, _: &str) -> Result<Option<nomifun_db::models::OAuthTokenRow>, nomifun_db::DbError> {
            Ok(None)
        }

        async fn upsert(
            &self,
            _: UpsertOAuthTokenParams<'_>,
        ) -> Result<nomifun_db::models::OAuthTokenRow, nomifun_db::DbError> {
            unimplemented!()
        }

        async fn delete(&self, _: &str) -> Result<(), nomifun_db::DbError> {
            Ok(())
        }

        async fn list_authenticated_urls(&self) -> Result<Vec<String>, nomifun_db::DbError> {
            Ok(vec![])
        }
    }

    struct IdempotentDeleteRepo;

    #[async_trait::async_trait]
    impl IOAuthTokenRepository for IdempotentDeleteRepo {
        async fn get_by_url(&self, _: &str) -> Result<Option<nomifun_db::models::OAuthTokenRow>, nomifun_db::DbError> {
            Ok(None)
        }

        async fn upsert(
            &self,
            _: UpsertOAuthTokenParams<'_>,
        ) -> Result<nomifun_db::models::OAuthTokenRow, nomifun_db::DbError> {
            unimplemented!()
        }

        async fn delete(&self, url: &str) -> Result<(), nomifun_db::DbError> {
            Err(nomifun_db::DbError::NotFound(format!(
                "OAuth token for '{url}' not found"
            )))
        }

        async fn list_authenticated_urls(&self) -> Result<Vec<String>, nomifun_db::DbError> {
            Ok(vec![])
        }
    }

    struct ValidTokenRepo;

    #[async_trait::async_trait]
    impl IOAuthTokenRepository for ValidTokenRepo {
        async fn get_by_url(&self, _: &str) -> Result<Option<nomifun_db::models::OAuthTokenRow>, nomifun_db::DbError> {
            Ok(Some(nomifun_db::models::OAuthTokenRow {
                server_url: "https://example.com".to_string(),
                access_token: "valid_access_token".to_string(),
                refresh_token: None,
                token_type: "bearer".to_string(),
                expires_at: Some(now_ms() + 3_600_000),
                created_at: now_ms(),
                updated_at: now_ms(),
            }))
        }

        async fn upsert(
            &self,
            _: UpsertOAuthTokenParams<'_>,
        ) -> Result<nomifun_db::models::OAuthTokenRow, nomifun_db::DbError> {
            unimplemented!()
        }

        async fn delete(&self, _: &str) -> Result<(), nomifun_db::DbError> {
            Ok(())
        }

        async fn list_authenticated_urls(&self) -> Result<Vec<String>, nomifun_db::DbError> {
            Ok(vec!["https://example.com".to_string()])
        }
    }

    struct ExpiredTokenRepo;

    #[async_trait::async_trait]
    impl IOAuthTokenRepository for ExpiredTokenRepo {
        async fn get_by_url(&self, _: &str) -> Result<Option<nomifun_db::models::OAuthTokenRow>, nomifun_db::DbError> {
            Ok(Some(nomifun_db::models::OAuthTokenRow {
                server_url: "https://example.com".to_string(),
                access_token: "expired_token".to_string(),
                refresh_token: None,
                token_type: "bearer".to_string(),
                expires_at: Some(1000),
                created_at: 500,
                updated_at: 500,
            }))
        }

        async fn upsert(
            &self,
            _: UpsertOAuthTokenParams<'_>,
        ) -> Result<nomifun_db::models::OAuthTokenRow, nomifun_db::DbError> {
            unimplemented!()
        }

        async fn delete(&self, _: &str) -> Result<(), nomifun_db::DbError> {
            Ok(())
        }

        async fn list_authenticated_urls(&self) -> Result<Vec<String>, nomifun_db::DbError> {
            Ok(vec![])
        }
    }

    struct NoExpiryTokenRepo;

    #[async_trait::async_trait]
    impl IOAuthTokenRepository for NoExpiryTokenRepo {
        async fn get_by_url(&self, _: &str) -> Result<Option<nomifun_db::models::OAuthTokenRow>, nomifun_db::DbError> {
            Ok(Some(nomifun_db::models::OAuthTokenRow {
                server_url: "https://example.com".to_string(),
                access_token: "no_expiry_token".to_string(),
                refresh_token: None,
                token_type: "bearer".to_string(),
                expires_at: None,
                created_at: now_ms(),
                updated_at: now_ms(),
            }))
        }

        async fn upsert(
            &self,
            _: UpsertOAuthTokenParams<'_>,
        ) -> Result<nomifun_db::models::OAuthTokenRow, nomifun_db::DbError> {
            unimplemented!()
        }

        async fn delete(&self, _: &str) -> Result<(), nomifun_db::DbError> {
            Ok(())
        }

        async fn list_authenticated_urls(&self) -> Result<Vec<String>, nomifun_db::DbError> {
            Ok(vec!["https://example.com".to_string()])
        }
    }

    // -- Service behavior tests ----------------------------------------------

    #[tokio::test]
    async fn check_status_no_token_returns_false() {
        let svc = McpOAuthService::new(Arc::new(MockTokenRepo), reqwest::Client::new());
        let status = svc.check_oauth_status("https://example.com").await.unwrap();
        assert!(!status.authenticated);
    }

    #[tokio::test]
    async fn check_status_with_valid_token() {
        let svc = McpOAuthService::new(Arc::new(ValidTokenRepo), reqwest::Client::new());
        let status = svc.check_oauth_status("https://example.com").await.unwrap();
        assert!(status.authenticated);
    }

    #[tokio::test]
    async fn check_status_with_expired_token() {
        let svc = McpOAuthService::new(Arc::new(ExpiredTokenRepo), reqwest::Client::new());
        let status = svc.check_oauth_status("https://example.com").await.unwrap();
        assert!(!status.authenticated);
    }

    #[tokio::test]
    async fn check_status_no_expiry_treated_as_valid() {
        let svc = McpOAuthService::new(Arc::new(NoExpiryTokenRepo), reqwest::Client::new());
        let status = svc.check_oauth_status("https://example.com").await.unwrap();
        assert!(status.authenticated);
    }

    #[tokio::test]
    async fn logout_idempotent_for_nonexistent() {
        let svc = McpOAuthService::new(Arc::new(IdempotentDeleteRepo), reqwest::Client::new());
        svc.logout("https://nonexistent.example.com").await.unwrap();
    }

    #[tokio::test]
    async fn get_authenticated_servers_empty() {
        let svc = McpOAuthService::new(Arc::new(MockTokenRepo), reqwest::Client::new());
        let urls = svc.get_authenticated_servers().await.unwrap();
        assert!(urls.is_empty());
    }

    #[tokio::test]
    async fn get_authenticated_servers_returns_urls() {
        let svc = McpOAuthService::new(Arc::new(ValidTokenRepo), reqwest::Client::new());
        let urls = svc.get_authenticated_servers().await.unwrap();
        assert_eq!(urls, vec!["https://example.com"]);
    }

    #[tokio::test]
    async fn get_token_returns_none_when_no_token() {
        let svc = McpOAuthService::new(Arc::new(MockTokenRepo), reqwest::Client::new());
        let token = svc.get_token("https://example.com").await.unwrap();
        assert!(token.is_none());
    }

    #[tokio::test]
    async fn get_token_returns_access_token() {
        let svc = McpOAuthService::new(Arc::new(ValidTokenRepo), reqwest::Client::new());
        let token = svc.get_token("https://example.com").await.unwrap();
        assert_eq!(token.as_deref(), Some("valid_access_token"));
    }

    #[tokio::test]
    async fn get_token_returns_expired_when_no_refresh() {
        let svc = McpOAuthService::new(Arc::new(ExpiredTokenRepo), reqwest::Client::new());
        // Expired token with no refresh_token: returns the expired token as-is.
        let token = svc.get_token("https://example.com").await.unwrap();
        assert_eq!(token.as_deref(), Some("expired_token"));
    }
}

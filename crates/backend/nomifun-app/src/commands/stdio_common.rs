//! Renewable loopback capability client shared by all scoped stdio bridges.
//!
//! A bridge receives one JSON bootstrap containing short-lived access plus a
//! process-scoped renewal proof. It never receives the backend root issuer.
//! Every child start renews immediately (so an ACP/Nomi respawn can reuse an
//! old env safely), subsequent refreshes are single-flight, and a 401 retries
//! exactly once after forced renewal.

use std::fmt::Debug;
use std::sync::Arc;
use std::time::Duration;

use nomifun_api_types::ScopedMcpChildBootstrap;
use nomifun_common::{
    LOOPBACK_CAPABILITY_RENEW_PATH, LOOPBACK_CAPABILITY_RENEWAL_MARGIN_SECS,
    LOOPBACK_CAPABILITY_REVOKE_PATH, LoopbackCapabilityAccess,
    LoopbackCapabilityClaims, LoopbackCapabilityError,
    LoopbackCapabilityRenewalRequest, unix_time_secs,
};
use rmcp::model::{CallToolResult, Content};
use serde::Serialize;
use serde::de::DeserializeOwned;
use tokio::sync::Mutex;

type ScopedAccess<S> = LoopbackCapabilityAccess<LoopbackCapabilityClaims<S>>;

/// Structured outcome of forwarding a tool call over the loopback bridge.
///
/// The distinction is derived from the HTTP status and the gateway's JSON
/// response envelope (a top-level `error` member), never from words appearing
/// in the rendered tool text.  MCP bridges use this to set protocol-level
/// `CallToolResult.isError` through [`into_mcp_tool_result`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ForwardToolOutcome {
    Success(String),
    Error(String),
}

impl ForwardToolOutcome {
    pub(crate) fn into_parts(self) -> (String, bool) {
        match self {
            Self::Success(text) => (text, false),
            Self::Error(text) => (text, true),
        }
    }
}

/// Convert a structured loopback outcome into the MCP wire result while
/// preserving its error bit. A capability may optionally attach
/// `_mcp_images: [{"mime_type","data"}]`; those entries become MCP image
/// content and are removed from the text payload.
pub(crate) fn into_mcp_tool_result(outcome: ForwardToolOutcome) -> CallToolResult {
    let (text, is_error) = outcome.into_parts();
    if !text.contains("_mcp_images") {
        return call_tool_result(vec![Content::text(text)], is_error);
    }

    let parsed: Option<serde_json::Value> = serde_json::from_str(&text).ok();
    let images: Vec<Content> = parsed
        .as_ref()
        .and_then(|value| value.get("_mcp_images"))
        .and_then(serde_json::Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(|image| {
                    let data = image.get("data").and_then(serde_json::Value::as_str)?;
                    let mime = image
                        .get("mime_type")
                        .and_then(serde_json::Value::as_str)?;
                    Some(Content::image(data.to_owned(), mime.to_owned()))
                })
                .collect()
        })
        .unwrap_or_default();
    if images.is_empty() {
        return call_tool_result(vec![Content::text(text)], is_error);
    }

    let text_out = match parsed {
        Some(serde_json::Value::Object(mut map)) => {
            map.remove("_mcp_images");
            serde_json::to_string(&serde_json::Value::Object(map)).unwrap_or(text)
        }
        _ => text,
    };
    let mut content = vec![Content::text(text_out)];
    content.extend(images);
    call_tool_result(content, is_error)
}

fn call_tool_result(content: Vec<Content>, is_error: bool) -> CallToolResult {
    if is_error {
        CallToolResult::error(content)
    } else {
        CallToolResult::success(content)
    }
}

/// Build the HTTP client used only for process-local callback traffic.
pub fn build_bridge_http_client() -> reqwest::Client {
    reqwest::Client::builder()
        .no_proxy()
        .pool_max_idle_per_host(0)
        .connect_timeout(Duration::from_secs(5))
        .timeout(Duration::from_secs(60))
        .build()
        .unwrap_or_else(|_| reqwest::Client::new())
}

#[derive(Clone)]
pub struct ScopedBridgeClient<S> {
    inner: Arc<ScopedBridgeInner<S>>,
}

struct ScopedBridgeInner<S> {
    port: u16,
    domain: &'static str,
    log_prefix: &'static str,
    renewal: LoopbackCapabilityRenewalRequest,
    immutable_claims: LoopbackCapabilityClaims<S>,
    access: Mutex<ScopedAccess<S>>,
    http_client: reqwest::Client,
    validate_domain: fn(&LoopbackCapabilityClaims<S>) -> Result<(), LoopbackCapabilityError>,
    clock: Arc<dyn Fn() -> u64 + Send + Sync>,
}

impl<S> ScopedBridgeClient<S>
where
    S: Clone + Debug + PartialEq + Eq + Serialize + DeserializeOwned + Send + Sync + 'static,
{
    /// Parse the sole capability env and canonicalize it against the parent
    /// process registry before exposing any MCP operation.
    pub async fn from_env(
        env_name: &str,
        domain: &'static str,
        log_prefix: &'static str,
        validate_domain: fn(
            &LoopbackCapabilityClaims<S>,
        ) -> Result<(), LoopbackCapabilityError>,
    ) -> Result<Self, String> {
        let raw = std::env::var(env_name).map_err(|_| format!("missing {env_name}"))?;
        let bootstrap: ScopedMcpChildBootstrap<LoopbackCapabilityClaims<S>> =
            serde_json::from_str(&raw).map_err(|error| format!("invalid {env_name}: {error}"))?;
        Self::from_bootstrap(
            bootstrap,
            domain,
            log_prefix,
            validate_domain,
            Arc::new(unix_time_secs),
        )
        .await
    }

    pub(crate) async fn from_bootstrap(
        bootstrap: ScopedMcpChildBootstrap<LoopbackCapabilityClaims<S>>,
        domain: &'static str,
        log_prefix: &'static str,
        validate_domain: fn(
            &LoopbackCapabilityClaims<S>,
        ) -> Result<(), LoopbackCapabilityError>,
        clock: Arc<dyn Fn() -> u64 + Send + Sync>,
    ) -> Result<Self, String> {
        if bootstrap.port == 0 {
            return Err("capability bootstrap has invalid loopback port".into());
        }
        bootstrap
            .access
            .claims
            .validate_renewable_shape()
            .map_err(|error| error.to_string())?;
        validate_domain(&bootstrap.access.claims).map_err(|error| error.to_string())?;
        if bootstrap.renewal.lease_id != bootstrap.access.claims.lease_id {
            return Err("capability bootstrap lease mismatch".into());
        }

        let client = Self {
            inner: Arc::new(ScopedBridgeInner {
                port: bootstrap.port,
                domain,
                log_prefix,
                renewal: bootstrap.renewal,
                immutable_claims: bootstrap.access.claims.clone(),
                access: Mutex::new(bootstrap.access),
                http_client: build_bridge_http_client(),
                validate_domain,
                clock,
            }),
        };

        // Always renew at startup. ACP and Nomi may respawn an MCP process
        // from the original env long after its first access token expired.
        client.ensure_access(true).await?;
        Ok(client)
    }

    pub fn port(&self) -> u16 {
        self.inner.port
    }

    pub async fn access(&self) -> Result<ScopedAccess<S>, String> {
        self.ensure_access(false).await
    }

    pub async fn access_for(&self, operation: &str) -> Result<ScopedAccess<S>, String> {
        let access = self.ensure_access(false).await?;
        if !access.claims.allows(operation) {
            return Err(format!(
                "operation is outside capability scope: {operation}"
            ));
        }
        Ok(access)
    }

    async fn ensure_access(&self, force: bool) -> Result<ScopedAccess<S>, String> {
        // The mutex is deliberately held across renewal I/O: every concurrent
        // caller observes one refresh and reuses its result (single-flight).
        let mut current = self.inner.access.lock().await;
        let now = (self.inner.clock)();
        if !force
            && current.claims.validate_at(now).is_ok()
            && current.claims.expires_at_unix_secs
                > now.saturating_add(LOOPBACK_CAPABILITY_RENEWAL_MARGIN_SECS)
        {
            return Ok(current.clone());
        }

        let renewed = self.request_renewal().await?;
        self.validate_renewed_access(&renewed, now)?;
        *current = renewed.clone();
        Ok(renewed)
    }

    /// Renew after a request was rejected with this exact access token. If a
    /// concurrent caller already replaced that token while we waited for the
    /// mutex, reuse its fresh access instead of serially renewing again.
    async fn renew_after_unauthorized(
        &self,
        rejected_token: &str,
    ) -> Result<ScopedAccess<S>, String> {
        let mut current = self.inner.access.lock().await;
        if current.token != rejected_token {
            return Ok(current.clone());
        }

        let now = (self.inner.clock)();
        let renewed = self.request_renewal().await?;
        self.validate_renewed_access(&renewed, now)?;
        *current = renewed.clone();
        Ok(renewed)
    }

    fn validate_renewed_access(&self, renewed: &ScopedAccess<S>, now: u64) -> Result<(), String> {
        renewed
            .claims
            .validate_at(now)
            .map_err(|error| format!("invalid renewed capability: {error}"))?;
        (self.inner.validate_domain)(&renewed.claims)
            .map_err(|error| format!("invalid renewed capability scope: {error}"))?;
        if renewed.token.is_empty() || renewed.token.trim() != renewed.token {
            return Err("renewal returned an invalid access token".into());
        }

        let expected = &self.inner.immutable_claims;
        let actual = &renewed.claims;
        if actual.version != expected.version
            || actual.lease_id != expected.lease_id
            || actual.user_id != expected.user_id
            || actual.session != expected.session
            || actual.allowed_tools != expected.allowed_tools
            || actual.scope != expected.scope
        {
            return Err("renewal changed immutable capability authorization".into());
        }
        Ok(())
    }

    async fn request_renewal(&self) -> Result<ScopedAccess<S>, String> {
        let url = format!(
            "http://127.0.0.1:{}{}",
            self.inner.port, LOOPBACK_CAPABILITY_RENEW_PATH
        );
        let delays_ms = [0_u64, 250, 750, 1_500];
        let mut last_error = String::new();
        for (attempt, delay_ms) in delays_ms.into_iter().enumerate() {
            if delay_ms > 0 {
                tokio::time::sleep(Duration::from_millis(delay_ms)).await;
            }
            match self
                .inner
                .http_client
                .post(&url)
                .json(&self.inner.renewal)
                .send()
                .await
            {
                Ok(response) => {
                    let status = response.status();
                    let text = response.text().await.unwrap_or_default();
                    if status.is_success() {
                        return serde_json::from_str(&text).map_err(|error| {
                            format!(
                                "{} renewal returned malformed access: {error}",
                                self.inner.domain
                            )
                        });
                    }
                    last_error = format!(
                        "{} renewal rejected with HTTP {status}",
                        self.inner.domain
                    );
                    if status.is_client_error() {
                        break;
                    }
                }
                Err(error) => {
                    last_error = format!("renewal transport failed: {error:#}");
                }
            }
            eprintln!(
                "[{}] capability renewal retry {}",
                self.inner.log_prefix,
                attempt + 2
            );
        }
        Err(last_error)
    }

    /// Forward one tool call while preserving whether the remote endpoint
    /// returned a tool-level error.  This is the MCP-facing variant: callers
    /// must map [`ForwardToolOutcome::Error`] to `CallToolResult.isError=true`.
    pub(crate) async fn forward_tool_outcome(
        &self,
        operation: &str,
        mut body: serde_json::Value,
        stringify_non_string_result: bool,
    ) -> ForwardToolOutcome {
        let first = match self.access_for(operation).await {
            Ok(access) => access,
            Err(error) => return ForwardToolOutcome::Error(format!("Error: {error}")),
        };
        inject_session(&mut body, &first.claims);
        let first_response = self.post_tool_with_retry(&first.token, &body).await;

        let (status, text) = match first_response {
            Ok(response) if response.0 == reqwest::StatusCode::UNAUTHORIZED => {
                let renewed = match self.renew_after_unauthorized(&first.token).await {
                    Ok(access) => access,
                    Err(error) => {
                        return ForwardToolOutcome::Error(format!(
                            "Error: capability renewal failed: {error}"
                        ));
                    }
                };
                inject_session(&mut body, &renewed.claims);
                match self.post_tool_with_retry(&renewed.token, &body).await {
                    Ok(response) => response,
                    Err(error) => {
                        return ForwardToolOutcome::Error(format!("Error: {error}"));
                    }
                }
            }
            Ok(response) => response,
            Err(error) => return ForwardToolOutcome::Error(format!("Error: {error}")),
        };

        eprintln!(
            "[{}] POST /tool -> status={status}",
            self.inner.log_prefix
        );
        render_tool_response(status, &text, stringify_non_string_result)
    }

    async fn post_tool_with_retry(
        &self,
        token: &str,
        body: &serde_json::Value,
    ) -> Result<(reqwest::StatusCode, String), String> {
        let url = format!("http://127.0.0.1:{}/tool", self.inner.port);
        let delays_ms = [0_u64, 250, 750, 1_500];
        let mut last_error = String::new();
        for delay_ms in delays_ms {
            if delay_ms > 0 {
                tokio::time::sleep(Duration::from_millis(delay_ms)).await;
            }
            match self
                .inner
                .http_client
                .post(&url)
                .bearer_auth(token)
                .json(body)
                .send()
                .await
            {
                Ok(response) => {
                    let status = response.status();
                    let text = response
                        .text()
                        .await
                        .map_err(|error| format!("failed to read response: {error}"))?;
                    return Ok((status, text));
                }
                Err(error) => last_error = format!("tool transport failed: {error:#}"),
            }
        }
        Err(last_error)
    }

    /// Best-effort child-side teardown. The main runtime/PTY guard is the
    /// independent backstop when the child is killed abruptly.
    pub async fn revoke(&self) {
        let url = format!(
            "http://127.0.0.1:{}{}",
            self.inner.port, LOOPBACK_CAPABILITY_REVOKE_PATH
        );
        let _ = self
            .inner
            .http_client
            .post(url)
            .timeout(Duration::from_secs(2))
            .json(&self.inner.renewal)
            .send()
            .await;
    }
}

fn inject_session<S: Serialize>(body: &mut serde_json::Value, claims: &LoopbackCapabilityClaims<S>) {
    let Some(object) = body.as_object_mut() else {
        *body = serde_json::json!({});
        return inject_session(body, claims);
    };
    object.insert(
        "session".into(),
        serde_json::to_value(claims).expect("validated capability claims serialize"),
    );
}

fn render_tool_response(
    status: reqwest::StatusCode,
    text: &str,
    stringify_non_string_result: bool,
) -> ForwardToolOutcome {
    if !status.is_success() {
        return ForwardToolOutcome::Error(text.to_owned());
    }

    let value = match serde_json::from_str::<serde_json::Value>(text) {
        Ok(value) => value,
        Err(error) => {
            return ForwardToolOutcome::Error(format!(
                "Error: invalid loopback tool response (expected JSON envelope): {error}"
            ));
        }
    };

    // Loopback handlers use an explicit top-level result/error envelope.  Keep
    // this fail-closed: a malformed 2xx response must never turn into a
    // successful MCP result.  `needs_confirmation` is the one explicit control
    // outcome emitted by the gateway permission gate.
    let has_result = value.get("result").is_some();
    let has_error = value.get("error").is_some();
    let is_confirmation = value
        .get("needs_confirmation")
        .and_then(serde_json::Value::as_bool)
        == Some(true);
    if has_result && has_error {
        return ForwardToolOutcome::Error(
            "Error: invalid loopback tool response (both `result` and `error` are present)".into(),
        );
    }
    if is_confirmation && (has_result || has_error) {
        return ForwardToolOutcome::Error(
            "Error: invalid loopback tool response (confirmation mixed with result envelope)"
                .into(),
        );
    }
    if let Some(error) = value.get("error") {
        return ForwardToolOutcome::Error(format!("Error: {error}"));
    }
    if let Some(result) = value.get("result") {
        return match result {
            serde_json::Value::String(result) => ForwardToolOutcome::Success(result.clone()),
            _ if stringify_non_string_result => ForwardToolOutcome::Success(
                serde_json::to_string_pretty(result).unwrap_or_else(|_| result.to_string()),
            ),
            _ => ForwardToolOutcome::Success(text.to_owned()),
        };
    }
    if is_confirmation {
        return ForwardToolOutcome::Success(text.to_owned());
    }

    ForwardToolOutcome::Error(
        "Error: invalid loopback tool response (missing `result` or `error`)".into(),
    )
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};

    use axum::Json;
    use axum::extract::State;
    use axum::http::StatusCode;
    use axum::response::IntoResponse;
    use nomifun_common::{
        LOOPBACK_CAPABILITY_TTL_SECS, LoopbackCapabilityIssuer,
        LoopbackSessionBinding,
    };
    use serde::{Deserialize, Serialize};

    use super::*;

    const DOMAIN: &str = "stdio-common-test-v2";

    #[test]
    fn response_renderer_preserves_structured_gateway_error() {
        let outcome = render_tool_response(
            reqwest::StatusCode::OK,
            r#"{"error":"invalid arguments for this tool: missing field `kb_id`"}"#,
            true,
        );
        assert!(matches!(outcome, ForwardToolOutcome::Error(text) if text.contains("kb_id")));
    }

    #[test]
    fn response_renderer_does_not_guess_errors_from_text() {
        let outcome = render_tool_response(
            reqwest::StatusCode::OK,
            r#"{"result":"Error: this is ordinary successful tool output"}"#,
            true,
        );
        assert_eq!(
            outcome,
            ForwardToolOutcome::Success("Error: this is ordinary successful tool output".into())
        );

        let nested = render_tool_response(
            reqwest::StatusCode::OK,
            r#"{"result":{"error":"a payload field, not the gateway envelope"}}"#,
            true,
        );
        assert!(matches!(nested, ForwardToolOutcome::Success(_)));
    }

    #[test]
    fn response_renderer_marks_non_success_http_status_as_error() {
        let outcome = render_tool_response(
            reqwest::StatusCode::INTERNAL_SERVER_ERROR,
            "upstream unavailable",
            true,
        );
        assert_eq!(
            outcome,
            ForwardToolOutcome::Error("upstream unavailable".into())
        );
    }

    #[test]
    fn response_renderer_rejects_malformed_success_envelopes() {
        for text in ["not json", r#"{"unexpected":true}"#, "null"] {
            let outcome = render_tool_response(reqwest::StatusCode::OK, text, true);
            assert!(
                matches!(outcome, ForwardToolOutcome::Error(ref message) if message.contains("invalid loopback tool response")),
                "unexpected outcome for {text:?}: {outcome:?}"
            );
        }

        let ambiguous = render_tool_response(
            reqwest::StatusCode::OK,
            r#"{"result":"ok","error":"failed"}"#,
            true,
        );
        assert!(matches!(ambiguous, ForwardToolOutcome::Error(message) if message.contains("both `result` and `error`")));

        let mixed_confirmation = render_tool_response(
            reqwest::StatusCode::OK,
            r#"{"result":"ok","needs_confirmation":true}"#,
            true,
        );
        assert!(matches!(mixed_confirmation, ForwardToolOutcome::Error(message) if message.contains("confirmation mixed")));
    }

    #[test]
    fn response_renderer_accepts_explicit_confirmation_outcome() {
        let text = r#"{"needs_confirmation":true,"tool":"nomi_delete"}"#;
        assert_eq!(
            render_tool_response(reqwest::StatusCode::OK, text, true),
            ForwardToolOutcome::Success(text.into())
        );
    }

    #[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
    struct TestScope {
        resource: String,
    }

    #[derive(Clone)]
    struct TestState {
        issuer: Arc<LoopbackCapabilityIssuer>,
        now: Arc<AtomicU64>,
        renew_count: Arc<AtomicUsize>,
        tool_count: Arc<AtomicUsize>,
        tamper_scope: Arc<AtomicBool>,
        reject_tools: Arc<AtomicBool>,
    }

    fn validate_test_claims(
        claims: &LoopbackCapabilityClaims<TestScope>,
    ) -> Result<(), LoopbackCapabilityError> {
        claims.validate_renewable_shape()?;
        if claims.scope.resource.trim().is_empty() {
            return Err(LoopbackCapabilityError::InvalidIdentity);
        }
        Ok(())
    }

    fn bootstrap(
        issuer: &Arc<LoopbackCapabilityIssuer>,
        port: u16,
        now: u64,
    ) -> ScopedMcpChildBootstrap<LoopbackCapabilityClaims<TestScope>> {
        let claims = LoopbackCapabilityClaims::issue_at(
            "0190f5fe-7c00-7a00-8000-000000000001",
            LoopbackSessionBinding::conversation("0190f5fe-7c00-7a00-8000-000000000001"),
            ["tools/call", "tools/list"],
            TestScope {
                resource: "alpha".into(),
            },
            now,
            LOOPBACK_CAPABILITY_TTL_SECS,
        )
        .unwrap();
        let (token, renewal_proof) = issuer.activate(DOMAIN, &claims).unwrap();
        ScopedMcpChildBootstrap {
            port,
            renewal: LoopbackCapabilityRenewalRequest {
                lease_id: claims.lease_id.clone(),
                renewal_proof,
            },
            access: LoopbackCapabilityAccess { token, claims },
        }
    }

    async fn renew_handler(
        State(state): State<TestState>,
        Json(request): Json<LoopbackCapabilityRenewalRequest>,
    ) -> impl IntoResponse {
        state.renew_count.fetch_add(1, Ordering::SeqCst);
        match state.issuer.renew_at::<TestScope>(
            DOMAIN,
            &request,
            state.now.load(Ordering::SeqCst),
        ) {
            Ok(mut access) => {
                if state.tamper_scope.load(Ordering::SeqCst) {
                    access.claims.scope.resource = "beta".into();
                }
                (StatusCode::OK, Json(serde_json::json!(access))).into_response()
            }
            Err(_) => (
                StatusCode::UNAUTHORIZED,
                Json(serde_json::json!({"error": "unauthorized"})),
            )
                .into_response(),
        }
    }

    async fn tool_handler(State(state): State<TestState>) -> impl IntoResponse {
        state.tool_count.fetch_add(1, Ordering::SeqCst);
        if state.reject_tools.load(Ordering::SeqCst) {
            (
                StatusCode::UNAUTHORIZED,
                Json(serde_json::json!({"error": "unauthorized"})),
            )
                .into_response()
        } else {
            (
                StatusCode::OK,
                Json(serde_json::json!({"result": "ok"})),
            )
                .into_response()
        }
    }

    async fn spawn_server(
        state: TestState,
    ) -> (u16, tokio::task::JoinHandle<()>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .unwrap();
        let port = listener.local_addr().unwrap().port();
        let app = axum::Router::new()
            .route(LOOPBACK_CAPABILITY_RENEW_PATH, axum::routing::post(renew_handler))
            .route("/tool", axum::routing::post(tool_handler))
            .with_state(state);
        let handle = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        (port, handle)
    }

    fn state(now: u64) -> TestState {
        TestState {
            issuer: Arc::new(LoopbackCapabilityIssuer::random().unwrap()),
            now: Arc::new(AtomicU64::new(now)),
            renew_count: Arc::new(AtomicUsize::new(0)),
            tool_count: Arc::new(AtomicUsize::new(0)),
            tamper_scope: Arc::new(AtomicBool::new(false)),
            reject_tools: Arc::new(AtomicBool::new(false)),
        }
    }

    #[tokio::test]
    async fn expired_original_env_renews_on_start_and_refresh_is_single_flight() {
        let now = unix_time_secs();
        let state = state(now);
        let (port, server) = spawn_server(state.clone()).await;
        let mut bootstrap = bootstrap(&state.issuer, port, now);
        // Simulate an ACP/Nomi MCP respawn from the original env after access
        // expiry. Renewal proof remains bound to the active process lease.
        bootstrap.access.claims.issued_at_unix_secs = now.saturating_sub(61);
        bootstrap.access.claims.expires_at_unix_secs = now.saturating_sub(1);

        let clock_state = state.now.clone();
        let client = ScopedBridgeClient::from_bootstrap(
            bootstrap,
            DOMAIN,
            "test-bridge",
            validate_test_claims,
            Arc::new(move || clock_state.load(Ordering::SeqCst)),
        )
        .await
        .expect("expired bootstrap must canonical-renew");
        assert_eq!(state.renew_count.load(Ordering::SeqCst), 1);

        let first = client.access().await.unwrap();
        state.now.store(
            first
                .claims
                .expires_at_unix_secs
                .saturating_sub(LOOPBACK_CAPABILITY_RENEWAL_MARGIN_SECS)
                .saturating_add(1),
            Ordering::SeqCst,
        );
        let mut tasks = Vec::new();
        for _ in 0..16 {
            let client = client.clone();
            tasks.push(tokio::spawn(async move { client.access().await.unwrap() }));
        }
        for task in tasks {
            task.await.unwrap();
        }
        assert_eq!(
            state.renew_count.load(Ordering::SeqCst),
            2,
            "all concurrent refreshes must share one renewal"
        );
        server.abort();
    }

    #[tokio::test]
    async fn renewal_rejects_server_response_that_changes_full_immutable_scope() {
        let now = unix_time_secs();
        let state = state(now);
        state.tamper_scope.store(true, Ordering::SeqCst);
        let (port, server) = spawn_server(state.clone()).await;
        let bootstrap = bootstrap(&state.issuer, port, now);
        let clock_state = state.now.clone();

        let error = ScopedBridgeClient::from_bootstrap(
            bootstrap,
            DOMAIN,
            "test-bridge",
            validate_test_claims,
            Arc::new(move || clock_state.load(Ordering::SeqCst)),
        )
        .await
        .err()
        .expect("tampered renewal must fail closed");
        assert!(error.contains("immutable capability authorization"));
        server.abort();
    }

    #[tokio::test]
    async fn unauthorized_tool_response_forces_one_renewal_and_one_retry_only() {
        let now = unix_time_secs();
        let state = state(now);
        state.reject_tools.store(true, Ordering::SeqCst);
        let (port, server) = spawn_server(state.clone()).await;
        let bootstrap = bootstrap(&state.issuer, port, now);
        let clock_state = state.now.clone();
        let client = ScopedBridgeClient::from_bootstrap(
            bootstrap,
            DOMAIN,
            "test-bridge",
            validate_test_claims,
            Arc::new(move || clock_state.load(Ordering::SeqCst)),
        )
        .await
        .unwrap();

        let result = client
            .forward_tool_outcome(
                "tools/call",
                serde_json::json!({"tool": "demo", "args": {}}),
                false,
            )
            .await;
        assert!(
            matches!(result, ForwardToolOutcome::Error(text) if text.contains("unauthorized"))
        );
        assert_eq!(state.renew_count.load(Ordering::SeqCst), 2);
        assert_eq!(state.tool_count.load(Ordering::SeqCst), 2);
        server.abort();
    }

    #[tokio::test]
    async fn concurrent_unauthorized_requests_share_one_forced_renewal() {
        let now = unix_time_secs();
        let state = state(now);
        let (port, server) = spawn_server(state.clone()).await;
        let bootstrap = bootstrap(&state.issuer, port, now);
        let clock_state = state.now.clone();
        let client = ScopedBridgeClient::from_bootstrap(
            bootstrap,
            DOMAIN,
            "test-bridge",
            validate_test_claims,
            Arc::new(move || clock_state.load(Ordering::SeqCst)),
        )
        .await
        .unwrap();

        let rejected_token = client.access().await.unwrap().token;
        let mut tasks = Vec::new();
        for _ in 0..16 {
            let client = client.clone();
            let rejected_token = rejected_token.clone();
            tasks.push(tokio::spawn(async move {
                client
                    .renew_after_unauthorized(&rejected_token)
                    .await
                    .unwrap()
                    .token
            }));
        }

        let mut renewed_tokens = Vec::new();
        for task in tasks {
            renewed_tokens.push(task.await.unwrap());
        }
        assert!(renewed_tokens.iter().all(|token| token != &rejected_token));
        assert!(renewed_tokens.windows(2).all(|pair| pair[0] == pair[1]));
        assert_eq!(
            state.renew_count.load(Ordering::SeqCst),
            2,
            "startup renewal plus one shared forced renewal"
        );
        server.abort();
    }
}

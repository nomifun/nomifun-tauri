//! Local-trust resolution: how a backend distinguishes its own desktop webview
//! (no login) from a remote LAN browser (must log in).
//!
//! The single source of truth is [`AuthPolicy`] (replacing the old
//! `local: bool`). Trust is decided PER REQUEST by [`trust_resolve_middleware`],
//! which runs as the outermost application middleware — before CSRF and the
//! per-route auth middleware — so both can read the [`LocalTrusted`] marker and
//! the injected [`CurrentUser`] it leaves in the request extensions.
//!
//! The desktop's own webview proves it is the trusted local client by
//! presenting a per-boot secret in the [`LOCAL_TRUST_HEADER`] header (and, for
//! the WebSocket upgrade where browsers cannot set custom headers, as a
//! `Sec-WebSocket-Protocol` value — see `extract_token_from_ws_headers`). The
//! secret identifies the *process* the desktop injected it into, NOT "any
//! loopback connection", so other local OS accounts and same-host reverse
//! proxies are not trusted.

use std::sync::Arc;

use axum::extract::{Request, State};
use axum::http::HeaderMap;
use axum::middleware::Next;
use axum::response::Response;

use nomifun_common::{AppError, UserId};

use crate::middleware::CurrentUser;

/// HTTP header the desktop webview presents to prove it is the trusted local
/// client. Value = the per-boot local-trust secret. Named with a `token`-ish
/// shape so request-logging redaction patterns mask it.
pub const LOCAL_TRUST_HEADER: &str = "x-nomi-local-trust";

/// Authentication policy for a backend instance — the single source of truth
/// that replaces the former scattered `local: bool`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum AuthPolicy {
    /// Authentication fully disabled; every request is the database-resolved
    /// installation owner.
    /// Used by `--insecure-no-auth` (dev / `dev:webui`).
    NoAuth,
    /// JWT required for every client. Standalone `nomifun-web` default.
    Required,
    /// JWT required for everyone EXCEPT requests bearing the per-boot
    /// local-trust secret (the desktop's own webview), which are
    /// the database-resolved installation owner. Used by the desktop shell.
    TrustLocalToken,
}

impl AuthPolicy {
    /// No authentication at all (every request is the installation owner).
    pub fn is_no_auth(self) -> bool {
        matches!(self, AuthPolicy::NoAuth)
    }

    /// Whether the desktop's own cross-origin webview may connect. Its document
    /// origin (`tauri://` / `http://tauri.localhost`) differs from the loopback
    /// API port, so permissive CORS is required for these policies.
    pub fn allows_local_webview(self) -> bool {
        matches!(self, AuthPolicy::NoAuth | AuthPolicy::TrustLocalToken)
    }

    /// Whether boot-time admin credential pre-seeding applies. Only the
    /// standalone authenticated host pre-seeds; NoAuth needs no admin and the
    /// desktop provisions a password lazily when remote access is first enabled.
    pub fn requires_admin_provisioning(self) -> bool {
        matches!(self, AuthPolicy::Required)
    }
}

/// Marker inserted into request extensions when a request has been granted
/// local trust (NoAuth, or a valid local-trust secret). Read by the CSRF
/// middleware (to skip — header-trusted requests are not cookie-ambient).
#[derive(Clone, Copy, Debug)]
pub struct LocalTrusted;

/// State for [`trust_resolve_middleware`].
#[derive(Clone)]
pub struct TrustState {
    pub policy: AuthPolicy,
    /// The per-boot secret. Only `Some` under [`AuthPolicy::TrustLocalToken`].
    pub local_trust_secret: Option<Arc<str>>,
    /// Canonical installation owner resolved from the database at boot. Local
    /// trust grants this exact identity; it never reconstructs one from a
    /// username, request payload, or module-level fallback.
    pub authoritative_user_id: Arc<str>,
}

/// Constant-time comparison of two strings. Length may leak (the secret is
/// fixed-length hex); the byte contents do not.
fn ct_eq(a: &str, b: &str) -> bool {
    let a = a.as_bytes();
    let b = b.as_bytes();
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

fn header_secret_matches(headers: &HeaderMap, secret: &str) -> bool {
    headers
        .get(LOCAL_TRUST_HEADER)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|presented| ct_eq(presented, secret))
}

/// Resolve whether the given headers carry valid local trust under `state`.
/// Shared by the HTTP middleware and the WebSocket validator.
pub fn is_locally_trusted(state: &TrustState, headers: &HeaderMap) -> bool {
    match state.policy {
        AuthPolicy::NoAuth => true,
        AuthPolicy::TrustLocalToken => state
            .local_trust_secret
            .as_deref()
            .is_some_and(|secret| header_secret_matches(headers, secret)),
        AuthPolicy::Required => false,
    }
}

/// Outermost application middleware. Resolves local trust BEFORE CSRF and the
/// per-route auth middleware run. When trusted it injects the privileged
/// [`CurrentUser`] plus a [`LocalTrusted`] marker; otherwise it passes through
/// untouched so per-route auth can enforce JWT where required.
pub async fn trust_resolve_middleware(State(state): State<TrustState>, mut request: Request, next: Next) -> Response {
    if is_locally_trusted(&state, request.headers()) {
        let Ok(user_id) = UserId::parse(state.authoritative_user_id.as_ref()) else {
            tracing::error!("local trust owner has a noncanonical user id; request is not elevated");
            return next.run(request).await;
        };
        request.extensions_mut().insert(CurrentUser {
            id: user_id,
            username: state.authoritative_user_id.to_string(),
        });
        request.extensions_mut().insert(LocalTrusted);
    }
    next.run(request).await
}

/// Route-layer middleware that rejects any request not granted local trust.
/// Applied to the `/api/webui/*` and `/api/auth/internal/*` credential routes
/// (which sit in the otherwise-public group with no auth middleware).
pub async fn require_local_trust_middleware(request: Request, next: Next) -> Result<Response, AppError> {
    if request.extensions().get::<LocalTrusted>().is_some() {
        Ok(next.run(request).await)
    } else {
        Err(AppError::Forbidden(
            "This endpoint is only available to the local desktop client".into(),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TEST_OWNER_ID: &str = "0190f5fe-7c00-7a00-8000-000000000001";

    fn hdrs(secret: Option<&str>) -> HeaderMap {
        let mut h = HeaderMap::new();
        if let Some(s) = secret {
            h.insert(LOCAL_TRUST_HEADER, s.parse().unwrap());
        }
        h
    }

    #[test]
    fn no_auth_always_trusted() {
        let st = TrustState {
            policy: AuthPolicy::NoAuth,
            local_trust_secret: None,
            authoritative_user_id: Arc::from(TEST_OWNER_ID),
        };
        assert!(is_locally_trusted(&st, &hdrs(None)));
    }

    #[test]
    fn required_never_trusted() {
        let st = TrustState {
            policy: AuthPolicy::Required,
            local_trust_secret: None,
            authoritative_user_id: Arc::from(TEST_OWNER_ID),
        };
        assert!(!is_locally_trusted(&st, &hdrs(Some("anything"))));
    }

    #[test]
    fn trust_local_token_matches_secret_only() {
        let st = TrustState {
            policy: AuthPolicy::TrustLocalToken,
            local_trust_secret: Some(Arc::from("s3cr3t-abc")),
            authoritative_user_id: Arc::from(TEST_OWNER_ID),
        };
        assert!(is_locally_trusted(&st, &hdrs(Some("s3cr3t-abc"))));
        assert!(!is_locally_trusted(&st, &hdrs(Some("wrong"))));
        assert!(!is_locally_trusted(&st, &hdrs(None)));
    }

    #[test]
    fn ct_eq_basic() {
        assert!(ct_eq("abc", "abc"));
        assert!(!ct_eq("abc", "abd"));
        assert!(!ct_eq("abc", "abcd"));
    }
}

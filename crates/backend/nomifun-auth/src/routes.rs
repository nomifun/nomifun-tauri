use std::sync::Arc;
use std::time::Duration;

use axum::extract::rejection::JsonRejection;
use axum::extract::{Json, Path, State};
use axum::http::{HeaderMap, header};
use axum::middleware::{from_fn, from_fn_with_state};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Extension, Router};
use serde::Deserialize;

use nomifun_api_types::{
    ApiResponse, AuthStatusResponse, ChangePasswordRequest, LoginRequest, LoginResponse, PublicUser, QrLoginRequest,
    RefreshResponse, RefreshTokenRequest, UserInfoResponse, WebuiChangePasswordRequest, WebuiChangeUsernameRequest,
    WebuiChangeUsernameResponse, WebuiGenerateQrTokenResponse, WebuiResetPasswordResponse, WsTokenResponse,
};
use nomifun_common::{AppError, UserId};
use nomifun_common::constants::SESSION_MAX_AGE_SECONDS;
use nomifun_db::{IUserRepository, models::User};

use crate::extract::extract_token_from_headers;
use crate::middleware::{AuthState, CurrentUser, auth_middleware};
use crate::trust::require_local_trust_middleware;
use crate::password::{dummy_password_hash, generate_password, hash_password, verify_password_timed};
use crate::qr_token::QrTokenStore;
use crate::rate_limit::{
    RateLimiter, api_rate_limit_middleware, auth_rate_limit_middleware, authenticated_action_rate_limit_middleware,
};
use crate::validation::{validate_password, validate_username};
use crate::{CookieConfig, JwtService};

/// Shared state for all auth route handlers.
#[derive(Clone)]
pub struct AuthRouterState {
    pub jwt_service: Arc<JwtService>,
    pub user_repo: Arc<dyn IUserRepository>,
    pub cookie_config: Arc<CookieConfig>,
    pub qr_token_store: Arc<QrTokenStore>,
}

#[derive(Debug, Deserialize)]
struct CreateInternalUserRequest {
    username: String,
    password_hash: String,
}

#[derive(Debug, Deserialize)]
struct SetSystemUserCredentialsRequest {
    username: String,
    password_hash: String,
}

#[derive(Debug, Deserialize)]
struct UpdatePasswordHashRequest {
    password_hash: String,
}

#[derive(Debug, Deserialize)]
struct UpdateUsernameRequest {
    username: String,
}

#[derive(Debug, Deserialize)]
struct UpdateJwtSecretRequest {
    jwt_secret: String,
}

fn into_public_user(user: User) -> Result<PublicUser, AppError> {
    Ok(PublicUser {
        user_id: user.user_id,
        username: user.username,
    })
}

/// Build the auth router with all endpoints and middleware layers.
///
/// Returns a `Router` with these endpoints:
/// - `POST /login`
/// - `POST /api/auth/setup` (one-time first-run admin creation)
/// - `POST /logout`
/// - `GET /api/auth/status`
/// - `GET /api/auth/user`
/// - `POST /api/auth/change-password`
/// - `POST /api/auth/refresh`
/// - `GET /api/ws-token`
/// - `POST /api/auth/qr-login`
/// - `GET /qr-login`
/// - `POST /api/webui/change-password` (local-only)
/// - `POST /api/webui/change-username` (local-only)
/// - `POST /api/webui/reset-password` (local-only)
/// - `POST /api/webui/generate-qr-token` (local-only)
pub fn auth_routes(state: AuthRouterState) -> Router {
    let auth_limiter = Arc::new(RateLimiter::auth());
    let api_limiter = Arc::new(RateLimiter::api());
    let action_limiter = Arc::new(RateLimiter::authenticated_action());

    // Start periodic cleanup for rate limiters
    let cleanup_interval = Duration::from_secs(60);
    auth_limiter.start_cleanup_task(cleanup_interval);
    api_limiter.start_cleanup_task(cleanup_interval);
    action_limiter.start_cleanup_task(cleanup_interval);

    let auth_state = AuthState {
        jwt_service: state.jwt_service.clone(),
        user_repo: state.user_repo.clone(),
    };

    // Auth rate limited routes (login, setup, qr-login)
    let auth_rate_limited = Router::new()
        .route("/login", post(login_handler))
        .route("/api/auth/setup", post(setup_handler))
        .route("/api/auth/qr-login", post(qr_login_handler))
        .route_layer(from_fn_with_state(auth_limiter, auth_rate_limit_middleware))
        .with_state(state.clone());

    // Truly public, no-auth route: first-run/login status probe.
    let api_public = Router::new()
        .route("/api/auth/status", get(status_handler))
        .route_layer(from_fn_with_state(api_limiter.clone(), api_rate_limit_middleware))
        .with_state(state.clone());

    // Local-only credential/internal routes. These have NO auth middleware, so
    // they are gated by `require_local_trust_middleware`: only the local desktop
    // client (which presents the per-boot trust secret, resolved upstream by
    // `trust_resolve_middleware` into a `LocalTrusted` marker) may reach them.
    let api_local_only = Router::new()
        .route(
            "/api/auth/internal/users",
            get(list_internal_users_handler).post(create_internal_user_handler),
        )
        .route("/api/auth/internal/users/system", get(get_system_user_handler))
        .route(
            "/api/auth/internal/users/system/credentials",
            post(set_system_user_credentials_handler),
        )
        .route(
            "/api/auth/internal/users/by-username/{username}",
            get(find_user_by_username_handler),
        )
        .route("/api/auth/internal/users/{id}", get(find_user_by_id_handler))
        .route(
            "/api/auth/internal/users/{id}/password",
            post(update_user_password_hash_handler),
        )
        .route(
            "/api/auth/internal/users/{id}/username",
            post(update_user_username_handler),
        )
        .route(
            "/api/auth/internal/users/{id}/jwt-secret",
            post(update_user_jwt_secret_handler),
        )
        .route(
            "/api/auth/internal/users/{id}/last-login",
            post(update_user_last_login_handler),
        )
        // WebUI admin credential endpoints — local desktop client only.
        .route("/api/webui/change-password", post(webui_change_password_handler))
        .route("/api/webui/change-username", post(webui_change_username_handler))
        .route("/api/webui/reset-password", post(webui_reset_password_handler))
        .route("/api/webui/generate-qr-token", post(webui_generate_qr_token_handler))
        .route_layer(from_fn(require_local_trust_middleware))
        .route_layer(from_fn_with_state(api_limiter.clone(), api_rate_limit_middleware))
        .with_state(state.clone());

    // Authenticated routes: api limiter -> auth -> action limiter
    // route_layer order: last added = outermost (first to process)
    let authenticated = Router::new()
        .route("/logout", post(logout_handler))
        .route("/api/auth/user", get(user_handler))
        .route("/api/auth/change-password", post(change_password_handler))
        .route("/api/ws-token", get(ws_token_handler))
        .route_layer(from_fn_with_state(
            action_limiter.clone(),
            authenticated_action_rate_limit_middleware,
        ))
        .route_layer(from_fn_with_state(auth_state, auth_middleware))
        .route_layer(from_fn_with_state(api_limiter.clone(), api_rate_limit_middleware))
        .with_state(state.clone());

    // API + action limited routes (token in body, no auth middleware)
    let api_action_limited = Router::new()
        .route("/api/auth/refresh", post(refresh_handler))
        .route_layer(from_fn_with_state(
            action_limiter,
            authenticated_action_rate_limit_middleware,
        ))
        .route_layer(from_fn_with_state(api_limiter, api_rate_limit_middleware))
        .with_state(state);

    // Static page (no middleware)
    let static_routes = Router::new().route("/qr-login", get(qr_login_page));

    Router::new()
        .merge(auth_rate_limited)
        .merge(api_public)
        .merge(api_local_only)
        .merge(authenticated)
        .merge(api_action_limited)
        .merge(static_routes)
}

// ---------------------------------------------------------------------------
// POST /login
// ---------------------------------------------------------------------------

async fn login_handler(
    State(state): State<AuthRouterState>,
    body: Result<Json<LoginRequest>, JsonRejection>,
) -> Result<Response, AppError> {
    let Json(req) = body.map_err(|e| AppError::BadRequest(e.to_string()))?;

    // Input length validation (per API spec)
    if req.username.len() > 32 {
        return Err(AppError::BadRequest("Username must not exceed 32 characters".into()));
    }
    if req.password.len() > 128 {
        return Err(AppError::BadRequest("Password must not exceed 128 characters".into()));
    }

    // Look up user; run dummy verify on miss to prevent timing attacks
    let user = state
        .user_repo
        .find_by_username(&req.username)
        .await
        .map_err(|e| AppError::Internal(format!("Database error: {e}")))?;

    let (found_user, password_valid) = match user {
        Some(u) if u.password_hash.trim().is_empty() => {
            // Seeded user with no password yet (first-run local mode).
            // Treat as invalid credentials; run dummy verify for timing symmetry
            // and to avoid bcrypt error on empty hash leaking as a 500.
            let _ = verify_password_timed(&req.password, dummy_password_hash()).await;
            (None, false)
        }
        Some(u) => {
            let valid = verify_password_timed(&req.password, &u.password_hash).await?;
            (Some(u), valid)
        }
        None => {
            // Prevent user enumeration via timing
            let _ = verify_password_timed(&req.password, dummy_password_hash()).await;
            (None, false)
        }
    };

    if !password_valid {
        return Err(AppError::Unauthorized("Invalid username or password".into()));
    }

    let user = found_user.ok_or_else(|| AppError::Unauthorized("Invalid username or password".into()))?;

    let token = state
        .jwt_service
        .sign(user.user_id.as_str(), &user.username)
        .map_err(|e| AppError::Internal(format!("Token signing error: {e}")))?;

    // Update last login (best-effort)
    if let Err(e) = state.user_repo.update_last_login(user.user_id.as_str()).await {
        tracing::warn!("Failed to update last login for {}: {e}", user.user_id);
    }

    let cookie = state.cookie_config.build_session_cookie(&token);
    let resp = LoginResponse::new(into_public_user(user)?, token);

    Ok(([(header::SET_COOKIE, cookie)], Json(resp)).into_response())
}

// ---------------------------------------------------------------------------
// POST /logout
// ---------------------------------------------------------------------------

async fn logout_handler(State(state): State<AuthRouterState>, headers: HeaderMap) -> Result<Response, AppError> {
    if let Some(token) = extract_token_from_headers(&headers) {
        state.jwt_service.blacklist_token(&token);
    }

    let cookie = state.cookie_config.clear_session_cookie();
    let resp = ApiResponse::message("Logged out successfully");

    Ok(([(header::SET_COOKIE, cookie)], Json(resp)).into_response())
}

// ---------------------------------------------------------------------------
// GET /api/auth/status
// ---------------------------------------------------------------------------

async fn status_handler(
    State(state): State<AuthRouterState>,
    headers: HeaderMap,
) -> Result<Json<AuthStatusResponse>, AppError> {
    let has_users = state
        .user_repo
        .has_users()
        .await
        .map_err(|e| AppError::Internal(format!("Database error: {e}")))?;

    let user_count = state
        .user_repo
        .count_users()
        .await
        .map_err(|e| AppError::Internal(format!("Database error: {e}")))?;

    // Check authentication without requiring it
    let is_authenticated = extract_token_from_headers(&headers)
        .and_then(|token| state.jwt_service.verify(&token).ok())
        .is_some();

    Ok(Json(AuthStatusResponse {
        success: true,
        needs_setup: !has_users,
        user_count: user_count as u64,
        is_authenticated,
    }))
}

// ---------------------------------------------------------------------------
// POST /api/auth/setup — one-time first-run admin creation
// ---------------------------------------------------------------------------

/// Create the initial admin account on a fresh install, then log them in.
///
/// Available ONLY while the install is uninitialised: the very first visitor's
/// chosen username + password become the admin credentials, and the response
/// sets the session cookie so they are immediately logged in. The write is an
/// atomic conditional UPDATE (only matches the empty-password installation owner), so
/// even two concurrent first-run requests cannot both win — the loser gets
/// `409 Conflict` and never overwrites the winner's account.
///
/// Reuses [`LoginRequest`]/[`LoginResponse`] (same `{username, password}` shape
/// and `{success, user, token}` reply as `/login`). CSRF-exempt like `/login`
/// (see `csrf::csrf_middleware`) and behind the auth rate limiter.
///
/// SECURITY: there is a brief first-run window before setup completes where any
/// client reaching the port could claim the admin. Operators who need to close
/// it can pre-seed with `NOMIFUN_ADMIN_PASSWORD` (see
/// `nomifun_app::bootstrap::ensure_admin_credentials`), which makes this return
/// 409 from the first boot.
async fn setup_handler(
    State(state): State<AuthRouterState>,
    body: Result<Json<LoginRequest>, JsonRejection>,
) -> Result<Response, AppError> {
    // One-time only: refuse once any real (non-empty-password) user exists.
    let has_users = state
        .user_repo
        .has_users()
        .await
        .map_err(|e| AppError::Internal(format!("Database error: {e}")))?;
    if has_users {
        return Err(AppError::Conflict("Admin account already initialized".into()));
    }

    let Json(req) = body.map_err(|e| AppError::BadRequest(e.to_string()))?;

    let username = req.username.trim().to_owned();
    validate_username(&username).map_err(|e| AppError::BadRequest(e.to_string()))?;
    validate_password(&req.password).map_err(|e| AppError::BadRequest(e.to_string()))?;

    // Hash on a blocking thread (bcrypt is CPU-bound), mirroring change-password.
    let password = req.password.clone();
    let password_hash = tokio::task::spawn_blocking(move || hash_password(&password))
        .await
        .map_err(|e| AppError::Internal(format!("Task join error: {e}")))??;

    // Atomically claim the uninitialised admin slot. The conditional UPDATE is
    // the authoritative one-time gate: if a concurrent request already set the
    // credentials, this writes 0 rows and we return 409 instead of clobbering
    // the winner. (The has_users() check above is just a cheap pre-reject.)
    let provisioned = state
        .user_repo
        .set_system_user_credentials_if_uninitialized(&username, &password_hash)
        .await
        .map_err(|e| AppError::Internal(format!("Database error: {e}")))?;
    if !provisioned {
        return Err(AppError::Conflict("Admin account already initialized".into()));
    }

    let user = state
        .user_repo
        .get_primary_webui_user()
        .await
        .map_err(|e| AppError::Internal(format!("Database error: {e}")))?
        .ok_or_else(|| AppError::Internal("Admin user missing after setup".into()))?;

    let token = state
        .jwt_service
        .sign(user.user_id.as_str(), &user.username)
        .map_err(|e| AppError::Internal(format!("Token signing error: {e}")))?;

    if let Err(e) = state.user_repo.update_last_login(user.user_id.as_str()).await {
        tracing::warn!("Failed to update last login for {}: {e}", user.user_id);
    }

    let cookie = state.cookie_config.build_session_cookie(&token);
    let resp = LoginResponse::new(into_public_user(user)?, token);

    tracing::info!("first-run setup: initial admin account created");
    Ok(([(header::SET_COOKIE, cookie)], Json(resp)).into_response())
}

// ---------------------------------------------------------------------------
// Local-only internal user routes
// ---------------------------------------------------------------------------

async fn list_internal_users_handler(
    State(state): State<AuthRouterState>,
) -> Result<Json<ApiResponse<Vec<User>>>, AppError> {

    let users = state.user_repo.list_users().await?;
    Ok(Json(ApiResponse::ok(users)))
}

async fn get_system_user_handler(
    State(state): State<AuthRouterState>,
) -> Result<Json<ApiResponse<Option<User>>>, AppError> {

    let user = state.user_repo.get_system_user().await?;
    Ok(Json(ApiResponse::ok(user)))
}

async fn find_user_by_username_handler(
    State(state): State<AuthRouterState>,
    Path(username): Path<String>,
) -> Result<Json<ApiResponse<Option<User>>>, AppError> {

    let user = state.user_repo.find_by_username(&username).await?;
    Ok(Json(ApiResponse::ok(user)))
}

async fn find_user_by_id_handler(
    State(state): State<AuthRouterState>,
    Path(id): Path<UserId>,
) -> Result<Json<ApiResponse<Option<User>>>, AppError> {

    let user = state.user_repo.find_by_id(id.as_str()).await?;
    Ok(Json(ApiResponse::ok(user)))
}

async fn create_internal_user_handler(
    State(state): State<AuthRouterState>,
    body: Result<Json<CreateInternalUserRequest>, JsonRejection>,
) -> Result<Json<ApiResponse<User>>, AppError> {

    let Json(req) = body.map_err(|e| AppError::BadRequest(e.to_string()))?;
    let user = state.user_repo.create_user(&req.username, &req.password_hash).await?;
    Ok(Json(ApiResponse::ok(user)))
}

async fn set_system_user_credentials_handler(
    State(state): State<AuthRouterState>,
    body: Result<Json<SetSystemUserCredentialsRequest>, JsonRejection>,
) -> Result<Json<ApiResponse<()>>, AppError> {

    let Json(req) = body.map_err(|e| AppError::BadRequest(e.to_string()))?;
    state
        .user_repo
        .set_system_user_credentials(&req.username, &req.password_hash)
        .await?;
    Ok(Json(ApiResponse::ok(())))
}

async fn update_user_password_hash_handler(
    State(state): State<AuthRouterState>,
    Path(id): Path<UserId>,
    body: Result<Json<UpdatePasswordHashRequest>, JsonRejection>,
) -> Result<Json<ApiResponse<()>>, AppError> {

    let Json(req) = body.map_err(|e| AppError::BadRequest(e.to_string()))?;
    state.user_repo.update_password(id.as_str(), &req.password_hash).await?;
    Ok(Json(ApiResponse::ok(())))
}

async fn update_user_username_handler(
    State(state): State<AuthRouterState>,
    Path(id): Path<UserId>,
    body: Result<Json<UpdateUsernameRequest>, JsonRejection>,
) -> Result<Json<ApiResponse<()>>, AppError> {

    let Json(req) = body.map_err(|e| AppError::BadRequest(e.to_string()))?;
    state.user_repo.update_username(id.as_str(), &req.username).await?;
    Ok(Json(ApiResponse::ok(())))
}

async fn update_user_jwt_secret_handler(
    State(state): State<AuthRouterState>,
    Path(id): Path<UserId>,
    body: Result<Json<UpdateJwtSecretRequest>, JsonRejection>,
) -> Result<Json<ApiResponse<()>>, AppError> {

    let Json(req) = body.map_err(|e| AppError::BadRequest(e.to_string()))?;
    state.user_repo.update_jwt_secret(id.as_str(), &req.jwt_secret).await?;
    Ok(Json(ApiResponse::ok(())))
}

async fn update_user_last_login_handler(
    State(state): State<AuthRouterState>,
    Path(id): Path<UserId>,
) -> Result<Json<ApiResponse<()>>, AppError> {

    state.user_repo.update_last_login(id.as_str()).await?;
    Ok(Json(ApiResponse::ok(())))
}

// ---------------------------------------------------------------------------
// GET /api/auth/user
// ---------------------------------------------------------------------------

async fn user_handler(Extension(user): Extension<CurrentUser>) -> Json<UserInfoResponse> {
    Json(UserInfoResponse {
        success: true,
        user: PublicUser {
            user_id: user.id,
            username: user.username,
        },
    })
}

// ---------------------------------------------------------------------------
// POST /api/auth/change-password
// ---------------------------------------------------------------------------

async fn change_password_handler(
    State(state): State<AuthRouterState>,
    Extension(current_user): Extension<CurrentUser>,
    body: Result<Json<ChangePasswordRequest>, JsonRejection>,
) -> Result<Json<ApiResponse<()>>, AppError> {
    let Json(req) = body.map_err(|e| AppError::BadRequest(e.to_string()))?;

    // Validate new password strength
    validate_password(&req.new_password)?;

    // Fetch user record
    let user = state
        .user_repo
        .find_by_id(current_user.id.as_str())
        .await
        .map_err(|e| AppError::Internal(format!("Database error: {e}")))?
        .ok_or_else(|| AppError::NotFound("User not found".into()))?;

    // Verify current password
    let valid = verify_password_timed(&req.current_password, &user.password_hash).await?;
    if !valid {
        return Err(AppError::Unauthorized("Current password is incorrect".into()));
    }

    // Hash new password on blocking thread
    let password = req.new_password.clone();
    let new_hash = tokio::task::spawn_blocking(move || hash_password(&password))
        .await
        .map_err(|e| AppError::Internal(format!("Task join error: {e}")))??;

    // Persist new password hash
    state
        .user_repo
        .update_password(current_user.id.as_str(), &new_hash)
        .await
        .map_err(|e| AppError::Internal(format!("Database error: {e}")))?;

    // Rotate JWT secret to invalidate all sessions
    let new_secret = state
        .jwt_service
        .rotate_secret()
        .map_err(|e| AppError::Internal(format!("Secret rotation error: {e}")))?;

    // Persist new secret to database
    state
        .user_repo
        .update_jwt_secret(current_user.id.as_str(), &new_secret)
        .await
        .map_err(|e| AppError::Internal(format!("Database error: {e}")))?;

    Ok(Json(ApiResponse::message("Password changed successfully")))
}

// ---------------------------------------------------------------------------
// POST /api/auth/refresh
// ---------------------------------------------------------------------------

async fn refresh_handler(
    State(state): State<AuthRouterState>,
    body: Result<Json<RefreshTokenRequest>, JsonRejection>,
) -> Result<Json<RefreshResponse>, AppError> {
    let Json(req) = body.map_err(|e| AppError::BadRequest(e.to_string()))?;

    let payload = state
        .jwt_service
        .verify(&req.token)
        .map_err(|_| AppError::Unauthorized("Invalid or expired token".into()))?;

    let new_token = state
        .jwt_service
        .sign(payload.user_id.as_str(), &payload.username)
        .map_err(|e| AppError::Internal(format!("Token signing error: {e}")))?;

    Ok(Json(RefreshResponse {
        success: true,
        token: new_token,
    }))
}

// ---------------------------------------------------------------------------
// GET /api/ws-token
// ---------------------------------------------------------------------------

async fn ws_token_handler(
    State(state): State<AuthRouterState>,
    Extension(current_user): Extension<CurrentUser>,
    headers: HeaderMap,
) -> Result<Json<WsTokenResponse>, AppError> {
    // Reuse the existing session token for WebSocket connections
    let token = extract_token_from_headers(&headers).ok_or_else(|| AppError::Unauthorized("No token found".into()))?;

    // Ensure user still exists
    state
        .user_repo
        .find_by_id(current_user.id.as_str())
        .await
        .map_err(|e| AppError::Internal(format!("Database error: {e}")))?
        .ok_or_else(|| AppError::Unauthorized("User not found".into()))?;

    // Cookie max age in milliseconds
    let expires_in = SESSION_MAX_AGE_SECONDS * 1000;

    Ok(Json(WsTokenResponse {
        success: true,
        ws_token: token,
        expires_in,
    }))
}

// ---------------------------------------------------------------------------
// POST /api/auth/qr-login
// ---------------------------------------------------------------------------

async fn qr_login_handler(
    State(state): State<AuthRouterState>,
    body: Result<Json<QrLoginRequest>, JsonRejection>,
) -> Result<Response, AppError> {
    let Json(req) = body.map_err(|e| AppError::BadRequest(e.to_string()))?;

    // Validate and consume QR token (one-time use)
    state.qr_token_store.validate_and_consume(&req.qr_token)?;

    // Get primary WebUI user for QR login
    let user = state
        .user_repo
        .get_primary_webui_user()
        .await
        .map_err(|e| AppError::Internal(format!("Database error: {e}")))?
        .ok_or_else(|| AppError::Internal("No primary user configured".into()))?;

    let token = state
        .jwt_service
        .sign(user.user_id.as_str(), &user.username)
        .map_err(|e| AppError::Internal(format!("Token signing error: {e}")))?;

    // Update last login (best-effort)
    if let Err(e) = state.user_repo.update_last_login(user.user_id.as_str()).await {
        tracing::warn!("Failed to update last login for {}: {e}", user.user_id);
    }

    let cookie = state.cookie_config.build_session_cookie(&token);
    let resp = LoginResponse::new(into_public_user(user)?, token);

    Ok(([(header::SET_COOKIE, cookie)], Json(resp)).into_response())
}

// ---------------------------------------------------------------------------
// GET /qr-login (static HTML page)
// ---------------------------------------------------------------------------

async fn qr_login_page() -> Html<&'static str> {
    Html(QR_LOGIN_HTML)
}

const QR_LOGIN_HTML: &str = r#"<!DOCTYPE html>
<html lang="en">
<head>
<meta charset="UTF-8">
<meta name="viewport" content="width=device-width, initial-scale=1.0">
<title>QR Login - Nomi</title>
<style>
  body { font-family: system-ui, sans-serif; display: flex; justify-content: center;
         align-items: center; min-height: 100vh; margin: 0; background: #f5f5f5; }
  .card { background: white; padding: 2rem; border-radius: 8px;
          box-shadow: 0 2px 8px rgba(0,0,0,0.1); text-align: center; max-width: 400px; }
  .status { margin-top: 1rem; color: #666; }
  .error { color: #d32f2f; }
  .success { color: #388e3c; }
</style>
</head>
<body>
<div class="card">
  <h1>Nomi</h1>
  <p id="status" class="status">Processing login...</p>
</div>
<script>
(function() {
  var el = document.getElementById('status');
  var params = new URLSearchParams(window.location.search);
  var token = params.get('token');
  if (!token) {
    el.textContent = 'Error: No token provided';
    el.className = 'status error';
    return;
  }
  function verifyAppShellThenRedirect() {
    fetch('/?nomifun_spa_shell_check=1', {
      method: 'GET',
      cache: 'no-store',
      credentials: 'same-origin'
    })
    .then(function(r) {
      if (!r.ok) {
        throw new Error('HTTP ' + r.status);
      }
      window.location.replace('/#/guid');
    })
    .catch(function(err) {
      el.textContent = 'Login succeeded, but WebUI app shell is not reachable: ' + err.message;
      el.className = 'status error';
    });
  }
  fetch('/api/auth/qr-login', {
    method: 'POST',
    headers: { 'Content-Type': 'application/json' },
    credentials: 'same-origin',
    body: JSON.stringify({ qr_token: token })
  })
  .then(function(r) { return r.json(); })
  .then(function(data) {
    if (data.success) {
      el.textContent = 'Login successful! Redirecting...';
      el.className = 'status success';
      try {
        sessionStorage.setItem('nomifun:qr-login-resume', JSON.stringify({
          at: Date.now(),
          user: data.user
        }));
      } catch (e) {}
      setTimeout(verifyAppShellThenRedirect, 600);
    } else {
      el.textContent = 'Login failed: ' + (data.error || 'Unknown error');
      el.className = 'status error';
    }
  })
  .catch(function(err) {
    el.textContent = 'Error: ' + err.message;
    el.className = 'status error';
  });
})();
</script>
</body>
</html>"#;

// ---------------------------------------------------------------------------
// WebUI admin credential endpoints (local-only)
// ---------------------------------------------------------------------------

/// Random password length for `/api/webui/reset-password`.
const RESET_PASSWORD_LEN: usize = 16;

/// Resolve the WebUI admin user, falling back to NotFound when absent.
async fn resolve_webui_admin(user_repo: &dyn IUserRepository) -> Result<User, AppError> {
    user_repo
        .get_primary_webui_user()
        .await
        .map_err(|e| AppError::Internal(format!("Database error: {e}")))?
        .ok_or_else(|| AppError::NotFound("No WebUI admin user configured".into()))
}

// ---------------------------------------------------------------------------
// POST /api/webui/change-password
// ---------------------------------------------------------------------------

async fn webui_change_password_handler(
    State(state): State<AuthRouterState>,
    body: Result<Json<WebuiChangePasswordRequest>, JsonRejection>,
) -> Result<Json<ApiResponse<()>>, AppError> {

    let Json(req) = body.map_err(|e| AppError::BadRequest(e.to_string()))?;

    validate_password(&req.new_password)?;

    let user = resolve_webui_admin(&*state.user_repo).await?;

    let password = req.new_password;
    let new_hash = tokio::task::spawn_blocking(move || hash_password(&password))
        .await
        .map_err(|e| AppError::Internal(format!("Task join error: {e}")))??;

    state
        .user_repo
        .update_password(user.user_id.as_str(), &new_hash)
        .await
        .map_err(|e| AppError::Internal(format!("Database error: {e}")))?;

    Ok(Json(ApiResponse::message("Password changed successfully")))
}

// ---------------------------------------------------------------------------
// POST /api/webui/change-username
// ---------------------------------------------------------------------------

async fn webui_change_username_handler(
    State(state): State<AuthRouterState>,
    body: Result<Json<WebuiChangeUsernameRequest>, JsonRejection>,
) -> Result<Json<ApiResponse<WebuiChangeUsernameResponse>>, AppError> {

    let Json(req) = body.map_err(|e| AppError::BadRequest(e.to_string()))?;

    let trimmed = req.new_username.trim().to_owned();
    validate_username(&trimmed)?;

    let user = resolve_webui_admin(&*state.user_repo).await?;

    if user.username != trimmed {
        state
            .user_repo
            .update_username(user.user_id.as_str(), &trimmed)
            .await
            .map_err(|e| AppError::Internal(format!("Database error: {e}")))?;
    }

    Ok(Json(ApiResponse::ok(WebuiChangeUsernameResponse { username: trimmed })))
}

// ---------------------------------------------------------------------------
// POST /api/webui/reset-password
// ---------------------------------------------------------------------------

async fn webui_reset_password_handler(
    State(state): State<AuthRouterState>,
) -> Result<Json<ApiResponse<WebuiResetPasswordResponse>>, AppError> {


    let user = resolve_webui_admin(&*state.user_repo).await?;

    let new_password = generate_password(RESET_PASSWORD_LEN);
    let password_for_hash = new_password.clone();
    let new_hash = tokio::task::spawn_blocking(move || hash_password(&password_for_hash))
        .await
        .map_err(|e| AppError::Internal(format!("Task join error: {e}")))??;

    state
        .user_repo
        .update_password(user.user_id.as_str(), &new_hash)
        .await
        .map_err(|e| AppError::Internal(format!("Database error: {e}")))?;

    Ok(Json(ApiResponse::ok(WebuiResetPasswordResponse { new_password })))
}

// ---------------------------------------------------------------------------
// POST /api/webui/generate-qr-token
// ---------------------------------------------------------------------------

async fn webui_generate_qr_token_handler(
    State(state): State<AuthRouterState>,
) -> Result<Json<ApiResponse<WebuiGenerateQrTokenResponse>>, AppError> {


    let (token, expires_at_ms) = state.qr_token_store.generate_with_expiry();

    Ok(Json(ApiResponse::ok(WebuiGenerateQrTokenResponse {
        token,
        expires_at_ms,
    })))
}

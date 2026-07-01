//! Desktop in-process serving: a PERMANENT loopback listener for the app's own
//! webview plus an ON-DEMAND LAN listener for remote browsers, sharing ONE
//! router that is built exactly once.
//!
//! Why two listeners instead of rebinding one (see the design doc):
//! - The loopback serve task is never touched by a LAN toggle, so the desktop
//!   webview's long-lived `/ws` and in-flight requests never blip.
//! - A LAN bind failure (port in use, firewall) is reported via [`WebUiStatus`]
//!   without affecting the already-serving loopback listener.
//! - `connect-info` (real peer IP for rate-limiting) and the SPA static
//!   fallback are added ONLY to the LAN listener, leaving the loopback path and
//!   the standalone-web/test paths byte-identical.
//!
//! Trust model: the router is built under [`AuthPolicy::TrustLocalToken`] with a
//! per-boot secret. The desktop injects that secret into its own webview
//! (`window.__nomiLocalTrust`), which presents it on every request — so the
//! desktop webview is trusted with no login while remote LAN browsers must log
//! in. Trust is the secret, NOT "arrived on loopback", so other local OS
//! accounts and same-host reverse proxies are not trusted.

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result};
use axum::Router;
use axum::extract::Request;
use axum::http::{StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use nomifun_auth::generate_random_hex_secret;
use tokio::net::TcpListener;
use tokio::runtime::Handle;
use tokio::sync::{Mutex, watch};
use tower_http::services::{ServeDir, ServeFile};

use crate::cli::Cli;
use crate::{AppServices, bootstrap, create_router};
use nomifun_auth::AuthPolicy;
use nomifun_db::IUserRepository;

/// Stable, bookmarkable port for the LAN listener (matches the UI's
/// `WEBUI_DEFAULT_PORT`). Falls back to an ephemeral port if occupied.
pub const WEBUI_LAN_PORT: u16 = 25808;

/// Snapshot of the WebUI serving state, surfaced to the desktop UI and emitted
/// on every change. Field names match the frontend `IWebUIStatus` /
/// `IWebUIStartResult` shapes.
#[derive(Clone, Debug, Default, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WebUiStatus {
    /// LAN listener is active (remote browsers can connect).
    pub running: bool,
    /// LAN listener port when running (else 0).
    pub port: u16,
    /// Whether remote (non-loopback) access is allowed.
    pub allow_remote: bool,
    /// `http://localhost:<loopback_port>` — the desktop's own origin.
    pub local_url: String,
    /// `http://<lan_ip>:<port>` — the primary/preferred LAN URL.
    pub network_url: Option<String>,
    /// A quick-access URL for EVERY non-loopback IPv4 NIC (`http://<ip>:<port>`),
    /// routing-preferred first. A multi-homed / VPN host has several; the UI
    /// lists them all so the user can pick whichever their other device reaches.
    pub network_urls: Vec<String>,
    /// Detected LAN/VPN IPv4 used to build the primary network URL.
    #[serde(rename = "lanIP")]
    pub lan_ip: Option<String>,
    /// Admin username for the remote login (default `admin`).
    pub admin_username: String,
    /// Whether a real admin password has been set (non-empty `password_hash`).
    /// Lets the desktop UI distinguish "credential configured (hidden)" from
    /// "never provisioned" even when the LAN server is stopped, so a persisted
    /// password is not perceived as lost after a restart.
    pub password_set: bool,
    /// One-time initial password, set ONLY in the direct return of a first
    /// `start` that provisioned credentials. Never broadcast on the status
    /// channel (so it is shown once, not persisted in memory).
    pub initial_password: Option<String>,
    /// Populated when the last start attempt failed (e.g. port in use).
    pub error: Option<String>,
}

struct LanListener {
    shutdown: watch::Sender<bool>,
    port: u16,
}

/// Owns the desktop's in-process backend serving. Construct with
/// [`DesktopServer::start`]; drive the LAN listener with the `*_blocking`
/// methods (safe to call from Tauri command threads).
///
/// Holds only `Send + Sync` handles so it can live in Tauri managed state. The
/// heavy, app-lifetime state (the exclusive data-dir lock and all service
/// handles the router depends on) is returned separately as [`DesktopKeepAlive`]
/// and held by the backend thread.
pub struct DesktopServer {
    loopback_port: u16,
    local_trust_secret: Arc<str>,
    /// Built once; cloned per listener.
    router: Router,
    /// Bundled SPA directory (`ui/dist`) served to remote browsers by the LAN
    /// listener in PRODUCTION. `None` disables remote serving of the app shell.
    spa_dir: Option<PathBuf>,
    /// In DEV, the vite dev-server URL (e.g. `http://localhost:5173`) the desktop
    /// webview itself loads. When set, the LAN listener PROXIES the SPA to it
    /// instead of serving the (stale) bundled `ui/dist`, so remote browsers get
    /// the exact same live frontend the desktop shows. `None` in production.
    dev_frontend_url: Option<Arc<str>>,
    /// Backend runtime handle, so Tauri command threads can drive async work.
    runtime: Handle,
    /// The singleton terminal service (live PTY map + session repo). Held here so
    /// the Tauri main thread can trigger a synchronous cleanup of all terminal
    /// sessions on real app exit (see [`shutdown_terminals_blocking`]).
    terminal_service: Arc<nomifun_terminal::TerminalService>,
    /// For the pre-LAN safety gate (refuse to expose before an admin exists).
    user_repo: Arc<dyn IUserRepository>,
    lan: Mutex<Option<LanListener>>,
    status_tx: watch::Sender<WebUiStatus>,
    status_rx: watch::Receiver<WebUiStatus>,
}

/// App-lifetime state that must outlive the server: the exclusive data-dir lock
/// (inside `ServerEnvironment`) and all long-lived service handles (MCP servers,
/// background tasks) the router depends on. The backend thread holds this for
/// the process lifetime; dropping it tears the backend down.
pub struct DesktopKeepAlive {
    _env: bootstrap::ServerEnvironment,
    _services: AppServices,
}

impl DesktopServer {
    /// Boot the embedded backend under `TrustLocalToken`, bind the permanent
    /// loopback listener, and spawn its serve task. Returns the shared handle
    /// plus a [`DesktopKeepAlive`] the caller MUST keep alive for the process
    /// lifetime; the loopback listener runs until the process exits.
    ///
    /// `spa_dir` is the bundled `ui/dist` directory used to serve the app shell
    /// to remote browsers in production. `dev_frontend_url` (e.g.
    /// `http://localhost:5173`) is set ONLY in dev: the LAN listener then proxies
    /// the SPA to the vite dev server so remote browsers match the live desktop.
    pub async fn start(
        cli: &Cli,
        merged_path: &str,
        spa_dir: Option<PathBuf>,
        dev_frontend_url: Option<String>,
    ) -> Result<(Arc<DesktopServer>, DesktopKeepAlive)> {
        let env = bootstrap::init_environment(cli, merged_path)?;

        // Override the CLI-derived policy: the desktop trusts its own webview via
        // a per-boot secret, and requires login for everyone else.
        // The desktop trusts its own webview via a per-boot secret presented in a
        // header (HTTP) and as a `Sec-WebSocket-Protocol` subprotocol (WS upgrade,
        // where browsers cannot set custom headers). The secret MUST therefore be
        // a valid subprotocol token — hex, not base64 (base64's `+`/`/`/`=` make
        // `new WebSocket(url, [secret])` throw, which silently killed every desktop
        // WebSocket → no live stream → the companion bubble never echoed).
        let secret: Arc<str> = Arc::from(generate_random_hex_secret().as_str());
        let mut config = env.config.clone();
        config.auth_policy = AuthPolicy::TrustLocalToken;
        config.local_trust_secret = Some(secret.clone());

        let database = bootstrap::init_data_layer(&config).await?;
        let services = AppServices::from_config(database, &config).await?;
        let user_repo = services.user_repo.clone();
        let terminal_service = services.terminal_service.clone();
        let router = create_router(&services).await;

        // Permanent loopback listener on an ephemeral port (today's behavior).
        let loopback = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
            .await
            .context("failed to bind loopback listener")?;
        let loopback_port = loopback.local_addr()?.port();

        {
            let router = router.clone();
            tokio::spawn(async move {
                if let Err(e) = axum::serve(loopback, router).await {
                    tracing::error!(error = %e, "desktop loopback server exited");
                }
            });
        }

        // Seed the initial status with the PERSISTED admin identity so the
        // desktop UI shows the real username / "password set" state immediately
        // at boot, before the LAN listener is ever started.
        let (initial_admin, initial_pw_set) = resolve_admin(&*user_repo).await;
        let initial = WebUiStatus {
            running: false,
            local_url: format!("http://localhost:{loopback_port}"),
            admin_username: initial_admin,
            password_set: initial_pw_set,
            ..Default::default()
        };
        let (status_tx, status_rx) = watch::channel(initial);

        tracing::info!(
            loopback_port,
            "desktop backend serving on loopback (LAN access off)"
        );

        let server = Arc::new(DesktopServer {
            loopback_port,
            local_trust_secret: secret,
            router,
            spa_dir,
            dev_frontend_url: dev_frontend_url.map(|u| Arc::from(u.trim_end_matches('/'))),
            runtime: Handle::current(),
            terminal_service,
            user_repo,
            lan: Mutex::new(None),
            status_tx,
            status_rx,
        });
        let keep_alive = DesktopKeepAlive {
            _env: env,
            _services: services,
        };
        Ok((server, keep_alive))
    }

    /// The loopback port the webview connects to (`window.__backendPort`).
    pub fn loopback_port(&self) -> u16 {
        self.loopback_port
    }

    /// The per-boot local-trust secret to inject into the webview
    /// (`window.__nomiLocalTrust`).
    pub fn local_trust_secret(&self) -> &str {
        &self.local_trust_secret
    }

    /// Current status snapshot.
    pub fn status(&self) -> WebUiStatus {
        self.status_rx.borrow().clone()
    }

    /// Status snapshot with the admin identity resolved FRESH from the DB.
    ///
    /// The cached watch value only carries the username while the LAN listener
    /// is running (it is set inside `start_lan`); a stopped or just-booted
    /// server would otherwise report an empty username and `password_set=false`,
    /// making a persisted credential look lost after a restart. This overlays
    /// the persisted `admin_username` / `password_set` so `getStatus` is always
    /// truthful regardless of LAN state.
    pub async fn status_snapshot(&self) -> WebUiStatus {
        let mut st = self.status();
        let (username, password_set) = resolve_admin(&*self.user_repo).await;
        st.admin_username = username;
        st.password_set = password_set;
        st
    }

    /// Subscribe to status changes (for emitting a Tauri event on each change).
    pub fn subscribe_status(&self) -> watch::Receiver<WebUiStatus> {
        self.status_rx.clone()
    }

    /// Start LAN serving (bind `0.0.0.0:WEBUI_LAN_PORT`). Awaitable directly
    /// from an async Tauri command — the LAN serve task is spawned on the
    /// backend runtime regardless of which runtime drives this call.
    pub async fn start_lan(self: &Arc<Self>) -> WebUiStatus {
        let mut lan = self.lan.lock().await;
        if lan.is_some() {
            return self.status();
        }

        if self.spa_dir.is_none() && self.dev_frontend_url.is_none() {
            return self.fail_start(
                "WebUI app shell not found; remote browsers would receive QR/API endpoints but no app shell"
                    .to_string(),
            );
        }

        // Provision an admin credential before exposing the LAN listener.
        // Without one, the first remote visitor could claim the empty-password
        // admin via `/api/auth/setup`. On first enable we generate a strong
        // password and return it ONCE (shown to the owner), mirroring the old
        // desktop behavior; thereafter the stored credential is reused.
        //
        // We fill ONLY the password (`set_system_user_password_if_uninitialized`),
        // never the username: the panel lets the user rename the admin while
        // WebUI is off (which leaves password_hash empty), so overwriting the
        // username here would silently revert their chosen name back to "admin".
        let has_password = self.user_repo.has_users().await.unwrap_or(false);
        let mut initial_password: Option<String> = None;
        if !has_password {
            let pw = nomifun_auth::generate_password(20);
            let pw_for_hash = pw.clone();
            let hashed = match tokio::task::spawn_blocking(move || {
                nomifun_auth::hash_password(&pw_for_hash)
            })
            .await
            {
                Ok(Ok(h)) => h,
                Ok(Err(e)) => return self.fail_start(format!("生成访问密码失败: {e}")),
                Err(e) => return self.fail_start(format!("生成访问密码失败: {e}")),
            };
            match self
                .user_repo
                .set_system_user_password_if_uninitialized(&hashed)
                .await
            {
                // Freshly provisioned — surface the one-time plaintext once.
                Ok(true) => initial_password = Some(pw),
                // A password already existed (race / prior enable): reuse it.
                Ok(false) => {}
                Err(e) => return self.fail_start(format!("写入访问密码失败: {e}")),
            }
        }

        // Build the LAN app: shared router (/api, /ws) + the app shell.
        // In dev → proxy the SPA to the vite dev server (live, matches desktop).
        // In prod → serve the bundled `ui/dist` statically.
        let mut app = self.router.clone();
        if let Some(dev_url) = &self.dev_frontend_url {
            let dev_url = dev_url.clone();
            app = app.fallback(move |req: Request| {
                let dev_url = dev_url.clone();
                async move { dev_spa_proxy(&dev_url, req).await }
            });
        } else if let Some(dir) = &self.spa_dir {
            app = app.fallback_service(
                ServeDir::new(dir)
                    .append_index_html_on_directories(true)
                    .fallback(ServeFile::new(dir.join("index.html"))),
            );
        }
        // DNS-rebinding guard on the LAN edge (Host/Origin must be IP/localhost).
        app = app.layer(axum::middleware::from_fn(host_guard_middleware));

        let lan_ips = detect_all_lan_ipv4s();
        let lan_ip = lan_ips.first().copied();

        let (port, listener) = match bind_lan(WEBUI_LAN_PORT).await {
            Ok(pair) => pair,
            Err(e) => return self.fail_start(format!("无法绑定局域网端口: {e}")),
        };

        let (sd_tx, mut sd_rx) = watch::channel(false);
        let make = app.into_make_service_with_connect_info::<SocketAddr>();
        self.runtime.spawn(async move {
            let shutdown = async move {
                while sd_rx.changed().await.is_ok() {
                    if *sd_rx.borrow() {
                        break;
                    }
                }
            };
            if let Err(e) = axum::serve(listener, make)
                .with_graceful_shutdown(shutdown)
                .await
            {
                tracing::error!(error = %e, "desktop LAN server exited");
            }
        });

        *lan = Some(LanListener {
            shutdown: sd_tx,
            port,
        });

        let (admin_username, password_set) = resolve_admin(&*self.user_repo).await;
        let network_url = lan_ip.as_ref().map(|ip| format!("http://{ip}:{port}"));
        let network_urls: Vec<String> = lan_ips
            .iter()
            .map(|ip| format!("http://{ip}:{port}"))
            .collect();

        // Broadcast the persistent status WITHOUT the one-time password (it must
        // not linger in the watch channel / future getStatus calls).
        let broadcast = WebUiStatus {
            running: true,
            port,
            allow_remote: true,
            local_url: format!("http://localhost:{}", self.loopback_port),
            network_url: network_url.clone(),
            network_urls: network_urls.clone(),
            lan_ip: lan_ip.as_ref().map(|ip| ip.to_string()),
            admin_username: admin_username.clone(),
            password_set,
            initial_password: None,
            error: None,
        };
        tracing::info!(port, "desktop LAN serving started");
        let _ = self.status_tx.send(broadcast.clone());

        // Direct return carries the one-time initial password (if just provisioned).
        WebUiStatus {
            initial_password,
            ..broadcast
        }
    }

    /// Build a failed-start status, broadcast it, and return it.
    fn fail_start(self: &Arc<Self>, error: String) -> WebUiStatus {
        let st = WebUiStatus {
            running: false,
            local_url: format!("http://localhost:{}", self.loopback_port),
            error: Some(error),
            ..Default::default()
        };
        let _ = self.status_tx.send(st.clone());
        st
    }

    /// Stop LAN serving and return the resulting status. The loopback listener
    /// (the desktop's own webview) is unaffected.
    pub async fn stop_lan(self: &Arc<Self>) -> WebUiStatus {
        let mut lan = self.lan.lock().await;
        if let Some(listener) = lan.take() {
            let _ = listener.shutdown.send(true);
            tracing::info!(port = listener.port, "desktop LAN serving stopped");
        }
        // Carry the persisted admin identity into the stopped status so the UI
        // keeps showing the real username / password-set state after stopping.
        let (admin_username, password_set) = resolve_admin(&*self.user_repo).await;
        let status = WebUiStatus {
            running: false,
            local_url: format!("http://localhost:{}", self.loopback_port),
            admin_username,
            password_set,
            ..Default::default()
        };
        let _ = self.status_tx.send(status.clone());
        status
    }

    /// Synchronously kill all live PTYs and delete all terminal session rows on
    /// real app exit. Called from the Tauri main thread (which is NOT a Tokio
    /// runtime thread) right before the process tears down, so the work must run
    /// on the backend runtime and this thread must block until it finishes.
    ///
    /// Uses a one-shot `std::sync::mpsc` channel rather than `Handle::block_on`
    /// (which panics inside some contexts): we spawn the async cleanup on the
    /// backend runtime and wait on the channel with a hard cap so a hung kill or
    /// DB write can never wedge the quit. Failures only warn — a best-effort wipe
    /// is acceptable (the OS reaps the process tree on exit either way).
    pub fn shutdown_terminals_blocking(&self) {
        let ts = self.terminal_service.clone();
        let (tx, rx) = std::sync::mpsc::channel();
        self.runtime.spawn(async move {
            let r = tokio::time::timeout(std::time::Duration::from_secs(3), ts.shutdown_cleanup()).await;
            let _ = tx.send(r);
        });
        match rx.recv_timeout(std::time::Duration::from_secs(4)) {
            Ok(Ok(Ok(n))) => tracing::info!(deleted = n, "terminal sessions cleaned up on exit"),
            Ok(Ok(Err(e))) => tracing::warn!(error = %e, "terminal exit cleanup failed"),
            Ok(Err(_)) => tracing::warn!("terminal exit cleanup timed out (3s) — proceeding with quit"),
            Err(e) => tracing::warn!(error = %e, "terminal exit cleanup did not report back — proceeding with quit"),
        }
    }
}

/// Bind `0.0.0.0:preferred` for the LAN listener, falling back through a bounded
/// port scan to an ephemeral port. Delegates to the shared `bind_with_fallback`
/// so the desktop LAN listener, `nomifun-web`, and the `nomicore` bin fail over
/// identically.
async fn bind_lan(preferred: u16) -> Result<(u16, TcpListener)> {
    crate::bootstrap::bind_with_fallback(IpAddr::V4(Ipv4Addr::UNSPECIFIED), preferred).await
}

/// Resolve the persisted admin identity from the DB: `(username, password_set)`.
///
/// `password_set` is true when the system user has a non-empty `password_hash`
/// (i.e. a real credential exists). Falls back to `("admin", false)` on any DB
/// error or missing row, so callers always get a displayable username.
async fn resolve_admin(user_repo: &dyn IUserRepository) -> (String, bool) {
    match user_repo.get_system_user().await {
        Ok(Some(u)) => {
            let name = if u.username.is_empty() { "admin".to_string() } else { u.username };
            (name, !u.password_hash.trim().is_empty())
        }
        _ => ("admin".to_string(), false),
    }
}

/// Routing-preferred source IPv4 (the address used to reach off-box hosts).
/// Connecting a UDP socket only sets its local address; no packets are sent.
fn routing_primary_ipv4() -> Option<Ipv4Addr> {
    let sock = std::net::UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0)).ok()?;
    sock.connect((Ipv4Addr::new(8, 8, 8, 8), 80)).ok()?;
    match sock.local_addr().ok()? {
        SocketAddr::V4(a) => {
            let ip = *a.ip();
            is_webui_lan_ip_candidate(ip).then_some(ip)
        }
        _ => None,
    }
}

fn is_webui_lan_ip_candidate(ip: Ipv4Addr) -> bool {
    let octets = ip.octets();
    if ip.is_loopback() || ip.is_unspecified() || ip.is_link_local() || ip.is_multicast() {
        return false;
    }
    if octets == [255, 255, 255, 255] {
        return false;
    }
    // RFC 2544 benchmarking addresses are commonly created by virtual network
    // adapters and are not reachable from a phone on the user's LAN.
    if octets[0] == 198 && (octets[1] == 18 || octets[1] == 19) {
        return false;
    }
    // Documentation-only ranges should never be offered as a real WebUI target.
    if (octets[0] == 192 && octets[1] == 0 && octets[2] == 2)
        || (octets[0] == 198 && octets[1] == 51 && octets[2] == 100)
        || (octets[0] == 203 && octets[1] == 0 && octets[2] == 113)
    {
        return false;
    }
    true
}

/// All WebUI-usable IPv4 NIC addresses — routing-preferred first, then the rest
/// (private ranges before public). A multi-homed / VPN host still yields
/// several; obvious virtual/special-purpose addresses are filtered out.
fn detect_all_lan_ipv4s() -> Vec<Ipv4Addr> {
    let mut addrs: Vec<Ipv4Addr> = Vec::new();
    if let Some(primary) = routing_primary_ipv4() {
        addrs.push(primary);
    }
    let mut rest: Vec<Ipv4Addr> = Vec::new();
    if let Ok(ifaces) = if_addrs::get_if_addrs() {
        for iface in ifaces {
            if iface.is_loopback() {
                continue;
            }
            if let IpAddr::V4(v4) = iface.ip()
                && is_webui_lan_ip_candidate(v4)
                && !addrs.contains(&v4)
                && !rest.contains(&v4)
            {
                rest.push(v4);
            }
        }
    }
    rest.sort_by_key(|ip| !ip.is_private()); // private (false→0) first
    addrs.extend(rest);
    addrs
}

/// Reverse-proxy a SPA request to the vite dev server (DEV only) so remote
/// browsers receive the exact live frontend the desktop webview loads — instead
/// of a stale bundled `ui/dist`. Only reached for paths the backend router did
/// not handle (non-`/api` SPA assets); vite's own SPA fallback serves
/// `index.html` for client routes.
async fn dev_spa_proxy(dev_url: &str, req: Request) -> Response {
    let path_q = req
        .uri()
        .path_and_query()
        .map(|p| p.as_str())
        .unwrap_or("/");
    let target = format!("{dev_url}{path_q}");
    let client = reqwest::Client::builder()
        .no_proxy()
        .build()
        .unwrap_or_else(|_| reqwest::Client::new());
    match client.get(&target).send().await {
        Ok(resp) => {
            let status = resp.status().as_u16();
            let ctype = resp
                .headers()
                .get(reqwest::header::CONTENT_TYPE)
                .and_then(|v| v.to_str().ok())
                .map(|s| s.to_owned());
            let body = resp.bytes().await.unwrap_or_default();
            let mut builder = Response::builder().status(status);
            if let Some(ct) = ctype {
                builder = builder.header(header::CONTENT_TYPE, ct);
            }
            builder
                .body(axum::body::Body::from(body))
                .unwrap_or_else(|_| StatusCode::BAD_GATEWAY.into_response())
        }
        Err(e) => {
            tracing::warn!(error = %e, %target, "dev SPA proxy to vite failed");
            (StatusCode::BAD_GATEWAY, "dev frontend (vite) not reachable").into_response()
        }
    }
}

/// Reject requests whose `Host` (or, if present, `Origin`) is a DNS name rather
/// than an IP literal or `localhost`. This blocks DNS-rebinding against the
/// LAN-exposed server while permitting all IP-based access (the URL/QR the app
/// advertises is always IP-based). Only applied to the LAN listener.
async fn host_guard_middleware(request: Request, next: Next) -> Result<Response, StatusCode> {
    let headers = request.headers();
    if let Some(host) = headers.get(header::HOST).and_then(|v| v.to_str().ok())
        && !host_is_ip_or_localhost(host)
    {
        return Err(StatusCode::FORBIDDEN);
    }
    if let Some(origin) = headers.get(header::ORIGIN).and_then(|v| v.to_str().ok())
        && !origin_is_ip_or_localhost(origin)
    {
        return Err(StatusCode::FORBIDDEN);
    }
    Ok(next.run(request).await)
}

/// True if `host` (possibly `host:port`) is an IP literal or `localhost`.
fn host_is_ip_or_localhost(host: &str) -> bool {
    let bare = strip_port(host);
    bare.eq_ignore_ascii_case("localhost") || bare.parse::<IpAddr>().is_ok()
}

fn origin_is_ip_or_localhost(origin: &str) -> bool {
    // origin = scheme://host[:port]
    let after_scheme = origin.split("://").nth(1).unwrap_or(origin);
    host_is_ip_or_localhost(after_scheme)
}

/// Strip a trailing `:port` from a host, handling bracketed IPv6 (`[::1]:80`).
fn strip_port(host: &str) -> &str {
    if let Some(rest) = host.strip_prefix('[') {
        // [ipv6]:port → ipv6
        return rest.split(']').next().unwrap_or(rest);
    }
    match host.rsplit_once(':') {
        Some((h, _)) => h,
        None => host,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn host_guard_allows_ip_and_localhost() {
        assert!(host_is_ip_or_localhost("127.0.0.1"));
        assert!(host_is_ip_or_localhost("127.0.0.1:25808"));
        assert!(host_is_ip_or_localhost("192.168.1.5:25808"));
        assert!(host_is_ip_or_localhost("localhost:25808"));
        assert!(host_is_ip_or_localhost("[::1]:25808"));
    }

    #[test]
    fn host_guard_rejects_domain_names() {
        assert!(!host_is_ip_or_localhost("evil.com"));
        assert!(!host_is_ip_or_localhost("attacker.example:25808"));
        assert!(!host_is_ip_or_localhost("nomi.local"));
    }

    #[test]
    fn origin_shape_check() {
        assert!(origin_is_ip_or_localhost("http://192.168.1.5:25808"));
        assert!(!origin_is_ip_or_localhost("http://evil.com"));
    }

    #[test]
    fn webui_lan_ip_candidate_filters_special_purpose_ranges() {
        assert!(!is_webui_lan_ip_candidate(Ipv4Addr::new(0, 0, 0, 0)));
        assert!(!is_webui_lan_ip_candidate(Ipv4Addr::new(127, 0, 0, 1)));
        assert!(!is_webui_lan_ip_candidate(Ipv4Addr::new(169, 254, 10, 20)));
        assert!(!is_webui_lan_ip_candidate(Ipv4Addr::new(198, 18, 0, 1)));
        assert!(!is_webui_lan_ip_candidate(Ipv4Addr::new(198, 19, 255, 1)));
        assert!(!is_webui_lan_ip_candidate(Ipv4Addr::new(224, 0, 0, 1)));
        assert!(!is_webui_lan_ip_candidate(Ipv4Addr::new(
            255, 255, 255, 255
        )));
    }

    #[test]
    fn webui_lan_ip_candidate_keeps_private_lan_ranges() {
        assert!(is_webui_lan_ip_candidate(Ipv4Addr::new(10, 8, 0, 2)));
        assert!(is_webui_lan_ip_candidate(Ipv4Addr::new(172, 16, 1, 20)));
        assert!(is_webui_lan_ip_candidate(Ipv4Addr::new(192, 168, 31, 5)));
    }
}

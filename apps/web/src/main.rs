//! `nomifun-web` — the standalone Web host for the browser deployment.
//!
//! In the unified architecture there is ONE Rust backend (`nomifun-app`, the
//! former `nomicore`). It runs in two host modes:
//!   * embedded in the Tauri desktop shell (`apps/desktop`), started in-process
//!     on a localhost port in `--local` (no-auth) mode — the shell IS the trust
//!     boundary, so no login is required;
//!   * here, as a standalone server that ALSO serves the built SPA (`ui/dist`)
//!     so browsers hit the same HTTP API. This replaces the old Node `web-host`.
//!
//! Unlike the desktop shell, this host is reachable over the network, so it
//! boots the backend in AUTHENTICATED mode by default (login required). On a
//! fresh data dir it provisions the first admin out-of-band (see
//! `ensure_admin_credentials`), because the in-band setup endpoints are
//! local-only. `--insecure-no-auth` opts back into desktop-style no-auth for a
//! host that is only reachable over loopback / a trusted private network.
//!
//! This host boots the backend **in-process** (same binary), composes its `/api`
//! router with a static `ServeDir` fallback for the SPA, and serves both on one
//! port. Env mutation + runtime init happen before the tokio runtime starts,
//! mirroring the `nomicore` bin's ordering.

use std::net::{IpAddr, SocketAddr};
use std::path::PathBuf;
use std::process::ExitCode;

use anyhow::{Context, Result};
use clap::Parser;
use tower_http::services::{ServeDir, ServeFile};
use tower_http::trace::TraceLayer;

/// Env var that, when truthy, opts into `--insecure-no-auth` without the flag.
const ENV_INSECURE_NO_AUTH: &str = "NOMIFUN_WEB_INSECURE_NO_AUTH";

#[derive(Parser, Debug)]
#[command(
    name = "nomifun-web",
    about = "NomiFun unified Web host (SPA + backend API)"
)]
struct Args {
    /// Host/IP address to bind on. Defaults to loopback; use `0.0.0.0` to accept
    /// connections from other machines (do so behind a trusted gateway / private
    /// network — see `--insecure-no-auth`).
    #[arg(long, env = "NOMIFUN_WEB_HOST", default_value = "127.0.0.1")]
    host: String,
    /// Port to listen on (serves both the API and the SPA).
    #[arg(long, env = "NOMIFUN_WEB_PORT", default_value_t = 8787)]
    port: u16,
    /// Data directory for the backend (db + storage). Defaults to the same
    /// per-user dir as the desktop shell (`%LOCALAPPDATA%\NomiFun\Nomi` on
    /// Windows, see `nomifun_app::cli::default_data_dir`) so every host and
    /// dev loop shares one state by default. The env value is taken literally
    /// (no `/Nomi` suffix) — production deployments (Docker `/data`, systemd
    /// `/var/lib/nomifun`) rely on that.
    #[arg(
        long,
        env = "NOMIFUN_DATA_DIR",
        default_value_os_t = nomifun_app::cli::default_data_dir(),
        value_parser = nomifun_app::cli::parse_non_empty_path
    )]
    data_dir: PathBuf,
    /// Directory containing the built SPA (ui/dist).
    #[arg(long, env = "NOMIFUN_WEB_DIST", default_value = "../../ui/dist")]
    dist: PathBuf,
    /// Serve only the backend/API surface. Intended for the Vite-proxied WebUI
    /// development loop, where mounting ui/dist would expose a stale bundle on
    /// the backend port.
    #[arg(long)]
    api_only: bool,
    /// DANGER: run the backend in local mode — authentication is fully DISABLED
    /// and every client acts as a privileged user with shell/file/agent access.
    /// Only for a host reachable solely over loopback or a trusted private
    /// network. Without this flag the web host requires a login (safe default).
    /// Can also be enabled via `NOMIFUN_WEB_INSECURE_NO_AUTH=true`.
    #[arg(long)]
    insecure_no_auth: bool,
    /// Initial admin username provisioned on first run (authenticated mode only).
    /// Ignored once an admin exists.
    #[arg(long, env = "NOMIFUN_ADMIN_USERNAME", default_value = "admin")]
    admin_user: String,
    /// Initial admin password provisioned on first run (authenticated mode only).
    /// If omitted, no admin is pre-seeded: the install is left uninitialised and
    /// the first WebUI visitor creates the admin interactively via first-run
    /// setup (`POST /api/auth/setup`). Ignored once an admin exists.
    #[arg(long, env = "NOMIFUN_ADMIN_PASSWORD")]
    admin_password: Option<String>,
}

/// Parse a truthy env value (`1`/`true`/`yes`/`on`, case-insensitive).
fn env_flag(name: &str) -> bool {
    std::env::var(name)
        .map(|v| {
            matches!(
                v.trim().to_ascii_lowercase().as_str(),
                "1" | "true" | "yes" | "on"
            )
        })
        .unwrap_or(false)
}

fn validate_static_dist(args: &Args) -> Result<Option<nomifun_app::bootstrap::UiBuildManifest>> {
    validate_static_dist_with_build_id(args, option_env!("NOMIFUN_FRONTEND_BUILD_ID"))
}

fn validate_static_dist_with_build_id(
    args: &Args,
    expected_build_id: Option<&str>,
) -> Result<Option<nomifun_app::bootstrap::UiBuildManifest>> {
    if args.api_only {
        return Ok(None);
    }

    let expected_build_id = expected_build_id.context(
        "this nomifun-web binary has no exact frontend build identity and cannot serve static assets; run `bun run serve:web` (or build with `--features static-webui`) after `bun run build:ui`",
    )?;

    nomifun_app::bootstrap::validate_webui_dist(
        &args.dist,
        env!("CARGO_PKG_VERSION"),
        Some(expected_build_id),
    )
    .map(Some)
    .with_context(|| format!("refusing to serve incompatible WebUI assets from {}", args.dist.display()))
}

fn main() -> Result<ExitCode> {
    // If an ACP agent CLI spawned this binary as an MCP stdio bridge
    // (`current_exe() mcp-requirement-stdio` etc.), run that helper and exit
    // BEFORE clap parses our own Args (which would reject the subcommand) and
    // before any backend/server init. Every host binary must honor these or the
    // injected declaration tools (requirement_complete / team / guide) never
    // appear in the agent's session.
    if let Some(code) = nomifun_app::commands::run_mcp_stdio_subcommand_if_present() {
        return Ok(code);
    }

    let args = Args::parse();
    // Fail before runtime, database, and auth initialization. API-only mode is
    // the explicit Vite-development bypass and never mounts the static bundle.
    let _static_manifest = validate_static_dist(&args)?;

    // Authentication is ON by default; `--insecure-no-auth` (or the env var)
    // opts into the desktop-style no-auth local mode. The env is read manually
    // to avoid clap's bool+env flag ambiguity.
    let insecure_no_auth = args.insecure_no_auth || env_flag(ENV_INSECURE_NO_AUTH);

    // Build a fully-defaulted backend CLI without touching this process's argv,
    // then override the bits this host owns. `parse_from` gives a defaulted Cli.
    let mut cli = nomifun_app::cli::Cli::parse_from(["nomifun-web"]);
    cli.host = args.host.clone();
    cli.port = args.port;
    cli.data_dir = args.data_dir.clone();
    cli.local = insecure_no_auth;

    // Same ordering as the nomicore bin: runtime init + PATH enhancement BEFORE
    // any worker thread / tokio runtime exists.
    nomifun_runtime::init(&cli.data_dir);
    // SAFETY: called before the tokio runtime (and its threads) is built.
    let merged_path = unsafe { nomifun_runtime::enhance_process_path() };

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    runtime.block_on(serve(cli, merged_path, args))
}

async fn serve(cli: nomifun_app::cli::Cli, merged_path: String, args: Args) -> Result<ExitCode> {
    // Resolve the bind address up front so a bad --host fails fast with a clear
    // message instead of a cryptic socket error.
    let ip: IpAddr = args.host.parse().with_context(|| {
        format!(
            "invalid --host '{}': expected an IP like 127.0.0.1 or 0.0.0.0",
            args.host
        )
    })?;
    if !ip.is_loopback() && cli.local {
        tracing::warn!(
            %ip,
            "binding a non-loopback address with --insecure-no-auth: authentication is DISABLED — \
             anyone who can reach this port gets full host access (shell, files, agents). \
             Put a trusted gateway in front, use a private network, or drop --insecure-no-auth."
        );
    }

    // Boot the backend in-process (env → data layer → services), then mount the
    // real API router with the SPA as the fallback for non-/api routes.
    let env = nomifun_app::bootstrap::init_environment(&cli, &merged_path)?;
    let database = nomifun_app::bootstrap::init_data_layer(&env.config).await?;
    let services = nomifun_app::AppServices::from_config(database, &env.config).await?;
    nomifun_app::bootstrap::finalize_data_layer(&env.config)?;

    // First-run admin provisioning. No-op in local mode and once an admin
    // exists; otherwise a fresh authenticated install would have no way to set
    // the first password (the in-band setup routes are local-only). Returns
    // whether the install still awaits interactive first-run setup.
    let needs_first_run_setup = nomifun_app::bootstrap::ensure_admin_credentials(
        &services,
        nomifun_app::bootstrap::AdminBootstrap {
            username: Some(args.admin_user.clone()),
            password: args.admin_password.clone(),
        },
    )
    .await?;
    if needs_first_run_setup && !ip.is_loopback() {
        tracing::warn!(
            %ip,
            "first-run setup is OPEN on a non-loopback address: the NEXT client to reach this \
             port will create the admin account. Complete setup over a trusted network/tunnel \
             first, or pre-seed with NOMIFUN_ADMIN_PASSWORD."
        );
    }

    let mut app = nomifun_app::create_router(&services).await;
    if !args.api_only {
        app = app.fallback_service(
            ServeDir::new(&args.dist)
                .append_index_html_on_directories(true)
                .fallback(ServeFile::new(args.dist.join("index.html"))),
        );
    }
    let app = app.layer(TraceLayer::new_for_http());

    let addr = SocketAddr::new(ip, args.port);
    tracing::info!(
        requested = %addr,
        auth = if cli.local { "disabled (insecure-no-auth)" } else { "required" },
        static_mode = if args.api_only { "api-only" } else { "spa" },
        dist = ?if args.api_only { None } else { Some(&args.dist) },
        "nomifun-web: embedded backend + SPA on one port"
    );
    // Port failover: if `args.port` is taken, bind a bounded-scan neighbour (or
    // an ephemeral port) instead of hard-failing, then announce the actually
    // bound port via `{data_dir}/port.json` + stdout so the operator/launcher
    // can re-point clients — a browser cannot self-discover a moved port.
    let (actual_port, listener) = nomifun_app::bootstrap::bind_with_fallback(ip, args.port).await?;
    if actual_port != args.port {
        tracing::warn!(
            requested = args.port,
            actual = actual_port,
            "preferred port was busy; bound a fallback port"
        );
    }
    nomifun_app::bootstrap::announce_bound_port(&cli.data_dir, &args.host, actual_port);
    axum::serve(listener, app).await?;

    services.database.close().await;
    drop(env);
    Ok(ExitCode::SUCCESS)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn api_only_flag_disables_static_spa_mode() {
        let args = Args::try_parse_from(["nomifun-web", "--api-only"]).expect("parse --api-only");

        assert!(args.api_only);
    }

    #[test]
    fn api_only_mode_does_not_require_a_static_distribution() {
        let args = Args::try_parse_from([
            "nomifun-web",
            "--api-only",
            "--dist",
            "/definitely/missing/nomifun-ui-dist",
        ])
        .expect("parse API-only args");

        assert!(validate_static_dist(&args).expect("API-only mode should bypass static validation").is_none());
    }

    #[test]
    fn static_mode_rejects_a_missing_distribution_before_server_boot() {
        let args = Args::try_parse_from([
            "nomifun-web",
            "--dist",
            "/definitely/missing/nomifun-ui-dist",
        ])
        .expect("parse static args");

        let error = validate_static_dist_with_build_id(&args, Some("paired-test-build"))
            .expect_err("static mode must reject a missing distribution");
        let error_text = format!("{error:#}");
        assert!(error_text.contains("index.html"), "unexpected error: {error_text}");
    }

    #[test]
    fn static_mode_rejects_a_host_without_an_exact_embedded_build_id() {
        let args = Args::try_parse_from(["nomifun-web", "--dist", "ui/dist"])
            .expect("parse static args");

        let error = validate_static_dist_with_build_id(&args, None)
            .expect_err("an API-only host binary must never serve static assets");
        let error_text = format!("{error:#}");
        assert!(error_text.contains("exact frontend build identity"), "unexpected error: {error_text}");
        assert!(error_text.contains("serve:web"), "unexpected error: {error_text}");
    }

    #[cfg(feature = "static-webui")]
    #[test]
    fn static_webui_feature_accepts_only_its_paired_distribution() {
        let dist = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../ui/dist");
        let args = Args::try_parse_from([
            "nomifun-web",
            "--dist",
            dist.to_str().expect("workspace path must be UTF-8"),
        ])
        .expect("parse paired static args");

        let manifest = validate_static_dist(&args)
            .expect("the freshly built static host and UI must be paired")
            .expect("static mode must return its manifest");
        assert_eq!(
            manifest.frontend_build_id,
            option_env!("NOMIFUN_FRONTEND_BUILD_ID")
                .expect("static-webui builds must embed an exact frontend build ID")
        );
    }
}

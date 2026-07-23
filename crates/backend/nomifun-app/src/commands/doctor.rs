//! `nomicore doctor` subcommand: agent CLI detection self-check.
//!
//! Hydrates the agent registry against the real on-disk database and
//! prints a per-agent availability table to stdout. Mirrors the
//! server's PATH probing path exactly — `main` runs the same
//! `nomifun_runtime::init` + `enhance_process_path` for `Doctor` as it
//! does for the server, so the bundled `bun` resolves through the
//! same cache the server uses.
//!
//! Writes its report to stdout so an interactive caller does not need to grep
//! logs. Unlike the old shortcut, it enters the same locked v3 reset/database
//! gate as the server before hydrating the real catalog. This is intentionally
//! not a read-only bypass: a receipt-valid legacy database must be retired (or
//! fail closed) before any writable database initialization occurs.

use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;

use anyhow::Result;

use nomifun_ai_agent::{AgentRegistry, UnavailableReason};
use nomifun_db::{IAgentMetadataRepository, SqliteAgentMetadataRepository};

use crate::cli::Cli;

pub async fn run_doctor(cli: &Cli, merged_path: &str) -> Result<ExitCode> {
    print_environment(merged_path, &cli.data_dir);

    // Hold the same exclusive data-dir lock and execute the same pre-open v3
    // lifecycle/probe as server startup. In particular, never call
    // `init_database` directly against receipt-claimed data.
    let environment = crate::bootstrap::init_environment(cli, merged_path)?;
    let database = environment.init_doctor_data_layer().await?;

    let repo: Arc<dyn IAgentMetadataRepository> = Arc::new(SqliteAgentMetadataRepository::new(database.pool().clone()));
    let registry = AgentRegistry::new(repo);
    registry
        .hydrate()
        .await
        .map_err(|e| anyhow::anyhow!("failed to hydrate agent registry: {e}"))?;

    let snapshot = registry.diagnostic_snapshot().await;
    print_snapshot(&snapshot);

    database.close().await;
    drop(environment);

    Ok(ExitCode::SUCCESS)
}

fn print_environment(merged_path: &str, data_dir: &Path) {
    let path_segments = merged_path.split(if cfg!(windows) { ';' } else { ':' }).count();
    println!("Nomi backend doctor — agent CLI detection self-check");
    println!("  data-dir       : {}", data_dir.display());
    println!("  PATH segments  : {path_segments}");
    println!("  PATH length    : {}", merged_path.len());
    if let Some(p) = std::env::var_os("NOMIFUN_BUN_PATH") {
        println!("  NOMIFUN_BUN_PATH: {}", PathBuf::from(p).display());
    }
    println!();
}

fn print_snapshot(snapshot: &[(nomifun_api_types::AgentMetadata, Option<UnavailableReason>)]) {
    let total = snapshot.len();
    let available = snapshot.iter().filter(|(m, _)| m.available).count();
    let unavailable = total - available;

    println!("Agents in catalog: {total}  available: {available}  unavailable: {unavailable}");
    println!();
    println!(
        "{:<32} {:<10} {:<14} {:<10} REASON / RESOLVED",
        "ID", "BACKEND", "SOURCE", "STATUS"
    );
    println!("{}", "-".repeat(110));

    for (meta, reason) in snapshot {
        let backend = meta.backend.as_deref().unwrap_or("-");
        let source = format!("{:?}", meta.agent_source);
        let status = if meta.available { "available" } else { "missing" };
        let trailer = match (meta.available, reason) {
            (true, _) => meta
                .resolved_command
                .as_deref()
                .map(|p| p.display().to_string())
                .unwrap_or_else(|| "<internal>".to_owned()),
            (false, Some(r)) => describe_reason(r),
            (false, None) => "unavailable (no reason recorded — registry bug)".to_owned(),
        };
        println!(
            "{:<32} {:<10} {:<14} {:<10} {}",
            meta.agent_id, backend, source, status, trailer
        );
    }

    if unavailable > 0 {
        println!();
        println!("Tip: rows marked `missing` could not resolve their CLI on $PATH from this shell.");
        println!("     If a CLI is installed but missing here, the Electron app may inherit a different PATH —");
        println!("     reproduce by launching the app from this same shell or check launchctl/setenv setup.");
    }
}

fn describe_reason(reason: &UnavailableReason) -> String {
    match reason {
        UnavailableReason::Disabled => "disabled by user".to_owned(),
        UnavailableReason::NoCommand => "no spawn command configured (seed data bug)".to_owned(),
        UnavailableReason::BridgeMissing { bridge } => format!("bridge `{bridge}` not on $PATH"),
        UnavailableReason::PrimaryMissing { binary } => format!("CLI `{binary}` not on $PATH"),
        UnavailableReason::CommandMissing { command } => format!("`{command}` not on $PATH"),
    }
}

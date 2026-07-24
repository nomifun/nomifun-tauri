//! CLI argument definitions for the `nomicore` binary.
//!
//! Kept separate from `main.rs` to isolate the clap surface (struct + enum +
//! attribute soup) from the runtime entry point. Visibility is `pub(crate)`
//! because only `main.rs` consumes it.

use std::path::PathBuf;

use clap::{Parser, Subcommand};

/// The default data directory shared by all hosts built for the same channel
/// (desktop shell, `nomifun-web`, the `nomicore` bin): the per-user
/// application-data dir joined with `NomiFun/Nomi<channel-suffix>`. Stable
/// builds use `Nomi`; dev builds use `Nomi-dev`. Extreme fallback when the OS
/// reports no user dir: `<system temp>/nomifun-data/Nomi<channel-suffix>`.
///
/// Sharing within a channel is deliberate, while isolating non-stable channels
/// prevents development loops from touching installed-app state. The
/// `NOMIFUN_DATA_DIR` env / `--data-dir` flag remain the escape hatch for an
/// explicitly selected directory. Concurrent use of one dir is prevented by
/// the exclusive server lock (see `bootstrap::server_lock`).
///
/// This is only the *unset* default — it does NOT consult `NOMIFUN_DATA_DIR`.
/// Env semantics stay host-specific: the desktop shell appends `/Nomi` to the
/// env value, web/nomicore take it literally (clap `env` binding).
pub fn default_data_dir() -> PathBuf {
    dirs::data_local_dir()
        .map(|dir| dir.join("NomiFun"))
        .unwrap_or_else(|| std::env::temp_dir().join("nomifun-data"))
        .join(nomi_leaf(&crate::channel::dir_suffix()))
}

/// The data-dir leaf for the active build channel: `Nomi` on stable, `Nomi-dev`
/// (etc.) on non-stable channels. The channel suffix attaches to the `Nomi`
/// leaf — NOT to the `NomiFun` vendor segment — so a non-stable build lands in a
/// sibling directory next to the production one (`…/NomiFun/Nomi-dev`), keeping
/// dev state fully isolated from the installed app. Pure, for unit testing;
/// only `default_data_dir`'s unset default uses it (explicit `NOMIFUN_DATA_DIR`
/// is taken verbatim by clap, channel-agnostic).
fn nomi_leaf(suffix: &str) -> String {
    format!("Nomi{suffix}")
}

/// Reject empty `--data-dir` / `NOMIFUN_DATA_DIR` values. clap's env binding
/// takes an empty env var (a common `.env` slip) literally, which would
/// resolve the data dir to `""` — scattering a `./logs` dir into the CWD
/// before failing cryptically. `NOMIFUN_WORK_DIR` already gets the same
/// non-empty filter in `bootstrap::work_dir`.
pub fn parse_non_empty_path(s: &str) -> Result<PathBuf, String> {
    if s.trim().is_empty() {
        return Err(
            "must not be empty (unset NOMIFUN_DATA_DIR instead of setting it to an empty string)"
                .into(),
        );
    }
    Ok(PathBuf::from(s))
}

#[derive(Parser)]
#[command(name = "nomicore", about = "Nomi Backend Server", version)]
pub struct Cli {
    /// Host address to listen on.
    #[arg(long, default_value_t = String::from(nomifun_common::constants::DEFAULT_HOST))]
    pub host: String,

    /// Port number to listen on.
    #[arg(long, default_value_t = nomifun_common::constants::DEFAULT_PORT)]
    pub port: u16,

    /// Data directory for database and file storage.
    #[arg(long, env = "NOMIFUN_DATA_DIR", default_value_os_t = default_data_dir(), value_parser = parse_non_empty_path)]
    pub data_dir: PathBuf,

    /// Working directory for conversation workspaces.
    /// Falls back to NOMIFUN_WORK_DIR env, then to data-dir.
    #[arg(long)]
    pub work_dir: Option<PathBuf>,

    /// Host application version used for extension engine compatibility.
    #[arg(long, default_value_t = env!("CARGO_PKG_VERSION").to_string())]
    pub app_version: String,

    /// Run in local embedded mode (skip authentication and use the
    /// database-resolved installation owner).
    #[arg(long)]
    pub local: bool,

    /// Directory for log files. Defaults to {data-dir}/logs/.
    #[arg(long)]
    pub log_dir: Option<PathBuf>,

    /// Log level filter (e.g. "info", "debug", "info,nomifun_mcp=trace").
    #[arg(long)]
    pub log_level: Option<String>,

    #[command(subcommand)]
    pub command: Option<Command>,
}

// `Mcp` prefix is load-bearing on Mcp* variants — clap derives kebab-case
// subcommand names (`mcp-requirement-stdio`, etc.) that external callers
// (ACP agent CLI, injected MCP bridge specs) depend on verbatim.
#[derive(Subcommand)]
pub enum Command {
    /// MCP stdio server for AutoWork requirement declaration tools
    /// (`requirement_complete` / `requirement_update_status`; spawned by the ACP agent CLI).
    McpRequirementStdio,
    /// MCP stdio server for the per-session knowledge-search tool
    /// (`knowledge_search`; spawned by the ACP agent CLI when knowledge bases are
    /// mounted into the session).
    McpKnowledgeStdio,
    /// MCP stdio server for the Platform Gateway tools (`nomi_*` — conversations,
    /// cron jobs, global memory, requirements; spawned by agent sessions that
    /// receive a process-issued scoped capability).
    McpGatewayStdio,
    /// MCP stdio server exposing a single reliable `open` tool (URL / file /
    /// folder / application via ShellExecute; spawned by the ACP agent CLI on
    /// Windows so the agent stops launching apps with fragile `cmd /c start`).
    McpOpenStdio,
    /// MCP stdio server exposing the desktop computer-use capability as discrete
    /// tools (snapshot / click / type / launch / …; spawned by the ACP agent CLI
    /// on Windows when the `computer-use` build is present). A thin facade over
    /// the in-tree ComputerTool, so codex/ACP get the same upgraded automation.
    McpComputerStdio,
    /// MCP stdio server exposing the browser-use capability as discrete tools
    /// (navigate / observe / click / type / …; spawned by the ACP agent CLI when
    /// the `browser-use` build is present). A thin facade over the in-tree
    /// BrowserTool, so codex/ACP get the same self-hosted-CDP browser automation.
    McpBrowserStdio,
    /// One-shot terminal lifecycle hook relay (invoked by claude/codex native
    /// hooks; reads the event JSON from stdin and POSTs it to the in-process
    /// TerminalLifecycleServer). NOT an MCP server — fire-and-forget.
    TerminalHook {
        /// Lifecycle kind: turn_end | tool_use | notification | session_start.
        #[arg(long)]
        event: String,
    },
    /// Self-check: hydrate the agent registry, probe every CLI on `$PATH`,
    /// and print a per-agent availability table. Useful when the user
    /// reports "no agent works" — running this from the same shell the
    /// app launched from confirms whether each backend is detectable
    /// before involving server logs.
    Doctor,
    /// List the capabilities exposed on the Remote surface (name + description),
    /// as JSON. Offline — reads the capability registry directly, no running
    /// instance required.
    Tools,
    /// Invoke a capability on a RUNNING NomiFun instance via its REST `/v1` API.
    /// Endpoint/token from `--url`/`--token` or `NOMIFUN_URL` /
    /// `NOMIFUN_COMPANION_TOKEN`.
    Call {
        /// Capability name, e.g. `nomi_cron_list` (see `nomicore tools`).
        name: String,
        /// JSON arguments object (default `{}`).
        args: Option<String>,
        /// Instance base URL (default `$NOMIFUN_URL` or http://127.0.0.1:25808).
        #[arg(long)]
        url: Option<String>,
        /// Per-companion access token (default `$NOMIFUN_COMPANION_TOKEN`).
        #[arg(long)]
        token: Option<String>,
    },
    /// Create a complete offline backup bundle from the current data/work directories.
    ///
    /// The command acquires the same exclusive server lock used by the backend,
    /// so it refuses to race a running instance. It includes the database,
    /// persistent encryption key, companion files, and only backend-managed
    /// `<work-dir>/conversations` workspaces. Custom external workspaces, logs,
    /// and caches are excluded. The output must be outside both source roots.
    /// The bundle contains credentials and must be protected as sensitive data.
    Backup {
        /// Destination directory for the new backup bundle (must not exist).
        #[arg(long)]
        output: PathBuf,
    },
    /// Restore a complete offline backup bundle into a new data directory.
    ///
    /// The destination must be absent or empty; existing data is never
    /// overwritten. Entity IDs, encryption key, companion files, and managed
    /// conversation workspaces are restored below the new data directory while
    /// storage-generation is rotated. Custom external workspaces are not in the
    /// bundle and must be restored separately by their owner.
    Restore {
        /// Source backup bundle directory.
        #[arg(long)]
        bundle: PathBuf,
        /// Destination data directory (must be absent or empty).
        #[arg(long = "destination-data-dir")]
        destination_data_dir: PathBuf,
    },
}

#[cfg(test)]
mod tests {
    use clap::{CommandFactory, Parser};
    use clap::error::ErrorKind;
    use std::path::PathBuf;

    use super::{Cli, Command};

    #[test]
    fn default_data_dir_matches_active_channel() {
        // Pure shape check on the unset default — env handling belongs to clap
        // (`env = "NOMIFUN_DATA_DIR"`) and is not exercised here to keep the
        // test independent of the ambient environment.
        let dir = super::default_data_dir();
        let leaf = super::nomi_leaf(&crate::channel::dir_suffix());
        let user_suffix = PathBuf::from("NomiFun").join(&leaf);
        let fallback_suffix = PathBuf::from("nomifun-data").join(&leaf);
        assert!(
            dir.is_absolute(),
            "default data dir must be absolute, got {dir:?}"
        );
        assert!(
            dir.ends_with(&user_suffix) || dir.ends_with(&fallback_suffix),
            "default data dir should end with {user_suffix:?} (or {fallback_suffix:?}), got {dir:?}"
        );
    }

    #[test]
    fn nomi_leaf_stable_is_plain_nomi() {
        assert_eq!(super::nomi_leaf(""), "Nomi");
    }

    #[test]
    fn nomi_leaf_non_stable_attaches_suffix_to_nomi() {
        // The channel suffix must land on the `Nomi` leaf, yielding a sibling of
        // the production dir (`…/NomiFun/Nomi-dev`) — never on `NomiFun`.
        assert_eq!(super::nomi_leaf("-dev"), "Nomi-dev");
    }

    #[test]
    fn long_version_flag_uses_workspace_package_version() {
        let result = Cli::try_parse_from(["nomicore", "--version"]);
        let err = match result {
            Ok(_) => panic!("expected --version to exit through clap DisplayVersion"),
            Err(err) => err,
        };

        assert_eq!(err.kind(), ErrorKind::DisplayVersion);
        let rendered = err.to_string();
        assert!(
            rendered.contains("nomicore"),
            "version output should contain binary name, got: {rendered:?}"
        );
        assert!(
            rendered.contains(env!("CARGO_PKG_VERSION")),
            "version output should contain package version {}, got: {rendered:?}",
            env!("CARGO_PKG_VERSION")
        );
    }

    #[test]
    fn short_version_flag_uses_workspace_package_version() {
        let result = Cli::try_parse_from(["nomicore", "-V"]);
        let err = match result {
            Ok(_) => panic!("expected -V to exit through clap DisplayVersion"),
            Err(err) => err,
        };

        assert_eq!(err.kind(), ErrorKind::DisplayVersion);
        let rendered = err.to_string();
        assert!(
            rendered.contains("nomicore"),
            "version output should contain binary name, got: {rendered:?}"
        );
        assert!(
            rendered.contains(env!("CARGO_PKG_VERSION")),
            "version output should contain package version {}, got: {rendered:?}",
            env!("CARGO_PKG_VERSION")
        );
    }

    #[test]
    fn backup_subcommand_parses_output_and_data_dir() {
        let cli = Cli::try_parse_from([
            "nomicore",
            "--data-dir",
            "/source-data",
            "backup",
            "--output",
            "/backups/backup-1",
        ])
        .unwrap();
        assert_eq!(cli.data_dir, PathBuf::from("/source-data"));
        assert!(matches!(
            cli.command,
            Some(Command::Backup { output }) if output == PathBuf::from("/backups/backup-1")
        ));
    }

    #[test]
    fn restore_subcommand_parses_bundle_and_destination() {
        let cli = Cli::try_parse_from([
            "nomicore",
            "restore",
            "--bundle",
            "/backups/backup-1",
            "--destination-data-dir",
            "/restored-data",
        ])
        .unwrap();
        assert!(matches!(
            cli.command,
            Some(Command::Restore {
                bundle,
                destination_data_dir,
            }) if bundle == PathBuf::from("/backups/backup-1")
                && destination_data_dir == PathBuf::from("/restored-data")
        ));
    }

    #[test]
    fn backup_and_restore_require_their_paths() {
        let backup = match Cli::try_parse_from(["nomicore", "backup"]) {
            Ok(_) => panic!("backup without --output must fail"),
            Err(error) => error,
        };
        assert_eq!(backup.kind(), ErrorKind::MissingRequiredArgument);

        let restore =
            match Cli::try_parse_from(["nomicore", "restore", "--bundle", "/bundle"]) {
                Ok(_) => panic!("restore without --destination-data-dir must fail"),
                Err(error) => error,
            };
        assert_eq!(restore.kind(), ErrorKind::MissingRequiredArgument);
    }

    #[test]
    fn backup_and_restore_long_help_state_portable_scope() {
        let command = Cli::command();
        let backup = command
            .find_subcommand("backup")
            .unwrap()
            .clone()
            .render_long_help()
            .to_string();
        assert!(backup.contains("Custom external workspaces"));
        assert!(backup.contains("logs"));
        assert!(backup.contains("caches"));
        assert!(backup.contains("sensitive data"));

        let restore = command
            .find_subcommand("restore")
            .unwrap()
            .clone()
            .render_long_help()
            .to_string();
        assert!(restore.contains("Custom external workspaces"));
        assert!(restore.contains("storage-generation"));
    }
}

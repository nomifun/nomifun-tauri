use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::Path;
use std::sync::Arc;

use clap::Parser;

use nomi_agent::bootstrap::AgentBootstrap;
use nomi_agent::output::OutputSink;
use nomi_agent::output::protocol_sink::ProtocolSink;
use nomi_agent::output::terminal::TerminalSink;
use nomi_agent::session;
use nomi_config::auth;
use nomi_config::config::{self, CliArgs, Config, McpServerConfig, TransportType};
use nomi_mcp::manager::McpManager;
use nomi_mcp::tool_proxy::register_single_server_tools;
use nomi_protocol::commands::{ApprovalScope, ProtocolCommand};
use nomi_protocol::events::ProtocolEvent;
use nomi_protocol::reader::spawn_stdin_reader;
use nomi_protocol::writer::{ProtocolEmitter, ProtocolWriter};
use nomi_protocol::{ToolApprovalManager, ToolApprovalResult};

#[derive(Default)]
struct ConnectedMcpServerNames {
    names: BTreeSet<String>,
}

impl ConnectedMcpServerNames {
    fn new(names: impl IntoIterator<Item = String>) -> Self {
        Self {
            names: names.into_iter().collect(),
        }
    }

    fn contains(&self, name: &str) -> bool {
        self.names.contains(name)
    }

    /// Record only a fully connected, atomically registered server.
    fn record_success(&mut self, name: String) -> bool {
        self.names.insert(name)
    }
}

#[derive(Parser)]
#[command(
    name = "nomi",
    about = "Nomi agent CLI — multi-provider AI agent with tool execution and delegation",
    version
)]
struct Cli {
    /// Provider: "anthropic" or "openai"
    #[arg(short, long, env = "PROVIDER")]
    provider: Option<String>,

    /// API key
    #[arg(short = 'k', long, env = "API_KEY")]
    api_key: Option<String>,

    /// Base URL for the API
    #[arg(short, long, env = "BASE_URL")]
    base_url: Option<String>,

    /// Model name
    #[arg(short, long, env = "MODEL")]
    model: Option<String>,

    /// Max output tokens per response
    #[arg(long)]
    max_tokens: Option<u32>,

    /// Max agent loop turns
    #[arg(long)]
    max_turns: Option<usize>,

    /// Custom system prompt
    #[arg(long)]
    system_prompt: Option<String>,

    /// Named profile from config file
    #[arg(long)]
    profile: Option<String>,

    /// Auto-approve all tool executions (skip confirmation)
    #[arg(long)]
    auto_approve: bool,

    /// Project directory to load .nomi.toml from (defaults to CWD)
    #[arg(long)]
    project_dir: Option<std::path::PathBuf>,

    /// Resume a previous session
    #[arg(long)]
    resume: Option<String>,

    /// Use a specific session ID (instead of auto-generating one)
    #[arg(long)]
    session_id: Option<String>,

    /// List saved sessions
    #[arg(long)]
    list_sessions: bool,

    /// Disable colored output
    #[arg(long)]
    no_color: bool,

    /// Enable JSON streaming mode for host client integration
    #[arg(long)]
    json_stream: bool,

    /// Generate a default config file
    #[arg(long)]
    init_config: bool,

    /// Print config file path and exit
    #[arg(long)]
    config_path: bool,

    /// Print skill directory paths and exit
    #[arg(long)]
    skills_path: bool,

    /// Login with Anthropic account (OAuth device flow)
    #[arg(long)]
    login: bool,

    /// Logout (remove saved OAuth credentials)
    #[arg(long)]
    logout: bool,

    /// Output compaction level: off, safe (default), full
    #[arg(long)]
    compaction: Option<String>,

    /// Enable TOON encoding for JSON arrays (session-level, cannot change mid-conversation)
    #[arg(long)]
    toon: bool,

    /// Log directory (enables file logging)
    #[arg(long)]
    log_dir: Option<String>,

    /// Log level filter (e.g. "info", "debug", "info,nomi_providers=debug")
    #[arg(long)]
    log_level: Option<String>,

    /// Initial prompt (if omitted, enters interactive REPL mode)
    #[arg(trailing_var_arg = true)]
    prompt: Vec<String>,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();

    if cli.resume.is_some() && cli.session_id.is_some() {
        anyhow::bail!("Cannot use --resume and --session-id together");
    }

    // Handle --config-path
    if cli.config_path {
        println!("{}", config::global_config_path().display());
        return Ok(());
    }

    // Handle --skills-path
    if cli.skills_path {
        print_skills_paths();
        return Ok(());
    }

    // Handle --init-config
    if cli.init_config {
        return config::init_config();
    }

    // Handle --login / --logout
    if cli.login || cli.logout {
        let oauth = auth::OAuthManager::new(auth::AuthConfig::default());
        if cli.login {
            oauth.login().await?;
            eprintln!("Login successful! You can now use nomi without --api-key.");
        } else {
            oauth.logout()?;
        }
        return Ok(());
    }

    let terminal = Arc::new(TerminalSink::new(cli.no_color));
    let output: Arc<dyn OutputSink> = terminal.clone();

    // Resolve config from files + CLI args + env vars
    let cli_args = CliArgs {
        provider: cli.provider,
        api_key: cli.api_key,
        base_url: cli.base_url,
        model: cli.model,
        max_tokens: cli.max_tokens,
        max_turns: cli.max_turns,
        system_prompt: cli.system_prompt,
        profile: cli.profile,
        auto_approve: cli.auto_approve,
        project_dir: cli.project_dir,
    };

    let mut config = Config::resolve(&cli_args)?;

    if let Some(ref level_str) = cli.compaction {
        match level_str.parse::<nomi_compact::CompactionLevel>() {
            Ok(level) => config.compact.compaction = level,
            Err(e) => anyhow::bail!("Invalid --compaction value: {e}"),
        }
    }
    if cli.toon {
        config.compact.toon = true;
    }

    let _log_guard = {
        use tracing_subscriber::layer::SubscriberExt;
        use tracing_subscriber::util::SubscriberInitExt;

        let resolved = config
            .logging
            .resolve(cli.log_dir.as_deref(), cli.log_level.as_deref());
        if resolved.enabled {
            match nomi_config::logging::create_file_layer(&resolved) {
                Ok((layer, guard)) => {
                    tracing_subscriber::registry().with(layer).init();
                    Some(guard)
                }
                Err(e) => {
                    eprintln!("Warning: failed to initialize logging: {e}");
                    None
                }
            }
        } else {
            None
        }
    };

    let cwd = std::env::current_dir()?.to_string_lossy().to_string();

    // Handle --list-sessions
    if cli.list_sessions {
        let session_mgr = session::SessionManager::new(
            config.session.directory.clone().into(),
            config.session.max_sessions,
        );
        let sessions = session_mgr.list()?;
        if sessions.is_empty() {
            eprintln!("No saved sessions.");
        } else {
            eprintln!(
                "{:<8} {:<12} {:<30} {:>5}  Summary",
                "ID", "Date", "Model", "Msgs"
            );
            for s in &sessions {
                eprintln!(
                    "{:<8} {:<12} {:<30} {:>5}  {}",
                    s.id,
                    s.created_at.format("%Y-%m-%d"),
                    s.model,
                    s.message_count,
                    s.summary
                );
            }
        }
        return Ok(());
    }

    // Branch to JSON stream mode
    if cli.json_stream {
        return run_json_stream_mode(config, &cwd, cli.resume, cli.session_id).await;
    }

    let provider_name = config.provider_label.clone();

    // Bootstrap engine with full feature initialization
    let mut bootstrap = AgentBootstrap::new(config, &cwd, output.clone());

    if let Some(resume_id) = &cli.resume {
        let cfg = bootstrap.config();
        let session_mgr = session::SessionManager::new(
            cfg.session.directory.clone().into(),
            cfg.session.max_sessions,
        );
        let session = session_mgr.load(resume_id)?;
        terminal.formatter().session_info(&format!(
            "Resumed session {} ({} messages, {} model)",
            session.id,
            session.messages.len(),
            session.model
        ));
        bootstrap = bootstrap.resume(session);
    }

    let result = bootstrap.build().await?;
    let mut engine = result.engine;

    if cli.resume.is_none() {
        engine.init_session(&provider_name, &cwd, cli.session_id.as_deref())?;
    }

    let prompt = cli.prompt.join(" ");
    if prompt.is_empty() {
        repl_loop(&mut engine, &terminal, &output).await?;
    } else {
        let turn_result = engine.execute_turn(&prompt, "").await?;
        output.emit_stream_end(
            "",
            turn_result.turns,
            turn_result.usage.input_tokens,
            turn_result.usage.output_tokens,
            turn_result.usage.cache_creation_tokens,
            turn_result.usage.cache_read_tokens,
        );
    }

    engine.run_stop_hooks().await;
    if let Some(report) = engine.shutdown_processes().await
        && report.sessions.iter().any(|session| {
            matches!(
                &session.outcome,
                nomi_process_runtime::ProcessOutcome::Lost { cleanup, .. } if !cleanup.reaped
            )
        })
    {
        tracing::error!(
            target: "nomi_cli",
            "engine shutdown could not prove every command process tree was reaped"
        );
    }

    for mgr in &result.mcp_managers {
        mgr.shutdown().await;
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::ConnectedMcpServerNames;

    #[test]
    fn mcp_server_name_is_claimed_only_after_success_and_includes_static_connections() {
        let mut names = ConnectedMcpServerNames::new(["static-server".to_owned()]);
        assert!(names.contains("static-server"));
        assert!(!names.contains("dynamic-server"));

        // A failed connection/registration records nothing, so retry remains possible.
        assert!(!names.contains("dynamic-server"));

        assert!(names.record_success("dynamic-server".to_owned()));
        assert!(names.contains("dynamic-server"));
        assert!(!names.record_success("dynamic-server".to_owned()));
    }
}

async fn repl_loop(
    engine: &mut nomi_agent::engine::AgentEngine,
    terminal: &Arc<TerminalSink>,
    output: &Arc<dyn OutputSink>,
) -> anyhow::Result<()> {
    use std::io::{self, BufRead};

    loop {
        terminal.formatter().repl_prompt();

        let mut input = String::new();
        io::stdin().lock().read_line(&mut input)?;
        let input = input.trim();

        if input.is_empty() {
            break;
        }

        match engine.execute_turn(input, "").await {
            Ok(result) => {
                if result.turns > 0 {
                    output.emit_stream_end(
                        "",
                        result.turns,
                        result.usage.input_tokens,
                        result.usage.output_tokens,
                        result.usage.cache_creation_tokens,
                        result.usage.cache_read_tokens,
                    );
                }
            }
            Err(nomi_agent::engine::AgentError::UserAborted) => break,
            Err(e) => {
                output.emit_error(&e.to_string());
            }
        }
    }

    Ok(())
}

fn print_skills_paths() {
    use nomi_skills::paths::{project_commands_dirs, project_skills_dirs, user_skills_dir};

    fn status(p: &Path) -> &'static str {
        if p.is_dir() { "exists" } else { "not found" }
    }

    // User-level
    match user_skills_dir() {
        Some(dir) => println!("User:    {}  ({})", dir.display(), status(&dir)),
        None => println!("User:    <unable to determine config directory>"),
    }

    // Project-level
    let cwd = std::env::current_dir().unwrap_or_default();
    let project_dirs = project_skills_dirs(&cwd);
    if project_dirs.is_empty() {
        println!("Project: <none found>");
    } else {
        for dir in &project_dirs {
            println!("Project: {}  ({})", dir.display(), status(dir));
        }
    }

    // Project-owned legacy commands. v3 intentionally has no second
    // user-global commands root outside the managed application data set.
    let mut has_legacy = false;
    for dir in project_commands_dirs(&cwd) {
        println!("Legacy:  {}  ({})", dir.display(), status(&dir));
        has_legacy = true;
    }
    if !has_legacy {
        println!("Legacy:  <none found>");
    }
}

fn to_mcp_server_config(
    transport: &str,
    command: Option<String>,
    args: Option<Vec<String>>,
    env: Option<HashMap<String, String>>,
    url: Option<String>,
    headers: Option<HashMap<String, String>>,
) -> Result<McpServerConfig, String> {
    let transport_type = match transport {
        "stdio" => TransportType::Stdio,
        "sse" => TransportType::Sse,
        "streamable-http" | "streamable_http" => TransportType::StreamableHttp,
        other => return Err(format!("unknown transport: {other}")),
    };
    Ok(McpServerConfig {
        transport: transport_type,
        command,
        args,
        env,
        url,
        headers,
        deferred: Some(false),
    })
}

/// Pending config fields: (model, thinking, thinking_budget, effort)
type PendingConfig = (
    Option<String>,
    Option<String>,
    Option<u32>,
    Option<String>,
    Option<String>,
);

async fn run_json_stream_mode(
    config: Config,
    cwd: &str,
    resume: Option<String>,
    session_id: Option<String>,
) -> anyhow::Result<()> {
    let writer = Arc::new(ProtocolWriter::new());
    let protocol_sink = Arc::new(ProtocolSink::new(writer.clone()));
    let approval_manager = Arc::new(ToolApprovalManager::new());
    let output: Arc<dyn OutputSink> = protocol_sink.clone();

    let provider_name = config.provider_label.clone();

    // Bootstrap engine with full feature initialization
    // P3-X1: pass the same approval manager into bootstrap so the native BrowserTool's redline
    // gate reads the LIVE runtime session mode (a SetMode command flips it immediately) instead
    // of the construction-time auto_approve snapshot.
    let mut bootstrap =
        AgentBootstrap::new(config, cwd, output.clone()).approval_manager(approval_manager.clone());

    if let Some(resume_id) = &resume {
        let cfg = bootstrap.config();
        let session_mgr = session::SessionManager::new(
            cfg.session.directory.clone().into(),
            cfg.session.max_sessions,
        );
        let session = session_mgr.load(resume_id)?;
        bootstrap = bootstrap.resume(session);
    }

    let result = bootstrap.build().await?;
    let mut connected_mcp_server_names = ConnectedMcpServerNames::new(
        result
            .mcp_managers
            .iter()
            .flat_map(|manager| manager.server_names()),
    );
    let mut engine = result.engine;
    let initial_has_mcp = result.has_mcp;

    if resume.is_none() {
        engine.init_session(&provider_name, cwd, session_id.as_deref())?;
    }

    let sid = engine.current_session_id();
    protocol_sink.emit_ready(
        engine.compat(),
        initial_has_mcp,
        sid,
        &approval_manager.current_mode(),
    );

    engine.set_approval_manager(approval_manager.clone());
    engine.set_protocol_writer(writer.clone());

    let mut cmd_rx = spawn_stdin_reader();

    // --- Pre-message phase: accept AddMcpServer commands ---
    let mut dynamic_managers: Vec<Arc<McpManager>> = Vec::new();
    let mut first_cmd: Option<ProtocolCommand> = None;

    while let Some(cmd) = cmd_rx.recv().await {
        match cmd {
            ProtocolCommand::AddMcpServer {
                name,
                transport,
                command,
                args,
                env,
                url,
                headers,
            } => {
                if connected_mcp_server_names.contains(&name) {
                    output.emit_error(&format!(
                        "AddMcpServer '{name}': rejected — a connected MCP server already uses this name"
                    ));
                    continue;
                }
                tracing::info!(target: "nomi_mcp", %name, %transport, ?command, "AddMcpServer received");
                let config =
                    match to_mcp_server_config(&transport, command, args, env, url, headers) {
                        Ok(c) => c,
                        Err(e) => {
                            output.emit_error(&format!("AddMcpServer '{name}': {e}"));
                            continue;
                        }
                    };

                let mut single_configs = HashMap::new();
                single_configs.insert(name.clone(), config.clone());
                tracing::info!(target: "nomi_mcp", %name, "connecting to mcp server");
                match McpManager::connect_all(&single_configs).await {
                    Ok(mgr) => {
                        if !mgr.server_names().iter().any(|server| server == &name) {
                            output.emit_error(&format!(
                                "AddMcpServer '{name}' failed: the server did not connect"
                            ));
                            continue;
                        }
                        let advertised_tool_names: Vec<String> = mgr
                            .all_tools()
                            .iter()
                            .map(|(_, t)| t.name.clone())
                            .collect();
                        tracing::info!(target: "nomi_mcp", %name, tools = advertised_tool_names.len(), "mcp server connected");
                        let mgr_arc = Arc::new(mgr);
                        let registrations = match register_single_server_tools(
                            engine.registry_mut(),
                            &mgr_arc,
                            &name,
                            config.deferred.unwrap_or(true),
                        ) {
                            Ok(registrations) => registrations,
                            Err(error) => {
                                mgr_arc.shutdown().await;
                                output.emit_error(&format!(
                                    "AddMcpServer '{name}' rejected: {error}"
                                ));
                                continue;
                            }
                        };
                        let newly_recorded = connected_mcp_server_names.record_success(name.clone());
                        debug_assert!(newly_recorded);
                        let tool_names = registrations
                            .iter()
                            .map(|registration| registration.original_name.clone())
                            .collect();
                        let provider_tools: BTreeMap<String, String> = registrations
                            .into_iter()
                            .map(|registration| {
                                (registration.original_name, registration.provider_name)
                            })
                            .collect();
                        dynamic_managers.push(mgr_arc);
                        let _ = writer.emit(&ProtocolEvent::McpReady {
                            name,
                            tools: tool_names,
                            provider_tools,
                        });
                    }
                    Err(e) => {
                        tracing::warn!(target: "nomi_mcp", %name, error = %e, "mcp server connection failed");
                        output.emit_error(&format!("AddMcpServer '{name}' failed: {e}"));
                    }
                }
            }
            ProtocolCommand::Stop => return Ok(()),
            other => {
                first_cmd = Some(other);
                break;
            }
        }
    }

    let has_mcp = initial_has_mcp || !dynamic_managers.is_empty();
    let mut pending_cmd = first_cmd;

    loop {
        let cmd = if let Some(c) = pending_cmd.take() {
            c
        } else {
            match cmd_rx.recv().await {
                Some(c) => c,
                None => break,
            }
        };

        match cmd {
            ProtocolCommand::Message {
                msg_id,
                content,
                files: _,
            } => {
                let mut stopped = false;
                let mut pending_config: Option<PendingConfig> = None;
                let mut mode_changed = false;

                {
                    let turn_execution = engine.execute_turn(&content, &msg_id);
                    tokio::pin!(turn_execution);

                    loop {
                        tokio::select! {
                            result = &mut turn_execution => {
                                match result {
                                    Ok(result) => {
                                        output.emit_stream_end(
                                            &msg_id,
                                            result.turns,
                                            result.usage.input_tokens,
                                            result.usage.output_tokens,
                                            result.usage.cache_creation_tokens,
                                            result.usage.cache_read_tokens,
                                        );
                                    }
                                    Err(e) => {
                                        output.emit_error(&e.to_string());
                                        output.emit_stream_end(&msg_id, 0, 0, 0, 0, 0);
                                    }
                                }
                                break;
                            }
                            Some(sub_cmd) = cmd_rx.recv() => {
                                match sub_cmd {
                                    ProtocolCommand::ToolApprove { call_id, scope: _ } => {
                                        approval_manager.resolve(&call_id, ToolApprovalResult::Approved);
                                    }
                                    ProtocolCommand::ToolDeny { call_id, reason } => {
                                        approval_manager.resolve(&call_id, ToolApprovalResult::Denied { reason });
                                    }
                                    ProtocolCommand::Stop => {
                                        stopped = true;
                                        break;
                                    }
                                    ProtocolCommand::SetConfig { model, thinking, thinking_budget, effort, compaction } => {
                                        pending_config = Some((model, thinking, thinking_budget, effort, compaction));
                                        let _ = writer.emit(&nomi_protocol::events::ProtocolEvent::Info {
                                            msg_id: String::new(),
                                            message: "set_config: queued, will apply after current response".to_string(),
                                        });
                                    }
                                    ProtocolCommand::SetMode { mode } => {
                                        approval_manager.set_mode(mode);
                                        mode_changed = true;
                                        let _ = writer.emit(&nomi_protocol::events::ProtocolEvent::Info {
                                            msg_id: String::new(),
                                            message: format!("mode updated: {}", approval_manager.current_mode()),
                                        });
                                    }
                                    ProtocolCommand::Ping => {
                                        let _ = writer.emit(&nomi_protocol::events::ProtocolEvent::Pong);
                                    }
                                    _ => {
                                        tracing::debug!(target: "nomi_protocol", "ignoring command during active message processing");
                                    }
                                }
                            }
                        }
                    }
                }

                if let Some((model, thinking, thinking_budget, effort, compaction)) =
                    pending_config.take()
                {
                    let changes = engine.apply_config_update(
                        model,
                        thinking,
                        thinking_budget,
                        effort,
                        compaction,
                    );
                    if !changes.is_empty() {
                        let _ = writer.emit(&nomi_protocol::events::ProtocolEvent::Info {
                            msg_id: String::new(),
                            message: format!("config applied: {}", changes.join(", ")),
                        });
                    }
                    protocol_sink.emit_config_changed(
                        engine.compat(),
                        has_mcp,
                        &approval_manager.current_mode(),
                    );
                } else if mode_changed {
                    protocol_sink.emit_config_changed(
                        engine.compat(),
                        has_mcp,
                        &approval_manager.current_mode(),
                    );
                }
                if stopped {
                    break;
                }
            }
            ProtocolCommand::Stop => {
                break;
            }
            ProtocolCommand::ToolApprove { call_id, scope } => {
                if matches!(scope, ApprovalScope::Always) {
                    // Auto-approve all future calls of this tool's category
                }
                approval_manager.resolve(&call_id, ToolApprovalResult::Approved);
            }
            ProtocolCommand::ToolDeny { call_id, reason } => {
                approval_manager.resolve(&call_id, ToolApprovalResult::Denied { reason });
            }
            ProtocolCommand::InitHistory { text } => {
                tracing::debug!(target: "nomi_protocol", chars = text.len(), "InitHistory received");
            }
            ProtocolCommand::SetMode { mode } => {
                let mode_str = format!("{mode:?}").to_lowercase();
                approval_manager.set_mode(mode);
                let _ = writer.emit(&nomi_protocol::events::ProtocolEvent::Info {
                    msg_id: String::new(),
                    message: format!("mode updated: {}", approval_manager.current_mode()),
                });
                protocol_sink.emit_config_changed(
                    engine.compat(),
                    has_mcp,
                    &approval_manager.current_mode(),
                );
                tracing::debug!(target: "nomi_protocol", mode = %mode_str, "SetMode applied");
            }
            ProtocolCommand::SetConfig {
                model,
                thinking,
                thinking_budget,
                effort,
                compaction,
            } => {
                let changes = engine.apply_config_update(
                    model,
                    thinking,
                    thinking_budget,
                    effort,
                    compaction,
                );
                let message = if changes.is_empty() {
                    "set_config: no changes".to_string()
                } else {
                    format!("config updated: {}", changes.join(", "))
                };
                let _ = writer.emit(&nomi_protocol::events::ProtocolEvent::Info {
                    msg_id: String::new(),
                    message,
                });
                protocol_sink.emit_config_changed(
                    engine.compat(),
                    has_mcp,
                    &approval_manager.current_mode(),
                );
            }
            ProtocolCommand::AddMcpServer { name, .. } => {
                output.emit_error(&format!(
                    "AddMcpServer '{name}': rejected — only allowed before first Message"
                ));
            }
            ProtocolCommand::Ping => {
                let _ = writer.emit(&nomi_protocol::events::ProtocolEvent::Pong);
            }
        }
    }

    engine.run_stop_hooks().await;
    if let Some(report) = engine.shutdown_processes().await
        && report.sessions.iter().any(|session| {
            matches!(
                &session.outcome,
                nomi_process_runtime::ProcessOutcome::Lost { cleanup, .. } if !cleanup.reaped
            )
        })
    {
        tracing::error!(
            target: "nomi_cli",
            "engine shutdown could not prove every command process tree was reaped"
        );
    }
    for mgr in &result.mcp_managers {
        mgr.shutdown().await;
    }
    for mgr in &dynamic_managers {
        mgr.shutdown().await;
    }

    Ok(())
}

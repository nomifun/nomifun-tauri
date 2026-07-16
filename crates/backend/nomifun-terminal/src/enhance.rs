//! Terminal launch enhancement: the single PTY-spawn seam that renders
//! platform capabilities (today: MCP servers) into each agent CLI's NATIVE
//! launch config. Per-CLI capability rendering is isolated into `AgentCli`;
//! unknown CLIs get nothing (honest — no pretense, no pollution).

use std::collections::{BTreeMap, HashMap};
use std::path::Path;

use nomifun_common::TerminalId;

/// One MCP server to inject into a terminal-launched CLI. Backend-agnostic; a
/// per-CLI renderer turns this into that CLI's native MCP config.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpServerSpec {
    /// Wire-level server name (e.g. "nomifun-knowledge"). Must be a bare-key-safe
    /// identifier (ASCII alphanumeric, `-`, `_`) for codex dotted-key rendering.
    pub name: String,
    /// Program that launches the stdio bridge (the backend's own executable).
    pub command: String,
    /// Bridge subcommand args (e.g. ["mcp-knowledge-stdio"]).
    pub args: Vec<String>,
    /// Environment inherited by the bridge process (port/scoped token/claims).
    /// Renderers keep these values out of config files and command-line args.
    pub env: HashMap<String, String>,
}

/// Lifecycle hook wiring baked into a terminal spawn so the CLI's native hooks
/// can call back to the in-process `TerminalLifecycleServer`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LifecycleHookWiring {
    pub port: u16,
    pub token: String,
    pub terminal_id: TerminalId,
    /// Absolute path to the backend binary (`nomicore`); used as the hook command
    /// prefix (`<bin> terminal-hook --event <kind>`).
    pub binary_path: String,
}

/// Everything the platform injects into one terminal launch: MCP servers +
/// lifecycle hook wiring.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TerminalLaunchEnhancement {
    pub mcp_servers: Vec<McpServerSpec>,
    pub lifecycle: Option<LifecycleHookWiring>,
}

impl TerminalLaunchEnhancement {
    pub fn is_empty(&self) -> bool {
        self.mcp_servers.is_empty() && self.lifecycle.is_none()
    }
}

/// Which agent CLI a launch program is, for capability injection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentCli {
    Claude,
    Codex,
    Gemini,
}

/// Map a stem (file name without extension, lowercased) to a known agent family.
fn family_from_stem(s: &str) -> Option<AgentCli> {
    let stem = Path::new(s)
        .file_stem()
        .and_then(|x| x.to_str())?
        .to_ascii_lowercase();
    match stem.as_str() {
        "claude" => Some(AgentCli::Claude),
        "codex" => Some(AgentCli::Codex),
        "gemini" => Some(AgentCli::Gemini),
        _ => None,
    }
}

/// Map a launch to its agent family for platform-MCP injection/registration.
/// Resolution order: explicitly DECLARED backend (preset/user) → the program's
/// own stem → a known family token among the args (wrapper/launcher like
/// `stepcode claude`, `npx codex`) → None (honest: unknown CLI).
pub fn resolve_agent_family(
    program: &str,
    args: &[String],
    declared_backend: Option<&str>,
) -> Option<AgentCli> {
    // 1. Declared backend wins (user/preset explicitly said "this is codex").
    if let Some(b) = declared_backend.and_then(family_from_stem) {
        return Some(b);
    }
    // 2. Program stem.
    if let Some(p) = family_from_stem(program) {
        return Some(p);
    }
    // 3. Wrapper: scan args for the FIRST token whose stem is a known family.
    args.iter().find_map(|a| family_from_stem(a))
}

/// Resolve a launch program (absolute path or bare name) to a known agent CLI
/// by its lowercased file stem. Thin wrapper over `resolve_agent_family` for
/// call sites that only have the program (no args / no declared backend).
pub fn detect_agent_cli(program: &str) -> Option<AgentCli> {
    resolve_agent_family(program, &[], None)
}

impl AgentCli {
    /// Whether this CLI family has a lifecycle-hook renderer (Stop → TurnEnd)
    /// in `apply_enhancement`. Terminal AutoWork requires this: it is the ONLY
    /// structured turn-end signal (see `nomifun-requirement`'s AutoWork runner —
    /// no quiescence fallback). Claude/Codex have launch-flag renderers; Gemini
    /// has no launch-time injection mechanism, so it is NOT autowork-capable.
    pub fn supports_lifecycle_hooks(self) -> bool {
        matches!(self, AgentCli::Claude | AgentCli::Codex)
    }
}

/// Whether a terminal launch is eligible for AutoWork: it resolves to an agent
/// CLI family that has a lifecycle-hook renderer, so the platform can detect
/// turn boundaries. This MIRRORS exactly what `apply_enhancement` will inject
/// for the same `(command, args, declared_backend)` — the single source of
/// truth for "AutoWork can drive this terminal". Covers wrappers (`stepcode
/// claude`, `npx codex`), bare/custom commands, and explicitly declared
/// backends, so the eligibility gate never rejects a launch the injector would
/// have hooked.
pub fn terminal_autowork_capable(command: &str, args: &[String], declared_backend: Option<&str>) -> bool {
    let (program, prog_args) = crate::types::resolve_command(command, args);
    resolve_agent_family(&program, &prog_args, declared_backend).is_some_and(AgentCli::supports_lifecycle_hooks)
}

/// Render the enhancement as a claude `--mcp-config` JSON file in `session_dir`
/// and return the EXTRA argv to append. Additive (no `--strict-mcp-config`) so
/// the user's own project/user `.mcp.json` servers are preserved; ours is added
/// alongside. Collision risk is negligible (platform servers use reserved
/// `nomifun-*` names). The file lives in the platform's session-private dir,
/// NEVER the user's cwd (no git pollution). claude auth (keychain/~/.claude) is
/// untouched.
fn claude_mcp_argv(enh: &TerminalLaunchEnhancement, session_dir: &Path) -> std::io::Result<Vec<String>> {
    let servers: serde_json::Map<String, serde_json::Value> = enh
        .mcp_servers
        .iter()
        .map(|s| {
            (
                s.name.clone(),
                serde_json::json!({ "command": s.command, "args": s.args }),
            )
        })
        .collect();
    let doc = serde_json::json!({ "mcpServers": servers });
    std::fs::create_dir_all(session_dir)?;
    let path = session_dir.join("mcp.json");
    let bytes = serde_json::to_vec_pretty(&doc)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    std::fs::write(&path, bytes)?;
    Ok(vec![
        "--mcp-config".to_owned(),
        path.to_string_lossy().into_owned(),
    ])
}

/// TOML bare-key-safe server name (so `mcp_servers.<name>.x` is a valid dotted key).
fn is_bare_key_safe(name: &str) -> bool {
    !name.is_empty() && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

/// Render the enhancement as codex `-c mcp_servers.*` config overrides appended
/// to argv. Uses `-c` (NOT `CODEX_HOME`) so the user's `~/.codex/auth.json` and
/// base config stay the source of truth — relocating CODEX_HOME would strand
/// the login. Each value is TOML (strings quoted, arrays as TOML arrays); codex
/// parses dotted-path `-c` values as TOML (`codex --help`).
fn codex_mcp_argv(enh: &TerminalLaunchEnhancement) -> Vec<String> {
    let mut argv = Vec::new();
    for s in &enh.mcp_servers {
        if !is_bare_key_safe(&s.name) {
            tracing::warn!(name = %s.name, "skipping codex MCP server with non-bare-key-safe name");
            continue;
        }
        let base = format!("mcp_servers.{}", s.name);
        argv.push("-c".to_owned());
        argv.push(format!("{base}.command={}", toml_str(&s.command)));
        argv.push("-c".to_owned());
        argv.push(format!("{base}.args={}", toml_str_array(&s.args)));
    }
    argv
}

/// Merge scoped bridge environments deterministically. These values are placed
/// in the PTY process environment so MCP subprocesses inherit them naturally;
/// secrets therefore never appear in Claude config files or Codex argv.
fn mcp_process_env(enh: &TerminalLaunchEnhancement) -> Vec<(String, String)> {
    let mut merged = BTreeMap::new();
    for server in &enh.mcp_servers {
        for (key, value) in &server.env {
            if let Some(previous) = merged.insert(key.clone(), value.clone())
                && previous != *value
            {
                tracing::warn!(key, server = %server.name, "conflicting MCP process environment; using the last scoped value");
            }
        }
    }
    merged.into_iter().collect()
}

/// TOML basic-string literal: wrap in quotes, escape `\`, `"`, and control chars.
fn toml_str(s: &str) -> String {
    use std::fmt::Write as _;
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            '\t' => out.push_str("\\t"),
            '\r' => out.push_str("\\r"),
            c if c.is_control() => { let _ = write!(out, "\\u{:04X}", c as u32); }
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// TOML inline array of basic strings.
fn toml_str_array(items: &[String]) -> String {
    let inner: Vec<String> = items.iter().map(|i| toml_str(i)).collect();
    format!("[{}]", inner.join(","))
}

/// Double-quote a path for use inside a CLI hook's shell `command` string
/// (handles spaces; escapes backslash and quote). Used by both claude & codex
/// hook command rendering.
fn shell_quote_arg(s: &str) -> String {
    format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
}

/// Render lifecycle hook commands for claude: writes a `settings.json` in
/// `session_dir` containing hook definitions for Stop/PostToolUse/Notification,
/// and returns the `--settings <path>` argv + env additions.
fn claude_lifecycle_argv(
    lc: &LifecycleHookWiring,
    session_dir: &Path,
) -> std::io::Result<(Vec<String>, Vec<(String, String)>)> {
    // Shell command strings — quote the binary path (may contain spaces).
    let quoted_bin = shell_quote_arg(&lc.binary_path);
    let cmd_turn_end = format!("{} terminal-hook --event turn_end", quoted_bin);
    let cmd_tool_use = format!("{} terminal-hook --event tool_use", quoted_bin);
    let cmd_notification = format!("{} terminal-hook --event notification", quoted_bin);

    let doc = serde_json::json!({
        "hooks": {
            "Stop": [{"hooks": [{"type": "command", "command": cmd_turn_end}]}],
            "PostToolUse": [{"hooks": [{"type": "command", "command": cmd_tool_use}]}],
            "Notification": [{"hooks": [{"type": "command", "command": cmd_notification}]}],
        }
    });
    std::fs::create_dir_all(session_dir)?;
    let path = session_dir.join("settings.json");
    let bytes = serde_json::to_vec_pretty(&doc)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    std::fs::write(&path, bytes)?;

    let argv = vec!["--settings".to_owned(), path.to_string_lossy().into_owned()];
    let env = lifecycle_env(lc);
    Ok((argv, env))
}

/// Render lifecycle hook commands for codex: appends `-c hooks.*` TOML overrides
/// plus the lifecycle env. Coexists with Plan 1 MCP `-c` overrides (codex
/// handles multiple `-c` flags additively).
///
/// Do not inject the retired `--dangerously-bypass-hook-trust` switch. Current
/// Codex releases reject it during argument parsing (exit code 2) before any
/// hook or interactive UI can start.
fn codex_lifecycle_argv(lc: &LifecycleHookWiring) -> (Vec<String>, Vec<(String, String)>) {
    let quoted_bin = shell_quote_arg(&lc.binary_path);
    let cmd_turn_end = format!("{} terminal-hook --event turn_end", quoted_bin);
    let cmd_tool_use = format!("{} terminal-hook --event tool_use", quoted_bin);
    let cmd_session_start = format!("{} terminal-hook --event session_start", quoted_bin);

    let mut argv = Vec::new();
    // Stop
    argv.push("-c".to_owned());
    argv.push(format!(
        "hooks.Stop=[{{hooks=[{{type=\"command\",command={}}}]}}]",
        toml_str(&cmd_turn_end)
    ));
    // PostToolUse
    argv.push("-c".to_owned());
    argv.push(format!(
        "hooks.PostToolUse=[{{hooks=[{{type=\"command\",command={}}}]}}]",
        toml_str(&cmd_tool_use)
    ));
    // SessionStart
    argv.push("-c".to_owned());
    argv.push(format!(
        "hooks.SessionStart=[{{hooks=[{{type=\"command\",command={}}}]}}]",
        toml_str(&cmd_session_start)
    ));

    let env = lifecycle_env(lc);
    (argv, env)
}

/// Env vars baked into the PTY so the `terminal-hook` shim can reach the
/// in-process TerminalLifecycleServer.
fn lifecycle_env(lc: &LifecycleHookWiring) -> Vec<(String, String)> {
    vec![
        ("NOMI_TERM_HOOK_PORT".to_owned(), lc.port.to_string()),
        ("NOMI_TERM_HOOK_TOKEN".to_owned(), lc.token.clone()),
        ("NOMI_TERM_HOOK_ID".to_owned(), lc.terminal_id.to_string()),
    ]
}

/// Apply platform enhancement to a resolved launch argv. Dispatches on the
/// resolved agent family (declared backend > stem > wrapper arg token); unknown
/// programs are returned UNCHANGED (honest no-op, no pretense). A failed claude
/// config write degrades to "launch without the tool" (warn), never blocks the
/// PTY. `session_dir` is a platform-private dir (NEVER the user's cwd).
///
/// Returns `(args, env_additions)` — the caller merges `env_additions` into
/// the PTY spawn env.
pub fn apply_enhancement(
    program: &str,
    mut args: Vec<String>,
    enh: &TerminalLaunchEnhancement,
    session_dir: &Path,
    declared_backend: Option<&str>,
) -> (Vec<String>, Vec<(String, String)>) {
    if enh.is_empty() {
        return (args, Vec::new());
    }

    let mut env_additions: Vec<(String, String)> = Vec::new();

    // Resolve family BEFORE any args.extend (borrows &args).
    let family = resolve_agent_family(program, &args, declared_backend);

    match family {
        Some(AgentCli::Claude) => {
            // MCP injection
            if !enh.mcp_servers.is_empty() {
                match claude_mcp_argv(enh, session_dir) {
                    Ok(extra) => {
                        args.extend(extra);
                        env_additions.extend(mcp_process_env(enh));
                    }
                    Err(e) => tracing::warn!(error = %e, "claude MCP config write failed; launching without knowledge tool"),
                }
            }
            // Lifecycle hooks
            if let Some(lc) = &enh.lifecycle {
                match claude_lifecycle_argv(lc, session_dir) {
                    Ok((extra_args, extra_env)) => {
                        args.extend(extra_args);
                        env_additions.extend(extra_env);
                    }
                    Err(e) => tracing::warn!(error = %e, "claude lifecycle settings write failed; launching without hooks"),
                }
            }
        }
        Some(AgentCli::Codex) => {
            // MCP injection
            if !enh.mcp_servers.is_empty() {
                args.extend(codex_mcp_argv(enh));
                env_additions.extend(mcp_process_env(enh));
            }
            // Lifecycle hooks
            if let Some(lc) = &enh.lifecycle {
                let (extra_args, extra_env) = codex_lifecycle_argv(lc);
                args.extend(extra_args);
                env_additions.extend(extra_env);
            }
        }
        Some(AgentCli::Gemini) => {
            // Gemini has no secure launch-flag injection mechanism. A standalone
            // config cannot obtain session-bound claims, so this is an honest no-op.
        }
        None => {} // unknown CLI: no injection (honest)
    }
    (args, env_additions)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn sample_kb_server() -> McpServerSpec {
        McpServerSpec {
            name: "nomifun-knowledge".into(),
            command: "/opt/nomi/nomicore".into(),
            args: vec!["mcp-knowledge-stdio".into()],
            env: HashMap::from([(
                "NOMI_KB_MCP_CAPABILITY".into(),
                "scoped-bootstrap".into(),
            )]),
        }
    }

    #[test]
    fn enhancement_empty_when_no_servers_and_no_lifecycle() {
        assert!(TerminalLaunchEnhancement::default().is_empty());
        let e = TerminalLaunchEnhancement { mcp_servers: vec![sample_kb_server()], lifecycle: None };
        assert!(!e.is_empty());
        let e2 = TerminalLaunchEnhancement {
            mcp_servers: vec![],
            lifecycle: Some(LifecycleHookWiring { port: 1, token: "t".into(), terminal_id: TerminalId::new(), binary_path: "/bin".into() }),
        };
        assert!(!e2.is_empty());
    }

    #[test]
    fn detect_agent_cli_by_stem_case_and_path_insensitive() {
        assert_eq!(detect_agent_cli("claude"), Some(AgentCli::Claude));
        assert_eq!(detect_agent_cli("/usr/local/bin/claude"), Some(AgentCli::Claude));
        assert_eq!(detect_agent_cli("codex"), Some(AgentCli::Codex));
        assert_eq!(detect_agent_cli("/Users/u/.bun/bin/Codex"), Some(AgentCli::Codex));
        assert_eq!(detect_agent_cli("gemini"), Some(AgentCli::Gemini));
        // Unknown / shells / near-misses → None (honest: no injection).
        assert_eq!(detect_agent_cli("/bin/bash"), None);
        assert_eq!(detect_agent_cli("claude-helper"), None);
        assert_eq!(detect_agent_cli(""), None);
    }

    #[test]
    fn claude_renderer_writes_mcp_json_outside_cwd_and_returns_additive_argv() {
        let dir = tempfile::TempDir::new().unwrap();
        let enh = TerminalLaunchEnhancement { mcp_servers: vec![sample_kb_server()], lifecycle: None };
        let argv = claude_mcp_argv(&enh, dir.path()).expect("write ok");

        // argv 形如 ["--mcp-config", "<dir>/mcp.json"] — additive, no --strict-mcp-config
        assert_eq!(argv.len(), 2);
        assert_eq!(argv[0], "--mcp-config");
        assert!(argv[1].ends_with("mcp.json"));
        assert!(std::path::Path::new(&argv[1]).starts_with(dir.path())); // 不在用户 cwd

        let doc: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&argv[1]).unwrap()).unwrap();
        let srv = &doc["mcpServers"]["nomifun-knowledge"];
        assert_eq!(srv["command"], "/opt/nomi/nomicore");
        assert_eq!(srv["args"][0], "mcp-knowledge-stdio");
        assert!(
            srv.get("env").is_none(),
            "capability values must not be written to disk: {srv}"
        );
    }

    #[test]
    fn codex_renderer_emits_c_overrides_preserving_user_config() {
        let enh = TerminalLaunchEnhancement { mcp_servers: vec![sample_kb_server()], lifecycle: None };
        let argv = codex_mcp_argv(&enh);
        // 形如 -c mcp_servers.nomifun-knowledge.command="..." -c ...args=[...]
        let joined = argv.join(" ");
        assert!(joined.contains(r#"-c mcp_servers.nomifun-knowledge.command="/opt/nomi/nomicore""#));
        assert!(joined.contains(r#"mcp_servers.nomifun-knowledge.args=["mcp-knowledge-stdio"]"#));
        assert!(!joined.contains("scoped-bootstrap"));
        assert!(!joined.contains("NOMI_KB_MCP_CAPABILITY"));
        // 每个 override 前都有独立的 -c (command + args = 2)
        assert_eq!(argv.iter().filter(|a| *a == "-c").count(), 2);
        assert!(!joined.contains("CODEX_HOME"));
        // Loose KB_IDS must never appear; scope lives in signed claims.
        assert!(!joined.contains("KB_MCP_KB_IDS"), "kb_ids must not be baked");
    }

    #[test]
    fn toml_str_escapes_quotes_and_backslashes() {
        assert_eq!(toml_str(r#"a"b\c"#), r#""a\"b\\c""#);
    }

    #[test]
    fn apply_enhancement_dispatches_by_cli_and_noops_unknown() {
        let dir = tempfile::TempDir::new().unwrap();
        let enh = TerminalLaunchEnhancement { mcp_servers: vec![sample_kb_server()], lifecycle: None };

        // claude → 追加 --mcp-config (additive, 不含 --strict-mcp-config)
        let (out, env) = apply_enhancement("claude", vec!["--dangerously-skip-permissions".into()], &enh, dir.path(), None);
        assert_eq!(out[0], "--dangerously-skip-permissions");
        assert!(out.iter().any(|a| a == "--mcp-config"));
        let env: HashMap<_, _> = env.into_iter().collect();
        assert_eq!(
            env.get("NOMI_KB_MCP_CAPABILITY").map(String::as_str),
            Some("scoped-bootstrap")
        );

        // codex → 追加 -c mcp_servers...
        let (out, env) = apply_enhancement("codex", vec![], &enh, dir.path(), None);
        assert!(out.iter().any(|a| a == "-c"));
        assert!(out.iter().any(|a| a.starts_with("mcp_servers.nomifun-knowledge")));
        let env: HashMap<_, _> = env.into_iter().collect();
        assert_eq!(
            env.get("NOMI_KB_MCP_CAPABILITY").map(String::as_str),
            Some("scoped-bootstrap")
        );

        let (out, env) =
            apply_enhancement("/bin/bash", vec!["-l".into()], &enh, dir.path(), None);
        assert_eq!(out, vec!["-l".to_owned()]);
        assert!(env.is_empty());

        let (out, env) = apply_enhancement(
            "claude",
            vec!["-x".into()],
            &TerminalLaunchEnhancement::default(),
            dir.path(),
            None,
        );
        assert_eq!(out, vec!["-x".to_owned()]);
        assert!(env.is_empty());
    }

    #[test]
    fn toml_str_escapes_control_chars() {
        // Named escapes for \n \t
        assert_eq!(toml_str("a\nb\tc"), r#""a\nb\tc""#);
        // Raw control char U+0001 → 
        assert_eq!(toml_str("\u{1}"), "\"\\u0001\"");
    }

    #[test]
    fn codex_empty_args_renders_empty_array() {
        let server = McpServerSpec {
            name: "simple".into(),
            command: "/bin/echo".into(),
            args: vec![],
            env: HashMap::new(),
        };
        let enh = TerminalLaunchEnhancement { mcp_servers: vec![server], lifecycle: None };
        let argv = codex_mcp_argv(&enh);
        let joined = argv.join(" ");
        assert!(joined.contains("mcp_servers.simple.args=[]"));
    }

    #[test]
    fn codex_skips_non_bare_key_safe_name_and_emits_safe_ones() {
        let bad = McpServerSpec {
            name: "bad.name".into(),
            command: "/bin/x".into(),
            args: vec![],
            env: HashMap::new(),
        };
        let good = McpServerSpec {
            name: "nomifun-knowledge".into(),
            command: "/opt/nomi/nomicore".into(),
            args: vec!["mcp-knowledge-stdio".into()],
            env: HashMap::new(),
        };
        let enh = TerminalLaunchEnhancement { mcp_servers: vec![bad, good], lifecycle: None };
        let argv = codex_mcp_argv(&enh);
        let joined = argv.join(" ");
        // bad.name must NOT appear in output
        assert!(!joined.contains("bad.name"));
        // good name emitted normally
        assert!(joined.contains("mcp_servers.nomifun-knowledge.command="));
    }

    #[test]
    fn apply_enhancement_with_lifecycle_renders_hooks_and_env() {
        let dir = tempfile::TempDir::new().unwrap();
        let terminal_id = TerminalId::new();
        let enh = TerminalLaunchEnhancement {
            mcp_servers: vec![],
            lifecycle: Some(LifecycleHookWiring {
                port: 5151,
                token: "htok".into(),
                terminal_id: terminal_id.clone(),
                binary_path: "/opt/nomi/nomicore".into(),
            }),
        };

        // claude: --settings file written with Stop/PostToolUse/Notification hooks; env carries hook wiring
        let (args, env) = apply_enhancement("claude", vec![], &enh, dir.path(), None);
        assert!(args.iter().any(|a| a == "--settings"));
        let env_map: HashMap<String, String> = env.into_iter().collect();
        assert_eq!(env_map.get("NOMI_TERM_HOOK_PORT").map(String::as_str), Some("5151"));
        assert_eq!(env_map.get("NOMI_TERM_HOOK_TOKEN").map(String::as_str), Some("htok"));
        assert_eq!(
            env_map.get("NOMI_TERM_HOOK_ID").map(String::as_str),
            Some(terminal_id.as_str())
        );
        // settings file contains Stop/PostToolUse/Notification hooks calling `terminal-hook`
        let settings_path = args.iter().position(|a| a == "--settings").map(|i| args[i + 1].clone()).unwrap();
        let doc: serde_json::Value = serde_json::from_slice(&std::fs::read(&settings_path).unwrap()).unwrap();
        assert!(doc["hooks"]["Stop"][0]["hooks"][0]["command"].as_str().unwrap().contains("terminal-hook --event turn_end"));
        assert!(doc["hooks"]["PostToolUse"][0]["hooks"][0]["command"].as_str().unwrap().contains("terminal-hook --event tool_use"));
        assert!(doc["hooks"]["Notification"][0]["hooks"][0]["command"].as_str().unwrap().contains("terminal-hook --event notification"));

        // codex: hook overrides + same env; obsolete CLI flags must never be
        // injected because argument parsing happens before the TUI starts.
        let (cargs, cenv) = apply_enhancement("codex", vec![], &enh, dir.path(), None);
        assert!(!cargs.iter().any(|a| a == "--dangerously-bypass-hook-trust"));
        let cenv_map: HashMap<String, String> = cenv.into_iter().collect();
        assert_eq!(cenv_map.get("NOMI_TERM_HOOK_PORT").map(String::as_str), Some("5151"));
        assert_eq!(cenv_map.get("NOMI_TERM_HOOK_TOKEN").map(String::as_str), Some("htok"));
        assert_eq!(
            cenv_map.get("NOMI_TERM_HOOK_ID").map(String::as_str),
            Some(terminal_id.as_str())
        );
        // codex hooks: Stop, PostToolUse, SessionStart (no Notification)
        let joined = cargs.join(" ");
        assert!(joined.contains("hooks.Stop="));
        assert!(joined.contains("hooks.PostToolUse="));
        assert!(joined.contains("hooks.SessionStart="));
        assert!(!joined.contains("Notification"));
        // Each hook `-c` value contains `terminal-hook --event`
        assert!(joined.contains("terminal-hook --event turn_end"));
        assert!(joined.contains("terminal-hook --event tool_use"));
        assert!(joined.contains("terminal-hook --event session_start"));

        // unknown CLI → no hook args, no hook env (honest)
        let (uargs, uenv) = apply_enhancement("/bin/bash", vec![], &enh, dir.path(), None);
        assert!(uargs.is_empty() && uenv.is_empty());
    }

    #[test]
    fn lifecycle_hooks_shell_quote_binary_path_with_spaces() {
        let dir = tempfile::TempDir::new().unwrap();
        let enh = TerminalLaunchEnhancement {
            mcp_servers: vec![],
            lifecycle: Some(LifecycleHookWiring {
                port: 5151,
                token: "htok".into(),
                terminal_id: TerminalId::new(),
                binary_path: "/Users/John Doe/bin/nomicore".into(),
            }),
        };

        // claude: the settings.json hook commands must contain the quoted binary
        let (args, _env) = apply_enhancement("claude", vec![], &enh, dir.path(), None);
        let settings_path = args.iter().position(|a| a == "--settings").map(|i| args[i + 1].clone()).unwrap();
        let doc: serde_json::Value = serde_json::from_slice(&std::fs::read(&settings_path).unwrap()).unwrap();
        let stop_cmd = doc["hooks"]["Stop"][0]["hooks"][0]["command"].as_str().unwrap();
        assert!(
            stop_cmd.contains(r#""/Users/John Doe/bin/nomicore""#),
            "claude hook command must shell-quote the binary path, got: {stop_cmd}"
        );

        // codex: the `-c` TOML hook values must contain the shell-quoted binary
        // (the inner quotes get TOML-escaped inside the TOML string value)
        let (cargs, _cenv) = apply_enhancement("codex", vec![], &enh, dir.path(), None);
        let joined = cargs.join(" ");
        // Inside the TOML string, the shell double-quotes become escaped: \"
        // The command inside TOML looks like: \""/Users/John Doe/bin/nomicore\" terminal-hook ...\"
        // When joined in the argv the literal chars are: \"/Users/John Doe/bin/nomicore\"
        assert!(
            joined.contains(r#"\"/Users/John Doe/bin/nomicore\""#),
            "codex hook command must shell-quote the binary path (TOML-escaped), got: {joined}"
        );
    }

    #[test]
    fn resolve_agent_family_prefers_declared_then_stem_then_wrapped_token() {
        use AgentCli::*;
        // declared backend wins
        assert_eq!(resolve_agent_family("stepcode", &["claude".into()], Some("codex")), Some(Codex));
        // program stem
        assert_eq!(resolve_agent_family("/usr/bin/claude", &[], None), Some(Claude));
        assert_eq!(resolve_agent_family("codex", &[], None), Some(Codex));
        assert_eq!(resolve_agent_family("gemini", &[], None), Some(Gemini));
        // wrapper: program is unknown, a known family appears as an arg token
        assert_eq!(resolve_agent_family("stepcode", &["claude".into(), "--yolo".into()], None), Some(Claude));
        assert_eq!(resolve_agent_family("npx", &["codex".into()], None), Some(Codex));
        // none: unknown program, no known token, no declared backend
        assert_eq!(resolve_agent_family("/bin/bash", &["-l".into()], None), None);
        assert_eq!(resolve_agent_family("stepcode", &["frobnicate".into()], None), None);
    }

    #[test]
    fn apply_enhancement_wrapper_resolves_family_via_declared_and_args() {
        let dir = tempfile::TempDir::new().unwrap();
        let enh = TerminalLaunchEnhancement { mcp_servers: vec![sample_kb_server()], lifecycle: None };

        // Wrapper `stepcode claude` with no declared backend → resolves to Claude via arg token
        let (out, _env) = apply_enhancement("stepcode", vec!["claude".into()], &enh, dir.path(), None);
        assert!(out.iter().any(|a| a == "--mcp-config"), "wrapper 'stepcode claude' must render claude --mcp-config");

        // Declared backend overrides: program is stepcode, arg is claude, but declared is codex → codex
        let (out, _env) = apply_enhancement("stepcode", vec!["claude".into()], &enh, dir.path(), Some("codex"));
        assert!(out.iter().any(|a| a == "-c"), "declared codex must render codex -c overrides");
        assert!(out.iter().any(|a| a.starts_with("mcp_servers.nomifun-knowledge")));

        // Unknown wrapper with no known arg token → no injection (honest)
        let (out, _env) = apply_enhancement("stepcode", vec!["frob".into()], &enh, dir.path(), None);
        assert_eq!(out, vec!["frob".to_owned()], "unknown wrapper must not inject");

        // Gemini via declared → no launch injection (honest: no flag renderer)
        let (out, _env) = apply_enhancement("stepcode", vec!["claude".into()], &enh, dir.path(), Some("gemini"));
        // Gemini = no-op for launch injection, args are unchanged
        assert_eq!(out, vec!["claude".to_owned()], "gemini declared must not inject launch flags");
    }

    #[test]
    fn supports_lifecycle_hooks_only_claude_and_codex() {
        assert!(AgentCli::Claude.supports_lifecycle_hooks());
        assert!(AgentCli::Codex.supports_lifecycle_hooks());
        // Gemini has no launch-time lifecycle renderer → not autowork-capable.
        assert!(!AgentCli::Gemini.supports_lifecycle_hooks());
    }

    #[test]
    fn terminal_autowork_capable_matches_what_apply_enhancement_hooks() {
        let no_args: Vec<String> = vec![];

        // Bare / direct agent CLIs are capable.
        assert!(terminal_autowork_capable("claude", &no_args, None));
        assert!(terminal_autowork_capable("codex", &no_args, None));

        // Wrappers resolve via the arg token (no declared backend) — exactly the
        // case the old backend-string gate wrongly rejected.
        assert!(terminal_autowork_capable("stepcode", &["claude".to_owned()], None));
        assert!(terminal_autowork_capable("npx", &["codex".to_owned()], None));

        // Declared backend wins.
        assert!(terminal_autowork_capable("stepcode", &["claude".to_owned()], Some("codex")));

        // Gemini resolves to a family but has no lifecycle renderer → NOT capable.
        assert!(!terminal_autowork_capable("gemini", &no_args, None));
        assert!(!terminal_autowork_capable("stepcode", &["gemini".to_owned()], None));

        // Plain shell / unknown CLI → not capable.
        assert!(!terminal_autowork_capable(crate::types::SHELL_SENTINEL, &no_args, None));
        assert!(!terminal_autowork_capable("/bin/bash", &["-l".to_owned()], None));
        assert!(!terminal_autowork_capable("stepcode", &["frobnicate".to_owned()], None));
    }
}

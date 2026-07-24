use nomifun_api_types::TerminalSessionResponse;
use nomifun_common::ConversationId;
use nomifun_db::TerminalSessionRow;
use std::collections::HashMap;
use std::path::Path;

/// Sentinel `command` value meaning "use the platform login shell". Resolved at
/// spawn time so the stored row stays portable across machines.
pub const SHELL_SENTINEL: &str = "$SHELL";

/// Reserved key stored only inside `terminal_sessions.env` until a dedicated
/// owner column can be introduced. It is backend metadata, never a real child
/// process environment variable.
///
/// Every create path removes a caller-supplied value first, and every spawn
/// path removes it again before constructing the PTY environment. Keeping the
/// key private to this crate makes that invariant hard to bypass accidentally.
pub(crate) const OWNER_CONVERSATION_ID_ENV_KEY: &str =
    "__NOMIFUN_INTERNAL_OWNER_CONVERSATION_ID";

/// Parsed owner classification for one persisted terminal row.
///
/// Malformed JSON or an invalid reserved value is kept separate from
/// `Standalone`: callers must fail closed instead of accidentally exposing a
/// corrupted conversation terminal in the standalone sidebar.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum TerminalOwnerScope {
    Standalone,
    Conversation(ConversationId),
    Invalid,
}

/// Remove all backend-only metadata before a map is passed to a child process.
pub(crate) fn strip_internal_terminal_env(
    env: Option<HashMap<String, String>>,
) -> Option<HashMap<String, String>> {
    let mut env = env.unwrap_or_default();
    env.remove(OWNER_CONVERSATION_ID_ENV_KEY);
    (!env.is_empty()).then_some(env)
}

/// Build the authoritative map persisted in `terminal_sessions.env`.
///
/// A caller cannot forge ownership: its reserved entry is removed first, then
/// the server-selected owner (if any) is written back.
pub(crate) fn persisted_terminal_env(
    env: Option<HashMap<String, String>>,
    owner_conversation_id: Option<&ConversationId>,
) -> Option<HashMap<String, String>> {
    let mut env = strip_internal_terminal_env(env).unwrap_or_default();
    if let Some(owner_conversation_id) = owner_conversation_id {
        env.insert(
            OWNER_CONVERSATION_ID_ENV_KEY.to_owned(),
            owner_conversation_id.to_string(),
        );
    }
    (!env.is_empty()).then_some(env)
}

/// Parse a persisted environment for PTY use while stripping backend metadata.
/// Malformed legacy JSON is tolerated as an empty environment, matching the
/// terminal service's previous relaunch behavior.
pub(crate) fn process_env_from_persisted(
    env_json: Option<&str>,
) -> Option<HashMap<String, String>> {
    let env = env_json.and_then(|json| serde_json::from_str(json).ok());
    strip_internal_terminal_env(env)
}

/// Decode the conversation owner from a persisted row.
pub(crate) fn terminal_owner_scope(env_json: Option<&str>) -> TerminalOwnerScope {
    let Some(env_json) = env_json else {
        return TerminalOwnerScope::Standalone;
    };
    let Ok(env) = serde_json::from_str::<HashMap<String, String>>(env_json) else {
        return TerminalOwnerScope::Invalid;
    };
    let Some(raw_owner) = env.get(OWNER_CONVERSATION_ID_ENV_KEY) else {
        return TerminalOwnerScope::Standalone;
    };
    match ConversationId::parse(raw_owner) {
        Ok(owner) => TerminalOwnerScope::Conversation(owner),
        Err(_) => TerminalOwnerScope::Invalid,
    }
}

/// Resolve the launch (program, argv) for a session, expanding the shell
/// sentinel to the platform default shell and resolving a bare program name
/// to its absolute executable path.
pub fn resolve_command(command: &str, args: &[String]) -> (String, Vec<String>) {
    let program = if command == SHELL_SENTINEL {
        default_login_shell()
    } else {
        command.to_owned()
    };
    (resolve_program(&program), args.to_vec())
}

/// Resolve a bare command name to its absolute executable path so the PTY
/// backend (portable-pty) launches the intended file.
///
/// portable-pty's own Windows `PATH` search picks the extension-less npm
/// shell shim (e.g. `…\npm\claude`), which `CreateProcessW` rejects with
/// "not a valid Win32 application" (os error 193). `resolve_command_path`
/// honours `PATHEXT` plus the `.cmd / .ps1 / .bat` fallback, so `claude`
/// resolves to `…\claude.cmd` — which ConPTY runs correctly.
///
/// Inputs that already contain a path separator (an absolute path, or the
/// expanded login shell) or that don't resolve are returned unchanged.
/// Uses the bundled-toolchain resolver before the PTY backend starts the process.
fn resolve_program(program: &str) -> String {
    if !program.is_empty()
        && !program.contains('/')
        && !program.contains('\\')
        && let Some(path) = nomifun_runtime::resolve_command_path(program)
    {
        return path.to_string_lossy().into_owned();
    }
    program.to_owned()
}

/// The platform's default interactive shell.
pub fn default_login_shell() -> String {
    #[cfg(windows)]
    {
        // Prefer PowerShell profiles for newly-created interactive sessions.
        // `%ComSpec%` is normally `cmd.exe`; using it first makes common
        // cross-platform commands such as `pwd` and `ls` fail even though the
        // PTY itself is healthy.  Resolve an absolute executable up front so
        // Store/MSI installations and npm-style PATH entries are handled by
        // the same runtime resolver as explicit terminal commands.
        for candidate in ["pwsh.exe", "powershell.exe"] {
            if let Some(path) = nomifun_runtime::resolve_command_path(candidate) {
                return path.to_string_lossy().into_owned();
            }
        }

        std::env::var("ComSpec").unwrap_or_else(|_| "cmd.exe".to_owned())
    }
    #[cfg(not(windows))]
    {
        std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".to_owned())
    }
}

/// Parse the JSON args array stored on a row, tolerating malformed values.
pub fn parse_args(json: &str) -> Vec<String> {
    serde_json::from_str::<Vec<String>>(json).unwrap_or_default()
}

/// Build the API response for a session row. `scrollback_b64` is filled in by
/// the caller for single-session GET only.
///
/// `work_dir` is the backend-managed default work dir; the response exposes a
/// derived `is_default_workpath` flag (cwd equals or sits under `work_dir`)
/// without storing it on the row — same pattern as conversations'
/// `is_temporary_workspace` (nomifun-conversation/src/convert.rs).
pub fn row_to_response(
    row: &TerminalSessionRow,
    scrollback_b64: Option<String>,
    work_dir: &Path,
) -> TerminalSessionResponse {
    // `Path::starts_with` already covers the `cwd == work_dir` equality case.
    // Guard both sides against blanks: an empty `work_dir` would make every
    // path "start with" it, and an empty cwd carries no grouping signal.
    let is_default_workpath =
        !row.cwd.is_empty() && !work_dir.as_os_str().is_empty() && Path::new(&row.cwd).starts_with(work_dir);
    let owner_conversation_id = match terminal_owner_scope(row.env.as_deref()) {
        TerminalOwnerScope::Conversation(owner) => Some(owner),
        TerminalOwnerScope::Standalone | TerminalOwnerScope::Invalid => None,
    };
    TerminalSessionResponse {
        terminal_id: row.terminal_id.clone(),
        owner_conversation_id,
        name: row.name.clone(),
        cwd: row.cwd.clone(),
        is_default_workpath,
        command: row.command.clone(),
        args: parse_args(&row.args),
        backend: row.backend.clone(),
        mode: row.mode.clone(),
        cols: row.cols as u16,
        rows: row.rows as u16,
        created_at: row.created_at,
        updated_at: row.updated_at,
        last_status: row.last_status.clone(),
        exit_code: row.exit_code.map(|c| c as i32),
        pinned: row.pinned,
        pinned_at: row.pinned_at,
        scrollback_b64,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_row() -> TerminalSessionRow {
        TerminalSessionRow {
            id: 1,
            terminal_id: nomifun_common::TerminalId::new(),
            name: "shell".into(),
            cwd: "/tmp".into(),
            command: "$SHELL".into(),
            args: r#"["-l"]"#.into(),
            env: None,
            backend: None,
            mode: None,
            cols: 100,
            rows: 30,
            created_at: 10,
            updated_at: 20,
            last_status: "running".into(),
            exit_code: None,
            user_id: nomifun_common::UserId::new(),
            pinned: false,
            pinned_at: None,
            autowork: None,
            idmm: None,
        }
    }

    #[test]
    fn resolve_command_expands_shell_sentinel() {
        let (program, args) = resolve_command(SHELL_SENTINEL, &["-l".to_owned()]);
        assert_ne!(program, SHELL_SENTINEL);
        assert!(!program.is_empty());
        assert_eq!(args, vec!["-l".to_owned()]);
    }

    #[cfg(windows)]
    #[test]
    fn default_shell_prefers_the_first_available_powershell() {
        let expected = ["pwsh.exe", "powershell.exe"]
            .into_iter()
            .find_map(nomifun_runtime::resolve_command_path)
            .map(|path| path.to_string_lossy().into_owned());

        if let Some(expected) = expected {
            assert_eq!(default_login_shell(), expected);
        } else {
            assert_eq!(
                default_login_shell(),
                std::env::var("ComSpec").unwrap_or_else(|_| "cmd.exe".to_owned())
            );
        }
    }

    #[test]
    fn resolve_command_resolves_bare_name_to_absolute_path() {
        // A bare command present on PATH must resolve to an absolute executable
        // path so the PTY backend (portable-pty) launches the real file. On
        // Windows this is what turns an npm `claude` shim into `claude.cmd`
        // instead of the extension-less shell script CreateProcessW rejects
        // (os error 193).
        #[cfg(windows)]
        let bare = "cmd";
        #[cfg(not(windows))]
        let bare = "sh";

        let (program, args) = resolve_command(bare, &["--flag".to_owned()]);
        assert_ne!(program, bare, "bare name should resolve to an absolute path");
        assert!(
            std::path::Path::new(&program).is_absolute(),
            "resolved program should be absolute, got {program}"
        );
        #[cfg(windows)]
        assert!(
            program.to_ascii_lowercase().ends_with("cmd.exe"),
            "expected cmd.exe, got {program}"
        );
        assert_eq!(args, vec!["--flag".to_owned()], "args must be preserved");
    }

    #[test]
    fn resolve_command_passes_through_unresolvable_command() {
        // A name that isn't on PATH can't be resolved; keep it verbatim so the
        // spawn error surfaces the original command the user asked for.
        let name = "nomifun-definitely-not-on-path-xyz-987";
        let (program, args) = resolve_command(name, &["a".to_owned()]);
        assert_eq!(program, name);
        assert_eq!(args, vec!["a".to_owned()]);
    }

    #[test]
    fn resolve_command_passes_through_path_like_command() {
        // Inputs that already carry a path separator are used as-is — no PATH
        // search, matching the production resolver above.
        let p = if cfg!(windows) {
            r"C:\tools\my agent.exe"
        } else {
            "/opt/tools/my-agent"
        };
        let (program, _args) = resolve_command(p, &[]);
        assert_eq!(program, p);
    }

    #[test]
    fn parse_args_handles_valid_and_invalid() {
        assert_eq!(parse_args(r#"["a","b"]"#), vec!["a".to_owned(), "b".to_owned()]);
        assert!(parse_args("not json").is_empty());
        assert!(parse_args("[]").is_empty());
    }

    #[test]
    fn persisted_owner_is_server_authoritative_and_never_reaches_process_env() {
        let forged = ConversationId::new();
        let authoritative = ConversationId::new();
        let mut caller_env = HashMap::from([
            ("VISIBLE".to_owned(), "yes".to_owned()),
            (
                OWNER_CONVERSATION_ID_ENV_KEY.to_owned(),
                forged.to_string(),
            ),
        ]);

        let persisted = persisted_terminal_env(
            Some(std::mem::take(&mut caller_env)),
            Some(&authoritative),
        )
        .expect("owner metadata should be persisted");
        assert_eq!(
            persisted.get(OWNER_CONVERSATION_ID_ENV_KEY),
            Some(&authoritative.to_string())
        );
        let json = serde_json::to_string(&persisted).unwrap();
        assert_eq!(
            terminal_owner_scope(Some(&json)),
            TerminalOwnerScope::Conversation(authoritative)
        );

        let process_env =
            process_env_from_persisted(Some(&json)).expect("visible env should remain");
        assert_eq!(process_env.get("VISIBLE").map(String::as_str), Some("yes"));
        assert!(!process_env.contains_key(OWNER_CONVERSATION_ID_ENV_KEY));
    }

    #[test]
    fn malformed_owner_metadata_fails_closed() {
        let invalid_owner = serde_json::json!({
            OWNER_CONVERSATION_ID_ENV_KEY: "not-a-conversation-id"
        })
        .to_string();
        assert_eq!(
            terminal_owner_scope(Some(&invalid_owner)),
            TerminalOwnerScope::Invalid
        );
        assert_eq!(
            terminal_owner_scope(Some("not-json")),
            TerminalOwnerScope::Invalid
        );
    }

    #[test]
    fn row_to_response_parses_args_and_maps_fields() {
        let resp = row_to_response(&sample_row(), Some("c2I=".into()), Path::new("/work"));
        assert!(nomifun_common::validate_uuidv7(resp.terminal_id.as_str()).is_ok());
        assert_eq!(resp.args, vec!["-l".to_owned()]);
        assert_eq!((resp.cols, resp.rows), (100, 30));
        assert_eq!(resp.scrollback_b64.as_deref(), Some("c2I="));
        assert_eq!(resp.last_status, "running");
        assert!(resp.owner_conversation_id.is_none());
    }

    #[test]
    fn row_to_response_exposes_conversation_owner() {
        let owner = ConversationId::new();
        let mut row = sample_row();
        row.env = Some(
            serde_json::to_string(
                &persisted_terminal_env(None, Some(&owner)).expect("owner metadata"),
            )
            .unwrap(),
        );
        let resp = row_to_response(&row, None, Path::new("/work"));
        assert_eq!(resp.owner_conversation_id, Some(owner));
    }

    #[test]
    fn row_to_response_derives_is_default_workpath() {
        let work_dir = Path::new("/srv/nomi-work");
        let mut row = sample_row();

        // cwd equal to work_dir → default workpath (starts_with covers equality).
        row.cwd = "/srv/nomi-work".into();
        assert!(row_to_response(&row, None, work_dir).is_default_workpath);

        // cwd under work_dir → default workpath.
        row.cwd = "/srv/nomi-work/projects/demo".into();
        assert!(row_to_response(&row, None, work_dir).is_default_workpath);

        // cwd outside work_dir → custom workpath. A same-prefix sibling must
        // not match either (component-wise, not string-prefix, semantics).
        row.cwd = "/Users/alice/my-project".into();
        assert!(!row_to_response(&row, None, work_dir).is_default_workpath);
        row.cwd = "/srv/nomi-workspace".into();
        assert!(!row_to_response(&row, None, work_dir).is_default_workpath);

        // Blank guards: empty cwd, or an unset work_dir, never claim the group.
        row.cwd = String::new();
        assert!(!row_to_response(&row, None, work_dir).is_default_workpath);
        row.cwd = "/srv/nomi-work".into();
        assert!(!row_to_response(&row, None, Path::new("")).is_default_workpath);
    }
}

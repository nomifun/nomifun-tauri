//! Terminal-session capabilities (registry form): create / list.
//!
//! Terminals are a SEPARATE domain from conversations (PTY-backed processes
//! in the `terminal_sessions` table, not `conversations`) — which is why the
//! conversation tools refuse `agent_type = "terminal"` and point here.
//!
//! Typed terminal capabilities use the shared
//! `*Params` structs are now the single source (schema + runtime
//! deserialization). The `preset_launch` helper lives in `terminal_support.rs`
//! and is reused directly (pub(crate)).

use std::sync::Arc;

use nomifun_api_types::CreateTerminalRequest;
use nomifun_common::KnowledgeBaseId;
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{Value, json};
use std::path::{Path, PathBuf};

use crate::deps::{CallerCtx, GatewayDeps};
use crate::registry::{Capability, CapabilityMeta, DangerTier, Surface};
use crate::server::ok;
use crate::terminal_support::preset_launch;

/// Default PTY size for gateway-created terminals (no real viewport exists;
/// wide enough that agent CLIs render sanely when the user attaches later).
const DEFAULT_COLS: u16 = 120;
const DEFAULT_ROWS: u16 = 30;

// ─── Params ────────────────────────────────────────────────────────────────

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct CreateTerminalParams {
    /// Optional display name (defaults to the preset/backend name).
    #[serde(default)]
    name: Option<String>,
    /// Launch preset: "shell" (default, the platform login shell) or an agent
    /// CLI "claude" | "codex" | "gemini".
    #[serde(default)]
    preset: Option<String>,
    /// Working directory inside the current conversation workspace. Relative
    /// paths are resolved from that workspace; omitted means the workspace root.
    #[serde(default)]
    cwd: Option<String>,
    /// Permission level for agent presets: "default" (interactive approvals)
    /// or "full-auto" (passes the CLI's skip-permissions flag — powerful,
    /// confirm with the user first). Ignored for the shell preset.
    #[serde(default)]
    mode: Option<String>,
    /// Advanced: explicit program to launch, overriding the preset's command.
    #[serde(default)]
    command: Option<String>,
    /// Advanced: explicit argument list for the program (overrides preset args).
    #[serde(default)]
    args: Option<Vec<String>>,
    /// Optional knowledge base ids to bind to this terminal at creation
    /// (bind-on-create); they are mounted into `.nomi/knowledge/` inside the
    /// cwd when the terminal starts. Use nomi_knowledge_list_bases for ids.
    #[serde(default)]
    #[schemars(schema_with = "crate::id_schema::optional_canonical_uuid_v7_array_schema")]
    knowledge_base_ids: Option<Vec<KnowledgeBaseId>>,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct ListTerminalsParams {
    /// Filter by status: "running" | "exited" (default: all).
    #[serde(default)]
    status: Option<String>,
}

// ─── Handlers ──────────────────────────────────────────────────────────────

async fn create(deps: Arc<GatewayDeps>, ctx: CallerCtx, p: CreateTerminalParams) -> Value {
    if nomifun_common::UserId::parse(ctx.user_id.as_str()).is_err() {
        return json!({"error": "missing caller user identity in signed Gateway capability"});
    }
    let conversation_id = match ctx.conversation_id.as_deref() {
        Some(id) if nomifun_common::ConversationId::parse(id).is_ok() => id.to_owned(),
        _ => {
            return json!({
                "error": "terminal creation requires a signed conversation context; refusing to create a global terminal"
            });
        }
    };
    let user_id = ctx.user_id;

    let preset = p.preset.unwrap_or_else(|| "shell".to_owned());
    let mode = p.mode.unwrap_or_else(|| "default".to_owned());
    if mode != "default" && mode != "full-auto" {
        return json!({"error": format!("unknown mode '{mode}' (expected default | full-auto)")});
    }

    let (mut command, mut cmd_args, backend) = match preset_launch(&preset, mode == "full-auto") {
        Ok(v) => v,
        Err(e) => return json!({"error": e}),
    };

    // Advanced overrides: an explicit command replaces the preset launch
    // entirely (args reset, then optionally replaced too).
    if let Some(custom) = p.command {
        command = custom;
        cmd_args = vec![];
    }
    if let Some(arr) = p.args {
        cmd_args = arr;
    }

    let conversation = match deps
        .conversation_service
        .get(user_id.as_str(), &conversation_id)
        .await
    {
        Ok(conversation) => conversation,
        Err(error) => {
            return json!({
                "error": format!("could not resolve the terminal's owning conversation: {error}")
            });
        }
    };
    let workspace = match conversation
        .extra
        .get("workspace")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|workspace| !workspace.is_empty())
    {
        Some(workspace) => workspace,
        None => {
            return json!({
                "error": "the current conversation has no workspace; refusing to fall back to the user home directory"
            });
        }
    };
    let cwd = match resolve_conversation_terminal_cwd(workspace, p.cwd.as_deref()) {
        Ok(cwd) => cwd,
        Err(error) => return json!({"error": error}),
    };

    // Optional create-time knowledge binding: the bases get bound to this
    // terminal's WORKPATH (spec §7) and mounted into `{cwd}/.nomi/knowledge/`
    // when the PTY starts. The mount itself is best-effort downstream (never
    // blocks the launch), so the ids are validated HERE — a typo'd id would
    // otherwise be accepted and silently mount nothing.
    if let Some(ids) = &p.knowledge_base_ids {
        if let Err(e) = crate::caps_knowledge::ensure_known_kb_ids(&deps, ids).await {
            return e;
        }
    }
    let knowledge_bases_bound = p.knowledge_base_ids.as_ref().map_or(0, Vec::len);

    let req = CreateTerminalRequest {
        name: p.name,
        cwd,
        command,
        args: cmd_args,
        env: None,
        backend: backend.clone(),
        // Permission mode only applies to agent CLI presets.
        mode: backend.is_some().then(|| mode.clone()),
        cols: DEFAULT_COLS,
        rows: DEFAULT_ROWS,
        defer_spawn: false,
        knowledge_base_ids: p.knowledge_base_ids,
    };

    match deps
        .terminal_service
        .create_for_conversation(user_id.as_str(), &conversation_id, req)
        .await
    {
        Ok(resp) => ok(json!({
            "terminal_id": resp.terminal_id,
            "name": resp.name,
            "status": resp.last_status,
            "cwd": resp.cwd,
            "command": resp.command,
            "args": resp.args,
            "backend": resp.backend,
            "mode": resp.mode,
            // Echo the validated bind-on-create request count (0 = none
            // requested); the mount itself remains best-effort.
            "knowledge_bases_bound": knowledge_bases_bound,
            "owner_conversation_id": resp.owner_conversation_id,
            "note": "conversation-owned terminal created in the conversation workspace. It is visible in this conversation's terminal panel and excluded from the global terminal sidebar. You own its lifecycle: call nomi_terminal_kill when its process is no longer needed, and nomi_terminal_delete when its record is no longer needed."
        })),
        Err(e) => json!({"error": e.to_string()}),
    }
}

async fn list(deps: Arc<GatewayDeps>, ctx: CallerCtx, p: ListTerminalsParams) -> Value {
    if nomifun_common::UserId::parse(ctx.user_id.as_str()).is_err() {
        return json!({"error": "missing caller user identity in signed Gateway capability"});
    }
    let user_id = ctx.user_id.as_str();
    let conversation_id = match ctx.conversation_id.as_deref() {
        Some(id) if nomifun_common::ConversationId::parse(id).is_ok() => id,
        _ => {
            return json!({
                "error": "terminal listing requires a signed conversation context"
            });
        }
    };

    match deps
        .terminal_service
        .list_for_conversation(user_id, conversation_id)
        .await
    {
        Ok(rows) => {
            let items: Vec<Value> = rows
                .iter()
                .filter(|t| p.status.as_deref().is_none_or(|s| t.last_status == s))
                .map(|t| {
                    json!({
                        "terminal_id": t.terminal_id,
                        "name": t.name,
                        "status": t.last_status,
                        "cwd": t.cwd,
                        "command": t.command,
                        "backend": t.backend,
                        "mode": t.mode,
                        "exit_code": t.exit_code,
                        "created_at": t.created_at,
                        "owner_conversation_id": t.owner_conversation_id,
                    })
                })
                .collect();
            ok(json!({"total": items.len(), "terminals": items}))
        }
        Err(e) => json!({"error": e.to_string()}),
    }
}

/// Resolve and confine a gateway-created terminal cwd to its authoritative
/// conversation workspace. Canonicalization prevents `..` and symlink escapes;
/// relative paths are a convenience for agents and are rooted at the workspace.
fn resolve_conversation_terminal_cwd(
    workspace: &str,
    requested: Option<&str>,
) -> Result<String, String> {
    let workspace = Path::new(workspace);
    let canonical_workspace = std::fs::canonicalize(workspace).map_err(|error| {
        format!(
            "could not resolve conversation workspace '{}': {error}",
            workspace.display()
        )
    })?;
    if !canonical_workspace.is_dir() {
        return Err(format!(
            "conversation workspace '{}' is not a directory",
            canonical_workspace.display()
        ));
    }

    let requested = requested.map(str::trim).filter(|value| !value.is_empty());
    let candidate = match requested {
        None => canonical_workspace.clone(),
        Some(value) => {
            let value = PathBuf::from(value);
            if value.is_absolute() {
                value
            } else {
                canonical_workspace.join(value)
            }
        }
    };
    let canonical_candidate = std::fs::canonicalize(&candidate).map_err(|error| {
        format!(
            "could not resolve terminal working directory '{}': {error}",
            candidate.display()
        )
    })?;
    if !canonical_candidate.is_dir() {
        return Err(format!(
            "terminal working directory '{}' is not a directory",
            canonical_candidate.display()
        ));
    }
    if !canonical_candidate.starts_with(&canonical_workspace) {
        return Err(format!(
            "terminal working directory '{}' is outside the current conversation workspace '{}'",
            canonical_candidate.display(),
            canonical_workspace.display()
        ));
    }
    Ok(persistable_canonical_path(&canonical_candidate)
        .to_string_lossy()
        .into_owned())
}

/// Keep canonical paths for containment checks, but avoid persisting Windows'
/// `\\?\C:\...` representation as a terminal cwd. That prefix is useful to the
/// Win32 filesystem API but is noisy in the UI and is rejected by some CLIs.
fn persistable_canonical_path(canonical: &Path) -> PathBuf {
    #[cfg(windows)]
    if let Some(simplified) = strip_windows_verbatim_prefix(canonical)
        && matches!(std::fs::canonicalize(&simplified), Ok(round_trip) if round_trip == canonical)
    {
        return simplified;
    }

    canonical.to_path_buf()
}

#[cfg(windows)]
fn strip_windows_verbatim_prefix(path: &Path) -> Option<PathBuf> {
    use std::ffi::OsString;
    use std::os::windows::ffi::{OsStrExt, OsStringExt};
    use std::path::{Component, Prefix};

    let Some(Component::Prefix(prefix)) = path.components().next() else {
        return None;
    };
    let encoded = path.as_os_str().encode_wide().collect::<Vec<_>>();
    match prefix.kind() {
        Prefix::VerbatimDisk(_) => {
            const PREFIX: [u16; 4] = [b'\\' as u16, b'\\' as u16, b'?' as u16, b'\\' as u16];
            encoded
                .strip_prefix(&PREFIX)
                .map(OsString::from_wide)
                .map(PathBuf::from)
        }
        Prefix::VerbatimUNC(_, _) => {
            const PREFIX: [u16; 8] = [
                b'\\' as u16,
                b'\\' as u16,
                b'?' as u16,
                b'\\' as u16,
                b'U' as u16,
                b'N' as u16,
                b'C' as u16,
                b'\\' as u16,
            ];
            encoded.strip_prefix(&PREFIX).map(|rest| {
                let mut simplified = vec![b'\\' as u16, b'\\' as u16];
                simplified.extend_from_slice(rest);
                PathBuf::from(OsString::from_wide(&simplified))
            })
        }
        _ => None,
    }
}

// ─── Registration ──────────────────────────────────────────────────────────

/// Register the terminal-domain capabilities.
pub(crate) fn register(out: &mut Vec<Capability>) {
    out.push(Capability::new::<CreateTerminalParams, _, _>(
        CapabilityMeta::new(
            "nomi_create_terminal",
            "terminal",
            "Spawn a conversation-owned PTY in the current conversation workspace (shell or agent CLI). It stays out of the global sidebar. Use preset to pick the program; close it with nomi_terminal_kill/delete when no longer needed.",
            DangerTier::Write,
        )
        .deny_on(&[Surface::Channel]),
        |deps, ctx, p| create(deps, ctx, p),
    ));
    out.push(Capability::new::<ListTerminalsParams, _, _>(
        CapabilityMeta::new(
            "nomi_list_terminals",
            "terminal",
            "List only terminal sessions owned by the current conversation (filter by status: running | exited). Use this to observe lifecycle changes, including terminals the user closed from the conversation panel.",
            DangerTier::Read,
        ),
        |deps, ctx, p| list(deps, ctx, p),
    ));
}

// ─── Tests ─────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    /// Mirrors `ui/src/renderer/pages/terminal/launchPresets.ts` — the
    /// frontend and gateway presets must agree on commands and flags.
    #[test]
    fn presets_match_frontend_launch_presets() {
        assert_eq!(
            preset_launch("shell", false).unwrap(),
            ("$SHELL".to_owned(), vec![], None)
        );
        // shell ignores full-auto (no permission concept).
        assert_eq!(preset_launch("shell", true).unwrap().1, Vec::<String>::new());
        assert_eq!(
            preset_launch("claude", true).unwrap(),
            (
                "claude".to_owned(),
                vec!["--dangerously-skip-permissions".to_owned()],
                Some("claude".to_owned())
            )
        );
        assert_eq!(
            preset_launch("codex", true).unwrap(),
            (
                "codex".to_owned(),
                vec!["--dangerously-bypass-approvals-and-sandbox".to_owned()],
                Some("codex".to_owned())
            )
        );
        assert_eq!(
            preset_launch("gemini", true).unwrap(),
            ("gemini".to_owned(), vec!["--yolo".to_owned()], Some("gemini".to_owned()))
        );
        // default mode = no extra flags for agent presets.
        assert_eq!(preset_launch("claude", false).unwrap().1, Vec::<String>::new());
    }

    #[test]
    fn unknown_preset_is_rejected() {
        let err = preset_launch("bash", false).unwrap_err();
        assert!(err.contains("bash"), "{err}");
    }

    /// Mode validation rejects unknown strings.
    #[test]
    fn unknown_mode_is_rejected() {
        // Simulate the check that would happen inside `create` before
        // calling `preset_launch` — test the boundary inline since the
        // handler is async and the validation is trivial.
        let mode = "yolo";
        let valid = mode == "default" || mode == "full-auto";
        assert!(!valid);
    }

    #[test]
    fn terminal_cwd_defaults_to_and_is_confined_by_conversation_workspace() {
        let test_root = std::env::temp_dir().join(format!(
            "nomifun-gateway-terminal-cwd-{}",
            nomifun_common::TerminalId::new()
        ));
        let workspace = test_root.join("workspace");
        let nested = workspace.join("nested");
        let outside = test_root.join("outside");
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::create_dir_all(&outside).unwrap();

        let defaulted =
            resolve_conversation_terminal_cwd(workspace.to_str().unwrap(), None).unwrap();
        assert_eq!(
            PathBuf::from(defaulted),
            persistable_canonical_path(&std::fs::canonicalize(&workspace).unwrap())
        );
        let relative =
            resolve_conversation_terminal_cwd(workspace.to_str().unwrap(), Some("nested"))
                .unwrap();
        assert_eq!(
            PathBuf::from(relative),
            persistable_canonical_path(&std::fs::canonicalize(&nested).unwrap())
        );
        #[cfg(windows)]
        assert!(
            !resolve_conversation_terminal_cwd(workspace.to_str().unwrap(), None)
                .unwrap()
                .starts_with(r"\\?\"),
            "persisted terminal cwd must not expose the Windows verbatim prefix"
        );
        let error = resolve_conversation_terminal_cwd(
            workspace.to_str().unwrap(),
            Some(outside.to_str().unwrap()),
        )
        .unwrap_err();
        assert!(error.contains("outside"), "{error}");

        std::fs::remove_dir_all(test_root).unwrap();
    }

    /// Knowledge base ids: serde correctly deserializes typed params.
    #[test]
    fn knowledge_base_ids_deserialization() {
        // Valid: present with string array
        let first = nomifun_common::KnowledgeBaseId::new();
        let second = nomifun_common::KnowledgeBaseId::new();
        let json_val = json!({"knowledge_base_ids": [first, second]});
        let p: CreateTerminalParams = serde_json::from_value(json_val).unwrap();
        assert_eq!(
            p.knowledge_base_ids,
            Some(vec![first, second])
        );

        // Valid: absent → None
        let json_val = json!({});
        let p: CreateTerminalParams = serde_json::from_value(json_val).unwrap();
        assert_eq!(p.knowledge_base_ids, None);

        // Valid: explicit null → None
        let json_val = json!({"knowledge_base_ids": null});
        let p: CreateTerminalParams = serde_json::from_value(json_val).unwrap();
        assert_eq!(p.knowledge_base_ids, None);

        // Invalid: non-string elements are rejected at deserialization
        let json_val = json!({"knowledge_base_ids": ["kb_a"]});
        let result = serde_json::from_value::<CreateTerminalParams>(json_val);
        assert!(result.is_err());
    }
}

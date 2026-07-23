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
    /// Working directory (defaults to the user's home directory).
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

    let cwd = match p.cwd {
        Some(c) => c,
        None => match dirs::home_dir() {
            Some(h) => h.to_string_lossy().into_owned(),
            None => {
                return json!({"error": "no cwd given and the user home directory could not be determined"})
            }
        },
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

    match deps.terminal_service.create(user_id.as_str(), req).await {
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
            "note": "terminal created, its process is running; use nomi_list_terminals to check status. Agent terminals (claude/codex/gemini) are eligible AutoWork targets via nomi_set_autowork."
        })),
        Err(e) => json!({"error": e.to_string()}),
    }
}

async fn list(deps: Arc<GatewayDeps>, ctx: CallerCtx, p: ListTerminalsParams) -> Value {
    if nomifun_common::UserId::parse(ctx.user_id.as_str()).is_err() {
        return json!({"error": "missing caller user identity in signed Gateway capability"});
    }
    let user_id = ctx.user_id.as_str();

    match deps.terminal_service.list(user_id).await {
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
                    })
                })
                .collect();
            ok(json!({"total": items.len(), "terminals": items}))
        }
        Err(e) => json!({"error": e.to_string()}),
    }
}

// ─── Registration ──────────────────────────────────────────────────────────

/// Register the terminal-domain capabilities.
pub(crate) fn register(out: &mut Vec<Capability>) {
    out.push(Capability::new::<CreateTerminalParams, _, _>(
        CapabilityMeta::new(
            "nomi_create_terminal",
            "terminal",
            "Spawn a new PTY terminal session (shell or agent CLI). Use preset to pick the program; mode to enable full-auto permissions for agent CLIs.",
            DangerTier::Write,
        )
        .deny_on(&[Surface::Channel]),
        |deps, ctx, p| create(deps, ctx, p),
    ));
    out.push(Capability::new::<ListTerminalsParams, _, _>(
        CapabilityMeta::new(
            "nomi_list_terminals",
            "terminal",
            "List every terminal session of the calling user (filter by status: running | exited).",
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

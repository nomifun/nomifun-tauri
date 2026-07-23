//! Extended terminal-session capabilities (registry form): get / write_input /
//! kill / delete / resize / relaunch / update.
//!
//! Companion module to `caps_terminal.rs` (which covers create / list). These
//! are the remaining mutation and query endpoints that a gateway-connected agent
//! needs to fully manage PTY sessions.

use std::sync::Arc;
use std::borrow::Cow;

use schemars::{JsonSchema, Schema, SchemaGenerator};
use serde::de;
use serde::{Deserialize, Deserializer};
use serde_json::{Value, json};

use crate::deps::{CallerCtx, GatewayDeps};
use crate::registry::{Capability, CapabilityMeta, DangerTier, Surface};
use crate::server::ok;

// 闁冲厜鍋撻柍鍏夊亾闁冲厜鍋?Params 闁冲厜鍋撻柍鍏夊亾闁冲厜鍋撻柍鍏夊亾闁冲厜鍋撻柍鍏夊亾闁冲厜鍋撻柍鍏夊亾闁冲厜鍋撻柍鍏夊亾闁冲厜鍋撻柍鍏夊亾闁冲厜鍋撻柍鍏夊亾闁冲厜鍋撻柍鍏夊亾闁冲厜鍋撻柍鍏夊亾闁冲厜鍋撻柍鍏夊亾闁冲厜鍋撻柍鍏夊亾闁冲厜鍋撻柍鍏夊亾闁冲厜鍋撻柍鍏夊亾闁冲厜鍋撻柍鍏夊亾闁冲厜鍋撻柍鍏夊亾闁冲厜鍋撻柍鍏夊亾闁冲厜鍋撻柍鍏夊亾闁冲厜鍋撻柍鍏夊亾闁冲厜鍋撻柍鍏夊亾闁冲厜鍋撻柍鍏夊亾闁冲厜鍋撻柍鍏夊亾闁冲厜鍋撻柍鍏夊亾闁冲厜鍋撻柍鍏夊亾闁冲厜鍋撻柍鍏夊亾闁冲厜鍋撻柍鍏夊亾闁冲厜鍋撻柍鍏夊亾闁冲厜鍋撻柍鍏夊亾闁冲厜鍋撻柍鍏夊亾闁冲厜鍋撻柍鍏夊亾闁冲厜鍋撻柍鍏夊亾闁冲厜鍋撻柍鍏夊亾闁冲厜鍋撻柍鍏夊亾

/// A canonical terminal entity ID. MCP clients must pass the exact bare UUIDv7
/// string returned by the terminal APIs; numbers and legacy prefixes are
/// rejected.
#[derive(Debug, Clone, PartialEq, Eq)]
struct TerminalId(String);

impl TerminalId {
    fn as_str(&self) -> &str {
        &self.0
    }
}

impl<'de> Deserialize<'de> for TerminalId {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        nomifun_common::TerminalId::try_from(value.as_str())
            .map_err(de::Error::custom)?;
        Ok(Self(value))
    }
}

impl JsonSchema for TerminalId {
    fn inline_schema() -> bool { true }
    fn schema_name() -> Cow<'static, str> { "TerminalId".into() }
    fn json_schema(_generator: &mut SchemaGenerator) -> Schema {
        schemars::json_schema!({
            "type": "string",
            "pattern": "^[0-9a-f]{8}-[0-9a-f]{4}-7[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$",
            "description": "Canonical NomiFun terminal UUIDv7 entity ID."
        })
    }
}
/// Parameters for reading a single terminal session's detail/status.
#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct GetTerminalParams {
    /// The terminal session business ID (from nomi_list_terminals).
    terminal_id: TerminalId,
}

/// Parameters for writing bytes/keystrokes to a terminal's PTY.
#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct WriteInputParams {
    /// The terminal session business ID.
    terminal_id: TerminalId,
    /// Base64-encoded bytes to write to the PTY stdin. Encode raw keystrokes
    /// (including control sequences like \r for Enter, \x03 for Ctrl-C) as
    /// base64 before passing here.
    data_b64: String,
}

/// Parameters for submitting text to a terminal so it EXECUTES (the high-level
/// "type it and press Enter" op 闁?no base64, no manual newline).
#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct SubmitTerminalParams {
    /// The terminal session business ID (from nomi_list_terminals).
    terminal_id: TerminalId,
    /// Plain UTF-8 text/command to type into the terminal and RUN. Do NOT
    /// base64-encode and do NOT append a newline 闁?submission (Enter) is handled
    /// for you, including the bracketed-paste sequence agent CLIs (claude/codex/
    /// gemini) need so the text actually executes instead of sitting unrun.
    text: String,
    /// Wait for the turn to settle and return an output tail. Default false
    /// (fire-and-forget). true 闁?also returns settle_reason + output_tail.
    #[serde(default)]
    wait: bool,
    /// Max seconds to wait when `wait` is true (default 300, capped 1800).
    #[serde(default)]
    timeout_secs: Option<u64>,
}

/// Parameters for reading a terminal's recent output (ANSI-stripped scrollback tail).
#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct ReadTerminalOutputParams {
    /// The terminal session business ID.
    terminal_id: TerminalId,
    /// Max bytes of the scrollback TAIL to return after ANSI stripping
    /// (default 16384, capped 65536).
    #[serde(default)]
    max_bytes: Option<usize>,
}

/// Parameters for terminating a terminal's running process (SIGKILL).
#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct KillTerminalParams {
    /// The terminal session business ID.
    terminal_id: TerminalId,
}

/// Parameters for permanently deleting a terminal session (kills process + removes row).
#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct DeleteTerminalParams {
    /// The terminal session business ID.
    terminal_id: TerminalId,
}

/// Parameters for resizing a terminal's PTY (cols x rows).
#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct ResizeTerminalParams {
    /// The terminal session business ID.
    terminal_id: TerminalId,
    /// Number of columns (width in characters).
    cols: u16,
    /// Number of rows (height in characters).
    rows: u16,
}

/// Parameters for relaunching a terminal's process in place (same terminal_id,
/// fresh child process).
#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct RelaunchTerminalParams {
    /// The terminal session business ID.
    terminal_id: TerminalId,
}

/// Parameters for updating a terminal session's metadata (rename / pin).
#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct UpdateTerminalParams {
    /// The terminal session business ID.
    terminal_id: TerminalId,
    /// New display name (omit to keep current).
    #[serde(default)]
    name: Option<String>,
    /// Pin (true) or unpin (false) the terminal; pinned terminals persist in
    /// the sidebar. Omit to keep current.
    #[serde(default)]
    pinned: Option<bool>,
}

// 闁冲厜鍋撻柍鍏夊亾闁冲厜鍋?Handlers 闁冲厜鍋撻柍鍏夊亾闁冲厜鍋撻柍鍏夊亾闁冲厜鍋撻柍鍏夊亾闁冲厜鍋撻柍鍏夊亾闁冲厜鍋撻柍鍏夊亾闁冲厜鍋撻柍鍏夊亾闁冲厜鍋撻柍鍏夊亾闁冲厜鍋撻柍鍏夊亾闁冲厜鍋撻柍鍏夊亾闁冲厜鍋撻柍鍏夊亾闁冲厜鍋撻柍鍏夊亾闁冲厜鍋撻柍鍏夊亾闁冲厜鍋撻柍鍏夊亾闁冲厜鍋撻柍鍏夊亾闁冲厜鍋撻柍鍏夊亾闁冲厜鍋撻柍鍏夊亾闁冲厜鍋撻柍鍏夊亾闁冲厜鍋撻柍鍏夊亾闁冲厜鍋撻柍鍏夊亾闁冲厜鍋撻柍鍏夊亾闁冲厜鍋撻柍鍏夊亾闁冲厜鍋撻柍鍏夊亾闁冲厜鍋撻柍鍏夊亾闁冲厜鍋撻柍鍏夊亾闁冲厜鍋撻柍鍏夊亾闁冲厜鍋撻柍鍏夊亾闁冲厜鍋撻柍鍏夊亾闁冲厜鍋撻柍鍏夊亾闁冲厜鍋撻柍鍏夊亾闁冲厜鍋撻柍鍏夊亾闁冲厜鍋撻柍鍏夊亾

async fn get_terminal(deps: Arc<GatewayDeps>, ctx: CallerCtx, p: GetTerminalParams) -> Value {
    if nomifun_common::UserId::parse(ctx.user_id.as_str()).is_err() {
        return json!({"error": "missing caller user identity in signed Gateway capability"});
    }
    let terminal_id = p.terminal_id.as_str();
    match deps.terminal_service.get(terminal_id).await {
        Ok(resp) => ok(json!({
            "terminal_id": resp.terminal_id,
            "name": resp.name,
            "status": resp.last_status,
            "cwd": resp.cwd,
            "command": resp.command,
            "args": resp.args,
            "backend": resp.backend,
            "mode": resp.mode,
            "cols": resp.cols,
            "rows": resp.rows,
            "exit_code": resp.exit_code,
            "pinned": resp.pinned,
            "created_at": resp.created_at,
            "updated_at": resp.updated_at,
        })),
        Err(e) => json!({"error": e.to_string()}),
    }
}

async fn write_input(deps: Arc<GatewayDeps>, ctx: CallerCtx, p: WriteInputParams) -> Value {
    if nomifun_common::UserId::parse(ctx.user_id.as_str()).is_err() {
        return json!({"error": "missing caller user identity in signed Gateway capability"});
    }
    let terminal_id = p.terminal_id.as_str();
    match deps
        .terminal_service
        .input(terminal_id, &p.data_b64)
        .await
    {
        Ok(()) => ok(json!({"written": true})),
        Err(e) => json!({"error": e.to_string()}),
    }
}

async fn submit_terminal(deps: Arc<GatewayDeps>, ctx: CallerCtx, p: SubmitTerminalParams) -> Value {
    if nomifun_common::UserId::parse(ctx.user_id.as_str()).is_err() {
        return json!({"error": "missing caller user identity in signed Gateway capability"});
    }
    let terminal_id = p.terminal_id.as_str();
    if let Err(e) = deps
        .terminal_service
        .submit_text(terminal_id, &p.text)
        .await
    {
        // A not-live session is the common, actionable failure 闁?point at relaunch.
        return json!({
            "error": e.to_string(),
            "hint": "if the session has exited, call nomi_terminal_relaunch first, then retry"
        });
    }
    if !p.wait {
        return ok(json!({"submitted": true, "terminal_id": terminal_id, "note": "text submitted; use nomi_terminal_read_output to see the result"}));
    }
    let secs = p.timeout_secs.unwrap_or(300).min(1800);
    let reason = deps
        .terminal_service
        .await_turn_settle(terminal_id, std::time::Duration::from_secs(secs))
        .await;
    let tail = deps
        .terminal_service
        .read_output_tail(terminal_id, 4096)
        .await
        .map(|t| t.text)
        .unwrap_or_default();
    ok(json!({
        "submitted": true,
        "terminal_id": terminal_id,
        "settle_reason": reason,
        "output_tail": tail,
    }))
}

async fn read_terminal_output(deps: Arc<GatewayDeps>, ctx: CallerCtx, p: ReadTerminalOutputParams) -> Value {
    if nomifun_common::UserId::parse(ctx.user_id.as_str()).is_err() {
        return json!({"error": "missing caller user identity in signed Gateway capability"});
    }
    let terminal_id = p.terminal_id.as_str();
    let cap = p.max_bytes.unwrap_or(16_384).min(65_536);
    match deps
        .terminal_service
        .read_output_tail(terminal_id, cap)
        .await
    {
        Ok(t) => ok(json!({
            "terminal_id": terminal_id,
            "text": t.text,
            "truncated": t.truncated,
            "status": t.status,
        })),
        Err(e) => json!({"error": e.to_string()}),
    }
}

async fn kill_terminal(deps: Arc<GatewayDeps>, ctx: CallerCtx, p: KillTerminalParams) -> Value {
    if nomifun_common::UserId::parse(ctx.user_id.as_str()).is_err() {
        return json!({"error": "missing caller user identity in signed Gateway capability"});
    }
    let terminal_id = p.terminal_id.as_str();
    match deps.terminal_service.kill(terminal_id).await {
        Ok(()) => ok(json!({"killed": true, "terminal_id": terminal_id})),
        Err(e) => json!({"error": e.to_string()}),
    }
}

async fn delete_terminal(deps: Arc<GatewayDeps>, ctx: CallerCtx, p: DeleteTerminalParams) -> Value {
    if nomifun_common::UserId::parse(ctx.user_id.as_str()).is_err() {
        return json!({"error": "missing caller user identity in signed Gateway capability"});
    }
    let terminal_id = p.terminal_id.as_str();
    match deps.terminal_service.delete(terminal_id).await {
        Ok(()) => ok(json!({"deleted": true, "terminal_id": terminal_id})),
        Err(e) => json!({"error": e.to_string()}),
    }
}

async fn resize_terminal(deps: Arc<GatewayDeps>, ctx: CallerCtx, p: ResizeTerminalParams) -> Value {
    if nomifun_common::UserId::parse(ctx.user_id.as_str()).is_err() {
        return json!({"error": "missing caller user identity in signed Gateway capability"});
    }
    let terminal_id = p.terminal_id.as_str();
    match deps
        .terminal_service
        .resize(terminal_id, p.cols, p.rows)
        .await
    {
        Ok(()) => ok(json!({"resized": true, "terminal_id": terminal_id, "cols": p.cols, "rows": p.rows})),
        Err(e) => json!({"error": e.to_string()}),
    }
}

async fn relaunch_terminal(deps: Arc<GatewayDeps>, ctx: CallerCtx, p: RelaunchTerminalParams) -> Value {
    if nomifun_common::UserId::parse(ctx.user_id.as_str()).is_err() {
        return json!({"error": "missing caller user identity in signed Gateway capability"});
    }
    let terminal_id = p.terminal_id.as_str();
    match deps.terminal_service.relaunch(terminal_id).await {
        Ok(resp) => ok(json!({
            "terminal_id": resp.terminal_id,
            "name": resp.name,
            "status": resp.last_status,
            "cwd": resp.cwd,
            "command": resp.command,
            "args": resp.args,
            "backend": resp.backend,
            "mode": resp.mode,
            "note": "process relaunched in place (same terminal_id, fresh child)"
        })),
        Err(e) => json!({"error": e.to_string()}),
    }
}

async fn update_terminal(deps: Arc<GatewayDeps>, ctx: CallerCtx, p: UpdateTerminalParams) -> Value {
    if nomifun_common::UserId::parse(ctx.user_id.as_str()).is_err() {
        return json!({"error": "missing caller user identity in signed Gateway capability"});
    }
    if p.name.is_none() && p.pinned.is_none() {
        return json!({"error": "nothing to update: provide at least one of name / pinned"});
    }
    let terminal_id = p.terminal_id.as_str();
    match deps
        .terminal_service
        .update_meta(terminal_id, p.name, p.pinned)
        .await
    {
        Ok(resp) => ok(json!({
            "terminal_id": resp.terminal_id,
            "name": resp.name,
            "pinned": resp.pinned,
            "status": resp.last_status,
        })),
        Err(e) => json!({"error": e.to_string()}),
    }
}

// 闁冲厜鍋撻柍鍏夊亾闁冲厜鍋?Registration 闁冲厜鍋撻柍鍏夊亾闁冲厜鍋撻柍鍏夊亾闁冲厜鍋撻柍鍏夊亾闁冲厜鍋撻柍鍏夊亾闁冲厜鍋撻柍鍏夊亾闁冲厜鍋撻柍鍏夊亾闁冲厜鍋撻柍鍏夊亾闁冲厜鍋撻柍鍏夊亾闁冲厜鍋撻柍鍏夊亾闁冲厜鍋撻柍鍏夊亾闁冲厜鍋撻柍鍏夊亾闁冲厜鍋撻柍鍏夊亾闁冲厜鍋撻柍鍏夊亾闁冲厜鍋撻柍鍏夊亾闁冲厜鍋撻柍鍏夊亾闁冲厜鍋撻柍鍏夊亾闁冲厜鍋撻柍鍏夊亾闁冲厜鍋撻柍鍏夊亾闁冲厜鍋撻柍鍏夊亾闁冲厜鍋撻柍鍏夊亾闁冲厜鍋撻柍鍏夊亾闁冲厜鍋撻柍鍏夊亾闁冲厜鍋撻柍鍏夊亾闁冲厜鍋撻柍鍏夊亾闁冲厜鍋撻柍鍏夊亾闁冲厜鍋撻柍鍏夊亾闁冲厜鍋撻柍鍏夊亾闁冲厜鍋撻柍鍏夊亾闁冲厜鍋撻柍鍏夊亾

/// Register the extended terminal-domain capabilities.
pub(crate) fn register(out: &mut Vec<Capability>) {
    out.push(Capability::new::<GetTerminalParams, _, _>(
        CapabilityMeta::new(
            "nomi_terminal_get",
            "terminal",
            "Get a single terminal session's detail and current status (running/exited, exit code, dimensions, etc.).",
            DangerTier::Read,
        ),
        |deps, ctx, p| get_terminal(deps, ctx, p),
    ));
    out.push(Capability::new::<WriteInputParams, _, _>(
        CapabilityMeta::new(
            "nomi_terminal_write_input",
            "terminal",
            "Write base64-encoded bytes/keystrokes to a terminal's PTY stdin. Powerful: can execute arbitrary commands in the running shell. For sending a command/prompt to run, prefer nomi_terminal_send (handles Enter + agent-CLI paste); use this for raw control bytes like Ctrl-C.",
            DangerTier::Write,
        )
        .deny_on(&[Surface::Channel]),
        |deps, ctx, p| write_input(deps, ctx, p),
    ));
    out.push(Capability::new::<SubmitTerminalParams, _, _>(
        CapabilityMeta::new(
            "nomi_terminal_send",
            "terminal",
            "Type text/a command into a terminal and RUN it (plain text, no base64, no manual newline 闁?Enter and the agent-CLI paste sequence are handled). Optional wait=true returns settle_reason + output_tail. Preferred over nomi_terminal_write_input for sending commands.",
            DangerTier::Write,
        )
        .deny_on(&[Surface::Channel]),
        |deps, ctx, p| submit_terminal(deps, ctx, p),
    ));
    out.push(Capability::new::<ReadTerminalOutputParams, _, _>(
        CapabilityMeta::new(
            "nomi_terminal_read_output",
            "terminal",
            "Read a terminal's recent output (ANSI-stripped scrollback tail) to see a command's result or diagnose. The terminal analogue of nomi_conversation_status.",
            DangerTier::Read,
        ),
        // Intentionally no Channel deny: this is a read-only status/output view
        // and follows the registry's default Read policy, unlike send/write and
        // other terminal process mutations.
        |deps, ctx, p| read_terminal_output(deps, ctx, p),
    ));
    out.push(Capability::new::<KillTerminalParams, _, _>(
        CapabilityMeta::new(
            "nomi_terminal_kill",
            "terminal",
            "Send SIGKILL to terminate the terminal's running process. The session remains (status becomes 'exited'); use relaunch to restart or delete to remove entirely.",
            DangerTier::Destructive,
        )
        .deny_on(&[Surface::Channel]),
        |deps, ctx, p| kill_terminal(deps, ctx, p),
    ));
    out.push(Capability::new::<DeleteTerminalParams, _, _>(
        CapabilityMeta::new(
            "nomi_terminal_delete",
            "terminal",
            "Permanently delete a terminal session (kills the process if running, removes the row and all associated data).",
            DangerTier::Destructive,
        )
        .deny_on(&[Surface::Channel]),
        |deps, ctx, p| delete_terminal(deps, ctx, p),
    ));
    out.push(Capability::new::<ResizeTerminalParams, _, _>(
        CapabilityMeta::new(
            "nomi_terminal_resize",
            "terminal",
            "Resize a terminal's PTY to the given cols x rows (triggers deferred spawn if the session was created with defer_spawn).",
            DangerTier::Write,
        ),
        |deps, ctx, p| resize_terminal(deps, ctx, p),
    ));
    out.push(Capability::new::<RelaunchTerminalParams, _, _>(
        CapabilityMeta::new(
            "nomi_terminal_relaunch",
            "terminal",
            "Relaunch a terminal's process in place: kills the old child and spawns a fresh one reusing the same terminal_id, command, and cwd.",
            DangerTier::Write,
        )
        .deny_on(&[Surface::Channel]),
        |deps, ctx, p| relaunch_terminal(deps, ctx, p),
    ));
    out.push(Capability::new::<UpdateTerminalParams, _, _>(
        CapabilityMeta::new(
            "nomi_terminal_update",
            "terminal",
            "Update a terminal session's metadata: rename it and/or pin/unpin it.",
            DangerTier::Write,
        ),
        |deps, ctx, p| update_terminal(deps, ctx, p),
    ));
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const TERM_ID: &str = "0190f5fe-7c00-7a00-8abc-012345678901";

    #[test]
    fn terminal_params_require_canonical_string_ids() {
        let p: SubmitTerminalParams = serde_json::from_value(
            json!({"terminal_id": TERM_ID, "text": "git status", "wait": true, "timeout_secs": 60}),
        )
        .unwrap();
        assert_eq!(p.terminal_id.as_str(), TERM_ID);
        assert!(p.wait);
        assert_eq!(p.timeout_secs, Some(60));

        assert!(
            serde_json::from_value::<GetTerminalParams>(json!({"terminal_id": 7})).is_err()
        );
        assert!(
            serde_json::from_value::<GetTerminalParams>(json!({"terminal_id": "7"})).is_err()
        );
        assert!(serde_json::from_value::<GetTerminalParams>(json!({
            "terminal_id": "term_0190f5fe-7c00-7a00-8abc-012345678901"
        }))
        .is_err());
        assert!(
            serde_json::from_value::<GetTerminalParams>(json!({"id": TERM_ID})).is_err()
        );
    }

    #[test]
    fn terminal_tool_schemas_expose_named_canonical_business_ids() {
        use crate::registry::Registry;
        let specs = Registry::global().tool_specs(crate::registry::Surface::Desktop);
        for name in [
            "nomi_terminal_get", "nomi_terminal_write_input", "nomi_terminal_send",
            "nomi_terminal_read_output", "nomi_terminal_kill", "nomi_terminal_delete",
            "nomi_terminal_resize", "nomi_terminal_relaunch", "nomi_terminal_update",
        ] {
            let spec = specs.iter().find(|spec| spec.name == name).expect("tool registered");
            let properties = spec
                .input_schema
                .get("properties")
                .and_then(Value::as_object)
                .expect("tool properties");
            let terminal_id_schema = properties
                .get("terminal_id")
                .expect("terminal_id schema present");
            assert!(!properties.contains_key("id"), "{name}");
            assert_eq!(
                terminal_id_schema.get("type").and_then(Value::as_str),
                Some("string"),
                "{name}"
            );
            assert!(
                terminal_id_schema
                    .get("pattern")
                    .and_then(Value::as_str)
                    .is_some(),
                "{name}"
            );
        }
    }

    #[test]
    fn send_and_read_are_registered_and_desktop_visible_but_channel_denied() {
        use crate::registry::Registry;
        let reg = Registry::global();
        for name in ["nomi_terminal_send", "nomi_terminal_read_output"] {
            assert!(reg.contains(name));
            assert!(reg.tool_visible(crate::registry::Surface::Desktop, name));
        }
        assert!(!reg.tool_visible(crate::registry::Surface::Channel, "nomi_terminal_send"));
        assert!(reg.tool_visible(crate::registry::Surface::Channel, "nomi_terminal_read_output"));
    }
}

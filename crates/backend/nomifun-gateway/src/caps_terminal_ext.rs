//! Extended terminal-session capabilities (registry form): get / write_input /
//! kill / delete / resize / relaunch / update.
//!
//! Companion module to `caps_terminal.rs` (which covers create / list). These
//! are the remaining mutation and query endpoints that a gateway-connected agent
//! needs to fully manage PTY sessions.

use std::sync::Arc;
use std::{borrow::Cow, fmt};

use schemars::{JsonSchema, Schema, SchemaGenerator};
use serde::de::{self, Visitor};
use serde::{Deserialize, Deserializer};
use serde_json::{Value, json};

use crate::deps::{CallerCtx, GatewayDeps};
use crate::registry::{Capability, CapabilityMeta, DangerTier, Surface};
use crate::server::ok;

// ─── Params ────────────────────────────────────────────────────────────────

/// LLM/MCP clients sometimes quote numeric tool args; keep service calls typed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct TerminalId(i64);

impl TerminalId {
    fn get(self) -> i64 {
        self.0
    }
}

impl<'de> Deserialize<'de> for TerminalId {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct TerminalIdVisitor;

        impl Visitor<'_> for TerminalIdVisitor {
            type Value = TerminalId;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("an integer terminal session id or numeric string")
            }

            fn visit_i64<E>(self, value: i64) -> Result<Self::Value, E> {
                Ok(TerminalId(value))
            }

            fn visit_u64<E>(self, value: u64) -> Result<Self::Value, E>
            where
                E: de::Error,
            {
                i64::try_from(value)
                    .map(TerminalId)
                    .map_err(|_| E::custom("terminal session id overflows i64"))
            }

            fn visit_str<E>(self, value: &str) -> Result<Self::Value, E>
            where
                E: de::Error,
            {
                let trimmed = value.trim();
                if trimmed.is_empty() {
                    return Err(E::custom("terminal session id must not be empty"));
                }
                trimmed
                    .parse::<i64>()
                    .map(TerminalId)
                    .map_err(|_| E::custom("terminal session id string must be an integer"))
            }
        }

        deserializer.deserialize_any(TerminalIdVisitor)
    }
}

impl JsonSchema for TerminalId {
    fn inline_schema() -> bool {
        true
    }

    fn schema_name() -> Cow<'static, str> {
        "TerminalId".into()
    }

    fn json_schema(_generator: &mut SchemaGenerator) -> Schema {
        schemars::json_schema!({
            "oneOf": [
                { "type": "integer", "format": "int64" },
                {
                    "type": "string",
                    "pattern": "^-?\\d+$",
                    "description": "Numeric terminal session id as a string."
                }
            ]
        })
    }
}

/// Parameters for reading a single terminal session's detail/status.
#[derive(Deserialize, JsonSchema)]
struct GetTerminalParams {
    /// The terminal session id (from nomi_list_terminals).
    id: TerminalId,
}

/// Parameters for writing bytes/keystrokes to a terminal's PTY.
#[derive(Deserialize, JsonSchema)]
struct WriteInputParams {
    /// The terminal session id.
    id: TerminalId,
    /// Base64-encoded bytes to write to the PTY stdin. Encode raw keystrokes
    /// (including control sequences like \r for Enter, \x03 for Ctrl-C) as
    /// base64 before passing here.
    data_b64: String,
}

/// Parameters for submitting text to a terminal so it EXECUTES (the high-level
/// "type it and press Enter" op — no base64, no manual newline).
#[derive(Deserialize, JsonSchema)]
struct SubmitTerminalParams {
    /// The terminal session id (from nomi_list_terminals).
    id: TerminalId,
    /// Plain UTF-8 text/command to type into the terminal and RUN. Do NOT
    /// base64-encode and do NOT append a newline — submission (Enter) is handled
    /// for you, including the bracketed-paste sequence agent CLIs (claude/codex/
    /// gemini) need so the text actually executes instead of sitting unrun.
    text: String,
    /// Wait for the turn to settle and return an output tail. Default false
    /// (fire-and-forget). true → also returns settle_reason + output_tail.
    #[serde(default)]
    wait: bool,
    /// Max seconds to wait when `wait` is true (default 300, capped 1800).
    #[serde(default)]
    timeout_secs: Option<u64>,
}

/// Parameters for reading a terminal's recent output (ANSI-stripped scrollback tail).
#[derive(Deserialize, JsonSchema)]
struct ReadTerminalOutputParams {
    /// The terminal session id.
    id: TerminalId,
    /// Max bytes of the scrollback TAIL to return after ANSI stripping
    /// (default 16384, capped 65536).
    #[serde(default)]
    max_bytes: Option<usize>,
}

/// Parameters for terminating a terminal's running process (SIGKILL).
#[derive(Deserialize, JsonSchema)]
struct KillTerminalParams {
    /// The terminal session id.
    id: TerminalId,
}

/// Parameters for permanently deleting a terminal session (kills process + removes row).
#[derive(Deserialize, JsonSchema)]
struct DeleteTerminalParams {
    /// The terminal session id.
    id: TerminalId,
}

/// Parameters for resizing a terminal's PTY (cols x rows).
#[derive(Deserialize, JsonSchema)]
struct ResizeTerminalParams {
    /// The terminal session id.
    id: TerminalId,
    /// Number of columns (width in characters).
    cols: u16,
    /// Number of rows (height in characters).
    rows: u16,
}

/// Parameters for relaunching a terminal's process in place (same session id,
/// fresh child process).
#[derive(Deserialize, JsonSchema)]
struct RelaunchTerminalParams {
    /// The terminal session id.
    id: TerminalId,
}

/// Parameters for updating a terminal session's metadata (rename / pin).
#[derive(Deserialize, JsonSchema)]
struct UpdateTerminalParams {
    /// The terminal session id.
    id: TerminalId,
    /// New display name (omit to keep current).
    #[serde(default)]
    name: Option<String>,
    /// Pin (true) or unpin (false) the terminal; pinned terminals persist in
    /// the sidebar. Omit to keep current.
    #[serde(default)]
    pinned: Option<bool>,
}

// ─── Handlers ──────────────────────────────────────────────────────────────

async fn get_terminal(deps: Arc<GatewayDeps>, ctx: CallerCtx, p: GetTerminalParams) -> Value {
    if ctx.user_id.is_empty() {
        return json!({"error": "missing caller user identity (NOMI_GW_MCP_USER_ID)"});
    }
    let id = p.id.get();
    match deps.terminal_service.get(id).await {
        Ok(resp) => ok(json!({
            "id": resp.id,
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
    if ctx.user_id.is_empty() {
        return json!({"error": "missing caller user identity (NOMI_GW_MCP_USER_ID)"});
    }
    let id = p.id.get();
    match deps.terminal_service.input(id, &p.data_b64).await {
        Ok(()) => ok(json!({"written": true})),
        Err(e) => json!({"error": e.to_string()}),
    }
}

async fn submit_terminal(deps: Arc<GatewayDeps>, ctx: CallerCtx, p: SubmitTerminalParams) -> Value {
    if ctx.user_id.is_empty() {
        return json!({"error": "missing caller user identity (NOMI_GW_MCP_USER_ID)"});
    }
    let id = p.id.get();
    if let Err(e) = deps.terminal_service.submit_text(id, &p.text).await {
        // A not-live session is the common, actionable failure — point at relaunch.
        return json!({
            "error": e.to_string(),
            "hint": "if the session has exited, call nomi_terminal_relaunch first, then retry"
        });
    }
    if !p.wait {
        return ok(json!({"submitted": true, "id": id, "note": "text submitted; use nomi_terminal_read_output to see the result"}));
    }
    let secs = p.timeout_secs.unwrap_or(300).min(1800);
    let reason = deps
        .terminal_service
        .await_turn_settle(id, std::time::Duration::from_secs(secs))
        .await;
    let tail = deps
        .terminal_service
        .read_output_tail(id, 4096)
        .await
        .map(|t| t.text)
        .unwrap_or_default();
    ok(json!({
        "submitted": true,
        "id": id,
        "settle_reason": reason,
        "output_tail": tail,
    }))
}

async fn read_terminal_output(deps: Arc<GatewayDeps>, ctx: CallerCtx, p: ReadTerminalOutputParams) -> Value {
    if ctx.user_id.is_empty() {
        return json!({"error": "missing caller user identity (NOMI_GW_MCP_USER_ID)"});
    }
    let id = p.id.get();
    let cap = p.max_bytes.unwrap_or(16_384).min(65_536);
    match deps.terminal_service.read_output_tail(id, cap).await {
        Ok(t) => ok(json!({
            "id": id,
            "text": t.text,
            "truncated": t.truncated,
            "status": t.status,
        })),
        Err(e) => json!({"error": e.to_string()}),
    }
}

async fn kill_terminal(deps: Arc<GatewayDeps>, ctx: CallerCtx, p: KillTerminalParams) -> Value {
    if ctx.user_id.is_empty() {
        return json!({"error": "missing caller user identity (NOMI_GW_MCP_USER_ID)"});
    }
    let id = p.id.get();
    match deps.terminal_service.kill(id).await {
        Ok(()) => ok(json!({"killed": true, "id": id})),
        Err(e) => json!({"error": e.to_string()}),
    }
}

async fn delete_terminal(deps: Arc<GatewayDeps>, ctx: CallerCtx, p: DeleteTerminalParams) -> Value {
    if ctx.user_id.is_empty() {
        return json!({"error": "missing caller user identity (NOMI_GW_MCP_USER_ID)"});
    }
    let id = p.id.get();
    match deps.terminal_service.delete(id).await {
        Ok(()) => ok(json!({"deleted": true, "id": id})),
        Err(e) => json!({"error": e.to_string()}),
    }
}

async fn resize_terminal(deps: Arc<GatewayDeps>, ctx: CallerCtx, p: ResizeTerminalParams) -> Value {
    if ctx.user_id.is_empty() {
        return json!({"error": "missing caller user identity (NOMI_GW_MCP_USER_ID)"});
    }
    let id = p.id.get();
    match deps.terminal_service.resize(id, p.cols, p.rows).await {
        Ok(()) => ok(json!({"resized": true, "id": id, "cols": p.cols, "rows": p.rows})),
        Err(e) => json!({"error": e.to_string()}),
    }
}

async fn relaunch_terminal(deps: Arc<GatewayDeps>, ctx: CallerCtx, p: RelaunchTerminalParams) -> Value {
    if ctx.user_id.is_empty() {
        return json!({"error": "missing caller user identity (NOMI_GW_MCP_USER_ID)"});
    }
    let id = p.id.get();
    match deps.terminal_service.relaunch(id).await {
        Ok(resp) => ok(json!({
            "id": resp.id,
            "name": resp.name,
            "status": resp.last_status,
            "cwd": resp.cwd,
            "command": resp.command,
            "args": resp.args,
            "backend": resp.backend,
            "mode": resp.mode,
            "note": "process relaunched in place (same session id, fresh child)"
        })),
        Err(e) => json!({"error": e.to_string()}),
    }
}

async fn update_terminal(deps: Arc<GatewayDeps>, ctx: CallerCtx, p: UpdateTerminalParams) -> Value {
    if ctx.user_id.is_empty() {
        return json!({"error": "missing caller user identity (NOMI_GW_MCP_USER_ID)"});
    }
    if p.name.is_none() && p.pinned.is_none() {
        return json!({"error": "nothing to update: provide at least one of name / pinned"});
    }
    let id = p.id.get();
    match deps.terminal_service.update_meta(id, p.name, p.pinned).await {
        Ok(resp) => ok(json!({
            "id": resp.id,
            "name": resp.name,
            "pinned": resp.pinned,
            "status": resp.last_status,
        })),
        Err(e) => json!({"error": e.to_string()}),
    }
}

// ─── Registration ──────────────────────────────────────────────────────────

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
            "Type text/a command into a terminal and RUN it (plain text, no base64, no manual newline — Enter and the agent-CLI paste sequence are handled). Optional wait=true returns settle_reason + output_tail. Preferred over nomi_terminal_write_input for sending commands.",
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
            "Relaunch a terminal's process in place: kills the old child and spawns a fresh one reusing the same session id, command, and cwd.",
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

    #[test]
    fn submit_params_plain_text_no_base64() {
        let p: SubmitTerminalParams =
            serde_json::from_value(json!({"id": 7, "text": "git status"})).unwrap();
        assert_eq!(p.id.get(), 7);
        assert_eq!(p.text, "git status");
        assert!(!p.wait);
        assert_eq!(p.timeout_secs, None);

        let p2: SubmitTerminalParams = serde_json::from_value(
            json!({"id": 1, "text": "run", "wait": true, "timeout_secs": 60}),
        )
        .unwrap();
        assert!(p2.wait);
        assert_eq!(p2.timeout_secs, Some(60));
    }

    #[test]
    fn terminal_id_params_accept_numeric_string_ids() {
        let p: GetTerminalParams = serde_json::from_value(json!({"id": "7"})).unwrap();
        assert_eq!(p.id.get(), 7);

        let p: WriteInputParams =
            serde_json::from_value(json!({"id": "7", "data_b64": "cHdkDQo="})).unwrap();
        assert_eq!(p.id.get(), 7);

        let p: SubmitTerminalParams =
            serde_json::from_value(json!({"id": "7", "text": "pwd"})).unwrap();
        assert_eq!(p.id.get(), 7);

        let p: ReadTerminalOutputParams = serde_json::from_value(json!({"id": "7"})).unwrap();
        assert_eq!(p.id.get(), 7);

        let p: KillTerminalParams = serde_json::from_value(json!({"id": "7"})).unwrap();
        assert_eq!(p.id.get(), 7);

        let p: DeleteTerminalParams = serde_json::from_value(json!({"id": "7"})).unwrap();
        assert_eq!(p.id.get(), 7);

        let p: ResizeTerminalParams =
            serde_json::from_value(json!({"id": "7", "cols": 120, "rows": 30})).unwrap();
        assert_eq!(p.id.get(), 7);

        let p: RelaunchTerminalParams = serde_json::from_value(json!({"id": "7"})).unwrap();
        assert_eq!(p.id.get(), 7);

        let p: UpdateTerminalParams =
            serde_json::from_value(json!({"id": "7", "name": "work"})).unwrap();
        assert_eq!(p.id.get(), 7);
    }

    #[test]
    fn terminal_id_schemas_accept_numeric_strings() {
        use crate::registry::Registry;
        let specs = Registry::global().tool_specs(crate::registry::Surface::Desktop);

        for name in [
            "nomi_terminal_get",
            "nomi_terminal_write_input",
            "nomi_terminal_send",
            "nomi_terminal_read_output",
            "nomi_terminal_kill",
            "nomi_terminal_delete",
            "nomi_terminal_resize",
            "nomi_terminal_relaunch",
            "nomi_terminal_update",
        ] {
            let spec = specs.iter().find(|spec| spec.name == name).expect("tool registered");
            let id_schema = spec
                .input_schema
                .get("properties")
                .and_then(Value::as_object)
                .and_then(|props| props.get("id"))
                .expect("id schema present");
            let variants = id_schema
                .get("oneOf")
                .or_else(|| id_schema.get("anyOf"))
                .and_then(Value::as_array)
                .expect("id schema must accept integer or numeric string");
            assert!(
                variants
                    .iter()
                    .any(|v| v.get("type").and_then(Value::as_str) == Some("integer")),
                "{name} id schema must accept integer"
            );
            assert!(
                variants
                    .iter()
                    .any(|v| v.get("type").and_then(Value::as_str) == Some("string")),
                "{name} id schema must accept numeric string"
            );
        }
    }

    #[test]
    fn read_output_params_defaults() {
        let p: ReadTerminalOutputParams = serde_json::from_value(json!({"id": 3})).unwrap();
        assert_eq!(p.id.get(), 3);
        assert_eq!(p.max_bytes, None);
    }

    #[test]
    fn send_and_read_are_registered_and_desktop_visible_but_channel_denied() {
        use crate::registry::Registry;
        let reg = Registry::global();
        for name in ["nomi_terminal_send", "nomi_terminal_read_output"] {
            assert!(reg.contains(name), "{name} must be registered");
            assert!(
                reg.tool_visible(crate::registry::Surface::Desktop, name),
                "{name} must be visible to the Desktop companion"
            );
        }
        // send 写类：渠道面必须拒绝。read 只读：渠道面可见（不放大攻击面，仅只读）。
        assert!(
            !reg.tool_visible(crate::registry::Surface::Channel, "nomi_terminal_send"),
            "nomi_terminal_send must be denied on Channel"
        );
        assert!(
            reg.tool_visible(crate::registry::Surface::Channel, "nomi_terminal_read_output"),
            "nomi_terminal_read_output is read-only and should follow Channel's default Read visibility"
        );
    }
}

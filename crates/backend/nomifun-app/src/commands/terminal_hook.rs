//! `nomicore terminal-hook --event <kind>`: one-shot shim invoked by a native
//! CLI hook (claude --settings hooks / codex hooks). Reads the CLI's hook event
//! JSON from stdin, POSTs {terminal_id, pty_epoch, kind, payload} to the in-process
//! TerminalLifecycleServer at 127.0.0.1:{NOMI_TERM_HOOK_PORT}/hook (bearer
//! NOMI_TERM_HOOK_TOKEN), then exits 0 WITHOUT emitting any blocking outcome —
//! observe-only, never alters the agent's turn.

use std::process::ExitCode;

use nomifun_common::TerminalId;

/// Build the JSON body for the lifecycle hook POST.
///
/// Pure function — maps the CLI `--event` kind, terminal_id (from env), and the
/// raw stdin JSON into the `{terminal_id, kind, payload}` envelope the
/// `TerminalLifecycleServer` expects.
pub(crate) fn build_hook_post(
    event: &str,
    terminal_id: &TerminalId,
    pty_epoch: u64,
    stdin_json: serde_json::Value,
) -> serde_json::Value {
    serde_json::json!({
        "terminal_id": terminal_id.as_str(),
        "pty_epoch": pty_epoch,
        "kind": event,
        "payload": stdin_json,
    })
}

/// Entry point for `nomicore terminal-hook --event <kind>`.
///
/// Reads env `NOMI_TERM_HOOK_{PORT,TOKEN,ID,EPOCH}` (baked at PTY spawn time by the
/// enhance layer), reads the CLI's hook event JSON from stdin, and fires a POST
/// to the in-process `TerminalLifecycleServer`. Always exits 0 — observe-only,
/// never blocks the agent's turn even on failure.
pub async fn run_terminal_hook(event: &str) -> ExitCode {
    // Identify the terminal + server from env baked at spawn. Missing → no-op
    // exit 0 (never break the agent's turn just because wiring is absent).
    let (Ok(port), Ok(token), Ok(id), Ok(epoch)) = (
        std::env::var("NOMI_TERM_HOOK_PORT"),
        std::env::var("NOMI_TERM_HOOK_TOKEN"),
        std::env::var("NOMI_TERM_HOOK_ID"),
        std::env::var("NOMI_TERM_HOOK_EPOCH"),
    ) else {
        return ExitCode::SUCCESS;
    };
    let Ok(terminal_id) = id.parse::<TerminalId>() else {
        return ExitCode::SUCCESS;
    };
    let Ok(pty_epoch) = epoch.parse::<u64>() else {
        return ExitCode::SUCCESS;
    };

    // Read the CLI's hook event JSON from stdin (best-effort; Null if empty or
    // unparseable — never fail).
    let mut buf = String::new();
    use std::io::Read as _;
    let _ = std::io::stdin().read_to_string(&mut buf);
    let stdin_json: serde_json::Value =
        serde_json::from_str(&buf).unwrap_or(serde_json::Value::Null);

    let body = build_hook_post(event, &terminal_id, pty_epoch, stdin_json);

    // Reuse the bridge HTTP client (short-lived, no keepalive pool).
    let client = super::stdio_common::build_bridge_http_client();
    let url = format!("http://127.0.0.1:{port}/hook");

    // Fire-and-forget with a tight per-request bound; hook must never stall the
    // agent's turn. The client's 5s connect_timeout covers "not listening"; this
    // bounds a slow response body too.
    let _ = client
        .post(&url)
        .header("Authorization", format!("Bearer {token}"))
        .timeout(std::time::Duration::from_secs(5))
        .json(&body)
        .send()
        .await;

    ExitCode::SUCCESS
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_hook_post_maps_event_and_wraps_payload() {
        let stdin =
            serde_json::json!({"last_assistant_message": "done", "stop_hook_active": false});
        let terminal_id = TerminalId::new();
        let body = build_hook_post("turn_end", &terminal_id, 37, stdin.clone());
        assert_eq!(body["terminal_id"], terminal_id.as_str());
        assert_eq!(body["pty_epoch"], 37);
        assert_eq!(body["kind"], "turn_end");
        assert_eq!(body["payload"], stdin);
    }

    #[test]
    fn build_hook_post_null_payload() {
        let terminal_id = TerminalId::new();
        let body =
            build_hook_post("session_start", &terminal_id, 38, serde_json::Value::Null);
        assert_eq!(body["terminal_id"], terminal_id.as_str());
        assert_eq!(body["pty_epoch"], 38);
        assert_eq!(body["kind"], "session_start");
        assert!(body["payload"].is_null());
    }
}

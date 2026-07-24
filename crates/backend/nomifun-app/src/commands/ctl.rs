//! `nomicore` user-facing CLI verbs (the CLI adapter): `tools` and `call`.
//!
//! `tools` is offline — it reads the deps-free capability Registry directly.
//! `call` is a thin HTTP client to a RUNNING instance's REST `/v1`
//! adapter (they do NOT boot a second backend — the exclusive `server.lock`
//! forbids that). Endpoint + token come from `--url`/`--token` or the
//! `NOMIFUN_URL` / `NOMIFUN_COMPANION_TOKEN` env vars (the token is a
//! per-companion access token; the caller runs as the bound companion).

use std::process::ExitCode;

use nomifun_gateway::{Registry, Surface};
use serde_json::{Value, json};

/// `nomicore tools` — list the capabilities exposed on the Remote surface
/// (name + description), as JSON. Offline; no running instance required.
pub async fn run_tools() -> ExitCode {
    let tools: Vec<Value> = Registry::global()
        .tool_specs(Surface::Remote)
        .into_iter()
        .map(|s| json!({ "name": s.name, "description": s.description }))
        .collect();
    let out = json!({ "count": tools.len(), "tools": tools });
    println!("{}", serde_json::to_string_pretty(&out).unwrap_or_else(|_| out.to_string()));
    ExitCode::SUCCESS
}

const DEFAULT_URL: &str = "http://127.0.0.1:25808";

fn resolve_endpoint(url: Option<String>, token: Option<String>) -> Result<(String, String), String> {
    let base = url
        .or_else(|| std::env::var("NOMIFUN_URL").ok())
        .map(|u| u.trim_end_matches('/').to_owned())
        .filter(|u| !u.is_empty())
        .unwrap_or_else(|| DEFAULT_URL.to_owned());
    let token = token
        .or_else(|| std::env::var("NOMIFUN_COMPANION_TOKEN").ok())
        .filter(|t| !t.trim().is_empty())
        .ok_or_else(|| {
            "no access token: pass --token or set NOMIFUN_COMPANION_TOKEN (mint one in the \
             desktop app per companion via POST /api/webui/companions/{id}/access-token)"
                .to_owned()
        })?;
    Ok((base, token))
}

/// `nomicore call <name> [json-args]` — invoke a capability on a running
/// instance via REST `/v1/tools/{name}`. Prints the result JSON; exit code
/// reflects HTTP success.
pub async fn run_call(name: &str, args: Option<&str>, url: Option<String>, token: Option<String>) -> ExitCode {
    let (base, token) = match resolve_endpoint(url, token) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("{e}");
            return ExitCode::from(2);
        }
    };
    let body: Value = match args {
        Some(s) if !s.trim().is_empty() => match serde_json::from_str(s) {
            Ok(v) => v,
            Err(e) => {
                eprintln!("invalid JSON args: {e}");
                return ExitCode::from(2);
            }
        },
        _ => json!({}),
    };
    let client = reqwest::Client::new();
    let idempotency_key =
        format!("nomicore-call-v1-{}", uuid::Uuid::new_v4());
    match client
        .post(format!("{base}/v1/tools/{name}"))
        .bearer_auth(&token)
        .header("Idempotency-Key", idempotency_key)
        .json(&body)
        .send()
        .await
    {
        Ok(resp) => {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            println!("{text}");
            if status.is_success() {
                ExitCode::SUCCESS
            } else {
                eprintln!("HTTP {status}");
                ExitCode::from(1)
            }
        }
        Err(e) => {
            eprintln!("request to {base}/v1/tools/{name} failed: {e} (is NomiFun running and NOMIFUN_URL correct?)");
            ExitCode::from(1)
        }
    }
}

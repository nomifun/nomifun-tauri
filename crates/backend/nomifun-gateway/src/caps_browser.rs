//! Browser-domain capabilities (registry form, feature-gated). Lets a
//! remote/master agent drive the desktop's in-process CDP browser, scoped +
//! serialized per companion via [`crate::browser_registry::BrowserRegistry`].
//!
//! The GW2 out-of-band approval state machine still gates irreversible browser
//! actions for default callers, while full-auto/yolo callers bypass that hold to
//! keep browser use low-friction. Browser tools are NOT denied on the Channel
//! surface: remote browser driving is the entire point.
//!
//! Only compiled when the `browser-use` feature is on.

use std::sync::Arc;

use nomi_browser::{ApprovalTier, OUT_OF_BAND_CONFIRMED_KEY};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{Map, Value, json};

use crate::browser_registry::{BrowserRegistry, tool_result_to_value};
use crate::deps::{CallerCtx, GatewayDeps};
use crate::registry::{Capability, CapabilityMeta, DangerTier};

// ── params ────────────────────────────────────────────────────────────────

#[derive(Deserialize, JsonSchema)]
struct NavigateParams {
    /// The URL to load in the caller's browser.
    url: String,
    /// Open in a new tab instead of the current one (default false).
    #[serde(default)]
    new_tab: Option<bool>,
}

#[derive(Deserialize, JsonSchema)]
struct ObserveParams {
    /// Optional cap on the aria-snapshot depth (for huge pages).
    #[serde(default)]
    max_depth: Option<u64>,
}

#[derive(Deserialize, JsonSchema)]
struct ActParams {
    /// The facade action name (click / type / scroll / screenshot /
    /// get_page_text / back / press_key / …). Re-observe after any action that
    /// changes the page (refs go stale).
    action: String,
    /// Action-specific params (ref / text / url / keys / …), passed through
    /// verbatim to the browser facade.
    #[serde(flatten)]
    rest: Map<String, Value>,
}

#[derive(Deserialize, JsonSchema)]
struct ConfirmParams {
    /// The call_id from an `approval_required` envelope.
    call_id: String,
    /// "proceed_once" to approve the held irreversible action, "cancel" to deny.
    #[serde(default)]
    option: Option<String>,
}

// ── per-caller registry + GW2 helpers (ported verbatim) ───────────────────

fn registry_and_key<'a>(deps: &'a GatewayDeps, ctx: &CallerCtx) -> Result<(&'a BrowserRegistry, String), Value> {
    let registry = deps
        .browser_registry
        .as_ref()
        .ok_or_else(|| json!({"error": "browser tools are not available on this desktop"}))?;
    let key = BrowserRegistry::key_for(ctx.companion_id.as_deref(), &ctx.conversation_id);
    Ok((registry, key))
}

/// Strip any caller-supplied out-of-band sentinel before classify/forward (trust boundary).
fn sanitize_out_of_band(mut input: Value) -> Value {
    if let Some(obj) = input.as_object_mut() {
        obj.remove(OUT_OF_BAND_CONFIRMED_KEY);
    }
    input
}

fn approval_required_value(call_id: &str, action: &str, args: &Value) -> Value {
    json!({
        "approval_required": {
            "call_id": call_id,
            "title": format!("Approve irreversible browser action: {action}"),
            "description": describe_pending(action, args),
            "how_to": "This action is irreversible (submit / payment / delete / send) and the \
                       caller does not auto-approve Browser approval. Relay this to the user; \
                       once they approve, call nomi_browser_confirm with this call_id and option \
                       \"proceed_once\" (or \"cancel\" to deny).",
            "options": [
                {"label": "Approve once", "value": "proceed_once"},
                {"label": "Deny", "value": "cancel"},
            ],
        }
    })
}

fn describe_pending(action: &str, args: &Value) -> String {
    let detail = match action {
        "navigate" => args.get("url").and_then(Value::as_str).map(|u| format!("navigate to {u}")),
        "click" => args.get("ref").and_then(Value::as_str).map(|r| format!("click [ref={r}]")),
        "press_key" => args.get("keys").and_then(Value::as_str).map(|k| format!("press {k}")),
        "reload" => Some("reload the page".to_string()),
        _ => None,
    };
    match detail {
        Some(d) => format!("Will {d} — irreversible (may submit / pay / delete / send)."),
        None => format!("Will run irreversible action `{action}` (may submit / pay / delete / send)."),
    }
}

/// Gate an outbound action through out-of-band approval. `input` MUST already be
/// sanitized. Returns `Some(json)` to short-circuit, `None` to proceed.
fn caller_bypasses_browser_approval(ctx: &CallerCtx) -> bool {
    matches!(
        ctx.session_mode.as_deref().map(str::trim),
        Some("yolo" | "yoloNoSandbox" | "full-access" | "bypassPermissions")
    )
}

fn gw2_gate(ctx: &CallerCtx, registry: &BrowserRegistry, key: &str, action: &str, input: &Value) -> Option<Value> {
    if caller_bypasses_browser_approval(ctx) {
        return None;
    }
    if registry.classify(key, action, input) != ApprovalTier::Irreversible {
        return None;
    }
    match registry.stash_pending(key, input.clone()) {
        Some(call_id) => Some(approval_required_value(&call_id, action, input)),
        None => Some(json!({
            "error": "too many browser actions are awaiting approval; resolve or cancel some via \
                      nomi_browser_confirm before issuing more irreversible actions"
        })),
    }
}

// ── handlers ────────────────────────────────────────────────────────────────

async fn navigate(deps: Arc<GatewayDeps>, ctx: CallerCtx, p: NavigateParams) -> Value {
    let (registry, key) = match registry_and_key(&deps, &ctx) {
        Ok(v) => v,
        Err(e) => return e,
    };
    let input = json!({"action": "navigate", "url": p.url, "new_tab": p.new_tab.unwrap_or(false)});
    if let Some(short_circuit) = gw2_gate(&ctx, registry, &key, "navigate", &input) {
        return short_circuit;
    }
    tool_result_to_value(registry.execute(&key, input).await)
}

async fn observe(deps: Arc<GatewayDeps>, ctx: CallerCtx, p: ObserveParams) -> Value {
    let (registry, key) = match registry_and_key(&deps, &ctx) {
        Ok(v) => v,
        Err(e) => return e,
    };
    let mut input = json!({"action": "observe"});
    if let Some(d) = p.max_depth {
        input["max_depth"] = json!(d);
    }
    if let Some(short_circuit) = gw2_gate(&ctx, registry, &key, "observe", &input) {
        return short_circuit;
    }
    tool_result_to_value(registry.execute(&key, input).await)
}

async fn act(deps: Arc<GatewayDeps>, ctx: CallerCtx, p: ActParams) -> Value {
    let (registry, key) = match registry_and_key(&deps, &ctx) {
        Ok(v) => v,
        Err(e) => return e,
    };
    // Reconstruct the facade input from the passthrough params, strip any
    // caller-supplied sentinel (trust boundary), then set the validated action.
    let mut input = sanitize_out_of_band(Value::Object(p.rest));
    input["action"] = json!(p.action);
    if let Some(short_circuit) = gw2_gate(&ctx, registry, &key, &p.action, &input) {
        return short_circuit;
    }
    tool_result_to_value(registry.execute(&key, input).await)
}

async fn confirm(deps: Arc<GatewayDeps>, ctx: CallerCtx, p: ConfirmParams) -> Value {
    let (registry, key) = match registry_and_key(&deps, &ctx) {
        Ok(v) => v,
        Err(e) => return e,
    };
    let option = p.option.as_deref().map(str::trim).unwrap_or("cancel");
    let approve = matches!(option, "proceed_once" | "proceed_always" | "approve" | "yes");

    let Some(pending) = registry.take_pending(&p.call_id) else {
        return json!({"error": format!("no pending browser approval with call_id {} (already resolved, expired, or never existed)", p.call_id)});
    };
    if pending.key != key {
        return json!({"error": "this pending browser approval belongs to a different session and cannot be resolved here"});
    }
    if !approve {
        return json!({"resolved": p.call_id, "approved": false, "result": {"text": "Denied. The irreversible browser action was not run."}});
    }
    let mut envelope = tool_result_to_value(registry.execute_confirmed(&key, pending.input).await);
    if let Some(obj) = envelope.as_object_mut() {
        obj.insert("resolved".to_string(), json!(p.call_id));
        obj.insert("approved".to_string(), json!(true));
    }
    envelope
}

pub(crate) fn register(out: &mut Vec<Capability>) {
    out.push(Capability::new::<NavigateParams, _, _>(
        CapabilityMeta::new("nomi_browser_navigate", "browser", "Load a URL in the caller's browser (optionally a new tab).", DangerTier::Write),
        navigate,
    ));
    out.push(Capability::new::<ObserveParams, _, _>(
        CapabilityMeta::new("nomi_browser_observe", "browser", "Read the page's accessibility tree (aria snapshot + ref table) to target later. Read-only.", DangerTier::Read),
        observe,
    ));
    out.push(Capability::new::<ActParams, _, _>(
        CapabilityMeta::new("nomi_browser_act", "browser", "Run any browser action (click/type/scroll/screenshot/...); irreversible actions are held for out-of-band approval.", DangerTier::Write),
        act,
    ));
    out.push(Capability::new::<ConfirmParams, _, _>(
        CapabilityMeta::new("nomi_browser_confirm", "browser", "Resolve a pending out-of-band browser approval (proceed_once / cancel).", DangerTier::Write),
        confirm,
    ));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_strips_caller_supplied_out_of_band_sentinel() {
        let dirty = json!({"action": "click", "ref": "f0e1", OUT_OF_BAND_CONFIRMED_KEY: true});
        let clean = sanitize_out_of_band(dirty);
        assert!(clean.get(OUT_OF_BAND_CONFIRMED_KEY).is_none());
        assert_eq!(clean.get("action").and_then(Value::as_str), Some("click"));
    }

    #[test]
    fn describe_pending_surfaces_action_detail_without_secrets() {
        assert!(describe_pending("navigate", &json!({"url": "https://shop.test/pay"})).contains("shop.test/pay"));
        assert!(describe_pending("click", &json!({"ref": "f0e9"})).contains("f0e9"));
        let d = describe_pending("type", &json!({"text": "secret:CARD"}));
        assert!(!d.contains("secret:CARD"), "preview must not echo a secret reference: {d}");
    }

    #[test]
    fn approval_required_value_mirrors_confirmation_shape() {
        let v = approval_required_value("browser_oob_123", "click", &json!({"ref": "f0e3"}));
        let ar = v.get("approval_required").expect("approval_required envelope");
        assert_eq!(ar.get("call_id").and_then(Value::as_str), Some("browser_oob_123"));
        let opts = ar.get("options").and_then(Value::as_array).expect("options");
        let values: Vec<&str> = opts.iter().filter_map(|o| o.get("value").and_then(Value::as_str)).collect();
        assert!(values.contains(&"proceed_once") && values.contains(&"cancel"));
    }

    #[test]
    fn gw2_gate_keeps_irreversible_prompt_for_default_context() {
        let registry = BrowserRegistry::default_for_browser_use();
        let key = "conversation:default";
        let input = json!({"action": "press_key", "keys": "Enter"});

        let ctx = CallerCtx::default();
        let result = gw2_gate(&ctx, &registry, key, "press_key", &input);

        assert!(
            result.and_then(|v| v.get("approval_required").cloned()).is_some(),
            "default gateway browser context should keep the explicit irreversible-action approval"
        );
    }

    #[test]
    fn gw2_gate_skips_irreversible_prompt_for_yolo_context() {
        let ctx = CallerCtx {
            session_mode: Some("yolo".to_owned()),
            ..Default::default()
        };
        let registry = BrowserRegistry::default_for_browser_use();
        let key = "conversation:yolo";
        let input = json!({"action": "press_key", "keys": "Enter"});

        let result = gw2_gate(&ctx, &registry, key, "press_key", &input);

        assert!(
            result.is_none(),
            "yolo gateway browser context should not return approval_required"
        );
    }

    #[test]
    fn act_flatten_captures_passthrough_params() {
        let p: ActParams = serde_json::from_value(json!({"action": "click", "ref": "f0e1", "text": "hi"})).unwrap();
        assert_eq!(p.action, "click");
        assert_eq!(p.rest.get("ref").and_then(Value::as_str), Some("f0e1"));
        assert_eq!(p.rest.get("text").and_then(Value::as_str), Some("hi"));
        assert!(!p.rest.contains_key("action"), "flatten must exclude the named action field");
    }
}

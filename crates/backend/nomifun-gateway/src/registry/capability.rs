//! Capability descriptor: the single source of truth for one operable platform
//! capability — its MCP tool name, LLM-facing description, JSON Schema, danger
//! tier, per-surface permission policy, and async handler.
//!
//! The design rule that kills the historical "three definitions per tool" drift
//! (schemars Param struct in the bridge ↔ hand-parser in `tools_*.rs` ↔ service
//! request type): a capability owns ONE typed `Request` struct `P`. Its JSON
//! Schema is generated from `P` (`schemars`), its runtime arguments are
//! deserialized into the SAME `P`, and the handler receives a typed `P`. Schema,
//! validation, and execution can no longer disagree.
//!
//! The registry is **deps-free**: a handler receives `Arc<GatewayDeps>` as an
//! argument at dispatch time, so `Registry::build()` constructs no services. The
//! identical registry therefore serves both processes — the in-process server
//! (which dispatches with real deps) and the `mcp-gateway-stdio` bridge (which
//! only reads `tool_specs()` to answer `tools/list`).

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use schemars::JsonSchema;
use serde::de::DeserializeOwned;
use serde_json::{Map, Value, json};

use crate::deps::{CallerCtx, GatewayDeps};

/// Boxed handler future. `Value` is the tool result (the `{"result": …}` /
/// `{"error": …}` envelope the existing tools already produce).
pub type BoxFut = Pin<Box<dyn Future<Output = Value> + Send>>;

/// Type-erased capability handler: `(deps, caller, raw_args) -> result`.
pub type Handler = Arc<dyn Fn(Arc<GatewayDeps>, CallerCtx, Value) -> BoxFut + Send + Sync>;

/// A streaming capability emits intermediate progress `Value`s through this sink
/// while it runs (e.g. a delegated agent's text/tool-call deltas), then returns
/// its final `Value`. Adapters that don't stream (MCP `tools/call`, REST
/// `/v1/tools/{name}`, CLI) just run the buffered [`Handler`] instead and get
/// the final value only — so adding a streaming variant never breaks them.
pub type ProgressSink = tokio::sync::mpsc::Sender<Value>;

/// Type-erased streaming handler: like [`Handler`] but also handed a
/// [`ProgressSink`] for incremental output. Returns the final `Value`.
pub type StreamingHandler =
    Arc<dyn Fn(Arc<GatewayDeps>, CallerCtx, Value, ProgressSink) -> BoxFut + Send + Sync>;

/// How dangerous an operation is. Drives the default per-surface permission
/// decision (see [`default_decision`]). Promoted from IDMM's regex-on-command
/// heuristic to a first-class, per-capability annotation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DangerTier {
    /// No side effects, no secrets. Always allowed.
    Read,
    /// Creates / modifies state, reversible.
    Write,
    /// Irreversible deletion / reset.
    Destructive,
    /// Reads or writes secrets / credentials.
    Sensitive,
}

/// Which kind of session is calling. Derived from [`CallerCtx`]: a channel
/// platform marks an external IM session; otherwise it is a local desktop
/// session. `Remote` is reserved for future LAN/web/device sessions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Surface {
    /// Local desktop session (companion thread or a plain local conversation).
    Desktop,
    /// External IM channel master-agent session (telegram / lark / …).
    Channel,
    /// Future: remote LAN / web / external-device session.
    Remote,
}

impl CallerCtx {
    /// The permission surface this caller acts on.
    pub fn surface(&self) -> Surface {
        if self.remote {
            Surface::Remote
        } else if self.channel_platform.is_some() {
            Surface::Channel
        } else {
            Surface::Desktop
        }
    }
}

/// The pre-dispatch gate outcome.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    /// Execute the handler.
    Allow,
    /// Refuse until the agent restates the action and re-calls with `confirm=true`.
    Confirm,
    /// Hard-refuse on this surface regardless of confirmation.
    Deny,
}

/// The default decision for a `(surface, danger)` pair — the policy matrix from
/// the design spec §4. Capability-level `deny_on` / `confirm_on` overrides
/// refine this in [`decide`].
///
/// | Surface | Read | Write | Destructive | Sensitive |
/// |---------|------|-------|-------------|-----------|
/// | Desktop | Allow | Allow | Confirm | Confirm |
/// | Channel | Allow | Allow | Deny  | Deny  |
/// | Remote  | Allow | Allow | Confirm | Deny  |
pub fn default_decision(surface: Surface, danger: DangerTier) -> Decision {
    use DangerTier::*;
    use Surface::*;
    match (surface, danger) {
        (_, Read) | (_, Write) => Decision::Allow,
        (Desktop, Destructive) | (Desktop, Sensitive) => Decision::Confirm,
        (Channel, Destructive) | (Channel, Sensitive) => Decision::Deny,
        (Remote, Destructive) => Decision::Confirm,
        (Remote, Sensitive) => Decision::Deny,
    }
}

/// Resolve the final gate decision for a capability on a surface, honoring the
/// capability's explicit `deny_on` / `confirm_on` overrides and whether the
/// caller already passed `confirm=true`.
pub fn decide(meta: &CapabilityMeta, surface: Surface, confirmed: bool) -> Decision {
    if meta.deny_on.contains(&surface) {
        return Decision::Deny;
    }
    let base = default_decision(surface, meta.danger);
    if base == Decision::Deny {
        return Decision::Deny;
    }
    let needs_confirm = base == Decision::Confirm || meta.confirm_on.contains(&surface);
    if needs_confirm && !confirmed {
        Decision::Confirm
    } else {
        Decision::Allow
    }
}

/// Static metadata for one capability. All `&'static` so the registry is cheap
/// to build and the bridge can list tools with zero allocation beyond the schema.
pub struct CapabilityMeta {
    /// MCP tool name. Convention: `nomi_<domain>_<verb_object>`, lower_snake,
    /// kept concise. The fully-namespaced wire name is `mcp__nomifun-desktop__<name>`
    /// (22-char prefix); Anthropic caps that at 64 chars, so the tool name has a
    /// 42-char hard budget. The registry self-test enforces both the hard limit
    /// and a tighter style budget so names cannot creep toward the ceiling.
    pub name: &'static str,
    /// Coarse domain label (for diagnostics / grouping).
    pub domain: &'static str,
    /// LLM-facing one-line description.
    pub summary: &'static str,
    /// Danger tier — drives the default permission decision.
    pub danger: DangerTier,
    /// Surfaces where this capability is hard-denied regardless of confirmation
    /// (escape hatch beyond the danger matrix, e.g. a `Write` too risky for IM).
    pub deny_on: &'static [Surface],
    /// Surfaces where this capability additionally requires confirmation
    /// (escape hatch to force confirm on an otherwise-allowed surface).
    pub confirm_on: &'static [Surface],
}

impl CapabilityMeta {
    /// Construct metadata with no surface overrides (the danger matrix applies as-is).
    pub const fn new(
        name: &'static str,
        domain: &'static str,
        summary: &'static str,
        danger: DangerTier,
    ) -> Self {
        Self {
            name,
            domain,
            summary,
            danger,
            deny_on: &[],
            confirm_on: &[],
        }
    }

    /// Hard-deny this capability on the given surfaces (beyond the danger matrix).
    pub const fn deny_on(mut self, surfaces: &'static [Surface]) -> Self {
        self.deny_on = surfaces;
        self
    }

    /// Force confirmation for this capability on the given surfaces.
    pub const fn confirm_on(mut self, surfaces: &'static [Surface]) -> Self {
        self.confirm_on = surfaces;
        self
    }

    /// Whether this capability can require a `confirm=true` on ANY surface — used
    /// to decide whether to inject the `confirm` property into its schema.
    fn confirmable(&self) -> bool {
        matches!(self.danger, DangerTier::Destructive | DangerTier::Sensitive)
            || !self.confirm_on.is_empty()
    }
}

/// One operable capability: metadata + generated schema + typed handler.
pub struct Capability {
    pub meta: CapabilityMeta,
    /// JSON Schema object for the tool's arguments (MCP `inputSchema`).
    pub input_schema: Map<String, Value>,
    pub handler: Handler,
    /// Optional streaming handler. `Some` for capabilities that can emit
    /// incremental progress (e.g. `nomi_agent_run`); consumed by
    /// [`Registry::dispatch_stream`]. The buffered [`handler`](Self::handler) is
    /// always present, so non-streaming adapters are unaffected.
    pub stream: Option<StreamingHandler>,
}

impl Capability {
    /// Build a capability from a typed request `P` and an async handler.
    ///
    /// `P` is the single source: its `JsonSchema` becomes the MCP `inputSchema`,
    /// and incoming arguments are deserialized into `P` before the handler runs.
    /// A deserialization failure returns a structured `{"error": …}` the agent
    /// can self-correct from — it never reaches the handler.
    pub fn new<P, F, Fut>(meta: CapabilityMeta, f: F) -> Self
    where
        P: DeserializeOwned + JsonSchema + Send + 'static,
        F: Fn(Arc<GatewayDeps>, CallerCtx, P) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Value> + Send + 'static,
    {
        let mut input_schema = schema_for_params::<P>();
        if meta.confirmable() {
            inject_confirm_property(&mut input_schema);
        }
        let f = Arc::new(f);
        let schema = Value::Object(input_schema.clone());
        let handler: Handler = Arc::new(move |deps, ctx, args: Value| {
            let f = f.clone();
            let schema = schema.clone();
            Box::pin(async move {
                // `confirm` is a cross-cutting gate field injected into the schema,
                // not part of `P`; drop it before typed deserialization so an
                // `deny_unknown_fields` request type would not choke on it.
                let args = strip_confirm(coerce_args_to_schema(&schema, args));
                match serde_json::from_value::<P>(args) {
                    Ok(p) => f(deps, ctx, p).await,
                    Err(e) => json!({ "error": format!("invalid arguments for this tool: {e}") }),
                }
            })
        });
        Capability {
            meta,
            input_schema,
            handler,
            stream: None,
        }
    }

    /// Build a STREAMING capability: `f` receives a [`ProgressSink`] for
    /// incremental output and returns the final `Value`. A buffered
    /// [`Handler`](Self::handler) is synthesized automatically (it runs `f` with
    /// a draining sink and returns only the final value), so MCP `tools/call`,
    /// REST `/v1/tools/{name}`, and CLI keep working unchanged; streaming
    /// adapters use [`Registry::dispatch_stream`] to see the progress events.
    pub fn new_streaming<P, F, Fut>(meta: CapabilityMeta, f: F) -> Self
    where
        P: DeserializeOwned + JsonSchema + Send + 'static,
        F: Fn(Arc<GatewayDeps>, CallerCtx, P, ProgressSink) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Value> + Send + 'static,
    {
        let mut input_schema = schema_for_params::<P>();
        if meta.confirmable() {
            inject_confirm_property(&mut input_schema);
        }
        let f = Arc::new(f);
        let schema = Value::Object(input_schema.clone());

        // Streaming path: deserialize P, run f feeding the caller's sink.
        let stream_f = f.clone();
        let stream_schema = schema.clone();
        let stream: StreamingHandler =
            Arc::new(move |deps, ctx, args: Value, sink: ProgressSink| {
                let f = stream_f.clone();
                let schema = stream_schema.clone();
                Box::pin(async move {
                    let args = strip_confirm(coerce_args_to_schema(&schema, args));
                    match serde_json::from_value::<P>(args) {
                        Ok(p) => f(deps, ctx, p, sink).await,
                        Err(e) => {
                            json!({ "error": format!("invalid arguments for this tool: {e}") })
                        }
                    }
                })
            });

        // Buffered path: run the same handler with a sink whose receiver is
        // drained-and-discarded, returning only the final value.
        let handler_schema = schema.clone();
        let handler: Handler = Arc::new(move |deps, ctx, args: Value| {
            let f = f.clone();
            let schema = handler_schema.clone();
            Box::pin(async move {
                let args = strip_confirm(coerce_args_to_schema(&schema, args));
                let p = match serde_json::from_value::<P>(args) {
                    Ok(p) => p,
                    Err(e) => {
                        return json!({ "error": format!("invalid arguments for this tool: {e}") });
                    }
                };
                let (tx, mut rx) = tokio::sync::mpsc::channel::<Value>(64);
                let drain = tokio::spawn(async move { while rx.recv().await.is_some() {} });
                let result = f(deps, ctx, p, tx).await;
                drain.abort();
                result
            })
        });

        Capability {
            meta,
            input_schema,
            handler,
            stream: Some(stream),
        }
    }
}

/// Generate the MCP-facing JSON Schema object for a request type `P`, stripped
/// of the meta keys schemars adds (`$schema`, `title`) that MCP clients ignore.
fn schema_for_params<P: JsonSchema>() -> Map<String, Value> {
    let schema = schemars::schema_for!(P);
    let value = serde_json::to_value(schema).unwrap_or_else(|_| json!({ "type": "object" }));
    let mut map = match value {
        Value::Object(m) => m,
        _ => Map::new(),
    };
    map.remove("$schema");
    map.remove("title");
    map.entry("type").or_insert_with(|| json!("object"));
    // Tools with no fields still need a `properties` object so clients render an
    // empty-args form rather than rejecting the schema.
    map.entry("properties").or_insert_with(|| json!({}));
    map
}

/// Add the cross-cutting `confirm` argument to a confirm-gated tool's schema so
/// the LLM can discover it.
fn inject_confirm_property(schema: &mut Map<String, Value>) {
    let props = schema.entry("properties").or_insert_with(|| json!({}));
    if let Some(obj) = props.as_object_mut() {
        obj.insert(
            "confirm".into(),
            json!({
                "type": "boolean",
                "description": "Set true ONLY after restating the exact destructive/sensitive action and its target to the user and getting explicit agreement. Required to execute confirm-gated actions."
            }),
        );
    }
}

/// Remove the gate-only `confirm` key before typed deserialization.
fn strip_confirm(mut args: Value) -> Value {
    if let Value::Object(ref mut m) = args {
        m.remove("confirm");
    }
    args
}

pub(crate) fn coerce_args_to_schema(schema: &Value, args: Value) -> Value {
    let mut args = match args {
        Value::String(ref s) => match serde_json::from_str::<Value>(s) {
            Ok(parsed @ Value::Object(_)) => parsed,
            _ => return args,
        },
        other => other,
    };

    let Some(props) = schema.get("properties").and_then(Value::as_object) else {
        return args;
    };
    let Some(obj) = args.as_object_mut() else {
        return args;
    };

    for (key, prop_schema) in props {
        let expected = schema_type_names(prop_schema);
        if expected.iter().any(|t| *t == "string") {
            continue;
        }
        let Some(s) = obj.get(key).and_then(Value::as_str).map(str::to_owned) else {
            continue;
        };
        if let Some(coerced) = coerce_string_to_types(&s, &expected) {
            obj.insert(key.clone(), coerced);
        }
    }

    args
}

fn schema_type_names(schema: &Value) -> Vec<&str> {
    let mut types = Vec::new();
    collect_schema_type_names(schema, &mut types);
    types
}

fn collect_schema_type_names<'a>(schema: &'a Value, out: &mut Vec<&'a str>) {
    match schema.get("type") {
        Some(Value::String(s)) => out.push(s.as_str()),
        Some(Value::Array(items)) => {
            for item in items {
                if let Some(s) = item.as_str() {
                    out.push(s);
                }
            }
        }
        _ => {}
    }
    for key in ["oneOf", "anyOf"] {
        if let Some(items) = schema.get(key).and_then(Value::as_array) {
            for item in items {
                collect_schema_type_names(item, out);
            }
        }
    }
}

fn coerce_string_to_types(s: &str, expected: &[&str]) -> Option<Value> {
    if expected.iter().any(|t| *t == "array" || *t == "object") {
        if let Ok(parsed) = serde_json::from_str::<Value>(s) {
            if (expected.contains(&"array") && parsed.is_array())
                || (expected.contains(&"object") && parsed.is_object())
            {
                return Some(parsed);
            }
        }
    }

    let trimmed = s.trim();
    if trimmed.is_empty() {
        return None;
    }
    if expected.contains(&"integer") {
        if let Ok(n) = trimmed.parse::<i64>() {
            return Some(Value::Number(n.into()));
        }
    }
    if expected.contains(&"number") {
        if let Ok(n) = trimmed.parse::<f64>() {
            return serde_json::Number::from_f64(n).map(Value::Number);
        }
    }
    if expected.contains(&"boolean") {
        if trimmed.eq_ignore_ascii_case("true") {
            return Some(Value::Bool(true));
        }
        if trimmed.eq_ignore_ascii_case("false") {
            return Some(Value::Bool(false));
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::Deserialize;

    #[test]
    fn matrix_allows_reads_and_writes_everywhere() {
        for s in [Surface::Desktop, Surface::Channel, Surface::Remote] {
            assert_eq!(default_decision(s, DangerTier::Read), Decision::Allow);
            assert_eq!(default_decision(s, DangerTier::Write), Decision::Allow);
        }
    }

    #[test]
    fn matrix_gates_destructive_and_sensitive() {
        assert_eq!(
            default_decision(Surface::Desktop, DangerTier::Destructive),
            Decision::Confirm
        );
        assert_eq!(
            default_decision(Surface::Desktop, DangerTier::Sensitive),
            Decision::Confirm
        );
        assert_eq!(
            default_decision(Surface::Channel, DangerTier::Destructive),
            Decision::Deny
        );
        assert_eq!(
            default_decision(Surface::Channel, DangerTier::Sensitive),
            Decision::Deny
        );
        assert_eq!(
            default_decision(Surface::Remote, DangerTier::Destructive),
            Decision::Confirm
        );
        assert_eq!(
            default_decision(Surface::Remote, DangerTier::Sensitive),
            Decision::Deny
        );
    }

    #[test]
    fn remote_caller_resolves_remote_surface() {
        // The Remote front door sets `remote: true`; surface() must yield Remote.
        let ctx = CallerCtx {
            remote: true,
            ..Default::default()
        };
        assert_eq!(ctx.surface(), Surface::Remote);
        // `remote` takes precedence over a stray channel_platform value.
        let ctx2 = CallerCtx {
            remote: true,
            channel_platform: Some("lark".into()),
            ..Default::default()
        };
        assert_eq!(ctx2.surface(), Surface::Remote);
        // Without the marker, behaviour is unchanged (desktop / channel).
        assert_eq!(CallerCtx::default().surface(), Surface::Desktop);
        assert_eq!(
            CallerCtx {
                channel_platform: Some("lark".into()),
                ..Default::default()
            }
            .surface(),
            Surface::Channel
        );
    }

    const META_DESTRUCTIVE: CapabilityMeta = CapabilityMeta {
        name: "t_del",
        domain: "test",
        summary: "delete a thing",
        danger: DangerTier::Destructive,
        deny_on: &[],
        confirm_on: &[],
    };

    #[test]
    fn destructive_needs_confirm_on_desktop_until_confirmed() {
        assert_eq!(
            decide(&META_DESTRUCTIVE, Surface::Desktop, false),
            Decision::Confirm
        );
        assert_eq!(
            decide(&META_DESTRUCTIVE, Surface::Desktop, true),
            Decision::Allow
        );
        // External channels hard-deny destructive ops even with confirm=true.
        assert_eq!(
            decide(&META_DESTRUCTIVE, Surface::Channel, true),
            Decision::Deny
        );
    }

    const META_WRITE_DENY_CHANNEL: CapabilityMeta = CapabilityMeta {
        name: "t_write",
        domain: "test",
        summary: "write a thing",
        danger: DangerTier::Write,
        deny_on: &[Surface::Channel],
        confirm_on: &[],
    };

    #[test]
    fn deny_on_override_hard_denies_even_writes() {
        assert_eq!(
            decide(&META_WRITE_DENY_CHANNEL, Surface::Channel, true),
            Decision::Deny
        );
        assert_eq!(
            decide(&META_WRITE_DENY_CHANNEL, Surface::Desktop, false),
            Decision::Allow
        );
    }

    #[test]
    fn confirmable_drives_schema_injection() {
        assert!(META_DESTRUCTIVE.confirmable());
        assert!(!META_WRITE_DENY_CHANNEL.confirmable());
    }

    #[derive(Deserialize, JsonSchema)]
    struct NumericStringParams {
        id: i64,
        limit: Option<u32>,
        wait_secs: Option<f64>,
        confirm: Option<bool>,
    }

    #[test]
    fn schema_coercion_accepts_numeric_and_boolean_strings() {
        let schema = schema_for_params::<NumericStringParams>();
        let coerced = coerce_args_to_schema(
            &Value::Object(schema),
            json!({
                "id": "8",
                "limit": "50",
                "wait_secs": "1.5",
                "confirm": "true"
            }),
        );
        let parsed: NumericStringParams = serde_json::from_value(coerced).unwrap();
        assert_eq!(parsed.id, 8);
        assert_eq!(parsed.limit, Some(50));
        assert_eq!(parsed.wait_secs, Some(1.5));
        assert_eq!(parsed.confirm, Some(true));
    }
}

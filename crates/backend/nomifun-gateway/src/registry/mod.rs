//! The capability registry: a deps-free, compile-time-known collection of every
//! operable platform capability, keyed by MCP tool name.
//!
//! - The in-process [`crate::server`] dispatches tool calls through
//!   [`Registry::dispatch_opt`] (with real `GatewayDeps`).
//! - The `mcp-gateway-stdio` bridge answers `tools/list` from
//!   [`Registry::tool_specs`] (schema only, no deps).
//!
//! Unknown names return `None`; transports turn that into their native unknown-
//! tool response. There is no secondary dispatch table.

mod capability;

pub use capability::{
    AccessScope, Capability, CapabilityMeta, DangerTier, Decision, ProgressSink,
    StreamingHandler, Surface, decide, default_decision,
};

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, OnceLock};

use serde_json::{Map, Value, json};

use crate::deps::{CallerCtx, GatewayDeps};

/// Enforce the one gateway outcome protocol before a capability result reaches
/// any transport adapter. Keeping this at the shared dispatch boundary means
/// MCP, REST, stdio, and streaming calls cannot disagree about whether a
/// malformed handler response succeeded.
fn validate_dispatch_outcome(value: Value) -> Value {
    let Some(object) = value.as_object() else {
        return json!({
            "error": "invalid capability response envelope: expected a JSON object"
        });
    };

    let has_result = object.contains_key("result");
    let has_error = object.contains_key("error");
    let is_confirmation = object
        .get("needs_confirmation")
        .and_then(Value::as_bool)
        == Some(true);

    if is_confirmation && (has_result || has_error) {
        return json!({
            "error": "invalid capability response envelope: confirmation cannot be mixed with result or error"
        });
    }
    if has_result ^ has_error || is_confirmation {
        return value;
    }

    json!({
        "error": "invalid capability response envelope: expected exactly one of result or error"
    })
}

/// A tool advertised to MCP clients via `tools/list`.
pub struct ToolSpec {
    pub name: &'static str,
    pub domain: &'static str,
    pub description: &'static str,
    pub input_schema: Map<String, Value>,
}

/// The global capability set.
pub struct Registry {
    by_name: BTreeMap<&'static str, Capability>,
}

/// Collaboration tool names retired by the AgentExecution hard cut. Prefixes
/// are limited to the old collaboration namespaces; ordinary process spawning
/// and domain-scoped verbs such as `nomi_cron_run` remain legal.
const RETIRED_COLLABORATION_TOOL_PREFIXES: &[&str] = &[
    "nomi_team_",
    "nomi_orchestrator_", // vocabulary-guard: retired-name-deny
    "nomi_cluster_",
    "nomi_taskboard_", // vocabulary-guard: retired-name-deny
    "nomi_run_",
    "nomi_task_",
    "nomi_agent_run_",
    "nomi_agent_result_",
];

/// Exact legacy names that do not live under one of the retired namespaces.
/// Keep `spawn` exact: domain-qualified process spawning remains legal.
const RETIRED_COLLABORATION_TOOL_NAMES: &[&str] = &[
    "nomi_team",
    "nomi_orchestrator", // vocabulary-guard: retired-name-deny
    "nomi_cluster",
    "nomi_taskboard", // vocabulary-guard: retired-name-deny
    "nomi_spawn",
    "nomi_agent_run",
    "nomi_agent_result",
];

fn retired_collaboration_name_rule(name: &str) -> Option<&'static str> {
    if RETIRED_COLLABORATION_TOOL_NAMES.contains(&name) {
        return Some("exact retired name");
    }
    RETIRED_COLLABORATION_TOOL_PREFIXES
        .iter()
        .copied()
        .find(|prefix| name.starts_with(prefix))
}

/// Validate the complete registration batch before it becomes dispatchable.
/// A retired collaboration name or duplicate is a boot-time programmer error:
/// fail closed instead of silently advertising an old or shadowed capability.
fn validate_registered_tool_names<'a>(
    names: impl IntoIterator<Item = &'a str>,
) -> Result<(), String> {
    let mut seen = BTreeSet::new();
    for name in names {
        if let Some(rule) = retired_collaboration_name_rule(name) {
            return Err(format!(
                "retired collaboration capability name '{name}' matched {rule}"
            ));
        }
        if !seen.insert(name) {
            return Err(format!("duplicate gateway capability name: {name}"));
        }
    }
    Ok(())
}

/// Register one installation-scoped capability domain and stamp every tool in
/// that domain with the same owner boundary. Keeping the classification next
/// to the registry composition prevents individual tools from accidentally
/// omitting the gate as the domain grows.
fn register_instance_owner_domain(
    out: &mut Vec<Capability>,
    register: fn(&mut Vec<Capability>),
) {
    let first = out.len();
    register(out);
    for capability in &mut out[first..] {
        capability.meta.access_scope = AccessScope::InstanceOwner;
    }
}

impl Registry {
    /// The process-wide registry, built once. Construction allocates only the
    /// capability closures + their generated schemas — no services — so this is
    /// safe to call from the bridge process too.
    pub fn global() -> &'static Registry {
        static REG: OnceLock<Registry> = OnceLock::new();
        REG.get_or_init(Registry::build)
    }

    fn build() -> Registry {
        let mut caps: Vec<Capability> = Vec::new();

        // ── capability domains ───────────────────────────────────────────
        // NEW DOMAIN? Three steps (the `all_caps_modules_are_mod_declared_and_registered`
        // test fails CI if you miss 1–2; the compiler fails if you miss 4):
        //   1. create `caps_<domain>.rs` with `pub(crate) fn register(out: &mut Vec<Capability>)`
        //   2. add `mod caps_<domain>;` to lib.rs
        //   3. add the domain HERE through the direct or instance-owner path
        //   4. if it needs a NEW service: add a field to deps.rs::GatewayDeps and
        //      wire it in nomifun-app/src/router/routes.rs::inject_gateway_deps.
        // Adding a tool to an EXISTING domain is just one more `out.push(...)` — no wiring.
        register_instance_owner_domain(&mut caps, crate::caps_memory::register);
        register_instance_owner_domain(&mut caps, crate::caps_agent_execution::register);
        crate::caps_confirmation::register(&mut caps);
        crate::caps_conversation::register(&mut caps);
        register_instance_owner_domain(&mut caps, crate::caps_provider::register);
        crate::caps_cron::register(&mut caps);
        register_instance_owner_domain(&mut caps, crate::caps_requirement::register);
        register_instance_owner_domain(&mut caps, crate::caps_autowork::register);
        register_instance_owner_domain(&mut caps, crate::caps_idmm::register);
        register_instance_owner_domain(&mut caps, crate::caps_terminal::register);
        register_instance_owner_domain(&mut caps, crate::caps_knowledge::register);
        register_instance_owner_domain(&mut caps, crate::caps_knowledge_ext::register);
        register_instance_owner_domain(&mut caps, crate::caps_system::register);
        register_instance_owner_domain(&mut caps, crate::caps_companion::register);
        register_instance_owner_domain(&mut caps, crate::caps_channel::register);
        crate::caps_scheduling_ext::register(&mut caps);
        register_instance_owner_domain(&mut caps, crate::caps_terminal_ext::register);
        register_instance_owner_domain(&mut caps, crate::caps_files::register);
        register_instance_owner_domain(&mut caps, crate::caps_mcp::register);
        register_instance_owner_domain(&mut caps, crate::caps_agent::register);
        register_instance_owner_domain(&mut caps, crate::caps_workshop::register);
        #[cfg(feature = "browser-use")]
        register_instance_owner_domain(&mut caps, crate::caps_browser::register);
        #[cfg(feature = "computer-use")]
        register_instance_owner_domain(&mut caps, crate::caps_computer::register);

        validate_registered_tool_names(caps.iter().map(|capability| capability.meta.name))
            .unwrap_or_else(|error| panic!("gateway capability registration rejected: {error}"));
        let by_name = caps
            .into_iter()
            .map(|capability| (capability.meta.name, capability))
            .collect();
        Registry { by_name }
    }

    /// Whether a tool name is handled by the registry (migration check).
    pub fn contains(&self, name: &str) -> bool {
        self.by_name.contains_key(name)
    }

    /// Total registered capabilities.
    pub fn len(&self) -> usize {
        self.by_name.len()
    }

    pub fn is_empty(&self) -> bool {
        self.by_name.is_empty()
    }

    /// The tools visible on a surface: everything except the hard-denied set.
    /// Confirm-gated tools ARE listed (they are usable with `confirm=true`);
    /// passing `confirmed = true` to [`decide`] collapses `Confirm → Allow`, so
    /// only `Deny` outcomes are filtered out.
    pub fn tool_specs(&self, surface: Surface) -> Vec<ToolSpec> {
        self.tool_specs_for_caller(surface, None, true)
    }

    /// Project the catalog for one authenticated caller. `domains` is an
    /// optional capability-domain allow-list; owner-only declarations are
    /// omitted unless the caller is the canonical installation owner. Runtime
    /// dispatch independently enforces the same scope.
    pub fn tool_specs_for_caller(
        &self,
        surface: Surface,
        domains: Option<&[&str]>,
        is_instance_owner: bool,
    ) -> Vec<ToolSpec> {
        self.by_name
            .values()
            .filter(|c| domains.is_none_or(|domains| domains.contains(&c.meta.domain)))
            .filter(|c| {
                c.meta.access_scope != AccessScope::InstanceOwner || is_instance_owner
            })
            .filter(|c| decide(&c.meta, surface, true) != Decision::Deny)
            .map(|c| ToolSpec {
                name: c.meta.name,
                domain: c.meta.domain,
                description: c.meta.summary,
                input_schema: c.input_schema.clone(),
            })
            .collect()
    }

    /// Like [`tool_specs`](Self::tool_specs) but restricted to the given
    /// capability domains (`CapabilityMeta::domain`). Powers curated external
    /// "profiles" (e.g. an `agent` profile = do-work domains only), so a remote
    /// MCP client gets a tight, intent-focused tool list instead of all ~150.
    /// An empty `domains` slice yields an empty result (callers pass the full
    /// set or use [`tool_specs`](Self::tool_specs) for "everything").
    pub fn tool_specs_for(&self, surface: Surface, domains: &[&str]) -> Vec<ToolSpec> {
        self.tool_specs_for_caller(surface, Some(domains), true)
    }

    pub fn tool_visible(&self, surface: Surface, name: &str) -> bool {
        self.tool_visible_for_caller(surface, None, true, name)
    }

    pub fn tool_visible_for(&self, surface: Surface, domains: &[&str], name: &str) -> bool {
        self.tool_visible_for_caller(surface, Some(domains), true, name)
    }

    pub fn tool_visible_for_caller(
        &self,
        surface: Surface,
        domains: Option<&[&str]>,
        is_instance_owner: bool,
        name: &str,
    ) -> bool {
        self.by_name.get(name).is_some_and(|c| {
            domains.is_none_or(|domains| domains.contains(&c.meta.domain))
                && (c.meta.access_scope != AccessScope::InstanceOwner || is_instance_owner)
                && decide(&c.meta, surface, true) != Decision::Deny
        })
    }

    /// Dispatch a tool call if the registry owns the tool; `None` means "not a
    /// registered tool.
    pub async fn dispatch_opt(
        &self,
        deps: Arc<GatewayDeps>,
        ctx: CallerCtx,
        name: &str,
        args: &Value,
    ) -> Option<Value> {
        let cap = self.by_name.get(name)?;
        if cap.meta.access_scope == AccessScope::InstanceOwner
            && ctx.user_id.as_str() != deps.authoritative_user_id.as_ref()
        {
            return Some(validate_dispatch_outcome(json!({
                "error": "installation_owner_required",
                "tool": name,
            })));
        }
        let surface = ctx.surface();
        let confirmed = args
            .get("confirm")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let result = match decide(&cap.meta, surface, confirmed) {
            Decision::Deny => json!({
                "error": format!("'{name}' is not permitted on the {surface:?} surface")
            }),
            Decision::Confirm => json!({
                "needs_confirmation": true,
                "tool": name,
                "danger": format!("{:?}", cap.meta.danger),
                "note": "This action is destructive or sensitive. Restate the exact action and its target to the user, get explicit agreement, then call again with confirm=true."
            }),
            Decision::Allow => (cap.handler)(deps, ctx, args.clone()).await,
        };
        Some(validate_dispatch_outcome(result))
    }

    /// Streaming dispatch: like [`dispatch_opt`](Self::dispatch_opt) but a
    /// streaming-capable tool emits intermediate progress through `progress`
    /// while it runs, and the returned `Value` is the final result. A
    /// non-streaming tool emits nothing on `progress` and returns its single
    /// value (so the streaming endpoint works uniformly for every tool).
    /// `None` means the tool name is unknown.
    pub async fn dispatch_stream(
        &self,
        deps: Arc<GatewayDeps>,
        ctx: CallerCtx,
        name: &str,
        args: &Value,
        progress: ProgressSink,
    ) -> Option<Value> {
        let cap = self.by_name.get(name)?;
        if cap.meta.access_scope == AccessScope::InstanceOwner
            && ctx.user_id.as_str() != deps.authoritative_user_id.as_ref()
        {
            return Some(validate_dispatch_outcome(json!({
                "error": "installation_owner_required",
                "tool": name,
            })));
        }
        let surface = ctx.surface();
        let confirmed = args
            .get("confirm")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let result = match decide(&cap.meta, surface, confirmed) {
            Decision::Deny => json!({
                "error": format!("'{name}' is not permitted on the {surface:?} surface")
            }),
            Decision::Confirm => json!({
                "needs_confirmation": true,
                "tool": name,
                "danger": format!("{:?}", cap.meta.danger),
                "note": "This action is destructive or sensitive. Restate the exact action and its target to the user, get explicit agreement, then call again with confirm=true."
            }),
            Decision::Allow => match &cap.stream {
                Some(stream) => stream(deps, ctx, args.clone(), progress).await,
                None => (cap.handler)(deps, ctx, args.clone()).await,
            },
        };
        Some(validate_dispatch_outcome(result))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nomifun_api_types::GatewayMcpConfig;

    #[test]
    fn dispatch_outcome_protocol_is_fail_closed() {
        for valid in [
            json!({"result": null}),
            json!({"error": "failed"}),
            json!({"needs_confirmation": true, "tool": "nomi_delete"}),
        ] {
            assert_eq!(validate_dispatch_outcome(valid.clone()), valid);
        }

        for invalid in [
            json!(null),
            json!(["bare"]),
            json!({"ok": true}),
            json!({"result": "ok", "error": "failed"}),
            json!({"result": "ok", "needs_confirmation": true}),
            json!({"error": "failed", "needs_confirmation": true}),
        ] {
            let outcome = validate_dispatch_outcome(invalid);
            assert!(outcome.get("error").is_some());
            assert!(outcome.get("result").is_none());
        }

        let nested_domain_error = json!({"result": {"error": "ordinary domain data"}});
        assert_eq!(
            validate_dispatch_outcome(nested_domain_error.clone()),
            nested_domain_error
        );
    }

    /// Boot-time invariants for every registered capability: unique names
    /// (panics in `build` otherwise), `nomi_`-prefixed, a non-empty summary, a
    /// well-formed object schema, and a fully-namespaced MCP wire name within
    /// the Anthropic 64-char limit — with a tighter style budget on the tool
    /// name itself so length cannot creep up to the hard ceiling unnoticed.
    #[test]
    fn registry_builds_and_names_fit_mcp_limit() {
        let reg = Registry::global();
        // Wire name = `mcp__<server>__<tool>`; derive the prefix from the REAL
        // server-name constant so a rename can never silently invalidate this.
        let prefix = format!("mcp__{}__", GatewayMcpConfig::SERVER_NAME).len();
        // Hard ceiling Anthropic enforces on the wire name.
        const HARD_WIRE_LIMIT: usize = 64;
        // Style budget for the tool name alone (see CapabilityMeta::name doc):
        // keeps a comfortable margin under the ceiling as domains grow.
        const TOOL_NAME_BUDGET: usize = 42;

        for (name, cap) in reg.by_name.iter() {
            assert!(
                name.starts_with("nomi_"),
                "gateway tool names are nomi_-prefixed: {name}"
            );
            assert!(
                prefix + name.len() <= HARD_WIRE_LIMIT,
                "tool name breaks the MCP 64-char wire limit: {name} ({prefix} + {} > {HARD_WIRE_LIMIT})",
                name.len()
            );
            assert!(
                name.len() <= TOOL_NAME_BUDGET,
                "tool name exceeds the {TOOL_NAME_BUDGET}-char style budget (keep `nomi_<domain>_<verb_object>` concise): {name} ({} chars)",
                name.len()
            );
            assert!(
                !cap.meta.summary.trim().is_empty(),
                "capability {name} has an empty summary (LLMs need it)"
            );
            assert!(
                cap.input_schema.contains_key("properties"),
                "capability {name} schema missing `properties` (MCP/OpenAI clients reject such schemas)"
            );
        }
    }

    /// Floor on the registered-capability count. A drop below this almost always
    /// means a `caps_*` module's `register()` call was accidentally removed from
    /// `build()` (or a domain module deleted). Bump the floor when capabilities
    /// are intentionally removed. Default build (no `browser-use`) sits just
    /// below the feature-on count, so the floor allows for the gated module.
    #[test]
    fn registry_capability_count_floor() {
        let n = Registry::global().len();
        assert!(
            n >= 135,
            "capability count fell to {n} (floor 135) — a caps_* module may have lost its \
             register() call in Registry::build(), or a domain was removed. If intentional, lower the floor."
        );
    }

    #[test]
    fn gateway_surfaces_do_not_advertise_team_tools() {
        let reg = Registry::global();
        for surface in [Surface::Desktop, Surface::Remote, Surface::Channel] {
            let team_tools: Vec<&str> = reg
                .tool_specs(surface)
                .iter()
                .map(|s| s.name)
                .filter(|name| name.starts_with("nomi_team_"))
                .collect();
            assert!(
                team_tools.is_empty(),
                "team tools must not be advertised on {surface:?}: {team_tools:?}"
            );
        }
    }

    #[test]
    fn retired_collaboration_name_guard_covers_every_legacy_family() {
        for name in [
            "nomi_team_send_message",
            "nomi_orchestrator_plan", // vocabulary-guard: retired-name-deny-fixture
            "nomi_cluster_create",
            "nomi_taskboard_update", // vocabulary-guard: retired-name-deny-fixture
            "nomi_team",
            "nomi_orchestrator", // vocabulary-guard: retired-name-deny-fixture
            "nomi_cluster",
            "nomi_taskboard", // vocabulary-guard: retired-name-deny-fixture
            "nomi_spawn",
            "nomi_agent_run",
            "nomi_agent_result",
            "nomi_agent_run_v2",
            "nomi_agent_result_status",
            "nomi_run_create",
            "nomi_run_status",
            "nomi_run_result",
            "nomi_run_adjust",
            "nomi_run_add_tasks",
            "nomi_run_cancel",
            "nomi_run_resume",
            "nomi_task_rerun",
            "nomi_task_config",
            "nomi_task_question",
            "nomi_task_create",
        ] {
            let error = validate_registered_tool_names([name])
                .expect_err("retired collaboration name must fail closed");
            assert!(
                error.contains(name),
                "registration error must identify rejected name: {error}"
            );
        }
    }

    #[test]
    fn collaboration_name_guard_does_not_reject_similar_legal_tools() {
        validate_registered_tool_names([
            "nomi_delegate",
            "nomi_process_spawn",
            "nomi_spawn_process",
            "nomi_cron_run",
            "nomi_requirement_list",
            "nomi_teamwork_summarize",
            "nomi_clustered_search",
        ])
        .expect("precise retired-name rules must not reject unrelated capabilities");
    }

    #[test]
    fn registration_rejects_duplicate_capability_names() {
        let error = validate_registered_tool_names(["nomi_example_read", "nomi_example_read"])
            .expect_err("duplicate registration must fail closed");
        assert_eq!(
            error,
            "duplicate gateway capability name: nomi_example_read"
        );
    }

    #[test]
    fn ownership_scope_matrix_is_explicit_and_stable() {
        let reg = Registry::global();
        let scope = |name: &str| {
            reg.by_name
                .get(name)
                .unwrap_or_else(|| panic!("missing capability {name}"))
                .meta
                .access_scope
        };

        // User-owned aggregates keep their own repository/service owner checks.
        assert_eq!(scope("nomi_list_conversations"), AccessScope::User);
        assert_eq!(scope("nomi_cron_list"), AccessScope::User);

        // Installation-wide control planes are rejected centrally before their
        // handlers can observe or mutate shared state.
        assert_eq!(
            scope("nomi_system_get_settings"),
            AccessScope::InstanceOwner
        );
        assert_eq!(
            scope("nomi_requirement_list"),
            AccessScope::InstanceOwner
        );
        assert_eq!(
            scope("nomi_knowledge_list_bases"),
            AccessScope::InstanceOwner
        );
        assert_eq!(
            scope("nomi_companion_list"),
            AccessScope::InstanceOwner
        );
        assert_eq!(
            scope("nomi_channel_list_plugins"),
            AccessScope::InstanceOwner
        );
        assert_eq!(
            scope("nomi_idmm_get_settings"),
            AccessScope::InstanceOwner
        );
        assert_eq!(scope("nomi_delegate"), AccessScope::InstanceOwner);
        assert_eq!(scope("nomi_execution_get"), AccessScope::InstanceOwner);
        assert_eq!(scope("nomi_execution_update"), AccessScope::InstanceOwner);
        // Target-scoped IDMM data is user-owned and every handler verifies the
        // target owner; only its global settings remain installation-owned.
        assert_eq!(scope("nomi_idmm_get_log"), AccessScope::User);
        assert_eq!(scope("nomi_create_terminal"), AccessScope::InstanceOwner);
        assert_eq!(scope("nomi_terminal_get"), AccessScope::InstanceOwner);
        assert_eq!(scope("nomi_fs_read_file"), AccessScope::InstanceOwner);
    }

    #[test]
    fn secondary_caller_catalog_hides_owner_scope_without_hiding_user_scope() {
        let reg = Registry::global();
        let names: Vec<&str> = reg
            .tool_specs_for_caller(Surface::Desktop, None, false)
            .iter()
            .map(|spec| spec.name)
            .collect();
        assert!(names.contains(&"nomi_list_conversations"));
        assert!(names.contains(&"nomi_cron_list"));
        assert!(!names.contains(&"nomi_delegate"));
        assert!(!names.contains(&"nomi_system_get_settings"));
        assert!(!names.contains(&"nomi_requirement_list"));
        assert!(!names.contains(&"nomi_knowledge_list_bases"));
        assert!(!reg.tool_visible_for_caller(
            Surface::Desktop,
            None,
            false,
            "nomi_system_get_settings"
        ));
    }

    #[test]
    fn tool_specs_for_filters_to_domains() {
        let reg = Registry::global();
        let agentish = reg.tool_specs_for(
            Surface::Remote,
            &["agent_execution", "agent", "remote", "conversation"],
        );
        assert!(
            !agentish.is_empty(),
            "agent/conversation domains must expose tools"
        );
        // strict subset of the full Remote surface
        let all: std::collections::BTreeSet<&str> = reg
            .tool_specs(Surface::Remote)
            .iter()
            .map(|s| s.name)
            .collect();
        assert!(agentish.iter().all(|s| all.contains(s.name)));
        assert!(
            agentish.len() < all.len(),
            "a profile must be narrower than the full surface"
        );
        // contains the agent-delegation cap, excludes a system-management cap
        let names: Vec<&str> = agentish.iter().map(|s| s.name).collect();
        assert!(names.contains(&"nomi_delegate"));
        assert!(names.contains(&"nomi_remote_agent_list"));
        assert!(
            !names.contains(&"nomi_remote_agent_handshake"),
            "active saved-endpoint probes are hard-denied on the Remote surface"
        );
        assert!(
            !names.contains(&"nomi_system_update_settings"),
            "system domain must be excluded"
        );
        // unknown domain yields nothing
        assert!(
            reg.tool_specs_for(Surface::Remote, &["does_not_exist"])
                .is_empty()
        );
    }

    /// **Anti-drift guard (the structural fix for the historical ~10% coverage gap).**
    ///
    /// Every `caps_*.rs` file on disk MUST be both `mod`-declared in `lib.rs` and
    /// have its `register()` called in `Registry::build()`. A new domain file that
    /// forgets either step compiles silently and contributes ZERO tools with no
    /// other test failure — exactly the silent non-exposure that let coverage rot
    /// before. This test makes that mistake a hard CI failure. Pure source-text
    /// scanning (no proc-macro / inventory / linkme), so it also covers
    /// feature-gated modules whose `cfg` lines keep them out of a default build.
    #[test]
    fn all_caps_modules_are_mod_declared_and_registered() {
        use std::fs;
        use std::path::PathBuf;

        let src_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src");

        // 1. Ground truth: caps_*.rs files on disk.
        let mut on_disk: Vec<String> = fs::read_dir(&src_dir)
            .expect("read gateway src dir")
            .filter_map(|e| e.ok())
            .filter_map(|e| {
                let n = e.file_name().to_string_lossy().into_owned();
                (n.starts_with("caps_") && n.ends_with(".rs"))
                    .then(|| n.trim_end_matches(".rs").to_owned())
            })
            .collect();
        on_disk.sort();
        assert!(
            !on_disk.is_empty(),
            "no caps_*.rs files found — test misconfigured?"
        );

        // 2. `mod caps_*;` declarations in lib.rs (ignores the cfg line above them).
        let lib_rs = fs::read_to_string(src_dir.join("lib.rs")).expect("read lib.rs");
        let modded: Vec<String> = lib_rs
            .lines()
            .filter_map(|l| {
                let t = l.trim();
                t.strip_prefix("mod ")
                    .or_else(|| t.strip_prefix("pub mod "))
                    .and_then(|r| r.strip_suffix(';'))
                    .filter(|n| n.starts_with("caps_"))
                    .map(str::to_owned)
            })
            .collect();

        // 3. Capability register call sites in build(). Domains may register
        // directly or through `register_instance_owner_domain`; both are
        // first-class composition paths and must be recognized by this guard.
        let reg_rs =
            fs::read_to_string(src_dir.join("registry/mod.rs")).expect("read registry/mod.rs");
        let registered: Vec<String> = reg_rs
            .lines()
            .filter_map(|l| {
                let line = l.trim();
                let start = line.find("crate::caps_")? + "crate::".len();
                let remainder = &line[start..];
                let end = remainder.find("::register")?;
                let module = &remainder[..end];
                module.starts_with("caps_").then(|| module.to_owned())
            })
            .collect();

        let not_modded: Vec<&String> = on_disk.iter().filter(|f| !modded.contains(f)).collect();
        assert!(
            not_modded.is_empty(),
            "caps_*.rs on disk but NOT `mod`-declared in lib.rs (dead, never compiled): {not_modded:?} — add `mod <name>;`"
        );
        let not_registered: Vec<&String> =
            on_disk.iter().filter(|f| !registered.contains(f)).collect();
        assert!(
            not_registered.is_empty(),
            "caps_*.rs NOT registered in Registry::build(): {not_registered:?} — add it through the direct or instance-owner registration path (silently contributes ZERO tools otherwise)"
        );
    }
}

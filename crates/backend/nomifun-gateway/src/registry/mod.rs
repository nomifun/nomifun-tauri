//! The capability registry: a deps-free, compile-time-known collection of every
//! operable platform capability, keyed by MCP tool name.
//!
//! - The in-process [`crate::server`] dispatches tool calls through
//!   [`Registry::dispatch_opt`] (with real `GatewayDeps`).
//! - The `mcp-gateway-stdio` bridge answers `tools/list` from
//!   [`Registry::tool_specs`] (schema only, no deps).
//!
//! During migration the registry coexists with the legacy `tools_*.rs` dispatch
//! match: `dispatch_opt` returns `None` for any tool not yet registered, letting
//! the legacy match handle it. Once every tool is migrated the legacy match is
//! deleted and the bridge flips to listing `tool_specs()` dynamically.

mod capability;

pub use capability::{
    Capability, CapabilityMeta, DangerTier, Decision, ProgressSink, StreamingHandler, Surface,
    decide, default_decision,
};

use std::collections::BTreeMap;
use std::sync::{Arc, OnceLock};

use serde_json::{Map, Value, json};

use crate::deps::{CallerCtx, GatewayDeps};

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
        //   3. add `crate::caps_<domain>::register(&mut caps);` HERE
        //   4. if it needs a NEW service: add a field to deps.rs::GatewayDeps and
        //      wire it in nomifun-app/src/router/routes.rs::inject_gateway_deps.
        // Adding a tool to an EXISTING domain is just one more `out.push(...)` — no wiring.
        crate::caps_memory::register(&mut caps);
        crate::caps_orchestrator::register(&mut caps);
        crate::caps_confirmation::register(&mut caps);
        crate::caps_conversation::register(&mut caps);
        crate::caps_provider::register(&mut caps);
        crate::caps_cron::register(&mut caps);
        crate::caps_requirement::register(&mut caps);
        crate::caps_autowork::register(&mut caps);
        crate::caps_idmm::register(&mut caps);
        crate::caps_terminal::register(&mut caps);
        crate::caps_knowledge::register(&mut caps);
        crate::caps_knowledge_ext::register(&mut caps);
        crate::caps_system::register(&mut caps);
        crate::caps_companion::register(&mut caps);
        crate::caps_channel::register(&mut caps);
        crate::caps_scheduling_ext::register(&mut caps);
        crate::caps_terminal_ext::register(&mut caps);
        crate::caps_files::register(&mut caps);
        crate::caps_mcp::register(&mut caps);
        crate::caps_agent::register(&mut caps);
        #[cfg(feature = "browser-use")]
        crate::caps_browser::register(&mut caps);
        #[cfg(feature = "computer-use")]
        crate::caps_computer::register(&mut caps);

        // De-duplicate by name; a collision is a programmer error worth failing
        // fast on at first use (boot), not a silent last-writer-wins.
        let mut by_name = BTreeMap::new();
        for c in caps {
            let name = c.meta.name;
            if by_name.insert(name, c).is_some() {
                panic!("duplicate gateway capability name: {name}");
            }
        }
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
        self.by_name
            .values()
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
        self.by_name
            .values()
            .filter(|c| domains.contains(&c.meta.domain))
            .filter(|c| decide(&c.meta, surface, true) != Decision::Deny)
            .map(|c| ToolSpec {
                name: c.meta.name,
                domain: c.meta.domain,
                description: c.meta.summary,
                input_schema: c.input_schema.clone(),
            })
            .collect()
    }

    pub fn tool_visible(&self, surface: Surface, name: &str) -> bool {
        self.by_name
            .get(name)
            .is_some_and(|c| decide(&c.meta, surface, true) != Decision::Deny)
    }

    pub fn tool_visible_for(&self, surface: Surface, domains: &[&str], name: &str) -> bool {
        self.by_name.get(name).is_some_and(|c| {
            domains.contains(&c.meta.domain) && decide(&c.meta, surface, true) != Decision::Deny
        })
    }

    /// Dispatch a tool call if the registry owns the tool; `None` means "not a
    /// registry tool — let the legacy match handle it".
    pub async fn dispatch_opt(
        &self,
        deps: Arc<GatewayDeps>,
        ctx: CallerCtx,
        name: &str,
        args: &Value,
    ) -> Option<Value> {
        let cap = self.by_name.get(name)?;
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
        Some(result)
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
        Some(result)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nomifun_api_types::GatewayMcpConfig;

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
    fn tool_specs_for_filters_to_domains() {
        let reg = Registry::global();
        let agentish = reg.tool_specs_for(Surface::Remote, &["agent", "conversation"]);
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
        assert!(names.contains(&"nomi_agent_run"));
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

        // 3. `crate::caps_*::register(&mut caps);` call sites in build().
        let reg_rs =
            fs::read_to_string(src_dir.join("registry/mod.rs")).expect("read registry/mod.rs");
        let registered: Vec<String> = reg_rs
            .lines()
            .filter_map(|l| {
                l.trim()
                    .strip_prefix("crate::")
                    .and_then(|r| r.strip_suffix("::register(&mut caps);"))
                    .filter(|n| n.starts_with("caps_"))
                    .map(str::to_owned)
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
            "caps_*.rs NOT registered in Registry::build(): {not_registered:?} — add `crate::<name>::register(&mut caps);` (silently contributes ZERO tools otherwise)"
        );
    }
}

//! Agent-stack domain capabilities: agent catalog/health, custom agent CRUD,
//! remote agent management, and model failover configuration.
//!
//! Backed by:
//! - `nomifun_ai_agent::AgentService` — installed agent listing, health checks,
//!   custom agent CRUD, enable/disable.
//! - `nomifun_ai_agent::RemoteAgentService` — remote (A2A/MCP) agent CRUD +
//!   connection testing.
//! - `nomifun_conversation::model_failover` — global model-failover config read/write
//!   (stored in `client_preferences` key `agent.model_failover`).
//!
//! NEW GatewayDeps fields assumed (parent wires):
//! - `agent_service: Arc<nomifun_ai_agent::AgentService>`
//! - `remote_agent_service: Arc<nomifun_ai_agent::RemoteAgentService>`
//! - `client_pref_repo: Arc<dyn nomifun_db::IClientPreferenceRepository>`

use std::sync::Arc;

use nomifun_api_types::{
    CustomAgentUpsertRequest, ModelFailoverConfig, ProviderHealthCheckRequest,
    TestRemoteAgentConnectionRequest, TryConnectCustomAgentRequest,
};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::deps::GatewayDeps;
use crate::registry::{Capability, CapabilityMeta, DangerTier, Surface};
use crate::server::ok;

// ── param structs (single source: schema + runtime) ──────────────────────

/// List all installed agent backends with their status and metadata.
#[derive(Deserialize, JsonSchema)]
struct AgentListParams {}

/// Run an ACP health check against a specific agent backend.
#[derive(Deserialize, JsonSchema)]
struct AgentHealthCheckParams {
    /// The agent backend identifier to health-check (e.g. "claude", "codex").
    backend: String,
}

/// Run a provider-level health check (verify model reachability via a provider).
#[derive(Deserialize, JsonSchema)]
struct AgentProviderHealthCheckParams {
    /// Provider id to test against.
    provider_id: String,
    /// Model name to probe (must be enabled on the provider).
    model: String,
}

/// Enable or disable an agent backend.
#[derive(Deserialize, JsonSchema)]
struct AgentSetEnabledParams {
    /// Agent id to toggle.
    id: String,
    /// Whether to enable (true) or disable (false) the agent.
    enabled: bool,
}

/// Create a custom (user-registered) agent backend.
#[derive(Deserialize, JsonSchema)]
struct AgentCustomCreateParams {
    /// Display name for the custom agent.
    name: String,
    /// CLI command to launch the agent process (absolute path or PATH-resolvable).
    command: String,
    /// Optional icon URL or data URI.
    #[serde(default)]
    icon: Option<String>,
    /// Extra CLI arguments passed after `command`.
    #[serde(default)]
    args: Vec<String>,
    /// Environment variables injected into the agent process.
    #[serde(default)]
    env: Vec<AgentEnvEntryParam>,
    /// Advanced behavior overrides (yolo_id, native_skills_dirs, behavior_policy, description).
    #[serde(default)]
    advanced: Option<Value>,
}

/// Update an existing custom agent backend.
#[derive(Deserialize, JsonSchema)]
struct AgentCustomUpdateParams {
    /// The custom agent id to update.
    id: String,
    /// Display name for the custom agent.
    name: String,
    /// CLI command to launch the agent process.
    command: String,
    /// Optional icon URL or data URI.
    #[serde(default)]
    icon: Option<String>,
    /// Extra CLI arguments passed after `command`.
    #[serde(default)]
    args: Vec<String>,
    /// Environment variables injected into the agent process.
    #[serde(default)]
    env: Vec<AgentEnvEntryParam>,
    /// Advanced behavior overrides.
    #[serde(default)]
    advanced: Option<Value>,
}

/// Delete a custom agent backend (irreversible).
#[derive(Deserialize, JsonSchema)]
struct AgentCustomDeleteParams {
    /// The custom agent id to permanently delete.
    id: String,
}

/// Test connectivity to a custom agent binary (try-connect handshake).
#[derive(Deserialize, JsonSchema)]
struct AgentCustomTryConnectParams {
    /// CLI command to launch the agent process.
    command: String,
    /// ACP protocol arguments (if any).
    #[serde(default)]
    acp_args: Vec<String>,
    /// Environment variables for the test subprocess.
    #[serde(default)]
    env: std::collections::HashMap<String, String>,
}

/// An environment variable entry for custom agent configuration.
#[derive(Deserialize, JsonSchema, Clone)]
struct AgentEnvEntryParam {
    /// Variable name.
    name: String,
    /// Variable value.
    value: String,
    /// Optional human-readable description of what this variable controls.
    #[serde(default)]
    description: Option<String>,
}

// ── Remote agent param structs ──────────────────────────────────────────

/// List all registered remote agents.
#[derive(Deserialize, JsonSchema)]
struct RemoteAgentListParams {}

/// Get details of a single remote agent by id.
#[derive(Deserialize, JsonSchema)]
struct RemoteAgentGetParams {
    /// Remote agent id (numeric, as string for consistency).
    id: String,
}

/// Register a new remote agent.
#[derive(Deserialize, JsonSchema)]
struct RemoteAgentCreateParams {
    /// Display name.
    name: String,
    /// Protocol: "a2a" or "mcp-sse".
    protocol: String,
    /// Agent endpoint URL.
    url: String,
    /// Authentication type: "none", "bearer", or "header".
    auth_type: String,
    /// Auth token (required when auth_type is "bearer" or "header").
    #[serde(default)]
    auth_token: Option<String>,
    /// Allow connecting to HTTP (non-TLS) endpoints.
    #[serde(default)]
    allow_insecure: bool,
    /// Optional avatar URL.
    #[serde(default)]
    avatar: Option<String>,
    /// Optional description.
    #[serde(default)]
    description: Option<String>,
}

/// Update an existing remote agent (partial — only provided fields are changed).
#[derive(Deserialize, JsonSchema)]
struct RemoteAgentUpdateParams {
    /// Remote agent id to update.
    id: String,
    /// New display name.
    #[serde(default)]
    name: Option<String>,
    /// New protocol.
    #[serde(default)]
    protocol: Option<String>,
    /// New endpoint URL.
    #[serde(default)]
    url: Option<String>,
    /// New auth type.
    #[serde(default)]
    auth_type: Option<String>,
    /// New auth token (null to clear).
    #[serde(default)]
    auth_token: Option<Option<String>>,
    /// New allow_insecure flag.
    #[serde(default)]
    allow_insecure: Option<bool>,
    /// New avatar (null to clear).
    #[serde(default)]
    avatar: Option<Option<String>>,
    /// New description (null to clear).
    #[serde(default)]
    description: Option<Option<String>>,
}

/// Delete a remote agent registration (irreversible).
#[derive(Deserialize, JsonSchema)]
struct RemoteAgentDeleteParams {
    /// Remote agent id to permanently delete.
    id: String,
}

/// Test connectivity to a remote agent endpoint without persisting it.
#[derive(Deserialize, JsonSchema)]
struct RemoteAgentTestParams {
    /// Endpoint URL to test.
    url: String,
    /// Auth type for the test connection.
    #[serde(default)]
    auth_type: Option<String>,
    /// Auth token for the test connection.
    #[serde(default)]
    auth_token: Option<String>,
    /// Allow HTTP (non-TLS) endpoints.
    #[serde(default)]
    allow_insecure: bool,
}

// ── Model failover param structs ────────────────────────────────────────

/// Read the global model-failover configuration.
#[derive(Deserialize, JsonSchema)]
struct ModelFailoverGetParams {}

/// Set the global model-failover configuration.
#[derive(Deserialize, JsonSchema)]
struct ModelFailoverSetParams {
    /// Whether model failover is enabled.
    enabled: bool,
    /// Ordered list of provider+model pairs to try on failure (first = primary fallback).
    /// Each entry: { "provider_id": "...", "model": "...", "use_model": null | "..." }.
    #[serde(default)]
    queue: Vec<Value>,
    /// Maximum number of model switches per conversation turn (default: 4).
    #[serde(default = "default_max_switches")]
    max_switches: u32,
    /// Whether to mark the failed provider-model as unhealthy after failover (default: true).
    #[serde(default = "default_stamp_unhealthy")]
    stamp_unhealthy: bool,
}

fn default_max_switches() -> u32 {
    4
}
fn default_stamp_unhealthy() -> bool {
    true
}

// ── handlers ──────────────────────────────────────────────────────────────

async fn agent_list(deps: Arc<GatewayDeps>, _p: AgentListParams) -> Value {
    match deps.agent_service.list_agents().await {
        Ok(agents) => ok(agents),
        Err(e) => json!({ "error": e.to_string() }),
    }
}

async fn agent_health_check(deps: Arc<GatewayDeps>, p: AgentHealthCheckParams) -> Value {
    let req = nomifun_api_types::AcpHealthCheckRequest {
        backend: p.backend,
    };
    match deps.agent_service.acp_health_check(req).await {
        Ok(resp) => ok(resp),
        Err(e) => json!({ "error": e.to_string() }),
    }
}

async fn agent_provider_health_check(
    deps: Arc<GatewayDeps>,
    p: AgentProviderHealthCheckParams,
) -> Value {
    let req = ProviderHealthCheckRequest {
        provider_id: p.provider_id,
        model: p.model,
        task: None,
    };
    match deps.agent_service.provider_health_check(req).await {
        Ok(resp) => ok(resp),
        Err(e) => json!({ "error": e.to_string() }),
    }
}

async fn agent_set_enabled(deps: Arc<GatewayDeps>, p: AgentSetEnabledParams) -> Value {
    match deps.agent_service.set_agent_enabled(&p.id, p.enabled).await {
        Ok(meta) => ok(meta),
        Err(e) => json!({ "error": e.to_string() }),
    }
}

async fn agent_custom_create(deps: Arc<GatewayDeps>, p: AgentCustomCreateParams) -> Value {
    let advanced = match p.advanced {
        Some(val) => match serde_json::from_value(val) {
            Ok(adv) => Some(adv),
            Err(e) => return json!({ "error": format!("invalid advanced field: {e}") }),
        },
        None => None,
    };
    let req = CustomAgentUpsertRequest {
        name: p.name,
        command: p.command,
        icon: p.icon,
        args: p.args,
        env: p
            .env
            .into_iter()
            .map(|e| nomifun_api_types::AgentEnvEntry {
                name: e.name,
                value: e.value,
                description: e.description,
            })
            .collect(),
        advanced,
    };
    match deps.agent_service.create_custom_agent(req).await {
        Ok(meta) => ok(meta),
        Err(e) => json!({ "error": e.to_string() }),
    }
}

async fn agent_custom_update(deps: Arc<GatewayDeps>, p: AgentCustomUpdateParams) -> Value {
    let advanced = match p.advanced {
        Some(val) => match serde_json::from_value(val) {
            Ok(adv) => Some(adv),
            Err(e) => return json!({ "error": format!("invalid advanced field: {e}") }),
        },
        None => None,
    };
    let req = CustomAgentUpsertRequest {
        name: p.name,
        command: p.command,
        icon: p.icon,
        args: p.args,
        env: p
            .env
            .into_iter()
            .map(|e| nomifun_api_types::AgentEnvEntry {
                name: e.name,
                value: e.value,
                description: e.description,
            })
            .collect(),
        advanced,
    };
    match deps.agent_service.update_custom_agent(&p.id, req).await {
        Ok(meta) => ok(meta),
        Err(e) => json!({ "error": e.to_string() }),
    }
}

async fn agent_custom_delete(deps: Arc<GatewayDeps>, p: AgentCustomDeleteParams) -> Value {
    match deps.agent_service.delete_custom_agent(&p.id).await {
        Ok(()) => ok(json!({ "deleted": p.id })),
        Err(e) => json!({ "error": e.to_string() }),
    }
}

async fn agent_custom_try_connect(
    deps: Arc<GatewayDeps>,
    p: AgentCustomTryConnectParams,
) -> Value {
    let req = TryConnectCustomAgentRequest {
        command: p.command,
        acp_args: p.acp_args,
        env: p.env,
    };
    match deps.agent_service.try_connect_custom_agent(req).await {
        Ok(resp) => ok(resp),
        Err(e) => json!({ "error": e.to_string() }),
    }
}

// ── remote agent handlers ───────────────────────────────────────────────

async fn remote_agent_list(deps: Arc<GatewayDeps>, _p: RemoteAgentListParams) -> Value {
    match deps.remote_agent_service.list().await {
        Ok(list) => ok(list),
        Err(e) => json!({ "error": e.to_string() }),
    }
}

async fn remote_agent_get(deps: Arc<GatewayDeps>, p: RemoteAgentGetParams) -> Value {
    match deps.remote_agent_service.get(&p.id).await {
        Ok(resp) => ok(resp),
        Err(e) => json!({ "error": e.to_string() }),
    }
}

async fn remote_agent_create(deps: Arc<GatewayDeps>, p: RemoteAgentCreateParams) -> Value {
    // Deserialize protocol/auth_type from string to the typed enums via serde.
    let protocol = match serde_json::from_value(json!(p.protocol)) {
        Ok(v) => v,
        Err(e) => return json!({ "error": format!("invalid protocol: {e}") }),
    };
    let auth_type = match serde_json::from_value(json!(p.auth_type)) {
        Ok(v) => v,
        Err(e) => return json!({ "error": format!("invalid auth_type: {e}") }),
    };
    let req = nomifun_api_types::CreateRemoteAgentRequest {
        name: p.name,
        protocol,
        url: p.url,
        auth_type,
        auth_token: p.auth_token,
        allow_insecure: p.allow_insecure,
        avatar: p.avatar,
        description: p.description,
    };
    match deps.remote_agent_service.create(req).await {
        Ok(resp) => ok(resp),
        Err(e) => json!({ "error": e.to_string() }),
    }
}

async fn remote_agent_update(deps: Arc<GatewayDeps>, p: RemoteAgentUpdateParams) -> Value {
    let protocol = match p.protocol {
        Some(v) => match serde_json::from_value(json!(v)) {
            Ok(parsed) => Some(parsed),
            Err(e) => return json!({ "error": format!("invalid protocol: {e}") }),
        },
        None => None,
    };
    let auth_type = match p.auth_type {
        Some(v) => match serde_json::from_value(json!(v)) {
            Ok(parsed) => Some(parsed),
            Err(e) => return json!({ "error": format!("invalid auth_type: {e}") }),
        },
        None => None,
    };
    let req = nomifun_api_types::UpdateRemoteAgentRequest {
        name: p.name,
        protocol,
        url: p.url,
        auth_type,
        auth_token: p.auth_token,
        allow_insecure: p.allow_insecure,
        avatar: p.avatar,
        description: p.description,
    };
    match deps.remote_agent_service.update(&p.id, req).await {
        Ok(resp) => ok(resp),
        Err(e) => json!({ "error": e.to_string() }),
    }
}

async fn remote_agent_delete(deps: Arc<GatewayDeps>, p: RemoteAgentDeleteParams) -> Value {
    match deps.remote_agent_service.delete(&p.id).await {
        Ok(()) => ok(json!({ "deleted": p.id })),
        Err(e) => json!({ "error": e.to_string() }),
    }
}

async fn remote_agent_test(deps: Arc<GatewayDeps>, p: RemoteAgentTestParams) -> Value {
    let auth_type = match p.auth_type {
        Some(v) => match serde_json::from_value(json!(v)) {
            Ok(parsed) => Some(parsed),
            Err(e) => return json!({ "error": format!("invalid auth_type: {e}") }),
        },
        None => None,
    };
    let req = TestRemoteAgentConnectionRequest {
        url: p.url,
        auth_type,
        auth_token: p.auth_token,
        allow_insecure: p.allow_insecure,
    };
    match deps.remote_agent_service.test_connection(req).await {
        Ok(()) => ok(json!({ "connected": true })),
        Err(e) => json!({ "error": e.to_string() }),
    }
}

// ── model failover handlers ─────────────────────────────────────────────

async fn model_failover_get(deps: Arc<GatewayDeps>, _p: ModelFailoverGetParams) -> Value {
    let cfg =
        nomifun_conversation::model_failover::get_global_failover_config(&deps.client_pref_repo)
            .await;
    ok(cfg)
}

async fn model_failover_set(deps: Arc<GatewayDeps>, p: ModelFailoverSetParams) -> Value {
    // Deserialize queue entries into the typed ProviderWithModel vec.
    let queue: Vec<nomifun_common::ProviderWithModel> = match p
        .queue
        .into_iter()
        .map(serde_json::from_value)
        .collect::<Result<Vec<_>, _>>()
    {
        Ok(q) => q,
        Err(e) => {
            return json!({ "error": format!("invalid queue entry: {e}. Each entry must have provider_id and model fields.") })
        }
    };

    let cfg = ModelFailoverConfig {
        enabled: p.enabled,
        queue,
        max_switches: p.max_switches,
        stamp_unhealthy: p.stamp_unhealthy,
    };

    match nomifun_conversation::model_failover::set_global_failover_config(
        &deps.client_pref_repo,
        &cfg,
    )
    .await
    {
        Ok(()) => ok(cfg),
        Err(e) => json!({ "error": e.to_string() }),
    }
}

// ── registration ─────────────────────────────────────────────────────────

/// Register the agent-stack domain capabilities.
pub(crate) fn register(out: &mut Vec<Capability>) {
    // ─── Agent catalog ───────────────────────────────────────────────────

    // 1. List agents (Read)
    out.push(Capability::new::<AgentListParams, _, _>(
        CapabilityMeta::new(
            "nomi_agent_list",
            "agent",
            "List all installed agent backends with their availability status, type, and configuration.",
            DangerTier::Read,
        ),
        |deps, _ctx, p| agent_list(deps, p),
    ));

    // 2. ACP health check (Read)
    out.push(Capability::new::<AgentHealthCheckParams, _, _>(
        CapabilityMeta::new(
            "nomi_agent_health_check",
            "agent",
            "Run an ACP health check against a specific agent backend to verify it is responsive.",
            DangerTier::Read,
        ),
        |deps, _ctx, p| agent_health_check(deps, p),
    ));

    // 3. Provider health check (Read)
    out.push(Capability::new::<AgentProviderHealthCheckParams, _, _>(
        CapabilityMeta::new(
            "nomi_agent_provider_health_check",
            "agent",
            "Test model reachability through a specific provider (verify API key, model availability, latency).",
            DangerTier::Read,
        ),
        |deps, _ctx, p| agent_provider_health_check(deps, p),
    ));

    // 4. Set agent enabled (Write)
    out.push(Capability::new::<AgentSetEnabledParams, _, _>(
        CapabilityMeta::new(
            "nomi_agent_set_enabled",
            "agent",
            "Enable or disable an agent backend. Disabled agents are not available for new conversations.",
            DangerTier::Write,
        ),
        |deps, _ctx, p| agent_set_enabled(deps, p),
    ));

    // ─── Custom agents ───────────────────────────────────────────────────

    // 5. Create custom agent (Write)
    out.push(Capability::new::<AgentCustomCreateParams, _, _>(
        CapabilityMeta::new(
            "nomi_agent_custom_create",
            "agent",
            "Register a new custom agent backend (user-provided CLI binary). The process will be launched on demand.",
            DangerTier::Write,
        ),
        |deps, _ctx, p| agent_custom_create(deps, p),
    ));

    // 6. Update custom agent (Write)
    out.push(Capability::new::<AgentCustomUpdateParams, _, _>(
        CapabilityMeta::new(
            "nomi_agent_custom_update",
            "agent",
            "Update an existing custom agent backend's configuration (name, command, args, env, advanced overrides).",
            DangerTier::Write,
        ),
        |deps, _ctx, p| agent_custom_update(deps, p),
    ));

    // 7. Delete custom agent (Destructive, deny_on Channel)
    out.push(Capability::new::<AgentCustomDeleteParams, _, _>(
        CapabilityMeta::new(
            "nomi_agent_custom_delete",
            "agent",
            "Permanently delete a custom agent backend registration. Running sessions using this agent will fail on next turn.",
            DangerTier::Destructive,
        )
        .deny_on(&[Surface::Channel]),
        |deps, _ctx, p| agent_custom_delete(deps, p),
    ));

    // 8. Try-connect custom agent (Read — network probe, no state change)
    out.push(Capability::new::<AgentCustomTryConnectParams, _, _>(
        CapabilityMeta::new(
            "nomi_agent_custom_try_connect",
            "agent",
            "Test connectivity to a custom agent binary by spawning it and performing an ACP handshake (dry-run, no persistence).",
            DangerTier::Read,
        ),
        |deps, _ctx, p| agent_custom_try_connect(deps, p),
    ));

    // ─── Remote agents ───────────────────────────────────────────────────

    // 9. List remote agents (Read)
    out.push(Capability::new::<RemoteAgentListParams, _, _>(
        CapabilityMeta::new(
            "nomi_remote_agent_list",
            "agent",
            "List all registered remote agents (A2A / MCP-SSE) with their connection status.",
            DangerTier::Read,
        ),
        |deps, _ctx, p| remote_agent_list(deps, p),
    ));

    // 10. Get remote agent (Read)
    out.push(Capability::new::<RemoteAgentGetParams, _, _>(
        CapabilityMeta::new(
            "nomi_remote_agent_get",
            "agent",
            "Get full details of a remote agent by id (includes auth token if present).",
            DangerTier::Read,
        ),
        |deps, _ctx, p| remote_agent_get(deps, p),
    ));

    // 11. Create remote agent (Write)
    out.push(Capability::new::<RemoteAgentCreateParams, _, _>(
        CapabilityMeta::new(
            "nomi_remote_agent_create",
            "agent",
            "Register a new remote agent endpoint (A2A or MCP-SSE protocol) with optional authentication.",
            DangerTier::Write,
        ),
        |deps, _ctx, p| remote_agent_create(deps, p),
    ));

    // 12. Update remote agent (Write)
    out.push(Capability::new::<RemoteAgentUpdateParams, _, _>(
        CapabilityMeta::new(
            "nomi_remote_agent_update",
            "agent",
            "Update an existing remote agent's configuration. Only provided fields are changed.",
            DangerTier::Write,
        ),
        |deps, _ctx, p| remote_agent_update(deps, p),
    ));

    // 13. Delete remote agent (Destructive, deny_on Channel)
    out.push(Capability::new::<RemoteAgentDeleteParams, _, _>(
        CapabilityMeta::new(
            "nomi_remote_agent_delete",
            "agent",
            "Permanently delete a remote agent registration. Active delegations to this agent will fail.",
            DangerTier::Destructive,
        )
        .deny_on(&[Surface::Channel]),
        |deps, _ctx, p| remote_agent_delete(deps, p),
    ));

    // 14. Test remote agent connection (Read — network probe only)
    out.push(Capability::new::<RemoteAgentTestParams, _, _>(
        CapabilityMeta::new(
            "nomi_remote_agent_test",
            "agent",
            "Test connectivity to a remote agent endpoint without persisting it (dry-run handshake).",
            DangerTier::Read,
        ),
        |deps, _ctx, p| remote_agent_test(deps, p),
    ));

    // ─── Model failover ──────────────────────────────────────────────────

    // 15. Get model failover config (Read)
    out.push(Capability::new::<ModelFailoverGetParams, _, _>(
        CapabilityMeta::new(
            "nomi_model_failover_get",
            "agent",
            "Read the global model-failover configuration (enabled flag, ordered queue of fallback provider+model pairs, max switches).",
            DangerTier::Read,
        ),
        |deps, _ctx, p| model_failover_get(deps, p),
    ));

    // 16. Set model failover config (Write)
    out.push(Capability::new::<ModelFailoverSetParams, _, _>(
        CapabilityMeta::new(
            "nomi_model_failover_set",
            "agent",
            "Set the global model-failover configuration. Controls automatic fallback to alternative models when the primary provider fails.",
            DangerTier::Write,
        ),
        |deps, _ctx, p| model_failover_set(deps, p),
    ));
}

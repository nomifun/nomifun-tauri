//! Agent-stack domain capabilities: agent catalog/health, custom agent CRUD,
//! remote agent management, and model failover configuration.
//!
//! Backed by:
//! - `nomifun_ai_agent::AgentService` — installed agent listing, health checks,
//!   custom agent CRUD, enable/disable.
//! - `nomifun_ai_agent::RemoteAgentService` — remote OpenClaw Gateway CRUD,
//!   authentication, pairing, and connection testing.
//! - `nomifun_conversation::model_failover` — global model-failover config read/write
//!   (stored in `client_preferences` key `agent.model_failover`).
//!
//! NEW GatewayDeps fields assumed (parent wires):
//! - `agent_service: Arc<nomifun_ai_agent::AgentService>`
//! - `remote_agent_service: Arc<nomifun_ai_agent::RemoteAgentService>`
//! - `client_pref_repo: Arc<dyn nomifun_db::IClientPreferenceRepository>`

use std::sync::Arc;

use nomifun_api_types::{
    BehaviorPolicy, CustomAgentAdvancedOverrides, CustomAgentUpsertRequest, ModelFailoverConfig,
    ProviderHealthCheckRequest, TestRemoteAgentConnectionRequest, TryConnectCustomAgentRequest,
};
use nomifun_common::{AgentId, ProviderId, ProviderWithModel, RemoteAgentId};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::deps::GatewayDeps;
use crate::registry::{Capability, CapabilityMeta, DangerTier, Surface};
use crate::server::ok;

// ── param structs (single source: schema + runtime) ──────────────────────

/// List all installed agent backends with their status and metadata.
#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct AgentListParams {}

/// Run an ACP health check against a specific agent backend.
#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct AgentHealthCheckParams {
    /// The agent backend identifier to health-check (e.g. "claude", "codex").
    backend: String,
}

/// Run a provider-level health check (verify model reachability via a provider).
#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct AgentProviderHealthCheckParams {
    /// Provider id to test against.
    #[schemars(schema_with = "crate::id_schema::canonical_uuid_v7_schema")]
    provider_id: ProviderId,
    /// Model name to probe (must be enabled on the provider).
    #[serde(deserialize_with = "deserialize_model_name")]
    model: String,
}

/// Enable or disable an agent backend.
#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct AgentSetEnabledParams {
    /// Canonical agent_metadata.agent_id UUIDv7.
    #[schemars(schema_with = "crate::id_schema::canonical_uuid_v7_schema")]
    agent_id: AgentId,
    /// Whether to enable (true) or disable (false) the agent.
    enabled: bool,
}

/// Create a custom (user-registered) agent backend.
#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
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
    advanced: Option<CustomAgentAdvancedParam>,
}

/// Update an existing custom agent backend.
#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct AgentCustomUpdateParams {
    /// The custom agent id to update.
    #[schemars(schema_with = "crate::id_schema::canonical_uuid_v7_schema")]
    agent_id: AgentId,
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
    advanced: Option<CustomAgentAdvancedParam>,
}

/// Delete a custom agent backend (irreversible).
#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct AgentCustomDeleteParams {
    /// The custom agent id to permanently delete.
    #[schemars(schema_with = "crate::id_schema::canonical_uuid_v7_schema")]
    agent_id: AgentId,
}

/// Test connectivity to a custom agent binary (try-connect handshake).
#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
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
#[serde(deny_unknown_fields)]
struct AgentEnvEntryParam {
    /// Variable name.
    name: String,
    /// Variable value.
    value: String,
    /// Optional human-readable description of what this variable controls.
    #[serde(default)]
    description: Option<String>,
}

/// Fixed wire shape for the custom-agent advanced editor. Keep this local to
/// the gateway because capability schemas must be generated from types that
/// implement `JsonSchema`; the API crate intentionally remains schemars-free.
#[derive(Deserialize, JsonSchema, Clone)]
#[serde(deny_unknown_fields)]
struct CustomAgentAdvancedParam {
    #[serde(default)]
    yolo_id: Option<String>,
    #[serde(default)]
    native_skills_dirs: Option<Vec<String>>,
    #[serde(default)]
    behavior_policy: Option<BehaviorPolicyParam>,
    #[serde(default)]
    description: Option<String>,
}

#[derive(Deserialize, JsonSchema, Clone)]
#[serde(deny_unknown_fields)]
struct BehaviorPolicyParam {
    #[serde(default)]
    supports_side_question: bool,
    #[serde(default)]
    self_identity_sticky: bool,
    #[serde(default)]
    session_load_via_meta_field: bool,
}

impl From<BehaviorPolicyParam> for BehaviorPolicy {
    fn from(value: BehaviorPolicyParam) -> Self {
        Self {
            supports_side_question: value.supports_side_question,
            self_identity_sticky: value.self_identity_sticky,
            session_load_via_meta_field: value.session_load_via_meta_field,
        }
    }
}

impl From<CustomAgentAdvancedParam> for CustomAgentAdvancedOverrides {
    fn from(value: CustomAgentAdvancedParam) -> Self {
        Self {
            yolo_id: value.yolo_id,
            native_skills_dirs: value.native_skills_dirs,
            behavior_policy: value.behavior_policy.map(Into::into),
            description: value.description,
        }
    }
}

// ── Remote agent param structs ──────────────────────────────────────────

/// List all registered remote agents.
#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct RemoteAgentListParams {}

/// Get details of a single remote agent by id.
#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct RemoteAgentGetParams {
    /// Remote agent id returned by `nomi_remote_agent_list`.
    #[schemars(schema_with = "crate::id_schema::canonical_uuid_v7_schema")]
    remote_agent_id: RemoteAgentId,
}

/// Register a new remote agent.
#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct RemoteAgentCreateParams {
    /// Display name.
    name: String,
    /// Protocol. Currently "openclaw" is implemented for remote control.
    protocol: String,
    /// Agent endpoint URL.
    url: String,
    /// Authentication type: "none", "bearer", or "password".
    auth_type: String,
    /// Credential (required when auth_type is "bearer" or "password").
    #[serde(default)]
    auth_token: Option<String>,
    /// Skip certificate-chain and hostname verification for self-signed wss:// endpoints.
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
#[serde(deny_unknown_fields)]
struct RemoteAgentUpdateParams {
    /// Remote agent id to update.
    #[schemars(schema_with = "crate::id_schema::canonical_uuid_v7_schema")]
    remote_agent_id: RemoteAgentId,
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
#[serde(deny_unknown_fields)]
struct RemoteAgentDeleteParams {
    /// Remote agent id to permanently delete.
    #[schemars(schema_with = "crate::id_schema::canonical_uuid_v7_schema")]
    remote_agent_id: RemoteAgentId,
}

/// Test connectivity to a remote agent endpoint without persisting it.
#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct RemoteAgentTestParams {
    /// Endpoint URL to test.
    url: String,
    /// Auth type for the test connection.
    #[serde(default)]
    auth_type: Option<String>,
    /// Auth token for the test connection.
    #[serde(default)]
    auth_token: Option<String>,
    /// Skip certificate-chain and hostname verification for self-signed wss:// endpoints.
    #[serde(default)]
    allow_insecure: bool,
}

/// Perform a saved OpenClaw protocol handshake and update cached status.
#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct RemoteAgentHandshakeParams {
    /// Remote agent id to connect and authenticate.
    #[schemars(schema_with = "crate::id_schema::canonical_uuid_v7_schema")]
    remote_agent_id: RemoteAgentId,
}

// ── Model failover param structs ────────────────────────────────────────

/// Read the global model-failover configuration.
#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct ModelFailoverGetParams {}

/// Set the global model-failover configuration.
#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct ModelFailoverSetParams {
    /// Whether model failover is enabled.
    enabled: bool,
    /// Ordered list of provider+model pairs to try on failure (first = primary fallback).
    /// Each entry has exactly `{ "provider_id": "...", "model": "..." }`.
    #[serde(default)]
    queue: Vec<ModelRefParam>,
    /// Maximum number of model switches per conversation turn (default: 4).
    #[serde(default = "default_max_switches")]
    max_switches: u32,
    /// Whether to mark the failed provider-model as unhealthy after failover (default: true).
    #[serde(default = "default_stamp_unhealthy")]
    stamp_unhealthy: bool,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct ModelRefParam {
    #[schemars(schema_with = "crate::id_schema::canonical_uuid_v7_schema")]
    provider_id: ProviderId,
    #[serde(deserialize_with = "deserialize_model_name")]
    model: String,
}

impl From<ModelRefParam> for ProviderWithModel {
    fn from(value: ModelRefParam) -> Self {
        Self {
            provider_id: value.provider_id.into_string(),
            model: value.model,
            use_model: None,
        }
    }
}

fn default_max_switches() -> u32 {
    4
}
fn default_stamp_unhealthy() -> bool {
    true
}

fn deserialize_model_name<'de, D>(deserializer: D) -> Result<String, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = String::deserialize(deserializer)?;
    if value.is_empty() || value.trim() != value {
        return Err(serde::de::Error::custom(
            "model must be a non-empty trimmed natural key",
        ));
    }
    Ok(value)
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
        provider_id: p.provider_id.into_string(),
        model: p.model,
        task: None,
    };
    match deps.agent_service.provider_health_check(req).await {
        Ok(resp) => ok(resp),
        Err(e) => json!({ "error": e.to_string() }),
    }
}

async fn agent_set_enabled(deps: Arc<GatewayDeps>, p: AgentSetEnabledParams) -> Value {
    let agent_id = p.agent_id.into_string();
    match deps.agent_service.set_agent_enabled(&agent_id, p.enabled).await {
        Ok(meta) => ok(meta),
        Err(e) => json!({ "error": e.to_string() }),
    }
}

async fn agent_custom_create(deps: Arc<GatewayDeps>, p: AgentCustomCreateParams) -> Value {
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
        advanced: p.advanced.map(Into::into),
    };
    match deps.agent_service.create_custom_agent(req).await {
        Ok(meta) => ok(meta),
        Err(e) => json!({ "error": e.to_string() }),
    }
}

async fn agent_custom_update(deps: Arc<GatewayDeps>, p: AgentCustomUpdateParams) -> Value {
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
        advanced: p.advanced.map(Into::into),
    };
    match deps
        .agent_service
        .update_custom_agent(p.agent_id.as_str(), req)
        .await
    {
        Ok(meta) => ok(meta),
        Err(e) => json!({ "error": e.to_string() }),
    }
}

async fn agent_custom_delete(deps: Arc<GatewayDeps>, p: AgentCustomDeleteParams) -> Value {
    match deps
        .agent_service
        .delete_custom_agent(p.agent_id.as_str())
        .await
    {
        Ok(()) => ok(json!({ "deleted": p.agent_id })),
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
    match deps.remote_agent_service.get(&p.remote_agent_id).await {
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
    match deps
        .remote_agent_service
        .update(&p.remote_agent_id, req)
        .await
    {
        Ok(resp) => ok(resp),
        Err(e) => json!({ "error": e.to_string() }),
    }
}

async fn remote_agent_delete(deps: Arc<GatewayDeps>, p: RemoteAgentDeleteParams) -> Value {
    match deps.remote_agent_service.delete(&p.remote_agent_id).await {
        Ok(()) => ok(json!({ "remote_agent_id": p.remote_agent_id })),
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

async fn remote_agent_handshake(deps: Arc<GatewayDeps>, p: RemoteAgentHandshakeParams) -> Value {
    match deps
        .remote_agent_service
        .handshake(&p.remote_agent_id)
        .await
    {
        Ok(response) => ok(response),
        Err(e) => json!({ "error": e.to_string() }),
    }
}

async fn model_failover_get(deps: Arc<GatewayDeps>, _p: ModelFailoverGetParams) -> Value {
    let cfg =
        nomifun_conversation::model_failover::get_global_failover_config(&deps.client_pref_repo)
            .await;
    ok(cfg)
}

async fn model_failover_set(deps: Arc<GatewayDeps>, p: ModelFailoverSetParams) -> Value {
    let cfg = ModelFailoverConfig {
        enabled: p.enabled,
        queue: p.queue.into_iter().map(Into::into).collect(),
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
            "remote",
            "List registered remote OpenClaw gateways with their connection status.",
            DangerTier::Read,
        ),
        |deps, _ctx, p| remote_agent_list(deps, p),
    ));

    // 10. Get remote agent (Read)
    out.push(Capability::new::<RemoteAgentGetParams, _, _>(
        CapabilityMeta::new(
            "nomi_remote_agent_get",
            "remote",
            "Get a remote-agent configuration by remote_agent_id. Stored credentials are masked.",
            DangerTier::Read,
        ),
        |deps, _ctx, p| remote_agent_get(deps, p),
    ));

    // 11. Create remote agent (Sensitive, local Desktop only)
    out.push(Capability::new::<RemoteAgentCreateParams, _, _>(
        CapabilityMeta::new(
            "nomi_remote_agent_create",
            "remote",
            "Register a remote OpenClaw Gateway endpoint with none, bearer-token, or password authentication.",
            DangerTier::Sensitive,
        )
        .deny_on(&[Surface::Channel, Surface::Remote]),
        |deps, _ctx, p| remote_agent_create(deps, p),
    ));

    // 12. Update remote agent (Sensitive, local Desktop only)
    out.push(Capability::new::<RemoteAgentUpdateParams, _, _>(
        CapabilityMeta::new(
            "nomi_remote_agent_update",
            "remote",
            "Update an existing remote agent's configuration. Only provided fields are changed.",
            DangerTier::Sensitive,
        )
        .deny_on(&[Surface::Channel, Surface::Remote]),
        |deps, _ctx, p| remote_agent_update(deps, p),
    ));

    // 13. Delete remote agent (Destructive, local Desktop only)
    out.push(Capability::new::<RemoteAgentDeleteParams, _, _>(
        CapabilityMeta::new(
            "nomi_remote_agent_delete",
            "remote",
            "Permanently delete a remote agent registration. Active delegations to this agent will fail.",
            DangerTier::Destructive,
        )
        .deny_on(&[Surface::Channel, Surface::Remote]),
        |deps, _ctx, p| remote_agent_delete(deps, p),
    ));

    // 14. Active network access is denied from external surfaces so this
    // capability cannot become an unaudited internal-network probe.
    out.push(Capability::new::<RemoteAgentTestParams, _, _>(
        CapabilityMeta::new(
            "nomi_remote_agent_test",
            "remote",
            "Test connectivity to a remote agent endpoint without persisting it (dry-run handshake).",
            DangerTier::Sensitive,
        )
        .deny_on(&[Surface::Channel, Surface::Remote]),
        |deps, _ctx, p| remote_agent_test(deps, p),
    ));

    out.push(Capability::new::<RemoteAgentHandshakeParams, _, _>(
        CapabilityMeta::new(
            "nomi_remote_agent_handshake",
            "remote",
            "Authenticate a saved remote OpenClaw Gateway, perform its device/protocol handshake, and update connection status.",
            DangerTier::Sensitive,
        )
        .deny_on(&[Surface::Channel, Surface::Remote]),
        |deps, _ctx, p| remote_agent_handshake(deps, p),
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn set_enabled_accepts_only_canonical_agent_ids() {
        let canonical_id = AgentId::new().into_string();
        let parsed: AgentSetEnabledParams =
            serde_json::from_value(json!({ "agent_id": canonical_id })).unwrap();
        assert_eq!(parsed.agent_id.as_str(), canonical_id);

        for invalid_id in [
            "claude",
            "nomi",
            "extension:agent",
            "agent_extension_demo",
        ] {
            assert!(
                serde_json::from_value::<AgentSetEnabledParams>(
                    json!({ "agent_id": invalid_id })
                )
                .is_err(),
                "set-enabled must reject non-UUIDv7 agent_id {invalid_id}"
            );
        }
    }

    #[test]
    fn remote_agent_ids_are_canonical_strings() {
        let id = "0190f5fe-7c00-7a00-8000-000000000012";
        let get: RemoteAgentGetParams =
            serde_json::from_value(json!({ "remote_agent_id": id })).unwrap();
        let update: RemoteAgentUpdateParams =
            serde_json::from_value(json!({ "remote_agent_id": id })).unwrap();
        let delete: RemoteAgentDeleteParams =
            serde_json::from_value(json!({ "remote_agent_id": id })).unwrap();
        let handshake: RemoteAgentHandshakeParams =
            serde_json::from_value(json!({ "remote_agent_id": id })).unwrap();

        assert_eq!(get.remote_agent_id.as_str(), id);
        assert_eq!(update.remote_agent_id.as_str(), id);
        assert_eq!(delete.remote_agent_id.as_str(), id);
        assert_eq!(handshake.remote_agent_id.as_str(), id);
        assert!(
            serde_json::from_value::<RemoteAgentGetParams>(
                json!({ "remote_agent_id": "1" })
            )
            .is_err()
        );

        for legacy in [
            json!({ "id": id }),
            json!({ "id": id, "remote_agent_id": id }),
        ] {
            assert!(serde_json::from_value::<RemoteAgentGetParams>(legacy.clone()).is_err());
            assert!(serde_json::from_value::<RemoteAgentUpdateParams>(legacy.clone()).is_err());
            assert!(serde_json::from_value::<RemoteAgentDeleteParams>(legacy.clone()).is_err());
            assert!(serde_json::from_value::<RemoteAgentHandshakeParams>(legacy).is_err());
        }
    }

    #[test]
    fn remote_agent_capability_schemas_expose_only_named_wire_id() {
        let mut caps = Vec::new();
        register(&mut caps);

        for name in [
            "nomi_remote_agent_get",
            "nomi_remote_agent_update",
            "nomi_remote_agent_delete",
            "nomi_remote_agent_handshake",
        ] {
            let cap = caps
                .iter()
                .find(|cap| cap.meta.name == name)
                .unwrap_or_else(|| panic!("missing capability: {name}"));
            let properties = cap.input_schema["properties"]
                .as_object()
                .unwrap_or_else(|| panic!("capability {name} has no properties object"));

            assert!(
                properties.contains_key("remote_agent_id"),
                "capability {name} must expose remote_agent_id"
            );
            assert!(
                !properties.contains_key("id"),
                "capability {name} must reject the legacy id field"
            );
            assert_eq!(
                cap.input_schema.get("additionalProperties"),
                Some(&json!(false))
            );
        }
    }

    #[test]
    fn remote_control_surface_policy_is_local_and_sensitive() {
        let mut caps = Vec::new();
        register(&mut caps);

        for name in [
            "nomi_remote_agent_create",
            "nomi_remote_agent_update",
            "nomi_remote_agent_test",
            "nomi_remote_agent_handshake",
        ] {
            let cap = caps
                .iter()
                .find(|cap| cap.meta.name == name)
                .unwrap_or_else(|| panic!("missing capability: {name}"));
            assert_eq!(cap.meta.domain, "remote");
            assert_eq!(cap.meta.danger, DangerTier::Sensitive);
            assert!(cap.meta.deny_on.contains(&Surface::Channel));
            assert!(cap.meta.deny_on.contains(&Surface::Remote));
        }

        for name in ["nomi_remote_agent_list", "nomi_remote_agent_get"] {
            let cap = caps
                .iter()
                .find(|cap| cap.meta.name == name)
                .unwrap_or_else(|| panic!("missing capability: {name}"));
            assert_eq!(cap.meta.domain, "remote");
            assert_eq!(cap.meta.danger, DangerTier::Read);
            assert!(cap.meta.deny_on.is_empty());
        }
    }
}

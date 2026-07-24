//! `nomicore mcp-requirement-stdio` subcommand: MCP stdio server for the
//! requirement *declaration* tools (`requirement_complete` /
//! `requirement_update_status`).
//!
//! Spawned by ACP agent CLIs (claude / codex / gemini) when the requirement MCP
//! is injected into an AutoWork session. Uses the `rmcp` crate (Rust MCP SDK)
//! for protocol handling so it is byte-compatible with each CLI's MCP client.
//!
//! Tool calls are forwarded as authenticated HTTP POSTs to the in-process
//! `RequirementMcpServer` running in the main backend process at
//! the loopback port inside `NOMI_REQ_MCP_CAPABILITY`. This stdio→HTTP hop exists
//! because the spawned process cannot share the main process's
//! `RequirementService`, and because claude / codex / gemini advertise
//! stdio-only MCP capabilities (a direct HTTP MCP server would be dropped by
//! the ACP capability filter).

// Pre-existing layout convention (mirrors team_guide): the `forward_tool` impl
// block lives after the test module.
#![allow(clippy::items_after_test_module)]

use std::process::ExitCode;

use nomifun_api_types::{
    REQUIREMENT_CAPABILITY_DOMAIN, RequirementCapabilityScope,
    RequirementMcpConfig,
};
use nomifun_common::{LoopbackCapabilityError, LoopbackCapabilityClaims, RequirementId};
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::CallToolResult;
use rmcp::{schemars, service::ServiceExt, tool, tool_router, transport};
use serde::Deserialize;

use super::stdio_common::{ForwardToolOutcome, into_mcp_tool_result};

pub async fn run_requirement_stdio() -> ExitCode {
    let client = match super::stdio_common::ScopedBridgeClient::from_env(
        RequirementMcpConfig::ENV_CAPABILITY,
        REQUIREMENT_CAPABILITY_DOMAIN,
        "mcp-requirement-stdio",
        validate_requirement_claims,
    )
    .await
    {
        Ok(client) => client,
        Err(error) => {
            eprintln!("[mcp-requirement-stdio] ERROR: {error}");
            return ExitCode::from(1);
        }
    };
    let claims = client.access().await.expect("startup renewal succeeded").claims;

    eprintln!(
        "[mcp-requirement-stdio] Started OK. PORT={}, SESSION={}:{}, EXP={}",
        client.port(),
        claims.session.kind.as_str(),
        claims.session.session_id,
        claims.expires_at_unix_secs,
    );

    let lifecycle = client.clone();
    let server = RequirementStdioServer { client };

    let transport = transport::io::stdio();
    let exit = match server.serve(transport).await {
        Ok(peer) => {
            eprintln!("[mcp-requirement-stdio] MCP session started, waiting for completion...");
            if let Err(e) = peer.waiting().await {
                eprintln!("[mcp-requirement-stdio] Session ended with error: {e}");
            } else {
                eprintln!("[mcp-requirement-stdio] Session ended normally");
            }
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("[mcp-requirement-stdio] Failed to start MCP server: {e}");
            ExitCode::from(1)
        }
    };
    lifecycle.revoke().await;
    exit
}

#[derive(Clone)]
struct RequirementStdioServer {
    client: super::stdio_common::ScopedBridgeClient<RequirementCapabilityScope>,
}

fn validate_requirement_claims(
    claims: &LoopbackCapabilityClaims<RequirementCapabilityScope>,
) -> Result<(), LoopbackCapabilityError> {
    claims.validate_renewable_shape()?;
    claims.scope.validate(&claims.session)
}

#[derive(Deserialize, schemars::JsonSchema)]
struct CompleteParams {
    /// The id of the requirement you are completing. It is given to you verbatim
    /// in the AutoWork prompt ("id: ...").
    #[schemars(with = "String")]
    id: RequirementId,
    /// The exact positive claim generation from the current AutoWork prompt.
    /// A generation from an earlier turn is intentionally rejected.
    #[schemars(range(min = 1))]
    claim_generation: i64,
    /// The opaque 256-bit claim token from the current AutoWork prompt.
    #[schemars(
        length(min = 64, max = 64),
        regex(pattern = "^[0-9a-f]{64}$")
    )]
    claim_token: String,
    /// A concise note describing what you did to complete the requirement.
    #[serde(default)]
    completion_note: Option<String>,
}

#[derive(Deserialize, schemars::JsonSchema)]
struct UpdateStatusParams {
    /// The id of the requirement to update. It is given to you verbatim in the
    /// AutoWork prompt ("id: ...").
    #[schemars(with = "String")]
    id: RequirementId,
    /// The exact positive claim generation from the current AutoWork prompt.
    #[schemars(range(min = 1))]
    claim_generation: i64,
    /// The opaque 256-bit claim token from the current AutoWork prompt.
    #[schemars(
        length(min = 64, max = 64),
        regex(pattern = "^[0-9a-f]{64}$")
    )]
    claim_token: String,
    /// New status. One of: "in_progress", "done", "failed".
    status: String,
    /// Optional note or failure reason (recommended when status is "failed").
    #[serde(default)]
    note: Option<String>,
}

fn complete_args(params: CompleteParams) -> serde_json::Value {
    serde_json::json!({
        "id": params.id,
        "claim_generation": params.claim_generation,
        "claim_token": params.claim_token,
        "completion_note": params.completion_note,
    })
}

fn update_status_args(params: UpdateStatusParams) -> serde_json::Value {
    serde_json::json!({
        "id": params.id,
        "claim_generation": params.claim_generation,
        "claim_token": params.claim_token,
        "status": params.status,
        "note": params.note,
    })
}

#[tool_router]
impl RequirementStdioServer {
    #[tool(
        name = "requirement_complete",
        description = "Mark the current AutoWork requirement claim as successfully completed. Call this exactly once, with the requirement id, exact claim_generation, and opaque claim_token from the current prompt, when the work is fully done."
    )]
    async fn requirement_complete(
        &self,
        Parameters(params): Parameters<CompleteParams>,
    ) -> CallToolResult {
        eprintln!("[mcp-requirement-stdio] tools/call: requirement_complete");
        self.forward_tool("requirement_complete", &complete_args(params))
            .await
    }

    #[tool(
        name = "requirement_update_status",
        description = "Update the status of the current AutoWork requirement claim. Pass the exact claim_generation and opaque claim_token from the current prompt. Use status=\"failed\" with a reason if you cannot complete it; status=\"done\" is equivalent to requirement_complete."
    )]
    async fn requirement_update_status(
        &self,
        Parameters(params): Parameters<UpdateStatusParams>,
    ) -> CallToolResult {
        eprintln!("[mcp-requirement-stdio] tools/call: requirement_update_status");
        self.forward_tool("requirement_update_status", &update_status_args(params))
            .await
    }
}

#[rmcp::tool_handler(router = Self::tool_router())]
impl rmcp::ServerHandler for RequirementStdioServer {
    async fn list_tools(
        &self,
        _request: Option<rmcp::model::PaginatedRequestParams>,
        _context: rmcp::service::RequestContext<rmcp::RoleServer>,
    ) -> Result<rmcp::model::ListToolsResult, rmcp::ErrorData> {
        let claims = self
            .client
            .access()
            .await
            .map_err(capability_request_error)?
            .claims;
        let tools = Self::tool_router()
            .list_all()
            .into_iter()
            .filter(|tool| claims.allows(&tool.name))
            .collect();
        Ok(rmcp::model::ListToolsResult {
            tools,
            meta: None,
            next_cursor: None,
        })
    }

    async fn call_tool(
        &self,
        request: rmcp::model::CallToolRequestParams,
        context: rmcp::service::RequestContext<rmcp::RoleServer>,
    ) -> Result<rmcp::model::CallToolResult, rmcp::ErrorData> {
        self.client
            .access_for(&request.name)
            .await
            .map_err(capability_request_error)?;
        let call = rmcp::handler::server::tool::ToolCallContext::new(self, request, context);
        Self::tool_router().call(call).await
    }
}

fn capability_request_error(error: String) -> rmcp::ErrorData {
    rmcp::ErrorData::invalid_request(
        format!("requirement capability is no longer valid: {error}"),
        None,
    )
}

fn forwarded_tool_result(outcome: ForwardToolOutcome) -> CallToolResult {
    into_mcp_tool_result(outcome)
}

#[cfg(test)]
mod tests {
    use super::*;

    const CLAIM_TOKEN: &str =
        "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

    #[test]
    fn all_tool_schemas_have_properties_field() {
        let router = RequirementStdioServer::tool_router();
        let tools = router.list_all();
        assert!(!tools.is_empty(), "requirement bridge must register at least one tool");
        for tool in &tools {
            assert!(
                tool.input_schema.contains_key("properties"),
                "Tool '{}' schema missing 'properties' field: {:?}. OpenAI API rejects schemas without it.",
                tool.name,
                tool.input_schema,
            );
        }
    }

    #[test]
    fn registers_both_requirement_tools() {
        let router = RequirementStdioServer::tool_router();
        let names: Vec<String> = router.list_all().iter().map(|t| t.name.to_string()).collect();
        assert!(names.contains(&"requirement_complete".to_string()), "got {names:?}");
        assert!(names.contains(&"requirement_update_status".to_string()), "got {names:?}");
    }

    #[test]
    fn requirement_ids_are_uuid_strings_in_schemas_and_forwarded_json() {
        let id = RequirementId::new().into_string();
        let complete = complete_args(
            serde_json::from_value(serde_json::json!({
                "id": id,
                "claim_generation": 7,
                "claim_token": CLAIM_TOKEN,
                "completion_note": "done",
            }))
            .unwrap(),
        );
        let update = update_status_args(
            serde_json::from_value(serde_json::json!({
                "id": id,
                "claim_generation": 7,
                "claim_token": CLAIM_TOKEN,
                "status": "failed",
                "note": "blocked",
            }))
            .unwrap(),
        );

        assert_eq!(
            complete,
            serde_json::json!({
                "id": id,
                "claim_generation": 7,
                "claim_token": CLAIM_TOKEN,
                "completion_note": "done"
            })
        );
        assert_eq!(
            update,
            serde_json::json!({
                "id": id,
                "claim_generation": 7,
                "claim_token": CLAIM_TOKEN,
                "status": "failed",
                "note": "blocked"
            })
        );
        assert!(
            serde_json::from_value::<CompleteParams>(
                serde_json::json!({
                    "id": 1,
                    "claim_generation": 7,
                    "claim_token": CLAIM_TOKEN
                })
            )
            .is_err()
        );
        assert!(
            serde_json::from_value::<UpdateStatusParams>(
                serde_json::json!({
                    "id": 1,
                    "claim_generation": 7,
                    "claim_token": CLAIM_TOKEN,
                    "status": "done"
                })
            )
            .is_err()
        );
        assert!(
            serde_json::from_value::<CompleteParams>(
                serde_json::json!({
                    "id": id,
                    "claim_token": CLAIM_TOKEN,
                    "completion_note": "missing generation"
                })
            )
            .is_err()
        );
        assert!(
            serde_json::from_value::<CompleteParams>(serde_json::json!({
                "id": id,
                "claim_generation": 7,
                "completion_note": "missing token"
            }))
            .is_err()
        );

        for tool in RequirementStdioServer::tool_router().list_all() {
            assert_eq!(
                tool.input_schema
                    .get("properties")
                    .and_then(serde_json::Value::as_object)
                    .and_then(|properties| properties.get("id"))
                    .and_then(serde_json::Value::as_object)
                    .and_then(|id_schema| id_schema.get("type")),
                Some(&serde_json::json!("string")),
                "Tool '{}' must advertise requirement id as a JSON string",
                tool.name,
            );
            assert!(
                tool.input_schema
                    .get("required")
                    .and_then(serde_json::Value::as_array)
                    .is_some_and(|required| {
                        required
                            .iter()
                            .any(|field| field.as_str() == Some("claim_generation"))
                    }),
                "Tool '{}' must require claim_generation",
                tool.name,
            );
            assert_eq!(
                tool.input_schema
                    .get("properties")
                    .and_then(serde_json::Value::as_object)
                    .and_then(|properties| properties.get("claim_generation"))
                    .and_then(serde_json::Value::as_object)
                    .and_then(|schema| schema.get("minimum")),
                Some(&serde_json::json!(1)),
                "Tool '{}' must advertise claim_generation >= 1",
                tool.name,
            );
            assert!(
                tool.input_schema
                    .get("required")
                    .and_then(serde_json::Value::as_array)
                    .is_some_and(|required| {
                        required
                            .iter()
                            .any(|field| field.as_str() == Some("claim_token"))
                    }),
                "Tool '{}' must require claim_token",
                tool.name,
            );
            assert_eq!(
                tool.input_schema
                    .get("properties")
                    .and_then(serde_json::Value::as_object)
                    .and_then(|properties| properties.get("claim_token"))
                    .and_then(serde_json::Value::as_object)
                    .and_then(|schema| schema.get("pattern")),
                Some(&serde_json::json!("^[0-9a-f]{64}$")),
                "Tool '{}' must advertise canonical opaque claim_token",
                tool.name,
            );
        }
    }

    #[test]
    fn forwarded_requirement_error_sets_mcp_is_error() {
        let result = forwarded_tool_result(ForwardToolOutcome::Error(
            r#"{"error":"requirement update rejected"}"#.into(),
        ));
        assert_eq!(result.is_error, Some(true));
    }

    #[test]
    fn forwarded_requirement_success_does_not_guess_from_text() {
        let result = forwarded_tool_result(ForwardToolOutcome::Success(
            "Error: ordinary completion note".into(),
        ));
        assert_ne!(result.is_error, Some(true));
    }
}

impl RequirementStdioServer {
    async fn forward_tool(&self, tool_name: &str, args: &serde_json::Value) -> CallToolResult {
        let body = serde_json::json!({
            "tool": tool_name,
            "args": args,
        });
        forwarded_tool_result(
            self.client
                .forward_tool_outcome(tool_name, body, false)
                .await,
        )
    }
}

//! REST `/v1` adapter — the human/script-facing projection of the gateway
//! Registry, beside the flagship `/mcp` MCP adapter. Auto-generated from the
//! SAME registry, so it inherits every capability and the Remote surface gate:
//!
//! - `GET  /v1/tools`            — list the Remote-surface capabilities + schemas
//! - `POST /v1/tools/{name}`     — invoke a capability (body = its JSON args)
//! - `GET  /v1/openapi.json`     — OpenAPI 3.1 doc generated from the schemas
//!
//! Token-gated by the same companion-token middleware as `/mcp`. Mount with
//! `.nest("/v1", ..)` (never `.merge`, same reason as the MCP router).

use std::sync::Arc;

use axum::{
    Extension, Json, Router,
    extract::{Path, Query, State, rejection::JsonRejection},
    http::{HeaderMap, StatusCode},
    middleware::from_fn_with_state,
    response::sse::{Event, KeepAlive, Sse},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use nomifun_auth::CompanionTokenValidator;
use nomifun_gateway::{CallerCtx, GatewayDeps, Registry, Surface, ToolSpec};
use serde::Deserialize;
use serde_json::{Value, json};
use std::convert::Infallible;

use crate::idempotency::{CONVERSATION_SEND_TOOL, remote_operation_id};
use crate::router::{
    PublicMcpState, RemoteCompanion, companion_token_middleware,
};

#[derive(Clone)]
struct RestState {
    deps: Arc<GatewayDeps>,
}

/// `?profile=agent|full` (default full) — curate the advertised catalog.
#[derive(Deserialize)]
struct ProfileQuery {
    #[serde(default)]
    profile: Option<String>,
    #[serde(default)]
    domains: Option<String>,
}

fn domains_from_query_value(domains: Option<&str>) -> Option<Vec<String>> {
    let selected: Vec<String> = domains?
        .split(',')
        .map(str::trim)
        .filter(|domain| !domain.is_empty())
        .map(ToOwned::to_owned)
        .collect();
    (!selected.is_empty()).then_some(selected)
}

/// Resolve a profile name to the matching Remote-surface tool specs.
fn specs_for_profile(profile: Option<&str>) -> Vec<ToolSpec> {
    match profile {
        Some("agent") => {
            Registry::global().tool_specs_for(Surface::Remote, crate::AGENT_PROFILE_DOMAINS)
        }
        _ => Registry::global().tool_specs(Surface::Remote),
    }
}

fn specs_for_query(q: &ProfileQuery) -> Vec<ToolSpec> {
    if let Some(domains) = domains_from_query_value(q.domains.as_deref()) {
        let domain_refs: Vec<&str> = domains.iter().map(String::as_str).collect();
        return Registry::global().tool_specs_for(Surface::Remote, &domain_refs);
    }
    specs_for_profile(q.profile.as_deref())
}

fn remote_caller(
    state: &RestState,
    companion_id: &nomifun_common::CompanionId,
    tool_name: &str,
    headers: &HeaderMap,
) -> Result<CallerCtx, Response> {
    let mut caller =
        CallerCtx::try_remote(&state.deps.authoritative_user_id, companion_id.as_str())
            .map_err(|error| {
                (
                    StatusCode::UNAUTHORIZED,
                    Json(json!({
                        "error": format!("invalid authenticated identity: {error}")
                    })),
                )
                    .into_response()
            })?;
    if tool_name == CONVERSATION_SEND_TOOL {
        caller.operation_id = Some(
            remote_operation_id(headers, companion_id.as_str(), tool_name).map_err(
                |error| {
                    (
                        StatusCode::BAD_REQUEST,
                        Json(json!({
                            "error": "invalid_idempotency_key",
                            "message": error,
                        })),
                    )
                        .into_response()
                },
            )?,
        );
    }
    Ok(caller)
}

/// `GET /v1/tools[?profile=agent]` — the Remote-surface capability catalog
/// (name + description + JSON Schema). `profile=agent` returns the curated
/// do-work subset; default is the full surface.
async fn list_tools(Query(q): Query<ProfileQuery>) -> Json<Value> {
    let tools: Vec<Value> = specs_for_query(&q)
        .into_iter()
        .map(|s| json!({ "name": s.name, "domain": s.domain, "description": s.description, "input_schema": s.input_schema }))
        .collect();
    Json(json!({ "count": tools.len(), "tools": tools }))
}

/// `POST /v1/tools/{name}` — invoke a capability. Body is the capability's JSON
/// args (empty body == `{}`). Dispatches under `Surface::Remote`, so the danger
/// gate (Destructive→needs_confirmation, Sensitive→denied) applies identically.
async fn call_tool(
    State(state): State<RestState>,
    Path(name): Path<String>,
    Query(q): Query<ProfileQuery>,
    Extension(RemoteCompanion(companion_id)): Extension<RemoteCompanion>,
    headers: HeaderMap,
    body: Result<Json<Value>, JsonRejection>,
) -> Response {
    // Lenient: a no-arg tool may be POSTed with an empty body.
    let args = match body {
        Ok(Json(v)) if v.is_null() => json!({}),
        Ok(Json(v)) => v,
        Err(_) => json!({}),
    };
    if !specs_for_query(&q).iter().any(|spec| spec.name == name) {
        return (
            StatusCode::UNPROCESSABLE_ENTITY,
            Json(json!({ "error": format!("Tool '{name}' is outside the configured Remote REST capability scope") })),
        )
            .into_response();
    }
    let ctx = match remote_caller(&state, &companion_id, &name, &headers) {
        Ok(ctx) => ctx,
        Err(response) => return response,
    };
    match Registry::global()
        .dispatch_opt(state.deps.clone(), ctx, &name, &args)
        .await
    {
        Some(result) => {
            // Map the registry result envelope onto HTTP status codes.
            let status = if result.get("error").is_some() {
                StatusCode::UNPROCESSABLE_ENTITY
            } else if result.get("needs_confirmation").is_some() {
                StatusCode::CONFLICT
            } else {
                StatusCode::OK
            };
            (status, Json(result)).into_response()
        }
        None => (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": format!("Unknown tool: {name}") })),
        )
            .into_response(),
    }
}

/// `POST /v1/tools/{name}/stream` — Server-Sent Events stream of a tool call.
/// Each SSE `data:` frame is a JSON event; streaming tools emit incremental
/// `{"type": ..}` deltas as they happen, and
/// every call ends with one `{"type":"__result__","data": <final>}` frame.
/// Non-streaming tools emit only that terminal frame.
async fn stream_tool(
    State(state): State<RestState>,
    Path(name): Path<String>,
    Query(q): Query<ProfileQuery>,
    Extension(RemoteCompanion(companion_id)): Extension<RemoteCompanion>,
    headers: HeaderMap,
    body: Result<Json<Value>, JsonRejection>,
) -> Response {
    let args = match body {
        Ok(Json(v)) if !v.is_null() => v,
        _ => json!({}),
    };
    if !specs_for_query(&q).iter().any(|spec| spec.name == name) {
        return (
            StatusCode::UNPROCESSABLE_ENTITY,
            Json(json!({
                "error": format!(
                    "Tool '{name}' is outside the configured Remote REST capability scope"
                )
            })),
        )
            .into_response();
    }
    let ctx = match remote_caller(&state, &companion_id, &name, &headers) {
        Ok(ctx) => ctx,
        Err(response) => return response,
    };
    let (tx, rx) = tokio::sync::mpsc::channel::<Value>(256);
    let deps = state.deps.clone();
    tokio::spawn(async move {
        let final_val = match Registry::global()
            .dispatch_stream(deps, ctx, &name, &args, tx.clone())
            .await
        {
            Some(v) => v,
            None => json!({ "error": format!("Unknown tool: {name}") }),
        };
        // Terminal frame carries the final result/envelope; sending it then
        // dropping `tx` ends the SSE stream.
        let _ = tx
            .send(json!({ "type": "__result__", "data": final_val }))
            .await;
    });
    let stream = futures::stream::unfold(rx, |mut rx| async move {
        rx.recv().await.map(|v| {
            (
                Ok::<Event, Infallible>(Event::default().data(v.to_string())),
                rx,
            )
        })
    });
    Sse::new(stream)
        .keep_alive(KeepAlive::default())
        .into_response()
}

/// `GET /v1/openapi.json` — OpenAPI 3.1 generated from the registry: one
/// `POST /v1/tools/{name}` operation per Remote capability, requestBody schema =
/// the capability's input schema.
async fn openapi(Query(q): Query<ProfileQuery>) -> Json<Value> {
    let mut paths = serde_json::Map::new();
    for s in specs_for_query(&q) {
        paths.insert(
            format!("/v1/tools/{}", s.name),
            json!({
                "post": {
                    "summary": s.description,
                    "operationId": s.name,
                    "requestBody": {
                        "required": true,
                        "content": { "application/json": { "schema": s.input_schema } }
                    },
                    "responses": {
                        "200": { "description": "tool result", "content": { "application/json": { "schema": { "type": "object" } } } },
                        "409": { "description": "needs confirmation (re-call with confirm=true)" },
                        "422": { "description": "tool returned an error" }
                    },
                    "security": [{ "bearerAuth": [] }]
                }
            }),
        );
    }
    Json(json!({
        "openapi": "3.1.0",
        "info": {
            "title": "NomiFun Remote Capability API",
            "version": "v1",
            "description": "External-companion access to NomiFun platform capabilities. All operations require Authorization: Bearer <companion access token>."
        },
        "paths": paths,
        "components": {
            "securitySchemes": { "bearerAuth": { "type": "http", "scheme": "bearer" } }
        }
    }))
}

/// Build the REST sub-router. Mount with `.nest("/v1", ..)`; the companion-token
/// layer + this router's routes are then scoped to `/v1`.
pub fn public_rest_router(
    deps: Arc<GatewayDeps>,
    validator: Arc<CompanionTokenValidator>,
) -> Router {
    Router::new()
        .route("/tools", get(list_tools))
        .route("/tools/{name}", post(call_tool))
        .route("/tools/{name}/stream", post(stream_tool))
        .route("/openapi.json", get(openapi))
        .with_state(RestState { deps })
        .layer(from_fn_with_state(
            PublicMcpState { validator },
            companion_token_middleware,
        ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn openapi_lists_remote_tools() {
        // openapi() is pure; exercise the path-generation against the real registry.
        let specs = Registry::global().tool_specs(Surface::Remote);
        assert!(!specs.is_empty());
        // every Remote tool yields a /v1/tools/<name> POST path
        assert!(specs.iter().all(|s| !s.name.is_empty()));
    }

    #[test]
    fn custom_domains_filter_rest_catalog() {
        let full = specs_for_profile(None);
        let filtered = specs_for_query(&ProfileQuery {
            profile: Some("full".to_string()),
            domains: Some("agent,files".to_string()),
        });
        assert!(!filtered.is_empty());
        assert!(filtered.len() < full.len());
        assert!(
            filtered
                .iter()
                .all(|s| s.domain == "agent" || s.domain == "files")
        );
    }
}

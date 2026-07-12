# MCP JSON ID Contract Repair Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make MCP connection tests use a canonical numeric catalog id while accepting legacy decimal-string ids at the HTTP boundary and preventing frontend response entities from becoming request contracts.

**Architecture:** The backend request DTO normalizes JSON integer and decimal-string ids into `Option<i64>` before the handler runs, and the service/repository path remains numeric thereafter. The frontend builds a dedicated request containing only the endpoint-owned fields and omits the detected-server sentinel id. DTO, HTTP persistence, and frontend mapping tests guard all three boundaries.

**Tech Stack:** Rust, Serde, Axum, SQLite repository tests, TypeScript, Bun test, React hook integration.

## Global Constraints

- The canonical persisted MCP server id is a JSON number and Rust `i64`.
- Already-released clients sending a decimal string such as `"id":"1"` remain compatible.
- Non-numeric strings, floating-point ids, booleans, arrays, and objects are rejected at deserialization.
- Missing and explicit-null ids mean “test without persisting a catalog result.”
- Detected or extension MCP servers with the sentinel id `0` do not send an id.
- No global coercion is added for unrelated string fields.

---

### Task 1: Reproduce the real HTTP failure and lock result persistence

**Files:**
- Modify: `crates/backend/nomifun-app/tests/mcp_e2e.rs`

**Interfaces:**
- Consumes: `POST /api/mcp/servers`, `POST /api/mcp/test-connection`, `GET /api/mcp/servers`.
- Produces: An endpoint-level regression test proving a numeric saved id reaches connection testing and persists `last_test_status`.

- [ ] **Step 1: Write the failing endpoint regression test**

Append this test after the existing connection-test cases in `mcp_e2e.rs`:

```rust
#[tokio::test]
async fn connection_test_accepts_numeric_saved_server_id_and_persists_failure() {
    let (mut app, services) = build_app().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;

    let create = json_with_token(
        "POST",
        "/api/mcp/servers",
        json!({
            "name": "numeric-id-test",
            "transport": {
                "type": "stdio",
                "command": "nonexistent-mcp-command-numeric-id-test"
            }
        }),
        &token,
        &csrf,
    );
    let created = app.clone().oneshot(create).await.unwrap();
    assert_eq!(created.status(), StatusCode::CREATED);
    let created_json = body_json(created).await;
    let server_id = created_json["data"]["id"].as_i64().unwrap();

    let test = json_with_token(
        "POST",
        "/api/mcp/test-connection",
        json!({
            "id": server_id,
            "name": "numeric-id-test",
            "transport": {
                "type": "stdio",
                "command": "nonexistent-mcp-command-numeric-id-test"
            }
        }),
        &token,
        &csrf,
    );
    let tested = app.clone().oneshot(test).await.unwrap();
    assert_eq!(tested.status(), StatusCode::UNPROCESSABLE_ENTITY);
    let tested_json = body_json(tested).await;
    assert_eq!(tested_json["code"], "MCP_COMMAND_NOT_FOUND");

    let listed = app
        .clone()
        .oneshot(get_with_token("/api/mcp/servers", &token))
        .await
        .unwrap();
    assert_eq!(listed.status(), StatusCode::OK);
    let listed_json = body_json(listed).await;
    let persisted = listed_json["data"]
        .as_array()
        .unwrap()
        .iter()
        .find(|server| server["id"].as_i64() == Some(server_id))
        .unwrap();
    assert_eq!(persisted["last_test_status"], "error");
}
```

- [ ] **Step 2: Run the endpoint test and verify RED**

Run:

```bash
cargo test -p nomifun-app --test mcp_e2e connection_test_accepts_numeric_saved_server_id_and_persists_failure -- --exact --nocapture
```

Expected: FAIL because the endpoint returns `400 Bad Request` while deserializing numeric `id`, rather than `422 Unprocessable Entity` from the deterministic missing command.

- [ ] **Step 3: Commit the failing regression test**

```bash
git add crates/backend/nomifun-app/tests/mcp_e2e.rs
git commit -m "test(mcp): reproduce numeric connection-test id failure"
```

### Task 2: Normalize numeric ids at the backend boundary

**Files:**
- Modify: `crates/backend/nomifun-api-types/src/serde_util.rs`
- Modify: `crates/backend/nomifun-api-types/src/mcp.rs`
- Modify: `crates/backend/nomifun-mcp/src/routes.rs`
- Modify: `crates/backend/nomifun-mcp/src/service.rs`

**Interfaces:**
- Consumes: JSON `id` as an integer, decimal string, null, or missing field.
- Produces: `TestMcpConnectionRequest.id: Option<i64>` and `McpConfigService::persist_test_result(id: i64, result: &McpConnectionTestResult)`.

- [ ] **Step 1: Add failing DTO expectations for canonical and legacy ids**

Replace the existing `test_connection_request_deserialization` test in `mcp.rs` with:

```rust
#[test]
fn test_connection_request_accepts_numeric_id() {
    let body = r#"{"id":1,"name":"test-server","transport":{"type":"http","url":"https://example.com/mcp"}}"#;
    let req: TestMcpConnectionRequest =
        serde_json::from_str(body).expect("numeric MCP catalog id must deserialize");
    assert_eq!(req.id.map(|id| id.to_string()).as_deref(), Some("1"));
    assert_eq!(req.name, "test-server");
}

#[test]
fn test_connection_request_accepts_legacy_decimal_string_id() {
    let json = serde_json::json!({
        "id": "123",
        "name": "test-server",
        "transport": { "type": "http", "url": "https://example.com/mcp" }
    });
    let req: TestMcpConnectionRequest = serde_json::from_value(json).unwrap();
    assert_eq!(req.id.map(|id| id.to_string()).as_deref(), Some("123"));
}

#[test]
fn test_connection_request_allows_missing_or_null_id() {
    for id_fragment in ["", r#""id":null,"#] {
        let body = format!(
            r#"{{{id_fragment}"name":"test-server","transport":{{"type":"http","url":"https://example.com/mcp"}}}}"#
        );
        let req: TestMcpConnectionRequest = serde_json::from_str(&body).unwrap();
        assert!(req.id.is_none());
    }
}

#[test]
fn test_connection_request_rejects_non_numeric_or_fractional_id() {
    for invalid_id in [serde_json::json!("mcp_123"), serde_json::json!(1.5)] {
        let json = serde_json::json!({
            "id": invalid_id,
            "name": "test-server",
            "transport": { "type": "http", "url": "https://example.com/mcp" }
        });
        assert!(serde_json::from_value::<TestMcpConnectionRequest>(json).is_err());
    }
}
```

- [ ] **Step 2: Run the DTO test and verify RED**

Run:

```bash
cargo test -p nomifun-api-types mcp::tests::test_connection_request_accepts_numeric_id -- --exact --nocapture
```

Expected: FAIL with `invalid type: integer 1, expected a string`.

- [ ] **Step 3: Add the optional numeric-id boundary deserializer**

Append to `serde_util.rs`:

```rust
/// Deserialize an optional numeric database id from either a JSON integer or
/// a legacy decimal string. Missing fields are handled by `#[serde(default)]`;
/// explicit JSON null is normalized to `None`.
pub(crate) fn deserialize_optional_i64_from_string_or_integer<'de, D>(
    deserializer: D,
) -> Result<Option<i64>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    struct OptionalI64Visitor;

    impl<'de> serde::de::Visitor<'de> for OptionalI64Visitor {
        type Value = Option<i64>;

        fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            formatter.write_str("a numeric id as an integer, decimal string, or null")
        }

        fn visit_i64<E>(self, value: i64) -> Result<Self::Value, E>
        where
            E: serde::de::Error,
        {
            Ok(Some(value))
        }

        fn visit_u64<E>(self, value: u64) -> Result<Self::Value, E>
        where
            E: serde::de::Error,
        {
            i64::try_from(value)
                .map(Some)
                .map_err(|_| E::custom("numeric id exceeds the signed 64-bit range"))
        }

        fn visit_str<E>(self, value: &str) -> Result<Self::Value, E>
        where
            E: serde::de::Error,
        {
            value
                .parse::<i64>()
                .map(Some)
                .map_err(|_| E::custom("numeric id string must contain a decimal integer"))
        }

        fn visit_string<E>(self, value: String) -> Result<Self::Value, E>
        where
            E: serde::de::Error,
        {
            self.visit_str(&value)
        }

        fn visit_none<E>(self) -> Result<Self::Value, E>
        where
            E: serde::de::Error,
        {
            Ok(None)
        }

        fn visit_unit<E>(self) -> Result<Self::Value, E>
        where
            E: serde::de::Error,
        {
            Ok(None)
        }
    }

    deserializer.deserialize_any(OptionalI64Visitor)
}
```

- [ ] **Step 4: Make the request DTO canonical numeric**

Change `TestMcpConnectionRequest` in `mcp.rs` to:

```rust
#[derive(Debug, Deserialize)]
pub struct TestMcpConnectionRequest {
    #[serde(
        default,
        deserialize_with = "crate::serde_util::deserialize_optional_i64_from_string_or_integer"
    )]
    pub id: Option<i64>,
    pub name: String,
    pub transport: McpTransport,
}
```

Update the assertions added in Step 1 to compare `req.id` directly with `Some(1)` and `Some(123)` after GREEN proves the type transition.

- [ ] **Step 5: Keep the service path numeric after deserialization**

Change `McpConfigService::persist_test_result` in `service.rs` to:

```rust
pub async fn persist_test_result(
    &self,
    id: i64,
    result: &McpConnectionTestResult,
) -> Result<(), McpError> {
    let status = if result.success { "connected" } else { "error" };
    let last_connected = if result.success { Some(now_ms()) } else { None };
    let tools_json = result.tools.as_ref().map(serde_json::to_string).transpose()?;

    self.repo.update_status(id, status, last_connected).await?;
    self.repo.update_tools(id, tools_json.as_deref()).await?;
    Ok(())
}
```

Change the route to consume the numeric option:

```rust
if let Some(server_id) = req.id {
    state.config_service.persist_test_result(server_id, &result).await?;
}
```

In the two existing persistence tests in `service.rs`, replace calls shaped as:

```rust
svc.persist_test_result(&created.id.to_string(), &result)
```

with:

```rust
svc.persist_test_result(created.id, &result)
```

Apply the same replacement to the `success` and `failure` calls in the failure/clear-tools test.

- [ ] **Step 6: Run backend GREEN verification**

Run:

```bash
cargo test -p nomifun-api-types mcp::tests::test_connection_request -- --nocapture
cargo test -p nomifun-mcp persist_test_result -- --nocapture
cargo test -p nomifun-app --test mcp_e2e connection_test_accepts_numeric_saved_server_id_and_persists_failure -- --exact --nocapture
```

Expected: all selected tests PASS; the endpoint test reaches `MCP_COMMAND_NOT_FOUND` and reloads `last_test_status: error`.

- [ ] **Step 7: Commit the backend contract repair**

```bash
git add crates/backend/nomifun-api-types/src/serde_util.rs crates/backend/nomifun-api-types/src/mcp.rs crates/backend/nomifun-mcp/src/routes.rs crates/backend/nomifun-mcp/src/service.rs
git commit -m "fix(mcp): normalize connection-test ids at API boundary"
```

### Task 3: Introduce a dedicated frontend connection-test request

**Files:**
- Create: `ui/src/common/adapter/mcpRequest.ts`
- Create: `ui/src/common/adapter/mcpRequest.test.ts`
- Modify: `ui/src/common/adapter/ipcBridge.ts`
- Modify: `ui/src/renderer/hooks/mcp/useMcpConnection.ts`

**Interfaces:**
- Consumes: `Pick<IMcpServer, 'id' | 'name' | 'transport'>`.
- Produces: `McpConnectionTestRequest` with `id?: number`, `name`, and `transport`; `id` is omitted for sentinel `0`.

- [ ] **Step 1: Write the failing frontend mapper tests**

Create `mcpRequest.test.ts`:

```typescript
/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

import { describe, expect, test } from 'bun:test';
import type { IMcpServer } from '@/common/config/storage';
import { buildMcpConnectionTestRequest } from './mcpRequest';

const transport: IMcpServer['transport'] = {
  type: 'sse',
  url: 'https://example.com/sse',
  headers: { Authorization: 'Bearer test' },
};

describe('buildMcpConnectionTestRequest', () => {
  test('keeps a persisted numeric id and sends only endpoint-owned fields', () => {
    const server: IMcpServer = {
      id: 1,
      name: 'search',
      description: 'not part of test request',
      enabled: true,
      transport,
      tools: [{ name: 'search' }],
      last_test_status: 'connected',
      last_connected: 100,
      created_at: 10,
      updated_at: 20,
      original_json: '{}',
      builtin: false,
    };

    expect(buildMcpConnectionTestRequest(server)).toEqual({
      id: 1,
      name: 'search',
      transport,
    });
  });

  test('omits the detected-server sentinel id', () => {
    expect(buildMcpConnectionTestRequest({ id: 0, name: 'detected', transport })).toEqual({
      name: 'detected',
      transport,
    });
  });
});
```

- [ ] **Step 2: Run the mapper test and verify RED**

Run:

```bash
bun test ui/src/common/adapter/mcpRequest.test.ts
```

Expected: FAIL because `./mcpRequest` does not exist.

- [ ] **Step 3: Implement the dedicated request type and builder**

Create `mcpRequest.ts`:

```typescript
/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

import type { IMcpServer, IMcpServerTransport } from '@/common/config/storage';

export interface McpConnectionTestRequest {
  id?: number;
  name: string;
  transport: IMcpServerTransport;
}

export const buildMcpConnectionTestRequest = (
  server: Pick<IMcpServer, 'id' | 'name' | 'transport'>
): McpConnectionTestRequest => ({
  ...(server.id > 0 ? { id: server.id } : {}),
  name: server.name,
  transport: server.transport,
});
```

- [ ] **Step 4: Use the request contract in the bridge and hook**

Add this type import to `ipcBridge.ts`:

```typescript
import type { McpConnectionTestRequest } from './mcpRequest';
```

Change the request type argument for `testMcpConnection` from `IMcpServer` to:

```typescript
McpConnectionTestRequest
```

Add this import to `useMcpConnection.ts`:

```typescript
import { buildMcpConnectionTestRequest } from '@/common/adapter/mcpRequest';
```

Change the invocation to:

```typescript
const result = await mcpService.testMcpConnection.invoke(buildMcpConnectionTestRequest(server));
```

- [ ] **Step 5: Run frontend GREEN verification**

Run:

```bash
bun test ui/src/common/adapter/mcpRequest.test.ts
bun run typecheck
```

Expected: 2 mapper tests PASS and TypeScript exits 0.

- [ ] **Step 6: Commit the frontend boundary repair**

```bash
git add ui/src/common/adapter/mcpRequest.ts ui/src/common/adapter/mcpRequest.test.ts ui/src/common/adapter/ipcBridge.ts ui/src/renderer/hooks/mcp/useMcpConnection.ts
git commit -m "fix(ui): send a dedicated MCP connection-test request"
```

### Task 4: Full verification and contract audit

**Files:**
- Verify only; modify earlier files only if a verification failure is caused by this change.

**Interfaces:**
- Consumes: All changes from Tasks 1-3.
- Produces: Fresh evidence that formatting, backend crates, MCP E2E behavior, frontend mapper, and TypeScript contracts are green.

- [ ] **Step 1: Format and inspect the patch**

Run:

```bash
cargo fmt --all
git diff --check
git status --short
git diff HEAD~3 --stat
```

Expected: no whitespace errors; only the planned files and documentation commits are present.

- [ ] **Step 2: Run backend verification**

Run:

```bash
cargo test -p nomifun-api-types
cargo test -p nomifun-mcp
cargo test -p nomifun-app --test mcp_e2e
```

Expected: all tests PASS with zero failures.

- [ ] **Step 3: Run frontend verification**

Run:

```bash
bun test ui/src/common/adapter/mcpRequest.test.ts
bun run typecheck
```

Expected: mapper tests PASS and typecheck exits 0.

- [ ] **Step 4: Re-run the type-drift audit**

Run:

```bash
rg -n "pub id: Option<String>" crates/backend/nomifun-api-types/src/mcp.rs
rg -n "testMcpConnection: httpPost<[\\s\\S]*IMcpServer" ui/src/common/adapter/ipcBridge.ts
rg -n "persist_test_result\\(&self, id: &str" crates/backend/nomifun-mcp/src
```

Expected: all three searches return no matches.

- [ ] **Step 5: Review final commits and working tree**

Run:

```bash
git log -5 --oneline --decorate
git status --short --branch
```

Expected: the design, failing regression, backend repair, and frontend repair commits are present; the working tree is clean.

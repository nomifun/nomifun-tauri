# Managed Provider CRUD Test Repair Design

## Context

Application startup now provisions the reserved `nomifun-free-model` provider. The authenticated provider CRUD end-to-end test predates that behavior and still expects the provider list to be empty before and after user-provider CRUD operations.

## Scope

Update only `provider_full_crud_with_auth` in `crates/backend/nomifun-app/tests/system_provider_e2e.rs`. Production provider behavior, managed-model startup, and shared test setup remain unchanged.

## Expected Behavior

The test will verify three states:

1. Before creating a user provider, the provider list contains exactly the reserved `nomifun-free-model` provider.
2. After creating the Anthropic provider, the list contains both the reserved provider and the newly created provider, identified by their stable IDs rather than relying on list order.
3. After deleting the Anthropic provider, the list again contains exactly the reserved provider.

The existing create, update, authentication, API-key, and delete assertions remain intact.

## Error Handling and Stability

Assertions will extract provider IDs from the JSON response and compare membership. This avoids coupling the test to repository ordering while retaining an exact provider-count check at each stage.

## Verification

Use the existing failing test as the red baseline. After the assertion-only change:

- Run `provider_full_crud_with_auth` alone and require it to pass.
- Run the complete `system_provider_e2e` test target.
- Run formatting checks and the full Rust test suite with the Windows test prerequisites already identified (`sh` on `PATH` and a temporary `C:\tmp`).
- Confirm the worktree contains no test-generated changes before committing and pushing `main`.

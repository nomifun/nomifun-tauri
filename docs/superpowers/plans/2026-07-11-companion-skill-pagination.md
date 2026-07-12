# Companion Skill Pagination Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add server-side pagination and configurable page size to the companion skills tab.

**Architecture:** The store returns a matching skill slice plus its count. The service enriches only that slice with descriptions, the route exposes the page response, and the UI requests the selected page and renders the existing Arco pagination control.

**Tech Stack:** Rust, SQLx/SQLite, Axum, React, TypeScript, Arco Design.

## Global Constraints

- Default UI page size is 10; permitted UI sizes are 10, 20, and 50.
- Server limits are clamped to 1–500 and offsets cannot be negative.
- Status filtering and counting use the same scope predicate.
- Preserve direct edits on the user-authorized `main` branch.

---

### Task 1: Specify and test the store/service pagination contract

**Files:**

- Modify: `crates/backend/nomifun-companion/src/store.rs`
- Modify: `crates/backend/nomifun-companion/src/service.rs`

**Interfaces:**

- Produces: `CompanionSkillPage { items: Vec<CompanionSkill>, total: i64 }` from the store.
- Produces: `CompanionSkillViewPage { items: Vec<CompanionSkillView>, total: i64 }` from the service.

- [ ] **Step 1: Write failing tests**

```rust
let page = store.list_skill_page(&cid, true, Some("active"), 1, 1).await.unwrap();
assert_eq!(page.total, 2);
assert_eq!(page.items.len(), 1);
assert_eq!(page.items[0].skill_name, "second-active");
```

- [ ] **Step 2: Run the focused tests and verify they fail because the page API does not exist**

Run: `cargo test -p nomifun-companion --lib store::tests::list_skill_page_filters_counts_and_pages service::tests::list_companion_skill_page_enriches_only_current_page -- --exact`

- [ ] **Step 3: Add the minimal page structs and SQL queries**

```rust
pub async fn list_skill_page(&self, companion_id: &str, include_shared: bool, status: Option<&str>, limit: i64, offset: i64) -> Result<CompanionSkillPage, AppError>
```

Count and select queries must share visibility and optional status predicates, sort by `strength DESC`, and select with `LIMIT ? OFFSET ?`.

- [ ] **Step 4: Enrich only `page.items` with descriptions in the service**

```rust
pub async fn list_companion_skill_page(&self, companion_id: &str, include_shared: bool, status: Option<&str>, limit: i64, offset: i64) -> Result<CompanionSkillViewPage, AppError>
```

- [ ] **Step 5: Run the focused tests and verify they pass**

Run: `cargo test -p nomifun-companion --lib store::tests::list_skill_page_filters_counts_and_pages service::tests::list_companion_skill_page_enriches_only_current_page -- --exact`

### Task 2: Expose the page response and consume it in the skill tab

**Files:**

- Modify: `crates/backend/nomifun-companion/src/routes.rs`
- Modify: `ui/src/common/adapter/ipcBridge.ts`
- Modify: `ui/src/renderer/pages/nomi/tabs/SkillsTab.tsx`

**Interfaces:**

- Consumes: `list_companion_skill_page`.
- Produces: `ICompanionSkillPage { items: ICompanionSkill[]; total: number }` in the frontend bridge.

- [ ] **Step 1: Make the route accept `status`, `limit`, and `offset`**

```rust
struct ListSkillsQuery { include_shared: Option<bool>, status: Option<String>, limit: Option<i64>, offset: Option<i64> }
```

Return `ApiResponse<CompanionSkillViewPage>`.

- [ ] **Step 2: Update the bridge query serializer**

```ts
listSkills: httpGet<ICompanionSkillPage, { companion_id: string; include_shared?: boolean; status?: string; limit?: number; offset?: number }>(...)
```

- [ ] **Step 3: Request the active page and render Arco `Pagination`**

Keep `page`, `pageSize`, and `total` in `SkillsTab`. Reset page on companion/filter/page-size changes, preserve stale-response protection, and set the final valid page when a mutation removes the only item on a trailing page.

- [ ] **Step 4: Verify TypeScript**

Run: `cd ui && bun run typecheck`

### Task 3: Final verification and commit

**Files:**

- Modify: the files above.

- [ ] **Step 1: Format and run the full companion library suite**

Run: `cargo fmt --check -p nomifun-companion && cargo test -p nomifun-companion --lib`

- [ ] **Step 2: Run UI validation**

Run: `cd ui && bun run typecheck`

- [ ] **Step 3: Inspect the final diff and commit**

Run: `git diff --check && git status --short`

Commit: `feat(companion): paginate skills`

## Review checklist

- The request uses 10 by default and offers 20/50.
- Every status filter has matching `items` and `total` values.
- A skills refresh never leaves an empty, out-of-range trailing page.
- Existing skill interactions retain their callbacks and error behavior.

# 桌面伙伴建议列表分页 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 为桌面伙伴“共享 / 建议”列表增加筛选一致的服务端分页，默认 10 条并允许 20/50 条切换。

**Architecture:** `CompanionStore` 新增建议页面查询，按同一状态条件查询当前页与总数；HTTP 路由和 TypeScript 桥接暴露页面对象。`SuggestionsTab` 维护页码、页大小和总数，将状态筛选、实时事件与决策操作接到分页刷新流程。

**Tech Stack:** Rust、Axum、SQLx/SQLite、TypeScript、React 19、Arco Design、Bun test。

## Global Constraints

- 默认页大小为 10，用户只能切换至 10、20、50。
- 页面查询和总数查询必须使用同一个可选 `status` 条件，排序固定为 `created_at DESC`。
- 状态筛选和页大小变更必须回到第 1 页；决策后若页码超界必须回到最后有效页。
- 保持建议卡片、状态定义和采纳/忽略的副作用不变。
- 不增加搜索、排序、批量操作或全量前端加载。

---

### Task 1: 实现并验证建议存储分页

**Files:**
- Modify: `crates/backend/nomifun-companion/src/store.rs:1550-1565,2680-2770`

**Interfaces:**
- Consumes: `status: Option<&str>`, `limit: i64`, `offset: i64`。
- Produces: `SuggestionPage { items: Vec<CompanionSuggestion>, total: i64 }` 和 `CompanionStore::list_suggestion_page`。

- [ ] **Step 1: 写出失败的状态分页测试**

在建议存储测试内创建两个 `new` 建议和一个已决策建议，然后验证只统计待处理建议，并按偏移量返回单条：

```rust
let page = store.list_suggestion_page(Some("new"), 1, 1).await.unwrap();

assert_eq!(page.total, 2);
assert_eq!(page.items.len(), 1);
assert_eq!(page.items[0].status, "new");
```

- [ ] **Step 2: 运行测试，确认缺少页面查询而失败**

Run: `CARGO_TARGET_DIR=/tmp/nomifun-suggestion-target CARGO_BUILD_BUILD_DIR=/tmp/nomifun-suggestion-build cargo test -p nomifun-companion list_suggestion_page_counts_the_same_status`

Expected: FAIL，提示 `list_suggestion_page` 方法不存在。

- [ ] **Step 3: 实现最小页面对象与同筛选查询**

在 `CompanionSuggestion` 附近添加：

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SuggestionPage {
    pub items: Vec<CompanionSuggestion>,
    pub total: i64,
}
```

实现：

```rust
pub async fn list_suggestion_page(
    &self,
    status: Option<&str>,
    limit: i64,
    offset: i64,
) -> Result<SuggestionPage, AppError>
```

当 `status` 存在时，页面 SQL 使用 `WHERE status = ? ORDER BY created_at DESC LIMIT ? OFFSET ?`，总数 SQL 使用 `WHERE status = ?`；否则两者都不加 `WHERE`。页大小绑定 `limit.clamp(1, 500)`，偏移量绑定 `offset.max(0)`。保留现有 `list_suggestions`，避免学习器与其它内部调用额外执行计数查询。

- [ ] **Step 4: 运行新测试与现有建议回归**

Run: `CARGO_TARGET_DIR=/tmp/nomifun-suggestion-target CARGO_BUILD_BUILD_DIR=/tmp/nomifun-suggestion-build cargo test -p nomifun-companion list_suggestion_page_counts_the_same_status`

Expected: PASS，测试证明总数和页内容都只包含请求状态。

- [ ] **Step 5: 提交存储层变更**

```bash
git add crates/backend/nomifun-companion/src/store.rs
git commit -m "feat(companion): page suggestions by status"
```

### Task 2: 暴露建议分页 HTTP 与桥接契约

**Files:**
- Modify: `crates/backend/nomifun-companion/src/service.rs:1004-1008`
- Modify: `crates/backend/nomifun-companion/src/routes.rs:229-250`
- Modify: `ui/src/common/adapter/ipcBridge.ts:3014-3024,3310-3320`
- Create: `ui/src/common/adapter/ipcBridge.companion-suggestion-pagination.test.ts`

**Interfaces:**
- Consumes: `CompanionService::list_suggestion_page(status, limit, offset)`。
- Produces: `GET /api/companion/suggestions -> ApiResponse<SuggestionPage>` 和 `ICompanionSuggestionPage { items, total }`。

- [ ] **Step 1: 写出失败的桥接契约测试**

新增 Bun 源码契约测试：

```ts
expect(source.includes('export interface ICompanionSuggestionPage')).toBe(true);
expect(source.includes('items: ICompanionSuggestion[];')).toBe(true);
expect(source.includes('total: number;')).toBe(true);
expect(/listSuggestions: httpGet<\s*ICompanionSuggestionPage,/.test(source)).toBe(true);
expect(source.includes('offset?: number')).toBe(true);
```

- [ ] **Step 2: 运行测试，确认缺少页面类型而失败**

Run: `cd ui && bun test src/common/adapter/ipcBridge.companion-suggestion-pagination.test.ts`

Expected: FAIL，缺少建议页面类型或 `offset` 请求参数。

- [ ] **Step 3: 贯通服务、路由与 TypeScript 页面类型**

在服务层增加委托：

```rust
pub async fn list_suggestion_page(
    &self,
    status: Option<&str>,
    limit: i64,
    offset: i64,
) -> Result<SuggestionPage, AppError> {
    self.store.list_suggestion_page(status, limit, offset).await
}
```

为路由查询结构添加 `offset: Option<i64>`，并改为返回 `ApiResponse<SuggestionPage>`。前端新增：

```ts
export interface ICompanionSuggestionPage {
  items: ICompanionSuggestion[];
  total: number;
}
```

把 `listSuggestions` 返回类型改为页面类型，并为请求参数与 URLSearchParams 添加 `offset`。

- [ ] **Step 4: 验证桥接契约与 Rust 测试**

Run: `cd ui && bun test src/common/adapter/ipcBridge.companion-suggestion-pagination.test.ts`

Run: `CARGO_TARGET_DIR=/tmp/nomifun-suggestion-target CARGO_BUILD_BUILD_DIR=/tmp/nomifun-suggestion-build cargo test -p nomifun-companion --lib`

Expected: 两项命令均通过。

- [ ] **Step 5: 提交 API 契约**

```bash
git add crates/backend/nomifun-companion/src/service.rs crates/backend/nomifun-companion/src/routes.rs ui/src/common/adapter/ipcBridge.ts ui/src/common/adapter/ipcBridge.companion-suggestion-pagination.test.ts
git commit -m "feat(companion): expose suggestion page totals"
```

### Task 3: 将建议列表接入分页状态与页脚

**Files:**
- Modify: `ui/src/renderer/pages/nomi/tabs/SuggestionsTab.tsx`
- Create: `ui/src/renderer/pages/nomi/tabs/SuggestionsTab.test.ts`

**Interfaces:**
- Consumes: `ipcBridge.companion.listSuggestions.invoke({ status, limit, offset }) -> ICompanionSuggestionPage`。
- Produces: 默认 10 条、10/20/50 选择、状态切换重置和空页回退的建议列表。

- [ ] **Step 1: 写出失败的建议页分页结构测试**

读取页面源码并断言：

```ts
expect(source.includes('const [page, setPage] = useState(1);')).toBe(true);
expect(source.includes('const [pageSize, setPageSize] = useState(10);')).toBe(true);
expect(source.includes('limit: pageSize')).toBe(true);
expect(source.includes('offset: (page - 1) * pageSize')).toBe(true);
expect(source.includes('<Pagination')).toBe(true);
expect(source.includes('sizeOptions={[10, 20, 50]}')).toBe(true);
```

- [ ] **Step 2: 运行测试，确认旧页面没有分页状态而失败**

Run: `cd ui && bun test src/renderer/pages/nomi/tabs/SuggestionsTab.test.ts`

Expected: FAIL，缺少 `page`、`Pagination` 或分页请求参数。

- [ ] **Step 3: 实现最小的分页 UI 状态**

添加 `page`、`pageSize`、`total` 和安全最大页码。请求使用：

```ts
limit: pageSize,
offset: (page - 1) * pageSize,
```

从响应更新 `items` 与 `total`。用 effect 在 `filter` 和 `pageSize` 变化时 `setPage(1)`；`Pagination.onChange` 在页大小变化时设置新页大小并回到第 1 页。若返回总数使当前页超过最大页，设置最后有效页并由刷新 effect 重新获取。

首次加载保留 Spinner；后续翻页保持既有卡片并以透明度表示加载。筛选结果大于零时在页脚添加：

```tsx
<Pagination
  current={page}
  pageSize={pageSize}
  total={total}
  showTotal
  sizeCanChange
  sizeOptions={[10, 20, 50]}
  showJumper={total > pageSize}
  onChange={handlePageChange}
/>
```

采纳或忽略继续调用既有 `decide`，随后刷新当前页；现有导航和错误反馈不变。

- [ ] **Step 4: 运行页面测试与类型检查**

Run: `cd ui && bun test src/renderer/pages/nomi/tabs/SuggestionsTab.test.ts src/common/adapter/ipcBridge.companion-suggestion-pagination.test.ts`

Run: `cd ui && bun run typecheck`

Expected: 测试和类型检查均通过。

- [ ] **Step 5: 提交建议页体验**

```bash
git add ui/src/renderer/pages/nomi/tabs/SuggestionsTab.tsx ui/src/renderer/pages/nomi/tabs/SuggestionsTab.test.ts
git commit -m "feat(companion): paginate suggestions"
```

### Task 4: 完整回归与交付检查

**Files:**
- Verify: `crates/backend/nomifun-companion/src/store.rs`
- Verify: `crates/backend/nomifun-companion/src/routes.rs`
- Verify: `ui/src/common/adapter/ipcBridge.ts`
- Verify: `ui/src/renderer/pages/nomi/tabs/SuggestionsTab.tsx`

**Interfaces:**
- Consumes: 建议分页 API 与前端状态。
- Produces: 已验证且不包含格式错误的交付实现。

- [ ] **Step 1: 运行后端完整库测试**

Run: `CARGO_TARGET_DIR=/tmp/nomifun-suggestion-target CARGO_BUILD_BUILD_DIR=/tmp/nomifun-suggestion-build cargo test -p nomifun-companion --lib`

Expected: PASS，全部 companion 库测试通过。

- [ ] **Step 2: 运行前端分页回归与类型检查**

Run: `cd ui && bun test src/renderer/pages/nomi/tabs/SuggestionsTab.test.ts src/common/adapter/ipcBridge.companion-suggestion-pagination.test.ts`

Run: `cd ui && bun run typecheck`

Expected: PASS，桥接与页面分页契约、TypeScript 检查均通过。

- [ ] **Step 3: 检查格式与变更边界**

Run: `cargo fmt --check -p nomifun-companion`

Run: `git diff --check && git status --short`

Expected: 无 Rust 格式或 Git 空白错误；仅包含本功能的文件。

## Plan Self-Review

- Spec coverage: Task 1 覆盖状态一致的页面和总数；Task 2 覆盖 API 和桥接；Task 3 覆盖默认页大小、页大小切换、状态重置与超页回退；Task 4 覆盖完整验证。
- Placeholder scan: 每项任务均给出目标文件、接口、失败测试、执行命令及通过条件；没有待定范围。
- Type consistency: 后端使用 `SuggestionPage`，前端使用 `ICompanionSuggestionPage`；`page` 为 1 基，`offset` 固定为 `(page - 1) * pageSize`。

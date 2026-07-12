# 桌面伙伴记忆列表分页与层级优化 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 为桌面伙伴的记忆标签提供带总数的服务端分页，并将长列表重构为内容优先、操作收纳的专业列表。

**Architecture:** `CompanionStore` 生成与筛选条件一致的页面数据与总数，路由将该页面对象直接传给 UI。`MemoriesTab` 保存页码和页大小，以已有筛选条件加上 `limit`/`offset` 请求这一页面；它将置顶作为高频图标操作，将编辑、归档和删除收进菜单，再在底部复用 Arco Pagination。

**Tech Stack:** Rust、Axum、SQLx/SQLite、TypeScript、React 19、Arco Design、Bun test。

## Global Constraints

- 默认页大小为 10，用户只可在 10、20、50 间切换。
- `kind`、`q`、`status`、`scope_companion_id` 必须同时应用于页面查询和总数查询。
- 排序必须保持 `pinned DESC, strength DESC, updated_at DESC`。
- 筛选、搜索、范围和页大小变更统一回到第 1 页；写操作或实时事件刷新当前页。
- 不新增批量操作、前端全量加载、排序切换或记忆字段。

---

### Task 1: 在存储层交付可验证的筛选分页结果

**Files:**
- Modify: `crates/backend/nomifun-companion/src/store.rs:150-162,1026-1058,2475-2494`

**Interfaces:**
- Consumes: `MemoryFilter { kind, q, status, scope_companion_id, limit, offset }`。
- Produces: `MemoryPage { items: Vec<CompanionMemory>, total: i64 }` 及 `CompanionStore::list_memory_page(&MemoryFilter)`。

- [ ] **Step 1: 写出失败的存储层分页测试**

在现有 `list_memories_scope_filter_excludes_other_private` 后新增测试，插入一条共享记忆、两条 `c1` 私有记忆及一条 `c2` 私有记忆，然后使用 `scope_companion_id: Some("c1".into()), limit: 2, offset: 1` 断言：

```rust
let page = store.list_memory_page(&MemoryFilter {
    scope_companion_id: Some("c1".into()),
    limit: 2,
    offset: 1,
    ..Default::default()
}).await.unwrap();

assert_eq!(page.total, 3);
assert_eq!(page.items.len(), 2);
assert!(page.items.iter().all(|m| m.scope_companion_id != "c2"));
```

- [ ] **Step 2: 运行测试，确认因缺少 API 而失败**

Run: `cargo test -p nomifun-companion list_memory_page_counts_the_same_filtered_scope`

Expected: 编译失败，提示 `list_memory_page` 方法不存在。

- [ ] **Step 3: 实现同条件的页面与计数查询**

在 `MemoryFilter` 后声明：

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemoryPage {
    pub items: Vec<CompanionMemory>,
    pub total: i64,
}
```

将现有 `list_memories` 的条件生成和参数绑定提取为一个私有的、可复用的 SQL 条件片段，使 `SELECT * ... ORDER BY ... LIMIT ? OFFSET ?` 和 `SELECT COUNT(*) AS n ...` 使用同一 `kind`、`content LIKE`、`status`、共享加所属伙伴可见性条件。`list_memory_page` 返回：

```rust
Ok(MemoryPage {
    items: rows.iter().map(row_to_memory).collect(),
    total: count_row.get("n"),
})
```

保留 `list_memories`，让现有学习和注入调用继续取得 `Vec<CompanionMemory>`；其实现可委托给 `list_memory_page` 后返回 `.items`。

- [ ] **Step 4: 运行新增测试和存储层现有回归**

Run: `cargo test -p nomifun-companion 'list_memories_scope_filter_excludes_other_private|list_memory_page_counts_the_same_filtered_scope'`

Expected: 两个测试通过，确认分页总数与范围过滤一致且原可见性规则未变。

- [ ] **Step 5: 提交存储层变更**

```bash
git add crates/backend/nomifun-companion/src/store.rs
git commit -m "feat(companion): page filtered memories"
```

### Task 2: 将分页对象贯通 HTTP 与前端桥接类型

**Files:**
- Modify: `crates/backend/nomifun-companion/src/service.rs:911-913`
- Modify: `crates/backend/nomifun-companion/src/routes.rs:19,154-167`
- Modify: `ui/src/common/adapter/ipcBridge.ts:2995-3012,3286-3300`

**Interfaces:**
- Consumes: `CompanionStore::list_memory_page(&MemoryFilter)`。
- Produces: `GET /api/companion/memories -> ApiResponse<MemoryPage>` 与 `ICompanionMemoryPage { items, total }`。

- [ ] **Step 1: 写出失败的桥接契约测试**

创建 `ui/src/common/adapter/ipcBridge.companion-memory-pagination.test.ts`，读取桥接源码并断言：

```ts
expect(source).toContain('export interface ICompanionMemoryPage');
expect(source).toContain('items: ICompanionMemory[];');
expect(source).toContain('total: number;');
expect(source).toContain('listMemories: httpGet<ICompanionMemoryPage');
```

- [ ] **Step 2: 运行测试，确认缺少分页接口而失败**

Run: `cd ui && bun test src/common/adapter/ipcBridge.companion-memory-pagination.test.ts`

Expected: FAIL，找不到 `ICompanionMemoryPage` 或新的 `listMemories` 返回类型。

- [ ] **Step 3: 替换 HTTP 返回类型并在服务层转发页面对象**

在 Rust 路由中导入 `MemoryPage`，并将签名及返回体改为：

```rust
) -> Result<Json<ApiResponse<MemoryPage>>, AppError> {
    // 保持现有 filter 构建不变
    Ok(Json(ApiResponse::ok(state.service.list_memory_page(&filter).await?)))
}
```

在 `CompanionService` 添加 `list_memory_page`，只委托给存储层。前端在 `ICompanionMemory` 后声明：

```ts
export interface ICompanionMemoryPage {
  items: ICompanionMemory[];
  total: number;
}
```

并把 `listMemories` 的 `httpGet` 成功类型从数组改为 `ICompanionMemoryPage`；请求参数不变。

- [ ] **Step 4: 验证桥接契约与 Rust 包编译**

Run: `cd ui && bun test src/common/adapter/ipcBridge.companion-memory-pagination.test.ts && cd .. && cargo test -p nomifun-companion --lib`

Expected: 桥接测试和 companion crate 的库测试均通过。

- [ ] **Step 5: 提交 API 契约变更**

```bash
git add crates/backend/nomifun-companion/src/service.rs crates/backend/nomifun-companion/src/routes.rs ui/src/common/adapter/ipcBridge.ts ui/src/common/adapter/ipcBridge.companion-memory-pagination.test.ts
git commit -m "feat(companion): expose memory page totals"
```

### Task 3: 实现记忆页的分页状态、密度与菜单操作

**Files:**
- Create: `ui/src/renderer/pages/nomi/tabs/MemoriesTab.test.ts`
- Modify: `ui/src/renderer/pages/nomi/tabs/MemoriesTab.tsx`
- Modify: `ui/src/renderer/services/i18n/locales/zh-CN/nomi.json:131-157`
- Modify: `ui/src/renderer/services/i18n/locales/en-US/nomi.json:131-157`

**Interfaces:**
- Consumes: `ipcBridge.companion.listMemories.invoke({ limit, offset, kind, q, status, scope_companion_id }) -> ICompanionMemoryPage`。
- Produces: 10 条默认页、10/20/50 条切换、带 `total` 的 Arco Pagination、内容优先的两层记忆行。

- [ ] **Step 1: 写出失败的列表分页结构测试**

以同目录既有 Bun 源码结构测试的方式创建 `MemoriesTab.test.ts`，读取 `MemoriesTab.tsx` 并断言：

```ts
expect(source).toContain("const [page, setPage] = useState(1)");
expect(source).toContain("const [pageSize, setPageSize] = useState(10)");
expect(source).toContain('limit: pageSize');
expect(source).toContain('offset: (page - 1) * pageSize');
expect(source).toContain('<Pagination');
expect(source).toContain('sizeCanChange');
expect(source).toContain('pageSizeOptions={[10, 20, 50]}');
expect(source).toContain('<Dropdown');
```

- [ ] **Step 2: 运行测试，确认当前列表缺少分页与收纳菜单**

Run: `cd ui && bun test src/renderer/pages/nomi/tabs/MemoriesTab.test.ts`

Expected: FAIL，缺少分页状态、`Pagination` 或 `Dropdown`。

- [ ] **Step 3: 以最小状态实现服务端分页与专业列表层级**

从 Arco 导入 `Dropdown`、`Menu`、`Pagination`，以 `More` 图标替代三个并列文字按钮。新增状态及安全页码计算：

```ts
const [page, setPage] = useState(1);
const [pageSize, setPageSize] = useState(10);
const [total, setTotal] = useState(0);
const maxPage = Math.max(1, Math.ceil(total / pageSize));
```

在请求中使用 `limit: pageSize` 与 `offset: (page - 1) * pageSize`，并从返回对象更新 `memories` 与 `total`。依赖筛选条件的 effect 先 `setPage(1)`；翻页只更新 `page`。若刷新结果为空但 `total > 0` 且 `page > 1`，设置到 `maxPage` 后由 effect 再取一次。

每行正文使用 `line-clamp-2`，将标签、置顶图标与正文放在第一行；范围、强度、更新时间、来源收纳到第二行。菜单内容为编辑、归档/恢复及嵌套 `Popconfirm` 的危险删除。分页区使用：

```tsx
<Pagination
  current={page}
  pageSize={pageSize}
  total={total}
  showTotal
  sizeCanChange
  sizeOptions={[10, 20, 50]}
  showJumper={total > pageSize}
  onChange={(nextPage, nextPageSize) => {
    if (nextPageSize !== pageSize) setPageSize(nextPageSize);
    setPage(nextPageSize !== pageSize ? 1 : nextPage);
  }}
/>
```

在中英文 `memories` 命名空间添加总数显示、更多操作和分页总数需要的准确文案。加载后翻页时保持列表在 DOM 中并用 `opacity` 表达进行中状态；首次加载仍显示当前密度的骨架行。

- [ ] **Step 4: 运行 UI 测试和类型检查**

Run: `cd ui && bun test src/renderer/pages/nomi/tabs/MemoriesTab.test.ts src/common/adapter/ipcBridge.companion-memory-pagination.test.ts && bun run typecheck`

Expected: 两个 Bun 测试通过，TypeScript 零错误。

- [ ] **Step 5: 提交列表体验优化**

```bash
git add ui/src/renderer/pages/nomi/tabs/MemoriesTab.tsx ui/src/renderer/pages/nomi/tabs/MemoriesTab.test.ts ui/src/renderer/services/i18n/locales/zh-CN/nomi.json ui/src/renderer/services/i18n/locales/en-US/nomi.json
git commit -m "feat(companion): paginate and streamline memories"
```

### Task 4: 完整回归与交付检查

**Files:**
- Verify: `crates/backend/nomifun-companion/src/store.rs`
- Verify: `crates/backend/nomifun-companion/src/routes.rs`
- Verify: `ui/src/common/adapter/ipcBridge.ts`
- Verify: `ui/src/renderer/pages/nomi/tabs/MemoriesTab.tsx`

**Interfaces:**
- Consumes: 已合并的分页 API 和 UI 状态。
- Produces: 无格式错误、无类型错误、分页契约稳定的可交付实现。

- [ ] **Step 1: 运行后端分页回归**

Run: `cargo test -p nomifun-companion --lib`

Expected: PASS，存储分页、范围过滤和既有 companion 单元测试全部通过。

- [ ] **Step 2: 运行前端静态与类型回归**

Run: `cd ui && bun test src/renderer/pages/nomi/tabs/MemoriesTab.test.ts src/common/adapter/ipcBridge.companion-memory-pagination.test.ts && bun run typecheck`

Expected: PASS，页面结构契约、桥接契约和 TypeScript 类型检查均通过。

- [ ] **Step 3: 审查变更边界**

Run: `git diff --check HEAD~3..HEAD && git status --short`

Expected: 无空白错误，工作区仅包含本功能预期的文件。

- [ ] **Step 4: 提交最终验证记录（若本阶段引入验证文档）**

```bash
git status --short
```

Expected: 无额外验证文件时保持干净；不为仅运行测试创建无价值提交。

## Plan Self-Review

- Spec coverage: Task 1 和 2 覆盖同筛选条件的服务端总数契约；Task 3 覆盖默认 10 条、10/20/50 切换、筛选回第一页、超页回退、两层信息与收纳操作；Task 4 覆盖回归验证。
- Placeholder scan: 无待定范围、泛化的“适当处理”说明或未定义接口；每个测试、命令和文件路径均已指定。
- Type consistency: 后端统一使用 `MemoryPage`，前端统一使用 `ICompanionMemoryPage`；`page` 是 1 基，`offset` 始终为 `(page - 1) * pageSize`。

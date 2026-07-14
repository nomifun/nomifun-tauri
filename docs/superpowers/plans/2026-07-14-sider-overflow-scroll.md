# 应用侧栏溢出滚动 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 使应用级左侧导航在内容超过窗口高度时可纵向滚动，同时固定底部设置区。

**Architecture:** 侧栏已经由外层 column flex 布局约束高度，主导航容器也已有 `flex-1 min-h-0`。将该容器的裁切改为仅纵向自动滚动即可建立正确滚动边界；固定设置区维持 `shrink-0`，不改动路由、数据或导航项。

**Tech Stack:** React 19、TypeScript、UnoCSS utility classes、Bun test。

## Global Constraints

- 只修改应用级 `Sider` 的滚动布局及其结构回归测试。
- 保持底部设置组固定，禁止主导航横向滚动。
- 不新增依赖、状态、文案、路由或业务逻辑。

---

### Task 1: 为侧栏主导航恢复可滚动访问

**Files:**
- Create: `ui/src/renderer/components/layout/Sider/siderOverflowScroll.test.ts`
- Modify: `ui/src/renderer/components/layout/Sider/index.tsx:156`

**Interfaces:**
- Consumes: `Sider` 外层既有的 column flex 高度约束，以及底部设置组的 `shrink-0` 布局。
- Produces: 主导航容器可通过浏览器原生滚轮、触控板、键盘聚焦与滚动条访问溢出项目。

- [ ] **Step 1: Write the failing structural regression test**

```ts
import { readFileSync } from 'node:fs';
import { describe, expect, test } from 'bun:test';

const source = readFileSync(new URL('./index.tsx', import.meta.url), 'utf8');

describe('application sider overflow handling', () => {
  test('scrolls the navigation body while keeping the settings group pinned', () => {
    expect(source).toContain("'flex-1 min-h-0 overflow-y-auto overflow-x-hidden'");
    expect(source).toContain("'shrink-0 mt-auto pt-8px flex flex-col gap-2px");
  });
});
```

- [ ] **Step 2: Run the regression test and confirm it fails on the current clipped container**

Run: `bun test ui/src/renderer/components/layout/Sider/siderOverflowScroll.test.ts`

Expected: FAIL because `index.tsx` still contains `overflow-hidden` instead of `overflow-y-auto overflow-x-hidden`.

- [ ] **Step 3: Apply the minimal layout change**

```tsx
<div className='flex-1 min-h-0 overflow-y-auto overflow-x-hidden'>
```

Replace the existing main-content container class only. Do not change its child navigation entries or the bottom `shrink-0` group.

- [ ] **Step 4: Run the regression test and typecheck**

Run: `bun test ui/src/renderer/components/layout/Sider/siderOverflowScroll.test.ts && bun run --filter=./ui typecheck`

Expected: both commands exit with code 0.

- [ ] **Step 5: Review the scoped diff**

Run: `git diff --check && git diff -- ui/src/renderer/components/layout/Sider/index.tsx ui/src/renderer/components/layout/Sider/siderOverflowScroll.test.ts`

Expected: only the main navigation overflow utilities and its regression test are present.

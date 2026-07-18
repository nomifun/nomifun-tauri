# 新建知识库桌面端滚动修复 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 修复桌面端新建知识库弹窗在小窗口下滚动到底仍显示不全、窗口尺寸变化后滚动条消失的问题。

**Architecture:** 为弹窗内部 column flex 根节点提供确定的动态视口高度，让头部和底部保持固定；将中间桌面 Grid 的行显式设为可收缩轨道，并让右侧配置面板独立负责纵向滚动。该方案只调整布局约束，不引入运行时监听或业务状态。

**Tech Stack:** React 19、TypeScript、CSS Grid、UnoCSS utility classes、Bun test。

## Global Constraints

- 只修改 `CreateStudio` 的滚动布局、结构回归测试和对应设计/计划文档。
- 保持头部与底部操作区固定，仅中间配置内容滚动。
- 不新增依赖、状态、监听器、文案或业务逻辑。

---

### Task 1: 建立桌面端小窗口滚动约束

**Files:**
- Create: `ui/src/renderer/pages/knowledge/createStudioScrollLayout.test.ts`
- Modify: `ui/src/renderer/pages/knowledge/CreateStudio/index.tsx`

- [ ] **Step 1: Write the failing structural regression test**

断言弹窗根节点使用确定的 `100dvh` 高度、桌面 Grid 行使用 `minmax(0, 1fr)`，且配置面板包含 `min-h-0 overflow-y-auto`。

- [ ] **Step 2: Run the regression test and confirm it fails**

Run: `bun test ui/src/renderer/pages/knowledge/createStudioScrollLayout.test.ts`

Expected: FAIL，因为现状只有 `maxHeight`，Grid 仍使用隐式 auto 行。

- [ ] **Step 3: Apply the minimal layout change**

新增响应式弹窗高度常量；根节点同时使用确定的 `height`/`maxHeight`；桌面 Grid 使用可收缩行列轨道；配置面板补充 `min-h-0`。

- [ ] **Step 4: Run focused and related tests**

Run: `bun test ui/src/renderer/pages/knowledge/createStudioScrollLayout.test.ts`

Run: `bun test ui/src/renderer/pages/knowledge`

Expected: 两组测试均通过。

- [ ] **Step 5: Run static and build verification**

Run: `bun run typecheck`

Run: `bun run build:ui`

Expected: 类型检查与生产构建均成功。

- [ ] **Step 6: Verify the original desktop symptom**

在桌面端将窗口高度缩小后打开新建知识库弹窗，滚动右侧配置区到底，确认教学卡片完整位于固定底部操作区上方；再次调整窗口高度，确认滚动条按内容正确保留或消失。

- [ ] **Step 7: Review, commit, and push**

Run: `git diff --check`，审查限定文件，然后提交并执行 `git push origin main`。

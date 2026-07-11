# Workspace Tool Rail Right Edge Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Keep the desktop workspace tool rail at the window's right edge when its panel is expanded and remove the redundant desktop titlebar toggle.

**Architecture:** Reorder existing desktop JSX siblings so the expandable panel renders before `WorkspaceToolRail` in both conversation and terminal layouts. Keep the existing titlebar workspace event path for mobile, but restrict its button to mobile layouts.

**Tech Stack:** React 19, TypeScript, CSS flex layout, Bun test runner.

## Global Constraints

- Apply the right-edge ordering to both desktop conversation and terminal workspace layouts.
- Preserve the existing 32px rail width and 48px tool-entry height.
- Hide the titlebar workspace toggle on desktop and keep it on mobile.
- Preserve panel resizing, tabs, active states, badges, and the rail footer toggle.
- Do not change mobile overlay or slide-out behavior.

---

### Task 1: Lock the desired workspace layout structure

**Files:**
- Modify: `ui/src/renderer/pages/conversation/components/ChatLayout/advancedControls.test.ts`
- Modify: `ui/src/renderer/pages/terminal/TerminalSessionPage.structure.test.ts`
- Modify: `ui/src/renderer/components/layout/Titlebar/index.test.ts`

**Interfaces:**
- Consumes: source files for `ChatLayout`, `TerminalSessionPage`, and `Titlebar`.
- Produces: source-contract tests for sibling order and mobile-only titlebar visibility.

- [x] **Step 1: Add failing source-contract tests**

Add this test to `ChatLayout/advancedControls.test.ts`:

```ts
test('keeps the workspace tool rail at the far right of the expanded panel', () => {
  const source = readSource(new URL('./index.tsx', import.meta.url));
  const panelIndex = source.indexOf("className={classNames('!bg-1 relative chat-layout-right-sider layout-sider')}");
  const railIndex = source.indexOf('<WorkspaceToolRail');

  expect(panelIndex >= 0).toBe(true);
  expect(railIndex >= 0).toBe(true);
  expect(panelIndex < railIndex).toBe(true);
});
```

Add this test to `TerminalSessionPage.structure.test.ts`:

```ts
test('keeps the workspace tool rail at the far right of the expanded panel', () => {
  const panelIndex = source.indexOf("className='!bg-1 relative layout-sider'");
  const railIndex = source.indexOf('<WorkspaceToolRail');

  expect(panelIndex >= 0).toBe(true);
  expect(railIndex >= 0).toBe(true);
  expect(panelIndex < railIndex).toBe(true);
});
```

Add this test to `Titlebar/index.test.ts`:

```ts
test('shows the workspace titlebar toggle on mobile only', () => {
  expect(titlebarSource.includes('const showWorkspaceButton = workspaceAvailable && Boolean(layout?.isMobile);')).toBe(
    true
  );
});
```

- [x] **Step 2: Run the focused tests and verify they fail**

Run:

```bash
bun test \
  ui/src/renderer/pages/conversation/components/ChatLayout/advancedControls.test.ts \
  ui/src/renderer/pages/terminal/TerminalSessionPage.structure.test.ts \
  ui/src/renderer/components/layout/Titlebar/index.test.ts
```

Expected: the conversation and terminal order assertions fail because `WorkspaceToolRail` precedes the panel; the titlebar assertion fails because desktop platforms are included.

### Task 2: Move the rail to the right edge and remove the desktop titlebar entry

**Files:**
- Modify: `ui/src/renderer/pages/conversation/components/ChatLayout/index.tsx:413-492`
- Modify: `ui/src/renderer/pages/terminal/TerminalSessionPage.tsx:160-220`
- Modify: `ui/src/renderer/components/layout/Titlebar/index.tsx:118-130`

**Interfaces:**
- Consumes: existing `WorkspaceToolRail`, desktop workspace panel, and `layout.isMobile` state.
- Produces: desktop sibling order `main content → workspace panel → workspace tool rail`, plus a mobile-only titlebar toggle.

- [x] **Step 1: Reorder the conversation desktop siblings**

Move the existing unchanged block beginning with:

```tsx
{workspaceEnabled && !layout?.isMobile && (
  <WorkspaceToolRail
```

from before the mobile trigger and desktop `.chat-layout-right-sider` block to immediately after the closing `)}` of the desktop `.chat-layout-right-sider` block. Keep the `WorkspaceToolRail` props and footer button body unchanged.

- [x] **Step 2: Reorder the terminal desktop siblings**

Move the existing unchanged block beginning with:

```tsx
{!isMobile && (
  <WorkspaceToolRail
```

from before the terminal `.layout-sider` panel to immediately after that panel's closing `)}`. Keep the rail props and footer button unchanged.

- [x] **Step 3: Restrict the titlebar workspace button to mobile**

Replace the platform-specific titlebar condition with:

```ts
// Desktop workspace surfaces use the persistent far-right tool rail as their
// single toggle. Mobile keeps the titlebar entry because the rail is hidden.
const showWorkspaceButton = workspaceAvailable && Boolean(layout?.isMobile);
```

Remove `isWindows` from the platform import and delete `const isWinRuntime = ...`, because the desktop workspace condition no longer consumes them. Keep `isDesktopRuntime` and `isMacRuntime` for window controls and macOS spacing.

- [x] **Step 4: Run the focused tests and verify they pass**

Run:

```bash
bun test \
  ui/src/renderer/pages/conversation/components/ChatLayout/advancedControls.test.ts \
  ui/src/renderer/pages/terminal/TerminalSessionPage.structure.test.ts \
  ui/src/renderer/components/layout/Titlebar/index.test.ts
```

Expected: all focused tests pass with zero failures.

- [x] **Step 5: Run front-end type checking**

Run: `bun run typecheck`

Expected: exit code 0 with no TypeScript diagnostics.

- [x] **Step 6: Commit the implementation**

```bash
git add \
  ui/src/renderer/pages/conversation/components/ChatLayout/advancedControls.test.ts \
  ui/src/renderer/pages/conversation/components/ChatLayout/index.tsx \
  ui/src/renderer/pages/terminal/TerminalSessionPage.structure.test.ts \
  ui/src/renderer/pages/terminal/TerminalSessionPage.tsx \
  ui/src/renderer/components/layout/Titlebar/index.test.ts \
  ui/src/renderer/components/layout/Titlebar/index.tsx \
  docs/superpowers/plans/2026-07-11-workspace-tool-rail-right-edge.md
git commit -m "fix(ui): keep workspace tool rail at right edge"
```

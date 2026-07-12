# Workspace Tool Rail Visual Polish Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Hide visible tool labels, remove the active vertical bar, and make workspace tooltips smaller without changing rail dimensions or accessibility.

**Architecture:** Keep labels in the DOM as visually hidden accessible text while the icon remains the only visible button content. Use Arco Tooltip's `mini` mode plus a rail-scoped popup class so compact typography does not leak to other tooltips.

**Tech Stack:** React 19, TypeScript, CSS, Arco Design Tooltip, Bun test runner.

## Global Constraints

- Keep rail width `32px`, button width `28px`, button height `48px`, and icon size `18px`.
- Hide visible labels while retaining accessible button names.
- Remove only the active vertical bar; preserve active background, border, shadow, and icon color.
- Use `11px` tooltip text, `16px` line height, and `3px 6px` padding.
- Scope tooltip styling to workspace tool-rail popups only.

---

### Task 1: Lock and implement compact rail visuals

**Files:**
- Modify: `ui/src/renderer/pages/conversation/components/ChatLayout/workspaceToolRail.test.ts`
- Modify: `ui/src/renderer/pages/conversation/components/ChatLayout/WorkspaceToolRail.tsx`
- Modify: `ui/src/renderer/pages/conversation/components/ChatLayout/chat-layout.css`

**Interfaces:**
- Consumes: `ToolRailItem` label/icon props and Arco `TooltipProps.mini`/`className`.
- Produces: visually hidden accessible labels, no active pseudo-element bar, and a scoped compact tooltip.

- [x] **Step 1: Add failing structure and style assertions**

Extend `workspaceToolRail.test.ts` with a component source and these tests:

```ts
const componentSource = readFileSync(new URL('./WorkspaceToolRail.tsx', import.meta.url), 'utf8');

test('keeps labels accessible but visually hidden beneath icon-only controls', () => {
  const label = rule('\\.workspace-tool-rail__label');

  expect(componentSource.includes("className='workspace-tool-rail__label'")).toBe(true);
  expect(label.includes('position: absolute;')).toBe(true);
  expect(label.includes('width: 1px;')).toBe(true);
  expect(label.includes('height: 1px;')).toBe(true);
  expect(label.includes('overflow: hidden;')).toBe(true);
});

test('uses a compact scoped tooltip and removes the active vertical bar', () => {
  const tooltip = rule('\\.workspace-tool-rail__tooltip \\.arco-tooltip-content');

  expect(componentSource.includes("mini className='workspace-tool-rail__tooltip'")).toBe(true);
  expect(stylesheet.includes('.workspace-tool-rail__item--active::before')).toBe(false);
  expect(tooltip.includes('font-size: 11px;')).toBe(true);
  expect(tooltip.includes('line-height: 16px;')).toBe(true);
  expect(tooltip.includes('padding: 3px 6px;')).toBe(true);
});
```

- [x] **Step 2: Run the test and verify it fails**

Run: `bun test ui/src/renderer/pages/conversation/components/ChatLayout/workspaceToolRail.test.ts`

Expected: FAIL because labels are visible, the active pseudo-element exists, and the tooltip is not scoped or compact.

- [x] **Step 3: Enable the rail-specific compact tooltip**

Change `ToolRailItem` to wrap its button with:

```tsx
<Tooltip position='left' content={label} mini className='workspace-tool-rail__tooltip'>
```

Keep `<span className='workspace-tool-rail__label'>{label}</span>` in the button so it remains the accessible name.

- [x] **Step 4: Hide labels visually and remove the active bar**

Replace the label rule with:

```css
.workspace-tool-rail__label {
  position: absolute;
  width: 1px;
  height: 1px;
  padding: 0;
  margin: -1px;
  overflow: hidden;
  clip: rect(0, 0, 0, 0);
  white-space: nowrap;
  border: 0;
}
```

Delete the complete `.workspace-tool-rail__item--active::before` rule.

- [x] **Step 5: Add the scoped compact tooltip rule**

```css
.workspace-tool-rail__tooltip .arco-tooltip-content {
  padding: 3px 6px;
  font-size: 11px;
  line-height: 16px;
}
```

- [x] **Step 6: Run focused tests and type checking**

Run:

```bash
bun test ui/src/renderer/pages/conversation/components/ChatLayout/workspaceToolRail.test.ts
bun run typecheck
```

Expected: all workspace rail tests pass and type checking exits with code 0.

- [x] **Step 7: Commit the implementation**

```bash
git add \
  ui/src/renderer/pages/conversation/components/ChatLayout/workspaceToolRail.test.ts \
  ui/src/renderer/pages/conversation/components/ChatLayout/WorkspaceToolRail.tsx \
  ui/src/renderer/pages/conversation/components/ChatLayout/chat-layout.css \
  docs/superpowers/plans/2026-07-11-workspace-tool-rail-visual-polish.md
git commit -m "style(ui): simplify workspace tool rail"
```

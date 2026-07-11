# Scheduled Task List Density Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make desktop scheduled-task rows compact and remove the list perimeter while preserving internal dividers and the mobile card layout.

**Architecture:** Keep the existing single responsive task DOM and change only desktop `md:` utility classes in `ScheduledTasksPage/index.tsx`. Add source-level styling assertions to the existing Bun test so the UnoCSS-visible JSX contract is covered without introducing runtime layout helpers.

**Tech Stack:** React 19, TypeScript 5.8, UnoCSS, Bun test runner.

## Global Constraints

- Desktop task rows use `md:min-h-48px` and `md:py-8px`, producing an approximately 64px total row height.
- Desktop header and list have no four-sided perimeter or rounded outer corners.
- Desktop header, list container, and task rows have transparent backgrounds.
- The header bottom rule and task-to-task horizontal dividers remain visible.
- Mobile cards retain their existing border, padding, layout, and behavior.
- Detail navigation and start/pause callbacks remain unchanged.

---

### Task 1: Compact borderless desktop rows

**Files:**
- Modify: `ui/src/renderer/pages/cron/ScheduledTasksPage/index.tsx`
- Test: `ui/src/renderer/pages/cron/ScheduledTasksPage/scheduledTaskLayout.test.ts`

**Interfaces:**
- Consumes: the existing single responsive task DOM and `md:` breakpoint.
- Produces: compact desktop rows without a perimeter; mobile behavior is unchanged.

- [x] **Step 1: Write the failing styling contract test**

Add `readFileSync` and the page source fixture to `scheduledTaskLayout.test.ts`:

```ts
import { readFileSync } from 'node:fs';

const pageSource = readFileSync(new URL('./index.tsx', import.meta.url), 'utf8');
```

Add these tests:

```ts
test('uses compact desktop task rows', () => {
  expect(pageSource.includes('md:min-h-48px')).toBe(true);
  expect(pageSource.includes('md:py-8px')).toBe(true);
  expect(pageSource.includes('md:min-h-68px')).toBe(false);
  expect(pageSource.includes('md:py-14px')).toBe(false);
});

test('removes only the desktop perimeter and keeps internal dividers', () => {
  expect(pageSource.includes('rounded-t-12px')).toBe(false);
  expect(pageSource.includes('md:rounded-b-12px')).toBe(false);
  expect(pageSource.includes('md:divide-y')).toBe(true);
  expect(pageSource.includes('border-b-[var(--color-border-2)]')).toBe(true);
});
```

- [x] **Step 2: Run the focused test and verify RED**

Run: `bun test ui/src/renderer/pages/cron/ScheduledTasksPage/scheduledTaskLayout.test.ts`

Expected: FAIL because the current JSX still contains `md:min-h-68px`, `md:py-14px`, and rounded bordered perimeter classes.

- [x] **Step 3: Apply the minimal JSX class changes**

Change the desktop header class to use only a bottom border:

```tsx
className='hidden items-center gap-16px border-b border-b-solid border-b-[var(--color-border-2)] bg-fill-2 px-18px py-10px text-12px font-medium leading-18px text-t-tertiary md:grid'
```

Change the list class to remove desktop outer border and rounding while preserving `md:divide-y`:

```tsx
className='grid w-full grid-cols-1 items-start gap-12px md:block md:bg-fill-1 md:divide-y md:divide-solid md:divide-[var(--color-border-2)]'
```

In the task-row class, replace `md:min-h-68px md:py-14px` with `md:min-h-48px md:py-8px`. Do not change the non-`md:` mobile classes.

- [x] **Step 4: Run focused and adjacent tests**

Run: `bun test ui/src/renderer/pages/cron/ScheduledTasksPage/scheduledTaskLayout.test.ts ui/src/renderer/pages/cron/ScheduledTasksPage/cronJobSearch.test.ts ui/src/renderer/pages/cron/ScheduledTasksPage/scheduledCreateTarget.test.ts`

Expected: all tests pass.

- [x] **Step 5: Run static and production verification**

Run: `bun run typecheck && bun run build:ui`

Expected: both commands exit with code 0.

- [x] **Step 6: Commit the style adjustment**

```bash
git add ui/src/renderer/pages/cron/ScheduledTasksPage/index.tsx ui/src/renderer/pages/cron/ScheduledTasksPage/scheduledTaskLayout.test.ts docs/superpowers/specs/2026-07-11-scheduled-task-horizontal-list-design.md docs/superpowers/plans/2026-07-11-scheduled-task-list-density.md
git commit -m "style(ui): compact scheduled task rows"
```

### Task 2: Transparent desktop table surfaces

**Files:**
- Modify: `ui/src/renderer/pages/cron/ScheduledTasksPage/index.tsx`
- Test: `ui/src/renderer/pages/cron/ScheduledTasksPage/scheduledTaskLayout.test.ts`

**Interfaces:**
- Consumes: the compact borderless desktop markup from Task 1.
- Produces: transparent desktop header, list container, and task rows while preserving mobile cards, internal dividers, and hover feedback.

- [x] **Step 1: Write the failing transparency test**

Add to `scheduledTaskLayout.test.ts`:

```ts
test('keeps desktop table surfaces transparent', () => {
  const desktopHeaderClass =
    pageSource.match(/className='hidden items-center gap-16px[^']*md:grid'/)?.[0] ?? '';
  const desktopListClass =
    pageSource.match(/className='grid w-full grid-cols-1 items-start gap-12px[^']*md:divide-\[var\(--color-border-2\)\]'/)?.[0] ?? '';
  const desktopRowClass =
    pageSource.match(/className='group flex cursor-pointer flex-col[^']*md:hover:shadow-none'/)?.[0] ?? '';

  expect(desktopHeaderClass.includes('bg-fill-2')).toBe(false);
  expect(desktopListClass.includes('md:bg-fill-1')).toBe(false);
  expect(desktopRowClass.includes('bg-fill-1')).toBe(true);
  expect(desktopRowClass.includes('md:bg-transparent')).toBe(true);
  expect(desktopHeaderClass.includes('border-b-[var(--color-border-2)]')).toBe(true);
  expect(desktopListClass.includes('md:divide-y')).toBe(true);
});
```

- [x] **Step 2: Run the focused test and verify RED**

Run: `bun test ui/src/renderer/pages/cron/ScheduledTasksPage/scheduledTaskLayout.test.ts`

Expected: FAIL because at least one desktop table surface still has a background, including a mobile row background that is not explicitly cleared at the desktop breakpoint.

- [x] **Step 3: Remove only the desktop surface backgrounds**

In `index.tsx`, remove `bg-fill-2` from the desktop-only header class, remove `md:bg-fill-1` from the desktop list class, and add `md:bg-transparent` to each task row. Keep `border-b-[var(--color-border-2)]`, `md:divide-y`, the row's mobile `bg-fill-1`, and `md:hover:bg-fill-2` unchanged.

- [x] **Step 4: Run focused and adjacent tests**

Run: `bun test ui/src/renderer/pages/cron/ScheduledTasksPage/scheduledTaskLayout.test.ts ui/src/renderer/pages/cron/ScheduledTasksPage/cronJobSearch.test.ts ui/src/renderer/pages/cron/ScheduledTasksPage/scheduledCreateTarget.test.ts`

Expected: all tests pass.

- [x] **Step 5: Run type and production verification**

Run: `bun run typecheck && bun run build:ui`

Expected: both commands exit with code 0.

- [x] **Step 6: Commit the transparency adjustment**

```bash
git add ui/src/renderer/pages/cron/ScheduledTasksPage/index.tsx ui/src/renderer/pages/cron/ScheduledTasksPage/scheduledTaskLayout.test.ts docs/superpowers/specs/2026-07-11-scheduled-task-horizontal-list-design.md docs/superpowers/plans/2026-07-11-scheduled-task-list-density.md
git commit -m "style(ui): clear scheduled task row background"
```

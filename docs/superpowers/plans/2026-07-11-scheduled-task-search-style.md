# Scheduled Task Search Style Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Give the scheduled-task search box a visible 1px themed border and pill-shaped corners without changing search behavior or other pages.

**Architecture:** Keep the existing Arco `Input.Search` and apply page-local UnoCSS descendant variants to its `.arco-input-inner-wrapper`. Extend the existing source-level layout test so the exact visual contract is verified before changing JSX.

**Tech Stack:** React 19, TypeScript 5.8, Arco Design `Input.Search`, UnoCSS, Bun test runner.

## Global Constraints

- The visual change applies only to `ScheduledTasksPage`.
- The search box uses a 1px solid `var(--color-border-2)` default border and `rounded-full` pill corners.
- Hover uses `var(--color-border-3)` and focus retains `rgb(var(--primary-6))` feedback.
- `allowClear`, `value`, `onChange`, placeholder text, and filtering behavior remain unchanged.
- No wrapper element, global Arco override, or new dependency is introduced.

---

### Task 1: Bordered pill search box

**Files:**
- Modify: `ui/src/renderer/pages/cron/ScheduledTasksPage/index.tsx:154-160`
- Test: `ui/src/renderer/pages/cron/ScheduledTasksPage/scheduledTaskLayout.test.ts`

**Interfaces:**
- Consumes: the existing `Input.Search` props and Arco `.arco-input-inner-wrapper` DOM class.
- Produces: a page-local search style contract expressed entirely in the existing `className` prop.

- [x] **Step 1: Write the failing style contract test**

Append this test to `scheduledTaskLayout.test.ts`:

```ts
test('styles the scheduled task search as a bordered pill', () => {
  const searchClass =
    pageSource.match(/<Input\.Search[\s\S]*?className='([^']+)'[\s\S]*?\/>/)?.[1] ?? '';
  const searchClasses = searchClass.split(/\s+/);

  expect(searchClasses.includes('[&_.arco-input-inner-wrapper]:!rounded-full')).toBe(true);
  expect(searchClasses.includes('[&_.arco-input-inner-wrapper]:!border')).toBe(true);
  expect(searchClasses.includes('[&_.arco-input-inner-wrapper]:!border-solid')).toBe(true);
  expect(searchClasses.includes('[&_.arco-input-inner-wrapper]:!border-[var(--color-border-2)]')).toBe(true);
  expect(searchClasses.includes('[&_.arco-input-inner-wrapper:hover]:!border-[var(--color-border-3)]')).toBe(true);
  expect(searchClasses.includes('[&_.arco-input-inner-wrapper-focus]:!border-[rgb(var(--primary-6))]')).toBe(true);
});
```

- [x] **Step 2: Run the focused test and verify RED**

Run:

```bash
bun test ui/src/renderer/pages/cron/ScheduledTasksPage/scheduledTaskLayout.test.ts
```

Expected: the new test fails because the current search class is only `w-full`.

- [x] **Step 3: Apply the minimal local search styles**

Change only the existing `Input.Search` `className`:

```tsx
className='w-full [&_.arco-input-inner-wrapper]:!rounded-full [&_.arco-input-inner-wrapper]:!border [&_.arco-input-inner-wrapper]:!border-solid [&_.arco-input-inner-wrapper]:!border-[var(--color-border-2)] [&_.arco-input-inner-wrapper:hover]:!border-[var(--color-border-3)] [&_.arco-input-inner-wrapper-focus]:!border-[rgb(var(--primary-6))]'
```

Do not change any other `Input.Search` prop or add a wrapper.

- [x] **Step 4: Run focused and adjacent tests**

Run:

```bash
bun test ui/src/renderer/pages/cron/ScheduledTasksPage/scheduledTaskLayout.test.ts ui/src/renderer/pages/cron/ScheduledTasksPage/cronJobSearch.test.ts ui/src/renderer/pages/cron/ScheduledTasksPage/scheduledCreateTarget.test.ts
```

Expected: 11 tests pass with 0 failures.

- [x] **Step 5: Run static and production verification**

Run:

```bash
bun run typecheck
bun run build:ui
git diff --check
```

Expected: all commands exit with code 0. Existing Vite chunk-size warnings are allowed; new errors are not.

- [x] **Step 6: Commit the implementation**

```bash
git add ui/src/renderer/pages/cron/ScheduledTasksPage/index.tsx ui/src/renderer/pages/cron/ScheduledTasksPage/scheduledTaskLayout.test.ts docs/superpowers/plans/2026-07-11-scheduled-task-search-style.md
git commit -m "style(ui): round scheduled task search"
```

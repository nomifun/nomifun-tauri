# Scheduled Task Horizontal List Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Convert the desktop scheduled-task card grid into a single-column horizontal list while preserving the current mobile cards and start/pause behavior.

**Architecture:** Keep task fetching and mutations in `ScheduledTasksPage`, and extract only the responsive presentation decision into a small pure helper so it can be developed test-first. The page renders a desktop CSS Grid list with a shared header and divided rows when the helper returns `row`, while the existing mobile card markup remains the `card` branch.

**Tech Stack:** React 19, TypeScript 5.8, Arco Design, UnoCSS utility classes, Bun test runner.

## Global Constraints

- Desktop uses a single-column horizontal list with a divider between each task.
- Desktop columns are task title, next run time, task status, execution mode, and start/pause control.
- Mobile keeps the current card layout and does not introduce horizontal scrolling.
- Existing search, create, keep-awake, navigation, status, and mutation behavior must remain unchanged.
- Manual-only tasks keep the existing rule and do not display a start/pause switch.
- No backend API, task model, or scheduling behavior changes.

---

### Task 1: Responsive presentation decision

**Files:**
- Create: `ui/src/renderer/pages/cron/ScheduledTasksPage/scheduledTaskLayout.ts`
- Test: `ui/src/renderer/pages/cron/ScheduledTasksPage/scheduledTaskLayout.test.ts`

**Interfaces:**
- Consumes: the `isMobile: boolean` value from `LayoutContext`.
- Produces: `getScheduledTaskLayout(isMobile: boolean): 'card' | 'row'` for `ScheduledTasksPage`.

- [ ] **Step 1: Write the failing test**

```ts
import { describe, expect, test } from 'bun:test';
import { getScheduledTaskLayout } from './scheduledTaskLayout';

describe('getScheduledTaskLayout', () => {
  test('keeps cards on mobile', () => {
    expect(getScheduledTaskLayout(true)).toBe('card');
  });

  test('uses horizontal rows on desktop', () => {
    expect(getScheduledTaskLayout(false)).toBe('row');
  });
});
```

- [ ] **Step 2: Run the test and verify RED**

Run: `bun test ui/src/renderer/pages/cron/ScheduledTasksPage/scheduledTaskLayout.test.ts`

Expected: FAIL because `./scheduledTaskLayout` does not exist.

- [ ] **Step 3: Add the minimal implementation**

```ts
export type ScheduledTaskLayout = 'card' | 'row';

export function getScheduledTaskLayout(isMobile: boolean): ScheduledTaskLayout {
  return isMobile ? 'card' : 'row';
}
```

- [ ] **Step 4: Run the test and verify GREEN**

Run: `bun test ui/src/renderer/pages/cron/ScheduledTasksPage/scheduledTaskLayout.test.ts`

Expected: 2 tests pass with no warnings or errors.

- [ ] **Step 5: Commit the presentation seam**

```bash
git add ui/src/renderer/pages/cron/ScheduledTasksPage/scheduledTaskLayout.ts ui/src/renderer/pages/cron/ScheduledTasksPage/scheduledTaskLayout.test.ts
git commit -m "test(ui): define scheduled task responsive layout"
```

### Task 2: Desktop horizontal task list

**Files:**
- Modify: `ui/src/renderer/pages/cron/ScheduledTasksPage/index.tsx`
- Modify: `ui/src/renderer/services/i18n/locales/zh-CN/cron.json`
- Modify: `ui/src/renderer/services/i18n/locales/en-US/cron.json`
- Modify (generated): `ui/src/renderer/services/i18n/i18n-keys.d.ts`
- Test: `ui/src/renderer/pages/cron/ScheduledTasksPage/scheduledTaskLayout.test.ts`

**Interfaces:**
- Consumes: `getScheduledTaskLayout(isMobile)` from Task 1, existing `ICronJob`, `CronStatusTag`, `formatNextRun`, `getJobAgentMeta`, and `handleToggleEnabled`.
- Produces: desktop list markup with the stable column template `minmax(0,1.6fr) minmax(150px,1.1fr) minmax(84px,auto) minmax(120px,1fr) 44px`; the mobile branch retains the existing cards.

- [ ] **Step 1: Extend the failing test with the desktop column contract**

Replace the helper import and add locale imports at the top of `scheduledTaskLayout.test.ts`:

```ts
import { DESKTOP_SCHEDULED_TASK_COLUMNS, getScheduledTaskLayout } from './scheduledTaskLayout';
import cronEn from '@renderer/services/i18n/locales/en-US/cron.json';
import cronZh from '@renderer/services/i18n/locales/zh-CN/cron.json';
```

Then add these tests:

```ts
test('defines five readable desktop columns', () => {
  expect(DESKTOP_SCHEDULED_TASK_COLUMNS).toBe(
    'minmax(0,1.6fr) minmax(150px,1.1fr) minmax(84px,auto) minmax(120px,1fr) 44px'
  );
});

test('provides localized desktop-only column labels', () => {
  expect((cronZh.page as Record<string, unknown>).list).toEqual({
    task: '任务标题',
    status: '任务状态',
    action: '启停',
  });
  expect((cronEn.page as Record<string, unknown>).list).toEqual({
    task: 'Task',
    status: 'Status',
    action: 'On / off',
  });
});
```

- [ ] **Step 2: Run the test and verify RED**

Run: `bun test ui/src/renderer/pages/cron/ScheduledTasksPage/scheduledTaskLayout.test.ts`

Expected: FAIL because `DESKTOP_SCHEDULED_TASK_COLUMNS` is not exported and the desktop-only labels are absent.

- [ ] **Step 3: Add the shared desktop grid constant**

Add to `scheduledTaskLayout.ts`:

```ts
export const DESKTOP_SCHEDULED_TASK_COLUMNS =
  'minmax(0,1.6fr) minmax(150px,1.1fr) minmax(84px,auto) minmax(120px,1fr) 44px';
```

Add the following `list` object under `page` in each cron locale:

```json
// zh-CN
"list": { "task": "任务标题", "status": "任务状态", "action": "启停" }

// en-US
"list": { "task": "Task", "status": "Status", "action": "On / off" }
```

Regenerate the typed key union:

Run: `bun run gen:i18n`

Expected: `i18n-keys.d.ts` includes `cron.page.list.action`, `cron.page.list.status`, and `cron.page.list.task`.

- [ ] **Step 4: Run the focused test and verify GREEN**

Run: `bun test ui/src/renderer/pages/cron/ScheduledTasksPage/scheduledTaskLayout.test.ts`

Expected: 4 tests pass.

- [ ] **Step 5: Render the desktop header and divided rows**

In `index.tsx`, import both exports and compute the layout once:

```ts
const taskLayout = getScheduledTaskLayout(isMobile);
```

For `taskLayout === 'row'`, render one bordered, rounded list container. Render a muted header and every task row with the same inline grid template:

```tsx
style={{ gridTemplateColumns: DESKTOP_SCHEDULED_TASK_COLUMNS }}
```

Use the five cells below:

```tsx
<span className='min-w-0 truncate font-medium text-t-primary' title={job.name}>{job.name}</span>
<span className='truncate text-t-secondary' title={nextRunLabel}>{nextRunLabel}</span>
<CronStatusTag job={job} />
<span className='min-w-0 truncate text-t-secondary'>{executionModeLabel}</span>
<Switch size='small' checked={job.enabled} onChange={() => handleToggleEnabled(job)} />
```

Keep `onClick={() => handleGoToDetail(job)}` on the row and `onClick={(event) => event.stopPropagation()}` on the switch wrapper. Use `divide-y divide-border-2` on the row container and a hover background on each row. For manual-only tasks, render an empty operation cell rather than the switch so column alignment remains stable.

For `taskLayout === 'card'`, retain the current card grid and card markup without changing field order or interactions.

Use `cron.page.list.task`, `cron.nextRun`, `cron.page.list.status`, `cron.page.form.executionMode`, and `cron.page.list.action` for the five desktop header labels.

- [ ] **Step 6: Run focused and adjacent cron tests**

Run: `bun test ui/src/renderer/pages/cron/ScheduledTasksPage/scheduledTaskLayout.test.ts ui/src/renderer/pages/cron/ScheduledTasksPage/cronJobSearch.test.ts ui/src/renderer/pages/cron/ScheduledTasksPage/scheduledCreateTarget.test.ts`

Expected: all tests pass.

- [ ] **Step 7: Run static verification**

Run: `bun run typecheck`

Expected: TypeScript exits with code 0.

- [ ] **Step 8: Review the responsive markup**

Inspect the desktop branch for exactly five aligned cells and divider lines. Inspect the mobile branch to confirm the original schedule description, next-run label, Agent metadata, status tag, and switch are all still present. Confirm the switch wrapper stops propagation in both branches.

- [ ] **Step 9: Commit the UI implementation**

```bash
git add ui/src/renderer/pages/cron/ScheduledTasksPage/index.tsx ui/src/renderer/pages/cron/ScheduledTasksPage/scheduledTaskLayout.ts ui/src/renderer/pages/cron/ScheduledTasksPage/scheduledTaskLayout.test.ts ui/src/renderer/services/i18n/locales/zh-CN/cron.json ui/src/renderer/services/i18n/locales/en-US/cron.json ui/src/renderer/services/i18n/i18n-keys.d.ts docs/superpowers/specs/2026-07-11-scheduled-task-horizontal-list-design.md docs/superpowers/plans/2026-07-11-scheduled-task-horizontal-list.md
git commit -m "style(ui): show scheduled tasks as desktop rows"
```

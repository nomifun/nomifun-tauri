# Scheduled Task More Actions Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Replace the desktop scheduled-task enable switch column with a hover-revealed `More` menu while preserving existing pause, resume, delete, navigation, and mobile-card behavior.

**Architecture:** Add a focused `ScheduledTaskActions` presentation component that owns only dropdown visibility, menu rendering, and delete confirmation. Keep `pauseJob`, `resumeJob`, `deleteJob`, success/error messages, and task-state ownership in `ScheduledTasksPage`; retain the existing mobile switch behind `md:hidden`.

**Tech Stack:** React 19, TypeScript 5.8, Arco Design `Dropdown` / `Menu` / `Modal`, IconPark React, UnoCSS, Bun test runner, Codex in-app Browser.

## Global Constraints

- Desktop removes the visible “启停” / “On / off” header and inline switch.
- The IconPark `More` trigger sits at the far right of each desktop row.
- The trigger is hidden until its row is hovered, keyboard-focused, or its dropdown is open.
- Scheduled jobs show exactly `pause + remove` when enabled and `resume + remove` when disabled.
- Manual-only jobs show exactly `remove`; they never receive pause or resume behavior.
- Remove reuses `deleteJob`, `cron.confirmDeleteWithConversations`, `cron.deleteSuccess`, and existing error handling.
- Menu interactions stop row click propagation and never navigate to task detail.
- Mobile keeps the existing card layout and `Switch`; it does not render the desktop menu.
- No backend, routing, task-model, global Arco style, or dependency changes.

---

### Task 1: Desktop task action menu component

**Files:**
- Create: `ui/src/renderer/pages/cron/ScheduledTasksPage/ScheduledTaskActions.tsx`
- Create: `ui/src/renderer/pages/cron/ScheduledTasksPage/ScheduledTaskActions.test.ts`

**Interfaces:**
- Consumes: `enabled: boolean`, `isManualOnly: boolean`, `onToggle: () => Promise<void>`, and `onRemove: () => Promise<void>`.
- Produces: `getScheduledTaskMenuActions(enabled, isManualOnly): ScheduledTaskMenuAction[]` and the default `ScheduledTaskActions` component.

- [x] **Step 1: Write failing behavior and source-contract tests**

Create `ScheduledTaskActions.test.ts`:

```ts
/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

import { expect, test } from 'bun:test';
import { readFileSync } from 'node:fs';

const actionModulePath = './ScheduledTaskActions';

async function loadActionModule() {
  try {
    return await import(actionModulePath);
  } catch {
    return {};
  }
}

function readActionSource(): string {
  try {
    return readFileSync(new URL('./ScheduledTaskActions.tsx', import.meta.url), 'utf8');
  } catch {
    return '';
  }
}

test('keeps manual-only jobs remove-only and maps scheduled jobs to their current toggle action', async () => {
  const actionModule = (await loadActionModule()) as {
    getScheduledTaskMenuActions?: (enabled: boolean, isManualOnly: boolean) => string[];
  };
  const getActions = actionModule.getScheduledTaskMenuActions;

  expect(typeof getActions).toBe('function');
  if (!getActions) return;

  expect(getActions(true, false)).toEqual(['pause', 'remove']);
  expect(getActions(false, false)).toEqual(['resume', 'remove']);
  expect(getActions(true, true)).toEqual(['remove']);
  expect(getActions(false, true)).toEqual(['remove']);
});

test('keeps the desktop more trigger visible for row hover, focus, and an open menu', () => {
  const actionSource = readActionSource();

  expect(actionSource.includes("import { DeleteOne, More, PauseOne, PlayOne } from '@icon-park/react'")).toBe(true);
  expect(actionSource.includes('group-hover:opacity-100')).toBe(true);
  expect(actionSource.includes('focus-visible:opacity-100')).toBe(true);
  expect(actionSource.includes("menuVisible && '!pointer-events-auto !opacity-100'")).toBe(true);
  expect(actionSource.includes('onClick={(event) => event.stopPropagation()}')).toBe(true);
  expect(actionSource.includes('Modal.confirm({')).toBe(true);
});
```

- [x] **Step 2: Run the component test and verify RED**

Run:

```bash
bun test ui/src/renderer/pages/cron/ScheduledTasksPage/ScheduledTaskActions.test.ts
```

Expected: both tests fail because `ScheduledTaskActions.tsx` does not exist, the action function is undefined, and the source string is empty.

- [x] **Step 3: Implement the focused action component**

Create `ScheduledTaskActions.tsx`:

```tsx
/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

import classNames from 'classnames';
import React, { useMemo, useState } from 'react';
import { useTranslation } from 'react-i18next';
import { Button, Dropdown, Menu, Modal } from '@arco-design/web-react';
import { DeleteOne, More, PauseOne, PlayOne } from '@icon-park/react';

export type ScheduledTaskMenuAction = 'pause' | 'resume' | 'remove';

export function getScheduledTaskMenuActions(
  enabled: boolean,
  isManualOnly: boolean
): ScheduledTaskMenuAction[] {
  return isManualOnly ? ['remove'] : [enabled ? 'pause' : 'resume', 'remove'];
}

interface ScheduledTaskActionsProps {
  enabled: boolean;
  isManualOnly: boolean;
  onToggle: () => Promise<void>;
  onRemove: () => Promise<void>;
}

const ScheduledTaskActions: React.FC<ScheduledTaskActionsProps> = ({
  enabled,
  isManualOnly,
  onToggle,
  onRemove,
}) => {
  const { t } = useTranslation();
  const [menuVisible, setMenuVisible] = useState(false);
  const actions = useMemo(
    () => getScheduledTaskMenuActions(enabled, isManualOnly),
    [enabled, isManualOnly]
  );

  const handleMenuItem = (key: string) => {
    setMenuVisible(false);
    if (key === 'remove') {
      Modal.confirm({
        title: t('cron.confirmDeleteWithConversations'),
        okText: t('common.remove'),
        cancelText: t('common.cancel'),
        okButtonProps: { status: 'danger' },
        onOk: onRemove,
      });
      return;
    }
    void onToggle();
  };

  return (
    <div
      className='hidden shrink-0 md:block md:[grid-column:5] md:[grid-row:1] md:justify-self-center'
      onClick={(event) => event.stopPropagation()}
    >
      <Dropdown
        trigger='click'
        position='br'
        popupVisible={menuVisible}
        onVisibleChange={setMenuVisible}
        getPopupContainer={() => document.body}
        unmountOnExit={false}
        droplist={
          <Menu onClickMenuItem={handleMenuItem}>
            {actions.map((action) => {
              const isRemove = action === 'remove';
              const label = isRemove
                ? t('common.remove')
                : t(action === 'pause' ? 'cron.actions.pause' : 'cron.actions.resume');
              const icon = isRemove ? (
                <DeleteOne theme='outline' size='14' />
              ) : action === 'pause' ? (
                <PauseOne theme='outline' size='14' />
              ) : (
                <PlayOne theme='outline' size='14' />
              );

              return (
                <Menu.Item key={action}>
                  <div
                    className={classNames(
                      'flex items-center gap-8px',
                      isRemove && 'text-[rgb(var(--danger-6))]'
                    )}
                  >
                    {icon}
                    <span>{label}</span>
                  </div>
                </Menu.Item>
              );
            })}
          </Menu>
        }
      >
        <Button
          type='text'
          size='mini'
          aria-label={t('common.more')}
          className={classNames(
            '!h-24px !w-24px !min-w-24px !rounded-6px !p-0 !text-t-secondary',
            'pointer-events-none opacity-0 transition-opacity hover:!text-t-primary',
            'group-hover:pointer-events-auto group-hover:opacity-100',
            'focus-visible:pointer-events-auto focus-visible:opacity-100',
            menuVisible && '!pointer-events-auto !opacity-100'
          )}
          icon={<More theme='outline' size='14' fill='currentColor' className='block leading-none' />}
          onClick={() => {
            setMenuVisible(true);
          }}
        />
      </Dropdown>
    </div>
  );
};

export default ScheduledTaskActions;
```

- [x] **Step 4: Run the component test and verify GREEN**

Run:

```bash
bun test ui/src/renderer/pages/cron/ScheduledTasksPage/ScheduledTaskActions.test.ts
```

Expected: 2 tests pass with 0 failures.

- [x] **Step 5: Commit the standalone component**

```bash
git add ui/src/renderer/pages/cron/ScheduledTasksPage/ScheduledTaskActions.tsx ui/src/renderer/pages/cron/ScheduledTasksPage/ScheduledTaskActions.test.ts docs/superpowers/plans/2026-07-11-scheduled-task-more-actions.md
git commit -m "feat(ui): add scheduled task action menu"
```

---

### Task 2: Integrate desktop menu and preserve mobile switch

**Files:**
- Modify: `ui/src/renderer/pages/cron/ScheduledTasksPage/index.tsx`
- Modify: `ui/src/renderer/pages/cron/ScheduledTasksPage/scheduledTaskLayout.test.ts`

**Interfaces:**
- Consumes: `ScheduledTaskActions`, `deleteJob`, `handleToggleEnabled`, and the existing `isManualOnly` calculation.
- Produces: desktop hover actions wired to existing callbacks, with the original switch remaining mobile-only.

- [x] **Step 1: Write the failing integration contract test**

Append to `scheduledTaskLayout.test.ts`:

```ts
test('uses a desktop more menu without changing the mobile switch contract', () => {
  expect(pageSource.includes("t('cron.page.list.action')")).toBe(false);
  expect(pageSource.includes("import ScheduledTaskActions from './ScheduledTaskActions'")).toBe(true);
  expect(pageSource.includes('deleteJob')).toBe(true);
  expect(pageSource.includes('<ScheduledTaskActions')).toBe(true);

  const mobileSwitchBlock =
    pageSource.match(/className='shrink-0 md:hidden'[\s\S]*?<Switch[\s\S]*?handleToggleEnabled\(job\)/)?.[0] ?? '';
  expect(Boolean(mobileSwitchBlock)).toBe(true);
});
```

- [x] **Step 2: Run the layout test and verify RED**

Run:

```bash
bun test ui/src/renderer/pages/cron/ScheduledTasksPage/scheduledTaskLayout.test.ts
```

Expected: the new test fails because the page still renders the action header and desktop switch and does not import the action component or `deleteJob`.

- [x] **Step 3: Wire existing pause, resume, and delete callbacks into the new layout**

In `index.tsx`:

1. Import `ScheduledTaskActions`.
2. Destructure `deleteJob` from `useAllCronJobs()`.
3. Add the list-level delete callback:

```ts
const handleRemoveJob = useCallback(
  async (job: ICronJob) => {
    try {
      await deleteJob(job.id);
      Message.success(t('cron.deleteSuccess'));
    } catch (err) {
      Message.error(String(err));
    }
  },
  [deleteJob, t]
);
```

4. Remove this header node:

```tsx
<span className='text-center'>{t('cron.page.list.action')}</span>
```

5. Replace the current action wrapper with the mobile switch and desktop component:

```tsx
<div
  className='shrink-0 md:hidden'
  onClick={(event) => event.stopPropagation()}
>
  {!isManualOnly && (
    <Switch size='small' checked={job.enabled} onChange={() => handleToggleEnabled(job)} />
  )}
</div>

<ScheduledTaskActions
  enabled={job.enabled}
  isManualOnly={isManualOnly}
  onToggle={() => handleToggleEnabled(job)}
  onRemove={() => handleRemoveJob(job)}
/>
```

Do not change `handleToggleEnabled`, `handleGoToDetail`, `isManualOnly`, `DESKTOP_SCHEDULED_TASK_COLUMNS`, or any mobile card content.

- [x] **Step 4: Run focused and adjacent tests**

Run:

```bash
bun test ui/src/renderer/pages/cron/ScheduledTasksPage/ScheduledTaskActions.test.ts ui/src/renderer/pages/cron/ScheduledTasksPage/scheduledTaskLayout.test.ts ui/src/renderer/pages/cron/ScheduledTasksPage/cronJobSearch.test.ts ui/src/renderer/pages/cron/ScheduledTasksPage/scheduledCreateTarget.test.ts
```

Expected: all tests pass with 0 failures.

- [x] **Step 5: Run static and production verification**

Run:

```bash
bun run typecheck
bun run build:ui
git diff --check
```

Expected: all commands exit with code 0. Existing Vite chunk-size warnings are allowed; new errors are not.

- [x] **Step 6: Commit the page integration**

```bash
git add ui/src/renderer/pages/cron/ScheduledTasksPage/index.tsx ui/src/renderer/pages/cron/ScheduledTasksPage/scheduledTaskLayout.test.ts docs/superpowers/plans/2026-07-11-scheduled-task-more-actions.md
git commit -m "style(ui): move scheduled task actions into menu"
```

---

### Task 3: Browser interaction and design QA

**Files:**
- Create: `design-qa.md`

**Interfaces:**
- Consumes: the user-provided desktop screenshot and the implemented `/scheduled` page.
- Produces: visual evidence that the desktop hover/menu state matches the requested interaction and mobile layout has not changed.

- [x] **Step 1: Start or reuse the local UI**

If port 5173 is not already serving the application, run:

```bash
bun run dev:ui -- --host 0.0.0.0 --port 5173 --strictPort
```

Expected: Vite serves the UI on port 5173 without compilation errors.

- [x] **Step 2: Inspect the desktop state in the Codex in-app Browser**

Use the in-app Browser and open the scheduled-task page at the same desktop viewport as the reference screenshot. Verify all of the following:

- No “启停” column header or desktop switch is visible.
- Moving onto one row reveals only that row's `More` icon.
- Moving into the popup keeps the trigger visible.
- Enabled jobs show “暂停” and “移除”.
- Paused jobs show “恢复” and “移除”.
- Manual-only jobs show only “移除”.
- Clicking the trigger or menu does not open task detail.
- Removing requires confirmation; cancel leaves the task unchanged.

- [x] **Step 3: Inspect the mobile state**

Set the Browser viewport to 390 × 844 and verify:

- Tasks remain cards with no horizontal overflow.
- The original switch remains visible for scheduled jobs.
- Manual-only cards still have no switch.
- No desktop `More` menu is rendered.

- [x] **Step 4: Compare captures and write the QA gate**

Open the reference image and the latest desktop and mobile captures together. Fix any P0/P1/P2 mismatch, recapture, and repeat. When all blocking checks pass, create `design-qa.md` with:

```md
# Scheduled Task More Actions Design QA

- Reference: `codex-clipboard-db57196a-7f7f-4137-b3e8-e8cbac38d78d.png`
- Desktop result: passed — the action header/switch are removed, row hover reveals one More trigger, and the popup remains anchored while open.
- Menu behavior: passed — enabled, paused, and manual-only jobs expose only their allowed actions; confirmation guards removal; menu clicks do not navigate.
- Mobile result: passed — existing cards and scheduled-job switches remain unchanged with no desktop menu or horizontal overflow.
- Remaining P0/P1/P2 issues: none.

final result: passed
```

- [x] **Step 5: Commit QA evidence**

```bash
git add design-qa.md docs/superpowers/plans/2026-07-11-scheduled-task-more-actions.md
git commit -m "test(ui): verify scheduled task action menu"
```

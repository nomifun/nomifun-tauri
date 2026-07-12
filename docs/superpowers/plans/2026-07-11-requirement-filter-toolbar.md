# Requirement Filter Toolbar Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Replace the requirements page's heavy Select/Search controls with a compact icon-and-text filter toolbar while preserving every existing filter, sort, pagination, and batch-action behavior.

**Architecture:** Keep all business state and callbacks in `WorkspacePage`; only `RequirementFilters` gains dropdown presentation and local search-activation state. Extract the two search visibility rules into a small pure module so the interaction can be developed test-first without adding a browser test dependency.

**Tech Stack:** React 19, TypeScript 5.8, Arco Design `Dropdown`/`Menu`/`Input`, IconPark icons, UnoCSS utilities, i18next, Bun test runner.

## Global Constraints

- Only presentation and local toolbar interaction may change; existing filter values, query callbacks, page resets, board behavior, and batch delete behavior remain unchanged.
- Selected entries display `icon + function label + selected content`.
- Search displays `icon + Search` while idle, expands and focuses on click, and collapses after Escape or empty blur; a non-empty query stays visible.
- Sort field and direction live in one dropdown; no separate direction button remains in the toolbar.
- Use existing theme tokens and support light/dark themes and narrow-width wrapping.
- Add no new runtime or test dependency.

---

### Task 1: Search visibility state model

**Files:**
- Create: `ui/src/renderer/pages/requirements/WorkspacePage/requirementFilterToolbarState.ts`
- Test: `ui/src/renderer/pages/requirements/WorkspacePage/requirementFilterToolbarState.test.ts`

**Interfaces:**
- Consumes: `active: boolean` and `query: string` from `RequirementFilters`.
- Produces: `isRequirementSearchExpanded(active: boolean, query: string): boolean` and `shouldCollapseRequirementSearch(query: string): boolean`.

- [ ] **Step 1: Write the failing tests**

```ts
import { describe, expect, test } from 'bun:test';

import {
  isRequirementSearchExpanded,
  shouldCollapseRequirementSearch,
} from './requirementFilterToolbarState';

describe('requirement filter toolbar search state', () => {
  test('stays collapsed when inactive and empty', () => {
    expect(isRequirementSearchExpanded(false, '')).toBe(false);
  });

  test('expands when activated or when a query is present', () => {
    expect(isRequirementSearchExpanded(true, '')).toBe(true);
    expect(isRequirementSearchExpanded(false, 'agent')).toBe(true);
  });

  test('only collapses on blur or Escape when the query is empty', () => {
    expect(shouldCollapseRequirementSearch('')).toBe(true);
    expect(shouldCollapseRequirementSearch('   ')).toBe(true);
    expect(shouldCollapseRequirementSearch('agent')).toBe(false);
  });
});
```

- [ ] **Step 2: Run the test and verify RED**

Run: `cd ui && bun test src/renderer/pages/requirements/WorkspacePage/requirementFilterToolbarState.test.ts`

Expected: FAIL because `./requirementFilterToolbarState` does not exist.

- [ ] **Step 3: Add the minimal pure implementation**

```ts
export function isRequirementSearchExpanded(active: boolean, query: string): boolean {
  return active || query.length > 0;
}

export function shouldCollapseRequirementSearch(query: string): boolean {
  return query.trim().length === 0;
}
```

- [ ] **Step 4: Run the focused test and verify GREEN**

Run: `cd ui && bun test src/renderer/pages/requirements/WorkspacePage/requirementFilterToolbarState.test.ts`

Expected: 3 tests pass, 0 fail.

- [ ] **Step 5: Commit the state model**

```bash
git add ui/src/renderer/pages/requirements/WorkspacePage/requirementFilterToolbarState.ts ui/src/renderer/pages/requirements/WorkspacePage/requirementFilterToolbarState.test.ts
git commit -m "test(requirements): define filter toolbar search behavior"
```

---

### Task 2: Compact icon-and-text filter toolbar

**Files:**
- Modify: `ui/src/renderer/pages/requirements/WorkspacePage/RequirementFilters.tsx`
- Modify: `ui/src/renderer/services/i18n/locales/zh-CN/requirements.json`
- Modify: `ui/src/renderer/services/i18n/locales/en-US/requirements.json`
- Modify: `ui/src/renderer/services/i18n/i18n-keys.d.ts` (generated)
- Test: `ui/src/renderer/pages/requirements/WorkspacePage/RequirementFilters.test.tsx`
- Test: `ui/src/renderer/pages/requirements/WorkspacePage/requirementFilterToolbarState.test.ts`

**Interfaces:**
- Consumes: all existing `RequirementFiltersProps`; `isRequirementSearchExpanded` and `shouldCollapseRequirementSearch` from Task 1.
- Produces: the same callbacks and values already consumed by `WorkspacePage`; no parent API change.

- [ ] **Step 1: Write a failing trigger-rendering test**

Create `RequirementFilters.test.tsx`:

```tsx
import { describe, expect, test } from 'bun:test';
import React from 'react';
import { renderToStaticMarkup } from 'react-dom/server';

import { FilterTrigger } from './RequirementFilters';

describe('RequirementFilters trigger', () => {
  test('renders icon, function label, and selected content', () => {
    const html = renderToStaticMarkup(
      <FilterTrigger icon={<span>icon</span>} label='标签' value='产品' />
    );

    expect(html).toContain('icon');
    expect(html).toContain('标签');
    expect(html).toContain('产品');
    expect(html).toContain('aria-label="标签: 产品"');
  });

  test('omits selected content when the filter is inactive', () => {
    const html = renderToStaticMarkup(
      <FilterTrigger icon={<span>icon</span>} label='状态' />
    );

    expect(html).toContain('aria-label="状态"');
    expect(html).not.toContain('undefined');
  });
});
```

Run: `cd ui && bun test src/renderer/pages/requirements/WorkspacePage/RequirementFilters.test.tsx`

Expected: FAIL because `RequirementFilters.tsx` does not export `FilterTrigger`.

- [ ] **Step 2: Add localized toolbar copy**

Add these keys beside the existing `search` and inside `sort` in both locale files:

```json
"searchLabel": "搜索",
"sort": {
  "direction": "排序方向"
}
```

```json
"searchLabel": "Search",
"sort": {
  "direction": "Direction"
}
```

Preserve every existing member of `sort`; only insert `direction`.

- [ ] **Step 3: Replace heavy controls with dropdown triggers**

In `RequirementFilters.tsx`:

1. Replace the Arco imports with `Dropdown`, `Input`, `Menu`, and `Popconfirm`/`Button` for the unchanged batch bar.
2. Import `Check`, `Filter`, `Search`, `SortTwo`, and `Tag` from `@icon-park/react`.
3. Import `useEffect`, `useRef`, and `useState` from React, `RefInputType` from `@arco-design/web-react/es/Input/interface`, plus the Task 1 helpers.
4. Add `ALL_TAGS = '__all_tags__'`, `ALL_STATUSES = '__all_statuses__'`, `SORT_ASC = '__sort_asc__'`, and `SORT_DESC = '__sort_desc__'` sentinels next to `DEFAULT_SORT`.
5. Add and export a reusable trigger inside the file with this exact public shape:

```tsx
interface FilterTriggerProps {
  icon: React.ReactNode;
  label: string;
  value?: string;
}

export const FilterTrigger: React.FC<FilterTriggerProps> = ({ icon, label, value }) => (
  <button
    type='button'
    aria-label={value ? `${label}: ${value}` : label}
    className='inline-flex h-32px max-w-full items-center gap-6px rounded-6px border-0 bg-transparent px-8px text-13px text-[var(--color-text-2)] transition-colors hover:bg-[var(--color-fill-2)] hover:text-[var(--color-text-1)] focus-visible:outline-2 focus-visible:outline-[rgb(var(--primary-6))]'
  >
    <span aria-hidden='true' className='inline-flex shrink-0'>{icon}</span>
    <span className='shrink-0'>{label}</span>
    {value && <span className='max-w-160px truncate font-medium text-[var(--color-text-1)]'>{value}</span>}
  </button>
);
```

6. Build tag and status `Menu` instances with an explicit first item for clearing, followed by the existing options. Use `Check` to mark the current item. `onClickMenuItem` must map the clear sentinel to `undefined` and all other keys to the existing callbacks.
7. Build one sort `Menu` with a field `Menu.ItemGroup` containing default/ID/created/updated/status and a direction `Menu.ItemGroup` containing ascending/descending. Disable both direction items when `orderBy` is undefined; call the existing `onOrderByChange` and `onOrderChange` callbacks.
8. Wrap the three `FilterTrigger` buttons in Arco `Dropdown trigger='click' position='bl' getPopupContainer={() => document.body}`. Their values are respectively `tag`, localized status, and localized sort field. Omit the value for default sort.
9. Add local search state and autofocus:

```tsx
const [searchActive, setSearchActive] = useState(false);
const searchInputRef = useRef<RefInputType | null>(null);
const searchExpanded = isRequirementSearchExpanded(searchActive, search);

useEffect(() => {
  if (searchActive) searchInputRef.current?.focus();
}, [searchActive]);
```

10. Render the collapsed search with `FilterTrigger` using the `Search` icon and `requirements.searchLabel`. Render expanded search with Arco `Input`, `ref={searchInputRef}`, `allowClear`, the existing long placeholder, the existing value/callback, `onBlur={() => { if (shouldCollapseRequirementSearch(search)) setSearchActive(false); }}`, and Escape handling that clears activation and blurs the input. Give it `className='max-w-full w-260px'`.
11. Keep the parent wrapper `flex flex-wrap items-center gap-8px`; remove the old right-aligned sort wrapper so all four entries follow reference-image order and wrap naturally.
12. Leave `SOFT_BATCH_BAR_STYLE` and the entire selected-count batch bar unchanged.

- [ ] **Step 4: Regenerate i18n types**

Run: `bun run gen:i18n`

Expected: `ui/src/renderer/services/i18n/i18n-keys.d.ts` contains `requirements.searchLabel` and `requirements.sort.direction`.

- [ ] **Step 5: Verify focused tests and static contracts**

Run: `cd ui && bun test src/renderer/pages/requirements/WorkspacePage/requirementFilterToolbarState.test.ts src/renderer/pages/requirements/WorkspacePage/RequirementFilters.test.tsx`

Expected: 5 tests pass, 0 fail.

Run: `bun run typecheck && bun run check:i18n && bun run check:icons`

Expected: all commands exit 0.

- [ ] **Step 6: Verify the production UI build**

Run: `bun run build:ui`

Expected: Vite build exits 0 with generated assets in `ui/dist`.

- [ ] **Step 7: Inspect the rendered requirements page**

Run `bun run dev:ui`, open the requirements page, and verify:

- idle triggers read 标签 / 状态 / 排序 / 搜索 with icons and no input-like borders;
- selected triggers append their localized values;
- clear items restore the unselected trigger;
- sort field and direction both work from one popup;
- clicking Search focuses the input; empty blur/Escape collapses it; a query keeps it expanded;
- list and board filtering still update results and list filter changes still reset pagination;
- narrow width wraps controls without overlap; light and dark themes retain readable hover/focus states;
- the batch delete bar is visually and behaviorally unchanged.

- [ ] **Step 8: Commit the toolbar implementation**

```bash
git add ui/src/renderer/pages/requirements/WorkspacePage/RequirementFilters.tsx ui/src/renderer/pages/requirements/WorkspacePage/RequirementFilters.test.tsx ui/src/renderer/pages/requirements/WorkspacePage/requirementFilterToolbarState.ts ui/src/renderer/pages/requirements/WorkspacePage/requirementFilterToolbarState.test.ts ui/src/renderer/services/i18n/locales/zh-CN/requirements.json ui/src/renderer/services/i18n/locales/en-US/requirements.json ui/src/renderer/services/i18n/i18n-keys.d.ts
git commit -m "style(requirements): compact filter toolbar"
```

---

### Task 3: Final regression verification

**Files:**
- Inspect only: all files changed in Tasks 1–2.

**Interfaces:**
- Consumes: completed toolbar and tests.
- Produces: verification evidence; no code unless a verification failure reveals a defect.

- [ ] **Step 1: Run the complete scoped verification**

Run:

```bash
cd ui && bun test src/renderer/pages/requirements/WorkspacePage/requirementFilterToolbarState.test.ts src/renderer/pages/requirements/WorkspacePage/RequirementFilters.test.tsx
cd .. && bun run typecheck && bun run check:i18n && bun run check:icons && bun run build:ui
```

Expected: all tests pass and all checks/builds exit 0.

- [ ] **Step 2: Review the final diff against the spec**

Run: `git diff HEAD~2 --check && git diff HEAD~2 --stat && git status --short`

Expected: no whitespace errors; only the planned toolbar, state test/helper, i18n, generated type, spec, and plan files are present; worktree is clean after commits.

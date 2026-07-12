# AutoWork Tag Empty State Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Replace AutoWork's generic empty tag dropdown with a polished loading/error/empty flow that sends users directly to the existing new-requirement drawer.

**Architecture:** Extend `useRequirementTags` with reducer-backed request state so consumers can distinguish an initial empty result from loading and failure. Keep AutoWork presentation decisions in a small pure model, then render compact Select `notFoundContent` states and navigate to `/requirements?new=1` without duplicating the requirement form.

**Tech Stack:** React 19, TypeScript 5.8, React Router 7, Arco Design Select/Button/Spin/Switch, IconPark, i18next, Bun test runner, UnoCSS utility classes.

## Global Constraints

- Do not change requirement, tag, or AutoWork backend APIs or persistence models.
- Do not embed or duplicate the requirement form inside the AutoWork popover.
- Do not allow creation of a tag without a requirement.
- Keep existing tagged selection, enabled-state locking, status display, draft mode, and live-event refresh behavior unchanged.
- Existing enabled AutoWork bindings must always remain switchable off, even while tags are loading or failed.
- Use only existing theme tokens, components, dependencies, and Simplified Chinese/English locale bundles.
- The sole empty-state CTA must navigate to exactly `/requirements?new=1`.

---

### Task 1: Add explicit requirement-tag request state

**Files:**
- Create: `ui/src/renderer/pages/requirements/requirementTagLoadState.ts`
- Create: `ui/src/renderer/pages/requirements/requirementTagLoadState.test.ts`
- Modify: `ui/src/renderer/pages/requirements/useRequirements.ts`

**Interfaces:**
- Produces: `RequirementTagSummary`, `RequirementTagLoadState`, `RequirementTagLoadAction`, `initialRequirementTagLoadState`, and `reduceRequirementTagLoadState`.
- Produces: `useRequirementTags(): RequirementTagLoadState & { refresh: () => Promise<void> }`.
- Consumes: `ipcBridge.requirements.tags.invoke()` and the existing requirement live events.

- [ ] **Step 1: Write the failing reducer tests**

Create `requirementTagLoadState.test.ts`:

```ts
import { describe, expect, test } from 'bun:test';
import { initialRequirementTagLoadState, reduceRequirementTagLoadState } from './requirementTagLoadState';

describe('requirement tag load state', () => {
  test('starts loading without discarding existing tags', () => {
    const current = {
      tags: [{ tag: 'release', done: 1, total: 2 }],
      loading: false,
      error: 'old error',
    };

    expect(reduceRequirementTagLoadState(current, { type: 'start' })).toEqual({
      tags: current.tags,
      loading: true,
      error: 'old error',
    });
  });

  test('stores successful tags and clears the previous error', () => {
    const tags = [{ tag: 'release', done: 2, total: 2 }];
    const current = { ...initialRequirementTagLoadState, loading: true, error: 'network' };

    expect(reduceRequirementTagLoadState(current, { type: 'success', tags })).toEqual({
      tags,
      loading: true,
      error: null,
    });
  });

  test('records failure while preserving the last successful tags', () => {
    const tags = [{ tag: 'release', done: 1, total: 2 }];
    const current = { tags, loading: true, error: null };

    expect(reduceRequirementTagLoadState(current, { type: 'failure', error: 'offline' })).toEqual({
      tags,
      loading: true,
      error: 'offline',
    });
  });

  test('finishes every request without changing data or error', () => {
    const current = { tags: [], loading: true, error: 'offline' };
    expect(reduceRequirementTagLoadState(current, { type: 'finish' })).toEqual({
      tags: [],
      loading: false,
      error: 'offline',
    });
  });
});
```

- [ ] **Step 2: Run the reducer test and verify RED**

Run:

```bash
bun test ui/src/renderer/pages/requirements/requirementTagLoadState.test.ts
```

Expected: FAIL because `./requirementTagLoadState` does not exist.

- [ ] **Step 3: Implement the reducer**

Create `requirementTagLoadState.ts`:

```ts
export interface RequirementTagSummary {
  tag: string;
  done: number;
  total: number;
}

export interface RequirementTagLoadState {
  tags: RequirementTagSummary[];
  loading: boolean;
  error: string | null;
}

export type RequirementTagLoadAction =
  | { type: 'start' }
  | { type: 'success'; tags: RequirementTagSummary[] }
  | { type: 'failure'; error: string }
  | { type: 'finish' };

export const initialRequirementTagLoadState: RequirementTagLoadState = {
  tags: [],
  loading: false,
  error: null,
};

export function reduceRequirementTagLoadState(
  state: RequirementTagLoadState,
  action: RequirementTagLoadAction
): RequirementTagLoadState {
  switch (action.type) {
    case 'start':
      return { ...state, loading: true };
    case 'success':
      return { ...state, tags: action.tags, error: null };
    case 'failure':
      return { ...state, error: action.error };
    case 'finish':
      return { ...state, loading: false };
  }
}
```

- [ ] **Step 4: Connect the reducer to `useRequirementTags`**

In `useRequirements.ts`, import `useReducer`, `initialRequirementTagLoadState`, and `reduceRequirementTagLoadState`. Replace the tag hook's `useState` with:

```ts
const [state, dispatch] = useReducer(reduceRequirementTagLoadState, initialRequirementTagLoadState);
```

Implement `refresh` as:

```ts
const refresh = useCallback(async () => {
  dispatch({ type: 'start' });
  try {
    const res = await ipcBridge.requirements.tags.invoke();
    dispatch({
      type: 'success',
      tags: res.map((tag) => ({ tag: tag.tag, done: tag.done, total: tag.total })),
    });
  } catch (e) {
    if (!isHandledAuthExpiredHttpError(e)) {
      console.error('Failed to load tags', e);
      dispatch({ type: 'failure', error: String(e) });
    }
  } finally {
    dispatch({ type: 'finish' });
  }
}, []);
```

Keep the existing mount/live-event effect and return:

```ts
return { ...state, refresh };
```

- [ ] **Step 5: Run the reducer test and typecheck**

Run:

```bash
bun test ui/src/renderer/pages/requirements/requirementTagLoadState.test.ts
bun run typecheck
```

Expected: reducer tests PASS and TypeScript exits with code 0.

- [ ] **Step 6: Commit Task 1**

```bash
git add ui/src/renderer/pages/requirements/requirementTagLoadState.ts ui/src/renderer/pages/requirements/requirementTagLoadState.test.ts ui/src/renderer/pages/requirements/useRequirements.ts
git commit -m "feat(requirements): expose tag loading state"
```

---

### Task 2: Render the actionable AutoWork empty state

**Files:**
- Create: `ui/src/renderer/pages/conversation/components/AutoWorkControl.model.ts`
- Create: `ui/src/renderer/pages/conversation/components/AutoWorkControl.model.test.ts`
- Create: `ui/src/renderer/pages/conversation/components/AutoWorkControl.emptyState.test.ts`
- Modify: `ui/src/renderer/pages/conversation/components/AutoWorkControl.tsx`
- Modify: `ui/src/renderer/services/i18n/locales/zh-CN/requirements.json`
- Modify: `ui/src/renderer/services/i18n/locales/en-US/requirements.json`
- Regenerate: `ui/src/renderer/services/i18n/i18n-keys.d.ts`

**Interfaces:**
- Consumes: `useRequirementTags()` fields `tags`, `loading`, `error`, and `refresh` from Task 1.
- Produces: `AutoWorkTagPickerMode = 'loading' | 'error' | 'empty' | 'ready'`.
- Produces: `getAutoWorkTagPickerMode(tagCount, loading, error)` and `isAutoWorkEnableBlocked(enabled, mode)`.
- Produces: Select empty/loading/error content and navigation to `/requirements?new=1`.

- [ ] **Step 1: Write failing pure-model tests**

Create `AutoWorkControl.model.test.ts`:

```ts
import { describe, expect, test } from 'bun:test';
import { getAutoWorkTagPickerMode, isAutoWorkEnableBlocked } from './AutoWorkControl.model';

describe('AutoWork tag picker state', () => {
  test('distinguishes loading, ready, failure, and empty results', () => {
    expect(getAutoWorkTagPickerMode(0, true, null)).toBe('loading');
    expect(getAutoWorkTagPickerMode(2, false, null)).toBe('ready');
    expect(getAutoWorkTagPickerMode(0, false, 'offline')).toBe('error');
    expect(getAutoWorkTagPickerMode(0, false, null)).toBe('empty');
  });

  test('keeps an existing binding switchable off in every state', () => {
    for (const mode of ['loading', 'error', 'empty', 'ready'] as const) {
      expect(isAutoWorkEnableBlocked(true, mode)).toBe(false);
    }
  });

  test('only allows a disabled binding to turn on when tags are ready', () => {
    expect(isAutoWorkEnableBlocked(false, 'loading')).toBe(true);
    expect(isAutoWorkEnableBlocked(false, 'error')).toBe(true);
    expect(isAutoWorkEnableBlocked(false, 'empty')).toBe(true);
    expect(isAutoWorkEnableBlocked(false, 'ready')).toBe(false);
  });
});
```

- [ ] **Step 2: Write the failing integration-structure test**

Create `AutoWorkControl.emptyState.test.ts`:

```ts
import { readFileSync } from 'node:fs';
import { describe, expect, test } from 'bun:test';

const readSource = (url: URL) => readFileSync(url, 'utf8');

describe('AutoWork tag picker empty state', () => {
  test('renders actionable feedback and opens the canonical requirement form', () => {
    const source = readSource(new URL('./AutoWorkControl.tsx', import.meta.url));

    expect(source.includes('notFoundContent={tagPickerFeedback}')).toBe(true);
    expect(source.includes("navigate('/requirements?new=1')")).toBe(true);
    expect(source.includes("t('requirements.autowork.emptyTitle')")).toBe(true);
    expect(source.includes("t('requirements.autowork.emptyDescription')")).toBe(true);
    expect(source.includes("t('requirements.autowork.emptyCta')")).toBe(true);
    expect(source.includes("t('requirements.autowork.loadingTags')")).toBe(true);
    expect(source.includes("t('requirements.autowork.loadErrorTitle')")).toBe(true);
    expect(source.includes("t('requirements.autowork.retry')")).toBe(true);
    expect(source.includes('isAutoWorkEnableBlocked(enabled, tagPickerMode)')).toBe(true);
  });

  test('keeps Chinese and English copy keys aligned', () => {
    const zh = JSON.parse(readSource(new URL('../../../services/i18n/locales/zh-CN/requirements.json', import.meta.url)));
    const en = JSON.parse(readSource(new URL('../../../services/i18n/locales/en-US/requirements.json', import.meta.url)));
    const keys = [
      'emptyTitle',
      'emptyDescription',
      'emptyCta',
      'loadingTags',
      'loadErrorTitle',
      'loadErrorDescription',
      'retry',
    ];

    expect(keys.map((key) => zh.autowork[key] && key)).toEqual(keys);
    expect(keys.map((key) => en.autowork[key] && key)).toEqual(keys);
  });
});
```

- [ ] **Step 3: Run both tests and verify RED**

Run:

```bash
bun test ui/src/renderer/pages/conversation/components/AutoWorkControl.model.test.ts ui/src/renderer/pages/conversation/components/AutoWorkControl.emptyState.test.ts
```

Expected: FAIL because the model file, feedback JSX, route, and locale keys do not exist.

- [ ] **Step 4: Implement the pure presentation model**

Create `AutoWorkControl.model.ts`:

```ts
export type AutoWorkTagPickerMode = 'loading' | 'error' | 'empty' | 'ready';

export function getAutoWorkTagPickerMode(
  tagCount: number,
  loading: boolean,
  error: string | null
): AutoWorkTagPickerMode {
  if (loading) return 'loading';
  if (tagCount > 0) return 'ready';
  if (error) return 'error';
  return 'empty';
}

export function isAutoWorkEnableBlocked(enabled: boolean, mode: AutoWorkTagPickerMode): boolean {
  return !enabled && mode !== 'ready';
}
```

- [ ] **Step 5: Add aligned locale copy and regenerate types**

Add these keys inside both `requirements.autowork` objects.

Chinese:

```json
"emptyTitle": "还没有可用标签",
"emptyDescription": "标签会在创建需求后自动生成。先去需求平台添加一条带标签的需求。",
"emptyCta": "新建需求",
"loadingTags": "正在加载标签",
"loadErrorTitle": "标签加载失败",
"loadErrorDescription": "请检查连接后重试。",
"retry": "重新加载"
```

English:

```json
"emptyTitle": "No tags available",
"emptyDescription": "Tags are created automatically from requirements. Add a tagged requirement to get started.",
"emptyCta": "New requirement",
"loadingTags": "Loading tags",
"loadErrorTitle": "Couldn't load tags",
"loadErrorDescription": "Check your connection and try again.",
"retry": "Retry"
```

Regenerate typed keys:

```bash
bun run gen:i18n
```

Expected: `i18n-keys.d.ts` gains the seven `requirements.autowork.*` keys.

- [ ] **Step 6: Implement the compact Select feedback states**

In `AutoWorkControl.tsx`:

1. Import `Spin`, `ListAdd`, `useNavigate`, and the two model functions.
2. Read `loading`, `error`, and `refresh` from `useRequirementTags`.
3. Compute `tagPickerMode` and build `tagPickerFeedback`.

Use this behavior:

```tsx
const navigate = useNavigate();
const { tags, loading: tagsLoading, error: tagsError, refresh: refreshTags } = useRequirementTags();
const tagPickerMode = getAutoWorkTagPickerMode(tags.length, tagsLoading, tagsError);

const openNewRequirement = () => navigate('/requirements?new=1');

const tagPickerFeedback =
  tagPickerMode === 'loading' ? (
    <div className='flex items-center justify-center gap-8px px-16px py-18px text-12px text-t-tertiary'>
      <Spin size={16} />
      <span>{t('requirements.autowork.loadingTags')}</span>
    </div>
  ) : tagPickerMode === 'error' ? (
    <div className='flex flex-col items-center gap-6px px-16px py-16px text-center'>
      <span className='text-13px font-500 text-t-primary'>{t('requirements.autowork.loadErrorTitle')}</span>
      <span className='text-12px leading-16px text-t-tertiary'>
        {t('requirements.autowork.loadErrorDescription')}
      </span>
      <Button size='mini' type='text' onClick={() => void refreshTags()}>
        {t('requirements.autowork.retry')}
      </Button>
    </div>
  ) : tagPickerMode === 'empty' ? (
    <div className='flex flex-col items-center gap-8px px-14px py-16px text-center'>
      <span className='grid h-34px w-34px place-items-center rounded-full bg-fill-2 text-primary-6' aria-hidden='true'>
        <ListAdd theme='outline' size='18' strokeWidth={3} />
      </span>
      <span className='text-13px font-500 text-t-primary'>{t('requirements.autowork.emptyTitle')}</span>
      <span className='text-12px leading-16px text-t-tertiary'>
        {t('requirements.autowork.emptyDescription')}
      </span>
      <Button size='mini' type='primary' shape='round' onClick={openNewRequirement}>
        {t('requirements.autowork.emptyCta')}
      </Button>
    </div>
  ) : null;
```

Pass it to Select:

```tsx
notFoundContent={tagPickerFeedback}
```

Keep Select disabled only while AutoWork itself is enabled. Disable only invalid attempts to turn AutoWork on:

```tsx
<Switch
  checked={enabled}
  disabled={isAutoWorkEnableBlocked(enabled, tagPickerMode)}
  onChange={toggle}
/>
```

- [ ] **Step 7: Run the focused tests, i18n check, and typecheck**

Run:

```bash
bun test ui/src/renderer/pages/conversation/components/AutoWorkControl.model.test.ts ui/src/renderer/pages/conversation/components/AutoWorkControl.emptyState.test.ts ui/src/renderer/pages/conversation/components/CapabilityHeaderButton.structure.test.ts
bun run check:i18n
bun run typecheck
```

Expected: all tests PASS; i18n and TypeScript exit with code 0.

- [ ] **Step 8: Commit Task 2**

```bash
git add ui/src/renderer/pages/conversation/components/AutoWorkControl.tsx ui/src/renderer/pages/conversation/components/AutoWorkControl.model.ts ui/src/renderer/pages/conversation/components/AutoWorkControl.model.test.ts ui/src/renderer/pages/conversation/components/AutoWorkControl.emptyState.test.ts ui/src/renderer/services/i18n/locales/zh-CN/requirements.json ui/src/renderer/services/i18n/locales/en-US/requirements.json ui/src/renderer/services/i18n/i18n-keys.d.ts
git commit -m "feat(conversation): guide empty autowork tags"
```

---

### Task 3: Full regression and visual-quality verification

**Files:**
- Verify only: all files changed in Tasks 1 and 2.

**Interfaces:**
- Consumes: the completed hook state, presentation model, localized feedback JSX, and canonical new-requirement route.
- Produces: fresh evidence that the feature is type-safe, localized, buildable, and visually coherent.

- [ ] **Step 1: Run all focused regression tests together**

```bash
bun test ui/src/renderer/pages/requirements/requirementTagLoadState.test.ts ui/src/renderer/pages/conversation/components/AutoWorkControl.model.test.ts ui/src/renderer/pages/conversation/components/AutoWorkControl.emptyState.test.ts ui/src/renderer/pages/conversation/components/CapabilityHeaderButton.structure.test.ts ui/src/renderer/pages/conversation/components/ChatLayout/advancedControls.test.ts ui/src/renderer/pages/guid/GuidPage.advancedControls.test.ts
```

Expected: every listed test passes with zero failures.

- [ ] **Step 2: Run repository UI quality gates**

```bash
bun run check:i18n
bun run check:theme
bun run check:icons
bun run typecheck
bun run build:ui
```

Expected: every command exits with code 0; the Vite build completes without TypeScript or bundling errors.

- [ ] **Step 3: Inspect the final diff for scope and generated artifacts**

```bash
git diff --check
git status --short
git diff HEAD~2 -- ui/src/renderer/pages/requirements ui/src/renderer/pages/conversation/components/AutoWorkControl.tsx ui/src/renderer/pages/conversation/components/AutoWorkControl.model.ts ui/src/renderer/services/i18n
```

Expected: no whitespace errors, no unrelated files, and all seven locale keys appear in both locale bundles and the generated declaration.

- [ ] **Step 4: Verify the interaction in the running UI when the local app is available**

Open a conversation with zero requirement tags and confirm:

1. Opening AutoWork shows the normal panel.
2. Opening Tag shows the compact icon, title, explanatory copy, and one rounded primary CTA.
3. The AutoWork switch is disabled while no tag exists.
4. Keyboard Tab focuses `新建需求`; Enter opens `/requirements?new=1` and its drawer.
5. Dark and light themes preserve readable contrast with no clipping at 240px panel width.
6. After a tagged requirement exists, the normal `tag (done/total)` option list returns and the switch can be enabled.

If the local backend cannot provide these states, report visual verification as unavailable rather than claiming it passed; automated verification remains mandatory.

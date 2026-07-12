# Desktop Companion Memory Bubble Layout Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Render the desktop companion's unread suggestion panel outside the character, show every item's full content, and constrain long lists to a responsive internal scroller without moving the saved companion position.

**Architecture:** A new pure geometry module chooses above/left/right placement in physical pixels from the desk-window anchor and host monitor. `CompanionPage` owns one expanded-window session that captures the original native rectangle, applies either chat or memory-panel geometry, suppresses persistence for internal moves, and restores the original rectangle exactly. JSX and CSS render the panel as a sibling of the figure stage with semantic buttons and bounded scrolling.

**Tech Stack:** React 19, TypeScript, Bun test/Vitest APIs, Tauri v2 window API, CSS.

## Global Constraints

- The memory panel keeps at least 12 logical pixels between its visual bounds and the character.
- Desired panel width is `clamp(300px, 32vw, 360px)`; desired content height is `min(320px, 42vh)` with a 160px preferred minimum.
- Suggestion titles and bodies render in full; unbroken text uses `overflow-wrap: anywhere`.
- Overflow scrolls inside the panel and never grows the native window without a cap.
- Internal native-window size/position changes never patch `companion_x` or `companion_y`.
- Closing the last expanded surface restores the exact pre-expansion physical rectangle.
- Opening the memory panel temporarily hides other transient companion chrome without clearing its state; closing the panel restores it.
- Existing suggestion data, click destination, unread count, accept/dismiss semantics, and pagination remain unchanged.
- Implementation changes remain uncommitted, as requested by the user.

## File Map

- Create `ui/src/renderer/pages/companion/memoryPanelGeometry.ts`: pure monitor selection and panel/window placement.
- Create `ui/src/renderer/pages/companion/memoryPanelGeometry.test.ts`: center, edge, scale, custom-size, and multi-monitor geometry coverage.
- Modify `ui/src/renderer/pages/companion/index.tsx`: expanded-window session, exact restoration, persistence suppression, semantic panel markup, Escape/focus behavior.
- Modify `ui/src/renderer/pages/companion/companion.css`: non-overlapping directional layout, full text, responsive max-height, internal scrolling, focus/hover polish.
- Modify `ui/src/renderer/pages/companion/companionChromeLayout.test.ts`: regression contract for DOM order and CSS truncation removal.

---

### Task 1: Pure memory-panel geometry

**Files:**

- Create: `ui/src/renderer/pages/companion/memoryPanelGeometry.ts`
- Create: `ui/src/renderer/pages/companion/memoryPanelGeometry.test.ts`

**Interfaces:**

- Consumes: `GeomRect` and `GeomSize` from `windowGeometry.ts`.
- Produces: `chooseMemoryPanelLayout(input: MemoryPanelLayoutInput): MemoryPanelLayout`.
- Produces: `pickHostMonitor(anchor: GeomRect, monitors: GeomRect[]): GeomRect | null`.

- [ ] **Step 1: Write failing geometry tests**

```ts
import { describe, expect, it } from 'vitest';
import { chooseMemoryPanelLayout, pickHostMonitor } from './memoryPanelGeometry';

const monitor = { x: 0, y: 0, width: 1920, height: 1080 };

it('places the panel above with a 12px gap when headroom is available', () => {
  const layout = chooseMemoryPanelLayout({
    anchor: { x: 800, y: 700, width: 240, height: 214 },
    monitor,
    scaleFactor: 1,
    desiredPanel: { width: 340, height: 300 },
  });
  expect(layout.placement).toBe('above');
  expect(layout.windowRect.y + layout.panelMaxHeight + 12).toBe(700);
  expect(layout.anchorOffset.y).toBe(312);
});

it('flips right near the top when the right side has more room', () => {
  const layout = chooseMemoryPanelLayout({
    anchor: { x: 120, y: 10, width: 240, height: 214 },
    monitor,
    scaleFactor: 1,
    desiredPanel: { width: 340, height: 300 },
  });
  expect(layout.placement).toBe('right');
  expect(layout.windowRect.x).toBe(120);
  expect(layout.anchorOffset.x).toBe(0);
});

it('uses physical gap and dimensions at 150 percent scale', () => {
  const layout = chooseMemoryPanelLayout({
    anchor: { x: 900, y: 900, width: 360, height: 321 },
    monitor: { x: 0, y: 0, width: 2880, height: 1800 },
    scaleFactor: 1.5,
    desiredPanel: { width: 340, height: 300 },
  });
  expect(layout.gap).toBe(18);
  expect(layout.panelMaxWidth).toBe(510);
});

it('keeps a large custom companion and panel disjoint', () => {
  const anchor = { x: 700, y: 500, width: 400, height: 464 };
  const layout = chooseMemoryPanelLayout({ anchor, monitor, scaleFactor: 1, desiredPanel: { width: 360, height: 320 } });
  const panelBottom = layout.windowRect.y + layout.panelMaxHeight;
  expect(panelBottom + layout.gap).toBeLessThanOrEqual(anchor.y);
});

it('picks the monitor with the largest overlap, including negative coordinates', () => {
  const left = { x: -1920, y: 0, width: 1920, height: 1080 };
  expect(pickHostMonitor({ x: -400, y: 600, width: 240, height: 214 }, [monitor, left])).toEqual(left);
});
```

- [ ] **Step 2: Run the tests and verify RED**

Run: `cd ui && bun test src/renderer/pages/companion/memoryPanelGeometry.test.ts`

Expected: FAIL because `memoryPanelGeometry.ts` does not exist.

- [ ] **Step 3: Implement deterministic placement**

```ts
import type { GeomRect, GeomSize } from './windowGeometry';

export type MemoryPanelPlacement = 'above' | 'left' | 'right';

export interface MemoryPanelLayoutInput {
  anchor: GeomRect;
  monitor: GeomRect;
  scaleFactor: number;
  desiredPanel: GeomSize;
}

export interface MemoryPanelLayout {
  placement: MemoryPanelPlacement;
  windowRect: GeomRect;
  panelMaxWidth: number;
  panelMaxHeight: number;
  gap: number;
  anchorOffset: { x: number; y: number };
}

const overlapArea = (a: GeomRect, b: GeomRect): number => {
  const width = Math.max(0, Math.min(a.x + a.width, b.x + b.width) - Math.max(a.x, b.x));
  const height = Math.max(0, Math.min(a.y + a.height, b.y + b.height) - Math.max(a.y, b.y));
  return width * height;
};

export const pickHostMonitor = (anchor: GeomRect, monitors: GeomRect[]): GeomRect | null =>
  monitors.reduce<GeomRect | null>((best, monitor) => {
    if (!best) return monitor;
    return overlapArea(anchor, monitor) > overlapArea(anchor, best) ? monitor : best;
  }, null);

export function chooseMemoryPanelLayout(input: MemoryPanelLayoutInput): MemoryPanelLayout {
  const { anchor, monitor } = input;
  const scale = Number.isFinite(input.scaleFactor) && input.scaleFactor > 0 ? input.scaleFactor : 1;
  const gap = Math.round(12 * scale);
  const desiredWidth = Math.round(input.desiredPanel.width * scale);
  const desiredHeight = Math.round(input.desiredPanel.height * scale);
  const top = Math.max(0, anchor.y - monitor.y - gap);
  const left = Math.max(0, anchor.x - monitor.x - gap);
  const right = Math.max(0, monitor.x + monitor.width - (anchor.x + anchor.width) - gap);
  const preferredMinHeight = Math.round(160 * scale);
  const placement: MemoryPanelPlacement =
    top >= desiredHeight
      ? 'above'
      : Math.max(left, right) >= desiredWidth
        ? right >= left
          ? 'right'
          : 'left'
        : top * Math.min(desiredWidth, monitor.width) >=
            Math.max(left, right) * Math.min(desiredHeight, anchor.y + anchor.height - monitor.y)
          ? 'above'
          : right >= left
            ? 'right'
            : 'left';

  const panelMaxWidth = Math.max(1, Math.min(desiredWidth, placement === 'above' ? monitor.width : placement === 'left' ? left : right));
  const sideHeight = Math.max(1, Math.min(desiredHeight, anchor.y + anchor.height - monitor.y));
  const panelMaxHeight = placement === 'above' ? Math.max(1, Math.min(desiredHeight, top)) : Math.max(Math.min(preferredMinHeight, sideHeight), sideHeight);
  const width = placement === 'above' ? Math.max(anchor.width, panelMaxWidth) : anchor.width + gap + panelMaxWidth;
  const height = placement === 'above' ? anchor.height + gap + panelMaxHeight : Math.max(anchor.height, panelMaxHeight);
  const unclampedX = placement === 'above' ? anchor.x + Math.round((anchor.width - width) / 2) : placement === 'left' ? anchor.x - gap - panelMaxWidth : anchor.x;
  const unclampedY = placement === 'above' ? anchor.y - gap - panelMaxHeight : anchor.y + anchor.height - height;
  const x = Math.min(Math.max(unclampedX, monitor.x), monitor.x + monitor.width - width);
  const y = Math.min(Math.max(unclampedY, monitor.y), monitor.y + monitor.height - height);
  return {
    placement,
    windowRect: { x, y, width, height },
    panelMaxWidth,
    panelMaxHeight,
    gap,
    anchorOffset: { x: anchor.x - x, y: anchor.y - y },
  };
}
```

- [ ] **Step 4: Run the focused test and verify GREEN**

Run: `cd ui && bun test src/renderer/pages/companion/memoryPanelGeometry.test.ts`

Expected: all geometry tests pass with 0 failures.

---

### Task 2: Exact native-window expansion and restoration

**Files:**

- Modify: `ui/src/renderer/pages/companion/index.tsx`
- Test: `ui/src/renderer/pages/companion/memoryPanelGeometry.test.ts`

**Interfaces:**

- Consumes: `chooseMemoryPanelLayout`, `pickHostMonitor`, and `MemoryPanelPlacement`.
- Produces: one `ExpandedWindowSession` whose `anchor` is the exact pre-expansion physical rectangle.
- Produces: `syncExpandedWindow(mode: 'memory' | 'chat' | null): Promise<void>`.

- [ ] **Step 1: Add failing structural assertions for expansion ownership**

Append tests that read `index.tsx` and assert it contains `showSuggestions ? 'memory'`, `expandedWindowSessionRef`, and an `onMoved` guard that skips persistence while an internal layout session is active.

- [ ] **Step 2: Run the structural test and verify RED**

Run: `cd ui && bun test src/renderer/pages/companion/companionChromeLayout.test.ts`

Expected: FAIL because the memory panel does not participate in native-window expansion.

- [ ] **Step 3: Replace the chat-only size flag with an expanded-window session**

```ts
interface ExpandedWindowSession {
  anchor: { x: number; y: number; width: number; height: number };
  scaleFactor: number;
  mode: 'memory' | 'chat';
}

const expandedWindowSessionRef = useRef<ExpandedWindowSession | null>(null);
const internalWindowLayoutRef = useRef(false);
const [memoryPanelPlacement, setMemoryPanelPlacement] = useState<MemoryPanelPlacement>('above');
const [memoryPanelMaxHeight, setMemoryPanelMaxHeight] = useState(320);
const [memoryPanelMaxWidth, setMemoryPanelMaxWidth] = useState(340);
```

`syncExpandedWindow` must capture `outerPosition`, `outerSize`, and `scaleFactor` only when the first expanded surface opens. For memory mode, use `pickHostMonitor` and `chooseMemoryPanelLayout`, set the native size before position, then expose logical `panelMaxWidth`/`panelMaxHeight` to CSS state. For chat mode, preserve the existing responsive target sizing but derive its placement from the saved anchor. For `null`, set the saved size and position exactly and clear the session only after both calls resolve.

- [ ] **Step 4: Prevent internal layout moves from changing saved coordinates**

```ts
unlisten = await getCurrentWindow().onMoved(({ payload }) => {
  if (internalWindowLayoutRef.current || expandedWindowSessionRef.current) return;
  // existing debounced patchCompanion path remains unchanged below this guard
});
```

Keep the guard active during size/position application and restoration. Existing user drag persistence remains unchanged in the normal desk state.

- [ ] **Step 5: Include memory-panel state and empty-list cleanup**

```ts
const expandedMode: 'memory' | 'chat' | null = showSuggestions
  ? 'memory'
  : hasBubble || composerOpen
    ? 'chat'
    : null;

useEffect(() => {
  if (!isTauriRuntime()) return;
  void syncExpandedWindow(expandedMode);
}, [expandedMode, syncExpandedWindow]);

useEffect(() => {
  if (suggestions.length === 0 && showSuggestions) setShowSuggestions(false);
}, [showSuggestions, suggestions.length]);
```

- [ ] **Step 6: Run focused tests and type checking**

Run: `cd ui && bun test src/renderer/pages/companion/memoryPanelGeometry.test.ts src/renderer/pages/companion/companionChromeLayout.test.ts && bun run typecheck`

Expected: tests and TypeScript pass with 0 errors.

---

### Task 3: Non-overlapping panel markup and polished bounded scrolling

**Files:**

- Modify: `ui/src/renderer/pages/companion/index.tsx`
- Modify: `ui/src/renderer/pages/companion/companion.css`
- Modify: `ui/src/renderer/pages/companion/companionChromeLayout.test.ts`

**Interfaces:**

- Consumes: `memoryPanelPlacement`, `memoryPanelMaxWidth`, and `memoryPanelMaxHeight` from Task 2.
- Produces: `#nomi-companion-memory-panel`, controlled by the unread badge button.

- [ ] **Step 1: Add failing DOM/CSS regression assertions**

```ts
test('renders the memory panel as a sibling before the figure stage', () => {
  const panel = companionSource.indexOf("id='nomi-companion-memory-panel'");
  const stage = companionSource.indexOf("className='nomi-companion-stage'");
  expect(panel).toBeGreaterThan(-1);
  expect(panel).toBeLessThan(stage);
});

test('shows full suggestion content inside a bounded scroller', () => {
  expect(companionCss.includes('max-height: var(--memory-panel-max-height)')).toBe(true);
  expect(companionCss.includes('overflow-y: auto')).toBe(true);
  expect(companionCss.includes('-webkit-line-clamp')).toBe(false);
  expect(companionCss.includes('overflow-wrap: anywhere')).toBe(true);
});
```

- [ ] **Step 2: Run the structural test and verify RED**

Run: `cd ui && bun test src/renderer/pages/companion/companionChromeLayout.test.ts`

Expected: FAIL because the panel is still absolutely positioned inside the stage and body text is clamped.

- [ ] **Step 3: Move the panel outside the figure stage and make controls semantic**

```tsx
<div
  className={`nomi-companion-stage-shell nomi-companion-stage-shell--${memoryPanelPlacement}`}
  style={{
    '--memory-panel-max-width': `${memoryPanelMaxWidth}px`,
    '--memory-panel-max-height': `${memoryPanelMaxHeight}px`,
  } as React.CSSProperties}
>
  {showSuggestions && suggestions.length > 0 && (
    <div id='nomi-companion-memory-panel' className='nomi-companion-suggestions' data-companion-hit>
      {suggestions.map((suggestion) => (
        <button
          key={suggestion.id}
          type='button'
          className='nomi-companion-suggestions__item'
          onClick={() => void clickSuggestion(suggestion)}
        >
          <span className='nomi-companion-suggestions__title'>{suggestion.title}</span>
          <span className='nomi-companion-suggestions__body'>{suggestion.body}</span>
        </button>
      ))}
    </div>
  )}
  <div className='nomi-companion-stage'>…existing badge and figure…</div>
</div>
```

Change the badge wrapper to `button type='button'`, add `ref={unreadBadgeRef}`, `aria-expanded={showSuggestions}`, `aria-controls='nomi-companion-memory-panel'`, and `aria-label={t('nomi.tabs.suggestions')}`.

- [ ] **Step 4: Add Escape close and focus restoration**

```ts
const unreadBadgeRef = useRef<HTMLButtonElement | null>(null);

useEffect(() => {
  if (!showSuggestions) return;
  const close = (event: KeyboardEvent) => {
    if (event.key !== 'Escape') return;
    setShowSuggestions(false);
    requestAnimationFrame(() => unreadBadgeRef.current?.focus());
  };
  window.addEventListener('keydown', close);
  return () => window.removeEventListener('keydown', close);
}, [showSuggestions]);
```

- [ ] **Step 5: Replace popover CSS with directional flex layout and bounded scrolling**

```css
.nomi-companion-stage-shell {
  flex: none;
  display: flex;
  align-items: flex-end;
  justify-content: center;
  max-width: 100%;
}

.nomi-companion-stage-shell--above {
  flex-direction: column;
  align-items: center;
}

.nomi-companion-stage-shell--left { flex-direction: row; }
.nomi-companion-stage-shell--right { flex-direction: row-reverse; }

.nomi-companion-suggestions {
  width: min(92vw, var(--memory-panel-max-width, 340px));
  max-height: var(--memory-panel-max-height, min(320px, 42vh));
  overflow-y: auto;
  overflow-x: hidden;
  overscroll-behavior: contain;
  scrollbar-width: thin;
  scrollbar-color: var(--bg-4) transparent;
  padding: 6px;
  border: 1px solid var(--border-base);
  border-radius: 14px;
  background: var(--bg-base);
  box-shadow: 0 8px 24px rgba(0, 0, 0, 0.18);
  line-height: 1.55;
}

.nomi-companion-stage-shell--above .nomi-companion-suggestions { margin-bottom: 12px; }
.nomi-companion-stage-shell--left .nomi-companion-suggestions { margin-right: 12px; }
.nomi-companion-stage-shell--right .nomi-companion-suggestions { margin-left: 12px; }

.nomi-companion-suggestions__item {
  display: block;
  width: 100%;
  padding: 9px 10px;
  border: 0;
  border-radius: 8px;
  background: transparent;
  color: inherit;
  font: inherit;
  text-align: left;
  cursor: pointer;
}

.nomi-companion-suggestions__item:focus-visible {
  outline: 2px solid var(--primary);
  outline-offset: -2px;
}

.nomi-companion-suggestions__title,
.nomi-companion-suggestions__body {
  display: block;
  overflow-wrap: anywhere;
}
```

- [ ] **Step 6: Run the focused UI tests and type checker**

Run: `cd ui && bun test src/renderer/pages/companion/companionChromeLayout.test.ts src/renderer/pages/companion/memoryPanelGeometry.test.ts src/renderer/pages/companion/companionCapturePolicy.test.ts && bun run typecheck`

Expected: all tests pass and TypeScript reports 0 errors.

---

### Task 4: Regression and visual verification

**Files:**

- Verify: all files above.

- [ ] **Step 1: Run the complete companion-page test group**

Run: `cd ui && bun test src/renderer/pages/companion`

Expected: all companion-page tests pass with 0 failures.

- [ ] **Step 2: Run full UI type checking and production build**

Run: `cd ui && bun run typecheck && bun run build`

Expected: both commands exit 0.

- [ ] **Step 3: Inspect implementation integrity without committing**

Run: `git diff --check && git status --short && git diff -- ui/src/renderer/pages/companion docs/superpowers/plans/2026-07-11-companion-memory-bubble-layout.md`

Expected: only the plan and intended companion UI/test files are modified; no whitespace errors and no implementation commit exists.

- [ ] **Step 4: Verify the visible state in the desktop runtime when available**

Open the companion with four long suggestion items and check the user's reported state at a normal screen position, each screen edge, 125%/150% scale, a 400px custom figure, and a secondary monitor. Confirm the character remains unobscured, every body is reachable, overflow scrolls inside the panel, and closing restores the original window rectangle.

## Review Checklist

- The panel is never a child of the figure stage and has no `top/right` absolute placement.
- The panel and figure are separated by 12 logical pixels in every chosen direction.
- Full text is rendered without `-webkit-line-clamp` or ellipsis.
- Panel height is capped and excess content scrolls internally.
- The current monitor and scale factor drive physical native-window geometry.
- Expanded-window internal moves cannot persist as user coordinates.
- The exact original rectangle is restored after close.
- Badge and suggestion items are keyboard-operable; Escape closes and restores focus.
- Suggestion business behavior is unchanged.
- No code commit is created.

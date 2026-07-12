# Desktop Companion Detached Memory Panel Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Replace the in-window desktop-companion memory panel with one reusable Tauri window that never moves or resizes the companion and closes safely when it loses focus.

**Architecture:** The companion window remains the single source of suggestion data and owns an explicit request-scoped controller. A hidden `nomi-memory-panel` Webview renders and measures the card, then a pure physical-pixel geometry function places that window inside the monitor work area without intersecting the companion anchor. Rust exposes only fixed-label prepare/place/show/hide commands and rejects stale request IDs, while a pure frontend lifecycle reducer prevents late focus and close events from affecting newer opens.

**Tech Stack:** React 19, TypeScript 5.8, Bun test/Vitest, Tauri 2, Rust, CSS theme variables, Tauri window and event APIs.

## Global Constraints

- The memory panel native label is exactly `nomi-memory-panel`; it must not match the existing `companion-*` cleanup prefix.
- The memory panel route is exactly `#/nomi-memory-panel` and is outside the authenticated main layout, like `/companion`.
- The companion native window rectangle is read-only for every memory-panel operation.
- All native geometry is calculated and applied in physical pixels; 12 logical pixels is the panel-to-companion gap.
- The panel must fit entirely inside its monitor `workArea` and must not intersect the companion `anchorRect`.
- Minimum readable panel size is 280×120 logical pixels; smaller results fall back to the main suggestion page.
- The panel is a single reusable Webview, not one window per companion and not one window per open.
- Do not grant `core:window:allow-create`; use fixed-purpose Rust commands.
- Suggestions in the companion page remain the authoritative data source; the panel never independently fetches suggestions.
- Blur closes without restoring focus; Escape closes and restores focus to the badge; suggestion activation is acknowledged before close.
- `prefers-reduced-motion: reduce` removes the close delay and scale animation.
- Do not add WebGL, `backdrop-filter`, large transparent shadow padding, or a second memory-panel implementation path.
- Work in the current `main` checkout: the user explicitly requested commit/pull followed by the implementation and then confirmed execution.

---

## File Map

**Create**

- `ui/src/renderer/pages/companion/detachedMemoryPanelGeometry.ts` — pure monitor selection, non-intersecting placement, and fallback decision.
- `ui/src/renderer/pages/companion/detachedMemoryPanelGeometry.test.ts` — Dock/taskbar, edge, mixed-DPI, negative-coordinate, and minimum-size regressions.
- `ui/src/renderer/pages/companion/memoryPanelProtocol.ts` — event names, payload types, request IDs, and pure owner lifecycle reducer.
- `ui/src/renderer/pages/companion/memoryPanelProtocol.test.ts` — stale-event and close/open race tests.
- `ui/src/renderer/pages/companion/memoryPanelShell.ts` — guarded Tauri command/event adapter.
- `ui/src/renderer/pages/companion/useDetachedMemoryPanel.ts` — owner-side prepare/measure/place/show/activate/close controller.
- `ui/src/renderer/pages/memoryPanel/index.tsx` — standalone panel route and focus-loss lifecycle.
- `ui/src/renderer/pages/memoryPanel/memoryPanel.css` — card, scrolling, focus, opening and closing visuals.
- `ui/src/renderer/pages/memoryPanel/memoryPanelRoute.test.ts` — source-level route, semantic, scrolling, and reduced-motion contract.
- `apps/desktop/src/memory_panel_window.rs` — fixed-label single-window state, validation, Tauri commands, and Rust tests.

**Modify**

- `apps/desktop/src/main.rs` — register module/state/commands and invalidate the panel owner during companion reconciliation.
- `apps/desktop/capabilities/default.json` — add exact `nomi-memory-panel` window scope without create permission.
- `ui/src/renderer/components/layout/Router.tsx` — lazy standalone `/nomi-memory-panel` route.
- `ui/src/renderer/pages/companion/index.tsx` — use detached controller, keep badge, remove memory from shared expanded-window mode and DOM.
- `ui/src/renderer/pages/companion/companion.css` — remove obsolete in-window memory panel and memory-reading layout rules.
- `ui/src/renderer/pages/companion/companionChromeLayout.test.ts` — assert memory no longer participates in shared native resizing.
- `ui/src/renderer/pages/companion/companionCapturePolicy.ts` — remove obsolete `showSuggestions` input.
- `ui/src/renderer/pages/companion/companionCapturePolicy.test.ts` — update policy fixtures.
- `ui/src/renderer/pages/companion/memoryPanelGeometry.ts` — retain only monitor/desk-restore helpers still used by chat expansion.
- `ui/src/renderer/pages/companion/memoryPanelGeometry.test.ts` — retain chat restore and monitor-selection coverage.
- `docs/superpowers/specs/2026-07-11-companion-memory-popover-window-design.md` — align command signatures with the implementation after verification.

---

### Task 1: Detached Physical-Pixel Placement

**Files:**
- Create: `ui/src/renderer/pages/companion/detachedMemoryPanelGeometry.ts`
- Create: `ui/src/renderer/pages/companion/detachedMemoryPanelGeometry.test.ts`

**Interfaces:**
- Consumes: `GeomRect` and `GeomSize` from `windowGeometry.ts`.
- Produces: `chooseDetachedMemoryPanelLayout(input: DetachedMemoryPanelInput): DetachedMemoryPanelResult`.

- [ ] **Step 1: Write the failing geometry tests**

Create tests using this API and assertions:

```ts
import { describe, expect, it } from 'vitest';
import { chooseDetachedMemoryPanelLayout, type DetachedMonitor } from './detachedMemoryPanelGeometry';

const monitor: DetachedMonitor = {
  id: 'main',
  bounds: { x: 0, y: 0, width: 1920, height: 1080 },
  workArea: { x: 0, y: 0, width: 1720, height: 1080 },
  scaleFactor: 1,
};

describe('chooseDetachedMemoryPanelLayout', () => {
  it('keeps a companion in the right-side Dock area untouched', () => {
    const anchor = { x: 1660, y: 760, width: 240, height: 214 };
    const result = chooseDetachedMemoryPanelLayout({
      anchor,
      monitors: [monitor],
      logicalPanel: { width: 340, height: 300 },
    });

    expect(result).toMatchObject({ kind: 'placed', placement: 'above', monitorId: 'main' });
    if (result.kind !== 'placed') throw new Error('expected placement');
    expect(result.anchorRect).toEqual(anchor);
    expect(result.panelRect.x + result.panelRect.width).toBeLessThanOrEqual(1720);
    expect(result.panelRect.y + result.panelRect.height).toBeLessThanOrEqual(anchor.y - result.gap);
  });

  it('chooses the left side at the top-right edge', () => {
    const result = chooseDetachedMemoryPanelLayout({
      anchor: { x: 1660, y: 8, width: 240, height: 214 },
      monitors: [{ ...monitor, workArea: monitor.bounds }],
      logicalPanel: { width: 340, height: 300 },
    });
    expect(result).toMatchObject({ kind: 'placed', placement: 'left' });
  });

  it('uses the largest overlap monitor with negative coordinates and its scale', () => {
    const left: DetachedMonitor = {
      id: 'left-150',
      bounds: { x: -2880, y: 0, width: 2880, height: 1800 },
      workArea: { x: -2880, y: 48, width: 2880, height: 1752 },
      scaleFactor: 1.5,
    };
    const result = chooseDetachedMemoryPanelLayout({
      anchor: { x: -500, y: 1200, width: 360, height: 321 },
      monitors: [monitor, left],
      logicalPanel: { width: 340, height: 300 },
    });
    expect(result).toMatchObject({ kind: 'placed', monitorId: 'left-150', gap: 18 });
  });

  it('falls back instead of overlapping when no readable region exists', () => {
    const result = chooseDetachedMemoryPanelLayout({
      anchor: { x: 0, y: 0, width: 300, height: 300 },
      monitors: [{ id: 'tiny', bounds: { x: 0, y: 0, width: 320, height: 320 }, workArea: { x: 0, y: 0, width: 320, height: 320 }, scaleFactor: 1 }],
      logicalPanel: { width: 340, height: 300 },
    });
    expect(result).toEqual({ kind: 'fallback', reason: 'insufficient-space' });
  });
});
```

Add table-driven cases for Dock/taskbar on left, right, top, and bottom; 1.25/1.5/2 scale; a 400×464 custom companion; and panel/anchor intersection checks.

- [ ] **Step 2: Run the test and verify RED**

Run:

```bash
cd ui && bun test src/renderer/pages/companion/detachedMemoryPanelGeometry.test.ts
```

Expected: FAIL because `detachedMemoryPanelGeometry.ts` does not exist.

- [ ] **Step 3: Implement the pure geometry API**

Create these exact exported types and function:

```ts
import type { GeomRect, GeomSize } from './windowGeometry';

export type DetachedMemoryPanelPlacement = 'above' | 'left' | 'right';

export interface DetachedMonitor {
  id: string;
  bounds: GeomRect;
  workArea: GeomRect;
  scaleFactor: number;
}

export interface DetachedMemoryPanelInput {
  anchor: GeomRect;
  monitors: DetachedMonitor[];
  logicalPanel: GeomSize;
  logicalMinimum?: GeomSize;
  logicalGap?: number;
}

export type DetachedMemoryPanelResult =
  | {
      kind: 'placed';
      placement: DetachedMemoryPanelPlacement;
      panelRect: GeomRect;
      anchorRect: GeomRect;
      monitorId: string;
      scaleFactor: number;
      gap: number;
    }
  | { kind: 'fallback'; reason: 'no-monitor' | 'insufficient-space' };

export function chooseDetachedMemoryPanelLayout(input: DetachedMemoryPanelInput): DetachedMemoryPanelResult;
```

Implementation rules: select the host by overlap with `bounds`; normalize invalid scale to 1; calculate desired/minimum/gap in physical pixels; build above/left/right candidates constrained only by `workArea`; reject candidates smaller than the minimum or intersecting `anchor`; prefer a full-size above candidate, otherwise the full-size side with more width, otherwise the readable candidate with the largest area; never modify `anchor`.

- [ ] **Step 4: Verify GREEN and the full geometry suite**

Run:

```bash
cd ui && bun test src/renderer/pages/companion/detachedMemoryPanelGeometry.test.ts src/renderer/pages/companion/memoryPanelGeometry.test.ts
```

Expected: all tests pass with zero failures.

- [ ] **Step 5: Commit Task 1**

```bash
git add ui/src/renderer/pages/companion/detachedMemoryPanelGeometry.ts ui/src/renderer/pages/companion/detachedMemoryPanelGeometry.test.ts
git commit -m "feat(companion): add detached memory panel geometry"
```

---

### Task 2: Fixed-Purpose Rust Window Manager

**Files:**
- Create: `apps/desktop/src/memory_panel_window.rs`
- Modify: `apps/desktop/src/main.rs`
- Modify: `apps/desktop/capabilities/default.json`

**Interfaces:**
- Consumes: `webui_init_script`, `run_on_main_thread_task`, `DesktopServer`, and Tauri `AppHandle`.
- Produces commands `prepare_companion_memory_panel`, `place_companion_memory_panel`, `show_companion_memory_panel`, `hide_companion_memory_panel` and managed `MemoryPanelWindowState`.

- [ ] **Step 1: Write Rust state and validation tests first**

Create `memory_panel_window.rs` with a `#[cfg(test)]` module that asserts:

```rust
#[test]
fn stale_request_cannot_hide_new_owner() {
    let state = MemoryPanelWindowState::default();
    assert!(state.place("r1", "companion-a", PhysicalRect::new(10, 20, 340, 300)).is_ok());
    assert!(state.place("r2", "companion-b", PhysicalRect::new(30, 40, 340, 300)).is_ok());
    assert!(!state.finish_hide("r1"));
    assert_eq!(state.snapshot().request_id.as_deref(), Some("r2"));
}

#[test]
fn rejects_unsafe_panel_rectangles() {
    assert!(PhysicalRect::new(0, 0, 0, 300).validate().is_err());
    assert!(PhysicalRect::new(0, 0, 5000, 300).validate().is_err());
    assert!(PhysicalRect::new(0, 0, 340, 5000).validate().is_err());
}

#[test]
fn invalidates_owner_when_companion_is_disabled() {
    let state = MemoryPanelWindowState::default();
    state.place("r1", "companion-a", PhysicalRect::new(10, 20, 340, 300)).unwrap();
    let enabled = std::collections::HashSet::from(["companion-b".to_string()]);
    assert!(state.invalidate_owner_unless(&enabled));
    assert!(state.snapshot().request_id.is_none());
}
```

- [ ] **Step 2: Run the Rust tests and verify RED**

Run:

```bash
cargo test -p nomifun-desktop memory_panel_window::tests -- --nocapture
```

Expected: FAIL because the module and types are missing.

- [ ] **Step 3: Implement state, validation, and fixed-label commands**

Use these public definitions:

```rust
pub const MEMORY_PANEL_LABEL: &str = "nomi-memory-panel";

#[derive(Clone, Copy, Debug, serde::Deserialize, PartialEq, Eq)]
pub struct PhysicalRect {
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
}

impl PhysicalRect {
    pub fn new(x: i32, y: i32, width: u32, height: u32) -> Self;
    pub fn validate(self) -> Result<Self, String>;
}

#[derive(Default)]
pub struct MemoryPanelWindowState(std::sync::Mutex<MemoryPanelSession>);

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MemoryPanelSessionSnapshot {
    pub request_id: Option<String>,
    pub owner_companion_id: Option<String>,
    pub rect: Option<PhysicalRect>,
}

impl MemoryPanelWindowState {
    pub fn place(&self, request_id: &str, owner_companion_id: &str, rect: PhysicalRect) -> Result<(), String>;
    pub fn can_show(&self, request_id: &str, owner_companion_id: &str) -> bool;
    pub fn finish_hide(&self, request_id: &str) -> bool;
    pub fn invalidate_owner_unless(&self, enabled: &std::collections::HashSet<String>) -> bool;
    pub fn snapshot(&self) -> MemoryPanelSessionSnapshot;
}
```

`PhysicalRect::validate` accepts width/height from 1 through 4096. `place` rejects empty request or owner IDs. Preparing is idempotent and creates exactly one hidden window using `index.html#/nomi-memory-panel`, the existing initialization script, `decorations(false)`, `transparent(true)`, `resizable(false)`, `always_on_top(true)`, `skip_taskbar(true)`, and `shadow(true)`.

`place` hides any visible old session, records the new request, then sets `PhysicalSize` and `PhysicalPosition` while hidden. `show` requires matching request/owner and a placed rectangle. `hide` returns `false` for stale requests instead of hiding the current session.

- [ ] **Step 4: Register state, commands, reconciliation, and capability scope**

In `main.rs`:

```rust
mod memory_panel_window;

// Builder chain
.manage(memory_panel_window::MemoryPanelWindowState::default())
.invoke_handler(tauri::generate_handler![
    check_for_updates,
    sync_companion_windows,
    memory_panel_window::prepare_companion_memory_panel,
    memory_panel_window::place_companion_memory_panel,
    memory_panel_window::show_companion_memory_panel,
    memory_panel_window::hide_companion_memory_panel,
    webui_get_status,
    webui_start,
    webui_stop,
    set_keep_awake,
    set_tray_labels
])
```

Add `MemoryPanelWindowState` to `sync_companion_windows`; derive enabled companion IDs from `specs`, call `invalidate_owner_unless`, and hide `nomi-memory-panel` on the main thread when invalidated.

In `capabilities/default.json`, change only the window scope:

```json
"windows": ["main", "companion-*", "nomi-memory-panel"]
```

Do not add `core:window:allow-create` or `core:webview:allow-create-webview-window`.

- [ ] **Step 5: Verify Rust and formatting**

Run:

```bash
cargo fmt --check
cargo test -p nomifun-desktop memory_panel_window::tests -- --nocapture
cargo test -p nomifun-desktop main_thread_task_runner_executes_work_inside_dispatcher -- --nocapture
```

Expected: formatting clean and all selected tests pass.

- [ ] **Step 6: Commit Task 2**

```bash
git add apps/desktop/src/memory_panel_window.rs apps/desktop/src/main.rs apps/desktop/capabilities/default.json
git commit -m "feat(desktop): manage detached memory panel window"
```

---

### Task 3: Request-Scoped Protocol and Lifecycle

**Files:**
- Create: `ui/src/renderer/pages/companion/memoryPanelProtocol.ts`
- Create: `ui/src/renderer/pages/companion/memoryPanelProtocol.test.ts`
- Create: `ui/src/renderer/pages/companion/memoryPanelShell.ts`

**Interfaces:**
- Consumes: `ICompanionSuggestion`, `GeomRect`, and `DetachedMemoryPanelPlacement`.
- Produces: event payload contracts, `memoryPanelReducer`, request IDs, and guarded Tauri adapters.

- [ ] **Step 1: Write lifecycle tests first**

Test the following exact transitions:

```ts
import { describe, expect, it } from 'vitest';
import { initialMemoryPanelState, memoryPanelReducer } from './memoryPanelProtocol';

describe('memoryPanelReducer', () => {
  it('ignores stale close completion after a newer open', () => {
    const preparing = memoryPanelReducer(initialMemoryPanelState, { type: 'begin', requestId: 'r1', ownerCompanionId: 'a' });
    const newer = memoryPanelReducer(preparing, { type: 'begin', requestId: 'r2', ownerCompanionId: 'b' });
    expect(memoryPanelReducer(newer, { type: 'closed', requestId: 'r1' })).toEqual(newer);
  });

  it('accepts blur only after the panel is open', () => {
    const preparing = memoryPanelReducer(initialMemoryPanelState, { type: 'begin', requestId: 'r1', ownerCompanionId: 'a' });
    expect(memoryPanelReducer(preparing, { type: 'request-close', requestId: 'r1', reason: 'blur' })).toEqual(preparing);
    const open = memoryPanelReducer(preparing, { type: 'opened', requestId: 'r1' });
    expect(memoryPanelReducer(open, { type: 'request-close', requestId: 'r1', reason: 'blur' }).phase).toBe('closing');
  });

  it('records Escape as the only close reason that restores badge focus', () => {
    const open = { phase: 'open', requestId: 'r1', ownerCompanionId: 'a', closeReason: null } as const;
    expect(memoryPanelReducer(open, { type: 'request-close', requestId: 'r1', reason: 'escape' }).closeReason).toBe('escape');
  });
});
```

- [ ] **Step 2: Run and verify RED**

```bash
cd ui && bun test src/renderer/pages/companion/memoryPanelProtocol.test.ts
```

Expected: FAIL because the protocol module does not exist.

- [ ] **Step 3: Implement event and reducer contracts**

Export these constants and types:

```ts
export const MEMORY_PANEL_LABEL = 'nomi-memory-panel';
export const MEMORY_PANEL_EVENTS = {
  probe: 'nomi-memory-panel://probe',
  ready: 'nomi-memory-panel://ready',
  snapshot: 'nomi-memory-panel://snapshot',
  measured: 'nomi-memory-panel://measured',
  present: 'nomi-memory-panel://present',
  visible: 'nomi-memory-panel://visible',
  close: 'nomi-memory-panel://close',
  closed: 'nomi-memory-panel://closed',
  activate: 'nomi-memory-panel://activate',
  actionAck: 'nomi-memory-panel://action-ack',
} as const;

export type MemoryPanelPhase = 'closed' | 'preparing' | 'opening' | 'open' | 'closing';
export type MemoryPanelCloseReason = 'blur' | 'escape' | 'toggle' | 'empty' | 'owner-invalid' | 'activation';

export interface MemoryPanelSnapshotPayload {
  requestId: string;
  ownerCompanionId: string;
  ownerWindowLabel: string;
  suggestions: ICompanionSuggestion[];
  theme: 'light' | 'dark';
  customCss: string;
}
```

Also export payloads for probe/ready/measured/present/visible/close/closed/activate/actionAck, `nextMemoryPanelRequestId(ownerCompanionId: string): string`, `initialMemoryPanelState`, and a reducer that ignores every action whose request ID does not match the current request except `begin`.

- [ ] **Step 4: Implement guarded shell wrappers**

`memoryPanelShell.ts` must export:

```ts
export async function prepareMemoryPanelWindow(): Promise<void>;
export async function placeMemoryPanelWindow(args: { requestId: string; ownerCompanionId: string; rect: GeomRect }): Promise<void>;
export async function showMemoryPanelWindow(args: { requestId: string; ownerCompanionId: string }): Promise<void>;
export async function hideMemoryPanelWindow(requestId: string): Promise<boolean>;
export async function emitToMemoryPanel<T>(event: string, payload: T): Promise<void>;
export async function listenCurrentWindow<T>(event: string, handler: (payload: T) => void): Promise<() => void>;
```

Every function checks `isTauriRuntime()`; command wrappers use dynamic `@tauri-apps/api/core` imports and events use dynamic `@tauri-apps/api/event` imports. Browser fallback never throws and never creates a window.

- [ ] **Step 5: Verify protocol tests and typecheck**

```bash
cd ui && bun test src/renderer/pages/companion/memoryPanelProtocol.test.ts && bun run typecheck
```

Expected: tests and TypeScript pass.

- [ ] **Step 6: Commit Task 3**

```bash
git add ui/src/renderer/pages/companion/memoryPanelProtocol.ts ui/src/renderer/pages/companion/memoryPanelProtocol.test.ts ui/src/renderer/pages/companion/memoryPanelShell.ts
git commit -m "feat(companion): add memory panel window protocol"
```

---

### Task 4: Standalone Memory Panel Route and View

**Files:**
- Create: `ui/src/renderer/pages/memoryPanel/index.tsx`
- Create: `ui/src/renderer/pages/memoryPanel/memoryPanel.css`
- Create: `ui/src/renderer/pages/memoryPanel/memoryPanelRoute.test.ts`
- Modify: `ui/src/renderer/components/layout/Router.tsx`

**Interfaces:**
- Consumes: Task 3 events/payloads and shell hide/event adapters.
- Produces: a hidden-measureable, focus-aware memory card route.

- [ ] **Step 1: Write source-contract tests first**

The test reads `Router.tsx`, `index.tsx`, and `memoryPanel.css` and asserts:

```ts
expect(routerSource.includes("path='/nomi-memory-panel'")).toBe(true);
expect(panelSource.includes('onFocusChanged')).toBe(true);
expect(panelSource.includes("phaseRef.current !== 'open'")).toBe(true);
expect(panelSource.includes('MEMORY_PANEL_EVENTS.activate')).toBe(true);
expect(panelSource.includes("role='dialog'")).toBe(true);
expect(panelCss.includes('overflow-y: auto')).toBe(true);
expect(panelCss.includes('overflow-wrap: anywhere')).toBe(true);
expect(panelCss.includes('@media (prefers-reduced-motion: reduce)')).toBe(true);
expect(panelCss.includes('-webkit-line-clamp')).toBe(false);
expect(panelCss.includes('backdrop-filter')).toBe(false);
```

- [ ] **Step 2: Run and verify RED**

```bash
cd ui && bun test src/renderer/pages/memoryPanel/memoryPanelRoute.test.ts
```

Expected: FAIL because the route/view files are missing.

- [ ] **Step 3: Add the standalone route**

In `Router.tsx` add:

```tsx
const MemoryPanelPage = React.lazy(() => import('@renderer/pages/memoryPanel'));

// Beside /companion, outside ProtectedLayout:
<Route path='/nomi-memory-panel' element={withRouteFallback(MemoryPanelPage)} />
```

- [ ] **Step 4: Implement the panel page lifecycle**

The page must:

- start with no snapshot and `phase='closed'`;
- respond to every matching probe with ready;
- render a received snapshot while the native window is hidden;
- measure a fixed 340px logical width and capped 320px content height in `useLayoutEffect`, then emit measured;
- accept present/visible only for the current request;
- subscribe to `getCurrentWindow().onFocusChanged`; blur closes only when `phaseRef.current === 'open'`;
- handle Escape with close reason `escape`;
- emit activate and wait for a matching actionAck before activation close;
- run one idempotent close timer, call `hideMemoryPanelWindow(requestId)`, then emit closed;
- re-check `isFocused()` after visible and close if the window already lost focus;
- unsubscribe every event/window listener and cancel the timer on unmount.

Render this semantic structure using real suggestion content:

```tsx
<main className={`nomi-memory-panel nomi-memory-panel--${phase}`}>
  <section className='nomi-memory-panel__card' role='dialog' aria-label={t('nomi.tabs.suggestions')}>
    <div className='nomi-memory-panel__list'>
      {snapshot.suggestions.map((suggestion) => (
        <button key={suggestion.id} type='button' className='nomi-memory-panel__item' onClick={() => activate(suggestion)}>
          <span className='nomi-memory-panel__title'>{suggestion.title}</span>
          <span className='nomi-memory-panel__body'>{suggestion.body}</span>
        </button>
      ))}
    </div>
  </section>
</main>
```

- [ ] **Step 5: Implement exact-fit visual behavior**

`memoryPanel.css` must set transparent page backgrounds, make the card fill the viewport, use existing theme variables, 14px radius, 1px border, 12px typography, and `overflow-y:auto`. Opening uses opacity only; closing uses 120–150ms opacity plus scale. Reduced motion sets animation/transition duration to 0ms. Do not add outer transparent padding or external CSS shadow that enlarges the native hit area.

- [ ] **Step 6: Verify route tests and typecheck**

```bash
cd ui && bun test src/renderer/pages/memoryPanel/memoryPanelRoute.test.ts && bun run typecheck
```

Expected: all pass.

- [ ] **Step 7: Commit Task 4**

```bash
git add ui/src/renderer/components/layout/Router.tsx ui/src/renderer/pages/memoryPanel/index.tsx ui/src/renderer/pages/memoryPanel/memoryPanel.css ui/src/renderer/pages/memoryPanel/memoryPanelRoute.test.ts
git commit -m "feat(companion): build detached memory panel view"
```

---

### Task 5: Owner Controller and Companion Integration

**Files:**
- Create: `ui/src/renderer/pages/companion/useDetachedMemoryPanel.ts`
- Modify: `ui/src/renderer/pages/companion/index.tsx`
- Modify: `ui/src/renderer/pages/companion/companionChromeLayout.test.ts`

**Interfaces:**
- Consumes: Tasks 1, 3, and 4 plus owner suggestions and `openMainAt`.
- Produces: `{ phase, isExpanded, toggle, close }` for the companion badge.

- [ ] **Step 1: Replace old static assertions with failing detached-controller assertions**

Update `companionChromeLayout.test.ts` to require:

```ts
expect(companionSource.includes('useDetachedMemoryPanel')).toBe(true);
expect(companionSource.includes("showSuggestions ? 'memory'")).toBe(false);
expect(companionSource.includes("type ExpandedWindowMode = 'chat'")).toBe(true);
expect(companionSource.includes("id='nomi-companion-memory-panel'")).toBe(false);
expect(companionSource.includes('aria-expanded={memoryPanel.isExpanded}')).toBe(true);
expect(companionSource.includes('memoryPanel.toggle')).toBe(true);
expect(companionCss.includes('.nomi-companion-suggestions')).toBe(false);
```

Add a source assertion that the owner hook calls `chooseDetachedMemoryPanelLayout` with monitor `bounds` and `workArea`, and never calls `setSize` or `setPosition` on the companion window.

- [ ] **Step 2: Run and verify RED**

```bash
cd ui && bun test src/renderer/pages/companion/companionChromeLayout.test.ts
```

Expected: FAIL on the new detached-controller assertions.

- [ ] **Step 3: Implement `useDetachedMemoryPanel`**

Export:

```ts
export interface DetachedMemoryPanelController {
  phase: MemoryPanelPhase;
  isExpanded: boolean;
  toggle(): void;
  close(reason?: MemoryPanelCloseReason): void;
}

export function useDetachedMemoryPanel(options: {
  companionId: string | null;
  suggestions: ICompanionSuggestion[];
  onActivate: (suggestion: ICompanionSuggestion) => Promise<void>;
  onFallback: () => Promise<void>;
  badgeRef: React.RefObject<HTMLButtonElement | null>;
}): DetachedMemoryPanelController;
```

Implementation requirements:

- prewarm `prepareMemoryPanelWindow()` once when suggestions first become non-empty;
- register owner listeners once and filter every payload by request ID;
- on toggle-open, begin a new request, capture the current window label/physical anchor/available monitors, and probe every 60ms for at most 30 attempts;
- on ready, send the complete snapshot with current theme/custom CSS;
- on measured, run detached geometry; fallback closes preparation and calls `onFallback`;
- on placed result, call Rust place, send present, call Rust show, send visible, and mark opened;
- on activate, resolve the suggestion from the latest authoritative ref, await `onActivate`, then emit actionAck;
- on closed, reset state; restore badge focus only when close reason is Escape;
- when suggestions become empty, owner is invalid, or the companion starts moving, request close;
- cancel probe timers and unlisten on unmount;
- never call `setSize` or `setPosition` on the companion window.

- [ ] **Step 4: Integrate the hook into `CompanionPage`**

Keep `suggestions` and `unread` state. Replace `showSuggestions` with:

```ts
const memoryPanel = useDetachedMemoryPanel({
  companionId,
  suggestions,
  onActivate: async (suggestion) => openMainAt(suggestion.action?.to || '/nomi?tab=suggestions'),
  onFallback: async () => openMainAt('/nomi?tab=suggestions'),
  badgeRef: unreadBadgeRef,
});
```

The badge uses `aria-expanded={memoryPanel.isExpanded}` and `onClick={memoryPanel.toggle}`. Remove cross-document `aria-controls`. `clearUnreadSuggestions`, item decisions, hide/delete paths, and drag start call `memoryPanel.close(...)` when needed.

- [ ] **Step 5: Remove memory from the shared expanded-window path**

Change `ExpandedWindowMode` to `'chat'`; delete the memory branch in `syncExpandedWindow`; calculate `expandedMode` only from reply/composer state; remove memory placement/max size/stage shift React state. Chat expansion, desk restoration, and persistence suppression remain unchanged.

- [ ] **Step 6: Verify companion tests and typecheck**

```bash
cd ui && bun test src/renderer/pages/companion && bun run typecheck
```

Expected: all companion tests and typecheck pass.

- [ ] **Step 7: Commit Task 5**

```bash
git add ui/src/renderer/pages/companion/useDetachedMemoryPanel.ts ui/src/renderer/pages/companion/index.tsx ui/src/renderer/pages/companion/companionChromeLayout.test.ts
git commit -m "feat(companion): connect detached memory panel"
```

---

### Task 6: Remove the Superseded Shared Memory Surface

**Files:**
- Modify: `ui/src/renderer/pages/companion/companion.css`
- Modify: `ui/src/renderer/pages/companion/companionCapturePolicy.ts`
- Modify: `ui/src/renderer/pages/companion/companionCapturePolicy.test.ts`
- Modify: `ui/src/renderer/pages/companion/memoryPanelGeometry.ts`
- Modify: `ui/src/renderer/pages/companion/memoryPanelGeometry.test.ts`

**Interfaces:**
- Consumes: completed detached panel integration.
- Produces: one memory-panel implementation path and unchanged chat restore helpers.

- [ ] **Step 1: Add failing cleanup assertions**

Update tests to require that:

```ts
expect(companionCss.includes('is-memory-panel-open')).toBe(false);
expect(companionCss.includes('nomi-companion-stage-shell--above')).toBe(false);
expect(companionCss.includes('nomi-companion-suggestions')).toBe(false);
expect(capturePolicySource.includes('showSuggestions')).toBe(false);
expect(memoryGeometrySource.includes('chooseMemoryPanelLayout')).toBe(false);
expect(memoryGeometrySource.includes('fitMemoryPanelInAchievedWindow')).toBe(false);
expect(memoryGeometrySource.includes('memoryPanelStageShiftX')).toBe(false);
expect(memoryGeometrySource.includes('resolveDeskRestoreLayout')).toBe(true);
```

- [ ] **Step 2: Run and verify RED**

```bash
cd ui && bun test src/renderer/pages/companion/companionChromeLayout.test.ts src/renderer/pages/companion/companionCapturePolicy.test.ts src/renderer/pages/companion/memoryPanelGeometry.test.ts
```

Expected: FAIL because obsolete shared memory code still exists.

- [ ] **Step 3: Delete obsolete CSS and geometry only**

Remove the memory reading-mode root styles, directional stage-shell variants, stage translation, and `.nomi-companion-suggestions*` rules from `companion.css`. Preserve badge, stage, reply bubble, chatbar, composer, character, and loading styles.

Remove `MemoryPanelPlacement`, `MemoryPanelLayoutInput`, `MemoryPanelLayout`, `AchievedMemoryPanelInput`, `chooseMemoryPanelLayout`, `fitMemoryPanelInAchievedWindow`, and `memoryPanelStageShiftX` from `memoryPanelGeometry.ts`. Preserve `MonitorLayout`, `pickHostMonitor`, and `resolveDeskRestoreLayout` because chat expansion still uses them.

Remove `showSuggestions` from `CompanionCapturePolicyState` and test fixtures; the function continues to return only `state.dragOver`.

- [ ] **Step 4: Verify cleanup and all companion tests**

```bash
cd ui && bun test src/renderer/pages/companion src/renderer/pages/memoryPanel && bun run typecheck
```

Expected: all tests and typecheck pass.

- [ ] **Step 5: Commit Task 6**

```bash
git add ui/src/renderer/pages/companion/companion.css ui/src/renderer/pages/companion/companionCapturePolicy.ts ui/src/renderer/pages/companion/companionCapturePolicy.test.ts ui/src/renderer/pages/companion/memoryPanelGeometry.ts ui/src/renderer/pages/companion/memoryPanelGeometry.test.ts
git commit -m "refactor(companion): remove shared memory expansion"
```

---

### Task 7: Race, Lifecycle, and Release-Gate Regression Pass

**Files:**
- Modify: `ui/src/renderer/pages/companion/detachedMemoryPanelGeometry.test.ts`
- Modify: `ui/src/renderer/pages/companion/memoryPanelProtocol.test.ts`
- Modify: `ui/src/renderer/pages/memoryPanel/memoryPanelRoute.test.ts`
- Modify: `apps/desktop/src/memory_panel_window.rs`
- Modify: `docs/superpowers/specs/2026-07-11-companion-memory-popover-window-design.md`

**Interfaces:**
- Consumes: the complete feature.
- Produces: release evidence and implementation-aligned specification.

- [ ] **Step 1: Add the final regression cases before any hardening code**

Add tests for:

- left/right/top/bottom work-area insets;
- anchor partly outside workArea but inside bounds;
- anchor on a negative-coordinate mixed-DPI monitor;
- close during opening ignored until open;
- duplicate blur produces one closing transition;
- close-r1 after begin-r2 ignored;
- activation ack from stale request ignored;
- Rust stale place/show/hide rejected;
- Rust invalid owner invalidation clears state;
- exact capability scope contains `nomi-memory-panel` and no create permission;
- route has one standalone memory panel and companion DOM has none.

- [ ] **Step 2: Run new tests and verify each new case is meaningful**

Run focused frontend and Rust commands. If a new test passes immediately, inspect whether it covers existing behavior; adjust it until it fails for the missing hardening behavior before changing production code.

```bash
cd ui && bun test src/renderer/pages/companion/detachedMemoryPanelGeometry.test.ts src/renderer/pages/companion/memoryPanelProtocol.test.ts src/renderer/pages/memoryPanel/memoryPanelRoute.test.ts
cargo test -p nomifun-desktop memory_panel_window::tests -- --nocapture
```

- [ ] **Step 3: Implement only the hardening required by RED tests**

Keep fixes inside the geometry candidate filter, request reducer, listener cleanup, Rust state validation, or exact capability scope. Do not introduce another timer, another panel window, or a fallback shared DOM panel.

- [ ] **Step 4: Align the design spec with actual command signatures**

Update only the command signature wording in the design spec if implementation uses a no-argument prepare command for prewarming. Preserve the fixed-label, request validation, and restricted-permission requirements.

- [ ] **Step 5: Run full fresh verification**

```bash
cd ui && bun test src/renderer/pages/companion src/renderer/pages/memoryPanel
cd ui && bun run typecheck
cd ui && bun run build
cargo fmt --check
cargo test -p nomifun-desktop
git diff --check
git status --short
```

Expected:

- frontend tests: zero failures;
- TypeScript: exit 0;
- Vite production build: exit 0 (existing unrelated chunk-size warnings may remain, but no new errors);
- Rust desktop tests: zero failures;
- formatting and diff checks: clean;
- status lists only intentional implementation/spec/plan changes not yet committed by this task.

- [ ] **Step 6: Perform required desktop visual checks**

Run the desktop app and verify the user screenshot scenario plus center/top/left/right edges. Capture the same state before and after the change. Confirm:

- partner bounds are visually unchanged and complete;
- panel is outside the partner and inside the work area;
- outside click reaches its target and closes the panel;
- 20 open/close cycles do not move the partner;
- theme, scrolling, Escape, suggestion activation, and reduced motion behave as specified.

If the current environment cannot run an interactive Tauri desktop session, state that limitation explicitly; do not claim the real-OS visual matrix passed from unit tests alone.

- [ ] **Step 7: Commit Task 7**

```bash
git add ui/src/renderer/pages/companion/detachedMemoryPanelGeometry.test.ts ui/src/renderer/pages/companion/memoryPanelProtocol.test.ts ui/src/renderer/pages/memoryPanel/memoryPanelRoute.test.ts apps/desktop/src/memory_panel_window.rs docs/superpowers/specs/2026-07-11-companion-memory-popover-window-design.md
git commit -m "test(companion): harden detached memory panel"
```

---

### Task 8: Compact Width and Two-Line Body Preview

**Files:**
- Modify: `apps/desktop/src/memory_panel_window.rs`
- Modify: `ui/src/renderer/pages/memoryPanel/memoryPanel.css`
- Modify: `ui/src/renderer/pages/memoryPanel/memoryPanelRoute.test.ts`
- Modify: `docs/superpowers/specs/2026-07-11-companion-memory-popover-window-design.md`

**Interfaces:**
- Consumes: the existing hidden-measurement and detached placement flow.
- Produces: a 300px default logical width and a two-line body preview without changing title wrapping or lifecycle behavior.

- [x] **Step 1: Write failing source-contract tests**

Assert that the Rust window builder uses `.inner_size(300.0, 320.0)` and the panel body CSS contains `-webkit-line-clamp: 2`, vertical box orientation, and hidden overflow. Remove the obsolete assertion that line clamping is forbidden.

- [x] **Step 2: Run the focused route test and verify RED**

```bash
cd ui && bun test src/renderer/pages/memoryPanel/memoryPanelRoute.test.ts
```

Expected: FAIL because the native width is still 340px and the body is not clamped.

- [x] **Step 3: Implement the approved visual contract**

Change the hidden native window's default logical width from 340 to 300. Apply the two-line WebKit box clamp only to `.nomi-memory-panel__body`; leave `.nomi-memory-panel__title` normally wrapped and keep list-level vertical scrolling.

- [x] **Step 4: Align the design specification**

Document the 300px default width, full title wrapping, two-line body preview, and existing list scrolling. Remove statements promising unlimited body text in the desktop preview.

- [x] **Step 5: Run fresh verification**

```bash
cd ui && bun test src/renderer/pages/companion src/renderer/pages/memoryPanel
cd ui && bun run typecheck
cd ui && bun run build
cargo fmt --all --check
cargo test -p nomifun-desktop
git diff --check
```

Expected: all tests and builds pass; existing unrelated build warnings may remain.

---

## Plan Self-Review

- **Spec coverage:** Tasks cover the single fixed window, restricted Rust permissions, request-scoped protocol, hidden measurement, physical-pixel placement, workArea versus bounds, blur/Escape/activation semantics, multi-owner races, cleanup, accessibility, scrolling, fallback, and release verification.
- **No placeholders:** The plan contains no deferred implementation choices. Every new module has exact exported interfaces and focused RED/GREEN commands.
- **Type consistency:** `requestId`, `ownerCompanionId`, `ownerWindowLabel`, `DetachedMemoryPanelPlacement`, `GeomRect`, and all event names are consistent across Tasks 1–8.
- **Scope:** Reply bubbles and composer expansion remain unchanged; only memory-panel ownership moves to the independent window.
- **Commit strategy:** Each task has one independently reviewable commit; no implementation commit mixes unrelated product work.

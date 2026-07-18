# Companion Multi-Monitor Pointer Routing Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Replace global desktop-coordinate subtraction with native window-local normalized pointer sampling so desktop companions remain clickable and draggable on every supported monitor and fail to capture mode on unsupported or broken paths.

**Architecture:** One label-scoped Rust command returns a normalized local point on AppKit, Win32, and X11, or an explicit unsupported result on native Wayland. A typed renderer adapter converts the normalized point into DOM client coordinates, while a testable controller owns capture/passthrough transitions, recovery cadence, hover fallback, and teardown safety.

**Tech Stack:** Rust 2021, Tauri 2.11, objc2/AppKit 0.3, windows-rs 0.61, GTK 3/gtk-rs 0.18, React 19, TypeScript 5.8, Bun test.

## Global Constraints

- Follow `docs/superpowers/specs/2026-07-18-companion-multimonitor-pointer-routing-design.md` exactly.
- Never subtract global cursor and window positions in renderer code and never use `window.devicePixelRatio` for native-to-DOM pointer conversion.
- The command name is exactly `get_companion_local_pointer`; accepted window labels are non-empty `companion-<id>` labels only.
- JSON point responses are `{ kind: 'point', backend: 'appkit' | 'win32' | 'x11', xRatio, yRatio }`; unsupported responses are `{ kind: 'unsupported', backend: 'wayland' | 'other' }`.
- Ratios outside `[0, 1]` remain unchanged; invalid or non-finite geometry is an error.
- Every uncertain, unsupported, failed, stale, or teardown state resolves to `setIgnoreCursorEvents(false)`.
- Native Wayland uses whole-window capture and DOM hover fallback; it must never consume Tao's synthetic `(0, 0)` cursor position.
- Preserve the existing 40 ms normal polling interval and use 1000 ms only for failure recovery.
- Preserve existing companion DOM hit targets, avatar alpha-mask semantics, drag behavior, and layout.
- Minimize Rust test executions: one focused RED, one focused GREEN, then `cargo check`; do not run the workspace-wide `cargo test` command.
- Per user instruction, do not run `git add`, `git commit`, or otherwise commit implementation or plan changes. Leave all implementation changes unstaged and uncommitted.

---

### Task 1: Native normalized pointer command

**Files:**
- Create: `apps/desktop/src/companion_pointer.rs`
- Modify: `apps/desktop/Cargo.toml`
- Modify: `apps/desktop/src/main.rs:25-30`
- Modify: `apps/desktop/src/main.rs:547-553`
- Modify: `apps/desktop/src/main.rs:836-854`
- Test: inline `apps/desktop/src/companion_pointer.rs` unit tests

**Interfaces:**
- Consumes: invoking `tauri::WebviewWindow`, AppKit `NSView`, Win32 HWND, or GTK `ApplicationWindow`.
- Produces: `get_companion_local_pointer(window) -> Result<CompanionLocalPointer, String>` and `supports_initial_pointer_passthrough(window) -> bool`.

- [ ] **Step 1: Add the failing pure contract tests and command wiring**

Create the module declaration and handler entry:

```rust
mod companion_pointer;
```

```rust
.invoke_handler(tauri::generate_handler![
    check_for_updates,
    companion_pointer::get_companion_local_pointer,
    updater_install_context::get_updater_install_context,
])
```

Start `companion_pointer.rs` with tests that define the required contract before implementation:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn companion_labels_require_a_non_empty_id() {
        assert!(is_companion_label("companion-alice"));
        assert!(!is_companion_label("companion-"));
        assert!(!is_companion_label("main"));
    }

    #[test]
    fn normalization_preserves_outside_window_coordinates() {
        assert_eq!(normalize_local_point("x11", -20.0, 240.0, 200.0, 120.0).unwrap(),
            CompanionLocalPointer::Point { backend: "x11", x_ratio: -0.1, y_ratio: 2.0 });
    }

    #[test]
    fn normalization_rejects_invalid_geometry() {
        for input in [
            (f64::NAN, 1.0, 10.0, 10.0),
            (1.0, f64::INFINITY, 10.0, 10.0),
            (1.0, 1.0, 0.0, 10.0),
            (1.0, 1.0, 10.0, -1.0),
        ] {
            assert!(normalize_local_point("appkit", input.0, input.1, input.2, input.3).is_err());
        }
    }

    #[test]
    fn appkit_y_conversion_handles_flipped_and_unflipped_views() {
        assert_eq!(top_left_view_y(30.0, 10.0, 100.0, true), 20.0);
        assert_eq!(top_left_view_y(30.0, 10.0, 100.0, false), 80.0);
    }

    #[test]
    fn only_recoverable_backends_start_in_passthrough() {
        assert!(backend_supports_reentry(PointerBackend::AppKit));
        assert!(backend_supports_reentry(PointerBackend::Win32));
        assert!(backend_supports_reentry(PointerBackend::X11));
        assert!(!backend_supports_reentry(PointerBackend::Wayland));
        assert!(!backend_supports_reentry(PointerBackend::Other));
    }
}
```

- [ ] **Step 2: Run the one focused Rust RED**

Run:

```bash
cargo test -p nomifun-desktop --bin nomifun-desktop companion_pointer
```

Expected: FAIL because `CompanionLocalPointer`, `PointerBackend`, and the pure helpers are not implemented.

- [ ] **Step 3: Implement the shared contract and platform samplers**

Implement these exact shared types and guards:

```rust
use serde::Serialize;
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case", rename_all_fields = "camelCase")]
pub enum CompanionLocalPointer {
    Point { backend: &'static str, x_ratio: f64, y_ratio: f64 },
    Unsupported { backend: &'static str },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PointerBackend { AppKit, Win32, X11, Wayland, Other }

fn is_companion_label(label: &str) -> bool {
    label.strip_prefix("companion-").is_some_and(|id| !id.is_empty())
}

fn backend_supports_reentry(backend: PointerBackend) -> bool {
    matches!(backend, PointerBackend::AppKit | PointerBackend::Win32 | PointerBackend::X11)
}

fn normalize_local_point(
    backend: &'static str,
    x: f64,
    y: f64,
    width: f64,
    height: f64,
) -> Result<CompanionLocalPointer, String> {
    if !x.is_finite() || !y.is_finite() || !width.is_finite() || !height.is_finite()
        || width <= 0.0 || height <= 0.0 {
        return Err("invalid companion pointer geometry".to_string());
    }
    Ok(CompanionLocalPointer::Point {
        backend,
        x_ratio: x / width,
        y_ratio: y / height,
    })
}

fn top_left_view_y(view_y: f64, bounds_y: f64, height: f64, flipped: bool) -> f64 {
    if flipped { view_y - bounds_y } else { bounds_y + height - view_y }
}
```

Use one `sample_native` implementation per target and a shared label-scoped command:

```rust
#[tauri::command]
pub fn get_companion_local_pointer(
    window: tauri::WebviewWindow,
) -> Result<CompanionLocalPointer, String> {
    if !is_companion_label(window.label()) {
        return Err("local pointer sampling is restricted to companion windows".to_string());
    }
    sample_native(&window)
}

#[cfg(target_os = "macos")]
fn sample_native(window: &tauri::WebviewWindow) -> Result<CompanionLocalPointer, String> {
    use objc2_app_kit::NSView;

    let view_ptr = window.ns_view().map_err(|error| error.to_string())?;
    let view = unsafe { view_ptr.cast::<NSView>().as_ref() }
        .ok_or_else(|| "companion NSView pointer is null".to_string())?;
    let native_window = view
        .window()
        .ok_or_else(|| "companion NSView has no NSWindow".to_string())?;
    let window_point = native_window.mouseLocationOutsideOfEventStream();
    let view_point = view.convertPoint_fromView(window_point, None);
    let bounds = view.bounds();
    normalize_local_point(
        "appkit",
        view_point.x - bounds.origin.x,
        top_left_view_y(view_point.y, bounds.origin.y, bounds.size.height, view.isFlipped()),
        bounds.size.width,
        bounds.size.height,
    )
}

#[cfg(windows)]
fn sample_native(window: &tauri::WebviewWindow) -> Result<CompanionLocalPointer, String> {
    use windows::Win32::Foundation::{POINT, RECT};
    use windows::Win32::UI::WindowsAndMessaging::{GetClientRect, GetCursorPos, ScreenToClient};

    let hwnd = window.hwnd().map_err(|error| error.to_string())?;
    let mut point = POINT::default();
    let mut rect = RECT::default();
    unsafe {
        GetCursorPos(&mut point).map_err(|error| error.to_string())?;
        ScreenToClient(hwnd, &mut point).map_err(|error| error.to_string())?;
        GetClientRect(hwnd, &mut rect).map_err(|error| error.to_string())?;
    }
    normalize_local_point(
        "win32",
        f64::from(point.x),
        f64::from(point.y),
        f64::from(rect.right - rect.left),
        f64::from(rect.bottom - rect.top),
    )
}

#[cfg(target_os = "linux")]
fn linux_backend(window: &tauri::WebviewWindow) -> Result<PointerBackend, String> {
    use gtk::{gdk::prelude::*, prelude::*};

    let display = window.gtk_window().map_err(|error| error.to_string())?.display();
    let backend = display.backend();
    Ok(if backend.is_wayland() {
        PointerBackend::Wayland
    } else if backend.is_x11() {
        PointerBackend::X11
    } else {
        PointerBackend::Other
    })
}

#[cfg(target_os = "linux")]
fn sample_native(window: &tauri::WebviewWindow) -> Result<CompanionLocalPointer, String> {
    use gtk::{gdk::prelude::*, prelude::*};

    match linux_backend(window)? {
        PointerBackend::Wayland => return Ok(CompanionLocalPointer::Unsupported { backend: "wayland" }),
        PointerBackend::Other => return Ok(CompanionLocalPointer::Unsupported { backend: "other" }),
        PointerBackend::X11 => {}
        PointerBackend::AppKit | PointerBackend::Win32 => unreachable!(),
    }
    let top = window.gtk_window().map_err(|error| error.to_string())?;
    let display = top.display();
    let surface = top.window().ok_or_else(|| "companion GTK window is not realized".to_string())?;
    let pointer = display.default_seat().and_then(|seat| seat.pointer())
        .ok_or_else(|| "Linux display has no pointer device".to_string())?;
    let (_, x, y, _) = surface.device_position_double(&pointer);
    normalize_local_point("x11", x, y, f64::from(surface.width()), f64::from(surface.height()))
}
```

Use these target dependencies:

```toml
[target.'cfg(target_os = "macos")'.dependencies]
objc2 = "0.6"
objc2-app-kit = { version = "0.3", default-features = false, features = ["std", "NSResponder", "NSView", "NSWindow"] }

[target.'cfg(windows)'.dependencies]
windows = { version = "0.61", features = ["Win32_Foundation", "Win32_UI_WindowsAndMessaging"] }

[target.'cfg(target_os = "linux")'.dependencies]
gtk = { version = "0.18", features = ["v3_24"] }
```

Implement startup capability without caching native handles:

```rust
pub fn supports_initial_pointer_passthrough(window: &tauri::WebviewWindow) -> bool {
    #[cfg(target_os = "macos")]
    let backend = Ok(PointerBackend::AppKit);
    #[cfg(windows)]
    let backend = Ok(PointerBackend::Win32);
    #[cfg(target_os = "linux")]
    let backend = linux_backend(window);
    #[cfg(not(any(target_os = "macos", windows, target_os = "linux")))]
    let backend: Result<PointerBackend, String> = Ok(PointerBackend::Other);
    backend.is_ok_and(backend_supports_reentry)
}
```

Replace the unconditional create-time `set_ignore_cursor_events(true)` with a guarded call:

```rust
if companion_pointer::supports_initial_pointer_passthrough(&window) {
    if let Err(error) = window.set_ignore_cursor_events(true) {
        tracing::warn!(error = %error, label = %label, "failed to set startup companion click-through");
    }
}
```

- [ ] **Step 4: Run the one focused Rust GREEN and format only affected Rust**

Run:

```bash
cargo fmt -p nomifun-desktop
cargo test -p nomifun-desktop --bin nomifun-desktop companion_pointer
```

Expected: focused companion pointer tests PASS. Do not run another Rust test command during implementation.

- [ ] **Step 5: Run the cheaper compile checkpoint**

Run:

```bash
cargo check -p nomifun-desktop
```

Expected: current-host desktop compilation exits 0. Keep Windows and Linux runtime claims limited to source/CI coverage on this macOS host.

---

### Task 2: Typed renderer adapter and normalized geometry

**Files:**
- Create: `ui/src/renderer/pages/companion/companionLocalPointer.ts`
- Create: `ui/src/renderer/pages/companion/companionLocalPointer.test.ts`

**Interfaces:**
- Consumes: JSON returned by `get_companion_local_pointer` and a DOM viewport size.
- Produces: `getCompanionLocalPointer()`, `parseCompanionLocalPointer(value)`, and `toCompanionClientPoint(sample, viewport)`.

- [ ] **Step 1: Write the failing Bun tests**

```ts
import { describe, expect, test } from 'bun:test';
import { parseCompanionLocalPointer, toCompanionClientPoint } from './companionLocalPointer';

describe('companion local pointer', () => {
  test('maps normalized native coordinates through the current DOM viewport', () => {
    const sample = parseCompanionLocalPointer({ kind: 'point', backend: 'appkit', xRatio: 0.25, yRatio: 0.5 });
    expect(toCompanionClientPoint(sample, { width: 192, height: 171.2 })).toEqual({ x: 48, y: 85.6 });
    expect(toCompanionClientPoint(sample, { width: 240, height: 214 })).toEqual({ x: 60, y: 107 });
    expect(toCompanionClientPoint(sample, { width: 312, height: 278.2 })).toEqual({ x: 78, y: 139.1 });
  });

  test('preserves points outside the native window', () => {
    const sample = parseCompanionLocalPointer({ kind: 'point', backend: 'x11', xRatio: -0.1, yRatio: 1.25 });
    expect(toCompanionClientPoint(sample, { width: 200, height: 100 })).toEqual({ x: -20, y: 125 });
  });

  test('accepts explicit Wayland fallback and rejects malformed values', () => {
    expect(parseCompanionLocalPointer({ kind: 'unsupported', backend: 'wayland' })).toEqual({ kind: 'unsupported', backend: 'wayland' });
    for (const value of [null, {}, { kind: 'point', backend: 'appkit', xRatio: Number.NaN, yRatio: 0 }, { kind: 'unsupported', backend: 'x11' }]) {
      expect(() => parseCompanionLocalPointer(value)).toThrow();
    }
  });

  test('rejects invalid viewport geometry', () => {
    const sample = parseCompanionLocalPointer({ kind: 'point', backend: 'win32', xRatio: 0.5, yRatio: 0.5 });
    expect(toCompanionClientPoint(sample, { width: 0, height: 100 })).toBeNull();
    expect(toCompanionClientPoint(sample, { width: 100, height: Number.POSITIVE_INFINITY })).toBeNull();
  });
});
```

- [ ] **Step 2: Run the focused frontend RED**

Run:

```bash
bun test ui/src/renderer/pages/companion/companionLocalPointer.test.ts
```

Expected: FAIL because `companionLocalPointer.ts` does not exist.

- [ ] **Step 3: Implement the adapter and pure conversion**

```ts
export type CompanionPointerBackend = 'appkit' | 'win32' | 'x11';
export type CompanionUnsupportedBackend = 'wayland' | 'other';
export type CompanionLocalPointerSample =
  | { kind: 'point'; backend: CompanionPointerBackend; xRatio: number; yRatio: number }
  | { kind: 'unsupported'; backend: CompanionUnsupportedBackend };

const pointBackends = new Set<CompanionPointerBackend>(['appkit', 'win32', 'x11']);
const unsupportedBackends = new Set<CompanionUnsupportedBackend>(['wayland', 'other']);

export function parseCompanionLocalPointer(value: unknown): CompanionLocalPointerSample {
  if (!value || typeof value !== 'object') throw new Error('invalid companion pointer response');
  const data = value as Record<string, unknown>;
  if (data.kind === 'point' && pointBackends.has(data.backend as CompanionPointerBackend)
      && typeof data.xRatio === 'number' && Number.isFinite(data.xRatio)
      && typeof data.yRatio === 'number' && Number.isFinite(data.yRatio)) {
    return { kind: 'point', backend: data.backend as CompanionPointerBackend, xRatio: data.xRatio, yRatio: data.yRatio };
  }
  if (data.kind === 'unsupported' && unsupportedBackends.has(data.backend as CompanionUnsupportedBackend)) {
    return { kind: 'unsupported', backend: data.backend as CompanionUnsupportedBackend };
  }
  throw new Error('invalid companion pointer response');
}

export function toCompanionClientPoint(
  sample: CompanionLocalPointerSample,
  viewport: { width: number; height: number }
): { x: number; y: number } | null {
  if (sample.kind !== 'point' || !Number.isFinite(viewport.width) || !Number.isFinite(viewport.height)
      || viewport.width <= 0 || viewport.height <= 0) return null;
  return { x: sample.xRatio * viewport.width, y: sample.yRatio * viewport.height };
}

export async function getCompanionLocalPointer(): Promise<CompanionLocalPointerSample> {
  const { invoke } = await import('@tauri-apps/api/core');
  return parseCompanionLocalPointer(await invoke<unknown>('get_companion_local_pointer'));
}
```

- [ ] **Step 4: Run the focused frontend GREEN**

Run:

```bash
bun test ui/src/renderer/pages/companion/companionLocalPointer.test.ts
```

Expected: all adapter/geometry tests PASS.

---

### Task 3: Fail-operable click-through controller

**Files:**
- Create: `ui/src/renderer/pages/companion/companionClickThroughController.ts`
- Create: `ui/src/renderer/pages/companion/companionClickThroughController.test.ts`

**Interfaces:**
- Consumes: `CompanionLocalPointerSample`, `toCompanionClientPoint`, injected native set-ignore/sample functions, viewport, and DOM hit test.
- Produces: `CompanionClickThroughController` with `initialize`, `tick`, `handleFallbackPointerMove`, `handleFallbackPointerLeave`, `dispose`, and `mode`.

- [ ] **Step 1: Write controller tests before implementation**

Use this real dependency helper around the controller:

```ts
import { describe, expect, test } from 'bun:test';
import { CompanionClickThroughController } from './companionClickThroughController';
import type { CompanionLocalPointerSample } from './companionLocalPointer';

const point = (xRatio: number, yRatio: number): CompanionLocalPointerSample => ({
  kind: 'point', backend: 'appkit', xRatio, yRatio,
});

const deferred = <T>() => {
  let resolve!: (value: T) => void;
  const promise = new Promise<T>((done) => { resolve = done; });
  return { promise, resolve };
};

function makeController(opts: {
  samples?: CompanionLocalPointerSample[];
  sampleError?: Error;
  samplePromise?: Promise<CompanionLocalPointerSample>;
  ignored: boolean[];
  hover?: boolean[];
  hitTest?: (x: number, y: number) => boolean;
}) {
  const queue = [...(opts.samples ?? [])];
  return new CompanionClickThroughController({
    sample: async () => {
      if (opts.samplePromise) return opts.samplePromise;
      if (opts.sampleError) throw opts.sampleError;
      const sample = queue.shift();
      if (!sample) throw new Error('test sample queue exhausted');
      return sample;
    },
    setIgnore: async (ignore) => { opts.ignored.push(ignore); },
    viewport: () => ({ width: 100, height: 100 }),
    hitTest: opts.hitTest ?? (() => false),
    onHoverChange: (over) => opts.hover?.push(over),
  });
}
```

Assert these exact behaviors:

```ts
test('captures before sampling and flips only after a valid hit decision', async () => {
  const ignored: boolean[] = [];
  const samples = [point(0.5, 0.5), point(1.5, 1.5)];
  const controller = makeController({ samples, ignored, hitTest: (x, y) => x === 50 && y === 50 });
  await controller.initialize();
  await controller.tick({ captureAll: false, dragging: false });
  await controller.tick({ captureAll: false, dragging: false });
  expect(ignored).toEqual([false, true]);
});

test('unsupported Wayland stays captured and uses DOM hover fallback', async () => {
  const ignored: boolean[] = [];
  const hover: boolean[] = [];
  const controller = makeController({ samples: [{ kind: 'unsupported', backend: 'wayland' }], ignored, hover, hitTest: (x) => x === 20 });
  await controller.initialize();
  expect(await controller.tick({ captureAll: false, dragging: false })).toBe('unsupported');
  controller.handleFallbackPointerMove(20, 1);
  controller.handleFallbackPointerLeave();
  expect(ignored).toEqual([false]);
  expect(hover).toEqual([true, false]);
});

test('sampling failure captures and enters recovery mode', async () => {
  const ignored: boolean[] = [];
  const controller = makeController({ sampleError: new Error('old shell'), ignored });
  await controller.initialize();
  expect(await controller.tick({ captureAll: false, dragging: false })).toBe('recover');
  expect(ignored).toEqual([false]);
});

test('dispose during an in-flight sample prevents a stale ignore=true write', async () => {
  const pending = deferred<CompanionLocalPointerSample>();
  const ignored: boolean[] = [];
  const controller = makeController({ samplePromise: pending.promise, ignored, hitTest: () => false });
  await controller.initialize();
  const tick = controller.tick({ captureAll: false, dragging: false });
  await controller.dispose();
  pending.resolve(point(2, 2));
  await tick;
  expect(ignored).toEqual([false, false]);
});
```

- [ ] **Step 2: Run the controller RED**

Run:

```bash
bun test ui/src/renderer/pages/companion/companionClickThroughController.test.ts
```

Expected: FAIL because the controller module does not exist.

- [ ] **Step 3: Implement the controller state machine**

Implement the exact public dependencies and state surface:

```ts
import { toCompanionClientPoint, type CompanionLocalPointerSample } from './companionLocalPointer';

export type CompanionClickThroughMode = 'poll' | 'recover' | 'unsupported';

export interface CompanionClickThroughControllerDeps {
  sample: () => Promise<CompanionLocalPointerSample>;
  setIgnore: (ignore: boolean) => Promise<void>;
  viewport: () => { width: number; height: number };
  hitTest: (clientX: number, clientY: number) => boolean;
  onHoverChange?: (over: boolean) => void;
  onError?: (error: unknown) => void;
}

export class CompanionClickThroughController {
  private disposed = false;
  private lastIgnore: boolean | null = null;
  private lastOver: boolean | null = null;
  mode: CompanionClickThroughMode = 'poll';

  constructor(private readonly deps: CompanionClickThroughControllerDeps) {}

  async initialize(): Promise<void> {
    try {
      await this.deps.setIgnore(false);
      this.lastIgnore = false;
    } catch (error) {
      this.mode = 'recover';
      this.deps.onError?.(error);
    }
  }

  async tick(state: { captureAll: boolean; dragging: boolean }): Promise<CompanionClickThroughMode> {
    if (this.disposed) return this.mode;
    if (state.captureAll || state.dragging) {
      await this.applyIgnore(false);
      return this.mode;
    }
    try {
      const sample = await this.deps.sample();
      if (this.disposed) return this.mode;
      if (sample.kind === 'unsupported') {
        this.mode = 'unsupported';
        await this.applyIgnore(false);
        return this.mode;
      }
      const client = toCompanionClientPoint(sample, this.deps.viewport());
      if (!client) throw new Error('invalid companion pointer viewport');
      const over = this.deps.hitTest(client.x, client.y);
      this.reportHover(over);
      await this.applyIgnore(!over);
      this.mode = 'poll';
    } catch (error) {
      if (!this.disposed) {
        this.mode = 'recover';
        this.deps.onError?.(error);
        try {
          await this.applyIgnore(false);
        } catch (captureError) {
          this.deps.onError?.(captureError);
        }
      }
    }
    return this.mode;
  }

  handleFallbackPointerMove(clientX: number, clientY: number): void {
    if (!this.disposed && this.mode !== 'poll') this.reportHover(this.deps.hitTest(clientX, clientY));
  }

  handleFallbackPointerLeave(): void {
    if (!this.disposed && this.mode !== 'poll') this.reportHover(false);
  }

  async dispose(): Promise<void> {
    if (this.disposed) return;
    this.disposed = true;
    this.reportHover(false);
    try {
      await this.deps.setIgnore(false);
      this.lastIgnore = false;
    } catch (error) {
      this.deps.onError?.(error);
    }
  }

  private async applyIgnore(ignore: boolean): Promise<void> {
    if (this.disposed || this.lastIgnore === ignore) return;
    await this.deps.setIgnore(ignore);
    if (!this.disposed) this.lastIgnore = ignore;
  }

  private reportHover(over: boolean): void {
    if (this.lastOver === over) return;
    this.lastOver = over;
    this.deps.onHoverChange?.(over);
  }
}
```

`applyIgnore` updates `lastIgnore` only after the native promise resolves. Fallback pointer methods act only while mode is `recover` or `unsupported`. `dispose()` sets `disposed=true` before any await, reports hover false when necessary, and directly calls the injected `setIgnore(false)` without consulting `lastIgnore`.

- [ ] **Step 4: Run controller and geometry tests together**

Run:

```bash
bun test ui/src/renderer/pages/companion/companionClickThroughController.test.ts ui/src/renderer/pages/companion/companionLocalPointer.test.ts
```

Expected: both files PASS.

---

### Task 4: Replace the hook's global-coordinate polling

**Files:**
- Modify: `ui/src/renderer/pages/companion/useCompanionClickThrough.ts`
- Create: `ui/src/renderer/pages/companion/useCompanionClickThrough.test.ts`
- Test: `ui/src/renderer/pages/companion/companionHitTarget.test.ts`

**Interfaces:**
- Consumes: `CompanionClickThroughController`, `getCompanionLocalPointer`, and the existing `isPointOverCompanionHitTarget`.
- Produces: the unchanged public React hook API with local sampling, 40 ms polling, 1000 ms recovery, and DOM fallback listeners.

- [ ] **Step 1: Write a wiring regression that fails on current source**

```ts
import { describe, expect, test } from 'bun:test';
import { readFileSync } from 'node:fs';

const source = readFileSync(new URL('./useCompanionClickThrough.ts', import.meta.url), 'utf8');

describe('companion click-through wiring', () => {
  test('uses native local samples and never rebuilds them from global geometry', () => {
    expect(source.includes('getCompanionLocalPointer')).toBe(true);
    expect(source.includes('CompanionClickThroughController')).toBe(true);
    expect(source.includes('cursorPosition')).toBe(false);
    expect(source.includes('outerPosition')).toBe(false);
    expect(source.includes('devicePixelRatio')).toBe(false);
    expect(source.includes('onMoved')).toBe(false);
    expect(source.includes('onResized')).toBe(false);
  });

  test('keeps normal and recovery scheduling explicit', () => {
    expect(source.includes('RECOVERY_INTERVAL_MS = 1000')).toBe(true);
    expect(source.includes("mode === 'poll' ? intervalMs : RECOVERY_INTERVAL_MS")).toBe(true);
    expect(source.includes("addEventListener('pointermove'")).toBe(true);
    expect(source.includes("addEventListener('pointerleave'")).toBe(true);
  });
});
```

- [ ] **Step 2: Run the hook wiring RED**

Run:

```bash
bun test ui/src/renderer/pages/companion/useCompanionClickThrough.test.ts
```

Expected: FAIL because the current hook still contains global cursor/origin/DPR logic.

- [ ] **Step 3: Integrate the controller with a cancellation-safe recursive timer**

Keep the hook's options and refs unchanged. Replace origin caching, moved/resized listeners, heartbeat, and interval polling with:

```ts
const RECOVERY_INTERVAL_MS = 1000;
let disposed = false;
let timer: ReturnType<typeof setTimeout> | null = null;
let controller: CompanionClickThroughController | null = null;
let warned = false;

const schedule = (delay: number): void => {
  if (disposed) return;
  timer = setTimeout(() => void runTick(), delay);
};

const runTick = async (): Promise<void> => {
  if (disposed || !controller) return;
  const mode = await controller.tick({
    captureAll: Boolean(captureAllRef.current),
    dragging: Boolean(draggingRef.current),
  });
  if (!disposed && mode !== 'unsupported') {
    schedule(mode === 'poll' ? intervalMs : RECOVERY_INTERVAL_MS);
  }
};
```

The async setup must import the current window, instantiate the controller with the real sampler, viewport, hit-test, hover callback, and a warn-once `onError`, then await `initialize()` before scheduling the first tick. Register `window` pointermove and pointerleave handlers that forward client coordinates to the controller fallback methods. Wrap the entire setup in a catch that warns once and requests `setIgnoreCursorEvents(false)`.

Cleanup must set `disposed=true`, clear the timeout, remove both DOM pointer listeners, clear hover, and call `void controller?.dispose()`; if controller construction never completed, directly request `setIgnoreCursorEvents(false)` through the imported window handle.

- [ ] **Step 4: Run focused hook/controller/hit tests**

Run:

```bash
bun test ui/src/renderer/pages/companion/useCompanionClickThrough.test.ts ui/src/renderer/pages/companion/companionClickThroughController.test.ts ui/src/renderer/pages/companion/companionLocalPointer.test.ts ui/src/renderer/pages/companion/companionHitTarget.test.ts
```

Expected: all focused tests PASS.

---

### Task 5: Efficient final verification without committing

**Files:**
- Verify all files changed by Tasks 1-4.
- Keep `docs/superpowers/plans/2026-07-18-companion-multimonitor-pointer-routing.md` unstaged.

**Interfaces:**
- Consumes: completed Rust command, renderer adapter/controller/hook, and tests.
- Produces: fresh verification evidence and an uncommitted handoff diff.

- [ ] **Step 1: Run the full companion frontend directory once**

Run:

```bash
bun test ui/src/renderer/pages/companion
```

Expected: all companion tests PASS with zero failures.

- [ ] **Step 2: Run TypeScript validation once**

Run:

```bash
bun run typecheck
```

Expected: exit 0 with no TypeScript errors.

- [ ] **Step 3: Reuse the Task 1 Rust evidence and run only non-test Rust gates**

Do not repeat `cargo test`. Run:

```bash
cargo fmt --check
cargo check -p nomifun-desktop
```

Expected: both commands exit 0.

- [ ] **Step 4: Inspect the final uncommitted change set**

Run:

```bash
git diff --check
git status --short
git diff --stat
```

Expected: no whitespace errors; only the plan and intended companion/Cargo files are modified or untracked; no implementation file is staged or committed.

- [ ] **Step 5: Report platform evidence honestly**

Report current-host macOS compile/test evidence separately from source-reviewed Windows/Linux coverage. State that physical Windows mixed-DPI, Linux X11/XWayland, and native Wayland runtime matrices require their respective hosts and do not claim those hardware runs from this macOS workspace.

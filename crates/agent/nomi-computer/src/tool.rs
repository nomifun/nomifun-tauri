//! The Computer tool: screenshot, mouse/keyboard synthesis, window control.

use std::sync::Mutex;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use serde_json::{Value, json};

use nomi_a11y::{
    A11yEngine, A11yError, ElementAction, ElementEntry, ObserveOpts, SnapshotGen, Source, Target,
};
use nomi_config::config::ComputerConfig;
use nomi_protocol::events::ToolCategory;
use nomi_tools::Tool;
use nomi_types::tool::{JsonSchema, ToolResult};

use crate::input::{self, ScrollDirection};
use crate::keys::parse_key_combo;
use crate::scale::{map_llm_coord, map_screen_coord};
use crate::screen::{CaptureGeometry, capture_screen, encode_png};
use crate::fallback_backend;

const MAX_WAIT_SECONDS: f64 = 5.0;
const DEFAULT_SCROLL_AMOUNT: i64 = 3;

/// Example key combo for the platform we are compiled for. The accelerator
/// modifier differs by OS (Command on macOS, Control on Windows/Linux), so we
/// steer the model toward the right one instead of always suggesting `cmd`,
/// which is a macOS idiom. `parse_key_combo` still accepts `cmd` everywhere and
/// remaps it per-platform, but a correct example reduces wrong presses.
#[cfg(target_os = "macos")]
const KEY_COMBO_EXAMPLE: &str = "cmd+shift+t";
#[cfg(not(target_os = "macos"))]
const KEY_COMBO_EXAMPLE: &str = "ctrl+shift+t";

const DESCRIPTION: &str = "Control the local desktop: read the accessibility tree, take \
screenshots, move and click the mouse, type text, press keys, scroll, and manage windows.\n\n\
PREFER the accessibility-first flow: call `observe` to get a numbered list of interactable \
elements (with a Set-of-Marks overlay screenshot when available), then act on one with \
`click_element` and its `[ref]`. This is far more reliable than guessing pixel coordinates. \
Fall back to pixel actions only when an element is not in the accessibility tree (e.g. canvas \
or some web content).\n\n\
Actions:\n\
- observe: read the desktop's accessibility tree (foreground window + other open windows) → a \
hierarchical `desktop → window → controls` tree with numbered `[ref]` elements (+ a Set-of-Marks \
overlay). Do this first; re-run it after any UI change (a `[ref]` is only valid for the latest \
snapshot).\n\
- click_element: activate the element with the given `ref` from the latest `observe` \
(uses the accessibility action, with an automatic pixel-click fallback).\n\
- right_click_element / double_click_element: right-click or double-click the element with the \
given `ref` (pixel gesture at the element center).\n\
- set_element_value: set the `text` value of the element with the given `ref` (accessibility \
set-value, with a focus-and-type fallback).\n\
- launch: open an application, URL, file, or folder reliably via the OS shell — pass `target` \
(e.g. \"notepad\", \"msedge\", \"https://example.com\", a file path) and optionally `app` to open \
the target WITH a specific application. ALWAYS use this to open apps/URLs; do NOT run `cmd /c \
start`, `Start-Process`, or `explorer` in a shell — those are unreliable here.\n\
- screenshot: capture the screen (optional `display` index) when you need raw pixels.\n\
- cursor_position: report the mouse cursor position in screenshot coordinates.\n\
- list_windows: list open windows with ids, titles, positions and sizes.\n\
- left_click / right_click / middle_click / double_click / triple_click: click at (`x`, `y`).\n\
- mouse_move: move the cursor to (`x`, `y`) without clicking.\n\
- left_click_drag: press at (`start_x`, `start_y`), drag to (`end_x`, `end_y`), release.\n\
- type: type the `text` string into the focused control.\n\
- key: press a key or combo from `key`, e.g. \"enter\". Use the platform's primary \
accelerator modifier (Command on macOS, Control on Windows/Linux) for shortcuts.\n\
- scroll: scroll in `direction` (up/down/left/right) by `amount` wheel clicks, optionally \
at (`x`, `y`).\n\
- focus_window: bring the window with `window_id` to the front (click-to-raise fallback).\n\
- wait: pause for `seconds` (max 5) to let the UI settle.\n\n\
Usage notes:\n\
- Prefer `observe` + `click_element [ref]` over pixel coordinates whenever the element is \
listed.\n\
- Take a screenshot before interacting so you can see the screen, and take another after \
acting to verify the effect.\n\
- All coordinates are pixel positions in the most recent screenshot; they are mapped to \
the real screen automatically.\n\
- Prefer key presses (e.g. \"enter\") over clicking buttons when both work.";

pub struct ComputerTool {
    max_screenshot_edge: u32,
    /// Tool description with the session's dynamic capability note appended
    /// (computed once at construction; part of the cacheable tool schema).
    description: String,
    /// Geometry of the most recent screenshot; pointer coordinates from the
    /// model are interpreted in that image's pixel space.
    last_capture: Mutex<Option<CaptureGeometry>>,
    /// Lazily-initialized accessibility engine (a11y-first targeting). `Some(Err)`
    /// caches an unavailable backend (an OS without an a11y engine, or a startup
    /// failure) so we don't retry per call.
    a11y: Mutex<Option<Result<Arc<dyn A11yEngine>, String>>>,
    /// The most recent `observe` snapshot, for resolving `[ref]` actions. Element
    /// bounds here are in OS accessibility coordinates (screen logical points).
    last_snapshot: Mutex<Option<SnapshotCache>>,
}

struct SnapshotCache {
    generation: SnapshotGen,
    entries: Vec<CachedEntry>,
}

#[derive(Clone)]
struct CachedEntry {
    /// What the model sees: display `[ref]`, role/name, pixel-space bounds, source.
    display: ElementEntry,
    target: CachedTarget,
}

#[derive(Clone, Copy)]
enum CachedTarget {
    /// Accessibility element: act via the engine; `screen_center` is the pixel
    /// fallback (screen logical coordinates).
    Ax {
        engine_ref: u32,
        screen_center: (i32, i32),
    },
    /// OCR / pixel-only element: act by clicking `screen_center`.
    Pixel { screen_center: (i32, i32) },
}

/// Run OCR fusion when the accessibility tree yields fewer than this many
/// interactable elements (e.g. Electron/canvas/game windows), to recover text
/// targets the tree does not expose. Keeps OCR off the hot path for a11y-rich
/// native apps.
const OCR_FUSION_AX_THRESHOLD: usize = 6;

impl ComputerTool {
    pub fn new(config: &ComputerConfig) -> Self {
        Self {
            max_screenshot_edge: config.max_screenshot_edge,
            description: format!("{DESCRIPTION}{}", capabilities_note()),
            last_capture: Mutex::new(None),
            a11y: Mutex::new(None),
            last_snapshot: Mutex::new(None),
        }
    }

    /// Lazily construct (and cache) the accessibility engine. The error string
    /// is cached too, so an unavailable backend is reported without retrying.
    fn engine(&self) -> Result<Arc<dyn A11yEngine>, String> {
        let mut guard = self.a11y.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        if guard.is_none() {
            *guard = Some(nomi_a11y::create_engine().map_err(|e| e.to_string()));
        }
        guard.as_ref().unwrap().clone()
    }

    /// a11y-first "look": read the focused window's accessibility tree, return a
    /// numbered element list, and (when screen capture is available) a
    /// Set-of-Marks overlay screenshot. Needs only the Accessibility grant for
    /// the element list; the overlay additionally needs Screen Recording.
    async fn do_observe(&self) -> ToolResult {
        let engine = match self.engine() {
            Ok(e) => e,
            Err(msg) => {
                return ToolResult::error(format!(
                    "Accessibility engine unavailable: {msg} Use the pixel actions \
                     (screenshot + click with x,y) instead."
                ));
            }
        };
        let eng = engine.clone();
        let snap = tokio::task::spawn_blocking(move || eng.observe(&ObserveOpts::default()))
            .await
            .unwrap_or_else(|e| Err(A11yError::Backend(format!("observe task failed: {e}"))));
        let snap = match snap {
            Ok(s) => s,
            Err(e) => return ToolResult::error(format!("Accessibility observe failed: {e}")),
        };
        let app_note = snap
            .app_name
            .as_deref()
            .map(|a| format!(" in {a}"))
            .unwrap_or_default();
        let ax_note = if snap.truncated {
            " (a11y tree truncated to the node budget)"
        } else {
            ""
        };
        // The engine numbers controls 1..N (grouped per window, then reading
        // order); OCR-fused targets continue after this so refs never collide.
        let max_ax_ref = snap.entries.iter().map(|e| e.r#ref).max().unwrap_or(0);

        // Capture a screenshot for the overlay + OCR fusion + pixel mapping. If
        // it is denied, fall back to an AX-only text list (a11y needs only the
        // Accessibility grant) — the core a11y-first win.
        let max_edge = self.max_screenshot_edge;
        let captured = tokio::task::spawn_blocking(move || capture_screen(None, max_edge))
            .await
            .unwrap_or_else(|e| Err(format!("Screenshot task failed: {e}")));

        let mut shot = match captured {
            Ok(shot) => {
                *self.last_capture.lock().unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(shot.geometry);
                shot
            }
            Err(_) => {
                // AX-only: no overlay, no OCR. Display bounds stay AX-space; show
                // the engine's hierarchical semantic tree (desktop → window → …).
                let mut cached: Vec<CachedEntry> = Vec::with_capacity(snap.entries.len());
                for e in &snap.entries {
                    let (cx, cy) = e.bounds.center();
                    cached.push(CachedEntry {
                        display: e.clone(),
                        target: CachedTarget::Ax {
                            engine_ref: e.r#ref,
                            screen_center: (cx as i32, cy as i32),
                        },
                    });
                }
                let count = cached.len();
                *self.last_snapshot.lock().unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(SnapshotCache {
                    generation: snap.generation,
                    entries: cached,
                });
                return ToolResult::text(format!(
                    "Accessibility snapshot (gen {}): {count} element(s){app_note}{ax_note}. No \
                     Set-of-Marks overlay (screen capture unavailable) — still actionable by \
                     [ref] with `click_element`.\n\n{}",
                    snap.generation.0, snap.text
                ));
            }
        };
        let geom = shot.geometry;

        // AX entries keep their engine [ref] (so the overlay numbers, the semantic
        // tree, and click_element all agree); bounds are mapped to pixel space for
        // the overlay.
        let mut cached: Vec<CachedEntry> = Vec::with_capacity(snap.entries.len());
        for e in &snap.entries {
            let (cx, cy) = e.bounds.center();
            let mut display = e.clone();
            display.bounds = ax_rect_to_pixel(e.bounds, &geom);
            cached.push(CachedEntry {
                display,
                target: CachedTarget::Ax {
                    engine_ref: e.r#ref,
                    screen_center: (cx as i32, cy as i32),
                },
            });
        }

        // OCR fusion when the accessibility tree is thin (Electron/canvas/games):
        // recover on-screen text Vision can read but the tree does not expose.
        // OCR targets are numbered AFTER the AX refs and listed in an appendix.
        let mut ocr_appendix: Vec<String> = Vec::new();
        let mut next_ref = max_ax_ref;
        if snap.entries.len() < OCR_FUSION_AX_THRESHOLD {
            let img = shot.image.clone();
            let langs = vec!["zh-Hans".to_string(), "en-US".to_string()];
            let ocr = tokio::task::spawn_blocking(move || nomi_a11y::ocr_screenshot(&img, &langs))
                .await
                .unwrap_or_else(|e| Err(A11yError::Backend(format!("OCR task failed: {e}"))));
            if let Ok(lines) = ocr {
                for line in lines {
                    let (lcx, lcy) = line.bounds.center();
                    // Skip text already covered by an accessibility element.
                    let covered = cached.iter().any(|c| {
                        let b = c.display.bounds;
                        matches!(c.target, CachedTarget::Ax { .. })
                            && lcx >= b.x
                            && lcx <= b.x + b.w
                            && lcy >= b.y
                            && lcy <= b.y + b.h
                    });
                    if covered {
                        continue;
                    }
                    next_ref += 1;
                    let (sx, sy) = self.to_screen(lcx as i32, lcy as i32);
                    ocr_appendix.push(format!("[{next_ref}] text {:?}", line.text));
                    cached.push(CachedEntry {
                        display: ElementEntry {
                            r#ref: next_ref,
                            role: "text".to_string(),
                            name: Some(line.text),
                            value: None,
                            states: Vec::new(),
                            bounds: line.bounds,
                            source: Source::Ocr,
                        },
                        target: CachedTarget::Pixel {
                            screen_center: (sx, sy),
                        },
                    });
                }
            }
        }

        let display: Vec<ElementEntry> = cached.iter().map(|c| c.display.clone()).collect();
        // Display the engine's hierarchical semantic tree; append OCR targets.
        let ocr_count = ocr_appendix.len();
        let mut text = snap.text.clone();
        if !ocr_appendix.is_empty() {
            text.push_str("\n\nAdditional on-screen text (OCR — click by [ref]):\n");
            text.push_str(&ocr_appendix.join("\n"));
        }
        let ocr_note = if ocr_count > 0 {
            format!(" (+{ocr_count} via OCR for a11y-thin content)")
        } else {
            String::new()
        };
        let header = format!(
            "Accessibility snapshot (gen {}): {} element(s){app_note}{ax_note}{ocr_note}. The tree \
             below is desktop → window → controls; act on a control with the `click_element` \
             action and its [ref]. Re-run `observe` after any UI change (a [ref] is only valid for \
             the latest snapshot).\n\n{text}",
            snap.generation.0,
            display.len()
        );

        *self.last_snapshot.lock().unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(SnapshotCache {
            generation: snap.generation,
            entries: cached,
        });

        nomi_a11y::overlay::draw_set_of_marks(&mut shot.image, &display);
        match encode_png(&shot.image) {
            Ok(img) => ToolResult::text(header).with_images(vec![img]),
            Err(_) => ToolResult::text(header),
        }
    }

    /// Look up a `[ref]` in the latest snapshot, returning its generation and a
    /// clone of the cached entry (with its action target).
    fn resolve_ref(&self, r: u32) -> Result<(SnapshotGen, CachedEntry), String> {
        let guard = self.last_snapshot.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        match guard.as_ref() {
            Some(cache) => match cache.entries.iter().find(|c| c.display.r#ref == r) {
                Some(c) => Ok((cache.generation, c.clone())),
                None => Err(format!(
                    "No element [{r}] in the latest snapshot. Run `observe` and use a ref it lists."
                )),
            },
            None => Err("No accessibility snapshot yet. Run the `observe` action first.".to_string()),
        }
    }

    /// Act on an element by its `[ref]` from the latest `observe` snapshot.
    /// Accessibility elements use AXPress with an automatic pixel-click
    /// fallback; OCR/pixel-only elements click their center directly.
    async fn do_click_element(&self, input: &Value) -> ToolResult {
        let Some(r) = input.get("ref").and_then(|v| v.as_u64()) else {
            return ToolResult::error(
                "Missing required parameter `ref` for click_element (a number from the latest \
                 observe snapshot).",
            );
        };
        let r = r as u32;
        let (generation, entry) = match self.resolve_ref(r) {
            Ok(v) => v,
            Err(msg) => return ToolResult::error(msg),
        };

        match entry.target {
            CachedTarget::Ax {
                engine_ref,
                screen_center,
            } => {
                let engine = match self.engine() {
                    Ok(e) => e,
                    Err(msg) => {
                        return ToolResult::error(format!("Accessibility engine unavailable: {msg}"));
                    }
                };
                let eng = engine.clone();
                let result = tokio::task::spawn_blocking(move || {
                    eng.invoke(&Target::Ref(engine_ref), generation, ElementAction::LeftClick)
                })
                .await
                .unwrap_or_else(|e| Err(A11yError::Backend(format!("invoke task failed: {e}"))));
                match result {
                    Ok(eff) => ToolResult::text(format!(
                        "{}. Run `observe` (or take a screenshot) to verify the result.",
                        eff.message
                    )),
                    Err(e) => {
                        let (sx, sy) = screen_center;
                        match input::click(sx, sy, enigo::Button::Left, 1).await {
                            Ok(()) => ToolResult::text(format!(
                                "The accessibility action on [{r}] did not succeed ({e}); fell \
                                 back to a pixel click at the element center. Run `observe` to verify."
                            )),
                            Err(pe) => ToolResult::error(format!(
                                "Accessibility action on [{r}] failed ({e}) and the pixel fallback \
                                 also failed: {pe}"
                            )),
                        }
                    }
                }
            }
            CachedTarget::Pixel { screen_center } => {
                let (sx, sy) = screen_center;
                match input::click(sx, sy, enigo::Button::Left, 1).await {
                    Ok(()) => ToolResult::text(format!(
                        "Clicked element [{r}] (OCR/pixel target) at its center. Run `observe` to verify."
                    )),
                    Err(pe) => ToolResult::error(format!("Click on [{r}] failed: {pe}")),
                }
            }
        }
    }

    /// Set the text value of an element by `[ref]`. Accessibility elements use
    /// AXValue with a focus-then-type fallback; OCR/pixel-only elements click
    /// then type.
    async fn do_set_element_value(&self, input: &Value) -> ToolResult {
        let Some(r) = input.get("ref").and_then(|v| v.as_u64()) else {
            return ToolResult::error(
                "Missing required parameter `ref` for set_element_value (from the latest observe).",
            );
        };
        let Some(text) = input.get("text").and_then(|v| v.as_str()) else {
            return ToolResult::error("Missing required parameter `text` for set_element_value.");
        };
        let r = r as u32;
        let text = text.to_string();
        let (generation, entry) = match self.resolve_ref(r) {
            Ok(v) => v,
            Err(msg) => return ToolResult::error(msg),
        };

        // For accessibility elements, try the semantic set-value first.
        if let CachedTarget::Ax { engine_ref, .. } = entry.target {
            let engine = match self.engine() {
                Ok(e) => e,
                Err(msg) => {
                    return ToolResult::error(format!("Accessibility engine unavailable: {msg}"));
                }
            };
            let eng = engine.clone();
            let value = text.clone();
            let result = tokio::task::spawn_blocking(move || {
                eng.invoke(
                    &Target::Ref(engine_ref),
                    generation,
                    ElementAction::SetValue(value),
                )
            })
            .await
            .unwrap_or_else(|e| Err(A11yError::Backend(format!("invoke task failed: {e}"))));
            if let Ok(eff) = result {
                return ToolResult::text(format!("{}. Run `observe` to verify the value.", eff.message));
            }
        }

        // Fallback (OCR/pixel target, or AX set-value did not take): click the
        // element to focus it, then type.
        let (sx, sy) = match entry.target {
            CachedTarget::Ax { screen_center, .. } => screen_center,
            CachedTarget::Pixel { screen_center } => screen_center,
        };
        if let Err(pe) = input::click(sx, sy, enigo::Button::Left, 1).await {
            return ToolResult::error(format!(
                "Could not focus element [{r}] to type into it: {pe}"
            ));
        }
        match input::type_text(text).await {
            Ok(()) => ToolResult::text(format!(
                "Set element [{r}] by clicking the field and typing the text. Run `observe` to verify."
            )),
            Err(te) => ToolResult::error(format!("Typing into element [{r}] failed: {te}")),
        }
    }

    /// Resolve a `[ref]` from the latest snapshot to its screen-space center,
    /// for a pixel gesture (right/double click) that has no semantic equivalent.
    fn ref_screen_center(&self, r: u32) -> Result<(i32, i32), String> {
        let (_, entry) = self.resolve_ref(r)?;
        Ok(match entry.target {
            CachedTarget::Ax { screen_center, .. } => screen_center,
            CachedTarget::Pixel { screen_center } => screen_center,
        })
    }

    /// Perform a pixel mouse gesture (right-click / double-click) on the element
    /// addressed by `[ref]`. These are pointer gestures with no UIA semantic
    /// equivalent, so they click the element's center directly.
    async fn do_element_gesture(
        &self,
        input: &Value,
        button: enigo::Button,
        count: u32,
        verb: &str,
    ) -> ToolResult {
        let Some(r) = input.get("ref").and_then(|v| v.as_u64()) else {
            return ToolResult::error(format!(
                "Missing required parameter `ref` for {verb} (a number from the latest observe \
                 snapshot)."
            ));
        };
        let r = r as u32;
        let (sx, sy) = match self.ref_screen_center(r) {
            Ok(v) => v,
            Err(msg) => return ToolResult::error(msg),
        };
        match input::click(sx, sy, button, count).await {
            Ok(()) => ToolResult::text(format!(
                "Performed {verb} on element [{r}] at its center. Run `observe` (or take a \
                 screenshot) to verify the result."
            )),
            Err(e) => ToolResult::error(format!("{verb} on element [{r}] failed: {e}")),
        }
    }

    /// Reliably open an application, URL, file, or folder via the OS shell
    /// (ShellExecute on Windows). The dependable way to launch things — never
    /// shell out to `cmd /c start` / `Start-Process`, which fail and pop a
    /// "Windows cannot find" dialog on this host.
    async fn do_launch(&self, input: &Value) -> ToolResult {
        let Some(target) = input.get("target").and_then(|v| v.as_str()) else {
            return ToolResult::error(
                "Missing required parameter `target` for launch (a URL like \"https://…\", a \
                 file/folder path, or an application name like \"notepad\" / \"msedge\").",
            );
        };
        let app = input.get("app").and_then(|v| v.as_str());
        match crate::launch::launch(target, app).await {
            Ok(msg) => ToolResult::text(format!(
                "{msg} Take a screenshot or run `observe` to see the result."
            )),
            Err(e) => ToolResult::error(e),
        }
    }

    /// Map model-provided screenshot coordinates to absolute screen
    /// coordinates. Identity when no screenshot has been taken yet.
    fn to_screen(&self, x: i32, y: i32) -> (i32, i32) {
        match *self.last_capture.lock().unwrap_or_else(|poisoned| poisoned.into_inner()) {
            Some(g) => {
                let (lx, ly) = map_llm_coord(x, y, g.img_w, g.img_h, g.logical_w, g.logical_h);
                (g.origin_x + lx, g.origin_y + ly)
            }
            None => (x, y),
        }
    }

    /// Map an absolute screen coordinate into the most recent screenshot's
    /// pixel space (for reporting the cursor to the model).
    fn to_image(&self, x: i32, y: i32) -> (i32, i32) {
        match *self.last_capture.lock().unwrap_or_else(|poisoned| poisoned.into_inner()) {
            Some(g) => map_screen_coord(
                x - g.origin_x,
                y - g.origin_y,
                g.img_w,
                g.img_h,
                g.logical_w,
                g.logical_h,
            ),
            None => (x, y),
        }
    }

    async fn do_screenshot(&self, input: &Value) -> ToolResult {
        let display = match input.get("display") {
            None | Some(Value::Null) => None,
            Some(v) => match v.as_u64() {
                Some(d) => Some(d as usize),
                None => {
                    return ToolResult::error(
                        "Parameter `display` must be a non-negative integer display index.",
                    );
                }
            },
        };

        let max_edge = self.max_screenshot_edge;
        let captured = tokio::task::spawn_blocking(move || capture_screen(display, max_edge))
            .await
            .unwrap_or_else(|e| Err(format!("Screenshot task failed: {e}")));

        match captured {
            Ok(shot) => {
                *self.last_capture.lock().unwrap_or_else(|poisoned| poisoned.into_inner()) =
                    Some(shot.geometry);
                let text = format!(
                    "Screenshot captured: {}x{} (display {}, scaled from {}x{} physical). \
                     Coordinates you provide will be mapped back to the screen automatically.",
                    shot.geometry.img_w,
                    shot.geometry.img_h,
                    shot.display_index,
                    shot.physical_w,
                    shot.physical_h
                );
                match encode_png(&shot.image) {
                    Ok(img) => ToolResult::text(text).with_images(vec![img]),
                    Err(e) => ToolResult::error(e),
                }
            }
            Err(e) => ToolResult::error(e),
        }
    }

    async fn do_cursor_position(&self) -> ToolResult {
        match input::cursor_position().await {
            Ok((sx, sy)) => {
                let has_capture = self
                    .last_capture
                    .lock()
                    .expect("last_capture poisoned")
                    .is_some();
                if has_capture {
                    let (ix, iy) = self.to_image(sx, sy);
                    ToolResult::text(format!(
                        "Cursor position: ({ix}, {iy}) in screenshot coordinates \
                         (screen: ({sx}, {sy}))."
                    ))
                } else {
                    ToolResult::text(format!(
                        "Cursor position: ({sx}, {sy}) in screen coordinates \
                         (no screenshot taken yet)."
                    ))
                }
            }
            Err(e) => ToolResult::error(e),
        }
    }

    async fn do_list_windows(&self) -> ToolResult {
        let listed = tokio::task::spawn_blocking(fallback_backend::list_windows)
            .await
            .unwrap_or_else(|e| Err(format!("Window listing task failed: {e}")));
        match listed {
            Ok(list) => ToolResult::text(fallback_backend::format_window_list(&list)),
            Err(e) => ToolResult::error(e),
        }
    }

    async fn do_click(&self, input: &Value, button: enigo::Button, count: u32) -> ToolResult {
        let (x, y) = match require_xy(input, "x", "y") {
            Ok(xy) => xy,
            Err(e) => return ToolResult::error(e),
        };
        let (sx, sy) = self.to_screen(x, y);
        match input::click(sx, sy, button, count).await {
            Ok(()) => ToolResult::text(format!(
                "Clicked at ({x}, {y}) (screen ({sx}, {sy})). Take a screenshot to verify the \
                 result."
            )),
            Err(e) => ToolResult::error(e),
        }
    }

    async fn do_mouse_move(&self, input: &Value) -> ToolResult {
        let (x, y) = match require_xy(input, "x", "y") {
            Ok(xy) => xy,
            Err(e) => return ToolResult::error(e),
        };
        let (sx, sy) = self.to_screen(x, y);
        match input::mouse_move(sx, sy).await {
            Ok(()) => ToolResult::text(format!("Moved cursor to ({x}, {y}) (screen ({sx}, {sy}))." )),
            Err(e) => ToolResult::error(e),
        }
    }

    async fn do_drag(&self, input: &Value) -> ToolResult {
        let (start_x, start_y) = match require_xy(input, "start_x", "start_y") {
            Ok(xy) => xy,
            Err(e) => return ToolResult::error(e),
        };
        let (end_x, end_y) = match require_xy(input, "end_x", "end_y") {
            Ok(xy) => xy,
            Err(e) => return ToolResult::error(e),
        };
        let (sx, sy) = self.to_screen(start_x, start_y);
        let (ex, ey) = self.to_screen(end_x, end_y);
        match input::drag(sx, sy, ex, ey).await {
            Ok(()) => ToolResult::text(format!(
                "Dragged from ({start_x}, {start_y}) to ({end_x}, {end_y}). Take a screenshot \
                 to verify the result."
            )),
            Err(e) => ToolResult::error(e),
        }
    }

    async fn do_type(&self, input: &Value) -> ToolResult {
        let Some(text) = input.get("text").and_then(|v| v.as_str()) else {
            return ToolResult::error(
                "Missing required parameter `text` for the type action.",
            );
        };
        let char_count = text.chars().count();
        match input::type_text(text.to_string()).await {
            Ok(()) => ToolResult::text(format!("Typed {char_count} character(s).")),
            Err(e) => ToolResult::error(e),
        }
    }

    async fn do_key(&self, input: &Value) -> ToolResult {
        let Some(combo) = input.get("key").and_then(|v| v.as_str()) else {
            return ToolResult::error(format!(
                "Missing required parameter `key` for the key action, e.g. \"enter\" or \
                 \"{KEY_COMBO_EXAMPLE}\"."
            ));
        };
        let keys = match parse_key_combo(combo) {
            Ok(keys) => keys,
            Err(e) => return ToolResult::error(e),
        };
        match input::key_combo(keys).await {
            Ok(()) => ToolResult::text(format!("Pressed {combo:?}.")),
            Err(e) => ToolResult::error(e),
        }
    }

    async fn do_scroll(&self, input: &Value) -> ToolResult {
        let Some(direction_str) = input.get("direction").and_then(|v| v.as_str()) else {
            return ToolResult::error(
                "Missing required parameter `direction` for the scroll action \
                 (up, down, left or right).",
            );
        };
        let direction = match ScrollDirection::parse(direction_str) {
            Ok(d) => d,
            Err(e) => return ToolResult::error(e),
        };
        let amount = input
            .get("amount")
            .and_then(|v| v.as_i64())
            .unwrap_or(DEFAULT_SCROLL_AMOUNT)
            .clamp(1, 100) as i32;
        let at = match (
            input.get("x").and_then(|v| v.as_i64()),
            input.get("y").and_then(|v| v.as_i64()),
        ) {
            (Some(x), Some(y)) => Some(self.to_screen(x as i32, y as i32)),
            _ => None,
        };
        match input::scroll(at, direction, amount).await {
            Ok(()) => ToolResult::text(format!(
                "Scrolled {direction_str} by {amount}. Take a screenshot to see the result."
            )),
            Err(e) => ToolResult::error(e),
        }
    }

    async fn do_focus_window(&self, input: &Value) -> ToolResult {
        let Some(window_id) = input.get("window_id").and_then(|v| v.as_u64()) else {
            return ToolResult::error(
                "Missing required parameter `window_id` for the focus_window action. \
                 Use list_windows to get window ids.",
            );
        };
        match fallback_backend::focus_window(window_id as u32).await {
            Ok(msg) => ToolResult::text(msg),
            Err(e) => ToolResult::error(e),
        }
    }

    async fn do_wait(&self, input: &Value) -> ToolResult {
        let requested = input
            .get("seconds")
            .and_then(|v| v.as_f64())
            .unwrap_or(1.0);
        let seconds = requested.clamp(0.0, MAX_WAIT_SECONDS);
        tokio::time::sleep(Duration::from_secs_f64(seconds)).await;
        ToolResult::text(format!("Waited {seconds} second(s)."))
    }
}

/// A session capability note appended to the tool description so the model
/// knows its real abilities up front (a11y availability + the two macOS TCC
/// grants). Computed once at construction from the platform + a live permission
/// probe — it lives in the (cacheable) tool schema, not the system prompt, so it
/// never thrashes the prompt cache.
fn capabilities_note() -> String {
    let mut s = String::from("\n\nThis session's desktop capabilities:\n");
    #[cfg(target_os = "macos")]
    {
        s.push_str(
            "- Accessibility-first targeting (observe / click_element / set_element_value): \
             available, with OCR fusion for accessibility-thin apps.\n",
        );
    }
    #[cfg(target_os = "windows")]
    {
        s.push_str(
            "- Accessibility-first targeting (observe / click_element / set_element_value): \
             available (UI Automation), with OCR fusion for accessibility-thin apps.\n",
        );
    }
    #[cfg(target_os = "linux")]
    {
        s.push_str(
            "- Accessibility-first targeting (observe / click_element / set_element_value): \
             available (AT-SPI).\n",
        );
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows", target_os = "linux")))]
    {
        s.push_str(
            "- Accessibility-first targeting (observe): not yet available on this OS — use the \
             pixel actions (screenshot + click x,y).\n",
        );
    }
    let status = crate::permissions::permission_status();
    let fmt = |g: Option<bool>| match g {
        Some(true) => "granted",
        Some(false) => "NOT granted",
        None => "not applicable on this OS",
    };
    s.push_str(&format!(
        "- Accessibility permission (needed for `observe` and input synthesis): {}.\n",
        fmt(status.accessibility)
    ));
    s.push_str(&format!(
        "- Screen Recording permission (needed for screenshots and the Set-of-Marks overlay; \
         `observe`'s element list works without it): {}.\n",
        fmt(status.screen_recording)
    ));
    s
}

/// Convert an element rectangle from OS accessibility coordinates (screen
/// logical points, top-left origin) into the most recent screenshot's pixel
/// space, so the Set-of-Marks overlay aligns with the captured image. This is
/// the AX-points→pixel conversion (per-monitor origin + logical→pixel scale);
/// it is NOT a plain reuse of the LLM-coordinate mapping.
fn ax_rect_to_pixel(r: nomi_a11y::Rect, g: &CaptureGeometry) -> nomi_a11y::Rect {
    let sx = g.img_w as f64 / g.logical_w.max(1) as f64;
    let sy = g.img_h as f64 / g.logical_h.max(1) as f64;
    nomi_a11y::Rect {
        x: (r.x - g.origin_x as f64) * sx,
        y: (r.y - g.origin_y as f64) * sy,
        w: r.w * sx,
        h: r.h * sy,
    }
}

/// Extract a required (x, y)-style coordinate pair, naming the missing
/// parameters in the error.
fn require_xy(input: &Value, x_name: &str, y_name: &str) -> Result<(i32, i32), String> {
    let x = input.get(x_name).and_then(|v| v.as_i64());
    let y = input.get(y_name).and_then(|v| v.as_i64());
    match (x, y) {
        (Some(x), Some(y)) => Ok((x as i32, y as i32)),
        (None, Some(_)) => Err(format!("Missing required parameter `{x_name}`.")),
        (Some(_), None) => Err(format!("Missing required parameter `{y_name}`.")),
        (None, None) => Err(format!(
            "Missing required parameters `{x_name}` and `{y_name}`."
        )),
    }
}

#[async_trait]
impl Tool for ComputerTool {
    fn name(&self) -> &str {
        "Computer"
    }

    fn description(&self) -> &str {
        &self.description
    }

    fn input_schema(&self) -> JsonSchema {
        json!({
            "type": "object",
            "properties": {
                "action": {
                    "type": "string",
                    "enum": [
                        "observe", "click_element", "set_element_value",
                        "right_click_element", "double_click_element",
                        "launch",
                        "screenshot", "cursor_position", "list_windows",
                        "left_click", "right_click", "middle_click",
                        "double_click", "triple_click", "mouse_move",
                        "left_click_drag", "type", "key", "scroll",
                        "focus_window", "wait"
                    ],
                    "description": "The desktop operation to perform"
                },
                "ref": { "type": "integer", "description": "Element number from the latest `observe` snapshot (click_element / right_click_element / double_click_element / set_element_value)" },
                "target": { "type": "string", "description": "What to open (launch action): a URL (https://…), a file/folder path, or an application name (e.g. \"notepad\", \"msedge\")" },
                "app": { "type": "string", "description": "Optional application to open the `target` WITH (launch action), e.g. target a URL and app=\"msedge\" to open it in Edge" },
                "x": { "type": "integer", "description": "X coordinate in pixels of the most recent screenshot" },
                "y": { "type": "integer", "description": "Y coordinate in pixels of the most recent screenshot" },
                "start_x": { "type": "integer", "description": "Drag start X (left_click_drag)" },
                "start_y": { "type": "integer", "description": "Drag start Y (left_click_drag)" },
                "end_x": { "type": "integer", "description": "Drag end X (left_click_drag)" },
                "end_y": { "type": "integer", "description": "Drag end Y (left_click_drag)" },
                "text": { "type": "string", "description": "Text to type (type action)" },
                "key": { "type": "string", "description": format!("Key or combo to press, e.g. \"enter\" or \"{KEY_COMBO_EXAMPLE}\" (key action)") },
                "direction": {
                    "type": "string",
                    "enum": ["up", "down", "left", "right"],
                    "description": "Scroll direction (scroll action)"
                },
                "amount": { "type": "integer", "description": "Scroll wheel clicks, default 3 (scroll action)" },
                "display": { "type": "integer", "description": "Display index to capture, default primary (screenshot action)" },
                "window_id": { "type": "integer", "description": "Window id from list_windows (focus_window action)" },
                "seconds": { "type": "number", "description": "Seconds to wait, max 5 (wait action)" }
            },
            "required": ["action"]
        })
    }

    fn is_concurrency_safe(&self, _input: &Value) -> bool {
        false
    }

    async fn execute(&self, input: Value) -> ToolResult {
        let Some(action) = input.get("action").and_then(|v| v.as_str()) else {
            return ToolResult::error(
                "Missing required parameter `action`. See the tool description for the \
                 list of supported actions.",
            );
        };

        tracing::debug!(action = %action, "ComputerTool executing");

        match action {
            "observe" => self.do_observe().await,
            "click_element" => self.do_click_element(&input).await,
            "set_element_value" => self.do_set_element_value(&input).await,
            "right_click_element" => {
                self.do_element_gesture(&input, enigo::Button::Right, 1, "right-click").await
            }
            "double_click_element" => {
                self.do_element_gesture(&input, enigo::Button::Left, 2, "double-click").await
            }
            "launch" => self.do_launch(&input).await,
            "screenshot" => self.do_screenshot(&input).await,
            "cursor_position" => self.do_cursor_position().await,
            "list_windows" => self.do_list_windows().await,
            "left_click" => self.do_click(&input, enigo::Button::Left, 1).await,
            "right_click" => self.do_click(&input, enigo::Button::Right, 1).await,
            "middle_click" => self.do_click(&input, enigo::Button::Middle, 1).await,
            "double_click" => self.do_click(&input, enigo::Button::Left, 2).await,
            "triple_click" => self.do_click(&input, enigo::Button::Left, 3).await,
            "mouse_move" => self.do_mouse_move(&input).await,
            "left_click_drag" => self.do_drag(&input).await,
            "type" => self.do_type(&input).await,
            "key" => self.do_key(&input).await,
            "scroll" => self.do_scroll(&input).await,
            "focus_window" => self.do_focus_window(&input).await,
            "wait" => self.do_wait(&input).await,
            other => ToolResult::error(format!(
                "Unknown action {other:?}. Supported actions: observe, click_element, \
                 set_element_value, right_click_element, double_click_element, launch, screenshot, \
                 cursor_position, list_windows, left_click, right_click, middle_click, \
                 double_click, triple_click, mouse_move, left_click_drag, type, key, scroll, \
                 focus_window, wait."
            )),
        }
    }

    fn category(&self) -> ToolCategory {
        // Conservative default; per-action classification in category_for.
        ToolCategory::Exec
    }

    fn category_for(&self, input: &Value) -> ToolCategory {
        match input.get("action").and_then(|v| v.as_str()) {
            Some("observe" | "screenshot" | "cursor_position" | "list_windows" | "wait") => {
                ToolCategory::Info
            }
            _ => ToolCategory::Exec,
        }
    }

    fn describe(&self, input: &Value) -> String {
        let action = input.get("action").and_then(|v| v.as_str()).unwrap_or("?");
        let detail = match action {
            "observe" => "observe (accessibility snapshot)".to_string(),
            "click_element" => {
                let r = input.get("ref").and_then(|v| v.as_u64()).unwrap_or(0);
                format!("click element [{r}]")
            }
            "set_element_value" => {
                let r = input.get("ref").and_then(|v| v.as_u64()).unwrap_or(0);
                let text = input.get("text").and_then(|v| v.as_str()).unwrap_or("");
                format!("set element [{r}] = {:?}", nomi_tools::truncate_utf8(text, 40))
            }
            "right_click_element" | "double_click_element" => {
                let r = input.get("ref").and_then(|v| v.as_u64()).unwrap_or(0);
                let verb = if action == "right_click_element" { "right-click" } else { "double-click" };
                format!("{verb} element [{r}]")
            }
            "launch" => {
                let target = input.get("target").and_then(|v| v.as_str()).unwrap_or("");
                match input.get("app").and_then(|v| v.as_str()) {
                    Some(app) => format!("launch {:?} with {app:?}", nomi_tools::truncate_utf8(target, 60)),
                    None => format!("launch {:?}", nomi_tools::truncate_utf8(target, 60)),
                }
            }
            "screenshot" => match input.get("display").and_then(|v| v.as_u64()) {
                Some(d) => format!("screenshot of display {d}"),
                None => "screenshot".to_string(),
            },
            "left_click" | "right_click" | "middle_click" | "double_click" | "triple_click"
            | "mouse_move" => {
                let x = input.get("x").and_then(|v| v.as_i64()).unwrap_or(0);
                let y = input.get("y").and_then(|v| v.as_i64()).unwrap_or(0);
                format!("{action} at ({x}, {y})")
            }
            "left_click_drag" => {
                let sx = input.get("start_x").and_then(|v| v.as_i64()).unwrap_or(0);
                let sy = input.get("start_y").and_then(|v| v.as_i64()).unwrap_or(0);
                let ex = input.get("end_x").and_then(|v| v.as_i64()).unwrap_or(0);
                let ey = input.get("end_y").and_then(|v| v.as_i64()).unwrap_or(0);
                format!("drag from ({sx}, {sy}) to ({ex}, {ey})")
            }
            "type" => {
                let text = input.get("text").and_then(|v| v.as_str()).unwrap_or("");
                format!("type {:?}", nomi_tools::truncate_utf8(text, 40))
            }
            "key" => {
                let key = input.get("key").and_then(|v| v.as_str()).unwrap_or("");
                format!("press {key:?}")
            }
            "scroll" => {
                let dir = input.get("direction").and_then(|v| v.as_str()).unwrap_or("?");
                let amount = input
                    .get("amount")
                    .and_then(|v| v.as_i64())
                    .unwrap_or(DEFAULT_SCROLL_AMOUNT);
                format!("scroll {dir} by {amount}")
            }
            "focus_window" => {
                let id = input.get("window_id").and_then(|v| v.as_u64()).unwrap_or(0);
                format!("focus window {id}")
            }
            "wait" => {
                let secs = input.get("seconds").and_then(|v| v.as_f64()).unwrap_or(1.0);
                format!("wait {secs}s")
            }
            other => other.to_string(),
        };
        format!("Computer: {detail}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tool() -> ComputerTool {
        ComputerTool::new(&ComputerConfig::default())
    }

    // --- schema ---

    #[test]
    fn schema_is_valid_object_with_required_action() {
        let schema = tool().input_schema();
        assert_eq!(schema["type"], "object");
        assert_eq!(schema["required"], json!(["action"]));
        let actions = schema["properties"]["action"]["enum"]
            .as_array()
            .expect("action enum");
        for expected in [
            "screenshot",
            "cursor_position",
            "list_windows",
            "left_click",
            "right_click",
            "middle_click",
            "double_click",
            "triple_click",
            "mouse_move",
            "left_click_drag",
            "type",
            "key",
            "scroll",
            "focus_window",
            "wait",
        ] {
            assert!(
                actions.iter().any(|a| a == expected),
                "schema enum missing {expected}"
            );
        }
        // Round-trips through serde_json without loss.
        let text = serde_json::to_string(&schema).unwrap();
        let parsed: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(parsed, schema);
    }

    #[test]
    fn name_and_metadata() {
        let t = tool();
        assert_eq!(t.name(), "Computer");
        assert!(!t.description().is_empty());
        assert!(!t.is_concurrency_safe(&json!({})));
        assert_eq!(t.category(), ToolCategory::Exec);
    }

    // --- category_for ---

    #[test]
    fn category_for_info_actions() {
        let t = tool();
        for action in ["observe", "screenshot", "cursor_position", "list_windows", "wait"] {
            assert_eq!(
                t.category_for(&json!({"action": action})),
                ToolCategory::Info,
                "{action} should be Info"
            );
        }
    }

    #[test]
    fn click_element_is_exec_and_observe_is_info() {
        let t = tool();
        assert_eq!(
            t.category_for(&json!({"action": "click_element", "ref": 3})),
            ToolCategory::Exec
        );
        assert_eq!(
            t.category_for(&json!({"action": "observe"})),
            ToolCategory::Info
        );
    }

    #[tokio::test]
    async fn click_element_without_ref_is_error() {
        let result = tool().execute(json!({"action": "click_element"})).await;
        assert!(result.is_error);
        assert!(result.content.contains("ref"), "{}", result.content);
    }

    #[tokio::test]
    async fn click_element_without_snapshot_is_error() {
        // No observe has run, so there is no snapshot to resolve the ref against.
        // (On macOS the engine initializes; the error is about the missing
        // snapshot, surfaced as a non-panicking ToolResult error.)
        let result = tool()
            .execute(json!({"action": "click_element", "ref": 1}))
            .await;
        assert!(result.is_error);
    }

    #[test]
    fn describe_observe_and_click_element() {
        let t = tool();
        assert_eq!(
            t.describe(&json!({"action": "observe"})),
            "Computer: observe (accessibility snapshot)"
        );
        assert_eq!(
            t.describe(&json!({"action": "click_element", "ref": 7})),
            "Computer: click element [7]"
        );
    }

    #[test]
    fn category_for_exec_actions() {
        let t = tool();
        for action in [
            "left_click",
            "right_click",
            "middle_click",
            "double_click",
            "triple_click",
            "mouse_move",
            "left_click_drag",
            "type",
            "key",
            "scroll",
            "focus_window",
        ] {
            assert_eq!(
                t.category_for(&json!({"action": action})),
                ToolCategory::Exec,
                "{action} should be Exec"
            );
        }
    }

    #[test]
    fn category_for_unknown_or_missing_action_is_exec() {
        let t = tool();
        assert_eq!(
            t.category_for(&json!({"action": "bogus"})),
            ToolCategory::Exec
        );
        assert_eq!(t.category_for(&json!({})), ToolCategory::Exec);
    }

    // --- execute error paths (no real screen/input needed) ---

    #[tokio::test]
    async fn unknown_action_is_error() {
        let result = tool().execute(json!({"action": "fly"})).await;
        assert!(result.is_error);
        assert!(result.content.contains("fly"), "{}", result.content);
    }

    #[tokio::test]
    async fn missing_action_is_error() {
        let result = tool().execute(json!({})).await;
        assert!(result.is_error);
        assert!(result.content.contains("action"), "{}", result.content);
    }

    #[tokio::test]
    async fn click_without_coordinates_is_error_naming_params() {
        let result = tool().execute(json!({"action": "left_click"})).await;
        assert!(result.is_error);
        assert!(result.content.contains("x"), "{}", result.content);
        assert!(result.content.contains("y"), "{}", result.content);
    }

    #[tokio::test]
    async fn click_with_only_x_is_error_naming_y() {
        let result = tool()
            .execute(json!({"action": "left_click", "x": 10}))
            .await;
        assert!(result.is_error);
        assert!(result.content.contains("`y`"), "{}", result.content);
    }

    #[tokio::test]
    async fn drag_without_end_is_error_naming_params() {
        let result = tool()
            .execute(json!({"action": "left_click_drag", "start_x": 1, "start_y": 2}))
            .await;
        assert!(result.is_error);
        assert!(
            result.content.contains("end_x") && result.content.contains("end_y"),
            "{}",
            result.content
        );
    }

    #[tokio::test]
    async fn type_without_text_is_error() {
        let result = tool().execute(json!({"action": "type"})).await;
        assert!(result.is_error);
        assert!(result.content.contains("text"), "{}", result.content);
    }

    #[tokio::test]
    async fn key_without_key_is_error() {
        let result = tool().execute(json!({"action": "key"})).await;
        assert!(result.is_error);
        assert!(result.content.contains("key"), "{}", result.content);
    }

    #[tokio::test]
    async fn key_with_unknown_combo_is_error() {
        let result = tool()
            .execute(json!({"action": "key", "key": "cmd+notakey"}))
            .await;
        assert!(result.is_error);
        assert!(result.content.contains("notakey"), "{}", result.content);
    }

    #[tokio::test]
    async fn scroll_without_direction_is_error() {
        let result = tool().execute(json!({"action": "scroll"})).await;
        assert!(result.is_error);
        assert!(result.content.contains("direction"), "{}", result.content);
    }

    #[tokio::test]
    async fn scroll_with_bad_direction_is_error() {
        let result = tool()
            .execute(json!({"action": "scroll", "direction": "sideways"}))
            .await;
        assert!(result.is_error);
        assert!(result.content.contains("sideways"), "{}", result.content);
    }

    #[tokio::test]
    async fn focus_window_without_id_is_error() {
        let result = tool().execute(json!({"action": "focus_window"})).await;
        assert!(result.is_error);
        assert!(result.content.contains("window_id"), "{}", result.content);
    }

    #[tokio::test]
    async fn screenshot_with_bad_display_type_is_error() {
        let result = tool()
            .execute(json!({"action": "screenshot", "display": "main"}))
            .await;
        assert!(result.is_error);
        assert!(result.content.contains("display"), "{}", result.content);
    }

    // --- wait ---

    #[tokio::test(start_paused = true)]
    async fn wait_clamps_to_five_seconds() {
        let start = tokio::time::Instant::now();
        let result = tool()
            .execute(json!({"action": "wait", "seconds": 60}))
            .await;
        assert!(!result.is_error, "{}", result.content);
        // Paused-clock runtime: the virtual elapsed time is the slept time.
        assert_eq!(start.elapsed(), Duration::from_secs(5));
        assert!(result.content.contains('5'), "{}", result.content);
    }

    #[tokio::test(start_paused = true)]
    async fn wait_default_is_one_second() {
        let start = tokio::time::Instant::now();
        let result = tool().execute(json!({"action": "wait"})).await;
        assert!(!result.is_error);
        assert_eq!(start.elapsed(), Duration::from_secs(1));
    }

    #[tokio::test(start_paused = true)]
    async fn wait_negative_clamps_to_zero() {
        let start = tokio::time::Instant::now();
        let result = tool()
            .execute(json!({"action": "wait", "seconds": -3}))
            .await;
        assert!(!result.is_error);
        assert_eq!(start.elapsed(), Duration::from_secs(0));
    }

    #[tokio::test]
    async fn wait_is_info_and_not_error() {
        let t = tool();
        let input = json!({"action": "wait", "seconds": 0});
        assert_eq!(t.category_for(&input), ToolCategory::Info);
        let result = t.execute(input).await;
        assert!(!result.is_error);
    }

    // --- describe ---

    #[test]
    fn describe_click() {
        let d = tool().describe(&json!({"action": "left_click", "x": 120, "y": 340}));
        assert_eq!(d, "Computer: left_click at (120, 340)");
    }

    #[test]
    fn describe_screenshot_and_key_and_type() {
        let t = tool();
        assert_eq!(
            t.describe(&json!({"action": "screenshot"})),
            "Computer: screenshot"
        );
        assert_eq!(
            t.describe(&json!({"action": "key", "key": "cmd+shift+t"})),
            "Computer: press \"cmd+shift+t\""
        );
        let typed = t.describe(&json!({"action": "type", "text": "hello"}));
        assert!(typed.contains("hello"), "{typed}");
    }

    #[test]
    fn describe_drag_scroll_focus_wait() {
        let t = tool();
        assert_eq!(
            t.describe(&json!({
                "action": "left_click_drag",
                "start_x": 1, "start_y": 2, "end_x": 3, "end_y": 4
            })),
            "Computer: drag from (1, 2) to (3, 4)"
        );
        assert_eq!(
            t.describe(&json!({"action": "scroll", "direction": "down", "amount": 5})),
            "Computer: scroll down by 5"
        );
        assert_eq!(
            t.describe(&json!({"action": "focus_window", "window_id": 7})),
            "Computer: focus window 7"
        );
        assert_eq!(
            t.describe(&json!({"action": "wait", "seconds": 2})),
            "Computer: wait 2s"
        );
    }

    // --- coordinate mapping through stored geometry ---

    #[test]
    fn to_screen_identity_without_capture() {
        assert_eq!(tool().to_screen(123, 456), (123, 456));
    }

    #[test]
    fn to_screen_maps_with_capture_geometry() {
        let t = tool();
        *t.last_capture.lock().unwrap() = Some(crate::screen::CaptureGeometry {
            img_w: 1568,
            img_h: 980,
            logical_w: 1440,
            logical_h: 900,
            origin_x: 0,
            origin_y: 0,
        });
        assert_eq!(t.to_screen(0, 0), (0, 0));
        assert_eq!(t.to_screen(1567, 979), (1439, 899));
    }

    #[test]
    fn to_screen_applies_monitor_origin() {
        let t = tool();
        *t.last_capture.lock().unwrap() = Some(crate::screen::CaptureGeometry {
            img_w: 1000,
            img_h: 800,
            logical_w: 1000,
            logical_h: 800,
            origin_x: 1440,
            origin_y: -100,
        });
        assert_eq!(t.to_screen(10, 20), (1450, -80));
    }

    // --- capability note (model contract) ---

    #[test]
    fn capabilities_note_advertises_a11y_where_an_engine_exists() {
        let note = capabilities_note();
        // On every OS that create_engine() supports (macOS / Windows / Linux),
        // the model must be told the accessibility-first path is available —
        // never that it is "not yet available on this OS", which would steer it
        // onto fragile pixel guessing even though observe/click_element work.
        #[cfg(any(target_os = "macos", target_os = "windows", target_os = "linux"))]
        {
            assert!(
                note.contains("observe"),
                "the a11y-first path should be advertised: {note}"
            );
            assert!(
                !note.contains("not yet available on this OS"),
                "must not tell the model the a11y path is unavailable on a \
                 supported OS: {note}"
            );
        }
        // The macOS wording must stay byte-for-byte as before so the macOS
        // contract (and prompt cache) does not regress.
        #[cfg(target_os = "macos")]
        assert!(
            note.contains(
                "Accessibility-first targeting (observe / click_element / set_element_value): \
                 available, with OCR fusion for accessibility-thin apps."
            ),
            "macOS capability note changed: {note}"
        );
    }

    // --- real-device tests ---

    // Requires a display and Screen Recording permission.
    #[tokio::test]
    #[ignore]
    async fn screenshot_real() {
        let result = tool().execute(json!({"action": "screenshot"})).await;
        assert!(!result.is_error, "{}", result.content);
        assert_eq!(result.images.len(), 1);
        assert_eq!(result.images[0].media_type, "image/png");
        assert!(result.content.contains("Screenshot captured"));
    }

    // Requires Accessibility permission.
    #[tokio::test]
    #[ignore]
    async fn cursor_position_real() {
        let result = tool().execute(json!({"action": "cursor_position"})).await;
        assert!(!result.is_error, "{}", result.content);
        assert!(result.content.contains("Cursor position"));
    }
}

//! Computer-use domain capabilities (registry form, feature-gated). Lets a
//! Remote/companion agent drive the local desktop — a thin facade over the
//! in-tree `nomi_computer::ComputerTool`, mirroring the inward
//! `mcp-computer-stdio` bridge (same 14 discrete tools, same action mapping,
//! zero duplicated logic). Only compiled with the `computer-use` feature.
//!
//! DangerTier: observe/screenshot/cursor_position/list_windows/wait are `Read`;
//! input-synthesis actions (click/type/key/scroll/launch/…) are `Write` — which
//! is Allowed on every surface incl. Remote, so an external "外部伙伴" can drive
//! the desktop (same posture as the browser `act` tools).

use std::sync::Arc;

use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::computer_registry::{ComputerRegistry, tool_result_to_value};
use crate::deps::{CallerCtx, GatewayDeps};
use crate::registry::{Capability, CapabilityMeta, DangerTier};

fn registry(deps: &GatewayDeps) -> Result<&ComputerRegistry, Value> {
    deps.computer_registry
        .as_ref()
        .ok_or_else(|| json!({ "error": "computer-use is not available on this host" }))
}

async fn run(deps: &GatewayDeps, input: Value) -> Value {
    match registry(deps) {
        Ok(reg) => tool_result_to_value(reg.execute(input).await),
        Err(e) => e,
    }
}

// ---- parameter structs (lifted from the inward computer_stdio bridge) -------

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct NoParams {}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct RefParams {
    /// Element number `[ref]` from the most recent `nomi_computer_snapshot`.
    r#ref: u32,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct SetValueParams {
    /// Element number `[ref]` from the most recent snapshot.
    r#ref: u32,
    /// The text to set into the element.
    text: String,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct XyParams {
    /// X coordinate in pixels of the most recent screenshot.
    x: i64,
    /// Y coordinate in pixels of the most recent screenshot.
    y: i64,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct TypeParams {
    /// The text to type into the focused control.
    text: String,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct KeyParams {
    /// Key or combo to press, e.g. "enter" or "ctrl+a".
    key: String,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct ScrollParams {
    /// Scroll direction: up, down, left, or right.
    direction: String,
    /// Wheel clicks (default 3).
    #[serde(default)]
    amount: Option<i64>,
    /// Optional X to scroll at (screenshot pixels).
    #[serde(default)]
    x: Option<i64>,
    /// Optional Y to scroll at (screenshot pixels).
    #[serde(default)]
    y: Option<i64>,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct LaunchParams {
    /// What to open: a URL (https://…), a file/folder path, or an app name.
    target: String,
    /// Optional application to open the target WITH (e.g. app="msedge").
    #[serde(default)]
    app: Option<String>,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct ScreenshotParams {
    /// Optional display index to capture (default: primary).
    #[serde(default)]
    display: Option<u64>,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct WaitParams {
    /// Seconds to wait (max 5).
    #[serde(default)]
    seconds: Option<f64>,
}

// ---- handlers (forward to the shared tool's action dispatcher) --------------

async fn snapshot(deps: Arc<GatewayDeps>, _ctx: CallerCtx, _p: NoParams) -> Value {
    run(&deps, json!({ "action": "observe" })).await
}
async fn screenshot(deps: Arc<GatewayDeps>, _ctx: CallerCtx, p: ScreenshotParams) -> Value {
    run(&deps, json!({ "action": "screenshot", "display": p.display })).await
}
async fn click(deps: Arc<GatewayDeps>, _ctx: CallerCtx, p: RefParams) -> Value {
    run(&deps, json!({ "action": "click_element", "ref": p.r#ref })).await
}
async fn right_click(deps: Arc<GatewayDeps>, _ctx: CallerCtx, p: RefParams) -> Value {
    run(&deps, json!({ "action": "right_click_element", "ref": p.r#ref })).await
}
async fn double_click(deps: Arc<GatewayDeps>, _ctx: CallerCtx, p: RefParams) -> Value {
    run(&deps, json!({ "action": "double_click_element", "ref": p.r#ref })).await
}
async fn set_value(deps: Arc<GatewayDeps>, _ctx: CallerCtx, p: SetValueParams) -> Value {
    run(&deps, json!({ "action": "set_element_value", "ref": p.r#ref, "text": p.text })).await
}
async fn click_xy(deps: Arc<GatewayDeps>, _ctx: CallerCtx, p: XyParams) -> Value {
    run(&deps, json!({ "action": "left_click", "x": p.x, "y": p.y })).await
}
async fn type_text(deps: Arc<GatewayDeps>, _ctx: CallerCtx, p: TypeParams) -> Value {
    run(&deps, json!({ "action": "type", "text": p.text })).await
}
async fn key(deps: Arc<GatewayDeps>, _ctx: CallerCtx, p: KeyParams) -> Value {
    run(&deps, json!({ "action": "key", "key": p.key })).await
}
async fn scroll(deps: Arc<GatewayDeps>, _ctx: CallerCtx, p: ScrollParams) -> Value {
    run(
        &deps,
        json!({ "action": "scroll", "direction": p.direction, "amount": p.amount, "x": p.x, "y": p.y }),
    )
    .await
}
async fn launch(deps: Arc<GatewayDeps>, _ctx: CallerCtx, p: LaunchParams) -> Value {
    run(&deps, json!({ "action": "launch", "target": p.target, "app": p.app })).await
}
async fn list_windows(deps: Arc<GatewayDeps>, _ctx: CallerCtx, _p: NoParams) -> Value {
    run(&deps, json!({ "action": "list_windows" })).await
}
async fn cursor_position(deps: Arc<GatewayDeps>, _ctx: CallerCtx, _p: NoParams) -> Value {
    run(&deps, json!({ "action": "cursor_position" })).await
}
async fn wait(deps: Arc<GatewayDeps>, _ctx: CallerCtx, p: WaitParams) -> Value {
    run(&deps, json!({ "action": "wait", "seconds": p.seconds })).await
}

pub(crate) fn register(out: &mut Vec<Capability>) {
    out.push(Capability::new::<NoParams, _, _>(
        CapabilityMeta::new(
            "nomi_computer_snapshot",
            "computer",
            "Read the desktop accessibility tree (windows → controls, numbered [ref] + Set-of-Marks overlay). Do this first, then act on a [ref]. Re-run after any UI change. Read-only.",
            DangerTier::Read,
        ),
        snapshot,
    ));
    out.push(Capability::new::<ScreenshotParams, _, _>(
        CapabilityMeta::new(
            "nomi_computer_screenshot",
            "computer",
            "Capture the screen as a PNG (optional `display` index).",
            DangerTier::Read,
        ),
        screenshot,
    ));
    out.push(Capability::new::<RefParams, _, _>(
        CapabilityMeta::new(
            "nomi_computer_click",
            "computer",
            "Activate the element with the given `ref` from the latest snapshot.",
            DangerTier::Write,
        ),
        click,
    ));
    out.push(Capability::new::<RefParams, _, _>(
        CapabilityMeta::new(
            "nomi_computer_right_click",
            "computer",
            "Right-click the element with the given `ref` (opens its context menu).",
            DangerTier::Write,
        ),
        right_click,
    ));
    out.push(Capability::new::<RefParams, _, _>(
        CapabilityMeta::new(
            "nomi_computer_double_click",
            "computer",
            "Double-click the element with the given `ref`.",
            DangerTier::Write,
        ),
        double_click,
    ));
    out.push(Capability::new::<SetValueParams, _, _>(
        CapabilityMeta::new(
            "nomi_computer_set_value",
            "computer",
            "Set the `text` value of the element with the given `ref` (good for text fields).",
            DangerTier::Write,
        ),
        set_value,
    ));
    out.push(Capability::new::<XyParams, _, _>(
        CapabilityMeta::new(
            "nomi_computer_click_xy",
            "computer",
            "Left-click at pixel coordinates (`x`, `y`) of the most recent screenshot. Prefer click-by-ref when possible.",
            DangerTier::Write,
        ),
        click_xy,
    ));
    out.push(Capability::new::<TypeParams, _, _>(
        CapabilityMeta::new(
            "nomi_computer_type",
            "computer",
            "Type the `text` string into the focused control.",
            DangerTier::Write,
        ),
        type_text,
    ));
    out.push(Capability::new::<KeyParams, _, _>(
        CapabilityMeta::new(
            "nomi_computer_key",
            "computer",
            "Press a key or combo, e.g. \"enter\" or \"ctrl+a\".",
            DangerTier::Write,
        ),
        key,
    ));
    out.push(Capability::new::<ScrollParams, _, _>(
        CapabilityMeta::new(
            "nomi_computer_scroll",
            "computer",
            "Scroll in `direction` (up/down/left/right) by `amount` wheel clicks, optionally at (`x`, `y`).",
            DangerTier::Write,
        ),
        scroll,
    ));
    out.push(Capability::new::<LaunchParams, _, _>(
        CapabilityMeta::new(
            "nomi_computer_launch",
            "computer",
            "Open an application, URL, file, or folder via the OS shell. Always use this instead of shell `start`/`Start-Process`.",
            DangerTier::Write,
        ),
        launch,
    ));
    out.push(Capability::new::<NoParams, _, _>(
        CapabilityMeta::new(
            "nomi_computer_list_windows",
            "computer",
            "List open windows with ids, titles, positions and sizes.",
            DangerTier::Read,
        ),
        list_windows,
    ));
    out.push(Capability::new::<NoParams, _, _>(
        CapabilityMeta::new(
            "nomi_computer_cursor_position",
            "computer",
            "Report the mouse cursor position in screenshot coordinates.",
            DangerTier::Read,
        ),
        cursor_position,
    ));
    out.push(Capability::new::<WaitParams, _, _>(
        CapabilityMeta::new(
            "nomi_computer_wait",
            "computer",
            "Pause for `seconds` (max 5) to let the UI settle.",
            DangerTier::Read,
        ),
        wait,
    ));
}

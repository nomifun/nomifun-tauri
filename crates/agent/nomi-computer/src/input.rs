//! Input synthesis via enigo.
//!
//! Enigo handles are not `Send`, so each operation constructs a fresh Enigo
//! inside `tokio::task::spawn_blocking` and the whole blocking task is
//! wrapped in a 10s timeout. On macOS, Enigo's keyboard path queries Carbon
//! TIS/TSM input-source APIs, so the actual Enigo construction and operation
//! are synchronously dispatched to the main queue. Coordinates passed in here
//! are already absolute screen coordinates (mapped from screenshot space by
//! the caller).

use std::time::Duration;

use enigo::{Axis, Button, Direction, Enigo, Keyboard, Mouse, Settings};
// `Coordinate::Abs` is only used on the non-Windows actuation path; Windows
// moves the cursor via SendInput over the virtual desktop instead (see
// `move_abs`).
#[cfg(not(target_os = "windows"))]
use enigo::Coordinate;

use crate::permissions;

const INPUT_TIMEOUT: Duration = Duration::from_secs(10);

/// Pause between press and release (and between repeated clicks) so target
/// apps register distinct events.
const CLICK_PAUSE: Duration = Duration::from_millis(20);

/// Scroll direction accepted by the `scroll` action.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScrollDirection {
    Up,
    Down,
    Left,
    Right,
}

impl ScrollDirection {
    pub fn parse(s: &str) -> Result<Self, String> {
        match s {
            "up" => Ok(Self::Up),
            "down" => Ok(Self::Down),
            "left" => Ok(Self::Left),
            "right" => Ok(Self::Right),
            other => Err(format!(
                "Unknown scroll direction {other:?}. Use one of: up, down, left, right."
            )),
        }
    }
}

/// Map an absolute global virtual-desktop screen coordinate into the 0..=65535
/// normalized range that `SendInput` expects with
/// `MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK`, relative to the
/// virtual-screen rectangle `(v_left, v_top, v_width, v_height)` reported by
/// `GetSystemMetrics(SM_*VIRTUALSCREEN)`. The endpoints `v_left` and
/// `v_left + v_width - 1` map to 0 and 65535 respectively (round-to-nearest);
/// coordinates outside the desktop clamp into range.
///
/// This is the Windows-only fix for enigo 0.6's `Coordinate::Abs`, which
/// normalizes against the PRIMARY monitor (`GetSystemMetrics(SM_CXSCREEN)`) and
/// omits `MOUSEEVENTF_VIRTUALDESK`, so any target on a secondary monitor — or a
/// monitor whose virtual-desktop origin is negative/non-zero — is mis-projected
/// onto the primary display.
#[cfg(target_os = "windows")]
fn normalize_to_virtual_desktop(
    x: i32,
    y: i32,
    v_left: i32,
    v_top: i32,
    v_width: i32,
    v_height: i32,
) -> (i32, i32) {
    // Map [origin, origin + extent - 1] onto [0, 65535] (round-to-nearest),
    // clamped so out-of-desktop inputs never escape the range.
    fn axis(coord: i32, origin: i32, extent: i32) -> i32 {
        let span = (extent as i64) - 1;
        if span <= 0 {
            return 0;
        }
        let rel = (coord as i64 - origin as i64).max(0);
        let n = (rel * 65535 + span / 2) / span;
        n.clamp(0, 65535) as i32
    }
    (axis(x, v_left, v_width), axis(y, v_top, v_height))
}


/// Move the cursor to an absolute global screen coordinate (the space produced
/// by `to_screen()` / xcap monitor origins).
///
/// On Windows we bypass enigo's `Coordinate::Abs` — it normalizes against the
/// primary monitor only and omits `MOUSEEVENTF_VIRTUALDESK`, so multi-monitor
/// and negative/non-zero-origin targets land on the wrong display — and emit a
/// `SendInput` move across the whole virtual desktop. On macOS / Linux enigo
/// already actuates global coordinates correctly, so its path is unchanged.
fn move_abs(enigo: &mut Enigo, x: i32, y: i32) -> Result<(), String> {
    #[cfg(target_os = "windows")]
    {
        let _ = enigo;
        move_abs_windows(x, y)
    }
    #[cfg(not(target_os = "windows"))]
    {
        enigo.move_mouse(x, y, Coordinate::Abs).map_err(input_err)
    }
}

/// Windows absolute cursor move over the entire virtual desktop via `SendInput`
/// (`MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK`),
/// normalized against `GetSystemMetrics(SM_*VIRTUALSCREEN)`.
#[cfg(target_os = "windows")]
fn move_abs_windows(x: i32, y: i32) -> Result<(), String> {
    use windows::Win32::UI::Input::KeyboardAndMouse::{
        INPUT, INPUT_0, INPUT_MOUSE, MOUSEEVENTF_ABSOLUTE, MOUSEEVENTF_MOVE,
        MOUSEEVENTF_VIRTUALDESK, MOUSEINPUT, SendInput,
    };
    use windows::Win32::UI::WindowsAndMessaging::{
        GetSystemMetrics, SM_CXVIRTUALSCREEN, SM_CYVIRTUALSCREEN, SM_XVIRTUALSCREEN,
        SM_YVIRTUALSCREEN,
    };

    // SAFETY: GetSystemMetrics reads global display metrics; no preconditions.
    let (v_left, v_top, v_width, v_height) = unsafe {
        (
            GetSystemMetrics(SM_XVIRTUALSCREEN),
            GetSystemMetrics(SM_YVIRTUALSCREEN),
            GetSystemMetrics(SM_CXVIRTUALSCREEN),
            GetSystemMetrics(SM_CYVIRTUALSCREEN),
        )
    };
    if v_width <= 0 || v_height <= 0 {
        return Err(
            "Could not read the Windows virtual-screen dimensions for absolute \
             cursor positioning."
                .to_string(),
        );
    }
    let (nx, ny) = normalize_to_virtual_desktop(x, y, v_left, v_top, v_width, v_height);
    let input = INPUT {
        r#type: INPUT_MOUSE,
        Anonymous: INPUT_0 {
            mi: MOUSEINPUT {
                dx: nx,
                dy: ny,
                mouseData: 0,
                dwFlags: MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK,
                time: 0,
                dwExtraInfo: 0,
            },
        },
    };
    // SAFETY: a single well-formed INPUT value; cbsize matches its size.
    let sent = unsafe { SendInput(&[input], std::mem::size_of::<INPUT>() as i32) };
    if sent == 1 {
        Ok(())
    } else {
        Err(
            "Windows refused the synthetic mouse move (SendInput inserted no \
             events; input may be blocked by a higher-integrity window or the \
             secure desktop)."
                .to_string(),
        )
    }
}

fn new_enigo() -> Result<Enigo, String> {
    let settings = Settings {
        // Never block the agent on an interactive permission prompt.
        open_prompt_to_get_permissions: false,
        ..Settings::default()
    };
    Enigo::new(&settings).map_err(|e| {
        format!(
            "Failed to initialize input synthesis: {e}. {}",
            permissions::accessibility_hint_detailed()
        )
    })
}

fn run_input_task_blocking<T, F>(task: F) -> Result<T, String>
where
    T: Send + 'static,
    F: FnOnce() -> Result<T, String> + Send + 'static,
{
    crate::macos_main::run_blocking(task)
}

fn run_enigo_operation_blocking<T, F>(op: F) -> Result<T, String>
where
    T: Send + 'static,
    F: FnOnce(&mut Enigo) -> Result<T, String> + Send + 'static,
{
    run_input_task_blocking(move || {
        let mut enigo = new_enigo()?;
        op(&mut enigo)
    })
}

/// Run an input operation on a fresh Enigo instance inside spawn_blocking,
/// bounded by a 10s timeout.
async fn with_enigo<T, F>(op: F) -> Result<T, String>
where
    T: Send + 'static,
    F: FnOnce(&mut Enigo) -> Result<T, String> + Send + 'static,
{
    let handle = tokio::task::spawn_blocking(move || run_enigo_operation_blocking(op));
    match tokio::time::timeout(INPUT_TIMEOUT, handle).await {
        Ok(Ok(result)) => result,
        Ok(Err(join_err)) => Err(format!("Input task failed: {join_err}")),
        Err(_) => Err(format!(
            "Input operation timed out after {}s. The system may be blocking \
             synthetic input. {}",
            INPUT_TIMEOUT.as_secs(),
            permissions::accessibility_hint_detailed()
        )),
    }
}

fn input_err(e: enigo::InputError) -> String {
    format!(
        "Input synthesis failed: {e}. {}",
        permissions::accessibility_hint()
    )
}

/// Move the cursor to absolute screen coordinates.
pub async fn mouse_move(x: i32, y: i32) -> Result<(), String> {
    with_enigo(move |enigo| move_abs(enigo, x, y)).await
}

/// Click `button` `count` times at absolute screen coordinates.
pub async fn click(x: i32, y: i32, button: Button, count: u32) -> Result<(), String> {
    with_enigo(move |enigo| {
        move_abs(enigo, x, y)?;
        for i in 0..count {
            if i > 0 {
                std::thread::sleep(CLICK_PAUSE);
            }
            enigo.button(button, Direction::Click).map_err(input_err)?;
        }
        Ok(())
    })
    .await
}

/// Press at (start), drag to (end), release. Includes intermediate moves so
/// apps that track motion register the drag.
pub async fn drag(start_x: i32, start_y: i32, end_x: i32, end_y: i32) -> Result<(), String> {
    with_enigo(move |enigo| {
        move_abs(enigo, start_x, start_y)?;
        enigo
            .button(Button::Left, Direction::Press)
            .map_err(input_err)?;
        std::thread::sleep(CLICK_PAUSE);
        // A few intermediate steps make drags more reliable than a teleport.
        const STEPS: i32 = 8;
        for i in 1..=STEPS {
            let ix = start_x + (end_x - start_x) * i / STEPS;
            let iy = start_y + (end_y - start_y) * i / STEPS;
            move_abs(enigo, ix, iy)?;
            std::thread::sleep(Duration::from_millis(10));
        }
        enigo
            .button(Button::Left, Direction::Release)
            .map_err(input_err)?;
        Ok(())
    })
    .await
}

/// Type a unicode string (layout-independent).
pub async fn type_text(text: String) -> Result<(), String> {
    with_enigo(move |enigo| enigo.text(&text).map_err(input_err)).await
}

/// Press a key combo: press front-to-back, release back-to-front.
pub async fn key_combo(keys: Vec<enigo::Key>) -> Result<(), String> {
    with_enigo(move |enigo| {
        let mut pressed: Vec<enigo::Key> = Vec::with_capacity(keys.len());
        for key in &keys {
            if let Err(e) = enigo.key(*key, Direction::Press) {
                // Release anything already held before bailing out.
                for held in pressed.iter().rev() {
                    let _ = enigo.key(*held, Direction::Release);
                }
                return Err(input_err(e));
            }
            pressed.push(*key);
        }
        std::thread::sleep(CLICK_PAUSE);
        let mut result = Ok(());
        for key in pressed.iter().rev() {
            if let Err(e) = enigo.key(*key, Direction::Release) {
                result = Err(input_err(e));
            }
        }
        result
    })
    .await
}

/// Scroll by `amount` wheel clicks in `direction` (optionally moving the
/// cursor to (x, y) first so the scroll lands on the right surface).
pub async fn scroll(
    at: Option<(i32, i32)>,
    direction: ScrollDirection,
    amount: i32,
) -> Result<(), String> {
    with_enigo(move |enigo| {
        if let Some((x, y)) = at {
            move_abs(enigo, x, y)?;
        }
        let (axis, length) = match direction {
            ScrollDirection::Up => (Axis::Vertical, -amount),
            ScrollDirection::Down => (Axis::Vertical, amount),
            ScrollDirection::Left => (Axis::Horizontal, -amount),
            ScrollDirection::Right => (Axis::Horizontal, amount),
        };
        enigo.scroll(length, axis).map_err(input_err)
    })
    .await
}

/// Current cursor location in absolute screen coordinates.
pub async fn cursor_position() -> Result<(i32, i32), String> {
    with_enigo(|enigo| enigo.location().map_err(input_err)).await
}

/// Size (width, height) of the main display in enigo's coordinate system.
/// Blocking variant for use inside other spawn_blocking sections.
pub fn main_display_size_blocking() -> Result<(i32, i32), String> {
    run_enigo_operation_blocking(|enigo| enigo.main_display().map_err(input_err))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scroll_direction_parses_all_variants() {
        assert_eq!(ScrollDirection::parse("up").unwrap(), ScrollDirection::Up);
        assert_eq!(
            ScrollDirection::parse("down").unwrap(),
            ScrollDirection::Down
        );
        assert_eq!(
            ScrollDirection::parse("left").unwrap(),
            ScrollDirection::Left
        );
        assert_eq!(
            ScrollDirection::parse("right").unwrap(),
            ScrollDirection::Right
        );
    }

    #[test]
    fn scroll_direction_unknown_is_error() {
        let err = ScrollDirection::parse("diagonal").unwrap_err();
        assert!(err.contains("diagonal"));
    }

    // --- virtual-desktop coordinate normalization (Windows actuation fix) ---

    #[cfg(target_os = "windows")]
    #[test]
    fn vd_center_of_single_primary_maps_to_midrange() {
        // One 1920x1080 monitor at the virtual-desktop origin.
        let (nx, ny) = normalize_to_virtual_desktop(960, 540, 0, 0, 1920, 1080);
        assert!((32000..=33500).contains(&nx), "nx={nx}");
        assert!((32000..=33500).contains(&ny), "ny={ny}");
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn vd_point_on_secondary_monitor_maps_to_upper_range() {
        // Two 1920-wide monitors side by side; target is the centre of the RIGHT
        // (secondary) monitor. The fixed mapping must land in the upper half of
        // the 0..65535 range — enigo's primary-only normalization would divide
        // 2880 by the primary width (1920) and overflow past 65535 onto the
        // primary display.
        let (nx, _) = normalize_to_virtual_desktop(2880, 540, 0, 0, 3840, 1080);
        assert!(nx > 40000 && nx <= 65535, "nx={nx}");
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn vd_point_on_negative_origin_monitor_maps_to_lower_range() {
        // A monitor to the LEFT of the primary (negative virtual-desktop origin).
        // Target is the centre of that left monitor; it must map to the lower
        // half — enigo would produce a negative normalized value (off-screen).
        let (nx, _) = normalize_to_virtual_desktop(-960, 540, -1920, 0, 3840, 1080);
        assert!(nx > 10000 && nx < 25000, "nx={nx}");
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn vd_endpoints_map_to_full_range() {
        // Left/top edge -> 0, right/bottom edge -> 65535, with a non-zero origin.
        assert_eq!(normalize_to_virtual_desktop(100, 50, 100, 50, 1920, 1080).0, 0);
        assert_eq!(
            normalize_to_virtual_desktop(100 + 1920 - 1, 50, 100, 50, 1920, 1080).0,
            65535
        );
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn vd_out_of_desktop_clamps() {
        // Beyond the right/below the left edge -> clamped, never out of [0,65535].
        assert_eq!(normalize_to_virtual_desktop(99999, 0, 0, 0, 1920, 1080).0, 65535);
        assert_eq!(normalize_to_virtual_desktop(-99999, 0, 0, 0, 1920, 1080).0, 0);
    }

    // Requires a real input device and (on macOS) Accessibility permission.
    #[tokio::test]
    #[ignore]
    async fn cursor_position_real() {
        let (x, y) = cursor_position().await.expect("should read cursor");
        assert!(x >= -20_000 && x <= 20_000);
        assert!(y >= -20_000 && y <= 20_000);
    }

    #[tokio::test]
    #[ignore]
    async fn mouse_move_real() {
        mouse_move(10, 10).await.expect("should move cursor");
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_input_task_runs_inside_dispatcher() {
        use std::sync::{Arc, Mutex};

        let caller_thread = std::thread::current().id();
        let dispatcher_thread = Arc::new(Mutex::new(None));
        let work_thread = Arc::new(Mutex::new(None));
        let dispatcher_thread_seen = dispatcher_thread.clone();
        let work_thread_seen = work_thread.clone();

        let result = crate::macos_main::run_task_with(
            move |task| {
                *dispatcher_thread_seen.lock().unwrap() = Some(std::thread::current().id());
                let handle = std::thread::spawn(task);
                handle.join().expect("dispatched task should not panic")
            },
            move || {
                *work_thread_seen.lock().unwrap() = Some(std::thread::current().id());
                Ok("ok")
            },
        )
        .expect("task should complete");

        assert_eq!(result, "ok");
        assert_eq!(dispatcher_thread.lock().unwrap().unwrap(), caller_thread);
        assert_ne!(
            work_thread.lock().unwrap().unwrap(),
            caller_thread,
            "work must run inside the dispatcher task, not on the caller thread"
        );
    }
}

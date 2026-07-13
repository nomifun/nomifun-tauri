//! Screen capture via xcap, downscaled and PNG/base64-encoded for the LLM.
//!
//! Coordinate systems: `Monitor::width()/height()` report the size enigo's
//! absolute mouse coordinates use (logical points on macOS, device pixels on
//! Windows/Linux), while `capture_image()` returns physical pixels (2x on
//! Retina). `CaptureGeometry` records both so clicks can be mapped back.

use base64::Engine as _;
use xcap::Monitor;

use nomi_types::tool::ToolImage;

use crate::permissions;
use crate::scale::fit_within;

/// Geometry of the most recent capture, used to map LLM (screenshot-pixel)
/// coordinates to absolute screen coordinates.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CaptureGeometry {
    /// Dimensions of the image the LLM sees (after downscaling).
    pub img_w: u32,
    pub img_h: u32,
    /// Monitor size in the coordinate system input synthesis uses.
    pub logical_w: u32,
    pub logical_h: u32,
    /// Monitor origin in the global (multi-display) coordinate space.
    pub origin_x: i32,
    pub origin_y: i32,
}

/// A completed screen capture. `image` is the downscaled RGBA frame (not yet
/// encoded) so callers can draw a Set-of-Marks overlay before encoding; use
/// `encode_png` to produce the `ToolImage`.
#[derive(Debug)]
pub struct CapturedScreen {
    pub image: image::RgbaImage,
    pub geometry: CaptureGeometry,
    /// Raw captured frame size in physical pixels (before downscaling).
    pub physical_w: u32,
    pub physical_h: u32,
    /// Index of the captured monitor within `Monitor::all()`.
    pub display_index: usize,
}

const MAX_SCREENSHOT_PNG_BYTES: usize = 5 * 1024 * 1024;
const MIN_SCREENSHOT_LONG_EDGE: u32 = 320;
const MAX_ENCODE_ATTEMPTS: usize = 12;

/// Encoded screenshot and the exact pixel geometry sent to the model.
#[derive(Debug)]
pub struct EncodedPng {
    pub image: ToolImage,
    pub width: u32,
    pub height: u32,
}

fn png_bytes(img: &image::RgbaImage) -> Result<Vec<u8>, String> {
    let mut png = Vec::new();
    image::DynamicImage::ImageRgba8(img.clone())
        .write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)
        .map_err(|e| format!("Failed to encode screenshot as PNG: {e}"))?;
    Ok(png)
}

/// Encode a (possibly overlay-annotated) RGBA frame as a byte-bounded PNG.
/// High-entropy or unusually large frames are downscaled deterministically so
/// native Computer screenshots can never exceed the shared 5 MiB image limit.
pub fn encode_png(img: &image::RgbaImage) -> Result<EncodedPng, String> {
    if img.width() == 0 || img.height() == 0 {
        return Err("Cannot encode an empty screenshot".to_owned());
    }

    let mut working = img.clone();
    for _ in 0..MAX_ENCODE_ATTEMPTS {
        let png = png_bytes(&working)?;
        if png.len() <= MAX_SCREENSHOT_PNG_BYTES {
            return Ok(EncodedPng {
                width: working.width(),
                height: working.height(),
                image: ToolImage {
                    media_type: "image/png".to_string(),
                    data: base64::engine::general_purpose::STANDARD.encode(&png),
                },
            });
        }

        let old_w = working.width();
        let old_h = working.height();
        if old_w.max(old_h) <= MIN_SCREENSHOT_LONG_EDGE {
            break;
        }
        let ratio = ((MAX_SCREENSHOT_PNG_BYTES as f64 / png.len() as f64).sqrt() * 0.92)
            .clamp(0.1, 0.9);
        let min_ratio = MIN_SCREENSHOT_LONG_EDGE as f64 / old_w.max(old_h) as f64;
        let ratio = ratio.max(min_ratio.min(0.9));
        let new_w = ((old_w as f64 * ratio).floor() as u32).max(1);
        let new_h = ((old_h as f64 * ratio).floor() as u32).max(1);
        if (new_w, new_h) == (old_w, old_h) {
            break;
        }
        working = image::imageops::resize(
            &working,
            new_w,
            new_h,
            image::imageops::FilterType::Triangle,
        );
    }

    Err(format!(
        "Screenshot could not be reduced below the {} MiB provider image limit",
        MAX_SCREENSHOT_PNG_BYTES / (1024 * 1024)
    ))
}

/// Pick the monitor to capture: explicit index, else the primary, else the
/// first one listed.
fn select_monitor(monitors: &[Monitor], display: Option<usize>) -> Result<usize, String> {
    if monitors.is_empty() {
        return Err(format!(
            "No displays found. {}",
            permissions::screen_capture_hint_detailed()
        ));
    }
    match display {
        Some(idx) => {
            if idx < monitors.len() {
                Ok(idx)
            } else {
                Err(format!(
                    "Display {idx} does not exist; {} display(s) available (0-{}).",
                    monitors.len(),
                    monitors.len() - 1
                ))
            }
        }
        None => Ok(monitors
            .iter()
            .position(|m| m.is_primary().unwrap_or(false))
            .unwrap_or(0)),
    }
}

/// Capture a monitor, downscale to `max_edge`, and encode as base64 PNG.
/// Blocking: call from `spawn_blocking`. On macOS, xcap enumerates displays
/// through AppKit (`NSScreen`), so the native capture work is dispatched to the
/// main queue before it touches xcap.
pub fn capture_screen(display: Option<usize>, max_edge: u32) -> Result<CapturedScreen, String> {
    crate::macos_main::run_blocking(move || capture_screen_inner(display, max_edge))
}

fn capture_screen_inner(display: Option<usize>, max_edge: u32) -> Result<CapturedScreen, String> {
    // Proactive + authoritative on macOS: a denied Screen Recording grant lets
    // capture "succeed" with a black frame, so fail fast with a clear message
    // instead of relying solely on the downstream black-frame heuristic.
    if permissions::screen_recording_granted() == Some(false) {
        return Err(format!(
            "Screen Recording permission is not granted, so the screen cannot be captured. {}",
            permissions::screen_capture_hint_detailed()
        ));
    }
    let monitors = Monitor::all().map_err(|e| {
        format!(
            "Failed to enumerate displays: {e}. {}",
            permissions::screen_capture_hint_detailed()
        )
    })?;
    let display_index = select_monitor(&monitors, display)?;
    let monitor = &monitors[display_index];

    let frame = monitor.capture_image().map_err(|e| {
        format!(
            "Failed to capture the screen: {e}. {}",
            permissions::screen_capture_hint_detailed()
        )
    })?;
    permissions::screenshot_permission_check(&frame)?;

    let (physical_w, physical_h) = frame.dimensions();
    if physical_w == 0 || physical_h == 0 {
        return Err(format!(
            "Capture returned an empty frame. {}",
            permissions::screen_capture_hint_detailed()
        ));
    }

    let (img_w, img_h) = fit_within(physical_w, physical_h, max_edge);
    let scaled = if (img_w, img_h) == (physical_w, physical_h) {
        frame
    } else {
        image::imageops::resize(&frame, img_w, img_h, image::imageops::FilterType::Triangle)
    };

    let logical_w = monitor.width().unwrap_or(physical_w);
    let logical_h = monitor.height().unwrap_or(physical_h);
    let origin_x = monitor.x().unwrap_or(0);
    let origin_y = monitor.y().unwrap_or(0);

    Ok(CapturedScreen {
        image: scaled,
        geometry: CaptureGeometry {
            img_w,
            img_h,
            logical_w,
            logical_h,
            origin_x,
            origin_y,
        },
        physical_w,
        physical_h,
        display_index,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    // Requires a real display and (on macOS) Screen Recording permission.
    #[test]
    #[ignore]
    fn capture_primary_screen_real() {
        let captured = capture_screen(None, 1568).expect("capture should succeed");
        assert!(captured.image.width() > 0 && captured.image.height() > 0);
        let encoded = encode_png(&captured.image).expect("encode should succeed");
        assert_eq!(encoded.image.media_type, "image/png");
        assert!(!encoded.image.data.is_empty());
        assert!(captured.geometry.img_w.max(captured.geometry.img_h) <= 1568);
        assert!(captured.physical_w >= captured.geometry.img_w);
    }

    #[test]
    #[ignore]
    fn capture_invalid_display_errors_real() {
        let err = capture_screen(Some(99), 1568).unwrap_err();
        assert!(err.contains("99"), "error should name the display: {err}");
    }

    #[test]
    fn encode_png_bounds_high_entropy_screenshot_payload() {
        let img = image::RgbaImage::from_fn(3_000, 2_000, |x, y| {
            let mut value = x
                .wrapping_mul(0x9e37_79b9)
                .wrapping_add(y.wrapping_mul(0x85eb_ca6b));
            value ^= value >> 16;
            value = value.wrapping_mul(0x7feb_352d);
            value ^= value >> 15;
            image::Rgba([
                value as u8,
                (value >> 8) as u8,
                (value >> 16) as u8,
                255,
            ])
        });

        let encoded = encode_png(&img).expect("encode should succeed");

        const MAX_BASE64_BYTES: usize = (5 * 1024 * 1024_usize).div_ceil(3) * 4;
        assert!(encoded.image.data.len() <= MAX_BASE64_BYTES);
        assert!(encoded.width > 0 && encoded.height > 0);
        assert!(encoded.width < img.width() || encoded.height < img.height());
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_capture_task_runs_inside_dispatcher() {
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

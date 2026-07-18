// Copyright 2025-2026 NomiFun (nomifun.com)
// SPDX-License-Identifier: Apache-2.0

use serde::Serialize;

#[derive(Debug, Clone, PartialEq, Serialize)]
#[cfg_attr(any(target_os = "macos", windows), allow(dead_code))]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum CompanionLocalPointer {
    Point {
        backend: &'static str,
        #[serde(rename = "xRatio")]
        x_ratio: f64,
        #[serde(rename = "yRatio")]
        y_ratio: f64,
    },
    Unsupported {
        backend: &'static str,
    },
}

#[cfg(any(test, target_os = "linux"))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PointerBackend {
    AppKit,
    Win32,
    X11,
    Wayland,
    Other,
}

fn is_companion_label(label: &str) -> bool {
    label
        .strip_prefix("companion-")
        .is_some_and(|id| !id.is_empty())
}

#[cfg(any(test, target_os = "linux"))]
fn backend_supports_reentry(backend: PointerBackend) -> bool {
    matches!(
        backend,
        PointerBackend::AppKit | PointerBackend::Win32 | PointerBackend::X11
    )
}

fn normalize_local_point(
    backend: &'static str,
    x: f64,
    y: f64,
    width: f64,
    height: f64,
) -> Result<CompanionLocalPointer, String> {
    if !x.is_finite()
        || !y.is_finite()
        || !width.is_finite()
        || !height.is_finite()
        || width <= 0.0
        || height <= 0.0
    {
        return Err("invalid companion pointer geometry".to_string());
    }

    Ok(CompanionLocalPointer::Point {
        backend,
        x_ratio: x / width,
        y_ratio: y / height,
    })
}

fn top_left_view_y(view_y: f64, bounds_y: f64, height: f64, flipped: bool) -> f64 {
    if flipped {
        view_y - bounds_y
    } else {
        bounds_y + height - view_y
    }
}

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
        top_left_view_y(
            view_point.y,
            bounds.origin.y,
            bounds.size.height,
            view.isFlipped(),
        ),
        bounds.size.width,
        bounds.size.height,
    )
}

#[cfg(windows)]
fn sample_native(window: &tauri::WebviewWindow) -> Result<CompanionLocalPointer, String> {
    use windows::Win32::Foundation::{POINT, RECT};
    use windows::Win32::UI::WindowsAndMessaging::{
        GetClientRect, GetCursorPos, ScreenToClient,
    };

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
    use gtk::prelude::*;

    let display = window
        .gtk_window()
        .map_err(|error| error.to_string())?
        .display();
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
    use gtk::prelude::*;

    match linux_backend(window)? {
        PointerBackend::Wayland => {
            return Ok(CompanionLocalPointer::Unsupported {
                backend: "wayland",
            });
        }
        PointerBackend::Other => {
            return Ok(CompanionLocalPointer::Unsupported { backend: "other" });
        }
        PointerBackend::X11 => {}
        PointerBackend::AppKit | PointerBackend::Win32 => unreachable!(),
    }

    let top = window.gtk_window().map_err(|error| error.to_string())?;
    let display = top.display();
    let surface = top
        .window()
        .ok_or_else(|| "companion GTK window is not realized".to_string())?;
    let pointer = display
        .default_seat()
        .and_then(|seat| seat.pointer())
        .ok_or_else(|| "Linux display has no pointer device".to_string())?;
    let (_, x, y, _) = surface.device_position_double(&pointer);

    normalize_local_point(
        "x11",
        x,
        y,
        f64::from(surface.width()),
        f64::from(surface.height()),
    )
}

#[cfg(not(any(target_os = "macos", windows, target_os = "linux")))]
fn sample_native(_window: &tauri::WebviewWindow) -> Result<CompanionLocalPointer, String> {
    Ok(CompanionLocalPointer::Unsupported { backend: "other" })
}

#[cfg(any(target_os = "macos", windows))]
pub fn supports_initial_pointer_passthrough(_window: &tauri::WebviewWindow) -> bool {
    true
}

#[cfg(target_os = "linux")]
pub fn supports_initial_pointer_passthrough(window: &tauri::WebviewWindow) -> bool {
    linux_backend(window).is_ok_and(backend_supports_reentry)
}

#[cfg(not(any(target_os = "macos", windows, target_os = "linux")))]
pub fn supports_initial_pointer_passthrough(_window: &tauri::WebviewWindow) -> bool {
    false
}

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
        assert_eq!(
            normalize_local_point("x11", -20.0, 240.0, 200.0, 120.0).unwrap(),
            CompanionLocalPointer::Point {
                backend: "x11",
                x_ratio: -0.1,
                y_ratio: 2.0,
            }
        );
    }

    #[test]
    fn normalization_rejects_invalid_geometry() {
        for input in [
            (f64::NAN, 1.0, 10.0, 10.0),
            (1.0, f64::INFINITY, 10.0, 10.0),
            (1.0, 1.0, 0.0, 10.0),
            (1.0, 1.0, 10.0, -1.0),
        ] {
            assert!(
                normalize_local_point("appkit", input.0, input.1, input.2, input.3)
                    .is_err()
            );
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

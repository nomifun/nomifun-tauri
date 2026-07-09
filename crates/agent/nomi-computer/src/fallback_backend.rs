//! Platform-neutral fallback backend: window enumeration and best-effort
//! focusing via xcap (cross-platform). This is NOT the Windows-OS backend —
//! the file was historically named `windows.rs`, which collided conceptually
//! with the future per-OS UI-Automation submodule; it is renamed to make clear
//! it is the generic xcap/enigo fallback used on every platform.
//!
//! xcap 0.9 has no focus/activate API on any platform, so `focus_window`
//! falls back to clicking the window's center (which raises and focuses it
//! on macOS and most Linux WMs). Window x/y/width/height from xcap are in
//! the same logical coordinate space enigo uses on macOS.
//!
//! Real window activation (macOS NSRunningApplication / Windows UIA SetFocus /
//! Linux per-WM) is designed in the cross-platform computer-use spec and will
//! replace the click-to-raise fallback as the a11y engine lands.

use xcap::Window;

// The click-to-raise focus fallback (and only that) uses synthetic input; on
// Windows we activate the real window via Win32 instead, so the input crate is
// not referenced there.
#[cfg(not(target_os = "windows"))]
use crate::input;

/// A snapshot of one window's metadata.
#[derive(Debug, Clone)]
pub struct WindowInfo {
    pub id: u32,
    pub title: String,
    pub app_name: String,
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
    pub is_focused: bool,
}

/// Enumerate windows (front-to-back z order). Blocking: call from
/// spawn_blocking. On macOS, xcap window metadata checks can touch AppKit
/// (`NSWorkspace`), so enumeration is dispatched to the main queue.
pub fn list_windows() -> Result<Vec<WindowInfo>, String> {
    crate::macos_main::run_blocking(list_windows_inner)
}

fn list_windows_inner() -> Result<Vec<WindowInfo>, String> {
    let windows = Window::all().map_err(|e| format!("Failed to enumerate windows: {e}"))?;
    let mut infos = Vec::with_capacity(windows.len());
    for w in &windows {
        // Skip windows whose metadata cannot be read instead of failing the
        // whole listing.
        let Ok(id) = w.id() else { continue };
        infos.push(WindowInfo {
            id,
            title: w.title().unwrap_or_default(),
            app_name: w.app_name().unwrap_or_default(),
            x: w.x().unwrap_or(0),
            y: w.y().unwrap_or(0),
            width: w.width().unwrap_or(0),
            height: w.height().unwrap_or(0),
            is_focused: w.is_focused().unwrap_or(false),
        });
    }
    Ok(infos)
}

/// Render a window list as human-readable text for the LLM.
pub fn format_window_list(windows: &[WindowInfo]) -> String {
    if windows.is_empty() {
        return "No windows found.".to_string();
    }
    let mut out = String::from("Windows (front to back):\n");
    for w in windows {
        let focus = if w.is_focused { " [focused]" } else { "" };
        out.push_str(&format!(
            "- id={} app={:?} title={:?} at ({}, {}) size {}x{}{}\n",
            w.id, w.app_name, w.title, w.x, w.y, w.width, w.height, focus
        ));
    }
    out
}

/// Find a window by id. Blocking: call from spawn_blocking.
pub fn find_window(window_id: u32) -> Result<WindowInfo, String> {
    let windows = list_windows()?;
    windows
        .into_iter()
        .find(|w| w.id == window_id)
        .ok_or_else(|| {
            format!(
                "Window {window_id} not found. Use the list_windows action to get current ids."
            )
        })
}

/// Bring a window to the foreground.
///
/// On Windows this uses the real activation API (`SetForegroundWindow` on the
/// exact target HWND, restoring it if minimized, with a foreground-lock
/// workaround) so subsequent `type`/`key` input lands in the intended window.
/// On macOS / Linux, where xcap exposes no activate API and a center click
/// reliably raises and focuses the window, it falls back to click-to-raise.
pub async fn focus_window(window_id: u32) -> Result<String, String> {
    let info = tokio::task::spawn_blocking(move || find_window(window_id))
        .await
        .map_err(|e| format!("Window lookup task failed: {e}"))??;

    #[cfg(target_os = "windows")]
    {
        tokio::task::spawn_blocking(move || set_foreground_window(window_id))
            .await
            .map_err(|e| format!("Focus task failed: {e}"))??;
        Ok(format!(
            "Activated window {window_id} ({:?} — {:?}) via the Windows foreground API. \
             Take a screenshot to verify.",
            info.app_name, info.title
        ))
    }

    #[cfg(not(target_os = "windows"))]
    {
        if info.width == 0 || info.height == 0 {
            return Err(format!(
                "Window {window_id} ({:?}) has zero size; it may be minimized or hidden. \
                 Cannot focus it by clicking.",
                info.title
            ));
        }

        let cx = info.x + info.width as i32 / 2;
        let cy = info.y + info.height as i32 / 2;
        input::click(cx, cy, enigo::Button::Left, 1).await?;
        Ok(format!(
            "Clicked the center of window {window_id} ({:?} — {:?}) at ({cx}, {cy}) to focus it. \
             Note: the platform exposes no direct focus API, so this is a click-to-raise fallback; \
             the click may interact with whatever is at the window center. \
             Take a screenshot to verify the result.",
            info.app_name, info.title
        ))
    }
}

/// Windows: activate the exact target window (xcap window ids are HWNDs).
/// Restores it if minimized, briefly attaches our input queue to the current
/// foreground thread so the OS honors the activation (the foreground-stealing
/// lock otherwise silently no-ops), and reports a clear error if Windows still
/// refuses (e.g. an elevated/higher-integrity target).
#[cfg(target_os = "windows")]
fn set_foreground_window(window_id: u32) -> Result<(), String> {
    use core::ffi::c_void;
    use windows::Win32::Foundation::HWND;
    use windows::Win32::System::Threading::{AttachThreadInput, GetCurrentThreadId};
    use windows::Win32::UI::WindowsAndMessaging::{
        GetForegroundWindow, GetWindowThreadProcessId, IsIconic, IsWindow, SW_RESTORE, SW_SHOW,
        SetForegroundWindow, ShowWindow,
    };

    let hwnd = HWND(window_id as usize as *mut c_void);
    // SAFETY: window-management calls on a possibly-stale handle. `IsWindow`
    // guards validity first, and every call below fails (rather than UB) on an
    // invalid HWND.
    unsafe {
        if !IsWindow(Some(hwnd)).as_bool() {
            return Err(format!(
                "Window {window_id} no longer exists. Use list_windows to get current ids."
            ));
        }
        if IsIconic(hwnd).as_bool() {
            let _ = ShowWindow(hwnd, SW_RESTORE);
        } else {
            let _ = ShowWindow(hwnd, SW_SHOW);
        }
        // Windows only honors SetForegroundWindow from the thread owning the
        // current foreground window, so briefly share its input state.
        let cur = GetCurrentThreadId();
        let fg = GetForegroundWindow();
        let fg_tid = if fg.0.is_null() {
            0
        } else {
            GetWindowThreadProcessId(fg, None)
        };
        let attached =
            fg_tid != 0 && fg_tid != cur && AttachThreadInput(cur, fg_tid, true).as_bool();
        let ok = SetForegroundWindow(hwnd).as_bool();
        if attached {
            let _ = AttachThreadInput(cur, fg_tid, false);
        }
        if ok {
            Ok(())
        } else {
            Err(format!(
                "Windows refused to bring window {window_id} to the foreground (foreground \
                 activation is OS-restricted; the target may be elevated / higher-integrity than \
                 this app, or another app holds the foreground lock). Try a pixel click instead."
            ))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_empty_window_list() {
        assert_eq!(format_window_list(&[]), "No windows found.");
    }

    #[test]
    fn format_window_list_includes_fields() {
        let windows = vec![WindowInfo {
            id: 42,
            title: "Inbox".to_string(),
            app_name: "Mail".to_string(),
            x: 10,
            y: 20,
            width: 800,
            height: 600,
            is_focused: true,
        }];
        let text = format_window_list(&windows);
        assert!(text.contains("id=42"));
        assert!(text.contains("Mail"));
        assert!(text.contains("Inbox"));
        assert!(text.contains("800x600"));
        assert!(text.contains("[focused]"));
    }

    // Requires a real window server session.
    #[test]
    #[ignore]
    fn list_windows_real() {
        let windows = list_windows().expect("should enumerate windows");
        // There is at least a desktop-level window in a real session.
        assert!(!windows.is_empty());
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_window_task_runs_inside_dispatcher() {
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

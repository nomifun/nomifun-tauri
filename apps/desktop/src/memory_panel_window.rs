use std::collections::HashSet;
use std::sync::{Arc, Mutex};

use nomifun_app::DesktopServer;
use tauri::{Manager, PhysicalPosition, PhysicalSize};

use crate::{run_on_main_thread_task, webui_init_script};

pub const MEMORY_PANEL_LABEL: &str = "nomi-memory-panel";
const MAX_PANEL_DIMENSION: u32 = 4096;

#[derive(Clone, Copy, Debug, serde::Deserialize, PartialEq, Eq)]
pub struct PhysicalRect {
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
}

impl PhysicalRect {
    pub fn new(x: i32, y: i32, width: u32, height: u32) -> Self {
        Self { x, y, width, height }
    }

    pub fn validate(self) -> Result<Self, String> {
        if self.width == 0 || self.height == 0 || self.width > MAX_PANEL_DIMENSION || self.height > MAX_PANEL_DIMENSION {
            return Err("invalid memory panel rectangle".to_string());
        }
        Ok(self)
    }
}

#[derive(Clone, Debug, Default)]
struct MemoryPanelSession {
    request_id: Option<String>,
    owner_companion_id: Option<String>,
    rect: Option<PhysicalRect>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MemoryPanelSessionSnapshot {
    pub request_id: Option<String>,
    pub owner_companion_id: Option<String>,
    pub rect: Option<PhysicalRect>,
}

#[derive(Clone, Default)]
pub struct MemoryPanelWindowState(Arc<Mutex<MemoryPanelSession>>);

impl MemoryPanelWindowState {
    pub fn place(&self, request_id: &str, owner_companion_id: &str, rect: PhysicalRect) -> Result<(), String> {
        if request_id.trim().is_empty() || owner_companion_id.trim().is_empty() {
            return Err("memory panel request and owner are required".to_string());
        }
        let rect = rect.validate()?;
        let mut state = self.0.lock().map_err(|_| "memory panel state poisoned".to_string())?;
        state.request_id = Some(request_id.to_string());
        state.owner_companion_id = Some(owner_companion_id.to_string());
        state.rect = Some(rect);
        Ok(())
    }

    pub fn can_show(&self, request_id: &str, owner_companion_id: &str) -> bool {
        self.0.lock().map(|state| state.request_id.as_deref() == Some(request_id) && state.owner_companion_id.as_deref() == Some(owner_companion_id) && state.rect.is_some()).unwrap_or(false)
    }

    pub fn is_empty(&self) -> bool {
        self.0.lock().map(|state| state.request_id.is_none()).unwrap_or(false)
    }

    fn run_if_current<F>(&self, request_id: &str, owner_companion_id: &str, task: F) -> Result<bool, String>
    where
        F: FnOnce() -> Result<(), String>,
    {
        let state = self.0.lock().map_err(|_| "memory panel state poisoned".to_string())?;
        if state.request_id.as_deref() != Some(request_id)
            || state.owner_companion_id.as_deref() != Some(owner_companion_id)
            || state.rect.is_none()
        {
            return Ok(false);
        }
        task()?;
        Ok(true)
    }

    pub(crate) fn run_if_empty<F>(&self, task: F) -> Result<bool, String>
    where
        F: FnOnce() -> Result<(), String>,
    {
        let state = self.0.lock().map_err(|_| "memory panel state poisoned".to_string())?;
        if state.request_id.is_some() { return Ok(false); }
        task()?;
        Ok(true)
    }

    pub fn finish_hide(&self, request_id: &str) -> bool {
        let Ok(mut state) = self.0.lock() else { return false };
        if state.request_id.as_deref() != Some(request_id) { return false; }
        *state = MemoryPanelSession::default();
        true
    }

    pub fn invalidate_owner_unless(&self, enabled: &HashSet<String>) -> bool {
        let Ok(mut state) = self.0.lock() else { return false };
        let invalid = state.owner_companion_id.as_ref().is_some_and(|owner| !enabled.contains(owner));
        if invalid { *state = MemoryPanelSession::default(); }
        invalid
    }

    pub fn snapshot(&self) -> MemoryPanelSessionSnapshot {
        self.0.lock().map(|state| MemoryPanelSessionSnapshot { request_id: state.request_id.clone(), owner_companion_id: state.owner_companion_id.clone(), rect: state.rect }).unwrap_or(MemoryPanelSessionSnapshot { request_id: None, owner_companion_id: None, rect: None })
    }
}

#[tauri::command]
pub async fn prepare_companion_memory_panel(app: tauri::AppHandle, server: tauri::State<'_, Arc<DesktopServer>>) -> Result<(), String> {
    let init_script = webui_init_script(server.loopback_port(), server.local_trust_secret());
    let app_for_task = app.clone();
    run_on_main_thread_task(
        move |task| app.run_on_main_thread(task).map_err(|error| error.to_string()),
        move || {
            if app_for_task.get_webview_window(MEMORY_PANEL_LABEL).is_some() { return Ok(()); }
            tauri::WebviewWindowBuilder::new(&app_for_task, MEMORY_PANEL_LABEL, tauri::WebviewUrl::App("index.html#/nomi-memory-panel".into()))
                .title("NomiFun Memory")
                .inner_size(300.0, 320.0)
                .resizable(false)
                .decorations(false)
                .transparent(true)
                .always_on_top(true)
                .skip_taskbar(true)
                .shadow(true)
                .visible(false)
                .initialization_script(&init_script)
                .build()
                .map(|_| ())
                .map_err(|error| error.to_string())
        },
    ).await
}

#[tauri::command]
pub async fn place_companion_memory_panel(app: tauri::AppHandle, state: tauri::State<'_, MemoryPanelWindowState>, request_id: String, owner_companion_id: String, rect: PhysicalRect) -> Result<(), String> {
    state.place(&request_id, &owner_companion_id, rect)?;
    let task_state = state.inner().clone();
    let task_request_id = request_id.clone();
    let task_owner_companion_id = owner_companion_id.clone();
    let app_for_task = app.clone();
    let result = run_on_main_thread_task(
        move |task| app.run_on_main_thread(task).map_err(|error| error.to_string()),
        move || {
            task_state
                .run_if_current(&task_request_id, &task_owner_companion_id, || {
                    let window = app_for_task.get_webview_window(MEMORY_PANEL_LABEL).ok_or_else(|| "memory panel window is not prepared".to_string())?;
                    let _ = window.hide();
                    window.set_size(PhysicalSize::new(rect.width, rect.height)).map_err(|error| error.to_string())?;
                    window.set_position(PhysicalPosition::new(rect.x, rect.y)).map_err(|error| error.to_string())
                })
                .map(|_| ())
        },
    ).await;
    if result.is_err() { state.finish_hide(&request_id); }
    result
}

#[tauri::command]
pub async fn show_companion_memory_panel(app: tauri::AppHandle, state: tauri::State<'_, MemoryPanelWindowState>, request_id: String, owner_companion_id: String) -> Result<bool, String> {
    if !state.can_show(&request_id, &owner_companion_id) { return Ok(false); }
    let task_state = state.inner().clone();
    let task_request_id = request_id.clone();
    let task_owner_companion_id = owner_companion_id.clone();
    let app_for_task = app.clone();
    run_on_main_thread_task(
        move |task| app.run_on_main_thread(task).map_err(|error| error.to_string()),
        move || {
            task_state
                .run_if_current(&task_request_id, &task_owner_companion_id, || {
                    let window = app_for_task.get_webview_window(MEMORY_PANEL_LABEL).ok_or_else(|| "memory panel window is not prepared".to_string())?;
                    window.show().map_err(|error| error.to_string())?;
                    window.set_focus().map_err(|error| error.to_string())
                })
                .map(|_| ())
        },
    ).await?;
    Ok(state.can_show(&request_id, &owner_companion_id))
}

#[tauri::command]
pub async fn hide_companion_memory_panel(app: tauri::AppHandle, state: tauri::State<'_, MemoryPanelWindowState>, request_id: String) -> Result<bool, String> {
    if !state.finish_hide(&request_id) { return Ok(false); }
    let task_state = state.inner().clone();
    let app_for_task = app.clone();
    run_on_main_thread_task(
        move |task| app.run_on_main_thread(task).map_err(|error| error.to_string()),
        move || {
            task_state
                .run_if_empty(|| {
                    if let Some(window) = app_for_task.get_webview_window(MEMORY_PANEL_LABEL) { window.hide().map_err(|error| error.to_string())?; }
                    Ok(())
                })
                .map(|_| ())
        },
    ).await?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn stale_request_cannot_hide_new_owner() {
        let state = MemoryPanelWindowState::default();
        state.place("r1", "a", PhysicalRect::new(10, 20, 340, 300)).unwrap();
        state.place("r2", "b", PhysicalRect::new(30, 40, 340, 300)).unwrap();
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
        state.place("r1", "a", PhysicalRect::new(10, 20, 340, 300)).unwrap();
        assert!(state.invalidate_owner_unless(&HashSet::from(["b".to_string()])));
        assert!(state.snapshot().request_id.is_none());
    }

    #[test]
    fn cloned_state_guards_native_tasks_against_newer_requests() {
        let state = MemoryPanelWindowState::default();
        let native_task_state = state.clone();
        assert!(native_task_state.is_empty());

        state.place("r1", "a", PhysicalRect::new(10, 20, 340, 300)).unwrap();
        assert!(native_task_state.can_show("r1", "a"));
        assert!(!native_task_state.is_empty());

        assert!(state.finish_hide("r1"));
        assert!(native_task_state.is_empty());
        state.place("r2", "b", PhysicalRect::new(30, 40, 340, 300)).unwrap();
        assert!(!native_task_state.is_empty());
        assert!(!native_task_state.can_show("r1", "a"));
        assert!(native_task_state.can_show("r2", "b"));

        let mut stale_task_ran = false;
        assert!(!native_task_state
            .run_if_current("r1", "a", || {
                stale_task_ran = true;
                Ok(())
            })
            .unwrap());
        assert!(!stale_task_ran);

        let mut stale_hide_ran = false;
        assert!(!native_task_state
            .run_if_empty(|| {
                stale_hide_ran = true;
                Ok(())
            })
            .unwrap());
        assert!(!stale_hide_ran);
    }
}

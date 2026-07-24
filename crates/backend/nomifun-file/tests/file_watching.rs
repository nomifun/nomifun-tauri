//! Integration tests for file watching (task 7.8).
//!
//! Tests exercise `IFileWatchService` through `FileWatchService`, verifying
//! that filesystem changes produce the expected broadcast events.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use nomifun_api_types::WebSocketMessage;
use nomifun_file::{FileWatchService, IFileWatchService};
use nomifun_realtime::UserEventSink;

// -----------------------------------------------------------------------
// Test helpers
// -----------------------------------------------------------------------

/// A broadcaster that records every event for later assertion.
struct RecordingBroadcaster {
    events: Mutex<Vec<WebSocketMessage<serde_json::Value>>>,
    owners: Mutex<Vec<String>>,
}

impl RecordingBroadcaster {
    fn new() -> Self {
        Self {
            events: Mutex::new(Vec::new()),
            owners: Mutex::new(Vec::new()),
        }
    }

    /// Drain all recorded events.
    fn take_events(&self) -> Vec<WebSocketMessage<serde_json::Value>> {
        let mut guard = self.events.lock().unwrap();
        std::mem::take(&mut *guard)
    }

    fn take_owners(&self) -> Vec<String> {
        let mut guard = self.owners.lock().unwrap();
        std::mem::take(&mut *guard)
    }
}

impl UserEventSink for RecordingBroadcaster {
    fn send_to_user(&self, user_id: &str, event: WebSocketMessage<serde_json::Value>) {
        self.owners.lock().unwrap().push(user_id.to_owned());
        self.events.lock().unwrap().push(event);
    }
}

fn make_service() -> (Arc<dyn IFileWatchService>, Arc<RecordingBroadcaster>) {
    let recorder = Arc::new(RecordingBroadcaster::new());
    let svc = FileWatchService::new(recorder.clone()).unwrap();
    (Arc::new(svc), recorder)
}

/// Wait a bit for the OS file-system event to propagate and the watcher
/// callback to fire. File-system notifications are inherently asynchronous.
async fn settle() {
    tokio::time::sleep(Duration::from_millis(500)).await;
}

// -----------------------------------------------------------------------
// Single-file watching
// -----------------------------------------------------------------------

#[tokio::test]
async fn start_watch_and_detect_change() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("watched.txt");
    std::fs::write(&file, "initial").unwrap();

    let (svc, recorder) = make_service();
    svc.start_watch("0190f5fe-7c00-7a00-8abc-012345678901", file.to_str().unwrap()).await.unwrap();

    // Modify the file.
    settle().await;
    std::fs::write(&file, "updated").unwrap();
    settle().await;

    let events = recorder.take_events();
    assert!(
        events.iter().any(|e| e.name == "fileWatch.fileChanged"),
        "expected fileWatch.fileChanged event, got: {events:?}"
    );

    let ev = events.iter().find(|e| e.name == "fileWatch.fileChanged").unwrap();
    assert!(ev.data["file_path"].as_str().is_some());
    assert!(ev.data["event_type"].as_str().is_some());
}

#[tokio::test]
async fn stop_watch_stops_events() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("stop_me.txt");
    std::fs::write(&file, "v1").unwrap();

    let (svc, recorder) = make_service();
    let path_str = file.to_str().unwrap();
    svc.start_watch("0190f5fe-7c00-7a00-8abc-012345678901", path_str).await.unwrap();
    settle().await;

    svc.stop_watch("0190f5fe-7c00-7a00-8abc-012345678901", path_str).await.unwrap();
    // Drain any events from the watch setup.
    recorder.take_events();

    // Modify after stop — should NOT produce events.
    std::fs::write(&file, "v2").unwrap();
    settle().await;

    let events = recorder.take_events();
    assert!(events.is_empty(), "expected no events after stop, got: {events:?}");
}

#[tokio::test]
async fn stop_all_watches_clears_file_watches() {
    let dir = tempfile::tempdir().unwrap();
    let file_a = dir.path().join("a.txt");
    let file_b = dir.path().join("b.txt");
    std::fs::write(&file_a, "a").unwrap();
    std::fs::write(&file_b, "b").unwrap();

    let (svc, recorder) = make_service();
    svc.start_watch("0190f5fe-7c00-7a00-8abc-012345678901", file_a.to_str().unwrap()).await.unwrap();
    svc.start_watch("0190f5fe-7c00-7a00-8abc-012345678901", file_b.to_str().unwrap()).await.unwrap();
    settle().await;

    svc.stop_all_watches("0190f5fe-7c00-7a00-8abc-012345678901").await.unwrap();
    recorder.take_events();

    std::fs::write(&file_a, "a2").unwrap();
    std::fs::write(&file_b, "b2").unwrap();
    settle().await;

    let events = recorder.take_events();
    assert!(events.is_empty(), "expected no events after stop_all, got: {events:?}");
}

#[tokio::test]
async fn idempotent_start_watch() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("idem.txt");
    std::fs::write(&file, "x").unwrap();

    let (svc, _recorder) = make_service();
    let path_str = file.to_str().unwrap();
    svc.start_watch("0190f5fe-7c00-7a00-8abc-012345678901", path_str).await.unwrap();
    // Second start should be a no-op, not an error.
    svc.start_watch("0190f5fe-7c00-7a00-8abc-012345678901", path_str).await.unwrap();
}

#[tokio::test]
async fn shared_file_watch_keeps_each_owner_isolated_until_they_stop() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("shared.txt");
    std::fs::write(&file, "v1").unwrap();

    let (svc, recorder) = make_service();
    let path = file.to_str().unwrap();
    svc.start_watch("0190f5fe-7c00-7a00-8abc-012345678901", path).await.unwrap();
    svc.start_watch("0190f5fe-7c00-7a00-8abc-012345678902", path).await.unwrap();
    settle().await;

    std::fs::write(&file, "v2").unwrap();
    settle().await;
    let mut owners = recorder.take_owners();
    owners.sort();
    owners.dedup();
    assert_eq!(owners, vec!["0190f5fe-7c00-7a00-8abc-012345678901", "0190f5fe-7c00-7a00-8abc-012345678902"]);
    recorder.take_events();

    svc.stop_watch("0190f5fe-7c00-7a00-8abc-012345678901", path).await.unwrap();
    settle().await;
    std::fs::write(&file, "v3").unwrap();
    settle().await;
    let owners = recorder.take_owners();
    assert!(!owners.is_empty());
    assert!(owners.iter().all(|owner| owner == "0190f5fe-7c00-7a00-8abc-012345678902"));
}

#[tokio::test]
async fn watch_nonexistent_file_returns_error() {
    let (svc, _recorder) = make_service();
    let result = svc.start_watch("0190f5fe-7c00-7a00-8abc-012345678901", "/tmp/nonexistent_12345.txt").await;
    assert!(result.is_err());
}

// -----------------------------------------------------------------------
// Workspace Office file watching
// -----------------------------------------------------------------------

#[tokio::test]
async fn office_watch_detects_docx() {
    let dir = tempfile::tempdir().unwrap();
    let (svc, recorder) = make_service();
    svc.start_office_watch("0190f5fe-7c00-7a00-8abc-012345678901", dir.path().to_str().unwrap())
        .await
        .unwrap();
    settle().await;

    // Create a .docx file.
    std::fs::write(dir.path().join("report.docx"), "fake docx").unwrap();
    settle().await;

    let events = recorder.take_events();
    assert!(
        events.iter().any(|e| e.name == "workspaceOfficeWatch.fileAdded"),
        "expected workspaceOfficeWatch.fileAdded event, got: {events:?}"
    );
}

#[tokio::test]
async fn office_watch_detects_xlsx() {
    let dir = tempfile::tempdir().unwrap();
    let (svc, recorder) = make_service();
    svc.start_office_watch("0190f5fe-7c00-7a00-8abc-012345678901", dir.path().to_str().unwrap())
        .await
        .unwrap();
    settle().await;

    std::fs::write(dir.path().join("data.xlsx"), "fake xlsx").unwrap();
    settle().await;

    let events = recorder.take_events();
    assert!(
        events.iter().any(|e| e.name == "workspaceOfficeWatch.fileAdded"),
        "expected fileAdded for .xlsx, got: {events:?}"
    );
}

#[tokio::test]
async fn office_watch_detects_pptx() {
    let dir = tempfile::tempdir().unwrap();
    let (svc, recorder) = make_service();
    svc.start_office_watch("0190f5fe-7c00-7a00-8abc-012345678901", dir.path().to_str().unwrap())
        .await
        .unwrap();
    settle().await;

    std::fs::write(dir.path().join("slides.pptx"), "fake pptx").unwrap();
    settle().await;

    let events = recorder.take_events();
    assert!(
        events.iter().any(|e| e.name == "workspaceOfficeWatch.fileAdded"),
        "expected fileAdded for .pptx, got: {events:?}"
    );
}

#[tokio::test]
async fn office_watch_ignores_non_office_files() {
    let dir = tempfile::tempdir().unwrap();
    let (svc, recorder) = make_service();
    svc.start_office_watch("0190f5fe-7c00-7a00-8abc-012345678901", dir.path().to_str().unwrap())
        .await
        .unwrap();
    settle().await;

    // Drain any setup events.
    recorder.take_events();

    // Create a non-Office file — should NOT trigger.
    std::fs::write(dir.path().join("notes.txt"), "hello").unwrap();
    settle().await;

    let events = recorder.take_events();
    let office_events: Vec<_> = events
        .iter()
        .filter(|e| e.name == "workspaceOfficeWatch.fileAdded")
        .collect();
    assert!(
        office_events.is_empty(),
        "expected no office events for .txt, got: {office_events:?}"
    );
}

#[tokio::test]
async fn stop_office_watch_stops_events() {
    let dir = tempfile::tempdir().unwrap();
    let (svc, recorder) = make_service();
    let ws = dir.path().to_str().unwrap();
    svc.start_office_watch("0190f5fe-7c00-7a00-8abc-012345678901", ws).await.unwrap();
    settle().await;

    svc.stop_office_watch("0190f5fe-7c00-7a00-8abc-012345678901", ws).await.unwrap();
    recorder.take_events();

    std::fs::write(dir.path().join("after_stop.docx"), "data").unwrap();
    settle().await;

    let events = recorder.take_events();
    let office_events: Vec<_> = events
        .iter()
        .filter(|e| e.name == "workspaceOfficeWatch.fileAdded")
        .collect();
    assert!(
        office_events.is_empty(),
        "expected no events after stop, got: {office_events:?}"
    );
}

#[tokio::test]
async fn idempotent_office_watch() {
    let dir = tempfile::tempdir().unwrap();
    let (svc, _recorder) = make_service();
    let ws = dir.path().to_str().unwrap();
    svc.start_office_watch("0190f5fe-7c00-7a00-8abc-012345678901", ws).await.unwrap();
    // Second call should be a no-op.
    svc.start_office_watch("0190f5fe-7c00-7a00-8abc-012345678901", ws).await.unwrap();
}

#[tokio::test]
async fn shared_office_watch_remains_active_for_the_other_owner() {
    let dir = tempfile::tempdir().unwrap();
    let (svc, recorder) = make_service();
    let workspace = dir.path().to_str().unwrap();
    svc.start_office_watch("0190f5fe-7c00-7a00-8abc-012345678901", workspace).await.unwrap();
    svc.start_office_watch("0190f5fe-7c00-7a00-8abc-012345678902", workspace).await.unwrap();
    settle().await;

    std::fs::write(dir.path().join("shared.docx"), "v1").unwrap();
    settle().await;
    let mut owners = recorder.take_owners();
    owners.sort();
    owners.dedup();
    assert_eq!(owners, vec!["0190f5fe-7c00-7a00-8abc-012345678901", "0190f5fe-7c00-7a00-8abc-012345678902"]);
    recorder.take_events();

    svc.stop_office_watch("0190f5fe-7c00-7a00-8abc-012345678901", workspace).await.unwrap();
    settle().await;
    std::fs::write(dir.path().join("still-watched.docx"), "v2").unwrap();
    settle().await;
    let owners = recorder.take_owners();
    assert!(!owners.is_empty());
    assert!(owners.iter().all(|owner| owner == "0190f5fe-7c00-7a00-8abc-012345678902"));
}

#[tokio::test]
async fn office_watch_event_has_correct_fields() {
    let dir = tempfile::tempdir().unwrap();
    let (svc, recorder) = make_service();
    svc.start_office_watch("0190f5fe-7c00-7a00-8abc-012345678901", dir.path().to_str().unwrap())
        .await
        .unwrap();
    settle().await;

    std::fs::write(dir.path().join("check.docx"), "content").unwrap();
    settle().await;

    let events = recorder.take_events();
    let ev = events.iter().find(|e| e.name == "workspaceOfficeWatch.fileAdded");
    assert!(ev.is_some(), "expected fileAdded event, got: {events:?}");

    let data = &ev.unwrap().data;
    assert!(
        data["file_path"].as_str().is_some_and(|p| p.ends_with("check.docx")),
        "file_path should end with check.docx: {data:?}"
    );
    assert!(
        data["workspace"].as_str().is_some(),
        "workspace should be present: {data:?}"
    );
}

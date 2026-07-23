use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::time::Duration;

use nomifun_api_types::WebSocketMessage;
use nomifun_office::{DocType, OfficeError, OfficecliWatchManager, ProcessHandle, ProcessSpawner};
use nomifun_realtime::UserEventSink;

// ---------------------------------------------------------------------------
// Test doubles
// ---------------------------------------------------------------------------

struct MockHandle {
    alive: AtomicBool,
}

impl MockHandle {
    fn new() -> Self {
        Self {
            alive: AtomicBool::new(true),
        }
    }
}

impl ProcessHandle for MockHandle {
    fn kill(&self) {
        self.alive.store(false, Ordering::SeqCst);
    }

    fn is_alive(&self) -> bool {
        self.alive.load(Ordering::SeqCst)
    }
}

struct TestSpawner {
    installed: AtomicBool,
    spawn_count: AtomicU32,
    install_count: AtomicU32,
}

impl TestSpawner {
    fn new(installed: bool) -> Self {
        Self {
            installed: AtomicBool::new(installed),
            spawn_count: AtomicU32::new(0),
            install_count: AtomicU32::new(0),
        }
    }
}

#[async_trait::async_trait]
impl ProcessSpawner for TestSpawner {
    async fn spawn_officecli(
        &self,
        _file_path: &str,
        port: u16,
        _doc_type: DocType,
    ) -> Result<Box<dyn ProcessHandle>, OfficeError> {
        self.spawn_count.fetch_add(1, Ordering::SeqCst);

        if !self.installed.load(Ordering::SeqCst) {
            return Err(OfficeError::OfficecliNotFound);
        }

        let listener = std::net::TcpListener::bind(format!("127.0.0.1:{port}"))
            .map_err(|e| OfficeError::StartFailed(e.to_string()))?;
        std::mem::forget(listener);

        Ok(Box::new(MockHandle::new()))
    }

    async fn install_officecli(&self) -> Result<(), OfficeError> {
        self.install_count.fetch_add(1, Ordering::SeqCst);
        self.installed.store(true, Ordering::SeqCst);
        Ok(())
    }

    async fn is_officecli_installed(&self) -> bool {
        self.installed.load(Ordering::SeqCst)
    }

    async fn check_update(&self, _doc_type: DocType) -> Result<(), OfficeError> {
        Ok(())
    }
}

struct TestBroadcaster {
    events: std::sync::Mutex<Vec<WebSocketMessage<serde_json::Value>>>,
}

impl TestBroadcaster {
    fn new() -> Self {
        Self {
            events: std::sync::Mutex::new(Vec::new()),
        }
    }

    fn event_names(&self) -> Vec<String> {
        self.events.lock().unwrap().iter().map(|e| e.name.clone()).collect()
    }

    fn event_states(&self) -> Vec<String> {
        self.events
            .lock()
            .unwrap()
            .iter()
            .filter_map(|e| e.data["state"].as_str().map(String::from))
            .collect()
    }
}

impl UserEventSink for TestBroadcaster {
    fn send_to_user(&self, _user_id: &str, event: WebSocketMessage<serde_json::Value>) {
        self.events.lock().unwrap().push(event);
    }
}

fn create_temp_file(dir: &tempfile::TempDir, name: &str) -> String {
    let path = dir.path().join(name);
    std::fs::write(&path, b"test content").unwrap();
    path.to_string_lossy().into_owned()
}

// ---------------------------------------------------------------------------
// WP-2: Session reuse (same file, same doc type)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn wp2_session_reuse_returns_same_port() {
    let spawner = Arc::new(TestSpawner::new(true));
    let broadcaster = Arc::new(TestBroadcaster::new());
    let mgr = OfficecliWatchManager::new(spawner.clone(), broadcaster);

    let dir = tempfile::tempdir().unwrap();
    let path = create_temp_file(&dir, "doc.docx");

    let first = mgr.start("0190f5fe-7c00-7a00-8abc-012345678901", &path, DocType::Word).await.unwrap();
    let second = mgr.start("0190f5fe-7c00-7a00-8abc-012345678901", &path, DocType::Word).await.unwrap();

    assert_eq!(first.port, second.port);
    assert_ne!(first.capability, second.capability);
    assert!(mgr.resolve_capability(&first.capability).is_some());
    assert!(mgr.resolve_capability(&second.capability).is_some());
    assert_eq!(spawner.spawn_count.load(Ordering::SeqCst), 1);
}

// ---------------------------------------------------------------------------
// WP-3: Stop removes session and kills process
// ---------------------------------------------------------------------------

#[tokio::test]
async fn wp3_stop_terminates_session() {
    let spawner = Arc::new(TestSpawner::new(true));
    let broadcaster = Arc::new(TestBroadcaster::new());
    let mgr = OfficecliWatchManager::new(spawner, broadcaster);

    let dir = tempfile::tempdir().unwrap();
    let path = create_temp_file(&dir, "doc.docx");

    let access = mgr.start("0190f5fe-7c00-7a00-8abc-012345678901", &path, DocType::Word).await.unwrap();
    assert!(mgr.resolve_capability(&access.capability).is_some());

    mgr.stop("0190f5fe-7c00-7a00-8abc-012345678901", DocType::Word, &access.capability)
        .await;
    assert!(mgr.resolve_capability(&access.capability).is_none());
    assert_eq!(mgr.active_session_count(), 0);
}

// ---------------------------------------------------------------------------
// WP-4: Auto-install when officecli not found
// ---------------------------------------------------------------------------

#[tokio::test]
async fn wp4_auto_install_on_not_found() {
    let spawner = Arc::new(TestSpawner::new(false));
    let broadcaster = Arc::new(TestBroadcaster::new());
    let mgr = OfficecliWatchManager::new(spawner.clone(), broadcaster.clone());

    let dir = tempfile::tempdir().unwrap();
    let path = create_temp_file(&dir, "doc.docx");

    let access = mgr.start("0190f5fe-7c00-7a00-8abc-012345678901", &path, DocType::Word).await.unwrap();
    assert!(access.port > 0);
    assert_eq!(spawner.install_count.load(Ordering::SeqCst), 1);

    let states = broadcaster.event_states();
    assert!(states.contains(&"starting".to_string()));
    assert!(states.contains(&"installing".to_string()));
    assert!(states.contains(&"ready".to_string()));
}

// ---------------------------------------------------------------------------
// EP-1: Excel uses independent session pool
// ---------------------------------------------------------------------------

#[tokio::test]
async fn ep1_excel_independent_session_pool() {
    let spawner = Arc::new(TestSpawner::new(true));
    let broadcaster = Arc::new(TestBroadcaster::new());
    let mgr = OfficecliWatchManager::new(spawner, broadcaster);

    let dir = tempfile::tempdir().unwrap();
    let path = create_temp_file(&dir, "data.xlsx");

    let word = mgr.start("0190f5fe-7c00-7a00-8abc-012345678901", &path, DocType::Word).await.unwrap();
    let excel = mgr.start("0190f5fe-7c00-7a00-8abc-012345678901", &path, DocType::Excel).await.unwrap();

    assert_ne!(word.port, excel.port);
    assert_eq!(mgr.active_session_count(), 2);
    assert_eq!(
        mgr.resolve_capability(&word.capability).unwrap().doc_type,
        DocType::Word
    );
    assert_eq!(
        mgr.resolve_capability(&excel.capability).unwrap().doc_type,
        DocType::Excel
    );
}

// ---------------------------------------------------------------------------
// PP-1: PPT uses independent session pool
// ---------------------------------------------------------------------------

#[tokio::test]
async fn pp1_ppt_independent_session_pool() {
    let spawner = Arc::new(TestSpawner::new(true));
    let broadcaster = Arc::new(TestBroadcaster::new());
    let mgr = OfficecliWatchManager::new(spawner, broadcaster);

    let dir = tempfile::tempdir().unwrap();
    let path = create_temp_file(&dir, "slides.pptx");

    let access = mgr.start("0190f5fe-7c00-7a00-8abc-012345678901", &path, DocType::Ppt).await.unwrap();
    assert!(access.port > 0);
    assert_eq!(
        mgr.resolve_capability(&access.capability).unwrap().doc_type,
        DocType::Ppt
    );
}

// ---------------------------------------------------------------------------
// PP-3: PPT triggers background version check
// ---------------------------------------------------------------------------

#[tokio::test]
async fn pp3_ppt_background_version_check() {
    let spawner = Arc::new(TestSpawner::new(true));
    let broadcaster = Arc::new(TestBroadcaster::new());
    let mgr = OfficecliWatchManager::new(spawner, broadcaster);

    let dir = tempfile::tempdir().unwrap();
    let path = create_temp_file(&dir, "slides.pptx");

    mgr.start("0190f5fe-7c00-7a00-8abc-012345678901", &path, DocType::Ppt).await.unwrap();

    tokio::time::sleep(Duration::from_millis(50)).await;
    // Version check is fire-and-forget; we just verify it doesn't panic
}

// ---------------------------------------------------------------------------
// Status event naming per doc type
// ---------------------------------------------------------------------------

#[tokio::test]
async fn status_events_use_correct_prefix() {
    let spawner = Arc::new(TestSpawner::new(true));
    let broadcaster = Arc::new(TestBroadcaster::new());
    let mgr = OfficecliWatchManager::new(spawner, broadcaster.clone());

    let dir = tempfile::tempdir().unwrap();

    let f1 = create_temp_file(&dir, "a.docx");
    let f2 = create_temp_file(&dir, "b.xlsx");
    let f3 = create_temp_file(&dir, "c.pptx");

    mgr.start("0190f5fe-7c00-7a00-8abc-012345678901", &f1, DocType::Word).await.unwrap();
    mgr.start("0190f5fe-7c00-7a00-8abc-012345678901", &f2, DocType::Excel).await.unwrap();
    mgr.start("0190f5fe-7c00-7a00-8abc-012345678901", &f3, DocType::Ppt).await.unwrap();

    let names = broadcaster.event_names();
    assert!(names.contains(&"word-preview.status".to_string()));
    assert!(names.contains(&"excel-preview.status".to_string()));
    assert!(names.contains(&"ppt-preview.status".to_string()));
}

// ---------------------------------------------------------------------------
// stop_all lifecycle
// ---------------------------------------------------------------------------

#[tokio::test]
async fn stop_all_clears_all_sessions() {
    let spawner = Arc::new(TestSpawner::new(true));
    let broadcaster = Arc::new(TestBroadcaster::new());
    let mgr = OfficecliWatchManager::new(spawner, broadcaster);

    let dir = tempfile::tempdir().unwrap();
    let f1 = create_temp_file(&dir, "a.docx");
    let f2 = create_temp_file(&dir, "b.xlsx");
    let f3 = create_temp_file(&dir, "c.pptx");

    mgr.start("0190f5fe-7c00-7a00-8abc-012345678901", &f1, DocType::Word).await.unwrap();
    mgr.start("0190f5fe-7c00-7a00-8abc-012345678901", &f2, DocType::Excel).await.unwrap();
    mgr.start("0190f5fe-7c00-7a00-8abc-012345678901", &f3, DocType::Ppt).await.unwrap();
    assert_eq!(mgr.active_session_count(), 3);

    mgr.stop_all().await;
    assert_eq!(mgr.active_session_count(), 0);
}

// ---------------------------------------------------------------------------
// SSRF defense: guessed and legacy port capabilities fail closed
// ---------------------------------------------------------------------------

#[tokio::test]
async fn rp2_guessed_capability_rejected() {
    let spawner = Arc::new(TestSpawner::new(true));
    let broadcaster = Arc::new(TestBroadcaster::new());
    let mgr = OfficecliWatchManager::new(spawner, broadcaster);

    assert!(mgr.resolve_capability("8080").is_none());
    assert!(mgr.resolve_capability(&"0".repeat(64)).is_none());
}

// ---------------------------------------------------------------------------
// Stop then restart creates new session
// ---------------------------------------------------------------------------

#[tokio::test]
async fn stop_then_restart_creates_new_session() {
    let spawner = Arc::new(TestSpawner::new(true));
    let broadcaster = Arc::new(TestBroadcaster::new());
    let mgr = OfficecliWatchManager::new(spawner.clone(), broadcaster);

    let dir = tempfile::tempdir().unwrap();
    let path = create_temp_file(&dir, "doc.docx");

    let first = mgr.start("0190f5fe-7c00-7a00-8abc-012345678901", &path, DocType::Word).await.unwrap();
    mgr.stop("0190f5fe-7c00-7a00-8abc-012345678901", DocType::Word, &first.capability)
        .await;

    let second = mgr.start("0190f5fe-7c00-7a00-8abc-012345678901", &path, DocType::Word).await.unwrap();
    assert_ne!(first.port, second.port);
    assert_ne!(first.capability, second.capability);
    assert_eq!(spawner.spawn_count.load(Ordering::SeqCst), 2);
}

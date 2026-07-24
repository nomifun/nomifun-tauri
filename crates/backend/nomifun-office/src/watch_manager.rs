use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use dashmap::DashMap;
use nomifun_api_types::{
    is_preview_capability, PreviewState, PreviewStatusEvent, WebSocketMessage, PREVIEW_CAPABILITY_BYTES,
};
use nomifun_common::UserId;
use nomifun_realtime::UserEventSink;
use nomi_process_runtime::ChildProcessBuilder as CmdBuilder;
use tokio::sync::Mutex;

use crate::error::OfficeError;
use crate::port::{allocate_port, is_port_listening};
use crate::types::DocType;

const POLL_INTERVAL_MS: u64 = 100;
#[cfg(not(test))]
const POLL_MAX_ATTEMPTS: u32 = 150;
#[cfg(test)]
const POLL_MAX_ATTEMPTS: u32 = 3;
const STOP_DELAY_MS: u64 = 500;
const VERSION_CHECK_INTERVAL: Duration = Duration::from_secs(24 * 60 * 60);

// ---------------------------------------------------------------------------
// ProcessSpawner trait — abstraction for child process management
// ---------------------------------------------------------------------------

#[async_trait::async_trait]
pub trait ProcessSpawner: Send + Sync {
    async fn spawn_officecli(
        &self,
        file_path: &str,
        port: u16,
        doc_type: DocType,
    ) -> Result<Box<dyn ProcessHandle>, OfficeError>;

    async fn install_officecli(&self) -> Result<(), OfficeError>;

    async fn is_officecli_installed(&self) -> bool;

    async fn check_update(&self, doc_type: DocType) -> Result<(), OfficeError>;
}

pub trait ProcessHandle: Send + Sync {
    fn kill(&self);
    fn is_alive(&self) -> bool;
}

// ---------------------------------------------------------------------------
// WatchSession — per-file preview session
// ---------------------------------------------------------------------------

struct WatchSession {
    port: u16,
    process: Box<dyn ProcessHandle>,
    file_path: String,
    doc_type: DocType,
    /// Every successful start owns one independently revocable capability.
    /// The owner is retained only for authenticated stop authorization; proxy
    /// requests never receive or trust an owner id.
    capabilities: HashMap<String, String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PreviewCapabilityBinding {
    session_key: String,
    owner_id: String,
    port: u16,
    doc_type: DocType,
}

/// The result of starting one preview lease.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreviewAccess {
    pub port: u16,
    pub capability: String,
    pub doc_type: DocType,
}

/// The only upstream coordinates exposed to the reverse proxy after a
/// capability has been validated against the live session registry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PreviewProxyTarget {
    pub port: u16,
    pub doc_type: DocType,
}

// ---------------------------------------------------------------------------
// OfficecliWatchManager
// ---------------------------------------------------------------------------

pub struct OfficecliWatchManager {
    sessions: DashMap<String, WatchSession>,
    /// Deliberately memory-only: process exit/restart revokes every bearer
    /// capability instead of resurrecting access from durable state.
    capabilities: DashMap<String, PreviewCapabilityBinding>,
    /// Makes the async check/start/insert and stop/remove lifecycle atomic.
    session_lifecycle: tokio::sync::Mutex<()>,
    spawner: Arc<dyn ProcessSpawner>,
    user_events: Arc<dyn UserEventSink>,
    last_version_check: Mutex<Option<std::time::Instant>>,
}

impl OfficecliWatchManager {
    pub fn new(spawner: Arc<dyn ProcessSpawner>, user_events: Arc<dyn UserEventSink>) -> Self {
        Self {
            sessions: DashMap::new(),
            capabilities: DashMap::new(),
            session_lifecycle: tokio::sync::Mutex::new(()),
            spawner,
            user_events,
            last_version_check: Mutex::new(None),
        }
    }

    pub async fn start(
        &self,
        owner_id: &str,
        file_path: &str,
        doc_type: DocType,
    ) -> Result<PreviewAccess, OfficeError> {
        require_owner(owner_id)?;
        let resolved = resolve_path(file_path)?;
        let key = session_key(&resolved, doc_type);
        // Startup contains awaits. Serializing this transition prevents two
        // concurrent callers from spawning duplicate processes and losing an
        // owner when the later DashMap insert replaces the earlier session.
        let _lifecycle = self.session_lifecycle.lock().await;
        let capability = self.mint_capability()?;

        if let Some(mut entry) = self.sessions.get_mut(&key) {
            if entry.process.is_alive() {
                let port = entry.port;
                entry
                    .capabilities
                    .insert(capability.clone(), owner_id.to_owned());
                self.capabilities.insert(
                    capability.clone(),
                    PreviewCapabilityBinding {
                        session_key: key,
                        owner_id: owner_id.to_owned(),
                        port,
                        doc_type,
                    },
                );
                return Ok(PreviewAccess {
                    port,
                    capability,
                    doc_type,
                });
            }
            drop(entry);
            if let Some((_, stale)) = self.sessions.remove(&key) {
                self.revoke_session_capabilities(&stale);
                stale.process.kill();
            }
        }

        self.send_status(owner_id, doc_type, PreviewState::Starting, None);

        let result = self
            .try_start(owner_id, &resolved, doc_type, capability)
            .await;

        match &result {
            Ok(access) => {
                self.send_status(owner_id, doc_type, PreviewState::Ready, None);
                if doc_type == DocType::Ppt {
                    self.maybe_check_update(doc_type).await;
                }
                Ok(access.clone())
            }
            Err(e) => {
                self.send_status(owner_id, doc_type, PreviewState::Error, Some(e.to_string()));
                Err(match e {
                    OfficeError::OfficecliNotFound => OfficeError::OfficecliNotFound,
                    OfficeError::InstallFailed(m) => OfficeError::InstallFailed(m.clone()),
                    OfficeError::StartFailed(m) => OfficeError::StartFailed(m.clone()),
                    OfficeError::PortTimeout(m) => OfficeError::PortTimeout(m.clone()),
                    OfficeError::Io(io) => OfficeError::StartFailed(format!("IO error: {io}")),
                    OfficeError::Snapshot(m) => OfficeError::StartFailed(m.clone()),
                    OfficeError::Json(e) => OfficeError::StartFailed(format!("JSON error: {e}")),
                    OfficeError::Conversion(m) => OfficeError::StartFailed(m.clone()),
                    OfficeError::ToolNotFound(m) => OfficeError::StartFailed(m.clone()),
                })
            }
        }
    }

    async fn try_start(
        &self,
        owner_id: &str,
        resolved: &str,
        doc_type: DocType,
        capability: String,
    ) -> Result<PreviewAccess, OfficeError> {
        let port = allocate_port()?;

        let spawn_result = self.spawner.spawn_officecli(resolved, port, doc_type).await;

        let process = match spawn_result {
            Ok(p) => p,
            Err(OfficeError::OfficecliNotFound) => {
                self.send_status(owner_id, doc_type, PreviewState::Installing, None);
                self.spawner.install_officecli().await?;
                self.spawner
                    .spawn_officecli(resolved, port, doc_type)
                    .await?
            }
            Err(e) => return Err(e),
        };

        self.poll_port_ready(port, resolved).await?;

        let key = session_key(resolved, doc_type);
        let capabilities = HashMap::from([(capability.clone(), owner_id.to_owned())]);
        self.sessions.insert(
            key.clone(),
            WatchSession {
                port,
                process,
                file_path: resolved.to_owned(),
                doc_type,
                capabilities,
            },
        );
        self.capabilities.insert(
            capability.clone(),
            PreviewCapabilityBinding {
                session_key: key,
                owner_id: owner_id.to_owned(),
                port,
                doc_type,
            },
        );

        Ok(PreviewAccess {
            port,
            capability,
            doc_type,
        })
    }

    async fn poll_port_ready(&self, port: u16, file_path: &str) -> Result<(), OfficeError> {
        for _ in 0..POLL_MAX_ATTEMPTS {
            if is_port_listening(port).await {
                return Ok(());
            }
            tokio::time::sleep(Duration::from_millis(POLL_INTERVAL_MS)).await;
        }
        Err(OfficeError::PortTimeout(file_path.to_owned()))
    }

    pub async fn stop(
        &self,
        owner_id: &str,
        doc_type: DocType,
        capability: &str,
    ) {
        if require_owner(owner_id).is_err() || !is_preview_capability(capability) {
            return;
        }

        let session_to_kill = {
            let _lifecycle = self.session_lifecycle.lock().await;
            let Some(binding) = self
                .capabilities
                .get(capability)
                .map(|entry| entry.value().clone())
            else {
                return;
            };
            if binding.owner_id != owner_id || binding.doc_type != doc_type {
                return;
            }
            let key = binding.session_key;
            let remove_session = self.sessions.get_mut(&key).is_some_and(|mut session| {
                let session_owns_capability = session
                    .capabilities
                    .get(capability)
                    .is_some_and(|capability_owner| capability_owner == owner_id);
                let binding_matches = binding.port == session.port && binding.doc_type == session.doc_type;

                if !session_owns_capability || !binding_matches {
                    return false;
                }

                // Revoke before any delayed process cleanup. From this point a
                // concurrent iframe request fails closed immediately.
                self.capabilities.remove(capability);
                session.capabilities.remove(capability);
                session.capabilities.is_empty()
            });

            if remove_session {
                self.sessions.remove(&key).map(|(_, session)| session)
            } else {
                None
            }
        };

        if let Some(session) = session_to_kill {
            self.revoke_session_capabilities(&session);
            tokio::time::sleep(Duration::from_millis(STOP_DELAY_MS)).await;
            session.process.kill();
        }
    }

    pub async fn stop_all(&self) {
        let _lifecycle = self.session_lifecycle.lock().await;
        for entry in self.sessions.iter() {
            tracing::debug!(
                file_path = %entry.value().file_path,
                doc_type = %entry.value().doc_type,
                "stopping preview session"
            );
            entry.value().process.kill();
        }
        self.capabilities.clear();
        self.sessions.clear();
    }

    /// Resolve an untrusted URL capability to an active loopback target. Every
    /// field is cross-checked against both indexes so stale or partially removed
    /// state fails closed.
    pub fn resolve_capability(&self, capability: &str) -> Option<PreviewProxyTarget> {
        if !is_preview_capability(capability) {
            return None;
        }

        let binding = self.capabilities.get(capability)?;
        let session = self.sessions.get(&binding.session_key)?;
        let owner_matches = session
            .capabilities
            .get(capability)
            .is_some_and(|owner_id| owner_id == &binding.owner_id);

        if !owner_matches
            || !session.process.is_alive()
            || session.port != binding.port
            || session.doc_type != binding.doc_type
        {
            return None;
        }

        Some(PreviewProxyTarget {
            port: binding.port,
            doc_type: binding.doc_type,
        })
    }

    pub fn active_session_count(&self) -> usize {
        self.sessions.len()
    }

    fn mint_capability(&self) -> Result<String, OfficeError> {
        loop {
            let mut bytes = [0_u8; PREVIEW_CAPABILITY_BYTES];
            getrandom::getrandom(&mut bytes)
                .map_err(|e| OfficeError::StartFailed(format!("preview capability RNG failure: {e}")))?;
            let capability = hex::encode(bytes);
            if !self.capabilities.contains_key(&capability) {
                return Ok(capability);
            }
        }
    }

    fn revoke_session_capabilities(&self, session: &WatchSession) {
        for capability in session.capabilities.keys() {
            self.capabilities.remove(capability);
        }
    }

    async fn maybe_check_update(&self, doc_type: DocType) {
        let mut last = self.last_version_check.lock().await;
        let should_check = match *last {
            Some(t) => t.elapsed() >= VERSION_CHECK_INTERVAL,
            None => true,
        };
        if should_check {
            *last = Some(std::time::Instant::now());
            drop(last);
            let spawner = Arc::clone(&self.spawner);
            tokio::spawn(async move {
                if let Err(e) = spawner.check_update(doc_type).await {
                    tracing::warn!("officecli version check failed: {e}");
                }
            });
        }
    }

    fn send_status(&self, owner_id: &str, doc_type: DocType, state: PreviewState, message: Option<String>) {
        let event_name = format!("{}.status", doc_type.event_prefix());
        let payload = PreviewStatusEvent { state, message };
        let data = match serde_json::to_value(payload) {
            Ok(v) => v,
            Err(e) => {
                tracing::error!("failed to serialize preview status: {e}");
                return;
            }
        };
        self.user_events
            .send_to_user(owner_id, WebSocketMessage::new(event_name, data));
    }
}

impl Drop for OfficecliWatchManager {
    fn drop(&mut self) {
        for entry in self.sessions.iter() {
            entry.value().process.kill();
        }
        self.capabilities.clear();
        self.sessions.clear();
    }
}

// ---------------------------------------------------------------------------
// DefaultProcessSpawner — real implementation using tokio::process
// ---------------------------------------------------------------------------

pub struct DefaultProcessSpawner;

struct TokioProcessHandle {
    child: Mutex<Option<tokio::process::Child>>,
}

impl ProcessHandle for TokioProcessHandle {
    fn kill(&self) {
        if let Ok(mut guard) = self.child.try_lock() {
            if let Some(ref mut child) = *guard {
                let _ = child.start_kill();
            }
            *guard = None;
        }
    }

    fn is_alive(&self) -> bool {
        if let Ok(mut guard) = self.child.try_lock()
            && let Some(ref mut child) = *guard
        {
            return child.try_wait().ok().flatten().is_none();
        }
        false
    }
}

#[async_trait::async_trait]
impl ProcessSpawner for DefaultProcessSpawner {
    async fn spawn_officecli(
        &self,
        file_path: &str,
        port: u16,
        _doc_type: DocType,
    ) -> Result<Box<dyn ProcessHandle>, OfficeError> {
        let mut builder = CmdBuilder::new("officecli");
        builder
            .arg("watch")
            .arg(file_path)
            .arg("--port")
            .arg(port.to_string())
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());
        let child = builder.spawn().map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                OfficeError::OfficecliNotFound
            } else {
                OfficeError::StartFailed(e.to_string())
            }
        })?;

        Ok(Box::new(TokioProcessHandle {
            child: Mutex::new(Some(child)),
        }))
    }

    async fn install_officecli(&self) -> Result<(), OfficeError> {
        let mut builder = CmdBuilder::clean_cli("npm");
        builder.args(["install", "-g", "officecli"]);
        let output = builder
            .output()
            .await
            .map_err(|e| OfficeError::InstallFailed(e.to_string()))?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(OfficeError::InstallFailed(stderr.into_owned()));
        }
        Ok(())
    }

    async fn is_officecli_installed(&self) -> bool {
        let mut builder = CmdBuilder::clean_cli("officecli");
        builder.arg("--version");
        builder.output().await.is_ok_and(|o| o.status.success())
    }

    async fn check_update(&self, _doc_type: DocType) -> Result<(), OfficeError> {
        let mut builder = CmdBuilder::clean_cli("npm");
        builder.args(["outdated", "-g", "officecli"]);
        let output = builder
            .output()
            .await
            .map_err(|e| OfficeError::StartFailed(e.to_string()))?;

        if !output.status.success() {
            tracing::info!("officecli update available, installing...");
            let mut install_builder = CmdBuilder::clean_cli("npm");
            install_builder.args(["install", "-g", "officecli@latest"]);
            let install = install_builder
                .output()
                .await
                .map_err(|e| OfficeError::InstallFailed(e.to_string()))?;

            if !install.status.success() {
                let stderr = String::from_utf8_lossy(&install.stderr);
                tracing::warn!("officecli update failed: {stderr}");
            }
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn resolve_path(file_path: &str) -> Result<String, OfficeError> {
    let path = std::path::Path::new(file_path);
    let resolved = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    Ok(resolved.to_string_lossy().into_owned())
}

fn session_key(resolved_path: &str, doc_type: DocType) -> String {
    format!("{doc_type}:{resolved_path}")
}

fn require_owner(owner_id: &str) -> Result<(), OfficeError> {
    UserId::parse(owner_id)
        .map(|_| ())
        .map_err(|error| OfficeError::StartFailed(format!("invalid preview owner: {error}")))
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};

    struct MockProcessHandle {
        alive: AtomicBool,
        killed: AtomicBool,
    }

    impl MockProcessHandle {
        fn new() -> Self {
            Self {
                alive: AtomicBool::new(true),
                killed: AtomicBool::new(false),
            }
        }
    }

    impl ProcessHandle for MockProcessHandle {
        fn kill(&self) {
            self.alive.store(false, Ordering::SeqCst);
            self.killed.store(true, Ordering::SeqCst);
        }

        fn is_alive(&self) -> bool {
            self.alive.load(Ordering::SeqCst)
        }
    }

    struct MockSpawner {
        installed: AtomicBool,
        spawn_count: AtomicU32,
        install_count: AtomicU32,
        update_count: AtomicU32,
        fail_spawn: AtomicBool,
        start_listener: AtomicBool,
    }

    impl MockSpawner {
        fn new() -> Self {
            Self {
                installed: AtomicBool::new(true),
                spawn_count: AtomicU32::new(0),
                install_count: AtomicU32::new(0),
                update_count: AtomicU32::new(0),
                fail_spawn: AtomicBool::new(false),
                start_listener: AtomicBool::new(true),
            }
        }
    }

    #[async_trait::async_trait]
    impl ProcessSpawner for MockSpawner {
        async fn spawn_officecli(
            &self,
            _file_path: &str,
            port: u16,
            _doc_type: DocType,
        ) -> Result<Box<dyn ProcessHandle>, OfficeError> {
            self.spawn_count.fetch_add(1, Ordering::SeqCst);

            if self.fail_spawn.load(Ordering::SeqCst) {
                return Err(OfficeError::StartFailed("mock spawn failure".into()));
            }

            if !self.installed.load(Ordering::SeqCst) {
                return Err(OfficeError::OfficecliNotFound);
            }

            if self.start_listener.load(Ordering::SeqCst) {
                let listener = std::net::TcpListener::bind(format!("127.0.0.1:{port}"))
                    .map_err(|e| OfficeError::StartFailed(e.to_string()))?;
                std::mem::forget(listener);
            }

            Ok(Box::new(MockProcessHandle::new()))
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
            self.update_count.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
    }

    struct RecordingBroadcaster {
        events: std::sync::Mutex<Vec<WebSocketMessage<serde_json::Value>>>,
        owners: std::sync::Mutex<Vec<String>>,
    }

    impl RecordingBroadcaster {
        fn new() -> Self {
            Self {
                events: std::sync::Mutex::new(Vec::new()),
                owners: std::sync::Mutex::new(Vec::new()),
            }
        }

        fn events(&self) -> Vec<WebSocketMessage<serde_json::Value>> {
            self.events.lock().unwrap().clone()
        }
    }

    impl UserEventSink for RecordingBroadcaster {
        fn send_to_user(&self, user_id: &str, event: WebSocketMessage<serde_json::Value>) {
            self.owners.lock().unwrap().push(user_id.to_owned());
            self.events.lock().unwrap().push(event);
        }
    }

    fn make_manager(
        spawner: Arc<MockSpawner>,
        broadcaster: Arc<RecordingBroadcaster>,
    ) -> OfficecliWatchManager {
        OfficecliWatchManager::new(spawner, broadcaster)
    }

    #[test]
    fn session_key_format() {
        let key = session_key("/path/to/doc.docx", DocType::Word);
        assert_eq!(key, "word:/path/to/doc.docx");
    }

    #[test]
    fn session_key_different_doc_types() {
        let k1 = session_key("/a.docx", DocType::Word);
        let k2 = session_key("/a.docx", DocType::Excel);
        assert_ne!(k1, k2);
    }

    #[tokio::test]
    async fn start_creates_session() {
        let spawner = Arc::new(MockSpawner::new());
        let broadcaster = Arc::new(RecordingBroadcaster::new());
        let mgr = make_manager(Arc::clone(&spawner), Arc::clone(&broadcaster));

        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("test.docx");
        std::fs::write(&file, b"test").unwrap();

        let access = mgr
            .start("0190f5fe-7c00-7a00-8abc-012345678901", file.to_str().unwrap(), DocType::Word)
            .await
            .unwrap();
        assert!(access.port > 0);
        assert!(is_preview_capability(&access.capability));
        assert_eq!(access.doc_type, DocType::Word);
        assert_eq!(
            mgr.resolve_capability(&access.capability),
            Some(PreviewProxyTarget {
                port: access.port,
                doc_type: DocType::Word,
            })
        );
        assert_eq!(mgr.active_session_count(), 1);
        assert_eq!(spawner.spawn_count.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn start_reuses_existing_session() {
        let spawner = Arc::new(MockSpawner::new());
        let broadcaster = Arc::new(RecordingBroadcaster::new());
        let mgr = make_manager(Arc::clone(&spawner), Arc::clone(&broadcaster));

        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("test.docx");
        std::fs::write(&file, b"test").unwrap();

        let first = mgr
            .start("0190f5fe-7c00-7a00-8abc-012345678901", file.to_str().unwrap(), DocType::Word)
            .await
            .unwrap();
        let second = mgr
            .start("0190f5fe-7c00-7a00-8abc-012345678902", file.to_str().unwrap(), DocType::Word)
            .await
            .unwrap();

        assert_eq!(first.port, second.port);
        assert_ne!(first.capability, second.capability);
        assert_eq!(spawner.spawn_count.load(Ordering::SeqCst), 1);
        assert!(mgr.resolve_capability(&first.capability).is_some());
        assert!(mgr.resolve_capability(&second.capability).is_some());

        mgr.stop(
            "0190f5fe-7c00-7a00-8abc-012345678901",
            DocType::Word,
            &first.capability,
        )
        .await;
        assert!(mgr.resolve_capability(&first.capability).is_none());
        assert!(mgr.resolve_capability(&second.capability).is_some());
        assert_eq!(mgr.active_session_count(), 1);
    }

    #[tokio::test]
    async fn start_different_doc_types_independent() {
        let spawner = Arc::new(MockSpawner::new());
        let broadcaster = Arc::new(RecordingBroadcaster::new());
        let mgr = make_manager(Arc::clone(&spawner), Arc::clone(&broadcaster));

        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("test.docx");
        std::fs::write(&file, b"test").unwrap();

        let p1 = mgr
            .start("0190f5fe-7c00-7a00-8abc-012345678901", file.to_str().unwrap(), DocType::Word)
            .await
            .unwrap();
        let p2 = mgr
            .start("0190f5fe-7c00-7a00-8abc-012345678901", file.to_str().unwrap(), DocType::Excel)
            .await
            .unwrap();

        assert_ne!(p1.port, p2.port);
        assert_eq!(mgr.active_session_count(), 2);
        assert_eq!(spawner.spawn_count.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn stop_removes_session() {
        let spawner = Arc::new(MockSpawner::new());
        let broadcaster = Arc::new(RecordingBroadcaster::new());
        let mgr = make_manager(Arc::clone(&spawner), Arc::clone(&broadcaster));

        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("test.docx");
        std::fs::write(&file, b"test").unwrap();
        let path = file.to_str().unwrap();

        let access = mgr.start("0190f5fe-7c00-7a00-8abc-012345678901", path, DocType::Word).await.unwrap();
        assert_eq!(mgr.active_session_count(), 1);

        mgr.stop("0190f5fe-7c00-7a00-8abc-012345678901", DocType::Word, &access.capability)
            .await;
        assert!(mgr.resolve_capability(&access.capability).is_none());
        assert_eq!(mgr.active_session_count(), 0);
    }

    #[tokio::test]
    async fn stop_all_clears_everything() {
        let spawner = Arc::new(MockSpawner::new());
        let broadcaster = Arc::new(RecordingBroadcaster::new());
        let mgr = make_manager(Arc::clone(&spawner), Arc::clone(&broadcaster));

        let dir = tempfile::tempdir().unwrap();
        let f1 = dir.path().join("a.docx");
        let f2 = dir.path().join("b.xlsx");
        std::fs::write(&f1, b"a").unwrap();
        std::fs::write(&f2, b"b").unwrap();

        let word = mgr
            .start("0190f5fe-7c00-7a00-8abc-012345678901", f1.to_str().unwrap(), DocType::Word)
            .await
            .unwrap();
        let excel = mgr
            .start("0190f5fe-7c00-7a00-8abc-012345678901", f2.to_str().unwrap(), DocType::Excel)
            .await
            .unwrap();
        assert_eq!(mgr.active_session_count(), 2);

        mgr.stop_all().await;
        assert_eq!(mgr.active_session_count(), 0);
        assert!(mgr.resolve_capability(&word.capability).is_none());
        assert!(mgr.resolve_capability(&excel.capability).is_none());
    }

    #[tokio::test]
    async fn capability_resolution_rejects_guesses_and_noncanonical_tokens() {
        let spawner = Arc::new(MockSpawner::new());
        let broadcaster = Arc::new(RecordingBroadcaster::new());
        let mgr = make_manager(Arc::clone(&spawner), Arc::clone(&broadcaster));

        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("test.docx");
        std::fs::write(&file, b"test").unwrap();

        let access = mgr
            .start("0190f5fe-7c00-7a00-8abc-012345678901", file.to_str().unwrap(), DocType::Word)
            .await
            .unwrap();
        assert!(mgr.resolve_capability(&access.capability).is_some());
        assert!(mgr.resolve_capability("8080").is_none());
        assert!(mgr.resolve_capability(&"A".repeat(64)).is_none());
        assert!(mgr.resolve_capability(&"0".repeat(64)).is_none());
    }

    #[tokio::test]
    async fn capability_resolution_preserves_document_type() {
        let spawner = Arc::new(MockSpawner::new());
        let broadcaster = Arc::new(RecordingBroadcaster::new());
        let mgr = make_manager(Arc::clone(&spawner), Arc::clone(&broadcaster));

        let dir = tempfile::tempdir().unwrap();
        let word_file = dir.path().join("test.docx");
        let excel_file = dir.path().join("test.xlsx");
        let ppt_file = dir.path().join("test.pptx");
        std::fs::write(&word_file, b"w").unwrap();
        std::fs::write(&excel_file, b"e").unwrap();
        std::fs::write(&ppt_file, b"p").unwrap();

        let word = mgr
            .start("0190f5fe-7c00-7a00-8abc-012345678901", word_file.to_str().unwrap(), DocType::Word)
            .await
            .unwrap();
        let excel = mgr
            .start("0190f5fe-7c00-7a00-8abc-012345678901", excel_file.to_str().unwrap(), DocType::Excel)
            .await
            .unwrap();
        let ppt = mgr
            .start("0190f5fe-7c00-7a00-8abc-012345678901", ppt_file.to_str().unwrap(), DocType::Ppt)
            .await
            .unwrap();

        assert_eq!(
            mgr.resolve_capability(&word.capability).unwrap().doc_type,
            DocType::Word
        );
        assert_eq!(
            mgr.resolve_capability(&excel.capability).unwrap().doc_type,
            DocType::Excel
        );
        assert_eq!(
            mgr.resolve_capability(&ppt.capability).unwrap().doc_type,
            DocType::Ppt
        );
    }

    #[tokio::test]
    async fn stop_requires_exact_owner_and_capability() {
        let spawner = Arc::new(MockSpawner::new());
        let broadcaster = Arc::new(RecordingBroadcaster::new());
        let mgr = make_manager(Arc::clone(&spawner), Arc::clone(&broadcaster));

        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("test.docx");
        std::fs::write(&file, b"test").unwrap();
        let path = file.to_str().unwrap();
        let access = mgr.start("0190f5fe-7c00-7a00-8abc-012345678901", path, DocType::Word).await.unwrap();

        mgr.stop("0190f5fe-7c00-7a00-8abc-012345678902", DocType::Word, &access.capability)
            .await;
        assert!(mgr.resolve_capability(&access.capability).is_some());

        mgr.stop("0190f5fe-7c00-7a00-8abc-012345678901", DocType::Word, &"0".repeat(64))
            .await;
        assert!(mgr.resolve_capability(&access.capability).is_some());

        mgr.stop("0190f5fe-7c00-7a00-8abc-012345678901", DocType::Word, &access.capability)
            .await;
        assert!(mgr.resolve_capability(&access.capability).is_none());
    }

    #[tokio::test]
    async fn auto_install_when_not_found() {
        let spawner = Arc::new(MockSpawner::new());
        spawner.installed.store(false, Ordering::SeqCst);
        let broadcaster = Arc::new(RecordingBroadcaster::new());
        let mgr = make_manager(Arc::clone(&spawner), Arc::clone(&broadcaster));

        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("test.docx");
        std::fs::write(&file, b"test").unwrap();

        let access = mgr
            .start("0190f5fe-7c00-7a00-8abc-012345678901", file.to_str().unwrap(), DocType::Word)
            .await
            .unwrap();
        assert!(access.port > 0);
        assert_eq!(spawner.install_count.load(Ordering::SeqCst), 1);
        // First spawn fails (not installed), then install, then second spawn succeeds
        assert_eq!(spawner.spawn_count.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn broadcasts_starting_and_ready() {
        let spawner = Arc::new(MockSpawner::new());
        let broadcaster = Arc::new(RecordingBroadcaster::new());
        let mgr = make_manager(Arc::clone(&spawner), Arc::clone(&broadcaster));

        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("test.docx");
        std::fs::write(&file, b"test").unwrap();

        mgr.start("0190f5fe-7c00-7a00-8abc-012345678901", file.to_str().unwrap(), DocType::Word)
            .await
            .unwrap();

        let events = broadcaster.events();
        assert!(
            broadcaster
                .owners
                .lock()
                .unwrap()
                .iter()
                .all(|owner| owner == "0190f5fe-7c00-7a00-8abc-012345678901")
        );
        assert!(events.len() >= 2);
        assert_eq!(events[0].name, "word-preview.status");
        assert_eq!(events[0].data["state"], "starting");
        assert_eq!(events[1].name, "word-preview.status");
        assert_eq!(events[1].data["state"], "ready");
    }

    #[tokio::test]
    async fn broadcasts_installing_on_auto_install() {
        let spawner = Arc::new(MockSpawner::new());
        spawner.installed.store(false, Ordering::SeqCst);
        let broadcaster = Arc::new(RecordingBroadcaster::new());
        let mgr = make_manager(Arc::clone(&spawner), Arc::clone(&broadcaster));

        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("test.docx");
        std::fs::write(&file, b"test").unwrap();

        mgr.start("0190f5fe-7c00-7a00-8abc-012345678901", file.to_str().unwrap(), DocType::Word)
            .await
            .unwrap();

        let events = broadcaster.events();
        let states: Vec<&str> = events
            .iter()
            .filter_map(|e| e.data["state"].as_str())
            .collect();
        assert!(states.contains(&"starting"));
        assert!(states.contains(&"installing"));
        assert!(states.contains(&"ready"));
    }

    #[tokio::test]
    async fn broadcasts_error_on_failure() {
        let spawner = Arc::new(MockSpawner::new());
        spawner.fail_spawn.store(true, Ordering::SeqCst);
        let broadcaster = Arc::new(RecordingBroadcaster::new());
        let mgr = make_manager(Arc::clone(&spawner), Arc::clone(&broadcaster));

        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("test.docx");
        std::fs::write(&file, b"test").unwrap();

        let result = mgr
            .start("0190f5fe-7c00-7a00-8abc-012345678901", file.to_str().unwrap(), DocType::Word)
            .await;
        assert!(result.is_err());

        let events = broadcaster.events();
        let last = events.last().unwrap();
        assert_eq!(last.data["state"], "error");
    }

    #[tokio::test]
    async fn port_timeout_on_no_listener() {
        let spawner = Arc::new(MockSpawner::new());
        spawner.start_listener.store(false, Ordering::SeqCst);
        let broadcaster = Arc::new(RecordingBroadcaster::new());
        let mgr = make_manager(Arc::clone(&spawner), Arc::clone(&broadcaster));

        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("test.docx");
        std::fs::write(&file, b"test").unwrap();

        let resolved = resolve_path(file.to_str().unwrap()).unwrap();
        let port = allocate_port().unwrap();
        let result = mgr.poll_port_ready(port, &resolved).await;
        assert!(matches!(result, Err(OfficeError::PortTimeout(_))));
    }

    #[tokio::test]
    async fn ppt_triggers_version_check() {
        let spawner = Arc::new(MockSpawner::new());
        let broadcaster = Arc::new(RecordingBroadcaster::new());
        let mgr = make_manager(Arc::clone(&spawner), Arc::clone(&broadcaster));

        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("test.pptx");
        std::fs::write(&file, b"test").unwrap();

        mgr.start("0190f5fe-7c00-7a00-8abc-012345678901", file.to_str().unwrap(), DocType::Ppt)
            .await
            .unwrap();

        // Give the spawned task a moment
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(spawner.update_count.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn word_does_not_trigger_version_check() {
        let spawner = Arc::new(MockSpawner::new());
        let broadcaster = Arc::new(RecordingBroadcaster::new());
        let mgr = make_manager(Arc::clone(&spawner), Arc::clone(&broadcaster));

        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("test.docx");
        std::fs::write(&file, b"test").unwrap();

        mgr.start("0190f5fe-7c00-7a00-8abc-012345678901", file.to_str().unwrap(), DocType::Word)
            .await
            .unwrap();

        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(spawner.update_count.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn stop_unknown_capability_is_no_op() {
        let spawner = Arc::new(MockSpawner::new());
        let broadcaster = Arc::new(RecordingBroadcaster::new());
        let mgr = make_manager(spawner, broadcaster);

        mgr.stop("0190f5fe-7c00-7a00-8abc-012345678901", DocType::Word, &"0".repeat(64))
            .await;
        assert_eq!(mgr.active_session_count(), 0);
    }

    #[test]
    fn resolve_path_normalizes() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("test.docx");
        std::fs::write(&file, b"test").unwrap();

        let resolved = resolve_path(file.to_str().unwrap()).unwrap();
        assert!(!resolved.is_empty());
    }

    #[test]
    fn resolve_path_nonexistent_returns_original() {
        let result = resolve_path("/nonexistent/path/test.docx").unwrap();
        assert_eq!(result, "/nonexistent/path/test.docx");
    }
}

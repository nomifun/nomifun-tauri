//! Owner-only local broker used by unmanaged Claude/Gemini/Codex MCP
//! registrations.
//!
//! Config files contain only the stable `nomicore mcp-knowledge-stdio`
//! command. On each process start this broker authenticates the local OS peer,
//! accepts only protocol version plus the process' current working directory,
//! and resolves every authorization field inside the main process. The control
//! connection remains open for the lifetime of the stdio bridge; EOF revokes
//! its renewable capability immediately.

use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex, Weak};

use nomifun_api_types::{
    KnowledgeCapabilityClaims, KnowledgeMcpConfig, ScopedMcpChildBootstrap,
};
use nomifun_common::{LoopbackCapabilityLease, generate_id};
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

use crate::service::{KnowledgeService, WriteMode, WriteSurface, resolve_write_policy};

const BROKER_PROTOCOL_VERSION: u16 = 1;
const MAX_BROKER_MESSAGE_BYTES: usize = 16 * 1024;
const MAX_CONCURRENT_EXTERNAL_PROCESSES: usize = 32;

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct BrokerRequest {
    version: u16,
    cwd: String,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
enum BrokerResponse {
    Ok {
        bootstrap: ScopedMcpChildBootstrap<KnowledgeCapabilityClaims>,
    },
    Error {
        code: String,
        message: String,
    },
}

/// Client-side control channel. It intentionally exposes no raw transport;
/// keeping this value alive is the lease heartbeat, and dropping it produces
/// EOF in the main process which revokes the capability.
pub struct ExternalKnowledgeBrokerConnection {
    bootstrap: ScopedMcpChildBootstrap<KnowledgeCapabilityClaims>,
    _control: platform::ClientStream,
}

impl ExternalKnowledgeBrokerConnection {
    pub fn bootstrap(&self) -> &ScopedMcpChildBootstrap<KnowledgeCapabilityClaims> {
        &self.bootstrap
    }
}

/// Connect an unmanaged MCP stdio process to the installation owner's local
/// broker. The only caller-controlled authorization input is the current cwd;
/// the broker canonicalizes it and resolves all persisted scope server-side.
pub async fn connect_external_knowledge_broker(
    cwd: &Path,
) -> Result<ExternalKnowledgeBrokerConnection, String> {
    platform::connect(cwd).await
}

/// Main-process broker lifetime. The final `stop`/drop synchronously revokes
/// every issued lease before aborting transport tasks, so a backend restart
/// cannot leave an old external process renewable.
pub(crate) struct KnowledgeBroker {
    task: Option<tokio::task::JoinHandle<()>>,
    leases: Arc<Mutex<HashMap<String, LoopbackCapabilityLease>>>,
    cleanup: platform::ServerCleanup,
}

impl KnowledgeBroker {
    pub(crate) async fn start(
        config: KnowledgeMcpConfig,
        service: Weak<KnowledgeService>,
        installation_owner_id: String,
    ) -> Result<Self, String> {
        platform::start(config, service, installation_owner_id).await
    }

    fn stop(&mut self) {
        let mut leases = self
            .leases
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        for lease in leases.values() {
            lease.revoke();
        }
        leases.clear();
        drop(leases);
        if let Some(task) = self.task.take() {
            task.abort();
        }
        self.cleanup.cleanup();
    }
}

impl Drop for KnowledgeBroker {
    fn drop(&mut self) {
        self.stop();
    }
}

async fn serve_authenticated_connection<S>(
    mut stream: S,
    config: KnowledgeMcpConfig,
    service: Weak<KnowledgeService>,
    installation_owner_id: Arc<str>,
    leases: Arc<Mutex<HashMap<String, LoopbackCapabilityLease>>>,
    _permit: tokio::sync::OwnedSemaphorePermit,
) where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let request = match read_request(&mut stream).await {
        Ok(request) => request,
        Err(error) => {
            let _ = send_error(&mut stream, "invalid_request", error).await;
            return;
        }
    };
    if request.version != BROKER_PROTOCOL_VERSION {
        let _ = send_error(
            &mut stream,
            "unsupported_version",
            format!(
                "unsupported broker protocol version {}; expected {}",
                request.version, BROKER_PROTOCOL_VERSION
            ),
        )
        .await;
        return;
    }

    let canonical_cwd = match canonical_workspace(&request.cwd) {
        Ok(path) => path,
        Err(error) => {
            let _ = send_error(&mut stream, "invalid_cwd", error).await;
            return;
        }
    };
    let Some(service) = service.upgrade() else {
        let _ = send_error(
            &mut stream,
            "service_unavailable",
            "knowledge service is not available".into(),
        )
        .await;
        return;
    };

    let canonical_cwd_string = canonical_cwd.to_string_lossy().into_owned();
    let (kb_ids, binding, workpath_key) = service
        .resolve_write_context_for_cwd(&canonical_cwd_string)
        .await;
    // This is an ephemeral session identity, not a display label. Keep it in
    // the same canonical UUIDv7 shape as every other v3 business/session ID.
    let process_session_id = generate_id();
    let policy = resolve_write_policy(
        WriteSurface::TerminalAcp,
        &binding,
        &workpath_key,
    );
    // Falling back to all registered bases is a read-only convenience for an
    // unbound/empty workspace. Write authority requires a real, non-empty
    // persisted binding in addition to its writeback policy.
    let has_bound_scope = binding.enabled && !binding.kb_ids.is_empty();
    let allow_write = has_bound_scope && !matches!(policy.mode, WriteMode::Disabled);
    let child = match config.issue_for_external_process(
        &installation_owner_id,
        &process_session_id,
        &canonical_cwd_string,
        &kb_ids,
        allow_write,
    ) {
        Ok(child) => child,
        Err(error) => {
            let _ = send_error(
                &mut stream,
                "scope_unavailable",
                format!("could not issue knowledge scope: {error}"),
            )
            .await;
            return;
        }
    };

    let lease_id = child.bootstrap.access.claims.lease_id.clone();
    let lease = child.lease.clone();
    leases
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .insert(lease_id.clone(), lease.clone());

    let response = BrokerResponse::Ok {
        bootstrap: child.bootstrap,
    };
    if send_response(&mut stream, &response).await.is_err() {
        lease.revoke();
        leases
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remove(&lease_id);
        return;
    }

    // This is a control channel, not a second command channel. EOF, any extra
    // bytes, or a transport error all terminate and revoke the lease.
    let mut unexpected = [0_u8; 1];
    let _ = stream.read(&mut unexpected).await;
    lease.revoke();
    leases
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .remove(&lease_id);
}

fn canonical_workspace(raw: &str) -> Result<std::path::PathBuf, String> {
    if raw.is_empty() || raw.trim() != raw {
        return Err("cwd must be a non-empty exact path".into());
    }
    let path = std::fs::canonicalize(raw)
        .map_err(|error| format!("cwd does not exist or cannot be canonicalized: {error}"))?;
    let metadata = std::fs::metadata(&path)
        .map_err(|error| format!("could not inspect canonical cwd: {error}"))?;
    if !metadata.is_dir() {
        return Err("cwd must resolve to a directory".into());
    }
    Ok(path)
}

async fn read_request<S>(stream: &mut S) -> Result<BrokerRequest, String>
where
    S: AsyncRead + Unpin,
{
    let line = read_single_frame(stream, "broker request").await?;
    serde_json::from_slice(&line).map_err(|error| format!("invalid broker request JSON: {error}"))
}

async fn read_single_frame<S>(stream: &mut S, label: &str) -> Result<Vec<u8>, String>
where
    S: AsyncRead + Unpin,
{
    let mut frame = Vec::new();
    let mut chunk = [0_u8; 1024];
    loop {
        let remaining = MAX_BROKER_MESSAGE_BYTES.saturating_sub(frame.len());
        if remaining == 0 {
            return Err(format!("{label} exceeds size limit or is not newline terminated"));
        }
        let read_limit = remaining.min(chunk.len());
        let read = stream
            .read(&mut chunk[..read_limit])
            .await
            .map_err(|error| format!("could not read {label}: {error}"))?;
        if read == 0 {
            return Err(format!("{label} must be newline terminated"));
        }
        if let Some(newline) = chunk[..read].iter().position(|byte| *byte == b'\n') {
            frame.extend_from_slice(&chunk[..=newline]);
            if newline + 1 != read {
                return Err(format!("{label} contains trailing bytes"));
            }
            return Ok(frame);
        }
        frame.extend_from_slice(&chunk[..read]);
    }
}

async fn send_error<S>(stream: &mut S, code: &'static str, message: String) -> Result<(), String>
where
    S: AsyncWrite + Unpin,
{
    send_response(
        stream,
        &BrokerResponse::Error {
            code: code.to_owned(),
            message,
        },
    )
    .await
}

async fn send_response<S>(stream: &mut S, response: &BrokerResponse) -> Result<(), String>
where
    S: AsyncWrite + Unpin,
{
    let mut bytes = serde_json::to_vec(response)
        .map_err(|error| format!("could not encode broker response: {error}"))?;
    bytes.push(b'\n');
    stream
        .write_all(&bytes)
        .await
        .map_err(|error| format!("could not write broker response: {error}"))?;
    stream
        .flush()
        .await
        .map_err(|error| format!("could not flush broker response: {error}"))
}

async fn read_response<S>(stream: &mut S) -> Result<ScopedMcpChildBootstrap<KnowledgeCapabilityClaims>, String>
where
    S: AsyncRead + Unpin,
{
    let line = read_single_frame(stream, "broker response").await?;
    match serde_json::from_slice::<BrokerResponse>(&line)
        .map_err(|error| format!("invalid broker response JSON: {error}"))?
    {
        BrokerResponse::Ok { bootstrap } => Ok(bootstrap),
        BrokerResponse::Error { code, message } => Err(format!("broker {code}: {message}")),
    }
}

#[cfg(unix)]
mod platform {
    use std::fs::{DirBuilder, Permissions};
    use std::io;
    use std::os::fd::AsRawFd;
    use std::os::unix::fs::{DirBuilderExt, FileTypeExt, MetadataExt, PermissionsExt};
    use std::path::{Path, PathBuf};
    use std::sync::{Arc, Mutex, Weak};

    use nomifun_api_types::KnowledgeMcpConfig;
    use nomifun_common::LoopbackCapabilityLease;
    use tokio::net::{UnixListener, UnixStream};
    use tokio::sync::Semaphore;
    use tokio::task::JoinSet;

    use super::{
        BROKER_PROTOCOL_VERSION, BrokerRequest, ExternalKnowledgeBrokerConnection,
        KnowledgeBroker, MAX_CONCURRENT_EXTERNAL_PROCESSES, read_response,
        serve_authenticated_connection,
    };
    use crate::service::KnowledgeService;

    const RUNTIME_DIR_PREFIX: &str = "nomifun-knowledge-broker";
    const SOCKET_FILE_NAME: &str = "control.sock";

    pub type ClientStream = UnixStream;

    pub struct ServerCleanup {
        socket_path: PathBuf,
    }

    impl ServerCleanup {
        pub fn cleanup(&self) {
            match std::fs::remove_file(&self.socket_path) {
                Ok(()) => {}
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => tracing::warn!(path = %self.socket_path.display(), %error, "failed to remove knowledge broker socket"),
            }
        }
    }

    pub async fn start(
        config: KnowledgeMcpConfig,
        service: Weak<KnowledgeService>,
        installation_owner_id: String,
    ) -> Result<KnowledgeBroker, String> {
        start_at(
            production_socket_path(),
            config,
            service,
            installation_owner_id,
        )
        .await
    }

    async fn start_at(
        socket_path: PathBuf,
        config: KnowledgeMcpConfig,
        service: Weak<KnowledgeService>,
        installation_owner_id: String,
    ) -> Result<KnowledgeBroker, String> {
        prepare_socket_parent(&socket_path)?;
        remove_stale_socket(&socket_path)?;
        let listener = UnixListener::bind(&socket_path)
            .map_err(|error| format!("failed to bind knowledge broker socket: {error}"))?;
        std::fs::set_permissions(&socket_path, Permissions::from_mode(0o600))
            .map_err(|error| format!("failed to protect knowledge broker socket: {error}"))?;
        validate_socket(&socket_path, effective_uid())?;

        let leases = Arc::new(Mutex::new(std::collections::HashMap::<
            String,
            LoopbackCapabilityLease,
        >::new()));
        let task_leases = leases.clone();
        let owner: Arc<str> = Arc::from(installation_owner_id);
        let semaphore = Arc::new(Semaphore::new(MAX_CONCURRENT_EXTERNAL_PROCESSES));
        let task = tokio::spawn(async move {
            let mut connections = JoinSet::new();
            loop {
                tokio::select! {
                    accepted = listener.accept() => {
                        let (mut stream, _) = match accepted {
                            Ok(value) => value,
                            Err(error) => {
                                tracing::warn!(%error, "knowledge broker accept failed");
                                break;
                            }
                        };
                        if let Err(error) = verify_peer_uid(&stream, effective_uid()) {
                            tracing::warn!(%error, "knowledge broker rejected foreign Unix peer");
                            continue;
                        }
                        let permit = match semaphore.clone().try_acquire_owned() {
                            Ok(permit) => permit,
                            Err(_) => {
                                let _ = super::send_error(
                                    &mut stream,
                                    "concurrency_limit",
                                    "too many concurrent external knowledge processes".into(),
                                ).await;
                                continue;
                            }
                        };
                        connections.spawn(serve_authenticated_connection(
                            stream,
                            config.clone(),
                            service.clone(),
                            owner.clone(),
                            task_leases.clone(),
                            permit,
                        ));
                    }
                    completed = connections.join_next(), if !connections.is_empty() => {
                        if let Some(Err(error)) = completed {
                            tracing::warn!(%error, "knowledge broker connection task failed");
                        }
                    }
                }
            }
        });

        Ok(KnowledgeBroker {
            task: Some(task),
            leases,
            cleanup: ServerCleanup { socket_path },
        })
    }

    pub async fn connect(cwd: &Path) -> Result<ExternalKnowledgeBrokerConnection, String> {
        connect_at(&production_socket_path(), cwd).await
    }

    async fn connect_at(
        socket_path: &Path,
        cwd: &Path,
    ) -> Result<ExternalKnowledgeBrokerConnection, String> {
        let expected_uid = effective_uid();
        let parent = socket_path
            .parent()
            .ok_or_else(|| "knowledge broker socket has no parent".to_string())?;
        validate_runtime_dir(parent, expected_uid)?;
        validate_socket(socket_path, expected_uid)?;
        let mut control = UnixStream::connect(socket_path)
            .await
            .map_err(|error| format!("could not connect to knowledge broker: {error}"))?;
        verify_peer_uid(&control, expected_uid)?;
        let cwd = cwd
            .to_str()
            .ok_or_else(|| "cwd is not valid UTF-8".to_string())?;
        let mut request = serde_json::to_vec(&BrokerRequest {
            version: BROKER_PROTOCOL_VERSION,
            cwd: cwd.to_owned(),
        })
        .map_err(|error| format!("could not encode broker request: {error}"))?;
        request.push(b'\n');
        use tokio::io::AsyncWriteExt;
        control
            .write_all(&request)
            .await
            .map_err(|error| format!("could not send broker request: {error}"))?;
        control
            .flush()
            .await
            .map_err(|error| format!("could not flush broker request: {error}"))?;
        let bootstrap = read_response(&mut control).await?;
        Ok(ExternalKnowledgeBrokerConnection {
            bootstrap,
            _control: control,
        })
    }

    fn production_socket_path() -> PathBuf {
        std::env::temp_dir()
            .join(format!("{RUNTIME_DIR_PREFIX}-{}", effective_uid()))
            .join(SOCKET_FILE_NAME)
    }

    fn prepare_socket_parent(socket_path: &Path) -> Result<(), String> {
        let parent = socket_path
            .parent()
            .ok_or_else(|| "knowledge broker socket has no parent".to_string())?;
        match std::fs::symlink_metadata(parent) {
            Ok(metadata) => {
                if !metadata.file_type().is_dir() || metadata.file_type().is_symlink() {
                    return Err("knowledge broker runtime path is not a real directory".into());
                }
                if metadata.uid() != effective_uid() {
                    return Err("knowledge broker runtime directory has a foreign owner".into());
                }
                std::fs::set_permissions(parent, Permissions::from_mode(0o700))
                    .map_err(|error| format!("failed to protect broker runtime directory: {error}"))?;
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                let mut builder = DirBuilder::new();
                builder.mode(0o700);
                builder
                    .create(parent)
                    .map_err(|error| format!("failed to create broker runtime directory: {error}"))?;
            }
            Err(error) => {
                return Err(format!("failed to inspect broker runtime directory: {error}"));
            }
        }
        validate_runtime_dir(parent, effective_uid())
    }

    fn remove_stale_socket(path: &Path) -> Result<(), String> {
        let metadata = match std::fs::symlink_metadata(path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(format!("failed to inspect broker socket: {error}")),
        };
        if metadata.file_type().is_symlink()
            || !metadata.file_type().is_socket()
            || metadata.uid() != effective_uid()
        {
            return Err("refusing to replace an unsafe broker socket path".into());
        }
        if std::os::unix::net::UnixStream::connect(path).is_ok() {
            return Err("another knowledge broker is already running".into());
        }
        std::fs::remove_file(path)
            .map_err(|error| format!("failed to remove stale broker socket: {error}"))
    }

    fn validate_runtime_dir(path: &Path, expected_uid: u32) -> Result<(), String> {
        let metadata = std::fs::symlink_metadata(path)
            .map_err(|error| format!("failed to inspect broker runtime directory: {error}"))?;
        if metadata.file_type().is_symlink()
            || !metadata.file_type().is_dir()
            || metadata.uid() != expected_uid
            || metadata.mode() & 0o777 != 0o700
        {
            return Err("knowledge broker runtime directory is not owner-only (0700)".into());
        }
        Ok(())
    }

    fn validate_socket(path: &Path, expected_uid: u32) -> Result<(), String> {
        let metadata = std::fs::symlink_metadata(path)
            .map_err(|error| format!("failed to inspect broker socket: {error}"))?;
        if metadata.file_type().is_symlink()
            || !metadata.file_type().is_socket()
            || metadata.uid() != expected_uid
            || metadata.mode() & 0o777 != 0o600
        {
            return Err("knowledge broker socket is not owner-only (0600)".into());
        }
        Ok(())
    }

    fn effective_uid() -> u32 {
        // SAFETY: geteuid has no preconditions and returns the calling process'
        // effective uid.
        unsafe { libc::geteuid() as u32 }
    }

    fn verify_peer_uid(stream: &UnixStream, expected_uid: u32) -> Result<(), String> {
        let actual = peer_uid(stream).map_err(|error| format!("could not authenticate Unix peer: {error}"))?;
        if actual != expected_uid {
            return Err(format!(
                "Unix peer uid mismatch: expected {expected_uid}, got {actual}"
            ));
        }
        Ok(())
    }

    #[cfg(target_os = "linux")]
    fn peer_uid(stream: &UnixStream) -> io::Result<u32> {
        let mut credential: libc::ucred = unsafe { std::mem::zeroed() };
        let mut length = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
        // SAFETY: pointers target an initialized `ucred` buffer and its exact
        // length; the fd remains owned by `stream` for the call duration.
        let result = unsafe {
            libc::getsockopt(
                stream.as_raw_fd(),
                libc::SOL_SOCKET,
                libc::SO_PEERCRED,
                (&mut credential as *mut libc::ucred).cast(),
                &mut length,
            )
        };
        if result != 0 {
            return Err(io::Error::last_os_error());
        }
        if length as usize != std::mem::size_of::<libc::ucred>() {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "invalid SO_PEERCRED length"));
        }
        Ok(credential.uid)
    }

    #[cfg(any(
        target_os = "macos",
        target_os = "ios",
        target_os = "freebsd",
        target_os = "dragonfly",
        target_os = "netbsd",
        target_os = "openbsd"
    ))]
    fn peer_uid(stream: &UnixStream) -> io::Result<u32> {
        let mut uid: libc::uid_t = 0;
        let mut gid: libc::gid_t = 0;
        // SAFETY: getpeereid writes exactly one uid and gid through valid
        // pointers; the fd remains owned by `stream` for the call duration.
        let result = unsafe { libc::getpeereid(stream.as_raw_fd(), &mut uid, &mut gid) };
        if result != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(uid as u32)
    }

    #[cfg(not(any(
        target_os = "linux",
        target_os = "macos",
        target_os = "ios",
        target_os = "freebsd",
        target_os = "dragonfly",
        target_os = "netbsd",
        target_os = "openbsd"
    )))]
    fn peer_uid(_stream: &UnixStream) -> io::Result<u32> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "this Unix target has no supported peer credential API",
        ))
    }

    #[cfg(test)]
    mod tests {
        use std::sync::Arc;

        use nomifun_api_types::{
            KNOWLEDGE_CAPABILITY_DOMAIN, KNOWLEDGE_WRITE_TOOL, KnowledgeMcpConfig,
        };
        use nomifun_common::{LoopbackCapabilityIssuer, LoopbackSessionKind};

        use super::*;
        use crate::service::{KnowledgeBinding, KnowledgeService};
        use crate::testutil::make_service;

        fn socket_path(temp: &tempfile::TempDir) -> PathBuf {
            temp.path().join("runtime").join("control.sock")
        }

        fn test_config() -> (KnowledgeMcpConfig, Arc<LoopbackCapabilityIssuer>) {
            let issuer = Arc::new(LoopbackCapabilityIssuer::random().unwrap());
            (
                KnowledgeMcpConfig::from_issuer(43210, issuer.clone(), "/bin/nomicore".into()),
                issuer,
            )
        }

        async fn create_base(service: &KnowledgeService, root: &Path, name: &str) -> String {
            let base_root = root.join(name);
            std::fs::create_dir(&base_root).unwrap();
            service
                .create_base(name, "", Some(base_root.to_str().unwrap()), None)
                .await
                .unwrap()
                .knowledge_base_id
                .into_string()
        }

        async fn wait_until_revoked(
            issuer: &LoopbackCapabilityIssuer,
            renewal: &nomifun_common::LoopbackCapabilityRenewalRequest,
        ) {
            tokio::time::timeout(std::time::Duration::from_secs(2), async {
                loop {
                    if issuer
                        .renew::<nomifun_api_types::KnowledgeCapabilityScope>(
                            KNOWLEDGE_CAPABILITY_DOMAIN,
                            renewal,
                        )
                        .is_err()
                    {
                        break;
                    }
                    tokio::task::yield_now().await;
                }
            })
            .await
            .expect("lease should be revoked promptly");
        }

        #[tokio::test]
        async fn peer_uid_and_owner_only_permissions_are_enforced() {
            let (left, _right) = UnixStream::pair().unwrap();
            verify_peer_uid(&left, effective_uid()).unwrap();
            assert!(verify_peer_uid(&left, effective_uid().wrapping_add(1)).is_err());

            let temp = tempfile::tempdir().unwrap();
            let data = temp.path().join("data");
            std::fs::create_dir(&data).unwrap();
            let service = Arc::new(make_service(&data));
            let (config, _) = test_config();
            let socket = socket_path(&temp);
            let broker = start_at(socket.clone(), config, Arc::downgrade(&service), "019abcde-f012-7abc-8abc-0123456789ab".into())
                .await
                .unwrap();
            assert_eq!(std::fs::metadata(socket.parent().unwrap()).unwrap().mode() & 0o777, 0o700);
            assert_eq!(std::fs::metadata(&socket).unwrap().mode() & 0o777, 0o600);
            drop(broker);
        }

        #[tokio::test]
        async fn broker_resolves_isolated_workspaces_and_unbound_read_only_scope() {
            let temp = tempfile::tempdir().unwrap();
            let data = temp.path().join("data");
            let workspace_a = temp.path().join("workspace-a");
            let workspace_b = temp.path().join("workspace-b");
            let unbound = temp.path().join("unbound");
            for path in [&data, &workspace_a, &workspace_b, &unbound] {
                std::fs::create_dir(path).unwrap();
            }
            let canonical_workspace_a = std::fs::canonicalize(&workspace_a).unwrap();
            let canonical_workspace_b = std::fs::canonicalize(&workspace_b).unwrap();
            let canonical_unbound = std::fs::canonicalize(&unbound).unwrap();
            let service = Arc::new(make_service(&data));
            let kb_a = create_base(&service, temp.path(), "base-a").await;
            let kb_b = create_base(&service, temp.path(), "base-b").await;
            let kb_a_id = nomifun_common::KnowledgeBaseId::parse(kb_a).unwrap();
            let kb_b_id = nomifun_common::KnowledgeBaseId::parse(kb_b).unwrap();
            service
                .set_binding(
                    "workpath",
                    canonical_workspace_a.to_str().unwrap(),
                    KnowledgeBinding {
                        enabled: true,
                        writeback: true,
                        writeback_mode: "staged".into(),
                        kb_ids: vec![kb_a_id.clone()],
                        ..KnowledgeBinding::default()
                    },
                )
                .await
                .unwrap();
            service
                .set_binding(
                    "workpath",
                    canonical_workspace_b.to_str().unwrap(),
                    KnowledgeBinding {
                        enabled: true,
                        writeback: false,
                        kb_ids: vec![kb_b_id.clone()],
                        ..KnowledgeBinding::default()
                    },
                )
                .await
                .unwrap();
            service
                .set_binding(
                    "workpath",
                    canonical_unbound.to_str().unwrap(),
                    KnowledgeBinding {
                        enabled: true,
                        writeback: true,
                        kb_ids: Vec::new(),
                        ..KnowledgeBinding::default()
                    },
                )
                .await
                .unwrap();

            let (config, _) = test_config();
            let socket = socket_path(&temp);
            let _broker = start_at(
                socket.clone(),
                config,
                Arc::downgrade(&service),
                "019abcde-f012-7abc-8abc-0123456789ab".into(),
            )
            .await
            .unwrap();
            let connection_a = connect_at(&socket, &workspace_a).await.unwrap();
            let claims_a = &connection_a.bootstrap().access.claims;
            assert_eq!(claims_a.user_id.as_str(), "019abcde-f012-7abc-8abc-0123456789ab");
            assert_eq!(claims_a.session.kind, LoopbackSessionKind::ExternalProcess);
            assert_eq!(claims_a.scope.kb_ids, vec![kb_a_id.clone()]);
            assert!(claims_a.allows(KNOWLEDGE_WRITE_TOOL));

            let connection_b = connect_at(&socket, &workspace_b).await.unwrap();
            let claims_b = &connection_b.bootstrap().access.claims;
            assert_eq!(claims_b.scope.kb_ids, vec![kb_b_id.clone()]);
            assert!(!claims_b.allows(KNOWLEDGE_WRITE_TOOL));

            let unbound_connection = connect_at(&socket, &unbound).await.unwrap();
            let unbound_claims = &unbound_connection.bootstrap().access.claims;
            assert_eq!(unbound_claims.scope.kb_ids, vec![kb_a_id, kb_b_id]);
            assert!(!unbound_claims.allows(KNOWLEDGE_WRITE_TOOL));
        }

        #[tokio::test]
        async fn symlink_cwd_is_canonical_and_eof_or_restart_revokes() {
            use std::os::unix::fs::symlink;

            let temp = tempfile::tempdir().unwrap();
            let data = temp.path().join("data");
            let workspace = temp.path().join("workspace");
            std::fs::create_dir(&data).unwrap();
            std::fs::create_dir(&workspace).unwrap();
            let alias = temp.path().join("alias");
            symlink(&workspace, &alias).unwrap();
            let canonical_workspace = std::fs::canonicalize(&workspace).unwrap();
            let service = Arc::new(make_service(&data));
            let kb = create_base(&service, temp.path(), "base").await;
            service
                .set_binding(
                    "workpath",
                    canonical_workspace.to_str().unwrap(),
                    KnowledgeBinding {
                        enabled: true,
                        kb_ids: vec![nomifun_common::KnowledgeBaseId::parse(kb).unwrap()],
                        ..KnowledgeBinding::default()
                    },
                )
                .await
                .unwrap();
            let (config, issuer) = test_config();
            let socket = socket_path(&temp);
            let broker = start_at(
                socket.clone(),
                config,
                Arc::downgrade(&service),
                "019abcde-f012-7abc-8abc-0123456789ab".into(),
            )
            .await
            .unwrap();

            let connection = connect_at(&socket, &alias).await.unwrap();
            assert_eq!(
                connection.bootstrap().access.claims.scope.workspace_path,
                canonical_workspace.to_string_lossy()
            );
            let renewal = connection.bootstrap().renewal.clone();
            assert!(
                issuer
                    .renew::<nomifun_api_types::KnowledgeCapabilityScope>(
                        KNOWLEDGE_CAPABILITY_DOMAIN,
                        &renewal,
                    )
                    .is_ok()
            );
            drop(connection);
            wait_until_revoked(&issuer, &renewal).await;

            let restart_connection = connect_at(&socket, &workspace).await.unwrap();
            let restart_renewal = restart_connection.bootstrap().renewal.clone();
            drop(broker);
            assert!(
                issuer
                    .renew::<nomifun_api_types::KnowledgeCapabilityScope>(
                        KNOWLEDGE_CAPABILITY_DOMAIN,
                        &restart_renewal,
                    )
                    .is_err(),
                "broker shutdown must synchronously invalidate active leases"
            );
            drop(restart_connection);
        }
    }
}

#[cfg(windows)]
mod platform {
    use std::ffi::c_void;
    use std::io;
    use std::os::windows::io::AsRawHandle;
    use std::path::Path;
    use std::ptr;
    use std::sync::{Arc, Mutex, Weak};
    use std::time::{Duration, Instant};

    use nomifun_api_types::KnowledgeMcpConfig;
    use nomifun_common::LoopbackCapabilityLease;
    use tokio::net::windows::named_pipe::{
        ClientOptions, NamedPipeClient, NamedPipeServer, ServerOptions,
    };
    use tokio::sync::Semaphore;
    use tokio::task::JoinSet;
    use windows_sys::Win32::Foundation::{
        CloseHandle, ERROR_INSUFFICIENT_BUFFER, ERROR_PIPE_BUSY, HANDLE, LocalFree,
    };
    use windows_sys::Win32::Security::Authorization::{
        ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW,
        SDDL_REVISION_1,
    };
    use windows_sys::Win32::Security::{
        CopySid, EqualSid, GetLengthSid, GetTokenInformation, SECURITY_ATTRIBUTES,
        TOKEN_QUERY, TOKEN_USER, TokenUser,
    };
    use windows_sys::Win32::System::Pipes::{
        GetNamedPipeClientProcessId, GetNamedPipeServerProcessId,
    };
    use windows_sys::Win32::System::Threading::{
        GetCurrentProcessId, OpenProcess, OpenProcessToken,
        PROCESS_QUERY_LIMITED_INFORMATION,
    };

    use super::{
        BROKER_PROTOCOL_VERSION, BrokerRequest, ExternalKnowledgeBrokerConnection,
        KnowledgeBroker, MAX_CONCURRENT_EXTERNAL_PROCESSES, read_response,
        serve_authenticated_connection,
    };
    use crate::service::KnowledgeService;

    const PIPE_PREFIX: &str = r"\\.\pipe\nomifun-knowledge-broker";
    const CLIENT_CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

    pub type ClientStream = NamedPipeClient;
    pub struct ServerCleanup;

    impl ServerCleanup {
        pub fn cleanup(&self) {}
    }

    pub async fn start(
        config: KnowledgeMcpConfig,
        service: Weak<KnowledgeService>,
        installation_owner_id: String,
    ) -> Result<KnowledgeBroker, String> {
        let current_sid = process_user_sid(unsafe { GetCurrentProcessId() })?;
        let sid_string = sid_string(&current_sid)?;
        let pipe_name = pipe_name_for_sid(&sid_string);
        start_at(
            pipe_name,
            current_sid,
            config,
            service,
            installation_owner_id,
        )
        .await
    }

    async fn start_at(
        pipe_name: String,
        current_sid: OwnedSid,
        config: KnowledgeMcpConfig,
        service: Weak<KnowledgeService>,
        installation_owner_id: String,
    ) -> Result<KnowledgeBroker, String> {
        let sid_string = sid_string(&current_sid)?;
        let initial_server = create_server(&pipe_name, &sid_string, true)?;
        let leases = Arc::new(Mutex::new(std::collections::HashMap::<
            String,
            LoopbackCapabilityLease,
        >::new()));
        let task_leases = leases.clone();
        let owner: Arc<str> = Arc::from(installation_owner_id);
        let semaphore = Arc::new(Semaphore::new(MAX_CONCURRENT_EXTERNAL_PROCESSES));
        let task = tokio::spawn(async move {
            let mut server = initial_server;
            let mut connections = JoinSet::new();
            loop {
                // Reserve capacity before accepting. The pipe namespace always
                // retains one protected listening instance while at most 32
                // authenticated control connections own permits.
                let permit = match semaphore.clone().acquire_owned().await {
                    Ok(permit) => permit,
                    Err(_) => break,
                };
                if let Err(error) = server.connect().await {
                    tracing::warn!(%error, "knowledge broker named-pipe connect failed");
                    break;
                }
                let next_server = match create_server(&pipe_name, &sid_string, false) {
                    Ok(server) => server,
                    Err(error) => {
                        tracing::warn!(%error, "knowledge broker could not create next named-pipe instance");
                        break;
                    }
                };
                if let Err(error) = verify_client_sid(&server, &current_sid) {
                    tracing::warn!(%error, "knowledge broker rejected foreign Windows peer");
                    server = next_server;
                    continue;
                }
                connections.spawn(serve_authenticated_connection(
                    server,
                    config.clone(),
                    service.clone(),
                    owner.clone(),
                    task_leases.clone(),
                    permit,
                ));
                server = next_server;
                while let Some(result) = connections.try_join_next() {
                    if let Err(error) = result {
                        tracing::warn!(%error, "knowledge broker connection task failed");
                    }
                }
            }
        });
        Ok(KnowledgeBroker {
            task: Some(task),
            leases,
            cleanup: ServerCleanup,
        })
    }

    pub async fn connect(cwd: &Path) -> Result<ExternalKnowledgeBrokerConnection, String> {
        let current_sid = process_user_sid(unsafe { GetCurrentProcessId() })?;
        let sid_string = sid_string(&current_sid)?;
        let pipe_name = pipe_name_for_sid(&sid_string);
        connect_at(&pipe_name, &current_sid, cwd).await
    }

    async fn connect_at(
        pipe_name: &str,
        current_sid: &OwnedSid,
        cwd: &Path,
    ) -> Result<ExternalKnowledgeBrokerConnection, String> {
        let deadline = Instant::now() + CLIENT_CONNECT_TIMEOUT;
        let mut control = loop {
            match ClientOptions::new().open(pipe_name) {
                Ok(client) => break client,
                Err(error)
                    if error.raw_os_error() == Some(ERROR_PIPE_BUSY as i32)
                        && Instant::now() < deadline =>
                {
                    tokio::time::sleep(Duration::from_millis(25)).await;
                }
                Err(error) => {
                    return Err(format!("could not connect to knowledge broker named pipe: {error}"));
                }
            }
        };
        verify_server_sid(&control, current_sid)?;

        let cwd = cwd
            .to_str()
            .ok_or_else(|| "cwd is not valid Unicode".to_string())?;
        let mut request = serde_json::to_vec(&BrokerRequest {
            version: BROKER_PROTOCOL_VERSION,
            cwd: cwd.to_owned(),
        })
        .map_err(|error| format!("could not encode broker request: {error}"))?;
        request.push(b'\n');
        use tokio::io::AsyncWriteExt;
        control
            .write_all(&request)
            .await
            .map_err(|error| format!("could not send broker request: {error}"))?;
        control
            .flush()
            .await
            .map_err(|error| format!("could not flush broker request: {error}"))?;
        let bootstrap = read_response(&mut control).await?;
        Ok(ExternalKnowledgeBrokerConnection {
            bootstrap,
            _control: control,
        })
    }

    fn pipe_name_for_sid(sid: &str) -> String {
        format!("{PIPE_PREFIX}-{sid}")
    }

    fn owner_only_sddl(sid: &str) -> String {
        // Protected DACL, exactly one generic-all ACE for the current user.
        // No Everyone/Users/Administrators fallback is granted.
        format!("D:P(A;;GA;;;{sid})")
    }

    fn create_server(
        pipe_name: &str,
        sid: &str,
        first_instance: bool,
    ) -> Result<NamedPipeServer, String> {
        let sddl = wide_null(&owner_only_sddl(sid));
        let mut descriptor: *mut c_void = ptr::null_mut();
        // SAFETY: `sddl` is a NUL-terminated UTF-16 string and descriptor is a
        // valid out-pointer released with LocalFree below.
        if unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                sddl.as_ptr(),
                SDDL_REVISION_1,
                &mut descriptor,
                ptr::null_mut(),
            )
        } == 0
        {
            return Err(format!(
                "could not build current-user knowledge broker DACL: {}",
                io::Error::last_os_error()
            ));
        }
        let descriptor = LocalAllocation(descriptor);
        let mut attributes = SECURITY_ATTRIBUTES {
            nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: descriptor.0,
            bInheritHandle: 0,
        };
        let mut options = ServerOptions::new();
        options
            .first_pipe_instance(first_instance)
            .reject_remote_clients(true)
            .max_instances(MAX_CONCURRENT_EXTERNAL_PROCESSES + 1);
        // SAFETY: attributes and its descriptor remain live through the create
        // call; CreateNamedPipe copies the security descriptor into the object.
        unsafe {
            options.create_with_security_attributes_raw(
                pipe_name,
                (&mut attributes as *mut SECURITY_ATTRIBUTES).cast(),
            )
        }
        .map_err(|error| format!("failed to create protected knowledge broker pipe: {error}"))
    }

    fn verify_client_sid(server: &NamedPipeServer, expected: &OwnedSid) -> Result<(), String> {
        let mut process_id = 0_u32;
        // SAFETY: server owns a connected named-pipe handle and process_id is a
        // valid out pointer.
        if unsafe {
            GetNamedPipeClientProcessId(server.as_raw_handle() as HANDLE, &mut process_id)
        } == 0
        {
            return Err(format!(
                "could not resolve named-pipe client process: {}",
                io::Error::last_os_error()
            ));
        }
        verify_process_sid(process_id, expected, "client")
    }

    fn verify_server_sid(client: &NamedPipeClient, expected: &OwnedSid) -> Result<(), String> {
        let mut process_id = 0_u32;
        // SAFETY: client owns a connected named-pipe handle and process_id is a
        // valid out pointer.
        if unsafe {
            GetNamedPipeServerProcessId(client.as_raw_handle() as HANDLE, &mut process_id)
        } == 0
        {
            return Err(format!(
                "could not resolve named-pipe server process: {}",
                io::Error::last_os_error()
            ));
        }
        verify_process_sid(process_id, expected, "server")
    }

    fn verify_process_sid(process_id: u32, expected: &OwnedSid, role: &str) -> Result<(), String> {
        let actual = process_user_sid(process_id)?;
        // SAFETY: both pointers reference complete copied SID buffers for the
        // duration of EqualSid.
        if unsafe { EqualSid(actual.as_ptr(), expected.as_ptr()) } == 0 {
            return Err(format!("named-pipe {role} belongs to a different Windows user SID"));
        }
        Ok(())
    }

    #[derive(Clone)]
    struct OwnedSid {
        // usize backing supplies the alignment required by SID APIs.
        storage: Vec<usize>,
    }

    impl OwnedSid {
        fn as_ptr(&self) -> *mut c_void {
            self.storage.as_ptr().cast_mut().cast()
        }
    }

    fn process_user_sid(process_id: u32) -> Result<OwnedSid, String> {
        // SAFETY: OpenProcess returns a new non-inheritable query handle.
        let process = OwnedHandle::new(unsafe {
            OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, process_id)
        })
        .map_err(|error| format!("could not open peer process {process_id}: {error}"))?;
        let mut token: HANDLE = ptr::null_mut();
        // SAFETY: process is valid and token is a valid out-pointer.
        if unsafe { OpenProcessToken(process.0, TOKEN_QUERY, &mut token) } == 0 {
            return Err(format!("could not open peer process token: {}", io::Error::last_os_error()));
        }
        let token = OwnedHandle::new(token)
            .map_err(|error| format!("invalid peer process token: {error}"))?;
        let mut bytes_required = 0_u32;
        // SAFETY: the zero-sized probe intentionally supplies a null buffer.
        let probe = unsafe {
            GetTokenInformation(token.0, TokenUser, ptr::null_mut(), 0, &mut bytes_required)
        };
        if probe != 0
            || io::Error::last_os_error().raw_os_error()
                != Some(ERROR_INSUFFICIENT_BUFFER as i32)
            || bytes_required == 0
        {
            return Err(format!(
                "could not size peer token user information: {}",
                io::Error::last_os_error()
            ));
        }
        let word_bytes = std::mem::size_of::<usize>();
        let mut token_storage = vec![0_usize; (bytes_required as usize + word_bytes - 1) / word_bytes];
        // SAFETY: token_storage is aligned and holds at least bytes_required.
        if unsafe {
            GetTokenInformation(
                token.0,
                TokenUser,
                token_storage.as_mut_ptr().cast(),
                bytes_required,
                &mut bytes_required,
            )
        } == 0
        {
            return Err(format!("could not read peer token user: {}", io::Error::last_os_error()));
        }
        // SAFETY: successful TokenUser output begins with a complete TOKEN_USER.
        let source = unsafe { (*(token_storage.as_ptr() as *const TOKEN_USER)).User.Sid };
        // SAFETY: source comes from the live token information buffer.
        let sid_bytes = unsafe { GetLengthSid(source) };
        if sid_bytes == 0 {
            return Err(format!("could not size peer SID: {}", io::Error::last_os_error()));
        }
        let mut storage = vec![0_usize; (sid_bytes as usize + word_bytes - 1) / word_bytes];
        // SAFETY: destination has sid_bytes capacity and source remains live.
        if unsafe { CopySid(sid_bytes, storage.as_mut_ptr().cast(), source) } == 0 {
            return Err(format!("could not copy peer SID: {}", io::Error::last_os_error()));
        }
        Ok(OwnedSid { storage })
    }

    fn sid_string(sid: &OwnedSid) -> Result<String, String> {
        let mut raw: *mut u16 = ptr::null_mut();
        // SAFETY: sid contains a complete SID and raw is a valid out-pointer.
        if unsafe { ConvertSidToStringSidW(sid.as_ptr(), &mut raw) } == 0 {
            return Err(format!("could not stringify current user SID: {}", io::Error::last_os_error()));
        }
        let allocation = LocalAllocation(raw.cast());
        let mut length = 0_usize;
        // SAFETY: ConvertSidToStringSidW returns a NUL-terminated allocation.
        unsafe {
            while *raw.add(length) != 0 {
                length += 1;
            }
        }
        // SAFETY: exactly `length` initialized UTF-16 code units precede NUL.
        let value = String::from_utf16(unsafe { std::slice::from_raw_parts(raw, length) })
            .map_err(|error| format!("current user SID is invalid UTF-16: {error}"))?;
        drop(allocation);
        Ok(value)
    }

    fn wide_null(value: &str) -> Vec<u16> {
        use std::os::windows::ffi::OsStrExt;
        std::ffi::OsStr::new(value)
            .encode_wide()
            .chain(std::iter::once(0))
            .collect()
    }

    struct OwnedHandle(HANDLE);

    impl OwnedHandle {
        fn new(handle: HANDLE) -> io::Result<Self> {
            if handle.is_null() {
                Err(io::Error::last_os_error())
            } else {
                Ok(Self(handle))
            }
        }
    }

    unsafe impl Send for OwnedHandle {}
    unsafe impl Sync for OwnedHandle {}

    impl Drop for OwnedHandle {
        fn drop(&mut self) {
            // SAFETY: this wrapper owns one valid kernel handle.
            let _ = unsafe { CloseHandle(self.0) };
        }
    }

    struct LocalAllocation(*mut c_void);

    impl Drop for LocalAllocation {
        fn drop(&mut self) {
            if !self.0.is_null() {
                // SAFETY: pointer was allocated by a Win32 LocalAlloc-returning
                // conversion API and is freed exactly once here.
                let _ = unsafe { LocalFree(self.0) };
            }
        }
    }

    #[cfg(test)]
    mod tests {
        use std::sync::Arc;
        use std::time::Duration;

        use nomifun_api_types::{
            KNOWLEDGE_CAPABILITY_DOMAIN, KNOWLEDGE_WRITE_TOOL, KnowledgeMcpConfig,
        };
        use nomifun_common::{
            LoopbackCapabilityIssuer, LoopbackSessionKind, generate_id,
        };

        use super::*;
        use crate::testutil::make_service;

        #[test]
        fn windows_broker_owner_only_sddl_and_pipe_namespace_are_exact() {
            let current_sid = process_user_sid(unsafe { GetCurrentProcessId() }).unwrap();
            let sid = sid_string(&current_sid).unwrap();
            let sddl = owner_only_sddl(&sid);
            assert_eq!(sddl, format!("D:P(A;;GA;;;{sid})"));
            assert!(sddl.starts_with("D:P("), "DACL must be protected");
            assert_eq!(sddl.matches("(A;;GA;;;").count(), 1);
            assert!(!sddl.contains("WD"), "Everyone must not be granted access");
            assert!(!sddl.contains("BU"), "Users must not be granted access");
            assert!(!sddl.contains("BA"), "Administrators must not be granted access");

            let name = pipe_name_for_sid(&sid);
            assert_eq!(name, format!(r"\\.\pipe\nomifun-knowledge-broker-{sid}"));
            assert!(name.starts_with(r"\\.\pipe\"));
            assert!(!name.starts_with(r"\\?\"));
            assert!(!name.contains(r"\Global\"));
            assert!(!name.contains("127.0.0.1"));
            assert!(!name.contains("tcp"));
        }

        #[tokio::test]
        async fn windows_broker_protected_pipe_same_process_smoke() {
            let temp = tempfile::tempdir().unwrap();
            let data = temp.path().join("data");
            let workspace = temp.path().join("workspace");
            std::fs::create_dir(&data).unwrap();
            std::fs::create_dir(&workspace).unwrap();
            let canonical_workspace = std::fs::canonicalize(&workspace).unwrap();
            let service = Arc::new(make_service(&data));

            let issuer = Arc::new(LoopbackCapabilityIssuer::random().unwrap());
            let config = KnowledgeMcpConfig::from_issuer(
                43210,
                issuer.clone(),
                r"C:\Program Files\NomiFun\nomicore.exe".into(),
            );
            let current_sid = process_user_sid(unsafe { GetCurrentProcessId() }).unwrap();
            let sid = sid_string(&current_sid).unwrap();
            let pipe_name = format!(
                "{}-test-{}-{}",
                pipe_name_for_sid(&sid),
                unsafe { GetCurrentProcessId() },
                generate_id()
            );
            let broker = start_at(
                pipe_name.clone(),
                current_sid.clone(),
                config,
                Arc::downgrade(&service),
                "019abcde-f012-7abc-8abc-0123456789ab".into(),
            )
            .await
            .unwrap();

            // This handshake traverses the protected named-pipe DACL and both
            // SID checks before the broker can return a scoped bootstrap.
            let connection = connect_at(&pipe_name, &current_sid, &workspace)
                .await
                .unwrap();
            let claims = &connection.bootstrap().access.claims;
            assert_eq!(claims.user_id.as_str(), "019abcde-f012-7abc-8abc-0123456789ab");
            assert_eq!(claims.session.kind, LoopbackSessionKind::ExternalProcess);
            assert_eq!(
                claims.scope.workspace_path,
                canonical_workspace.to_string_lossy()
            );
            assert!(claims.scope.kb_ids.is_empty());
            assert!(!claims.allows(KNOWLEDGE_WRITE_TOOL));
            assert!(
                issuer
                    .renew::<nomifun_api_types::KnowledgeCapabilityScope>(
                        KNOWLEDGE_CAPABILITY_DOMAIN,
                        &connection.bootstrap().renewal,
                    )
                    .is_ok()
            );

            let renewal = connection.bootstrap().renewal.clone();
            drop(connection);
            tokio::time::timeout(Duration::from_secs(2), async {
                loop {
                    if issuer
                        .renew::<nomifun_api_types::KnowledgeCapabilityScope>(
                            KNOWLEDGE_CAPABILITY_DOMAIN,
                            &renewal,
                        )
                        .is_err()
                    {
                        break;
                    }
                    tokio::task::yield_now().await;
                }
            })
            .await
            .expect("closing the named-pipe control channel must revoke the lease");
            drop(broker);
        }
    }
}

#[cfg(test)]
mod protocol_tests {
    use super::*;

    #[test]
    fn request_cannot_submit_authorization_scope() {
        let forged = serde_json::json!({
            "version": BROKER_PROTOCOL_VERSION,
            "cwd": "/workspace",
            "user_id": "attacker",
            "kb_ids": ["all"],
            "allow_write": true,
            "tools": ["knowledge_write"]
        });
        assert!(serde_json::from_value::<BrokerRequest>(forged).is_err());
    }

    #[tokio::test]
    async fn request_frame_rejects_pipelined_control_bytes() {
        use tokio::io::AsyncWriteExt as _;

        let (mut client, mut server) = tokio::io::duplex(512);
        client
            .write_all(b"{\"version\":1,\"cwd\":\"/workspace\"}\nX")
            .await
            .unwrap();
        let error = read_request(&mut server).await.unwrap_err();
        assert!(error.contains("trailing bytes"), "unexpected error: {error}");
    }

    #[cfg(unix)]
    #[test]
    fn canonical_workspace_collapses_symlinks_and_rejects_files() {
        use std::os::unix::fs::symlink;

        let temp = tempfile::tempdir().unwrap();
        let real = temp.path().join("real");
        std::fs::create_dir(&real).unwrap();
        let alias = temp.path().join("alias");
        symlink(&real, &alias).unwrap();
        assert_eq!(
            canonical_workspace(alias.to_str().unwrap()).unwrap(),
            std::fs::canonicalize(real).unwrap()
        );

        let file = temp.path().join("file");
        std::fs::write(&file, b"x").unwrap();
        assert!(canonical_workspace(file.to_str().unwrap()).is_err());
    }
}

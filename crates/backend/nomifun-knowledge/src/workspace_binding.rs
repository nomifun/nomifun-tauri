//! Process-local authority for the physical knowledge-mount namespace.
//!
//! A custom workspace can be shared by more than one conversation.  The
//! mount engine reconciles one process-owned directory,
//! `.nomi/knowledge`, so a per-conversation mutex is insufficient: two live
//! runtimes with different bindings would otherwise repeatedly replace each
//! other's links.  This module gives that physical directory one binding
//! authority for the complete lifetime of every runtime using it.
//!
//! Authority has two layers: a signature-aware in-process registry permits
//! exact-binding sharing, while a user-wide OS file lock serializes different
//! backend processes and data directories. Leases are RAII values held by the
//! exact runtime-registry slot; aborts, panics and process exit therefore
//! release safely, while failed runtime teardown keeps the lease alive.

use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use fs2::FileExt;
use nomifun_common::AppError;

const LOCK_REGISTRY_RELATIVE_DIR: &[&str] =
    &["NomiFun", "runtime-locks", "knowledge-workspaces"];
const UNBOUND_WORKSPACE_SIGNATURE: &str = "kb-binding-v1:unbound";

#[derive(Debug)]
struct BindingAuthorityEntry {
    signature: String,
    holders: HashMap<u64, String>,
    /// Cross-process authority. The first in-process holder acquires it and
    /// the entry retains the file handle until the final holder drops.
    _os_lock: File,
}

#[derive(Debug)]
struct WorkspaceBindingLeaseInner {
    workspace_key: PathBuf,
    signature: String,
    owner: String,
    lease_id: u64,
}

/// Exclusive-by-signature authority over one physical workspace's
/// `.nomi/knowledge` namespace.
///
/// Clones are handles to the same logical lease.  The authority holder is
/// removed only after the last clone drops, which lets build options transfer
/// ownership into an exact runtime slot without an unprotected gap.
#[derive(Debug, Clone)]
pub struct WorkspaceBindingLease {
    inner: Arc<WorkspaceBindingLeaseInner>,
}

impl WorkspaceBindingLease {
    /// Acquire conservative authority for a runtime that has no active
    /// KnowledgeService plan.
    ///
    /// This does not touch `.nomi/knowledge`; it only prevents a runtime built
    /// without mount metadata from racing with, or observing the mutable
    /// namespace of, a differently-bound runtime in the same physical
    /// workspace.
    pub fn acquire_unbound(
        workspace: &Path,
        owner: impl Into<String>,
    ) -> Result<Self, AppError> {
        Self::acquire(workspace, UNBOUND_WORKSPACE_SIGNATURE, owner)
    }

    /// Acquire authority for `signature` in the physical `workspace`.
    ///
    /// Multiple owners may share the workspace only when their exact binding
    /// signatures match.  A conflicting signature is rejected before any
    /// mount reconciliation can touch the filesystem.
    pub fn acquire(
        workspace: &Path,
        signature: impl Into<String>,
        owner: impl Into<String>,
    ) -> Result<Self, AppError> {
        let canonical_workspace = canonical_workspace_path(workspace)?;
        let workspace_key = platform_comparison_path(canonical_workspace.clone());
        let signature = signature.into();
        if signature.is_empty() {
            return Err(AppError::Internal(
                "knowledge workspace binding signature must not be empty".to_owned(),
            ));
        }
        let owner = owner.into();
        if owner.trim().is_empty() {
            return Err(AppError::Internal(
                "knowledge workspace binding lease owner must not be empty".to_owned(),
            ));
        }

        let lease_id = next_lease_id();
        let mut authorities = authority_registry().lock().map_err(|_| {
            AppError::Internal("knowledge workspace binding authority is poisoned".to_owned())
        })?;
        match authorities.entry(workspace_key.clone()) {
            std::collections::hash_map::Entry::Vacant(entry) => {
                // Acquire the OS lock while holding the process-local registry
                // mutex. This serializes first-holder races in this process;
                // the non-blocking system lock then arbitrates other backend
                // processes and fails closed when locking is unsupported.
                let os_lock = acquire_workspace_os_lock(&canonical_workspace)?;
                entry.insert(BindingAuthorityEntry {
                    signature: signature.clone(),
                    holders: HashMap::from([(lease_id, owner.clone())]),
                    _os_lock: os_lock,
                });
            }
            std::collections::hash_map::Entry::Occupied(mut entry) => {
                if entry.get().signature != signature {
                    let mut owners = entry.get().holders.values().cloned().collect::<Vec<_>>();
                    owners.sort();
                    owners.dedup();
                    return Err(AppError::Conflict(format!(
                        "workspace {} is already leased by a different knowledge binding (owners: {})",
                        workspace_key.display(),
                        owners.join(", ")
                    )));
                }
                entry.get_mut().holders.insert(lease_id, owner.clone());
            }
        }
        drop(authorities);

        Ok(Self {
            inner: Arc::new(WorkspaceBindingLeaseInner {
                workspace_key,
                signature,
                owner,
                lease_id,
            }),
        })
    }

    /// Canonical, platform-comparison identity of the physical workspace.
    pub fn workspace_key(&self) -> &Path {
        &self.inner.workspace_key
    }

    /// Exact logical binding signature protected by this lease.
    pub fn signature(&self) -> &str {
        &self.inner.signature
    }

    /// Process-local lifecycle owner (normally a conversation id).
    pub fn owner(&self) -> &str {
        &self.inner.owner
    }

    /// Whether this lease represents a runtime with no activated knowledge
    /// mount plan.
    pub fn is_unbound(&self) -> bool {
        self.signature() == UNBOUND_WORKSPACE_SIGNATURE
    }

    /// Verify that `workspace` resolves to the exact physical workspace
    /// protected by this lease.
    ///
    /// Runtime registries call this before attaching a lease to a slot.  That
    /// prevents a caller from acquiring authority for one directory and then
    /// using the lease to build a process in another directory whose
    /// `.nomi/knowledge` namespace is not protected.
    pub fn matches_workspace(&self, workspace: &Path) -> Result<bool, AppError> {
        Ok(self.workspace_key() == canonical_workspace_key(workspace)?)
    }

    /// Whether two handles protect exactly the same workspace binding.
    pub fn same_binding(&self, other: &Self) -> bool {
        self.workspace_key() == other.workspace_key() && self.signature() == other.signature()
    }
}

impl Drop for WorkspaceBindingLeaseInner {
    fn drop(&mut self) {
        let mut authorities = authority_registry()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let should_remove = if let Some(entry) = authorities.get_mut(&self.workspace_key) {
            // A lease id is globally unique.  The signature check is a second
            // fail-safe against an impossible stale Drop deleting a successor
            // authority after key reuse.
            if entry.signature == self.signature {
                entry.holders.remove(&self.lease_id);
            }
            entry.holders.is_empty()
        } else {
            false
        };
        if should_remove {
            if let Some(entry) = authorities.remove(&self.workspace_key) {
                // Do not rely only on `File`'s close-on-drop side effect here.
                // On Unix, heavily concurrent last-holder hand-offs exposed a
                // small window where a fresh open of the same lock inode could
                // still observe EWOULDBLOCK after the registry entry vanished.
                // Release the advisory lock synchronously while the registry
                // mutex still prevents an in-process successor from racing the
                // hand-off; dropping the handle remains the final fail-safe.
                let _ = FileExt::unlock(&entry._os_lock);
            }
        }
    }
}

fn authority_registry() -> &'static Mutex<HashMap<PathBuf, BindingAuthorityEntry>> {
    static AUTHORITIES: OnceLock<Mutex<HashMap<PathBuf, BindingAuthorityEntry>>> = OnceLock::new();
    AUTHORITIES.get_or_init(|| Mutex::new(HashMap::new()))
}

fn next_lease_id() -> u64 {
    static NEXT_LEASE_ID: AtomicU64 = AtomicU64::new(1);
    NEXT_LEASE_ID.fetch_add(1, Ordering::Relaxed)
}

/// Resolve aliases to one physical workspace identity.
///
/// Canonicalization resolves Windows junctions and Unix symlinks.  Windows
/// and macOS are additionally case-folded because their common filesystems
/// are case-insensitive.  This intentionally over-serializes distinct
/// case-sensitive APFS paths: a conservative conflict is safer than silently
/// treating two spellings of one mount namespace as independent authorities.
pub(crate) fn canonical_workspace_key(workspace: &Path) -> Result<PathBuf, AppError> {
    canonical_workspace_path(workspace).map(platform_comparison_path)
}

fn canonical_workspace_path(workspace: &Path) -> Result<PathBuf, AppError> {
    let canonical = std::fs::canonicalize(workspace).map_err(|error| {
        AppError::BadRequest(format!(
            "knowledge workspace {} cannot be canonicalized: {error}",
            workspace.display()
        ))
    })?;
    let metadata = std::fs::metadata(&canonical).map_err(|error| {
        AppError::BadRequest(format!(
            "knowledge workspace {} cannot be inspected: {error}",
            canonical.display()
        ))
    })?;
    if !metadata.is_dir() {
        return Err(AppError::BadRequest(format!(
            "knowledge workspace {} is not a directory",
            canonical.display()
        )));
    }
    Ok(canonical)
}

fn acquire_workspace_os_lock(workspace: &Path) -> Result<File, AppError> {
    let lock_path = workspace_lock_path(workspace)?;
    validate_lock_path(&lock_path, true)?;
    let file = open_workspace_lock_file(&lock_path).map_err(|error| {
        AppError::Conflict(format!(
            "knowledge workspace {} authority file {} cannot be opened safely: {error}",
            workspace.display(),
            lock_path.display()
        ))
    })?;
    // On Windows the open handle denies delete/rename sharing, so this
    // pathname check cannot be swapped out after open. Combined with
    // FILE_FLAG_OPEN_REPARSE_POINT it rejects, rather than follows, a junction
    // or symlink that won a create race.
    validate_lock_path(&lock_path, false)?;
    let metadata = file.metadata().map_err(|error| {
        AppError::Conflict(format!(
            "knowledge workspace lock file {} cannot be inspected: {error}",
            lock_path.display()
        ))
    })?;
    if !metadata.is_file() || lock_file_is_reparse_point(&metadata) {
        return Err(AppError::Conflict(format!(
            "knowledge workspace lock path {} is not a regular non-link file",
            lock_path.display()
        )));
    }
    FileExt::try_lock_exclusive(&file).map_err(|error| {
        AppError::Conflict(format!(
            "knowledge workspace {} is locked by another application process or does not support exclusive file locking: {error}",
            workspace.display()
        ))
    })?;
    Ok(file)
}

fn validate_lock_path(path: &Path, allow_missing: bool) -> Result<(), AppError> {
    if lock_path_reparse_state(path)?.is_some_and(|is_reparse| is_reparse) {
        return Err(AppError::Conflict(format!(
            "knowledge workspace authority path {} is a Windows reparse point",
            path.display()
        )));
    }
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_file() && !lock_file_is_reparse_point(&metadata) => Ok(()),
        Ok(_) => Err(AppError::Conflict(format!(
            "knowledge workspace authority path {} is not a regular non-link file",
            path.display()
        ))),
        Err(error) if allow_missing && error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(AppError::Conflict(format!(
            "knowledge workspace authority path {} cannot be inspected: {error}",
            path.display()
        ))),
    }
}

#[cfg(windows)]
fn lock_path_reparse_state(path: &Path) -> Result<Option<bool>, AppError> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::{
        FILE_ATTRIBUTE_REPARSE_POINT, GetFileAttributesW, INVALID_FILE_ATTRIBUTES,
    };

    let mut wide = path.as_os_str().encode_wide().collect::<Vec<_>>();
    wide.push(0);
    // SAFETY: `wide` is NUL-terminated and remains alive for the call.
    let attributes = unsafe { GetFileAttributesW(wide.as_ptr()) };
    if attributes == INVALID_FILE_ATTRIBUTES {
        let error = std::io::Error::last_os_error();
        if error.kind() == std::io::ErrorKind::NotFound {
            return Ok(None);
        }
        return Err(AppError::Conflict(format!(
            "knowledge workspace authority path {} attributes cannot be read: {error}",
            path.display()
        )));
    }
    Ok(Some(attributes & FILE_ATTRIBUTE_REPARSE_POINT != 0))
}

#[cfg(not(windows))]
fn lock_path_reparse_state(_path: &Path) -> Result<Option<bool>, AppError> {
    Ok(None)
}

fn workspace_lock_path(workspace: &Path) -> Result<PathBuf, AppError> {
    workspace_lock_path_from_candidates(workspace, lock_registry_candidates()?)
}

fn workspace_lock_path_from_candidates(
    workspace: &Path,
    candidates: impl IntoIterator<Item = PathBuf>,
) -> Result<PathBuf, AppError> {
    use sha2::{Digest, Sha256};

    for registry in candidates {
        if lock_registry_overlaps_workspace(workspace, &registry) {
            continue;
        }
        // The first non-overlapping candidate is deterministic for every
        // process. Failure to create/secure it is terminal rather than a
        // per-process fallback that could split authority across registries.
        create_lock_registry_dir(&registry)?;
        let registry = std::fs::canonicalize(&registry).map_err(|error| {
            AppError::Conflict(format!(
                "knowledge workspace authority directory {} cannot be canonicalized: {error}",
                registry.display()
            ))
        })?;
        if lock_registry_overlaps_workspace(workspace, &registry) {
            // A symlinked ancestor may reveal physical overlap only after
            // canonicalization. Skip deterministically to the next namespace.
            continue;
        }
        let workspace_key = platform_comparison_path(workspace.to_path_buf());
        let digest = Sha256::digest(workspace_key.to_string_lossy().as_bytes());
        return Ok(registry.join(format!("{}.lock", hex::encode(digest))));
    }
    Err(AppError::Conflict(format!(
        "knowledge workspace {} contains every available stable authority namespace",
        workspace.display()
    )))
}

fn lock_registry_overlaps_workspace(workspace: &Path, registry: &Path) -> bool {
    let workspace_key = platform_comparison_path(workspace.to_path_buf());
    let registry_key = platform_comparison_path(registry.to_path_buf());
    registry_key.starts_with(&workspace_key) || workspace_key.starts_with(&registry_key)
}

fn lock_registry_candidates() -> Result<Vec<PathBuf>, AppError> {
    let mut candidates = Vec::new();
    if let Some(mut data_local) = dirs::data_local_dir() {
        for component in LOCK_REGISTRY_RELATIVE_DIR {
            data_local.push(component);
        }
        candidates.push(data_local);
    }

    #[cfg(windows)]
    if let Some(program_data) = std::env::var_os("ProgramData") {
        candidates.push(
            PathBuf::from(program_data)
                .join("NomiFun")
                .join("runtime-locks")
                .join("knowledge-workspaces"),
        );
    }

    let mut temporary = std::env::temp_dir();
    #[cfg(unix)]
    temporary.push(format!(
        "nomifun-runtime-locks-{}",
        unsafe { libc::geteuid() }
    ));
    #[cfg(not(unix))]
    temporary.push("NomiFun-runtime-locks");
    temporary.push("knowledge-workspaces");
    candidates.push(temporary);

    if candidates.is_empty() {
        return Err(AppError::Conflict(
            "knowledge workspace authority refused: no stable lock namespace is available"
                .to_owned(),
        ));
    }
    Ok(candidates)
}

fn create_lock_registry_dir(path: &Path) -> Result<(), AppError> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};

        let mut builder = std::fs::DirBuilder::new();
        builder.recursive(true).mode(0o700);
        builder.create(path).map_err(|error| {
            AppError::Conflict(format!(
                "knowledge workspace authority directory {} cannot be created: {error}",
                path.display()
            ))
        })?;
        let metadata = std::fs::symlink_metadata(path).map_err(|error| {
            AppError::Conflict(format!(
                "knowledge workspace authority directory {} cannot be inspected: {error}",
                path.display()
            ))
        })?;
        if !metadata.is_dir() || metadata.file_type().is_symlink() {
            return Err(AppError::Conflict(format!(
                "knowledge workspace authority path {} is not a real directory",
                path.display()
            )));
        }
        if metadata.uid() != unsafe { libc::geteuid() } {
            return Err(AppError::Conflict(format!(
                "knowledge workspace authority directory {} is not owned by the current user",
                path.display()
            )));
        }
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700)).map_err(
            |error| {
                AppError::Conflict(format!(
                    "knowledge workspace authority directory {} cannot be secured: {error}",
                    path.display()
                ))
            },
        )?;
    }
    #[cfg(not(unix))]
    {
        std::fs::create_dir_all(path).map_err(|error| {
            AppError::Conflict(format!(
                "knowledge workspace authority directory {} cannot be created: {error}",
                path.display()
            ))
        })?;
        let metadata = std::fs::symlink_metadata(path).map_err(|error| {
            AppError::Conflict(format!(
                "knowledge workspace authority directory {} cannot be inspected: {error}",
                path.display()
            ))
        })?;
        if !metadata.is_dir() || lock_file_is_reparse_point(&metadata) {
            return Err(AppError::Conflict(format!(
                "knowledge workspace authority path {} is not a real directory",
                path.display()
            )));
        }
    }
    Ok(())
}

#[cfg(unix)]
fn open_workspace_lock_file(path: &Path) -> std::io::Result<File> {
    use std::os::unix::fs::OpenOptionsExt;

    OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .mode(0o600)
        // O_NOFOLLOW rejects a hostile final-component symlink atomically.
        // O_NONBLOCK prevents a pre-existing FIFO/device from blocking before
        // the handle metadata check rejects it as non-regular.
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)
}

#[cfg(windows)]
fn open_workspace_lock_file(path: &Path) -> std::io::Result<File> {
    use std::os::windows::fs::OpenOptionsExt;
    use windows_sys::Win32::Storage::FileSystem::{
        FILE_FLAG_OPEN_REPARSE_POINT, FILE_SHARE_READ, FILE_SHARE_WRITE,
    };

    OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        // Other processes must be able to open the same inode and observe the
        // advisory lock, but nobody may rename/delete it while authority is
        // held (FILE_SHARE_DELETE is deliberately absent).
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
        // Open the reparse point itself, then reject it by handle metadata
        // below; never follow it to an attacker-selected target.
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
        .open(path)
}

#[cfg(not(any(unix, windows)))]
fn open_workspace_lock_file(path: &Path) -> std::io::Result<File> {
    OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .open(path)
}

#[cfg(windows)]
fn lock_file_is_reparse_point(metadata: &std::fs::Metadata) -> bool {
    use std::os::windows::fs::MetadataExt;
    use windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT;

    metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
}

#[cfg(not(windows))]
fn lock_file_is_reparse_point(_metadata: &std::fs::Metadata) -> bool {
    false
}

#[cfg(any(windows, target_os = "macos"))]
fn platform_comparison_path(path: PathBuf) -> PathBuf {
    // Lossy conversion can only merge otherwise distinct non-Unicode names,
    // producing a conservative conflict.  It can never split one physical
    // case-insensitive path into two authorities.
    PathBuf::from(path.to_string_lossy().to_lowercase())
}

#[cfg(not(any(windows, target_os = "macos")))]
fn platform_comparison_path(path: PathBuf) -> PathBuf {
    path
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn absent_user_lock_registry_is_created_and_canonicalized() {
        let root = tempfile::tempdir().unwrap();
        let workspace = root.path().join("workspace");
        std::fs::create_dir(&workspace).unwrap();
        let registry = root
            .path()
            .join("missing")
            .join("user-data")
            .join("knowledge-locks");

        let lock_path =
            workspace_lock_path_from_candidates(&workspace, [registry.clone()]).unwrap();

        assert_eq!(
            lock_path.parent().unwrap(),
            std::fs::canonicalize(registry).unwrap()
        );
        assert_eq!(lock_path.extension().and_then(|ext| ext.to_str()), Some("lock"));
    }

    #[test]
    fn home_sized_workspace_uses_non_overlapping_lock_namespace() {
        let root = tempfile::tempdir().unwrap();
        let workspace = root.path().join("home");
        std::fs::create_dir(&workspace).unwrap();
        let nested_user_data = workspace.join(".local").join("share").join("knowledge-locks");
        let outside = root.path().join("system-runtime").join("knowledge-locks");

        let lock_path = workspace_lock_path_from_candidates(
            &workspace,
            [nested_user_data.clone(), outside.clone()],
        )
        .unwrap();

        assert!(
            !nested_user_data.exists(),
            "an overlapping registry candidate must not be created inside the model workspace"
        );
        assert_eq!(
            lock_path.parent().unwrap(),
            std::fs::canonicalize(outside).unwrap()
        );
    }

    #[test]
    fn cross_process_workspace_lock_blocks_second_backend_until_release() {
        const HELPER_MODE: &str = "NOMIFUN_KB_LOCK_HELPER_MODE";
        const HELPER_WORKSPACE: &str = "NOMIFUN_KB_LOCK_HELPER_WORKSPACE";
        const TEST_NAME: &str =
            "workspace_binding::tests::cross_process_workspace_lock_blocks_second_backend_until_release";

        if let Some(mode) = std::env::var_os(HELPER_MODE) {
            let mode = mode.to_string_lossy();
            let workspace = PathBuf::from(
                std::env::var_os(HELPER_WORKSPACE)
                    .expect("cross-process lock helper workspace is required"),
            );
            if mode == "workspace-unlink-attack-must-conflict" {
                let obsolete_workspace_lock =
                    workspace.join(".nomifun-knowledge-binding.lock");
                let _ = std::fs::remove_file(&obsolete_workspace_lock);
                std::fs::write(&obsolete_workspace_lock, b"attacker replacement").unwrap();
            }
            let result =
                WorkspaceBindingLease::acquire(&workspace, "binding-a", "helper-process");
            match mode.as_ref() {
                "must-conflict" | "workspace-unlink-attack-must-conflict" => assert!(
                    matches!(result, Err(AppError::Conflict(_))),
                    "a second backend process must not share workspace authority"
                ),
                "must-acquire" => {
                    result.expect("workspace lock must become available after owner exit");
                }
                other => panic!("unknown cross-process lock helper mode {other}"),
            }
            return;
        }

        let workspace = tempfile::tempdir().unwrap();
        let owner =
            WorkspaceBindingLease::acquire(workspace.path(), "binding-a", "parent-process")
                .unwrap();
        let run_helper = |mode: &str| {
            std::process::Command::new(std::env::current_exe().expect("current test executable"))
                .arg("--exact")
                .arg(TEST_NAME)
                .arg("--nocapture")
                .env(HELPER_MODE, mode)
                .env(HELPER_WORKSPACE, workspace.path())
                .output()
                .expect("spawn cross-process lock helper")
        };

        let blocked = run_helper("must-conflict");
        assert!(
            blocked.status.success(),
            "second-process lock assertion failed\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&blocked.stdout),
            String::from_utf8_lossy(&blocked.stderr)
        );

        let workspace_replacement = run_helper("workspace-unlink-attack-must-conflict");
        assert!(
            workspace_replacement.status.success(),
            "workspace-local unlink/recreate bypassed the stable authority lock\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&workspace_replacement.stdout),
            String::from_utf8_lossy(&workspace_replacement.stderr)
        );

        drop(owner);
        let acquired = run_helper("must-acquire");
        assert!(
            acquired.status.success(),
            "released lock was not reusable\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&acquired.stdout),
            String::from_utf8_lossy(&acquired.stderr)
        );
    }

    #[cfg(unix)]
    #[test]
    fn workspace_lock_refuses_final_component_symlink() {
        use std::os::unix::fs::symlink;

        let workspace = tempfile::tempdir().unwrap();
        let target = workspace.path().join("attacker-target");
        std::fs::write(&target, b"do not lock through this path").unwrap();
        let canonical_workspace = canonical_workspace_path(workspace.path()).unwrap();
        let lock_path = workspace_lock_path(&canonical_workspace).unwrap();
        let _ = std::fs::remove_file(&lock_path);
        symlink(&target, &lock_path).unwrap();

        let result =
            WorkspaceBindingLease::acquire(workspace.path(), "binding-a", "conversation-a");
        assert!(
            matches!(result, Err(AppError::Conflict(_))),
            "unexpected reparse lock result: {result:?}"
        );
        std::fs::remove_file(lock_path).unwrap();
    }

    #[cfg(windows)]
    #[test]
    fn workspace_lock_refuses_final_component_reparse_point() {
        let workspace = tempfile::tempdir().unwrap();
        let target = tempfile::tempdir().unwrap();
        let canonical_workspace = canonical_workspace_path(workspace.path()).unwrap();
        let lock_path = workspace_lock_path(&canonical_workspace).unwrap();
        let _ = std::fs::remove_file(&lock_path);
        junction::create(target.path(), &lock_path).unwrap();
        assert!(junction::exists(&lock_path).unwrap());
        assert_eq!(lock_path_reparse_state(&lock_path).unwrap(), Some(true));

        let result =
            WorkspaceBindingLease::acquire(workspace.path(), "binding-a", "conversation-a");
        assert!(
            matches!(result, Err(AppError::Conflict(_))),
            "unexpected reparse lock result: {result:?}"
        );
        junction::delete(lock_path).unwrap();
    }

    #[test]
    fn different_binding_is_blocked_until_active_owner_releases() {
        let workspace = tempfile::tempdir().unwrap();
        let owner_a =
            WorkspaceBindingLease::acquire(workspace.path(), "binding-a", "conversation-a")
                .unwrap();

        let conflict =
            WorkspaceBindingLease::acquire(workspace.path(), "binding-b", "conversation-b")
                .unwrap_err();
        assert!(matches!(conflict, AppError::Conflict(_)));

        drop(owner_a);
        let owner_b =
            WorkspaceBindingLease::acquire(workspace.path(), "binding-b", "conversation-b")
                .expect("a new binding can take over only after the old runtime releases");
        assert_eq!(owner_b.signature(), "binding-b");
    }

    #[test]
    fn same_binding_can_be_shared_by_multiple_runtime_owners() {
        let workspace = tempfile::tempdir().unwrap();
        let owner_a =
            WorkspaceBindingLease::acquire(workspace.path(), "shared-binding", "conversation-a")
                .unwrap();
        let owner_b =
            WorkspaceBindingLease::acquire(workspace.path(), "shared-binding", "conversation-b")
                .expect("the exact same physical binding is safe to share");

        assert!(owner_a.same_binding(&owner_b));
        drop(owner_a);
        assert!(
            WorkspaceBindingLease::acquire(workspace.path(), "other-binding", "conversation-c")
                .is_err(),
            "one remaining shared owner must keep the old binding authoritative"
        );
        drop(owner_b);
        WorkspaceBindingLease::acquire(workspace.path(), "other-binding", "conversation-c")
            .unwrap();
    }

    #[test]
    fn repeated_last_holder_handoffs_release_os_lock_synchronously() {
        let workspace = tempfile::tempdir().unwrap();

        for generation in 0..256 {
            let signature = format!("binding-{generation}");
            let first = WorkspaceBindingLease::acquire(
                workspace.path(),
                signature.clone(),
                format!("first-{generation}"),
            )
            .unwrap();
            let second = WorkspaceBindingLease::acquire(
                workspace.path(),
                signature,
                format!("second-{generation}"),
            )
            .unwrap();

            drop(first);
            drop(second);

            // The final holder's Drop is the linearization point for the
            // next binding. A successor must never observe a process-local
            // registry vacancy while the previous OS lock is still held.
            let successor = WorkspaceBindingLease::acquire(
                workspace.path(),
                format!("successor-{generation}"),
                format!("successor-owner-{generation}"),
            )
            .unwrap();
            drop(successor);
        }
    }

    #[test]
    fn unbound_runtime_authority_shares_but_conflicts_with_mounted_binding() {
        let workspace = tempfile::tempdir().unwrap();
        let unbound_a =
            WorkspaceBindingLease::acquire_unbound(workspace.path(), "conversation-a").unwrap();
        let unbound_b =
            WorkspaceBindingLease::acquire_unbound(workspace.path(), "conversation-b")
                .expect("unbound runtimes may share the exact same physical authority");

        assert!(unbound_a.same_binding(&unbound_b));
        assert!(
            WorkspaceBindingLease::acquire(
                workspace.path(),
                "kb-binding-v1:mounted",
                "conversation-c"
            )
            .is_err(),
            "a mounted binding must conflict with live unbound runtimes"
        );
        drop(unbound_a);
        drop(unbound_b);
        WorkspaceBindingLease::acquire(
            workspace.path(),
            "kb-binding-v1:mounted",
            "conversation-c",
        )
        .expect("mounted authority may take over after all unbound runtimes exit");
    }

    #[test]
    fn clone_keeps_authority_until_last_runtime_handle_drops() {
        let workspace = tempfile::tempdir().unwrap();
        let lease =
            WorkspaceBindingLease::acquire(workspace.path(), "binding-a", "conversation-a")
                .unwrap();
        let runtime_handle = lease.clone();
        drop(lease);

        assert!(
            WorkspaceBindingLease::acquire(workspace.path(), "binding-b", "conversation-b")
                .is_err()
        );
        drop(runtime_handle);
        WorkspaceBindingLease::acquire(workspace.path(), "binding-b", "conversation-b").unwrap();
    }

    #[test]
    fn panic_unwind_releases_untransferred_lease() {
        let workspace = tempfile::tempdir().unwrap();
        let path = workspace.path().to_path_buf();
        let _ = std::panic::catch_unwind(|| {
            let _lease =
                WorkspaceBindingLease::acquire(&path, "binding-a", "conversation-a").unwrap();
            panic!("simulate aborted mount preparation");
        });

        WorkspaceBindingLease::acquire(workspace.path(), "binding-b", "conversation-b")
            .expect("RAII drop must release authority during unwind");
    }

    #[test]
    fn nonexistent_workspace_fails_closed() {
        let parent = tempfile::tempdir().unwrap();
        let missing = parent.path().join("missing");
        assert!(
            WorkspaceBindingLease::acquire(&missing, "binding-a", "conversation-a").is_err()
        );
    }

    #[cfg(unix)]
    #[test]
    fn symlink_alias_uses_the_same_physical_authority() {
        use std::os::unix::fs::symlink;

        let parent = tempfile::tempdir().unwrap();
        let workspace = parent.path().join("workspace");
        let alias = parent.path().join("alias");
        std::fs::create_dir(&workspace).unwrap();
        symlink(&workspace, &alias).unwrap();

        let _owner_a =
            WorkspaceBindingLease::acquire(&workspace, "binding-a", "conversation-a").unwrap();
        assert!(
            WorkspaceBindingLease::acquire(&alias, "binding-b", "conversation-b").is_err(),
            "a symlink spelling must not create an independent mount authority"
        );
    }

    #[cfg(windows)]
    #[test]
    fn junction_alias_uses_the_same_physical_authority() {
        let parent = tempfile::tempdir().unwrap();
        let workspace = parent.path().join("workspace");
        let alias = parent.path().join("alias");
        std::fs::create_dir(&workspace).unwrap();
        junction::create(&workspace, &alias).unwrap();

        let _owner_a =
            WorkspaceBindingLease::acquire(&workspace, "binding-a", "conversation-a").unwrap();
        assert!(
            WorkspaceBindingLease::acquire(&alias, "binding-b", "conversation-b").is_err(),
            "a junction spelling must not create an independent mount authority"
        );
    }
}
